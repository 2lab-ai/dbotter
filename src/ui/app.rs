//! Native three-zone UI. Rendering and state folding perform no I/O.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use eframe::egui;
use egui_extras::{Column as TableColumn, TableBuilder};

use crate::config::MigrationConsent;
use crate::model::{
    CatalogRequest, Cell, DEFAULT_CATALOG_PAGE_SIZE, DEFAULT_CATALOG_TIMEOUT,
    DEFAULT_REDIS_SCAN_COUNT, DraftId, DriverAvailability, DriverCapabilities, DriverKind,
    OperationId, OperationKind, ProfileFieldId, ProfileGeneration, ProfileId, PublicSummary,
    RedisKeyInspectRequest, RedisScanRequest, RequestIdentity, SessionCredentialIntent,
};
use crate::service::DeleteProfileRequest;

use super::accessibility::{named_author_id, named_author_id_with_label};
use super::adapter::{SubmitError, UiCommand, UiPort};
use super::editor::{EditorIntent, EditorSurface};
use super::layout::NativeLayout;
use super::model::{ConnectionState, ProfileSnapshot, UiEvent, UiModel};
use super::mysql_explorer::{MySqlExplorerIntent, MySqlExplorerState};
use super::profile_form::{
    DraftTestAttempt, FormAction, ProfileEditor, ProfileEventResult, SaveAttempt,
};
use super::redis_explorer::{RedisExplorer, RedisExplorerIntent};
use super::theme::OpenAiTheme;

const EVENT_DRAIN_LIMIT: usize = 128;
pub const DEFAULT_EXECUTE_ROW_LIMIT: u32 = 500;
const DEFAULT_TIMEOUT_MS: u64 = 30_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ActiveOperation {
    operation_id: OperationId,
    profile_generation: ProfileGeneration,
    kind: OperationKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PendingDelete {
    operation_id: OperationId,
    profile_generation: ProfileGeneration,
    prior_active: Option<ActiveOperation>,
    prior_finished: bool,
}

const fn delete_failure_is_known_non_committed(summary: PublicSummary) -> bool {
    matches!(
        summary,
        PublicSummary::InvalidInput
            | PublicSummary::ResourceBusy
            | PublicSummary::ResourceStale
            | PublicSummary::ConfigWriteNotCommitted
    )
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DeleteConfirmation {
    profile_id: ProfileId,
    profile_generation: ProfileGeneration,
    profile_name: String,
    active_kind: Option<OperationKind>,
    migration_confirmed: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DeleteDialogAction {
    Cancel,
    Confirm,
}

pub struct DbotterApp {
    port: UiPort,
    model: UiModel,
    mysql_explorers: HashMap<(ProfileId, ProfileGeneration), MySqlExplorerState>,
    profile_editor: Option<ProfileEditor>,
    editor_surface: EditorSurface,
    redis_explorer: RedisExplorer,
    first_run_driver: DriverKind,
    active_operations: HashMap<ProfileId, ActiveOperation>,
    pending_deletes: HashMap<ProfileId, PendingDelete>,
    delete_confirmation: Option<DeleteConfirmation>,
    next_draft_id: u64,
    pending_connect_after_refresh: Option<(ProfileId, OperationId)>,
}

impl DbotterApp {
    pub fn new(port: UiPort) -> Self {
        let mut app = Self {
            port,
            model: UiModel::default(),
            mysql_explorers: HashMap::new(),
            profile_editor: None,
            editor_surface: EditorSurface::default(),
            redis_explorer: RedisExplorer::default(),
            first_run_driver: DriverKind::MySql,
            active_operations: HashMap::new(),
            pending_deletes: HashMap::new(),
            delete_confirmation: None,
            next_draft_id: 1,
            pending_connect_after_refresh: None,
        };
        let operation_id = app.model.next_operation();
        let _ = app
            .port
            .try_submit(UiCommand::RefreshProfiles { operation_id });
        app
    }

    fn allocate_draft_id(&mut self) -> DraftId {
        let draft_id = DraftId(self.next_draft_id);
        self.next_draft_id = self.next_draft_id.saturating_add(1);
        draft_id
    }

    fn poll_events(&mut self) {
        for event in self.port.drain_events(EVENT_DRAIN_LIMIT) {
            self.finish_active_operation(&event);
            self.fold_mysql_explorer_event(&event);
            self.redis_explorer.handle_event(&event);
            let profile_result = self
                .profile_editor
                .as_mut()
                .map_or(ProfileEventResult::Ignored, |editor| {
                    editor.handle_event(&event)
                });
            match profile_result {
                ProfileEventResult::Saved(profile_id, warning) => {
                    self.model.fold(event);
                    self.model.selected_profile = Some(profile_id);
                    self.model.status = warning.map_or_else(
                        || "Profile saved; refreshing profiles…".to_owned(),
                        |summary| summary.message().to_owned(),
                    );
                    self.profile_editor = None;
                    let operation_id = self.model.next_operation();
                    if let Err(error) = self
                        .port
                        .try_submit(UiCommand::RefreshProfiles { operation_id })
                    {
                        self.report_submit_error(error);
                    }
                    continue;
                }
                ProfileEventResult::SavedAndConnect(profile_id, warning) => {
                    self.model.fold(event);
                    self.model.selected_profile = Some(profile_id.clone());
                    self.model.status = warning.map_or_else(
                        || "Profile saved; refreshing before connect…".to_owned(),
                        |summary| summary.message().to_owned(),
                    );
                    self.profile_editor = None;
                    let operation_id = self.model.next_operation();
                    match self
                        .port
                        .try_submit(UiCommand::RefreshProfiles { operation_id })
                    {
                        Ok(()) => {
                            self.pending_connect_after_refresh = Some((profile_id, operation_id));
                        }
                        Err(error) => {
                            self.pending_connect_after_refresh = None;
                            self.report_submit_error(error);
                        }
                    }
                    continue;
                }
                ProfileEventResult::Failed => {
                    if let Some(editor) = &self.profile_editor {
                        self.model.status = editor.status().to_owned();
                    }
                    continue;
                }
                ProfileEventResult::Ignored => {}
            }
            let connect_follow_up = self.pending_connect_after_refresh.as_ref().and_then(
                |(profile_id, expected_operation)| match &event {
                    UiEvent::ProfilesLoaded { operation_id, .. }
                        if operation_id == expected_operation =>
                    {
                        Some((
                            profile_id.clone(),
                            true,
                            self.model.profiles_operation_is_newer(*operation_id),
                        ))
                    }
                    UiEvent::ProfilesFailed { operation_id, .. }
                        if operation_id == expected_operation =>
                    {
                        Some((profile_id.clone(), false, false))
                    }
                    _ => None,
                },
            );
            self.model.fold(event);
            if let Some((profile_id, loaded, accepted)) = connect_follow_up {
                self.pending_connect_after_refresh = None;
                if loaded && accepted && self.model.active_generation(&profile_id).is_some() {
                    self.model.selected_profile = Some(profile_id.clone());
                    self.submit_test(profile_id);
                }
            }
        }
        self.mysql_explorers.retain(|(profile_id, generation), _| {
            self.model.active_generation(profile_id) == Some(*generation)
        });
        self.prune_active_operations();
        if let Some(editor) = self.profile_editor.as_mut() {
            editor.set_config_uncertain(self.model.is_config_uncertain());
        }
    }

    fn finish_active_operation(&mut self, event: &UiEvent) {
        if matches!(
            event,
            UiEvent::ConfigUncertain { .. } | UiEvent::RuntimeShutdown { .. }
        ) {
            self.active_operations.clear();
            self.pending_deletes.clear();
            return;
        }
        let terminal = match event {
            UiEvent::ConnectionReady {
                operation_id,
                profile_id,
                ..
            }
            | UiEvent::ConnectionClosed {
                operation_id,
                profile_id,
                ..
            }
            | UiEvent::QueryFinished {
                operation_id,
                profile_id,
                ..
            }
            | UiEvent::ExecuteUnavailable {
                operation_id,
                profile_id,
                ..
            }
            | UiEvent::OperationFailed {
                operation_id,
                profile_id,
                ..
            }
            | UiEvent::ProfileDeleted {
                operation_id,
                profile_id,
                ..
            } => Some((profile_id, *operation_id)),
            UiEvent::CatalogPageLoaded { page, .. } => {
                Some((&page.identity.profile_id, page.identity.operation_id))
            }
            UiEvent::CatalogPageFailed { request, .. } => {
                Some((request.profile_id(), request.operation_id()))
            }
            UiEvent::RedisKeysLoaded { page, .. } => {
                Some((&page.identity.profile_id, page.identity.operation_id))
            }
            UiEvent::RedisKeysFailed { request, .. } => {
                Some((request.profile_id(), request.operation_id()))
            }
            UiEvent::RedisKeyInspected { preview, .. } => {
                Some((&preview.identity.profile_id, preview.identity.operation_id))
            }
            UiEvent::RedisKeyInspectFailed { request, .. } => {
                Some((request.profile_id(), request.operation_id()))
            }
            _ => None,
        };
        if let Some((profile_id, operation_id)) = terminal {
            if let Some(pending) = self.pending_deletes.get_mut(profile_id)
                && pending
                    .prior_active
                    .is_some_and(|prior| prior.operation_id == operation_id)
            {
                pending.prior_finished = true;
            }
            if let UiEvent::OperationFailed {
                kind: OperationKind::DeleteProfile,
                summary,
                ..
            } = event
                && let Some(pending) = self.pending_deletes.get(profile_id).copied()
                && pending.operation_id == operation_id
            {
                self.pending_deletes.remove(profile_id);
                if self
                    .active_operations
                    .get(profile_id)
                    .is_some_and(|active| active.operation_id == operation_id)
                {
                    self.active_operations.remove(profile_id);
                }
                if delete_failure_is_known_non_committed(*summary)
                    && !pending.prior_finished
                    && self.model.active_generation(profile_id) == Some(pending.profile_generation)
                    && let Some(prior) = pending.prior_active
                {
                    self.active_operations.insert(profile_id.clone(), prior);
                }
                return;
            }
            if matches!(event, UiEvent::ProfileDeleted { .. }) {
                self.pending_deletes.remove(profile_id);
            }
            if self
                .active_operations
                .get(profile_id)
                .is_some_and(|active| active.operation_id == operation_id)
            {
                self.active_operations.remove(profile_id);
            }
        }
    }

    fn prune_active_operations(&mut self) {
        self.active_operations.retain(|profile_id, active| {
            self.model.active_generation(profile_id) == Some(active.profile_generation)
        });
        self.pending_deletes.retain(|profile_id, pending| {
            self.model.active_generation(profile_id) == Some(pending.profile_generation)
        });
    }

    fn fold_mysql_explorer_event(&mut self, event: &UiEvent) {
        match event {
            UiEvent::CatalogPageLoaded { page, .. } => {
                let key = (
                    page.identity.profile_id.clone(),
                    page.identity.profile_generation,
                );
                self.mysql_explorers
                    .entry(key)
                    .or_default()
                    .handle_loaded(page.clone());
            }
            UiEvent::CatalogPageFailed { request, error, .. } => {
                let key = (request.profile_id().clone(), request.profile_generation());
                self.mysql_explorers
                    .entry(key)
                    .or_default()
                    .handle_failed(request.clone(), error.summary);
            }
            UiEvent::ProfileDeleted {
                profile_id,
                profile_generation,
                ..
            } => {
                self.mysql_explorers
                    .remove(&(profile_id.clone(), *profile_generation));
            }
            _ => {}
        }
    }

    fn submit_refresh(&mut self) {
        let operation_id = self.model.next_operation();
        match self
            .port
            .try_submit(UiCommand::RefreshProfiles { operation_id })
        {
            Ok(()) => self.model.status = "Reloading profiles…".to_owned(),
            Err(error) => self.report_submit_error(error),
        }
    }

    fn submit_test(&mut self, profile_id: ProfileId) {
        if self.model.is_config_uncertain() {
            self.model.status = "Reload profiles before using connections.".to_owned();
            return;
        }
        let Some(profile) = self
            .model
            .profiles
            .iter()
            .find(|profile| profile.id == profile_id)
            .cloned()
        else {
            self.model.status = "Unknown profile".to_owned();
            return;
        };
        if !profile.is_ready() {
            self.model.status = "Driver is planned and unavailable".to_owned();
            return;
        }
        if self.model.active_generation(&profile_id) != Some(profile.generation) {
            self.model.status = "The selected profile generation is stale.".to_owned();
            return;
        }
        if profile.persisted.credential_mode == crate::model::CredentialMode::Session
            && (!profile.has_current_session_secret
                || matches!(
                    self.model.connection_state(&profile_id),
                    ConnectionState::NeedsCredential
                ))
        {
            self.open_session_credential_prompt(profile_id);
            return;
        }
        if self.model.connection_state(&profile_id).is_pending() {
            self.model.status = "Connection work is already pending".to_owned();
            return;
        }
        if self.active_operations.contains_key(&profile_id) {
            self.model.status = "Another operation is active for this connection".to_owned();
            return;
        }
        let profile_generation = profile.generation;
        let operation_id = self.model.next_operation();
        match self.port.try_submit(UiCommand::TestConnection {
            operation_id,
            profile_id: profile_id.clone(),
            profile_generation,
            timeout_ms: DEFAULT_TIMEOUT_MS,
        }) {
            Ok(()) => {
                self.model
                    .connection_states
                    .insert(profile_id.clone(), ConnectionState::Pending(operation_id));
                self.active_operations.insert(
                    profile_id,
                    ActiveOperation {
                        operation_id,
                        profile_generation,
                        kind: OperationKind::ConnectProfile,
                    },
                );
                self.model.status = "Connecting…".to_owned();
            }
            Err(error) => self.report_submit_error(error),
        }
    }

    fn open_session_credential_prompt(&mut self, profile_id: ProfileId) {
        let Some(profile) = self
            .model
            .profiles
            .iter()
            .find(|profile| profile.id == profile_id)
            .cloned()
        else {
            self.model.status = "Unknown profile".to_owned();
            return;
        };
        if self.model.active_generation(&profile_id) != Some(profile.generation)
            || profile.persisted.credential_mode != crate::model::CredentialMode::Session
        {
            self.model.status =
                "The current profile cannot accept a session credential.".to_owned();
            return;
        }
        let draft_id = self.allocate_draft_id();
        let mut editor = ProfileEditor::edit(
            draft_id,
            &profile.persisted,
            profile.generation,
            profile.has_current_session_secret,
        );
        editor.select_session_intent(SessionCredentialIntent::Replace);
        editor.request_focus(ProfileFieldId::SessionCredential);
        self.profile_editor = Some(editor);
        self.model.status = "Enter the session credential, then Save & Connect.".to_owned();
    }

    fn submit_disconnect(&mut self, profile_id: ProfileId) {
        if self.model.is_config_uncertain() {
            self.model.status = "Reload profiles before using connections.".to_owned();
            return;
        }
        let Some(profile_generation) = self.model.active_generation(&profile_id) else {
            self.model.status = "Unknown profile".to_owned();
            return;
        };
        let operation_id = self.model.next_operation();
        match self.port.try_submit(UiCommand::DisconnectProfile {
            operation_id,
            profile_id: profile_id.clone(),
            profile_generation,
        }) {
            Ok(()) => {
                self.model
                    .connection_states
                    .insert(profile_id.clone(), ConnectionState::Pending(operation_id));
                self.active_operations.insert(
                    profile_id,
                    ActiveOperation {
                        operation_id,
                        profile_generation,
                        kind: OperationKind::DisconnectProfile,
                    },
                );
                self.model.status = "Disconnecting…".to_owned();
            }
            Err(error) => self.report_submit_error(error),
        }
    }

    fn submit_reconnect(&mut self, profile_id: ProfileId) {
        if self.model.is_config_uncertain() {
            self.model.status = "Reload profiles before using connections.".to_owned();
            return;
        }
        let Some(profile_generation) = self.model.active_generation(&profile_id) else {
            self.model.status = "Unknown profile".to_owned();
            return;
        };
        let operation_id = self.model.next_operation();
        match self.port.try_submit(UiCommand::ReconnectProfile {
            operation_id,
            profile_id: profile_id.clone(),
            profile_generation,
            timeout_ms: DEFAULT_TIMEOUT_MS,
        }) {
            Ok(()) => {
                self.model
                    .connection_states
                    .insert(profile_id.clone(), ConnectionState::Pending(operation_id));
                self.active_operations.insert(
                    profile_id,
                    ActiveOperation {
                        operation_id,
                        profile_generation,
                        kind: OperationKind::ReconnectProfile,
                    },
                );
                self.model.status = "Reconnecting…".to_owned();
            }
            Err(error) => self.report_submit_error(error),
        }
    }

    fn submit_editor_intent(&mut self, intent: EditorIntent) {
        if self.model.is_config_uncertain() {
            self.model.status = "Reload profiles before executing.".to_owned();
            return;
        }
        match intent {
            EditorIntent::Execute(intent) => {
                let profile_id = intent.profile_id().clone();
                let profile_generation = intent.profile_generation();
                let operation_kind = intent.operation_kind();
                let workspace_key =
                    super::model::WorkspaceKey::new(profile_id.clone(), profile_generation);
                if self.model.active_generation(intent.profile_id())
                    != Some(intent.profile_generation())
                {
                    self.model.status = "The selected profile generation is stale.".to_owned();
                    return;
                }
                if self
                    .model
                    .workspace(&workspace_key)
                    .is_some_and(|workspace| workspace.pending_execute.is_some())
                {
                    self.model.status = "Execute is already pending".to_owned();
                    return;
                }
                if self.active_operations.contains_key(&profile_id) {
                    self.model.status =
                        "Another operation is active for this connection".to_owned();
                    return;
                }
                let operation_id = self.model.next_operation();
                match self.port.try_submit(intent.into_ui_command(operation_id)) {
                    Ok(()) => {
                        self.model.workspace_mut(workspace_key).pending_execute =
                            Some(operation_id);
                        self.active_operations.insert(
                            profile_id,
                            ActiveOperation {
                                operation_id,
                                profile_generation,
                                kind: operation_kind,
                            },
                        );
                        self.model.status = "Executing…".to_owned();
                    }
                    Err(error) => self.report_submit_error(error),
                }
            }
            EditorIntent::Cancel { operation_id } => {
                if !self
                    .model
                    .selected_workspace()
                    .is_some_and(|workspace| workspace.pending_execute == Some(operation_id))
                {
                    self.model.status = "The pending execution is no longer current.".to_owned();
                    return;
                }
                match self
                    .port
                    .try_submit(UiCommand::CancelOperation { operation_id })
                {
                    Ok(()) => {
                        self.model.status = format!("Cancelling operation {}…", operation_id.0);
                    }
                    Err(error) => self.report_submit_error(error),
                }
            }
        }
    }

    fn report_submit_error(&mut self, error: SubmitError) {
        self.model.status = match error {
            SubmitError::Busy => "Service is busy; command was not submitted".to_owned(),
            SubmitError::Disconnected => "Service is unavailable".to_owned(),
        };
    }

    fn submit_mysql_explorer_intent(
        &mut self,
        profile: &ProfileSnapshot,
        intent: MySqlExplorerIntent,
    ) {
        if let MySqlExplorerIntent::InsertTemplate(template) = intent {
            let key = super::model::WorkspaceKey::new(profile.id.clone(), profile.generation);
            self.model.workspace_mut(key).editor_text = template;
            self.model.status = "Bounded SELECT template inserted; it was not executed".to_owned();
            return;
        }
        if self.model.is_config_uncertain() {
            self.model.status = "Reload profiles before browsing the catalog.".to_owned();
            return;
        }
        if self.active_operations.contains_key(&profile.id) {
            self.model.status = "Another operation is active for this connection".to_owned();
            return;
        }
        let operation_id = self.model.next_operation();
        let identity = RequestIdentity::new(profile.id.clone(), profile.generation, operation_id);
        let request = match intent {
            MySqlExplorerIntent::RefreshSchemas { prefix } => CatalogRequest::Schemas {
                identity,
                prefix,
                page_token: None,
                page_size: DEFAULT_CATALOG_PAGE_SIZE,
                timeout: DEFAULT_CATALOG_TIMEOUT,
            },
            MySqlExplorerIntent::LoadMore(request) => {
                catalog_request_with_identity(request, identity)
            }
            MySqlExplorerIntent::LoadRelations {
                schema,
                prefix,
                token,
            } => CatalogRequest::Relations {
                identity,
                schema,
                prefix,
                page_token: token,
                page_size: DEFAULT_CATALOG_PAGE_SIZE,
                timeout: DEFAULT_CATALOG_TIMEOUT,
            },
            MySqlExplorerIntent::LoadColumns {
                schema,
                relation,
                prefix,
                token,
            } => CatalogRequest::Columns {
                identity,
                schema,
                relation,
                prefix,
                page_token: token,
                page_size: DEFAULT_CATALOG_PAGE_SIZE,
                timeout: DEFAULT_CATALOG_TIMEOUT,
            },
            MySqlExplorerIntent::Retry(request) => catalog_request_with_identity(request, identity),
            MySqlExplorerIntent::InsertTemplate(_) => return,
        };
        match self
            .port
            .try_submit(UiCommand::BrowseCatalog(request.clone()))
        {
            Ok(()) => {
                self.mysql_explorers
                    .entry((profile.id.clone(), profile.generation))
                    .or_default()
                    .mark_submitted(request);
                self.active_operations.insert(
                    profile.id.clone(),
                    ActiveOperation {
                        operation_id,
                        profile_generation: profile.generation,
                        kind: OperationKind::BrowseMySql,
                    },
                );
                self.model.status = "Loading MySQL catalog page…".to_owned();
            }
            Err(error) => self.report_submit_error(error),
        }
    }

    fn submit_redis_intent(&mut self, intent: RedisExplorerIntent) {
        if let RedisExplorerIntent::EditProfileField { profile_id, field } = &intent {
            self.open_profile_editor_at(profile_id, *field);
            return;
        }
        if let RedisExplorerIntent::Cancel { operation_id } = &intent {
            let operation_id = *operation_id;
            match self
                .port
                .try_submit(UiCommand::CancelOperation { operation_id })
            {
                Ok(()) => {
                    self.redis_explorer.cancel_submitted(operation_id);
                    self.model.status = "Cancelling Redis operation…".to_owned();
                }
                Err(error) => {
                    self.redis_explorer
                        .submission_failed(submit_error_message(error));
                    self.report_submit_error(error);
                }
            }
            return;
        }
        if self.model.is_config_uncertain() {
            self.redis_explorer
                .submission_failed("Reload profiles before browsing Redis.");
            return;
        }
        let Some(profile) = self.model.selected_profile_snapshot().cloned() else {
            self.redis_explorer
                .submission_failed("Select a Redis profile.");
            return;
        };
        let keyspace_ready = profile.driver == DriverKind::Redis
            && profile.is_ready()
            && crate::drivers::descriptors()
                .into_iter()
                .find(|descriptor| descriptor.kind == DriverKind::Redis)
                .is_some_and(|descriptor| {
                    descriptor
                        .capabilities
                        .contains(DriverCapabilities::KEYSPACE_BROWSE)
                });
        if !keyspace_ready {
            self.redis_explorer
                .submission_failed("Redis keyspace browsing is unavailable.");
            return;
        }
        if self.active_operations.contains_key(&profile.id) {
            self.redis_explorer
                .submission_failed("Another operation is active for this connection.");
            return;
        }
        let operation_id = self.model.next_operation();
        let identity = RequestIdentity::new(profile.id.clone(), profile.generation, operation_id);
        match intent {
            RedisExplorerIntent::Scan {
                filter,
                cursor,
                restart,
            } => {
                let request = RedisScanRequest {
                    identity,
                    filter: filter.clone(),
                    cursor,
                    count_hint: DEFAULT_REDIS_SCAN_COUNT,
                    timeout: Duration::from_secs(5),
                };
                match self.port.try_submit(UiCommand::ScanRedisKeys(request)) {
                    Ok(()) => {
                        self.redis_explorer
                            .begin_scan(operation_id, filter, cursor, restart);
                        self.active_operations.insert(
                            profile.id.clone(),
                            ActiveOperation {
                                operation_id,
                                profile_generation: profile.generation,
                                kind: OperationKind::BrowseRedis,
                            },
                        );
                        self.model.status = "Scanning Redis keys…".to_owned();
                    }
                    Err(error) => {
                        self.redis_explorer
                            .submission_failed(submit_error_message(error));
                        self.report_submit_error(error);
                    }
                }
            }
            RedisExplorerIntent::Inspect { key } => {
                let request = RedisKeyInspectRequest {
                    identity,
                    key: key.clone(),
                    timeout: Duration::from_secs(5),
                };
                match self.port.try_submit(UiCommand::InspectRedisKey(request)) {
                    Ok(()) => {
                        self.redis_explorer.begin_inspect(operation_id, key);
                        self.active_operations.insert(
                            profile.id.clone(),
                            ActiveOperation {
                                operation_id,
                                profile_generation: profile.generation,
                                kind: OperationKind::InspectRedis,
                            },
                        );
                        self.model.status = "Inspecting Redis key…".to_owned();
                    }
                    Err(error) => {
                        self.redis_explorer
                            .submission_failed(submit_error_message(error));
                        self.report_submit_error(error);
                    }
                }
            }
            RedisExplorerIntent::Cancel { .. } => unreachable!("handled above"),
            RedisExplorerIntent::EditProfileField { .. } => unreachable!("handled above"),
        }
    }

    fn open_profile_editor_at(&mut self, profile_id: &ProfileId, field: ProfileFieldId) {
        let Some(profile) = self
            .model
            .profiles
            .iter()
            .find(|profile| &profile.id == profile_id)
            .cloned()
        else {
            self.model.status = "The Redis profile is no longer available.".to_owned();
            return;
        };
        let draft_id = self.allocate_draft_id();
        let mut editor = ProfileEditor::edit(
            draft_id,
            &profile.persisted,
            profile.generation,
            profile.has_current_session_secret,
        );
        editor.request_focus(field);
        self.profile_editor = Some(editor);
    }

    fn open_delete_confirmation(&mut self, profile: &ProfileSnapshot) {
        if self.model.is_config_uncertain() {
            self.model.status = "Reload profiles before deleting.".to_owned();
            return;
        }
        if self.model.active_generation(&profile.id) != Some(profile.generation) {
            self.model.status = "The selected profile generation is stale.".to_owned();
            return;
        }
        self.delete_confirmation = Some(DeleteConfirmation {
            profile_id: profile.id.clone(),
            profile_generation: profile.generation,
            profile_name: profile.name.clone(),
            active_kind: self
                .active_operations
                .get(&profile.id)
                .map(|active| active.kind),
            migration_confirmed: false,
        });
    }

    fn cancel_delete_confirmation(&mut self) {
        self.delete_confirmation = None;
    }

    fn confirm_delete_confirmation(&mut self) {
        let Some(confirmation) = self.delete_confirmation.as_ref() else {
            return;
        };
        if self.model.active_generation(&confirmation.profile_id)
            != Some(confirmation.profile_generation)
        {
            self.delete_confirmation = None;
            self.model.status = "The selected profile generation is stale.".to_owned();
            return;
        }
        let profile_id = confirmation.profile_id.clone();
        let profile_generation = confirmation.profile_generation;
        let migration_consent =
            MigrationConsent::from_confirmation(confirmation.migration_confirmed);
        let operation_id = self.model.next_operation();
        let request = DeleteProfileRequest {
            profile_id: profile_id.clone(),
            expected_generation: profile_generation,
            operation_id,
            migration_consent,
        };
        match self.port.try_submit(UiCommand::DeleteProfile(request)) {
            Ok(()) => {
                self.delete_confirmation = None;
                self.model
                    .connection_states
                    .insert(profile_id.clone(), ConnectionState::Closing);
                let prior_active = self.active_operations.insert(
                    profile_id.clone(),
                    ActiveOperation {
                        operation_id,
                        profile_generation,
                        kind: OperationKind::DeleteProfile,
                    },
                );
                self.pending_deletes.insert(
                    profile_id,
                    PendingDelete {
                        operation_id,
                        profile_generation,
                        prior_active,
                        prior_finished: false,
                    },
                );
                self.model.status = "Deleting profile…".to_owned();
            }
            Err(error) => self.report_submit_error(error),
        }
    }

    fn show_delete_confirmation(&mut self, root_ui: &mut egui::Ui) {
        let Some(confirmation) = self.delete_confirmation.as_mut() else {
            return;
        };
        let mut action = None;
        egui::Window::new("Delete connection")
            .collapsible(false)
            .resizable(false)
            .show(root_ui.ctx(), |ui| {
                ui.heading(format!("Delete {}?", confirmation.profile_name));
                ui.label("This removes the saved profile and its in-memory session credential.");
                if let Some(kind) = confirmation.active_kind {
                    let warning = format!(
                        "{kind:?} is active. Dbotter will stop waiting; the server operation may continue."
                    );
                    named_author_id_with_label(
                        ui.strong(warning.clone()),
                        "profile.delete.active_warning",
                        warning,
                    );
                    ui.strong("After confirmed deletion, server state will be reported as Unknown.");
                }
                let migration = ui.checkbox(
                    &mut confirmation.migration_confirmed,
                    "Allow a version-1 configuration migration with backup if required",
                );
                named_author_id(
                    migration,
                    "profile.delete.migration_confirm",
                    "Confirm delete configuration migration backup",
                );
                ui.horizontal(|ui| {
                    let cancel = ui.add_sized(
                        [104.0, OpenAiTheme::MIN_CONTROL_HEIGHT],
                        egui::Button::new("Cancel"),
                    );
                    if named_author_id(
                        cancel,
                        "profile.delete.cancel",
                        "Cancel profile deletion",
                    )
                    .clicked()
                    {
                        action = Some(DeleteDialogAction::Cancel);
                    }
                    let confirm = ui.add(
                        egui::Button::new(
                            egui::RichText::new("Delete profile").color(egui::Color32::WHITE),
                        )
                        .fill(egui::Color32::BLACK)
                        .min_size(egui::vec2(144.0, OpenAiTheme::MIN_CONTROL_HEIGHT)),
                    );
                    if named_author_id(
                        confirm,
                        "profile.delete.confirm",
                        "Confirm profile deletion",
                    )
                    .clicked()
                    {
                        action = Some(DeleteDialogAction::Confirm);
                    }
                });
            });
        match action {
            Some(DeleteDialogAction::Cancel) => self.cancel_delete_confirmation(),
            Some(DeleteDialogAction::Confirm) => self.confirm_delete_confirmation(),
            None => {}
        }
    }

    fn show_native(&mut self, ui: &mut egui::Ui) {
        OpenAiTheme::apply(ui.ctx());
        if self.model.profile_load_succeeded()
            && self.model.profiles.is_empty()
            && self.profile_editor.is_none()
        {
            egui::CentralPanel::default().show(ui, |ui| self.show_first_run(ui));
            return;
        }

        if NativeLayout::columns_for_width(ui.available_width()) == 3 {
            self.connections(ui);
            self.explorer(ui);
        } else {
            self.narrow_navigation(ui);
        }
        self.editor_and_results(ui);
        self.show_delete_confirmation(ui);
    }

    fn show_first_run(&mut self, ui: &mut egui::Ui) {
        ui.vertical_centered(|ui| {
            ui.set_max_width(520.0);
            ui.add_space(64.0);
            ui.heading("Connect your first database");
            ui.label("Create a local profile. Credentials stay outside the saved profile.");
            ui.add_space(24.0);
            ui.label("Database");

            let mysql = ui.add_sized(
                [280.0, OpenAiTheme::MIN_CONTROL_HEIGHT],
                egui::RadioButton::new(self.first_run_driver == DriverKind::MySql, "MySQL"),
            );
            if named_author_id(mysql, "connection.new.mysql", "New MySQL connection").clicked() {
                self.first_run_driver = DriverKind::MySql;
            }
            let redis = ui.add_sized(
                [280.0, OpenAiTheme::MIN_CONTROL_HEIGHT],
                egui::RadioButton::new(self.first_run_driver == DriverKind::Redis, "Redis"),
            );
            if named_author_id(redis, "connection.new.redis", "New Redis connection").clicked() {
                self.first_run_driver = DriverKind::Redis;
            }
            let mongodb = ui.add_enabled(false, egui::RadioButton::new(false, "MongoDB · Planned"));
            named_author_id(
                mongodb,
                "connection.mongodb.planned",
                "MongoDB planned and unavailable",
            );

            ui.add_space(24.0);
            let primary = ui.add(
                egui::Button::new(
                    egui::RichText::new("New connection").color(egui::Color32::WHITE),
                )
                .fill(egui::Color32::BLACK)
                .min_size(egui::vec2(280.0, OpenAiTheme::MIN_CONTROL_HEIGHT)),
            );
            if named_author_id(primary, "connection.new", "New connection").clicked() {
                let draft_id = self.allocate_draft_id();
                self.profile_editor = Some(ProfileEditor::new(draft_id, self.first_run_driver));
            }
            ui.add_space(24.0);
            ui.label("Credential sources: None · This app session · Environment variable");
        });
    }

    fn narrow_navigation(&mut self, root_ui: &mut egui::Ui) {
        egui::Panel::top("narrow-navigation").show(root_ui, |ui| {
            egui::CollapsingHeader::new("Connections")
                .default_open(false)
                .show(ui, |ui| self.connections_contents(ui));
            egui::CollapsingHeader::new("Explorer")
                .default_open(false)
                .show(ui, |ui| self.explorer_contents(ui));
        });
    }

    fn connections(&mut self, root_ui: &mut egui::Ui) {
        egui::Panel::left("connections")
            .resizable(false)
            .exact_size(NativeLayout::CONNECTIONS_WIDTH)
            .show(root_ui, |ui| self.connections_contents(ui));
    }

    fn connections_contents(&mut self, ui: &mut egui::Ui) {
        ui.heading("Connections");
        let actions_enabled = !self.model.is_config_uncertain();
        let mysql = ui.add_enabled(
            actions_enabled,
            egui::Button::new("+ MySQL")
                .min_size(egui::vec2(104.0, OpenAiTheme::MIN_CONTROL_HEIGHT)),
        );
        if named_author_id(mysql, "connection.new.mysql", "New MySQL connection").clicked() {
            let draft_id = self.allocate_draft_id();
            self.profile_editor = Some(ProfileEditor::new(draft_id, DriverKind::MySql));
        }
        let redis = ui.add_enabled(
            actions_enabled,
            egui::Button::new("+ Redis")
                .min_size(egui::vec2(104.0, OpenAiTheme::MIN_CONTROL_HEIGHT)),
        );
        if named_author_id(redis, "connection.new.redis", "New Redis connection").clicked() {
            let draft_id = self.allocate_draft_id();
            self.profile_editor = Some(ProfileEditor::new(draft_id, DriverKind::Redis));
        }
        let mongodb = ui.add_enabled(
            false,
            egui::Button::new("MongoDB · Planned")
                .min_size(egui::vec2(160.0, OpenAiTheme::MIN_CONTROL_HEIGHT)),
        );
        named_author_id(
            mongodb,
            "connection.mongodb.planned",
            "MongoDB planned and unavailable",
        );
        if ui
            .add_sized(
                [104.0, OpenAiTheme::MIN_CONTROL_HEIGHT],
                egui::Button::new("Reload"),
            )
            .clicked()
        {
            self.submit_refresh();
        }
        ui.separator();
        egui::ScrollArea::vertical().show(ui, |ui| {
            for profile in self.model.profiles.clone() {
                self.profile_card(ui, &profile);
                ui.add_space(8.0);
            }
        });
    }

    fn explorer(&mut self, root_ui: &mut egui::Ui) {
        egui::Panel::right("explorer")
            .resizable(false)
            .exact_size(NativeLayout::EXPLORER_WIDTH)
            .show(root_ui, |ui| {
                ui.heading("Explorer");
                ui.separator();
                self.explorer_contents(ui);
            });
    }

    fn explorer_contents(&mut self, ui: &mut egui::Ui) {
        let selected = self.model.selected_profile_snapshot().cloned();
        match selected {
            Some(profile) if profile.driver == DriverKind::MySql && profile.is_ready() => {
                self.redis_explorer.set_profile(None);
                let intents = self
                    .mysql_explorers
                    .entry((profile.id.clone(), profile.generation))
                    .or_default()
                    .show(ui);
                for intent in intents {
                    self.submit_mysql_explorer_intent(&profile, intent);
                }
            }
            Some(profile) if profile.driver == DriverKind::Redis && profile.is_ready() => {
                self.redis_explorer
                    .set_profile(Some((profile.id.clone(), profile.generation)));
                if let Some(intent) = self
                    .redis_explorer
                    .show(ui, !self.model.is_config_uncertain())
                {
                    self.submit_redis_intent(intent);
                }
            }
            Some(profile) => {
                self.redis_explorer.set_profile(None);
                ui.weak(format!("{} explorer is unavailable", profile.driver));
            }
            None => {
                self.redis_explorer.set_profile(None);
                ui.weak("Select a connection to browse resources.");
            }
        }
    }

    fn profile_card(&mut self, ui: &mut egui::Ui, profile: &ProfileSnapshot) {
        let selected = self.model.selected_profile.as_ref() == Some(&profile.id);
        if ui
            .selectable_label(selected, format!("{} · {}", profile.name, profile.driver))
            .clicked()
        {
            self.model.selected_profile = Some(profile.id.clone());
        }
        ui.small(&profile.endpoint);
        if let Some(database) = &profile.database {
            ui.small(format!("database: {database}"));
        }
        let state = self.model.connection_state(&profile.id).clone();
        let actions_enabled = !self.model.is_config_uncertain();
        ui.horizontal_wrapped(|ui| {
            ui.label(connection_label(&state));
            match state {
                ConnectionState::Disconnected | ConnectionState::Failed { .. } => {
                    let connect = ui.add_enabled(
                        actions_enabled && profile.is_ready(),
                        egui::Button::new("Connect")
                            .min_size(egui::vec2(104.0, OpenAiTheme::MIN_CONTROL_HEIGHT)),
                    );
                    if named_author_id(connect, "connection.connect", "Connect to profile")
                        .clicked()
                    {
                        self.submit_test(profile.id.clone());
                    }
                }
                ConnectionState::NeedsCredential => {
                    let credential = ui.add_enabled(
                        actions_enabled,
                        egui::Button::new("Enter credential")
                            .min_size(egui::vec2(144.0, OpenAiTheme::MIN_CONTROL_HEIGHT)),
                    );
                    if named_author_id(
                        credential,
                        "connection.credential.open",
                        "Enter session credential",
                    )
                    .clicked()
                    {
                        self.open_session_credential_prompt(profile.id.clone());
                    }
                }
                ConnectionState::Connected { .. } => {
                    let disconnect = ui.add_enabled(
                        actions_enabled,
                        egui::Button::new("Disconnect")
                            .min_size(egui::vec2(112.0, OpenAiTheme::MIN_CONTROL_HEIGHT)),
                    );
                    if named_author_id(disconnect, "connection.disconnect", "Disconnect profile")
                        .clicked()
                    {
                        self.submit_disconnect(profile.id.clone());
                    }
                    let reconnect = ui.add_enabled(
                        actions_enabled,
                        egui::Button::new("Reconnect")
                            .min_size(egui::vec2(112.0, OpenAiTheme::MIN_CONTROL_HEIGHT)),
                    );
                    if named_author_id(reconnect, "connection.reconnect", "Reconnect profile")
                        .clicked()
                    {
                        self.submit_reconnect(profile.id.clone());
                    }
                }
                ConnectionState::Pending(_) | ConnectionState::Closing => {}
            }
        });
        ui.horizontal_wrapped(|ui| {
            let edit = ui.add_enabled(
                actions_enabled,
                egui::Button::new("Edit")
                    .min_size(egui::vec2(88.0, OpenAiTheme::MIN_CONTROL_HEIGHT)),
            );
            if named_author_id(edit, "profile.edit", "Edit profile").clicked() {
                let draft_id = self.allocate_draft_id();
                self.profile_editor = Some(ProfileEditor::edit(
                    draft_id,
                    &profile.persisted,
                    profile.generation,
                    profile.has_current_session_secret,
                ));
            }
            let delete = ui.add_enabled(
                actions_enabled,
                egui::Button::new("Delete")
                    .min_size(egui::vec2(88.0, OpenAiTheme::MIN_CONTROL_HEIGHT)),
            );
            if named_author_id(delete, "profile.delete", "Delete profile").clicked() {
                self.open_delete_confirmation(profile);
            }
        });
        if profile.availability == DriverAvailability::Planned {
            ui.strong(format!(
                "Planned: {}",
                profile.planned_reason.as_deref().unwrap_or("not available")
            ));
        }
        if selected {
            ui.weak("Open in Explorer");
        }
    }

    fn editor_and_results(&mut self, root_ui: &mut egui::Ui) {
        if self.profile_editor.is_some() {
            let mut action = FormAction::None;
            egui::CentralPanel::default().show(root_ui, |ui| {
                if let Some(editor) = self.profile_editor.as_mut() {
                    action = editor.show(ui);
                }
            });
            match action {
                FormAction::Save { connect } => {
                    let operation_id = self.model.next_operation();
                    if let Some(editor) = self.profile_editor.as_mut() {
                        match editor.try_save_with_connect(&self.port, operation_id, connect) {
                            SaveAttempt::Submitted(_) => {
                                self.model.status = "Saving profile…".to_owned();
                            }
                            SaveAttempt::Invalid => {
                                self.model.status = "Fix the profile form".to_owned();
                            }
                            SaveAttempt::Busy => {
                                self.model.status = "Service is busy".to_owned();
                            }
                            SaveAttempt::Disconnected => {
                                self.model.status = "Service is unavailable".to_owned();
                            }
                            SaveAttempt::ConfigUncertain => {
                                self.model.status = "Reload profiles before saving.".to_owned();
                            }
                            SaveAttempt::AlreadyPending(_) => {
                                self.model.status = "Profile save is already pending".to_owned();
                            }
                        }
                    }
                }
                FormAction::TestDraft => {
                    let operation_id = self.model.next_operation();
                    if let Some(editor) = self.profile_editor.as_mut() {
                        match editor.try_test_draft(&self.port, operation_id) {
                            DraftTestAttempt::Submitted(_) => {
                                self.model.status = "Testing draft connection…".to_owned();
                            }
                            DraftTestAttempt::Invalid | DraftTestAttempt::Unavailable => {
                                self.model.status = editor.status().to_owned();
                            }
                            DraftTestAttempt::Busy => {
                                self.model.status = "Service is busy".to_owned();
                            }
                            DraftTestAttempt::Disconnected => {
                                self.model.status = "Service is unavailable".to_owned();
                            }
                            DraftTestAttempt::ConfigUncertain => {
                                self.model.status = "Reload profiles before testing.".to_owned();
                            }
                            DraftTestAttempt::AlreadyPending(_) => {
                                self.model.status = "Profile work is already pending".to_owned();
                            }
                        }
                    }
                }
                FormAction::ProbeEnvironment => {
                    if let Some(editor) = self.profile_editor.as_mut() {
                        let availability = editor.probe_environment_availability();
                        self.model.status = format!("Environment credential: {availability:?}");
                    }
                }
                FormAction::Cancel => self.profile_editor = None,
                FormAction::PickRedisCaFile => {
                    if let Some(path) = native_redis_ca_file_picker()
                        && let Some(editor) = self.profile_editor.as_mut()
                    {
                        editor.bind_redis_ca_file(path);
                        self.model.status = "Redis CA file selected".to_owned();
                    }
                }
                FormAction::None => {}
            }
            return;
        }
        let mut editor_intent = None;
        egui::CentralPanel::default().show(root_ui, |ui| {
            let selected_workspace_key = self.model.selected_workspace_key();
            if let Some(profile) = self.model.selected_profile_snapshot().cloned() {
                let editor_enabled = !self.model.is_config_uncertain();
                let key = super::model::WorkspaceKey::new(profile.id.clone(), profile.generation);
                let workspace = self.model.workspace_mut(key);
                editor_intent = self.editor_surface.show(
                    ui,
                    &profile,
                    workspace,
                    editor_enabled && profile.is_ready(),
                );
            } else {
                ui.weak("Select a connection to edit a statement or command.");
            }
            ui.horizontal(|ui| {
                ui.label(&self.model.status);
            });
            ui.separator();
            ui.heading("Results");
            if let Some(result) = selected_workspace_key
                .as_ref()
                .and_then(|key| self.model.workspace(key))
                .and_then(|workspace| workspace.result.as_ref())
            {
                render_result(ui, result.as_ref());
            } else {
                ui.weak("No result yet");
            }
        });
        if let Some(intent) = editor_intent {
            self.submit_editor_intent(intent);
        }
    }
}

impl eframe::App for DbotterApp {
    fn logic(&mut self, context: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_events();
        context.request_repaint_after(Duration::from_millis(50));
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.show_native(ui);
    }
}

fn catalog_request_with_identity(
    request: CatalogRequest,
    identity: RequestIdentity,
) -> CatalogRequest {
    match request {
        CatalogRequest::Schemas {
            prefix,
            page_token,
            page_size,
            timeout,
            ..
        } => CatalogRequest::Schemas {
            identity,
            prefix,
            page_token,
            page_size,
            timeout,
        },
        CatalogRequest::Relations {
            schema,
            prefix,
            page_token,
            page_size,
            timeout,
            ..
        } => CatalogRequest::Relations {
            identity,
            schema,
            prefix,
            page_token,
            page_size,
            timeout,
        },
        CatalogRequest::Columns {
            schema,
            relation,
            prefix,
            page_token,
            page_size,
            timeout,
            ..
        } => CatalogRequest::Columns {
            identity,
            schema,
            relation,
            prefix,
            page_token,
            page_size,
            timeout,
        },
    }
}

fn connection_label(state: &ConnectionState) -> String {
    match state {
        ConnectionState::Disconnected => "● Disconnected".to_owned(),
        ConnectionState::Pending(_) => "◌ Connecting…".to_owned(),
        ConnectionState::Connected { elapsed_ms, .. } => {
            format!("● Connected · {elapsed_ms} ms")
        }
        ConnectionState::NeedsCredential => "● Credential required".to_owned(),
        ConnectionState::Failed { summary } => {
            format!("● Failed · {}", summary.message())
        }
        ConnectionState::Closing => "◌ Closing…".to_owned(),
    }
}

const fn submit_error_message(error: SubmitError) -> &'static str {
    match error {
        SubmitError::Busy => "The service queue is busy; try again.",
        SubmitError::Disconnected => "The service is unavailable.",
    }
}

#[cfg(target_os = "macos")]
fn native_redis_ca_file_picker() -> Option<PathBuf> {
    let output = std::process::Command::new("/usr/bin/osascript")
        .args([
            "-e",
            "POSIX path of (choose file with prompt \"Choose a Redis TLS CA file\")",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?;
    let value = value.trim_end_matches(['\r', '\n']);
    (!value.is_empty()).then(|| PathBuf::from(value))
}

#[cfg(not(target_os = "macos"))]
fn native_redis_ca_file_picker() -> Option<PathBuf> {
    None
}

fn render_result(ui: &mut egui::Ui, result: &crate::model::ResultSnapshot) {
    ui.horizontal_wrapped(|ui| {
        ui.label(format!("{} rows", result.rows.len()));
        ui.label(format!("{} affected", result.affected_rows));
        ui.label(format!("{} ms", result.provenance.duration_ms));
        if let Some(last_insert_id) = result.last_insert_id {
            ui.label(format!("last insert id {last_insert_id}"));
        }
        if result.truncated {
            ui.strong("Warning: result is truncated");
        }
    });
    for notice in &result.notices {
        ui.small(notice.message());
    }
    if result.columns.is_empty() {
        return;
    }
    let column_count = result.columns.len();
    let mut table = TableBuilder::new(ui)
        .striped(true)
        .resizable(true)
        .column(TableColumn::auto());
    if column_count > 1 {
        table = table.columns(TableColumn::remainder(), column_count - 1);
    }
    table
        .header(24.0, |mut header| {
            for column in &result.columns {
                header.col(|ui| {
                    ui.strong(&column.name);
                    ui.small(&column.type_name);
                });
            }
        })
        .body(|body| {
            body.rows(22.0, result.rows.len(), |mut row| {
                let cells = &result.rows[row.index()];
                for index in 0..column_count {
                    row.col(|ui| match cells.get(index) {
                        Some(cell) => {
                            ui.label(display_cell(cell));
                        }
                        None => {
                            ui.strong("Error: <missing>");
                        }
                    });
                }
            });
        });
}

fn display_cell(cell: &Cell) -> String {
    match cell {
        Cell::Null => "NULL".to_owned(),
        Cell::Bool(value) => value.to_string(),
        Cell::Int(value) => value.to_string(),
        Cell::UInt(value) => value.to_string(),
        Cell::Float(value) => value.to_string(),
        Cell::Decimal(value) | Cell::Text(value) | Cell::DateTime(value) => value.clone(),
        Cell::Bytes { preview, len } => format!("{preview} ({len} bytes)"),
        Cell::Json(value) => value.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::{
        ActiveOperation, ConnectionState, DbotterApp, MySqlExplorerIntent, PendingDelete,
        ProfileEditor,
    };
    use crate::model::{
        ConnectionProfile, CredentialMode, DraftId, DriverAvailability, DriverKind, OperationId,
        OperationKind, OperationRecipeId, ProfileFieldId, ProfileGeneration, ProfileId, PublicCode,
        PublicSummary, RedisKeyFilter, RedisKeyId, RedisTlsConfig, SessionGeneration, TlsMode,
    };
    use crate::public_error::{PublicOperationError, RecoveryAction, SafeContext};
    use crate::ui::adapter::{UiCommand, bounded_ports};
    use crate::ui::editor::{EditorCursor, EditorIntent, build_execute_intent};
    use crate::ui::model::{ProfileSnapshot, WorkspaceKey};
    use crate::ui::redis_explorer::RedisExplorerIntent;
    use eframe::egui::{Context, Event, Key, Modifiers, RawInput, accesskit};

    fn profile(driver: DriverKind, availability: DriverAvailability) -> ProfileSnapshot {
        let persisted = ConnectionProfile {
            id: "profile".to_owned(),
            name: "Profile".to_owned(),
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
            credential_mode: CredentialMode::None,
            secret_env: None,
            redis_tls: RedisTlsConfig::default(),
        };
        ProfileSnapshot {
            id: ProfileId("profile".to_owned()),
            generation: ProfileGeneration(1),
            name: "Profile".to_owned(),
            driver,
            endpoint: "mysql://127.0.0.1:3306".to_owned(),
            database: None,
            availability,
            planned_reason: None,
            has_current_session_secret: false,
            persisted,
        }
    }

    fn prime_save_and_connect(
        app: &mut DbotterApp,
        service: &mut crate::ui::adapter::ServicePort,
    ) -> (ProfileId, OperationId) {
        let mut editor = ProfileEditor::new(DraftId(401), DriverKind::MySql);
        editor.draft.name = "Profile".to_owned();
        let save_operation = app.model.next_operation();
        assert!(matches!(
            editor.try_save_with_connect(&app.port, save_operation, true),
            crate::ui::profile_form::SaveAttempt::Submitted(operation)
                if operation == save_operation
        ));
        app.profile_editor = Some(editor);
        assert!(matches!(
            service.try_next_command(),
            Some(UiCommand::CreateProfile(request)) if request.operation_id == save_operation
        ));
        let profile_id = ProfileId("profile".to_owned());
        assert!(service.try_emit(crate::ui::UiEvent::ProfileSaved {
            operation_id: save_operation,
            profile_id: profile_id.clone(),
            previous_generation: None,
            profile_generation: ProfileGeneration(1),
            session_retained: false,
            warning: None,
        }));
        app.poll_events();
        let Some(UiCommand::RefreshProfiles { operation_id }) = service.try_next_command() else {
            panic!("Save & Connect must submit an exact follow-up refresh");
        };
        (profile_id, operation_id)
    }

    fn reload_failure(operation_id: OperationId) -> PublicOperationError {
        PublicOperationError::new_or_internal(
            OperationKind::ReloadConfiguration,
            PublicSummary::NetworkUnavailable,
            PublicCode::None,
            &SafeContext::global(operation_id),
        )
    }

    #[test]
    fn actual_app_profile_editor_exposes_frozen_ids_and_config_cancel_escape() {
        let (ui_port, mut service) = bounded_ports(4);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let mut editor = ProfileEditor::new(DraftId(400), DriverKind::Redis);
        editor.draft.name = "Redis Profile".to_owned();
        editor.draft.select_tls(TlsMode::Required);
        editor.select_credential_mode(CredentialMode::Session);
        editor.set_config_uncertain(true);
        app.profile_editor = Some(editor);

        let context = Context::default();
        context.enable_accesskit();
        let output = context.run_ui(RawInput::default(), |ui| app.editor_and_results(ui));
        let update = output
            .platform_output
            .accesskit_update
            .expect("actual app profile frame must emit AccessKit");
        for expected in [
            "profile.connection_id",
            "profile.host",
            "profile.redis_tls.ca_file",
            "profile.redis_tls.ca_file.pick",
            "profile.credential.session.keep",
            "profile.credential.session.replace",
            "profile.credential.session.forget",
        ] {
            assert!(
                update
                    .nodes
                    .iter()
                    .any(|(_, node)| node.author_id() == Some(expected)),
                "missing actual app AX id {expected}"
            );
        }
        let cancel = update
            .nodes
            .iter()
            .find_map(|(_, node)| (node.author_id() == Some("profile.cancel")).then_some(node))
            .expect("actual app Cancel node");
        assert!(
            !cancel.is_disabled(),
            "Config uncertain must not trap the form"
        );
    }

    #[test]
    fn actual_app_first_run_exposes_one_primary_action_and_driver_choices() {
        let (ui_port, mut service) = bounded_ports(4);
        let mut app = DbotterApp::new(ui_port);
        let operation_id = match service.try_next_command() {
            Some(UiCommand::RefreshProfiles { operation_id }) => operation_id,
            _ => panic!("startup must request the exact profile list"),
        };
        assert!(service.try_emit(crate::ui::UiEvent::ProfilesLoaded {
            operation_id,
            profiles: Vec::new(),
        }));
        app.poll_events();

        let context = Context::default();
        context.enable_accesskit();
        let output = context.run_ui(RawInput::default(), |ui| app.show_native(ui));
        let update = output
            .platform_output
            .accesskit_update
            .expect("actual first-run app frame must emit AccessKit");
        let node = |author_id: &str| {
            update
                .nodes
                .iter()
                .find_map(|(_, node)| (node.author_id() == Some(author_id)).then_some(node))
                .unwrap_or_else(|| panic!("missing actual first-run AX id {author_id}"))
        };

        assert_eq!(node("connection.new").role(), accesskit::Role::Button);
        assert_eq!(
            node("connection.new.mysql").role(),
            accesskit::Role::RadioButton
        );
        assert_eq!(
            node("connection.new.redis").role(),
            accesskit::Role::RadioButton
        );
        let mongodb = node("connection.mongodb.planned");
        assert_eq!(mongodb.role(), accesskit::Role::RadioButton);
        assert!(mongodb.is_disabled());
    }

    #[test]
    fn actual_profile_cards_expose_exact_lifecycle_delete_and_credential_actions() {
        let render_ids = |state: ConnectionState, session_profile: bool| {
            let (ui_port, mut service) = bounded_ports(4);
            let mut app = DbotterApp::new(ui_port);
            assert!(service.try_next_command().is_some());
            let mut profile = profile(DriverKind::MySql, DriverAvailability::Ready);
            if session_profile {
                profile.persisted.credential_mode = CredentialMode::Session;
            }
            app.model.selected_profile = Some(profile.id.clone());
            app.model
                .active_generations
                .insert(profile.id.clone(), profile.generation);
            app.model
                .connection_states
                .insert(profile.id.clone(), state);
            app.model.profiles = vec![profile];

            let context = Context::default();
            context.enable_accesskit();
            context
                .run_ui(RawInput::default(), |ui| app.connections(ui))
                .platform_output
                .accesskit_update
                .expect("actual lifecycle frame must emit AccessKit")
                .nodes
                .into_iter()
                .filter_map(|(_, node)| node.author_id().map(str::to_owned))
                .collect::<BTreeSet<_>>()
        };

        let disconnected = render_ids(ConnectionState::Disconnected, false);
        assert!(disconnected.contains("connection.connect"));
        assert!(disconnected.contains("profile.delete"));

        let connected = render_ids(
            ConnectionState::Connected {
                session_generation: SessionGeneration(8),
                elapsed_ms: 3,
            },
            false,
        );
        assert!(connected.contains("connection.disconnect"));
        assert!(connected.contains("connection.reconnect"));
        assert!(connected.contains("profile.delete"));

        let needs_credential = render_ids(ConnectionState::NeedsCredential, true);
        assert!(needs_credential.contains("connection.credential.open"));
        assert!(needs_credential.contains("profile.delete"));
    }

    #[test]
    fn lifecycle_submits_exact_commands_and_sets_pending_only_after_acceptance() {
        let (ui_port, mut service) = bounded_ports(4);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let profile = profile(DriverKind::MySql, DriverAvailability::Ready);
        app.model.profiles = vec![profile.clone()];
        app.model.selected_profile = Some(profile.id.clone());
        app.model
            .active_generations
            .insert(profile.id.clone(), profile.generation);

        app.submit_test(profile.id.clone());
        let connect_operation = match service.try_next_command() {
            Some(UiCommand::TestConnection {
                operation_id,
                profile_id,
                profile_generation,
                timeout_ms,
            }) => {
                assert_eq!(profile_id, profile.id);
                assert_eq!(profile_generation, profile.generation);
                assert_eq!(timeout_ms, super::DEFAULT_TIMEOUT_MS);
                operation_id
            }
            _ => panic!("Connect must submit one exact non-secret TestConnection"),
        };
        assert_eq!(
            app.model.connection_state(&profile.id),
            &ConnectionState::Pending(connect_operation)
        );
        assert_eq!(
            app.active_operations.get(&profile.id),
            Some(&ActiveOperation {
                operation_id: connect_operation,
                profile_generation: profile.generation,
                kind: OperationKind::ConnectProfile,
            })
        );

        assert!(service.try_emit(crate::ui::UiEvent::ConnectionReady {
            operation_id: connect_operation,
            profile_id: profile.id.clone(),
            profile_generation: profile.generation,
            session_generation: SessionGeneration(11),
            elapsed_ms: 4,
        }));
        app.poll_events();
        app.submit_disconnect(profile.id.clone());
        let disconnect_operation = match service.try_next_command() {
            Some(UiCommand::DisconnectProfile {
                operation_id,
                profile_id,
                profile_generation,
            }) => {
                assert_eq!(profile_id, profile.id);
                assert_eq!(profile_generation, profile.generation);
                operation_id
            }
            _ => panic!("Disconnect must submit one exact control command"),
        };
        assert_eq!(
            app.model.connection_state(&profile.id),
            &ConnectionState::Pending(disconnect_operation)
        );

        assert!(service.try_emit(crate::ui::UiEvent::ConnectionClosed {
            operation_id: disconnect_operation,
            profile_id: profile.id.clone(),
            profile_generation: profile.generation,
            post_close: crate::ui::PostCloseState::Disconnected,
        }));
        app.poll_events();
        app.submit_reconnect(profile.id.clone());
        let reconnect_operation = match service.try_next_command() {
            Some(UiCommand::ReconnectProfile {
                operation_id,
                profile_id,
                profile_generation,
                timeout_ms,
            }) => {
                assert_eq!(profile_id, profile.id);
                assert_eq!(profile_generation, profile.generation);
                assert_eq!(timeout_ms, super::DEFAULT_TIMEOUT_MS);
                operation_id
            }
            _ => panic!("Reconnect must submit one exact control command"),
        };
        assert_eq!(
            app.model.connection_state(&profile.id),
            &ConnectionState::Pending(reconnect_operation)
        );

        for lifecycle in [
            OperationKind::ConnectProfile,
            OperationKind::DisconnectProfile,
            OperationKind::ReconnectProfile,
        ] {
            let (ui_port, mut service) = bounded_ports(1);
            let mut blocked = DbotterApp::new(ui_port);
            assert!(service.try_next_command().is_some());
            blocked.model.profiles = vec![profile.clone()];
            blocked
                .model
                .active_generations
                .insert(profile.id.clone(), profile.generation);
            blocked.model.connection_states.insert(
                profile.id.clone(),
                ConnectionState::Connected {
                    session_generation: SessionGeneration(12),
                    elapsed_ms: 1,
                },
            );
            let filler = match lifecycle {
                OperationKind::ConnectProfile => UiCommand::TestConnection {
                    operation_id: OperationId(800),
                    profile_id: ProfileId("filler".to_owned()),
                    profile_generation: ProfileGeneration(1),
                    timeout_ms: 1,
                },
                OperationKind::DisconnectProfile | OperationKind::ReconnectProfile => {
                    UiCommand::CancelOperation {
                        operation_id: OperationId(801),
                    }
                }
                _ => unreachable!("closed lifecycle fixture"),
            };
            assert_eq!(blocked.port.try_submit(filler), Ok(()));
            match lifecycle {
                OperationKind::ConnectProfile => {
                    blocked
                        .model
                        .connection_states
                        .insert(profile.id.clone(), ConnectionState::Disconnected);
                    blocked.submit_test(profile.id.clone());
                    assert_eq!(
                        blocked.model.connection_state(&profile.id),
                        &ConnectionState::Disconnected
                    );
                }
                OperationKind::DisconnectProfile => {
                    blocked.submit_disconnect(profile.id.clone());
                    assert!(matches!(
                        blocked.model.connection_state(&profile.id),
                        ConnectionState::Connected { .. }
                    ));
                }
                OperationKind::ReconnectProfile => {
                    blocked.submit_reconnect(profile.id.clone());
                    assert!(matches!(
                        blocked.model.connection_state(&profile.id),
                        ConnectionState::Connected { .. }
                    ));
                }
                _ => unreachable!("closed lifecycle fixture"),
            }
            assert!(!blocked.active_operations.contains_key(&profile.id));
        }
    }

    #[test]
    fn credential_prompt_stores_exact_generation_then_retries_connect_exactly_once() {
        let (ui_port, mut service) = bounded_ports(4);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let mut session_profile = profile(DriverKind::MySql, DriverAvailability::Ready);
        session_profile.persisted.credential_mode = CredentialMode::Session;
        session_profile.has_current_session_secret = false;
        app.model.profiles = vec![session_profile.clone()];
        app.model
            .active_generations
            .insert(session_profile.id.clone(), session_profile.generation);
        app.model
            .connection_states
            .insert(session_profile.id.clone(), ConnectionState::NeedsCredential);

        app.submit_test(session_profile.id.clone());
        assert!(service.try_next_command().is_none());
        assert!(
            app.profile_editor.is_none(),
            "credential entry is not profile editing"
        );
        let prompt = app
            .credential_prompt
            .as_mut()
            .expect("protected credential prompt");
        prompt.secret =
            crate::secrets::ReplacementSecretBuffer::new("memory-only-secret".to_owned());
        app.submit_credential_prompt();
        let store_operation = match service.try_next_command() {
            Some(UiCommand::StoreCredentials {
                operation_id,
                profile_id,
                profile_generation,
                source_operation,
                ..
            }) => {
                assert_eq!(profile_id, session_profile.id);
                assert_eq!(profile_generation, session_profile.generation);
                assert_eq!(source_operation, OperationKind::ConnectProfile);
                operation_id
            }
            _ => panic!("credential prompt must submit one StoreCredentials command"),
        };
        assert!(service.try_next_command().is_none());
        assert!(service.try_emit(crate::ui::UiEvent::CredentialsStored {
            operation_id: store_operation,
            profile_id: session_profile.id.clone(),
            profile_generation: session_profile.generation,
        }));
        app.poll_events();
        let retry_operation = match service.try_next_command() {
            Some(UiCommand::TestConnection {
                operation_id,
                profile_id,
                profile_generation,
                timeout_ms,
            }) => {
                assert_eq!(profile_id, session_profile.id);
                assert_eq!(profile_generation, session_profile.generation);
                assert_eq!(timeout_ms, super::DEFAULT_TIMEOUT_MS);
                operation_id
            }
            _ => panic!("successful credential storage retries the exact connect recipe"),
        };
        assert_ne!(retry_operation, store_operation);
        assert_eq!(
            app.model.active_generation(&session_profile.id),
            Some(session_profile.generation),
            "credential storage must not mutate profile generation"
        );

        assert!(service.try_emit(crate::ui::UiEvent::CredentialsStored {
            operation_id: store_operation,
            profile_id: session_profile.id.clone(),
            profile_generation: session_profile.generation,
        }));
        app.poll_events();
        assert!(
            service.try_next_command().is_none(),
            "store ack retries only once"
        );
    }

    #[test]
    fn credential_cancel_store_failure_and_generation_change_never_retry() {
        let (ui_port, mut service) = bounded_ports(4);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let mut session_profile = profile(DriverKind::MySql, DriverAvailability::Ready);
        session_profile.persisted.credential_mode = CredentialMode::Session;
        app.model.profiles = vec![session_profile.clone()];
        app.model
            .active_generations
            .insert(session_profile.id.clone(), session_profile.generation);

        app.open_session_credential_prompt(session_profile.id.clone());
        app.credential_prompt.as_mut().expect("prompt").secret =
            crate::secrets::ReplacementSecretBuffer::new("cancelled-secret".to_owned());
        app.cancel_credential_prompt();
        assert!(app.credential_prompt.is_none());
        assert!(service.try_next_command().is_none());

        app.open_session_credential_prompt(session_profile.id.clone());
        app.credential_prompt.as_mut().expect("prompt").secret =
            crate::secrets::ReplacementSecretBuffer::new("stale-secret".to_owned());
        app.submit_credential_prompt();
        let store_operation = match service.try_next_command() {
            Some(UiCommand::StoreCredentials { operation_id, .. }) => operation_id,
            _ => panic!("StoreCredentials command"),
        };
        app.model.active_generations.insert(
            session_profile.id.clone(),
            ProfileGeneration(session_profile.generation.0 + 1),
        );
        assert!(service.try_emit(crate::ui::UiEvent::CredentialsStored {
            operation_id: store_operation,
            profile_id: session_profile.id.clone(),
            profile_generation: session_profile.generation,
        }));
        app.poll_events();
        assert!(app.credential_prompt.is_none());
        assert!(service.try_next_command().is_none());
    }

    #[test]
    fn rejected_delete_submission_keeps_confirmation_and_pending_state_local() {
        let (ui_port, mut service) = bounded_ports(1);
        let mut blocked_delete = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let profile = profile(DriverKind::MySql, DriverAvailability::Ready);
        blocked_delete.model.profiles = vec![profile.clone()];
        blocked_delete
            .model
            .active_generations
            .insert(profile.id.clone(), profile.generation);
        blocked_delete.open_delete_confirmation(&profile);
        assert_eq!(
            blocked_delete.port.try_submit(UiCommand::RefreshProfiles {
                operation_id: OperationId(903),
            }),
            Ok(())
        );
        blocked_delete.confirm_delete_confirmation();
        assert!(blocked_delete.delete_confirmation.is_some());
        assert_eq!(
            blocked_delete.model.connection_state(&profile.id),
            &ConnectionState::Disconnected
        );
        assert!(!blocked_delete.active_operations.contains_key(&profile.id));
        assert!(matches!(
            service.try_next_command(),
            Some(UiCommand::RefreshProfiles {
                operation_id: OperationId(903)
            })
        ));
        assert!(service.try_next_command().is_none());
    }

    #[test]
    fn generation_checked_catalog_and_redis_recipes_replay_exact_requests() {
        let (ui_port, mut service) = bounded_ports(8);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let mysql = profile(DriverKind::MySql, DriverAvailability::Ready);
        app.model.profiles = vec![mysql.clone()];
        app.model.selected_profile = Some(mysql.id.clone());
        app.model
            .active_generations
            .insert(mysql.id.clone(), mysql.generation);
        app.submit_mysql_explorer_intent(
            &mysql,
            MySqlExplorerIntent::RefreshSchemas {
                prefix: Some("app".to_owned()),
            },
        );
        let request = match service.try_next_command() {
            Some(UiCommand::BrowseCatalog(request)) => request,
            _ => panic!("catalog request"),
        };
        let recipe_id = OperationRecipeId(request.operation_id().0);
        let error = PublicOperationError::new_or_internal(
            OperationKind::BrowseMySql,
            PublicSummary::ResourceStale,
            PublicCode::None,
            &SafeContext::profile_with_recipe(mysql.id.clone(), request.operation_id(), recipe_id),
        );
        assert!(service.try_emit(crate::ui::UiEvent::CatalogPageFailed {
            request: request.clone(),
            summary: error.summary,
            error: error.clone(),
            session_generation: None,
            session_disposition: None,
        }));
        app.poll_events();
        app.dispatch_error_recovery(
            request.operation_id(),
            &error,
            RecoveryAction::Retry(recipe_id),
        );
        let retried = match service.try_next_command() {
            Some(UiCommand::BrowseCatalog(request)) => request,
            _ => panic!("typed catalog Retry must replay one exact request"),
        };
        assert_eq!(retried.profile_id(), request.profile_id());
        assert_eq!(retried.profile_generation(), request.profile_generation());
        assert_ne!(retried.operation_id(), request.operation_id());

        let (ui_port, mut service) = bounded_ports(8);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let redis = profile(DriverKind::Redis, DriverAvailability::Ready);
        app.model.profiles = vec![redis.clone()];
        app.model.selected_profile = Some(redis.id.clone());
        app.model
            .active_generations
            .insert(redis.id.clone(), redis.generation);
        app.submit_redis_intent(RedisExplorerIntent::Scan {
            filter: RedisKeyFilter::LiteralPrefix("orders:".to_owned()),
            cursor: 41,
            restart: false,
        });
        let request = match service.try_next_command() {
            Some(UiCommand::ScanRedisKeys(request)) => request,
            _ => panic!("Redis scan request"),
        };
        let recipe_id = OperationRecipeId(request.operation_id().0);
        let error = PublicOperationError::new_or_internal(
            OperationKind::BrowseRedis,
            PublicSummary::ResourceStale,
            PublicCode::None,
            &SafeContext::profile_with_recipe(redis.id.clone(), request.operation_id(), recipe_id),
        );
        assert!(service.try_emit(crate::ui::UiEvent::RedisKeysFailed {
            request: request.clone(),
            error: error.clone(),
            session_generation: None,
            session_disposition: None,
            connection_outcome: crate::ui::ConnectionFailureOutcome::Preserve,
        }));
        app.poll_events();
        app.dispatch_error_recovery(
            request.operation_id(),
            &error,
            RecoveryAction::Retry(recipe_id),
        );
        let retried = match service.try_next_command() {
            Some(UiCommand::ScanRedisKeys(request)) => request,
            _ => panic!("typed Redis Retry must replay one exact request"),
        };
        assert_eq!(retried.filter, request.filter);
        assert_eq!(retried.cursor, request.cursor);
        assert_eq!(retried.profile_generation(), request.profile_generation());
        assert_ne!(retried.operation_id(), request.operation_id());
    }

    #[test]
    fn actual_app_renders_common_catalog_and_redis_typed_recovery_actions() {
        let render_ids = |app: &mut DbotterApp, explorer_only: bool| {
            let context = Context::default();
            context.enable_accesskit();
            context
                .run_ui(RawInput::default(), |ui| {
                    if explorer_only {
                        app.explorer_contents(ui);
                    } else {
                        app.show_native(ui);
                    }
                })
                .platform_output
                .accesskit_update
                .expect("actual recovery frame must emit AccessKit")
                .nodes
                .into_iter()
                .filter_map(|(_, node)| node.author_id().map(str::to_owned))
                .collect::<BTreeSet<_>>()
        };

        let (ui_port, mut service) = bounded_ports(8);
        let mut common = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let mysql = profile(DriverKind::MySql, DriverAvailability::Ready);
        common.model.profiles = vec![mysql.clone()];
        common.model.selected_profile = Some(mysql.id.clone());
        common
            .model
            .active_generations
            .insert(mysql.id.clone(), mysql.generation);
        common.submit_test(mysql.id.clone());
        let operation_id = match service.try_next_command() {
            Some(UiCommand::TestConnection { operation_id, .. }) => operation_id,
            _ => panic!("connect command"),
        };
        let recipe_id = OperationRecipeId(operation_id.0);
        let error = PublicOperationError::new_or_internal(
            OperationKind::ConnectProfile,
            PublicSummary::NetworkUnavailable,
            PublicCode::None,
            &SafeContext::profile_with_recipe(mysql.id.clone(), operation_id, recipe_id),
        );
        assert!(service.try_emit(crate::ui::UiEvent::OperationFailed {
            operation_id,
            profile_id: mysql.id.clone(),
            profile_generation: mysql.generation,
            session_generation: None,
            kind: OperationKind::ConnectProfile,
            summary: error.summary,
            error,
            session_disposition: None,
            connection_outcome: crate::ui::ConnectionFailureOutcome::Disconnected,
        }));
        common.poll_events();
        let ids = render_ids(&mut common, false);
        assert!(ids.contains("recovery.common.edit_profile"));
        assert!(ids.contains("recovery.common.reconnect"));
        assert!(ids.contains("recovery.common.retry"));

        let (ui_port, mut service) = bounded_ports(8);
        let mut catalog = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        catalog.model.profiles = vec![mysql.clone()];
        catalog.model.selected_profile = Some(mysql.id.clone());
        catalog
            .model
            .active_generations
            .insert(mysql.id.clone(), mysql.generation);
        catalog.submit_mysql_explorer_intent(
            &mysql,
            MySqlExplorerIntent::RefreshSchemas { prefix: None },
        );
        let request = match service.try_next_command() {
            Some(UiCommand::BrowseCatalog(request)) => request,
            _ => panic!("catalog command"),
        };
        let recipe_id = OperationRecipeId(request.operation_id().0);
        let error = PublicOperationError::new_or_internal(
            OperationKind::BrowseMySql,
            PublicSummary::ResourceStale,
            PublicCode::None,
            &SafeContext::profile_with_recipe(mysql.id.clone(), request.operation_id(), recipe_id),
        );
        assert!(service.try_emit(crate::ui::UiEvent::CatalogPageFailed {
            request,
            summary: error.summary,
            error,
            session_generation: None,
            session_disposition: None,
        }));
        catalog.poll_events();
        assert!(render_ids(&mut catalog, true).contains("recovery.catalog.retry"));

        let (ui_port, mut service) = bounded_ports(8);
        let mut redis_app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let redis = profile(DriverKind::Redis, DriverAvailability::Ready);
        redis_app.model.profiles = vec![redis.clone()];
        redis_app.model.selected_profile = Some(redis.id.clone());
        redis_app
            .model
            .active_generations
            .insert(redis.id.clone(), redis.generation);
        redis_app.submit_redis_intent(RedisExplorerIntent::Scan {
            filter: RedisKeyFilter::Glob("*".to_owned()),
            cursor: 0,
            restart: true,
        });
        let request = match service.try_next_command() {
            Some(UiCommand::ScanRedisKeys(request)) => request,
            _ => panic!("Redis command"),
        };
        let recipe_id = OperationRecipeId(request.operation_id().0);
        let error = PublicOperationError::new_or_internal(
            OperationKind::BrowseRedis,
            PublicSummary::ResourceStale,
            PublicCode::None,
            &SafeContext::profile_with_recipe(redis.id.clone(), request.operation_id(), recipe_id),
        );
        assert!(service.try_emit(crate::ui::UiEvent::RedisKeysFailed {
            request,
            error,
            session_generation: None,
            session_disposition: None,
            connection_outcome: crate::ui::ConnectionFailureOutcome::Preserve,
        }));
        redis_app.poll_events();
        assert!(render_ids(&mut redis_app, true).contains("recovery.redis_scan.retry"));
    }

    #[test]
    fn mutation_execute_never_registers_or_dispatches_retry() {
        let (ui_port, mut service) = bounded_ports(4);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let mysql = profile(DriverKind::MySql, DriverAvailability::Ready);
        let key = WorkspaceKey::new(mysql.id.clone(), mysql.generation);
        app.model.profiles = vec![mysql.clone()];
        app.model.selected_profile = Some(mysql.id.clone());
        app.model
            .active_generations
            .insert(mysql.id.clone(), mysql.generation);
        app.model.workspace_mut(key.clone()).editor_text =
            "UPDATE inventory SET count = count + 1".to_owned();
        let intent = build_execute_intent(
            &mysql,
            app.model.workspace(&key).expect("workspace"),
            EditorCursor::caret(0),
        )
        .expect("mutation intent");
        assert_eq!(intent.operation_kind(), OperationKind::ExecuteMutation);
        app.submit_editor_intent(EditorIntent::Execute(intent));
        let operation_id = match service.try_next_command() {
            Some(UiCommand::Execute { operation_id, .. }) => operation_id,
            _ => panic!("mutation command"),
        };
        let recipe_id = OperationRecipeId(operation_id.0);
        assert!(!app.retry_recipes.contains(recipe_id));

        let forged = PublicOperationError {
            operation: OperationKind::ExecuteMutation,
            category: crate::public_error::ErrorCategory::Network,
            code: PublicCode::None,
            summary: PublicSummary::NetworkUnavailable,
            recovery: crate::public_error::NonEmpty::new(RecoveryAction::Retry(recipe_id)),
        };
        app.dispatch_error_recovery(operation_id, &forged, RecoveryAction::Retry(recipe_id));
        assert!(service.try_next_command().is_none());
    }

    #[test]
    fn confirmed_delete_is_exact_cancel_is_pure_and_unknown_truth_is_visible() {
        let (ui_port, mut service) = bounded_ports(4);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let profile = profile(DriverKind::MySql, DriverAvailability::Ready);
        app.model.profiles = vec![profile.clone()];
        app.model.selected_profile = Some(profile.id.clone());
        app.model
            .active_generations
            .insert(profile.id.clone(), profile.generation);
        let workspace_key = WorkspaceKey::new(profile.id.clone(), profile.generation);
        app.model.workspace_mut(workspace_key.clone()).editor_text =
            "UPDATE inventory SET count = count + 1".to_owned();
        let execute = build_execute_intent(
            &profile,
            app.model.workspace(&workspace_key).expect("workspace"),
            EditorCursor::caret(0),
        )
        .expect("mutation intent");
        app.submit_editor_intent(EditorIntent::Execute(execute));
        let active_operation = match service.try_next_command() {
            Some(UiCommand::Execute { operation_id, .. }) => operation_id,
            _ => panic!("mutation must be active before delete opens"),
        };
        assert_eq!(
            app.active_operations.get(&profile.id),
            Some(&ActiveOperation {
                operation_id: active_operation,
                profile_generation: profile.generation,
                kind: OperationKind::ExecuteMutation,
            })
        );

        app.open_delete_confirmation(&profile);
        let context = Context::default();
        context.enable_accesskit();
        let output = context.run_ui(RawInput::default(), |ui| app.show_delete_confirmation(ui));
        let warning = output
            .platform_output
            .accesskit_update
            .expect("delete dialog AccessKit")
            .nodes
            .into_iter()
            .find_map(|(_, node)| {
                (node.author_id() == Some("profile.delete.active_warning"))
                    .then(|| node.label().map(str::to_owned))
            })
            .flatten()
            .expect("active delete warning");
        assert_eq!(
            warning,
            "ExecuteMutation is active. Dbotter will stop waiting; the server operation may continue."
        );

        app.cancel_delete_confirmation();
        assert!(service.try_next_command().is_none());
        assert_eq!(
            app.active_operations.get(&profile.id),
            Some(&ActiveOperation {
                operation_id: active_operation,
                profile_generation: profile.generation,
                kind: OperationKind::ExecuteMutation,
            })
        );

        app.open_delete_confirmation(&profile);
        app.delete_confirmation
            .as_mut()
            .expect("delete confirmation")
            .migration_confirmed = true;
        app.confirm_delete_confirmation();
        let delete_operation = match service.try_next_command() {
            Some(UiCommand::DeleteProfile(request)) => {
                assert_eq!(request.profile_id, profile.id);
                assert_eq!(request.expected_generation, profile.generation);
                assert_eq!(
                    request.migration_consent,
                    super::MigrationConsent::Confirmed
                );
                request.operation_id
            }
            _ => panic!("confirmed delete must submit exactly once"),
        };
        assert_eq!(
            app.model.connection_state(&profile.id),
            &ConnectionState::Closing
        );
        app.confirm_delete_confirmation();
        assert!(service.try_next_command().is_none());

        assert!(service.try_emit(crate::ui::UiEvent::ProfileDeleted {
            operation_id: delete_operation,
            profile_id: profile.id.clone(),
            profile_generation: ProfileGeneration(profile.generation.0 + 1),
            server_state_unknown: true,
        }));
        app.poll_events();
        assert_eq!(
            app.model.status,
            "Profile deleted; server state is unknown."
        );
    }

    #[test]
    fn uncommitted_delete_failure_restores_the_exact_prior_active_operation() {
        let (ui_port, mut service) = bounded_ports(4);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let profile = profile(DriverKind::MySql, DriverAvailability::Ready);
        app.model.profiles = vec![profile.clone()];
        app.model
            .active_generations
            .insert(profile.id.clone(), profile.generation);
        let prior = ActiveOperation {
            operation_id: OperationId(77),
            profile_generation: profile.generation,
            kind: OperationKind::ExecuteMutation,
        };
        app.active_operations.insert(profile.id.clone(), prior);

        app.open_delete_confirmation(&profile);
        app.confirm_delete_confirmation();
        let delete_operation = match service.try_next_command() {
            Some(UiCommand::DeleteProfile(request)) => request.operation_id,
            _ => panic!("confirmed delete command"),
        };
        let error = PublicOperationError::new_or_internal(
            OperationKind::DeleteProfile,
            PublicSummary::ConfigWriteNotCommitted,
            PublicCode::None,
            &SafeContext::profile(profile.id.clone(), delete_operation),
        );
        assert!(service.try_emit(crate::ui::UiEvent::OperationFailed {
            operation_id: delete_operation,
            profile_id: profile.id.clone(),
            profile_generation: profile.generation,
            session_generation: None,
            kind: OperationKind::DeleteProfile,
            summary: error.summary,
            error,
            session_disposition: None,
            connection_outcome: crate::ui::ConnectionFailureOutcome::Preserve,
        }));
        app.poll_events();

        assert_eq!(app.active_operations.get(&profile.id), Some(&prior));
        app.open_delete_confirmation(&profile);
        assert_eq!(
            app.delete_confirmation
                .as_ref()
                .and_then(|confirmation| confirmation.active_kind),
            Some(OperationKind::ExecuteMutation)
        );
    }

    #[test]
    fn durability_unknown_delete_failure_does_not_restore_prior_active_operation() {
        let (ui_port, mut service) = bounded_ports(4);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let profile = profile(DriverKind::MySql, DriverAvailability::Ready);
        app.model.profiles = vec![profile.clone()];
        app.model
            .active_generations
            .insert(profile.id.clone(), profile.generation);
        let prior = ActiveOperation {
            operation_id: OperationId(78),
            profile_generation: profile.generation,
            kind: OperationKind::ExecuteMutation,
        };
        app.active_operations.insert(profile.id.clone(), prior);

        app.open_delete_confirmation(&profile);
        app.confirm_delete_confirmation();
        let delete_operation = match service.try_next_command() {
            Some(UiCommand::DeleteProfile(request)) => request.operation_id,
            _ => panic!("confirmed delete command"),
        };
        let error = PublicOperationError::new_or_internal(
            OperationKind::DeleteProfile,
            PublicSummary::CommittedDurabilityUnknown,
            PublicCode::None,
            &SafeContext::profile(profile.id.clone(), delete_operation),
        );
        assert!(service.try_emit(crate::ui::UiEvent::OperationFailed {
            operation_id: delete_operation,
            profile_id: profile.id.clone(),
            profile_generation: profile.generation,
            session_generation: None,
            kind: OperationKind::DeleteProfile,
            summary: error.summary,
            error,
            session_disposition: None,
            connection_outcome: crate::ui::ConnectionFailureOutcome::Preserve,
        }));
        app.poll_events();

        assert!(!app.active_operations.contains_key(&profile.id));
        assert!(!app.pending_deletes.contains_key(&profile.id));
        app.open_delete_confirmation(&profile);
        assert_eq!(
            app.delete_confirmation
                .as_ref()
                .and_then(|confirmation| confirmation.active_kind),
            None
        );
    }

    #[test]
    fn stale_active_tracking_clears_on_generation_change_uncertainty_and_shutdown() {
        let (ui_port, mut service) = bounded_ports(4);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let profile_id = ProfileId("lifecycle-prune".to_owned());
        let active = ActiveOperation {
            operation_id: OperationId(90),
            profile_generation: ProfileGeneration(1),
            kind: OperationKind::ExecuteRead,
        };
        app.model
            .active_generations
            .insert(profile_id.clone(), ProfileGeneration(2));
        app.active_operations.insert(profile_id.clone(), active);
        app.pending_deletes.insert(
            profile_id.clone(),
            PendingDelete {
                operation_id: OperationId(91),
                profile_generation: ProfileGeneration(1),
                prior_active: Some(active),
                prior_finished: false,
            },
        );
        app.prune_active_operations();
        assert!(app.active_operations.is_empty());
        assert!(app.pending_deletes.is_empty());

        let current = ActiveOperation {
            operation_id: OperationId(92),
            profile_generation: ProfileGeneration(2),
            kind: OperationKind::BrowseMySql,
        };
        app.active_operations.insert(profile_id.clone(), current);
        app.pending_deletes.insert(
            profile_id.clone(),
            PendingDelete {
                operation_id: OperationId(93),
                profile_generation: ProfileGeneration(2),
                prior_active: Some(current),
                prior_finished: false,
            },
        );
        app.finish_active_operation(&crate::ui::UiEvent::ConfigUncertain {
            operation_id: OperationId(94),
        });
        assert!(app.active_operations.is_empty());
        assert!(app.pending_deletes.is_empty());

        app.active_operations.insert(profile_id, current);
        app.finish_active_operation(&crate::ui::UiEvent::RuntimeShutdown {
            operation_id: OperationId(95),
        });
        assert!(app.active_operations.is_empty());
    }

    #[test]
    fn failed_save_connect_refresh_clears_follow_up_before_unrelated_reload() {
        let (ui_port, mut service) = bounded_ports(4);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let (_profile_id, refresh_operation) = prime_save_and_connect(&mut app, &mut service);

        let error = reload_failure(refresh_operation);
        assert!(service.try_emit(crate::ui::UiEvent::ProfilesFailed {
            operation_id: refresh_operation,
            summary: error.summary,
            error,
        }));
        app.poll_events();
        assert!(service.try_emit(crate::ui::UiEvent::ProfilesLoaded {
            operation_id: OperationId(refresh_operation.0 + 1),
            profiles: vec![profile(DriverKind::MySql, DriverAvailability::Ready)],
        }));
        app.poll_events();

        assert!(
            service.try_next_command().is_none(),
            "an unrelated later reload must not silently connect"
        );
    }

    #[test]
    fn busy_save_connect_refresh_submit_does_not_arm_a_later_reload() {
        let (ui_port, mut service) = bounded_ports(1);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());

        let mut editor = ProfileEditor::new(DraftId(402), DriverKind::MySql);
        editor.draft.name = "Profile".to_owned();
        let save_operation = app.model.next_operation();
        assert!(matches!(
            editor.try_save_with_connect(&app.port, save_operation, true),
            crate::ui::profile_form::SaveAttempt::Submitted(_)
        ));
        app.profile_editor = Some(editor);
        assert!(service.try_next_command().is_some());
        assert_eq!(
            app.port.try_submit(UiCommand::RefreshProfiles {
                operation_id: OperationId(999),
            }),
            Ok(())
        );
        assert!(service.try_emit(crate::ui::UiEvent::ProfileSaved {
            operation_id: save_operation,
            profile_id: ProfileId("profile".to_owned()),
            previous_generation: None,
            profile_generation: ProfileGeneration(1),
            session_retained: false,
            warning: None,
        }));
        app.poll_events();
        assert!(matches!(
            service.try_next_command(),
            Some(UiCommand::RefreshProfiles {
                operation_id: OperationId(999)
            })
        ));

        assert!(service.try_emit(crate::ui::UiEvent::ProfilesLoaded {
            operation_id: OperationId(1_000),
            profiles: vec![profile(DriverKind::MySql, DriverAvailability::Ready)],
        }));
        app.poll_events();
        assert!(
            service.try_next_command().is_none(),
            "a failed follow-up submit must not arm an unrelated later reload"
        );
    }

    #[test]
    fn actual_app_editor_shortcut_submits_exact_generation_and_accesskit_ids() {
        let (ui_port, mut service) = bounded_ports(4);
        let mut app = DbotterApp::new(ui_port);
        assert!(matches!(
            service.try_next_command(),
            Some(UiCommand::RefreshProfiles { .. })
        ));
        let profile = profile(DriverKind::MySql, DriverAvailability::Ready);
        let key = WorkspaceKey::new(profile.id.clone(), profile.generation);
        app.model.profiles = vec![profile];
        app.model.selected_profile = Some(ProfileId("profile".to_owned()));
        app.model
            .active_generations
            .insert(ProfileId("profile".to_owned()), ProfileGeneration(1));
        app.model.workspace_mut(key.clone()).editor_text = "SELECT 1".to_owned();

        #[cfg(target_os = "macos")]
        let modifiers = Modifiers {
            mac_cmd: true,
            command: true,
            ..Modifiers::default()
        };
        #[cfg(not(target_os = "macos"))]
        let modifiers = Modifiers {
            ctrl: true,
            command: true,
            ..Modifiers::default()
        };
        let input = RawInput {
            events: vec![Event::Key {
                key: Key::Enter,
                physical_key: Some(Key::Enter),
                pressed: true,
                repeat: false,
                modifiers,
            }],
            ..RawInput::default()
        };
        let context = Context::default();
        context.enable_accesskit();
        let output = context.run_ui(input, |ui| app.editor_and_results(ui));

        let Some(UiCommand::Execute {
            operation_id,
            profile_id,
            profile_generation,
            text,
            row_limit,
            timeout_ms,
            ..
        }) = service.try_next_command()
        else {
            panic!("actual app shortcut did not submit Execute");
        };
        assert_eq!(operation_id, OperationId(2));
        assert_eq!(profile_id, ProfileId("profile".to_owned()));
        assert_eq!(profile_generation, ProfileGeneration(1));
        assert_eq!(text, "SELECT 1");
        assert_eq!(row_limit, 500);
        assert_eq!(timeout_ms, 30_000);
        assert_eq!(
            app.model
                .workspace(&key)
                .and_then(|workspace| workspace.pending_execute),
            Some(operation_id)
        );

        let ids = output
            .platform_output
            .accesskit_update
            .expect("actual app frame must emit AccessKit")
            .nodes
            .into_iter()
            .filter_map(|(_, node)| node.author_id().map(str::to_owned))
            .collect::<BTreeSet<_>>();
        for expected in [
            "editor.target",
            "editor.input",
            "editor.row_limit",
            "editor.timeout",
            "editor.execute",
        ] {
            assert!(
                ids.contains(expected),
                "missing actual app AX id {expected}"
            );
        }
    }

    #[test]
    fn editor_submission_sets_pending_only_after_success_and_cancel_is_exact() {
        let (ui_port, mut service) = bounded_ports(1);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let profile = profile(DriverKind::MySql, DriverAvailability::Ready);
        let key = WorkspaceKey::new(profile.id.clone(), profile.generation);
        app.model.profiles = vec![profile.clone()];
        app.model.selected_profile = Some(profile.id.clone());
        app.model
            .active_generations
            .insert(profile.id.clone(), profile.generation);
        app.model.workspace_mut(key.clone()).editor_text = "SELECT 1".to_owned();
        let intent = build_execute_intent(
            &profile,
            app.model.workspace(&key).expect("workspace"),
            EditorCursor::caret(0),
        )
        .expect("execute intent");

        app.port
            .try_submit(UiCommand::TestConnection {
                operation_id: OperationId(99),
                profile_id: profile.id.clone(),
                profile_generation: profile.generation,
                timeout_ms: 1_000,
            })
            .expect("fill work lane");
        app.submit_editor_intent(EditorIntent::Execute(intent.clone()));
        assert!(
            app.model
                .workspace(&key)
                .is_some_and(|workspace| workspace.pending_execute.is_none()),
            "a Busy submit must not fabricate pending state"
        );
        assert!(matches!(
            service.try_next_command(),
            Some(UiCommand::TestConnection {
                operation_id: OperationId(99),
                ..
            })
        ));

        app.submit_editor_intent(EditorIntent::Execute(intent));
        let Some(UiCommand::Execute { operation_id, .. }) = service.try_next_command() else {
            panic!("expected exact Execute after capacity became available");
        };
        assert_eq!(
            app.model
                .workspace(&key)
                .and_then(|workspace| workspace.pending_execute),
            Some(operation_id)
        );

        app.submit_editor_intent(EditorIntent::Cancel { operation_id });
        assert!(matches!(
            service.try_next_command(),
            Some(UiCommand::CancelOperation {
                operation_id: cancelled,
            }) if cancelled == operation_id
        ));
        assert_eq!(
            app.model
                .workspace(&key)
                .and_then(|workspace| workspace.pending_execute),
            Some(operation_id),
            "Cancel submission waits for the correlated terminal event"
        );
    }

    #[test]
    fn double_execute_while_pending_submits_only_once() {
        let (ui, mut service) = bounded_ports(4);
        let mut app = DbotterApp::new(ui);
        assert!(matches!(
            service.try_next_command(),
            Some(UiCommand::RefreshProfiles { .. })
        ));
        app.model.profiles = vec![profile(DriverKind::MySql, DriverAvailability::Ready)];
        app.model.selected_profile = Some(ProfileId("profile".to_owned()));
        app.model
            .active_generations
            .insert(ProfileId("profile".to_owned()), ProfileGeneration(1));
        app.model
            .workspace_mut(WorkspaceKey::new(
                ProfileId("profile".to_owned()),
                ProfileGeneration(1),
            ))
            .editor_text = "SELECT 1".to_owned();

        let profile = app
            .model
            .selected_profile_snapshot()
            .cloned()
            .expect("selected profile");
        let intent = build_execute_intent(
            &profile,
            app.model
                .workspace(&WorkspaceKey::new(profile.id.clone(), profile.generation))
                .expect("workspace"),
            EditorCursor::caret(0),
        )
        .expect("execute intent");
        app.submit_editor_intent(EditorIntent::Execute(intent.clone()));
        app.submit_editor_intent(EditorIntent::Execute(intent));

        assert!(matches!(
            service.try_next_command(),
            Some(UiCommand::Execute { .. })
        ));
        assert!(service.try_next_command().is_none());
    }

    #[test]
    fn mongodb_planned_profile_submits_no_command() {
        let (ui, mut service) = bounded_ports(4);
        let mut app = DbotterApp::new(ui);
        assert!(service.try_next_command().is_some());
        app.model.profiles = vec![profile(DriverKind::MongoDb, DriverAvailability::Planned)];
        app.model.selected_profile = Some(ProfileId("profile".to_owned()));
        app.model
            .workspace_mut(WorkspaceKey::new(
                ProfileId("profile".to_owned()),
                ProfileGeneration(1),
            ))
            .editor_text = "{}".to_owned();

        let profile = app
            .model
            .selected_profile_snapshot()
            .cloned()
            .expect("selected profile");
        assert!(
            build_execute_intent(
                &profile,
                app.model
                    .workspace(&WorkspaceKey::new(profile.id.clone(), profile.generation,))
                    .expect("workspace"),
                EditorCursor::caret(0),
            )
            .is_err()
        );

        assert!(service.try_next_command().is_none());
    }

    #[test]
    fn config_uncertain_submits_neither_profile_network_work_nor_execute() {
        let (ui, mut service) = bounded_ports(4);
        let mut app = DbotterApp::new(ui);
        assert!(service.try_next_command().is_some());
        app.model.profiles = vec![profile(DriverKind::MySql, DriverAvailability::Ready)];
        app.model.selected_profile = Some(ProfileId("profile".to_owned()));
        app.model
            .active_generations
            .insert(ProfileId("profile".to_owned()), ProfileGeneration(1));
        app.model
            .workspace_mut(WorkspaceKey::new(
                ProfileId("profile".to_owned()),
                ProfileGeneration(1),
            ))
            .editor_text = "SELECT 1".to_owned();
        let profile = app
            .model
            .selected_profile_snapshot()
            .cloned()
            .expect("selected profile");
        let intent = build_execute_intent(
            &profile,
            app.model
                .workspace(&WorkspaceKey::new(profile.id.clone(), profile.generation))
                .expect("workspace"),
            EditorCursor::caret(0),
        )
        .expect("execute intent");
        app.model.fold(crate::ui::UiEvent::ConfigUncertain {
            operation_id: crate::model::OperationId(10),
        });

        app.submit_test(ProfileId("profile".to_owned()));
        app.submit_editor_intent(EditorIntent::Execute(intent));

        assert!(
            service.try_next_command().is_none(),
            "configuration uncertainty must block test and execute at the UI boundary"
        );
    }

    #[test]
    fn draft_ids_are_owned_by_the_app_and_monotonic() {
        let (ui, mut service) = bounded_ports(1);
        let mut app = DbotterApp::new(ui);
        assert!(service.try_next_command().is_some());

        assert_eq!(app.allocate_draft_id(), DraftId(1));
        assert_eq!(app.allocate_draft_id(), DraftId(2));
        assert_eq!(app.allocate_draft_id(), DraftId(3));
    }

    #[test]
    fn redis_scan_intent_submits_exact_profile_generation_filter_and_cursor() {
        let (ui, mut service) = bounded_ports(4);
        let mut app = DbotterApp::new(ui);
        assert!(service.try_next_command().is_some());
        app.model.profiles = vec![profile(DriverKind::Redis, DriverAvailability::Ready)];
        app.model.selected_profile = Some(ProfileId("profile".to_owned()));

        app.submit_redis_intent(RedisExplorerIntent::Scan {
            filter: RedisKeyFilter::LiteralPrefix("orders:[".to_owned()),
            cursor: 41,
            restart: false,
        });

        let Some(UiCommand::ScanRedisKeys(request)) = service.try_next_command() else {
            panic!("expected one Redis SCAN command");
        };
        assert_eq!(request.identity.profile_id, ProfileId("profile".to_owned()));
        assert_eq!(request.identity.profile_generation, ProfileGeneration(1));
        assert_eq!(
            request.filter,
            RedisKeyFilter::LiteralPrefix("orders:[".to_owned())
        );
        assert_eq!(request.cursor, 41);
        assert_eq!(request.count_hint, crate::model::DEFAULT_REDIS_SCAN_COUNT);
        assert!(service.try_next_command().is_none());
    }

    #[test]
    fn redis_tls_recovery_focuses_only_the_typed_field_and_preserves_the_same_ca() {
        let (ui, mut service) = bounded_ports(4);
        let mut app = DbotterApp::new(ui);
        assert!(service.try_next_command().is_some());
        let mut redis = profile(DriverKind::Redis, DriverAvailability::Ready);
        redis.persisted.tls = TlsMode::Required;
        redis.persisted.redis_tls.ca_file = Some("/tmp/same-ca.pem".into());
        app.model.profiles = vec![redis];

        app.submit_redis_intent(RedisExplorerIntent::EditProfileField {
            profile_id: ProfileId("profile".to_owned()),
            field: ProfileFieldId::Host,
        });
        let host_editor = app.profile_editor.as_ref().expect("host editor");
        assert_eq!(host_editor.requested_focus(), Some(ProfileFieldId::Host));
        assert_eq!(host_editor.draft.redis_ca_file, "/tmp/same-ca.pem");

        app.profile_editor = None;
        app.submit_redis_intent(RedisExplorerIntent::EditProfileField {
            profile_id: ProfileId("profile".to_owned()),
            field: ProfileFieldId::RedisCaFile,
        });
        let ca_editor = app.profile_editor.as_ref().expect("CA editor");
        assert_eq!(
            ca_editor.requested_focus(),
            Some(ProfileFieldId::RedisCaFile)
        );
        assert_eq!(ca_editor.draft.redis_ca_file, "/tmp/same-ca.pem");
        assert!(service.try_next_command().is_none());
    }

    #[test]
    fn redis_inspect_and_cancel_commands_preserve_raw_identity_and_operation_id() {
        let (ui, mut service) = bounded_ports(4);
        let mut app = DbotterApp::new(ui);
        assert!(service.try_next_command().is_some());
        app.model.profiles = vec![profile(DriverKind::Redis, DriverAvailability::Ready)];
        app.model.selected_profile = Some(ProfileId("profile".to_owned()));
        let raw_key = RedisKeyId(vec![b'b', 0, 0xff, b'k']);

        app.submit_redis_intent(RedisExplorerIntent::Inspect {
            key: raw_key.clone(),
        });
        let Some(UiCommand::InspectRedisKey(request)) = service.try_next_command() else {
            panic!("expected one Redis inspect command");
        };
        assert_eq!(request.key, raw_key);
        let operation_id = request.identity.operation_id;

        app.submit_redis_intent(RedisExplorerIntent::Cancel { operation_id });
        assert!(matches!(
            service.try_next_command(),
            Some(UiCommand::CancelOperation {
                operation_id: submitted
            }) if submitted == operation_id
        ));
        assert!(service.try_next_command().is_none());
    }

    #[test]
    fn redis_browse_is_blocked_for_unready_profile_without_consuming_operation() {
        let (ui, mut service) = bounded_ports(4);
        let mut app = DbotterApp::new(ui);
        assert!(service.try_next_command().is_some());
        app.model.profiles = vec![profile(DriverKind::Redis, DriverAvailability::Planned)];
        app.model.selected_profile = Some(ProfileId("profile".to_owned()));

        app.submit_redis_intent(RedisExplorerIntent::Scan {
            filter: RedisKeyFilter::Glob("*".to_owned()),
            cursor: 0,
            restart: true,
        });

        assert!(service.try_next_command().is_none());
        assert_eq!(app.model.next_operation(), OperationId(2));
    }
}
