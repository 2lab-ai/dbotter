use std::time::Duration;

use dbotter::drivers::{self, DriverError, Session};
use dbotter::model::{
    Cell, ConnectionProfile, CredentialMode, DriverKind, ExecuteRequest, OperationId, ProfileId,
    QueryLanguage, QueryResult, RedisTlsConfig, TlsMode,
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
    session
        .execute(&ExecuteRequest {
            operation_id: OperationId(1),
            profile_id: ProfileId("mysql-contract".to_owned()),
            language: QueryLanguage::Sql,
            text: text.to_owned(),
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
        "  /* leading ; block comment */\n -- leading ; line comment\n SELECT ';' AS semi, 'a;b' AS embedded; /* trailing ; comment */",
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
async fn multiple_statements_are_rejected_before_either_can_execute() {
    if !live_contract_enabled() {
        return;
    }
    let session = mysql_session().await;
    let error = execute(&session, "SELECT 1; SELECT 2")
        .await
        .expect_err("multiple statements must be rejected locally");

    assert!(matches!(
        error,
        DriverError::InvalidConfig {
            driver: DriverKind::MySql,
            message
        } if message == "exactly one MySQL statement is required"
    ));
}

#[tokio::test]
async fn select_show_and_explain_are_metadata_classified_result_sets() {
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
        .expect("SHOW result set");
    assert!(!show.columns.is_empty());
    assert!(!show.rows.is_empty());
    assert_eq!(show.affected_rows, 0);

    let explain = execute(&session, "EXPLAIN SELECT 1")
        .await
        .expect("EXPLAIN result set");
    assert!(!explain.columns.is_empty());
    assert!(!explain.rows.is_empty());
    assert_eq!(explain.affected_rows, 0);
}

#[tokio::test]
async fn mutation_returns_affected_count_instead_of_rows() {
    if !live_contract_enabled() {
        return;
    }
    let session = mysql_session().await;
    execute(
        &session,
        "DROP TABLE IF EXISTS dbotter_sql_mutation_contract",
    )
    .await
    .expect("drop mutation fixture");
    execute(
        &session,
        "CREATE TABLE dbotter_sql_mutation_contract (id INT PRIMARY KEY, marker VARCHAR(32) NOT NULL)",
    )
    .await
    .expect("create mutation fixture");
    let result = execute(
        &session,
        "INSERT INTO dbotter_sql_mutation_contract (id, marker) VALUES (1, 'plain')",
    )
    .await
    .expect("insert mutation");

    assert!(result.columns.is_empty());
    assert!(result.rows.is_empty());
    assert_eq!(result.affected_rows, 1);
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
async fn cte_update_is_a_mutation() {
    if !live_contract_enabled() {
        return;
    }
    let session = mysql_session().await;
    execute(
        &session,
        "DROP TABLE IF EXISTS dbotter_sql_cte_mutation_contract",
    )
    .await
    .expect("drop CTE fixture");
    execute(
        &session,
        "CREATE TABLE dbotter_sql_cte_mutation_contract (id INT PRIMARY KEY, marker VARCHAR(32) NOT NULL)",
    )
    .await
    .expect("create CTE fixture");
    execute(
        &session,
        "INSERT INTO dbotter_sql_cte_mutation_contract (id, marker) VALUES (1, 'before')",
    )
    .await
    .expect("seed CTE fixture");

    let result = execute(
        &session,
        "WITH source AS (SELECT 1 AS id, 'after' AS marker) UPDATE dbotter_sql_cte_mutation_contract AS target JOIN source ON source.id = target.id SET target.marker = source.marker",
    )
    .await
    .expect("CTE UPDATE");

    assert!(result.columns.is_empty());
    assert!(result.rows.is_empty());
    assert_eq!(result.affected_rows, 1);
}

#[tokio::test]
async fn unsupported_prepare_uses_validated_raw_fallback() {
    if !live_contract_enabled() {
        return;
    }
    let session = mysql_session().await;
    let result = execute(&session, "USE dbotter")
        .await
        .expect("USE should execute through the MySQL 1295 raw fallback");

    assert!(result.columns.is_empty());
    assert!(result.rows.is_empty());
}
