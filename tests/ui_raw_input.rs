#![cfg(feature = "desktop")]

use dbotter::model::{ProfileGeneration, ProfileId};
use dbotter::ui::{NativeUiHarness, UiModel, WorkspaceKey};
use eframe::egui::{Context, RawInput};

#[test]
fn egui_035_raw_input_harness_always_emits_an_accesskit_update() {
    let context = Context::default();
    context.enable_accesskit();
    let mut harness = NativeUiHarness::first_run();

    let output = context.run_ui(RawInput::default(), |ui| harness.show(ui));

    assert!(output.platform_output.accesskit_update.is_some());
}

#[test]
fn profile_workspaces_are_isolated_by_profile_and_generation() {
    let profile = ProfileId("mysql-local".to_owned());
    let first = WorkspaceKey::new(profile.clone(), ProfileGeneration(7));
    let replacement = WorkspaceKey::new(profile, ProfileGeneration(8));
    let mut model = UiModel::default();

    model.workspace_mut(first.clone()).editor_text = "SELECT 'first'".to_owned();
    model.workspace_mut(replacement.clone()).editor_text = "SELECT 'replacement'".to_owned();

    assert_eq!(
        model
            .workspace(&first)
            .map(|workspace| workspace.editor_text.as_str()),
        Some("SELECT 'first'")
    );
    assert_eq!(
        model
            .workspace(&replacement)
            .map(|workspace| workspace.editor_text.as_str()),
        Some("SELECT 'replacement'")
    );
}
