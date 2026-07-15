#![cfg(feature = "desktop")]

use std::collections::BTreeSet;

use dbotter::model::{
    Cell, Column, DriverKind, OperationId, ProfileGeneration, ProfileId, QueryResult, ResultId,
    ResultProvenance, ResultRetentionPolicy, ResultSnapshot,
};
use dbotter::ui::{
    NativeUiHarness, OpenAiTheme, RESULT_ACTION_HEIGHT, RESULT_ROW_HEIGHT, copy_all_rows,
    copy_cell, copy_selected_rows,
};
use eframe::egui::{Context, RawInput};

fn snapshot() -> ResultSnapshot {
    ResultSnapshot::retain(
        QueryResult {
            columns: vec![
                Column {
                    name: "dup".to_owned(),
                    type_name: "TEXT".to_owned(),
                },
                Column {
                    name: "dup".to_owned(),
                    type_name: "TEXT".to_owned(),
                },
            ],
            rows: vec![
                vec![Cell::Text("a\tb".to_owned()), Cell::Null],
                vec![Cell::Text("line\nbreak\\".to_owned()), Cell::Bool(true)],
                vec![
                    Cell::TextPreview {
                        preview: "끝".to_owned(),
                        original_len: 9,
                    },
                    Cell::Int(-7),
                ],
            ],
            affected_rows: 0,
            last_insert_id: None,
            elapsed_ms: 7,
            truncated: true,
            backend_notices_present: false,
        },
        ResultProvenance {
            result_id: ResultId(71),
            profile_id: ProfileId("mysql-local".to_owned()),
            profile_generation: ProfileGeneration(2),
            operation_id: OperationId(9),
            driver: DriverKind::MySql,
            completed_at_unix_ms: 0,
            duration_ms: 7,
        },
        ResultRetentionPolicy::mysql(3),
    )
}

#[test]
fn copy_cell_is_literal_and_has_no_header_or_final_newline() {
    let snapshot = snapshot();

    assert_eq!(
        copy_cell(&snapshot, 1, 0).as_deref(),
        Some("line\nbreak\\")
    );
    assert_eq!(copy_cell(&snapshot, 0, 1).as_deref(), Some(""));
    assert_eq!(copy_cell(&snapshot, 99, 0), None);
}

#[test]
fn selected_and_all_copy_keep_visible_order_escape_controls_and_end_in_one_lf() {
    let snapshot = snapshot();
    let selected = BTreeSet::from([2, 0]);

    assert_eq!(
        copy_selected_rows(&snapshot, &selected).as_deref(),
        Some("dup\tdup\na\\tb\t\n끝…[dbotter-truncated;original_len=9]\t-7\n")
    );
    assert_eq!(copy_selected_rows(&snapshot, &BTreeSet::new()), None);
    assert_eq!(
        copy_all_rows(&snapshot).as_deref(),
        Some(
            "dup\tdup\na\\tb\t\nline\\nbreak\\\\\ttrue\n끝…[dbotter-truncated;original_len=9]\t-7\n"
        )
    );
}

#[test]
fn native_result_inventory_exposes_every_installed_journey_action() {
    let context = Context::default();
    context.enable_accesskit();
    let mut harness = NativeUiHarness::p7_result();
    let output = context.run_ui(RawInput::default(), |ui| harness.show(ui));
    let update = output
        .platform_output
        .accesskit_update
        .expect("the native result harness must emit AccessKit");
    let ids = update
        .nodes
        .into_iter()
        .filter_map(|(_, node)| node.author_id().map(str::to_owned))
        .collect::<BTreeSet<_>>();

    for expected in [
        "result.table",
        "result.copy.cell",
        "result.copy.row",
        "result.copy.all",
        "result.export.csv",
        "result.export.tsv",
        "result.export.json",
    ] {
        assert!(ids.contains(expected), "missing result author id {expected}");
    }
}

#[test]
fn result_actions_follow_the_openai_control_geometry() {
    assert!(RESULT_ACTION_HEIGHT >= OpenAiTheme::MIN_CONTROL_HEIGHT);
    assert!(RESULT_ROW_HEIGHT >= OpenAiTheme::MIN_CONTROL_HEIGHT);

    let source = include_str!("../src/ui/result_view.rs");
    for forbidden in ["CornerRadius", "Shadow", "Color32::RED", "Color32::YELLOW"] {
        assert!(
            !source.contains(forbidden),
            "result UI must stay inside the OpenAI monochrome token system: {forbidden}"
        );
    }
}
