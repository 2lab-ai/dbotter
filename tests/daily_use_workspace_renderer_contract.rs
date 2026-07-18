#![cfg(feature = "desktop")]

use std::collections::BTreeSet;

use dbotter::ui::MySqlExplorerState;
use eframe::egui::{Context, RawInput};

const APP_RENDERER_SOURCE: &str = include_str!("../src/ui/app.rs");
const UI_ENTRY_SOURCE: &str = include_str!("../src/ui/mod.rs");
const MYSQL_EXPLORER_SOURCE: &str = include_str!("../src/ui/mysql_explorer.rs");
const REDIS_EXPLORER_SOURCE: &str = include_str!("../src/ui/redis_explorer.rs");

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

fn has_bound_author_id(source: &str, id: &str) -> bool {
    let literal = format!("\"{id}\"");
    let lines = source.lines().collect::<Vec<_>>();
    lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.contains(&literal))
        .any(|(line, _)| {
            let start = line.saturating_sub(8);
            let end = (line + 9).min(lines.len());
            lines[start..end].join("\n").contains("author_id")
        })
}

fn author_ids(output: &eframe::egui::FullOutput) -> BTreeSet<String> {
    output
        .platform_output
        .accesskit_update
        .as_ref()
        .expect("workspace frame must emit AccessKit")
        .nodes
        .iter()
        .filter_map(|(_, node)| node.author_id().map(str::to_owned))
        .collect()
}

fn string_literals(source: &str) -> impl Iterator<Item = &str> {
    source.split('"').skip(1).step_by(2)
}

#[test]
fn navigator_renders_connection_and_object_filters_with_stable_author_ids() {
    let connections = function_body(APP_RENDERER_SOURCE, "connections_contents");
    assert!(
        connections.contains("TextEdit::singleline"),
        "the actual connection navigator needs a rendered filter field"
    );
    assert!(
        has_bound_author_id(connections, "navigator.connection-filter"),
        "the actual connection filter needs stable author id `navigator.connection-filter`"
    );

    let context = Context::default();
    context.enable_accesskit();
    let mut explorer = MySqlExplorerState::default();
    let output = context.run_ui(RawInput::default(), |ui| {
        let _ = explorer.show(ui);
    });
    assert!(
        author_ids(&output).contains("navigator.object-filter"),
        "the actual MySQL object filter needs stable author id `navigator.object-filter`"
    );
    assert!(
        has_bound_author_id(MYSQL_EXPLORER_SOURCE, "navigator.object-filter"),
        "the MySQL renderer must bind the shared object-filter identity"
    );
    assert!(
        has_bound_author_id(REDIS_EXPLORER_SOURCE, "navigator.object-filter"),
        "the Redis renderer must bind the same object-filter identity"
    );
}

#[test]
fn profile_cards_expose_opaque_ordered_selection_identity() {
    let connections = function_body(APP_RENDERER_SOURCE, "connections_contents");
    let card = function_body(APP_RENDERER_SOURCE, "profile_card");
    assert!(
        connections.contains(".enumerate()")
            && connections.contains("self.profile_card(ui, &profile, profile_index)"),
        "profile cards must retain their config-order identity through filtering"
    );
    assert!(
        card.contains("named_dynamic_author_id")
            && card.contains("format!(\"connection.profile.{profile_index}\")"),
        "each profile selection needs a unique opaque AX identifier"
    );
    assert!(
        !card.contains("format!(\"connection.profile.{}\", profile.id"),
        "profile AX identifiers must not expose user-owned profile text"
    );
}

#[test]
fn result_area_exposes_distinct_results_and_history_tabs() {
    let results = function_body(APP_RENDERER_SOURCE, "show_result_surface");
    assert!(
        results.matches("selectable_label").count() >= 2,
        "Results and History must be two actual selectable tabs"
    );
    for (label, author_id) in [
        ("Results", "result.tab.results"),
        ("History", "result.tab.history"),
    ] {
        assert!(
            results.contains(&format!("\"{label}\"")),
            "result area is missing the visible {label} tab"
        );
        assert!(
            has_bound_author_id(results, author_id),
            "result area is missing stable author id `{author_id}`"
        );
    }
}

#[test]
fn durable_workspace_controls_replace_session_only_retention_in_the_actual_renderer() {
    let persistence = function_body(APP_RENDERER_SOURCE, "show_workspace_persistence_controls");
    assert!(
        !persistence.contains("workspace.session-retention")
            && !persistence.contains("tabs and history clear on quit"),
        "the durable workspace must not advertise session-only retention"
    );
    let tabs = function_body(APP_RENDERER_SOURCE, "show_editor_tab_strip");
    assert!(
        tabs.contains("is_dirty()") && tabs.contains("editor.tab.discard"),
        "dirty editor tabs need visible state and an explicit discard confirmation"
    );
    for author_id in [
        "workspace.persistence.status",
        "workspace.persistence.toggle",
        "workspace.persistence.clear",
        "editor.save",
    ] {
        assert!(
            has_bound_author_id(persistence, author_id),
            "durable workspace control needs stable author id `{author_id}`"
        );
    }
    assert!(
        persistence.contains("OpenAiTheme::MIN_CONTROL_HEIGHT"),
        "all discrete persistence controls need the shared 44-point minimum"
    );
}

#[test]
fn editor_tab_strip_exposes_accessible_reorder_controls() {
    let tabs = function_body(APP_RENDERER_SOURCE, "show_editor_tab_strip");
    for author_id in ["editor.tab.move_left", "editor.tab.move_right"] {
        assert!(
            has_bound_author_id(tabs, author_id),
            "editor reorder control needs stable author id `{author_id}`"
        );
    }
    assert!(
        tabs.contains("reorder_editor_tab") && tabs.contains("OpenAiTheme::MIN_CONTROL_HEIGHT"),
        "editor reorder controls must invoke the model and retain 44-point targets"
    );
}

#[test]
fn result_renderer_keeps_multiple_execution_outputs_selectable() {
    let results = function_body(APP_RENDERER_SOURCE, "show_result_surface");
    assert!(
        results.contains("selected_editor_tab_id")
            && results.contains("result_tabs_for_editor")
            && results.contains("select_result_tab")
            && results.contains("close_result_tab"),
        "the actual result surface must scope retained outputs to the selected editor, then render, select and close them"
    );
    assert!(
        results.contains("result.output.") && results.contains("result.output.close."),
        "each retained output and its close action need stable dynamic author-id prefixes"
    );
}

#[test]
fn mysql_relations_expose_structure_and_safe_data_actions() {
    for (label, author_id) in [
        ("Structure", "navigator.object.structure"),
        ("Data", "navigator.object.data"),
    ] {
        assert!(
            MYSQL_EXPLORER_SOURCE.contains(&format!("\"{label}\"")),
            "relation rows must expose a visible {label} action"
        );
        assert!(
            has_bound_author_id(MYSQL_EXPLORER_SOURCE, author_id),
            "relation {label} action needs stable author id `{author_id}`"
        );
    }
    assert!(
        MYSQL_EXPLORER_SOURCE.contains("View data unavailable"),
        "views must state why Data is unavailable instead of pretending to execute"
    );
}

#[test]
fn actual_app_restores_and_saves_geometry_through_native_storage() {
    assert!(
        UI_ENTRY_SOURCE.contains("creation.storage"),
        "native app creation must restore retained workspace geometry"
    );
    assert!(
        APP_RENDERER_SOURCE.contains("fn save(")
            && APP_RENDERER_SOURCE.contains("WORKSPACE_GEOMETRY_STORAGE_KEY"),
        "the native app must save geometry through eframe storage"
    );
    assert!(
        APP_RENDERER_SOURCE.contains("fn persist_egui_memory(&self) -> bool")
            && APP_RENDERER_SOURCE.contains("fn auto_save_interval(&self) -> Duration"),
        "native persistence must disable generic egui memory and retain the two-second cadence"
    );
}

#[test]
fn private_history_renderer_searches_full_source_but_bounds_visible_rows() {
    let history = function_body(APP_RENDERER_SOURCE, "show_result_surface");
    for author_id in ["history.search", "history.clear"] {
        assert!(
            has_bound_author_id(history, author_id),
            "private history control needs stable author id `{author_id}`"
        );
    }
    for metric in [
        "duration_ms()",
        "returned_rows()",
        "affected_rows()",
        "truncated()",
    ] {
        assert!(
            history.contains(metric),
            "history rows must render typed metric `{metric}`"
        );
    }
    assert!(
        APP_RENDERER_SOURCE.contains("MAX_PREVIEW_CHARACTERS")
            && history.contains("workspace_history_source_preview"),
        "history must search retained source while rendering a bounded single-line preview"
    );
}

#[test]
fn workspace_never_advertises_unimplemented_gis_or_erd_views() {
    for literal in string_literals(APP_RENDERER_SOURCE)
        .chain(string_literals(MYSQL_EXPLORER_SOURCE))
        .chain(string_literals(REDIS_EXPLORER_SOURCE))
    {
        let normalized = literal.to_ascii_lowercase();
        let words = normalized
            .split(|character: char| !character.is_ascii_alphanumeric())
            .collect::<Vec<_>>();
        for forbidden in ["gis", "map", "erd"] {
            assert!(
                !words.contains(&forbidden),
                "P0 renderer must not advertise fake {forbidden} controls"
            );
        }
        assert!(
            !normalized.contains("er-diagram"),
            "P0 renderer must not advertise a fake ER-diagram control"
        );
    }
}
