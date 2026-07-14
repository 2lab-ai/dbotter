use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use dbotter::config::{
    CommitState, ConfigError, ConfigMutation, ConfigSourceVersion, ConfigWriter, MigrationConsent,
    MutationFailpoint, MutationFaultInjector, PostCommitObservation, config_contract, load_path,
    migration_backup_path, mutate_path, resolve_config_path,
};
use dbotter::model::{ConnectionProfile, CredentialMode, DriverKind, RedisTlsConfig, TlsMode};

#[path = "fixtures/f665438_v1_reader.rs"]
mod f665438_v1_reader;

const V1_BYTES: &[u8] = br#"version = 1

[[profiles]]
id = "redis-local"
name = "Redis local"
driver = "redis"
host = "127.0.0.1"
port = 6379
tls = "disabled"
secret_env = "DBOTTER_REDIS_PASSWORD"
"#;

#[test]
fn explicit_config_path_has_exact_precedence() {
    let explicit = Path::new("/explicit/dbotter.toml");
    let resolved = resolve_config_path(
        Some(explicit),
        Some(OsStr::new("/environment/dbotter.toml")),
        Some(OsStr::new("/home/example")),
    )
    .expect("path resolves");

    assert_eq!(resolved, explicit);
}

#[test]
fn v1_normalizes_read_only_and_missing_is_empty_v2() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    fs::write(&path, V1_BYTES).expect("fixture writes");

    let loaded = load_path(&path).expect("v1 loads");
    assert_eq!(loaded.source_version, ConfigSourceVersion::V1);
    assert!(loaded.migration_required);
    assert_eq!(loaded.original_bytes.as_deref(), Some(V1_BYTES));
    assert_eq!(loaded.config.version, 2);
    assert_eq!(
        loaded.config.profiles[0].credential_mode,
        CredentialMode::Environment
    );
    assert_eq!(fs::read(&path).expect("read back"), V1_BYTES);

    let missing = load_path(&directory.path().join("missing.toml")).expect("missing is valid");
    assert_eq!(missing.source_version, ConfigSourceVersion::Missing);
    assert!(!missing.migration_required);
    assert_eq!(missing.config.version, 2);
    assert!(missing.config.profiles.is_empty());
}

#[test]
fn v1_without_secret_is_none_but_v2_requires_an_explicit_credential_mode() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    fs::write(
        &path,
        "version = 1\n[[profiles]]\nid = \"plain\"\nname = \"Plain\"\ndriver = \"redis\"\nport = 6379\ntls = \"disabled\"\n",
    )
    .expect("v1 fixture");
    assert_eq!(
        load_path(&path).expect("v1 load").config.profiles[0].credential_mode,
        CredentialMode::None
    );

    fs::write(
        &path,
        "version = 2\n[[profiles]]\nid = \"plain\"\nname = \"Plain\"\ndriver = \"redis\"\nport = 6379\ntls = \"disabled\"\n",
    )
    .expect("v2 fixture");
    assert!(matches!(load_path(&path), Err(ConfigError::Parse(_))));

    fs::write(&path, "version = 3\n").expect("unsupported fixture");
    assert!(matches!(
        load_path(&path),
        Err(ConfigError::UnsupportedVersion(3))
    ));
}

#[test]
fn first_confirmed_mutation_preserves_exact_v1_backup_and_writes_v2() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    fs::write(&path, V1_BYTES).expect("fixture writes");

    let cancelled = mutate_path(
        &path,
        ConfigMutation::Create(profile("cancelled")),
        MigrationConsent::Cancelled,
    );
    assert!(matches!(
        cancelled,
        Err(ConfigError::MigrationConfirmationRequired { .. })
    ));
    assert_eq!(fs::read(&path).expect("main unchanged"), V1_BYTES);
    assert!(!migration_backup_path(&path).exists());

    let outcome = mutate_path(
        &path,
        ConfigMutation::Create(profile("created")),
        MigrationConsent::Confirmed,
    )
    .expect("confirmed mutation commits");
    assert_eq!(outcome.state, CommitState::Committed);
    assert_eq!(outcome.migration_backup, Some(migration_backup_path(&path)));
    assert_eq!(
        fs::read(migration_backup_path(&path)).expect("backup bytes"),
        V1_BYTES
    );
    assert_eq!(
        load_path(&path).expect("v2 reload").source_version,
        ConfigSourceVersion::V2
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            fs::metadata(migration_backup_path(&path))
                .expect("backup metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(&path)
                .expect("main metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}

#[test]
fn invalid_create_auto_base_id_is_rejected_before_v1_backup_or_main_side_effect() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    fs::write(&path, V1_BYTES).expect("v1 fixture writes");
    let mutation = ConfigMutation::CreateAuto {
        base_id: " invalid-auto-base".to_owned(),
        profile: profile("valid-destination"),
    };

    let result = ConfigWriter::default().mutate_path(&path, mutation, MigrationConsent::Confirmed);

    assert!(matches!(result, Err(ConfigError::InvalidProfile)));
    assert_eq!(fs::read(&path).expect("main unchanged"), V1_BYTES);
    assert!(!migration_backup_path(&path).exists());
    assert_eq!(
        fs::read_dir(directory.path())
            .expect("directory")
            .filter_map(Result::ok)
            .count(),
        1,
        "validation occurs before backup/temp creation"
    );
}

#[test]
fn identical_backup_retry_repeats_parent_sync_before_main_commit() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    fs::write(&path, V1_BYTES).expect("v1 fixture");
    let fault = Arc::new(AlwaysFailBackupDirectorySync::default());
    let writer = ConfigWriter::with_fault_injector(fault.clone());

    for attempt in 1..=2 {
        let result = writer.mutate_path(
            &path,
            ConfigMutation::Create(profile("must-not-commit")),
            MigrationConsent::Confirmed,
        );
        assert!(
            matches!(
                result,
                Err(ConfigError::NotCommitted {
                    stage: MutationFailpoint::BackupDirectorySync,
                    ..
                })
            ),
            "attempt {attempt} must repeat the backup directory sync"
        );
        assert_eq!(fs::read(&path).expect("main bytes"), V1_BYTES);
        assert_eq!(
            fs::read(migration_backup_path(&path)).expect("identical backup remains"),
            V1_BYTES
        );
    }
    assert_eq!(fault.checks.load(Ordering::SeqCst), 2);
}

#[cfg(unix)]
#[test]
fn identical_existing_backup_must_be_a_regular_exactly_0600_file() {
    use std::os::unix::fs::{PermissionsExt as _, symlink};

    for invalid in ["0644", "symlink", "directory"] {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("config.toml");
        let backup = migration_backup_path(&path);
        fs::write(&path, V1_BYTES).expect("v1 fixture");
        match invalid {
            "0644" => {
                fs::write(&backup, V1_BYTES).expect("identical backup");
                fs::set_permissions(&backup, fs::Permissions::from_mode(0o644))
                    .expect("world-readable mode");
            }
            "symlink" => {
                let target = directory.path().join("backup-target");
                fs::write(&target, V1_BYTES).expect("target");
                fs::set_permissions(&target, fs::Permissions::from_mode(0o600))
                    .expect("target mode");
                symlink(target, &backup).expect("backup symlink");
            }
            "directory" => fs::create_dir(&backup).expect("backup directory"),
            _ => unreachable!(),
        }

        let result = mutate_path(
            &path,
            ConfigMutation::Create(profile("must-not-commit")),
            MigrationConsent::Confirmed,
        );

        assert!(
            matches!(result, Err(ConfigError::BackupConflict { .. })),
            "{invalid}"
        );
        assert_eq!(
            fs::read(&path).expect("main unchanged"),
            V1_BYTES,
            "{invalid}"
        );
    }

    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    let backup = migration_backup_path(&path);
    fs::write(&path, V1_BYTES).expect("v1 fixture");
    fs::write(&backup, V1_BYTES).expect("identical backup");
    fs::set_permissions(&backup, fs::Permissions::from_mode(0o600)).expect("private mode");

    mutate_path(
        &path,
        ConfigMutation::Create(profile("committed")),
        MigrationConsent::Confirmed,
    )
    .expect("private regular identical backup is accepted");
    assert_eq!(fs::read(&backup).expect("backup unchanged"), V1_BYTES);
    assert_eq!(
        fs::metadata(&backup)
            .expect("backup metadata")
            .permissions()
            .mode()
            & 0o7777,
        0o600
    );
}

#[derive(Default)]
struct AlwaysFailBackupDirectorySync {
    checks: AtomicUsize,
}

impl MutationFaultInjector for AlwaysFailBackupDirectorySync {
    fn check(&self, point: MutationFailpoint, _path: &Path) -> std::io::Result<()> {
        if point == MutationFailpoint::BackupDirectorySync {
            self.checks.fetch_add(1, Ordering::SeqCst);
            Err(std::io::Error::other(
                "injected backup directory sync failure",
            ))
        } else {
            Ok(())
        }
    }
}

#[test]
fn backup_is_no_replace_and_create_update_delete_are_distinct() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    fs::write(&path, V1_BYTES).expect("fixture writes");
    fs::write(migration_backup_path(&path), b"different").expect("conflicting backup");

    let conflict = mutate_path(
        &path,
        ConfigMutation::Create(profile("created")),
        MigrationConsent::Confirmed,
    );
    assert!(matches!(conflict, Err(ConfigError::BackupConflict { .. })));
    assert_eq!(fs::read(&path).expect("main unchanged"), V1_BYTES);

    fs::remove_file(migration_backup_path(&path)).expect("remove conflict");
    mutate_path(
        &path,
        ConfigMutation::Create(profile("created")),
        MigrationConsent::Confirmed,
    )
    .expect("create");
    assert!(matches!(
        mutate_path(
            &path,
            ConfigMutation::Create(profile("created")),
            MigrationConsent::Confirmed,
        ),
        Err(ConfigError::ProfileAlreadyExists(_))
    ));

    let mut edited = profile("created");
    edited.port = 16379;
    let expected = load_path(&path).expect("before update").config.profiles[1].clone();
    mutate_path(
        &path,
        ConfigMutation::UpdateChecked {
            profile_id: "created".into(),
            expected_profile: expected,
            profile: edited,
        },
        MigrationConsent::Confirmed,
    )
    .expect("update");
    assert_eq!(
        load_path(&path).expect("reload").config.profiles[1].port,
        16379
    );

    let expected = load_path(&path).expect("before delete").config.profiles[1].clone();
    mutate_path(
        &path,
        ConfigMutation::DeleteChecked {
            profile_id: "created".into(),
            expected_profile: expected,
        },
        MigrationConsent::Confirmed,
    )
    .expect("delete");
    assert!(
        load_path(&path)
            .expect("reload")
            .config
            .profiles
            .iter()
            .all(|profile| profile.id != "created")
    );
}

#[test]
fn post_rename_observation_failure_preserves_commit_classification_and_redacts_paths() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("sentinel-secret-config-name.toml");
    fs::write(&path, V1_BYTES).expect("v1 fixture");
    let writer = ConfigWriter::with_fault_injector(Arc::new(FailObservation));

    let outcome = writer
        .mutate_path(
            &path,
            ConfigMutation::Create(profile("committed")),
            MigrationConsent::Confirmed,
        )
        .expect("rename commit is returned as a typed observation outcome");

    assert_eq!(outcome.state, CommitState::CommittedDurabilityUnknown);
    let PostCommitObservation::Failed(error) = &outcome.observation else {
        panic!("observation must fail through the injected load seam");
    };
    assert_eq!(
        error.commit_state(),
        CommitState::CommittedDurabilityUnknown
    );
    assert!(outcome.migration_backup.is_some());
    assert!(
        load_path(&path)
            .expect("committed file remains readable")
            .config
            .profiles
            .iter()
            .any(|profile| profile.id == "committed")
    );
    let debug = format!("{outcome:?}");
    assert!(!debug.contains("sentinel-secret-config-name.toml"));
}

struct FailObservation;

impl MutationFaultInjector for FailObservation {
    fn check(&self, point: MutationFailpoint, _path: &Path) -> std::io::Result<()> {
        if point == MutationFailpoint::MainObservationLoad {
            Err(std::io::Error::other("injected observation failure"))
        } else {
            Ok(())
        }
    }
}

#[test]
fn config_contract_json_is_exact_and_v1_reader_rejects_v2() {
    assert_eq!(
        serde_json::to_value(config_contract()).expect("serialize contract"),
        serde_json::json!({
            "read_versions": [1, 2],
            "write_version": 2,
            "migration_backup_suffix": ".v1.bak"
        })
    );

    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    mutate_path(
        &path,
        ConfigMutation::Create(profile("current")),
        MigrationConsent::Confirmed,
    )
    .expect("write v2");
    let service_constructions = AtomicUsize::new(0);
    let network_acquisitions = AtomicUsize::new(0);
    let result = f665438_v1_reader::load_before_service_or_network(
        path,
        || {
            service_constructions.fetch_add(1, Ordering::SeqCst);
        },
        || {
            network_acquisitions.fetch_add(1, Ordering::SeqCst);
        },
    );
    assert_eq!(
        result,
        Err(f665438_v1_reader::FrozenReaderError::UnsupportedVersion(2))
    );
    assert_eq!(service_constructions.load(Ordering::SeqCst), 0);
    assert_eq!(network_acquisitions.load(Ordering::SeqCst), 0);
}

#[test]
fn config_error_debug_and_display_redact_paths_and_sources() {
    let path = PathBuf::from("/sentinel/secret-config-path.toml");
    let errors = [
        ConfigError::Io {
            path: path.clone(),
            source: std::io::Error::other("raw-source-secret-sentinel"),
        },
        ConfigError::MigrationConfirmationRequired {
            backup: path.clone(),
        },
        ConfigError::BackupConflict { path },
        ConfigError::NotCommitted {
            stage: MutationFailpoint::MainWrite,
            source: std::io::Error::other("raw-source-secret-sentinel"),
        },
    ];

    for error in errors {
        let debug = format!("{error:?}");
        let display = error.to_string();
        for sentinel in ["secret-config-path", "raw-source-secret-sentinel"] {
            assert!(!debug.contains(sentinel), "debug={debug}");
            assert!(!display.contains(sentinel), "display={display}");
        }
    }
}

fn profile(id: &str) -> ConnectionProfile {
    ConnectionProfile {
        id: id.to_owned(),
        name: id.to_owned(),
        driver: DriverKind::Redis,
        host: "127.0.0.1".to_owned(),
        port: 6379,
        database: None,
        username: None,
        tls: TlsMode::Disabled,
        credential_mode: CredentialMode::None,
        secret_env: None,
        redis_tls: RedisTlsConfig::default(),
    }
}

#[allow(dead_code)]
fn assert_path(_: PathBuf) {}
