use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use dbotter::config::{
    CommitState, Config, ConfigError, ConfigMutation, ConfigSourceVersion, ConfigWriter,
    MigrationConsent, MutationFailpoint, MutationFaultInjector, load_path,
};
use dbotter::drivers::{
    CatalogBrowser, ConnectedResources, ConnectionPing, DriverError, KeyspaceBrowser,
    MySqlPreparedExecution, RedisExecution,
};
use dbotter::execution::{ExecutionLanguage, ExecutionTarget, classify_execution_kind};
use dbotter::model::{
    CatalogPage, CatalogRequest, ConnectionDraft, ConnectionProfile, CredentialMode, DraftId,
    DriverKind, ExecuteRequest, LegacyConfigVersion, MySqlPublicErrorCode, OperationId,
    OperationKind, PreparedMySqlRequest, ProfileAccess, ProfileEnvironment, ProfileFieldId,
    ProfileGeneration, ProfileId, ProfileInstanceId, ProfileSafetyPosture, PublicCode,
    PublicSummary, QueryLanguage, QueryResult, RedisExecuteRequest, RedisKeyInspectRequest,
    RedisKeyPage, RedisScanRequest, RedisValuePreview, TlsMode,
};
use dbotter::public_error::{RecoveryAction, SafeContext, recovery_for};
use dbotter::secrets::{
    EnvironmentAvailability, ReplacementSecretBuffer, SecretError, SessionSecret,
    SessionSecretStore, SessionSecretUpdate,
};
use dbotter::service::{
    ApplicationService, CheckOutcome, CreateProfileRequest, DeleteProfileRequest, ExecuteOutcome,
    ProfileMutationOutcome, ProfileValidationError, SecretResolver, ServiceError, SessionConnector,
    SessionDisposition, SessionHandle, UpdateProfileRequest, validate_config_identity,
    validate_config_mutation,
};

#[async_trait]
trait CurrentGenerationTestExt {
    async fn check(
        &self,
        operation_id: OperationId,
        profile_id: ProfileId,
        timeout: Duration,
    ) -> Result<CheckOutcome, ServiceError>;

    async fn execute(&self, request: ExecuteRequest) -> Result<ExecuteOutcome, ServiceError>;
}

#[test]
fn prepared_unsupported_retains_only_a_typed_proven_healthy_session() {
    assert_eq!(
        SessionDisposition::for_driver_error(&DriverError::PreparedStatementUnsupported {
            session_healthy: true,
        }),
        SessionDisposition::Keep
    );
    assert_eq!(
        SessionDisposition::for_driver_error(&DriverError::PreparedStatementUnsupported {
            session_healthy: false,
        }),
        SessionDisposition::Evict
    );
    let service_error = ServiceError::from(DriverError::PreparedStatementUnsupported {
        session_healthy: true,
    });
    assert_eq!(
        service_error.public_summary(),
        PublicSummary::UnsupportedFeature
    );
    assert_eq!(
        service_error.public_code(),
        PublicCode::PreparedStatementUnsupported
    );
}

#[tokio::test]
async fn mysql_authentication_failure_maps_to_the_exact_credential_source_and_action() {
    for (credential_mode, expected_code, expected_action) in [
        (
            CredentialMode::Session,
            PublicCode::SessionCredential,
            RecoveryAction::OpenCredentialPrompt(ProfileId("auth-session".to_owned())),
        ),
        (
            CredentialMode::Environment,
            PublicCode::CredentialEnvironmentName,
            RecoveryAction::EditProfile(
                ProfileId("auth-environment".to_owned()),
                ProfileFieldId::CredentialEnvironmentName,
            ),
        ),
    ] {
        let directory = tempfile::tempdir().expect("MySQL auth contract tempdir");
        let path = directory.path().join("config.toml");
        let profile_id = match credential_mode {
            CredentialMode::Session => ProfileId("auth-session".to_owned()),
            CredentialMode::Environment => ProfileId("auth-environment".to_owned()),
            CredentialMode::None => unreachable!(),
        };
        let mut profile = dbotter::model::ConnectionProfile::from_draft(
            profile_id.0.clone(),
            draft(DriverKind::MySql),
        );
        profile.credential_mode = credential_mode;
        profile.secret_env =
            (credential_mode == CredentialMode::Environment).then(|| "EXACT_ENV_NAME".to_owned());
        fs::write(
            &path,
            toml::to_string(&Config {
                version: 2,
                profiles: vec![profile],
            })
            .expect("serialize MySQL auth profile"),
        )
        .expect("write MySQL auth profile");
        let secrets = Arc::new(SessionSecretStore::default());
        if credential_mode == CredentialMode::Session {
            secrets
                .apply(
                    &profile_id,
                    SessionSecretUpdate::Replace(Arc::new(SessionSecret::new(
                        "contract-session-secret".to_owned(),
                    ))),
                )
                .expect("install contract Session secret");
        }
        let service = ApplicationService::with_dependencies(
            &path,
            Arc::new(MySqlAuthenticationFailureConnector),
            Arc::new(FixedEnvironment::default()),
            secrets,
            ConfigWriter::default(),
        )
        .expect("MySQL auth contract service");
        let generation = service
            .profile_generation(&profile_id)
            .await
            .expect("MySQL auth profile generation");
        let error = service
            .check_at(
                OperationId(69),
                profile_id.clone(),
                generation,
                Duration::from_secs(1),
            )
            .await
            .expect_err("injected MySQL authentication must fail");
        assert_eq!(
            error.public_error_parts(),
            (PublicSummary::AuthenticationFailed, expected_code)
        );
        assert_eq!(
            recovery_for(
                OperationKind::ConnectProfile,
                error.public_summary(),
                error.public_code(),
                &SafeContext::profile(profile_id, OperationId(69)),
            )
            .expect("MySQL authentication recovery")
            .as_slice(),
            &[expected_action]
        );
    }
}

#[test]
fn catalog_integrity_key_is_lazy_and_uses_the_internal_browse_error_row() {
    let directory = tempfile::tempdir().expect("catalog key lazy tempdir");
    let path = directory.path().join("config.toml");
    let original = b"version = 2\nprofiles = []\n";
    fs::write(&path, original).expect("catalog key lazy config");

    let service = ApplicationService::load_path(&path).expect("load service without key write");
    drop(service);
    assert_eq!(fs::read(&path).expect("unchanged config bytes"), original);
    assert_eq!(
        fs::read_dir(directory.path())
            .expect("lazy key directory")
            .filter_map(Result::ok)
            .count(),
        1,
        "ApplicationService construction must not create the sidecar"
    );

    let error = ServiceError::CatalogTokenKeyUnavailable;
    assert_eq!(
        error.public_error_parts(),
        (PublicSummary::InternalFailure, PublicCode::None)
    );
    assert_eq!(format!("{error:?}"), "CatalogTokenKeyUnavailable");
    assert_eq!(
        error.to_string(),
        "catalog token integrity key is unavailable"
    );
    assert!(
        recovery_for(
            OperationKind::BrowseMySql,
            PublicSummary::InternalFailure,
            PublicCode::None,
            &SafeContext::profile(ProfileId("catalog".to_owned()), OperationId(8)),
        )
        .is_ok()
    );
}

#[async_trait]
impl CurrentGenerationTestExt for ApplicationService {
    async fn check(
        &self,
        operation_id: OperationId,
        profile_id: ProfileId,
        timeout: Duration,
    ) -> Result<CheckOutcome, ServiceError> {
        let generation = self.profile_generation(&profile_id).await?;
        self.check_at(operation_id, profile_id, generation, timeout)
            .await
    }

    async fn execute(&self, request: ExecuteRequest) -> Result<ExecuteOutcome, ServiceError> {
        let generation = self.profile_generation(&request.profile_id).await?;
        let mut request = request;
        request.profile_generation = generation;
        self.execute_at(request).await
    }
}

#[derive(Default)]
struct FakeSession {
    pings: AtomicUsize,
    executes: Arc<AtomicUsize>,
    closes: AtomicUsize,
}

#[derive(Clone)]
struct FakeMySqlResources {
    executes: Arc<AtomicUsize>,
}

#[async_trait]
impl ConnectionPing for FakeMySqlResources {
    async fn ping(&self, _timeout: Duration) -> Result<(), DriverError> {
        Ok(())
    }
}

#[async_trait]
impl MySqlPreparedExecution for FakeMySqlResources {
    async fn execute_prepared(
        &self,
        _request: &PreparedMySqlRequest,
    ) -> Result<QueryResult, DriverError> {
        self.executes.fetch_add(1, Ordering::SeqCst);
        Ok(empty_result())
    }
}

#[async_trait]
impl CatalogBrowser for FakeMySqlResources {
    async fn load_page(
        &self,
        _request: &CatalogRequest,
        _token_key: &dbotter::drivers::mysql_catalog::CatalogTokenKey,
    ) -> Result<CatalogPage, DriverError> {
        Err(DriverError::Unsupported {
            driver: DriverKind::MySql,
            operation: "test catalog".to_owned(),
        })
    }
}

#[async_trait]
impl SessionHandle for FakeSession {
    async fn ping(&self, _timeout: Duration) -> Result<(), DriverError> {
        self.pings.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn connected_resources(&self) -> Option<ConnectedResources> {
        let resources = Arc::new(FakeMySqlResources {
            executes: self.executes.clone(),
        });
        Some(ConnectedResources::MySql {
            ping: resources.clone(),
            execution: resources.clone(),
            catalog: resources,
        })
    }

    async fn close(&self) -> Result<(), DriverError> {
        self.closes.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

struct FakeConnector {
    connects: AtomicUsize,
    saw_secret: AtomicBool,
    session: Arc<FakeSession>,
    redis_tls: bool,
}

struct MySqlAuthenticationFailureConnector;

#[async_trait]
impl SessionConnector for MySqlAuthenticationFailureConnector {
    async fn connect(
        &self,
        _profile: &dbotter::model::ConnectionProfile,
        _secret: Option<&SessionSecret>,
        _timeout: Duration,
    ) -> Result<Arc<dyn SessionHandle>, DriverError> {
        Err(DriverError::MySqlServer {
            code: MySqlPublicErrorCode::new(1045, "28000").expect("valid MySQL auth code"),
        })
    }
}

struct EndpointSecretConnector {
    calls: Mutex<Vec<(String, Option<usize>)>>,
    session: Arc<FakeSession>,
}

struct AsyncGate {
    entered: AtomicBool,
    entered_notify: tokio::sync::Notify,
    release_notify: tokio::sync::Notify,
}

impl AsyncGate {
    fn new() -> Self {
        Self {
            entered: AtomicBool::new(false),
            entered_notify: tokio::sync::Notify::new(),
            release_notify: tokio::sync::Notify::new(),
        }
    }

    async fn enter_and_wait(&self) {
        self.entered.store(true, Ordering::SeqCst);
        self.entered_notify.notify_waiters();
        self.release_notify.notified().await;
    }

    async fn wait_until_entered(&self) {
        while !self.entered.load(Ordering::SeqCst) {
            self.entered_notify.notified().await;
        }
    }

    fn release(&self) {
        self.release_notify.notify_one();
    }
}

struct ConnectBarrierConnector {
    gate: Arc<AsyncGate>,
    session: Arc<FakeSession>,
    connects: AtomicUsize,
}

impl ConnectBarrierConnector {
    fn new() -> Self {
        Self {
            gate: Arc::new(AsyncGate::new()),
            session: Arc::new(FakeSession::default()),
            connects: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl SessionConnector for ConnectBarrierConnector {
    async fn connect(
        &self,
        _profile: &dbotter::model::ConnectionProfile,
        _secret: Option<&SessionSecret>,
        _timeout: Duration,
    ) -> Result<Arc<dyn SessionHandle>, DriverError> {
        self.connects.fetch_add(1, Ordering::SeqCst);
        self.gate.enter_and_wait().await;
        Ok(self.session.clone())
    }
}

struct ControlledSession {
    ping_gate: Option<Arc<AsyncGate>>,
    ping_error: bool,
    pings: AtomicUsize,
    closes: AtomicUsize,
}

impl ControlledSession {
    fn immediate() -> Self {
        Self {
            ping_gate: None,
            ping_error: false,
            pings: AtomicUsize::new(0),
            closes: AtomicUsize::new(0),
        }
    }

    fn blocked_failure(gate: Arc<AsyncGate>) -> Self {
        Self {
            ping_gate: Some(gate),
            ping_error: true,
            pings: AtomicUsize::new(0),
            closes: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl SessionHandle for ControlledSession {
    async fn ping(&self, _timeout: Duration) -> Result<(), DriverError> {
        self.pings.fetch_add(1, Ordering::SeqCst);
        if let Some(gate) = &self.ping_gate {
            gate.enter_and_wait().await;
        }
        if self.ping_error {
            Err(DriverError::Unavailable {
                driver: DriverKind::MySql,
                reason: "injected ping failure",
            })
        } else {
            Ok(())
        }
    }

    async fn close(&self) -> Result<(), DriverError> {
        self.closes.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

struct SequencedConnector {
    sessions: Vec<Arc<ControlledSession>>,
    connects: AtomicUsize,
}

#[async_trait]
impl SessionConnector for SequencedConnector {
    async fn connect(
        &self,
        profile: &dbotter::model::ConnectionProfile,
        _secret: Option<&SessionSecret>,
        _timeout: Duration,
    ) -> Result<Arc<dyn SessionHandle>, DriverError> {
        let index = self.connects.fetch_add(1, Ordering::SeqCst);
        self.sessions
            .get(index)
            .cloned()
            .map(|session| session as Arc<dyn SessionHandle>)
            .ok_or_else(|| DriverError::Unavailable {
                driver: profile.driver,
                reason: "missing sequenced test session",
            })
    }
}

impl EndpointSecretConnector {
    fn new() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            session: Arc::new(FakeSession::default()),
        }
    }
}

#[async_trait]
impl SessionConnector for EndpointSecretConnector {
    async fn connect(
        &self,
        profile: &dbotter::model::ConnectionProfile,
        secret: Option<&SessionSecret>,
        _timeout: Duration,
    ) -> Result<Arc<dyn SessionHandle>, DriverError> {
        self.calls
            .lock()
            .map_err(|_| DriverError::InvalidConfig {
                driver: profile.driver,
                message: "test recorder unavailable".to_owned(),
            })?
            .push((
                profile.host.clone(),
                secret.map(|value| std::ptr::from_ref(value).addr()),
            ));
        Ok(self.session.clone())
    }
}

impl FakeConnector {
    fn new(redis_tls: bool) -> Self {
        Self {
            connects: AtomicUsize::new(0),
            saw_secret: AtomicBool::new(false),
            session: Arc::new(FakeSession::default()),
            redis_tls,
        }
    }
}

#[async_trait]
impl SessionConnector for FakeConnector {
    async fn connect(
        &self,
        _profile: &dbotter::model::ConnectionProfile,
        secret: Option<&SessionSecret>,
        _timeout: Duration,
    ) -> Result<Arc<dyn SessionHandle>, DriverError> {
        self.connects.fetch_add(1, Ordering::SeqCst);
        self.saw_secret.store(secret.is_some(), Ordering::SeqCst);
        Ok(self.session.clone())
    }

    fn supports_redis_tls(&self) -> bool {
        self.redis_tls
    }
}

#[derive(Default)]
struct AccessAdmissionIo {
    non_target_session_io: AtomicUsize,
    user_target_dispatches: AtomicUsize,
}

struct AccessAdmissionSession {
    driver: DriverKind,
    io: Arc<AccessAdmissionIo>,
}

#[derive(Clone)]
struct AccessAdmissionResources {
    driver: DriverKind,
    io: Arc<AccessAdmissionIo>,
}

#[async_trait]
impl ConnectionPing for AccessAdmissionResources {
    async fn ping(&self, _timeout: Duration) -> Result<(), DriverError> {
        self.io.non_target_session_io.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[async_trait]
impl MySqlPreparedExecution for AccessAdmissionResources {
    async fn execute_prepared(
        &self,
        _request: &PreparedMySqlRequest,
    ) -> Result<QueryResult, DriverError> {
        self.io
            .user_target_dispatches
            .fetch_add(1, Ordering::SeqCst);
        Ok(empty_result())
    }
}

#[async_trait]
impl RedisExecution for AccessAdmissionResources {
    async fn execute_command(
        &self,
        _request: &RedisExecuteRequest,
    ) -> Result<QueryResult, DriverError> {
        self.io
            .user_target_dispatches
            .fetch_add(1, Ordering::SeqCst);
        Ok(empty_result())
    }
}

#[async_trait]
impl CatalogBrowser for AccessAdmissionResources {
    async fn load_page(
        &self,
        _request: &CatalogRequest,
        _token_key: &dbotter::drivers::mysql_catalog::CatalogTokenKey,
    ) -> Result<CatalogPage, DriverError> {
        Err(DriverError::Unsupported {
            driver: self.driver,
            operation: "access-admission catalog".to_owned(),
        })
    }
}

#[async_trait]
impl KeyspaceBrowser for AccessAdmissionResources {
    async fn scan_keys(&self, _request: &RedisScanRequest) -> Result<RedisKeyPage, DriverError> {
        Err(DriverError::Unsupported {
            driver: self.driver,
            operation: "access-admission key scan".to_owned(),
        })
    }

    async fn inspect_key(
        &self,
        _request: &RedisKeyInspectRequest,
    ) -> Result<RedisValuePreview, DriverError> {
        Err(DriverError::Unsupported {
            driver: self.driver,
            operation: "access-admission key inspect".to_owned(),
        })
    }
}

#[async_trait]
impl SessionHandle for AccessAdmissionSession {
    async fn ping(&self, _timeout: Duration) -> Result<(), DriverError> {
        self.io.non_target_session_io.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn connected_resources(&self) -> Option<ConnectedResources> {
        let resources = Arc::new(AccessAdmissionResources {
            driver: self.driver,
            io: self.io.clone(),
        });
        match self.driver {
            DriverKind::MySql => Some(ConnectedResources::MySql {
                ping: resources.clone(),
                execution: resources.clone(),
                catalog: resources,
            }),
            DriverKind::Redis => Some(ConnectedResources::Redis {
                ping: resources.clone(),
                execution: resources.clone(),
                keyspace: resources,
            }),
            DriverKind::MongoDb => None,
        }
    }

    async fn close(&self) -> Result<(), DriverError> {
        Ok(())
    }
}

struct AccessAdmissionConnector {
    driver: DriverKind,
    io: Arc<AccessAdmissionIo>,
    saw_secret: AtomicBool,
}

impl AccessAdmissionConnector {
    fn new(driver: DriverKind) -> Self {
        Self {
            driver,
            io: Arc::new(AccessAdmissionIo::default()),
            saw_secret: AtomicBool::new(false),
        }
    }
}

#[async_trait]
impl SessionConnector for AccessAdmissionConnector {
    async fn connect(
        &self,
        profile: &dbotter::model::ConnectionProfile,
        secret: Option<&SessionSecret>,
        _timeout: Duration,
    ) -> Result<Arc<dyn SessionHandle>, DriverError> {
        if profile.driver != self.driver {
            return Err(DriverError::InvalidConfig {
                driver: profile.driver,
                message: "access-admission driver mismatch".to_owned(),
            });
        }
        self.io.non_target_session_io.fetch_add(1, Ordering::SeqCst);
        self.saw_secret.store(secret.is_some(), Ordering::SeqCst);
        Ok(Arc::new(AccessAdmissionSession {
            driver: self.driver,
            io: self.io.clone(),
        }))
    }
}

#[derive(Default)]
struct MissingEnvironment;

impl SecretResolver for MissingEnvironment {
    fn resolve_environment(&self, name: &str) -> Result<Arc<SessionSecret>, SecretError> {
        Err(SecretError::MissingEnv(name.to_owned()))
    }

    fn probe_environment(&self, _name: &str) -> EnvironmentAvailability {
        EnvironmentAvailability::Missing
    }
}

#[derive(Default)]
struct FixedEnvironment {
    resolutions: AtomicUsize,
}

impl SecretResolver for FixedEnvironment {
    fn resolve_environment(&self, name: &str) -> Result<Arc<SessionSecret>, SecretError> {
        if name != "EXACT_ENV_NAME" {
            return Err(SecretError::MissingEnv(name.to_owned()));
        }
        self.resolutions.fetch_add(1, Ordering::SeqCst);
        Ok(Arc::new(SessionSecret::new(
            "resolved-environment-secret".to_owned(),
        )))
    }

    fn probe_environment(&self, name: &str) -> EnvironmentAvailability {
        if name == "EXACT_ENV_NAME" {
            EnvironmentAvailability::Available
        } else {
            EnvironmentAvailability::Missing
        }
    }
}

const DU01_ENDPOINT_SENTINEL: &str = "du01-endpoint-sentinel.invalid";
const DU01_CREDENTIAL_SENTINEL: &str = "du01-credential-sentinel";
const DU01_POSTURE_SENTINEL: &str = "DU01 posture sentinel";
const DU01_MYSQL_READ: &str = "SELECT 7";
const DU01_MYSQL_MUTATION: &str = "UPDATE widgets SET value = 1 WHERE id = 7 LIMIT 1";
const DU01_MYSQL_UDF_READ_SHAPE: &str = "SELECT side_effecting_udf()";
const DU01_MYSQL_VIEW_READ_SHAPE: &str = "SELECT * FROM sql_security_definer_view";
const DU01_MYSQL_EXPLAIN_ANALYZE: &str = "EXPLAIN ANALYZE SELECT 7";
const DU01_REDIS_READ: &str = "GET du01:key";
const DU01_REDIS_MUTATION: &str = "SET du01:key du01-value";

struct AccessAdmissionObservation {
    label: &'static str,
    operation_id: OperationId,
    result: Result<ExecuteOutcome, ServiceError>,
    non_target_session_io: usize,
    user_target_dispatches: usize,
    saw_secret: bool,
    expected_secret: bool,
    forbidden_error_text: Vec<String>,
}

async fn observe_classified_access_admission(
    label: &'static str,
    operation_id: OperationId,
    driver: DriverKind,
    access: ProfileAccess,
    language: QueryLanguage,
    source: &'static str,
) -> AccessAdmissionObservation {
    let directory = tempfile::tempdir().expect("classified access-admission tempdir");
    let path = directory.path().join("config.toml");
    let connector = Arc::new(AccessAdmissionConnector::new(driver));
    let secrets = Arc::new(SessionSecretStore::default());
    let service = ApplicationService::with_dependencies(
        &path,
        connector.clone(),
        Arc::new(MissingEnvironment),
        secrets,
        ConfigWriter::default(),
    )
    .expect("classified access-admission service");
    let mut profile_draft = named_draft(driver, DU01_POSTURE_SENTINEL);
    profile_draft.host = DU01_ENDPOINT_SENTINEL.to_owned();
    profile_draft.environment = ProfileEnvironment::Production;
    profile_draft.access = access;
    profile_draft.credential_mode = CredentialMode::Session;
    let created = service
        .create_profile(create_request(
            DraftId(operation_id.0),
            OperationId(operation_id.0.saturating_sub(1)),
            Some("du01-profile"),
            profile_draft,
            SessionSecretUpdate::Replace(Arc::new(SessionSecret::new(
                DU01_CREDENTIAL_SENTINEL.to_owned(),
            ))),
        ))
        .await
        .expect("classified access-admission profile");
    let profiles = service.profiles_snapshot().await;
    assert_eq!(profiles.len(), 1, "{label}: exact classified profile");
    assert!(matches!(
        profiles[0].safety,
        ProfileSafetyPosture::Classified {
            environment: ProfileEnvironment::Production,
            access: actual,
            ..
        } if actual == access
    ));

    let result = service
        .execute_at(ExecuteRequest {
            operation_id,
            profile_id: created.profile_id,
            profile_generation: created.profile_generation,
            language,
            text: source.to_owned(),
            row_limit: 7,
            timeout: Duration::from_secs(1),
        })
        .await;
    AccessAdmissionObservation {
        label,
        operation_id,
        result,
        non_target_session_io: connector.io.non_target_session_io.load(Ordering::SeqCst),
        user_target_dispatches: connector.io.user_target_dispatches.load(Ordering::SeqCst),
        saw_secret: connector.saw_secret.load(Ordering::SeqCst),
        expected_secret: true,
        forbidden_error_text: vec![
            DU01_ENDPOINT_SENTINEL.to_owned(),
            DU01_CREDENTIAL_SENTINEL.to_owned(),
            DU01_POSTURE_SENTINEL.to_owned(),
            source.to_owned(),
            format!("{access:?}"),
            "Production".to_owned(),
        ],
    }
}

async fn observe_legacy_access_admission(
    label: &'static str,
    operation_id: OperationId,
    source_version: LegacyConfigVersion,
    driver: DriverKind,
    language: QueryLanguage,
    source: &'static str,
) -> AccessAdmissionObservation {
    let directory = tempfile::tempdir().expect("legacy access-admission tempdir");
    let path = directory.path().join("config.toml");
    let (version, driver_name, port, tls, credential_mode) = match (source_version, driver) {
        (LegacyConfigVersion::V1, DriverKind::MySql) => (1, "mysql", 3306, "preferred", ""),
        (LegacyConfigVersion::V1, DriverKind::Redis) => (1, "redis", 6379, "disabled", ""),
        (LegacyConfigVersion::V2, DriverKind::MySql) => (
            2,
            "mysql",
            3306,
            "preferred",
            "credential_mode = \"none\"\n",
        ),
        (LegacyConfigVersion::V2, DriverKind::Redis) => {
            (2, "redis", 6379, "disabled", "credential_mode = \"none\"\n")
        }
        (_, DriverKind::MongoDb) => unreachable!("DU-01 admission covers MySQL and Redis"),
    };
    let encoded = format!(
        "version = {version}\n\n[[profiles]]\nid = \"du01-legacy\"\nname = \"{DU01_POSTURE_SENTINEL}\"\ndriver = \"{driver_name}\"\nhost = \"{DU01_ENDPOINT_SENTINEL}\"\nport = {port}\ntls = \"{tls}\"\n{credential_mode}"
    );
    fs::write(&path, encoded).expect("legacy access-admission fixture");
    let connector = Arc::new(AccessAdmissionConnector::new(driver));
    let service = ApplicationService::with_dependencies(
        &path,
        connector.clone(),
        Arc::new(MissingEnvironment),
        Arc::new(SessionSecretStore::default()),
        ConfigWriter::default(),
    )
    .expect("legacy access-admission service");
    let profiles = service.profiles_snapshot().await;
    assert_eq!(profiles.len(), 1, "{label}: exact legacy profile");
    assert!(matches!(
        profiles[0].safety,
        ProfileSafetyPosture::UnclassifiedLegacy { source } if source == source_version
    ));
    let profile_id = ProfileId("du01-legacy".to_owned());
    let profile_generation = service
        .profile_generation(&profile_id)
        .await
        .expect("legacy access-admission generation");
    let result = service
        .execute_at(ExecuteRequest {
            operation_id,
            profile_id,
            profile_generation,
            language,
            text: source.to_owned(),
            row_limit: 7,
            timeout: Duration::from_secs(1),
        })
        .await;
    AccessAdmissionObservation {
        label,
        operation_id,
        result,
        non_target_session_io: connector.io.non_target_session_io.load(Ordering::SeqCst),
        user_target_dispatches: connector.io.user_target_dispatches.load(Ordering::SeqCst),
        saw_secret: connector.saw_secret.load(Ordering::SeqCst),
        expected_secret: false,
        forbidden_error_text: vec![
            DU01_ENDPOINT_SENTINEL.to_owned(),
            DU01_CREDENTIAL_SENTINEL.to_owned(),
            DU01_POSTURE_SENTINEL.to_owned(),
            source.to_owned(),
            "UnclassifiedLegacy".to_owned(),
            format!("{source_version:?}"),
        ],
    }
}

fn assert_access_admission_error_is_public_safe(
    label: &str,
    error: &ServiceError,
    forbidden: &[String],
) {
    assert!(matches!(
        error,
        ServiceError::ReadOnlyProfile {
            code: PublicCode::ReadOnlyProfile,
        }
    ));
    assert_eq!(
        error.public_error_parts(),
        (PublicSummary::PermissionDenied, PublicCode::ReadOnlyProfile,),
        "{label}: stable local access-admission error"
    );
    let public_surfaces = format!(
        "{error:?}\n{error}\n{:?}\n{:?}\n{}",
        error.public_summary(),
        error.public_code(),
        error.public_summary()
    );
    for forbidden_value in forbidden {
        assert!(
            !public_surfaces.contains(forbidden_value),
            "{label}: public error leaked forbidden profile, target, endpoint, or credential data"
        );
    }
}

#[tokio::test]
async fn du01_access_admission_is_effective_read_only_without_blocking_read_paths() {
    for (language, target, expected) in [
        (
            ExecutionLanguage::MySql,
            ExecutionTarget::MySqlText(DU01_MYSQL_READ.to_owned()),
            OperationKind::ExecuteRead,
        ),
        (
            ExecutionLanguage::MySql,
            ExecutionTarget::MySqlText(DU01_MYSQL_MUTATION.to_owned()),
            OperationKind::ExecuteMutation,
        ),
        (
            ExecutionLanguage::MySql,
            ExecutionTarget::MySqlText(DU01_MYSQL_UDF_READ_SHAPE.to_owned()),
            OperationKind::ExecuteMutation,
        ),
        (
            ExecutionLanguage::MySql,
            ExecutionTarget::MySqlText(DU01_MYSQL_VIEW_READ_SHAPE.to_owned()),
            OperationKind::ExecuteMutation,
        ),
        (
            ExecutionLanguage::MySql,
            ExecutionTarget::MySqlText(DU01_MYSQL_EXPLAIN_ANALYZE.to_owned()),
            OperationKind::ExecuteMutation,
        ),
        (
            ExecutionLanguage::MySql,
            ExecutionTarget::MySqlText("SHOW TABLES".to_owned()),
            OperationKind::ExecuteMutation,
        ),
        (
            ExecutionLanguage::MySql,
            ExecutionTarget::MySqlText("DESCRIBE widgets".to_owned()),
            OperationKind::ExecuteMutation,
        ),
        (
            ExecutionLanguage::MySql,
            ExecutionTarget::MySqlText("DESC widgets".to_owned()),
            OperationKind::ExecuteMutation,
        ),
        (
            ExecutionLanguage::Redis,
            ExecutionTarget::RedisArgv(vec!["GET".to_owned(), "du01:key".to_owned()]),
            OperationKind::ExecuteRead,
        ),
        (
            ExecutionLanguage::Redis,
            ExecutionTarget::RedisArgv(vec![
                "SET".to_owned(),
                "du01:key".to_owned(),
                "du01-value".to_owned(),
            ]),
            OperationKind::ExecuteMutation,
        ),
    ] {
        assert_eq!(classify_execution_kind(language, &target), expected);
    }

    let blocked = vec![
        observe_classified_access_admission(
            "classified read-only MySQL mutation",
            OperationId(701),
            DriverKind::MySql,
            ProfileAccess::ReadOnly,
            QueryLanguage::Sql,
            DU01_MYSQL_MUTATION,
        )
        .await,
        observe_classified_access_admission(
            "classified read-only Redis mutation",
            OperationId(702),
            DriverKind::Redis,
            ProfileAccess::ReadOnly,
            QueryLanguage::RedisCommand,
            DU01_REDIS_MUTATION,
        )
        .await,
        observe_classified_access_admission(
            "classified read-only MySQL UDF-shaped target",
            OperationId(711),
            DriverKind::MySql,
            ProfileAccess::ReadOnly,
            QueryLanguage::Sql,
            DU01_MYSQL_UDF_READ_SHAPE,
        )
        .await,
        observe_classified_access_admission(
            "classified read-only MySQL view-shaped target",
            OperationId(712),
            DriverKind::MySql,
            ProfileAccess::ReadOnly,
            QueryLanguage::Sql,
            DU01_MYSQL_VIEW_READ_SHAPE,
        )
        .await,
        observe_classified_access_admission(
            "classified read-only MySQL EXPLAIN ANALYZE target",
            OperationId(713),
            DriverKind::MySql,
            ProfileAccess::ReadOnly,
            QueryLanguage::Sql,
            DU01_MYSQL_EXPLAIN_ANALYZE,
        )
        .await,
        observe_legacy_access_admission(
            "unclassified v1 MySQL mutation",
            OperationId(703),
            LegacyConfigVersion::V1,
            DriverKind::MySql,
            QueryLanguage::Sql,
            DU01_MYSQL_MUTATION,
        )
        .await,
        observe_legacy_access_admission(
            "unclassified v2 Redis mutation",
            OperationId(704),
            LegacyConfigVersion::V2,
            DriverKind::Redis,
            QueryLanguage::RedisCommand,
            DU01_REDIS_MUTATION,
        )
        .await,
    ];
    let allowed = vec![
        observe_classified_access_admission(
            "classified read-only MySQL read",
            OperationId(705),
            DriverKind::MySql,
            ProfileAccess::ReadOnly,
            QueryLanguage::Sql,
            DU01_MYSQL_READ,
        )
        .await,
        observe_classified_access_admission(
            "classified read-only Redis read",
            OperationId(706),
            DriverKind::Redis,
            ProfileAccess::ReadOnly,
            QueryLanguage::RedisCommand,
            DU01_REDIS_READ,
        )
        .await,
        observe_legacy_access_admission(
            "unclassified v1 MySQL read",
            OperationId(707),
            LegacyConfigVersion::V1,
            DriverKind::MySql,
            QueryLanguage::Sql,
            DU01_MYSQL_READ,
        )
        .await,
        observe_legacy_access_admission(
            "unclassified v2 Redis read",
            OperationId(708),
            LegacyConfigVersion::V2,
            DriverKind::Redis,
            QueryLanguage::RedisCommand,
            DU01_REDIS_READ,
        )
        .await,
        observe_classified_access_admission(
            "classified read-write MySQL mutation",
            OperationId(709),
            DriverKind::MySql,
            ProfileAccess::ReadWrite,
            QueryLanguage::Sql,
            DU01_MYSQL_MUTATION,
        )
        .await,
        observe_classified_access_admission(
            "classified read-write Redis mutation",
            OperationId(710),
            DriverKind::Redis,
            ProfileAccess::ReadWrite,
            QueryLanguage::RedisCommand,
            DU01_REDIS_MUTATION,
        )
        .await,
    ];

    for observation in blocked {
        assert_eq!(
            observation.user_target_dispatches, 0,
            "{}: mutation reached a typed user-target driver port after {} permitted non-target session I/O calls",
            observation.label, observation.non_target_session_io
        );
        if observation.non_target_session_io > 0 {
            assert_eq!(
                observation.saw_secret, observation.expected_secret,
                "{}: metadata-only lease used the wrong credential capability",
                observation.label
            );
        }
        let error = observation
            .result
            .expect_err("DU-01 mutation posture must reject locally");
        assert_access_admission_error_is_public_safe(
            observation.label,
            &error,
            &observation.forbidden_error_text,
        );
    }

    for observation in allowed {
        assert_eq!(
            observation.user_target_dispatches, 1,
            "{}: admitted path must dispatch exactly one typed user target",
            observation.label
        );
        assert_eq!(
            observation.saw_secret, observation.expected_secret,
            "{}: admitted path used the wrong credential capability",
            observation.label
        );
        let outcome = observation
            .result
            .expect("DU-01 effective read-only and classified read-write paths remain available");
        assert_eq!(outcome.operation_id, observation.operation_id);
    }
}

#[tokio::test]
async fn saved_check_then_execute_reuses_a_session_and_preserves_correlation() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    let connector = Arc::new(FakeConnector::new(false));
    let service = service(&path, connector.clone(), ConfigWriter::default());
    let created = service
        .create_profile(create_request(
            DraftId(1),
            OperationId(1),
            Some("profile"),
            draft(DriverKind::MySql),
            SessionSecretUpdate::Clear,
        ))
        .await
        .expect("create");

    let check = service
        .check(
            OperationId(41),
            created.profile_id.clone(),
            Duration::from_secs(1),
        )
        .await
        .expect("check");
    let execute = service
        .execute(ExecuteRequest {
            operation_id: OperationId(42),
            profile_id: created.profile_id.clone(),
            profile_generation: created.profile_generation,
            language: QueryLanguage::Sql,
            text: "SELECT 1".to_owned(),
            row_limit: 100,
            timeout: Duration::from_secs(1),
        })
        .await
        .expect("execute");

    assert_eq!(connector.connects.load(Ordering::SeqCst), 1);
    assert_eq!(connector.session.pings.load(Ordering::SeqCst), 1);
    assert_eq!(connector.session.executes.load(Ordering::SeqCst), 1);
    assert_eq!(check.operation_id, OperationId(41));
    assert_eq!(execute.operation_id, OperationId(42));
}

#[tokio::test]
async fn startup_rejects_identity_corruption_but_keeps_semantic_invalid_profiles_editable() {
    let directory = tempfile::tempdir().expect("tempdir");
    let mut semantic_invalid_profiles = Vec::new();
    let valid =
        dbotter::model::ConnectionProfile::from_draft("valid".to_owned(), draft(DriverKind::Redis));

    let mut invalid_host = valid.clone();
    invalid_host.host.clear();
    semantic_invalid_profiles.push(("host", invalid_host));
    let mut invalid_port = valid.clone();
    invalid_port.port = 0;
    semantic_invalid_profiles.push(("port", invalid_port));
    let mut invalid_environment = valid.clone();
    invalid_environment.credential_mode = CredentialMode::Environment;
    invalid_environment.secret_env = None;
    semantic_invalid_profiles.push(("missing environment name", invalid_environment));
    let mut invalid_none = valid.clone();
    invalid_none.credential_mode = CredentialMode::None;
    invalid_none.secret_env = Some("MUST_NOT_SURVIVE".to_owned());
    semantic_invalid_profiles.push(("None with environment name", invalid_none));
    let mut invalid_redis_ca = valid.clone();
    invalid_redis_ca.tls = TlsMode::Disabled;
    invalid_redis_ca.redis_tls.ca_file = Some(directory.path().join("must-not-be-read.pem"));
    semantic_invalid_profiles.push(("Redis CA while TLS disabled", invalid_redis_ca));

    for (index, (case, profile)) in semantic_invalid_profiles.into_iter().enumerate() {
        let path = directory.path().join(format!("invalid-{index}.toml"));
        let profile_id = ProfileId(profile.id.clone());
        let encoded = toml::to_string(&Config {
            version: 2,
            profiles: vec![profile],
        })
        .expect("invalid semantic fixture still serializes");
        fs::write(&path, encoded).expect("fixture write");
        let connector = Arc::new(FakeConnector::new(false));
        let service = ApplicationService::with_dependencies(
            &path,
            connector.clone(),
            Arc::new(MissingEnvironment),
            Arc::new(SessionSecretStore::default()),
            ConfigWriter::default(),
        )
        .expect("semantic-invalid profile remains visible for editing");
        assert!(
            matches!(
                service
                    .check(OperationId(50), profile_id, Duration::from_secs(1))
                    .await,
                Err(ServiceError::InvalidProfile(_))
            ),
            "case={case}"
        );
        assert_eq!(connector.connects.load(Ordering::SeqCst), 0, "{case}");
    }

    let path = directory.path().join("invalid-id.toml");
    let mut invalid_id = valid.clone();
    invalid_id.id = " invalid".to_owned();
    fs::write(
        &path,
        toml::to_string(&Config {
            version: 2,
            profiles: vec![invalid_id],
        })
        .expect("invalid-id fixture serializes"),
    )
    .expect("invalid-id write");
    assert!(matches!(
        ApplicationService::with_dependencies(
            &path,
            Arc::new(FakeConnector::new(false)),
            Arc::new(MissingEnvironment),
            Arc::new(SessionSecretStore::default()),
            ConfigWriter::default(),
        ),
        Err(ServiceError::InvalidProfile(
            ProfileValidationError::Field {
                field: ProfileFieldId::ConnectionId,
                ..
            }
        ))
    ));

    let path = directory.path().join("duplicate.toml");
    let duplicate = valid.clone();
    let encoded = toml::to_string(&Config {
        version: 2,
        profiles: vec![valid, duplicate],
    })
    .expect("duplicate fixture serializes");
    fs::write(&path, encoded).expect("duplicate fixture write");
    let connector = Arc::new(FakeConnector::new(false));
    let result = ApplicationService::with_dependencies(
        &path,
        connector.clone(),
        Arc::new(MissingEnvironment),
        Arc::new(SessionSecretStore::default()),
        ConfigWriter::default(),
    );
    assert!(matches!(
        result,
        Err(ServiceError::InvalidProfile(
            ProfileValidationError::Field {
                field: ProfileFieldId::ConnectionId,
                ..
            }
        ))
    ));
    assert_eq!(connector.connects.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn legacy_redis_preferred_loads_visibly_but_is_typed_edit_required_before_network() {
    let directory = tempfile::tempdir().expect("tempdir");
    let mut preferred = dbotter::model::ConnectionProfile::from_draft(
        "legacy-redis".to_owned(),
        draft(DriverKind::Redis),
    );
    preferred.tls = TlsMode::Preferred;
    let v1 = "version = 1\n[[profiles]]\nid = \"legacy-redis\"\nname = \"Legacy Redis\"\ndriver = \"redis\"\nhost = \"127.0.0.1\"\nport = 6379\ntls = \"preferred\"\n".to_owned();
    let v2 = toml::to_string(&Config {
        version: 2,
        profiles: vec![preferred],
    })
    .expect("v2 legacy fixture");

    for (index, encoded) in [v1, v2].into_iter().enumerate() {
        let path = directory
            .path()
            .join(format!("preferred-v{}.toml", index + 1));
        fs::write(&path, encoded).expect("legacy fixture");
        let connector = Arc::new(FakeConnector::new(false));
        let service = ApplicationService::with_dependencies(
            &path,
            connector.clone(),
            Arc::new(MissingEnvironment),
            Arc::new(SessionSecretStore::default()),
            ConfigWriter::default(),
        )
        .expect("legacy profile remains visible");
        let snapshots = service.profiles_snapshot().await;
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].tls, TlsMode::Preferred);

        let saved_error = service
            .check(
                OperationId(61),
                ProfileId("legacy-redis".to_owned()),
                Duration::from_secs(1),
            )
            .await
            .expect_err("legacy Preferred is edit-required before network");
        assert!(matches!(
            &saved_error,
            ServiceError::InvalidProfile(ProfileValidationError::Field {
                field: ProfileFieldId::RedisTlsMode,
                code: PublicCode::RedisTlsPreferredLegacy,
            })
        ));
        assert_eq!(
            saved_error.public_error_parts(),
            (
                PublicSummary::UnsupportedFeature,
                PublicCode::RedisTlsPreferredLegacy,
            )
        );
        assert_eq!(
            recovery_for(
                OperationKind::ConnectProfile,
                saved_error.public_summary(),
                saved_error.public_code(),
                &SafeContext::profile(ProfileId("legacy-redis".to_owned()), OperationId(61),),
            )
            .expect("saved legacy recovery")
            .as_slice(),
            &[RecoveryAction::EditProfile(
                ProfileId("legacy-redis".to_owned()),
                ProfileFieldId::RedisTlsMode,
            )]
        );

        let mut preferred_draft = draft(DriverKind::Redis);
        preferred_draft.tls = TlsMode::Preferred;
        let preferred_request = service
            .prepare_secretless_draft_test(
                DraftId(62),
                OperationId(62),
                preferred_draft,
                Duration::from_secs(1),
            )
            .expect("prepare Preferred draft");
        let draft_error = service
            .test_draft_connection(preferred_request)
            .await
            .expect_err("draft Preferred is rejected before network");
        assert_eq!(
            draft_error.public_error_parts(),
            (
                PublicSummary::UnsupportedFeature,
                PublicCode::RedisTlsPreferredLegacy,
            )
        );
        assert_eq!(
            recovery_for(
                OperationKind::TestDraftConnection,
                draft_error.public_summary(),
                draft_error.public_code(),
                &SafeContext::draft(DraftId(62), OperationId(62)),
            )
            .expect("draft legacy recovery")
            .as_slice(),
            &[RecoveryAction::EditDraft(
                DraftId(62),
                ProfileFieldId::RedisTlsMode,
            )]
        );
        assert_eq!(connector.connects.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn auto_suffix_is_deterministic_and_explicit_collision_never_overwrites() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    let connector = Arc::new(FakeConnector::new(false));
    let service = service(&path, connector, ConfigWriter::default());

    let first = service
        .create_profile(create_request(
            DraftId(10),
            OperationId(10),
            None,
            named_draft(DriverKind::Redis, "Local Redis"),
            SessionSecretUpdate::Clear,
        ))
        .await
        .expect("first auto id");
    let second = service
        .create_profile(create_request(
            DraftId(11),
            OperationId(11),
            None,
            named_draft(DriverKind::Redis, "Local Redis"),
            SessionSecretUpdate::Clear,
        ))
        .await
        .expect("second auto id");
    assert_eq!(first.profile_id, ProfileId("local-redis".to_owned()));
    assert_eq!(second.profile_id, ProfileId("local-redis-2".to_owned()));

    let before = fs::read(&path).expect("before collision");
    let collision = service
        .create_profile(create_request(
            DraftId(12),
            OperationId(12),
            Some("local-redis"),
            named_draft(DriverKind::Redis, "Must not overwrite"),
            SessionSecretUpdate::Clear,
        ))
        .await
        .expect_err("explicit collision");
    assert!(matches!(
        collision,
        ServiceError::ProfileIdConflict {
            draft_id: DraftId(12),
            operation_id: OperationId(12)
        }
    ));
    assert_eq!(fs::read(&path).expect("after collision"), before);
}

#[tokio::test]
async fn update_requires_current_generation_and_delete_cannot_recreate_missing_profile() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    let service = service(
        &path,
        Arc::new(FakeConnector::new(false)),
        ConfigWriter::default(),
    );
    let created = service
        .create_profile(create_request(
            DraftId(20),
            OperationId(20),
            Some("immutable"),
            draft(DriverKind::MySql),
            SessionSecretUpdate::Clear,
        ))
        .await
        .expect("create");

    let stale = service
        .update_profile(UpdateProfileRequest {
            profile_id: created.profile_id.clone(),
            expected_generation: ProfileGeneration(created.profile_generation.0 + 1),
            operation_id: OperationId(21),
            draft: named_draft(DriverKind::MySql, "Edited"),
            secret_update: SessionSecretUpdate::Clear,
            migration_consent: MigrationConsent::Confirmed,
        })
        .await
        .expect_err("stale update");
    assert!(matches!(stale, ServiceError::ProfileStale { .. }));

    let updated = service
        .update_profile(UpdateProfileRequest {
            profile_id: created.profile_id.clone(),
            expected_generation: created.profile_generation,
            operation_id: OperationId(22),
            draft: named_draft(DriverKind::MySql, "Edited"),
            secret_update: SessionSecretUpdate::Clear,
            migration_consent: MigrationConsent::Confirmed,
        })
        .await
        .expect("update");
    service
        .delete_profile(DeleteProfileRequest {
            profile_id: updated.profile_id.clone(),
            expected_generation: updated.profile_generation,
            operation_id: OperationId(23),
            migration_consent: MigrationConsent::Confirmed,
        })
        .await
        .expect("delete");
    assert!(service.profiles_snapshot().await.is_empty());
    assert!(matches!(
        service
            .update_profile(UpdateProfileRequest {
                profile_id: updated.profile_id,
                expected_generation: updated.profile_generation,
                operation_id: OperationId(24),
                draft: draft(DriverKind::MySql),
                secret_update: SessionSecretUpdate::Clear,
                migration_consent: MigrationConsent::Confirmed,
            })
            .await,
        Err(ServiceError::ProfileStale { .. })
    ));
}

#[tokio::test]
async fn test_draft_is_ephemeral_and_replace_secret_is_not_stored() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    let connector = Arc::new(FakeConnector::new(false));
    let store = Arc::new(SessionSecretStore::default());
    let service = ApplicationService::with_dependencies(
        &path,
        connector.clone(),
        Arc::new(MissingEnvironment),
        store.clone(),
        ConfigWriter::default(),
    )
    .expect("service");
    let before_profiles = service.profiles_snapshot().await;
    let before_cache = service.cached_session_count().await;
    let replacement = ReplacementSecretBuffer::new("one-shot-secret".to_owned());
    let mut session_draft = draft(DriverKind::Redis);
    session_draft.credential_mode = CredentialMode::Session;

    let request = service
        .prepare_replacement_draft_test(
            DraftId(30),
            OperationId(30),
            session_draft,
            &replacement,
            Duration::from_secs(1),
        )
        .expect("prepare replacement draft");
    let outcome = service
        .test_draft_connection(request)
        .await
        .expect("draft test");

    assert_eq!(outcome.draft_id, DraftId(30));
    assert_eq!(connector.connects.load(Ordering::SeqCst), 1);
    assert_eq!(connector.session.pings.load(Ordering::SeqCst), 1);
    assert_eq!(connector.session.closes.load(Ordering::SeqCst), 1);
    assert!(connector.saw_secret.load(Ordering::SeqCst));
    assert_eq!(service.profiles_snapshot().await, before_profiles);
    assert_eq!(service.cached_session_count().await, before_cache);
    assert!(store.is_empty().expect("store unchanged"));
    assert_eq!(replacement.as_str(), "one-shot-secret");
    assert!(!path.exists());
}

#[tokio::test]
async fn keep_draft_test_clones_the_current_arc_read_only_and_leaves_store_unchanged() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    let connector = Arc::new(FakeConnector::new(false));
    let store = Arc::new(SessionSecretStore::default());
    let stored = Arc::new(SessionSecret::new("current-session-secret".to_owned()));
    let service = ApplicationService::with_dependencies(
        &path,
        connector.clone(),
        Arc::new(MissingEnvironment),
        store.clone(),
        ConfigWriter::default(),
    )
    .expect("service");
    let mut session_draft = draft(DriverKind::Redis);
    session_draft.credential_mode = CredentialMode::Session;
    let created = service
        .create_profile(create_request(
            DraftId(32),
            OperationId(31),
            Some("existing"),
            session_draft.clone(),
            SessionSecretUpdate::Replace(stored.clone()),
        ))
        .await
        .expect("seed saved profile and store");
    let request = service
        .prepare_keep_current_draft_test(
            created.profile_id.clone(),
            created.profile_generation,
            DraftId(32),
            OperationId(32),
            session_draft,
            Duration::from_secs(1),
        )
        .await
        .expect("prepare Keep test");
    let persisted_before_test = fs::read(&path).expect("saved config");
    assert_eq!(Arc::strong_count(&stored), 3);

    service
        .test_draft_connection(request)
        .await
        .expect("Keep test");

    assert_eq!(Arc::strong_count(&stored), 2);
    assert!(store.has_current(&created.profile_id).expect("still set"));
    assert_eq!(connector.connects.load(Ordering::SeqCst), 1);
    assert_eq!(connector.session.closes.load(Ordering::SeqCst), 1);
    assert_eq!(
        fs::read(&path).expect("unchanged config"),
        persisted_before_test
    );
    assert_eq!(service.cached_session_count().await, 0);
}

#[tokio::test]
async fn keep_current_never_attaches_a_saved_secret_to_an_edited_connection_draft() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    let connector = Arc::new(FakeConnector::new(false));
    let store = Arc::new(SessionSecretStore::default());
    let service = ApplicationService::with_dependencies(
        &path,
        connector.clone(),
        Arc::new(MissingEnvironment),
        store.clone(),
        ConfigWriter::default(),
    )
    .expect("service");
    let mut persisted = draft(DriverKind::Redis);
    persisted.credential_mode = CredentialMode::Session;
    let created = service
        .create_profile(create_request(
            DraftId(33),
            OperationId(33),
            Some("keep-exact"),
            persisted.clone(),
            SessionSecretUpdate::Replace(Arc::new(SessionSecret::new(
                "saved-secret-must-not-leave".to_owned(),
            ))),
        ))
        .await
        .expect("saved session profile");

    let mut changed = Vec::new();
    let mut host = persisted.clone();
    host.host = "attacker.invalid".to_owned();
    changed.push(("host", host));
    let mut port = persisted.clone();
    port.port = 16_379;
    changed.push(("port", port));
    let mut database = persisted.clone();
    database.database = Some("7".to_owned());
    changed.push(("database", database));
    let mut username = persisted.clone();
    username.username = Some("other-user".to_owned());
    changed.push(("username", username));
    let mut driver = persisted.clone();
    driver.driver = DriverKind::MySql;
    changed.push(("driver", driver));
    let mut tls = persisted.clone();
    tls.tls = TlsMode::Required;
    changed.push(("tls", tls));
    let mut ca = persisted.clone();
    ca.redis_tls.ca_file = Some(directory.path().join("other-ca.pem"));
    changed.push(("ca", ca));
    for (index, (field, draft)) in changed.into_iter().enumerate() {
        let operation_id = OperationId(340 + u64::try_from(index).expect("small index"));
        let error = service
            .prepare_keep_current_draft_test(
                created.profile_id.clone(),
                created.profile_generation,
                DraftId(34),
                operation_id,
                draft,
                Duration::from_secs(1),
            )
            .await
            .expect_err("edited drafts require a replacement secret");
        assert!(
            matches!(
                error,
                ServiceError::DraftCredentialRequired {
                    draft_id: DraftId(34),
                    operation_id: actual,
                    code: dbotter::model::PublicCode::SessionCredential,
                } if actual == operation_id
            ),
            "field={field}"
        );
    }
    assert_eq!(connector.connects.load(Ordering::SeqCst), 0);
    let mut name_only = persisted;
    name_only.name = "Runtime-neutral display name".to_owned();
    let request = service
        .prepare_keep_current_draft_test(
            created.profile_id.clone(),
            created.profile_generation,
            DraftId(35),
            OperationId(35),
            name_only,
            Duration::from_secs(1),
        )
        .await
        .expect("display-name-only edit may use the read-only current Arc");
    service
        .test_draft_connection(request)
        .await
        .expect("display-name-only Keep test succeeds");
    assert_eq!(connector.connects.load(Ordering::SeqCst), 1);
    assert!(
        store
            .has_current(&created.profile_id)
            .expect("stored Arc remains current")
    );
}

#[tokio::test]
async fn reload_cannot_clear_a_keep_capability_between_validation_and_commit() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    let store = Arc::new(SessionSecretStore::default());
    let profile_id = ProfileId("reload-keep".to_owned());
    let barrier = Arc::new(PreRenameBarrier::new(store.clone(), profile_id.clone()));
    let service = ApplicationService::with_dependencies(
        &path,
        Arc::new(FakeConnector::new(false)),
        Arc::new(MissingEnvironment),
        store,
        ConfigWriter::with_fault_injector(barrier.clone()),
    )
    .expect("service");
    let mut session_draft = draft(DriverKind::MySql);
    session_draft.credential_mode = CredentialMode::Session;
    let created = service
        .create_profile(create_request(
            DraftId(35),
            OperationId(35),
            Some(profile_id.as_str()),
            session_draft.clone(),
            SessionSecretUpdate::Replace(Arc::new(SessionSecret::new(
                "keep-across-commit".to_owned(),
            ))),
        ))
        .await
        .expect("seed profile");
    barrier.arm();

    let mut changed_draft = session_draft.clone();
    changed_draft.name = "Changed while reload waits".to_owned();
    changed_draft.host = "database.internal".to_owned();
    let updating = service.clone();
    let update = tokio::spawn(async move {
        updating
            .update_profile(UpdateProfileRequest {
                profile_id: created.profile_id,
                expected_generation: created.profile_generation,
                operation_id: OperationId(36),
                draft: changed_draft,
                secret_update: SessionSecretUpdate::Keep,
                migration_consent: MigrationConsent::Cancelled,
            })
            .await
    });
    let waiting = barrier.clone();
    tokio::task::spawn_blocking(move || waiting.wait_until_entered())
        .await
        .expect("barrier wait joins");

    let reloading = service.clone();
    let reload = tokio::spawn(async move { reloading.reload_configuration().await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    let reload_completed_before_commit = reload.is_finished();
    barrier.release();

    update
        .await
        .expect("update task joins")
        .expect("Keep update commits");
    reload
        .await
        .expect("reload task joins")
        .expect("serialized reload succeeds");
    assert!(!reload_completed_before_commit);
    assert!(barrier.saw_current_at_release.load(Ordering::SeqCst));
    let disk = load_path(&path).expect("final exact disk config").config;
    assert_eq!(service.profiles_snapshot().await, disk.profiles);
    assert_eq!(disk.profiles[0].name, "Changed while reload waits");
    assert_eq!(disk.profiles[0].host, "database.internal");
    assert!(
        service
            .has_current_session_secret(&profile_id)
            .expect("unchanged reload preserves current Arc")
    );
}

#[tokio::test]
async fn keep_prepare_and_replace_update_cannot_form_an_old_draft_new_secret_pair() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    let connector = Arc::new(FakeConnector::new(false));
    let store = Arc::new(SessionSecretStore::default());
    let profile_id = ProfileId("prepare-keep-race".to_owned());
    let barrier = Arc::new(PreRenameBarrier::new(store.clone(), profile_id.clone()));
    let service = ApplicationService::with_dependencies(
        &path,
        connector.clone(),
        Arc::new(MissingEnvironment),
        store.clone(),
        ConfigWriter::with_fault_injector(barrier.clone()),
    )
    .expect("service");
    let mut original_draft = draft(DriverKind::Redis);
    original_draft.credential_mode = CredentialMode::Session;
    let created = service
        .create_profile(create_request(
            DraftId(37),
            OperationId(37),
            Some(profile_id.as_str()),
            original_draft.clone(),
            SessionSecretUpdate::Replace(Arc::new(SessionSecret::new("old-credential".to_owned()))),
        ))
        .await
        .expect("seed profile");
    let mut changed_draft = original_draft.clone();
    changed_draft.host = "new-endpoint.internal".to_owned();
    barrier.arm();

    let updating = service.clone();
    let update_profile_id = created.profile_id.clone();
    let update = tokio::spawn(async move {
        updating
            .update_profile(UpdateProfileRequest {
                profile_id: update_profile_id,
                expected_generation: created.profile_generation,
                operation_id: OperationId(38),
                draft: changed_draft,
                secret_update: SessionSecretUpdate::Replace(Arc::new(SessionSecret::new(
                    "new-credential".to_owned(),
                ))),
                migration_consent: MigrationConsent::Cancelled,
            })
            .await
    });
    let waiting = barrier.clone();
    tokio::task::spawn_blocking(move || waiting.wait_until_entered())
        .await
        .expect("barrier wait joins");

    let preparing = service.clone();
    let prepare_profile_id = profile_id.clone();
    let prepare_and_test = tokio::spawn(async move {
        let request = preparing
            .prepare_keep_current_draft_test(
                prepare_profile_id,
                created.profile_generation,
                DraftId(39),
                OperationId(39),
                original_draft,
                Duration::from_secs(1),
            )
            .await?;
        preparing.test_draft_connection(request).await.map(|_| ())
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !prepare_and_test.is_finished(),
        "prepare must wait for the mutation snapshot to finish"
    );
    barrier.release();

    update
        .await
        .expect("update task joins")
        .expect("replacement update commits");
    let error = prepare_and_test
        .await
        .expect("prepare task joins")
        .expect_err("old generation is rejected after serialized update");
    assert!(matches!(
        error,
        ServiceError::ProfileStale {
            profile_id: actual,
            operation_id: OperationId(39),
        } if actual == profile_id
    ));
    assert_eq!(connector.connects.load(Ordering::SeqCst), 0);
    assert!(
        store
            .has_current(&profile_id)
            .expect("new Arc remains current")
    );
    assert_eq!(
        service.profiles_snapshot().await,
        load_path(&path).expect("disk config").config.profiles
    );
}

#[tokio::test]
async fn forget_or_missing_session_secret_fails_before_connector() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    let connector = Arc::new(FakeConnector::new(false));
    let service = service(&path, connector.clone(), ConfigWriter::default());
    let mut session_draft = draft(DriverKind::Redis);
    session_draft.credential_mode = CredentialMode::Session;

    let request = service
        .prepare_secretless_draft_test(
            DraftId(31),
            OperationId(31),
            session_draft,
            Duration::from_secs(1),
        )
        .expect("prepare Forget draft");
    let error = service
        .test_draft_connection(request)
        .await
        .expect_err("forget needs credential");
    assert!(matches!(
        error,
        ServiceError::DraftCredentialRequired {
            draft_id: DraftId(31),
            ..
        }
    ));
    assert_eq!(connector.connects.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn redis_required_invalid_ca_and_missing_transport_are_fail_closed() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    let invalid_ca = directory.path().join("invalid.pem");
    fs::write(&invalid_ca, "not a certificate").expect("invalid CA");
    let connector = Arc::new(FakeConnector::new(false));
    let service = service(&path, connector.clone(), ConfigWriter::default());
    let mut required = draft(DriverKind::Redis);
    required.tls = TlsMode::Required;
    required.redis_tls.ca_file = Some(invalid_ca);

    let invalid_ca_request = service
        .prepare_secretless_draft_test(
            DraftId(40),
            OperationId(40),
            required,
            Duration::from_secs(1),
        )
        .expect("prepare invalid CA draft");
    assert!(matches!(
        service.test_draft_connection(invalid_ca_request).await,
        Err(ServiceError::InvalidProfile(_))
    ));
    assert_eq!(connector.connects.load(Ordering::SeqCst), 0);

    let mut os_roots = draft(DriverKind::Redis);
    os_roots.tls = TlsMode::Required;
    let os_roots_request = service
        .prepare_secretless_draft_test(
            DraftId(41),
            OperationId(41),
            os_roots,
            Duration::from_secs(1),
        )
        .expect("prepare OS-roots draft");
    assert!(matches!(
        service.test_draft_connection(os_roots_request).await,
        Err(ServiceError::Driver(DriverError::Unsupported {
            driver: DriverKind::Redis,
            ..
        }))
    ));
    assert_eq!(connector.connects.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn independent_service_writers_preserve_unrelated_profiles() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    let left = service(
        &path,
        Arc::new(FakeConnector::new(false)),
        ConfigWriter::default(),
    );
    let right = service(
        &path,
        Arc::new(FakeConnector::new(false)),
        ConfigWriter::default(),
    );

    let (left_result, right_result) = tokio::join!(
        left.create_profile(create_request(
            DraftId(50),
            OperationId(50),
            Some("left"),
            draft(DriverKind::MySql),
            SessionSecretUpdate::Clear,
        )),
        right.create_profile(create_request(
            DraftId(51),
            OperationId(51),
            Some("right"),
            draft(DriverKind::Redis),
            SessionSecretUpdate::Clear,
        )),
    );
    left_result.expect("left writer");
    right_result.expect("right writer");
    let loaded = dbotter::config::load_path(&path).expect("reload");
    assert!(
        loaded
            .config
            .profiles
            .iter()
            .any(|profile| profile.id == "left")
    );
    assert!(
        loaded
            .config
            .profiles
            .iter()
            .any(|profile| profile.id == "right")
    );
}

#[tokio::test]
async fn sequential_services_reconcile_added_changed_and_removed_profiles_without_secret_pairing() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    let store_a = Arc::new(SessionSecretStore::default());
    let store_b = Arc::new(SessionSecretStore::default());
    let connector_b = Arc::new(EndpointSecretConnector::new());
    let service_a = ApplicationService::with_dependencies(
        &path,
        Arc::new(FakeConnector::new(false)),
        Arc::new(MissingEnvironment),
        store_a,
        ConfigWriter::default(),
    )
    .expect("service A");
    let service_b = ApplicationService::with_dependencies(
        &path,
        connector_b.clone(),
        Arc::new(MissingEnvironment),
        store_b.clone(),
        ConfigWriter::default(),
    )
    .expect("service B");

    let mut left_draft = named_draft(DriverKind::Redis, "Left");
    left_draft.credential_mode = CredentialMode::Session;
    let left_created = service_a
        .create_profile(create_request(
            DraftId(520),
            OperationId(520),
            Some("left"),
            left_draft.clone(),
            SessionSecretUpdate::Replace(Arc::new(SessionSecret::new("service-a-left".to_owned()))),
        ))
        .await
        .expect("A creates left");

    let mut right_draft = named_draft(DriverKind::Redis, "Right");
    right_draft.credential_mode = CredentialMode::Session;
    let right_secret = Arc::new(SessionSecret::new("service-b-right".to_owned()));
    let right_created = service_b
        .create_profile(create_request(
            DraftId(521),
            OperationId(521),
            Some("right"),
            right_draft.clone(),
            SessionSecretUpdate::Replace(right_secret.clone()),
        ))
        .await
        .expect("B creates right and observes left");
    let left_id = ProfileId("left".to_owned());
    let right_id = ProfileId("right".to_owned());
    let first_left_generation = service_b
        .profile_generation(&left_id)
        .await
        .expect("externally added left gets a generation");
    assert_eq!(service_b.profiles_snapshot().await.len(), 2);

    let old_left_secret = Arc::new(SessionSecret::new("service-b-old-left".to_owned()));
    let old_left_secret_address = Arc::as_ptr(&old_left_secret).addr();
    store_b
        .apply(&left_id, SessionSecretUpdate::Replace(old_left_secret))
        .expect("seed B's old left Arc");
    service_b
        .check(OperationId(522), left_id.clone(), Duration::from_secs(1))
        .await
        .expect("cache old left endpoint");
    service_b
        .check(OperationId(523), right_id.clone(), Duration::from_secs(1))
        .await
        .expect("cache right endpoint");
    assert_eq!(service_b.cached_session_count().await, 2);

    let mut changed_left = left_draft;
    changed_left.host = "new-left.internal".to_owned();
    let left_updated = service_a
        .update_profile(UpdateProfileRequest {
            profile_id: left_id.clone(),
            expected_generation: left_created.profile_generation,
            operation_id: OperationId(524),
            draft: changed_left,
            secret_update: SessionSecretUpdate::Keep,
            migration_consent: MigrationConsent::Cancelled,
        })
        .await
        .expect("A changes left endpoint");

    right_draft.name = "Right renamed".to_owned();
    let right_updated = service_b
        .update_profile(UpdateProfileRequest {
            profile_id: right_id.clone(),
            expected_generation: right_created.profile_generation,
            operation_id: OperationId(525),
            draft: right_draft.clone(),
            secret_update: SessionSecretUpdate::Keep,
            migration_consent: MigrationConsent::Cancelled,
        })
        .await
        .expect("B mutation observes changed left");
    let changed_left_generation = service_b
        .profile_generation(&left_id)
        .await
        .expect("changed external left gets a new generation");
    assert_ne!(changed_left_generation, first_left_generation);
    assert_eq!(
        service_b
            .profiles_snapshot()
            .await
            .iter()
            .find(|profile| profile.id == "left")
            .expect("left remains visible")
            .host,
        "new-left.internal"
    );
    assert!(!store_b.has_current(&left_id).expect("old left Arc cleared"));
    assert!(
        store_b
            .has_current(&right_id)
            .expect("local Keep Arc retained")
    );
    assert_eq!(service_b.cached_session_count().await, 0);
    assert!(connector_b.session.closes.load(Ordering::SeqCst) >= 2);
    let calls_before_new_left = connector_b.calls.lock().expect("calls").len();
    assert!(matches!(
        service_b
            .check(OperationId(526), left_id.clone(), Duration::from_secs(1))
            .await,
        Err(ServiceError::Secret(SecretError::SessionCredentialRequired))
    ));
    {
        let calls = connector_b.calls.lock().expect("calls");
        assert_eq!(calls.len(), calls_before_new_left);
        assert!(
            calls.iter().all(|(host, secret_address)| {
                !(host == "new-left.internal" && *secret_address == Some(old_left_secret_address))
            }),
            "old left credential must never reach the new endpoint"
        );
    }

    service_a
        .delete_profile(DeleteProfileRequest {
            profile_id: left_id.clone(),
            expected_generation: left_updated.profile_generation,
            operation_id: OperationId(527),
            migration_consent: MigrationConsent::Cancelled,
        })
        .await
        .expect("A deletes left");
    service_b
        .check(OperationId(528), right_id.clone(), Duration::from_secs(1))
        .await
        .expect("recache unchanged right");
    assert_eq!(service_b.cached_session_count().await, 1);
    let final_right = service_b
        .update_profile(UpdateProfileRequest {
            profile_id: right_id.clone(),
            expected_generation: right_updated.profile_generation,
            operation_id: OperationId(529),
            draft: right_draft,
            secret_update: SessionSecretUpdate::Keep,
            migration_consent: MigrationConsent::Cancelled,
        })
        .await
        .expect("B mutation observes removed left");
    assert!(matches!(
        service_b.profile_generation(&left_id).await,
        Err(ServiceError::UnknownProfile(_))
    ));
    assert_eq!(service_b.profiles_snapshot().await.len(), 1);
    assert_eq!(service_b.profiles_snapshot().await[0].id, "right");
    assert!(store_b.has_current(&right_id).expect("right Keep remains"));
    assert_eq!(service_b.cached_session_count().await, 1);
    assert_eq!(
        final_right.profile_generation.0,
        right_updated.profile_generation.0 + 1
    );
    assert_eq!(
        service_b.profiles_snapshot().await,
        load_path(&path).expect("final disk").config.profiles
    );
}

#[tokio::test]
async fn failed_precommit_replace_does_not_update_secret_store() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    let store = Arc::new(SessionSecretStore::default());
    let writer = ConfigWriter::with_fault_injector(Arc::new(FailAt(MutationFailpoint::MainWrite)));
    let service = ApplicationService::with_dependencies(
        &path,
        Arc::new(FakeConnector::new(false)),
        Arc::new(MissingEnvironment),
        store.clone(),
        writer,
    )
    .expect("service");
    let mut session_draft = draft(DriverKind::Redis);
    session_draft.credential_mode = CredentialMode::Session;
    let result = service
        .create_profile(create_request(
            DraftId(60),
            OperationId(60),
            Some("session"),
            session_draft,
            SessionSecretUpdate::Replace(Arc::new(SessionSecret::new("must-not-store".to_owned()))),
        ))
        .await;
    assert!(matches!(result, Err(ServiceError::Config(_))));
    assert!(store.is_empty().expect("store unchanged"));
    assert!(!path.exists());
}

#[tokio::test]
async fn draft_test_consumes_only_a_pre_resolved_secret_capability() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    let connector = Arc::new(FakeConnector::new(false));
    let service = service(&path, connector.clone(), ConfigWriter::default());
    let buffer = ReplacementSecretBuffer::new("one-shot-secret".to_owned());
    let mut session_draft = draft(DriverKind::Redis);
    session_draft.credential_mode = CredentialMode::Session;

    let request = service
        .prepare_replacement_draft_test(
            DraftId(70),
            OperationId(70),
            session_draft,
            &buffer,
            Duration::from_secs(1),
        )
        .expect("service prepares typed SessionReplace capability");
    service
        .test_draft_connection(request)
        .await
        .expect("draft test");

    assert_eq!(buffer.as_str(), "one-shot-secret");
    assert_eq!(connector.connects.load(Ordering::SeqCst), 1);
    assert_eq!(connector.session.closes.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn environment_draft_uses_only_exact_resolver_capability_and_has_typed_missing_recovery() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    let connector = Arc::new(FakeConnector::new(false));
    let resolved = Arc::new(FixedEnvironment::default());
    let service = ApplicationService::with_dependencies(
        &path,
        connector.clone(),
        resolved.clone(),
        Arc::new(SessionSecretStore::default()),
        ConfigWriter::default(),
    )
    .expect("service");
    let mut environment_draft = draft(DriverKind::Redis);
    environment_draft.credential_mode = CredentialMode::Environment;
    environment_draft.secret_env = Some("EXACT_ENV_NAME".to_owned());
    let request = service
        .prepare_environment_draft_test(
            DraftId(701),
            OperationId(701),
            environment_draft.clone(),
            Duration::from_secs(1),
        )
        .expect("exact environment name resolves");
    service
        .test_draft_connection(request)
        .await
        .expect("typed EnvironmentResolved capability is accepted");
    assert_eq!(resolved.resolutions.load(Ordering::SeqCst), 1);
    assert_eq!(connector.connects.load(Ordering::SeqCst), 1);
    assert!(connector.saw_secret.load(Ordering::SeqCst));

    let replacement = ReplacementSecretBuffer::new("wrong-source-secret".to_owned());
    let wrong_source = service
        .prepare_replacement_draft_test(
            DraftId(702),
            OperationId(702),
            environment_draft,
            &replacement,
            Duration::from_secs(1),
        )
        .expect_err("Environment draft cannot consume a Session replacement source");
    assert_eq!(
        wrong_source.public_error_parts(),
        (
            PublicSummary::InvalidInput,
            PublicCode::Field(ProfileFieldId::CredentialMode)
        )
    );
    assert_eq!(
        recovery_for(
            OperationKind::TestDraftConnection,
            wrong_source.public_summary(),
            wrong_source.public_code(),
            &SafeContext::draft(DraftId(702), OperationId(702)),
        )
        .expect("wrong source has exact draft recovery")
        .as_slice(),
        &[RecoveryAction::EditDraft(
            DraftId(702),
            ProfileFieldId::CredentialMode,
        )]
    );
    assert_eq!(connector.connects.load(Ordering::SeqCst), 1);

    let missing_connector = Arc::new(FakeConnector::new(false));
    let missing = ApplicationService::with_dependencies(
        directory.path().join("missing.toml"),
        missing_connector.clone(),
        Arc::new(MissingEnvironment),
        Arc::new(SessionSecretStore::default()),
        ConfigWriter::default(),
    )
    .expect("missing-env service");
    let mut missing_draft = draft(DriverKind::Redis);
    missing_draft.credential_mode = CredentialMode::Environment;
    missing_draft.secret_env = Some("MISSING_SECRET_ENV_SENTINEL".to_owned());
    let error = missing
        .prepare_environment_draft_test(
            DraftId(703),
            OperationId(704),
            missing_draft,
            Duration::from_secs(1),
        )
        .expect_err("missing environment is typed to its originating draft");
    assert!(matches!(
        &error,
        ServiceError::DraftCredentialRequired {
            draft_id: DraftId(703),
            operation_id: OperationId(704),
            code: PublicCode::CredentialEnvironmentName,
        }
    ));
    assert_eq!(
        recovery_for(
            OperationKind::TestDraftConnection,
            error.public_summary(),
            error.public_code(),
            &SafeContext::draft(DraftId(703), OperationId(704)),
        )
        .expect("environment recovery")
        .as_slice(),
        &[RecoveryAction::EditDraft(
            DraftId(703),
            ProfileFieldId::CredentialEnvironmentName,
        )]
    );
    assert_eq!(missing_connector.connects.load(Ordering::SeqCst), 0);
    assert!(!format!("{error:?}").contains("MISSING_SECRET_ENV_SENTINEL"));
}

#[tokio::test]
async fn actual_secret_failures_reach_exact_nonempty_public_recovery_rows() {
    let directory = tempfile::tempdir().expect("tempdir");

    let environment_path = directory.path().join("environment.toml");
    let mut environment_profile = dbotter::model::ConnectionProfile::from_draft(
        "environment".to_owned(),
        named_draft(DriverKind::MySql, "Environment"),
    );
    environment_profile.credential_mode = CredentialMode::Environment;
    environment_profile.secret_env = Some("MISSING_SAVED_ENV".to_owned());
    fs::write(
        &environment_path,
        toml::to_string(&Config {
            version: 2,
            profiles: vec![environment_profile],
        })
        .expect("environment fixture"),
    )
    .expect("environment write");
    let environment_connector = Arc::new(FakeConnector::new(false));
    let environment_service = ApplicationService::with_dependencies(
        &environment_path,
        environment_connector.clone(),
        Arc::new(MissingEnvironment),
        Arc::new(SessionSecretStore::default()),
        ConfigWriter::default(),
    )
    .expect("environment service");
    let environment_id = ProfileId("environment".to_owned());
    let environment_error = environment_service
        .check(
            OperationId(710),
            environment_id.clone(),
            Duration::from_secs(1),
        )
        .await
        .expect_err("missing saved environment fails before connector");
    assert!(matches!(
        &environment_error,
        ServiceError::Secret(SecretError::MissingEnv(_))
    ));
    assert_eq!(
        environment_error.public_error_parts(),
        (
            PublicSummary::AuthenticationFailed,
            PublicCode::CredentialEnvironmentName,
        )
    );
    assert_eq!(
        recovery_for(
            OperationKind::ConnectProfile,
            environment_error.public_summary(),
            environment_error.public_code(),
            &SafeContext::profile(environment_id.clone(), OperationId(710)),
        )
        .expect("saved environment recovery")
        .as_slice(),
        &[RecoveryAction::EditProfile(
            environment_id,
            ProfileFieldId::CredentialEnvironmentName,
        )]
    );
    assert_eq!(environment_connector.connects.load(Ordering::SeqCst), 0);

    let session_path = directory.path().join("session.toml");
    let mut session_profile = dbotter::model::ConnectionProfile::from_draft(
        "session".to_owned(),
        named_draft(DriverKind::MySql, "Session"),
    );
    session_profile.credential_mode = CredentialMode::Session;
    fs::write(
        &session_path,
        toml::to_string(&Config {
            version: 2,
            profiles: vec![session_profile],
        })
        .expect("session fixture"),
    )
    .expect("session write");
    let session_connector = Arc::new(FakeConnector::new(false));
    let session_service = ApplicationService::with_dependencies(
        &session_path,
        session_connector.clone(),
        Arc::new(MissingEnvironment),
        Arc::new(SessionSecretStore::default()),
        ConfigWriter::default(),
    )
    .expect("session service");
    let session_id = ProfileId("session".to_owned());
    let session_error = session_service
        .check(OperationId(711), session_id.clone(), Duration::from_secs(1))
        .await
        .expect_err("missing Session Arc fails before connector");
    assert!(matches!(
        &session_error,
        ServiceError::Secret(SecretError::SessionCredentialRequired)
    ));
    assert_eq!(
        session_error.public_error_parts(),
        (
            PublicSummary::CredentialRequired,
            PublicCode::SessionCredential,
        )
    );
    assert_eq!(
        recovery_for(
            OperationKind::ConnectProfile,
            session_error.public_summary(),
            session_error.public_code(),
            &SafeContext::profile(session_id.clone(), OperationId(711)),
        )
        .expect("saved Session recovery")
        .as_slice(),
        &[
            RecoveryAction::OpenCredentialPrompt(session_id.clone()),
            RecoveryAction::EditProfile(session_id, ProfileFieldId::SessionCredential),
        ]
    );
    assert_eq!(session_connector.connects.load(Ordering::SeqCst), 0);

    let draft_connector = Arc::new(FakeConnector::new(false));
    let draft_service = service(
        &directory.path().join("draft.toml"),
        draft_connector.clone(),
        ConfigWriter::default(),
    );
    let mut session_draft = named_draft(DriverKind::MySql, "Draft Session");
    session_draft.credential_mode = CredentialMode::Session;
    let empty = ReplacementSecretBuffer::default();
    let empty_error = draft_service
        .prepare_replacement_draft_test(
            DraftId(712),
            OperationId(712),
            session_draft,
            &empty,
            Duration::from_secs(1),
        )
        .expect_err("empty replacement is contextualized with originating ids");
    assert!(matches!(
        &empty_error,
        ServiceError::DraftCredentialRequired {
            draft_id: DraftId(712),
            operation_id: OperationId(712),
            code: PublicCode::SessionCredential,
        }
    ));
    assert_eq!(
        recovery_for(
            OperationKind::TestDraftConnection,
            empty_error.public_summary(),
            empty_error.public_code(),
            &SafeContext::draft(DraftId(712), OperationId(712)),
        )
        .expect("empty replacement recovery")
        .as_slice(),
        &[RecoveryAction::EditDraft(
            DraftId(712),
            ProfileFieldId::SessionCredential,
        )]
    );
    assert_eq!(draft_connector.connects.load(Ordering::SeqCst), 0);

    let intent_connector = Arc::new(FakeConnector::new(false));
    let intent_service = service(
        &directory.path().join("intent.toml"),
        intent_connector.clone(),
        ConfigWriter::default(),
    );
    let create_error = intent_service
        .create_profile(create_request(
            DraftId(715),
            OperationId(715),
            Some("invalid-create-intent"),
            named_draft(DriverKind::MySql, "Invalid create intent"),
            SessionSecretUpdate::Keep,
        ))
        .await
        .expect_err("Create Keep without Session mode is invalid input");
    assert_eq!(
        create_error.public_error_parts(),
        (PublicSummary::InvalidInput, PublicCode::SessionCredential)
    );
    assert_eq!(
        recovery_for(
            OperationKind::CreateProfile,
            create_error.public_summary(),
            create_error.public_code(),
            &SafeContext::draft(DraftId(715), OperationId(715)),
        )
        .expect("Create invalid intent recovery")
        .as_slice(),
        &[RecoveryAction::EditDraft(
            DraftId(715),
            ProfileFieldId::SessionCredential,
        )]
    );
    let valid = intent_service
        .create_profile(create_request(
            DraftId(716),
            OperationId(716),
            Some("invalid-update-intent"),
            named_draft(DriverKind::MySql, "Valid None"),
            SessionSecretUpdate::Clear,
        ))
        .await
        .expect("seed valid None profile");
    let mut enter_session = named_draft(DriverKind::MySql, "Enter Session");
    enter_session.credential_mode = CredentialMode::Session;
    let update_error = intent_service
        .update_profile(UpdateProfileRequest {
            profile_id: valid.profile_id.clone(),
            expected_generation: valid.profile_generation,
            operation_id: OperationId(717),
            draft: enter_session,
            secret_update: SessionSecretUpdate::Keep,
            migration_consent: MigrationConsent::Cancelled,
        })
        .await
        .expect_err("Update Keep without current Arc is invalid input");
    assert_eq!(
        update_error.public_error_parts(),
        (PublicSummary::InvalidInput, PublicCode::SessionCredential)
    );
    assert_eq!(
        recovery_for(
            OperationKind::UpdateProfile,
            update_error.public_summary(),
            update_error.public_code(),
            &SafeContext::profile(valid.profile_id.clone(), OperationId(717)),
        )
        .expect("Update invalid intent recovery")
        .as_slice(),
        &[RecoveryAction::EditProfile(
            valid.profile_id,
            ProfileFieldId::SessionCredential,
        )]
    );
    assert_eq!(intent_connector.connects.load(Ordering::SeqCst), 0);
}

#[test]
fn every_bare_secret_error_has_a_canonical_public_pair_and_nonempty_recovery_or_wrapper_rule() {
    let profile_id = ProfileId("secret-errors".to_owned());
    let cases = [
        (
            ServiceError::Secret(SecretError::MissingEnv("REDACTED".to_owned())),
            PublicSummary::AuthenticationFailed,
            PublicCode::CredentialEnvironmentName,
        ),
        (
            ServiceError::Secret(SecretError::EmptyEnv("REDACTED".to_owned())),
            PublicSummary::AuthenticationFailed,
            PublicCode::CredentialEnvironmentName,
        ),
        (
            ServiceError::Secret(SecretError::ReplacementRequired),
            PublicSummary::CredentialRequired,
            PublicCode::SessionCredential,
        ),
        (
            ServiceError::Secret(SecretError::SessionCredentialRequired),
            PublicSummary::CredentialRequired,
            PublicCode::SessionCredential,
        ),
        (
            ServiceError::Secret(SecretError::StoreUnavailable),
            PublicSummary::InternalFailure,
            PublicCode::None,
        ),
    ];
    for (error, summary, code) in cases {
        assert_eq!(error.public_error_parts(), (summary, code));
        assert!(
            recovery_for(
                OperationKind::ConnectProfile,
                summary,
                code,
                &SafeContext::profile(profile_id.clone(), OperationId(713)),
            )
            .is_ok(),
            "pair={summary:?}/{code:?}"
        );
    }
    let invalid_intent = ServiceError::Secret(SecretError::InvalidSessionIntent);
    assert_eq!(
        invalid_intent.public_error_parts(),
        (
            PublicSummary::InvalidInput,
            PublicCode::Field(ProfileFieldId::CredentialMode),
        )
    );
    assert!(
        recovery_for(
            OperationKind::UpdateProfile,
            invalid_intent.public_summary(),
            invalid_intent.public_code(),
            &SafeContext::profile(profile_id, OperationId(714)),
        )
        .is_ok(),
        "service entry points normally replace this bare error with a contextual field error"
    );
}

#[tokio::test]
async fn keep_requires_persisted_session_mode_and_a_current_store_arc_before_write() {
    let directory = tempfile::tempdir().expect("tempdir");
    let none_path = directory.path().join("none.toml");
    let none_service = service(
        &none_path,
        Arc::new(FakeConnector::new(false)),
        ConfigWriter::default(),
    );
    let none = none_service
        .create_profile(create_request(
            DraftId(71),
            OperationId(71),
            Some("none"),
            draft(DriverKind::MySql),
            SessionSecretUpdate::Clear,
        ))
        .await
        .expect("none create");
    let mut enter_session = draft(DriverKind::MySql);
    enter_session.credential_mode = CredentialMode::Session;
    let before = fs::read(&none_path).expect("before invalid Keep");
    assert!(matches!(
        none_service
            .update_profile(UpdateProfileRequest {
                profile_id: none.profile_id,
                expected_generation: none.profile_generation,
                operation_id: OperationId(72),
                draft: enter_session,
                secret_update: SessionSecretUpdate::Keep,
                migration_consent: MigrationConsent::Cancelled,
            })
            .await,
        Err(ServiceError::InvalidProfile(
            ProfileValidationError::Field {
                field: ProfileFieldId::SessionCredential,
                code: PublicCode::SessionCredential,
            }
        ))
    ));
    assert_eq!(fs::read(&none_path).expect("unchanged"), before);

    let restart_path = directory.path().join("restart.toml");
    let first = service(
        &restart_path,
        Arc::new(FakeConnector::new(false)),
        ConfigWriter::default(),
    );
    let mut session_draft = draft(DriverKind::MySql);
    session_draft.credential_mode = CredentialMode::Session;
    first
        .create_profile(create_request(
            DraftId(73),
            OperationId(73),
            Some("restart"),
            session_draft.clone(),
            SessionSecretUpdate::Clear,
        ))
        .await
        .expect("session without secret");
    drop(first);
    let restarted = service(
        &restart_path,
        Arc::new(FakeConnector::new(false)),
        ConfigWriter::default(),
    );
    let generation = restarted
        .profile_generation(&ProfileId("restart".to_owned()))
        .await
        .expect("restart generation");
    assert!(matches!(
        restarted
            .update_profile(UpdateProfileRequest {
                profile_id: ProfileId("restart".to_owned()),
                expected_generation: generation,
                operation_id: OperationId(74),
                draft: session_draft,
                secret_update: SessionSecretUpdate::Keep,
                migration_consent: MigrationConsent::Cancelled,
            })
            .await,
        Err(ServiceError::InvalidProfile(
            ProfileValidationError::Field {
                field: ProfileFieldId::SessionCredential,
                code: PublicCode::SessionCredential,
            }
        ))
    ));

    let keep_path = directory.path().join("keep.toml");
    let keep_service = service(
        &keep_path,
        Arc::new(FakeConnector::new(false)),
        ConfigWriter::default(),
    );
    let mut session_draft = draft(DriverKind::MySql);
    session_draft.credential_mode = CredentialMode::Session;
    let created = keep_service
        .create_profile(create_request(
            DraftId(75),
            OperationId(75),
            Some("keep"),
            session_draft.clone(),
            SessionSecretUpdate::Replace(Arc::new(SessionSecret::new("current".to_owned()))),
        ))
        .await
        .expect("session with current Arc");
    keep_service
        .update_profile(UpdateProfileRequest {
            profile_id: created.profile_id,
            expected_generation: created.profile_generation,
            operation_id: OperationId(76),
            draft: session_draft,
            secret_update: SessionSecretUpdate::Keep,
            migration_consent: MigrationConsent::Cancelled,
        })
        .await
        .expect("valid Keep");
}

#[tokio::test]
async fn replace_and_clear_evict_cached_session_even_when_profile_fields_are_identical() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    let connector = Arc::new(FakeConnector::new(false));
    let store = Arc::new(SessionSecretStore::default());
    let service = ApplicationService::with_dependencies(
        &path,
        connector,
        Arc::new(MissingEnvironment),
        store.clone(),
        ConfigWriter::default(),
    )
    .expect("service");
    let mut session_draft = draft(DriverKind::MySql);
    session_draft.credential_mode = CredentialMode::Session;
    let created = service
        .create_profile(create_request(
            DraftId(77),
            OperationId(77),
            Some("session"),
            session_draft.clone(),
            SessionSecretUpdate::Replace(Arc::new(SessionSecret::new("first".to_owned()))),
        ))
        .await
        .expect("create");
    service
        .check(
            OperationId(78),
            created.profile_id.clone(),
            Duration::from_secs(1),
        )
        .await
        .expect("cache session");
    assert_eq!(service.cached_session_count().await, 1);

    let replaced = service
        .update_profile(UpdateProfileRequest {
            profile_id: created.profile_id.clone(),
            expected_generation: created.profile_generation,
            operation_id: OperationId(79),
            draft: session_draft.clone(),
            secret_update: SessionSecretUpdate::Replace(Arc::new(SessionSecret::new(
                "second".to_owned(),
            ))),
            migration_consent: MigrationConsent::Cancelled,
        })
        .await
        .expect("replace");
    assert_eq!(service.cached_session_count().await, 0);

    service
        .check(
            OperationId(80),
            replaced.profile_id.clone(),
            Duration::from_secs(1),
        )
        .await
        .expect("cache replacement");
    service
        .update_profile(UpdateProfileRequest {
            profile_id: replaced.profile_id,
            expected_generation: replaced.profile_generation,
            operation_id: OperationId(81),
            draft: session_draft,
            secret_update: SessionSecretUpdate::Clear,
            migration_consent: MigrationConsent::Cancelled,
        })
        .await
        .expect("clear");
    assert_eq!(service.cached_session_count().await, 0);
    assert!(store.is_empty().expect("store cleared"));
}

#[tokio::test]
async fn unreadable_post_commit_observation_enters_uncertain_until_exact_path_reload() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("sentinel-config-path.toml");
    dbotter::config::mutate_path(
        &path,
        ConfigMutation::Create(dbotter::model::ConnectionProfile::from_draft(
            "uncertain".to_owned(),
            draft(DriverKind::MySql),
        )),
        MigrationConsent::Cancelled,
    )
    .expect("fixture config");
    let connector = Arc::new(FakeConnector::new(false));
    let store = Arc::new(SessionSecretStore::default());
    store
        .apply(
            &ProfileId("unrelated-secret".to_owned()),
            SessionSecretUpdate::Replace(Arc::new(SessionSecret::new("clear-me".to_owned()))),
        )
        .expect("seed secret");
    let service = ApplicationService::with_dependencies(
        &path,
        connector.clone(),
        Arc::new(MissingEnvironment),
        store.clone(),
        ConfigWriter::with_fault_injector(Arc::new(FailAt(MutationFailpoint::MainObservationLoad))),
    )
    .expect("service");
    service
        .check(
            OperationId(82),
            ProfileId("uncertain".to_owned()),
            Duration::from_secs(1),
        )
        .await
        .expect("cache before mutation");
    let generation = service
        .profile_generation(&ProfileId("uncertain".to_owned()))
        .await
        .expect("generation");

    let error = service
        .update_profile(UpdateProfileRequest {
            profile_id: ProfileId("uncertain".to_owned()),
            expected_generation: generation,
            operation_id: OperationId(83),
            draft: named_draft(DriverKind::MySql, "Committed but unobserved"),
            secret_update: SessionSecretUpdate::Clear,
            migration_consent: MigrationConsent::Cancelled,
        })
        .await
        .expect_err("post-commit observation fails");
    assert!(matches!(error, ServiceError::PostCommitObservation(_)));
    assert_eq!(
        error.public_summary(),
        dbotter::model::PublicSummary::CommittedDurabilityUnknown
    );
    assert!(service.is_config_uncertain());
    assert_eq!(service.cached_session_count().await, 0);
    assert!(store.is_empty().expect("all secrets cleared"));
    assert!(connector.session.closes.load(Ordering::SeqCst) >= 1);

    assert!(matches!(
        service
            .check(
                OperationId(84),
                ProfileId("uncertain".to_owned()),
                Duration::from_secs(1),
            )
            .await,
        Err(ServiceError::ConfigUncertain)
    ));
    assert!(matches!(
        service.prepare_secretless_draft_test(
            DraftId(85),
            OperationId(85),
            draft(DriverKind::Redis),
            Duration::from_secs(1),
        ),
        Err(ServiceError::ConfigUncertain)
    ));

    service
        .reload_configuration()
        .await
        .expect("explicit exact-path reload recovers");
    assert!(!service.is_config_uncertain());
    service
        .check(
            OperationId(86),
            ProfileId("uncertain".to_owned()),
            Duration::from_secs(1),
        )
        .await
        .expect("network re-enabled after reload");
}

#[tokio::test]
async fn create_profile_posture_rewrite_at_observation_fails_closed() {
    const PROFILE_ID: &str = "create-posture-observation";
    const ENDPOINT_SENTINEL: &str = "create-posture-endpoint-sentinel.invalid";
    const SECRET_SENTINEL: &str = "create-posture-secret-sentinel";
    const POSTURE_SENTINEL: &str = "create-posture-request-sentinel";

    let directory = tempfile::tempdir().expect("create posture observation tempdir");
    let path = directory.path().join("config.toml");
    let connector = Arc::new(FakeConnector::new(false));
    let store = Arc::new(SessionSecretStore::default());
    let profile_id = ProfileId(PROFILE_ID.to_owned());
    store
        .apply(
            &profile_id,
            SessionSecretUpdate::Replace(Arc::new(SessionSecret::new(
                "preexisting-create-secret".to_owned(),
            ))),
        )
        .expect("seed preexisting secret");
    let rewrite = Arc::new(RewriteCreatedPostureAtObservation::new(PROFILE_ID));
    let service = ApplicationService::with_dependencies(
        &path,
        connector.clone(),
        Arc::new(MissingEnvironment),
        store.clone(),
        ConfigWriter::with_fault_injector(rewrite.clone()),
    )
    .expect("create posture observation service");
    let before = service.profiles_snapshot().await;
    assert!(before.is_empty());
    assert_eq!(service.source_version().await, ConfigSourceVersion::Missing);

    let mut requested = named_draft(DriverKind::MySql, POSTURE_SENTINEL);
    requested.host = ENDPOINT_SENTINEL.to_owned();
    requested.environment = ProfileEnvironment::Development;
    requested.access = ProfileAccess::ReadOnly;
    requested.credential_mode = CredentialMode::Session;
    let error = match service
        .create_profile(create_request(
            DraftId(923),
            OperationId(923),
            Some(PROFILE_ID),
            requested,
            SessionSecretUpdate::Replace(Arc::new(SessionSecret::new(SECRET_SENTINEL.to_owned()))),
        ))
        .await
    {
        Err(error) => error,
        Ok(_) => panic!("observed posture rewrite was accepted as the requested posture"),
    };

    assert!(matches!(&error, ServiceError::ConfigUncertain));
    assert_eq!(
        error.public_error_parts(),
        (
            PublicSummary::ResourceStale,
            PublicCode::ConfigExternalChange
        )
    );
    assert!(service.is_config_uncertain());
    assert_eq!(service.profiles_snapshot().await, before);
    assert_eq!(service.source_version().await, ConfigSourceVersion::Missing);
    assert_eq!(service.cached_session_count().await, 0);
    assert_eq!(connector.connects.load(Ordering::SeqCst), 0);
    assert!(
        store
            .is_empty()
            .expect("uncertain create clears all secrets")
    );
    assert!(
        !service
            .has_current_session_secret(&profile_id)
            .expect("rewritten profile secret remains absent")
    );

    let disk = load_path(&path).expect("externally rewritten v3 remains valid");
    assert_eq!(disk.source_version, ConfigSourceVersion::V3);
    let persisted = disk
        .config
        .profiles
        .iter()
        .find(|profile| profile.id == PROFILE_ID)
        .expect("rewritten profile remains on disk");
    assert_eq!(persisted.host, ENDPOINT_SENTINEL);
    assert_eq!(persisted.name, POSTURE_SENTINEL);
    assert_eq!(persisted.credential_mode, CredentialMode::Session);
    assert!(matches!(
        persisted.safety,
        ProfileSafetyPosture::Classified {
            environment: ProfileEnvironment::Production,
            access: ProfileAccess::ReadWrite,
            instance_id,
        } if instance_id == rewrite.preserved_instance_id()
    ));

    let public_surfaces = format!(
        "{error:?}\n{error}\n{:?}\n{:?}\n{}",
        error.public_summary(),
        error.public_code(),
        error.public_summary()
    );
    for forbidden in [
        PROFILE_ID,
        ENDPOINT_SENTINEL,
        SECRET_SENTINEL,
        POSTURE_SENTINEL,
        "development",
        "read-only",
        "production",
        "read-write",
    ] {
        assert!(
            !public_surfaces.contains(forbidden),
            "create observation failure leaked a profile, endpoint, secret, or posture sentinel"
        );
    }
}

#[tokio::test]
async fn create_profile_instance_id_rewrite_at_observation_fails_closed() {
    const PROFILE_ID: &str = "create-instance-observation";
    const ENDPOINT_SENTINEL: &str = "create-instance-endpoint-sentinel.invalid";
    const SECRET_SENTINEL: &str = "create-instance-secret-sentinel";
    const PROFILE_SENTINEL: &str = "create-instance-request-sentinel";

    let directory = tempfile::tempdir().expect("create instance observation tempdir");
    let path = directory.path().join("config.toml");
    let connector = Arc::new(FakeConnector::new(false));
    let store = Arc::new(SessionSecretStore::default());
    let profile_id = ProfileId(PROFILE_ID.to_owned());
    store
        .apply(
            &profile_id,
            SessionSecretUpdate::Replace(Arc::new(SessionSecret::new(
                "preexisting-instance-secret".to_owned(),
            ))),
        )
        .expect("seed preexisting secret");
    let rewrite = Arc::new(RewriteCreatedInstanceIdAtObservation::new(PROFILE_ID));
    let service = ApplicationService::with_dependencies(
        &path,
        connector.clone(),
        Arc::new(MissingEnvironment),
        store.clone(),
        ConfigWriter::with_fault_injector(rewrite.clone()),
    )
    .expect("create instance observation service");
    let before = service.profiles_snapshot().await;
    assert!(before.is_empty());
    assert_eq!(service.source_version().await, ConfigSourceVersion::Missing);

    let mut requested = named_draft(DriverKind::MySql, PROFILE_SENTINEL);
    requested.host = ENDPOINT_SENTINEL.to_owned();
    requested.environment = ProfileEnvironment::Development;
    requested.access = ProfileAccess::ReadOnly;
    requested.credential_mode = CredentialMode::Session;
    let expected_draft = requested.clone();
    let error = match service
        .create_profile(create_request(
            DraftId(924),
            OperationId(924),
            Some(PROFILE_ID),
            requested,
            SessionSecretUpdate::Replace(Arc::new(SessionSecret::new(SECRET_SENTINEL.to_owned()))),
        ))
        .await
    {
        Err(error) => error,
        Ok(_) => panic!("observed instance id rewrite was accepted as the created identity"),
    };

    assert!(matches!(&error, ServiceError::ConfigUncertain));
    assert_eq!(
        error.public_error_parts(),
        (
            PublicSummary::ResourceStale,
            PublicCode::ConfigExternalChange
        )
    );
    assert!(service.is_config_uncertain());
    assert_eq!(service.profiles_snapshot().await, before);
    assert!(matches!(
        service.profile_generation(&profile_id).await,
        Err(ServiceError::UnknownProfile(unknown)) if unknown == profile_id
    ));
    assert_eq!(service.source_version().await, ConfigSourceVersion::Missing);
    assert_eq!(service.cached_session_count().await, 0);
    assert_eq!(connector.connects.load(Ordering::SeqCst), 0);
    assert!(
        store
            .is_empty()
            .expect("uncertain instance rewrite clears all secrets")
    );
    assert!(
        !service
            .has_current_session_secret(&profile_id)
            .expect("substituted profile secret remains absent")
    );

    let disk = load_path(&path).expect("instance-rewritten v3 remains valid");
    assert_eq!(disk.source_version, ConfigSourceVersion::V3);
    let persisted = disk
        .config
        .profiles
        .iter()
        .find(|profile| profile.id == PROFILE_ID)
        .expect("instance-rewritten profile remains on disk");
    assert_eq!(persisted.as_draft(), expected_draft);
    let persisted_instance_id = persisted
        .safety
        .instance_id()
        .expect("rewritten profile remains classified");
    assert_eq!(persisted_instance_id, rewrite.substituted_instance_id());
    assert_ne!(persisted_instance_id, rewrite.original_instance_id());
    assert_eq!(
        ProfileInstanceId::parse(&persisted_instance_id.to_string()),
        Ok(persisted_instance_id)
    );

    let public_surfaces = format!(
        "{error:?}\n{error}\n{:?}\n{:?}\n{}",
        error.public_summary(),
        error.public_code(),
        error.public_summary()
    );
    for forbidden in [
        PROFILE_ID,
        ENDPOINT_SENTINEL,
        SECRET_SENTINEL,
        PROFILE_SENTINEL,
    ] {
        assert!(
            !public_surfaces.contains(forbidden),
            "create instance observation failure leaked a profile, endpoint, or secret sentinel"
        );
    }
}

#[tokio::test]
async fn create_observation_rejects_existing_instance_identity_transplant() {
    assert_identity_transplant_fails_closed(MatrixMutation::Create).await;
}

#[tokio::test]
async fn update_observation_rejects_existing_instance_identity_transplant() {
    assert_identity_transplant_fails_closed(MatrixMutation::Update).await;
}

#[tokio::test]
async fn delete_observation_rejects_deleted_instance_identity_transplant() {
    assert_identity_transplant_fails_closed(MatrixMutation::Delete).await;
}

#[tokio::test]
async fn create_reconciliation_rejects_prewrite_identity_transplant() {
    assert_prewrite_identity_transplant_fails_closed(MatrixMutation::Create).await;
}

#[tokio::test]
async fn update_reconciliation_rejects_prewrite_identity_transplant() {
    assert_prewrite_identity_transplant_fails_closed(MatrixMutation::Update).await;
}

#[tokio::test]
async fn delete_reconciliation_rejects_prewrite_identity_transplant() {
    assert_prewrite_identity_transplant_fails_closed(MatrixMutation::Delete).await;
}

async fn assert_prewrite_identity_transplant_fails_closed(mutation: MatrixMutation) {
    const TARGET_ID: &str = "prewrite-transplant-target";
    const SOURCE_ID: &str = "prewrite-transplant-source";
    const TRANSPLANTED_ID: &str = "prewrite-transplant-destination";

    let directory = tempfile::tempdir().expect("prewrite identity transplant tempdir");
    let path = directory.path().join("config.toml");
    let target = classified_profile(
        TARGET_ID,
        named_draft(DriverKind::MySql, "Prewrite transplant target"),
        31,
    );
    let source = classified_profile(
        SOURCE_ID,
        named_draft(DriverKind::Redis, "Prewrite transplant source"),
        32,
    );
    let initial_profiles = match mutation {
        MatrixMutation::Create => vec![source],
        MatrixMutation::Update | MatrixMutation::Delete => vec![target, source],
    };
    let persisted = write_v3_profiles(&path, initial_profiles);
    let service = service(
        &path,
        Arc::new(FakeConnector::new(false)),
        ConfigWriter::default(),
    );
    let before = service.profiles_snapshot().await;
    let target_id = ProfileId(TARGET_ID.to_owned());
    let target_generation = match mutation {
        MatrixMutation::Create => None,
        MatrixMutation::Update | MatrixMutation::Delete => Some(
            service
                .profile_generation(&target_id)
                .await
                .expect("prewrite target generation"),
        ),
    };

    let mut externally_transplanted = load_path(&path).expect("prewrite source config").config;
    externally_transplanted
        .profiles
        .iter_mut()
        .find(|profile| profile.id == SOURCE_ID)
        .expect("prewrite transplant source")
        .id = TRANSPLANTED_ID.to_owned();
    fs::write(
        &path,
        toml::to_string(&externally_transplanted).expect("prewrite transplant serialization"),
    )
    .expect("prewrite transplant write");
    validate_config_identity(
        &load_path(&path)
            .expect("strict-valid prewrite transplant")
            .config,
    )
    .expect("prewrite transplant remains strict-valid");

    let result = match mutation {
        MatrixMutation::Create => {
            service
                .create_profile(create_request(
                    DraftId(960),
                    OperationId(960),
                    Some(TARGET_ID),
                    named_draft(DriverKind::MySql, "Created after prewrite transplant"),
                    SessionSecretUpdate::Clear,
                ))
                .await
        }
        MatrixMutation::Update => {
            let mut draft = persisted_profile(&persisted, TARGET_ID).as_draft();
            draft.name = "Updated after prewrite transplant".to_owned();
            service
                .update_profile(UpdateProfileRequest {
                    profile_id: target_id.clone(),
                    expected_generation: target_generation.expect("update generation"),
                    operation_id: OperationId(961),
                    draft,
                    secret_update: SessionSecretUpdate::Clear,
                    migration_consent: MigrationConsent::Cancelled,
                })
                .await
        }
        MatrixMutation::Delete => {
            service
                .delete_profile(DeleteProfileRequest {
                    profile_id: target_id,
                    expected_generation: target_generation.expect("delete generation"),
                    operation_id: OperationId(962),
                    migration_consent: MigrationConsent::Cancelled,
                })
                .await
        }
    };
    let error = result.expect_err("prewrite identity transplant must fence reconciliation");
    assert!(matches!(error, ServiceError::ConfigUncertain));
    assert!(service.is_config_uncertain());
    assert_eq!(service.profiles_snapshot().await, before);

    let disk = load_path(&path)
        .expect("committed prewrite transplant disk")
        .config;
    assert!(
        disk.profiles
            .iter()
            .any(|profile| profile.id == TRANSPLANTED_ID)
    );
    assert!(disk.profiles.iter().all(|profile| profile.id != SOURCE_ID));
}

async fn assert_identity_transplant_fails_closed(mutation: MatrixMutation) {
    const TARGET_ID: &str = "identity-transplant-target";
    const SOURCE_ID: &str = "identity-transplant-source";
    const STABLE_ID: &str = "identity-transplant-stable";
    const TRANSPLANTED_ID: &str = "identity-transplant-destination";
    const SECRET_SENTINEL: &str = "identity-transplant-secret-sentinel";

    let directory = tempfile::tempdir().expect("identity transplant tempdir");
    let path = directory.path().join("config.toml");
    let target = classified_profile(
        TARGET_ID,
        named_draft(DriverKind::MySql, "Identity transplant target"),
        1,
    );
    let source = classified_profile(
        SOURCE_ID,
        named_draft(DriverKind::MySql, "Identity transplant source"),
        2,
    );
    let stable = classified_profile(
        STABLE_ID,
        named_draft(DriverKind::Redis, "Identity transplant stable"),
        3,
    );
    let initial_profiles = match mutation {
        MatrixMutation::Create => vec![source, stable],
        MatrixMutation::Update => vec![target, source, stable],
        MatrixMutation::Delete => vec![target, stable],
    };
    let persisted = write_v3_profiles(&path, initial_profiles);
    let original = match mutation {
        MatrixMutation::Create | MatrixMutation::Update => persisted_profile(&persisted, SOURCE_ID),
        MatrixMutation::Delete => persisted_profile(&persisted, TARGET_ID),
    };
    let original_instance_id = original
        .safety
        .instance_id()
        .expect("transplant source remains classified");
    let deleted_profile = matches!(mutation, MatrixMutation::Delete).then_some(original);
    let rewrite = Arc::new(TransplantProfileInstanceAtObservation::new(
        mutation,
        TARGET_ID,
        SOURCE_ID,
        TRANSPLANTED_ID,
        deleted_profile,
    ));
    let connector = Arc::new(FakeConnector::new(false));
    let store = Arc::new(SessionSecretStore::default());
    let mut seeded_ids = persisted
        .iter()
        .map(|profile| ProfileId(profile.id.clone()))
        .collect::<Vec<_>>();
    seeded_ids.push(ProfileId(TRANSPLANTED_ID.to_owned()));
    seeded_ids.push(ProfileId(TARGET_ID.to_owned()));
    for profile_id in &seeded_ids {
        store
            .apply(
                profile_id,
                SessionSecretUpdate::Replace(Arc::new(SessionSecret::new(
                    SECRET_SENTINEL.to_owned(),
                ))),
            )
            .expect("seed identity transplant secret");
    }
    let service = ApplicationService::with_dependencies(
        &path,
        connector.clone(),
        Arc::new(MissingEnvironment),
        store.clone(),
        ConfigWriter::with_fault_injector(rewrite),
    )
    .expect("identity transplant service");
    for (index, profile) in persisted.iter().enumerate() {
        service
            .check(
                OperationId(940 + index as u64),
                ProfileId(profile.id.clone()),
                Duration::from_secs(1),
            )
            .await
            .expect("cache every pre-mutation profile");
    }
    let before = service.profiles_snapshot().await;
    assert_eq!(service.cached_session_count().await, before.len());
    let target_id = ProfileId(TARGET_ID.to_owned());
    let target_generation = match mutation {
        MatrixMutation::Create => None,
        MatrixMutation::Update | MatrixMutation::Delete => Some(
            service
                .profile_generation(&target_id)
                .await
                .expect("target generation"),
        ),
    };

    let result = match mutation {
        MatrixMutation::Create => {
            let mut draft = named_draft(DriverKind::MySql, "Created transplant target");
            draft.host = "created-transplant-target.internal".to_owned();
            draft.credential_mode = CredentialMode::Session;
            service
                .create_profile(create_request(
                    DraftId(950),
                    OperationId(950),
                    Some(TARGET_ID),
                    draft,
                    SessionSecretUpdate::Replace(Arc::new(SessionSecret::new(
                        SECRET_SENTINEL.to_owned(),
                    ))),
                ))
                .await
        }
        MatrixMutation::Update => {
            let mut draft = persisted_profile(&persisted, TARGET_ID).as_draft();
            draft.name = "Updated transplant target".to_owned();
            draft.host = "updated-transplant-target.internal".to_owned();
            service
                .update_profile(UpdateProfileRequest {
                    profile_id: target_id.clone(),
                    expected_generation: target_generation.expect("update generation"),
                    operation_id: OperationId(951),
                    draft,
                    secret_update: SessionSecretUpdate::Clear,
                    migration_consent: MigrationConsent::Cancelled,
                })
                .await
        }
        MatrixMutation::Delete => {
            service
                .delete_profile(DeleteProfileRequest {
                    profile_id: target_id.clone(),
                    expected_generation: target_generation.expect("delete generation"),
                    operation_id: OperationId(952),
                    migration_consent: MigrationConsent::Cancelled,
                })
                .await
        }
    };
    let error = match result {
        Err(error) => error,
        Ok(_) => panic!("{mutation:?} accepted a transplanted immutable profile identity"),
    };

    assert!(matches!(&error, ServiceError::ConfigUncertain));
    assert_eq!(
        error.public_error_parts(),
        (
            PublicSummary::ResourceStale,
            PublicCode::ConfigExternalChange
        )
    );
    assert!(service.is_config_uncertain());
    assert_eq!(service.profiles_snapshot().await, before);
    assert!(
        service
            .profiles_snapshot()
            .await
            .iter()
            .all(|profile| profile.id != TRANSPLANTED_ID)
    );
    assert_eq!(service.source_version().await, ConfigSourceVersion::V3);
    assert_eq!(service.cached_session_count().await, 0);
    assert!(
        store
            .is_empty()
            .expect("identity transplant clears secrets")
    );
    assert!(
        connector.session.closes.load(Ordering::SeqCst) >= before.len(),
        "{mutation:?} must close every cached pre-mutation session"
    );

    let reload = service
        .reload_configuration()
        .await
        .expect_err("reload must not accept a transplanted immutable profile identity");
    assert!(matches!(reload, ServiceError::ConfigUncertain));
    assert!(service.is_config_uncertain());
    assert_eq!(service.profiles_snapshot().await, before);

    let disk = load_path(&path).expect("strict-valid transplanted disk remains reloadable");
    assert_eq!(disk.source_version, ConfigSourceVersion::V3);
    validate_config_identity(&disk.config).expect("transplanted disk remains identity-valid v3");
    let transplanted = disk
        .config
        .profiles
        .iter()
        .find(|profile| profile.id == TRANSPLANTED_ID)
        .expect("transplanted row remains exact disk truth");
    assert_eq!(
        transplanted.safety.instance_id(),
        Some(original_instance_id)
    );
    assert!(
        disk.config
            .profiles
            .iter()
            .all(|profile| profile.id != SOURCE_ID),
        "{mutation:?} removes the original identity binding from disk"
    );
    match mutation {
        MatrixMutation::Create => {
            let created = disk
                .config
                .profiles
                .iter()
                .find(|profile| profile.id == TARGET_ID)
                .expect("intended create result remains on disk");
            assert_eq!(created.name, "Created transplant target");
            assert_ne!(created.safety.instance_id(), Some(original_instance_id));
        }
        MatrixMutation::Update => {
            let updated = disk
                .config
                .profiles
                .iter()
                .find(|profile| profile.id == TARGET_ID)
                .expect("intended update result remains on disk");
            assert_eq!(updated.name, "Updated transplant target");
            assert_eq!(updated.host, "updated-transplant-target.internal");
        }
        MatrixMutation::Delete => assert!(
            disk.config
                .profiles
                .iter()
                .all(|profile| profile.id != TARGET_ID),
            "intended deleted ProfileId remains absent on disk"
        ),
    }
}

#[tokio::test]
async fn identity_or_source_rewrite_during_observation_is_committed_unknown_and_uncertain() {
    for rewrite in [
        ObservationRewrite::DuplicateId,
        ObservationRewrite::InvalidId,
        ObservationRewrite::V1,
        ObservationRewrite::Missing,
    ] {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("config.toml");
        let target = classified_profile(
            "observation-target",
            named_draft(DriverKind::MySql, "Observation target"),
            1,
        );
        write_v3_profiles(&path, vec![target]);
        let target_id = ProfileId("observation-target".to_owned());
        let connector = Arc::new(FakeConnector::new(false));
        let store = Arc::new(SessionSecretStore::default());
        store
            .apply(
                &target_id,
                SessionSecretUpdate::Replace(Arc::new(SessionSecret::new(
                    "must-clear-on-uncertain".to_owned(),
                ))),
            )
            .expect("seed store");
        let service = ApplicationService::with_dependencies(
            &path,
            connector.clone(),
            Arc::new(MissingEnvironment),
            store.clone(),
            ConfigWriter::with_fault_injector(Arc::new(RewriteAtObservation::new(rewrite))),
        )
        .expect("service");
        service
            .check(OperationId(910), target_id.clone(), Duration::from_secs(1))
            .await
            .expect("cache target");
        let before = service.profiles_snapshot().await;

        let error = service
            .create_profile(create_request(
                DraftId(911),
                OperationId(911),
                Some("committed-before-rewrite"),
                named_draft(DriverKind::Redis, "Committed before rewrite"),
                SessionSecretUpdate::Clear,
            ))
            .await
            .expect_err("invalid exact observation must fail closed");
        let ServiceError::PostCommitObservation(observation) = &error else {
            panic!("rewrite={rewrite:?}, error={error:?}");
        };
        assert_eq!(
            observation.commit_state(),
            CommitState::CommittedDurabilityUnknown,
            "rewrite={rewrite:?}"
        );
        assert_eq!(
            error.public_summary(),
            PublicSummary::CommittedDurabilityUnknown
        );
        assert!(service.is_config_uncertain(), "rewrite={rewrite:?}");
        assert_eq!(service.profiles_snapshot().await, before);
        assert_eq!(service.source_version().await, ConfigSourceVersion::V3);
        assert_eq!(service.cached_session_count().await, 0);
        assert!(store.is_empty().expect("uncertain clears all secrets"));
        assert!(connector.session.closes.load(Ordering::SeqCst) >= 1);

        let disk = load_path(&path).expect("rewritten disk remains exact truth");
        match rewrite {
            ObservationRewrite::DuplicateId => {
                assert_eq!(disk.source_version, ConfigSourceVersion::V3);
                assert_eq!(disk.config.profiles[0].id, disk.config.profiles[2].id);
            }
            ObservationRewrite::InvalidId => {
                assert_eq!(disk.source_version, ConfigSourceVersion::V3);
                assert!(
                    disk.config
                        .profiles
                        .iter()
                        .any(|profile| { profile.id == " invalid-observed-id" })
                );
            }
            ObservationRewrite::V1 => {
                assert_eq!(disk.source_version, ConfigSourceVersion::V1);
            }
            ObservationRewrite::Missing => {
                assert_eq!(disk.source_version, ConfigSourceVersion::Missing);
            }
            ObservationRewrite::BlankHost => unreachable!(),
        }
    }
}

#[tokio::test]
async fn semantic_invalid_observation_is_visible_fenced_and_recoverable_without_uncertainty() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    let target = classified_profile(
        "semantic-target",
        named_draft(DriverKind::MySql, "Semantic target"),
        1,
    );
    write_v3_profiles(&path, vec![target]);
    let connector = Arc::new(FakeConnector::new(false));
    let service = ApplicationService::with_dependencies(
        &path,
        connector.clone(),
        Arc::new(MissingEnvironment),
        Arc::new(SessionSecretStore::default()),
        ConfigWriter::with_fault_injector(Arc::new(RewriteAtObservation::new(
            ObservationRewrite::BlankHost,
        ))),
    )
    .expect("service");
    service
        .check(
            OperationId(920),
            ProfileId("semantic-target".to_owned()),
            Duration::from_secs(1),
        )
        .await
        .expect("cache unchanged target");
    let connects_before = connector.connects.load(Ordering::SeqCst);

    service
        .create_profile(create_request(
            DraftId(921),
            OperationId(921),
            Some("created-with-semantic-observation"),
            named_draft(DriverKind::Redis, "Created"),
            SessionSecretUpdate::Clear,
        ))
        .await
        .expect("identity-valid semantic observation is accepted");
    assert!(!service.is_config_uncertain());
    assert_eq!(service.source_version().await, ConfigSourceVersion::V3);
    let disk = load_path(&path).expect("disk observation").config;
    assert_eq!(service.profiles_snapshot().await, disk.profiles);
    let invalid_id = ProfileId("semantic-invalid-observed".to_owned());
    service
        .profile_generation(&invalid_id)
        .await
        .expect("semantic-invalid row receives a generation fence");
    let error = service
        .check(OperationId(922), invalid_id.clone(), Duration::from_secs(1))
        .await
        .expect_err("blank host is fenced before connector use");
    assert!(matches!(
        error,
        ServiceError::InvalidProfile(ProfileValidationError::Field {
            field: ProfileFieldId::Host,
            ..
        })
    ));
    assert_eq!(connector.connects.load(Ordering::SeqCst), connects_before);
    assert_eq!(service.cached_session_count().await, 1);
    assert_eq!(
        recovery_for(
            OperationKind::ConnectProfile,
            error.public_summary(),
            error.public_code(),
            &SafeContext::profile(invalid_id.clone(), OperationId(922)),
        )
        .expect("semantic invalid row has typed edit recovery")
        .as_slice(),
        &[RecoveryAction::EditProfile(
            invalid_id,
            ProfileFieldId::Host,
        )]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn observed_profile_generation_snapshot_publishes_only_all_old_or_all_new() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    let old_profile = classified_profile(
        "atomic-profile",
        named_draft(DriverKind::MySql, "All old"),
        1,
    );
    let persisted = write_v3_profiles(&path, vec![old_profile]);
    let old_profile = persisted_profile(&persisted, "atomic-profile");
    let observation = Arc::new(MainObservationBarrier::new());
    let service = ApplicationService::with_dependencies(
        &path,
        Arc::new(FakeConnector::new(false)),
        Arc::new(MissingEnvironment),
        Arc::new(SessionSecretStore::default()),
        ConfigWriter::with_fault_injector(observation.clone()),
    )
    .expect("service");
    let profile_id = ProfileId("atomic-profile".to_owned());
    let old_generation = service
        .profile_generation(&profile_id)
        .await
        .expect("old generation");
    let mut new_draft = old_profile.as_draft();
    new_draft.name = "All new".to_owned();
    let updating = service.clone();
    let update_profile_id = profile_id.clone();
    let update = tokio::spawn(async move {
        updating
            .update_profile(UpdateProfileRequest {
                profile_id: update_profile_id,
                expected_generation: old_generation,
                operation_id: OperationId(930),
                draft: new_draft,
                secret_update: SessionSecretUpdate::Clear,
                migration_consent: MigrationConsent::Cancelled,
            })
            .await
    });
    let waiting = observation.clone();
    tokio::task::spawn_blocking(move || waiting.wait_until_entered())
        .await
        .expect("observation wait joins");

    assert_eq!(
        service.profiles_with_generations_snapshot().await,
        vec![(old_profile.clone(), old_generation)],
        "committed disk is not published before exact observation completes"
    );
    let launch = Arc::new(tokio::sync::Barrier::new(33));
    let mut snapshots = Vec::new();
    for _ in 0..32 {
        let service = service.clone();
        let launch = launch.clone();
        snapshots.push(tokio::spawn(async move {
            launch.wait().await;
            service.profiles_with_generations_snapshot().await
        }));
    }
    launch.wait().await;
    observation.release();
    let updated = update
        .await
        .expect("update task joins")
        .expect("update succeeds");
    let new_profile = service
        .profiles_snapshot()
        .await
        .into_iter()
        .next()
        .expect("new profile");
    assert_eq!(new_profile.name, "All new");
    for snapshot in snapshots {
        let snapshot = snapshot.await.expect("snapshot joins");
        assert_eq!(snapshot.len(), 1);
        let (profile, generation) = &snapshot[0];
        assert!(
            (profile == &old_profile && *generation == old_generation)
                || (profile == &new_profile && *generation == updated.profile_generation),
            "mixed profile/generation tuple: {snapshot:?}"
        );
    }
    assert_eq!(
        service.profiles_with_generations_snapshot().await,
        vec![(new_profile.clone(), updated.profile_generation)]
    );

    let before_stale = fs::read(&path).expect("before stale write");
    let stale = service
        .update_profile(UpdateProfileRequest {
            profile_id,
            expected_generation: old_generation,
            operation_id: OperationId(931),
            draft: old_profile.as_draft(),
            secret_update: SessionSecretUpdate::Clear,
            migration_consent: MigrationConsent::Cancelled,
        })
        .await
        .expect_err("old snapshot generation cannot overwrite new profile");
    assert!(matches!(stale, ServiceError::ProfileStale { .. }));
    assert_eq!(
        fs::read(&path).expect("stale write has no effect"),
        before_stale
    );
}

#[tokio::test]
async fn connect_result_cannot_insert_an_old_profile_after_update_publish() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    let old_profile = classified_profile(
        "connect-race",
        named_draft(DriverKind::MySql, "Connect race"),
        1,
    );
    let persisted = write_v3_profiles(&path, vec![old_profile]);
    let old_profile = persisted_profile(&persisted, "connect-race");
    let connector = Arc::new(ConnectBarrierConnector::new());
    let service = ApplicationService::with_dependencies(
        &path,
        connector.clone(),
        Arc::new(MissingEnvironment),
        Arc::new(SessionSecretStore::default()),
        ConfigWriter::default(),
    )
    .expect("service");
    let profile_id = ProfileId("connect-race".to_owned());
    let generation = service
        .profile_generation(&profile_id)
        .await
        .expect("old generation");
    let checking = service.clone();
    let check_profile_id = profile_id.clone();
    let check = tokio::spawn(async move {
        checking
            .check(OperationId(940), check_profile_id, Duration::from_secs(1))
            .await
    });
    connector.gate.wait_until_entered().await;

    let mut changed = old_profile.as_draft();
    changed.host = "new-connect-endpoint.internal".to_owned();
    let updated = service
        .update_profile(UpdateProfileRequest {
            profile_id: profile_id.clone(),
            expected_generation: generation,
            operation_id: OperationId(941),
            draft: changed,
            secret_update: SessionSecretUpdate::Clear,
            migration_consent: MigrationConsent::Cancelled,
        })
        .await
        .expect("update publishes while old connect waits");
    assert_eq!(service.cached_session_count().await, 0);
    connector.gate.release();
    let error = check
        .await
        .expect("check joins")
        .expect_err("old connect result is stale");
    assert!(matches!(error, ServiceError::ProfileStale { .. }));
    assert_eq!(service.cached_session_count().await, 0);
    assert_eq!(connector.session.closes.load(Ordering::SeqCst), 1);
    assert_eq!(connector.connects.load(Ordering::SeqCst), 1);
    assert_eq!(
        service
            .profile_generation(&profile_id)
            .await
            .expect("new generation"),
        updated.profile_generation
    );
}

#[tokio::test]
async fn uncertain_epoch_rejects_pending_connect_before_and_after_successful_reload() {
    for reload_before_release in [false, true] {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("config.toml");
        let profile = classified_profile(
            "uncertain-connect",
            named_draft(DriverKind::MySql, "Uncertain connect"),
            1,
        );
        write_v3_profiles(&path, vec![profile]);
        let profile_id = ProfileId("uncertain-connect".to_owned());
        let connector = Arc::new(ConnectBarrierConnector::new());
        let store = Arc::new(SessionSecretStore::default());
        store
            .apply(
                &profile_id,
                SessionSecretUpdate::Replace(Arc::new(SessionSecret::new(
                    "must-not-resurrect".to_owned(),
                ))),
            )
            .expect("seed secret");
        let service = ApplicationService::with_dependencies(
            &path,
            connector.clone(),
            Arc::new(MissingEnvironment),
            store.clone(),
            ConfigWriter::with_fault_injector(Arc::new(FailOnceAt::new(
                MutationFailpoint::MainObservationLoad,
            ))),
        )
        .expect("service");
        let checking = service.clone();
        let check_profile_id = profile_id.clone();
        let check = tokio::spawn(async move {
            checking
                .check(OperationId(950), check_profile_id, Duration::from_secs(1))
                .await
        });
        connector.gate.wait_until_entered().await;

        let mutation_error = service
            .create_profile(create_request(
                DraftId(951),
                OperationId(951),
                Some("committed-unobserved"),
                named_draft(DriverKind::Redis, "Committed unobserved"),
                SessionSecretUpdate::Clear,
            ))
            .await
            .expect_err("observation failure enters uncertain state");
        assert!(matches!(
            mutation_error,
            ServiceError::PostCommitObservation(_)
        ));
        assert!(service.is_config_uncertain());
        assert_eq!(service.cached_session_count().await, 0);
        assert!(store.is_empty().expect("uncertain clears old Arc"));

        if reload_before_release {
            service
                .reload_configuration()
                .await
                .expect("exact-path reload succeeds");
            assert!(!service.is_config_uncertain());
        }
        connector.gate.release();
        let error = check
            .await
            .expect("pending check joins")
            .expect_err("pre-uncertain connect cannot succeed");
        if reload_before_release {
            assert!(matches!(error, ServiceError::ProfileStale { .. }));
        } else {
            assert!(matches!(error, ServiceError::ConfigUncertain));
        }
        assert_eq!(service.cached_session_count().await, 0);
        assert_eq!(connector.session.closes.load(Ordering::SeqCst), 1);
        assert_eq!(connector.connects.load(Ordering::SeqCst), 1);
        assert!(store.is_empty().expect("old Arc stays cleared"));
    }
}

#[tokio::test]
async fn failed_old_ping_cannot_remove_or_close_a_new_generation_session() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    let old_profile =
        classified_profile("ping-race", named_draft(DriverKind::MySql, "Ping race"), 1);
    let persisted = write_v3_profiles(&path, vec![old_profile]);
    let old_profile = persisted_profile(&persisted, "ping-race");
    let old_ping = Arc::new(AsyncGate::new());
    let old_session = Arc::new(ControlledSession::blocked_failure(old_ping.clone()));
    let new_session = Arc::new(ControlledSession::immediate());
    let connector = Arc::new(SequencedConnector {
        sessions: vec![old_session.clone(), new_session.clone()],
        connects: AtomicUsize::new(0),
    });
    let service = ApplicationService::with_dependencies(
        &path,
        connector.clone(),
        Arc::new(MissingEnvironment),
        Arc::new(SessionSecretStore::default()),
        ConfigWriter::default(),
    )
    .expect("service");
    let profile_id = ProfileId("ping-race".to_owned());
    let old_generation = service
        .profile_generation(&profile_id)
        .await
        .expect("old generation");
    let old_checking = service.clone();
    let old_profile_id = profile_id.clone();
    let old_check = tokio::spawn(async move {
        old_checking
            .check(OperationId(960), old_profile_id, Duration::from_secs(1))
            .await
    });
    old_ping.wait_until_entered().await;

    let mut changed = old_profile.as_draft();
    changed.host = "new-ping-endpoint.internal".to_owned();
    let updated = service
        .update_profile(UpdateProfileRequest {
            profile_id: profile_id.clone(),
            expected_generation: old_generation,
            operation_id: OperationId(961),
            draft: changed,
            secret_update: SessionSecretUpdate::Clear,
            migration_consent: MigrationConsent::Cancelled,
        })
        .await
        .expect("new generation publishes");
    assert_eq!(old_session.closes.load(Ordering::SeqCst), 1);
    service
        .check(OperationId(962), profile_id.clone(), Duration::from_secs(1))
        .await
        .expect("new generation check succeeds");
    assert_eq!(service.cached_session_count().await, 1);
    assert_eq!(new_session.closes.load(Ordering::SeqCst), 0);

    old_ping.release();
    let error = old_check
        .await
        .expect("old check joins")
        .expect_err("old ping completion is stale");
    assert!(matches!(error, ServiceError::ProfileStale { .. }));
    assert_eq!(service.cached_session_count().await, 1);
    assert_eq!(old_session.closes.load(Ordering::SeqCst), 1);
    assert_eq!(new_session.closes.load(Ordering::SeqCst), 0);
    assert_eq!(connector.connects.load(Ordering::SeqCst), 2);
    assert_eq!(
        service
            .profile_generation(&profile_id)
            .await
            .expect("current generation"),
        updated.profile_generation
    );
    service
        .check(OperationId(963), profile_id, Duration::from_secs(1))
        .await
        .expect("new cached session remains reusable");
    assert_eq!(connector.connects.load(Ordering::SeqCst), 2);
}

#[test]
fn canonical_service_error_parts_drive_stale_and_tls_recovery_rows() {
    let profile_id = ProfileId("typed-profile".to_owned());
    let stale = ServiceError::ProfileStale {
        profile_id: profile_id.clone(),
        operation_id: OperationId(870),
    };
    assert_eq!(
        stale.public_error_parts(),
        (PublicSummary::ResourceStale, PublicCode::ProfileStale)
    );
    assert_eq!(
        recovery_for(
            OperationKind::UpdateProfile,
            stale.public_summary(),
            stale.public_code(),
            &SafeContext::profile(profile_id.clone(), OperationId(870)),
        )
        .expect("stale recovery")
        .as_slice(),
        &[RecoveryAction::ReloadConfiguration]
    );

    for code in [
        PublicCode::RedisTlsCaInvalidPem,
        PublicCode::RedisTlsCaUntrustedIssuer,
    ] {
        let error = ServiceError::InvalidProfile(ProfileValidationError::Field {
            field: ProfileFieldId::RedisCaFile,
            code,
        });
        assert_eq!(
            error.public_error_parts(),
            (PublicSummary::TlsVerificationFailed, code)
        );
        assert_eq!(
            recovery_for(
                OperationKind::ConnectProfile,
                error.public_summary(),
                error.public_code(),
                &SafeContext::profile(profile_id.clone(), OperationId(871)),
            )
            .expect("saved CA recovery")
            .as_slice(),
            &[RecoveryAction::EditProfile(
                profile_id.clone(),
                ProfileFieldId::RedisCaFile,
            )]
        );
        assert_eq!(
            recovery_for(
                OperationKind::TestDraftConnection,
                error.public_summary(),
                error.public_code(),
                &SafeContext::draft(DraftId(872), OperationId(872)),
            )
            .expect("draft CA recovery")
            .as_slice(),
            &[RecoveryAction::EditDraft(
                DraftId(872),
                ProfileFieldId::RedisCaFile,
            )]
        );
    }
}

#[test]
fn invalid_create_auto_base_has_exact_connection_id_draft_recovery() {
    let mutation = ConfigMutation::CreateAuto {
        base_id: " invalid-auto-base".to_owned(),
        profile: dbotter::model::ConnectionProfile::from_draft(
            "valid-destination".to_owned(),
            named_draft(DriverKind::Redis, "Valid destination"),
        ),
    };
    let error = ServiceError::InvalidProfile(
        validate_config_mutation(&mutation).expect_err("invalid base id is rejected"),
    );
    assert_eq!(
        error.public_error_parts(),
        (
            PublicSummary::InvalidInput,
            PublicCode::Field(ProfileFieldId::ConnectionId),
        )
    );
    assert_eq!(
        recovery_for(
            OperationKind::CreateProfile,
            error.public_summary(),
            error.public_code(),
            &SafeContext::draft(DraftId(975), OperationId(975)),
        )
        .expect("invalid auto base has typed Create recovery")
        .as_slice(),
        &[RecoveryAction::EditDraft(
            DraftId(975),
            ProfileFieldId::ConnectionId,
        )]
    );
}

#[tokio::test]
async fn identity_corruption_blocks_but_semantic_invalid_external_profiles_remain_editable() {
    for duplicate_identity in [false, true] {
        for mutation in [
            MatrixMutation::Create,
            MatrixMutation::Update,
            MatrixMutation::Delete,
        ] {
            let directory = tempfile::tempdir().expect("tempdir");
            let path = directory.path().join("config.toml");
            let target_profile =
                classified_profile("target", named_draft(DriverKind::Redis, "Target"), 1);
            let unrelated_profile =
                classified_profile("unrelated", named_draft(DriverKind::Redis, "Unrelated"), 2);
            let persisted = write_v3_profiles(&path, vec![target_profile, unrelated_profile]);
            let target_profile = persisted_profile(&persisted, "target");
            let unrelated_profile = persisted_profile(&persisted, "unrelated");
            let connector = Arc::new(FakeConnector::new(false));
            let store = Arc::new(SessionSecretStore::default());
            let target = ProfileId("target".to_owned());
            let unrelated = ProfileId("unrelated".to_owned());
            for profile_id in [&target, &unrelated] {
                store
                    .apply(
                        profile_id,
                        SessionSecretUpdate::Replace(Arc::new(SessionSecret::new(
                            "retained-current".to_owned(),
                        ))),
                    )
                    .expect("seed Arc");
            }
            let service = ApplicationService::with_dependencies(
                &path,
                connector.clone(),
                Arc::new(MissingEnvironment),
                store.clone(),
                ConfigWriter::default(),
            )
            .expect("valid initial service");
            service
                .check(OperationId(880), target.clone(), Duration::from_secs(1))
                .await
                .expect("seed cache");
            service
                .check(OperationId(881), unrelated.clone(), Duration::from_secs(1))
                .await
                .expect("seed unrelated cache");
            let generation = service
                .profile_generation(&target)
                .await
                .expect("generation");
            let unrelated_generation = service
                .profile_generation(&unrelated)
                .await
                .expect("unrelated generation");
            let before_profiles = service.profiles_snapshot().await;
            let connects_before = connector.connects.load(Ordering::SeqCst);

            let mut external_profiles = vec![target_profile, unrelated_profile.clone()];
            if duplicate_identity {
                external_profiles.push(unrelated_profile);
            } else {
                external_profiles[1].host.clear();
            }
            let external_bytes = toml::to_string(&Config {
                version: 3,
                profiles: external_profiles,
            })
            .expect("external invalid config")
            .into_bytes();
            fs::write(&path, &external_bytes).expect("external invalid edit");

            if duplicate_identity {
                let error = run_matrix_mutation(&service, mutation, generation)
                    .await
                    .expect_err("identity-corrupt latest snapshot blocks mutation");
                assert!(matches!(
                    &error,
                    ServiceError::Config(ConfigError::InvalidProfile)
                ));
                assert_eq!(
                    error.public_error_parts(),
                    (
                        PublicSummary::ResourceStale,
                        PublicCode::ConfigExternalChange
                    )
                );
                assert_eq!(
                    fs::read(&path).expect("identity-corrupt bytes remain"),
                    external_bytes
                );
                assert_eq!(service.profiles_snapshot().await, before_profiles);
                assert_eq!(service.cached_session_count().await, 2);
                assert!(store.has_current(&target).expect("target Arc retained"));
                assert!(
                    store
                        .has_current(&unrelated)
                        .expect("unrelated Arc retained")
                );
            } else {
                run_matrix_mutation(&service, mutation, generation)
                    .await
                    .expect("semantic-invalid unrelated profile does not block mutation");
                let disk = load_path(&path).expect("final disk config").config;
                assert_eq!(service.profiles_snapshot().await, disk.profiles);
                let invalid = disk
                    .profiles
                    .iter()
                    .find(|profile| profile.id == unrelated.as_str())
                    .expect("semantic-invalid profile remains visible");
                assert!(invalid.host.is_empty());
                assert_ne!(
                    service
                        .profile_generation(&unrelated)
                        .await
                        .expect("changed invalid generation"),
                    unrelated_generation
                );
                assert!(
                    !store
                        .has_current(&unrelated)
                        .expect("changed invalid Arc cleared")
                );
                let network_error = service
                    .check(OperationId(904), unrelated.clone(), Duration::from_secs(1))
                    .await
                    .expect_err("semantic-invalid profile is fenced before network");
                assert!(matches!(
                    network_error,
                    ServiceError::InvalidProfile(ProfileValidationError::Field {
                        field: ProfileFieldId::Host,
                        ..
                    })
                ));
                assert_eq!(connector.connects.load(Ordering::SeqCst), connects_before);
                assert!(connector.session.closes.load(Ordering::SeqCst) >= 1);
            }
        }
    }
}

#[tokio::test]
async fn every_semantic_invalid_profile_can_be_repaired_or_deleted_without_unrelated_loss() {
    for kind in EditableSemanticKind::ALL {
        for repair in [true, false] {
            let directory = tempfile::tempdir().expect("tempdir");
            let path = directory.path().join("config.toml");
            let invalid = classify_profile(kind.profile(directory.path()), 2);
            let unrelated = classified_profile(
                "unrelated-valid",
                named_draft(DriverKind::MySql, "Unrelated valid"),
                1,
            );
            let persisted = write_v3_profiles(&path, vec![unrelated, invalid]);
            let invalid = persisted_profile(&persisted, "editable-invalid");
            let invalid_id = ProfileId("editable-invalid".to_owned());
            let unrelated_id = ProfileId("unrelated-valid".to_owned());
            let connector = Arc::new(FakeConnector::new(false));
            let store = Arc::new(SessionSecretStore::default());
            for profile_id in [&invalid_id, &unrelated_id] {
                store
                    .apply(
                        profile_id,
                        SessionSecretUpdate::Replace(Arc::new(SessionSecret::new(
                            "matrix-current".to_owned(),
                        ))),
                    )
                    .expect("seed Arc");
            }
            let service = ApplicationService::with_dependencies(
                &path,
                connector.clone(),
                Arc::new(MissingEnvironment),
                store.clone(),
                ConfigWriter::default(),
            )
            .expect("semantic-invalid profile loads visibly");
            service
                .check(
                    OperationId(970),
                    unrelated_id.clone(),
                    Duration::from_secs(1),
                )
                .await
                .expect("cache unrelated profile");
            let generation = service
                .profile_generation(&invalid_id)
                .await
                .expect("invalid row generation");
            let network_error = service
                .check(OperationId(971), invalid_id.clone(), Duration::from_secs(1))
                .await
                .expect_err("invalid row is fenced before connector");
            assert!(matches!(
                &network_error,
                ServiceError::InvalidProfile(ProfileValidationError::Field { field, .. })
                    if *field == kind.field()
            ));
            assert_eq!(connector.connects.load(Ordering::SeqCst), 1);
            assert_eq!(
                recovery_for(
                    OperationKind::ConnectProfile,
                    network_error.public_summary(),
                    network_error.public_code(),
                    &SafeContext::profile(invalid_id.clone(), OperationId(971)),
                )
                .expect("invalid row has exact edit recovery")
                .as_slice(),
                &[RecoveryAction::EditProfile(
                    invalid_id.clone(),
                    kind.field(),
                )]
            );

            let before_rejected = fs::read(&path).expect("before rejected destination");
            let rejected = service
                .update_profile(UpdateProfileRequest {
                    profile_id: invalid_id.clone(),
                    expected_generation: generation,
                    operation_id: OperationId(972),
                    draft: invalid.as_draft(),
                    secret_update: SessionSecretUpdate::Clear,
                    migration_consent: MigrationConsent::Cancelled,
                })
                .await
                .expect_err("new semantic-invalid destination remains strict");
            assert!(matches!(
                rejected,
                ServiceError::InvalidProfile(ProfileValidationError::Field { field, .. })
                    if field == kind.field()
            ));
            assert_eq!(
                fs::read(&path).expect("rejected destination has no write"),
                before_rejected
            );
            assert_eq!(service.cached_session_count().await, 1);
            assert!(
                store
                    .has_current(&unrelated_id)
                    .expect("unrelated Arc remains")
            );

            if repair {
                let result = service
                    .update_profile(UpdateProfileRequest {
                        profile_id: invalid_id.clone(),
                        expected_generation: generation,
                        operation_id: OperationId(973),
                        draft: named_draft(DriverKind::Redis, "Repaired"),
                        secret_update: SessionSecretUpdate::Clear,
                        migration_consent: MigrationConsent::Cancelled,
                    })
                    .await;
                let outcome = match result {
                    Ok(outcome) => outcome,
                    Err(ServiceError::Config(ConfigError::ExternalChange)) => {
                        panic!("{kind:?}: repair unexpectedly observed an external change")
                    }
                    Err(ServiceError::Config(ConfigError::InvalidProfile)) => {
                        panic!("{kind:?}: repair unexpectedly failed identity validation")
                    }
                    Err(error) => panic!("{kind:?}: repair failed: {error:?}"),
                };
                assert_ne!(outcome.profile_generation, generation);
                assert!(
                    service
                        .profiles_snapshot()
                        .await
                        .iter()
                        .any(
                            |profile| profile.id == invalid_id.as_str() && !profile.host.is_empty()
                        )
                );
            } else {
                service
                    .delete_profile(DeleteProfileRequest {
                        profile_id: invalid_id.clone(),
                        expected_generation: generation,
                        operation_id: OperationId(974),
                        migration_consent: MigrationConsent::Cancelled,
                    })
                    .await
                    .expect("invalid row can be deleted");
                assert!(
                    service
                        .profiles_snapshot()
                        .await
                        .iter()
                        .all(|profile| profile.id != invalid_id.as_str())
                );
            }
            let disk = load_path(&path).expect("final disk").config;
            assert_eq!(service.profiles_snapshot().await, disk.profiles);
            assert!(
                disk.profiles
                    .iter()
                    .any(|profile| profile.id == unrelated_id.as_str())
            );
            assert_eq!(service.cached_session_count().await, 1);
            assert!(
                store
                    .has_current(&unrelated_id)
                    .expect("unrelated Arc preserved")
            );
            assert!(
                !store
                    .has_current(&invalid_id)
                    .expect("repaired/deleted row Arc cleared")
            );
            assert_eq!(connector.connects.load(Ordering::SeqCst), 1);
        }
    }
}

#[tokio::test]
async fn editable_missing_ca_is_visible_and_does_not_block_unrelated_mutations_or_own_recovery() {
    for mutation in [
        MatrixMutation::Create,
        MatrixMutation::Update,
        MatrixMutation::Delete,
    ] {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("config.toml");
        let mut broken = dbotter::model::ConnectionProfile::from_draft(
            "broken-ca".to_owned(),
            named_draft(DriverKind::Redis, "Broken CA"),
        );
        broken.tls = TlsMode::Required;
        broken.redis_tls.ca_file = Some(directory.path().join("missing-ca.pem"));
        let broken = classify_profile(broken, 2);
        let target = classified_profile("target", named_draft(DriverKind::Redis, "Target"), 1);
        write_v3_profiles(&path, vec![target, broken]);
        let connector = Arc::new(FakeConnector::new(true));
        let service = ApplicationService::with_dependencies(
            &path,
            connector.clone(),
            Arc::new(MissingEnvironment),
            Arc::new(SessionSecretStore::default()),
            ConfigWriter::default(),
        )
        .expect("missing CA stays visible for edit");
        let broken_error = service
            .check(
                OperationId(890),
                ProfileId("broken-ca".to_owned()),
                Duration::from_secs(1),
            )
            .await
            .expect_err("missing CA is blocked before connector");
        assert_eq!(
            broken_error.public_error_parts(),
            (
                PublicSummary::TlsVerificationFailed,
                PublicCode::RedisTlsCaInvalidPem,
            )
        );
        assert_eq!(connector.connects.load(Ordering::SeqCst), 0);

        let generation = service
            .profile_generation(&ProfileId("target".to_owned()))
            .await
            .expect("target generation");
        run_matrix_mutation(&service, mutation, generation)
            .await
            .expect("unrelated editable CA state does not block mutation");
    }

    for recover_by_delete in [false, true] {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("config.toml");
        let mut broken = dbotter::model::ConnectionProfile::from_draft(
            "broken-ca".to_owned(),
            named_draft(DriverKind::Redis, "Broken CA"),
        );
        broken.tls = TlsMode::Required;
        broken.redis_tls.ca_file = Some(directory.path().join("missing-ca.pem"));
        let broken = classify_profile(broken, 1);
        write_v3_profiles(&path, vec![broken]);
        let service = service(
            &path,
            Arc::new(FakeConnector::new(true)),
            ConfigWriter::default(),
        );
        let profile_id = ProfileId("broken-ca".to_owned());
        let generation = service
            .profile_generation(&profile_id)
            .await
            .expect("broken generation");
        if recover_by_delete {
            service
                .delete_profile(DeleteProfileRequest {
                    profile_id,
                    expected_generation: generation,
                    operation_id: OperationId(895),
                    migration_consent: MigrationConsent::Cancelled,
                })
                .await
                .expect("broken profile can be deleted");
        } else {
            service
                .update_profile(UpdateProfileRequest {
                    profile_id,
                    expected_generation: generation,
                    operation_id: OperationId(896),
                    draft: named_draft(DriverKind::Redis, "Recovered"),
                    secret_update: SessionSecretUpdate::Clear,
                    migration_consent: MigrationConsent::Cancelled,
                })
                .await
                .expect("broken profile can be updated to valid");
        }
    }
}

#[tokio::test]
async fn mutation_generations_are_exactly_monotonic_by_one() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    let service = service(
        &path,
        Arc::new(FakeConnector::new(false)),
        ConfigWriter::default(),
    );
    let first = service
        .create_profile(create_request(
            DraftId(897),
            OperationId(897),
            Some("sequence"),
            named_draft(DriverKind::Redis, "One"),
            SessionSecretUpdate::Clear,
        ))
        .await
        .expect("create");
    let second = service
        .update_profile(UpdateProfileRequest {
            profile_id: first.profile_id.clone(),
            expected_generation: first.profile_generation,
            operation_id: OperationId(898),
            draft: named_draft(DriverKind::Redis, "Two"),
            secret_update: SessionSecretUpdate::Clear,
            migration_consent: MigrationConsent::Cancelled,
        })
        .await
        .expect("update");
    let third = service
        .delete_profile(DeleteProfileRequest {
            profile_id: second.profile_id,
            expected_generation: second.profile_generation,
            operation_id: OperationId(899),
            migration_consent: MigrationConsent::Cancelled,
        })
        .await
        .expect("delete");
    let fourth = service
        .create_profile(create_request(
            DraftId(900),
            OperationId(900),
            Some("sequence"),
            named_draft(DriverKind::Redis, "Four"),
            SessionSecretUpdate::Clear,
        ))
        .await
        .expect("recreate");
    let fifth = service
        .create_profile(create_request(
            DraftId(901),
            OperationId(901),
            Some("next"),
            named_draft(DriverKind::Redis, "Five"),
            SessionSecretUpdate::Clear,
        ))
        .await
        .expect("next create");

    assert_eq!(
        [
            first.profile_generation,
            second.profile_generation,
            third.profile_generation,
            fourth.profile_generation,
            fifth.profile_generation,
        ],
        [
            ProfileGeneration(1),
            ProfileGeneration(2),
            ProfileGeneration(3),
            ProfileGeneration(4),
            ProfileGeneration(5),
        ]
    );
}

#[derive(Clone, Copy, Debug)]
enum MatrixMutation {
    Create,
    Update,
    Delete,
}

#[derive(Clone, Copy, Debug)]
enum EditableSemanticKind {
    BlankHost,
    ZeroPort,
    EnvironmentMissingName,
    NoneWithEnvironmentName,
    RedisDisabledWithCa,
    RedisRequiredWithMissingCa,
    RedisPreferred,
}

impl EditableSemanticKind {
    const ALL: [Self; 7] = [
        Self::BlankHost,
        Self::ZeroPort,
        Self::EnvironmentMissingName,
        Self::NoneWithEnvironmentName,
        Self::RedisDisabledWithCa,
        Self::RedisRequiredWithMissingCa,
        Self::RedisPreferred,
    ];

    fn profile(self, directory: &Path) -> dbotter::model::ConnectionProfile {
        let mut profile = dbotter::model::ConnectionProfile::from_draft(
            "editable-invalid".to_owned(),
            named_draft(DriverKind::Redis, "Editable invalid"),
        );
        match self {
            Self::BlankHost => profile.host.clear(),
            Self::ZeroPort => profile.port = 0,
            Self::EnvironmentMissingName => {
                profile.credential_mode = CredentialMode::Environment;
                profile.secret_env = None;
            }
            Self::NoneWithEnvironmentName => {
                profile.credential_mode = CredentialMode::None;
                profile.secret_env = Some("MUST_BE_CLEARED".to_owned());
            }
            Self::RedisDisabledWithCa => {
                profile.tls = TlsMode::Disabled;
                profile.redis_tls.ca_file = Some(directory.join("hidden-ca.pem"));
            }
            Self::RedisRequiredWithMissingCa => {
                profile.tls = TlsMode::Required;
                profile.redis_tls.ca_file = Some(directory.join("missing-ca.pem"));
            }
            Self::RedisPreferred => profile.tls = TlsMode::Preferred,
        }
        profile
    }

    fn field(self) -> ProfileFieldId {
        match self {
            Self::BlankHost => ProfileFieldId::Host,
            Self::ZeroPort => ProfileFieldId::Port,
            Self::EnvironmentMissingName | Self::NoneWithEnvironmentName => {
                ProfileFieldId::CredentialEnvironmentName
            }
            Self::RedisDisabledWithCa | Self::RedisRequiredWithMissingCa => {
                ProfileFieldId::RedisCaFile
            }
            Self::RedisPreferred => ProfileFieldId::RedisTlsMode,
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum MatrixOutcome {
    PreCommitRetry,
    DurabilityUnknown,
    ObservationFailed,
}

#[tokio::test]
async fn create_update_delete_cover_every_backup_and_main_failpoint_with_isolated_state() {
    let cases = [
        (
            MutationFailpoint::BackupTempCreate,
            MatrixOutcome::PreCommitRetry,
        ),
        (
            MutationFailpoint::BackupWrite,
            MatrixOutcome::PreCommitRetry,
        ),
        (
            MutationFailpoint::BackupFileSync,
            MatrixOutcome::PreCommitRetry,
        ),
        (
            MutationFailpoint::BackupRename,
            MatrixOutcome::PreCommitRetry,
        ),
        (
            MutationFailpoint::BackupDirectorySync,
            MatrixOutcome::PreCommitRetry,
        ),
        (
            MutationFailpoint::MainTempCreate,
            MatrixOutcome::PreCommitRetry,
        ),
        (MutationFailpoint::MainWrite, MatrixOutcome::PreCommitRetry),
        (
            MutationFailpoint::MainFileSync,
            MatrixOutcome::PreCommitRetry,
        ),
        (
            MutationFailpoint::MainPreRename,
            MatrixOutcome::PreCommitRetry,
        ),
        (
            MutationFailpoint::MainPostRename,
            MatrixOutcome::DurabilityUnknown,
        ),
        (
            MutationFailpoint::MainDirectorySync,
            MatrixOutcome::DurabilityUnknown,
        ),
        (
            MutationFailpoint::MainObservationLoad,
            MatrixOutcome::ObservationFailed,
        ),
    ];

    for mutation in [
        MatrixMutation::Create,
        MatrixMutation::Update,
        MatrixMutation::Delete,
    ] {
        for (point, expected) in cases {
            assert_matrix_failpoint_case(mutation, point, expected).await;
        }
    }
}

async fn assert_matrix_failpoint_case(
    mutation: MatrixMutation,
    point: MutationFailpoint,
    expected: MatrixOutcome,
) {
    const ORIGINAL: &[u8] = br#"version = 1

[[profiles]]
id = "target"
name = "Before"
driver = "redis"
host = "127.0.0.1"
port = 6379
tls = "disabled"

[[profiles]]
id = "unrelated"
name = "Unrelated"
driver = "redis"
host = "127.0.0.1"
port = 6380
tls = "disabled"
"#;

    if is_backup_failpoint(point) {
        assert!(matches!(expected, MatrixOutcome::PreCommitRetry));
        assert_backup_failpoint_case(mutation, point, ORIGINAL).await;
        return;
    }

    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    let target_profile = classified_profile("target", named_draft(DriverKind::Redis, "Before"), 1);
    let mut unrelated_draft = named_draft(DriverKind::Redis, "Unrelated");
    unrelated_draft.port = 6380;
    let unrelated_profile = classified_profile("unrelated", unrelated_draft, 2);
    write_v3_profiles(&path, vec![target_profile, unrelated_profile]);
    let original = fs::read(&path).expect("v3 matrix fixture bytes");
    let target = ProfileId("target".to_owned());
    let unrelated = ProfileId("unrelated".to_owned());
    let orphan = ProfileId("orphan".to_owned());
    let created = ProfileId("created".to_owned());
    let connector = Arc::new(FakeConnector::new(false));
    let store = Arc::new(SessionSecretStore::default());
    seed_matrix_secrets(&store);
    let fault = Arc::new(FailOnceAt::new(point));
    let service = ApplicationService::with_dependencies(
        &path,
        connector,
        Arc::new(MissingEnvironment),
        store.clone(),
        ConfigWriter::with_fault_injector(fault.clone()),
    )
    .expect("matrix service");
    service
        .check(OperationId(900), target.clone(), Duration::from_secs(1))
        .await
        .expect("seed target cache");
    let generation = service
        .profile_generation(&target)
        .await
        .expect("target generation");
    let before_profiles = service.profiles_snapshot().await;

    let first = run_matrix_mutation(&service, mutation, generation).await;
    assert!(fault.has_failed(), "{mutation:?}/{point:?} fault must fire");
    match expected {
        MatrixOutcome::PreCommitRetry => {
            assert!(
                matches!(
                    first,
                    Err(ServiceError::Config(ConfigError::NotCommitted { stage, .. }))
                        if stage == point
                ),
                "{mutation:?}/{point:?}: {first:?}"
            );
            assert_eq!(fs::read(&path).expect("main bytes"), original);
            assert_eq!(service.profiles_snapshot().await, before_profiles);
            assert_eq!(service.cached_session_count().await, 1);
            assert!(store.has_current(&target).expect("target Arc"));
            assert!(store.has_current(&unrelated).expect("unrelated Arc"));
            assert!(store.has_current(&orphan).expect("orphan before commit"));
            assert!(!store.has_current(&created).expect("created absent"));
            assert!(!service.is_config_uncertain());

            let retried = run_matrix_mutation(&service, mutation, generation)
                .await
                .expect("pre-commit request can be retried exactly once");
            assert_eq!(retried, CommitState::Committed, "{mutation:?}/{point:?}");
            assert_committed_matrix_state(&service, &store, &path, mutation).await;
        }
        MatrixOutcome::DurabilityUnknown => {
            assert_eq!(
                first.expect("post-rename state is observed"),
                CommitState::CommittedDurabilityUnknown,
                "{mutation:?}/{point:?}"
            );
            assert_committed_matrix_state(&service, &store, &path, mutation).await;
        }
        MatrixOutcome::ObservationFailed => {
            assert!(
                matches!(first, Err(ServiceError::PostCommitObservation(_))),
                "{mutation:?}/{point:?}: {first:?}"
            );
            assert!(service.is_config_uncertain());
            assert_eq!(service.profiles_snapshot().await, before_profiles);
            assert_eq!(service.cached_session_count().await, 0);
            assert!(store.is_empty().expect("uncertain clears all Arcs"));
            assert_disk_matrix_mutation(&path, mutation);
        }
    }
}

async fn assert_backup_failpoint_case(
    mutation: MatrixMutation,
    point: MutationFailpoint,
    original: &[u8],
) {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    fs::write(&path, original).expect("v1 matrix fixture");
    let target = ProfileId("target".to_owned());
    let unrelated = ProfileId("unrelated".to_owned());
    let orphan = ProfileId("orphan".to_owned());
    let created = ProfileId("created".to_owned());
    let connector = Arc::new(FakeConnector::new(false));
    let store = Arc::new(SessionSecretStore::default());
    seed_matrix_secrets(&store);
    let fault = Arc::new(FailOnceAt::new(point));
    let service = ApplicationService::with_dependencies(
        &path,
        connector,
        Arc::new(MissingEnvironment),
        store.clone(),
        ConfigWriter::with_fault_injector(fault.clone()),
    )
    .expect("legacy matrix service");
    service
        .check(OperationId(900), target.clone(), Duration::from_secs(1))
        .await
        .expect("seed target cache");
    let before_profiles = service.profiles_snapshot().await;
    let migration_writer = ConfigWriter::with_fault_injector(fault.clone());
    let posture_document = explicit_migration_document(&migration_writer, &path);

    let first = migration_writer.migrate_v3(&path, &posture_document);
    assert!(fault.has_failed(), "{mutation:?}/{point:?} fault must fire");
    assert!(
        matches!(
            first,
            Err(ConfigError::NotCommitted { stage, .. }) if stage == point
        ),
        "{mutation:?}/{point:?}: {first:?}"
    );
    assert_eq!(fs::read(&path).expect("main bytes"), original);
    assert_eq!(service.profiles_snapshot().await, before_profiles);
    assert_eq!(service.cached_session_count().await, 1);
    assert!(store.has_current(&target).expect("target Arc"));
    assert!(store.has_current(&unrelated).expect("unrelated Arc"));
    assert!(store.has_current(&orphan).expect("orphan before commit"));
    assert!(!store.has_current(&created).expect("created absent"));
    assert!(!service.is_config_uncertain());

    let migrated = migration_writer
        .migrate_v3(&path, &posture_document)
        .expect("explicit migration can be retried exactly once");
    assert_eq!(migrated.state, CommitState::Committed);
    assert!(migrated.migration_backup.is_some());
    service
        .reload_configuration()
        .await
        .expect("service adopts classified v3 migration");
    assert_eq!(service.source_version().await, ConfigSourceVersion::V3);

    seed_matrix_secrets(&store);
    service
        .check(OperationId(904), target.clone(), Duration::from_secs(1))
        .await
        .expect("reseed classified target cache");
    let generation = service
        .profile_generation(&target)
        .await
        .expect("classified target generation");
    let committed = run_matrix_mutation(&service, mutation, generation)
        .await
        .expect("classified mutation commits after migration retry");
    assert_eq!(committed, CommitState::Committed, "{mutation:?}/{point:?}");
    assert_committed_matrix_state(&service, &store, &path, mutation).await;
}

fn seed_matrix_secrets(store: &SessionSecretStore) {
    for (profile_id, secret) in [
        ("target", "target-current"),
        ("unrelated", "unrelated-current"),
        ("orphan", "orphan-must-clear-after-reconcile"),
    ] {
        store
            .apply(
                &ProfileId(profile_id.to_owned()),
                SessionSecretUpdate::Replace(Arc::new(SessionSecret::new(secret.to_owned()))),
            )
            .expect("seed matrix secret Arc");
    }
}

async fn run_matrix_mutation(
    service: &ApplicationService,
    mutation: MatrixMutation,
    generation: ProfileGeneration,
) -> Result<CommitState, ServiceError> {
    let outcome = match mutation {
        MatrixMutation::Create => {
            let mut draft = named_draft(DriverKind::Redis, "Created");
            draft.credential_mode = CredentialMode::Session;
            service
                .create_profile(create_request(
                    DraftId(901),
                    OperationId(901),
                    Some("created"),
                    draft,
                    SessionSecretUpdate::Replace(Arc::new(SessionSecret::new(
                        "created-current".to_owned(),
                    ))),
                ))
                .await?
        }
        MatrixMutation::Update => {
            service
                .update_profile(UpdateProfileRequest {
                    profile_id: ProfileId("target".to_owned()),
                    expected_generation: generation,
                    operation_id: OperationId(902),
                    draft: named_draft(DriverKind::Redis, "Updated"),
                    secret_update: SessionSecretUpdate::Clear,
                    migration_consent: MigrationConsent::Confirmed,
                })
                .await?
        }
        MatrixMutation::Delete => {
            service
                .delete_profile(DeleteProfileRequest {
                    profile_id: ProfileId("target".to_owned()),
                    expected_generation: generation,
                    operation_id: OperationId(903),
                    migration_consent: MigrationConsent::Confirmed,
                })
                .await?
        }
    };
    Ok(outcome.commit_state)
}

async fn assert_committed_matrix_state(
    service: &ApplicationService,
    store: &SessionSecretStore,
    path: &Path,
    mutation: MatrixMutation,
) {
    let disk = load_path(path).expect("committed disk config").config;
    assert_eq!(service.profiles_snapshot().await, disk.profiles);
    assert!(!service.is_config_uncertain());
    assert!(
        store
            .has_current(&ProfileId("unrelated".to_owned()))
            .expect("unrelated Arc")
    );
    assert!(
        !store
            .has_current(&ProfileId("orphan".to_owned()))
            .expect("orphan cleared")
    );
    match mutation {
        MatrixMutation::Create => {
            assert!(disk.profiles.iter().any(|profile| profile.id == "target"));
            assert!(disk.profiles.iter().any(|profile| profile.id == "created"));
            assert_eq!(service.cached_session_count().await, 1);
            assert!(
                store
                    .has_current(&ProfileId("target".to_owned()))
                    .expect("target Arc")
            );
            assert!(
                store
                    .has_current(&ProfileId("created".to_owned()))
                    .expect("created Arc")
            );
        }
        MatrixMutation::Update => {
            let target = disk
                .profiles
                .iter()
                .find(|profile| profile.id == "target")
                .expect("updated target");
            assert_eq!(target.name, "Updated");
            assert_eq!(service.cached_session_count().await, 0);
            assert!(
                !store
                    .has_current(&ProfileId("target".to_owned()))
                    .expect("target cleared")
            );
        }
        MatrixMutation::Delete => {
            assert!(disk.profiles.iter().all(|profile| profile.id != "target"));
            assert_eq!(service.cached_session_count().await, 0);
            assert!(
                !store
                    .has_current(&ProfileId("target".to_owned()))
                    .expect("target cleared")
            );
        }
    }
}

fn assert_disk_matrix_mutation(path: &Path, mutation: MatrixMutation) {
    let disk = load_path(path)
        .expect("committed disk remains readable")
        .config;
    match mutation {
        MatrixMutation::Create => {
            assert!(disk.profiles.iter().any(|profile| profile.id == "created"));
        }
        MatrixMutation::Update => {
            assert_eq!(
                disk.profiles
                    .iter()
                    .find(|profile| profile.id == "target")
                    .expect("updated target")
                    .name,
                "Updated"
            );
        }
        MatrixMutation::Delete => {
            assert!(disk.profiles.iter().all(|profile| profile.id != "target"));
        }
    }
}

#[tokio::test]
async fn profile_mutation_outcome_debug_redacts_an_available_backup_path() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory
        .path()
        .join("sentinel-profile-outcome-config.toml");
    fs::write(&path, b"version = 1\nprofiles = []\n").expect("v1 fixture");
    let writer = ConfigWriter::default();
    let posture_document = explicit_migration_document(&writer, &path);
    let migration = writer
        .migrate_v3(&path, &posture_document)
        .expect("explicit migration save");
    assert_eq!(migration.state, CommitState::Committed);
    let outcome = ProfileMutationOutcome {
        operation_id: OperationId(87),
        profile_id: ProfileId("redacted-outcome".to_owned()),
        profile_generation: ProfileGeneration(1),
        commit_state: migration.state,
        migration_backup: migration.migration_backup,
    };

    assert!(outcome.migration_backup.is_some());
    assert!(!format!("{outcome:?}").contains("sentinel-profile-outcome-config.toml"));
}

struct FailAt(MutationFailpoint);

impl MutationFaultInjector for FailAt {
    fn check(&self, point: MutationFailpoint, _path: &Path) -> std::io::Result<()> {
        if point == self.0 {
            Err(std::io::Error::other("injected"))
        } else {
            Ok(())
        }
    }
}

struct RewriteCreatedPostureAtObservation {
    profile_id: String,
    fired: AtomicBool,
    preserved_instance_id: Mutex<Option<ProfileInstanceId>>,
}

impl RewriteCreatedPostureAtObservation {
    fn new(profile_id: &str) -> Self {
        Self {
            profile_id: profile_id.to_owned(),
            fired: AtomicBool::new(false),
            preserved_instance_id: Mutex::new(None),
        }
    }

    fn preserved_instance_id(&self) -> ProfileInstanceId {
        (*self
            .preserved_instance_id
            .lock()
            .expect("preserved instance id lock"))
        .expect("observation rewrite captured an instance id")
    }
}

impl MutationFaultInjector for RewriteCreatedPostureAtObservation {
    fn check(&self, point: MutationFailpoint, path: &Path) -> std::io::Result<()> {
        if point != MutationFailpoint::MainObservationLoad
            || self.fired.swap(true, Ordering::SeqCst)
        {
            return Ok(());
        }
        let loaded =
            load_path(path).map_err(|_| std::io::Error::other("new v3 config was not readable"))?;
        if loaded.source_version != ConfigSourceVersion::V3 {
            return Err(std::io::Error::other("new config was not version 3"));
        }
        let mut config = loaded.config;
        let profile = config
            .profiles
            .iter_mut()
            .find(|profile| profile.id == self.profile_id)
            .ok_or_else(|| std::io::Error::other("created profile was not written"))?;
        if profile.safety.environment() != Some(ProfileEnvironment::Development)
            || profile.safety.access() != Some(ProfileAccess::ReadOnly)
        {
            return Err(std::io::Error::other(
                "created profile did not retain the requested posture",
            ));
        }
        let instance_id = profile
            .safety
            .instance_id()
            .ok_or_else(|| std::io::Error::other("created profile has no instance id"))?;
        profile.safety = ProfileSafetyPosture::classified(
            ProfileEnvironment::Production,
            ProfileAccess::ReadWrite,
            instance_id,
        );
        *self
            .preserved_instance_id
            .lock()
            .map_err(|_| std::io::Error::other("preserved instance id lock poisoned"))? =
            Some(instance_id);
        let encoded = toml::to_string(&config)
            .map_err(|_| std::io::Error::other("rewritten v3 config did not serialize"))?;
        fs::write(path, encoded)
    }
}

struct RewriteCreatedInstanceIdAtObservation {
    profile_id: String,
    fired: AtomicBool,
    original_instance_id: Mutex<Option<ProfileInstanceId>>,
    substituted_instance_id: Mutex<Option<ProfileInstanceId>>,
}

impl RewriteCreatedInstanceIdAtObservation {
    fn new(profile_id: &str) -> Self {
        Self {
            profile_id: profile_id.to_owned(),
            fired: AtomicBool::new(false),
            original_instance_id: Mutex::new(None),
            substituted_instance_id: Mutex::new(None),
        }
    }

    fn original_instance_id(&self) -> ProfileInstanceId {
        (*self
            .original_instance_id
            .lock()
            .expect("original instance id lock"))
        .expect("observation rewrite captured the original instance id")
    }

    fn substituted_instance_id(&self) -> ProfileInstanceId {
        (*self
            .substituted_instance_id
            .lock()
            .expect("substituted instance id lock"))
        .expect("observation rewrite captured the substituted instance id")
    }
}

impl MutationFaultInjector for RewriteCreatedInstanceIdAtObservation {
    fn check(&self, point: MutationFailpoint, path: &Path) -> std::io::Result<()> {
        if point != MutationFailpoint::MainObservationLoad
            || self.fired.swap(true, Ordering::SeqCst)
        {
            return Ok(());
        }
        let loaded =
            load_path(path).map_err(|_| std::io::Error::other("new v3 config was not readable"))?;
        if loaded.source_version != ConfigSourceVersion::V3 {
            return Err(std::io::Error::other("new config was not version 3"));
        }
        let mut config = loaded.config;
        let profile_index = config
            .profiles
            .iter()
            .position(|profile| profile.id == self.profile_id)
            .ok_or_else(|| std::io::Error::other("created profile was not written"))?;
        let profile = &config.profiles[profile_index];
        let environment = profile
            .safety
            .environment()
            .ok_or_else(|| std::io::Error::other("created profile has no environment"))?;
        let access = profile
            .safety
            .access()
            .ok_or_else(|| std::io::Error::other("created profile has no access"))?;
        let original_instance_id = profile
            .safety
            .instance_id()
            .ok_or_else(|| std::io::Error::other("created profile has no instance id"))?;
        let substituted_instance_id = [0xab_u8, 0xcd, 0xef]
            .into_iter()
            .map(|byte| ProfileInstanceId::from_bytes([byte; 16]))
            .find(|candidate| {
                config
                    .profiles
                    .iter()
                    .all(|profile| profile.safety.instance_id() != Some(*candidate))
            })
            .ok_or_else(|| std::io::Error::other("no unique replacement instance id"))?;
        config.profiles[profile_index].safety =
            ProfileSafetyPosture::classified(environment, access, substituted_instance_id);
        *self
            .original_instance_id
            .lock()
            .map_err(|_| std::io::Error::other("original instance id lock poisoned"))? =
            Some(original_instance_id);
        *self
            .substituted_instance_id
            .lock()
            .map_err(|_| std::io::Error::other("substituted instance id lock poisoned"))? =
            Some(substituted_instance_id);
        let encoded = toml::to_string(&config)
            .map_err(|_| std::io::Error::other("rewritten v3 config did not serialize"))?;
        fs::write(path, encoded)
    }
}

struct TransplantProfileInstanceAtObservation {
    mutation: MatrixMutation,
    target_id: String,
    source_id: String,
    transplanted_id: String,
    deleted_profile: Option<ConnectionProfile>,
    fired: AtomicBool,
}

impl TransplantProfileInstanceAtObservation {
    fn new(
        mutation: MatrixMutation,
        target_id: &str,
        source_id: &str,
        transplanted_id: &str,
        deleted_profile: Option<ConnectionProfile>,
    ) -> Self {
        Self {
            mutation,
            target_id: target_id.to_owned(),
            source_id: source_id.to_owned(),
            transplanted_id: transplanted_id.to_owned(),
            deleted_profile,
            fired: AtomicBool::new(false),
        }
    }
}

impl MutationFaultInjector for TransplantProfileInstanceAtObservation {
    fn check(&self, point: MutationFailpoint, path: &Path) -> std::io::Result<()> {
        if point != MutationFailpoint::MainObservationLoad
            || self.fired.swap(true, Ordering::SeqCst)
        {
            return Ok(());
        }
        let mut config = load_path(path)
            .map_err(|_| std::io::Error::other("committed v3 config was not readable"))?
            .config;
        if config
            .profiles
            .iter()
            .any(|profile| profile.id == self.transplanted_id)
        {
            return Err(std::io::Error::other(
                "transplant destination unexpectedly exists",
            ));
        }
        match self.mutation {
            MatrixMutation::Create | MatrixMutation::Update => {
                if !config
                    .profiles
                    .iter()
                    .any(|profile| profile.id == self.target_id)
                {
                    return Err(std::io::Error::other(
                        "intended create or update target result is absent",
                    ));
                }
                let source = config
                    .profiles
                    .iter_mut()
                    .find(|profile| profile.id == self.source_id)
                    .ok_or_else(|| std::io::Error::other("transplant source is absent"))?;
                source.id.clone_from(&self.transplanted_id);
            }
            MatrixMutation::Delete => {
                if config
                    .profiles
                    .iter()
                    .any(|profile| profile.id == self.target_id)
                {
                    return Err(std::io::Error::other(
                        "intended delete target result is still present",
                    ));
                }
                let mut transplanted = self
                    .deleted_profile
                    .clone()
                    .ok_or_else(|| std::io::Error::other("deleted transplant source is absent"))?;
                transplanted.id.clone_from(&self.transplanted_id);
                config.profiles.push(transplanted);
            }
        }
        let encoded = toml::to_string(&config)
            .map_err(|_| std::io::Error::other("transplanted v3 config did not serialize"))?;
        fs::write(path, encoded)
    }
}

#[derive(Clone, Copy, Debug)]
enum ObservationRewrite {
    DuplicateId,
    InvalidId,
    BlankHost,
    V1,
    Missing,
}

struct RewriteAtObservation {
    rewrite: ObservationRewrite,
    fired: AtomicBool,
}

struct MainObservationBarrier {
    entered: (Mutex<bool>, Condvar),
    released: (Mutex<bool>, Condvar),
}

impl MainObservationBarrier {
    fn new() -> Self {
        Self {
            entered: (Mutex::new(false), Condvar::new()),
            released: (Mutex::new(false), Condvar::new()),
        }
    }

    fn wait_until_entered(&self) {
        let (lock, condition) = &self.entered;
        let entered = lock.lock().expect("observation entered lock");
        drop(
            condition
                .wait_while(entered, |entered| !*entered)
                .expect("observation entered wait"),
        );
    }

    fn release(&self) {
        let (lock, condition) = &self.released;
        *lock.lock().expect("observation release lock") = true;
        condition.notify_all();
    }
}

impl MutationFaultInjector for MainObservationBarrier {
    fn check(&self, point: MutationFailpoint, _path: &Path) -> std::io::Result<()> {
        if point != MutationFailpoint::MainObservationLoad {
            return Ok(());
        }
        let (entered_lock, entered_condition) = &self.entered;
        *entered_lock
            .lock()
            .map_err(|_| std::io::Error::other("observation entered lock poisoned"))? = true;
        entered_condition.notify_all();
        let (release_lock, release_condition) = &self.released;
        let released = release_lock
            .lock()
            .map_err(|_| std::io::Error::other("observation release lock poisoned"))?;
        drop(
            release_condition
                .wait_while(released, |released| !*released)
                .map_err(|_| std::io::Error::other("observation release wait poisoned"))?,
        );
        Ok(())
    }
}

impl RewriteAtObservation {
    fn new(rewrite: ObservationRewrite) -> Self {
        Self {
            rewrite,
            fired: AtomicBool::new(false),
        }
    }
}

impl MutationFaultInjector for RewriteAtObservation {
    fn check(&self, point: MutationFailpoint, path: &Path) -> std::io::Result<()> {
        if point != MutationFailpoint::MainObservationLoad
            || self.fired.swap(true, Ordering::SeqCst)
        {
            return Ok(());
        }
        match self.rewrite {
            ObservationRewrite::V1 => fs::write(path, b"version = 1\nprofiles = []\n"),
            ObservationRewrite::Missing => fs::remove_file(path),
            rewrite => {
                let mut config = load_path(path)
                    .map_err(|error| std::io::Error::other(format!("{error:?}")))?
                    .config;
                let mut injected = config
                    .profiles
                    .first()
                    .cloned()
                    .ok_or_else(|| std::io::Error::other("missing rewrite seed"))?;
                let environment = injected
                    .safety
                    .environment()
                    .ok_or_else(|| std::io::Error::other("rewrite seed has no environment"))?;
                let access = injected
                    .safety
                    .access()
                    .ok_or_else(|| std::io::Error::other("rewrite seed has no access"))?;
                let instance_id = (0_u8..=u8::MAX)
                    .map(|byte| ProfileInstanceId::from_bytes([byte; 16]))
                    .find(|candidate| {
                        config
                            .profiles
                            .iter()
                            .all(|profile| profile.safety.instance_id() != Some(*candidate))
                    })
                    .ok_or_else(|| std::io::Error::other("no distinct rewrite identity"))?;
                injected.safety =
                    ProfileSafetyPosture::classified(environment, access, instance_id);
                match rewrite {
                    ObservationRewrite::DuplicateId => {}
                    ObservationRewrite::InvalidId => {
                        injected.id = " invalid-observed-id".to_owned();
                    }
                    ObservationRewrite::BlankHost => {
                        injected.id = "semantic-invalid-observed".to_owned();
                        injected.host.clear();
                    }
                    ObservationRewrite::V1 | ObservationRewrite::Missing => {
                        return Err(std::io::Error::other("unreachable rewrite branch"));
                    }
                }
                config.profiles.push(injected);
                let encoded = toml::to_string(&config).map_err(std::io::Error::other)?;
                fs::write(path, encoded)
            }
        }
    }
}

struct FailOnceAt {
    point: MutationFailpoint,
    failed: AtomicBool,
}

impl FailOnceAt {
    fn new(point: MutationFailpoint) -> Self {
        Self {
            point,
            failed: AtomicBool::new(false),
        }
    }

    fn has_failed(&self) -> bool {
        self.failed.load(Ordering::SeqCst)
    }
}

impl MutationFaultInjector for FailOnceAt {
    fn check(&self, point: MutationFailpoint, _path: &Path) -> std::io::Result<()> {
        if point == self.point && !self.failed.swap(true, Ordering::SeqCst) {
            Err(std::io::Error::other("injected once"))
        } else {
            Ok(())
        }
    }
}

struct PreRenameBarrier {
    armed: AtomicBool,
    entered: (Mutex<bool>, Condvar),
    released: (Mutex<bool>, Condvar),
    store: Arc<SessionSecretStore>,
    profile_id: ProfileId,
    saw_current_at_release: AtomicBool,
}

impl PreRenameBarrier {
    fn new(store: Arc<SessionSecretStore>, profile_id: ProfileId) -> Self {
        Self {
            armed: AtomicBool::new(false),
            entered: (Mutex::new(false), Condvar::new()),
            released: (Mutex::new(false), Condvar::new()),
            store,
            profile_id,
            saw_current_at_release: AtomicBool::new(false),
        }
    }

    fn arm(&self) {
        self.armed.store(true, Ordering::SeqCst);
    }

    fn wait_until_entered(&self) {
        let (lock, condition) = &self.entered;
        let entered = lock.lock().expect("entered lock");
        drop(
            condition
                .wait_while(entered, |entered| !*entered)
                .expect("entered wait"),
        );
    }

    fn release(&self) {
        let (lock, condition) = &self.released;
        *lock.lock().expect("release lock") = true;
        condition.notify_all();
    }
}

impl MutationFaultInjector for PreRenameBarrier {
    fn check(&self, point: MutationFailpoint, _path: &Path) -> std::io::Result<()> {
        if point != MutationFailpoint::MainPreRename || !self.armed.swap(false, Ordering::SeqCst) {
            return Ok(());
        }
        let (entered_lock, entered_condition) = &self.entered;
        *entered_lock
            .lock()
            .map_err(|_| std::io::Error::other("entered lock poisoned"))? = true;
        entered_condition.notify_all();

        let (release_lock, release_condition) = &self.released;
        let released = release_lock
            .lock()
            .map_err(|_| std::io::Error::other("release lock poisoned"))?;
        drop(
            release_condition
                .wait_while(released, |released| !*released)
                .map_err(|_| std::io::Error::other("release wait poisoned"))?,
        );
        self.saw_current_at_release.store(
            self.store.has_current(&self.profile_id).unwrap_or(false),
            Ordering::SeqCst,
        );
        Ok(())
    }
}

fn service(path: &Path, connector: Arc<FakeConnector>, writer: ConfigWriter) -> ApplicationService {
    ApplicationService::with_dependencies(
        path,
        connector,
        Arc::new(MissingEnvironment),
        Arc::new(SessionSecretStore::default()),
        writer,
    )
    .expect("service")
}

fn create_request(
    draft_id: DraftId,
    operation_id: OperationId,
    explicit_id: Option<&str>,
    draft: ConnectionDraft,
    secret_update: SessionSecretUpdate,
) -> CreateProfileRequest {
    CreateProfileRequest {
        draft_id,
        operation_id,
        explicit_id: explicit_id.map(|value| ProfileId(value.to_owned())),
        draft,
        secret_update,
        migration_consent: MigrationConsent::Confirmed,
    }
}

fn draft(driver: DriverKind) -> ConnectionDraft {
    named_draft(driver, "Profile")
}

fn named_draft(driver: DriverKind, name: &str) -> ConnectionDraft {
    let mut draft = ConnectionDraft::for_driver(driver);
    draft.name = name.to_owned();
    draft
}

fn classified_profile(id: &str, draft: ConnectionDraft, identity_byte: u8) -> ConnectionProfile {
    classify_profile(
        ConnectionProfile::from_draft(id.to_owned(), draft),
        identity_byte,
    )
}

fn classify_profile(mut profile: ConnectionProfile, identity_byte: u8) -> ConnectionProfile {
    let environment = profile
        .safety
        .environment()
        .expect("fixture profile starts with an explicit environment");
    let access = profile
        .safety
        .access()
        .expect("fixture profile starts with explicit access");
    profile.safety = ProfileSafetyPosture::classified(
        environment,
        access,
        ProfileInstanceId::from_bytes([identity_byte; 16]),
    );
    profile
}

fn write_v3_profiles(path: &Path, profiles: Vec<ConnectionProfile>) -> Vec<ConnectionProfile> {
    fs::write(
        path,
        toml::to_string(&Config {
            version: 3,
            profiles,
        })
        .expect("classified v3 fixture serializes"),
    )
    .expect("classified v3 fixture writes");
    let loaded = load_path(path).expect("classified v3 fixture reloads");
    assert_eq!(loaded.source_version, ConfigSourceVersion::V3);
    loaded.config.profiles
}

fn persisted_profile(profiles: &[ConnectionProfile], id: &str) -> ConnectionProfile {
    profiles
        .iter()
        .find(|profile| profile.id == id)
        .expect("persisted fixture profile")
        .clone()
}

fn explicit_migration_document(writer: &ConfigWriter, path: &Path) -> Vec<u8> {
    let plan = writer.migration_plan(path).expect("legacy migration plan");
    assert!(matches!(plan.source_version, 1 | 2));
    let assignments = plan
        .profiles
        .iter()
        .map(|profile| {
            serde_json::json!({
                "profile_id": profile.profile_id,
                "environment": "development",
                "access": "read_write"
            })
        })
        .collect::<Vec<_>>();
    serde_json::to_vec(&serde_json::json!({
        "config_fingerprint": plan.config_fingerprint,
        "profiles": assignments
    }))
    .expect("migration posture document")
}

const fn is_backup_failpoint(point: MutationFailpoint) -> bool {
    matches!(
        point,
        MutationFailpoint::BackupTempCreate
            | MutationFailpoint::BackupWrite
            | MutationFailpoint::BackupFileSync
            | MutationFailpoint::BackupRename
            | MutationFailpoint::BackupDirectorySync
    )
}

fn empty_result() -> QueryResult {
    QueryResult {
        columns: Vec::new(),
        rows: Vec::new(),
        affected_rows: 0,
        last_insert_id: None,
        elapsed_ms: 0,
        truncated: false,
        backend_notices_present: false,
    }
}
