#![cfg(feature = "desktop")]

use dbotter::drivers::{
    MySqlProvenReadLease, MySqlReadAdmission, MySqlReadExecution, MySqlUnprovenReadLease,
};
use dbotter::execution::{
    ExecutionLanguage, ExecutionTarget, ExecutionTargetError, classify_execution_kind,
    classify_mysql_execution_kind_with_sql_mode, extract_and_validate_target,
};
use dbotter::model::OperationKind;

const DRIVER_SOURCE: &str = include_str!("../src/drivers/mysql.rs");
const DRIVER_PORT_SOURCE: &str = include_str!("../src/drivers/mod.rs");
const SERVICE_SOURCE: &str = include_str!("../src/service.rs");

fn mysql_kind(source: &str) -> OperationKind {
    classify_execution_kind(
        ExecutionLanguage::MySql,
        &ExecutionTarget::MySqlText(source.to_owned()),
    )
}

#[test]
fn v11_general_selects_route_to_the_read_only_port() {
    for source in [
        "SELECT * FROM app.users WHERE id = 1 LIMIT 200",
        "WITH recent AS (SELECT id FROM app.users LIMIT 5) SELECT * FROM recent",
        "SELECT side_effecting_udf()",
        "SELECT * FROM sql_security_definer_view",
        "SELECT /*+ MAX_EXECUTION_TIME(1000) */ 1",
    ] {
        assert_eq!(
            mysql_kind(source),
            OperationKind::ExecuteRead,
            "a v1.1 SELECT must reach only the server-enforced read-only port"
        );
    }
}

#[test]
fn select_side_effect_and_lock_shapes_never_route_as_reads() {
    for source in [
        "SELECT * FROM app.users FOR UPDATE",
        "SELECT * FROM app.users FOR SHARE",
        "SELECT * FROM app.users LOCK IN SHARE MODE",
        "SELECT * FROM app.users INTO OUTFILE '/tmp/users.csv'",
        "SELECT * FROM app.users INTO DUMPFILE '/tmp/users.bin'",
        "SELECT id INTO @selected_id FROM app.users LIMIT 1",
        "SELECT @counter := @counter + 1",
        "/*!40101 SET @x = 1 */",
        "/*M!100100 SET @x = 1 */",
    ] {
        assert_eq!(
            mysql_kind(source),
            OperationKind::ExecuteMutation,
            "locking, INTO, assignment and executable-comment shapes must fail closed"
        );
    }
}

#[test]
fn executable_comment_marker_bytes_are_rejected_even_inside_quotes() {
    for source in [
        "SELECT '/*!40101 SET @x=1 */'",
        "SELECT \"/*M!100100 SET @x=1 */\"",
        "SELECT `/*m!100100 SET @x=1 */`",
    ] {
        assert_eq!(
            extract_and_validate_target(
                source,
                source.chars().count(),
                Some(0..source.chars().count()),
                ExecutionLanguage::MySql,
                100,
                10,
            )
            .expect_err("forbidden marker bytes must fail before session acquisition"),
            ExecutionTargetError::ForbiddenExecutableComment
        );
    }
}

#[test]
fn non_select_statement_families_remain_closed() {
    for source in [
        "INSERT INTO app.users(id) VALUES (1)",
        "UPDATE app.users SET name = 'x' WHERE id = 1 LIMIT 1",
        "DELETE FROM app.users WHERE id = 1 LIMIT 1",
        "SHOW TABLES",
        "DESCRIBE app.users",
        "EXPLAIN SELECT * FROM app.users",
        "REPLACE INTO app.users(id) VALUES (1)",
        "CALL mutate_users()",
        "START TRANSACTION",
        "COMMIT",
    ] {
        assert_eq!(
            mysql_kind(source),
            OperationKind::ExecuteMutation,
            "only a positively classified SELECT may use the read-only port"
        );
    }
}

#[test]
fn exact_sql_mode_changes_quote_lexing_without_reopening_side_effects() {
    assert_eq!(
        classify_mysql_execution_kind_with_sql_mode(
            "SELECT \"display name\" FROM \"app users\"",
            "ANSI_QUOTES,NO_BACKSLASH_ESCAPES,IGNORE_SPACE,STRICT_TRANS_TABLES",
        ),
        Some(OperationKind::ExecuteRead)
    );
    assert_eq!(
        classify_mysql_execution_kind_with_sql_mode(
            "SELECT 'UPDATE widgets SET value = 1', /* DELETE FROM widgets */ 7",
            "NO_BACKSLASH_ESCAPES",
        ),
        Some(OperationKind::ExecuteRead),
        "keywords inside strings and inert comments are data"
    );
    assert_eq!(
        classify_mysql_execution_kind_with_sql_mode(
            "WITH source AS (SELECT 1) UPDATE widgets SET value = 1",
            "STRICT_TRANS_TABLES",
        ),
        Some(OperationKind::ExecuteMutation)
    );
    assert_eq!(
        classify_mysql_execution_kind_with_sql_mode("SELECT 1", "ANSI_QUOTES,,IGNORE_SPACE"),
        None,
        "an undecodable server sql_mode must block before user dispatch"
    );
    assert_eq!(
        classify_mysql_execution_kind_with_sql_mode("SELECT 1", "ansi_quotes"),
        None,
        "server mode tokens must use the exact uppercase wire form"
    );
}

#[test]
fn mysql_driver_port_is_read_only_by_construction() {
    fn accepts_read_port<T: MySqlReadExecution>() {}
    fn accepts_admission(_value: Option<MySqlReadAdmission>) {}
    fn accepts_unproven(_value: Option<Box<dyn MySqlUnprovenReadLease>>) {}
    fn accepts_proven(_value: Option<Box<dyn MySqlProvenReadLease>>) {}
    accepts_read_port::<dbotter::drivers::Session>();
    accepts_admission(None);
    accepts_unproven(None);
    accepts_proven(None);

    assert!(
        DRIVER_PORT_SOURCE.contains("trait MySqlReadExecution"),
        "the connected MySQL capability must begin a read admission"
    );
    assert!(
        DRIVER_PORT_SOURCE.contains("begin_read_admission"),
        "the generic session must yield an operation-scoped physical read lease"
    );
    assert!(
        DRIVER_PORT_SOURCE.contains("trait MySqlUnprovenReadLease")
            && DRIVER_PORT_SOURCE.contains("trait MySqlProvenReadLease"),
        "only the proven typestate may expose prepared user execution"
    );
    assert!(
        !DRIVER_PORT_SOURCE.contains("trait MySqlPreparedExecution"),
        "the generic prepared execution escape hatch must be removed"
    );
    assert!(
        SERVICE_SOURCE.contains("begin_read_admission")
            && SERVICE_SOURCE.contains("prove_read_only")
            && SERVICE_SOURCE.contains("execute_prepared"),
        "ApplicationService must parse with the held admission and execute only after proof"
    );
}

#[test]
fn physical_connection_proves_read_only_before_user_statement() {
    let admission_start = DRIVER_SOURCE
        .find("pub async fn begin_read_admission")
        .expect("read admission implementation");
    let admission_end = DRIVER_SOURCE[admission_start..]
        .find("pub async fn load_page")
        .map(|offset| admission_start + offset)
        .expect("read admission implementation boundary");
    let admission = &DRIVER_SOURCE[admission_start..admission_end];
    let acquire = admission
        .find("self.pool.acquire()")
        .expect("read execution must acquire one physical connection");
    let close_on_drop = admission
        .find("close_on_drop")
        .expect("session posture must never leak back into the shared pool");
    let capability = admission
        .find("@@SESSION.sql_mode")
        .expect("the held connection must load the typed capability row");
    let precheck = admission
        .find("@@SESSION.autocommit")
        .expect("the held connection must prove the dedicated read session uses autocommit");

    let proof_start = DRIVER_SOURCE
        .find("impl MySqlUnprovenReadLease for UnprovenMySqlReadLease")
        .expect("unproven lease implementation");
    let proof_end = DRIVER_SOURCE[proof_start..]
        .find("struct ProvenMySqlReadLease")
        .map(|offset| proof_start + offset)
        .expect("unproven lease implementation boundary");
    let proof_source = &DRIVER_SOURCE[proof_start..proof_end];
    let set_read_only = proof_source
        .find("SET SESSION TRANSACTION READ ONLY")
        .expect("read execution must set the session default read-only");
    let proof = proof_source
        .find("@@SESSION.transaction_read_only")
        .expect("the capability row must prove the session read-only value");

    assert!(
        acquire < close_on_drop
            && close_on_drop < capability
            && capability < precheck
            && set_read_only < proof,
        "one non-reusable connection must follow acquire → capability → clean precheck, then read-only SET → typed proof"
    );
    assert!(
        !DRIVER_SOURCE.contains("@@SESSION.in_transaction"),
        "MySQL does not expose an in_transaction system variable"
    );
    assert!(
        DRIVER_SOURCE.contains("impl MySqlProvenReadLease for ProvenMySqlReadLease")
            && DRIVER_SOURCE.contains(".prepare(sqlx::AssertSqlSafe"),
        "only the proven lease implementation may reach the prepared user target"
    );

    let begin = SERVICE_SOURCE
        .find("begin_read_admission")
        .expect("service must begin admission");
    let prove = SERVICE_SOURCE
        .find("prove_read_only")
        .expect("service must prove the held lease");
    let execute = SERVICE_SOURCE[prove..]
        .find("execute_prepared")
        .map(|offset| prove + offset)
        .expect("service must execute through the proven lease");
    assert!(
        begin < prove && prove < execute,
        "service dispatch must be admission → proof → prepared execution"
    );
}
