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
use eframe::egui::{Context, Event, Key, Modifiers, RawInput, accesskit};

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

    assert_eq!(copy_cell(&snapshot, 1, 0).as_deref(), Some("line\nbreak\\"));
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
        assert!(
            ids.contains(expected),
            "missing result author id {expected}"
        );
    }
}

#[test]
fn result_grid_exposes_filter_sort_and_keyboard_record_detail() {
    let context = Context::default();
    context.enable_accesskit();
    let mut harness = NativeUiHarness::p7_result();
    let initial = context.run_ui(RawInput::default(), |ui| harness.show(ui));
    let initial_update = initial
        .platform_output
        .accesskit_update
        .expect("the native result harness must emit AccessKit");
    let author_node = |author_id: &str| {
        initial_update
            .nodes
            .iter()
            .find_map(|(node_id, node)| {
                (node.author_id() == Some(author_id)).then_some((*node_id, node))
            })
            .unwrap_or_else(|| panic!("missing actual result AX id {author_id}"))
    };

    let (_, grid) = author_node("result.mode.grid");
    assert_eq!(grid.is_selected(), Some(true));
    let (record_id, record) = author_node("result.mode.record");
    assert_eq!(record.is_selected(), Some(false));
    assert!(record.supports_action(accesskit::Action::Focus));
    assert!(record.supports_action(accesskit::Action::Click));

    let (_, filter) = author_node("result.filter");
    assert_eq!(filter.role(), accesskit::Role::TextInput);
    assert!(filter.supports_action(accesskit::Action::Focus));
    let (_, sort) = author_node("result.sort.0");
    assert!(sort.supports_action(accesskit::Action::Click));

    let mut focus_record = RawInput::default();
    focus_record
        .events
        .push(Event::AccessKitActionRequest(accesskit::ActionRequest {
            action: accesskit::Action::Focus,
            target_tree: accesskit::TreeId::ROOT,
            target_node: record_id,
            data: None,
        }));
    let _ = context.run_ui(focus_record, |ui| harness.show(ui));
    let record_output = context.run_ui(
        RawInput {
            events: vec![Event::Key {
                key: Key::Enter,
                physical_key: Some(Key::Enter),
                pressed: true,
                repeat: false,
                modifiers: Modifiers::NONE,
            }],
            ..RawInput::default()
        },
        |ui| harness.show(ui),
    );
    let record_update = record_output
        .platform_output
        .accesskit_update
        .expect("record mode must emit AccessKit");
    let detail = record_update
        .nodes
        .iter()
        .find_map(|(_, node)| (node.author_id() == Some("result.record.field.0")).then_some(node))
        .expect("record mode must expose the selected row as typed fields");
    assert_eq!(detail.label(), Some("Record field value"));
    assert_eq!(detail.value(), Some("sample"));
    let record_mode = record_update
        .nodes
        .iter()
        .find_map(|(_, node)| (node.author_id() == Some("result.mode.record")).then_some(node))
        .expect("record mode action must remain visible");
    assert_eq!(record_mode.is_selected(), Some(true));
}

#[test]
fn a_new_result_has_one_valid_initial_cell_and_row_selection() {
    let context = Context::default();
    context.enable_accesskit();
    let mut harness = NativeUiHarness::p7_result();
    let output = context.run_ui(RawInput::default(), |ui| harness.show(ui));
    let update = output
        .platform_output
        .accesskit_update
        .expect("the native result harness must emit AccessKit");

    for author_id in ["result.copy.cell", "result.copy.row"] {
        let node = update
            .nodes
            .iter()
            .find_map(|(_, node)| (node.author_id() == Some(author_id)).then_some(node))
            .unwrap_or_else(|| panic!("missing {author_id}"));
        assert!(!node.is_disabled(), "{author_id} must start enabled");
        assert!(node.supports_action(accesskit::Action::Click));
    }
}

#[test]
fn result_actions_follow_the_openai_control_geometry() {
    assert!(std::hint::black_box(RESULT_ACTION_HEIGHT) >= OpenAiTheme::MIN_CONTROL_HEIGHT);
    assert!(std::hint::black_box(RESULT_ROW_HEIGHT) >= OpenAiTheme::MIN_CONTROL_HEIGHT);

    let source = include_str!("../src/ui/result_view.rs");
    for forbidden in ["CornerRadius", "Shadow", "Color32::RED", "Color32::YELLOW"] {
        assert!(
            !source.contains(forbidden),
            "result UI must stay inside the OpenAI monochrome token system: {forbidden}"
        );
    }
}
