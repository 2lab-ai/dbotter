use std::collections::BTreeSet;

use dbotter::drivers::redis_browser::{RedisScanAccumulator, inspect_command_names};
use dbotter::drivers::{DriverError, RedisTlsFailure};
use dbotter::model::{
    DriverCapabilities, MAX_REDIS_KEY_BYTES, OperationId, ProfileGeneration, ProfileId, PublicCode,
    PublicSummary, RedisKeyEntry, RedisKeyFilter, RedisKeyId, RedisKeyPage, RedisScanConsistency,
    RedisValueType, RequestIdentity,
};
use dbotter::service::{ServiceError, SessionDisposition};

fn identity(operation_id: u64) -> RequestIdentity {
    RequestIdentity::new(
        ProfileId("redis-contract".to_owned()),
        ProfileGeneration(7),
        OperationId(operation_id),
    )
}

fn page(operation_id: u64, next_cursor: u64, keys: Vec<Vec<u8>>) -> RedisKeyPage {
    let entries = keys
        .into_iter()
        .map(|key| RedisKeyEntry::new(RedisKeyId(key)))
        .collect::<Vec<_>>();
    RedisKeyPage {
        identity: identity(operation_id),
        next_cursor,
        retained_count: entries.len(),
        retained_bytes: entries.iter().map(|entry| entry.id.as_bytes().len()).sum(),
        keys: entries,
        skipped_oversize: 0,
        consistency: RedisScanConsistency::Weak,
        truncated: false,
        stale: false,
    }
}

#[test]
fn literal_prefix_and_glob_have_exact_non_interchangeable_wire_semantics() {
    let literal = RedisKeyFilter::LiteralPrefix(r"a*?[]\z".to_owned());
    assert_eq!(literal.match_pattern().as_deref(), Ok(r"a\*\?\[\]\\z*"));

    let glob = RedisKeyFilter::Glob(r"a*?[]\z".to_owned());
    assert_eq!(glob.match_pattern().as_deref(), Ok(r"a*?[]\z"));
}

#[test]
fn scan_accumulator_dedupes_only_by_raw_bytes_and_restart_clears_cycle_state() {
    let mut scan = RedisScanAccumulator::new(RedisKeyFilter::LiteralPrefix("bin:".to_owned()));
    scan.apply_page(page(
        1,
        91,
        vec![b"bin:\xff".to_vec(), vec![b'b', b'i', b'n', b':', 0xff]],
    ));
    scan.apply_page(page(
        2,
        0,
        vec![vec![b'b', b'i', b'n', b':', 0xfe], b"bin:\xff".to_vec()],
    ));

    assert_eq!(scan.keys().len(), 2, "only exact raw duplicates collapse");
    assert_eq!(
        scan.keys()[0].display,
        scan.keys()[1].display,
        "lossy display collisions must remain separate identities"
    );
    assert_eq!(scan.next_cursor(), 0, "cursor zero alone closes this cycle");
    assert!(scan.is_complete());
    assert_eq!(
        scan.keys()
            .iter()
            .map(|entry| entry.key_base64.as_str())
            .collect::<BTreeSet<_>>()
            .len(),
        2
    );

    scan.restart(RedisKeyFilter::Glob("bin:*".to_owned()));
    assert!(scan.keys().is_empty());
    assert_eq!(scan.next_cursor(), 0);
    assert!(
        !scan.is_complete(),
        "cursor zero before a request is not completion"
    );
}

#[test]
fn oversize_keys_never_become_truncated_selectable_identities() {
    let mut scan = RedisScanAccumulator::new(RedisKeyFilter::Glob("*".to_owned()));
    let mut response = page(
        1,
        13,
        vec![b"kept".to_vec(), vec![b'x'; MAX_REDIS_KEY_BYTES + 1]],
    );
    response.skipped_oversize = 1;
    response.keys.pop();
    response.retained_count = 1;
    response.retained_bytes = 4;
    response.truncated = true;
    scan.apply_page(response);

    assert_eq!(scan.keys().len(), 1);
    assert_eq!(scan.skipped_oversize(), 1);
    assert!(scan.truncated());
}

#[test]
fn inspect_uses_only_the_frozen_representative_command_families() {
    assert_eq!(
        inspect_command_names(RedisValueType::String),
        &["TYPE", "PTTL", "STRLEN", "GETRANGE", "TYPE"]
    );
    assert_eq!(
        inspect_command_names(RedisValueType::Hash),
        &["TYPE", "PTTL", "HLEN", "HSCAN", "TYPE"]
    );
    assert_eq!(
        inspect_command_names(RedisValueType::List),
        &["TYPE", "PTTL", "LLEN", "LRANGE", "TYPE"]
    );
    assert_eq!(
        inspect_command_names(RedisValueType::Set),
        &["TYPE", "PTTL", "SCARD", "SSCAN", "TYPE"]
    );
    assert_eq!(
        inspect_command_names(RedisValueType::SortedSet),
        &["TYPE", "PTTL", "ZCARD", "ZRANGE", "TYPE"]
    );
    assert_eq!(
        inspect_command_names(RedisValueType::Stream),
        &["TYPE", "PTTL", "XLEN", "XRANGE", "TYPE"]
    );
    assert_eq!(
        inspect_command_names(RedisValueType::ModuleOrUnknown),
        &["TYPE", "PTTL", "TYPE"]
    );
}

#[test]
fn tls_ca_and_hostname_failures_have_disjoint_public_codes() {
    let ca = ServiceError::Driver(DriverError::RedisTls {
        failure: RedisTlsFailure::CaUntrusted,
    });
    assert_eq!(
        ca.public_error_parts(),
        (
            PublicSummary::TlsVerificationFailed,
            PublicCode::RedisTlsCaUntrustedIssuer,
        )
    );

    let host = ServiceError::Driver(DriverError::RedisTls {
        failure: RedisTlsFailure::HostnameMismatch,
    });
    assert_eq!(
        host.public_error_parts(),
        (
            PublicSummary::TlsVerificationFailed,
            PublicCode::TlsHostnameMismatch,
        )
    );
}

#[test]
fn resource_staleness_keeps_the_session_but_tls_failure_evicts_it() {
    assert_eq!(
        SessionDisposition::for_driver_error(&DriverError::RedisKeyMissing),
        SessionDisposition::Keep
    );
    assert_eq!(
        SessionDisposition::for_driver_error(&DriverError::RedisKeyTypeChanged),
        SessionDisposition::Keep
    );
    assert_eq!(
        SessionDisposition::for_driver_error(&DriverError::RedisTls {
            failure: RedisTlsFailure::CaUntrusted,
        }),
        SessionDisposition::Evict
    );
}

#[test]
fn keyspace_capability_flips_ready_as_one_slice() {
    let descriptor = &dbotter::drivers::redis::DESCRIPTOR;
    assert!(
        descriptor
            .capabilities
            .contains(DriverCapabilities::KEYSPACE_BROWSE)
    );
    assert!(
        !descriptor
            .planned_capabilities
            .contains(DriverCapabilities::KEYSPACE_BROWSE)
    );
}

#[test]
fn production_source_has_no_keys_command_or_plaintext_tls_fallback() {
    let browser = include_str!("../src/drivers/redis_browser.rs");
    let redis = include_str!("../src/drivers/redis.rs");
    let compact = |source: &str| {
        source
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect::<String>()
    };
    let browser = compact(browser);
    let redis = compact(redis);

    assert!(!browser.contains("cmd(\"KEYS\")"));
    assert!(browser.contains("cmd(\"SCAN\")"));
    let required_start = redis
        .find("TlsMode::Required=>{")
        .expect("Required TLS arm");
    let required_end = redis[required_start..]
        .find("TlsMode::Preferred=>{")
        .map(|offset| required_start + offset)
        .expect("Preferred TLS arm after Required");
    let required = &redis[required_start..required_end];
    assert!(required.contains("ConnectionAddr::TcpTls"));
    assert!(required.contains("insecure:false"));
    assert!(!required.contains("ConnectionAddr::Tcp{"));
    assert!(!required.contains("or_else("));
}

#[test]
fn redis_ui_events_must_preserve_typed_public_error_and_exact_session_truth() {
    let model = include_str!("../src/ui/model.rs");
    let runtime = include_str!("../src/ui/runtime.rs");

    assert!(
        model.contains("error: PublicOperationError"),
        "Redis failure events currently collapse PublicCode into PublicSummary"
    );
    assert!(
        model.contains("session_generation: SessionGeneration")
            && model.contains("session_disposition: SessionDisposition"),
        "Redis success events must publish their exact acquired session truth"
    );
    assert!(
        runtime.contains("ControllerMessage::SessionAcquired")
            && runtime.contains("failed_redis_resource_event"),
        "Redis runtime must use the scoped lease lifecycle and typed error carrier"
    );
}

#[test]
fn required_redis_tls_ui_must_have_stable_ca_ids_and_real_picker_binding() {
    let profile_form = include_str!("../src/ui/profile_form.rs");
    let app = include_str!("../src/ui/app.rs");

    for required in [
        "profile.redis_tls.ca_file",
        "profile.redis_tls.ca_file.pick",
        "PickRedisCaFile",
        "bind_redis_ca_file",
    ] {
        assert!(
            profile_form.contains(required) || app.contains(required),
            "missing Required TLS UI contract: {required}"
        );
    }
}

#[test]
fn redis_load_more_must_be_bound_to_the_page_filter_context() {
    let explorer = include_str!("../src/ui/redis_explorer.rs");
    assert!(
        explorer.contains("page_filter_matches_draft"),
        "editing the draft filter must require Refresh before cursor reuse"
    );
}
