#![cfg(feature = "desktop")]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use dbotter::config::{ConfigWriter, MigrationConsent, MutationFailpoint, MutationFaultInjector};
use dbotter::drivers::{
    CatalogBrowser, ConnectedResources, ConnectionPing, DriverError, MySqlPreparedExecution,
};
use dbotter::model::{
    CatalogPage, CatalogRequest, ConnectionDraft, CredentialMode, DraftId, DriverKind, OperationId,
    PreparedMySqlRequest, ProfileGeneration, ProfileId, QueryResult, RedisKeyFilter, RedisKeyId,
    RedisKeyInspectRequest, RedisScanRequest, RequestIdentity, ResultId, ResultProvenance,
    ResultRetentionPolicy, ResultSnapshot, SessionGeneration,
};
use dbotter::secrets::{SecretError, SessionSecret, SessionSecretStore, SessionSecretUpdate};
use dbotter::service::{
    ApplicationService, CreateProfileRequest, DeleteProfileRequest, SecretResolver,
    SessionConnector, SessionDisposition, SessionHandle, UpdateProfileRequest,
};
use dbotter::ui::{
    CONTROL_CAPACITY, ConnectionFailureOutcome, EVENT_CAPACITY, MUTATION_CAPACITY, PostCloseState,
    ProfileSnapshot, SubmitError, TaskScope, UiCommand, UiEvent, UiModel, UiPort, WORK_CAPACITY,
    bounded_ports, controller_ports, spawn_with_service,
};

#[test]
fn controller_capacities_and_task_scope_shape_are_exact() {
    assert_eq!(WORK_CAPACITY, 32);
    assert_eq!(MUTATION_CAPACITY, 16);
    assert_eq!(CONTROL_CAPACITY, 16);
    assert_eq!(EVENT_CAPACITY, 128);

    let profile = TaskScope::Profile {
        profile_id: ProfileId("profile".to_owned()),
        profile_generation: ProfileGeneration(7),
        session_generation: Some(SessionGeneration(11)),
    };
    let draft = TaskScope::Draft {
        draft_id: DraftId(13),
    };
    let export = TaskScope::Export {
        result_id: dbotter::model::ResultId(17),
    };
    let global = TaskScope::Global;

    assert!(matches!(profile, TaskScope::Profile { .. }));
    assert!(matches!(draft, TaskScope::Draft { .. }));
    assert!(matches!(export, TaskScope::Export { .. }));
    assert!(matches!(global, TaskScope::Global));
}

#[tokio::test]
async fn cache_identity_uses_profile_and_session_generations_for_compare_remove() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    let connector = Arc::new(CountingConnector::default());
    let service = test_service(&path, connector.clone());
    let created = service
        .create_profile(create_request("cache-race", OperationId(1)))
        .await
        .expect("create");

    let first = service
        .check_at(
            OperationId(2),
            created.profile_id.clone(),
            created.profile_generation,
            Duration::from_secs(1),
        )
        .await
        .expect("first connect");
    assert_eq!(first.profile_generation, created.profile_generation);
    let first_session = first.session_generation;

    assert!(
        service
            .evict_cached_session_exact(
                &created.profile_id,
                created.profile_generation,
                first_session,
            )
            .await
    );

    let replacement = service
        .check_at(
            OperationId(3),
            created.profile_id.clone(),
            created.profile_generation,
            Duration::from_secs(1),
        )
        .await
        .expect("replacement connect");
    assert!(replacement.session_generation.0 > first_session.0);
    assert!(
        !service
            .evict_cached_session_exact(
                &created.profile_id,
                created.profile_generation,
                first_session,
            )
            .await
    );
    assert_eq!(
        service
            .cached_session_identity(&created.profile_id)
            .await
            .map(|identity| identity.session_generation),
        Some(replacement.session_generation)
    );
    assert_eq!(connector.connects.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn cached_fingerprint_debug_is_redacted_and_stale_disconnect_cannot_remove_replacement() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    let connector = Arc::new(CountingConnector::default());
    let service = test_service(&path, connector);
    let mut request = create_request("sentinel-cache", OperationId(4));
    request.draft.host = "host-sentinel-must-not-leak".to_owned();
    request.draft.database = Some("database-sentinel-must-not-leak".to_owned());
    request.draft.username = Some("username-sentinel-must-not-leak".to_owned());
    let created = service
        .create_profile(request)
        .await
        .expect("create sentinel");
    let first = service
        .check_at(
            OperationId(5),
            created.profile_id.clone(),
            created.profile_generation,
            Duration::from_secs(1),
        )
        .await
        .expect("first session");
    let first_identity = service
        .cached_session_identity(&created.profile_id)
        .await
        .expect("cached identity");
    let debug = format!("{first_identity:?}");
    for sentinel in [
        "host-sentinel-must-not-leak",
        "database-sentinel-must-not-leak",
        "username-sentinel-must-not-leak",
    ] {
        assert!(!debug.contains(sentinel));
    }

    assert!(
        service
            .evict_cached_session_exact(
                &created.profile_id,
                created.profile_generation,
                first.session_generation,
            )
            .await
    );
    let replacement = service
        .check_at(
            OperationId(6),
            created.profile_id.clone(),
            created.profile_generation,
            Duration::from_secs(1),
        )
        .await
        .expect("replacement");
    let stale_disconnect = service
        .disconnect_profile_exact(
            OperationId(7),
            &created.profile_id,
            created.profile_generation,
            Some(first.session_generation),
        )
        .await;
    assert!(matches!(
        stale_disconnect,
        Err(dbotter::service::ServiceError::ProfileStale { .. })
    ));
    assert_eq!(
        service
            .cached_session_identity(&created.profile_id)
            .await
            .map(|identity| identity.session_generation),
        Some(replacement.session_generation)
    );
}

#[tokio::test]
async fn delete_publishes_a_tombstone_and_recreation_advances_generation() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    let service = test_service(&path, Arc::new(CountingConnector::default()));
    let created = service
        .create_profile(create_request("recreated", OperationId(10)))
        .await
        .expect("create");
    let deleted = service
        .delete_profile(DeleteProfileRequest {
            profile_id: created.profile_id.clone(),
            expected_generation: created.profile_generation,
            operation_id: OperationId(11),
            migration_consent: MigrationConsent::Cancelled,
        })
        .await
        .expect("delete");

    assert_eq!(
        service.tombstone_generation(&created.profile_id).await,
        Some(deleted.profile_generation)
    );
    let recreated = service
        .create_profile(create_request("recreated", OperationId(12)))
        .await
        .expect("recreate");
    assert!(recreated.profile_generation.0 > deleted.profile_generation.0);
}

#[tokio::test]
async fn idle_delete_reports_known_server_state_while_active_delete_reports_unknown() {
    for active in [false, true] {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("config.toml");
        let gate = Arc::new(AsyncGate::new());
        let connector = Arc::new(ScriptedConnector::new(vec![SessionBehavior::Blocked(
            gate.clone(),
        )]));
        let service = test_service_with_connector(&path, connector);
        let profile = seed_profiles(&service, 1).await.remove(0);
        let (mut ui, service_port) = controller_ports();
        let runtime = spawn_with_service(service_port, service);
        if active {
            submit_test(&ui, &profile, OperationId(13), 5_000).expect("active target");
            gate.wait_until_entered().await;
        }
        ui.try_submit(UiCommand::DeleteProfile(DeleteProfileRequest {
            profile_id: profile.id,
            expected_generation: profile.generation,
            operation_id: OperationId(14),
            migration_consent: MigrationConsent::Cancelled,
        }))
        .expect("delete");
        let event = wait_for_event(&mut ui, |event| {
            matches!(
                event,
                UiEvent::ProfileDeleted {
                    operation_id: OperationId(14),
                    ..
                }
            )
        })
        .await;
        assert!(matches!(
            event,
            UiEvent::ProfileDeleted {
                server_state_unknown,
                ..
            } if server_state_unknown == active
        ));
        shutdown(&ui, runtime, OperationId(15)).await;
    }
}

#[test]
fn ui_fold_rejects_late_profile_events_after_tombstone_and_recreation() {
    let profile_id = ProfileId("folded".to_owned());
    let mut model = UiModel::default();
    model.fold(UiEvent::ProfilesLoaded {
        operation_id: OperationId(19),
        profiles: vec![snapshot(profile_id.clone(), ProfileGeneration(5))],
    });
    model.fold(UiEvent::ProfileDeleted {
        operation_id: OperationId(20),
        profile_id: profile_id.clone(),
        profile_generation: ProfileGeneration(6),
        server_state_unknown: true,
    });
    model.fold(UiEvent::ProfilesLoaded {
        operation_id: OperationId(22),
        profiles: vec![snapshot(profile_id.clone(), ProfileGeneration(7))],
    });
    model.fold(UiEvent::ConnectionReady {
        operation_id: OperationId(21),
        profile_id: profile_id.clone(),
        profile_generation: ProfileGeneration(5),
        session_generation: SessionGeneration(2),
        elapsed_ms: 1,
    });

    assert_eq!(
        model.active_generation(&profile_id),
        Some(ProfileGeneration(7))
    );
    assert_eq!(
        model.tombstone_generation(&profile_id),
        Some(ProfileGeneration(6))
    );
    assert!(matches!(
        model.connection_state(&profile_id),
        dbotter::ui::ConnectionState::Disconnected
    ));
}

#[test]
fn reload_removal_tombstones_late_create_and_ignores_stale_profiles_loaded() {
    let profile_id = ProfileId("reload-removed".to_owned());
    let mut model = UiModel::default();
    model.fold(UiEvent::ProfilesLoaded {
        operation_id: OperationId(70),
        profiles: vec![snapshot(profile_id.clone(), ProfileGeneration(5))],
    });
    model.fold(UiEvent::ProfilesLoaded {
        operation_id: OperationId(72),
        profiles: Vec::new(),
    });
    assert_eq!(
        model.tombstone_generation(&profile_id),
        Some(ProfileGeneration(5)),
        "reload removal must publish a local tombstone fence"
    );

    model.fold(UiEvent::ProfileSaved {
        operation_id: OperationId(71),
        profile_id: profile_id.clone(),
        previous_generation: None,
        profile_generation: ProfileGeneration(5),
        session_retained: false,
        warning: None,
    });
    model.fold(UiEvent::ProfilesLoaded {
        operation_id: OperationId(71),
        profiles: vec![snapshot(profile_id.clone(), ProfileGeneration(9))],
    });
    assert_eq!(model.active_generation(&profile_id), None);

    model.fold(UiEvent::ProfilesLoaded {
        operation_id: OperationId(73),
        profiles: vec![snapshot(profile_id.clone(), ProfileGeneration(5))],
    });
    assert_eq!(model.active_generation(&profile_id), None);

    model.fold(UiEvent::ProfilesLoaded {
        operation_id: OperationId(74),
        profiles: vec![snapshot(profile_id.clone(), ProfileGeneration(6))],
    });
    assert_eq!(
        model.active_generation(&profile_id),
        Some(ProfileGeneration(6)),
        "only a newer generation may recreate a reload-removed profile"
    );
}

#[test]
fn ui_fold_preserves_runtime_neutral_retag_and_uncertain_state_rejects_late_events() {
    let profile_id = ProfileId("retagged".to_owned());
    let mut model = UiModel::default();
    model.fold(UiEvent::ProfilesLoaded {
        operation_id: OperationId(30),
        profiles: vec![snapshot(profile_id.clone(), ProfileGeneration(1))],
    });
    model.connection_states.insert(
        profile_id.clone(),
        dbotter::ui::ConnectionState::Connected {
            session_generation: SessionGeneration(9),
            elapsed_ms: 2,
        },
    );
    model.fold(UiEvent::ProfileSaved {
        operation_id: OperationId(31),
        profile_id: profile_id.clone(),
        previous_generation: Some(ProfileGeneration(1)),
        profile_generation: ProfileGeneration(2),
        session_retained: true,
        warning: None,
    });
    model.fold(UiEvent::ProfilesLoaded {
        operation_id: OperationId(32),
        profiles: vec![snapshot(profile_id.clone(), ProfileGeneration(2))],
    });
    assert!(matches!(
        model.connection_state(&profile_id),
        dbotter::ui::ConnectionState::Connected {
            session_generation: SessionGeneration(9),
            ..
        }
    ));

    model.fold(UiEvent::ConfigUncertain {
        operation_id: OperationId(33),
    });
    assert!(model.is_config_uncertain());
    model.fold(UiEvent::ConnectionReady {
        operation_id: OperationId(34),
        profile_id: profile_id.clone(),
        profile_generation: ProfileGeneration(2),
        session_generation: SessionGeneration(10),
        elapsed_ms: 1,
    });
    assert!(!matches!(
        model.connection_state(&profile_id),
        dbotter::ui::ConnectionState::Connected {
            session_generation: SessionGeneration(10),
            ..
        }
    ));
}

#[test]
fn ui_connection_state_table_covers_credentials_cache_disposition_and_shutdown() {
    let profile_id = ProfileId("state-table".to_owned());
    let other_id = ProfileId("state-table-other".to_owned());
    let generation = ProfileGeneration(1);
    let mut model = UiModel::default();
    model.fold(UiEvent::ProfilesLoaded {
        operation_id: OperationId(40),
        profiles: vec![
            session_snapshot(profile_id.clone(), generation, false),
            snapshot(other_id.clone(), generation),
        ],
    });
    assert!(matches!(
        model.connection_state(&profile_id),
        dbotter::ui::ConnectionState::NeedsCredential
    ));

    model.connection_states.insert(
        profile_id.clone(),
        dbotter::ui::ConnectionState::Pending(OperationId(41)),
    );
    model.fold(UiEvent::ConnectionClosed {
        operation_id: OperationId(41),
        profile_id: profile_id.clone(),
        profile_generation: generation,
        post_close: PostCloseState::NeedsCredential,
    });
    assert!(matches!(
        model.connection_state(&profile_id),
        dbotter::ui::ConnectionState::NeedsCredential
    ));
    model.connection_states.insert(
        profile_id.clone(),
        dbotter::ui::ConnectionState::Pending(OperationId(42)),
    );
    model.fold(UiEvent::ConnectionClosed {
        operation_id: OperationId(42),
        profile_id: profile_id.clone(),
        profile_generation: generation,
        post_close: PostCloseState::Disconnected,
    });
    assert!(matches!(
        model.connection_state(&profile_id),
        dbotter::ui::ConnectionState::Disconnected
    ));

    model.pending_execute = Some((OperationId(43), profile_id.clone(), generation));
    model.fold(UiEvent::QueryFinished {
        operation_id: OperationId(43),
        profile_id: profile_id.clone(),
        profile_generation: generation,
        session_generation: SessionGeneration(7),
        result: empty_query_result(),
    });
    assert!(matches!(
        model.connection_state(&profile_id),
        dbotter::ui::ConnectionState::Connected {
            session_generation: SessionGeneration(7),
            ..
        }
    ));

    model.pending_execute = Some((OperationId(44), profile_id.clone(), generation));
    model.fold(UiEvent::OperationFailed {
        operation_id: OperationId(44),
        profile_id: profile_id.clone(),
        profile_generation: generation,
        session_generation: Some(SessionGeneration(7)),
        kind: dbotter::model::OperationKind::ExecuteRead,
        summary: dbotter::model::PublicSummary::ConstraintRejected,
        session_disposition: Some(SessionDisposition::Keep),
        connection_outcome: ConnectionFailureOutcome::Preserve,
    });
    assert!(matches!(
        model.connection_state(&profile_id),
        dbotter::ui::ConnectionState::Connected { .. }
    ));

    for (operation_id, summary, connection_outcome) in [
        (
            OperationId(45),
            dbotter::model::PublicSummary::NetworkUnavailable,
            ConnectionFailureOutcome::Disconnected,
        ),
        (
            OperationId(46),
            dbotter::model::PublicSummary::OperationCancelled,
            ConnectionFailureOutcome::Unknown,
        ),
        (
            OperationId(47),
            dbotter::model::PublicSummary::OperationTimedOut,
            ConnectionFailureOutcome::Unknown,
        ),
    ] {
        model.connection_states.insert(
            profile_id.clone(),
            dbotter::ui::ConnectionState::Connected {
                session_generation: SessionGeneration(7),
                elapsed_ms: 1,
            },
        );
        model.pending_execute = Some((operation_id, profile_id.clone(), generation));
        model.fold(UiEvent::OperationFailed {
            operation_id,
            profile_id: profile_id.clone(),
            profile_generation: generation,
            session_generation: Some(SessionGeneration(7)),
            kind: dbotter::model::OperationKind::ExecuteRead,
            summary,
            session_disposition: Some(SessionDisposition::Evict),
            connection_outcome,
        });
        assert!(matches!(
            model.connection_state(&profile_id),
            dbotter::ui::ConnectionState::Disconnected
        ));
    }

    let credential_operation = OperationId(48);
    model.connection_states.insert(
        profile_id.clone(),
        dbotter::ui::ConnectionState::Pending(credential_operation),
    );
    model.fold(UiEvent::OperationFailed {
        operation_id: credential_operation,
        profile_id: profile_id.clone(),
        profile_generation: generation,
        session_generation: None,
        kind: dbotter::model::OperationKind::ConnectProfile,
        summary: dbotter::model::PublicSummary::CredentialRequired,
        session_disposition: None,
        connection_outcome: ConnectionFailureOutcome::NeedsCredential,
    });
    assert!(matches!(
        model.connection_state(&profile_id),
        dbotter::ui::ConnectionState::NeedsCredential
    ));

    model.fold(UiEvent::RuntimeShutdown {
        operation_id: OperationId(49),
    });
    assert!(matches!(
        model.connection_state(&profile_id),
        dbotter::ui::ConnectionState::Closing
    ));
    assert!(matches!(
        model.connection_state(&other_id),
        dbotter::ui::ConnectionState::Closing
    ));
}

#[test]
fn connect_auth_tls_network_and_timeout_failures_remain_visibly_failed() {
    let profile_id = ProfileId("visible-connect-failure".to_owned());
    let generation = ProfileGeneration(1);
    let mut model = UiModel::default();
    model.fold(UiEvent::ProfilesLoaded {
        operation_id: OperationId(60),
        profiles: vec![snapshot(profile_id.clone(), generation)],
    });

    let cases = [
        (
            dbotter::model::PublicSummary::AuthenticationFailed,
            ConnectionFailureOutcome::Disconnected,
        ),
        (
            dbotter::model::PublicSummary::TlsVerificationFailed,
            ConnectionFailureOutcome::Disconnected,
        ),
        (
            dbotter::model::PublicSummary::NetworkUnavailable,
            ConnectionFailureOutcome::Disconnected,
        ),
        (
            dbotter::model::PublicSummary::OperationTimedOut,
            ConnectionFailureOutcome::Unknown,
        ),
    ];
    let mut actual = Vec::new();
    let mut expected = Vec::new();
    for (offset, (summary, connection_outcome)) in cases.into_iter().enumerate() {
        let operation_id = OperationId(61 + offset as u64);
        model.connection_states.insert(
            profile_id.clone(),
            dbotter::ui::ConnectionState::Pending(operation_id),
        );
        model.fold(UiEvent::OperationFailed {
            operation_id,
            profile_id: profile_id.clone(),
            profile_generation: generation,
            session_generation: Some(SessionGeneration(70 + offset as u64)),
            kind: dbotter::model::OperationKind::ConnectProfile,
            summary,
            session_disposition: Some(SessionDisposition::Evict),
            connection_outcome,
        });
        actual.push(model.connection_state(&profile_id).clone());
        expected.push(dbotter::ui::ConnectionState::Failed { summary });
    }

    let credential_operation = OperationId(65);
    model.connection_states.insert(
        profile_id.clone(),
        dbotter::ui::ConnectionState::Pending(credential_operation),
    );
    model.fold(UiEvent::OperationFailed {
        operation_id: credential_operation,
        profile_id: profile_id.clone(),
        profile_generation: generation,
        session_generation: None,
        kind: dbotter::model::OperationKind::ConnectProfile,
        summary: dbotter::model::PublicSummary::CredentialRequired,
        session_disposition: None,
        connection_outcome: ConnectionFailureOutcome::NeedsCredential,
    });
    actual.push(model.connection_state(&profile_id).clone());
    expected.push(dbotter::ui::ConnectionState::NeedsCredential);

    assert_eq!(actual, expected);
}

#[test]
fn changed_removed_and_stale_save_fences_clear_only_the_exact_profile_workspace() {
    let target = ProfileId("workspace-target".to_owned());
    let survivor = ProfileId("workspace-survivor".to_owned());
    let mut model = UiModel::default();
    model.fold(UiEvent::ProfilesLoaded {
        operation_id: OperationId(50),
        profiles: vec![
            snapshot(target.clone(), ProfileGeneration(1)),
            snapshot(survivor.clone(), ProfileGeneration(2)),
        ],
    });
    model.selected_profile = Some(target.clone());
    model.editor_text = "target-only statement".to_owned();
    model.pending_execute = Some((OperationId(51), target.clone(), ProfileGeneration(1)));
    model.result = Some(empty_query_result());
    model.fold(UiEvent::ProfilesLoaded {
        operation_id: OperationId(52),
        profiles: vec![
            snapshot(target.clone(), ProfileGeneration(3)),
            snapshot(survivor.clone(), ProfileGeneration(2)),
        ],
    });
    assert!(model.editor_text.is_empty());
    assert!(model.pending_execute.is_none());
    assert!(model.result.is_none());

    model.editor_text = "deleted target statement".to_owned();
    model.result = Some(empty_query_result());
    model.selected_profile = Some(target.clone());
    model.fold(UiEvent::ProfileDeleted {
        operation_id: OperationId(53),
        profile_id: target.clone(),
        profile_generation: ProfileGeneration(4),
        server_state_unknown: false,
    });
    assert_eq!(model.selected_profile, Some(survivor.clone()));
    assert!(model.editor_text.is_empty());
    assert!(model.result.is_none());

    model.fold(UiEvent::ProfilesLoaded {
        operation_id: OperationId(54),
        profiles: vec![
            snapshot(target.clone(), ProfileGeneration(5)),
            snapshot(survivor, ProfileGeneration(2)),
        ],
    });
    model.connection_states.insert(
        target.clone(),
        dbotter::ui::ConnectionState::Connected {
            session_generation: SessionGeneration(9),
            elapsed_ms: 1,
        },
    );
    model.editor_text = "new generation workspace".to_owned();
    model.result = Some(empty_query_result());
    model.fold(UiEvent::ProfileSaved {
        operation_id: OperationId(55),
        profile_id: target.clone(),
        previous_generation: Some(ProfileGeneration(1)),
        profile_generation: ProfileGeneration(3),
        session_retained: false,
        warning: None,
    });
    assert_eq!(model.active_generation(&target), Some(ProfileGeneration(5)));
    assert_eq!(model.editor_text, "new generation workspace");
    assert!(model.result.is_some());
    assert!(matches!(
        model.connection_state(&target),
        dbotter::ui::ConnectionState::Connected { .. }
    ));
}

#[tokio::test]
async fn controller_enforces_one_per_profile_and_four_global_without_spawning_busy_work() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    let gates = (0..4)
        .map(|_| Arc::new(AsyncGate::new()))
        .collect::<Vec<_>>();
    let connector = Arc::new(ScriptedConnector::new(
        gates
            .iter()
            .cloned()
            .map(SessionBehavior::Blocked)
            .collect(),
    ));
    let service = test_service_with_connector(&path, connector.clone());
    let profiles = seed_profiles(&service, 5).await;
    let (mut ui, service_port) = controller_ports();
    let runtime = spawn_with_service(service_port, service);

    submit_test(&ui, &profiles[0], OperationId(100), 5_000).expect("first work");
    gates[0].wait_until_entered().await;
    submit_test(&ui, &profiles[0], OperationId(101), 5_000).expect("duplicate queues");
    let duplicate = wait_for_event(&mut ui, |event| {
        matches!(
            event,
            UiEvent::OperationFailed {
                operation_id: OperationId(101),
                summary: dbotter::model::PublicSummary::ResourceBusy,
                ..
            }
        )
    })
    .await;
    assert!(matches!(duplicate, UiEvent::OperationFailed { .. }));

    for (index, gate) in gates.iter().enumerate().skip(1) {
        submit_test(
            &ui,
            &profiles[index],
            OperationId(101 + index as u64),
            5_000,
        )
        .expect("allowed global work");
        gate.wait_until_entered().await;
    }
    submit_test(&ui, &profiles[4], OperationId(110), 5_000).expect("fifth queues");
    wait_for_event(&mut ui, |event| {
        matches!(
            event,
            UiEvent::OperationFailed {
                operation_id: OperationId(110),
                summary: dbotter::model::PublicSummary::ResourceBusy,
                ..
            }
        )
    })
    .await;
    assert_eq!(connector.connects.load(Ordering::SeqCst), 4);

    shutdown(&ui, runtime, OperationId(199)).await;
}

#[tokio::test]
async fn p3_invalid_execute_targets_fail_before_session_acquisition() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    let connector = Arc::new(CountingConnector::default());
    let service = test_service(&path, connector.clone());
    let mysql = service
        .create_profile(create_request("mysql-execute-disabled", OperationId(180)))
        .await
        .expect("mysql profile");
    let mut redis_request = create_request("redis-execute-disabled", OperationId(181));
    redis_request.draft = ConnectionDraft::for_driver(DriverKind::Redis);
    redis_request.draft.name = "redis-execute-disabled".to_owned();
    redis_request.draft.credential_mode = CredentialMode::None;
    let redis = service
        .create_profile(redis_request)
        .await
        .expect("redis profile");
    let (mut ui, service_port) = controller_ports();
    let runtime = spawn_with_service(service_port, service);
    for (operation_id, profile_id, generation, language, text) in [
        (
            OperationId(182),
            mysql.profile_id,
            mysql.profile_generation,
            dbotter::model::QueryLanguage::Sql,
            "SELECT 1; SELECT 2;",
        ),
        (
            OperationId(183),
            redis.profile_id.clone(),
            redis.profile_generation,
            dbotter::model::QueryLanguage::RedisCommand,
            "SUBSCRIBE sentinel",
        ),
        (
            OperationId(184),
            redis.profile_id,
            redis.profile_generation,
            dbotter::model::QueryLanguage::RedisCommand,
            "XREAD BLOCK 0 STREAMS sentinel 0",
        ),
    ] {
        ui.try_submit(UiCommand::Execute {
            operation_id,
            profile_id,
            profile_generation: generation,
            language,
            text: text.to_owned(),
            row_limit: 100,
            timeout_ms: 1_000,
        })
        .expect("invalid execute reaches controller");
        wait_for_event(&mut ui, |event| {
            matches!(
                event,
                UiEvent::OperationFailed {
                    operation_id: actual,
                    summary: dbotter::model::PublicSummary::InvalidInput,
                    ..
                } if *actual == operation_id
            )
        })
        .await;
    }
    assert_eq!(connector.connects.load(Ordering::SeqCst), 0);
    shutdown(&ui, runtime, OperationId(189)).await;
}

#[tokio::test]
async fn p3_valid_execute_reaches_typed_prepared_resource_and_retains_exact_provenance() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    let connector = Arc::new(CountingConnector::default());
    let service = test_service(&path, connector.clone());
    let profile = service
        .create_profile(create_request("mysql-p3-execute", OperationId(190)))
        .await
        .expect("mysql profile");
    let (mut ui, service_port) = controller_ports();
    let runtime = spawn_with_service(service_port, service);

    ui.try_submit(UiCommand::Execute {
        operation_id: OperationId(191),
        profile_id: profile.profile_id.clone(),
        profile_generation: profile.profile_generation,
        language: dbotter::model::QueryLanguage::Sql,
        text: "SELECT 1".to_owned(),
        row_limit: 500,
        timeout_ms: 1_000,
    })
    .expect("execute submits");
    let finished = wait_for_event(&mut ui, |event| {
        matches!(
            event,
            UiEvent::QueryFinished {
                operation_id: OperationId(191),
                profile_id,
                profile_generation,
                ..
            } if profile_id == &profile.profile_id
                && *profile_generation == profile.profile_generation
        )
    })
    .await;
    let UiEvent::QueryFinished { result, .. } = finished else {
        panic!("expected query result");
    };
    assert_eq!(result.provenance.operation_id, OperationId(191));
    assert_eq!(result.provenance.profile_id, profile.profile_id);
    assert_eq!(
        result.provenance.profile_generation,
        profile.profile_generation
    );
    assert_eq!(result.provenance.driver, DriverKind::MySql);
    assert_eq!(connector.connects.load(Ordering::SeqCst), 1);
    assert_eq!(connector.executes.load(Ordering::SeqCst), 1);
    shutdown(&ui, runtime, OperationId(199)).await;
}

#[tokio::test]
async fn p3_execute_cancel_drops_driver_future_before_exact_session_close() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    let session = Arc::new(ExecuteTestSession::new(ExecuteResourceBehavior::Blocked));
    let service = test_service_with_connector(
        &path,
        Arc::new(ExecuteTestConnector {
            session: session.clone(),
        }),
    );
    let profile = service
        .create_profile(create_request("mysql-p3-cancel", OperationId(210)))
        .await
        .expect("mysql profile");
    let (mut ui, service_port) = controller_ports();
    let runtime = spawn_with_service(service_port, service);

    ui.try_submit(UiCommand::Execute {
        operation_id: OperationId(211),
        profile_id: profile.profile_id,
        profile_generation: profile.profile_generation,
        language: dbotter::model::QueryLanguage::Sql,
        text: "SELECT 1".to_owned(),
        row_limit: 500,
        timeout_ms: 5_000,
    })
    .expect("execute submits");
    session.resources.wait_until_started().await;
    ui.try_submit(UiCommand::CancelOperation {
        operation_id: OperationId(211),
    })
    .expect("cancel submits");
    wait_for_event(&mut ui, |event| terminal_is(event, OperationId(211), true)).await;

    assert!(session.resources.execute_dropped.load(Ordering::SeqCst));
    assert!(
        !session.close_before_execute_drop.load(Ordering::SeqCst),
        "the driver future must release its connection before pool/session close"
    );
    assert_eq!(session.closes.load(Ordering::SeqCst), 1);
    shutdown(&ui, runtime, OperationId(219)).await;
}

#[tokio::test]
async fn p3_driver_session_disposition_is_identical_in_cache_event_and_ui_outcome() {
    for (index, session_healthy) in [true, false].into_iter().enumerate() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("config.toml");
        let session = Arc::new(ExecuteTestSession::new(
            ExecuteResourceBehavior::PreparedUnsupported { session_healthy },
        ));
        let service = test_service_with_connector(
            &path,
            Arc::new(ExecuteTestConnector {
                session: session.clone(),
            }),
        );
        let create_operation = OperationId(220 + index as u64 * 10);
        let execute_operation = OperationId(create_operation.0 + 1);
        let profile = service
            .create_profile(create_request(
                &format!("mysql-p3-disposition-{index}"),
                create_operation,
            ))
            .await
            .expect("mysql profile");
        let profile_id = profile.profile_id.clone();
        let (mut ui, service_port) = controller_ports();
        let runtime = spawn_with_service(service_port, service.clone());

        ui.try_submit(UiCommand::Execute {
            operation_id: execute_operation,
            profile_id,
            profile_generation: profile.profile_generation,
            language: dbotter::model::QueryLanguage::Sql,
            text: "USE dbotter".to_owned(),
            row_limit: 500,
            timeout_ms: 1_000,
        })
        .expect("execute submits");
        let terminal = wait_for_event(&mut ui, |event| {
            matches!(
                event,
                UiEvent::OperationFailed {
                    operation_id,
                    summary: dbotter::model::PublicSummary::UnsupportedFeature,
                    ..
                } if *operation_id == execute_operation
            )
        })
        .await;
        let expected_disposition = if session_healthy {
            SessionDisposition::Keep
        } else {
            SessionDisposition::Evict
        };
        let expected_outcome = if session_healthy {
            ConnectionFailureOutcome::Preserve
        } else {
            ConnectionFailureOutcome::Disconnected
        };
        let UiEvent::OperationFailed {
            session_disposition,
            connection_outcome,
            ..
        } = terminal
        else {
            panic!("operation failure expected");
        };
        assert_eq!(session_disposition, Some(expected_disposition));
        assert_eq!(connection_outcome, expected_outcome);
        assert_eq!(
            service.cached_session_count().await,
            usize::from(session_healthy)
        );
        assert_eq!(
            session.closes.load(Ordering::SeqCst),
            usize::from(!session_healthy)
        );
        shutdown(
            &ui,
            runtime,
            OperationId(create_operation.0.saturating_add(9)),
        )
        .await;
    }
}

#[tokio::test]
async fn p3_typed_resource_commands_are_capability_gated_before_connector_invocation() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    let connector = Arc::new(CountingConnector::default());
    let service = test_service(&path, connector.clone());
    let mysql = service
        .create_profile(create_request("mysql-p3-resource", OperationId(200)))
        .await
        .expect("mysql profile");
    let mut redis_create = create_request("redis-p3-resource", OperationId(201));
    redis_create.draft = ConnectionDraft::for_driver(DriverKind::Redis);
    redis_create.draft.name = "redis-p3-resource".to_owned();
    redis_create.draft.credential_mode = CredentialMode::None;
    let redis = service
        .create_profile(redis_create)
        .await
        .expect("redis profile");
    let (mut ui, service_port) = controller_ports();
    let runtime = spawn_with_service(service_port, service);

    let catalog = CatalogRequest::Schemas {
        identity: RequestIdentity::new(
            mysql.profile_id.clone(),
            mysql.profile_generation,
            OperationId(202),
        ),
        prefix: None,
        page_token: None,
        page_size: 50,
        timeout: Duration::from_secs(5),
    };
    ui.try_submit(UiCommand::BrowseCatalog(catalog))
        .expect("catalog submits");
    wait_for_event(&mut ui, |event| {
        matches!(
            event,
            UiEvent::CatalogPageFailed {
                request,
                summary: dbotter::model::PublicSummary::UnsupportedFeature,
            } if request.operation_id() == OperationId(202)
        )
    })
    .await;

    let mismatched_catalog = CatalogRequest::Schemas {
        identity: RequestIdentity::new(
            redis.profile_id.clone(),
            redis.profile_generation,
            OperationId(205),
        ),
        prefix: None,
        page_token: None,
        page_size: 50,
        timeout: Duration::from_secs(5),
    };
    ui.try_submit(UiCommand::BrowseCatalog(mismatched_catalog))
        .expect("mismatched catalog submits");
    wait_for_event(&mut ui, |event| {
        matches!(
            event,
            UiEvent::CatalogPageFailed {
                request,
                summary: dbotter::model::PublicSummary::InvalidInput,
            } if request.operation_id() == OperationId(205)
        )
    })
    .await;

    let scan = RedisScanRequest {
        identity: RequestIdentity::new(
            redis.profile_id.clone(),
            redis.profile_generation,
            OperationId(203),
        ),
        filter: RedisKeyFilter::LiteralPrefix("receipt:".to_owned()),
        cursor: 0,
        count_hint: 100,
        timeout: Duration::from_secs(5),
    };
    ui.try_submit(UiCommand::ScanRedisKeys(scan))
        .expect("scan submits");
    wait_for_event(&mut ui, |event| {
        matches!(
            event,
            UiEvent::RedisKeysFailed {
                request,
                summary: dbotter::model::PublicSummary::UnsupportedFeature,
            } if request.operation_id() == OperationId(203)
        )
    })
    .await;

    let inspect = RedisKeyInspectRequest {
        identity: RequestIdentity::new(
            redis.profile_id,
            redis.profile_generation,
            OperationId(204),
        ),
        key: RedisKeyId(b"receipt:marker".to_vec()),
        timeout: Duration::from_secs(5),
    };
    ui.try_submit(UiCommand::InspectRedisKey(inspect))
        .expect("inspect submits");
    wait_for_event(&mut ui, |event| {
        matches!(
            event,
            UiEvent::RedisKeyInspectFailed {
                request,
                summary: dbotter::model::PublicSummary::UnsupportedFeature,
            } if request.operation_id() == OperationId(204)
        )
    })
    .await;

    assert_eq!(connector.connects.load(Ordering::SeqCst), 0);
    shutdown(&ui, runtime, OperationId(209)).await;
}

#[tokio::test]
async fn reconnect_is_busy_for_unrelated_global_saturation_but_takes_over_same_profile() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    let gates = (0..4)
        .map(|_| Arc::new(AsyncGate::new()))
        .collect::<Vec<_>>();
    let connector = Arc::new(ScriptedConnector::new(
        gates
            .iter()
            .cloned()
            .map(SessionBehavior::Blocked)
            .chain(std::iter::once(SessionBehavior::Immediate))
            .collect(),
    ));
    let service = test_service_with_connector(&path, connector.clone());
    let profiles = seed_profiles(&service, 5).await;
    let (mut ui, service_port) = controller_ports();
    let runtime = spawn_with_service(service_port, service);

    for (index, gate) in gates.iter().enumerate() {
        submit_test(
            &ui,
            &profiles[index],
            OperationId(120 + index as u64),
            5_000,
        )
        .expect("global permit holder queues");
        gate.wait_until_entered().await;
    }
    ui.try_submit(UiCommand::ReconnectProfile {
        operation_id: OperationId(124),
        profile_id: profiles[4].id.clone(),
        profile_generation: profiles[4].generation,
        timeout_ms: 5_000,
    })
    .expect("reconnect reaches admission control");

    tokio::time::timeout(
        Duration::from_millis(300),
        wait_for_event(&mut ui, |event| {
            matches!(
                event,
                UiEvent::OperationFailed {
                    operation_id: OperationId(124),
                    kind: dbotter::model::OperationKind::ReconnectProfile,
                    summary: dbotter::model::PublicSummary::ResourceBusy,
                    ..
                }
            )
        }),
    )
    .await
    .expect("saturated reconnect must fail before worker spawn");
    assert_eq!(
        connector.connects.load(Ordering::SeqCst),
        4,
        "busy reconnect must not reach connector acquisition"
    );

    ui.try_submit(UiCommand::ReconnectProfile {
        operation_id: OperationId(125),
        profile_id: profiles[0].id.clone(),
        profile_generation: profiles[0].generation,
        timeout_ms: 5_000,
    })
    .expect("same-profile reconnect reaches takeover control");
    let prior_terminal = wait_for_event(&mut ui, |event| {
        matches!(
            event,
            UiEvent::OperationFailed {
                operation_id: OperationId(120),
                summary: dbotter::model::PublicSummary::OperationCancelled,
                ..
            } | UiEvent::ConnectionReady {
                operation_id: OperationId(125),
                ..
            } | UiEvent::OperationFailed {
                operation_id: OperationId(125),
                ..
            }
        )
    })
    .await;
    assert!(matches!(
        prior_terminal,
        UiEvent::OperationFailed {
            operation_id: OperationId(120),
            summary: dbotter::model::PublicSummary::OperationCancelled,
            ..
        }
    ));
    wait_for_event(&mut ui, |event| {
        matches!(
            event,
            UiEvent::ConnectionReady {
                operation_id: OperationId(125),
                ..
            }
        )
    })
    .await;
    assert_eq!(connector.connects.load(Ordering::SeqCst), 5);
    shutdown(&ui, runtime, OperationId(129)).await;
}

#[tokio::test]
async fn saturated_work_lane_cannot_delay_control_cancellation() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    let gate = Arc::new(AsyncGate::new());
    let connector = Arc::new(ScriptedConnector::new(vec![SessionBehavior::Blocked(
        gate.clone(),
    )]));
    let service = test_service_with_connector(&path, connector);
    let profile = seed_profiles(&service, 1).await.remove(0);
    let (mut ui, service_port) = controller_ports();
    for offset in 0..WORK_CAPACITY {
        submit_test(&ui, &profile, OperationId(200 + offset as u64), 5_000)
            .expect("fill work lane");
    }
    assert_eq!(
        submit_test(&ui, &profile, OperationId(299), 5_000),
        Err(SubmitError::Busy)
    );
    let runtime = spawn_with_service(service_port, service);
    gate.wait_until_entered().await;
    ui.try_submit(UiCommand::CancelOperation {
        operation_id: OperationId(200),
    })
    .expect("control lane remains available");

    tokio::time::timeout(
        Duration::from_millis(300),
        wait_for_event(&mut ui, |event| {
            matches!(
                event,
                UiEvent::OperationFailed {
                    operation_id: OperationId(200),
                    summary: dbotter::model::PublicSummary::OperationCancelled,
                    ..
                }
            )
        }),
    )
    .await
    .expect("cancel is not delayed by work backlog");
    shutdown(&ui, runtime, OperationId(299)).await;
}

#[tokio::test]
async fn duplicate_operation_id_mutation_is_busy_without_spawn_and_drops_queued_secret() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    let gate = Arc::new(AsyncGate::new());
    let connector = Arc::new(ScriptedConnector::new(vec![SessionBehavior::Blocked(
        gate.clone(),
    )]));
    let service = test_service_with_connector(&path, connector);
    let profile = seed_profiles(&service, 1).await.remove(0);
    let (mut ui, service_port) = controller_ports();
    let runtime = spawn_with_service(service_port, service.clone());
    submit_test(&ui, &profile, OperationId(250), 5_000).expect("registered work");
    gate.wait_until_entered().await;

    let secret = Arc::new(SessionSecret::new("duplicate-operation-secret".to_owned()));
    let weak = Arc::downgrade(&secret);
    let mut draft = ConnectionDraft::for_driver(DriverKind::MySql);
    draft.name = "Must not spawn".to_owned();
    draft.credential_mode = CredentialMode::Session;
    ui.try_submit(UiCommand::CreateProfile(CreateProfileRequest {
        draft_id: DraftId(250),
        operation_id: OperationId(250),
        explicit_id: Some(ProfileId("must-not-spawn".to_owned())),
        draft,
        secret_update: SessionSecretUpdate::Replace(secret),
        migration_consent: MigrationConsent::Cancelled,
    }))
    .expect("duplicate reaches registry");
    wait_for_event(&mut ui, |event| {
        matches!(
            event,
            UiEvent::ProfileSaveFailed {
                operation_id: OperationId(250),
                summary: dbotter::model::PublicSummary::ResourceBusy,
                ..
            }
        )
    })
    .await;
    wait_until(Duration::from_secs(1), || weak.upgrade().is_none()).await;
    assert!(
        service
            .profiles_snapshot()
            .await
            .iter()
            .all(|profile| profile.id != "must-not-spawn")
    );
    shutdown(&ui, runtime, OperationId(259)).await;
}

#[tokio::test]
async fn ready_mutation_lane_is_polled_before_ready_work_lane() {
    let (ui, mut service_port) = controller_ports();
    ui.try_submit(UiCommand::TestConnection {
        operation_id: OperationId(280),
        profile_id: ProfileId("priority".to_owned()),
        profile_generation: ProfileGeneration(1),
        timeout_ms: 1_000,
    })
    .expect("ready work");
    ui.try_submit(UiCommand::RefreshProfiles {
        operation_id: OperationId(281),
    })
    .expect("ready mutation");
    let first = service_port
        .next_command()
        .await
        .expect("first prioritized command");
    assert!(matches!(
        first,
        UiCommand::RefreshProfiles {
            operation_id: OperationId(281),
        }
    ));
}

#[tokio::test]
async fn full_event_lane_does_not_strand_registry_or_profile_permit() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    let gate = Arc::new(AsyncGate::new());
    let connector = Arc::new(ScriptedConnector::new(vec![
        SessionBehavior::Blocked(gate.clone()),
        SessionBehavior::Immediate,
    ]));
    let service = test_service_with_connector(&path, connector.clone());
    let profile = seed_profiles(&service, 1).await.remove(0);
    let (ui, service_port) = controller_ports();
    let runtime = spawn_with_service(service_port, service);
    submit_test(&ui, &profile, OperationId(300), 5_000).expect("blocking work");
    gate.wait_until_entered().await;

    for offset in 0..(EVENT_CAPACITY + 8) {
        loop {
            match submit_test(&ui, &profile, OperationId(301 + offset as u64), 5_000) {
                Ok(()) => break,
                Err(SubmitError::Busy) => tokio::task::yield_now().await,
                Err(SubmitError::Disconnected) => panic!("runtime disconnected"),
            }
        }
    }
    ui.try_submit(UiCommand::CancelOperation {
        operation_id: OperationId(300),
    })
    .expect("cancel through full event lane");
    wait_until(Duration::from_secs(1), || {
        connector.closes.load(Ordering::SeqCst) >= 1
    })
    .await;

    loop {
        match submit_test(&ui, &profile, OperationId(499), 1_000) {
            Ok(()) => break,
            Err(SubmitError::Busy) => tokio::task::yield_now().await,
            Err(SubmitError::Disconnected) => panic!("runtime disconnected"),
        }
    }
    wait_until(Duration::from_secs(1), || {
        connector.connects.load(Ordering::SeqCst) >= 2
    })
    .await;
    assert_eq!(connector.connects.load(Ordering::SeqCst), 2);
    shutdown(&ui, runtime, OperationId(500)).await;
}

#[tokio::test]
async fn cancel_and_timeout_are_distinct_exactly_once_and_cleanup_before_replacement() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    let cancel_gate = Arc::new(AsyncGate::new());
    let timeout_gate = Arc::new(AsyncGate::new());
    let connector = Arc::new(ScriptedConnector::new(vec![
        SessionBehavior::Blocked(cancel_gate.clone()),
        SessionBehavior::Blocked(timeout_gate.clone()),
        SessionBehavior::Immediate,
    ]));
    let service = test_service_with_connector(&path, connector);
    let profile = seed_profiles(&service, 1).await.remove(0);
    let (mut ui, service_port) = controller_ports();
    let runtime = spawn_with_service(service_port, service.clone());

    submit_test(&ui, &profile, OperationId(600), 5_000).expect("cancel target");
    cancel_gate.wait_until_entered().await;
    ui.try_submit(UiCommand::CancelOperation {
        operation_id: OperationId(600),
    })
    .expect("cancel");
    wait_for_event(&mut ui, |event| terminal_is(event, OperationId(600), true)).await;

    submit_test(&ui, &profile, OperationId(601), 20).expect("timeout target");
    timeout_gate.wait_until_entered().await;
    wait_for_event(&mut ui, |event| terminal_is(event, OperationId(601), false)).await;

    submit_test(&ui, &profile, OperationId(602), 1_000).expect("replacement");
    wait_for_event(&mut ui, |event| {
        matches!(
            event,
            UiEvent::ConnectionReady {
                operation_id: OperationId(602),
                ..
            }
        )
    })
    .await;
    assert!(service.cached_session_identity(&profile.id).await.is_some());
    tokio::time::sleep(Duration::from_millis(40)).await;
    let duplicate_terminals = ui
        .drain_events(EVENT_CAPACITY)
        .into_iter()
        .filter(|event| {
            terminal_is(event, OperationId(600), true)
                || terminal_is(event, OperationId(601), false)
        })
        .count();
    assert_eq!(duplicate_terminals, 0);
    shutdown(&ui, runtime, OperationId(699)).await;
}

#[tokio::test]
async fn reload_exact_diff_preserves_unchanged_and_uncertain_fences_everything() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    let connector = Arc::new(ScriptedConnector::new(vec![
        SessionBehavior::Immediate,
        SessionBehavior::Immediate,
    ]));
    let service = test_service_with_connector(&path, connector);
    let profiles = seed_profiles(&service, 2).await;
    service
        .check_at(
            OperationId(700),
            profiles[0].id.clone(),
            profiles[0].generation,
            Duration::from_secs(1),
        )
        .await
        .expect("cache changed profile");
    service
        .check_at(
            OperationId(701),
            profiles[1].id.clone(),
            profiles[1].generation,
            Duration::from_secs(1),
        )
        .await
        .expect("cache unchanged profile");
    let unchanged_identity = service
        .cached_session_identity(&profiles[1].id)
        .await
        .expect("unchanged identity");

    let external = ApplicationService::load_path(&path).expect("external service");
    let mut changed = profiles[0].persisted.as_draft();
    changed.host = "changed.internal".to_owned();
    external
        .update_profile(UpdateProfileRequest {
            profile_id: profiles[0].id.clone(),
            expected_generation: external
                .profile_generation(&profiles[0].id)
                .await
                .expect("external generation"),
            operation_id: OperationId(702),
            draft: changed,
            secret_update: SessionSecretUpdate::Clear,
            migration_consent: MigrationConsent::Cancelled,
        })
        .await
        .expect("external update");

    let (mut ui, service_port) = controller_ports();
    let runtime = spawn_with_service(service_port, service.clone());
    ui.try_submit(UiCommand::RefreshProfiles {
        operation_id: OperationId(703),
    })
    .expect("reload");
    wait_for_event(&mut ui, |event| {
        matches!(
            event,
            UiEvent::ProfilesLoaded {
                operation_id: OperationId(703),
                ..
            }
        )
    })
    .await;
    assert!(
        service
            .cached_session_identity(&profiles[0].id)
            .await
            .is_none()
    );
    assert_eq!(
        service.cached_session_identity(&profiles[1].id).await,
        Some(unchanged_identity)
    );

    std::fs::write(&path, b"not = [valid").expect("corrupt config");
    ui.try_submit(UiCommand::RefreshProfiles {
        operation_id: OperationId(704),
    })
    .expect("uncertain reload");
    wait_for_event(&mut ui, |event| {
        matches!(
            event,
            UiEvent::ConfigUncertain {
                operation_id: OperationId(704)
            }
        )
    })
    .await;
    assert!(service.is_config_uncertain());
    assert_eq!(service.cached_session_count().await, 0);
    shutdown(&ui, runtime, OperationId(799)).await;
}

#[tokio::test]
async fn task_panic_maps_to_one_internal_failure_after_cache_and_permit_cleanup() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    let connector = Arc::new(ScriptedConnector::new(vec![
        SessionBehavior::Panic,
        SessionBehavior::Immediate,
    ]));
    let service = test_service_with_connector(&path, connector);
    let profile = seed_profiles(&service, 1).await.remove(0);
    let (mut ui, service_port) = controller_ports();
    let runtime = spawn_with_service(service_port, service.clone());
    submit_test(&ui, &profile, OperationId(800), 1_000).expect("panic work");
    wait_for_event(&mut ui, |event| {
        matches!(
            event,
            UiEvent::OperationFailed {
                operation_id: OperationId(800),
                summary: dbotter::model::PublicSummary::InternalFailure,
                ..
            }
        )
    })
    .await;
    assert!(service.cached_session_identity(&profile.id).await.is_none());

    submit_test(&ui, &profile, OperationId(801), 1_000).expect("permit released");
    wait_for_event(&mut ui, |event| {
        matches!(
            event,
            UiEvent::ConnectionReady {
                operation_id: OperationId(801),
                ..
            }
        )
    })
    .await;
    shutdown(&ui, runtime, OperationId(899)).await;
}

#[tokio::test]
async fn edit_and_delete_cancel_active_work_only_after_observed_commit() {
    for delete in [false, true] {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("config.toml");
        let gate = Arc::new(AsyncGate::new());
        let connector = Arc::new(ScriptedConnector::new(vec![SessionBehavior::Blocked(
            gate.clone(),
        )]));
        let failpoint = Arc::new(ArmedPreRenameFailure::default());
        let service = ApplicationService::with_dependencies(
            &path,
            connector.clone(),
            Arc::new(MissingEnvironment),
            Arc::new(SessionSecretStore::default()),
            ConfigWriter::with_fault_injector(failpoint.clone()),
        )
        .expect("service");
        let profile = seed_profiles(&service, 1).await.remove(0);
        let (mut ui, service_port) = controller_ports();
        let runtime = spawn_with_service(service_port, service.clone());
        submit_test(&ui, &profile, OperationId(850), 5_000).expect("active work");
        gate.wait_until_entered().await;

        failpoint.arm();
        if delete {
            ui.try_submit(UiCommand::DeleteProfile(DeleteProfileRequest {
                profile_id: profile.id.clone(),
                expected_generation: profile.generation,
                operation_id: OperationId(851),
                migration_consent: MigrationConsent::Cancelled,
            }))
            .expect("failed delete queues");
        } else {
            let mut draft = profile.persisted.as_draft();
            draft.name = "Renamed after commit".to_owned();
            ui.try_submit(UiCommand::UpdateProfile(UpdateProfileRequest {
                profile_id: profile.id.clone(),
                expected_generation: profile.generation,
                operation_id: OperationId(851),
                draft,
                secret_update: SessionSecretUpdate::Clear,
                migration_consent: MigrationConsent::Cancelled,
            }))
            .expect("failed edit queues");
        }
        wait_for_event(&mut ui, |event| {
            matches!(
                event,
                UiEvent::ProfileSaveFailed {
                    operation_id: OperationId(851),
                    ..
                } | UiEvent::OperationFailed {
                    operation_id: OperationId(851),
                    ..
                }
            )
        })
        .await;
        assert_eq!(connector.closes.load(Ordering::SeqCst), 0);
        assert_eq!(
            service
                .profile_generation(&profile.id)
                .await
                .expect("profile generation remains readable"),
            profile.generation
        );

        if delete {
            ui.try_submit(UiCommand::DeleteProfile(DeleteProfileRequest {
                profile_id: profile.id.clone(),
                expected_generation: profile.generation,
                operation_id: OperationId(852),
                migration_consent: MigrationConsent::Cancelled,
            }))
            .expect("committed delete queues");
            wait_for_event(&mut ui, |event| {
                matches!(
                    event,
                    UiEvent::ProfileDeleted {
                        operation_id: OperationId(852),
                        ..
                    }
                )
            })
            .await;
        } else {
            let mut draft = profile.persisted.as_draft();
            draft.name = "Renamed after commit".to_owned();
            ui.try_submit(UiCommand::UpdateProfile(UpdateProfileRequest {
                profile_id: profile.id.clone(),
                expected_generation: profile.generation,
                operation_id: OperationId(852),
                draft,
                secret_update: SessionSecretUpdate::Clear,
                migration_consent: MigrationConsent::Cancelled,
            }))
            .expect("committed edit queues");
            wait_for_event(&mut ui, |event| {
                matches!(
                    event,
                    UiEvent::ProfileSaved {
                        operation_id: OperationId(852),
                        ..
                    }
                )
            })
            .await;
        }
        wait_until(Duration::from_secs(1), || {
            connector.closes.load(Ordering::SeqCst) >= 1
        })
        .await;
        shutdown(&ui, runtime, OperationId(859)).await;
    }
}

#[tokio::test]
async fn runtime_uncertain_mutation_joins_active_work_and_clears_secrets_before_terminal() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    let session = Arc::new(OrderingSession::default());
    let connector = Arc::new(OrderingConnector {
        session: session.clone(),
    });
    let store = Arc::new(SessionSecretStore::default());
    let fault = Arc::new(ArmedObservationFailure::default());
    let service = ApplicationService::with_dependencies(
        &path,
        connector,
        Arc::new(MissingEnvironment),
        store.clone(),
        ConfigWriter::with_fault_injector(fault.clone()),
    )
    .expect("service");
    let profile = seed_profiles(&service, 1).await.remove(0);
    let orphan = Arc::new(SessionSecret::new("clear-on-uncertain".to_owned()));
    let weak = Arc::downgrade(&orphan);
    store
        .apply(
            &ProfileId("orphan".to_owned()),
            SessionSecretUpdate::Replace(orphan),
        )
        .expect("seed orphan secret");
    let (mut ui, service_port) = controller_ports();
    let runtime = spawn_with_service(service_port, service.clone());
    submit_test(&ui, &profile, OperationId(1_400), 5_000).expect("active work");
    session.wait_until_ping_started().await;
    fault.arm();
    let mut changed = profile.persisted.as_draft();
    changed.name = "Committed but unobserved".to_owned();
    ui.try_submit(UiCommand::UpdateProfile(UpdateProfileRequest {
        profile_id: profile.id.clone(),
        expected_generation: profile.generation,
        operation_id: OperationId(1_401),
        draft: changed,
        secret_update: SessionSecretUpdate::Clear,
        migration_consent: MigrationConsent::Cancelled,
    }))
    .expect("uncertain mutation");

    wait_for_event(&mut ui, |event| {
        matches!(
            event,
            UiEvent::ConfigUncertain {
                operation_id: OperationId(1_401)
            }
        )
    })
    .await;
    assert!(session.ping_dropped.load(Ordering::SeqCst));
    assert!(
        !session.close_before_ping_drop.load(Ordering::SeqCst),
        "runtime failure cleanup must join active ping before exact close"
    );
    assert!(
        weak.upgrade().is_none(),
        "runtime failure cleanup must clear all secrets before terminal"
    );
    shutdown(&ui, runtime, OperationId(1_409)).await;
}

#[tokio::test]
async fn reconnect_allocates_a_new_session_generation_and_disconnect_is_profile_isolated() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    let other_gate = Arc::new(AsyncGate::new());
    let connector = Arc::new(ScriptedConnector::new(vec![
        SessionBehavior::Immediate,
        SessionBehavior::Blocked(other_gate.clone()),
        SessionBehavior::Immediate,
    ]));
    let service = test_service_with_connector(&path, connector);
    let profiles = seed_profiles(&service, 2).await;
    let (mut ui, service_port) = controller_ports();
    let runtime = spawn_with_service(service_port, service);

    submit_test(&ui, &profiles[0], OperationId(860), 1_000).expect("first connect");
    let first = wait_for_event(&mut ui, |event| {
        matches!(
            event,
            UiEvent::ConnectionReady {
                operation_id: OperationId(860),
                ..
            }
        )
    })
    .await;
    let UiEvent::ConnectionReady {
        session_generation: first_session,
        ..
    } = first
    else {
        unreachable!("matched connection event")
    };

    submit_test(&ui, &profiles[1], OperationId(861), 5_000).expect("other active");
    other_gate.wait_until_entered().await;
    ui.try_submit(UiCommand::ReconnectProfile {
        operation_id: OperationId(862),
        profile_id: profiles[0].id.clone(),
        profile_generation: profiles[0].generation,
        timeout_ms: 1_000,
    })
    .expect("reconnect control");
    let reconnected = wait_for_event(&mut ui, |event| {
        matches!(
            event,
            UiEvent::ConnectionReady {
                operation_id: OperationId(862),
                ..
            }
        )
    })
    .await;
    assert!(matches!(
        reconnected,
        UiEvent::ConnectionReady {
            session_generation,
            ..
        } if session_generation.0 > first_session.0
    ));

    submit_test(&ui, &profiles[1], OperationId(863), 5_000).expect("other duplicate queues");
    wait_for_event(&mut ui, |event| {
        matches!(
            event,
            UiEvent::OperationFailed {
                operation_id: OperationId(863),
                summary: dbotter::model::PublicSummary::ResourceBusy,
                ..
            }
        )
    })
    .await;
    shutdown(&ui, runtime, OperationId(869)).await;
}

#[tokio::test]
async fn shutdown_watch_drains_queued_secret_arcs_before_runtime_completion() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    let service = test_service(&path, Arc::new(CountingConnector::default()));
    let (ui, service_port) = controller_ports();
    let mut weak = Vec::new();
    for index in 0..MUTATION_CAPACITY {
        let secret = Arc::new(SessionSecret::new(format!("queued-secret-{index}")));
        weak.push(Arc::downgrade(&secret));
        let mut draft = ConnectionDraft::for_driver(DriverKind::MySql);
        draft.name = format!("Queued {index}");
        draft.credential_mode = CredentialMode::Session;
        ui.try_submit(UiCommand::CreateProfile(CreateProfileRequest {
            draft_id: DraftId(900 + index as u64),
            operation_id: OperationId(900 + index as u64),
            explicit_id: Some(ProfileId(format!("queued-{index}"))),
            draft,
            secret_update: SessionSecretUpdate::Replace(secret),
            migration_consent: MigrationConsent::Cancelled,
        }))
        .expect("fill mutation lane");
    }
    ui.try_submit(UiCommand::ShutdownRuntime {
        operation_id: OperationId(999),
    })
    .expect("watch shutdown bypasses queues");
    let runtime = spawn_with_service(service_port, service);
    runtime.wait().await.expect("runtime shutdown joins");
    assert!(weak.into_iter().all(|secret| secret.upgrade().is_none()));
}

#[tokio::test]
async fn shutdown_joins_and_emits_the_classified_in_flight_mutation_before_runtime_shutdown() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    let barrier = Arc::new(BlockingPreRename::default());
    let service = ApplicationService::with_dependencies(
        &path,
        Arc::new(CountingConnector::default()),
        Arc::new(MissingEnvironment),
        Arc::new(SessionSecretStore::default()),
        ConfigWriter::with_fault_injector(barrier.clone()),
    )
    .expect("service");
    let (mut ui, service_port) = controller_ports();
    let runtime = spawn_with_service(service_port, service.clone());
    ui.try_submit(UiCommand::CreateProfile(create_request(
        "shutdown-commit",
        OperationId(980),
    )))
    .expect("mutation");
    barrier.wait_until_entered().await;
    ui.try_submit(UiCommand::ShutdownRuntime {
        operation_id: OperationId(981),
    })
    .expect("shutdown");
    let wait = tokio::spawn(runtime.wait());
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(
        !wait.is_finished(),
        "mutation must be joined, never detached"
    );
    barrier.release();
    wait.await
        .expect("wait task joins")
        .expect("controller joins");

    let events = ui.drain_events(EVENT_CAPACITY);
    let saved_position = events.iter().position(|event| {
        matches!(
            event,
            UiEvent::ProfileSaved {
                operation_id: OperationId(980),
                ..
            }
        )
    });
    let shutdown_position = events.iter().position(|event| {
        matches!(
            event,
            UiEvent::RuntimeShutdown {
                operation_id: OperationId(981),
            }
        )
    });
    assert!(matches!(
        (saved_position, shutdown_position),
        (Some(saved), Some(shutdown)) if saved < shutdown
    ));
    assert!(
        service
            .profiles_snapshot()
            .await
            .iter()
            .any(|profile| profile.id == "shutdown-commit")
    );
}

#[tokio::test]
async fn draft_work_is_same_draft_and_global_bounded_and_cancel_releases_its_slot() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    let gates = (0..4)
        .map(|_| Arc::new(AsyncGate::new()))
        .collect::<Vec<_>>();
    let connector = Arc::new(ScriptedConnector::new(
        gates
            .iter()
            .cloned()
            .map(SessionBehavior::Blocked)
            .collect(),
    ));
    let service = test_service_with_connector(&path, connector.clone());
    let (mut ui, service_port) = controller_ports();
    let runtime = spawn_with_service(service_port, service.clone());

    let draft_request = |draft_id: DraftId, operation_id: OperationId| {
        service
            .prepare_secretless_draft_test(
                draft_id,
                operation_id,
                draft_for_test(),
                Duration::from_secs(5),
            )
            .expect("prepared draft request")
    };

    ui.try_submit(UiCommand::TestDraftConnection(draft_request(
        DraftId(1),
        OperationId(1_000),
    )))
    .expect("first draft work");
    gates[0].wait_until_entered().await;
    ui.try_submit(UiCommand::TestDraftConnection(draft_request(
        DraftId(1),
        OperationId(1_001),
    )))
    .expect("duplicate reaches controller");
    wait_for_event(&mut ui, |event| {
        matches!(
            event,
            UiEvent::DraftOperationFailed {
                draft_id: DraftId(1),
                operation_id: OperationId(1_001),
                summary: dbotter::model::PublicSummary::ResourceBusy,
            }
        )
    })
    .await;

    for (index, gate) in gates.iter().enumerate().skip(1) {
        ui.try_submit(UiCommand::TestDraftConnection(draft_request(
            DraftId(1 + index as u64),
            OperationId(1_001 + index as u64),
        )))
        .expect("draft within global limit");
        gate.wait_until_entered().await;
    }
    ui.try_submit(UiCommand::TestDraftConnection(draft_request(
        DraftId(9),
        OperationId(1_010),
    )))
    .expect("fifth draft reaches controller");
    wait_for_event(&mut ui, |event| {
        matches!(
            event,
            UiEvent::DraftOperationFailed {
                operation_id: OperationId(1_010),
                summary: dbotter::model::PublicSummary::ResourceBusy,
                ..
            }
        )
    })
    .await;
    assert_eq!(connector.connects.load(Ordering::SeqCst), 4);

    ui.try_submit(UiCommand::CancelOperation {
        operation_id: OperationId(1_000),
    })
    .expect("cancel draft");
    wait_for_event(&mut ui, |event| {
        matches!(
            event,
            UiEvent::DraftOperationFailed {
                operation_id: OperationId(1_000),
                summary: dbotter::model::PublicSummary::OperationCancelled,
                ..
            }
        )
    })
    .await;
    ui.try_submit(UiCommand::TestDraftConnection(draft_request(
        DraftId(1),
        OperationId(1_011),
    )))
    .expect("same draft slot is reusable");
    wait_for_event(&mut ui, |event| {
        matches!(
            event,
            UiEvent::DraftConnectionReady {
                draft_id: DraftId(1),
                operation_id: OperationId(1_011),
                ..
            }
        )
    })
    .await;
    assert_eq!(connector.connects.load(Ordering::SeqCst), 5);
    shutdown(&ui, runtime, OperationId(1_099)).await;
}

#[tokio::test]
async fn draft_cancel_and_timeout_close_the_exact_temporary_handle_before_terminal_event() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    let cancel_gate = Arc::new(AsyncGate::new());
    let timeout_gate = Arc::new(AsyncGate::new());
    let connector = Arc::new(ScriptedConnector::new(vec![
        SessionBehavior::Blocked(cancel_gate.clone()),
        SessionBehavior::Blocked(timeout_gate.clone()),
    ]));
    let service = test_service_with_connector(&path, connector.clone());
    let (mut ui, service_port) = controller_ports();
    let runtime = spawn_with_service(service_port, service.clone());

    let cancel_request = service
        .prepare_secretless_draft_test(
            DraftId(20),
            OperationId(1_020),
            draft_for_test(),
            Duration::from_secs(5),
        )
        .expect("cancel draft request");
    ui.try_submit(UiCommand::TestDraftConnection(cancel_request))
        .expect("cancel draft");
    cancel_gate.wait_until_entered().await;
    ui.try_submit(UiCommand::CancelOperation {
        operation_id: OperationId(1_020),
    })
    .expect("cancel control");
    wait_for_event(&mut ui, |event| {
        matches!(
            event,
            UiEvent::DraftOperationFailed {
                operation_id: OperationId(1_020),
                summary: dbotter::model::PublicSummary::OperationCancelled,
                ..
            }
        ) && connector.closes.load(Ordering::SeqCst) == 1
    })
    .await;

    let timeout_request = service
        .prepare_secretless_draft_test(
            DraftId(20),
            OperationId(1_021),
            draft_for_test(),
            Duration::from_millis(20),
        )
        .expect("timeout draft request");
    ui.try_submit(UiCommand::TestDraftConnection(timeout_request))
        .expect("timeout draft");
    timeout_gate.wait_until_entered().await;
    wait_for_event(&mut ui, |event| {
        matches!(
            event,
            UiEvent::DraftOperationFailed {
                operation_id: OperationId(1_021),
                summary: dbotter::model::PublicSummary::OperationTimedOut,
                ..
            }
        ) && connector.closes.load(Ordering::SeqCst) == 2
    })
    .await;
    shutdown(&ui, runtime, OperationId(1_029)).await;
}

#[tokio::test]
async fn draft_panic_closes_temporary_handle_once_before_internal_failure_and_reuses_slot() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    let connector = Arc::new(ScriptedConnector::new(vec![
        SessionBehavior::Panic,
        SessionBehavior::Immediate,
    ]));
    let service = test_service_with_connector(&path, connector.clone());
    let (mut ui, service_port) = controller_ports();
    let runtime = spawn_with_service(service_port, service.clone());

    let panic_request = service
        .prepare_secretless_draft_test(
            DraftId(30),
            OperationId(1_030),
            draft_for_test(),
            Duration::from_secs(1),
        )
        .expect("panic draft request");
    ui.try_submit(UiCommand::TestDraftConnection(panic_request))
        .expect("panic draft");
    wait_for_event(&mut ui, |event| {
        matches!(
            event,
            UiEvent::DraftOperationFailed {
                operation_id: OperationId(1_030),
                summary: dbotter::model::PublicSummary::InternalFailure,
                ..
            }
        )
    })
    .await;
    assert_eq!(
        connector.closes.load(Ordering::SeqCst),
        1,
        "panic recovery must close the ephemeral handle exactly once before terminal"
    );

    let replacement = service
        .prepare_secretless_draft_test(
            DraftId(30),
            OperationId(1_031),
            draft_for_test(),
            Duration::from_secs(1),
        )
        .expect("replacement draft request");
    ui.try_submit(UiCommand::TestDraftConnection(replacement))
        .expect("replacement draft");
    wait_for_event(&mut ui, |event| {
        matches!(
            event,
            UiEvent::DraftConnectionReady {
                operation_id: OperationId(1_031),
                draft_id: DraftId(30),
                ..
            }
        )
    })
    .await;
    assert_eq!(connector.closes.load(Ordering::SeqCst), 2);
    shutdown(&ui, runtime, OperationId(1_039)).await;
}

#[tokio::test]
async fn draft_event_full_cleans_before_emit_and_releases_same_draft_slot() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    let gate = Arc::new(AsyncGate::new());
    let connector = Arc::new(ScriptedConnector::new(vec![
        SessionBehavior::Blocked(gate.clone()),
        SessionBehavior::Immediate,
    ]));
    let service = test_service_with_connector(&path, connector.clone());
    let (mut ui, service_port) = bounded_ports(1);
    let runtime = spawn_with_service(service_port, service.clone());
    let request = |operation_id| {
        service
            .prepare_secretless_draft_test(
                DraftId(40),
                operation_id,
                draft_for_test(),
                Duration::from_secs(2),
            )
            .expect("draft request")
    };

    ui.try_submit(UiCommand::TestDraftConnection(request(OperationId(1_040))))
        .expect("blocked draft");
    gate.wait_until_entered().await;
    ui.try_submit(UiCommand::TestDraftConnection(request(OperationId(1_041))))
        .expect("busy draft fills event lane");
    tokio::time::sleep(Duration::from_millis(20)).await;
    ui.try_submit(UiCommand::CancelOperation {
        operation_id: OperationId(1_040),
    })
    .expect("cancel while event lane is full");
    wait_until(Duration::from_secs(1), || {
        connector.closes.load(Ordering::SeqCst) == 1
    })
    .await;
    assert_eq!(connector.closes.load(Ordering::SeqCst), 1);

    for attempt in 0..16_u64 {
        let _ = ui.drain_events(EVENT_CAPACITY);
        match ui.try_submit(UiCommand::TestDraftConnection(request(OperationId(
            1_050 + attempt,
        )))) {
            Ok(()) | Err(SubmitError::Busy) => {}
            Err(SubmitError::Disconnected) => panic!("runtime disconnected"),
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
        if connector.connects.load(Ordering::SeqCst) >= 2 {
            break;
        }
    }
    wait_until(Duration::from_secs(1), || {
        connector.closes.load(Ordering::SeqCst) == 2
    })
    .await;
    assert_eq!(connector.connects.load(Ordering::SeqCst), 2);
    assert_eq!(connector.closes.load(Ordering::SeqCst), 2);
    shutdown(&ui, runtime, OperationId(1_079)).await;
}

#[tokio::test]
async fn draft_event_closed_closes_once_without_runtime_deadlock() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    let gate = Arc::new(AsyncGate::new());
    let connector = Arc::new(ScriptedConnector::new(vec![SessionBehavior::Blocked(
        gate.clone(),
    )]));
    let service = test_service_with_connector(&path, connector.clone());
    let (ui, service_port) = bounded_ports(1);
    let runtime = spawn_with_service(service_port, service.clone());
    let request = service
        .prepare_secretless_draft_test(
            DraftId(50),
            OperationId(1_080),
            draft_for_test(),
            Duration::from_secs(2),
        )
        .expect("draft request");

    ui.try_submit(UiCommand::TestDraftConnection(request))
        .expect("blocked draft");
    gate.wait_until_entered().await;
    drop(ui);
    tokio::time::timeout(Duration::from_millis(500), runtime.wait())
        .await
        .expect("closed event receiver must not deadlock runtime")
        .expect("runtime joins");
    assert_eq!(connector.closes.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn delete_and_changed_reload_cancel_active_ping_before_closing_its_session() {
    for mode in ["delete", "changed", "uncertain"] {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("config.toml");
        let session = Arc::new(OrderingSession::default());
        let connector = Arc::new(OrderingConnector {
            session: session.clone(),
        });
        let service = test_service_with_connector(&path, connector);
        let profile = seed_profiles(&service, 1).await.remove(0);
        let (mut ui, service_port) = controller_ports();
        let runtime = spawn_with_service(service_port, service.clone());
        submit_test(&ui, &profile, OperationId(1_100), 5_000).expect("active ping");
        session.wait_until_ping_started().await;

        if mode == "changed" {
            let external = ApplicationService::load_path(&path).expect("external service");
            let mut changed = profile.persisted.as_draft();
            changed.host = "changed-before-runtime-cleanup.internal".to_owned();
            external
                .update_profile(UpdateProfileRequest {
                    profile_id: profile.id.clone(),
                    expected_generation: external
                        .profile_generation(&profile.id)
                        .await
                        .expect("external generation"),
                    operation_id: OperationId(1_101),
                    draft: changed,
                    secret_update: SessionSecretUpdate::Clear,
                    migration_consent: MigrationConsent::Cancelled,
                })
                .await
                .expect("external update");
            ui.try_submit(UiCommand::RefreshProfiles {
                operation_id: OperationId(1_102),
            })
            .expect("reload");
            wait_for_event(&mut ui, |event| {
                matches!(
                    event,
                    UiEvent::ProfilesLoaded {
                        operation_id: OperationId(1_102),
                        ..
                    }
                )
            })
            .await;
        } else if mode == "uncertain" {
            std::fs::write(&path, b"invalid = [toml").expect("corrupt config");
            ui.try_submit(UiCommand::RefreshProfiles {
                operation_id: OperationId(1_104),
            })
            .expect("uncertain reload");
            wait_for_event(&mut ui, |event| {
                matches!(
                    event,
                    UiEvent::ConfigUncertain {
                        operation_id: OperationId(1_104),
                    }
                )
            })
            .await;
        } else {
            ui.try_submit(UiCommand::DeleteProfile(DeleteProfileRequest {
                profile_id: profile.id.clone(),
                expected_generation: profile.generation,
                operation_id: OperationId(1_103),
                migration_consent: MigrationConsent::Cancelled,
            }))
            .expect("delete");
            wait_for_event(&mut ui, |event| {
                matches!(
                    event,
                    UiEvent::ProfileDeleted {
                        operation_id: OperationId(1_103),
                        ..
                    }
                )
            })
            .await;
        }
        assert!(session.ping_dropped.load(Ordering::SeqCst));
        assert!(
            !session.close_before_ping_drop.load(Ordering::SeqCst),
            "{mode}: fence must publish, then runtime must cancel/join, then close"
        );
        shutdown(&ui, runtime, OperationId(1_109)).await;
    }
}

#[tokio::test]
async fn whole_config_collateral_diff_defers_b_cleanup_and_preserves_a_b_and_c() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    let b_session = Arc::new(OrderingSession::default());
    let connector = Arc::new(CollateralConnector {
        blocked_profile: "profile-0".to_owned(),
        blocked: b_session.clone(),
    });
    let service = test_service_with_connector(&path, connector);
    let profiles = seed_profiles(&service, 3).await;
    let b = profiles[0].clone();
    let c = profiles[2].clone();
    let c_connected = service
        .check_at(
            OperationId(1_150),
            c.id.clone(),
            c.generation,
            Duration::from_secs(1),
        )
        .await
        .expect("cache unrelated C");

    let (mut ui, service_port) = controller_ports();
    let runtime = spawn_with_service(service_port, service.clone());
    submit_test(&ui, &b, OperationId(1_151), 5_000).expect("active B");
    b_session.wait_until_ping_started().await;

    let external = ApplicationService::load_path(&path).expect("external service");
    let mut changed_b = b.persisted.as_draft();
    changed_b.host = "externally-changed-b.internal".to_owned();
    external
        .update_profile(UpdateProfileRequest {
            profile_id: b.id.clone(),
            expected_generation: external
                .profile_generation(&b.id)
                .await
                .expect("external B generation"),
            operation_id: OperationId(1_152),
            draft: changed_b,
            secret_update: SessionSecretUpdate::Clear,
            migration_consent: MigrationConsent::Cancelled,
        })
        .await
        .expect("external B update");

    ui.try_submit(UiCommand::CreateProfile(create_request(
        "local-a",
        OperationId(1_153),
    )))
    .expect("local A create");
    wait_for_event(&mut ui, |event| {
        matches!(
            event,
            UiEvent::ProfileSaved {
                operation_id: OperationId(1_153),
                ..
            }
        )
    })
    .await;

    assert!(b_session.ping_dropped.load(Ordering::SeqCst));
    assert!(
        !b_session.close_before_ping_drop.load(Ordering::SeqCst),
        "B must be fenced and joined before its exact old session closes"
    );
    assert!(service.cached_session_identity(&b.id).await.is_none());
    assert_eq!(
        service
            .cached_session_identity(&c.id)
            .await
            .map(|identity| identity.session_generation),
        Some(c_connected.session_generation)
    );
    let persisted = service.profiles_snapshot().await;
    assert!(persisted.iter().any(|profile| profile.id == "local-a"));
    assert!(persisted.iter().any(|profile| {
        profile.id == b.id.as_str() && profile.host == "externally-changed-b.internal"
    }));
    assert!(persisted.iter().any(|profile| profile.id == c.id.as_str()));
    shutdown(&ui, runtime, OperationId(1_159)).await;
}

#[tokio::test]
async fn cancellation_before_lease_identity_cannot_evict_a_concurrent_replacement() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    let connector = Arc::new(PreLeaseConnector::default());
    let service = test_service_with_connector(&path, connector.clone());
    let profile = seed_profiles(&service, 1).await.remove(0);
    let (mut ui, service_port) = controller_ports();
    let runtime = spawn_with_service(service_port, service.clone());

    submit_test(&ui, &profile, OperationId(1_200), 5_000).expect("pre-lease work");
    connector.first_connect.wait_until_entered().await;
    let replacement = service
        .check_at(
            OperationId(1_201),
            profile.id.clone(),
            profile.generation,
            Duration::from_secs(1),
        )
        .await
        .expect("concurrent replacement");
    ui.try_submit(UiCommand::CancelOperation {
        operation_id: OperationId(1_200),
    })
    .expect("cancel before lease");
    wait_for_event(&mut ui, |event| {
        matches!(
            event,
            UiEvent::OperationFailed {
                operation_id: OperationId(1_200),
                summary: dbotter::model::PublicSummary::OperationCancelled,
                ..
            }
        )
    })
    .await;
    assert_eq!(
        service
            .cached_session_identity(&profile.id)
            .await
            .map(|identity| identity.session_generation),
        Some(replacement.session_generation)
    );
    shutdown(&ui, runtime, OperationId(1_209)).await;
}

#[tokio::test]
async fn cancel_key_coalesces_while_target_cleanup_blocks_and_releases_on_terminal() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    let session = Arc::new(CloseBarrierSession::pending());
    let connector = Arc::new(ReplacementRaceConnector::new(session.clone()));
    let service = test_service_with_connector(&path, connector);
    let profile = seed_profiles(&service, 1).await.remove(0);
    let (mut ui, service_port) = bounded_ports(1);
    let runtime = spawn_with_service(service_port, service);
    let operation_id = OperationId(1_240);

    submit_test(&ui, &profile, operation_id, 5_000).expect("blocked profile work");
    session.wait_until_ping_started().await;
    ui.try_submit(UiCommand::CancelOperation { operation_id })
        .expect("first cancel");
    session.wait_until_close_started().await;

    for _ in 0..64 {
        ui.try_submit(UiCommand::CancelOperation { operation_id })
            .expect("duplicate cancel is coalesced while cleanup is pending");
    }
    let probe_profile = ProfileId("cancel-key-probe".to_owned());
    assert_eq!(
        ui.try_submit(UiCommand::ReconnectProfile {
            operation_id: OperationId(1_241),
            profile_id: probe_profile.clone(),
            profile_generation: ProfileGeneration(1),
            timeout_ms: 100,
        }),
        Ok(()),
        "duplicate cancel must not consume the control lane before terminal cleanup"
    );
    wait_for_event(&mut ui, |event| {
        matches!(
            event,
            UiEvent::OperationFailed {
                operation_id: OperationId(1_241),
                kind: dbotter::model::OperationKind::ReconnectProfile,
                summary: dbotter::model::PublicSummary::ResourceStale,
                ..
            }
        )
    })
    .await;

    session.release_close();
    wait_for_event(&mut ui, |event| {
        matches!(
            event,
            UiEvent::OperationFailed {
                operation_id: actual,
                summary: dbotter::model::PublicSummary::OperationCancelled,
                ..
            } if *actual == operation_id
        )
    })
    .await;

    ui.try_submit(UiCommand::CancelOperation { operation_id })
        .expect("terminal event releases cancel coalescing key");
    assert_eq!(
        ui.try_submit(UiCommand::ReconnectProfile {
            operation_id: OperationId(1_242),
            profile_id: probe_profile,
            profile_generation: ProfileGeneration(1),
            timeout_ms: 100,
        }),
        Err(SubmitError::Busy),
        "post-terminal stale cancel must enter normal control-lane processing"
    );
    shutdown(&ui, runtime, OperationId(1_249)).await;
}

#[tokio::test]
async fn cancel_then_disconnect_preserves_target_terminal_before_takeover_terminal() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    let session = Arc::new(CloseBarrierSession::pending());
    let connector = Arc::new(ReplacementRaceConnector::new(session.clone()));
    let service = test_service_with_connector(&path, connector);
    let profile = seed_profiles(&service, 1).await.remove(0);
    let (mut ui, service_port) = controller_ports();
    let runtime = spawn_with_service(service_port, service);
    let target_operation = OperationId(1_243);
    let disconnect_operation = OperationId(1_244);

    submit_test(&ui, &profile, target_operation, 5_000).expect("active target");
    session.wait_until_ping_started().await;
    ui.try_submit(UiCommand::CancelOperation {
        operation_id: target_operation,
    })
    .expect("cancel target");
    session.wait_until_close_started().await;
    ui.try_submit(UiCommand::DisconnectProfile {
        operation_id: disconnect_operation,
        profile_id: profile.id.clone(),
        profile_generation: profile.generation,
    })
    .expect("disconnect takeover");
    tokio::task::yield_now().await;
    session.release_close();

    let first = wait_for_event(&mut ui, |event| {
        matches!(
            event,
            UiEvent::OperationFailed {
                operation_id: actual,
                summary: dbotter::model::PublicSummary::OperationCancelled,
                ..
            } if *actual == target_operation
        ) || matches!(
            event,
            UiEvent::ConnectionClosed {
                operation_id: actual,
                ..
            } if *actual == disconnect_operation
        )
    })
    .await;
    assert!(matches!(
        first,
        UiEvent::OperationFailed {
            operation_id: actual,
            summary: dbotter::model::PublicSummary::OperationCancelled,
            ..
        } if actual == target_operation
    ));
    wait_for_event(&mut ui, |event| {
        matches!(
            event,
            UiEvent::ConnectionClosed {
                operation_id: actual,
                ..
            } if *actual == disconnect_operation
        )
    })
    .await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(
        ui.drain_events(EVENT_CAPACITY)
            .into_iter()
            .filter(|event| matches!(
                event,
                UiEvent::OperationFailed {
                    operation_id: actual,
                    summary: dbotter::model::PublicSummary::OperationCancelled,
                    ..
                } if *actual == target_operation
            ))
            .count(),
        0,
        "target cancellation terminal must be emitted exactly once"
    );
    shutdown(&ui, runtime, OperationId(1_249)).await;
}

#[tokio::test]
async fn cancel_join_compare_removes_only_registered_session_during_replacement_install() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    let old_session = Arc::new(CloseBarrierSession::pending());
    let connector = Arc::new(ReplacementRaceConnector::new(old_session.clone()));
    let service = test_service_with_connector(&path, connector);
    let profile = seed_profiles(&service, 1).await.remove(0);
    let (mut ui, service_port) = controller_ports();
    let runtime = spawn_with_service(service_port, service.clone());

    submit_test(&ui, &profile, OperationId(1_250), 5_000).expect("old work");
    old_session.wait_until_ping_started().await;
    ui.try_submit(UiCommand::DisconnectProfile {
        operation_id: OperationId(1_251),
        profile_id: profile.id.clone(),
        profile_generation: profile.generation,
    })
    .expect("disconnect old session");
    old_session.wait_until_close_started().await;

    let replacement = service
        .check_at(
            OperationId(1_252),
            profile.id.clone(),
            profile.generation,
            Duration::from_secs(1),
        )
        .await
        .expect("replacement session");
    old_session.release_close();
    wait_for_event(&mut ui, |event| {
        matches!(
            event,
            UiEvent::ConnectionClosed {
                operation_id: OperationId(1_251),
                ..
            }
        )
    })
    .await;

    assert_eq!(
        service
            .cached_session_identity(&profile.id)
            .await
            .map(|identity| identity.session_generation),
        Some(replacement.session_generation),
        "disconnect cleanup must compare-remove the registered old identity only"
    );
    shutdown(&ui, runtime, OperationId(1_259)).await;
}

#[tokio::test]
async fn panic_cleanup_compare_removes_only_registered_session_during_replacement_install() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    let old_session = Arc::new(CloseBarrierSession::panicking());
    let connector = Arc::new(ReplacementRaceConnector::new(old_session.clone()));
    let service = test_service_with_connector(&path, connector);
    let profile = seed_profiles(&service, 1).await.remove(0);
    let (mut ui, service_port) = controller_ports();
    let runtime = spawn_with_service(service_port, service.clone());

    submit_test(&ui, &profile, OperationId(1_260), 5_000).expect("panicking work");
    old_session.wait_until_close_started().await;
    let replacement = service
        .check_at(
            OperationId(1_261),
            profile.id.clone(),
            profile.generation,
            Duration::from_secs(1),
        )
        .await
        .expect("replacement after panic");
    old_session.release_close();
    wait_for_event(&mut ui, |event| {
        matches!(
            event,
            UiEvent::OperationFailed {
                operation_id: OperationId(1_260),
                summary: dbotter::model::PublicSummary::InternalFailure,
                ..
            }
        )
    })
    .await;

    assert_eq!(
        service
            .cached_session_identity(&profile.id)
            .await
            .map(|identity| identity.session_generation),
        Some(replacement.session_generation),
        "panic cleanup must compare-remove the registered old identity only"
    );
    shutdown(&ui, runtime, OperationId(1_269)).await;
}

#[tokio::test]
async fn shutdown_aborts_registered_blocked_panic_cleanup_without_inline_retry() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    let session = Arc::new(CloseBarrierSession::panicking());
    let connector = Arc::new(ReplacementRaceConnector::new(session.clone()));
    let service = test_service_with_connector(&path, connector);
    let profile = seed_profiles(&service, 1).await.remove(0);
    let (mut ui, service_port) = controller_ports();
    let runtime = spawn_with_service(service_port, service.clone());
    let panic_operation = OperationId(1_263);
    let shutdown_operation = OperationId(1_264);

    submit_test(&ui, &profile, panic_operation, 5_000).expect("panicking work");
    session.wait_until_close_started().await;
    ui.try_submit(UiCommand::ShutdownRuntime {
        operation_id: shutdown_operation,
    })
    .expect("shutdown remains pollable during panic cleanup");

    let mut wait = tokio::spawn(runtime.wait());
    let completed = tokio::time::timeout(Duration::from_millis(2_300), &mut wait).await;
    if completed.is_err() {
        session.release_close();
        let _ = tokio::time::timeout(Duration::from_millis(500), &mut wait).await;
    }
    assert!(
        completed.is_ok(),
        "panic cleanup must be registered and abortable after the two-second shutdown grace"
    );
    assert!(
        service.cached_session_identity(&profile.id).await.is_none(),
        "registered panic cleanup must exact-take the failed cached identity before close"
    );

    let events = ui.drain_events(EVENT_CAPACITY);
    let failure_positions = events
        .iter()
        .enumerate()
        .filter_map(|(index, event)| {
            matches!(
                event,
                UiEvent::OperationFailed {
                    operation_id: actual,
                    summary: dbotter::model::PublicSummary::InternalFailure,
                    ..
                } if *actual == panic_operation
            )
            .then_some(index)
        })
        .collect::<Vec<_>>();
    let shutdown_position = events
        .iter()
        .position(|event| {
            matches!(
                event,
                UiEvent::RuntimeShutdown {
                    operation_id: actual
                } if *actual == shutdown_operation
            )
        })
        .expect("runtime shutdown terminal");
    assert_eq!(
        failure_positions.len(),
        1,
        "panic terminal must be emitted once"
    );
    assert!(
        failure_positions[0] < shutdown_position,
        "panic terminal must precede RuntimeShutdown"
    );
}

#[tokio::test]
async fn shutdown_interrupts_blocked_disconnect_cleanup_within_two_second_grace() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    let session = Arc::new(CloseBarrierSession::pending());
    let connector = Arc::new(ReplacementRaceConnector::new(session.clone()));
    let service = test_service_with_connector(&path, connector);
    let profile = seed_profiles(&service, 1).await.remove(0);
    let (ui, service_port) = controller_ports();
    let runtime = spawn_with_service(service_port, service);

    submit_test(&ui, &profile, OperationId(1_270), 5_000).expect("active work");
    session.wait_until_ping_started().await;
    ui.try_submit(UiCommand::DisconnectProfile {
        operation_id: OperationId(1_271),
        profile_id: profile.id.clone(),
        profile_generation: profile.generation,
    })
    .expect("blocked disconnect");
    session.wait_until_close_started().await;
    for offset in 0..(CONTROL_CAPACITY * 4) {
        ui.try_submit(UiCommand::DisconnectProfile {
            operation_id: OperationId(1_300 + offset as u64),
            profile_id: profile.id.clone(),
            profile_generation: profile.generation,
        })
        .expect("duplicate disconnect is coalesced without consuming control capacity");
    }
    ui.try_submit(UiCommand::ShutdownRuntime {
        operation_id: OperationId(1_272),
    })
    .expect("shutdown bypasses blocked control cleanup");

    let mut wait = tokio::spawn(runtime.wait());
    let completed = tokio::time::timeout(Duration::from_millis(2_300), &mut wait).await;
    if completed.is_err() {
        session.release_close();
        let _ = tokio::time::timeout(Duration::from_millis(500), &mut wait).await;
    }
    assert!(
        completed.is_ok(),
        "shutdown watch must remain pollable and abort blocked network cleanup after 2s"
    );
}

#[tokio::test]
async fn shutdown_remains_pollable_during_committed_delete_cleanup_and_preserves_terminal_order() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    let session = Arc::new(CloseBarrierSession::pending());
    let connector = Arc::new(ReplacementRaceConnector::new(session.clone()));
    let service = test_service_with_connector(&path, connector);
    let profile = seed_profiles(&service, 1).await.remove(0);
    let (mut ui, service_port) = controller_ports();
    let runtime = spawn_with_service(service_port, service);

    submit_test(&ui, &profile, OperationId(1_280), 5_000).expect("active ping");
    session.wait_until_ping_started().await;
    ui.try_submit(UiCommand::DeleteProfile(DeleteProfileRequest {
        profile_id: profile.id.clone(),
        expected_generation: profile.generation,
        operation_id: OperationId(1_281),
        migration_consent: MigrationConsent::Cancelled,
    }))
    .expect("committed delete");
    session.wait_until_close_started().await;
    ui.try_submit(UiCommand::ShutdownRuntime {
        operation_id: OperationId(1_282),
    })
    .expect("shutdown watch");

    let mut runtime_wait = tokio::spawn(runtime.wait());
    let terminals = tokio::time::timeout(Duration::from_millis(2_300), async {
        let mutation = loop {
            let event = ui
                .next_event()
                .await
                .expect("runtime event lane remains open");
            if matches!(
                event,
                UiEvent::ProfileDeleted {
                    operation_id: OperationId(1_281),
                    ..
                } | UiEvent::RuntimeShutdown {
                    operation_id: OperationId(1_282)
                }
            ) {
                break event;
            }
        };
        assert!(matches!(
            mutation,
            UiEvent::ProfileDeleted {
                operation_id: OperationId(1_281),
                ..
            }
        ));
        loop {
            let event = ui
                .next_event()
                .await
                .expect("runtime event lane remains open");
            if matches!(
                event,
                UiEvent::RuntimeShutdown {
                    operation_id: OperationId(1_282)
                }
            ) {
                break;
            }
        }
    })
    .await;
    if terminals.is_err() {
        session.release_close();
        let _ = tokio::time::timeout(Duration::from_millis(500), &mut runtime_wait).await;
    }
    assert!(
        terminals.is_ok(),
        "shutdown must abort blocked network cleanup after grace without losing delete outcome"
    );
    runtime_wait
        .await
        .expect("runtime wait task joins")
        .expect("runtime joins");
}

#[tokio::test]
async fn stale_reconnect_cannot_evict_a_newer_profile_generation_session() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    let service = test_service(&path, Arc::new(CountingConnector::default()));
    let profile = seed_profiles(&service, 1).await.remove(0);
    let mut changed = profile.persisted.as_draft();
    changed.name = "new generation".to_owned();
    let updated = service
        .update_profile(UpdateProfileRequest {
            profile_id: profile.id.clone(),
            expected_generation: profile.generation,
            operation_id: OperationId(1_300),
            draft: changed,
            secret_update: SessionSecretUpdate::Clear,
            migration_consent: MigrationConsent::Cancelled,
        })
        .await
        .expect("new profile generation");
    let connected = service
        .check_at(
            OperationId(1_301),
            profile.id.clone(),
            updated.profile_generation,
            Duration::from_secs(1),
        )
        .await
        .expect("new generation session");
    let (mut ui, service_port) = controller_ports();
    let runtime = spawn_with_service(service_port, service.clone());
    ui.try_submit(UiCommand::ReconnectProfile {
        operation_id: OperationId(1_302),
        profile_id: profile.id.clone(),
        profile_generation: profile.generation,
        timeout_ms: 1_000,
    })
    .expect("stale reconnect");
    wait_for_event(&mut ui, |event| {
        matches!(
            event,
            UiEvent::OperationFailed {
                operation_id: OperationId(1_302),
                summary: dbotter::model::PublicSummary::ResourceStale,
                ..
            }
        )
    })
    .await;
    assert_eq!(
        service
            .cached_session_identity(&profile.id)
            .await
            .map(|identity| (identity.profile_generation, identity.session_generation)),
        Some((updated.profile_generation, connected.session_generation))
    );
    shutdown(&ui, runtime, OperationId(1_309)).await;
}

#[derive(Default)]
struct CountingSession {
    executes: Arc<AtomicUsize>,
}

#[derive(Clone)]
struct CountingMySqlResources {
    executes: Arc<AtomicUsize>,
}

#[async_trait]
impl ConnectionPing for CountingMySqlResources {
    async fn ping(&self, _timeout: Duration) -> Result<(), DriverError> {
        Ok(())
    }
}

#[async_trait]
impl MySqlPreparedExecution for CountingMySqlResources {
    async fn execute_prepared(
        &self,
        _request: &PreparedMySqlRequest,
    ) -> Result<QueryResult, DriverError> {
        self.executes.fetch_add(1, Ordering::SeqCst);
        Ok(empty_driver_result())
    }
}

#[async_trait]
impl CatalogBrowser for CountingMySqlResources {
    async fn load_page(&self, _request: &CatalogRequest) -> Result<CatalogPage, DriverError> {
        Err(DriverError::Unsupported {
            driver: DriverKind::MySql,
            operation: "test catalog".to_owned(),
        })
    }
}

#[async_trait]
impl SessionHandle for CountingSession {
    async fn ping(&self, _timeout: Duration) -> Result<(), DriverError> {
        Ok(())
    }

    fn connected_resources(&self) -> Option<ConnectedResources> {
        let resources = Arc::new(CountingMySqlResources {
            executes: self.executes.clone(),
        });
        Some(ConnectedResources::MySql {
            ping: resources.clone(),
            execution: resources.clone(),
            catalog: resources,
        })
    }
}

#[derive(Default)]
struct CountingConnector {
    connects: AtomicUsize,
    executes: Arc<AtomicUsize>,
}

#[async_trait]
impl SessionConnector for CountingConnector {
    async fn connect(
        &self,
        _profile: &dbotter::model::ConnectionProfile,
        _secret: Option<&SessionSecret>,
        _timeout: Duration,
    ) -> Result<Arc<dyn SessionHandle>, DriverError> {
        self.connects.fetch_add(1, Ordering::SeqCst);
        Ok(Arc::new(CountingSession {
            executes: self.executes.clone(),
        }))
    }
}

#[derive(Clone, Copy)]
enum ExecuteResourceBehavior {
    Blocked,
    PreparedUnsupported { session_healthy: bool },
}

struct ExecuteDropGuard(Arc<AtomicBool>);

impl Drop for ExecuteDropGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

struct ExecuteTestResources {
    behavior: ExecuteResourceBehavior,
    started: AtomicBool,
    started_notify: tokio::sync::Notify,
    execute_dropped: Arc<AtomicBool>,
}

impl ExecuteTestResources {
    fn new(behavior: ExecuteResourceBehavior) -> Self {
        Self {
            behavior,
            started: AtomicBool::new(false),
            started_notify: tokio::sync::Notify::new(),
            execute_dropped: Arc::new(AtomicBool::new(false)),
        }
    }

    async fn wait_until_started(&self) {
        while !self.started.load(Ordering::SeqCst) {
            self.started_notify.notified().await;
        }
    }
}

#[async_trait]
impl ConnectionPing for ExecuteTestResources {
    async fn ping(&self, _timeout: Duration) -> Result<(), DriverError> {
        Ok(())
    }
}

#[async_trait]
impl MySqlPreparedExecution for ExecuteTestResources {
    async fn execute_prepared(
        &self,
        _request: &PreparedMySqlRequest,
    ) -> Result<QueryResult, DriverError> {
        match self.behavior {
            ExecuteResourceBehavior::Blocked => {
                let _guard = ExecuteDropGuard(self.execute_dropped.clone());
                self.started.store(true, Ordering::SeqCst);
                self.started_notify.notify_waiters();
                std::future::pending::<()>().await;
                Ok(empty_driver_result())
            }
            ExecuteResourceBehavior::PreparedUnsupported { session_healthy } => {
                Err(DriverError::PreparedStatementUnsupported { session_healthy })
            }
        }
    }
}

#[async_trait]
impl CatalogBrowser for ExecuteTestResources {
    async fn load_page(&self, _request: &CatalogRequest) -> Result<CatalogPage, DriverError> {
        Err(DriverError::Unsupported {
            driver: DriverKind::MySql,
            operation: "test catalog".to_owned(),
        })
    }
}

struct ExecuteTestSession {
    resources: Arc<ExecuteTestResources>,
    closes: AtomicUsize,
    close_before_execute_drop: AtomicBool,
}

impl ExecuteTestSession {
    fn new(behavior: ExecuteResourceBehavior) -> Self {
        Self {
            resources: Arc::new(ExecuteTestResources::new(behavior)),
            closes: AtomicUsize::new(0),
            close_before_execute_drop: AtomicBool::new(false),
        }
    }
}

#[async_trait]
impl SessionHandle for ExecuteTestSession {
    async fn ping(&self, _timeout: Duration) -> Result<(), DriverError> {
        Ok(())
    }

    fn connected_resources(&self) -> Option<ConnectedResources> {
        Some(ConnectedResources::MySql {
            ping: self.resources.clone(),
            execution: self.resources.clone(),
            catalog: self.resources.clone(),
        })
    }

    async fn close(&self) -> Result<(), DriverError> {
        if matches!(self.resources.behavior, ExecuteResourceBehavior::Blocked)
            && !self.resources.execute_dropped.load(Ordering::SeqCst)
        {
            self.close_before_execute_drop.store(true, Ordering::SeqCst);
        }
        self.closes.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

struct ExecuteTestConnector {
    session: Arc<ExecuteTestSession>,
}

#[async_trait]
impl SessionConnector for ExecuteTestConnector {
    async fn connect(
        &self,
        _profile: &dbotter::model::ConnectionProfile,
        _secret: Option<&SessionSecret>,
        _timeout: Duration,
    ) -> Result<Arc<dyn SessionHandle>, DriverError> {
        Ok(self.session.clone())
    }
}

struct PingDropGuard(Arc<AtomicBool>);

impl Drop for PingDropGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

struct OrderingSession {
    ping_started: AtomicBool,
    ping_started_notify: tokio::sync::Notify,
    ping_dropped: Arc<AtomicBool>,
    close_before_ping_drop: AtomicBool,
}

impl Default for OrderingSession {
    fn default() -> Self {
        Self {
            ping_started: AtomicBool::new(false),
            ping_started_notify: tokio::sync::Notify::new(),
            ping_dropped: Arc::new(AtomicBool::new(false)),
            close_before_ping_drop: AtomicBool::new(false),
        }
    }
}

impl OrderingSession {
    async fn wait_until_ping_started(&self) {
        while !self.ping_started.load(Ordering::SeqCst) {
            self.ping_started_notify.notified().await;
        }
    }
}

#[async_trait]
impl SessionHandle for OrderingSession {
    async fn ping(&self, _timeout: Duration) -> Result<(), DriverError> {
        let _guard = PingDropGuard(self.ping_dropped.clone());
        self.ping_started.store(true, Ordering::SeqCst);
        self.ping_started_notify.notify_waiters();
        std::future::pending::<()>().await;
        Ok(())
    }

    async fn close(&self) -> Result<(), DriverError> {
        if !self.ping_dropped.load(Ordering::SeqCst) {
            self.close_before_ping_drop.store(true, Ordering::SeqCst);
        }
        Ok(())
    }
}

struct OrderingConnector {
    session: Arc<OrderingSession>,
}

#[async_trait]
impl SessionConnector for OrderingConnector {
    async fn connect(
        &self,
        _profile: &dbotter::model::ConnectionProfile,
        _secret: Option<&SessionSecret>,
        _timeout: Duration,
    ) -> Result<Arc<dyn SessionHandle>, DriverError> {
        Ok(self.session.clone())
    }
}

struct CollateralConnector {
    blocked_profile: String,
    blocked: Arc<OrderingSession>,
}

#[async_trait]
impl SessionConnector for CollateralConnector {
    async fn connect(
        &self,
        profile: &dbotter::model::ConnectionProfile,
        _secret: Option<&SessionSecret>,
        _timeout: Duration,
    ) -> Result<Arc<dyn SessionHandle>, DriverError> {
        if profile.id == self.blocked_profile {
            Ok(self.blocked.clone())
        } else {
            Ok(Arc::new(CountingSession::default()))
        }
    }
}

struct PreLeaseConnector {
    first_connect: Arc<AsyncGate>,
    calls: AtomicUsize,
}

impl Default for PreLeaseConnector {
    fn default() -> Self {
        Self {
            first_connect: Arc::new(AsyncGate::new()),
            calls: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl SessionConnector for PreLeaseConnector {
    async fn connect(
        &self,
        _profile: &dbotter::model::ConnectionProfile,
        _secret: Option<&SessionSecret>,
        _timeout: Duration,
    ) -> Result<Arc<dyn SessionHandle>, DriverError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            self.first_connect.wait().await;
        }
        Ok(Arc::new(CountingSession::default()))
    }
}

#[derive(Clone, Copy)]
enum BarrierPingBehavior {
    Pending,
    Panic,
}

struct CloseBarrierSession {
    ping_behavior: BarrierPingBehavior,
    ping_started: AtomicBool,
    ping_started_notify: tokio::sync::Notify,
    close_started: AtomicBool,
    close_started_notify: tokio::sync::Notify,
    close_released: AtomicBool,
    close_released_notify: tokio::sync::Notify,
}

impl CloseBarrierSession {
    fn pending() -> Self {
        Self::new(BarrierPingBehavior::Pending)
    }

    fn panicking() -> Self {
        Self::new(BarrierPingBehavior::Panic)
    }

    fn new(ping_behavior: BarrierPingBehavior) -> Self {
        Self {
            ping_behavior,
            ping_started: AtomicBool::new(false),
            ping_started_notify: tokio::sync::Notify::new(),
            close_started: AtomicBool::new(false),
            close_started_notify: tokio::sync::Notify::new(),
            close_released: AtomicBool::new(false),
            close_released_notify: tokio::sync::Notify::new(),
        }
    }

    async fn wait_until_ping_started(&self) {
        while !self.ping_started.load(Ordering::SeqCst) {
            self.ping_started_notify.notified().await;
        }
    }

    async fn wait_until_close_started(&self) {
        while !self.close_started.load(Ordering::SeqCst) {
            self.close_started_notify.notified().await;
        }
    }

    fn release_close(&self) {
        self.close_released.store(true, Ordering::SeqCst);
        self.close_released_notify.notify_waiters();
    }
}

#[async_trait]
impl SessionHandle for CloseBarrierSession {
    async fn ping(&self, _timeout: Duration) -> Result<(), DriverError> {
        self.ping_started.store(true, Ordering::SeqCst);
        self.ping_started_notify.notify_waiters();
        match self.ping_behavior {
            BarrierPingBehavior::Pending => std::future::pending::<Result<(), DriverError>>().await,
            BarrierPingBehavior::Panic => panic!("injected replacement-race panic"),
        }
    }

    async fn close(&self) -> Result<(), DriverError> {
        self.close_started.store(true, Ordering::SeqCst);
        self.close_started_notify.notify_waiters();
        while !self.close_released.load(Ordering::SeqCst) {
            self.close_released_notify.notified().await;
        }
        Ok(())
    }
}

struct ReplacementRaceConnector {
    old_session: Arc<CloseBarrierSession>,
    calls: AtomicUsize,
}

impl ReplacementRaceConnector {
    fn new(old_session: Arc<CloseBarrierSession>) -> Self {
        Self {
            old_session,
            calls: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl SessionConnector for ReplacementRaceConnector {
    async fn connect(
        &self,
        _profile: &dbotter::model::ConnectionProfile,
        _secret: Option<&SessionSecret>,
        _timeout: Duration,
    ) -> Result<Arc<dyn SessionHandle>, DriverError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            Ok(self.old_session.clone())
        } else {
            Ok(Arc::new(CountingSession::default()))
        }
    }
}

struct AsyncGate {
    entered: AtomicBool,
    notify: tokio::sync::Notify,
}

impl AsyncGate {
    fn new() -> Self {
        Self {
            entered: AtomicBool::new(false),
            notify: tokio::sync::Notify::new(),
        }
    }

    async fn wait(&self) {
        self.entered.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
        std::future::pending::<()>().await;
    }

    async fn wait_until_entered(&self) {
        while !self.entered.load(Ordering::SeqCst) {
            self.notify.notified().await;
        }
    }
}

enum SessionBehavior {
    Immediate,
    Blocked(Arc<AsyncGate>),
    Panic,
}

struct ScriptedSession {
    behavior: SessionBehavior,
    closes: Arc<AtomicUsize>,
}

#[async_trait]
impl SessionHandle for ScriptedSession {
    async fn ping(&self, _timeout: Duration) -> Result<(), DriverError> {
        match &self.behavior {
            SessionBehavior::Immediate => Ok(()),
            SessionBehavior::Blocked(gate) => {
                gate.wait().await;
                Ok(())
            }
            SessionBehavior::Panic => panic!("injected session panic"),
        }
    }

    async fn close(&self) -> Result<(), DriverError> {
        self.closes.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

struct ScriptedConnector {
    behaviors: Mutex<VecDeque<SessionBehavior>>,
    connects: AtomicUsize,
    closes: Arc<AtomicUsize>,
}

#[derive(Default)]
struct ArmedPreRenameFailure {
    armed: AtomicBool,
}

#[derive(Default)]
struct ArmedObservationFailure {
    armed: AtomicBool,
}

#[derive(Default)]
struct BlockingPreRename {
    entered: AtomicBool,
    released: Mutex<bool>,
    condition: Condvar,
}

impl BlockingPreRename {
    async fn wait_until_entered(&self) {
        wait_until(Duration::from_secs(1), || {
            self.entered.load(Ordering::SeqCst)
        })
        .await;
    }

    fn release(&self) {
        if let Ok(mut released) = self.released.lock() {
            *released = true;
            self.condition.notify_all();
        }
    }
}

impl MutationFaultInjector for BlockingPreRename {
    fn check(&self, point: MutationFailpoint, _path: &std::path::Path) -> std::io::Result<()> {
        if point != MutationFailpoint::MainPreRename {
            return Ok(());
        }
        self.entered.store(true, Ordering::SeqCst);
        let mut released = self
            .released
            .lock()
            .map_err(|_| std::io::Error::other("barrier lock poisoned"))?;
        while !*released {
            released = self
                .condition
                .wait(released)
                .map_err(|_| std::io::Error::other("barrier wait poisoned"))?;
        }
        Ok(())
    }
}

impl ArmedPreRenameFailure {
    fn arm(&self) {
        self.armed.store(true, Ordering::SeqCst);
    }
}

impl ArmedObservationFailure {
    fn arm(&self) {
        self.armed.store(true, Ordering::SeqCst);
    }
}

impl MutationFaultInjector for ArmedObservationFailure {
    fn check(&self, point: MutationFailpoint, _path: &std::path::Path) -> std::io::Result<()> {
        if point == MutationFailpoint::MainObservationLoad
            && self.armed.swap(false, Ordering::SeqCst)
        {
            Err(std::io::Error::other("injected observation failure"))
        } else {
            Ok(())
        }
    }
}

impl MutationFaultInjector for ArmedPreRenameFailure {
    fn check(&self, point: MutationFailpoint, _path: &std::path::Path) -> std::io::Result<()> {
        if point == MutationFailpoint::MainPreRename && self.armed.swap(false, Ordering::SeqCst) {
            Err(std::io::Error::other("injected pre-rename failure"))
        } else {
            Ok(())
        }
    }
}

impl ScriptedConnector {
    fn new(behaviors: Vec<SessionBehavior>) -> Self {
        Self {
            behaviors: Mutex::new(behaviors.into()),
            connects: AtomicUsize::new(0),
            closes: Arc::new(AtomicUsize::new(0)),
        }
    }
}

#[async_trait]
impl SessionConnector for ScriptedConnector {
    async fn connect(
        &self,
        profile: &dbotter::model::ConnectionProfile,
        _secret: Option<&SessionSecret>,
        _timeout: Duration,
    ) -> Result<Arc<dyn SessionHandle>, DriverError> {
        self.connects.fetch_add(1, Ordering::SeqCst);
        let behavior = self
            .behaviors
            .lock()
            .map_err(|_| DriverError::Unavailable {
                driver: profile.driver,
                reason: "script lock unavailable",
            })?
            .pop_front()
            .unwrap_or(SessionBehavior::Immediate);
        Ok(Arc::new(ScriptedSession {
            behavior,
            closes: self.closes.clone(),
        }))
    }
}

#[derive(Default)]
struct MissingEnvironment;

impl SecretResolver for MissingEnvironment {
    fn resolve_environment(&self, name: &str) -> Result<Arc<SessionSecret>, SecretError> {
        Err(SecretError::MissingEnv(name.to_owned()))
    }
}

fn test_service(path: &std::path::Path, connector: Arc<CountingConnector>) -> ApplicationService {
    test_service_with_connector(path, connector)
}

fn test_service_with_connector(
    path: &std::path::Path,
    connector: Arc<dyn SessionConnector>,
) -> ApplicationService {
    ApplicationService::with_dependencies(
        path,
        connector,
        Arc::new(MissingEnvironment),
        Arc::new(SessionSecretStore::default()),
        ConfigWriter::default(),
    )
    .expect("service")
}

async fn seed_profiles(service: &ApplicationService, count: usize) -> Vec<ProfileSnapshot> {
    let mut snapshots = Vec::with_capacity(count);
    for index in 0..count {
        let id = format!("profile-{index}");
        let created = service
            .create_profile(create_request(&id, OperationId(10_000 + index as u64)))
            .await
            .expect("seed profile");
        let profile = service
            .profiles_snapshot()
            .await
            .into_iter()
            .find(|profile| profile.id == id)
            .expect("seed profile snapshot");
        snapshots.push(ProfileSnapshot::from_profile(
            &profile,
            created.profile_generation,
            false,
        ));
    }
    snapshots
}

fn submit_test(
    ui: &UiPort,
    profile: &ProfileSnapshot,
    operation_id: OperationId,
    timeout_ms: u64,
) -> Result<(), SubmitError> {
    ui.try_submit(UiCommand::TestConnection {
        operation_id,
        profile_id: profile.id.clone(),
        profile_generation: profile.generation,
        timeout_ms,
    })
}

async fn wait_for_event(ui: &mut UiPort, mut predicate: impl FnMut(&UiEvent) -> bool) -> UiEvent {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let event = ui
                .next_event()
                .await
                .expect("runtime event lane remains open");
            if predicate(&event) {
                return event;
            }
        }
    })
    .await
    .expect("matching runtime event")
}

fn terminal_is(event: &UiEvent, operation_id: OperationId, cancelled: bool) -> bool {
    matches!(
        event,
        UiEvent::OperationFailed {
            operation_id: actual,
            summary,
            ..
        } if *actual == operation_id
            && *summary
                == if cancelled {
                    dbotter::model::PublicSummary::OperationCancelled
                } else {
                    dbotter::model::PublicSummary::OperationTimedOut
                }
    )
}

async fn wait_until(timeout: Duration, predicate: impl Fn() -> bool) {
    tokio::time::timeout(timeout, async {
        while !predicate() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("condition reached");
}

async fn shutdown(ui: &UiPort, runtime: dbotter::ui::RuntimeHandle, operation_id: OperationId) {
    ui.try_submit(UiCommand::ShutdownRuntime { operation_id })
        .expect("shutdown watch");
    runtime.wait().await.expect("runtime joins");
}

fn create_request(id: &str, operation_id: OperationId) -> CreateProfileRequest {
    let mut draft = ConnectionDraft::for_driver(DriverKind::MySql);
    draft.name = id.to_owned();
    draft.credential_mode = CredentialMode::None;
    CreateProfileRequest {
        draft_id: DraftId(operation_id.0),
        operation_id,
        explicit_id: Some(ProfileId(id.to_owned())),
        draft,
        secret_update: SessionSecretUpdate::Clear,
        migration_consent: MigrationConsent::Cancelled,
    }
}

fn draft_for_test() -> ConnectionDraft {
    let mut draft = ConnectionDraft::for_driver(DriverKind::MySql);
    draft.name = "Draft controller".to_owned();
    draft.credential_mode = CredentialMode::None;
    draft
}

fn empty_driver_result() -> QueryResult {
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

fn empty_query_result() -> ResultSnapshot {
    let raw = empty_driver_result();
    ResultSnapshot::retain(
        raw,
        ResultProvenance {
            result_id: ResultId(1),
            profile_id: ProfileId("snapshot".to_owned()),
            profile_generation: ProfileGeneration(1),
            operation_id: OperationId(1),
            driver: DriverKind::MySql,
            completed_at_unix_ms: 0,
            duration_ms: 0,
        },
        ResultRetentionPolicy::mysql(500),
    )
}

fn snapshot(profile_id: ProfileId, generation: ProfileGeneration) -> ProfileSnapshot {
    let mut draft = ConnectionDraft::for_driver(DriverKind::MySql);
    draft.name = "Folded".to_owned();
    let profile = dbotter::model::ConnectionProfile::from_draft(profile_id.0.clone(), draft);
    ProfileSnapshot::from_profile(&profile, generation, false)
}

fn session_snapshot(
    profile_id: ProfileId,
    generation: ProfileGeneration,
    has_current_session_secret: bool,
) -> ProfileSnapshot {
    let mut draft = ConnectionDraft::for_driver(DriverKind::MySql);
    draft.name = "Session profile".to_owned();
    draft.credential_mode = CredentialMode::Session;
    let profile = dbotter::model::ConnectionProfile::from_draft(profile_id.0.clone(), draft);
    ProfileSnapshot::from_profile(&profile, generation, has_current_session_secret)
}
