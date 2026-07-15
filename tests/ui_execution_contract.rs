#![cfg(feature = "desktop")]

use dbotter::execution::{ExecutionLanguage, ExecutionTarget, classify_execution_kind};
use dbotter::model::OperationKind;
use dbotter::ui::DEFAULT_EXECUTE_ROW_LIMIT;

#[test]
fn execute_operation_kind_is_truthful_for_read_and_mutation_targets() {
    let cases = [
        (
            ExecutionLanguage::MySql,
            ExecutionTarget::MySqlText("SELECT 1".to_owned()),
            OperationKind::ExecuteRead,
        ),
        (
            ExecutionLanguage::MySql,
            ExecutionTarget::MySqlText("INSERT INTO t VALUES (1)".to_owned()),
            OperationKind::ExecuteMutation,
        ),
        (
            ExecutionLanguage::Redis,
            ExecutionTarget::RedisArgv(vec!["GET".to_owned(), "key".to_owned()]),
            OperationKind::ExecuteRead,
        ),
        (
            ExecutionLanguage::Redis,
            ExecutionTarget::RedisArgv(vec!["SET".to_owned(), "key".to_owned(), "v".to_owned()]),
            OperationKind::ExecuteMutation,
        ),
    ];

    for (language, target, expected) in cases {
        assert_eq!(classify_execution_kind(language, &target), expected);
    }
}

#[test]
fn execute_ui_default_matches_the_frozen_contract() {
    assert_eq!(std::hint::black_box(DEFAULT_EXECUTE_ROW_LIMIT), 500);
}
