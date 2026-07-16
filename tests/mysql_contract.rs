use std::time::Duration;

use dbotter::drivers::{self, DriverError, MySqlReadExecution, Session};
use dbotter::model::{
    Cell, ConnectionProfile, CredentialMode, DriverKind, OperationId, PreparedMySqlRequest,
    ProfileAccess, ProfileEnvironment, ProfileGeneration, ProfileId, ProfileSafetyPosture,
    QueryResult, RedisTlsConfig, RequestIdentity, TlsMode,
};
use secrecy::SecretString;

fn live_contract_enabled() -> bool {
    std::env::var("DBOTTER_TEST_MYSQL").as_deref() == Ok("1")
}

async fn mysql_session() -> Session {
    let port = std::env::var("DBOTTER_TEST_MYSQL_PORT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(33306);
    let password =
        std::env::var("DBOTTER_MYSQL_PASSWORD").unwrap_or_else(|_| "dbotter-local-only".to_owned());
    let secret = SecretString::from(password);
    let profile = ConnectionProfile {
        id: "mysql-contract".to_owned(),
        name: "MySQL contract".to_owned(),
        driver: DriverKind::MySql,
        host: "127.0.0.1".to_owned(),
        port,
        database: Some("dbotter".to_owned()),
        username: Some("dbotter".to_owned()),
        safety: ProfileSafetyPosture::new(
            ProfileEnvironment::Development,
            ProfileAccess::ReadWrite,
        ),
        tls: TlsMode::Disabled,
        credential_mode: CredentialMode::None,
        secret_env: None,
        redis_tls: RedisTlsConfig::default(),
    };

    drivers::connect(&profile, Some(&secret), Duration::from_secs(10))
        .await
        .expect("connect to opt-in MySQL fixture")
}

async fn execute(session: &Session, text: &str) -> Result<QueryResult, DriverError> {
    let admission = session
        .begin_read_admission(Duration::from_secs(10))
        .await?;
    let mut proven = admission.prove_read_only().await?;
    proven
        .execute_prepared(&PreparedMySqlRequest {
            identity: RequestIdentity::new(
                ProfileId("mysql-contract".to_owned()),
                ProfileGeneration(1),
                OperationId(1),
            ),
            statement: text.to_owned(),
            row_limit: 10,
            timeout: Duration::from_secs(10),
        })
        .await
}

#[tokio::test]
async fn leading_comments_and_quoted_semicolons_are_one_row_query() {
    if !live_contract_enabled() {
        return;
    }
    let session = mysql_session().await;
    let result = execute(
        &session,
        "  /* leading ; block comment */\n -- leading ; line comment\n SELECT ';' AS semi, 'a;b' AS embedded;",
    )
    .await
    .expect("commented SELECT with quoted semicolons");

    assert_eq!(
        result
            .columns
            .iter()
            .map(|column| column.name.as_str())
            .collect::<Vec<_>>(),
        ["semi", "embedded"]
    );
    assert_eq!(
        result.rows,
        [vec![
            Cell::Text(";".to_owned()),
            Cell::Text("a;b".to_owned())
        ]]
    );
    assert_eq!(result.affected_rows, 0);
}

#[tokio::test]
async fn multiple_statements_are_rejected_before_the_read_target() {
    if !live_contract_enabled() {
        return;
    }
    let session = mysql_session().await;
    let error = execute(&session, "SELECT 1 AS first; SELECT 2 AS second")
        .await
        .expect_err("the proven read lease must reject a multi-statement target");
    assert!(matches!(error, DriverError::MySqlReadOnlyNotProven { .. }));
}

#[tokio::test]
async fn select_is_a_result_set_while_metadata_statement_families_are_denied() {
    if !live_contract_enabled() {
        return;
    }
    let session = mysql_session().await;

    let select = execute(&session, "/* lead */ SELECT 1 AS n WHERE FALSE")
        .await
        .expect("empty SELECT");
    assert_eq!(select.columns[0].name, "n");
    assert!(select.rows.is_empty());
    assert_eq!(select.affected_rows, 0);

    let show = execute(&session, "SHOW STATUS LIKE 'Threads_connected'")
        .await
        .expect_err("SHOW is outside the v1.1 editor read grammar");
    assert!(matches!(show, DriverError::MySqlReadOnlyNotProven { .. }));

    let explain = execute(&session, "EXPLAIN SELECT 1")
        .await
        .expect_err("EXPLAIN is outside the v1.1 editor read grammar");
    assert!(matches!(
        explain,
        DriverError::MySqlReadOnlyNotProven { .. }
    ));
}

#[tokio::test]
async fn mutation_is_denied_by_the_proven_read_port() {
    if !live_contract_enabled() {
        return;
    }
    let session = mysql_session().await;
    let error = execute(
        &session,
        "INSERT INTO dbotter_live_execute (id, marker) VALUES (1, 'plain')",
    )
    .await
    .expect_err("mutation must not reach the proven read port");
    assert!(matches!(error, DriverError::MySqlReadOnlyNotProven { .. }));
}

#[tokio::test]
async fn cte_select_is_a_result_set() {
    if !live_contract_enabled() {
        return;
    }
    let session = mysql_session().await;
    let result = execute(
        &session,
        "WITH answer AS (SELECT 42 AS value) SELECT value FROM answer",
    )
    .await
    .expect("CTE SELECT");

    assert_eq!(result.columns[0].name, "value");
    assert_eq!(result.rows, [vec![Cell::Int(42)]]);
    assert_eq!(result.affected_rows, 0);
}

#[tokio::test]
async fn cte_update_is_denied_by_the_proven_read_port() {
    if !live_contract_enabled() {
        return;
    }
    let session = mysql_session().await;
    let error = execute(
        &session,
        "WITH source AS (SELECT 1 AS id, 'after' AS marker) UPDATE dbotter_sql_cte_mutation_contract AS target JOIN source ON source.id = target.id SET target.marker = source.marker",
    )
    .await
    .expect_err("CTE UPDATE must not reach the proven read port");
    assert!(matches!(error, DriverError::MySqlReadOnlyNotProven { .. }));
}

#[tokio::test]
async fn non_select_prepared_family_is_denied_before_prepare() {
    if !live_contract_enabled() {
        return;
    }
    let session = mysql_session().await;
    let error = execute(&session, "USE dbotter")
        .await
        .expect_err("USE must not cross the read-only driver boundary");

    assert!(matches!(error, DriverError::MySqlReadOnlyNotProven { .. }));
}
