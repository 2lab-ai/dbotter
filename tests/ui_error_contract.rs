#![cfg(feature = "desktop")]

use dbotter::ui::UiEvent;

#[allow(dead_code)]
fn every_resource_and_common_failure_keeps_the_typed_public_error(event: UiEvent) {
    match event {
        UiEvent::CatalogPageFailed { error, .. }
        | UiEvent::RedisKeysFailed { error, .. }
        | UiEvent::RedisKeyInspectFailed { error, .. }
        | UiEvent::OperationFailed { error, .. } => {
            let _ = error.code;
            let _ = error.recovery;
        }
        _ => {}
    }
}

#[allow(dead_code)]
fn create_failure_is_correlated_only_by_draft_identity(event: UiEvent) {
    if let UiEvent::ProfileCreateFailed {
        draft_id, error, ..
    } = event
    {
        let _ = draft_id;
        let _ = error.recovery;
    }
}

#[test]
fn runtime_source_never_synthesizes_a_saved_profile_id_for_draft_failure() {
    let runtime = include_str!("../src/ui/runtime.rs");
    assert!(!runtime.contains("ProfileId(format!(\"draft-"));
}
