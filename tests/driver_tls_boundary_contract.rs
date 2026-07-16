use std::time::Duration;

use dbotter::drivers::{DriverError, connect};
use dbotter::model::{
    ConnectionProfile, CredentialMode, DriverKind, ProfileAccess, ProfileEnvironment,
    ProfileSafetyPosture, RedisTlsConfig, TlsMode,
};

#[tokio::test]
async fn public_driver_connect_rejects_legacy_preferred_before_io() {
    let result = connect(
        &redis_profile(TlsMode::Preferred),
        None,
        Duration::from_millis(1),
    )
    .await;
    assert!(matches!(
        result,
        Err(DriverError::Unsupported {
            driver: DriverKind::Redis,
            ..
        })
    ));
}

#[tokio::test]
async fn required_redis_transport_is_supported_and_never_reclassified_as_unsupported() {
    let result = dbotter::drivers::redis::RedisSession::connect(
        &redis_profile(TlsMode::Required),
        None,
        Duration::from_millis(1),
    )
    .await;
    assert!(
        !matches!(result, Err(DriverError::Unsupported { .. })),
        "Required must enter the verified TLS transport path"
    );
}

fn redis_profile(tls: TlsMode) -> ConnectionProfile {
    ConnectionProfile {
        id: "redis-boundary".to_owned(),
        name: "Redis boundary".to_owned(),
        driver: DriverKind::Redis,
        host: "203.0.113.1".to_owned(),
        port: 6379,
        database: None,
        username: None,
        safety: ProfileSafetyPosture::new(
            ProfileEnvironment::Development,
            ProfileAccess::ReadWrite,
        ),
        tls,
        credential_mode: CredentialMode::None,
        secret_env: None,
        redis_tls: RedisTlsConfig::default(),
    }
}
