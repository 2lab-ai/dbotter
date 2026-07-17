//! Bounded background controller for profile-scoped database operations.

use std::collections::{HashMap, HashSet, VecDeque};
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};
use tokio::task::{JoinError, JoinHandle};
use tokio_util::sync::CancellationToken;

use futures_util::FutureExt as _;

use crate::config::{CommitState, ConfigSourceVersion};
use crate::export_file::{ConfirmedDestination, ExportFileError, export_result_to_file};
use crate::model::{
    DraftId, ExportFormat, ExportResult, OperationId, OperationKind, OperationRecipeId,
    OverwritePolicy, ProfileGeneration, ProfileId, PublicCode, PublicSummary,
    RedisKeyInspectRequest, RedisScanRequest, ResultId, SessionGeneration,
};
use crate::public_error::{PublicOperationError, SafeContext};
use crate::service::{
    ApplicationService, RetainedBatchExecution, RuntimeCreateOutcome, RuntimeDeleteOutcome,
    RuntimeMutationFailure, RuntimeReloadOutcome, RuntimeUpdateOutcome, ServiceError,
    SessionDisposition, TestDraftRequest,
};
use crate::workspace::{
    ProfileWorkspaceSnapshot, WorkspaceReadOnlyReason, WorkspaceSnapshotError, WorkspaceStore,
    WorkspaceStoreError, WorkspaceStoreMode, WorkspaceStoreWarning,
};

use super::adapter::{ControlKey, DraftTestIntent, ServicePort, UiCommand};
use super::editor::{classify_execute_batch_operation, classify_execute_operation};
use super::model::{
    ConfigPresentation, ConnectionFailureOutcome, PostCloseState, ProfileSnapshot, UiEvent,
    WorkspaceAction, WorkspaceFailureCode, WorkspaceIdentity,
};

const GLOBAL_NETWORK_LIMIT: usize = 4;
pub(super) const PROCESS_EXPORT_LIMIT: usize = 2;
const SHUTDOWN_ASYNC_GRACE: Duration = Duration::from_secs(2);
const WORKSPACE_PENDING_CAPACITY: usize = super::adapter::WORKSPACE_CAPACITY;
const WORKSPACE_SHUTDOWN_PENDING_CAPACITY: usize =
    WORKSPACE_PENDING_CAPACITY + super::adapter::WORKSPACE_CAPACITY;
pub(super) static PROCESS_EXPORT_PERMITS: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(PROCESS_EXPORT_LIMIT)));

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
        kind: OperationKind,
    },
    Export {
        result_id: ResultId,
        format: ExportFormat,
        overwrite_policy: OverwritePolicy,
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
                kind: OperationKind::TestDraftConnection,
            },
            TaskScope::Export { result_id } => FailureContext::Export {
                result_id: *result_id,
                format: ExportFormat::Json,
                overwrite_policy: OverwritePolicy::DenyOverwrite,
            },
            TaskScope::Global => FailureContext::Profiles,
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

    fn active_export(&self, result_id: ResultId) -> Option<OperationId> {
        self.entries.values().find_map(|entry| {
            (entry.class == TaskClass::Export
                && matches!(&entry.task.scope, TaskScope::Export { result_id: active } if *active == result_id))
            .then_some(entry.task.operation_id)
        })
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

#[derive(Clone)]
enum WorkspaceBackend {
    Ready(Arc<WorkspaceStore>),
    Unavailable(WorkspaceFailureCode),
}

#[derive(Clone)]
struct WorkspaceOperationMeta {
    operation_id: OperationId,
    identity: WorkspaceIdentity,
    revision: u64,
    action: WorkspaceAction,
}

enum WorkspaceRequest {
    Load {
        operation_id: OperationId,
        identity: WorkspaceIdentity,
        base_revision: u64,
    },
    Commit {
        operation_id: OperationId,
        identity: WorkspaceIdentity,
        revision: u64,
        snapshot: Box<ProfileWorkspaceSnapshot>,
    },
    Clear {
        operation_id: OperationId,
        identity: WorkspaceIdentity,
        base_revision: u64,
    },
}

impl WorkspaceRequest {
    fn from_command(command: UiCommand) -> Option<Self> {
        match command {
            UiCommand::LoadWorkspace {
                operation_id,
                identity,
                base_revision,
            } => Some(Self::Load {
                operation_id,
                identity,
                base_revision,
            }),
            UiCommand::CommitWorkspace {
                operation_id,
                identity,
                revision,
                snapshot,
            } => Some(Self::Commit {
                operation_id,
                identity,
                revision,
                snapshot,
            }),
            UiCommand::ClearWorkspace {
                operation_id,
                identity,
                base_revision,
            } => Some(Self::Clear {
                operation_id,
                identity,
                base_revision,
            }),
            _ => None,
        }
    }

    fn meta(&self) -> WorkspaceOperationMeta {
        match self {
            Self::Load {
                operation_id,
                identity,
                base_revision,
            } => WorkspaceOperationMeta {
                operation_id: *operation_id,
                identity: identity.clone(),
                revision: *base_revision,
                action: WorkspaceAction::Load,
            },
            Self::Commit {
                operation_id,
                identity,
                revision,
                ..
            } => WorkspaceOperationMeta {
                operation_id: *operation_id,
                identity: identity.clone(),
                revision: *revision,
                action: WorkspaceAction::Commit,
            },
            Self::Clear {
                operation_id,
                identity,
                base_revision,
            } => WorkspaceOperationMeta {
                operation_id: *operation_id,
                identity: identity.clone(),
                revision: *base_revision,
                action: WorkspaceAction::Clear,
            },
        }
    }

    fn is_commit_for(&self, target: &WorkspaceIdentity) -> bool {
        matches!(
            self,
            Self::Commit { identity, .. } if identity == target
        )
    }

    fn is_barrier_for(&self, target: &WorkspaceIdentity) -> bool {
        matches!(
            self,
            Self::Load { identity, .. } | Self::Clear { identity, .. }
                if identity == target
        )
    }

    fn snapshot_identity_is_valid(&self) -> bool {
        match self {
            Self::Commit {
                identity, snapshot, ..
            } => {
                snapshot.instance_id() == identity.instance_id()
                    && snapshot.profile_id() == identity.profile_id()
            }
            Self::Load { .. } | Self::Clear { .. } => true,
        }
    }
}

struct ReservedWorkspaceRequest {
    reservation: TaskReservation,
    request: WorkspaceRequest,
}

struct ActiveWorkspaceOperation {
    reservation: TaskReservation,
    meta: WorkspaceOperationMeta,
    completion_sent: Arc<AtomicBool>,
    join: JoinHandle<()>,
}

enum WorkspaceStoreOutput {
    Loaded {
        mode: WorkspaceStoreMode,
        read_only_reason: Option<WorkspaceReadOnlyReason>,
        snapshot: Option<Box<ProfileWorkspaceSnapshot>>,
    },
    Committed {
        generation: u64,
        warnings: Vec<WorkspaceStoreWarning>,
    },
    Cleared,
}

type WorkspaceExecutionResult = Result<WorkspaceStoreOutput, WorkspaceFailureCode>;

struct WorkspaceCoordinator {
    backend: WorkspaceBackend,
    active: Option<ActiveWorkspaceOperation>,
    pending: VecDeque<ReservedWorkspaceRequest>,
}

impl WorkspaceCoordinator {
    fn new(backend: WorkspaceBackend) -> Self {
        Self {
            backend,
            active: None,
            pending: VecDeque::with_capacity(WORKSPACE_PENDING_CAPACITY),
        }
    }

    fn is_idle(&self) -> bool {
        self.active.is_none() && self.pending.is_empty()
    }

    #[allow(clippy::too_many_arguments)]
    async fn enqueue(
        &mut self,
        command: UiCommand,
        pending_limit: usize,
        port: &ServicePort,
        application: &ApplicationService,
        message_tx: &mpsc::UnboundedSender<ControllerMessage>,
        registry: &mut TaskRegistry,
    ) {
        let command_operation_id = command.operation_id();
        let request = match WorkspaceRequest::from_command(command) {
            Some(request) => request,
            None => {
                let _ = port
                    .emit(profiles_failed_event(
                        command_operation_id,
                        PublicSummary::InternalFailure,
                        PublicCode::None,
                    ))
                    .await;
                return;
            }
        };
        let meta = request.meta();
        let reservation = match registry.reserve(meta.operation_id) {
            Ok(reservation) => reservation,
            Err(()) => {
                let _ = port
                    .emit(workspace_failure_event(meta, WorkspaceFailureCode::Busy))
                    .await;
                return;
            }
        };
        if !request.snapshot_identity_is_valid() {
            registry.release_reservation(reservation);
            let _ = port
                .emit(workspace_failure_event(
                    meta,
                    WorkspaceFailureCode::InvalidIdentity,
                ))
                .await;
            return;
        }

        let mut reserved = ReservedWorkspaceRequest {
            reservation,
            request,
        };
        match reserved.request.meta().action {
            WorkspaceAction::Commit => {
                match self.replace_pending_commit(reserved, port, registry).await {
                    Ok(()) => return,
                    Err(not_replaced) => {
                        let replacement_meta = not_replaced.request.meta();
                        if let Some(active_meta) = self.active_commit_superseding(&replacement_meta)
                        {
                            registry.release_reservation(not_replaced.reservation);
                            let _ = port
                                .emit(workspace_superseded_event(replacement_meta, &active_meta))
                                .await;
                            return;
                        }
                        reserved = not_replaced;
                    }
                }
            }
            WorkspaceAction::Clear => {
                self.supersede_pending_commits_for_clear(&reserved.request.meta(), port, registry)
                    .await;
            }
            WorkspaceAction::Load => {}
        }
        if self.pending.len() >= pending_limit {
            let meta = reserved.request.meta();
            registry.release_reservation(reserved.reservation);
            let _ = port
                .emit(workspace_failure_event(meta, WorkspaceFailureCode::Busy))
                .await;
            return;
        }
        self.pending.push_back(reserved);
        self.start_next(application, message_tx);
    }

    async fn replace_pending_commit(
        &mut self,
        replacement: ReservedWorkspaceRequest,
        port: &ServicePort,
        registry: &mut TaskRegistry,
    ) -> Result<(), ReservedWorkspaceRequest> {
        let replacement_meta = replacement.request.meta();
        let mut replace_index = None;
        for index in (0..self.pending.len()).rev() {
            let Some(candidate) = self.pending.get(index) else {
                continue;
            };
            if candidate.request.is_barrier_for(&replacement_meta.identity) {
                break;
            }
            if candidate.request.is_commit_for(&replacement_meta.identity) {
                replace_index = Some(index);
                break;
            }
        }
        let Some(index) = replace_index else {
            return Err(replacement);
        };
        let Some(slot) = self.pending.get_mut(index) else {
            return Err(replacement);
        };
        let retained_meta = slot.request.meta();
        if replacement_meta.revision <= retained_meta.revision {
            registry.release_reservation(replacement.reservation);
            let _ = port
                .emit(workspace_superseded_event(replacement_meta, &retained_meta))
                .await;
            return Ok(());
        }
        let superseded = std::mem::replace(slot, replacement);
        let superseded_meta = superseded.request.meta();
        registry.release_reservation(superseded.reservation);
        let _ = port
            .emit(workspace_superseded_event(
                superseded_meta,
                &replacement_meta,
            ))
            .await;
        Ok(())
    }

    fn active_commit_superseding(
        &self,
        replacement: &WorkspaceOperationMeta,
    ) -> Option<WorkspaceOperationMeta> {
        for pending in self.pending.iter().rev() {
            if pending.request.is_barrier_for(&replacement.identity) {
                return None;
            }
            if pending.request.is_commit_for(&replacement.identity) {
                return None;
            }
        }
        self.active
            .as_ref()
            .filter(|active| {
                active.meta.action == WorkspaceAction::Commit
                    && active.meta.identity == replacement.identity
                    && active.meta.revision >= replacement.revision
            })
            .map(|active| active.meta.clone())
    }

    async fn supersede_pending_commits_for_clear(
        &mut self,
        clear: &WorkspaceOperationMeta,
        port: &ServicePort,
        registry: &mut TaskRegistry,
    ) {
        let mut index = self
            .pending
            .iter()
            .rposition(|pending| pending.request.is_barrier_for(&clear.identity))
            .map_or(0, |barrier| barrier.saturating_add(1));
        while index < self.pending.len() {
            let should_remove = self
                .pending
                .get(index)
                .is_some_and(|pending| pending.request.is_commit_for(&clear.identity));
            if !should_remove {
                index += 1;
                continue;
            }
            let Some(superseded) = self.pending.remove(index) else {
                continue;
            };
            let superseded_meta = superseded.request.meta();
            registry.release_reservation(superseded.reservation);
            let _ = port
                .emit(workspace_superseded_event(superseded_meta, clear))
                .await;
        }
    }

    fn start_next(
        &mut self,
        application: &ApplicationService,
        message_tx: &mpsc::UnboundedSender<ControllerMessage>,
    ) {
        if self.active.is_some() {
            return;
        }
        let Some(reserved) = self.pending.pop_front() else {
            return;
        };
        let meta = reserved.request.meta();
        let backend = self.backend.clone();
        let service = application.clone();
        let messages = message_tx.clone();
        let completion_sent = Arc::new(AtomicBool::new(false));
        let task_completion_sent = completion_sent.clone();
        let operation_id = meta.operation_id;
        let join = tokio::spawn(async move {
            let result = execute_workspace_request(backend, service, reserved.request).await;
            let sent = messages
                .send(ControllerMessage::WorkspaceCompleted {
                    operation_id,
                    result,
                })
                .is_ok();
            task_completion_sent.store(sent, Ordering::Release);
        });
        self.active = Some(ActiveWorkspaceOperation {
            reservation: reserved.reservation,
            meta,
            completion_sent,
            join,
        });
    }

    async fn complete(
        &mut self,
        operation_id: OperationId,
        result: WorkspaceExecutionResult,
        port: &ServicePort,
        application: &ApplicationService,
        message_tx: &mpsc::UnboundedSender<ControllerMessage>,
        registry: &mut TaskRegistry,
    ) {
        if self
            .active
            .as_ref()
            .is_none_or(|active| active.meta.operation_id != operation_id)
        {
            return;
        }
        let Some(active) = self.active.take() else {
            return;
        };
        let join_failed = active.join.await.is_err();
        registry.release_reservation(active.reservation);
        let event = if join_failed {
            workspace_failure_event(active.meta, WorkspaceFailureCode::Internal)
        } else {
            workspace_terminal_event(active.meta, result)
        };
        let _ = port.emit(event).await;
        self.start_next(application, message_tx);
    }

    async fn reap_panicked(
        &mut self,
        port: &ServicePort,
        application: &ApplicationService,
        message_tx: &mpsc::UnboundedSender<ControllerMessage>,
        registry: &mut TaskRegistry,
    ) {
        let should_reap = self.active.as_ref().is_some_and(|active| {
            active.join.is_finished() && !active.completion_sent.load(Ordering::Acquire)
        });
        if !should_reap {
            return;
        }
        let Some(active) = self.active.take() else {
            return;
        };
        let _ = active.join.await;
        registry.release_reservation(active.reservation);
        let _ = port
            .emit(workspace_failure_event(
                active.meta,
                WorkspaceFailureCode::Internal,
            ))
            .await;
        self.start_next(application, message_tx);
    }
}

async fn open_workspace_backend(config_path: PathBuf) -> WorkspaceBackend {
    match tokio::task::spawn_blocking(move || WorkspaceStore::open(&config_path)).await {
        Ok(Ok(store)) => WorkspaceBackend::Ready(Arc::new(store)),
        Ok(Err(error)) => WorkspaceBackend::Unavailable(workspace_store_failure(error, None)),
        Err(_) => WorkspaceBackend::Unavailable(WorkspaceFailureCode::Internal),
    }
}

async fn workspace_identity_is_current(
    application: &ApplicationService,
    identity: &WorkspaceIdentity,
) -> bool {
    if application.is_config_uncertain()
        || application.source_version().await != ConfigSourceVersion::V3
    {
        return false;
    }
    application
        .profiles_with_generations_snapshot()
        .await
        .into_iter()
        .any(|(profile, generation)| {
            profile.id == identity.profile_id().as_str()
                && generation == identity.profile_generation()
                && profile.safety.instance_id() == Some(identity.instance_id())
        })
}

async fn execute_workspace_request(
    backend: WorkspaceBackend,
    application: ApplicationService,
    request: WorkspaceRequest,
) -> WorkspaceExecutionResult {
    let identity = request.meta().identity;
    if !workspace_identity_is_current(&application, &identity).await {
        return Err(WorkspaceFailureCode::Stale);
    }
    let store = match backend {
        WorkspaceBackend::Ready(store) => store,
        WorkspaceBackend::Unavailable(code) => return Err(code),
    };
    let blocking_store = store.clone();
    let result = match tokio::task::spawn_blocking(move || {
        run_workspace_store_request(&blocking_store, request)
    })
    .await
    {
        Ok(result) => result,
        Err(_) => Err(WorkspaceFailureCode::Internal),
    };
    finalize_workspace_result(&application, &identity, result).await
}

async fn finalize_workspace_result(
    application: &ApplicationService,
    identity: &WorkspaceIdentity,
    result: WorkspaceExecutionResult,
) -> WorkspaceExecutionResult {
    if result.is_ok() && !workspace_identity_is_current(application, identity).await {
        return Err(WorkspaceFailureCode::Stale);
    }
    result
}

fn run_workspace_store_request(
    store: &WorkspaceStore,
    request: WorkspaceRequest,
) -> WorkspaceExecutionResult {
    match request {
        WorkspaceRequest::Load { identity, .. } => {
            let snapshot = store
                .load(identity.instance_id())
                .map_err(|error| workspace_store_failure(error, store.read_only_reason()))?;
            if snapshot.as_ref().is_some_and(|snapshot| {
                snapshot.instance_id() != identity.instance_id()
                    || snapshot.profile_id() != identity.profile_id()
            }) {
                return Err(WorkspaceFailureCode::InvalidIdentity);
            }
            Ok(WorkspaceStoreOutput::Loaded {
                mode: store.mode(),
                read_only_reason: store.read_only_reason(),
                snapshot: snapshot.map(Box::new),
            })
        }
        WorkspaceRequest::Commit { snapshot, .. } => {
            let commit = store
                .commit(&snapshot)
                .map_err(|error| workspace_store_failure(error, store.read_only_reason()))?;
            Ok(WorkspaceStoreOutput::Committed {
                generation: commit.generation(),
                warnings: commit.warnings().to_vec(),
            })
        }
        WorkspaceRequest::Clear { identity, .. } => {
            store
                .clear(identity.instance_id())
                .map_err(|error| workspace_store_failure(error, store.read_only_reason()))?;
            Ok(WorkspaceStoreOutput::Cleared)
        }
    }
}

fn workspace_store_failure(
    error: WorkspaceStoreError,
    read_only_reason: Option<WorkspaceReadOnlyReason>,
) -> WorkspaceFailureCode {
    match error {
        WorkspaceStoreError::InvalidConfigPath | WorkspaceStoreError::WriterUnavailable => {
            WorkspaceFailureCode::Unavailable
        }
        WorkspaceStoreError::ReadOnly => read_only_reason.map_or(
            WorkspaceFailureCode::Unavailable,
            WorkspaceFailureCode::ReadOnly,
        ),
        WorkspaceStoreError::UnsafePath => WorkspaceFailureCode::UnsafeStorage,
        WorkspaceStoreError::CorruptManifest | WorkspaceStoreError::CorruptShard => {
            WorkspaceFailureCode::Corrupt
        }
        WorkspaceStoreError::UnsupportedVersion => WorkspaceFailureCode::UnsupportedVersion,
        WorkspaceStoreError::Snapshot(snapshot) => workspace_snapshot_failure(snapshot),
        WorkspaceStoreError::ShardTooLarge | WorkspaceStoreError::StoreTooLarge => {
            WorkspaceFailureCode::LimitExceeded
        }
        WorkspaceStoreError::ExternalChange => WorkspaceFailureCode::ExternalChange,
        WorkspaceStoreError::DurabilityUnknown => WorkspaceFailureCode::DurabilityUnknown,
        WorkspaceStoreError::RecoveryRequired => WorkspaceFailureCode::RecoveryRequired,
        WorkspaceStoreError::Io(kind) => WorkspaceFailureCode::Io(kind),
    }
}

fn workspace_snapshot_failure(error: WorkspaceSnapshotError) -> WorkspaceFailureCode {
    match error {
        WorkspaceSnapshotError::EditorSourceTooLarge
        | WorkspaceSnapshotError::DatabaseBindingTooLarge
        | WorkspaceSnapshotError::TooManyEditorTabs
        | WorkspaceSnapshotError::TooManyEditorTabsTotal
        | WorkspaceSnapshotError::TooManyHistoryEntries
        | WorkspaceSnapshotError::TooManyHistoryEntriesTotal => WorkspaceFailureCode::LimitExceeded,
        WorkspaceSnapshotError::InvalidEditorId
        | WorkspaceSnapshotError::InvalidEditorTitle
        | WorkspaceSnapshotError::InvalidEditorCursor
        | WorkspaceSnapshotError::InvalidEditorSelection
        | WorkspaceSnapshotError::InvalidHistoryId
        | WorkspaceSnapshotError::InvalidHistorySource
        | WorkspaceSnapshotError::InvalidGeometry
        | WorkspaceSnapshotError::InvalidProfileId
        | WorkspaceSnapshotError::DisabledPersistenceHasContent
        | WorkspaceSnapshotError::DuplicateEditorId
        | WorkspaceSnapshotError::UnknownSelectedEditor
        | WorkspaceSnapshotError::DuplicateHistoryId
        | WorkspaceSnapshotError::DuplicateProfileInstance => WorkspaceFailureCode::InvalidSnapshot,
    }
}

fn workspace_terminal_event(
    meta: WorkspaceOperationMeta,
    result: WorkspaceExecutionResult,
) -> UiEvent {
    match (meta.action, result) {
        (
            WorkspaceAction::Load,
            Ok(WorkspaceStoreOutput::Loaded {
                mode,
                read_only_reason,
                snapshot,
            }),
        ) => UiEvent::WorkspaceLoaded {
            operation_id: meta.operation_id,
            identity: meta.identity,
            base_revision: meta.revision,
            mode,
            read_only_reason,
            snapshot,
        },
        (
            WorkspaceAction::Commit,
            Ok(WorkspaceStoreOutput::Committed {
                generation,
                warnings,
            }),
        ) => UiEvent::WorkspaceCommitted {
            operation_id: meta.operation_id,
            identity: meta.identity,
            revision: meta.revision,
            generation,
            warnings,
        },
        (WorkspaceAction::Clear, Ok(WorkspaceStoreOutput::Cleared)) => UiEvent::WorkspaceCleared {
            operation_id: meta.operation_id,
            identity: meta.identity,
            base_revision: meta.revision,
        },
        (_, Err(code)) => workspace_failure_event(meta, code),
        _ => workspace_failure_event(meta, WorkspaceFailureCode::Internal),
    }
}

fn workspace_failure_event(meta: WorkspaceOperationMeta, code: WorkspaceFailureCode) -> UiEvent {
    UiEvent::WorkspaceOperationFailed {
        operation_id: meta.operation_id,
        identity: meta.identity,
        revision: meta.revision,
        action: meta.action,
        code,
    }
}

fn workspace_superseded_event(
    superseded: WorkspaceOperationMeta,
    replacement: &WorkspaceOperationMeta,
) -> UiEvent {
    UiEvent::WorkspaceCommitSuperseded {
        operation_id: superseded.operation_id,
        identity: superseded.identity,
        revision: superseded.revision,
        superseded_by: replacement.operation_id,
        superseded_by_revision: replacement.revision,
    }
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
            let service_path = config_path.clone();
            match tokio::task::spawn_blocking(move || ApplicationService::load_path(service_path))
                .await
            {
                Ok(Ok(application)) => {
                    let workspace = open_workspace_backend(config_path).await;
                    run_controller(service_port, application, workspace).await;
                }
                Ok(Err(error)) => run_unavailable(service_port, error.public_summary()).await,
                Err(_) => run_unavailable(service_port, PublicSummary::InternalFailure).await,
            }
        }),
    }
}

pub fn spawn_with_service(
    service_port: ServicePort,
    application: ApplicationService,
) -> RuntimeHandle {
    let config_path = application.config_path().to_owned();
    RuntimeHandle {
        join: tokio::spawn(async move {
            let workspace = open_workspace_backend(config_path).await;
            run_controller(service_port, application, workspace).await;
        }),
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
    WorkspaceCompleted {
        operation_id: OperationId,
        result: WorkspaceExecutionResult,
    },
}

struct ReloadPresentation {
    outcome: RuntimeReloadOutcome,
    profiles: Vec<ProfileSnapshot>,
    config: ConfigPresentation,
}

enum TaskOutput {
    Event(Box<UiEvent>),
    Reload {
        operation_id: OperationId,
        result: Box<Result<ReloadPresentation, ServiceError>>,
    },
    Create {
        operation_id: OperationId,
        draft_id: DraftId,
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
        editor_tab_id: Option<super::model::EditorTabId>,
    },
    ExecuteBatch {
        request: crate::model::ExecuteBatchRequest,
        kind: OperationKind,
        editor_tab_id: Option<super::model::EditorTabId>,
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

enum DraftWorkInput {
    Prepared(TestDraftRequest),
    Intent(DraftTestIntent),
}

impl DraftWorkInput {
    fn operation_id(&self) -> OperationId {
        match self {
            Self::Prepared(request) => request.operation_id(),
            Self::Intent(intent) => intent.operation_id(),
        }
    }

    fn draft_id(&self) -> DraftId {
        match self {
            Self::Prepared(request) => request.draft_id(),
            Self::Intent(intent) => intent.draft_id(),
        }
    }
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
            Self::Execute { request, kind, .. } => (
                request.operation_id,
                &request.profile_id,
                request.profile_generation,
                *kind,
            ),
            Self::ExecuteBatch { request, kind, .. } => (
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

async fn run_controller(
    mut port: ServicePort,
    application: ApplicationService,
    workspace_backend: WorkspaceBackend,
) {
    let (message_tx, mut message_rx) = mpsc::unbounded_channel();
    let global_permits = Arc::new(Semaphore::new(GLOBAL_NETWORK_LIMIT));
    let mut profile_permits = ProfilePermitRegistry::default();
    let mut draft_permits = DraftPermitRegistry::default();
    let mut registry = TaskRegistry::default();
    let mut workspace = WorkspaceCoordinator::new(workspace_backend);
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
                    &mut workspace,
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
            Some(command) = port.workspace_rx.recv() => {
                workspace.enqueue(
                    command,
                    WORKSPACE_PENDING_CAPACITY,
                    &port,
                    &application,
                    &message_tx,
                    &mut registry,
                ).await;
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
                workspace.reap_panicked(
                    &port,
                    &application,
                    &message_tx,
                    &mut registry,
                ).await;
                profile_permits.prune_idle();
                draft_permits.prune_idle();
            }
        }
    };

    let queued_workspace = port.close_and_drain_for_shutdown();
    for command in queued_workspace {
        workspace
            .enqueue(
                command,
                WORKSPACE_SHUTDOWN_PENDING_CAPACITY,
                &port,
                &application,
                &message_tx,
                &mut registry,
            )
            .await;
    }
    finish_controller_shutdown(
        &port,
        &application,
        &message_tx,
        &mut message_rx,
        &mut registry,
        &mut mutation_active,
        &mut workspace,
    )
    .await;
    application.shutdown_runtime().await;
    drop(workspace);
    let _ = port
        .emit(UiEvent::RuntimeShutdown {
            operation_id: shutdown_operation,
        })
        .await;
}

async fn finish_controller_shutdown(
    port: &ServicePort,
    application: &ApplicationService,
    message_tx: &mpsc::UnboundedSender<ControllerMessage>,
    message_rx: &mut mpsc::UnboundedReceiver<ControllerMessage>,
    registry: &mut TaskRegistry,
    mutation_active: &mut bool,
    workspace: &mut WorkspaceCoordinator,
) {
    registry.cancel_all();
    let deadline = tokio::time::Instant::now() + SHUTDOWN_ASYNC_GRACE;
    let mut reap_tick = tokio::time::interval(Duration::from_millis(5));
    // Workspace commits and clears are durability barriers: once admitted they
    // are joined without the network grace timeout so shutdown cannot report
    // completion while an accepted save is still queued or writing.
    while !registry.is_empty_runtime() || !workspace.is_idle() {
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
                    workspace,
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
                workspace.reap_panicked(
                    port,
                    application,
                    message_tx,
                    registry,
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
            Some(command) = port.workspace_rx.recv() => {
                let _ = port.emit(failure_for_unavailable(command, summary)).await;
            }
            Some(command) = port.work_rx.recv() => {
                let _ = port.try_emit(failure_for_unavailable(command, summary));
            }
        }
    };
    for command in port.close_and_drain_for_shutdown() {
        let _ = port.emit(failure_for_unavailable(command, summary)).await;
    }
    let _ = port.emit(UiEvent::RuntimeShutdown { operation_id }).await;
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
    let command = match command {
        UiCommand::ExportResult {
            request,
            confirmation,
        } => {
            start_export_work(request, confirmation, port, message_tx, registry);
            return;
        }
        other => other,
    };
    if application.is_config_uncertain() {
        let _ = port.try_emit(UiEvent::ConfigUncertain {
            operation_id: command.operation_id(),
        });
        return;
    }
    let work = match command {
        UiCommand::TestDraftConnection(request) => {
            start_draft_work(
                DraftWorkInput::Prepared(request),
                port,
                application,
                message_tx,
                global_permits,
                draft_permits,
                registry,
            );
            return;
        }
        UiCommand::PrepareDraftConnectionTest(intent) => {
            start_draft_work(
                DraftWorkInput::Intent(intent),
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
            editor_tab_id,
            language,
            text,
            row_limit,
            timeout_ms,
        } => {
            let kind = classify_execute_operation(language, &text);
            ProfileWork::Execute {
                request: crate::model::ExecuteRequest {
                    operation_id,
                    profile_id,
                    profile_generation,
                    language,
                    text,
                    row_limit,
                    timeout: duration_from_millis(timeout_ms),
                },
                kind,
                editor_tab_id,
            }
        }
        UiCommand::ExecuteBatch {
            operation_id,
            profile_id,
            profile_generation,
            editor_tab_id,
            language,
            text,
            row_limit,
            timeout_ms,
        } => {
            let kind = classify_execute_batch_operation(language, &text, row_limit, timeout_ms);
            ProfileWork::ExecuteBatch {
                request: crate::model::ExecuteBatchRequest {
                    operation_id,
                    profile_id,
                    profile_generation,
                    language,
                    text,
                    row_limit,
                    timeout: duration_from_millis(timeout_ms),
                },
                kind,
                editor_tab_id,
            }
        }
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

fn start_export_work(
    request: ExportResult,
    confirmation: Option<ConfirmedDestination>,
    port: &ServicePort,
    message_tx: &mpsc::UnboundedSender<ControllerMessage>,
    registry: &mut TaskRegistry,
) {
    let operation_id = request.operation_id;
    let result_id = request.result_id;
    let format = request.format;
    let overwrite_policy = request.overwrite_policy;
    if registry.active_export(result_id).is_some() {
        let _ = port.try_emit(result_export_failed_event(
            operation_id,
            result_id,
            format,
            overwrite_policy,
            PublicSummary::ResourceBusy,
            PublicCode::None,
            false,
        ));
        return;
    }
    let reservation = match registry.reserve(operation_id) {
        Ok(reservation) => reservation,
        Err(()) => {
            let _ = port.try_emit(result_export_failed_event(
                operation_id,
                result_id,
                format,
                overwrite_policy,
                PublicSummary::ResourceBusy,
                PublicCode::None,
                false,
            ));
            return;
        }
    };
    let export_permit = Arc::clone(&PROCESS_EXPORT_PERMITS).try_acquire_owned();
    let Ok(export_permit) = export_permit else {
        registry.release_reservation(reservation);
        let _ = port.try_emit(result_export_failed_event(
            operation_id,
            result_id,
            format,
            overwrite_policy,
            PublicSummary::ResourceBusy,
            PublicCode::None,
            false,
        ));
        return;
    };

    let cancel = CancellationToken::new();
    let task_cancel = cancel.clone();
    let messages = message_tx.clone();
    let completion_sent = Arc::new(AtomicBool::new(false));
    let task_completion_sent = completion_sent.clone();
    let (start_tx, start_rx) = tokio::sync::oneshot::channel();
    let join = tokio::spawn(async move {
        if start_rx.await.is_err() {
            return;
        }
        let worker = tokio::task::spawn_blocking(move || {
            export_result_to_file(&request, confirmation.as_ref(), || {
                task_cancel.is_cancelled()
            })
        });
        let event = match worker.await {
            Ok(Ok(outcome)) => UiEvent::ResultExported {
                operation_id,
                result_id,
                format: outcome.format,
                overwrite_policy: outcome.overwrite_policy,
                row_count: outcome.row_count,
                bytes_written: outcome.bytes_written,
            },
            Ok(Err(error)) => result_export_file_error_event(
                operation_id,
                result_id,
                format,
                overwrite_policy,
                &error,
            ),
            Err(_join_error) => result_export_failed_event(
                operation_id,
                result_id,
                format,
                overwrite_policy,
                PublicSummary::InternalFailure,
                PublicCode::None,
                false,
            ),
        };
        drop(export_permit);
        task_completion_sent.store(true, Ordering::Release);
        let _ = messages.send(ControllerMessage::Completed {
            operation_id,
            output: Box::new(TaskOutput::Event(Box::new(event))),
        });
    });
    let task = RegisteredTask {
        operation_id,
        scope: TaskScope::Export { result_id },
        cancel,
        join,
    };
    match registry.commit_reservation(
        reservation,
        task,
        TaskClass::Export,
        FailureContext::Export {
            result_id,
            format,
            overwrite_policy,
        },
        completion_sent,
    ) {
        Ok(()) => {
            let _ = start_tx.send(());
        }
        Err(task) => {
            drop(start_tx);
            task.join.abort();
            let _ = port.try_emit(result_export_failed_event(
                operation_id,
                result_id,
                format,
                overwrite_policy,
                PublicSummary::ResourceBusy,
                PublicCode::None,
                false,
            ));
        }
    }
}

fn result_export_file_error_event(
    operation_id: OperationId,
    result_id: ResultId,
    format: ExportFormat,
    overwrite_policy: OverwritePolicy,
    error: &ExportFileError,
) -> UiEvent {
    let (summary, code, destination_committed) = match error {
        ExportFileError::Cancelled => (PublicSummary::OperationCancelled, PublicCode::None, false),
        ExportFileError::DestinationExists
        | ExportFileError::InvalidDestinationType
        | ExportFileError::ConfirmationRequired
        | ExportFileError::ConfirmationMismatch
        | ExportFileError::DestinationChanged
        | ExportFileError::ResultIdentityMismatch => (
            PublicSummary::InvalidInput,
            PublicCode::ExportDestination,
            false,
        ),
        ExportFileError::Encode { .. } | ExportFileError::NotCommitted { .. } => {
            (PublicSummary::ExportFailed, PublicCode::None, false)
        }
        ExportFileError::CommittedDurabilityUnknown { .. } => (
            PublicSummary::CommittedDurabilityUnknown,
            PublicCode::ExportDestinationCommitted,
            true,
        ),
    };
    result_export_failed_event(
        operation_id,
        result_id,
        format,
        overwrite_policy,
        summary,
        code,
        destination_committed,
    )
}

#[allow(clippy::too_many_arguments)]
fn result_export_failed_event(
    operation_id: OperationId,
    result_id: ResultId,
    format: ExportFormat,
    overwrite_policy: OverwritePolicy,
    summary: PublicSummary,
    code: PublicCode,
    destination_committed: bool,
) -> UiEvent {
    let error = PublicOperationError::new_or_internal(
        OperationKind::ExportResult,
        summary,
        code,
        &SafeContext::export(result_id, operation_id, destination_committed),
    );
    UiEvent::ResultExportFailed {
        operation_id,
        result_id,
        format,
        overwrite_policy,
        summary,
        error,
        destination_committed,
    }
}

#[allow(clippy::too_many_arguments)]
fn start_draft_work(
    input: DraftWorkInput,
    port: &ServicePort,
    application: &ApplicationService,
    message_tx: &mpsc::UnboundedSender<ControllerMessage>,
    global_permits: &Arc<Semaphore>,
    draft_permits: &mut DraftPermitRegistry,
    registry: &mut TaskRegistry,
) {
    let operation_id = input.operation_id();
    let draft_id = input.draft_id();
    let reservation = match registry.reserve(operation_id) {
        Ok(reservation) => reservation,
        Err(()) => {
            let _ = port.try_emit(draft_failed_event(
                operation_id,
                draft_id,
                OperationKind::TestDraftConnection,
                PublicSummary::ResourceBusy,
                PublicCode::None,
            ));
            return;
        }
    };
    let Ok(draft_permit) = draft_permits.try_acquire(draft_id) else {
        registry.release_reservation(reservation);
        let _ = port.try_emit(draft_failed_event(
            operation_id,
            draft_id,
            OperationKind::TestDraftConnection,
            PublicSummary::ResourceBusy,
            PublicCode::None,
        ));
        return;
    };
    let Ok(global_permit) = global_permits.clone().try_acquire_owned() else {
        drop(draft_permit);
        registry.release_reservation(reservation);
        let _ = port.try_emit(draft_failed_event(
            operation_id,
            draft_id,
            OperationKind::TestDraftConnection,
            PublicSummary::ResourceBusy,
            PublicCode::None,
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
        let event = match input {
            DraftWorkInput::Prepared(request) => {
                run_draft_work(&service, request, &task_cancel).await
            }
            DraftWorkInput::Intent(intent) => {
                match prepare_draft_test_intent(&service, intent).await {
                    Ok(request) => run_draft_work(&service, request, &task_cancel).await,
                    Err((failed_draft, failed_operation, error)) => {
                        let (summary, code) = error.public_error_parts();
                        draft_failed_event(
                            failed_operation,
                            failed_draft,
                            OperationKind::TestDraftConnection,
                            summary,
                            code,
                        )
                    }
                }
            }
        };
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
        FailureContext::Draft {
            draft_id,
            kind: OperationKind::TestDraftConnection,
        },
        completion_sent,
    ) {
        Ok(()) => {
            let _ = start_tx.send(());
        }
        Err(task) => {
            drop(start_tx);
            task.join.abort();
            let _ = port.try_emit(draft_failed_event(
                operation_id,
                draft_id,
                OperationKind::TestDraftConnection,
                PublicSummary::ResourceBusy,
                PublicCode::None,
            ));
        }
    }
}

async fn prepare_draft_test_intent(
    service: &ApplicationService,
    intent: DraftTestIntent,
) -> Result<TestDraftRequest, (DraftId, OperationId, ServiceError)> {
    let draft_id = intent.draft_id();
    let operation_id = intent.operation_id();
    let result = match intent {
        DraftTestIntent::Secretless { draft, timeout, .. } => {
            service.prepare_secretless_draft_test(draft_id, operation_id, draft, timeout)
        }
        DraftTestIntent::SessionReplace {
            draft,
            secret,
            timeout,
            ..
        } => service.prepare_replacement_secret_draft_test(
            draft_id,
            operation_id,
            draft,
            secret,
            timeout,
        ),
        DraftTestIntent::SessionKeep {
            profile_id,
            profile_generation,
            draft,
            timeout,
            ..
        } => {
            service
                .prepare_keep_current_draft_test(
                    profile_id,
                    profile_generation,
                    draft_id,
                    operation_id,
                    draft,
                    timeout,
                )
                .await
        }
        DraftTestIntent::Environment { draft, timeout, .. } => {
            service.prepare_environment_draft_test(draft_id, operation_id, draft, timeout)
        }
    };
    result.map_err(|error| (draft_id, operation_id, error))
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
            return draft_failed_event(
                operation_id,
                draft_id,
                OperationKind::TestDraftConnection,
                PublicSummary::OperationCancelled,
                PublicCode::None,
            );
        }
        () = tokio::time::sleep_until(deadline) => {
            return draft_failed_event(
                operation_id,
                draft_id,
                OperationKind::TestDraftConnection,
                PublicSummary::OperationTimedOut,
                PublicCode::None,
            );
        }
        result = &mut acquire => match result {
            Ok(lease) => lease,
            Err(error) => {
                let (summary, code) = error.public_error_parts();
                return draft_failed_event(
                    operation_id,
                    draft_id,
                    OperationKind::TestDraftConnection,
                    summary,
                    code,
                );
            }
        }
    };
    let ping = AssertUnwindSafe(lease.ping()).catch_unwind();
    tokio::pin!(ping);
    enum DraftAttempt {
        Error(Option<(PublicSummary, PublicCode)>),
        Panicked(Box<dyn std::any::Any + Send>),
    }
    let attempt = tokio::select! {
        biased;
        () = cancel.cancelled() => DraftAttempt::Error(Some((
            PublicSummary::OperationCancelled,
            PublicCode::None,
        ))),
        () = tokio::time::sleep_until(deadline) => {
            DraftAttempt::Error(Some((PublicSummary::OperationTimedOut, PublicCode::None)))
        }
        result = &mut ping => match result {
            Ok(result) => DraftAttempt::Error(
                result.err().map(|error| ServiceError::from(error).public_error_parts())
            ),
            Err(payload) => DraftAttempt::Panicked(payload),
        },
    };
    let close_error = lease
        .close()
        .await
        .err()
        .map(|error| ServiceError::from(error).public_error_parts());
    match attempt {
        DraftAttempt::Panicked(payload) => std::panic::resume_unwind(payload),
        DraftAttempt::Error(error) => {
            if let Some((summary, code)) = error.or(close_error) {
                draft_failed_event(
                    operation_id,
                    draft_id,
                    OperationKind::TestDraftConnection,
                    summary,
                    code,
                )
            } else if service.is_config_uncertain() {
                draft_failed_event(
                    operation_id,
                    draft_id,
                    OperationKind::TestDraftConnection,
                    PublicSummary::ResourceStale,
                    PublicCode::ConfigExternalChange,
                )
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
        ProfileWork::Execute {
            request,
            kind,
            editor_tab_id,
        } => run_execute(service, request, kind, editor_tab_id, cancel, messages).await,
        ProfileWork::ExecuteBatch {
            request,
            kind,
            editor_tab_id,
        } => run_execute_batch(service, request, kind, editor_tab_id, cancel, messages).await,
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
    editor_tab_id: Option<super::model::EditorTabId>,
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
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let execute = lease.execute_typed(&typed_request, remaining);
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
            editor_tab_id,
            session_generation,
            result: service.retain_execute_result(&typed_request, result),
        },
        (ExecuteAttempt::Driver(Err(error)), Ok(())) => {
            let disposition = SessionDisposition::for_driver_error(&error);
            let service_error = ServiceError::from(error);
            let (summary, code) = service_error.public_error_parts();
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
                code,
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

async fn run_execute_batch(
    service: &ApplicationService,
    request: crate::model::ExecuteBatchRequest,
    kind: OperationKind,
    editor_tab_id: Option<super::model::EditorTabId>,
    cancel: &CancellationToken,
    messages: &mpsc::UnboundedSender<ControllerMessage>,
) -> UiEvent {
    let operation_id = request.operation_id;
    let profile_id = request.profile_id.clone();
    let profile_generation = request.profile_generation;
    let timeout = request.timeout;
    let deadline = tokio::time::Instant::now() + timeout;
    let prepare = service.prepare_execute_batch_request(&request);
    tokio::pin!(prepare);
    let typed_batch = tokio::select! {
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
            Ok(batch) => batch,
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
    let target_count = typed_batch.len();
    enum ExecuteBatchAttempt {
        Driver(Result<RetainedBatchExecution, crate::drivers::DriverError>),
        Cancelled,
        TimedOut,
    }
    let attempt = {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let execute = lease.execute_typed_batch(
            &typed_batch,
            remaining,
            |identity, driver, retention, result| {
                service.retain_result(identity, driver, retention, result)
            },
        );
        tokio::pin!(execute);
        tokio::select! {
            biased;
            () = cancel.cancelled() => ExecuteBatchAttempt::Cancelled,
            () = tokio::time::sleep_until(deadline) => ExecuteBatchAttempt::TimedOut,
            result = &mut execute => ExecuteBatchAttempt::Driver(result),
        }
    };
    let observation = service.observe_session(&lease, operation_id).await;
    match (attempt, observation) {
        (ExecuteBatchAttempt::Driver(Ok(mut outcome)), Ok(())) => {
            let (error, session_disposition) = if let Some(error) = outcome.failure.take() {
                let session_disposition = SessionDisposition::for_driver_error(&error);
                let service_error = ServiceError::from(error);
                let (summary, code) = service_error.public_error_parts();
                if session_disposition == SessionDisposition::Evict {
                    service.evict_session_lease(&lease).await;
                }
                (
                    Some(public_profile_error(
                        kind,
                        profile_id.clone(),
                        operation_id,
                        summary,
                        code,
                    )),
                    session_disposition,
                )
            } else {
                (None, SessionDisposition::Keep)
            };
            UiEvent::QueryBatchFinished {
                operation_id,
                profile_id,
                profile_generation,
                editor_tab_id,
                session_generation,
                target_count,
                completed_targets: outcome.completed_targets,
                discarded_results: outcome.discarded_results,
                results: outcome.results,
                error,
                session_disposition,
            }
        }
        (ExecuteBatchAttempt::Driver(Err(error)), Ok(())) => {
            let disposition = SessionDisposition::for_driver_error(&error);
            let service_error = ServiceError::from(error);
            let (summary, code) = service_error.public_error_parts();
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
                code,
                disposition,
            )
        }
        (ExecuteBatchAttempt::Cancelled, _) => {
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
        (ExecuteBatchAttempt::TimedOut, _) => {
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
        (ExecuteBatchAttempt::Driver(_), Err(error)) => {
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
    catalog_failed_event_with_code(
        request,
        summary,
        PublicCode::None,
        session_generation,
        session_disposition,
    )
}

fn catalog_failed_event_with_code(
    request: crate::model::CatalogRequest,
    summary: PublicSummary,
    code: PublicCode,
    session_generation: Option<SessionGeneration>,
    session_disposition: Option<SessionDisposition>,
) -> UiEvent {
    let error = public_profile_error(
        OperationKind::BrowseMySql,
        request.profile_id().clone(),
        request.operation_id(),
        summary,
        code,
    );
    UiEvent::CatalogPageFailed {
        request,
        summary,
        error,
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
        Succeeded(Box<crate::model::RedisValuePreview>),
        Failed(crate::drivers::DriverError),
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
            result = &mut inspect => match result {
                Ok(preview) => InspectAttempt::Succeeded(Box::new(preview)),
                Err(error) => InspectAttempt::Failed(error),
            },
        }
    };
    let observation = service.observe_session(&lease, operation_id).await;
    match (attempt, observation) {
        (InspectAttempt::Succeeded(preview), Ok(())) => UiEvent::RedisKeyInspected {
            preview: *preview,
            session_generation,
            session_disposition: SessionDisposition::Keep,
        },
        (InspectAttempt::Failed(driver_error), Ok(())) => {
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
        (InspectAttempt::Succeeded(_) | InspectAttempt::Failed(_), Err(error)) => {
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
            FailureContext::Draft {
                draft_id: request.draft_id,
                kind: OperationKind::CreateProfile,
            },
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
        UiCommand::StoreCredentials {
            operation_id,
            profile_id,
            profile_generation,
            ..
        } => (
            *operation_id,
            TaskScope::Profile {
                profile_id: profile_id.clone(),
                profile_generation: *profile_generation,
                session_generation: None,
            },
            FailureContext::Profile {
                profile_id: profile_id.clone(),
                profile_generation: *profile_generation,
                kind: OperationKind::UpdateProfile,
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
                    snapshots(service)
                        .await
                        .map(|(profiles, config)| ReloadPresentation {
                            outcome,
                            profiles,
                            config,
                        })
                }
                Ok(outcome) => Ok(ReloadPresentation {
                    outcome,
                    profiles: Vec::new(),
                    config: ConfigPresentation::default(),
                }),
                Err(error) => Err(error),
            };
            TaskOutput::Reload {
                operation_id,
                result: Box::new(result),
            }
        }
        UiCommand::CreateProfile(request) => {
            let operation_id = request.operation_id;
            let draft_id = request.draft_id;
            TaskOutput::Create {
                operation_id,
                draft_id,
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
        UiCommand::StoreCredentials {
            operation_id,
            profile_id,
            profile_generation,
            source_operation: _,
            secret,
        } => {
            let result = service
                .store_session_credential_exact(
                    operation_id,
                    &profile_id,
                    profile_generation,
                    secret,
                )
                .await;
            let event = match result {
                Ok(()) => UiEvent::CredentialsStored {
                    operation_id,
                    profile_id,
                    profile_generation,
                },
                Err(error) => credentials_store_failed_event(
                    operation_id,
                    profile_id,
                    profile_generation,
                    &error,
                ),
            };
            TaskOutput::Event(Box::new(event))
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
    workspace: &mut WorkspaceCoordinator,
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
        ControllerMessage::WorkspaceCompleted {
            operation_id,
            result,
        } => {
            workspace
                .complete(
                    operation_id,
                    result,
                    port,
                    application,
                    message_tx,
                    registry,
                )
                .await;
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
            Ok(reload) => reload.outcome.cleanup.targets().collect::<Vec<_>>(),
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
            Ok(reload) => {
                if let Err(error) = application
                    .apply_deferred_cleanup(reload.outcome.cleanup)
                    .await
                {
                    let (summary, code) = error.public_error_parts();
                    profiles_failed_event(operation_id, summary, code)
                } else if reload.outcome.config_uncertain {
                    UiEvent::ConfigUncertain { operation_id }
                } else {
                    UiEvent::ProfilesLoaded {
                        operation_id,
                        profiles: reload.profiles,
                        config: reload.config,
                    }
                }
            }
            Err(_error) if application.is_config_uncertain() => {
                UiEvent::ConfigUncertain { operation_id }
            }
            Err(error) => {
                let (summary, code) = error.public_error_parts();
                profiles_failed_event(operation_id, summary, code)
            }
        },
        TaskOutput::Create {
            operation_id,
            draft_id,
            result,
        } => match *result {
            Ok(outcome) => {
                if let Err(error) = application.apply_deferred_cleanup(outcome.cleanup).await {
                    let (summary, code) = error.public_error_parts();
                    draft_failed_event(
                        operation_id,
                        draft_id,
                        OperationKind::CreateProfile,
                        summary,
                        code,
                    )
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
                    let (summary, code) = error.public_error_parts();
                    draft_failed_event(
                        operation_id,
                        draft_id,
                        OperationKind::CreateProfile,
                        summary,
                        code,
                    )
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
                    let (summary, code) = error.public_error_parts();
                    return profile_update_failed_event(
                        operation_id,
                        profile_id,
                        previous_generation,
                        summary,
                        code,
                    );
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
                    let (summary, code) = error.public_error_parts();
                    profile_update_failed_event(
                        operation_id,
                        profile_id,
                        previous_generation,
                        summary,
                        code,
                    )
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
    if application.is_config_uncertain() && !matches!(&command, UiCommand::CancelOperation { .. }) {
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
        FailureContext::Profiles => profiles_failed_event(
            operation_id,
            PublicSummary::InternalFailure,
            PublicCode::None,
        ),
        FailureContext::Draft { draft_id, kind } => draft_failed_event(
            operation_id,
            draft_id,
            kind,
            PublicSummary::InternalFailure,
            PublicCode::None,
        ),
        FailureContext::Export {
            result_id,
            format,
            overwrite_policy,
        } => result_export_failed_event(
            operation_id,
            result_id,
            format,
            overwrite_policy,
            PublicSummary::InternalFailure,
            PublicCode::None,
            false,
        ),
    }
}

pub(super) async fn snapshots(
    application: &ApplicationService,
) -> Result<(Vec<ProfileSnapshot>, ConfigPresentation), ServiceError> {
    let mut snapshots = Vec::new();
    for (profile, generation) in application.profiles_with_generations_snapshot().await {
        let profile_id = ProfileId(profile.id.clone());
        let has_current_session_secret = application.has_current_session_secret(&profile_id)?;
        let environment_availability = (profile.credential_mode
            == crate::model::CredentialMode::Environment)
            .then(|| {
                profile
                    .secret_env
                    .as_deref()
                    .map(|name| application.environment_availability(name))
            })
            .flatten();
        snapshots.push(ProfileSnapshot::from_profile(
            &profile,
            generation,
            has_current_session_secret,
            environment_availability,
        ));
    }
    let source_version = application.source_version().await;
    let config = ConfigPresentation::for_source(source_version, application.config_path());
    Ok((snapshots, config))
}

enum RedisResourceRequest {
    Scan(RedisScanRequest),
    Inspect(RedisKeyInspectRequest),
}

fn public_global_error(
    kind: OperationKind,
    operation_id: OperationId,
    summary: PublicSummary,
    code: PublicCode,
) -> PublicOperationError {
    PublicOperationError::new_or_internal(kind, summary, code, &SafeContext::global(operation_id))
}

fn public_draft_error(
    kind: OperationKind,
    draft_id: DraftId,
    operation_id: OperationId,
    summary: PublicSummary,
    code: PublicCode,
) -> PublicOperationError {
    PublicOperationError::new_or_internal(
        kind,
        summary,
        code,
        &SafeContext::draft(draft_id, operation_id),
    )
}

fn public_profile_error(
    kind: OperationKind,
    profile_id: ProfileId,
    operation_id: OperationId,
    summary: PublicSummary,
    code: PublicCode,
) -> PublicOperationError {
    let context = if runtime_operation_has_retry_recipe(kind) {
        SafeContext::profile_with_recipe(
            profile_id,
            operation_id,
            OperationRecipeId(operation_id.0),
        )
    } else {
        SafeContext::profile(profile_id, operation_id)
    };
    PublicOperationError::new_or_internal(kind, summary, code, &context)
}

const fn runtime_operation_has_retry_recipe(kind: OperationKind) -> bool {
    matches!(
        kind,
        OperationKind::ConnectProfile
            | OperationKind::ReconnectProfile
            | OperationKind::BrowseMySql
            | OperationKind::BrowseRedis
            | OperationKind::InspectRedis
    )
}

fn profiles_failed_event(
    operation_id: OperationId,
    summary: PublicSummary,
    code: PublicCode,
) -> UiEvent {
    UiEvent::ProfilesFailed {
        operation_id,
        summary,
        error: public_global_error(
            OperationKind::ReloadConfiguration,
            operation_id,
            summary,
            code,
        ),
    }
}

fn draft_failed_event(
    operation_id: OperationId,
    draft_id: DraftId,
    kind: OperationKind,
    summary: PublicSummary,
    code: PublicCode,
) -> UiEvent {
    let error = public_draft_error(kind, draft_id, operation_id, summary, code);
    if kind == OperationKind::CreateProfile {
        UiEvent::ProfileCreateFailed {
            operation_id,
            draft_id,
            summary,
            error,
        }
    } else {
        UiEvent::DraftOperationFailed {
            operation_id,
            draft_id,
            summary,
            error,
        }
    }
}

fn profile_update_failed_event(
    operation_id: OperationId,
    profile_id: ProfileId,
    profile_generation: ProfileGeneration,
    summary: PublicSummary,
    code: PublicCode,
) -> UiEvent {
    let error = public_profile_error(
        OperationKind::UpdateProfile,
        profile_id.clone(),
        operation_id,
        summary,
        code,
    );
    UiEvent::ProfileUpdateFailed {
        operation_id,
        profile_id,
        profile_generation,
        summary,
        error,
    }
}

fn credentials_store_failed_event(
    operation_id: OperationId,
    profile_id: ProfileId,
    profile_generation: ProfileGeneration,
    service_error: &ServiceError,
) -> UiEvent {
    let (summary, code) = service_error.public_error_parts();
    credentials_store_failed_event_with_parts(
        operation_id,
        profile_id,
        profile_generation,
        summary,
        code,
    )
}

fn credentials_store_failed_event_with_parts(
    operation_id: OperationId,
    profile_id: ProfileId,
    profile_generation: ProfileGeneration,
    summary: PublicSummary,
    code: PublicCode,
) -> UiEvent {
    let error = PublicOperationError::new_or_internal(
        OperationKind::UpdateProfile,
        summary,
        code,
        &SafeContext::profile(profile_id.clone(), operation_id),
    );
    UiEvent::CredentialsStoreFailed {
        operation_id,
        profile_id,
        profile_generation,
        summary: error.summary,
        error,
    }
}

fn public_redis_resource_error(
    kind: OperationKind,
    profile_id: ProfileId,
    operation_id: OperationId,
    summary: PublicSummary,
    code: PublicCode,
) -> PublicOperationError {
    public_profile_error(kind, profile_id, operation_id, summary, code)
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
    let (summary, code) = error.public_error_parts();
    failed_profile_event_with_code(
        operation_id,
        profile_id,
        profile_generation,
        session_generation,
        kind,
        summary,
        code,
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
    failed_profile_event_with_code(
        operation_id,
        profile_id,
        profile_generation,
        session_generation,
        kind,
        summary,
        PublicCode::None,
    )
}

#[allow(clippy::too_many_arguments)]
fn failed_profile_event_with_code(
    operation_id: OperationId,
    profile_id: ProfileId,
    profile_generation: ProfileGeneration,
    session_generation: Option<SessionGeneration>,
    kind: OperationKind,
    summary: PublicSummary,
    code: PublicCode,
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
    let error = public_profile_error(kind, profile_id.clone(), operation_id, summary, code);
    UiEvent::OperationFailed {
        operation_id,
        profile_id,
        profile_generation,
        session_generation,
        kind,
        summary,
        error,
        session_disposition,
        connection_outcome,
    }
}

#[allow(clippy::too_many_arguments)]
fn failed_profile_event_with_disposition(
    operation_id: OperationId,
    profile_id: ProfileId,
    profile_generation: ProfileGeneration,
    session_generation: SessionGeneration,
    kind: OperationKind,
    summary: PublicSummary,
    code: PublicCode,
    session_disposition: SessionDisposition,
) -> UiEvent {
    let connection_outcome = match session_disposition {
        SessionDisposition::Keep => ConnectionFailureOutcome::Preserve,
        SessionDisposition::Evict => ConnectionFailureOutcome::Disconnected,
    };
    let error = public_profile_error(kind, profile_id.clone(), operation_id, summary, code);
    UiEvent::OperationFailed {
        operation_id,
        profile_id,
        profile_generation,
        session_generation: Some(session_generation),
        kind,
        summary,
        error,
        session_disposition: Some(session_disposition),
        connection_outcome,
    }
}

fn failure_for_unavailable(command: UiCommand, summary: PublicSummary) -> UiEvent {
    match command {
        UiCommand::RefreshProfiles { operation_id } => {
            profiles_failed_event(operation_id, summary, PublicCode::None)
        }
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
        UiCommand::CreateProfile(request) => draft_failed_event(
            request.operation_id,
            request.draft_id,
            OperationKind::CreateProfile,
            summary,
            PublicCode::None,
        ),
        UiCommand::UpdateProfile(request) => profile_update_failed_event(
            request.operation_id,
            request.profile_id,
            request.expected_generation,
            summary,
            PublicCode::None,
        ),
        UiCommand::DeleteProfile(request) => failed_profile_event(
            request.operation_id,
            request.profile_id,
            request.expected_generation,
            None,
            OperationKind::DeleteProfile,
            summary,
        ),
        UiCommand::StoreCredentials {
            operation_id,
            profile_id,
            profile_generation,
            ..
        } => credentials_store_failed_event_with_parts(
            operation_id,
            profile_id,
            profile_generation,
            summary,
            PublicCode::None,
        ),
        UiCommand::LoadWorkspace {
            operation_id,
            identity,
            base_revision,
        } => workspace_failure_event(
            WorkspaceOperationMeta {
                operation_id,
                identity,
                revision: base_revision,
                action: WorkspaceAction::Load,
            },
            workspace_unavailable_code(summary),
        ),
        UiCommand::CommitWorkspace {
            operation_id,
            identity,
            revision,
            ..
        } => workspace_failure_event(
            WorkspaceOperationMeta {
                operation_id,
                identity,
                revision,
                action: WorkspaceAction::Commit,
            },
            workspace_unavailable_code(summary),
        ),
        UiCommand::ClearWorkspace {
            operation_id,
            identity,
            base_revision,
        } => workspace_failure_event(
            WorkspaceOperationMeta {
                operation_id,
                identity,
                revision: base_revision,
                action: WorkspaceAction::Clear,
            },
            workspace_unavailable_code(summary),
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
        UiCommand::TestDraftConnection(request) => draft_failed_event(
            request.operation_id(),
            request.draft_id(),
            OperationKind::TestDraftConnection,
            summary,
            PublicCode::None,
        ),
        UiCommand::PrepareDraftConnectionTest(intent) => draft_failed_event(
            intent.operation_id(),
            intent.draft_id(),
            OperationKind::TestDraftConnection,
            summary,
            PublicCode::None,
        ),
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
        UiCommand::ExecuteBatch {
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
        UiCommand::ExportResult { request, .. } => result_export_failed_event(
            request.operation_id,
            request.result_id,
            request.format,
            request.overwrite_policy,
            summary,
            PublicCode::None,
            false,
        ),
        UiCommand::CancelOperation { operation_id }
        | UiCommand::ShutdownRuntime { operation_id } => {
            profiles_failed_event(operation_id, summary, PublicCode::None)
        }
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

fn workspace_unavailable_code(summary: PublicSummary) -> WorkspaceFailureCode {
    match summary {
        PublicSummary::ResourceBusy => WorkspaceFailureCode::Busy,
        PublicSummary::InternalFailure => WorkspaceFailureCode::Internal,
        _ => WorkspaceFailureCode::Unavailable,
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

#[cfg(test)]
mod workspace_runtime_tests {
    use std::fs;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;

    use crate::config::{CURRENT_CONFIG_VERSION, Config};
    use crate::model::{
        ConnectionProfile, CredentialMode, DriverKind, OperationId, ProfileAccess,
        ProfileEnvironment, ProfileGeneration, ProfileId, ProfileInstanceId, ProfileSafetyPosture,
        RedisTlsConfig, TlsMode,
    };
    use crate::service::ApplicationService;
    use crate::ui::adapter::{
        UiCommand, UiPort, bounded_ports, controller_ports, controller_ports_with_event_capacity,
    };
    use crate::ui::model::{UiEvent, WorkspaceAction, WorkspaceFailureCode, WorkspaceIdentity};
    use crate::workspace::{
        EditorTabSnapshot, ProfileWorkspaceSnapshot, WorkspaceGeometrySnapshot, WorkspaceLanguage,
        WorkspaceReadOnlyReason, WorkspaceStore, WorkspaceStoreMode,
    };

    use super::{
        ActiveWorkspaceOperation, ControllerMessage, RuntimeHandle, TaskRegistry, WorkspaceBackend,
        WorkspaceCoordinator, WorkspaceOperationMeta, WorkspaceRequest, WorkspaceStoreOutput,
        execute_workspace_request, finalize_workspace_result, run_workspace_store_request,
        spawn_with_service,
    };

    fn classified_profile(id: &str, instance_id: ProfileInstanceId) -> ConnectionProfile {
        ConnectionProfile {
            id: id.to_owned(),
            name: "Workspace fixture".to_owned(),
            driver: DriverKind::MySql,
            host: "127.0.0.1".to_owned(),
            port: 3306,
            database: None,
            username: None,
            safety: ProfileSafetyPosture::classified(
                ProfileEnvironment::Development,
                ProfileAccess::ReadWrite,
                instance_id,
            ),
            tls: TlsMode::Disabled,
            credential_mode: CredentialMode::None,
            secret_env: None,
            redis_tls: RedisTlsConfig::default(),
        }
    }

    fn write_config(path: &std::path::Path, profiles: Vec<ConnectionProfile>) {
        let config = Config {
            version: CURRENT_CONFIG_VERSION,
            profiles,
        };
        fs::write(
            path,
            toml::to_string(&config).expect("serialize test config"),
        )
        .expect("write test config");
    }

    fn service_fixture(
        path: &std::path::Path,
        profile_id: &str,
        instance_id: ProfileInstanceId,
    ) -> ApplicationService {
        write_config(path, vec![classified_profile(profile_id, instance_id)]);
        ApplicationService::load_path(path).expect("load test service")
    }

    fn identity(
        profile_id: &str,
        generation: u64,
        instance_id: ProfileInstanceId,
    ) -> WorkspaceIdentity {
        WorkspaceIdentity::new(
            ProfileId(profile_id.to_owned()),
            ProfileGeneration(generation),
            instance_id,
        )
    }

    fn snapshot(
        profile_id: &str,
        instance_id: ProfileInstanceId,
        source: &str,
    ) -> Box<ProfileWorkspaceSnapshot> {
        let editor = EditorTabSnapshot::new(
            1,
            "Workspace",
            WorkspaceLanguage::Sql,
            source,
            None,
            source.chars().count(),
            None,
        )
        .expect("valid editor snapshot");
        Box::new(
            ProfileWorkspaceSnapshot::new(
                instance_id,
                ProfileId(profile_id.to_owned()),
                true,
                vec![editor],
                Some(1),
                WorkspaceGeometrySnapshot::new(240.0, 0.6, false)
                    .expect("valid workspace geometry"),
                Vec::new(),
            )
            .expect("valid workspace snapshot"),
        )
    }

    fn commit_command(
        operation_id: u64,
        identity: WorkspaceIdentity,
        revision: u64,
        source: &str,
    ) -> UiCommand {
        UiCommand::CommitWorkspace {
            operation_id: OperationId(operation_id),
            snapshot: snapshot(
                identity.profile_id().as_str(),
                identity.instance_id(),
                source,
            ),
            identity,
            revision,
        }
    }

    fn load_command(
        operation_id: u64,
        identity: WorkspaceIdentity,
        base_revision: u64,
    ) -> UiCommand {
        UiCommand::LoadWorkspace {
            operation_id: OperationId(operation_id),
            identity,
            base_revision,
        }
    }

    fn clear_command(
        operation_id: u64,
        identity: WorkspaceIdentity,
        base_revision: u64,
    ) -> UiCommand {
        UiCommand::ClearWorkspace {
            operation_id: OperationId(operation_id),
            identity,
            base_revision,
        }
    }

    fn block_coordinator(
        coordinator: &mut WorkspaceCoordinator,
        registry: &mut TaskRegistry,
        operation_id: OperationId,
        identity: WorkspaceIdentity,
        revision: u64,
    ) {
        let reservation = registry
            .reserve(operation_id)
            .expect("reserve active workspace fixture");
        coordinator.active = Some(ActiveWorkspaceOperation {
            reservation,
            meta: WorkspaceOperationMeta {
                operation_id,
                identity,
                revision,
                action: WorkspaceAction::Commit,
            },
            completion_sent: Arc::new(AtomicBool::new(false)),
            join: tokio::spawn(std::future::pending()),
        });
    }

    async fn release_blocked_coordinator(
        coordinator: &mut WorkspaceCoordinator,
        registry: &mut TaskRegistry,
    ) {
        if let Some(active) = coordinator.active.take() {
            active.join.abort();
            let _ = active.join.await;
            registry.release_reservation(active.reservation);
        }
    }

    async fn next_event(ui: &mut UiPort) -> UiEvent {
        tokio::time::timeout(Duration::from_secs(3), ui.next_event())
            .await
            .expect("runtime event timeout")
            .expect("runtime event channel open")
    }

    async fn wait_for_profiles(ui: &mut UiPort, operation_id: OperationId) {
        loop {
            if matches!(
                next_event(ui).await,
                UiEvent::ProfilesLoaded {
                    operation_id: event_operation,
                    ..
                } if event_operation == operation_id
            ) {
                return;
            }
        }
    }

    async fn shutdown_runtime(ui: &mut UiPort, runtime: RuntimeHandle, operation_id: OperationId) {
        ui.try_submit(UiCommand::ShutdownRuntime { operation_id })
            .expect("submit runtime shutdown");
        loop {
            if matches!(
                next_event(ui).await,
                UiEvent::RuntimeShutdown {
                    operation_id: event_operation,
                } if event_operation == operation_id
            ) {
                break;
            }
        }
        tokio::time::timeout(Duration::from_secs(3), runtime.wait())
            .await
            .expect("runtime shutdown timeout")
            .expect("runtime task join");
    }

    #[tokio::test]
    async fn coordinator_keeps_active_and_only_latest_pending_commit_per_exact_identity() {
        let directory = tempfile::tempdir().expect("tempdir");
        let config_path = directory.path().join("config.toml");
        let instance_id = ProfileInstanceId::from_bytes([1; 16]);
        let application = service_fixture(&config_path, "coalesce", instance_id);
        let (mut ui, port) = bounded_ports(16);
        let (message_tx, _message_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut registry = TaskRegistry::default();
        let mut coordinator = WorkspaceCoordinator::new(WorkspaceBackend::Unavailable(
            WorkspaceFailureCode::Unavailable,
        ));
        let identity = identity("coalesce", 1, instance_id);
        block_coordinator(
            &mut coordinator,
            &mut registry,
            OperationId(1),
            identity.clone(),
            1,
        );

        coordinator
            .enqueue(
                commit_command(6, identity.clone(), 1, "duplicate active revision"),
                4,
                &port,
                &application,
                &message_tx,
                &mut registry,
            )
            .await;
        for (operation_id, revision) in [(2, 2), (3, 3), (4, 4)] {
            coordinator
                .enqueue(
                    commit_command(operation_id, identity.clone(), revision, "select 1"),
                    4,
                    &port,
                    &application,
                    &message_tx,
                    &mut registry,
                )
                .await;
        }
        coordinator
            .enqueue(
                commit_command(5, identity.clone(), 3, "late stale revision"),
                4,
                &port,
                &application,
                &message_tx,
                &mut registry,
            )
            .await;

        assert_eq!(coordinator.pending.len(), 1);
        assert_eq!(
            coordinator
                .pending
                .front()
                .map(|pending| pending.request.meta().operation_id),
            Some(OperationId(4))
        );
        let events = ui.drain_events(8);
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(
                    event,
                    UiEvent::WorkspaceCommitSuperseded {
                        operation_id: OperationId(2 | 3 | 5 | 6),
                        ..
                    }
                ))
                .count(),
            4
        );
        assert!(events.iter().any(|event| matches!(
            event,
            UiEvent::WorkspaceCommitSuperseded {
                operation_id: OperationId(6),
                superseded_by: OperationId(1),
                superseded_by_revision: 1,
                ..
            }
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            UiEvent::WorkspaceCommitSuperseded {
                operation_id: OperationId(5),
                superseded_by: OperationId(4),
                superseded_by_revision: 4,
                ..
            }
        )));
        for operation_id in [
            OperationId(2),
            OperationId(3),
            OperationId(5),
            OperationId(6),
        ] {
            let released = registry
                .reserve(operation_id)
                .expect("superseded reservation released exactly once");
            registry.release_reservation(released);
        }
        assert!(registry.reserve(OperationId(1)).is_err());
        assert!(registry.reserve(OperationId(4)).is_err());
        release_blocked_coordinator(&mut coordinator, &mut registry).await;
    }

    #[tokio::test]
    async fn clear_uses_exact_identity_and_does_not_cross_the_latest_load_barrier() {
        let directory = tempfile::tempdir().expect("tempdir");
        let config_path = directory.path().join("config.toml");
        let instance_id = ProfileInstanceId::from_bytes([2; 16]);
        let application = service_fixture(&config_path, "barrier", instance_id);
        let (mut ui, port) = bounded_ports(16);
        let (message_tx, _message_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut registry = TaskRegistry::default();
        let mut coordinator = WorkspaceCoordinator::new(WorkspaceBackend::Unavailable(
            WorkspaceFailureCode::Unavailable,
        ));
        let current_identity = identity("barrier", 1, instance_id);
        block_coordinator(
            &mut coordinator,
            &mut registry,
            OperationId(10),
            current_identity.clone(),
            1,
        );

        coordinator
            .enqueue(
                commit_command(11, current_identity.clone(), 2, "before load"),
                8,
                &port,
                &application,
                &message_tx,
                &mut registry,
            )
            .await;
        coordinator
            .enqueue(
                load_command(12, current_identity.clone(), 2),
                8,
                &port,
                &application,
                &message_tx,
                &mut registry,
            )
            .await;
        let stale_generation = identity("barrier", 2, instance_id);
        coordinator
            .enqueue(
                commit_command(17, stale_generation, 3, "stale generation"),
                8,
                &port,
                &application,
                &message_tx,
                &mut registry,
            )
            .await;
        coordinator
            .enqueue(
                commit_command(13, current_identity.clone(), 3, "after load"),
                8,
                &port,
                &application,
                &message_tx,
                &mut registry,
            )
            .await;
        coordinator
            .enqueue(
                clear_command(14, current_identity.clone(), 3),
                8,
                &port,
                &application,
                &message_tx,
                &mut registry,
            )
            .await;
        coordinator
            .enqueue(
                commit_command(15, current_identity.clone(), 4, "after clear"),
                8,
                &port,
                &application,
                &message_tx,
                &mut registry,
            )
            .await;
        coordinator
            .enqueue(
                commit_command(16, current_identity.clone(), 5, "latest after clear"),
                8,
                &port,
                &application,
                &message_tx,
                &mut registry,
            )
            .await;

        let pending = coordinator
            .pending
            .iter()
            .map(|pending| pending.request.meta().operation_id)
            .collect::<Vec<_>>();
        assert_eq!(
            pending,
            vec![
                OperationId(11),
                OperationId(12),
                OperationId(17),
                OperationId(14),
                OperationId(16),
            ]
        );
        let events = ui.drain_events(8);
        assert!(events.iter().any(|event| matches!(
            event,
            UiEvent::WorkspaceCommitSuperseded {
                operation_id: OperationId(13),
                superseded_by: OperationId(14),
                ..
            }
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            UiEvent::WorkspaceCommitSuperseded {
                operation_id: OperationId(15),
                superseded_by: OperationId(16),
                ..
            }
        )));
        assert!(!events.iter().any(|event| matches!(
            event,
            UiEvent::WorkspaceCommitSuperseded {
                operation_id: OperationId(11 | 17),
                ..
            }
        )));
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, UiEvent::WorkspaceCommitSuperseded { .. }))
                .count(),
            2
        );
        release_blocked_coordinator(&mut coordinator, &mut registry).await;
    }

    #[tokio::test]
    async fn load_barrier_prevents_cross_barrier_commit_coalescing() {
        let directory = tempfile::tempdir().expect("tempdir");
        let config_path = directory.path().join("config.toml");
        let instance_id = ProfileInstanceId::from_bytes([3; 16]);
        let application = service_fixture(&config_path, "load-barrier", instance_id);
        let (mut ui, port) = bounded_ports(16);
        let (message_tx, _message_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut registry = TaskRegistry::default();
        let mut coordinator = WorkspaceCoordinator::new(WorkspaceBackend::Unavailable(
            WorkspaceFailureCode::Unavailable,
        ));
        let identity = identity("load-barrier", 1, instance_id);
        block_coordinator(
            &mut coordinator,
            &mut registry,
            OperationId(20),
            identity.clone(),
            1,
        );

        for command in [
            commit_command(21, identity.clone(), 2, "before load"),
            load_command(22, identity.clone(), 2),
            commit_command(23, identity.clone(), 3, "after load"),
            commit_command(24, identity.clone(), 4, "latest after load"),
        ] {
            coordinator
                .enqueue(command, 4, &port, &application, &message_tx, &mut registry)
                .await;
        }

        let pending = coordinator
            .pending
            .iter()
            .map(|pending| pending.request.meta().operation_id)
            .collect::<Vec<_>>();
        assert_eq!(
            pending,
            vec![OperationId(21), OperationId(22), OperationId(24)]
        );
        let events = ui.drain_events(8);
        assert_eq!(events.len(), 1);
        assert!(matches!(
            events.first(),
            Some(UiEvent::WorkspaceCommitSuperseded {
                operation_id: OperationId(23),
                superseded_by: OperationId(24),
                ..
            })
        ));
        release_blocked_coordinator(&mut coordinator, &mut registry).await;
    }

    #[tokio::test]
    async fn coordinator_capacity_collision_and_invalid_identity_release_reservations() {
        let directory = tempfile::tempdir().expect("tempdir");
        let config_path = directory.path().join("config.toml");
        let instance_id = ProfileInstanceId::from_bytes([4; 16]);
        let application = service_fixture(&config_path, "capacity", instance_id);
        let (mut ui, port) = bounded_ports(16);
        let (message_tx, _message_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut registry = TaskRegistry::default();
        let mut coordinator = WorkspaceCoordinator::new(WorkspaceBackend::Unavailable(
            WorkspaceFailureCode::Unavailable,
        ));
        let identity = identity("capacity", 1, instance_id);
        block_coordinator(
            &mut coordinator,
            &mut registry,
            OperationId(30),
            identity.clone(),
            1,
        );
        for operation_id in [31, 32, 33] {
            coordinator
                .enqueue(
                    load_command(operation_id, identity.clone(), 1),
                    2,
                    &port,
                    &application,
                    &message_tx,
                    &mut registry,
                )
                .await;
        }
        assert_eq!(coordinator.pending.len(), 2);
        assert!(ui.drain_events(4).iter().any(|event| matches!(
            event,
            UiEvent::WorkspaceOperationFailed {
                operation_id: OperationId(33),
                code: WorkspaceFailureCode::Busy,
                ..
            }
        )));
        let capacity_released = registry
            .reserve(OperationId(33))
            .expect("capacity rejection releases reservation");
        registry.release_reservation(capacity_released);

        let collision = registry
            .reserve(OperationId(34))
            .expect("simulate globally active non-workspace operation");
        coordinator
            .enqueue(
                load_command(34, identity.clone(), 1),
                2,
                &port,
                &application,
                &message_tx,
                &mut registry,
            )
            .await;
        assert!(ui.drain_events(4).iter().any(|event| matches!(
            event,
            UiEvent::WorkspaceOperationFailed {
                operation_id: OperationId(34),
                code: WorkspaceFailureCode::Busy,
                ..
            }
        )));
        registry.release_reservation(collision);

        let invalid_snapshot = UiCommand::CommitWorkspace {
            operation_id: OperationId(35),
            identity,
            revision: 2,
            snapshot: snapshot("different-profile", instance_id, "invalid identity"),
        };
        coordinator
            .enqueue(
                invalid_snapshot,
                2,
                &port,
                &application,
                &message_tx,
                &mut registry,
            )
            .await;
        assert!(ui.drain_events(4).iter().any(|event| matches!(
            event,
            UiEvent::WorkspaceOperationFailed {
                operation_id: OperationId(35),
                code: WorkspaceFailureCode::InvalidIdentity,
                ..
            }
        )));
        let invalid_released = registry
            .reserve(OperationId(35))
            .expect("invalid identity releases reservation");
        registry.release_reservation(invalid_released);
        release_blocked_coordinator(&mut coordinator, &mut registry).await;
    }

    #[tokio::test]
    async fn closed_completion_channel_reaps_once_and_releases_reservation() {
        let directory = tempfile::tempdir().expect("tempdir");
        let config_path = directory.path().join("config.toml");
        let instance_id = ProfileInstanceId::from_bytes([5; 16]);
        let application = service_fixture(&config_path, "closed", instance_id);
        let (mut ui, port) = bounded_ports(16);
        let (message_tx, message_rx) = tokio::sync::mpsc::unbounded_channel::<ControllerMessage>();
        drop(message_rx);
        let mut registry = TaskRegistry::default();
        let mut coordinator = WorkspaceCoordinator::new(WorkspaceBackend::Unavailable(
            WorkspaceFailureCode::Unavailable,
        ));
        coordinator
            .enqueue(
                load_command(40, identity("closed", 1, instance_id), 0),
                4,
                &port,
                &application,
                &message_tx,
                &mut registry,
            )
            .await;
        for _ in 0..100 {
            if coordinator
                .active
                .as_ref()
                .is_some_and(|active| active.join.is_finished())
            {
                break;
            }
            tokio::task::yield_now().await;
        }
        coordinator
            .reap_panicked(&port, &application, &message_tx, &mut registry)
            .await;
        let events = ui.drain_events(4);
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(
                    event,
                    UiEvent::WorkspaceOperationFailed {
                        operation_id: OperationId(40),
                        code: WorkspaceFailureCode::Internal,
                        ..
                    }
                ))
                .count(),
            1
        );
        let released = registry
            .reserve(OperationId(40))
            .expect("closed completion releases reservation");
        registry.release_reservation(released);
    }

    #[tokio::test]
    async fn panicked_workspace_task_reaps_once_and_releases_reservation() {
        let directory = tempfile::tempdir().expect("tempdir");
        let config_path = directory.path().join("config.toml");
        let instance_id = ProfileInstanceId::from_bytes([6; 16]);
        let application = service_fixture(&config_path, "panic", instance_id);
        let (mut ui, port) = bounded_ports(16);
        let (message_tx, _message_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut registry = TaskRegistry::default();
        let mut coordinator = WorkspaceCoordinator::new(WorkspaceBackend::Unavailable(
            WorkspaceFailureCode::Unavailable,
        ));
        let reservation = registry
            .reserve(OperationId(50))
            .expect("reserve panic fixture");
        coordinator.active = Some(ActiveWorkspaceOperation {
            reservation,
            meta: WorkspaceOperationMeta {
                operation_id: OperationId(50),
                identity: identity("panic", 1, instance_id),
                revision: 1,
                action: WorkspaceAction::Commit,
            },
            completion_sent: Arc::new(AtomicBool::new(false)),
            join: tokio::spawn(async { panic!("test-only workspace worker panic") }),
        });
        while coordinator
            .active
            .as_ref()
            .is_some_and(|active| !active.join.is_finished())
        {
            tokio::task::yield_now().await;
        }
        coordinator
            .reap_panicked(&port, &application, &message_tx, &mut registry)
            .await;
        assert_eq!(
            ui.drain_events(4)
                .iter()
                .filter(|event| matches!(
                    event,
                    UiEvent::WorkspaceOperationFailed {
                        operation_id: OperationId(50),
                        code: WorkspaceFailureCode::Internal,
                        ..
                    }
                ))
                .count(),
            1
        );
        let released = registry
            .reserve(OperationId(50))
            .expect("panic releases reservation");
        registry.release_reservation(released);
    }

    #[tokio::test]
    async fn stale_identity_is_rejected_before_io_and_cannot_commit() {
        let directory = tempfile::tempdir().expect("tempdir");
        let config_path = directory.path().join("config.toml");
        let instance_id = ProfileInstanceId::from_bytes([7; 16]);
        let application = service_fixture(&config_path, "pre-stale", instance_id);
        let store = Arc::new(WorkspaceStore::open(&config_path).expect("open workspace store"));
        let stale_identity = identity("pre-stale", 2, instance_id);
        let result = execute_workspace_request(
            WorkspaceBackend::Ready(store.clone()),
            application,
            WorkspaceRequest::Commit {
                operation_id: OperationId(60),
                identity: stale_identity,
                revision: 1,
                snapshot: snapshot("pre-stale", instance_id, "must not persist"),
            },
        )
        .await;
        assert!(matches!(result, Err(WorkspaceFailureCode::Stale)));
        assert!(
            store
                .load(instance_id)
                .expect("load workspace after stale rejection")
                .is_none()
        );
    }

    #[tokio::test]
    async fn stale_identity_after_io_masks_success_without_marking_current() {
        let directory = tempfile::tempdir().expect("tempdir");
        let config_path = directory.path().join("config.toml");
        let instance_id = ProfileInstanceId::from_bytes([8; 16]);
        let application = service_fixture(&config_path, "post-stale", instance_id);
        let store = WorkspaceStore::open(&config_path).expect("open workspace store");
        let current_identity = identity("post-stale", 1, instance_id);
        let result = run_workspace_store_request(
            &store,
            WorkspaceRequest::Commit {
                operation_id: OperationId(61),
                identity: current_identity.clone(),
                revision: 1,
                snapshot: snapshot("post-stale", instance_id, "persisted before stale"),
            },
        );
        assert!(matches!(result, Ok(WorkspaceStoreOutput::Committed { .. })));

        write_config(&config_path, Vec::new());
        application
            .reload_configuration()
            .await
            .expect("reload removed profile");
        assert!(!application.is_config_uncertain());
        assert!(matches!(
            finalize_workspace_result(&application, &current_identity, result).await,
            Err(WorkspaceFailureCode::Stale)
        ));
        assert!(
            store
                .load(instance_id)
                .expect("load post-I/O workspace")
                .is_some(),
            "the post-I/O validator must distinguish a completed write from a current UI save"
        );
    }

    #[tokio::test]
    async fn store_lease_lives_until_shutdown_and_writer_busy_is_failure_isolated() {
        let directory = tempfile::tempdir().expect("tempdir");
        let config_path = directory.path().join("config.toml");
        let instance_id = ProfileInstanceId::from_bytes([9; 16]);
        let service_a = service_fixture(&config_path, "lease", instance_id);
        let service_b = ApplicationService::load_path(&config_path).expect("second service");

        let (mut ui_a, port_a) = controller_ports();
        let runtime_a = spawn_with_service(port_a, service_a);
        ui_a.try_submit(UiCommand::RefreshProfiles {
            operation_id: OperationId(70),
        })
        .expect("refresh first runtime");
        wait_for_profiles(&mut ui_a, OperationId(70)).await;

        let observer = WorkspaceStore::open(&config_path).expect("open observer store");
        assert_eq!(observer.mode(), WorkspaceStoreMode::ReadOnly);
        assert_eq!(
            observer.read_only_reason(),
            Some(WorkspaceReadOnlyReason::WriterBusy)
        );
        drop(observer);

        let (mut ui_b, port_b) = controller_ports();
        let runtime_b = spawn_with_service(port_b, service_b.clone());
        ui_b.try_submit(UiCommand::RefreshProfiles {
            operation_id: OperationId(71),
        })
        .expect("refresh second runtime");
        wait_for_profiles(&mut ui_b, OperationId(71)).await;

        let current_identity = identity("lease", 1, instance_id);
        ui_b.try_submit(load_command(72, current_identity.clone(), 0))
            .expect("submit read-only load");
        assert!(matches!(
            next_event(&mut ui_b).await,
            UiEvent::WorkspaceLoaded {
                operation_id: OperationId(72),
                mode: WorkspaceStoreMode::ReadOnly,
                read_only_reason: Some(WorkspaceReadOnlyReason::WriterBusy),
                ..
            }
        ));

        ui_b.try_submit(commit_command(73, current_identity, 1, "read-only commit"))
            .expect("submit read-only commit");
        assert!(matches!(
            next_event(&mut ui_b).await,
            UiEvent::WorkspaceOperationFailed {
                operation_id: OperationId(73),
                action: WorkspaceAction::Commit,
                code: WorkspaceFailureCode::ReadOnly(WorkspaceReadOnlyReason::WriterBusy),
                ..
            }
        ));
        assert!(!service_b.is_config_uncertain());
        assert_eq!(service_b.cached_session_count().await, 0);

        ui_b.try_submit(UiCommand::RefreshProfiles {
            operation_id: OperationId(74),
        })
        .expect("refresh after workspace failure");
        wait_for_profiles(&mut ui_b, OperationId(74)).await;

        shutdown_runtime(&mut ui_b, runtime_b, OperationId(75)).await;
        assert_eq!(
            WorkspaceStore::open(&config_path)
                .expect("first runtime still owns writer")
                .mode(),
            WorkspaceStoreMode::ReadOnly
        );
        shutdown_runtime(&mut ui_a, runtime_a, OperationId(76)).await;
        assert_eq!(
            WorkspaceStore::open(&config_path)
                .expect("writer lease released after runtime shutdown")
                .mode(),
            WorkspaceStoreMode::ReadWrite
        );
    }

    #[tokio::test]
    async fn saturated_event_lane_backpressures_terminals_and_keeps_shutdown_last() {
        let directory = tempfile::tempdir().expect("tempdir");
        let config_path = directory.path().join("config.toml");
        let instance_id = ProfileInstanceId::from_bytes([10; 16]);
        let application = service_fixture(&config_path, "shutdown", instance_id);
        let current_identity = identity("shutdown", 1, instance_id);
        let (mut ui, port) = controller_ports_with_event_capacity(1);
        assert!(port.try_emit(UiEvent::ConfigUncertain {
            operation_id: OperationId(79),
        }));
        let critical_reserve = crate::ui::WORKSPACE_CAPACITY
            .saturating_mul(2)
            .saturating_add(2);
        for offset in 0..critical_reserve {
            assert!(
                port.emit(UiEvent::ConfigUncertain {
                    operation_id: OperationId(790 + offset as u64),
                })
                .await
            );
        }

        for command in [
            commit_command(80, current_identity.clone(), 1, "revision one"),
            commit_command(81, current_identity.clone(), 2, "superseded"),
            clear_command(82, current_identity.clone(), 2),
            commit_command(83, current_identity, 3, "revision three"),
        ] {
            ui.try_submit(command).expect("queue workspace command");
        }
        ui.try_submit(UiCommand::ShutdownRuntime {
            operation_id: OperationId(89),
        })
        .expect("queue shutdown");
        let runtime = spawn_with_service(port, application);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !runtime.join.is_finished(),
            "a full bounded event lane must backpressure terminal delivery"
        );
        assert!(matches!(
            next_event(&mut ui).await,
            UiEvent::ConfigUncertain {
                operation_id: OperationId(79)
            }
        ));
        for offset in 0..critical_reserve {
            assert!(matches!(
                next_event(&mut ui).await,
                UiEvent::ConfigUncertain {
                    operation_id,
                } if operation_id == OperationId(790 + offset as u64)
            ));
        }

        let mut events = Vec::new();
        loop {
            let event = next_event(&mut ui).await;
            let is_shutdown = matches!(
                event,
                UiEvent::RuntimeShutdown {
                    operation_id: OperationId(89)
                }
            );
            events.push(event);
            if is_shutdown {
                break;
            }
        }
        tokio::time::timeout(Duration::from_secs(3), runtime.wait())
            .await
            .expect("queued shutdown timeout")
            .expect("queued shutdown task");

        for operation_id in [80, 81, 82, 83] {
            let operation_id = OperationId(operation_id);
            assert_eq!(
                events
                    .iter()
                    .filter(|event| match event {
                        UiEvent::WorkspaceCommitted {
                            operation_id: event_operation,
                            ..
                        }
                        | UiEvent::WorkspaceCleared {
                            operation_id: event_operation,
                            ..
                        }
                        | UiEvent::WorkspaceCommitSuperseded {
                            operation_id: event_operation,
                            ..
                        }
                        | UiEvent::WorkspaceOperationFailed {
                            operation_id: event_operation,
                            ..
                        } => *event_operation == operation_id,
                        _ => false,
                    })
                    .count(),
                1,
                "each queued workspace command receives exactly one terminal"
            );
        }
        assert!(events.iter().any(|event| matches!(
            event,
            UiEvent::WorkspaceCommitSuperseded {
                operation_id: OperationId(81),
                superseded_by: OperationId(82),
                ..
            }
        )));
        let committed_one = events
            .iter()
            .position(|event| {
                matches!(
                    event,
                    UiEvent::WorkspaceCommitted {
                        operation_id: OperationId(80),
                        ..
                    }
                )
            })
            .expect("revision one committed");
        let cleared = events
            .iter()
            .position(|event| {
                matches!(
                    event,
                    UiEvent::WorkspaceCleared {
                        operation_id: OperationId(82),
                        ..
                    }
                )
            })
            .expect("clear completed");
        let committed_three = events
            .iter()
            .position(|event| {
                matches!(
                    event,
                    UiEvent::WorkspaceCommitted {
                        operation_id: OperationId(83),
                        ..
                    }
                )
            })
            .expect("revision three committed");
        assert!(committed_one < cleared);
        assert!(cleared < committed_three);
        assert!(matches!(
            events.last(),
            Some(UiEvent::RuntimeShutdown {
                operation_id: OperationId(89)
            })
        ));

        let store = WorkspaceStore::open(&config_path).expect("reopen after runtime shutdown");
        assert_eq!(store.mode(), WorkspaceStoreMode::ReadWrite);
        let saved = store
            .load(instance_id)
            .expect("load final workspace")
            .expect("final workspace persisted");
        assert_eq!(saved.editor_tabs()[0].source(), "revision three");
    }
}
