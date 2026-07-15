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

#[path = "common/live_evidence.rs"]
mod live_evidence;

use base64::Engine as _;
use dbotter::config::{Config, ConfigWriter};
use dbotter::drivers::DriverError;
use dbotter::drivers::redis::{
    RedisSession, reset_transport_attempt_counts, transport_attempt_counts,
};
use dbotter::drivers::redis_browser::RedisScanAccumulator;
use dbotter::execution::{ExecutionLanguage, ExecutionTargetError, extract_and_validate_target};
use dbotter::model::{
    Cell, ConnectionProfile, CredentialMode, DriverKind, MAX_REDIS_KEY_BYTES, OperationId,
    OperationKind, ProfileFieldId, ProfileGeneration, ProfileId, PublicCode, PublicSummary,
    RedisExecuteRequest, RedisKeyFilter, RedisKeyId, RedisKeyInspectRequest, RedisScanRequest,
    RedisTlsConfig, RedisTtl, RedisValueType, RequestIdentity, TlsMode,
};
use dbotter::public_error::{RecoveryAction, SafeContext, recovery_for};
use dbotter::secrets::{
    EnvironmentAvailability, SecretError, SessionSecret, SessionSecretStore, SessionSecretUpdate,
};
use dbotter::service::{ApplicationService, DriverConnector, SecretResolver, ServiceError};
use live_evidence::LiveEvidence;
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

struct RedisMeasurements {
    scan_pages: usize,
    inspect_types: usize,
    mutation_readbacks: usize,
}

async fn assert_scan_and_inspection(
    session: &RedisSession,
    binary_key: &[u8],
    evidence: &mut LiveEvidence,
) -> RedisMeasurements {
    let multiple_pages = evidence.begin("redis.scan.multiple_pages");
    let raw_binary_identity = evidence.begin("redis.scan.raw_binary_identity");
    let oversize_skipped = evidence.begin("redis.scan.oversize_skipped");
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
    evidence.pass(multiple_pages);
    assert!(
        accumulator
            .keys()
            .iter()
            .any(|entry| entry.id.as_bytes() == binary_key),
        "binary key identity must survive SCAN"
    );
    evidence.pass(raw_binary_identity);
    assert_eq!(accumulator.skipped_oversize(), 1);
    assert!(accumulator.truncated());
    assert!(accumulator.is_complete());
    evidence.pass(oversize_skipped);

    let cases = [
        (
            "p5:string",
            RedisValueType::String,
            "redis.inspect.type.string",
        ),
        ("p5:hash", RedisValueType::Hash, "redis.inspect.type.hash"),
        ("p5:list", RedisValueType::List, "redis.inspect.type.list"),
        ("p5:set", RedisValueType::Set, "redis.inspect.type.set"),
        (
            "p5:zset",
            RedisValueType::SortedSet,
            "redis.inspect.type.zset",
        ),
        (
            "p5:stream",
            RedisValueType::Stream,
            "redis.inspect.type.stream",
        ),
    ];
    let persistent_ttl = evidence.begin("redis.inspect.ttl.persistent");
    let mut inspect_types = 0_usize;
    for (index, (key, expected_type, case_id)) in cases.into_iter().enumerate() {
        let type_checkpoint = evidence.begin(case_id);
        let preview = inspect(session, 300 + index as u64, key.as_bytes().to_vec())
            .await
            .unwrap_or_else(|error| panic!("inspect {key}: {error:?}"));
        assert_eq!(preview.value_type, expected_type, "key={key}");
        assert_eq!(preview.ttl, RedisTtl::Persistent, "key={key}");
        assert!(!preview.items.is_empty(), "key={key}");
        assert!(preview.retained_items <= 100, "key={key}");
        assert!(preview.retained_bytes <= 1024 * 1024, "key={key}");
        inspect_types += 1;
        evidence.pass(type_checkpoint);
    }
    evidence.pass(persistent_ttl);

    let expiring_ttl = evidence.begin("redis.inspect.ttl.expiring");
    let ttl = inspect(session, 400, b"p5:ttl".to_vec())
        .await
        .expect("expiring key");
    assert!(matches!(ttl.ttl, RedisTtl::ExpiresIn(value) if value > 0));
    evidence.pass(expiring_ttl);

    let truncation = evidence.begin("redis.inspect.truncation_64kib");
    let large = inspect(session, 401, b"p5:large-string".to_vec())
        .await
        .expect("large string");
    assert_eq!(large.value_type, RedisValueType::String);
    assert_eq!(large.size, Some(70_000));
    assert!(large.truncated);
    assert!(large.retained_bytes <= 64 * 1024);
    evidence.pass(truncation);

    let binary = inspect(session, 402, binary_key.to_vec())
        .await
        .expect("binary key inspect");
    assert_eq!(binary.key.id.as_bytes(), binary_key);
    assert_eq!(binary.value_type, RedisValueType::String);

    let missing_ttl = evidence.begin("redis.inspect.ttl.missing");
    assert!(matches!(
        inspect(session, 403, b"p5:missing".to_vec()).await,
        Err(DriverError::RedisKeyMissing)
    ));
    evidence.pass(missing_ttl);
    let oversize = RedisKeyInspectRequest {
        identity: identity("redis-live-direct", 404),
        key: RedisKeyId(vec![b'x'; MAX_REDIS_KEY_BYTES + 1]),
        timeout: TIMEOUT,
    };
    assert!(oversize.validate().is_err());

    let mutation_readback = evidence.begin("redis.mutation.readback");
    let mut mutation_readbacks = 0_usize;
    execute(session, 405, argv(&["SET", "p5:mutation", "before"])).await;
    let before = inspect(session, 406, b"p5:mutation".to_vec())
        .await
        .expect("mutation before");
    assert!(matches!(&before.items[0], Cell::Text(value) if value == "before"));
    mutation_readbacks += 1;
    execute(session, 407, argv(&["SET", "p5:mutation", "after"])).await;
    let after = inspect(session, 408, b"p5:mutation".to_vec())
        .await
        .expect("mutation after");
    assert!(matches!(&after.items[0], Cell::Text(value) if value == "after"));
    mutation_readbacks += 1;
    evidence.pass(mutation_readback);
    RedisMeasurements {
        scan_pages: pages,
        inspect_types,
        mutation_readbacks,
    }
}

fn assert_classifier_without_command(evidence: &mut LiveEvidence) {
    reset_transport_attempt_counts();
    let classifier = evidence.begin("redis.classifier.no_command");
    let rejected = extract_and_validate_target(
        "SUBSCRIBE measured-channel",
        0,
        None,
        ExecutionLanguage::Redis,
        100,
        5,
    )
    .expect_err("blocking Redis family must be rejected before session acquisition");
    assert_eq!(rejected, ExecutionTargetError::RedisCommandDenied);
    let attempts = transport_attempt_counts();
    assert_eq!(attempts.plaintext, 0);
    assert_eq!(attempts.required_tls, 0);
    evidence.pass(classifier);
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

struct UnavailableAuthCaseIds {
    failure: &'static str,
    code: &'static str,
    action: &'static str,
    recovery: &'static str,
}

struct AuthCaseIds {
    session_correct: &'static str,
    session_wrong: &'static str,
    session_wrong_code: &'static str,
    session_wrong_action: &'static str,
    session_wrong_recovery: &'static str,
    environment_correct: &'static str,
    environment_wrong: &'static str,
    environment_wrong_code: &'static str,
    environment_wrong_action: &'static str,
    environment_wrong_recovery: &'static str,
    missing: UnavailableAuthCaseIds,
    empty: UnavailableAuthCaseIds,
}

const PLAINTEXT_AUTH: AuthCaseIds = AuthCaseIds {
    session_correct: "redis.auth.plaintext.session.correct",
    session_wrong: "redis.auth.plaintext.session.wrong",
    session_wrong_code: "redis.auth.plaintext.session.wrong.code",
    session_wrong_action: "redis.auth.plaintext.session.wrong.action",
    session_wrong_recovery: "redis.auth.plaintext.session.wrong.recovery",
    environment_correct: "redis.auth.plaintext.environment.available.correct",
    environment_wrong: "redis.auth.plaintext.environment.available.wrong",
    environment_wrong_code: "redis.auth.plaintext.environment.available.wrong.code",
    environment_wrong_action: "redis.auth.plaintext.environment.available.wrong.action",
    environment_wrong_recovery: "redis.auth.plaintext.environment.available.wrong.recovery",
    missing: UnavailableAuthCaseIds {
        failure: "redis.auth.plaintext.environment.missing",
        code: "redis.auth.plaintext.environment.missing.code",
        action: "redis.auth.plaintext.environment.missing.action",
        recovery: "redis.auth.plaintext.environment.missing.recovery",
    },
    empty: UnavailableAuthCaseIds {
        failure: "redis.auth.plaintext.environment.empty",
        code: "redis.auth.plaintext.environment.empty.code",
        action: "redis.auth.plaintext.environment.empty.action",
        recovery: "redis.auth.plaintext.environment.empty.recovery",
    },
};

const TLS_AUTH: AuthCaseIds = AuthCaseIds {
    session_correct: "redis.auth.tls.session.correct",
    session_wrong: "redis.auth.tls.session.wrong",
    session_wrong_code: "redis.auth.tls.session.wrong.code",
    session_wrong_action: "redis.auth.tls.session.wrong.action",
    session_wrong_recovery: "redis.auth.tls.session.wrong.recovery",
    environment_correct: "redis.auth.tls.environment.available.correct",
    environment_wrong: "redis.auth.tls.environment.available.wrong",
    environment_wrong_code: "redis.auth.tls.environment.available.wrong.code",
    environment_wrong_action: "redis.auth.tls.environment.available.wrong.action",
    environment_wrong_recovery: "redis.auth.tls.environment.available.wrong.recovery",
    missing: UnavailableAuthCaseIds {
        failure: "redis.auth.tls.environment.missing",
        code: "redis.auth.tls.environment.missing.code",
        action: "redis.auth.tls.environment.missing.action",
        recovery: "redis.auth.tls.environment.missing.recovery",
    },
    empty: UnavailableAuthCaseIds {
        failure: "redis.auth.tls.environment.empty",
        code: "redis.auth.tls.environment.empty.code",
        action: "redis.auth.tls.environment.empty.action",
        recovery: "redis.auth.tls.environment.empty.recovery",
    },
};

fn assert_auth_code(error: &ServiceError, code: PublicCode) {
    assert_eq!(
        error.public_error_parts(),
        (PublicSummary::AuthenticationFailed, code)
    );
}

fn assert_auth_action(error: &ServiceError, profile_id: &str, expected: RecoveryAction) {
    let actions = recovery_for(
        OperationKind::ConnectProfile,
        error.public_summary(),
        error.public_code(),
        &SafeContext::profile(ProfileId(profile_id.to_owned()), OperationId(700)),
    )
    .expect("typed Redis Authentication recovery");
    assert_eq!(actions.as_slice(), &[expected]);
}

async fn unavailable_environment_case(
    fixture: &LiveFixture,
    evidence: &mut LiveEvidence,
    tls: TlsMode,
    label: &str,
    state: ResolverState,
    ids: &UnavailableAuthCaseIds,
) -> usize {
    let profile_id = format!("environment-{label}");
    let failure = evidence.begin(ids.failure);
    let error = service_check(
        fixture.profile(&profile_id, tls, CredentialMode::Environment),
        state,
        None,
    )
    .await
    .expect_err("unavailable Redis Environment credential must fail");
    let auth_failures = 1;
    evidence.pass(failure);

    let code = evidence.begin(ids.code);
    assert_auth_code(&error, PublicCode::CredentialEnvironmentName);
    evidence.pass(code);

    let action = evidence.begin(ids.action);
    assert_auth_action(
        &error,
        &profile_id,
        RecoveryAction::EditProfile(
            ProfileId(profile_id.clone()),
            ProfileFieldId::CredentialEnvironmentName,
        ),
    );
    evidence.pass(action);

    let recovery = evidence.begin(ids.recovery);
    service_check(
        fixture.profile(
            &format!("{profile_id}-recovered"),
            tls,
            CredentialMode::Environment,
        ),
        ResolverState::Available(CORRECT_PASSWORD.to_owned()),
        None,
    )
    .await
    .expect("Redis unavailable Environment recovery");
    evidence.pass(recovery);
    auth_failures
}

async fn assert_auth_matrix(fixture: &LiveFixture, evidence: &mut LiveEvidence) -> usize {
    let mut auth_failures = 0_usize;
    for (tls, label, ids) in [
        (TlsMode::Disabled, "plaintext", &PLAINTEXT_AUTH),
        (TlsMode::Required, "tls", &TLS_AUTH),
    ] {
        let session_correct = evidence.begin(ids.session_correct);
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
        evidence.pass(session_correct);

        let session_profile_id = format!("session-wrong-{label}");
        let session_wrong = evidence.begin(ids.session_wrong);
        let wrong_session = service_check(
            fixture.profile(&session_profile_id, tls, CredentialMode::Session),
            ResolverState::Missing,
            Some(WRONG_PASSWORD),
        )
        .await
        .expect_err("wrong Redis Session password must fail");
        auth_failures += 1;
        evidence.pass(session_wrong);
        let session_code = evidence.begin(ids.session_wrong_code);
        assert_auth_code(&wrong_session, PublicCode::SessionCredential);
        evidence.pass(session_code);
        let session_action = evidence.begin(ids.session_wrong_action);
        assert_auth_action(
            &wrong_session,
            &session_profile_id,
            RecoveryAction::OpenCredentialPrompt(ProfileId(session_profile_id.clone())),
        );
        evidence.pass(session_action);
        let session_recovery = evidence.begin(ids.session_wrong_recovery);
        service_check(
            fixture.profile(
                &format!("session-recovered-{label}"),
                tls,
                CredentialMode::Session,
            ),
            ResolverState::Missing,
            Some(CORRECT_PASSWORD),
        )
        .await
        .expect("Redis Session recovery");
        evidence.pass(session_recovery);

        let environment_correct = evidence.begin(ids.environment_correct);
        service_check(
            fixture.profile(
                &format!("environment-correct-{label}"),
                tls,
                CredentialMode::Environment,
            ),
            ResolverState::Available(CORRECT_PASSWORD.to_owned()),
            None,
        )
        .await
        .unwrap_or_else(|error| panic!("Environment Available correct/{label}: {error:?}"));
        evidence.pass(environment_correct);

        let environment_profile_id = format!("environment-wrong-{label}");
        let environment_wrong = evidence.begin(ids.environment_wrong);
        let wrong_environment = service_check(
            fixture.profile(&environment_profile_id, tls, CredentialMode::Environment),
            ResolverState::Available(WRONG_PASSWORD.to_owned()),
            None,
        )
        .await
        .expect_err("wrong Redis Environment password must fail");
        auth_failures += 1;
        evidence.pass(environment_wrong);
        let environment_code = evidence.begin(ids.environment_wrong_code);
        assert_auth_code(&wrong_environment, PublicCode::CredentialEnvironmentName);
        evidence.pass(environment_code);
        let environment_action = evidence.begin(ids.environment_wrong_action);
        assert_auth_action(
            &wrong_environment,
            &environment_profile_id,
            RecoveryAction::EditProfile(
                ProfileId(environment_profile_id.clone()),
                ProfileFieldId::CredentialEnvironmentName,
            ),
        );
        evidence.pass(environment_action);
        let environment_recovery = evidence.begin(ids.environment_wrong_recovery);
        service_check(
            fixture.profile(
                &format!("environment-recovered-{label}"),
                tls,
                CredentialMode::Environment,
            ),
            ResolverState::Available(CORRECT_PASSWORD.to_owned()),
            None,
        )
        .await
        .expect("Redis Environment wrong recovery");
        evidence.pass(environment_recovery);

        auth_failures += unavailable_environment_case(
            fixture,
            evidence,
            tls,
            &format!("missing-{label}"),
            ResolverState::Missing,
            &ids.missing,
        )
        .await;
        auth_failures += unavailable_environment_case(
            fixture,
            evidence,
            tls,
            &format!("empty-{label}"),
            ResolverState::Empty,
            &ids.empty,
        )
        .await;
    }
    auth_failures
}

async fn assert_tls_verification_and_no_fallback(
    fixture: &LiveFixture,
    evidence: &mut LiveEvidence,
) -> (usize, usize, usize) {
    reset_transport_attempt_counts();

    let wrong_ca_code = evidence.begin("redis.tls.wrong_ca.code");
    let mut wrong_ca = fixture.profile("tls-wrong-ca", TlsMode::Required, CredentialMode::Session);
    wrong_ca.redis_tls.ca_file = Some(fixture.wrong_ca_file.clone());
    let wrong_ca_error = service_check(wrong_ca, ResolverState::Missing, Some(CORRECT_PASSWORD))
        .await
        .expect_err("wrong CA must fail");
    assert_eq!(
        wrong_ca_error.public_error_parts(),
        (
            PublicSummary::TlsVerificationFailed,
            PublicCode::RedisTlsCaUntrustedIssuer,
        )
    );
    evidence.pass(wrong_ca_code);
    let wrong_ca_action = evidence.begin("redis.tls.wrong_ca.action");
    let wrong_ca_recovery = recovery_for(
        OperationKind::ConnectProfile,
        wrong_ca_error.public_summary(),
        wrong_ca_error.public_code(),
        &SafeContext::profile(ProfileId("tls-wrong-ca".to_owned()), OperationId(700)),
    )
    .expect("wrong CA recovery");
    assert_eq!(
        wrong_ca_recovery.as_slice(),
        &[RecoveryAction::EditProfile(
            ProfileId("tls-wrong-ca".to_owned()),
            ProfileFieldId::RedisCaFile,
        )]
    );
    evidence.pass(wrong_ca_action);
    let wrong_ca_focus = evidence.begin("redis.tls.wrong_ca.focus_ca");
    assert_eq!(
        ProfileFieldId::RedisCaFile.focus_id(),
        "profile.redis_tls.ca_file"
    );
    evidence.pass(wrong_ca_focus);

    let wrong_host_code = evidence.begin("redis.tls.wrong_host.code");
    let mut wrong_host =
        fixture.profile("tls-wrong-host", TlsMode::Required, CredentialMode::Session);
    let original_ca = wrong_host.redis_tls.ca_file.clone();
    wrong_host.host = "127.0.0.1".to_owned();
    let wrong_host_error = service_check(
        wrong_host.clone(),
        ResolverState::Missing,
        Some(CORRECT_PASSWORD),
    )
    .await
    .expect_err("wrong host must fail");
    assert_eq!(
        wrong_host_error.public_error_parts(),
        (
            PublicSummary::TlsVerificationFailed,
            PublicCode::TlsHostnameMismatch,
        )
    );
    evidence.pass(wrong_host_code);
    let wrong_host_action = evidence.begin("redis.tls.wrong_host.action");
    let wrong_host_recovery = recovery_for(
        OperationKind::ConnectProfile,
        wrong_host_error.public_summary(),
        wrong_host_error.public_code(),
        &SafeContext::profile(ProfileId("tls-wrong-host".to_owned()), OperationId(700)),
    )
    .expect("wrong host recovery");
    assert_eq!(
        wrong_host_recovery.as_slice(),
        &[RecoveryAction::EditProfile(
            ProfileId("tls-wrong-host".to_owned()),
            ProfileFieldId::Host,
        )]
    );
    evidence.pass(wrong_host_action);
    let wrong_host_focus = evidence.begin("redis.tls.wrong_host.focus_host");
    assert_eq!(ProfileFieldId::Host.focus_id(), "profile.host");
    evidence.pass(wrong_host_focus);

    let ca_preserved = evidence.begin("redis.tls.ca_preserved");
    let host_recovery = evidence.begin("redis.tls.host_recovery");
    wrong_host.host = fixture.tls_host.clone();
    assert_eq!(wrong_host.redis_tls.ca_file, original_ca);
    service_check(wrong_host, ResolverState::Missing, Some(CORRECT_PASSWORD))
        .await
        .expect("same CA succeeds when only the host is corrected");
    let tls_recovery_attempts = 1_usize;
    evidence.pass(ca_preserved);
    evidence.pass(host_recovery);

    let attempts = transport_attempt_counts();
    assert_eq!(
        attempts.plaintext, 0,
        "Required must never attempt plaintext"
    );
    assert_eq!(
        attempts.required_tls, 3,
        "all three attempts remain TLS-only"
    );
    (
        usize::try_from(attempts.plaintext).expect("plaintext attempt count"),
        usize::try_from(attempts.required_tls).expect("TLS attempt count"),
        tls_recovery_attempts,
    )
}

fn assert_cli_round_trip(
    fixture: &LiveFixture,
    binary_key: &[u8],
    evidence: &mut LiveEvidence,
) -> usize {
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
    let mut cli_operations = 0_usize;
    let cli_browse = evidence.begin("redis.cli.browse");
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
    cli_operations += 1;
    evidence.pass(cli_browse);

    let cli_inspect = evidence.begin("redis.cli.inspect");
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
    cli_operations += 1;
    evidence.pass(cli_inspect);
    cli_operations
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
    let mut evidence =
        LiveEvidence::required("redis", "redis_live_receipt", "DBOTTER_LIVE_REDIS_EVIDENCE")
            .expect("initialize Redis evidence");
    let fixture = LiveFixture::required();
    assert_classifier_without_command(&mut evidence);
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
    let measurements = assert_scan_and_inspection(&session, &binary_key, &mut evidence).await;

    let auth_failures = assert_auth_matrix(&fixture, &mut evidence).await;
    let (plaintext_fallback_attempts, required_tls_attempts, tls_recovery_attempts) =
        assert_tls_verification_and_no_fallback(&fixture, &mut evidence).await;
    let cli_operations = assert_cli_round_trip(&fixture, &binary_key, &mut evidence);

    execute(&session, 999, argv(&["FLUSHDB"])).await;
    evidence
        .measure("auth_failures", auth_failures)
        .expect("Redis auth failure count");
    evidence
        .measure("cli_operations", cli_operations)
        .expect("Redis CLI operation count");
    evidence
        .measure("inspect_types", measurements.inspect_types)
        .expect("Redis inspect type count");
    evidence
        .measure("mutation_readbacks", measurements.mutation_readbacks)
        .expect("Redis mutation readback count");
    evidence
        .measure("plaintext_fallback_attempts", plaintext_fallback_attempts)
        .expect("Redis plaintext fallback count");
    evidence
        .measure("required_tls_attempts", required_tls_attempts)
        .expect("Redis required TLS count");
    evidence
        .measure("scan_pages", measurements.scan_pages)
        .expect("Redis scan page count");
    evidence
        .measure("tls_recovery_attempts", tls_recovery_attempts)
        .expect("Redis TLS recovery count");
    evidence.finish().expect("publish Redis evidence");
}

#[test]
fn live_receipt_source_requires_all_frozen_proof_dimensions() {
    let source = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(file!()))
        .expect("live receipt source");
    let implementation = source
        .split("#[test]\nfn live_receipt_source_requires_all_frozen_proof_dimensions()")
        .next()
        .expect("live receipt implementation section");
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
        "ids.session_correct",
        "ids.environment_correct",
        "UnavailableAuthCaseIds",
        "PublicCode::RedisTlsCaUntrustedIssuer",
        "PublicCode::TlsHostnameMismatch",
        "ProfileFieldId::RedisCaFile.focus_id()",
        "ProfileFieldId::Host.focus_id()",
        "wrong_host.redis_tls.ca_file, original_ca",
        "attempts.plaintext, 0",
        "CARGO_BIN_EXE_dbotter",
        "evidence.finish()",
    ] {
        assert!(
            implementation.contains(required),
            "missing live proof: {required}"
        );
    }
}
