use std::fs;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::config::{
    CURRENT_CONFIG_VERSION, Config, ConfigSourceVersion, ConfigWriter, migration_backup_path,
};
use crate::model::{
    ConnectionProfile, CredentialMode, DraftId, DriverKind, OperationId, ProfileGeneration,
    ProfileId, PublicSummary, RedisTlsConfig, ResultId, SessionGeneration, TlsMode,
};
use crate::secrets::{EnvironmentAvailability, SecretError, SessionSecret, SessionSecretStore};
use crate::service::{ApplicationService, DriverConnector, SecretResolver};

use super::runtime::{
    DraftPermitRegistry, PROCESS_EXPORT_LIMIT, PROCESS_EXPORT_PERMITS, ProfilePermitRegistry,
    RegisteredTask, TaskClass, TaskRegistry, TaskScope, await_pre_session_blocking,
    join_registered_for_shutdown_with_grace, snapshots,
};

struct DropFlag(Arc<AtomicBool>);

#[cfg(test)]
struct MutableEnvironment {
    availability: Mutex<EnvironmentAvailability>,
    probes: AtomicUsize,
    resolves: AtomicUsize,
}

#[cfg(test)]
impl MutableEnvironment {
    fn new(availability: EnvironmentAvailability) -> Self {
        Self {
            availability: Mutex::new(availability),
            probes: AtomicUsize::new(0),
            resolves: AtomicUsize::new(0),
        }
    }

    fn set(&self, availability: EnvironmentAvailability) {
        *self.availability.lock().expect("environment state") = availability;
    }
}

#[cfg(test)]
impl SecretResolver for MutableEnvironment {
    fn resolve_environment(&self, name: &str) -> Result<Arc<SessionSecret>, SecretError> {
        self.resolves.fetch_add(1, Ordering::SeqCst);
        match *self.availability.lock().expect("environment state") {
            EnvironmentAvailability::Available => Ok(Arc::new(SessionSecret::new(
                "ENVIRONMENT_VALUE_MUST_NOT_REACH_A_SNAPSHOT".to_owned(),
            ))),
            EnvironmentAvailability::Missing => Err(SecretError::MissingEnv(name.to_owned())),
            EnvironmentAvailability::Empty => Err(SecretError::EmptyEnv(name.to_owned())),
        }
    }

    fn probe_environment(&self, _name: &str) -> EnvironmentAvailability {
        self.probes.fetch_add(1, Ordering::SeqCst);
        *self.availability.lock().expect("environment state")
    }
}

#[cfg(test)]
fn environment_service(
    path: &std::path::Path,
    environment: Arc<MutableEnvironment>,
) -> ApplicationService {
    ApplicationService::with_dependencies(
        path,
        Arc::new(DriverConnector),
        environment,
        Arc::new(SessionSecretStore::default()),
        ConfigWriter::default(),
    )
    .expect("environment service")
}

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[cfg(test)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pre_session_blocking_cancel_and_timeout_return_before_running_worker_finishes() {
    for cancel_first in [true, false] {
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let worker_gate = Arc::clone(&gate);
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (finished_tx, mut finished_rx) = tokio::sync::oneshot::channel();
        let task = tokio::task::spawn_blocking(move || {
            let _ = started_tx.send(());
            let (lock, condition) = &*worker_gate;
            let mut released = lock.lock().expect("blocking gate lock");
            while !*released {
                released = condition.wait(released).expect("blocking gate wait");
            }
            let _ = finished_tx.send(());
        });
        started_rx.await.expect("blocking worker started");

        let cancel = CancellationToken::new();
        let (deadline, expected) = if cancel_first {
            cancel.cancel();
            (
                tokio::time::Instant::now() + Duration::from_secs(10),
                PublicSummary::OperationCancelled,
            )
        } else {
            (
                tokio::time::Instant::now() + Duration::from_millis(10),
                PublicSummary::OperationTimedOut,
            )
        };
        let result = tokio::time::timeout(
            Duration::from_millis(250),
            await_pre_session_blocking(task, &cancel, deadline),
        )
        .await
        .expect("pre-session terminal remains responsive");
        assert_eq!(result.expect_err("cancel or timeout"), expected);
        assert!(matches!(
            finished_rx.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));

        let (lock, condition) = &*gate;
        *lock.lock().expect("release gate lock") = true;
        condition.notify_all();
        tokio::time::timeout(Duration::from_secs(1), &mut finished_rx)
            .await
            .expect("detached idempotent worker finishes")
            .expect("blocking worker completion signal");
    }
}

#[cfg(test)]
#[tokio::test]
async fn registry_reaps_all_four_closed_scope_variants_without_hybrid_identity() {
    let scopes = [
        TaskScope::Profile {
            profile_id: ProfileId("registry".to_owned()),
            profile_generation: ProfileGeneration(4),
            session_generation: Some(SessionGeneration(8)),
        },
        TaskScope::Draft {
            draft_id: DraftId(5),
        },
        TaskScope::Export {
            result_id: ResultId(6),
        },
        TaskScope::Global,
    ];
    let mut registry = TaskRegistry::default();
    for (index, scope) in scopes.into_iter().enumerate() {
        registry
            .insert(
                RegisteredTask {
                    operation_id: OperationId(100 + index as u64),
                    scope,
                    cancel: CancellationToken::new(),
                    join: tokio::spawn(async {}),
                },
                if index == 2 {
                    TaskClass::Export
                } else {
                    TaskClass::AsyncNetwork
                },
            )
            .expect("unique scope registers");
    }
    tokio::task::yield_now().await;
    let reaped = registry.reap_finished().await;
    assert_eq!(reaped.len(), 4);
    assert!(registry.is_empty());
}

#[cfg(test)]
#[test]
fn process_export_pool_has_exactly_two_non_network_slots() {
    assert_eq!(PROCESS_EXPORT_LIMIT, 2);
    let first = Arc::clone(&PROCESS_EXPORT_PERMITS)
        .try_acquire_owned()
        .expect("first export slot");
    let second = Arc::clone(&PROCESS_EXPORT_PERMITS)
        .try_acquire_owned()
        .expect("second export slot");
    assert!(
        Arc::clone(&PROCESS_EXPORT_PERMITS)
            .try_acquire_owned()
            .is_err(),
        "a third process-wide export must be rejected without entering a worker"
    );
    drop(first);
    assert!(
        Arc::clone(&PROCESS_EXPORT_PERMITS)
            .try_acquire_owned()
            .is_ok(),
        "an export slot must return after its worker exits"
    );
    drop(second);
}

#[cfg(test)]
#[tokio::test]
async fn shutdown_aborts_only_async_after_grace_and_joins_mutation_and_export() {
    let async_dropped = Arc::new(AtomicBool::new(false));
    let mutation_released = Arc::new(tokio::sync::Notify::new());
    let export_released = Arc::new(tokio::sync::Notify::new());

    let async_task = RegisteredTask {
        operation_id: OperationId(1),
        scope: TaskScope::Global,
        cancel: CancellationToken::new(),
        join: tokio::spawn({
            let async_dropped = async_dropped.clone();
            async move {
                let _drop = DropFlag(async_dropped);
                std::future::pending::<()>().await;
            }
        }),
    };
    let mutation_task = RegisteredTask {
        operation_id: OperationId(2),
        scope: TaskScope::Global,
        cancel: CancellationToken::new(),
        join: tokio::spawn({
            let mutation_released = mutation_released.clone();
            async move { mutation_released.notified().await }
        }),
    };
    let export_task = RegisteredTask {
        operation_id: OperationId(3),
        scope: TaskScope::Export {
            result_id: ResultId(3),
        },
        cancel: CancellationToken::new(),
        join: tokio::spawn({
            let export_released = export_released.clone();
            async move { export_released.notified().await }
        }),
    };

    let shutdown = tokio::spawn(join_registered_for_shutdown_with_grace(
        vec![
            (async_task, TaskClass::AsyncNetwork),
            (mutation_task, TaskClass::Mutation),
            (export_task, TaskClass::Export),
        ],
        Duration::from_millis(20),
    ));
    export_released.notify_one();
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert!(async_dropped.load(Ordering::SeqCst));
    assert!(
        !shutdown.is_finished(),
        "mutation/export must not be detached"
    );

    mutation_released.notify_one();
    let report = shutdown.await.expect("shutdown helper task joins");
    assert_eq!(report.async_aborted, 1);
    assert_eq!(report.mutations_joined, 1);
    assert_eq!(report.exports_joined, 1);
}

#[cfg(test)]
#[tokio::test]
async fn shutdown_waits_for_blocked_export_cleanup_beyond_async_grace() {
    let export_dropped = Arc::new(AtomicBool::new(false));
    let export_released = Arc::new(tokio::sync::Notify::new());
    let export_task = RegisteredTask {
        operation_id: OperationId(4),
        scope: TaskScope::Export {
            result_id: ResultId(4),
        },
        cancel: CancellationToken::new(),
        join: tokio::spawn({
            let export_dropped = export_dropped.clone();
            let export_released = export_released.clone();
            async move {
                let _temp_cleanup = DropFlag(export_dropped);
                export_released.notified().await;
            }
        }),
    };
    let mut shutdown = tokio::spawn(join_registered_for_shutdown_with_grace(
        vec![(export_task, TaskClass::Export)],
        Duration::from_millis(20),
    ));

    tokio::time::sleep(Duration::from_millis(80)).await;
    assert!(
        !shutdown.is_finished(),
        "cooperative export must keep runtime shutdown pending beyond async grace"
    );
    assert!(
        !export_dropped.load(Ordering::SeqCst),
        "export temp cleanup must remain owned by the live worker"
    );

    export_released.notify_waiters();
    let report = tokio::time::timeout(Duration::from_millis(80), &mut shutdown)
        .await
        .expect("shutdown completes after cooperative export returns")
        .expect("shutdown helper task joins");
    assert_eq!(report.exports_joined, 1);
    assert!(export_dropped.load(Ordering::SeqCst));
}

#[cfg(test)]
#[test]
fn profile_slot_is_keyed_only_by_profile_id_and_prunes_after_permit_drop() {
    let profile_id = ProfileId("one-profile-across-generations".to_owned());
    let mut permits = ProfilePermitRegistry::default();
    let first = permits
        .try_acquire(&profile_id)
        .expect("first generation owns the profile slot");
    assert!(
        permits.try_acquire(&profile_id).is_err(),
        "a newer generation cannot run concurrently for the same profile id"
    );
    assert_eq!(permits.tracked_profiles(), 1);
    drop(first);
    permits.prune_idle();
    assert_eq!(permits.tracked_profiles(), 0);
    assert!(permits.try_acquire(&profile_id).is_ok());
}

#[cfg(test)]
#[test]
fn draft_slot_is_keyed_by_draft_id_and_prunes_after_permit_drop() {
    let draft_id = DraftId(88);
    let mut permits = DraftPermitRegistry::default();
    let first = permits
        .try_acquire(draft_id)
        .expect("first operation owns the draft slot");
    assert!(permits.try_acquire(draft_id).is_err());
    assert_eq!(permits.tracked_drafts(), 1);
    drop(first);
    permits.prune_idle();
    assert_eq!(permits.tracked_drafts(), 0);
}

#[cfg(test)]
#[test]
fn duplicate_operation_id_is_rejected_by_reservation_before_spawn() {
    let operation_id = OperationId(9_999);
    let mut registry = TaskRegistry::default();
    let reservation = registry
        .reserve(operation_id)
        .expect("first command reserves before spawn");
    let mut spawned = 0_u8;
    if registry.reserve(operation_id).is_ok() {
        spawned = spawned.saturating_add(1);
    }
    assert_eq!(spawned, 0, "duplicate must not reach its spawn factory");
    registry.release_reservation(reservation);
    assert!(registry.reserve(operation_id).is_ok());
}

#[cfg(test)]
#[tokio::test]
async fn saved_environment_availability_refreshes_on_reload_and_restart_without_resolving_value() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    let profile = ConnectionProfile {
        id: "saved-environment".to_owned(),
        name: "Saved Environment".to_owned(),
        driver: DriverKind::MySql,
        host: "127.0.0.1".to_owned(),
        port: 3306,
        database: None,
        username: None,
        tls: TlsMode::Disabled,
        credential_mode: CredentialMode::Environment,
        secret_env: Some("DBOTTER_SAVED_ENV".to_owned()),
        redis_tls: RedisTlsConfig::default(),
    };
    let config = Config {
        version: CURRENT_CONFIG_VERSION,
        profiles: vec![profile],
    };
    fs::write(&path, toml::to_string(&config).expect("serialize config")).expect("write config");

    let environment = Arc::new(MutableEnvironment::new(EnvironmentAvailability::Missing));
    let process_a = environment_service(&path, environment.clone());
    let (missing, config) = snapshots(&process_a).await.expect("missing snapshot");
    assert_eq!(config.source_version(), ConfigSourceVersion::V2);
    assert_eq!(
        missing[0].environment_availability,
        Some(EnvironmentAvailability::Missing)
    );

    environment.set(EnvironmentAvailability::Available);
    process_a
        .reload_configuration()
        .await
        .expect("reload process A");
    let (available, _) = snapshots(&process_a).await.expect("available snapshot");
    assert_eq!(
        available[0].environment_availability,
        Some(EnvironmentAvailability::Available)
    );

    environment.set(EnvironmentAvailability::Empty);
    let process_b = environment_service(&path, environment.clone());
    let (empty, _) = snapshots(&process_b).await.expect("restart snapshot");
    assert_eq!(
        empty[0].environment_availability,
        Some(EnvironmentAvailability::Empty)
    );
    assert_eq!(environment.probes.load(Ordering::SeqCst), 3);
    assert_eq!(
        environment.resolves.load(Ordering::SeqCst),
        0,
        "profile snapshots probe availability without resolving an environment value"
    );
}

#[cfg(test)]
#[tokio::test]
async fn empty_v1_snapshot_reports_the_exact_fixed_backup_without_debug_disclosure() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("v1-config.toml");
    fs::write(&path, "version = 1\n").expect("write v1 config");
    let environment = Arc::new(MutableEnvironment::new(EnvironmentAvailability::Missing));
    let service = environment_service(&path, environment);

    let (profiles, config) = snapshots(&service).await.expect("v1 snapshot");
    assert!(profiles.is_empty());
    assert_eq!(config.source_version(), ConfigSourceVersion::V1);
    assert!(config.migration_required());
    assert_eq!(
        config.migration_backup(),
        Some(migration_backup_path(&path).as_path())
    );
    let debug = format!("{config:?}");
    assert!(debug.contains("<redacted>"));
    assert!(!debug.contains(path.to_string_lossy().as_ref()));
}
