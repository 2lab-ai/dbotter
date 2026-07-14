use std::time::Duration;

use dbotter::drivers::{DriverError, connect};
use dbotter::model::{ConnectionProfile, CredentialMode, DriverKind, RedisTlsConfig, TlsMode};

#[tokio::test]
async fn public_driver_connect_rejects_every_non_plaintext_redis_mode_before_io() {
    for tls in [TlsMode::Preferred, TlsMode::Required] {
        let result = connect(&redis_profile(tls), None, Duration::from_millis(1)).await;
        assert!(
            matches!(
                result,
                Err(DriverError::Unsupported {
                    driver: DriverKind::Redis,
                    ..
                })
            ),
            "TLS mode {tls:?} must fail closed at drivers::connect"
        );
    }
}

#[tokio::test]
async fn redis_session_constructor_independently_rejects_non_plaintext_modes() {
    for tls in [TlsMode::Preferred, TlsMode::Required] {
        let result = dbotter::drivers::redis::RedisSession::connect(
            &redis_profile(tls),
            None,
            Duration::from_millis(1),
        )
        .await;
        assert!(
            matches!(
                result,
                Err(DriverError::Unsupported {
                    driver: DriverKind::Redis,
                    ..
                })
            ),
            "TLS mode {tls:?} must fail closed at RedisSession::connect"
        );
    }
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
        tls,
        credential_mode: CredentialMode::None,
        secret_env: None,
        redis_tls: RedisTlsConfig::default(),
    }
}
