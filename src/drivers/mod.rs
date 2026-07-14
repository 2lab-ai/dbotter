pub mod mongodb;
pub mod mysql;
pub mod redis;

use std::fmt;
use std::time::Duration;

use secrecy::SecretString;

use crate::model::{
    ConnectionProfile, DriverDescriptor, DriverKind, ExecuteRequest, QueryResult, TlsMode,
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
    #[error("redis operation failed")]
    Redis(
        #[from]
        #[source]
        ::redis::RedisError,
    ),
    #[error("redis command could not be parsed")]
    RedisParse(String),
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
            Self::Redis(_) => formatter.write_str("Redis(<redacted>)"),
            Self::RedisParse(_) => formatter.write_str("RedisParse(<redacted>)"),
            Self::Unsupported { driver, .. } => formatter
                .debug_struct("Unsupported")
                .field("driver", driver)
                .field("operation", &"<redacted>")
                .finish(),
        }
    }
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

    pub async fn execute(&self, request: &ExecuteRequest) -> Result<QueryResult, DriverError> {
        match self {
            Self::MySql(session) => session.execute(request).await,
            Self::Redis(session) => session.execute(request).await,
        }
    }

    pub async fn close(&self) {
        match self {
            Self::MySql(session) => session.close().await,
            Self::Redis(session) => session.close().await,
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
