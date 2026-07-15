//! Shared exact-path application service for CLI and desktop runtime.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use base64::Engine as _;
use tokio::sync::{RwLock, Semaphore};

use crate::config::{
    CommitState, Config, ConfigError, ConfigMutation, ConfigSourceVersion, ConfigWriter,
    LoadedConfig, MigrationConsent, MutationOutcome, PostCommitObservation,
    PostCommitObservationError,
};
use crate::drivers::mysql_catalog::CatalogTokenKey;
use crate::drivers::{ConnectedResources, DriverError, Session};
use crate::execution::{
    ExecutionLanguage, ExecutionTarget, ExecutionTargetError, ValidatedExecutionTarget,
    extract_and_validate_target,
};
use crate::model::{
    CatalogPage, CatalogRequest, ConnectionDraft, ConnectionProfile, CredentialMode, DraftId,
    DriverAvailability, DriverCapabilities, DriverKind, ExecuteRequest, OperationId,
    PreparedMySqlRequest, ProfileFieldId, ProfileGeneration, ProfileId, PublicCode, PublicSummary,
    QueryLanguage, QueryResult, RedisExecuteRequest, RedisKeyInspectRequest, RedisKeyPage,
    RedisScanRequest, RedisValuePreview, RequestIdentity, RequestValidationError, ResultId,
    ResultProvenance, ResultRetentionPolicy, ResultSnapshot, SessionGeneration, TlsMode,
};
use crate::secrets::{
    ReplacementSecretBuffer, SecretError, SessionSecret, SessionSecretStore, SessionSecretUpdate,
};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProfileValidationError {
    #[error("profile field {field:?} is invalid")]
    Field {
        field: ProfileFieldId,
        code: PublicCode,
    },
}

impl ProfileValidationError {
    fn field(field: ProfileFieldId) -> Self {
        Self::Field {
            field,
            code: PublicCode::Field(field),
        }
    }
}

#[derive(thiserror::Error)]
pub enum ServiceError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Secret(#[from] SecretError),
    #[error(transparent)]
    Driver(#[from] DriverError),
    #[error("unknown profile")]
    UnknownProfile(ProfileId),
    #[error("query language does not match the selected driver")]
    LanguageMismatch {
        driver: DriverKind,
        actual: QueryLanguage,
    },
    #[error("the request does not match the selected driver")]
    DriverMismatch {
        expected: DriverKind,
        actual: DriverKind,
    },
    #[error("the request is invalid")]
    InvalidRequest { code: PublicCode },
    #[error("row limit must be between 1 and 10000")]
    InvalidRowLimit,
    #[error(transparent)]
    InvalidProfile(#[from] ProfileValidationError),
    #[error("the requested profile id already exists")]
    ProfileIdConflict {
        draft_id: DraftId,
        operation_id: OperationId,
    },
    #[error("the profile changed before this operation")]
    ProfileStale {
        profile_id: ProfileId,
        operation_id: OperationId,
    },
    #[error("a draft session credential is required")]
    DraftCredentialRequired {
        draft_id: DraftId,
        operation_id: OperationId,
        code: PublicCode,
    },
    #[error("background configuration work could not be joined")]
    ConfigTaskFailed,
    #[error("configuration mutation lane is closed")]
    MutationLaneClosed,
    #[error(transparent)]
    PostCommitObservation(#[from] PostCommitObservationError),
    #[error("configuration state is uncertain; reload the exact configured path")]
    ConfigUncertain,
}

impl fmt::Debug for ServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(_) => formatter.write_str("Config(<redacted>)"),
            Self::Secret(_) => formatter.write_str("Secret(<redacted>)"),
            Self::Driver(_) => formatter.write_str("Driver(<redacted>)"),
            Self::UnknownProfile(profile_id) => formatter
                .debug_tuple("UnknownProfile")
                .field(profile_id)
                .finish(),
            Self::LanguageMismatch { driver, actual } => formatter
                .debug_struct("LanguageMismatch")
                .field("driver", driver)
                .field("actual", actual)
                .finish(),
            Self::DriverMismatch { expected, actual } => formatter
                .debug_struct("DriverMismatch")
                .field("expected", expected)
                .field("actual", actual)
                .finish(),
            Self::InvalidRequest { code } => formatter
                .debug_struct("InvalidRequest")
                .field("code", code)
                .finish(),
            Self::InvalidRowLimit => formatter.write_str("InvalidRowLimit"),
            Self::InvalidProfile(error) => formatter
                .debug_tuple("InvalidProfile")
                .field(error)
                .finish(),
            Self::ProfileIdConflict {
                draft_id,
                operation_id,
            } => formatter
                .debug_struct("ProfileIdConflict")
                .field("draft_id", draft_id)
                .field("operation_id", operation_id)
                .finish(),
            Self::ProfileStale {
                profile_id,
                operation_id,
            } => formatter
                .debug_struct("ProfileStale")
                .field("profile_id", profile_id)
                .field("operation_id", operation_id)
                .finish(),
            Self::DraftCredentialRequired {
                draft_id,
                operation_id,
                code,
            } => formatter
                .debug_struct("DraftCredentialRequired")
                .field("draft_id", draft_id)
                .field("operation_id", operation_id)
                .field("code", code)
                .finish(),
            Self::ConfigTaskFailed => formatter.write_str("ConfigTaskFailed"),
            Self::MutationLaneClosed => formatter.write_str("MutationLaneClosed"),
            Self::PostCommitObservation(error) => formatter
                .debug_tuple("PostCommitObservation")
                .field(error)
                .finish(),
            Self::ConfigUncertain => formatter.write_str("ConfigUncertain"),
        }
    }
}

impl ServiceError {
    pub fn public_error_parts(&self) -> (PublicSummary, PublicCode) {
        let code = match self {
            Self::InvalidProfile(ProfileValidationError::Field { code, .. })
            | Self::InvalidRequest { code }
            | Self::DraftCredentialRequired { code, .. } => *code,
            Self::ProfileIdConflict { .. } => PublicCode::ProfileIdConflict,
            Self::ProfileStale { .. } => PublicCode::ProfileStale,
            Self::Secret(SecretError::MissingEnv(_) | SecretError::EmptyEnv(_)) => {
                PublicCode::CredentialEnvironmentName
            }
            Self::Secret(
                SecretError::ReplacementRequired | SecretError::SessionCredentialRequired,
            ) => PublicCode::SessionCredential,
            Self::Secret(SecretError::InvalidSessionIntent) => {
                PublicCode::Field(ProfileFieldId::CredentialMode)
            }
            Self::Secret(SecretError::StoreUnavailable) => PublicCode::None,
            Self::Config(ConfigError::ExternalChange | ConfigError::InvalidProfile)
            | Self::ConfigUncertain => PublicCode::ConfigExternalChange,
            Self::Driver(DriverError::PreparedStatementUnsupported { .. }) => {
                PublicCode::PreparedStatementUnsupported
            }
            Self::Driver(DriverError::InvalidCatalogRequest) => PublicCode::Catalog,
            Self::Driver(error) => error
                .mysql_public_code()
                .map_or(PublicCode::None, PublicCode::MySql),
            _ => PublicCode::None,
        };
        let summary = match self {
            Self::InvalidProfile(ProfileValidationError::Field {
                code: PublicCode::RedisTlsPreferredLegacy,
                ..
            }) => PublicSummary::UnsupportedFeature,
            Self::InvalidProfile(ProfileValidationError::Field {
                code:
                    PublicCode::RedisTlsCaInvalidPem
                    | PublicCode::RedisTlsCaUntrustedIssuer
                    | PublicCode::TlsHostnameMismatch,
                ..
            }) => PublicSummary::TlsVerificationFailed,
            Self::ProfileStale { .. }
            | Self::Config(ConfigError::ExternalChange | ConfigError::InvalidProfile)
            | Self::ConfigUncertain => PublicSummary::ResourceStale,
            Self::InvalidProfile(_)
            | Self::InvalidRowLimit
            | Self::LanguageMismatch { .. }
            | Self::DriverMismatch { .. }
            | Self::InvalidRequest { .. }
            | Self::ProfileIdConflict { .. }
            | Self::UnknownProfile(_) => PublicSummary::InvalidInput,
            Self::Driver(DriverError::InvalidCatalogRequest) => PublicSummary::InvalidInput,
            Self::Driver(error) if error.is_mysql_permission_denied() => {
                PublicSummary::PermissionDenied
            }
            Self::Driver(error) if error.is_mysql_authentication_failed() => {
                PublicSummary::AuthenticationFailed
            }
            Self::Secret(SecretError::MissingEnv(_) | SecretError::EmptyEnv(_)) => {
                PublicSummary::AuthenticationFailed
            }
            Self::DraftCredentialRequired { .. }
            | Self::Secret(
                SecretError::ReplacementRequired | SecretError::SessionCredentialRequired,
            ) => PublicSummary::CredentialRequired,
            Self::Secret(SecretError::InvalidSessionIntent) => PublicSummary::InvalidInput,
            Self::Secret(SecretError::StoreUnavailable) => PublicSummary::InternalFailure,
            Self::Driver(DriverError::Timeout { .. }) => PublicSummary::OperationTimedOut,
            Self::Driver(
                DriverError::Unavailable { .. }
                | DriverError::Unsupported { .. }
                | DriverError::PreparedStatementUnsupported { .. },
            ) => PublicSummary::UnsupportedFeature,
            Self::Driver(_) => PublicSummary::NetworkUnavailable,
            Self::Config(_) => PublicSummary::ConfigWriteNotCommitted,
            Self::PostCommitObservation(_) => PublicSummary::CommittedDurabilityUnknown,
            Self::ConfigTaskFailed | Self::MutationLaneClosed => PublicSummary::InternalFailure,
        };
        (summary, code)
    }

    pub fn public_code(&self) -> PublicCode {
        self.public_error_parts().1
    }

    pub fn public_summary(&self) -> PublicSummary {
        self.public_error_parts().0
    }
}

#[derive(Debug, Clone)]
pub struct CheckOutcome {
    pub operation_id: OperationId,
    pub profile_id: ProfileId,
    pub profile_generation: ProfileGeneration,
    pub session_generation: SessionGeneration,
    pub driver: DriverKind,
    pub endpoint: String,
    pub elapsed_ms: u128,
}

#[derive(Debug, Clone)]
pub struct ExecuteOutcome {
    pub operation_id: OperationId,
    pub profile_id: ProfileId,
    pub profile_generation: ProfileGeneration,
    pub session_generation: SessionGeneration,
    pub driver: DriverKind,
    pub endpoint: String,
    pub result: ResultSnapshot,
}

#[derive(Debug, Clone)]
pub struct DraftTestOutcome {
    pub draft_id: DraftId,
    pub operation_id: OperationId,
    pub driver: DriverKind,
    pub endpoint: String,
    pub elapsed_ms: u128,
}

/// Sensitive mutation request. It intentionally has no Serialize implementation.
///
/// ```compile_fail
/// fn requires_serialize<T: serde::Serialize>() {}
/// requires_serialize::<dbotter::service::CreateProfileRequest>();
/// ```
pub struct CreateProfileRequest {
    pub draft_id: DraftId,
    pub operation_id: OperationId,
    pub explicit_id: Option<ProfileId>,
    pub draft: ConnectionDraft,
    pub secret_update: SessionSecretUpdate,
    pub migration_consent: MigrationConsent,
}

impl fmt::Debug for CreateProfileRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CreateProfileRequest")
            .field("draft_id", &self.draft_id)
            .field("operation_id", &self.operation_id)
            .field("explicit_id", &self.explicit_id)
            .field("draft", &self.draft)
            .field("secret_update", &"<redacted>")
            .field("migration_consent", &self.migration_consent)
            .finish()
    }
}

/// Sensitive mutation request. It intentionally has no Serialize implementation.
///
/// ```compile_fail
/// fn requires_serialize<T: serde::Serialize>() {}
/// requires_serialize::<dbotter::service::UpdateProfileRequest>();
/// ```
pub struct UpdateProfileRequest {
    pub profile_id: ProfileId,
    pub expected_generation: ProfileGeneration,
    pub operation_id: OperationId,
    pub draft: ConnectionDraft,
    pub secret_update: SessionSecretUpdate,
    pub migration_consent: MigrationConsent,
}

impl fmt::Debug for UpdateProfileRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("UpdateProfileRequest")
            .field("profile_id", &self.profile_id)
            .field("expected_generation", &self.expected_generation)
            .field("operation_id", &self.operation_id)
            .field("draft", &self.draft)
            .field("secret_update", &"<redacted>")
            .field("migration_consent", &self.migration_consent)
            .finish()
    }
}

#[derive(Debug)]
pub struct DeleteProfileRequest {
    pub profile_id: ProfileId,
    pub expected_generation: ProfileGeneration,
    pub operation_id: OperationId,
    pub migration_consent: MigrationConsent,
}

/// Prepared draft network request. Saved-profile identity and credential intent
/// are resolved before this value exists.
///
/// ```compile_fail
/// use dbotter::service::TestDraftRequest;
/// fn requires_serialize<T: serde::Serialize>() {}
/// requires_serialize::<TestDraftRequest>();
/// ```
///
/// ```compile_fail
/// use dbotter::model::{ConnectionDraft, DraftId, DriverKind, OperationId, ProfileId};
/// use dbotter::service::TestDraftRequest;
/// let _ = TestDraftRequest {
///     draft_id: DraftId(1),
///     operation_id: OperationId(2),
///     draft: ConnectionDraft::for_driver(DriverKind::Redis),
///     existing_profile_id: Some(ProfileId("saved".to_owned())),
///     timeout: std::time::Duration::from_secs(1),
/// };
/// ```
pub struct TestDraftRequest {
    draft_id: DraftId,
    operation_id: OperationId,
    draft: ConnectionDraft,
    credential_source: Option<DraftCredentialSource>,
    timeout: Duration,
}

enum DraftCredentialSource {
    SessionKeep(Arc<SessionSecret>),
    SessionReplace(Arc<SessionSecret>),
    EnvironmentResolved(Arc<SessionSecret>),
}

impl TestDraftRequest {
    pub(crate) fn draft_id(&self) -> DraftId {
        self.draft_id
    }

    pub(crate) fn operation_id(&self) -> OperationId {
        self.operation_id
    }

    pub(crate) fn timeout(&self) -> Duration {
        self.timeout
    }

    fn without_credential(
        draft_id: DraftId,
        operation_id: OperationId,
        draft: ConnectionDraft,
        timeout: Duration,
    ) -> Self {
        Self {
            draft_id,
            operation_id,
            draft,
            credential_source: None,
            timeout,
        }
    }

    fn with_credential(
        draft_id: DraftId,
        operation_id: OperationId,
        draft: ConnectionDraft,
        credential_source: DraftCredentialSource,
        timeout: Duration,
    ) -> Self {
        Self {
            draft_id,
            operation_id,
            draft,
            credential_source: Some(credential_source),
            timeout,
        }
    }
}

impl fmt::Debug for TestDraftRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TestDraftRequest")
            .field("draft_id", &self.draft_id)
            .field("operation_id", &self.operation_id)
            .field("draft", &self.draft)
            .field("credential_source", &"<redacted>")
            .field("timeout", &self.timeout)
            .finish()
    }
}

#[derive(Clone)]
pub struct ProfileMutationOutcome {
    pub operation_id: OperationId,
    pub profile_id: ProfileId,
    pub profile_generation: ProfileGeneration,
    pub commit_state: CommitState,
    pub migration_backup: Option<PathBuf>,
}

impl fmt::Debug for ProfileMutationOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProfileMutationOutcome")
            .field("operation_id", &self.operation_id)
            .field("profile_id", &self.profile_id)
            .field("profile_generation", &self.profile_generation)
            .field("commit_state", &self.commit_state)
            .field(
                "migration_backup",
                &self.migration_backup.as_ref().map(|_| "<available>"),
            )
            .finish()
    }
}

#[async_trait]
pub trait SessionHandle: Send + Sync {
    async fn ping(&self, timeout: Duration) -> Result<(), DriverError>;
    fn connected_resources(&self) -> Option<ConnectedResources> {
        None
    }
    async fn close(&self) -> Result<(), DriverError> {
        Ok(())
    }
}

#[async_trait]
impl SessionHandle for Session {
    async fn ping(&self, timeout: Duration) -> Result<(), DriverError> {
        Session::ping(self, timeout).await
    }

    fn connected_resources(&self) -> Option<ConnectedResources> {
        Some(Session::connected_resources(self))
    }

    async fn close(&self) -> Result<(), DriverError> {
        Session::close(self).await;
        Ok(())
    }
}

#[async_trait]
pub trait SessionConnector: Send + Sync {
    async fn connect(
        &self,
        profile: &ConnectionProfile,
        secret: Option<&SessionSecret>,
        timeout: Duration,
    ) -> Result<Arc<dyn SessionHandle>, DriverError>;

    fn supports_redis_tls(&self) -> bool {
        false
    }
}

#[derive(Default)]
pub struct DriverConnector;

#[async_trait]
impl SessionConnector for DriverConnector {
    async fn connect(
        &self,
        profile: &ConnectionProfile,
        secret: Option<&SessionSecret>,
        timeout: Duration,
    ) -> Result<Arc<dyn SessionHandle>, DriverError> {
        let session =
            crate::drivers::connect(profile, secret.map(SessionSecret::inner), timeout).await?;
        Ok(Arc::new(session))
    }
}

pub trait SecretResolver: Send + Sync {
    fn resolve_environment(&self, name: &str) -> Result<Arc<SessionSecret>, SecretError>;
}

#[derive(Default)]
pub struct EnvironmentSecrets;

impl SecretResolver for EnvironmentSecrets {
    fn resolve_environment(&self, name: &str) -> Result<Arc<SessionSecret>, SecretError> {
        crate::secrets::resolve_environment(name)
    }
}

#[derive(Clone)]
pub struct ApplicationService {
    config_path: Arc<PathBuf>,
    state: Arc<RwLock<ServiceState>>,
    connector: Arc<dyn SessionConnector>,
    environment: Arc<dyn SecretResolver>,
    session_secrets: Arc<SessionSecretStore>,
    catalog_token_key: Arc<CatalogTokenKey>,
    next_generation: Arc<AtomicU64>,
    next_session_generation: Arc<AtomicU64>,
    next_result_id: Arc<AtomicU64>,
    writer: ConfigWriter,
    mutation_lane: Arc<Semaphore>,
    config_uncertain: Arc<AtomicBool>,
}

struct ServiceState {
    observed: ObservedState,
    sessions: HashMap<ProfileId, CachedSession>,
}

struct ObservedState {
    config: Config,
    source_version: ConfigSourceVersion,
    generations: HashMap<ProfileId, ProfileGeneration>,
    tombstones: HashMap<ProfileId, ProfileGeneration>,
}

#[derive(Clone, PartialEq, Eq)]
pub struct ConnectionFingerprint {
    driver: DriverKind,
    host: String,
    port: u16,
    database: Option<String>,
    username: Option<String>,
    tls: TlsMode,
    credential_mode: CredentialMode,
    secret_env: Option<String>,
    redis_ca_file: Option<PathBuf>,
}

impl fmt::Debug for ConnectionFingerprint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ConnectionFingerprint(<redacted>)")
    }
}

impl From<&ConnectionProfile> for ConnectionFingerprint {
    fn from(profile: &ConnectionProfile) -> Self {
        Self {
            driver: profile.driver,
            host: profile.host.clone(),
            port: profile.port,
            database: profile.database.clone(),
            username: profile.username.clone(),
            tls: profile.tls,
            credential_mode: profile.credential_mode,
            secret_env: profile.secret_env.clone(),
            redis_ca_file: profile.redis_tls.ca_file.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CachedSessionIdentity {
    pub profile_generation: ProfileGeneration,
    pub session_generation: SessionGeneration,
    pub connection_fingerprint: ConnectionFingerprint,
}

pub(crate) struct RuntimeUpdateOutcome {
    pub(crate) mutation: ProfileMutationOutcome,
    pub(crate) deferred_session: Option<DeferredSessionFence>,
    pub(crate) cleanup: DeferredRuntimeCleanup,
}

pub(crate) struct DeferredSessionFence {
    profile_id: ProfileId,
    previous: CachedSessionIdentity,
    next_profile_generation: ProfileGeneration,
    next_fingerprint: ConnectionFingerprint,
    retag_eligible: bool,
}

pub(crate) struct DeferredRuntimeCleanup {
    targets: Vec<DeferredCleanupTarget>,
    secret_updates: Vec<(ProfileId, SessionSecretUpdate)>,
    clear_all_secrets: bool,
    retain_secret_profiles: Option<HashSet<ProfileId>>,
}

pub(crate) struct DeferredCleanupTarget {
    profile_id: ProfileId,
    previous_generation: ProfileGeneration,
    session: Option<CachedSessionIdentity>,
    clear_secret: bool,
}

impl DeferredRuntimeCleanup {
    fn empty() -> Self {
        Self {
            targets: Vec::new(),
            secret_updates: Vec::new(),
            clear_all_secrets: false,
            retain_secret_profiles: None,
        }
    }

    pub(crate) fn targets(&self) -> impl Iterator<Item = (&ProfileId, ProfileGeneration)> {
        self.targets
            .iter()
            .map(|target| (&target.profile_id, target.previous_generation))
    }
}

pub(crate) struct RuntimeDeleteOutcome {
    pub(crate) mutation: ProfileMutationOutcome,
    pub(crate) cleanup: DeferredRuntimeCleanup,
}

pub(crate) struct RuntimeCreateOutcome {
    pub(crate) mutation: ProfileMutationOutcome,
    pub(crate) cleanup: DeferredRuntimeCleanup,
}

pub(crate) struct RuntimeMutationFailure {
    pub(crate) error: ServiceError,
    pub(crate) cleanup: DeferredRuntimeCleanup,
}

impl From<ServiceError> for RuntimeMutationFailure {
    fn from(error: ServiceError) -> Self {
        Self {
            error,
            cleanup: DeferredRuntimeCleanup::empty(),
        }
    }
}

impl From<ProfileValidationError> for RuntimeMutationFailure {
    fn from(error: ProfileValidationError) -> Self {
        ServiceError::from(error).into()
    }
}

impl From<SecretError> for RuntimeMutationFailure {
    fn from(error: SecretError) -> Self {
        ServiceError::from(error).into()
    }
}

pub(crate) struct RuntimeReloadOutcome {
    pub(crate) diff: Option<ReloadConfigurationOutcome>,
    pub(crate) cleanup: DeferredRuntimeCleanup,
    pub(crate) config_uncertain: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReloadConfigurationOutcome {
    pub unchanged: Vec<ProfileId>,
    pub added: Vec<ProfileId>,
    pub changed: Vec<(ProfileId, ProfileGeneration)>,
    pub removed: Vec<(ProfileId, ProfileGeneration)>,
}

#[derive(Clone)]
struct CachedSession {
    profile_generation: ProfileGeneration,
    session_generation: SessionGeneration,
    connection_fingerprint: ConnectionFingerprint,
    handle: Arc<dyn SessionHandle>,
}

impl CachedSession {
    fn identity(&self) -> CachedSessionIdentity {
        CachedSessionIdentity {
            profile_generation: self.profile_generation,
            session_generation: self.session_generation,
            connection_fingerprint: self.connection_fingerprint.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionDisposition {
    Keep,
    Evict,
}

impl SessionDisposition {
    pub fn for_driver_error(error: &DriverError) -> Self {
        match error {
            DriverError::MySql(sqlx::Error::Database(_)) | DriverError::MySqlServer { .. } => {
                Self::Keep
            }
            DriverError::Redis(error) if matches!(error.kind(), redis::ErrorKind::Server(_)) => {
                Self::Keep
            }
            DriverError::PreparedStatementUnsupported { session_healthy } => {
                if *session_healthy {
                    Self::Keep
                } else {
                    Self::Evict
                }
            }
            DriverError::InvalidCatalogRequest => Self::Keep,
            DriverError::InvalidConfig { .. }
            | DriverError::Unavailable { .. }
            | DriverError::Timeout { .. }
            | DriverError::MySql(_)
            | DriverError::Redis(_)
            | DriverError::RedisParse(_)
            | DriverError::Unsupported { .. } => Self::Evict,
        }
    }
}

pub(crate) struct SessionLease {
    profile_id: ProfileId,
    profile: ConnectionProfile,
    identity: CachedSessionIdentity,
    handle: Arc<dyn SessionHandle>,
}

pub(crate) struct DraftSessionLease {
    draft_id: DraftId,
    operation_id: OperationId,
    profile: ConnectionProfile,
    timeout: Duration,
    started: Instant,
    handle: Arc<dyn SessionHandle>,
}

impl DraftSessionLease {
    pub(crate) fn draft_id(&self) -> DraftId {
        self.draft_id
    }

    pub(crate) fn operation_id(&self) -> OperationId {
        self.operation_id
    }

    pub(crate) fn elapsed_ms(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    pub(crate) async fn ping(&self) -> Result<(), DriverError> {
        self.handle.ping(self.timeout).await
    }

    pub(crate) async fn close(&self) -> Result<(), DriverError> {
        self.handle.close().await
    }
}

impl SessionLease {
    pub(crate) fn identity(&self) -> &CachedSessionIdentity {
        &self.identity
    }

    pub(crate) async fn ping(&self, timeout: Duration) -> Result<(), DriverError> {
        self.handle.ping(timeout).await
    }

    fn connected_resources(&self) -> Result<ConnectedResources, DriverError> {
        self.handle
            .connected_resources()
            .ok_or_else(|| DriverError::Unsupported {
                driver: self.profile.driver,
                operation: "typed connected resources".to_owned(),
            })
    }

    pub(crate) async fn execute_typed(
        &self,
        request: &TypedExecuteRequest,
    ) -> Result<QueryResult, DriverError> {
        match (request, self.connected_resources()?) {
            (TypedExecuteRequest::MySql(request), ConnectedResources::MySql { execution, .. }) => {
                execution.execute_prepared(request).await
            }
            (TypedExecuteRequest::Redis(request), ConnectedResources::Redis { execution, .. }) => {
                execution.execute_command(request).await
            }
            _ => Err(DriverError::Unsupported {
                driver: self.profile.driver,
                operation: "mismatched typed execution resource".to_owned(),
            }),
        }
    }

    pub(crate) async fn load_catalog_page(
        &self,
        request: &CatalogRequest,
        token_key: &CatalogTokenKey,
    ) -> Result<CatalogPage, DriverError> {
        match self.connected_resources()? {
            ConnectedResources::MySql { catalog, .. } => {
                catalog.load_page(request, token_key).await
            }
            ConnectedResources::Redis { .. } => Err(DriverError::Unsupported {
                driver: self.profile.driver,
                operation: "mismatched catalog resource".to_owned(),
            }),
        }
    }
}

struct ObservedMutationOutcome {
    state: CommitState,
    loaded: LoadedConfig,
    migration_backup: Option<PathBuf>,
    affected_profile_id: Option<String>,
}

impl ApplicationService {
    pub fn load_path(path: impl Into<PathBuf>) -> Result<Self, ServiceError> {
        Self::with_dependencies(
            path,
            Arc::new(DriverConnector),
            Arc::new(EnvironmentSecrets),
            Arc::new(SessionSecretStore::default()),
            ConfigWriter::default(),
        )
    }

    pub fn with_dependencies(
        path: impl Into<PathBuf>,
        connector: Arc<dyn SessionConnector>,
        environment: Arc<dyn SecretResolver>,
        session_secrets: Arc<SessionSecretStore>,
        writer: ConfigWriter,
    ) -> Result<Self, ServiceError> {
        let path = path.into();
        let loaded = crate::config::load_path(&path)?;
        validate_config_identity(&loaded.config)?;
        Self::from_validated_loaded(
            path,
            loaded,
            connector,
            environment,
            session_secrets,
            writer,
        )
    }

    fn from_validated_loaded(
        path: PathBuf,
        loaded: LoadedConfig,
        connector: Arc<dyn SessionConnector>,
        environment: Arc<dyn SecretResolver>,
        session_secrets: Arc<SessionSecretStore>,
        writer: ConfigWriter,
    ) -> Result<Self, ServiceError> {
        let mut generations = HashMap::new();
        let mut next = 1_u64;
        for profile in &loaded.config.profiles {
            generations.insert(ProfileId(profile.id.clone()), ProfileGeneration(next));
            next = next.saturating_add(1);
        }
        let catalog_token_key =
            Arc::new(
                CatalogTokenKey::generate().map_err(|_| DriverError::Unavailable {
                    driver: DriverKind::MySql,
                    reason: "catalog token entropy unavailable",
                })?,
            );
        Ok(Self {
            config_path: Arc::new(path),
            state: Arc::new(RwLock::new(ServiceState {
                observed: ObservedState {
                    config: loaded.config,
                    source_version: loaded.source_version,
                    generations,
                    tombstones: HashMap::new(),
                },
                sessions: HashMap::new(),
            })),
            connector,
            environment,
            session_secrets,
            catalog_token_key,
            next_generation: Arc::new(AtomicU64::new(next)),
            next_session_generation: Arc::new(AtomicU64::new(1)),
            next_result_id: Arc::new(AtomicU64::new(1)),
            writer,
            mutation_lane: Arc::new(Semaphore::new(1)),
            config_uncertain: Arc::new(AtomicBool::new(false)),
        })
    }

    pub fn config_path(&self) -> &Path {
        self.config_path.as_path()
    }

    pub async fn source_version(&self) -> ConfigSourceVersion {
        self.state.read().await.observed.source_version
    }

    pub async fn profiles_snapshot(&self) -> Vec<ConnectionProfile> {
        self.state.read().await.observed.config.profiles.clone()
    }

    pub async fn profiles_with_generations_snapshot(
        &self,
    ) -> Vec<(ConnectionProfile, ProfileGeneration)> {
        let state = self.state.read().await;
        let observed = &state.observed;
        observed
            .config
            .profiles
            .iter()
            .filter_map(|profile| {
                observed
                    .generations
                    .get(&ProfileId(profile.id.clone()))
                    .copied()
                    .map(|generation| (profile.clone(), generation))
            })
            .collect()
    }

    pub async fn profile_generation(
        &self,
        profile_id: &ProfileId,
    ) -> Result<ProfileGeneration, ServiceError> {
        self.state
            .read()
            .await
            .observed
            .generations
            .get(profile_id)
            .copied()
            .ok_or_else(|| ServiceError::UnknownProfile(profile_id.clone()))
    }

    pub async fn tombstone_generation(&self, profile_id: &ProfileId) -> Option<ProfileGeneration> {
        self.state
            .read()
            .await
            .observed
            .tombstones
            .get(profile_id)
            .copied()
    }

    pub async fn cached_session_count(&self) -> usize {
        self.state.read().await.sessions.len()
    }

    pub async fn cached_session_identity(
        &self,
        profile_id: &ProfileId,
    ) -> Option<CachedSessionIdentity> {
        self.state
            .read()
            .await
            .sessions
            .get(profile_id)
            .map(CachedSession::identity)
    }

    /// Reports only whether the exact saved profile currently owns an
    /// in-process Session credential. The credential capability never leaves
    /// the service boundary.
    pub fn has_current_session_secret(&self, profile_id: &ProfileId) -> Result<bool, ServiceError> {
        self.session_secrets
            .has_current(profile_id)
            .map_err(ServiceError::Secret)
    }

    pub(crate) async fn needs_session_credential(
        &self,
        profile_id: &ProfileId,
    ) -> Result<bool, ServiceError> {
        let profile = self.profile(profile_id).await?;
        Ok(profile.credential_mode == CredentialMode::Session
            && !self.has_current_session_secret(profile_id)?)
    }

    pub fn is_config_uncertain(&self) -> bool {
        self.config_uncertain.load(Ordering::Acquire)
    }

    pub async fn reload_configuration(&self) -> Result<(), ServiceError> {
        self.reload_configuration_with_outcome().await.map(|_| ())
    }

    pub async fn reload_configuration_with_outcome(
        &self,
    ) -> Result<ReloadConfigurationOutcome, ServiceError> {
        let outcome = self.reload_configuration_for_runtime().await?;
        self.apply_deferred_cleanup(outcome.cleanup).await?;
        if outcome.config_uncertain {
            Err(ServiceError::ConfigUncertain)
        } else {
            outcome.diff.ok_or(ServiceError::ConfigUncertain)
        }
    }

    pub(crate) async fn reload_configuration_for_runtime(
        &self,
    ) -> Result<RuntimeReloadOutcome, ServiceError> {
        let _permit = self
            .mutation_lane
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| ServiceError::MutationLaneClosed)?;
        let path = self.config_path.as_ref().clone();
        let loaded =
            match tokio::task::spawn_blocking(move || crate::config::load_path(&path)).await {
                Ok(Ok(loaded)) if validate_config_identity(&loaded.config).is_ok() => loaded,
                Ok(Ok(_)) | Ok(Err(_)) | Err(_) => {
                    let cleanup = self.enter_config_uncertain_deferred().await;
                    return Ok(RuntimeReloadOutcome {
                        diff: None,
                        cleanup,
                        config_uncertain: true,
                    });
                }
            };
        let outcome = {
            let state = self.state.read().await;
            let observed = &state.observed;
            let previous = observed
                .config
                .profiles
                .iter()
                .map(|profile| (ProfileId(profile.id.clone()), profile))
                .collect::<HashMap<_, _>>();
            let next = loaded
                .config
                .profiles
                .iter()
                .map(|profile| (ProfileId(profile.id.clone()), profile))
                .collect::<HashMap<_, _>>();
            let mut outcome = ReloadConfigurationOutcome::default();
            for (profile_id, profile) in &next {
                match previous.get(profile_id) {
                    Some(previous_profile) if *previous_profile == *profile => {
                        outcome.unchanged.push(profile_id.clone());
                    }
                    Some(_) => {
                        if let Some(generation) = observed.generations.get(profile_id).copied() {
                            outcome.changed.push((profile_id.clone(), generation));
                        }
                    }
                    None => outcome.added.push(profile_id.clone()),
                }
            }
            for profile_id in previous.keys() {
                if !next.contains_key(profile_id)
                    && let Some(generation) = observed.generations.get(profile_id).copied()
                {
                    outcome.removed.push((profile_id.clone(), generation));
                }
            }
            outcome
                .unchanged
                .sort_by(|left, right| left.0.cmp(&right.0));
            outcome.added.sort_by(|left, right| left.0.cmp(&right.0));
            outcome
                .changed
                .sort_by(|left, right| left.0.0.cmp(&right.0.0));
            outcome
                .removed
                .sort_by(|left, right| left.0.0.cmp(&right.0.0));
            outcome
        };
        let cleanup = self.replace_loaded_config(loaded).await;
        self.config_uncertain.store(false, Ordering::Release);
        Ok(RuntimeReloadOutcome {
            diff: Some(outcome),
            cleanup,
            config_uncertain: false,
        })
    }

    pub async fn disconnect_profile_exact(
        &self,
        operation_id: OperationId,
        profile_id: &ProfileId,
        expected_generation: ProfileGeneration,
        expected_session_generation: Option<SessionGeneration>,
    ) -> Result<Option<CachedSessionIdentity>, ServiceError> {
        self.ensure_generation(profile_id, expected_generation, operation_id)
            .await?;
        let current = self.cached_session_identity(profile_id).await;
        match (current, expected_session_generation) {
            (None, None) => Ok(None),
            (Some(identity), Some(expected)) if identity.session_generation == expected => {
                if self
                    .evict_cached_session_exact(profile_id, expected_generation, expected)
                    .await
                {
                    Ok(Some(identity))
                } else {
                    Err(ServiceError::ProfileStale {
                        profile_id: profile_id.clone(),
                        operation_id,
                    })
                }
            }
            (Some(_), None) | (None, Some(_)) | (Some(_), Some(_)) => {
                Err(ServiceError::ProfileStale {
                    profile_id: profile_id.clone(),
                    operation_id,
                })
            }
        }
    }

    pub async fn shutdown_runtime(&self) {
        let handles = self
            .state
            .write()
            .await
            .sessions
            .drain()
            .map(|(_, cached)| cached.handle)
            .collect::<Vec<_>>();
        let _ = self.session_secrets.clear_all();
        for handle in handles {
            let _ = handle.close().await;
        }
    }

    pub fn prepare_secretless_draft_test(
        &self,
        draft_id: DraftId,
        operation_id: OperationId,
        draft: ConnectionDraft,
        timeout: Duration,
    ) -> Result<TestDraftRequest, ServiceError> {
        self.ensure_config_certain()?;
        Ok(TestDraftRequest::without_credential(
            draft_id,
            operation_id,
            draft,
            timeout,
        ))
    }

    pub fn prepare_replacement_draft_test(
        &self,
        draft_id: DraftId,
        operation_id: OperationId,
        draft: ConnectionDraft,
        replacement: &ReplacementSecretBuffer,
        timeout: Duration,
    ) -> Result<TestDraftRequest, ServiceError> {
        self.ensure_config_certain()?;
        validate_connection_draft(&draft)?;
        if draft.credential_mode != CredentialMode::Session {
            return Err(ProfileValidationError::field(ProfileFieldId::CredentialMode).into());
        }
        let secret =
            replacement
                .copy_for_test()
                .map_err(|_| ServiceError::DraftCredentialRequired {
                    draft_id,
                    operation_id,
                    code: PublicCode::SessionCredential,
                })?;
        Ok(TestDraftRequest::with_credential(
            draft_id,
            operation_id,
            draft,
            DraftCredentialSource::SessionReplace(secret),
            timeout,
        ))
    }

    pub async fn prepare_keep_current_draft_test(
        &self,
        profile_id: ProfileId,
        expected_generation: ProfileGeneration,
        draft_id: DraftId,
        operation_id: OperationId,
        draft: ConnectionDraft,
        timeout: Duration,
    ) -> Result<TestDraftRequest, ServiceError> {
        self.ensure_config_certain()?;
        let _permit = self
            .mutation_lane
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| ServiceError::MutationLaneClosed)?;
        self.ensure_config_certain()?;
        self.ensure_generation(&profile_id, expected_generation, operation_id)
            .await?;
        let persisted = self.profile(&profile_id).await?;
        if persisted.credential_mode != CredentialMode::Session
            || draft.credential_mode != CredentialMode::Session
        {
            return Err(ProfileValidationError::field(ProfileFieldId::CredentialMode).into());
        }
        if !same_keep_test_connection(&persisted, &draft) {
            return Err(ServiceError::DraftCredentialRequired {
                draft_id,
                operation_id,
                code: PublicCode::SessionCredential,
            });
        }
        let secret = self.session_secrets.clone_for_profile(&profile_id)?.ok_or(
            ServiceError::DraftCredentialRequired {
                draft_id,
                operation_id,
                code: PublicCode::SessionCredential,
            },
        )?;
        Ok(TestDraftRequest::with_credential(
            draft_id,
            operation_id,
            draft,
            DraftCredentialSource::SessionKeep(secret),
            timeout,
        ))
    }

    pub fn prepare_environment_draft_test(
        &self,
        draft_id: DraftId,
        operation_id: OperationId,
        draft: ConnectionDraft,
        timeout: Duration,
    ) -> Result<TestDraftRequest, ServiceError> {
        self.ensure_config_certain()?;
        validate_connection_draft(&draft)?;
        if draft.credential_mode != CredentialMode::Environment {
            return Err(SecretError::InvalidSessionIntent.into());
        }
        let name = draft
            .secret_env
            .as_deref()
            .ok_or(ServiceError::DraftCredentialRequired {
                draft_id,
                operation_id,
                code: PublicCode::CredentialEnvironmentName,
            })?;
        let secret = self.environment.resolve_environment(name).map_err(|_| {
            ServiceError::DraftCredentialRequired {
                draft_id,
                operation_id,
                code: PublicCode::CredentialEnvironmentName,
            }
        })?;
        Ok(TestDraftRequest::with_credential(
            draft_id,
            operation_id,
            draft,
            DraftCredentialSource::EnvironmentResolved(secret),
            timeout,
        ))
    }

    pub async fn language_for(
        &self,
        profile_id: &ProfileId,
    ) -> Result<QueryLanguage, ServiceError> {
        Ok(self.profile(profile_id).await?.driver.language())
    }

    pub async fn create_profile(
        &self,
        request: CreateProfileRequest,
    ) -> Result<ProfileMutationOutcome, ServiceError> {
        match self.create_profile_inner(request, false).await {
            Ok(outcome) => Ok(outcome.mutation),
            Err(failure) => {
                self.apply_deferred_cleanup(failure.cleanup).await?;
                Err(failure.error)
            }
        }
    }

    pub(crate) async fn create_profile_for_runtime(
        &self,
        request: CreateProfileRequest,
    ) -> Result<RuntimeCreateOutcome, RuntimeMutationFailure> {
        self.create_profile_inner(request, true).await
    }

    async fn create_profile_inner(
        &self,
        request: CreateProfileRequest,
        defer_cleanup: bool,
    ) -> Result<RuntimeCreateOutcome, RuntimeMutationFailure> {
        self.ensure_config_certain()?;
        validate_connection_draft(&request.draft)?;
        validate_create_secret_update(request.draft.credential_mode, &request.secret_update)
            .map_err(|_| invalid_session_intent_error())?;
        let _permit = self
            .mutation_lane
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| ServiceError::MutationLaneClosed)?;
        self.ensure_config_certain()?;
        let CreateProfileRequest {
            draft_id,
            operation_id,
            explicit_id,
            draft,
            secret_update,
            migration_consent,
        } = request;
        let (mutation, requested_id, mut expected_profile) = match explicit_id {
            Some(profile_id) => {
                validate_profile_id(profile_id.as_str())?;
                let profile = ConnectionProfile::from_draft(profile_id.0.clone(), draft);
                (
                    ConfigMutation::Create(profile.clone()),
                    Some(profile_id),
                    profile,
                )
            }
            None => {
                let base_id = slugify_profile_id(&draft.name);
                let profile = ConnectionProfile::from_draft(base_id.clone(), draft);
                (
                    ConfigMutation::CreateAuto {
                        base_id,
                        profile: profile.clone(),
                    },
                    None,
                    profile,
                )
            }
        };
        let outcome = match self.write_config(mutation, migration_consent).await {
            Err(failure)
                if requested_id.is_some()
                    && matches!(
                        &failure.error,
                        ServiceError::Config(ConfigError::ProfileAlreadyExists(_))
                    ) =>
            {
                return Err(ServiceError::ProfileIdConflict {
                    draft_id,
                    operation_id,
                }
                .into());
            }
            Err(failure) => return Err(failure),
            Ok(outcome) => outcome,
        };
        let affected_profile_id = outcome
            .affected_profile_id
            .as_deref()
            .map(|value| ProfileId(value.to_owned()))
            .ok_or(ServiceError::ConfigTaskFailed)?;
        if let Some(requested_id) = requested_id.as_ref()
            && requested_id != &affected_profile_id
        {
            return Err(self
                .runtime_mutation_failure(ServiceError::ConfigUncertain)
                .await);
        }
        expected_profile.id.clone_from(&affected_profile_id.0);
        if observed_profile(&outcome.loaded.config, &affected_profile_id) != Some(&expected_profile)
        {
            return Err(self
                .runtime_mutation_failure(ServiceError::ConfigUncertain)
                .await);
        }
        let profile_id = affected_profile_id;
        let reconciled = match self
            .reconcile_after_mutation(
                outcome.loaded.config.clone(),
                MutationIdentity::Create(&profile_id),
            )
            .await
        {
            Ok(reconciled) => reconciled,
            Err(ServiceError::ConfigUncertain) => {
                return Err(self
                    .runtime_mutation_failure(ServiceError::ConfigUncertain)
                    .await);
            }
            Err(error) => return Err(error.into()),
        };
        let mut cleanup = reconciled.cleanup;
        cleanup
            .secret_updates
            .push((profile_id.clone(), secret_update));
        let cleanup = if defer_cleanup {
            cleanup
        } else {
            self.apply_deferred_cleanup(cleanup).await?;
            DeferredRuntimeCleanup::empty()
        };
        Ok(RuntimeCreateOutcome {
            mutation: ProfileMutationOutcome {
                operation_id,
                profile_id,
                profile_generation: reconciled.profile_generation,
                commit_state: outcome.state,
                migration_backup: outcome.migration_backup,
            },
            cleanup,
        })
    }

    pub async fn update_profile(
        &self,
        request: UpdateProfileRequest,
    ) -> Result<ProfileMutationOutcome, ServiceError> {
        match self
            .update_profile_inner(request, UpdateSessionPolicy::Legacy)
            .await
        {
            Ok(outcome) => Ok(outcome.mutation),
            Err(failure) => {
                self.apply_deferred_cleanup(failure.cleanup).await?;
                Err(failure.error)
            }
        }
    }

    pub(crate) async fn update_profile_for_runtime(
        &self,
        request: UpdateProfileRequest,
    ) -> Result<RuntimeUpdateOutcome, RuntimeMutationFailure> {
        self.update_profile_inner(request, UpdateSessionPolicy::Defer)
            .await
    }

    async fn update_profile_inner(
        &self,
        request: UpdateProfileRequest,
        session_policy: UpdateSessionPolicy,
    ) -> Result<RuntimeUpdateOutcome, RuntimeMutationFailure> {
        self.ensure_config_certain()?;
        validate_connection_draft(&request.draft)?;
        let _permit = self
            .mutation_lane
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| ServiceError::MutationLaneClosed)?;
        self.ensure_config_certain()?;
        self.ensure_generation(
            &request.profile_id,
            request.expected_generation,
            request.operation_id,
        )
        .await?;
        let expected_profile = self.profile(&request.profile_id).await?;
        validate_update_secret(
            &expected_profile,
            request.draft.credential_mode,
            &request.secret_update,
            self.session_secrets.has_current(&request.profile_id)?,
        )
        .map_err(|_| invalid_session_intent_error())?;
        let updated = ConnectionProfile::from_draft(request.profile_id.0.clone(), request.draft);
        let keep_secret = matches!(&request.secret_update, SessionSecretUpdate::Keep);
        let secret_preserves_connection = match updated.credential_mode {
            CredentialMode::Session => keep_secret,
            CredentialMode::None | CredentialMode::Environment => true,
        };
        let connection_fingerprint_unchanged =
            ConnectionFingerprint::from(&expected_profile) == ConnectionFingerprint::from(&updated);
        let retain_idle_session = secret_preserves_connection
            && connection_fingerprint_unchanged
            && expected_profile == updated;
        let retag_eligible = secret_preserves_connection && connection_fingerprint_unchanged;
        let mutation = ConfigMutation::UpdateChecked {
            profile_id: request.profile_id.0.clone(),
            expected_profile,
            profile: updated.clone(),
        };
        let outcome = match self.write_config(mutation, request.migration_consent).await {
            Err(failure)
                if matches!(
                    &failure.error,
                    ServiceError::Config(
                        ConfigError::ProfileMissing(_) | ConfigError::ExternalChange
                    )
                ) =>
            {
                return Err(ServiceError::ProfileStale {
                    profile_id: request.profile_id,
                    operation_id: request.operation_id,
                }
                .into());
            }
            Err(failure) => return Err(failure),
            Ok(outcome) => outcome,
        };
        if outcome.affected_profile_id.as_deref() != Some(request.profile_id.as_str())
            || observed_profile(&outcome.loaded.config, &request.profile_id) != Some(&updated)
        {
            return Err(self
                .runtime_mutation_failure(ServiceError::ConfigUncertain)
                .await);
        }
        let reconciled = match self
            .reconcile_after_mutation(
                outcome.loaded.config.clone(),
                MutationIdentity::Update {
                    profile_id: &request.profile_id,
                    session_policy: match session_policy {
                        UpdateSessionPolicy::Legacy => {
                            LocalSessionPolicy::Resolve(retain_idle_session)
                        }
                        UpdateSessionPolicy::Defer => LocalSessionPolicy::Defer { retag_eligible },
                    },
                },
            )
            .await
        {
            Ok(reconciled) => reconciled,
            Err(ServiceError::ConfigUncertain) => {
                return Err(self
                    .runtime_mutation_failure(ServiceError::ConfigUncertain)
                    .await);
            }
            Err(error) => return Err(error.into()),
        };
        let mut cleanup = reconciled.cleanup;
        cleanup
            .secret_updates
            .push((request.profile_id.clone(), request.secret_update));
        let deferred_session = reconciled.deferred_session;
        let (deferred_session, cleanup) = match session_policy {
            UpdateSessionPolicy::Legacy => {
                if let Some(fence) = deferred_session {
                    self.resolve_deferred_session(fence, true).await;
                }
                self.apply_deferred_cleanup(cleanup).await?;
                (None, DeferredRuntimeCleanup::empty())
            }
            UpdateSessionPolicy::Defer => (deferred_session, cleanup),
        };
        Ok(RuntimeUpdateOutcome {
            mutation: ProfileMutationOutcome {
                operation_id: request.operation_id,
                profile_id: request.profile_id,
                profile_generation: reconciled.profile_generation,
                commit_state: outcome.state,
                migration_backup: outcome.migration_backup,
            },
            deferred_session,
            cleanup,
        })
    }

    pub async fn delete_profile(
        &self,
        request: DeleteProfileRequest,
    ) -> Result<ProfileMutationOutcome, ServiceError> {
        match self.delete_profile_inner(request, false).await {
            Ok(outcome) => Ok(outcome.mutation),
            Err(failure) => {
                self.apply_deferred_cleanup(failure.cleanup).await?;
                Err(failure.error)
            }
        }
    }

    pub(crate) async fn delete_profile_for_runtime(
        &self,
        request: DeleteProfileRequest,
    ) -> Result<RuntimeDeleteOutcome, RuntimeMutationFailure> {
        self.delete_profile_inner(request, true).await
    }

    async fn delete_profile_inner(
        &self,
        request: DeleteProfileRequest,
        defer_cleanup: bool,
    ) -> Result<RuntimeDeleteOutcome, RuntimeMutationFailure> {
        self.ensure_config_certain()?;
        let _permit = self
            .mutation_lane
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| ServiceError::MutationLaneClosed)?;
        self.ensure_config_certain()?;
        self.ensure_generation(
            &request.profile_id,
            request.expected_generation,
            request.operation_id,
        )
        .await?;
        let expected_profile = self.profile(&request.profile_id).await?;
        let mutation = ConfigMutation::DeleteChecked {
            profile_id: request.profile_id.0.clone(),
            expected_profile,
        };
        let outcome = match self.write_config(mutation, request.migration_consent).await {
            Err(failure)
                if matches!(
                    &failure.error,
                    ServiceError::Config(
                        ConfigError::ProfileMissing(_) | ConfigError::ExternalChange
                    )
                ) =>
            {
                return Err(ServiceError::ProfileStale {
                    profile_id: request.profile_id,
                    operation_id: request.operation_id,
                }
                .into());
            }
            Err(failure) => return Err(failure),
            Ok(outcome) => outcome,
        };
        if outcome.affected_profile_id.as_deref() != Some(request.profile_id.as_str())
            || observed_profile(&outcome.loaded.config, &request.profile_id).is_some()
        {
            return Err(self
                .runtime_mutation_failure(ServiceError::ConfigUncertain)
                .await);
        }
        let reconciled = match self
            .reconcile_after_mutation(
                outcome.loaded.config.clone(),
                MutationIdentity::Delete(&request.profile_id),
            )
            .await
        {
            Ok(reconciled) => reconciled,
            Err(ServiceError::ConfigUncertain) => {
                return Err(self
                    .runtime_mutation_failure(ServiceError::ConfigUncertain)
                    .await);
            }
            Err(error) => return Err(error.into()),
        };
        let cleanup = if defer_cleanup {
            reconciled.cleanup
        } else {
            self.apply_deferred_cleanup(reconciled.cleanup).await?;
            DeferredRuntimeCleanup::empty()
        };
        Ok(RuntimeDeleteOutcome {
            mutation: ProfileMutationOutcome {
                operation_id: request.operation_id,
                profile_id: request.profile_id,
                profile_generation: reconciled.profile_generation,
                commit_state: outcome.state,
                migration_backup: outcome.migration_backup,
            },
            cleanup,
        })
    }

    pub async fn test_draft_connection(
        &self,
        request: TestDraftRequest,
    ) -> Result<DraftTestOutcome, ServiceError> {
        let temporary = self.acquire_draft_session(request).await?;
        let ping = temporary.ping().await;
        let close = temporary.close().await;
        self.ensure_config_certain()?;
        ping?;
        close?;
        Ok(DraftTestOutcome {
            draft_id: temporary.draft_id,
            operation_id: temporary.operation_id,
            driver: temporary.profile.driver,
            endpoint: temporary.profile.redacted_endpoint(),
            elapsed_ms: temporary.started.elapsed().as_millis(),
        })
    }

    pub(crate) async fn acquire_draft_session(
        &self,
        request: TestDraftRequest,
    ) -> Result<DraftSessionLease, ServiceError> {
        self.ensure_config_certain()?;
        validate_connection_draft(&request.draft)?;
        let TestDraftRequest {
            draft_id,
            operation_id,
            draft,
            credential_source,
            timeout,
        } = request;
        let secret = match (draft.credential_mode, credential_source) {
            (CredentialMode::None, None) => None,
            (CredentialMode::None, Some(_)) => {
                return Err(SecretError::InvalidSessionIntent.into());
            }
            (
                CredentialMode::Session,
                Some(
                    DraftCredentialSource::SessionKeep(secret)
                    | DraftCredentialSource::SessionReplace(secret),
                ),
            )
            | (
                CredentialMode::Environment,
                Some(DraftCredentialSource::EnvironmentResolved(secret)),
            ) => Some(secret),
            (CredentialMode::Session | CredentialMode::Environment, Some(_)) => {
                return Err(SecretError::InvalidSessionIntent.into());
            }
            (CredentialMode::Session, None) => {
                return Err(ServiceError::DraftCredentialRequired {
                    draft_id,
                    operation_id,
                    code: PublicCode::SessionCredential,
                });
            }
            (CredentialMode::Environment, None) => {
                return Err(ServiceError::DraftCredentialRequired {
                    draft_id,
                    operation_id,
                    code: PublicCode::CredentialEnvironmentName,
                });
            }
        };
        let profile = ConnectionProfile::from_draft(format!("draft-{}", draft_id.0), draft);
        ensure_ready(&profile)?;
        ensure_connector_tls_support(self.connector.as_ref(), &profile)?;
        let started = Instant::now();
        let temporary = self
            .connector
            .connect(&profile, secret.as_deref(), timeout)
            .await?;
        Ok(DraftSessionLease {
            draft_id,
            operation_id,
            profile,
            timeout,
            started,
            handle: temporary,
        })
    }

    pub async fn check_at(
        &self,
        operation_id: OperationId,
        profile_id: ProfileId,
        expected_generation: ProfileGeneration,
        timeout: Duration,
    ) -> Result<CheckOutcome, ServiceError> {
        let started = Instant::now();
        let lease = self
            .acquire_session_at(
                operation_id,
                profile_id.clone(),
                expected_generation,
                timeout,
            )
            .await?;
        let ping = lease.ping(timeout).await;
        let observation = self.observe_session(&lease, operation_id).await;
        if ping.is_err() || observation.is_err() {
            self.evict_session_lease(&lease).await;
        }
        observation?;
        ping?;
        Ok(CheckOutcome {
            operation_id,
            profile_id,
            profile_generation: lease.identity.profile_generation,
            session_generation: lease.identity.session_generation,
            driver: lease.profile.driver,
            endpoint: lease.profile.redacted_endpoint(),
            elapsed_ms: started.elapsed().as_millis(),
        })
    }

    pub(crate) async fn prepare_execute_request(
        &self,
        request: &ExecuteRequest,
    ) -> Result<TypedExecuteRequest, ServiceError> {
        self.ensure_config_certain()?;
        let target = validate_execute_target(request)?;
        let (profile, generation) = self.profile_with_generation(&request.profile_id).await?;
        if generation != request.profile_generation {
            return Err(ServiceError::ProfileStale {
                profile_id: request.profile_id.clone(),
                operation_id: request.operation_id,
            });
        }
        if profile.driver.language() != request.language {
            return Err(ServiceError::LanguageMismatch {
                driver: profile.driver,
                actual: request.language,
            });
        }
        let identity = RequestIdentity::new(
            request.profile_id.clone(),
            request.profile_generation,
            request.operation_id,
        );
        match (profile.driver, target) {
            (DriverKind::MySql, ExecutionTarget::MySqlText(statement)) => {
                let typed = PreparedMySqlRequest {
                    identity,
                    statement,
                    row_limit: request.row_limit,
                    timeout: request.timeout,
                };
                typed.validate().map_err(service_request_error)?;
                Ok(TypedExecuteRequest::MySql(typed))
            }
            (DriverKind::Redis, ExecutionTarget::RedisArgv(argv)) => {
                let typed = RedisExecuteRequest::new(
                    identity,
                    argv.into_iter().map(String::into_bytes).collect(),
                    request.row_limit,
                    request.timeout,
                )
                .map_err(service_request_error)?;
                Ok(TypedExecuteRequest::Redis(typed))
            }
            (actual, _) => Err(ServiceError::Driver(DriverError::Unsupported {
                driver: actual,
                operation: "typed execution".to_owned(),
            })),
        }
    }

    pub(crate) fn retain_execute_result(
        &self,
        request: &TypedExecuteRequest,
        result: QueryResult,
    ) -> ResultSnapshot {
        let duration_ms = result.elapsed_ms;
        let completed_at_unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|duration| i64::try_from(duration.as_millis()).ok())
            .unwrap_or(i64::MAX);
        let identity = request.identity();
        let provenance = ResultProvenance {
            result_id: ResultId(self.next_result_id.fetch_add(1, Ordering::SeqCst)),
            profile_id: identity.profile_id.clone(),
            profile_generation: identity.profile_generation,
            operation_id: identity.operation_id,
            driver: request.driver(),
            completed_at_unix_ms,
            duration_ms,
        };
        ResultSnapshot::retain(result, provenance, request.retention_policy())
    }

    pub async fn execute_at(
        &self,
        request: ExecuteRequest,
    ) -> Result<ExecuteOutcome, ServiceError> {
        let typed_request = self.prepare_execute_request(&request).await?;
        let lease = self
            .acquire_session_at(
                request.operation_id,
                request.profile_id.clone(),
                request.profile_generation,
                request.timeout,
            )
            .await?;
        let result = lease.execute_typed(&typed_request).await;
        let observation = self.observe_session(&lease, request.operation_id).await;
        let disposition = result.as_ref().err().map_or(
            SessionDisposition::Keep,
            SessionDisposition::for_driver_error,
        );
        if disposition == SessionDisposition::Evict || observation.is_err() {
            self.evict_session_lease(&lease).await;
        }
        observation?;
        let result = self.retain_execute_result(&typed_request, result?);
        Ok(ExecuteOutcome {
            operation_id: request.operation_id,
            profile_id: request.profile_id,
            profile_generation: lease.identity.profile_generation,
            session_generation: lease.identity.session_generation,
            driver: lease.profile.driver,
            endpoint: lease.profile.redacted_endpoint(),
            result,
        })
    }

    pub async fn load_catalog_page(
        &self,
        request: CatalogRequest,
    ) -> Result<CatalogPage, ServiceError> {
        self.prepare_catalog_request(&request).await?;
        let lease = self
            .acquire_session_at(
                request.operation_id(),
                request.profile_id().clone(),
                request.profile_generation(),
                request.timeout(),
            )
            .await?;
        let result = lease
            .load_catalog_page(&request, self.catalog_token_key())
            .await;
        self.finish_resource_operation(&lease, request.operation_id(), result)
            .await
    }

    pub(crate) async fn prepare_catalog_request(
        &self,
        request: &CatalogRequest,
    ) -> Result<(), ServiceError> {
        request.validate().map_err(service_request_error)?;
        self.ensure_typed_resource_request(
            request.identity(),
            DriverKind::MySql,
            DriverCapabilities::CATALOG,
            "mysql catalog browsing",
        )
        .await
    }

    pub(crate) fn catalog_token_key(&self) -> &CatalogTokenKey {
        self.catalog_token_key.as_ref()
    }

    pub async fn scan_redis_keys(
        &self,
        request: RedisScanRequest,
    ) -> Result<RedisKeyPage, ServiceError> {
        request.validate().map_err(service_request_error)?;
        self.ensure_typed_resource_request(
            request.identity(),
            DriverKind::Redis,
            DriverCapabilities::KEYSPACE_BROWSE,
            "redis keyspace browsing",
        )
        .await?;
        let lease = self
            .acquire_session_at(
                request.operation_id(),
                request.profile_id().clone(),
                request.profile_generation(),
                request.timeout,
            )
            .await?;
        let result = match lease.connected_resources() {
            Ok(ConnectedResources::Redis { keyspace, .. }) => keyspace.scan_keys(&request).await,
            Ok(_) => Err(DriverError::Unsupported {
                driver: lease.profile.driver,
                operation: "mismatched keyspace resource".to_owned(),
            }),
            Err(error) => Err(error),
        };
        self.finish_resource_operation(&lease, request.operation_id(), result)
            .await
    }

    pub async fn inspect_redis_key(
        &self,
        request: RedisKeyInspectRequest,
    ) -> Result<RedisValuePreview, ServiceError> {
        request.validate().map_err(service_request_error)?;
        self.ensure_typed_resource_request(
            request.identity(),
            DriverKind::Redis,
            DriverCapabilities::KEYSPACE_BROWSE,
            "redis key inspection",
        )
        .await?;
        let lease = self
            .acquire_session_at(
                request.operation_id(),
                request.profile_id().clone(),
                request.profile_generation(),
                request.timeout,
            )
            .await?;
        let result = match lease.connected_resources() {
            Ok(ConnectedResources::Redis { keyspace, .. }) => keyspace.inspect_key(&request).await,
            Ok(_) => Err(DriverError::Unsupported {
                driver: lease.profile.driver,
                operation: "mismatched key inspection resource".to_owned(),
            }),
            Err(error) => Err(error),
        };
        self.finish_resource_operation(&lease, request.operation_id(), result)
            .await
    }

    async fn ensure_typed_resource_request(
        &self,
        identity: &RequestIdentity,
        expected_driver: DriverKind,
        capability: DriverCapabilities,
        operation: &'static str,
    ) -> Result<(), ServiceError> {
        self.ensure_config_certain()?;
        let (profile, generation) = self.profile_with_generation(&identity.profile_id).await?;
        if generation != identity.profile_generation {
            return Err(ServiceError::ProfileStale {
                profile_id: identity.profile_id.clone(),
                operation_id: identity.operation_id,
            });
        }
        if profile.driver != expected_driver {
            return Err(ServiceError::DriverMismatch {
                expected: expected_driver,
                actual: profile.driver,
            });
        }
        let ready = crate::drivers::descriptors()
            .into_iter()
            .find(|descriptor| descriptor.kind == profile.driver)
            .is_some_and(|descriptor| descriptor.capabilities.contains(capability));
        if !ready {
            return Err(DriverError::Unsupported {
                driver: profile.driver,
                operation: operation.to_owned(),
            }
            .into());
        }
        Ok(())
    }

    async fn finish_resource_operation<T>(
        &self,
        lease: &SessionLease,
        operation_id: OperationId,
        result: Result<T, DriverError>,
    ) -> Result<T, ServiceError> {
        let observation = self.observe_session(lease, operation_id).await;
        let disposition = result.as_ref().err().map_or(
            SessionDisposition::Keep,
            SessionDisposition::for_driver_error,
        );
        if disposition == SessionDisposition::Evict || observation.is_err() {
            self.evict_session_lease(lease).await;
        }
        observation?;
        result.map_err(ServiceError::from)
    }

    pub(crate) async fn acquire_session_at(
        &self,
        operation_id: OperationId,
        profile_id: ProfileId,
        expected_generation: ProfileGeneration,
        timeout: Duration,
    ) -> Result<SessionLease, ServiceError> {
        self.ensure_config_certain()?;
        let (profile, generation) = self.profile_with_generation(&profile_id).await?;
        if generation != expected_generation {
            return Err(ServiceError::ProfileStale {
                profile_id,
                operation_id,
            });
        }
        validate_profile_for_network(&profile)?;
        self.session_for(&profile, generation, operation_id, timeout)
            .await
    }

    pub(crate) async fn observe_session(
        &self,
        lease: &SessionLease,
        operation_id: OperationId,
    ) -> Result<(), ServiceError> {
        self.ensure_session_observation(
            &lease.profile_id,
            lease.identity.profile_generation,
            operation_id,
        )
        .await
    }

    pub(crate) async fn evict_session_lease(&self, lease: &SessionLease) -> bool {
        self.evict_cached_session_exact(
            &lease.profile_id,
            lease.identity.profile_generation,
            lease.identity.session_generation,
        )
        .await
    }

    async fn write_config(
        &self,
        mutation: ConfigMutation,
        consent: MigrationConsent,
    ) -> Result<ObservedMutationOutcome, RuntimeMutationFailure> {
        let writer = self.writer.clone();
        let path = self.config_path.as_ref().clone();
        let outcome =
            tokio::task::spawn_blocking(move || writer.mutate_path(&path, mutation, consent))
                .await
                .map_err(|_| ServiceError::ConfigTaskFailed)?
                .map_err(ServiceError::Config)?;
        let MutationOutcome {
            state,
            observation,
            migration_backup,
            affected_profile_id,
        } = outcome;
        match observation {
            PostCommitObservation::Observed(loaded) => Ok(ObservedMutationOutcome {
                state,
                loaded,
                migration_backup,
                affected_profile_id,
            }),
            PostCommitObservation::Failed(error) => {
                let cleanup = self.enter_config_uncertain_deferred().await;
                Err(RuntimeMutationFailure {
                    error: ServiceError::PostCommitObservation(error),
                    cleanup,
                })
            }
        }
    }

    fn ensure_config_certain(&self) -> Result<(), ServiceError> {
        if self.is_config_uncertain() {
            Err(ServiceError::ConfigUncertain)
        } else {
            Ok(())
        }
    }

    async fn runtime_mutation_failure(&self, error: ServiceError) -> RuntimeMutationFailure {
        RuntimeMutationFailure {
            error,
            cleanup: self.enter_config_uncertain_deferred().await,
        }
    }

    async fn enter_config_uncertain_deferred(&self) -> DeferredRuntimeCleanup {
        self.config_uncertain.store(true, Ordering::Release);
        let targets = {
            let mut state = self.state.write().await;
            let targets = state
                .observed
                .generations
                .iter()
                .map(|(profile_id, generation)| DeferredCleanupTarget {
                    profile_id: profile_id.clone(),
                    previous_generation: *generation,
                    session: state.sessions.get(profile_id).map(CachedSession::identity),
                    clear_secret: true,
                })
                .collect::<Vec<_>>();
            for generation in state.observed.generations.values_mut() {
                *generation = self.allocate_profile_generation();
            }
            targets
        };
        DeferredRuntimeCleanup {
            targets,
            secret_updates: Vec::new(),
            clear_all_secrets: true,
            retain_secret_profiles: None,
        }
    }

    pub async fn evict_cached_session_exact(
        &self,
        profile_id: &ProfileId,
        profile_generation: ProfileGeneration,
        session_generation: SessionGeneration,
    ) -> bool {
        let removed = self
            .take_cached_session_exact(profile_id, profile_generation, session_generation)
            .await;
        if let Some(removed) = removed {
            let _ = removed.close().await;
            true
        } else {
            false
        }
    }

    pub(crate) async fn take_cached_session_exact(
        &self,
        profile_id: &ProfileId,
        profile_generation: ProfileGeneration,
        session_generation: SessionGeneration,
    ) -> Option<Arc<dyn SessionHandle>> {
        {
            let mut state = self.state.write().await;
            let remove_exact = state.sessions.get(profile_id).is_some_and(|cached| {
                cached.profile_generation == profile_generation
                    && cached.session_generation == session_generation
            });
            if remove_exact {
                state
                    .sessions
                    .remove(profile_id)
                    .map(|cached| cached.handle)
            } else {
                None
            }
        }
    }

    pub async fn evict_cached_session_profile_generation(
        &self,
        profile_id: &ProfileId,
        profile_generation: ProfileGeneration,
    ) -> bool {
        let identity = self.cached_session_identity(profile_id).await;
        match identity {
            Some(identity) if identity.profile_generation == profile_generation => {
                self.evict_cached_session_exact(
                    profile_id,
                    profile_generation,
                    identity.session_generation,
                )
                .await
            }
            Some(_) | None => false,
        }
    }

    pub(crate) async fn retag_cached_session_exact(
        &self,
        profile_id: &ProfileId,
        expected: &CachedSessionIdentity,
        new_profile_generation: ProfileGeneration,
        new_fingerprint: &ConnectionFingerprint,
    ) -> bool {
        let mut state = self.state.write().await;
        let Some(cached) = state.sessions.get_mut(profile_id) else {
            return false;
        };
        if cached.identity() != *expected || cached.connection_fingerprint != *new_fingerprint {
            return false;
        }
        cached.profile_generation = new_profile_generation;
        true
    }

    pub(crate) async fn resolve_deferred_session(
        &self,
        fence: DeferredSessionFence,
        allow_retag: bool,
    ) -> bool {
        if allow_retag && fence.retag_eligible {
            self.retag_cached_session_exact(
                &fence.profile_id,
                &fence.previous,
                fence.next_profile_generation,
                &fence.next_fingerprint,
            )
            .await
        } else {
            self.evict_cached_session_exact(
                &fence.profile_id,
                fence.previous.profile_generation,
                fence.previous.session_generation,
            )
            .await;
            false
        }
    }

    pub(crate) async fn apply_deferred_cleanup(
        &self,
        cleanup: DeferredRuntimeCleanup,
    ) -> Result<(), ServiceError> {
        for target in &cleanup.targets {
            if let Some(identity) = &target.session {
                self.evict_cached_session_exact(
                    &target.profile_id,
                    identity.profile_generation,
                    identity.session_generation,
                )
                .await;
            }
        }
        for target in &cleanup.targets {
            if target.clear_secret {
                self.session_secrets
                    .apply(&target.profile_id, SessionSecretUpdate::Clear)?;
            }
        }
        if cleanup.clear_all_secrets {
            self.session_secrets.clear_all()?;
            return Ok(());
        }
        if let Some(profile_ids) = &cleanup.retain_secret_profiles {
            self.session_secrets.retain_profiles(profile_ids)?;
        }
        for (profile_id, update) in cleanup.secret_updates {
            self.session_secrets.apply(&profile_id, update)?;
        }
        Ok(())
    }

    async fn replace_loaded_config(&self, loaded: LoadedConfig) -> DeferredRuntimeCleanup {
        let (previous_profiles, previous_generations, mut tombstones, previous_sessions) = {
            let state = self.state.read().await;
            let observed = &state.observed;
            (
                observed
                    .config
                    .profiles
                    .iter()
                    .cloned()
                    .map(|profile| (ProfileId(profile.id.clone()), profile))
                    .collect::<HashMap<_, _>>(),
                observed.generations.clone(),
                observed.tombstones.clone(),
                state
                    .sessions
                    .iter()
                    .map(|(profile_id, cached)| (profile_id.clone(), cached.identity()))
                    .collect::<HashMap<_, _>>(),
            )
        };
        let next_profiles: HashMap<ProfileId, ConnectionProfile> = loaded
            .config
            .profiles
            .iter()
            .cloned()
            .map(|profile| (ProfileId(profile.id.clone()), profile))
            .collect();
        let unchanged: HashSet<ProfileId> = next_profiles
            .iter()
            .filter(|(profile_id, profile)| previous_profiles.get(*profile_id) == Some(*profile))
            .map(|(profile_id, _)| profile_id.clone())
            .collect();
        let mut generations = HashMap::new();
        for profile in &loaded.config.profiles {
            let profile_id = ProfileId(profile.id.clone());
            let generation = if unchanged.contains(&profile_id) {
                previous_generations
                    .get(&profile_id)
                    .copied()
                    .unwrap_or_else(|| self.allocate_profile_generation())
            } else {
                self.allocate_profile_generation()
            };
            generations.insert(profile_id, generation);
        }
        for profile_id in previous_profiles.keys() {
            if !next_profiles.contains_key(profile_id) {
                tombstones.insert(profile_id.clone(), self.allocate_profile_generation());
            }
        }
        {
            let mut state = self.state.write().await;
            state.observed = ObservedState {
                config: loaded.config,
                source_version: loaded.source_version,
                generations,
                tombstones,
            };
        }
        let targets = previous_profiles
            .iter()
            .filter(|(profile_id, previous)| next_profiles.get(*profile_id) != Some(*previous))
            .filter_map(|(profile_id, _)| {
                previous_generations
                    .get(profile_id)
                    .copied()
                    .map(|previous_generation| DeferredCleanupTarget {
                        profile_id: profile_id.clone(),
                        previous_generation,
                        session: previous_sessions.get(profile_id).cloned(),
                        clear_secret: true,
                    })
            })
            .collect();
        DeferredRuntimeCleanup {
            targets,
            secret_updates: Vec::new(),
            clear_all_secrets: false,
            retain_secret_profiles: Some(next_profiles.keys().cloned().collect()),
        }
    }

    async fn reconcile_after_mutation(
        &self,
        config: Config,
        identity: MutationIdentity<'_>,
    ) -> Result<ReconcileMutationOutcome, ServiceError> {
        let (previous_profiles, previous_generations, mut tombstones, previous_sessions) = {
            let state = self.state.read().await;
            let observed = &state.observed;
            (
                observed
                    .config
                    .profiles
                    .iter()
                    .cloned()
                    .map(|profile| (ProfileId(profile.id.clone()), profile))
                    .collect::<HashMap<_, _>>(),
                observed.generations.clone(),
                observed.tombstones.clone(),
                state
                    .sessions
                    .iter()
                    .map(|(profile_id, cached)| (profile_id.clone(), cached.identity()))
                    .collect::<HashMap<_, _>>(),
            )
        };
        let next_profiles: HashMap<ProfileId, ConnectionProfile> = config
            .profiles
            .iter()
            .cloned()
            .map(|profile| (ProfileId(profile.id.clone()), profile))
            .collect();
        let local_profile_id = identity.profile_id();
        let mut generations = HashMap::with_capacity(config.profiles.len());
        let mut affected_generation = None;
        for profile in &config.profiles {
            let profile_id = ProfileId(profile.id.clone());
            let is_local_target = &profile_id == local_profile_id;
            let force_local_generation = is_local_target && !identity.is_delete();
            let generation = if force_local_generation {
                self.allocate_profile_generation()
            } else if previous_profiles.get(&profile_id) == Some(profile) {
                previous_generations
                    .get(&profile_id)
                    .copied()
                    .unwrap_or_else(|| self.allocate_profile_generation())
            } else {
                self.allocate_profile_generation()
            };
            if is_local_target {
                affected_generation = Some(generation);
            }
            generations.insert(profile_id, generation);
        }
        if identity.is_delete() {
            let deletion_generation = self.allocate_profile_generation();
            tombstones.insert(local_profile_id.clone(), deletion_generation);
            affected_generation = Some(deletion_generation);
        }
        let mut deferred_session = None;
        let mut cleanup = DeferredRuntimeCleanup::empty();
        cleanup.retain_secret_profiles = Some(next_profiles.keys().cloned().collect());
        for (profile_id, previous_profile) in &previous_profiles {
            let is_local_target = profile_id == local_profile_id;
            let profile_changed = next_profiles.get(profile_id) != Some(previous_profile);
            let local_generation_changed =
                is_local_target && !matches!(identity, MutationIdentity::Create(_));
            if !profile_changed && !local_generation_changed {
                continue;
            }
            let previous_generation = previous_generations
                .get(profile_id)
                .copied()
                .ok_or(ServiceError::ConfigUncertain)?;
            let mut session = previous_sessions.get(profile_id).cloned();
            if is_local_target
                && let Some(retag_eligible) = identity.deferred_retag_eligibility()
                && let (Some(previous), Some(next_profile_generation), Some(next_profile)) = (
                    session.take(),
                    generations.get(profile_id),
                    next_profiles.get(profile_id),
                )
            {
                deferred_session = Some(DeferredSessionFence {
                    profile_id: profile_id.clone(),
                    previous,
                    next_profile_generation: *next_profile_generation,
                    next_fingerprint: ConnectionFingerprint::from(next_profile),
                    retag_eligible,
                });
            }
            cleanup.targets.push(DeferredCleanupTarget {
                profile_id: profile_id.clone(),
                previous_generation,
                session,
                clear_secret: !is_local_target || identity.is_delete(),
            });
        }
        let profile_generation = affected_generation.ok_or(ServiceError::ConfigUncertain)?;
        {
            let mut state = self.state.write().await;
            state.observed = ObservedState {
                config,
                source_version: ConfigSourceVersion::V2,
                generations,
                tombstones,
            };
        }
        Ok(ReconcileMutationOutcome {
            profile_generation,
            deferred_session,
            cleanup,
        })
    }

    async fn ensure_generation(
        &self,
        profile_id: &ProfileId,
        expected: ProfileGeneration,
        operation_id: OperationId,
    ) -> Result<(), ServiceError> {
        if self
            .state
            .read()
            .await
            .observed
            .generations
            .get(profile_id)
            .copied()
            == Some(expected)
        {
            Ok(())
        } else {
            Err(ServiceError::ProfileStale {
                profile_id: profile_id.clone(),
                operation_id,
            })
        }
    }

    pub(crate) async fn ensure_profile_generation(
        &self,
        profile_id: &ProfileId,
        expected: ProfileGeneration,
        operation_id: OperationId,
    ) -> Result<(), ServiceError> {
        self.ensure_generation(profile_id, expected, operation_id)
            .await
    }

    async fn profile(&self, profile_id: &ProfileId) -> Result<ConnectionProfile, ServiceError> {
        self.state
            .read()
            .await
            .observed
            .config
            .profiles
            .iter()
            .find(|profile| profile.id == profile_id.as_str())
            .cloned()
            .ok_or_else(|| ServiceError::UnknownProfile(profile_id.clone()))
    }

    async fn profile_with_generation(
        &self,
        profile_id: &ProfileId,
    ) -> Result<(ConnectionProfile, ProfileGeneration), ServiceError> {
        let state = self.state.read().await;
        let observed = &state.observed;
        let profile = observed
            .config
            .profiles
            .iter()
            .find(|profile| profile.id == profile_id.as_str())
            .cloned()
            .ok_or_else(|| ServiceError::UnknownProfile(profile_id.clone()))?;
        let generation = observed
            .generations
            .get(profile_id)
            .copied()
            .ok_or_else(|| ServiceError::UnknownProfile(profile_id.clone()))?;
        Ok((profile, generation))
    }

    async fn ensure_session_observation(
        &self,
        profile_id: &ProfileId,
        generation: ProfileGeneration,
        operation_id: OperationId,
    ) -> Result<(), ServiceError> {
        let state = self.state.read().await;
        let observed = &state.observed;
        if self.is_config_uncertain() {
            Err(ServiceError::ConfigUncertain)
        } else if observed.generations.get(profile_id).copied() == Some(generation) {
            Ok(())
        } else {
            Err(ServiceError::ProfileStale {
                profile_id: profile_id.clone(),
                operation_id,
            })
        }
    }

    async fn session_for(
        &self,
        profile: &ConnectionProfile,
        generation: ProfileGeneration,
        operation_id: OperationId,
        timeout: Duration,
    ) -> Result<SessionLease, ServiceError> {
        self.ensure_config_certain()?;
        let profile_id = ProfileId(profile.id.clone());
        let fingerprint = ConnectionFingerprint::from(profile);
        {
            let state = self.state.read().await;
            let observed = &state.observed;
            let is_current = state.observed.generations.get(&profile_id).copied()
                == Some(generation)
                && observed
                    .config
                    .profiles
                    .iter()
                    .any(|current| current == profile);
            if self.is_config_uncertain() {
                return Err(ServiceError::ConfigUncertain);
            }
            if !is_current {
                return Err(ServiceError::ProfileStale {
                    profile_id,
                    operation_id,
                });
            }
            if let Some(cached) = state.sessions.get(&profile_id)
                && cached.connection_fingerprint == fingerprint
                && cached.profile_generation == generation
            {
                return Ok(SessionLease {
                    profile_id,
                    profile: profile.clone(),
                    identity: cached.identity(),
                    handle: cached.handle.clone(),
                });
            }
        }
        ensure_ready(profile)?;
        ensure_connector_tls_support(self.connector.as_ref(), profile)?;
        let secret = self.resolve_profile_secret(profile)?;
        let attempted_session_generation = self.allocate_session_generation();
        let connected = match self
            .connector
            .connect(profile, secret.as_deref(), timeout)
            .await
        {
            Ok(connected) => connected,
            Err(error) => {
                self.ensure_session_observation(&profile_id, generation, operation_id)
                    .await?;
                return Err(error.into());
            }
        };
        enum CacheInstall {
            Installed(Option<Arc<dyn SessionHandle>>, CachedSessionIdentity),
            Existing(Arc<dyn SessionHandle>, CachedSessionIdentity),
            Stale,
            Uncertain,
        }
        let install = {
            let mut state = self.state.write().await;
            let is_current = state.observed.generations.get(&profile_id).copied()
                == Some(generation)
                && state
                    .observed
                    .config
                    .profiles
                    .iter()
                    .any(|current| current == profile);
            if self.is_config_uncertain() {
                CacheInstall::Uncertain
            } else if !is_current {
                CacheInstall::Stale
            } else if let Some(cached) = state.sessions.get(&profile_id)
                && cached.connection_fingerprint == fingerprint
                && cached.profile_generation == generation
            {
                CacheInstall::Existing(cached.handle.clone(), cached.identity())
            } else {
                let identity = CachedSessionIdentity {
                    profile_generation: generation,
                    session_generation: attempted_session_generation,
                    connection_fingerprint: fingerprint.clone(),
                };
                let displaced = state
                    .sessions
                    .insert(
                        profile_id.clone(),
                        CachedSession {
                            profile_generation: generation,
                            session_generation: attempted_session_generation,
                            connection_fingerprint: fingerprint.clone(),
                            handle: connected.clone(),
                        },
                    )
                    .map(|cached| cached.handle);
                CacheInstall::Installed(displaced, identity)
            }
        };
        match install {
            CacheInstall::Installed(displaced, identity) => {
                if let Some(displaced) = displaced {
                    let _ = displaced.close().await;
                }
                Ok(SessionLease {
                    profile_id,
                    profile: profile.clone(),
                    identity,
                    handle: connected,
                })
            }
            CacheInstall::Existing(existing, identity) => {
                let _ = connected.close().await;
                Ok(SessionLease {
                    profile_id,
                    profile: profile.clone(),
                    identity,
                    handle: existing,
                })
            }
            CacheInstall::Stale => {
                let _ = connected.close().await;
                Err(ServiceError::ProfileStale {
                    profile_id,
                    operation_id,
                })
            }
            CacheInstall::Uncertain => {
                let _ = connected.close().await;
                Err(ServiceError::ConfigUncertain)
            }
        }
    }

    fn allocate_profile_generation(&self) -> ProfileGeneration {
        ProfileGeneration(self.next_generation.fetch_add(1, Ordering::SeqCst))
    }

    fn allocate_session_generation(&self) -> SessionGeneration {
        SessionGeneration(self.next_session_generation.fetch_add(1, Ordering::SeqCst))
    }

    fn resolve_profile_secret(
        &self,
        profile: &ConnectionProfile,
    ) -> Result<Option<Arc<SessionSecret>>, ServiceError> {
        match profile.credential_mode {
            CredentialMode::None => Ok(None),
            CredentialMode::Environment => {
                let name = profile.secret_env.as_deref().ok_or_else(|| {
                    ProfileValidationError::field(ProfileFieldId::CredentialEnvironmentName)
                })?;
                self.environment
                    .resolve_environment(name)
                    .map(Some)
                    .map_err(ServiceError::Secret)
            }
            CredentialMode::Session => self
                .session_secrets
                .clone_for_profile(&ProfileId(profile.id.clone()))?
                .map(Some)
                .ok_or_else(|| SecretError::SessionCredentialRequired.into()),
        }
    }
}

pub(crate) enum TypedExecuteRequest {
    MySql(PreparedMySqlRequest),
    Redis(RedisExecuteRequest),
}

impl TypedExecuteRequest {
    fn identity(&self) -> &RequestIdentity {
        match self {
            Self::MySql(request) => request.identity(),
            Self::Redis(request) => request.identity(),
        }
    }

    const fn driver(&self) -> DriverKind {
        match self {
            Self::MySql(_) => DriverKind::MySql,
            Self::Redis(_) => DriverKind::Redis,
        }
    }

    const fn retention_policy(&self) -> ResultRetentionPolicy {
        match self {
            Self::MySql(request) => ResultRetentionPolicy::mysql(request.row_limit as usize),
            Self::Redis(request) => ResultRetentionPolicy::redis(request.row_limit() as usize),
        }
    }
}

fn validate_execute_target(request: &ExecuteRequest) -> Result<ExecutionTarget, ServiceError> {
    let language = match request.language {
        QueryLanguage::Sql => ExecutionLanguage::MySql,
        QueryLanguage::RedisCommand => ExecutionLanguage::Redis,
        QueryLanguage::MongoDocument => {
            return Err(ServiceError::Driver(DriverError::Unsupported {
                driver: DriverKind::MongoDb,
                operation: "document execution".to_owned(),
            }));
        }
    };
    let timeout_seconds =
        u32::try_from(request.timeout.as_secs()).map_err(|_| ServiceError::InvalidRequest {
            code: PublicCode::TimeoutInput,
        })?;
    let character_count = request.text.chars().count();
    extract_and_validate_target(
        &request.text,
        character_count,
        Some(0..character_count),
        language,
        request.row_limit,
        timeout_seconds,
    )
    .map(ValidatedExecutionTarget::into_target)
    .map_err(execution_target_error)
}

fn execution_target_error(error: ExecutionTargetError) -> ServiceError {
    let code = match error {
        ExecutionTargetError::InvalidRowLimit => PublicCode::RowLimit,
        ExecutionTargetError::InvalidTimeout => PublicCode::TimeoutInput,
        ExecutionTargetError::AmbiguousSqlMode => PublicCode::AmbiguousSqlMode,
        ExecutionTargetError::UnterminatedSqlToken => PublicCode::UnterminatedSqlToken,
        ExecutionTargetError::InvalidCaretPosition
        | ExecutionTargetError::InvalidSelectionRange
        | ExecutionTargetError::NoCurrentStatement
        | ExecutionTargetError::MultipleStatements
        | ExecutionTargetError::RedisShellParseFailed
        | ExecutionTargetError::RedisCommandDenied
        | ExecutionTargetError::RedisTargetTooLarge
        | ExecutionTargetError::RedisTooManyTokens
        | ExecutionTargetError::RedisTokenTooLarge => PublicCode::StatementTarget,
    };
    ServiceError::InvalidRequest { code }
}

fn service_request_error(error: RequestValidationError) -> ServiceError {
    let code = match error {
        RequestValidationError::InvalidRowLimit => PublicCode::RowLimit,
        RequestValidationError::InvalidExecuteTimeout => PublicCode::TimeoutInput,
        RequestValidationError::InvalidCatalogPageSize
        | RequestValidationError::InvalidCatalogTimeout
        | RequestValidationError::EmptyCatalogParent
        | RequestValidationError::EmptyCatalogPageToken => PublicCode::Catalog,
        RequestValidationError::RedisFilterTooLarge
        | RequestValidationError::InvalidRedisScanCount
        | RequestValidationError::RedisKeyTooLarge => PublicCode::RedisScan,
        RequestValidationError::EmptyStatement
        | RequestValidationError::EmptyRedisCommand
        | RequestValidationError::RedisCommandTooLarge
        | RequestValidationError::TooManyRedisTokens
        | RequestValidationError::RedisTokenTooLarge
        | RequestValidationError::RedisCommandDenied => PublicCode::StatementTarget,
    };
    ServiceError::InvalidRequest { code }
}

enum MutationIdentity<'a> {
    Create(&'a ProfileId),
    Update {
        profile_id: &'a ProfileId,
        session_policy: LocalSessionPolicy,
    },
    Delete(&'a ProfileId),
}

impl MutationIdentity<'_> {
    fn profile_id(&self) -> &ProfileId {
        match self {
            Self::Create(profile_id)
            | Self::Update { profile_id, .. }
            | Self::Delete(profile_id) => profile_id,
        }
    }

    fn deferred_retag_eligibility(&self) -> Option<bool> {
        match self {
            Self::Update {
                session_policy: LocalSessionPolicy::Resolve(retag_eligible),
                ..
            }
            | Self::Update {
                session_policy: LocalSessionPolicy::Defer { retag_eligible },
                ..
            } => Some(*retag_eligible),
            Self::Create(_) | Self::Delete(_) => None,
        }
    }

    fn is_delete(&self) -> bool {
        matches!(self, Self::Delete(_))
    }
}

#[derive(Clone, Copy)]
enum UpdateSessionPolicy {
    Legacy,
    Defer,
}

#[derive(Clone, Copy)]
enum LocalSessionPolicy {
    Resolve(bool),
    Defer { retag_eligible: bool },
}

struct ReconcileMutationOutcome {
    profile_generation: ProfileGeneration,
    deferred_session: Option<DeferredSessionFence>,
    cleanup: DeferredRuntimeCleanup,
}

pub fn validate_connection_draft(draft: &ConnectionDraft) -> Result<(), ProfileValidationError> {
    if draft.name.trim().is_empty() {
        return Err(ProfileValidationError::field(ProfileFieldId::DisplayName));
    }
    if draft.host.trim().is_empty() {
        return Err(ProfileValidationError::field(ProfileFieldId::Host));
    }
    if draft.port == 0 {
        return Err(ProfileValidationError::field(ProfileFieldId::Port));
    }
    match draft.credential_mode {
        CredentialMode::None | CredentialMode::Session => {
            if draft.secret_env.is_some() {
                return Err(ProfileValidationError::field(
                    ProfileFieldId::CredentialEnvironmentName,
                ));
            }
        }
        CredentialMode::Environment => {
            let name = draft.secret_env.as_deref().ok_or_else(|| {
                ProfileValidationError::field(ProfileFieldId::CredentialEnvironmentName)
            })?;
            if !valid_env_name(name) {
                return Err(ProfileValidationError::field(
                    ProfileFieldId::CredentialEnvironmentName,
                ));
            }
        }
    }
    if draft.driver == DriverKind::Redis {
        if let Some(database) = draft.database.as_deref()
            && database.parse::<u32>().is_err()
        {
            return Err(ProfileValidationError::field(ProfileFieldId::Database));
        }
        match draft.tls {
            TlsMode::Preferred => {
                return Err(ProfileValidationError::Field {
                    field: ProfileFieldId::RedisTlsMode,
                    code: PublicCode::RedisTlsPreferredLegacy,
                });
            }
            TlsMode::Disabled if draft.redis_tls.ca_file.is_some() => {
                return Err(ProfileValidationError::field(ProfileFieldId::RedisCaFile));
            }
            TlsMode::Required => {
                if let Some(ca_file) = draft.redis_tls.ca_file.as_deref() {
                    validate_ca_file(ca_file)?;
                }
            }
            TlsMode::Disabled => {}
        }
    } else if !draft.redis_tls.is_empty() {
        return Err(ProfileValidationError::field(ProfileFieldId::RedisCaFile));
    }
    Ok(())
}

pub fn validate_persisted_profile(
    profile: &ConnectionProfile,
) -> Result<(), ProfileValidationError> {
    validate_profile_id(&profile.id)?;
    let mut draft = profile.as_draft();
    if profile.driver == DriverKind::Redis {
        match profile.tls {
            TlsMode::Preferred => {
                draft.tls = TlsMode::Disabled;
                draft.redis_tls.ca_file = None;
            }
            TlsMode::Required => draft.redis_tls.ca_file = None,
            TlsMode::Disabled => {}
        }
    }
    validate_connection_draft(&draft)
}

pub fn validate_config_identity(config: &Config) -> Result<(), ProfileValidationError> {
    let mut profile_ids = HashSet::with_capacity(config.profiles.len());
    for profile in &config.profiles {
        validate_profile_id(&profile.id)?;
        if !profile_ids.insert(profile.id.as_str()) {
            return Err(ProfileValidationError::field(ProfileFieldId::ConnectionId));
        }
    }
    Ok(())
}

pub fn validate_config_mutation(mutation: &ConfigMutation) -> Result<(), ProfileValidationError> {
    match mutation {
        ConfigMutation::Create(profile) => validate_strict_persisted_profile(profile),
        ConfigMutation::CreateAuto { base_id, profile } => {
            validate_profile_id(base_id)?;
            validate_strict_persisted_profile(profile)
        }
        ConfigMutation::UpdateChecked {
            profile_id,
            expected_profile,
            profile,
        } => {
            validate_profile_id(&expected_profile.id)?;
            if expected_profile.id != *profile_id || profile.id != *profile_id {
                return Err(ProfileValidationError::field(ProfileFieldId::ConnectionId));
            }
            validate_strict_persisted_profile(profile)
        }
        ConfigMutation::DeleteChecked {
            profile_id,
            expected_profile,
        } => {
            validate_profile_id(&expected_profile.id)?;
            if expected_profile.id != *profile_id {
                return Err(ProfileValidationError::field(ProfileFieldId::ConnectionId));
            }
            Ok(())
        }
    }
}

fn validate_strict_persisted_profile(
    profile: &ConnectionProfile,
) -> Result<(), ProfileValidationError> {
    validate_profile_id(&profile.id)?;
    validate_connection_draft(&profile.as_draft())
}

fn validate_profile_for_network(profile: &ConnectionProfile) -> Result<(), ProfileValidationError> {
    validate_persisted_profile(profile)?;
    if profile.driver == DriverKind::Redis && profile.tls == TlsMode::Preferred {
        return Err(ProfileValidationError::Field {
            field: ProfileFieldId::RedisTlsMode,
            code: PublicCode::RedisTlsPreferredLegacy,
        });
    }
    if profile.driver == DriverKind::Redis
        && profile.tls == TlsMode::Required
        && let Some(ca_file) = profile.redis_tls.ca_file.as_deref()
    {
        validate_ca_file(ca_file)?;
    }
    Ok(())
}

pub fn slugify_profile_id(display_name: &str) -> String {
    let mut slug = String::new();
    let mut separator_pending = false;
    for character in display_name.chars() {
        if character.is_ascii_alphanumeric() {
            if separator_pending && !slug.is_empty() {
                slug.push('-');
            }
            slug.push(character.to_ascii_lowercase());
            separator_pending = false;
        } else if !slug.is_empty() {
            separator_pending = true;
        }
    }
    if slug.is_empty() {
        "connection".to_owned()
    } else {
        slug
    }
}

fn validate_profile_id(value: &str) -> Result<(), ProfileValidationError> {
    if value.trim() != value || !valid_profile_id(value) {
        Err(ProfileValidationError::field(ProfileFieldId::ConnectionId))
    } else {
        Ok(())
    }
}

fn validate_create_secret_update(
    mode: CredentialMode,
    update: &SessionSecretUpdate,
) -> Result<(), SecretError> {
    match (mode, update) {
        (CredentialMode::None | CredentialMode::Environment, SessionSecretUpdate::Clear) => Ok(()),
        (CredentialMode::Session, SessionSecretUpdate::Replace(_)) => Ok(()),
        (CredentialMode::Session, SessionSecretUpdate::Clear) => Ok(()),
        _ => Err(SecretError::InvalidSessionIntent),
    }
}

fn invalid_session_intent_error() -> ServiceError {
    ServiceError::InvalidProfile(ProfileValidationError::Field {
        field: ProfileFieldId::SessionCredential,
        code: PublicCode::SessionCredential,
    })
}

fn validate_update_secret(
    persisted: &ConnectionProfile,
    destination_mode: CredentialMode,
    update: &SessionSecretUpdate,
    has_current: bool,
) -> Result<(), SecretError> {
    match (destination_mode, update) {
        (CredentialMode::None | CredentialMode::Environment, SessionSecretUpdate::Clear) => Ok(()),
        (CredentialMode::Session, SessionSecretUpdate::Replace(_)) => Ok(()),
        (CredentialMode::Session, SessionSecretUpdate::Clear) => Ok(()),
        (CredentialMode::Session, SessionSecretUpdate::Keep)
            if persisted.credential_mode == CredentialMode::Session && has_current =>
        {
            Ok(())
        }
        _ => Err(SecretError::InvalidSessionIntent),
    }
}

fn observed_profile<'a>(
    config: &'a Config,
    profile_id: &ProfileId,
) -> Option<&'a ConnectionProfile> {
    config
        .profiles
        .iter()
        .find(|profile| profile.id == profile_id.as_str())
}

fn same_keep_test_connection(profile: &ConnectionProfile, draft: &ConnectionDraft) -> bool {
    profile.driver == draft.driver
        && profile.host == draft.host
        && profile.port == draft.port
        && profile.database == draft.database
        && profile.username == draft.username
        && profile.tls == draft.tls
        && profile.credential_mode == draft.credential_mode
        && profile.secret_env == draft.secret_env
        && profile.redis_tls == draft.redis_tls
}

fn validate_ca_file(path: &Path) -> Result<(), ProfileValidationError> {
    let metadata = fs::metadata(path).map_err(|_| ProfileValidationError::Field {
        field: ProfileFieldId::RedisCaFile,
        code: PublicCode::RedisTlsCaInvalidPem,
    })?;
    if !metadata.is_file() {
        return Err(ProfileValidationError::Field {
            field: ProfileFieldId::RedisCaFile,
            code: PublicCode::RedisTlsCaInvalidPem,
        });
    }
    let bytes = fs::read(path).map_err(|_| ProfileValidationError::Field {
        field: ProfileFieldId::RedisCaFile,
        code: PublicCode::RedisTlsCaInvalidPem,
    })?;
    validate_certificate_pem(&bytes).map_err(|code| ProfileValidationError::Field {
        field: ProfileFieldId::RedisCaFile,
        code,
    })
}

fn validate_certificate_pem(bytes: &[u8]) -> Result<(), PublicCode> {
    let text = std::str::from_utf8(bytes).map_err(|_| PublicCode::RedisTlsCaInvalidPem)?;
    let begin = "-----BEGIN CERTIFICATE-----";
    let end = "-----END CERTIFICATE-----";
    let mut remaining = text;
    let mut count = 0_usize;
    while let Some(start) = remaining.find(begin) {
        let body_start = start + begin.len();
        let after_begin = &remaining[body_start..];
        let Some(body_end) = after_begin.find(end) else {
            return Err(PublicCode::RedisTlsCaInvalidPem);
        };
        let encoded: String = after_begin[..body_end]
            .chars()
            .filter(|character| !character.is_ascii_whitespace())
            .collect();
        let der = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|_| PublicCode::RedisTlsCaInvalidPem)?;
        let certificate = rustls_pki_types::CertificateDer::from(der);
        webpki::EndEntityCert::try_from(&certificate)
            .map_err(|_| PublicCode::RedisTlsCaInvalidPem)?;
        webpki::anchor_from_trusted_cert(&certificate)
            .map_err(|_| PublicCode::RedisTlsCaUntrustedIssuer)?;
        count += 1;
        remaining = &after_begin[body_end + end.len()..];
    }
    if count == 0 {
        Err(PublicCode::RedisTlsCaInvalidPem)
    } else {
        Ok(())
    }
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

fn ensure_connector_tls_support(
    connector: &dyn SessionConnector,
    profile: &ConnectionProfile,
) -> Result<(), DriverError> {
    if profile.driver == DriverKind::Redis
        && profile.tls == TlsMode::Required
        && !connector.supports_redis_tls()
    {
        return Err(DriverError::Unsupported {
            driver: DriverKind::Redis,
            operation: "required TLS transport".to_owned(),
        });
    }
    Ok(())
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
