use std::fs;
use std::sync::Arc;
use std::time::Duration;

#[path = "common/live_evidence.rs"]
mod live_evidence;

use dbotter::config::{Config, ConfigWriter};
use dbotter::drivers::{self, DriverError, MySqlPreparedExecution, Session};
use dbotter::execution::{ExecutionLanguage, ExecutionTargetError, extract_and_validate_target};
use dbotter::model::{
    Cell, ConnectionProfile, CredentialMode, DriverKind, OperationId, OperationKind,
    PreparedMySqlRequest, ProfileFieldId, ProfileGeneration, ProfileId, PublicCode, PublicSummary,
    RedisTlsConfig, RequestIdentity, TlsMode,
};
use dbotter::public_error::{RecoveryAction, SafeContext, recovery_for};
use dbotter::secrets::{SecretError, SessionSecret, SessionSecretStore, SessionSecretUpdate};
use dbotter::service::{
    ApplicationService, DriverConnector, SecretResolver, ServiceError, SessionDisposition,
};
use live_evidence::LiveEvidence;
use secrecy::SecretString;

const CORRECT_PASSWORD: &str = "dbotter-local-only";
const WRONG_PASSWORD: &str = "dbotter-definitely-wrong";
const ENV_NAME: &str = "DBOTTER_MYSQL_PASSWORD";
const TIMEOUT: Duration = Duration::from_secs(10);
const MARKER_TEXT: &str = "INSERT INTO dbotter_live_marker VALUES ('first'); INSERT INTO dbotter_live_marker VALUES ('second')";

fn live_port() -> u16 {
    std::env::var("DBOTTER_TEST_MYSQL_PORT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(33_306)
}

fn profile(id: &str, credential_mode: CredentialMode) -> ConnectionProfile {
    ConnectionProfile {
        id: id.to_owned(),
        name: format!("Live MySQL {id}"),
        driver: DriverKind::MySql,
        host: "127.0.0.1".to_owned(),
        port: live_port(),
        database: Some("dbotter".to_owned()),
        username: Some("dbotter".to_owned()),
        tls: TlsMode::Disabled,
        credential_mode,
        secret_env: (credential_mode == CredentialMode::Environment).then(|| ENV_NAME.to_owned()),
        redis_tls: RedisTlsConfig::default(),
    }
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
}

async fn service_check(
    profile: ConnectionProfile,
    resolver: ResolverState,
    session_password: Option<&str>,
) -> Result<(), ServiceError> {
    let directory = tempfile::tempdir().expect("MySQL auth tempdir");
    let path = directory.path().join("config.toml");
    fs::write(
        &path,
        toml::to_string(&Config {
            version: 2,
            profiles: vec![profile.clone()],
        })
        .expect("serialize MySQL auth profile"),
    )
    .expect("write MySQL auth profile");
    let store = Arc::new(SessionSecretStore::default());
    if let Some(password) = session_password {
        store
            .apply(
                &ProfileId(profile.id.clone()),
                SessionSecretUpdate::Replace(Arc::new(SessionSecret::new(password.to_owned()))),
            )
            .expect("install MySQL Session credential");
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
    .expect("typed Authentication recovery");
    assert_eq!(actions.as_slice(), &[expected]);
}

async fn assert_auth_matrix(evidence: &mut LiveEvidence) -> usize {
    let session_correct = evidence.begin("mysql.auth.session.correct");
    service_check(
        profile("session-correct", CredentialMode::Session),
        ResolverState::Missing,
        Some(CORRECT_PASSWORD),
    )
    .await
    .expect("Session correct");
    evidence.pass(session_correct);

    let session_wrong = evidence.begin("mysql.auth.session.wrong");
    let session_error = service_check(
        profile("session-wrong", CredentialMode::Session),
        ResolverState::Missing,
        Some(WRONG_PASSWORD),
    )
    .await
    .expect_err("Session wrong must fail");
    evidence.pass(session_wrong);
    let session_code = evidence.begin("mysql.auth.session.wrong.code");
    assert_auth_code(&session_error, PublicCode::SessionCredential);
    evidence.pass(session_code);
    let session_action = evidence.begin("mysql.auth.session.wrong.action");
    assert_auth_action(
        &session_error,
        "session-wrong",
        RecoveryAction::OpenCredentialPrompt(ProfileId("session-wrong".to_owned())),
    );
    evidence.pass(session_action);
    let session_recovery = evidence.begin("mysql.auth.session.wrong.recovery");
    service_check(
        profile("session-recovered", CredentialMode::Session),
        ResolverState::Missing,
        Some(CORRECT_PASSWORD),
    )
    .await
    .expect("Session recovery");
    evidence.pass(session_recovery);

    let environment_correct = evidence.begin("mysql.auth.environment.available.correct");
    service_check(
        profile("environment-correct", CredentialMode::Environment),
        ResolverState::Available(CORRECT_PASSWORD.to_owned()),
        None,
    )
    .await
    .expect("Environment Available correct");
    evidence.pass(environment_correct);

    let environment_wrong = evidence.begin("mysql.auth.environment.available.wrong");
    let environment_wrong_error = service_check(
        profile("environment-wrong", CredentialMode::Environment),
        ResolverState::Available(WRONG_PASSWORD.to_owned()),
        None,
    )
    .await
    .expect_err("Environment Available wrong must fail");
    evidence.pass(environment_wrong);
    let environment_wrong_code = evidence.begin("mysql.auth.environment.available.wrong.code");
    assert_auth_code(
        &environment_wrong_error,
        PublicCode::CredentialEnvironmentName,
    );
    evidence.pass(environment_wrong_code);
    let environment_wrong_action = evidence.begin("mysql.auth.environment.available.wrong.action");
    assert_auth_action(
        &environment_wrong_error,
        "environment-wrong",
        RecoveryAction::EditProfile(
            ProfileId("environment-wrong".to_owned()),
            ProfileFieldId::CredentialEnvironmentName,
        ),
    );
    evidence.pass(environment_wrong_action);
    let environment_wrong_recovery =
        evidence.begin("mysql.auth.environment.available.wrong.recovery");
    service_check(
        profile("environment-wrong-recovered", CredentialMode::Environment),
        ResolverState::Available(CORRECT_PASSWORD.to_owned()),
        None,
    )
    .await
    .expect("Environment wrong recovery");
    evidence.pass(environment_wrong_recovery);

    for (state, label) in [
        (ResolverState::Missing, "missing"),
        (ResolverState::Empty, "empty"),
    ] {
        let profile_id = format!("environment-{label}");
        let failed_case = match label {
            "missing" => "mysql.auth.environment.missing",
            "empty" => "mysql.auth.environment.empty",
            _ => unreachable!(),
        };
        let failed = evidence.begin(failed_case);
        let error = service_check(
            profile(&profile_id, CredentialMode::Environment),
            state,
            None,
        )
        .await
        .expect_err("unavailable Environment credential must fail");
        evidence.pass(failed);

        let code_case = match label {
            "missing" => "mysql.auth.environment.missing.code",
            "empty" => "mysql.auth.environment.empty.code",
            _ => unreachable!(),
        };
        let code = evidence.begin(code_case);
        assert_auth_code(&error, PublicCode::CredentialEnvironmentName);
        evidence.pass(code);

        let action_case = match label {
            "missing" => "mysql.auth.environment.missing.action",
            "empty" => "mysql.auth.environment.empty.action",
            _ => unreachable!(),
        };
        let action = evidence.begin(action_case);
        assert_auth_action(
            &error,
            &profile_id,
            RecoveryAction::EditProfile(
                ProfileId(profile_id.clone()),
                ProfileFieldId::CredentialEnvironmentName,
            ),
        );
        evidence.pass(action);

        let recovery_case = match label {
            "missing" => "mysql.auth.environment.missing.recovery",
            "empty" => "mysql.auth.environment.empty.recovery",
            _ => unreachable!(),
        };
        let recovery = evidence.begin(recovery_case);
        service_check(
            profile(
                &format!("{profile_id}-recovered"),
                CredentialMode::Environment,
            ),
            ResolverState::Available(CORRECT_PASSWORD.to_owned()),
            None,
        )
        .await
        .expect("unavailable Environment recovery");
        evidence.pass(recovery);
    }

    4
}

async fn mysql_session() -> Session {
    let password = std::env::var(ENV_NAME).expect("MySQL fixture password");
    assert_eq!(password, CORRECT_PASSWORD);
    drivers::connect(
        &profile("mysql-live-safety", CredentialMode::Session),
        Some(&SecretString::from(password)),
        TIMEOUT,
    )
    .await
    .expect("connect MySQL safety session")
}

async fn execute(
    session: &Session,
    operation: u64,
    statement: &str,
) -> Result<dbotter::model::QueryResult, DriverError> {
    session
        .execute_prepared(&PreparedMySqlRequest {
            identity: RequestIdentity::new(
                ProfileId("mysql-live-safety".to_owned()),
                ProfileGeneration(1),
                OperationId(operation),
            ),
            statement: statement.to_owned(),
            row_limit: 100,
            timeout: TIMEOUT,
        })
        .await
}

fn text_cell(result: &dbotter::model::QueryResult) -> &str {
    match &result.rows[0][0] {
        Cell::Text(value) => value,
        other => panic!("expected one text cell, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires scripts/verify-live-contracts.sh MySQL fixture"]
async fn live_mysql_safety_receipt() {
    let mut evidence = LiveEvidence::required(
        "mysql_safety",
        "live_mysql_safety_receipt",
        "DBOTTER_LIVE_MYSQL_SAFETY_EVIDENCE",
    )
    .expect("initialize MySQL safety evidence");
    let auth_failures = assert_auth_matrix(&mut evidence).await;

    let session = mysql_session().await;
    execute(&session, 1, "DROP TABLE IF EXISTS dbotter_live_execute")
        .await
        .expect("drop execute fixture");
    execute(
        &session,
        2,
        "CREATE TABLE dbotter_live_execute (id INT PRIMARY KEY, marker VARCHAR(32) NOT NULL)",
    )
    .await
    .expect("create execute fixture");

    let execute_read = evidence.begin("mysql.execute.read");
    let read = execute(&session, 3, "SELECT 42 AS value")
        .await
        .expect("prepared read");
    assert_eq!(read.rows, [vec![Cell::Int(42)]]);
    assert_eq!(read.affected_rows, 0);
    evidence.pass(execute_read);

    let execute_mutation = evidence.begin("mysql.execute.mutation");
    let mutation = execute(
        &session,
        4,
        "INSERT INTO dbotter_live_execute (id, marker) VALUES (1, 'measured')",
    )
    .await
    .expect("prepared mutation");
    assert!(mutation.columns.is_empty());
    assert!(mutation.rows.is_empty());
    assert_eq!(mutation.affected_rows, 1);
    let mutation_readback = execute(
        &session,
        5,
        "SELECT marker FROM dbotter_live_execute WHERE id = 1",
    )
    .await
    .expect("mutation readback");
    assert_eq!(text_cell(&mutation_readback), "measured");
    evidence.pass(execute_mutation);

    execute(&session, 6, "DROP TABLE IF EXISTS dbotter_live_marker")
        .await
        .expect("drop marker fixture");
    execute(
        &session,
        7,
        "CREATE TABLE dbotter_live_marker (marker VARCHAR(32) PRIMARY KEY)",
    )
    .await
    .expect("create marker fixture");

    let ui_rejection = evidence.begin("mysql.marker.explicit_selection.ui_rejected");
    let ui_error = extract_and_validate_target(
        MARKER_TEXT,
        MARKER_TEXT.chars().count(),
        Some(0..MARKER_TEXT.chars().count()),
        ExecutionLanguage::MySql,
        100,
        10,
    )
    .expect_err("explicit two-statement selection must be rejected locally");
    assert_eq!(ui_error, ExecutionTargetError::MultipleStatements);
    evidence.pass(ui_rejection);

    let explicit_prepare = evidence.begin("mysql.marker.explicit_selection.prepare_only_rejected");
    let explicit_error = execute(&session, 8, MARKER_TEXT)
        .await
        .expect_err("explicit selection adapter must stop at server prepare");
    assert!(matches!(explicit_error, DriverError::MySql(_)));
    evidence.pass(explicit_prepare);
    let explicit_absent = evidence.begin("mysql.marker.explicit_selection.absent");
    let after_explicit = execute(
        &session,
        9,
        "SELECT marker FROM dbotter_live_marker ORDER BY marker",
    )
    .await
    .expect("read marker after explicit selection rejection");
    assert!(after_explicit.rows.is_empty());
    evidence.pass(explicit_absent);

    let current_prepare = evidence.begin("mysql.marker.current_target.prepare_only_rejected");
    let current_error = execute(&session, 10, MARKER_TEXT)
        .await
        .expect_err("current-target adapter must stop at server prepare");
    assert!(matches!(current_error, DriverError::MySql(_)));
    evidence.pass(current_prepare);
    let current_absent = evidence.begin("mysql.marker.current_target.absent");
    let after_current = execute(
        &session,
        11,
        "SELECT marker FROM dbotter_live_marker ORDER BY marker",
    )
    .await
    .expect("read marker after current-target rejection");
    assert!(after_current.rows.is_empty());
    evidence.pass(current_absent);

    let unsupported_error_checkpoint = evidence.begin("mysql.prepared_unsupported.error");
    let unsupported = execute(&session, 12, "USE information_schema")
        .await
        .expect_err("USE must be unsupported by the prepared protocol");
    assert!(matches!(
        &unsupported,
        DriverError::PreparedStatementUnsupported {
            session_healthy: true
        }
    ));
    evidence.pass(unsupported_error_checkpoint);

    let retained = evidence.begin("mysql.prepared_unsupported.session_retained");
    assert_eq!(
        SessionDisposition::for_driver_error(&unsupported),
        SessionDisposition::Keep
    );
    let retained_read = execute(&session, 13, "SELECT 1 AS healthy")
        .await
        .expect("same session remains healthy after prepared unsupported");
    assert_eq!(retained_read.rows, [vec![Cell::Int(1)]]);
    evidence.pass(retained);

    let no_fallback = evidence.begin("mysql.prepared_unsupported.no_raw_fallback");
    let database = execute(&session, 14, "SELECT DATABASE()")
        .await
        .expect("read current database after unsupported prepare");
    let raw_fallback_attempts = usize::from(text_cell(&database) != "dbotter");
    assert_eq!(
        raw_fallback_attempts, 0,
        "raw USE fallback changed database"
    );
    evidence.pass(no_fallback);

    let static_recovery = evidence.begin("mysql.prepared_unsupported.static_recovery");
    let service_error = ServiceError::from(unsupported);
    assert_eq!(
        service_error.public_error_parts(),
        (
            PublicSummary::UnsupportedFeature,
            PublicCode::PreparedStatementUnsupported,
        )
    );
    let recovery_profile = ProfileId("mysql-live-safety".to_owned());
    let recovery = recovery_for(
        OperationKind::ExecuteRead,
        service_error.public_summary(),
        service_error.public_code(),
        &SafeContext::profile(recovery_profile.clone(), OperationId(12)),
    )
    .expect("prepared unsupported recovery");
    assert_eq!(
        recovery.as_slice(),
        &[
            RecoveryAction::FocusEditor(recovery_profile),
            RecoveryAction::DismissError(OperationId(12)),
        ]
    );
    evidence.pass(static_recovery);

    evidence
        .measure("auth_failures", auth_failures)
        .expect("auth failure count");
    evidence
        .measure("marker_prepared_attempts", 2)
        .expect("marker prepare count");
    evidence
        .measure("marker_rows_after", after_current.rows.len())
        .expect("marker rows after");
    evidence
        .measure("prepared_unsupported_attempts", 1)
        .expect("prepared unsupported count");
    evidence
        .measure("raw_fallback_attempts", raw_fallback_attempts)
        .expect("raw fallback count");
    evidence
        .measure("statements_executed", 2)
        .expect("required statement count");
    evidence.finish().expect("publish MySQL safety evidence");
}
