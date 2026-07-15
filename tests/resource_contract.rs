use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use dbotter::model::{
    CatalogLevel, CatalogPage, CatalogPageToken, CatalogRequest, CatalogRetainedCounts, Cell,
    Column, DriverKind, ExportFormat, ExportResult, MAX_CATALOG_PAGE_SIZE, MAX_REDIS_CELL_BYTES,
    MAX_REDIS_COMMAND_BYTES, MAX_REDIS_DEPTH, MAX_RESULT_BYTES, MAX_RESULT_CELL_BYTES,
    OverwritePolicy, PreparedMySqlRequest, ProfileGeneration, ProfileId, QueryResult,
    RedisExecuteRequest, RedisKeyEntry, RedisKeyFilter, RedisKeyId, RedisKeyInspectRequest,
    RedisKeyPage, RedisScanConsistency, RedisScanRequest, RequestIdentity, ResultId, ResultNotice,
    ResultProvenance, ResultRetentionPolicy, ResultSnapshot,
};

fn source(path: &str) -> String {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    fs::read_to_string(root.join(path)).unwrap_or_else(|error| panic!("read {path}: {error}"))
}

fn compact(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect()
}

#[test]
fn p3_resource_models_are_typed_bounded_and_keep_raw_redis_identity() {
    let model = source("src/model.rs");

    for required in [
        "CatalogRequest",
        "CatalogPage",
        "RedisScanRequest",
        "RedisKeyInspectRequest",
        "RedisKeyId",
        "RedisKeyPage",
        "RedisValuePreview",
        "ResultSnapshot",
        "ResultProvenance",
        "ResultNotice",
    ] {
        assert!(
            model.contains(required),
            "missing typed P3 model {required}"
        );
    }

    assert!(
        model.contains("RedisKeyId(pub Vec<u8>)")
            || compact(&model).contains("RedisKeyId(pubVec<u8>)"),
        "Redis key identity must retain raw bytes rather than display text"
    );
    for cap in [
        "MAX_RESULT_COLUMNS",
        "MAX_RESULT_BYTES",
        "MAX_RESULT_CELL_BYTES",
        "MAX_RESULT_NOTICES",
        "MAX_REDIS_CELL_BYTES",
        "MAX_REDIS_DEPTH",
    ] {
        assert!(model.contains(cap), "missing retained-result cap {cap}");
    }
}

#[test]
fn p3_driver_boundary_is_split_by_backend_and_resource_kind() {
    let drivers = source("src/drivers/mod.rs");
    for required in [
        "trait ConnectionPing",
        "trait MySqlPreparedExecution",
        "trait RedisExecution",
        "trait CatalogBrowser",
        "trait KeyspaceBrowser",
        "enum ConnectedResources",
    ] {
        assert!(
            drivers.contains(required),
            "missing split driver seam {required}"
        );
    }

    assert!(
        !drivers.contains("load_resource")
            && !drivers.contains("load_page(&self, resource: String"),
        "resource loading must not collapse to a stringly generic seam"
    );
}

#[test]
fn redis_typed_execute_policy_is_constructor_bound_and_rechecked_before_io() {
    let model = compact(&source("src/model.rs"));
    let driver = compact(&source("src/drivers/redis.rs"));
    let request_start = model
        .find("pubstructRedisExecuteRequest{")
        .expect("Redis request struct");
    let request_end = model[request_start..]
        .find('}')
        .map(|offset| request_start + offset)
        .expect("Redis request struct end");
    let request_fields = &model[request_start..request_end];

    assert!(model.contains("implRedisExecuteRequest{pubfnnew("));
    for public_field in [
        "pubidentity:RequestIdentity",
        "pubargv:Vec<Vec<u8>>",
        "pubrow_limit:u32",
        "pubtimeout:Duration",
    ] {
        assert!(
            !request_fields.contains(public_field),
            "validated Redis request field became forgeable: {public_field}"
        );
    }
    let execute_start = driver
        .find("pubasyncfnexecute_command(")
        .expect("Redis execute method");
    let execute_end = driver[execute_start..]
        .find("pubasyncfnscan_keys(")
        .map(|offset| execute_start + offset)
        .expect("Redis execute method end");
    let execute = &driver[execute_start..execute_end];
    let validate = execute
        .find("request.validate()")
        .expect("driver request validation");
    let network = execute.find("query_async").expect("Redis network dispatch");
    assert!(
        validate < network,
        "the closed Redis policy must be rechecked before network dispatch"
    );
}

#[test]
fn catalog_and_keyspace_capabilities_are_independently_ready() {
    let model = source("src/model.rs");
    let mysql = &dbotter::drivers::mysql::DESCRIPTOR;
    let redis = &dbotter::drivers::redis::DESCRIPTOR;

    assert!(
        model.contains("KEYSPACE_BROWSE"),
        "missing independent Redis browse capability"
    );
    assert!(
        mysql
            .capabilities
            .contains(dbotter::model::DriverCapabilities::CATALOG)
    );
    assert!(
        !mysql
            .planned_capabilities
            .contains(dbotter::model::DriverCapabilities::CATALOG)
    );
    assert!(
        redis
            .capabilities
            .contains(dbotter::model::DriverCapabilities::KEYSPACE_BROWSE)
    );
    assert!(
        !redis
            .planned_capabilities
            .contains(dbotter::model::DriverCapabilities::KEYSPACE_BROWSE)
    );
}

fn identity() -> RequestIdentity {
    RequestIdentity::new(
        ProfileId("sensitive-profile-id".to_owned()),
        ProfileGeneration(2),
        dbotter::model::OperationId(3),
    )
}

fn provenance(driver: DriverKind) -> ResultProvenance {
    ResultProvenance {
        result_id: ResultId(4),
        profile_id: ProfileId("sensitive-profile-id".to_owned()),
        profile_generation: ProfileGeneration(2),
        operation_id: dbotter::model::OperationId(3),
        driver,
        completed_at_unix_ms: 1_700_000_000_000,
        duration_ms: 17,
    }
}

fn one_text_result(text: String) -> QueryResult {
    QueryResult {
        columns: vec![Column {
            name: "value".to_owned(),
            type_name: "TEXT".to_owned(),
        }],
        rows: vec![vec![Cell::Text(text)]],
        affected_rows: 0,
        last_insert_id: None,
        elapsed_ms: 17,
        truncated: false,
        backend_notices_present: false,
    }
}

#[test]
fn typed_requests_validate_bounds_before_network_and_redact_value_data() {
    let sentinel = "request-value-sentinel";
    let mysql = PreparedMySqlRequest {
        identity: identity(),
        statement: sentinel.to_owned(),
        row_limit: 500,
        timeout: Duration::from_secs(30),
    };
    assert!(mysql.validate().is_ok());
    let mysql_debug = format!("{mysql:?}");
    assert!(!mysql_debug.contains(sentinel));
    assert!(!mysql_debug.contains("sensitive-profile-id"));

    let redis = RedisExecuteRequest::new(
        identity(),
        vec![b"GET".to_vec(), sentinel.as_bytes().to_vec()],
        500,
        Duration::from_secs(30),
    )
    .expect("allowed command");
    let redis_debug = format!("{redis:?}");
    assert!(!redis_debug.contains(sentinel));
    assert!(!redis_debug.contains("sensitive-profile-id"));

    assert!(
        RedisExecuteRequest::new(
            identity(),
            vec![vec![b'x'; MAX_REDIS_COMMAND_BYTES + 1]],
            500,
            Duration::from_secs(30),
        )
        .is_err()
    );
    assert!(
        RedisExecuteRequest::new(
            identity(),
            vec![b"XREAD".to_vec(), b"BLOCK".to_vec(), b"0".to_vec()],
            500,
            Duration::from_secs(30),
        )
        .is_err(),
        "the typed request constructor must enforce the closed policy"
    );

    let catalog = CatalogRequest::Relations {
        identity: identity(),
        schema: sentinel.to_owned(),
        prefix: Some(sentinel.to_owned()),
        page_token: Some(CatalogPageToken(sentinel.to_owned())),
        page_size: 50,
        timeout: Duration::from_secs(5),
    };
    assert!(catalog.validate().is_ok());
    let catalog_debug = format!("{catalog:?}");
    assert!(!catalog_debug.contains(sentinel));
    assert!(!catalog_debug.contains("sensitive-profile-id"));

    let too_large_page = CatalogRequest::Schemas {
        identity: identity(),
        prefix: None,
        page_token: None,
        page_size: MAX_CATALOG_PAGE_SIZE + 1,
        timeout: Duration::from_secs(5),
    };
    assert!(too_large_page.validate().is_err());
}

#[test]
fn redis_filter_and_raw_key_contract_preserve_exact_bytes_without_debug_disclosure() {
    let filter = RedisKeyFilter::LiteralPrefix("a*?[b]\\c".to_owned());
    assert_eq!(
        filter.match_pattern().expect("valid filter"),
        "a\\*\\?\\[b\\]\\\\c*"
    );

    let raw = RedisKeyId(vec![0, 0xff, b'a']);
    let inspect = RedisKeyInspectRequest {
        identity: identity(),
        key: raw.clone(),
        timeout: Duration::from_secs(5),
    };
    assert!(inspect.validate().is_ok());
    assert_eq!(inspect.key.as_bytes(), [0, 0xff, b'a']);
    assert!(!format!("{inspect:?}").contains("ff"));

    let scan = RedisScanRequest {
        identity: identity(),
        filter: RedisKeyFilter::Glob("raw:*".to_owned()),
        cursor: 0,
        count_hint: 100,
        timeout: Duration::from_secs(5),
    };
    assert!(scan.validate().is_ok());
    assert!(!format!("{scan:?}").contains("raw:*"));
}

#[test]
fn result_snapshot_enforces_mysql_cell_and_total_caps_with_original_lengths() {
    let original_len = MAX_RESULT_CELL_BYTES + 17;
    let snapshot = ResultSnapshot::retain(
        one_text_result("x".repeat(original_len)),
        provenance(DriverKind::MySql),
        ResultRetentionPolicy::mysql(500),
    );
    let Cell::Text(preview) = &snapshot.rows[0][0] else {
        panic!("expected retained text preview");
    };
    assert_eq!(preview.len(), MAX_RESULT_CELL_BYTES);
    assert_eq!(snapshot.cell_truncations.len(), 1);
    assert_eq!(
        snapshot.cell_truncations[0].original_len,
        Some(original_len)
    );
    assert!(snapshot.cell_truncations[0].truncated);
    assert!(
        snapshot.truncated
            || snapshot
                .notices
                .contains(&ResultNotice::CellPreviewTruncated)
    );
    assert!(snapshot.retained_bytes <= MAX_RESULT_BYTES);

    let rows = (0..9)
        .map(|_| vec![Cell::Text("y".repeat(MAX_RESULT_CELL_BYTES))])
        .collect();
    let total = ResultSnapshot::retain(
        QueryResult {
            columns: vec![Column {
                name: "value".to_owned(),
                type_name: "TEXT".to_owned(),
            }],
            rows,
            affected_rows: 0,
            last_insert_id: None,
            elapsed_ms: 0,
            truncated: false,
            backend_notices_present: false,
        },
        provenance(DriverKind::MySql),
        ResultRetentionPolicy::mysql(10_000),
    );
    assert!(total.retained_bytes <= MAX_RESULT_BYTES);
    assert!(total.truncated);
    assert!(
        total
            .notices
            .contains(&ResultNotice::SnapshotByteLimitReached)
    );
}

#[test]
fn result_snapshot_applies_stricter_redis_cell_depth_and_static_notice_contract() {
    let sentinel = "backend-notice-sentinel";
    let mut nested = serde_json::json!({ "leaf": "value" });
    for _ in 0..=MAX_REDIS_DEPTH {
        nested = serde_json::json!({ "next": nested });
    }
    let snapshot = ResultSnapshot::retain(
        QueryResult {
            columns: vec![Column {
                name: "value".to_owned(),
                type_name: "RESP".to_owned(),
            }],
            rows: vec![
                vec![Cell::Text("z".repeat(MAX_REDIS_CELL_BYTES + 1))],
                vec![Cell::Json(nested)],
            ],
            affected_rows: 0,
            last_insert_id: None,
            elapsed_ms: 0,
            truncated: false,
            backend_notices_present: true,
        },
        provenance(DriverKind::Redis),
        ResultRetentionPolicy::redis(10_000),
    );
    let Cell::Text(preview) = &snapshot.rows[0][0] else {
        panic!("expected Redis text preview");
    };
    assert_eq!(preview.len(), MAX_REDIS_CELL_BYTES);
    assert!(
        snapshot
            .notices
            .contains(&ResultNotice::RedisDepthLimitReached)
    );
    assert!(
        snapshot
            .notices
            .contains(&ResultNotice::BackendNoticesDiscarded)
    );
    assert!(
        !serde_json::to_string(&snapshot)
            .expect("snapshot JSON")
            .contains(sentinel)
    );
    assert!(!format!("{snapshot:?}").contains("value"));
}

#[test]
fn export_result_keeps_exact_tuple_and_redacts_snapshot_and_destination() {
    let snapshot = ResultSnapshot::retain(
        one_text_result("export-value-sentinel".to_owned()),
        provenance(DriverKind::MySql),
        ResultRetentionPolicy::mysql(500),
    );
    let request = ExportResult {
        result_id: ResultId(4),
        operation_id: dbotter::model::OperationId(9),
        snapshot: Arc::new(snapshot),
        format: ExportFormat::Csv,
        destination: PathBuf::from("/tmp/export-path-sentinel.csv"),
        overwrite_policy: OverwritePolicy::DenyOverwrite,
    };
    let debug = format!("{request:?}");
    assert!(debug.starts_with("ExportResult"));
    assert!(debug.contains("ResultId(4)"));
    assert!(debug.contains("OperationId(9)"));
    assert!(!debug.contains("export-value-sentinel"));
    assert!(!debug.contains("export-path-sentinel"));
}

#[test]
fn headless_resource_json_schemas_are_stable_and_never_serialize_raw_key_identity() {
    let catalog = CatalogPage {
        identity: identity(),
        level: CatalogLevel::Schemas,
        parent: None,
        nodes: Vec::new(),
        next_token: None,
        retained_counts: CatalogRetainedCounts::default(),
        retained_utf8_bytes: 0,
        truncated: false,
        stale: false,
        loaded_at: "2026-07-15T00:00:00Z".to_owned(),
    };
    let catalog_value = serde_json::to_value(catalog).expect("catalog JSON");
    let catalog_keys = catalog_value
        .as_object()
        .expect("catalog object")
        .keys()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    assert_eq!(
        catalog_keys,
        BTreeSet::from([
            "identity",
            "level",
            "loaded_at",
            "next_token",
            "nodes",
            "parent",
            "retained_counts",
            "retained_utf8_bytes",
            "stale",
            "truncated",
        ])
    );
    assert!(catalog_value.get("query_text").is_none());

    let raw = RedisKeyId(vec![0, 0xff, b'k']);
    let page = RedisKeyPage {
        identity: identity(),
        next_cursor: 0,
        keys: vec![RedisKeyEntry::new(raw)],
        retained_count: 1,
        skipped_oversize: 0,
        retained_bytes: 3,
        consistency: RedisScanConsistency::Weak,
        truncated: false,
        stale: false,
    };
    let page_value = serde_json::to_value(page).expect("Redis page JSON");
    let key = &page_value["keys"][0];
    assert_eq!(key["key_base64"], "AP9r");
    assert!(key.get("id").is_none());
    assert!(page_value.get("query_text").is_none());
}
