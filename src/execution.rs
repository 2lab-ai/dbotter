use std::fmt;
use std::ops::Range;

use crate::model::OperationKind;

pub const MAX_EXECUTE_ROW_LIMIT: u32 = 10_000;
pub const MAX_EXECUTE_TIMEOUT_SECONDS: u32 = 300;
pub const MAX_REDIS_TARGET_BYTES: usize = 65_536;
pub const MAX_REDIS_TOKENS: usize = 1_024;
pub const MAX_REDIS_TOKEN_BYTES: usize = 16 * 1_024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecutionLanguage {
    MySql,
    Redis,
}

#[derive(Clone, PartialEq, Eq)]
pub enum ExecutionTarget {
    MySqlText(String),
    RedisArgv(Vec<String>),
}

impl fmt::Debug for ExecutionTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MySqlText(_) => formatter
                .debug_tuple("MySqlText")
                .field(&"<redacted>")
                .finish(),
            Self::RedisArgv(_) => formatter
                .debug_tuple("RedisArgv")
                .field(&"<redacted>")
                .finish(),
        }
    }
}

/// Classifies only a closed, clearly read-only set as `ExecuteRead`.
/// Language/target mismatches and unknown or side-effecting forms fail closed.
#[must_use]
pub fn classify_execution_kind(
    language: ExecutionLanguage,
    target: &ExecutionTarget,
) -> OperationKind {
    let is_read = match (language, target) {
        (ExecutionLanguage::MySql, ExecutionTarget::MySqlText(text)) => {
            mysql_is_clearly_read_only(text)
        }
        (ExecutionLanguage::Redis, ExecutionTarget::RedisArgv(arguments)) => {
            redis_is_clearly_read_only(arguments)
        }
        _ => false,
    };
    if is_read {
        OperationKind::ExecuteRead
    } else {
        OperationKind::ExecuteMutation
    }
}

fn mysql_is_clearly_read_only(text: &str) -> bool {
    classify_mysql_execution_kind_with_sql_mode(text, "") == Some(OperationKind::ExecuteRead)
}

/// Classifies one already-bounded MySQL statement with the exact session mode.
/// `None` means the server returned an undecodable `sql_mode` capability.
#[must_use]
pub fn classify_mysql_execution_kind_with_sql_mode(
    text: &str,
    sql_mode: &str,
) -> Option<OperationKind> {
    let mode = MysqlClassifierMode::decode(sql_mode)?;
    Some(if mysql_statement_is_read_only(text, mode) {
        OperationKind::ExecuteRead
    } else {
        OperationKind::ExecuteMutation
    })
}

/// Rejects executable-comment marker bytes before any session or mode lookup.
/// The scan is intentionally quote-agnostic because MySQL/MariaDB can change
/// lexical interpretation through session modes and version-comment rules.
#[must_use]
pub fn mysql_has_forbidden_executable_comment(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.windows(3).any(|window| window == b"/*!")
        || bytes
            .windows(4)
            .any(|window| matches!(window, b"/*M!" | b"/*m!"))
}

/// Reports whether any relevant exact session mode can classify the target as
/// a read. This keeps the pre-capability UI/service posture conservative
/// without mislabeling mode-dependent SELECTs as data changes.
#[must_use]
pub fn mysql_may_be_read_with_session_mode(text: &str) -> bool {
    if mysql_has_forbidden_executable_comment(text) {
        return false;
    }
    [
        "",
        "ANSI_QUOTES",
        "NO_BACKSLASH_ESCAPES",
        "ANSI_QUOTES,NO_BACKSLASH_ESCAPES",
    ]
    .into_iter()
    .any(|sql_mode| {
        classify_mysql_execution_kind_with_sql_mode(text, sql_mode)
            == Some(OperationKind::ExecuteRead)
    })
}

#[derive(Clone, Copy, Default)]
struct MysqlClassifierMode {
    ansi_quotes: bool,
    no_backslash_escapes: bool,
}

impl MysqlClassifierMode {
    fn decode(sql_mode: &str) -> Option<Self> {
        if sql_mode.is_empty() {
            return Some(Self::default());
        }
        let mut decoded = Self::default();
        for token in sql_mode.split(',') {
            if token.is_empty()
                || !token
                    .bytes()
                    .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
            {
                return None;
            }
            match token {
                "ANSI" | "ANSI_QUOTES" => decoded.ansi_quotes = true,
                "NO_BACKSLASH_ESCAPES" => decoded.no_backslash_escapes = true,
                "IGNORE_SPACE" => {}
                _ => {}
            }
        }
        Some(decoded)
    }
}

fn mysql_statement_is_read_only(text: &str, mode: MysqlClassifierMode) -> bool {
    let text = text.trim();
    let text = text.strip_suffix(';').unwrap_or(text).trim_end();
    if text.is_empty() {
        return false;
    }
    let Some(words) = mysql_classifier_words(text, mode) else {
        return false;
    };
    let Some(first) = words.first() else {
        return false;
    };
    if !first.eq_ignore_ascii_case("SELECT") && !first.eq_ignore_ascii_case("WITH") {
        return false;
    }
    if first.eq_ignore_ascii_case("WITH")
        && !words.iter().any(|word| word.eq_ignore_ascii_case("SELECT"))
    {
        return false;
    }

    !words.iter().any(|word| {
        [
            "ALTER",
            "ANALYZE",
            "BEGIN",
            "CALL",
            "COMMIT",
            "CREATE",
            "DELETE",
            "DESC",
            "DESCRIBE",
            "DO",
            "DROP",
            "DUMPFILE",
            "EXPLAIN",
            "GRANT",
            "HANDLER",
            "INSERT",
            "INSTALL",
            "INTO",
            "LOAD",
            "LOCK",
            "PROCEDURE",
            "RELEASE",
            "RENAME",
            "REPLACE",
            "REVOKE",
            "ROLLBACK",
            "SAVEPOINT",
            "SET",
            "SHARE",
            "SHOW",
            "START",
            "TRUNCATE",
            "UNINSTALL",
            "UPDATE",
            "XA",
        ]
        .iter()
        .any(|forbidden| word.eq_ignore_ascii_case(forbidden))
    })
}

fn mysql_classifier_words(text: &str, mode: MysqlClassifierMode) -> Option<Vec<&str>> {
    let bytes = text.as_bytes();
    let mut words = Vec::new();
    let mut index = 0_usize;
    while index < bytes.len() {
        if bytes[index] == b'#' || mysql_dash_comment_starts(bytes, index) {
            index = line_comment_end(bytes, index);
            continue;
        }
        if bytes[index..].starts_with(b"/*") {
            if bytes[index..].starts_with(b"/*!")
                || bytes[index..].starts_with(b"/*M!")
                || bytes[index..].starts_with(b"/*m!")
            {
                return None;
            }
            index = block_comment_end(bytes, index + 2)?;
            continue;
        }
        let delimiter = bytes[index];
        if matches!(delimiter, b'\'' | b'"' | b'`') {
            index = mysql_classifier_quote_end(bytes, index, delimiter, mode)?;
            continue;
        }
        if bytes[index..].starts_with(b":=") || bytes[index] == b';' {
            return None;
        }
        if bytes[index].is_ascii_alphabetic() || bytes[index] == b'_' {
            let start = index;
            index += 1;
            while bytes
                .get(index)
                .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
            {
                index += 1;
            }
            words.push(&text[start..index]);
            continue;
        }
        index += 1;
    }
    Some(words)
}

fn mysql_classifier_quote_end(
    bytes: &[u8],
    start: usize,
    delimiter: u8,
    mode: MysqlClassifierMode,
) -> Option<usize> {
    let quoted_identifier = delimiter == b'`' || (delimiter == b'"' && mode.ansi_quotes);
    let mut index = start + 1;
    while index < bytes.len() {
        if bytes[index] == delimiter {
            if bytes.get(index + 1) == Some(&delimiter) {
                index += 2;
                continue;
            }
            return Some(index + 1);
        }
        if bytes[index] == b'\\' && !quoted_identifier && !mode.no_backslash_escapes {
            index = index.saturating_add(2);
            continue;
        }
        index += 1;
    }
    None
}

fn redis_is_clearly_read_only(arguments: &[String]) -> bool {
    let Some(command) = arguments.first() else {
        return false;
    };
    matches!(
        command.to_ascii_uppercase().as_str(),
        "PING"
            | "ECHO"
            | "GET"
            | "MGET"
            | "EXISTS"
            | "TYPE"
            | "TTL"
            | "PTTL"
            | "STRLEN"
            | "GETRANGE"
            | "BITCOUNT"
            | "HGET"
            | "HMGET"
            | "HGETALL"
            | "HEXISTS"
            | "HLEN"
            | "HKEYS"
            | "HVALS"
            | "LLEN"
            | "LINDEX"
            | "LRANGE"
            | "SCARD"
            | "SISMEMBER"
            | "SMISMEMBER"
            | "SMEMBERS"
            | "ZCARD"
            | "ZCOUNT"
            | "ZSCORE"
            | "ZMSCORE"
            | "ZRANGE"
            | "ZRANGEBYSCORE"
            | "ZREVRANGE"
            | "ZRANK"
            | "ZREVRANK"
            | "XLEN"
            | "XRANGE"
            | "XREVRANGE"
            | "SCAN"
            | "SSCAN"
            | "HSCAN"
            | "ZSCAN"
            | "DBSIZE"
            | "INFO"
            | "TIME"
    )
}

#[derive(Clone, PartialEq, Eq)]
pub struct ValidatedExecutionTarget {
    target: ExecutionTarget,
    source_text: String,
    row_limit: u32,
    timeout_seconds: u32,
}

impl ValidatedExecutionTarget {
    #[must_use]
    pub fn target(&self) -> &ExecutionTarget {
        &self.target
    }

    #[must_use]
    pub const fn row_limit(&self) -> u32 {
        self.row_limit
    }

    #[must_use]
    pub const fn timeout_seconds(&self) -> u32 {
        self.timeout_seconds
    }

    #[must_use]
    pub fn into_target(self) -> ExecutionTarget {
        self.target
    }

    /// The exact trimmed source span selected by the scanner.
    /// MySQL shares the target allocation; Redis retains this before argv parsing.
    #[must_use]
    pub fn source_text(&self) -> &str {
        match &self.target {
            ExecutionTarget::MySqlText(text) => text,
            ExecutionTarget::RedisArgv(_) => &self.source_text,
        }
    }

    #[must_use]
    pub fn into_source_text(self) -> String {
        match self.target {
            ExecutionTarget::MySqlText(text) => text,
            ExecutionTarget::RedisArgv(_) => self.source_text,
        }
    }
}

impl fmt::Debug for ValidatedExecutionTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ValidatedExecutionTarget")
            .field("target", &self.target)
            .field("source_text", &"<redacted>")
            .field("row_limit", &self.row_limit)
            .field("timeout_seconds", &self.timeout_seconds)
            .finish()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ExecutionTargetError {
    InvalidCaretPosition,
    InvalidSelectionRange,
    InvalidRowLimit,
    InvalidTimeout,
    NoCurrentStatement,
    MultipleStatements,
    AmbiguousSqlMode,
    UnterminatedSqlToken,
    ForbiddenExecutableComment,
    RedisShellParseFailed,
    RedisCommandDenied,
    RedisTargetTooLarge,
    RedisTooManyTokens,
    RedisTokenTooLarge,
}

impl ExecutionTargetError {
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::InvalidCaretPosition => "INVALID_CARET_POSITION",
            Self::InvalidSelectionRange => "INVALID_SELECTION_RANGE",
            Self::InvalidRowLimit => "INVALID_EXECUTE_ROW_LIMIT",
            Self::InvalidTimeout => "INVALID_EXECUTE_TIMEOUT",
            Self::NoCurrentStatement => "NO_CURRENT_STATEMENT",
            Self::MultipleStatements => "MULTIPLE_STATEMENTS",
            Self::AmbiguousSqlMode => "AMBIGUOUS_SQL_MODE",
            Self::UnterminatedSqlToken => "UNTERMINATED_SQL_TOKEN",
            Self::ForbiddenExecutableComment => "FORBIDDEN_EXECUTABLE_COMMENT",
            Self::RedisShellParseFailed => "REDIS_SHELL_PARSE_FAILED",
            Self::RedisCommandDenied => "REDIS_COMMAND_DENIED",
            Self::RedisTargetTooLarge => "REDIS_TARGET_TOO_LARGE",
            Self::RedisTooManyTokens => "REDIS_TOO_MANY_TOKENS",
            Self::RedisTokenTooLarge => "REDIS_TOKEN_TOO_LARGE",
        }
    }

    #[must_use]
    pub const fn summary(self) -> &'static str {
        match self {
            Self::InvalidCaretPosition => "The cursor position is invalid.",
            Self::InvalidSelectionRange => "The selection range is invalid.",
            Self::InvalidRowLimit => "The execute row limit must be between 1 and 10000.",
            Self::InvalidTimeout => "The execute timeout must be between 1 and 300 seconds.",
            Self::NoCurrentStatement => "No executable statement is selected.",
            Self::MultipleStatements => "Execute accepts exactly one statement.",
            Self::AmbiguousSqlMode => {
                "The SQL boundary depends on SQL mode; select the exact statement."
            }
            Self::UnterminatedSqlToken => "The SQL target contains an unterminated token.",
            Self::ForbiddenExecutableComment => {
                "MySQL and MariaDB executable comments are not allowed."
            }
            Self::RedisShellParseFailed => "The Redis command could not be parsed.",
            Self::RedisCommandDenied => "The Redis command is not allowed for local execution.",
            Self::RedisTargetTooLarge => "The Redis command exceeds the input limit.",
            Self::RedisTooManyTokens => "The Redis command has too many tokens.",
            Self::RedisTokenTooLarge => "A Redis command token exceeds the input limit.",
        }
    }
}

impl fmt::Debug for ExecutionTargetError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExecutionTargetError")
            .field("code", &self.code())
            .finish()
    }
}

impl fmt::Display for ExecutionTargetError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.summary())
    }
}

impl std::error::Error for ExecutionTargetError {}

pub fn extract_and_validate_target(
    editor_text: &str,
    caret_character_index: usize,
    selection_character_range: Option<Range<usize>>,
    language: ExecutionLanguage,
    row_limit: u32,
    timeout_seconds: u32,
) -> Result<ValidatedExecutionTarget, ExecutionTargetError> {
    if !(1..=MAX_EXECUTE_ROW_LIMIT).contains(&row_limit) {
        return Err(ExecutionTargetError::InvalidRowLimit);
    }
    if !(1..=MAX_EXECUTE_TIMEOUT_SECONDS).contains(&timeout_seconds) {
        return Err(ExecutionTargetError::InvalidTimeout);
    }
    if language == ExecutionLanguage::MySql && mysql_has_forbidden_executable_comment(editor_text) {
        return Err(ExecutionTargetError::ForbiddenExecutableComment);
    }

    let caret_byte = char_to_byte(editor_text, caret_character_index)
        .ok_or(ExecutionTargetError::InvalidCaretPosition)?;
    let selected = selection_character_range
        .map(|range| checked_character_slice(editor_text, range))
        .transpose()?
        .map(str::trim);

    let (target, source_text) = match (language, selected) {
        (ExecutionLanguage::MySql, Some(selection)) => {
            validate_mysql_selection(selection)?;
            (
                ExecutionTarget::MySqlText(selection.to_owned()),
                String::new(),
            )
        }
        (ExecutionLanguage::MySql, None) => {
            let target = infer_mysql_target(editor_text, caret_byte)?;
            (ExecutionTarget::MySqlText(target.to_owned()), String::new())
        }
        (ExecutionLanguage::Redis, Some(selection)) => {
            validate_single_redis_physical_command(selection)?;
            (
                ExecutionTarget::RedisArgv(validate_redis_target(selection)?),
                selection.to_owned(),
            )
        }
        (ExecutionLanguage::Redis, None) => {
            let line = physical_line_at(editor_text, caret_byte).trim();
            (
                ExecutionTarget::RedisArgv(validate_redis_target(line)?),
                line.to_owned(),
            )
        }
    };

    Ok(ValidatedExecutionTarget {
        target,
        source_text,
        row_limit,
        timeout_seconds,
    })
}

fn char_to_byte(text: &str, character_index: usize) -> Option<usize> {
    if character_index == text.chars().count() {
        return Some(text.len());
    }
    text.char_indices()
        .nth(character_index)
        .map(|(byte_index, _)| byte_index)
}

fn checked_character_slice(text: &str, range: Range<usize>) -> Result<&str, ExecutionTargetError> {
    if range.start > range.end {
        return Err(ExecutionTargetError::InvalidSelectionRange);
    }
    let start =
        char_to_byte(text, range.start).ok_or(ExecutionTargetError::InvalidSelectionRange)?;
    let end = char_to_byte(text, range.end).ok_or(ExecutionTargetError::InvalidSelectionRange)?;
    text.get(start..end)
        .ok_or(ExecutionTargetError::InvalidSelectionRange)
}

#[derive(Default)]
struct PendingMysqlStatement {
    first_executable: Option<usize>,
    last_executable_end: usize,
    ambiguous_sql_mode: bool,
}

impl PendingMysqlStatement {
    fn mark_executable(&mut self, start: usize, end: usize) {
        if self.first_executable.is_none() {
            self.first_executable = Some(start);
        }
        self.last_executable_end = end;
    }

    fn finish(&mut self, end: usize) -> Option<MysqlStatement> {
        let start = self.first_executable?;
        let statement = MysqlStatement {
            range: start..end,
            ambiguous_sql_mode: self.ambiguous_sql_mode,
        };
        *self = Self::default();
        Some(statement)
    }
}

struct MysqlStatement {
    range: Range<usize>,
    ambiguous_sql_mode: bool,
}

struct MysqlLexIssue {
    statement_start: usize,
    ambiguous_sql_mode: bool,
}

struct MysqlScan {
    statements: Vec<MysqlStatement>,
    issue: Option<MysqlLexIssue>,
    stray_terminator: bool,
}

struct QuotedSpan {
    end: usize,
    ambiguous_sql_mode: bool,
}

fn scan_mysql(text: &str) -> MysqlScan {
    let bytes = text.as_bytes();
    let mut statements = Vec::new();
    let mut pending = PendingMysqlStatement::default();
    let mut stray_terminator = false;
    let mut index = 0;

    while index < bytes.len() {
        if bytes[index] == b'#' || mysql_dash_comment_starts(bytes, index) {
            index = line_comment_end(bytes, index);
            continue;
        }

        if bytes[index..].starts_with(b"/*") {
            let executable_version_comment = bytes.get(index + 2) == Some(&b'!');
            let Some(end) = block_comment_end(bytes, index + 2) else {
                return MysqlScan {
                    statements,
                    issue: Some(MysqlLexIssue {
                        statement_start: pending.first_executable.unwrap_or(index),
                        ambiguous_sql_mode: pending.ambiguous_sql_mode,
                    }),
                    stray_terminator,
                };
            };
            if executable_version_comment {
                pending.mark_executable(index, end);
            }
            index = end;
            continue;
        }

        let byte = bytes[index];
        if matches!(byte, b'\'' | b'"' | b'`') {
            pending.mark_executable(index, index + 1);
            let preceding_odd_backslash = byte != b'`' && odd_backslash_run_before(bytes, index);
            match quoted_span(text, index, byte) {
                Ok(quoted) => {
                    pending.mark_executable(index, quoted.end);
                    pending.ambiguous_sql_mode |=
                        preceding_odd_backslash || quoted.ambiguous_sql_mode;
                    index = quoted.end;
                }
                Err(ambiguous_sql_mode) => {
                    pending.ambiguous_sql_mode |= preceding_odd_backslash || ambiguous_sql_mode;
                    return MysqlScan {
                        statements,
                        issue: Some(MysqlLexIssue {
                            statement_start: pending.first_executable.unwrap_or(index),
                            ambiguous_sql_mode: pending.ambiguous_sql_mode,
                        }),
                        stray_terminator,
                    };
                }
            }
            continue;
        }

        if byte == b';' {
            if let Some(statement) = pending.finish(index + 1) {
                statements.push(statement);
            } else {
                stray_terminator = true;
            }
            index += 1;
            continue;
        }

        let end = next_char_boundary(text, index);
        if !text[index..end].chars().all(char::is_whitespace) {
            pending.mark_executable(index, end);
        }
        index = end;
    }

    if let Some(statement) = pending.finish(pending.last_executable_end) {
        statements.push(statement);
    }

    MysqlScan {
        statements,
        issue: None,
        stray_terminator,
    }
}

fn mysql_dash_comment_starts(bytes: &[u8], index: usize) -> bool {
    bytes.get(index..index + 2) == Some(b"--")
        && bytes
            .get(index + 2)
            .is_some_and(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
}

fn line_comment_end(bytes: &[u8], start: usize) -> usize {
    bytes[start..]
        .iter()
        .position(|byte| *byte == b'\n')
        .map_or(bytes.len(), |offset| start + offset)
}

fn block_comment_end(bytes: &[u8], search_start: usize) -> Option<usize> {
    bytes
        .get(search_start..)?
        .windows(2)
        .position(|window| window == b"*/")
        .map(|offset| search_start + offset + 2)
}

fn quoted_span(text: &str, start: usize, delimiter: u8) -> Result<QuotedSpan, bool> {
    let bytes = text.as_bytes();
    let mut index = start + 1;
    let mut ambiguous_sql_mode = false;

    while index < bytes.len() {
        if bytes[index] == b'\\' {
            let run_start = index;
            while bytes.get(index) == Some(&b'\\') {
                index += 1;
            }
            let odd_run = (index - run_start) % 2 == 1;
            if index >= bytes.len() {
                break;
            }
            if bytes[index] == delimiter && delimiter != b'`' && odd_run {
                ambiguous_sql_mode = true;
            }
            if odd_run {
                index = next_char_boundary(text, index);
            }
            continue;
        }

        if bytes[index] == delimiter {
            if bytes.get(index + 1) == Some(&delimiter) {
                index += 2;
                continue;
            }
            return Ok(QuotedSpan {
                end: index + 1,
                ambiguous_sql_mode,
            });
        }
        index = next_char_boundary(text, index);
    }

    Err(ambiguous_sql_mode)
}

fn odd_backslash_run_before(bytes: &[u8], index: usize) -> bool {
    bytes[..index]
        .iter()
        .rev()
        .take_while(|byte| **byte == b'\\')
        .count()
        % 2
        == 1
}

fn next_char_boundary(text: &str, byte_index: usize) -> usize {
    text[byte_index..]
        .char_indices()
        .nth(1)
        .map_or(text.len(), |(offset, _)| byte_index + offset)
}

fn validate_mysql_selection(selection: &str) -> Result<(), ExecutionTargetError> {
    if selection.is_empty() {
        return Err(ExecutionTargetError::NoCurrentStatement);
    }
    let scan = scan_mysql(selection);
    if let Some(issue) = scan.issue {
        return if issue.ambiguous_sql_mode {
            Ok(())
        } else {
            Err(ExecutionTargetError::UnterminatedSqlToken)
        };
    }
    match scan.statements.len() {
        0 => Err(ExecutionTargetError::NoCurrentStatement),
        1 if !scan.stray_terminator => Ok(()),
        _ => Err(ExecutionTargetError::MultipleStatements),
    }
}

fn infer_mysql_target(text: &str, caret_byte: usize) -> Result<&str, ExecutionTargetError> {
    let scan = scan_mysql(text);
    if let Some(statement) = scan
        .statements
        .iter()
        .find(|statement| mysql_statement_contains(statement, caret_byte, text.len()))
    {
        if statement.ambiguous_sql_mode {
            return Err(ExecutionTargetError::AmbiguousSqlMode);
        }
        return text
            .get(statement.range.clone())
            .ok_or(ExecutionTargetError::InvalidCaretPosition);
    }

    if let Some(issue) = scan
        .issue
        .filter(|issue| caret_byte >= issue.statement_start)
    {
        return if issue.ambiguous_sql_mode {
            Err(ExecutionTargetError::AmbiguousSqlMode)
        } else {
            Err(ExecutionTargetError::UnterminatedSqlToken)
        };
    }

    Err(ExecutionTargetError::NoCurrentStatement)
}

fn mysql_statement_contains(statement: &MysqlStatement, caret: usize, text_len: usize) -> bool {
    statement.range.contains(&caret)
        || (caret == text_len && statement.range.end == text_len && caret >= statement.range.start)
}

fn physical_line_at(text: &str, caret_byte: usize) -> &str {
    let start = text[..caret_byte]
        .rfind('\n')
        .map_or(0, |position| position + 1);
    let end = text[caret_byte..]
        .find('\n')
        .map_or(text.len(), |offset| caret_byte + offset);
    &text[start..end]
}

fn validate_single_redis_physical_command(selection: &str) -> Result<(), ExecutionTargetError> {
    if selection.is_empty() {
        return Err(ExecutionTargetError::NoCurrentStatement);
    }
    if selection.len() > MAX_REDIS_TARGET_BYTES {
        return Err(ExecutionTargetError::RedisTargetTooLarge);
    }
    let mut command_lines = 0_usize;
    for line in selection.split('\n') {
        let argv = shell_words::split(line.trim_end_matches('\r'))
            .map_err(|_| ExecutionTargetError::RedisShellParseFailed)?;
        if !argv.is_empty() {
            command_lines += 1;
            if command_lines > 1 {
                return Err(ExecutionTargetError::MultipleStatements);
            }
        }
    }
    Ok(())
}

fn validate_redis_target(target: &str) -> Result<Vec<String>, ExecutionTargetError> {
    if target.len() > MAX_REDIS_TARGET_BYTES {
        return Err(ExecutionTargetError::RedisTargetTooLarge);
    }
    let argv =
        shell_words::split(target).map_err(|_| ExecutionTargetError::RedisShellParseFailed)?;
    let Some(command) = argv.first() else {
        return Err(ExecutionTargetError::NoCurrentStatement);
    };
    if command.is_empty() {
        return Err(ExecutionTargetError::RedisShellParseFailed);
    }
    if argv.len() > MAX_REDIS_TOKENS {
        return Err(ExecutionTargetError::RedisTooManyTokens);
    }
    if argv.iter().any(|token| token.len() > MAX_REDIS_TOKEN_BYTES) {
        return Err(ExecutionTargetError::RedisTokenTooLarge);
    }
    if redis_command_is_denied(&argv) {
        return Err(ExecutionTargetError::RedisCommandDenied);
    }
    Ok(argv)
}

fn redis_command_is_denied(argv: &[String]) -> bool {
    let bytes = argv
        .iter()
        .map(|argument| argument.as_bytes())
        .collect::<Vec<_>>();
    redis_bytes_are_denied(&bytes)
}

pub(crate) fn redis_argv_is_denied(argv: &[Vec<u8>]) -> bool {
    let bytes = argv.iter().map(Vec::as_slice).collect::<Vec<_>>();
    redis_bytes_are_denied(&bytes)
}

fn redis_bytes_are_denied(argv: &[&[u8]]) -> bool {
    let Some(command) = argv.first().copied() else {
        return false;
    };
    const EXACT_DENY: [&str; 17] = [
        "SUBSCRIBE",
        "PSUBSCRIBE",
        "SSUBSCRIBE",
        "UNSUBSCRIBE",
        "PUNSUBSCRIBE",
        "SUNSUBSCRIBE",
        "MONITOR",
        "SYNC",
        "PSYNC",
        "REPLCONF",
        "WAIT",
        "WAITAOF",
        "BRPOP",
        "BRPOPLPUSH",
        "BZPOPMIN",
        "BZPOPMAX",
        "BZMPOP",
    ];

    let bl_prefix = command
        .get(..2)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"BL"));
    if bl_prefix {
        return true;
    }

    if command.eq_ignore_ascii_case(b"XREAD") || command.eq_ignore_ascii_case(b"XREADGROUP") {
        return block_precedes_streams(argv);
    }

    EXACT_DENY
        .iter()
        .any(|denied| command.eq_ignore_ascii_case(denied.as_bytes()))
}

fn block_precedes_streams(argv: &[&[u8]]) -> bool {
    for argument in argv.iter().skip(1) {
        if argument.eq_ignore_ascii_case(b"STREAMS") {
            return false;
        }
        if argument.eq_ignore_ascii_case(b"BLOCK") {
            return true;
        }
    }
    false
}
