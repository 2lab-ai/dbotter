pub mod mongodb;
pub mod mysql;
pub mod mysql_catalog;
pub mod redis;

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use secrecy::SecretString;

use crate::model::{
    CatalogPage, CatalogRequest, ConnectionProfile, DriverDescriptor, DriverKind,
    MySqlPublicErrorCode, PreparedMySqlRequest, QueryResult, RedisExecuteRequest,
    RedisKeyInspectRequest, RedisKeyPage, RedisScanRequest, RedisValuePreview, TlsMode,
};

#[derive(thiserror::Error)]
pub enum DriverError {
    #[error("invalid {driver} profile")]
    InvalidConfig { driver: DriverKind, message: String },
    #[error("{driver} is unavailable")]
    Unavailable {
        driver: DriverKind,
        reason: &'static str,
    },
    #[error("{driver} operation timed out after {seconds}s")]
    Timeout { driver: DriverKind, seconds: u64 },
    #[error("mysql operation failed")]
    MySql(
        #[from]
        #[source]
        sqlx::Error,
    ),
    #[error("mysql server rejected the operation")]
    MySqlServer { code: MySqlPublicErrorCode },
    #[error("redis operation failed")]
    Redis(
        #[from]
        #[source]
        ::redis::RedisError,
    ),
    #[error("redis command could not be parsed")]
    RedisParse(String),
    #[error("mysql server prepared protocol does not support this statement")]
    PreparedStatementUnsupported { session_healthy: bool },
    #[error("mysql catalog request or page token is invalid")]
    InvalidCatalogRequest,
    #[error("unsupported {driver} operation")]
    Unsupported {
        driver: DriverKind,
        operation: String,
    },
}

impl fmt::Debug for DriverError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfig { driver, .. } => formatter
                .debug_struct("InvalidConfig")
                .field("driver", driver)
                .field("message", &"<redacted>")
                .finish(),
            Self::Unavailable { driver, .. } => formatter
                .debug_struct("Unavailable")
                .field("driver", driver)
                .field("reason", &"<redacted>")
                .finish(),
            Self::Timeout { driver, seconds } => formatter
                .debug_struct("Timeout")
                .field("driver", driver)
                .field("seconds", seconds)
                .finish(),
            Self::MySql(_) => formatter.write_str("MySql(<redacted>)"),
            Self::MySqlServer { code } => formatter
                .debug_struct("MySqlServer")
                .field("code", code)
                .finish(),
            Self::Redis(_) => formatter.write_str("Redis(<redacted>)"),
            Self::RedisParse(_) => formatter.write_str("RedisParse(<redacted>)"),
            Self::PreparedStatementUnsupported { session_healthy } => formatter
                .debug_struct("PreparedStatementUnsupported")
                .field("session_healthy", session_healthy)
                .finish(),
            Self::InvalidCatalogRequest => formatter.write_str("InvalidCatalogRequest"),
            Self::Unsupported { driver, .. } => formatter
                .debug_struct("Unsupported")
                .field("driver", driver)
                .field("operation", &"<redacted>")
                .finish(),
        }
    }
}

impl DriverError {
    pub fn mysql_public_code(&self) -> Option<MySqlPublicErrorCode> {
        if let Self::MySqlServer { code } = self {
            return Some(*code);
        }
        let Self::MySql(sqlx::Error::Database(database)) = self else {
            return None;
        };
        let database = database.try_downcast_ref::<sqlx::mysql::MySqlDatabaseError>()?;
        MySqlPublicErrorCode::new(database.number(), database.code()?).ok()
    }

    pub fn is_mysql_permission_denied(&self) -> bool {
        self.mysql_public_code()
            .is_some_and(|code| matches!(code.errno(), 1044 | 1142 | 1143 | 1227 | 1370 | 1419))
    }

    pub fn is_mysql_authentication_failed(&self) -> bool {
        self.mysql_public_code()
            .is_some_and(|code| code.errno() == 1045)
    }
}

#[async_trait]
pub trait ConnectionPing: Send + Sync {
    async fn ping(&self, timeout: Duration) -> Result<(), DriverError>;
}

#[async_trait]
pub trait MySqlPreparedExecution: Send + Sync {
    async fn execute_prepared(
        &self,
        request: &PreparedMySqlRequest,
    ) -> Result<QueryResult, DriverError>;
}

#[async_trait]
pub trait RedisExecution: Send + Sync {
    async fn execute_command(
        &self,
        request: &RedisExecuteRequest,
    ) -> Result<QueryResult, DriverError>;
}

#[async_trait]
pub trait CatalogBrowser: Send + Sync {
    async fn load_page(&self, request: &CatalogRequest) -> Result<CatalogPage, DriverError>;
}

#[async_trait]
pub trait KeyspaceBrowser: Send + Sync {
    async fn scan_keys(&self, request: &RedisScanRequest) -> Result<RedisKeyPage, DriverError>;

    async fn inspect_key(
        &self,
        request: &RedisKeyInspectRequest,
    ) -> Result<RedisValuePreview, DriverError>;
}

#[derive(Clone)]
pub enum ConnectedResources {
    MySql {
        ping: Arc<dyn ConnectionPing>,
        execution: Arc<dyn MySqlPreparedExecution>,
        catalog: Arc<dyn CatalogBrowser>,
    },
    Redis {
        ping: Arc<dyn ConnectionPing>,
        execution: Arc<dyn RedisExecution>,
        keyspace: Arc<dyn KeyspaceBrowser>,
    },
}

#[derive(Clone)]
pub enum Session {
    MySql(mysql::MySqlSession),
    Redis(redis::RedisSession),
}

impl Session {
    pub async fn ping(&self, timeout: Duration) -> Result<(), DriverError> {
        match self {
            Self::MySql(session) => session.ping(timeout).await,
            Self::Redis(session) => session.ping(timeout).await,
        }
    }

    pub fn connected_resources(&self) -> ConnectedResources {
        let session = Arc::new(self.clone());
        match self {
            Self::MySql(_) => ConnectedResources::MySql {
                ping: session.clone(),
                execution: session.clone(),
                catalog: session,
            },
            Self::Redis(_) => ConnectedResources::Redis {
                ping: session.clone(),
                execution: session.clone(),
                keyspace: session,
            },
        }
    }

    pub async fn close(&self) {
        match self {
            Self::MySql(session) => session.close().await,
            Self::Redis(session) => session.close().await,
        }
    }
}

#[async_trait]
impl ConnectionPing for Session {
    async fn ping(&self, timeout: Duration) -> Result<(), DriverError> {
        Session::ping(self, timeout).await
    }
}

#[async_trait]
impl MySqlPreparedExecution for Session {
    async fn execute_prepared(
        &self,
        request: &PreparedMySqlRequest,
    ) -> Result<QueryResult, DriverError> {
        match self {
            Self::MySql(session) => session.execute_prepared(request).await,
            Self::Redis(_) => Err(DriverError::Unsupported {
                driver: DriverKind::Redis,
                operation: "mysql prepared execution".to_owned(),
            }),
        }
    }
}

#[async_trait]
impl RedisExecution for Session {
    async fn execute_command(
        &self,
        request: &RedisExecuteRequest,
    ) -> Result<QueryResult, DriverError> {
        match self {
            Self::Redis(session) => session.execute_command(request).await,
            Self::MySql(_) => Err(DriverError::Unsupported {
                driver: DriverKind::MySql,
                operation: "redis command execution".to_owned(),
            }),
        }
    }
}

#[async_trait]
impl CatalogBrowser for Session {
    async fn load_page(&self, request: &CatalogRequest) -> Result<CatalogPage, DriverError> {
        match self {
            Self::MySql(session) => session.load_page(request).await,
            Self::Redis(_) => Err(DriverError::Unsupported {
                driver: DriverKind::Redis,
                operation: "mysql catalog browsing".to_owned(),
            }),
        }
    }
}

#[async_trait]
impl KeyspaceBrowser for Session {
    async fn scan_keys(&self, request: &RedisScanRequest) -> Result<RedisKeyPage, DriverError> {
        match self {
            Self::Redis(session) => session.scan_keys(request).await,
            Self::MySql(_) => Err(DriverError::Unsupported {
                driver: DriverKind::MySql,
                operation: "redis keyspace browsing".to_owned(),
            }),
        }
    }

    async fn inspect_key(
        &self,
        request: &RedisKeyInspectRequest,
    ) -> Result<RedisValuePreview, DriverError> {
        match self {
            Self::Redis(session) => session.inspect_key(request).await,
            Self::MySql(_) => Err(DriverError::Unsupported {
                driver: DriverKind::MySql,
                operation: "redis key inspection".to_owned(),
            }),
        }
    }
}

pub fn descriptors() -> [&'static DriverDescriptor; 3] {
    [&mysql::DESCRIPTOR, &redis::DESCRIPTOR, &mongodb::DESCRIPTOR]
}

pub async fn connect(
    profile: &ConnectionProfile,
    secret: Option<&SecretString>,
    timeout: Duration,
) -> Result<Session, DriverError> {
    validate_profile(profile)?;
    if profile.driver == DriverKind::Redis && profile.tls != TlsMode::Disabled {
        return Err(DriverError::Unsupported {
            driver: DriverKind::Redis,
            operation: "non-plaintext transport is not implemented".to_owned(),
        });
    }
    match profile.driver {
        DriverKind::MySql => Ok(Session::MySql(
            mysql::MySqlSession::connect(profile, secret, timeout).await?,
        )),
        DriverKind::Redis => Ok(Session::Redis(
            redis::RedisSession::connect(profile, secret, timeout).await?,
        )),
        DriverKind::MongoDb => Err(mongodb::unavailable()),
    }
}

fn validate_profile(profile: &ConnectionProfile) -> Result<(), DriverError> {
    if profile.host.trim().is_empty() {
        return Err(DriverError::InvalidConfig {
            driver: profile.driver,
            message: "host is empty".to_owned(),
        });
    }
    if profile.port == 0 {
        return Err(DriverError::InvalidConfig {
            driver: profile.driver,
            message: "port must be non-zero".to_owned(),
        });
    }
    Ok(())
}
