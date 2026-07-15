//! Redis SCAN and bounded representative inspection.
//!
//! Every command name in this module is static. Raw key bytes are the only
//! identity accepted by inspection; display strings never flow back to Redis.

use std::collections::HashSet;
use std::time::Duration;

use base64::Engine as _;
use redis::aio::ConnectionManager;

use crate::drivers::DriverError;
use crate::model::{
    Cell, DriverKind, MAX_REDIS_CELL_BYTES, MAX_REDIS_DEPTH, MAX_REDIS_KEY_BYTES, MAX_REDIS_KEYS,
    MAX_REDIS_PREVIEW_BYTES, MAX_REDIS_PREVIEW_ITEMS, MAX_REDIS_RETAINED_KEY_BYTES, RedisKeyEntry,
    RedisKeyFilter, RedisKeyInspectRequest, RedisKeyPage, RedisScanConsistency, RedisScanRequest,
    RedisTtl, RedisValuePreview, RedisValueType, ResultNotice, TransientAllocationQualification,
};

type RawHashEntry = (Vec<u8>, Vec<u8>);

/// UI/runtime-owned state for one weakly consistent SCAN cycle.
///
/// The driver performs exactly one SCAN per page. This accumulator owns the
/// cross-page raw-byte dedupe and per-profile retained caps.
pub struct RedisScanAccumulator {
    filter: RedisKeyFilter,
    keys: Vec<RedisKeyEntry>,
    seen: HashSet<Vec<u8>>,
    next_cursor: u64,
    requested: bool,
    skipped_oversize: usize,
    retained_bytes: usize,
    truncated: bool,
    stale: bool,
}

impl RedisScanAccumulator {
    pub fn new(filter: RedisKeyFilter) -> Self {
        Self {
            filter,
            keys: Vec::new(),
            seen: HashSet::new(),
            next_cursor: 0,
            requested: false,
            skipped_oversize: 0,
            retained_bytes: 0,
            truncated: false,
            stale: false,
        }
    }

    pub fn restart(&mut self, filter: RedisKeyFilter) {
        *self = Self::new(filter);
    }

    pub fn apply_page(&mut self, page: RedisKeyPage) {
        self.requested = true;
        self.next_cursor = page.next_cursor;
        self.skipped_oversize = self.skipped_oversize.saturating_add(page.skipped_oversize);
        self.truncated |= page.truncated || page.skipped_oversize > 0;
        self.stale |= page.stale;
        for key in page.keys {
            let raw = key.id.as_bytes();
            if raw.len() > MAX_REDIS_KEY_BYTES {
                self.skipped_oversize = self.skipped_oversize.saturating_add(1);
                self.truncated = true;
                continue;
            }
            if self.seen.contains(raw) {
                continue;
            }
            let next_bytes = self.retained_bytes.saturating_add(raw.len());
            if self.keys.len() >= MAX_REDIS_KEYS || next_bytes > MAX_REDIS_RETAINED_KEY_BYTES {
                self.truncated = true;
                continue;
            }
            self.seen.insert(raw.to_vec());
            self.retained_bytes = next_bytes;
            self.keys.push(key);
        }
    }

    pub fn filter(&self) -> &RedisKeyFilter {
        &self.filter
    }

    pub fn keys(&self) -> &[RedisKeyEntry] {
        &self.keys
    }

    pub const fn next_cursor(&self) -> u64 {
        self.next_cursor
    }

    pub const fn is_complete(&self) -> bool {
        self.requested && self.next_cursor == 0
    }

    pub const fn skipped_oversize(&self) -> usize {
        self.skipped_oversize
    }

    pub const fn retained_bytes(&self) -> usize {
        self.retained_bytes
    }

    pub const fn truncated(&self) -> bool {
        self.truncated
    }

    pub const fn stale(&self) -> bool {
        self.stale
    }

    pub fn mark_stale(&mut self) {
        self.stale = true;
    }
}

pub const fn inspect_command_names(value_type: RedisValueType) -> &'static [&'static str] {
    match value_type {
        RedisValueType::String => &["TYPE", "PTTL", "STRLEN", "GETRANGE", "TYPE"],
        RedisValueType::Hash => &["TYPE", "PTTL", "HLEN", "HSCAN", "TYPE"],
        RedisValueType::List => &["TYPE", "PTTL", "LLEN", "LRANGE", "TYPE"],
        RedisValueType::Set => &["TYPE", "PTTL", "SCARD", "SSCAN", "TYPE"],
        RedisValueType::SortedSet => &["TYPE", "PTTL", "ZCARD", "ZRANGE", "TYPE"],
        RedisValueType::Stream => &["TYPE", "PTTL", "XLEN", "XRANGE", "TYPE"],
        RedisValueType::ModuleOrUnknown => &["TYPE", "PTTL", "TYPE"],
    }
}

pub async fn scan_keys(
    connection: &mut ConnectionManager,
    request: &RedisScanRequest,
) -> Result<RedisKeyPage, DriverError> {
    request
        .validate()
        .map_err(|error| DriverError::InvalidConfig {
            driver: DriverKind::Redis,
            message: error.to_string(),
        })?;
    let pattern = request
        .filter
        .match_pattern()
        .map_err(|error| DriverError::InvalidConfig {
            driver: DriverKind::Redis,
            message: error.to_string(),
        })?;
    let mut command = redis::cmd("SCAN");
    command
        .arg(request.cursor)
        .arg("MATCH")
        .arg(pattern.as_bytes())
        .arg("COUNT")
        .arg(request.count_hint);
    let (next_cursor, raw_keys): (u64, Vec<Vec<u8>>) =
        query(connection, request.timeout, &mut command).await?;

    let mut seen = HashSet::new();
    let mut keys = Vec::new();
    let mut retained_bytes = 0_usize;
    let mut skipped_oversize = 0_usize;
    let mut truncated = false;
    for raw in raw_keys {
        if raw.len() > MAX_REDIS_KEY_BYTES {
            skipped_oversize = skipped_oversize.saturating_add(1);
            truncated = true;
            continue;
        }
        if !seen.insert(raw.clone()) {
            continue;
        }
        let next_bytes = retained_bytes.saturating_add(raw.len());
        if keys.len() >= MAX_REDIS_KEYS || next_bytes > MAX_REDIS_RETAINED_KEY_BYTES {
            truncated = true;
            continue;
        }
        retained_bytes = next_bytes;
        keys.push(RedisKeyEntry::new(crate::model::RedisKeyId(raw)));
    }
    Ok(RedisKeyPage {
        identity: request.identity.clone(),
        next_cursor,
        retained_count: keys.len(),
        keys,
        skipped_oversize,
        retained_bytes,
        consistency: RedisScanConsistency::Weak,
        truncated,
        stale: false,
    })
}

pub async fn inspect_key(
    connection: &mut ConnectionManager,
    request: &RedisKeyInspectRequest,
) -> Result<RedisValuePreview, DriverError> {
    request
        .validate()
        .map_err(|error| DriverError::InvalidConfig {
            driver: DriverKind::Redis,
            message: error.to_string(),
        })?;
    let raw_key = request.key.as_bytes();
    let initial_type = key_type(connection, raw_key, request.timeout).await?;
    if initial_type == "none" {
        return Err(DriverError::RedisKeyMissing);
    }
    let value_type = value_type(&initial_type);
    let ttl = read_ttl(connection, raw_key, request.timeout).await?;
    let mut preview = PreviewRetention::new();
    let size = match value_type {
        RedisValueType::String => {
            let size: u64 = key_query(
                connection,
                request.timeout,
                redis::cmd("STRLEN").arg(raw_key),
            )
            .await?;
            let value: redis::Value = key_query(
                connection,
                request.timeout,
                redis::cmd("GETRANGE").arg(raw_key).arg(0).arg(65_535),
            )
            .await?;
            let bytes = bulk_bytes(value)?;
            preview.mark_more(size > u64::try_from(bytes.len()).unwrap_or(u64::MAX));
            preview.push_binary(bytes);
            Some(size)
        }
        RedisValueType::Hash => {
            let size: u64 =
                key_query(connection, request.timeout, redis::cmd("HLEN").arg(raw_key)).await?;
            let (next_cursor, entries): (u64, Vec<RawHashEntry>) = key_query(
                connection,
                request.timeout,
                redis::cmd("HSCAN")
                    .arg(raw_key)
                    .arg(0)
                    .arg("COUNT")
                    .arg(100),
            )
            .await?;
            preview.mark_more(next_cursor != 0 || size > entries.len() as u64);
            for (field, value) in entries {
                preview.push_pair("field", field, "value", value);
            }
            Some(size)
        }
        RedisValueType::List => {
            let size: u64 =
                key_query(connection, request.timeout, redis::cmd("LLEN").arg(raw_key)).await?;
            let values: Vec<Vec<u8>> = key_query(
                connection,
                request.timeout,
                redis::cmd("LRANGE").arg(raw_key).arg(0).arg(99),
            )
            .await?;
            preview.mark_more(size > values.len() as u64);
            for value in values {
                preview.push_binary(value);
            }
            Some(size)
        }
        RedisValueType::Set => {
            let size: u64 = key_query(
                connection,
                request.timeout,
                redis::cmd("SCARD").arg(raw_key),
            )
            .await?;
            let (next_cursor, values): (u64, Vec<Vec<u8>>) = key_query(
                connection,
                request.timeout,
                redis::cmd("SSCAN")
                    .arg(raw_key)
                    .arg(0)
                    .arg("COUNT")
                    .arg(100),
            )
            .await?;
            preview.mark_more(next_cursor != 0 || size > values.len() as u64);
            for value in values {
                preview.push_binary(value);
            }
            Some(size)
        }
        RedisValueType::SortedSet => {
            let size: u64 = key_query(
                connection,
                request.timeout,
                redis::cmd("ZCARD").arg(raw_key),
            )
            .await?;
            let values: Vec<(Vec<u8>, f64)> = key_query(
                connection,
                request.timeout,
                redis::cmd("ZRANGE")
                    .arg(raw_key)
                    .arg(0)
                    .arg(99)
                    .arg("WITHSCORES"),
            )
            .await?;
            preview.mark_more(size > values.len() as u64);
            for (member, score) in values {
                preview.push_scored(member, score);
            }
            Some(size)
        }
        RedisValueType::Stream => {
            let size: u64 =
                key_query(connection, request.timeout, redis::cmd("XLEN").arg(raw_key)).await?;
            let value: redis::Value = key_query(
                connection,
                request.timeout,
                redis::cmd("XRANGE")
                    .arg(raw_key)
                    .arg("-")
                    .arg("+")
                    .arg("COUNT")
                    .arg(100),
            )
            .await?;
            if let redis::Value::Array(values) = &value {
                preview.mark_more(size > values.len() as u64);
            }
            preview.push_resp_sequence(value);
            Some(size)
        }
        RedisValueType::ModuleOrUnknown => {
            preview.push_static_unsupported();
            None
        }
    };

    let final_type = key_type(connection, raw_key, request.timeout).await?;
    if final_type == "none" {
        return Err(DriverError::RedisKeyMissing);
    }
    if final_type != initial_type {
        return Err(DriverError::RedisKeyTypeChanged);
    }
    let notices = preview.notices();
    Ok(RedisValuePreview {
        identity: request.identity.clone(),
        key: RedisKeyEntry::new(request.key.clone()),
        value_type,
        ttl,
        size,
        retained_items: preview.items.len(),
        items: preview.items,
        retained_bytes: preview.retained_bytes,
        truncated: preview.truncated,
        stale: false,
        transient_allocation: TransientAllocationQualification::RedisWholeRespFrame,
        notices,
    })
}

async fn key_type(
    connection: &mut ConnectionManager,
    raw_key: &[u8],
    timeout: Duration,
) -> Result<String, DriverError> {
    let value: String = key_query(connection, timeout, redis::cmd("TYPE").arg(raw_key)).await?;
    Ok(value.to_ascii_lowercase())
}

async fn read_ttl(
    connection: &mut ConnectionManager,
    raw_key: &[u8],
    timeout: Duration,
) -> Result<RedisTtl, DriverError> {
    let pttl: i64 = key_query(connection, timeout, redis::cmd("PTTL").arg(raw_key)).await?;
    match pttl {
        -2 => Err(DriverError::RedisKeyMissing),
        -1 => Ok(RedisTtl::Persistent),
        value if value >= 0 => Ok(RedisTtl::ExpiresIn(value)),
        _ => Err(DriverError::RedisParse(
            "unexpected static PTTL state".to_owned(),
        )),
    }
}

const fn value_type(redis_type: &str) -> RedisValueType {
    match redis_type.as_bytes() {
        b"string" => RedisValueType::String,
        b"hash" => RedisValueType::Hash,
        b"list" => RedisValueType::List,
        b"set" => RedisValueType::Set,
        b"zset" => RedisValueType::SortedSet,
        b"stream" => RedisValueType::Stream,
        _ => RedisValueType::ModuleOrUnknown,
    }
}

async fn query<T: redis::FromRedisValue>(
    connection: &mut ConnectionManager,
    timeout: Duration,
    command: &mut redis::Cmd,
) -> Result<T, DriverError> {
    tokio::time::timeout(timeout, command.query_async(connection))
        .await
        .map_err(|_| DriverError::Timeout {
            driver: DriverKind::Redis,
            seconds: timeout.as_secs(),
        })?
        .map_err(DriverError::Redis)
}

async fn key_query<T: redis::FromRedisValue>(
    connection: &mut ConnectionManager,
    timeout: Duration,
    command: &mut redis::Cmd,
) -> Result<T, DriverError> {
    match query(connection, timeout, command).await {
        Err(DriverError::Redis(error)) if error.code() == Some("WRONGTYPE") => {
            Err(DriverError::RedisKeyTypeChanged)
        }
        result => result,
    }
}

fn bulk_bytes(value: redis::Value) -> Result<Vec<u8>, DriverError> {
    match value {
        redis::Value::BulkString(bytes) => Ok(bytes),
        redis::Value::SimpleString(value) => Ok(value.into_bytes()),
        redis::Value::Nil => Err(DriverError::RedisKeyMissing),
        _ => Err(DriverError::RedisKeyTypeChanged),
    }
}

struct PreviewRetention {
    items: Vec<Cell>,
    retained_bytes: usize,
    truncated: bool,
    depth_truncated: bool,
}

impl PreviewRetention {
    fn new() -> Self {
        Self {
            items: Vec::new(),
            retained_bytes: 0,
            truncated: false,
            depth_truncated: false,
        }
    }

    fn push_binary(&mut self, bytes: Vec<u8>) {
        let original_len = bytes.len();
        let cell = match std::str::from_utf8(&bytes) {
            Ok(value) => {
                let end = utf8_prefix_len(value, MAX_REDIS_CELL_BYTES);
                Cell::Text(value[..end].to_owned())
            }
            Err(_) => {
                // Base64 expands by 4/3. Restrict the raw slice so the public
                // preview itself, not merely its decoded form, stays bounded.
                let binary_limit = (MAX_REDIS_CELL_BYTES / 4) * 3;
                let retained = &bytes[..original_len.min(binary_limit)];
                Cell::Bytes {
                    preview: base64::engine::general_purpose::STANDARD.encode(retained),
                    len: original_len,
                }
            }
        };
        let cell_truncated = original_len
            > match &cell {
                Cell::Text(value) => value.len(),
                Cell::Bytes { preview, .. } => (preview.len() / 4) * 3,
                _ => 0,
            };
        self.push(cell, original_len.min(MAX_REDIS_CELL_BYTES), cell_truncated);
    }

    fn mark_more(&mut self, more: bool) {
        self.truncated |= more;
    }

    fn push_pair(
        &mut self,
        left_name: &'static str,
        left: Vec<u8>,
        right_name: &'static str,
        right: Vec<u8>,
    ) {
        let raw_len = left.len().saturating_add(right.len());
        let cell_truncated = raw_len > MAX_REDIS_CELL_BYTES;
        let allowance = (MAX_REDIS_CELL_BYTES.saturating_sub(512)) / 2;
        let mut fields = serde_json::Map::new();
        fields.insert(left_name.to_owned(), binary_json(&left, allowance));
        fields.insert(right_name.to_owned(), binary_json(&right, allowance));
        let cell = Cell::Json(fields.into());
        self.push(cell, raw_len.min(MAX_REDIS_CELL_BYTES), cell_truncated);
    }

    fn push_scored(&mut self, member: Vec<u8>, score: f64) {
        let raw_len = member.len().saturating_add(std::mem::size_of::<f64>());
        let cell_truncated = raw_len > MAX_REDIS_CELL_BYTES;
        let cell = Cell::Json(serde_json::json!({
            "member": binary_json(&member, MAX_REDIS_CELL_BYTES.saturating_sub(512)),
            "score": score.to_string(),
        }));
        self.push(cell, raw_len.min(MAX_REDIS_CELL_BYTES), cell_truncated);
    }

    fn push_resp_sequence(&mut self, value: redis::Value) {
        let values = match value {
            redis::Value::Array(values) | redis::Value::Set(values) => values,
            redis::Value::Nil => return,
            value => vec![value],
        };
        for value in values {
            let mut retained = 0_usize;
            let mut truncated = false;
            let json = bounded_resp_json(
                value,
                0,
                &mut retained,
                &mut truncated,
                &mut self.depth_truncated,
            );
            self.push(Cell::Json(json), retained, truncated);
        }
    }

    fn push_static_unsupported(&mut self) {
        self.push(Cell::Text("[unsupported-redis-type]".to_owned()), 24, false);
    }

    fn push(&mut self, mut cell: Cell, _source_retained_bytes: usize, mut cell_truncated: bool) {
        let mut materialized_bytes = cell_materialized_bytes(&cell);
        if materialized_bytes > MAX_REDIS_CELL_BYTES {
            cell = Cell::Text("[dbotter-cell-preview-truncated]".to_owned());
            materialized_bytes = cell_materialized_bytes(&cell);
            cell_truncated = true;
        }
        if self.items.len() >= MAX_REDIS_PREVIEW_ITEMS
            || self.retained_bytes.saturating_add(materialized_bytes) > MAX_REDIS_PREVIEW_BYTES
        {
            self.truncated = true;
            return;
        }
        self.retained_bytes = self.retained_bytes.saturating_add(materialized_bytes);
        self.truncated |= cell_truncated;
        self.items.push(cell);
    }

    fn notices(&self) -> Vec<ResultNotice> {
        let mut notices = vec![ResultNotice::RedisTransientFrameAllocation];
        if self.truncated {
            notices.push(ResultNotice::CellPreviewTruncated);
        }
        if self.depth_truncated {
            notices.push(ResultNotice::RedisDepthLimitReached);
        }
        notices
    }
}

fn binary_json(bytes: &[u8], allowance: usize) -> serde_json::Value {
    match std::str::from_utf8(bytes) {
        Ok(value) => {
            let text_end = utf8_prefix_len(value, allowance);
            let retained = &value[..text_end];
            serde_json::json!({
                    "text": retained,
                "original_len": bytes.len(),
                "truncated": retained.len() < bytes.len(),
            })
        }
        Err(_) => {
            let raw_allowance = (allowance / 4) * 3;
            let retained = &bytes[..bytes.len().min(raw_allowance)];
            serde_json::json!({
                "base64": base64::engine::general_purpose::STANDARD.encode(retained),
                "original_len": bytes.len(),
                "truncated": retained.len() < bytes.len(),
            })
        }
    }
}

fn bounded_resp_json(
    value: redis::Value,
    depth: usize,
    retained: &mut usize,
    truncated: &mut bool,
    depth_truncated: &mut bool,
) -> serde_json::Value {
    if depth >= MAX_REDIS_DEPTH || *retained >= MAX_REDIS_CELL_BYTES {
        *truncated = true;
        *depth_truncated |= depth >= MAX_REDIS_DEPTH;
        return serde_json::Value::String("[dbotter-truncated]".to_owned());
    }
    match value {
        redis::Value::Nil => serde_json::Value::Null,
        redis::Value::Int(value) => value.into(),
        redis::Value::BulkString(value) => {
            let allowance = MAX_REDIS_CELL_BYTES.saturating_sub(*retained);
            let json = binary_json(&value, allowance);
            *retained = retained.saturating_add(value.len().min(allowance));
            *truncated |= value.len() > allowance;
            json
        }
        redis::Value::SimpleString(value) => {
            let allowance = MAX_REDIS_CELL_BYTES.saturating_sub(*retained);
            let retained_value = &value[..utf8_prefix_len(&value, allowance)];
            *retained = retained.saturating_add(retained_value.len());
            *truncated |= retained_value.len() < value.len();
            retained_value.into()
        }
        redis::Value::Okay => "OK".into(),
        redis::Value::Double(value) => serde_json::json!(value),
        redis::Value::Boolean(value) => value.into(),
        redis::Value::Array(values) | redis::Value::Set(values) => {
            *truncated |= values.len() > MAX_REDIS_PREVIEW_ITEMS;
            values
                .into_iter()
                .take(MAX_REDIS_PREVIEW_ITEMS)
                .map(|value| {
                    bounded_resp_json(value, depth + 1, retained, truncated, depth_truncated)
                })
                .collect()
        }
        redis::Value::Map(entries) => {
            *truncated |= entries.len() > MAX_REDIS_PREVIEW_ITEMS;
            entries
                .into_iter()
                .take(MAX_REDIS_PREVIEW_ITEMS)
                .map(|(key, value)| {
                    serde_json::json!({
                        "key": bounded_resp_json(
                            key,
                            depth + 1,
                            retained,
                            truncated,
                            depth_truncated,
                        ),
                        "value": bounded_resp_json(
                            value,
                            depth + 1,
                            retained,
                            truncated,
                            depth_truncated,
                        ),
                    })
                })
                .collect()
        }
        redis::Value::VerbatimString { text, .. } => bounded_string_json(text, retained, truncated),
        redis::Value::BigNumber(value) => {
            bounded_string_json(value.to_string(), retained, truncated)
        }
        _ => "[unsupported-resp-value]".into(),
    }
}

fn bounded_string_json(
    value: String,
    retained: &mut usize,
    truncated: &mut bool,
) -> serde_json::Value {
    let allowance = MAX_REDIS_CELL_BYTES.saturating_sub(*retained);
    let end = utf8_prefix_len(&value, allowance);
    *retained = retained.saturating_add(end);
    *truncated |= end < value.len();
    value[..end].into()
}

fn utf8_prefix_len(value: &str, maximum: usize) -> usize {
    let mut end = value.len().min(maximum);
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    end
}

fn cell_materialized_bytes(cell: &Cell) -> usize {
    match cell {
        Cell::Text(value) | Cell::Decimal(value) => value.len(),
        Cell::Bytes { preview, .. } => preview.len(),
        Cell::Json(value) => {
            serde_json::to_vec(value).map_or(MAX_REDIS_CELL_BYTES + 1, |v| v.len())
        }
        Cell::Null
        | Cell::Bool(_)
        | Cell::Int(_)
        | Cell::UInt(_)
        | Cell::Float(_)
        | Cell::DateTime(_) => std::mem::size_of_val(cell),
    }
}

#[cfg(test)]
mod tests {
    use super::{PreviewRetention, cell_materialized_bytes};
    use crate::model::{MAX_REDIS_CELL_BYTES, MAX_REDIS_PREVIEW_BYTES};

    #[test]
    fn preview_total_counts_materialized_base64_and_never_exceeds_cell_or_total_caps() {
        let mut preview = PreviewRetention::new();
        for _ in 0..100 {
            preview.push_binary(vec![0xff; MAX_REDIS_CELL_BYTES]);
        }

        assert!(preview.truncated);
        assert!(preview.retained_bytes <= MAX_REDIS_PREVIEW_BYTES);
        assert!(
            preview
                .items
                .iter()
                .all(|cell| cell_materialized_bytes(cell) <= MAX_REDIS_CELL_BYTES)
        );
        assert_eq!(
            preview.retained_bytes,
            preview
                .items
                .iter()
                .map(cell_materialized_bytes)
                .sum::<usize>()
        );
    }
}
