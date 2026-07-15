#![cfg(feature = "desktop")]

use dbotter::ui::{NativeLayout, OpenAiTheme};

#[test]
fn openai_surface_forbids_decorative_effects() {
    assert!(!std::hint::black_box(OpenAiTheme::USES_GRADIENTS));
    assert!(!std::hint::black_box(OpenAiTheme::USES_SHADOWS));
    assert_eq!(OpenAiTheme::CORNER_RADIUS, 0.0);
}

#[test]
fn responsive_layout_keeps_editor_as_the_primary_narrow_surface() {
    assert_eq!(NativeLayout::columns_for_width(1180.0), 3);
    assert_eq!(NativeLayout::columns_for_width(839.0), 1);
}

#[test]
fn runtime_does_not_hard_code_every_execute_as_a_mutation() {
    let source = include_str!("../src/ui/runtime.rs");
    assert!(!source.contains("kind: OperationKind::ExecuteMutation,"));
}

#[test]
fn actual_app_owns_the_openai_theme_first_run_and_responsive_layout() {
    let source = include_str!("../src/ui/app.rs");

    assert!(source.contains("OpenAiTheme::apply(ui.ctx())"));
    assert!(source.contains("NativeLayout::columns_for_width"));
    assert!(source.contains("show_first_run"));
    assert!(source.contains("connection.mongodb.planned"));
}

#[test]
fn actual_app_owns_confirmed_delete_and_exact_active_operation_warning() {
    let source = include_str!("../src/ui/app.rs");

    assert!(source.contains("DeleteProfileRequest"));
    assert!(source.contains("profile.delete.active_warning"));
    assert!(source.contains("profile.delete.confirm"));
    assert!(source.contains("profile.delete.cancel"));
    assert!(source.contains("OperationKind::DeleteProfile"));
}
