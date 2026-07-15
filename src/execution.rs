use std::fmt;
use std::ops::Range;

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

#[derive(Clone, PartialEq, Eq)]
pub struct ValidatedExecutionTarget {
    target: ExecutionTarget,
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
}

impl fmt::Debug for ValidatedExecutionTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ValidatedExecutionTarget")
            .field("target", &self.target)
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

    let caret_byte = char_to_byte(editor_text, caret_character_index)
        .ok_or(ExecutionTargetError::InvalidCaretPosition)?;
    let selected = selection_character_range
        .map(|range| checked_character_slice(editor_text, range))
        .transpose()?
        .map(str::trim);

    let target = match (language, selected) {
        (ExecutionLanguage::MySql, Some(selection)) => {
            validate_mysql_selection(selection)?;
            ExecutionTarget::MySqlText(selection.to_owned())
        }
        (ExecutionLanguage::MySql, None) => {
            let target = infer_mysql_target(editor_text, caret_byte)?;
            ExecutionTarget::MySqlText(target.to_owned())
        }
        (ExecutionLanguage::Redis, Some(selection)) => {
            validate_single_redis_physical_command(selection)?;
            ExecutionTarget::RedisArgv(validate_redis_target(selection)?)
        }
        (ExecutionLanguage::Redis, None) => {
            let line = physical_line_at(editor_text, caret_byte).trim();
            ExecutionTarget::RedisArgv(validate_redis_target(line)?)
        }
    };

    Ok(ValidatedExecutionTarget {
        target,
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
                Some(quoted) => {
                    pending.mark_executable(index, quoted.end);
                    pending.ambiguous_sql_mode |=
                        preceding_odd_backslash || quoted.ambiguous_sql_mode;
                    index = quoted.end;
                }
                None => {
                    pending.ambiguous_sql_mode |= preceding_odd_backslash;
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

fn quoted_span(text: &str, start: usize, delimiter: u8) -> Option<QuotedSpan> {
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
            return Some(QuotedSpan {
                end: index + 1,
                ambiguous_sql_mode,
            });
        }
        index = next_char_boundary(text, index);
    }

    None
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
    if scan.issue.is_some() {
        return Err(ExecutionTargetError::UnterminatedSqlToken);
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
