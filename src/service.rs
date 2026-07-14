//! Shared application service for CLI and desktop runtime.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use secrecy::SecretString;
use tokio::sync::RwLock;

use crate::config::{Config, ConfigError};
use crate::drivers::{DriverError, Session};
use crate::model::{
    ConnectionProfile, DriverAvailability, DriverCapabilities, DriverKind, ExecuteRequest,
    OperationId, ProfileId, QueryLanguage, QueryResult,
};
use crate::secrets::SecretError;

#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Secret(#[from] SecretError),
    #[error(transparent)]
    Driver(#[from] DriverError),
    #[error("unknown profile: {0}")]
    UnknownProfile(String),
    #[error("query language {actual:?} does not match {driver}")]
    LanguageMismatch {
        driver: DriverKind,
        actual: QueryLanguage,
    },
    #[error("row limit must be between 1 and 10000")]
    InvalidRowLimit,
    #[error("invalid profile: {0}")]
    InvalidProfile(String),
}

#[derive(Debug, Clone)]
pub struct CheckOutcome {
    pub operation_id: OperationId,
    pub profile_id: ProfileId,
    pub driver: DriverKind,
    pub endpoint: String,
    pub elapsed_ms: u128,
}

#[derive(Debug, Clone)]
pub struct ExecuteOutcome {
    pub operation_id: OperationId,
    pub profile_id: ProfileId,
    pub driver: DriverKind,
    pub endpoint: String,
    pub result: QueryResult,
}

#[async_trait]
pub trait SessionHandle: Send + Sync {
    async fn ping(&self, timeout: Duration) -> Result<(), DriverError>;
    async fn execute(&self, request: &ExecuteRequest) -> Result<QueryResult, DriverError>;
}

#[async_trait]
impl SessionHandle for Session {
    async fn ping(&self, timeout: Duration) -> Result<(), DriverError> {
        Session::ping(self, timeout).await
    }

    async fn execute(&self, request: &ExecuteRequest) -> Result<QueryResult, DriverError> {
        Session::execute(self, request).await
    }
}

#[async_trait]
pub trait SessionConnector: Send + Sync {
    async fn connect(
        &self,
        profile: &ConnectionProfile,
        secret: Option<&SecretString>,
        timeout: Duration,
    ) -> Result<Arc<dyn SessionHandle>, DriverError>;
}

#[derive(Default)]
pub struct DriverConnector;

#[async_trait]
impl SessionConnector for DriverConnector {
    async fn connect(
        &self,
        profile: &ConnectionProfile,
        secret: Option<&SecretString>,
        timeout: Duration,
    ) -> Result<Arc<dyn SessionHandle>, DriverError> {
        let session = crate::drivers::connect(profile, secret, timeout).await?;
        Ok(Arc::new(session))
    }
}

pub trait SecretResolver: Send + Sync {
    fn resolve(&self, secret_env: Option<&str>) -> Result<Option<SecretString>, SecretError>;
}

#[derive(Default)]
pub struct EnvironmentSecrets;

impl SecretResolver for EnvironmentSecrets {
    fn resolve(&self, secret_env: Option<&str>) -> Result<Option<SecretString>, SecretError> {
        crate::secrets::resolve(secret_env)
    }
}

#[derive(Clone)]
pub struct ApplicationService {
    config: Arc<RwLock<Config>>,
    connector: Arc<dyn SessionConnector>,
    secrets: Arc<dyn SecretResolver>,
    sessions: Arc<RwLock<HashMap<ProfileId, CachedSession>>>,
}

#[derive(Clone)]
struct CachedSession {
    profile: ConnectionProfile,
    handle: Arc<dyn SessionHandle>,
}

impl ApplicationService {
    pub fn load() -> Result<Self, ServiceError> {
        Ok(Self::new(
            crate::config::load()?,
            Arc::new(DriverConnector),
            Arc::new(EnvironmentSecrets),
        ))
    }

    pub fn new(
        config: Config,
        connector: Arc<dyn SessionConnector>,
        secrets: Arc<dyn SecretResolver>,
    ) -> Self {
        Self {
            config: Arc::new(RwLock::new(config)),
            connector,
            secrets,
            sessions: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub async fn profiles_snapshot(&self) -> Vec<ConnectionProfile> {
        self.config.read().await.profiles.clone()
    }

    pub async fn language_for(
        &self,
        profile_id: &ProfileId,
    ) -> Result<QueryLanguage, ServiceError> {
        Ok(self.profile(profile_id).await?.driver.language())
    }

    pub async fn upsert_profile_path(
        &self,
        path: &Path,
        profile: ConnectionProfile,
    ) -> Result<ProfileId, ServiceError> {
        validate_persisted_profile(&profile)?;
        let profile_id = ProfileId(profile.id.clone());
        let updated = crate::config::upsert_profile_path(path, profile)?;
        self.replace_config(updated).await;
        Ok(profile_id)
    }

    pub async fn replace_config(&self, config: Config) {
        let valid_profiles: HashMap<ProfileId, ConnectionProfile> = config
            .profiles
            .iter()
            .cloned()
            .map(|profile| (ProfileId(profile.id.clone()), profile))
            .collect();
        *self.config.write().await = config;
        self.sessions
            .write()
            .await
            .retain(|profile_id, cached| valid_profiles.get(profile_id) == Some(&cached.profile));
    }

    pub async fn check(
        &self,
        operation_id: OperationId,
        profile_id: ProfileId,
        timeout: Duration,
    ) -> Result<CheckOutcome, ServiceError> {
        let profile = self.profile(&profile_id).await?;
        let started = Instant::now();
        let session = self.session_for(&profile, timeout).await?;
        if let Err(error) = session.ping(timeout).await {
            self.sessions.write().await.remove(&profile_id);
            return Err(error.into());
        }
        Ok(CheckOutcome {
            operation_id,
            profile_id,
            driver: profile.driver,
            endpoint: profile.redacted_endpoint(),
            elapsed_ms: started.elapsed().as_millis(),
        })
    }

    pub async fn execute(&self, request: ExecuteRequest) -> Result<ExecuteOutcome, ServiceError> {
        if request.row_limit == 0 || request.row_limit > 10_000 {
            return Err(ServiceError::InvalidRowLimit);
        }
        let profile = self.profile(&request.profile_id).await?;
        if profile.driver.language() != request.language {
            return Err(ServiceError::LanguageMismatch {
                driver: profile.driver,
                actual: request.language,
            });
        }
        let session = self.session_for(&profile, request.timeout).await?;
        let result = session.execute(&request).await?;
        Ok(ExecuteOutcome {
            operation_id: request.operation_id,
            profile_id: request.profile_id,
            driver: profile.driver,
            endpoint: profile.redacted_endpoint(),
            result,
        })
    }

    async fn profile(&self, profile_id: &ProfileId) -> Result<ConnectionProfile, ServiceError> {
        self.config
            .read()
            .await
            .profiles
            .iter()
            .find(|profile| profile.id == profile_id.as_str())
            .cloned()
            .ok_or_else(|| ServiceError::UnknownProfile(profile_id.0.clone()))
    }

    async fn session_for(
        &self,
        profile: &ConnectionProfile,
        timeout: Duration,
    ) -> Result<Arc<dyn SessionHandle>, ServiceError> {
        let profile_id = ProfileId(profile.id.clone());
        if let Some(cached) = self.sessions.read().await.get(&profile_id).cloned()
            && cached.profile == *profile
        {
            return Ok(cached.handle);
        }
        ensure_ready(profile)?;
        let secret = self.secrets.resolve(profile.secret_env.as_deref())?;
        let connected = self
            .connector
            .connect(profile, secret.as_ref(), timeout)
            .await?;
        let mut sessions = self.sessions.write().await;
        if let Some(cached) = sessions.get(&profile_id)
            && cached.profile == *profile
        {
            return Ok(cached.handle.clone());
        }
        sessions.insert(
            profile_id,
            CachedSession {
                profile: profile.clone(),
                handle: connected.clone(),
            },
        );
        Ok(connected)
    }
}

fn validate_persisted_profile(profile: &ConnectionProfile) -> Result<(), ServiceError> {
    let id = profile.id.trim();
    if id.is_empty() || id != profile.id || !valid_profile_id(id) {
        return Err(ServiceError::InvalidProfile(
            "profile id must start with a letter or digit and contain only letters, digits, dot, underscore, or hyphen"
                .to_owned(),
        ));
    }
    if profile.name.trim().is_empty() {
        return Err(ServiceError::InvalidProfile(
            "display name is required".to_owned(),
        ));
    }
    if profile.host.trim().is_empty() {
        return Err(ServiceError::InvalidProfile("host is required".to_owned()));
    }
    if profile.port == 0 {
        return Err(ServiceError::InvalidProfile(
            "port must be between 1 and 65535".to_owned(),
        ));
    }
    if profile.driver == DriverKind::Redis
        && let Some(database) = profile.database.as_deref()
        && database.parse::<u32>().is_err()
    {
        return Err(ServiceError::InvalidProfile(
            "Redis database must be a non-negative integer".to_owned(),
        ));
    }
    if let Some(secret_env) = profile.secret_env.as_deref()
        && !valid_env_name(secret_env)
    {
        return Err(ServiceError::InvalidProfile(
            "secret_env must be an environment-variable name".to_owned(),
        ));
    }
    Ok(())
}

fn valid_profile_id(value: &str) -> bool {
    let mut characters = value.chars();
    characters
        .next()
        .is_some_and(|character| character.is_ascii_alphanumeric())
        && characters.all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-')
        })
}

fn valid_env_name(value: &str) -> bool {
    let mut characters = value.chars();
    characters
        .next()
        .is_some_and(|character| character.is_ascii_alphabetic() || character == '_')
        && characters.all(|character| character.is_ascii_alphanumeric() || character == '_')
}

fn ensure_ready(profile: &ConnectionProfile) -> Result<(), DriverError> {
    let descriptor = crate::drivers::descriptors()
        .into_iter()
        .find(|descriptor| descriptor.kind == profile.driver)
        .ok_or_else(|| DriverError::InvalidConfig {
            driver: profile.driver,
            message: "driver descriptor is missing".to_owned(),
        })?;
    if descriptor.availability != DriverAvailability::Ready {
        return Err(DriverError::Unavailable {
            driver: profile.driver,
            reason: descriptor.reason.unwrap_or("driver is planned"),
        });
    }
    let required = match profile.driver.language() {
        QueryLanguage::Sql => DriverCapabilities::SQL,
        QueryLanguage::RedisCommand => DriverCapabilities::COMMAND,
        QueryLanguage::MongoDocument => DriverCapabilities::DOCUMENT,
    };
    if !descriptor.capabilities.contains(required) {
        return Err(DriverError::Unsupported {
            driver: profile.driver,
            operation: format!("{:?}", profile.driver.language()),
        });
    }
    Ok(())
}
