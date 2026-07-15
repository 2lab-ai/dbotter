//! Bounded background controller for profile-scoped database operations.

use std::collections::{HashMap, HashSet};
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};
use tokio::task::{JoinError, JoinHandle};
use tokio_util::sync::CancellationToken;

use futures_util::FutureExt as _;

use crate::config::CommitState;
use crate::model::{
    DraftId, OperationId, OperationKind, ProfileGeneration, ProfileId, PublicCode, PublicSummary,
    RedisKeyInspectRequest, RedisScanRequest, ResultId, SessionGeneration,
};
use crate::public_error::{PublicOperationError, SafeContext};
use crate::service::{
    ApplicationService, RuntimeCreateOutcome, RuntimeDeleteOutcome, RuntimeMutationFailure,
    RuntimeReloadOutcome, RuntimeUpdateOutcome, ServiceError, SessionDisposition, TestDraftRequest,
};

use super::adapter::{ControlKey, ServicePort, UiCommand};
use super::model::{ConnectionFailureOutcome, PostCloseState, ProfileSnapshot, UiEvent};

const GLOBAL_NETWORK_LIMIT: usize = 4;
const SHUTDOWN_ASYNC_GRACE: Duration = Duration::from_secs(2);

#[derive(Default)]
pub(super) struct ProfilePermitRegistry {
    permits: HashMap<ProfileId, Arc<Semaphore>>,
}

#[derive(Default)]
pub(super) struct DraftPermitRegistry {
    permits: HashMap<DraftId, Arc<Semaphore>>,
}

impl DraftPermitRegistry {
    pub(super) fn try_acquire(
        &mut self,
        draft_id: DraftId,
    ) -> Result<tokio::sync::OwnedSemaphorePermit, tokio::sync::TryAcquireError> {
        self.permits
            .entry(draft_id)
            .or_insert_with(|| Arc::new(Semaphore::new(1)))
            .clone()
            .try_acquire_owned()
    }

    pub(super) fn prune_idle(&mut self) {
        self.permits
            .retain(|_, permit| Arc::strong_count(permit) > 1 || permit.available_permits() == 0);
    }

    #[cfg(test)]
    pub(super) fn tracked_drafts(&self) -> usize {
        self.permits.len()
    }
}

impl ProfilePermitRegistry {
    fn slot(&mut self, profile_id: &ProfileId) -> Arc<Semaphore> {
        self.permits
            .entry(profile_id.clone())
            .or_insert_with(|| Arc::new(Semaphore::new(1)))
            .clone()
    }

    pub(super) fn try_acquire(
        &mut self,
        profile_id: &ProfileId,
    ) -> Result<tokio::sync::OwnedSemaphorePermit, tokio::sync::TryAcquireError> {
        self.slot(profile_id).try_acquire_owned()
    }

    pub(super) fn prune_idle(&mut self) {
        self.permits
            .retain(|_, permit| Arc::strong_count(permit) > 1 || permit.available_permits() == 0);
    }

    #[cfg(test)]
    pub(super) fn tracked_profiles(&self) -> usize {
        self.permits.len()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TaskScope {
    Profile {
        profile_id: ProfileId,
        profile_generation: ProfileGeneration,
        session_generation: Option<SessionGeneration>,
    },
    Draft {
        draft_id: DraftId,
    },
    Export {
        result_id: ResultId,
    },
    Global,
}

#[derive(Debug)]
pub struct RegisteredTask {
    pub operation_id: OperationId,
    pub scope: TaskScope,
    pub cancel: CancellationToken,
    pub join: JoinHandle<()>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TaskClass {
    AsyncNetwork,
    Mutation,
    #[allow(dead_code)]
    Export,
}

#[derive(Clone)]
enum FailureContext {
    Profile {
        profile_id: ProfileId,
        profile_generation: ProfileGeneration,
        kind: OperationKind,
    },
    Draft {
        draft_id: DraftId,
    },
    Profiles,
}

struct RegistryEntry {
    task: RegisteredTask,
    class: TaskClass,
    failure: FailureContext,
    completion_sent: Arc<AtomicBool>,
    terminal: CancellationToken,
    cleanup_started: bool,
}

pub(super) struct ReapedTask {
    operation_id: OperationId,
    scope: TaskScope,
    class: TaskClass,
    failure: FailureContext,
    join_error: Option<JoinError>,
    terminal: CancellationToken,
    cleanup_started: bool,
}

#[derive(Default)]
pub(super) struct TaskRegistry {
    entries: HashMap<OperationId, RegistryEntry>,
    reserved: HashSet<OperationId>,
}

#[derive(Debug)]
pub(super) struct TaskReservation(OperationId);

impl TaskRegistry {
    pub(super) fn reserve(&mut self, operation_id: OperationId) -> Result<TaskReservation, ()> {
        if self.entries.contains_key(&operation_id) || !self.reserved.insert(operation_id) {
            Err(())
        } else {
            Ok(TaskReservation(operation_id))
        }
    }

    pub(super) fn release_reservation(&mut self, reservation: TaskReservation) {
        self.reserved.remove(&reservation.0);
    }

    fn commit_reservation(
        &mut self,
        reservation: TaskReservation,
        task: RegisteredTask,
        class: TaskClass,
        failure: FailureContext,
        completion_sent: Arc<AtomicBool>,
    ) -> Result<(), RegisteredTask> {
        if reservation.0 != task.operation_id || !self.reserved.remove(&reservation.0) {
            return Err(task);
        }
        self.insert_with_metadata(task, class, failure, completion_sent)
    }

    #[allow(clippy::too_many_arguments)]
    fn commit_reservation_with_state(
        &mut self,
        reservation: TaskReservation,
        task: RegisteredTask,
        class: TaskClass,
        failure: FailureContext,
        completion_sent: Arc<AtomicBool>,
        terminal: CancellationToken,
        cleanup_started: bool,
    ) -> Result<(), RegisteredTask> {
        if reservation.0 != task.operation_id || !self.reserved.remove(&reservation.0) {
            return Err(task);
        }
        self.insert_with_state(
            task,
            class,
            failure,
            completion_sent,
            terminal,
            cleanup_started,
        )
    }

    #[cfg(test)]
    pub(super) fn insert(
        &mut self,
        task: RegisteredTask,
        class: TaskClass,
    ) -> Result<(), RegisteredTask> {
        let failure = match &task.scope {
            TaskScope::Profile {
                profile_id,
                profile_generation,
                ..
            } => FailureContext::Profile {
                profile_id: profile_id.clone(),
                profile_generation: *profile_generation,
                kind: OperationKind::ConnectProfile,
            },
            TaskScope::Draft { draft_id } => FailureContext::Draft {
                draft_id: *draft_id,
            },
            TaskScope::Export { .. } | TaskScope::Global => FailureContext::Profiles,
        };
        self.insert_with_metadata(task, class, failure, Arc::new(AtomicBool::new(false)))
    }

    fn insert_with_metadata(
        &mut self,
        task: RegisteredTask,
        class: TaskClass,
        failure: FailureContext,
        completion_sent: Arc<AtomicBool>,
    ) -> Result<(), RegisteredTask> {
        self.insert_with_state(
            task,
            class,
            failure,
            completion_sent,
            CancellationToken::new(),
            false,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_with_state(
        &mut self,
        task: RegisteredTask,
        class: TaskClass,
        failure: FailureContext,
        completion_sent: Arc<AtomicBool>,
        terminal: CancellationToken,
        cleanup_started: bool,
    ) -> Result<(), RegisteredTask> {
        if self.entries.contains_key(&task.operation_id) {
            return Err(task);
        }
        self.entries.insert(
            task.operation_id,
            RegistryEntry {
                task,
                class,
                failure,
                completion_sent,
                terminal,
                cleanup_started,
            },
        );
        Ok(())
    }

    fn update_session(&mut self, operation_id: OperationId, session_generation: SessionGeneration) {
        if let Some(entry) = self.entries.get_mut(&operation_id)
            && let TaskScope::Profile {
                session_generation: registered,
                ..
            } = &mut entry.task.scope
        {
            *registered = Some(session_generation);
        }
    }

    fn cancel(&self, operation_id: OperationId) -> bool {
        if let Some(entry) = self.entries.get(&operation_id) {
            entry.task.cancel.cancel();
            true
        } else {
            false
        }
    }

    fn take(&mut self, operation_id: OperationId) -> Option<RegistryEntry> {
        self.entries.remove(&operation_id)
    }

    fn has_profile_network(
        &self,
        profile_id: &ProfileId,
        profile_generation: ProfileGeneration,
    ) -> bool {
        self.entries.values().any(|entry| {
            entry.class == TaskClass::AsyncNetwork
                && matches!(
                    &entry.task.scope,
                    TaskScope::Profile {
                        profile_id: registered_id,
                        profile_generation: registered_generation,
                        ..
                    } if registered_id == profile_id
                        && *registered_generation == profile_generation
                )
        })
    }

    fn has_profile_network_for_id(&self, profile_id: &ProfileId) -> bool {
        self.entries.values().any(|entry| {
            entry.class == TaskClass::AsyncNetwork
                && matches!(
                    &entry.task.scope,
                    TaskScope::Profile {
                        profile_id: registered_id,
                        ..
                    } if registered_id == profile_id
                )
        })
    }

    fn cancel_profile_network_waiters(
        &self,
        profile_id: &ProfileId,
        profile_generation: ProfileGeneration,
    ) -> Vec<CancellationToken> {
        self.entries
            .values()
            .filter(|entry| {
                entry.class == TaskClass::AsyncNetwork
                    && matches!(
                        &entry.task.scope,
                        TaskScope::Profile {
                            profile_id: registered_id,
                            profile_generation: registered_generation,
                            ..
                        } if registered_id == profile_id
                            && *registered_generation == profile_generation
                    )
            })
            .map(|entry| {
                entry.task.cancel.cancel();
                entry.terminal.clone()
            })
            .collect()
    }

    fn cancel_all_network_waiters(&self) -> Vec<CancellationToken> {
        self.entries
            .values()
            .filter(|entry| entry.class == TaskClass::AsyncNetwork)
            .map(|entry| {
                entry.task.cancel.cancel();
                entry.terminal.clone()
            })
            .collect()
    }

    fn cancel_all(&self) {
        for entry in self.entries.values() {
            entry.task.cancel.cancel();
        }
    }

    fn has_bounded_shutdown_work(&self) -> bool {
        self.entries
            .values()
            .any(|entry| entry.class == TaskClass::AsyncNetwork)
    }

    fn take_all_bounded_shutdown_entries(&mut self) -> Vec<RegistryEntry> {
        let operation_ids = self
            .entries
            .iter()
            .filter_map(|(operation_id, entry)| {
                (entry.class == TaskClass::AsyncNetwork).then_some(*operation_id)
            })
            .collect::<Vec<_>>();
        operation_ids
            .into_iter()
            .filter_map(|operation_id| self.entries.remove(&operation_id))
            .collect()
    }

    fn is_empty_runtime(&self) -> bool {
        self.entries.is_empty()
    }

    pub(super) async fn reap_finished(&mut self) -> Vec<ReapedTask> {
        let operation_ids = self
            .entries
            .iter()
            .filter_map(|(operation_id, entry)| {
                (entry.task.join.is_finished() && !entry.completion_sent.load(Ordering::Acquire))
                    .then_some(*operation_id)
            })
            .collect::<Vec<_>>();
        let mut reaped = Vec::with_capacity(operation_ids.len());
        for operation_id in operation_ids {
            if let Some(entry) = self.entries.remove(&operation_id) {
                let RegistryEntry {
                    task,
                    class,
                    failure,
                    terminal,
                    cleanup_started,
                    ..
                } = entry;
                let join_error = task.join.await.err();
                reaped.push(ReapedTask {
                    operation_id: task.operation_id,
                    scope: task.scope,
                    class,
                    failure,
                    join_error,
                    terminal,
                    cleanup_started,
                });
            }
        }
        reaped
    }

    #[cfg(test)]
    pub(super) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct ShutdownJoinReport {
    pub(super) async_aborted: usize,
    pub(super) mutations_joined: usize,
    pub(super) exports_joined: usize,
}

#[cfg(test)]
pub(super) async fn join_registered_for_shutdown_with_grace(
    tasks: Vec<(RegisteredTask, TaskClass)>,
    grace: Duration,
) -> ShutdownJoinReport {
    for (task, _) in &tasks {
        task.cancel.cancel();
    }

    let mut bounded_tasks = Vec::new();
    let mut durable_tasks = Vec::new();
    for task in tasks {
        if task.1 == TaskClass::AsyncNetwork {
            bounded_tasks.push(task);
        } else {
            durable_tasks.push(task);
        }
    }
    let deadline = tokio::time::Instant::now() + grace;
    let mut report = ShutdownJoinReport::default();
    for (mut task, class) in bounded_tasks {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, &mut task.join).await {
            Ok(_) => {}
            Err(_) => {
                task.join.abort();
                let _ = task.join.await;
                if class == TaskClass::AsyncNetwork {
                    report.async_aborted = report.async_aborted.saturating_add(1);
                }
            }
        }
    }
    for (task, class) in durable_tasks {
        match class {
            TaskClass::AsyncNetwork => {}
            TaskClass::Mutation => {
                let _ = task.join.await;
                report.mutations_joined = report.mutations_joined.saturating_add(1);
            }
            TaskClass::Export => {
                let _ = task.join.await;
                report.exports_joined = report.exports_joined.saturating_add(1);
            }
        }
    }
    report
}

pub struct RuntimeHandle {
    join: JoinHandle<()>,
}

impl RuntimeHandle {
    pub async fn wait(self) -> Result<(), JoinError> {
        self.join.await
    }
}

pub fn spawn(service_port: ServicePort, config_path: std::path::PathBuf) -> RuntimeHandle {
    RuntimeHandle {
        join: tokio::spawn(async move {
            match ApplicationService::load_path(config_path) {
                Ok(application) => run_controller(service_port, application).await,
                Err(error) => run_unavailable(service_port, error.public_summary()).await,
            }
        }),
    }
}

pub fn spawn_with_service(
    service_port: ServicePort,
    application: ApplicationService,
) -> RuntimeHandle {
    RuntimeHandle {
        join: tokio::spawn(run_controller(service_port, application)),
    }
}

enum ControllerMessage {
    SessionAcquired {
        operation_id: OperationId,
        session_generation: SessionGeneration,
    },
    Completed {
        operation_id: OperationId,
        output: Box<TaskOutput>,
    },
}

enum TaskOutput {
    Event(Box<UiEvent>),
    Reload {
        operation_id: OperationId,
        result: Box<Result<(RuntimeReloadOutcome, Vec<ProfileSnapshot>), ServiceError>>,
    },
    Create {
        operation_id: OperationId,
        fallback_profile_id: ProfileId,
        result: Box<Result<RuntimeCreateOutcome, RuntimeMutationFailure>>,
    },
    Update {
        operation_id: OperationId,
        profile_id: ProfileId,
        previous_generation: ProfileGeneration,
        result: Box<Result<RuntimeUpdateOutcome, RuntimeMutationFailure>>,
    },
    Delete {
        operation_id: OperationId,
        profile_id: ProfileId,
        previous_generation: ProfileGeneration,
        result: Box<Result<RuntimeDeleteOutcome, RuntimeMutationFailure>>,
    },
}

enum ProfileWork {
    Connect {
        operation_id: OperationId,
        profile_id: ProfileId,
        profile_generation: ProfileGeneration,
        timeout: Duration,
        kind: OperationKind,
    },
    Execute {
        request: crate::model::ExecuteRequest,
        kind: OperationKind,
    },
    BrowseCatalog {
        request: crate::model::CatalogRequest,
    },
    ScanRedisKeys {
        request: crate::model::RedisScanRequest,
    },
    InspectRedisKey {
        request: crate::model::RedisKeyInspectRequest,
    },
}

enum ProfileControlWork {
    Disconnect {
        operation_id: OperationId,
        profile_id: ProfileId,
        profile_generation: ProfileGeneration,
        session_generation: Option<SessionGeneration>,
    },
    Reconnect {
        operation_id: OperationId,
        profile_id: ProfileId,
        profile_generation: ProfileGeneration,
        session_generation: Option<SessionGeneration>,
        timeout: Duration,
    },
}

impl ProfileControlWork {
    fn identity(
        &self,
    ) -> (
        OperationId,
        &ProfileId,
        ProfileGeneration,
        Option<SessionGeneration>,
        OperationKind,
    ) {
        match self {
            Self::Disconnect {
                operation_id,
                profile_id,
                profile_generation,
                session_generation,
            } => (
                *operation_id,
                profile_id,
                *profile_generation,
                *session_generation,
                OperationKind::DisconnectProfile,
            ),
            Self::Reconnect {
                operation_id,
                profile_id,
                profile_generation,
                session_generation,
                ..
            } => (
                *operation_id,
                profile_id,
                *profile_generation,
                *session_generation,
                OperationKind::ReconnectProfile,
            ),
        }
    }
}

enum ReconnectPermitPlan {
    Ready {
        profile: OwnedSemaphorePermit,
        global: OwnedSemaphorePermit,
    },
    Wait {
        profile: Arc<Semaphore>,
        global: Arc<Semaphore>,
    },
}

impl ProfileWork {
    fn identity(&self) -> (OperationId, &ProfileId, ProfileGeneration, OperationKind) {
        match self {
            Self::Connect {
                operation_id,
                profile_id,
                profile_generation,
                kind,
                ..
            } => (*operation_id, profile_id, *profile_generation, *kind),
            Self::Execute { request, kind } => (
                request.operation_id,
                &request.profile_id,
                request.profile_generation,
                *kind,
            ),
            Self::BrowseCatalog { request } => (
                request.operation_id(),
                request.profile_id(),
                request.profile_generation(),
                OperationKind::BrowseMySql,
            ),
            Self::ScanRedisKeys { request } => (
                request.operation_id(),
                request.profile_id(),
                request.profile_generation(),
                OperationKind::BrowseRedis,
            ),
            Self::InspectRedisKey { request } => (
                request.operation_id(),
                request.profile_id(),
                request.profile_generation(),
                OperationKind::InspectRedis,
            ),
        }
    }
}

async fn run_controller(mut port: ServicePort, application: ApplicationService) {
    let (message_tx, mut message_rx) = mpsc::unbounded_channel();
    let global_permits = Arc::new(Semaphore::new(GLOBAL_NETWORK_LIMIT));
    let mut profile_permits = ProfilePermitRegistry::default();
    let mut draft_permits = DraftPermitRegistry::default();
    let mut registry = TaskRegistry::default();
    let mut mutation_active = false;
    let mut reap_tick = tokio::time::interval(Duration::from_millis(5));
    let shutdown_operation = loop {
        if let Some(operation_id) = *port.shutdown_rx.borrow() {
            break operation_id;
        }
        tokio::select! {
            biased;
            changed = port.shutdown_rx.changed() => {
                if changed.is_err() {
                    break OperationId(0);
                }
            }
            Some(message) = message_rx.recv() => {
                handle_controller_message(
                    message,
                    &port,
                    &application,
                    &message_tx,
                    &mut registry,
                    &mut mutation_active,
                ).await;
            }
            Some(command) = port.control_rx.recv() => {
                handle_control(
                    command,
                    &port,
                    &application,
                    &message_tx,
                    &global_permits,
                    &mut profile_permits,
                    &mut registry,
                ).await;
            }
            Some(command) = port.mutation_rx.recv(), if !mutation_active => {
                mutation_active = start_mutation(
                    command,
                    &port,
                    &application,
                    &message_tx,
                    &mut registry,
                );
            }
            Some(command) = port.work_rx.recv() => {
                handle_work(
                    command,
                    &port,
                    &application,
                    &message_tx,
                    &global_permits,
                    &mut profile_permits,
                    &mut draft_permits,
                    &mut registry,
                );
            }
            _ = reap_tick.tick() => {
                reap_panicked_tasks(
                    &port,
                    &application,
                    &message_tx,
                    &mut registry,
                    &mut mutation_active,
                ).await;
                profile_permits.prune_idle();
                draft_permits.prune_idle();
            }
        }
    };

    port.close_and_drain();
    finish_controller_shutdown(
        &port,
        &application,
        &message_tx,
        &mut message_rx,
        &mut registry,
        &mut mutation_active,
    )
    .await;
    application.shutdown_runtime().await;
    let _ = port.try_emit(UiEvent::RuntimeShutdown {
        operation_id: shutdown_operation,
    });
}

async fn finish_controller_shutdown(
    port: &ServicePort,
    application: &ApplicationService,
    message_tx: &mpsc::UnboundedSender<ControllerMessage>,
    message_rx: &mut mpsc::UnboundedReceiver<ControllerMessage>,
    registry: &mut TaskRegistry,
    mutation_active: &mut bool,
) {
    registry.cancel_all();
    let deadline = tokio::time::Instant::now() + SHUTDOWN_ASYNC_GRACE;
    let mut reap_tick = tokio::time::interval(Duration::from_millis(5));
    while !registry.is_empty_runtime() {
        if registry.has_bounded_shutdown_work() && tokio::time::Instant::now() >= deadline {
            let mut cleanup_ready = Vec::new();
            for entry in registry.take_all_bounded_shutdown_entries() {
                let RegistryEntry {
                    task,
                    failure,
                    terminal,
                    cleanup_started,
                    ..
                } = entry;
                let operation_id = task.operation_id;
                let scope = task.scope;
                task.join.abort();
                let _ = task.join.await;
                if cleanup_started {
                    emit_internal_failure(port, operation_id, failure);
                    terminal.cancel();
                    continue;
                }
                let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
                if start_registered_failure_cleanup_with_ready(
                    operation_id,
                    scope,
                    failure.clone(),
                    terminal.clone(),
                    application,
                    message_tx,
                    registry,
                    Some(ready_tx),
                ) {
                    cleanup_ready.push(ready_rx);
                } else {
                    emit_internal_failure(port, operation_id, failure);
                    terminal.cancel();
                }
            }
            if !cleanup_ready.is_empty() {
                let ready = async move {
                    for ready in cleanup_ready {
                        let _ = ready.await;
                    }
                };
                let _ = tokio::time::timeout(Duration::from_millis(100), ready).await;
            }
            continue;
        }
        tokio::select! {
            biased;
            Some(message) = message_rx.recv() => {
                handle_controller_message(
                    message,
                    port,
                    application,
                    message_tx,
                    registry,
                    mutation_active,
                ).await;
            }
            _ = tokio::time::sleep_until(deadline), if registry.has_bounded_shutdown_work() => {}
            _ = reap_tick.tick() => {
                reap_panicked_tasks(
                    port,
                    application,
                    message_tx,
                    registry,
                    mutation_active,
                ).await;
            }
        }
    }
}

async fn run_unavailable(mut port: ServicePort, summary: PublicSummary) {
    let operation_id = loop {
        if let Some(operation_id) = *port.shutdown_rx.borrow() {
            break operation_id;
        }
        tokio::select! {
            biased;
            changed = port.shutdown_rx.changed() => {
                if changed.is_err() {
                    break OperationId(0);
                }
            }
            Some(command) = port.control_rx.recv() => {
                let _ = port.try_emit(failure_for_unavailable(command, summary));
            }
            Some(command) = port.mutation_rx.recv() => {
                let _ = port.try_emit(failure_for_unavailable(command, summary));
            }
            Some(command) = port.work_rx.recv() => {
                let _ = port.try_emit(failure_for_unavailable(command, summary));
            }
        }
    };
    port.close_and_drain();
    let _ = port.try_emit(UiEvent::RuntimeShutdown { operation_id });
}

#[allow(clippy::too_many_arguments)]
fn handle_work(
    command: UiCommand,
    port: &ServicePort,
    application: &ApplicationService,
    message_tx: &mpsc::UnboundedSender<ControllerMessage>,
    global_permits: &Arc<Semaphore>,
    profile_permits: &mut ProfilePermitRegistry,
    draft_permits: &mut DraftPermitRegistry,
    registry: &mut TaskRegistry,
) {
    if application.is_config_uncertain() {
        let _ = port.try_emit(UiEvent::ConfigUncertain {
            operation_id: command.operation_id(),
        });
        return;
    }
    let work = match command {
        UiCommand::TestDraftConnection(request) => {
            start_draft_work(
                request,
                port,
                application,
                message_tx,
                global_permits,
                draft_permits,
                registry,
            );
            return;
        }
        UiCommand::TestConnection {
            operation_id,
            profile_id,
            profile_generation,
            timeout_ms,
        } => ProfileWork::Connect {
            operation_id,
            profile_id,
            profile_generation,
            timeout: duration_from_millis(timeout_ms),
            kind: OperationKind::ConnectProfile,
        },
        UiCommand::Execute {
            operation_id,
            profile_id,
            profile_generation,
            language,
            text,
            row_limit,
            timeout_ms,
        } => ProfileWork::Execute {
            request: crate::model::ExecuteRequest {
                operation_id,
                profile_id,
                profile_generation,
                language,
                text,
                row_limit,
                timeout: duration_from_millis(timeout_ms),
            },
            kind: OperationKind::ExecuteMutation,
        },
        UiCommand::BrowseCatalog(request) => ProfileWork::BrowseCatalog { request },
        UiCommand::ScanRedisKeys(request) => ProfileWork::ScanRedisKeys { request },
        UiCommand::InspectRedisKey(request) => ProfileWork::InspectRedisKey { request },
        other => {
            let _ = port.try_emit(failure_for_unavailable(
                other,
                PublicSummary::InternalFailure,
            ));
            return;
        }
    };
    start_profile_work(
        work,
        port,
        application,
        message_tx,
        global_permits,
        profile_permits,
        registry,
    );
}

#[allow(clippy::too_many_arguments)]
fn start_draft_work(
    request: TestDraftRequest,
    port: &ServicePort,
    application: &ApplicationService,
    message_tx: &mpsc::UnboundedSender<ControllerMessage>,
    global_permits: &Arc<Semaphore>,
    draft_permits: &mut DraftPermitRegistry,
    registry: &mut TaskRegistry,
) {
    let operation_id = request.operation_id();
    let draft_id = request.draft_id();
    let reservation = match registry.reserve(operation_id) {
        Ok(reservation) => reservation,
        Err(()) => {
            let _ = port.try_emit(UiEvent::DraftOperationFailed {
                operation_id,
                draft_id,
                summary: PublicSummary::ResourceBusy,
            });
            return;
        }
    };
    let Ok(draft_permit) = draft_permits.try_acquire(draft_id) else {
        registry.release_reservation(reservation);
        let _ = port.try_emit(UiEvent::DraftOperationFailed {
            operation_id,
            draft_id,
            summary: PublicSummary::ResourceBusy,
        });
        return;
    };
    let Ok(global_permit) = global_permits.clone().try_acquire_owned() else {
        drop(draft_permit);
        registry.release_reservation(reservation);
        let _ = port.try_emit(UiEvent::DraftOperationFailed {
            operation_id,
            draft_id,
            summary: PublicSummary::ResourceBusy,
        });
        return;
    };

    let cancel = CancellationToken::new();
    let task_cancel = cancel.clone();
    let service = application.clone();
    let messages = message_tx.clone();
    let completion_sent = Arc::new(AtomicBool::new(false));
    let task_completion_sent = completion_sent.clone();
    let (start_tx, start_rx) = tokio::sync::oneshot::channel();
    let join = tokio::spawn(async move {
        if start_rx.await.is_err() {
            return;
        }
        let event = run_draft_work(&service, request, &task_cancel).await;
        drop(draft_permit);
        drop(global_permit);
        task_completion_sent.store(true, Ordering::Release);
        let _ = messages.send(ControllerMessage::Completed {
            operation_id,
            output: Box::new(TaskOutput::Event(Box::new(event))),
        });
    });
    let task = RegisteredTask {
        operation_id,
        scope: TaskScope::Draft { draft_id },
        cancel,
        join,
    };
    match registry.commit_reservation(
        reservation,
        task,
        TaskClass::AsyncNetwork,
        FailureContext::Draft { draft_id },
        completion_sent,
    ) {
        Ok(()) => {
            let _ = start_tx.send(());
        }
        Err(task) => {
            drop(start_tx);
            task.join.abort();
            let _ = port.try_emit(UiEvent::DraftOperationFailed {
                operation_id,
                draft_id,
                summary: PublicSummary::ResourceBusy,
            });
        }
    }
}

async fn run_draft_work(
    service: &ApplicationService,
    request: TestDraftRequest,
    cancel: &CancellationToken,
) -> UiEvent {
    let operation_id = request.operation_id();
    let draft_id = request.draft_id();
    let deadline = tokio::time::Instant::now() + request.timeout();
    let acquire = service.acquire_draft_session(request);
    tokio::pin!(acquire);
    let lease = tokio::select! {
        biased;
        () = cancel.cancelled() => {
            return UiEvent::DraftOperationFailed {
                operation_id,
                draft_id,
                summary: PublicSummary::OperationCancelled,
            };
        }
        () = tokio::time::sleep_until(deadline) => {
            return UiEvent::DraftOperationFailed {
                operation_id,
                draft_id,
                summary: PublicSummary::OperationTimedOut,
            };
        }
        result = &mut acquire => match result {
            Ok(lease) => lease,
            Err(error) => {
                return UiEvent::DraftOperationFailed {
                    operation_id,
                    draft_id,
                    summary: error.public_summary(),
                };
            }
        }
    };
    let ping = AssertUnwindSafe(lease.ping()).catch_unwind();
    tokio::pin!(ping);
    enum DraftAttempt {
        Summary(Option<PublicSummary>),
        Panicked(Box<dyn std::any::Any + Send>),
    }
    let attempt = tokio::select! {
        biased;
        () = cancel.cancelled() => DraftAttempt::Summary(Some(PublicSummary::OperationCancelled)),
        () = tokio::time::sleep_until(deadline) => {
            DraftAttempt::Summary(Some(PublicSummary::OperationTimedOut))
        }
        result = &mut ping => match result {
            Ok(result) => DraftAttempt::Summary(
                result.err().map(|error| ServiceError::from(error).public_summary())
            ),
            Err(payload) => DraftAttempt::Panicked(payload),
        },
    };
    let close_summary = lease
        .close()
        .await
        .err()
        .map(|error| ServiceError::from(error).public_summary());
    match attempt {
        DraftAttempt::Panicked(payload) => std::panic::resume_unwind(payload),
        DraftAttempt::Summary(summary) => {
            if let Some(summary) = summary.or(close_summary) {
                UiEvent::DraftOperationFailed {
                    operation_id,
                    draft_id,
                    summary,
                }
            } else if service.is_config_uncertain() {
                UiEvent::DraftOperationFailed {
                    operation_id,
                    draft_id,
                    summary: PublicSummary::ResourceStale,
                }
            } else {
                UiEvent::DraftConnectionReady {
                    operation_id: lease.operation_id(),
                    draft_id: lease.draft_id(),
                    elapsed_ms: lease.elapsed_ms(),
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn start_profile_work(
    work: ProfileWork,
    port: &ServicePort,
    application: &ApplicationService,
    message_tx: &mpsc::UnboundedSender<ControllerMessage>,
    global_permits: &Arc<Semaphore>,
    profile_permits: &mut ProfilePermitRegistry,
    registry: &mut TaskRegistry,
) {
    let (operation_id, profile_id, profile_generation, kind) = work.identity();
    let profile_id = profile_id.clone();
    if registry.has_profile_network_for_id(&profile_id) {
        let _ = port.try_emit(failed_profile_event(
            operation_id,
            profile_id,
            profile_generation,
            None,
            kind,
            PublicSummary::ResourceBusy,
        ));
        return;
    }
    let reservation = match registry.reserve(operation_id) {
        Ok(reservation) => reservation,
        Err(()) => {
            let _ = port.try_emit(failed_profile_event(
                operation_id,
                profile_id,
                profile_generation,
                None,
                kind,
                PublicSummary::ResourceBusy,
            ));
            return;
        }
    };
    let profile_permit = profile_permits.try_acquire(&profile_id);
    let Ok(profile_permit) = profile_permit else {
        registry.release_reservation(reservation);
        let _ = port.try_emit(failed_profile_event(
            operation_id,
            profile_id,
            profile_generation,
            None,
            kind,
            PublicSummary::ResourceBusy,
        ));
        return;
    };
    let global_permit = global_permits.clone().try_acquire_owned();
    let Ok(global_permit) = global_permit else {
        drop(profile_permit);
        registry.release_reservation(reservation);
        let _ = port.try_emit(failed_profile_event(
            operation_id,
            profile_id,
            profile_generation,
            None,
            kind,
            PublicSummary::ResourceBusy,
        ));
        return;
    };

    let cancel = CancellationToken::new();
    let task_cancel = cancel.clone();
    let service = application.clone();
    let messages = message_tx.clone();
    let completion_sent = Arc::new(AtomicBool::new(false));
    let task_completion_sent = completion_sent.clone();
    let (start_tx, start_rx) = tokio::sync::oneshot::channel();
    let join = tokio::spawn(async move {
        if start_rx.await.is_err() {
            return;
        }
        let event = run_profile_work(&service, work, &task_cancel, &messages).await;
        drop(profile_permit);
        drop(global_permit);
        task_completion_sent.store(true, Ordering::Release);
        let _ = messages.send(ControllerMessage::Completed {
            operation_id,
            output: Box::new(TaskOutput::Event(Box::new(event))),
        });
    });
    let task = RegisteredTask {
        operation_id,
        scope: TaskScope::Profile {
            profile_id: profile_id.clone(),
            profile_generation,
            session_generation: None,
        },
        cancel,
        join,
    };
    let inserted = registry.commit_reservation(
        reservation,
        task,
        TaskClass::AsyncNetwork,
        FailureContext::Profile {
            profile_id,
            profile_generation,
            kind,
        },
        completion_sent,
    );
    match inserted {
        Ok(()) => {
            let _ = start_tx.send(());
        }
        Err(task) => {
            drop(start_tx);
            task.join.abort();
            let _ = port.try_emit(failed_profile_event(
                operation_id,
                match task.scope {
                    TaskScope::Profile { profile_id, .. } => profile_id,
                    TaskScope::Draft { .. } | TaskScope::Export { .. } | TaskScope::Global => {
                        ProfileId("unavailable".to_owned())
                    }
                },
                profile_generation,
                None,
                kind,
                PublicSummary::ResourceBusy,
            ));
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn start_profile_control(
    work: ProfileControlWork,
    port: &ServicePort,
    application: &ApplicationService,
    message_tx: &mpsc::UnboundedSender<ControllerMessage>,
    global_permits: &Arc<Semaphore>,
    profile_permits: &mut ProfilePermitRegistry,
    registry: &mut TaskRegistry,
) {
    let (operation_id, profile_id, profile_generation, session_generation, kind) = work.identity();
    let profile_id = profile_id.clone();
    let reservation = match registry.reserve(operation_id) {
        Ok(reservation) => reservation,
        Err(()) => {
            let _ = port.try_emit(failed_profile_event(
                operation_id,
                profile_id,
                profile_generation,
                session_generation,
                kind,
                PublicSummary::ResourceBusy,
            ));
            return;
        }
    };
    // Leave the target entries registered so their normal Completed handling
    // emits exactly one terminal event and releases any Cancel coalescing key.
    let target_terminals = registry.cancel_profile_network_waiters(&profile_id, profile_generation);
    let reconnect_permits = if matches!(&work, ProfileControlWork::Reconnect { .. }) {
        if target_terminals.is_empty() {
            let profile_permit = profile_permits.try_acquire(&profile_id);
            let Ok(profile_permit) = profile_permit else {
                registry.release_reservation(reservation);
                let _ = port.try_emit(failed_profile_event(
                    operation_id,
                    profile_id,
                    profile_generation,
                    session_generation,
                    kind,
                    PublicSummary::ResourceBusy,
                ));
                return;
            };
            let global_permit = global_permits.clone().try_acquire_owned();
            let Ok(global_permit) = global_permit else {
                drop(profile_permit);
                registry.release_reservation(reservation);
                let _ = port.try_emit(failed_profile_event(
                    operation_id,
                    profile_id,
                    profile_generation,
                    session_generation,
                    kind,
                    PublicSummary::ResourceBusy,
                ));
                return;
            };
            Some(ReconnectPermitPlan::Ready {
                profile: profile_permit,
                global: global_permit,
            })
        } else {
            Some(ReconnectPermitPlan::Wait {
                profile: profile_permits.slot(&profile_id),
                global: global_permits.clone(),
            })
        }
    } else {
        None
    };
    let cancel = CancellationToken::new();
    let task_cancel = cancel.clone();
    let service = application.clone();
    let messages = message_tx.clone();
    let completion_sent = Arc::new(AtomicBool::new(false));
    let task_completion_sent = completion_sent.clone();
    let (start_tx, start_rx) = tokio::sync::oneshot::channel();
    let join = tokio::spawn(async move {
        if start_rx.await.is_err() {
            return;
        }
        let event = run_profile_control(
            &service,
            work,
            target_terminals,
            &task_cancel,
            &messages,
            reconnect_permits,
        )
        .await;
        task_completion_sent.store(true, Ordering::Release);
        let _ = messages.send(ControllerMessage::Completed {
            operation_id,
            output: Box::new(TaskOutput::Event(Box::new(event))),
        });
    });
    let task = RegisteredTask {
        operation_id,
        scope: TaskScope::Profile {
            profile_id: profile_id.clone(),
            profile_generation,
            session_generation,
        },
        cancel,
        join,
    };
    match registry.commit_reservation(
        reservation,
        task,
        TaskClass::AsyncNetwork,
        FailureContext::Profile {
            profile_id,
            profile_generation,
            kind,
        },
        completion_sent,
    ) {
        Ok(()) => {
            let _ = start_tx.send(());
        }
        Err(task) => {
            drop(start_tx);
            task.join.abort();
            let _ = port.try_emit(failed_profile_event(
                operation_id,
                match task.scope {
                    TaskScope::Profile { profile_id, .. } => profile_id,
                    TaskScope::Draft { .. } | TaskScope::Export { .. } | TaskScope::Global => {
                        ProfileId("unavailable".to_owned())
                    }
                },
                profile_generation,
                session_generation,
                kind,
                PublicSummary::ResourceBusy,
            ));
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_profile_control(
    service: &ApplicationService,
    work: ProfileControlWork,
    target_terminals: Vec<CancellationToken>,
    cancel: &CancellationToken,
    messages: &mpsc::UnboundedSender<ControllerMessage>,
    reconnect_permits: Option<ReconnectPermitPlan>,
) -> UiEvent {
    match work {
        ProfileControlWork::Disconnect {
            operation_id,
            profile_id,
            profile_generation,
            session_generation,
        } => {
            let cancelled = wait_for_target_terminals(target_terminals, cancel).await;
            if cancelled {
                return failed_profile_event(
                    operation_id,
                    profile_id,
                    profile_generation,
                    session_generation,
                    OperationKind::DisconnectProfile,
                    PublicSummary::OperationCancelled,
                );
            }
            if let Err(error) = service
                .ensure_profile_generation(&profile_id, profile_generation, operation_id)
                .await
            {
                return failed_from_service(
                    operation_id,
                    profile_id,
                    profile_generation,
                    session_generation,
                    OperationKind::DisconnectProfile,
                    &error,
                );
            }
            if let Some(session_generation) = session_generation {
                service
                    .evict_cached_session_exact(&profile_id, profile_generation, session_generation)
                    .await;
            }
            let post_close = match service.needs_session_credential(&profile_id).await {
                Ok(true) => PostCloseState::NeedsCredential,
                Ok(false) | Err(_) => PostCloseState::Disconnected,
            };
            UiEvent::ConnectionClosed {
                operation_id,
                profile_id,
                profile_generation,
                post_close,
            }
        }
        ProfileControlWork::Reconnect {
            operation_id,
            profile_id,
            profile_generation,
            session_generation,
            timeout,
        } => {
            let deadline = tokio::time::Instant::now() + timeout;
            let Some(reconnect_permits) = reconnect_permits else {
                return failed_profile_event(
                    operation_id,
                    profile_id,
                    profile_generation,
                    session_generation,
                    OperationKind::ReconnectProfile,
                    PublicSummary::InternalFailure,
                );
            };
            let permits = acquire_reconnect_permits_after_targets(
                reconnect_permits,
                target_terminals,
                cancel,
                deadline,
            )
            .await;
            let (profile_permit, global_permit) = match permits {
                Ok(permits) => permits,
                Err(summary) => {
                    return failed_profile_event(
                        operation_id,
                        profile_id,
                        profile_generation,
                        session_generation,
                        OperationKind::ReconnectProfile,
                        summary,
                    );
                }
            };
            if let Some(session_generation) = session_generation {
                service
                    .evict_cached_session_exact(&profile_id, profile_generation, session_generation)
                    .await;
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let event = run_connect(
                service,
                operation_id,
                profile_id,
                profile_generation,
                remaining,
                OperationKind::ReconnectProfile,
                cancel,
                messages,
            )
            .await;
            drop(profile_permit);
            drop(global_permit);
            event
        }
    }
}

async fn wait_for_target_terminals(
    target_terminals: Vec<CancellationToken>,
    cancel: &CancellationToken,
) -> bool {
    let mut cancelled = cancel.is_cancelled();
    for terminal in target_terminals {
        if cancelled {
            terminal.cancelled().await;
            continue;
        }
        tokio::select! {
            biased;
            () = cancel.cancelled() => {
                cancelled = true;
                terminal.cancelled().await;
            }
            () = terminal.cancelled() => {}
        }
    }
    cancelled
}

async fn acquire_reconnect_permits_after_targets(
    permit_plan: ReconnectPermitPlan,
    target_terminals: Vec<CancellationToken>,
    cancel: &CancellationToken,
    deadline: tokio::time::Instant,
) -> Result<(OwnedSemaphorePermit, OwnedSemaphorePermit), PublicSummary> {
    match permit_plan {
        ReconnectPermitPlan::Ready { profile, global } => Ok((profile, global)),
        ReconnectPermitPlan::Wait { profile, global } => {
            let wait_terminals = target_terminals.clone();
            let wait = async move {
                let terminals = async move {
                    for terminal in wait_terminals {
                        terminal.cancelled().await;
                    }
                };
                let profile = profile.acquire_owned();
                let global = global.acquire_owned();
                let (_, profile, global) = tokio::join!(terminals, profile, global);
                match (profile, global) {
                    (Ok(profile), Ok(global)) => Some((profile, global)),
                    _ => None,
                }
            };
            tokio::pin!(wait);
            let failure = tokio::select! {
                biased;
                () = cancel.cancelled() => PublicSummary::OperationCancelled,
                () = tokio::time::sleep_until(deadline) => PublicSummary::OperationTimedOut,
                permits = &mut wait => {
                    return permits.ok_or(PublicSummary::InternalFailure);
                }
            };
            // Preserve target terminal ordering even when this continuation is
            // cancelled or times out while queued for the inherited permits.
            for terminal in target_terminals {
                terminal.cancelled().await;
            }
            Err(failure)
        }
    }
}

async fn run_profile_work(
    service: &ApplicationService,
    work: ProfileWork,
    cancel: &CancellationToken,
    messages: &mpsc::UnboundedSender<ControllerMessage>,
) -> UiEvent {
    match work {
        ProfileWork::Connect {
            operation_id,
            profile_id,
            profile_generation,
            timeout,
            kind,
        } => {
            run_connect(
                service,
                operation_id,
                profile_id,
                profile_generation,
                timeout,
                kind,
                cancel,
                messages,
            )
            .await
        }
        ProfileWork::Execute { request, kind } => {
            run_execute(service, request, kind, cancel, messages).await
        }
        ProfileWork::BrowseCatalog { request } => {
            run_catalog_browse(service, request, cancel, messages).await
        }
        ProfileWork::ScanRedisKeys { request } => {
            run_redis_scan(service, request, cancel, messages).await
        }
        ProfileWork::InspectRedisKey { request } => {
            run_redis_inspect(service, request, cancel, messages).await
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_connect(
    service: &ApplicationService,
    operation_id: OperationId,
    profile_id: ProfileId,
    profile_generation: ProfileGeneration,
    timeout: Duration,
    kind: OperationKind,
    cancel: &CancellationToken,
    messages: &mpsc::UnboundedSender<ControllerMessage>,
) -> UiEvent {
    let started = Instant::now();
    let deadline = tokio::time::Instant::now() + timeout;
    let acquire = service.acquire_session_at(
        operation_id,
        profile_id.clone(),
        profile_generation,
        timeout,
    );
    tokio::pin!(acquire);
    let lease = tokio::select! {
        biased;
        () = cancel.cancelled() => {
            return failed_profile_event(
                operation_id,
                profile_id,
                profile_generation,
                None,
                kind,
                PublicSummary::OperationCancelled,
            );
        }
        () = tokio::time::sleep_until(deadline) => {
            return failed_profile_event(
                operation_id,
                profile_id,
                profile_generation,
                None,
                kind,
                PublicSummary::OperationTimedOut,
            );
        }
        result = &mut acquire => match result {
            Ok(lease) => lease,
            Err(error) => {
                return failed_from_service(
                    operation_id,
                    profile_id,
                    profile_generation,
                    None,
                    kind,
                    &error,
                );
            }
        }
    };
    let session_generation = lease.identity().session_generation;
    let _ = messages.send(ControllerMessage::SessionAcquired {
        operation_id,
        session_generation,
    });
    tokio::task::yield_now().await;
    let result = {
        let ping = lease.ping(timeout);
        tokio::pin!(ping);
        tokio::select! {
            biased;
            () = cancel.cancelled() => Err(PublicSummary::OperationCancelled),
            () = tokio::time::sleep_until(deadline) => Err(PublicSummary::OperationTimedOut),
            result = &mut ping => match result {
                Ok(()) => Ok(()),
                Err(error) => Err(ServiceError::from(error).public_summary()),
            }
        }
    };
    let observation = service.observe_session(&lease, operation_id).await;
    match (result, observation) {
        (Ok(()), Ok(())) => UiEvent::ConnectionReady {
            operation_id,
            profile_id,
            profile_generation,
            session_generation,
            elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        },
        (Err(summary), _) => {
            service.evict_session_lease(&lease).await;
            failed_profile_event(
                operation_id,
                profile_id,
                profile_generation,
                Some(session_generation),
                kind,
                summary,
            )
        }
        (Ok(()), Err(error)) => {
            service.evict_session_lease(&lease).await;
            failed_from_service(
                operation_id,
                profile_id,
                profile_generation,
                Some(session_generation),
                kind,
                &error,
            )
        }
    }
}

async fn run_execute(
    service: &ApplicationService,
    request: crate::model::ExecuteRequest,
    kind: OperationKind,
    cancel: &CancellationToken,
    messages: &mpsc::UnboundedSender<ControllerMessage>,
) -> UiEvent {
    let operation_id = request.operation_id;
    let profile_id = request.profile_id.clone();
    let profile_generation = request.profile_generation;
    let timeout = request.timeout;
    let deadline = tokio::time::Instant::now() + timeout;
    let prepare = service.prepare_execute_request(&request);
    tokio::pin!(prepare);
    let typed_request = tokio::select! {
        biased;
        () = cancel.cancelled() => {
            return failed_profile_event(
                operation_id,
                profile_id,
                profile_generation,
                None,
                kind,
                PublicSummary::OperationCancelled,
            );
        }
        () = tokio::time::sleep_until(deadline) => {
            return failed_profile_event(
                operation_id,
                profile_id,
                profile_generation,
                None,
                kind,
                PublicSummary::OperationTimedOut,
            );
        }
        result = &mut prepare => match result {
            Ok(request) => request,
            Err(error) => {
                return failed_from_service(
                    operation_id,
                    profile_id,
                    profile_generation,
                    None,
                    kind,
                    &error,
                );
            }
        }
    };
    let acquire = service.acquire_session_at(
        operation_id,
        profile_id.clone(),
        profile_generation,
        timeout,
    );
    tokio::pin!(acquire);
    let lease = tokio::select! {
        biased;
        () = cancel.cancelled() => {
            return failed_profile_event(
                operation_id,
                profile_id,
                profile_generation,
                None,
                kind,
                PublicSummary::OperationCancelled,
            );
        }
        () = tokio::time::sleep_until(deadline) => {
            return failed_profile_event(
                operation_id,
                profile_id,
                profile_generation,
                None,
                kind,
                PublicSummary::OperationTimedOut,
            );
        }
        result = &mut acquire => match result {
            Ok(lease) => lease,
            Err(error) => {
                return failed_from_service(
                    operation_id,
                    profile_id,
                    profile_generation,
                    None,
                    kind,
                    &error,
                );
            }
        }
    };
    let session_generation = lease.identity().session_generation;
    let _ = messages.send(ControllerMessage::SessionAcquired {
        operation_id,
        session_generation,
    });
    tokio::task::yield_now().await;
    enum ExecuteAttempt {
        Driver(Result<crate::model::QueryResult, crate::drivers::DriverError>),
        Cancelled,
        TimedOut,
    }
    let attempt = {
        let execute = lease.execute_typed(&typed_request);
        tokio::pin!(execute);
        tokio::select! {
            biased;
            () = cancel.cancelled() => ExecuteAttempt::Cancelled,
            () = tokio::time::sleep_until(deadline) => ExecuteAttempt::TimedOut,
            result = &mut execute => ExecuteAttempt::Driver(result),
        }
    };
    let observation = service.observe_session(&lease, operation_id).await;
    match (attempt, observation) {
        (ExecuteAttempt::Driver(Ok(result)), Ok(())) => UiEvent::QueryFinished {
            operation_id,
            profile_id,
            profile_generation,
            session_generation,
            result: service.retain_execute_result(&typed_request, result),
        },
        (ExecuteAttempt::Driver(Err(error)), Ok(())) => {
            let disposition = SessionDisposition::for_driver_error(&error);
            let summary = ServiceError::from(error).public_summary();
            if disposition == SessionDisposition::Evict {
                service.evict_session_lease(&lease).await;
            }
            failed_profile_event_with_disposition(
                operation_id,
                profile_id,
                profile_generation,
                session_generation,
                kind,
                summary,
                disposition,
            )
        }
        (ExecuteAttempt::Cancelled, _) => {
            service.evict_session_lease(&lease).await;
            failed_profile_event(
                operation_id,
                profile_id,
                profile_generation,
                Some(session_generation),
                kind,
                PublicSummary::OperationCancelled,
            )
        }
        (ExecuteAttempt::TimedOut, _) => {
            service.evict_session_lease(&lease).await;
            failed_profile_event(
                operation_id,
                profile_id,
                profile_generation,
                Some(session_generation),
                kind,
                PublicSummary::OperationTimedOut,
            )
        }
        (ExecuteAttempt::Driver(_), Err(error)) => {
            service.evict_session_lease(&lease).await;
            failed_from_service(
                operation_id,
                profile_id,
                profile_generation,
                Some(session_generation),
                kind,
                &error,
            )
        }
    }
}

async fn run_catalog_browse(
    service: &ApplicationService,
    request: crate::model::CatalogRequest,
    cancel: &CancellationToken,
    messages: &mpsc::UnboundedSender<ControllerMessage>,
) -> UiEvent {
    let operation_id = request.operation_id();
    let profile_id = request.profile_id().clone();
    let profile_generation = request.profile_generation();
    let timeout = request.timeout();
    let deadline = tokio::time::Instant::now() + request.timeout();
    let prepared = {
        let prepare = service.prepare_catalog_request(&request);
        tokio::pin!(prepare);
        tokio::select! {
            biased;
            () = cancel.cancelled() => Err(PublicSummary::OperationCancelled),
            () = tokio::time::sleep_until(deadline) => Err(PublicSummary::OperationTimedOut),
            result = &mut prepare => result.map_err(|error| error.public_summary()),
        }
    };
    let token_key_context = match prepared {
        Ok(context) => context,
        Err(summary) => return catalog_failed_event(request, summary, None, None),
    };

    let token_key_result = match await_pre_session_blocking(
        service.spawn_catalog_token_key_load(token_key_context),
        cancel,
        deadline,
    )
    .await
    {
        Ok(result) => result,
        Err(summary) => return catalog_failed_event(request, summary, None, None),
    };
    let token_key = match token_key_result {
        Ok(token_key) => token_key,
        Err(error) => {
            return catalog_failed_event(request, error.public_summary(), None, None);
        }
    };

    let acquire = service.acquire_session_at(operation_id, profile_id, profile_generation, timeout);
    tokio::pin!(acquire);
    let lease = tokio::select! {
        biased;
        () = cancel.cancelled() => {
            return catalog_failed_event(
                request,
                PublicSummary::OperationCancelled,
                None,
                None,
            );
        }
        () = tokio::time::sleep_until(deadline) => {
            return catalog_failed_event(
                request,
                PublicSummary::OperationTimedOut,
                None,
                None,
            );
        }
        result = &mut acquire => match result {
            Ok(lease) => lease,
            Err(error) => {
                return catalog_failed_event(request, error.public_summary(), None, None);
            }
        }
    };
    let session_generation = lease.identity().session_generation;
    let _ = messages.send(ControllerMessage::SessionAcquired {
        operation_id,
        session_generation,
    });
    tokio::task::yield_now().await;

    enum CatalogAttempt {
        Driver(Box<Result<crate::model::CatalogPage, crate::drivers::DriverError>>),
        Cancelled,
        TimedOut,
    }
    let attempt = {
        let browse = lease.load_catalog_page(&request, token_key.as_ref());
        tokio::pin!(browse);
        tokio::select! {
            biased;
            () = cancel.cancelled() => CatalogAttempt::Cancelled,
            () = tokio::time::sleep_until(deadline) => CatalogAttempt::TimedOut,
            result = &mut browse => CatalogAttempt::Driver(Box::new(result)),
        }
    };
    let observation = service.observe_session(&lease, operation_id).await;
    match attempt {
        CatalogAttempt::Driver(result) => match (*result, observation) {
            (Ok(page), Ok(())) => UiEvent::CatalogPageLoaded {
                page,
                session_generation,
                session_disposition: SessionDisposition::Keep,
            },
            (Err(error), Ok(())) => {
                let disposition = SessionDisposition::for_driver_error(&error);
                let summary = ServiceError::from(error).public_summary();
                if disposition == SessionDisposition::Evict {
                    service.evict_session_lease(&lease).await;
                }
                catalog_failed_event(
                    request,
                    summary,
                    Some(session_generation),
                    Some(disposition),
                )
            }
            (_, Err(error)) => {
                service.evict_session_lease(&lease).await;
                catalog_failed_event(
                    request,
                    error.public_summary(),
                    Some(session_generation),
                    Some(SessionDisposition::Evict),
                )
            }
        },
        CatalogAttempt::Cancelled => {
            service.evict_session_lease(&lease).await;
            catalog_failed_event(
                request,
                PublicSummary::OperationCancelled,
                Some(session_generation),
                Some(SessionDisposition::Evict),
            )
        }
        CatalogAttempt::TimedOut => {
            service.evict_session_lease(&lease).await;
            catalog_failed_event(
                request,
                PublicSummary::OperationTimedOut,
                Some(session_generation),
                Some(SessionDisposition::Evict),
            )
        }
    }
}

pub(super) async fn await_pre_session_blocking<T: Send + 'static>(
    mut task: JoinHandle<T>,
    cancel: &CancellationToken,
    deadline: tokio::time::Instant,
) -> Result<T, PublicSummary> {
    tokio::select! {
        biased;
        () = cancel.cancelled() => {
            // `abort` prevents work that has not started. A running
            // `spawn_blocking` may finish the idempotent 32-byte sidecar
            // publish after this handle is dropped, but it has no database,
            // session, event, or UI side effects.
            task.abort();
            Err(PublicSummary::OperationCancelled)
        }
        () = tokio::time::sleep_until(deadline) => {
            task.abort();
            Err(PublicSummary::OperationTimedOut)
        }
        result = &mut task => result.map_err(|_| PublicSummary::InternalFailure),
    }
}

fn catalog_failed_event(
    request: crate::model::CatalogRequest,
    summary: PublicSummary,
    session_generation: Option<SessionGeneration>,
    session_disposition: Option<SessionDisposition>,
) -> UiEvent {
    UiEvent::CatalogPageFailed {
        request,
        summary,
        session_generation,
        session_disposition,
    }
}

async fn run_redis_scan(
    service: &ApplicationService,
    request: RedisScanRequest,
    cancel: &CancellationToken,
    messages: &mpsc::UnboundedSender<ControllerMessage>,
) -> UiEvent {
    let operation_id = request.operation_id();
    let profile_id = request.profile_id().clone();
    let profile_generation = request.profile_generation();
    let kind = OperationKind::BrowseRedis;
    let deadline = tokio::time::Instant::now() + request.timeout;
    let prepared = {
        let prepare = service.prepare_redis_scan_request(&request);
        tokio::pin!(prepare);
        tokio::select! {
            biased;
            () = cancel.cancelled() => Err((PublicSummary::OperationCancelled, PublicCode::None)),
            () = tokio::time::sleep_until(deadline) => {
                Err((PublicSummary::OperationTimedOut, PublicCode::None))
            }
            result = &mut prepare => result.map_err(|error| error.public_error_parts()),
        }
    };
    if let Err((summary, code)) = prepared {
        return failed_redis_resource_event(
            RedisResourceRequest::Scan(request),
            public_redis_resource_error(kind, profile_id, operation_id, summary, code),
            None,
            None,
            connection_outcome_for_summary(summary),
        );
    }

    let acquire = service.acquire_session_at(
        operation_id,
        profile_id.clone(),
        profile_generation,
        request.timeout,
    );
    tokio::pin!(acquire);
    let lease = tokio::select! {
        biased;
        () = cancel.cancelled() => {
            return failed_redis_resource_event(
                RedisResourceRequest::Scan(request),
                public_redis_resource_error(
                    kind,
                    profile_id,
                    operation_id,
                    PublicSummary::OperationCancelled,
                    PublicCode::None,
                ),
                None,
                None,
                ConnectionFailureOutcome::Unknown,
            );
        }
        () = tokio::time::sleep_until(deadline) => {
            return failed_redis_resource_event(
                RedisResourceRequest::Scan(request),
                public_redis_resource_error(
                    kind,
                    profile_id,
                    operation_id,
                    PublicSummary::OperationTimedOut,
                    PublicCode::None,
                ),
                None,
                None,
                ConnectionFailureOutcome::Unknown,
            );
        }
        result = &mut acquire => match result {
            Ok(lease) => lease,
            Err(error) => {
                let (summary, code) = error.public_error_parts();
                return failed_redis_resource_event(
                    RedisResourceRequest::Scan(request),
                    public_redis_resource_error(
                        kind,
                        profile_id,
                        operation_id,
                        summary,
                        code,
                    ),
                    None,
                    None,
                    connection_outcome_for_summary(summary),
                );
            }
        }
    };
    let session_generation = lease.identity().session_generation;
    let _ = messages.send(ControllerMessage::SessionAcquired {
        operation_id,
        session_generation,
    });
    tokio::task::yield_now().await;

    enum ScanAttempt {
        Driver(Result<crate::model::RedisKeyPage, crate::drivers::DriverError>),
        Cancelled,
        TimedOut,
    }
    let attempt = {
        let scan = lease.scan_redis_keys(&request);
        tokio::pin!(scan);
        tokio::select! {
            biased;
            () = cancel.cancelled() => ScanAttempt::Cancelled,
            () = tokio::time::sleep_until(deadline) => ScanAttempt::TimedOut,
            result = &mut scan => ScanAttempt::Driver(result),
        }
    };
    let observation = service.observe_session(&lease, operation_id).await;
    match (attempt, observation) {
        (ScanAttempt::Driver(Ok(page)), Ok(())) => UiEvent::RedisKeysLoaded {
            page,
            session_generation,
            session_disposition: SessionDisposition::Keep,
        },
        (ScanAttempt::Driver(Err(driver_error)), Ok(())) => {
            let session_disposition = SessionDisposition::for_driver_error(&driver_error);
            let service_error = ServiceError::from(driver_error);
            let (summary, code) = service_error.public_error_parts();
            if session_disposition == SessionDisposition::Evict {
                service.evict_session_lease(&lease).await;
            }
            failed_redis_resource_event(
                RedisResourceRequest::Scan(request),
                public_redis_resource_error(kind, profile_id, operation_id, summary, code),
                Some(session_generation),
                Some(session_disposition),
                connection_outcome_for_disposition(session_disposition),
            )
        }
        (ScanAttempt::Cancelled, _) => {
            service.evict_session_lease(&lease).await;
            failed_redis_resource_event(
                RedisResourceRequest::Scan(request),
                public_redis_resource_error(
                    kind,
                    profile_id,
                    operation_id,
                    PublicSummary::OperationCancelled,
                    PublicCode::None,
                ),
                Some(session_generation),
                Some(SessionDisposition::Evict),
                ConnectionFailureOutcome::Disconnected,
            )
        }
        (ScanAttempt::TimedOut, _) => {
            service.evict_session_lease(&lease).await;
            failed_redis_resource_event(
                RedisResourceRequest::Scan(request),
                public_redis_resource_error(
                    kind,
                    profile_id,
                    operation_id,
                    PublicSummary::OperationTimedOut,
                    PublicCode::None,
                ),
                Some(session_generation),
                Some(SessionDisposition::Evict),
                ConnectionFailureOutcome::Disconnected,
            )
        }
        (ScanAttempt::Driver(_), Err(error)) => {
            service.evict_session_lease(&lease).await;
            let (summary, code) = error.public_error_parts();
            failed_redis_resource_event(
                RedisResourceRequest::Scan(request),
                public_redis_resource_error(kind, profile_id, operation_id, summary, code),
                Some(session_generation),
                Some(SessionDisposition::Evict),
                ConnectionFailureOutcome::Disconnected,
            )
        }
    }
}

async fn run_redis_inspect(
    service: &ApplicationService,
    request: RedisKeyInspectRequest,
    cancel: &CancellationToken,
    messages: &mpsc::UnboundedSender<ControllerMessage>,
) -> UiEvent {
    let operation_id = request.operation_id();
    let profile_id = request.profile_id().clone();
    let profile_generation = request.profile_generation();
    let kind = OperationKind::InspectRedis;
    let deadline = tokio::time::Instant::now() + request.timeout;
    let prepared = {
        let prepare = service.prepare_redis_inspect_request(&request);
        tokio::pin!(prepare);
        tokio::select! {
            biased;
            () = cancel.cancelled() => Err((PublicSummary::OperationCancelled, PublicCode::None)),
            () = tokio::time::sleep_until(deadline) => {
                Err((PublicSummary::OperationTimedOut, PublicCode::None))
            }
            result = &mut prepare => result.map_err(|error| error.public_error_parts()),
        }
    };
    if let Err((summary, code)) = prepared {
        return failed_redis_resource_event(
            RedisResourceRequest::Inspect(request),
            public_redis_resource_error(kind, profile_id, operation_id, summary, code),
            None,
            None,
            connection_outcome_for_summary(summary),
        );
    }

    let acquire = service.acquire_session_at(
        operation_id,
        profile_id.clone(),
        profile_generation,
        request.timeout,
    );
    tokio::pin!(acquire);
    let lease = tokio::select! {
        biased;
        () = cancel.cancelled() => {
            return failed_redis_resource_event(
                RedisResourceRequest::Inspect(request),
                public_redis_resource_error(
                    kind,
                    profile_id,
                    operation_id,
                    PublicSummary::OperationCancelled,
                    PublicCode::None,
                ),
                None,
                None,
                ConnectionFailureOutcome::Unknown,
            );
        }
        () = tokio::time::sleep_until(deadline) => {
            return failed_redis_resource_event(
                RedisResourceRequest::Inspect(request),
                public_redis_resource_error(
                    kind,
                    profile_id,
                    operation_id,
                    PublicSummary::OperationTimedOut,
                    PublicCode::None,
                ),
                None,
                None,
                ConnectionFailureOutcome::Unknown,
            );
        }
        result = &mut acquire => match result {
            Ok(lease) => lease,
            Err(error) => {
                let (summary, code) = error.public_error_parts();
                return failed_redis_resource_event(
                    RedisResourceRequest::Inspect(request),
                    public_redis_resource_error(
                        kind,
                        profile_id,
                        operation_id,
                        summary,
                        code,
                    ),
                    None,
                    None,
                    connection_outcome_for_summary(summary),
                );
            }
        }
    };
    let session_generation = lease.identity().session_generation;
    let _ = messages.send(ControllerMessage::SessionAcquired {
        operation_id,
        session_generation,
    });
    tokio::task::yield_now().await;

    enum InspectAttempt {
        Driver(Result<crate::model::RedisValuePreview, crate::drivers::DriverError>),
        Cancelled,
        TimedOut,
    }
    let attempt = {
        let inspect = lease.inspect_redis_key(&request);
        tokio::pin!(inspect);
        tokio::select! {
            biased;
            () = cancel.cancelled() => InspectAttempt::Cancelled,
            () = tokio::time::sleep_until(deadline) => InspectAttempt::TimedOut,
            result = &mut inspect => InspectAttempt::Driver(result),
        }
    };
    let observation = service.observe_session(&lease, operation_id).await;
    match (attempt, observation) {
        (InspectAttempt::Driver(Ok(preview)), Ok(())) => UiEvent::RedisKeyInspected {
            preview,
            session_generation,
            session_disposition: SessionDisposition::Keep,
        },
        (InspectAttempt::Driver(Err(driver_error)), Ok(())) => {
            let session_disposition = SessionDisposition::for_driver_error(&driver_error);
            let service_error = ServiceError::from(driver_error);
            let (summary, code) = service_error.public_error_parts();
            if session_disposition == SessionDisposition::Evict {
                service.evict_session_lease(&lease).await;
            }
            failed_redis_resource_event(
                RedisResourceRequest::Inspect(request),
                public_redis_resource_error(kind, profile_id, operation_id, summary, code),
                Some(session_generation),
                Some(session_disposition),
                connection_outcome_for_disposition(session_disposition),
            )
        }
        (InspectAttempt::Cancelled, _) => {
            service.evict_session_lease(&lease).await;
            failed_redis_resource_event(
                RedisResourceRequest::Inspect(request),
                public_redis_resource_error(
                    kind,
                    profile_id,
                    operation_id,
                    PublicSummary::OperationCancelled,
                    PublicCode::None,
                ),
                Some(session_generation),
                Some(SessionDisposition::Evict),
                ConnectionFailureOutcome::Disconnected,
            )
        }
        (InspectAttempt::TimedOut, _) => {
            service.evict_session_lease(&lease).await;
            failed_redis_resource_event(
                RedisResourceRequest::Inspect(request),
                public_redis_resource_error(
                    kind,
                    profile_id,
                    operation_id,
                    PublicSummary::OperationTimedOut,
                    PublicCode::None,
                ),
                Some(session_generation),
                Some(SessionDisposition::Evict),
                ConnectionFailureOutcome::Disconnected,
            )
        }
        (InspectAttempt::Driver(_), Err(error)) => {
            service.evict_session_lease(&lease).await;
            let (summary, code) = error.public_error_parts();
            failed_redis_resource_event(
                RedisResourceRequest::Inspect(request),
                public_redis_resource_error(kind, profile_id, operation_id, summary, code),
                Some(session_generation),
                Some(SessionDisposition::Evict),
                ConnectionFailureOutcome::Disconnected,
            )
        }
    }
}

fn start_mutation(
    command: UiCommand,
    port: &ServicePort,
    application: &ApplicationService,
    message_tx: &mpsc::UnboundedSender<ControllerMessage>,
    registry: &mut TaskRegistry,
) -> bool {
    if application.is_config_uncertain() && !matches!(&command, UiCommand::RefreshProfiles { .. }) {
        let _ = port.try_emit(UiEvent::ConfigUncertain {
            operation_id: command.operation_id(),
        });
        return false;
    }
    let reservation = match registry.reserve(command.operation_id()) {
        Ok(reservation) => reservation,
        Err(()) => {
            let _ = port.try_emit(failure_for_unavailable(
                command,
                PublicSummary::ResourceBusy,
            ));
            return false;
        }
    };
    let (operation_id, scope, failure) = match &command {
        UiCommand::RefreshProfiles { operation_id } => {
            (*operation_id, TaskScope::Global, FailureContext::Profiles)
        }
        UiCommand::CreateProfile(request) => (
            request.operation_id,
            TaskScope::Draft {
                draft_id: request.draft_id,
            },
            FailureContext::Profiles,
        ),
        UiCommand::UpdateProfile(request) => (
            request.operation_id,
            TaskScope::Profile {
                profile_id: request.profile_id.clone(),
                profile_generation: request.expected_generation,
                session_generation: None,
            },
            FailureContext::Profile {
                profile_id: request.profile_id.clone(),
                profile_generation: request.expected_generation,
                kind: OperationKind::UpdateProfile,
            },
        ),
        UiCommand::DeleteProfile(request) => (
            request.operation_id,
            TaskScope::Profile {
                profile_id: request.profile_id.clone(),
                profile_generation: request.expected_generation,
                session_generation: None,
            },
            FailureContext::Profile {
                profile_id: request.profile_id.clone(),
                profile_generation: request.expected_generation,
                kind: OperationKind::DeleteProfile,
            },
        ),
        _ => {
            registry.release_reservation(reservation);
            return false;
        }
    };
    let cancel = CancellationToken::new();
    let service = application.clone();
    let messages = message_tx.clone();
    let completion_sent = Arc::new(AtomicBool::new(false));
    let task_completion_sent = completion_sent.clone();
    let (start_tx, start_rx) = tokio::sync::oneshot::channel();
    let join = tokio::spawn(async move {
        if start_rx.await.is_err() {
            return;
        }
        let output = run_mutation(&service, command).await;
        task_completion_sent.store(true, Ordering::Release);
        let _ = messages.send(ControllerMessage::Completed {
            operation_id,
            output: Box::new(output),
        });
    });
    let task = RegisteredTask {
        operation_id,
        scope,
        cancel,
        join,
    };
    match registry.commit_reservation(
        reservation,
        task,
        TaskClass::Mutation,
        failure,
        completion_sent,
    ) {
        Ok(()) => {
            let _ = start_tx.send(());
            true
        }
        Err(task) => {
            drop(start_tx);
            task.join.abort();
            false
        }
    }
}

async fn run_mutation(service: &ApplicationService, command: UiCommand) -> TaskOutput {
    match command {
        UiCommand::RefreshProfiles { operation_id } => {
            let result = match service.reload_configuration_for_runtime().await {
                Ok(outcome) if !outcome.config_uncertain => {
                    snapshots(service).await.map(|profiles| (outcome, profiles))
                }
                Ok(outcome) => Ok((outcome, Vec::new())),
                Err(error) => Err(error),
            };
            TaskOutput::Reload {
                operation_id,
                result: Box::new(result),
            }
        }
        UiCommand::CreateProfile(request) => {
            let operation_id = request.operation_id;
            let fallback_profile_id = request
                .explicit_id
                .clone()
                .unwrap_or_else(|| ProfileId(format!("draft-{}", request.draft_id.0)));
            TaskOutput::Create {
                operation_id,
                fallback_profile_id,
                result: Box::new(service.create_profile_for_runtime(request).await),
            }
        }
        UiCommand::UpdateProfile(request) => {
            let operation_id = request.operation_id;
            let profile_id = request.profile_id.clone();
            let previous_generation = request.expected_generation;
            TaskOutput::Update {
                operation_id,
                profile_id,
                previous_generation,
                result: Box::new(service.update_profile_for_runtime(request).await),
            }
        }
        UiCommand::DeleteProfile(request) => {
            let operation_id = request.operation_id;
            let profile_id = request.profile_id.clone();
            let previous_generation = request.expected_generation;
            TaskOutput::Delete {
                operation_id,
                profile_id,
                previous_generation,
                result: Box::new(service.delete_profile_for_runtime(request).await),
            }
        }
        other => TaskOutput::Event(Box::new(failure_for_unavailable(
            other,
            PublicSummary::InternalFailure,
        ))),
    }
}

async fn handle_controller_message(
    message: ControllerMessage,
    port: &ServicePort,
    application: &ApplicationService,
    message_tx: &mpsc::UnboundedSender<ControllerMessage>,
    registry: &mut TaskRegistry,
    mutation_active: &mut bool,
) {
    match message {
        ControllerMessage::SessionAcquired {
            operation_id,
            session_generation,
        } => registry.update_session(operation_id, session_generation),
        ControllerMessage::Completed {
            operation_id,
            output,
        } => {
            let Some(entry) = registry.take(operation_id) else {
                return;
            };
            let RegistryEntry {
                task,
                class,
                failure,
                terminal,
                cleanup_started,
                ..
            } = entry;
            let scope = task.scope;
            if task.join.await.is_err() {
                if class == TaskClass::Mutation {
                    *mutation_active = false;
                }
                if !cleanup_started
                    && start_registered_failure_cleanup(
                        operation_id,
                        scope,
                        failure.clone(),
                        terminal.clone(),
                        application,
                        message_tx,
                        registry,
                    )
                {
                    return;
                }
                emit_internal_failure(port, operation_id, failure);
                terminal.cancel();
                return;
            }
            if class == TaskClass::Mutation && !matches!(output.as_ref(), TaskOutput::Event(_)) {
                let registered = start_registered_mutation_completion(
                    operation_id,
                    scope,
                    failure.clone(),
                    *output,
                    application,
                    message_tx,
                    registry,
                );
                terminal.cancel();
                if registered {
                    return;
                }
                *mutation_active = false;
                emit_internal_failure(port, operation_id, failure);
                return;
            }
            if class == TaskClass::Mutation {
                *mutation_active = false;
            }
            let event = finish_task_output(*output, application, false).await;
            let _ = port.try_emit(event);
            terminal.cancel();
        }
    }
}

fn start_registered_failure_cleanup(
    operation_id: OperationId,
    scope: TaskScope,
    failure: FailureContext,
    terminal: CancellationToken,
    application: &ApplicationService,
    message_tx: &mpsc::UnboundedSender<ControllerMessage>,
    registry: &mut TaskRegistry,
) -> bool {
    start_registered_failure_cleanup_with_ready(
        operation_id,
        scope,
        failure,
        terminal,
        application,
        message_tx,
        registry,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
fn start_registered_failure_cleanup_with_ready(
    operation_id: OperationId,
    scope: TaskScope,
    failure: FailureContext,
    terminal: CancellationToken,
    application: &ApplicationService,
    message_tx: &mpsc::UnboundedSender<ControllerMessage>,
    registry: &mut TaskRegistry,
    ready: Option<tokio::sync::oneshot::Sender<()>>,
) -> bool {
    let reservation = match registry.reserve(operation_id) {
        Ok(reservation) => reservation,
        Err(()) => return false,
    };
    let cancel = CancellationToken::new();
    let service = application.clone();
    let cleanup_scope = scope.clone();
    let cleanup_failure = failure.clone();
    let messages = message_tx.clone();
    let completion_sent = Arc::new(AtomicBool::new(false));
    let task_completion_sent = completion_sent.clone();
    let (start_tx, start_rx) = tokio::sync::oneshot::channel();
    let join = tokio::spawn(async move {
        if start_rx.await.is_err() {
            return;
        }
        cleanup_failed_scope(&service, &cleanup_scope, ready).await;
        let event = internal_failure_event(operation_id, cleanup_failure);
        task_completion_sent.store(true, Ordering::Release);
        let _ = messages.send(ControllerMessage::Completed {
            operation_id,
            output: Box::new(TaskOutput::Event(Box::new(event))),
        });
    });
    let task = RegisteredTask {
        operation_id,
        scope,
        cancel,
        join,
    };
    match registry.commit_reservation_with_state(
        reservation,
        task,
        TaskClass::AsyncNetwork,
        failure,
        completion_sent,
        terminal,
        true,
    ) {
        Ok(()) => {
            let _ = start_tx.send(());
            true
        }
        Err(task) => {
            drop(start_tx);
            task.join.abort();
            false
        }
    }
}

fn start_registered_mutation_completion(
    operation_id: OperationId,
    scope: TaskScope,
    failure: FailureContext,
    output: TaskOutput,
    application: &ApplicationService,
    message_tx: &mpsc::UnboundedSender<ControllerMessage>,
    registry: &mut TaskRegistry,
) -> bool {
    let reservation = match registry.reserve(operation_id) {
        Ok(reservation) => reservation,
        Err(()) => return false,
    };
    let (waiters, had_active) = mutation_completion_waiters(&output, application, registry);
    let cancel = CancellationToken::new();
    let service = application.clone();
    let messages = message_tx.clone();
    let completion_sent = Arc::new(AtomicBool::new(false));
    let task_completion_sent = completion_sent.clone();
    let (start_tx, start_rx) = tokio::sync::oneshot::channel();
    let join = tokio::spawn(async move {
        if start_rx.await.is_err() {
            return;
        }
        for waiter in waiters {
            waiter.cancelled().await;
        }
        let event = finish_task_output(output, &service, had_active).await;
        task_completion_sent.store(true, Ordering::Release);
        let _ = messages.send(ControllerMessage::Completed {
            operation_id,
            output: Box::new(TaskOutput::Event(Box::new(event))),
        });
    });
    let task = RegisteredTask {
        operation_id,
        scope,
        cancel,
        join,
    };
    match registry.commit_reservation(
        reservation,
        task,
        TaskClass::Mutation,
        failure,
        completion_sent,
    ) {
        Ok(()) => {
            let _ = start_tx.send(());
            true
        }
        Err(task) => {
            drop(start_tx);
            task.join.abort();
            false
        }
    }
}

fn mutation_completion_waiters(
    output: &TaskOutput,
    application: &ApplicationService,
    registry: &TaskRegistry,
) -> (Vec<CancellationToken>, bool) {
    let had_active = match output {
        TaskOutput::Update {
            profile_id,
            previous_generation,
            ..
        }
        | TaskOutput::Delete {
            profile_id,
            previous_generation,
            ..
        } => registry.has_profile_network(profile_id, *previous_generation),
        TaskOutput::Event(_) | TaskOutput::Reload { .. } | TaskOutput::Create { .. } => false,
    };
    if application.is_config_uncertain() {
        return (registry.cancel_all_network_waiters(), had_active);
    }

    let targets = match output {
        TaskOutput::Reload { result, .. } => match result.as_ref() {
            Ok((outcome, _)) => outcome.cleanup.targets().collect::<Vec<_>>(),
            Err(_) => Vec::new(),
        },
        TaskOutput::Create { result, .. } => match result.as_ref() {
            Ok(outcome) => outcome.cleanup.targets().collect::<Vec<_>>(),
            Err(failure) => failure.cleanup.targets().collect::<Vec<_>>(),
        },
        TaskOutput::Update { result, .. } => match result.as_ref() {
            Ok(outcome) => outcome.cleanup.targets().collect::<Vec<_>>(),
            Err(failure) => failure.cleanup.targets().collect::<Vec<_>>(),
        },
        TaskOutput::Delete { result, .. } => match result.as_ref() {
            Ok(outcome) => outcome.cleanup.targets().collect::<Vec<_>>(),
            Err(failure) => failure.cleanup.targets().collect::<Vec<_>>(),
        },
        TaskOutput::Event(_) => Vec::new(),
    };
    let waiters = targets
        .into_iter()
        .flat_map(|(profile_id, generation)| {
            registry.cancel_profile_network_waiters(profile_id, generation)
        })
        .collect();
    (waiters, had_active)
}

async fn finish_runtime_mutation_failure(
    application: &ApplicationService,
    failure: RuntimeMutationFailure,
) -> ServiceError {
    match application.apply_deferred_cleanup(failure.cleanup).await {
        Ok(()) => failure.error,
        Err(error) => error,
    }
}

async fn finish_task_output(
    output: TaskOutput,
    application: &ApplicationService,
    had_active: bool,
) -> UiEvent {
    match output {
        TaskOutput::Event(event) => *event,
        TaskOutput::Reload {
            operation_id,
            result,
        } => match *result {
            Ok((outcome, profiles)) => {
                if let Err(error) = application.apply_deferred_cleanup(outcome.cleanup).await {
                    UiEvent::ProfilesFailed {
                        operation_id,
                        summary: error.public_summary(),
                    }
                } else if outcome.config_uncertain {
                    UiEvent::ConfigUncertain { operation_id }
                } else {
                    UiEvent::ProfilesLoaded {
                        operation_id,
                        profiles,
                    }
                }
            }
            Err(_error) if application.is_config_uncertain() => {
                UiEvent::ConfigUncertain { operation_id }
            }
            Err(error) => UiEvent::ProfilesFailed {
                operation_id,
                summary: error.public_summary(),
            },
        },
        TaskOutput::Create {
            operation_id,
            fallback_profile_id,
            result,
        } => match *result {
            Ok(outcome) => {
                if let Err(error) = application.apply_deferred_cleanup(outcome.cleanup).await {
                    UiEvent::ProfileSaveFailed {
                        operation_id,
                        profile_id: fallback_profile_id,
                        summary: error.public_summary(),
                    }
                } else {
                    UiEvent::ProfileSaved {
                        operation_id,
                        profile_id: outcome.mutation.profile_id,
                        previous_generation: None,
                        profile_generation: outcome.mutation.profile_generation,
                        session_retained: false,
                        warning: commit_warning(outcome.mutation.commit_state),
                    }
                }
            }
            Err(failure) => {
                let error = finish_runtime_mutation_failure(application, failure).await;
                if application.is_config_uncertain() {
                    UiEvent::ConfigUncertain { operation_id }
                } else {
                    UiEvent::ProfileSaveFailed {
                        operation_id,
                        profile_id: fallback_profile_id,
                        summary: error.public_summary(),
                    }
                }
            }
        },
        TaskOutput::Update {
            operation_id,
            profile_id,
            previous_generation,
            result,
        } => match *result {
            Ok(outcome) => {
                let session_retained = match outcome.deferred_session {
                    Some(fence) => {
                        application
                            .resolve_deferred_session(fence, !had_active)
                            .await
                    }
                    None => false,
                };
                if let Err(error) = application.apply_deferred_cleanup(outcome.cleanup).await {
                    return UiEvent::ProfileSaveFailed {
                        operation_id,
                        profile_id,
                        summary: error.public_summary(),
                    };
                }
                UiEvent::ProfileSaved {
                    operation_id,
                    profile_id: outcome.mutation.profile_id,
                    previous_generation: Some(previous_generation),
                    profile_generation: outcome.mutation.profile_generation,
                    session_retained,
                    warning: commit_warning(outcome.mutation.commit_state),
                }
            }
            Err(failure) => {
                let error = finish_runtime_mutation_failure(application, failure).await;
                if application.is_config_uncertain() {
                    UiEvent::ConfigUncertain { operation_id }
                } else {
                    UiEvent::ProfileSaveFailed {
                        operation_id,
                        profile_id,
                        summary: error.public_summary(),
                    }
                }
            }
        },
        TaskOutput::Delete {
            operation_id,
            profile_id,
            previous_generation,
            result,
        } => match *result {
            Ok(outcome) => {
                let cleanup_result = application.apply_deferred_cleanup(outcome.cleanup).await;
                if let Err(error) = cleanup_result {
                    return failed_profile_event(
                        operation_id,
                        profile_id,
                        previous_generation,
                        None,
                        OperationKind::DeleteProfile,
                        error.public_summary(),
                    );
                }
                UiEvent::ProfileDeleted {
                    operation_id,
                    profile_id: outcome.mutation.profile_id,
                    profile_generation: outcome.mutation.profile_generation,
                    server_state_unknown: had_active,
                }
            }
            Err(failure) => {
                let error = finish_runtime_mutation_failure(application, failure).await;
                if application.is_config_uncertain() {
                    UiEvent::ConfigUncertain { operation_id }
                } else {
                    failed_profile_event(
                        operation_id,
                        profile_id,
                        previous_generation,
                        None,
                        OperationKind::DeleteProfile,
                        error.public_summary(),
                    )
                }
            }
        },
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_control(
    command: UiCommand,
    port: &ServicePort,
    application: &ApplicationService,
    message_tx: &mpsc::UnboundedSender<ControllerMessage>,
    global_permits: &Arc<Semaphore>,
    profile_permits: &mut ProfilePermitRegistry,
    registry: &mut TaskRegistry,
) {
    if application.is_config_uncertain() {
        let key = command.control_key();
        let _ = port.try_emit(UiEvent::ConfigUncertain {
            operation_id: command.operation_id(),
        });
        if let Some(key) = key
            && !matches!(key, ControlKey::Cancel(_))
        {
            port.release_control_key(&key);
        }
        return;
    }
    match command {
        UiCommand::CancelOperation { operation_id } => {
            if !registry.cancel(operation_id) {
                port.release_control_key(&ControlKey::Cancel(operation_id));
            }
        }
        UiCommand::DisconnectProfile {
            operation_id,
            profile_id,
            profile_generation,
        } => {
            let session_generation = application
                .cached_session_identity(&profile_id)
                .await
                .filter(|identity| identity.profile_generation == profile_generation)
                .map(|identity| identity.session_generation);
            start_profile_control(
                ProfileControlWork::Disconnect {
                    operation_id,
                    profile_id,
                    profile_generation,
                    session_generation,
                },
                port,
                application,
                message_tx,
                global_permits,
                profile_permits,
                registry,
            );
        }
        UiCommand::ReconnectProfile {
            operation_id,
            profile_id,
            profile_generation,
            timeout_ms,
        } => {
            if let Err(error) = application
                .ensure_profile_generation(&profile_id, profile_generation, operation_id)
                .await
            {
                let _ = port.try_emit(failed_from_service(
                    operation_id,
                    profile_id,
                    profile_generation,
                    None,
                    OperationKind::ReconnectProfile,
                    &error,
                ));
                return;
            }
            let session_generation = application
                .cached_session_identity(&profile_id)
                .await
                .filter(|identity| identity.profile_generation == profile_generation)
                .map(|identity| identity.session_generation);
            start_profile_control(
                ProfileControlWork::Reconnect {
                    operation_id,
                    profile_id,
                    profile_generation,
                    session_generation,
                    timeout: duration_from_millis(timeout_ms),
                },
                port,
                application,
                message_tx,
                global_permits,
                profile_permits,
                registry,
            );
        }
        other => {
            let _ = port.try_emit(failure_for_unavailable(
                other,
                PublicSummary::InternalFailure,
            ));
        }
    }
}

async fn reap_panicked_tasks(
    port: &ServicePort,
    application: &ApplicationService,
    message_tx: &mpsc::UnboundedSender<ControllerMessage>,
    registry: &mut TaskRegistry,
    mutation_active: &mut bool,
) {
    for reaped in registry.reap_finished().await {
        if reaped.class == TaskClass::Mutation {
            *mutation_active = false;
        }
        if !reaped.cleanup_started
            && start_registered_failure_cleanup(
                reaped.operation_id,
                reaped.scope,
                reaped.failure.clone(),
                reaped.terminal.clone(),
                application,
                message_tx,
                registry,
            )
        {
            continue;
        }
        if reaped.join_error.is_some() || reaped.cleanup_started {
            emit_internal_failure(port, reaped.operation_id, reaped.failure);
        }
        reaped.terminal.cancel();
    }
}

async fn cleanup_failed_scope(
    application: &ApplicationService,
    scope: &TaskScope,
    ready: Option<tokio::sync::oneshot::Sender<()>>,
) {
    let removed = if let TaskScope::Profile {
        profile_id,
        profile_generation,
        session_generation: Some(session_generation),
    } = scope
    {
        application
            .take_cached_session_exact(profile_id, *profile_generation, *session_generation)
            .await
    } else {
        None
    };
    if let Some(ready) = ready {
        let _ = ready.send(());
    }
    if let Some(removed) = removed {
        let _ = removed.close().await;
    }
}

fn emit_internal_failure(port: &ServicePort, operation_id: OperationId, failure: FailureContext) {
    let _ = port.try_emit(internal_failure_event(operation_id, failure));
}

fn internal_failure_event(operation_id: OperationId, failure: FailureContext) -> UiEvent {
    match failure {
        FailureContext::Profile {
            profile_id,
            profile_generation,
            kind,
        } => failed_profile_event(
            operation_id,
            profile_id,
            profile_generation,
            None,
            kind,
            PublicSummary::InternalFailure,
        ),
        FailureContext::Profiles => UiEvent::ProfilesFailed {
            operation_id,
            summary: PublicSummary::InternalFailure,
        },
        FailureContext::Draft { draft_id } => UiEvent::DraftOperationFailed {
            operation_id,
            draft_id,
            summary: PublicSummary::InternalFailure,
        },
    }
}

async fn snapshots(application: &ApplicationService) -> Result<Vec<ProfileSnapshot>, ServiceError> {
    let mut snapshots = Vec::new();
    for (profile, generation) in application.profiles_with_generations_snapshot().await {
        let profile_id = ProfileId(profile.id.clone());
        let has_current_session_secret = application.has_current_session_secret(&profile_id)?;
        snapshots.push(ProfileSnapshot::from_profile(
            &profile,
            generation,
            has_current_session_secret,
        ));
    }
    Ok(snapshots)
}

enum RedisResourceRequest {
    Scan(RedisScanRequest),
    Inspect(RedisKeyInspectRequest),
}

fn public_redis_resource_error(
    kind: OperationKind,
    profile_id: ProfileId,
    operation_id: OperationId,
    summary: PublicSummary,
    code: PublicCode,
) -> PublicOperationError {
    PublicOperationError::new_or_internal(
        kind,
        summary,
        code,
        &SafeContext::profile(profile_id, operation_id),
    )
}

fn connection_outcome_for_summary(summary: PublicSummary) -> ConnectionFailureOutcome {
    match summary {
        PublicSummary::CredentialRequired => ConnectionFailureOutcome::NeedsCredential,
        PublicSummary::AuthenticationFailed
        | PublicSummary::NetworkUnavailable
        | PublicSummary::TlsVerificationFailed => ConnectionFailureOutcome::Disconnected,
        PublicSummary::OperationCancelled | PublicSummary::OperationTimedOut => {
            ConnectionFailureOutcome::Unknown
        }
        _ => ConnectionFailureOutcome::Preserve,
    }
}

fn connection_outcome_for_disposition(disposition: SessionDisposition) -> ConnectionFailureOutcome {
    match disposition {
        SessionDisposition::Keep => ConnectionFailureOutcome::Preserve,
        SessionDisposition::Evict => ConnectionFailureOutcome::Disconnected,
    }
}

fn failed_redis_resource_event(
    request: RedisResourceRequest,
    error: PublicOperationError,
    session_generation: Option<SessionGeneration>,
    session_disposition: Option<SessionDisposition>,
    connection_outcome: ConnectionFailureOutcome,
) -> UiEvent {
    match request {
        RedisResourceRequest::Scan(request) => UiEvent::RedisKeysFailed {
            request,
            error,
            session_generation,
            session_disposition,
            connection_outcome,
        },
        RedisResourceRequest::Inspect(request) => UiEvent::RedisKeyInspectFailed {
            request,
            error,
            session_generation,
            session_disposition,
            connection_outcome,
        },
    }
}

fn failed_from_service(
    operation_id: OperationId,
    profile_id: ProfileId,
    profile_generation: ProfileGeneration,
    session_generation: Option<SessionGeneration>,
    kind: OperationKind,
    error: &ServiceError,
) -> UiEvent {
    failed_profile_event(
        operation_id,
        profile_id,
        profile_generation,
        session_generation,
        kind,
        error.public_summary(),
    )
}

fn failed_profile_event(
    operation_id: OperationId,
    profile_id: ProfileId,
    profile_generation: ProfileGeneration,
    session_generation: Option<SessionGeneration>,
    kind: OperationKind,
    summary: PublicSummary,
) -> UiEvent {
    let connection_outcome = match summary {
        PublicSummary::CredentialRequired => ConnectionFailureOutcome::NeedsCredential,
        PublicSummary::AuthenticationFailed
        | PublicSummary::NetworkUnavailable
        | PublicSummary::TlsVerificationFailed => ConnectionFailureOutcome::Disconnected,
        PublicSummary::OperationCancelled | PublicSummary::OperationTimedOut => {
            ConnectionFailureOutcome::Unknown
        }
        _ => ConnectionFailureOutcome::Preserve,
    };
    let session_disposition = session_generation.map(|_| match summary {
        PublicSummary::CredentialRequired
        | PublicSummary::AuthenticationFailed
        | PublicSummary::NetworkUnavailable
        | PublicSummary::TlsVerificationFailed
        | PublicSummary::OperationCancelled
        | PublicSummary::OperationTimedOut => SessionDisposition::Evict,
        _ => SessionDisposition::Keep,
    });
    UiEvent::OperationFailed {
        operation_id,
        profile_id,
        profile_generation,
        session_generation,
        kind,
        summary,
        session_disposition,
        connection_outcome,
    }
}

fn failed_profile_event_with_disposition(
    operation_id: OperationId,
    profile_id: ProfileId,
    profile_generation: ProfileGeneration,
    session_generation: SessionGeneration,
    kind: OperationKind,
    summary: PublicSummary,
    session_disposition: SessionDisposition,
) -> UiEvent {
    let connection_outcome = match session_disposition {
        SessionDisposition::Keep => ConnectionFailureOutcome::Preserve,
        SessionDisposition::Evict => ConnectionFailureOutcome::Disconnected,
    };
    UiEvent::OperationFailed {
        operation_id,
        profile_id,
        profile_generation,
        session_generation: Some(session_generation),
        kind,
        summary,
        session_disposition: Some(session_disposition),
        connection_outcome,
    }
}

fn failure_for_unavailable(command: UiCommand, summary: PublicSummary) -> UiEvent {
    match command {
        UiCommand::RefreshProfiles { operation_id } => UiEvent::ProfilesFailed {
            operation_id,
            summary,
        },
        UiCommand::BrowseCatalog(request) => catalog_failed_event(request, summary, None, None),
        UiCommand::ScanRedisKeys(request) => {
            let error = public_redis_resource_error(
                OperationKind::BrowseRedis,
                request.profile_id().clone(),
                request.operation_id(),
                summary,
                PublicCode::None,
            );
            failed_redis_resource_event(
                RedisResourceRequest::Scan(request),
                error,
                None,
                None,
                connection_outcome_for_summary(summary),
            )
        }
        UiCommand::InspectRedisKey(request) => {
            let error = public_redis_resource_error(
                OperationKind::InspectRedis,
                request.profile_id().clone(),
                request.operation_id(),
                summary,
                PublicCode::None,
            );
            failed_redis_resource_event(
                RedisResourceRequest::Inspect(request),
                error,
                None,
                None,
                connection_outcome_for_summary(summary),
            )
        }
        UiCommand::CreateProfile(request) => UiEvent::ProfileSaveFailed {
            operation_id: request.operation_id,
            profile_id: request
                .explicit_id
                .unwrap_or_else(|| ProfileId(format!("draft-{}", request.draft_id.0))),
            summary,
        },
        UiCommand::UpdateProfile(request) => UiEvent::ProfileSaveFailed {
            operation_id: request.operation_id,
            profile_id: request.profile_id,
            summary,
        },
        UiCommand::DeleteProfile(request) => failed_profile_event(
            request.operation_id,
            request.profile_id,
            request.expected_generation,
            None,
            OperationKind::DeleteProfile,
            summary,
        ),
        UiCommand::TestConnection {
            operation_id,
            profile_id,
            profile_generation,
            ..
        } => failed_profile_event(
            operation_id,
            profile_id,
            profile_generation,
            None,
            OperationKind::ConnectProfile,
            summary,
        ),
        UiCommand::TestDraftConnection(request) => UiEvent::DraftOperationFailed {
            operation_id: request.operation_id(),
            draft_id: request.draft_id(),
            summary,
        },
        UiCommand::Execute {
            operation_id,
            profile_id,
            profile_generation,
            ..
        } => UiEvent::ExecuteUnavailable {
            operation_id,
            profile_id,
            profile_generation,
            summary,
        },
        UiCommand::CancelOperation { operation_id }
        | UiCommand::ShutdownRuntime { operation_id } => UiEvent::ProfilesFailed {
            operation_id,
            summary,
        },
        UiCommand::DisconnectProfile {
            operation_id,
            profile_id,
            profile_generation,
        } => failed_profile_event(
            operation_id,
            profile_id,
            profile_generation,
            None,
            OperationKind::DisconnectProfile,
            summary,
        ),
        UiCommand::ReconnectProfile {
            operation_id,
            profile_id,
            profile_generation,
            ..
        } => failed_profile_event(
            operation_id,
            profile_id,
            profile_generation,
            None,
            OperationKind::ReconnectProfile,
            summary,
        ),
    }
}

fn duration_from_millis(timeout_ms: u64) -> Duration {
    Duration::from_millis(timeout_ms.max(1))
}

pub(super) fn commit_warning(state: CommitState) -> Option<PublicSummary> {
    match state {
        CommitState::NotCommitted => Some(PublicSummary::ConfigWriteNotCommitted),
        CommitState::Committed => None,
        CommitState::CommittedDurabilityUnknown => Some(PublicSummary::CommittedDurabilityUnknown),
    }
}
