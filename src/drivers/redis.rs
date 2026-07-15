use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use base64::Engine as _;
use redis::IntoConnectionInfo as _;
use secrecy::{ExposeSecret as _, SecretString};

use crate::drivers::{DriverError, RedisTlsFailure};
use crate::model::{
    Cell, Column, ConnectionProfile, DriverAvailability, DriverCapabilities, DriverDescriptor,
    DriverKind, MAX_REDIS_CELL_BYTES, MAX_REDIS_CELLS, MAX_REDIS_DEPTH, MAX_RESULT_BYTES,
    QueryLanguage, QueryResult, RedisExecuteRequest, RedisKeyInspectRequest, RedisKeyPage,
    RedisScanRequest, RedisValuePreview, TlsMode,
};

pub const DESCRIPTOR: DriverDescriptor = DriverDescriptor {
    kind: DriverKind::Redis,
    display_name: "Redis",
    default_port: 6379,
    availability: DriverAvailability::Ready,
    languages: &[QueryLanguage::RedisCommand],
    capabilities: DriverCapabilities::CONNECT
        .union(DriverCapabilities::PING)
        .union(DriverCapabilities::COMMAND)
        .union(DriverCapabilities::KEYSPACE_BROWSE),
    planned_capabilities: DriverCapabilities::empty(),
    reason: None,
};

static PLAINTEXT_TRANSPORT_ATTEMPTS: AtomicU64 = AtomicU64::new(0);
static REQUIRED_TLS_TRANSPORT_ATTEMPTS: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RedisTransportAttemptCounts {
    pub plaintext: u64,
    pub required_tls: u64,
}

/// Test/receipt instrumentation for proving that Required has no plaintext edge.
/// The counters contain no endpoint, credential, or user value.
#[doc(hidden)]
pub fn transport_attempt_counts() -> RedisTransportAttemptCounts {
    RedisTransportAttemptCounts {
        plaintext: PLAINTEXT_TRANSPORT_ATTEMPTS.load(Ordering::SeqCst),
        required_tls: REQUIRED_TLS_TRANSPORT_ATTEMPTS.load(Ordering::SeqCst),
    }
}

#[doc(hidden)]
pub fn reset_transport_attempt_counts() {
    PLAINTEXT_TRANSPORT_ATTEMPTS.store(0, Ordering::SeqCst);
    REQUIRED_TLS_TRANSPORT_ATTEMPTS.store(0, Ordering::SeqCst);
}

#[derive(Clone)]
pub struct RedisSession {
    connection: ::redis::aio::ConnectionManager,
}

impl RedisSession {
    pub async fn connect(
        profile: &ConnectionProfile,
        secret: Option<&SecretString>,
        timeout: Duration,
    ) -> Result<Self, DriverError> {
        if profile.tls == TlsMode::Preferred {
            return Err(DriverError::Unsupported {
                driver: DriverKind::Redis,
                operation: "legacy Preferred TLS mode".to_owned(),
            });
        }
        let db = profile
            .database
            .as_deref()
            .unwrap_or("0")
            .parse::<i64>()
            .map_err(|error| DriverError::InvalidConfig {
                driver: DriverKind::Redis,
                message: format!("database must be an integer: {error}"),
            })?;
        let mut redis = ::redis::RedisConnectionInfo::default().set_db(db);
        if let Some(username) = profile.username.as_deref() {
            redis = redis.set_username(username);
        }
        if let Some(secret) = secret {
            redis = redis.set_password(secret.expose_secret());
        }
        // Build the address without a credential-bearing URL. redis 1.3 keeps
        // ConnectionInfo fields private and exposes mutation builders instead.
        let mut info = (profile.host.clone(), profile.port)
            .into_connection_info()?
            .set_redis_settings(redis);
        let client = match profile.tls {
            TlsMode::Disabled => {
                PLAINTEXT_TRANSPORT_ATTEMPTS.fetch_add(1, Ordering::SeqCst);
                ::redis::Client::open(info)?
            }
            TlsMode::Required => {
                REQUIRED_TLS_TRANSPORT_ATTEMPTS.fetch_add(1, Ordering::SeqCst);
                info = info.set_addr(::redis::ConnectionAddr::TcpTls {
                    host: profile.host.clone(),
                    port: profile.port,
                    insecure: false,
                    tls_params: None,
                });
                if let Some(ca_file) = profile.redis_tls.ca_file.as_deref() {
                    let root_cert =
                        std::fs::read(ca_file).map_err(|_| DriverError::InvalidConfig {
                            driver: DriverKind::Redis,
                            message: "configured Redis CA file is not readable".to_owned(),
                        })?;
                    ::redis::Client::build_with_tls(
                        info,
                        ::redis::TlsCertificates {
                            client_tls: None,
                            root_cert: Some(root_cert),
                        },
                    )?
                } else {
                    ::redis::Client::open(info)?
                }
            }
            TlsMode::Preferred => {
                return Err(DriverError::Unsupported {
                    driver: DriverKind::Redis,
                    operation: "legacy Preferred TLS mode".to_owned(),
                });
            }
        };
        // The manager's default initial-connect retry loop can turn a stable
        // authentication rejection into our outer timeout. Initial connect is
        // single-attempt so the typed Redis error remains available; the
        // manager still owns reconnect behavior after a successful session.
        let manager_config = ::redis::aio::ConnectionManagerConfig::new()
            .set_number_of_retries(0)
            .set_connection_timeout(Some(timeout))
            .set_response_timeout(Some(timeout));
        let connection = match tokio::time::timeout(
            timeout,
            client.get_connection_manager_with_config(manager_config),
        )
        .await
        {
            Err(_) => {
                return Err(DriverError::Timeout {
                    driver: DriverKind::Redis,
                    seconds: timeout.as_secs(),
                });
            }
            Ok(Err(error)) if profile.tls == TlsMode::Required => {
                return Err(classify_required_tls_error(error));
            }
            Ok(Err(error)) => return Err(error.into()),
            Ok(Ok(connection)) => connection,
        };
        Ok(Self { connection })
    }

    pub async fn ping(&self, timeout: Duration) -> Result<(), DriverError> {
        let mut connection = self.connection.clone();
        let pong: String =
            tokio::time::timeout(timeout, ::redis::cmd("PING").query_async(&mut connection))
                .await
                .map_err(|_| DriverError::Timeout {
                    driver: DriverKind::Redis,
                    seconds: timeout.as_secs(),
                })??;
        if pong == "PONG" {
            Ok(())
        } else {
            Err(DriverError::RedisParse(format!(
                "unexpected PING response: {pong}"
            )))
        }
    }

    pub async fn close(&self) {}

    pub async fn execute_command(
        &self,
        request: &RedisExecuteRequest,
    ) -> Result<QueryResult, DriverError> {
        request
            .validate()
            .map_err(|error| DriverError::InvalidConfig {
                driver: DriverKind::Redis,
                message: error.to_string(),
            })?;
        let Some((command_name, arguments)) = request.argv().split_first() else {
            return Err(DriverError::RedisParse("command argv is empty".to_owned()));
        };
        let command_name = std::str::from_utf8(command_name)
            .map_err(|_| DriverError::RedisParse("command name is not UTF-8".to_owned()))?;
        let mut command = ::redis::cmd(command_name);
        for argument in arguments {
            command.arg(argument.as_slice());
        }
        let started = Instant::now();
        let mut connection = self.connection.clone();
        let value: ::redis::Value =
            tokio::time::timeout(request.timeout(), command.query_async(&mut connection))
                .await
                .map_err(|_| DriverError::Timeout {
                    driver: DriverKind::Redis,
                    seconds: request.timeout().as_secs(),
                })??;
        let (rows, truncated) = value_rows(value, request.row_limit() as usize);
        Ok(QueryResult {
            columns: vec![Column {
                name: "value".to_owned(),
                type_name: "RESP".to_owned(),
            }],
            rows,
            affected_rows: 0,
            last_insert_id: None,
            elapsed_ms: started.elapsed().as_millis(),
            truncated,
            backend_notices_present: false,
        })
    }

    pub async fn scan_keys(&self, request: &RedisScanRequest) -> Result<RedisKeyPage, DriverError> {
        let mut connection = self.connection.clone();
        crate::drivers::redis_browser::scan_keys(&mut connection, request).await
    }

    pub async fn inspect_key(
        &self,
        request: &RedisKeyInspectRequest,
    ) -> Result<RedisValuePreview, DriverError> {
        let mut connection = self.connection.clone();
        crate::drivers::redis_browser::inspect_key(&mut connection, request).await
    }
}

fn classify_required_tls_error(error: ::redis::RedisError) -> DriverError {
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(&error);
    while let Some(current) = source {
        if let Some(failure) = redis_tls_failure(current) {
            return DriverError::RedisTls { failure };
        }
        source = current.source();
    }
    DriverError::Redis(error)
}

fn redis_tls_failure(error: &(dyn std::error::Error + 'static)) -> Option<RedisTlsFailure> {
    if let Some(rustls::Error::InvalidCertificate(certificate)) =
        error.downcast_ref::<rustls::Error>()
    {
        return Some(match certificate {
            rustls::CertificateError::NotValidForName
            | rustls::CertificateError::NotValidForNameContext { .. } => {
                RedisTlsFailure::HostnameMismatch
            }
            _ => RedisTlsFailure::CaUntrusted,
        });
    }
    // tokio-rustls stores the typed rustls failure inside `io::Error`.
    // redis-rs in turn stores that as `Arc<dyn Error>` and exposes the Arc
    // itself as `source()`, so both wrappers must be opened explicitly.
    if let Some(io_error) = error.downcast_ref::<std::io::Error>()
        && let Some(inner) = io_error.get_ref()
    {
        return redis_tls_failure(inner);
    }
    if let Some(shared) =
        error.downcast_ref::<std::sync::Arc<dyn std::error::Error + Send + Sync + 'static>>()
    {
        return redis_tls_failure(shared.as_ref());
    }
    None
}

struct DecodeBudget {
    remaining_nodes: usize,
    remaining_bytes: usize,
    truncated: bool,
}

impl DecodeBudget {
    fn mark_truncated(&mut self) {
        self.truncated = true;
    }
}

struct DecodedCell {
    cell: Cell,
    stop_after: bool,
}

#[derive(Clone, Copy)]
struct CompositeMeasure {
    nodes: usize,
    encoded_bytes: usize,
}

fn value_rows(value: ::redis::Value, row_limit: usize) -> (Vec<Vec<Cell>>, bool) {
    let mut budget = DecodeBudget {
        remaining_nodes: MAX_REDIS_CELLS,
        remaining_bytes: MAX_RESULT_BYTES,
        truncated: false,
    };
    match value {
        ::redis::Value::Array(values) => {
            let mut truncated = values.len() > row_limit;
            let mut rows = Vec::with_capacity(values.len().min(row_limit));
            for value in values.into_iter().take(row_limit) {
                let Some(decoded) = decode_value_cell(value, &mut budget) else {
                    truncated = true;
                    break;
                };
                rows.push(vec![decoded.cell]);
                if decoded.stop_after {
                    truncated = true;
                    break;
                }
            }
            (rows, truncated || budget.truncated)
        }
        _ if row_limit == 0 => (Vec::new(), true),
        value => {
            let Some(decoded) = decode_value_cell(value, &mut budget) else {
                return (Vec::new(), true);
            };
            (
                vec![vec![decoded.cell]],
                decoded.stop_after || budget.truncated,
            )
        }
    }
}

fn decode_value_cell(value: ::redis::Value, budget: &mut DecodeBudget) -> Option<DecodedCell> {
    if is_composite(&value) {
        return decode_composite(value, budget);
    }
    decode_scalar(value, budget)
}

fn is_composite(value: &::redis::Value) -> bool {
    matches!(
        value,
        ::redis::Value::Array(_)
            | ::redis::Value::Map(_)
            | ::redis::Value::Attribute { .. }
            | ::redis::Value::Set(_)
            | ::redis::Value::Push { .. }
    )
}

fn decode_composite(value: ::redis::Value, budget: &mut DecodeBudget) -> Option<DecodedCell> {
    let Some(measure) = measure_composite(
        &value,
        budget.remaining_nodes,
        budget.remaining_bytes.min(MAX_REDIS_CELL_BYTES),
    ) else {
        budget.mark_truncated();
        return None;
    };
    budget.remaining_nodes = budget.remaining_nodes.saturating_sub(measure.nodes);
    budget.remaining_bytes = budget.remaining_bytes.saturating_sub(measure.encoded_bytes);
    Some(DecodedCell {
        cell: value_cell_complete(value),
        stop_after: false,
    })
}

fn decode_scalar(value: ::redis::Value, budget: &mut DecodeBudget) -> Option<DecodedCell> {
    let Some(remaining_nodes) = budget.remaining_nodes.checked_sub(1) else {
        budget.mark_truncated();
        return None;
    };
    let (retained_bytes, previewable) = scalar_measure(&value);
    let crosses_limit =
        retained_bytes > MAX_REDIS_CELL_BYTES || retained_bytes > budget.remaining_bytes;
    if crosses_limit && !previewable {
        budget.mark_truncated();
        return None;
    }
    budget.remaining_nodes = remaining_nodes;
    if crosses_limit {
        budget.remaining_bytes = 0;
        budget.mark_truncated();
    } else {
        budget.remaining_bytes = budget.remaining_bytes.saturating_sub(retained_bytes);
    }
    Some(DecodedCell {
        cell: value_cell_complete(value),
        stop_after: crosses_limit,
    })
}

fn scalar_measure(value: &::redis::Value) -> (usize, bool) {
    match value {
        ::redis::Value::Nil => (4, false),
        ::redis::Value::Int(value) => (signed_decimal_digits(*value), false),
        ::redis::Value::BulkString(value) => (value.len(), true),
        ::redis::Value::SimpleString(value) => (value.len(), true),
        ::redis::Value::Okay => (2, true),
        ::redis::Value::Double(value) => (json_float_bytes(*value), false),
        ::redis::Value::Boolean(value) => (if *value { 4 } else { 5 }, false),
        ::redis::Value::VerbatimString { text, .. } => (text.len(), true),
        ::redis::Value::BigNumber(value) => (decimal_digits_from_bits(value.bits()), false),
        ::redis::Value::ServerError(_) => ("[redis-server-error]".len(), false),
        _ => ("[unsupported-resp-value]".len(), false),
    }
}

struct CompositeMeter {
    remaining_nodes: usize,
    remaining_bytes: usize,
    nodes: usize,
    encoded_bytes: usize,
}

impl CompositeMeter {
    fn consume_node(&mut self, depth: usize) -> Option<()> {
        if depth >= MAX_REDIS_DEPTH {
            return None;
        }
        self.remaining_nodes = self.remaining_nodes.checked_sub(1)?;
        self.nodes = self.nodes.checked_add(1)?;
        Some(())
    }

    fn add_bytes(&mut self, bytes: usize) -> Option<()> {
        self.remaining_bytes = self.remaining_bytes.checked_sub(bytes)?;
        self.encoded_bytes = self.encoded_bytes.checked_add(bytes)?;
        Some(())
    }
}

fn measure_composite(
    value: &::redis::Value,
    remaining_nodes: usize,
    byte_limit: usize,
) -> Option<CompositeMeasure> {
    let mut meter = CompositeMeter {
        remaining_nodes,
        remaining_bytes: byte_limit,
        nodes: 0,
        encoded_bytes: 0,
    };
    measure_json_value_into(value, 0, &mut meter)?;
    Some(CompositeMeasure {
        nodes: meter.nodes,
        encoded_bytes: meter.encoded_bytes,
    })
}

fn measure_json_value_into(
    value: &::redis::Value,
    depth: usize,
    meter: &mut CompositeMeter,
) -> Option<()> {
    meter.consume_node(depth)?;
    match value {
        ::redis::Value::Nil => meter.add_bytes(4),
        ::redis::Value::Int(value) => meter.add_bytes(signed_decimal_digits(*value)),
        ::redis::Value::BulkString(value) => match std::str::from_utf8(value) {
            Ok(text) => meter.add_bytes(json_string_bytes(text)?),
            Err(_) => {
                let encoded = base64_encoded_bytes(value.len())?;
                meter.add_bytes(
                    47_usize
                        .checked_add(encoded)?
                        .checked_add(decimal_digits(value.len()))?,
                )
            }
        },
        ::redis::Value::SimpleString(value) => meter.add_bytes(json_string_bytes(value)?),
        ::redis::Value::Okay => meter.add_bytes(4),
        ::redis::Value::Double(value) => meter.add_bytes(json_float_bytes(*value)),
        ::redis::Value::Boolean(value) => meter.add_bytes(if *value { 4 } else { 5 }),
        ::redis::Value::Array(values) | ::redis::Value::Set(values) => {
            measure_json_array(values, depth + 1, meter)
        }
        ::redis::Value::Map(entries) => measure_json_entries(entries, depth + 1, meter),
        ::redis::Value::Attribute { data, attributes } => {
            meter.add_bytes(23)?;
            measure_json_entries(attributes, depth + 1, meter)?;
            measure_json_value_into(data, depth + 1, meter)
        }
        ::redis::Value::VerbatimString { text, .. } => meter.add_bytes(json_string_bytes(text)?),
        ::redis::Value::BigNumber(value) => meter.add_bytes(decimal_digits_from_bits(value.bits())),
        ::redis::Value::Push { data, .. } => {
            meter.add_bytes(23)?;
            measure_json_array(data, depth + 1, meter)
        }
        ::redis::Value::ServerError(_) => {
            meter.add_bytes(json_string_bytes("[redis-server-error]")?)
        }
        _ => meter.add_bytes(json_string_bytes("[unsupported-resp-value]")?),
    }
}

fn measure_json_array(
    values: &[::redis::Value],
    depth: usize,
    meter: &mut CompositeMeter,
) -> Option<()> {
    meter.add_bytes(2_usize.checked_add(values.len().saturating_sub(1))?)?;
    for value in values {
        measure_json_value_into(value, depth, meter)?;
    }
    Some(())
}

fn measure_json_entries(
    entries: &[(::redis::Value, ::redis::Value)],
    depth: usize,
    meter: &mut CompositeMeter,
) -> Option<()> {
    meter.add_bytes(2_usize.checked_add(entries.len().saturating_sub(1))?)?;
    for (key, value) in entries {
        meter.add_bytes(17)?;
        measure_json_value_into(key, depth, meter)?;
        measure_json_value_into(value, depth, meter)?;
    }
    Some(())
}

fn json_string_bytes(value: &str) -> Option<usize> {
    let mut bytes = 2_usize;
    for character in value.chars() {
        let encoded = match character {
            '"' | '\\' | '\u{08}' | '\u{0c}' | '\n' | '\r' | '\t' => 2,
            character if character <= '\u{1f}' => 6,
            character => character.len_utf8(),
        };
        bytes = bytes.checked_add(encoded)?;
    }
    Some(bytes)
}

fn base64_encoded_bytes(bytes: usize) -> Option<usize> {
    bytes.checked_add(2)?.checked_div(3)?.checked_mul(4)
}

fn decimal_digits(mut value: usize) -> usize {
    if value == 0 {
        return 1;
    }
    let mut digits = 0;
    while value != 0 {
        digits += 1;
        value /= 10;
    }
    digits
}

fn signed_decimal_digits(value: i64) -> usize {
    let mut magnitude = value.unsigned_abs();
    let mut digits = usize::from(value.is_negative());
    if magnitude == 0 {
        return 1;
    }
    while magnitude != 0 {
        digits += 1;
        magnitude /= 10;
    }
    digits
}

fn json_float_bytes(value: f64) -> usize {
    serde_json::Number::from_f64(value).map_or(4, |number| number.to_string().len())
}

fn decimal_digits_from_bits(bits: u64) -> usize {
    usize::try_from(bits)
        .unwrap_or(usize::MAX)
        .saturating_mul(30_103)
        .saturating_div(100_000)
        .saturating_add(2)
}

fn value_cell_complete(value: ::redis::Value) -> Cell {
    match value {
        ::redis::Value::Nil => Cell::Null,
        ::redis::Value::Int(value) => Cell::Int(value),
        ::redis::Value::BulkString(value) => String::from_utf8(value)
            .map(Cell::Text)
            .unwrap_or_else(|error| bytes_cell(error.into_bytes())),
        ::redis::Value::SimpleString(value) => Cell::Text(value),
        ::redis::Value::Okay => Cell::Text("OK".to_owned()),
        ::redis::Value::Double(value) => Cell::Float(value),
        ::redis::Value::Boolean(value) => Cell::Bool(value),
        ::redis::Value::Array(values) | ::redis::Value::Set(values) => {
            Cell::Json(serde_json::Value::Array(values_json_complete(values)))
        }
        ::redis::Value::Map(entries) => {
            Cell::Json(serde_json::Value::Array(entries_json_complete(entries)))
        }
        ::redis::Value::Attribute { data, attributes } => {
            let data = value_json_complete(*data);
            let attributes = entries_json_complete(attributes);
            Cell::Json(serde_json::json!({ "data": data, "attributes": attributes }))
        }
        ::redis::Value::VerbatimString { text, .. } => Cell::Text(text),
        ::redis::Value::BigNumber(value) => {
            #[cfg(test)]
            BIG_NUMBER_STRINGIFICATIONS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Cell::Decimal(value.to_string())
        }
        ::redis::Value::Push { data, .. } => Cell::Json(serde_json::json!({
            "type": "push",
            "data": values_json_complete(data)
        })),
        ::redis::Value::ServerError(_) => Cell::Text("[redis-server-error]".to_owned()),
        _ => Cell::Text("[unsupported-resp-value]".to_owned()),
    }
}

fn values_json_complete(values: Vec<::redis::Value>) -> Vec<serde_json::Value> {
    values.into_iter().map(value_json_complete).collect()
}

fn entries_json_complete(entries: Vec<(::redis::Value, ::redis::Value)>) -> Vec<serde_json::Value> {
    let mut decoded = Vec::with_capacity(entries.len());
    for (key, value) in entries {
        let key = value_json_complete(key);
        let value = value_json_complete(value);
        decoded.push(serde_json::json!({ "key": key, "value": value }));
    }
    decoded
}

fn value_json_complete(value: ::redis::Value) -> serde_json::Value {
    match value_cell_complete(value) {
        Cell::Null => serde_json::Value::Null,
        Cell::Bool(value) => value.into(),
        Cell::Int(value) => value.into(),
        Cell::UInt(value) => value.into(),
        Cell::Float(value) => serde_json::json!(value),
        Cell::Decimal(value) | Cell::Text(value) | Cell::DateTime(value) => value.into(),
        Cell::TextPreview {
            preview,
            original_len,
        } => serde_json::json!({
            "preview": preview,
            "original_len": original_len,
            "truncated": true,
        }),
        Cell::Bytes {
            retained,
            original_len,
        } => serde_json::json!({
            "base64": base64::engine::general_purpose::STANDARD.encode(&retained),
            "original_len": original_len,
            "truncated": retained.len() < original_len,
        }),
        Cell::Json(value) => value,
        Cell::JsonPreview {
            preview,
            original_len,
        } => serde_json::json!({
            "preview": preview,
            "original_len": original_len,
            "truncated": true,
        }),
    }
}

fn bytes_cell(bytes: Vec<u8>) -> Cell {
    let original_len = bytes.len();
    Cell::Bytes {
        retained: bytes,
        original_len,
    }
}

#[cfg(test)]
static BIG_NUMBER_STRINGIFICATIONS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

#[cfg(test)]
mod tests {
    use super::*;

    static BIG_NUMBER_TEST_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn retain_decoded(rows: Vec<Vec<Cell>>, truncated: bool) -> crate::model::ResultSnapshot {
        use crate::model::{
            OperationId, ProfileGeneration, ProfileId, ResultId, ResultProvenance,
            ResultRetentionPolicy, ResultSnapshot,
        };

        ResultSnapshot::retain(
            QueryResult {
                columns: vec![Column {
                    name: "value".to_owned(),
                    type_name: "RESP".to_owned(),
                }],
                rows,
                affected_rows: 0,
                last_insert_id: None,
                elapsed_ms: 0,
                truncated,
                backend_notices_present: false,
            },
            ResultProvenance {
                result_id: ResultId(71),
                profile_id: ProfileId("profile-redis-decode".to_owned()),
                profile_generation: ProfileGeneration(7),
                operation_id: OperationId(17),
                driver: DriverKind::Redis,
                completed_at_unix_ms: 1_700_000_000_123,
                duration_ms: 19,
            },
            ResultRetentionPolicy::redis(1),
        )
    }

    #[test]
    fn flat_array_becomes_one_row_per_value() {
        let (rows, truncated) = value_rows(
            ::redis::Value::Array(vec![
                ::redis::Value::Int(1),
                ::redis::Value::BulkString(b"two".to_vec()),
            ]),
            2,
        );
        assert!(!truncated);
        assert_eq!(
            rows,
            vec![vec![Cell::Int(1)], vec![Cell::Text("two".into())]]
        );
    }

    #[test]
    fn nil_and_nested_resp_values_keep_their_shapes() {
        assert_eq!(
            value_rows(::redis::Value::Nil, 1),
            (vec![vec![Cell::Null]], false)
        );

        let (rows, truncated) = value_rows(
            ::redis::Value::Array(vec![::redis::Value::Map(vec![(
                ::redis::Value::SimpleString("key".to_owned()),
                ::redis::Value::Array(vec![::redis::Value::Int(1), ::redis::Value::Nil]),
            )])]),
            1,
        );
        assert!(!truncated);
        assert_eq!(
            rows,
            vec![vec![Cell::Json(serde_json::json!([
                {"key": "key", "value": [1, null]}
            ]))]]
        );
    }

    #[test]
    fn response_rows_respect_the_request_limit() {
        let (rows, truncated) = value_rows(
            ::redis::Value::Array(vec![::redis::Value::Int(1), ::redis::Value::Int(2)]),
            1,
        );
        assert_eq!(rows, vec![vec![Cell::Int(1)]]);
        assert!(truncated);
    }

    #[test]
    fn top_level_composite_propagates_depth_and_node_truncation() {
        let mut nested = ::redis::Value::Int(1);
        for _ in 0..=MAX_REDIS_DEPTH {
            nested = ::redis::Value::Array(vec![nested]);
        }
        let (depth_rows, depth_truncated) = value_rows(
            ::redis::Value::Array(vec![
                ::redis::Value::Int(7),
                ::redis::Value::Map(vec![(
                    ::redis::Value::SimpleString("key".to_owned()),
                    nested,
                )]),
            ]),
            100,
        );
        assert!(depth_truncated);
        assert_eq!(depth_rows, vec![vec![Cell::Int(7)]]);

        let (node_rows, nodes_truncated) = value_rows(
            ::redis::Value::Array(vec![
                ::redis::Value::Int(8),
                ::redis::Value::Set(
                    (0..=MAX_REDIS_CELLS)
                        .map(|value| ::redis::Value::Int(value as i64))
                        .collect(),
                ),
            ]),
            100,
        );
        assert!(nodes_truncated);
        assert_eq!(node_rows, vec![vec![Cell::Int(8)]]);
    }

    #[test]
    fn oversized_nested_text_and_binary_drop_before_derived_json_allocation() {
        for oversized in [
            ::redis::Value::BulkString(vec![b'x'; crate::model::MAX_REDIS_CELL_BYTES + 1]),
            ::redis::Value::BulkString(vec![0xff; crate::model::MAX_REDIS_CELL_BYTES + 1]),
        ] {
            let (rows, truncated) = value_rows(
                ::redis::Value::Array(vec![
                    ::redis::Value::Int(9),
                    ::redis::Value::Map(vec![(
                        ::redis::Value::SimpleString("payload".to_owned()),
                        oversized,
                    )]),
                    ::redis::Value::Int(10),
                ]),
                3,
            );

            assert!(truncated);
            assert_eq!(rows, vec![vec![Cell::Int(9)]]);
        }
    }

    #[test]
    fn nested_composite_derived_bytes_never_cross_the_total_snapshot_cap() {
        let values = (0..200)
            .map(|_| {
                ::redis::Value::Map(vec![(
                    ::redis::Value::SimpleString("payload".to_owned()),
                    ::redis::Value::BulkString(vec![b'x'; 60 * 1024]),
                )])
            })
            .collect();
        let (rows, truncated) = value_rows(::redis::Value::Array(values), 200);
        let derived_bytes = rows
            .iter()
            .map(|row| match &row[0] {
                Cell::Json(value) => serde_json::to_vec(value)
                    .expect("serialize retained composite")
                    .len(),
                _ => 0,
            })
            .fold(0_usize, usize::saturating_add);

        assert!(truncated);
        assert!(derived_bytes <= crate::model::MAX_RESULT_BYTES);
        assert!(rows.len() < 200);
    }

    #[test]
    fn nested_big_numbers_count_json_quotes_before_the_per_cell_cap() {
        let _serial = BIG_NUMBER_TEST_SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        BIG_NUMBER_STRINGIFICATIONS.store(0, std::sync::atomic::Ordering::SeqCst);
        let negative_hundred = ::redis::Value::BigNumber(
            "-100"
                .parse()
                .expect("parse the nested BigNumber counterexample"),
        );
        let values = (0..9_363).map(|_| negative_hundred.clone()).collect();

        let (rows, truncated) = value_rows(::redis::Value::Set(values), 1);
        let actual_bytes = rows.first().map_or(0, |row| match &row[0] {
            Cell::Json(value) => serde_json::to_vec(value)
                .expect("serialize nested BigNumber counterexample")
                .len(),
            _ => 0,
        });
        let stringifications =
            BIG_NUMBER_STRINGIFICATIONS.load(std::sync::atomic::Ordering::SeqCst);

        assert!(
            truncated
                && rows.is_empty()
                && actual_bytes <= MAX_REDIS_CELL_BYTES
                && stringifications == 0,
            "nested BigNumbers crossed the cell cap: bytes={actual_bytes}, stringifications={stringifications}, rows={}, truncated={truncated}",
            rows.len()
        );
    }

    #[test]
    fn cumulative_nested_big_numbers_count_sign_and_quotes_before_the_total_cap() {
        let _serial = BIG_NUMBER_TEST_SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        BIG_NUMBER_STRINGIFICATIONS.store(0, std::sync::atomic::Ordering::SeqCst);
        let decimal = format!("-{}", "9".repeat(1_674));
        let big_number = ::redis::Value::BigNumber(
            decimal
                .parse()
                .expect("parse the cumulative BigNumber counterexample"),
        );
        let values = (0..4_999)
            .map(|_| ::redis::Value::Array(vec![big_number.clone()]))
            .collect();

        let (rows, truncated) = value_rows(::redis::Value::Array(values), 4_999);
        let actual_bytes = rows
            .iter()
            .map(|row| match &row[0] {
                Cell::Json(value) => serde_json::to_vec(value)
                    .expect("serialize cumulative BigNumber counterexample")
                    .len(),
                _ => 0,
            })
            .fold(0_usize, usize::saturating_add);
        let stringifications =
            BIG_NUMBER_STRINGIFICATIONS.load(std::sync::atomic::Ordering::SeqCst);

        assert!(
            truncated
                && rows.len() < 4_999
                && actual_bytes <= MAX_RESULT_BYTES
                && stringifications == rows.len(),
            "nested BigNumbers crossed the total cap: bytes={actual_bytes}, stringifications={stringifications}, rows={}, truncated={truncated}",
            rows.len()
        );
    }

    #[test]
    fn enormous_top_level_big_number_is_rejected_before_decimal_string_allocation() {
        let _serial = BIG_NUMBER_TEST_SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        BIG_NUMBER_STRINGIFICATIONS.store(0, std::sync::atomic::Ordering::SeqCst);
        let decimal = format!("-{}", "9".repeat(MAX_REDIS_CELL_BYTES + 1));
        let value = ::redis::Value::BigNumber(
            decimal
                .parse()
                .expect("parse the top-level BigNumber counterexample"),
        );

        let (rows, truncated) = value_rows(value, 1);

        assert!(truncated);
        assert!(rows.is_empty());
        assert_eq!(
            BIG_NUMBER_STRINGIFICATIONS.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "top-level preflight must reject before BigNumber::to_string"
        );
    }

    #[test]
    fn nested_composite_accepts_the_exact_cell_byte_boundary_and_rejects_the_next_byte() {
        const MAP_WRAPPER_BYTES: usize = 30;
        let value = |payload_bytes| {
            ::redis::Value::Map(vec![(
                ::redis::Value::SimpleString("payload".to_owned()),
                ::redis::Value::BulkString(vec![b'x'; payload_bytes]),
            )])
        };

        let (exact_rows, exact_truncated) =
            value_rows(value(MAX_REDIS_CELL_BYTES - MAP_WRAPPER_BYTES), 1);
        assert!(!exact_truncated);
        let Cell::Json(exact) = &exact_rows[0][0] else {
            panic!("exact-boundary composite must stay complete");
        };
        assert_eq!(
            serde_json::to_vec(exact)
                .expect("serialize exact-boundary composite")
                .len(),
            MAX_REDIS_CELL_BYTES
        );

        let (oversized_rows, oversized_truncated) =
            value_rows(value(MAX_REDIS_CELL_BYTES - MAP_WRAPPER_BYTES + 1), 1);
        assert!(oversized_truncated);
        assert!(oversized_rows.is_empty());
    }

    #[test]
    fn exact_depth_and_node_boundaries_remain_complete() {
        let mut exact_depth = ::redis::Value::Int(1);
        for _ in 0..MAX_REDIS_DEPTH - 1 {
            exact_depth = ::redis::Value::Array(vec![exact_depth]);
        }
        let (depth_rows, depth_truncated) = value_rows(exact_depth, 1);
        assert!(!depth_truncated);
        assert_eq!(depth_rows.len(), 1);

        let exact_nodes = ::redis::Value::Set(
            (0..MAX_REDIS_CELLS - 1)
                .map(|_| ::redis::Value::Int(0))
                .collect(),
        );
        let (node_rows, node_truncated) = value_rows(exact_nodes, 1);
        assert!(!node_truncated);
        let Cell::Json(serde_json::Value::Array(values)) = &node_rows[0][0] else {
            panic!("exact-node-boundary composite must stay complete");
        };
        assert_eq!(values.len(), MAX_REDIS_CELLS - 1);
    }

    #[test]
    fn complete_nested_composites_and_top_level_bytes_keep_exact_identity() {
        let (rows, truncated) = value_rows(
            ::redis::Value::Array(vec![
                ::redis::Value::Map(vec![(
                    ::redis::Value::SimpleString("key".to_owned()),
                    ::redis::Value::Array(vec![::redis::Value::Int(1), ::redis::Value::Nil]),
                )]),
                ::redis::Value::BulkString(vec![0, 0xff]),
            ]),
            2,
        );

        assert!(!truncated);
        assert_eq!(
            rows,
            vec![
                vec![Cell::Json(serde_json::json!([
                    {"key": "key", "value": [1, null]}
                ]))],
                vec![Cell::Bytes {
                    retained: vec![0, 0xff],
                    original_len: 2,
                }],
            ]
        );
    }

    #[test]
    fn oversized_top_level_text_and_bytes_reach_snapshot_preview_truth_without_a_copy() {
        let original_len = MAX_REDIS_CELL_BYTES + 1;
        let (text_rows, text_truncated) =
            value_rows(::redis::Value::BulkString(vec![b'x'; original_len]), 1);
        let text = retain_decoded(text_rows, text_truncated);
        assert!(text_truncated);
        assert!(matches!(
            &text.rows[0][0],
            Cell::TextPreview {
                preview,
                original_len: retained_original_len,
            } if preview.len() == MAX_REDIS_CELL_BYTES && *retained_original_len == original_len
        ));

        let (byte_rows, byte_truncated) =
            value_rows(::redis::Value::BulkString(vec![0xff; original_len]), 1);
        let bytes = retain_decoded(byte_rows, byte_truncated);
        assert!(byte_truncated);
        assert!(matches!(
            &bytes.rows[0][0],
            Cell::Bytes {
                retained,
                original_len: retained_original_len,
            } if retained.len() == MAX_REDIS_CELL_BYTES && *retained_original_len == original_len
        ));
    }
}
