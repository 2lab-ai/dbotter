//! Profile-generation-scoped query editor and exact target intent construction.

use std::fmt;
use std::ops::Range;

use eframe::egui;

use crate::execution::{
    ExecutionLanguage, ExecutionTarget, ExecutionTargetError, MAX_EXECUTE_ROW_LIMIT,
    MAX_EXECUTE_TIMEOUT_SECONDS, classify_execution_batch_kind, classify_execution_kind,
    extract_and_validate_all_targets, extract_and_validate_target,
    mysql_may_be_read_with_session_mode,
};
use crate::model::{
    CatalogNodeKind, CatalogPage, DriverKind, OperationId, OperationKind, ProfileGeneration,
    ProfileId, QueryLanguage, RequestIdentity, TlsMode,
};

use super::accessibility::{named_author_id, named_dynamic_value_author_id};
use super::adapter::UiCommand;
use super::model::{
    EditorTabId, MAX_EDITOR_TAB_TEXT_BYTES, ProfileSnapshot, ProfileWorkspace, ResultAreaTab,
    WorkspaceKey,
};
use super::theme::OpenAiTheme;
use crate::workspace::WorkspaceRunTarget;

pub const EDITOR_TARGET_ID: &str = "editor.target";
pub const EDITOR_INPUT_ID: &str = "editor.input";
pub const EDITOR_ROW_LIMIT_ID: &str = "editor.row_limit";
pub const EDITOR_TIMEOUT_ID: &str = "editor.timeout";
pub const EDITOR_EXECUTE_ID: &str = "editor.execute";
pub const EDITOR_EXECUTE_ALL_ID: &str = "editor.execute_all";
pub const EDITOR_HISTORY_ID: &str = "editor.history";
pub const EDITOR_CANCEL_ID: &str = "editor.cancel";
pub const EDITOR_PENDING_ID: &str = "editor.pending";

const EDITOR_AUTOCOMPLETE_ID: &str = "editor.autocomplete";
const EDITOR_AUTOCOMPLETE_CANDIDATE_PREFIX: &str = "editor.autocomplete.candidate.";
const MAX_HIGHLIGHT_SOURCE_BYTES: usize = MAX_EDITOR_TAB_TEXT_BYTES.saturating_add(1);
const MAX_AUTOCOMPLETE_SOURCE_BYTES: usize = MAX_EDITOR_TAB_TEXT_BYTES;
const MAX_AUTOCOMPLETE_TOKEN_BYTES: usize = 128;
const MAX_AUTOCOMPLETE_IDENTIFIER_BYTES: usize = 256;
const MAX_AUTOCOMPLETE_CATALOG_NODES_SCANNED: usize = 512;
const MAX_AUTOCOMPLETE_CANDIDATES: usize = 20;

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
    BatchMutationUnavailable,
    UnsupportedDriver,
}

impl EditorValidationError {
    pub const fn control_id(self) -> &'static str {
        match self {
            Self::TextTooLarge => EDITOR_INPUT_ID,
            Self::RowLimit => EDITOR_ROW_LIMIT_ID,
            Self::Timeout => EDITOR_TIMEOUT_ID,
            Self::Target(_) | Self::BatchMutationUnavailable | Self::UnsupportedDriver => {
                EDITOR_INPUT_ID
            }
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
            Self::BatchMutationUnavailable => {
                "Run all currently accepts only preflighted read-only targets."
            }
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
    editor_tab_id: Option<EditorTabId>,
    language: QueryLanguage,
    text: String,
    row_limit: u32,
    timeout_ms: u64,
    operation_kind: OperationKind,
    run_target: WorkspaceRunTarget,
}

impl EditorExecuteIntent {
    pub fn profile_id(&self) -> &ProfileId {
        &self.profile_id
    }

    pub const fn profile_generation(&self) -> ProfileGeneration {
        self.profile_generation
    }

    pub const fn editor_tab_id(&self) -> Option<EditorTabId> {
        self.editor_tab_id
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

    pub const fn run_target(&self) -> WorkspaceRunTarget {
        self.run_target
    }

    pub fn into_ui_command(self, operation_id: OperationId) -> UiCommand {
        UiCommand::Execute {
            operation_id,
            profile_id: self.profile_id,
            profile_generation: self.profile_generation,
            editor_tab_id: self.editor_tab_id,
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
            .field("editor_tab_id", &self.editor_tab_id)
            .field("language", &self.language)
            .field("text", &"<redacted>")
            .field("row_limit", &self.row_limit)
            .field("timeout_ms", &self.timeout_ms)
            .field("operation_kind", &self.operation_kind)
            .field("run_target", &self.run_target)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct EditorExecuteBatchIntent {
    profile_id: ProfileId,
    profile_generation: ProfileGeneration,
    editor_tab_id: Option<EditorTabId>,
    language: QueryLanguage,
    text: String,
    target_count: usize,
    row_limit: u32,
    timeout_ms: u64,
    operation_kind: OperationKind,
    run_target: WorkspaceRunTarget,
}

impl EditorExecuteBatchIntent {
    pub fn profile_id(&self) -> &ProfileId {
        &self.profile_id
    }

    pub const fn profile_generation(&self) -> ProfileGeneration {
        self.profile_generation
    }

    pub const fn editor_tab_id(&self) -> Option<EditorTabId> {
        self.editor_tab_id
    }

    pub const fn language(&self) -> QueryLanguage {
        self.language
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub const fn target_count(&self) -> usize {
        self.target_count
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

    pub const fn run_target(&self) -> WorkspaceRunTarget {
        self.run_target
    }

    pub fn into_ui_command(self, operation_id: OperationId) -> UiCommand {
        UiCommand::ExecuteBatch {
            operation_id,
            profile_id: self.profile_id,
            profile_generation: self.profile_generation,
            editor_tab_id: self.editor_tab_id,
            language: self.language,
            text: self.text,
            row_limit: self.row_limit,
            timeout_ms: self.timeout_ms,
        }
    }
}

impl fmt::Debug for EditorExecuteBatchIntent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EditorExecuteBatchIntent")
            .field("profile_id", &self.profile_id)
            .field("profile_generation", &self.profile_generation)
            .field("editor_tab_id", &self.editor_tab_id)
            .field("language", &self.language)
            .field("text", &"<redacted>")
            .field("target_count", &self.target_count)
            .field("row_limit", &self.row_limit)
            .field("timeout_ms", &self.timeout_ms)
            .field("operation_kind", &self.operation_kind)
            .field("run_target", &self.run_target)
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EditorIntent {
    Execute(EditorExecuteIntent),
    ExecuteAll(EditorExecuteBatchIntent),
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
    let run_target = run_target_for_cursor(&cursor);
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
        editor_tab_id: workspace.selected_editor_tab_id(),
        language: query_language,
        text,
        row_limit,
        timeout_ms: u64::from(timeout_seconds) * 1_000,
        operation_kind,
        run_target,
    })
}

pub fn build_execute_all_intent(
    profile: &ProfileSnapshot,
    workspace: &ProfileWorkspace,
) -> Result<EditorExecuteBatchIntent, EditorValidationError> {
    let row_limit = parse_row_limit(&workspace.row_limit)?;
    let timeout_seconds = parse_timeout(&workspace.timeout_seconds)?;
    let (execution_language, query_language) = match profile.driver {
        DriverKind::MySql => (ExecutionLanguage::MySql, QueryLanguage::Sql),
        DriverKind::Redis => (ExecutionLanguage::Redis, QueryLanguage::RedisCommand),
        DriverKind::MongoDb => return Err(EditorValidationError::UnsupportedDriver),
    };
    let validated = extract_and_validate_all_targets(
        &workspace.editor_text,
        execution_language,
        row_limit,
        timeout_seconds,
    )
    .map_err(EditorValidationError::Target)?;
    if classify_execution_batch_kind(execution_language, validated.targets())
        != OperationKind::ExecuteRead
    {
        return Err(EditorValidationError::BatchMutationUnavailable);
    }
    Ok(EditorExecuteBatchIntent {
        profile_id: profile.id.clone(),
        profile_generation: profile.generation,
        editor_tab_id: workspace.selected_editor_tab_id(),
        language: query_language,
        text: workspace.editor_text.clone(),
        target_count: validated.targets().len(),
        row_limit,
        timeout_ms: u64::from(timeout_seconds) * 1_000,
        operation_kind: OperationKind::ExecuteRead,
        run_target: WorkspaceRunTarget::All,
    })
}

fn run_target_for_cursor(cursor: &EditorCursor) -> WorkspaceRunTarget {
    if cursor
        .selection_character_range
        .as_ref()
        .is_some_and(|selection| !selection.is_empty())
    {
        WorkspaceRunTarget::Selection
    } else {
        WorkspaceRunTarget::Current
    }
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

/// Classifies an adapter batch without trusting its command shape. Any invalid
/// or mode-dependent source is mutation-shaped until the service revalidates it.
pub fn classify_execute_batch_operation(
    language: QueryLanguage,
    text: &str,
    row_limit: u32,
    timeout_ms: u64,
) -> OperationKind {
    if !timeout_ms.is_multiple_of(1_000) {
        return OperationKind::ExecuteMutation;
    }
    let Ok(timeout_seconds) = u32::try_from(timeout_ms / 1_000) else {
        return OperationKind::ExecuteMutation;
    };
    let execution_language = match language {
        QueryLanguage::Sql => ExecutionLanguage::MySql,
        QueryLanguage::RedisCommand => ExecutionLanguage::Redis,
        QueryLanguage::MongoDocument => return OperationKind::ExecuteMutation,
    };
    let Ok(batch) =
        extract_and_validate_all_targets(text, execution_language, row_limit, timeout_seconds)
    else {
        return OperationKind::ExecuteMutation;
    };
    classify_execution_batch_kind(execution_language, batch.targets())
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
            profile
                .database
                .as_deref()
                .map_or("server default", |database| database)
        ),
        DriverKind::Redis => {
            format!(
                "Redis DB {}",
                profile.database.as_deref().map_or("0", |database| database)
            )
        }
        DriverKind::MongoDb => format!(
            "Database {}",
            profile
                .database
                .as_deref()
                .map_or("server default", |database| database)
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SyntaxClass {
    Plain,
    Keyword,
    Literal,
    Number,
    Comment,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SyntaxSpan {
    byte_range: Range<usize>,
    class: SyntaxClass,
}

fn syntax_layout_job(
    ui: &egui::Ui,
    language: QueryLanguage,
    source: &str,
    wrap_width: f32,
) -> egui::text::LayoutJob {
    let font_id = egui::TextStyle::Monospace.resolve(ui.style());
    let mut job = egui::text::LayoutJob::default();
    job.wrap.max_width = wrap_width;
    job.keep_trailing_whitespace = true;
    for span in syntax_spans(language, source) {
        job.append(
            &source[span.byte_range],
            0.0,
            syntax_format(font_id.clone(), span.class),
        );
    }
    job
}

fn syntax_format(font_id: egui::FontId, class: SyntaxClass) -> egui::TextFormat {
    let (alpha, weight) = match class {
        SyntaxClass::Plain => (224, 450.0),
        SyntaxClass::Keyword => (255, 650.0),
        SyntaxClass::Literal => (190, 450.0),
        SyntaxClass::Number => (208, 500.0),
        SyntaxClass::Comment => (153, 400.0),
    };
    egui::TextFormat {
        font_id,
        color: egui::Color32::from_black_alpha(alpha),
        coords: egui::epaint::text::VariationCoords::new([(b"wght", weight)]),
        ..egui::TextFormat::default()
    }
}

fn syntax_spans(language: QueryLanguage, source: &str) -> Vec<SyntaxSpan> {
    if source.is_empty() {
        return Vec::new();
    }
    if source.len() > MAX_HIGHLIGHT_SOURCE_BYTES {
        return vec![SyntaxSpan {
            byte_range: 0..source.len(),
            class: SyntaxClass::Plain,
        }];
    }

    let bytes = source.as_bytes();
    let mut spans = Vec::new();
    let mut offset = 0_usize;
    let mut line_has_token = false;
    while offset < bytes.len() {
        let start = offset;
        let byte = bytes[offset];
        let (end, class) = if byte == b'\n' {
            line_has_token = false;
            (offset + 1, SyntaxClass::Plain)
        } else if byte.is_ascii_whitespace() {
            offset += 1;
            while offset < bytes.len()
                && bytes[offset].is_ascii_whitespace()
                && bytes[offset] != b'\n'
            {
                offset += 1;
            }
            (offset, SyntaxClass::Plain)
        } else if line_comment_starts(language, bytes, offset) {
            (scan_line_end(bytes, offset), SyntaxClass::Comment)
        } else if block_comment_starts(language, bytes, offset) {
            (scan_block_comment(bytes, offset), SyntaxClass::Comment)
        } else if syntax_quote(language, byte) {
            (scan_quoted(bytes, offset, byte), SyntaxClass::Literal)
        } else if byte.is_ascii_digit() {
            offset += 1;
            while offset < bytes.len()
                && (bytes[offset].is_ascii_alphanumeric() || matches!(bytes[offset], b'.' | b'_'))
            {
                offset += 1;
            }
            line_has_token = true;
            (offset, SyntaxClass::Number)
        } else if is_syntax_identifier_start(byte) {
            offset += 1;
            while offset < bytes.len() && is_syntax_identifier_continue(bytes[offset]) {
                offset += 1;
            }
            let token = &source[start..offset];
            let keyword = match language {
                QueryLanguage::Sql => is_sql_keyword(token),
                QueryLanguage::RedisCommand => !line_has_token || is_redis_keyword(token),
                QueryLanguage::MongoDocument => is_mongo_keyword(token),
            };
            line_has_token = true;
            (
                offset,
                if keyword {
                    SyntaxClass::Keyword
                } else {
                    SyntaxClass::Plain
                },
            )
        } else {
            offset += source[offset..].chars().next().map_or(1, char::len_utf8);
            line_has_token = true;
            (offset, SyntaxClass::Plain)
        };
        offset = end;
        push_syntax_span(&mut spans, start..end, class);
    }
    spans
}

fn syntax_status_value(language: QueryLanguage, source: &str) -> String {
    let mut keywords = 0_usize;
    let mut literals = 0_usize;
    let mut numbers = 0_usize;
    let mut comments = 0_usize;
    for span in syntax_spans(language, source) {
        match span.class {
            SyntaxClass::Plain => {}
            SyntaxClass::Keyword => keywords = keywords.saturating_add(1),
            SyntaxClass::Literal => literals = literals.saturating_add(1),
            SyntaxClass::Number => numbers = numbers.saturating_add(1),
            SyntaxClass::Comment => comments = comments.saturating_add(1),
        }
    }
    let language = match language {
        QueryLanguage::Sql => "SQL",
        QueryLanguage::RedisCommand => "Redis",
        QueryLanguage::MongoDocument => "MongoDB",
    };
    format!(
        "{language} syntax · keywords {keywords} · literals {literals} · numbers {numbers} · comments {comments}"
    )
}

fn push_syntax_span(spans: &mut Vec<SyntaxSpan>, byte_range: Range<usize>, class: SyntaxClass) {
    if let Some(last) = spans.last_mut()
        && last.class == class
        && last.byte_range.end == byte_range.start
    {
        last.byte_range.end = byte_range.end;
        return;
    }
    spans.push(SyntaxSpan { byte_range, class });
}

fn line_comment_starts(language: QueryLanguage, bytes: &[u8], offset: usize) -> bool {
    let tail = &bytes[offset..];
    match language {
        QueryLanguage::Sql => tail.starts_with(b"--") || tail.starts_with(b"#"),
        QueryLanguage::RedisCommand => tail.starts_with(b"#"),
        QueryLanguage::MongoDocument => tail.starts_with(b"//"),
    }
}

fn block_comment_starts(language: QueryLanguage, bytes: &[u8], offset: usize) -> bool {
    language != QueryLanguage::RedisCommand && bytes[offset..].starts_with(b"/*")
}

fn scan_line_end(bytes: &[u8], mut offset: usize) -> usize {
    while offset < bytes.len() && bytes[offset] != b'\n' {
        offset += 1;
    }
    offset
}

fn scan_block_comment(bytes: &[u8], mut offset: usize) -> usize {
    offset = offset.saturating_add(2);
    while offset < bytes.len() {
        if bytes[offset..].starts_with(b"*/") {
            return offset + 2;
        }
        offset += 1;
    }
    bytes.len()
}

fn syntax_quote(language: QueryLanguage, byte: u8) -> bool {
    match language {
        QueryLanguage::Sql | QueryLanguage::MongoDocument => matches!(byte, b'\'' | b'"' | b'`'),
        QueryLanguage::RedisCommand => matches!(byte, b'\'' | b'"'),
    }
}

fn scan_quoted(bytes: &[u8], start: usize, quote: u8) -> usize {
    let mut offset = start + 1;
    while offset < bytes.len() {
        if bytes[offset] == b'\\' {
            offset = (offset + 2).min(bytes.len());
        } else if bytes[offset] == quote {
            if bytes.get(offset + 1) == Some(&quote) {
                offset += 2;
            } else {
                return offset + 1;
            }
        } else {
            offset += 1;
        }
    }
    bytes.len()
}

const fn is_syntax_identifier_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || matches!(byte, b'_' | b'$')
}

const fn is_syntax_identifier_continue(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$')
}

fn is_sql_keyword(value: &str) -> bool {
    const KEYWORDS: &[&str] = &[
        "ALL", "AND", "AS", "ASC", "BETWEEN", "BY", "CASE", "DESC", "DESCRIBE", "DISTINCT", "ELSE",
        "END", "EXPLAIN", "FALSE", "FROM", "FULL", "GROUP", "HAVING", "IN", "INNER", "IS", "JOIN",
        "LEFT", "LIKE", "LIMIT", "NOT", "NULL", "OFFSET", "ON", "OR", "ORDER", "OUTER", "RIGHT",
        "SELECT", "SHOW", "THEN", "TRUE", "UNION", "WHEN", "WHERE", "WITH",
    ];
    KEYWORDS
        .iter()
        .any(|keyword| value.eq_ignore_ascii_case(keyword))
}

fn is_redis_keyword(value: &str) -> bool {
    const KEYWORDS: &[&str] = &[
        "COUNT",
        "GET",
        "HGET",
        "HGETALL",
        "HMGET",
        "INFO",
        "KEYS",
        "LIMIT",
        "LRANGE",
        "MATCH",
        "PING",
        "SCAN",
        "SCARD",
        "SMEMBERS",
        "TYPE",
        "WITHSCORES",
        "ZRANGE",
    ];
    KEYWORDS
        .iter()
        .any(|keyword| value.eq_ignore_ascii_case(keyword))
}

fn is_mongo_keyword(value: &str) -> bool {
    value.starts_with('$')
        || ["FALSE", "NULL", "TRUE"]
            .iter()
            .any(|keyword| value.eq_ignore_ascii_case(keyword))
}

#[derive(Clone)]
struct AutocompleteCandidate {
    display: String,
    insertion: String,
    kind: CatalogNodeKind,
}

impl fmt::Debug for AutocompleteCandidate {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AutocompleteCandidate")
            .field("display", &"<redacted>")
            .field("insertion", &"<redacted>")
            .field("kind", &self.kind)
            .finish()
    }
}

#[derive(Default)]
struct AutocompleteState {
    candidates: Vec<AutocompleteCandidate>,
    selected: usize,
    replacement_character_range: Range<usize>,
    caret_character_index: usize,
    catalog_source: Option<AutocompleteCatalogSource>,
    open: bool,
}

impl AutocompleteState {
    fn close(&mut self) {
        self.candidates.clear();
        self.selected = 0;
        self.replacement_character_range = 0..0;
        self.caret_character_index = 0;
        self.catalog_source = None;
        self.open = false;
    }
}

#[derive(Clone, PartialEq, Eq)]
struct AutocompleteCatalogSource {
    identity: RequestIdentity,
    loaded_at: String,
}

struct AutocompleteToken {
    prefix: String,
    replacement_character_range: Range<usize>,
    after_separator: bool,
}

impl AutocompleteToken {
    fn is_contextual(&self) -> bool {
        self.after_separator || self.prefix.chars().count() >= 2
    }
}

#[derive(Clone, Copy)]
enum AutocompleteNavigation {
    Previous,
    Next,
    Accept,
    Dismiss,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AutocompleteAcceptOutcome {
    Accepted,
    Rejected,
    TextTooLarge,
}

const fn autocomplete_keyboard_is_active(
    popup_open: bool,
    editor_enabled: bool,
    editor_has_focus: bool,
) -> bool {
    popup_open && editor_enabled && editor_has_focus
}

fn autocomplete_catalog_is_current(
    source: Option<&AutocompleteCatalogSource>,
    page: Option<&CatalogPage>,
) -> bool {
    matches!(
        (source, page),
        (Some(source), Some(page))
            if !page.stale
                && page.identity == source.identity
                && page.loaded_at == source.loaded_at
    )
}

fn autocomplete_token(source: &str, caret_character_index: usize) -> Option<AutocompleteToken> {
    if source.len() > MAX_AUTOCOMPLETE_SOURCE_BYTES {
        return None;
    }
    let caret_byte = byte_index_for_character(source, caret_character_index)?;
    let mut floor = caret_byte.saturating_sub(MAX_AUTOCOMPLETE_TOKEN_BYTES);
    while floor < caret_byte && !source.is_char_boundary(floor) {
        floor += 1;
    }
    let mut start = caret_byte;
    for (relative, character) in source[floor..caret_byte].char_indices().rev() {
        if is_autocomplete_token_character(character) {
            start = floor + relative;
        } else {
            break;
        }
    }
    if start == floor
        && floor > 0
        && source[..floor]
            .chars()
            .next_back()
            .is_some_and(is_autocomplete_token_character)
    {
        return None;
    }
    let prefix = source[start..caret_byte].to_owned();
    let start_character = source[..start].chars().count();
    let end_character = start_character + prefix.chars().count();
    let after_separator = source[..start]
        .chars()
        .next_back()
        .is_some_and(|character| matches!(character, '.' | '`'));
    Some(AutocompleteToken {
        prefix,
        replacement_character_range: start_character..end_character,
        after_separator,
    })
}

fn byte_index_for_character(source: &str, character_index: usize) -> Option<usize> {
    let mut seen = 0_usize;
    for (byte_index, _) in source.char_indices() {
        if seen == character_index {
            return Some(byte_index);
        }
        seen += 1;
    }
    (seen == character_index).then_some(source.len())
}

fn is_autocomplete_token_character(character: char) -> bool {
    character.is_alphanumeric() || matches!(character, '_' | '$')
}

fn catalog_candidates(
    page: &CatalogPage,
    profile_id: &ProfileId,
    profile_generation: ProfileGeneration,
    language: QueryLanguage,
    prefix: &str,
) -> Vec<AutocompleteCandidate> {
    if page.stale
        || page.identity.profile_id != *profile_id
        || page.identity.profile_generation != profile_generation
    {
        return Vec::new();
    }

    let mut candidates: Vec<AutocompleteCandidate> = Vec::new();
    for node in page
        .nodes
        .iter()
        .take(MAX_AUTOCOMPLETE_CATALOG_NODES_SCANNED)
    {
        let name = node.name.as_str();
        if name.is_empty()
            || name.len() > MAX_AUTOCOMPLETE_IDENTIFIER_BYTES
            || name.chars().any(char::is_control)
            || !starts_with_case_folded(name, prefix)
            || candidates
                .iter()
                .any(|candidate| candidate.display.eq_ignore_ascii_case(name))
        {
            continue;
        }
        let Some(insertion) = catalog_insertion(language, name) else {
            continue;
        };
        candidates.push(AutocompleteCandidate {
            display: name.to_owned(),
            insertion,
            kind: node.kind,
        });
        if candidates.len() == MAX_AUTOCOMPLETE_CANDIDATES {
            break;
        }
    }
    candidates
}

fn starts_with_case_folded(value: &str, prefix: &str) -> bool {
    let mut value_characters = value.chars();
    prefix.chars().all(|prefix_character| {
        value_characters.next().is_some_and(|value_character| {
            value_character == prefix_character
                || value_character.eq_ignore_ascii_case(&prefix_character)
        })
    })
}

fn catalog_insertion(language: QueryLanguage, name: &str) -> Option<String> {
    if language == QueryLanguage::Sql {
        let escaped = name.replace('`', "``");
        let encoded_length = escaped.len().checked_add(2)?;
        return (encoded_length
            <= MAX_AUTOCOMPLETE_IDENTIFIER_BYTES
                .saturating_mul(2)
                .saturating_add(2))
        .then(|| format!("`{escaped}`"));
    }

    let safe_identifier = name
        .chars()
        .next()
        .is_some_and(|character| character.is_alphabetic() || character == '_')
        && name
            .chars()
            .all(|character| character.is_alphanumeric() || matches!(character, '_' | '$'));
    if safe_identifier {
        return Some(name.to_owned());
    }
    None
}

fn replace_autocomplete_token(
    source: &mut String,
    replacement_character_range: Range<usize>,
    insertion: &str,
) -> Option<usize> {
    if source.len() > MAX_AUTOCOMPLETE_SOURCE_BYTES
        || replacement_character_range.start > replacement_character_range.end
    {
        return None;
    }
    let start_byte = byte_index_for_character(source, replacement_character_range.start)?;
    let end_byte = byte_index_for_character(source, replacement_character_range.end)?;
    let new_length = source
        .len()
        .checked_sub(end_byte.checked_sub(start_byte)?)?
        .checked_add(insertion.len())?;
    if new_length > MAX_AUTOCOMPLETE_SOURCE_BYTES {
        return None;
    }
    source.replace_range(start_byte..end_byte, insertion);
    Some(replacement_character_range.start + insertion.chars().count())
}

const fn candidate_kind_label(kind: CatalogNodeKind) -> &'static str {
    match kind {
        CatalogNodeKind::Schema => "schema",
        CatalogNodeKind::Table => "table",
        CatalogNodeKind::View => "view",
        CatalogNodeKind::Column => "column",
    }
}

const fn query_language_for_driver(driver: DriverKind) -> QueryLanguage {
    match driver {
        DriverKind::MySql => QueryLanguage::Sql,
        DriverKind::Redis => QueryLanguage::RedisCommand,
        DriverKind::MongoDb => QueryLanguage::MongoDocument,
    }
}

fn workspace_query_language(
    workspace: &ProfileWorkspace,
    fallback_driver: DriverKind,
) -> QueryLanguage {
    workspace
        .selected_editor_tab_id()
        .and_then(|tab_id| workspace.editor_tab(tab_id))
        .map_or_else(
            || query_language_for_driver(fallback_driver),
            super::model::EditorTab::language,
        )
}

#[derive(Default)]
pub struct EditorSurface {
    active_workspace: Option<(WorkspaceKey, Option<super::model::EditorTabId>)>,
    cursor: Option<EditorCursor>,
    validation_error: Option<EditorValidationError>,
    requested_focus: Option<&'static str>,
    autocomplete: AutocompleteState,
}

impl EditorSurface {
    pub fn request_focus(&mut self, control_id: &'static str) {
        self.requested_focus = Some(control_id);
    }

    fn refresh_autocomplete(
        &mut self,
        profile: &ProfileSnapshot,
        workspace: &ProfileWorkspace,
        language: QueryLanguage,
        manual: bool,
    ) {
        let Some(cursor) = self.cursor.as_ref() else {
            self.autocomplete.close();
            return;
        };
        if cursor.selection_character_range.is_some() {
            self.autocomplete.close();
            return;
        }
        let Some(token) = autocomplete_token(&workspace.editor_text, cursor.caret_character_index)
        else {
            self.autocomplete.close();
            return;
        };
        if !manual && !token.is_contextual() {
            self.autocomplete.close();
            return;
        }
        let Some(page) = workspace.catalog_page.as_ref() else {
            self.autocomplete.close();
            return;
        };
        let candidates = catalog_candidates(
            page,
            &profile.id,
            profile.generation,
            language,
            &token.prefix,
        );
        if candidates.is_empty() {
            self.autocomplete.close();
            return;
        }
        self.autocomplete = AutocompleteState {
            candidates,
            selected: 0,
            replacement_character_range: token.replacement_character_range,
            caret_character_index: cursor.caret_character_index,
            catalog_source: Some(AutocompleteCatalogSource {
                identity: page.identity.clone(),
                loaded_at: page.loaded_at.clone(),
            }),
            open: true,
        };
    }

    fn accept_autocomplete(
        &mut self,
        context: &egui::Context,
        editor_id: egui::Id,
        workspace: &mut ProfileWorkspace,
    ) -> AutocompleteAcceptOutcome {
        if !autocomplete_catalog_is_current(
            self.autocomplete.catalog_source.as_ref(),
            workspace.catalog_page.as_ref(),
        ) {
            self.autocomplete.close();
            return AutocompleteAcceptOutcome::Rejected;
        }
        let Some(candidate) = self
            .autocomplete
            .candidates
            .get(self.autocomplete.selected)
            .cloned()
        else {
            self.autocomplete.close();
            return AutocompleteAcceptOutcome::Rejected;
        };
        let replacement = self.autocomplete.replacement_character_range.clone();
        let Some(caret) = replace_autocomplete_token(
            &mut workspace.editor_text,
            replacement,
            &candidate.insertion,
        ) else {
            self.autocomplete.close();
            return AutocompleteAcceptOutcome::TextTooLarge;
        };
        workspace.caret_character_index = caret;
        workspace.selection_character_range = None;
        self.cursor = Some(EditorCursor::caret(caret));
        store_editor_caret(context, editor_id, caret);
        self.autocomplete.close();
        AutocompleteAcceptOutcome::Accepted
    }

    fn accept_autocomplete_and_update(
        &mut self,
        context: &egui::Context,
        editor_id: egui::Id,
        workspace: &mut ProfileWorkspace,
    ) {
        match self.accept_autocomplete(context, editor_id, workspace) {
            AutocompleteAcceptOutcome::Accepted => self.validation_error = None,
            AutocompleteAcceptOutcome::TextTooLarge => {
                self.validation_error = Some(EditorValidationError::TextTooLarge);
            }
            AutocompleteAcceptOutcome::Rejected => {}
        }
    }

    fn show_autocomplete_popup(
        &self,
        ui: &egui::Ui,
        anchor: egui::Rect,
        enabled: bool,
    ) -> Option<usize> {
        let mut clicked = None;
        let width = anchor.width().clamp(240.0, 420.0);
        let selected = self.autocomplete.selected;
        egui::Area::new(egui::Id::new(EDITOR_AUTOCOMPLETE_ID))
            .order(egui::Order::Foreground)
            .fixed_pos(anchor.left_bottom() + egui::vec2(0.0, 4.0))
            .movable(false)
            .fade_in(false)
            .constrain(true)
            .show(ui.ctx(), |ui| {
                egui::Frame::new()
                    .fill(egui::Color32::WHITE)
                    .stroke(egui::Stroke::new(1.0, egui::Color32::from_black_alpha(112)))
                    .corner_radius(egui::CornerRadius::ZERO)
                    .inner_margin(egui::Margin::same(8))
                    .show(ui, |ui| {
                        ui.set_min_width(width);
                        ui.set_max_width(width);
                        ui.weak(format!(
                            "Catalog suggestions · {} of {} max",
                            self.autocomplete.candidates.len(),
                            MAX_AUTOCOMPLETE_CANDIDATES
                        ));
                        ui.add_space(4.0);
                        egui::ScrollArea::vertical()
                            .max_height(220.0)
                            .show(ui, |ui| {
                                for (index, candidate) in
                                    self.autocomplete.candidates.iter().enumerate()
                                {
                                    let is_selected = index == selected;
                                    let foreground = if is_selected {
                                        egui::Color32::WHITE
                                    } else {
                                        egui::Color32::BLACK
                                    };
                                    let background = if is_selected {
                                        egui::Color32::BLACK
                                    } else {
                                        egui::Color32::WHITE
                                    };
                                    let mut label = egui::RichText::new(format!(
                                        "{}    {}",
                                        candidate.display,
                                        candidate_kind_label(candidate.kind)
                                    ))
                                    .monospace()
                                    .color(foreground);
                                    if is_selected {
                                        label = label.strong();
                                    }
                                    let response = ui.push_id(index, |ui| {
                                        ui.add_enabled(
                                            enabled,
                                            egui::Button::new(label)
                                                .selected(is_selected)
                                                .fill(background)
                                                .stroke(egui::Stroke::new(
                                                    1.0,
                                                    egui::Color32::from_black_alpha(112),
                                                ))
                                                .corner_radius(egui::CornerRadius::ZERO)
                                                .min_size(egui::vec2(
                                                    width,
                                                    OpenAiTheme::MIN_CONTROL_HEIGHT,
                                                ))
                                                .truncate(),
                                        )
                                    });
                                    let response = named_dynamic_value_author_id(
                                        response.inner,
                                        format!("{EDITOR_AUTOCOMPLETE_CANDIDATE_PREFIX}{index}"),
                                        "Catalog autocomplete candidate".to_owned(),
                                        candidate.display.clone(),
                                    );
                                    response.ctx.accesskit_node_builder(response.id, |node| {
                                        node.set_selected(is_selected);
                                    });
                                    if response.clicked() {
                                        clicked = Some(index);
                                    }
                                }
                            });
                    });
            });
        clicked
    }

    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        profile: &ProfileSnapshot,
        workspace: &mut ProfileWorkspace,
        enabled: bool,
    ) -> Option<EditorIntent> {
        OpenAiTheme::apply(ui.ctx());
        let language = workspace_query_language(workspace, profile.driver);
        let workspace_key = WorkspaceKey::new(profile.id.clone(), profile.generation);
        let active_editor = (workspace_key, workspace.selected_editor_tab_id());
        let editor_id = egui::Id::new(EDITOR_INPUT_ID).with(&active_editor);
        if self.active_workspace.as_ref() != Some(&active_editor) {
            self.active_workspace = Some(active_editor);
            self.autocomplete.close();
            let cursor = workspace_cursor(workspace);
            workspace.caret_character_index = cursor.caret_character_index;
            workspace.selection_character_range = cursor.selection_character_range.clone();
            let mut state = load_text_edit_state(ui.ctx(), editor_id);
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
        let editor_had_focus = ui.memory(|memory| memory.has_focus(editor_id));
        let autocomplete_keyboard_active =
            autocomplete_keyboard_is_active(self.autocomplete.open, enabled, editor_had_focus);
        if self.autocomplete.open && !autocomplete_keyboard_active {
            self.autocomplete.close();
        }
        let navigation = autocomplete_keyboard_active
            .then(|| consume_autocomplete_navigation(ui))
            .flatten();
        let autocomplete_dismissed = matches!(navigation, Some(AutocompleteNavigation::Dismiss));
        if let Some(navigation) = navigation {
            match navigation {
                AutocompleteNavigation::Previous => {
                    if !self.autocomplete.candidates.is_empty() {
                        self.autocomplete.selected = if self.autocomplete.selected == 0 {
                            self.autocomplete.candidates.len() - 1
                        } else {
                            self.autocomplete.selected - 1
                        };
                    }
                }
                AutocompleteNavigation::Next => {
                    if !self.autocomplete.candidates.is_empty() {
                        self.autocomplete.selected =
                            (self.autocomplete.selected + 1) % self.autocomplete.candidates.len();
                    }
                }
                AutocompleteNavigation::Accept => {
                    self.accept_autocomplete_and_update(ui.ctx(), editor_id, workspace);
                }
                AutocompleteNavigation::Dismiss => self.autocomplete.close(),
            }
        }
        let autocomplete_shortcut_pressed =
            enabled && editor_had_focus && consume_autocomplete_shortcut(ui);
        let shortcut_all_pressed = execute_enabled && consume_execute_all_shortcut(ui);
        let shortcut_pressed = execute_enabled && consume_execute_shortcut(ui);
        let previous_editor_text = workspace.editor_text.clone();
        let mut layouter = |ui: &egui::Ui, buffer: &dyn egui::TextBuffer, wrap_width: f32| {
            let job = syntax_layout_job(ui, language, buffer.as_str(), wrap_width);
            ui.fonts_mut(|fonts| fonts.layout_job(job))
        };
        let editor_output = egui::TextEdit::multiline(&mut workspace.editor_text)
            .id(editor_id)
            .code_editor()
            .layouter(&mut layouter)
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
            self.autocomplete.close();
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
        if autocomplete_dismissed {
            editor_response.request_focus();
        }
        if editor_limit_exceeded {
            self.validation_error = Some(EditorValidationError::TextTooLarge);
            editor_response.request_focus();
        } else if !enabled
            || !editor_response.has_focus()
            || self
                .cursor
                .as_ref()
                .is_some_and(|cursor| cursor.selection_character_range.is_some())
        {
            self.autocomplete.close();
        } else if autocomplete_shortcut_pressed {
            self.refresh_autocomplete(profile, workspace, language, true);
        } else if editor_response.changed() {
            self.refresh_autocomplete(profile, workspace, language, false);
        } else if self.autocomplete.open
            && self.cursor.as_ref().is_none_or(|cursor| {
                cursor.caret_character_index != self.autocomplete.caret_character_index
            })
        {
            self.autocomplete.close();
        }

        if self.autocomplete.open
            && let Some(candidate_index) =
                self.show_autocomplete_popup(ui, editor_response.rect, enabled)
        {
            self.autocomplete.selected = candidate_index;
            self.accept_autocomplete_and_update(ui.ctx(), editor_id, workspace);
            editor_response.request_focus();
        }

        let syntax_status_value = syntax_status_value(language, &workspace.editor_text);
        let syntax_status = ui.small(&syntax_status_value);
        named_dynamic_value_author_id(
            syntax_status,
            "editor.syntax.status".to_owned(),
            "Editor syntax highlighting status".to_owned(),
            syntax_status_value,
        );

        ui.add_space(16.0);
        let mut row_response = None;
        let mut timeout_response = None;
        let mut execute_clicked = false;
        let mut execute_all_clicked = false;
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
                ui.label("Script");
                let execute_all = ui.add_enabled(
                    execute_enabled,
                    egui::Button::new("Run all")
                        .min_size(egui::vec2(112.0, OpenAiTheme::MIN_CONTROL_HEIGHT)),
                );
                execute_all_clicked = named_author_id(
                    execute_all,
                    EDITOR_EXECUTE_ALL_ID,
                    "Run all statements or commands",
                )
                .clicked();
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
        } else if execute_all_clicked || shortcut_all_pressed {
            match build_execute_all_intent(profile, workspace) {
                Ok(intent) => {
                    self.validation_error = None;
                    Some(EditorIntent::ExecuteAll(intent))
                }
                Err(error) => {
                    self.validation_error = Some(error);
                    request_validation_focus(
                        error,
                        &editor_response,
                        row_response.as_ref(),
                        timeout_response.as_ref(),
                    );
                    None
                }
            }
        } else if execute_clicked || shortcut_pressed {
            let cursor = match self.cursor.as_ref() {
                Some(cursor) => cursor.clone(),
                None => EditorCursor::caret(workspace.editor_text.chars().count()),
            };
            match build_execute_intent(profile, workspace, cursor) {
                Ok(intent) => {
                    self.validation_error = None;
                    Some(EditorIntent::Execute(intent))
                }
                Err(error) => {
                    self.validation_error = Some(error);
                    request_validation_focus(
                        error,
                        &editor_response,
                        row_response.as_ref(),
                        timeout_response.as_ref(),
                    );
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
            ui.horizontal(|ui| {
                let spinner = ui.add(egui::Spinner::new());
                let _ = named_author_id(spinner, EDITOR_PENDING_ID, "Execution in progress");
                ui.label(format!(
                    "Executing operation {}. Cancel stops waiting; server state may be unknown.",
                    operation_id.0
                ));
            });
        } else {
            ui.add_space(8.0);
            ui.weak(
                "Run current: Cmd/Ctrl+Enter · Run all: Shift+Cmd/Ctrl+Enter (read-only baseline)",
            );
        }

        intent
    }
}

fn request_validation_focus(
    error: EditorValidationError,
    editor_response: &egui::Response,
    row_response: Option<&egui::Response>,
    timeout_response: Option<&egui::Response>,
) {
    match error.control_id() {
        EDITOR_ROW_LIMIT_ID => {
            if let Some(response) = row_response {
                response.request_focus();
            }
        }
        EDITOR_TIMEOUT_ID => {
            if let Some(response) = timeout_response {
                response.request_focus();
            }
        }
        _ => editor_response.request_focus(),
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

fn store_editor_caret(context: &egui::Context, editor_id: egui::Id, caret: usize) {
    let mut state = load_text_edit_state(context, editor_id);
    state
        .cursor
        .set_char_range(Some(egui::text::CCursorRange::one(
            egui::text::CCursor::new(caret),
        )));
    state.store(context, editor_id);
}

#[allow(clippy::manual_unwrap_or_default)]
fn load_text_edit_state(
    context: &egui::Context,
    editor_id: egui::Id,
) -> egui::text_edit::TextEditState {
    match egui::text_edit::TextEditState::load(context, editor_id) {
        Some(state) => state,
        None => egui::text_edit::TextEditState::default(),
    }
}

fn consume_autocomplete_shortcut(ui: &egui::Ui) -> bool {
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
            let matches = *key == egui::Key::Space
                && *key_pressed
                && platform_autocomplete_modifiers(*modifiers);
            if matches && !*repeat {
                pressed = true;
            }
            !matches
        });
        pressed
    })
}

fn consume_autocomplete_navigation(ui: &egui::Ui) -> Option<AutocompleteNavigation> {
    ui.input_mut(|input| {
        let mut navigation = None;
        input.events.retain(|event| {
            let egui::Event::Key {
                key,
                pressed,
                modifiers,
                ..
            } = event
            else {
                return true;
            };
            if !*pressed || modifiers.ctrl || modifiers.mac_cmd || modifiers.alt || modifiers.shift
            {
                return true;
            }
            let action = match key {
                egui::Key::ArrowUp => Some(AutocompleteNavigation::Previous),
                egui::Key::ArrowDown => Some(AutocompleteNavigation::Next),
                egui::Key::Enter => Some(AutocompleteNavigation::Accept),
                egui::Key::Escape => Some(AutocompleteNavigation::Dismiss),
                _ => None,
            };
            if navigation.is_none() {
                navigation = action;
            }
            action.is_none()
        });
        navigation
    })
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

fn consume_execute_all_shortcut(ui: &egui::Ui) -> bool {
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
            let matches = *key == egui::Key::Enter
                && *key_pressed
                && platform_execute_all_modifiers(*modifiers);
            if matches && !*repeat {
                pressed = true;
            }
            !matches
        });
        pressed
    })
}

#[cfg(target_os = "macos")]
const fn platform_autocomplete_modifiers(modifiers: egui::Modifiers) -> bool {
    (modifiers.mac_cmd || modifiers.ctrl) && !modifiers.shift && !modifiers.alt
}

#[cfg(not(target_os = "macos"))]
const fn platform_autocomplete_modifiers(modifiers: egui::Modifiers) -> bool {
    modifiers.ctrl && !modifiers.mac_cmd && !modifiers.shift && !modifiers.alt
}

#[cfg(target_os = "macos")]
const fn platform_execute_modifiers(modifiers: egui::Modifiers) -> bool {
    modifiers.mac_cmd && !modifiers.ctrl && !modifiers.shift && !modifiers.alt
}

#[cfg(target_os = "macos")]
const fn platform_execute_all_modifiers(modifiers: egui::Modifiers) -> bool {
    modifiers.mac_cmd && !modifiers.ctrl && modifiers.shift && !modifiers.alt
}

#[cfg(not(target_os = "macos"))]
const fn platform_execute_modifiers(modifiers: egui::Modifiers) -> bool {
    modifiers.ctrl && !modifiers.mac_cmd && !modifiers.shift && !modifiers.alt
}

#[cfg(not(target_os = "macos"))]
const fn platform_execute_all_modifiers(modifiers: egui::Modifiers) -> bool {
    modifiers.ctrl && !modifiers.mac_cmd && modifiers.shift && !modifiers.alt
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        CatalogLevel, CatalogNode, CatalogNodeIdentity, CatalogRetainedCounts, RequestIdentity,
    };

    fn catalog_page(
        profile_id: &ProfileId,
        names: impl IntoIterator<Item = String>,
    ) -> CatalogPage {
        let nodes = names
            .into_iter()
            .enumerate()
            .map(|(index, name)| CatalogNode {
                identity: CatalogNodeIdentity::Relation {
                    schema: "app".to_owned(),
                    relation: name.clone(),
                },
                kind: if index.is_multiple_of(2) {
                    CatalogNodeKind::Table
                } else {
                    CatalogNodeKind::View
                },
                name,
                type_name: None,
                nullable: None,
                ordinal: None,
            })
            .collect::<Vec<_>>();
        CatalogPage {
            identity: RequestIdentity::new(
                profile_id.clone(),
                ProfileGeneration(7),
                OperationId(11),
            ),
            level: CatalogLevel::Relations,
            parent: Some(CatalogNodeIdentity::Schema {
                schema: "app".to_owned(),
            }),
            retained_counts: CatalogRetainedCounts {
                relations: nodes.len(),
                ..CatalogRetainedCounts::default()
            },
            retained_utf8_bytes: nodes.iter().map(|node| node.name.len()).sum(),
            nodes,
            next_token: None,
            truncated: false,
            stale: false,
            loaded_at: "synthetic".to_owned(),
        }
    }

    fn span_has(source: &str, spans: &[SyntaxSpan], needle: &str, class: SyntaxClass) -> bool {
        spans
            .iter()
            .any(|span| span.class == class && source[span.byte_range.clone()].contains(needle))
    }

    fn contrast_ratio_on_white(color: egui::Color32) -> f32 {
        let composited = f32::from(255_u8.saturating_sub(color.a())) / 255.0;
        let luminance = if composited <= 0.04045 {
            composited / 12.92
        } else {
            ((composited + 0.055) / 1.055).powf(2.4)
        };
        1.05 / (luminance + 0.05)
    }

    #[test]
    fn sql_redis_and_mongo_highlighting_spans_are_monochrome_and_complete() {
        let fixtures = [
            (
                QueryLanguage::Sql,
                "SELECT 'value' FROM users -- note\nWHERE id = 42",
                [
                    ("SELECT", SyntaxClass::Keyword),
                    ("'value'", SyntaxClass::Literal),
                    ("-- note", SyntaxClass::Comment),
                    ("42", SyntaxClass::Number),
                ],
            ),
            (
                QueryLanguage::RedisCommand,
                "GET \"key\"\nSCAN 0 MATCH user:* # note",
                [
                    ("GET", SyntaxClass::Keyword),
                    ("\"key\"", SyntaxClass::Literal),
                    ("MATCH", SyntaxClass::Keyword),
                    ("# note", SyntaxClass::Comment),
                ],
            ),
            (
                QueryLanguage::MongoDocument,
                "{ active: true, count: 42 } // note",
                [
                    ("true", SyntaxClass::Keyword),
                    ("42", SyntaxClass::Number),
                    ("// note", SyntaxClass::Comment),
                    ("active", SyntaxClass::Plain),
                ],
            ),
        ];

        for (language, source, expected) in fixtures {
            let spans = syntax_spans(language, source);
            let mut covered = 0_usize;
            let mut reconstructed = String::new();
            for span in &spans {
                assert_eq!(span.byte_range.start, covered);
                covered = span.byte_range.end;
                reconstructed.push_str(&source[span.byte_range.clone()]);
            }
            assert_eq!(covered, source.len());
            assert_eq!(reconstructed, source);
            for (needle, class) in expected {
                assert!(
                    span_has(source, &spans, needle, class),
                    "missing {class:?} span {needle:?} for {language:?}"
                );
            }
        }

        let unterminated = "SELECT '한글 é";
        let unterminated_spans = syntax_spans(QueryLanguage::Sql, unterminated);
        assert!(span_has(
            unterminated,
            &unterminated_spans,
            "'한글 é",
            SyntaxClass::Literal
        ));
        assert_eq!(
            unterminated_spans
                .iter()
                .map(|span| &unterminated[span.byte_range.clone()])
                .collect::<String>(),
            unterminated
        );
        assert_eq!(
            syntax_status_value(QueryLanguage::Sql, "-- note\nSELECT 42, 'value'"),
            "SQL syntax · keywords 1 · literals 1 · numbers 1 · comments 1"
        );

        let keyword = syntax_format(egui::FontId::monospace(14.0), SyntaxClass::Keyword);
        let comment = syntax_format(egui::FontId::monospace(14.0), SyntaxClass::Comment);
        for format in [&keyword, &comment] {
            assert_eq!(
                (format.color.r(), format.color.g(), format.color.b()),
                (0, 0, 0)
            );
            assert_eq!(format.coords.as_ref().len(), 1);
            assert_eq!(format.background, egui::Color32::TRANSPARENT);
        }
        assert!(keyword.color.a() > comment.color.a());
        assert_ne!(keyword.coords, comment.coords);
        assert!(
            contrast_ratio_on_white(comment.color) >= 4.5,
            "comment text must preserve WCAG AA contrast on the white editor"
        );
    }

    #[test]
    fn selected_persisted_tab_language_overrides_profile_driver_fallback() {
        let mut workspace = ProfileWorkspace::default();
        assert_eq!(
            workspace_query_language(&workspace, DriverKind::MySql),
            QueryLanguage::Sql
        );
        let tab_id = workspace
            .create_editor_tab(QueryLanguage::MongoDocument, "Mongo", "{ ping: 1 }")
            .expect("bounded persisted editor tab");
        assert_eq!(workspace.selected_editor_tab_id(), Some(tab_id));
        assert_eq!(
            workspace_query_language(&workspace, DriverKind::MySql),
            QueryLanguage::MongoDocument
        );
    }

    #[test]
    fn history_run_target_preserves_current_selection_and_all_intent_shapes() {
        assert_eq!(
            run_target_for_cursor(&EditorCursor::caret(3)),
            WorkspaceRunTarget::Current
        );
        assert_eq!(
            run_target_for_cursor(&EditorCursor::with_selection(7, 2..7)),
            WorkspaceRunTarget::Selection
        );
        assert_eq!(
            run_target_for_cursor(&EditorCursor::with_selection(7, 7..7)),
            WorkspaceRunTarget::Current
        );
        assert_eq!(
            WorkspaceRunTarget::All,
            EditorExecuteBatchIntent {
                profile_id: ProfileId("typed-target".to_owned()),
                profile_generation: ProfileGeneration(1),
                editor_tab_id: None,
                language: QueryLanguage::Sql,
                text: "SELECT 1".to_owned(),
                target_count: 1,
                row_limit: 1,
                timeout_ms: 1_000,
                operation_kind: OperationKind::ExecuteRead,
                run_target: WorkspaceRunTarget::All,
            }
            .run_target()
        );
    }

    #[test]
    fn catalog_autocomplete_filters_and_stops_at_the_closed_candidate_bound() {
        let profile_id = ProfileId("profile-a".to_owned());
        let page = catalog_page(
            &profile_id,
            (0..30).map(|index| format!("Account_{index:02}")),
        );
        let candidates = catalog_candidates(
            &page,
            &profile_id,
            ProfileGeneration(7),
            QueryLanguage::Sql,
            "account_",
        );

        assert_eq!(candidates.len(), MAX_AUTOCOMPLETE_CANDIDATES);
        assert!(
            candidates
                .iter()
                .all(|candidate| candidate.display.starts_with("Account_"))
        );
        let debug = format!("{:?}", candidates[0]);
        assert!(!debug.contains("Account_00"));
        assert!(debug.contains("<redacted>"));

        let mut scan_names = vec!["ignored".to_owned(); MAX_AUTOCOMPLETE_CATALOG_NODES_SCANNED + 1];
        scan_names[MAX_AUTOCOMPLETE_CATALOG_NODES_SCANNED - 1] = "scan_inside_bound".to_owned();
        scan_names[MAX_AUTOCOMPLETE_CATALOG_NODES_SCANNED] = "scan_outside_bound".to_owned();
        let scan_page = catalog_page(&profile_id, scan_names);
        let scan_candidates = catalog_candidates(
            &scan_page,
            &profile_id,
            ProfileGeneration(7),
            QueryLanguage::Sql,
            "scan_",
        );
        assert_eq!(scan_candidates.len(), 1);
        assert_eq!(scan_candidates[0].display, "scan_inside_bound");

        let mismatched = catalog_candidates(
            &page,
            &profile_id,
            ProfileGeneration(8),
            QueryLanguage::Sql,
            "",
        );
        assert!(mismatched.is_empty());
        let mut stale = page.clone();
        stale.stale = true;
        assert!(
            catalog_candidates(
                &stale,
                &profile_id,
                ProfileGeneration(7),
                QueryLanguage::Sql,
                ""
            )
            .is_empty()
        );
        let exact_token = "x".repeat(MAX_AUTOCOMPLETE_TOKEN_BYTES);
        assert_eq!(
            autocomplete_token(&exact_token, exact_token.chars().count())
                .map(|token| token.prefix.len()),
            Some(MAX_AUTOCOMPLETE_TOKEN_BYTES)
        );
        assert!(
            autocomplete_token(
                &"x".repeat(MAX_AUTOCOMPLETE_TOKEN_BYTES + 1),
                MAX_AUTOCOMPLETE_TOKEN_BYTES + 1
            )
            .is_none()
        );
    }

    #[test]
    fn autocomplete_replacement_preserves_unicode_character_caret_and_source_bound() {
        let mut source = "SELECT 'é' FROM acc".to_owned();
        let caret = source.chars().count();
        let token = autocomplete_token(&source, caret).expect("bounded token");
        let next_caret =
            replace_autocomplete_token(&mut source, token.replacement_character_range, "accounts")
                .expect("bounded insertion");
        assert_eq!(source, "SELECT 'é' FROM accounts");
        assert_eq!(next_caret, source.chars().count());
        assert_eq!(
            catalog_insertion(QueryLanguage::Sql, "accounts"),
            Some("`accounts`".to_owned())
        );
        assert_eq!(
            catalog_insertion(QueryLanguage::Sql, "select"),
            Some("`select`".to_owned())
        );
        assert_eq!(
            catalog_insertion(QueryLanguage::Sql, "odd`name"),
            Some("`odd``name`".to_owned())
        );

        let mut exact = "x".repeat(MAX_AUTOCOMPLETE_SOURCE_BYTES);
        let retained = exact.clone();
        assert!(replace_autocomplete_token(&mut exact, 0..0, "y").is_none());
        assert_eq!(exact, retained);
        assert_eq!(replace_autocomplete_token(&mut exact, 0..1, "y"), Some(1));
        assert_eq!(exact.len(), MAX_AUTOCOMPLETE_SOURCE_BYTES);
    }

    #[test]
    fn accepting_autocomplete_changes_only_source_and_character_caret() {
        let context = egui::Context::default();
        let editor_id = egui::Id::new("focused-autocomplete-test");
        let profile_id = ProfileId("profile-a".to_owned());
        let page = catalog_page(&profile_id, ["accounts".to_owned()]);
        let mut workspace = ProfileWorkspace::default();
        workspace.editor_text = "SELECT acc".to_owned();
        workspace.caret_character_index = 10;
        workspace.row_limit = "91".to_owned();
        workspace.timeout_seconds = "17".to_owned();
        workspace.catalog_page = Some(page.clone());
        let mut surface = EditorSurface {
            autocomplete: AutocompleteState {
                candidates: vec![AutocompleteCandidate {
                    display: "accounts".to_owned(),
                    insertion: "`accounts`".to_owned(),
                    kind: CatalogNodeKind::Table,
                }],
                selected: 0,
                replacement_character_range: 7..10,
                caret_character_index: 10,
                catalog_source: Some(AutocompleteCatalogSource {
                    identity: page.identity,
                    loaded_at: page.loaded_at,
                }),
                open: true,
            },
            ..EditorSurface::default()
        };

        let outcome = surface.accept_autocomplete(&context, editor_id, &mut workspace);

        assert_eq!(outcome, AutocompleteAcceptOutcome::Accepted);
        assert_eq!(workspace.editor_text, "SELECT `accounts`");
        assert_eq!(workspace.caret_character_index, 17);
        assert_eq!(workspace.selection_character_range, None);
        assert_eq!(workspace.row_limit, "91");
        assert_eq!(workspace.timeout_seconds, "17");
        assert_eq!(workspace.pending_execute, None);
        assert!(!surface.autocomplete.open);
    }

    #[test]
    fn autocomplete_keyboard_requires_open_enabled_focused_editor() {
        assert!(autocomplete_keyboard_is_active(true, true, true));
        assert!(!autocomplete_keyboard_is_active(false, true, true));
        assert!(!autocomplete_keyboard_is_active(true, false, true));
        assert!(!autocomplete_keyboard_is_active(true, true, false));
    }

    #[test]
    fn autocomplete_accept_rejects_stale_identity_and_loaded_marker() {
        let context = egui::Context::default();
        let editor_id = egui::Id::new("stale-autocomplete-test");
        let profile_id = ProfileId("profile-a".to_owned());
        let page = catalog_page(&profile_id, ["accounts".to_owned()]);
        let source = AutocompleteCatalogSource {
            identity: page.identity.clone(),
            loaded_at: page.loaded_at.clone(),
        };
        assert!(autocomplete_catalog_is_current(Some(&source), Some(&page)));

        let mut stale_page = page.clone();
        stale_page.stale = true;
        assert!(!autocomplete_catalog_is_current(
            Some(&source),
            Some(&stale_page)
        ));
        let mut new_identity_page = page.clone();
        new_identity_page.identity.operation_id = OperationId(12);
        assert!(!autocomplete_catalog_is_current(
            Some(&source),
            Some(&new_identity_page)
        ));
        let mut new_loaded_page = page.clone();
        new_loaded_page.loaded_at = "replacement-load".to_owned();
        assert!(!autocomplete_catalog_is_current(
            Some(&source),
            Some(&new_loaded_page)
        ));

        let mut workspace = ProfileWorkspace::default();
        workspace.editor_text = "SELECT acc".to_owned();
        workspace.caret_character_index = 10;
        workspace.catalog_page = Some(stale_page);
        let mut surface = EditorSurface {
            autocomplete: AutocompleteState {
                candidates: vec![AutocompleteCandidate {
                    display: "accounts".to_owned(),
                    insertion: "`accounts`".to_owned(),
                    kind: CatalogNodeKind::Table,
                }],
                selected: 0,
                replacement_character_range: 7..10,
                caret_character_index: 10,
                catalog_source: Some(source),
                open: true,
            },
            ..EditorSurface::default()
        };

        let outcome = surface.accept_autocomplete(&context, editor_id, &mut workspace);

        assert_eq!(outcome, AutocompleteAcceptOutcome::Rejected);
        assert_eq!(workspace.editor_text, "SELECT acc");
        assert_eq!(workspace.caret_character_index, 10);
        assert!(!surface.autocomplete.open);
    }
}
