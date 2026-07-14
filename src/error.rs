#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error(transparent)]
    Service(#[from] crate::service::ServiceError),
    #[error(transparent)]
    Config(#[from] crate::config::ConfigError),
    #[error(transparent)]
    Secret(#[from] crate::secrets::SecretError),
    #[error(transparent)]
    Driver(#[from] crate::drivers::DriverError),
    #[error("unknown profile: {0}")]
    UnknownProfile(String),
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("desktop support is not enabled; rebuild with --features desktop")]
    DesktopDisabled,
    #[cfg(feature = "desktop")]
    #[error("desktop error: {0}")]
    Desktop(String),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}
