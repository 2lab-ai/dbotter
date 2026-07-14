use std::fs;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use dbotter::config::Config;
use dbotter::drivers::DriverError;
use dbotter::model::{
    ConnectionProfile, DriverKind, ExecuteRequest, OperationId, ProfileId, QueryLanguage,
    QueryResult, TlsMode,
};
use dbotter::secrets::SecretError;
use dbotter::service::{
    ApplicationService, SecretResolver, ServiceError, SessionConnector, SessionHandle,
};
use secrecy::SecretString;

#[derive(Default)]
struct FakeSession {
    pings: AtomicUsize,
    executes: AtomicUsize,
}

#[async_trait]
impl SessionHandle for FakeSession {
    async fn ping(&self, _timeout: Duration) -> Result<(), DriverError> {
        self.pings.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn execute(&self, _request: &ExecuteRequest) -> Result<QueryResult, DriverError> {
        self.executes.fetch_add(1, Ordering::SeqCst);
        Ok(empty_result())
    }
}

struct FakeConnector {
    connects: AtomicUsize,
    session: Arc<FakeSession>,
}

#[async_trait]
impl SessionConnector for FakeConnector {
    async fn connect(
        &self,
        _profile: &ConnectionProfile,
        _secret: Option<&SecretString>,
        _timeout: Duration,
    ) -> Result<Arc<dyn SessionHandle>, DriverError> {
        self.connects.fetch_add(1, Ordering::SeqCst);
        Ok(self.session.clone())
    }
}

#[derive(Default)]
struct NoSecrets;

impl SecretResolver for NoSecrets {
    fn resolve(&self, _secret_env: Option<&str>) -> Result<Option<SecretString>, SecretError> {
        Ok(None)
    }
}

struct MissingSecrets;

impl SecretResolver for MissingSecrets {
    fn resolve(&self, secret_env: Option<&str>) -> Result<Option<SecretString>, SecretError> {
        Err(SecretError::MissingEnv(
            secret_env.unwrap_or("missing").to_owned(),
        ))
    }
}

#[tokio::test]
async fn check_then_execute_reuses_session_and_preserves_correlation() {
    let session = Arc::new(FakeSession::default());
    let connector = Arc::new(FakeConnector {
        connects: AtomicUsize::new(0),
        session: session.clone(),
    });
    let service = ApplicationService::new(
        config(profile(DriverKind::MySql, None)),
        connector.clone(),
        Arc::new(NoSecrets),
    );
    let profile_id = ProfileId("profile".to_owned());

    let check = service
        .check(OperationId(41), profile_id.clone(), Duration::from_secs(1))
        .await
        .expect("check succeeds");
    let execute = service
        .execute(ExecuteRequest {
            operation_id: OperationId(42),
            profile_id: profile_id.clone(),
            language: QueryLanguage::Sql,
            text: "SELECT 1".to_owned(),
            row_limit: 100,
            timeout: Duration::from_secs(1),
        })
        .await
        .expect("execute succeeds");

    assert_eq!(connector.connects.load(Ordering::SeqCst), 1);
    assert_eq!(session.pings.load(Ordering::SeqCst), 1);
    assert_eq!(session.executes.load(Ordering::SeqCst), 1);
    assert_eq!(check.operation_id, OperationId(41));
    assert_eq!(check.profile_id, profile_id);
    assert_eq!(execute.operation_id, OperationId(42));
    assert_eq!(execute.profile_id, ProfileId("profile".to_owned()));
}

#[tokio::test]
async fn planned_mongodb_is_rejected_before_connector() {
    let connector = Arc::new(FakeConnector {
        connects: AtomicUsize::new(0),
        session: Arc::new(FakeSession::default()),
    });
    let service = ApplicationService::new(
        config(profile(DriverKind::MongoDb, None)),
        connector.clone(),
        Arc::new(NoSecrets),
    );

    let error = service
        .check(
            OperationId(1),
            ProfileId("profile".to_owned()),
            Duration::from_secs(1),
        )
        .await
        .expect_err("MongoDB is planned");

    assert!(matches!(
        error,
        ServiceError::Driver(DriverError::Unavailable {
            driver: DriverKind::MongoDb,
            ..
        })
    ));
    assert_eq!(connector.connects.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn missing_secret_is_rejected_before_connector() {
    let connector = Arc::new(FakeConnector {
        connects: AtomicUsize::new(0),
        session: Arc::new(FakeSession::default()),
    });
    let service = ApplicationService::new(
        config(profile(DriverKind::MySql, Some("DBOTTER_TEST_PASSWORD"))),
        connector.clone(),
        Arc::new(MissingSecrets),
    );

    let error = service
        .check(
            OperationId(1),
            ProfileId("profile".to_owned()),
            Duration::from_secs(1),
        )
        .await
        .expect_err("secret is missing");

    assert!(matches!(
        error,
        ServiceError::Secret(SecretError::MissingEnv(_))
    ));
    assert_eq!(connector.connects.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn saved_profile_is_immediately_available_to_check_and_execute() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");
    let session = Arc::new(FakeSession::default());
    let connector = Arc::new(FakeConnector {
        connects: AtomicUsize::new(0),
        session: session.clone(),
    });
    let service =
        ApplicationService::new(Config::default(), connector.clone(), Arc::new(NoSecrets));
    let saved = profile(DriverKind::MySql, None);
    let profile_id = service
        .upsert_profile_path(&path, saved)
        .await
        .expect("profile saves");

    service
        .check(OperationId(51), profile_id.clone(), Duration::from_secs(1))
        .await
        .expect("saved profile checks without service reconstruction");
    service
        .execute(ExecuteRequest {
            operation_id: OperationId(52),
            profile_id,
            language: QueryLanguage::Sql,
            text: "SELECT 1".to_owned(),
            row_limit: 10,
            timeout: Duration::from_secs(1),
        })
        .await
        .expect("saved profile executes without service reconstruction");

    assert_eq!(connector.connects.load(Ordering::SeqCst), 1);
    assert_eq!(session.pings.load(Ordering::SeqCst), 1);
    assert_eq!(session.executes.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn editing_connection_fields_invalidates_only_that_cached_session() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");
    let connector = Arc::new(FakeConnector {
        connects: AtomicUsize::new(0),
        session: Arc::new(FakeSession::default()),
    });
    let original = profile(DriverKind::MySql, None);
    let mut unchanged = profile(DriverKind::Redis, None);
    unchanged.id = "unchanged".to_owned();
    unchanged.name = "Unchanged Redis".to_owned();
    let initial = Config {
        version: 1,
        profiles: vec![original.clone(), unchanged.clone()],
    };
    dbotter::config::save_path(&path, &initial).expect("initial config saves");
    let service = ApplicationService::new(initial, connector.clone(), Arc::new(NoSecrets));
    let profile_id = ProfileId(original.id.clone());
    let unchanged_id = ProfileId(unchanged.id);
    service
        .check(OperationId(61), profile_id.clone(), Duration::from_secs(1))
        .await
        .expect("initial check");
    service
        .check(
            OperationId(62),
            unchanged_id.clone(),
            Duration::from_secs(1),
        )
        .await
        .expect("unchanged profile caches");

    let mut edited = original;
    edited.port = 3307;
    service
        .upsert_profile_path(&path, edited)
        .await
        .expect("edited profile saves");
    service
        .check(OperationId(63), profile_id, Duration::from_secs(1))
        .await
        .expect("edited profile reconnects");
    service
        .check(OperationId(64), unchanged_id, Duration::from_secs(1))
        .await
        .expect("unchanged profile reuses its cached session");

    assert_eq!(connector.connects.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn invalid_direct_profile_is_rejected_without_config_mutation() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");
    let service = ApplicationService::new(
        Config::default(),
        Arc::new(FakeConnector {
            connects: AtomicUsize::new(0),
            session: Arc::new(FakeSession::default()),
        }),
        Arc::new(NoSecrets),
    );
    let mut invalid = profile(DriverKind::Redis, None);
    invalid.host.clear();

    let error = service
        .upsert_profile_path(&path, invalid)
        .await
        .expect_err("invalid direct profile is rejected");

    assert!(matches!(error, ServiceError::InvalidProfile(_)));
    assert!(!path.exists());
    assert!(service.profiles_snapshot().await.is_empty());
}

#[tokio::test]
async fn profile_upsert_persists_secret_env_name_not_secret_literal() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");
    let service = ApplicationService::new(
        Config::default(),
        Arc::new(FakeConnector {
            connects: AtomicUsize::new(0),
            session: Arc::new(FakeSession::default()),
        }),
        Arc::new(NoSecrets),
    );
    let saved = profile(DriverKind::MySql, Some("DBOTTER_TEST_PASSWORD"));
    service
        .upsert_profile_path(&path, saved)
        .await
        .expect("profile saves");
    let encoded = fs::read_to_string(path).expect("saved config reads");

    assert!(encoded.contains("DBOTTER_TEST_PASSWORD"));
    let secret_literal = "plain-text-password-must-not-persist";
    assert!(!encoded.contains(secret_literal));
}

fn config(profile: ConnectionProfile) -> Config {
    Config {
        version: 1,
        profiles: vec![profile],
    }
}

fn profile(driver: DriverKind, secret_env: Option<&str>) -> ConnectionProfile {
    ConnectionProfile {
        id: "profile".to_owned(),
        name: "Profile".to_owned(),
        driver,
        host: "127.0.0.1".to_owned(),
        port: match driver {
            DriverKind::MySql => 3306,
            DriverKind::Redis => 6379,
            DriverKind::MongoDb => 27017,
        },
        database: None,
        username: None,
        tls: TlsMode::Disabled,
        secret_env: secret_env.map(str::to_owned),
    }
}

fn empty_result() -> QueryResult {
    QueryResult {
        columns: Vec::new(),
        rows: Vec::new(),
        affected_rows: 0,
        last_insert_id: None,
        elapsed_ms: 0,
        truncated: false,
        notices: Vec::new(),
    }
}
