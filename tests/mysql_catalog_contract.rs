use std::fs;
use std::path::PathBuf;

use dbotter::drivers::mysql_catalog::{
    CatalogRetention, bounded_select_template, quote_mysql_identifier,
};
use dbotter::model::{
    CatalogNode, CatalogNodeIdentity, CatalogNodeKind, CatalogRetainedCounts, MAX_CATALOG_SCHEMAS,
    MAX_CATALOG_UTF8_BYTES,
};

fn source(path: &str) -> String {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    fs::read_to_string(root.join(path)).unwrap_or_else(|error| panic!("read {path}: {error}"))
}

fn schema_node(name: String) -> CatalogNode {
    CatalogNode {
        identity: CatalogNodeIdentity::Schema {
            schema: name.clone(),
        },
        kind: CatalogNodeKind::Schema,
        name,
        type_name: None,
        nullable: None,
        ordinal: None,
    }
}

#[test]
fn p4_catalog_queries_are_three_static_prepared_binary_keyset_plans() {
    let catalog = source("src/drivers/mysql_catalog.rs");

    for required in [
        "information_schema.SCHEMATA",
        "information_schema.TABLES",
        "information_schema.COLUMNS",
        "SCHEMA_QUERY",
        "RELATION_QUERY",
        "COLUMN_QUERY",
    ] {
        assert!(
            catalog.contains(required),
            "missing catalog plan {required}"
        );
    }
    assert_eq!(
        catalog.matches("LIMIT ?").count(),
        3,
        "each level must bind exactly one page_size + 1 server limit"
    );
    assert!(
        catalog.matches(".prepare(").count() >= 3,
        "all three static catalog statements must cross the prepared protocol"
    );
    assert!(
        !catalog.contains("sqlx::query(")
            && !catalog.contains("sqlx::raw_sql")
            && !catalog.contains("format!(\"SELECT"),
        "catalog SQL must not gain a text-protocol or interpolated path"
    );
    assert!(
        catalog.contains("CAST(SCHEMA_NAME AS BINARY)")
            && catalog.contains("CAST(TABLE_NAME AS BINARY)")
            && catalog.contains("CAST(COLUMN_NAME AS BINARY)"),
        "catalog keysets must have deterministic binary ordering"
    );
    assert!(
        catalog.contains("page_size.saturating_add(1)"),
        "the bound limit must be page_size + 1"
    );
}

#[test]
fn p4_opaque_tokens_are_context_bound_and_integrity_checked() {
    let catalog = source("src/drivers/mysql_catalog.rs");
    let service = source("src/service.rs");
    let runtime = source("src/ui/runtime.rs");
    let manifest = source("Cargo.toml");

    for required in [
        "TOKEN_VERSION",
        "encode_page_token",
        "decode_page_token",
        "Hmac<Sha256>",
        "verify_slice",
        "profile_fingerprint",
        "profile_generation",
        "parent_fingerprint",
        "prefix_fingerprint",
        "page_size",
    ] {
        assert!(
            catalog.contains(required),
            "opaque token integrity is missing {required}"
        );
    }
    for required in ["hmac =", "getrandom =", "zeroize ="] {
        assert!(
            manifest.contains(required),
            "catalog token key dependency is missing {required}"
        );
    }
    for required in [
        "CatalogTokenKeyStore",
        "catalog_token_keys: Arc<CatalogTokenKeyStore>",
        "CatalogTokenKeyStore::for_config_path",
        "load_or_create",
        "create_new(true)",
        "mode(0o600)",
        "sync_all()",
    ] {
        assert!(
            catalog.contains(required) || service.contains(required),
            "persistent cross-process token integrity is missing {required}"
        );
    }
    assert!(
        !service.contains("CatalogTokenKey::generate"),
        "ApplicationService construction must not generate a process-local key"
    );
    assert!(
        service.contains("spawn_catalog_token_key_load")
            && service.contains("tokio::task::spawn_blocking")
            && service.contains("CatalogTokenKeyUnavailable")
            && service.contains("PublicSummary::InternalFailure"),
        "sidecar I/O must use a typed blocking/internal-failure service path"
    );
    let browse = runtime
        .split("async fn run_catalog_browse")
        .nth(1)
        .expect("catalog runtime function");
    assert!(
        browse
            .find("spawn_catalog_token_key_load")
            .expect("pre-session key load")
            < browse
                .find("acquire_session_at")
                .expect("catalog session acquisition"),
        "catalog key loading must finish or cancel before session acquisition"
    );
    assert!(
        browse.contains("await_pre_session_blocking"),
        "cancel/timeout must stay responsive while sidecar I/O is blocking"
    );
    assert!(
        catalog.contains("CatalogTokenKey(<redacted>)") && !catalog.contains("fn token_digest("),
        "token keys must be redacted and tokens must not use a forgeable plain digest"
    );
}

#[test]
fn p4_retention_caps_stop_tokens_and_clear_restores_prefix_recovery() {
    let mut retention = CatalogRetention::default();
    let nodes = (0..=MAX_CATALOG_SCHEMAS)
        .map(|index| schema_node(format!("schema-{index:04}")))
        .collect();
    let outcome = retention.retain(nodes);

    assert_eq!(outcome.nodes.len(), MAX_CATALOG_SCHEMAS);
    assert!(outcome.truncated);
    assert_eq!(
        outcome.retained_counts,
        CatalogRetainedCounts {
            schemas: MAX_CATALOG_SCHEMAS,
            ..CatalogRetainedCounts::default()
        }
    );

    retention.clear();
    let narrowed = retention.retain(vec![schema_node("schema-0200".to_owned())]);
    assert_eq!(narrowed.nodes.len(), 1);
    assert!(!narrowed.truncated);

    retention.clear();
    let bytes = retention.retain(vec![schema_node("x".repeat(MAX_CATALOG_UTF8_BYTES + 1))]);
    assert!(bytes.nodes.is_empty());
    assert!(bytes.truncated);
    assert_eq!(bytes.retained_utf8_bytes, 0);
}

#[test]
fn p4_identifier_quoting_and_select_template_are_exact_and_bounded() {
    assert_eq!(quote_mysql_identifier("a`b"), "`a``b`");
    assert_eq!(
        bounded_select_template("schema`name", "view`name"),
        "SELECT * FROM `schema``name`.`view``name` LIMIT 500"
    );
}
