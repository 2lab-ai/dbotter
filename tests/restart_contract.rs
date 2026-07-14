use std::fs;

use dbotter::config::{
    ConfigMutation, ConfigSourceVersion, MigrationConsent, load_path, migration_backup_path,
    mutate_path,
};
use dbotter::model::{ConnectionProfile, CredentialMode, DriverKind, RedisTlsConfig, TlsMode};
use dbotter::secrets::SessionSecretStore;

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

    let profile = ConnectionProfile {
        id: "redis-tls".to_owned(),
        name: "Redis TLS".to_owned(),
        driver: DriverKind::Redis,
        host: "redis.local".to_owned(),
        port: 6380,
        database: Some("1".to_owned()),
        username: Some("client".to_owned()),
        tls: TlsMode::Required,
        credential_mode: CredentialMode::Session,
        secret_env: None,
        redis_tls: RedisTlsConfig {
            ca_file: Some(ca_path.clone()),
        },
    };
    mutate_path(
        &path,
        ConfigMutation::Create(profile.clone()),
        MigrationConsent::Confirmed,
    )
    .expect("migration commits");
    assert_eq!(fs::read(migration_backup_path(&path)).expect("backup"), v1);

    let process_b = load_path(&path).expect("fresh process load");
    assert_eq!(process_b.source_version, ConfigSourceVersion::V2);
    assert_eq!(process_b.config.profiles, vec![profile]);
    assert!(SessionSecretStore::default().is_empty().expect("store"));
}
