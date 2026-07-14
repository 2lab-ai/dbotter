use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

use dbotter::model::OperationKind;

#[test]
fn draft_safe_context_has_only_the_frozen_two_ids() {
    let source =
        fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/public_error.rs"))
            .expect("public error source");
    let draft = section(&source, "Draft {", "Profile {");

    assert!(draft.contains("draft_id: DraftId"));
    assert!(draft.contains("operation_id: OperationId"));
    assert!(!draft.contains("active_operation"));
    assert!(!source.contains("draft_with_active"));
}

#[test]
fn operation_kind_is_single_source_generated_and_independently_frozen() {
    let source = fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/model.rs"))
        .expect("model source");
    assert!(source.contains("macro_rules! define_operation_kinds"));

    let expected = [
        (OperationKind::LoadConfiguration, "load_configuration"),
        (OperationKind::ReloadConfiguration, "reload_configuration"),
        (OperationKind::MigrateConfiguration, "migrate_configuration"),
        (OperationKind::CreateProfile, "create_profile"),
        (OperationKind::UpdateProfile, "update_profile"),
        (OperationKind::DeleteProfile, "delete_profile"),
        (OperationKind::TestDraftConnection, "test_draft_connection"),
        (OperationKind::ConnectProfile, "connect_profile"),
        (OperationKind::DisconnectProfile, "disconnect_profile"),
        (OperationKind::ReconnectProfile, "reconnect_profile"),
        (OperationKind::ExecuteRead, "execute_read"),
        (OperationKind::ExecuteMutation, "execute_mutation"),
        (OperationKind::BrowseMySql, "browse_my_sql"),
        (OperationKind::BrowseRedis, "browse_redis"),
        (OperationKind::InspectRedis, "inspect_redis"),
        (OperationKind::ExportResult, "export_result"),
        (OperationKind::ShutdownRuntime, "shutdown_runtime"),
    ];

    assert_eq!(OperationKind::ALL.len(), expected.len());
    assert_eq!(OperationKind::COUNT, expected.len());
    let mut unique = HashSet::new();
    for ((actual, expected_wire), listed) in expected.into_iter().zip(OperationKind::ALL) {
        assert_eq!(*listed, actual);
        assert!(unique.insert(listed));
        assert_eq!(
            serde_json::to_value(actual).expect("operation serializes"),
            expected_wire
        );
        assert_eq!(actual.exhaustive_index(), unique.len() - 1);
    }
}

fn section<'a>(source: &'a str, start: &str, end: &str) -> &'a str {
    let start = source.find(start).expect("section start");
    let tail = &source[start..];
    let end = tail.find(end).expect("section end");
    &tail[..end]
}
