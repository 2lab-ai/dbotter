use std::fs;
use std::process::Command;
use std::time::Duration;

use dbotter::drivers::mysql_catalog::CatalogRetention;
use dbotter::model::{
    CatalogNodeKind, CatalogPage, CatalogPageToken, CatalogRequest, ExecuteRequest,
    MAX_CATALOG_RELATIONS, MAX_CATALOG_UTF8_BYTES, OperationId, ProfileId, PublicCode,
    PublicSummary, QueryLanguage, RequestIdentity,
};
use dbotter::service::{ApplicationService, ServiceError};
#[cfg(feature = "desktop")]
use dbotter::ui::MySqlExplorerState;

const PASSWORD_ENV: &str = "DBOTTER_MYSQL_PASSWORD";

fn live_port() -> u16 {
    std::env::var("DBOTTER_P4_MYSQL_PORT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(33_306)
}

fn write_live_config() -> tempfile::TempDir {
    assert!(
        std::env::var_os(PASSWORD_ENV).is_some(),
        "{PASSWORD_ENV} must be present for the ignored live contract"
    );
    let directory = tempfile::tempdir().expect("live config tempdir");
    let config = format!(
        r#"version = 2

[[profiles]]
id = "catalog-scoped"
name = "Catalog scoped"
driver = "mysql"
host = "127.0.0.1"
port = {port}
database = "dbotter_allowed"
username = "dbotter_catalog"
tls = "preferred"
credential_mode = "environment"
secret_env = "{password_env}"

[[profiles]]
id = "catalog-visible"
name = "Catalog visible"
driver = "mysql"
host = "127.0.0.1"
port = {port}
username = "dbotter_catalog"
tls = "preferred"
credential_mode = "environment"
secret_env = "{password_env}"

[[profiles]]
id = "catalog-denied"
name = "Catalog denied"
driver = "mysql"
host = "127.0.0.1"
port = {port}
database = "dbotter_forbidden"
username = "dbotter_denied"
tls = "preferred"
credential_mode = "environment"
secret_env = "{password_env}"
"#,
        port = live_port(),
        password_env = PASSWORD_ENV,
    );
    fs::write(directory.path().join("config.toml"), config).expect("write live config");
    directory
}

async fn identity(service: &ApplicationService, profile: &str, operation: u64) -> RequestIdentity {
    let profile_id = ProfileId(profile.to_owned());
    let generation = service
        .profile_generation(&profile_id)
        .await
        .expect("live profile generation");
    RequestIdentity::new(profile_id, generation, OperationId(operation))
}

async fn relations(
    service: &ApplicationService,
    profile: &str,
    schema: &str,
    prefix: Option<&str>,
    token: Option<CatalogPageToken>,
    page_size: u16,
    operation: u64,
) -> Result<CatalogPage, ServiceError> {
    service
        .load_catalog_page(CatalogRequest::Relations {
            identity: identity(service, profile, operation).await,
            schema: schema.to_owned(),
            prefix: prefix.map(str::to_owned),
            page_token: token,
            page_size,
            timeout: Duration::from_secs(30),
        })
        .await
}

async fn columns(
    service: &ApplicationService,
    relation: &str,
    prefix: Option<&str>,
    token: Option<CatalogPageToken>,
    page_size: u16,
    operation: u64,
) -> Result<CatalogPage, ServiceError> {
    service
        .load_catalog_page(CatalogRequest::Columns {
            identity: identity(service, "catalog-scoped", operation).await,
            schema: "dbotter_allowed".to_owned(),
            relation: relation.to_owned(),
            prefix: prefix.map(str::to_owned),
            page_token: token,
            page_size,
            timeout: Duration::from_secs(30),
        })
        .await
}

fn assert_binary_order(nodes: &[dbotter::model::CatalogNode]) {
    assert!(
        nodes
            .windows(2)
            .all(|pair| pair[0].name.as_bytes() < pair[1].name.as_bytes()),
        "catalog names must be strictly binary ordered"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires the dbotter-p4 MySQL Compose fixture"]
async fn p4_live_catalog_fixture_proves_pages_caps_permissions_and_cli() {
    let directory = write_live_config();
    let config_path = directory.path().join("config.toml");
    let service = ApplicationService::load_path(&config_path).expect("load live service");

    let scoped = service
        .load_catalog_page(CatalogRequest::Schemas {
            identity: identity(&service, "catalog-scoped", 1).await,
            prefix: None,
            page_token: None,
            page_size: 50,
            timeout: Duration::from_secs(30),
        })
        .await
        .expect("configured database schema scope");
    assert_eq!(
        scoped
            .nodes
            .iter()
            .map(|node| node.name.as_str())
            .collect::<Vec<_>>(),
        ["dbotter_allowed"]
    );

    let visible = service
        .load_catalog_page(CatalogRequest::Schemas {
            identity: identity(&service, "catalog-visible", 2).await,
            prefix: None,
            page_token: None,
            page_size: 50,
            timeout: Duration::from_secs(30),
        })
        .await
        .expect("restricted visible schemas");
    assert!(
        visible
            .nodes
            .iter()
            .any(|node| node.name == "dbotter_allowed")
    );
    assert!(
        visible
            .nodes
            .iter()
            .all(|node| node.name != "dbotter_forbidden"),
        "information_schema omission is a successful absence, not Permission"
    );

    let first = relations(
        &service,
        "catalog-scoped",
        "dbotter_allowed",
        Some("bulk_"),
        None,
        17,
        3,
    )
    .await
    .expect("first relation page");
    #[cfg(feature = "desktop")]
    let live_stale_seed = first.clone();
    assert_eq!(first.nodes.len(), 17);
    assert_binary_order(&first.nodes);
    let second = relations(
        &service,
        "catalog-scoped",
        "dbotter_allowed",
        Some("bulk_"),
        first.next_token.clone(),
        17,
        4,
    )
    .await
    .expect("second relation page");
    assert_eq!(second.nodes.len(), 17);
    assert!(
        first.nodes.last().expect("first last").name.as_bytes()
            < second.nodes.first().expect("second first").name.as_bytes()
    );

    let catalog_relations = relations(
        &service,
        "catalog-scoped",
        "dbotter_allowed",
        Some("catalog_"),
        None,
        50,
        5,
    )
    .await
    .expect("table and view page");
    assert!(
        catalog_relations
            .nodes
            .iter()
            .any(|node| node.name == "catalog_anchor" && node.kind == CatalogNodeKind::Table)
    );
    assert!(
        catalog_relations
            .nodes
            .iter()
            .any(|node| node.name == "catalog_view" && node.kind == CatalogNodeKind::View)
    );

    let wide_first = columns(&service, "wide_catalog", None, None, 50, 6)
        .await
        .expect("wide first column page");
    let wide_second = columns(
        &service,
        "wide_catalog",
        None,
        wide_first.next_token.clone(),
        50,
        7,
    )
    .await
    .expect("wide second column page");
    assert_eq!(wide_first.nodes.len(), 50);
    assert_eq!(wide_second.nodes.len(), 50);
    assert!(
        wide_first.nodes.last().and_then(|node| node.ordinal)
            < wide_second.nodes.first().and_then(|node| node.ordinal)
    );

    let mut token = None;
    let mut retained_relations = 0_usize;
    let capped = loop {
        let page = relations(
            &service,
            "catalog-scoped",
            "dbotter_allowed",
            Some("bulk_"),
            token,
            200,
            10 + retained_relations as u64,
        )
        .await
        .expect("count-cap relation page");
        retained_relations = retained_relations.saturating_add(page.nodes.len());
        if page.truncated || page.next_token.is_none() {
            break page;
        }
        token = page.next_token.clone();
    };
    assert_eq!(retained_relations, MAX_CATALOG_RELATIONS);
    assert!(capped.truncated);
    assert!(capped.next_token.is_none());
    assert_eq!(capped.retained_counts.relations, MAX_CATALOG_RELATIONS);

    let tail = relations(
        &service,
        "catalog-scoped",
        "dbotter_allowed",
        Some("tail_"),
        None,
        50,
        9_000,
    )
    .await
    .expect("narrowed prefix after count cap");
    assert_eq!(tail.nodes.len(), 1);
    assert_eq!(tail.nodes[0].name, "tail_after_cap");

    let metadata_relations = relations(
        &service,
        "catalog-scoped",
        "dbotter_allowed",
        Some("meta_"),
        None,
        200,
        9_001,
    )
    .await
    .expect("metadata relations");
    assert_eq!(metadata_relations.nodes.len(), 90);
    let mut retention = CatalogRetention::default();
    let mut byte_cap_reached = false;
    for (index, relation) in metadata_relations.nodes.iter().enumerate() {
        let page = columns(
            &service,
            &relation.name,
            None,
            None,
            200,
            10_000 + index as u64,
        )
        .await
        .expect("metadata column page");
        let outcome = retention.retain(page.nodes);
        if outcome.truncated {
            byte_cap_reached = true;
            break;
        }
    }
    assert!(
        byte_cap_reached,
        "live type metadata must cross the 4 MiB cap"
    );
    assert!(retention.retained_utf8_bytes() <= MAX_CATALOG_UTF8_BYTES);
    retention.clear();
    let recovered_relation = relations(
        &service,
        "catalog-scoped",
        "dbotter_allowed",
        Some("meta_089"),
        None,
        50,
        20_000,
    )
    .await
    .expect("narrow metadata relation after clear");
    assert_eq!(recovered_relation.nodes.len(), 1);
    let recovered_column = columns(&service, "meta_089", Some("payload"), None, 50, 20_001)
        .await
        .expect("narrow metadata column after clear");
    assert_eq!(retention.retain(recovered_column.nodes).nodes.len(), 1);

    let empty = relations(
        &service,
        "catalog-scoped",
        "dbotter_allowed",
        Some("definitely_absent_"),
        None,
        50,
        20_002,
    )
    .await
    .expect("successful empty page");
    assert!(empty.nodes.is_empty());
    assert!(!empty.truncated);

    let invalid_request = CatalogRequest::Relations {
        identity: identity(&service, "catalog-scoped", 20_003).await,
        schema: "dbotter_allowed".to_owned(),
        prefix: Some("bulk_".to_owned()),
        page_token: Some(CatalogPageToken("v1.tampered.invalid".to_owned())),
        page_size: 50,
        timeout: Duration::from_secs(30),
    };
    let invalid_token = service
        .load_catalog_page(invalid_request.clone())
        .await
        .expect_err("tampered token fails closed");
    assert_eq!(invalid_token.public_summary(), PublicSummary::InvalidInput);
    assert_eq!(invalid_token.public_code(), PublicCode::Catalog);
    #[cfg(feature = "desktop")]
    {
        let mut explorer = MySqlExplorerState::default();
        explorer.handle_loaded(live_stale_seed);
        explorer.mark_submitted(invalid_request.clone());
        explorer.handle_failed(invalid_request.clone(), invalid_token.public_summary());
        assert!(explorer.is_stale_for(&invalid_request));
        assert_eq!(explorer.retry_request(), Some(&invalid_request));
    }

    let denied_profile = ProfileId("catalog-denied".to_owned());
    let denied_generation = service
        .profile_generation(&denied_profile)
        .await
        .expect("denied generation");
    let denied_check = service
        .check_at(
            OperationId(30_000),
            denied_profile.clone(),
            denied_generation,
            Duration::from_secs(5),
        )
        .await
        .expect_err("unauthorized default database check");
    assert_eq!(
        denied_check.public_summary(),
        PublicSummary::PermissionDenied
    );
    let denied_execute = service
        .execute_at(ExecuteRequest {
            operation_id: OperationId(30_001),
            profile_id: denied_profile,
            profile_generation: denied_generation,
            language: QueryLanguage::Sql,
            text: "SELECT 1".to_owned(),
            row_limit: 1,
            timeout: Duration::from_secs(5),
        })
        .await
        .expect_err("unauthorized default database execute");
    assert_eq!(
        denied_execute.public_summary(),
        PublicSummary::PermissionDenied
    );

    let cli = Command::new(env!("CARGO_BIN_EXE_dbotter"))
        .args([
            "--config",
            config_path.to_string_lossy().as_ref(),
            "browse",
            "mysql",
            "relations",
            "--profile",
            "catalog-scoped",
            "--schema",
            "dbotter_allowed",
            "--prefix",
            "catalog_",
            "--page-size",
            "50",
            "--format",
            "json",
        ])
        .output()
        .expect("run headless catalog CLI");
    assert!(
        cli.status.success(),
        "CLI stderr must be static: {:?}",
        cli.stderr
    );
    assert!(cli.stderr.is_empty());
    let cli_page: serde_json::Value =
        serde_json::from_slice(&cli.stdout).expect("catalog CLI JSON");
    assert_eq!(cli_page["level"], "relations");
    assert_eq!(cli_page["identity"]["profile_id"], "catalog-scoped");
    assert!(cli_page.get("query_text").is_none());
    assert_eq!(cli_page["nodes"].as_array().expect("CLI nodes").len(), 2);
}
