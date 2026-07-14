//! Pure UI snapshots and event fold. No driver or network client belongs here.

use std::collections::HashMap;

use crate::model::{
    ConnectionProfile, DriverAvailability, DriverKind, OperationId, ProfileGeneration, ProfileId,
    PublicSummary, QueryResult,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfileSnapshot {
    pub id: ProfileId,
    pub generation: ProfileGeneration,
    pub name: String,
    pub driver: DriverKind,
    pub endpoint: String,
    pub database: Option<String>,
    pub availability: DriverAvailability,
    pub planned_reason: Option<String>,
    pub has_current_session_secret: bool,
    pub persisted: ConnectionProfile,
}

impl ProfileSnapshot {
    pub fn from_profile(
        profile: &ConnectionProfile,
        generation: ProfileGeneration,
        has_current_session_secret: bool,
    ) -> Self {
        let descriptor = crate::drivers::descriptors()
            .into_iter()
            .find(|descriptor| descriptor.kind == profile.driver);
        let availability =
            descriptor.map_or(DriverAvailability::Planned, |value| value.availability);
        Self {
            id: ProfileId(profile.id.clone()),
            generation,
            name: profile.name.clone(),
            driver: profile.driver,
            endpoint: profile.redacted_endpoint(),
            database: profile.database.clone(),
            availability,
            planned_reason: descriptor.and_then(|value| value.reason).map(str::to_owned),
            has_current_session_secret,
            persisted: profile.clone(),
        }
    }

    pub fn is_ready(&self) -> bool {
        self.availability == DriverAvailability::Ready
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectionState {
    Disconnected,
    Pending(OperationId),
    Connected { elapsed_ms: u64 },
    Failed { summary: PublicSummary },
}

impl ConnectionState {
    pub fn is_pending(&self) -> bool {
        matches!(self, Self::Pending(_))
    }
}

#[derive(Clone, Debug)]
pub enum UiEvent {
    ProfilesLoaded(Vec<ProfileSnapshot>),
    ProfilesFailed(PublicSummary),
    ProfileSaved {
        operation_id: OperationId,
        profile_id: ProfileId,
        warning: Option<PublicSummary>,
    },
    ProfileSaveFailed {
        operation_id: OperationId,
        profile_id: ProfileId,
        summary: PublicSummary,
    },
    ConnectionReady {
        operation_id: OperationId,
        profile_id: ProfileId,
        elapsed_ms: u64,
    },
    QueryFinished {
        operation_id: OperationId,
        profile_id: ProfileId,
        result: QueryResult,
    },
    OperationFailed {
        operation_id: OperationId,
        profile_id: ProfileId,
        kind: OperationKind,
        summary: PublicSummary,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OperationKind {
    Connection,
    Execute,
}

pub struct UiModel {
    pub profiles: Vec<ProfileSnapshot>,
    pub selected_profile: Option<ProfileId>,
    pub connection_states: HashMap<ProfileId, ConnectionState>,
    pub editor_text: String,
    pub pending_execute: Option<(OperationId, ProfileId)>,
    pub result: Option<QueryResult>,
    pub status: String,
    next_operation_id: u64,
}

impl Default for UiModel {
    fn default() -> Self {
        Self {
            profiles: Vec::new(),
            selected_profile: None,
            connection_states: HashMap::new(),
            editor_text: String::new(),
            pending_execute: None,
            result: None,
            status: "Loading profiles…".to_owned(),
            next_operation_id: 1,
        }
    }
}

impl UiModel {
    pub fn next_operation(&mut self) -> OperationId {
        let operation_id = OperationId(self.next_operation_id);
        self.next_operation_id = self.next_operation_id.saturating_add(1);
        operation_id
    }

    pub fn selected_profile_snapshot(&self) -> Option<&ProfileSnapshot> {
        let selected = self.selected_profile.as_ref()?;
        self.profiles.iter().find(|profile| profile.id == *selected)
    }

    pub fn connection_state(&self, profile_id: &ProfileId) -> &ConnectionState {
        self.connection_states
            .get(profile_id)
            .unwrap_or(&ConnectionState::Disconnected)
    }

    pub fn fold(&mut self, event: UiEvent) {
        match event {
            UiEvent::ProfilesLoaded(profiles) => self.fold_profiles(profiles),
            UiEvent::ProfilesFailed(summary) => self.status = summary.message().to_owned(),
            UiEvent::ProfileSaved { warning, .. } => {
                if let Some(summary) = warning {
                    self.status = summary.message().to_owned();
                }
            }
            UiEvent::ProfileSaveFailed { summary, .. } => {
                self.status = summary.message().to_owned();
            }
            UiEvent::ConnectionReady {
                operation_id,
                profile_id,
                elapsed_ms,
            } => {
                if self.connection_states.get(&profile_id)
                    == Some(&ConnectionState::Pending(operation_id))
                {
                    self.connection_states
                        .insert(profile_id, ConnectionState::Connected { elapsed_ms });
                    self.status = format!("Connection ready in {elapsed_ms} ms");
                }
            }
            UiEvent::QueryFinished {
                operation_id,
                profile_id,
                result,
            } => {
                if self.pending_execute.as_ref() == Some(&(operation_id, profile_id)) {
                    self.pending_execute = None;
                    self.status = format!("Query finished in {} ms", result.elapsed_ms);
                    self.result = Some(result);
                }
            }
            UiEvent::OperationFailed {
                operation_id,
                profile_id,
                kind,
                summary,
            } => match kind {
                OperationKind::Connection => {
                    if self.connection_states.get(&profile_id)
                        == Some(&ConnectionState::Pending(operation_id))
                    {
                        self.connection_states
                            .insert(profile_id, ConnectionState::Failed { summary });
                        self.status = summary.message().to_owned();
                    }
                }
                OperationKind::Execute => {
                    if self.pending_execute.as_ref() == Some(&(operation_id, profile_id)) {
                        self.pending_execute = None;
                        self.status = summary.message().to_owned();
                    }
                }
            },
        }
    }

    fn fold_profiles(&mut self, profiles: Vec<ProfileSnapshot>) {
        self.connection_states.retain(|profile_id, _| {
            let previous = self
                .profiles
                .iter()
                .find(|profile| profile.id == *profile_id);
            let refreshed = profiles.iter().find(|profile| profile.id == *profile_id);
            matches!((previous, refreshed), (Some(previous), Some(refreshed)) if previous.persisted == refreshed.persisted)
        });
        if self
            .selected_profile
            .as_ref()
            .is_none_or(|selected| !profiles.iter().any(|profile| profile.id == *selected))
        {
            self.selected_profile = profiles.first().map(|profile| profile.id.clone());
        }
        self.profiles = profiles;
        self.status = format!("{} profiles loaded", self.profiles.len());
    }
}

#[cfg(test)]
mod tests {
    use super::{ConnectionState, ProfileSnapshot, UiEvent, UiModel};
    use crate::model::{
        ConnectionProfile, CredentialMode, DriverAvailability, DriverKind, OperationId,
        ProfileGeneration, ProfileId, QueryResult, RedisTlsConfig, TlsMode,
    };

    fn result(elapsed_ms: u128) -> QueryResult {
        QueryResult {
            columns: Vec::new(),
            rows: Vec::new(),
            affected_rows: 0,
            last_insert_id: None,
            elapsed_ms,
            truncated: false,
            notices: Vec::new(),
        }
    }

    #[test]
    fn stale_query_event_does_not_overwrite_result() {
        let profile_id = ProfileId("mysql-local".to_owned());
        let mut model = UiModel {
            pending_execute: Some((OperationId(2), profile_id.clone())),
            result: Some(result(7)),
            ..UiModel::default()
        };

        model.fold(UiEvent::QueryFinished {
            operation_id: OperationId(1),
            profile_id,
            result: result(99),
        });

        assert_eq!(
            model.pending_execute.map(|pending| pending.0),
            Some(OperationId(2))
        );
        assert_eq!(model.result.as_ref().map(|value| value.elapsed_ms), Some(7));
    }

    #[test]
    fn refreshed_changed_profile_clears_stale_connection_state() {
        let profile_id = ProfileId("mysql-local".to_owned());
        let original = profile(3306);
        let mut model = UiModel {
            profiles: vec![original],
            connection_states: [(
                profile_id.clone(),
                ConnectionState::Connected { elapsed_ms: 5 },
            )]
            .into(),
            ..UiModel::default()
        };

        model.fold(UiEvent::ProfilesLoaded(vec![profile(3307)]));

        assert_eq!(
            model.connection_state(&profile_id),
            &ConnectionState::Disconnected
        );
    }

    fn profile(port: u16) -> ProfileSnapshot {
        let persisted = ConnectionProfile {
            id: "mysql-local".to_owned(),
            name: "MySQL".to_owned(),
            driver: DriverKind::MySql,
            host: "127.0.0.1".to_owned(),
            port,
            database: None,
            username: None,
            tls: TlsMode::Disabled,
            credential_mode: CredentialMode::None,
            secret_env: None,
            redis_tls: RedisTlsConfig::default(),
        };
        ProfileSnapshot {
            id: ProfileId(persisted.id.clone()),
            generation: ProfileGeneration(1),
            name: persisted.name.clone(),
            driver: persisted.driver,
            endpoint: persisted.redacted_endpoint(),
            database: None,
            availability: DriverAvailability::Ready,
            planned_reason: None,
            has_current_session_secret: false,
            persisted,
        }
    }
}
