use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use serde::ser::Error as _;
use serde::{Deserialize, Serialize, Serializer};
use sha2::{Digest as _, Sha256};

use crate::model::{
    ConnectionProfile, CredentialMode, LegacyConfigVersion, ProfileAccess, ProfileEnvironment,
    ProfileInstanceId, ProfileSafetyPosture, RedisTlsConfig,
};

pub const CONFIG_ENV: &str = "DBOTTER_CONFIG";
pub const CURRENT_CONFIG_VERSION: u32 = 3;
pub const V1_MIGRATION_BACKUP_SUFFIX: &str = ".v1.bak";
pub const V2_MIGRATION_BACKUP_SUFFIX: &str = ".v2.bak";
pub const MIGRATION_BACKUP_SUFFIX: &str = V1_MIGRATION_BACKUP_SUFFIX;

const MIGRATION_DOCUMENT_MAX_BYTES: usize = 1024 * 1024;
const PROFILE_INSTANCE_ID_GENERATION_ATTEMPTS: usize = 16;

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(1);
static PROCESS_WRITER: OnceLock<Mutex<()>> = OnceLock::new();

#[derive(thiserror::Error)]
pub enum ConfigError {
    #[error("configuration I/O failed")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("config is not valid UTF-8")]
    InvalidUtf8,
    #[error("configuration parse failed")]
    Parse(#[from] toml::de::Error),
    #[error("configuration serialization failed")]
    Serialize(#[from] toml::ser::Error),
    #[error("unsupported config version {0}")]
    UnsupportedVersion(u32),
    #[error("could not determine config path; set DBOTTER_CONFIG")]
    NoConfigPath,
    #[error("version 1 migration must be confirmed")]
    MigrationConfirmationRequired { backup: PathBuf },
    #[error("legacy profiles require explicit version 3 posture classification")]
    MigrationPostureRequired,
    #[error("migration posture document is invalid")]
    InvalidMigrationDocument,
    #[error("migration posture document exceeds its byte limit")]
    MigrationDocumentTooLarge { limit: usize, actual: usize },
    #[error("migration backup already exists with different contents")]
    BackupConflict { path: PathBuf },
    #[error("profile already exists")]
    ProfileAlreadyExists(String),
    #[error("profile does not exist")]
    ProfileMissing(String),
    #[error("profile id is immutable")]
    ImmutableProfileId,
    #[error("configuration changed outside this writer")]
    ExternalChange,
    #[error("configuration contains an invalid profile")]
    InvalidProfile,
    #[error("secure profile identity generation failed")]
    EntropyUnavailable,
    #[error("configuration mutation failed before commit at {stage:?}")]
    NotCommitted {
        stage: MutationFailpoint,
        #[source]
        source: std::io::Error,
    },
    #[error("configuration writer lock is unavailable")]
    WriterUnavailable,
}

impl fmt::Debug for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { .. } => formatter.write_str("Io(<redacted>)"),
            Self::InvalidUtf8 => formatter.write_str("InvalidUtf8"),
            Self::Parse(_) => formatter.write_str("Parse(<redacted>)"),
            Self::Serialize(_) => formatter.write_str("Serialize(<redacted>)"),
            Self::UnsupportedVersion(version) => formatter
                .debug_tuple("UnsupportedVersion")
                .field(version)
                .finish(),
            Self::NoConfigPath => formatter.write_str("NoConfigPath"),
            Self::MigrationConfirmationRequired { .. } => {
                formatter.write_str("MigrationConfirmationRequired(<redacted>)")
            }
            Self::MigrationPostureRequired => formatter.write_str("MigrationPostureRequired"),
            Self::InvalidMigrationDocument => formatter.write_str("InvalidMigrationDocument"),
            Self::MigrationDocumentTooLarge { limit, actual } => formatter
                .debug_struct("MigrationDocumentTooLarge")
                .field("limit", limit)
                .field("actual", actual)
                .finish(),
            Self::BackupConflict { .. } => formatter.write_str("BackupConflict(<redacted>)"),
            Self::ProfileAlreadyExists(_) => {
                formatter.write_str("ProfileAlreadyExists(<redacted>)")
            }
            Self::ProfileMissing(_) => formatter.write_str("ProfileMissing(<redacted>)"),
            Self::ImmutableProfileId => formatter.write_str("ImmutableProfileId"),
            Self::ExternalChange => formatter.write_str("ExternalChange"),
            Self::InvalidProfile => formatter.write_str("InvalidProfile"),
            Self::EntropyUnavailable => formatter.write_str("EntropyUnavailable"),
            Self::NotCommitted { stage, .. } => formatter
                .debug_struct("NotCommitted")
                .field("stage", stage)
                .field("source", &"<redacted>")
                .finish(),
            Self::WriterUnavailable => formatter.write_str("WriterUnavailable"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub version: u32,
    pub profiles: Vec<ConnectionProfile>,
}

impl Serialize for Config {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self.version {
            2 => ConfigV2Wire::from_config(self).serialize(serializer),
            CURRENT_CONFIG_VERSION => ConfigV3Wire::try_from_config(self)
                .map_err(S::Error::custom)?
                .serialize(serializer),
            version => Err(S::Error::custom(format_args!(
                "unsupported config version {version}"
            ))),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: CURRENT_CONFIG_VERSION,
            profiles: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigSourceVersion {
    Missing,
    V1,
    V2,
    V3,
}

#[derive(Clone, Serialize, PartialEq, Eq)]
pub struct MigrationPlan {
    pub source_version: u32,
    pub config_fingerprint: String,
    pub profiles: Vec<MigrationProfileSummary>,
}

impl fmt::Debug for MigrationPlan {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MigrationPlan")
            .field("source_version", &self.source_version)
            .field("config_fingerprint", &"<redacted>")
            .field("profile_count", &self.profiles.len())
            .finish()
    }
}

#[derive(Clone, Serialize, PartialEq, Eq)]
pub struct MigrationProfileSummary {
    pub profile_id: String,
    pub endpoint: String,
}

#[derive(Clone, PartialEq, Eq)]
struct DestinationFingerprint(Option<Vec<u8>>);

#[derive(Clone)]
pub struct LoadedConfig {
    pub config: Config,
    pub source_version: ConfigSourceVersion,
    pub migration_required: bool,
    pub original_bytes: Option<Vec<u8>>,
    fingerprint: DestinationFingerprint,
}

impl std::fmt::Debug for LoadedConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LoadedConfig")
            .field("source_version", &self.source_version)
            .field("migration_required", &self.migration_required)
            .field("profile_count", &self.config.profiles.len())
            .field("has_original_bytes", &self.original_bytes.is_some())
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrationConsent {
    Confirmed,
    Cancelled,
}

impl MigrationConsent {
    pub const fn from_confirmation(confirmed: bool) -> Self {
        if confirmed {
            Self::Confirmed
        } else {
            Self::Cancelled
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigMutation {
    Create(ConnectionProfile),
    CreateAuto {
        base_id: String,
        profile: ConnectionProfile,
    },
    UpdateChecked {
        profile_id: String,
        expected_profile: ConnectionProfile,
        profile: ConnectionProfile,
    },
    DeleteChecked {
        profile_id: String,
        expected_profile: ConnectionProfile,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CommitState {
    NotCommitted,
    Committed,
    CommittedDurabilityUnknown,
}

#[derive(thiserror::Error)]
#[error("configuration committed but exact-path observation failed")]
pub struct PostCommitObservationError {
    commit_state: CommitState,
    #[source]
    source: ConfigError,
    identity_binding_changed: bool,
}

impl PostCommitObservationError {
    pub const fn commit_state(&self) -> CommitState {
        self.commit_state
    }

    pub(crate) const fn identity_binding_changed(&self) -> bool {
        self.identity_binding_changed
    }
}

impl std::fmt::Debug for PostCommitObservationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PostCommitObservationError")
            .field("commit_state", &self.commit_state)
            .field("source", &"<redacted>")
            .finish()
    }
}

pub enum PostCommitObservation {
    Observed(LoadedConfig),
    Failed(PostCommitObservationError),
}

impl std::fmt::Debug for PostCommitObservation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Observed(loaded) => formatter
                .debug_struct("Observed")
                .field("source_version", &loaded.source_version)
                .field("profile_count", &loaded.config.profiles.len())
                .finish(),
            Self::Failed(error) => error.fmt(formatter),
        }
    }
}

pub struct MutationOutcome {
    pub state: CommitState,
    pub observation: PostCommitObservation,
    pub migration_backup: Option<PathBuf>,
    pub affected_profile_id: Option<String>,
    pub affected_profile_instance_id: Option<ProfileInstanceId>,
}

impl std::fmt::Debug for MutationOutcome {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MutationOutcome")
            .field("state", &self.state)
            .field("observation", &self.observation)
            .field(
                "migration_backup",
                &self.migration_backup.as_ref().map(|_| "<available>"),
            )
            .field("affected_profile_id", &self.affected_profile_id)
            .field(
                "affected_profile_instance_id",
                &self
                    .affected_profile_instance_id
                    .as_ref()
                    .map(|_| "<redacted>"),
            )
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MutationFailpoint {
    BackupTempCreate,
    BackupWrite,
    BackupFileSync,
    BackupRename,
    BackupDirectorySync,
    MainTempCreate,
    MainWrite,
    MainFileSync,
    MainPreRename,
    MainPostRename,
    MainDirectorySync,
    MainObservationLoad,
}

pub trait MutationFaultInjector: Send + Sync {
    fn check(&self, point: MutationFailpoint, path: &Path) -> std::io::Result<()>;
}

pub trait ProfileInstanceIdGenerator: Send + Sync {
    fn generate(&self) -> Option<ProfileInstanceId>;
}

#[derive(Default)]
struct SecureProfileInstanceIdGenerator;

impl ProfileInstanceIdGenerator for SecureProfileInstanceIdGenerator {
    fn generate(&self) -> Option<ProfileInstanceId> {
        ProfileInstanceId::generate().ok()
    }
}

#[derive(Default)]
struct NoFaults;

impl MutationFaultInjector for NoFaults {
    fn check(&self, _point: MutationFailpoint, _path: &Path) -> std::io::Result<()> {
        Ok(())
    }
}

#[derive(Clone)]
/// A single-process serialized writer with an exact destination recheck.
///
/// It deliberately makes no multi-process claim: another process can still
/// race after the recheck and before rename.
pub struct ConfigWriter {
    faults: Arc<dyn MutationFaultInjector>,
    profile_instance_ids: Arc<dyn ProfileInstanceIdGenerator>,
}

struct ObservationIdentityGuard {
    stable_bindings: HashMap<ProfileInstanceId, String>,
    stable_profile_instances: HashMap<String, ProfileInstanceId>,
    deleted_instance_id: Option<ProfileInstanceId>,
}

struct MainObservationFailure {
    source: ConfigError,
    identity_binding_changed: bool,
}

impl MainObservationFailure {
    const fn generic(source: ConfigError) -> Self {
        Self {
            source,
            identity_binding_changed: false,
        }
    }

    const fn identity_binding_changed(source: ConfigError) -> Self {
        Self {
            source,
            identity_binding_changed: true,
        }
    }
}

impl ObservationIdentityGuard {
    fn for_mutation(config: &Config, mutation: &ConfigMutation) -> Self {
        let stable_bindings = config
            .profiles
            .iter()
            .filter_map(|profile| {
                profile
                    .safety
                    .instance_id()
                    .map(|instance_id| (instance_id, profile.id.clone()))
            })
            .collect();
        let stable_profile_instances = config
            .profiles
            .iter()
            .filter_map(|profile| {
                profile
                    .safety
                    .instance_id()
                    .map(|instance_id| (profile.id.clone(), instance_id))
            })
            .collect();
        let deleted_instance_id = match mutation {
            ConfigMutation::DeleteChecked { profile_id, .. } => config
                .profiles
                .iter()
                .find(|profile| profile.id == *profile_id)
                .and_then(|profile| profile.safety.instance_id()),
            ConfigMutation::Create(_)
            | ConfigMutation::CreateAuto { .. }
            | ConfigMutation::UpdateChecked { .. } => None,
        };
        Self {
            stable_bindings,
            stable_profile_instances,
            deleted_instance_id,
        }
    }

    fn validate(&self, observed: &Config) -> Result<(), ConfigError> {
        for profile in &observed.profiles {
            let Some(instance_id) = profile.safety.instance_id() else {
                return Err(ConfigError::ExternalChange);
            };
            if self.deleted_instance_id == Some(instance_id)
                || self
                    .stable_bindings
                    .get(&instance_id)
                    .is_some_and(|profile_id| profile_id != &profile.id)
                || self
                    .stable_profile_instances
                    .get(&profile.id)
                    .is_some_and(|stable_instance_id| stable_instance_id != &instance_id)
            {
                return Err(ConfigError::ExternalChange);
            }
        }
        Ok(())
    }
}

impl Default for ConfigWriter {
    fn default() -> Self {
        Self {
            faults: Arc::new(NoFaults),
            profile_instance_ids: Arc::new(SecureProfileInstanceIdGenerator),
        }
    }
}

impl ConfigWriter {
    pub fn with_fault_injector(faults: Arc<dyn MutationFaultInjector>) -> Self {
        Self {
            faults,
            profile_instance_ids: Arc::new(SecureProfileInstanceIdGenerator),
        }
    }

    pub fn with_profile_instance_id_generator(
        mut self,
        generator: Arc<dyn ProfileInstanceIdGenerator>,
    ) -> Self {
        self.profile_instance_ids = generator;
        self
    }

    pub fn migration_plan(&self, path: &Path) -> Result<MigrationPlan, ConfigError> {
        let lock = PROCESS_WRITER.get_or_init(|| Mutex::new(()));
        let _guard = lock.lock().map_err(|_| ConfigError::WriterUnavailable)?;
        let loaded = load_path(path)?;
        validate_legacy_migration_source(&loaded)?;
        let original = loaded
            .original_bytes
            .as_deref()
            .ok_or(ConfigError::MigrationPostureRequired)?;
        Ok(MigrationPlan {
            source_version: migration_source_number(loaded.source_version)
                .ok_or(ConfigError::MigrationPostureRequired)?,
            config_fingerprint: migration_config_fingerprint(original),
            profiles: loaded
                .config
                .profiles
                .iter()
                .map(|profile| MigrationProfileSummary {
                    profile_id: profile.id.clone(),
                    endpoint: profile.redacted_endpoint(),
                })
                .collect(),
        })
    }

    pub fn migrate_v3(
        &self,
        path: &Path,
        posture_document: &[u8],
    ) -> Result<MutationOutcome, ConfigError> {
        let document = parse_migration_posture_document(posture_document)?;
        let lock = PROCESS_WRITER.get_or_init(|| Mutex::new(()));
        let _guard = lock.lock().map_err(|_| ConfigError::WriterUnavailable)?;
        let loaded = load_path(path)?;
        validate_legacy_migration_source(&loaded)?;
        let original = loaded
            .original_bytes
            .as_deref()
            .ok_or(ConfigError::MigrationPostureRequired)?;
        if migration_config_fingerprint(original) != document.config_fingerprint {
            return Err(ConfigError::ExternalChange);
        }
        let mut assignments = migration_assignments(document, &loaded.config)?;
        let mut config = loaded.config.clone();
        let mut instance_ids = HashSet::with_capacity(config.profiles.len());
        for profile in &mut config.profiles {
            let assignment = assignments
                .remove(&profile.id)
                .ok_or(ConfigError::InvalidMigrationDocument)?;
            let instance_id = generate_unique_profile_instance_id(
                self.profile_instance_ids.as_ref(),
                &mut instance_ids,
            )?;
            profile.safety = ProfileSafetyPosture::classified(
                assignment.environment,
                assignment.access,
                instance_id,
            );
        }
        if !assignments.is_empty() {
            return Err(ConfigError::InvalidMigrationDocument);
        }
        config.version = CURRENT_CONFIG_VERSION;
        validate_v3_profile_identities(&config)?;
        crate::service::validate_config_identity(&config)
            .map_err(|_| ConfigError::InvalidProfile)?;
        let encoded = encode_config(&config)?;

        let backup = migration_backup_path_for_source(path, loaded.source_version);
        self.write_backup(&backup, original)?;
        let mut state = self.write_main(path, &encoded, &loaded.fingerprint)?;
        let observation = match self.observe_main(path, Some(&encoded), None) {
            Ok(observed) => PostCommitObservation::Observed(observed),
            Err(source) => {
                state = CommitState::CommittedDurabilityUnknown;
                PostCommitObservation::Failed(PostCommitObservationError {
                    commit_state: state,
                    source: source.source,
                    identity_binding_changed: source.identity_binding_changed,
                })
            }
        };
        Ok(MutationOutcome {
            state,
            observation,
            migration_backup: Some(backup),
            affected_profile_id: None,
            affected_profile_instance_id: None,
        })
    }

    pub fn mutate_path(
        &self,
        path: &Path,
        mutation: ConfigMutation,
        consent: MigrationConsent,
    ) -> Result<MutationOutcome, ConfigError> {
        let lock = PROCESS_WRITER.get_or_init(|| Mutex::new(()));
        let _guard = lock.lock().map_err(|_| ConfigError::WriterUnavailable)?;
        let loaded = load_path(path)?;
        if loaded.migration_required {
            return Err(ConfigError::MigrationPostureRequired);
        }
        crate::service::validate_config_identity(&loaded.config)
            .map_err(|_| ConfigError::InvalidProfile)?;
        crate::service::validate_config_mutation(&mutation)
            .map_err(|_| ConfigError::InvalidProfile)?;
        let observation_identity =
            ObservationIdentityGuard::for_mutation(&loaded.config, &mutation);
        let backup = self.prepare_migration(path, &loaded, consent)?;
        let mut config = loaded.config.clone();
        let affected_profile_id =
            apply_mutation(&mut config, mutation, self.profile_instance_ids.as_ref())?;
        config.version = CURRENT_CONFIG_VERSION;
        crate::service::validate_config_identity(&config)
            .map_err(|_| ConfigError::InvalidProfile)?;
        let affected_profile_instance_id = affected_profile_id.as_deref().and_then(|profile_id| {
            config
                .profiles
                .iter()
                .find(|profile| profile.id == profile_id)
                .and_then(|profile| profile.safety.instance_id())
        });
        let encoded = encode_config(&config)?;
        let mut state = self.write_main(path, &encoded, &loaded.fingerprint)?;
        let observation = match self.observe_main(path, None, Some(&observation_identity)) {
            Ok(observed) => PostCommitObservation::Observed(observed),
            Err(source) => {
                state = CommitState::CommittedDurabilityUnknown;
                PostCommitObservation::Failed(PostCommitObservationError {
                    commit_state: state,
                    source: source.source,
                    identity_binding_changed: source.identity_binding_changed,
                })
            }
        };
        Ok(MutationOutcome {
            state,
            observation,
            migration_backup: backup,
            affected_profile_id,
            affected_profile_instance_id,
        })
    }

    fn observe_main(
        &self,
        path: &Path,
        expected_bytes: Option<&[u8]>,
        identity_guard: Option<&ObservationIdentityGuard>,
    ) -> Result<LoadedConfig, MainObservationFailure> {
        self.faults
            .check(MutationFailpoint::MainObservationLoad, path)
            .map_err(|source| ConfigError::Io {
                path: path.to_owned(),
                source,
            })
            .map_err(MainObservationFailure::generic)?;
        let loaded = load_path(path).map_err(MainObservationFailure::generic)?;
        crate::service::validate_config_identity(&loaded.config)
            .map_err(|_| ConfigError::InvalidProfile)
            .map_err(MainObservationFailure::generic)?;
        if loaded.source_version != ConfigSourceVersion::V3
            || expected_bytes
                .is_some_and(|expected| loaded.original_bytes.as_deref() != Some(expected))
        {
            return Err(MainObservationFailure::generic(ConfigError::ExternalChange));
        }
        if let Some(identity_guard) = identity_guard {
            identity_guard
                .validate(&loaded.config)
                .map_err(MainObservationFailure::identity_binding_changed)?;
        }
        Ok(loaded)
    }

    fn prepare_migration(
        &self,
        path: &Path,
        loaded: &LoadedConfig,
        consent: MigrationConsent,
    ) -> Result<Option<PathBuf>, ConfigError> {
        if !loaded.migration_required {
            return Ok(None);
        }
        let backup = migration_backup_path_for_source(path, loaded.source_version);
        if consent != MigrationConsent::Confirmed {
            return Err(ConfigError::MigrationConfirmationRequired { backup });
        }
        let original = loaded.original_bytes.as_deref().unwrap_or_default();
        self.write_backup(&backup, original)?;
        Ok(Some(backup))
    }

    fn write_backup(&self, backup: &Path, original: &[u8]) -> Result<(), ConfigError> {
        match read_existing_backup(backup)? {
            Some(existing) if existing == original => {
                let directory = ensure_parent(backup)?;
                self.faults
                    .check(MutationFailpoint::BackupDirectorySync, backup)
                    .map_err(|source| {
                        not_committed(MutationFailpoint::BackupDirectorySync, source)
                    })?;
                return sync_directory(&directory).map_err(|source| {
                    not_committed(MutationFailpoint::BackupDirectorySync, source)
                });
            }
            Some(_) => {
                return Err(ConfigError::BackupConflict {
                    path: backup.to_owned(),
                });
            }
            None => {}
        }

        let directory = ensure_parent(backup)?;
        self.faults
            .check(MutationFailpoint::BackupTempCreate, backup)
            .map_err(|source| not_committed(MutationFailpoint::BackupTempCreate, source))?;
        let (temp, mut file) = create_temp(&directory, "backup")?;
        let cleanup = TempCleanup::new(temp.clone());
        self.faults
            .check(MutationFailpoint::BackupWrite, backup)
            .map_err(|source| not_committed(MutationFailpoint::BackupWrite, source))?;
        file.write_all(original)
            .map_err(|source| not_committed(MutationFailpoint::BackupWrite, source))?;
        file.flush()
            .map_err(|source| not_committed(MutationFailpoint::BackupWrite, source))?;
        self.faults
            .check(MutationFailpoint::BackupFileSync, backup)
            .map_err(|source| not_committed(MutationFailpoint::BackupFileSync, source))?;
        file.sync_all()
            .map_err(|source| not_committed(MutationFailpoint::BackupFileSync, source))?;
        drop(file);
        self.faults
            .check(MutationFailpoint::BackupRename, backup)
            .map_err(|source| not_committed(MutationFailpoint::BackupRename, source))?;
        match rename_no_replace(&temp, backup) {
            Ok(()) => cleanup.disarm(),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let existing = read_existing_backup(backup)?;
                if existing.as_deref() != Some(original) {
                    return Err(ConfigError::BackupConflict {
                        path: backup.to_owned(),
                    });
                }
            }
            Err(source) => {
                return Err(not_committed(MutationFailpoint::BackupRename, source));
            }
        }
        self.faults
            .check(MutationFailpoint::BackupDirectorySync, backup)
            .map_err(|source| not_committed(MutationFailpoint::BackupDirectorySync, source))?;
        sync_directory(&directory)
            .map_err(|source| not_committed(MutationFailpoint::BackupDirectorySync, source))
    }

    fn write_main(
        &self,
        path: &Path,
        encoded: &[u8],
        expected: &DestinationFingerprint,
    ) -> Result<CommitState, ConfigError> {
        let directory = ensure_parent(path)?;
        self.faults
            .check(MutationFailpoint::MainTempCreate, path)
            .map_err(|source| not_committed(MutationFailpoint::MainTempCreate, source))?;
        let (temp, mut file) = create_temp(&directory, "main")?;
        let cleanup = TempCleanup::new(temp.clone());
        self.faults
            .check(MutationFailpoint::MainWrite, path)
            .map_err(|source| not_committed(MutationFailpoint::MainWrite, source))?;
        file.write_all(encoded)
            .map_err(|source| not_committed(MutationFailpoint::MainWrite, source))?;
        file.flush()
            .map_err(|source| not_committed(MutationFailpoint::MainWrite, source))?;
        self.faults
            .check(MutationFailpoint::MainFileSync, path)
            .map_err(|source| not_committed(MutationFailpoint::MainFileSync, source))?;
        file.sync_all()
            .map_err(|source| not_committed(MutationFailpoint::MainFileSync, source))?;
        drop(file);

        self.faults
            .check(MutationFailpoint::MainPreRename, path)
            .map_err(|source| not_committed(MutationFailpoint::MainPreRename, source))?;
        if fingerprint(path)? != *expected {
            return Err(ConfigError::ExternalChange);
        }
        fs::rename(&temp, path)
            .map_err(|source| not_committed(MutationFailpoint::MainPreRename, source))?;
        cleanup.disarm();

        if self
            .faults
            .check(MutationFailpoint::MainPostRename, path)
            .is_err()
        {
            return Ok(CommitState::CommittedDurabilityUnknown);
        }
        if self
            .faults
            .check(MutationFailpoint::MainDirectorySync, path)
            .is_err()
            || sync_directory(&directory).is_err()
        {
            return Ok(CommitState::CommittedDurabilityUnknown);
        }
        Ok(CommitState::Committed)
    }
}

fn read_existing_backup(path: &Path) -> Result<Option<Vec<u8>>, ConfigError> {
    let link_metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(ConfigError::Io {
                path: path.to_owned(),
                source,
            });
        }
    };
    if !link_metadata.file_type().is_file() || !backup_mode_is_private(&link_metadata) {
        return Err(ConfigError::BackupConflict {
            path: path.to_owned(),
        });
    }
    let mut file = open_backup_no_follow(path).map_err(|source| ConfigError::Io {
        path: path.to_owned(),
        source,
    })?;
    let opened_metadata = file.metadata().map_err(|source| ConfigError::Io {
        path: path.to_owned(),
        source,
    })?;
    if !opened_metadata.file_type().is_file() || !backup_mode_is_private(&opened_metadata) {
        return Err(ConfigError::BackupConflict {
            path: path.to_owned(),
        });
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|source| ConfigError::Io {
            path: path.to_owned(),
            source,
        })?;
    Ok(Some(bytes))
}

#[cfg(unix)]
fn backup_mode_is_private(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt as _;

    metadata.permissions().mode() & 0o7777 == 0o600
}

#[cfg(not(unix))]
fn backup_mode_is_private(_metadata: &fs::Metadata) -> bool {
    true
}

#[cfg(unix)]
fn open_backup_no_follow(path: &Path) -> std::io::Result<fs::File> {
    use rustix::fs::{Mode, OFlags};

    rustix::fs::openat(
        rustix::fs::CWD,
        path,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map(fs::File::from)
    .map_err(std::io::Error::from)
}

#[cfg(not(unix))]
fn open_backup_no_follow(path: &Path) -> std::io::Result<fs::File> {
    fs::File::open(path)
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ConfigContract {
    pub read_versions: [u32; 3],
    pub write_version: u32,
    pub migration_backup_suffixes: MigrationBackupSuffixes,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct MigrationBackupSuffixes {
    #[serde(rename = "1")]
    pub v1: &'static str,
    #[serde(rename = "2")]
    pub v2: &'static str,
}

pub const fn config_contract() -> ConfigContract {
    ConfigContract {
        read_versions: [1, 2, 3],
        write_version: CURRENT_CONFIG_VERSION,
        migration_backup_suffixes: MigrationBackupSuffixes {
            v1: V1_MIGRATION_BACKUP_SUFFIX,
            v2: V2_MIGRATION_BACKUP_SUFFIX,
        },
    }
}

pub fn resolve_config_path(
    explicit: Option<&Path>,
    environment: Option<&OsStr>,
    home: Option<&OsStr>,
) -> Result<PathBuf, ConfigError> {
    if let Some(path) = explicit {
        return Ok(path.to_owned());
    }
    if let Some(path) = environment.filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    let home = home.ok_or(ConfigError::NoConfigPath)?;
    Ok(PathBuf::from(home).join(".config/dbotter/config.toml"))
}

pub fn config_path() -> Result<PathBuf, ConfigError> {
    resolve_config_path(
        None,
        std::env::var_os(CONFIG_ENV).as_deref(),
        std::env::var_os("HOME").as_deref(),
    )
}

pub fn load() -> Result<LoadedConfig, ConfigError> {
    load_path(&config_path()?)
}

pub fn load_path(path: &Path) -> Result<LoadedConfig, ConfigError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            return Ok(LoadedConfig {
                config: Config::default(),
                source_version: ConfigSourceVersion::Missing,
                migration_required: false,
                original_bytes: None,
                fingerprint: DestinationFingerprint(None),
            });
        }
        Err(source) => {
            return Err(ConfigError::Io {
                path: path.to_owned(),
                source,
            });
        }
    };
    let raw = std::str::from_utf8(&bytes).map_err(|_| ConfigError::InvalidUtf8)?;
    let header: VersionHeader = toml::from_str(raw)?;
    let (mut config, source_version, migration_required) = match header.version {
        1 => {
            let wire: ConfigV1 = toml::from_str(raw)?;
            (normalize_v1(wire), ConfigSourceVersion::V1, true)
        }
        2 => {
            reject_v3_posture_fields_in_v2(raw)?;
            let wire: ConfigV2Wire = toml::from_str(raw)?;
            (wire.into_config(), ConfigSourceVersion::V2, true)
        }
        CURRENT_CONFIG_VERSION => {
            validate_v3_document_shape(raw)?;
            let wire: ConfigV3Wire = toml::from_str(raw)?;
            let config = wire.into_config()?;
            validate_v3_profile_identities(&config)?;
            (config, ConfigSourceVersion::V3, false)
        }
        version => return Err(ConfigError::UnsupportedVersion(version)),
    };
    config.version = CURRENT_CONFIG_VERSION;
    Ok(LoadedConfig {
        config,
        source_version,
        migration_required,
        original_bytes: Some(bytes.clone()),
        fingerprint: DestinationFingerprint(Some(bytes)),
    })
}

pub fn mutate_path(
    path: &Path,
    mutation: ConfigMutation,
    consent: MigrationConsent,
) -> Result<MutationOutcome, ConfigError> {
    ConfigWriter::default().mutate_path(path, mutation, consent)
}

pub fn migration_backup_path(path: &Path) -> PathBuf {
    migration_backup_path_for_source(path, ConfigSourceVersion::V1)
}

pub fn migration_backup_path_for_source(path: &Path, source: ConfigSourceVersion) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(match source {
        ConfigSourceVersion::V1 => V1_MIGRATION_BACKUP_SUFFIX,
        ConfigSourceVersion::V2 => V2_MIGRATION_BACKUP_SUFFIX,
        ConfigSourceVersion::Missing | ConfigSourceVersion::V3 => V1_MIGRATION_BACKUP_SUFFIX,
    });
    PathBuf::from(value)
}

fn validate_v3_profile_identities(config: &Config) -> Result<(), ConfigError> {
    let mut instance_ids = HashSet::with_capacity(config.profiles.len());
    for profile in &config.profiles {
        let Some(instance_id) = profile.safety.instance_id() else {
            return Err(ConfigError::InvalidProfile);
        };
        if !instance_ids.insert(instance_id) {
            return Err(ConfigError::InvalidProfile);
        }
    }
    Ok(())
}

fn apply_mutation(
    config: &mut Config,
    mutation: ConfigMutation,
    profile_instance_ids: &dyn ProfileInstanceIdGenerator,
) -> Result<Option<String>, ConfigError> {
    match mutation {
        ConfigMutation::Create(mut profile) => {
            if config
                .profiles
                .iter()
                .any(|candidate| candidate.id == profile.id)
            {
                return Err(ConfigError::ProfileAlreadyExists(profile.id));
            }
            classify_new_profile(config, &mut profile, profile_instance_ids)?;
            let profile_id = profile.id.clone();
            config.profiles.push(profile);
            Ok(Some(profile_id))
        }
        ConfigMutation::CreateAuto {
            base_id,
            mut profile,
        } => {
            let profile_id = allocate_profile_id(&base_id, &config.profiles);
            profile.id.clone_from(&profile_id);
            classify_new_profile(config, &mut profile, profile_instance_ids)?;
            config.profiles.push(profile);
            Ok(Some(profile_id))
        }
        ConfigMutation::UpdateChecked {
            profile_id,
            expected_profile,
            mut profile,
        } => {
            if profile.id != profile_id || expected_profile.id != profile_id {
                return Err(ConfigError::ImmutableProfileId);
            }
            let existing = config
                .profiles
                .iter_mut()
                .find(|candidate| candidate.id == profile_id)
                .ok_or_else(|| ConfigError::ProfileMissing(profile_id.clone()))?;
            if *existing != expected_profile {
                return Err(ConfigError::ExternalChange);
            }
            profile.safety = profile
                .safety
                .preserve_classified_identity(expected_profile.safety)
                .ok_or(ConfigError::InvalidProfile)?;
            *existing = profile;
            Ok(Some(profile_id))
        }
        ConfigMutation::DeleteChecked {
            profile_id,
            expected_profile,
        } => {
            let Some(index) = config
                .profiles
                .iter()
                .position(|candidate| candidate.id == profile_id)
            else {
                return Err(ConfigError::ProfileMissing(profile_id));
            };
            if config.profiles[index] != expected_profile {
                return Err(ConfigError::ExternalChange);
            }
            config.profiles.remove(index);
            Ok(Some(profile_id))
        }
    }
}

fn classify_new_profile(
    config: &Config,
    profile: &mut ConnectionProfile,
    profile_instance_ids: &dyn ProfileInstanceIdGenerator,
) -> Result<(), ConfigError> {
    for _ in 0..PROFILE_INSTANCE_ID_GENERATION_ATTEMPTS {
        let candidate = profile_instance_ids
            .generate()
            .ok_or(ConfigError::EntropyUnavailable)?;
        if config
            .profiles
            .iter()
            .all(|existing| existing.safety.instance_id() != Some(candidate))
        {
            profile.safety = profile
                .safety
                .classify_new(candidate)
                .ok_or(ConfigError::InvalidProfile)?;
            return Ok(());
        }
    }
    Err(ConfigError::EntropyUnavailable)
}

fn allocate_profile_id(base_id: &str, profiles: &[ConnectionProfile]) -> String {
    if profiles.iter().all(|profile| profile.id != base_id) {
        return base_id.to_owned();
    }
    let mut suffix = 2_u64;
    loop {
        let candidate = format!("{base_id}-{suffix}");
        if profiles.iter().all(|profile| profile.id != candidate) {
            return candidate;
        }
        suffix = suffix.saturating_add(1);
    }
}

fn encode_config(config: &Config) -> Result<Vec<u8>, ConfigError> {
    let mut encoded = toml::to_string_pretty(config)?;
    encoded.push('\n');
    Ok(encoded.into_bytes())
}

fn fingerprint(path: &Path) -> Result<DestinationFingerprint, ConfigError> {
    match fs::read(path) {
        Ok(bytes) => Ok(DestinationFingerprint(Some(bytes))),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            Ok(DestinationFingerprint(None))
        }
        Err(source) => Err(ConfigError::Io {
            path: path.to_owned(),
            source,
        }),
    }
}

fn ensure_parent(path: &Path) -> Result<PathBuf, ConfigError> {
    let directory = path
        .parent()
        .filter(|value| !value.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(directory).map_err(|source| ConfigError::Io {
        path: directory.to_owned(),
        source,
    })?;
    Ok(directory.to_owned())
}

fn create_temp(directory: &Path, purpose: &str) -> Result<(PathBuf, fs::File), ConfigError> {
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temp = directory.join(format!(
        ".dbotter-config.{purpose}.tmp.{}.{}",
        std::process::id(),
        sequence
    ));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let file = options.open(&temp).map_err(|source| ConfigError::Io {
        path: temp.clone(),
        source,
    })?;
    Ok((temp, file))
}

#[cfg(unix)]
fn rename_no_replace(from: &Path, to: &Path) -> std::io::Result<()> {
    rustix::fs::renameat_with(
        rustix::fs::CWD,
        from,
        rustix::fs::CWD,
        to,
        rustix::fs::RenameFlags::NOREPLACE,
    )
    .map_err(std::io::Error::from)
}

#[cfg(not(unix))]
fn rename_no_replace(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::hard_link(from, to)?;
    fs::remove_file(from)
}

fn sync_directory(directory: &Path) -> std::io::Result<()> {
    fs::File::open(directory)?.sync_all()
}

fn not_committed(stage: MutationFailpoint, source: std::io::Error) -> ConfigError {
    ConfigError::NotCommitted { stage, source }
}

struct TempCleanup {
    path: PathBuf,
    armed: std::sync::atomic::AtomicBool,
}

impl TempCleanup {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            armed: std::sync::atomic::AtomicBool::new(true),
        }
    }

    fn disarm(&self) {
        self.armed.store(false, Ordering::Relaxed);
    }
}

impl Drop for TempCleanup {
    fn drop(&mut self) {
        if self.armed.load(Ordering::Relaxed) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[derive(Deserialize)]
struct VersionHeader {
    version: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MigrationPostureDocument {
    config_fingerprint: String,
    profiles: Vec<MigrationPostureAssignment>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MigrationPostureAssignment {
    profile_id: String,
    environment: MigrationEnvironment,
    access: MigrationAccess,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum MigrationEnvironment {
    Development,
    Production,
}

impl From<MigrationEnvironment> for ProfileEnvironment {
    fn from(environment: MigrationEnvironment) -> Self {
        match environment {
            MigrationEnvironment::Development => Self::Development,
            MigrationEnvironment::Production => Self::Production,
        }
    }
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum MigrationAccess {
    ReadWrite,
    ReadOnly,
}

impl From<MigrationAccess> for ProfileAccess {
    fn from(access: MigrationAccess) -> Self {
        match access {
            MigrationAccess::ReadWrite => Self::ReadWrite,
            MigrationAccess::ReadOnly => Self::ReadOnly,
        }
    }
}

struct ResolvedMigrationPosture {
    environment: ProfileEnvironment,
    access: ProfileAccess,
}

fn parse_migration_posture_document(bytes: &[u8]) -> Result<MigrationPostureDocument, ConfigError> {
    if bytes.len() > MIGRATION_DOCUMENT_MAX_BYTES {
        return Err(ConfigError::MigrationDocumentTooLarge {
            limit: MIGRATION_DOCUMENT_MAX_BYTES,
            actual: bytes.len(),
        });
    }
    let document: MigrationPostureDocument =
        serde_json::from_slice(bytes).map_err(|_| ConfigError::InvalidMigrationDocument)?;
    if !is_sha256_hex(&document.config_fingerprint) {
        return Err(ConfigError::InvalidMigrationDocument);
    }
    Ok(document)
}

fn migration_assignments(
    document: MigrationPostureDocument,
    config: &Config,
) -> Result<HashMap<String, ResolvedMigrationPosture>, ConfigError> {
    let expected = config
        .profiles
        .iter()
        .map(|profile| profile.id.as_str())
        .collect::<HashSet<_>>();
    let mut assignments = HashMap::with_capacity(document.profiles.len());
    for assignment in document.profiles {
        if !expected.contains(assignment.profile_id.as_str())
            || assignments
                .insert(
                    assignment.profile_id,
                    ResolvedMigrationPosture {
                        environment: assignment.environment.into(),
                        access: assignment.access.into(),
                    },
                )
                .is_some()
        {
            return Err(ConfigError::InvalidMigrationDocument);
        }
    }
    if assignments.len() != expected.len() {
        return Err(ConfigError::InvalidMigrationDocument);
    }
    Ok(assignments)
}

fn validate_legacy_migration_source(loaded: &LoadedConfig) -> Result<(), ConfigError> {
    let expected_source = match loaded.source_version {
        ConfigSourceVersion::V1 => LegacyConfigVersion::V1,
        ConfigSourceVersion::V2 => LegacyConfigVersion::V2,
        ConfigSourceVersion::Missing | ConfigSourceVersion::V3 => {
            return Err(ConfigError::MigrationPostureRequired);
        }
    };
    if !loaded.migration_required {
        return Err(ConfigError::MigrationPostureRequired);
    }
    crate::service::validate_config_identity(&loaded.config)
        .map_err(|_| ConfigError::InvalidProfile)?;
    for profile in &loaded.config.profiles {
        if !matches!(
            profile.safety,
            ProfileSafetyPosture::UnclassifiedLegacy { source }
                if source == expected_source
        ) {
            return Err(ConfigError::InvalidProfile);
        }
    }
    Ok(())
}

const fn migration_source_number(source: ConfigSourceVersion) -> Option<u32> {
    match source {
        ConfigSourceVersion::V1 => Some(1),
        ConfigSourceVersion::V2 => Some(2),
        ConfigSourceVersion::Missing | ConfigSourceVersion::V3 => None,
    }
}

fn migration_config_fingerprint(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn generate_unique_profile_instance_id(
    generator: &dyn ProfileInstanceIdGenerator,
    used: &mut HashSet<ProfileInstanceId>,
) -> Result<ProfileInstanceId, ConfigError> {
    for _ in 0..PROFILE_INSTANCE_ID_GENERATION_ATTEMPTS {
        let candidate = generator
            .generate()
            .ok_or(ConfigError::EntropyUnavailable)?;
        if used.insert(candidate) {
            return Ok(candidate);
        }
    }
    Err(ConfigError::EntropyUnavailable)
}

fn reject_v3_posture_fields_in_v2(raw: &str) -> Result<(), ConfigError> {
    let document: toml::Value = toml::from_str(raw)?;
    let Some(profiles) = document.get("profiles").and_then(toml::Value::as_array) else {
        return Ok(());
    };
    for profile in profiles {
        let Some(profile) = profile.as_table() else {
            continue;
        };
        if ["environment", "access", "instance_id"]
            .into_iter()
            .any(|field| profile.contains_key(field))
        {
            return Err(ConfigError::InvalidProfile);
        }
    }
    Ok(())
}

fn validate_v3_document_shape(raw: &str) -> Result<(), ConfigError> {
    const TOP_LEVEL_FIELDS: &[&str] = &["version", "profiles"];
    const PROFILE_FIELDS: &[&str] = &[
        "id",
        "name",
        "driver",
        "host",
        "port",
        "database",
        "username",
        "environment",
        "access",
        "instance_id",
        "tls",
        "credential_mode",
        "secret_env",
        "redis_tls",
    ];
    const REDIS_TLS_FIELDS: &[&str] = &["ca_file"];

    let document: toml::Value = toml::from_str(raw)?;
    let Some(document) = document.as_table() else {
        return Err(ConfigError::InvalidProfile);
    };
    if document
        .keys()
        .any(|field| !TOP_LEVEL_FIELDS.contains(&field.as_str()))
    {
        return Err(ConfigError::InvalidProfile);
    }

    let Some(profiles) = document.get("profiles").and_then(toml::Value::as_array) else {
        return Ok(());
    };
    for profile in profiles {
        let Some(profile) = profile.as_table() else {
            continue;
        };
        if profile
            .keys()
            .any(|field| !PROFILE_FIELDS.contains(&field.as_str()))
        {
            return Err(ConfigError::InvalidProfile);
        }
        if let Some(redis_tls) = profile.get("redis_tls").and_then(toml::Value::as_table)
            && redis_tls
                .keys()
                .any(|field| !REDIS_TLS_FIELDS.contains(&field.as_str()))
        {
            return Err(ConfigError::InvalidProfile);
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ConfigV2Wire {
    version: u32,
    #[serde(default)]
    profiles: Vec<ConnectionProfileV2Wire>,
}

impl ConfigV2Wire {
    fn from_config(config: &Config) -> Self {
        Self {
            version: 2,
            profiles: config
                .profiles
                .iter()
                .map(ConnectionProfileV2Wire::from_profile)
                .collect(),
        }
    }

    fn into_config(self) -> Config {
        Config {
            version: CURRENT_CONFIG_VERSION,
            profiles: self
                .profiles
                .into_iter()
                .map(ConnectionProfileV2Wire::into_profile)
                .collect(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ConnectionProfileV2Wire {
    id: String,
    name: String,
    driver: crate::model::DriverKind,
    #[serde(default = "default_host")]
    host: String,
    port: u16,
    #[serde(default)]
    database: Option<String>,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    tls: crate::model::TlsMode,
    credential_mode: CredentialMode,
    #[serde(default)]
    secret_env: Option<String>,
    #[serde(default, skip_serializing_if = "RedisTlsConfig::is_empty")]
    redis_tls: RedisTlsConfig,
}

impl ConnectionProfileV2Wire {
    fn from_profile(profile: &ConnectionProfile) -> Self {
        Self {
            id: profile.id.clone(),
            name: profile.name.clone(),
            driver: profile.driver,
            host: profile.host.clone(),
            port: profile.port,
            database: profile.database.clone(),
            username: profile.username.clone(),
            tls: profile.tls,
            credential_mode: profile.credential_mode,
            secret_env: profile.secret_env.clone(),
            redis_tls: profile.redis_tls.clone(),
        }
    }

    fn into_profile(self) -> ConnectionProfile {
        ConnectionProfile {
            id: self.id,
            name: self.name,
            driver: self.driver,
            host: self.host,
            port: self.port,
            database: self.database,
            username: self.username,
            safety: ProfileSafetyPosture::unclassified_legacy(LegacyConfigVersion::V2),
            tls: self.tls,
            credential_mode: self.credential_mode,
            secret_env: self.secret_env,
            redis_tls: self.redis_tls,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ConfigV3Wire {
    version: u32,
    #[serde(default)]
    profiles: Vec<ConnectionProfileV3Wire>,
}

impl ConfigV3Wire {
    fn try_from_config(config: &Config) -> Result<Self, &'static str> {
        Ok(Self {
            version: CURRENT_CONFIG_VERSION,
            profiles: config
                .profiles
                .iter()
                .map(ConnectionProfileV3Wire::try_from_profile)
                .collect::<Result<_, _>>()?,
        })
    }

    fn into_config(self) -> Result<Config, ConfigError> {
        Ok(Config {
            version: CURRENT_CONFIG_VERSION,
            profiles: self
                .profiles
                .into_iter()
                .map(ConnectionProfileV3Wire::into_profile)
                .collect::<Result<_, _>>()?,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ConnectionProfileV3Wire {
    id: String,
    name: String,
    driver: crate::model::DriverKind,
    #[serde(default = "default_host")]
    host: String,
    port: u16,
    #[serde(default)]
    database: Option<String>,
    #[serde(default)]
    username: Option<String>,
    #[serde(flatten)]
    safety: ProfileSafetyPostureV3Wire,
    #[serde(default)]
    tls: crate::model::TlsMode,
    credential_mode: CredentialMode,
    #[serde(default)]
    secret_env: Option<String>,
    #[serde(default, skip_serializing_if = "RedisTlsConfig::is_empty")]
    redis_tls: RedisTlsConfig,
}

impl ConnectionProfileV3Wire {
    fn try_from_profile(profile: &ConnectionProfile) -> Result<Self, &'static str> {
        let ProfileSafetyPosture::Classified {
            environment,
            access,
            instance_id,
        } = profile.safety
        else {
            return Err("version 3 profiles must have classified safety posture");
        };
        Ok(Self {
            id: profile.id.clone(),
            name: profile.name.clone(),
            driver: profile.driver,
            host: profile.host.clone(),
            port: profile.port,
            database: profile.database.clone(),
            username: profile.username.clone(),
            safety: ProfileSafetyPostureV3Wire {
                environment: Some(environment),
                access: Some(access),
                instance_id: Some(instance_id.to_string()),
            },
            tls: profile.tls,
            credential_mode: profile.credential_mode,
            secret_env: profile.secret_env.clone(),
            redis_tls: profile.redis_tls.clone(),
        })
    }

    fn into_profile(self) -> Result<ConnectionProfile, ConfigError> {
        let Some(environment) = self.safety.environment else {
            return Err(ConfigError::InvalidProfile);
        };
        let Some(access) = self.safety.access else {
            return Err(ConfigError::InvalidProfile);
        };
        let Some(instance_id) = self.safety.instance_id else {
            return Err(ConfigError::InvalidProfile);
        };
        let instance_id =
            ProfileInstanceId::parse(&instance_id).map_err(|_| ConfigError::InvalidProfile)?;
        Ok(ConnectionProfile {
            id: self.id,
            name: self.name,
            driver: self.driver,
            host: self.host,
            port: self.port,
            database: self.database,
            username: self.username,
            safety: ProfileSafetyPosture::classified(environment, access, instance_id),
            tls: self.tls,
            credential_mode: self.credential_mode,
            secret_env: self.secret_env,
            redis_tls: self.redis_tls,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProfileSafetyPostureV3Wire {
    #[serde(default)]
    environment: Option<ProfileEnvironment>,
    #[serde(default)]
    access: Option<ProfileAccess>,
    #[serde(default)]
    instance_id: Option<String>,
}

#[derive(Deserialize)]
struct ConfigV1 {
    #[allow(dead_code)]
    version: u32,
    #[serde(default)]
    profiles: Vec<ConnectionProfileV1>,
}

#[derive(Deserialize)]
struct ConnectionProfileV1 {
    id: String,
    name: String,
    driver: crate::model::DriverKind,
    #[serde(default = "default_host")]
    host: String,
    port: u16,
    #[serde(default)]
    database: Option<String>,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    tls: crate::model::TlsMode,
    #[serde(default)]
    secret_env: Option<String>,
}

fn default_host() -> String {
    "127.0.0.1".to_owned()
}

fn normalize_v1(wire: ConfigV1) -> Config {
    Config {
        version: CURRENT_CONFIG_VERSION,
        profiles: wire
            .profiles
            .into_iter()
            .map(|profile| {
                let credential_mode = if profile.secret_env.is_some() {
                    CredentialMode::Environment
                } else {
                    CredentialMode::None
                };
                ConnectionProfile {
                    id: profile.id,
                    name: profile.name,
                    driver: profile.driver,
                    host: profile.host,
                    port: profile.port,
                    database: profile.database,
                    username: profile.username,
                    safety: ProfileSafetyPosture::unclassified_legacy(LegacyConfigVersion::V1),
                    tls: profile.tls,
                    credential_mode,
                    secret_env: profile.secret_env,
                    redis_tls: RedisTlsConfig::default(),
                }
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EMPTY_V3_BYTES: &[u8] = b"version = 3\nprofiles = []\n";

    struct OneFault(MutationFailpoint);

    impl MutationFaultInjector for OneFault {
        fn check(&self, point: MutationFailpoint, _path: &Path) -> std::io::Result<()> {
            if point == self.0 {
                Err(std::io::Error::other("injected"))
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn every_precommit_main_failpoint_preserves_original_and_cleans_temp() {
        for point in [
            MutationFailpoint::MainTempCreate,
            MutationFailpoint::MainWrite,
            MutationFailpoint::MainFileSync,
            MutationFailpoint::MainPreRename,
        ] {
            let directory = tempfile::tempdir().expect("tempdir");
            let path = directory.path().join("config.toml");
            let original = EMPTY_V3_BYTES;
            fs::write(&path, original).expect("fixture");
            let writer = ConfigWriter::with_fault_injector(Arc::new(OneFault(point)));
            let result = writer.mutate_path(
                &path,
                ConfigMutation::Create(fixture_profile("new")),
                MigrationConsent::Confirmed,
            );
            assert!(result.is_err(), "{point:?}");
            assert_eq!(fs::read(&path).expect("main"), original, "{point:?}");
            assert_eq!(temp_count(directory.path()), 0, "{point:?}");
        }
    }

    #[test]
    fn post_rename_failpoints_report_durability_unknown_and_reload_new_bytes() {
        for point in [
            MutationFailpoint::MainPostRename,
            MutationFailpoint::MainDirectorySync,
        ] {
            let directory = tempfile::tempdir().expect("tempdir");
            let path = directory.path().join("config.toml");
            let writer = ConfigWriter::with_fault_injector(Arc::new(OneFault(point)));
            let outcome = writer
                .mutate_path(
                    &path,
                    ConfigMutation::Create(fixture_profile("new")),
                    MigrationConsent::Confirmed,
                )
                .expect("observed commit");
            assert_eq!(outcome.state, CommitState::CommittedDurabilityUnknown);
            assert_eq!(load_path(&path).expect("reload").config.profiles.len(), 1);
            assert_eq!(temp_count(directory.path()), 0);
        }
    }

    #[test]
    fn every_backup_failpoint_aborts_before_main_and_cleans_temp() {
        for point in [
            MutationFailpoint::BackupTempCreate,
            MutationFailpoint::BackupWrite,
            MutationFailpoint::BackupFileSync,
            MutationFailpoint::BackupRename,
            MutationFailpoint::BackupDirectorySync,
        ] {
            let directory = tempfile::tempdir().expect("tempdir");
            let path = directory.path().join("config.toml");
            let original = b"version = 1\n";
            fs::write(&path, original).expect("fixture");
            let writer = ConfigWriter::with_fault_injector(Arc::new(OneFault(point)));
            let posture_document = empty_v1_posture_document(&writer, &path, original);
            let result = writer.migrate_v3(&path, &posture_document);
            assert!(result.is_err(), "{point:?}");
            assert_eq!(fs::read(&path).expect("main"), original, "{point:?}");
            assert_eq!(temp_count(directory.path()), 0, "{point:?}");
        }
    }

    #[test]
    fn no_replace_backup_race_accepts_identical_and_rejects_different_bytes() {
        for (raced_bytes, succeeds) in [
            (b"version = 1\n".as_slice(), true),
            (b"different\n".as_slice(), false),
        ] {
            let directory = tempfile::tempdir().expect("tempdir");
            let path = directory.path().join("config.toml");
            let original = b"version = 1\n";
            fs::write(&path, original).expect("fixture");
            let writer = ConfigWriter::with_fault_injector(Arc::new(BackupRace {
                bytes: raced_bytes.to_vec(),
            }));
            let posture_document = empty_v1_posture_document(&writer, &path, original);
            let result = writer.migrate_v3(&path, &posture_document);
            assert_eq!(result.is_ok(), succeeds);
            assert_eq!(
                fs::read(migration_backup_path(&path)).expect("raced backup"),
                raced_bytes
            );
            if !succeeds {
                assert_eq!(fs::read(&path).expect("main"), original);
            }
            assert_eq!(temp_count(directory.path()), 0);
        }
    }

    #[test]
    fn fingerprint_recheck_rejects_injected_external_content() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("config.toml");
        let original = EMPTY_V3_BYTES;
        let external = b"version = 3\n# external writer\nprofiles = []\n";
        fs::write(&path, original).expect("fixture");
        let writer = ConfigWriter::with_fault_injector(Arc::new(ExternalChange {
            bytes: external.to_vec(),
        }));

        let result = writer.mutate_path(
            &path,
            ConfigMutation::Create(fixture_profile("new")),
            MigrationConsent::Confirmed,
        );

        assert!(matches!(result, Err(ConfigError::ExternalChange)));
        assert_eq!(fs::read(&path).expect("external bytes"), external);
        assert_eq!(temp_count(directory.path()), 0);
    }

    struct BackupRace {
        bytes: Vec<u8>,
    }

    impl MutationFaultInjector for BackupRace {
        fn check(&self, point: MutationFailpoint, path: &Path) -> std::io::Result<()> {
            if point == MutationFailpoint::BackupRename {
                fs::write(path, &self.bytes)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt as _;
                    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
                }
            }
            Ok(())
        }
    }

    struct ExternalChange {
        bytes: Vec<u8>,
    }

    impl MutationFaultInjector for ExternalChange {
        fn check(&self, point: MutationFailpoint, path: &Path) -> std::io::Result<()> {
            if point == MutationFailpoint::MainPreRename {
                fs::write(path, &self.bytes)?;
            }
            Ok(())
        }
    }

    fn fixture_profile(id: &str) -> ConnectionProfile {
        ConnectionProfile {
            id: id.to_owned(),
            name: id.to_owned(),
            driver: crate::model::DriverKind::Redis,
            host: "127.0.0.1".to_owned(),
            port: 6379,
            database: None,
            username: None,
            safety: ProfileSafetyPosture::new(
                ProfileEnvironment::Development,
                ProfileAccess::ReadWrite,
            ),
            tls: crate::model::TlsMode::Disabled,
            credential_mode: CredentialMode::None,
            secret_env: None,
            redis_tls: RedisTlsConfig::default(),
        }
    }

    fn empty_v1_posture_document(writer: &ConfigWriter, path: &Path, source: &[u8]) -> Vec<u8> {
        let plan = writer
            .migration_plan(path)
            .expect("empty v1 migration plan");
        assert_eq!(plan.source_version, 1);
        assert_eq!(
            plan.config_fingerprint,
            migration_config_fingerprint(source)
        );
        assert!(plan.profiles.is_empty());
        serde_json::to_vec(&serde_json::json!({
            "config_fingerprint": plan.config_fingerprint,
            "profiles": []
        }))
        .expect("empty v1 posture document")
    }

    fn temp_count(directory: &Path) -> usize {
        fs::read_dir(directory)
            .expect("read dir")
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp."))
            .count()
    }
}
