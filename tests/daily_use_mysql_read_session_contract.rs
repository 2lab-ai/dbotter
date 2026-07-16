#![cfg(feature = "desktop")]

use dbotter::drivers::{
    MySqlProvenReadLease, MySqlReadAdmission, MySqlReadExecution, MySqlUnprovenReadLease,
};
use dbotter::execution::{ExecutionLanguage, ExecutionTarget, classify_execution_kind};
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
    let acquire = DRIVER_SOURCE
        .find("self.pool.acquire()")
        .expect("read execution must acquire one physical connection");
    let close_on_drop = DRIVER_SOURCE
        .find("close_on_drop")
        .expect("session posture must never leak back into the shared pool");
    let capability = DRIVER_SOURCE
        .find("@@SESSION.sql_mode")
        .expect("the held connection must load the typed capability row");
    let precheck = DRIVER_SOURCE
        .find("@@SESSION.in_transaction")
        .expect("the held connection must prove no transaction is already active");
    let set_read_only = DRIVER_SOURCE
        .find("SET SESSION TRANSACTION READ ONLY")
        .expect("read execution must set the session default read-only");
    let proof = DRIVER_SOURCE
        .find("@@SESSION.transaction_read_only")
        .expect("the capability row must prove the session read-only value");
    let prepare = DRIVER_SOURCE
        .find(".prepare(sqlx::AssertSqlSafe")
        .expect("the user target must still use the prepared protocol");

    assert!(
        acquire < close_on_drop
            && close_on_drop < capability
            && capability < precheck
            && precheck < set_read_only
            && set_read_only < proof
            && proof < prepare,
        "one non-reusable connection must follow acquire → capability → clean precheck → read-only SET → typed proof → user prepare"
    );
}
