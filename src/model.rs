use std::fmt;
use std::path::PathBuf;
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

macro_rules! numeric_id {
    ($name:ident) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub u64);
    };
}

numeric_id!(DraftId);
numeric_id!(ProfileGeneration);
numeric_id!(SessionGeneration);
numeric_id!(ResultId);
numeric_id!(OperationRecipeId);

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CredentialMode {
    None,
    Session,
    Environment,
}

/// UI-only intent. It must never become a wire payload.
///
/// ```compile_fail
/// fn requires_serialize<T: serde::Serialize>() {}
/// requires_serialize::<dbotter::model::SessionCredentialIntent>();
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionCredentialIntent {
    KeepCurrent,
    Replace,
    Forget,
}

impl SessionCredentialIntent {
    pub const KEEP_CONTROL_ID: &'static str = "profile.credential.session.keep";
    pub const REPLACE_CONTROL_ID: &'static str = "profile.credential.session.replace";
    pub const FORGET_CONTROL_ID: &'static str = "profile.credential.session.forget";
}

#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RedisTlsConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca_file: Option<PathBuf>,
}

impl fmt::Debug for RedisTlsConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RedisTlsConfig")
            .field("ca_file", &self.ca_file.as_ref().map(|_| "<configured>"))
            .finish()
    }
}

fn default_host() -> String {
    "127.0.0.1".to_owned()
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    pub credential_mode: CredentialMode,
    #[serde(default)]
    pub secret_env: Option<String>,
    #[serde(default, skip_serializing_if = "RedisTlsConfig::is_empty")]
    pub redis_tls: RedisTlsConfig,
}

impl fmt::Debug for ConnectionProfile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ConnectionProfile")
            .field("id", &self.id)
            .field("name", &"<redacted>")
            .field("driver", &self.driver)
            .field("host", &"<redacted>")
            .field("port", &self.port)
            .field("database", &self.database.as_ref().map(|_| "<configured>"))
            .field("username", &self.username.as_ref().map(|_| "<configured>"))
            .field("tls", &self.tls)
            .field("credential_mode", &self.credential_mode)
            .field(
                "secret_env",
                &self.secret_env.as_ref().map(|_| "<configured>"),
            )
            .field("redis_tls", &self.redis_tls)
            .finish()
    }
}

impl ConnectionProfile {
    pub fn redacted_endpoint(&self) -> String {
        format!("{}://{}:{}", self.driver, self.host, self.port)
    }

    pub fn from_draft(id: String, draft: ConnectionDraft) -> Self {
        Self {
            id,
            name: draft.name,
            driver: draft.driver,
            host: draft.host,
            port: draft.port,
            database: draft.database,
            username: draft.username,
            tls: draft.tls,
            credential_mode: draft.credential_mode,
            secret_env: draft.secret_env,
            redis_tls: draft.redis_tls,
        }
    }

    pub fn as_draft(&self) -> ConnectionDraft {
        ConnectionDraft {
            name: self.name.clone(),
            driver: self.driver,
            host: self.host.clone(),
            port: self.port,
            database: self.database.clone(),
            username: self.username.clone(),
            tls: self.tls,
            credential_mode: self.credential_mode,
            secret_env: self.secret_env.clone(),
            redis_tls: self.redis_tls.clone(),
        }
    }
}

impl RedisTlsConfig {
    pub fn is_empty(&self) -> bool {
        self.ca_file.is_none()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct ConnectionDraft {
    pub name: String,
    pub driver: DriverKind,
    pub host: String,
    pub port: u16,
    pub database: Option<String>,
    pub username: Option<String>,
    pub tls: TlsMode,
    pub credential_mode: CredentialMode,
    pub secret_env: Option<String>,
    pub redis_tls: RedisTlsConfig,
}

impl fmt::Debug for ConnectionDraft {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ConnectionDraft")
            .field("name", &"<redacted>")
            .field("driver", &self.driver)
            .field("host", &"<redacted>")
            .field("port", &self.port)
            .field("database", &self.database.as_ref().map(|_| "<configured>"))
            .field("username", &self.username.as_ref().map(|_| "<configured>"))
            .field("tls", &self.tls)
            .field("credential_mode", &self.credential_mode)
            .field(
                "secret_env",
                &self.secret_env.as_ref().map(|_| "<configured>"),
            )
            .field("redis_tls", &self.redis_tls)
            .finish()
    }
}

impl ConnectionDraft {
    pub fn for_driver(driver: DriverKind) -> Self {
        let (port, tls) = match driver {
            DriverKind::MySql => (3306, TlsMode::Preferred),
            DriverKind::Redis => (6379, TlsMode::Disabled),
            DriverKind::MongoDb => (27017, TlsMode::Preferred),
        };
        Self {
            name: String::new(),
            driver,
            host: default_host(),
            port,
            database: None,
            username: None,
            tls,
            credential_mode: CredentialMode::None,
            secret_env: None,
            redis_tls: RedisTlsConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileFieldId {
    ConnectionId,
    DisplayName,
    Host,
    Port,
    Database,
    Username,
    CredentialMode,
    CredentialEnvironmentName,
    SessionCredential,
    RedisTlsMode,
    RedisCaFile,
}

impl ProfileFieldId {
    pub const fn focus_id(self) -> &'static str {
        match self {
            Self::ConnectionId => "profile.connection_id",
            Self::DisplayName => "profile.display_name",
            Self::Host => "profile.host",
            Self::Port => "profile.port",
            Self::Database => "profile.database",
            Self::Username => "profile.username",
            Self::CredentialMode => "profile.credential.mode",
            Self::CredentialEnvironmentName => "profile.credential.environment_name",
            Self::SessionCredential => "profile.credential.session.value",
            Self::RedisTlsMode => "profile.redis_tls.mode",
            Self::RedisCaFile => "profile.redis_tls.ca_file",
        }
    }
}

macro_rules! define_operation_kinds {
    ($($variant:ident = $index:expr),+ $(,)?) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
        #[serde(rename_all = "snake_case")]
        pub enum OperationKind {
            $($variant),+
        }

        impl OperationKind {
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];
            pub const COUNT: usize = [$(stringify!($variant)),+].len();

            pub const fn exhaustive_index(self) -> usize {
                match self {
                    $(Self::$variant => $index),+
                }
            }
        }
    };
}

define_operation_kinds!(
    LoadConfiguration = 0,
    ReloadConfiguration = 1,
    MigrateConfiguration = 2,
    CreateProfile = 3,
    UpdateProfile = 4,
    DeleteProfile = 5,
    TestDraftConnection = 6,
    ConnectProfile = 7,
    DisconnectProfile = 8,
    ReconnectProfile = 9,
    ExecuteRead = 10,
    ExecuteMutation = 11,
    BrowseMySql = 12,
    BrowseRedis = 13,
    InspectRedis = 14,
    ExportResult = 15,
    ShutdownRuntime = 16,
);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicSummary {
    InvalidInput,
    CredentialRequired,
    AuthenticationFailed,
    PermissionDenied,
    NetworkUnavailable,
    TlsVerificationFailed,
    OperationTimedOut,
    SyntaxRejected,
    ConstraintRejected,
    UnsupportedFeature,
    OperationCancelled,
    ResourceBusy,
    ResourceStale,
    ConfigWriteNotCommitted,
    CommittedDurabilityUnknown,
    ExportFailed,
    InternalFailure,
}

impl PublicSummary {
    pub const ALL: &'static [Self] = &[
        Self::InvalidInput,
        Self::CredentialRequired,
        Self::AuthenticationFailed,
        Self::PermissionDenied,
        Self::NetworkUnavailable,
        Self::TlsVerificationFailed,
        Self::OperationTimedOut,
        Self::SyntaxRejected,
        Self::ConstraintRejected,
        Self::UnsupportedFeature,
        Self::OperationCancelled,
        Self::ResourceBusy,
        Self::ResourceStale,
        Self::ConfigWriteNotCommitted,
        Self::CommittedDurabilityUnknown,
        Self::ExportFailed,
        Self::InternalFailure,
    ];

    pub const fn message(self) -> &'static str {
        match self {
            Self::InvalidInput => "Some input needs attention.",
            Self::CredentialRequired => "A credential is required.",
            Self::AuthenticationFailed => "Authentication failed.",
            Self::PermissionDenied => "The server denied this operation.",
            Self::NetworkUnavailable => "The server is unavailable.",
            Self::TlsVerificationFailed => "TLS verification failed.",
            Self::OperationTimedOut => "The operation timed out.",
            Self::SyntaxRejected => "The server rejected the syntax.",
            Self::ConstraintRejected => "The server rejected the change.",
            Self::UnsupportedFeature => "This operation is not supported.",
            Self::OperationCancelled => "The operation was cancelled.",
            Self::ResourceBusy => "Another operation is already active.",
            Self::ResourceStale => "The selected state is stale.",
            Self::ConfigWriteNotCommitted => "The configuration was not changed.",
            Self::CommittedDurabilityUnknown => {
                "The change is visible, but durable storage confirmation failed."
            }
            Self::ExportFailed => "The result could not be exported.",
            Self::InternalFailure => "Dbotter could not complete the operation.",
        }
    }
}

impl fmt::Display for PublicSummary {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PublicCodeValidationError {
    #[error("MySQL errno must be non-zero")]
    InvalidMySqlErrno,
    #[error("SQLSTATE must contain exactly five uppercase ASCII letters or digits")]
    InvalidSqlState,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct MySqlPublicErrorCode {
    errno: u16,
    sql_state: [u8; 5],
}

impl MySqlPublicErrorCode {
    pub fn new(errno: u16, sql_state: &str) -> Result<Self, PublicCodeValidationError> {
        if errno == 0 {
            return Err(PublicCodeValidationError::InvalidMySqlErrno);
        }
        let bytes = sql_state.as_bytes();
        if bytes.len() != 5
            || !bytes
                .iter()
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
        {
            return Err(PublicCodeValidationError::InvalidSqlState);
        }
        let mut validated = [0_u8; 5];
        validated.copy_from_slice(bytes);
        Ok(Self {
            errno,
            sql_state: validated,
        })
    }

    pub const fn errno(self) -> u16 {
        self.errno
    }

    pub fn sql_state(&self) -> &str {
        std::str::from_utf8(&self.sql_state).unwrap_or("")
    }
}

impl fmt::Debug for MySqlPublicErrorCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MySqlPublicErrorCode")
            .field("errno", &self.errno)
            .field("sql_state", &self.sql_state())
            .finish()
    }
}

impl Serialize for MySqlPublicErrorCode {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct as _;

        let mut state = serializer.serialize_struct("MySqlPublicErrorCode", 2)?;
        state.serialize_field("errno", &self.errno)?;
        state.serialize_field("sql_state", self.sql_state())?;
        state.end()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RedisPublicErrorKind {
    Parse,
    AuthenticationFailed,
    UnexpectedReturnType,
    InvalidClientConfig,
    Io,
    Client,
    Extension,
    MasterNameNotFoundBySentinel,
    NoValidReplicasFoundBySentinel,
    EmptySentinelList,
    ClusterConnectionNotFound,
    Resp3NotSupported,
    ResponseError,
    ExecAbort,
    BusyLoading,
    NoScript,
    Moved,
    Ask,
    TryAgain,
    ClusterDown,
    CrossSlot,
    MasterDown,
    ReadOnly,
    NotBusy,
    NoSub,
    NoPerm,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("redis error kind is not in the public allowlist")]
pub struct UnsupportedRedisErrorKind;

impl TryFrom<redis::ErrorKind> for RedisPublicErrorKind {
    type Error = UnsupportedRedisErrorKind;

    fn try_from(kind: redis::ErrorKind) -> Result<Self, Self::Error> {
        match kind {
            redis::ErrorKind::Parse => Ok(Self::Parse),
            redis::ErrorKind::AuthenticationFailed => Ok(Self::AuthenticationFailed),
            redis::ErrorKind::UnexpectedReturnType => Ok(Self::UnexpectedReturnType),
            redis::ErrorKind::InvalidClientConfig => Ok(Self::InvalidClientConfig),
            redis::ErrorKind::Io => Ok(Self::Io),
            redis::ErrorKind::Client => Ok(Self::Client),
            redis::ErrorKind::Extension => Ok(Self::Extension),
            redis::ErrorKind::MasterNameNotFoundBySentinel => {
                Ok(Self::MasterNameNotFoundBySentinel)
            }
            redis::ErrorKind::NoValidReplicasFoundBySentinel => {
                Ok(Self::NoValidReplicasFoundBySentinel)
            }
            redis::ErrorKind::EmptySentinelList => Ok(Self::EmptySentinelList),
            redis::ErrorKind::ClusterConnectionNotFound => Ok(Self::ClusterConnectionNotFound),
            redis::ErrorKind::RESP3NotSupported => Ok(Self::Resp3NotSupported),
            redis::ErrorKind::Server(server) => match server {
                redis::ServerErrorKind::ResponseError => Ok(Self::ResponseError),
                redis::ServerErrorKind::ExecAbort => Ok(Self::ExecAbort),
                redis::ServerErrorKind::BusyLoading => Ok(Self::BusyLoading),
                redis::ServerErrorKind::NoScript => Ok(Self::NoScript),
                redis::ServerErrorKind::Moved => Ok(Self::Moved),
                redis::ServerErrorKind::Ask => Ok(Self::Ask),
                redis::ServerErrorKind::TryAgain => Ok(Self::TryAgain),
                redis::ServerErrorKind::ClusterDown => Ok(Self::ClusterDown),
                redis::ServerErrorKind::CrossSlot => Ok(Self::CrossSlot),
                redis::ServerErrorKind::MasterDown => Ok(Self::MasterDown),
                redis::ServerErrorKind::ReadOnly => Ok(Self::ReadOnly),
                redis::ServerErrorKind::NotBusy => Ok(Self::NotBusy),
                redis::ServerErrorKind::NoSub => Ok(Self::NoSub),
                redis::ServerErrorKind::NoPerm => Ok(Self::NoPerm),
                _ => Err(UnsupportedRedisErrorKind),
            },
            _ => Err(UnsupportedRedisErrorKind),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "type", content = "value", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PublicCode {
    None,
    Field(ProfileFieldId),
    ProfileIdConflict,
    ProfileStale,
    ConfigExternalChange,
    MigrationBackupAvailable,
    SessionCredential,
    CredentialEnvironmentName,
    Username,
    Database,
    StatementTarget,
    AmbiguousSqlMode,
    UnterminatedSqlToken,
    RowLimit,
    TimeoutInput,
    Catalog,
    RedisScan,
    ExportDestination,
    ExportDestinationCommitted,
    RedisTlsCaInvalidPem,
    RedisTlsCaUntrustedIssuer,
    TlsHostnameMismatch,
    RedisTlsPreferredLegacy,
    PreparedStatementUnsupported,
    MySql(MySqlPublicErrorCode),
    Redis(RedisPublicErrorKind),
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

/// A sensitive request: user text is intentionally not serializable.
///
/// ```compile_fail
/// fn requires_serialize<T: serde::Serialize>() {}
/// requires_serialize::<dbotter::model::ExecuteRequest>();
/// ```
#[derive(Clone)]
pub struct ExecuteRequest {
    pub operation_id: OperationId,
    pub profile_id: ProfileId,
    pub language: QueryLanguage,
    pub text: String,
    pub row_limit: u32,
    pub timeout: Duration,
}

impl fmt::Debug for ExecuteRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExecuteRequest")
            .field("operation_id", &self.operation_id)
            .field("profile_id", &self.profile_id)
            .field("language", &self.language)
            .field("text", &"<redacted>")
            .field("row_limit", &self.row_limit)
            .field("timeout", &self.timeout)
            .finish()
    }
}

#[derive(Clone, PartialEq, Serialize)]
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

impl fmt::Debug for Cell {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Null => "Cell::Null",
            Self::Bool(_) => "Cell::Bool(<redacted>)",
            Self::Int(_) => "Cell::Int(<redacted>)",
            Self::UInt(_) => "Cell::UInt(<redacted>)",
            Self::Float(_) => "Cell::Float(<redacted>)",
            Self::Decimal(_) => "Cell::Decimal(<redacted>)",
            Self::Text(_) => "Cell::Text(<redacted>)",
            Self::Bytes { .. } => "Cell::Bytes(<redacted>)",
            Self::Json(_) => "Cell::Json(<redacted>)",
            Self::DateTime(_) => "Cell::DateTime(<redacted>)",
        })
    }
}

#[derive(Clone, PartialEq, Eq, Serialize)]
pub struct Column {
    pub name: String,
    pub type_name: String,
}

impl fmt::Debug for Column {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Column(<redacted>)")
    }
}

#[derive(Clone, Serialize)]
pub struct QueryResult {
    pub columns: Vec<Column>,
    pub rows: Vec<Vec<Cell>>,
    pub affected_rows: u64,
    pub last_insert_id: Option<u64>,
    pub elapsed_ms: u128,
    pub truncated: bool,
    pub notices: Vec<String>,
}

impl fmt::Debug for QueryResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("QueryResult")
            .field("column_count", &self.columns.len())
            .field("row_count", &self.rows.len())
            .field("affected_rows", &self.affected_rows)
            .field("has_last_insert_id", &self.last_insert_id.is_some())
            .field("elapsed_ms", &self.elapsed_ms)
            .field("truncated", &self.truncated)
            .field("notice_count", &self.notices.len())
            .finish()
    }
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
    pub profile_id: ProfileId,
    pub driver: DriverKind,
    pub column_count: usize,
    pub row_count: usize,
    pub affected_rows: u64,
    pub elapsed_ms: u128,
    pub truncated: bool,
    pub notice_count: usize,
}

impl ExecReceipt {
    pub fn from_result(
        status: &'static str,
        operation_id: OperationId,
        profile_id: ProfileId,
        driver: DriverKind,
        result: &QueryResult,
    ) -> Self {
        Self {
            status,
            operation_id,
            profile_id,
            driver,
            column_count: result.columns.len(),
            row_count: result.rows.len(),
            affected_rows: result.affected_rows,
            elapsed_ms: result.elapsed_ms,
            truncated: result.truncated,
            notice_count: result.notices.len(),
        }
    }
}

#[derive(Serialize)]
pub struct ExecOutput {
    pub receipt: ExecReceipt,
    pub result: QueryResult,
}

impl fmt::Debug for ExecOutput {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExecOutput")
            .field("receipt", &self.receipt)
            .field("result", &"<redacted>")
            .finish()
    }
}
