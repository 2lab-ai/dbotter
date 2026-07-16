#![cfg(feature = "desktop")]

use std::collections::BTreeSet;

use dbotter::execution::ExecutionTargetError;
use dbotter::model::{
    ConnectionProfile, CredentialMode, DriverAvailability, DriverKind, OperationId, OperationKind,
    ProfileAccess, ProfileEnvironment, ProfileGeneration, ProfileId, ProfileSafetyPosture,
    QueryLanguage, RedisTlsConfig, TlsMode,
};
use dbotter::ui::{
    EditorCursor, EditorIntent, EditorSurface, EditorValidationError, ProfileSnapshot,
    ResultAreaTab, UiCommand, UiModel, WorkspaceKey, build_execute_intent,
    classify_execute_operation, editor_target_label, pending_cancel_intent,
};
use eframe::egui::{Context, Event, Key, Modifiers, RawInput, accesskit};

fn profile(
    id: &str,
    generation: u64,
    driver: DriverKind,
    database: Option<&str>,
    tls: TlsMode,
) -> ProfileSnapshot {
    let port = match driver {
        DriverKind::MySql => 3306,
        DriverKind::Redis => 6379,
        DriverKind::MongoDb => 27017,
    };
    let persisted = ConnectionProfile {
        id: id.to_owned(),
        name: format!("{id} display"),
        driver,
        host: "db.internal".to_owned(),
        port,
        database: database.map(str::to_owned),
        username: None,
        safety: ProfileSafetyPosture::new(
            ProfileEnvironment::Development,
            ProfileAccess::ReadWrite,
        ),
        tls,
        credential_mode: CredentialMode::None,
        secret_env: None,
        redis_tls: RedisTlsConfig::default(),
    };
    ProfileSnapshot {
        id: ProfileId(id.to_owned()),
        generation: ProfileGeneration(generation),
        name: persisted.name.clone(),
        driver,
        endpoint: persisted.redacted_endpoint(),
        database: persisted.database.clone(),
        availability: DriverAvailability::Ready,
        planned_reason: None,
        has_current_session_secret: false,
        environment_availability: None,
        persisted,
    }
}

#[test]
fn workspace_defaults_and_explicit_selection_produce_the_exact_command_tuple() {
    let profile = profile(
        "mysql-a",
        7,
        DriverKind::MySql,
        Some("app"),
        TlsMode::Required,
    );
    let key = WorkspaceKey::new(profile.id.clone(), profile.generation);
    let mut model = UiModel::default();
    let workspace = model.workspace_mut(key);
    workspace.editor_text = "SELECT 1; SELECT 2;".to_owned();

    assert_eq!(workspace.row_limit, "500");
    assert_eq!(workspace.timeout_seconds, "30");

    let intent = build_execute_intent(
        &profile,
        workspace,
        EditorCursor::with_selection(workspace.editor_text.chars().count(), 0..9),
    )
    .expect("the selected single statement is valid");

    assert_eq!(intent.profile_id(), &ProfileId("mysql-a".to_owned()));
    assert_eq!(intent.profile_generation(), ProfileGeneration(7));
    assert_eq!(intent.language(), QueryLanguage::Sql);
    assert_eq!(intent.text(), "SELECT 1;");
    assert_eq!(intent.row_limit(), 500);
    assert_eq!(intent.timeout_ms(), 30_000);
    assert_eq!(intent.operation_kind(), OperationKind::ExecuteRead);

    match intent.into_ui_command(OperationId(91)) {
        UiCommand::Execute {
            operation_id,
            profile_id,
            profile_generation,
            language,
            text,
            row_limit,
            timeout_ms,
        } => {
            assert_eq!(operation_id, OperationId(91));
            assert_eq!(profile_id, ProfileId("mysql-a".to_owned()));
            assert_eq!(profile_generation, ProfileGeneration(7));
            assert_eq!(language, QueryLanguage::Sql);
            assert_eq!(text, "SELECT 1;");
            assert_eq!(row_limit, 500);
            assert_eq!(timeout_ms, 30_000);
        }
        other => panic!("expected Execute, got {other:?}"),
    }
}

#[test]
fn invalid_explicit_selection_and_execute_limits_never_fall_back() {
    let profile = profile("mysql-a", 7, DriverKind::MySql, None, TlsMode::Disabled);
    let mut model = UiModel::default();
    let workspace = model.workspace_mut(WorkspaceKey::new(profile.id.clone(), profile.generation));
    workspace.editor_text = "SELECT 1; SELECT 2;".to_owned();

    let selection_error =
        build_execute_intent(&profile, workspace, EditorCursor::with_selection(15, 9..10))
            .expect_err("a whitespace selection must not fall back to the caret");
    assert_eq!(
        selection_error,
        EditorValidationError::Target(ExecutionTargetError::NoCurrentStatement)
    );
    assert_eq!(selection_error.control_id(), "editor.input");

    workspace.row_limit = "0".to_owned();
    let row_error = build_execute_intent(&profile, workspace, EditorCursor::caret(0))
        .expect_err("zero rows are invalid");
    assert_eq!(row_error.control_id(), "editor.row_limit");

    workspace.row_limit = "500".to_owned();
    workspace.timeout_seconds = "301".to_owned();
    let timeout_error = build_execute_intent(&profile, workspace, EditorCursor::caret(0))
        .expect_err("timeouts over the cap are invalid");
    assert_eq!(timeout_error.control_id(), "editor.timeout");
}

#[test]
fn redis_caret_uses_one_physical_line_and_keeps_the_correct_language() {
    let profile = profile(
        "redis-b",
        11,
        DriverKind::Redis,
        Some("3"),
        TlsMode::Required,
    );
    let mut model = UiModel::default();
    let workspace = model.workspace_mut(WorkspaceKey::new(profile.id.clone(), profile.generation));
    workspace.editor_text = "PING\nSET key \"a;b\"".to_owned();
    let caret = "PING\nSET".chars().count();

    let intent = build_execute_intent(&profile, workspace, EditorCursor::caret(caret))
        .expect("the Redis physical line is valid");

    assert_eq!(intent.profile_id(), &ProfileId("redis-b".to_owned()));
    assert_eq!(intent.profile_generation(), ProfileGeneration(11));
    assert_eq!(intent.language(), QueryLanguage::RedisCommand);
    assert_eq!(intent.text(), "SET key \"a;b\"");
    assert_eq!(intent.operation_kind(), OperationKind::ExecuteMutation);

    let target = editor_target_label(&profile);
    for expected in [
        "redis-b display",
        "redis-b",
        "redis",
        "redis://db.internal:6379",
        "Redis DB 3",
        "TLS Required",
    ] {
        assert!(
            target.contains(expected),
            "target omitted {expected}: {target}"
        );
    }
}

#[test]
fn operation_classification_is_conservative_for_side_effects() {
    assert_eq!(
        classify_execute_operation(QueryLanguage::Sql, "SELECT 1"),
        OperationKind::ExecuteRead
    );
    assert_eq!(
        classify_execute_operation(QueryLanguage::Sql, "SELECT 1 INTO OUTFILE '/tmp/x'"),
        OperationKind::ExecuteMutation
    );
    assert_eq!(
        classify_execute_operation(QueryLanguage::Sql, "UPDATE t SET value = 1"),
        OperationKind::ExecuteMutation
    );
    assert_eq!(
        classify_execute_operation(QueryLanguage::Sql, r"SELECT 'a\'"),
        OperationKind::ExecuteRead,
        "a SELECT valid under NO_BACKSLASH_ESCAPES must use read UI semantics"
    );
    assert_eq!(
        classify_execute_operation(
            QueryLanguage::Sql,
            "WITH source AS (SELECT 1) UPDATE t SET value = 1"
        ),
        OperationKind::ExecuteMutation,
        "a CTE mutation must remain a mutation under every relevant session mode"
    );
    assert_eq!(
        classify_execute_operation(QueryLanguage::RedisCommand, "GET key"),
        OperationKind::ExecuteRead
    );
    assert_eq!(
        classify_execute_operation(QueryLanguage::RedisCommand, "SET key value"),
        OperationKind::ExecuteMutation
    );
    assert_eq!(
        classify_execute_operation(QueryLanguage::RedisCommand, "FUTURECOMMAND key"),
        OperationKind::ExecuteMutation
    );
}

#[test]
fn mode_dependent_mysql_selection_builds_a_read_ui_intent() {
    let profile = profile("mysql-mode", 12, DriverKind::MySql, None, TlsMode::Disabled);
    let mut model = UiModel::default();
    let workspace = model.workspace_mut(WorkspaceKey::new(profile.id.clone(), profile.generation));
    workspace.editor_text = r"SELECT 'a\'".to_owned();
    let intent = build_execute_intent(
        &profile,
        workspace,
        EditorCursor::with_selection(
            workspace.editor_text.chars().count(),
            0..workspace.editor_text.chars().count(),
        ),
    )
    .expect("the exact server mode will decide this selected statement");

    assert_eq!(intent.operation_kind(), OperationKind::ExecuteRead);
    assert_eq!(intent.text(), r"SELECT 'a\'");
}

fn author_ids(output: &eframe::egui::FullOutput) -> BTreeSet<String> {
    output
        .platform_output
        .accesskit_update
        .as_ref()
        .expect("editor frame must emit AccessKit")
        .nodes
        .iter()
        .filter_map(|(_, node)| node.author_id().map(str::to_owned))
        .collect()
}

#[test]
fn raw_input_shortcut_submits_once_and_pending_work_exposes_exact_cancel() {
    let profile = profile(
        "mysql-a",
        7,
        DriverKind::MySql,
        Some("app"),
        TlsMode::Required,
    );
    let mut model = UiModel::default();
    let workspace = model.workspace_mut(WorkspaceKey::new(profile.id.clone(), profile.generation));
    workspace.editor_text = "SELECT 1".to_owned();

    let context = Context::default();
    context.enable_accesskit();
    let mut surface = EditorSurface::default();
    #[cfg(target_os = "macos")]
    let shortcut_modifiers = Modifiers {
        mac_cmd: true,
        command: true,
        ..Modifiers::default()
    };
    #[cfg(not(target_os = "macos"))]
    let shortcut_modifiers = Modifiers {
        ctrl: true,
        command: true,
        ..Modifiers::default()
    };
    let shortcut = Event::Key {
        key: Key::Enter,
        physical_key: Some(Key::Enter),
        pressed: true,
        repeat: false,
        modifiers: shortcut_modifiers,
    };
    let input = RawInput {
        events: vec![shortcut],
        ..RawInput::default()
    };
    let mut emitted = Vec::new();
    let output = context.run_ui(input, |ui| {
        if let Some(intent) = surface.show(ui, &profile, workspace, true) {
            emitted.push(intent);
        }
    });

    assert_eq!(emitted.len(), 1);
    assert!(matches!(emitted[0], EditorIntent::Execute(_)));
    let ids = author_ids(&output);
    for expected in [
        "editor.target",
        "editor.input",
        "editor.row_limit",
        "editor.timeout",
        "editor.execute",
    ] {
        assert!(ids.contains(expected), "missing author id {expected}");
    }

    workspace.pending_execute = Some(OperationId(73));
    assert_eq!(
        pending_cancel_intent(workspace),
        Some(EditorIntent::Cancel {
            operation_id: OperationId(73)
        })
    );

    let pending_context = Context::default();
    pending_context.enable_accesskit();
    let pending_output = pending_context.run_ui(RawInput::default(), |ui| {
        let _ = surface.show(ui, &profile, workspace, true);
    });
    let pending_ids = author_ids(&pending_output);
    assert!(pending_ids.contains("editor.execute"));
    assert!(pending_ids.contains("editor.cancel"));
}

#[test]
fn editor_action_bar_runs_current_and_opens_history_without_losing_the_draft() {
    let profile = profile(
        "mysql-history",
        8,
        DriverKind::MySql,
        Some("app"),
        TlsMode::Required,
    );
    let mut model = UiModel::default();
    let workspace = model.workspace_mut(WorkspaceKey::new(profile.id.clone(), profile.generation));
    workspace.editor_text = "SELECT keep_this_draft".to_owned();

    let context = Context::default();
    context.enable_accesskit();
    let mut surface = EditorSurface::default();
    let initial = context.run_ui(RawInput::default(), |ui| {
        assert!(surface.show(ui, &profile, workspace, true).is_none());
    });
    let initial_update = initial
        .platform_output
        .accesskit_update
        .expect("editor action bar must emit AccessKit");
    let author_node = |author_id: &str| {
        initial_update
            .nodes
            .iter()
            .find_map(|(node_id, node)| {
                (node.author_id() == Some(author_id)).then_some((*node_id, node))
            })
            .unwrap_or_else(|| panic!("missing editor action {author_id}"))
    };
    let (_, run_current) = author_node("editor.execute");
    assert_eq!(run_current.role(), accesskit::Role::Button);
    assert_eq!(run_current.label(), Some("Run current or selection"));
    let (history_id, history) = author_node("editor.history");
    assert_eq!(history.role(), accesskit::Role::Button);
    assert_eq!(history.label(), Some("Open execution history"));
    assert!(history.supports_action(accesskit::Action::Focus));
    assert!(history.supports_action(accesskit::Action::Click));

    let _ = context.run_ui(
        RawInput {
            events: vec![Event::AccessKitActionRequest(accesskit::ActionRequest {
                action: accesskit::Action::Focus,
                target_tree: accesskit::TreeId::ROOT,
                target_node: history_id,
                data: None,
            })],
            ..RawInput::default()
        },
        |ui| {
            assert!(surface.show(ui, &profile, workspace, true).is_none());
        },
    );
    let _ = context.run_ui(
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
        |ui| {
            assert!(surface.show(ui, &profile, workspace, true).is_none());
        },
    );

    assert_eq!(workspace.result_area_tab(), ResultAreaTab::History);
    assert_eq!(workspace.editor_text, "SELECT keep_this_draft");
    assert!(workspace.pending_execute.is_none());
}

#[test]
fn multi_frame_selection_survives_exact_profile_switches() {
    let profile_a = profile(
        "mysql-a",
        7,
        DriverKind::MySql,
        Some("app"),
        TlsMode::Required,
    );
    let profile_b = profile(
        "mysql-b",
        11,
        DriverKind::MySql,
        Some("audit"),
        TlsMode::Required,
    );
    let key_a = WorkspaceKey::new(profile_a.id.clone(), profile_a.generation);
    let key_b = WorkspaceKey::new(profile_b.id.clone(), profile_b.generation);
    let mut model = UiModel::default();
    {
        let workspace = model.workspace_mut(key_a.clone());
        workspace.editor_text = "SELECT 1;".to_owned();
        workspace.caret_character_index = workspace.editor_text.chars().count();
    }
    {
        let workspace = model.workspace_mut(key_b.clone());
        workspace.editor_text = "SELECT 2;".to_owned();
        workspace.caret_character_index = 0;
    }

    let context = Context::default();
    let mut surface = EditorSurface::default();
    surface.request_focus("editor.input");
    let _ = context.run_ui(RawInput::default(), |ui| {
        let _ = surface.show(ui, &profile_a, model.workspace_mut(key_a.clone()), true);
    });

    let select_previous_character = Event::Key {
        key: Key::ArrowLeft,
        physical_key: Some(Key::ArrowLeft),
        pressed: true,
        repeat: false,
        modifiers: Modifiers {
            shift: true,
            ..Modifiers::default()
        },
    };
    let _ = context.run_ui(
        RawInput {
            events: vec![select_previous_character],
            ..RawInput::default()
        },
        |ui| {
            let _ = surface.show(ui, &profile_a, model.workspace_mut(key_a.clone()), true);
        },
    );
    assert_eq!(
        model
            .workspace(&key_a)
            .expect("workspace A")
            .caret_character_index,
        8
    );
    assert_eq!(
        model
            .workspace(&key_a)
            .expect("workspace A")
            .selection_character_range,
        Some(8..9)
    );

    let _ = context.run_ui(RawInput::default(), |ui| {
        let _ = surface.show(ui, &profile_b, model.workspace_mut(key_b.clone()), true);
    });
    assert_eq!(
        model
            .workspace(&key_b)
            .expect("workspace B")
            .caret_character_index,
        0,
        "workspace B must not inherit workspace A's selection"
    );

    let _ = context.run_ui(RawInput::default(), |ui| {
        let _ = surface.show(ui, &profile_a, model.workspace_mut(key_a.clone()), true);
    });
    let workspace_a = model.workspace(&key_a).expect("workspace A after return");
    assert_eq!(workspace_a.caret_character_index, 8);
    assert_eq!(workspace_a.selection_character_range, Some(8..9));
}
