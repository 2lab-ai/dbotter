//! Pure UI snapshots and event folding. No driver or network client belongs here.

use std::collections::HashMap;

use crate::model::{
    CatalogPage, CatalogRequest, ConnectionProfile, CredentialMode, DraftId, DriverAvailability,
    DriverKind, OperationId, OperationKind, ProfileGeneration, ProfileId, PublicSummary,
    RedisKeyInspectRequest, RedisKeyPage, RedisScanRequest, RedisValuePreview, ResultSnapshot,
    SessionGeneration,
};
use crate::public_error::PublicOperationError;
use crate::service::SessionDisposition;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct WorkspaceKey {
    pub profile_id: ProfileId,
    pub profile_generation: ProfileGeneration,
}

impl WorkspaceKey {
    pub fn new(profile_id: ProfileId, profile_generation: ProfileGeneration) -> Self {
        Self {
            profile_id,
            profile_generation,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct ProfileWorkspace {
    pub editor_text: String,
    pub row_limit: String,
    pub timeout_seconds: String,
    pub pending_execute: Option<OperationId>,
    pub result: Option<ResultSnapshot>,
    pub error: Option<PublicOperationError>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PostCloseState {
    Disconnected,
    NeedsCredential,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionFailureOutcome {
    Preserve,
    Disconnected,
    Unknown,
    NeedsCredential,
}

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
    Connected {
        session_generation: SessionGeneration,
        elapsed_ms: u64,
    },
    NeedsCredential,
    Failed {
        summary: PublicSummary,
    },
    Closing,
}

impl ConnectionState {
    pub fn is_pending(&self) -> bool {
        matches!(self, Self::Pending(_))
    }
}

#[derive(Clone, Debug)]
pub enum UiEvent {
    ProfilesLoaded {
        operation_id: OperationId,
        profiles: Vec<ProfileSnapshot>,
    },
    ProfilesFailed {
        operation_id: OperationId,
        summary: PublicSummary,
    },
    ProfileSaved {
        operation_id: OperationId,
        profile_id: ProfileId,
        previous_generation: Option<ProfileGeneration>,
        profile_generation: ProfileGeneration,
        session_retained: bool,
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
        profile_generation: ProfileGeneration,
        session_generation: SessionGeneration,
        elapsed_ms: u64,
    },
    ConnectionClosed {
        operation_id: OperationId,
        profile_id: ProfileId,
        profile_generation: ProfileGeneration,
        post_close: PostCloseState,
    },
    DraftConnectionReady {
        operation_id: OperationId,
        draft_id: DraftId,
        elapsed_ms: u64,
    },
    DraftOperationFailed {
        operation_id: OperationId,
        draft_id: DraftId,
        summary: PublicSummary,
    },
    QueryFinished {
        operation_id: OperationId,
        profile_id: ProfileId,
        profile_generation: ProfileGeneration,
        session_generation: SessionGeneration,
        result: ResultSnapshot,
    },
    CatalogPageLoaded {
        page: CatalogPage,
        session_generation: SessionGeneration,
        session_disposition: SessionDisposition,
    },
    CatalogPageFailed {
        request: CatalogRequest,
        summary: PublicSummary,
        session_generation: Option<SessionGeneration>,
        session_disposition: Option<SessionDisposition>,
    },
    RedisKeysLoaded {
        page: RedisKeyPage,
        session_generation: SessionGeneration,
        session_disposition: SessionDisposition,
    },
    RedisKeysFailed {
        request: RedisScanRequest,
        error: PublicOperationError,
        session_generation: Option<SessionGeneration>,
        session_disposition: Option<SessionDisposition>,
        connection_outcome: ConnectionFailureOutcome,
    },
    RedisKeyInspected {
        preview: RedisValuePreview,
        session_generation: SessionGeneration,
        session_disposition: SessionDisposition,
    },
    RedisKeyInspectFailed {
        request: RedisKeyInspectRequest,
        error: PublicOperationError,
        session_generation: Option<SessionGeneration>,
        session_disposition: Option<SessionDisposition>,
        connection_outcome: ConnectionFailureOutcome,
    },
    OperationFailed {
        operation_id: OperationId,
        profile_id: ProfileId,
        profile_generation: ProfileGeneration,
        session_generation: Option<SessionGeneration>,
        kind: OperationKind,
        summary: PublicSummary,
        session_disposition: Option<SessionDisposition>,
        connection_outcome: ConnectionFailureOutcome,
    },
    ExecuteUnavailable {
        operation_id: OperationId,
        profile_id: ProfileId,
        profile_generation: ProfileGeneration,
        summary: PublicSummary,
    },
    ProfileDeleted {
        operation_id: OperationId,
        profile_id: ProfileId,
        profile_generation: ProfileGeneration,
        server_state_unknown: bool,
    },
    ConfigUncertain {
        operation_id: OperationId,
    },
    RuntimeShutdown {
        operation_id: OperationId,
    },
}

pub struct UiModel {
    pub profiles: Vec<ProfileSnapshot>,
    pub selected_profile: Option<ProfileId>,
    pub active_generations: HashMap<ProfileId, ProfileGeneration>,
    pub tombstones: HashMap<ProfileId, ProfileGeneration>,
    pub connection_states: HashMap<ProfileId, ConnectionState>,
    pub workspaces: HashMap<WorkspaceKey, ProfileWorkspace>,
    pub editor_text: String,
    pub pending_execute: Option<(OperationId, ProfileId, ProfileGeneration)>,
    pub result: Option<ResultSnapshot>,
    pub catalog_pages: HashMap<ProfileId, CatalogPage>,
    pub catalog_retry: HashMap<ProfileId, CatalogRequest>,
    pub redis_key_pages: HashMap<ProfileId, RedisKeyPage>,
    pub redis_scan_retry: HashMap<ProfileId, RedisScanRequest>,
    pub redis_scan_errors: HashMap<ProfileId, PublicOperationError>,
    pub redis_value_previews: HashMap<ProfileId, RedisValuePreview>,
    pub redis_inspect_retry: HashMap<ProfileId, RedisKeyInspectRequest>,
    pub redis_inspect_errors: HashMap<ProfileId, PublicOperationError>,
    pub status: String,
    config_uncertain: bool,
    last_profiles_operation: Option<OperationId>,
    pending_retags: HashMap<ProfileId, (ProfileGeneration, ProfileGeneration)>,
    next_operation_id: u64,
}

impl Default for UiModel {
    fn default() -> Self {
        Self {
            profiles: Vec::new(),
            selected_profile: None,
            active_generations: HashMap::new(),
            tombstones: HashMap::new(),
            connection_states: HashMap::new(),
            workspaces: HashMap::new(),
            editor_text: String::new(),
            pending_execute: None,
            result: None,
            catalog_pages: HashMap::new(),
            catalog_retry: HashMap::new(),
            redis_key_pages: HashMap::new(),
            redis_scan_retry: HashMap::new(),
            redis_scan_errors: HashMap::new(),
            redis_value_previews: HashMap::new(),
            redis_inspect_retry: HashMap::new(),
            redis_inspect_errors: HashMap::new(),
            status: "Loading profiles…".to_owned(),
            config_uncertain: false,
            last_profiles_operation: None,
            pending_retags: HashMap::new(),
            next_operation_id: 1,
        }
    }
}

impl UiModel {
    pub fn workspace(&self, key: &WorkspaceKey) -> Option<&ProfileWorkspace> {
        self.workspaces.get(key)
    }

    pub fn workspace_mut(&mut self, key: WorkspaceKey) -> &mut ProfileWorkspace {
        self.workspaces
            .entry(key)
            .or_insert_with(|| ProfileWorkspace {
                row_limit: "500".to_owned(),
                timeout_seconds: "30".to_owned(),
                ..ProfileWorkspace::default()
            })
    }

    pub fn selected_workspace_key(&self) -> Option<WorkspaceKey> {
        let profile = self.selected_profile_snapshot()?;
        Some(WorkspaceKey::new(profile.id.clone(), profile.generation))
    }

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

    pub fn active_generation(&self, profile_id: &ProfileId) -> Option<ProfileGeneration> {
        self.active_generations.get(profile_id).copied()
    }

    pub fn tombstone_generation(&self, profile_id: &ProfileId) -> Option<ProfileGeneration> {
        self.tombstones.get(profile_id).copied()
    }

    pub fn is_config_uncertain(&self) -> bool {
        self.config_uncertain
    }

    pub fn fold(&mut self, event: UiEvent) {
        match event {
            UiEvent::ProfilesLoaded {
                operation_id,
                profiles,
            } => {
                if !self.accept_profiles_operation(operation_id) {
                    return;
                }
                self.fold_profiles(profiles);
            }
            UiEvent::ProfilesFailed {
                operation_id,
                summary,
            } => {
                if !self.accept_profiles_operation(operation_id) {
                    return;
                }
                self.status = summary.message().to_owned();
            }
            UiEvent::ProfileSaved {
                profile_id,
                previous_generation,
                profile_generation,
                session_retained,
                warning,
                ..
            } => {
                let save_is_current = self
                    .tombstones
                    .get(&profile_id)
                    .is_none_or(|tombstone| profile_generation.0 > tombstone.0)
                    && match previous_generation {
                        Some(previous) => {
                            self.active_generations
                                .get(&profile_id)
                                .is_some_and(|active| {
                                    *active == previous || *active == profile_generation
                                })
                        }
                        None => self
                            .active_generations
                            .get(&profile_id)
                            .is_none_or(|active| active.0 <= profile_generation.0),
                    };
                if !save_is_current {
                    return;
                }
                if session_retained && let Some(previous_generation) = previous_generation {
                    self.pending_retags.insert(
                        profile_id.clone(),
                        (previous_generation, profile_generation),
                    );
                } else {
                    self.pending_retags.remove(&profile_id);
                    self.connection_states.remove(&profile_id);
                }
                self.active_generations
                    .insert(profile_id, profile_generation);
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
                profile_generation,
                session_generation,
                elapsed_ms,
            } => {
                if self.event_is_current(&profile_id, profile_generation)
                    && self.connection_states.get(&profile_id)
                        == Some(&ConnectionState::Pending(operation_id))
                {
                    self.connection_states.insert(
                        profile_id,
                        ConnectionState::Connected {
                            session_generation,
                            elapsed_ms,
                        },
                    );
                    self.status = format!("Connection ready in {elapsed_ms} ms");
                }
            }
            UiEvent::ConnectionClosed {
                operation_id,
                profile_id,
                profile_generation,
                post_close,
            } => {
                if self.event_is_current(&profile_id, profile_generation)
                    && self.connection_states.get(&profile_id)
                        == Some(&ConnectionState::Pending(operation_id))
                {
                    self.connection_states.insert(
                        profile_id,
                        match post_close {
                            PostCloseState::Disconnected => ConnectionState::Disconnected,
                            PostCloseState::NeedsCredential => ConnectionState::NeedsCredential,
                        },
                    );
                    self.status = "Disconnected".to_owned();
                }
            }
            UiEvent::DraftConnectionReady { elapsed_ms, .. } => {
                self.status = format!("Draft connection ready in {elapsed_ms} ms");
            }
            UiEvent::DraftOperationFailed { summary, .. }
            | UiEvent::ExecuteUnavailable { summary, .. } => {
                self.pending_execute = None;
                self.status = summary.message().to_owned();
            }
            UiEvent::QueryFinished {
                operation_id,
                profile_id,
                profile_generation,
                session_generation,
                result,
            } => {
                if self.event_is_current(&profile_id, profile_generation)
                    && self.pending_execute.as_ref()
                        == Some(&(operation_id, profile_id.clone(), profile_generation))
                {
                    self.pending_execute = None;
                    self.status = format!("Query finished in {} ms", result.provenance.duration_ms);
                    self.result = Some(result);
                    self.connection_states.insert(
                        profile_id,
                        ConnectionState::Connected {
                            session_generation,
                            elapsed_ms: 0,
                        },
                    );
                }
            }
            UiEvent::CatalogPageLoaded {
                page,
                session_generation,
                session_disposition,
            } => {
                let profile_id = page.identity.profile_id.clone();
                if self.event_is_current(&profile_id, page.identity.profile_generation) {
                    self.fold_catalog_session(
                        &profile_id,
                        Some(session_generation),
                        Some(session_disposition),
                    );
                    self.catalog_retry.remove(&profile_id);
                    self.catalog_pages.insert(profile_id, page);
                    self.status = "Catalog page loaded".to_owned();
                }
            }
            UiEvent::CatalogPageFailed {
                request,
                summary,
                session_generation,
                session_disposition,
            } => {
                let profile_id = request.profile_id().clone();
                if self.event_is_current(&profile_id, request.profile_generation()) {
                    self.fold_catalog_session(&profile_id, session_generation, session_disposition);
                    if let Some(page) = self.catalog_pages.get_mut(&profile_id) {
                        page.stale = true;
                    }
                    self.catalog_retry.insert(profile_id, request);
                    self.status = summary.message().to_owned();
                }
            }
            UiEvent::RedisKeysLoaded {
                page,
                session_generation,
                session_disposition,
            } => {
                let profile_id = page.identity.profile_id.clone();
                if self.event_is_current(&profile_id, page.identity.profile_generation) {
                    self.redis_scan_retry.remove(&profile_id);
                    self.redis_scan_errors.remove(&profile_id);
                    self.redis_key_pages.insert(profile_id.clone(), page);
                    self.apply_redis_session_truth(
                        profile_id,
                        Some(session_generation),
                        Some(session_disposition),
                        ConnectionFailureOutcome::Preserve,
                    );
                    self.status = "Redis keys loaded".to_owned();
                }
            }
            UiEvent::RedisKeysFailed {
                request,
                error,
                session_generation,
                session_disposition,
                connection_outcome,
            } => {
                let profile_id = request.profile_id().clone();
                if self.event_is_current(&profile_id, request.profile_generation()) {
                    if let Some(page) = self.redis_key_pages.get_mut(&profile_id) {
                        page.stale = true;
                    }
                    self.redis_scan_retry.insert(profile_id.clone(), request);
                    self.status = error.summary.message().to_owned();
                    self.redis_scan_errors.insert(profile_id.clone(), error);
                    self.apply_redis_session_truth(
                        profile_id,
                        session_generation,
                        session_disposition,
                        connection_outcome,
                    );
                }
            }
            UiEvent::RedisKeyInspected {
                preview,
                session_generation,
                session_disposition,
            } => {
                let profile_id = preview.identity.profile_id.clone();
                if self.event_is_current(&profile_id, preview.identity.profile_generation) {
                    self.redis_inspect_retry.remove(&profile_id);
                    self.redis_inspect_errors.remove(&profile_id);
                    self.redis_value_previews
                        .insert(profile_id.clone(), preview);
                    self.apply_redis_session_truth(
                        profile_id,
                        Some(session_generation),
                        Some(session_disposition),
                        ConnectionFailureOutcome::Preserve,
                    );
                    self.status = "Redis key inspected".to_owned();
                }
            }
            UiEvent::RedisKeyInspectFailed {
                request,
                error,
                session_generation,
                session_disposition,
                connection_outcome,
            } => {
                let profile_id = request.profile_id().clone();
                if self.event_is_current(&profile_id, request.profile_generation()) {
                    if let Some(preview) = self.redis_value_previews.get_mut(&profile_id) {
                        preview.stale = true;
                    }
                    self.redis_inspect_retry.insert(profile_id.clone(), request);
                    self.status = error.summary.message().to_owned();
                    self.redis_inspect_errors.insert(profile_id.clone(), error);
                    self.apply_redis_session_truth(
                        profile_id,
                        session_generation,
                        session_disposition,
                        connection_outcome,
                    );
                }
            }
            UiEvent::OperationFailed {
                operation_id,
                profile_id,
                profile_generation,
                kind,
                summary,
                connection_outcome,
                ..
            } => {
                if !self.event_is_current(&profile_id, profile_generation) {
                    return;
                }
                let is_connection_attempt = matches!(
                    kind,
                    OperationKind::ConnectProfile | OperationKind::ReconnectProfile
                );
                let connection_outcome_is_correlated = !matches!(
                    self.connection_states.get(&profile_id),
                    Some(ConnectionState::Pending(pending)) if *pending != operation_id
                );
                match kind {
                    OperationKind::ConnectProfile | OperationKind::ReconnectProfile => {
                        if self.connection_states.get(&profile_id)
                            == Some(&ConnectionState::Pending(operation_id))
                        {
                            let visible_state = if summary == PublicSummary::CredentialRequired
                                && connection_outcome == ConnectionFailureOutcome::NeedsCredential
                            {
                                ConnectionState::NeedsCredential
                            } else {
                                ConnectionState::Failed { summary }
                            };
                            self.connection_states
                                .insert(profile_id.clone(), visible_state);
                            self.status = summary.message().to_owned();
                        }
                    }
                    OperationKind::DisconnectProfile => {
                        if self.connection_states.get(&profile_id)
                            == Some(&ConnectionState::Pending(operation_id))
                        {
                            self.connection_states
                                .insert(profile_id.clone(), ConnectionState::Failed { summary });
                            self.status = summary.message().to_owned();
                        }
                    }
                    OperationKind::ExecuteRead | OperationKind::ExecuteMutation => {
                        if self.pending_execute.as_ref()
                            == Some(&(operation_id, profile_id.clone(), profile_generation))
                        {
                            self.pending_execute = None;
                            self.status = summary.message().to_owned();
                        }
                    }
                    _ => self.status = summary.message().to_owned(),
                }
                if !is_connection_attempt && connection_outcome_is_correlated {
                    match connection_outcome {
                        ConnectionFailureOutcome::Preserve => {}
                        ConnectionFailureOutcome::Disconnected
                        | ConnectionFailureOutcome::Unknown => {
                            self.connection_states
                                .insert(profile_id.clone(), ConnectionState::Disconnected);
                        }
                        ConnectionFailureOutcome::NeedsCredential => {
                            self.connection_states
                                .insert(profile_id, ConnectionState::NeedsCredential);
                        }
                    }
                }
            }
            UiEvent::ProfileDeleted {
                profile_id,
                profile_generation,
                server_state_unknown,
                ..
            } => self.fold_deleted(profile_id, profile_generation, server_state_unknown),
            UiEvent::ConfigUncertain { operation_id } => {
                if !self.accept_profiles_operation(operation_id) {
                    return;
                }
                self.config_uncertain = true;
                self.pending_retags.clear();
                self.connection_states.clear();
                self.pending_execute = None;
                self.status = "Configuration state is uncertain.".to_owned();
            }
            UiEvent::RuntimeShutdown { .. } => {
                for profile_id in self.active_generations.keys() {
                    self.connection_states
                        .insert(profile_id.clone(), ConnectionState::Closing);
                }
                self.pending_execute = None;
                self.status = "Runtime shut down".to_owned();
            }
        }
    }

    fn apply_redis_session_truth(
        &mut self,
        profile_id: ProfileId,
        session_generation: Option<SessionGeneration>,
        session_disposition: Option<SessionDisposition>,
        connection_outcome: ConnectionFailureOutcome,
    ) {
        match (session_generation, session_disposition) {
            (Some(session_generation), Some(SessionDisposition::Keep)) => {
                self.connection_states.insert(
                    profile_id,
                    ConnectionState::Connected {
                        session_generation,
                        elapsed_ms: 0,
                    },
                );
            }
            (_, Some(SessionDisposition::Evict)) => {
                self.connection_states
                    .insert(profile_id, ConnectionState::Disconnected);
            }
            _ => match connection_outcome {
                ConnectionFailureOutcome::Preserve => {}
                ConnectionFailureOutcome::Disconnected | ConnectionFailureOutcome::Unknown => {
                    self.connection_states
                        .insert(profile_id, ConnectionState::Disconnected);
                }
                ConnectionFailureOutcome::NeedsCredential => {
                    self.connection_states
                        .insert(profile_id, ConnectionState::NeedsCredential);
                }
            },
        }
    }

    fn accept_profiles_operation(&mut self, operation_id: OperationId) -> bool {
        if self
            .last_profiles_operation
            .is_some_and(|latest| operation_id.0 <= latest.0)
        {
            return false;
        }
        self.last_profiles_operation = Some(operation_id);
        true
    }

    fn event_is_current(&self, profile_id: &ProfileId, generation: ProfileGeneration) -> bool {
        !self.config_uncertain
            && self.active_generations.get(profile_id).copied() == Some(generation)
            && self
                .tombstones
                .get(profile_id)
                .is_none_or(|tombstone| generation.0 > tombstone.0)
    }

    fn fold_catalog_session(
        &mut self,
        profile_id: &ProfileId,
        session_generation: Option<SessionGeneration>,
        disposition: Option<SessionDisposition>,
    ) {
        let (Some(session_generation), Some(disposition)) = (session_generation, disposition)
        else {
            return;
        };
        let current = self.connection_states.get(profile_id);
        let protected = matches!(
            current,
            Some(ConnectionState::Pending(_) | ConnectionState::Closing)
        ) || matches!(
            current,
            Some(ConnectionState::Connected {
                session_generation: visible,
                ..
            }) if *visible != session_generation
        );
        if protected {
            return;
        }
        let next = match disposition {
            SessionDisposition::Keep => ConnectionState::Connected {
                session_generation,
                elapsed_ms: 0,
            },
            SessionDisposition::Evict => ConnectionState::Disconnected,
        };
        self.connection_states.insert(profile_id.clone(), next);
    }

    fn fold_deleted(
        &mut self,
        profile_id: ProfileId,
        deletion_generation: ProfileGeneration,
        server_state_unknown: bool,
    ) {
        if self
            .active_generations
            .get(&profile_id)
            .is_some_and(|active| active.0 >= deletion_generation.0)
        {
            return;
        }
        self.tombstones
            .entry(profile_id.clone())
            .and_modify(|current| {
                if deletion_generation.0 > current.0 {
                    *current = deletion_generation;
                }
            })
            .or_insert(deletion_generation);
        self.active_generations.remove(&profile_id);
        self.profiles.retain(|profile| profile.id != profile_id);
        self.connection_states.remove(&profile_id);
        self.catalog_pages.remove(&profile_id);
        self.catalog_retry.remove(&profile_id);
        self.redis_key_pages.remove(&profile_id);
        self.redis_scan_retry.remove(&profile_id);
        self.redis_scan_errors.remove(&profile_id);
        self.redis_value_previews.remove(&profile_id);
        self.redis_inspect_retry.remove(&profile_id);
        self.redis_inspect_errors.remove(&profile_id);
        if self
            .pending_execute
            .as_ref()
            .is_some_and(|(_, pending_profile, _)| pending_profile == &profile_id)
        {
            self.pending_execute = None;
        }
        if self.selected_profile.as_ref() == Some(&profile_id) {
            self.selected_profile = self.profiles.first().map(|profile| profile.id.clone());
            self.clear_workspace();
        }
        self.status = if server_state_unknown {
            "Profile deleted; server state is unknown.".to_owned()
        } else {
            "Profile deleted".to_owned()
        };
    }

    fn fold_profiles(&mut self, profiles: Vec<ProfileSnapshot>) {
        for (profile_id, generation) in self.active_generations.clone() {
            if profiles.iter().all(|profile| profile.id != profile_id) {
                self.tombstones
                    .entry(profile_id)
                    .and_modify(|current| {
                        if generation.0 > current.0 {
                            *current = generation;
                        }
                    })
                    .or_insert(generation);
            }
        }
        let profiles = profiles
            .into_iter()
            .filter(|profile| {
                self.tombstones
                    .get(&profile.id)
                    .is_none_or(|tombstone| profile.generation.0 > tombstone.0)
            })
            .collect::<Vec<_>>();
        let selected_changed = self.selected_profile.as_ref().is_some_and(|selected| {
            let previous = self.profiles.iter().find(|profile| profile.id == *selected);
            let refreshed = profiles.iter().find(|profile| profile.id == *selected);
            !matches!((previous, refreshed), (Some(previous), Some(refreshed)) if
                previous.generation == refreshed.generation
                    && previous.persisted == refreshed.persisted)
        });
        self.connection_states.retain(|profile_id, _| {
            let previous = self
                .profiles
                .iter()
                .find(|profile| profile.id == *profile_id);
            let refreshed = profiles.iter().find(|profile| profile.id == *profile_id);
            matches!((previous, refreshed), (Some(previous), Some(refreshed)) if
                (previous.generation == refreshed.generation
                    && previous.persisted == refreshed.persisted)
                || self.pending_retags.get(profile_id)
                    == Some(&(previous.generation, refreshed.generation)))
        });
        self.active_generations = profiles
            .iter()
            .filter(|profile| {
                self.tombstones
                    .get(&profile.id)
                    .is_none_or(|tombstone| profile.generation.0 > tombstone.0)
            })
            .map(|profile| (profile.id.clone(), profile.generation))
            .collect();
        if self
            .selected_profile
            .as_ref()
            .is_none_or(|selected| !profiles.iter().any(|profile| profile.id == *selected))
        {
            self.selected_profile = profiles.first().map(|profile| profile.id.clone());
        }
        self.profiles = profiles;
        for profile in &self.profiles {
            if profile.persisted.credential_mode == CredentialMode::Session
                && !profile.has_current_session_secret
            {
                self.connection_states
                    .insert(profile.id.clone(), ConnectionState::NeedsCredential);
            }
        }
        if selected_changed {
            self.clear_workspace();
        }
        self.pending_retags.clear();
        self.config_uncertain = false;
        self.status = format!("{} profiles loaded", self.profiles.len());
    }

    fn clear_workspace(&mut self) {
        self.editor_text.clear();
        self.pending_execute = None;
        self.result = None;
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{
        ConnectionFailureOutcome, ConnectionState, PostCloseState, ProfileSnapshot, UiEvent,
        UiModel,
    };
    use crate::model::{
        CatalogLevel, CatalogPage, CatalogRequest, CatalogRetainedCounts, ConnectionProfile,
        CredentialMode, DriverAvailability, DriverKind, OperationId, OperationKind,
        ProfileGeneration, ProfileId, PublicCode, PublicSummary, QueryResult, RedisKeyFilter,
        RedisKeyPage, RedisScanConsistency, RedisScanRequest, RedisTlsConfig, RequestIdentity,
        ResultId, ResultProvenance, ResultRetentionPolicy, ResultSnapshot, SessionGeneration,
        TlsMode,
    };
    use crate::public_error::{PublicOperationError, SafeContext};
    use crate::service::SessionDisposition;

    fn result(elapsed_ms: u128) -> ResultSnapshot {
        let raw = QueryResult {
            columns: Vec::new(),
            rows: Vec::new(),
            affected_rows: 0,
            last_insert_id: None,
            elapsed_ms,
            truncated: false,
            backend_notices_present: false,
        };
        ResultSnapshot::retain(
            raw,
            ResultProvenance {
                result_id: ResultId(1),
                profile_id: ProfileId("mysql-local".to_owned()),
                profile_generation: ProfileGeneration(1),
                operation_id: OperationId(1),
                driver: DriverKind::MySql,
                completed_at_unix_ms: 0,
                duration_ms: elapsed_ms,
            },
            ResultRetentionPolicy::mysql(500),
        )
    }

    fn identity(operation_id: u64) -> RequestIdentity {
        RequestIdentity {
            profile_id: ProfileId("mysql-local".to_owned()),
            profile_generation: ProfileGeneration(1),
            operation_id: OperationId(operation_id),
        }
    }

    #[test]
    fn resource_failures_preserve_last_pages_as_stale_and_success_clears_retry() {
        let profile_id = ProfileId("mysql-local".to_owned());
        let generation = ProfileGeneration(1);
        let catalog_request = CatalogRequest::Schemas {
            identity: identity(10),
            prefix: None,
            page_token: None,
            page_size: 100,
            timeout: Duration::from_secs(5),
        };
        let redis_request = RedisScanRequest {
            identity: identity(11),
            filter: RedisKeyFilter::LiteralPrefix(String::new()),
            cursor: 0,
            count_hint: 100,
            timeout: Duration::from_secs(5),
        };
        let catalog_page = CatalogPage {
            identity: identity(10),
            level: CatalogLevel::Schemas,
            parent: None,
            nodes: Vec::new(),
            next_token: None,
            retained_counts: CatalogRetainedCounts::default(),
            retained_utf8_bytes: 0,
            truncated: false,
            stale: false,
            loaded_at: "2026-07-15T00:00:00Z".to_owned(),
        };
        let redis_page = RedisKeyPage {
            identity: identity(11),
            next_cursor: 0,
            keys: Vec::new(),
            retained_count: 0,
            skipped_oversize: 0,
            retained_bytes: 0,
            consistency: RedisScanConsistency::Weak,
            truncated: false,
            stale: false,
        };
        let mut model = UiModel {
            active_generations: [(profile_id.clone(), generation)].into(),
            catalog_pages: [(profile_id.clone(), catalog_page.clone())].into(),
            redis_key_pages: [(profile_id.clone(), redis_page.clone())].into(),
            ..UiModel::default()
        };

        model.fold(UiEvent::CatalogPageFailed {
            request: catalog_request.clone(),
            summary: PublicSummary::PermissionDenied,
            session_generation: Some(SessionGeneration(31)),
            session_disposition: Some(SessionDisposition::Keep),
        });
        model.fold(UiEvent::RedisKeysFailed {
            request: redis_request.clone(),
            error: PublicOperationError::new_or_internal(
                OperationKind::BrowseRedis,
                PublicSummary::UnsupportedFeature,
                PublicCode::None,
                &SafeContext::profile(profile_id.clone(), OperationId(11)),
            ),
            session_generation: None,
            session_disposition: None,
            connection_outcome: ConnectionFailureOutcome::Preserve,
        });

        assert!(model.catalog_pages[&profile_id].stale);
        assert_eq!(model.catalog_retry[&profile_id], catalog_request);
        assert!(model.redis_key_pages[&profile_id].stale);
        assert_eq!(model.redis_scan_retry[&profile_id], redis_request);

        model.fold(UiEvent::CatalogPageLoaded {
            page: catalog_page,
            session_generation: SessionGeneration(31),
            session_disposition: SessionDisposition::Keep,
        });
        model.fold(UiEvent::RedisKeysLoaded {
            page: redis_page,
            session_generation: SessionGeneration(22),
            session_disposition: SessionDisposition::Keep,
        });

        assert!(!model.catalog_pages[&profile_id].stale);
        assert!(!model.catalog_retry.contains_key(&profile_id));
        assert!(!model.redis_key_pages[&profile_id].stale);
        assert!(!model.redis_scan_retry.contains_key(&profile_id));
        assert_eq!(
            model.connection_state(&profile_id),
            &ConnectionState::Connected {
                session_generation: SessionGeneration(22),
                elapsed_ms: 0,
            }
        );
    }

    #[test]
    fn redis_failure_fold_uses_exact_session_disposition_and_retains_typed_code() {
        let profile_id = ProfileId("redis-session-truth".to_owned());
        let generation = ProfileGeneration(8);
        let request = RedisScanRequest {
            identity: RequestIdentity::new(profile_id.clone(), generation, OperationId(31)),
            filter: RedisKeyFilter::LiteralPrefix(String::new()),
            cursor: 0,
            count_hint: 100,
            timeout: Duration::from_secs(5),
        };
        let mut model = UiModel {
            active_generations: [(profile_id.clone(), generation)].into(),
            ..UiModel::default()
        };

        model.fold(UiEvent::RedisKeysFailed {
            request: request.clone(),
            error: PublicOperationError::new_or_internal(
                OperationKind::BrowseRedis,
                PublicSummary::ResourceStale,
                PublicCode::None,
                &SafeContext::profile(profile_id.clone(), OperationId(31)),
            ),
            session_generation: Some(SessionGeneration(41)),
            session_disposition: Some(SessionDisposition::Keep),
            connection_outcome: ConnectionFailureOutcome::Preserve,
        });
        assert_eq!(
            model.connection_state(&profile_id),
            &ConnectionState::Connected {
                session_generation: SessionGeneration(41),
                elapsed_ms: 0,
            }
        );

        model.fold(UiEvent::RedisKeysFailed {
            request,
            error: PublicOperationError::new_or_internal(
                OperationKind::BrowseRedis,
                PublicSummary::TlsVerificationFailed,
                PublicCode::TlsHostnameMismatch,
                &SafeContext::profile(profile_id.clone(), OperationId(31)),
            ),
            session_generation: Some(SessionGeneration(41)),
            session_disposition: Some(SessionDisposition::Evict),
            connection_outcome: ConnectionFailureOutcome::Disconnected,
        });
        assert_eq!(
            model.redis_scan_errors[&profile_id].code,
            PublicCode::TlsHostnameMismatch
        );
        assert_eq!(
            model.connection_state(&profile_id),
            &ConnectionState::Disconnected
        );
    }

    #[test]
    fn stale_query_event_does_not_overwrite_result() {
        let profile_id = ProfileId("mysql-local".to_owned());
        let generation = ProfileGeneration(1);
        let mut model = UiModel {
            active_generations: [(profile_id.clone(), generation)].into(),
            pending_execute: Some((OperationId(2), profile_id.clone(), generation)),
            result: Some(result(7)),
            ..UiModel::default()
        };

        model.fold(UiEvent::QueryFinished {
            operation_id: OperationId(1),
            profile_id,
            profile_generation: generation,
            session_generation: SessionGeneration(1),
            result: result(99),
        });

        assert_eq!(
            model.pending_execute.map(|pending| pending.0),
            Some(OperationId(2))
        );
        assert_eq!(
            model
                .result
                .as_ref()
                .map(|value| value.provenance.duration_ms),
            Some(7)
        );
    }

    #[test]
    fn predecessor_connection_closed_cannot_replace_newer_pending_or_connected_state() {
        let profile_id = ProfileId("mysql-local".to_owned());
        let generation = ProfileGeneration(1);
        let reconnect = OperationId(12);
        let predecessor = OperationId(11);
        let mut model = UiModel {
            active_generations: [(profile_id.clone(), generation)].into(),
            connection_states: [(profile_id.clone(), ConnectionState::Pending(reconnect))].into(),
            ..UiModel::default()
        };

        model.fold(UiEvent::ConnectionClosed {
            operation_id: predecessor,
            profile_id: profile_id.clone(),
            profile_generation: generation,
            post_close: PostCloseState::Disconnected,
        });
        assert_eq!(
            model.connection_state(&profile_id),
            &ConnectionState::Pending(reconnect),
            "a predecessor close must not replace the newer pending reconnect"
        );

        model.fold(UiEvent::ConnectionReady {
            operation_id: reconnect,
            profile_id: profile_id.clone(),
            profile_generation: generation,
            session_generation: SessionGeneration(4),
            elapsed_ms: 7,
        });
        let connected = ConnectionState::Connected {
            session_generation: SessionGeneration(4),
            elapsed_ms: 7,
        };
        assert_eq!(model.connection_state(&profile_id), &connected);

        model.fold(UiEvent::ConnectionClosed {
            operation_id: predecessor,
            profile_id: profile_id.clone(),
            profile_generation: generation,
            post_close: PostCloseState::NeedsCredential,
        });
        assert_eq!(
            model.connection_state(&profile_id),
            &connected,
            "a predecessor close arriving after ready must not replace connected state"
        );
    }

    #[test]
    fn non_connect_failure_outcome_cannot_replace_another_pending_connection() {
        let profile_id = ProfileId("mysql-local".to_owned());
        let generation = ProfileGeneration(1);
        let reconnect = OperationId(22);
        let predecessor = OperationId(21);

        for outcome in [
            ConnectionFailureOutcome::Unknown,
            ConnectionFailureOutcome::Disconnected,
            ConnectionFailureOutcome::NeedsCredential,
        ] {
            let mut disconnect_model = UiModel {
                active_generations: [(profile_id.clone(), generation)].into(),
                connection_states: [(profile_id.clone(), ConnectionState::Pending(reconnect))]
                    .into(),
                status: "Reconnecting…".to_owned(),
                ..UiModel::default()
            };
            disconnect_model.fold(UiEvent::OperationFailed {
                operation_id: predecessor,
                profile_id: profile_id.clone(),
                profile_generation: generation,
                session_generation: None,
                kind: OperationKind::DisconnectProfile,
                summary: PublicSummary::OperationCancelled,
                session_disposition: None,
                connection_outcome: outcome,
            });
            assert_eq!(
                disconnect_model.connection_state(&profile_id),
                &ConnectionState::Pending(reconnect),
                "a predecessor disconnect outcome must not replace a newer pending reconnect"
            );
            assert_eq!(disconnect_model.status, "Reconnecting…");

            let execute = OperationId(23);
            let mut execute_model = UiModel {
                active_generations: [(profile_id.clone(), generation)].into(),
                connection_states: [(profile_id.clone(), ConnectionState::Pending(reconnect))]
                    .into(),
                pending_execute: Some((execute, profile_id.clone(), generation)),
                status: "Executing…".to_owned(),
                ..UiModel::default()
            };
            execute_model.fold(UiEvent::OperationFailed {
                operation_id: execute,
                profile_id: profile_id.clone(),
                profile_generation: generation,
                session_generation: None,
                kind: OperationKind::ExecuteRead,
                summary: PublicSummary::OperationCancelled,
                session_disposition: None,
                connection_outcome: outcome,
            });
            assert_eq!(
                execute_model.connection_state(&profile_id),
                &ConnectionState::Pending(reconnect),
                "a correlated execute terminal may clear execute state but not another operation's pending connection"
            );
            assert!(execute_model.pending_execute.is_none());
            assert_eq!(
                execute_model.status,
                PublicSummary::OperationCancelled.message()
            );

            let mut matching = UiModel {
                active_generations: [(profile_id.clone(), generation)].into(),
                connection_states: [(profile_id.clone(), ConnectionState::Pending(reconnect))]
                    .into(),
                ..UiModel::default()
            };
            matching.fold(UiEvent::OperationFailed {
                operation_id: reconnect,
                profile_id: profile_id.clone(),
                profile_generation: generation,
                session_generation: None,
                kind: OperationKind::DisconnectProfile,
                summary: PublicSummary::OperationCancelled,
                session_disposition: None,
                connection_outcome: outcome,
            });
            let expected = match outcome {
                ConnectionFailureOutcome::Unknown | ConnectionFailureOutcome::Disconnected => {
                    ConnectionState::Disconnected
                }
                ConnectionFailureOutcome::NeedsCredential => ConnectionState::NeedsCredential,
                ConnectionFailureOutcome::Preserve => unreachable!("fixture excludes Preserve"),
            };
            assert_eq!(
                matching.connection_state(&profile_id),
                &expected,
                "the matching operation outcome may update its own pending state"
            );
        }
    }

    #[test]
    fn refreshed_changed_profile_clears_stale_connection_state() {
        let profile_id = ProfileId("mysql-local".to_owned());
        let original = profile(3306);
        let mut model = UiModel {
            profiles: vec![original],
            connection_states: [(
                profile_id.clone(),
                ConnectionState::Connected {
                    session_generation: SessionGeneration(1),
                    elapsed_ms: 5,
                },
            )]
            .into(),
            ..UiModel::default()
        };

        model.fold(UiEvent::ProfilesLoaded {
            operation_id: OperationId(1),
            profiles: vec![profile(3307)],
        });

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
