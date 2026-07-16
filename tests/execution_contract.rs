use std::ops::Range;

use dbotter::execution::{
    ExecutionLanguage, ExecutionTarget, ExecutionTargetError, MAX_EXECUTE_BATCH_BYTES,
    MAX_EXECUTE_BATCH_TARGETS, MAX_EXECUTE_ROW_LIMIT, MAX_EXECUTE_TIMEOUT_SECONDS,
    MAX_MYSQL_NESTING_DEPTH, MAX_MYSQL_TOKENS, MAX_REDIS_TARGET_BYTES, MAX_REDIS_TOKEN_BYTES,
    MAX_REDIS_TOKENS, ValidatedExecutionTarget, classify_execution_batch_kind,
    extract_and_validate_all_targets, extract_and_validate_target,
};
use dbotter::model::{MAX_MYSQL_STATEMENT_BYTES, OperationKind};

const ROW_LIMIT: u32 = 500;
const TIMEOUT_SECONDS: u32 = 30;

fn character_index(text: &str, needle: &str) -> usize {
    let byte_index = text.find(needle).unwrap();
    text[..byte_index].chars().count()
}

fn mysql(
    text: &str,
    caret_character_index: usize,
) -> Result<ValidatedExecutionTarget, ExecutionTargetError> {
    extract_and_validate_target(
        text,
        caret_character_index,
        None,
        ExecutionLanguage::MySql,
        ROW_LIMIT,
        TIMEOUT_SECONDS,
    )
}

fn mysql_selection(
    text: &str,
    selection: Range<usize>,
) -> Result<ValidatedExecutionTarget, ExecutionTargetError> {
    extract_and_validate_target(
        text,
        text.chars().count(),
        Some(selection),
        ExecutionLanguage::MySql,
        ROW_LIMIT,
        TIMEOUT_SECONDS,
    )
}

fn redis(
    text: &str,
    caret_character_index: usize,
) -> Result<ValidatedExecutionTarget, ExecutionTargetError> {
    extract_and_validate_target(
        text,
        caret_character_index,
        None,
        ExecutionLanguage::Redis,
        ROW_LIMIT,
        TIMEOUT_SECONDS,
    )
}

fn redis_selection(
    text: &str,
    selection: Range<usize>,
) -> Result<ValidatedExecutionTarget, ExecutionTargetError> {
    extract_and_validate_target(
        text,
        text.chars().count(),
        Some(selection),
        ExecutionLanguage::Redis,
        ROW_LIMIT,
        TIMEOUT_SECONDS,
    )
}

#[test]
fn run_all_preflights_the_complete_mysql_batch_in_source_order() {
    let source = "SELECT 1;\nSELECT ';' AS value;\nSELECT 3";
    let batch = extract_and_validate_all_targets(
        source,
        ExecutionLanguage::MySql,
        ROW_LIMIT,
        TIMEOUT_SECONDS,
    )
    .expect("the complete read-only batch is valid");
    let targets = batch.targets();
    assert_eq!(targets.len(), 3);
    assert_eq!(targets[0].source_text(), "SELECT 1;");
    assert_eq!(targets[1].source_text(), "SELECT ';' AS value;");
    assert_eq!(targets[2].source_text(), "SELECT 3");
    assert_eq!(
        classify_execution_batch_kind(ExecutionLanguage::MySql, targets),
        OperationKind::ExecuteRead
    );

    let mutation = extract_and_validate_all_targets(
        "SELECT 1; UPDATE accounts SET enabled = 0;",
        ExecutionLanguage::MySql,
        ROW_LIMIT,
        TIMEOUT_SECONDS,
    )
    .expect("classification, not scanning, rejects mutation dispatch");
    assert_eq!(
        classify_execution_batch_kind(ExecutionLanguage::MySql, mutation.targets()),
        OperationKind::ExecuteMutation
    );
}

#[test]
fn run_all_rejects_ambiguous_empty_oversized_and_overcount_mysql_batches() {
    assert_eq!(
        extract_and_validate_all_targets(
            r"SELECT 'a\'b';",
            ExecutionLanguage::MySql,
            ROW_LIMIT,
            TIMEOUT_SECONDS,
        )
        .unwrap_err(),
        ExecutionTargetError::BatchAmbiguousSqlMode
    );
    assert_eq!(
        extract_and_validate_all_targets(
            "SELECT 1;; SELECT 2;",
            ExecutionLanguage::MySql,
            ROW_LIMIT,
            TIMEOUT_SECONDS,
        )
        .unwrap_err(),
        ExecutionTargetError::EmptyBatchTarget
    );
    assert_eq!(
        extract_and_validate_all_targets(
            "SELECT 1; /*!40101 SET @unsafe = 1 */;",
            ExecutionLanguage::MySql,
            ROW_LIMIT,
            TIMEOUT_SECONDS,
        )
        .unwrap_err(),
        ExecutionTargetError::ForbiddenExecutableComment
    );

    let overcount = (0..=MAX_EXECUTE_BATCH_TARGETS)
        .map(|index| format!("SELECT {index};"))
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(
        extract_and_validate_all_targets(
            &overcount,
            ExecutionLanguage::MySql,
            ROW_LIMIT,
            TIMEOUT_SECONDS,
        )
        .unwrap_err(),
        ExecutionTargetError::TooManyBatchTargets
    );

    let oversized = " ".repeat(MAX_EXECUTE_BATCH_BYTES + 1);
    assert_eq!(
        extract_and_validate_all_targets(
            &oversized,
            ExecutionLanguage::MySql,
            ROW_LIMIT,
            TIMEOUT_SECONDS,
        )
        .unwrap_err(),
        ExecutionTargetError::BatchSourceTooLarge
    );
}

#[test]
fn run_all_enforces_each_mysql_target_parse_token_and_depth_bound() {
    let oversized_target = format!("SELECT '{}'", "a".repeat(MAX_MYSQL_STATEMENT_BYTES));
    assert_eq!(
        extract_and_validate_all_targets(
            &oversized_target,
            ExecutionLanguage::MySql,
            ROW_LIMIT,
            TIMEOUT_SECONDS,
        )
        .unwrap_err(),
        ExecutionTargetError::MysqlBatchTargetTooLarge
    );

    let too_many_tokens = format!(
        "SELECT {}",
        std::iter::repeat_n("1", MAX_MYSQL_TOKENS)
            .collect::<Vec<_>>()
            .join(",")
    );
    assert_eq!(
        extract_and_validate_all_targets(
            &too_many_tokens,
            ExecutionLanguage::MySql,
            ROW_LIMIT,
            TIMEOUT_SECONDS,
        )
        .unwrap_err(),
        ExecutionTargetError::MysqlBatchTooManyTokens
    );

    let too_deep = format!(
        "SELECT {}1{}",
        "(".repeat(MAX_MYSQL_NESTING_DEPTH + 1),
        ")".repeat(MAX_MYSQL_NESTING_DEPTH + 1)
    );
    assert_eq!(
        extract_and_validate_all_targets(
            &too_deep,
            ExecutionLanguage::MySql,
            ROW_LIMIT,
            TIMEOUT_SECONDS,
        )
        .unwrap_err(),
        ExecutionTargetError::MysqlBatchNestingTooDeep
    );
    assert_eq!(
        extract_and_validate_all_targets(
            "SELECT (1;",
            ExecutionLanguage::MySql,
            ROW_LIMIT,
            TIMEOUT_SECONDS,
        )
        .unwrap_err(),
        ExecutionTargetError::MysqlBatchUnbalancedNesting
    );
    assert_eq!(
        extract_and_validate_all_targets(
            "SELECT 1 +;",
            ExecutionLanguage::MySql,
            ROW_LIMIT,
            TIMEOUT_SECONDS,
        )
        .unwrap_err(),
        ExecutionTargetError::MysqlBatchParseFailed
    );
}

#[test]
fn run_all_preflights_every_redis_line_before_returning_a_batch() {
    let batch = extract_and_validate_all_targets(
        "GET first\n\nMGET second third\nPING",
        ExecutionLanguage::Redis,
        ROW_LIMIT,
        TIMEOUT_SECONDS,
    )
    .expect("blank lines are skipped and read commands are retained");
    let targets = batch.targets();
    assert_eq!(targets.len(), 3);
    assert_eq!(targets[0].source_text(), "GET first");
    assert_eq!(targets[1].source_text(), "MGET second third");
    assert_eq!(targets[2].source_text(), "PING");
    assert_eq!(
        classify_execution_batch_kind(ExecutionLanguage::Redis, targets),
        OperationKind::ExecuteRead
    );

    assert_eq!(
        extract_and_validate_all_targets(
            "GET first\nSET first changed",
            ExecutionLanguage::Redis,
            ROW_LIMIT,
            TIMEOUT_SECONDS,
        )
        .unwrap_err(),
        ExecutionTargetError::RedisCommandDenied,
        "a command outside the exact read allowlist prevents any batch from being returned"
    );
    assert_eq!(
        extract_and_validate_all_targets(
            "GET first\nMONITOR",
            ExecutionLanguage::Redis,
            ROW_LIMIT,
            TIMEOUT_SECONDS,
        )
        .unwrap_err(),
        ExecutionTargetError::RedisCommandDenied,
        "a denied later command prevents any batch from being returned"
    );
}

fn mysql_text(result: &ValidatedExecutionTarget) -> &str {
    match result.target() {
        ExecutionTarget::MySqlText(text) => text,
        ExecutionTarget::RedisArgv(_) => panic!("expected a MySQL target"),
    }
}

fn redis_argv(result: &ValidatedExecutionTarget) -> &[String] {
    match result.target() {
        ExecutionTarget::RedisArgv(argv) => argv,
        ExecutionTarget::MySqlText(_) => panic!("expected a Redis target"),
    }
}

#[test]
fn limits_are_validated_before_target_work() {
    let row_low =
        extract_and_validate_target("", 0, None, ExecutionLanguage::Redis, 0, TIMEOUT_SECONDS);
    assert_eq!(row_low.unwrap_err(), ExecutionTargetError::InvalidRowLimit);

    let row_high = extract_and_validate_target(
        "PING",
        0,
        None,
        ExecutionLanguage::Redis,
        MAX_EXECUTE_ROW_LIMIT + 1,
        TIMEOUT_SECONDS,
    );
    assert_eq!(row_high.unwrap_err(), ExecutionTargetError::InvalidRowLimit);

    let timeout_low =
        extract_and_validate_target("PING", 0, None, ExecutionLanguage::Redis, ROW_LIMIT, 0);
    assert_eq!(
        timeout_low.unwrap_err(),
        ExecutionTargetError::InvalidTimeout
    );

    let timeout_high = extract_and_validate_target(
        "PING",
        0,
        None,
        ExecutionLanguage::Redis,
        ROW_LIMIT,
        MAX_EXECUTE_TIMEOUT_SECONDS + 1,
    );
    assert_eq!(
        timeout_high.unwrap_err(),
        ExecutionTargetError::InvalidTimeout
    );

    let maximums = extract_and_validate_target(
        "PING",
        0,
        None,
        ExecutionLanguage::Redis,
        MAX_EXECUTE_ROW_LIMIT,
        MAX_EXECUTE_TIMEOUT_SECONDS,
    )
    .unwrap();
    assert_eq!(maximums.row_limit(), MAX_EXECUTE_ROW_LIMIT);
    assert_eq!(maximums.timeout_seconds(), MAX_EXECUTE_TIMEOUT_SECONDS);
}

#[test]
fn character_positions_are_checked_and_utf8_safe() {
    let text = "SELECT '수달';  \nSELECT 2;";
    let result = mysql(text, character_index(text, "달")).unwrap();
    assert_eq!(mysql_text(&result), "SELECT '수달';");

    assert_eq!(
        mysql(text, text.chars().count() + 1).unwrap_err(),
        ExecutionTargetError::InvalidCaretPosition
    );
    let reversed_selection = std::ops::Range { start: 8, end: 3 };
    assert_eq!(
        mysql_selection(text, reversed_selection).unwrap_err(),
        ExecutionTargetError::InvalidSelectionRange
    );
    assert_eq!(
        mysql_selection(text, 0..text.chars().count() + 1).unwrap_err(),
        ExecutionTargetError::InvalidSelectionRange
    );

    let selected_text = "前 SELECT '수달'; 後";
    let selection_start = character_index(selected_text, "SELECT");
    let selection_end = selection_start + "SELECT '수달';".chars().count();
    let selected = mysql_selection(selected_text, selection_start..selection_end).unwrap();
    assert_eq!(mysql_text(&selected), "SELECT '수달';");
}

#[test]
fn selection_wins_exactly_and_never_falls_back() {
    let text = "SELECT 1; SELECT 2;";
    let first = mysql_selection(text, 0..9).unwrap();
    assert_eq!(mysql_text(&first), "SELECT 1;");

    assert_eq!(
        mysql_selection(text, 9..10).unwrap_err(),
        ExecutionTargetError::NoCurrentStatement
    );
    assert_eq!(
        mysql_selection(text, 0..text.chars().count()).unwrap_err(),
        ExecutionTargetError::MultipleStatements
    );

    let padded = "  SELECT 1;  ";
    let selected = mysql_selection(padded, 0..padded.chars().count()).unwrap();
    assert_eq!(mysql_text(&selected), "SELECT 1;");
}

#[test]
fn mysql_quotes_doubled_delimiters_and_semicolons_are_protected() {
    for text in [
        "SELECT ';' AS value;",
        "SELECT 'it''s;fine';",
        "SELECT \"semi;colon\";",
        "SELECT `semi;colon`;",
        r"SELECT 'backslash\;semicolon';",
    ] {
        let result = mysql(text, character_index(text, "SELECT")).unwrap();
        assert_eq!(mysql_text(&result), text);
    }
}

#[test]
fn mysql_line_comment_rules_are_exact() {
    let hash = "SELECT 1# ;\n";
    assert_eq!(
        mysql_text(&mysql(hash, character_index(hash, "SELECT")).unwrap()),
        "SELECT 1"
    );

    let arithmetic = "SELECT 1--1;";
    assert_eq!(
        mysql_text(&mysql(arithmetic, character_index(arithmetic, "--")).unwrap()),
        arithmetic
    );

    let conditional = "SELECT 1-- comment ;\n";
    assert_eq!(
        mysql_text(&mysql(conditional, character_index(conditional, "SELECT")).unwrap()),
        "SELECT 1"
    );

    let control = "SELECT 1--\tcomment ;\n";
    assert_eq!(
        mysql_text(&mysql(control, character_index(control, "SELECT")).unwrap()),
        "SELECT 1"
    );
}

#[test]
fn mysql_block_comment_kinds_follow_the_frozen_policy() {
    let ordinary = "SELECT 1 /* ; */;";
    assert_eq!(
        mysql_text(&mysql(ordinary, character_index(ordinary, "/*")).unwrap()),
        ordinary
    );
    let ordinary_only = "/* ordinary ; */";
    assert_eq!(
        mysql(ordinary_only, character_index(ordinary_only, "ordinary")).unwrap_err(),
        ExecutionTargetError::NoCurrentStatement
    );

    let version = "/*!40101 SET @x=';' */;";
    assert_eq!(
        mysql(version, character_index(version, "SET")).unwrap_err(),
        ExecutionTargetError::ForbiddenExecutableComment
    );
    for mariadb in ["/*M!100100 SET @x=1 */", "/*m!100100 SET @x=1 */"] {
        assert_eq!(
            mysql(mariadb, character_index(mariadb, "SET")).unwrap_err(),
            ExecutionTargetError::ForbiddenExecutableComment
        );
    }

    let hint = "SELECT /*+ hint; */ 1;";
    assert_eq!(
        mysql_text(&mysql(hint, character_index(hint, "hint")).unwrap()),
        hint
    );
    let hint_only = "/*+ hint; */";
    assert_eq!(
        mysql(hint_only, character_index(hint_only, "hint")).unwrap_err(),
        ExecutionTargetError::NoCurrentStatement
    );

    let surrounding = "/* lead */ SELECT 1 /* trail */";
    assert_eq!(
        mysql_text(&mysql(surrounding, character_index(surrounding, "SELECT")).unwrap()),
        "SELECT 1"
    );
}

#[test]
fn mysql_unterminated_tokens_reject_locally() {
    for text in [
        "SELECT 'unterminated",
        "SELECT \"unterminated",
        "SELECT `unterminated",
        "SELECT 1 /* unterminated",
    ] {
        assert_eq!(
            mysql(text, character_index(text, "SELECT")).unwrap_err(),
            ExecutionTargetError::UnterminatedSqlToken,
            "case: {text}"
        );
    }
}

#[test]
fn mysql_odd_backslash_quote_is_ambiguous_only_for_caret_inference() {
    for text in [r"SELECT 'a\'b';", r#"SELECT "a\"b";"#] {
        assert_eq!(
            mysql(text, character_index(text, "SELECT")).unwrap_err(),
            ExecutionTargetError::AmbiguousSqlMode,
            "case: {text}"
        );
        let selected = mysql_selection(text, 0..text.chars().count()).unwrap();
        assert_eq!(mysql_text(&selected), text);
    }
}

#[test]
fn mysql_trailing_terminator_gap_and_consecutive_targets_are_distinct() {
    let text = "SELECT 1;   SELECT 2;";
    let first = mysql(text, character_index(text, ";")).unwrap();
    assert_eq!(mysql_text(&first), "SELECT 1;");
    assert_eq!(
        mysql(text, character_index(text, "   ") + 1).unwrap_err(),
        ExecutionTargetError::NoCurrentStatement
    );
    let second = mysql(text, character_index(text, "SELECT 2")).unwrap();
    assert_eq!(mysql_text(&second), "SELECT 2;");
}

#[test]
fn redis_without_selection_uses_only_the_caret_physical_line() {
    let text = "GET first\nMGET second value";
    let first = redis(text, character_index(text, "GET")).unwrap();
    assert_eq!(redis_argv(&first), ["GET", "first"]);
    let second = redis(text, character_index(text, "MGET")).unwrap();
    assert_eq!(redis_argv(&second), ["MGET", "second", "value"]);

    assert_eq!(
        redis_selection(text, 0..text.chars().count()).unwrap_err(),
        ExecutionTargetError::MultipleStatements
    );

    let selectable = "PING   ";
    assert_eq!(
        redis_selection(selectable, 4..selectable.chars().count()).unwrap_err(),
        ExecutionTargetError::NoCurrentStatement
    );
}

#[test]
fn redis_shell_parsing_keeps_semicolons_as_data() {
    let quoted = "GET 'a;b'";
    let parsed = redis(quoted, 0).unwrap();
    assert_eq!(redis_argv(&parsed), ["GET", "a;b"]);

    let unquoted = "GET key;SET";
    let parsed = redis(unquoted, 0).unwrap();
    assert_eq!(redis_argv(&parsed), ["GET", "key;SET"]);
}

#[test]
fn redis_blank_comment_and_parse_errors_are_local() {
    for text in ["", "   ", "# comment only", "   # comment only"] {
        assert_eq!(
            redis(text, 0).unwrap_err(),
            ExecutionTargetError::NoCurrentStatement,
            "case: {text}"
        );
    }
    assert_eq!(
        redis("GET 'unterminated", 0).unwrap_err(),
        ExecutionTargetError::RedisShellParseFailed
    );
}

#[test]
fn redis_input_caps_are_checked_before_dispatch() {
    let oversized = "a".repeat(MAX_REDIS_TARGET_BYTES + 1);
    assert_eq!(
        redis(&oversized, 0).unwrap_err(),
        ExecutionTargetError::RedisTargetTooLarge
    );

    let too_many = format!("GET {}", vec!["x"; MAX_REDIS_TOKENS].join(" "));
    assert_eq!(
        redis(&too_many, 0).unwrap_err(),
        ExecutionTargetError::RedisTooManyTokens
    );

    let large_token = format!("GET {}", "x".repeat(MAX_REDIS_TOKEN_BYTES + 1));
    assert_eq!(
        redis(&large_token, 0).unwrap_err(),
        ExecutionTargetError::RedisTokenTooLarge
    );

    let exact_byte_cap = format!(
        "MGET {} {} {} {}",
        "a".repeat(MAX_REDIS_TOKEN_BYTES),
        "b".repeat(MAX_REDIS_TOKEN_BYTES),
        "c".repeat(MAX_REDIS_TOKEN_BYTES),
        "d".repeat(MAX_REDIS_TARGET_BYTES - 8 - (MAX_REDIS_TOKEN_BYTES * 3))
    );
    assert_eq!(exact_byte_cap.len(), MAX_REDIS_TARGET_BYTES);
    redis(&exact_byte_cap, 0).unwrap();
}

#[test]
fn redis_closed_classifier_admits_only_exact_bounded_shapes_case_insensitively() {
    for command in [
        "PING",
        "get key",
        "GETRANGE key 0 1048575",
        "MGET one two",
        "EXISTS one two",
        "TYPE key",
        "TTL key",
        "PTTL key",
        "STRLEN key",
        "HGET key field",
        "HEXISTS key field",
        "HSTRLEN key field",
        "HMGET key one two",
        "HLEN key",
        "HSCAN key 0 COUNT 200",
        "sscan key 0 match 'item:*' count 1",
        "ZSCAN key 0 COUNT 1 MATCH '*'",
        "LLEN key",
        "LINDEX key -1",
        "LRANGE key 0 199",
        "LRANGE key -200 -1",
        "SCARD key",
        "SISMEMBER key member",
        "SMISMEMBER key one two",
        "ZCARD key",
        "ZCOUNT key -inf '(+inf'",
        "ZLEXCOUNT key - '[z'",
        "ZRANK key member",
        "ZREVRANK key member",
        "ZSCORE key member",
        "ZMSCORE key one two",
        "ZRANGE key 0 199 WITHSCORES",
        "SCAN 0 TYPE hash MATCH 'item:*' COUNT 200",
        "XLEN stream",
        "XRANGE stream - + COUNT 200",
        "XREVRANGE stream + - count 1",
    ] {
        redis(command, 0).unwrap_or_else(|error| panic!("allowed case {command}: {error}"));
    }

    for command in [
        "PING payload",
        "GET key extra",
        "GETRANGE key 0 1048576",
        "LRANGE key 0 -1",
        "LRANGE key 0 200",
        "HGETALL key",
        "HKEYS key",
        "HVALS key",
        "SMEMBERS key",
        "ZRANGE key 0 -1",
        "ZRANGE key 0 10 REV",
        "SCAN 0",
        "SCAN 0 COUNT 0",
        "SCAN 0 COUNT 201",
        "SCAN 0 COUNT 1 COUNT 2",
        "SCAN 0 TYPE module COUNT 1",
        "HSCAN key 0 MATCH '*'",
        "XRANGE stream - +",
        "ZCOUNT key NaN +inf",
        "ZLEXCOUNT key '[' '[z'",
        "ZLEXCOUNT key '(a' '('",
        "INFO memory",
        "TIME",
        "DBSIZE",
        "XREAD STREAMS stream 0",
        "XREAD BLOCK 0 STREAMS stream 0",
        "SET key value",
        "MONITOR",
        "\"subscribe\" channel",
        "BlAnything",
    ] {
        assert_eq!(
            redis(command, 0).unwrap_err(),
            ExecutionTargetError::RedisCommandDenied,
            "case: {command}"
        );
    }
}

#[test]
fn redis_classifier_reads_command_and_option_keywords_without_reclassifying_arguments() {
    for allowed in [
        "GET SUBSCRIBE",
        "GET 'MONITOR'",
        "MGET BLOCK value",
        "GET BRAND",
        "SCAN 0 MATCH BLOCK COUNT 1",
    ] {
        redis(allowed, 0).unwrap();
    }
}

#[test]
fn target_and_error_debug_are_static_and_redacted() {
    let sentinel = "USER_EXECUTION_SENTINEL_7dc2";
    let mysql_target = mysql(sentinel, 0).unwrap();
    let redis_target = redis(&format!("GET {sentinel}"), 0).unwrap();
    for rendered in [format!("{mysql_target:?}"), format!("{redis_target:?}")] {
        assert!(!rendered.contains(sentinel));
        assert!(rendered.contains("<redacted>"));
    }

    let error = ExecutionTargetError::RedisCommandDenied;
    assert_eq!(error.code(), "REDIS_COMMAND_DENIED");
    assert!(!format!("{error:?}").contains(sentinel));
    assert!(!error.to_string().contains(sentinel));

    let source = include_str!("../src/execution.rs");
    assert!(!source.contains("serde::Serialize"));
    assert!(!source.contains("derive(Serialize"));
}
