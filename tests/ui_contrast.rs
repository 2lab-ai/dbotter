#![cfg(feature = "desktop")]

use dbotter::ui::{NativeLayout, OpenAiTheme};

#[test]
fn openai_palette_and_focus_geometry_meet_the_frozen_numerical_contract() {
    assert_eq!(OpenAiTheme::CANVAS, [255, 255, 255, 255]);
    assert_eq!(OpenAiTheme::INK, [0, 0, 0, 255]);
    assert_eq!(OpenAiTheme::SECONDARY_INK, [102, 102, 102, 255]);
    assert_eq!(OpenAiTheme::DISABLED_INK, [145, 145, 145, 255]);
    assert!(OpenAiTheme::contrast(OpenAiTheme::INK, OpenAiTheme::CANVAS) >= 4.5);
    assert!(OpenAiTheme::contrast(OpenAiTheme::SECONDARY_INK, OpenAiTheme::CANVAS) >= 4.5);
    assert!(OpenAiTheme::contrast(OpenAiTheme::BOUNDARY, OpenAiTheme::CANVAS) >= 3.0);
    assert!(std::hint::black_box(OpenAiTheme::FOCUS_STROKE_WIDTH) >= 2.0);
    assert!(std::hint::black_box(OpenAiTheme::MIN_CONTROL_HEIGHT) >= 44.0);
    assert_eq!(OpenAiTheme::CORNER_RADIUS, 0.0);
}

#[test]
fn native_layout_uses_three_columns_and_a_bounded_collapse_point() {
    assert!((248.0..=280.0).contains(&NativeLayout::CONNECTIONS_WIDTH));
    assert!((280.0..=340.0).contains(&NativeLayout::EXPLORER_WIDTH));
    assert!((820.0..=860.0).contains(&NativeLayout::COLLAPSE_WIDTH));
}

#[test]
fn native_status_and_error_ui_stays_textual_and_monochrome() {
    let sources = [
        include_str!("../src/ui/app.rs"),
        include_str!("../src/ui/profile_form.rs"),
    ];
    for source in sources {
        for forbidden in ["Color32::RED", "Color32::YELLOW"] {
            assert!(
                !source.contains(forbidden),
                "OpenAI native UI must not use chromatic status color {forbidden}"
            );
        }
    }
    assert!(sources[0].contains("Warning: result is truncated"));
    assert!(sources[0].contains("Error: <missing>"));
    assert!(sources[1].contains("Error: {error}"));
}
