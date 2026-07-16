#![cfg(feature = "desktop")]

use dbotter::ui::{
    CompactFallback, FallbackSurface, LayoutMode, NativeLayout, OpenAiTheme, Pane, SplitLayout,
    WorkspaceGeometry,
};

fn assert_monochrome(color: [u8; 4]) {
    assert_eq!(color[0], color[1]);
    assert_eq!(color[1], color[2]);
}

#[test]
fn openai_shell_tokens_are_monochrome_square_and_restrained() {
    for color in [
        OpenAiTheme::CANVAS,
        OpenAiTheme::INK,
        OpenAiTheme::SECONDARY_INK,
        OpenAiTheme::DISABLED_INK,
        OpenAiTheme::BOUNDARY,
        OpenAiTheme::PRIMARY_FILL,
        OpenAiTheme::PRIMARY_INK,
    ] {
        assert_monochrome(color);
    }

    assert_eq!(OpenAiTheme::PRIMARY_FILL, OpenAiTheme::INK);
    assert_eq!(OpenAiTheme::PRIMARY_INK, OpenAiTheme::CANVAS);
    assert!(OpenAiTheme::INK[0] < OpenAiTheme::SECONDARY_INK[0]);
    assert!(OpenAiTheme::SECONDARY_INK[0] < OpenAiTheme::DISABLED_INK[0]);
    assert_eq!(OpenAiTheme::CORNER_RADIUS, 0.0);
    assert!(!std::hint::black_box(OpenAiTheme::USES_GRADIENTS));
    assert!(!std::hint::black_box(OpenAiTheme::USES_SHADOWS));
    assert_eq!(OpenAiTheme::MOTION_DURATION_RANGE_MS, 150..=250);
    assert_eq!(OpenAiTheme::REDUCED_MOTION_DURATION_MS, 0);

    assert_eq!(NativeLayout::ACTION_MIN_SIZE, [44.0, 44.0]);
    assert_eq!(NativeLayout::ADJACENT_ACTION_GAP, 8.0);
    assert!((28.0..=32.0).contains(&NativeLayout::DENSE_ROW_HEIGHT));
}

#[test]
fn wide_shell_keeps_navigator_editor_results_and_status_context_simultaneously() {
    assert_eq!(NativeLayout::NAVIGATOR_DEFAULT_WIDTH, 280.0);
    assert_eq!(NativeLayout::NAVIGATOR_WIDTH_RANGE, 220.0..=420.0);
    assert_eq!(NativeLayout::CENTER_MIN_WIDTH, 520.0);
    assert_eq!(NativeLayout::SUBORDINATE_MIN_EXTENT, 240.0);
    assert_eq!(NativeLayout::PANE_MIN_EXTENT, 160.0);
    assert_eq!(NativeLayout::DEFAULT_EDITOR_SHARE, 0.60);
    assert_eq!(NativeLayout::SPLITTER_KEYBOARD_STEP, 5.0);
    assert_eq!(NativeLayout::SPLITTER_ACCESSIBLE_HIT_EXTENT, 44.0);
    assert_eq!(
        NativeLayout::P0_REGION_IDS,
        [
            "navigator",
            "object-editor-tabs",
            "result-history-tabs",
            "status-action-context",
        ]
    );

    let wide = NativeLayout::resolve(1440.0, 900.0, WorkspaceGeometry::default());
    assert_eq!(wide.mode(), LayoutMode::Wide);
    assert!(wide.navigator_is_persistent());
    assert_eq!(wide.navigator_width(), Some(280.0));
    assert!(wide.center_width() >= NativeLayout::CENTER_MIN_WIDTH);
    assert!(wide.subordinate_extent() >= NativeLayout::SUBORDINATE_MIN_EXTENT);
    assert_eq!(wide.visible_region_ids(), NativeLayout::P0_REGION_IDS);
    assert!(wide.status_action_context_visible());
    assert!(!wide.uses_horizontal_overflow());
}

#[test]
fn valid_geometry_is_retained_and_corrupt_geometry_resets_to_documented_defaults() {
    let valid = WorkspaceGeometry::restore(360.0, 0.70, false);
    assert_eq!(valid.navigator_width(), 360.0);
    assert_eq!(valid.editor_share(), 0.70);
    assert!(!valid.inspector_visible());

    assert_eq!(
        WorkspaceGeometry::restore(220.0, 0.60, true).navigator_width(),
        220.0
    );
    assert_eq!(
        WorkspaceGeometry::restore(420.0, 0.60, true).navigator_width(),
        420.0
    );

    for corrupt in [
        WorkspaceGeometry::restore(219.0, 0.60, true),
        WorkspaceGeometry::restore(421.0, 0.60, true),
        WorkspaceGeometry::restore(f32::NAN, 0.60, true),
        WorkspaceGeometry::restore(280.0, f32::NAN, true),
        WorkspaceGeometry::restore(280.0, 1.01, true),
    ] {
        assert_eq!(corrupt, WorkspaceGeometry::default());
    }
}

#[test]
fn split_boundary_retains_160_and_collapses_159_with_named_restore() {
    let exact_editor = SplitLayout::from_editor_extent(400.0, 160.0);
    assert_eq!(exact_editor.editor_extent(), Some(160.0));
    assert_eq!(exact_editor.subordinate_extent(), Some(240.0));
    assert_eq!(exact_editor.editor_restore_label(), None);

    let mut collapsed_editor = SplitLayout::from_editor_extent(400.0, 159.0);
    assert_eq!(collapsed_editor.editor_extent(), None);
    assert_eq!(
        collapsed_editor.editor_restore_label(),
        Some("Restore editor")
    );
    collapsed_editor.restore(Pane::Editor);
    assert_eq!(collapsed_editor.editor_extent(), Some(240.0));
    assert_eq!(collapsed_editor.subordinate_extent(), Some(160.0));

    let exact_subordinate = SplitLayout::from_subordinate_extent(400.0, 160.0);
    assert_eq!(exact_subordinate.subordinate_extent(), Some(160.0));
    assert_eq!(exact_subordinate.editor_extent(), Some(240.0));

    let mut collapsed_subordinate = SplitLayout::from_subordinate_extent(400.0, 159.0);
    assert_eq!(collapsed_subordinate.subordinate_extent(), None);
    assert_eq!(
        collapsed_subordinate.subordinate_restore_label(),
        Some("Restore results/history")
    );
    collapsed_subordinate.restore(Pane::Subordinate);
    assert_eq!(collapsed_subordinate.editor_extent(), Some(240.0));
    assert_eq!(collapsed_subordinate.subordinate_extent(), Some(160.0));
}

#[test]
fn splitter_keyboard_adjustment_and_reset_are_exact() {
    let mut split = SplitLayout::reset(600.0);
    assert_eq!(split.editor_extent(), Some(360.0));
    assert_eq!(split.subordinate_extent(), Some(240.0));

    split.keyboard_adjust(1);
    assert_eq!(split.editor_extent(), Some(365.0));
    assert_eq!(split.subordinate_extent(), Some(235.0));
    split.keyboard_adjust(-1);
    assert_eq!(split.editor_extent(), Some(360.0));
    assert_eq!(split.subordinate_extent(), Some(240.0));

    split.keyboard_adjust(4);
    split.reset_to_default();
    assert_eq!(split.editor_extent(), Some(360.0));
    assert_eq!(split.subordinate_extent(), Some(240.0));
}

#[test]
fn compact_shell_uses_named_single_surface_fallback_and_restores_focus() {
    let compact = NativeLayout::resolve(840.0, 560.0, WorkspaceGeometry::default());
    assert_eq!(compact.mode(), LayoutMode::Compact);
    assert!(!compact.navigator_is_persistent());
    assert!(compact.uses_named_navigator_drawer());
    assert!(compact.uses_one_at_a_time_inspector());
    assert!(compact.status_action_context_visible());
    assert!(!compact.uses_horizontal_overflow());

    let mut fallback = CompactFallback::default();
    fallback.open(FallbackSurface::Navigator, "editor.tab.current");
    assert_eq!(fallback.visible_surface(), Some(FallbackSurface::Navigator));
    assert_eq!(fallback.restore_focus_id(), Some("editor.tab.current"));
    assert!(!fallback.covers_status_action_context());

    fallback.open(FallbackSurface::Inspector, "result.tab.current");
    assert_eq!(fallback.visible_surface(), Some(FallbackSurface::Inspector));
    assert_eq!(fallback.restore_focus_id(), Some("result.tab.current"));
    assert!(!fallback.covers_status_action_context());
    assert_eq!(fallback.close(), Some("result.tab.current".to_owned()));
    assert_eq!(fallback.visible_surface(), None);
}

#[test]
fn p0_workspace_registry_never_advertises_fake_gis_or_erd_controls() {
    let controls = NativeLayout::P0_WORKSPACE_VIEW_IDS;
    for required in [
        "data",
        "structure",
        "new-editor",
        "grid",
        "record",
        "history",
        "review",
        "redis-value",
    ] {
        assert!(
            controls.contains(&required),
            "missing P0 surface {required}"
        );
    }
    for forbidden in ["gis", "map", "erd", "er-diagram"] {
        assert!(
            controls
                .iter()
                .all(|control| !control.to_ascii_lowercase().contains(forbidden)),
            "P0 must not advertise fake {forbidden} controls"
        );
    }
}
