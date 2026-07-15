//! Pure profile draft validation plus egui rendering and save correlation.

use std::path::PathBuf;

use eframe::egui;

use crate::config::MigrationConsent;
use crate::model::{
    ConnectionProfile, CredentialMode, DraftId, DriverKind, OperationId, ProfileGeneration,
    ProfileId, RedisTlsConfig, TlsMode,
};
use crate::secrets::SessionSecretUpdate;
use crate::service::{CreateProfileRequest, UpdateProfileRequest};

use super::adapter::{SubmitError, UiCommand, UiPort};
use super::model::UiEvent;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum EditorMode {
    Add,
    Edit {
        original_id: ProfileId,
        expected_generation: ProfileGeneration,
    },
}

#[derive(Clone, PartialEq, Eq)]
pub(super) struct ProfileDraft {
    pub id: String,
    pub name: String,
    pub driver: DriverKind,
    pub host: String,
    pub port: String,
    pub database: String,
    pub username: String,
    pub tls: TlsMode,
    pub credential_mode: CredentialMode,
    pub secret_env: String,
    pub redis_ca_file: String,
}

impl std::fmt::Debug for ProfileDraft {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProfileDraft")
            .field("id", &self.id)
            .field("name", &"<redacted>")
            .field("driver", &self.driver)
            .field("host", &"<redacted>")
            .field("port", &self.port)
            .field("database", &"<redacted>")
            .field("username", &"<redacted>")
            .field("tls", &self.tls)
            .field("credential_mode", &self.credential_mode)
            .field("secret_env", &"<redacted>")
            .field("redis_ca_file", &"<redacted>")
            .finish()
    }
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
            tls: if driver == DriverKind::Redis {
                TlsMode::Disabled
            } else {
                TlsMode::Preferred
            },
            credential_mode: CredentialMode::None,
            secret_env: String::new(),
            redis_ca_file: String::new(),
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
            credential_mode: profile.credential_mode,
            secret_env: profile.secret_env.clone().unwrap_or_default(),
            redis_ca_file: if profile.driver == DriverKind::Redis
                && profile.tls != TlsMode::Disabled
            {
                profile
                    .redis_tls
                    .ca_file
                    .as_deref()
                    .map(|path| path.to_string_lossy().into_owned())
                    .unwrap_or_default()
            } else {
                String::new()
            },
        }
    }

    pub fn select_driver(&mut self, driver: DriverKind) {
        let previous_default = default_port(self.driver).to_string();
        if self.port.trim().is_empty() || self.port == previous_default {
            self.port = default_port(driver).to_string();
        }
        if self.driver != DriverKind::Redis
            && driver == DriverKind::Redis
            && self.tls == TlsMode::Preferred
        {
            self.tls = TlsMode::Disabled;
        }
        self.driver = driver;
        if driver != DriverKind::Redis {
            self.redis_ca_file.clear();
        }
    }

    pub fn select_credential_mode(&mut self, mode: CredentialMode) {
        self.credential_mode = mode;
        if mode != CredentialMode::Environment {
            self.secret_env.clear();
        }
    }

    pub fn select_tls(&mut self, tls: TlsMode) {
        self.tls = tls;
        if self.driver == DriverKind::Redis && tls != TlsMode::Required {
            self.redis_ca_file.clear();
        }
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
        let secret_env = match self.credential_mode {
            CredentialMode::Environment => optional_trimmed(&self.secret_env),
            CredentialMode::None | CredentialMode::Session => None,
        };
        if self.credential_mode == CredentialMode::Environment {
            match secret_env.as_deref() {
                Some(value) if valid_env_name(value) => {}
                _ => errors.secret_env = Some("Use a valid environment variable name".to_owned()),
            }
        }
        if self.driver == DriverKind::Redis {
            match self.tls {
                TlsMode::Preferred => {
                    errors.tls = Some(
                        "Preferred is a legacy Redis mode; choose Disabled or Required".to_owned(),
                    );
                }
                TlsMode::Disabled if !self.redis_ca_file.trim().is_empty() => {
                    errors.redis_ca_file =
                        Some("A CA file is only available when Redis TLS is Required".to_owned());
                }
                TlsMode::Disabled | TlsMode::Required => {}
            }
        } else if !self.redis_ca_file.trim().is_empty() {
            errors.redis_ca_file = Some("A Redis CA file is only valid for Redis".to_owned());
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
            credential_mode: self.credential_mode,
            secret_env,
            redis_tls: RedisTlsConfig {
                ca_file: if self.driver == DriverKind::Redis && self.tls == TlsMode::Required {
                    optional_trimmed(&self.redis_ca_file).map(PathBuf::from)
                } else {
                    None
                },
            },
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
    pub tls: Option<String>,
    pub redis_ca_file: Option<String>,
}

impl ValidationErrors {
    fn is_empty(&self) -> bool {
        self.id.is_none()
            && self.name.is_none()
            && self.host.is_none()
            && self.port.is_none()
            && self.database.is_none()
            && self.secret_env.is_none()
            && self.tls.is_none()
            && self.redis_ca_file.is_none()
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
    ConfigUncertain,
    AlreadyPending(OperationId),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ProfileEventResult {
    Ignored,
    Saved(ProfileId, Option<crate::model::PublicSummary>),
    Failed,
}

pub(super) struct ProfileEditor {
    pub mode: EditorMode,
    draft_id: DraftId,
    session_keep_available: bool,
    pub draft: ProfileDraft,
    pub errors: ValidationErrors,
    status: String,
    pending_save: Option<(OperationId, ProfileId)>,
    config_uncertain: bool,
}

impl ProfileEditor {
    pub fn new(draft_id: DraftId, driver: DriverKind) -> Self {
        Self {
            mode: EditorMode::Add,
            draft_id,
            session_keep_available: false,
            draft: ProfileDraft::new(driver),
            errors: ValidationErrors::default(),
            status: "New profile".to_owned(),
            pending_save: None,
            config_uncertain: false,
        }
    }

    pub fn edit(
        draft_id: DraftId,
        profile: &ConnectionProfile,
        expected_generation: ProfileGeneration,
        has_current_session_secret: bool,
    ) -> Self {
        Self {
            mode: EditorMode::Edit {
                original_id: ProfileId(profile.id.clone()),
                expected_generation,
            },
            draft_id,
            session_keep_available: profile.credential_mode == CredentialMode::Session
                && has_current_session_secret,
            draft: ProfileDraft::from_profile(profile),
            errors: ValidationErrors::default(),
            status: "Editing profile".to_owned(),
            pending_save: None,
            config_uncertain: false,
        }
    }

    pub fn pending_operation(&self) -> Option<OperationId> {
        self.pending_save
            .as_ref()
            .map(|(operation_id, _)| *operation_id)
    }

    pub fn actions_enabled(&self) -> bool {
        !self.config_uncertain
            && self.draft.driver != DriverKind::MongoDb
            && self.pending_save.is_none()
    }

    pub fn set_config_uncertain(&mut self, config_uncertain: bool) {
        self.config_uncertain = config_uncertain;
        if config_uncertain {
            self.status = "Reload profiles before saving.".to_owned();
        }
    }

    pub fn status(&self) -> &str {
        &self.status
    }

    pub fn try_save(&mut self, port: &UiPort, operation_id: OperationId) -> SaveAttempt {
        if self.config_uncertain {
            self.status = "Reload profiles before saving.".to_owned();
            return SaveAttempt::ConfigUncertain;
        }
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
        if let EditorMode::Edit { original_id, .. } = &self.mode
            && profile.id != original_id.0
        {
            self.errors.id = Some("Profile id cannot change while editing".to_owned());
            self.status = "Fix the highlighted fields".to_owned();
            return SaveAttempt::Invalid;
        }
        if profile.credential_mode == CredentialMode::Session && !self.session_keep_available {
            self.status = "A replacement session credential is required".to_owned();
            return SaveAttempt::Invalid;
        }
        let profile_id = ProfileId(profile.id.clone());
        let draft = profile.as_draft();
        let pending_profile_id = profile_id.clone();
        let mode = self.mode.clone();
        let draft_id = self.draft_id;
        let destination_mode = profile.credential_mode;
        match port.try_submit_with(move || match mode {
            EditorMode::Add => UiCommand::CreateProfile(CreateProfileRequest {
                draft_id,
                operation_id,
                explicit_id: Some(profile_id),
                draft,
                secret_update: SessionSecretUpdate::Clear,
                migration_consent: MigrationConsent::Cancelled,
            }),
            EditorMode::Edit {
                original_id,
                expected_generation,
            } => UiCommand::UpdateProfile(UpdateProfileRequest {
                profile_id: original_id,
                expected_generation,
                operation_id,
                draft,
                secret_update: if destination_mode == CredentialMode::Session {
                    SessionSecretUpdate::Keep
                } else {
                    SessionSecretUpdate::Clear
                },
                migration_consent: MigrationConsent::Cancelled,
            }),
        }) {
            Ok(()) => {
                self.errors = ValidationErrors::default();
                self.pending_save = Some((operation_id, pending_profile_id));
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
        if matches!(event, UiEvent::ConfigUncertain { .. }) {
            self.set_config_uncertain(true);
            return ProfileEventResult::Ignored;
        }
        if self.config_uncertain {
            return ProfileEventResult::Ignored;
        }
        match event {
            UiEvent::ProfileSaved {
                operation_id,
                profile_id,
                warning,
                ..
            } if self.pending_save.as_ref() == Some(&(*operation_id, profile_id.clone())) => {
                self.pending_save = None;
                self.status = warning.map_or_else(
                    || "Profile saved".to_owned(),
                    |summary| summary.message().to_owned(),
                );
                ProfileEventResult::Saved(profile_id.clone(), *warning)
            }
            UiEvent::ProfileSaveFailed {
                operation_id,
                profile_id,
                summary,
            } if self.pending_save.as_ref() == Some(&(*operation_id, profile_id.clone())) => {
                self.pending_save = None;
                self.status = summary.message().to_owned();
                ProfileEventResult::Failed
            }
            _ => ProfileEventResult::Ignored,
        }
    }

    pub fn show(&mut self, ui: &mut egui::Ui) -> FormAction {
        if self.config_uncertain {
            ui.disable();
        }
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
        let mut selected_tls = self.draft.tls;
        egui::ComboBox::from_id_salt(if self.draft.driver == DriverKind::Redis {
            "profile.redis_tls.mode"
        } else {
            "profile.tls.mode"
        })
        .selected_text(tls_name(self.draft.tls))
        .show_ui(ui, |ui| {
            let choices: &[TlsMode] = if self.draft.driver == DriverKind::Redis {
                &[TlsMode::Disabled, TlsMode::Required]
            } else {
                &[TlsMode::Disabled, TlsMode::Preferred, TlsMode::Required]
            };
            for tls in choices {
                ui.selectable_value(&mut selected_tls, *tls, tls_name(*tls));
            }
        });
        if selected_tls != self.draft.tls {
            self.draft.select_tls(selected_tls);
        }
        render_error(ui, self.errors.tls.as_deref());
        if self.draft.driver == DriverKind::Redis && self.draft.tls == TlsMode::Preferred {
            ui.colored_label(
                egui::Color32::YELLOW,
                "Preferred is a legacy Redis mode. Choose Disabled or Required before saving.",
            );
        }
        if self.draft.driver == DriverKind::Redis && self.draft.tls == TlsMode::Required {
            text_field(
                ui,
                "Redis CA file (optional; blank uses OS roots)",
                &mut self.draft.redis_ca_file,
                self.errors.redis_ca_file.as_deref(),
            );
        }

        ui.label("Credential mode");
        let mut selected_credential_mode = self.draft.credential_mode;
        egui::ComboBox::from_id_salt("profile.credential.mode")
            .selected_text(credential_mode_name(self.draft.credential_mode))
            .show_ui(ui, |ui| {
                for mode in [
                    CredentialMode::None,
                    CredentialMode::Session,
                    CredentialMode::Environment,
                ] {
                    ui.selectable_value(
                        &mut selected_credential_mode,
                        mode,
                        credential_mode_name(mode),
                    );
                }
            });
        if selected_credential_mode != self.draft.credential_mode {
            self.draft.select_credential_mode(selected_credential_mode);
        }
        match self.draft.credential_mode {
            CredentialMode::Environment => {
                text_field(
                    ui,
                    "Secret environment variable",
                    &mut self.draft.secret_env,
                    self.errors.secret_env.as_deref(),
                );
                ui.small("Only the environment-variable name is persisted.");
            }
            CredentialMode::Session if self.session_keep_available => {
                ui.small("The current in-memory session credential will be kept.");
            }
            CredentialMode::Session => {
                ui.colored_label(
                    egui::Color32::YELLOW,
                    "No current session credential is available. Replacement entry is not yet available in this preview, so Save remains disabled for this mode.",
                );
            }
            CredentialMode::None => {
                ui.small("No credential reference will be stored.");
            }
        }
        if self.draft.driver == DriverKind::MongoDb && !self.actions_enabled() {
            ui.colored_label(
                egui::Color32::YELLOW,
                "MongoDB is planned. Save is available; Test and Execute are disabled.",
            );
        }
        ui.separator();
        ui.horizontal(|ui| {
            let session_save_available = self.draft.credential_mode != CredentialMode::Session
                || self.session_keep_available;
            let save = ui
                .add_enabled(
                    self.pending_operation().is_none() && session_save_available,
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

fn credential_mode_name(mode: CredentialMode) -> &'static str {
    match mode {
        CredentialMode::None => "None",
        CredentialMode::Session => "Session",
        CredentialMode::Environment => "Environment",
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
    use crate::config::MigrationConsent;
    use crate::model::{
        ConnectionProfile, CredentialMode, DraftId, DriverKind, OperationId, ProfileGeneration,
        ProfileId, RedisTlsConfig, TlsMode,
    };
    use crate::secrets::SessionSecretUpdate;
    use crate::ui::adapter::{UiCommand, bounded_ports};
    use crate::ui::model::UiEvent;

    fn valid_editor(driver: DriverKind) -> ProfileEditor {
        let mut editor = ProfileEditor::new(DraftId(101), driver);
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
    fn credential_mode_transition_requires_and_preserves_only_the_environment_name() {
        let mut draft = ProfileDraft::new(DriverKind::MySql);
        draft.id = "credential-mode".to_owned();
        draft.name = "Credential mode".to_owned();
        draft.select_credential_mode(CredentialMode::Environment);
        assert!(
            draft
                .validate()
                .expect_err("environment name required")
                .secret_env
                .is_some()
        );

        draft.secret_env = "DBOTTER_MYSQL_PASSWORD".to_owned();
        let environment = draft.validate().expect("valid environment reference");
        assert_eq!(
            environment.secret_env.as_deref(),
            Some("DBOTTER_MYSQL_PASSWORD")
        );
        draft.select_credential_mode(CredentialMode::None);
        assert!(draft.secret_env.is_empty());
        let none = draft.validate().expect("None mode validates");
        assert_eq!(none.credential_mode, CredentialMode::None);
        assert!(none.secret_env.is_none());

        draft.secret_env = "MUST_NOT_SURVIVE".to_owned();
        draft.select_credential_mode(CredentialMode::Session);
        assert!(draft.secret_env.is_empty());
        assert!(
            draft
                .validate()
                .expect("Session draft shape validates")
                .secret_env
                .is_none()
        );
    }

    #[test]
    fn redis_tls_controls_reject_legacy_preferred_and_clear_hidden_ca_state() {
        let mut draft = ProfileDraft::new(DriverKind::Redis);
        draft.id = "redis-tls".to_owned();
        draft.name = "Redis TLS".to_owned();
        assert_eq!(draft.tls, TlsMode::Disabled);

        draft.redis_ca_file = "/tmp/hidden-ca.pem".to_owned();
        assert!(
            draft
                .validate()
                .expect_err("Disabled cannot retain hidden CA state")
                .redis_ca_file
                .is_some()
        );
        draft.select_tls(TlsMode::Required);
        let required = draft.validate().expect("Required accepts a CA path shape");
        assert_eq!(
            required.redis_tls.ca_file.as_deref(),
            Some(std::path::Path::new("/tmp/hidden-ca.pem"))
        );
        draft.select_tls(TlsMode::Disabled);
        assert!(draft.redis_ca_file.is_empty());
        assert!(
            draft
                .validate()
                .expect("Disabled clears CA payload")
                .redis_tls
                .ca_file
                .is_none()
        );

        draft.select_tls(TlsMode::Preferred);
        assert!(
            draft
                .validate()
                .expect_err("legacy Preferred is edit-required")
                .tls
                .is_some()
        );
        draft.select_tls(TlsMode::Required);
        draft.redis_ca_file = "/tmp/redis-only.pem".to_owned();
        draft.select_driver(DriverKind::MySql);
        assert!(draft.redis_ca_file.is_empty());

        draft.tls = TlsMode::Preferred;
        draft.select_driver(DriverKind::Redis);
        assert_eq!(draft.tls, TlsMode::Disabled);

        let mut persisted = ConnectionProfile::from_draft(
            "persisted-hidden-ca".to_owned(),
            ProfileDraft::new(DriverKind::Redis)
                .validate()
                .unwrap_or_else(|_| ConnectionProfile {
                    id: "persisted-hidden-ca".to_owned(),
                    name: "Persisted hidden CA".to_owned(),
                    driver: DriverKind::Redis,
                    host: "127.0.0.1".to_owned(),
                    port: 6379,
                    database: None,
                    username: None,
                    tls: TlsMode::Disabled,
                    credential_mode: CredentialMode::None,
                    secret_env: None,
                    redis_tls: RedisTlsConfig::default(),
                })
                .as_draft(),
        );
        persisted.tls = TlsMode::Disabled;
        persisted.redis_tls.ca_file = Some("/tmp/persisted-hidden.pem".into());
        assert!(
            ProfileDraft::from_profile(&persisted)
                .redis_ca_file
                .is_empty()
        );

        persisted.tls = TlsMode::Preferred;
        let mut legacy = ProfileDraft::from_profile(&persisted);
        assert_eq!(legacy.redis_ca_file, "/tmp/persisted-hidden.pem");
        legacy.select_tls(TlsMode::Required);
        assert_eq!(legacy.redis_ca_file, "/tmp/persisted-hidden.pem");
        legacy.select_tls(TlsMode::Disabled);
        assert!(legacy.redis_ca_file.is_empty());
    }

    #[test]
    fn create_command_contains_env_reference_not_secret_literal() {
        let (ui, mut service) = bounded_ports(2);
        let mut editor = valid_editor(DriverKind::MySql);
        editor.draft.credential_mode = CredentialMode::Environment;
        editor.draft.secret_env = "DBOTTER_MYSQL_PASSWORD".to_owned();
        assert_eq!(
            editor.try_save(&ui, OperationId(7)),
            SaveAttempt::Submitted(OperationId(7))
        );
        let profile = match service.try_next_command() {
            Some(UiCommand::CreateProfile(request)) => {
                assert_eq!(request.migration_consent, MigrationConsent::Cancelled);
                ConnectionProfile::from_draft(
                    request.explicit_id.expect("explicit id").0,
                    request.draft,
                )
            }
            _ => panic!("create command missing"),
        };
        let persisted = toml::to_string(&profile).expect("profile serializes");
        assert!(persisted.contains("DBOTTER_MYSQL_PASSWORD"));
        assert!(!persisted.contains("plain-text-password-must-not-persist"));
    }

    #[test]
    fn edit_session_without_a_known_current_arc_never_builds_keep() {
        let (ui, mut service) = bounded_ports(1);
        let mut persisted = ConnectionProfile::from_draft(
            "session".to_owned(),
            ProfileDraft::new(DriverKind::MySql)
                .validate()
                .unwrap_or_else(|_| ConnectionProfile {
                    id: "session".to_owned(),
                    name: "Session".to_owned(),
                    driver: DriverKind::MySql,
                    host: "127.0.0.1".to_owned(),
                    port: 3306,
                    database: None,
                    username: None,
                    tls: TlsMode::Preferred,
                    credential_mode: CredentialMode::Session,
                    secret_env: None,
                    redis_tls: RedisTlsConfig::default(),
                })
                .as_draft(),
        );
        persisted.credential_mode = CredentialMode::Session;
        let mut editor = ProfileEditor::edit(DraftId(102), &persisted, ProfileGeneration(1), false);

        assert_eq!(editor.try_save(&ui, OperationId(8)), SaveAttempt::Invalid);
        assert!(service.try_next_command().is_none());
        assert!(editor.status().contains("replacement"));
    }

    #[test]
    fn edit_session_builds_keep_only_when_current_arc_is_known() {
        let (ui, mut service) = bounded_ports(1);
        let profile = session_profile();
        let mut editor = ProfileEditor::edit(DraftId(103), &profile, ProfileGeneration(1), true);

        assert_eq!(
            editor.try_save(&ui, OperationId(18)),
            SaveAttempt::Submitted(OperationId(18))
        );
        let Some(UiCommand::UpdateProfile(request)) = service.try_next_command() else {
            panic!("update command missing");
        };
        assert!(matches!(request.secret_update, SessionSecretUpdate::Keep));
    }

    #[test]
    fn double_save_and_busy_channel_submit_at_most_once() {
        let (ui, mut service) = bounded_ports(1);
        assert_eq!(
            ui.try_submit(UiCommand::RefreshProfiles {
                operation_id: OperationId(99),
            }),
            Ok(())
        );
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
            Some(UiCommand::CreateProfile(_))
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
            Some(UiCommand::CreateProfile(request))
                if request.draft.driver == DriverKind::MongoDb
        ));
    }

    #[test]
    fn config_uncertain_disables_profile_save_before_command_construction() {
        let (ui, mut service) = bounded_ports(1);
        let mut editor = valid_editor(DriverKind::MySql);
        assert_eq!(
            editor.handle_event(&UiEvent::ConfigUncertain {
                operation_id: OperationId(40),
            }),
            ProfileEventResult::Ignored
        );

        assert!(!editor.actions_enabled());
        let _ = editor.try_save(&ui, OperationId(41));
        assert!(
            service.try_next_command().is_none(),
            "uncertain profile save must not enter the mutation lane"
        );
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
                previous_generation: None,
                profile_generation: ProfileGeneration(1),
                session_retained: false,
                warning: None,
            }),
            ProfileEventResult::Saved(ProfileId("local-profile".to_owned()), None)
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
            credential_mode: CredentialMode::Environment,
            secret_env: Some("MYSQL_PASSWORD".to_owned()),
            redis_tls: RedisTlsConfig::default(),
        };

        let editor = ProfileEditor::edit(DraftId(104), &profile, ProfileGeneration(1), false);

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
            credential_mode: CredentialMode::None,
            secret_env: None,
            redis_tls: RedisTlsConfig::default(),
        };
        let (ui, mut service) = bounded_ports(1);
        let mut editor = ProfileEditor::edit(DraftId(105), &profile, ProfileGeneration(1), false);
        editor.draft.id = "renamed".to_owned();

        assert_eq!(editor.try_save(&ui, OperationId(11)), SaveAttempt::Invalid);
        assert!(service.try_next_command().is_none());
    }

    fn session_profile() -> ConnectionProfile {
        ConnectionProfile {
            id: "session".to_owned(),
            name: "Session".to_owned(),
            driver: DriverKind::MySql,
            host: "127.0.0.1".to_owned(),
            port: 3306,
            database: None,
            username: None,
            tls: TlsMode::Preferred,
            credential_mode: CredentialMode::Session,
            secret_env: None,
            redis_tls: RedisTlsConfig::default(),
        }
    }
}
