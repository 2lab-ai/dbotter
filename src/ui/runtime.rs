//! Background Tokio bridge. It delegates all database orchestration to ApplicationService.

use std::time::Duration;

use crate::config::CommitState;
use crate::model::{ExecuteRequest, PublicSummary};
use crate::service::{ApplicationService, ServiceError};

use super::adapter::{ServicePort, UiCommand};
use super::model::{OperationKind, ProfileSnapshot, UiEvent};

pub fn spawn(service_port: ServicePort, config_path: std::path::PathBuf) {
    tokio::spawn(async move {
        let application = ApplicationService::load_path(config_path);
        run(service_port, application).await;
    });
}

async fn run(mut service_port: ServicePort, application: Result<ApplicationService, ServiceError>) {
    while let Some(command) = service_port.next_command().await {
        let event = match &application {
            Ok(application) => handle_command(application, command).await,
            Err(error) => service_error_event(command, error.public_error_parts().0),
        };
        if service_port.emit(event).await.is_err() {
            break;
        }
    }
}

async fn handle_command(application: &ApplicationService, command: UiCommand) -> UiEvent {
    match command {
        UiCommand::RefreshProfiles => {
            let mut snapshots = Vec::new();
            for (profile, generation) in application.profiles_with_generations_snapshot().await {
                let profile_id = crate::model::ProfileId(profile.id.clone());
                let has_current_session_secret =
                    match application.has_current_session_secret(&profile_id) {
                        Ok(value) => value,
                        Err(error) => {
                            return UiEvent::ProfilesFailed(error.public_error_parts().0);
                        }
                    };
                snapshots.push(ProfileSnapshot::from_profile(
                    &profile,
                    generation,
                    has_current_session_secret,
                ));
            }
            UiEvent::ProfilesLoaded(snapshots)
        }
        UiCommand::CreateProfile(request) => {
            let operation_id = request.operation_id;
            let profile_id = request.explicit_id.clone().unwrap_or_else(|| {
                crate::model::ProfileId(crate::service::slugify_profile_id(&request.draft.name))
            });
            let outcome = application.create_profile(request).await;
            match outcome {
                Ok(outcome) => UiEvent::ProfileSaved {
                    operation_id: outcome.operation_id,
                    profile_id: outcome.profile_id,
                    warning: commit_warning(outcome.commit_state),
                },
                Err(error) => UiEvent::ProfileSaveFailed {
                    operation_id,
                    profile_id,
                    summary: error.public_error_parts().0,
                },
            }
        }
        UiCommand::UpdateProfile(request) => {
            let operation_id = request.operation_id;
            let profile_id = request.profile_id.clone();
            match application.update_profile(request).await {
                Ok(outcome) => UiEvent::ProfileSaved {
                    operation_id: outcome.operation_id,
                    profile_id: outcome.profile_id,
                    warning: commit_warning(outcome.commit_state),
                },
                Err(error) => UiEvent::ProfileSaveFailed {
                    operation_id,
                    profile_id,
                    summary: error.public_error_parts().0,
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
                error.public_error_parts().0,
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
                error.public_error_parts().0,
            ),
        },
    }
}

fn failed(
    operation_id: crate::model::OperationId,
    profile_id: crate::model::ProfileId,
    kind: OperationKind,
    summary: PublicSummary,
) -> UiEvent {
    UiEvent::OperationFailed {
        operation_id,
        profile_id,
        kind,
        summary,
    }
}

fn service_error_event(command: UiCommand, summary: PublicSummary) -> UiEvent {
    match command {
        UiCommand::RefreshProfiles => UiEvent::ProfilesFailed(summary),
        UiCommand::CreateProfile(request) => UiEvent::ProfileSaveFailed {
            operation_id: request.operation_id,
            profile_id: request.explicit_id.unwrap_or_else(|| {
                crate::model::ProfileId(crate::service::slugify_profile_id(&request.draft.name))
            }),
            summary,
        },
        UiCommand::UpdateProfile(request) => UiEvent::ProfileSaveFailed {
            operation_id: request.operation_id,
            profile_id: request.profile_id,
            summary,
        },
        UiCommand::TestConnection {
            operation_id,
            profile_id,
        } => failed(operation_id, profile_id, OperationKind::Connection, summary),
        UiCommand::Execute {
            operation_id,
            profile_id,
            ..
        } => failed(operation_id, profile_id, OperationKind::Execute, summary),
    }
}

const fn commit_warning(commit_state: CommitState) -> Option<PublicSummary> {
    match commit_state {
        CommitState::CommittedDurabilityUnknown => Some(PublicSummary::CommittedDurabilityUnknown),
        CommitState::NotCommitted | CommitState::Committed => None,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::config::MigrationConsent;
    use crate::model::{
        ConnectionDraft, CredentialMode, DraftId, DriverKind, OperationId, ProfileId,
        PublicSummary, QueryLanguage,
    };
    use crate::secrets::{SessionSecret, SessionSecretUpdate};
    use crate::service::{ApplicationService, CreateProfileRequest};

    use super::{UiCommand, UiEvent, handle_command};

    #[tokio::test]
    async fn saved_profile_is_in_the_live_service_and_next_refresh() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let service = ApplicationService::load_path(&path).expect("service");

        let saved = handle_command(
            &service,
            UiCommand::CreateProfile(CreateProfileRequest {
                draft_id: DraftId(1),
                operation_id: OperationId(11),
                explicit_id: Some(ProfileId("runtime-profile".to_owned())),
                draft: draft(DriverKind::Redis),
                secret_update: SessionSecretUpdate::Clear,
                migration_consent: MigrationConsent::Cancelled,
            }),
        )
        .await;

        assert!(matches!(
            saved,
            UiEvent::ProfileSaved {
                operation_id: OperationId(11),
                profile_id,
                warning: None,
            } if profile_id == ProfileId("runtime-profile".to_owned())
        ));
        assert_eq!(
            service
                .language_for(&ProfileId("runtime-profile".to_owned()))
                .await
                .expect("saved profile is immediately in the live service"),
            QueryLanguage::RedisCommand
        );

        let refreshed = handle_command(&service, UiCommand::RefreshProfiles).await;
        assert!(matches!(
            refreshed,
            UiEvent::ProfilesLoaded(profiles)
                if profiles.len() == 1
                    && profiles[0].id == ProfileId("runtime-profile".to_owned())
        ));
    }

    #[tokio::test]
    async fn refresh_exposes_only_the_safe_current_session_secret_boolean() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let service = ApplicationService::load_path(&path).expect("service");
        let mut session_draft = draft(DriverKind::Redis);
        session_draft.credential_mode = CredentialMode::Session;
        let saved = handle_command(
            &service,
            UiCommand::CreateProfile(CreateProfileRequest {
                draft_id: DraftId(20),
                operation_id: OperationId(20),
                explicit_id: Some(ProfileId("session-profile".to_owned())),
                draft: session_draft,
                secret_update: SessionSecretUpdate::Replace(Arc::new(SessionSecret::new(
                    "must-never-reach-the-snapshot".to_owned(),
                ))),
                migration_consent: MigrationConsent::Cancelled,
            }),
        )
        .await;
        assert!(matches!(saved, UiEvent::ProfileSaved { .. }));

        let refreshed = handle_command(&service, UiCommand::RefreshProfiles).await;
        assert!(matches!(
            refreshed,
            UiEvent::ProfilesLoaded(profiles)
                if profiles.len() == 1
                    && profiles[0].has_current_session_secret
                    && !format!("{:?}", profiles[0]).contains("must-never-reach-the-snapshot")
        ));

        let restarted = ApplicationService::load_path(&path).expect("restart service");
        let refreshed_after_restart = handle_command(&restarted, UiCommand::RefreshProfiles).await;
        assert!(matches!(
            refreshed_after_restart,
            UiEvent::ProfilesLoaded(profiles)
                if profiles.len() == 1 && !profiles[0].has_current_session_secret
        ));
    }

    #[tokio::test]
    async fn invalid_create_emits_static_failure_without_writing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let service = ApplicationService::load_path(&path).expect("service");
        let mut invalid = draft(DriverKind::MySql);
        invalid.host.clear();

        let event = handle_command(
            &service,
            UiCommand::CreateProfile(CreateProfileRequest {
                draft_id: DraftId(2),
                operation_id: OperationId(12),
                explicit_id: Some(ProfileId("runtime-profile".to_owned())),
                draft: invalid,
                secret_update: SessionSecretUpdate::Clear,
                migration_consent: MigrationConsent::Cancelled,
            }),
        )
        .await;

        assert!(matches!(
            event,
            UiEvent::ProfileSaveFailed {
                operation_id: OperationId(12),
                profile_id,
                summary: PublicSummary::InvalidInput,
            } if profile_id == ProfileId("runtime-profile".to_owned())
        ));
        assert!(!path.exists());
        assert!(service.profiles_snapshot().await.is_empty());
    }

    #[test]
    fn durability_unknown_commit_is_a_typed_ui_warning() {
        assert_eq!(
            super::commit_warning(crate::config::CommitState::CommittedDurabilityUnknown),
            Some(PublicSummary::CommittedDurabilityUnknown)
        );
        assert_eq!(
            super::commit_warning(crate::config::CommitState::Committed),
            None
        );
    }

    fn draft(driver: DriverKind) -> ConnectionDraft {
        let mut draft = ConnectionDraft::for_driver(driver);
        draft.name = "Runtime profile".to_owned();
        draft.credential_mode = CredentialMode::None;
        draft
    }
}
