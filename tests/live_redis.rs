//! Mandatory ignored receipt against the Docker Redis plaintext and TLS fixtures.
//!
//! Run through `scripts/verify-live-redis.sh`; missing fixture inputs are a
//! hard failure rather than an ignored-success path.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use dbotter::config::{Config, ConfigWriter};
use dbotter::drivers::redis::{
    RedisSession, reset_transport_attempt_counts, transport_attempt_counts,
};
use dbotter::drivers::redis_browser::RedisScanAccumulator;
use dbotter::drivers::{DriverError, RedisTlsFailure};
use dbotter::model::{
    Cell, ConnectionProfile, CredentialMode, DriverKind, MAX_REDIS_KEY_BYTES, OperationId,
    ProfileGeneration, ProfileId, PublicCode, PublicSummary, RedisExecuteRequest, RedisKeyFilter,
    RedisKeyId, RedisKeyInspectRequest, RedisScanRequest, RedisTlsConfig, RedisTtl, RedisValueType,
    RequestIdentity, TlsMode,
};
use dbotter::secrets::{
    EnvironmentAvailability, SecretError, SessionSecret, SessionSecretStore, SessionSecretUpdate,
};
use dbotter::service::{ApplicationService, DriverConnector, SecretResolver, ServiceError};
use secrecy::SecretString;

const CORRECT_PASSWORD: &str = "dbotter-redis-local-only";
const WRONG_PASSWORD: &str = "dbotter-redis-definitely-wrong";
const ENV_NAME: &str = "DBOTTER_REDIS_PASSWORD";
const TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone)]
struct LiveFixture {
    plaintext_host: String,
    plaintext_port: u16,
    tls_host: String,
    tls_port: u16,
    ca_file: PathBuf,
    wrong_ca_file: PathBuf,
}

impl LiveFixture {
    fn required() -> Self {
        let password = required_env(ENV_NAME);
        assert_eq!(
            password, CORRECT_PASSWORD,
            "{ENV_NAME} must contain the fixture password"
        );
        let fixture = Self {
            plaintext_host: required_env("DBOTTER_LIVE_REDIS_HOST"),
            plaintext_port: required_port("DBOTTER_LIVE_REDIS_PORT"),
            tls_host: required_env("DBOTTER_LIVE_REDIS_TLS_HOST"),
            tls_port: required_port("DBOTTER_LIVE_REDIS_TLS_PORT"),
            ca_file: required_path("DBOTTER_LIVE_REDIS_CA_FILE"),
            wrong_ca_file: required_path("DBOTTER_LIVE_REDIS_WRONG_CA_FILE"),
        };
        assert!(fixture.ca_file.is_file(), "Redis CA fixture is missing");
        assert!(
            fixture.wrong_ca_file.is_file(),
            "Redis wrong-CA fixture is missing"
        );
        fixture
    }

    fn profile(
        &self,
        id: &str,
        tls: TlsMode,
        credential_mode: CredentialMode,
    ) -> ConnectionProfile {
        let (host, port, ca_file) = match tls {
            TlsMode::Disabled => (self.plaintext_host.clone(), self.plaintext_port, None),
            TlsMode::Required => (
                self.tls_host.clone(),
                self.tls_port,
                Some(self.ca_file.clone()),
            ),
            TlsMode::Preferred => panic!("Preferred is not a live fixture mode"),
        };
        ConnectionProfile {
            id: id.to_owned(),
            name: format!("Live Redis {id}"),
            driver: DriverKind::Redis,
            host,
            port,
            database: Some("0".to_owned()),
            username: None,
            tls,
            credential_mode,
            secret_env: (credential_mode == CredentialMode::Environment)
                .then(|| ENV_NAME.to_owned()),
            redis_tls: RedisTlsConfig { ca_file },
        }
    }
}

fn required_env(name: &str) -> String {
    env::var(name).unwrap_or_else(|_| panic!("required live fixture variable {name} is missing"))
}

fn required_port(name: &str) -> u16 {
    required_env(name)
        .parse()
        .unwrap_or_else(|_| panic!("live fixture variable {name} is not a port"))
}

fn required_path(name: &str) -> PathBuf {
    PathBuf::from(required_env(name))
}

fn identity(profile_id: &str, operation: u64) -> RequestIdentity {
    RequestIdentity::new(
        ProfileId(profile_id.to_owned()),
        ProfileGeneration(1),
        OperationId(operation),
    )
}

async fn execute(session: &RedisSession, operation: u64, argv: Vec<Vec<u8>>) {
    let request = RedisExecuteRequest::new(
        identity("redis-live-direct", operation),
        argv,
        1_000,
        TIMEOUT,
    )
    .expect("valid live Redis command");
    session
        .execute_command(&request)
        .await
        .expect("live Redis command");
}

fn argv(parts: &[&str]) -> Vec<Vec<u8>> {
    parts.iter().map(|part| part.as_bytes().to_vec()).collect()
}

async fn inspect(
    session: &RedisSession,
    operation: u64,
    key: impl Into<Vec<u8>>,
) -> Result<dbotter::model::RedisValuePreview, DriverError> {
    session
        .inspect_key(&RedisKeyInspectRequest {
            identity: identity("redis-live-direct", operation),
            key: RedisKeyId(key.into()),
            timeout: TIMEOUT,
        })
        .await
}

async fn seed_representative_dataset(session: &RedisSession) -> Vec<u8> {
    execute(session, 1, argv(&["FLUSHDB"])).await;

    let mut mset = vec![b"MSET".to_vec()];
    for index in 0..160 {
        mset.push(format!("p5:scan:{index:03}").into_bytes());
        mset.push(format!("value-{index:03}").into_bytes());
    }
    execute(session, 2, mset).await;

    let binary_key = vec![b'p', b'5', b':', b'b', b'i', b'n', b':', 0xff];
    execute(
        session,
        3,
        vec![
            b"SET".to_vec(),
            binary_key.clone(),
            b"binary-value".to_vec(),
        ],
    )
    .await;
    let mut oversize_key = b"p5:oversize:".to_vec();
    oversize_key.extend(std::iter::repeat_n(
        b'x',
        MAX_REDIS_KEY_BYTES + 1 - oversize_key.len(),
    ));
    execute(
        session,
        4,
        vec![b"SET".to_vec(), oversize_key, b"not-selectable".to_vec()],
    )
    .await;

    execute(session, 5, argv(&["SET", "p5:string", "hello"])).await;
    execute(
        session,
        6,
        argv(&[
            "HSET", "p5:hash", "field-a", "value-a", "field-b", "value-b",
        ]),
    )
    .await;
    execute(
        session,
        7,
        argv(&["RPUSH", "p5:list", "alpha", "beta", "gamma"]),
    )
    .await;
    execute(
        session,
        8,
        argv(&["SADD", "p5:set", "alpha", "beta", "gamma"]),
    )
    .await;
    execute(
        session,
        9,
        argv(&["ZADD", "p5:zset", "1.25", "alpha", "2.5", "beta"]),
    )
    .await;
    execute(
        session,
        10,
        argv(&["XADD", "p5:stream", "*", "field", "value"]),
    )
    .await;
    execute(session, 11, argv(&["SET", "p5:ttl", "expires"])).await;
    execute(session, 12, argv(&["PEXPIRE", "p5:ttl", "120000"])).await;
    execute(
        session,
        13,
        argv(&[
            "EVAL",
            "return redis.call('SET', KEYS[1], string.rep('x', 70000))",
            "1",
            "p5:large-string",
        ]),
    )
    .await;
    binary_key
}

async fn assert_scan_and_inspection(session: &RedisSession, binary_key: &[u8]) {
    let filter = RedisKeyFilter::LiteralPrefix("p5:".to_owned());
    let mut accumulator = RedisScanAccumulator::new(filter.clone());
    let mut cursor = 0_u64;
    let mut pages = 0_usize;
    loop {
        pages += 1;
        let page = session
            .scan_keys(&RedisScanRequest {
                identity: identity("redis-live-direct", 100 + pages as u64),
                filter: filter.clone(),
                cursor,
                count_hint: 3,
                timeout: TIMEOUT,
            })
            .await
            .expect("one live SCAN page");
        cursor = page.next_cursor;
        accumulator.apply_page(page);
        if accumulator.is_complete() {
            break;
        }
        assert!(pages < 1_000, "SCAN cursor failed to converge");
    }
    assert!(pages > 1, "fixture must prove more than one SCAN page");
    assert!(
        accumulator
            .keys()
            .iter()
            .any(|entry| entry.id.as_bytes() == binary_key),
        "binary key identity must survive SCAN"
    );
    assert_eq!(accumulator.skipped_oversize(), 1);
    assert!(accumulator.truncated());
    assert!(accumulator.is_complete());

    let cases = [
        ("p5:string", RedisValueType::String, RedisTtl::Persistent),
        ("p5:hash", RedisValueType::Hash, RedisTtl::Persistent),
        ("p5:list", RedisValueType::List, RedisTtl::Persistent),
        ("p5:set", RedisValueType::Set, RedisTtl::Persistent),
        ("p5:zset", RedisValueType::SortedSet, RedisTtl::Persistent),
        ("p5:stream", RedisValueType::Stream, RedisTtl::Persistent),
    ];
    for (index, (key, expected_type, expected_ttl)) in cases.into_iter().enumerate() {
        let preview = inspect(session, 300 + index as u64, key.as_bytes().to_vec())
            .await
            .unwrap_or_else(|error| panic!("inspect {key}: {error:?}"));
        assert_eq!(preview.value_type, expected_type, "key={key}");
        assert_eq!(preview.ttl, expected_ttl, "key={key}");
        assert!(!preview.items.is_empty(), "key={key}");
        assert!(preview.retained_items <= 100, "key={key}");
        assert!(preview.retained_bytes <= 1024 * 1024, "key={key}");
    }

    let ttl = inspect(session, 400, b"p5:ttl".to_vec())
        .await
        .expect("expiring key");
    assert!(matches!(ttl.ttl, RedisTtl::ExpiresIn(value) if value > 0));

    let large = inspect(session, 401, b"p5:large-string".to_vec())
        .await
        .expect("large string");
    assert_eq!(large.value_type, RedisValueType::String);
    assert_eq!(large.size, Some(70_000));
    assert!(large.truncated);
    assert!(large.retained_bytes <= 64 * 1024);

    let binary = inspect(session, 402, binary_key.to_vec())
        .await
        .expect("binary key inspect");
    assert_eq!(binary.key.id.as_bytes(), binary_key);
    assert_eq!(binary.value_type, RedisValueType::String);

    assert!(matches!(
        inspect(session, 403, b"p5:missing".to_vec()).await,
        Err(DriverError::RedisKeyMissing)
    ));
    let oversize = RedisKeyInspectRequest {
        identity: identity("redis-live-direct", 404),
        key: RedisKeyId(vec![b'x'; MAX_REDIS_KEY_BYTES + 1]),
        timeout: TIMEOUT,
    };
    assert!(oversize.validate().is_err());

    execute(session, 405, argv(&["SET", "p5:mutation", "before"])).await;
    let before = inspect(session, 406, b"p5:mutation".to_vec())
        .await
        .expect("mutation before");
    assert!(matches!(&before.items[0], Cell::Text(value) if value == "before"));
    execute(session, 407, argv(&["SET", "p5:mutation", "after"])).await;
    let after = inspect(session, 408, b"p5:mutation".to_vec())
        .await
        .expect("mutation after");
    assert!(matches!(&after.items[0], Cell::Text(value) if value == "after"));
}

enum ResolverState {
    Available(String),
    Missing,
    Empty,
}

struct FixtureResolver(ResolverState);

impl SecretResolver for FixtureResolver {
    fn resolve_environment(&self, name: &str) -> Result<Arc<SessionSecret>, SecretError> {
        assert_eq!(name, ENV_NAME);
        match &self.0 {
            ResolverState::Available(value) => Ok(Arc::new(SessionSecret::new(value.clone()))),
            ResolverState::Missing => Err(SecretError::MissingEnv(name.to_owned())),
            ResolverState::Empty => Err(SecretError::EmptyEnv(name.to_owned())),
        }
    }

    fn probe_environment(&self, name: &str) -> EnvironmentAvailability {
        assert_eq!(name, ENV_NAME);
        match self.0 {
            ResolverState::Available(_) => EnvironmentAvailability::Available,
            ResolverState::Missing => EnvironmentAvailability::Missing,
            ResolverState::Empty => EnvironmentAvailability::Empty,
        }
    }
}

async fn service_check(
    profile: ConnectionProfile,
    resolver: ResolverState,
    session_password: Option<&str>,
) -> Result<(), ServiceError> {
    let directory = tempfile::tempdir().expect("live service tempdir");
    let path = directory.path().join("config.toml");
    fs::write(
        &path,
        toml::to_string(&Config {
            version: 2,
            profiles: vec![profile.clone()],
        })
        .expect("serialize live profile"),
    )
    .expect("write live profile");
    let store = Arc::new(SessionSecretStore::default());
    if let Some(password) = session_password {
        store
            .apply(
                &ProfileId(profile.id.clone()),
                SessionSecretUpdate::Replace(Arc::new(SessionSecret::new(password.to_owned()))),
            )
            .expect("install session fixture secret");
    }
    let service = ApplicationService::with_dependencies(
        &path,
        Arc::new(DriverConnector),
        Arc::new(FixtureResolver(resolver)),
        store,
        ConfigWriter::default(),
    )?;
    let profile_id = ProfileId(profile.id);
    let generation = service.profile_generation(&profile_id).await?;
    let result = service
        .check_at(OperationId(700), profile_id, generation, TIMEOUT)
        .await
        .map(|_| ());
    service.shutdown_runtime().await;
    result
}

async fn assert_auth_matrix(fixture: &LiveFixture) {
    for tls in [TlsMode::Disabled, TlsMode::Required] {
        let label = if tls == TlsMode::Required {
            "tls"
        } else {
            "plain"
        };

        service_check(
            fixture.profile(
                &format!("session-correct-{label}"),
                tls,
                CredentialMode::Session,
            ),
            ResolverState::Missing,
            Some(CORRECT_PASSWORD),
        )
        .await
        .unwrap_or_else(|error| panic!("Session correct/{label}: {error:?}"));
        let wrong_session = service_check(
            fixture.profile(
                &format!("session-wrong-{label}"),
                tls,
                CredentialMode::Session,
            ),
            ResolverState::Missing,
            Some(WRONG_PASSWORD),
        )
        .await
        .expect_err("wrong Session password must fail");
        assert_eq!(
            wrong_session.public_error_parts(),
            (
                PublicSummary::AuthenticationFailed,
                PublicCode::SessionCredential
            ),
            "Session wrong/{label}"
        );

        service_check(
            fixture.profile(
                &format!("env-correct-{label}"),
                tls,
                CredentialMode::Environment,
            ),
            ResolverState::Available(CORRECT_PASSWORD.to_owned()),
            None,
        )
        .await
        .unwrap_or_else(|error| panic!("Environment Available correct/{label}: {error:?}"));
        let wrong_environment = service_check(
            fixture.profile(
                &format!("env-wrong-{label}"),
                tls,
                CredentialMode::Environment,
            ),
            ResolverState::Available(WRONG_PASSWORD.to_owned()),
            None,
        )
        .await
        .expect_err("wrong Environment password must fail");
        assert_eq!(
            wrong_environment.public_error_parts(),
            (
                PublicSummary::AuthenticationFailed,
                PublicCode::CredentialEnvironmentName,
            ),
            "Environment Available wrong/{label}"
        );

        for (state, state_label) in [
            (ResolverState::Missing, "Missing"),
            (ResolverState::Empty, "Empty"),
        ] {
            let error = service_check(
                fixture.profile(
                    &format!("env-{}-{label}", state_label.to_ascii_lowercase()),
                    tls,
                    CredentialMode::Environment,
                ),
                state,
                None,
            )
            .await
            .expect_err("unavailable environment credential must fail");
            assert_eq!(
                error.public_error_parts(),
                (
                    PublicSummary::AuthenticationFailed,
                    PublicCode::CredentialEnvironmentName,
                ),
                "Environment {state_label}/{label}"
            );
        }
    }
}

async fn assert_tls_verification_and_no_fallback(fixture: &LiveFixture) {
    reset_transport_attempt_counts();
    let secret = SecretString::from(CORRECT_PASSWORD.to_owned());

    let mut wrong_ca = fixture.profile("tls-wrong-ca", TlsMode::Required, CredentialMode::Session);
    wrong_ca.redis_tls.ca_file = Some(fixture.wrong_ca_file.clone());
    match RedisSession::connect(&wrong_ca, Some(&secret), TIMEOUT).await {
        Err(DriverError::RedisTls {
            failure: RedisTlsFailure::CaUntrusted,
        }) => {}
        Ok(_) => panic!("wrong CA unexpectedly connected"),
        Err(error) => panic!("wrong CA classification: {error:?}"),
    }

    let mut wrong_host =
        fixture.profile("tls-wrong-host", TlsMode::Required, CredentialMode::Session);
    wrong_host.host = "127.0.0.1".to_owned();
    assert!(matches!(
        RedisSession::connect(&wrong_host, Some(&secret), TIMEOUT).await,
        Err(DriverError::RedisTls {
            failure: RedisTlsFailure::HostnameMismatch
        })
    ));

    wrong_host.host = fixture.tls_host.clone();
    RedisSession::connect(&wrong_host, Some(&secret), TIMEOUT)
        .await
        .expect("same CA succeeds when only the host is corrected");

    let attempts = transport_attempt_counts();
    assert_eq!(
        attempts.plaintext, 0,
        "Required must never attempt plaintext"
    );
    assert_eq!(
        attempts.required_tls, 3,
        "all three attempts remain TLS-only"
    );
}

fn assert_cli_round_trip(fixture: &LiveFixture, binary_key: &[u8]) {
    let directory = tempfile::tempdir().expect("CLI config tempdir");
    let path = directory.path().join("config.toml");
    let profile = fixture.profile(
        "redis-live-cli",
        TlsMode::Disabled,
        CredentialMode::Environment,
    );
    fs::write(
        &path,
        toml::to_string(&Config {
            version: 2,
            profiles: vec![profile],
        })
        .expect("serialize CLI profile"),
    )
    .expect("write CLI profile");

    let expected_base64 = base64::engine::general_purpose::STANDARD.encode(binary_key);
    let mut cursor = 0_u64;
    let mut found = false;
    for _ in 0..1_000 {
        let output = Command::new(env!("CARGO_BIN_EXE_dbotter"))
            .arg("--config")
            .arg(&path)
            .args([
                "browse",
                "redis",
                "keys",
                "--profile",
                "redis-live-cli",
                "--filter-mode",
                "literal-prefix",
                "--filter",
                "p5:",
                "--count",
                "3",
                "--cursor",
                &cursor.to_string(),
            ])
            .output()
            .expect("run dbotter browse CLI");
        assert_cli_success(&output, "browse");
        let page: serde_json::Value = serde_json::from_slice(&output.stdout).expect("browse JSON");
        found |= page["keys"]
            .as_array()
            .expect("keys array")
            .iter()
            .any(|entry| entry["key_base64"] == expected_base64);
        cursor = page["next_cursor"].as_u64().expect("next cursor");
        if cursor == 0 {
            break;
        }
    }
    assert!(
        found,
        "headless browse must expose the binary key as base64"
    );

    let output = Command::new(env!("CARGO_BIN_EXE_dbotter"))
        .arg("--config")
        .arg(&path)
        .args([
            "inspect",
            "redis",
            "key",
            "--profile",
            "redis-live-cli",
            "--key-base64",
            &expected_base64,
        ])
        .output()
        .expect("run dbotter inspect CLI");
    assert_cli_success(&output, "inspect");
    let preview: serde_json::Value = serde_json::from_slice(&output.stdout).expect("inspect JSON");
    assert_eq!(preview["key"]["key_base64"], expected_base64);
    assert_eq!(preview["value_type"], "string");
    assert_eq!(preview["ttl"]["state"], "persistent");
}

fn assert_cli_success(output: &std::process::Output, operation: &str) {
    assert!(
        output.status.success(),
        "CLI {operation} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires scripts/verify-live-redis.sh Docker fixture"]
async fn redis_live_receipt() {
    let fixture = LiveFixture::required();
    let secret = SecretString::from(CORRECT_PASSWORD.to_owned());
    let profile = fixture.profile(
        "redis-live-direct",
        TlsMode::Disabled,
        CredentialMode::Session,
    );
    let session = RedisSession::connect(&profile, Some(&secret), TIMEOUT)
        .await
        .expect("authenticated plaintext Redis fixture");
    let binary_key = seed_representative_dataset(&session).await;
    assert_scan_and_inspection(&session, &binary_key).await;

    assert_auth_matrix(&fixture).await;
    assert_tls_verification_and_no_fallback(&fixture).await;
    assert_cli_round_trip(&fixture, &binary_key);

    execute(&session, 999, argv(&["FLUSHDB"])).await;
}

#[test]
fn live_receipt_source_requires_all_frozen_proof_dimensions() {
    let source = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(file!()))
        .expect("live receipt source");
    for required in [
        "pages > 1",
        "binary key identity",
        "skipped_oversize",
        "RedisValueType::Hash",
        "RedisValueType::List",
        "RedisValueType::Set",
        "RedisValueType::SortedSet",
        "RedisValueType::Stream",
        "RedisTtl::ExpiresIn",
        "p5:large-string",
        "p5:mutation",
        "Session correct",
        "Environment Available",
        "Environment {state_label}",
        "RedisTlsFailure::CaUntrusted",
        "RedisTlsFailure::HostnameMismatch",
        "attempts.plaintext, 0",
        "CARGO_BIN_EXE_dbotter",
    ] {
        assert!(source.contains(required), "missing live proof: {required}");
    }
}
