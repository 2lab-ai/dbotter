use std::fmt::{Debug, Display};
use std::path::PathBuf;

use dbotter::config::ConfigError;
use dbotter::drivers::DriverError;
use dbotter::error::AppError;
use dbotter::model::DriverKind;
use dbotter::secrets::SecretError;
use dbotter::service::ServiceError;

const SENTINELS: &[&str] = &[
    "RAW_ENV_NAME_SENTINEL",
    "RAW_BACKEND_PROSE_SENTINEL",
    "RAW_SECRET_SENTINEL",
    "RAW_CONFIG_PATH_SENTINEL",
    "RAW_SOURCE_SENTINEL",
];

#[test]
fn error_debug_display_and_public_chain_never_reexpose_raw_payloads() {
    assert_redacted(&SecretError::MissingEnv(
        "RAW_ENV_NAME_SENTINEL_RAW_SECRET_SENTINEL".to_owned(),
    ));
    assert_redacted(&ServiceError::Secret(SecretError::EmptyEnv(
        "RAW_ENV_NAME_SENTINEL_RAW_SECRET_SENTINEL".to_owned(),
    )));
    let secret_app = AppError::Service(ServiceError::Secret(SecretError::MissingEnv(
        "RAW_ENV_NAME_SENTINEL_RAW_SECRET_SENTINEL".to_owned(),
    )));
    assert_redacted(&secret_app);
    assert_static_public(&secret_app);

    assert_redacted(&DriverError::from(sqlx::Error::Protocol(
        "RAW_BACKEND_PROSE_SENTINEL_RAW_SECRET_SENTINEL".to_owned(),
    )));
    assert_redacted(&ServiceError::Driver(DriverError::InvalidConfig {
        driver: DriverKind::Redis,
        message: "RAW_BACKEND_PROSE_SENTINEL_RAW_CONFIG_PATH_SENTINEL".to_owned(),
    }));
    let driver_app = AppError::Service(ServiceError::Driver(DriverError::RedisParse(
        "RAW_BACKEND_PROSE_SENTINEL_RAW_SECRET_SENTINEL".to_owned(),
    )));
    assert_redacted(&driver_app);
    assert_static_public(&driver_app);

    assert_redacted(&ConfigError::Io {
        path: PathBuf::from("/RAW_CONFIG_PATH_SENTINEL/config.toml"),
        source: std::io::Error::other("RAW_SOURCE_SENTINEL_RAW_SECRET_SENTINEL"),
    });
    assert_redacted(&ServiceError::Config(ConfigError::BackupConflict {
        path: PathBuf::from("/RAW_CONFIG_PATH_SENTINEL/config.v1.bak"),
    }));
    let config_app = AppError::Service(ServiceError::Config(ConfigError::Io {
        path: PathBuf::from("/RAW_CONFIG_PATH_SENTINEL/config.toml"),
        source: std::io::Error::other("RAW_SOURCE_SENTINEL_RAW_SECRET_SENTINEL"),
    }));
    assert_redacted(&config_app);
    assert_static_public(&config_app);
}

#[cfg(feature = "desktop")]
#[test]
fn desktop_wrapper_debug_is_also_redacted() {
    let error = AppError::Desktop(
        "RAW_BACKEND_PROSE_SENTINEL_RAW_SECRET_SENTINEL_RAW_CONFIG_PATH_SENTINEL".to_owned(),
    );
    assert_redacted(&error);
    assert_static_public(&error);
}

fn assert_redacted(error: &(impl Debug + Display)) {
    let debug = format!("{error:?}");
    let display = error.to_string();
    for sentinel in SENTINELS {
        assert!(
            !debug.contains(sentinel),
            "debug leaked {sentinel}: {debug}"
        );
        assert!(
            !display.contains(sentinel),
            "display leaked {sentinel}: {display}"
        );
    }
}

fn assert_static_public(error: &AppError) {
    let message = error.public_message();
    for sentinel in SENTINELS {
        assert!(!message.contains(sentinel));
    }
}
