use dbotter::model::{
    Cell, Column, DriverKind, OperationId, ProfileGeneration, ProfileId, QueryResult, ResultId,
    ResultNotice, ResultProvenance, ResultRetentionPolicy, ResultSnapshot,
};

fn provenance(driver: DriverKind) -> ResultProvenance {
    ResultProvenance {
        result_id: ResultId(71),
        profile_id: ProfileId("profile-cell-truth".to_owned()),
        profile_generation: ProfileGeneration(7),
        operation_id: OperationId(17),
        driver,
        completed_at_unix_ms: 1_700_000_000_123,
        duration_ms: 19,
    }
}

fn retain(cell: Cell, driver: DriverKind) -> ResultSnapshot {
    ResultSnapshot::retain(
        QueryResult {
            columns: vec![Column {
                name: "value".to_owned(),
                type_name: "VALUE".to_owned(),
            }],
            rows: vec![vec![cell]],
            affected_rows: 0,
            last_insert_id: None,
            elapsed_ms: 19,
            truncated: false,
            backend_notices_present: false,
        },
        provenance(driver),
        ResultRetentionPolicy::redis(1),
    )
}

#[test]
fn retained_text_exposes_preview_truth_in_the_cell_variant() {
    let original_len = dbotter::model::MAX_REDIS_CELL_BYTES + 11;
    let snapshot = retain(Cell::Text("x".repeat(original_len)), DriverKind::Redis);
    let encoded = serde_json::to_value(&snapshot.rows[0][0]).expect("serialize retained text");

    assert_eq!(encoded["type"], "text_preview");
    assert_eq!(encoded["value"]["original_len"], original_len);
    assert_eq!(
        encoded["value"]["preview"].as_str().map(str::len),
        Some(dbotter::model::MAX_REDIS_CELL_BYTES)
    );
}

#[test]
fn retained_json_exposes_utf8_preview_truth_in_the_cell_variant() {
    let original = serde_json::json!({
        "payload": "x".repeat(dbotter::model::MAX_REDIS_CELL_BYTES + 128)
    });
    let original_len = serde_json::to_vec(&original)
        .expect("serialize source JSON")
        .len();
    let snapshot = retain(Cell::Json(original), DriverKind::Redis);
    let encoded = serde_json::to_value(&snapshot.rows[0][0]).expect("serialize retained JSON");

    assert_eq!(encoded["type"], "json_preview");
    assert_eq!(encoded["value"]["original_len"], original_len);
    assert!(encoded["value"]["preview"].is_string());
}

#[test]
fn retained_bytes_keep_raw_identity_and_exact_original_length() {
    let snapshot = retain(
        Cell::Bytes {
            retained: vec![0, 0xff, b'a'],
            original_len: 9,
        },
        DriverKind::Redis,
    );

    let Cell::Bytes {
        retained,
        original_len,
    } = &snapshot.rows[0][0]
    else {
        panic!("expected retained byte cell");
    };
    assert_eq!(retained, &[0, 0xff, b'a']);
    assert_eq!(*original_len, 9);
    assert_eq!(snapshot.cell_truncations[0].original_len, Some(9));
}

#[test]
fn metadata_retention_stops_at_an_oversized_entry_and_preserves_row_width() {
    let exact_boundary_name = "x".repeat(
        dbotter::model::MAX_RESULT_CELL_BYTES
            .checked_sub("TEXT".len())
            .expect("metadata boundary exceeds the fixture type name"),
    );
    let snapshot = ResultSnapshot::retain(
        QueryResult {
            columns: vec![
                Column {
                    name: exact_boundary_name.clone(),
                    type_name: "TEXT".to_owned(),
                },
                Column {
                    name: "y".repeat(dbotter::model::MAX_RESULT_CELL_BYTES + 1),
                    type_name: String::new(),
                },
                Column {
                    name: "must-not-be-retained".to_owned(),
                    type_name: "TEXT".to_owned(),
                },
            ],
            rows: vec![vec![
                Cell::Text("first".to_owned()),
                Cell::Text("second".to_owned()),
                Cell::Text("third".to_owned()),
            ]],
            affected_rows: 0,
            last_insert_id: None,
            elapsed_ms: 19,
            truncated: false,
            backend_notices_present: false,
        },
        provenance(DriverKind::MySql),
        ResultRetentionPolicy::mysql(1),
    );

    assert_eq!(snapshot.columns.len(), 1);
    assert_eq!(snapshot.columns[0].name, exact_boundary_name);
    assert_eq!(
        snapshot.columns[0].name.len() + snapshot.columns[0].type_name.len(),
        dbotter::model::MAX_RESULT_CELL_BYTES,
        "an entry exactly at the boundary must be retained"
    );
    assert!(snapshot.truncated);
    assert!(snapshot.notices.contains(&ResultNotice::ColumnLimitReached));
    assert!(
        snapshot
            .notices
            .contains(&ResultNotice::SnapshotByteLimitReached)
    );
    assert_eq!(snapshot.rows.len(), 1);
    assert!(
        snapshot
            .rows
            .iter()
            .all(|row| row.len() == snapshot.columns.len()),
        "every retained row must remain positional with the retained schema"
    );
    assert_eq!(snapshot.rows[0], vec![Cell::Text("first".to_owned())]);
}

#[test]
fn unpreviewable_oversized_scalars_drop_the_crossing_row_without_false_complete_wire_truth() {
    let cases = [
        Cell::Decimal("9".repeat(dbotter::model::MAX_RESULT_CELL_BYTES + 1)),
        Cell::DateTime("2".repeat(dbotter::model::MAX_RESULT_CELL_BYTES + 1)),
    ];

    for oversized in cases {
        let snapshot = ResultSnapshot::retain(
            QueryResult {
                columns: vec![Column {
                    name: "value".to_owned(),
                    type_name: "VALUE".to_owned(),
                }],
                rows: vec![
                    vec![Cell::Text("complete-before-crossing".to_owned())],
                    vec![oversized],
                    vec![Cell::Text("must-not-pass-crossing".to_owned())],
                ],
                affected_rows: 0,
                last_insert_id: None,
                elapsed_ms: 19,
                truncated: false,
                backend_notices_present: false,
            },
            provenance(DriverKind::MySql),
            ResultRetentionPolicy::mysql(3),
        );

        assert_eq!(
            snapshot.rows,
            vec![vec![Cell::Text("complete-before-crossing".to_owned())]]
        );
        assert!(snapshot.truncated);
        assert!(
            snapshot
                .notices
                .contains(&ResultNotice::SnapshotByteLimitReached)
        );
        assert!(snapshot.notices.contains(&ResultNotice::RowLimitReached));
        assert!(
            !snapshot
                .notices
                .contains(&ResultNotice::CellPreviewTruncated)
        );
        assert!(snapshot.cell_truncations.is_empty());
    }
}
