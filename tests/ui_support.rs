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
