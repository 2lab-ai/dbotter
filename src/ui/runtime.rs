//! Background Tokio bridge. It delegates all database orchestration to ApplicationService.

use std::time::Duration;

use crate::model::ExecuteRequest;
use crate::service::{ApplicationService, ServiceError};

use super::adapter::{ServicePort, UiCommand};
use super::model::{OperationKind, ProfileSnapshot, UiEvent};

pub fn spawn(service_port: ServicePort) {
    tokio::spawn(async move {
        let application = ApplicationService::load();
        run(service_port, application).await;
    });
}

async fn run(mut service_port: ServicePort, application: Result<ApplicationService, ServiceError>) {
    while let Some(command) = service_port.next_command().await {
        let event = match &application {
            Ok(application) => handle_command(application, command).await,
            Err(error) => service_error_event(command, error.to_string()),
        };
        if service_port.emit(event).await.is_err() {
            break;
        }
    }
}

async fn handle_command(application: &ApplicationService, command: UiCommand) -> UiEvent {
    handle_command_with_config_path(application, command, crate::config::config_path()).await
}

async fn handle_command_with_config_path(
    application: &ApplicationService,
    command: UiCommand,
    config_path: Result<std::path::PathBuf, crate::config::ConfigError>,
) -> UiEvent {
    match command {
        UiCommand::RefreshProfiles => UiEvent::ProfilesLoaded(
            application
                .profiles_snapshot()
                .await
                .iter()
                .map(ProfileSnapshot::from_profile)
                .collect(),
        ),
        UiCommand::UpsertProfile {
            operation_id,
            profile,
        } => {
            let profile_id = crate::model::ProfileId(profile.id.clone());
            let outcome = match config_path {
                Ok(path) => application.upsert_profile_path(&path, profile).await,
                Err(error) => Err(error.into()),
            };
            match outcome {
                Ok(profile_id) => UiEvent::ProfileSaved {
                    operation_id,
                    profile_id,
                },
                Err(error) => UiEvent::ProfileSaveFailed {
                    operation_id,
                    profile_id,
                    message: error.to_string(),
                },
            }
        }
        UiCommand::TestConnection {
            operation_id,
            profile_id,
        } => match application
            .check(operation_id, profile_id.clone(), Duration::from_secs(10))
            .await
        {
            Ok(outcome) => UiEvent::ConnectionReady {
                operation_id: outcome.operation_id,
                profile_id: outcome.profile_id,
                elapsed_ms: u64::try_from(outcome.elapsed_ms).unwrap_or(u64::MAX),
            },
            Err(error) => failed(
                operation_id,
                profile_id,
                OperationKind::Connection,
                error.to_string(),
            ),
        },
        UiCommand::Execute {
            operation_id,
            profile_id,
            language,
            text,
            row_limit,
            timeout_ms,
        } => match application
            .execute(ExecuteRequest {
                operation_id,
                profile_id: profile_id.clone(),
                language,
                text,
                row_limit,
                timeout: Duration::from_millis(timeout_ms.max(1)),
            })
            .await
        {
            Ok(outcome) => UiEvent::QueryFinished {
                operation_id: outcome.operation_id,
                profile_id: outcome.profile_id,
                result: outcome.result,
            },
            Err(error) => failed(
                operation_id,
                profile_id,
                OperationKind::Execute,
                error.to_string(),
            ),
        },
    }
}

fn failed(
    operation_id: crate::model::OperationId,
    profile_id: crate::model::ProfileId,
    kind: OperationKind,
    message: String,
) -> UiEvent {
    UiEvent::OperationFailed {
        operation_id,
        profile_id,
        kind,
        message,
    }
}

fn service_error_event(command: UiCommand, message: String) -> UiEvent {
    match command {
        UiCommand::RefreshProfiles => UiEvent::ProfilesFailed(message),
        UiCommand::UpsertProfile {
            operation_id,
            profile,
        } => UiEvent::ProfileSaveFailed {
            operation_id,
            profile_id: crate::model::ProfileId(profile.id),
            message,
        },
        UiCommand::TestConnection {
            operation_id,
            profile_id,
        } => failed(operation_id, profile_id, OperationKind::Connection, message),
        UiCommand::Execute {
            operation_id,
            profile_id,
            ..
        } => failed(operation_id, profile_id, OperationKind::Execute, message),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::config::Config;
    use crate::model::{
        ConnectionProfile, DriverKind, OperationId, ProfileId, QueryLanguage, TlsMode,
    };
    use crate::service::{ApplicationService, DriverConnector, EnvironmentSecrets};

    use super::{UiCommand, UiEvent, handle_command_with_config_path};

    #[tokio::test]
    async fn saved_profile_is_in_the_live_service_and_next_refresh() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let service = service();

        let saved = handle_command_with_config_path(
            &service,
            UiCommand::UpsertProfile {
                operation_id: OperationId(11),
                profile: profile(DriverKind::Redis),
            },
            Ok(path.clone()),
        )
        .await;

        assert!(matches!(
            saved,
            UiEvent::ProfileSaved {
                operation_id: OperationId(11),
                profile_id,
            } if profile_id == ProfileId("runtime-profile".to_owned())
        ));
        assert_eq!(
            service
                .language_for(&ProfileId("runtime-profile".to_owned()))
                .await
                .expect("saved profile is immediately in the live service"),
            QueryLanguage::RedisCommand
        );

        let refreshed =
            handle_command_with_config_path(&service, UiCommand::RefreshProfiles, Ok(path)).await;
        assert!(matches!(
            refreshed,
            UiEvent::ProfilesLoaded(profiles)
                if profiles.len() == 1
                    && profiles[0].id == ProfileId("runtime-profile".to_owned())
        ));
    }

    #[tokio::test]
    async fn invalid_direct_upsert_emits_failure_without_writing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let service = service();
        let mut invalid = profile(DriverKind::MySql);
        invalid.host.clear();

        let event = handle_command_with_config_path(
            &service,
            UiCommand::UpsertProfile {
                operation_id: OperationId(12),
                profile: invalid,
            },
            Ok(path.clone()),
        )
        .await;

        assert!(matches!(
            event,
            UiEvent::ProfileSaveFailed {
                operation_id: OperationId(12),
                profile_id,
                message,
            } if profile_id == ProfileId("runtime-profile".to_owned())
                && message.contains("host is required")
        ));
        assert!(!path.exists());
        assert!(service.profiles_snapshot().await.is_empty());
    }

    fn service() -> ApplicationService {
        ApplicationService::new(
            Config::default(),
            Arc::new(DriverConnector),
            Arc::new(EnvironmentSecrets),
        )
    }

    fn profile(driver: DriverKind) -> ConnectionProfile {
        ConnectionProfile {
            id: "runtime-profile".to_owned(),
            name: "Runtime profile".to_owned(),
            driver,
            host: "127.0.0.1".to_owned(),
            port: match driver {
                DriverKind::MySql => 3306,
                DriverKind::Redis => 6379,
                DriverKind::MongoDb => 27017,
            },
            database: None,
            username: None,
            tls: TlsMode::Disabled,
            secret_env: None,
        }
    }
}
