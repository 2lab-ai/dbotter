use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use serde::{Deserialize, Serialize};

use crate::model::{ConnectionProfile, CredentialMode, RedisTlsConfig};

pub const CONFIG_ENV: &str = "DBOTTER_CONFIG";
pub const CURRENT_CONFIG_VERSION: u32 = 2;
pub const MIGRATION_BACKUP_SUFFIX: &str = ".v1.bak";

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
            Self::BackupConflict { .. } => formatter.write_str("BackupConflict(<redacted>)"),
            Self::ProfileAlreadyExists(_) => {
                formatter.write_str("ProfileAlreadyExists(<redacted>)")
            }
            Self::ProfileMissing(_) => formatter.write_str("ProfileMissing(<redacted>)"),
            Self::ImmutableProfileId => formatter.write_str("ImmutableProfileId"),
            Self::ExternalChange => formatter.write_str("ExternalChange"),
            Self::InvalidProfile => formatter.write_str("InvalidProfile"),
            Self::NotCommitted { stage, .. } => formatter
                .debug_struct("NotCommitted")
                .field("stage", stage)
                .field("source", &"<redacted>")
                .finish(),
            Self::WriterUnavailable => formatter.write_str("WriterUnavailable"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Config {
    pub version: u32,
    #[serde(default)]
    pub profiles: Vec<ConnectionProfile>,
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
}

impl PostCommitObservationError {
    pub const fn commit_state(&self) -> CommitState {
        self.commit_state
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
}

impl Default for ConfigWriter {
    fn default() -> Self {
        Self {
            faults: Arc::new(NoFaults),
        }
    }
}

impl ConfigWriter {
    pub fn with_fault_injector(faults: Arc<dyn MutationFaultInjector>) -> Self {
        Self { faults }
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
        crate::service::validate_config_identity(&loaded.config)
            .map_err(|_| ConfigError::InvalidProfile)?;
        crate::service::validate_config_mutation(&mutation)
            .map_err(|_| ConfigError::InvalidProfile)?;
        let backup = self.prepare_migration(path, &loaded, consent)?;
        let mut config = loaded.config.clone();
        let affected_profile_id = apply_mutation(&mut config, mutation)?;
        config.version = CURRENT_CONFIG_VERSION;
        crate::service::validate_config_identity(&config)
            .map_err(|_| ConfigError::InvalidProfile)?;
        let encoded = encode_config(&config)?;
        let mut state = self.write_main(path, &encoded, &loaded.fingerprint)?;
        let observation = match self.observe_main(path) {
            Ok(observed) => PostCommitObservation::Observed(observed),
            Err(source) => {
                state = CommitState::CommittedDurabilityUnknown;
                PostCommitObservation::Failed(PostCommitObservationError {
                    commit_state: state,
                    source,
                })
            }
        };
        Ok(MutationOutcome {
            state,
            observation,
            migration_backup: backup,
            affected_profile_id,
        })
    }

    fn observe_main(&self, path: &Path) -> Result<LoadedConfig, ConfigError> {
        self.faults
            .check(MutationFailpoint::MainObservationLoad, path)
            .map_err(|source| ConfigError::Io {
                path: path.to_owned(),
                source,
            })?;
        let loaded = load_path(path)?;
        crate::service::validate_config_identity(&loaded.config)
            .map_err(|_| ConfigError::InvalidProfile)?;
        if loaded.source_version != ConfigSourceVersion::V2 {
            return Err(ConfigError::ExternalChange);
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
        let backup = migration_backup_path(path);
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
    pub read_versions: [u32; 2],
    pub write_version: u32,
    pub migration_backup_suffix: &'static str,
}

pub const fn config_contract() -> ConfigContract {
    ConfigContract {
        read_versions: [1, 2],
        write_version: CURRENT_CONFIG_VERSION,
        migration_backup_suffix: MIGRATION_BACKUP_SUFFIX,
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
    let (config, source_version, migration_required) = match header.version {
        1 => {
            let wire: ConfigV1 = toml::from_str(raw)?;
            (normalize_v1(wire), ConfigSourceVersion::V1, true)
        }
        CURRENT_CONFIG_VERSION => {
            let config: Config = toml::from_str(raw)?;
            (config, ConfigSourceVersion::V2, false)
        }
        version => return Err(ConfigError::UnsupportedVersion(version)),
    };
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
    let mut value = path.as_os_str().to_os_string();
    value.push(MIGRATION_BACKUP_SUFFIX);
    PathBuf::from(value)
}

fn apply_mutation(
    config: &mut Config,
    mutation: ConfigMutation,
) -> Result<Option<String>, ConfigError> {
    match mutation {
        ConfigMutation::Create(profile) => {
            if config
                .profiles
                .iter()
                .any(|candidate| candidate.id == profile.id)
            {
                return Err(ConfigError::ProfileAlreadyExists(profile.id));
            }
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
            config.profiles.push(profile);
            Ok(Some(profile_id))
        }
        ConfigMutation::UpdateChecked {
            profile_id,
            expected_profile,
            profile,
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
            let original = b"version = 2\nprofiles = []\n";
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
            let result = writer.mutate_path(
                &path,
                ConfigMutation::Create(fixture_profile("new")),
                MigrationConsent::Confirmed,
            );
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
        let original = b"version = 2\nprofiles = []\n";
        let external = b"version = 2\n# external writer\nprofiles = []\n";
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
            tls: crate::model::TlsMode::Disabled,
            credential_mode: CredentialMode::None,
            secret_env: None,
            redis_tls: RedisTlsConfig::default(),
        }
    }

    fn temp_count(directory: &Path) -> usize {
        fs::read_dir(directory)
            .expect("read dir")
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp."))
            .count()
    }
}
