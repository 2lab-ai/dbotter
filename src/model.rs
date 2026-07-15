use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
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
        const KEYSPACE_BROWSE = 1 << 6;
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

pub const DEFAULT_EXECUTE_ROWS: u32 = 500;
pub const MAX_RESULT_ROWS: usize = 10_000;
pub const DEFAULT_EXECUTE_TIMEOUT: Duration = Duration::from_secs(30);
pub const MAX_EXECUTE_TIMEOUT: Duration = Duration::from_secs(300);
pub const MAX_RESULT_COLUMNS: usize = 1_024;
pub const MAX_RESULT_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_RESULT_CELL_BYTES: usize = 1024 * 1024;
pub const MAX_RESULT_NOTICES: usize = 32;
pub const MAX_RESULT_NOTICE_BYTES: usize = 512;

pub const DEFAULT_CATALOG_PAGE_SIZE: u16 = 50;
pub const MAX_CATALOG_PAGE_SIZE: u16 = 200;
pub const DEFAULT_CATALOG_TIMEOUT: Duration = Duration::from_secs(5);
pub const MAX_CATALOG_TIMEOUT: Duration = Duration::from_secs(30);
pub const MAX_CATALOG_SCHEMAS: usize = 200;
pub const MAX_CATALOG_RELATIONS: usize = 2_000;
pub const MAX_CATALOG_COLUMNS: usize = 10_000;
pub const MAX_CATALOG_COLUMNS_PER_RELATION: usize = 512;
pub const MAX_CATALOG_UTF8_BYTES: usize = 4 * 1024 * 1024;

pub const DEFAULT_REDIS_SCAN_COUNT: u32 = 100;
pub const MAX_REDIS_SCAN_COUNT: u32 = 1_000;
pub const MAX_REDIS_FILTER_BYTES: usize = 512;
pub const MAX_REDIS_COMMAND_BYTES: usize = 65_536;
pub const MAX_REDIS_COMMAND_TOKENS: usize = 1_024;
pub const MAX_REDIS_COMMAND_TOKEN_BYTES: usize = 16 * 1024;
pub const MAX_REDIS_KEYS: usize = 10_000;
pub const MAX_REDIS_KEY_BYTES: usize = 4 * 1024;
pub const MAX_REDIS_RETAINED_KEY_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_REDIS_PREVIEW_ITEMS: usize = 100;
pub const MAX_REDIS_PREVIEW_BYTES: usize = 1024 * 1024;
pub const MAX_REDIS_CELLS: usize = 10_000;
pub const MAX_REDIS_CELL_BYTES: usize = 64 * 1024;
pub const MAX_REDIS_DEPTH: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RequestValidationError {
    #[error("statement target is empty")]
    EmptyStatement,
    #[error("row limit must be between 1 and 10,000")]
    InvalidRowLimit,
    #[error("execute timeout must be between 1 and 300 seconds")]
    InvalidExecuteTimeout,
    #[error("Redis command must contain at least one token")]
    EmptyRedisCommand,
    #[error("Redis command exceeds 65,536 bytes")]
    RedisCommandTooLarge,
    #[error("Redis command exceeds 1,024 tokens")]
    TooManyRedisTokens,
    #[error("a Redis command token exceeds 16 KiB")]
    RedisTokenTooLarge,
    #[error("the Redis command is denied by the local nonblocking policy")]
    RedisCommandDenied,
    #[error("catalog page size must be between 1 and 200")]
    InvalidCatalogPageSize,
    #[error("catalog timeout must be between 1 and 30 seconds")]
    InvalidCatalogTimeout,
    #[error("catalog parent identity is empty")]
    EmptyCatalogParent,
    #[error("catalog page token is empty")]
    EmptyCatalogPageToken,
    #[error("Redis filter exceeds 512 UTF-8 bytes")]
    RedisFilterTooLarge,
    #[error("Redis SCAN COUNT hint must be between 1 and 1,000")]
    InvalidRedisScanCount,
    #[error("Redis key identity exceeds 4 KiB")]
    RedisKeyTooLarge,
}

/// Correlation for one saved-profile request. The identifier is deliberately
/// redacted from `Debug`; this type is serializable only because response/page
/// DTOs need to return correlation metadata, never user input.
#[derive(Clone, PartialEq, Eq, Hash, Serialize)]
pub struct RequestIdentity {
    pub profile_id: ProfileId,
    pub profile_generation: ProfileGeneration,
    pub operation_id: OperationId,
}

impl fmt::Debug for RequestIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RequestIdentity")
            .field("profile_id", &"<redacted>")
            .field("profile_generation", &self.profile_generation)
            .field("operation_id", &self.operation_id)
            .finish()
    }
}

impl RequestIdentity {
    pub fn new(
        profile_id: ProfileId,
        profile_generation: ProfileGeneration,
        operation_id: OperationId,
    ) -> Self {
        Self {
            profile_id,
            profile_generation,
            operation_id,
        }
    }
}

/// The only driver-level request that may contain user-provided MySQL text.
/// It is intentionally not serializable.
///
/// ```compile_fail
/// fn requires_serialize<T: serde::Serialize>() {}
/// requires_serialize::<dbotter::model::PreparedMySqlRequest>();
/// ```
#[derive(Clone, PartialEq, Eq)]
pub struct PreparedMySqlRequest {
    pub identity: RequestIdentity,
    pub statement: String,
    pub row_limit: u32,
    pub timeout: Duration,
}

impl fmt::Debug for PreparedMySqlRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedMySqlRequest")
            .field("identity", &self.identity)
            .field("statement", &"<redacted>")
            .field("row_limit", &self.row_limit)
            .field("timeout", &self.timeout)
            .finish()
    }
}

impl PreparedMySqlRequest {
    pub fn identity(&self) -> &RequestIdentity {
        &self.identity
    }

    pub fn profile_id(&self) -> &ProfileId {
        &self.identity.profile_id
    }

    pub const fn profile_generation(&self) -> ProfileGeneration {
        self.identity.profile_generation
    }

    pub const fn operation_id(&self) -> OperationId {
        self.identity.operation_id
    }

    pub fn validate(&self) -> Result<(), RequestValidationError> {
        if self.statement.trim().is_empty() {
            return Err(RequestValidationError::EmptyStatement);
        }
        validate_execute_limits(self.row_limit, self.timeout)
    }
}

/// A shell-parsed Redis command. Argument bytes are user data and never cross
/// a serialization or `Debug` boundary.
///
/// ```compile_fail
/// fn requires_serialize<T: serde::Serialize>() {}
/// requires_serialize::<dbotter::model::RedisExecuteRequest>();
/// ```
#[derive(Clone, PartialEq, Eq)]
pub struct RedisExecuteRequest {
    identity: RequestIdentity,
    argv: Vec<Vec<u8>>,
    row_limit: u32,
    timeout: Duration,
}

impl fmt::Debug for RedisExecuteRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RedisExecuteRequest")
            .field("identity", &self.identity)
            .field("argv", &"<redacted>")
            .field("argument_count", &self.argv.len())
            .field("row_limit", &self.row_limit)
            .field("timeout", &self.timeout)
            .finish()
    }
}

impl RedisExecuteRequest {
    pub fn new(
        identity: RequestIdentity,
        argv: Vec<Vec<u8>>,
        row_limit: u32,
        timeout: Duration,
    ) -> Result<Self, RequestValidationError> {
        let request = Self {
            identity,
            argv,
            row_limit,
            timeout,
        };
        request.validate()?;
        Ok(request)
    }

    pub fn identity(&self) -> &RequestIdentity {
        &self.identity
    }

    pub fn profile_id(&self) -> &ProfileId {
        &self.identity.profile_id
    }

    pub const fn profile_generation(&self) -> ProfileGeneration {
        self.identity.profile_generation
    }

    pub const fn operation_id(&self) -> OperationId {
        self.identity.operation_id
    }

    pub fn argv(&self) -> &[Vec<u8>] {
        &self.argv
    }

    pub const fn row_limit(&self) -> u32 {
        self.row_limit
    }

    pub const fn timeout(&self) -> Duration {
        self.timeout
    }

    pub fn validate(&self) -> Result<(), RequestValidationError> {
        validate_execute_limits(self.row_limit, self.timeout)?;
        if self.argv.is_empty() || self.argv[0].is_empty() {
            return Err(RequestValidationError::EmptyRedisCommand);
        }
        if self.argv.len() > MAX_REDIS_COMMAND_TOKENS {
            return Err(RequestValidationError::TooManyRedisTokens);
        }
        if self
            .argv
            .iter()
            .any(|argument| argument.len() > MAX_REDIS_COMMAND_TOKEN_BYTES)
        {
            return Err(RequestValidationError::RedisTokenTooLarge);
        }
        let command_bytes = self.argv.iter().try_fold(0_usize, |total, argument| {
            total
                .checked_add(argument.len())
                .and_then(|value| value.checked_add(1))
        });
        if command_bytes.is_none_or(|bytes| bytes.saturating_sub(1) > MAX_REDIS_COMMAND_BYTES) {
            return Err(RequestValidationError::RedisCommandTooLarge);
        }
        if crate::execution::redis_argv_is_denied(&self.argv) {
            return Err(RequestValidationError::RedisCommandDenied);
        }
        Ok(())
    }
}

fn validate_execute_limits(
    row_limit: u32,
    timeout: Duration,
) -> Result<(), RequestValidationError> {
    if row_limit == 0 || row_limit as usize > MAX_RESULT_ROWS {
        return Err(RequestValidationError::InvalidRowLimit);
    }
    if timeout < Duration::from_secs(1) || timeout > MAX_EXECUTE_TIMEOUT {
        return Err(RequestValidationError::InvalidExecuteTimeout);
    }
    Ok(())
}

#[derive(Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct CatalogPageToken(pub String);

impl fmt::Debug for CatalogPageToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CatalogPageToken(<redacted>)")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogLevel {
    Schemas,
    Relations,
    Columns,
}

#[derive(Clone, PartialEq, Eq)]
pub enum CatalogRequest {
    Schemas {
        identity: RequestIdentity,
        prefix: Option<String>,
        page_token: Option<CatalogPageToken>,
        page_size: u16,
        timeout: Duration,
    },
    Relations {
        identity: RequestIdentity,
        schema: String,
        prefix: Option<String>,
        page_token: Option<CatalogPageToken>,
        page_size: u16,
        timeout: Duration,
    },
    Columns {
        identity: RequestIdentity,
        schema: String,
        relation: String,
        prefix: Option<String>,
        page_token: Option<CatalogPageToken>,
        page_size: u16,
        timeout: Duration,
    },
}

impl fmt::Debug for CatalogRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug = formatter.debug_struct("CatalogRequest");
        debug
            .field("level", &self.level())
            .field("identity", self.identity())
            .field("parent", &"<redacted>")
            .field("prefix", &self.prefix().map(|_| "<configured>"))
            .field("page_token", &self.page_token().map(|_| "<configured>"))
            .field("page_size", &self.page_size())
            .field("timeout", &self.timeout())
            .finish()
    }
}

impl CatalogRequest {
    pub const fn level(&self) -> CatalogLevel {
        match self {
            Self::Schemas { .. } => CatalogLevel::Schemas,
            Self::Relations { .. } => CatalogLevel::Relations,
            Self::Columns { .. } => CatalogLevel::Columns,
        }
    }

    pub const fn identity(&self) -> &RequestIdentity {
        match self {
            Self::Schemas { identity, .. }
            | Self::Relations { identity, .. }
            | Self::Columns { identity, .. } => identity,
        }
    }

    pub fn profile_id(&self) -> &ProfileId {
        &self.identity().profile_id
    }

    pub const fn profile_generation(&self) -> ProfileGeneration {
        self.identity().profile_generation
    }

    pub const fn operation_id(&self) -> OperationId {
        self.identity().operation_id
    }

    pub fn prefix(&self) -> Option<&str> {
        match self {
            Self::Schemas { prefix, .. }
            | Self::Relations { prefix, .. }
            | Self::Columns { prefix, .. } => prefix.as_deref(),
        }
    }

    pub const fn page_token(&self) -> Option<&CatalogPageToken> {
        match self {
            Self::Schemas { page_token, .. }
            | Self::Relations { page_token, .. }
            | Self::Columns { page_token, .. } => page_token.as_ref(),
        }
    }

    pub const fn page_size(&self) -> u16 {
        match self {
            Self::Schemas { page_size, .. }
            | Self::Relations { page_size, .. }
            | Self::Columns { page_size, .. } => *page_size,
        }
    }

    pub const fn timeout(&self) -> Duration {
        match self {
            Self::Schemas { timeout, .. }
            | Self::Relations { timeout, .. }
            | Self::Columns { timeout, .. } => *timeout,
        }
    }

    pub fn validate(&self) -> Result<(), RequestValidationError> {
        if self.page_size() == 0 || self.page_size() > MAX_CATALOG_PAGE_SIZE {
            return Err(RequestValidationError::InvalidCatalogPageSize);
        }
        if self.timeout() < Duration::from_secs(1) || self.timeout() > MAX_CATALOG_TIMEOUT {
            return Err(RequestValidationError::InvalidCatalogTimeout);
        }
        let parents_valid = match self {
            Self::Schemas { .. } => true,
            Self::Relations { schema, .. } => !schema.is_empty(),
            Self::Columns {
                schema, relation, ..
            } => !schema.is_empty() && !relation.is_empty(),
        };
        if !parents_valid {
            return Err(RequestValidationError::EmptyCatalogParent);
        }
        if self.page_token().is_some_and(|token| token.0.is_empty()) {
            return Err(RequestValidationError::EmptyCatalogPageToken);
        }
        Ok(())
    }
}

#[derive(Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(tag = "level", rename_all = "snake_case")]
pub enum CatalogNodeIdentity {
    Schema {
        schema: String,
    },
    Relation {
        schema: String,
        relation: String,
    },
    Column {
        schema: String,
        relation: String,
        ordinal: u32,
    },
}

impl fmt::Debug for CatalogNodeIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Schema { .. } => "CatalogNodeIdentity::Schema(<redacted>)",
            Self::Relation { .. } => "CatalogNodeIdentity::Relation(<redacted>)",
            Self::Column { .. } => "CatalogNodeIdentity::Column(<redacted>)",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogNodeKind {
    Schema,
    Table,
    View,
    Column,
}

#[derive(Clone, PartialEq, Eq, Serialize)]
pub struct CatalogNode {
    pub identity: CatalogNodeIdentity,
    pub kind: CatalogNodeKind,
    pub name: String,
    pub type_name: Option<String>,
    pub nullable: Option<bool>,
    pub ordinal: Option<u32>,
}

impl fmt::Debug for CatalogNode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CatalogNode")
            .field("identity", &self.identity)
            .field("kind", &self.kind)
            .field("name", &"<redacted>")
            .field("type_name", &self.type_name.as_ref().map(|_| "<redacted>"))
            .field("nullable", &self.nullable)
            .field("ordinal", &self.ordinal)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct CatalogRetainedCounts {
    pub schemas: usize,
    pub relations: usize,
    pub columns: usize,
    pub columns_in_relation: usize,
}

#[derive(Clone, PartialEq, Eq, Serialize)]
pub struct CatalogPage {
    pub identity: RequestIdentity,
    pub level: CatalogLevel,
    pub parent: Option<CatalogNodeIdentity>,
    pub nodes: Vec<CatalogNode>,
    pub next_token: Option<CatalogPageToken>,
    pub retained_counts: CatalogRetainedCounts,
    pub retained_utf8_bytes: usize,
    pub truncated: bool,
    pub stale: bool,
    pub loaded_at: String,
}

impl fmt::Debug for CatalogPage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CatalogPage")
            .field("identity", &self.identity)
            .field("level", &self.level)
            .field("parent", &self.parent)
            .field("node_count", &self.nodes.len())
            .field("has_next_token", &self.next_token.is_some())
            .field("retained_counts", &self.retained_counts)
            .field("retained_utf8_bytes", &self.retained_utf8_bytes)
            .field("truncated", &self.truncated)
            .field("stale", &self.stale)
            .field("loaded_at", &self.loaded_at)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub enum RedisKeyFilter {
    LiteralPrefix(String),
    Glob(String),
}

impl fmt::Debug for RedisKeyFilter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::LiteralPrefix(_) => "RedisKeyFilter::LiteralPrefix(<redacted>)",
            Self::Glob(_) => "RedisKeyFilter::Glob(<redacted>)",
        })
    }
}

impl RedisKeyFilter {
    pub fn validate(&self) -> Result<(), RequestValidationError> {
        let value = match self {
            Self::LiteralPrefix(value) | Self::Glob(value) => value,
        };
        if value.len() > MAX_REDIS_FILTER_BYTES {
            return Err(RequestValidationError::RedisFilterTooLarge);
        }
        Ok(())
    }

    pub fn match_pattern(&self) -> Result<String, RequestValidationError> {
        self.validate()?;
        match self {
            Self::Glob(pattern) => Ok(pattern.clone()),
            Self::LiteralPrefix(prefix) => {
                let mut pattern = String::with_capacity(prefix.len().saturating_add(1));
                for character in prefix.chars() {
                    if matches!(character, '*' | '?' | '[' | ']' | '\\') {
                        pattern.push('\\');
                    }
                    pattern.push(character);
                }
                pattern.push('*');
                Ok(pattern)
            }
        }
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
pub struct RedisKeyId(pub Vec<u8>);

impl fmt::Debug for RedisKeyId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("RedisKeyId")
            .field(&format_args!("<redacted:{} bytes>", self.0.len()))
            .finish()
    }
}

impl RedisKeyId {
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }
}

/// Redis scan input is value-bearing and intentionally not serializable.
///
/// ```compile_fail
/// fn requires_serialize<T: serde::Serialize>() {}
/// requires_serialize::<dbotter::model::RedisScanRequest>();
/// ```
#[derive(Clone, PartialEq, Eq)]
pub struct RedisScanRequest {
    pub identity: RequestIdentity,
    pub filter: RedisKeyFilter,
    pub cursor: u64,
    pub count_hint: u32,
    pub timeout: Duration,
}

impl fmt::Debug for RedisScanRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RedisScanRequest")
            .field("identity", &self.identity)
            .field("filter", &self.filter)
            .field("cursor", &self.cursor)
            .field("count_hint", &self.count_hint)
            .field("timeout", &self.timeout)
            .finish()
    }
}

impl RedisScanRequest {
    pub fn identity(&self) -> &RequestIdentity {
        &self.identity
    }

    pub fn profile_id(&self) -> &ProfileId {
        &self.identity.profile_id
    }

    pub const fn profile_generation(&self) -> ProfileGeneration {
        self.identity.profile_generation
    }

    pub const fn operation_id(&self) -> OperationId {
        self.identity.operation_id
    }

    pub fn validate(&self) -> Result<(), RequestValidationError> {
        self.filter.validate()?;
        if self.count_hint == 0 || self.count_hint > MAX_REDIS_SCAN_COUNT {
            return Err(RequestValidationError::InvalidRedisScanCount);
        }
        if self.timeout < Duration::from_secs(1) || self.timeout > MAX_CATALOG_TIMEOUT {
            return Err(RequestValidationError::InvalidCatalogTimeout);
        }
        Ok(())
    }
}

/// Redis inspect input carries an exact raw key and is intentionally not
/// serializable.
///
/// ```compile_fail
/// fn requires_serialize<T: serde::Serialize>() {}
/// requires_serialize::<dbotter::model::RedisKeyInspectRequest>();
/// ```
#[derive(Clone, PartialEq, Eq)]
pub struct RedisKeyInspectRequest {
    pub identity: RequestIdentity,
    pub key: RedisKeyId,
    pub timeout: Duration,
}

impl fmt::Debug for RedisKeyInspectRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RedisKeyInspectRequest")
            .field("identity", &self.identity)
            .field("key", &self.key)
            .field("timeout", &self.timeout)
            .finish()
    }
}

impl RedisKeyInspectRequest {
    pub fn identity(&self) -> &RequestIdentity {
        &self.identity
    }

    pub fn profile_id(&self) -> &ProfileId {
        &self.identity.profile_id
    }

    pub const fn profile_generation(&self) -> ProfileGeneration {
        self.identity.profile_generation
    }

    pub const fn operation_id(&self) -> OperationId {
        self.identity.operation_id
    }

    pub fn validate(&self) -> Result<(), RequestValidationError> {
        if self.key.0.len() > MAX_REDIS_KEY_BYTES {
            return Err(RequestValidationError::RedisKeyTooLarge);
        }
        if self.timeout < Duration::from_secs(1) || self.timeout > MAX_CATALOG_TIMEOUT {
            return Err(RequestValidationError::InvalidCatalogTimeout);
        }
        Ok(())
    }
}

#[derive(Clone, PartialEq, Eq, Serialize)]
pub struct RedisKeyEntry {
    #[serde(skip)]
    pub id: RedisKeyId,
    pub key_base64: String,
    pub display: String,
    pub hex: String,
}

impl RedisKeyEntry {
    pub fn new(id: RedisKeyId) -> Self {
        let key_base64 = base64::engine::general_purpose::STANDARD.encode(id.as_bytes());
        let display = String::from_utf8_lossy(id.as_bytes()).into_owned();
        let mut hex = String::with_capacity(id.0.len().saturating_mul(2));
        for byte in id.as_bytes() {
            use std::fmt::Write as _;
            let _ = write!(&mut hex, "{byte:02x}");
        }
        Self {
            id,
            key_base64,
            display,
            hex,
        }
    }
}

impl fmt::Debug for RedisKeyEntry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RedisKeyEntry")
            .field("id", &self.id)
            .field("key_base64", &"<redacted>")
            .field("display", &"<redacted>")
            .field("hex", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RedisScanConsistency {
    Weak,
}

#[derive(Clone, PartialEq, Eq, Serialize)]
pub struct RedisKeyPage {
    pub identity: RequestIdentity,
    pub next_cursor: u64,
    pub keys: Vec<RedisKeyEntry>,
    pub retained_count: usize,
    pub skipped_oversize: usize,
    pub retained_bytes: usize,
    pub consistency: RedisScanConsistency,
    pub truncated: bool,
    pub stale: bool,
}

impl fmt::Debug for RedisKeyPage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RedisKeyPage")
            .field("identity", &self.identity)
            .field("next_cursor", &self.next_cursor)
            .field("key_count", &self.keys.len())
            .field("retained_count", &self.retained_count)
            .field("skipped_oversize", &self.skipped_oversize)
            .field("retained_bytes", &self.retained_bytes)
            .field("consistency", &self.consistency)
            .field("truncated", &self.truncated)
            .field("stale", &self.stale)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RedisValueType {
    String,
    Hash,
    List,
    Set,
    SortedSet,
    Stream,
    ModuleOrUnknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "state", content = "milliseconds", rename_all = "snake_case")]
pub enum RedisTtl {
    Missing,
    Persistent,
    ExpiresIn(i64),
}

#[derive(Clone, PartialEq, Serialize)]
pub struct RedisValuePreview {
    pub identity: RequestIdentity,
    pub key: RedisKeyEntry,
    pub value_type: RedisValueType,
    pub ttl: RedisTtl,
    pub size: Option<u64>,
    pub items: Vec<Cell>,
    pub retained_items: usize,
    pub retained_bytes: usize,
    pub truncated: bool,
    pub stale: bool,
    pub transient_allocation: TransientAllocationQualification,
    pub notices: Vec<ResultNotice>,
}

impl fmt::Debug for RedisValuePreview {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RedisValuePreview")
            .field("identity", &self.identity)
            .field("key", &self.key)
            .field("value_type", &self.value_type)
            .field("ttl", &self.ttl)
            .field("size", &self.size)
            .field("item_count", &self.items.len())
            .field("retained_items", &self.retained_items)
            .field("retained_bytes", &self.retained_bytes)
            .field("truncated", &self.truncated)
            .field("stale", &self.stale)
            .field("transient_allocation", &self.transient_allocation)
            .field("notice_count", &self.notices.len())
            .finish()
    }
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
    pub profile_generation: ProfileGeneration,
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
            .field("profile_id", &"<redacted>")
            .field("profile_generation", &self.profile_generation)
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
    TextPreview {
        preview: String,
        original_len: usize,
    },
    Bytes {
        retained: Vec<u8>,
        original_len: usize,
    },
    Json(serde_json::Value),
    JsonPreview {
        preview: String,
        original_len: usize,
    },
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
            Self::TextPreview { .. } => "Cell::TextPreview(<redacted>)",
            Self::Bytes { .. } => "Cell::Bytes(<redacted>)",
            Self::Json(_) => "Cell::Json(<redacted>)",
            Self::JsonPreview { .. } => "Cell::JsonPreview(<redacted>)",
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

/// Driver-only decoded output. It is deliberately not serializable; only a
/// capped `ResultSnapshot` may cross UI or CLI value boundaries.
///
/// ```compile_fail
/// fn requires_serialize<T: serde::Serialize>() {}
/// requires_serialize::<dbotter::model::QueryResult>();
/// ```
#[derive(Clone)]
pub struct QueryResult {
    pub columns: Vec<Column>,
    pub rows: Vec<Vec<Cell>>,
    pub affected_rows: u64,
    pub last_insert_id: Option<u64>,
    pub elapsed_ms: u128,
    pub truncated: bool,
    /// Records only that backend prose existed. The prose itself is never
    /// representable in this boundary type.
    pub backend_notices_present: bool,
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
            .field("backend_notices_present", &self.backend_notices_present)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq, Serialize)]
pub struct ResultProvenance {
    pub result_id: ResultId,
    pub profile_id: ProfileId,
    pub profile_generation: ProfileGeneration,
    pub operation_id: OperationId,
    pub driver: DriverKind,
    pub completed_at_unix_ms: i64,
    pub duration_ms: u128,
}

impl fmt::Debug for ResultProvenance {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResultProvenance")
            .field("result_id", &self.result_id)
            .field("profile_id", &"<redacted>")
            .field("profile_generation", &self.profile_generation)
            .field("operation_id", &self.operation_id)
            .field("driver", &self.driver)
            .field("completed_at_unix_ms", &self.completed_at_unix_ms)
            .field("duration_ms", &self.duration_ms)
            .finish()
    }
}

/// Static-only retained notices. Backend warning strings are deliberately not
/// representable in a `ResultSnapshot`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResultNotice {
    DriverReportedTruncation,
    ColumnLimitReached,
    RowLimitReached,
    SnapshotByteLimitReached,
    CellPreviewTruncated,
    RedisCellLimitReached,
    RedisDepthLimitReached,
    BackendNoticesDiscarded,
    MySqlTransientRowAllocation,
    RedisTransientFrameAllocation,
}

impl ResultNotice {
    pub const fn message(self) -> &'static str {
        match self {
            Self::DriverReportedTruncation => "The driver reported a truncated result.",
            Self::ColumnLimitReached => "Additional columns were not retained.",
            Self::RowLimitReached => "Additional rows were not retained.",
            Self::SnapshotByteLimitReached => "The retained result byte limit was reached.",
            Self::CellPreviewTruncated => "One or more cells contain bounded previews.",
            Self::RedisCellLimitReached => "Additional Redis cells were not retained.",
            Self::RedisDepthLimitReached => "Nested Redis data exceeded the retained depth limit.",
            Self::BackendNoticesDiscarded => "Backend warning text was not retained.",
            Self::MySqlTransientRowAllocation => {
                "The driver may materialize the current MySQL row or cell before retention limits apply."
            }
            Self::RedisTransientFrameAllocation => {
                "The driver may materialize one Redis response frame before retention limits apply."
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TransientAllocationQualification {
    MySqlCurrentRowOrCell,
    RedisWholeRespFrame,
}

impl TransientAllocationQualification {
    pub const fn notice(self) -> ResultNotice {
        match self {
            Self::MySqlCurrentRowOrCell => ResultNotice::MySqlTransientRowAllocation,
            Self::RedisWholeRespFrame => ResultNotice::RedisTransientFrameAllocation,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CellTruncationKind {
    Text,
    Bytes,
    Json,
    Decimal,
    DateTime,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CellTruncation {
    pub row_index: usize,
    pub column_index: usize,
    pub kind: CellTruncationKind,
    pub retained_bytes: usize,
    pub original_len: Option<usize>,
    pub truncated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResultRetentionPolicy {
    MySql { requested_rows: usize },
    Redis { requested_cells: usize },
}

impl ResultRetentionPolicy {
    pub const fn mysql(requested_rows: usize) -> Self {
        Self::MySql { requested_rows }
    }

    pub const fn redis(requested_cells: usize) -> Self {
        Self::Redis { requested_cells }
    }

    const fn row_limit(self) -> usize {
        match self {
            Self::MySql { requested_rows } => {
                if requested_rows < MAX_RESULT_ROWS {
                    requested_rows
                } else {
                    MAX_RESULT_ROWS
                }
            }
            Self::Redis { .. } => MAX_RESULT_ROWS,
        }
    }

    const fn cell_limit(self) -> usize {
        match self {
            Self::MySql { .. } => usize::MAX,
            Self::Redis { requested_cells } => {
                if requested_cells < MAX_REDIS_CELLS {
                    requested_cells
                } else {
                    MAX_REDIS_CELLS
                }
            }
        }
    }

    const fn cell_bytes(self) -> usize {
        match self {
            Self::MySql { .. } => MAX_RESULT_CELL_BYTES,
            Self::Redis { .. } => MAX_REDIS_CELL_BYTES,
        }
    }

    const fn max_depth(self) -> Option<usize> {
        match self {
            Self::MySql { .. } => None,
            Self::Redis { .. } => Some(MAX_REDIS_DEPTH),
        }
    }

    const fn transient_allocation(self) -> TransientAllocationQualification {
        match self {
            Self::MySql { .. } => TransientAllocationQualification::MySqlCurrentRowOrCell,
            Self::Redis { .. } => TransientAllocationQualification::RedisWholeRespFrame,
        }
    }
}

#[derive(Clone, PartialEq, Serialize)]
pub struct ResultSnapshot {
    pub provenance: ResultProvenance,
    pub columns: Vec<Column>,
    pub rows: Vec<Vec<Cell>>,
    pub affected_rows: u64,
    pub last_insert_id: Option<u64>,
    pub truncated: bool,
    pub notices: Vec<ResultNotice>,
    pub retained_bytes: usize,
    pub transient_allocation: TransientAllocationQualification,
    pub cell_truncations: Vec<CellTruncation>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExportFormat {
    Csv,
    Tsv,
    Json,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverwritePolicy {
    DenyOverwrite,
    ReplaceConfirmed,
}

/// Value-bearing export work is intentionally not serializable. The immutable
/// snapshot and destination never cross logging or command-wire boundaries.
///
/// ```compile_fail
/// fn requires_serialize<T: serde::Serialize>() {}
/// requires_serialize::<dbotter::model::ExportResult>();
/// ```
#[derive(Clone)]
pub struct ExportResult {
    pub result_id: ResultId,
    pub operation_id: OperationId,
    pub snapshot: Arc<ResultSnapshot>,
    pub format: ExportFormat,
    pub destination: PathBuf,
    pub overwrite_policy: OverwritePolicy,
}

impl fmt::Debug for ExportResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExportResult")
            .field("result_id", &self.result_id)
            .field("operation_id", &self.operation_id)
            .field("snapshot", &"<redacted>")
            .field("format", &self.format)
            .field("destination", &"<redacted>")
            .field("overwrite_policy", &self.overwrite_policy)
            .finish()
    }
}

impl fmt::Debug for ResultSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResultSnapshot")
            .field("provenance", &self.provenance)
            .field("column_count", &self.columns.len())
            .field("row_count", &self.rows.len())
            .field("affected_rows", &self.affected_rows)
            .field("has_last_insert_id", &self.last_insert_id.is_some())
            .field("truncated", &self.truncated)
            .field("notice_count", &self.notices.len())
            .field("retained_bytes", &self.retained_bytes)
            .field("transient_allocation", &self.transient_allocation)
            .field("cell_truncation_count", &self.cell_truncations.len())
            .finish()
    }
}

impl ResultSnapshot {
    /// Converts a decoded driver result into the only retained result form.
    /// Driver decoding may already have materialized one row/cell or one RESP
    /// frame; the explicit qualification and static notice preserve that fact.
    pub fn retain(
        result: QueryResult,
        provenance: ResultProvenance,
        policy: ResultRetentionPolicy,
    ) -> Self {
        let QueryResult {
            columns: source_columns,
            rows: source_rows,
            affected_rows,
            last_insert_id,
            elapsed_ms: _,
            truncated: driver_truncated,
            backend_notices_present,
        } = result;

        let transient_allocation = policy.transient_allocation();
        let mut notices = Vec::with_capacity(8);
        push_result_notice(&mut notices, transient_allocation.notice());
        if driver_truncated {
            push_result_notice(&mut notices, ResultNotice::DriverReportedTruncation);
        }
        if backend_notices_present {
            push_result_notice(&mut notices, ResultNotice::BackendNoticesDiscarded);
        }

        let source_column_count = source_columns.len();
        let mut columns = Vec::with_capacity(source_column_count.min(MAX_RESULT_COLUMNS));
        let mut retained_bytes = 0_usize;
        for column in source_columns.into_iter().take(MAX_RESULT_COLUMNS) {
            let column_bytes = column.name.len().saturating_add(column.type_name.len());
            if column_bytes > MAX_RESULT_CELL_BYTES {
                push_result_notice(&mut notices, ResultNotice::SnapshotByteLimitReached);
                break;
            }
            if retained_bytes.saturating_add(column_bytes) > MAX_RESULT_BYTES {
                push_result_notice(&mut notices, ResultNotice::SnapshotByteLimitReached);
                break;
            }
            retained_bytes += column_bytes;
            columns.push(column);
        }
        let mut truncated = driver_truncated || columns.len() < source_column_count;
        if columns.len() < source_column_count {
            push_result_notice(&mut notices, ResultNotice::ColumnLimitReached);
        }

        let source_row_count = source_rows.len();
        let row_limit = policy.row_limit();
        let cell_limit = policy.cell_limit();
        let mut retained_cells = 0_usize;
        let mut rows = Vec::with_capacity(source_row_count.min(row_limit));
        let mut cell_truncations = Vec::new();

        for source_row in source_rows.into_iter().take(row_limit) {
            if retained_bytes >= MAX_RESULT_BYTES || retained_cells >= cell_limit {
                truncated = true;
                if retained_bytes >= MAX_RESULT_BYTES {
                    push_result_notice(&mut notices, ResultNotice::SnapshotByteLimitReached);
                }
                if retained_cells >= cell_limit {
                    push_result_notice(&mut notices, ResultNotice::RedisCellLimitReached);
                }
                break;
            }

            let row_index = rows.len();
            let source_cell_count = source_row.len();
            let allowed_columns = columns.len().min(source_cell_count);
            let allowed_cells = allowed_columns.min(cell_limit.saturating_sub(retained_cells));
            let mut retained_row = Vec::with_capacity(allowed_cells);
            let row_start_bytes = retained_bytes;
            let truncation_start = cell_truncations.len();
            let mut row_complete = true;

            for (column_index, cell) in source_row.into_iter().take(allowed_cells).enumerate() {
                let remaining = MAX_RESULT_BYTES.saturating_sub(retained_bytes);
                if remaining == 0 {
                    row_complete = false;
                    break;
                }
                let cell_cap = policy.cell_bytes().min(remaining);
                let retained =
                    retain_cell(cell, cell_cap, policy.max_depth(), row_index, column_index);
                if retained.retained_bytes > remaining {
                    row_complete = false;
                    break;
                }
                if retained.depth_truncated {
                    push_result_notice(&mut notices, ResultNotice::RedisDepthLimitReached);
                }
                if let Some(metadata) = retained.truncation {
                    truncated = true;
                    cell_truncations.push(metadata);
                    push_result_notice(&mut notices, ResultNotice::CellPreviewTruncated);
                }
                retained_bytes = retained_bytes.saturating_add(retained.retained_bytes);
                retained_cells += 1;
                retained_row.push(retained.cell);
            }

            if allowed_cells < source_cell_count || allowed_cells < columns.len() {
                row_complete = false;
            }
            if !row_complete {
                truncated = true;
            }
            if retained_row.len() != columns.len() && !columns.is_empty() {
                retained_bytes = row_start_bytes;
                retained_cells = retained_cells.saturating_sub(retained_row.len());
                cell_truncations.truncate(truncation_start);
                push_result_notice(&mut notices, ResultNotice::SnapshotByteLimitReached);
                break;
            }
            rows.push(retained_row);
        }

        if rows.len() < source_row_count {
            truncated = true;
            push_result_notice(&mut notices, ResultNotice::RowLimitReached);
        }
        if retained_cells >= cell_limit && source_row_count > rows.len() {
            push_result_notice(&mut notices, ResultNotice::RedisCellLimitReached);
        }
        if notices.len() > MAX_RESULT_NOTICES {
            notices.truncate(MAX_RESULT_NOTICES);
        }

        Self {
            provenance,
            columns,
            rows,
            affected_rows,
            last_insert_id,
            truncated,
            notices,
            retained_bytes,
            transient_allocation,
            cell_truncations,
        }
    }
}

struct RetainedCell {
    cell: Cell,
    retained_bytes: usize,
    truncation: Option<CellTruncation>,
    depth_truncated: bool,
}

fn retain_cell(
    cell: Cell,
    cap: usize,
    max_depth: Option<usize>,
    row_index: usize,
    column_index: usize,
) -> RetainedCell {
    match cell {
        Cell::Text(value) => retain_text_cell(value, None, cap, row_index, column_index),
        Cell::TextPreview {
            preview,
            original_len,
        } => retain_text_cell(preview, Some(original_len), cap, row_index, column_index),
        Cell::Decimal(value) => retain_string_cell(
            value,
            cap,
            row_index,
            column_index,
            CellTruncationKind::Decimal,
            Cell::Decimal,
        ),
        Cell::DateTime(value) => retain_string_cell(
            value,
            cap,
            row_index,
            column_index,
            CellTruncationKind::DateTime,
            Cell::DateTime,
        ),
        Cell::Bytes {
            mut retained,
            original_len,
        } => {
            let original_len = original_len.max(retained.len());
            retained.truncate(cap);
            let retained_bytes = retained.len();
            let truncated = retained_bytes < original_len;
            RetainedCell {
                cell: Cell::Bytes {
                    retained,
                    original_len,
                },
                retained_bytes,
                truncation: truncated.then_some(CellTruncation {
                    row_index,
                    column_index,
                    kind: CellTruncationKind::Bytes,
                    retained_bytes,
                    original_len: Some(original_len),
                    truncated: true,
                }),
                depth_truncated: false,
            }
        }
        Cell::Json(mut value) => {
            let original_len = serde_json::to_vec(&value).map_or(0, |encoded| encoded.len());
            let depth_truncated = max_depth.is_some_and(|depth| {
                let mut did_truncate = false;
                truncate_json_depth(&mut value, 1, depth, &mut did_truncate);
                did_truncate
            });
            let mut retained = serde_json::to_string(&value).unwrap_or_else(|_| "null".to_owned());
            let encoded_len = retained.len();
            truncate_utf8(&mut retained, cap);
            let byte_truncated = retained.len() < encoded_len;
            let truncated = depth_truncated || byte_truncated;
            let retained_bytes = retained.len();
            RetainedCell {
                cell: if truncated {
                    Cell::JsonPreview {
                        preview: retained,
                        original_len,
                    }
                } else {
                    Cell::Json(value)
                },
                retained_bytes,
                truncation: truncated.then_some(CellTruncation {
                    row_index,
                    column_index,
                    kind: CellTruncationKind::Json,
                    retained_bytes,
                    original_len: Some(original_len),
                    truncated: true,
                }),
                depth_truncated,
            }
        }
        Cell::JsonPreview {
            mut preview,
            original_len,
        } => {
            let original_len = original_len.max(preview.len());
            truncate_utf8(&mut preview, cap);
            let retained_bytes = preview.len();
            RetainedCell {
                cell: Cell::JsonPreview {
                    preview,
                    original_len,
                },
                retained_bytes,
                truncation: Some(CellTruncation {
                    row_index,
                    column_index,
                    kind: CellTruncationKind::Json,
                    retained_bytes,
                    original_len: Some(original_len),
                    truncated: true,
                }),
                depth_truncated: false,
            }
        }
        scalar => {
            let retained_bytes = scalar_retained_bytes(&scalar);
            RetainedCell {
                cell: scalar,
                retained_bytes,
                truncation: None,
                depth_truncated: false,
            }
        }
    }
}

fn retain_text_cell(
    mut preview: String,
    declared_original_len: Option<usize>,
    cap: usize,
    row_index: usize,
    column_index: usize,
) -> RetainedCell {
    let original_len = declared_original_len
        .unwrap_or(preview.len())
        .max(preview.len());
    truncate_utf8(&mut preview, cap);
    let retained_bytes = preview.len();
    let truncated = declared_original_len.is_some() || retained_bytes < original_len;
    RetainedCell {
        cell: if truncated {
            Cell::TextPreview {
                preview,
                original_len,
            }
        } else {
            Cell::Text(preview)
        },
        retained_bytes,
        truncation: truncated.then_some(CellTruncation {
            row_index,
            column_index,
            kind: CellTruncationKind::Text,
            retained_bytes,
            original_len: Some(original_len),
            truncated: true,
        }),
        depth_truncated: false,
    }
}

fn retain_string_cell(
    mut value: String,
    cap: usize,
    row_index: usize,
    column_index: usize,
    kind: CellTruncationKind,
    wrap: impl FnOnce(String) -> Cell,
) -> RetainedCell {
    let original_len = value.len();
    truncate_utf8(&mut value, cap);
    let retained_bytes = value.len();
    let truncation = (retained_bytes < original_len).then_some(CellTruncation {
        row_index,
        column_index,
        kind,
        retained_bytes,
        original_len: Some(original_len),
        truncated: true,
    });
    RetainedCell {
        cell: wrap(value),
        retained_bytes,
        truncation,
        depth_truncated: false,
    }
}

fn truncate_utf8(value: &mut String, cap: usize) {
    if value.len() <= cap {
        return;
    }
    let mut boundary = cap;
    while boundary > 0 && !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value.truncate(boundary);
}

fn truncate_json_depth(
    value: &mut serde_json::Value,
    depth: usize,
    max_depth: usize,
    truncated: &mut bool,
) {
    match value {
        serde_json::Value::Array(values) => {
            if depth >= max_depth {
                if !values.is_empty() {
                    values.clear();
                    *truncated = true;
                }
            } else {
                for value in values {
                    truncate_json_depth(value, depth + 1, max_depth, truncated);
                }
            }
        }
        serde_json::Value::Object(values) => {
            if depth >= max_depth {
                if !values.is_empty() {
                    values.clear();
                    *truncated = true;
                }
            } else {
                for value in values.values_mut() {
                    truncate_json_depth(value, depth + 1, max_depth, truncated);
                }
            }
        }
        _ => {}
    }
}

fn scalar_retained_bytes(cell: &Cell) -> usize {
    match cell {
        Cell::Null => 4,
        Cell::Bool(_) => 5,
        Cell::Int(value) => value.to_string().len(),
        Cell::UInt(value) => value.to_string().len(),
        Cell::Float(value) => value.to_string().len(),
        Cell::Decimal(value) | Cell::Text(value) | Cell::DateTime(value) => value.len(),
        Cell::TextPreview { preview, .. } | Cell::JsonPreview { preview, .. } => preview.len(),
        Cell::Bytes { retained, .. } => retained.len(),
        Cell::Json(value) => serde_json::to_vec(value).map_or(0, |encoded| encoded.len()),
    }
}

fn push_result_notice(notices: &mut Vec<ResultNotice>, notice: ResultNotice) {
    if notices.len() < MAX_RESULT_NOTICES && !notices.contains(&notice) {
        notices.push(notice);
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
        result: &ResultSnapshot,
    ) -> Self {
        Self {
            status,
            operation_id,
            profile_id,
            driver,
            column_count: result.columns.len(),
            row_count: result.rows.len(),
            affected_rows: result.affected_rows,
            elapsed_ms: result.provenance.duration_ms,
            truncated: result.truncated,
            notice_count: result.notices.len(),
        }
    }
}

#[derive(Serialize)]
pub struct ExecOutput {
    pub receipt: ExecReceipt,
    pub result: ResultSnapshot,
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
