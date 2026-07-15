use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::model::{
    DraftId, OperationId, ProfileGeneration, ProfileId, ResultId, SessionGeneration,
};

use super::runtime::{
    DraftPermitRegistry, ProfilePermitRegistry, RegisteredTask, TaskClass, TaskRegistry, TaskScope,
    join_registered_for_shutdown_with_grace,
};

struct DropFlag(Arc<AtomicBool>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
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
