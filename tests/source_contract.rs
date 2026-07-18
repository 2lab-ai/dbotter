use std::fs;
use std::path::{Path, PathBuf};

use proc_macro2::TokenStream;
use quote::ToTokens as _;
use syn::{Attribute, ImplItem, Item, TraitItem};

#[test]
fn every_production_ast_region_has_no_panicking_placeholder_or_legacy_upsert() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut files = vec![root.join("build.rs")];
    collect_rs(&root.join("src"), &mut files);
    for path in files {
        let production = production_tokens(&path);
        for forbidden in [
            "panic!(",
            "todo!(",
            "unimplemented!(",
            ".unwrap(",
            ".expect(",
            "upsert_profile",
            "UpsertProfile",
        ] {
            assert!(
                !production.contains(forbidden),
                "{} contains forbidden production token {forbidden}",
                path.display()
            );
        }
        if path.ends_with("src/ui/adapter.rs") {
            assert!(
                production.contains("bounded_ports"),
                "production after an item-level cfg(test) must still be scanned"
            );
        }
    }
}

#[test]
fn workspace_fingerprint_normalizes_platform_nanoseconds_with_checked_conversion() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace = fs::read_to_string(root.join("src/workspace.rs")).expect("workspace source");
    let compact = workspace
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect::<String>();

    for field in ["st_mtime_nsec", "st_ctime_nsec"] {
        let checked_conversion = format!("normalize_stat_nanoseconds(stat.{field})?");
        assert!(
            compact.contains(&checked_conversion),
            "{field} differs across Unix targets and must be normalized without truncation"
        );
    }
    assert!(
        compact.contains(
            "fnnormalize_stat_nanoseconds<T>(value:T)->Result<i64,WorkspaceStoreError>\
             whereT:TryInto<i64>,{value.try_into().map_err(|_|WorkspaceStoreError::UnsafePath)}"
        ),
        "platform stat nanoseconds must use one generic checked conversion"
    );
}

#[test]
fn workspace_fingerprint_normalizes_all_platform_varying_stat_fields() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace = fs::read_to_string(root.join("src/workspace.rs")).expect("workspace source");
    let compact = workspace
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect::<String>();

    for (field, expected_calls) in [("st_dev", 2), ("st_mode", 1), ("st_nlink", 1)] {
        let checked_conversion = format!("normalize_stat_value(stat.{field})?");
        assert_eq!(
            compact.matches(&checked_conversion).count(),
            expected_calls,
            "{field} differs across Unix targets and must use the generic checked normalizer"
        );
    }
    assert!(
        compact.contains(
            "fnnormalize_stat_value<T,U>(value:T)->Result<U,WorkspaceStoreError>\
             whereT:TryInto<U>,{value.try_into().map_err(|_|WorkspaceStoreError::UnsafePath)}"
        ),
        "platform stat fields must share one generic checked conversion"
    );

    let editor = fs::read_to_string(root.join("src/ui/editor.rs")).expect("editor source");
    assert!(
        !editor.contains("format!(\"{:?}\", &candidates[0])"),
        "the contract test must compile cleanly under the Preview Rust toolchain"
    );
}

#[test]
fn p2_controller_identity_and_secret_boundaries_are_structural() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let service = fs::read_to_string(root.join("src/service.rs")).expect("service source");
    let runtime = fs::read_to_string(root.join("src/ui/runtime.rs")).expect("runtime source");
    let ui = fs::read_to_string(root.join("src/ui/mod.rs")).expect("ui source");
    let compact_ui = ui
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect::<String>();

    assert!(service.contains("struct CachedSession"));
    assert!(
        service.contains("state: Arc<RwLock<ServiceState>>"),
        "observed generations and cached-session metadata need one atomic lock domain"
    );
    assert!(!service.contains("observed: Arc<RwLock<ObservedState>>"));
    assert!(!service.contains("sessions: Arc<RwLock<HashMap<ProfileId, CachedSession>>>"));
    for field in [
        "profile_generation: ProfileGeneration",
        "session_generation: SessionGeneration",
        "connection_fingerprint: ConnectionFingerprint",
    ] {
        assert!(
            service.contains(field),
            "missing cache identity field {field}"
        );
    }
    assert!(
        !service.contains("#[derive(Clone)]\npub struct SessionLease"),
        "raw session lease must not be cloneable"
    );
    assert!(
        !service.contains("pub async fn check(\n"),
        "public saved-profile work must require an explicit generation"
    );
    assert!(
        !service.contains("pub async fn execute(&self"),
        "public saved-profile execution must require an explicit generation"
    );
    assert!(runtime.contains("pub struct RegisteredTask"));
    for field in ["operation_id", "scope", "cancel", "join"] {
        assert!(
            runtime.contains(field),
            "missing registered-task field {field}"
        );
    }
    assert!(runtime.contains("tokio::select!"));
    assert!(runtime.contains("biased;"));
    assert!(
        ui.contains("pub async fn run"),
        "native wrapper must remain alive to join controller shutdown"
    );
    assert!(
        ui.contains("request_shutdown") && compact_ui.contains("runtime.wait().await"),
        "eframe return must signal and await runtime before Tokio teardown"
    );
    assert!(
        !ui.contains("_runtime"),
        "dropping a JoinHandle must not be the native shutdown strategy"
    );
}

#[test]
fn config_uncertain_native_controls_keep_only_reload_and_shutdown_available() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let app = fs::read_to_string(root.join("src/ui/app.rs")).expect("app source");
    let profile_form =
        fs::read_to_string(root.join("src/ui/profile_form.rs")).expect("profile form source");

    assert!(
        app.contains("is_config_uncertain") && app.contains("Reload"),
        "native app must derive action availability from config certainty and expose reload"
    );
    assert!(
        profile_form.contains("ConfigUncertain")
            && profile_form.contains("actions_enabled")
            && profile_form.contains("config_uncertain"),
        "profile save controls must close when config state becomes uncertain"
    );
}

#[test]
fn runtime_mutation_failure_carrier_stays_internal_and_non_debug() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let service = fs::read_to_string(root.join("src/service.rs")).expect("service source");

    assert!(service.contains("pub(crate) struct RuntimeMutationFailure"));
    assert!(
        !service.contains("#[derive(Debug)]\npub(crate) struct RuntimeMutationFailure")
            && !service.contains("impl Debug for RuntimeMutationFailure")
            && !service.contains("impl fmt::Debug for RuntimeMutationFailure"),
        "the failure carrier may contain deferred secret updates and must not be debug-formattable"
    );
    assert!(
        !service.contains("pub struct RuntimeMutationFailure"),
        "the deferred cleanup carrier must not cross the crate API boundary"
    );
}

#[test]
fn p3_user_text_and_resource_boundaries_are_structural() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let model = fs::read_to_string(root.join("src/model.rs")).expect("model source");
    let drivers = fs::read_to_string(root.join("src/drivers/mod.rs")).expect("driver source");
    let mysql = fs::read_to_string(root.join("src/drivers/mysql.rs")).expect("mysql source");

    for forbidden in ["sqlx::raw_sql", "execute_raw", "COM_QUERY", "raw fallback"] {
        assert!(
            !mysql.contains(forbidden),
            "prepared-only MySQL source contains forbidden token {forbidden}"
        );
    }
    for required in [
        "PreparedMySqlRequest",
        "RedisExecuteRequest",
        "ExecuteBatchRequest",
        "CatalogRequest",
        "RedisScanRequest",
        "RedisKeyInspectRequest",
        "ResultSnapshot",
    ] {
        assert!(
            model.contains(required),
            "missing typed P3 request/result {required}"
        );
    }
    for required in [
        "trait ConnectionPing",
        "trait MySqlReadExecution",
        "trait MySqlUnprovenReadLease",
        "trait MySqlProvenReadLease",
        "trait RedisExecution",
        "trait CatalogBrowser",
        "trait KeyspaceBrowser",
        "enum ConnectedResources",
    ] {
        assert!(
            drivers.contains(required),
            "missing typed P3 driver seam {required}"
        );
    }
    assert!(
        !drivers.contains("load_resource"),
        "generic stringly resource loading is forbidden"
    );
}

#[test]
fn p4_production_ast_has_only_closed_static_internal_text_and_prepared_user_fetches() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut files = vec![root.join("build.rs")];
    collect_rs(&root.join("src"), &mut files);
    let mut production = String::new();
    let mut mysql = String::new();
    let mut mysql_catalog = String::new();
    for path in files {
        let tokens = production_tokens(&path);
        if path.ends_with("src/drivers/mysql.rs") {
            mysql = tokens.clone();
        }
        if path.ends_with("src/drivers/mysql_catalog.rs") {
            mysql_catalog = tokens.clone();
        }
        production.push_str(&tokens);
    }

    assert_eq!(
        production.matches("sqlx::query(").count(),
        5,
        "PING, the frozen capability row and three closed read-session SET operations are the only static queries"
    );
    assert_eq!(
        production.matches(".execute(").count(),
        4,
        "only the four exact static internal statements may use Executor::execute"
    );
    for required in [
        "sqlx::query(\"SELECT1\").execute(&self.pool)",
        "sqlx::query(\"SELECT@@version,@@version_comment,@@SESSION.character_set_client,@@SESSION.character_set_connection,@@SESSION.character_set_results,@@SESSION.collation_connection,@@SESSION.time_zone,@@SESSION.sql_mode,@@GLOBAL.partial_revokes\").fetch_one(&mut*connection)",
        "sqlx::query(\"SETNAMESutf8mb4\").execute(&mut*connection)",
        "sqlx::query(\"SETSESSIONtime_zone='+00:00'\").execute(&mut*connection)",
        "sqlx::query(\"SETSESSIONTRANSACTIONREADONLY\").execute(&mut*self.connection)",
    ] {
        assert!(
            mysql.contains(required),
            "missing exact static internal MySQL statement {required}"
        );
    }
    assert_eq!(production.matches("sqlx::query_as(").count(), 2);
    assert_eq!(production.matches(".fetch_one(").count(), 3);
    for forbidden in [
        "sqlx::raw_sql(",
        "sqlx::query_scalar(",
        "sqlx::query_with(",
        "Executor::execute(",
        "Executor>::execute(",
        "::execute(",
        ".fetch(",
        ".fetch_all(",
        ".fetch_optional(",
    ] {
        assert!(
            !production.contains(forbidden),
            "production AST contains a text-protocol escape hatch: {forbidden}"
        );
    }
    assert_eq!(production.matches(".fetch_many(").count(), 4);
    assert!(
        mysql.contains(
            "letquery=statement.query();letmutstream=(&mut*connection).fetch_many(query);"
        ),
        "the user-text fetch must originate from the server-prepared statement"
    );
    assert_eq!(mysql_catalog.matches(".prepare(").count(), 3);
    assert_eq!(mysql_catalog.matches(".fetch_many(").count(), 3);
    assert_eq!(mysql_catalog.matches("statement.query()").count(), 3);
    assert!(
        !mysql_catalog.contains("sqlx::query(") && !mysql_catalog.contains("sqlx::raw_sql("),
        "catalog browsing must use only its three static server-prepared statements"
    );
}

fn production_tokens(path: &Path) -> String {
    let source = fs::read_to_string(path).expect("source reads");
    let parsed = syn::parse_file(&source).expect("production Rust parses");
    let mut tokens = TokenStream::new();
    collect_items(&parsed.items, &mut tokens);
    tokens
        .to_string()
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect()
}

fn collect_items(items: &[Item], output: &mut TokenStream) {
    for item in items {
        if test_only(item_attributes(item)) {
            continue;
        }
        match item {
            Item::Mod(module) if module.content.is_some() => {
                let (_, items) = module.content.as_ref().expect("checked inline module");
                collect_items(items, output);
            }
            Item::Impl(item_impl) => {
                item_impl.generics.to_tokens(output);
                item_impl.self_ty.to_tokens(output);
                for item in &item_impl.items {
                    if !test_only(impl_item_attributes(item)) {
                        item.to_tokens(output);
                    }
                }
            }
            Item::Trait(item_trait) => {
                item_trait.ident.to_tokens(output);
                for item in &item_trait.items {
                    if !test_only(trait_item_attributes(item)) {
                        item.to_tokens(output);
                    }
                }
            }
            _ => item.to_tokens(output),
        }
    }
}

fn test_only(attributes: &[Attribute]) -> bool {
    attributes.iter().any(|attribute| {
        if !attribute.path().is_ident("cfg") {
            return false;
        }
        let syn::Meta::List(list) = &attribute.meta else {
            return false;
        };
        list.tokens
            .to_string()
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect::<String>()
            == "test"
    })
}

fn item_attributes(item: &Item) -> &[Attribute] {
    match item {
        Item::Const(item) => &item.attrs,
        Item::Enum(item) => &item.attrs,
        Item::ExternCrate(item) => &item.attrs,
        Item::Fn(item) => &item.attrs,
        Item::ForeignMod(item) => &item.attrs,
        Item::Impl(item) => &item.attrs,
        Item::Macro(item) => &item.attrs,
        Item::Mod(item) => &item.attrs,
        Item::Static(item) => &item.attrs,
        Item::Struct(item) => &item.attrs,
        Item::Trait(item) => &item.attrs,
        Item::TraitAlias(item) => &item.attrs,
        Item::Type(item) => &item.attrs,
        Item::Union(item) => &item.attrs,
        Item::Use(item) => &item.attrs,
        Item::Verbatim(_) => &[],
        _ => &[],
    }
}

fn impl_item_attributes(item: &ImplItem) -> &[Attribute] {
    match item {
        ImplItem::Const(item) => &item.attrs,
        ImplItem::Fn(item) => &item.attrs,
        ImplItem::Type(item) => &item.attrs,
        ImplItem::Macro(item) => &item.attrs,
        ImplItem::Verbatim(_) => &[],
        _ => &[],
    }
}

fn trait_item_attributes(item: &TraitItem) -> &[Attribute] {
    match item {
        TraitItem::Const(item) => &item.attrs,
        TraitItem::Fn(item) => &item.attrs,
        TraitItem::Type(item) => &item.attrs,
        TraitItem::Macro(item) => &item.attrs,
        TraitItem::Verbatim(_) => &[],
        _ => &[],
    }
}

fn collect_rs(directory: &Path, files: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(directory).expect("source directory reads") {
        let entry = entry.expect("directory entry");
        let path = entry.path();
        if path.is_dir() {
            collect_rs(&path, files);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            files.push(path);
        }
    }
}
