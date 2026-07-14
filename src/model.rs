use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Serialize};

bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
    pub struct DriverCapabilities: u16 {
        const CONNECT = 1 << 0;
        const PING = 1 << 1;
        const SQL = 1 << 2;
        const COMMAND = 1 << 3;
        const DOCUMENT = 1 << 4;
        const CATALOG = 1 << 5;
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProfileId(pub String);

impl ProfileId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct OperationId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DriverKind {
    #[serde(rename = "mysql")]
    MySql,
    #[serde(rename = "redis")]
    Redis,
    #[serde(rename = "mongodb")]
    MongoDb,
}

impl fmt::Display for DriverKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::MySql => "mysql",
            Self::Redis => "redis",
            Self::MongoDb => "mongodb",
        })
    }
}

impl DriverKind {
    pub const fn language(self) -> QueryLanguage {
        match self {
            Self::MySql => QueryLanguage::Sql,
            Self::Redis => QueryLanguage::RedisCommand,
            Self::MongoDb => QueryLanguage::MongoDocument,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TlsMode {
    Disabled,
    #[default]
    Preferred,
    Required,
}

fn default_host() -> String {
    "127.0.0.1".to_owned()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionProfile {
    pub id: String,
    pub name: String,
    pub driver: DriverKind,
    #[serde(default = "default_host")]
    pub host: String,
    pub port: u16,
    #[serde(default)]
    pub database: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub tls: TlsMode,
    #[serde(default)]
    pub secret_env: Option<String>,
}

impl ConnectionProfile {
    pub fn redacted_endpoint(&self) -> String {
        format!("{}://{}:{}", self.driver, self.host, self.port)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DriverAvailability {
    Ready,
    Planned,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum QueryLanguage {
    Sql,
    RedisCommand,
    MongoDocument,
}

#[derive(Debug, Clone, Serialize)]
pub struct DriverDescriptor {
    pub kind: DriverKind,
    pub display_name: &'static str,
    pub default_port: u16,
    pub availability: DriverAvailability,
    pub languages: &'static [QueryLanguage],
    pub capabilities: DriverCapabilities,
    pub planned_capabilities: DriverCapabilities,
    pub reason: Option<&'static str>,
}

#[derive(Debug, Clone)]
pub struct ExecuteRequest {
    pub operation_id: OperationId,
    pub profile_id: ProfileId,
    pub language: QueryLanguage,
    pub text: String,
    pub row_limit: u32,
    pub timeout: Duration,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum Cell {
    Null,
    Bool(bool),
    Int(i64),
    UInt(u64),
    Float(f64),
    Decimal(String),
    Text(String),
    Bytes { preview: String, len: usize },
    Json(serde_json::Value),
    DateTime(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Column {
    pub name: String,
    pub type_name: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct QueryResult {
    pub columns: Vec<Column>,
    pub rows: Vec<Vec<Cell>>,
    pub affected_rows: u64,
    pub last_insert_id: Option<u64>,
    pub elapsed_ms: u128,
    pub truncated: bool,
    pub notices: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct CheckReceipt {
    pub status: &'static str,
    pub operation_id: OperationId,
    pub profile_id: String,
    pub driver: DriverKind,
    pub endpoint: String,
    pub elapsed_ms: u128,
}

#[derive(Debug, Serialize)]
pub struct ExecReceipt {
    pub status: &'static str,
    pub operation_id: OperationId,
    pub profile_id: String,
    pub driver: DriverKind,
    pub endpoint: String,
    pub result: QueryResult,
}
