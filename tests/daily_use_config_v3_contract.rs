use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};

use dbotter::config::{
    ConfigError, ConfigMutation, ConfigSourceVersion, ConfigWriter, MigrationConsent,
    config_contract, load_path,
};
use dbotter::model::{ConnectionDraft, ConnectionProfile, DriverKind};

#[path = "fixtures/c424e4e_v2_reader.rs"]
mod c424e4e_v2_reader;

const V2_PROFILE: &str = r#"version = 2

[[profiles]]
id = "legacy"
name = "Legacy"
driver = "mysql"
host = "127.0.0.1"
port = 3306
tls = "preferred"
credential_mode = "none"
"#;

const V3_PROFILE: &str = r#"version = 3

[[profiles]]
id = "daily"
name = "Daily"
driver = "mysql"
host = "127.0.0.1"
port = 3306
tls = "preferred"
credential_mode = "none"
environment = "development"
access = "read-write"
instance_id = "00112233445566778899aabbccddeeff"
"#;

const V3_REDIS_PROFILE: &str = r#"version = 3

[[profiles]]
id = "daily-redis"
name = "Daily Redis"
driver = "redis"
host = "127.0.0.1"
port = 6379
tls = "required"
credential_mode = "none"
environment = "development"
access = "read-write"
instance_id = "ffeeddccbbaa99887766554433221100"
redis_tls = { ca_file = "redis-ca.pem" }
"#;

fn assert_v3_invalid_profile(raw: &str) {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    fs::write(&path, raw).expect("invalid v3 fixture");

    assert!(matches!(load_path(&path), Err(ConfigError::InvalidProfile)));
}

#[test]
fn config_v3_compatibility_json_is_exact() {
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
}

#[test]
fn missing_config_starts_as_empty_v3_without_migration() {
    let directory = tempfile::tempdir().expect("tempdir");
    let loaded = load_path(&directory.path().join("missing.toml")).expect("missing loads");

    assert_eq!(loaded.source_version, ConfigSourceVersion::Missing);
    assert!(!loaded.migration_required);
    assert_eq!(loaded.config.version, 3);
    assert!(loaded.config.profiles.is_empty());
}

#[test]
fn v1_and_v2_are_read_only_legacy_inputs_until_explicit_upgrade() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");

    fs::write(&path, "version = 1\n").expect("v1 fixture");
    let v1 = load_path(&path).expect("v1 loads");
    assert_eq!(v1.source_version, ConfigSourceVersion::V1);
    assert!(v1.migration_required);
    assert_eq!(v1.config.version, 3);
    assert_eq!(
        fs::read_to_string(&path).expect("v1 unchanged"),
        "version = 1\n"
    );

    fs::write(&path, V2_PROFILE).expect("v2 fixture");
    let v2 = load_path(&path).expect("v2 loads");
    assert_eq!(v2.source_version, ConfigSourceVersion::V2);
    assert!(v2.migration_required);
    assert_eq!(v2.config.version, 3);
    assert_eq!(fs::read_to_string(&path).expect("v2 unchanged"), V2_PROFILE);
}

#[test]
fn well_formed_v3_profile_loads_without_migration() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    fs::write(&path, V3_PROFILE).expect("v3 fixture");

    let loaded = load_path(&path).expect("v3 loads");
    assert_eq!(format!("{:?}", loaded.source_version), "V3");
    assert!(!loaded.migration_required);
    assert_eq!(loaded.config.version, 3);
    assert_eq!(loaded.config.profiles.len(), 1);
}

#[test]
fn v3_missing_or_duplicate_instance_identity_fails_closed() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");

    fs::write(&path, V2_PROFILE.replace("version = 2", "version = 3"))
        .expect("missing identity fixture");
    assert!(matches!(load_path(&path), Err(ConfigError::InvalidProfile)));

    let second = V3_PROFILE.replacen("version = 3\n\n", "", 1).replacen(
        "id = \"daily\"",
        "id = \"daily-copy\"",
        1,
    );
    let duplicate = format!("{V3_PROFILE}\n{second}");
    fs::write(&path, duplicate).expect("duplicate identity fixture");
    assert!(matches!(load_path(&path), Err(ConfigError::InvalidProfile)));
}

#[test]
fn v3_malformed_instance_identity_fails_as_an_invalid_profile() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");

    for malformed in [
        "00112233445566778899aabbccddee",
        "00112233445566778899aabbccddeeff00",
        "00112233445566778899AABBCCDDEEFF",
        "00112233445566778899aabbccddeefg",
    ] {
        fs::write(
            &path,
            V3_PROFILE.replace("00112233445566778899aabbccddeeff", malformed),
        )
        .expect("malformed fixture");
        assert!(
            matches!(load_path(&path), Err(ConfigError::InvalidProfile)),
            "malformed instance id must fail at the profile boundary: {malformed}"
        );
    }
}

#[test]
fn v3_top_level_extra_field_fails_before_service_construction() {
    assert_v3_invalid_profile(&V3_PROFILE.replacen(
        "version = 3\n",
        "version = 3\nunexpected_top_level = true\n",
        1,
    ));
}

#[test]
fn v3_profile_level_extra_field_fails_before_service_construction() {
    assert_v3_invalid_profile(&format!("{V3_PROFILE}unexpected_profile_field = true\n"));
}

#[test]
fn v3_nested_redis_tls_extra_field_fails_before_service_construction() {
    assert_v3_invalid_profile(&V3_REDIS_PROFILE.replace(
        "redis_tls = { ca_file = \"redis-ca.pem\" }",
        "redis_tls = { ca_file = \"redis-ca.pem\", unexpected_nested = true }",
    ));
}

#[test]
fn v2_rejects_each_v3_posture_field_instead_of_silently_classifying_legacy() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");

    for injected in [
        "environment = \"development\"",
        "access = \"read-write\"",
        "instance_id = \"00112233445566778899aabbccddeeff\"",
    ] {
        fs::write(&path, format!("{V2_PROFILE}{injected}\n"))
            .expect("v2 posture injection fixture");
        assert!(
            matches!(load_path(&path), Err(ConfigError::InvalidProfile)),
            "v2 must explicitly reject {injected}"
        );
    }
}

#[test]
fn v2_keeps_permissive_loading_for_non_v3_extension_fields() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    let fixture = format!("{V2_PROFILE}legacy_extension = \"still-readable\"\n");
    fs::write(&path, &fixture).expect("v2 extension fixture");

    assert_eq!(
        c424e4e_v2_reader::load_before_service_or_network(path.clone(), || {}, || {}),
        Ok(()),
        "the frozen version-2 reader accepted arbitrary profile extensions"
    );
    let loaded = load_path(&path).expect("current reader preserves version-2 permissiveness");
    assert_eq!(loaded.source_version, ConfigSourceVersion::V2);
    assert!(loaded.migration_required);
}

#[test]
fn frozen_v2_reader_rejects_v3_before_service_or_network() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    fs::write(&path, V3_PROFILE).expect("v3 fixture");
    let service_constructions = AtomicUsize::new(0);
    let network_acquisitions = AtomicUsize::new(0);

    let result = c424e4e_v2_reader::load_before_service_or_network(
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
        Err(c424e4e_v2_reader::FrozenReaderError::UnsupportedVersion(3))
    );
    assert_eq!(service_constructions.load(Ordering::SeqCst), 0);
    assert_eq!(network_acquisitions.load(Ordering::SeqCst), 0);
}

#[test]
fn ordinary_legacy_crud_is_rejected_with_zero_filesystem_side_effects() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    fs::write(&path, V2_PROFILE).expect("v2 fixture");
    let before = fs::read(&path).expect("original bytes");
    let profile = ConnectionProfile::from_draft(
        "must-not-exist".to_owned(),
        ConnectionDraft::for_driver(DriverKind::MySql),
    );

    let result = ConfigWriter::default().mutate_path(
        &path,
        ConfigMutation::Create(profile),
        MigrationConsent::Confirmed,
    );

    assert!(matches!(result, Err(ConfigError::MigrationPostureRequired)));
    assert_eq!(fs::read(&path).expect("main unchanged"), before);
    let entries = fs::read_dir(directory.path())
        .expect("directory")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name())
        .collect::<Vec<_>>();
    assert_eq!(entries, vec!["config.toml"]);
}
