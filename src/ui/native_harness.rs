use eframe::egui;

use super::accessibility::named_author_id;
use super::theme::OpenAiTheme;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HarnessSurface {
    FirstRun,
    Inventory,
}

/// A deterministic `RawInput` surface that uses the same author-id helpers as
/// the native app. It contains no driver or service mock.
pub struct NativeUiHarness {
    surface: HarnessSurface,
    connection_id: String,
    host: String,
    ca_file: String,
    editor_target: String,
    row_limit: String,
    timeout: String,
}

impl NativeUiHarness {
    pub fn first_run() -> Self {
        Self::new(HarnessSurface::FirstRun)
    }

    pub fn p6_inventory() -> Self {
        Self::new(HarnessSurface::Inventory)
    }

    fn new(surface: HarnessSurface) -> Self {
        Self {
            surface,
            connection_id: "local".to_owned(),
            host: "localhost".to_owned(),
            ca_file: "/tmp/dbotter-ca.pem".to_owned(),
            editor_target: "mysql-local · mysql · localhost:3306".to_owned(),
            row_limit: "500".to_owned(),
            timeout: "30".to_owned(),
        }
    }

    pub fn show(&mut self, ui: &mut egui::Ui) {
        OpenAiTheme::apply(ui.ctx());
        match self.surface {
            HarnessSurface::FirstRun => show_first_run(ui),
            HarnessSurface::Inventory => self.show_inventory(ui),
        }
    }

    fn show_inventory(&mut self, ui: &mut egui::Ui) {
        ui.heading("Connection details");
        let connection_id = ui.add(
            egui::TextEdit::singleline(&mut self.connection_id).id_source("profile.connection_id"),
        );
        named_author_id(connection_id, "profile.connection_id", "Connection id");
        let host = ui.add(egui::TextEdit::singleline(&mut self.host).id_source("profile.host"));
        named_author_id(host, "profile.host", "Host");
        let ca_file = ui.add(
            egui::TextEdit::singleline(&mut self.ca_file).id_source("profile.redis_tls.ca_file"),
        );
        named_author_id(ca_file, "profile.redis_tls.ca_file", "Redis CA file");
        named_author_id(
            ui.button("Choose CA file"),
            "profile.redis_tls.ca_file.pick",
            "Choose Redis CA file",
        );
        named_author_id(
            ui.add_enabled(false, egui::RadioButton::new(false, "Keep current · set")),
            "profile.credential.session.keep",
            "Keep current session credential",
        );
        named_author_id(
            ui.radio(false, "Replace"),
            "profile.credential.session.replace",
            "Replace session credential",
        );
        named_author_id(
            ui.radio(false, "Forget"),
            "profile.credential.session.forget",
            "Forget session credential",
        );
        let target = ui.add_enabled(
            false,
            egui::TextEdit::singleline(&mut self.editor_target).id_source("editor.target"),
        );
        named_author_id(target, "editor.target", "Execution target");
        let row_limit =
            ui.add(egui::TextEdit::singleline(&mut self.row_limit).id_source("editor.row_limit"));
        named_author_id(row_limit, "editor.row_limit", "Execute row limit");
        let timeout =
            ui.add(egui::TextEdit::singleline(&mut self.timeout).id_source("editor.timeout"));
        named_author_id(timeout, "editor.timeout", "Execute timeout seconds");
        named_author_id(
            ui.label("ExecuteRead is active. Dbotter will stop waiting; the server operation may continue."),
            "profile.delete.active_warning",
            "Active operation delete warning",
        );
    }
}

pub(crate) fn show_first_run(ui: &mut egui::Ui) {
    ui.vertical_centered(|ui| {
        ui.add_space(48.0);
        ui.heading("Connect your first database");
        ui.label("Create a local profile. Credentials remain outside the saved profile.");
        ui.add_space(24.0);
        let primary = ui.add(
            egui::Button::new(egui::RichText::new("New connection").color(egui::Color32::WHITE))
                .fill(egui::Color32::BLACK),
        );
        named_author_id(primary, "connection.new", "New connection");
        named_author_id(
            ui.button("New MySQL"),
            "connection.new.mysql",
            "New MySQL connection",
        );
        named_author_id(
            ui.button("New Redis"),
            "connection.new.redis",
            "New Redis connection",
        );
        named_author_id(
            ui.add_enabled(false, egui::Button::new("MongoDB · Planned")),
            "connection.mongodb.planned",
            "MongoDB planned and unavailable",
        );
        ui.add_space(24.0);
        ui.label("Credential sources: None · This app session · Environment variable");
    });
}
