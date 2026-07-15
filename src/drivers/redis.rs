use std::time::{Duration, Instant};

use redis::IntoConnectionInfo as _;
use secrecy::{ExposeSecret as _, SecretString};

use crate::drivers::DriverError;
use crate::model::{
    Cell, Column, ConnectionProfile, DriverAvailability, DriverCapabilities, DriverDescriptor,
    DriverKind, MAX_REDIS_CELLS, MAX_REDIS_DEPTH, QueryLanguage, QueryResult, RedisExecuteRequest,
    RedisKeyInspectRequest, RedisKeyPage, RedisScanRequest, RedisValuePreview, TlsMode,
};

pub const DESCRIPTOR: DriverDescriptor = DriverDescriptor {
    kind: DriverKind::Redis,
    display_name: "Redis",
    default_port: 6379,
    availability: DriverAvailability::Ready,
    languages: &[QueryLanguage::RedisCommand],
    capabilities: DriverCapabilities::CONNECT
        .union(DriverCapabilities::PING)
        .union(DriverCapabilities::COMMAND),
    planned_capabilities: DriverCapabilities::KEYSPACE_BROWSE,
    reason: None,
};

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
        if profile.tls != TlsMode::Disabled {
            return Err(DriverError::Unsupported {
                driver: DriverKind::Redis,
                operation: "non-plaintext transport is not implemented".to_owned(),
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
        let info = (profile.host.clone(), profile.port)
            .into_connection_info()?
            .set_redis_settings(redis);
        let client = ::redis::Client::open(info)?;
        let connection = tokio::time::timeout(timeout, client.get_connection_manager())
            .await
            .map_err(|_| DriverError::Timeout {
                driver: DriverKind::Redis,
                seconds: timeout.as_secs(),
            })??;
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

    pub async fn scan_keys(
        &self,
        _request: &RedisScanRequest,
    ) -> Result<RedisKeyPage, DriverError> {
        Err(DriverError::Unsupported {
            driver: DriverKind::Redis,
            operation: "keyspace browsing is planned".to_owned(),
        })
    }

    pub async fn inspect_key(
        &self,
        _request: &RedisKeyInspectRequest,
    ) -> Result<RedisValuePreview, DriverError> {
        Err(DriverError::Unsupported {
            driver: DriverKind::Redis,
            operation: "key inspection is planned".to_owned(),
        })
    }
}

struct DecodeBudget {
    remaining_nodes: usize,
    truncated: bool,
}

impl DecodeBudget {
    fn consume(&mut self) -> bool {
        let Some(remaining) = self.remaining_nodes.checked_sub(1) else {
            return false;
        };
        self.remaining_nodes = remaining;
        true
    }
}

fn value_rows(value: ::redis::Value, row_limit: usize) -> (Vec<Vec<Cell>>, bool) {
    let mut budget = DecodeBudget {
        remaining_nodes: MAX_REDIS_CELLS,
        truncated: false,
    };
    match value {
        ::redis::Value::Array(values) => {
            let truncated = values.len() > row_limit;
            let rows = values
                .into_iter()
                .take(row_limit)
                .map(|value| vec![value_cell(value, &mut budget, 0)])
                .collect();
            (rows, truncated || budget.truncated)
        }
        _ if row_limit == 0 => (Vec::new(), true),
        value => {
            let cell = value_cell(value, &mut budget, 0);
            (vec![vec![cell]], budget.truncated)
        }
    }
}

fn value_cell(value: ::redis::Value, budget: &mut DecodeBudget, depth: usize) -> Cell {
    if depth >= MAX_REDIS_DEPTH || !budget.consume() {
        budget.truncated = true;
        return Cell::Text("[dbotter-truncated]".to_owned());
    }
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
        ::redis::Value::Array(values) | ::redis::Value::Set(values) => Cell::Json(
            serde_json::Value::Array(values_json(values, budget, depth + 1)),
        ),
        ::redis::Value::Map(entries) => Cell::Json(serde_json::Value::Array(entries_json(
            entries,
            budget,
            depth + 1,
        ))),
        ::redis::Value::Attribute { data, attributes } => {
            let data = value_json(*data, budget, depth + 1);
            let attributes = entries_json(attributes, budget, depth + 1);
            Cell::Json(serde_json::json!({ "data": data, "attributes": attributes }))
        }
        ::redis::Value::VerbatimString { text, .. } => Cell::Text(text),
        ::redis::Value::BigNumber(value) => Cell::Decimal(value.to_string()),
        ::redis::Value::Push { data, .. } => Cell::Json(serde_json::json!({
            "type": "push",
            "data": values_json(data, budget, depth + 1)
        })),
        ::redis::Value::ServerError(_) => Cell::Text("[redis-server-error]".to_owned()),
        _ => Cell::Text("[unsupported-resp-value]".to_owned()),
    }
}

fn values_json(
    values: Vec<::redis::Value>,
    budget: &mut DecodeBudget,
    depth: usize,
) -> Vec<serde_json::Value> {
    let mut decoded = Vec::with_capacity(values.len().min(budget.remaining_nodes));
    for value in values {
        if budget.remaining_nodes == 0 || depth >= MAX_REDIS_DEPTH {
            budget.truncated = true;
            break;
        }
        decoded.push(value_json(value, budget, depth));
    }
    decoded
}

fn entries_json(
    entries: Vec<(::redis::Value, ::redis::Value)>,
    budget: &mut DecodeBudget,
    depth: usize,
) -> Vec<serde_json::Value> {
    let mut decoded = Vec::with_capacity(entries.len().min(budget.remaining_nodes / 2));
    for (key, value) in entries {
        if budget.remaining_nodes == 0 || depth >= MAX_REDIS_DEPTH {
            budget.truncated = true;
            break;
        }
        let key = value_json(key, budget, depth);
        let value = value_json(value, budget, depth);
        decoded.push(serde_json::json!({ "key": key, "value": value }));
    }
    decoded
}

fn value_json(value: ::redis::Value, budget: &mut DecodeBudget, depth: usize) -> serde_json::Value {
    match value_cell(value, budget, depth) {
        Cell::Null => serde_json::Value::Null,
        Cell::Bool(value) => value.into(),
        Cell::Int(value) => value.into(),
        Cell::UInt(value) => value.into(),
        Cell::Float(value) => serde_json::json!(value),
        Cell::Decimal(value) | Cell::Text(value) | Cell::DateTime(value) => value.into(),
        Cell::Bytes { preview, len } => serde_json::json!({ "preview": preview, "len": len }),
        Cell::Json(value) => value,
    }
}

fn bytes_cell(bytes: Vec<u8>) -> Cell {
    let mut preview = String::with_capacity(bytes.len().min(32) * 2 + 1);
    for byte in bytes.iter().take(32) {
        use std::fmt::Write as _;
        let _ = write!(&mut preview, "{byte:02x}");
    }
    if bytes.len() > 32 {
        preview.push('…');
    }
    Cell::Bytes {
        preview,
        len: bytes.len(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let (_, depth_truncated) = value_rows(
            ::redis::Value::Map(vec![(
                ::redis::Value::SimpleString("key".to_owned()),
                nested,
            )]),
            100,
        );
        assert!(depth_truncated);

        let (_, nodes_truncated) = value_rows(
            ::redis::Value::Set(
                (0..=MAX_REDIS_CELLS)
                    .map(|value| ::redis::Value::Int(value as i64))
                    .collect(),
            ),
            100,
        );
        assert!(nodes_truncated);
    }
}
