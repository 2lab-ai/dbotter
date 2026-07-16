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
use dbotter::model::{
    ConnectionProfile, CredentialMode, DriverKind, LegacyConfigVersion, ProfileAccess,
    ProfileEnvironment, ProfileInstanceId, ProfileSafetyPosture, RedisTlsConfig, TlsMode,
};
use sha2::{Digest as _, Sha256};

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

const EMPTY_V3_BYTES: &[u8] = b"version = 3\nprofiles = []\n";

const OBSERVATION_PROFILE_A: &str = "migration-observation-profile-a-sentinel";
const OBSERVATION_PROFILE_B: &str = "migration-observation-profile-b-sentinel";
const OBSERVATION_REWRITE_V1_BYTES: &[u8] = br#"version = 1

[[profiles]]
id = "migration-observation-profile-a-sentinel"
name = "Observation profile A"
driver = "mysql"
host = "mysql-a.internal"
port = 3306
tls = "disabled"

[[profiles]]
id = "migration-observation-profile-b-sentinel"
name = "Observation profile B"
driver = "redis"
host = "redis-b.internal"
port = 6379
tls = "disabled"
"#;
const REWRITTEN_INSTANCE_A: ProfileInstanceId = ProfileInstanceId::from_bytes([0xa1; 16]);
const REWRITTEN_INSTANCE_B: ProfileInstanceId = ProfileInstanceId::from_bytes([0xb2; 16]);

const SEMANTIC_INVALID_PROFILE_ID: &str = "semantic-invalid-v1-profile";
const SEMANTIC_INVALID_V1_BYTES: &[u8] = br#"version = 1

[[profiles]]
id = "semantic-invalid-v1-profile"
name = "Semantic invalid v1 profile"
driver = "mysql"
host = ""
port = 3306
tls = "disabled"
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
fn v1_normalizes_read_only_and_missing_is_empty_v3() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    fs::write(&path, V1_BYTES).expect("fixture writes");

    let loaded = load_path(&path).expect("v1 loads");
    assert_eq!(loaded.source_version, ConfigSourceVersion::V1);
    assert!(loaded.migration_required);
    assert_eq!(loaded.original_bytes.as_deref(), Some(V1_BYTES));
    assert_eq!(loaded.config.version, 3);
    assert!(matches!(
        loaded.config.profiles[0].safety,
        ProfileSafetyPosture::UnclassifiedLegacy {
            source: LegacyConfigVersion::V1
        }
    ));
    assert_eq!(
        loaded.config.profiles[0].safety.effective_access(),
        ProfileAccess::ReadOnly
    );
    assert_eq!(
        loaded.config.profiles[0].credential_mode,
        CredentialMode::Environment
    );
    assert_eq!(fs::read(&path).expect("read back"), V1_BYTES);

    let missing = load_path(&directory.path().join("missing.toml")).expect("missing is valid");
    assert_eq!(missing.source_version, ConfigSourceVersion::Missing);
    assert!(!missing.migration_required);
    assert_eq!(missing.config.version, 3);
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

    fs::write(&path, "version = 4\n").expect("unsupported fixture");
    assert!(matches!(
        load_path(&path),
        Err(ConfigError::UnsupportedVersion(4))
    ));
}

#[test]
fn explicit_v1_migration_preserves_exact_backup_and_writes_v3() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    fs::write(&path, V1_BYTES).expect("fixture writes");
    let writer = ConfigWriter::default();
    let posture_document = v1_posture_document(&writer, &path);

    assert_eq!(fs::read(&path).expect("planning is read-only"), V1_BYTES);
    assert!(!migration_backup_path(&path).exists());

    let legacy_crud = writer.mutate_path(
        &path,
        ConfigMutation::Create(profile("cancelled")),
        MigrationConsent::Cancelled,
    );
    assert!(matches!(
        legacy_crud,
        Err(ConfigError::MigrationPostureRequired)
    ));
    assert_eq!(fs::read(&path).expect("main unchanged"), V1_BYTES);
    assert!(!migration_backup_path(&path).exists());

    let outcome = writer
        .migrate_v3(&path, &posture_document)
        .expect("explicit migration commits");
    assert_eq!(outcome.state, CommitState::Committed);
    assert_eq!(outcome.migration_backup, Some(migration_backup_path(&path)));
    assert_eq!(
        fs::read(migration_backup_path(&path)).expect("backup bytes"),
        V1_BYTES
    );
    assert_eq!(
        load_path(&path).expect("v3 reload").source_version,
        ConfigSourceVersion::V3
    );
    assert!(matches!(
        load_path(&path).expect("classified reload").config.profiles[0].safety,
        ProfileSafetyPosture::Classified {
            environment: ProfileEnvironment::Production,
            access: ProfileAccess::ReadOnly,
            ..
        }
    ));

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
fn identity_valid_semantic_invalid_v1_migrates_before_repair_or_delete() {
    for repair in [true, false] {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join(if repair {
            "semantic-invalid-repair.toml"
        } else {
            "semantic-invalid-delete.toml"
        });
        fs::write(&path, SEMANTIC_INVALID_V1_BYTES).expect("semantic-invalid v1 fixture");

        let legacy = load_path(&path).expect("identity-valid semantic-invalid v1 loads");
        assert_eq!(legacy.source_version, ConfigSourceVersion::V1);
        assert_eq!(legacy.config.profiles.len(), 1);
        assert_eq!(legacy.config.profiles[0].id, SEMANTIC_INVALID_PROFILE_ID);
        assert!(legacy.config.profiles[0].host.is_empty());
        assert!(matches!(
            legacy.config.profiles[0].safety,
            ProfileSafetyPosture::UnclassifiedLegacy {
                source: LegacyConfigVersion::V1
            }
        ));

        let writer = ConfigWriter::default();
        let plan = writer
            .migration_plan(&path)
            .expect("identity-valid semantic-invalid legacy remains migratable");
        assert_eq!(plan.source_version, 1);
        assert_eq!(
            plan.config_fingerprint,
            sha256_hex(SEMANTIC_INVALID_V1_BYTES)
        );
        assert_eq!(plan.profiles.len(), 1);
        assert_eq!(plan.profiles[0].profile_id, SEMANTIC_INVALID_PROFILE_ID);
        assert_eq!(plan.profiles[0].endpoint, "mysql://:3306");
        let posture_document = serde_json::to_vec(&serde_json::json!({
            "config_fingerprint": plan.config_fingerprint,
            "profiles": [{
                "profile_id": SEMANTIC_INVALID_PROFILE_ID,
                "environment": "production",
                "access": "read_only"
            }]
        }))
        .expect("semantic-invalid posture document");

        let outcome = writer
            .migrate_v3(&path, &posture_document)
            .expect("semantic-invalid legacy migration commits");
        assert_eq!(outcome.state, CommitState::Committed);
        assert!(matches!(
            &outcome.observation,
            PostCommitObservation::Observed(_)
        ));
        let backup = migration_backup_path(&path);
        assert_eq!(outcome.migration_backup, Some(backup.clone()));
        assert_eq!(
            fs::read(&backup).expect("semantic-invalid exact backup"),
            SEMANTIC_INVALID_V1_BYTES
        );

        let migrated = load_path(&path).expect("semantic-invalid classified v3 loads");
        assert_eq!(migrated.source_version, ConfigSourceVersion::V3);
        assert_eq!(migrated.config.profiles.len(), 1);
        let expected = migrated.config.profiles[0].clone();
        assert_eq!(expected.id, SEMANTIC_INVALID_PROFILE_ID);
        assert!(
            expected.host.is_empty(),
            "migration preserves the editable field"
        );
        let instance_id = expected
            .safety
            .instance_id()
            .expect("migration assigns an internal identity");
        assert!(matches!(
            expected.safety,
            ProfileSafetyPosture::Classified {
                environment: ProfileEnvironment::Production,
                access: ProfileAccess::ReadOnly,
                ..
            }
        ));

        if repair {
            let mut repaired = expected.clone();
            repaired.host = "repaired.internal".to_owned();
            writer
                .mutate_path(
                    &path,
                    ConfigMutation::UpdateChecked {
                        profile_id: SEMANTIC_INVALID_PROFILE_ID.to_owned(),
                        expected_profile: expected,
                        profile: repaired,
                    },
                    MigrationConsent::Cancelled,
                )
                .expect("semantic-invalid classified profile can be repaired");
            let repaired = load_path(&path).expect("repaired v3 loads");
            assert_eq!(repaired.config.profiles[0].host, "repaired.internal");
            assert_eq!(
                repaired.config.profiles[0].safety.instance_id(),
                Some(instance_id),
                "repair preserves the immutable identity"
            );
        } else {
            writer
                .mutate_path(
                    &path,
                    ConfigMutation::DeleteChecked {
                        profile_id: SEMANTIC_INVALID_PROFILE_ID.to_owned(),
                        expected_profile: expected,
                    },
                    MigrationConsent::Cancelled,
                )
                .expect("semantic-invalid classified profile can be deleted");
            assert!(
                load_path(&path)
                    .expect("post-delete v3 loads")
                    .config
                    .profiles
                    .is_empty()
            );
        }
    }
}

#[test]
fn invalid_create_auto_base_id_is_rejected_before_v3_main_side_effect() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    fs::write(&path, EMPTY_V3_BYTES).expect("v3 fixture writes");
    let mutation = ConfigMutation::CreateAuto {
        base_id: " invalid-auto-base".to_owned(),
        profile: profile("valid-destination"),
    };

    let result = ConfigWriter::default().mutate_path(&path, mutation, MigrationConsent::Confirmed);

    assert!(matches!(result, Err(ConfigError::InvalidProfile)));
    assert_eq!(fs::read(&path).expect("main unchanged"), EMPTY_V3_BYTES);
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
    let posture_document = v1_posture_document(&writer, &path);

    for attempt in 1..=2 {
        let result = writer.migrate_v3(&path, &posture_document);
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
        let writer = ConfigWriter::default();
        let posture_document = v1_posture_document(&writer, &path);
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

        let result = writer.migrate_v3(&path, &posture_document);

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
    let writer = ConfigWriter::default();
    let posture_document = v1_posture_document(&writer, &path);
    fs::write(&backup, V1_BYTES).expect("identical backup");
    fs::set_permissions(&backup, fs::Permissions::from_mode(0o600)).expect("private mode");

    writer
        .migrate_v3(&path, &posture_document)
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
    let writer = ConfigWriter::default();
    let posture_document = v1_posture_document(&writer, &path);
    fs::write(migration_backup_path(&path), b"different").expect("conflicting backup");

    let conflict = writer.migrate_v3(&path, &posture_document);
    assert!(matches!(conflict, Err(ConfigError::BackupConflict { .. })));
    assert_eq!(fs::read(&path).expect("main unchanged"), V1_BYTES);

    fs::remove_file(migration_backup_path(&path)).expect("remove conflict");
    writer
        .migrate_v3(&path, &posture_document)
        .expect("explicit migration");
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
    let posture_document = v1_posture_document(&writer, &path);
    let backup = migration_backup_path(&path);
    assert!(!backup.exists());

    let outcome = writer
        .migrate_v3(&path, &posture_document)
        .expect("migration rename commit is returned as a typed observation outcome");

    assert_eq!(outcome.state, CommitState::CommittedDurabilityUnknown);
    let PostCommitObservation::Failed(error) = &outcome.observation else {
        panic!("observation must fail through the injected load seam");
    };
    assert_eq!(
        error.commit_state(),
        CommitState::CommittedDurabilityUnknown
    );
    assert_eq!(outcome.migration_backup, Some(backup.clone()));
    assert_eq!(
        fs::read(&backup).expect("exact v1 backup remains"),
        V1_BYTES
    );

    let committed = load_path(&path).expect("committed v3 remains readable");
    assert_eq!(committed.source_version, ConfigSourceVersion::V3);
    assert!(!committed.migration_required);
    assert_eq!(committed.config.profiles.len(), 1);
    assert_eq!(committed.config.profiles[0].id, "redis-local");
    assert!(matches!(
        committed.config.profiles[0].safety,
        ProfileSafetyPosture::Classified {
            environment: ProfileEnvironment::Production,
            access: ProfileAccess::ReadOnly,
            ..
        }
    ));

    let debug = format!("{outcome:?}\n{error:?}");
    let main_path = path.display().to_string();
    let backup_path = backup.display().to_string();
    for forbidden in [
        main_path.as_str(),
        backup_path.as_str(),
        "sentinel-secret-config-name.toml",
    ] {
        assert!(!debug.contains(forbidden), "debug leaked {forbidden}");
    }
}

#[test]
fn migration_observation_rejects_strict_valid_all_profile_posture_and_identity_rewrite() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory
        .path()
        .join("migration-observation-path-sentinel.toml");
    fs::write(&path, OBSERVATION_REWRITE_V1_BYTES).expect("v1 observation fixture");
    let rewrite = Arc::new(RewriteMigrationObservation::default());
    let writer = ConfigWriter::with_fault_injector(rewrite.clone());
    let plan = writer
        .migration_plan(&path)
        .expect("all-profile migration plan");
    assert_eq!(plan.profiles.len(), 2);
    let posture_document = serde_json::to_vec(&serde_json::json!({
        "config_fingerprint": plan.config_fingerprint,
        "profiles": [
            {
                "profile_id": OBSERVATION_PROFILE_A,
                "environment": "production",
                "access": "read_only"
            },
            {
                "profile_id": OBSERVATION_PROFILE_B,
                "environment": "development",
                "access": "read_write"
            }
        ]
    }))
    .expect("all-profile posture document");

    let outcome = writer
        .migrate_v3(&path, &posture_document)
        .expect("rename commit returns a typed observation outcome");
    assert_eq!(rewrite.checks.load(Ordering::SeqCst), 1);
    let backup = migration_backup_path(&path);
    assert_eq!(
        fs::read(&backup).expect("exact observation backup"),
        OBSERVATION_REWRITE_V1_BYTES
    );

    let rewritten = load_path(&path).expect("rewritten file remains strict-valid v3");
    assert_eq!(rewritten.source_version, ConfigSourceVersion::V3);
    assert_eq!(rewritten.config.profiles.len(), 2);
    assert!(rewritten.config.profiles.iter().any(|profile| {
        profile.id == OBSERVATION_PROFILE_A
            && profile.safety
                == ProfileSafetyPosture::classified(
                    ProfileEnvironment::Development,
                    ProfileAccess::ReadWrite,
                    REWRITTEN_INSTANCE_A,
                )
    }));
    assert!(rewritten.config.profiles.iter().any(|profile| {
        profile.id == OBSERVATION_PROFILE_B
            && profile.safety
                == ProfileSafetyPosture::classified(
                    ProfileEnvironment::Production,
                    ProfileAccess::ReadOnly,
                    REWRITTEN_INSTANCE_B,
                )
    }));

    let error = match &outcome.observation {
        PostCommitObservation::Failed(error) => error,
        PostCommitObservation::Observed(_) => {
            panic!("strict-valid all-profile migration rewrite was accepted as Observed")
        }
    };
    assert_eq!(outcome.state, CommitState::CommittedDurabilityUnknown);
    assert_eq!(
        error.commit_state(),
        CommitState::CommittedDurabilityUnknown
    );
    let source =
        std::error::Error::source(error).and_then(|source| source.downcast_ref::<ConfigError>());
    assert!(matches!(source, Some(ConfigError::ExternalChange)));

    let debug = format!("{outcome:?}\n{error:?}");
    let main_path = path.display().to_string();
    let backup_path = backup.display().to_string();
    let rewritten_instance_a = REWRITTEN_INSTANCE_A.to_string();
    let rewritten_instance_b = REWRITTEN_INSTANCE_B.to_string();
    for forbidden in [
        main_path.as_str(),
        backup_path.as_str(),
        "migration-observation-path-sentinel.toml",
        OBSERVATION_PROFILE_A,
        OBSERVATION_PROFILE_B,
        rewritten_instance_a.as_str(),
        rewritten_instance_b.as_str(),
    ] {
        assert!(!debug.contains(forbidden), "debug leaked {forbidden}");
    }
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

#[derive(Default)]
struct RewriteMigrationObservation {
    checks: AtomicUsize,
}

impl MutationFaultInjector for RewriteMigrationObservation {
    fn check(&self, point: MutationFailpoint, path: &Path) -> std::io::Result<()> {
        if point != MutationFailpoint::MainObservationLoad {
            return Ok(());
        }
        if self.checks.fetch_add(1, Ordering::SeqCst) != 0 {
            return Ok(());
        }

        let mut config = load_path(path)
            .map_err(|_| std::io::Error::other("committed migration was not readable"))?
            .config;
        let mut rewritten = 0;
        for profile in &mut config.profiles {
            match profile.id.as_str() {
                OBSERVATION_PROFILE_A => {
                    profile.safety = ProfileSafetyPosture::classified(
                        ProfileEnvironment::Development,
                        ProfileAccess::ReadWrite,
                        REWRITTEN_INSTANCE_A,
                    );
                    rewritten += 1;
                }
                OBSERVATION_PROFILE_B => {
                    profile.safety = ProfileSafetyPosture::classified(
                        ProfileEnvironment::Production,
                        ProfileAccess::ReadOnly,
                        REWRITTEN_INSTANCE_B,
                    );
                    rewritten += 1;
                }
                _ => {}
            }
        }
        if rewritten != 2 {
            return Err(std::io::Error::other(
                "observation rewrite did not find every migration profile",
            ));
        }
        let encoded = toml::to_string(&config)
            .map_err(|_| std::io::Error::other("strict-valid rewrite did not serialize"))?;
        fs::write(path, encoded)
    }
}

#[test]
fn config_contract_json_is_exact_and_v1_reader_rejects_v3() {
    assert_eq!(
        serde_json::to_value(config_contract()).expect("serialize contract"),
        serde_json::json!({
            "read_versions": [1, 2, 3],
            "write_version": 3,
            "migration_backup_suffixes": {
                "1": ".v1.bak",
                "2": ".v2.bak"
            }
        })
    );

    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    mutate_path(
        &path,
        ConfigMutation::Create(profile("current")),
        MigrationConsent::Confirmed,
    )
    .expect("write v3");
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
        Err(f665438_v1_reader::FrozenReaderError::UnsupportedVersion(3))
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

fn v1_posture_document(writer: &ConfigWriter, path: &Path) -> Vec<u8> {
    let plan = writer.migration_plan(path).expect("v1 migration plan");
    assert_eq!(plan.source_version, 1);
    assert_eq!(plan.config_fingerprint, sha256_hex(V1_BYTES));
    assert_eq!(plan.profiles.len(), 1);
    assert_eq!(plan.profiles[0].profile_id, "redis-local");
    assert_eq!(plan.profiles[0].endpoint, "redis://127.0.0.1:6379");
    serde_json::to_vec(&serde_json::json!({
        "config_fingerprint": plan.config_fingerprint,
        "profiles": [{
            "profile_id": "redis-local",
            "environment": "production",
            "access": "read_only"
        }]
    }))
    .expect("v1 posture document")
}

fn sha256_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
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
        safety: ProfileSafetyPosture::new(
            ProfileEnvironment::Development,
            ProfileAccess::ReadWrite,
        ),
        tls: TlsMode::Disabled,
        credential_mode: CredentialMode::None,
        secret_env: None,
        redis_tls: RedisTlsConfig::default(),
    }
}

#[allow(dead_code)]
fn assert_path(_: PathBuf) {}
