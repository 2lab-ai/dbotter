use dbotter::model::{
    Cell, Column, DriverKind, ExecOutput, ExecReceipt, OperationId, ProfileGeneration, ProfileId,
    QueryResult, ResultId, ResultProvenance, ResultRetentionPolicy, ResultSnapshot,
};

#[test]
fn execution_receipt_contains_only_safe_metadata_while_cli_output_is_explicitly_value_bearing() {
    let sentinel = "user-result-value-sentinel";
    let raw = QueryResult {
        columns: vec![Column {
            name: "value".to_owned(),
            type_name: "TEXT".to_owned(),
        }],
        rows: vec![vec![Cell::Text(sentinel.to_owned())]],
        affected_rows: 0,
        last_insert_id: None,
        elapsed_ms: 17,
        truncated: false,
        backend_notices_present: false,
    };
    let result = ResultSnapshot::retain(
        raw,
        ResultProvenance {
            result_id: ResultId(1),
            profile_id: ProfileId("profile".to_owned()),
            profile_generation: ProfileGeneration(1),
            operation_id: OperationId(7),
            driver: DriverKind::MySql,
            completed_at_unix_ms: 0,
            duration_ms: 17,
        },
        ResultRetentionPolicy::mysql(500),
    );
    let receipt = ExecReceipt::from_result(
        "ok",
        OperationId(7),
        ProfileId("profile".to_owned()),
        DriverKind::MySql,
        &result,
    );

    let receipt_json = serde_json::to_string(&receipt).expect("receipt serializes");
    assert!(!receipt_json.contains(sentinel));
    let receipt_value: serde_json::Value =
        serde_json::from_str(&receipt_json).expect("receipt JSON");
    let receipt_object = receipt_value.as_object().expect("receipt object");
    assert!(!receipt_object.contains_key("columns"));
    assert!(!receipt_object.contains_key("rows"));
    assert!(!receipt_object.contains_key("result"));
    assert!(receipt_json.contains("column_count"));
    assert!(receipt_json.contains("row_count"));

    let output = ExecOutput { receipt, result };
    let output_json = serde_json::to_string(&output).expect("CLI output serializes");
    assert!(output_json.contains(sentinel));
    assert!(output_json.contains("\"receipt\""));
    assert!(output_json.contains("\"result\""));
    assert!(!format!("{output:?}").contains(sentinel));
}
