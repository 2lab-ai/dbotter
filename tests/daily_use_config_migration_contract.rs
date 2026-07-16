use std::collections::VecDeque;
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use dbotter::config::{
    CommitState, ConfigError, ConfigSourceVersion, ConfigWriter, MutationFailpoint,
    MutationFaultInjector, ProfileInstanceIdGenerator, load_path, migration_backup_path_for_source,
};
use dbotter::model::{ProfileAccess, ProfileEnvironment, ProfileInstanceId, ProfileSafetyPosture};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

const MIGRATION_DOCUMENT_MAX_BYTES: usize = 1024 * 1024;

const V1_BYTES: &[u8] = br#"version = 1

[[profiles]]
id = "legacy-v1"
name = "Legacy v1"
driver = "mysql"
host = "v1.internal"
port = 3306
database = "app"
username = "operator"
tls = "preferred"
secret_env = "DBOTTER_V1_PASSWORD"
"#;

const V2_BYTES: &[u8] = br#"version = 2

[[profiles]]
id = "mysql-local"
name = "MySQL local"
driver = "mysql"
host = "mysql.internal"
port = 3306
database = "app"
username = "operator"
tls = "preferred"
credential_mode = "environment"
secret_env = "DBOTTER_MYSQL_PASSWORD"

[[profiles]]
id = "redis-local"
name = "Redis local"
driver = "redis"
host = "redis.internal"
port = 6379
database = "0"
tls = "disabled"
credential_mode = "none"
"#;

#[test]
fn migration_plan_is_exact_read_only_and_fingerprints_exact_source_bytes() {
    for (source, source_version, profiles) in [
        (
            V1_BYTES,
            1_u64,
            json!([
                {
                    "profile_id": "legacy-v1",
                    "endpoint": "mysql://v1.internal:3306"
                }
            ]),
        ),
        (
            V2_BYTES,
            2_u64,
            json!([
                {
                    "profile_id": "mysql-local",
                    "endpoint": "mysql://mysql.internal:3306"
                },
                {
                    "profile_id": "redis-local",
                    "endpoint": "redis://redis.internal:6379"
                }
            ]),
        ),
    ] {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("config.toml");
        fs::write(&path, source).expect("legacy fixture");

        let first = serde_json::to_value(
            ConfigWriter::default()
                .migration_plan(&path)
                .expect("legacy migration plan"),
        )
        .expect("plan JSON");
        let second = serde_json::to_value(
            ConfigWriter::default()
                .migration_plan(&path)
                .expect("repeat migration plan"),
        )
        .expect("repeat plan JSON");

        assert_eq!(
            first,
            json!({
                "source_version": source_version,
                "config_fingerprint": sha256_hex(source),
                "profiles": profiles,
            })
        );
        assert_eq!(second, first, "planning is deterministic");
        assert_eq!(fs::read(&path).expect("main bytes"), source);
        assert_eq!(directory_entries(directory.path()), vec!["config.toml"]);
    }
}

#[test]
fn explicit_all_profile_migration_writes_v3_and_exact_source_version_backups() {
    for fixture in [
        SuccessFixture {
            source: V1_BYTES,
            source_version: ConfigSourceVersion::V1,
            assignments: json!([
                {
                    "profile_id": "legacy-v1",
                    "environment": "production",
                    "access": "read_only"
                }
            ]),
            expected: vec![ExpectedProfile {
                profile_id: "legacy-v1",
                environment: ProfileEnvironment::Production,
                access: ProfileAccess::ReadOnly,
                instance_id: generated_id(1),
            }],
        },
        SuccessFixture {
            source: V2_BYTES,
            source_version: ConfigSourceVersion::V2,
            assignments: json!([
                {
                    "profile_id": "redis-local",
                    "environment": "development",
                    "access": "read_write"
                },
                {
                    "profile_id": "mysql-local",
                    "environment": "production",
                    "access": "read_only"
                }
            ]),
            expected: vec![
                ExpectedProfile {
                    profile_id: "mysql-local",
                    environment: ProfileEnvironment::Production,
                    access: ProfileAccess::ReadOnly,
                    instance_id: generated_id(1),
                },
                ExpectedProfile {
                    profile_id: "redis-local",
                    environment: ProfileEnvironment::Development,
                    access: ProfileAccess::ReadWrite,
                    instance_id: generated_id(2),
                },
            ],
        },
    ] {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("config.toml");
        fs::write(&path, fixture.source).expect("legacy fixture");
        let generated = fixture
            .expected
            .iter()
            .map(|profile| Some(profile.instance_id))
            .collect();
        let (writer, generator) = writer_with_ids(generated);
        let document = posture_document(sha256_hex(fixture.source), fixture.assignments);

        let outcome = writer
            .migrate_v3(&path, &document)
            .expect("explicit migration commits");

        let backup = migration_backup_path_for_source(&path, fixture.source_version);
        assert_eq!(outcome.state, CommitState::Committed);
        assert_eq!(outcome.migration_backup, Some(backup.clone()));
        assert_eq!(fs::read(&backup).expect("backup bytes"), fixture.source);
        assert_eq!(generator.calls(), fixture.expected.len());
        assert_private_file(&backup);
        assert_private_file(&path);

        let loaded = load_path(&path).expect("v3 reload");
        assert_eq!(loaded.source_version, ConfigSourceVersion::V3);
        assert!(!loaded.migration_required);
        assert_eq!(loaded.config.version, 3);
        assert_eq!(loaded.config.profiles.len(), fixture.expected.len());
        for expected in fixture.expected {
            let profile = loaded
                .config
                .profiles
                .iter()
                .find(|profile| profile.id == expected.profile_id)
                .expect("every planned profile remains present");
            assert_eq!(
                profile.safety,
                ProfileSafetyPosture::classified(
                    expected.environment,
                    expected.access,
                    expected.instance_id,
                )
            );
        }
    }
}

#[test]
fn posture_document_requires_every_current_profile_exactly_once_and_no_extra_fields() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    fs::write(&path, V2_BYTES).expect("v2 fixture");
    let fingerprint = sha256_hex(V2_BYTES);
    let valid_mysql = json!({
        "profile_id": "mysql-local",
        "environment": "production",
        "access": "read_only"
    });
    let valid_redis = json!({
        "profile_id": "redis-local",
        "environment": "development",
        "access": "read_write"
    });
    let invalid_documents = vec![
        posture_document(fingerprint.clone(), json!([valid_mysql.clone()])),
        posture_document(
            fingerprint.clone(),
            json!([
                valid_mysql.clone(),
                valid_redis.clone(),
                {
                    "profile_id": "not-in-plan",
                    "environment": "development",
                    "access": "read_only"
                }
            ]),
        ),
        posture_document(
            fingerprint.clone(),
            json!([
                valid_mysql.clone(),
                valid_mysql.clone(),
                valid_redis.clone()
            ]),
        ),
        serde_json::to_vec(&json!({
            "config_fingerprint": fingerprint,
            "profiles": [valid_mysql.clone(), valid_redis.clone()],
            "extra": true
        }))
        .expect("top-level extra field fixture"),
        posture_document(
            sha256_hex(V2_BYTES),
            json!([
                {
                    "profile_id": "mysql-local",
                    "environment": "production",
                    "access": "read_only",
                    "extra": true
                },
                valid_redis.clone()
            ]),
        ),
        posture_document(
            sha256_hex(V2_BYTES),
            json!([
                {
                    "profile_id": "mysql-local",
                    "environment": "production",
                    "access": "read_only",
                    "instance_id": "00112233445566778899aabbccddeeff"
                },
                valid_redis.clone()
            ]),
        ),
        posture_document(
            sha256_hex(V2_BYTES),
            json!([
                {
                    "profile_id": "mysql-local",
                    "environment": "staging",
                    "access": "read_only"
                },
                valid_redis.clone()
            ]),
        ),
        posture_document(
            sha256_hex(V2_BYTES),
            json!([
                valid_mysql.clone(),
                {
                    "profile_id": "redis-local",
                    "environment": "development",
                    "access": "safe"
                }
            ]),
        ),
        serde_json::to_vec(&json!({
            "profiles": [valid_mysql.clone(), valid_redis.clone()]
        }))
        .expect("missing fingerprint fixture"),
        serde_json::to_vec(&json!({
            "config_fingerprint": sha256_hex(V2_BYTES)
        }))
        .expect("missing profiles fixture"),
        serde_json::to_vec(&json!([valid_mysql.clone(), valid_redis.clone()]))
            .expect("wrong root fixture"),
        br#"{"config_fingerprint":"truncated""#.to_vec(),
    ];

    for (index, document) in invalid_documents.into_iter().enumerate() {
        let result = ConfigWriter::default().migrate_v3(&path, &document);
        assert!(
            matches!(result, Err(ConfigError::InvalidMigrationDocument)),
            "invalid document {index} must fail at the bounded document boundary: {result:?}"
        );
        assert_legacy_untouched(directory.path(), &path, V2_BYTES, ConfigSourceVersion::V2);
    }
}

#[test]
fn stale_plan_fingerprint_rejects_before_identity_generation_or_filesystem_mutation() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    fs::write(&path, V2_BYTES).expect("v2 fixture");
    let plan = ConfigWriter::default()
        .migration_plan(&path)
        .expect("migration plan");
    let plan_json = serde_json::to_value(plan).expect("plan JSON");
    let fingerprint = plan_json["config_fingerprint"]
        .as_str()
        .expect("string fingerprint")
        .to_owned();
    let document = valid_v2_document(fingerprint);
    let mut externally_changed = V2_BYTES.to_vec();
    externally_changed.extend_from_slice(b"\n# external edit after plan\n");
    fs::write(&path, &externally_changed).expect("external edit");
    let (writer, generator) = writer_with_ids(vec![Some(generated_id(1)), Some(generated_id(2))]);

    let result = writer.migrate_v3(&path, &document);

    assert!(matches!(result, Err(ConfigError::ExternalChange)));
    assert_eq!(generator.calls(), 0);
    assert_eq!(fs::read(&path).expect("external bytes"), externally_changed);
    assert!(!migration_backup_path_for_source(&path, ConfigSourceVersion::V2).exists());
    assert_eq!(directory_entries(directory.path()), vec!["config.toml"]);
}

#[test]
fn posture_document_accepts_exactly_one_mib_and_rejects_one_byte_more() {
    let exact_directory = tempfile::tempdir().expect("exact tempdir");
    let exact_path = exact_directory.path().join("config.toml");
    fs::write(&exact_path, V2_BYTES).expect("exact v2 fixture");
    let mut exact = valid_v2_document(sha256_hex(V2_BYTES));
    assert!(exact.len() < MIGRATION_DOCUMENT_MAX_BYTES);
    exact.resize(MIGRATION_DOCUMENT_MAX_BYTES, b' ');
    let (exact_writer, exact_generator) =
        writer_with_ids(vec![Some(generated_id(1)), Some(generated_id(2))]);

    exact_writer
        .migrate_v3(&exact_path, &exact)
        .expect("exactly 1 MiB is accepted");
    assert_eq!(exact_generator.calls(), 2);
    assert_eq!(
        load_path(&exact_path).expect("exact reload").source_version,
        ConfigSourceVersion::V3
    );

    let over_directory = tempfile::tempdir().expect("over tempdir");
    let over_path = over_directory.path().join("config.toml");
    fs::write(&over_path, V2_BYTES).expect("over v2 fixture");
    let mut over = valid_v2_document(sha256_hex(V2_BYTES));
    over.resize(MIGRATION_DOCUMENT_MAX_BYTES + 1, b' ');
    let (over_writer, over_generator) =
        writer_with_ids(vec![Some(generated_id(1)), Some(generated_id(2))]);

    let result = over_writer.migrate_v3(&over_path, &over);

    assert!(matches!(
        result,
        Err(ConfigError::MigrationDocumentTooLarge { limit, actual })
            if limit == MIGRATION_DOCUMENT_MAX_BYTES
                && actual == MIGRATION_DOCUMENT_MAX_BYTES + 1
    ));
    assert_eq!(over_generator.calls(), 0);
    assert_legacy_untouched(
        over_directory.path(),
        &over_path,
        V2_BYTES,
        ConfigSourceVersion::V2,
    );
}

#[test]
fn entropy_failure_and_collision_exhaustion_leave_main_and_backup_absent() {
    for generated in [
        vec![None],
        vec![Some(generated_id(1)), Some(generated_id(1)), None],
    ] {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("config.toml");
        fs::write(&path, V2_BYTES).expect("v2 fixture");
        let document = valid_v2_document(sha256_hex(V2_BYTES));
        let expected_calls = generated.len();
        let (writer, generator) = writer_with_ids(generated);

        let result = writer.migrate_v3(&path, &document);

        assert!(matches!(result, Err(ConfigError::EntropyUnavailable)));
        assert_eq!(generator.calls(), expected_calls);
        assert_legacy_untouched(directory.path(), &path, V2_BYTES, ConfigSourceVersion::V2);
    }
}

#[test]
fn generated_identity_collision_is_retried_and_never_persists_a_duplicate() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    fs::write(&path, V2_BYTES).expect("v2 fixture");
    let document = valid_v2_document(sha256_hex(V2_BYTES));
    let (writer, generator) = writer_with_ids(vec![
        Some(generated_id(1)),
        Some(generated_id(1)),
        Some(generated_id(2)),
    ]);

    writer
        .migrate_v3(&path, &document)
        .expect("collision is retried");

    let loaded = load_path(&path).expect("v3 reload");
    let ids = loaded
        .config
        .profiles
        .iter()
        .map(|profile| profile.safety.instance_id().expect("classified profile"))
        .collect::<Vec<_>>();
    assert_eq!(ids, vec![generated_id(1), generated_id(2)]);
    assert_ne!(ids[0], ids[1]);
    assert_eq!(generator.calls(), 3);
}

#[test]
fn conflicting_source_version_backup_never_overwrites_or_commits_main() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    let backup = migration_backup_path_for_source(&path, ConfigSourceVersion::V2);
    fs::write(&path, V2_BYTES).expect("v2 fixture");
    fs::write(&backup, b"different existing backup").expect("conflicting backup");
    let document = valid_v2_document(sha256_hex(V2_BYTES));
    let (writer, _) = writer_with_ids(vec![Some(generated_id(1)), Some(generated_id(2))]);

    let result = writer.migrate_v3(&path, &document);

    assert!(matches!(result, Err(ConfigError::BackupConflict { .. })));
    assert_eq!(fs::read(&path).expect("main unchanged"), V2_BYTES);
    assert_eq!(
        fs::read(&backup).expect("backup unchanged"),
        b"different existing backup"
    );
}

#[test]
fn main_write_failpoint_has_zero_main_commit_and_retains_exact_completed_backup() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    fs::write(&path, V2_BYTES).expect("v2 fixture");
    let document = valid_v2_document(sha256_hex(V2_BYTES));
    let generator = Arc::new(SequenceGenerator::new(vec![
        Some(generated_id(1)),
        Some(generated_id(2)),
    ]));
    let writer =
        ConfigWriter::with_fault_injector(Arc::new(OneFault(MutationFailpoint::MainWrite)))
            .with_profile_instance_id_generator(generator.clone());

    let result = writer.migrate_v3(&path, &document);

    assert!(matches!(
        result,
        Err(ConfigError::NotCommitted {
            stage: MutationFailpoint::MainWrite,
            ..
        })
    ));
    assert_eq!(generator.calls(), 2);
    assert_eq!(fs::read(&path).expect("main unchanged"), V2_BYTES);
    let backup = migration_backup_path_for_source(&path, ConfigSourceVersion::V2);
    assert_eq!(fs::read(&backup).expect("exact backup"), V2_BYTES);
    assert_eq!(
        directory_entries(directory.path()),
        vec!["config.toml", "config.toml.v2.bak"]
    );
}

struct SuccessFixture {
    source: &'static [u8],
    source_version: ConfigSourceVersion,
    assignments: Value,
    expected: Vec<ExpectedProfile>,
}

struct ExpectedProfile {
    profile_id: &'static str,
    environment: ProfileEnvironment,
    access: ProfileAccess,
    instance_id: ProfileInstanceId,
}

struct SequenceGenerator {
    generated: Mutex<VecDeque<Option<ProfileInstanceId>>>,
    calls: AtomicUsize,
}

impl SequenceGenerator {
    fn new(generated: Vec<Option<ProfileInstanceId>>) -> Self {
        Self {
            generated: Mutex::new(generated.into()),
            calls: AtomicUsize::new(0),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl ProfileInstanceIdGenerator for SequenceGenerator {
    fn generate(&self) -> Option<ProfileInstanceId> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.generated.lock().ok()?.pop_front().flatten()
    }
}

struct OneFault(MutationFailpoint);

impl MutationFaultInjector for OneFault {
    fn check(&self, point: MutationFailpoint, _path: &Path) -> std::io::Result<()> {
        if point == self.0 {
            Err(std::io::Error::other("injected migration failure"))
        } else {
            Ok(())
        }
    }
}

fn writer_with_ids(
    generated: Vec<Option<ProfileInstanceId>>,
) -> (ConfigWriter, Arc<SequenceGenerator>) {
    let generator = Arc::new(SequenceGenerator::new(generated));
    let writer = ConfigWriter::default().with_profile_instance_id_generator(generator.clone());
    (writer, generator)
}

fn posture_document(config_fingerprint: String, profiles: Value) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "config_fingerprint": config_fingerprint,
        "profiles": profiles,
    }))
    .expect("posture document fixture")
}

fn valid_v2_document(config_fingerprint: String) -> Vec<u8> {
    posture_document(
        config_fingerprint,
        json!([
            {
                "profile_id": "mysql-local",
                "environment": "production",
                "access": "read_only"
            },
            {
                "profile_id": "redis-local",
                "environment": "development",
                "access": "read_write"
            }
        ]),
    )
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

fn generated_id(byte: u8) -> ProfileInstanceId {
    ProfileInstanceId::from_bytes([byte; 16])
}

fn assert_legacy_untouched(
    directory: &Path,
    path: &Path,
    expected: &[u8],
    source_version: ConfigSourceVersion,
) {
    assert_eq!(fs::read(path).expect("main bytes"), expected);
    assert!(!migration_backup_path_for_source(path, source_version).exists());
    assert_eq!(directory_entries(directory), vec!["config.toml"]);
}

fn directory_entries(directory: &Path) -> Vec<String> {
    let mut entries = fs::read_dir(directory)
        .expect("read directory")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    entries.sort();
    entries
}

#[cfg(unix)]
fn assert_private_file(path: &Path) {
    use std::os::unix::fs::PermissionsExt as _;

    assert_eq!(
        fs::metadata(path)
            .expect("file metadata")
            .permissions()
            .mode()
            & 0o7777,
        0o600
    );
}

#[cfg(not(unix))]
fn assert_private_file(path: &Path) {
    assert!(path.is_file());
}
