use std::fmt;

#[derive(thiserror::Error)]
pub enum AppError {
    #[error(transparent)]
    Service(#[from] crate::service::ServiceError),
    #[error(transparent)]
    Config(#[from] crate::config::ConfigError),
    #[error(transparent)]
    Secret(#[from] crate::secrets::SecretError),
    #[error(transparent)]
    Driver(#[from] crate::drivers::DriverError),
    #[error("unknown profile")]
    UnknownProfile,
    #[error("invalid input")]
    InvalidInput,
    #[error("desktop support is not enabled; rebuild with --features desktop")]
    DesktopDisabled,
    #[cfg(feature = "desktop")]
    #[error("desktop operation failed")]
    Desktop(String),
    #[error("JSON processing failed")]
    Json(#[from] serde_json::Error),
}

impl fmt::Debug for AppError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Service(_) => formatter.write_str("Service(<redacted>)"),
            Self::Config(_) => formatter.write_str("Config(<redacted>)"),
            Self::Secret(_) => formatter.write_str("Secret(<redacted>)"),
            Self::Driver(_) => formatter.write_str("Driver(<redacted>)"),
            Self::UnknownProfile => formatter.write_str("UnknownProfile"),
            Self::InvalidInput => formatter.write_str("InvalidInput"),
            Self::DesktopDisabled => formatter.write_str("DesktopDisabled"),
            #[cfg(feature = "desktop")]
            Self::Desktop(_) => formatter.write_str("Desktop(<redacted>)"),
            Self::Json(_) => formatter.write_str("Json(<redacted>)"),
        }
    }
}

impl AppError {
    pub fn public_message(&self) -> &'static str {
        use crate::model::PublicSummary;

        match self {
            Self::Service(error) => error.public_error_parts().0.message(),
            Self::InvalidInput | Self::UnknownProfile => PublicSummary::InvalidInput.message(),
            Self::Secret(_) => PublicSummary::CredentialRequired.message(),
            Self::Driver(crate::drivers::DriverError::Timeout { .. }) => {
                PublicSummary::OperationTimedOut.message()
            }
            Self::Driver(
                crate::drivers::DriverError::Unavailable { .. }
                | crate::drivers::DriverError::Unsupported { .. }
                | crate::drivers::DriverError::PreparedStatementUnsupported { .. },
            )
            | Self::DesktopDisabled => PublicSummary::UnsupportedFeature.message(),
            Self::Driver(_) => PublicSummary::NetworkUnavailable.message(),
            Self::Config(_) => PublicSummary::ConfigWriteNotCommitted.message(),
            Self::Json(_) => PublicSummary::InternalFailure.message(),
            #[cfg(feature = "desktop")]
            Self::Desktop(_) => PublicSummary::InternalFailure.message(),
        }
    }
}
