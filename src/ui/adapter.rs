//! Bounded, nonblocking seam between the platform UI and background runtime.

use std::collections::HashSet;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc::{self, error::TryRecvError, error::TrySendError};
use tokio::sync::watch;

use crate::export_file::ConfirmedDestination;
use crate::model::{
    CatalogRequest, ConnectionDraft, DraftId, ExportResult, OperationId, OperationKind,
    ProfileGeneration, ProfileId, QueryLanguage, RedisKeyInspectRequest, RedisScanRequest,
};
use crate::secrets::SessionSecret;
use crate::service::{
    CreateProfileRequest, DeleteProfileRequest, TestDraftRequest, UpdateProfileRequest,
};
use crate::workspace::ProfileWorkspaceSnapshot;

use super::model::{EditorTabId, UiEvent, WorkspaceIdentity};

pub enum DraftTestIntent {
    Secretless {
        draft_id: DraftId,
        operation_id: OperationId,
        draft: ConnectionDraft,
        timeout: Duration,
    },
    SessionReplace {
        draft_id: DraftId,
        operation_id: OperationId,
        draft: ConnectionDraft,
        secret: Arc<SessionSecret>,
        timeout: Duration,
    },
    SessionKeep {
        profile_id: ProfileId,
        profile_generation: ProfileGeneration,
        draft_id: DraftId,
        operation_id: OperationId,
        draft: ConnectionDraft,
        timeout: Duration,
    },
    Environment {
        draft_id: DraftId,
        operation_id: OperationId,
        draft: ConnectionDraft,
        timeout: Duration,
    },
}

impl DraftTestIntent {
    pub(crate) const fn draft_id(&self) -> DraftId {
        match self {
            Self::Secretless { draft_id, .. }
            | Self::SessionReplace { draft_id, .. }
            | Self::SessionKeep { draft_id, .. }
            | Self::Environment { draft_id, .. } => *draft_id,
        }
    }

    pub(crate) const fn operation_id(&self) -> OperationId {
        match self {
            Self::Secretless { operation_id, .. }
            | Self::SessionReplace { operation_id, .. }
            | Self::SessionKeep { operation_id, .. }
            | Self::Environment { operation_id, .. } => *operation_id,
        }
    }
}

impl fmt::Debug for DraftTestIntent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Secretless { .. } => "Secretless",
            Self::SessionReplace { .. } => "SessionReplace",
            Self::SessionKeep { .. } => "SessionKeep",
            Self::Environment { .. } => "Environment",
        };
        formatter
            .debug_struct("DraftTestIntent")
            .field("kind", &name)
            .field("draft_id", &self.draft_id())
            .field("operation_id", &self.operation_id())
            .field("payload", &"<redacted>")
            .finish()
    }
}

pub const WORK_CAPACITY: usize = 32;
pub const MUTATION_CAPACITY: usize = 16;
pub const CONTROL_CAPACITY: usize = 16;
pub const WORKSPACE_CAPACITY: usize = 4;
pub const EVENT_CAPACITY: usize = 128;

/// Sensitive command payloads use a redacted Debug and cannot be serialized.
///
/// ```compile_fail
/// # #[cfg(feature = "desktop")]
/// fn check() {
/// fn requires_serialize<T: serde::Serialize>() {}
/// requires_serialize::<dbotter::ui::UiCommand>();
/// # }
/// ```
pub enum UiCommand {
    RefreshProfiles {
        operation_id: OperationId,
    },
    CreateProfile(CreateProfileRequest),
    UpdateProfile(UpdateProfileRequest),
    DeleteProfile(DeleteProfileRequest),
    StoreCredentials {
        operation_id: OperationId,
        profile_id: ProfileId,
        profile_generation: ProfileGeneration,
        source_operation: OperationKind,
        secret: Arc<SessionSecret>,
    },
    LoadWorkspace {
        operation_id: OperationId,
        identity: WorkspaceIdentity,
        base_revision: u64,
    },
    CommitWorkspace {
        operation_id: OperationId,
        identity: WorkspaceIdentity,
        revision: u64,
        snapshot: Box<ProfileWorkspaceSnapshot>,
    },
    ClearWorkspace {
        operation_id: OperationId,
        identity: WorkspaceIdentity,
        base_revision: u64,
    },
    TestConnection {
        operation_id: OperationId,
        profile_id: ProfileId,
        profile_generation: ProfileGeneration,
        timeout_ms: u64,
    },
    TestDraftConnection(TestDraftRequest),
    PrepareDraftConnectionTest(DraftTestIntent),
    Execute {
        operation_id: OperationId,
        profile_id: ProfileId,
        profile_generation: ProfileGeneration,
        editor_tab_id: Option<EditorTabId>,
        language: QueryLanguage,
        text: String,
        row_limit: u32,
        timeout_ms: u64,
    },
    ExecuteBatch {
        operation_id: OperationId,
        profile_id: ProfileId,
        profile_generation: ProfileGeneration,
        editor_tab_id: Option<EditorTabId>,
        language: QueryLanguage,
        text: String,
        row_limit: u32,
        timeout_ms: u64,
    },
    BrowseCatalog(CatalogRequest),
    ScanRedisKeys(RedisScanRequest),
    InspectRedisKey(RedisKeyInspectRequest),
    ExportResult {
        request: ExportResult,
        confirmation: Option<ConfirmedDestination>,
    },
    CancelOperation {
        operation_id: OperationId,
    },
    DisconnectProfile {
        operation_id: OperationId,
        profile_id: ProfileId,
        profile_generation: ProfileGeneration,
    },
    ReconnectProfile {
        operation_id: OperationId,
        profile_id: ProfileId,
        profile_generation: ProfileGeneration,
        timeout_ms: u64,
    },
    ShutdownRuntime {
        operation_id: OperationId,
    },
}

impl UiCommand {
    pub(crate) fn operation_id(&self) -> OperationId {
        match self {
            Self::RefreshProfiles { operation_id }
            | Self::TestConnection { operation_id, .. }
            | Self::Execute { operation_id, .. }
            | Self::ExecuteBatch { operation_id, .. }
            | Self::CancelOperation { operation_id }
            | Self::DisconnectProfile { operation_id, .. }
            | Self::ReconnectProfile { operation_id, .. }
            | Self::StoreCredentials { operation_id, .. }
            | Self::LoadWorkspace { operation_id, .. }
            | Self::CommitWorkspace { operation_id, .. }
            | Self::ClearWorkspace { operation_id, .. }
            | Self::ShutdownRuntime { operation_id } => *operation_id,
            Self::BrowseCatalog(request) => request.operation_id(),
            Self::ScanRedisKeys(request) => request.operation_id(),
            Self::InspectRedisKey(request) => request.operation_id(),
            Self::ExportResult { request, .. } => request.operation_id,
            Self::CreateProfile(request) => request.operation_id,
            Self::UpdateProfile(request) => request.operation_id,
            Self::DeleteProfile(request) => request.operation_id,
            Self::TestDraftConnection(request) => request.operation_id(),
            Self::PrepareDraftConnectionTest(intent) => intent.operation_id(),
        }
    }

    fn lane(&self) -> CommandLane {
        match self {
            Self::RefreshProfiles { .. }
            | Self::CreateProfile(_)
            | Self::UpdateProfile(_)
            | Self::DeleteProfile(_)
            | Self::StoreCredentials { .. } => CommandLane::Mutation,
            Self::LoadWorkspace { .. }
            | Self::CommitWorkspace { .. }
            | Self::ClearWorkspace { .. } => CommandLane::Workspace,
            Self::TestConnection { .. }
            | Self::TestDraftConnection(_)
            | Self::PrepareDraftConnectionTest(_)
            | Self::Execute { .. }
            | Self::ExecuteBatch { .. }
            | Self::BrowseCatalog(_)
            | Self::ScanRedisKeys(_)
            | Self::InspectRedisKey(_)
            | Self::ExportResult { .. } => CommandLane::Work,
            Self::CancelOperation { .. }
            | Self::DisconnectProfile { .. }
            | Self::ReconnectProfile { .. } => CommandLane::Control,
            Self::ShutdownRuntime { .. } => CommandLane::Shutdown,
        }
    }
}

impl fmt::Debug for UiCommand {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RefreshProfiles { operation_id } => formatter
                .debug_struct("UiCommand::RefreshProfiles")
                .field("operation_id", operation_id)
                .finish(),
            Self::CreateProfile(request) => formatter
                .debug_tuple("UiCommand::CreateProfile")
                .field(request)
                .finish(),
            Self::UpdateProfile(request) => formatter
                .debug_tuple("UiCommand::UpdateProfile")
                .field(request)
                .finish(),
            Self::DeleteProfile(request) => formatter
                .debug_tuple("UiCommand::DeleteProfile")
                .field(request)
                .finish(),
            Self::StoreCredentials {
                operation_id,
                profile_id,
                profile_generation,
                source_operation,
                ..
            } => formatter
                .debug_struct("UiCommand::StoreCredentials")
                .field("operation_id", operation_id)
                .field("profile_id", profile_id)
                .field("profile_generation", profile_generation)
                .field("source_operation", source_operation)
                .field("secret", &"<redacted>")
                .finish(),
            Self::LoadWorkspace {
                operation_id,
                identity,
                base_revision,
            } => formatter
                .debug_struct("UiCommand::LoadWorkspace")
                .field("operation_id", operation_id)
                .field("identity", identity)
                .field("base_revision", base_revision)
                .finish(),
            Self::CommitWorkspace {
                operation_id,
                identity,
                revision,
                ..
            } => formatter
                .debug_struct("UiCommand::CommitWorkspace")
                .field("operation_id", operation_id)
                .field("identity", identity)
                .field("revision", revision)
                .field("snapshot", &"<redacted>")
                .finish(),
            Self::ClearWorkspace {
                operation_id,
                identity,
                base_revision,
            } => formatter
                .debug_struct("UiCommand::ClearWorkspace")
                .field("operation_id", operation_id)
                .field("identity", identity)
                .field("base_revision", base_revision)
                .finish(),
            Self::TestConnection {
                operation_id,
                profile_id,
                profile_generation,
                timeout_ms,
            } => formatter
                .debug_struct("UiCommand::TestConnection")
                .field("operation_id", operation_id)
                .field("profile_id", profile_id)
                .field("profile_generation", profile_generation)
                .field("timeout_ms", timeout_ms)
                .finish(),
            Self::TestDraftConnection(request) => formatter
                .debug_tuple("UiCommand::TestDraftConnection")
                .field(request)
                .finish(),
            Self::PrepareDraftConnectionTest(intent) => formatter
                .debug_tuple("UiCommand::PrepareDraftConnectionTest")
                .field(intent)
                .finish(),
            Self::Execute {
                operation_id,
                profile_id,
                profile_generation,
                editor_tab_id,
                language,
                row_limit,
                timeout_ms,
                ..
            } => formatter
                .debug_struct("UiCommand::Execute")
                .field("operation_id", operation_id)
                .field("profile_id", profile_id)
                .field("profile_generation", profile_generation)
                .field("editor_tab_id", editor_tab_id)
                .field("language", language)
                .field("text", &"<redacted>")
                .field("row_limit", row_limit)
                .field("timeout_ms", timeout_ms)
                .finish(),
            Self::ExecuteBatch {
                operation_id,
                profile_id,
                profile_generation,
                editor_tab_id,
                language,
                row_limit,
                timeout_ms,
                ..
            } => formatter
                .debug_struct("UiCommand::ExecuteBatch")
                .field("operation_id", operation_id)
                .field("profile_id", profile_id)
                .field("profile_generation", profile_generation)
                .field("editor_tab_id", editor_tab_id)
                .field("language", language)
                .field("text", &"<redacted>")
                .field("row_limit", row_limit)
                .field("timeout_ms", timeout_ms)
                .finish(),
            Self::BrowseCatalog(request) => formatter
                .debug_tuple("UiCommand::BrowseCatalog")
                .field(request)
                .finish(),
            Self::ScanRedisKeys(request) => formatter
                .debug_tuple("UiCommand::ScanRedisKeys")
                .field(request)
                .finish(),
            Self::InspectRedisKey(request) => formatter
                .debug_tuple("UiCommand::InspectRedisKey")
                .field(request)
                .finish(),
            Self::ExportResult { request, .. } => formatter
                .debug_struct("UiCommand::ExportResult")
                .field("request", request)
                .field("confirmation", &"<redacted>")
                .finish(),
            Self::CancelOperation { operation_id } => formatter
                .debug_struct("UiCommand::CancelOperation")
                .field("operation_id", operation_id)
                .finish(),
            Self::DisconnectProfile {
                operation_id,
                profile_id,
                profile_generation,
            } => formatter
                .debug_struct("UiCommand::DisconnectProfile")
                .field("operation_id", operation_id)
                .field("profile_id", profile_id)
                .field("profile_generation", profile_generation)
                .finish(),
            Self::ReconnectProfile {
                operation_id,
                profile_id,
                profile_generation,
                timeout_ms,
            } => formatter
                .debug_struct("UiCommand::ReconnectProfile")
                .field("operation_id", operation_id)
                .field("profile_id", profile_id)
                .field("profile_generation", profile_generation)
                .field("timeout_ms", timeout_ms)
                .finish(),
            Self::ShutdownRuntime { operation_id } => formatter
                .debug_struct("UiCommand::ShutdownRuntime")
                .field("operation_id", operation_id)
                .finish(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CommandLane {
    Work,
    Mutation,
    Workspace,
    Control,
    Shutdown,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum ControlKey {
    Cancel(OperationId),
    Disconnect(ProfileId, ProfileGeneration),
    Reconnect(ProfileId, ProfileGeneration),
}

impl UiCommand {
    pub(crate) fn control_key(&self) -> Option<ControlKey> {
        match self {
            Self::CancelOperation { operation_id } => Some(ControlKey::Cancel(*operation_id)),
            Self::DisconnectProfile {
                profile_id,
                profile_generation,
                ..
            } => Some(ControlKey::Disconnect(
                profile_id.clone(),
                *profile_generation,
            )),
            Self::ReconnectProfile {
                profile_id,
                profile_generation,
                ..
            } => Some(ControlKey::Reconnect(
                profile_id.clone(),
                *profile_generation,
            )),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubmitError {
    Busy,
    Disconnected,
}

pub struct UiPort {
    work_tx: mpsc::Sender<UiCommand>,
    mutation_tx: mpsc::Sender<UiCommand>,
    workspace_tx: mpsc::Sender<UiCommand>,
    control_tx: mpsc::Sender<UiCommand>,
    shutdown_tx: watch::Sender<Option<OperationId>>,
    event_rx: mpsc::Receiver<UiEvent>,
    control_keys: Arc<Mutex<HashSet<ControlKey>>>,
}

pub(crate) struct MutationSubmitPermit<'a> {
    permit: mpsc::Permit<'a, UiCommand>,
}

impl MutationSubmitPermit<'_> {
    pub(crate) fn submit(self, command: UiCommand) -> Result<(), SubmitError> {
        if command.lane() != CommandLane::Mutation {
            return Err(SubmitError::Disconnected);
        }
        self.permit.send(command);
        Ok(())
    }
}

#[derive(Clone)]
pub(crate) struct ShutdownRequester {
    shutdown_tx: watch::Sender<Option<OperationId>>,
}

impl ShutdownRequester {
    pub(crate) fn request_shutdown(&self) -> Result<(), SubmitError> {
        self.shutdown_tx
            .send(Some(OperationId(u64::MAX)))
            .map_err(|_| SubmitError::Disconnected)
    }
}

impl UiPort {
    pub(crate) fn shutdown_requester(&self) -> ShutdownRequester {
        ShutdownRequester {
            shutdown_tx: self.shutdown_tx.clone(),
        }
    }

    pub fn try_submit(&self, command: UiCommand) -> Result<(), SubmitError> {
        match command.lane() {
            CommandLane::Work => try_send(&self.work_tx, command),
            CommandLane::Mutation => try_send(&self.mutation_tx, command),
            CommandLane::Workspace => try_send(&self.workspace_tx, command),
            CommandLane::Control => {
                let key = command.control_key().ok_or(SubmitError::Disconnected)?;
                {
                    let mut keys = self
                        .control_keys
                        .lock()
                        .map_err(|_| SubmitError::Disconnected)?;
                    if !keys.insert(key.clone()) {
                        return Ok(());
                    }
                }
                if let Err(error) = try_send(&self.control_tx, command) {
                    if let Ok(mut keys) = self.control_keys.lock() {
                        keys.remove(&key);
                    }
                    return Err(error);
                }
                Ok(())
            }
            CommandLane::Shutdown => {
                let UiCommand::ShutdownRuntime { operation_id } = command else {
                    return Err(SubmitError::Disconnected);
                };
                self.shutdown_tx
                    .send(Some(operation_id))
                    .map_err(|_| SubmitError::Disconnected)
            }
        }
    }

    /// Profile form saves are always mutation-lane commands. Reserve the lane
    /// before constructing a request that can own a secret Arc.
    pub fn try_submit_with(&self, build: impl FnOnce() -> UiCommand) -> Result<(), SubmitError> {
        let permit = self.mutation_tx.try_reserve().map_err(map_try_send_error)?;
        let command = build();
        if command.lane() != CommandLane::Mutation {
            return Err(SubmitError::Disconnected);
        }
        permit.send(command);
        Ok(())
    }

    pub(crate) fn try_reserve_mutation(&self) -> Result<MutationSubmitPermit<'_>, SubmitError> {
        self.mutation_tx
            .try_reserve()
            .map(|permit| MutationSubmitPermit { permit })
            .map_err(map_try_send_error)
    }

    pub fn drain_events(&mut self, limit: usize) -> Vec<UiEvent> {
        let mut events = Vec::with_capacity(limit.min(32));
        for _ in 0..limit {
            match self.event_rx.try_recv() {
                Ok(event) => events.push(event),
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }
        events
    }

    pub async fn next_event(&mut self) -> Option<UiEvent> {
        self.event_rx.recv().await
    }
}

pub struct ServicePort {
    pub(crate) work_rx: mpsc::Receiver<UiCommand>,
    pub(crate) mutation_rx: mpsc::Receiver<UiCommand>,
    pub(crate) workspace_rx: mpsc::Receiver<UiCommand>,
    pub(crate) control_rx: mpsc::Receiver<UiCommand>,
    pub(crate) shutdown_rx: watch::Receiver<Option<OperationId>>,
    event_tx: mpsc::Sender<UiEvent>,
    critical_event_reserve: usize,
    control_keys: Arc<Mutex<HashSet<ControlKey>>>,
}

impl ServicePort {
    pub async fn next_command(&mut self) -> Option<UiCommand> {
        tokio::select! {
            biased;
            command = self.control_rx.recv() => command,
            command = self.mutation_rx.recv() => command,
            command = self.workspace_rx.recv() => command,
            command = self.work_rx.recv() => command,
        }
    }

    pub async fn emit(&self, event: UiEvent) -> bool {
        self.release_for_event(&event);
        self.event_tx.send(event).await.is_ok()
    }

    pub(crate) fn try_emit(&self, event: UiEvent) -> bool {
        self.release_for_event(&event);
        if self.event_tx.capacity() <= self.critical_event_reserve {
            return false;
        }
        self.event_tx.try_send(event).is_ok()
    }

    fn release_for_event(&self, event: &UiEvent) {
        let mut keys = match self.control_keys.lock() {
            Ok(keys) => keys,
            Err(_) => return,
        };
        let operation_id = match event {
            UiEvent::ProfilesLoaded { operation_id, .. }
            | UiEvent::ProfilesFailed { operation_id, .. }
            | UiEvent::ProfileSaved { operation_id, .. }
            | UiEvent::ProfileCreateFailed { operation_id, .. }
            | UiEvent::ProfileUpdateFailed { operation_id, .. }
            | UiEvent::CredentialsStored { operation_id, .. }
            | UiEvent::CredentialsStoreFailed { operation_id, .. }
            | UiEvent::ConnectionReady { operation_id, .. }
            | UiEvent::ConnectionClosed { operation_id, .. }
            | UiEvent::DraftConnectionReady { operation_id, .. }
            | UiEvent::DraftOperationFailed { operation_id, .. }
            | UiEvent::QueryFinished { operation_id, .. }
            | UiEvent::QueryBatchFinished { operation_id, .. }
            | UiEvent::ResultExported { operation_id, .. }
            | UiEvent::ResultExportFailed { operation_id, .. }
            | UiEvent::OperationFailed { operation_id, .. }
            | UiEvent::ExecuteUnavailable { operation_id, .. }
            | UiEvent::ProfileDeleted { operation_id, .. }
            | UiEvent::WorkspaceLoaded { operation_id, .. }
            | UiEvent::WorkspaceCommitted { operation_id, .. }
            | UiEvent::WorkspaceCleared { operation_id, .. }
            | UiEvent::WorkspaceCommitSuperseded { operation_id, .. }
            | UiEvent::WorkspaceOperationFailed { operation_id, .. }
            | UiEvent::ConfigUncertain { operation_id }
            | UiEvent::RuntimeShutdown { operation_id } => *operation_id,
            UiEvent::CatalogPageLoaded { page, .. } => page.identity.operation_id,
            UiEvent::CatalogPageFailed { request, .. } => request.operation_id(),
            UiEvent::RedisKeysLoaded { page, .. } => page.identity.operation_id,
            UiEvent::RedisKeysFailed { request, .. } => request.operation_id(),
            UiEvent::RedisKeyInspected { preview, .. } => preview.identity.operation_id,
            UiEvent::RedisKeyInspectFailed { request, .. } => request.operation_id(),
        };
        keys.remove(&ControlKey::Cancel(operation_id));
        match event {
            UiEvent::ConnectionReady {
                profile_id,
                profile_generation,
                ..
            } => {
                keys.remove(&ControlKey::Reconnect(
                    profile_id.clone(),
                    *profile_generation,
                ));
            }
            UiEvent::ConnectionClosed {
                profile_id,
                profile_generation,
                ..
            } => {
                keys.remove(&ControlKey::Disconnect(
                    profile_id.clone(),
                    *profile_generation,
                ));
            }
            UiEvent::OperationFailed {
                profile_id,
                profile_generation,
                kind,
                ..
            } => match kind {
                OperationKind::DisconnectProfile => {
                    keys.remove(&ControlKey::Disconnect(
                        profile_id.clone(),
                        *profile_generation,
                    ));
                }
                OperationKind::ReconnectProfile => {
                    keys.remove(&ControlKey::Reconnect(
                        profile_id.clone(),
                        *profile_generation,
                    ));
                }
                _ => {}
            },
            _ => {}
        }
    }

    pub(crate) fn release_control_key(&self, key: &ControlKey) {
        if let Ok(mut keys) = self.control_keys.lock() {
            keys.remove(key);
        }
    }

    pub(crate) fn close_and_drain_for_shutdown(&mut self) -> Vec<UiCommand> {
        self.work_rx.close();
        self.mutation_rx.close();
        self.workspace_rx.close();
        self.control_rx.close();
        while self.work_rx.try_recv().is_ok() {}
        while self.mutation_rx.try_recv().is_ok() {}
        while self.control_rx.try_recv().is_ok() {}
        let mut workspace = Vec::with_capacity(WORKSPACE_CAPACITY);
        while let Ok(command) = self.workspace_rx.try_recv() {
            workspace.push(command);
        }
        workspace
    }

    #[cfg(test)]
    pub fn try_next_command(&mut self) -> Option<UiCommand> {
        self.control_rx
            .try_recv()
            .ok()
            .or_else(|| self.mutation_rx.try_recv().ok())
            .or_else(|| self.workspace_rx.try_recv().ok())
            .or_else(|| self.work_rx.try_recv().ok())
    }
}

fn try_send(sender: &mpsc::Sender<UiCommand>, command: UiCommand) -> Result<(), SubmitError> {
    sender.try_send(command).map_err(map_try_send_error)
}

fn map_try_send_error<T>(error: TrySendError<T>) -> SubmitError {
    match error {
        TrySendError::Full(_) => SubmitError::Busy,
        TrySendError::Closed(_) => SubmitError::Disconnected,
    }
}

#[must_use]
pub fn controller_ports() -> (UiPort, ServicePort) {
    ports_with_capacities(
        WORK_CAPACITY,
        MUTATION_CAPACITY,
        WORKSPACE_CAPACITY,
        CONTROL_CAPACITY,
        EVENT_CAPACITY,
    )
}

#[cfg(test)]
pub(crate) fn controller_ports_with_event_capacity(event_capacity: usize) -> (UiPort, ServicePort) {
    ports_with_capacities(
        WORK_CAPACITY,
        MUTATION_CAPACITY,
        WORKSPACE_CAPACITY,
        CONTROL_CAPACITY,
        event_capacity.max(1),
    )
}

#[must_use]
pub fn bounded_ports(capacity: usize) -> (UiPort, ServicePort) {
    let capacity = capacity.max(1);
    ports_with_capacities(capacity, capacity, capacity, capacity, capacity)
}

fn ports_with_capacities(
    work_capacity: usize,
    mutation_capacity: usize,
    workspace_capacity: usize,
    control_capacity: usize,
    event_capacity: usize,
) -> (UiPort, ServicePort) {
    let (work_tx, work_rx) = mpsc::channel(work_capacity);
    let (mutation_tx, mutation_rx) = mpsc::channel(mutation_capacity);
    let (workspace_tx, workspace_rx) = mpsc::channel(workspace_capacity);
    let (control_tx, control_rx) = mpsc::channel(control_capacity);
    let critical_event_reserve = workspace_capacity.saturating_mul(2).saturating_add(2);
    let physical_event_capacity = event_capacity.saturating_add(critical_event_reserve);
    let (event_tx, event_rx) = mpsc::channel(physical_event_capacity);
    let (shutdown_tx, shutdown_rx) = watch::channel(None);
    let control_keys = Arc::new(Mutex::new(HashSet::new()));
    (
        UiPort {
            work_tx,
            mutation_tx,
            workspace_tx,
            control_tx,
            shutdown_tx,
            event_rx,
            control_keys: control_keys.clone(),
        },
        ServicePort {
            work_rx,
            mutation_rx,
            workspace_rx,
            control_rx,
            shutdown_rx,
            event_tx,
            critical_event_reserve,
            control_keys,
        },
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use crate::model::{OperationId, ProfileGeneration, ProfileId, ProfileInstanceId};
    use crate::workspace::{
        EditorTabSnapshot, ProfileWorkspaceSnapshot, WorkspaceGeometrySnapshot, WorkspaceLanguage,
        WorkspaceStoreMode,
    };

    use super::{SubmitError, UiCommand, bounded_ports};
    use crate::ui::{UiEvent, WorkspaceIdentity};

    fn workspace_fixture(
        operation_id: u64,
        revision: u64,
        profile_id: &str,
        source: &str,
    ) -> UiCommand {
        let instance_id = ProfileInstanceId::from_bytes([7; 16]);
        let editor = EditorTabSnapshot::new(
            1,
            "private title",
            WorkspaceLanguage::Sql,
            source,
            None,
            source.chars().count(),
            None,
        )
        .expect("valid editor fixture");
        let snapshot = ProfileWorkspaceSnapshot::new(
            instance_id,
            ProfileId(profile_id.to_owned()),
            true,
            vec![editor],
            Some(1),
            WorkspaceGeometrySnapshot::new(240.0, 0.6, false).expect("valid geometry fixture"),
            Vec::new(),
        )
        .expect("valid workspace fixture");
        UiCommand::CommitWorkspace {
            operation_id: OperationId(operation_id),
            identity: WorkspaceIdentity::new(
                ProfileId(profile_id.to_owned()),
                ProfileGeneration(1),
                instance_id,
            ),
            revision,
            snapshot: Box::new(snapshot),
        }
    }

    #[test]
    fn full_mutation_channel_is_busy_instead_of_blocking() {
        let (ui, _service) = bounded_ports(1);
        let refresh = || UiCommand::RefreshProfiles {
            operation_id: OperationId(1),
        };
        assert_eq!(ui.try_submit(refresh()), Ok(()));
        assert_eq!(ui.try_submit(refresh()), Err(SubmitError::Busy));
    }

    #[test]
    fn full_channel_does_not_build_or_move_a_sensitive_command() {
        let (ui, _service) = bounded_ports(1);
        assert_eq!(
            ui.try_submit(UiCommand::RefreshProfiles {
                operation_id: OperationId(1),
            }),
            Ok(())
        );
        let built = Arc::new(AtomicBool::new(false));
        let marker = built.clone();
        assert_eq!(
            ui.try_submit_with(move || {
                marker.store(true, Ordering::SeqCst);
                UiCommand::RefreshProfiles {
                    operation_id: OperationId(2),
                }
            }),
            Err(SubmitError::Busy)
        );
        assert!(!built.load(Ordering::SeqCst));
    }

    #[test]
    fn workspace_lane_is_independently_bounded_and_reports_busy() {
        let (ui, mut service) = bounded_ports(1);
        assert_eq!(
            ui.try_submit(workspace_fixture(10, 1, "workspace", "select 1")),
            Ok(())
        );
        assert_eq!(
            ui.try_submit(workspace_fixture(11, 2, "workspace", "select 2")),
            Err(SubmitError::Busy)
        );
        assert_eq!(
            ui.try_submit(UiCommand::RefreshProfiles {
                operation_id: OperationId(12),
            }),
            Ok(()),
            "workspace pressure must not consume mutation capacity"
        );
        assert!(matches!(
            service.try_next_command(),
            Some(UiCommand::RefreshProfiles { .. })
        ));
        assert!(matches!(
            service.try_next_command(),
            Some(UiCommand::CommitWorkspace { .. })
        ));
    }

    #[test]
    fn workspace_command_debug_redacts_identity_and_snapshot_payload() {
        const PROFILE_SENTINEL: &str = "PROFILE_MUST_NOT_LEAK";
        const SOURCE_SENTINEL: &str = "SOURCE_MUST_NOT_LEAK";
        let debug = format!(
            "{:?}",
            workspace_fixture(20, 9, PROFILE_SENTINEL, SOURCE_SENTINEL)
        );
        assert!(!debug.contains(PROFILE_SENTINEL));
        assert!(!debug.contains(SOURCE_SENTINEL));
        assert!(debug.contains("<redacted>"));
        assert!(debug.contains("revision: 9"));

        let UiCommand::CommitWorkspace {
            identity, snapshot, ..
        } = workspace_fixture(21, 9, PROFILE_SENTINEL, SOURCE_SENTINEL)
        else {
            unreachable!("workspace fixture is always a commit")
        };
        let event_debug = format!(
            "{:?}",
            UiEvent::WorkspaceLoaded {
                operation_id: OperationId(21),
                identity,
                base_revision: 9,
                mode: WorkspaceStoreMode::ReadWrite,
                read_only_reason: None,
                generation: Some(1),
                committed_bytes: 1,
                snapshot: Some(snapshot),
            }
        );
        assert!(!event_debug.contains(PROFILE_SENTINEL));
        assert!(!event_debug.contains(SOURCE_SENTINEL));
        assert!(event_debug.contains("<redacted>"));
    }
}
