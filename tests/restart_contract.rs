use std::fs;

use dbotter::config::{
    ConfigMutation, ConfigSourceVersion, ConfigWriter, MigrationConsent, load_path,
    migration_backup_path,
};
use dbotter::model::{
    ConnectionProfile, CredentialMode, DriverKind, ProfileAccess, ProfileEnvironment,
    ProfileSafetyPosture, RedisTlsConfig, TlsMode,
};
use dbotter::secrets::SessionSecretStore;
use sha2::{Digest as _, Sha256};

#[test]
fn v1_migration_then_restart_persists_only_non_secret_profile_state() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    let ca_path = directory.path().join("redis-ca.pem");
    fs::write(
        &ca_path,
        "-----BEGIN CERTIFICATE-----\n\
MIIB0zCCAYWgAwIBAgIUEFN5G3AUb5d/ZC+q+YtFuMeoWvowBQYDK2VwMF8xCzAJ\n\
BgNVBAYTAnVzMRMwEQYDVQQIDApjYWxpZm9ybmlhMSEwHwYDVQQKDBhJbnRlcm5l\n\
dCBXaWRnaXRzIFB0eSBMdGQxGDAWBgNVBAMMD0F1c3RpbiBCb25hbmRlcjAeFw0y\n\
NTA3MDEwMzA4MTVaFw0zNTA2MjkwMzA4MTVaMF8xCzAJBgNVBAYTAnVzMRMwEQYD\n\
VQQIDApjYWxpZm9ybmlhMSEwHwYDVQQKDBhJbnRlcm5ldCBXaWRnaXRzIFB0eSBM\n\
dGQxGDAWBgNVBAMMD0F1c3RpbiBCb25hbmRlcjAqMAUGAytlcAMhAHfjdF5QJ4OW\n\
k/3XLlsxDcP8cwBVmB+ySWKq2JanRS8uo1MwUTAdBgNVHQ4EFgQUCw2pVpGKz2xk\n\
IjbVHYh0LnzdkW4wHwYDVR0jBBgwFoAUCw2pVpGKz2xkIjbVHYh0LnzdkW4wDwYD\n\
VR0TAQH/BAUwAwEB/zAFBgMrZXADQQBA6VMDBPz9x0b5Wvw4D+2UrLdyhzzjqtrX\n\
UQOjCTqcKdEwWvgS6ftiQlQJPDfkVDEMOAJgqRmEGvsKjvwMCPIC\n\
-----END CERTIFICATE-----\n",
    )
    .expect("ca fixture");
    let v1 = b"version = 1\n";
    fs::write(&path, v1).expect("v1 fixture");

    let writer = ConfigWriter::default();
    let plan = writer
        .migration_plan(&path)
        .expect("empty v1 migration plan");
    assert_eq!(plan.source_version, 1);
    assert_eq!(plan.config_fingerprint, sha256_hex(v1));
    assert!(plan.profiles.is_empty());
    let posture_document = serde_json::to_vec(&serde_json::json!({
        "config_fingerprint": plan.config_fingerprint,
        "profiles": []
    }))
    .expect("empty v1 posture document");
    writer
        .migrate_v3(&path, &posture_document)
        .expect("explicit empty v1 migration commits");
    assert_eq!(fs::read(migration_backup_path(&path)).expect("backup"), v1);
    assert_eq!(
        load_path(&path)
            .expect("post-migration load")
            .source_version,
        ConfigSourceVersion::V3
    );

    let profile = ConnectionProfile {
        id: "redis-tls".to_owned(),
        name: "Redis TLS".to_owned(),
        driver: DriverKind::Redis,
        host: "redis.local".to_owned(),
        port: 6380,
        database: Some("1".to_owned()),
        username: Some("client".to_owned()),
        safety: ProfileSafetyPosture::new(
            ProfileEnvironment::Development,
            ProfileAccess::ReadWrite,
        ),
        tls: TlsMode::Required,
        credential_mode: CredentialMode::Session,
        secret_env: None,
        redis_tls: RedisTlsConfig {
            ca_file: Some(ca_path.clone()),
        },
    };
    writer
        .mutate_path(
            &path,
            ConfigMutation::Create(profile.clone()),
            MigrationConsent::Confirmed,
        )
        .expect("post-migration create commits");

    let created = load_path(&path).expect("created profile load");
    let persisted = created
        .config
        .profiles
        .first()
        .expect("created profile")
        .clone();
    let generated_instance_id = persisted
        .safety
        .instance_id()
        .expect("created profile is classified");
    assert!(matches!(
        persisted.safety,
        ProfileSafetyPosture::Classified {
            environment: ProfileEnvironment::Development,
            access: ProfileAccess::ReadWrite,
            ..
        }
    ));
    let mut expected_profile = profile;
    expected_profile.safety = ProfileSafetyPosture::classified(
        ProfileEnvironment::Development,
        ProfileAccess::ReadWrite,
        generated_instance_id,
    );
    assert_eq!(persisted, expected_profile);

    let process_b = load_path(&path).expect("fresh process load");
    assert_eq!(process_b.source_version, ConfigSourceVersion::V3);
    assert_eq!(process_b.config.profiles, vec![expected_profile]);
    assert_eq!(
        process_b.config.profiles[0].safety.instance_id(),
        Some(generated_instance_id)
    );
    assert!(SessionSecretStore::default().is_empty().expect("store"));
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
