//! Profile-generation-scoped query editor and exact target intent construction.

use std::fmt;
use std::ops::Range;

use eframe::egui;

use crate::execution::{
    ExecutionLanguage, ExecutionTarget, ExecutionTargetError, MAX_EXECUTE_ROW_LIMIT,
    MAX_EXECUTE_TIMEOUT_SECONDS, classify_execution_kind, extract_and_validate_target,
};
use crate::model::{
    DriverKind, OperationId, OperationKind, ProfileGeneration, ProfileId, QueryLanguage, TlsMode,
};

use super::accessibility::named_author_id;
use super::adapter::UiCommand;
use super::model::{ProfileSnapshot, ProfileWorkspace, WorkspaceKey};
use super::theme::OpenAiTheme;

pub const EDITOR_TARGET_ID: &str = "editor.target";
pub const EDITOR_INPUT_ID: &str = "editor.input";
pub const EDITOR_ROW_LIMIT_ID: &str = "editor.row_limit";
pub const EDITOR_TIMEOUT_ID: &str = "editor.timeout";
pub const EDITOR_EXECUTE_ID: &str = "editor.execute";
pub const EDITOR_CANCEL_ID: &str = "editor.cancel";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditorCursor {
    caret_character_index: usize,
    selection_character_range: Option<Range<usize>>,
}

impl EditorCursor {
    pub const fn caret(caret_character_index: usize) -> Self {
        Self {
            caret_character_index,
            selection_character_range: None,
        }
    }

    pub const fn with_selection(
        caret_character_index: usize,
        selection_character_range: Range<usize>,
    ) -> Self {
        Self {
            caret_character_index,
            selection_character_range: Some(selection_character_range),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditorValidationError {
    RowLimit,
    Timeout,
    Target(ExecutionTargetError),
    UnsupportedDriver,
}

impl EditorValidationError {
    pub const fn control_id(self) -> &'static str {
        match self {
            Self::RowLimit => EDITOR_ROW_LIMIT_ID,
            Self::Timeout => EDITOR_TIMEOUT_ID,
            Self::Target(_) | Self::UnsupportedDriver => EDITOR_INPUT_ID,
        }
    }

    pub const fn message(self) -> &'static str {
        match self {
            Self::RowLimit => "Enter a row limit from 1 to 10000.",
            Self::Timeout => "Enter a timeout from 1 to 300 seconds.",
            Self::Target(error) => error.summary(),
            Self::UnsupportedDriver => "This driver does not support query execution.",
        }
    }
}

impl fmt::Display for EditorValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message())
    }
}

impl std::error::Error for EditorValidationError {}

#[derive(Clone, PartialEq, Eq)]
pub struct EditorExecuteIntent {
    profile_id: ProfileId,
    profile_generation: ProfileGeneration,
    language: QueryLanguage,
    text: String,
    row_limit: u32,
    timeout_ms: u64,
    operation_kind: OperationKind,
}

impl EditorExecuteIntent {
    pub fn profile_id(&self) -> &ProfileId {
        &self.profile_id
    }

    pub const fn profile_generation(&self) -> ProfileGeneration {
        self.profile_generation
    }

    pub const fn language(&self) -> QueryLanguage {
        self.language
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub const fn row_limit(&self) -> u32 {
        self.row_limit
    }

    pub const fn timeout_ms(&self) -> u64 {
        self.timeout_ms
    }

    pub const fn operation_kind(&self) -> OperationKind {
        self.operation_kind
    }

    pub fn into_ui_command(self, operation_id: OperationId) -> UiCommand {
        UiCommand::Execute {
            operation_id,
            profile_id: self.profile_id,
            profile_generation: self.profile_generation,
            language: self.language,
            text: self.text,
            row_limit: self.row_limit,
            timeout_ms: self.timeout_ms,
        }
    }
}

impl fmt::Debug for EditorExecuteIntent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EditorExecuteIntent")
            .field("profile_id", &self.profile_id)
            .field("profile_generation", &self.profile_generation)
            .field("language", &self.language)
            .field("text", &"<redacted>")
            .field("row_limit", &self.row_limit)
            .field("timeout_ms", &self.timeout_ms)
            .field("operation_kind", &self.operation_kind)
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EditorIntent {
    Execute(EditorExecuteIntent),
    Cancel { operation_id: OperationId },
}

pub fn pending_cancel_intent(workspace: &ProfileWorkspace) -> Option<EditorIntent> {
    workspace
        .pending_execute
        .map(|operation_id| EditorIntent::Cancel { operation_id })
}

pub fn build_execute_intent(
    profile: &ProfileSnapshot,
    workspace: &ProfileWorkspace,
    cursor: EditorCursor,
) -> Result<EditorExecuteIntent, EditorValidationError> {
    let row_limit = parse_row_limit(&workspace.row_limit)?;
    let timeout_seconds = parse_timeout(&workspace.timeout_seconds)?;
    let (execution_language, query_language) = match profile.driver {
        DriverKind::MySql => (ExecutionLanguage::MySql, QueryLanguage::Sql),
        DriverKind::Redis => (ExecutionLanguage::Redis, QueryLanguage::RedisCommand),
        DriverKind::MongoDb => return Err(EditorValidationError::UnsupportedDriver),
    };
    let validated = extract_and_validate_target(
        &workspace.editor_text,
        cursor.caret_character_index,
        cursor.selection_character_range,
        execution_language,
        row_limit,
        timeout_seconds,
    )
    .map_err(EditorValidationError::Target)?;
    let operation_kind = classify_execution_kind(execution_language, validated.target());
    let text = validated.into_source_text();
    Ok(EditorExecuteIntent {
        profile_id: profile.id.clone(),
        profile_generation: profile.generation,
        language: query_language,
        text,
        row_limit,
        timeout_ms: u64::from(timeout_seconds) * 1_000,
        operation_kind,
    })
}

fn parse_row_limit(value: &str) -> Result<u32, EditorValidationError> {
    value
        .trim()
        .parse::<u32>()
        .ok()
        .filter(|value| (1..=MAX_EXECUTE_ROW_LIMIT).contains(value))
        .ok_or(EditorValidationError::RowLimit)
}

fn parse_timeout(value: &str) -> Result<u32, EditorValidationError> {
    value
        .trim()
        .parse::<u32>()
        .ok()
        .filter(|value| (1..=MAX_EXECUTE_TIMEOUT_SECONDS).contains(value))
        .ok_or(EditorValidationError::Timeout)
}

/// Classifies only a closed, clearly read-only set as `ExecuteRead`.
/// Unknown or side-effecting forms fail closed to `ExecuteMutation`.
pub fn classify_execute_operation(language: QueryLanguage, text: &str) -> OperationKind {
    let (execution_language, target) = match language {
        QueryLanguage::Sql => (
            ExecutionLanguage::MySql,
            ExecutionTarget::MySqlText(text.to_owned()),
        ),
        QueryLanguage::RedisCommand => {
            let Ok(arguments) = shell_words::split(text) else {
                return OperationKind::ExecuteMutation;
            };
            (
                ExecutionLanguage::Redis,
                ExecutionTarget::RedisArgv(arguments),
            )
        }
        QueryLanguage::MongoDocument => return OperationKind::ExecuteMutation,
    };
    classify_execution_kind(execution_language, &target)
}

pub fn editor_target_label(profile: &ProfileSnapshot) -> String {
    let scope = match profile.driver {
        DriverKind::MySql => format!(
            "Database {}",
            profile.database.as_deref().unwrap_or("server default")
        ),
        DriverKind::Redis => {
            format!("Redis DB {}", profile.database.as_deref().unwrap_or("0"))
        }
        DriverKind::MongoDb => format!(
            "Database {}",
            profile.database.as_deref().unwrap_or("server default")
        ),
    };
    format!(
        "{} ({}) · {} · {} · {} · TLS {}",
        profile.name,
        profile.id.0,
        profile.driver,
        profile.endpoint,
        scope,
        tls_label(profile.persisted.tls)
    )
}

const fn tls_label(mode: TlsMode) -> &'static str {
    match mode {
        TlsMode::Disabled => "Disabled",
        TlsMode::Preferred => "Preferred",
        TlsMode::Required => "Required",
    }
}

#[derive(Default)]
pub struct EditorSurface {
    active_workspace: Option<WorkspaceKey>,
    cursor: Option<EditorCursor>,
    validation_error: Option<EditorValidationError>,
}

impl EditorSurface {
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        profile: &ProfileSnapshot,
        workspace: &mut ProfileWorkspace,
        enabled: bool,
    ) -> Option<EditorIntent> {
        OpenAiTheme::apply(ui.ctx());
        let workspace_key = WorkspaceKey::new(profile.id.clone(), profile.generation);
        if self.active_workspace.as_ref() != Some(&workspace_key) {
            self.active_workspace = Some(workspace_key);
            self.cursor = Some(EditorCursor::caret(workspace.editor_text.chars().count()));
            self.validation_error = None;
        }

        ui.heading("Query editor");
        ui.add_space(8.0);
        ui.label("Target");
        let mut target = editor_target_label(profile);
        let target_response = egui::TextEdit::singleline(&mut target)
            .id_salt(EDITOR_TARGET_ID)
            .interactive(false)
            .desired_width(f32::INFINITY)
            .show(ui)
            .response
            .response;
        named_author_id(target_response, EDITOR_TARGET_ID, "Execution target");

        ui.add_space(16.0);
        ui.label("Statement or command");
        let execute_enabled = enabled && workspace.pending_execute.is_none();
        let shortcut_pressed = execute_enabled && consume_execute_shortcut(ui);
        let editor_output = egui::TextEdit::multiline(&mut workspace.editor_text)
            .id_salt(EDITOR_INPUT_ID)
            .code_editor()
            .desired_rows(12)
            .desired_width(f32::INFINITY)
            .interactive(enabled)
            .hint_text("SELECT 1  or  GET key")
            .show(ui);
        if let Some(cursor_range) = editor_output.cursor_range {
            let caret = cursor_range.primary.index.0;
            let selection = (!cursor_range.is_empty()).then(|| {
                let range = cursor_range.as_sorted_char_range();
                range.start.0..range.end.0
            });
            self.cursor = Some(match selection {
                Some(selection) => EditorCursor::with_selection(caret, selection),
                None => EditorCursor::caret(caret),
            });
        }
        let editor_response = named_author_id(
            editor_output.response.response,
            EDITOR_INPUT_ID,
            "Statement or command",
        );

        ui.add_space(16.0);
        let mut row_response = None;
        let mut timeout_response = None;
        let mut execute_clicked = false;
        let mut cancel_clicked = false;
        ui.horizontal_wrapped(|ui| {
            ui.vertical(|ui| {
                ui.label("Row limit");
                let response = ui.add_enabled(
                    enabled,
                    egui::TextEdit::singleline(&mut workspace.row_limit)
                        .id_salt(EDITOR_ROW_LIMIT_ID)
                        .desired_width(104.0),
                );
                row_response = Some(named_author_id(
                    response,
                    EDITOR_ROW_LIMIT_ID,
                    "Execute row limit",
                ));
            });
            ui.vertical(|ui| {
                ui.label("Timeout (seconds)");
                let response = ui.add_enabled(
                    enabled,
                    egui::TextEdit::singleline(&mut workspace.timeout_seconds)
                        .id_salt(EDITOR_TIMEOUT_ID)
                        .desired_width(120.0),
                );
                timeout_response = Some(named_author_id(
                    response,
                    EDITOR_TIMEOUT_ID,
                    "Execute timeout seconds",
                ));
            });
            ui.vertical(|ui| {
                ui.label("Action");
                let execute = ui.add_enabled(
                    execute_enabled,
                    egui::Button::new(egui::RichText::new("Execute").color(egui::Color32::WHITE))
                        .fill(egui::Color32::BLACK)
                        .min_size(egui::vec2(112.0, OpenAiTheme::MIN_CONTROL_HEIGHT)),
                );
                execute_clicked = named_author_id(
                    execute,
                    EDITOR_EXECUTE_ID,
                    "Execute selected or current target",
                )
                .clicked();
            });
            if workspace.pending_execute.is_some() {
                ui.vertical(|ui| {
                    ui.label("Pending");
                    let cancel = ui.add_sized(
                        [112.0, OpenAiTheme::MIN_CONTROL_HEIGHT],
                        egui::Button::new("Cancel"),
                    );
                    cancel_clicked =
                        named_author_id(cancel, EDITOR_CANCEL_ID, "Cancel pending execution")
                            .clicked();
                });
            }
        });

        let controls_changed = editor_response.changed()
            || row_response.as_ref().is_some_and(egui::Response::changed)
            || timeout_response
                .as_ref()
                .is_some_and(egui::Response::changed);
        if controls_changed {
            self.validation_error = None;
        }

        let intent = if cancel_clicked {
            pending_cancel_intent(workspace)
        } else if execute_clicked || shortcut_pressed {
            let cursor = self
                .cursor
                .clone()
                .unwrap_or_else(|| EditorCursor::caret(workspace.editor_text.chars().count()));
            match build_execute_intent(profile, workspace, cursor) {
                Ok(intent) => {
                    self.validation_error = None;
                    Some(EditorIntent::Execute(intent))
                }
                Err(error) => {
                    self.validation_error = Some(error);
                    match error.control_id() {
                        EDITOR_ROW_LIMIT_ID => {
                            if let Some(response) = &row_response {
                                response.request_focus();
                            }
                        }
                        EDITOR_TIMEOUT_ID => {
                            if let Some(response) = &timeout_response {
                                response.request_focus();
                            }
                        }
                        _ => editor_response.request_focus(),
                    }
                    None
                }
            }
        } else {
            None
        };

        if let Some(error) = self.validation_error {
            ui.add_space(8.0);
            ui.strong(format!("Error: {}", error.message()));
        } else if let Some(operation_id) = workspace.pending_execute {
            ui.add_space(8.0);
            ui.label(format!(
                "Executing operation {}. Cancel stops waiting; server state may be unknown.",
                operation_id.0
            ));
        } else {
            ui.add_space(8.0);
            ui.weak("Cmd+Enter on macOS · Ctrl+Enter on Windows and Linux");
        }

        intent
    }
}

fn consume_execute_shortcut(ui: &egui::Ui) -> bool {
    ui.input_mut(|input| {
        let mut pressed = false;
        input.events.retain(|event| {
            let egui::Event::Key {
                key,
                pressed: key_pressed,
                repeat,
                modifiers,
                ..
            } = event
            else {
                return true;
            };
            let matches =
                *key == egui::Key::Enter && *key_pressed && platform_execute_modifiers(*modifiers);
            if matches && !*repeat {
                pressed = true;
            }
            !matches
        });
        pressed
    })
}

#[cfg(target_os = "macos")]
const fn platform_execute_modifiers(modifiers: egui::Modifiers) -> bool {
    modifiers.mac_cmd && !modifiers.ctrl && !modifiers.shift && !modifiers.alt
}

#[cfg(not(target_os = "macos"))]
const fn platform_execute_modifiers(modifiers: egui::Modifiers) -> bool {
    modifiers.ctrl && !modifiers.mac_cmd && !modifiers.shift && !modifiers.alt
}
