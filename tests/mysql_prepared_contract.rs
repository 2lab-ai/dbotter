use std::fs;
use std::path::PathBuf;

fn source(path: &str) -> String {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    fs::read_to_string(root.join(path)).unwrap_or_else(|error| panic!("read {path}: {error}"))
}

#[test]
fn mysql_user_text_has_one_prepared_only_driver_entry() {
    let mysql = source("src/drivers/mysql.rs");
    let drivers = source("src/drivers/mod.rs");
    let service = source("src/service.rs");

    assert!(mysql.contains("PreparedMySqlRequest"));
    assert!(mysql.contains("execute_prepared"));
    assert!(drivers.contains("trait MySqlPreparedExecution"));
    assert!(service.contains("PreparedMySqlRequest"));

    for forbidden in [
        "sqlx::raw_sql",
        "execute_raw",
        "COM_QUERY",
        "is_unsupported_prepared_statement",
        "raw fallback",
    ] {
        assert!(
            !mysql.contains(forbidden),
            "MySQL user-text path contains forbidden prepared fallback token {forbidden}"
        );
    }
}

#[test]
fn generic_execute_request_cannot_cross_the_driver_boundary() {
    let drivers = source("src/drivers/mod.rs");
    let service = source("src/service.rs");

    assert!(
        !drivers.contains("pub async fn execute(&self, request: &ExecuteRequest")
            && !service.contains("async fn execute(&self, request: &ExecuteRequest"),
        "generic ExecuteRequest must be converted before the typed driver boundary"
    );
    assert!(drivers.contains("RedisExecuteRequest"));
}

#[test]
fn prepared_unsupported_is_static_and_never_resubmits_user_text() {
    let drivers = source("src/drivers/mod.rs");
    let mysql = source("src/drivers/mysql.rs");
    let public_error = source("src/public_error.rs");

    assert!(drivers.contains("PreparedStatementUnsupported"));
    assert!(mysql.contains("PreparedStatementUnsupported"));
    assert!(public_error.contains("PublicCode::PreparedStatementUnsupported"));
    assert!(public_error.contains("RecoveryAction::FocusEditor"));
    assert!(public_error.contains("RecoveryAction::DismissError"));
}
