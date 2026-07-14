pub mod mongodb;
pub mod mysql;
pub mod redis;

use std::time::Duration;

use secrecy::SecretString;

use crate::model::{ConnectionProfile, DriverDescriptor, DriverKind, ExecuteRequest, QueryResult};

#[derive(Debug, thiserror::Error)]
pub enum DriverError {
    #[error("invalid {driver} profile: {message}")]
    InvalidConfig { driver: DriverKind, message: String },
    #[error("{driver} is unavailable: {reason}")]
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
    #[error("unsupported {driver} operation: {operation}")]
    Unsupported {
        driver: DriverKind,
        operation: String,
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

    pub async fn execute(&self, request: &ExecuteRequest) -> Result<QueryResult, DriverError> {
        match self {
            Self::MySql(session) => session.execute(request).await,
            Self::Redis(session) => session.execute(request).await,
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
