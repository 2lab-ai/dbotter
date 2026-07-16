#![cfg(feature = "desktop")]

use std::sync::Arc;

use dbotter::model::{
    ConnectionProfile, CredentialMode, DriverKind, OperationId, ProfileAccess, ProfileEnvironment,
    ProfileGeneration, ProfileId, ProfileInstanceId, ProfileSafetyPosture, QueryLanguage,
    QueryResult, RedisTlsConfig, ResultId, ResultProvenance, ResultRetentionPolicy, ResultSnapshot,
    TlsMode,
};
use dbotter::ui::{
    EditorTabError, MAX_EDITOR_TAB_TEXT_BYTES, ProfileSnapshot, ProfileWorkspace, ResultAreaTab,
    UiEvent, UiModel, WorkspaceGeometry, WorkspaceKey,
};

fn profile(id: &str, generation: u64) -> ProfileSnapshot {
    let persisted = ConnectionProfile {
        id: id.to_owned(),
        name: format!("{id} display"),
        driver: DriverKind::MySql,
        host: "db.internal".to_owned(),
        port: 3306,
        database: Some("app".to_owned()),
        username: None,
        safety: ProfileSafetyPosture::classified(
            ProfileEnvironment::Development,
            ProfileAccess::ReadWrite,
            ProfileInstanceId::from_bytes([id.as_bytes()[0]; 16]),
        ),
        tls: TlsMode::Required,
        credential_mode: CredentialMode::None,
        secret_env: None,
        redis_tls: RedisTlsConfig::default(),
    };
    ProfileSnapshot::from_profile(&persisted, ProfileGeneration(generation), false, None)
}

fn result(profile: &ProfileSnapshot, result_id: u64) -> ResultSnapshot {
    ResultSnapshot::retain(
        QueryResult {
            columns: Vec::new(),
            rows: Vec::new(),
            affected_rows: 0,
            last_insert_id: None,
            elapsed_ms: u128::from(result_id),
            truncated: false,
            backend_notices_present: false,
        },
        ResultProvenance {
            result_id: ResultId(result_id),
            profile_id: profile.id.clone(),
            profile_generation: profile.generation,
            operation_id: OperationId(result_id),
            driver: profile.driver,
            completed_at_unix_ms: i64::try_from(result_id).expect("fixture timestamp fits"),
            duration_ms: u128::from(result_id),
        },
        ResultRetentionPolicy::mysql(1),
    )
}

#[test]
fn editor_tab_strip_supports_create_rename_duplicate_select_and_close() {
    let mut workspace = ProfileWorkspace::default();
    let original = workspace
        .create_editor_tab(QueryLanguage::Sql, "Orders", "SELECT 1")
        .expect("first tab should be created");
    assert_eq!(workspace.selected_editor_tab_id(), Some(original));

    workspace
        .rename_editor_tab(original, "Orders archive")
        .expect("tab should be renamed");
    assert_eq!(
        workspace
            .editor_tab(original)
            .expect("renamed tab should remain addressable")
            .title(),
        "Orders archive"
    );
    let duplicate = workspace
        .duplicate_editor_tab(original)
        .expect("tab should be duplicated");
    assert_ne!(duplicate, original, "duplicate needs a stable new local id");

    let duplicated = workspace
        .editor_tab(duplicate)
        .expect("duplicated tab should remain addressable by its stable id");
    assert_eq!(duplicated.language(), QueryLanguage::Sql);
    assert_eq!(duplicated.text(), "SELECT 1");

    workspace
        .select_editor_tab(duplicate)
        .expect("duplicate should be selectable");
    assert_eq!(
        workspace.close_editor_tab(original),
        Err(EditorTabError::Dirty),
        "dirty drafts require an explicit discard confirmation"
    );
    assert!(workspace.editor_tab(original).is_some());
    workspace
        .discard_editor_tab(original)
        .expect("confirmed discard should remove the local draft");
    assert!(workspace.editor_tab(original).is_none());
    assert_eq!(workspace.selected_editor_tab_id(), Some(duplicate));
}

#[test]
fn editor_text_is_bounded_and_oversize_surface_sync_fails_without_replacing_the_tab() {
    assert_eq!(MAX_EDITOR_TAB_TEXT_BYTES, 256 * 1024);
    let mut workspace = ProfileWorkspace::default();
    let tab = workspace
        .create_editor_tab(QueryLanguage::Sql, "Bounded", "SELECT 1")
        .expect("bounded tab");
    let retained = workspace
        .editor_tab(tab)
        .expect("tab retained")
        .text()
        .to_owned();
    workspace.editor_text = "x".repeat(MAX_EDITOR_TAB_TEXT_BYTES + 1);

    assert_eq!(
        workspace.sync_selected_editor_tab_from_surface(),
        Err(EditorTabError::TextTooLarge)
    );
    assert_eq!(
        workspace
            .editor_tab(tab)
            .expect("tab still retained")
            .text(),
        retained
    );
}

#[test]
fn profile_edit_retags_only_editor_context_for_the_same_immutable_instance() {
    let original = profile("alpha", 1);
    let refreshed = profile("alpha", 2);
    let original_key = WorkspaceKey::new(original.id.clone(), original.generation);
    let refreshed_key = WorkspaceKey::new(refreshed.id.clone(), refreshed.generation);
    let mut model = UiModel::default();
    model.fold(UiEvent::ProfilesLoaded {
        operation_id: OperationId(100),
        profiles: vec![original.clone()],
        config: Default::default(),
    });
    let tab = model
        .workspace_mut(original_key.clone())
        .create_editor_tab(QueryLanguage::Sql, "Draft", "SELECT retained_draft")
        .expect("draft tab");
    model
        .workspace_mut(original_key.clone())
        .append_result_tab(Arc::new(result(&original, 101)))
        .expect("stale result fixture");

    model.fold(UiEvent::ProfilesLoaded {
        operation_id: OperationId(101),
        profiles: vec![refreshed],
        config: Default::default(),
    });

    assert!(model.workspace(&original_key).is_none());
    let retained = model
        .workspace(&refreshed_key)
        .expect("same immutable instance retains its editor workspace");
    assert_eq!(
        retained.editor_tab(tab).expect("draft retained").text(),
        "SELECT retained_draft"
    );
    assert!(
        retained.result_tabs().is_empty(),
        "generation-bound results must not cross a profile edit"
    );

    let mut replacement = profile("alpha", 3);
    replacement.persisted.safety = ProfileSafetyPosture::classified(
        ProfileEnvironment::Development,
        ProfileAccess::ReadWrite,
        ProfileInstanceId::from_bytes([0xff; 16]),
    );
    model.fold(UiEvent::ProfilesLoaded {
        operation_id: OperationId(102),
        profiles: vec![replacement.clone()],
        config: Default::default(),
    });
    assert!(
        model
            .workspace(&WorkspaceKey::new(replacement.id, replacement.generation))
            .is_none(),
        "a replacement instance must never inherit another instance's drafts"
    );
}

#[test]
fn switching_selected_profile_preserves_each_exact_editor_workspace() {
    let alpha = profile("alpha", 1);
    let beta = profile("beta", 1);
    let mut model = UiModel::default();
    model.profiles = vec![alpha.clone(), beta.clone()];

    model.selected_profile = Some(alpha.id.clone());
    let alpha_tab = model
        .selected_workspace_mut()
        .expect("alpha workspace")
        .create_editor_tab(QueryLanguage::Sql, "Alpha", "SELECT 'alpha'")
        .expect("alpha tab");

    model.selected_profile = Some(beta.id.clone());
    let beta_tab = model
        .selected_workspace_mut()
        .expect("beta workspace")
        .create_editor_tab(QueryLanguage::Sql, "Beta", "SELECT 'beta'")
        .expect("beta tab");

    model.selected_profile = Some(alpha.id.clone());
    let alpha_workspace = model.selected_workspace().expect("alpha after return");
    assert_eq!(alpha_workspace.selected_editor_tab_id(), Some(alpha_tab));
    assert_eq!(
        alpha_workspace
            .editor_tab(alpha_tab)
            .expect("alpha tab retained")
            .text(),
        "SELECT 'alpha'"
    );

    model.selected_profile = Some(beta.id.clone());
    let beta_workspace = model.selected_workspace().expect("beta after return");
    assert_eq!(beta_workspace.selected_editor_tab_id(), Some(beta_tab));
    assert_eq!(
        beta_workspace
            .editor_tab(beta_tab)
            .expect("beta tab retained")
            .text(),
        "SELECT 'beta'"
    );
}

#[test]
fn result_area_selection_is_real_profile_workspace_state() {
    let mut workspace = ProfileWorkspace::default();
    assert_eq!(workspace.result_area_tab(), ResultAreaTab::Results);
    workspace.select_result_area_tab(ResultAreaTab::History);
    assert_eq!(workspace.result_area_tab(), ResultAreaTab::History);
}

#[test]
fn successful_executions_append_distinct_selectable_result_tabs() {
    let profile = profile("alpha", 1);
    let mut workspace = ProfileWorkspace::default();
    let first = workspace
        .append_result_tab(Arc::new(result(&profile, 11)))
        .expect("first result tab");
    let second = workspace
        .append_result_tab(Arc::new(result(&profile, 12)))
        .expect("second result tab");

    assert_ne!(first, second);
    assert_eq!(workspace.result_tabs().len(), 2);
    assert_eq!(workspace.selected_result_tab_id(), Some(second));
    workspace
        .select_result_tab(first)
        .expect("older result remains selectable");
    assert_eq!(workspace.selected_result_tab_id(), Some(first));
    assert_eq!(
        workspace
            .selected_result_tab()
            .expect("selected result")
            .snapshot()
            .provenance
            .result_id,
        ResultId(11)
    );
}

#[test]
fn geometry_round_trips_with_its_workspace_key() {
    let key = WorkspaceKey::new(ProfileId("alpha".to_owned()), ProfileGeneration(7));
    let geometry = WorkspaceGeometry::restore(360.0, 0.70, false);
    let encoded = serde_json::to_string(&vec![(key.clone(), geometry)])
        .expect("workspace geometry should serialize with its key");
    let decoded: Vec<(WorkspaceKey, WorkspaceGeometry)> =
        serde_json::from_str(&encoded).expect("workspace geometry should restore with its key");

    assert_eq!(decoded, vec![(key, geometry)]);
    assert_eq!(decoded[0].1.navigator_width(), 360.0);
    assert_eq!(decoded[0].1.editor_share(), 0.70);
    assert!(!decoded[0].1.inspector_visible());
}
