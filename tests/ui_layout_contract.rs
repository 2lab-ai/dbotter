#![cfg(feature = "desktop")]

use dbotter::ui::{
    CompactFallback, FallbackSurface, LayoutMode, NativeLayout, OpenAiTheme, Pane, SplitLayout,
    WorkspaceGeometry,
};

const APP_RENDERER_SOURCE: &str = include_str!("../src/ui/app.rs");
const MYSQL_EXPLORER_SOURCE: &str = include_str!("../src/ui/mysql_explorer.rs");

fn function_body<'a>(source: &'a str, name: &str) -> &'a str {
    let signature = format!("fn {name}(");
    let function_start = source
        .find(&signature)
        .unwrap_or_else(|| panic!("renderer is missing `{signature}`"));
    let body_start = source[function_start..]
        .find('{')
        .map(|offset| function_start + offset)
        .unwrap_or_else(|| panic!("renderer function `{name}` has no body"));

    let mut depth = 0_usize;
    for (offset, byte) in source.as_bytes()[body_start..].iter().enumerate() {
        match byte {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return &source[body_start..=body_start + offset];
                }
            }
            _ => {}
        }
    }

    panic!("renderer function `{name}` has an unterminated body")
}

fn without_ascii_whitespace(source: &str) -> String {
    source
        .chars()
        .filter(|character| !character.is_ascii_whitespace())
        .collect()
}

fn has_bound_author_id(source: &str, id: &str) -> bool {
    let literal = format!("\"{id}\"");
    source
        .lines()
        .enumerate()
        .filter(|(_, line)| line.contains(&literal))
        .any(|(line, _)| {
            let lines = source.lines().collect::<Vec<_>>();
            let start = line.saturating_sub(8);
            let end = (line + 9).min(lines.len());
            lines[start..end].join("\n").contains("author_id")
        })
}

fn string_literals(source: &str) -> impl Iterator<Item = &str> {
    source.split('"').skip(1).step_by(2)
}

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
fn actual_renderer_routes_native_layout_geometry_and_compact_fallback() {
    let show_native = function_body(APP_RENDERER_SOURCE, "show_native");

    assert!(
        show_native.contains("NativeLayout::resolve"),
        "show_native must resolve the real 1440x900/840x560 shell, not only test the pure layout API"
    );
    assert!(
        show_native.contains("WorkspaceGeometry"),
        "show_native must supply the selected workspace geometry to NativeLayout::resolve"
    );
    assert!(
        show_native.contains("CompactFallback"),
        "show_native must route compact rendering through the one-at-a-time fallback state"
    );
    assert!(
        !show_native.contains("NativeLayout::columns_for_width(ui.available_width())"),
        "the legacy width-only three-column switch must be removed"
    );
    assert!(
        !show_native.contains("self.narrow_navigation(ui)"),
        "compact rendering must use the named fallback instead of collapsing headers"
    );
}

#[test]
fn actual_renderer_binds_stable_region_and_compact_control_author_ids() {
    for id in [
        "navigator",
        "object-editor-tabs",
        "result-history-tabs",
        "status-action-context",
        "navigator.open",
        "inspector.open",
    ] {
        assert!(
            has_bound_author_id(APP_RENDERER_SOURCE, id),
            "actual renderer must bind stable author id `{id}` to a rendered response"
        );
    }

    let has_shared_close = ["compact.close", "fallback.close"]
        .into_iter()
        .any(|id| has_bound_author_id(APP_RENDERER_SOURCE, id));
    let has_surface_closes = has_bound_author_id(APP_RENDERER_SOURCE, "navigator.close")
        && has_bound_author_id(APP_RENDERER_SOURCE, "inspector.close");
    assert!(
        has_shared_close || has_surface_closes,
        "the one-at-a-time compact surface needs either one stable shared close control or stable close controls for both surfaces"
    );
}

#[test]
fn actual_wide_renderer_owns_one_resizable_navigator_center_subordinate_and_status() {
    let compact_source = without_ascii_whitespace(APP_RENDERER_SOURCE);
    let show_native = without_ascii_whitespace(function_body(APP_RENDERER_SOURCE, "show_native"));

    assert!(
        !compact_source.contains("Panel::left(\"connections\")")
            && !compact_source.contains("Panel::right(\"explorer\")"),
        "connections and object explorer must be composed inside one navigator, not fixed sibling side panels"
    );
    assert!(
        !show_native.contains("self.connections(ui);")
            && !show_native.contains("self.explorer(ui);"),
        "show_native must not coordinate the legacy fixed connections/explorer pair"
    );

    let navigator_start = compact_source
        .find("Panel::left(\"navigator\")")
        .expect("wide shell must own a left panel with the stable navigator identity");
    let navigator_builder = compact_source[navigator_start..]
        .split(".show(")
        .next()
        .expect("navigator panel must have a builder before show");
    assert!(
        navigator_builder.contains(".resizable(true)"),
        "the unified navigator must be user-resizable"
    );
    assert!(
        navigator_builder.contains("NativeLayout::NAVIGATOR_DEFAULT_WIDTH")
            && navigator_builder.contains("NativeLayout::NAVIGATOR_WIDTH_RANGE"),
        "the actual navigator must use the documented 280 default and 220..=420 range"
    );
    assert!(
        compact_source.contains("connections_contents(ui)")
            && compact_source.contains("explorer_contents(ui)"),
        "the unified navigator must retain connection selection and object exploration together"
    );

    assert!(
        compact_source.contains("CentralPanel::default()"),
        "the editor must remain the center workspace"
    );
    assert!(
        compact_source.matches("Panel::bottom(").count() >= 2,
        "the renderer needs distinct subordinate result/history and persistent status bottom panels"
    );
    assert!(
        compact_source.contains("SplitLayout") && compact_source.contains("subordinate_extent"),
        "the actual editor/result coordinator must consume the pure split layout"
    );
    assert!(
        show_native.contains("show_status_strip"),
        "show_native must always coordinate the persistent bottom status/action context"
    );
}

#[test]
fn actual_renderer_removes_mysql_width_overflow_and_forbids_fake_spatial_views() {
    let compact_mysql = without_ascii_whitespace(MYSQL_EXPLORER_SOURCE);
    assert!(
        !compact_mysql.contains("set_min_width(300"),
        "the MySQL explorer must fit the 280pt unified navigator instead of forcing 300pt content"
    );

    for literal in
        string_literals(APP_RENDERER_SOURCE).chain(string_literals(MYSQL_EXPLORER_SOURCE))
    {
        let normalized = literal.to_ascii_lowercase();
        let words = normalized
            .split(|character: char| !character.is_ascii_alphanumeric())
            .collect::<Vec<_>>();
        for forbidden in ["gis", "map", "erd"] {
            assert!(
                !words.contains(&forbidden),
                "actual renderer must not advertise fake {forbidden} controls"
            );
        }
        assert!(
            !normalized.contains("er-diagram"),
            "actual renderer must not advertise fake ER-diagram controls"
        );
    }
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
