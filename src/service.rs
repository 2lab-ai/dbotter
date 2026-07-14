//! Shared exact-path application service for CLI and desktop runtime.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::Engine as _;
use tokio::sync::{RwLock, Semaphore};

use crate::config::{
    CommitState, Config, ConfigError, ConfigMutation, ConfigSourceVersion, ConfigWriter,
    LoadedConfig, MigrationConsent, MutationOutcome, PostCommitObservation,
    PostCommitObservationError,
};
use crate::drivers::{DriverError, Session};
use crate::model::{
    ConnectionDraft, ConnectionProfile, CredentialMode, DraftId, DriverAvailability,
    DriverCapabilities, DriverKind, ExecuteRequest, OperationId, ProfileFieldId, ProfileGeneration,
    ProfileId, PublicCode, PublicSummary, QueryLanguage, QueryResult, TlsMode,
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
            | Self::ProfileIdConflict { .. }
            | Self::UnknownProfile(_) => PublicSummary::InvalidInput,
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
            Self::Driver(DriverError::Unavailable { .. } | DriverError::Unsupported { .. }) => {
                PublicSummary::UnsupportedFeature
            }
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
    async fn execute(&self, request: &ExecuteRequest) -> Result<QueryResult, DriverError>;
    async fn close(&self) -> Result<(), DriverError> {
        Ok(())
    }
}

#[async_trait]
impl SessionHandle for Session {
    async fn ping(&self, timeout: Duration) -> Result<(), DriverError> {
        Session::ping(self, timeout).await
    }

    async fn execute(&self, request: &ExecuteRequest) -> Result<QueryResult, DriverError> {
        Session::execute(self, request).await
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
    observed: Arc<RwLock<ObservedState>>,
    connector: Arc<dyn SessionConnector>,
    environment: Arc<dyn SecretResolver>,
    session_secrets: Arc<SessionSecretStore>,
    sessions: Arc<RwLock<HashMap<ProfileId, CachedSession>>>,
    next_generation: Arc<AtomicU64>,
    writer: ConfigWriter,
    mutation_lane: Arc<Semaphore>,
    config_uncertain: Arc<AtomicBool>,
}

struct ObservedState {
    config: Config,
    source_version: ConfigSourceVersion,
    generations: HashMap<ProfileId, ProfileGeneration>,
    session_epoch: u64,
}

#[derive(Clone)]
struct CachedSession {
    profile: ConnectionProfile,
    generation: ProfileGeneration,
    session_epoch: u64,
    handle: Arc<dyn SessionHandle>,
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
        Ok(Self::from_validated_loaded(
            path,
            loaded,
            connector,
            environment,
            session_secrets,
            writer,
        ))
    }

    fn from_validated_loaded(
        path: PathBuf,
        loaded: LoadedConfig,
        connector: Arc<dyn SessionConnector>,
        environment: Arc<dyn SecretResolver>,
        session_secrets: Arc<SessionSecretStore>,
        writer: ConfigWriter,
    ) -> Self {
        let mut generations = HashMap::new();
        let mut next = 1_u64;
        for profile in &loaded.config.profiles {
            generations.insert(ProfileId(profile.id.clone()), ProfileGeneration(next));
            next = next.saturating_add(1);
        }
        Self {
            config_path: Arc::new(path),
            observed: Arc::new(RwLock::new(ObservedState {
                config: loaded.config,
                source_version: loaded.source_version,
                generations,
                session_epoch: 1,
            })),
            connector,
            environment,
            session_secrets,
            sessions: Arc::new(RwLock::new(HashMap::new())),
            next_generation: Arc::new(AtomicU64::new(next)),
            writer,
            mutation_lane: Arc::new(Semaphore::new(1)),
            config_uncertain: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn config_path(&self) -> &Path {
        self.config_path.as_path()
    }

    pub async fn source_version(&self) -> ConfigSourceVersion {
        self.observed.read().await.source_version
    }

    pub async fn profiles_snapshot(&self) -> Vec<ConnectionProfile> {
        self.observed.read().await.config.profiles.clone()
    }

    pub async fn profiles_with_generations_snapshot(
        &self,
    ) -> Vec<(ConnectionProfile, ProfileGeneration)> {
        let observed = self.observed.read().await;
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
        self.observed
            .read()
            .await
            .generations
            .get(profile_id)
            .copied()
            .ok_or_else(|| ServiceError::UnknownProfile(profile_id.clone()))
    }

    pub async fn cached_session_count(&self) -> usize {
        self.sessions.read().await.len()
    }

    /// Reports only whether the exact saved profile currently owns an
    /// in-process Session credential. The credential capability never leaves
    /// the service boundary.
    pub fn has_current_session_secret(&self, profile_id: &ProfileId) -> Result<bool, ServiceError> {
        self.session_secrets
            .has_current(profile_id)
            .map_err(ServiceError::Secret)
    }

    pub fn is_config_uncertain(&self) -> bool {
        self.config_uncertain.load(Ordering::Acquire)
    }

    pub async fn reload_configuration(&self) -> Result<(), ServiceError> {
        let _permit = self
            .mutation_lane
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| ServiceError::MutationLaneClosed)?;
        let path = self.config_path.as_ref().clone();
        let loaded = tokio::task::spawn_blocking(move || crate::config::load_path(&path))
            .await
            .map_err(|_| ServiceError::ConfigTaskFailed)??;
        validate_config_identity(&loaded.config)?;
        self.replace_loaded_config(loaded).await;
        self.config_uncertain.store(false, Ordering::Release);
        Ok(())
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
            Err(ServiceError::Config(ConfigError::ProfileAlreadyExists(_)))
                if requested_id.is_some() =>
            {
                return Err(ServiceError::ProfileIdConflict {
                    draft_id,
                    operation_id,
                });
            }
            result => result?,
        };
        let affected_profile_id = outcome
            .affected_profile_id
            .as_deref()
            .map(|value| ProfileId(value.to_owned()))
            .ok_or(ServiceError::ConfigTaskFailed)?;
        if let Some(requested_id) = requested_id.as_ref()
            && requested_id != &affected_profile_id
        {
            self.enter_config_uncertain().await;
            return Err(ServiceError::ConfigUncertain);
        }
        expected_profile.id.clone_from(&affected_profile_id.0);
        if observed_profile(&outcome.loaded.config, &affected_profile_id) != Some(&expected_profile)
        {
            self.enter_config_uncertain().await;
            return Err(ServiceError::ConfigUncertain);
        }
        let profile_id = affected_profile_id;
        let generation = self
            .reconcile_after_mutation(
                outcome.loaded.config.clone(),
                MutationIdentity::Create(&profile_id),
            )
            .await?;
        self.session_secrets.apply(&profile_id, secret_update)?;
        Ok(ProfileMutationOutcome {
            operation_id,
            profile_id,
            profile_generation: generation,
            commit_state: outcome.state,
            migration_backup: outcome.migration_backup,
        })
    }

    pub async fn update_profile(
        &self,
        request: UpdateProfileRequest,
    ) -> Result<ProfileMutationOutcome, ServiceError> {
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
        let mutation = ConfigMutation::UpdateChecked {
            profile_id: request.profile_id.0.clone(),
            expected_profile,
            profile: updated.clone(),
        };
        let outcome = match self.write_config(mutation, request.migration_consent).await {
            Err(ServiceError::Config(
                ConfigError::ProfileMissing(_) | ConfigError::ExternalChange,
            )) => {
                return Err(ServiceError::ProfileStale {
                    profile_id: request.profile_id,
                    operation_id: request.operation_id,
                });
            }
            result => result?,
        };
        if outcome.affected_profile_id.as_deref() != Some(request.profile_id.as_str())
            || observed_profile(&outcome.loaded.config, &request.profile_id) != Some(&updated)
        {
            self.enter_config_uncertain().await;
            return Err(ServiceError::ConfigUncertain);
        }
        let keep_secret = matches!(&request.secret_update, SessionSecretUpdate::Keep);
        let generation = self
            .reconcile_after_mutation(
                outcome.loaded.config.clone(),
                MutationIdentity::Update {
                    profile_id: &request.profile_id,
                    keep_secret,
                },
            )
            .await?;
        let evict_session = !matches!(&request.secret_update, SessionSecretUpdate::Keep);
        if evict_session {
            self.evict_session(&request.profile_id).await;
        }
        self.session_secrets
            .apply(&request.profile_id, request.secret_update)?;
        Ok(ProfileMutationOutcome {
            operation_id: request.operation_id,
            profile_id: request.profile_id,
            profile_generation: generation,
            commit_state: outcome.state,
            migration_backup: outcome.migration_backup,
        })
    }

    pub async fn delete_profile(
        &self,
        request: DeleteProfileRequest,
    ) -> Result<ProfileMutationOutcome, ServiceError> {
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
            Err(ServiceError::Config(
                ConfigError::ProfileMissing(_) | ConfigError::ExternalChange,
            )) => {
                return Err(ServiceError::ProfileStale {
                    profile_id: request.profile_id,
                    operation_id: request.operation_id,
                });
            }
            result => result?,
        };
        if outcome.affected_profile_id.as_deref() != Some(request.profile_id.as_str())
            || observed_profile(&outcome.loaded.config, &request.profile_id).is_some()
        {
            self.enter_config_uncertain().await;
            return Err(ServiceError::ConfigUncertain);
        }
        let generation = self
            .reconcile_after_mutation(
                outcome.loaded.config.clone(),
                MutationIdentity::Delete(&request.profile_id),
            )
            .await?;
        self.evict_session(&request.profile_id).await;
        self.session_secrets
            .apply(&request.profile_id, SessionSecretUpdate::Clear)?;
        Ok(ProfileMutationOutcome {
            operation_id: request.operation_id,
            profile_id: request.profile_id,
            profile_generation: generation,
            commit_state: outcome.state,
            migration_backup: outcome.migration_backup,
        })
    }

    pub async fn test_draft_connection(
        &self,
        request: TestDraftRequest,
    ) -> Result<DraftTestOutcome, ServiceError> {
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
        let ping = temporary.ping(timeout).await;
        let close = temporary.close().await;
        self.ensure_config_certain()?;
        ping?;
        close?;
        Ok(DraftTestOutcome {
            draft_id,
            operation_id,
            driver: profile.driver,
            endpoint: profile.redacted_endpoint(),
            elapsed_ms: started.elapsed().as_millis(),
        })
    }

    pub async fn check(
        &self,
        operation_id: OperationId,
        profile_id: ProfileId,
        timeout: Duration,
    ) -> Result<CheckOutcome, ServiceError> {
        self.ensure_config_certain()?;
        let (profile, generation, session_epoch) =
            self.profile_with_generation(&profile_id).await?;
        validate_profile_for_network(&profile)?;
        let started = Instant::now();
        let session = self
            .session_for(&profile, generation, session_epoch, operation_id, timeout)
            .await?;
        let ping = session.ping(timeout).await;
        let observation = self
            .ensure_session_observation(&profile_id, generation, session_epoch, operation_id)
            .await;
        if ping.is_err() || observation.is_err() {
            self.close_exact_cached_session(
                &profile_id,
                &profile,
                generation,
                session_epoch,
                &session,
            )
            .await;
        }
        observation?;
        ping?;
        Ok(CheckOutcome {
            operation_id,
            profile_id,
            driver: profile.driver,
            endpoint: profile.redacted_endpoint(),
            elapsed_ms: started.elapsed().as_millis(),
        })
    }

    pub async fn execute(&self, request: ExecuteRequest) -> Result<ExecuteOutcome, ServiceError> {
        self.ensure_config_certain()?;
        if request.row_limit == 0 || request.row_limit > 10_000 {
            return Err(ServiceError::InvalidRowLimit);
        }
        let (profile, generation, session_epoch) =
            self.profile_with_generation(&request.profile_id).await?;
        validate_profile_for_network(&profile)?;
        if profile.driver.language() != request.language {
            return Err(ServiceError::LanguageMismatch {
                driver: profile.driver,
                actual: request.language,
            });
        }
        let session = self
            .session_for(
                &profile,
                generation,
                session_epoch,
                request.operation_id,
                request.timeout,
            )
            .await?;
        let result = session.execute(&request).await;
        let observation = self
            .ensure_session_observation(
                &request.profile_id,
                generation,
                session_epoch,
                request.operation_id,
            )
            .await;
        if result.is_err() || observation.is_err() {
            self.close_exact_cached_session(
                &request.profile_id,
                &profile,
                generation,
                session_epoch,
                &session,
            )
            .await;
        }
        observation?;
        let result = result?;
        Ok(ExecuteOutcome {
            operation_id: request.operation_id,
            profile_id: request.profile_id,
            driver: profile.driver,
            endpoint: profile.redacted_endpoint(),
            result,
        })
    }

    async fn write_config(
        &self,
        mutation: ConfigMutation,
        consent: MigrationConsent,
    ) -> Result<ObservedMutationOutcome, ServiceError> {
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
                self.enter_config_uncertain().await;
                Err(ServiceError::PostCommitObservation(error))
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

    async fn enter_config_uncertain(&self) {
        let handles = {
            let mut observed = self.observed.write().await;
            self.config_uncertain.store(true, Ordering::Release);
            let mut sessions = self.sessions.write().await;
            observed.session_epoch = observed.session_epoch.saturating_add(1);
            sessions
                .drain()
                .map(|(_, cached)| cached.handle)
                .collect::<Vec<_>>()
        };
        let _ = self.session_secrets.clear_all();
        for handle in handles {
            let _ = handle.close().await;
        }
    }

    async fn evict_session(&self, profile_id: &ProfileId) {
        let handle = self
            .sessions
            .write()
            .await
            .remove(profile_id)
            .map(|cached| cached.handle);
        if let Some(handle) = handle {
            let _ = handle.close().await;
        }
    }

    async fn close_exact_cached_session(
        &self,
        profile_id: &ProfileId,
        profile: &ConnectionProfile,
        generation: ProfileGeneration,
        session_epoch: u64,
        handle: &Arc<dyn SessionHandle>,
    ) {
        let removed = {
            let mut sessions = self.sessions.write().await;
            let remove_exact = sessions.get(profile_id).is_some_and(|cached| {
                cached.profile == *profile
                    && cached.generation == generation
                    && cached.session_epoch == session_epoch
                    && Arc::ptr_eq(&cached.handle, handle)
            });
            if remove_exact {
                sessions.remove(profile_id).map(|cached| cached.handle)
            } else {
                None
            }
        };
        if let Some(removed) = removed {
            let _ = removed.close().await;
        }
    }

    async fn replace_loaded_config(&self, loaded: LoadedConfig) {
        let (previous_profiles, previous_generations) = {
            let observed = self.observed.read().await;
            (
                observed
                    .config
                    .profiles
                    .iter()
                    .cloned()
                    .map(|profile| (ProfileId(profile.id.clone()), profile))
                    .collect::<HashMap<_, _>>(),
                observed.generations.clone(),
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
                    .unwrap_or_else(|| {
                        ProfileGeneration(self.next_generation.fetch_add(1, Ordering::Relaxed))
                    })
            } else {
                ProfileGeneration(self.next_generation.fetch_add(1, Ordering::Relaxed))
            };
            generations.insert(profile_id, generation);
        }
        let handles = {
            let mut observed = self.observed.write().await;
            let mut sessions = self.sessions.write().await;
            let session_epoch = observed.session_epoch;
            let handles = sessions
                .extract_if(|profile_id, cached| {
                    let retain = unchanged.contains(profile_id)
                        && next_profiles.get(profile_id) == Some(&cached.profile);
                    if retain && let Some(generation) = generations.get(profile_id) {
                        cached.generation = *generation;
                    }
                    !retain
                })
                .map(|(_, cached)| cached.handle)
                .collect::<Vec<_>>();
            let _ = self.session_secrets.retain_profiles(&unchanged);
            *observed = ObservedState {
                config: loaded.config,
                source_version: loaded.source_version,
                generations,
                session_epoch,
            };
            handles
        };
        for handle in handles {
            let _ = handle.close().await;
        }
    }

    async fn reconcile_after_mutation(
        &self,
        config: Config,
        identity: MutationIdentity<'_>,
    ) -> Result<ProfileGeneration, ServiceError> {
        let (previous_profiles, previous_generations) = {
            let observed = self.observed.read().await;
            (
                observed
                    .config
                    .profiles
                    .iter()
                    .cloned()
                    .map(|profile| (ProfileId(profile.id.clone()), profile))
                    .collect::<HashMap<_, _>>(),
                observed.generations.clone(),
            )
        };
        let next_profiles: HashMap<ProfileId, ConnectionProfile> = config
            .profiles
            .iter()
            .cloned()
            .map(|profile| (ProfileId(profile.id.clone()), profile))
            .collect();
        let local_profile_id = identity.profile_id();
        let local_keep_secret = identity.keeps_secret();
        let retained_cache_ids: HashSet<ProfileId> = next_profiles
            .iter()
            .filter(|(profile_id, profile)| {
                previous_profiles.get(*profile_id) == Some(*profile)
                    && (*profile_id != local_profile_id
                        || matches!(
                            identity,
                            MutationIdentity::Update {
                                keep_secret: true,
                                ..
                            }
                        ))
            })
            .map(|(profile_id, _)| profile_id.clone())
            .collect();
        let retained_secret_ids: HashSet<ProfileId> = next_profiles
            .iter()
            .filter(|(profile_id, profile)| {
                if *profile_id == local_profile_id {
                    local_keep_secret
                } else {
                    previous_profiles.get(*profile_id) == Some(*profile)
                }
            })
            .map(|(profile_id, _)| profile_id.clone())
            .collect();

        let mut generations = HashMap::with_capacity(config.profiles.len());
        let mut affected_generation = None;
        for profile in &config.profiles {
            let profile_id = ProfileId(profile.id.clone());
            let is_local_target = &profile_id == local_profile_id;
            let force_local_generation = is_local_target && !identity.is_delete();
            let generation = if force_local_generation {
                ProfileGeneration(self.next_generation.fetch_add(1, Ordering::Relaxed))
            } else if previous_profiles.get(&profile_id) == Some(profile) {
                previous_generations
                    .get(&profile_id)
                    .copied()
                    .unwrap_or_else(|| {
                        ProfileGeneration(self.next_generation.fetch_add(1, Ordering::Relaxed))
                    })
            } else {
                ProfileGeneration(self.next_generation.fetch_add(1, Ordering::Relaxed))
            };
            if is_local_target {
                affected_generation = Some(generation);
            }
            generations.insert(profile_id, generation);
        }
        if identity.is_delete() {
            affected_generation = Some(ProfileGeneration(
                self.next_generation.fetch_add(1, Ordering::Relaxed),
            ));
        }
        let handles = {
            let mut observed = self.observed.write().await;
            let mut sessions = self.sessions.write().await;
            let session_epoch = observed.session_epoch;
            let handles = sessions
                .extract_if(|profile_id, cached| {
                    let retain = retained_cache_ids.contains(profile_id)
                        && next_profiles.get(profile_id) == Some(&cached.profile);
                    if retain && let Some(generation) = generations.get(profile_id) {
                        cached.generation = *generation;
                    }
                    !retain
                })
                .map(|(_, cached)| cached.handle)
                .collect::<Vec<_>>();
            let _ = self.session_secrets.retain_profiles(&retained_secret_ids);
            *observed = ObservedState {
                config,
                source_version: ConfigSourceVersion::V2,
                generations,
                session_epoch,
            };
            handles
        };
        for handle in handles {
            let _ = handle.close().await;
        }
        match affected_generation {
            Some(generation) => Ok(generation),
            None => {
                self.enter_config_uncertain().await;
                Err(ServiceError::ConfigUncertain)
            }
        }
    }

    async fn ensure_generation(
        &self,
        profile_id: &ProfileId,
        expected: ProfileGeneration,
        operation_id: OperationId,
    ) -> Result<(), ServiceError> {
        if self
            .observed
            .read()
            .await
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

    async fn profile(&self, profile_id: &ProfileId) -> Result<ConnectionProfile, ServiceError> {
        self.observed
            .read()
            .await
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
    ) -> Result<(ConnectionProfile, ProfileGeneration, u64), ServiceError> {
        let observed = self.observed.read().await;
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
        Ok((profile, generation, observed.session_epoch))
    }

    async fn ensure_session_observation(
        &self,
        profile_id: &ProfileId,
        generation: ProfileGeneration,
        session_epoch: u64,
        operation_id: OperationId,
    ) -> Result<(), ServiceError> {
        let observed = self.observed.read().await;
        if self.is_config_uncertain() {
            Err(ServiceError::ConfigUncertain)
        } else if observed.generations.get(profile_id).copied() == Some(generation)
            && observed.session_epoch == session_epoch
        {
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
        session_epoch: u64,
        operation_id: OperationId,
        timeout: Duration,
    ) -> Result<Arc<dyn SessionHandle>, ServiceError> {
        self.ensure_config_certain()?;
        let profile_id = ProfileId(profile.id.clone());
        {
            let observed = self.observed.read().await;
            let sessions = self.sessions.read().await;
            let is_current = observed.generations.get(&profile_id).copied() == Some(generation)
                && observed.session_epoch == session_epoch
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
            if let Some(cached) = sessions.get(&profile_id)
                && cached.profile == *profile
                && cached.generation == generation
                && cached.session_epoch == session_epoch
            {
                return Ok(cached.handle.clone());
            }
        }
        ensure_ready(profile)?;
        ensure_connector_tls_support(self.connector.as_ref(), profile)?;
        let secret = self.resolve_profile_secret(profile)?;
        let connected = match self
            .connector
            .connect(profile, secret.as_deref(), timeout)
            .await
        {
            Ok(connected) => connected,
            Err(error) => {
                self.ensure_session_observation(
                    &profile_id,
                    generation,
                    session_epoch,
                    operation_id,
                )
                .await?;
                return Err(error.into());
            }
        };
        enum CacheInstall {
            Installed(Option<Arc<dyn SessionHandle>>),
            Existing(Arc<dyn SessionHandle>),
            Stale,
            Uncertain,
        }
        let install = {
            let observed = self.observed.read().await;
            let mut sessions = self.sessions.write().await;
            let is_current = observed.generations.get(&profile_id).copied() == Some(generation)
                && observed.session_epoch == session_epoch
                && observed
                    .config
                    .profiles
                    .iter()
                    .any(|current| current == profile);
            if self.is_config_uncertain() {
                CacheInstall::Uncertain
            } else if !is_current {
                CacheInstall::Stale
            } else if let Some(cached) = sessions.get(&profile_id)
                && cached.profile == *profile
                && cached.generation == generation
                && cached.session_epoch == session_epoch
            {
                CacheInstall::Existing(cached.handle.clone())
            } else {
                let displaced = sessions
                    .insert(
                        profile_id.clone(),
                        CachedSession {
                            profile: profile.clone(),
                            generation,
                            session_epoch,
                            handle: connected.clone(),
                        },
                    )
                    .map(|cached| cached.handle);
                CacheInstall::Installed(displaced)
            }
        };
        match install {
            CacheInstall::Installed(displaced) => {
                if let Some(displaced) = displaced {
                    let _ = displaced.close().await;
                }
                Ok(connected)
            }
            CacheInstall::Existing(existing) => {
                let _ = connected.close().await;
                Ok(existing)
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

enum MutationIdentity<'a> {
    Create(&'a ProfileId),
    Update {
        profile_id: &'a ProfileId,
        keep_secret: bool,
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

    fn keeps_secret(&self) -> bool {
        matches!(
            self,
            Self::Update {
                keep_secret: true,
                ..
            }
        )
    }

    fn is_delete(&self) -> bool {
        matches!(self, Self::Delete(_))
    }
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
