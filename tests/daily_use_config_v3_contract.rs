use std::fs;

use dbotter::config::{ConfigError, ConfigSourceVersion, config_contract, load_path};

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
