use std::fs;
use std::process::Command;
use std::time::Duration;

#[path = "common/live_evidence.rs"]
mod live_evidence;

use dbotter::drivers::mysql_catalog::CatalogRetention;
use dbotter::model::{
    CatalogNodeKind, CatalogPage, CatalogPageToken, CatalogRequest, ExecuteRequest,
    MAX_CATALOG_RELATIONS, MAX_CATALOG_UTF8_BYTES, OperationId, ProfileId, PublicCode,
    PublicSummary, QueryLanguage, RequestIdentity,
};
use dbotter::service::{ApplicationService, ServiceError};
#[cfg(feature = "desktop")]
use dbotter::ui::MySqlExplorerState;
use live_evidence::LiveEvidence;

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
    let mut evidence = LiveEvidence::required(
        "mysql_catalog",
        "p4_live_catalog_fixture_proves_pages_caps_permissions_and_cli",
        "DBOTTER_LIVE_MYSQL_CATALOG_EVIDENCE",
    )
    .expect("initialize MySQL catalog evidence");
    let directory = write_live_config();
    let config_path = directory.path().join("config.toml");
    let service = ApplicationService::load_path(&config_path).expect("load live service");

    let schema_scope = evidence.begin("mysql.catalog.schema.scope");
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
    evidence.pass(schema_scope);

    let schema_visibility = evidence.begin("mysql.catalog.schema.visibility");
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
    evidence.pass(schema_visibility);

    let relation_page1 = evidence.begin("mysql.catalog.relation.page1");
    let relation_order = evidence.begin("mysql.catalog.relation.binary_order");
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
    evidence.pass(relation_page1);
    let relation_page2 = evidence.begin("mysql.catalog.relation.page2");
    let other_service = ApplicationService::load_path(&config_path).expect("second live service");
    let cross_service = relations(
        &other_service,
        "catalog-scoped",
        "dbotter_allowed",
        Some("bulk_"),
        first.next_token.clone(),
        17,
        3_001,
    )
    .await
    .expect("same-config ApplicationService must continue the token");
    let second = cross_service;
    assert_eq!(second.nodes.len(), 17);
    assert!(
        first.nodes.last().expect("first last").name.as_bytes()
            < second.nodes.first().expect("second first").name.as_bytes()
    );
    evidence.pass(relation_page2);
    evidence.pass(relation_order);

    let cross_config_token = evidence.begin("mysql.catalog.token.cross_config_rejected");
    let different_config_path = directory.path().join("different-config.toml");
    fs::copy(&config_path, &different_config_path).expect("copy different config fixture");
    let different_config_service =
        ApplicationService::load_path(&different_config_path).expect("different config service");
    let cross_config = relations(
        &different_config_service,
        "catalog-scoped",
        "dbotter_allowed",
        Some("bulk_"),
        first.next_token.clone(),
        17,
        3_002,
    )
    .await
    .expect_err("different config integrity key must reject the token");
    assert_eq!(cross_config.public_summary(), PublicSummary::InvalidInput);
    assert_eq!(cross_config.public_code(), PublicCode::Catalog);
    evidence.pass(cross_config_token);

    let stale_connection_token_checkpoint =
        evidence.begin("mysql.catalog.token.stale_connection_rejected");
    let original_config = fs::read_to_string(&config_path).expect("read original live config");
    let rewritten_config = original_config.replacen(
        "database = \"dbotter_allowed\"",
        "database = \"information_schema\"",
        1,
    );
    assert_ne!(rewritten_config, original_config);
    fs::write(&config_path, &rewritten_config).expect("rewrite same-path connection data");
    let rewritten_service =
        ApplicationService::load_path(&config_path).expect("same-path rewritten service");
    let stale_connection_token = relations(
        &rewritten_service,
        "catalog-scoped",
        "dbotter_allowed",
        Some("bulk_"),
        first.next_token.clone(),
        17,
        3_003,
    )
    .await
    .expect_err("same-path connection fingerprint change must reject old token");
    assert_eq!(
        stale_connection_token.public_summary(),
        PublicSummary::InvalidInput
    );
    assert_eq!(stale_connection_token.public_code(), PublicCode::Catalog);
    fs::write(&config_path, original_config).expect("restore original live config");
    evidence.pass(stale_connection_token_checkpoint);

    let relation_table = evidence.begin("mysql.catalog.relation.table");
    let relation_view = evidence.begin("mysql.catalog.relation.view");
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
    evidence.pass(relation_table);
    assert!(
        catalog_relations
            .nodes
            .iter()
            .any(|node| node.name == "catalog_view" && node.kind == CatalogNodeKind::View)
    );
    evidence.pass(relation_view);

    let column_page1 = evidence.begin("mysql.catalog.column.page1");
    let column_order = evidence.begin("mysql.catalog.column.ordinal_order");
    let wide_first = columns(&service, "wide_catalog", None, None, 50, 6)
        .await
        .expect("wide first column page");
    evidence.pass(column_page1);
    let column_page2 = evidence.begin("mysql.catalog.column.page2");
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
    evidence.pass(column_page2);
    evidence.pass(column_order);

    let count_cap = evidence.begin("mysql.catalog.count_cap");
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
    evidence.pass(count_cap);

    let narrow_after_count_cap = evidence.begin("mysql.catalog.filter.narrow_after_count_cap");
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
    evidence.pass(narrow_after_count_cap);

    let metadata_cap = evidence.begin("mysql.catalog.metadata_cap_4mib");
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
    let metadata_retained_bytes = retention.retained_utf8_bytes();
    evidence.pass(metadata_cap);
    let clear_after_metadata = evidence.begin("mysql.catalog.filter.clear_after_metadata_cap");
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
    evidence.pass(clear_after_metadata);

    let empty_page = evidence.begin("mysql.catalog.empty");
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
    evidence.pass(empty_page);

    let invalid_token_checkpoint = evidence.begin("mysql.catalog.token.tampered_rejected");
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
    evidence.pass(invalid_token_checkpoint);

    let stale_generation_checkpoint =
        evidence.begin("mysql.catalog.token.stale_generation_rejected");
    let scoped_profile = ProfileId("catalog-scoped".to_owned());
    let stale_generation = service
        .profile_generation(&scoped_profile)
        .await
        .expect("generation before live reload");
    let before_reload = fs::read_to_string(&config_path).expect("config before live reload");
    let changed_config = before_reload.replacen(
        "name = \"Catalog scoped\"",
        "name = \"Catalog scoped reloaded\"",
        1,
    );
    assert_ne!(changed_config, before_reload);
    fs::write(&config_path, &changed_config).expect("change live profile generation");
    service
        .reload_configuration()
        .await
        .expect("reload changed live profile");
    let stale_generation_error = service
        .load_catalog_page(CatalogRequest::Relations {
            identity: RequestIdentity::new(scoped_profile, stale_generation, OperationId(20_004)),
            schema: "dbotter_allowed".to_owned(),
            prefix: Some("catalog_".to_owned()),
            page_token: None,
            page_size: 50,
            timeout: Duration::from_secs(30),
        })
        .await
        .expect_err("stale profile generation must fail before catalog dispatch");
    assert_eq!(
        stale_generation_error.public_summary(),
        PublicSummary::ResourceStale
    );
    assert_eq!(
        stale_generation_error.public_code(),
        PublicCode::ProfileStale
    );
    fs::write(&config_path, before_reload).expect("restore live profile config");
    service
        .reload_configuration()
        .await
        .expect("reload restored live profile");
    evidence.pass(stale_generation_checkpoint);

    let permission_check = evidence.begin("mysql.catalog.permission.check_denied");
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
    evidence.pass(permission_check);
    let permission_execute = evidence.begin("mysql.catalog.permission.execute_denied");
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
    evidence.pass(permission_execute);

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

    let cli_pagination_config = directory.path().join("cli-pagination.toml");
    fs::copy(&config_path, &cli_pagination_config).expect("copy CLI pagination config");
    let cli_page1 = evidence.begin("mysql.catalog.cli.page1");
    let cli_first = Command::new(env!("CARGO_BIN_EXE_dbotter"))
        .args([
            "--config",
            cli_pagination_config.to_string_lossy().as_ref(),
            "browse",
            "mysql",
            "relations",
            "--profile",
            "catalog-scoped",
            "--schema",
            "dbotter_allowed",
            "--prefix",
            "bulk_",
            "--page-size",
            "17",
            "--format",
            "json",
        ])
        .output()
        .expect("run first paginated catalog CLI process");
    assert!(
        cli_first.status.success(),
        "first CLI stderr must be static: {:?}",
        cli_first.stderr
    );
    assert!(cli_first.stderr.is_empty());
    let cli_first_page: serde_json::Value =
        serde_json::from_slice(&cli_first.stdout).expect("first catalog CLI JSON");
    let cli_token = cli_first_page["next_token"]
        .as_str()
        .expect("first CLI page token");
    let cli_first_last = cli_first_page["nodes"]
        .as_array()
        .and_then(|nodes| nodes.last())
        .and_then(|node| node["name"].as_str())
        .expect("first CLI last relation");
    evidence.pass(cli_page1);

    let cli_page2 = evidence.begin("mysql.catalog.cli.page2");
    let cli_second = Command::new(env!("CARGO_BIN_EXE_dbotter"))
        .args([
            "--config",
            cli_pagination_config.to_string_lossy().as_ref(),
            "browse",
            "mysql",
            "relations",
            "--profile",
            "catalog-scoped",
            "--schema",
            "dbotter_allowed",
            "--prefix",
            "bulk_",
            "--page-size",
            "17",
            "--page-token",
            cli_token,
            "--format",
            "json",
        ])
        .output()
        .expect("run second paginated catalog CLI process");
    assert!(
        cli_second.status.success(),
        "second CLI stderr must be static: {:?}",
        cli_second.stderr
    );
    assert!(cli_second.stderr.is_empty());
    let cli_second_page: serde_json::Value =
        serde_json::from_slice(&cli_second.stdout).expect("second catalog CLI JSON");
    let cli_second_first = cli_second_page["nodes"]
        .as_array()
        .and_then(|nodes| nodes.first())
        .and_then(|node| node["name"].as_str())
        .expect("second CLI first relation");
    assert!(cli_first_last.as_bytes() < cli_second_first.as_bytes());
    evidence.pass(cli_page2);

    evidence.measure("cli_pages", 2).expect("CLI page count");
    evidence
        .measure("column_pages", 2)
        .expect("column page count");
    evidence
        .measure(
            "column_rows",
            wide_first.nodes.len() + wide_second.nodes.len(),
        )
        .expect("column row count");
    evidence
        .measure("denied_operations", 2)
        .expect("permission operation count");
    evidence
        .measure("metadata_retained_bytes", metadata_retained_bytes)
        .expect("retained metadata bytes");
    evidence
        .measure("metadata_truncations", usize::from(byte_cap_reached))
        .expect("metadata truncation count");
    evidence
        .measure("relation_pages", 2)
        .expect("relation page count");
    evidence
        .measure("relation_rows", first.nodes.len() + second.nodes.len())
        .expect("relation row count");
    evidence
        .measure("retained_relations", retained_relations)
        .expect("retained relation count");
    evidence.finish().expect("publish MySQL catalog evidence");
}
