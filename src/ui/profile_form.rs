//! Pure profile draft validation plus egui rendering and save correlation.

use eframe::egui;

use crate::model::{ConnectionProfile, DriverKind, OperationId, ProfileId, TlsMode};

use super::adapter::{SubmitError, UiCommand, UiPort};
use super::model::UiEvent;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum EditorMode {
    Add,
    Edit { original_id: ProfileId },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ProfileDraft {
    pub id: String,
    pub name: String,
    pub driver: DriverKind,
    pub host: String,
    pub port: String,
    pub database: String,
    pub username: String,
    pub tls: TlsMode,
    pub secret_env: String,
}

impl ProfileDraft {
    pub fn new(driver: DriverKind) -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            driver,
            host: "127.0.0.1".to_owned(),
            port: default_port(driver).to_string(),
            database: String::new(),
            username: String::new(),
            tls: TlsMode::Preferred,
            secret_env: String::new(),
        }
    }

    pub fn from_profile(profile: &ConnectionProfile) -> Self {
        Self {
            id: profile.id.clone(),
            name: profile.name.clone(),
            driver: profile.driver,
            host: profile.host.clone(),
            port: profile.port.to_string(),
            database: profile.database.clone().unwrap_or_default(),
            username: profile.username.clone().unwrap_or_default(),
            tls: profile.tls,
            secret_env: profile.secret_env.clone().unwrap_or_default(),
        }
    }

    pub fn select_driver(&mut self, driver: DriverKind) {
        let previous_default = default_port(self.driver).to_string();
        if self.port.trim().is_empty() || self.port == previous_default {
            self.port = default_port(driver).to_string();
        }
        self.driver = driver;
    }

    pub fn validate(&self) -> Result<ConnectionProfile, Box<ValidationErrors>> {
        let mut errors = ValidationErrors::default();
        let id = self.id.trim();
        let name = self.name.trim();
        let host = self.host.trim();
        if id.is_empty() {
            errors.id = Some("Profile id is required".to_owned());
        } else if !valid_profile_id(id) {
            errors.id = Some(
                "Use letters, digits, dot, underscore, or hyphen; start with a letter or digit"
                    .to_owned(),
            );
        }
        if name.is_empty() {
            errors.name = Some("Display name is required".to_owned());
        }
        if host.is_empty() {
            errors.host = Some("Host is required".to_owned());
        }
        let port = match self.port.trim().parse::<u16>() {
            Ok(0) | Err(_) => {
                errors.port = Some("Port must be between 1 and 65535".to_owned());
                None
            }
            Ok(port) => Some(port),
        };
        let database = optional_trimmed(&self.database);
        if self.driver == DriverKind::Redis
            && let Some(database) = database.as_deref()
            && database.parse::<u32>().is_err()
        {
            errors.database = Some("Redis database must be a non-negative integer".to_owned());
        }
        let secret_env = optional_trimmed(&self.secret_env);
        if let Some(secret_env) = secret_env.as_deref()
            && !valid_env_name(secret_env)
        {
            errors.secret_env = Some("Use a valid environment variable name".to_owned());
        }
        if !errors.is_empty() {
            return Err(Box::new(errors));
        }
        let Some(port) = port else {
            return Err(Box::new(errors));
        };
        Ok(ConnectionProfile {
            id: id.to_owned(),
            name: name.to_owned(),
            driver: self.driver,
            host: host.to_owned(),
            port,
            database,
            username: optional_trimmed(&self.username),
            tls: self.tls,
            secret_env,
        })
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct ValidationErrors {
    pub id: Option<String>,
    pub name: Option<String>,
    pub host: Option<String>,
    pub port: Option<String>,
    pub database: Option<String>,
    pub secret_env: Option<String>,
}

impl ValidationErrors {
    fn is_empty(&self) -> bool {
        self.id.is_none()
            && self.name.is_none()
            && self.host.is_none()
            && self.port.is_none()
            && self.database.is_none()
            && self.secret_env.is_none()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FormAction {
    None,
    Save,
    Cancel,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum SaveAttempt {
    Submitted(OperationId),
    Invalid,
    Busy,
    Disconnected,
    AlreadyPending(OperationId),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ProfileEventResult {
    Ignored,
    Saved(ProfileId),
    Failed,
}

pub(super) struct ProfileEditor {
    pub mode: EditorMode,
    pub draft: ProfileDraft,
    pub errors: ValidationErrors,
    status: String,
    pending_save: Option<(OperationId, ProfileId)>,
}

impl ProfileEditor {
    pub fn new(driver: DriverKind) -> Self {
        Self {
            mode: EditorMode::Add,
            draft: ProfileDraft::new(driver),
            errors: ValidationErrors::default(),
            status: "New profile".to_owned(),
            pending_save: None,
        }
    }

    pub fn edit(profile: &ConnectionProfile) -> Self {
        Self {
            mode: EditorMode::Edit {
                original_id: ProfileId(profile.id.clone()),
            },
            draft: ProfileDraft::from_profile(profile),
            errors: ValidationErrors::default(),
            status: "Editing profile".to_owned(),
            pending_save: None,
        }
    }

    pub fn pending_operation(&self) -> Option<OperationId> {
        self.pending_save
            .as_ref()
            .map(|(operation_id, _)| *operation_id)
    }

    pub fn actions_enabled(&self) -> bool {
        self.draft.driver != DriverKind::MongoDb && self.pending_save.is_none()
    }

    pub fn status(&self) -> &str {
        &self.status
    }

    pub fn try_save(&mut self, port: &UiPort, operation_id: OperationId) -> SaveAttempt {
        if let Some((pending, _)) = self.pending_save {
            self.status = "Save is already pending".to_owned();
            return SaveAttempt::AlreadyPending(pending);
        }
        let profile = match self.draft.validate() {
            Ok(profile) => profile,
            Err(errors) => {
                self.errors = *errors;
                self.status = "Fix the highlighted fields".to_owned();
                return SaveAttempt::Invalid;
            }
        };
        if let EditorMode::Edit { original_id } = &self.mode
            && profile.id != original_id.0
        {
            self.errors.id = Some("Profile id cannot change while editing".to_owned());
            self.status = "Fix the highlighted fields".to_owned();
            return SaveAttempt::Invalid;
        }
        let profile_id = ProfileId(profile.id.clone());
        match port.try_submit(UiCommand::UpsertProfile {
            operation_id,
            profile,
        }) {
            Ok(()) => {
                self.errors = ValidationErrors::default();
                self.pending_save = Some((operation_id, profile_id));
                self.status = "Saving profile…".to_owned();
                SaveAttempt::Submitted(operation_id)
            }
            Err(SubmitError::Busy) => {
                self.status = "Service is busy; profile was not submitted".to_owned();
                SaveAttempt::Busy
            }
            Err(SubmitError::Disconnected) => {
                self.status = "Service is unavailable".to_owned();
                SaveAttempt::Disconnected
            }
        }
    }

    pub fn handle_event(&mut self, event: &UiEvent) -> ProfileEventResult {
        match event {
            UiEvent::ProfileSaved {
                operation_id,
                profile_id,
            } if self.pending_save.as_ref() == Some(&(*operation_id, profile_id.clone())) => {
                self.pending_save = None;
                self.status = "Profile saved".to_owned();
                ProfileEventResult::Saved(profile_id.clone())
            }
            UiEvent::ProfileSaveFailed {
                operation_id,
                profile_id,
                message,
            } if self.pending_save.as_ref() == Some(&(*operation_id, profile_id.clone())) => {
                self.pending_save = None;
                self.status = message.clone();
                ProfileEventResult::Failed
            }
            _ => ProfileEventResult::Ignored,
        }
    }

    pub fn show(&mut self, ui: &mut egui::Ui) -> FormAction {
        ui.heading(match self.mode {
            EditorMode::Add => "Add connection profile",
            EditorMode::Edit { .. } => "Edit connection profile",
        });
        ui.separator();
        ui.label("Driver");
        let mut selected_driver = self.draft.driver;
        egui::ComboBox::from_id_salt("profile-driver")
            .selected_text(driver_name(self.draft.driver))
            .show_ui(ui, |ui| {
                for driver in [DriverKind::MySql, DriverKind::Redis, DriverKind::MongoDb] {
                    ui.selectable_value(&mut selected_driver, driver, driver_name(driver));
                }
            });
        if self.draft.driver != selected_driver {
            self.draft.select_driver(selected_driver);
        }
        let id_editable = matches!(self.mode, EditorMode::Add);
        ui.label("Profile id");
        ui.add_enabled(id_editable, egui::TextEdit::singleline(&mut self.draft.id));
        render_error(ui, self.errors.id.as_deref());
        text_field(
            ui,
            "Display name",
            &mut self.draft.name,
            self.errors.name.as_deref(),
        );
        text_field(
            ui,
            "Host",
            &mut self.draft.host,
            self.errors.host.as_deref(),
        );
        text_field(
            ui,
            "Port",
            &mut self.draft.port,
            self.errors.port.as_deref(),
        );
        text_field(
            ui,
            if self.draft.driver == DriverKind::Redis {
                "Database number (optional)"
            } else {
                "Database (optional)"
            },
            &mut self.draft.database,
            self.errors.database.as_deref(),
        );
        text_field(ui, "Username (optional)", &mut self.draft.username, None);
        ui.label("TLS");
        egui::ComboBox::from_id_salt("profile-tls")
            .selected_text(tls_name(self.draft.tls))
            .show_ui(ui, |ui| {
                for tls in [TlsMode::Disabled, TlsMode::Preferred, TlsMode::Required] {
                    ui.selectable_value(&mut self.draft.tls, tls, tls_name(tls));
                }
            });
        text_field(
            ui,
            "Secret environment variable (optional)",
            &mut self.draft.secret_env,
            self.errors.secret_env.as_deref(),
        );
        ui.small("Only the environment-variable name is persisted. Password entry is deferred.");
        if self.draft.driver == DriverKind::MongoDb && !self.actions_enabled() {
            ui.colored_label(
                egui::Color32::YELLOW,
                "MongoDB is planned. Save is available; Test and Execute are disabled.",
            );
        }
        ui.separator();
        ui.horizontal(|ui| {
            let save = ui
                .add_enabled(
                    self.pending_operation().is_none(),
                    egui::Button::new("Save"),
                )
                .clicked();
            let cancel = ui
                .add_enabled(
                    self.pending_operation().is_none(),
                    egui::Button::new("Cancel"),
                )
                .clicked();
            if self.pending_save.is_some() {
                ui.spinner();
            }
            ui.label(&self.status);
            if save {
                FormAction::Save
            } else if cancel {
                FormAction::Cancel
            } else {
                FormAction::None
            }
        })
        .inner
    }
}

fn default_port(driver: DriverKind) -> u16 {
    crate::drivers::descriptors()
        .into_iter()
        .find(|descriptor| descriptor.kind == driver)
        .map_or(
            match driver {
                DriverKind::MySql => 3306,
                DriverKind::Redis => 6379,
                DriverKind::MongoDb => 27017,
            },
            |descriptor| descriptor.default_port,
        )
}

fn optional_trimmed(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

fn valid_profile_id(value: &str) -> bool {
    let mut characters = value.chars();
    characters
        .next()
        .is_some_and(|character| character.is_ascii_alphanumeric())
        && characters.all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-')
        })
}

fn valid_env_name(value: &str) -> bool {
    let mut characters = value.chars();
    characters
        .next()
        .is_some_and(|character| character.is_ascii_alphabetic() || character == '_')
        && characters.all(|character| character.is_ascii_alphanumeric() || character == '_')
}

fn driver_name(driver: DriverKind) -> &'static str {
    match driver {
        DriverKind::MySql => "MySQL",
        DriverKind::Redis => "Redis",
        DriverKind::MongoDb => "MongoDB",
    }
}

fn tls_name(tls: TlsMode) -> &'static str {
    match tls {
        TlsMode::Disabled => "Disabled",
        TlsMode::Preferred => "Preferred",
        TlsMode::Required => "Required",
    }
}

fn text_field(ui: &mut egui::Ui, label: &str, value: &mut String, error: Option<&str>) {
    ui.label(label);
    ui.text_edit_singleline(value);
    render_error(ui, error);
}

fn render_error(ui: &mut egui::Ui, error: Option<&str>) {
    if let Some(error) = error {
        ui.colored_label(egui::Color32::RED, error);
    }
}

#[cfg(test)]
mod tests {
    use super::{ProfileDraft, ProfileEditor, ProfileEventResult, SaveAttempt};
    use crate::model::{ConnectionProfile, DriverKind, OperationId, ProfileId, TlsMode};
    use crate::ui::adapter::{UiCommand, bounded_ports};
    use crate::ui::model::UiEvent;

    fn valid_editor(driver: DriverKind) -> ProfileEditor {
        let mut editor = ProfileEditor::new(driver);
        editor.draft.id = "local-profile".to_owned();
        editor.draft.name = "Local profile".to_owned();
        editor
    }

    #[test]
    fn required_fields_and_registry_defaults_are_enforced() {
        let mut draft = ProfileDraft::new(DriverKind::MySql);
        assert_eq!(draft.port, "3306");
        assert_eq!(ProfileDraft::new(DriverKind::Redis).port, "6379");
        assert_eq!(ProfileDraft::new(DriverKind::MongoDb).port, "27017");
        draft.host.clear();
        draft.port = "0".to_owned();
        let errors = draft.validate().expect_err("draft must be invalid");
        assert!(errors.id.is_some());
        assert!(errors.name.is_some());
        assert!(errors.host.is_some());
        assert!(errors.port.is_some());
    }

    #[test]
    fn upsert_command_contains_env_reference_not_secret_literal() {
        let (ui, mut service) = bounded_ports(2);
        let mut editor = valid_editor(DriverKind::MySql);
        editor.draft.secret_env = "DBOTTER_MYSQL_PASSWORD".to_owned();
        assert_eq!(
            editor.try_save(&ui, OperationId(7)),
            SaveAttempt::Submitted(OperationId(7))
        );
        let profile = match service.try_next_command() {
            Some(UiCommand::UpsertProfile { profile, .. }) => profile,
            _ => panic!("upsert command missing"),
        };
        let persisted = toml::to_string(&profile).expect("profile serializes");
        assert!(persisted.contains("DBOTTER_MYSQL_PASSWORD"));
        assert!(!persisted.contains("plain-text-password-must-not-persist"));
    }

    #[test]
    fn double_save_and_busy_channel_submit_at_most_once() {
        let (ui, mut service) = bounded_ports(1);
        assert_eq!(ui.try_submit(UiCommand::RefreshProfiles), Ok(()));
        let mut editor = valid_editor(DriverKind::Redis);
        assert_eq!(editor.try_save(&ui, OperationId(1)), SaveAttempt::Busy);
        assert!(editor.pending_operation().is_none());
        assert!(service.try_next_command().is_some());
        assert_eq!(
            editor.try_save(&ui, OperationId(2)),
            SaveAttempt::Submitted(OperationId(2))
        );
        assert_eq!(
            editor.try_save(&ui, OperationId(3)),
            SaveAttempt::AlreadyPending(OperationId(2))
        );
        assert!(matches!(
            service.try_next_command(),
            Some(UiCommand::UpsertProfile { .. })
        ));
        assert!(service.try_next_command().is_none());
    }

    #[test]
    fn mongodb_save_is_allowed_but_actions_are_disabled() {
        let (ui, mut service) = bounded_ports(1);
        let mut editor = valid_editor(DriverKind::MongoDb);
        assert!(!editor.actions_enabled());
        assert!(matches!(
            editor.try_save(&ui, OperationId(4)),
            SaveAttempt::Submitted(_)
        ));
        assert!(matches!(
            service.try_next_command(),
            Some(UiCommand::UpsertProfile { profile, .. })
                if profile.driver == DriverKind::MongoDb
        ));
    }

    #[test]
    fn save_event_is_correlated_for_refresh_selection() {
        let (ui, mut service) = bounded_ports(1);
        let mut editor = valid_editor(DriverKind::MySql);
        assert!(matches!(
            editor.try_save(&ui, OperationId(9)),
            SaveAttempt::Submitted(_)
        ));
        assert!(service.try_next_command().is_some());
        assert_eq!(
            editor.handle_event(&UiEvent::ProfileSaved {
                operation_id: OperationId(9),
                profile_id: ProfileId("local-profile".to_owned()),
            }),
            ProfileEventResult::Saved(ProfileId("local-profile".to_owned()))
        );
    }

    #[test]
    fn edit_round_trip_preserves_every_non_secret_profile_field() {
        let profile = ConnectionProfile {
            id: "mysql-local".to_owned(),
            name: "MySQL".to_owned(),
            driver: DriverKind::MySql,
            host: "db.internal".to_owned(),
            port: 3307,
            database: Some("app".to_owned()),
            username: Some("developer".to_owned()),
            tls: TlsMode::Required,
            secret_env: Some("MYSQL_PASSWORD".to_owned()),
        };

        let editor = ProfileEditor::edit(&profile);

        assert_eq!(editor.draft.validate(), Ok(profile));
    }

    #[test]
    fn edit_cannot_change_stable_profile_id() {
        let profile = ConnectionProfile {
            id: "redis-local".to_owned(),
            name: "Redis".to_owned(),
            driver: DriverKind::Redis,
            host: "127.0.0.1".to_owned(),
            port: 6379,
            database: Some("0".to_owned()),
            username: None,
            tls: TlsMode::Disabled,
            secret_env: None,
        };
        let (ui, mut service) = bounded_ports(1);
        let mut editor = ProfileEditor::edit(&profile);
        editor.draft.id = "renamed".to_owned();

        assert_eq!(editor.try_save(&ui, OperationId(11)), SaveAttempt::Invalid);
        assert!(service.try_next_command().is_none());
    }
}
