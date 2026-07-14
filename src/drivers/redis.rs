use std::time::{Duration, Instant};

use redis::IntoConnectionInfo as _;
use secrecy::{ExposeSecret as _, SecretString};

use crate::drivers::DriverError;
use crate::model::{
    Cell, Column, ConnectionProfile, DriverAvailability, DriverCapabilities, DriverDescriptor,
    DriverKind, ExecuteRequest, QueryLanguage, QueryResult,
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
    planned_capabilities: DriverCapabilities::empty(),
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

    pub async fn execute(&self, request: &ExecuteRequest) -> Result<QueryResult, DriverError> {
        let parts = parse_command(&request.text)?;
        let command_name = &parts[0];
        if is_blocking_command(command_name) {
            return Err(DriverError::Unsupported {
                driver: DriverKind::Redis,
                operation: command_name.to_ascii_uppercase(),
            });
        }
        let mut command = ::redis::cmd(command_name);
        for argument in &parts[1..] {
            command.arg(argument);
        }
        let started = Instant::now();
        let mut connection = self.connection.clone();
        let value: ::redis::Value =
            tokio::time::timeout(request.timeout, command.query_async(&mut connection))
                .await
                .map_err(|_| DriverError::Timeout {
                    driver: DriverKind::Redis,
                    seconds: request.timeout.as_secs(),
                })??;
        let rows = value_rows(value);
        Ok(QueryResult {
            columns: vec![Column {
                name: "value".to_owned(),
                type_name: "RESP".to_owned(),
            }],
            rows,
            affected_rows: 0,
            last_insert_id: None,
            elapsed_ms: started.elapsed().as_millis(),
            truncated: false,
            notices: Vec::new(),
        })
    }
}

fn parse_command(text: &str) -> Result<Vec<String>, DriverError> {
    let parts =
        shell_words::split(text).map_err(|error| DriverError::RedisParse(error.to_string()))?;
    if parts.is_empty() {
        return Err(DriverError::RedisParse("command is empty".to_owned()));
    }
    Ok(parts)
}

fn is_blocking_command(command: &str) -> bool {
    matches!(
        command.to_ascii_uppercase().as_str(),
        "SUBSCRIBE" | "PSUBSCRIBE" | "SSUBSCRIBE" | "MONITOR"
    )
}

fn value_rows(value: ::redis::Value) -> Vec<Vec<Cell>> {
    match value {
        ::redis::Value::Array(values) => values
            .into_iter()
            .map(|value| vec![value_cell(value)])
            .collect(),
        value => vec![vec![value_cell(value)]],
    }
}

fn value_cell(value: ::redis::Value) -> Cell {
    match value {
        ::redis::Value::Nil => Cell::Null,
        ::redis::Value::Int(value) => Cell::Int(value),
        ::redis::Value::BulkString(value) => match String::from_utf8(value) {
            Ok(value) => Cell::Text(value),
            Err(error) => Cell::Bytes {
                preview: format!("{:02x?}", error.as_bytes().iter().take(32).collect::<Vec<_>>()),
                len: error.as_bytes().len(),
            },
        },
        ::redis::Value::SimpleString(value) => Cell::Text(value),
        ::redis::Value::Okay => Cell::Text("OK".to_owned()),
        ::redis::Value::Double(value) => Cell::Float(value),
        ::redis::Value::Boolean(value) => Cell::Bool(value),
        ::redis::Value::Array(values) | ::redis::Value::Set(values) => Cell::Json(
            serde_json::Value::Array(values.into_iter().map(value_json).collect()),
        ),
        ::redis::Value::Map(entries) => Cell::Json(serde_json::Value::Array(
            entries
                .into_iter()
                .map(|(key, value)| {
                    serde_json::json!({ "key": value_json(key), "value": value_json(value) })
                })
                .collect(),
        )),
        other => Cell::Text(format!("{other:?}")),
    }
}

fn value_json(value: ::redis::Value) -> serde_json::Value {
    match value_cell(value) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoted_redis_arguments_are_preserved() {
        assert_eq!(
            parse_command("SET greeting 'hello world'").expect("parse"),
            ["SET", "greeting", "hello world"]
        );
    }

    #[test]
    fn blocking_commands_are_rejected_case_insensitively() {
        for command in ["subscribe", "PSUBSCRIBE", "ssubscribe", "Monitor"] {
            assert!(is_blocking_command(command), "{command} must be denied");
        }
        assert!(!is_blocking_command("get"));
    }

    #[test]
    fn flat_array_becomes_one_row_per_value() {
        let rows = value_rows(::redis::Value::Array(vec![
            ::redis::Value::Int(1),
            ::redis::Value::BulkString(b"two".to_vec()),
        ]));
        assert_eq!(
            rows,
            vec![vec![Cell::Int(1)], vec![Cell::Text("two".into())]]
        );
    }

    #[test]
    fn nil_and_nested_resp_values_keep_their_shapes() {
        assert_eq!(value_rows(::redis::Value::Nil), vec![vec![Cell::Null]]);

        let rows = value_rows(::redis::Value::Array(vec![::redis::Value::Map(vec![(
            ::redis::Value::SimpleString("key".to_owned()),
            ::redis::Value::Array(vec![::redis::Value::Int(1), ::redis::Value::Nil]),
        )])]));
        assert_eq!(
            rows,
            vec![vec![Cell::Json(serde_json::json!([
                {"key": "key", "value": [1, null]}
            ]))]]
        );
    }
}
