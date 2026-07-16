#![cfg(feature = "desktop")]

use dbotter::model::{
    ConnectionProfile, CredentialMode, DriverKind, ProfileAccess, ProfileEnvironment,
    ProfileGeneration, ProfileId, ProfileSafetyPosture, QueryLanguage, RedisTlsConfig, TlsMode,
};
use dbotter::ui::{
    ProfileSnapshot, ProfileWorkspace, ResultAreaTab, UiModel, WorkspaceGeometry, WorkspaceKey,
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
        safety: ProfileSafetyPosture::new(
            ProfileEnvironment::Development,
            ProfileAccess::ReadWrite,
        ),
        tls: TlsMode::Required,
        credential_mode: CredentialMode::None,
        secret_env: None,
        redis_tls: RedisTlsConfig::default(),
    };
    ProfileSnapshot::from_profile(&persisted, ProfileGeneration(generation), false, None)
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
    workspace
        .close_editor_tab(original)
        .expect("close should remove the local draft");
    assert!(workspace.editor_tab(original).is_none());
    assert_eq!(workspace.selected_editor_tab_id(), Some(duplicate));
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
