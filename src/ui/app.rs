//! Native three-zone UI. Rendering and state folding perform no I/O.

use std::time::Duration;

use eframe::egui;
use egui_extras::{Column as TableColumn, TableBuilder};

use crate::model::{Cell, DraftId, DriverAvailability, DriverKind, ProfileId};

use super::adapter::{SubmitError, UiCommand, UiPort};
use super::model::{ConnectionState, ProfileSnapshot, UiModel};
use super::profile_form::{FormAction, ProfileEditor, ProfileEventResult, SaveAttempt};

const EVENT_DRAIN_LIMIT: usize = 128;
const DEFAULT_ROW_LIMIT: u32 = 1_000;
const DEFAULT_TIMEOUT_MS: u64 = 30_000;

pub struct DbotterApp {
    port: UiPort,
    model: UiModel,
    profile_editor: Option<ProfileEditor>,
    next_draft_id: u64,
}

impl DbotterApp {
    pub fn new(port: UiPort) -> Self {
        let mut app = Self {
            port,
            model: UiModel::default(),
            profile_editor: None,
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
        if let Some(editor) = self.profile_editor.as_mut() {
            editor.set_config_uncertain(self.model.is_config_uncertain());
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

    fn submit_execute(&mut self) {
        if self.model.is_config_uncertain() {
            self.model.status = "Reload profiles before executing.".to_owned();
            return;
        }
        if self.model.pending_execute.is_some() {
            self.model.status = "Execute is already pending".to_owned();
            return;
        }
        let Some(profile) = self.model.selected_profile_snapshot().cloned() else {
            self.model.status = "Select a connection profile".to_owned();
            return;
        };
        if !profile.is_ready() {
            self.model.status = "MongoDB is planned; execute is disabled".to_owned();
            return;
        }
        let text = self.model.editor_text.trim().to_owned();
        if text.is_empty() {
            self.model.status = "Enter a statement or command".to_owned();
            return;
        }
        let operation_id = self.model.next_operation();
        let profile_id = profile.id;
        let profile_generation = profile.generation;
        match self.port.try_submit(UiCommand::Execute {
            operation_id,
            profile_id: profile_id.clone(),
            profile_generation,
            language: profile.driver.language(),
            text,
            row_limit: DEFAULT_ROW_LIMIT,
            timeout_ms: DEFAULT_TIMEOUT_MS,
        }) {
            Ok(()) => {
                self.model.pending_execute = Some((operation_id, profile_id, profile_generation));
                self.model.status = "Executing…".to_owned();
            }
            Err(error) => self.report_submit_error(error),
        }
    }

    fn report_submit_error(&mut self, error: SubmitError) {
        self.model.status = match error {
            SubmitError::Busy => "Service is busy; command was not submitted".to_owned(),
            SubmitError::Disconnected => "Service is unavailable".to_owned(),
        };
    }

    fn connections(&mut self, root_ui: &mut egui::Ui) {
        egui::Panel::left("connections")
            .resizable(true)
            .default_size(280.0)
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
        ui.weak("Catalog browsing is deferred in this MVP");
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
                FormAction::None => {}
            }
            return;
        }
        egui::CentralPanel::default().show(root_ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading("Editor");
                if let Some(profile) = self.model.selected_profile_snapshot() {
                    ui.label(format!(
                        "{} · {:?}",
                        profile.name,
                        profile.driver.language()
                    ));
                }
            });
            ui.add_enabled(
                !self.model.is_config_uncertain(),
                egui::TextEdit::multiline(&mut self.model.editor_text)
                    .code_editor()
                    .desired_rows(10)
                    .desired_width(f32::INFINITY)
                    .hint_text("SELECT 1  or  PING"),
            );
            let execute_enabled = self
                .model
                .selected_profile_snapshot()
                .is_some_and(ProfileSnapshot::is_ready)
                && self.model.pending_execute.is_none()
                && !self.model.is_config_uncertain();
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(execute_enabled, egui::Button::new("Execute"))
                    .clicked()
                {
                    self.submit_execute();
                }
                if let Some((operation_id, _, _)) = &self.model.pending_execute {
                    ui.spinner();
                    ui.label(format!("operation {}", operation_id.0));
                }
                ui.separator();
                ui.label(&self.model.status);
            });
            ui.separator();
            ui.heading("Results");
            if let Some(result) = &self.model.result {
                render_result(ui, result);
            } else {
                ui.weak("No result yet");
            }
        });
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
    use super::DbotterApp;
    use crate::model::{
        ConnectionProfile, CredentialMode, DraftId, DriverAvailability, DriverKind,
        ProfileGeneration, ProfileId, RedisTlsConfig, TlsMode,
    };
    use crate::ui::adapter::{UiCommand, bounded_ports};
    use crate::ui::model::ProfileSnapshot;

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
    fn double_execute_while_pending_submits_only_once() {
        let (ui, mut service) = bounded_ports(4);
        let mut app = DbotterApp::new(ui);
        assert!(matches!(
            service.try_next_command(),
            Some(UiCommand::RefreshProfiles { .. })
        ));
        app.model.profiles = vec![profile(DriverKind::MySql, DriverAvailability::Ready)];
        app.model.selected_profile = Some(ProfileId("profile".to_owned()));
        app.model.editor_text = "SELECT 1".to_owned();

        app.submit_execute();
        app.submit_execute();

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
        app.model.editor_text = "{}".to_owned();

        app.submit_execute();

        assert!(service.try_next_command().is_none());
    }

    #[test]
    fn config_uncertain_submits_neither_profile_network_work_nor_execute() {
        let (ui, mut service) = bounded_ports(4);
        let mut app = DbotterApp::new(ui);
        assert!(service.try_next_command().is_some());
        app.model.profiles = vec![profile(DriverKind::MySql, DriverAvailability::Ready)];
        app.model.selected_profile = Some(ProfileId("profile".to_owned()));
        app.model.editor_text = "SELECT 1".to_owned();
        app.model.fold(crate::ui::UiEvent::ConfigUncertain {
            operation_id: crate::model::OperationId(10),
        });

        app.submit_test(ProfileId("profile".to_owned()));
        app.submit_execute();

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
}
