//! Native three-zone UI. Rendering and state folding perform no I/O.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use eframe::egui;
use egui_extras::{Column as TableColumn, TableBuilder};

use crate::model::{
    CatalogRequest, Cell, DEFAULT_CATALOG_PAGE_SIZE, DEFAULT_CATALOG_TIMEOUT,
    DEFAULT_REDIS_SCAN_COUNT, DraftId, DriverAvailability, DriverCapabilities, DriverKind,
    ProfileFieldId, ProfileGeneration, ProfileId, RedisKeyInspectRequest, RedisScanRequest,
    RequestIdentity,
};

use super::adapter::{SubmitError, UiCommand, UiPort};
use super::editor::{EditorIntent, EditorSurface};
use super::model::{ConnectionState, ProfileSnapshot, UiEvent, UiModel};
use super::mysql_explorer::{MySqlExplorerIntent, MySqlExplorerState};
use super::profile_form::{FormAction, ProfileEditor, ProfileEventResult, SaveAttempt};
use super::redis_explorer::{RedisExplorer, RedisExplorerIntent};

const EVENT_DRAIN_LIMIT: usize = 128;
pub const DEFAULT_EXECUTE_ROW_LIMIT: u32 = 500;
const DEFAULT_TIMEOUT_MS: u64 = 30_000;

pub struct DbotterApp {
    port: UiPort,
    model: UiModel,
    mysql_explorers: HashMap<(ProfileId, ProfileGeneration), MySqlExplorerState>,
    profile_editor: Option<ProfileEditor>,
    editor_surface: EditorSurface,
    redis_explorer: RedisExplorer,
    next_draft_id: u64,
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
            next_draft_id: 1,
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
                ProfileEventResult::Failed => {
                    if let Some(editor) = &self.profile_editor {
                        self.model.status = editor.status().to_owned();
                    }
                    continue;
                }
                ProfileEventResult::Ignored => {}
            }
            self.model.fold(event);
        }
        self.mysql_explorers.retain(|(profile_id, generation), _| {
            self.model.active_generation(profile_id) == Some(*generation)
        });
        if let Some(editor) = self.profile_editor.as_mut() {
            editor.set_config_uncertain(self.model.is_config_uncertain());
        }
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
        else {
            self.model.status = "Unknown profile".to_owned();
            return;
        };
        if !profile.is_ready() {
            self.model.status = "Driver is planned and unavailable".to_owned();
            return;
        }
        if self.model.connection_state(&profile_id).is_pending() {
            self.model.status = "Connection test is already pending".to_owned();
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
                    .insert(profile_id, ConnectionState::Pending(operation_id));
                self.model.status = "Testing connection…".to_owned();
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
                let workspace_key = super::model::WorkspaceKey::new(
                    intent.profile_id().clone(),
                    intent.profile_generation(),
                );
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
                let operation_id = self.model.next_operation();
                match self.port.try_submit(intent.into_ui_command(operation_id)) {
                    Ok(()) => {
                        self.model.workspace_mut(workspace_key).pending_execute =
                            Some(operation_id);
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

    fn connections(&mut self, root_ui: &mut egui::Ui) {
        egui::Panel::left("connections")
            .resizable(true)
            .default_size(360.0)
            .show(root_ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.heading("Connections");
                    let actions_enabled = !self.model.is_config_uncertain();
                    if ui
                        .add_enabled(actions_enabled, egui::Button::new("+ MySQL").small())
                        .clicked()
                    {
                        let draft_id = self.allocate_draft_id();
                        self.profile_editor = Some(ProfileEditor::new(draft_id, DriverKind::MySql));
                    }
                    if ui
                        .add_enabled(actions_enabled, egui::Button::new("+ Redis").small())
                        .clicked()
                    {
                        let draft_id = self.allocate_draft_id();
                        self.profile_editor = Some(ProfileEditor::new(draft_id, DriverKind::Redis));
                    }
                    if ui
                        .add_enabled(actions_enabled, egui::Button::new("+ MongoDB").small())
                        .clicked()
                    {
                        let draft_id = self.allocate_draft_id();
                        self.profile_editor =
                            Some(ProfileEditor::new(draft_id, DriverKind::MongoDb));
                    }
                    if ui.small_button("Reload").clicked() {
                        self.submit_refresh();
                    }
                });
                ui.separator();
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for profile in self.model.profiles.clone() {
                        self.profile_card(ui, &profile);
                        ui.add_space(8.0);
                    }
                });
            });
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
        ui.horizontal(|ui| {
            ui.label(connection_label(&state));
            if ui
                .add_enabled(
                    actions_enabled && profile.is_ready() && !state.is_pending(),
                    egui::Button::new("Test"),
                )
                .clicked()
            {
                self.submit_test(profile.id.clone());
            }
            if ui
                .add_enabled(actions_enabled, egui::Button::new("Edit"))
                .clicked()
            {
                let draft_id = self.allocate_draft_id();
                self.profile_editor = Some(ProfileEditor::edit(
                    draft_id,
                    &profile.persisted,
                    profile.generation,
                    profile.has_current_session_secret,
                ));
            }
        });
        if profile.availability == DriverAvailability::Planned {
            ui.colored_label(
                egui::Color32::YELLOW,
                format!(
                    "Planned: {}",
                    profile.planned_reason.as_deref().unwrap_or("not available")
                ),
            );
        }
        if selected && profile.driver == DriverKind::MySql && profile.is_ready() {
            ui.add_space(12.0);
            let intents = self
                .mysql_explorers
                .entry((profile.id.clone(), profile.generation))
                .or_default()
                .show(ui);
            for intent in intents {
                self.submit_mysql_explorer_intent(profile, intent);
            }
        } else if profile.driver == DriverKind::Redis && profile.is_ready() {
            ui.weak("Keyspace browser ready · SCAN semantics");
        } else {
            ui.weak("Resource browser availability follows driver capabilities");
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
                FormAction::Save => {
                    let operation_id = self.model.next_operation();
                    if let Some(editor) = self.profile_editor.as_mut() {
                        match editor.try_save(&self.port, operation_id) {
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
        let mut redis_intent = None;
        let mut editor_intent = None;
        egui::CentralPanel::default().show(root_ui, |ui| {
            let selected_redis = self
                .model
                .selected_profile_snapshot()
                .filter(|profile| profile.driver == DriverKind::Redis)
                .map(|profile| (profile.id.clone(), profile.generation));
            self.redis_explorer.set_profile(selected_redis.clone());
            if selected_redis.is_some() {
                redis_intent = self
                    .redis_explorer
                    .show(ui, !self.model.is_config_uncertain());
                ui.add_space(16.0);
            }
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
        if let Some(intent) = redis_intent {
            self.submit_redis_intent(intent);
        }
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
        self.connections(ui);
        self.editor_and_results(ui);
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
            ui.colored_label(egui::Color32::YELLOW, "truncated");
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
                            ui.colored_label(egui::Color32::RED, "<missing>");
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

    use super::DbotterApp;
    use crate::model::{
        ConnectionProfile, CredentialMode, DraftId, DriverAvailability, DriverKind, OperationId,
        ProfileFieldId, ProfileGeneration, ProfileId, RedisKeyFilter, RedisKeyId, RedisTlsConfig,
        TlsMode,
    };
    use crate::ui::adapter::{UiCommand, bounded_ports};
    use crate::ui::editor::{EditorCursor, EditorIntent, build_execute_intent};
    use crate::ui::model::{ProfileSnapshot, WorkspaceKey};
    use crate::ui::redis_explorer::RedisExplorerIntent;
    use eframe::egui::{Context, Event, Key, Modifiers, RawInput};

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
