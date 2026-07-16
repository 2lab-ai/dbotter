//! Profile-generation-scoped query editor and exact target intent construction.

use std::fmt;
use std::ops::Range;

use eframe::egui;

use crate::execution::{
    ExecutionLanguage, ExecutionTarget, ExecutionTargetError, MAX_EXECUTE_ROW_LIMIT,
    MAX_EXECUTE_TIMEOUT_SECONDS, classify_execution_kind, extract_and_validate_target,
    mysql_may_be_read_with_session_mode,
};
use crate::model::{
    DriverKind, OperationId, OperationKind, ProfileGeneration, ProfileId, QueryLanguage, TlsMode,
};

use super::accessibility::named_author_id;
use super::adapter::UiCommand;
use super::model::{
    MAX_EDITOR_TAB_TEXT_BYTES, ProfileSnapshot, ProfileWorkspace, ResultAreaTab, WorkspaceKey,
};
use super::theme::OpenAiTheme;

pub const EDITOR_TARGET_ID: &str = "editor.target";
pub const EDITOR_INPUT_ID: &str = "editor.input";
pub const EDITOR_ROW_LIMIT_ID: &str = "editor.row_limit";
pub const EDITOR_TIMEOUT_ID: &str = "editor.timeout";
pub const EDITOR_EXECUTE_ID: &str = "editor.execute";
pub const EDITOR_HISTORY_ID: &str = "editor.history";
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
    TextTooLarge,
    RowLimit,
    Timeout,
    Target(ExecutionTargetError),
    UnsupportedDriver,
}

impl EditorValidationError {
    pub const fn control_id(self) -> &'static str {
        match self {
            Self::TextTooLarge => EDITOR_INPUT_ID,
            Self::RowLimit => EDITOR_ROW_LIMIT_ID,
            Self::Timeout => EDITOR_TIMEOUT_ID,
            Self::Target(_) | Self::UnsupportedDriver => EDITOR_INPUT_ID,
        }
    }

    pub const fn message(self) -> &'static str {
        match self {
            Self::TextTooLarge => {
                "Query text is limited to 256 KiB; the latest input was rejected."
            }
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
    let operation_kind = classify_validated_operation(execution_language, validated.target());
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
    classify_validated_operation(execution_language, &target)
}

fn classify_validated_operation(
    language: ExecutionLanguage,
    target: &ExecutionTarget,
) -> OperationKind {
    if let (ExecutionLanguage::MySql, ExecutionTarget::MySqlText(text)) = (language, target)
        && mysql_may_be_read_with_session_mode(text)
    {
        return OperationKind::ExecuteRead;
    }
    classify_execution_kind(language, target)
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
    active_workspace: Option<(WorkspaceKey, Option<super::model::EditorTabId>)>,
    cursor: Option<EditorCursor>,
    validation_error: Option<EditorValidationError>,
    requested_focus: Option<&'static str>,
}

impl EditorSurface {
    pub fn request_focus(&mut self, control_id: &'static str) {
        self.requested_focus = Some(control_id);
    }

    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        profile: &ProfileSnapshot,
        workspace: &mut ProfileWorkspace,
        enabled: bool,
    ) -> Option<EditorIntent> {
        OpenAiTheme::apply(ui.ctx());
        let workspace_key = WorkspaceKey::new(profile.id.clone(), profile.generation);
        let active_editor = (workspace_key, workspace.selected_editor_tab_id());
        let editor_id = egui::Id::new(EDITOR_INPUT_ID).with(&active_editor);
        if self.active_workspace.as_ref() != Some(&active_editor) {
            self.active_workspace = Some(active_editor);
            let cursor = workspace_cursor(workspace);
            workspace.caret_character_index = cursor.caret_character_index;
            workspace.selection_character_range = cursor.selection_character_range.clone();
            let mut state =
                egui::text_edit::TextEditState::load(ui.ctx(), editor_id).unwrap_or_default();
            state.cursor.set_char_range(Some(egui_cursor_range(
                &cursor,
                workspace.editor_text.chars().count(),
            )));
            state.store(ui.ctx(), editor_id);
            self.cursor = Some(cursor);
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
        let previous_editor_text = workspace.editor_text.clone();
        let editor_output = egui::TextEdit::multiline(&mut workspace.editor_text)
            .id(editor_id)
            .code_editor()
            .char_limit(MAX_EDITOR_TAB_TEXT_BYTES.saturating_add(1))
            .lock_focus(false)
            .desired_rows(12)
            .desired_width(f32::INFINITY)
            .interactive(enabled)
            .hint_text("SELECT 1  or  GET key")
            .show(ui);
        let editor_limit_exceeded = workspace.editor_text.len() > MAX_EDITOR_TAB_TEXT_BYTES;
        if editor_limit_exceeded {
            workspace.editor_text = previous_editor_text;
        }
        if let Some(cursor_range) = editor_output.cursor_range {
            let caret = cursor_range.primary.index.0;
            let selection = (!cursor_range.is_empty()).then(|| {
                let range = cursor_range.as_sorted_char_range();
                range.start.0..range.end.0
            });
            self.cursor = Some(match selection.as_ref() {
                Some(selection) => EditorCursor::with_selection(caret, selection.clone()),
                None => EditorCursor::caret(caret),
            });
            workspace.caret_character_index = caret;
            workspace.selection_character_range = selection;
        }
        let editor_response = named_author_id(
            editor_output.response.response,
            EDITOR_INPUT_ID,
            "Statement or command",
        );
        if editor_limit_exceeded {
            self.validation_error = Some(EditorValidationError::TextTooLarge);
            editor_response.request_focus();
        }

        ui.add_space(16.0);
        let mut row_response = None;
        let mut timeout_response = None;
        let mut execute_clicked = false;
        let mut history_clicked = false;
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
                let execute = ui
                    .push_id(EDITOR_EXECUTE_ID, |ui| {
                        ui.add_enabled(
                            execute_enabled,
                            egui::Button::new(
                                egui::RichText::new("Run current").color(egui::Color32::WHITE),
                            )
                            .fill(egui::Color32::BLACK)
                            .min_size(egui::vec2(128.0, OpenAiTheme::MIN_CONTROL_HEIGHT)),
                        )
                    })
                    .inner;
                let execute =
                    named_author_id(execute, EDITOR_EXECUTE_ID, "Run current or selection");
                execute_clicked = execute.clicked();
            });
            ui.vertical(|ui| {
                ui.label("Inspect");
                let history = ui.add_sized(
                    [112.0, OpenAiTheme::MIN_CONTROL_HEIGHT],
                    egui::Button::new("History"),
                );
                history_clicked =
                    named_author_id(history, EDITOR_HISTORY_ID, "Open execution history").clicked();
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
        if controls_changed && !editor_limit_exceeded {
            self.validation_error = None;
        }
        if history_clicked {
            workspace.select_result_area_tab(ResultAreaTab::History);
        }

        if let Some(control_id) = self.requested_focus.take() {
            match control_id {
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
        }

        let intent = if editor_limit_exceeded {
            None
        } else if cancel_clicked {
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
            ui.weak("Run current: Cmd+Enter on macOS · Ctrl+Enter on Windows and Linux");
        }

        intent
    }
}

fn workspace_cursor(workspace: &ProfileWorkspace) -> EditorCursor {
    let character_count = workspace.editor_text.chars().count();
    let caret = workspace.caret_character_index.min(character_count);
    let selection = workspace
        .selection_character_range
        .as_ref()
        .map(|selection| selection.start.min(character_count)..selection.end.min(character_count))
        .filter(|selection| selection.start < selection.end);
    match selection {
        Some(selection) => EditorCursor::with_selection(caret, selection),
        None => EditorCursor::caret(caret),
    }
}

fn egui_cursor_range(cursor: &EditorCursor, character_count: usize) -> egui::text::CCursorRange {
    let caret = cursor.caret_character_index.min(character_count);
    let primary = egui::text::CCursor::new(caret);
    let Some(selection) = cursor.selection_character_range.as_ref() else {
        return egui::text::CCursorRange::one(primary);
    };
    let start = selection.start.min(character_count);
    let end = selection.end.min(character_count);
    let secondary = if caret == start { end } else { start };
    egui::text::CCursorRange {
        primary,
        secondary: egui::text::CCursor::new(secondary),
        h_pos: None,
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
