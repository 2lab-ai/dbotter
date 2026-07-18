//! Native three-zone UI. Rendering and state folding perform no I/O.

use std::collections::{HashMap, HashSet, VecDeque};
use std::convert::Infallible;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Utc};
use eframe::egui;

use crate::config::{ConfigSourceVersion, MigrationConsent};
use crate::export_file::confirm_replace;
use crate::model::{
    CatalogRequest, CredentialMode, DEFAULT_CATALOG_PAGE_SIZE, DEFAULT_CATALOG_TIMEOUT,
    DEFAULT_REDIS_SCAN_COUNT, DraftId, DriverAvailability, DriverCapabilities, DriverKind,
    ExportFormat, ExportResult, OperationId, OperationKind, OperationRecipeId, OverwritePolicy,
    ProfileAccess, ProfileEnvironment, ProfileFieldId, ProfileGeneration, ProfileId, PublicCode,
    PublicSummary, RedisKeyInspectRequest, RedisScanRequest, RequestIdentity, ResultId,
    ResultSnapshot, SessionGeneration,
};
use crate::public_error::{
    PublicOperationError, RecoveryAction, RecoveryCommand, RecoveryCommandDispatcher,
    dispatch_recovery,
};
use crate::secrets::{EnvironmentAvailability, ReplacementSecretBuffer};
use crate::service::DeleteProfileRequest;
use crate::workspace::{
    EncodedProfileByteAccounting, MAX_HISTORY_ENTRIES_PER_PROFILE, MAX_HISTORY_ENTRIES_TOTAL,
    ProfileWorkspaceSnapshot, WorkspaceGeometrySnapshot, WorkspaceHistoryCode,
    WorkspaceHistoryEntry, WorkspaceHistoryStatus, WorkspaceReadOnlyReason,
    WorkspaceRetentionError, WorkspaceRetentionLimit, WorkspaceRunTarget, WorkspaceSnapshotSet,
    WorkspaceStoreMode, conservative_encoded_profile_bytes,
};

use super::accessibility::{
    named_author_id, named_author_id_with_label, named_dynamic_author_id,
    named_dynamic_value_author_id,
};
use super::adapter::{SubmitError, UiCommand, UiPort};
use super::editor::{
    EDITOR_INPUT_ID, EDITOR_ROW_LIMIT_ID, EDITOR_TIMEOUT_ID, EditorCursor,
    EditorExecuteBatchIntent, EditorExecuteIntent, EditorIntent, EditorSurface,
    build_execute_intent,
};
use super::layout::{
    CompactFallback, FallbackSurface, LayoutMode, NativeLayout, Pane, SplitLayout,
    WorkspaceGeometry,
};
use super::model::{
    ConnectionFailureOutcome, ConnectionState, EditorTabError, EditorTabId, ProfileSnapshot,
    ProfileWorkspace, ProfileWorkspacePersistence, ResultAreaTab, UiEvent, UiModel,
    WorkspaceAction, WorkspaceFailureCode, WorkspaceIdentity, WorkspaceKey,
};
use super::mysql_explorer::{MySqlExplorerIntent, MySqlExplorerState};
use super::profile_form::{
    DraftTestAttempt, FormAction, ProfileEditor, ProfileEventResult, SaveAttempt,
};
use super::redis_explorer::{RedisExplorer, RedisExplorerIntent};
use super::result_view::ResultViewIntent;
use super::theme::OpenAiTheme;

const EVENT_DRAIN_LIMIT: usize = 128;
const RETRY_RECIPE_LIMIT: usize = 64;
const WORKSPACE_GEOMETRY_STORAGE_KEY: &str = "dbotter.workspace-geometry.v1";
const MAX_RETAINED_WORKSPACE_GEOMETRIES: usize = 128;
const MAX_WORKSPACE_GEOMETRY_STORAGE_BYTES: usize = 64 * 1024;
pub const DEFAULT_EXECUTE_ROW_LIMIT: u32 = 500;
const DEFAULT_TIMEOUT_MS: u64 = 30_000;
const WORKSPACE_AUTOSAVE_DEBOUNCE: Duration = Duration::from_millis(750);
const WORKSPACE_SAVE_RETRY_DELAY: Duration = Duration::from_millis(250);
const WORKSPACE_EDITOR_COLLAPSED_SHARE: f32 = 0.1;
const WORKSPACE_RESULTS_COLLAPSED_SHARE: f32 = 0.9;
const MAX_RETENTION_COMMIT_QUEUE: usize = MAX_HISTORY_ENTRIES_TOTAL + 1;
const MAX_PENDING_WORKSPACE_HISTORY: usize = MAX_HISTORY_ENTRIES_TOTAL;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ActiveOperation {
    operation_id: OperationId,
    profile_generation: ProfileGeneration,
    kind: OperationKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EditorTabAction {
    Rename(EditorTabId),
    New,
    Duplicate(EditorTabId),
    MoveLeft(EditorTabId),
    MoveRight(EditorTabId),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum WorkspaceLoadPhase {
    Unloaded,
    Loading {
        operation_id: OperationId,
        base_revision: u64,
        restore_allowed: bool,
    },
    Ready,
    Failed(WorkspaceFailureCode),
    Conflict,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum WorkspaceSavePhase {
    Idle,
    Saving {
        operation_id: OperationId,
        revision: u64,
    },
    Failed {
        revision: u64,
        code: WorkspaceFailureCode,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum WorkspaceClearPhase {
    Idle,
    Pending {
        operation_id: OperationId,
        revision: u64,
    },
    Failed {
        revision: u64,
        code: WorkspaceFailureCode,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SubmittedWorkspaceCommit {
    operation_id: OperationId,
    revision: u64,
    accounting: EncodedProfileByteAccounting,
}

impl WorkspaceClearPhase {
    fn has_intent(&self) -> bool {
        !matches!(self, Self::Idle)
    }
}

#[derive(Clone, Debug)]
struct WorkspacePersistenceState {
    identity: WorkspaceIdentity,
    load: WorkspaceLoadPhase,
    mode: Option<WorkspaceStoreMode>,
    read_only_reason: Option<WorkspaceReadOnlyReason>,
    save: WorkspaceSavePhase,
    clear: WorkspaceClearPhase,
    observed_revision: u64,
    dirty_since: Option<Instant>,
    retry_not_before: Option<Instant>,
    force_commit_until_success: bool,
    resolve_conflict_on_commit: bool,
    clean_empty_baseline_pending: Option<u64>,
    restore_baseline_revision: Option<u64>,
    durable_generation: Option<u64>,
    durable_committed_bytes: u64,
    submitted_commit: Option<SubmittedWorkspaceCommit>,
    refresh_durable_baseline_only: bool,
}

impl WorkspacePersistenceState {
    fn new(
        identity: WorkspaceIdentity,
        revision: u64,
        restore_baseline_revision: Option<u64>,
    ) -> Self {
        Self {
            identity,
            load: WorkspaceLoadPhase::Unloaded,
            mode: None,
            read_only_reason: None,
            save: WorkspaceSavePhase::Idle,
            clear: WorkspaceClearPhase::Idle,
            observed_revision: revision,
            dirty_since: None,
            retry_not_before: None,
            force_commit_until_success: false,
            resolve_conflict_on_commit: false,
            clean_empty_baseline_pending: None,
            restore_baseline_revision,
            durable_generation: None,
            durable_committed_bytes: 0,
            submitted_commit: None,
            refresh_durable_baseline_only: false,
        }
    }

    fn is_read_only(&self) -> bool {
        self.mode == Some(WorkspaceStoreMode::ReadOnly)
    }

    fn load_can_restore(&self) -> bool {
        !matches!(self.load, WorkspaceLoadPhase::Ready)
    }
}

#[derive(Clone)]
struct PendingWorkspaceHistory {
    workspace_key: WorkspaceKey,
    instance_id: crate::model::ProfileInstanceId,
    history_id: u64,
    source: String,
    target: WorkspaceRunTarget,
    started_at: Instant,
    terminal: Option<WorkspaceHistoryTerminal>,
}

struct RetentionCommitRequest {
    workspace_key: WorkspaceKey,
    identity: WorkspaceIdentity,
    revision: u64,
    accounting: EncodedProfileByteAccounting,
    snapshot: Box<ProfileWorkspaceSnapshot>,
}

impl std::fmt::Debug for RetentionCommitRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RetentionCommitRequest")
            .field("workspace_key", &self.workspace_key)
            .field("identity", &self.identity)
            .field("revision", &self.revision)
            .field("history_count", &self.snapshot.history().len())
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ActiveRetentionCommit {
    workspace_key: WorkspaceKey,
    operation_id: OperationId,
    revision: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RetentionBarrierFailure {
    Submit(SubmitError),
    Operation(WorkspaceFailureCode),
    IdentityChanged,
    QueueLimit,
    Planning,
}

#[derive(Default)]
struct RetentionCommitBarrier {
    queue: VecDeque<RetentionCommitRequest>,
    active: Option<ActiveRetentionCommit>,
    failure: Option<RetentionBarrierFailure>,
}

impl std::fmt::Debug for RetentionCommitBarrier {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RetentionCommitBarrier")
            .field("queued", &self.queue.len())
            .field("active", &self.active)
            .field("failure", &self.failure)
            .finish()
    }
}

impl RetentionCommitBarrier {
    fn is_active(&self) -> bool {
        self.active.is_some() || !self.queue.is_empty()
    }

    fn is_pending_or_failed(&self) -> bool {
        self.is_active() || self.failure.is_some()
    }
}

struct WorkspaceRetentionPlan {
    workspaces: Vec<(WorkspaceKey, ProfileWorkspace)>,
    commit_requests: Vec<RetentionCommitRequest>,
    reserved_history_id: Option<u64>,
    local_only: bool,
}

impl std::fmt::Debug for WorkspaceRetentionPlan {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WorkspaceRetentionPlan")
            .field("workspace_updates", &self.workspaces.len())
            .field("commit_requests", &self.commit_requests.len())
            .field("reserved_history_id", &self.reserved_history_id)
            .field("local_only", &self.local_only)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RetentionPlanFailure {
    RestoreUnresolved,
    PersistenceTransition,
    SaveInFlight,
    BarrierActive,
    MissingWorkspace,
    InvalidSnapshot,
    Retention(WorkspaceRetentionError),
}

enum RetentionHistoryChange {
    Reserve {
        workspace_key: WorkspaceKey,
        seed: u64,
        source: String,
        target: WorkspaceRunTarget,
        started_at_unix_ms: i64,
    },
    Terminal {
        workspace_key: WorkspaceKey,
        history_id: u64,
        source: String,
        target: WorkspaceRunTarget,
        terminal: WorkspaceHistoryTerminal,
    },
}

#[derive(Clone, Copy, Debug)]
struct WorkspaceHistoryTerminal {
    status: WorkspaceHistoryStatus,
    completed_at_unix_ms: i64,
    duration_ms: u64,
    returned_rows: u64,
    affected_rows: u64,
    truncated: bool,
}

impl std::fmt::Debug for PendingWorkspaceHistory {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PendingWorkspaceHistory")
            .field("workspace_key", &self.workspace_key)
            .field("instance_id", &"<redacted>")
            .field("history_id", &self.history_id)
            .field("source", &"<redacted>")
            .field("target", &self.target)
            .field("started_at", &self.started_at)
            .field("terminal_buffered", &self.terminal.is_some())
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum WorkspaceCloseGuard {
    #[default]
    Closed,
    AwaitingSave,
    SaveFailed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ModalKind {
    Delete,
    EditorDiscard,
    Credential,
    WorkspaceClear,
    WorkspaceConflict,
    WorkspaceClose,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RedisResourceEventDisposition {
    NotRedis,
    Apply,
    Ignore,
    StaleTerminal(OperationId),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PendingDelete {
    operation_id: OperationId,
    profile_generation: ProfileGeneration,
    prior_active: Option<ActiveOperation>,
    prior_finished: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum RetryRecipe {
    Connect {
        profile_id: ProfileId,
        profile_generation: ProfileGeneration,
        timeout_ms: u64,
    },
    Reconnect {
        profile_id: ProfileId,
        profile_generation: ProfileGeneration,
        timeout_ms: u64,
    },
    Catalog(CatalogRequest),
    RedisScan {
        request: RedisScanRequest,
        restart: bool,
    },
    RedisInspect(RedisKeyInspectRequest),
}

impl RetryRecipe {
    fn profile_id(&self) -> &ProfileId {
        match self {
            Self::Connect { profile_id, .. } | Self::Reconnect { profile_id, .. } => profile_id,
            Self::Catalog(request) => request.profile_id(),
            Self::RedisScan { request, .. } => request.profile_id(),
            Self::RedisInspect(request) => request.profile_id(),
        }
    }

    const fn profile_generation(&self) -> ProfileGeneration {
        match self {
            Self::Connect {
                profile_generation, ..
            }
            | Self::Reconnect {
                profile_generation, ..
            } => *profile_generation,
            Self::Catalog(request) => request.profile_generation(),
            Self::RedisScan { request, .. } => request.profile_generation(),
            Self::RedisInspect(request) => request.profile_generation(),
        }
    }

    const fn operation_kind(&self) -> OperationKind {
        match self {
            Self::Connect { .. } => OperationKind::ConnectProfile,
            Self::Reconnect { .. } => OperationKind::ReconnectProfile,
            Self::Catalog(_) => OperationKind::BrowseMySql,
            Self::RedisScan { .. } => OperationKind::BrowseRedis,
            Self::RedisInspect(_) => OperationKind::InspectRedis,
        }
    }
}

#[derive(Default)]
struct RetryRecipeRegistry {
    recipes: HashMap<OperationRecipeId, RetryRecipe>,
    order: VecDeque<OperationRecipeId>,
}

impl RetryRecipeRegistry {
    fn register(&mut self, operation_id: OperationId, recipe: RetryRecipe) -> OperationRecipeId {
        let recipe_id = OperationRecipeId(operation_id.0);
        if self.recipes.insert(recipe_id, recipe).is_some() {
            self.order.retain(|existing| *existing != recipe_id);
        }
        self.order.push_back(recipe_id);
        while self.order.len() > RETRY_RECIPE_LIMIT {
            if let Some(expired) = self.order.pop_front() {
                self.recipes.remove(&expired);
            }
        }
        recipe_id
    }

    fn contains(&self, recipe_id: OperationRecipeId) -> bool {
        self.recipes.contains_key(&recipe_id)
    }

    fn get(&self, recipe_id: OperationRecipeId) -> Option<&RetryRecipe> {
        self.recipes.get(&recipe_id)
    }

    fn take(&mut self, recipe_id: OperationRecipeId) -> Option<RetryRecipe> {
        self.order.retain(|existing| *existing != recipe_id);
        self.recipes.remove(&recipe_id)
    }

    fn remove(&mut self, recipe_id: OperationRecipeId) {
        let _ = self.take(recipe_id);
    }

    fn retain_current(&mut self, generations: &HashMap<ProfileId, ProfileGeneration>) {
        self.recipes.retain(|_, recipe| {
            generations.get(recipe.profile_id()).copied() == Some(recipe.profile_generation())
        });
        self.order
            .retain(|recipe_id| self.recipes.contains_key(recipe_id));
    }

    fn clear(&mut self) {
        self.recipes.clear();
        self.order.clear();
    }
}

struct CredentialPrompt {
    profile_id: ProfileId,
    profile_generation: ProfileGeneration,
    source_operation: OperationKind,
    retry_recipe_id: Option<OperationRecipeId>,
    store_operation_id: Option<OperationId>,
    secret: ReplacementSecretBuffer,
    status: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct VisibleError {
    operation_id: OperationId,
    error: PublicOperationError,
}

#[derive(Clone, Copy)]
struct RecoveryDispatchContext {
    source_operation_id: OperationId,
    source_operation: OperationKind,
    code: PublicCode,
}

const fn delete_failure_is_known_non_committed(summary: PublicSummary) -> bool {
    matches!(
        summary,
        PublicSummary::InvalidInput
            | PublicSummary::ResourceBusy
            | PublicSummary::ResourceStale
            | PublicSummary::ConfigWriteNotCommitted
    )
}

const fn workspace_failure_is_transient(code: WorkspaceFailureCode) -> bool {
    matches!(
        code,
        WorkspaceFailureCode::Busy
            | WorkspaceFailureCode::Stale
            | WorkspaceFailureCode::Unavailable
    )
}

fn operation_status_claims_workspace_saved(status: &str) -> bool {
    status.starts_with("Private workspace Saved") || status == "Private workspace is Saved."
}

#[derive(Clone, PartialEq, Eq)]
struct DeleteConfirmation {
    profile_id: ProfileId,
    profile_generation: ProfileGeneration,
    profile_name: String,
    active_kind: Option<OperationKind>,
    migration_backup: Option<PathBuf>,
    migration_confirmed: bool,
}

impl std::fmt::Debug for DeleteConfirmation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DeleteConfirmation")
            .field("profile_id", &"<redacted>")
            .field("profile_generation", &self.profile_generation)
            .field("profile_name", &"<redacted>")
            .field("active_kind", &self.active_kind)
            .field(
                "migration_backup",
                &self.migration_backup.as_ref().map(|_| "<redacted>"),
            )
            .field("migration_confirmed", &self.migration_confirmed)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DeleteDialogAction {
    Cancel,
    Confirm,
}

#[derive(Clone, PartialEq, Eq)]
struct EditorDiscardConfirmation {
    workspace_key: WorkspaceKey,
    tab_id: EditorTabId,
    title: String,
    discard_author_id: &'static str,
}

impl std::fmt::Debug for EditorDiscardConfirmation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EditorDiscardConfirmation")
            .field("workspace_key", &self.workspace_key)
            .field("tab_id", &self.tab_id)
            .field("title", &"<redacted>")
            .field("discard_author_id", &self.discard_author_id)
            .finish()
    }
}

struct PendingExportDestination {
    result_id: ResultId,
    format: ExportFormat,
    path: PathBuf,
}

pub struct DbotterApp {
    port: UiPort,
    model: UiModel,
    mysql_explorers: HashMap<(ProfileId, ProfileGeneration), MySqlExplorerState>,
    profile_editor: Option<ProfileEditor>,
    editor_surface: EditorSurface,
    redis_explorers: HashMap<WorkspaceKey, RedisExplorer>,
    visible_redis_workspace: Option<WorkspaceKey>,
    first_run_driver: DriverKind,
    active_operations: HashMap<ProfileId, ActiveOperation>,
    pending_deletes: HashMap<ProfileId, PendingDelete>,
    retry_recipes: RetryRecipeRegistry,
    credential_prompt: Option<CredentialPrompt>,
    common_error: Option<VisibleError>,
    recovery_dispatch_context: Option<RecoveryDispatchContext>,
    delete_confirmation: Option<DeleteConfirmation>,
    editor_discard_confirmation: Option<EditorDiscardConfirmation>,
    next_draft_id: u64,
    pending_connect_after_refresh: Option<(ProfileId, OperationId)>,
    pending_export_destinations: HashMap<OperationId, PendingExportDestination>,
    committed_export_destinations: HashMap<ResultId, PathBuf>,
    result_export_formats: HashMap<ResultId, ExportFormat>,
    connection_filter: String,
    workspace_persistence: HashMap<WorkspaceKey, WorkspacePersistenceState>,
    pending_workspace_history: HashMap<OperationId, PendingWorkspaceHistory>,
    retention_commit_barrier: RetentionCommitBarrier,
    retention_reconcile_required: bool,
    workspace_history_search: HashMap<WorkspaceKey, String>,
    workspace_history_focus: Option<WorkspaceKey>,
    connection_filter_focus: bool,
    workspace_close_guard: WorkspaceCloseGuard,
    discard_local_changes_on_close: bool,
    workspace_clear_confirmation: Option<WorkspaceKey>,
    workspace_restore_conflict_confirmation: Option<WorkspaceKey>,
    close_after_restore_conflict_confirmation: bool,
    workspace_geometries: HashMap<WorkspaceKey, WorkspaceGeometry>,
    collapsed_workspace_panes: HashMap<WorkspaceKey, Pane>,
    compact_fallback: CompactFallback,
    compact_restore_focus: Option<egui::Id>,
    compact_workspace: Option<WorkspaceKey>,
    focused_modal: Option<ModalKind>,
    frame_workspace_dirty_hint: bool,
}

impl DbotterApp {
    pub fn new(port: UiPort) -> Self {
        Self::new_with_storage(port, None)
    }

    pub fn new_with_storage(port: UiPort, storage: Option<&dyn eframe::Storage>) -> Self {
        let workspace_geometries = restore_workspace_geometries(storage);
        let mut app = Self {
            port,
            model: UiModel::default(),
            mysql_explorers: HashMap::new(),
            profile_editor: None,
            editor_surface: EditorSurface::default(),
            redis_explorers: HashMap::new(),
            visible_redis_workspace: None,
            first_run_driver: DriverKind::MySql,
            active_operations: HashMap::new(),
            pending_deletes: HashMap::new(),
            retry_recipes: RetryRecipeRegistry::default(),
            credential_prompt: None,
            common_error: None,
            recovery_dispatch_context: None,
            delete_confirmation: None,
            editor_discard_confirmation: None,
            next_draft_id: 1,
            pending_connect_after_refresh: None,
            pending_export_destinations: HashMap::new(),
            committed_export_destinations: HashMap::new(),
            result_export_formats: HashMap::new(),
            connection_filter: String::new(),
            workspace_persistence: HashMap::new(),
            pending_workspace_history: HashMap::new(),
            retention_commit_barrier: RetentionCommitBarrier::default(),
            retention_reconcile_required: true,
            workspace_history_search: HashMap::new(),
            workspace_history_focus: None,
            connection_filter_focus: false,
            workspace_close_guard: WorkspaceCloseGuard::Closed,
            discard_local_changes_on_close: false,
            workspace_clear_confirmation: None,
            workspace_restore_conflict_confirmation: None,
            close_after_restore_conflict_confirmation: false,
            workspace_geometries,
            collapsed_workspace_panes: HashMap::new(),
            compact_fallback: CompactFallback::default(),
            compact_restore_focus: None,
            compact_workspace: None,
            focused_modal: None,
            frame_workspace_dirty_hint: false,
        };
        let operation_id = app.model.next_operation();
        let _ = app
            .port
            .try_submit(UiCommand::RefreshProfiles { operation_id });
        app
    }

    fn allocate_draft_id(&mut self) -> DraftId {
        let draft_id = DraftId(self.next_draft_id);
        self.next_draft_id = self.next_draft_id.saturating_add(1);
        draft_id
    }

    fn configured_profile_editor(&self, mut editor: ProfileEditor) -> ProfileEditor {
        editor.set_migration_presentation(
            self.model.config.migration_required(),
            self.model.config.migration_backup(),
        );
        editor
    }

    fn workspace_identity_for(&self, profile: &ProfileSnapshot) -> Option<WorkspaceIdentity> {
        if self.model.config.source_version() != ConfigSourceVersion::V3 {
            return None;
        }
        Some(WorkspaceIdentity::new(
            profile.id.clone(),
            profile.generation,
            profile.persisted.safety.instance_id()?,
        ))
    }

    fn workspace_geometry_snapshot(&self, key: &WorkspaceKey) -> Option<WorkspaceGeometrySnapshot> {
        let geometry = self
            .workspace_geometries
            .get(key)
            .copied()
            .unwrap_or_else(WorkspaceGeometry::default);
        WorkspaceGeometrySnapshot::new(
            geometry.navigator_width(),
            geometry.editor_share().clamp(
                WORKSPACE_EDITOR_COLLAPSED_SHARE,
                WORKSPACE_RESULTS_COLLAPSED_SHARE,
            ),
            geometry.inspector_visible(),
        )
        .ok()
    }

    /// Installs only the in-memory persistence identity. It is intentionally
    /// idempotent and never submits work, so save/logic/execute paths may call
    /// it while the renderer remains side-effect free.
    fn ensure_workspace_persistence_binding(
        &mut self,
        key: &WorkspaceKey,
        profile: &ProfileSnapshot,
    ) -> bool {
        let Some(identity) = self.workspace_identity_for(profile) else {
            return false;
        };
        if identity.profile_id() != &key.profile_id
            || identity.profile_generation() != key.profile_generation
        {
            return false;
        }
        let Some(geometry) = self.workspace_geometry_snapshot(key) else {
            self.model.status = "Workspace geometry is outside the durable bounds.".to_owned();
            return false;
        };
        let workspace = self.model.workspace_mut(key.clone());
        let pristine_before_binding =
            workspace.editor_tabs().is_empty() && workspace.editor_text.is_empty();
        if let Some(persistence) = workspace.persistence() {
            if persistence.profile_id() != identity.profile_id()
                || persistence.instance_id() != identity.instance_id()
            {
                self.model.status =
                    "Workspace persistence identity does not match this profile.".to_owned();
                return false;
            }
        } else {
            let Ok(persistence) = ProfileWorkspacePersistence::for_classified_profile(
                &profile.persisted,
                true,
                geometry,
                Vec::new(),
            ) else {
                self.model.status =
                    "Workspace persistence requires a classified v3 profile.".to_owned();
                return false;
            };
            if let Err(error) = workspace.bind_persistence(persistence) {
                self.model.status = error.to_string();
                return false;
            }
        }
        let revision = workspace.revision();
        let restore_baseline_revision = pristine_before_binding.then_some(revision);
        match self.workspace_persistence.get(key) {
            Some(state) if state.identity == identity => {}
            _ => {
                self.workspace_persistence.insert(
                    key.clone(),
                    WorkspacePersistenceState::new(identity, revision, restore_baseline_revision),
                );
            }
        }
        true
    }

    fn ensure_selected_workspace_persistence_binding(&mut self) -> Option<WorkspaceKey> {
        let profile = self.model.selected_profile_snapshot()?.clone();
        let key = WorkspaceKey::new(profile.id.clone(), profile.generation);
        self.ensure_workspace_persistence_binding(&key, &profile)
            .then_some(key)
    }

    fn ensure_current_workspace_persistence_bindings(&mut self) -> Vec<WorkspaceKey> {
        let profiles = self.model.profiles.clone();
        let mut bound = Vec::new();
        for profile in profiles {
            let key = WorkspaceKey::new(profile.id.clone(), profile.generation);
            if self.ensure_workspace_persistence_binding(&key, &profile) {
                bound.push(key);
            }
        }
        self.workspace_persistence.retain(|key, state| {
            self.model.active_generation(&key.profile_id) == Some(key.profile_generation)
                && self.model.profiles.iter().any(|profile| {
                    profile.id == key.profile_id
                        && profile.generation == key.profile_generation
                        && profile.persisted.safety.instance_id()
                            == Some(state.identity.instance_id())
                })
        });
        self.workspace_history_search.retain(|key, _| {
            self.model.active_generation(&key.profile_id) == Some(key.profile_generation)
        });
        bound
    }

    fn prepare_workspace_history_reservation(
        &mut self,
        workspace_key: &WorkspaceKey,
        seed: u64,
        source: String,
        target: WorkspaceRunTarget,
    ) -> Result<Option<WorkspaceRetentionPlan>, RetentionPlanFailure> {
        let persistence_enabled = self
            .model
            .workspace(workspace_key)
            .and_then(ProfileWorkspace::persistence)
            .is_some_and(ProfileWorkspacePersistence::persistence_enabled);
        if !persistence_enabled {
            return Ok(None);
        }
        if self.pending_workspace_history.len() >= MAX_PENDING_WORKSPACE_HISTORY {
            return Err(RetentionPlanFailure::Retention(
                WorkspaceRetentionError::RetentionExhausted(
                    WorkspaceRetentionLimit::TotalHistoryEntries,
                ),
            ));
        }
        if self.retention_commit_barrier.is_pending_or_failed() {
            return Err(RetentionPlanFailure::BarrierActive);
        }
        let keys = self.ensure_current_workspace_persistence_bindings();
        if !keys.iter().any(|key| key == workspace_key) {
            return Err(RetentionPlanFailure::MissingWorkspace);
        }
        for key in &keys {
            let Some(state) = self.workspace_persistence.get(key) else {
                return Err(RetentionPlanFailure::MissingWorkspace);
            };
            if !matches!(state.load, WorkspaceLoadPhase::Ready) || state.mode.is_none() {
                return Err(RetentionPlanFailure::RestoreUnresolved);
            }
            if state.clear.has_intent() || state.force_commit_until_success {
                return Err(RetentionPlanFailure::PersistenceTransition);
            }
            if matches!(state.save, WorkspaceSavePhase::Saving { .. }) {
                return Err(RetentionPlanFailure::SaveInFlight);
            }
        }
        self.build_workspace_retention_plan(
            &keys,
            Some(RetentionHistoryChange::Reserve {
                workspace_key: workspace_key.clone(),
                seed,
                source,
                target,
                started_at_unix_ms: current_unix_time_ms(),
            }),
        )
        .map(Some)
    }

    fn build_workspace_retention_plan(
        &self,
        keys: &[WorkspaceKey],
        change: Option<RetentionHistoryChange>,
    ) -> Result<WorkspaceRetentionPlan, RetentionPlanFailure> {
        let target_key = match change.as_ref() {
            Some(RetentionHistoryChange::Reserve { workspace_key, .. })
            | Some(RetentionHistoryChange::Terminal { workspace_key, .. }) => {
                Some(workspace_key.clone())
            }
            None => None,
        };
        let mut clones = Vec::with_capacity(keys.len());
        let mut input_snapshots = Vec::with_capacity(keys.len());
        let mut instance_to_key = HashMap::with_capacity(keys.len());
        let mut reserved_history_id = None;

        for key in keys {
            let Some(state) = self.workspace_persistence.get(key) else {
                return Err(RetentionPlanFailure::MissingWorkspace);
            };
            let Some(mut workspace) = self.model.workspace(key).cloned() else {
                return Err(RetentionPlanFailure::MissingWorkspace);
            };
            match change.as_ref() {
                Some(RetentionHistoryChange::Reserve {
                    workspace_key,
                    seed,
                    source,
                    target,
                    started_at_unix_ms,
                }) if workspace_key == key => {
                    let Some(persistence) = workspace.persistence() else {
                        return Err(RetentionPlanFailure::MissingWorkspace);
                    };
                    let mut history = persistence.history().to_vec();
                    let history_id = next_workspace_history_id(&history, *seed)
                        .ok_or(RetentionPlanFailure::InvalidSnapshot)?;
                    if history.len() >= MAX_HISTORY_ENTRIES_PER_PROFILE {
                        let removable = history
                            .iter()
                            .enumerate()
                            .filter(|(_, entry)| {
                                entry.status() != WorkspaceHistoryStatus::OutcomeUnknown
                            })
                            .min_by_key(|(_, entry)| (entry.completed_at_unix_ms(), entry.id()))
                            .map(|(index, _)| index)
                            .ok_or(RetentionPlanFailure::Retention(
                                WorkspaceRetentionError::RetentionExhausted(
                                    WorkspaceRetentionLimit::ProfileHistoryEntries,
                                ),
                            ))?;
                        history.remove(removable);
                    }
                    let provisional = WorkspaceHistoryEntry::new(
                        history_id,
                        source,
                        *target,
                        *started_at_unix_ms,
                        WorkspaceHistoryStatus::OutcomeUnknown,
                        0,
                        0,
                        0,
                        false,
                    )
                    .map_err(|_| RetentionPlanFailure::InvalidSnapshot)?;
                    history.push(provisional);
                    workspace
                        .replace_persistence_history(history)
                        .map_err(|_| RetentionPlanFailure::InvalidSnapshot)?;
                    reserved_history_id = Some(history_id);
                }
                Some(RetentionHistoryChange::Terminal {
                    workspace_key,
                    history_id,
                    source,
                    target,
                    terminal,
                }) if workspace_key == key => {
                    let Some(persistence) = workspace.persistence() else {
                        return Err(RetentionPlanFailure::MissingWorkspace);
                    };
                    let mut history = persistence.history().to_vec();
                    let Some(index) = history.iter().position(|entry| entry.id() == *history_id)
                    else {
                        return Err(RetentionPlanFailure::MissingWorkspace);
                    };
                    history[index] = WorkspaceHistoryEntry::new(
                        *history_id,
                        source,
                        *target,
                        terminal.completed_at_unix_ms,
                        terminal.status,
                        terminal.duration_ms,
                        terminal.returned_rows,
                        terminal.affected_rows,
                        terminal.truncated,
                    )
                    .map_err(|_| RetentionPlanFailure::InvalidSnapshot)?;
                    workspace
                        .replace_persistence_history(history)
                        .map_err(|_| RetentionPlanFailure::InvalidSnapshot)?;
                }
                _ => {}
            }
            let snapshot = workspace
                .to_persistence_snapshot()
                .map_err(|_| RetentionPlanFailure::InvalidSnapshot)?;
            if instance_to_key
                .insert(state.identity.instance_id(), key.clone())
                .is_some()
            {
                return Err(RetentionPlanFailure::InvalidSnapshot);
            }
            input_snapshots.push(snapshot);
            clones.push((key.clone(), workspace));
        }

        let planned = plan_workspace_snapshot_set(input_snapshots)
            .map_err(RetentionPlanFailure::Retention)?;
        let eviction_order = planned
            .history_evictions()
            .iter()
            .filter_map(|identity| instance_to_key.get(&identity.instance_id()).cloned())
            .collect::<Vec<_>>();
        let mut normalized = planned
            .into_profiles()
            .into_iter()
            .map(|snapshot| (snapshot.instance_id(), snapshot))
            .collect::<HashMap<_, _>>();
        let mut workspace_updates = Vec::new();
        let mut commit_candidates = HashSet::new();
        let mut request_parts = HashMap::new();
        let mut local_only = false;

        for (key, mut workspace) in clones {
            let Some(state) = self.workspace_persistence.get(&key) else {
                return Err(RetentionPlanFailure::MissingWorkspace);
            };
            let Some(planned_snapshot) = normalized.remove(&state.identity.instance_id()) else {
                return Err(RetentionPlanFailure::InvalidSnapshot);
            };
            let live_revision = self
                .model
                .workspace(&key)
                .map_or(u64::MAX, ProfileWorkspace::revision);
            let live_history = self
                .model
                .workspace(&key)
                .and_then(ProfileWorkspace::persistence)
                .map(ProfileWorkspacePersistence::history)
                .unwrap_or_default();
            let history_changed = live_history != planned_snapshot.history();
            if workspace
                .persistence()
                .is_some_and(ProfileWorkspacePersistence::persistence_enabled)
                && workspace
                    .persistence()
                    .is_some_and(|persistence| persistence.history() != planned_snapshot.history())
            {
                workspace
                    .replace_persistence_history(planned_snapshot.history().to_vec())
                    .map_err(|_| RetentionPlanFailure::InvalidSnapshot)?;
            }
            let workspace_changed = history_changed || workspace.revision() != live_revision;
            if workspace_changed {
                workspace_updates.push((key.clone(), workspace.clone()));
            }
            let persistence_enabled = workspace
                .persistence()
                .is_some_and(ProfileWorkspacePersistence::persistence_enabled);
            let live_dirty = self
                .model
                .workspace(&key)
                .is_some_and(|workspace| !workspace.is_saved());
            let accounting = EncodedProfileByteAccounting::new(&planned_snapshot)
                .map_err(RetentionPlanFailure::Retention)?;
            let upper_bound_bytes = conservative_encoded_profile_bytes(&planned_snapshot)
                .map_err(RetentionPlanFailure::Retention)?
                .1;
            let canonical_shrink = upper_bound_bytes < state.durable_committed_bytes;
            let needs_commit =
                (persistence_enabled && (workspace_changed || live_dirty)) || canonical_shrink;
            if needs_commit {
                commit_candidates.insert(key.clone());
                if state.is_read_only() {
                    local_only = true;
                }
            }
            request_parts.insert(
                key.clone(),
                (
                    state.identity.clone(),
                    workspace.revision(),
                    upper_bound_bytes,
                    accounting,
                    Box::new(planned_snapshot),
                ),
            );
        }
        if !normalized.is_empty() {
            return Err(RetentionPlanFailure::InvalidSnapshot);
        }

        let shrinking = commit_candidates
            .iter()
            .filter(|key| {
                let planned_upper_bound = request_parts
                    .get(*key)
                    .map(|(_, _, upper_bound_bytes, _, _)| *upper_bound_bytes);
                let durable_bytes = self
                    .workspace_persistence
                    .get(*key)
                    .map(|state| state.durable_committed_bytes);
                matches!((planned_upper_bound, durable_bytes), (Some(planned), Some(durable)) if planned < durable)
            })
            .cloned()
            .collect::<HashSet<_>>();
        let instance_sort_key = |key: &WorkspaceKey| {
            self.workspace_persistence
                .get(key)
                .map(|state| *state.identity.instance_id().as_bytes())
                .unwrap_or([u8::MAX; 16])
        };
        let mut commit_order = Vec::with_capacity(commit_candidates.len());
        for key in eviction_order {
            if shrinking.contains(&key) && commit_candidates.remove(&key) {
                commit_order.push(key);
            }
        }
        let mut remaining_shrinks = commit_candidates
            .iter()
            .filter(|key| shrinking.contains(*key))
            .cloned()
            .collect::<Vec<_>>();
        remaining_shrinks.sort_unstable_by_key(&instance_sort_key);
        for key in remaining_shrinks {
            if commit_candidates.remove(&key) {
                commit_order.push(key);
            }
        }
        let mut remaining = commit_candidates
            .iter()
            .filter(|key| target_key.as_ref() != Some(*key))
            .cloned()
            .collect::<Vec<_>>();
        remaining.sort_unstable_by_key(instance_sort_key);
        for key in remaining {
            if commit_candidates.remove(&key) {
                commit_order.push(key);
            }
        }
        if let Some(target_key) = target_key
            && commit_candidates.remove(&target_key)
        {
            commit_order.push(target_key);
        }
        let mut commit_requests = Vec::with_capacity(commit_order.len());
        for key in commit_order {
            let Some((identity, revision, _, accounting, snapshot)) = request_parts.remove(&key)
            else {
                return Err(RetentionPlanFailure::InvalidSnapshot);
            };
            commit_requests.push(RetentionCommitRequest {
                workspace_key: key,
                identity,
                revision,
                accounting,
                snapshot,
            });
        }
        if local_only {
            commit_requests.clear();
        }
        Ok(WorkspaceRetentionPlan {
            workspaces: workspace_updates,
            commit_requests,
            reserved_history_id,
            local_only,
        })
    }

    fn report_retention_plan_failure(&mut self, failure: RetentionPlanFailure) {
        self.model.status = match failure {
            RetentionPlanFailure::RestoreUnresolved => {
                "Persistent execution is waiting for every private workspace restore to resolve."
            }
            RetentionPlanFailure::PersistenceTransition => {
                "Persistent execution is waiting for Clear or Persistence Off to become durable."
            }
            RetentionPlanFailure::SaveInFlight | RetentionPlanFailure::BarrierActive => {
                "Persistent execution is waiting for the global retention save barrier."
            }
            RetentionPlanFailure::Retention(WorkspaceRetentionError::RetentionExhausted(_)) => {
                "Persistent execution is blocked because outcome-unknown history cannot be evicted; clear history or turn Persistence Off."
            }
            RetentionPlanFailure::MissingWorkspace
            | RetentionPlanFailure::InvalidSnapshot
            | RetentionPlanFailure::Retention(_) => {
                "Private history retention planning failed; no database command was submitted."
            }
        }
        .to_owned();
    }

    fn apply_workspace_retention_plan(&mut self, plan: WorkspaceRetentionPlan) -> Option<u64> {
        let reserved_history_id = plan.reserved_history_id;
        self.retention_reconcile_required = false;
        for (key, workspace) in plan.workspaces {
            self.model.workspaces.insert(key, workspace);
        }
        self.observe_workspace_revisions(Instant::now());
        if plan.local_only {
            self.model.status =
                "Private history is bounded locally but remains Unsaved because the store is read-only."
                    .to_owned();
            return reserved_history_id;
        }
        if !plan.commit_requests.is_empty() {
            self.merge_retention_commit_requests(plan.commit_requests);
            self.drive_retention_commit_barrier(false);
        }
        reserved_history_id
    }

    fn merge_retention_commit_requests(&mut self, requests: Vec<RetentionCommitRequest>) {
        let replacement_keys = requests
            .iter()
            .map(|request| request.workspace_key.clone())
            .collect::<HashSet<_>>();
        self.retention_commit_barrier
            .queue
            .retain(|request| !replacement_keys.contains(&request.workspace_key));
        let requests = requests
            .into_iter()
            .filter(|request| {
                !self
                    .retention_commit_barrier
                    .active
                    .as_ref()
                    .is_some_and(|active| {
                        active.workspace_key == request.workspace_key
                            && active.revision == request.revision
                    })
            })
            .collect::<Vec<_>>();
        if self
            .retention_commit_barrier
            .queue
            .len()
            .saturating_add(requests.len())
            > MAX_RETENTION_COMMIT_QUEUE
        {
            self.retention_commit_barrier.failure = Some(RetentionBarrierFailure::QueueLimit);
            self.model.status =
                "Private history retention queue reached its bound; Retry or discard local changes."
                    .to_owned();
            return;
        }
        self.retention_commit_barrier.queue.extend(requests);
    }

    fn drive_retention_commit_barrier(&mut self, retry_failed: bool) -> bool {
        if self.retention_commit_barrier.active.is_some() {
            self.model.status = "Saving the bounded private-history set…".to_owned();
            return true;
        }
        if self.retention_commit_barrier.failure.is_some() {
            if !retry_failed {
                return false;
            }
            self.retention_commit_barrier.failure = None;
        }
        let Some(request) = self.retention_commit_barrier.queue.front() else {
            return true;
        };
        let Some(state) = self.workspace_persistence.get(&request.workspace_key) else {
            self.retention_commit_barrier.failure = Some(RetentionBarrierFailure::IdentityChanged);
            self.model.status =
                "Private history save identity changed; reload before retrying.".to_owned();
            return false;
        };
        if state.identity != request.identity
            || !matches!(state.load, WorkspaceLoadPhase::Ready)
            || state.mode != Some(WorkspaceStoreMode::ReadWrite)
            || state.clear.has_intent()
            || state.force_commit_until_success
            || matches!(state.save, WorkspaceSavePhase::Saving { .. })
        {
            self.retention_commit_barrier.failure = Some(RetentionBarrierFailure::IdentityChanged);
            self.model.status =
                "Private history save barrier needs workspace recovery before Retry.".to_owned();
            return false;
        }
        let operation_id = self.model.next_operation();
        let command = UiCommand::CommitWorkspace {
            operation_id,
            identity: request.identity.clone(),
            revision: request.revision,
            snapshot: request.snapshot.clone(),
        };
        match self.port.try_submit(command) {
            Ok(()) => {
                let Some(request) = self.retention_commit_barrier.queue.pop_front() else {
                    self.retention_commit_barrier.failure = Some(RetentionBarrierFailure::Planning);
                    self.model.status =
                        "Private history save queue changed unexpectedly; Retry is required."
                            .to_owned();
                    return false;
                };
                if let Some(state) = self.workspace_persistence.get_mut(&request.workspace_key) {
                    state.save = WorkspaceSavePhase::Saving {
                        operation_id,
                        revision: request.revision,
                    };
                    state.submitted_commit = Some(SubmittedWorkspaceCommit {
                        operation_id,
                        revision: request.revision,
                        accounting: request.accounting,
                    });
                    state.dirty_since = None;
                    state.retry_not_before = None;
                }
                self.retention_commit_barrier.active = Some(ActiveRetentionCommit {
                    workspace_key: request.workspace_key,
                    operation_id,
                    revision: request.revision,
                });
                self.model.status = "Saving the bounded private-history set…".to_owned();
                true
            }
            Err(error) => {
                self.retention_commit_barrier.failure =
                    Some(RetentionBarrierFailure::Submit(error));
                if let Some(state) = self.workspace_persistence.get_mut(&request.workspace_key) {
                    let code = match error {
                        SubmitError::Busy => WorkspaceFailureCode::Busy,
                        SubmitError::Disconnected => WorkspaceFailureCode::Unavailable,
                    };
                    state.save = WorkspaceSavePhase::Failed {
                        revision: request.revision,
                        code,
                    };
                    state.submitted_commit = None;
                    state.dirty_since.get_or_insert(Instant::now());
                    state.retry_not_before = Some(Instant::now() + WORKSPACE_SAVE_RETRY_DELAY);
                }
                self.model.status =
                    "Private history save barrier is unavailable; Retry keeps every local change."
                        .to_owned();
                false
            }
        }
    }

    fn retry_retention_commit_barrier(&mut self) -> bool {
        if self.retention_commit_barrier.active.is_some() {
            return self.drive_retention_commit_barrier(false);
        }
        if matches!(
            self.retention_commit_barrier.failure,
            Some(
                RetentionBarrierFailure::Operation(_)
                    | RetentionBarrierFailure::IdentityChanged
                    | RetentionBarrierFailure::QueueLimit
                    | RetentionBarrierFailure::Planning
            )
        ) {
            self.retention_commit_barrier.queue.clear();
            self.retention_reconcile_required = true;
        }
        if self.retention_commit_barrier.queue.is_empty() {
            self.retention_commit_barrier.failure = None;
            self.retention_reconcile_required = true;
            if !self.reconcile_workspace_retention(true) {
                return false;
            }
        }
        self.drive_retention_commit_barrier(true)
    }

    fn reconcile_workspace_retention(&mut self, force: bool) -> bool {
        if !force && !self.retention_reconcile_required {
            return true;
        }
        let keys = self.ensure_current_workspace_persistence_bindings();
        if keys.is_empty() {
            self.retention_reconcile_required = false;
            return true;
        }
        if keys.iter().any(|key| {
            self.workspace_persistence.get(key).is_none_or(|state| {
                !matches!(state.load, WorkspaceLoadPhase::Ready)
                    || state.mode.is_none()
                    || state.clear.has_intent()
                    || state.force_commit_until_success
            })
        }) {
            return false;
        }
        let plan = match self.build_workspace_retention_plan(&keys, None) {
            Ok(plan) => plan,
            Err(failure) => {
                self.retention_commit_barrier.failure = Some(RetentionBarrierFailure::Planning);
                self.report_retention_plan_failure(failure);
                return false;
            }
        };
        self.retention_reconcile_required = false;
        let local_only = plan.local_only;
        let has_commits = !plan.commit_requests.is_empty();
        let _ = self.apply_workspace_retention_plan(plan);
        if !local_only && has_commits {
            self.model.status = "Normalizing the bounded private-history set…".to_owned();
        }
        true
    }

    fn retention_barrier_references_current_identities(&self) -> bool {
        let active_is_current =
            self.retention_commit_barrier
                .active
                .as_ref()
                .is_none_or(|active| {
                    self.workspace_persistence
                        .get(&active.workspace_key)
                        .is_some_and(|state| {
                            matches!(state.load, WorkspaceLoadPhase::Ready)
                                && matches!(
                                    state.save,
                                    WorkspaceSavePhase::Saving {
                                        operation_id,
                                        revision,
                                    } if operation_id == active.operation_id
                                        && revision == active.revision
                                )
                        })
                });
        active_is_current
            && self.retention_commit_barrier.queue.iter().all(|request| {
                self.workspace_persistence
                    .get(&request.workspace_key)
                    .is_some_and(|state| state.identity == request.identity)
            })
    }

    fn request_workspace_load(&mut self, key: &WorkspaceKey) {
        let should_load = self.workspace_persistence.get(key).is_some_and(|state| {
            matches!(state.load, WorkspaceLoadPhase::Unloaded) && !state.clear.has_intent()
        });
        if !should_load {
            return;
        }
        let Some((identity, restore_baseline_revision)) = self
            .workspace_persistence
            .get(key)
            .map(|state| (state.identity.clone(), state.restore_baseline_revision))
        else {
            return;
        };
        let Some(base_revision) = self.model.workspace(key).map(ProfileWorkspace::revision) else {
            return;
        };
        let restore_allowed = restore_baseline_revision == Some(base_revision);
        let operation_id = self.model.next_operation();
        match self.port.try_submit(UiCommand::LoadWorkspace {
            operation_id,
            identity,
            base_revision,
        }) {
            Ok(()) => {
                if let Some(state) = self.workspace_persistence.get_mut(key) {
                    state.load = WorkspaceLoadPhase::Loading {
                        operation_id,
                        base_revision,
                        restore_allowed,
                    };
                    state.clean_empty_baseline_pending = None;
                }
                self.model.status = "Loading private workspace…".to_owned();
            }
            Err(error) => {
                if let Some(state) = self.workspace_persistence.get_mut(key) {
                    let code = match error {
                        SubmitError::Busy => WorkspaceFailureCode::Busy,
                        SubmitError::Disconnected => WorkspaceFailureCode::Unavailable,
                    };
                    state.load = WorkspaceLoadPhase::Failed(code);
                    state.retry_not_before = Some(Instant::now() + WORKSPACE_SAVE_RETRY_DELAY);
                }
                self.report_submit_error(error);
            }
        }
    }

    fn observe_workspace_revisions(&mut self, now: Instant) {
        let mut became_dirty = false;
        let keys = self
            .workspace_persistence
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        for key in keys {
            let Some((revision, saved)) = self
                .model
                .workspace(&key)
                .map(|workspace| (workspace.revision(), workspace.is_saved()))
            else {
                continue;
            };
            let Some(state) = self.workspace_persistence.get_mut(&key) else {
                continue;
            };
            if revision != state.observed_revision {
                state.observed_revision = revision;
                state.clean_empty_baseline_pending = None;
                if !saved {
                    state.dirty_since = Some(now);
                    became_dirty = true;
                }
            }
            if saved {
                state.dirty_since = None;
                state.retry_not_before = None;
                if !matches!(state.save, WorkspaceSavePhase::Saving { .. }) {
                    state.save = WorkspaceSavePhase::Idle;
                }
            }
        }
        if became_dirty && operation_status_claims_workspace_saved(&self.model.status) {
            self.model.status = "Local workspace changes are Unsaved.".to_owned();
        }
    }

    fn submit_workspace_commit(&mut self, key: &WorkspaceKey, explicit: bool) -> bool {
        let force = self
            .workspace_persistence
            .get(key)
            .is_some_and(|state| state.force_commit_until_success);
        self.submit_workspace_commit_inner(key, explicit, force)
    }

    fn submit_workspace_commit_inner(
        &mut self,
        key: &WorkspaceKey,
        explicit: bool,
        force: bool,
    ) -> bool {
        if self.retention_commit_barrier.is_pending_or_failed() {
            return if explicit {
                self.retry_retention_commit_barrier()
            } else {
                self.drive_retention_commit_barrier(false)
            };
        }
        let Some(state) = self.workspace_persistence.get(key) else {
            return false;
        };
        if state.is_read_only() || state.clear.has_intent() {
            self.model.status = if state.is_read_only() {
                "Workspace persistence is read-only; changes remain local.".to_owned()
            } else {
                "Clearing private workspace data…".to_owned()
            };
            return false;
        }
        if matches!(state.save, WorkspaceSavePhase::Saving { .. }) {
            self.model.status = "Saving private workspace…".to_owned();
            return false;
        }
        if matches!(state.load, WorkspaceLoadPhase::Loading { .. }) {
            self.model.status =
                "Workspace restore is still loading; save remains pending.".to_owned();
            return false;
        }
        if !force && !matches!(state.load, WorkspaceLoadPhase::Ready) {
            self.model.status =
                "Workspace restore is unresolved; Retry restore before saving.".to_owned();
            return false;
        }
        if !explicit && state.mode != Some(WorkspaceStoreMode::ReadWrite) {
            return false;
        }
        let persistence_enabled = self
            .model
            .workspace(key)
            .and_then(ProfileWorkspace::persistence)
            .is_some_and(ProfileWorkspacePersistence::persistence_enabled);
        if !persistence_enabled && !force {
            self.model.status =
                "Persistence Off — editor changes are local only until quit.".to_owned();
            return false;
        }
        let identity = state.identity.clone();
        let snapshot = {
            let Some(workspace) = self.model.workspaces.get_mut(key) else {
                return false;
            };
            match workspace.to_persistence_snapshot() {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    self.model.status = error.to_string();
                    return false;
                }
            }
        };
        let Some(revision) = self.model.workspace(key).map(ProfileWorkspace::revision) else {
            return false;
        };
        let accounting = match EncodedProfileByteAccounting::new(&snapshot) {
            Ok(accounting) => accounting,
            Err(error) => {
                self.model.status = error.to_string();
                return false;
            }
        };
        if !force
            && self
                .model
                .workspace(key)
                .is_some_and(ProfileWorkspace::is_saved)
        {
            self.model.status = "Private workspace is Saved.".to_owned();
            return true;
        }
        let operation_id = self.model.next_operation();
        match self.port.try_submit(UiCommand::CommitWorkspace {
            operation_id,
            identity,
            revision,
            snapshot: Box::new(snapshot),
        }) {
            Ok(()) => {
                if let Some(state) = self.workspace_persistence.get_mut(key) {
                    state.save = WorkspaceSavePhase::Saving {
                        operation_id,
                        revision,
                    };
                    state.submitted_commit = Some(SubmittedWorkspaceCommit {
                        operation_id,
                        revision,
                        accounting,
                    });
                    state.dirty_since = None;
                    state.retry_not_before = None;
                }
                self.model.status = "Saving private workspace…".to_owned();
                true
            }
            Err(error) => {
                if let Some(state) = self.workspace_persistence.get_mut(key) {
                    let code = match error {
                        SubmitError::Busy => WorkspaceFailureCode::Busy,
                        SubmitError::Disconnected => WorkspaceFailureCode::Unavailable,
                    };
                    state.save = WorkspaceSavePhase::Failed { revision, code };
                    state.submitted_commit = None;
                    state.dirty_since.get_or_insert(Instant::now());
                    state.retry_not_before = Some(Instant::now() + WORKSPACE_SAVE_RETRY_DELAY);
                }
                self.report_submit_error(error);
                false
            }
        }
    }

    fn submit_conflict_resolution_commit(&mut self, key: &WorkspaceKey) -> bool {
        let is_conflict = self
            .workspace_persistence
            .get(key)
            .is_some_and(|state| matches!(state.load, WorkspaceLoadPhase::Conflict));
        if !is_conflict {
            return false;
        }
        if let Some(state) = self.workspace_persistence.get_mut(key) {
            state.load = WorkspaceLoadPhase::Ready;
        }
        let submitted = self.submit_workspace_commit_inner(key, true, false);
        let armed = self
            .workspace_persistence
            .get(key)
            .is_some_and(|state| matches!(state.save, WorkspaceSavePhase::Saving { .. }));
        if let Some(state) = self.workspace_persistence.get_mut(key) {
            state.load = WorkspaceLoadPhase::Conflict;
            state.resolve_conflict_on_commit = submitted && armed;
        }
        if submitted && armed {
            self.model.status =
                "Replacing the prior saved workspace with the confirmed local workspace…"
                    .to_owned();
            true
        } else {
            false
        }
    }

    fn request_selected_workspace_save(&mut self) -> bool {
        let Some(key) = self.ensure_selected_workspace_persistence_binding() else {
            self.model.status =
                "Workspace persistence requires a classified version 3 profile.".to_owned();
            return false;
        };
        if self
            .workspace_persistence
            .get(&key)
            .is_some_and(|state| matches!(state.load, WorkspaceLoadPhase::Conflict))
        {
            self.workspace_restore_conflict_confirmation = Some(key);
            return false;
        }
        self.observe_workspace_revisions(Instant::now());
        self.submit_workspace_commit(&key, true)
    }

    fn flush_selected_workspace(&mut self) -> bool {
        let Some(key) = self.ensure_selected_workspace_persistence_binding() else {
            self.model.status =
                "Workspace persistence requires a classified version 3 profile.".to_owned();
            return false;
        };
        self.observe_workspace_revisions(Instant::now());
        self.submit_workspace_commit(&key, true)
    }

    fn flush_all_dirty_workspaces(&mut self) {
        let keys = self.ensure_current_workspace_persistence_bindings();
        self.observe_workspace_revisions(Instant::now());
        if self.retention_commit_barrier.is_pending_or_failed() {
            let _ = self.retry_retention_commit_barrier();
            return;
        }
        for key in keys {
            let dirty = self
                .model
                .workspace(&key)
                .is_some_and(|workspace| !workspace.is_saved());
            if dirty {
                let _ = self.submit_workspace_commit(&key, true);
            }
        }
    }

    fn autosave_workspaces(&mut self, now: Instant) {
        if self.retention_commit_barrier.is_pending_or_failed() {
            let retry_ready = self
                .retention_commit_barrier
                .queue
                .front()
                .and_then(|request| self.workspace_persistence.get(&request.workspace_key))
                .is_some_and(|state| state.retry_not_before.is_none_or(|retry| now >= retry));
            if self.retention_commit_barrier.failure.is_none() {
                let _ = self.drive_retention_commit_barrier(false);
            } else if retry_ready {
                let _ = self.retry_retention_commit_barrier();
            }
            return;
        }
        let keys = self
            .workspace_persistence
            .iter()
            .filter_map(|(key, state)| {
                let current_revision = self.model.workspace(key).map(ProfileWorkspace::revision);
                let persistence_enabled = self
                    .model
                    .workspace(key)
                    .and_then(ProfileWorkspace::persistence)
                    .is_some_and(ProfileWorkspacePersistence::persistence_enabled);
                let debounce_elapsed = state.dirty_since.is_some_and(|changed| {
                    now.duration_since(changed) >= WORKSPACE_AUTOSAVE_DEBOUNCE
                });
                let retry_ready = state.retry_not_before.is_none_or(|retry| now >= retry);
                let ready = matches!(state.load, WorkspaceLoadPhase::Ready)
                    || state.force_commit_until_success;
                let permanent_failure_for_current_revision = matches!(
                    state.save,
                    WorkspaceSavePhase::Failed { revision, code }
                        if Some(revision) == current_revision
                            && !workspace_failure_is_transient(code)
                );
                (debounce_elapsed
                    && retry_ready
                    && ready
                    && state.mode == Some(WorkspaceStoreMode::ReadWrite)
                    && (persistence_enabled || state.force_commit_until_success)
                    && !matches!(state.save, WorkspaceSavePhase::Saving { .. })
                    && !permanent_failure_for_current_revision
                    && !state.clear.has_intent())
                .then(|| key.clone())
            })
            .collect::<Vec<_>>();
        for key in keys {
            let _ = self.submit_workspace_commit(&key, false);
        }
    }

    fn retry_workspace_loads(&mut self, now: Instant) {
        let retry_keys = self
            .workspace_persistence
            .iter()
            .filter_map(|(key, state)| {
                let transient = matches!(
                    state.load,
                    WorkspaceLoadPhase::Failed(
                        WorkspaceFailureCode::Busy
                            | WorkspaceFailureCode::Stale
                            | WorkspaceFailureCode::Unavailable
                    )
                );
                (transient
                    && !state.clear.has_intent()
                    && state.retry_not_before.is_none_or(|retry| now >= retry))
                .then(|| key.clone())
            })
            .collect::<Vec<_>>();
        for key in retry_keys {
            if let Some(state) = self.workspace_persistence.get_mut(&key) {
                state.load = WorkspaceLoadPhase::Unloaded;
                state.retry_not_before = None;
            }
            self.request_workspace_load(&key);
        }
    }

    fn handle_workspace_event(&mut self, event: &UiEvent) {
        match event {
            UiEvent::WorkspaceLoaded {
                operation_id,
                identity,
                base_revision,
                mode,
                read_only_reason,
                generation,
                committed_bytes,
                snapshot,
            } => {
                let key =
                    WorkspaceKey::new(identity.profile_id().clone(), identity.profile_generation());
                let restore_context = self.workspace_persistence.get(&key).and_then(|state| {
                    if state.identity != *identity {
                        return None;
                    }
                    match state.load {
                        WorkspaceLoadPhase::Loading {
                            operation_id: pending,
                            base_revision: pending_revision,
                            restore_allowed,
                        } if pending == *operation_id && pending_revision == *base_revision => {
                            Some((restore_allowed, state.refresh_durable_baseline_only))
                        }
                        _ => None,
                    }
                });
                let Some((restore_allowed, refresh_durable_baseline_only)) = restore_context else {
                    return;
                };
                let current_revision = self
                    .model
                    .workspace(&key)
                    .map_or(u64::MAX, ProfileWorkspace::revision);
                if let Some(state) = self.workspace_persistence.get_mut(&key) {
                    state.mode = Some(*mode);
                    state.read_only_reason = *read_only_reason;
                    state.load = WorkspaceLoadPhase::Ready;
                    state.retry_not_before = None;
                    state.clean_empty_baseline_pending = None;
                }
                if let Some(snapshot) = snapshot.as_deref()
                    && (snapshot.profile_id() != identity.profile_id()
                        || snapshot.instance_id() != identity.instance_id())
                {
                    if let Some(state) = self.workspace_persistence.get_mut(&key) {
                        state.load =
                            WorkspaceLoadPhase::Failed(WorkspaceFailureCode::InvalidIdentity);
                    }
                    self.model.status =
                        "The restored workspace identity did not match this profile.".to_owned();
                    return;
                }
                let durable_committed_bytes =
                    match (snapshot.as_deref(), *generation, *committed_bytes) {
                        (Some(_), Some(generation), committed_bytes)
                            if generation > 0 && committed_bytes > 0 =>
                        {
                            committed_bytes
                        }
                        (None, None, 0) => 0,
                        _ => {
                            if let Some(state) = self.workspace_persistence.get_mut(&key) {
                                state.load = WorkspaceLoadPhase::Failed(
                                    WorkspaceFailureCode::InvalidSnapshot,
                                );
                            }
                            self.model.status =
                                "The restored workspace byte metadata did not match its snapshot."
                                    .to_owned();
                            return;
                        }
                    };
                if let Some(state) = self.workspace_persistence.get_mut(&key) {
                    state.durable_generation = *generation;
                    state.durable_committed_bytes = durable_committed_bytes;
                    state.submitted_commit = None;
                }
                if refresh_durable_baseline_only {
                    if let Some(state) = self.workspace_persistence.get_mut(&key) {
                        state.refresh_durable_baseline_only = false;
                        state.observed_revision = current_revision;
                        state.save = WorkspaceSavePhase::Idle;
                        state.dirty_since.get_or_insert(Instant::now());
                        state.retry_not_before = None;
                        state.clean_empty_baseline_pending = None;
                    }
                    self.model.status = if *mode == WorkspaceStoreMode::ReadOnly {
                        "Workspace bytes rechecked read-only; local changes remain Unsaved."
                            .to_owned()
                    } else {
                        "Workspace bytes rechecked after profile refresh; local changes remain Unsaved."
                            .to_owned()
                    };
                    return;
                }
                let restored_opt_out = snapshot
                    .as_deref()
                    .is_some_and(|snapshot| !snapshot.persistence_enabled());
                if restored_opt_out && (current_revision != *base_revision || !restore_allowed) {
                    let resolved_revision =
                        if let Some(workspace) = self.model.workspaces.get_mut(&key) {
                            if let Err(error) = workspace.set_persistence_enabled(false) {
                                self.model.status = error.to_string();
                                return;
                            }
                            let revision = workspace.revision();
                            let _ = workspace.mark_saved_if_revision(revision);
                            revision
                        } else {
                            current_revision
                        };
                    if let Some(state) = self.workspace_persistence.get_mut(&key) {
                        state.load = WorkspaceLoadPhase::Ready;
                        state.observed_revision = resolved_revision;
                        state.dirty_since = None;
                        state.retry_not_before = None;
                        state.restore_baseline_revision = Some(resolved_revision);
                    }
                    self.model.status =
                        "Persistence Off restored; the local draft remains local-only.".to_owned();
                    return;
                }
                if snapshot.is_some() && (current_revision != *base_revision || !restore_allowed) {
                    if let Some(state) = self.workspace_persistence.get_mut(&key) {
                        state.load = WorkspaceLoadPhase::Conflict;
                        state.dirty_since.get_or_insert(Instant::now());
                        state.retry_not_before = None;
                    }
                    self.model.status =
                        "Restore conflict: choose Keep local, Persistence Off, or Clear saved data."
                            .to_owned();
                    return;
                }
                if let Some(snapshot) = snapshot.as_deref() {
                    let geometry = snapshot.geometry();
                    match ProfileWorkspace::from_persistence_snapshot(snapshot.clone()) {
                        Ok(restored) => {
                            let restored_revision = restored.revision();
                            let restored_empty = restored.editor_tabs().is_empty();
                            self.model.workspaces.insert(key.clone(), restored);
                            let restored_geometry = WorkspaceGeometry::restore(
                                geometry.navigator_width(),
                                geometry.editor_share(),
                                geometry.inspector_visible(),
                            );
                            self.workspace_geometries
                                .insert(key.clone(), restored_geometry);
                            self.sync_collapsed_workspace_pane(&key, restored_geometry);
                            if let Some(state) = self.workspace_persistence.get_mut(&key) {
                                state.observed_revision = restored_revision;
                                state.dirty_since = None;
                                state.save = WorkspaceSavePhase::Idle;
                                state.clean_empty_baseline_pending =
                                    restored_empty.then_some(restored_revision);
                                state.restore_baseline_revision = Some(restored_revision);
                            }
                            if self.model.selected_workspace_key().as_ref() == Some(&key) {
                                self.compact_workspace = None;
                            }
                            self.editor_surface = EditorSurface::default();
                            self.model.status = if *mode == WorkspaceStoreMode::ReadOnly {
                                "Private workspace restored read-only; another app owns the writer."
                                    .to_owned()
                            } else {
                                "Private workspace restored.".to_owned()
                            };
                        }
                        Err(error) => {
                            if let Some(state) = self.workspace_persistence.get_mut(&key) {
                                state.load = WorkspaceLoadPhase::Failed(
                                    WorkspaceFailureCode::InvalidSnapshot,
                                );
                            }
                            self.model.status = error.to_string();
                        }
                    }
                } else {
                    let clean_empty_baseline =
                        self.model
                            .workspaces
                            .get_mut(&key)
                            .is_some_and(|workspace| {
                                workspace.editor_tabs().is_empty()
                                    && workspace.mark_saved_if_revision(*base_revision)
                            });
                    let current_revision = self
                        .model
                        .workspace(&key)
                        .map_or(*base_revision, ProfileWorkspace::revision);
                    if let Some(state) = self.workspace_persistence.get_mut(&key) {
                        state.observed_revision = current_revision;
                        state.dirty_since = (!clean_empty_baseline).then_some(Instant::now());
                        state.save = WorkspaceSavePhase::Idle;
                        state.clean_empty_baseline_pending =
                            clean_empty_baseline.then_some(*base_revision);
                        state.restore_baseline_revision =
                            clean_empty_baseline.then_some(*base_revision);
                    }
                    self.model.status = if *mode == WorkspaceStoreMode::ReadOnly {
                        "Workspace persistence is read-only; another app owns the writer."
                            .to_owned()
                    } else {
                        "Private workspace is ready.".to_owned()
                    };
                }
            }
            UiEvent::WorkspaceCommitted {
                operation_id,
                identity,
                revision,
                generation,
                committed_bytes,
                warnings,
            } => {
                let key =
                    WorkspaceKey::new(identity.profile_id().clone(), identity.profile_generation());
                let barrier_matches =
                    self.retention_commit_barrier
                        .active
                        .as_ref()
                        .is_some_and(|active| {
                            active.workspace_key == key
                                && active.operation_id == *operation_id
                                && active.revision == *revision
                        });
                let submitted = self.workspace_persistence.get(&key).and_then(|state| {
                    let submitted = state.submitted_commit?;
                    (state.identity == *identity
                        && submitted.operation_id == *operation_id
                        && submitted.revision == *revision
                        && matches!(
                            state.save,
                            WorkspaceSavePhase::Saving {
                                operation_id: pending,
                                revision: pending_revision,
                            } if pending == *operation_id && pending_revision == *revision
                        ))
                    .then_some(submitted)
                });
                let Some(submitted) = submitted else {
                    return;
                };
                let calculated_committed_bytes = submitted
                    .accounting
                    .encoded_bytes_at_generation(*generation)
                    .ok()
                    .map(|(_, bytes)| bytes);
                if *generation == 0
                    || *committed_bytes == 0
                    || calculated_committed_bytes != Some(*committed_bytes)
                {
                    if let Some(state) = self.workspace_persistence.get_mut(&key) {
                        state.load = WorkspaceLoadPhase::Unloaded;
                        state.mode = None;
                        state.read_only_reason = None;
                        state.save = WorkspaceSavePhase::Idle;
                        state.submitted_commit = None;
                        state.refresh_durable_baseline_only = true;
                        state.dirty_since.get_or_insert(Instant::now());
                    }
                    if barrier_matches {
                        self.retention_commit_barrier.active = None;
                        self.retention_commit_barrier.failure =
                            Some(RetentionBarrierFailure::Planning);
                        self.retention_reconcile_required = true;
                    }
                    self.request_workspace_load(&key);
                    self.model.status =
                        "Workspace byte accounting needs an exact reload; local work remains Unsaved."
                            .to_owned();
                    return;
                }
                let saved = self
                    .model
                    .workspaces
                    .get_mut(&key)
                    .is_some_and(|workspace| workspace.mark_saved_if_revision(*revision));
                let current_revision = self
                    .model
                    .workspace(&key)
                    .map_or(*revision, ProfileWorkspace::revision);
                if let Some(state) = self.workspace_persistence.get_mut(&key) {
                    state.submitted_commit = None;
                    state.durable_generation = Some(*generation);
                    state.durable_committed_bytes = *committed_bytes;
                    if state.resolve_conflict_on_commit {
                        state.load = WorkspaceLoadPhase::Ready;
                    }
                    state.mode = Some(WorkspaceStoreMode::ReadWrite);
                    state.save = WorkspaceSavePhase::Idle;
                    state.observed_revision = current_revision;
                    state.retry_not_before = None;
                    state.force_commit_until_success = false;
                    state.resolve_conflict_on_commit = false;
                    state.restore_baseline_revision = Some(*revision);
                    state.refresh_durable_baseline_only = false;
                    if saved {
                        state.dirty_since = None;
                    } else {
                        state.dirty_since.get_or_insert(Instant::now());
                    }
                }
                self.model.status = if saved {
                    if warnings.is_empty() {
                        "Private workspace Saved.".to_owned()
                    } else {
                        "Private workspace Saved with a bounded recovery warning.".to_owned()
                    }
                } else {
                    "A newer local revision remains Unsaved; another save is queued.".to_owned()
                };
                if barrier_matches {
                    self.retention_commit_barrier.active = None;
                    self.retention_commit_barrier.failure = None;
                    if !saved {
                        self.retention_reconcile_required = true;
                    }
                    if self.retention_commit_barrier.queue.is_empty()
                        && self.retention_reconcile_required
                    {
                        let _ = self.reconcile_workspace_retention(true);
                    } else {
                        let _ = self.drive_retention_commit_barrier(false);
                    }
                }
            }
            UiEvent::WorkspaceCommitSuperseded {
                operation_id,
                identity,
                revision,
                ..
            } => {
                let key =
                    WorkspaceKey::new(identity.profile_id().clone(), identity.profile_generation());
                let barrier_matches =
                    self.retention_commit_barrier
                        .active
                        .as_ref()
                        .is_some_and(|active| {
                            active.workspace_key == key
                                && active.operation_id == *operation_id
                                && active.revision == *revision
                        });
                let matches_pending = self.workspace_persistence.get(&key).is_some_and(|state| {
                    state.identity == *identity
                        && state.submitted_commit.is_some_and(|submitted| {
                            submitted.operation_id == *operation_id
                                && submitted.revision == *revision
                        })
                        && matches!(
                            state.save,
                            WorkspaceSavePhase::Saving {
                                operation_id: pending,
                                revision: pending_revision,
                            } if pending == *operation_id && pending_revision == *revision
                        )
                });
                if matches_pending && let Some(state) = self.workspace_persistence.get_mut(&key) {
                    if state.submitted_commit.is_some_and(|submitted| {
                        submitted.operation_id == *operation_id && submitted.revision == *revision
                    }) {
                        state.submitted_commit = None;
                    }
                    state.save = WorkspaceSavePhase::Failed {
                        revision: *revision,
                        code: WorkspaceFailureCode::Stale,
                    };
                    state.dirty_since.get_or_insert(Instant::now());
                    state.retry_not_before = Some(Instant::now() + WORKSPACE_SAVE_RETRY_DELAY);
                    state.resolve_conflict_on_commit = false;
                    self.model.status =
                        "A newer save superseded this revision; local work remains Unsaved."
                            .to_owned();
                }
                if matches_pending && barrier_matches {
                    self.retention_commit_barrier.active = None;
                    self.retention_commit_barrier.failure = Some(
                        RetentionBarrierFailure::Operation(WorkspaceFailureCode::Stale),
                    );
                    self.retention_reconcile_required = true;
                    self.model.status =
                        "Private history save was superseded; Retry preserves the bounded set."
                            .to_owned();
                }
            }
            UiEvent::WorkspaceCleared {
                operation_id,
                identity,
                base_revision,
            } => {
                let key =
                    WorkspaceKey::new(identity.profile_id().clone(), identity.profile_generation());
                let matches_pending = self.workspace_persistence.get(&key).is_some_and(|state| {
                    state.identity == *identity
                        && matches!(
                            state.clear,
                            WorkspaceClearPhase::Pending {
                                operation_id: pending,
                                revision,
                            } if pending == *operation_id && revision == *base_revision
                        )
                });
                if !matches_pending {
                    return;
                }
                let current_revision = if let Some(workspace) = self.model.workspaces.get_mut(&key)
                {
                    if workspace.set_persistence_enabled(false).is_err() {
                        self.model.status =
                            "Saved data was cleared, but local persistence state needs attention."
                                .to_owned();
                        return;
                    }
                    workspace.revision()
                } else {
                    *base_revision
                };
                if let Some(state) = self.workspace_persistence.get_mut(&key) {
                    state.clear = WorkspaceClearPhase::Idle;
                    state.load = WorkspaceLoadPhase::Ready;
                    state.save = WorkspaceSavePhase::Idle;
                    state.dirty_since = Some(Instant::now());
                    state.retry_not_before = None;
                    state.observed_revision = current_revision;
                    state.force_commit_until_success = true;
                    state.resolve_conflict_on_commit = false;
                    state.clean_empty_baseline_pending = None;
                    state.restore_baseline_revision = Some(current_revision);
                    state.durable_generation = None;
                    state.durable_committed_bytes = 0;
                    state.submitted_commit = None;
                    state.refresh_durable_baseline_only = false;
                }
                self.model.status =
                    "Saved data cleared; recording durable Persistence Off…".to_owned();
                let _ = self.submit_workspace_commit_inner(&key, true, true);
            }
            UiEvent::WorkspaceOperationFailed {
                operation_id,
                identity,
                revision,
                action,
                code,
            } => {
                let key =
                    WorkspaceKey::new(identity.profile_id().clone(), identity.profile_generation());
                let barrier_matches =
                    self.retention_commit_barrier
                        .active
                        .as_ref()
                        .is_some_and(|active| {
                            active.workspace_key == key
                                && active.operation_id == *operation_id
                                && active.revision == *revision
                        });
                let Some(state) = self.workspace_persistence.get_mut(&key) else {
                    return;
                };
                if state.identity != *identity {
                    return;
                }
                match action {
                    WorkspaceAction::Load => {
                        let matches = matches!(
                            state.load,
                            WorkspaceLoadPhase::Loading {
                                operation_id: pending,
                                base_revision,
                                ..
                            } if pending == *operation_id && base_revision == *revision
                        );
                        if !matches {
                            return;
                        }
                        if let WorkspaceFailureCode::ReadOnly(reason) = code {
                            state.mode = Some(WorkspaceStoreMode::ReadOnly);
                            state.read_only_reason = Some(*reason);
                        }
                        state.load = WorkspaceLoadPhase::Failed(*code);
                        state.clean_empty_baseline_pending = None;
                        if matches!(
                            code,
                            WorkspaceFailureCode::Busy
                                | WorkspaceFailureCode::Stale
                                | WorkspaceFailureCode::Unavailable
                        ) {
                            state.retry_not_before =
                                Some(Instant::now() + WORKSPACE_SAVE_RETRY_DELAY);
                        }
                        self.model.status = "Private workspace restore failed.".to_owned();
                    }
                    WorkspaceAction::Commit => {
                        let matches = matches!(
                            state.save,
                            WorkspaceSavePhase::Saving {
                                operation_id: pending,
                                revision: pending_revision,
                            } if pending == *operation_id && pending_revision == *revision
                        );
                        if !matches {
                            return;
                        }
                        if !state.submitted_commit.is_some_and(|submitted| {
                            submitted.operation_id == *operation_id
                                && submitted.revision == *revision
                        }) {
                            return;
                        }
                        if state.submitted_commit.is_some_and(|submitted| {
                            submitted.operation_id == *operation_id
                                && submitted.revision == *revision
                        }) {
                            state.submitted_commit = None;
                        }
                        if let WorkspaceFailureCode::ReadOnly(reason) = code {
                            state.mode = Some(WorkspaceStoreMode::ReadOnly);
                            state.read_only_reason = Some(*reason);
                        }
                        state.save = WorkspaceSavePhase::Failed {
                            revision: *revision,
                            code: *code,
                        };
                        state.resolve_conflict_on_commit = false;
                        state.dirty_since.get_or_insert(Instant::now());
                        if matches!(
                            code,
                            WorkspaceFailureCode::Busy
                                | WorkspaceFailureCode::Stale
                                | WorkspaceFailureCode::Unavailable
                        ) {
                            state.retry_not_before =
                                Some(Instant::now() + WORKSPACE_SAVE_RETRY_DELAY);
                        }
                        self.model.status =
                            "Private workspace Save failed; local changes remain Unsaved."
                                .to_owned();
                        if barrier_matches {
                            self.retention_commit_barrier.active = None;
                            self.retention_commit_barrier.failure =
                                Some(RetentionBarrierFailure::Operation(*code));
                            self.retention_reconcile_required = true;
                            self.model.status =
                                "Private history save failed; Retry keeps the bounded set Unsaved."
                                    .to_owned();
                        }
                    }
                    WorkspaceAction::Clear => {
                        if !matches!(
                            state.clear,
                            WorkspaceClearPhase::Pending {
                                operation_id: pending,
                                revision: pending_revision,
                            } if pending == *operation_id && pending_revision == *revision
                        ) {
                            return;
                        }
                        if let WorkspaceFailureCode::ReadOnly(reason) = code {
                            state.mode = Some(WorkspaceStoreMode::ReadOnly);
                            state.read_only_reason = Some(*reason);
                        }
                        state.clear = WorkspaceClearPhase::Failed {
                            revision: *revision,
                            code: *code,
                        };
                        state.save = WorkspaceSavePhase::Idle;
                        self.model.status =
                            "Clearing private workspace failed; Retry keeps the clear intent."
                                .to_owned();
                    }
                }
            }
            UiEvent::ConfigUncertain { .. } | UiEvent::RuntimeShutdown { .. } => {
                self.pending_workspace_history.clear();
                if self.retention_commit_barrier.is_active() {
                    self.retention_commit_barrier.active = None;
                    self.retention_commit_barrier.failure =
                        Some(RetentionBarrierFailure::IdentityChanged);
                    self.retention_reconcile_required = true;
                }
            }
            _ => {}
        }
    }

    fn has_uncommitted_workspace(&self) -> bool {
        self.retention_commit_barrier.is_pending_or_failed()
            || self.workspace_persistence.iter().any(|(key, state)| {
                let persistence_enabled = self
                    .model
                    .workspace(key)
                    .and_then(ProfileWorkspace::persistence)
                    .is_some_and(ProfileWorkspacePersistence::persistence_enabled);
                let durable_dirty = persistence_enabled
                    && self
                        .model
                        .workspace(key)
                        .is_some_and(|workspace| !workspace.is_saved());
                durable_dirty
                    || matches!(state.save, WorkspaceSavePhase::Saving { .. })
                    || matches!(state.save, WorkspaceSavePhase::Failed { .. })
                    || state.clear.has_intent()
                    || state.force_commit_until_success
            })
    }

    fn has_workspace_save_failure(&self) -> bool {
        self.retention_commit_barrier.failure.is_some()
            || self.workspace_persistence.iter().any(|(key, state)| {
                let dirty = self
                    .model
                    .workspace(key)
                    .is_some_and(|workspace| !workspace.is_saved());
                (dirty
                    && (state.is_read_only()
                        || matches!(state.save, WorkspaceSavePhase::Failed { .. })
                        || matches!(
                            state.load,
                            WorkspaceLoadPhase::Failed(_) | WorkspaceLoadPhase::Conflict
                        )))
                    || (state.force_commit_until_success
                        && matches!(state.save, WorkspaceSavePhase::Failed { .. }))
                    || matches!(state.clear, WorkspaceClearPhase::Failed { .. })
            })
    }

    fn handle_workspace_close_request(&mut self, context: &egui::Context) {
        if self.discard_local_changes_on_close {
            self.workspace_close_guard = WorkspaceCloseGuard::Closed;
            return;
        }
        let close_requested = context.input(|input| input.viewport().close_requested());
        if close_requested && self.has_uncommitted_workspace() {
            context.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.workspace_close_guard = WorkspaceCloseGuard::AwaitingSave;
            self.flush_all_dirty_workspaces();
        }
        if self.workspace_close_guard == WorkspaceCloseGuard::AwaitingSave {
            if self.has_workspace_save_failure() {
                self.workspace_close_guard = WorkspaceCloseGuard::SaveFailed;
            } else if !self.has_uncommitted_workspace() {
                self.workspace_close_guard = WorkspaceCloseGuard::Closed;
                context.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }

    fn capture_pending_workspace_history(
        &mut self,
        operation_id: OperationId,
        workspace_key: WorkspaceKey,
        history_id: u64,
        source: String,
        target: WorkspaceRunTarget,
    ) {
        let Some(instance_id) = self
            .workspace_persistence
            .get(&workspace_key)
            .map(|state| state.identity.instance_id())
        else {
            return;
        };
        self.pending_workspace_history.insert(
            operation_id,
            PendingWorkspaceHistory {
                workspace_key,
                instance_id,
                history_id,
                source,
                target,
                started_at: Instant::now(),
                terminal: None,
            },
        );
    }

    fn capture_workspace_history_terminal(&mut self, event: &UiEvent) -> Option<String> {
        let (operation_id, terminal) = match event {
            UiEvent::QueryFinished {
                operation_id,
                result,
                ..
            } => (
                *operation_id,
                WorkspaceHistoryTerminal {
                    status: WorkspaceHistoryStatus::Succeeded,
                    completed_at_unix_ms: result.provenance.completed_at_unix_ms,
                    duration_ms: saturating_u128_to_u64(result.provenance.duration_ms),
                    returned_rows: saturating_usize_to_u64(result.rows.len()),
                    affected_rows: result.affected_rows,
                    truncated: result.truncated,
                },
            ),
            UiEvent::QueryBatchFinished {
                operation_id,
                discarded_results,
                results,
                error,
                ..
            } => {
                let completed_at_unix_ms =
                    results.last().map_or_else(current_unix_time_ms, |result| {
                        result.provenance.completed_at_unix_ms
                    });
                let duration_ms = results.iter().fold(0_u64, |total, result| {
                    total.saturating_add(saturating_u128_to_u64(result.provenance.duration_ms))
                });
                let returned_rows = results.iter().fold(0_u64, |total, result| {
                    total.saturating_add(saturating_usize_to_u64(result.rows.len()))
                });
                let affected_rows = results.iter().fold(0_u64, |total, result| {
                    total.saturating_add(result.affected_rows)
                });
                (
                    *operation_id,
                    WorkspaceHistoryTerminal {
                        status: error
                            .as_ref()
                            .map_or(WorkspaceHistoryStatus::Succeeded, |error| {
                                history_failure_status(
                                    error.summary,
                                    ConnectionFailureOutcome::Preserve,
                                )
                            }),
                        completed_at_unix_ms,
                        duration_ms,
                        returned_rows,
                        affected_rows,
                        truncated: *discarded_results > 0
                            || results.iter().any(|result| result.truncated),
                    },
                )
            }
            UiEvent::OperationFailed {
                operation_id,
                kind: OperationKind::ExecuteRead | OperationKind::ExecuteMutation,
                error,
                connection_outcome,
                ..
            } => {
                let pending = self.pending_workspace_history.get(operation_id)?;
                (
                    *operation_id,
                    WorkspaceHistoryTerminal {
                        status: history_failure_status(error.summary, *connection_outcome),
                        completed_at_unix_ms: current_unix_time_ms(),
                        duration_ms: saturating_u128_to_u64(
                            pending.started_at.elapsed().as_millis(),
                        ),
                        returned_rows: 0,
                        affected_rows: 0,
                        truncated: false,
                    },
                )
            }
            UiEvent::ExecuteUnavailable {
                operation_id,
                summary,
                ..
            } => {
                let pending = self.pending_workspace_history.get(operation_id)?;
                (
                    *operation_id,
                    WorkspaceHistoryTerminal {
                        status: history_failure_status(
                            *summary,
                            ConnectionFailureOutcome::Preserve,
                        ),
                        completed_at_unix_ms: current_unix_time_ms(),
                        duration_ms: saturating_u128_to_u64(
                            pending.started_at.elapsed().as_millis(),
                        ),
                        returned_rows: 0,
                        affected_rows: 0,
                        truncated: false,
                    },
                )
            }
            _ => return None,
        };
        let pending = self.pending_workspace_history.get_mut(&operation_id)?;
        pending.terminal.get_or_insert(terminal);
        self.drain_pending_workspace_history_terminals()
    }

    fn drain_pending_workspace_history_terminals(&mut self) -> Option<String> {
        let discarded = self
            .pending_workspace_history
            .iter()
            .filter_map(|(operation_id, pending)| {
                let current = self
                    .model
                    .profiles
                    .iter()
                    .find(|profile| {
                        profile.id == pending.workspace_key.profile_id
                            && profile.generation == pending.workspace_key.profile_generation
                    })
                    .and_then(|profile| profile.persisted.safety.instance_id())
                    == Some(pending.instance_id)
                    && self
                        .model
                        .active_generation(&pending.workspace_key.profile_id)
                        == Some(pending.workspace_key.profile_generation);
                (!current).then_some(*operation_id)
            })
            .collect::<Vec<_>>();
        for operation_id in discarded {
            self.pending_workspace_history.remove(&operation_id);
        }

        let mut terminal_operations = self
            .pending_workspace_history
            .iter()
            .filter_map(|(operation_id, pending)| pending.terminal.map(|_| *operation_id))
            .collect::<Vec<_>>();
        terminal_operations.sort_unstable_by_key(|operation_id| operation_id.0);
        let operation_id = terminal_operations.first().copied()?;
        let keys = self.ensure_current_workspace_persistence_bindings();
        let ready = !self.retention_commit_barrier.is_pending_or_failed()
            && keys.iter().all(|key| {
                self.workspace_persistence.get(key).is_some_and(|state| {
                    matches!(state.load, WorkspaceLoadPhase::Ready)
                        && state.mode.is_some()
                        && !state.clear.has_intent()
                        && !state.force_commit_until_success
                        && !matches!(state.save, WorkspaceSavePhase::Saving { .. })
                })
            });
        if !ready {
            let notice =
                "Execution finished, but its protected history remains OutcomeUnknown until workspace recovery."
                    .to_owned();
            self.model.status = notice.clone();
            return Some(notice);
        }
        let pending = self.pending_workspace_history.get(&operation_id).cloned()?;
        let persistence_enabled = self
            .model
            .workspace(&pending.workspace_key)
            .and_then(ProfileWorkspace::persistence)
            .is_some_and(ProfileWorkspacePersistence::persistence_enabled);
        if !persistence_enabled {
            self.pending_workspace_history.remove(&operation_id);
            return self.drain_pending_workspace_history_terminals();
        }
        let terminal = pending.terminal?;
        let plan = self.build_workspace_retention_plan(
            &keys,
            Some(RetentionHistoryChange::Terminal {
                workspace_key: pending.workspace_key,
                history_id: pending.history_id,
                source: pending.source,
                target: pending.target,
                terminal,
            }),
        );
        match plan {
            Ok(plan) => {
                let local_only = plan.local_only;
                let _ = self.apply_workspace_retention_plan(plan);
                self.pending_workspace_history.remove(&operation_id);
                if local_only {
                    let notice =
                        "Execution finished; bounded private history remains Unsaved because the store is read-only."
                            .to_owned();
                    self.model.status = notice.clone();
                    Some(notice)
                } else {
                    None
                }
            }
            Err(_) => {
                let notice =
                    "Execution finished, but its protected history remains OutcomeUnknown; Retry workspace save before continuing."
                        .to_owned();
                self.model.status = notice.clone();
                Some(notice)
            }
        }
    }

    fn set_workspace_persistence_enabled(&mut self, key: &WorkspaceKey, enabled: bool) {
        if self.retention_commit_barrier.is_pending_or_failed() {
            self.model.status =
                "Persistence changes wait for the global private-history save barrier.".to_owned();
            return;
        }
        let Some(profile) = self
            .model
            .profiles
            .iter()
            .find(|profile| {
                profile.id == key.profile_id && profile.generation == key.profile_generation
            })
            .cloned()
        else {
            self.model.status = "The selected workspace is stale.".to_owned();
            return;
        };
        if !self.ensure_workspace_persistence_binding(key, &profile) {
            return;
        }
        let blocked = self.workspace_persistence.get(key).is_some_and(|state| {
            state.is_read_only()
                || matches!(state.load, WorkspaceLoadPhase::Loading { .. })
                || matches!(state.save, WorkspaceSavePhase::Saving { .. })
                || state.clear.has_intent()
        });
        if blocked {
            self.model.status =
                "Workspace persistence cannot change while restore/save is unavailable.".to_owned();
            return;
        }
        let outcome = self
            .model
            .workspaces
            .get_mut(key)
            .map(|workspace| workspace.set_persistence_enabled(enabled));
        match outcome {
            Some(Ok(())) => {
                if !enabled {
                    let revision = self
                        .model
                        .workspace(key)
                        .map_or(0, ProfileWorkspace::revision);
                    if let Some(state) = self.workspace_persistence.get_mut(key) {
                        state.force_commit_until_success = true;
                        state.load = WorkspaceLoadPhase::Ready;
                        state.restore_baseline_revision = Some(revision);
                    }
                }
                self.observe_workspace_revisions(Instant::now());
                let force = !enabled;
                let _ = self.submit_workspace_commit_inner(key, true, force);
            }
            Some(Err(error)) => self.model.status = error.to_string(),
            None => self.model.status = "The selected workspace is unavailable.".to_owned(),
        }
    }

    fn clear_workspace_history(&mut self, key: &WorkspaceKey) {
        if self.retention_commit_barrier.is_pending_or_failed() {
            self.model.status =
                "History Clear waits for the global private-history save barrier.".to_owned();
            return;
        }
        let Some(profile) = self
            .model
            .profiles
            .iter()
            .find(|profile| {
                profile.id == key.profile_id && profile.generation == key.profile_generation
            })
            .cloned()
        else {
            return;
        };
        if !self.ensure_workspace_persistence_binding(key, &profile) {
            return;
        }
        let read_only = self
            .workspace_persistence
            .get(key)
            .is_some_and(WorkspacePersistenceState::is_read_only);
        if read_only {
            self.model.status = "Read-only workspace history cannot be cleared.".to_owned();
            return;
        }
        let outcome = self
            .model
            .workspaces
            .get_mut(key)
            .map(|workspace| workspace.replace_persistence_history(Vec::new()));
        match outcome {
            Some(Ok(())) => {
                self.observe_workspace_revisions(Instant::now());
                let _ = self.submit_workspace_commit(key, true);
            }
            Some(Err(error)) => self.model.status = error.to_string(),
            None => {}
        }
    }

    fn submit_clear_workspace(&mut self, key: &WorkspaceKey) {
        if self.retention_commit_barrier.is_pending_or_failed() {
            self.model.status =
                "Saved-data Clear waits for the global private-history save barrier.".to_owned();
            return;
        }
        let Some(state) = self.workspace_persistence.get(key) else {
            return;
        };
        if state.is_read_only()
            || matches!(state.clear, WorkspaceClearPhase::Pending { .. })
            || matches!(state.load, WorkspaceLoadPhase::Loading { .. })
            || matches!(state.save, WorkspaceSavePhase::Saving { .. })
        {
            self.model.status =
                "Private workspace data cannot be cleared while persistence is busy.".to_owned();
            return;
        }
        let identity = state.identity.clone();
        let Some(base_revision) = self.model.workspace(key).map(ProfileWorkspace::revision) else {
            return;
        };
        let operation_id = self.model.next_operation();
        match self.port.try_submit(UiCommand::ClearWorkspace {
            operation_id,
            identity,
            base_revision,
        }) {
            Ok(()) => {
                if let Some(state) = self.workspace_persistence.get_mut(key) {
                    state.clear = WorkspaceClearPhase::Pending {
                        operation_id,
                        revision: base_revision,
                    };
                }
                self.model.status = "Clearing saved private drafts and history…".to_owned();
            }
            Err(error) => {
                let code = match error {
                    SubmitError::Busy => WorkspaceFailureCode::Busy,
                    SubmitError::Disconnected => WorkspaceFailureCode::Unavailable,
                };
                if let Some(state) = self.workspace_persistence.get_mut(key) {
                    state.clear = WorkspaceClearPhase::Failed {
                        revision: base_revision,
                        code,
                    };
                }
                self.report_submit_error(error);
            }
        }
    }

    fn result_snapshot(&self, result_id: ResultId) -> Option<Arc<ResultSnapshot>> {
        self.model
            .workspaces
            .values()
            .find_map(|workspace| workspace.result_snapshot(result_id))
    }

    fn begin_result_export_state(
        &mut self,
        result_id: ResultId,
        operation_id: OperationId,
    ) -> bool {
        for workspace in self.model.workspaces.values_mut() {
            if workspace.result_snapshot(result_id).is_some() {
                return workspace.begin_result_export(result_id, operation_id);
            }
        }
        false
    }

    fn finish_result_export_state(&mut self, result_id: ResultId, operation_id: OperationId) {
        for workspace in self.model.workspaces.values_mut() {
            workspace.finish_result_export(result_id, operation_id);
        }
    }

    fn handle_export_terminal(&mut self, event: &UiEvent) {
        let (operation_id, result_id, format, destination_committed) = match event {
            UiEvent::ResultExported {
                operation_id,
                result_id,
                format,
                ..
            } => (*operation_id, *result_id, *format, true),
            UiEvent::ResultExportFailed {
                operation_id,
                result_id,
                format,
                destination_committed,
                ..
            } => (*operation_id, *result_id, *format, *destination_committed),
            UiEvent::RuntimeShutdown { .. } => {
                self.pending_export_destinations.clear();
                return;
            }
            _ => return,
        };
        self.finish_result_export_state(result_id, operation_id);
        self.result_export_formats.insert(result_id, format);
        let matching = self
            .pending_export_destinations
            .get(&operation_id)
            .is_some_and(|pending| pending.result_id == result_id && pending.format == format);
        if !matching {
            return;
        }
        if let Some(pending) = self.pending_export_destinations.remove(&operation_id)
            && destination_committed
        {
            self.committed_export_destinations
                .insert(result_id, pending.path);
        }
    }

    fn handle_result_view_intent(
        &mut self,
        snapshot: Arc<ResultSnapshot>,
        intent: ResultViewIntent,
    ) {
        match intent {
            ResultViewIntent::Export(format) => {
                self.choose_result_export_destination(snapshot, format);
            }
            ResultViewIntent::Cancel(operation_id) => {
                match self
                    .port
                    .try_submit(UiCommand::CancelOperation { operation_id })
                {
                    Ok(()) => {
                        self.model.status = format!("Cancelling export {}…", operation_id.0);
                    }
                    Err(error) => self.report_submit_error(error),
                }
            }
        }
    }

    fn choose_result_export_destination(
        &mut self,
        snapshot: Arc<ResultSnapshot>,
        format: ExportFormat,
    ) {
        let Some(path) = native_export_destination(snapshot.provenance.result_id, format) else {
            self.model.status = "Export destination selection cancelled.".to_owned();
            return;
        };
        self.submit_result_export_to(snapshot, format, path);
    }

    fn submit_result_export_to(
        &mut self,
        snapshot: Arc<ResultSnapshot>,
        format: ExportFormat,
        path: PathBuf,
    ) {
        let result_id = snapshot.provenance.result_id;
        if self
            .pending_export_destinations
            .values()
            .any(|pending| pending.result_id == result_id)
        {
            self.model.status = "An export is already active for this result.".to_owned();
            return;
        }
        let operation_id = self.model.next_operation();
        let (overwrite_policy, confirmation) = match std::fs::symlink_metadata(&path) {
            Ok(_) => match confirm_replace(&path) {
                Ok(confirmation) => (OverwritePolicy::ReplaceConfirmed, Some(confirmation)),
                Err(_) => {
                    self.present_local_export_destination_error(result_id, operation_id);
                    return;
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                (OverwritePolicy::DenyOverwrite, None)
            }
            Err(_) => {
                self.present_local_export_destination_error(result_id, operation_id);
                return;
            }
        };
        let command = UiCommand::ExportResult {
            request: ExportResult {
                result_id,
                operation_id,
                snapshot,
                format,
                destination: path.clone(),
                overwrite_policy,
            },
            confirmation,
        };
        match self.port.try_submit(command) {
            Ok(()) => {
                let view_started = self.begin_result_export_state(result_id, operation_id);
                self.pending_export_destinations.insert(
                    operation_id,
                    PendingExportDestination {
                        result_id,
                        format,
                        path,
                    },
                );
                self.result_export_formats.insert(result_id, format);
                self.common_error = None;
                self.model.status = if view_started {
                    format!("Exporting {}…", export_format_label(format))
                } else {
                    format!(
                        "Exporting {} for a result that is no longer visible…",
                        export_format_label(format)
                    )
                };
            }
            Err(error) => self.report_submit_error(error),
        }
    }

    fn present_local_export_destination_error(
        &mut self,
        result_id: ResultId,
        operation_id: OperationId,
    ) {
        let error = PublicOperationError::new_or_internal(
            OperationKind::ExportResult,
            PublicSummary::InvalidInput,
            PublicCode::ExportDestination,
            &crate::public_error::SafeContext::export(result_id, operation_id, false),
        );
        self.model.status = error.summary.message().to_owned();
        self.common_error = Some(VisibleError {
            operation_id,
            error,
        });
    }

    fn reveal_result_export_destination(&mut self, result_id: ResultId) {
        let Some(path) = self.committed_export_destinations.get(&result_id) else {
            self.model.status = "No committed export destination is available.".to_owned();
            return;
        };
        if native_reveal_file(path) {
            self.model.status = "Showing the exported file.".to_owned();
        } else {
            self.model.status = "The exported file is no longer available.".to_owned();
        }
    }

    fn redis_explorer_mut(&mut self, key: &WorkspaceKey) -> &mut RedisExplorer {
        self.redis_explorers.entry(key.clone()).or_insert_with(|| {
            let mut explorer = RedisExplorer::default();
            explorer.set_profile(Some((key.profile_id.clone(), key.profile_generation)));
            explorer
        })
    }

    fn redis_resource_event_identity(
        event: &UiEvent,
    ) -> Option<(
        WorkspaceKey,
        OperationId,
        OperationKind,
        Option<SessionGeneration>,
    )> {
        match event {
            UiEvent::RedisKeysLoaded {
                page,
                session_generation,
                ..
            } => Some((
                WorkspaceKey::new(
                    page.identity.profile_id.clone(),
                    page.identity.profile_generation,
                ),
                page.identity.operation_id,
                OperationKind::BrowseRedis,
                Some(*session_generation),
            )),
            UiEvent::RedisKeysFailed {
                request,
                session_generation,
                ..
            } => Some((
                WorkspaceKey::new(request.profile_id().clone(), request.profile_generation()),
                request.operation_id(),
                OperationKind::BrowseRedis,
                *session_generation,
            )),
            UiEvent::RedisKeyInspected {
                preview,
                session_generation,
                ..
            } => Some((
                WorkspaceKey::new(
                    preview.identity.profile_id.clone(),
                    preview.identity.profile_generation,
                ),
                preview.identity.operation_id,
                OperationKind::InspectRedis,
                Some(*session_generation),
            )),
            UiEvent::RedisKeyInspectFailed {
                request,
                session_generation,
                ..
            } => Some((
                WorkspaceKey::new(request.profile_id().clone(), request.profile_generation()),
                request.operation_id(),
                OperationKind::InspectRedis,
                *session_generation,
            )),
            _ => None,
        }
    }

    fn redis_resource_event_disposition(&self, event: &UiEvent) -> RedisResourceEventDisposition {
        let Some((key, operation_id, kind, session_generation)) =
            Self::redis_resource_event_identity(event)
        else {
            return RedisResourceEventDisposition::NotRedis;
        };
        if self.model.is_config_uncertain()
            || self.model.active_generation(&key.profile_id) != Some(key.profile_generation)
            || !self
                .active_operations
                .get(&key.profile_id)
                .is_some_and(|active| {
                    active.operation_id == operation_id
                        && active.profile_generation == key.profile_generation
                        && active.kind == kind
                })
        {
            return RedisResourceEventDisposition::Ignore;
        }
        if self
            .model
            .connection_state(&key.profile_id)
            .accepts_redis_event_session(session_generation)
        {
            RedisResourceEventDisposition::Apply
        } else {
            RedisResourceEventDisposition::StaleTerminal(operation_id)
        }
    }

    fn fold_redis_explorer_event(&mut self, event: &UiEvent) {
        if let Some((key, ..)) = Self::redis_resource_event_identity(event) {
            if let Some(explorer) = self.redis_explorers.get_mut(&key) {
                explorer.handle_event(event);
            }
            return;
        }
        let clear = match event {
            UiEvent::ConnectionClosed {
                operation_id,
                profile_id,
                profile_generation,
                ..
            } => self
                .active_operations
                .get(profile_id)
                .is_some_and(|active| {
                    active.operation_id == *operation_id
                        && active.profile_generation == *profile_generation
                        && active.kind == OperationKind::DisconnectProfile
                })
                .then(|| WorkspaceKey::new(profile_id.clone(), *profile_generation)),
            UiEvent::ConnectionReady {
                operation_id,
                profile_id,
                profile_generation,
                ..
            } => self
                .active_operations
                .get(profile_id)
                .is_some_and(|active| {
                    active.operation_id == *operation_id
                        && active.profile_generation == *profile_generation
                        && matches!(
                            active.kind,
                            OperationKind::ConnectProfile | OperationKind::ReconnectProfile
                        )
                })
                .then(|| WorkspaceKey::new(profile_id.clone(), *profile_generation)),
            UiEvent::OperationFailed {
                operation_id,
                profile_id,
                profile_generation,
                kind,
                connection_outcome,
                ..
            } if matches!(
                kind,
                OperationKind::DisconnectProfile | OperationKind::ReconnectProfile
            ) && !matches!(
                connection_outcome,
                super::model::ConnectionFailureOutcome::Preserve
            ) =>
            {
                self.active_operations
                    .get(profile_id)
                    .is_some_and(|active| {
                        active.operation_id == *operation_id
                            && active.profile_generation == *profile_generation
                            && active.kind == *kind
                    })
                    .then(|| WorkspaceKey::new(profile_id.clone(), *profile_generation))
            }
            UiEvent::ConfigUncertain { .. } | UiEvent::RuntimeShutdown { .. } => {
                self.redis_explorers.clear();
                self.visible_redis_workspace = None;
                None
            }
            _ => None,
        };
        if let Some(key) = clear {
            self.redis_explorers.remove(&key);
            if self.visible_redis_workspace.as_ref() == Some(&key) {
                self.visible_redis_workspace = None;
            }
        }
    }

    fn prune_redis_explorers(&mut self) {
        if self.model.is_config_uncertain() {
            self.redis_explorers.clear();
            self.visible_redis_workspace = None;
            return;
        }
        self.redis_explorers.retain(|key, _| {
            self.model.active_generation(&key.profile_id) == Some(key.profile_generation)
        });
        if self
            .visible_redis_workspace
            .as_ref()
            .is_some_and(|key| !self.redis_explorers.contains_key(key))
        {
            self.visible_redis_workspace = None;
        }
    }

    fn prune_result_export_state(&mut self) {
        let current_results = self
            .model
            .workspaces
            .values()
            .flat_map(|workspace| {
                workspace
                    .result_tabs()
                    .iter()
                    .map(|tab| tab.snapshot().provenance.result_id)
                    .chain(
                        workspace
                            .result
                            .as_ref()
                            .map(|result| result.provenance.result_id),
                    )
            })
            .chain(
                self.pending_export_destinations
                    .values()
                    .map(|pending| pending.result_id),
            )
            .collect::<HashSet<_>>();
        self.committed_export_destinations
            .retain(|result_id, _| current_results.contains(result_id));
        self.result_export_formats
            .retain(|result_id, _| current_results.contains(result_id));
    }

    fn workspace_retages_for_profiles(
        &self,
        profiles: &[ProfileSnapshot],
    ) -> Vec<(WorkspaceKey, WorkspaceKey)> {
        self.model
            .profiles
            .iter()
            .filter_map(|previous| {
                let refreshed = profiles.iter().find(|profile| profile.id == previous.id)?;
                let previous_instance = previous.persisted.safety.instance_id();
                let same_instance = previous_instance.is_some()
                    && previous_instance == refreshed.persisted.safety.instance_id();
                (same_instance && previous.generation != refreshed.generation).then(|| {
                    (
                        WorkspaceKey::new(previous.id.clone(), previous.generation),
                        WorkspaceKey::new(refreshed.id.clone(), refreshed.generation),
                    )
                })
            })
            .collect()
    }

    fn apply_workspace_retages(&mut self, retages: Vec<(WorkspaceKey, WorkspaceKey)>) {
        for (previous, refreshed) in retages {
            if self.model.active_generation(&refreshed.profile_id)
                != Some(refreshed.profile_generation)
            {
                continue;
            }
            for pending in self.pending_workspace_history.values_mut() {
                if pending.workspace_key == previous {
                    pending.workspace_key = refreshed.clone();
                }
            }
            if let Some(geometry) = self.workspace_geometries.remove(&previous)
                && !self.workspace_geometries.contains_key(&refreshed)
            {
                self.workspace_geometries
                    .insert(refreshed.clone(), geometry);
            }
            if let Some(pane) = self.collapsed_workspace_panes.remove(&previous)
                && !self.collapsed_workspace_panes.contains_key(&refreshed)
            {
                self.collapsed_workspace_panes
                    .insert(refreshed.clone(), pane);
            }
            if self.compact_workspace.as_ref() == Some(&previous) {
                self.compact_workspace = Some(refreshed.clone());
            }
            let mut reissue_clear = false;
            if let Some(mut state) = self.workspace_persistence.remove(&previous)
                && let Some(profile) = self.model.profiles.iter().find(|profile| {
                    profile.id == refreshed.profile_id
                        && profile.generation == refreshed.profile_generation
                })
                && let Some(instance_id) = profile.persisted.safety.instance_id()
            {
                state.identity = WorkspaceIdentity::new(
                    refreshed.profile_id.clone(),
                    refreshed.profile_generation,
                    instance_id,
                );
                let baseline_refresh_required =
                    state.refresh_durable_baseline_only || state.submitted_commit.is_some();
                state.refresh_durable_baseline_only = baseline_refresh_required;
                let restore_still_required =
                    !matches!(state.load, WorkspaceLoadPhase::Ready) || baseline_refresh_required;
                state.load = if restore_still_required {
                    state.mode = None;
                    state.read_only_reason = None;
                    WorkspaceLoadPhase::Unloaded
                } else {
                    WorkspaceLoadPhase::Ready
                };
                state.save = WorkspaceSavePhase::Idle;
                state.submitted_commit = None;
                state.resolve_conflict_on_commit = false;
                reissue_clear = state.clear.has_intent();
                state.clean_empty_baseline_pending = None;
                state.observed_revision = self
                    .model
                    .workspace(&refreshed)
                    .map_or(state.observed_revision, ProfileWorkspace::revision);
                state.clear = if reissue_clear {
                    WorkspaceClearPhase::Failed {
                        revision: state.observed_revision,
                        code: WorkspaceFailureCode::Stale,
                    }
                } else {
                    WorkspaceClearPhase::Idle
                };
                state.dirty_since.get_or_insert(Instant::now());
                self.workspace_persistence.insert(refreshed.clone(), state);
            }
            if reissue_clear {
                self.submit_clear_workspace(&refreshed);
            }
            if let Some(search) = self.workspace_history_search.remove(&previous) {
                self.workspace_history_search
                    .insert(refreshed.clone(), search);
            }
            if let Some(confirmation) = self.editor_discard_confirmation.as_mut()
                && confirmation.workspace_key == previous
            {
                confirmation.workspace_key = refreshed.clone();
            }
            if self.workspace_clear_confirmation.as_ref() == Some(&previous) {
                self.workspace_clear_confirmation = Some(refreshed.clone());
            }
            if self.workspace_restore_conflict_confirmation.as_ref() == Some(&previous) {
                self.workspace_restore_conflict_confirmation = Some(refreshed.clone());
            }
            if self.workspace_history_focus.as_ref() == Some(&previous) {
                self.workspace_history_focus = Some(refreshed);
            }
        }
    }

    fn poll_events(&mut self) {
        for mut event in self.port.drain_events(EVENT_DRAIN_LIMIT) {
            let profiles_refresh_accepted = match &event {
                UiEvent::ProfilesLoaded { operation_id, .. }
                | UiEvent::ProfilesFailed { operation_id, .. }
                | UiEvent::ConfigUncertain { operation_id } => {
                    self.model.profiles_operation_is_newer(*operation_id)
                }
                _ => true,
            };
            if !profiles_refresh_accepted {
                continue;
            }
            let profiles_loaded_accepted =
                profiles_refresh_accepted && matches!(&event, UiEvent::ProfilesLoaded { .. });
            match self.redis_resource_event_disposition(&event) {
                RedisResourceEventDisposition::Ignore => continue,
                RedisResourceEventDisposition::StaleTerminal(operation_id) => {
                    self.finish_active_operation(&event);
                    self.retry_recipes.remove(OperationRecipeId(operation_id.0));
                    continue;
                }
                RedisResourceEventDisposition::NotRedis | RedisResourceEventDisposition::Apply => {}
            }
            self.handle_workspace_event(&event);
            let history_terminal_notice = self.capture_workspace_history_terminal(&event);
            self.handle_export_terminal(&event);
            self.attach_retry_recipe(&mut event);
            self.capture_common_error(&event);
            let credential_retry = self.handle_credential_terminal(&event);
            self.fold_redis_explorer_event(&event);
            self.finish_active_operation(&event);
            self.fold_mysql_explorer_event(&event);
            let profile_result = self
                .profile_editor
                .as_mut()
                .map_or(ProfileEventResult::Ignored, |editor| {
                    editor.handle_event(&event)
                });
            match profile_result {
                ProfileEventResult::Saved(profile_id, warning) => {
                    self.model.fold(event);
                    self.model.selected_profile = Some(profile_id);
                    self.model.status = warning.map_or_else(
                        || "Profile saved; refreshing profiles…".to_owned(),
                        |summary| summary.message().to_owned(),
                    );
                    self.profile_editor = None;
                    let operation_id = self.model.next_operation();
                    if let Err(error) = self
                        .port
                        .try_submit(UiCommand::RefreshProfiles { operation_id })
                    {
                        self.report_submit_error(error);
                    }
                    continue;
                }
                ProfileEventResult::SavedAndConnect(profile_id, warning) => {
                    self.model.fold(event);
                    self.model.selected_profile = Some(profile_id.clone());
                    self.model.status = warning.map_or_else(
                        || "Profile saved; refreshing before connect…".to_owned(),
                        |summary| summary.message().to_owned(),
                    );
                    self.profile_editor = None;
                    let operation_id = self.model.next_operation();
                    match self
                        .port
                        .try_submit(UiCommand::RefreshProfiles { operation_id })
                    {
                        Ok(()) => {
                            self.pending_connect_after_refresh = Some((profile_id, operation_id));
                        }
                        Err(error) => {
                            self.pending_connect_after_refresh = None;
                            self.report_submit_error(error);
                        }
                    }
                    continue;
                }
                ProfileEventResult::Failed => {
                    if let Some(editor) = &self.profile_editor {
                        self.model.status = editor.status().to_owned();
                    }
                    continue;
                }
                ProfileEventResult::Ignored => {}
            }
            let connect_follow_up = self.pending_connect_after_refresh.as_ref().and_then(
                |(profile_id, expected_operation)| match &event {
                    UiEvent::ProfilesLoaded { operation_id, .. }
                        if operation_id == expected_operation =>
                    {
                        Some((
                            profile_id.clone(),
                            true,
                            self.model.profiles_operation_is_newer(*operation_id),
                        ))
                    }
                    UiEvent::ProfilesFailed { operation_id, .. }
                        if operation_id == expected_operation =>
                    {
                        Some((profile_id.clone(), false, false))
                    }
                    _ => None,
                },
            );
            let workspace_retages = match &event {
                UiEvent::ProfilesLoaded { profiles, .. } => {
                    self.workspace_retages_for_profiles(profiles)
                }
                _ => Vec::new(),
            };
            self.model.fold(event);
            if let Some(notice) = history_terminal_notice {
                self.model.status = notice;
            }
            self.apply_workspace_retages(workspace_retages);
            if profiles_loaded_accepted {
                let keys = self.ensure_current_workspace_persistence_bindings();
                if self.retention_commit_barrier.is_pending_or_failed()
                    && !self.retention_barrier_references_current_identities()
                {
                    self.retention_commit_barrier.active = None;
                    self.retention_commit_barrier.queue.clear();
                    self.retention_commit_barrier.failure =
                        Some(RetentionBarrierFailure::IdentityChanged);
                    self.retention_reconcile_required = true;
                    self.model.status =
                        "Profile identities changed during private-history save; Retry after restore."
                            .to_owned();
                }
                for key in keys {
                    self.request_workspace_load(&key);
                }
            }
            let _ = self.reconcile_workspace_retention(false);
            if let Some(notice) = self.drain_pending_workspace_history_terminals() {
                self.model.status = notice;
            }
            if let Some((profile_id, loaded, accepted)) = connect_follow_up {
                self.pending_connect_after_refresh = None;
                if loaded && accepted && self.model.active_generation(&profile_id).is_some() {
                    self.model.selected_profile = Some(profile_id.clone());
                    self.submit_test(profile_id);
                }
            }
            if let Some((recipe_id, source_operation)) = credential_retry {
                self.retry_recipe(recipe_id, Some(source_operation));
            }
        }
        self.mysql_explorers.retain(|(profile_id, generation), _| {
            self.model.active_generation(profile_id) == Some(*generation)
        });
        self.prune_redis_explorers();
        self.prune_result_export_state();
        self.prune_active_operations();
        let keep_editor_discard =
            self.editor_discard_confirmation
                .as_ref()
                .is_some_and(|confirmation| {
                    self.model
                        .workspace(&confirmation.workspace_key)
                        .and_then(|workspace| workspace.editor_tab(confirmation.tab_id))
                        .is_some()
                });
        if self.editor_discard_confirmation.is_some() && !keep_editor_discard {
            self.editor_discard_confirmation = None;
        }
        let migration_required = self.model.config.migration_required();
        let migration_backup = self.model.config.migration_backup().map(PathBuf::from);
        if let Some(editor) = self.profile_editor.as_mut() {
            editor.set_config_uncertain(self.model.is_config_uncertain());
            editor.set_migration_presentation(migration_required, migration_backup.as_deref());
        }
        if let Some(confirmation) = self.delete_confirmation.as_mut() {
            confirmation.migration_backup = migration_backup;
            if confirmation.migration_backup.is_none() {
                confirmation.migration_confirmed = false;
            }
        }
    }

    fn attach_retry_recipe(&self, event: &mut UiEvent) {
        match event {
            UiEvent::OperationFailed {
                operation_id,
                profile_id,
                kind,
                summary,
                error,
                ..
            } => {
                let recipe_id = OperationRecipeId(operation_id.0);
                if self
                    .retry_recipes
                    .get(recipe_id)
                    .is_some_and(|recipe| recipe.operation_kind() == *kind)
                {
                    *error = PublicOperationError::new_or_internal(
                        *kind,
                        *summary,
                        error.code,
                        &crate::public_error::SafeContext::profile_with_recipe(
                            profile_id.clone(),
                            *operation_id,
                            recipe_id,
                        ),
                    );
                }
            }
            UiEvent::CatalogPageFailed {
                request,
                summary,
                error,
                ..
            } => {
                let recipe_id = OperationRecipeId(request.operation_id().0);
                if self.retry_recipes.contains(recipe_id) {
                    *error = PublicOperationError::new_or_internal(
                        OperationKind::BrowseMySql,
                        *summary,
                        error.code,
                        &crate::public_error::SafeContext::profile_with_recipe(
                            request.profile_id().clone(),
                            request.operation_id(),
                            recipe_id,
                        ),
                    );
                }
            }
            UiEvent::RedisKeysFailed { request, error, .. } => {
                let recipe_id = OperationRecipeId(request.operation_id().0);
                if self.retry_recipes.contains(recipe_id) {
                    *error = PublicOperationError::new_or_internal(
                        OperationKind::BrowseRedis,
                        error.summary,
                        error.code,
                        &crate::public_error::SafeContext::profile_with_recipe(
                            request.profile_id().clone(),
                            request.operation_id(),
                            recipe_id,
                        ),
                    );
                }
            }
            UiEvent::RedisKeyInspectFailed { request, error, .. } => {
                let recipe_id = OperationRecipeId(request.operation_id().0);
                if self.retry_recipes.contains(recipe_id) {
                    *error = PublicOperationError::new_or_internal(
                        OperationKind::InspectRedis,
                        error.summary,
                        error.code,
                        &crate::public_error::SafeContext::profile_with_recipe(
                            request.profile_id().clone(),
                            request.operation_id(),
                            recipe_id,
                        ),
                    );
                }
            }
            _ => {}
        }
    }

    fn capture_common_error(&mut self, event: &UiEvent) {
        let visible = match event {
            UiEvent::ProfilesFailed {
                operation_id,
                error,
                ..
            }
            | UiEvent::ProfileCreateFailed {
                operation_id,
                error,
                ..
            }
            | UiEvent::ProfileUpdateFailed {
                operation_id,
                error,
                ..
            }
            | UiEvent::DraftOperationFailed {
                operation_id,
                error,
                ..
            }
            | UiEvent::CredentialsStoreFailed {
                operation_id,
                error,
                ..
            }
            | UiEvent::ResultExportFailed {
                operation_id,
                error,
                ..
            } => Some(VisibleError {
                operation_id: *operation_id,
                error: error.clone(),
            }),
            UiEvent::OperationFailed {
                operation_id,
                kind,
                error,
                ..
            } if !matches!(
                kind,
                OperationKind::ExecuteRead | OperationKind::ExecuteMutation
            ) =>
            {
                Some(VisibleError {
                    operation_id: *operation_id,
                    error: error.clone(),
                })
            }
            UiEvent::ConfigUncertain { .. } | UiEvent::RuntimeShutdown { .. } => {
                self.common_error = None;
                None
            }
            _ => None,
        };
        if let Some(visible) = visible {
            self.common_error = Some(visible);
        }
    }

    fn handle_credential_terminal(
        &mut self,
        event: &UiEvent,
    ) -> Option<(OperationRecipeId, OperationKind)> {
        if matches!(
            event,
            UiEvent::ConfigUncertain { .. } | UiEvent::RuntimeShutdown { .. }
        ) {
            self.cancel_credential_prompt();
            return None;
        }
        let (operation_id, profile_id, profile_generation, stored) = match event {
            UiEvent::CredentialsStored {
                operation_id,
                profile_id,
                profile_generation,
            } => (*operation_id, profile_id, *profile_generation, true),
            UiEvent::CredentialsStoreFailed {
                operation_id,
                profile_id,
                profile_generation,
                ..
            } => (*operation_id, profile_id, *profile_generation, false),
            UiEvent::OperationFailed {
                operation_id,
                profile_id,
                profile_generation,
                ..
            } if self
                .credential_prompt
                .as_ref()
                .is_some_and(|prompt| prompt.store_operation_id == Some(*operation_id)) =>
            {
                (*operation_id, profile_id, *profile_generation, false)
            }
            _ => return None,
        };
        let matches_prompt = self.credential_prompt.as_ref().is_some_and(|prompt| {
            prompt.store_operation_id == Some(operation_id)
                && prompt.profile_id == *profile_id
                && prompt.profile_generation == profile_generation
        });
        if !matches_prompt {
            return None;
        }
        let prompt = self.credential_prompt.take()?;
        let retry = prompt.retry_recipe_id;
        if !stored
            || self.model.active_generation(profile_id) != Some(profile_generation)
            || self.model.is_config_uncertain()
        {
            if let Some(recipe_id) = retry {
                self.retry_recipes.remove(recipe_id);
            }
            return None;
        }
        retry.map(|recipe_id| (recipe_id, prompt.source_operation))
    }

    fn finish_active_operation(&mut self, event: &UiEvent) {
        if let UiEvent::ConfigUncertain { operation_id } = event
            && !self.model.profiles_operation_is_newer(*operation_id)
        {
            return;
        }
        if matches!(
            event,
            UiEvent::ConfigUncertain { .. } | UiEvent::RuntimeShutdown { .. }
        ) {
            self.active_operations.clear();
            self.pending_deletes.clear();
            self.retry_recipes.clear();
            return;
        }
        let terminal = match event {
            UiEvent::ConnectionReady {
                operation_id,
                profile_id,
                ..
            }
            | UiEvent::ConnectionClosed {
                operation_id,
                profile_id,
                ..
            }
            | UiEvent::QueryFinished {
                operation_id,
                profile_id,
                ..
            }
            | UiEvent::QueryBatchFinished {
                operation_id,
                profile_id,
                ..
            }
            | UiEvent::ExecuteUnavailable {
                operation_id,
                profile_id,
                ..
            }
            | UiEvent::OperationFailed {
                operation_id,
                profile_id,
                ..
            }
            | UiEvent::ProfileDeleted {
                operation_id,
                profile_id,
                ..
            } => Some((profile_id, *operation_id)),
            UiEvent::CatalogPageLoaded { page, .. } => {
                Some((&page.identity.profile_id, page.identity.operation_id))
            }
            UiEvent::CatalogPageFailed { request, .. } => {
                Some((request.profile_id(), request.operation_id()))
            }
            UiEvent::RedisKeysLoaded { page, .. } => {
                Some((&page.identity.profile_id, page.identity.operation_id))
            }
            UiEvent::RedisKeysFailed { request, .. } => {
                Some((request.profile_id(), request.operation_id()))
            }
            UiEvent::RedisKeyInspected { preview, .. } => {
                Some((&preview.identity.profile_id, preview.identity.operation_id))
            }
            UiEvent::RedisKeyInspectFailed { request, .. } => {
                Some((request.profile_id(), request.operation_id()))
            }
            _ => None,
        };
        if let Some((profile_id, operation_id)) = terminal {
            if let Some(pending) = self.pending_deletes.get_mut(profile_id)
                && pending
                    .prior_active
                    .is_some_and(|prior| prior.operation_id == operation_id)
            {
                pending.prior_finished = true;
            }
            if let UiEvent::OperationFailed {
                kind: OperationKind::DeleteProfile,
                summary,
                ..
            } = event
                && let Some(pending) = self.pending_deletes.get(profile_id).copied()
                && pending.operation_id == operation_id
            {
                self.pending_deletes.remove(profile_id);
                if self
                    .active_operations
                    .get(profile_id)
                    .is_some_and(|active| active.operation_id == operation_id)
                {
                    self.active_operations.remove(profile_id);
                }
                if delete_failure_is_known_non_committed(*summary)
                    && !pending.prior_finished
                    && self.model.active_generation(profile_id) == Some(pending.profile_generation)
                    && let Some(prior) = pending.prior_active
                {
                    self.active_operations.insert(profile_id.clone(), prior);
                }
                return;
            }
            if matches!(event, UiEvent::ProfileDeleted { .. }) {
                self.pending_deletes.remove(profile_id);
            }
            if self
                .active_operations
                .get(profile_id)
                .is_some_and(|active| active.operation_id == operation_id)
            {
                self.active_operations.remove(profile_id);
            }
            if !matches!(
                event,
                UiEvent::OperationFailed { .. }
                    | UiEvent::CatalogPageFailed { .. }
                    | UiEvent::RedisKeysFailed { .. }
                    | UiEvent::RedisKeyInspectFailed { .. }
            ) {
                self.retry_recipes.remove(OperationRecipeId(operation_id.0));
            }
        }
    }

    fn prune_active_operations(&mut self) {
        self.active_operations.retain(|profile_id, active| {
            self.model.active_generation(profile_id) == Some(active.profile_generation)
        });
        self.pending_deletes.retain(|profile_id, pending| {
            self.model.active_generation(profile_id) == Some(pending.profile_generation)
        });
        self.collapsed_workspace_panes.retain(|key, _| {
            self.model.active_generation(&key.profile_id) == Some(key.profile_generation)
        });
        self.retry_recipes
            .retain_current(&self.model.active_generations);
        if self.credential_prompt.as_ref().is_some_and(|prompt| {
            self.model.active_generation(&prompt.profile_id) != Some(prompt.profile_generation)
        }) {
            self.cancel_credential_prompt();
        }
    }

    fn fold_mysql_explorer_event(&mut self, event: &UiEvent) {
        match event {
            UiEvent::CatalogPageLoaded { page, .. } => {
                let key = (
                    page.identity.profile_id.clone(),
                    page.identity.profile_generation,
                );
                self.mysql_explorers
                    .entry(key)
                    .or_default()
                    .handle_loaded(page.clone());
            }
            UiEvent::CatalogPageFailed { request, error, .. } => {
                let key = (request.profile_id().clone(), request.profile_generation());
                self.mysql_explorers
                    .entry(key)
                    .or_default()
                    .handle_failed(request.clone(), error.summary);
            }
            UiEvent::ProfileDeleted {
                profile_id,
                profile_generation,
                ..
            } => {
                self.mysql_explorers
                    .remove(&(profile_id.clone(), *profile_generation));
            }
            _ => {}
        }
    }

    fn submit_refresh(&mut self) {
        let operation_id = self.model.next_operation();
        match self
            .port
            .try_submit(UiCommand::RefreshProfiles { operation_id })
        {
            Ok(()) => self.model.status = "Reloading profiles…".to_owned(),
            Err(error) => self.report_submit_error(error),
        }
    }

    fn submit_test(&mut self, profile_id: ProfileId) {
        if self.model.is_config_uncertain() {
            self.model.status = "Reload profiles before using connections.".to_owned();
            return;
        }
        let Some(profile) = self
            .model
            .profiles
            .iter()
            .find(|profile| profile.id == profile_id)
            .cloned()
        else {
            self.model.status = "Unknown profile".to_owned();
            return;
        };
        if !profile.is_ready() {
            self.model.status = "Driver is planned and unavailable".to_owned();
            return;
        }
        if !profile.can_connect() {
            self.model.status = "Environment credential is not available".to_owned();
            return;
        }
        if self.model.active_generation(&profile_id) != Some(profile.generation) {
            self.model.status = "The selected profile generation is stale.".to_owned();
            return;
        }
        if profile.persisted.credential_mode == crate::model::CredentialMode::Session
            && (!profile.has_current_session_secret
                || matches!(
                    self.model.connection_state(&profile_id),
                    ConnectionState::NeedsCredential
                ))
        {
            self.open_session_credential_prompt(profile_id);
            return;
        }
        if self.model.connection_state(&profile_id).is_pending() {
            self.model.status = "Connection work is already pending".to_owned();
            return;
        }
        if self.active_operations.contains_key(&profile_id) {
            self.model.status = "Another operation is active for this connection".to_owned();
            return;
        }
        self.submit_connect_exact(profile_id, profile.generation, DEFAULT_TIMEOUT_MS);
    }

    fn submit_connect_exact(
        &mut self,
        profile_id: ProfileId,
        profile_generation: ProfileGeneration,
        timeout_ms: u64,
    ) {
        if self.model.connection_state(&profile_id).is_pending()
            || self.active_operations.contains_key(&profile_id)
        {
            self.model.status = "Connection work is already pending".to_owned();
            return;
        }
        let operation_id = self.model.next_operation();
        match self.port.try_submit(UiCommand::TestConnection {
            operation_id,
            profile_id: profile_id.clone(),
            profile_generation,
            timeout_ms,
        }) {
            Ok(()) => {
                self.retry_recipes.register(
                    operation_id,
                    RetryRecipe::Connect {
                        profile_id: profile_id.clone(),
                        profile_generation,
                        timeout_ms,
                    },
                );
                self.model
                    .connection_states
                    .insert(profile_id.clone(), ConnectionState::Pending(operation_id));
                self.active_operations.insert(
                    profile_id,
                    ActiveOperation {
                        operation_id,
                        profile_generation,
                        kind: OperationKind::ConnectProfile,
                    },
                );
                self.model.status = "Connecting…".to_owned();
            }
            Err(error) => self.report_submit_error(error),
        }
    }

    fn open_session_credential_prompt(&mut self, profile_id: ProfileId) {
        let Some(profile) = self
            .model
            .profiles
            .iter()
            .find(|profile| profile.id == profile_id)
            .cloned()
        else {
            self.model.status = "Unknown profile".to_owned();
            return;
        };
        if self.model.active_generation(&profile_id) != Some(profile.generation)
            || profile.persisted.credential_mode != crate::model::CredentialMode::Session
        {
            self.model.status =
                "The current profile cannot accept a session credential.".to_owned();
            return;
        }
        let recipe_seed = self.model.next_operation();
        let recipe_id = self.retry_recipes.register(
            recipe_seed,
            RetryRecipe::Connect {
                profile_id: profile_id.clone(),
                profile_generation: profile.generation,
                timeout_ms: DEFAULT_TIMEOUT_MS,
            },
        );
        self.open_credential_prompt_for(
            profile_id,
            profile.generation,
            OperationKind::ConnectProfile,
            Some(recipe_id),
        );
    }

    fn open_credential_prompt_for(
        &mut self,
        profile_id: ProfileId,
        profile_generation: ProfileGeneration,
        source_operation: OperationKind,
        retry_recipe_id: Option<OperationRecipeId>,
    ) {
        if self.model.active_generation(&profile_id) != Some(profile_generation) {
            if let Some(recipe_id) = retry_recipe_id {
                self.retry_recipes.remove(recipe_id);
            }
            self.model.status = "The selected profile generation is stale.".to_owned();
            return;
        }
        let accepts_session = self.model.profiles.iter().any(|profile| {
            profile.id == profile_id
                && profile.generation == profile_generation
                && profile.persisted.credential_mode == crate::model::CredentialMode::Session
        });
        if !accepts_session {
            if let Some(recipe_id) = retry_recipe_id {
                self.retry_recipes.remove(recipe_id);
            }
            self.model.status =
                "The current profile cannot accept a session credential.".to_owned();
            return;
        }
        self.cancel_credential_prompt();
        self.credential_prompt = Some(CredentialPrompt {
            profile_id,
            profile_generation,
            source_operation,
            retry_recipe_id,
            store_operation_id: None,
            secret: ReplacementSecretBuffer::default(),
            status: "Enter the credential for this app session.".to_owned(),
        });
        self.model.status = "Enter the protected session credential.".to_owned();
    }

    fn cancel_credential_prompt(&mut self) {
        if let Some(prompt) = self.credential_prompt.take()
            && let Some(recipe_id) = prompt.retry_recipe_id
        {
            self.retry_recipes.remove(recipe_id);
        }
    }

    fn submit_credential_prompt(&mut self) {
        let Some(mut prompt) = self.credential_prompt.take() else {
            return;
        };
        if prompt.store_operation_id.is_some() {
            self.credential_prompt = Some(prompt);
            return;
        }
        if self.model.is_config_uncertain()
            || self.model.active_generation(&prompt.profile_id) != Some(prompt.profile_generation)
        {
            if let Some(recipe_id) = prompt.retry_recipe_id {
                self.retry_recipes.remove(recipe_id);
            }
            self.model.status = "Reload profiles before storing credentials.".to_owned();
            return;
        }
        if prompt.secret.is_empty() {
            prompt.status = "Enter a session credential.".to_owned();
            self.credential_prompt = Some(prompt);
            return;
        }
        let permit = match self.port.try_reserve_mutation() {
            Ok(permit) => permit,
            Err(error) => {
                if let Some(recipe_id) = prompt.retry_recipe_id {
                    self.retry_recipes.remove(recipe_id);
                }
                self.model.status = match error {
                    SubmitError::Busy => "Service is busy; command was not submitted".to_owned(),
                    SubmitError::Disconnected => "Service is unavailable".to_owned(),
                };
                return;
            }
        };
        let secret = match prompt.secret.take_for_save() {
            Ok(secret) => secret,
            Err(_) => {
                prompt.status = "Enter a session credential.".to_owned();
                self.credential_prompt = Some(prompt);
                return;
            }
        };
        let operation_id = self.model.next_operation();
        let command = UiCommand::StoreCredentials {
            operation_id,
            profile_id: prompt.profile_id.clone(),
            profile_generation: prompt.profile_generation,
            source_operation: prompt.source_operation,
            secret,
        };
        match permit.submit(command) {
            Ok(()) => {
                prompt.store_operation_id = Some(operation_id);
                prompt.status = "Storing credential…".to_owned();
                self.credential_prompt = Some(prompt);
                self.model.status = "Storing protected session credential…".to_owned();
            }
            Err(error) => {
                if let Some(recipe_id) = prompt.retry_recipe_id {
                    self.retry_recipes.remove(recipe_id);
                }
                self.report_submit_error(error);
            }
        }
    }

    fn retry_recipe(
        &mut self,
        recipe_id: OperationRecipeId,
        expected_operation: Option<OperationKind>,
    ) {
        let Some(recipe) = self.retry_recipes.take(recipe_id) else {
            self.model.status = "That retry is no longer available.".to_owned();
            return;
        };
        if expected_operation.is_some_and(|expected| expected != recipe.operation_kind()) {
            self.model.status = "The retry does not match this error.".to_owned();
            return;
        }
        if self.model.is_config_uncertain()
            || self.model.active_generation(recipe.profile_id())
                != Some(recipe.profile_generation())
        {
            self.model.status = "The retry recipe is stale.".to_owned();
            return;
        }
        match recipe {
            RetryRecipe::Connect {
                profile_id,
                profile_generation,
                timeout_ms,
            } => self.submit_connect_exact(profile_id, profile_generation, timeout_ms),
            RetryRecipe::Reconnect {
                profile_id,
                profile_generation,
                timeout_ms,
            } => self.submit_reconnect_exact(profile_id, profile_generation, timeout_ms),
            RetryRecipe::Catalog(request) => self.retry_catalog_request(request),
            RetryRecipe::RedisScan { request, restart } => {
                self.retry_redis_scan(request, restart);
            }
            RetryRecipe::RedisInspect(request) => self.retry_redis_inspect(request),
        }
    }

    fn submit_disconnect(&mut self, profile_id: ProfileId) {
        if self.model.is_config_uncertain() {
            self.model.status = "Reload profiles before using connections.".to_owned();
            return;
        }
        let Some(profile_generation) = self.model.active_generation(&profile_id) else {
            self.model.status = "Unknown profile".to_owned();
            return;
        };
        let operation_id = self.model.next_operation();
        match self.port.try_submit(UiCommand::DisconnectProfile {
            operation_id,
            profile_id: profile_id.clone(),
            profile_generation,
        }) {
            Ok(()) => {
                self.model
                    .connection_states
                    .insert(profile_id.clone(), ConnectionState::Pending(operation_id));
                self.active_operations.insert(
                    profile_id,
                    ActiveOperation {
                        operation_id,
                        profile_generation,
                        kind: OperationKind::DisconnectProfile,
                    },
                );
                self.model.status = "Disconnecting…".to_owned();
            }
            Err(error) => self.report_submit_error(error),
        }
    }

    fn submit_reconnect(&mut self, profile_id: ProfileId) {
        if self.model.is_config_uncertain() {
            self.model.status = "Reload profiles before using connections.".to_owned();
            return;
        }
        let Some(profile_generation) = self.model.active_generation(&profile_id) else {
            self.model.status = "Unknown profile".to_owned();
            return;
        };
        self.submit_reconnect_exact(profile_id, profile_generation, DEFAULT_TIMEOUT_MS);
    }

    fn submit_reconnect_exact(
        &mut self,
        profile_id: ProfileId,
        profile_generation: ProfileGeneration,
        timeout_ms: u64,
    ) {
        if self.model.connection_state(&profile_id).is_pending()
            || self.active_operations.contains_key(&profile_id)
        {
            self.model.status = "Connection work is already pending".to_owned();
            return;
        }
        let operation_id = self.model.next_operation();
        match self.port.try_submit(UiCommand::ReconnectProfile {
            operation_id,
            profile_id: profile_id.clone(),
            profile_generation,
            timeout_ms,
        }) {
            Ok(()) => {
                self.retry_recipes.register(
                    operation_id,
                    RetryRecipe::Reconnect {
                        profile_id: profile_id.clone(),
                        profile_generation,
                        timeout_ms,
                    },
                );
                self.model
                    .connection_states
                    .insert(profile_id.clone(), ConnectionState::Pending(operation_id));
                self.active_operations.insert(
                    profile_id,
                    ActiveOperation {
                        operation_id,
                        profile_generation,
                        kind: OperationKind::ReconnectProfile,
                    },
                );
                self.model.status = "Reconnecting…".to_owned();
            }
            Err(error) => self.report_submit_error(error),
        }
    }

    fn submit_editor_intent(&mut self, intent: EditorIntent) {
        if self.model.is_config_uncertain() {
            self.model.status = "Reload profiles before executing.".to_owned();
            return;
        }
        match intent {
            EditorIntent::Execute(intent) => {
                let _ = self.submit_editor_execute(intent);
            }
            EditorIntent::ExecuteAll(intent) => {
                self.submit_editor_batch(intent);
            }
            EditorIntent::Cancel { operation_id } => {
                if !self
                    .model
                    .selected_workspace()
                    .is_some_and(|workspace| workspace.pending_execute == Some(operation_id))
                {
                    self.model.status = "The pending execution is no longer current.".to_owned();
                    return;
                }
                match self
                    .port
                    .try_submit(UiCommand::CancelOperation { operation_id })
                {
                    Ok(()) => {
                        self.model.status = format!("Cancelling operation {}…", operation_id.0);
                    }
                    Err(error) => self.report_submit_error(error),
                }
            }
        }
    }

    fn submit_editor_batch(&mut self, intent: EditorExecuteBatchIntent) {
        let profile_id = intent.profile_id().clone();
        let profile_generation = intent.profile_generation();
        let operation_kind = intent.operation_kind();
        let target_count = intent.target_count();
        let history_source = intent.text().to_owned();
        let history_target = intent.run_target();
        let workspace_key = WorkspaceKey::new(profile_id.clone(), profile_generation);
        if target_count == 0 || operation_kind != OperationKind::ExecuteRead {
            self.model.status = "Run all accepts a non-empty read-only batch.".to_owned();
            return;
        }
        if self.model.active_generation(&profile_id) != Some(profile_generation) {
            self.model.status = "The selected profile generation is stale.".to_owned();
            return;
        }
        if self
            .model
            .workspace(&workspace_key)
            .is_some_and(|workspace| workspace.pending_execute.is_some())
        {
            self.model.status = "Execute is already pending".to_owned();
            return;
        }
        if self.active_operations.contains_key(&profile_id) {
            self.model.status = "Another operation is active for this connection".to_owned();
            return;
        }
        if let Some(profile) = self
            .model
            .profiles
            .iter()
            .find(|profile| profile.id == profile_id && profile.generation == profile_generation)
            .cloned()
        {
            let _ = self.ensure_workspace_persistence_binding(&workspace_key, &profile);
        }
        let operation_id = self.model.next_operation();
        let retention_plan = match self.prepare_workspace_history_reservation(
            &workspace_key,
            operation_id.0,
            history_source.clone(),
            history_target,
        ) {
            Ok(plan) => plan,
            Err(failure) => {
                self.report_retention_plan_failure(failure);
                return;
            }
        };
        let retention_local_only = retention_plan.as_ref().is_some_and(|plan| plan.local_only);
        match self.port.try_submit(intent.into_ui_command(operation_id)) {
            Ok(()) => {
                if let Some(plan) = retention_plan
                    && let Some(history_id) = self.apply_workspace_retention_plan(plan)
                {
                    self.capture_pending_workspace_history(
                        operation_id,
                        WorkspaceKey::new(profile_id.clone(), profile_generation),
                        history_id,
                        history_source,
                        history_target,
                    );
                }
                self.model.workspace_mut(workspace_key).pending_execute = Some(operation_id);
                self.active_operations.insert(
                    profile_id,
                    ActiveOperation {
                        operation_id,
                        profile_generation,
                        kind: operation_kind,
                    },
                );
                self.model.status = if retention_local_only {
                    format!(
                        "Run all: executing {target_count} targets; private history remains Unsaved (read-only)."
                    )
                } else {
                    format!("Run all: executing {target_count} targets…")
                };
            }
            Err(error) => self.report_submit_error(error),
        }
    }

    fn submit_editor_execute(&mut self, intent: EditorExecuteIntent) -> bool {
        let profile_id = intent.profile_id().clone();
        let profile_generation = intent.profile_generation();
        let operation_kind = intent.operation_kind();
        let history_source = intent.text().to_owned();
        let history_target = intent.run_target();
        let workspace_key = WorkspaceKey::new(profile_id.clone(), profile_generation);
        if self.model.active_generation(intent.profile_id()) != Some(profile_generation) {
            self.model.status = "The selected profile generation is stale.".to_owned();
            return false;
        }
        if self
            .model
            .workspace(&workspace_key)
            .is_some_and(|workspace| workspace.pending_execute.is_some())
        {
            self.model.status = "Execute is already pending".to_owned();
            return false;
        }
        if self.active_operations.contains_key(&profile_id) {
            self.model.status = "Another operation is active for this connection".to_owned();
            return false;
        }
        if let Some(profile) = self
            .model
            .profiles
            .iter()
            .find(|profile| profile.id == profile_id && profile.generation == profile_generation)
            .cloned()
        {
            let _ = self.ensure_workspace_persistence_binding(&workspace_key, &profile);
        }
        let operation_id = self.model.next_operation();
        let retention_plan = match self.prepare_workspace_history_reservation(
            &workspace_key,
            operation_id.0,
            history_source.clone(),
            history_target,
        ) {
            Ok(plan) => plan,
            Err(failure) => {
                self.report_retention_plan_failure(failure);
                return false;
            }
        };
        let retention_local_only = retention_plan.as_ref().is_some_and(|plan| plan.local_only);
        match self.port.try_submit(intent.into_ui_command(operation_id)) {
            Ok(()) => {
                if let Some(plan) = retention_plan
                    && let Some(history_id) = self.apply_workspace_retention_plan(plan)
                {
                    self.capture_pending_workspace_history(
                        operation_id,
                        workspace_key.clone(),
                        history_id,
                        history_source,
                        history_target,
                    );
                }
                self.model
                    .workspace_mut(workspace_key.clone())
                    .pending_execute = Some(operation_id);
                self.active_operations.insert(
                    profile_id,
                    ActiveOperation {
                        operation_id,
                        profile_generation,
                        kind: operation_kind,
                    },
                );
                self.model.status = if retention_local_only {
                    "Executing… private history remains Unsaved (read-only).".to_owned()
                } else {
                    "Executing…".to_owned()
                };
                true
            }
            Err(error) => {
                self.report_submit_error(error);
                false
            }
        }
    }

    fn report_submit_error(&mut self, error: SubmitError) {
        self.model.status = match error {
            SubmitError::Busy => "Service is busy; command was not submitted".to_owned(),
            SubmitError::Disconnected => "Service is unavailable".to_owned(),
        };
    }

    fn dispatch_error_recovery(
        &mut self,
        operation_id: OperationId,
        error: &PublicOperationError,
        action: RecoveryAction,
    ) {
        if !error.recovery.as_slice().contains(&action) {
            self.model.status = "That recovery action is not available.".to_owned();
            return;
        }
        if error.operation == OperationKind::ExecuteMutation
            && matches!(action, RecoveryAction::Retry(_))
        {
            self.model.status =
                "Data-changing execution is never retried automatically.".to_owned();
            return;
        }
        self.recovery_dispatch_context = Some(RecoveryDispatchContext {
            source_operation_id: operation_id,
            source_operation: error.operation,
            code: error.code,
        });
        let result = dispatch_recovery(action, self);
        self.recovery_dispatch_context = None;
        match result {
            Ok(()) => {}
            Err(never) => match never {},
        }
    }

    fn dispatch_recovery_command(&mut self, command: RecoveryCommand) {
        let context = self.recovery_dispatch_context;
        match command {
            RecoveryCommand::OpenCredentialEditor(profile_id) => {
                let Some(profile_generation) = self.model.active_generation(&profile_id) else {
                    self.model.status = "The selected profile is no longer available.".to_owned();
                    return;
                };
                let (source_operation, retry_recipe_id) =
                    context.map_or((OperationKind::ConnectProfile, None), |context| {
                        let recipe_id = OperationRecipeId(context.source_operation_id.0);
                        let retry = self.retry_recipes.get(recipe_id).and_then(|recipe| {
                            (recipe.operation_kind() == context.source_operation
                                && recipe.profile_id() == &profile_id
                                && recipe.profile_generation() == profile_generation)
                                .then_some(recipe_id)
                        });
                        (context.source_operation, retry)
                    });
                self.open_credential_prompt_for(
                    profile_id,
                    profile_generation,
                    source_operation,
                    retry_recipe_id,
                );
            }
            RecoveryCommand::FocusDraftField(draft_id, field) => {
                if let Some(editor) = self.profile_editor.as_mut()
                    && editor.draft_id() == draft_id
                {
                    editor.request_focus(field);
                } else {
                    self.model.status = "That draft is no longer open.".to_owned();
                }
            }
            RecoveryCommand::FocusProfileField(profile_id, field) => {
                self.open_profile_editor_at(&profile_id, field);
            }
            RecoveryCommand::RetryRecipe(recipe_id) => {
                self.retry_recipe(recipe_id, context.map(|value| value.source_operation));
            }
            RecoveryCommand::FocusStatementEditor(profile_id) => {
                if self.model.active_generation(&profile_id).is_some() {
                    self.model.selected_profile = Some(profile_id);
                    self.editor_surface.request_focus(EDITOR_INPUT_ID);
                }
            }
            RecoveryCommand::FocusExecutionLimits(profile_id) => {
                if self.model.active_generation(&profile_id).is_some() {
                    self.model.selected_profile = Some(profile_id);
                    let control_id = match context.map(|value| value.code) {
                        Some(PublicCode::TimeoutInput) => EDITOR_TIMEOUT_ID,
                        Some(PublicCode::RowLimit) | None => EDITOR_ROW_LIMIT_ID,
                        Some(_) => EDITOR_ROW_LIMIT_ID,
                    };
                    self.editor_surface.request_focus(control_id);
                }
            }
            RecoveryCommand::ReloadConfiguredPath => self.submit_refresh(),
            RecoveryCommand::ReconnectProfile(profile_id) => self.submit_reconnect(profile_id),
            RecoveryCommand::CancelRunningOperation(operation_id) => {
                match self
                    .port
                    .try_submit(UiCommand::CancelOperation { operation_id })
                {
                    Ok(()) => {
                        self.model.status = format!("Cancelling operation {}…", operation_id.0);
                    }
                    Err(error) => self.report_submit_error(error),
                }
            }
            RecoveryCommand::ClearProfileCatalog(profile_id) => {
                if let Some(generation) = self.model.active_generation(&profile_id) {
                    if let Some(explorer) = self
                        .mysql_explorers
                        .get_mut(&(profile_id.clone(), generation))
                    {
                        explorer.clear();
                    }
                    let workspace = self
                        .model
                        .workspace_mut(super::model::WorkspaceKey::new(profile_id, generation));
                    workspace.catalog_page = None;
                    workspace.catalog_retry = None;
                    workspace.catalog_error = None;
                    self.model.status = "Catalog cleared.".to_owned();
                }
            }
            RecoveryCommand::RestartProfileRedisScan(profile_id) => {
                let request = self
                    .model
                    .active_generation(&profile_id)
                    .and_then(|generation| {
                        self.model
                            .workspace(&super::model::WorkspaceKey::new(
                                profile_id.clone(),
                                generation,
                            ))
                            .and_then(|workspace| workspace.redis_scan_retry.clone())
                    });
                if let Some(mut request) = request {
                    request.cursor = 0;
                    self.retry_redis_scan(request, true);
                } else {
                    self.model.status = "That Redis scan is no longer available.".to_owned();
                }
            }
            RecoveryCommand::ChooseResultExportDestination(result_id) => {
                let Some(snapshot) = self.result_snapshot(result_id) else {
                    self.model.status = "That result is no longer available.".to_owned();
                    return;
                };
                let format = self
                    .result_export_formats
                    .get(&result_id)
                    .copied()
                    .unwrap_or(ExportFormat::Json);
                self.choose_result_export_destination(snapshot, format);
            }
            RecoveryCommand::RevealResultExportDestination(result_id) => {
                self.reveal_result_export_destination(result_id);
            }
            RecoveryCommand::RevealConfiguredMigrationBackup => {
                self.model.status = "Migration backup reveal requested.".to_owned();
            }
            RecoveryCommand::RestartApplication => {
                let operation_id = self.model.next_operation();
                match self
                    .port
                    .try_submit(UiCommand::ShutdownRuntime { operation_id })
                {
                    Ok(()) => self.model.status = "Restart requested; shutting down…".to_owned(),
                    Err(error) => self.report_submit_error(error),
                }
            }
            RecoveryCommand::DismissOperationError(operation_id) => {
                self.dismiss_operation_error(operation_id);
            }
        }
    }

    fn dismiss_operation_error(&mut self, operation_id: OperationId) {
        self.retry_recipes.remove(OperationRecipeId(operation_id.0));
        if self
            .common_error
            .as_ref()
            .is_some_and(|visible| visible.operation_id == operation_id)
        {
            self.common_error = None;
        }
        let mut redis_workspaces = Vec::new();
        for (key, workspace) in &mut self.model.workspaces {
            if workspace
                .catalog_retry
                .as_ref()
                .is_some_and(|request| request.operation_id() == operation_id)
            {
                workspace.catalog_error = None;
            }
            if workspace
                .redis_scan_retry
                .as_ref()
                .is_some_and(|request| request.operation_id() == operation_id)
            {
                workspace.redis_scan_error = None;
                redis_workspaces.push(key.clone());
            }
            if workspace
                .redis_inspect_retry
                .as_ref()
                .is_some_and(|request| request.operation_id() == operation_id)
            {
                workspace.redis_inspect_error = None;
                if !redis_workspaces.contains(key) {
                    redis_workspaces.push(key.clone());
                }
            }
        }
        for explorer in self.mysql_explorers.values_mut() {
            explorer.dismiss_error();
        }
        for key in redis_workspaces {
            if let Some(explorer) = self.redis_explorers.get_mut(&key) {
                explorer.dismiss_errors();
            }
        }
        self.model.status = "Error dismissed.".to_owned();
    }

    fn submit_mysql_explorer_intent(
        &mut self,
        profile: &ProfileSnapshot,
        intent: MySqlExplorerIntent,
    ) {
        let intent = match intent {
            MySqlExplorerIntent::NewEditor { schema, relation } => {
                let title = mysql_context_editor_title(&schema, &relation);
                let key = super::model::WorkspaceKey::new(profile.id.clone(), profile.generation);
                match self.model.workspace_mut(key).create_editor_tab(
                    profile.driver.language(),
                    title,
                    "",
                ) {
                    Ok(_) => {
                        self.editor_surface = EditorSurface::default();
                        self.model.status = format!("New editor opened for {schema}.{relation}.");
                    }
                    Err(error) => self.model.status = error.to_string(),
                }
                return;
            }
            MySqlExplorerIntent::InsertTemplate(template) => {
                let key = super::model::WorkspaceKey::new(profile.id.clone(), profile.generation);
                let outcome = self.model.workspace_mut(key).create_editor_tab(
                    profile.driver.language(),
                    "Data query",
                    template,
                );
                match outcome {
                    Ok(_) => {
                        self.editor_surface = EditorSurface::default();
                        let key =
                            super::model::WorkspaceKey::new(profile.id.clone(), profile.generation);
                        let intent = self.model.workspace(&key).map(|workspace| {
                            let character_count = workspace.editor_text.chars().count();
                            build_execute_intent(
                                profile,
                                workspace,
                                EditorCursor::with_selection(character_count, 0..character_count),
                            )
                        });
                        match intent {
                            Some(Ok(intent)) => {
                                self.submit_editor_intent(EditorIntent::Execute(intent));
                            }
                            Some(Err(error)) => self.model.status = error.to_string(),
                            None => {
                                self.model.status =
                                    "The bounded table query could not be submitted.".to_owned();
                            }
                        }
                    }
                    Err(error) => self.model.status = error.to_string(),
                }
                return;
            }
            intent => intent,
        };
        if self.model.is_config_uncertain() {
            self.model.status = "Reload profiles before browsing the catalog.".to_owned();
            return;
        }
        if self.active_operations.contains_key(&profile.id) {
            self.model.status = "Another operation is active for this connection".to_owned();
            return;
        }
        let operation_id = self.model.next_operation();
        let identity = RequestIdentity::new(profile.id.clone(), profile.generation, operation_id);
        let request = match intent {
            MySqlExplorerIntent::RefreshSchemas { prefix } => CatalogRequest::Schemas {
                identity,
                prefix,
                page_token: None,
                page_size: DEFAULT_CATALOG_PAGE_SIZE,
                timeout: DEFAULT_CATALOG_TIMEOUT,
            },
            MySqlExplorerIntent::LoadMore(request) => {
                catalog_request_with_identity(request, identity)
            }
            MySqlExplorerIntent::LoadRelations {
                schema,
                prefix,
                token,
            } => CatalogRequest::Relations {
                identity,
                schema,
                prefix,
                page_token: token,
                page_size: DEFAULT_CATALOG_PAGE_SIZE,
                timeout: DEFAULT_CATALOG_TIMEOUT,
            },
            MySqlExplorerIntent::LoadColumns {
                schema,
                relation,
                prefix,
                token,
            } => CatalogRequest::Columns {
                identity,
                schema,
                relation,
                prefix,
                page_token: token,
                page_size: DEFAULT_CATALOG_PAGE_SIZE,
                timeout: DEFAULT_CATALOG_TIMEOUT,
            },
            MySqlExplorerIntent::Retry(request) => catalog_request_with_identity(request, identity),
            MySqlExplorerIntent::NewEditor { .. } | MySqlExplorerIntent::InsertTemplate(_) => {
                return;
            }
        };
        match self
            .port
            .try_submit(UiCommand::BrowseCatalog(request.clone()))
        {
            Ok(()) => {
                self.retry_recipes
                    .register(operation_id, RetryRecipe::Catalog(request.clone()));
                self.mysql_explorers
                    .entry((profile.id.clone(), profile.generation))
                    .or_default()
                    .mark_submitted(request);
                self.active_operations.insert(
                    profile.id.clone(),
                    ActiveOperation {
                        operation_id,
                        profile_generation: profile.generation,
                        kind: OperationKind::BrowseMySql,
                    },
                );
                self.model.status = "Loading MySQL catalog page…".to_owned();
            }
            Err(error) => self.report_submit_error(error),
        }
    }

    fn submit_redis_intent(&mut self, intent: RedisExplorerIntent) {
        let Some(key) = self.visible_redis_workspace.clone() else {
            self.model.status = "Redis explorer context is unavailable.".to_owned();
            return;
        };
        if self.model.selected_workspace_key().as_ref() != Some(&key) {
            self.redis_explorer_mut(&key)
                .submission_failed("Redis explorer context changed; retry from the visible panel.");
            self.model.status =
                "Redis explorer context changed; no command was submitted.".to_owned();
            return;
        }
        self.submit_redis_intent_for(&key, intent);
    }

    fn submit_redis_intent_for(&mut self, key: &WorkspaceKey, intent: RedisExplorerIntent) {
        if let RedisExplorerIntent::Cancel { operation_id } = &intent {
            let operation_id = *operation_id;
            if !self
                .active_operations
                .get(&key.profile_id)
                .is_some_and(|active| {
                    active.operation_id == operation_id
                        && active.profile_generation == key.profile_generation
                        && matches!(
                            active.kind,
                            OperationKind::BrowseRedis | OperationKind::InspectRedis
                        )
                })
            {
                self.redis_explorer_mut(key)
                    .submission_failed("That Redis operation is no longer active.");
                return;
            }
            match self
                .port
                .try_submit(UiCommand::CancelOperation { operation_id })
            {
                Ok(()) => {
                    self.redis_explorer_mut(key).cancel_submitted(operation_id);
                    self.model.status = "Cancelling Redis operation…".to_owned();
                }
                Err(error) => {
                    self.redis_explorer_mut(key)
                        .submission_failed(submit_error_message(error));
                    self.report_submit_error(error);
                }
            }
            return;
        }
        if self.model.is_config_uncertain() {
            self.redis_explorer_mut(key)
                .submission_failed("Reload profiles before browsing Redis.");
            return;
        }
        if self.model.active_generation(&key.profile_id) != Some(key.profile_generation) {
            self.redis_explorer_mut(key)
                .submission_failed("That Redis workspace is no longer current.");
            return;
        }
        let Some(profile) = self
            .model
            .profiles
            .iter()
            .find(|profile| {
                profile.id == key.profile_id && profile.generation == key.profile_generation
            })
            .cloned()
        else {
            self.redis_explorer_mut(key)
                .submission_failed("That Redis workspace is no longer current.");
            return;
        };
        let keyspace_ready = profile.driver == DriverKind::Redis
            && profile.is_ready()
            && crate::drivers::descriptors()
                .into_iter()
                .find(|descriptor| descriptor.kind == DriverKind::Redis)
                .is_some_and(|descriptor| {
                    descriptor
                        .capabilities
                        .contains(DriverCapabilities::KEYSPACE_BROWSE)
                });
        if !keyspace_ready {
            self.redis_explorer_mut(key)
                .submission_failed("Redis keyspace browsing is unavailable.");
            return;
        }
        if self.active_operations.contains_key(&profile.id) {
            self.redis_explorer_mut(key)
                .submission_failed("Another operation is active for this connection.");
            return;
        }
        let operation_id = self.model.next_operation();
        let identity = RequestIdentity::new(profile.id.clone(), profile.generation, operation_id);
        match intent {
            RedisExplorerIntent::Scan {
                filter,
                cursor,
                restart,
            } => {
                let request = RedisScanRequest {
                    identity,
                    filter: filter.clone(),
                    cursor,
                    count_hint: DEFAULT_REDIS_SCAN_COUNT,
                    timeout: Duration::from_secs(5),
                };
                match self
                    .port
                    .try_submit(UiCommand::ScanRedisKeys(request.clone()))
                {
                    Ok(()) => {
                        self.retry_recipes.register(
                            operation_id,
                            RetryRecipe::RedisScan {
                                request: request.clone(),
                                restart,
                            },
                        );
                        self.redis_explorer_mut(key).begin_scan(
                            operation_id,
                            filter,
                            cursor,
                            restart,
                        );
                        self.active_operations.insert(
                            profile.id.clone(),
                            ActiveOperation {
                                operation_id,
                                profile_generation: profile.generation,
                                kind: OperationKind::BrowseRedis,
                            },
                        );
                        self.model.status = "Scanning Redis keys…".to_owned();
                    }
                    Err(error) => {
                        self.redis_explorer_mut(key)
                            .submission_failed(submit_error_message(error));
                        self.report_submit_error(error);
                    }
                }
            }
            RedisExplorerIntent::Inspect { key: redis_key } => {
                let request = RedisKeyInspectRequest {
                    identity,
                    key: redis_key.clone(),
                    timeout: Duration::from_secs(5),
                };
                match self
                    .port
                    .try_submit(UiCommand::InspectRedisKey(request.clone()))
                {
                    Ok(()) => {
                        self.retry_recipes
                            .register(operation_id, RetryRecipe::RedisInspect(request.clone()));
                        self.redis_explorer_mut(key)
                            .begin_inspect(operation_id, redis_key);
                        self.active_operations.insert(
                            profile.id.clone(),
                            ActiveOperation {
                                operation_id,
                                profile_generation: profile.generation,
                                kind: OperationKind::InspectRedis,
                            },
                        );
                        self.model.status = "Inspecting Redis key…".to_owned();
                    }
                    Err(error) => {
                        self.redis_explorer_mut(key)
                            .submission_failed(submit_error_message(error));
                        self.report_submit_error(error);
                    }
                }
            }
            RedisExplorerIntent::Cancel { .. } => unreachable!("handled above"),
        }
    }

    fn retry_catalog_request(&mut self, request: CatalogRequest) {
        let profile_id = request.profile_id().clone();
        let profile_generation = request.profile_generation();
        if self.active_operations.contains_key(&profile_id) {
            self.model.status = "Another operation is active for this connection".to_owned();
            return;
        }
        let operation_id = self.model.next_operation();
        let request = catalog_request_with_identity(
            request,
            RequestIdentity::new(profile_id.clone(), profile_generation, operation_id),
        );
        match self
            .port
            .try_submit(UiCommand::BrowseCatalog(request.clone()))
        {
            Ok(()) => {
                self.retry_recipes
                    .register(operation_id, RetryRecipe::Catalog(request.clone()));
                self.mysql_explorers
                    .entry((profile_id.clone(), profile_generation))
                    .or_default()
                    .mark_submitted(request);
                self.active_operations.insert(
                    profile_id,
                    ActiveOperation {
                        operation_id,
                        profile_generation,
                        kind: OperationKind::BrowseMySql,
                    },
                );
                self.model.status = "Retrying MySQL catalog page…".to_owned();
            }
            Err(error) => self.report_submit_error(error),
        }
    }

    fn retry_redis_scan(&mut self, request: RedisScanRequest, restart: bool) {
        let profile_id = request.profile_id().clone();
        let profile_generation = request.profile_generation();
        let workspace_key = WorkspaceKey::new(profile_id.clone(), profile_generation);
        if self.active_operations.contains_key(&profile_id) {
            self.model.status = "Another operation is active for this connection".to_owned();
            return;
        }
        let operation_id = self.model.next_operation();
        let request = RedisScanRequest {
            identity: RequestIdentity::new(profile_id.clone(), profile_generation, operation_id),
            filter: request.filter,
            cursor: request.cursor,
            count_hint: request.count_hint,
            timeout: request.timeout,
        };
        match self
            .port
            .try_submit(UiCommand::ScanRedisKeys(request.clone()))
        {
            Ok(()) => {
                self.retry_recipes.register(
                    operation_id,
                    RetryRecipe::RedisScan {
                        request: request.clone(),
                        restart,
                    },
                );
                self.redis_explorer_mut(&workspace_key).begin_scan(
                    operation_id,
                    request.filter.clone(),
                    request.cursor,
                    restart,
                );
                self.active_operations.insert(
                    profile_id,
                    ActiveOperation {
                        operation_id,
                        profile_generation,
                        kind: OperationKind::BrowseRedis,
                    },
                );
                self.model.status = "Retrying Redis scan…".to_owned();
            }
            Err(error) => {
                self.redis_explorer_mut(&workspace_key)
                    .submission_failed(submit_error_message(error));
                self.report_submit_error(error);
            }
        }
    }

    fn retry_redis_inspect(&mut self, request: RedisKeyInspectRequest) {
        let profile_id = request.profile_id().clone();
        let profile_generation = request.profile_generation();
        let workspace_key = WorkspaceKey::new(profile_id.clone(), profile_generation);
        if self.active_operations.contains_key(&profile_id) {
            self.model.status = "Another operation is active for this connection".to_owned();
            return;
        }
        let operation_id = self.model.next_operation();
        let request = RedisKeyInspectRequest {
            identity: RequestIdentity::new(profile_id.clone(), profile_generation, operation_id),
            key: request.key,
            timeout: request.timeout,
        };
        match self
            .port
            .try_submit(UiCommand::InspectRedisKey(request.clone()))
        {
            Ok(()) => {
                self.retry_recipes
                    .register(operation_id, RetryRecipe::RedisInspect(request.clone()));
                self.redis_explorer_mut(&workspace_key)
                    .begin_inspect(operation_id, request.key.clone());
                self.active_operations.insert(
                    profile_id,
                    ActiveOperation {
                        operation_id,
                        profile_generation,
                        kind: OperationKind::InspectRedis,
                    },
                );
                self.model.status = "Retrying Redis inspection…".to_owned();
            }
            Err(error) => {
                self.redis_explorer_mut(&workspace_key)
                    .submission_failed(submit_error_message(error));
                self.report_submit_error(error);
            }
        }
    }

    fn open_profile_editor_at(&mut self, profile_id: &ProfileId, field: ProfileFieldId) {
        let Some(profile) = self
            .model
            .profiles
            .iter()
            .find(|profile| &profile.id == profile_id)
            .cloned()
        else {
            self.model.status = "The Redis profile is no longer available.".to_owned();
            return;
        };
        let draft_id = self.allocate_draft_id();
        let mut editor = self.configured_profile_editor(ProfileEditor::edit(
            draft_id,
            &profile.persisted,
            profile.generation,
            profile.has_current_session_secret,
        ));
        editor.request_focus(field);
        self.profile_editor = Some(editor);
    }

    fn open_delete_confirmation(&mut self, profile: &ProfileSnapshot) {
        if self.model.is_config_uncertain() {
            self.model.status = "Reload profiles before deleting.".to_owned();
            return;
        }
        if self.model.active_generation(&profile.id) != Some(profile.generation) {
            self.model.status = "The selected profile generation is stale.".to_owned();
            return;
        }
        self.delete_confirmation = Some(DeleteConfirmation {
            profile_id: profile.id.clone(),
            profile_generation: profile.generation,
            profile_name: profile.name.clone(),
            active_kind: self
                .active_operations
                .get(&profile.id)
                .map(|active| active.kind),
            migration_backup: self.model.config.migration_backup().map(PathBuf::from),
            migration_confirmed: false,
        });
    }

    fn cancel_delete_confirmation(&mut self) {
        self.delete_confirmation = None;
    }

    fn confirm_delete_confirmation(&mut self) {
        let Some(confirmation) = self.delete_confirmation.as_ref() else {
            return;
        };
        if self.retention_commit_barrier.is_pending_or_failed() {
            self.model.status =
                "Profile delete waits for the global private-history save barrier.".to_owned();
            return;
        }
        if self.pending_workspace_history.values().any(|pending| {
            pending.workspace_key.profile_id == confirmation.profile_id
                && pending.workspace_key.profile_generation == confirmation.profile_generation
        }) {
            self.model.status =
                "Profile delete waits for active execution history to settle; cancel or wait."
                    .to_owned();
            return;
        }
        if self.model.active_generation(&confirmation.profile_id)
            != Some(confirmation.profile_generation)
        {
            self.delete_confirmation = None;
            self.model.status = "The selected profile generation is stale.".to_owned();
            return;
        }
        let profile_id = confirmation.profile_id.clone();
        let profile_generation = confirmation.profile_generation;
        let migration_consent = MigrationConsent::from_confirmation(
            confirmation.migration_backup.is_some() && confirmation.migration_confirmed,
        );
        let operation_id = self.model.next_operation();
        let request = DeleteProfileRequest {
            profile_id: profile_id.clone(),
            expected_generation: profile_generation,
            operation_id,
            migration_consent,
        };
        match self.port.try_submit(UiCommand::DeleteProfile(request)) {
            Ok(()) => {
                self.delete_confirmation = None;
                self.model
                    .connection_states
                    .insert(profile_id.clone(), ConnectionState::Closing);
                let prior_active = self.active_operations.insert(
                    profile_id.clone(),
                    ActiveOperation {
                        operation_id,
                        profile_generation,
                        kind: OperationKind::DeleteProfile,
                    },
                );
                self.pending_deletes.insert(
                    profile_id,
                    PendingDelete {
                        operation_id,
                        profile_generation,
                        prior_active,
                        prior_finished: false,
                    },
                );
                self.model.status = "Deleting profile…".to_owned();
            }
            Err(error) => self.report_submit_error(error),
        }
    }

    fn show_delete_confirmation(&mut self, root_ui: &mut egui::Ui, request_focus: bool) {
        let Some(confirmation) = self.delete_confirmation.as_mut() else {
            return;
        };
        let mut action = None;
        egui::Window::new("Delete connection")
            .collapsible(false)
            .resizable(false)
            .show(root_ui.ctx(), |ui| {
                ui.heading(format!("Delete {}?", confirmation.profile_name));
                ui.label("This removes the saved profile and its in-memory session credential.");
                if let Some(kind) = confirmation.active_kind {
                    let warning = format!(
                        "{kind:?} is active. Dbotter will stop waiting; the server operation may continue."
                    );
                    named_author_id_with_label(
                        ui.strong(warning.clone()),
                        "profile.delete.active_warning",
                        warning,
                    );
                    ui.strong("After confirmed deletion, server state will be reported as Unknown.");
                }
                if let Some(backup) = confirmation.migration_backup.as_deref() {
                    let migration = ui.checkbox(
                        &mut confirmation.migration_confirmed,
                        "Allow the version-1 configuration migration",
                    );
                    named_author_id(
                        migration,
                        "profile.delete.migration_confirm",
                        "Confirm delete configuration migration backup",
                    );
                    let backup = backup.to_string_lossy().into_owned();
                    let response = ui.label(format!("Backup: {backup}"));
                    named_dynamic_value_author_id(
                        response,
                        "profile.delete.migration_backup".to_owned(),
                        "Delete migration backup path".to_owned(),
                        backup,
                    );
                }
                ui.horizontal(|ui| {
                    let cancel = ui.add_sized(
                        [104.0, OpenAiTheme::MIN_CONTROL_HEIGHT],
                        egui::Button::new("Cancel"),
                    );
                    let cancel = named_author_id(
                        cancel,
                        "profile.delete.cancel",
                        "Cancel profile deletion",
                    );
                    if request_focus {
                        cancel.request_focus();
                    }
                    if cancel.clicked() {
                        action = Some(DeleteDialogAction::Cancel);
                    }
                    let confirm = ui.add(
                        egui::Button::new(
                            egui::RichText::new("Delete profile").color(egui::Color32::WHITE),
                        )
                        .fill(egui::Color32::BLACK)
                        .min_size(egui::vec2(144.0, OpenAiTheme::MIN_CONTROL_HEIGHT)),
                    );
                    if named_author_id(
                        confirm,
                        "profile.delete.confirm",
                        "Confirm profile deletion",
                    )
                    .clicked()
                    {
                        action = Some(DeleteDialogAction::Confirm);
                    }
                });
            });
        match action {
            Some(DeleteDialogAction::Cancel) => self.cancel_delete_confirmation(),
            Some(DeleteDialogAction::Confirm) => self.confirm_delete_confirmation(),
            None => {}
        }
    }

    fn request_editor_tab_close(
        &mut self,
        workspace_key: WorkspaceKey,
        tab_id: EditorTabId,
        discard_author_id: &'static str,
    ) {
        let outcome = self
            .model
            .workspace_mut(workspace_key.clone())
            .close_editor_tab(tab_id);
        match outcome {
            Ok(()) => {
                self.editor_surface = EditorSurface::default();
                self.model.status = "Query tab closed".to_owned();
            }
            Err(EditorTabError::Dirty) => {
                let title = self
                    .model
                    .workspace(&workspace_key)
                    .and_then(|workspace| workspace.editor_tab(tab_id))
                    .map_or_else(|| "query tab".to_owned(), |tab| tab.title().to_owned());
                self.editor_discard_confirmation = Some(EditorDiscardConfirmation {
                    workspace_key,
                    tab_id,
                    title,
                    discard_author_id,
                });
                self.model.status = "Unsaved query requires discard confirmation".to_owned();
            }
            Err(error) => self.model.status = error.to_string(),
        }
    }

    fn show_editor_discard_confirmation(&mut self, root_ui: &mut egui::Ui, request_focus: bool) {
        let Some(confirmation) = self.editor_discard_confirmation.as_ref() else {
            return;
        };
        let title = confirmation.title.clone();
        let discard_author_id = confirmation.discard_author_id;
        let mut cancel =
            root_ui.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
        let mut discard = false;
        egui::Window::new("Discard unsaved query?")
            .collapsible(false)
            .resizable(false)
            .show(root_ui.ctx(), |ui| {
                ui.heading("Discard unsaved query tab?");
                ui.label(format!(
                    "{title} has changes that exist only in this session."
                ));
                ui.label("Discard permanently removes this tab's query text.");
                ui.horizontal(|ui| {
                    let keep = ui.add_sized(
                        [104.0, OpenAiTheme::MIN_CONTROL_HEIGHT],
                        egui::Button::new("Keep tab"),
                    );
                    let keep = named_author_id(
                        keep,
                        "editor.tab.discard.cancel",
                        "Keep unsaved query tab",
                    );
                    if request_focus {
                        keep.request_focus();
                    }
                    if keep.clicked() {
                        cancel = true;
                    }
                    let confirm = ui.add(
                        egui::Button::new(
                            egui::RichText::new("Discard tab").color(egui::Color32::WHITE),
                        )
                        .fill(egui::Color32::BLACK)
                        .min_size(egui::vec2(128.0, OpenAiTheme::MIN_CONTROL_HEIGHT)),
                    );
                    if named_author_id(confirm, discard_author_id, "Discard unsaved query tab")
                        .clicked()
                    {
                        discard = true;
                    }
                });
            });
        if cancel {
            self.editor_discard_confirmation = None;
            self.model.status = "Unsaved query tab kept".to_owned();
        } else if discard {
            let confirmation = self.editor_discard_confirmation.take();
            if let Some(confirmation) = confirmation {
                let outcome = self
                    .model
                    .workspace_mut(confirmation.workspace_key)
                    .discard_editor_tab(confirmation.tab_id);
                match outcome {
                    Ok(()) => {
                        self.editor_surface = EditorSurface::default();
                        self.model.status = "Unsaved query tab discarded".to_owned();
                    }
                    Err(error) => self.model.status = error.to_string(),
                }
            }
        }
    }

    fn show_credential_prompt(&mut self, root_ui: &mut egui::Ui, request_focus: bool) {
        let Some(prompt) = self.credential_prompt.as_mut() else {
            return;
        };
        let pending = prompt.store_operation_id.is_some();
        let mut cancel = false;
        let mut submit = false;
        egui::Window::new("Session credential")
            .collapsible(false)
            .resizable(false)
            .show(root_ui.ctx(), |ui| {
                ui.set_min_width(360.0);
                ui.heading("Enter credential");
                ui.label("Stored only in this running app session.");
                ui.add_space(12.0);
                ui.label("Credential");
                let credential = ui.add_enabled(
                    !pending,
                    egui::TextEdit::singleline(prompt.secret.as_mut_string())
                        .id_salt("connection.credential.value")
                        .password(true)
                        .desired_width(f32::INFINITY),
                );
                let credential = named_author_id(
                    credential,
                    "connection.credential.value",
                    "Protected session credential",
                );
                if request_focus {
                    credential.request_focus();
                }
                ui.small(&prompt.status);
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    let cancel_button = ui.add_sized(
                        [104.0, OpenAiTheme::MIN_CONTROL_HEIGHT],
                        egui::Button::new("Cancel"),
                    );
                    cancel = named_author_id(
                        cancel_button,
                        "connection.credential.cancel",
                        "Cancel credential entry",
                    )
                    .clicked();
                    let store_button = ui.add_enabled(
                        !pending,
                        egui::Button::new(
                            egui::RichText::new("Store & continue").color(egui::Color32::WHITE),
                        )
                        .fill(egui::Color32::BLACK)
                        .min_size(egui::vec2(160.0, OpenAiTheme::MIN_CONTROL_HEIGHT)),
                    );
                    submit = named_author_id(
                        store_button,
                        "connection.credential.store",
                        "Store credential for this app session",
                    )
                    .clicked();
                });
            });
        if cancel {
            self.cancel_credential_prompt();
        } else if submit {
            self.submit_credential_prompt();
        }
    }

    fn show_workspace_clear_confirmation(&mut self, root_ui: &mut egui::Ui, request_focus: bool) {
        let Some(key) = self.workspace_clear_confirmation.clone() else {
            return;
        };
        let mut cancel =
            root_ui.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
        let mut confirm = false;
        egui::Window::new("Clear saved workspace data?")
            .collapsible(false)
            .resizable(false)
            .show(root_ui.ctx(), |ui| {
                ui.heading("Clear this profile's saved drafts and history?");
                ui.label(
                    "The profile and credentials are not deleted. Open editors remain visible in this session.",
                );
                ui.label(
                    "After the durable clear completes, persistence switches Off so autosave cannot recreate the data.",
                );
                ui.horizontal(|ui| {
                    let keep = ui.add_sized(
                        [104.0, OpenAiTheme::MIN_CONTROL_HEIGHT],
                        egui::Button::new("Cancel"),
                    );
                    let keep = named_author_id(
                        keep,
                        "workspace.persistence.clear.cancel",
                        "Cancel clearing saved workspace data",
                    );
                    if request_focus {
                        keep.request_focus();
                    }
                    if keep.clicked() {
                        cancel = true;
                    }
                    let clear = ui.add(
                        egui::Button::new(
                            egui::RichText::new("Clear saved data")
                                .color(egui::Color32::WHITE),
                        )
                        .fill(egui::Color32::BLACK)
                        .min_size(egui::vec2(152.0, OpenAiTheme::MIN_CONTROL_HEIGHT)),
                    );
                    confirm = named_author_id(
                        clear,
                        "workspace.persistence.clear.confirm",
                        "Confirm clearing saved drafts and history",
                    )
                    .clicked();
                });
            });
        if cancel {
            self.workspace_clear_confirmation = None;
            self.model.status = "Saved workspace data was kept.".to_owned();
        } else if confirm {
            self.workspace_clear_confirmation = None;
            self.submit_clear_workspace(&key);
        }
    }

    fn show_workspace_restore_conflict_confirmation(
        &mut self,
        root_ui: &mut egui::Ui,
        request_focus: bool,
    ) {
        let Some(key) = self.workspace_restore_conflict_confirmation.clone() else {
            return;
        };
        let still_conflicted = self
            .workspace_persistence
            .get(&key)
            .is_some_and(|state| matches!(state.load, WorkspaceLoadPhase::Conflict));
        if !still_conflicted {
            self.workspace_restore_conflict_confirmation = None;
            self.close_after_restore_conflict_confirmation = false;
            return;
        }
        let mut cancel =
            root_ui.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
        let mut confirm = false;
        egui::Window::new("Replace prior saved workspace?")
            .collapsible(false)
            .resizable(false)
            .show(root_ui.ctx(), |ui| {
                ui.heading("Keep this local workspace instead?");
                ui.label(
                    "This replaces the prior durable tabs and private history that could not be restored.",
                );
                ui.label("Cancel keeps both the local draft in memory and prior saved data unchanged.");
                ui.horizontal(|ui| {
                    let cancel_button = ui.add_sized(
                        [112.0, OpenAiTheme::MIN_CONTROL_HEIGHT],
                        egui::Button::new("Cancel"),
                    );
                    let cancel_response = named_author_id(
                        cancel_button,
                        "workspace.conflict.cancel",
                        "Cancel replacing the prior saved workspace",
                    );
                    if request_focus {
                        cancel_response.request_focus();
                    }
                    cancel = cancel_response.clicked();
                    let confirm_button = ui.add(
                        egui::Button::new(
                            egui::RichText::new("Replace saved with local")
                                .color(egui::Color32::WHITE),
                        )
                        .fill(egui::Color32::BLACK)
                        .min_size(egui::vec2(208.0, OpenAiTheme::MIN_CONTROL_HEIGHT)),
                    );
                    confirm = named_author_id(
                        confirm_button,
                        "workspace.conflict.confirm",
                        "Replace prior saved tabs and history with the local workspace",
                    )
                    .clicked();
                });
            });
        if cancel {
            self.workspace_restore_conflict_confirmation = None;
            self.close_after_restore_conflict_confirmation = false;
            self.workspace_close_guard = WorkspaceCloseGuard::Closed;
            self.model.status =
                "Restore conflict kept unresolved; no saved data changed.".to_owned();
        } else if confirm {
            self.workspace_restore_conflict_confirmation = None;
            let close_after = std::mem::take(&mut self.close_after_restore_conflict_confirmation);
            if self.submit_conflict_resolution_commit(&key) && close_after {
                self.workspace_close_guard = WorkspaceCloseGuard::AwaitingSave;
            }
        }
    }

    fn show_workspace_close_guard(&mut self, root_ui: &mut egui::Ui, request_focus: bool) {
        if self.workspace_close_guard == WorkspaceCloseGuard::Closed {
            return;
        }
        let failed = self.workspace_close_guard == WorkspaceCloseGuard::SaveFailed;
        let clear_failed = self
            .workspace_persistence
            .values()
            .any(|state| matches!(state.clear, WorkspaceClearPhase::Failed { .. }));
        let restore_conflict = self
            .workspace_persistence
            .values()
            .any(|state| matches!(state.load, WorkspaceLoadPhase::Conflict));
        let local_dirty = self.workspace_persistence.keys().any(|key| {
            self.model
                .workspace(key)
                .is_some_and(|workspace| !workspace.is_saved())
        });
        let mut retry = false;
        let mut discard = false;
        let mut cancel =
            root_ui.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
        egui::Window::new(if failed {
            "Private workspace save failed"
        } else {
            "Saving before close"
        })
        .collapsible(false)
        .resizable(false)
        .show(root_ui.ctx(), |ui| {
            if failed {
                if restore_conflict {
                    ui.heading("Restored data conflicts with local changes.");
                    ui.label(
                        "Review Keep local, use Persistence Off or Clear saved data, or explicitly discard local changes and close.",
                    );
                } else if clear_failed && local_dirty {
                    ui.heading("Saved data was not cleared and local changes are unsaved.");
                    ui.label(
                        "Closing now keeps the durable data, abandons the clear request, and discards local changes.",
                    );
                } else if clear_failed {
                    ui.heading("Saved private data was not cleared.");
                    ui.label(
                        "Retry clearing, or keep the durable data and close. Closing now abandons the clear request.",
                    );
                } else {
                    ui.heading("Local workspace changes are not durably saved.");
                    ui.label(
                        "Retry saving, explicitly discard local changes and close, or cancel closing.",
                    );
                }
            } else {
                ui.heading("Saving private workspace before close…");
                ui.label("The window will close only after the exact revision is durable.");
                ui.add(egui::Spinner::new());
            }
            ui.horizontal_wrapped(|ui| {
                if failed {
                    let retry_button = ui.add_sized(
                        [104.0, OpenAiTheme::MIN_CONTROL_HEIGHT],
                        egui::Button::new(if restore_conflict {
                            "Keep local…"
                        } else {
                            "Retry"
                        }),
                    );
                    retry = named_author_id(
                        retry_button,
                        "workspace.close.retry",
                        if restore_conflict {
                            "Review replacing prior saved data with the local workspace"
                        } else {
                            "Retry saving before close"
                        },
                    )
                    .clicked();
                    let discard_button = ui.add(
                        egui::Button::new(
                            egui::RichText::new(if restore_conflict {
                                "Discard local & close"
                            } else if clear_failed && local_dirty {
                                "Keep saved data, discard local & close"
                            } else if clear_failed {
                                "Keep saved data & close"
                            } else {
                                "Discard local changes"
                            })
                                .color(egui::Color32::WHITE),
                        )
                        .fill(egui::Color32::BLACK)
                        .min_size(egui::vec2(176.0, OpenAiTheme::MIN_CONTROL_HEIGHT)),
                    );
                    discard = named_author_id(
                        discard_button,
                        "workspace.close.discard",
                        if restore_conflict {
                            "Discard local changes, preserve the prior durable workspace, and close"
                        } else if clear_failed && local_dirty {
                            "Keep durable saved data, abandon clearing, discard local changes, and close"
                        } else if clear_failed {
                            "Keep durable saved data, abandon clearing, and close"
                        } else {
                            "Discard local changes and close"
                        },
                    )
                    .clicked();
                }
                let cancel_button = ui.add_sized(
                    [112.0, OpenAiTheme::MIN_CONTROL_HEIGHT],
                    egui::Button::new("Cancel close"),
                );
                let cancel_button = named_author_id(
                    cancel_button,
                    "workspace.close.cancel",
                    "Cancel closing the application",
                );
                if request_focus {
                    cancel_button.request_focus();
                }
                if cancel_button.clicked() {
                    cancel = true;
                }
            });
        });
        if retry && restore_conflict {
            self.workspace_close_guard = WorkspaceCloseGuard::Closed;
            self.workspace_restore_conflict_confirmation =
                self.workspace_persistence.iter().find_map(|(key, state)| {
                    matches!(state.load, WorkspaceLoadPhase::Conflict).then(|| key.clone())
                });
            self.close_after_restore_conflict_confirmation =
                self.workspace_restore_conflict_confirmation.is_some();
            self.model.status =
                "Confirm whether the local workspace may replace prior saved data.".to_owned();
        } else if retry {
            self.discard_local_changes_on_close = false;
            let clear_keys = self
                .workspace_persistence
                .iter()
                .filter(|(_, state)| matches!(state.clear, WorkspaceClearPhase::Failed { .. }))
                .map(|(key, _)| key.clone())
                .collect::<Vec<_>>();
            for key in clear_keys {
                self.submit_clear_workspace(&key);
            }
            let load_keys = self
                .workspace_persistence
                .iter()
                .filter(|(_, state)| {
                    matches!(state.load, WorkspaceLoadPhase::Failed(_)) && !state.clear.has_intent()
                })
                .map(|(key, _)| key.clone())
                .collect::<Vec<_>>();
            for key in load_keys {
                if let Some(state) = self.workspace_persistence.get_mut(&key) {
                    state.load = WorkspaceLoadPhase::Unloaded;
                }
                self.request_workspace_load(&key);
            }
            self.flush_all_dirty_workspaces();
            self.workspace_close_guard = WorkspaceCloseGuard::AwaitingSave;
        } else if discard {
            self.workspace_close_guard = WorkspaceCloseGuard::Closed;
            self.discard_local_changes_on_close = true;
            root_ui
                .ctx()
                .send_viewport_cmd(egui::ViewportCommand::Close);
        } else if cancel {
            self.workspace_close_guard = WorkspaceCloseGuard::Closed;
            self.discard_local_changes_on_close = false;
            self.model.status = "Close cancelled; local changes were kept.".to_owned();
        }
    }

    fn handle_workspace_shortcuts(&mut self, ui: &mut egui::Ui) {
        let modal_open = self.active_modal_kind().is_some();
        if modal_open {
            ui.input_mut(|input| {
                input.events.retain(|event| {
                    !matches!(
                        event,
                        egui::Event::Key {
                            key: egui::Key::Enter,
                            pressed: true,
                            modifiers,
                            ..
                        } if modifiers.command && !modifiers.alt
                    )
                });
            });
        }
        let shortcut_context = !modal_open && self.profile_editor.is_none();
        let history_find = shortcut_context && consume_command_key(ui, egui::Key::F, true);
        let save = shortcut_context && consume_command_key(ui, egui::Key::S, false);
        let new_editor = shortcut_context && consume_command_key(ui, egui::Key::T, false);
        let close_editor = shortcut_context && consume_command_key(ui, egui::Key::W, false);
        let context_find =
            shortcut_context && !history_find && consume_command_key(ui, egui::Key::F, false);
        let escape_actionable = modal_open
            || self.profile_editor.is_some()
            || self
                .model
                .selected_workspace()
                .is_some_and(|workspace| workspace.pending_execute.is_some());
        let escape = escape_actionable
            && ui.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Escape));

        if save {
            let _ = self.request_selected_workspace_save();
        }
        if history_find && let Some(key) = self.model.selected_workspace_key() {
            self.model
                .workspace_mut(key.clone())
                .select_result_area_tab(ResultAreaTab::History);
            self.workspace_history_focus = Some(key);
        } else if context_find {
            let history_key = self.model.selected_workspace_key().filter(|key| {
                self.model
                    .workspace(key)
                    .is_some_and(|workspace| workspace.result_area_tab() == ResultAreaTab::History)
            });
            if let Some(key) = history_key {
                self.workspace_history_focus = Some(key);
            } else {
                self.connection_filter_focus = true;
            }
        }
        if new_editor && let Some(profile) = self.model.selected_profile_snapshot().cloned() {
            let key = WorkspaceKey::new(profile.id.clone(), profile.generation);
            let loading = self
                .workspace_persistence
                .get(&key)
                .is_some_and(WorkspacePersistenceState::load_can_restore);
            if loading {
                self.model.status =
                    "Wait for private workspace restore before creating a tab.".to_owned();
            } else {
                let workspace = self.model.workspace_mut(key);
                let title = format!("Query {}", workspace.editor_tabs().len().saturating_add(1));
                match workspace.create_editor_tab(profile.driver.language(), title, String::new()) {
                    Ok(_) => self.editor_surface = EditorSurface::default(),
                    Err(error) => self.model.status = error.to_string(),
                }
            }
        }
        if close_editor
            && let Some(key) = self.model.selected_workspace_key()
            && let Some(tab_id) = self
                .model
                .workspace(&key)
                .and_then(ProfileWorkspace::selected_editor_tab_id)
        {
            self.request_editor_tab_close(key, tab_id, "editor.tab.discard");
        }
        if escape {
            if self.workspace_close_guard != WorkspaceCloseGuard::Closed {
                self.workspace_close_guard = WorkspaceCloseGuard::Closed;
                self.discard_local_changes_on_close = false;
                self.model.status = "Close cancelled; local changes were kept.".to_owned();
            } else if self.workspace_restore_conflict_confirmation.is_some() {
                self.workspace_restore_conflict_confirmation = None;
                self.close_after_restore_conflict_confirmation = false;
            } else if self.workspace_clear_confirmation.is_some() {
                self.workspace_clear_confirmation = None;
            } else if self.editor_discard_confirmation.is_some() {
                self.editor_discard_confirmation = None;
            } else if self.delete_confirmation.is_some() {
                self.cancel_delete_confirmation();
            } else if self.credential_prompt.is_some() {
                self.cancel_credential_prompt();
            } else if self.profile_editor.is_some() {
                self.profile_editor = None;
            } else if let Some(operation_id) = self
                .model
                .selected_workspace()
                .and_then(|workspace| workspace.pending_execute)
            {
                self.submit_editor_intent(EditorIntent::Cancel { operation_id });
            }
        }
    }

    fn active_modal_kind(&self) -> Option<ModalKind> {
        if self.workspace_close_guard != WorkspaceCloseGuard::Closed {
            Some(ModalKind::WorkspaceClose)
        } else if self.workspace_restore_conflict_confirmation.is_some() {
            Some(ModalKind::WorkspaceConflict)
        } else if self.workspace_clear_confirmation.is_some() {
            Some(ModalKind::WorkspaceClear)
        } else if self.editor_discard_confirmation.is_some() {
            Some(ModalKind::EditorDiscard)
        } else if self.credential_prompt.is_some() {
            Some(ModalKind::Credential)
        } else if self.delete_confirmation.is_some() {
            Some(ModalKind::Delete)
        } else {
            None
        }
    }

    fn workspace_mutation_input_pending(ui: &egui::Ui) -> bool {
        ui.input(|input| {
            let pointer_dragging = input.pointer.any_down();
            input.events.iter().any(|event| match event {
                egui::Event::Cut
                | egui::Event::Paste(_)
                | egui::Event::Text(_)
                | egui::Event::Ime(_)
                | egui::Event::PointerButton { .. }
                | egui::Event::Touch { .. }
                | egui::Event::AccessKitActionRequest(_) => true,
                egui::Event::Key { pressed, .. } => *pressed,
                egui::Event::PointerMoved(_) => pointer_dragging,
                _ => false,
            })
        })
    }

    fn show_modal_surfaces(&mut self, ui: &mut egui::Ui) {
        let active = self.active_modal_kind();
        let request_focus = active.is_some() && self.focused_modal != active;
        self.focused_modal = active;
        match active {
            Some(ModalKind::Delete) => self.show_delete_confirmation(ui, request_focus),
            Some(ModalKind::EditorDiscard) => {
                self.show_editor_discard_confirmation(ui, request_focus);
            }
            Some(ModalKind::Credential) => self.show_credential_prompt(ui, request_focus),
            Some(ModalKind::WorkspaceClear) => {
                self.show_workspace_clear_confirmation(ui, request_focus);
            }
            Some(ModalKind::WorkspaceConflict) => {
                self.show_workspace_restore_conflict_confirmation(ui, request_focus);
            }
            Some(ModalKind::WorkspaceClose) => {
                self.show_workspace_close_guard(ui, request_focus);
            }
            None => {}
        }
    }

    fn show_native(&mut self, ui: &mut egui::Ui) {
        OpenAiTheme::apply(ui.ctx());
        self.handle_workspace_shortcuts(ui);
        let modal_open = self.active_modal_kind().is_some();
        let workspace_dirty_hint = !modal_open && Self::workspace_mutation_input_pending(ui);
        self.frame_workspace_dirty_hint = workspace_dirty_hint;
        let selected_workspace = self.model.selected_workspace_key();
        if self.compact_workspace != selected_workspace {
            ui.ctx().data_mut(|data| {
                data.remove::<egui::PanelState>(egui::Id::new("navigator"));
                data.remove::<egui::PanelState>(egui::Id::new("result-history-tabs"));
            });
            self.compact_workspace = selected_workspace.clone();
            self.compact_fallback = CompactFallback::default();
            self.compact_restore_focus = None;
        }
        let geometry: WorkspaceGeometry = selected_workspace
            .as_ref()
            .and_then(|key| self.workspace_geometries.get(key))
            .copied()
            .unwrap_or_else(WorkspaceGeometry::default);
        let layout = NativeLayout::resolve(ui.available_width(), ui.available_height(), geometry);
        let compact_fallback: CompactFallback = self.compact_fallback.clone();

        if self.model.profile_load_succeeded()
            && self.model.profiles.is_empty()
            && self.profile_editor.is_none()
        {
            self.show_status_strip(ui, workspace_dirty_hint);
            egui::CentralPanel::default().show(ui, |ui| {
                ui.add_enabled_ui(!modal_open, |ui| self.show_first_run(ui));
            });
            self.show_modal_surfaces(ui);
            return;
        }

        self.show_status_strip(ui, workspace_dirty_hint);
        match layout.mode() {
            LayoutMode::Wide => self.show_wide_workspace(ui, geometry),
            LayoutMode::Compact => self.show_compact_workspace(ui, compact_fallback),
        }
        self.show_modal_surfaces(ui);
    }

    fn show_wide_workspace(&mut self, root_ui: &mut egui::Ui, geometry: WorkspaceGeometry) {
        let modal_open = self.active_modal_kind().is_some();
        let navigator = egui::Panel::left("navigator")
            .resizable(true)
            .default_size(if geometry.navigator_width().is_finite() {
                geometry.navigator_width()
            } else {
                NativeLayout::NAVIGATOR_DEFAULT_WIDTH
            })
            .size_range(NativeLayout::NAVIGATOR_WIDTH_RANGE.clone())
            .show(root_ui, |ui| {
                ui.add_enabled_ui(!modal_open, |ui| self.show_workspace_navigator(ui));
            });
        self.remember_workspace_geometry(Some(navigator.response.rect.width()), None, None);
        self.show_editor_result_shell(root_ui, geometry);
    }

    fn show_compact_workspace(
        &mut self,
        root_ui: &mut egui::Ui,
        compact_fallback: CompactFallback,
    ) {
        let modal_open = self.active_modal_kind().is_some();
        let prior_focus = root_ui.ctx().memory(|memory| memory.focused());
        let mut open_surface = None;
        let mut navigator_open_id = None;
        let mut inspector_open_id = None;
        egui::Panel::top("compact-workspace-actions")
            .resizable(false)
            .show(root_ui, |ui| {
                ui.add_enabled_ui(!modal_open, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        let navigator = ui.add_sized(
                            NativeLayout::ACTION_MIN_SIZE,
                            egui::Button::new("Navigator"),
                        );
                        let navigator =
                            named_author_id(navigator, "navigator.open", "Open navigator");
                        navigator_open_id = Some(navigator.id);
                        if navigator.clicked() {
                            open_surface = Some(FallbackSurface::Navigator);
                        }

                        let inspector = ui
                            .add_sized(NativeLayout::ACTION_MIN_SIZE, egui::Button::new("Results"));
                        let inspector =
                            named_author_id(inspector, "inspector.open", "Open result inspector");
                        inspector_open_id = Some(inspector.id);
                        if inspector.clicked() {
                            open_surface = Some(FallbackSurface::Inspector);
                        }
                    });
                });
            });

        if let Some(surface) = open_surface {
            let (restore_author_id, opener_id) = match surface {
                FallbackSurface::Navigator => ("navigator.open", navigator_open_id),
                FallbackSurface::Inspector => ("inspector.open", inspector_open_id),
            };
            self.compact_restore_focus = prior_focus.or(opener_id);
            self.compact_fallback.open(surface, restore_author_id);
        }

        let visible_surface = self
            .compact_fallback
            .visible_surface()
            .or(compact_fallback.visible_surface());
        let mut close_fallback = false;
        egui::CentralPanel::default().show(root_ui, |ui| {
            ui.add_enabled_ui(!modal_open, |ui| match visible_surface {
                Some(FallbackSurface::Navigator) => {
                    close_fallback = self.show_fallback_close(ui, "Close navigator");
                    ui.separator();
                    self.show_workspace_navigator(ui);
                }
                Some(FallbackSurface::Inspector) => {
                    close_fallback = self.show_fallback_close(ui, "Close result inspector");
                    ui.separator();
                    self.show_result_surface(ui);
                }
                None => self.show_editor_surface(ui),
            });
        });

        if close_fallback {
            let _ = self.compact_fallback.close();
            if let Some(focus) = self.compact_restore_focus.take() {
                root_ui
                    .ctx()
                    .memory_mut(|memory| memory.request_focus(focus));
            }
        }
    }

    fn show_fallback_close(&mut self, ui: &mut egui::Ui, label: &'static str) -> bool {
        let close = ui.add_sized(
            [112.0, OpenAiTheme::MIN_CONTROL_HEIGHT],
            egui::Button::new("Close"),
        );
        named_author_id(close, "fallback.close", label).clicked()
    }

    fn remember_workspace_geometry(
        &mut self,
        navigator_width: Option<f32>,
        editor_share: Option<f32>,
        inspector_visible: Option<bool>,
    ) {
        let Some(key) = self.model.selected_workspace_key() else {
            return;
        };
        let previous = self
            .workspace_geometries
            .get(&key)
            .copied()
            .unwrap_or_else(WorkspaceGeometry::default);
        let navigator_width = navigator_width
            .unwrap_or_else(|| previous.navigator_width())
            .clamp(
                *NativeLayout::NAVIGATOR_WIDTH_RANGE.start(),
                *NativeLayout::NAVIGATOR_WIDTH_RANGE.end(),
            );
        let editor_share = editor_share
            .unwrap_or_else(|| previous.editor_share())
            .clamp(
                WORKSPACE_EDITOR_COLLAPSED_SHARE,
                WORKSPACE_RESULTS_COLLAPSED_SHARE,
            );
        let inspector_visible = inspector_visible.unwrap_or_else(|| previous.inspector_visible());
        let geometry = WorkspaceGeometry::restore(navigator_width, editor_share, inspector_visible);
        self.workspace_geometries.insert(key.clone(), geometry);
        self.sync_collapsed_workspace_pane(&key, geometry);
        if self
            .workspace_persistence
            .get(&key)
            .is_some_and(|state| !matches!(state.load, WorkspaceLoadPhase::Ready))
        {
            return;
        }
        if let Ok(snapshot) = WorkspaceGeometrySnapshot::new(
            geometry.navigator_width(),
            geometry.editor_share(),
            geometry.inspector_visible(),
        ) && self
            .model
            .workspace(&key)
            .and_then(ProfileWorkspace::persistence)
            .is_some()
            && let Err(error) = self
                .model
                .workspace_mut(key)
                .set_persistence_geometry(snapshot)
        {
            self.model.status = error.to_string();
        }
    }

    fn collapsed_pane_for_geometry(geometry: WorkspaceGeometry) -> Option<Pane> {
        if !geometry.inspector_visible()
            || geometry.editor_share() >= WORKSPACE_RESULTS_COLLAPSED_SHARE
        {
            Some(Pane::Subordinate)
        } else if geometry.editor_share() <= WORKSPACE_EDITOR_COLLAPSED_SHARE {
            Some(Pane::Editor)
        } else {
            None
        }
    }

    fn sync_collapsed_workspace_pane(&mut self, key: &WorkspaceKey, geometry: WorkspaceGeometry) {
        if let Some(pane) = Self::collapsed_pane_for_geometry(geometry) {
            self.collapsed_workspace_panes.insert(key.clone(), pane);
        } else {
            self.collapsed_workspace_panes.remove(key);
        }
    }

    fn show_status_strip(&mut self, root_ui: &mut egui::Ui, workspace_dirty_hint: bool) {
        let modal_open = self.active_modal_kind().is_some();
        let selected = self.model.selected_profile_snapshot().cloned();
        let connection = selected
            .as_ref()
            .map(|profile| connection_label(self.model.connection_state(&profile.id)))
            .unwrap_or_else(|| "No connection selected".to_owned());
        let active = selected.as_ref().and_then(|profile| {
            self.active_operations
                .get(&profile.id)
                .copied()
                .filter(|operation| operation.profile_generation == profile.generation)
        });
        let result_summary = selected
            .as_ref()
            .and_then(|profile| {
                self.model
                    .workspace(&WorkspaceKey::new(profile.id.clone(), profile.generation))
            })
            .and_then(|workspace| workspace.result.as_ref())
            .map_or_else(
                || "None".to_owned(),
                |result| {
                    format!(
                        "{} ms · {} returned · {} affected · {}",
                        result.provenance.duration_ms,
                        result.rows.len(),
                        result.affected_rows,
                        if result.truncated {
                            "Truncated"
                        } else {
                            "Complete"
                        }
                    )
                },
            );
        let selected_workspace_is_dirty = self
            .model
            .selected_workspace()
            .is_some_and(|workspace| !workspace.is_saved());
        let operation_status = if (workspace_dirty_hint || selected_workspace_is_dirty)
            && operation_status_claims_workspace_saved(&self.model.status)
        {
            "Local workspace changes are Unsaved.".to_owned()
        } else {
            self.model.status.clone()
        };
        let mut cancel = None;

        egui::Panel::bottom("status-action-context")
            .resizable(false)
            .show(root_ui, |ui| {
                ui.add_enabled_ui(!modal_open, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        let status = ui.strong("Status");
                        named_author_id(
                            status,
                            "status-action-context",
                            "Status and action context",
                        );
                        if let Some(profile) = selected.as_ref() {
                            let profile_name = profile.name.clone();
                            let profile_status = ui.small(format!("Profile: {profile_name}"));
                            named_dynamic_value_author_id(
                                profile_status,
                                "status.profile".to_owned(),
                                "Selected profile".to_owned(),
                                profile_name,
                            );

                            let target = match profile.database.as_deref() {
                                Some(database) => format!("{} / {database}", profile.endpoint),
                                None => profile.endpoint.clone(),
                            };
                            let target_status = ui.small(format!("Target: {target}"));
                            named_dynamic_value_author_id(
                                target_status,
                                "status.target".to_owned(),
                                "Selected target".to_owned(),
                                target,
                            );

                            ui.small(format!(
                                "Environment: {}",
                                profile_environment_label(profile.persisted.safety.environment())
                            ));
                            ui.small(format!(
                                "Access: {}",
                                profile_access_label(profile.persisted.safety.effective_access())
                            ));
                        } else {
                            ui.small("Profile: None");
                            ui.small("Environment: Unclassified");
                            ui.small("Access: Read-only");
                        }
                        ui.small(format!("Connection: {connection}"));
                    });
                    ui.horizontal_wrapped(|ui| {
                        let status = ui.small(format!("Operation: {operation_status}"));
                        named_dynamic_value_author_id(
                            status,
                            "status.operation".to_owned(),
                            "Current operation status".to_owned(),
                            operation_status,
                        );
                        let result = ui.small(format!("Latest result: {result_summary}"));
                        named_dynamic_value_author_id(
                            result,
                            "status.result".to_owned(),
                            "Selected result summary".to_owned(),
                            result_summary.clone(),
                        );
                        if let Some(operation) = active {
                            ui.small(format!("Active: {}", operation_kind_label(operation.kind)));
                            let button = ui.add_sized(
                                [112.0, OpenAiTheme::MIN_CONTROL_HEIGHT],
                                egui::Button::new("Cancel"),
                            );
                            if named_author_id(
                                button,
                                "status.cancel",
                                "Cancel current selected operation",
                            )
                            .clicked()
                            {
                                cancel = Some(operation);
                            }
                        }
                    });
                });
            });

        if let Some(operation) = cancel {
            let still_current = self
                .model
                .selected_profile_snapshot()
                .is_some_and(|profile| {
                    operation.profile_generation == profile.generation
                        && self.active_operations.get(&profile.id) == Some(&operation)
                });
            if !still_current {
                self.model.status = "The selected operation is no longer current.".to_owned();
            } else {
                match self.port.try_submit(UiCommand::CancelOperation {
                    operation_id: operation.operation_id,
                }) {
                    Ok(()) => {
                        self.model.status =
                            format!("Cancelling operation {}…", operation.operation_id.0);
                    }
                    Err(error) => self.report_submit_error(error),
                }
            }
        }
    }

    fn show_first_run(&mut self, ui: &mut egui::Ui) {
        ui.vertical_centered(|ui| {
            ui.set_max_width(520.0);
            ui.add_space(64.0);
            ui.heading("Connect your first database");
            ui.label("Create a local profile. Credentials stay outside the saved profile.");
            ui.add_space(24.0);
            ui.label("Database");

            let mysql = ui.add_sized(
                [280.0, OpenAiTheme::MIN_CONTROL_HEIGHT],
                egui::RadioButton::new(self.first_run_driver == DriverKind::MySql, "MySQL"),
            );
            if named_author_id(mysql, "connection.new.mysql", "New MySQL connection").clicked() {
                self.first_run_driver = DriverKind::MySql;
            }
            let redis = ui.add_sized(
                [280.0, OpenAiTheme::MIN_CONTROL_HEIGHT],
                egui::RadioButton::new(self.first_run_driver == DriverKind::Redis, "Redis"),
            );
            if named_author_id(redis, "connection.new.redis", "New Redis connection").clicked() {
                self.first_run_driver = DriverKind::Redis;
            }
            let mongodb = ui.add_enabled(false, egui::RadioButton::new(false, "MongoDB · Planned"));
            named_author_id(
                mongodb,
                "connection.mongodb.planned",
                "MongoDB planned and unavailable",
            );

            ui.add_space(24.0);
            let primary = ui.add(
                egui::Button::new(
                    egui::RichText::new("New connection").color(egui::Color32::WHITE),
                )
                .fill(egui::Color32::BLACK)
                .min_size(egui::vec2(280.0, OpenAiTheme::MIN_CONTROL_HEIGHT)),
            );
            if named_author_id(primary, "connection.new", "New connection").clicked() {
                let draft_id = self.allocate_draft_id();
                let editor = ProfileEditor::new(draft_id, self.first_run_driver);
                self.profile_editor = Some(self.configured_profile_editor(editor));
            }
            ui.add_space(24.0);
            ui.label("Credential sources: None · This app session · Environment variable");
        });
    }

    fn show_workspace_navigator(&mut self, ui: &mut egui::Ui) {
        let navigator = ui.heading("Navigator");
        named_author_id(navigator, "navigator", "Connection and object navigator");
        ui.separator();

        let available_height = ui.available_height();
        let connections_height = (available_height * 0.46)
            .clamp(180.0, 360.0)
            .min(available_height.max(0.0));
        ui.allocate_ui_with_layout(
            egui::vec2(ui.available_width(), connections_height),
            egui::Layout::top_down(egui::Align::Min),
            |ui| self.connections_contents(ui),
        );

        ui.separator();
        ui.heading("Object explorer");
        egui::ScrollArea::vertical()
            .id_salt("navigator.object-scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| self.explorer_contents(ui));
    }

    #[cfg(test)]
    fn connections(&mut self, root_ui: &mut egui::Ui) {
        self.connections_contents(root_ui);
    }

    fn connections_contents(&mut self, ui: &mut egui::Ui) {
        ui.heading("Connections");
        let filter = ui.add_sized(
            [ui.available_width(), OpenAiTheme::MIN_CONTROL_HEIGHT],
            egui::TextEdit::singleline(&mut self.connection_filter)
                .id_salt("navigator.connection-filter")
                .hint_text("Filter connections")
                .desired_width(f32::INFINITY),
        );
        let filter = named_author_id(filter, "navigator.connection-filter", "Filter connections");
        if self.connection_filter_focus {
            filter.request_focus();
            self.connection_filter_focus = false;
        }
        ui.add_space(8.0);
        let actions_enabled = !self.model.is_config_uncertain();
        let mut new_driver = None;
        let mut reload = false;
        ui.horizontal_wrapped(|ui| {
            let mysql = ui.add_enabled(
                actions_enabled,
                egui::Button::new("+ MySQL")
                    .min_size(egui::vec2(96.0, OpenAiTheme::MIN_CONTROL_HEIGHT)),
            );
            if named_author_id(mysql, "connection.new.mysql", "New MySQL connection").clicked() {
                new_driver = Some(DriverKind::MySql);
            }
            let redis = ui.add_enabled(
                actions_enabled,
                egui::Button::new("+ Redis")
                    .min_size(egui::vec2(96.0, OpenAiTheme::MIN_CONTROL_HEIGHT)),
            );
            if named_author_id(redis, "connection.new.redis", "New Redis connection").clicked() {
                new_driver = Some(DriverKind::Redis);
            }
            let mongodb = ui.add_enabled(
                false,
                egui::Button::new("MongoDB · Planned")
                    .min_size(egui::vec2(160.0, OpenAiTheme::MIN_CONTROL_HEIGHT)),
            );
            named_author_id(
                mongodb,
                "connection.mongodb.planned",
                "MongoDB planned and unavailable",
            );
            let reload_button = ui.add_sized(
                [96.0, OpenAiTheme::MIN_CONTROL_HEIGHT],
                egui::Button::new("Reload"),
            );
            reload = named_author_id(
                reload_button,
                "connection.reload",
                "Reload connection profiles",
            )
            .clicked();
        });
        if let Some(driver) = new_driver {
            let draft_id = self.allocate_draft_id();
            let editor = ProfileEditor::new(draft_id, driver);
            self.profile_editor = Some(self.configured_profile_editor(editor));
        }
        if reload {
            self.submit_refresh();
        }
        ui.separator();
        let list_height = ui.available_height().max(0.0);
        egui::ScrollArea::vertical()
            .id_salt("navigator.connections-scroll")
            .max_height(list_height)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let normalized_filter = self.connection_filter.trim().to_ascii_lowercase();
                let profiles = self
                    .model
                    .profiles
                    .iter()
                    .enumerate()
                    .filter(|(_, profile)| {
                        normalized_filter.is_empty()
                            || profile
                                .name
                                .to_ascii_lowercase()
                                .contains(&normalized_filter)
                            || profile
                                .id
                                .0
                                .to_ascii_lowercase()
                                .contains(&normalized_filter)
                            || profile
                                .driver
                                .to_string()
                                .to_ascii_lowercase()
                                .contains(&normalized_filter)
                            || profile
                                .endpoint
                                .to_ascii_lowercase()
                                .contains(&normalized_filter)
                            || profile.database.as_ref().is_some_and(|database| {
                                database.to_ascii_lowercase().contains(&normalized_filter)
                            })
                    })
                    .map(|(profile_index, profile)| (profile_index, profile.clone()))
                    .collect::<Vec<_>>();
                for (profile_index, profile) in profiles {
                    self.profile_card(ui, &profile, profile_index);
                    ui.add_space(8.0);
                }
                if !normalized_filter.is_empty()
                    && self.model.profiles.iter().all(|profile| {
                        !profile
                            .name
                            .to_ascii_lowercase()
                            .contains(&normalized_filter)
                            && !profile
                                .id
                                .0
                                .to_ascii_lowercase()
                                .contains(&normalized_filter)
                            && !profile
                                .driver
                                .to_string()
                                .to_ascii_lowercase()
                                .contains(&normalized_filter)
                            && !profile
                                .endpoint
                                .to_ascii_lowercase()
                                .contains(&normalized_filter)
                            && !profile.database.as_ref().is_some_and(|database| {
                                database.to_ascii_lowercase().contains(&normalized_filter)
                            })
                    })
                {
                    ui.weak("No matching connections");
                }
            });
    }

    fn explorer_contents(&mut self, ui: &mut egui::Ui) {
        let selected = self.model.selected_profile_snapshot().cloned();
        let mut recovery = None;
        self.visible_redis_workspace = None;
        match selected {
            Some(profile) if profile.driver == DriverKind::MySql && profile.is_ready() => {
                let intents = self
                    .mysql_explorers
                    .entry((profile.id.clone(), profile.generation))
                    .or_default()
                    .show(ui);
                for intent in intents {
                    self.submit_mysql_explorer_intent(&profile, intent);
                }
                let key = super::model::WorkspaceKey::new(profile.id.clone(), profile.generation);
                let visible = self.model.workspace(&key).and_then(|workspace| {
                    Some(VisibleError {
                        operation_id: workspace.catalog_retry.as_ref()?.operation_id(),
                        error: workspace.catalog_error.clone()?,
                    })
                });
                if let Some(visible) = visible
                    && let Some(action) = render_recovery_error(ui, "catalog", &visible)
                {
                    recovery = Some((visible, action));
                }
            }
            Some(profile) if profile.driver == DriverKind::Redis && profile.is_ready() => {
                let key = WorkspaceKey::new(profile.id.clone(), profile.generation);
                self.visible_redis_workspace = Some(key.clone());
                let actions_enabled = !self.model.is_config_uncertain();
                let intent = self.redis_explorer_mut(&key).show(ui, actions_enabled);
                if let Some(intent) = intent {
                    self.submit_redis_intent(intent);
                }
                let (scan, inspect) =
                    self.model
                        .workspace(&key)
                        .map_or((None, None), |workspace| {
                            let scan = workspace.redis_scan_retry.as_ref().and_then(|request| {
                                Some(VisibleError {
                                    operation_id: request.operation_id(),
                                    error: workspace.redis_scan_error.clone()?,
                                })
                            });
                            let inspect =
                                workspace.redis_inspect_retry.as_ref().and_then(|request| {
                                    Some(VisibleError {
                                        operation_id: request.operation_id(),
                                        error: workspace.redis_inspect_error.clone()?,
                                    })
                                });
                            (scan, inspect)
                        });
                if let Some(visible) = scan
                    && let Some(action) = render_recovery_error(ui, "redis_scan", &visible)
                {
                    recovery = Some((visible, action));
                }
                if let Some(visible) = inspect
                    && let Some(action) = render_recovery_error(ui, "redis_inspect", &visible)
                {
                    recovery = Some((visible, action));
                }
            }
            Some(profile) => {
                ui.weak(format!("{} explorer is unavailable", profile.driver));
            }
            None => {
                ui.weak("Select a connection to browse resources.");
            }
        }
        if let Some((visible, action)) = recovery {
            self.dispatch_error_recovery(visible.operation_id, &visible.error, action);
        }
    }

    fn profile_card(&mut self, ui: &mut egui::Ui, profile: &ProfileSnapshot, profile_index: usize) {
        let selected = self.model.selected_profile.as_ref() == Some(&profile.id);
        let profile_selection =
            ui.selectable_label(selected, format!("{} · {}", profile.name, profile.driver));
        let profile_selection = named_dynamic_author_id(
            profile_selection,
            format!("connection.profile.{profile_index}"),
            "Connection profile",
        );
        if profile_selection.clicked() {
            self.model.selected_profile = Some(profile.id.clone());
        }
        ui.small(&profile.endpoint);
        if let Some(database) = &profile.database {
            ui.small(format!("database: {database}"));
        }
        if profile.persisted.credential_mode == CredentialMode::Environment {
            let availability = profile
                .environment_availability
                .unwrap_or(EnvironmentAvailability::Missing);
            let availability = environment_availability_label(availability).to_owned();
            let response = ui.small(format!("Environment credential: {availability}"));
            named_dynamic_value_author_id(
                response,
                "profile.environment.availability".to_owned(),
                "Environment credential".to_owned(),
                availability,
            );
        }
        let state = self.model.connection_state(&profile.id).clone();
        let actions_enabled = !self.model.is_config_uncertain();
        ui.horizontal_wrapped(|ui| {
            ui.label(connection_label(&state));
            match state {
                ConnectionState::Disconnected | ConnectionState::Failed { .. } => {
                    let connect = ui.add_enabled(
                        actions_enabled && profile.can_connect(),
                        egui::Button::new("Connect")
                            .min_size(egui::vec2(104.0, OpenAiTheme::MIN_CONTROL_HEIGHT)),
                    );
                    if named_author_id(connect, "connection.connect", "Connect to profile")
                        .clicked()
                    {
                        self.submit_test(profile.id.clone());
                    }
                }
                ConnectionState::NeedsCredential => {
                    let credential = ui.add_enabled(
                        actions_enabled,
                        egui::Button::new("Enter credential")
                            .min_size(egui::vec2(144.0, OpenAiTheme::MIN_CONTROL_HEIGHT)),
                    );
                    if named_author_id(
                        credential,
                        "connection.credential.open",
                        "Enter session credential",
                    )
                    .clicked()
                    {
                        self.open_session_credential_prompt(profile.id.clone());
                    }
                }
                ConnectionState::Connected { .. } => {
                    let disconnect = ui.add_enabled(
                        actions_enabled,
                        egui::Button::new("Disconnect")
                            .min_size(egui::vec2(112.0, OpenAiTheme::MIN_CONTROL_HEIGHT)),
                    );
                    if named_author_id(disconnect, "connection.disconnect", "Disconnect profile")
                        .clicked()
                    {
                        self.submit_disconnect(profile.id.clone());
                    }
                    let reconnect = ui.add_enabled(
                        actions_enabled,
                        egui::Button::new("Reconnect")
                            .min_size(egui::vec2(112.0, OpenAiTheme::MIN_CONTROL_HEIGHT)),
                    );
                    if named_author_id(reconnect, "connection.reconnect", "Reconnect profile")
                        .clicked()
                    {
                        self.submit_reconnect(profile.id.clone());
                    }
                }
                ConnectionState::Pending(_) | ConnectionState::Closing => {}
            }
        });
        ui.horizontal_wrapped(|ui| {
            let edit = ui.add_enabled(
                actions_enabled,
                egui::Button::new("Edit")
                    .min_size(egui::vec2(88.0, OpenAiTheme::MIN_CONTROL_HEIGHT)),
            );
            if named_author_id(edit, "profile.edit", "Edit profile").clicked() {
                let draft_id = self.allocate_draft_id();
                let editor = ProfileEditor::edit(
                    draft_id,
                    &profile.persisted,
                    profile.generation,
                    profile.has_current_session_secret,
                );
                self.profile_editor = Some(self.configured_profile_editor(editor));
            }
            let delete = ui.add_enabled(
                actions_enabled,
                egui::Button::new("Delete")
                    .min_size(egui::vec2(88.0, OpenAiTheme::MIN_CONTROL_HEIGHT)),
            );
            if named_author_id(delete, "profile.delete", "Delete profile").clicked() {
                self.open_delete_confirmation(profile);
            }
        });
        if profile.availability == DriverAvailability::Planned {
            ui.strong(format!(
                "Planned: {}",
                profile.planned_reason.as_deref().unwrap_or("not available")
            ));
        }
        if selected {
            ui.weak("Open in Explorer");
        }
    }

    fn show_editor_result_shell(&mut self, root_ui: &mut egui::Ui, geometry: WorkspaceGeometry) {
        let modal_open = self.active_modal_kind().is_some();
        let workspace_key = self.model.selected_workspace_key();
        if let Some(key) = workspace_key.as_ref() {
            self.sync_collapsed_workspace_pane(key, geometry);
        }
        let collapsed = workspace_key
            .as_ref()
            .and_then(|key| self.collapsed_workspace_panes.get(key))
            .copied();
        if let Some(pane) = collapsed {
            let mut restore = false;
            egui::CentralPanel::default().show(root_ui, |ui| {
                ui.add_enabled_ui(!modal_open, |ui| {
                    let (label, author_id) = match pane {
                        Pane::Editor => ("Restore editor", "workspace.editor.restore"),
                        Pane::Subordinate => {
                            ("Restore results/history", "workspace.results.restore")
                        }
                    };
                    let button = ui.add_sized(
                        [184.0, OpenAiTheme::MIN_CONTROL_HEIGHT],
                        egui::Button::new(label),
                    );
                    restore = named_author_id(button, author_id, label).clicked();
                    ui.separator();
                    match pane {
                        Pane::Editor => self.show_result_surface(ui),
                        Pane::Subordinate => self.show_editor_surface(ui),
                    }
                });
            });
            if restore {
                if let Some(key) = workspace_key {
                    self.collapsed_workspace_panes.remove(&key);
                }
                self.remember_workspace_geometry(
                    None,
                    Some(NativeLayout::DEFAULT_EDITOR_SHARE),
                    Some(true),
                );
            }
            return;
        }

        let total_extent = root_ui
            .available_height()
            .max(NativeLayout::PANE_MIN_EXTENT * 2.0);
        let maximum_subordinate =
            (total_extent - NativeLayout::PANE_MIN_EXTENT).max(NativeLayout::PANE_MIN_EXTENT);
        let subordinate_extent = (total_extent * (1.0 - geometry.editor_share()))
            .clamp(NativeLayout::PANE_MIN_EXTENT, maximum_subordinate);
        let editor_extent = total_extent - subordinate_extent;

        let result_panel = egui::Panel::bottom("result-history-tabs")
            .resizable(false)
            .show_separator_line(false)
            .exact_size(subordinate_extent)
            .show(root_ui, |ui| {
                ui.add_enabled_ui(!modal_open, |ui| {
                    let next_layout = show_workspace_splitter(ui, total_extent, editor_extent);
                    self.show_result_surface(ui);
                    next_layout
                })
                .inner
            });
        if let Some(next_layout) = result_panel.inner {
            if next_layout.editor_restore_label().is_some() {
                self.remember_workspace_geometry(
                    None,
                    Some(WORKSPACE_EDITOR_COLLAPSED_SHARE),
                    Some(true),
                );
            } else if next_layout.subordinate_restore_label().is_some() {
                self.remember_workspace_geometry(
                    None,
                    Some(WORKSPACE_RESULTS_COLLAPSED_SHARE),
                    Some(false),
                );
            } else if let Some(next_editor_extent) = next_layout.editor_extent() {
                self.remember_workspace_geometry(
                    None,
                    Some(next_editor_extent / total_extent),
                    Some(true),
                );
            }
        }

        egui::CentralPanel::default().show(root_ui, |ui| {
            ui.add_enabled_ui(!modal_open, |ui| self.show_editor_surface(ui));
        });
    }

    #[cfg(test)]
    fn editor_and_results(&mut self, root_ui: &mut egui::Ui) {
        self.show_editor_result_shell(root_ui, WorkspaceGeometry::default());
    }

    fn show_editor_surface(&mut self, ui: &mut egui::Ui) {
        let tab_title = if self.profile_editor.is_some() {
            "Connection profile"
        } else {
            "Editor"
        };
        let tab = ui.selectable_label(true, tab_title);
        named_author_id(tab, "object-editor-tabs", "Object and editor tabs");
        ui.separator();

        if self.profile_editor.is_some() {
            let action = self
                .profile_editor
                .as_mut()
                .map_or(FormAction::None, |editor| editor.show(ui));
            self.apply_profile_form_action(action);
            return;
        }

        let mut editor_intent = None;
        let mut recovery = None;
        if let Some(profile) = self.model.selected_profile_snapshot().cloned() {
            let key = super::model::WorkspaceKey::new(profile.id.clone(), profile.generation);
            let restore_resolved = self
                .workspace_persistence
                .get(&key)
                .is_none_or(|state| !state.load_can_restore());
            let editor_enabled = !self.model.is_config_uncertain()
                && restore_resolved
                && self.active_modal_kind().is_none();
            let context_value = self.workspace_context_value(&profile);
            let context = ui.add(
                egui::Label::new(
                    egui::RichText::new(&context_value)
                        .color(egui::Color32::BLACK)
                        .strong(),
                )
                .selectable(false),
            );
            named_dynamic_value_author_id(
                context,
                "workspace.context".to_owned(),
                "Workspace context".to_owned(),
                context_value,
            );
            ui.add_space(4.0);
            self.show_editor_tab_strip(ui, &profile, &key, editor_enabled);
            ui.separator();
            let sync_error = {
                let workspace = self.model.workspace_mut(key.clone());
                editor_intent = self.editor_surface.show(
                    ui,
                    &profile,
                    workspace,
                    editor_enabled && profile.is_ready(),
                );
                workspace.sync_selected_editor_tab_from_surface().err()
            };
            if let Some(error) = sync_error {
                self.model.status = error.to_string();
            }
            self.observe_workspace_revisions(Instant::now());
            self.show_workspace_persistence_controls(ui, &profile, &key);
        } else {
            ui.weak("Select a connection to edit a statement or command.");
        }
        if let Some(visible) = self.common_error.clone()
            && let Some(action) = render_recovery_error(ui, "common", &visible)
        {
            recovery = Some((visible, action));
        }
        if let Some(intent) = editor_intent {
            self.submit_editor_intent(intent);
        }
        if let Some((visible, action)) = recovery {
            self.dispatch_error_recovery(visible.operation_id, &visible.error, action);
        }
    }

    fn workspace_context_value(&self, profile: &ProfileSnapshot) -> String {
        let mut context = vec![profile.name.clone()];
        if let Some(database) = profile.database.as_ref() {
            context.push(database.clone());
        }
        if profile.driver == DriverKind::MySql
            && let Some(selected) = self
                .mysql_explorers
                .get(&(profile.id.clone(), profile.generation))
                .and_then(MySqlExplorerState::selected_object_display)
        {
            context.push(selected);
        }
        context.join(" → ")
    }

    fn show_result_surface(&mut self, ui: &mut egui::Ui) {
        let selected_workspace_key = self.model.selected_workspace_key();
        let selected_area = selected_workspace_key
            .as_ref()
            .and_then(|key| self.model.workspace(key))
            .map_or(ResultAreaTab::Results, ProfileWorkspace::result_area_tab);
        let selected_editor_tab_id = selected_workspace_key
            .as_ref()
            .and_then(|key| self.model.workspace(key))
            .and_then(ProfileWorkspace::selected_editor_tab_id);
        let mut next_area = None;
        ui.horizontal(|ui| {
            let results = ui.selectable_label(selected_area == ResultAreaTab::Results, "Results");
            let results = named_author_id(results, "result.tab.results", "Results tab");
            if results.clicked() {
                next_area = Some(ResultAreaTab::Results);
            }
            let history = ui.selectable_label(selected_area == ResultAreaTab::History, "History");
            let history = named_author_id(history, "result.tab.history", "History tab");
            if history.clicked() {
                next_area = Some(ResultAreaTab::History);
            }
        });
        let region = ui.small("Results and execution history");
        named_author_id(region, "result-history-tabs", "Result and history tabs");
        ui.separator();

        if let (Some(key), Some(area)) = (selected_workspace_key.clone(), next_area) {
            self.model.workspace_mut(key).select_result_area_tab(area);
        }
        let selected_area = next_area.unwrap_or(selected_area);
        if selected_area == ResultAreaTab::History {
            let Some(key) = selected_workspace_key else {
                ui.weak("Select a profile to inspect private history.");
                return;
            };
            let history = self
                .model
                .workspace(&key)
                .and_then(ProfileWorkspace::persistence)
                .map_or_else(Vec::new, |persistence| persistence.history().to_vec());
            let persistence_enabled = self
                .model
                .workspace(&key)
                .and_then(ProfileWorkspace::persistence)
                .is_some_and(ProfileWorkspacePersistence::persistence_enabled);
            let history_writable = persistence_enabled
                && self.workspace_persistence.get(&key).is_none_or(|state| {
                    !state.is_read_only()
                        && !matches!(state.load, WorkspaceLoadPhase::Loading { .. })
                        && !matches!(state.save, WorkspaceSavePhase::Saving { .. })
                        && !state.clear.has_intent()
                });
            let search = self
                .workspace_history_search
                .entry(key.clone())
                .or_default();
            ui.heading("Execution history");
            let search_response = ui.add_sized(
                [
                    ui.available_width().max(120.0),
                    OpenAiTheme::MIN_CONTROL_HEIGHT,
                ],
                egui::TextEdit::singleline(search)
                    .id_salt("history.search")
                    .hint_text("Search source, status, or Unix date")
                    .desired_width(f32::INFINITY),
            );
            let search_response =
                named_author_id(search_response, "history.search", "Search private history");
            if self.workspace_history_focus.as_ref() == Some(&key) {
                search_response.request_focus();
                self.workspace_history_focus = None;
            }
            let normalized = search.trim().to_ascii_lowercase();
            let clear_button = ui
                .add_enabled(
                    !history.is_empty() && history_writable,
                    egui::Button::new("Clear history")
                        .min_size(egui::vec2(128.0, OpenAiTheme::MIN_CONTROL_HEIGHT)),
                )
                .on_disabled_hover_text(
                    "History can be cleared only while private persistence is On and writable.",
                );
            let clear = named_author_id(
                clear_button,
                "history.clear",
                "Clear persisted history for this profile",
            )
            .clicked();
            ui.separator();
            let mut reopen = None;
            let matching = history
                .iter()
                .rev()
                .filter(|entry| {
                    if normalized.is_empty() {
                        return true;
                    }
                    let source = entry.source().unwrap_or("");
                    let searchable = format!(
                        "{} {} {}",
                        source,
                        workspace_history_status_label(entry.status()),
                        workspace_history_date_label(entry.completed_at_unix_ms())
                    )
                    .to_ascii_lowercase();
                    searchable.contains(&normalized)
                })
                .collect::<Vec<_>>();
            if matching.is_empty() {
                ui.weak(if history.is_empty() {
                    "No execution history yet"
                } else {
                    "No history matches this search"
                });
            } else {
                egui::ScrollArea::vertical()
                    .id_salt("result.history.list")
                    .show(ui, |ui| {
                        for entry in matching {
                            let source_label = entry.source().map_or_else(
                                || "Source omitted (over 64 KiB)".to_owned(),
                                workspace_history_source_preview,
                            );
                            let label = format!(
                                "{} · {} · {} · {} · {} ms · {} returned · {} affected · {}",
                                source_label,
                                workspace_history_status_label(entry.status()),
                                workspace_history_date_label(entry.completed_at_unix_ms()),
                                workspace_run_target_label(entry.target()),
                                entry.duration_ms(),
                                entry.returned_rows(),
                                entry.affected_rows(),
                                if entry.truncated() {
                                    "Truncated"
                                } else {
                                    "Complete"
                                }
                            );
                            let button = ui.add_enabled(
                                entry.is_reopenable(),
                                egui::Button::new(&label)
                                    .min_size(egui::vec2(
                                        ui.available_width(),
                                        OpenAiTheme::MIN_CONTROL_HEIGHT,
                                    )),
                            );
                            let button = named_dynamic_value_author_id(
                                button,
                                format!("history.entry.{}", entry.id()),
                                if entry.source_omitted() {
                                    "History source omitted because it exceeded 64 KiB; this entry cannot be reopened"
                                        .to_owned()
                                } else {
                                    "Open history source in a new editor".to_owned()
                                },
                                label,
                            );
                            if button.clicked()
                                && let Some(source) = entry.source()
                            {
                                reopen = Some((entry.id(), source.to_owned()));
                            }
                        }
                    });
            }
            if clear {
                self.clear_workspace_history(&key);
            } else if let Some((history_id, source)) = reopen {
                let language = self
                    .model
                    .selected_profile_snapshot()
                    .map(|profile| profile.driver.language());
                if let Some(language) = language {
                    let title = format!("History {history_id}");
                    match self
                        .model
                        .workspace_mut(key)
                        .create_editor_tab(language, title, source)
                    {
                        Ok(_) => {
                            self.editor_surface = EditorSurface::default();
                            self.model.status =
                                "History opened as a new draft; Run remains explicit.".to_owned();
                        }
                        Err(error) => self.model.status = error.to_string(),
                    }
                }
            }
            return;
        }

        let result_tabs = selected_workspace_key
            .as_ref()
            .and_then(|key| self.model.workspace(key))
            .map_or_else(Vec::new, |workspace| {
                workspace
                    .result_tabs_for_editor(selected_editor_tab_id)
                    .map(|tab| (tab.id(), tab.title(), tab.can_close()))
                    .collect::<Vec<_>>()
            });
        let selected_result = selected_workspace_key
            .as_ref()
            .and_then(|key| self.model.workspace(key))
            .and_then(ProfileWorkspace::selected_result_tab_id);
        let selected_result = selected_result
            .filter(|selected| result_tabs.iter().any(|(tab_id, _, _)| tab_id == selected));
        let mut select_result = None;
        let mut close_result = None;
        egui::ScrollArea::horizontal()
            .id_salt("result.output.tabs")
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    for (tab_id, title, can_close) in &result_tabs {
                        ui.push_id(tab_id.0, |ui| {
                            ui.horizontal(|ui| {
                                let tab = ui.add_sized(
                                    [120.0, OpenAiTheme::MIN_CONTROL_HEIGHT],
                                    egui::Button::new(title)
                                        .selected(selected_result == Some(*tab_id)),
                                );
                                let tab = named_dynamic_author_id(
                                    tab,
                                    format!("result.output.{}", tab_id.0),
                                    "Execution result tab",
                                );
                                if tab.clicked() {
                                    select_result = Some(*tab_id);
                                }

                                let close = ui
                                    .add_enabled(
                                        *can_close,
                                        egui::Button::new("×").min_size(egui::vec2(
                                            OpenAiTheme::MIN_CONTROL_HEIGHT,
                                            OpenAiTheme::MIN_CONTROL_HEIGHT,
                                        )),
                                    )
                                    .on_hover_text(if *can_close {
                                        "Close result tab"
                                    } else {
                                        "Cancel the active result operation before closing"
                                    });
                                let close = named_dynamic_author_id(
                                    close,
                                    format!("result.output.close.{}", tab_id.0),
                                    "Close result tab",
                                );
                                if close.clicked() {
                                    close_result = Some(*tab_id);
                                }
                            });
                        });
                    }
                });
            });
        if let (Some(key), Some(tab_id)) = (selected_workspace_key.clone(), close_result) {
            let _ = self.model.workspace_mut(key).close_result_tab(tab_id);
        } else if let (Some(key), Some(tab_id)) = (selected_workspace_key.clone(), select_result) {
            let _ = self.model.workspace_mut(key).select_result_tab(tab_id);
        }

        let mut result_intent = None;
        let mut has_result = false;
        if let Some(key) = selected_workspace_key {
            let workspace = self.model.workspace_mut(key);
            if let Some(result) = workspace.result.clone() {
                has_result = true;
                if let Some(intent) = workspace.result_view.show(ui, result.as_ref(), true) {
                    result_intent = Some((result, intent));
                }
                workspace.sync_selected_result_tab_from_surface();
            }
        }
        if !has_result {
            ui.weak("No result yet");
        }
        if let Some((result, intent)) = result_intent {
            self.handle_result_view_intent(result, intent);
        }
    }

    fn show_editor_tab_strip(
        &mut self,
        ui: &mut egui::Ui,
        profile: &ProfileSnapshot,
        key: &WorkspaceKey,
        enabled: bool,
    ) {
        let needs_initial_tab = self
            .model
            .workspace(key)
            .is_none_or(|workspace| workspace.editor_tabs().is_empty());
        let restore_pending = self
            .workspace_persistence
            .get(key)
            .is_some_and(WorkspacePersistenceState::load_can_restore);
        if !needs_initial_tab && let Some(state) = self.workspace_persistence.get_mut(key) {
            state.clean_empty_baseline_pending = None;
        }
        if needs_initial_tab && !restore_pending {
            let clean_empty_baseline_revision = self
                .workspace_persistence
                .get(key)
                .and_then(|state| state.clean_empty_baseline_pending);
            let initial_text = self
                .model
                .workspace(key)
                .map_or_else(String::new, |workspace| workspace.editor_text.clone());
            let initial_text_is_empty = initial_text.is_empty();
            let created = {
                let workspace = self.model.workspace_mut(key.clone());
                let revision_before_create = workspace.revision();
                workspace
                    .create_editor_tab(profile.driver.language(), "Query 1", initial_text)
                    .map(|_| {
                        (
                            clean_empty_baseline_revision == Some(revision_before_create)
                                && initial_text_is_empty,
                            workspace.revision(),
                        )
                    })
                    .ok()
            };
            if let Some((mark_clean, created_revision)) = created {
                if let Some(state) = self.workspace_persistence.get_mut(key) {
                    state.clean_empty_baseline_pending = None;
                }
                if mark_clean {
                    let _ = self
                        .model
                        .workspace_mut(key.clone())
                        .mark_saved_if_revision(created_revision);
                }
            }
        }

        let (tabs, selected) = self.model.workspace(key).map_or_else(
            || (Vec::new(), None),
            |workspace| {
                (
                    workspace
                        .editor_tabs()
                        .iter()
                        .map(|tab| (tab.id(), tab.title().to_owned(), tab.is_dirty()))
                        .collect::<Vec<_>>(),
                    workspace.selected_editor_tab_id(),
                )
            },
        );

        let mut select = None;
        let mut close_tab = None;
        egui::ScrollArea::horizontal()
            .id_salt("editor.tab-strip")
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    for (tab_id, title, dirty) in &tabs {
                        let label = if *dirty {
                            format!("{title} •")
                        } else {
                            title.clone()
                        };
                        ui.push_id(tab_id.0, |ui| {
                            ui.horizontal(|ui| {
                                let response = ui.add_sized(
                                    [120.0, OpenAiTheme::MIN_CONTROL_HEIGHT],
                                    egui::Button::new(label).selected(selected == Some(*tab_id)),
                                );
                                let response = named_dynamic_author_id(
                                    response,
                                    format!("editor.tab.{}", tab_id.0),
                                    "Editor tab",
                                );
                                if response.clicked() {
                                    select = Some(*tab_id);
                                }

                                let close = ui
                                    .add_enabled(
                                        enabled,
                                        egui::Button::new("×").min_size(egui::vec2(
                                            OpenAiTheme::MIN_CONTROL_HEIGHT,
                                            OpenAiTheme::MIN_CONTROL_HEIGHT,
                                        )),
                                    )
                                    .on_hover_text("Close editor tab");
                                let close = named_dynamic_author_id(
                                    close,
                                    format!("editor.tab.close.{}", tab_id.0),
                                    "Close editor tab",
                                );
                                if close.clicked() {
                                    close_tab = Some(*tab_id);
                                }
                            });
                        });
                    }
                });
            });
        if let Some(tab_id) = close_tab {
            self.request_editor_tab_close(key.clone(), tab_id, "editor.tab.discard");
            return;
        }

        if let Some(tab_id) = select
            && self
                .model
                .workspace_mut(key.clone())
                .select_editor_tab(tab_id)
                .is_ok()
        {
            self.editor_surface = EditorSurface::default();
        }

        let selected = self
            .model
            .workspace(key)
            .and_then(ProfileWorkspace::selected_editor_tab_id);
        let mut title = selected
            .and_then(|tab_id| {
                self.model
                    .workspace(key)
                    .and_then(|workspace| workspace.editor_tab(tab_id))
                    .map(|tab| tab.title().to_owned())
            })
            .unwrap_or_default();
        let selected_index = selected
            .and_then(|selected| tabs.iter().position(|(tab_id, _, _)| *tab_id == selected));
        let mut action = None;
        ui.horizontal_wrapped(|ui| {
            let rename = ui.add_enabled(
                enabled && selected.is_some(),
                egui::TextEdit::singleline(&mut title)
                    .id_salt("editor.tab.title")
                    .hint_text("Tab title")
                    .desired_width(160.0),
            );
            let rename = named_author_id(rename, "editor.tab.title", "Rename editor tab");
            if rename.changed() {
                action = selected.map(EditorTabAction::Rename);
            }
            let new = ui.add_enabled(
                enabled,
                egui::Button::new("New")
                    .min_size(egui::vec2(72.0, OpenAiTheme::MIN_CONTROL_HEIGHT)),
            );
            if named_author_id(new, "editor.tab.new", "New editor tab").clicked() {
                action = Some(EditorTabAction::New);
            }
            let duplicate = ui.add_enabled(
                enabled && selected.is_some(),
                egui::Button::new("Duplicate")
                    .min_size(egui::vec2(96.0, OpenAiTheme::MIN_CONTROL_HEIGHT)),
            );
            if named_author_id(duplicate, "editor.tab.duplicate", "Duplicate editor tab").clicked()
            {
                action = selected.map(EditorTabAction::Duplicate);
            }
            let move_left = ui.add_enabled(
                enabled && selected_index.is_some_and(|index| index > 0),
                egui::Button::new("←").min_size(egui::vec2(
                    OpenAiTheme::MIN_CONTROL_HEIGHT,
                    OpenAiTheme::MIN_CONTROL_HEIGHT,
                )),
            );
            if named_author_id(move_left, "editor.tab.move_left", "Move editor tab left").clicked()
            {
                action = selected.map(EditorTabAction::MoveLeft);
            }
            let move_right = ui.add_enabled(
                enabled && selected_index.is_some_and(|index| index.saturating_add(1) < tabs.len()),
                egui::Button::new("→").min_size(egui::vec2(
                    OpenAiTheme::MIN_CONTROL_HEIGHT,
                    OpenAiTheme::MIN_CONTROL_HEIGHT,
                )),
            );
            if named_author_id(move_right, "editor.tab.move_right", "Move editor tab right")
                .clicked()
            {
                action = selected.map(EditorTabAction::MoveRight);
            }
        });
        let workspace = self.model.workspace_mut(key.clone());
        let outcome = match action {
            Some(EditorTabAction::Rename(tab_id)) => workspace.rename_editor_tab(tab_id, title),
            Some(EditorTabAction::New) => {
                let title = format!("Query {}", workspace.editor_tabs().len().saturating_add(1));
                workspace
                    .create_editor_tab(profile.driver.language(), title, String::new())
                    .map(|_| ())
            }
            Some(EditorTabAction::Duplicate(tab_id)) => {
                workspace.duplicate_editor_tab(tab_id).map(|_| ())
            }
            Some(EditorTabAction::MoveLeft(tab_id)) => workspace
                .editor_tabs()
                .iter()
                .position(|tab| tab.id() == tab_id)
                .and_then(|index| index.checked_sub(1))
                .map_or(Ok(()), |target| {
                    workspace.reorder_editor_tab(tab_id, target)
                }),
            Some(EditorTabAction::MoveRight(tab_id)) => workspace
                .editor_tabs()
                .iter()
                .position(|tab| tab.id() == tab_id)
                .and_then(|index| index.checked_add(1))
                .filter(|target| *target < workspace.editor_tabs().len())
                .map_or(Ok(()), |target| {
                    workspace.reorder_editor_tab(tab_id, target)
                }),
            None => Ok(()),
        };
        if let Err(error) = outcome {
            self.model.status = error.to_string();
        }
        if action.is_some() {
            self.editor_surface = EditorSurface::default();
        }
    }

    fn show_workspace_persistence_controls(
        &mut self,
        ui: &mut egui::Ui,
        profile: &ProfileSnapshot,
        key: &WorkspaceKey,
    ) {
        let available = self.model.config.source_version() == ConfigSourceVersion::V3
            && profile.persisted.safety.instance_id().is_some();
        let persistence_enabled = self
            .model
            .workspace(key)
            .and_then(ProfileWorkspace::persistence)
            .is_none_or(ProfileWorkspacePersistence::persistence_enabled);
        let state = self.workspace_persistence.get(key);
        let read_only = state.is_some_and(WorkspacePersistenceState::is_read_only);
        let clear_failed =
            state.is_some_and(|state| matches!(state.clear, WorkspaceClearPhase::Failed { .. }));
        let restore_conflict =
            state.is_some_and(|state| matches!(state.load, WorkspaceLoadPhase::Conflict));
        let busy = state.is_some_and(|state| {
            matches!(state.load, WorkspaceLoadPhase::Loading { .. })
                || matches!(state.save, WorkspaceSavePhase::Saving { .. })
                || matches!(state.clear, WorkspaceClearPhase::Pending { .. })
        });
        let writes_blocked = busy || clear_failed;
        let save_failed = state.is_some_and(|state| {
            matches!(
                state.load,
                WorkspaceLoadPhase::Failed(_) | WorkspaceLoadPhase::Conflict
            ) || matches!(state.save, WorkspaceSavePhase::Failed { .. })
                || clear_failed
        });
        let retryable_failure = state.is_some_and(|state| {
            matches!(state.load, WorkspaceLoadPhase::Failed(_))
                || matches!(state.save, WorkspaceSavePhase::Failed { .. })
                || clear_failed
        });
        let saved = !self.frame_workspace_dirty_hint
            && self
                .model
                .workspace(key)
                .is_some_and(ProfileWorkspace::is_saved);
        let status = if !available {
            "Unavailable — migrate this profile to classified v3"
        } else if read_only {
            "Read-only — another app instance owns workspace persistence"
        } else if state
            .is_some_and(|state| matches!(state.load, WorkspaceLoadPhase::Loading { .. }))
        {
            "Loading"
        } else if state.is_some_and(|state| matches!(state.save, WorkspaceSavePhase::Saving { .. }))
        {
            "Saving"
        } else if clear_failed {
            "Clear failed"
        } else if restore_conflict {
            "Restore conflict — choose Keep local, Off, or Clear"
        } else if save_failed {
            "Save failed"
        } else if !persistence_enabled {
            "Off — local-only until quit"
        } else if saved {
            "Saved"
        } else {
            "Unsaved"
        };
        ui.add_space(4.0);
        let mut toggle = false;
        let mut save = false;
        let mut clear = false;
        let mut retry = false;
        ui.horizontal_wrapped(|ui| {
            let status_response = ui.small(format!("Private workspace · {status}"));
            named_dynamic_value_author_id(
                status_response,
                "workspace.persistence.status".to_owned(),
                "Private workspace persistence status".to_owned(),
                status.to_owned(),
            );
            let toggle_button = ui.add_enabled(
                available && !read_only && !writes_blocked,
                egui::Button::new(if persistence_enabled {
                    "Persistence On"
                } else {
                    "Persistence Off"
                })
                .selected(persistence_enabled)
                .min_size(egui::vec2(144.0, OpenAiTheme::MIN_CONTROL_HEIGHT)),
            );
            toggle = named_author_id(
                toggle_button,
                "workspace.persistence.toggle",
                "Toggle private draft and history persistence",
            )
            .clicked();
            let save_button = ui.add_enabled(
                available && persistence_enabled && !read_only && !writes_blocked,
                egui::Button::new(if restore_conflict {
                    "Keep local…"
                } else {
                    "Save"
                })
                .min_size(egui::vec2(88.0, OpenAiTheme::MIN_CONTROL_HEIGHT)),
            );
            save = named_author_id(
                save_button,
                "editor.save",
                if restore_conflict {
                    "Keep local workspace after restore conflict"
                } else {
                    "Save private workspace"
                },
            )
            .clicked();
            let clear_button = ui.add_enabled(
                available && !read_only && !writes_blocked,
                egui::Button::new("Clear saved data")
                    .min_size(egui::vec2(144.0, OpenAiTheme::MIN_CONTROL_HEIGHT)),
            );
            clear = named_author_id(
                clear_button,
                "workspace.persistence.clear",
                "Clear all saved drafts and history for this profile",
            )
            .clicked();
            if retryable_failure {
                let retry_button = ui.add_enabled(
                    available && !read_only && !busy,
                    egui::Button::new("Retry")
                        .min_size(egui::vec2(88.0, OpenAiTheme::MIN_CONTROL_HEIGHT)),
                );
                retry = named_author_id(
                    retry_button,
                    "workspace.persistence.retry",
                    "Retry private workspace persistence",
                )
                .clicked();
            }
        });
        if toggle {
            self.set_workspace_persistence_enabled(key, !persistence_enabled);
        } else if save {
            if restore_conflict {
                self.workspace_restore_conflict_confirmation = Some(key.clone());
            } else {
                let _ = self.request_selected_workspace_save();
            }
        } else if clear {
            self.workspace_clear_confirmation = Some(key.clone());
        } else if retry {
            if restore_conflict {
                self.workspace_restore_conflict_confirmation = Some(key.clone());
                return;
            }
            let retry_clear = self
                .workspace_persistence
                .get(key)
                .is_some_and(|state| matches!(state.clear, WorkspaceClearPhase::Failed { .. }));
            let retry_load = self
                .workspace_persistence
                .get(key)
                .is_some_and(|state| matches!(state.load, WorkspaceLoadPhase::Failed(_)));
            if retry_clear {
                self.submit_clear_workspace(key);
            } else if retry_load {
                if let Some(state) = self.workspace_persistence.get_mut(key) {
                    state.load = WorkspaceLoadPhase::Unloaded;
                    state.retry_not_before = None;
                }
                self.request_workspace_load(key);
            } else {
                let _ = self.submit_workspace_commit(key, true);
            }
        }
    }

    fn apply_profile_form_action(&mut self, action: FormAction) {
        match action {
            FormAction::Save { connect } => {
                let operation_id = self.model.next_operation();
                if let Some(editor) = self.profile_editor.as_mut() {
                    match editor.try_save_with_connect(&self.port, operation_id, connect) {
                        SaveAttempt::Submitted(_) => {
                            self.model.status = "Saving profile…".to_owned();
                        }
                        SaveAttempt::Invalid => {
                            self.model.status = "Fix the profile form".to_owned();
                        }
                        SaveAttempt::Busy => {
                            self.model.status = "Service is busy".to_owned();
                        }
                        SaveAttempt::Disconnected => {
                            self.model.status = "Service is unavailable".to_owned();
                        }
                        SaveAttempt::ConfigUncertain => {
                            self.model.status = "Reload profiles before saving.".to_owned();
                        }
                        SaveAttempt::AlreadyPending(_) => {
                            self.model.status = "Profile save is already pending".to_owned();
                        }
                    }
                }
            }
            FormAction::TestDraft => {
                let operation_id = self.model.next_operation();
                if let Some(editor) = self.profile_editor.as_mut() {
                    match editor.try_test_draft(&self.port, operation_id) {
                        DraftTestAttempt::Submitted(_) => {
                            self.model.status = "Testing draft connection…".to_owned();
                        }
                        DraftTestAttempt::Invalid | DraftTestAttempt::Unavailable => {
                            self.model.status = editor.status().to_owned();
                        }
                        DraftTestAttempt::Busy => {
                            self.model.status = "Service is busy".to_owned();
                        }
                        DraftTestAttempt::Disconnected => {
                            self.model.status = "Service is unavailable".to_owned();
                        }
                        DraftTestAttempt::ConfigUncertain => {
                            self.model.status = "Reload profiles before testing.".to_owned();
                        }
                        DraftTestAttempt::AlreadyPending(_) => {
                            self.model.status = "Profile work is already pending".to_owned();
                        }
                    }
                }
            }
            FormAction::ProbeEnvironment => {
                if let Some(editor) = self.profile_editor.as_mut() {
                    let availability = editor.probe_environment_availability();
                    self.model.status = format!("Environment credential: {availability:?}");
                }
            }
            FormAction::Cancel => self.profile_editor = None,
            FormAction::PickRedisCaFile => {
                if let Some(path) = native_redis_ca_file_picker()
                    && let Some(editor) = self.profile_editor.as_mut()
                {
                    editor.bind_redis_ca_file(path);
                    self.model.status = "Redis CA file selected".to_owned();
                }
            }
            FormAction::None => {}
        }
    }
}

impl RecoveryCommandDispatcher for DbotterApp {
    type Error = Infallible;

    fn dispatch(&mut self, command: RecoveryCommand) -> Result<(), Self::Error> {
        self.dispatch_recovery_command(command);
        Ok(())
    }
}

impl eframe::App for DbotterApp {
    fn logic(&mut self, context: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_events();
        let _ = self.ensure_current_workspace_persistence_bindings();
        let now = Instant::now();
        self.observe_workspace_revisions(now);
        if !self.discard_local_changes_on_close
            && self.workspace_close_guard != WorkspaceCloseGuard::SaveFailed
        {
            self.retry_workspace_loads(now);
            self.autosave_workspaces(now);
        }
        self.handle_workspace_close_request(context);
        context.request_repaint_after(Duration::from_millis(50));
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.show_native(ui);
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        if !self.discard_local_changes_on_close {
            let _ = self.flush_selected_workspace();
        }
        let mut retained = self
            .workspace_geometries
            .iter()
            .map(|(key, geometry)| (key.clone(), *geometry))
            .collect::<Vec<_>>();
        retained.sort_by(|left, right| {
            left.0
                .profile_id
                .0
                .cmp(&right.0.profile_id.0)
                .then_with(|| {
                    left.0
                        .profile_generation
                        .0
                        .cmp(&right.0.profile_generation.0)
                })
        });
        retained.truncate(MAX_RETAINED_WORKSPACE_GEOMETRIES);
        if let Ok(encoded) = serde_json::to_string(&retained)
            && encoded.len() <= MAX_WORKSPACE_GEOMETRY_STORAGE_BYTES
        {
            storage.set_string(WORKSPACE_GEOMETRY_STORAGE_KEY, encoded);
        }
    }

    fn auto_save_interval(&self) -> Duration {
        Duration::from_secs(2)
    }

    fn persist_egui_memory(&self) -> bool {
        false
    }
}

fn consume_command_key(ui: &egui::Ui, key: egui::Key, shift: bool) -> bool {
    ui.input_mut(|input| {
        let mut pressed = false;
        input.events.retain(|event| {
            let egui::Event::Key {
                key: event_key,
                pressed: key_pressed,
                repeat,
                modifiers,
                ..
            } = event
            else {
                return true;
            };
            let matches = *event_key == key
                && *key_pressed
                && !*repeat
                && modifiers.command
                && modifiers.shift == shift
                && !modifiers.alt;
            if matches {
                pressed = true;
            }
            !matches
        });
        pressed
    })
}

fn saturating_u128_to_u64(value: u128) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

fn saturating_usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

fn current_unix_time_ms() -> i64 {
    let milliseconds = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_millis(),
        Err(_) => return 0,
    };
    i64::try_from(milliseconds).unwrap_or(i64::MAX)
}

fn plan_workspace_snapshot_set(
    snapshots: Vec<ProfileWorkspaceSnapshot>,
) -> Result<WorkspaceSnapshotSet, WorkspaceRetentionError> {
    WorkspaceSnapshotSet::new_with_retention(snapshots)
}

fn history_failure_status(
    summary: PublicSummary,
    connection_outcome: ConnectionFailureOutcome,
) -> WorkspaceHistoryStatus {
    if connection_outcome == ConnectionFailureOutcome::Unknown
        || summary == PublicSummary::CommittedDurabilityUnknown
    {
        return WorkspaceHistoryStatus::OutcomeUnknown;
    }
    if summary == PublicSummary::OperationCancelled {
        return WorkspaceHistoryStatus::Cancelled;
    }
    let code = match summary {
        PublicSummary::CredentialRequired | PublicSummary::AuthenticationFailed => {
            WorkspaceHistoryCode::Authentication
        }
        PublicSummary::PermissionDenied => WorkspaceHistoryCode::Permission,
        PublicSummary::NetworkUnavailable | PublicSummary::TlsVerificationFailed => {
            WorkspaceHistoryCode::Network
        }
        PublicSummary::OperationTimedOut => WorkspaceHistoryCode::Timeout,
        PublicSummary::InvalidInput
        | PublicSummary::SyntaxRejected
        | PublicSummary::ConstraintRejected
        | PublicSummary::UnsupportedFeature
        | PublicSummary::ResourceBusy
        | PublicSummary::ResourceStale => WorkspaceHistoryCode::Admission,
        PublicSummary::ConfigWriteNotCommitted | PublicSummary::ExportFailed => {
            WorkspaceHistoryCode::Backend
        }
        PublicSummary::InternalFailure | PublicSummary::CommittedDurabilityUnknown => {
            WorkspaceHistoryCode::Internal
        }
        PublicSummary::OperationCancelled => WorkspaceHistoryCode::None,
    };
    WorkspaceHistoryStatus::Failed(code)
}

fn next_workspace_history_id(history: &[WorkspaceHistoryEntry], preferred: u64) -> Option<u64> {
    if preferred != 0 && history.iter().all(|entry| entry.id() != preferred) {
        return Some(preferred);
    }
    let search_bound = u64::try_from(MAX_HISTORY_ENTRIES_PER_PROFILE)
        .ok()?
        .saturating_add(1);
    (1..=search_bound).find(|candidate| history.iter().all(|entry| entry.id() != *candidate))
}

const fn workspace_history_status_label(status: WorkspaceHistoryStatus) -> &'static str {
    match status {
        WorkspaceHistoryStatus::Succeeded => "Succeeded",
        WorkspaceHistoryStatus::Failed(WorkspaceHistoryCode::None) => "Failed",
        WorkspaceHistoryStatus::Failed(WorkspaceHistoryCode::Admission) => "Failed · admission",
        WorkspaceHistoryStatus::Failed(WorkspaceHistoryCode::Authentication) => {
            "Failed · authentication"
        }
        WorkspaceHistoryStatus::Failed(WorkspaceHistoryCode::Permission) => "Failed · permission",
        WorkspaceHistoryStatus::Failed(WorkspaceHistoryCode::Network) => "Failed · network",
        WorkspaceHistoryStatus::Failed(WorkspaceHistoryCode::Timeout) => "Failed · timeout",
        WorkspaceHistoryStatus::Failed(WorkspaceHistoryCode::Backend) => "Failed · backend",
        WorkspaceHistoryStatus::Failed(WorkspaceHistoryCode::Internal) => "Failed · internal",
        WorkspaceHistoryStatus::Cancelled => "Cancelled",
        WorkspaceHistoryStatus::OutcomeUnknown => "Outcome unknown",
    }
}

const fn workspace_run_target_label(target: WorkspaceRunTarget) -> &'static str {
    match target {
        WorkspaceRunTarget::Current => "Current",
        WorkspaceRunTarget::Selection => "Selection",
        WorkspaceRunTarget::All => "All",
    }
}

fn workspace_history_date_label(completed_at_unix_ms: i64) -> String {
    DateTime::<Utc>::from_timestamp_millis(completed_at_unix_ms).map_or_else(
        || format!("UTC timestamp {completed_at_unix_ms}"),
        |completed| completed.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
    )
}

fn workspace_history_source_preview(source: &str) -> String {
    const MAX_PREVIEW_CHARACTERS: usize = 160;
    let mut preview = String::with_capacity(MAX_PREVIEW_CHARACTERS.saturating_add(1));
    let mut truncated = false;
    for (index, character) in source.chars().enumerate() {
        if index >= MAX_PREVIEW_CHARACTERS {
            truncated = true;
            break;
        }
        if character == '\n' || character == '\r' {
            preview.push(' ');
        } else {
            preview.push(character);
        }
    }
    if truncated {
        preview.push('…');
    }
    preview
}

fn show_workspace_splitter(
    ui: &mut egui::Ui,
    total_extent: f32,
    editor_extent: f32,
) -> Option<SplitLayout> {
    let strip_extent = NativeLayout::SPLITTER_ACCESSIBLE_HIT_EXTENT;
    let reset_width = 112.0;
    let gap = NativeLayout::ADJACENT_ACTION_GAP;
    let (strip_rect, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), strip_extent),
        egui::Sense::hover(),
    );
    let reset_rect = egui::Rect::from_min_max(
        egui::pos2(strip_rect.max.x - reset_width, strip_rect.min.y),
        strip_rect.max,
    );
    let handle_rect = egui::Rect::from_min_max(
        strip_rect.min,
        egui::pos2(
            (reset_rect.min.x - gap).max(strip_rect.min.x),
            strip_rect.max.y,
        ),
    );
    let handle = ui
        .interact(
            handle_rect,
            egui::Id::new("workspace.splitter.handle"),
            egui::Sense::click_and_drag(),
        )
        .on_hover_cursor(egui::CursorIcon::ResizeVertical);

    let minimum = NativeLayout::PANE_MIN_EXTENT;
    let maximum = (total_extent - NativeLayout::PANE_MIN_EXTENT).max(minimum);
    let editor_extent = editor_extent.clamp(minimum, maximum);
    let mut next_layout = SplitLayout::from_editor_extent(total_extent, editor_extent);
    if handle.dragged() {
        next_layout = SplitLayout::from_editor_extent(
            total_extent,
            editor_extent + ui.input(|input| input.pointer.delta().y),
        );
    }
    if handle.has_focus() {
        let keyboard_steps = ui.input_mut(|input| {
            let upward = input.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp);
            let downward = input.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown);
            i32::from(downward) - i32::from(upward)
        });
        if keyboard_steps != 0 {
            handle.request_focus();
            next_layout = SplitLayout::from_editor_extent(
                total_extent,
                editor_extent + keyboard_steps as f32 * NativeLayout::SPLITTER_KEYBOARD_STEP,
            );
        }
    }
    if handle.double_clicked() {
        next_layout = SplitLayout::reset(total_extent);
    }

    let value_for_accessibility = next_layout.editor_extent().unwrap_or_else(|| {
        if next_layout.editor_restore_label().is_some() {
            minimum
        } else {
            maximum
        }
    });
    handle.widget_info(|| {
        let mut info = egui::WidgetInfo::labeled(
            egui::WidgetType::ResizeHandle,
            true,
            "Resize editor and results",
        );
        info.value = Some(f64::from(value_for_accessibility));
        info
    });
    let handle = named_author_id(handle, "workspace.splitter", "Resize editor and results");
    handle.ctx.accesskit_node_builder(handle.id, |node| {
        node.set_min_numeric_value(f64::from(minimum));
        node.set_max_numeric_value(f64::from(maximum));
        node.set_numeric_value_step(f64::from(NativeLayout::SPLITTER_KEYBOARD_STEP));
        node.set_orientation(egui::accesskit::Orientation::Horizontal);
    });
    let rule_color = if handle.hovered() || handle.has_focus() {
        OpenAiTheme::color(OpenAiTheme::INK)
    } else {
        OpenAiTheme::color(OpenAiTheme::BOUNDARY)
    };
    let rule_y = handle_rect.center().y;
    ui.painter().line_segment(
        [
            egui::pos2(handle_rect.min.x, rule_y),
            egui::pos2(handle_rect.max.x, rule_y),
        ],
        egui::Stroke::new(1.0, rule_color),
    );

    let reset = ui.put(
        reset_rect,
        egui::Button::new("Reset split").min_size(egui::vec2(
            reset_width,
            NativeLayout::SPLITTER_ACCESSIBLE_HIT_EXTENT,
        )),
    );
    let reset = named_author_id(reset, "workspace.split.reset", "Reset split to 60/40");
    if reset.clicked() {
        next_layout = SplitLayout::reset(total_extent);
    }

    let changed = next_layout.editor_restore_label().is_some()
        || next_layout.subordinate_restore_label().is_some()
        || next_layout
            .editor_extent()
            .is_some_and(|next| (next - editor_extent).abs() > f32::EPSILON);
    changed.then_some(next_layout)
}

fn restore_workspace_geometries(
    storage: Option<&dyn eframe::Storage>,
) -> HashMap<WorkspaceKey, WorkspaceGeometry> {
    let Some(encoded) = storage.and_then(|storage| {
        storage
            .get_string(WORKSPACE_GEOMETRY_STORAGE_KEY)
            .filter(|encoded| encoded.len() <= MAX_WORKSPACE_GEOMETRY_STORAGE_BYTES)
    }) else {
        return HashMap::new();
    };
    let Ok(retained) = serde_json::from_str::<Vec<(WorkspaceKey, WorkspaceGeometry)>>(&encoded)
    else {
        return HashMap::new();
    };
    retained
        .into_iter()
        .take(MAX_RETAINED_WORKSPACE_GEOMETRIES)
        .map(|(key, geometry)| {
            let geometry = WorkspaceGeometry::restore(
                geometry.navigator_width(),
                geometry.editor_share().clamp(
                    WORKSPACE_EDITOR_COLLAPSED_SHARE,
                    WORKSPACE_RESULTS_COLLAPSED_SHARE,
                ),
                geometry.inspector_visible(),
            );
            (key, geometry)
        })
        .collect()
}

fn mysql_context_editor_title(schema: &str, relation: &str) -> String {
    const MAX_TITLE_BYTES: usize = 120;
    const ELLIPSIS: &str = "…";

    let mut title = format!("{schema}.{relation}");
    if title.len() <= MAX_TITLE_BYTES {
        return title;
    }
    let mut end = MAX_TITLE_BYTES - ELLIPSIS.len();
    while !title.is_char_boundary(end) {
        end -= 1;
    }
    title.truncate(end);
    title.push_str(ELLIPSIS);
    title
}

fn catalog_request_with_identity(
    request: CatalogRequest,
    identity: RequestIdentity,
) -> CatalogRequest {
    match request {
        CatalogRequest::Schemas {
            prefix,
            page_token,
            page_size,
            timeout,
            ..
        } => CatalogRequest::Schemas {
            identity,
            prefix,
            page_token,
            page_size,
            timeout,
        },
        CatalogRequest::Relations {
            schema,
            prefix,
            page_token,
            page_size,
            timeout,
            ..
        } => CatalogRequest::Relations {
            identity,
            schema,
            prefix,
            page_token,
            page_size,
            timeout,
        },
        CatalogRequest::Columns {
            schema,
            relation,
            prefix,
            page_token,
            page_size,
            timeout,
            ..
        } => CatalogRequest::Columns {
            identity,
            schema,
            relation,
            prefix,
            page_token,
            page_size,
            timeout,
        },
    }
}

fn connection_label(state: &ConnectionState) -> String {
    match state {
        ConnectionState::Disconnected => "Disconnected".to_owned(),
        ConnectionState::Pending(_) => "Connecting…".to_owned(),
        ConnectionState::Connected { elapsed_ms, .. } => {
            format!("Connected · {elapsed_ms} ms")
        }
        ConnectionState::NeedsCredential => "Credential required".to_owned(),
        ConnectionState::Failed { summary } => {
            format!("Failed · {}", summary.message())
        }
        ConnectionState::Closing => "Closing…".to_owned(),
    }
}

const fn profile_environment_label(environment: Option<ProfileEnvironment>) -> &'static str {
    match environment {
        Some(ProfileEnvironment::Production) => "PRODUCTION",
        Some(ProfileEnvironment::Development) => "Development",
        None => "Unclassified",
    }
}

const fn profile_access_label(access: ProfileAccess) -> &'static str {
    match access {
        ProfileAccess::ReadOnly => "Read-only",
        ProfileAccess::ReadWrite => "Read-write",
    }
}

const fn operation_kind_label(kind: OperationKind) -> &'static str {
    match kind {
        OperationKind::LoadConfiguration => "Load configuration",
        OperationKind::ReloadConfiguration => "Reload configuration",
        OperationKind::MigrateConfiguration => "Migrate configuration",
        OperationKind::CreateProfile => "Create profile",
        OperationKind::UpdateProfile => "Update profile",
        OperationKind::DeleteProfile => "Delete profile",
        OperationKind::TestDraftConnection => "Test draft connection",
        OperationKind::ConnectProfile => "Connect",
        OperationKind::DisconnectProfile => "Disconnect",
        OperationKind::ReconnectProfile => "Reconnect",
        OperationKind::ExecuteRead => "Execute read",
        OperationKind::ExecuteMutation => "Execute data change",
        OperationKind::BrowseMySql => "Browse MySQL",
        OperationKind::BrowseRedis => "Browse Redis",
        OperationKind::InspectRedis => "Inspect Redis",
        OperationKind::ExportResult => "Export result",
        OperationKind::ShutdownRuntime => "Shut down runtime",
    }
}

const fn environment_availability_label(availability: EnvironmentAvailability) -> &'static str {
    match availability {
        EnvironmentAvailability::Available => "Available",
        EnvironmentAvailability::Missing => "Missing",
        EnvironmentAvailability::Empty => "Empty",
    }
}

fn render_recovery_error(
    ui: &mut egui::Ui,
    surface: &'static str,
    visible: &VisibleError,
) -> Option<RecoveryAction> {
    let mut clicked = None;
    egui::Frame::new()
        .fill(egui::Color32::WHITE)
        .stroke(egui::Stroke::new(1.0, egui::Color32::BLACK))
        .corner_radius(egui::CornerRadius::ZERO)
        .inner_margin(egui::Margin::same(12))
        .show(ui, |ui| {
            ui.strong(visible.error.summary.message());
            ui.small(format!("Category: {:?}", visible.error.category));
            ui.add_space(8.0);
            ui.horizontal_wrapped(|ui| {
                for action in visible.error.recovery.as_slice() {
                    if visible.error.operation == OperationKind::ExecuteMutation
                        && matches!(action, RecoveryAction::Retry(_))
                    {
                        continue;
                    }
                    let label = recovery_action_label(action);
                    let response = ui.add(
                        egui::Button::new(label)
                            .min_size(egui::vec2(112.0, OpenAiTheme::MIN_CONTROL_HEIGHT)),
                    );
                    let response = named_dynamic_author_id(
                        response,
                        format!("recovery.{surface}.{}", recovery_action_slug(action)),
                        label,
                    );
                    if response.clicked() {
                        clicked = Some(action.clone());
                    }
                }
            });
        });
    clicked
}

const fn recovery_action_slug(action: &RecoveryAction) -> &'static str {
    match action {
        RecoveryAction::OpenCredentialPrompt(_) => "open_credential",
        RecoveryAction::EditDraft(_, _) => "edit_draft",
        RecoveryAction::EditProfile(_, _) => "edit_profile",
        RecoveryAction::Retry(_) => "retry",
        RecoveryAction::FocusEditor(_) => "focus_editor",
        RecoveryAction::FocusExecuteLimits(_) => "focus_limits",
        RecoveryAction::ReloadConfiguration => "reload",
        RecoveryAction::Reconnect(_) => "reconnect",
        RecoveryAction::CancelOperation(_) => "cancel",
        RecoveryAction::ClearCatalog(_) => "clear_catalog",
        RecoveryAction::RestartRedisScan(_) => "restart_redis_scan",
        RecoveryAction::ChooseExportDestination(_) => "choose_export",
        RecoveryAction::RevealExportDestination(_) => "reveal_export",
        RecoveryAction::RevealMigrationBackup => "reveal_backup",
        RecoveryAction::RestartApplication => "restart_app",
        RecoveryAction::DismissError(_) => "dismiss",
    }
}

const fn recovery_action_label(action: &RecoveryAction) -> &'static str {
    match action {
        RecoveryAction::OpenCredentialPrompt(_) => "Enter credential",
        RecoveryAction::EditDraft(_, _) => "Edit draft",
        RecoveryAction::EditProfile(_, _) => "Edit profile",
        RecoveryAction::Retry(_) => "Retry",
        RecoveryAction::FocusEditor(_) => "Review statement",
        RecoveryAction::FocusExecuteLimits(_) => "Review limits",
        RecoveryAction::ReloadConfiguration => "Reload profiles",
        RecoveryAction::Reconnect(_) => "Reconnect",
        RecoveryAction::CancelOperation(_) => "Cancel operation",
        RecoveryAction::ClearCatalog(_) => "Clear catalog",
        RecoveryAction::RestartRedisScan(_) => "Restart scan",
        RecoveryAction::ChooseExportDestination(_) => "Choose destination",
        RecoveryAction::RevealExportDestination(_) => "Show destination",
        RecoveryAction::RevealMigrationBackup => "Show backup",
        RecoveryAction::RestartApplication => "Restart Dbotter",
        RecoveryAction::DismissError(_) => "Dismiss",
    }
}

const fn submit_error_message(error: SubmitError) -> &'static str {
    match error {
        SubmitError::Busy => "The service queue is busy; try again.",
        SubmitError::Disconnected => "The service is unavailable.",
    }
}

const fn export_format_label(format: ExportFormat) -> &'static str {
    match format {
        ExportFormat::Csv => "CSV",
        ExportFormat::Tsv => "TSV",
        ExportFormat::Json => "JSON",
    }
}

#[cfg(target_os = "macos")]
const fn export_format_extension(format: ExportFormat) -> &'static str {
    match format {
        ExportFormat::Csv => "csv",
        ExportFormat::Tsv => "tsv",
        ExportFormat::Json => "json",
    }
}

#[cfg(target_os = "macos")]
fn native_export_destination(result_id: ResultId, format: ExportFormat) -> Option<PathBuf> {
    use std::ffi::{CStr, OsStr};
    use std::os::unix::ffi::OsStrExt as _;

    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSModalResponseOK, NSSavePanel};
    use objc2_foundation::NSString;

    let mtm = MainThreadMarker::new()?;
    let panel = NSSavePanel::savePanel(mtm);
    let title = NSString::from_str("Export result");
    let message = NSString::from_str("Choose where to save this result.");
    let suggested_name = NSString::from_str(&format!(
        "dbotter-result-{}.{}",
        result_id.0,
        export_format_extension(format)
    ));
    panel.setTitle(Some(&title));
    panel.setMessage(Some(&message));
    panel.setNameFieldStringValue(&suggested_name);
    panel.setCanCreateDirectories(true);
    if panel.runModal() != NSModalResponseOK {
        return None;
    }
    let url = panel.URL()?;
    if !url.isFileURL() {
        return None;
    }
    let representation = url.fileSystemRepresentation();
    // SAFETY: Foundation guarantees this pointer is a NUL-terminated file-system
    // representation that remains valid for the lifetime of `url`.
    let bytes = unsafe { CStr::from_ptr(representation.as_ptr()) }.to_bytes();
    Some(PathBuf::from(OsStr::from_bytes(bytes)))
}

#[cfg(not(target_os = "macos"))]
fn native_export_destination(_result_id: ResultId, _format: ExportFormat) -> Option<PathBuf> {
    None
}

#[cfg(target_os = "macos")]
fn native_reveal_file(path: &Path) -> bool {
    if !std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_file()) {
        return false;
    }
    std::process::Command::new("/usr/bin/open")
        .arg("-R")
        .arg(path)
        .spawn()
        .is_ok()
}

#[cfg(not(target_os = "macos"))]
fn native_reveal_file(_path: &Path) -> bool {
    false
}

#[cfg(target_os = "macos")]
fn native_redis_ca_file_picker() -> Option<PathBuf> {
    let output = std::process::Command::new("/usr/bin/osascript")
        .args([
            "-e",
            "POSIX path of (choose file with prompt \"Choose a Redis TLS CA file\")",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?;
    let value = value.trim_end_matches(['\r', '\n']);
    (!value.is_empty()).then(|| PathBuf::from(value))
}

#[cfg(not(target_os = "macos"))]
fn native_redis_ca_file_picker() -> Option<PathBuf> {
    None
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeSet, HashMap};
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use super::{
        ActiveOperation, ConnectionState, DbotterApp, MySqlExplorerIntent, PendingDelete,
        ProfileEditor, ProfileWorkspace, WorkspaceClearPhase, WorkspaceCloseGuard,
        WorkspaceLoadPhase, WorkspaceSavePhase,
    };
    use crate::config::ConfigSourceVersion;
    use crate::model::{
        CatalogLevel, CatalogNode, CatalogNodeIdentity, CatalogNodeKind, CatalogPage,
        CatalogRetainedCounts, Cell, Column, ConnectionProfile, CredentialMode, DraftId,
        DriverAvailability, DriverKind, ExportFormat, OperationId, OperationKind,
        OperationRecipeId, OverwritePolicy, ProfileAccess, ProfileEnvironment, ProfileFieldId,
        ProfileGeneration, ProfileId, ProfileInstanceId, ProfileSafetyPosture, PublicCode,
        PublicSummary, QueryLanguage, QueryResult, RedisKeyEntry, RedisKeyFilter, RedisKeyId,
        RedisKeyPage, RedisScanConsistency, RedisScanRequest, RedisTlsConfig, RequestIdentity,
        ResultId, ResultProvenance, ResultRetentionPolicy, ResultSnapshot, SessionGeneration,
        TlsMode,
    };
    use crate::public_error::{PublicOperationError, RecoveryAction, SafeContext};
    use crate::secrets::EnvironmentAvailability;
    use crate::service::SessionDisposition;
    use crate::ui::accessibility::{accesskit_author_node, assert_accesskit_value_confined};
    use crate::ui::adapter::{ServicePort, UiCommand, bounded_ports};
    use crate::ui::editor::{
        EditorCursor, EditorIntent, build_execute_all_intent, build_execute_intent,
    };
    use crate::ui::layout::{NativeLayout, Pane, WorkspaceGeometry};
    use crate::ui::model::{
        ConfigPresentation, ProfileSnapshot, ResultAreaTab, UiEvent, WorkspaceAction,
        WorkspaceFailureCode, WorkspaceKey,
    };
    use crate::ui::redis_explorer::RedisExplorerIntent;
    use crate::workspace::{
        MAX_HISTORY_ENTRIES_PER_PROFILE, MAX_HISTORY_SOURCE_BYTES, MAX_PROFILE_SHARD_BYTES,
        MAX_WORKSPACE_STORE_BYTES, WorkspaceHistoryEntry, WorkspaceHistoryStatus,
        WorkspaceRetentionError, WorkspaceRetentionLimit, WorkspaceRunTarget, WorkspaceStoreMode,
        WorkspaceStoreWarning, conservative_encoded_profile_bytes_for_test,
        encoded_profile_bytes_at_generation,
    };
    use eframe::egui::{self, Context, Event, Key, Modifiers, RawInput, accesskit};

    #[derive(Default)]
    struct MemoryStorage {
        values: HashMap<String, String>,
    }

    impl eframe::Storage for MemoryStorage {
        fn get_string(&self, key: &str) -> Option<String> {
            self.values.get(key).cloned()
        }

        fn set_string(&mut self, key: &str, value: String) {
            self.values.insert(key.to_owned(), value);
        }

        fn remove_string(&mut self, key: &str) {
            self.values.remove(key);
        }

        fn flush(&mut self) {}
    }

    fn profile(driver: DriverKind, availability: DriverAvailability) -> ProfileSnapshot {
        let persisted = ConnectionProfile {
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
            safety: crate::model::ProfileSafetyPosture::new(
                crate::model::ProfileEnvironment::Development,
                crate::model::ProfileAccess::ReadWrite,
            ),
            tls: TlsMode::Disabled,
            credential_mode: CredentialMode::None,
            secret_env: None,
            redis_tls: RedisTlsConfig::default(),
        };
        ProfileSnapshot {
            id: ProfileId("profile".to_owned()),
            generation: ProfileGeneration(1),
            name: "Profile".to_owned(),
            driver,
            endpoint: "mysql://127.0.0.1:3306".to_owned(),
            database: None,
            availability,
            planned_reason: None,
            has_current_session_secret: false,
            environment_availability: None,
            persisted,
        }
    }

    fn redis_profile(id: &str, generation: u64) -> ProfileSnapshot {
        let mut profile = profile(DriverKind::Redis, DriverAvailability::Ready);
        profile.id = ProfileId(id.to_owned());
        profile.generation = ProfileGeneration(generation);
        profile.name = id.to_owned();
        profile.endpoint = format!("redis://{id}:6379");
        profile.persisted.id = id.to_owned();
        profile.persisted.name = id.to_owned();
        profile
    }

    fn render_redis_explorer(app: &mut DbotterApp) {
        let context = Context::default();
        context.enable_accesskit();
        let _ = context.run_ui(RawInput::default(), |ui| app.explorer_contents(ui));
    }

    #[test]
    fn native_storage_round_trips_only_bounded_workspace_geometry_and_rejects_corruption() {
        let (ui, mut service) = bounded_ports(4);
        let mut app = DbotterApp::new(ui);
        assert!(service.try_next_command().is_some());
        let key = WorkspaceKey::new(ProfileId("geometry".to_owned()), ProfileGeneration(7));
        let geometry = WorkspaceGeometry::restore(360.0, 0.70, false);
        app.workspace_geometries.insert(key.clone(), geometry);

        let mut storage = MemoryStorage::default();
        eframe::App::save(&mut app, &mut storage);
        let encoded = storage
            .values
            .get(super::WORKSPACE_GEOMETRY_STORAGE_KEY)
            .expect("geometry storage value");
        assert!(encoded.len() <= super::MAX_WORKSPACE_GEOMETRY_STORAGE_BYTES);
        assert!(!encoded.contains("SELECT"));

        let (restored_ui, mut restored_service) = bounded_ports(4);
        let restored = DbotterApp::new_with_storage(restored_ui, Some(&storage));
        assert!(restored_service.try_next_command().is_some());
        assert_eq!(restored.workspace_geometries.get(&key), Some(&geometry));

        storage.values.insert(
            super::WORKSPACE_GEOMETRY_STORAGE_KEY.to_owned(),
            "not-json".to_owned(),
        );
        let (corrupt_ui, mut corrupt_service) = bounded_ports(4);
        let corrupt = DbotterApp::new_with_storage(corrupt_ui, Some(&storage));
        assert!(corrupt_service.try_next_command().is_some());
        assert!(corrupt.workspace_geometries.is_empty());
    }

    fn shell_author_ids(app: &mut DbotterApp, width: f32, height: f32) -> BTreeSet<String> {
        let context = Context::default();
        context.enable_accesskit();
        context
            .run_ui(
                RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(width, height),
                    )),
                    ..RawInput::default()
                },
                |ui| app.show_native(ui),
            )
            .platform_output
            .accesskit_update
            .expect("actual shell frame must emit AccessKit")
            .nodes
            .into_iter()
            .filter_map(|(_, node)| node.author_id().map(str::to_owned))
            .collect()
    }

    fn j2_classified_environment_profile() -> ProfileSnapshot {
        let mut profile = profile(DriverKind::MySql, DriverAvailability::Ready);
        profile.persisted.safety = ProfileSafetyPosture::classified(
            ProfileEnvironment::Development,
            ProfileAccess::ReadWrite,
            ProfileInstanceId::from_bytes([0x2a; 16]),
        );
        profile.persisted.credential_mode = CredentialMode::Environment;
        profile.persisted.secret_env = Some("DBOTTER_J2_PASSWORD".to_owned());
        profile.environment_availability = Some(EnvironmentAvailability::Available);
        profile
    }

    fn install_j2_profile(app: &mut DbotterApp, profile: &ProfileSnapshot) -> WorkspaceKey {
        let key = WorkspaceKey::new(profile.id.clone(), profile.generation);
        app.model.profiles = vec![profile.clone()];
        app.model.selected_profile = Some(profile.id.clone());
        app.model
            .active_generations
            .insert(profile.id.clone(), profile.generation);
        app.model.config = ConfigPresentation::for_source(
            ConfigSourceVersion::V3,
            &PathBuf::from("/private/tmp/dbotter-j2-config.toml"),
        );
        key
    }

    fn submitted_committed_bytes_or_stale_sentinel(
        app: &DbotterApp,
        operation_id: OperationId,
        generation: u64,
    ) -> u64 {
        app.workspace_persistence
            .values()
            .filter_map(|state| state.submitted_commit)
            .find(|submitted| submitted.operation_id == operation_id)
            .and_then(|submitted| {
                submitted
                    .accounting
                    .encoded_bytes_at_generation(generation)
                    .ok()
                    .map(|(_, committed_bytes)| committed_bytes)
            })
            .unwrap_or(1)
    }

    fn j2_profile_with_identity(id: &str, instance_byte: u8) -> ProfileSnapshot {
        let mut profile = j2_classified_environment_profile();
        profile.id = ProfileId(id.to_owned());
        profile.name = id.to_owned();
        profile.persisted.id = id.to_owned();
        profile.persisted.name = id.to_owned();
        profile.persisted.safety = ProfileSafetyPosture::classified(
            ProfileEnvironment::Development,
            ProfileAccess::ReadWrite,
            ProfileInstanceId::from_bytes([instance_byte; 16]),
        );
        profile
    }

    fn install_j2_profiles_ready(
        app: &mut DbotterApp,
        profiles: &[ProfileSnapshot],
    ) -> Vec<WorkspaceKey> {
        app.model.profiles = profiles.to_vec();
        app.model.selected_profile = profiles.first().map(|profile| profile.id.clone());
        app.model.config = ConfigPresentation::for_source(
            ConfigSourceVersion::V3,
            &PathBuf::from("/private/tmp/dbotter-j2-multi-profile.toml"),
        );
        profiles
            .iter()
            .map(|profile| {
                app.model
                    .active_generations
                    .insert(profile.id.clone(), profile.generation);
                let key = WorkspaceKey::new(profile.id.clone(), profile.generation);
                assert!(app.ensure_workspace_persistence_binding(&key, profile));
                if let Some(state) = app.workspace_persistence.get_mut(&key) {
                    state.load = WorkspaceLoadPhase::Ready;
                    state.mode = Some(WorkspaceStoreMode::ReadWrite);
                }
                let revision = app
                    .model
                    .workspace(&key)
                    .map_or(u64::MAX, ProfileWorkspace::revision);
                assert!(
                    app.model
                        .workspaces
                        .get_mut(&key)
                        .is_some_and(|workspace| workspace.mark_saved_if_revision(revision))
                );
                key
            })
            .collect()
    }

    fn j2_workspace_snapshot(
        profile: &ProfileSnapshot,
        persistence_enabled: bool,
        source: Option<&str>,
    ) -> crate::workspace::ProfileWorkspaceSnapshot {
        let geometry = crate::workspace::WorkspaceGeometrySnapshot::new(320.0, 0.65, true)
            .expect("valid J2 fixture geometry");
        let persistence = super::ProfileWorkspacePersistence::for_classified_profile(
            &profile.persisted,
            persistence_enabled,
            geometry,
            Vec::new(),
        )
        .expect("classified J2 fixture persistence");
        let mut workspace = ProfileWorkspace::default();
        workspace
            .bind_persistence(persistence)
            .expect("bind J2 fixture persistence");
        if let Some(source) = source {
            workspace
                .create_editor_tab(QueryLanguage::Sql, "Durable", source)
                .expect("J2 fixture tab");
        }
        workspace
            .to_persistence_snapshot()
            .expect("J2 fixture snapshot")
    }

    fn byte_boundary_snapshot(
        instance_byte: u8,
        tunable_ascii_bytes: usize,
        status: WorkspaceHistoryStatus,
    ) -> crate::workspace::ProfileWorkspaceSnapshot {
        const FULL_ESCAPE_ENTRIES: usize = 85;
        const TUNABLE_NUL_BYTES: usize = 16 * 1024;
        assert!(tunable_ascii_bytes <= MAX_HISTORY_SOURCE_BYTES.saturating_sub(TUNABLE_NUL_BYTES));
        let full_escape_source = "\0".repeat(MAX_HISTORY_SOURCE_BYTES);
        let mut history = (0..FULL_ESCAPE_ENTRIES)
            .map(|index| {
                WorkspaceHistoryEntry::new(
                    u64::try_from(index + 1).expect("byte-boundary id"),
                    &full_escape_source,
                    WorkspaceRunTarget::Current,
                    i64::try_from(index + 100).expect("byte-boundary timestamp"),
                    status,
                    1,
                    0,
                    0,
                    false,
                )
                .expect("byte-boundary history")
            })
            .collect::<Vec<_>>();
        let mut tunable = "\0".repeat(TUNABLE_NUL_BYTES);
        tunable.push_str(&"x".repeat(tunable_ascii_bytes));
        history.push(
            WorkspaceHistoryEntry::new(
                u64::try_from(FULL_ESCAPE_ENTRIES + 1).expect("tunable history id"),
                &tunable,
                WorkspaceRunTarget::Current,
                i64::try_from(FULL_ESCAPE_ENTRIES + 100).expect("tunable timestamp"),
                status,
                1,
                0,
                0,
                false,
            )
            .expect("tunable byte-boundary history"),
        );
        crate::workspace::ProfileWorkspaceSnapshot::new(
            ProfileInstanceId::from_bytes([instance_byte; 16]),
            ProfileId(format!("byte-boundary-{instance_byte:02x}")),
            true,
            Vec::new(),
            None,
            crate::workspace::WorkspaceGeometrySnapshot::new(320.0, 0.65, true)
                .expect("byte-boundary geometry"),
            history,
        )
        .expect("byte-boundary snapshot")
    }

    fn byte_boundary_snapshot_with_oldest_first(
        instance_byte: u8,
        tunable_ascii_bytes: usize,
        status: WorkspaceHistoryStatus,
    ) -> crate::workspace::ProfileWorkspaceSnapshot {
        let snapshot = byte_boundary_snapshot(instance_byte, tunable_ascii_bytes, status);
        let mut history = snapshot.history().to_vec();
        let first = history.first().expect("byte-boundary first history");
        history[0] = WorkspaceHistoryEntry::new(
            first.id(),
            first.source().expect("byte-boundary retained source"),
            first.target(),
            0,
            first.status(),
            first.duration_ms(),
            first.returned_rows(),
            first.affected_rows(),
            first.truncated(),
        )
        .expect("uniquely oldest byte-boundary history");
        crate::workspace::ProfileWorkspaceSnapshot::new(
            snapshot.instance_id(),
            snapshot.profile_id().clone(),
            snapshot.persistence_enabled(),
            snapshot.editor_tabs().to_vec(),
            snapshot.selected_editor_tab_id(),
            snapshot.geometry(),
            history,
        )
        .expect("oldest-first byte-boundary snapshot")
    }

    fn command_modifiers(shift: bool) -> Modifiers {
        #[cfg(target_os = "macos")]
        {
            Modifiers {
                shift,
                mac_cmd: true,
                command: true,
                ..Modifiers::default()
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            Modifiers {
                ctrl: true,
                shift,
                command: true,
                ..Modifiers::default()
            }
        }
    }

    fn command_key_event(key: Key, shift: bool) -> Event {
        Event::Key {
            key,
            physical_key: Some(key),
            pressed: true,
            repeat: false,
            modifiers: command_modifiers(shift),
        }
    }

    #[test]
    fn j2_red_actual_frame_exposes_durable_workspace_and_searchable_history_controls() {
        let (ui_port, mut service) = bounded_ports(4);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let profile = j2_classified_environment_profile();
        let key = install_j2_profile(&mut app, &profile);
        let workspace = app.model.workspace_mut(key);
        workspace
            .create_editor_tab(QueryLanguage::Sql, "Orders", "SELECT 1")
            .expect("first J2 draft");
        workspace
            .create_editor_tab(QueryLanguage::Sql, "Customers", "SELECT 2")
            .expect("second J2 draft");
        workspace.select_result_area_tab(ResultAreaTab::History);

        let ids = shell_author_ids(&mut app, 1440.0, 900.0);
        let missing = [
            "workspace.persistence.status",
            "workspace.persistence.toggle",
            "editor.save",
            "history.search",
            "history.clear",
        ]
        .into_iter()
        .filter(|id| !ids.contains(*id))
        .collect::<Vec<_>>();

        assert!(
            missing.is_empty(),
            "J2 durable-work controls are absent from the actual frame: {missing:?}"
        );
        assert!(
            !ids.contains("workspace.session-retention"),
            "the actual frame still advertises session-only loss on quit"
        );
        assert!(
            service.try_next_command().is_none(),
            "rendering persistence controls must not dispatch work"
        );
    }

    #[test]
    fn j2_red_actual_history_reopens_as_new_editor_without_dispatch() {
        let (ui_port, mut service) = bounded_ports(8);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let profile = j2_classified_environment_profile();
        let key = install_j2_profile(&mut app, &profile);
        assert!(app.ensure_workspace_persistence_binding(&key, &profile));
        if let Some(state) = app.workspace_persistence.get_mut(&key) {
            state.load = WorkspaceLoadPhase::Ready;
            state.mode = Some(WorkspaceStoreMode::ReadWrite);
        }
        let original_tab = app
            .model
            .workspace_mut(key.clone())
            .create_editor_tab(
                QueryLanguage::Sql,
                "Original",
                "SELECT j2_history_private_source",
            )
            .expect("original history source");
        let intent = build_execute_intent(
            &profile,
            app.model.workspace(&key).expect("J2 workspace"),
            EditorCursor::caret(0),
        )
        .expect("read execution intent");
        app.submit_editor_intent(EditorIntent::Execute(intent));
        let (provisional_commit, workspace_identity, provisional_revision) =
            match service.try_next_command() {
                Some(UiCommand::CommitWorkspace {
                    operation_id,
                    identity,
                    revision,
                    ..
                }) => (operation_id, identity, revision),
                command => panic!("J2 fixture must reserve private history, got {command:?}"),
            };
        let operation_id = match service.try_next_command() {
            Some(UiCommand::Execute {
                operation_id,
                editor_tab_id: Some(editor_tab_id),
                text,
                ..
            }) => {
                assert_eq!(editor_tab_id, original_tab);
                assert_eq!(text, "SELECT j2_history_private_source");
                operation_id
            }
            _ => panic!("J2 fixture must dispatch one read"),
        };
        assert!(service.try_emit(UiEvent::QueryFinished {
            operation_id,
            profile_id: profile.id.clone(),
            profile_generation: profile.generation,
            editor_tab_id: Some(original_tab),
            session_generation: SessionGeneration(7),
            result: result_snapshot_for_operation(
                &profile,
                "ephemeral-result-must-not-persist",
                operation_id,
                ResultId(731),
            ),
        }));
        app.poll_events();
        assert!(service.try_emit(UiEvent::WorkspaceCommitted {
            operation_id: provisional_commit,
            identity: workspace_identity.clone(),
            revision: provisional_revision,
            generation: 1,
            committed_bytes: submitted_committed_bytes_or_stale_sentinel(
                &app,
                provisional_commit,
                1,
            ),
            warnings: Vec::new(),
        }));
        app.poll_events();
        let (terminal_commit, terminal_revision) = match service.try_next_command() {
            Some(UiCommand::CommitWorkspace {
                operation_id,
                revision,
                ..
            }) => (operation_id, revision),
            command => panic!("J2 terminal history must be saved, got {command:?}"),
        };
        assert!(service.try_emit(UiEvent::WorkspaceCommitted {
            operation_id: terminal_commit,
            identity: workspace_identity,
            revision: terminal_revision,
            generation: 2,
            committed_bytes: submitted_committed_bytes_or_stale_sentinel(&app, terminal_commit, 2,),
            warnings: Vec::new(),
        }));
        app.poll_events();
        {
            let workspace = app.model.workspace_mut(key.clone());
            let history = workspace
                .persistence()
                .expect("bound private persistence")
                .history();
            assert_eq!(history.len(), 1);
            assert_eq!(history[0].id(), operation_id.0);
            assert_eq!(
                history[0].target(),
                crate::workspace::WorkspaceRunTarget::Current
            );
            let snapshot = workspace
                .to_persistence_snapshot()
                .expect("bounded private snapshot");
            let encoded = serde_json::to_string(&snapshot).expect("snapshot JSON");
            assert!(encoded.contains("j2_history_private_source"));
            assert!(
                !encoded.contains("ephemeral-result-must-not-persist"),
                "result payload must never enter durable history"
            );
        }
        app.model
            .workspace_mut(key.clone())
            .select_result_area_tab(ResultAreaTab::History);
        assert!(service.try_next_command().is_none());

        let context = Context::default();
        context.enable_accesskit();
        let render = |app: &mut DbotterApp, events: Vec<Event>| {
            context.run_ui(
                RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1440.0, 900.0),
                    )),
                    events,
                    ..RawInput::default()
                },
                |ui| app.show_native(ui),
            )
        };

        let initial = render(&mut app, Vec::new());
        let initial = initial
            .platform_output
            .accesskit_update
            .expect("history frame must emit AccessKit");
        let (search_id, _) = accesskit_author_node(&initial, "history.search");
        let _ = render(
            &mut app,
            vec![Event::AccessKitActionRequest(accesskit::ActionRequest {
                action: accesskit::Action::Focus,
                target_tree: accesskit::TreeId::ROOT,
                target_node: search_id,
                data: None,
            })],
        );
        let filtered = render(
            &mut app,
            vec![Event::Text("j2_history_private_source".to_owned())],
        );
        let filtered = filtered
            .platform_output
            .accesskit_update
            .expect("filtered history frame must emit AccessKit");
        let entry_author_id = format!("history.entry.{}", operation_id.0);
        let (entry_id, _) = accesskit_author_node(&filtered, &entry_author_id);
        let _ = render(
            &mut app,
            vec![Event::AccessKitActionRequest(accesskit::ActionRequest {
                action: accesskit::Action::Focus,
                target_tree: accesskit::TreeId::ROOT,
                target_node: entry_id,
                data: None,
            })],
        );
        let _ = render(
            &mut app,
            vec![Event::Key {
                key: Key::Enter,
                physical_key: Some(Key::Enter),
                pressed: true,
                repeat: false,
                modifiers: Modifiers::NONE,
            }],
        );

        let workspace = app.model.workspace(&key).expect("J2 workspace retained");
        assert_eq!(workspace.editor_tabs().len(), 2);
        assert_eq!(
            workspace
                .selected_editor_tab_id()
                .and_then(|tab_id| workspace.editor_tab(tab_id))
                .map(|tab| tab.text()),
            Some("SELECT j2_history_private_source")
        );
        assert!(
            service.try_next_command().is_none(),
            "opening history must create a draft with zero automatic network dispatch"
        );
    }

    #[test]
    fn j2_red_native_save_flushes_private_workspace_without_leaking_source_to_eframe_storage() {
        let (ui_port, mut service) = bounded_ports(4);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let profile = j2_classified_environment_profile();
        let key = install_j2_profile(&mut app, &profile);
        assert!(app.ensure_workspace_persistence_binding(&key, &profile));
        let persistence = app
            .workspace_persistence
            .get_mut(&key)
            .expect("J2 workspace persistence");
        persistence.load = WorkspaceLoadPhase::Ready;
        persistence.mode = Some(WorkspaceStoreMode::ReadWrite);
        app.model
            .workspace_mut(key)
            .create_editor_tab(
                QueryLanguage::Sql,
                "Private draft",
                "SELECT j2_private_source",
            )
            .expect("J2 draft");

        let mut storage = MemoryStorage::default();
        eframe::App::save(&mut app, &mut storage);
        let native_storage = storage
            .values
            .values()
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");

        assert!(
            !native_storage.contains("j2_private_source"),
            "query source must never be serialized into eframe native storage"
        );
        assert!(
            !eframe::App::persist_egui_memory(&app),
            "generic egui memory may retain private TextEdit contents and must stay disabled"
        );
        match service.try_next_command() {
            Some(UiCommand::CommitWorkspace { snapshot, .. }) => {
                assert!(snapshot.persistence_enabled());
                assert_eq!(snapshot.editor_tabs().len(), 1);
                assert_eq!(
                    snapshot.editor_tabs()[0].source(),
                    "SELECT j2_private_source"
                );
            }
            command => panic!(
                "native Save must enqueue only a bounded private-workspace commit, got {command:?}"
            ),
        }
        assert!(
            service.try_next_command().is_none(),
            "native Save must never route through a database command"
        );
    }

    #[test]
    fn persistence_off_commits_only_disabled_empty_state_then_keeps_later_edits_local() {
        let (ui_port, mut service) = bounded_ports(8);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let profile = j2_classified_environment_profile();
        let key = install_j2_profile(&mut app, &profile);
        app.model
            .workspace_mut(key.clone())
            .create_editor_tab(QueryLanguage::Sql, "Private", "SELECT off_fixture")
            .expect("private tab");
        assert!(app.ensure_workspace_persistence_binding(&key, &profile));

        app.set_workspace_persistence_enabled(&key, false);
        let (operation_id, identity, revision) = match service.try_next_command() {
            Some(UiCommand::CommitWorkspace {
                operation_id,
                identity,
                revision,
                snapshot,
            }) => {
                assert!(!snapshot.persistence_enabled());
                assert!(snapshot.editor_tabs().is_empty());
                assert!(snapshot.history().is_empty());
                (operation_id, identity, revision)
            }
            command => panic!("toggle Off must submit disabled-empty commit, got {command:?}"),
        };
        assert!(service.try_emit(UiEvent::WorkspaceCommitted {
            operation_id,
            identity,
            revision,
            generation: 1,
            committed_bytes: submitted_committed_bytes_or_stale_sentinel(&app, operation_id, 1,),
            warnings: Vec::new(),
        }));
        app.poll_events();
        assert!(
            app.model
                .workspace(&key)
                .and_then(|workspace| workspace.persistence())
                .is_some_and(|persistence| !persistence.persistence_enabled())
        );
        assert!(
            !app.workspace_persistence
                .get(&key)
                .is_some_and(|state| state.force_commit_until_success)
        );

        {
            let workspace = app.model.workspace_mut(key.clone());
            workspace.editor_text.push_str(" -- remains local");
            workspace
                .sync_selected_editor_tab_from_surface()
                .expect("bounded local edit");
        }
        let mut storage = MemoryStorage::default();
        eframe::App::save(&mut app, &mut storage);
        assert!(
            service.try_next_command().is_none(),
            "Persistence Off must never autosave or native-save local SQL"
        );

        assert!(
            service.try_emit(UiEvent::WorkspaceCommitted {
                operation_id,
                identity: app
                    .workspace_persistence
                    .get(&key)
                    .expect("bound state")
                    .identity
                    .clone(),
                revision,
                generation: 1,
                committed_bytes: submitted_committed_bytes_or_stale_sentinel(
                    &app,
                    operation_id,
                    1,
                ),
                warnings: Vec::new(),
            })
        );
        app.poll_events();
        assert!(
            service.try_next_command().is_none(),
            "an older duplicate commit terminal must not resurrect saving"
        );
    }

    #[test]
    fn clear_success_forces_a_durable_disabled_empty_commit_before_completion() {
        let (ui_port, mut service) = bounded_ports(8);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let profile = j2_classified_environment_profile();
        let key = install_j2_profile(&mut app, &profile);
        app.model
            .workspace_mut(key.clone())
            .create_editor_tab(QueryLanguage::Sql, "Private", "SELECT clear_fixture")
            .expect("private tab");
        assert!(app.ensure_workspace_persistence_binding(&key, &profile));
        if let Some(state) = app.workspace_persistence.get_mut(&key) {
            state.load = WorkspaceLoadPhase::Ready;
            state.mode = Some(WorkspaceStoreMode::ReadWrite);
        }

        app.submit_clear_workspace(&key);
        let (clear_operation, identity, base_revision) = match service.try_next_command() {
            Some(UiCommand::ClearWorkspace {
                operation_id,
                identity,
                base_revision,
            }) => (operation_id, identity, base_revision),
            command => {
                panic!("clear confirmation must submit exact ClearWorkspace, got {command:?}")
            }
        };
        assert!(service.try_emit(UiEvent::WorkspaceCleared {
            operation_id: clear_operation,
            identity: identity.clone(),
            base_revision,
        }));
        app.poll_events();

        let (commit_operation, commit_revision) = match service.try_next_command() {
            Some(UiCommand::CommitWorkspace {
                operation_id,
                identity: committed_identity,
                revision,
                snapshot,
            }) => {
                assert_eq!(committed_identity, identity);
                assert!(!snapshot.persistence_enabled());
                assert!(snapshot.editor_tabs().is_empty());
                assert!(snapshot.history().is_empty());
                (operation_id, revision)
            }
            command => panic!(
                "successful clear must force a durable disabled-empty commit, got {command:?}"
            ),
        };
        assert!(app.workspace_persistence.get(&key).is_some_and(|state| {
            state.force_commit_until_success
                && matches!(
                    state.save,
                    WorkspaceSavePhase::Saving {
                        operation_id,
                        revision,
                    } if operation_id == commit_operation && revision == commit_revision
                )
        }));
        assert!(
            service.try_emit(UiEvent::WorkspaceCommitted {
                operation_id: commit_operation,
                identity,
                revision: commit_revision,
                generation: 2,
                committed_bytes: submitted_committed_bytes_or_stale_sentinel(
                    &app,
                    commit_operation,
                    2,
                ),
                warnings: vec![WorkspaceStoreWarning::CorruptProfileQuarantined],
            })
        );
        app.poll_events();
        assert!(app.workspace_persistence.get(&key).is_some_and(|state| {
            !state.force_commit_until_success && matches!(state.save, WorkspaceSavePhase::Idle)
        }));
    }

    #[test]
    fn discard_local_changes_close_never_reflushes_private_sql_through_native_save() {
        let (ui_port, mut service) = bounded_ports(8);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let profile = j2_classified_environment_profile();
        let key = install_j2_profile(&mut app, &profile);
        assert!(app.ensure_workspace_persistence_binding(&key, &profile));
        app.model
            .workspace_mut(key.clone())
            .create_editor_tab(
                QueryLanguage::Sql,
                "Discarded",
                "SELECT discard_close_fixture",
            )
            .expect("discard fixture");
        let revision = app
            .model
            .workspace(&key)
            .map_or(0, ProfileWorkspace::revision);
        if let Some(state) = app.workspace_persistence.get_mut(&key) {
            state.load = WorkspaceLoadPhase::Ready;
            state.mode = Some(WorkspaceStoreMode::ReadWrite);
            state.save = WorkspaceSavePhase::Failed {
                revision,
                code: super::WorkspaceFailureCode::Unavailable,
            };
            state.dirty_since = Some(Instant::now() - Duration::from_secs(2));
            state.retry_not_before = None;
        }
        app.workspace_close_guard = WorkspaceCloseGuard::SaveFailed;

        let context = Context::default();
        context.enable_accesskit();
        let first = context.run_ui(RawInput::default(), |ui| app.show_native(ui));
        let first = first
            .platform_output
            .accesskit_update
            .expect("close recovery frame");
        let (discard_id, _) = accesskit_author_node(&first, "workspace.close.discard");
        let _ = context.run_ui(
            RawInput {
                events: vec![Event::AccessKitActionRequest(accesskit::ActionRequest {
                    action: accesskit::Action::Click,
                    target_tree: accesskit::TreeId::ROOT,
                    target_node: discard_id,
                    data: None,
                })],
                ..RawInput::default()
            },
            |ui| app.show_native(ui),
        );
        assert!(app.discard_local_changes_on_close);
        assert!(service.try_next_command().is_none());

        let mut close_input = RawInput::default();
        close_input
            .viewports
            .get_mut(&egui::ViewportId::ROOT)
            .expect("root viewport")
            .events
            .push(egui::ViewportEvent::Close);
        let mut frame = eframe::Frame::_new_kittest();
        let close_output = context.run_ui(close_input, |ui| {
            eframe::App::logic(&mut app, ui.ctx(), &mut frame);
        });
        let close_commands = &close_output
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .expect("root viewport output")
            .commands;
        assert!(
            !close_commands.contains(&egui::ViewportCommand::CancelClose),
            "the frame after explicit Discard must never cancel closing"
        );
        assert!(
            service.try_next_command().is_none(),
            "the frame after explicit Discard must not autosave or flush"
        );

        let mut storage = MemoryStorage::default();
        eframe::App::save(&mut app, &mut storage);
        assert!(service.try_next_command().is_none());
        assert!(
            storage
                .values
                .values()
                .all(|value| !value.contains("discard_close_fixture"))
        );

        app.workspace_close_guard = WorkspaceCloseGuard::SaveFailed;
        let ids = shell_author_ids(&mut app, 1440.0, 900.0);
        for id in [
            "workspace.close.retry",
            "workspace.close.discard",
            "workspace.close.cancel",
        ] {
            assert!(ids.contains(id), "close recovery is missing {id}");
        }
    }

    #[test]
    fn stale_profiles_loaded_cannot_retag_or_reload_workspace_persistence() {
        let (ui_port, mut service) = bounded_ports(8);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let profile = j2_classified_environment_profile();
        let config = ConfigPresentation::for_source(
            ConfigSourceVersion::V3,
            &PathBuf::from("/private/tmp/dbotter-j2-config.toml"),
        );
        assert!(service.try_emit(UiEvent::ProfilesLoaded {
            operation_id: OperationId(100),
            profiles: vec![profile.clone()],
            config: config.clone(),
        }));
        app.poll_events();
        let key = WorkspaceKey::new(profile.id.clone(), profile.generation);
        assert!(matches!(
            service.try_next_command(),
            Some(UiCommand::LoadWorkspace { .. })
        ));
        app.workspace_persistence
            .get_mut(&key)
            .expect("accepted profile load bound persistence")
            .load = WorkspaceLoadPhase::Unloaded;
        app.workspace_geometries
            .insert(key.clone(), WorkspaceGeometry::restore(333.0, 0.61, true));
        app.workspace_history_search
            .insert(key.clone(), "stable-search".to_owned());

        let mut stale = profile.clone();
        stale.generation = ProfileGeneration(profile.generation.0.saturating_add(1));
        assert!(service.try_emit(UiEvent::ProfilesLoaded {
            operation_id: OperationId(99),
            profiles: vec![stale.clone()],
            config,
        }));
        app.poll_events();

        assert_eq!(
            app.model.active_generation(&profile.id),
            Some(profile.generation)
        );
        assert!(app.workspace_persistence.contains_key(&key));
        assert!(app.workspace_geometries.contains_key(&key));
        assert_eq!(
            app.workspace_history_search.get(&key).map(String::as_str),
            Some("stable-search")
        );
        assert!(
            !app.workspace_persistence
                .contains_key(&WorkspaceKey::new(stale.id, stale.generation))
        );
        assert!(service.try_next_command().is_none());
    }

    #[test]
    fn same_instance_retag_reissues_pending_load_with_the_original_restore_fence() {
        let (ui_port, mut service) = bounded_ports(12);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let profile = j2_classified_environment_profile();
        let old_key = install_j2_profile(&mut app, &profile);
        assert!(app.ensure_workspace_persistence_binding(&old_key, &profile));
        app.request_workspace_load(&old_key);
        let (old_operation, old_identity, old_base_revision) = match service.try_next_command() {
            Some(UiCommand::LoadWorkspace {
                operation_id,
                identity,
                base_revision,
            }) => (operation_id, identity, base_revision),
            command => panic!("expected initial exact load, got {command:?}"),
        };
        app.workspace_clear_confirmation = Some(old_key.clone());
        app.workspace_history_focus = Some(old_key.clone());

        let mut refreshed = profile.clone();
        refreshed.generation = ProfileGeneration(profile.generation.0.saturating_add(1));
        let new_key = WorkspaceKey::new(refreshed.id.clone(), refreshed.generation);
        assert!(service.try_emit(UiEvent::ProfilesLoaded {
            operation_id: OperationId(100),
            profiles: vec![refreshed.clone()],
            config: ConfigPresentation::for_source(
                ConfigSourceVersion::V3,
                &PathBuf::from("/private/tmp/dbotter-j2-config.toml"),
            ),
        }));
        app.poll_events();
        let (new_operation, new_identity, new_base_revision) = match service.try_next_command() {
            Some(UiCommand::LoadWorkspace {
                operation_id,
                identity,
                base_revision,
            }) => (operation_id, identity, base_revision),
            command => panic!("retag must reissue exact LoadWorkspace, got {command:?}"),
        };
        assert_eq!(new_identity.profile_generation(), refreshed.generation);
        assert_eq!(new_identity.instance_id(), old_identity.instance_id());
        assert_eq!(new_base_revision, old_base_revision);
        assert!(!app.workspace_persistence.contains_key(&old_key));
        assert!(matches!(
            app.workspace_persistence
                .get(&new_key)
                .map(|state| &state.load),
            Some(WorkspaceLoadPhase::Loading { .. })
        ));
        assert_eq!(app.workspace_clear_confirmation.as_ref(), Some(&new_key));
        assert_eq!(app.workspace_history_focus.as_ref(), Some(&new_key));

        let durable = j2_workspace_snapshot(&refreshed, true, Some("SELECT durable_after_retag"));
        let durable_bytes = encoded_profile_bytes_at_generation(&durable, 1)
            .expect("durable retag bytes")
            .1;
        assert!(service.try_emit(UiEvent::WorkspaceLoaded {
            operation_id: old_operation,
            identity: old_identity,
            base_revision: old_base_revision,
            mode: WorkspaceStoreMode::ReadWrite,
            read_only_reason: None,
            generation: Some(1),
            committed_bytes: durable_bytes,
            snapshot: Some(Box::new(durable.clone())),
        }));
        app.poll_events();
        assert!(
            app.model
                .workspace(&new_key)
                .is_some_and(|workspace| workspace.editor_tabs().is_empty()),
            "old-generation load completion must be ignored"
        );

        assert!(service.try_emit(UiEvent::WorkspaceLoaded {
            operation_id: new_operation,
            identity: new_identity,
            base_revision: new_base_revision,
            mode: WorkspaceStoreMode::ReadWrite,
            read_only_reason: None,
            generation: Some(1),
            committed_bytes: durable_bytes,
            snapshot: Some(Box::new(durable)),
        }));
        app.poll_events();
        assert!(app.model.workspace(&new_key).is_some_and(|workspace| {
            workspace
                .editor_tabs()
                .iter()
                .any(|tab| tab.text() == "SELECT durable_after_retag")
                && workspace.is_saved()
        }));
        assert!(service.try_next_command().is_none());
    }

    #[test]
    fn same_instance_retag_reissues_pending_clear_and_blocks_every_commit_until_off_is_durable() {
        let (ui_port, mut service) = bounded_ports(16);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let profile = j2_classified_environment_profile();
        let old_key = install_j2_profile(&mut app, &profile);
        assert!(app.ensure_workspace_persistence_binding(&old_key, &profile));
        app.model
            .workspace_mut(old_key.clone())
            .create_editor_tab(QueryLanguage::Sql, "Private", "SELECT clear_retag_fixture")
            .expect("clear retag fixture");
        if let Some(state) = app.workspace_persistence.get_mut(&old_key) {
            state.load = WorkspaceLoadPhase::Ready;
            state.mode = Some(WorkspaceStoreMode::ReadWrite);
        }
        app.submit_clear_workspace(&old_key);
        let (old_clear_operation, old_identity, old_revision) = match service.try_next_command() {
            Some(UiCommand::ClearWorkspace {
                operation_id,
                identity,
                base_revision,
            }) => (operation_id, identity, base_revision),
            command => panic!("expected initial exact clear, got {command:?}"),
        };

        let mut refreshed = profile.clone();
        refreshed.generation = ProfileGeneration(profile.generation.0.saturating_add(1));
        let new_key = WorkspaceKey::new(refreshed.id.clone(), refreshed.generation);
        assert!(service.try_emit(UiEvent::ProfilesLoaded {
            operation_id: OperationId(200),
            profiles: vec![refreshed.clone()],
            config: ConfigPresentation::for_source(
                ConfigSourceVersion::V3,
                &PathBuf::from("/private/tmp/dbotter-j2-config.toml"),
            ),
        }));
        app.poll_events();
        let (new_clear_operation, new_identity, new_revision) = match service.try_next_command() {
            Some(UiCommand::ClearWorkspace {
                operation_id,
                identity,
                base_revision,
            }) => (operation_id, identity, base_revision),
            command => panic!("retag must reissue exact ClearWorkspace, got {command:?}"),
        };
        assert_eq!(new_identity.profile_generation(), refreshed.generation);
        assert_eq!(new_identity.instance_id(), old_identity.instance_id());
        assert!(matches!(
            app.workspace_persistence
                .get(&new_key)
                .map(|state| &state.clear),
            Some(WorkspaceClearPhase::Pending { .. })
        ));

        assert!(service.try_emit(UiEvent::WorkspaceCleared {
            operation_id: old_clear_operation,
            identity: old_identity,
            base_revision: old_revision,
        }));
        app.poll_events();
        assert!(!app.flush_selected_workspace());
        assert!(
            service.try_next_command().is_none(),
            "old clear terminal and every save path must stay blocked"
        );

        assert!(service.try_emit(UiEvent::WorkspaceCleared {
            operation_id: new_clear_operation,
            identity: new_identity.clone(),
            base_revision: new_revision,
        }));
        app.poll_events();
        let (commit_operation, commit_revision) = match service.try_next_command() {
            Some(UiCommand::CommitWorkspace {
                operation_id,
                identity,
                revision,
                snapshot,
            }) => {
                assert_eq!(identity, new_identity);
                assert!(!snapshot.persistence_enabled());
                assert!(snapshot.editor_tabs().is_empty());
                assert!(snapshot.history().is_empty());
                (operation_id, revision)
            }
            command => panic!("clear success must force durable Off, got {command:?}"),
        };
        assert!(
            service.try_emit(UiEvent::WorkspaceCommitted {
                operation_id: commit_operation,
                identity: new_identity,
                revision: commit_revision,
                generation: 2,
                committed_bytes: submitted_committed_bytes_or_stale_sentinel(
                    &app,
                    commit_operation,
                    2,
                ),
                warnings: Vec::new(),
            })
        );
        app.poll_events();
        assert!(
            app.workspace_persistence
                .get(&new_key)
                .is_some_and(|state| {
                    matches!(state.clear, WorkspaceClearPhase::Idle)
                        && matches!(state.load, WorkspaceLoadPhase::Ready)
                        && !state.force_commit_until_success
                        && state.restore_baseline_revision == Some(commit_revision)
                })
        );
        assert!(service.try_next_command().is_none());
    }

    #[test]
    fn command_save_is_workspace_only_and_is_gated_while_a_modal_is_open() {
        let (ui_port, mut service) = bounded_ports(8);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let profile = j2_classified_environment_profile();
        let key = install_j2_profile(&mut app, &profile);
        assert!(app.ensure_workspace_persistence_binding(&key, &profile));
        let persistence = app
            .workspace_persistence
            .get_mut(&key)
            .expect("J2 workspace persistence");
        persistence.load = WorkspaceLoadPhase::Ready;
        persistence.mode = Some(WorkspaceStoreMode::ReadWrite);
        app.model
            .workspace_mut(key.clone())
            .create_editor_tab(
                QueryLanguage::Sql,
                "Shortcut",
                "SELECT command_save_fixture",
            )
            .expect("shortcut tab");
        let context = Context::default();
        context.enable_accesskit();

        app.workspace_clear_confirmation = Some(key);
        let _ = context.run_ui(
            RawInput {
                events: vec![command_key_event(Key::S, false)],
                ..RawInput::default()
            },
            |ui| app.show_native(ui),
        );
        assert!(
            service.try_next_command().is_none(),
            "modal context must not consume Cmd-S into any persistence or database action"
        );

        app.workspace_clear_confirmation = None;
        let save_context = Context::default();
        save_context.enable_accesskit();
        let _ = save_context.run_ui(
            RawInput {
                events: vec![command_key_event(Key::S, false)],
                ..RawInput::default()
            },
            |ui| app.show_native(ui),
        );
        match service.try_next_command() {
            Some(UiCommand::CommitWorkspace { snapshot, .. }) => {
                assert_eq!(
                    snapshot.editor_tabs()[0].source(),
                    "SELECT command_save_fixture"
                );
            }
            command => panic!("Cmd-S must dispatch only CommitWorkspace, got {command:?}"),
        }
        assert!(service.try_next_command().is_none());
    }

    #[test]
    fn editor_and_title_mutations_replace_both_saved_labels_in_the_same_actual_frame() {
        let (ui_port, mut service) = bounded_ports(8);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let profile = j2_classified_environment_profile();
        let key = install_j2_profile(&mut app, &profile);
        assert!(app.ensure_workspace_persistence_binding(&key, &profile));
        app.model
            .workspace_mut(key.clone())
            .create_editor_tab(QueryLanguage::Sql, "Saved draft", "SELECT 1")
            .expect("saved editor fixture");
        let mark_saved = |app: &mut DbotterApp| {
            let revision = app
                .model
                .workspace(&key)
                .map_or(0, ProfileWorkspace::revision);
            assert!(
                app.model
                    .workspace_mut(key.clone())
                    .mark_saved_if_revision(revision)
            );
            let state = app
                .workspace_persistence
                .get_mut(&key)
                .expect("workspace persistence state");
            state.load = WorkspaceLoadPhase::Ready;
            state.mode = Some(WorkspaceStoreMode::ReadWrite);
            state.save = WorkspaceSavePhase::Idle;
            state.observed_revision = revision;
            state.dirty_since = None;
            app.model.status = "Private workspace Saved.".to_owned();
        };
        mark_saved(&mut app);

        let context = Context::default();
        context.enable_accesskit();
        let render = |app: &mut DbotterApp, events: Vec<Event>| {
            context.run_ui(
                RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1440.0, 900.0),
                    )),
                    events,
                    ..RawInput::default()
                },
                |ui| app.show_native(ui),
            )
        };
        let assert_unsaved_labels = |update: &egui::accesskit::TreeUpdate| {
            let (_, persistence) = accesskit_author_node(update, "workspace.persistence.status");
            assert_eq!(
                persistence.value(),
                Some("Unsaved"),
                "the private-workspace label must change in the mutation frame"
            );
            let (_, operation) = accesskit_author_node(update, "status.operation");
            assert_eq!(
                operation.value(),
                Some("Local workspace changes are Unsaved."),
                "the operation strip must not retain a stale Saved claim"
            );
        };

        let initial = render(&mut app, Vec::new())
            .platform_output
            .accesskit_update
            .expect("initial saved frame");
        let (editor_id, _) = accesskit_author_node(&initial, "editor.input");
        let _ = render(
            &mut app,
            vec![Event::AccessKitActionRequest(accesskit::ActionRequest {
                action: accesskit::Action::Focus,
                target_tree: accesskit::TreeId::ROOT,
                target_node: editor_id,
                data: None,
            })],
        );
        app.model.status = "Private workspace Saved.".to_owned();
        let text_frame = render(&mut app, vec![Event::Text(" /* same-frame */".to_owned())])
            .platform_output
            .accesskit_update
            .expect("editor mutation frame");
        assert_unsaved_labels(&text_frame);
        assert!(
            app.model
                .workspace(&key)
                .and_then(ProfileWorkspace::selected_editor_tab_id)
                .and_then(|tab_id| {
                    app.model
                        .workspace(&key)
                        .and_then(|workspace| workspace.editor_tab(tab_id))
                })
                .is_some_and(|tab| tab.text().contains("same-frame")),
            "the SQL mutation must be visible in the same frame under test"
        );
        assert_eq!(
            app.model.status, "Local workspace changes are Unsaved.",
            "the next frame must inherit truthful operation state"
        );
        let mut forced_close_storage = MemoryStorage::default();
        eframe::App::save(&mut app, &mut forced_close_storage);
        match service.try_next_command() {
            Some(UiCommand::CommitWorkspace { snapshot, .. }) => assert!(
                snapshot.editor_tabs()[0].source().contains("same-frame"),
                "a force-close flush must carry the exact SQL that was visibly marked Unsaved"
            ),
            command => panic!("force-close flush must save the visible revision, got {command:?}"),
        }

        mark_saved(&mut app);
        let title_frame = render(&mut app, Vec::new())
            .platform_output
            .accesskit_update
            .expect("title baseline frame");
        let (title_id, _) = accesskit_author_node(&title_frame, "editor.tab.title");
        let _ = render(
            &mut app,
            vec![Event::AccessKitActionRequest(accesskit::ActionRequest {
                action: accesskit::Action::Focus,
                target_tree: accesskit::TreeId::ROOT,
                target_node: title_id,
                data: None,
            })],
        );
        app.model.status = "Private workspace Saved.".to_owned();
        let title_mutation = render(&mut app, vec![Event::Text(" renamed".to_owned())])
            .platform_output
            .accesskit_update
            .expect("title mutation frame");
        assert_unsaved_labels(&title_mutation);
        assert!(
            app.model
                .workspace(&key)
                .and_then(ProfileWorkspace::selected_editor_tab_id)
                .and_then(|tab_id| {
                    app.model
                        .workspace(&key)
                        .and_then(|workspace| workspace.editor_tab(tab_id))
                })
                .is_some_and(|tab| tab.title().contains("renamed")),
            "the title mutation must be visible in the same frame under test"
        );

        mark_saved(&mut app);
        let selection_baseline = render(&mut app, Vec::new())
            .platform_output
            .accesskit_update
            .expect("selection baseline frame");
        let (editor_id, _) = accesskit_author_node(&selection_baseline, "editor.input");
        let _ = render(
            &mut app,
            vec![Event::AccessKitActionRequest(accesskit::ActionRequest {
                action: accesskit::Action::Focus,
                target_tree: accesskit::TreeId::ROOT,
                target_node: editor_id,
                data: None,
            })],
        );
        app.model.status = "Private workspace Saved.".to_owned();
        let selection_mutation = render(&mut app, vec![command_key_event(Key::A, false)])
            .platform_output
            .accesskit_update
            .expect("selection mutation frame");
        assert_unsaved_labels(&selection_mutation);
        assert!(
            app.model
                .workspace(&key)
                .is_some_and(|workspace| workspace.selection_character_range.is_some()),
            "the keyboard selection must be visible in the same frame under test"
        );

        mark_saved(&mut app);
        let geometry_baseline = render(&mut app, Vec::new())
            .platform_output
            .accesskit_update
            .expect("geometry baseline frame");
        let (splitter_id, _) = accesskit_author_node(&geometry_baseline, "workspace.splitter");
        let _ = render(
            &mut app,
            vec![Event::AccessKitActionRequest(accesskit::ActionRequest {
                action: accesskit::Action::Focus,
                target_tree: accesskit::TreeId::ROOT,
                target_node: splitter_id,
                data: None,
            })],
        );
        app.model.status = "Private workspace Saved.".to_owned();
        let geometry_mutation = render(
            &mut app,
            vec![Event::Key {
                key: Key::ArrowDown,
                physical_key: Some(Key::ArrowDown),
                pressed: true,
                repeat: false,
                modifiers: Modifiers::NONE,
            }],
        )
        .platform_output
        .accesskit_update
        .expect("geometry mutation frame");
        assert_unsaved_labels(&geometry_mutation);
        assert_ne!(
            app.workspace_geometries
                .get(&key)
                .map(|geometry| geometry.editor_share()),
            Some(NativeLayout::DEFAULT_EDITOR_SHARE),
            "the keyboard geometry mutation must be visible in the tested frame"
        );
        assert!(service.try_next_command().is_none());
    }

    #[test]
    fn every_workspace_modal_contains_focus_and_blocks_background_editor_input_and_execution() {
        let (ui_port, mut service) = bounded_ports(16);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let profile = j2_classified_environment_profile();
        let key = install_j2_profile(&mut app, &profile);
        assert!(app.ensure_workspace_persistence_binding(&key, &profile));
        let tab_id = app
            .model
            .workspace_mut(key.clone())
            .create_editor_tab(QueryLanguage::Sql, "Modal guard", "SELECT modal_guard")
            .expect("modal editor fixture");
        let revision = app
            .model
            .workspace(&key)
            .map_or(0, ProfileWorkspace::revision);
        {
            let state = app
                .workspace_persistence
                .get_mut(&key)
                .expect("workspace persistence state");
            state.load = WorkspaceLoadPhase::Ready;
            state.mode = Some(WorkspaceStoreMode::ReadWrite);
            state.observed_revision = revision;
        }

        let context = Context::default();
        context.enable_accesskit();
        let render = |app: &mut DbotterApp, events: Vec<Event>| {
            context.run_ui(
                RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1440.0, 900.0),
                    )),
                    events,
                    ..RawInput::default()
                },
                |ui| app.show_native(ui),
            )
        };
        let initial = render(&mut app, Vec::new())
            .platform_output
            .accesskit_update
            .expect("initial editor frame");
        let (editor_id, _) = accesskit_author_node(&initial, "editor.input");
        let _ = render(
            &mut app,
            vec![Event::AccessKitActionRequest(accesskit::ActionRequest {
                action: accesskit::Action::Focus,
                target_tree: accesskit::TreeId::ROOT,
                target_node: editor_id,
                data: None,
            })],
        );

        let source_before = app
            .model
            .workspace(&key)
            .and_then(|workspace| workspace.editor_tab(tab_id))
            .map(|tab| tab.text().to_owned())
            .expect("editor source");
        let caret_before = app
            .model
            .workspace(&key)
            .map(|workspace| {
                (
                    workspace.caret_character_index,
                    workspace.selection_character_range.clone(),
                )
            })
            .expect("editor cursor");
        let mut assert_blocked = |app: &mut DbotterApp, expected_focus: &str| {
            let output = render(
                app,
                vec![
                    Event::Text(" SHOULD_NOT_APPEAR".to_owned()),
                    command_key_event(Key::Enter, false),
                    command_key_event(Key::Enter, true),
                ],
            );
            let update = output
                .platform_output
                .accesskit_update
                .expect("modal frame must emit AccessKit");
            let focused_author = update.nodes.iter().find_map(|(node_id, node)| {
                (*node_id == update.focus)
                    .then(|| node.author_id())
                    .flatten()
            });
            assert_eq!(
                focused_author,
                Some(expected_focus),
                "keyboard focus must move into the active dialog"
            );
            let (connect_id, connect_node) = accesskit_author_node(&update, "connection.connect");
            assert!(
                connect_node.is_disabled(),
                "the navigator Connect action must be inert behind every modal"
            );
            let workspace = app.model.workspace(&key).expect("workspace retained");
            assert_eq!(
                workspace.editor_tab(tab_id).map(|tab| tab.text()),
                Some(source_before.as_str())
            );
            assert_eq!(
                (
                    workspace.caret_character_index,
                    workspace.selection_character_range.clone(),
                ),
                caret_before
            );
            assert_eq!(workspace.revision(), revision);
            assert!(
                app.active_modal_kind().is_some(),
                "{expected_focus} modal must remain active after blocked editor shortcuts"
            );
            assert!(
                service.try_next_command().is_none(),
                "modal input must dispatch neither current nor Run all"
            );
            let _ = render(
                app,
                vec![Event::AccessKitActionRequest(accesskit::ActionRequest {
                    action: accesskit::Action::Click,
                    target_tree: accesskit::TreeId::ROOT,
                    target_node: connect_id,
                    data: None,
                })],
            );
            let background_command = service.try_next_command();
            assert!(
                background_command.is_none(),
                "an AccessKit click on a navigator action behind {expected_focus} must dispatch nothing, got {background_command:?}"
            );
        };

        app.workspace_clear_confirmation = Some(key.clone());
        app.workspace_close_guard = WorkspaceCloseGuard::SaveFailed;
        let _ = render(&mut app, Vec::new());
        let stacked = render(&mut app, Vec::new())
            .platform_output
            .accesskit_update
            .expect("the highest-priority modal must emit AccessKit");
        let stacked_ids = stacked
            .nodes
            .iter()
            .filter_map(|(_, node)| node.author_id())
            .collect::<BTreeSet<_>>();
        assert!(stacked_ids.contains("workspace.close.cancel"));
        assert!(
            !stacked_ids.contains("workspace.persistence.clear.cancel")
                && !stacked_ids.contains("workspace.persistence.clear.confirm"),
            "only the highest-priority modal may be rendered"
        );
        app.workspace_close_guard = WorkspaceCloseGuard::Closed;
        let _ = render(&mut app, Vec::new());
        let revealed = render(&mut app, Vec::new())
            .platform_output
            .accesskit_update
            .expect("the queued modal must become active after the priority modal closes");
        let revealed_ids = revealed
            .nodes
            .iter()
            .filter_map(|(_, node)| node.author_id())
            .collect::<BTreeSet<_>>();
        assert!(revealed_ids.contains("workspace.persistence.clear.cancel"));
        assert!(
            !revealed_ids.contains("workspace.close.cancel"),
            "the closed priority modal must leave the accessibility tree"
        );
        app.workspace_clear_confirmation = None;

        app.workspace_clear_confirmation = Some(key.clone());
        assert_blocked(&mut app, "workspace.persistence.clear.cancel");
        let tabbed = render(
            &mut app,
            vec![Event::Key {
                key: Key::Tab,
                physical_key: Some(Key::Tab),
                pressed: true,
                repeat: false,
                modifiers: Modifiers::NONE,
            }],
        )
        .platform_output
        .accesskit_update
        .expect("Tab navigation must keep emitting the active modal");
        let tabbed_focus = tabbed.nodes.iter().find_map(|(node_id, node)| {
            (*node_id == tabbed.focus)
                .then(|| node.author_id())
                .flatten()
        });
        assert_eq!(
            tabbed_focus,
            Some("workspace.persistence.clear.confirm"),
            "Tab must advance within the modal instead of resetting to its default action"
        );
        app.workspace_clear_confirmation = None;

        app.workspace_persistence
            .get_mut(&key)
            .expect("workspace persistence state")
            .load = WorkspaceLoadPhase::Conflict;
        app.workspace_restore_conflict_confirmation = Some(key.clone());
        assert_blocked(&mut app, "workspace.conflict.cancel");
        app.workspace_restore_conflict_confirmation = None;
        app.workspace_persistence
            .get_mut(&key)
            .expect("workspace persistence state")
            .load = WorkspaceLoadPhase::Ready;

        app.workspace_close_guard = WorkspaceCloseGuard::SaveFailed;
        assert_blocked(&mut app, "workspace.close.cancel");
        app.workspace_close_guard = WorkspaceCloseGuard::Closed;

        app.open_delete_confirmation(&profile);
        assert_blocked(&mut app, "profile.delete.cancel");
        app.delete_confirmation = None;

        app.editor_discard_confirmation = Some(super::EditorDiscardConfirmation {
            workspace_key: key.clone(),
            tab_id,
            title: "Modal guard".to_owned(),
            discard_author_id: "editor.tab.discard",
        });
        assert_blocked(&mut app, "editor.tab.discard.cancel");
    }

    #[test]
    fn editor_focus_keeps_new_close_find_and_escape_shortcuts_operable() {
        let (ui_port, mut service) = bounded_ports(8);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let profile = j2_classified_environment_profile();
        let key = install_j2_profile(&mut app, &profile);
        app.model
            .workspace_mut(key.clone())
            .create_editor_tab(QueryLanguage::Sql, "One", String::new())
            .expect("first empty tab");
        let context = Context::default();
        context.enable_accesskit();
        let render = |app: &mut DbotterApp, events: Vec<Event>| {
            context.run_ui(
                RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1440.0, 900.0),
                    )),
                    events,
                    ..RawInput::default()
                },
                |ui| app.show_native(ui),
            )
        };
        let initial = render(&mut app, Vec::new());
        let initial = initial
            .platform_output
            .accesskit_update
            .expect("editor frame");
        let (editor_id, _) = accesskit_author_node(&initial, "editor.input");
        let _ = render(
            &mut app,
            vec![Event::AccessKitActionRequest(accesskit::ActionRequest {
                action: accesskit::Action::Focus,
                target_tree: accesskit::TreeId::ROOT,
                target_node: editor_id,
                data: None,
            })],
        );
        let _ = render(&mut app, vec![command_key_event(Key::T, false)]);
        assert_eq!(
            app.model
                .workspace(&key)
                .map(|workspace| workspace.editor_tabs().len()),
            Some(2),
            "Cmd-T must create a tab even while the actual editor has focus"
        );
        let _ = render(&mut app, vec![command_key_event(Key::W, false)]);
        assert_eq!(
            app.model
                .workspace(&key)
                .map(|workspace| workspace.editor_tabs().len()),
            Some(1),
            "Cmd-W must close the selected clean editor"
        );

        let _ = render(&mut app, vec![command_key_event(Key::F, true)]);
        assert_eq!(
            app.model
                .workspace(&key)
                .map(ProfileWorkspace::result_area_tab),
            Some(ResultAreaTab::History)
        );
        let found = render(&mut app, vec![command_key_event(Key::F, false)]);
        let found = found
            .platform_output
            .accesskit_update
            .expect("history find frame");
        let (history_search_id, _) = accesskit_author_node(&found, "history.search");
        assert_eq!(
            found.focus, history_search_id,
            "Cmd-F in History must focus the actual search field"
        );

        app.model.workspace_mut(key).pending_execute = Some(OperationId(707));
        let _ = render(
            &mut app,
            vec![Event::Key {
                key: Key::Escape,
                physical_key: Some(Key::Escape),
                pressed: true,
                repeat: false,
                modifiers: Modifiers::NONE,
            }],
        );
        assert!(matches!(
            service.try_next_command(),
            Some(UiCommand::CancelOperation {
                operation_id: OperationId(707)
            })
        ));
        assert!(service.try_next_command().is_none());
    }

    #[test]
    fn read_only_empty_baseline_is_clean_but_user_input_requires_close_recovery() {
        let (ui_port, mut service) = bounded_ports(4);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let profile = j2_classified_environment_profile();
        let key = install_j2_profile(&mut app, &profile);
        assert!(app.ensure_workspace_persistence_binding(&key, &profile));
        let empty_load_revision = {
            let workspace = app.model.workspace_mut(key.clone());
            let revision = workspace.revision();
            assert!(workspace.mark_saved_if_revision(revision));
            revision
        };
        if let Some(state) = app.workspace_persistence.get_mut(&key) {
            state.load = WorkspaceLoadPhase::Ready;
            state.mode = Some(WorkspaceStoreMode::ReadOnly);
            state.clean_empty_baseline_pending = Some(empty_load_revision);
        }
        app.model
            .workspace_mut(key.clone())
            .select_result_area_tab(ResultAreaTab::History);
        let context = Context::default();
        context.enable_accesskit();
        let output = context.run_ui(RawInput::default(), |ui| app.show_native(ui));
        let update = output
            .platform_output
            .accesskit_update
            .expect("read-only frame");
        let read_only_controls = [
            "workspace.persistence.toggle",
            "editor.save",
            "workspace.persistence.clear",
            "history.clear",
        ];
        let mut read_only_nodes = Vec::new();
        for author_id in read_only_controls {
            let (node_id, node) = accesskit_author_node(&update, author_id);
            assert!(
                node.is_disabled(),
                "{author_id} must be disabled in a read-only second instance"
            );
            read_only_nodes.push(node_id);
        }
        for node_id in read_only_nodes {
            let _ = context.run_ui(
                RawInput {
                    events: vec![Event::AccessKitActionRequest(accesskit::ActionRequest {
                        action: accesskit::Action::Click,
                        target_tree: accesskit::TreeId::ROOT,
                        target_node: node_id,
                        data: None,
                    })],
                    ..RawInput::default()
                },
                |ui| app.show_native(ui),
            );
        }
        assert!(
            service.try_next_command().is_none(),
            "rendering/clicking no read-only write control may dispatch"
        );
        assert!(
            app.model
                .workspace(&key)
                .is_some_and(ProfileWorkspace::is_saved),
            "automatic empty Query 1 is only a clean read-only baseline"
        );
        assert!(!app.has_uncommitted_workspace());

        {
            let workspace = app.model.workspace_mut(key.clone());
            workspace.editor_text = "SELECT read_only_local_edit".to_owned();
            workspace
                .sync_selected_editor_tab_from_surface()
                .expect("bounded local edit");
        }
        app.observe_workspace_revisions(Instant::now());
        assert!(app.has_uncommitted_workspace());
        assert!(app.has_workspace_save_failure());
        if let Some(state) = app.workspace_persistence.get_mut(&key) {
            state.save = WorkspaceSavePhase::Failed {
                revision: app
                    .model
                    .workspace(&key)
                    .map_or(0, ProfileWorkspace::revision),
                code: super::WorkspaceFailureCode::ReadOnly(
                    crate::workspace::WorkspaceReadOnlyReason::WriterBusy,
                ),
            };
        }
        app.workspace_close_guard = WorkspaceCloseGuard::SaveFailed;
        let ids = shell_author_ids(&mut app, 1440.0, 900.0);
        assert!(ids.contains("workspace.close.retry"));
        assert!(ids.contains("workspace.close.discard"));
    }

    #[test]
    fn unavailable_commit_backs_off_and_clear_failure_requires_exact_retry_or_explicit_close() {
        let (ui_port, mut service) = bounded_ports(8);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let profile = j2_classified_environment_profile();
        let key = install_j2_profile(&mut app, &profile);
        assert!(app.ensure_workspace_persistence_binding(&key, &profile));
        app.model
            .workspace_mut(key.clone())
            .create_editor_tab(QueryLanguage::Sql, "Clean", String::new())
            .expect("clean tab fixture");
        let identity = app
            .workspace_persistence
            .get(&key)
            .expect("persistence state")
            .identity
            .clone();
        let revision = app
            .model
            .workspace(&key)
            .map_or(0, ProfileWorkspace::revision);
        let submitted_snapshot = j2_workspace_snapshot(&profile, true, None);
        let submitted_accounting =
            crate::workspace::EncodedProfileByteAccounting::new(&submitted_snapshot)
                .expect("submitted fixture accounting");
        if let Some(state) = app.workspace_persistence.get_mut(&key) {
            state.load = WorkspaceLoadPhase::Ready;
            state.mode = Some(WorkspaceStoreMode::ReadWrite);
            state.save = WorkspaceSavePhase::Saving {
                operation_id: OperationId(801),
                revision,
            };
            state.submitted_commit = Some(super::SubmittedWorkspaceCommit {
                operation_id: OperationId(801),
                revision,
                accounting: submitted_accounting,
            });
        }
        app.handle_workspace_event(&UiEvent::WorkspaceOperationFailed {
            operation_id: OperationId(801),
            identity: identity.clone(),
            revision,
            action: super::WorkspaceAction::Commit,
            code: super::WorkspaceFailureCode::Unavailable,
        });
        assert!(
            app.workspace_persistence
                .get(&key)
                .is_some_and(|state| state.retry_not_before.is_some())
        );

        {
            let workspace = app.model.workspace_mut(key.clone());
            let current = workspace.revision();
            assert!(workspace.mark_saved_if_revision(current));
        }
        if let Some(state) = app.workspace_persistence.get_mut(&key) {
            state.save = WorkspaceSavePhase::Idle;
            state.clear = WorkspaceClearPhase::Pending {
                operation_id: OperationId(802),
                revision,
            };
            state.dirty_since = None;
            state.retry_not_before = None;
        }
        app.handle_workspace_event(&UiEvent::WorkspaceOperationFailed {
            operation_id: OperationId(802),
            identity: identity.clone(),
            revision,
            action: super::WorkspaceAction::Clear,
            code: super::WorkspaceFailureCode::Unavailable,
        });
        assert!(app.workspace_persistence.get(&key).is_some_and(|state| {
            matches!(
                state.clear,
                WorkspaceClearPhase::Failed {
                    revision: failed_revision,
                    code: super::WorkspaceFailureCode::Unavailable,
                } if failed_revision == revision
            ) && matches!(state.save, WorkspaceSavePhase::Idle)
        }));
        assert!(app.has_uncommitted_workspace());
        assert!(app.has_workspace_save_failure());

        app.model
            .workspace_mut(key.clone())
            .select_result_area_tab(ResultAreaTab::History);
        app.workspace_close_guard = WorkspaceCloseGuard::SaveFailed;
        let context = Context::default();
        context.enable_accesskit();
        let _ = context.run_ui(
            RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1440.0, 900.0),
                )),
                ..RawInput::default()
            },
            |ui| app.show_native(ui),
        );
        let output = context.run_ui(
            RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1440.0, 900.0),
                )),
                ..RawInput::default()
            },
            |ui| app.show_native(ui),
        );
        let update = output
            .platform_output
            .accesskit_update
            .expect("clear failure frame");
        for author_id in [
            "workspace.persistence.toggle",
            "editor.save",
            "workspace.persistence.clear",
            "history.clear",
        ] {
            let (_, node) = accesskit_author_node(&update, author_id);
            assert!(node.is_disabled(), "{author_id} must stay disabled");
        }
        let (_, retry) = accesskit_author_node(&update, "workspace.persistence.retry");
        assert!(
            retry.is_disabled(),
            "the background Retry must be inert while close recovery owns focus"
        );
        let (_, close_retry) = accesskit_author_node(&update, "workspace.close.retry");
        assert!(
            !close_retry.is_disabled(),
            "the close dialog must retain the exact clear Retry path: {close_retry:?}"
        );
        let (_, discard_close) = accesskit_author_node(&update, "workspace.close.discard");
        assert_eq!(
            discard_close.label(),
            Some("Keep durable saved data, abandon clearing, and close")
        );

        app.submit_clear_workspace(&key);
        assert!(matches!(
            service.try_next_command(),
            Some(UiCommand::ClearWorkspace {
                identity: retry_identity,
                base_revision: retry_revision,
                ..
            }) if retry_identity == identity && retry_revision == revision
        ));
        assert!(service.try_next_command().is_none());
    }

    #[test]
    fn permanent_commit_failure_blocks_autosave_until_a_new_revision_exists() {
        let (ui_port, mut service) = bounded_ports(8);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let profile = j2_classified_environment_profile();
        let key = install_j2_profile(&mut app, &profile);
        assert!(app.ensure_workspace_persistence_binding(&key, &profile));
        app.model
            .workspace_mut(key.clone())
            .create_editor_tab(
                QueryLanguage::Sql,
                "Permanent failure",
                "SELECT first_revision",
            )
            .expect("permanent failure fixture");
        {
            let state = app
                .workspace_persistence
                .get_mut(&key)
                .expect("workspace persistence state");
            state.load = WorkspaceLoadPhase::Ready;
            state.mode = Some(WorkspaceStoreMode::ReadWrite);
        }
        app.observe_workspace_revisions(Instant::now());
        assert!(app.submit_workspace_commit(&key, true));
        let (operation_id, identity, failed_revision) = match service.try_next_command() {
            Some(UiCommand::CommitWorkspace {
                operation_id,
                identity,
                revision,
                ..
            }) => (operation_id, identity, revision),
            command => panic!("expected the initial workspace commit, got {command:?}"),
        };
        app.handle_workspace_event(&UiEvent::WorkspaceOperationFailed {
            operation_id,
            identity,
            revision: failed_revision,
            action: super::WorkspaceAction::Commit,
            code: super::WorkspaceFailureCode::LimitExceeded,
        });
        assert!(app.workspace_persistence.get(&key).is_some_and(|state| {
            matches!(
                state.save,
                WorkspaceSavePhase::Failed {
                    revision,
                    code: super::WorkspaceFailureCode::LimitExceeded,
                } if revision == failed_revision
            ) && state.retry_not_before.is_none()
        }));

        for seconds in [2, 10, 60] {
            app.autosave_workspaces(Instant::now() + Duration::from_secs(seconds));
            assert!(
                service.try_next_command().is_none(),
                "a permanent failure for the same revision must not create an autosave loop"
            );
        }

        {
            let workspace = app.model.workspace_mut(key.clone());
            workspace.editor_text = "SELECT second_revision".to_owned();
            workspace
                .sync_selected_editor_tab_from_surface()
                .expect("bounded second revision");
        }
        let changed_at = Instant::now();
        app.observe_workspace_revisions(changed_at);
        app.autosave_workspaces(changed_at + Duration::from_secs(2));
        match service.try_next_command() {
            Some(UiCommand::CommitWorkspace {
                revision, snapshot, ..
            }) => {
                assert!(revision > failed_revision);
                assert_eq!(snapshot.editor_tabs()[0].source(), "SELECT second_revision");
            }
            command => panic!("a new revision must resume autosave once, got {command:?}"),
        }
        assert!(service.try_next_command().is_none());
    }

    #[test]
    fn workspace_load_is_exactly_fenced_and_renderer_never_wins_with_query_one() {
        let (ui_port, mut service) = bounded_ports(8);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let profile = j2_classified_environment_profile();
        let key = install_j2_profile(&mut app, &profile);
        assert!(app.ensure_workspace_persistence_binding(&key, &profile));
        app.request_workspace_load(&key);
        let (operation_id, identity, base_revision) = match service.try_next_command() {
            Some(UiCommand::LoadWorkspace {
                operation_id,
                identity,
                base_revision,
            }) => (operation_id, identity, base_revision),
            command => panic!("expected exact workspace load, got {command:?}"),
        };

        let _ = shell_author_ids(&mut app, 1440.0, 900.0);
        assert!(
            app.model
                .workspace(&key)
                .is_some_and(|workspace| workspace.editor_tabs().is_empty()),
            "renderer must not create Query 1 while an exact restore can still arrive"
        );
        assert!(service.try_next_command().is_none());

        let geometry = crate::workspace::WorkspaceGeometrySnapshot::new(320.0, 0.65, true)
            .expect("valid restored geometry");
        let persistence = super::ProfileWorkspacePersistence::for_classified_profile(
            &profile.persisted,
            true,
            geometry,
            Vec::new(),
        )
        .expect("classified fixture persistence");
        let mut restored = ProfileWorkspace::default();
        restored
            .bind_persistence(persistence)
            .expect("bind restored fixture");
        restored
            .create_editor_tab(QueryLanguage::Sql, "Restored", "SELECT restored_source")
            .expect("restored tab");
        let snapshot = restored
            .to_persistence_snapshot()
            .expect("restored snapshot");

        app.model
            .workspace_mut(key.clone())
            .create_editor_tab(QueryLanguage::Sql, "Local", "SELECT local_wins")
            .expect("local edit during load");
        let snapshot_bytes = encoded_profile_bytes_at_generation(&snapshot, 1)
            .expect("conflicting load bytes")
            .1;
        assert!(service.try_emit(UiEvent::WorkspaceLoaded {
            operation_id,
            identity,
            base_revision,
            mode: WorkspaceStoreMode::ReadWrite,
            read_only_reason: None,
            generation: Some(1),
            committed_bytes: snapshot_bytes,
            snapshot: Some(Box::new(snapshot)),
        }));
        app.poll_events();
        let workspace = app.model.workspace(&key).expect("local workspace");
        assert!(
            workspace
                .editor_tabs()
                .iter()
                .any(|tab| tab.text() == "SELECT local_wins")
        );
        assert!(
            workspace
                .editor_tabs()
                .iter()
                .all(|tab| tab.text() != "SELECT restored_source")
        );
    }

    #[test]
    fn failed_load_retry_never_overwrites_local_sql_or_commits_before_resolution() {
        let (ui_port, mut service) = bounded_ports(12);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let profile = j2_classified_environment_profile();
        let key = install_j2_profile(&mut app, &profile);
        assert!(app.ensure_workspace_persistence_binding(&key, &profile));
        app.request_workspace_load(&key);
        let (failed_operation, identity, failed_revision) = match service.try_next_command() {
            Some(UiCommand::LoadWorkspace {
                operation_id,
                identity,
                base_revision,
            }) => (operation_id, identity, base_revision),
            command => panic!("expected initial exact load, got {command:?}"),
        };
        assert!(service.try_emit(UiEvent::WorkspaceOperationFailed {
            operation_id: failed_operation,
            identity: identity.clone(),
            revision: failed_revision,
            action: super::WorkspaceAction::Load,
            code: super::WorkspaceFailureCode::Unavailable,
        }));
        app.poll_events();

        let context = Context::default();
        context.enable_accesskit();
        let output = context.run_ui(
            RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1440.0, 900.0),
                )),
                ..RawInput::default()
            },
            |ui| app.show_native(ui),
        );
        let update = output
            .platform_output
            .accesskit_update
            .expect("failed restore frame");
        let (_, input) = accesskit_author_node(&update, "editor.input");
        assert!(
            !input.supports_action(accesskit::Action::ReplaceSelectedText)
                && !input.supports_action(accesskit::Action::SetValue),
            "the actual editor must remain disabled while a restore can still arrive"
        );
        assert!(
            app.model
                .workspace(&key)
                .is_some_and(|workspace| workspace.editor_tabs().is_empty()),
            "transient load failure must not manufacture Query 1"
        );

        app.model
            .workspace_mut(key.clone())
            .create_editor_tab(QueryLanguage::Sql, "Local", "SELECT local_after_failure")
            .expect("local race fixture");
        if let Some(state) = app.workspace_persistence.get_mut(&key) {
            state.load = WorkspaceLoadPhase::Unloaded;
            state.retry_not_before = None;
        }
        app.request_workspace_load(&key);
        let (retry_operation, retry_revision) = match service.try_next_command() {
            Some(UiCommand::LoadWorkspace {
                operation_id,
                identity: retry_identity,
                base_revision,
            }) => {
                assert_eq!(retry_identity, identity);
                (operation_id, base_revision)
            }
            command => panic!("expected exact retry load, got {command:?}"),
        };
        assert!(retry_revision > failed_revision);
        let durable = j2_workspace_snapshot(&profile, true, Some("SELECT durable_before_failure"));
        let durable_bytes = encoded_profile_bytes_at_generation(&durable, 1)
            .expect("durable conflict bytes")
            .1;
        assert!(service.try_emit(UiEvent::WorkspaceLoaded {
            operation_id: retry_operation,
            identity,
            base_revision: retry_revision,
            mode: WorkspaceStoreMode::ReadWrite,
            read_only_reason: None,
            generation: Some(1),
            committed_bytes: durable_bytes,
            snapshot: Some(Box::new(durable)),
        }));
        app.poll_events();
        assert!(matches!(
            app.workspace_persistence.get(&key).map(|state| &state.load),
            Some(WorkspaceLoadPhase::Conflict)
        ));
        assert!(app.model.workspace(&key).is_some_and(|workspace| {
            workspace
                .editor_tabs()
                .iter()
                .any(|tab| tab.text() == "SELECT local_after_failure")
                && workspace
                    .editor_tabs()
                    .iter()
                    .all(|tab| tab.text() != "SELECT durable_before_failure")
        }));

        let revision_before_geometry = app
            .model
            .workspace(&key)
            .map_or(u64::MAX, ProfileWorkspace::revision);
        app.remember_workspace_geometry(Some(375.0), Some(0.71), Some(true));
        assert_eq!(
            app.model.workspace(&key).map(ProfileWorkspace::revision),
            Some(revision_before_geometry),
            "unresolved restore must gate persistence geometry mutation"
        );
        let mut storage = MemoryStorage::default();
        eframe::App::save(&mut app, &mut storage);
        assert!(!app.flush_selected_workspace());
        app.autosave_workspaces(Instant::now() + Duration::from_secs(2));
        assert!(
            service.try_next_command().is_none(),
            "native save, explicit save before confirmation, and autosave must all stay blocked"
        );

        let conflict_context = Context::default();
        conflict_context.enable_accesskit();
        let render = |app: &mut DbotterApp, events: Vec<Event>| {
            conflict_context.run_ui(
                RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1440.0, 900.0),
                    )),
                    events,
                    ..RawInput::default()
                },
                |ui| app.show_native(ui),
            )
        };
        let conflict = render(&mut app, Vec::new())
            .platform_output
            .accesskit_update
            .expect("restore conflict frame");
        let (keep_local_id, keep_local) = accesskit_author_node(&conflict, "editor.save");
        assert_eq!(
            keep_local.label(),
            Some("Keep local workspace after restore conflict")
        );
        assert!(!keep_local.is_disabled());
        let _ = render(
            &mut app,
            vec![Event::AccessKitActionRequest(accesskit::ActionRequest {
                action: accesskit::Action::Click,
                target_tree: accesskit::TreeId::ROOT,
                target_node: keep_local_id,
                data: None,
            })],
        );
        assert!(service.try_next_command().is_none());
        let confirmation = render(&mut app, Vec::new())
            .platform_output
            .accesskit_update
            .expect("conflict confirmation frame");
        let (confirm_id, confirm_node) =
            accesskit_author_node(&confirmation, "workspace.conflict.confirm");
        assert_eq!(
            confirm_node.label(),
            Some("Replace prior saved tabs and history with the local workspace")
        );
        let _ = render(
            &mut app,
            vec![Event::AccessKitActionRequest(accesskit::ActionRequest {
                action: accesskit::Action::Click,
                target_tree: accesskit::TreeId::ROOT,
                target_node: confirm_id,
                data: None,
            })],
        );
        let (first_commit, first_commit_revision, first_commit_identity) =
            match service.try_next_command() {
                Some(UiCommand::CommitWorkspace {
                    operation_id,
                    identity,
                    revision,
                    snapshot,
                }) => {
                    assert!(
                        snapshot
                            .editor_tabs()
                            .iter()
                            .any(|tab| { tab.source() == "SELECT local_after_failure" })
                    );
                    assert!(
                        snapshot
                            .editor_tabs()
                            .iter()
                            .all(|tab| { tab.source() != "SELECT durable_before_failure" })
                    );
                    (operation_id, revision, identity)
                }
                command => panic!("confirmed Keep local must commit once, got {command:?}"),
            };
        assert!(service.try_emit(UiEvent::WorkspaceOperationFailed {
            operation_id: first_commit,
            identity: first_commit_identity.clone(),
            revision: first_commit_revision,
            action: super::WorkspaceAction::Commit,
            code: super::WorkspaceFailureCode::Unavailable,
        }));
        app.poll_events();
        assert!(app.workspace_persistence.get(&key).is_some_and(|state| {
            matches!(state.load, WorkspaceLoadPhase::Conflict)
                && matches!(state.save, WorkspaceSavePhase::Failed { .. })
                && !state.resolve_conflict_on_commit
        }));

        let failed = render(&mut app, Vec::new())
            .platform_output
            .accesskit_update
            .expect("failed Keep local frame");
        let (retry_id, retry) = accesskit_author_node(&failed, "workspace.persistence.retry");
        assert!(!retry.is_disabled());
        let _ = render(
            &mut app,
            vec![Event::AccessKitActionRequest(accesskit::ActionRequest {
                action: accesskit::Action::Click,
                target_tree: accesskit::TreeId::ROOT,
                target_node: retry_id,
                data: None,
            })],
        );
        assert!(service.try_next_command().is_none());
        let reconfirm = render(&mut app, Vec::new())
            .platform_output
            .accesskit_update
            .expect("reconfirm Keep local frame");
        let (reconfirm_id, _) = accesskit_author_node(&reconfirm, "workspace.conflict.confirm");
        let _ = render(
            &mut app,
            vec![Event::AccessKitActionRequest(accesskit::ActionRequest {
                action: accesskit::Action::Click,
                target_tree: accesskit::TreeId::ROOT,
                target_node: reconfirm_id,
                data: None,
            })],
        );
        let (successful_commit, successful_revision) = match service.try_next_command() {
            Some(UiCommand::CommitWorkspace {
                operation_id,
                identity,
                revision,
                ..
            }) => {
                assert_eq!(identity, first_commit_identity);
                (operation_id, revision)
            }
            command => panic!("reconfirmed Keep local must retry exact commit, got {command:?}"),
        };
        assert!(service.try_emit(UiEvent::WorkspaceCommitted {
            operation_id: successful_commit,
            identity: first_commit_identity,
            revision: successful_revision,
            generation: 3,
            committed_bytes: submitted_committed_bytes_or_stale_sentinel(
                &app,
                successful_commit,
                3,
            ),
            warnings: Vec::new(),
        }));
        app.poll_events();
        assert!(app.workspace_persistence.get(&key).is_some_and(|state| {
            matches!(state.load, WorkspaceLoadPhase::Ready)
                && matches!(state.save, WorkspaceSavePhase::Idle)
                && !state.resolve_conflict_on_commit
                && state.restore_baseline_revision == Some(successful_revision)
        }));
        assert!(
            app.model
                .workspace(&key)
                .is_some_and(ProfileWorkspace::is_saved)
        );
        assert!(service.try_next_command().is_none());
    }

    #[test]
    fn preexisting_local_draft_is_never_pristine_and_empty_store_response_autosaves_it() {
        let (ui_port, mut service) = bounded_ports(8);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let profile = j2_classified_environment_profile();
        let key = install_j2_profile(&mut app, &profile);
        app.model
            .workspace_mut(key.clone())
            .create_editor_tab(
                QueryLanguage::Sql,
                "Migrated local",
                "SELECT local_before_v3_binding",
            )
            .expect("preexisting local draft");
        assert!(app.ensure_workspace_persistence_binding(&key, &profile));
        assert!(
            app.workspace_persistence
                .get(&key)
                .is_some_and(|state| { state.restore_baseline_revision.is_none() })
        );
        app.request_workspace_load(&key);
        let (operation_id, identity, base_revision) = match service.try_next_command() {
            Some(UiCommand::LoadWorkspace {
                operation_id,
                identity,
                base_revision,
            }) => (operation_id, identity, base_revision),
            command => panic!("expected exact load, got {command:?}"),
        };
        assert!(service.try_emit(UiEvent::WorkspaceLoaded {
            operation_id,
            identity,
            base_revision,
            mode: WorkspaceStoreMode::ReadWrite,
            read_only_reason: None,
            generation: None,
            committed_bytes: 0,
            snapshot: None,
        }));
        app.poll_events();
        assert!(app.workspace_persistence.get(&key).is_some_and(|state| {
            matches!(state.load, WorkspaceLoadPhase::Ready)
                && matches!(state.save, WorkspaceSavePhase::Saving { .. })
                && state.dirty_since.is_none()
                && state.restore_baseline_revision.is_none()
        }));
        app.autosave_workspaces(Instant::now() + Duration::from_secs(2));
        match service.try_next_command() {
            Some(UiCommand::CommitWorkspace { snapshot, .. }) => {
                assert!(
                    snapshot
                        .editor_tabs()
                        .iter()
                        .any(|tab| { tab.source() == "SELECT local_before_v3_binding" })
                );
            }
            command => {
                panic!("empty durable response must autosave the local draft, got {command:?}")
            }
        }
        assert!(service.try_next_command().is_none());
    }

    #[test]
    fn exact_empty_load_marks_only_the_first_automatic_blank_baseline_clean() {
        let (ui_port, mut service) = bounded_ports(8);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let profile = j2_classified_environment_profile();
        let key = install_j2_profile(&mut app, &profile);
        assert!(app.ensure_workspace_persistence_binding(&key, &profile));
        app.request_workspace_load(&key);
        let (operation_id, identity, base_revision) = match service.try_next_command() {
            Some(UiCommand::LoadWorkspace {
                operation_id,
                identity,
                base_revision,
            }) => (operation_id, identity, base_revision),
            command => panic!("expected exact workspace load, got {command:?}"),
        };
        assert!(service.try_emit(UiEvent::WorkspaceLoaded {
            operation_id,
            identity,
            base_revision,
            mode: WorkspaceStoreMode::ReadWrite,
            read_only_reason: None,
            generation: None,
            committed_bytes: 0,
            snapshot: None,
        }));
        app.poll_events();

        let _ = shell_author_ids(&mut app, 1440.0, 900.0);
        let first_blank = app
            .model
            .workspace(&key)
            .and_then(ProfileWorkspace::selected_editor_tab_id)
            .expect("exact empty load creates one automatic baseline");
        assert!(
            app.model
                .workspace(&key)
                .is_some_and(ProfileWorkspace::is_saved),
            "the exact empty load may mark its one automatic blank baseline clean"
        );
        assert!(
            app.workspace_persistence
                .get(&key)
                .is_some_and(|state| { state.clean_empty_baseline_pending.is_none() })
        );

        app.request_editor_tab_close(key.clone(), first_blank, "editor.tab.discard");
        assert!(
            app.model
                .workspace(&key)
                .is_some_and(|workspace| workspace.editor_tabs().is_empty())
        );
        let _ = shell_author_ids(&mut app, 1440.0, 900.0);
        assert!(
            app.model
                .workspace(&key)
                .is_some_and(|workspace| !workspace.is_saved()),
            "a later last-tab close must leave its replacement blank tab Unsaved"
        );

        assert!(app.flush_selected_workspace());
        match service.try_next_command() {
            Some(UiCommand::CommitWorkspace { snapshot, .. }) => {
                assert_eq!(snapshot.editor_tabs().len(), 1);
                assert!(snapshot.editor_tabs()[0].source().is_empty());
            }
            command => {
                panic!("last-tab deletion must commit its blank replacement, got {command:?}")
            }
        }
        assert!(service.try_next_command().is_none());
    }

    #[test]
    fn exact_restored_geometry_resets_existing_panel_state_before_next_frame() {
        let (ui_port, mut service) = bounded_ports(8);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let profile = j2_classified_environment_profile();
        let key = install_j2_profile(&mut app, &profile);
        assert!(app.ensure_workspace_persistence_binding(&key, &profile));
        app.request_workspace_load(&key);
        let (operation_id, identity, base_revision) = match service.try_next_command() {
            Some(UiCommand::LoadWorkspace {
                operation_id,
                identity,
                base_revision,
            }) => (operation_id, identity, base_revision),
            command => panic!("expected exact workspace load, got {command:?}"),
        };

        let context = Context::default();
        context.enable_accesskit();
        let render = |app: &mut DbotterApp| {
            context.run_ui(
                RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1440.0, 900.0),
                    )),
                    ..RawInput::default()
                },
                |ui| app.show_native(ui),
            )
        };
        let _ = render(&mut app);
        assert_eq!(
            app.workspace_geometries
                .get(&key)
                .map(|geometry| geometry.navigator_width()),
            Some(NativeLayout::NAVIGATOR_DEFAULT_WIDTH)
        );
        assert_eq!(app.compact_workspace.as_ref(), Some(&key));

        let restored_geometry = crate::workspace::WorkspaceGeometrySnapshot::new(360.0, 0.67, true)
            .expect("valid private geometry");
        let persistence = super::ProfileWorkspacePersistence::for_classified_profile(
            &profile.persisted,
            true,
            restored_geometry,
            Vec::new(),
        )
        .expect("classified persistence");
        let mut restored = ProfileWorkspace::default();
        restored
            .bind_persistence(persistence)
            .expect("bind restored persistence");
        restored
            .create_editor_tab(QueryLanguage::Sql, "Restored", "SELECT geometry_fixture")
            .expect("restored tab");
        let snapshot = restored
            .to_persistence_snapshot()
            .expect("restored snapshot");
        let snapshot_bytes = encoded_profile_bytes_at_generation(&snapshot, 1)
            .expect("restored workspace bytes")
            .1;
        assert!(service.try_emit(UiEvent::WorkspaceLoaded {
            operation_id,
            identity,
            base_revision,
            mode: WorkspaceStoreMode::ReadWrite,
            read_only_reason: None,
            generation: Some(1),
            committed_bytes: snapshot_bytes,
            snapshot: Some(Box::new(snapshot)),
        }));
        app.poll_events();
        assert!(
            app.compact_workspace.is_none(),
            "selected exact restore must force stale egui PanelState eviction"
        );
        let restored_revision = app
            .model
            .workspace(&key)
            .map_or(u64::MAX, ProfileWorkspace::revision);

        let _ = render(&mut app);
        let applied = app
            .workspace_geometries
            .get(&key)
            .copied()
            .expect("applied geometry");
        assert_eq!(applied.navigator_width(), 360.0);
        assert_eq!(applied.editor_share(), 0.67);
        let workspace = app.model.workspace(&key).expect("restored workspace");
        let durable_geometry = workspace
            .persistence()
            .expect("restored persistence")
            .geometry();
        assert_eq!(durable_geometry.navigator_width(), 360.0);
        assert_eq!(durable_geometry.editor_share(), 0.67);
        assert_eq!(workspace.revision(), restored_revision);
        assert!(workspace.is_saved());

        app.observe_workspace_revisions(Instant::now());
        app.autosave_workspaces(Instant::now() + Duration::from_secs(2));
        assert!(
            service.try_next_command().is_none(),
            "applying exact private geometry must not create an autosave revision"
        );
    }

    #[test]
    fn all_outcome_unknown_history_refuses_a_new_persistent_run_without_mutation_or_dispatch() {
        let (ui_port, mut service) = bounded_ports(4);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let profile = j2_classified_environment_profile();
        let key = install_j2_profile(&mut app, &profile);
        assert!(app.ensure_workspace_persistence_binding(&key, &profile));
        app.model
            .workspace_mut(key.clone())
            .create_editor_tab(QueryLanguage::Sql, "Protected", "SELECT protected_history")
            .expect("protected-history editor");
        let history = (1..=MAX_HISTORY_ENTRIES_PER_PROFILE)
            .map(|id| {
                WorkspaceHistoryEntry::new(
                    u64::try_from(id).expect("bounded fixture id"),
                    "SELECT outcome_unknown",
                    WorkspaceRunTarget::Current,
                    i64::try_from(id).expect("bounded fixture timestamp"),
                    WorkspaceHistoryStatus::OutcomeUnknown,
                    1,
                    0,
                    0,
                    false,
                )
                .expect("bounded outcome-unknown entry")
            })
            .collect::<Vec<_>>();
        app.model
            .workspace_mut(key.clone())
            .replace_persistence_history(history)
            .expect("bounded history");
        if let Some(state) = app.workspace_persistence.get_mut(&key) {
            state.load = WorkspaceLoadPhase::Ready;
            state.mode = Some(WorkspaceStoreMode::ReadWrite);
        }
        let revision_before = app
            .model
            .workspace(&key)
            .map_or(u64::MAX, ProfileWorkspace::revision);
        let histories_before = app
            .model
            .workspace(&key)
            .and_then(ProfileWorkspace::persistence)
            .expect("retained history")
            .history()
            .to_vec();
        let intent = build_execute_intent(
            &profile,
            app.model.workspace(&key).expect("protected workspace"),
            EditorCursor::caret(0),
        )
        .expect("bounded read intent");

        assert!(!app.submit_editor_execute(intent));
        assert!(service.try_next_command().is_none());
        let retained = app
            .model
            .workspace(&key)
            .and_then(ProfileWorkspace::persistence)
            .expect("retained history")
            .history();
        assert_eq!(retained, histories_before);
        assert_eq!(
            app.model.workspace(&key).map(ProfileWorkspace::revision),
            Some(revision_before)
        );
        assert!(app.model.status.contains("outcome-unknown"));
        assert_eq!(retained.len(), MAX_HISTORY_ENTRIES_PER_PROFILE);
        assert!(
            retained
                .iter()
                .all(|entry| entry.status() == WorkspaceHistoryStatus::OutcomeUnknown)
        );
        let collisions = vec![
            WorkspaceHistoryEntry::new(
                1,
                "SELECT 1",
                WorkspaceRunTarget::Current,
                1,
                WorkspaceHistoryStatus::Succeeded,
                1,
                1,
                0,
                false,
            )
            .expect("collision one"),
            WorkspaceHistoryEntry::new(
                42,
                "SELECT 42",
                WorkspaceRunTarget::Current,
                2,
                WorkspaceHistoryStatus::Succeeded,
                1,
                1,
                0,
                false,
            )
            .expect("collision forty-two"),
        ];
        assert_eq!(super::next_workspace_history_id(&collisions, 42), Some(2));
        assert_eq!(super::next_workspace_history_id(&collisions, 99), Some(99));
    }

    #[test]
    fn global_retention_enqueues_oldest_shrink_before_target_growth() {
        let (ui_port, mut service) = bounded_ports(16);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let base = j2_classified_environment_profile();
        let profiles = (0_u8..6)
            .map(|index| {
                let mut profile = base.clone();
                let id = format!("retention-{index}");
                profile.id = ProfileId(id.clone());
                profile.name = id.clone();
                profile.persisted.id = id.clone();
                profile.persisted.name = id;
                profile.persisted.safety = ProfileSafetyPosture::classified(
                    ProfileEnvironment::Development,
                    ProfileAccess::ReadWrite,
                    ProfileInstanceId::from_bytes([index.saturating_add(1); 16]),
                );
                profile
            })
            .collect::<Vec<_>>();
        app.model.profiles.clone_from(&profiles);
        app.model.selected_profile = Some(profiles[5].id.clone());
        app.model.config = ConfigPresentation::for_source(
            ConfigSourceVersion::V3,
            &PathBuf::from("/private/tmp/dbotter-j2-global-retention.toml"),
        );
        for profile in &profiles {
            app.model
                .active_generations
                .insert(profile.id.clone(), profile.generation);
            let key = WorkspaceKey::new(profile.id.clone(), profile.generation);
            assert!(app.ensure_workspace_persistence_binding(&key, profile));
            if let Some(state) = app.workspace_persistence.get_mut(&key) {
                state.load = WorkspaceLoadPhase::Ready;
                state.mode = Some(WorkspaceStoreMode::ReadWrite);
            }
        }
        for (profile_index, profile) in profiles.iter().take(5).enumerate() {
            let key = WorkspaceKey::new(profile.id.clone(), profile.generation);
            let history = (1..=MAX_HISTORY_ENTRIES_PER_PROFILE)
                .map(|id| {
                    let timestamp = i64::try_from(
                        profile_index
                            .saturating_mul(MAX_HISTORY_ENTRIES_PER_PROFILE)
                            .saturating_add(id),
                    )
                    .expect("bounded global timestamp");
                    WorkspaceHistoryEntry::new(
                        u64::try_from(id).expect("bounded global id"),
                        "SELECT retained",
                        WorkspaceRunTarget::Current,
                        timestamp,
                        WorkspaceHistoryStatus::Succeeded,
                        1,
                        1,
                        0,
                        false,
                    )
                    .expect("valid global history")
                })
                .collect::<Vec<_>>();
            app.model
                .workspace_mut(key)
                .replace_persistence_history(history)
                .expect("global history fixture");
        }
        let target_key = WorkspaceKey::new(profiles[5].id.clone(), profiles[5].generation);
        app.model
            .workspace_mut(target_key.clone())
            .create_editor_tab(QueryLanguage::Sql, "Target", "SELECT global_retention")
            .expect("global target editor");
        let intent = build_execute_intent(
            &profiles[5],
            app.model.workspace(&target_key).expect("global target"),
            EditorCursor::caret(0),
        )
        .expect("global execute intent");

        assert!(app.submit_editor_execute(intent));
        let oldest_instance = profiles[0]
            .persisted
            .safety
            .instance_id()
            .expect("oldest instance");
        let target_instance = profiles[5]
            .persisted
            .safety
            .instance_id()
            .expect("target instance");
        let mut committed_instances = Vec::new();
        while app.retention_commit_barrier.is_active() {
            let (operation_id, identity, revision, history) = match service.try_next_command() {
                Some(UiCommand::CommitWorkspace {
                    operation_id,
                    identity,
                    revision,
                    snapshot,
                }) => (
                    operation_id,
                    identity,
                    revision,
                    snapshot.history().to_vec(),
                ),
                command => panic!("retention barrier must own the workspace lane, got {command:?}"),
            };
            if committed_instances.is_empty() {
                assert_eq!(identity.instance_id(), oldest_instance);
                assert_eq!(history.len(), MAX_HISTORY_ENTRIES_PER_PROFILE - 1);
                assert!(history.iter().all(|entry| entry.id() != 1));
            }
            committed_instances.push(identity.instance_id());
            assert!(
                service.try_emit(UiEvent::WorkspaceCommitted {
                    operation_id,
                    identity,
                    revision,
                    generation: 1,
                    committed_bytes: submitted_committed_bytes_or_stale_sentinel(
                        &app,
                        operation_id,
                        1,
                    ),
                    warnings: Vec::new(),
                })
            );
            app.poll_events();
        }
        assert_eq!(committed_instances.first(), Some(&oldest_instance));
        assert_eq!(committed_instances.last(), Some(&target_instance));
        // The test port observes the workspace lane before the work lane. This
        // proves store-barrier ordering only; it is not a pre-execution commit
        // protocol and does not claim the runtime waited for commit acks.
        assert!(matches!(
            service.try_next_command(),
            Some(UiCommand::Execute { .. })
        ));
        assert!(service.try_next_command().is_none());
        let target_history = app
            .model
            .workspace(&target_key)
            .and_then(ProfileWorkspace::persistence)
            .expect("target persistence")
            .history();
        assert_eq!(target_history.len(), 1);
        assert_eq!(
            target_history[0].status(),
            WorkspaceHistoryStatus::OutcomeUnknown
        );
    }

    #[test]
    fn durable_shrink_precedes_lower_instance_growth_and_retry_keeps_that_order() {
        const TUNABLE_CAPACITY: usize = MAX_HISTORY_SOURCE_BYTES - 16 * 1024;
        let shard_base = conservative_encoded_profile_bytes_for_test(&byte_boundary_snapshot(
            0x81,
            0,
            WorkspaceHistoryStatus::Succeeded,
        ))
        .expect("shard base")
        .0;
        let shard_exact_bytes = usize::try_from(
            (MAX_PROFILE_SHARD_BYTES as u64)
                .checked_sub(shard_base)
                .expect("fixture base below shard cap"),
        )
        .expect("shard delta fits usize");
        assert!(shard_exact_bytes <= TUNABLE_CAPACITY);
        let fixed = (0x81_u8..=0x83)
            .map(|instance_byte| {
                let snapshot = byte_boundary_snapshot(
                    instance_byte,
                    shard_exact_bytes,
                    WorkspaceHistoryStatus::Succeeded,
                );
                assert_eq!(
                    conservative_encoded_profile_bytes_for_test(&snapshot)
                        .expect("exact fixed shard")
                        .0,
                    MAX_PROFILE_SHARD_BYTES as u64
                );
                snapshot
            })
            .collect::<Vec<_>>();
        let fixed_committed = fixed.iter().fold(0_u64, |total, snapshot| {
            total.saturating_add(
                conservative_encoded_profile_bytes_for_test(snapshot)
                    .expect("fixed committed bytes")
                    .1,
            )
        });
        let growing_base = conservative_encoded_profile_bytes_for_test(&byte_boundary_snapshot(
            0x80,
            0,
            WorkspaceHistoryStatus::Succeeded,
        ))
        .expect("growing base")
        .1;
        let growing_bytes = usize::try_from(
            MAX_WORKSPACE_STORE_BYTES
                .checked_sub(fixed_committed)
                .and_then(|remaining| remaining.checked_sub(growing_base))
                .expect("exact total fixture has growing headroom"),
        )
        .expect("growing delta fits usize");
        assert!(growing_bytes <= TUNABLE_CAPACITY);
        let growing_baseline =
            byte_boundary_snapshot(0x80, growing_bytes, WorkspaceHistoryStatus::Succeeded);
        let mut baselines = vec![growing_baseline];
        baselines.extend(fixed);
        let baseline_total = baselines.iter().fold(0_u64, |total, snapshot| {
            total.saturating_add(
                conservative_encoded_profile_bytes_for_test(snapshot)
                    .expect("baseline committed bytes")
                    .1,
            )
        });
        assert_eq!(baseline_total, MAX_WORKSPACE_STORE_BYTES);

        let (ui_port, mut service) = bounded_ports(16);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let profiles = baselines
            .iter()
            .map(|snapshot| {
                let mut profile = j2_profile_with_identity(
                    &snapshot.profile_id().0,
                    snapshot.instance_id().as_bytes()[0],
                );
                profile.persisted.safety = ProfileSafetyPosture::classified(
                    ProfileEnvironment::Development,
                    ProfileAccess::ReadWrite,
                    snapshot.instance_id(),
                );
                profile
            })
            .collect::<Vec<_>>();
        app.model.profiles.clone_from(&profiles);
        app.model.selected_profile = Some(profiles[0].id.clone());
        app.model.config = ConfigPresentation::for_source(
            ConfigSourceVersion::V3,
            &PathBuf::from("/private/tmp/dbotter-j2-shrink-before-grow.toml"),
        );
        let mut loads = Vec::new();
        for profile in &profiles {
            app.model
                .active_generations
                .insert(profile.id.clone(), profile.generation);
            let key = WorkspaceKey::new(profile.id.clone(), profile.generation);
            assert!(app.ensure_workspace_persistence_binding(&key, profile));
            app.request_workspace_load(&key);
            match service.try_next_command() {
                Some(UiCommand::LoadWorkspace {
                    operation_id,
                    identity,
                    base_revision,
                }) => loads.push((operation_id, identity, base_revision)),
                command => panic!("expected exact baseline load, got {command:?}"),
            }
        }
        for (((operation_id, identity, base_revision), snapshot), profile) in loads
            .into_iter()
            .zip(baselines.iter().cloned())
            .zip(profiles.iter())
        {
            assert_eq!(identity.profile_id(), &profile.id);
            let committed_bytes = encoded_profile_bytes_at_generation(&snapshot, 1)
                .expect("baseline load bytes")
                .1;
            assert!(service.try_emit(UiEvent::WorkspaceLoaded {
                operation_id,
                identity,
                base_revision,
                mode: WorkspaceStoreMode::ReadWrite,
                read_only_reason: None,
                generation: Some(1),
                committed_bytes,
                snapshot: Some(Box::new(snapshot)),
            }));
            app.poll_events();
        }
        assert!(service.try_next_command().is_none());

        let grow_key = WorkspaceKey::new(profiles[0].id.clone(), profiles[0].generation);
        let shrink_key = WorkspaceKey::new(profiles[3].id.clone(), profiles[3].generation);
        let shrink_instance = profiles[3]
            .persisted
            .safety
            .instance_id()
            .expect("shrink instance");
        let grow_instance = profiles[0]
            .persisted
            .safety
            .instance_id()
            .expect("grow instance");
        assert!(grow_instance.as_bytes() < shrink_instance.as_bytes());
        app.model
            .workspace_mut(grow_key.clone())
            .create_editor_tab(QueryLanguage::Sql, "Grow", "SELECT grow_after_durable")
            .expect("growing editor");
        let shrunken_history = app
            .model
            .workspace(&shrink_key)
            .and_then(ProfileWorkspace::persistence)
            .expect("shrink persistence")
            .history()[1..]
            .to_vec();
        app.model
            .workspace_mut(shrink_key.clone())
            .replace_persistence_history(shrunken_history)
            .expect("shrink one durable entry");
        app.observe_workspace_revisions(Instant::now());
        assert!(app.reconcile_workspace_retention(true));

        let (first_operation, first_identity, first_revision) = match service.try_next_command() {
            Some(UiCommand::CommitWorkspace {
                operation_id,
                identity,
                revision,
                ..
            }) => (operation_id, identity, revision),
            command => panic!("expected shrink-first commit, got {command:?}"),
        };
        assert_eq!(first_identity.instance_id(), shrink_instance);
        assert!(service.try_emit(UiEvent::WorkspaceCommitSuperseded {
            operation_id: first_operation,
            identity: first_identity.clone(),
            revision: first_revision,
            superseded_by: OperationId(99_101),
            superseded_by_revision: first_revision.saturating_add(1),
        }));
        app.poll_events();
        assert!(app.retry_retention_commit_barrier());

        let mut simulated_total = baseline_total;
        let mut durable_by_instance = baselines
            .iter()
            .map(|snapshot| {
                (
                    snapshot.instance_id(),
                    conservative_encoded_profile_bytes_for_test(snapshot)
                        .expect("baseline accounting bytes")
                        .1,
                )
            })
            .collect::<HashMap<_, _>>();
        let mut committed = Vec::new();
        while app.retention_commit_barrier.is_active() {
            let (operation_id, identity, revision, committed_bytes) = match service
                .try_next_command()
            {
                Some(UiCommand::CommitWorkspace {
                    operation_id,
                    identity,
                    revision,
                    snapshot,
                }) => {
                    let committed_bytes = conservative_encoded_profile_bytes_for_test(&snapshot)
                        .expect("submitted committed bytes")
                        .1;
                    (operation_id, identity, revision, committed_bytes)
                }
                command => panic!("expected ordered retry commit, got {command:?}"),
            };
            if committed.is_empty() {
                assert_eq!(identity.instance_id(), shrink_instance);
            }
            let prior = durable_by_instance
                .insert(identity.instance_id(), committed_bytes)
                .expect("known durable baseline");
            simulated_total = simulated_total
                .checked_sub(prior)
                .and_then(|total| total.checked_add(committed_bytes))
                .expect("simulated committed accounting");
            assert!(simulated_total <= MAX_WORKSPACE_STORE_BYTES);
            committed.push(identity.instance_id());
            assert!(service.try_emit(UiEvent::WorkspaceCommitted {
                operation_id,
                identity,
                revision,
                generation: u64::try_from(committed.len()).expect("generation"),
                committed_bytes: submitted_committed_bytes_or_stale_sentinel(
                    &app,
                    operation_id,
                    u64::try_from(committed.len()).expect("generation"),
                ),
                warnings: Vec::new(),
            }));
            app.poll_events();
        }
        assert_eq!(committed, vec![shrink_instance, grow_instance]);
        assert!(service.try_next_command().is_none());
    }

    #[test]
    fn generation_nine_to_ten_growth_waits_for_the_single_global_shrink() {
        const DURABLE_GENERATION: u64 = 9;
        const NEXT_GENERATION: u64 = 10;
        const TUNABLE_CAPACITY: usize = MAX_HISTORY_SOURCE_BYTES - 16 * 1024;

        let fixed = (0x81_u8..=0x83)
            .map(|instance_byte| {
                let base = if instance_byte == 0x83 {
                    byte_boundary_snapshot_with_oldest_first(
                        instance_byte,
                        0,
                        WorkspaceHistoryStatus::Succeeded,
                    )
                } else {
                    byte_boundary_snapshot(instance_byte, 0, WorkspaceHistoryStatus::Succeeded)
                };
                let base_shard = conservative_encoded_profile_bytes_for_test(&base)
                    .expect("fixed profile base bytes")
                    .0;
                let tunable = usize::try_from(
                    (MAX_PROFILE_SHARD_BYTES as u64)
                        .checked_sub(base_shard)
                        .expect("fixed profile base below shard cap"),
                )
                .expect("fixed profile tunable bytes");
                assert!(tunable <= TUNABLE_CAPACITY);
                let snapshot = if instance_byte == 0x83 {
                    byte_boundary_snapshot_with_oldest_first(
                        instance_byte,
                        tunable,
                        WorkspaceHistoryStatus::Succeeded,
                    )
                } else {
                    byte_boundary_snapshot(
                        instance_byte,
                        tunable,
                        WorkspaceHistoryStatus::Succeeded,
                    )
                };
                assert_eq!(
                    conservative_encoded_profile_bytes_for_test(&snapshot)
                        .expect("fixed profile upper bound")
                        .0,
                    MAX_PROFILE_SHARD_BYTES as u64
                );
                snapshot
            })
            .collect::<Vec<_>>();
        let fixed_durable_bytes = fixed.iter().fold(0_u64, |total, snapshot| {
            total.saturating_add(
                encoded_profile_bytes_at_generation(snapshot, DURABLE_GENERATION)
                    .expect("fixed generation-nine bytes")
                    .1,
            )
        });
        let growing_base = byte_boundary_snapshot(0x80, 0, WorkspaceHistoryStatus::Succeeded);
        let growing_base_bytes =
            encoded_profile_bytes_at_generation(&growing_base, DURABLE_GENERATION)
                .expect("growing profile base bytes")
                .1;
        let growing_tunable = usize::try_from(
            MAX_WORKSPACE_STORE_BYTES
                .checked_sub(fixed_durable_bytes)
                .and_then(|remaining| remaining.checked_sub(growing_base_bytes))
                .expect("generation-nine total has tunable headroom"),
        )
        .expect("growing tunable bytes");
        assert!(growing_tunable > 0);
        assert!(growing_tunable <= TUNABLE_CAPACITY);
        let growing_baseline =
            byte_boundary_snapshot(0x80, growing_tunable, WorkspaceHistoryStatus::Succeeded);
        let growing_next =
            byte_boundary_snapshot(0x80, growing_tunable - 1, WorkspaceHistoryStatus::Succeeded);
        let mut baselines = vec![growing_baseline.clone()];
        baselines.extend(fixed.iter().cloned());

        let baseline_total = baselines.iter().fold(0_u64, |total, snapshot| {
            total.saturating_add(
                encoded_profile_bytes_at_generation(snapshot, DURABLE_GENERATION)
                    .expect("generation-nine baseline bytes")
                    .1,
            )
        });
        assert_eq!(baseline_total, MAX_WORKSPACE_STORE_BYTES);
        let growing_durable_bytes =
            encoded_profile_bytes_at_generation(&growing_baseline, DURABLE_GENERATION)
                .expect("growing durable bytes")
                .1;
        let growing_next_durable_bytes =
            encoded_profile_bytes_at_generation(&growing_next, DURABLE_GENERATION)
                .expect("shrunk generation-nine bytes")
                .1;
        let growing_next_commit_bytes =
            encoded_profile_bytes_at_generation(&growing_next, NEXT_GENERATION)
                .expect("generation-ten growth bytes")
                .1;
        assert_eq!(
            growing_next_durable_bytes,
            growing_durable_bytes.saturating_sub(1)
        );
        assert_eq!(
            growing_next_commit_bytes,
            growing_durable_bytes.saturating_add(1)
        );
        assert_eq!(
            baseline_total
                .checked_sub(growing_durable_bytes)
                .and_then(|total| total.checked_add(growing_next_commit_bytes)),
            Some(MAX_WORKSPACE_STORE_BYTES.saturating_add(1)),
            "committing A before any shrink must exceed the real store cap"
        );

        let mut before_eviction = vec![growing_next.clone()];
        before_eviction.extend(fixed.iter().cloned());
        let before_eviction_upper = before_eviction.iter().fold(0_u64, |total, snapshot| {
            total.saturating_add(
                conservative_encoded_profile_bytes_for_test(snapshot)
                    .expect("pre-eviction upper bound")
                    .1,
            )
        });
        assert_eq!(
            before_eviction_upper,
            MAX_WORKSPACE_STORE_BYTES.saturating_add(151)
        );
        let planned =
            super::plan_workspace_snapshot_set(before_eviction).expect("single global shrink plan");
        let shrinking_instance = fixed[2].instance_id();
        assert_eq!(planned.history_evictions().len(), 1);
        assert_eq!(
            planned.history_evictions()[0].instance_id(),
            shrinking_instance
        );
        assert_eq!(planned.history_evictions()[0].history_id(), 1);
        let planned_shrink = planned
            .profiles()
            .iter()
            .find(|snapshot| snapshot.instance_id() == shrinking_instance)
            .expect("planned shrinking profile");
        let shrinking_reduction = conservative_encoded_profile_bytes_for_test(&fixed[2])
            .expect("shrinking baseline upper bound")
            .1
            .checked_sub(
                conservative_encoded_profile_bytes_for_test(planned_shrink)
                    .expect("planned shrinking upper bound")
                    .1,
            )
            .expect("planned B is a strict shrink");
        assert!(shrinking_reduction > 151);
        assert!(
            planned
                .profiles()
                .iter()
                .map(|snapshot| {
                    conservative_encoded_profile_bytes_for_test(snapshot)
                        .expect("planned final upper bound")
                        .1
                })
                .sum::<u64>()
                <= MAX_WORKSPACE_STORE_BYTES
        );

        let (ui_port, mut service) = bounded_ports(16);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let profiles = baselines
            .iter()
            .map(|snapshot| {
                let mut profile = j2_profile_with_identity(
                    &snapshot.profile_id().0,
                    snapshot.instance_id().as_bytes()[0],
                );
                profile.persisted.safety = ProfileSafetyPosture::classified(
                    ProfileEnvironment::Development,
                    ProfileAccess::ReadWrite,
                    snapshot.instance_id(),
                );
                profile
            })
            .collect::<Vec<_>>();
        app.model.profiles.clone_from(&profiles);
        app.model.selected_profile = Some(profiles[0].id.clone());
        app.model.config = ConfigPresentation::for_source(
            ConfigSourceVersion::V3,
            &PathBuf::from("/private/tmp/dbotter-j2-generation-nine-to-ten.toml"),
        );
        let mut loads = Vec::new();
        for profile in &profiles {
            app.model
                .active_generations
                .insert(profile.id.clone(), profile.generation);
            let key = WorkspaceKey::new(profile.id.clone(), profile.generation);
            assert!(app.ensure_workspace_persistence_binding(&key, profile));
            app.request_workspace_load(&key);
            match service.try_next_command() {
                Some(UiCommand::LoadWorkspace {
                    operation_id,
                    identity,
                    base_revision,
                }) => loads.push((operation_id, identity, base_revision)),
                command => panic!("expected generation-nine load, got {command:?}"),
            }
        }
        // This fixture models a store that was already live at generation 9.
        // Startup normalization is covered separately; keep it from consuming
        // the boundary before the one-byte local revision is applied.
        app.retention_reconcile_required = false;
        for ((operation_id, identity, base_revision), snapshot) in
            loads.into_iter().zip(baselines.iter().cloned())
        {
            let committed_bytes =
                encoded_profile_bytes_at_generation(&snapshot, DURABLE_GENERATION)
                    .expect("generation-nine load bytes")
                    .1;
            assert!(service.try_emit(UiEvent::WorkspaceLoaded {
                operation_id,
                identity,
                base_revision,
                mode: WorkspaceStoreMode::ReadWrite,
                read_only_reason: None,
                generation: Some(DURABLE_GENERATION),
                committed_bytes,
                snapshot: Some(Box::new(snapshot)),
            }));
            app.poll_events();
        }
        assert!(service.try_next_command().is_none());

        let growing_key = WorkspaceKey::new(profiles[0].id.clone(), profiles[0].generation);
        app.model
            .workspace_mut(growing_key)
            .replace_persistence_history(growing_next.history().to_vec())
            .expect("one-byte A shrink");
        app.observe_workspace_revisions(Instant::now());
        assert!(app.reconcile_workspace_retention(true));

        let (first_operation, first_identity, first_revision, first_snapshot) =
            match service.try_next_command() {
                Some(UiCommand::CommitWorkspace {
                    operation_id,
                    identity,
                    revision,
                    snapshot,
                }) => (operation_id, identity, revision, snapshot),
                command => panic!("expected generation-boundary shrink first, got {command:?}"),
            };
        assert_eq!(first_identity.instance_id(), shrinking_instance);
        assert!(first_snapshot.history().iter().all(|entry| entry.id() != 1));
        assert!(service.try_emit(UiEvent::WorkspaceCommitSuperseded {
            operation_id: first_operation,
            identity: first_identity,
            revision: first_revision,
            superseded_by: OperationId(99_109),
            superseded_by_revision: first_revision.saturating_add(1),
        }));
        app.poll_events();
        assert!(app.retry_retention_commit_barrier());

        let mut simulated_total = baseline_total;
        let mut durable_by_instance = baselines
            .iter()
            .map(|snapshot| {
                (
                    snapshot.instance_id(),
                    encoded_profile_bytes_at_generation(snapshot, DURABLE_GENERATION)
                        .expect("generation-nine durable accounting")
                        .1,
                )
            })
            .collect::<HashMap<_, _>>();
        let growing_instance = growing_baseline.instance_id();
        let mut committed = Vec::new();
        while app.retention_commit_barrier.is_active() {
            let (operation_id, identity, revision, snapshot) = match service.try_next_command() {
                Some(UiCommand::CommitWorkspace {
                    operation_id,
                    identity,
                    revision,
                    snapshot,
                }) => (operation_id, identity, revision, snapshot),
                command => panic!("expected ordered generation-ten commit, got {command:?}"),
            };
            let committed_bytes = encoded_profile_bytes_at_generation(&snapshot, NEXT_GENERATION)
                .expect("generation-ten committed bytes")
                .1;
            let prior = durable_by_instance
                .insert(identity.instance_id(), committed_bytes)
                .expect("known generation-nine durable baseline");
            simulated_total = simulated_total
                .checked_sub(prior)
                .and_then(|total| total.checked_add(committed_bytes))
                .expect("simulated generation-ten accounting");
            assert!(simulated_total <= MAX_WORKSPACE_STORE_BYTES);
            committed.push(identity.instance_id());
            assert!(service.try_emit(UiEvent::WorkspaceCommitted {
                operation_id,
                identity,
                revision,
                generation: NEXT_GENERATION,
                committed_bytes,
                warnings: Vec::new(),
            }));
            app.poll_events();
        }
        assert_eq!(committed, vec![shrinking_instance, growing_instance]);
        assert!(service.try_next_command().is_none());
    }

    #[test]
    fn clean_persistence_off_noncanonical_bytes_are_rewritten_before_growth() {
        let (ui_port, mut service) = bounded_ports(8);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let growing = j2_profile_with_identity("canonical-grow", 0xa1);
        let noncanonical = j2_profile_with_identity("canonical-off", 0xa2);
        let keys = install_j2_profiles_ready(&mut app, &[growing.clone(), noncanonical.clone()]);
        let growing_key = keys[0].clone();
        let noncanonical_key = keys[1].clone();

        {
            let workspace = app.model.workspace_mut(noncanonical_key.clone());
            workspace
                .set_persistence_enabled(false)
                .expect("disable persistence for canonical rewrite");
            let revision = workspace.revision();
            assert!(workspace.mark_saved_if_revision(revision));
        }
        let noncanonical_snapshot = app
            .model
            .workspace(&noncanonical_key)
            .cloned()
            .expect("Persistence Off workspace")
            .to_persistence_snapshot()
            .expect("Persistence Off snapshot");
        let noncanonical_upper =
            conservative_encoded_profile_bytes_for_test(&noncanonical_snapshot)
                .expect("Persistence Off upper bound")
                .1;
        if let Some(state) = app.workspace_persistence.get_mut(&noncanonical_key) {
            state.durable_generation = Some(6);
            state.durable_committed_bytes = noncanonical_upper.saturating_add(23);
            state.observed_revision = app
                .model
                .workspace(&noncanonical_key)
                .map_or(u64::MAX, ProfileWorkspace::revision);
        }
        app.model
            .workspace_mut(growing_key)
            .create_editor_tab(QueryLanguage::Sql, "Grow", "SELECT canonical_growth")
            .expect("growing workspace");
        app.observe_workspace_revisions(Instant::now());

        assert!(app.reconcile_workspace_retention(true));
        let (off_operation, off_identity, off_revision, off_snapshot) =
            match service.try_next_command() {
                Some(UiCommand::CommitWorkspace {
                    operation_id,
                    identity,
                    revision,
                    snapshot,
                }) => (operation_id, identity, revision, snapshot),
                command => panic!("expected clean noncanonical shrink first, got {command:?}"),
            };
        assert_eq!(
            off_identity.instance_id(),
            noncanonical
                .persisted
                .safety
                .instance_id()
                .expect("noncanonical instance")
        );
        assert!(!off_snapshot.persistence_enabled());
        assert!(off_snapshot.editor_tabs().is_empty());
        assert!(off_snapshot.history().is_empty());
        let off_committed_bytes = encoded_profile_bytes_at_generation(&off_snapshot, 7)
            .expect("canonical Persistence Off bytes")
            .1;
        assert!(service.try_emit(UiEvent::WorkspaceCommitted {
            operation_id: off_operation,
            identity: off_identity,
            revision: off_revision,
            generation: 7,
            committed_bytes: off_committed_bytes,
            warnings: Vec::new(),
        }));
        app.poll_events();

        let (growth_operation, growth_identity, growth_revision, growth_snapshot) =
            match service.try_next_command() {
                Some(UiCommand::CommitWorkspace {
                    operation_id,
                    identity,
                    revision,
                    snapshot,
                }) => (operation_id, identity, revision, snapshot),
                command => panic!("expected growth after canonical shrink, got {command:?}"),
            };
        assert_eq!(
            growth_identity.instance_id(),
            growing
                .persisted
                .safety
                .instance_id()
                .expect("growing instance")
        );
        let growth_committed_bytes = encoded_profile_bytes_at_generation(&growth_snapshot, 1)
            .expect("canonical growth bytes")
            .1;
        assert!(service.try_emit(UiEvent::WorkspaceCommitted {
            operation_id: growth_operation,
            identity: growth_identity,
            revision: growth_revision,
            generation: 1,
            committed_bytes: growth_committed_bytes,
            warnings: Vec::new(),
        }));
        app.poll_events();
        assert!(service.try_next_command().is_none());
        assert!(
            app.workspace_persistence
                .get(&noncanonical_key)
                .is_some_and(|state| {
                    state.durable_generation == Some(7)
                        && state.durable_committed_bytes == off_committed_bytes
                })
        );
    }

    #[test]
    fn mismatched_commit_byte_ack_reloads_baseline_without_overwriting_local_work() {
        let (ui_port, mut service) = bounded_ports(8);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let profile = j2_profile_with_identity("commit-byte-mismatch", 0xa3);
        let key = install_j2_profile(&mut app, &profile);
        assert!(app.ensure_workspace_persistence_binding(&key, &profile));
        if let Some(state) = app.workspace_persistence.get_mut(&key) {
            state.load = WorkspaceLoadPhase::Ready;
            state.mode = Some(WorkspaceStoreMode::ReadWrite);
        }
        app.model
            .workspace_mut(key.clone())
            .create_editor_tab(QueryLanguage::Sql, "Local", "SELECT survives_byte_mismatch")
            .expect("local mismatch editor");
        app.observe_workspace_revisions(Instant::now());
        assert!(app.submit_workspace_commit(&key, true));
        let (operation_id, identity, revision, snapshot) = match service.try_next_command() {
            Some(UiCommand::CommitWorkspace {
                operation_id,
                identity,
                revision,
                snapshot,
            }) => (operation_id, identity, revision, snapshot),
            command => panic!("expected byte-mismatch commit, got {command:?}"),
        };
        let committed_bytes = encoded_profile_bytes_at_generation(&snapshot, 1)
            .expect("submitted exact bytes")
            .1;
        assert!(service.try_emit(UiEvent::WorkspaceCommitted {
            operation_id,
            identity: identity.clone(),
            revision,
            generation: 1,
            committed_bytes: committed_bytes.saturating_add(1),
            warnings: Vec::new(),
        }));
        app.poll_events();
        assert!(app.workspace_persistence.get(&key).is_some_and(|state| {
            state.durable_generation.is_none()
                && state.durable_committed_bytes == 0
                && state.submitted_commit.is_none()
                && state.refresh_durable_baseline_only
        }));
        let (load_operation, load_identity, base_revision) = match service.try_next_command() {
            Some(UiCommand::LoadWorkspace {
                operation_id,
                identity,
                base_revision,
            }) => (operation_id, identity, base_revision),
            command => panic!("mismatched ack must request exact load, got {command:?}"),
        };
        assert_eq!(load_identity, identity);
        app.retention_reconcile_required = false;
        assert!(service.try_emit(UiEvent::WorkspaceLoaded {
            operation_id: load_operation,
            identity: load_identity,
            base_revision,
            mode: WorkspaceStoreMode::ReadWrite,
            read_only_reason: None,
            generation: Some(1),
            committed_bytes,
            snapshot: Some(snapshot),
        }));
        app.poll_events();
        let workspace = app.model.workspace(&key).expect("local mismatch workspace");
        assert!(
            workspace
                .editor_tabs()
                .iter()
                .any(|tab| tab.text() == "SELECT survives_byte_mismatch")
        );
        assert!(!workspace.is_saved());
        assert!(app.workspace_persistence.get(&key).is_some_and(|state| {
            state.durable_generation == Some(1)
                && state.durable_committed_bytes == committed_bytes
                && !state.refresh_durable_baseline_only
                && state.dirty_since.is_some()
        }));
        assert!(service.try_next_command().is_none());
    }

    #[test]
    fn durable_baseline_advances_only_for_exact_load_and_commit_truth() {
        let (ui_port, mut service) = bounded_ports(16);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let profile = j2_profile_with_identity("durable-correlation", 0x91);
        let key = install_j2_profile(&mut app, &profile);
        assert!(app.ensure_workspace_persistence_binding(&key, &profile));
        app.request_workspace_load(&key);
        let (empty_load_operation, identity, empty_base_revision) = match service.try_next_command()
        {
            Some(UiCommand::LoadWorkspace {
                operation_id,
                identity,
                base_revision,
            }) => (operation_id, identity, base_revision),
            command => panic!("expected empty durable baseline load, got {command:?}"),
        };
        assert!(service.try_emit(UiEvent::WorkspaceLoaded {
            operation_id: empty_load_operation,
            identity: identity.clone(),
            base_revision: empty_base_revision,
            mode: WorkspaceStoreMode::ReadWrite,
            read_only_reason: None,
            generation: None,
            committed_bytes: 0,
            snapshot: None,
        }));
        app.poll_events();
        assert!(app.workspace_persistence.get(&key).is_some_and(|state| {
            state.durable_committed_bytes == 0 && state.submitted_commit.is_none()
        }));
        if let Some(state) = app.workspace_persistence.get_mut(&key) {
            state.load = WorkspaceLoadPhase::Unloaded;
        }
        app.request_workspace_load(&key);
        let (load_operation, loaded_identity, base_revision) = match service.try_next_command() {
            Some(UiCommand::LoadWorkspace {
                operation_id,
                identity,
                base_revision,
            }) => (operation_id, identity, base_revision),
            command => panic!("expected durable baseline load, got {command:?}"),
        };
        let loaded = j2_workspace_snapshot(&profile, true, Some("SELECT durable_loaded"));
        let loaded_bytes = encoded_profile_bytes_at_generation(&loaded, 1)
            .expect("loaded committed bytes")
            .1;
        assert!(service.try_emit(UiEvent::WorkspaceLoaded {
            operation_id: load_operation,
            identity: loaded_identity,
            base_revision,
            mode: WorkspaceStoreMode::ReadWrite,
            read_only_reason: None,
            generation: Some(1),
            committed_bytes: loaded_bytes,
            snapshot: Some(Box::new(loaded)),
        }));
        app.poll_events();
        assert!(app.workspace_persistence.get(&key).is_some_and(|state| {
            state.durable_committed_bytes == loaded_bytes && state.submitted_commit.is_none()
        }));

        app.model
            .workspace_mut(key.clone())
            .create_editor_tab(QueryLanguage::Sql, "Dirty", "SELECT pending_durable")
            .expect("durable dirty editor");
        app.observe_workspace_revisions(Instant::now());
        assert!(app.submit_workspace_commit(&key, true));
        let (failed_operation, failed_revision, failed_bytes) = match service.try_next_command() {
            Some(UiCommand::CommitWorkspace {
                operation_id,
                revision,
                snapshot,
                ..
            }) => (
                operation_id,
                revision,
                encoded_profile_bytes_at_generation(&snapshot, 2)
                    .expect("failed submitted bytes")
                    .1,
            ),
            command => panic!("expected correlated commit, got {command:?}"),
        };
        let submitted_before_mismatch = app
            .workspace_persistence
            .get(&key)
            .and_then(|state| state.submitted_commit)
            .expect("submitted correlation");
        assert_eq!(
            submitted_before_mismatch
                .accounting
                .encoded_bytes_at_generation(2)
                .expect("submitted generation bytes")
                .1,
            failed_bytes
        );
        assert!(service.try_emit(UiEvent::WorkspaceCommitted {
            operation_id: OperationId(failed_operation.0.saturating_add(50_000)),
            identity: identity.clone(),
            revision: failed_revision,
            generation: 2,
            committed_bytes: submitted_committed_bytes_or_stale_sentinel(
                &app,
                OperationId(failed_operation.0.saturating_add(50_000)),
                2,
            ),
            warnings: Vec::new(),
        }));
        app.poll_events();
        assert!(app.workspace_persistence.get(&key).is_some_and(|state| {
            state.durable_committed_bytes == loaded_bytes
                && state.submitted_commit == Some(submitted_before_mismatch)
        }));
        let foreign_submission = super::SubmittedWorkspaceCommit {
            operation_id: OperationId(failed_operation.0.saturating_add(1)),
            ..submitted_before_mismatch
        };
        if let Some(state) = app.workspace_persistence.get_mut(&key) {
            state.submitted_commit = Some(foreign_submission);
        }
        assert!(
            service.try_emit(UiEvent::WorkspaceCommitted {
                operation_id: failed_operation,
                identity: identity.clone(),
                revision: failed_revision,
                generation: 2,
                committed_bytes: submitted_committed_bytes_or_stale_sentinel(
                    &app,
                    failed_operation,
                    2,
                ),
                warnings: Vec::new(),
            })
        );
        app.poll_events();
        assert!(app.workspace_persistence.get(&key).is_some_and(|state| {
            state.durable_committed_bytes == loaded_bytes
                && state.submitted_commit == Some(foreign_submission)
                && matches!(
                    state.save,
                    WorkspaceSavePhase::Saving {
                        operation_id,
                        revision,
                    } if operation_id == failed_operation && revision == failed_revision
                )
        }));
        if let Some(state) = app.workspace_persistence.get_mut(&key) {
            state.submitted_commit = Some(submitted_before_mismatch);
        }

        assert!(service.try_emit(UiEvent::WorkspaceOperationFailed {
            operation_id: failed_operation,
            identity: identity.clone(),
            revision: failed_revision,
            action: WorkspaceAction::Commit,
            code: WorkspaceFailureCode::Busy,
        }));
        app.poll_events();
        assert!(app.workspace_persistence.get(&key).is_some_and(|state| {
            state.durable_committed_bytes == loaded_bytes && state.submitted_commit.is_none()
        }));

        assert!(app.submit_workspace_commit(&key, true));
        let (superseded_operation, superseded_revision) = match service.try_next_command() {
            Some(UiCommand::CommitWorkspace {
                operation_id,
                revision,
                ..
            }) => (operation_id, revision),
            command => panic!("expected superseded retry, got {command:?}"),
        };
        assert!(service.try_emit(UiEvent::WorkspaceCommitSuperseded {
            operation_id: superseded_operation,
            identity: identity.clone(),
            revision: superseded_revision,
            superseded_by: OperationId(99_201),
            superseded_by_revision: superseded_revision.saturating_add(1),
        }));
        app.poll_events();
        assert!(app.workspace_persistence.get(&key).is_some_and(|state| {
            state.durable_committed_bytes == loaded_bytes && state.submitted_commit.is_none()
        }));

        assert!(app.submit_workspace_commit(&key, true));
        let (success_operation, success_revision, success_bytes) = match service.try_next_command()
        {
            Some(UiCommand::CommitWorkspace {
                operation_id,
                revision,
                snapshot,
                ..
            }) => (
                operation_id,
                revision,
                encoded_profile_bytes_at_generation(&snapshot, 3)
                    .expect("successful submitted bytes")
                    .1,
            ),
            command => panic!("expected successful retry, got {command:?}"),
        };
        assert!(service.try_emit(UiEvent::WorkspaceCommitted {
            operation_id: success_operation,
            identity,
            revision: success_revision,
            generation: 3,
            committed_bytes: submitted_committed_bytes_or_stale_sentinel(
                &app,
                success_operation,
                3,
            ),
            warnings: Vec::new(),
        }));
        app.poll_events();
        assert!(app.workspace_persistence.get(&key).is_some_and(|state| {
            state.durable_committed_bytes == success_bytes && state.submitted_commit.is_none()
        }));
    }

    #[test]
    fn clear_zeroes_and_same_instance_retag_preserves_the_durable_baseline() {
        let (ui_port, mut service) = bounded_ports(16);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let profile = j2_profile_with_identity("durable-retag-clear", 0x92);
        let old_key = install_j2_profile(&mut app, &profile);
        assert!(app.ensure_workspace_persistence_binding(&old_key, &profile));
        app.request_workspace_load(&old_key);
        let (load_operation, old_identity, base_revision) = match service.try_next_command() {
            Some(UiCommand::LoadWorkspace {
                operation_id,
                identity,
                base_revision,
            }) => (operation_id, identity, base_revision),
            command => panic!("expected retag baseline load, got {command:?}"),
        };
        let loaded = j2_workspace_snapshot(&profile, true, Some("SELECT retag_baseline"));
        let loaded_bytes = encoded_profile_bytes_at_generation(&loaded, 1)
            .expect("retag baseline bytes")
            .1;
        assert!(service.try_emit(UiEvent::WorkspaceLoaded {
            operation_id: load_operation,
            identity: old_identity.clone(),
            base_revision,
            mode: WorkspaceStoreMode::ReadWrite,
            read_only_reason: None,
            generation: Some(1),
            committed_bytes: loaded_bytes,
            snapshot: Some(Box::new(loaded.clone())),
        }));
        app.poll_events();
        app.model
            .workspace_mut(old_key.clone())
            .create_editor_tab(QueryLanguage::Sql, "Pending", "SELECT retag_pending")
            .expect("retag pending editor");
        app.observe_workspace_revisions(Instant::now());
        assert!(app.submit_workspace_commit(&old_key, true));
        let (old_commit, old_revision) = match service.try_next_command() {
            Some(UiCommand::CommitWorkspace {
                operation_id,
                revision,
                ..
            }) => (operation_id, revision),
            command => panic!("expected old-identity pending commit, got {command:?}"),
        };

        let mut refreshed = profile.clone();
        refreshed.generation = ProfileGeneration(profile.generation.0.saturating_add(1));
        let new_key = WorkspaceKey::new(refreshed.id.clone(), refreshed.generation);
        assert!(service.try_emit(UiEvent::ProfilesLoaded {
            operation_id: OperationId(100),
            profiles: vec![refreshed.clone()],
            config: ConfigPresentation::for_source(
                ConfigSourceVersion::V3,
                &PathBuf::from("/private/tmp/dbotter-j2-durable-retag.toml"),
            ),
        }));
        app.poll_events();
        let (refresh_operation, refresh_identity, refresh_revision) =
            match service.try_next_command() {
                Some(UiCommand::LoadWorkspace {
                    operation_id,
                    identity,
                    base_revision,
                }) => (operation_id, identity, base_revision),
                command => panic!("expected exact baseline refresh after retag, got {command:?}"),
            };
        assert!(
            app.workspace_persistence
                .get(&new_key)
                .is_some_and(|state| {
                    state.durable_committed_bytes == loaded_bytes
                        && state.submitted_commit.is_none()
                })
        );
        assert!(service.try_emit(UiEvent::WorkspaceCommitted {
            operation_id: old_commit,
            identity: old_identity,
            revision: old_revision,
            generation: 2,
            committed_bytes: submitted_committed_bytes_or_stale_sentinel(&app, old_commit, 2,),
            warnings: Vec::new(),
        }));
        app.poll_events();
        assert!(service.try_emit(UiEvent::WorkspaceLoaded {
            operation_id: refresh_operation,
            identity: refresh_identity,
            base_revision: refresh_revision,
            mode: WorkspaceStoreMode::ReadWrite,
            read_only_reason: None,
            generation: Some(1),
            committed_bytes: loaded_bytes,
            snapshot: Some(Box::new(loaded)),
        }));
        app.poll_events();
        assert!(
            app.workspace_persistence
                .get(&new_key)
                .is_some_and(|state| {
                    matches!(state.load, WorkspaceLoadPhase::Ready)
                        && !state.refresh_durable_baseline_only
                        && state.durable_committed_bytes == loaded_bytes
                        && state.submitted_commit.is_none()
                })
        );

        app.submit_clear_workspace(&new_key);
        let (clear_operation, clear_identity, clear_revision) = match service.try_next_command() {
            Some(UiCommand::ClearWorkspace {
                operation_id,
                identity,
                base_revision,
            }) => (operation_id, identity, base_revision),
            command => panic!("expected exact clear, got {command:?}"),
        };
        assert!(service.try_emit(UiEvent::WorkspaceCleared {
            operation_id: clear_operation,
            identity: clear_identity,
            base_revision: clear_revision,
        }));
        app.poll_events();
        assert!(
            app.workspace_persistence
                .get(&new_key)
                .is_some_and(|state| {
                    state.durable_committed_bytes == 0 && state.submitted_commit.is_some()
                })
        );
        assert!(matches!(
            service.try_next_command(),
            Some(UiCommand::CommitWorkspace { .. })
        ));
    }

    #[test]
    fn inflight_same_instance_retag_reloads_bytes_without_overwriting_local_work() {
        let (ui_port, mut service) = bounded_ports(16);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let profile = j2_profile_with_identity("durable-retag-inflight", 0x93);
        let old_key = install_j2_profile(&mut app, &profile);
        assert!(app.ensure_workspace_persistence_binding(&old_key, &profile));
        app.request_workspace_load(&old_key);
        let (load_operation, old_identity, base_revision) = match service.try_next_command() {
            Some(UiCommand::LoadWorkspace {
                operation_id,
                identity,
                base_revision,
            }) => (operation_id, identity, base_revision),
            command => panic!("expected initial durable load, got {command:?}"),
        };
        let baseline = j2_workspace_snapshot(&profile, true, Some(&"b".repeat(48 * 1024)));
        let baseline_bytes = encoded_profile_bytes_at_generation(&baseline, 1)
            .expect("initial durable bytes")
            .1;
        assert!(service.try_emit(UiEvent::WorkspaceLoaded {
            operation_id: load_operation,
            identity: old_identity.clone(),
            base_revision,
            mode: WorkspaceStoreMode::ReadWrite,
            read_only_reason: None,
            generation: Some(1),
            committed_bytes: baseline_bytes,
            snapshot: Some(Box::new(baseline)),
        }));
        app.poll_events();

        let baseline_tab = app
            .model
            .workspace(&old_key)
            .and_then(ProfileWorkspace::selected_editor_tab_id)
            .expect("restored baseline tab");
        app.model
            .workspace_mut(old_key.clone())
            .discard_editor_tab(baseline_tab)
            .expect("shrink before in-flight commit");
        app.observe_workspace_revisions(Instant::now());
        assert!(app.submit_workspace_commit(&old_key, true));
        let (old_commit, old_revision, submitted_snapshot, submitted_bytes) =
            match service.try_next_command() {
                Some(UiCommand::CommitWorkspace {
                    operation_id,
                    revision,
                    snapshot,
                    ..
                }) => {
                    let bytes = encoded_profile_bytes_at_generation(&snapshot, 2)
                        .expect("submitted shrink bytes")
                        .1;
                    (operation_id, revision, snapshot, bytes)
                }
                command => panic!("expected in-flight shrinking commit, got {command:?}"),
            };
        assert!(submitted_bytes < baseline_bytes);

        let local_marker = "l".repeat(24 * 1024);
        app.model
            .workspace_mut(old_key.clone())
            .create_editor_tab(
                QueryLanguage::Sql,
                "Local after submit",
                local_marker.clone(),
            )
            .expect("newer local editor");
        app.observe_workspace_revisions(Instant::now());
        let local_revision = app
            .model
            .workspace(&old_key)
            .map_or(u64::MAX, ProfileWorkspace::revision);
        let mut local_workspace = app
            .model
            .workspace(&old_key)
            .cloned()
            .expect("newer local workspace");
        let local_snapshot = local_workspace
            .to_persistence_snapshot()
            .expect("newer local snapshot");
        let local_bytes = encoded_profile_bytes_at_generation(&local_snapshot, 2)
            .expect("newer local bytes")
            .1;
        assert!(submitted_bytes < local_bytes);
        assert!(local_bytes < baseline_bytes);

        let mut refreshed = profile.clone();
        refreshed.generation = ProfileGeneration(profile.generation.0.saturating_add(1));
        let new_key = WorkspaceKey::new(refreshed.id.clone(), refreshed.generation);
        assert!(service.try_emit(UiEvent::ProfilesLoaded {
            operation_id: OperationId(100),
            profiles: vec![refreshed],
            config: ConfigPresentation::for_source(
                ConfigSourceVersion::V3,
                &PathBuf::from("/private/tmp/dbotter-j2-inflight-retag.toml"),
            ),
        }));
        app.poll_events();
        let (refresh_operation, refresh_identity, refresh_revision) =
            match service.try_next_command() {
                Some(UiCommand::LoadWorkspace {
                    operation_id,
                    identity,
                    base_revision,
                }) => (operation_id, identity, base_revision),
                command => {
                    panic!("retagged in-flight commit must force an exact reload, got {command:?}")
                }
            };
        assert_eq!(
            refresh_identity.profile_generation(),
            new_key.profile_generation
        );
        assert!(service.try_emit(UiEvent::WorkspaceOperationFailed {
            operation_id: old_commit,
            identity: old_identity,
            revision: old_revision,
            action: WorkspaceAction::Commit,
            code: WorkspaceFailureCode::Stale,
        }));
        app.poll_events();
        let submitted_bytes = encoded_profile_bytes_at_generation(submitted_snapshot.as_ref(), 2)
            .expect("post-I/O submitted bytes")
            .1;
        assert!(service.try_emit(UiEvent::WorkspaceLoaded {
            operation_id: refresh_operation,
            identity: refresh_identity,
            base_revision: refresh_revision,
            mode: WorkspaceStoreMode::ReadWrite,
            read_only_reason: None,
            generation: Some(2),
            committed_bytes: submitted_bytes,
            snapshot: Some(submitted_snapshot),
        }));
        app.poll_events();

        let workspace = app
            .model
            .workspace(&new_key)
            .expect("retagged local workspace");
        assert_eq!(workspace.revision(), local_revision);
        assert!(
            workspace
                .editor_tabs()
                .iter()
                .any(|tab| tab.text() == local_marker)
        );
        assert!(workspace.editor_tabs().iter().all(|tab| tab.text() != ""));
        assert!(
            app.workspace_persistence
                .get(&new_key)
                .is_some_and(|state| {
                    matches!(state.load, WorkspaceLoadPhase::Ready)
                        && state.durable_committed_bytes == submitted_bytes
                        && state.submitted_commit.is_none()
                })
        );
        assert!(service.try_next_command().is_none());
    }

    #[test]
    fn failed_execute_submit_keeps_live_history_revision_and_barrier_unchanged() {
        let (ui_port, mut service) = bounded_ports(1);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let profile = j2_classified_environment_profile();
        let key = install_j2_profile(&mut app, &profile);
        assert!(app.ensure_workspace_persistence_binding(&key, &profile));
        app.model
            .workspace_mut(key.clone())
            .create_editor_tab(QueryLanguage::Sql, "Busy", "SELECT must_not_reserve")
            .expect("busy editor");
        if let Some(state) = app.workspace_persistence.get_mut(&key) {
            state.load = WorkspaceLoadPhase::Ready;
            state.mode = Some(WorkspaceStoreMode::ReadWrite);
        }
        app.port
            .try_submit(UiCommand::Execute {
                operation_id: OperationId(999_999),
                profile_id: profile.id.clone(),
                profile_generation: profile.generation,
                editor_tab_id: None,
                language: QueryLanguage::Sql,
                text: "SELECT fills_work_lane".to_owned(),
                row_limit: 1,
                timeout_ms: 1,
            })
            .expect("fill work lane");
        let revision_before = app
            .model
            .workspace(&key)
            .map_or(u64::MAX, ProfileWorkspace::revision);
        let history_before = app
            .model
            .workspace(&key)
            .and_then(ProfileWorkspace::persistence)
            .expect("busy persistence")
            .history()
            .to_vec();
        let intent = build_execute_intent(
            &profile,
            app.model.workspace(&key).expect("busy workspace"),
            EditorCursor::caret(0),
        )
        .expect("busy execute intent");

        assert!(!app.submit_editor_execute(intent));
        assert_eq!(
            app.model.workspace(&key).map(ProfileWorkspace::revision),
            Some(revision_before)
        );
        assert_eq!(
            app.model
                .workspace(&key)
                .and_then(ProfileWorkspace::persistence)
                .expect("unchanged busy persistence")
                .history(),
            history_before
        );
        assert!(!app.retention_commit_barrier.is_pending_or_failed());
        assert!(app.pending_workspace_history.is_empty());
        assert!(matches!(
            service.try_next_command(),
            Some(UiCommand::Execute {
                operation_id: OperationId(999_999),
                ..
            })
        ));
        assert!(service.try_next_command().is_none());
    }

    #[test]
    fn terminal_waits_through_another_workspace_clear_then_drains_same_id() {
        let (ui_port, mut service) = bounded_ports(16);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let profiles = vec![
            j2_profile_with_identity("terminal-a", 0x41),
            j2_profile_with_identity("clearing-b", 0x42),
        ];
        let keys = install_j2_profiles_ready(&mut app, &profiles);
        let source_marker = "SELECT buffered_terminal_private_marker";
        let editor_tab_id = app
            .model
            .workspace_mut(keys[0].clone())
            .create_editor_tab(QueryLanguage::Sql, "Buffered", source_marker)
            .expect("buffered-terminal editor");
        let intent = build_execute_intent(
            &profiles[0],
            app.model.workspace(&keys[0]).expect("terminal workspace"),
            EditorCursor::caret(0),
        )
        .expect("terminal execute intent");
        assert!(app.submit_editor_execute(intent));
        let (provisional_operation, provisional_identity, provisional_revision) =
            match service.try_next_command() {
                Some(UiCommand::CommitWorkspace {
                    operation_id,
                    identity,
                    revision,
                    ..
                }) => (operation_id, identity, revision),
                command => panic!("expected provisional commit, got {command:?}"),
            };
        let execute_operation = match service.try_next_command() {
            Some(UiCommand::Execute { operation_id, .. }) => operation_id,
            command => panic!("expected one execute, got {command:?}"),
        };
        assert!(service.try_emit(UiEvent::WorkspaceCommitted {
            operation_id: provisional_operation,
            identity: provisional_identity,
            revision: provisional_revision,
            generation: 1,
            committed_bytes: submitted_committed_bytes_or_stale_sentinel(
                &app,
                provisional_operation,
                1,
            ),
            warnings: Vec::new(),
        }));
        app.poll_events();

        app.submit_clear_workspace(&keys[1]);
        let (clear_operation, clear_identity, clear_revision) = match service.try_next_command() {
            Some(UiCommand::ClearWorkspace {
                operation_id,
                identity,
                base_revision,
            }) => (operation_id, identity, base_revision),
            command => panic!("expected B clear, got {command:?}"),
        };
        assert!(service.try_emit(UiEvent::QueryFinished {
            operation_id: execute_operation,
            profile_id: profiles[0].id.clone(),
            profile_generation: profiles[0].generation,
            editor_tab_id: Some(editor_tab_id),
            session_generation: SessionGeneration(31),
            result: result_snapshot_for_operation(
                &profiles[0],
                "buffered-terminal-result",
                execute_operation,
                ResultId(8_301),
            ),
        }));
        app.poll_events();
        assert!(
            app.pending_workspace_history
                .contains_key(&execute_operation)
        );
        let provisional = app
            .model
            .workspace(&keys[0])
            .and_then(ProfileWorkspace::persistence)
            .expect("protected provisional history")
            .history();
        assert_eq!(provisional.len(), 1);
        assert_eq!(provisional[0].id(), execute_operation.0);
        assert_eq!(
            provisional[0].status(),
            WorkspaceHistoryStatus::OutcomeUnknown
        );
        assert!(!format!("{:?}", app.pending_workspace_history).contains(source_marker));
        assert!(service.try_next_command().is_none());

        assert!(service.try_emit(UiEvent::WorkspaceCleared {
            operation_id: clear_operation,
            identity: clear_identity.clone(),
            base_revision: clear_revision,
        }));
        app.poll_events();
        let (off_operation, off_revision) = match service.try_next_command() {
            Some(UiCommand::CommitWorkspace {
                operation_id,
                identity,
                revision,
                ..
            }) => {
                assert_eq!(identity, clear_identity);
                (operation_id, revision)
            }
            command => panic!("expected B Persistence Off commit, got {command:?}"),
        };
        assert!(service.try_emit(UiEvent::WorkspaceCommitted {
            operation_id: off_operation,
            identity: clear_identity,
            revision: off_revision,
            generation: 2,
            committed_bytes: submitted_committed_bytes_or_stale_sentinel(&app, off_operation, 2,),
            warnings: Vec::new(),
        }));
        app.poll_events();
        let (terminal_operation, terminal_identity, terminal_revision) =
            match service.try_next_command() {
                Some(UiCommand::CommitWorkspace {
                    operation_id,
                    identity,
                    revision,
                    snapshot,
                }) => {
                    assert_eq!(snapshot.history().len(), 1);
                    assert_eq!(snapshot.history()[0].id(), execute_operation.0);
                    assert_eq!(
                        snapshot.history()[0].status(),
                        WorkspaceHistoryStatus::Succeeded
                    );
                    (operation_id, identity, revision)
                }
                command => panic!("expected buffered A terminal commit, got {command:?}"),
            };
        assert_eq!(terminal_identity.profile_id(), &profiles[0].id);
        assert!(!format!("{terminal_identity:?}").contains(source_marker));
        assert!(service.try_emit(UiEvent::WorkspaceCommitted {
            operation_id: terminal_operation,
            identity: terminal_identity,
            revision: terminal_revision,
            generation: 3,
            committed_bytes: submitted_committed_bytes_or_stale_sentinel(
                &app,
                terminal_operation,
                3,
            ),
            warnings: Vec::new(),
        }));
        app.poll_events();
        assert!(app.pending_workspace_history.is_empty());
        assert!(service.try_next_command().is_none());
    }

    #[test]
    fn terminal_waits_for_an_ordinary_save_ack_before_draining() {
        let (ui_port, mut service) = bounded_ports(16);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let profiles = vec![
            j2_profile_with_identity("ordinary-terminal-a", 0x43),
            j2_profile_with_identity("ordinary-saving-b", 0x44),
        ];
        let keys = install_j2_profiles_ready(&mut app, &profiles);
        let editor_tab_id = app
            .model
            .workspace_mut(keys[0].clone())
            .create_editor_tab(QueryLanguage::Sql, "Terminal", "SELECT ordinary_terminal")
            .expect("ordinary-terminal editor");
        let intent = build_execute_intent(
            &profiles[0],
            app.model.workspace(&keys[0]).expect("terminal workspace"),
            EditorCursor::caret(0),
        )
        .expect("terminal intent");
        assert!(app.submit_editor_execute(intent));
        let (provisional_operation, provisional_identity, provisional_revision) =
            match service.try_next_command() {
                Some(UiCommand::CommitWorkspace {
                    operation_id,
                    identity,
                    revision,
                    ..
                }) => (operation_id, identity, revision),
                command => panic!("expected provisional commit, got {command:?}"),
            };
        let execute_operation = match service.try_next_command() {
            Some(UiCommand::Execute { operation_id, .. }) => operation_id,
            command => panic!("expected execute, got {command:?}"),
        };
        assert!(service.try_emit(UiEvent::WorkspaceCommitted {
            operation_id: provisional_operation,
            identity: provisional_identity,
            revision: provisional_revision,
            generation: 1,
            committed_bytes: submitted_committed_bytes_or_stale_sentinel(
                &app,
                provisional_operation,
                1,
            ),
            warnings: Vec::new(),
        }));
        app.poll_events();

        app.model
            .workspace_mut(keys[1].clone())
            .create_editor_tab(QueryLanguage::Sql, "Dirty", "SELECT ordinary_save")
            .expect("ordinary-save editor");
        app.observe_workspace_revisions(Instant::now());
        assert!(app.submit_workspace_commit(&keys[1], true));
        let (save_operation, save_identity, save_revision) = match service.try_next_command() {
            Some(UiCommand::CommitWorkspace {
                operation_id,
                identity,
                revision,
                ..
            }) => (operation_id, identity, revision),
            command => panic!("expected ordinary B save, got {command:?}"),
        };
        assert!(service.try_emit(UiEvent::QueryFinished {
            operation_id: execute_operation,
            profile_id: profiles[0].id.clone(),
            profile_generation: profiles[0].generation,
            editor_tab_id: Some(editor_tab_id),
            session_generation: SessionGeneration(32),
            result: result_snapshot_for_operation(
                &profiles[0],
                "ordinary-terminal-result",
                execute_operation,
                ResultId(8_302),
            ),
        }));
        app.poll_events();
        assert!(
            app.pending_workspace_history
                .contains_key(&execute_operation)
        );
        assert!(service.try_next_command().is_none());

        assert!(service.try_emit(UiEvent::WorkspaceCommitted {
            operation_id: save_operation,
            identity: save_identity,
            revision: save_revision,
            generation: 2,
            committed_bytes: submitted_committed_bytes_or_stale_sentinel(&app, save_operation, 2,),
            warnings: Vec::new(),
        }));
        app.poll_events();
        let (terminal_operation, terminal_identity, terminal_revision) =
            match service.try_next_command() {
                Some(UiCommand::CommitWorkspace {
                    operation_id,
                    identity,
                    revision,
                    snapshot,
                }) => {
                    assert_eq!(snapshot.history()[0].id(), execute_operation.0);
                    assert_eq!(
                        snapshot.history()[0].status(),
                        WorkspaceHistoryStatus::Succeeded
                    );
                    (operation_id, identity, revision)
                }
                command => panic!("expected terminal after ordinary ack, got {command:?}"),
            };
        assert!(service.try_emit(UiEvent::WorkspaceCommitted {
            operation_id: terminal_operation,
            identity: terminal_identity,
            revision: terminal_revision,
            generation: 3,
            committed_bytes: submitted_committed_bytes_or_stale_sentinel(
                &app,
                terminal_operation,
                3,
            ),
            warnings: Vec::new(),
        }));
        app.poll_events();
        assert!(app.pending_workspace_history.is_empty());
        assert!(service.try_next_command().is_none());
    }

    #[test]
    fn same_instance_retag_moves_buffered_terminal_to_the_new_key() {
        let (ui_port, mut service) = bounded_ports(16);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let profile = j2_profile_with_identity("retag-terminal", 0x45);
        let old_key = install_j2_profile(&mut app, &profile);
        assert!(app.ensure_workspace_persistence_binding(&old_key, &profile));
        let source_marker = "SELECT retagged_terminal_private_marker";
        let editor_tab_id = app
            .model
            .workspace_mut(old_key.clone())
            .create_editor_tab(QueryLanguage::Sql, "Retag terminal", source_marker)
            .expect("retag-terminal editor");
        if let Some(state) = app.workspace_persistence.get_mut(&old_key) {
            state.load = WorkspaceLoadPhase::Ready;
            state.mode = Some(WorkspaceStoreMode::ReadWrite);
        }
        let intent = build_execute_intent(
            &profile,
            app.model
                .workspace(&old_key)
                .expect("old terminal workspace"),
            EditorCursor::caret(0),
        )
        .expect("retag-terminal intent");
        assert!(app.submit_editor_execute(intent));
        let (provisional_operation, provisional_identity, provisional_revision) =
            match service.try_next_command() {
                Some(UiCommand::CommitWorkspace {
                    operation_id,
                    identity,
                    revision,
                    ..
                }) => (operation_id, identity, revision),
                command => panic!("expected provisional commit, got {command:?}"),
            };
        let execute_operation = match service.try_next_command() {
            Some(UiCommand::Execute { operation_id, .. }) => operation_id,
            command => panic!("expected execute, got {command:?}"),
        };
        assert!(service.try_emit(UiEvent::WorkspaceCommitted {
            operation_id: provisional_operation,
            identity: provisional_identity,
            revision: provisional_revision,
            generation: 1,
            committed_bytes: submitted_committed_bytes_or_stale_sentinel(
                &app,
                provisional_operation,
                1,
            ),
            warnings: Vec::new(),
        }));
        app.poll_events();

        let mut refreshed = profile.clone();
        refreshed.generation = ProfileGeneration(profile.generation.0.saturating_add(1));
        let new_key = WorkspaceKey::new(refreshed.id.clone(), refreshed.generation);
        assert!(service.try_emit(UiEvent::ProfilesLoaded {
            operation_id: OperationId(100),
            profiles: vec![refreshed.clone()],
            config: ConfigPresentation::for_source(
                ConfigSourceVersion::V3,
                &PathBuf::from("/private/tmp/dbotter-j2-terminal-retag.toml"),
            ),
        }));
        app.poll_events();
        assert!(app.pending_workspace_history.values().any(|pending| {
            pending.workspace_key == new_key && pending.history_id == execute_operation.0
        }));
        assert!(!format!("{:?}", app.pending_workspace_history).contains(source_marker));

        assert!(service.try_emit(UiEvent::QueryFinished {
            operation_id: execute_operation,
            profile_id: profile.id.clone(),
            profile_generation: profile.generation,
            editor_tab_id: Some(editor_tab_id),
            session_generation: SessionGeneration(33),
            result: result_snapshot_for_operation(
                &profile,
                "retag-terminal-result",
                execute_operation,
                ResultId(8_303),
            ),
        }));
        app.poll_events();
        let (terminal_operation, terminal_identity, terminal_revision) =
            match service.try_next_command() {
                Some(UiCommand::CommitWorkspace {
                    operation_id,
                    identity,
                    revision,
                    snapshot,
                }) => {
                    assert_eq!(identity.profile_generation(), refreshed.generation);
                    assert_eq!(snapshot.history().len(), 1);
                    assert_eq!(snapshot.history()[0].id(), execute_operation.0);
                    assert_eq!(
                        snapshot.history()[0].status(),
                        WorkspaceHistoryStatus::Succeeded
                    );
                    (operation_id, identity, revision)
                }
                command => panic!("expected retagged terminal commit, got {command:?}"),
            };
        assert!(service.try_emit(UiEvent::WorkspaceCommitted {
            operation_id: terminal_operation,
            identity: terminal_identity,
            revision: terminal_revision,
            generation: 2,
            committed_bytes: submitted_committed_bytes_or_stale_sentinel(
                &app,
                terminal_operation,
                2,
            ),
            warnings: Vec::new(),
        }));
        app.poll_events();
        assert!(app.pending_workspace_history.is_empty());
        assert!(service.try_next_command().is_none());
    }

    #[test]
    fn buffered_terminal_is_pruned_on_delete_or_different_instance_without_resurrection() {
        for replace_instance in [false, true] {
            let (ui_port, mut service) = bounded_ports(16);
            let mut app = DbotterApp::new(ui_port);
            assert!(service.try_next_command().is_some());
            let profiles = vec![
                j2_profile_with_identity("pruned-terminal-a", 0x46),
                j2_profile_with_identity("pruned-saving-b", 0x47),
            ];
            let keys = install_j2_profiles_ready(&mut app, &profiles);
            let source_marker = "SELECT pruned_terminal_private_marker";
            let editor_tab_id = app
                .model
                .workspace_mut(keys[0].clone())
                .create_editor_tab(QueryLanguage::Sql, "Pruned", source_marker)
                .expect("pruned-terminal editor");
            let intent = build_execute_intent(
                &profiles[0],
                app.model.workspace(&keys[0]).expect("pruned workspace"),
                EditorCursor::caret(0),
            )
            .expect("pruned-terminal intent");
            assert!(app.submit_editor_execute(intent));
            let (provisional_operation, provisional_identity, provisional_revision) =
                match service.try_next_command() {
                    Some(UiCommand::CommitWorkspace {
                        operation_id,
                        identity,
                        revision,
                        ..
                    }) => (operation_id, identity, revision),
                    command => panic!("expected provisional commit, got {command:?}"),
                };
            let execute_operation = match service.try_next_command() {
                Some(UiCommand::Execute { operation_id, .. }) => operation_id,
                command => panic!("expected one execute, got {command:?}"),
            };
            assert!(service.try_emit(UiEvent::WorkspaceCommitted {
                operation_id: provisional_operation,
                identity: provisional_identity,
                revision: provisional_revision,
                generation: 1,
                committed_bytes: submitted_committed_bytes_or_stale_sentinel(
                    &app,
                    provisional_operation,
                    1,
                ),
                warnings: Vec::new(),
            }));
            app.poll_events();
            app.model
                .workspace_mut(keys[1].clone())
                .create_editor_tab(QueryLanguage::Sql, "Saving", "SELECT blocks_terminal")
                .expect("blocking save editor");
            app.observe_workspace_revisions(Instant::now());
            assert!(app.submit_workspace_commit(&keys[1], true));
            let (save_operation, save_identity, save_revision) = match service.try_next_command() {
                Some(UiCommand::CommitWorkspace {
                    operation_id,
                    identity,
                    revision,
                    ..
                }) => (operation_id, identity, revision),
                command => panic!("expected blocking B save, got {command:?}"),
            };
            assert!(service.try_emit(UiEvent::QueryFinished {
                operation_id: execute_operation,
                profile_id: profiles[0].id.clone(),
                profile_generation: profiles[0].generation,
                editor_tab_id: Some(editor_tab_id),
                session_generation: SessionGeneration(34),
                result: result_snapshot_for_operation(
                    &profiles[0],
                    "pruned-terminal-result",
                    execute_operation,
                    ResultId(8_304 + u64::from(replace_instance)),
                ),
            }));
            app.poll_events();
            assert!(
                app.pending_workspace_history
                    .contains_key(&execute_operation)
            );
            assert!(!format!("{:?}", app.pending_workspace_history).contains(source_marker));

            let mut refreshed_profiles = vec![profiles[1].clone()];
            if replace_instance {
                let mut replacement = profiles[0].clone();
                replacement.generation =
                    ProfileGeneration(replacement.generation.0.saturating_add(1));
                replacement.persisted.safety = ProfileSafetyPosture::classified(
                    ProfileEnvironment::Development,
                    ProfileAccess::ReadWrite,
                    ProfileInstanceId::from_bytes([0x48; 16]),
                );
                refreshed_profiles.insert(0, replacement);
            }
            assert!(service.try_emit(UiEvent::ProfilesLoaded {
                operation_id: OperationId(100),
                profiles: refreshed_profiles,
                config: ConfigPresentation::for_source(
                    ConfigSourceVersion::V3,
                    &PathBuf::from("/private/tmp/dbotter-j2-pruned-terminal.toml"),
                ),
            }));
            app.poll_events();
            assert!(
                !app.pending_workspace_history
                    .contains_key(&execute_operation)
            );

            let replacement_load = if replace_instance {
                match service.try_next_command() {
                    Some(UiCommand::LoadWorkspace {
                        operation_id,
                        identity,
                        base_revision,
                    }) => Some((operation_id, identity, base_revision)),
                    command => panic!("replacement may only restore its new identity: {command:?}"),
                }
            } else {
                None
            };
            assert!(service.try_emit(UiEvent::WorkspaceCommitted {
                operation_id: save_operation,
                identity: save_identity,
                revision: save_revision,
                generation: 2,
                committed_bytes: submitted_committed_bytes_or_stale_sentinel(
                    &app,
                    save_operation,
                    2,
                ),
                warnings: Vec::new(),
            }));
            app.poll_events();
            if let Some((operation_id, identity, base_revision)) = replacement_load {
                assert!(service.try_emit(UiEvent::WorkspaceLoaded {
                    operation_id,
                    identity,
                    base_revision,
                    mode: WorkspaceStoreMode::ReadWrite,
                    read_only_reason: None,
                    generation: None,
                    committed_bytes: 0,
                    snapshot: None,
                }));
                app.poll_events();
            }
            while let Some(command) = service.try_next_command() {
                assert!(!matches!(
                    command,
                    UiCommand::Execute { .. } | UiCommand::ExecuteBatch { .. }
                ));
                assert!(
                    !matches!(command, UiCommand::CommitWorkspace { ref snapshot, .. }
                        if snapshot.history().iter().any(|entry| entry.id() == execute_operation.0)),
                    "deleted or replaced terminal must never resurrect"
                );
            }
        }
    }

    #[test]
    fn terminal_replaces_the_reserved_history_id_before_or_after_provisional_ack() {
        for terminal_before_ack in [false, true] {
            let (ui_port, mut service) = bounded_ports(8);
            let mut app = DbotterApp::new(ui_port);
            assert!(service.try_next_command().is_some());
            let profile = j2_classified_environment_profile();
            let key = install_j2_profile(&mut app, &profile);
            assert!(app.ensure_workspace_persistence_binding(&key, &profile));
            let editor_tab_id = app
                .model
                .workspace_mut(key.clone())
                .create_editor_tab(QueryLanguage::Sql, "Same ID", "SELECT same_history_id")
                .expect("same-id editor");
            if let Some(state) = app.workspace_persistence.get_mut(&key) {
                state.load = WorkspaceLoadPhase::Ready;
                state.mode = Some(WorkspaceStoreMode::ReadWrite);
            }
            let intent = build_execute_intent(
                &profile,
                app.model.workspace(&key).expect("same-id workspace"),
                EditorCursor::caret(0),
            )
            .expect("same-id execute intent");
            assert!(app.submit_editor_execute(intent));
            let (provisional_commit, identity, provisional_revision) =
                match service.try_next_command() {
                    Some(UiCommand::CommitWorkspace {
                        operation_id,
                        identity,
                        revision,
                        snapshot,
                    }) => {
                        assert_eq!(snapshot.history().len(), 1);
                        assert_eq!(
                            snapshot.history()[0].status(),
                            WorkspaceHistoryStatus::OutcomeUnknown
                        );
                        (operation_id, identity, revision)
                    }
                    command => panic!("expected provisional commit, got {command:?}"),
                };
            let operation_id = match service.try_next_command() {
                Some(UiCommand::Execute { operation_id, .. }) => operation_id,
                command => panic!("expected accepted execute, got {command:?}"),
            };
            let reserved_id = app
                .model
                .workspace(&key)
                .and_then(ProfileWorkspace::persistence)
                .expect("provisional history")
                .history()[0]
                .id();
            assert_eq!(reserved_id, operation_id.0);
            let terminal = UiEvent::QueryFinished {
                operation_id,
                profile_id: profile.id.clone(),
                profile_generation: profile.generation,
                editor_tab_id: Some(editor_tab_id),
                session_generation: SessionGeneration(7),
                result: result_snapshot_for_operation(
                    &profile,
                    "same-id-result",
                    operation_id,
                    ResultId(8_001 + u64::from(terminal_before_ack)),
                ),
            };

            if terminal_before_ack {
                assert!(service.try_emit(terminal.clone()));
                app.poll_events();
                let history = app
                    .model
                    .workspace(&key)
                    .and_then(ProfileWorkspace::persistence)
                    .expect("terminal-before history")
                    .history();
                assert_eq!(history.len(), 1);
                assert_eq!(history[0].id(), reserved_id);
                assert_eq!(history[0].status(), WorkspaceHistoryStatus::OutcomeUnknown);
                assert!(app.pending_workspace_history.contains_key(&operation_id));
                assert!(service.try_next_command().is_none());
            }
            assert!(service.try_emit(UiEvent::WorkspaceCommitted {
                operation_id: provisional_commit,
                identity: identity.clone(),
                revision: provisional_revision,
                generation: 1,
                committed_bytes: submitted_committed_bytes_or_stale_sentinel(
                    &app,
                    provisional_commit,
                    1,
                ),
                warnings: Vec::new(),
            }));
            app.poll_events();
            if terminal_before_ack {
                assert!(
                    app.model
                        .workspace(&key)
                        .is_some_and(|workspace| !workspace.is_saved()),
                    "an older provisional ack must never mark a newer terminal revision Saved"
                );
                assert!(app.workspace_persistence.get(&key).is_some_and(|state| {
                    matches!(state.save, WorkspaceSavePhase::Saving { .. })
                }));
            }

            if !terminal_before_ack {
                let provisional = app
                    .model
                    .workspace(&key)
                    .and_then(ProfileWorkspace::persistence)
                    .expect("acked provisional history")
                    .history();
                assert_eq!(provisional.len(), 1);
                assert_eq!(provisional[0].id(), reserved_id);
                assert_eq!(
                    provisional[0].status(),
                    WorkspaceHistoryStatus::OutcomeUnknown
                );
                assert!(service.try_emit(terminal));
                app.poll_events();
            }
            let (terminal_commit, terminal_revision) = match service.try_next_command() {
                Some(UiCommand::CommitWorkspace {
                    operation_id,
                    revision,
                    snapshot,
                    ..
                }) => {
                    assert_eq!(snapshot.history().len(), 1);
                    assert_eq!(snapshot.history()[0].id(), reserved_id);
                    assert_eq!(
                        snapshot.history()[0].status(),
                        WorkspaceHistoryStatus::Succeeded
                    );
                    (operation_id, revision)
                }
                command => panic!("expected same-id terminal commit, got {command:?}"),
            };
            assert!(service.try_emit(UiEvent::WorkspaceCommitted {
                operation_id: terminal_commit,
                identity,
                revision: terminal_revision,
                generation: 2,
                committed_bytes: submitted_committed_bytes_or_stale_sentinel(
                    &app,
                    terminal_commit,
                    2,
                ),
                warnings: Vec::new(),
            }));
            app.poll_events();
            assert!(!app.retention_commit_barrier.is_pending_or_failed());
            assert!(
                app.model
                    .workspace(&key)
                    .is_some_and(ProfileWorkspace::is_saved)
            );
            let history = app
                .model
                .workspace(&key)
                .and_then(ProfileWorkspace::persistence)
                .expect("durable same-id history")
                .history();
            assert_eq!(history.len(), 1);
            assert_eq!(history[0].id(), reserved_id);
            assert_eq!(history[0].status(), WorkspaceHistoryStatus::Succeeded);
            assert!(service.try_next_command().is_none());
        }
    }

    #[test]
    fn persistent_single_and_batch_keep_pending_cancel_and_terminal_correlation() {
        {
            let (ui_port, mut service) = bounded_ports(8);
            let mut app = DbotterApp::new(ui_port);
            assert!(service.try_next_command().is_some());
            let profile = j2_classified_environment_profile();
            let key = install_j2_profile(&mut app, &profile);
            assert!(app.ensure_workspace_persistence_binding(&key, &profile));
            let editor_tab_id = app
                .model
                .workspace_mut(key.clone())
                .create_editor_tab(QueryLanguage::Sql, "Pending", "SELECT pending_single")
                .expect("persistent single editor");
            if let Some(state) = app.workspace_persistence.get_mut(&key) {
                state.load = WorkspaceLoadPhase::Ready;
                state.mode = Some(WorkspaceStoreMode::ReadWrite);
            }
            let intent = build_execute_intent(
                &profile,
                app.model
                    .workspace(&key)
                    .expect("persistent single workspace"),
                EditorCursor::caret(0),
            )
            .expect("persistent single intent");
            assert!(app.submit_editor_execute(intent));
            let pending = app
                .model
                .workspace(&key)
                .and_then(|workspace| workspace.pending_execute)
                .expect("persistent single pending");
            let (provisional_operation, provisional_identity, provisional_revision) =
                match service.try_next_command() {
                    Some(UiCommand::CommitWorkspace {
                        operation_id,
                        identity,
                        revision,
                        ..
                    }) => (operation_id, identity, revision),
                    command => panic!("expected single provisional commit, got {command:?}"),
                };
            assert!(matches!(
                service.try_next_command(),
                Some(UiCommand::Execute { operation_id, .. }) if operation_id == pending
            ));
            app.submit_editor_intent(EditorIntent::Cancel {
                operation_id: pending,
            });
            assert!(matches!(
                service.try_next_command(),
                Some(UiCommand::CancelOperation { operation_id }) if operation_id == pending
            ));
            assert_eq!(
                app.model
                    .workspace(&key)
                    .and_then(|workspace| workspace.pending_execute),
                Some(pending)
            );
            assert!(service.try_emit(UiEvent::QueryFinished {
                operation_id: pending,
                profile_id: profile.id.clone(),
                profile_generation: profile.generation,
                editor_tab_id: Some(editor_tab_id),
                session_generation: SessionGeneration(11),
                result: result_snapshot_for_operation(
                    &profile,
                    "persistent-single-result",
                    pending,
                    ResultId(8_201),
                ),
            }));
            app.poll_events();
            let provisional_history = app
                .model
                .workspace(&key)
                .and_then(ProfileWorkspace::persistence)
                .expect("single provisional history")
                .history();
            assert_eq!(provisional_history.len(), 1);
            assert_eq!(provisional_history[0].id(), pending.0);
            assert_eq!(
                provisional_history[0].status(),
                WorkspaceHistoryStatus::OutcomeUnknown
            );
            assert!(app.pending_workspace_history.contains_key(&pending));
            assert!(service.try_next_command().is_none());
            assert!(service.try_emit(UiEvent::WorkspaceCommitted {
                operation_id: provisional_operation,
                identity: provisional_identity.clone(),
                revision: provisional_revision,
                generation: 1,
                committed_bytes: submitted_committed_bytes_or_stale_sentinel(
                    &app,
                    provisional_operation,
                    1,
                ),
                warnings: Vec::new(),
            }));
            app.poll_events();
            let (terminal_operation, terminal_revision) = match service.try_next_command() {
                Some(UiCommand::CommitWorkspace {
                    operation_id,
                    revision,
                    snapshot,
                    ..
                }) => {
                    assert_eq!(snapshot.history().len(), 1);
                    assert_eq!(snapshot.history()[0].id(), pending.0);
                    assert_eq!(
                        snapshot.history()[0].status(),
                        WorkspaceHistoryStatus::Succeeded
                    );
                    (operation_id, revision)
                }
                command => panic!("expected single terminal commit, got {command:?}"),
            };
            assert!(service.try_emit(UiEvent::WorkspaceCommitted {
                operation_id: terminal_operation,
                identity: provisional_identity,
                revision: terminal_revision,
                generation: 2,
                committed_bytes: submitted_committed_bytes_or_stale_sentinel(
                    &app,
                    terminal_operation,
                    2,
                ),
                warnings: Vec::new(),
            }));
            app.poll_events();
            let workspace = app
                .model
                .workspace(&key)
                .expect("single terminal workspace");
            assert!(workspace.pending_execute.is_none());
            assert_eq!(
                workspace
                    .result_tabs()
                    .last()
                    .map(|tab| tab.snapshot().provenance.operation_id),
                Some(pending)
            );
            let history = workspace
                .persistence()
                .expect("single terminal history")
                .history();
            assert_eq!(history.len(), 1);
            assert_eq!(history[0].id(), pending.0);
            assert_eq!(history[0].status(), WorkspaceHistoryStatus::Succeeded);
        }

        {
            let (ui_port, mut service) = bounded_ports(8);
            let mut app = DbotterApp::new(ui_port);
            assert!(service.try_next_command().is_some());
            let profile = j2_classified_environment_profile();
            let key = install_j2_profile(&mut app, &profile);
            assert!(app.ensure_workspace_persistence_binding(&key, &profile));
            let editor_tab_id = app
                .model
                .workspace_mut(key.clone())
                .create_editor_tab(QueryLanguage::Sql, "Pending batch", "SELECT 1;\nSELECT 2;")
                .expect("persistent batch editor");
            if let Some(state) = app.workspace_persistence.get_mut(&key) {
                state.load = WorkspaceLoadPhase::Ready;
                state.mode = Some(WorkspaceStoreMode::ReadWrite);
            }
            let intent = build_execute_all_intent(
                &profile,
                app.model
                    .workspace(&key)
                    .expect("persistent batch workspace"),
            )
            .expect("persistent batch intent");
            app.submit_editor_batch(intent);
            let pending = app
                .model
                .workspace(&key)
                .and_then(|workspace| workspace.pending_execute)
                .expect("persistent batch pending");
            let (provisional_operation, provisional_identity, provisional_revision) =
                match service.try_next_command() {
                    Some(UiCommand::CommitWorkspace {
                        operation_id,
                        identity,
                        revision,
                        ..
                    }) => (operation_id, identity, revision),
                    command => panic!("expected batch provisional commit, got {command:?}"),
                };
            assert!(matches!(
                service.try_next_command(),
                Some(UiCommand::ExecuteBatch { operation_id, .. }) if operation_id == pending
            ));
            app.submit_editor_intent(EditorIntent::Cancel {
                operation_id: pending,
            });
            assert!(matches!(
                service.try_next_command(),
                Some(UiCommand::CancelOperation { operation_id }) if operation_id == pending
            ));
            assert_eq!(
                app.model
                    .workspace(&key)
                    .and_then(|workspace| workspace.pending_execute),
                Some(pending)
            );
            assert!(service.try_emit(UiEvent::QueryBatchFinished {
                operation_id: pending,
                profile_id: profile.id.clone(),
                profile_generation: profile.generation,
                editor_tab_id: Some(editor_tab_id),
                session_generation: SessionGeneration(12),
                target_count: 2,
                completed_targets: 2,
                discarded_results: 0,
                results: vec![
                    result_snapshot_for_operation(
                        &profile,
                        "persistent-batch-one",
                        pending,
                        ResultId(8_202),
                    ),
                    result_snapshot_for_operation(
                        &profile,
                        "persistent-batch-two",
                        pending,
                        ResultId(8_203),
                    ),
                ],
                error: None,
                session_disposition: SessionDisposition::Keep,
            }));
            app.poll_events();
            let provisional_history = app
                .model
                .workspace(&key)
                .and_then(ProfileWorkspace::persistence)
                .expect("batch provisional history")
                .history();
            assert_eq!(provisional_history.len(), 1);
            assert_eq!(provisional_history[0].id(), pending.0);
            assert_eq!(
                provisional_history[0].status(),
                WorkspaceHistoryStatus::OutcomeUnknown
            );
            assert!(app.pending_workspace_history.contains_key(&pending));
            assert!(service.try_next_command().is_none());
            assert!(service.try_emit(UiEvent::WorkspaceCommitted {
                operation_id: provisional_operation,
                identity: provisional_identity.clone(),
                revision: provisional_revision,
                generation: 1,
                committed_bytes: submitted_committed_bytes_or_stale_sentinel(
                    &app,
                    provisional_operation,
                    1,
                ),
                warnings: Vec::new(),
            }));
            app.poll_events();
            let (terminal_operation, terminal_revision) = match service.try_next_command() {
                Some(UiCommand::CommitWorkspace {
                    operation_id,
                    revision,
                    snapshot,
                    ..
                }) => {
                    assert_eq!(snapshot.history().len(), 1);
                    assert_eq!(snapshot.history()[0].id(), pending.0);
                    assert_eq!(
                        snapshot.history()[0].status(),
                        WorkspaceHistoryStatus::Succeeded
                    );
                    (operation_id, revision)
                }
                command => panic!("expected batch terminal commit, got {command:?}"),
            };
            assert!(service.try_emit(UiEvent::WorkspaceCommitted {
                operation_id: terminal_operation,
                identity: provisional_identity,
                revision: terminal_revision,
                generation: 2,
                committed_bytes: submitted_committed_bytes_or_stale_sentinel(
                    &app,
                    terminal_operation,
                    2,
                ),
                warnings: Vec::new(),
            }));
            app.poll_events();
            let workspace = app.model.workspace(&key).expect("batch terminal workspace");
            assert!(workspace.pending_execute.is_none());
            assert_eq!(workspace.result_tabs().len(), 2);
            assert!(workspace.result_tabs().iter().all(|tab| {
                tab.snapshot().provenance.operation_id == pending
                    && tab.origin_editor_tab_id() == Some(editor_tab_id)
            }));
            let history = workspace
                .persistence()
                .expect("batch terminal history")
                .history();
            assert_eq!(history.len(), 1);
            assert_eq!(history[0].id(), pending.0);
            assert_eq!(history[0].status(), WorkspaceHistoryStatus::Succeeded);
        }
    }

    #[test]
    fn superseded_retention_ack_never_marks_saved_and_close_retry_replans() {
        let (ui_port, mut service) = bounded_ports(8);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let profile = j2_classified_environment_profile();
        let key = install_j2_profile(&mut app, &profile);
        assert!(app.ensure_workspace_persistence_binding(&key, &profile));
        app.model
            .workspace_mut(key.clone())
            .create_editor_tab(
                QueryLanguage::Sql,
                "Superseded",
                "SELECT superseded_history",
            )
            .expect("superseded editor");
        if let Some(state) = app.workspace_persistence.get_mut(&key) {
            state.load = WorkspaceLoadPhase::Ready;
            state.mode = Some(WorkspaceStoreMode::ReadWrite);
        }
        let intent = build_execute_intent(
            &profile,
            app.model.workspace(&key).expect("superseded workspace"),
            EditorCursor::caret(0),
        )
        .expect("superseded execute");
        assert!(app.submit_editor_execute(intent));
        let (old_operation, identity, old_revision) = match service.try_next_command() {
            Some(UiCommand::CommitWorkspace {
                operation_id,
                identity,
                revision,
                ..
            }) => (operation_id, identity, revision),
            command => panic!("expected provisional save, got {command:?}"),
        };
        assert!(matches!(
            service.try_next_command(),
            Some(UiCommand::Execute { .. })
        ));
        assert!(service.try_emit(UiEvent::WorkspaceCommitSuperseded {
            operation_id: old_operation,
            identity: identity.clone(),
            revision: old_revision,
            superseded_by: OperationId(99_001),
            superseded_by_revision: old_revision.saturating_add(1),
        }));
        app.poll_events();
        assert!(app.retention_commit_barrier.failure.is_some());
        assert!(app.has_uncommitted_workspace());
        assert!(app.has_workspace_save_failure());
        assert!(
            app.model
                .workspace(&key)
                .is_some_and(|workspace| !workspace.is_saved())
        );

        assert!(service.try_emit(UiEvent::WorkspaceCommitted {
            operation_id: old_operation,
            identity: identity.clone(),
            revision: old_revision,
            generation: 1,
            committed_bytes: submitted_committed_bytes_or_stale_sentinel(&app, old_operation, 1,),
            warnings: Vec::new(),
        }));
        app.poll_events();
        assert!(app.retention_commit_barrier.failure.is_some());
        assert!(
            app.model
                .workspace(&key)
                .is_some_and(|workspace| !workspace.is_saved()),
            "a late superseded ack must not bypass the barrier"
        );

        app.flush_all_dirty_workspaces();
        let (retry_operation, retry_revision) = match service.try_next_command() {
            Some(UiCommand::CommitWorkspace {
                operation_id,
                revision,
                snapshot,
                ..
            }) => {
                assert_eq!(snapshot.history().len(), 1);
                assert_eq!(
                    snapshot.history()[0].status(),
                    WorkspaceHistoryStatus::OutcomeUnknown
                );
                (operation_id, revision)
            }
            command => panic!("close retry must replan the live bounded set, got {command:?}"),
        };
        assert!(service.try_emit(UiEvent::WorkspaceCommitted {
            operation_id: retry_operation,
            identity,
            revision: retry_revision,
            generation: 2,
            committed_bytes: submitted_committed_bytes_or_stale_sentinel(&app, retry_operation, 2,),
            warnings: Vec::new(),
        }));
        app.poll_events();
        assert!(!app.retention_commit_barrier.is_pending_or_failed());
        assert!(
            app.model
                .workspace(&key)
                .is_some_and(ProfileWorkspace::is_saved)
        );
    }

    #[test]
    fn busy_retention_lane_keeps_live_provisional_and_autosave_resumes() {
        let (ui_port, mut service) = bounded_ports(1);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let profile = j2_classified_environment_profile();
        let key = install_j2_profile(&mut app, &profile);
        assert!(app.ensure_workspace_persistence_binding(&key, &profile));
        app.model
            .workspace_mut(key.clone())
            .create_editor_tab(QueryLanguage::Sql, "Busy store", "SELECT busy_store")
            .expect("busy-store editor");
        if let Some(state) = app.workspace_persistence.get_mut(&key) {
            state.load = WorkspaceLoadPhase::Ready;
            state.mode = Some(WorkspaceStoreMode::ReadWrite);
        }
        let (identity, revision) = app
            .workspace_persistence
            .get(&key)
            .zip(app.model.workspace(&key))
            .map(|(state, workspace)| (state.identity.clone(), workspace.revision()))
            .expect("busy-store binding");
        app.port
            .try_submit(UiCommand::LoadWorkspace {
                operation_id: OperationId(88_001),
                identity,
                base_revision: revision,
            })
            .expect("fill workspace lane");
        let intent = build_execute_intent(
            &profile,
            app.model.workspace(&key).expect("busy-store workspace"),
            EditorCursor::caret(0),
        )
        .expect("busy-store execute");

        assert!(app.submit_editor_execute(intent));
        assert!(matches!(
            app.retention_commit_barrier.failure,
            Some(super::RetentionBarrierFailure::Submit(
                crate::ui::SubmitError::Busy
            ))
        ));
        assert!(app.has_uncommitted_workspace());
        assert!(app.has_workspace_save_failure());
        let history = app
            .model
            .workspace(&key)
            .and_then(ProfileWorkspace::persistence)
            .expect("busy provisional")
            .history();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].status(), WorkspaceHistoryStatus::OutcomeUnknown);
        assert!(matches!(
            service.try_next_command(),
            Some(UiCommand::LoadWorkspace {
                operation_id: OperationId(88_001),
                ..
            })
        ));

        app.autosave_workspaces(Instant::now() + Duration::from_secs(1));
        let (commit_operation, identity, commit_revision) = match service.try_next_command() {
            Some(UiCommand::CommitWorkspace {
                operation_id,
                identity,
                revision,
                ..
            }) => (operation_id, identity, revision),
            command => panic!("autosave must resume the Busy barrier, got {command:?}"),
        };
        assert!(matches!(
            service.try_next_command(),
            Some(UiCommand::Execute { .. })
        ));
        assert!(
            service.try_emit(UiEvent::WorkspaceCommitted {
                operation_id: commit_operation,
                identity,
                revision: commit_revision,
                generation: 1,
                committed_bytes: submitted_committed_bytes_or_stale_sentinel(
                    &app,
                    commit_operation,
                    1,
                ),
                warnings: Vec::new(),
            })
        );
        app.poll_events();
        assert!(!app.retention_commit_barrier.is_pending_or_failed());
        assert!(
            app.model
                .workspace(&key)
                .is_some_and(ProfileWorkspace::is_saved)
        );
    }

    #[test]
    fn read_only_history_stays_bounded_unsaved_and_does_not_block_later_queries() {
        let (ui_port, mut service) = bounded_ports(8);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let profile = j2_classified_environment_profile();
        let key = install_j2_profile(&mut app, &profile);
        assert!(app.ensure_workspace_persistence_binding(&key, &profile));
        let editor_tab_id = app
            .model
            .workspace_mut(key.clone())
            .create_editor_tab(QueryLanguage::Sql, "Read only", "SELECT read_only_history")
            .expect("read-only editor");
        if let Some(state) = app.workspace_persistence.get_mut(&key) {
            state.load = WorkspaceLoadPhase::Ready;
            state.mode = Some(WorkspaceStoreMode::ReadOnly);
        }
        let first_intent = build_execute_intent(
            &profile,
            app.model.workspace(&key).expect("read-only workspace"),
            EditorCursor::caret(0),
        )
        .expect("first read-only intent");
        assert!(app.submit_editor_execute(first_intent));
        assert!(app.model.status.contains("Unsaved"));
        let first_operation = match service.try_next_command() {
            Some(UiCommand::Execute { operation_id, .. }) => operation_id,
            command => panic!("read-only execution must not enqueue a store write: {command:?}"),
        };
        assert!(service.try_next_command().is_none());
        assert!(service.try_emit(UiEvent::QueryFinished {
            operation_id: first_operation,
            profile_id: profile.id.clone(),
            profile_generation: profile.generation,
            editor_tab_id: Some(editor_tab_id),
            session_generation: SessionGeneration(9),
            result: result_snapshot_for_operation(
                &profile,
                "read-only-result",
                first_operation,
                ResultId(8_101),
            ),
        }));
        app.poll_events();
        assert!(app.model.status.contains("Unsaved"));
        assert!(!app.retention_commit_barrier.is_pending_or_failed());
        let first_history_id = {
            let history = app
                .model
                .workspace(&key)
                .and_then(ProfileWorkspace::persistence)
                .expect("read-only history")
                .history();
            assert_eq!(history.len(), 1);
            assert_eq!(history[0].status(), WorkspaceHistoryStatus::Succeeded);
            history[0].id()
        };
        assert!(app.has_uncommitted_workspace());
        assert!(app.has_workspace_save_failure());

        let second_intent = build_execute_intent(
            &profile,
            app.model
                .workspace(&key)
                .expect("later read-only workspace"),
            EditorCursor::caret(0),
        )
        .expect("later read-only intent");
        assert!(app.submit_editor_execute(second_intent));
        let second_operation = match service.try_next_command() {
            Some(UiCommand::Execute { operation_id, .. }) => operation_id,
            command => panic!("later read-only execution must remain available: {command:?}"),
        };
        assert_ne!(second_operation.0, first_history_id);
        let history = app
            .model
            .workspace(&key)
            .and_then(ProfileWorkspace::persistence)
            .expect("later read-only history")
            .history();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].id(), first_history_id);
        assert_eq!(history[0].status(), WorkspaceHistoryStatus::Succeeded);
        assert_eq!(history[1].id(), second_operation.0);
        assert_eq!(history[1].status(), WorkspaceHistoryStatus::OutcomeUnknown);
        assert!(app.model.status.contains("Unsaved"));
        assert!(service.try_next_command().is_none());
    }

    #[test]
    fn startup_restore_reconciles_global_retention_without_database_dispatch() {
        let (ui_port, mut service) = bounded_ports(16);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let base = j2_classified_environment_profile();
        let profiles = (0_u8..6)
            .map(|index| {
                let mut profile = base.clone();
                let id = format!("startup-retention-{index}");
                profile.id = ProfileId(id.clone());
                profile.name = id.clone();
                profile.persisted.id = id.clone();
                profile.persisted.name = id;
                profile.persisted.safety = ProfileSafetyPosture::classified(
                    ProfileEnvironment::Development,
                    ProfileAccess::ReadWrite,
                    ProfileInstanceId::from_bytes([index.saturating_add(17); 16]),
                );
                profile
            })
            .collect::<Vec<_>>();
        app.model.profiles.clone_from(&profiles);
        app.model.selected_profile = Some(profiles[0].id.clone());
        app.model.config = ConfigPresentation::for_source(
            ConfigSourceVersion::V3,
            &PathBuf::from("/private/tmp/dbotter-j2-startup-retention.toml"),
        );
        let mut snapshots = HashMap::new();
        for (profile_index, profile) in profiles.iter().enumerate() {
            app.model
                .active_generations
                .insert(profile.id.clone(), profile.generation);
            let key = WorkspaceKey::new(profile.id.clone(), profile.generation);
            assert!(app.ensure_workspace_persistence_binding(&key, profile));
            let history_count = if profile_index < 5 {
                MAX_HISTORY_ENTRIES_PER_PROFILE
            } else {
                1
            };
            let history = (1..=history_count)
                .map(|id| {
                    let timestamp = i64::try_from(
                        profile_index
                            .saturating_mul(MAX_HISTORY_ENTRIES_PER_PROFILE)
                            .saturating_add(id),
                    )
                    .expect("startup timestamp");
                    WorkspaceHistoryEntry::new(
                        u64::try_from(id).expect("startup history id"),
                        "SELECT startup_retained",
                        WorkspaceRunTarget::Current,
                        timestamp,
                        WorkspaceHistoryStatus::Succeeded,
                        1,
                        1,
                        0,
                        false,
                    )
                    .expect("startup history")
                })
                .collect::<Vec<_>>();
            let persistence = super::ProfileWorkspacePersistence::for_classified_profile(
                &profile.persisted,
                true,
                crate::workspace::WorkspaceGeometrySnapshot::new(320.0, 0.65, true)
                    .expect("startup geometry"),
                history,
            )
            .expect("startup persistence");
            let mut restored = ProfileWorkspace::default();
            restored
                .bind_persistence(persistence)
                .expect("startup bind");
            snapshots.insert(
                profile
                    .persisted
                    .safety
                    .instance_id()
                    .expect("startup instance"),
                Box::new(
                    restored
                        .to_persistence_snapshot()
                        .expect("startup snapshot"),
                ),
            );
            app.request_workspace_load(&key);
        }
        let mut loads = Vec::new();
        for _ in &profiles {
            match service.try_next_command() {
                Some(UiCommand::LoadWorkspace {
                    operation_id,
                    identity,
                    base_revision,
                }) => loads.push((operation_id, identity, base_revision)),
                command => panic!("startup must only request restores, got {command:?}"),
            }
        }
        assert!(service.try_next_command().is_none());
        for (operation_id, identity, base_revision) in loads {
            let snapshot = snapshots
                .remove(&identity.instance_id())
                .expect("exact startup snapshot");
            let committed_bytes = encoded_profile_bytes_at_generation(&snapshot, 1)
                .expect("startup snapshot bytes")
                .1;
            assert!(service.try_emit(UiEvent::WorkspaceLoaded {
                operation_id,
                identity,
                base_revision,
                mode: WorkspaceStoreMode::ReadWrite,
                read_only_reason: None,
                generation: Some(1),
                committed_bytes,
                snapshot: Some(snapshot),
            }));
            app.poll_events();
        }
        let oldest_instance = profiles[0]
            .persisted
            .safety
            .instance_id()
            .expect("oldest startup instance");
        let (commit_operation, identity, revision) = match service.try_next_command() {
            Some(UiCommand::CommitWorkspace {
                operation_id,
                identity,
                revision,
                snapshot,
            }) => {
                assert_eq!(identity.instance_id(), oldest_instance);
                assert_eq!(
                    snapshot.history().len(),
                    MAX_HISTORY_ENTRIES_PER_PROFILE - 1
                );
                assert!(snapshot.history().iter().all(|entry| entry.id() != 1));
                (operation_id, identity, revision)
            }
            command => panic!("startup reconciliation must submit one shrink, got {command:?}"),
        };
        assert!(
            service.try_emit(UiEvent::WorkspaceCommitted {
                operation_id: commit_operation,
                identity,
                revision,
                generation: 1,
                committed_bytes: submitted_committed_bytes_or_stale_sentinel(
                    &app,
                    commit_operation,
                    1,
                ),
                warnings: Vec::new(),
            })
        );
        app.poll_events();
        assert!(!app.retention_commit_barrier.is_pending_or_failed());
        assert!(service.try_next_command().is_none());
        assert!(app.active_operations.is_empty());
        assert!(app.pending_workspace_history.is_empty());
    }

    #[test]
    fn app_planner_uses_exact_production_shard_and_store_byte_boundaries() {
        const TUNABLE_CAPACITY: usize = MAX_HISTORY_SOURCE_BYTES - 16 * 1024;

        let find_shard_exact = |instance_byte, status| {
            let size = |ascii_bytes| {
                conservative_encoded_profile_bytes_for_test(&byte_boundary_snapshot(
                    instance_byte,
                    ascii_bytes,
                    status,
                ))
                .expect("conservative shard bytes")
                .0
            };
            let base = size(0);
            let fits = usize::try_from(
                (MAX_PROFILE_SHARD_BYTES as u64)
                    .checked_sub(base)
                    .expect("base fixture fits the shard limit"),
            )
            .expect("shard byte delta fits usize");
            let over = fits.saturating_add(1);
            assert!(over <= TUNABLE_CAPACITY);
            assert_eq!(size(fits), MAX_PROFILE_SHARD_BYTES as u64);
            assert_eq!(size(over), MAX_PROFILE_SHARD_BYTES as u64 + 1);
            (fits, over)
        };

        let (terminal_exact_bytes, terminal_plus_one_bytes) =
            find_shard_exact(0x70, WorkspaceHistoryStatus::Succeeded);
        let terminal_exact = byte_boundary_snapshot(
            0x70,
            terminal_exact_bytes,
            WorkspaceHistoryStatus::Succeeded,
        );
        let terminal_plus_one = byte_boundary_snapshot(
            0x70,
            terminal_plus_one_bytes,
            WorkspaceHistoryStatus::Succeeded,
        );
        let exact_plan = super::plan_workspace_snapshot_set(vec![terminal_exact.clone()])
            .expect("exact 32 MiB app planner input");
        assert_eq!(exact_plan.history_evicted(), 0);
        let plus_one_plan = super::plan_workspace_snapshot_set(vec![terminal_plus_one])
            .expect("32 MiB plus-one terminal is normalized");
        assert_eq!(plus_one_plan.history_evicted(), 1);
        assert_eq!(plus_one_plan.history_evictions()[0].history_id(), 1);
        assert_eq!(
            plus_one_plan.history_evictions()[0].instance_id(),
            terminal_exact.instance_id()
        );

        let (unknown_exact_bytes, unknown_plus_one_bytes) =
            find_shard_exact(0x71, WorkspaceHistoryStatus::OutcomeUnknown);
        let unknown_plus_one = byte_boundary_snapshot(
            0x71,
            unknown_plus_one_bytes,
            WorkspaceHistoryStatus::OutcomeUnknown,
        );
        assert!(matches!(
            super::plan_workspace_snapshot_set(vec![unknown_plus_one.clone()]),
            Err(WorkspaceRetentionError::RetentionExhausted(
                WorkspaceRetentionLimit::ProfileShardBytes
            ))
        ));

        let shard_exact_profiles = (0x72_u8..=0x74)
            .map(|instance_byte| {
                byte_boundary_snapshot(
                    instance_byte,
                    terminal_exact_bytes,
                    WorkspaceHistoryStatus::Succeeded,
                )
            })
            .collect::<Vec<_>>();
        let prefix_committed = shard_exact_profiles.iter().fold(0_u64, |total, snapshot| {
            total.saturating_add(
                conservative_encoded_profile_bytes_for_test(snapshot)
                    .expect("terminal prefix bytes")
                    .1,
            )
        });
        let total_size = |ascii_bytes| {
            prefix_committed.saturating_add(
                conservative_encoded_profile_bytes_for_test(&byte_boundary_snapshot(
                    0x75,
                    ascii_bytes,
                    WorkspaceHistoryStatus::Succeeded,
                ))
                .expect("terminal total bytes")
                .1,
            )
        };
        let total_base = total_size(0);
        let total_fits = usize::try_from(
            MAX_WORKSPACE_STORE_BYTES
                .checked_sub(total_base)
                .expect("base fixture fits the total-store limit"),
        )
        .expect("total-store byte delta fits usize");
        let total_over = total_fits.saturating_add(1);
        assert!(total_over <= TUNABLE_CAPACITY);
        assert_eq!(total_size(total_fits), MAX_WORKSPACE_STORE_BYTES);
        assert_eq!(total_size(total_over), MAX_WORKSPACE_STORE_BYTES + 1);
        let mut total_exact = shard_exact_profiles.clone();
        total_exact.push(byte_boundary_snapshot(
            0x75,
            total_fits,
            WorkspaceHistoryStatus::Succeeded,
        ));
        assert_eq!(
            super::plan_workspace_snapshot_set(total_exact)
                .expect("exact 128 MiB app planner input")
                .history_evicted(),
            0
        );
        let mut total_plus_one = shard_exact_profiles;
        total_plus_one.push(byte_boundary_snapshot(
            0x75,
            total_over,
            WorkspaceHistoryStatus::Succeeded,
        ));
        let total_plan = super::plan_workspace_snapshot_set(total_plus_one)
            .expect("128 MiB plus-one terminal is normalized");
        assert_eq!(total_plan.history_evicted(), 1);
        assert_eq!(
            total_plan.history_evictions()[0].instance_id(),
            ProfileInstanceId::from_bytes([0x72; 16])
        );
        assert_eq!(total_plan.history_evictions()[0].history_id(), 1);

        let unknown_shard_exact_profiles = (0x76_u8..=0x78)
            .map(|instance_byte| {
                byte_boundary_snapshot(
                    instance_byte,
                    unknown_exact_bytes,
                    WorkspaceHistoryStatus::OutcomeUnknown,
                )
            })
            .collect::<Vec<_>>();
        let unknown_prefix = unknown_shard_exact_profiles
            .iter()
            .fold(0_u64, |total, snapshot| {
                total.saturating_add(
                    conservative_encoded_profile_bytes_for_test(snapshot)
                        .expect("unknown prefix bytes")
                        .1,
                )
            });
        let unknown_total_size = |ascii_bytes| {
            unknown_prefix.saturating_add(
                conservative_encoded_profile_bytes_for_test(&byte_boundary_snapshot(
                    0x79,
                    ascii_bytes,
                    WorkspaceHistoryStatus::OutcomeUnknown,
                ))
                .expect("unknown total bytes")
                .1,
            )
        };
        let unknown_total_base = unknown_total_size(0);
        let unknown_total_fits = usize::try_from(
            MAX_WORKSPACE_STORE_BYTES
                .checked_sub(unknown_total_base)
                .expect("unknown base fixture fits the total-store limit"),
        )
        .expect("unknown total-store byte delta fits usize");
        let unknown_total_over = unknown_total_fits.saturating_add(1);
        assert!(unknown_total_over <= TUNABLE_CAPACITY);
        assert_eq!(
            unknown_total_size(unknown_total_fits),
            MAX_WORKSPACE_STORE_BYTES
        );
        assert_eq!(
            unknown_total_size(unknown_total_over),
            MAX_WORKSPACE_STORE_BYTES + 1
        );
        let mut unknown_total_plus_one = unknown_shard_exact_profiles;
        unknown_total_plus_one.push(byte_boundary_snapshot(
            0x79,
            unknown_total_over,
            WorkspaceHistoryStatus::OutcomeUnknown,
        ));
        assert!(matches!(
            super::plan_workspace_snapshot_set(unknown_total_plus_one),
            Err(WorkspaceRetentionError::RetentionExhausted(
                WorkspaceRetentionLimit::TotalStoreBytes
            ))
        ));

        let (ui_port, mut service) = bounded_ports(4);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let mut profile = j2_classified_environment_profile();
        profile.id = unknown_plus_one.profile_id().clone();
        profile.name = profile.id.0.clone();
        profile.persisted.id = profile.id.0.clone();
        profile.persisted.name = profile.name.clone();
        profile.persisted.safety = ProfileSafetyPosture::classified(
            ProfileEnvironment::Development,
            ProfileAccess::ReadWrite,
            unknown_plus_one.instance_id(),
        );
        let key = install_j2_profile(&mut app, &profile);
        app.model.workspaces.insert(
            key.clone(),
            ProfileWorkspace::from_persistence_snapshot(unknown_plus_one)
                .expect("unknown plus-one live workspace"),
        );
        assert!(app.ensure_workspace_persistence_binding(&key, &profile));
        app.model
            .workspace_mut(key.clone())
            .create_editor_tab(
                QueryLanguage::Sql,
                "Protected bytes",
                "SELECT protected_bytes",
            )
            .expect("protected-byte editor");
        if let Some(state) = app.workspace_persistence.get_mut(&key) {
            state.load = WorkspaceLoadPhase::Ready;
            state.mode = Some(WorkspaceStoreMode::ReadWrite);
        }
        let revision_before = app
            .model
            .workspace(&key)
            .map_or(u64::MAX, ProfileWorkspace::revision);
        let history_before = app
            .model
            .workspace(&key)
            .and_then(ProfileWorkspace::persistence)
            .expect("protected-byte history")
            .history()
            .to_vec();
        let intent = build_execute_intent(
            &profile,
            app.model.workspace(&key).expect("protected-byte workspace"),
            EditorCursor::caret(0),
        )
        .expect("protected-byte execute intent");
        assert!(!app.submit_editor_execute(intent));
        assert!(service.try_next_command().is_none());
        assert_eq!(
            app.model.workspace(&key).map(ProfileWorkspace::revision),
            Some(revision_before)
        );
        assert_eq!(
            app.model
                .workspace(&key)
                .and_then(ProfileWorkspace::persistence)
                .expect("unchanged protected-byte history")
                .history(),
            history_before
        );
        assert!(app.model.status.contains("outcome-unknown"));
    }

    #[test]
    fn private_source_boundary_is_retained_then_omitted_and_debug_stays_redacted() {
        for (case, source_bytes, source_omitted) in [
            ("exact", MAX_HISTORY_SOURCE_BYTES, false),
            ("plus-one", MAX_HISTORY_SOURCE_BYTES.saturating_add(1), true),
        ] {
            let (ui_port, mut service) = bounded_ports(8);
            let mut app = DbotterApp::new(ui_port);
            assert!(service.try_next_command().is_some());
            let profile = j2_classified_environment_profile();
            let key = install_j2_profile(&mut app, &profile);
            assert!(app.ensure_workspace_persistence_binding(&key, &profile));
            let marker = format!("j2-private-{case}-marker");
            let mut source = marker.clone();
            source.push_str(&"x".repeat(source_bytes.saturating_sub(source.len())));
            assert_eq!(source.len(), source_bytes);
            app.model
                .workspace_mut(key.clone())
                .create_editor_tab(QueryLanguage::Sql, "Private boundary", source.clone())
                .expect("private boundary editor");
            if let Some(state) = app.workspace_persistence.get_mut(&key) {
                state.load = WorkspaceLoadPhase::Ready;
                state.mode = Some(WorkspaceStoreMode::ReadWrite);
            }
            let intent = build_execute_intent(
                &profile,
                app.model
                    .workspace(&key)
                    .expect("private boundary workspace"),
                EditorCursor::caret(0),
            )
            .expect("private boundary execute intent");
            assert!(app.submit_editor_execute(intent));
            let commit = service
                .try_next_command()
                .expect("private boundary provisional commit");
            let snapshot_debug = match &commit {
                UiCommand::CommitWorkspace { snapshot, .. } => {
                    assert_eq!(snapshot.history().len(), 1);
                    assert_eq!(snapshot.history()[0].source_omitted(), source_omitted);
                    if source_omitted {
                        assert!(snapshot.history()[0].source().is_none());
                    } else {
                        assert_eq!(snapshot.history()[0].source(), Some(source.as_str()));
                    }
                    format!("{snapshot:?}")
                }
                command => panic!("expected private boundary commit, got {command:?}"),
            };
            assert!(!snapshot_debug.contains(&marker));
            assert!(!format!("{:?}", app.pending_workspace_history).contains(&marker));
            assert!(!format!("{:?}", app.retention_commit_barrier).contains(&marker));
            assert!(!app.model.status.contains(&marker));
            let execute = service
                .try_next_command()
                .expect("private boundary execute");
            match &execute {
                UiCommand::Execute { text, .. } => assert_eq!(text, &source),
                command => panic!("expected private boundary execute, got {command:?}"),
            }
            assert!(!format!("{execute:?}").contains(&marker));
            assert!(service.try_next_command().is_none());
        }
    }

    #[test]
    fn retag_invalidates_active_barrier_and_clear_cannot_bypass_recovery() {
        let (ui_port, mut service) = bounded_ports(8);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let profile = j2_classified_environment_profile();
        let old_key = install_j2_profile(&mut app, &profile);
        assert!(app.ensure_workspace_persistence_binding(&old_key, &profile));
        app.model
            .workspace_mut(old_key.clone())
            .create_editor_tab(QueryLanguage::Sql, "Retag", "SELECT retag_barrier")
            .expect("retag editor");
        if let Some(state) = app.workspace_persistence.get_mut(&old_key) {
            state.load = WorkspaceLoadPhase::Ready;
            state.mode = Some(WorkspaceStoreMode::ReadWrite);
        }
        let intent = build_execute_intent(
            &profile,
            app.model.workspace(&old_key).expect("retag workspace"),
            EditorCursor::caret(0),
        )
        .expect("retag execute");
        assert!(app.submit_editor_execute(intent));
        let (old_commit, old_identity, old_revision, committed_snapshot) =
            match service.try_next_command() {
                Some(UiCommand::CommitWorkspace {
                    operation_id,
                    identity,
                    revision,
                    snapshot,
                }) => (operation_id, identity, revision, snapshot),
                command => panic!("expected old-key provisional commit, got {command:?}"),
            };
        assert!(matches!(
            service.try_next_command(),
            Some(UiCommand::Execute { .. })
        ));
        let history_before = app
            .model
            .workspace(&old_key)
            .and_then(ProfileWorkspace::persistence)
            .expect("old-key history")
            .history()
            .to_vec();
        app.clear_workspace_history(&old_key);
        app.set_workspace_persistence_enabled(&old_key, false);
        assert_eq!(
            app.model
                .workspace(&old_key)
                .and_then(ProfileWorkspace::persistence)
                .expect("blocked old-key history")
                .history(),
            history_before
        );
        assert!(
            app.model
                .workspace(&old_key)
                .and_then(ProfileWorkspace::persistence)
                .is_some_and(|persistence| persistence.persistence_enabled())
        );

        let mut refreshed = profile.clone();
        refreshed.generation = ProfileGeneration(profile.generation.0.saturating_add(1));
        let new_key = WorkspaceKey::new(refreshed.id.clone(), refreshed.generation);
        assert!(service.try_emit(UiEvent::ProfilesLoaded {
            operation_id: OperationId(100),
            profiles: vec![refreshed.clone()],
            config: ConfigPresentation::for_source(
                ConfigSourceVersion::V3,
                &PathBuf::from("/private/tmp/dbotter-j2-retag-barrier.toml"),
            ),
        }));
        app.poll_events();
        assert!(!app.workspace_persistence.contains_key(&old_key));
        assert!(app.workspace_persistence.contains_key(&new_key));
        assert!(app.retention_commit_barrier.active.is_none());
        assert!(matches!(
            app.retention_commit_barrier.failure,
            Some(super::RetentionBarrierFailure::IdentityChanged)
        ));
        assert!(app.retention_commit_barrier.queue.iter().all(|request| {
            request.workspace_key == new_key
                && request.identity.profile_generation() == refreshed.generation
        }));
        let (refresh_operation, refresh_identity, refresh_revision) =
            match service.try_next_command() {
                Some(UiCommand::LoadWorkspace {
                    operation_id,
                    identity,
                    base_revision,
                }) => (operation_id, identity, base_revision),
                command => {
                    panic!("retagged in-flight barrier must refresh durable bytes, got {command:?}")
                }
            };
        assert_eq!(refresh_identity.profile_generation(), refreshed.generation);
        assert!(service.try_next_command().is_none());

        assert!(service.try_emit(UiEvent::WorkspaceCommitted {
            operation_id: old_commit,
            identity: old_identity,
            revision: old_revision,
            generation: 1,
            committed_bytes: submitted_committed_bytes_or_stale_sentinel(&app, old_commit, 1,),
            warnings: Vec::new(),
        }));
        app.poll_events();
        let committed_bytes = encoded_profile_bytes_at_generation(&committed_snapshot, 1)
            .expect("retagged barrier bytes")
            .1;
        assert!(service.try_emit(UiEvent::WorkspaceLoaded {
            operation_id: refresh_operation,
            identity: refresh_identity,
            base_revision: refresh_revision,
            mode: WorkspaceStoreMode::ReadWrite,
            read_only_reason: None,
            generation: Some(1),
            committed_bytes,
            snapshot: Some(committed_snapshot),
        }));
        app.poll_events();
        assert!(matches!(
            app.retention_commit_barrier.failure,
            Some(super::RetentionBarrierFailure::IdentityChanged)
        ));
        let retagged_history = app
            .model
            .workspace(&new_key)
            .and_then(ProfileWorkspace::persistence)
            .expect("retagged history")
            .history()
            .to_vec();
        app.clear_workspace_history(&new_key);
        assert_eq!(
            app.model
                .workspace(&new_key)
                .and_then(ProfileWorkspace::persistence)
                .expect("blocked retagged history")
                .history(),
            retagged_history
        );

        assert!(app.retry_retention_commit_barrier());
        let (retry_operation, retry_identity, retry_revision) = match service.try_next_command() {
            Some(UiCommand::CommitWorkspace {
                operation_id,
                identity,
                revision,
                ..
            }) => (operation_id, identity, revision),
            command => panic!("retag retry must use the new identity, got {command:?}"),
        };
        assert_eq!(retry_identity.profile_generation(), refreshed.generation);
        assert!(service.try_emit(UiEvent::WorkspaceCommitted {
            operation_id: retry_operation,
            identity: retry_identity,
            revision: retry_revision,
            generation: 2,
            committed_bytes: submitted_committed_bytes_or_stale_sentinel(&app, retry_operation, 2,),
            warnings: Vec::new(),
        }));
        app.poll_events();
        assert!(!app.retention_commit_barrier.is_pending_or_failed());
    }

    #[test]
    fn identical_profile_refresh_preserves_the_exact_active_retention_barrier() {
        let (ui_port, mut service) = bounded_ports(8);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let profile = j2_classified_environment_profile();
        let key = install_j2_profile(&mut app, &profile);
        assert!(app.ensure_workspace_persistence_binding(&key, &profile));
        app.model
            .workspace_mut(key.clone())
            .create_editor_tab(QueryLanguage::Sql, "Refresh", "SELECT identical_refresh")
            .expect("identical-refresh editor");
        if let Some(state) = app.workspace_persistence.get_mut(&key) {
            state.load = WorkspaceLoadPhase::Ready;
            state.mode = Some(WorkspaceStoreMode::ReadWrite);
        }
        let intent = build_execute_intent(
            &profile,
            app.model
                .workspace(&key)
                .expect("identical-refresh workspace"),
            EditorCursor::caret(0),
        )
        .expect("identical-refresh execute");
        assert!(app.submit_editor_execute(intent));
        let (commit_operation, identity, revision) = match service.try_next_command() {
            Some(UiCommand::CommitWorkspace {
                operation_id,
                identity,
                revision,
                ..
            }) => (operation_id, identity, revision),
            command => panic!("expected active retention commit, got {command:?}"),
        };
        assert!(matches!(
            service.try_next_command(),
            Some(UiCommand::Execute { .. })
        ));
        let active_before = app
            .retention_commit_barrier
            .active
            .clone()
            .expect("active barrier before refresh");

        assert!(service.try_emit(UiEvent::ProfilesLoaded {
            operation_id: OperationId(100),
            profiles: vec![profile],
            config: ConfigPresentation::for_source(
                ConfigSourceVersion::V3,
                &PathBuf::from("/private/tmp/dbotter-j2-identical-refresh.toml"),
            ),
        }));
        app.poll_events();
        assert_eq!(
            app.retention_commit_barrier.active.as_ref(),
            Some(&active_before)
        );
        assert!(app.retention_commit_barrier.failure.is_none());
        assert!(app.retention_barrier_references_current_identities());
        assert!(service.try_next_command().is_none());

        assert!(
            service.try_emit(UiEvent::WorkspaceCommitted {
                operation_id: commit_operation,
                identity,
                revision,
                generation: 1,
                committed_bytes: submitted_committed_bytes_or_stale_sentinel(
                    &app,
                    commit_operation,
                    1,
                ),
                warnings: Vec::new(),
            })
        );
        app.poll_events();
        assert!(!app.retention_commit_barrier.is_pending_or_failed());
    }

    #[test]
    fn deleted_profile_invalidates_the_active_retention_identity_and_stale_ack() {
        let (ui_port, mut service) = bounded_ports(8);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let profile = j2_classified_environment_profile();
        let key = install_j2_profile(&mut app, &profile);
        assert!(app.ensure_workspace_persistence_binding(&key, &profile));
        app.model
            .workspace_mut(key.clone())
            .create_editor_tab(QueryLanguage::Sql, "Delete", "SELECT deleted_barrier")
            .expect("deleted-barrier editor");
        if let Some(state) = app.workspace_persistence.get_mut(&key) {
            state.load = WorkspaceLoadPhase::Ready;
            state.mode = Some(WorkspaceStoreMode::ReadWrite);
        }
        let intent = build_execute_intent(
            &profile,
            app.model
                .workspace(&key)
                .expect("deleted-barrier workspace"),
            EditorCursor::caret(0),
        )
        .expect("deleted-barrier execute");
        assert!(app.submit_editor_execute(intent));
        let (old_operation, old_identity, old_revision) = match service.try_next_command() {
            Some(UiCommand::CommitWorkspace {
                operation_id,
                identity,
                revision,
                ..
            }) => (operation_id, identity, revision),
            command => panic!("expected deleted-profile commit, got {command:?}"),
        };
        assert!(matches!(
            service.try_next_command(),
            Some(UiCommand::Execute { .. })
        ));
        assert!(service.try_emit(UiEvent::ProfilesLoaded {
            operation_id: OperationId(100),
            profiles: Vec::new(),
            config: ConfigPresentation::for_source(
                ConfigSourceVersion::V3,
                &PathBuf::from("/private/tmp/dbotter-j2-delete-barrier.toml"),
            ),
        }));
        app.poll_events();
        assert!(app.workspace_persistence.is_empty());
        assert!(app.retention_commit_barrier.active.is_none());
        assert!(app.retention_commit_barrier.queue.is_empty());
        assert!(matches!(
            app.retention_commit_barrier.failure,
            Some(super::RetentionBarrierFailure::IdentityChanged)
        ));
        assert!(service.try_emit(UiEvent::WorkspaceCommitted {
            operation_id: old_operation,
            identity: old_identity,
            revision: old_revision,
            generation: 1,
            committed_bytes: submitted_committed_bytes_or_stale_sentinel(&app, old_operation, 1,),
            warnings: Vec::new(),
        }));
        app.poll_events();
        assert!(matches!(
            app.retention_commit_barrier.failure,
            Some(super::RetentionBarrierFailure::IdentityChanged)
        ));
    }

    #[test]
    fn confirmed_delete_waits_for_retention_settle_and_keeps_confirmation() {
        let (ui_port, mut service) = bounded_ports(8);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let profile = j2_classified_environment_profile();
        let key = install_j2_profile(&mut app, &profile);
        assert!(app.ensure_workspace_persistence_binding(&key, &profile));
        app.model
            .workspace_mut(key.clone())
            .create_editor_tab(QueryLanguage::Sql, "Delete gate", "SELECT delete_gate")
            .expect("delete-gate editor");
        if let Some(state) = app.workspace_persistence.get_mut(&key) {
            state.load = WorkspaceLoadPhase::Ready;
            state.mode = Some(WorkspaceStoreMode::ReadWrite);
        }
        let intent = build_execute_intent(
            &profile,
            app.model.workspace(&key).expect("delete-gate workspace"),
            EditorCursor::caret(0),
        )
        .expect("delete-gate execute");
        assert!(app.submit_editor_execute(intent));
        let (commit_operation, identity, revision) = match service.try_next_command() {
            Some(UiCommand::CommitWorkspace {
                operation_id,
                identity,
                revision,
                ..
            }) => (operation_id, identity, revision),
            command => panic!("expected delete-gate retention commit, got {command:?}"),
        };
        let execute_operation = match service.try_next_command() {
            Some(UiCommand::Execute { operation_id, .. }) => operation_id,
            command => panic!("expected delete-gate execute, got {command:?}"),
        };

        app.open_delete_confirmation(&profile);
        app.confirm_delete_confirmation();
        assert!(app.delete_confirmation.is_some());
        assert!(app.model.status.contains("save barrier"));
        assert!(service.try_next_command().is_none());

        assert!(
            service.try_emit(UiEvent::WorkspaceCommitted {
                operation_id: commit_operation,
                identity,
                revision,
                generation: 1,
                committed_bytes: submitted_committed_bytes_or_stale_sentinel(
                    &app,
                    commit_operation,
                    1,
                ),
                warnings: Vec::new(),
            })
        );
        app.poll_events();
        assert!(!app.retention_commit_barrier.is_pending_or_failed());
        app.confirm_delete_confirmation();
        assert!(app.delete_confirmation.is_some());
        assert!(app.model.status.contains("execution history"));
        assert!(service.try_next_command().is_none());

        assert!(service.try_emit(UiEvent::ExecuteUnavailable {
            operation_id: execute_operation,
            profile_id: profile.id.clone(),
            profile_generation: profile.generation,
            summary: PublicSummary::InvalidInput,
        }));
        app.poll_events();
        let (terminal_operation, terminal_identity, terminal_revision) =
            match service.try_next_command() {
                Some(UiCommand::CommitWorkspace {
                    operation_id,
                    identity,
                    revision,
                    ..
                }) => (operation_id, identity, revision),
                command => panic!("expected delete-gate terminal commit, got {command:?}"),
            };
        assert!(service.try_emit(UiEvent::WorkspaceCommitted {
            operation_id: terminal_operation,
            identity: terminal_identity,
            revision: terminal_revision,
            generation: 2,
            committed_bytes: submitted_committed_bytes_or_stale_sentinel(
                &app,
                terminal_operation,
                2,
            ),
            warnings: Vec::new(),
        }));
        app.poll_events();
        assert!(!app.retention_commit_barrier.is_pending_or_failed());
        assert!(app.pending_workspace_history.is_empty());
        app.confirm_delete_confirmation();
        assert!(app.delete_confirmation.is_none());
        assert!(matches!(
            service.try_next_command(),
            Some(UiCommand::DeleteProfile(_))
        ));
        assert!(service.try_next_command().is_none());
    }

    #[test]
    fn dirty_editor_close_opens_an_accessible_discard_guard_without_losing_text() {
        let (ui_port, mut service) = bounded_ports(4);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let mysql = profile(DriverKind::MySql, DriverAvailability::Ready);
        let key = WorkspaceKey::new(mysql.id.clone(), mysql.generation);
        app.model.profiles = vec![mysql.clone()];
        app.model.selected_profile = Some(mysql.id.clone());
        app.model
            .active_generations
            .insert(mysql.id.clone(), mysql.generation);
        let tab = app
            .model
            .workspace_mut(key.clone())
            .create_editor_tab(QueryLanguage::Sql, "Unsaved", "SELECT keep_me")
            .expect("dirty tab");

        app.request_editor_tab_close(key.clone(), tab, "editor.tab.discard");

        assert!(app.editor_discard_confirmation.is_some());
        assert_eq!(
            app.model
                .workspace(&key)
                .and_then(|workspace| workspace.editor_tab(tab))
                .map(|tab| tab.text()),
            Some("SELECT keep_me")
        );
        let ids = shell_author_ids(&mut app, 1440.0, 900.0);
        assert!(ids.contains("editor.tab.discard"));
        assert!(ids.contains("editor.tab.discard.cancel"));
        assert!(
            !format!("{:?}", app.editor_discard_confirmation).contains("SELECT keep_me"),
            "discard guard Debug must not disclose query text"
        );
    }

    #[test]
    fn actual_editor_tabs_close_in_place_without_selecting_or_losing_a_dirty_draft() {
        let (ui_port, mut service) = bounded_ports(4);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let profile = profile(DriverKind::MySql, DriverAvailability::Ready);
        let key = WorkspaceKey::new(profile.id.clone(), profile.generation);
        app.model.profiles = vec![profile.clone()];
        app.model.selected_profile = Some(profile.id.clone());
        app.model
            .active_generations
            .insert(profile.id.clone(), profile.generation);
        let dirty = app
            .model
            .workspace_mut(key.clone())
            .create_editor_tab(QueryLanguage::Sql, "Unsaved", "SELECT keep_me")
            .expect("dirty tab");
        let selected = app
            .model
            .workspace_mut(key.clone())
            .create_editor_tab(QueryLanguage::Sql, "Current", "")
            .expect("selected clean tab");

        let context = Context::default();
        context.enable_accesskit();
        let initial = context.run_ui(RawInput::default(), |ui| {
            app.show_editor_tab_strip(ui, &profile, &key, true);
        });
        let initial = initial
            .platform_output
            .accesskit_update
            .expect("editor tab strip must emit AccessKit");
        let (close_id, close) =
            accesskit_author_node(&initial, &format!("editor.tab.close.{}", dirty.0));
        assert_eq!(close.role(), accesskit::Role::Button);
        assert_eq!(close.label(), Some("Close editor tab"));
        assert!(close.supports_action(accesskit::Action::Focus));
        assert!(close.supports_action(accesskit::Action::Click));

        let _ = context.run_ui(
            RawInput {
                events: vec![Event::AccessKitActionRequest(accesskit::ActionRequest {
                    action: accesskit::Action::Focus,
                    target_tree: accesskit::TreeId::ROOT,
                    target_node: close_id,
                    data: None,
                })],
                ..RawInput::default()
            },
            |ui| app.show_editor_tab_strip(ui, &profile, &key, true),
        );
        let _ = context.run_ui(
            RawInput {
                events: vec![Event::Key {
                    key: Key::Enter,
                    physical_key: Some(Key::Enter),
                    pressed: true,
                    repeat: false,
                    modifiers: Modifiers::NONE,
                }],
                ..RawInput::default()
            },
            |ui| app.show_editor_tab_strip(ui, &profile, &key, true),
        );

        let confirmation = app
            .editor_discard_confirmation
            .as_ref()
            .expect("dirty tab close must open its discard guard");
        assert_eq!(confirmation.workspace_key, key);
        assert_eq!(confirmation.tab_id, dirty);
        let workspace = app.model.workspace(&key).expect("workspace retained");
        assert_eq!(workspace.selected_editor_tab_id(), Some(selected));
        assert_eq!(
            workspace.editor_tab(dirty).map(|tab| tab.text()),
            Some("SELECT keep_me")
        );
        assert!(service.try_next_command().is_none());
    }

    fn redis_page(request: &RedisScanRequest, raw_key: &[u8]) -> RedisKeyPage {
        RedisKeyPage {
            identity: request.identity.clone(),
            next_cursor: 0,
            keys: vec![RedisKeyEntry::new(RedisKeyId(raw_key.to_vec()))],
            retained_count: 1,
            skipped_oversize: 0,
            retained_bytes: raw_key.len(),
            consistency: RedisScanConsistency::Weak,
            truncated: false,
            stale: false,
        }
    }

    fn result_snapshot(profile: &ProfileSnapshot, value: &str) -> ResultSnapshot {
        ResultSnapshot::retain(
            QueryResult {
                columns: vec![Column {
                    name: "value".to_owned(),
                    type_name: "TEXT".to_owned(),
                }],
                rows: vec![vec![Cell::Text(value.to_owned())]],
                affected_rows: 0,
                last_insert_id: None,
                elapsed_ms: 4,
                truncated: false,
                backend_notices_present: false,
            },
            ResultProvenance {
                result_id: ResultId(71),
                profile_id: profile.id.clone(),
                profile_generation: profile.generation,
                operation_id: OperationId(72),
                driver: profile.driver,
                completed_at_unix_ms: 0,
                duration_ms: 4,
            },
            ResultRetentionPolicy::mysql(1),
        )
    }

    fn result_snapshot_for_operation(
        profile: &ProfileSnapshot,
        value: &str,
        operation_id: OperationId,
        result_id: ResultId,
    ) -> ResultSnapshot {
        let mut snapshot = result_snapshot(profile, value);
        snapshot.provenance.operation_id = operation_id;
        snapshot.provenance.result_id = result_id;
        snapshot
    }

    fn redis_keys_for(app: &DbotterApp, key: &WorkspaceKey) -> Option<Vec<Vec<u8>>> {
        app.redis_explorers
            .get(key)
            .map(|explorer| explorer.test_retained_raw_keys())
    }

    fn load_redis_key(
        app: &mut DbotterApp,
        service: &mut ServicePort,
        key: &WorkspaceKey,
        raw_key: &[u8],
        session_generation: SessionGeneration,
    ) {
        app.model.connection_states.insert(
            key.profile_id.clone(),
            ConnectionState::Connected {
                session_generation,
                elapsed_ms: 0,
            },
        );
        app.model.selected_profile = Some(key.profile_id.clone());
        render_redis_explorer(app);
        app.submit_redis_intent(RedisExplorerIntent::Scan {
            filter: RedisKeyFilter::LiteralPrefix(String::new()),
            cursor: 0,
            restart: true,
        });
        let request = match service.try_next_command() {
            Some(UiCommand::ScanRedisKeys(request)) => request,
            _ => panic!("exact Redis scan command"),
        };
        assert_eq!(request.profile_id(), &key.profile_id);
        assert_eq!(request.profile_generation(), key.profile_generation);
        assert!(service.try_emit(crate::ui::UiEvent::RedisKeysLoaded {
            page: redis_page(&request, raw_key),
            session_generation,
            session_disposition: SessionDisposition::Keep,
        }));
        app.poll_events();
    }

    fn seed_two_redis_workspaces(
        app: &mut DbotterApp,
        service: &mut ServicePort,
    ) -> (ProfileSnapshot, ProfileSnapshot, WorkspaceKey, WorkspaceKey) {
        let alpha = redis_profile("redis-alpha", 1);
        let beta = redis_profile("redis-beta", 1);
        app.model.profiles = vec![alpha.clone(), beta.clone()];
        app.model
            .active_generations
            .insert(alpha.id.clone(), alpha.generation);
        app.model
            .active_generations
            .insert(beta.id.clone(), beta.generation);
        let alpha_key = WorkspaceKey::new(alpha.id.clone(), alpha.generation);
        let beta_key = WorkspaceKey::new(beta.id.clone(), beta.generation);
        load_redis_key(
            app,
            service,
            &alpha_key,
            b"alpha:key",
            SessionGeneration(11),
        );
        load_redis_key(app, service, &beta_key, b"beta:key", SessionGeneration(21));
        load_redis_key(
            app,
            service,
            &alpha_key,
            b"alpha:key",
            SessionGeneration(11),
        );
        (alpha, beta, alpha_key, beta_key)
    }

    fn prime_save_and_connect(
        app: &mut DbotterApp,
        service: &mut crate::ui::adapter::ServicePort,
    ) -> (ProfileId, OperationId) {
        let mut editor = ProfileEditor::new(DraftId(401), DriverKind::MySql);
        editor.draft.name = "Profile".to_owned();
        let save_operation = app.model.next_operation();
        assert!(matches!(
            editor.try_save_with_connect(&app.port, save_operation, true),
            crate::ui::profile_form::SaveAttempt::Submitted(operation)
                if operation == save_operation
        ));
        app.profile_editor = Some(editor);
        assert!(matches!(
            service.try_next_command(),
            Some(UiCommand::CreateProfile(request)) if request.operation_id == save_operation
        ));
        let profile_id = ProfileId("profile".to_owned());
        assert!(service.try_emit(crate::ui::UiEvent::ProfileSaved {
            operation_id: save_operation,
            profile_id: profile_id.clone(),
            previous_generation: None,
            profile_generation: ProfileGeneration(1),
            session_retained: false,
            warning: None,
        }));
        app.poll_events();
        let Some(UiCommand::RefreshProfiles { operation_id }) = service.try_next_command() else {
            panic!("Save & Connect must submit an exact follow-up refresh");
        };
        (profile_id, operation_id)
    }

    fn reload_failure(operation_id: OperationId) -> PublicOperationError {
        PublicOperationError::new_or_internal(
            OperationKind::ReloadConfiguration,
            PublicSummary::NetworkUnavailable,
            PublicCode::None,
            &SafeContext::global(operation_id),
        )
    }

    #[test]
    fn actual_app_profile_editor_exposes_frozen_ids_and_config_cancel_escape() {
        let (ui_port, mut service) = bounded_ports(4);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let mut editor = ProfileEditor::new(DraftId(400), DriverKind::Redis);
        editor.draft.name = "Redis Profile".to_owned();
        editor.draft.select_tls(TlsMode::Required);
        editor.select_credential_mode(CredentialMode::Session);
        editor.set_config_uncertain(true);
        app.profile_editor = Some(editor);

        let context = Context::default();
        context.enable_accesskit();
        let output = context.run_ui(RawInput::default(), |ui| app.editor_and_results(ui));
        let update = output
            .platform_output
            .accesskit_update
            .expect("actual app profile frame must emit AccessKit");
        for expected in [
            "profile.connection_id",
            "profile.host",
            "profile.redis_tls.ca_file",
            "profile.redis_tls.ca_file.pick",
            "profile.credential.session.keep",
            "profile.credential.session.replace",
            "profile.credential.session.forget",
        ] {
            assert!(
                update
                    .nodes
                    .iter()
                    .any(|(_, node)| node.author_id() == Some(expected)),
                "missing actual app AX id {expected}"
            );
        }
        let cancel = update
            .nodes
            .iter()
            .find_map(|(_, node)| (node.author_id() == Some("profile.cancel")).then_some(node))
            .expect("actual app Cancel node");
        assert!(
            !cancel.is_disabled(),
            "Config uncertain must not trap the form"
        );
    }

    #[test]
    fn actual_app_first_run_exposes_one_primary_action_and_driver_choices() {
        let (ui_port, mut service) = bounded_ports(4);
        let mut app = DbotterApp::new(ui_port);
        let operation_id = match service.try_next_command() {
            Some(UiCommand::RefreshProfiles { operation_id }) => operation_id,
            _ => panic!("startup must request the exact profile list"),
        };
        assert!(service.try_emit(crate::ui::UiEvent::ProfilesLoaded {
            operation_id,
            profiles: Vec::new(),
            config: Default::default(),
        }));
        app.poll_events();

        let context = Context::default();
        context.enable_accesskit();
        let output = context.run_ui(RawInput::default(), |ui| app.show_native(ui));
        let update = output
            .platform_output
            .accesskit_update
            .expect("actual first-run app frame must emit AccessKit");
        let node = |author_id: &str| {
            update
                .nodes
                .iter()
                .find_map(|(_, node)| (node.author_id() == Some(author_id)).then_some(node))
                .unwrap_or_else(|| panic!("missing actual first-run AX id {author_id}"))
        };

        assert_eq!(node("connection.new").role(), accesskit::Role::Button);
        assert_eq!(
            node("connection.new.mysql").role(),
            accesskit::Role::RadioButton
        );
        assert_eq!(
            node("connection.new.redis").role(),
            accesskit::Role::RadioButton
        );
        let mongodb = node("connection.mongodb.planned");
        assert_eq!(mongodb.role(), accesskit::Role::RadioButton);
        assert!(mongodb.is_disabled());
    }

    #[test]
    fn actual_wide_shell_exposes_all_persistent_regions_at_1440_by_900() {
        assert_eq!(super::NativeLayout::columns_for_width(1180.0), 3);
        let (ui_port, mut service) = bounded_ports(4);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let profile = profile(DriverKind::MySql, DriverAvailability::Ready);
        app.model.profiles = vec![profile.clone()];
        app.model.selected_profile = Some(profile.id.clone());
        app.model
            .active_generations
            .insert(profile.id.clone(), profile.generation);
        app.model
            .workspace_mut(WorkspaceKey::new(profile.id.clone(), profile.generation));

        let ids = shell_author_ids(&mut app, 1440.0, 900.0);
        for expected in [
            "navigator",
            "object-editor-tabs",
            "result-history-tabs",
            "status-action-context",
        ] {
            assert!(
                ids.contains(expected),
                "missing actual wide AX id {expected}"
            );
        }
        assert!(!ids.contains("navigator.open"));
        assert!(!ids.contains("inspector.open"));
    }

    #[test]
    fn actual_wide_splitter_is_accessible_keyboard_adjustable_and_resettable() {
        let (ui_port, mut service) = bounded_ports(4);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let profile = profile(DriverKind::MySql, DriverAvailability::Ready);
        let key = WorkspaceKey::new(profile.id.clone(), profile.generation);
        app.model.profiles = vec![profile.clone()];
        app.model.selected_profile = Some(profile.id.clone());
        app.model
            .active_generations
            .insert(profile.id.clone(), profile.generation);
        app.model.workspace_mut(key);

        let context = Context::default();
        context.enable_accesskit();
        let render = |app: &mut DbotterApp, events: Vec<Event>| {
            context.run_ui(
                RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1440.0, 900.0),
                    )),
                    events,
                    ..RawInput::default()
                },
                |ui| app.show_native(ui),
            )
        };

        let _ = render(&mut app, Vec::new());
        let initial = render(&mut app, Vec::new());
        let initial_update = initial
            .platform_output
            .accesskit_update
            .expect("wide splitter frame must emit AccessKit");
        let (splitter_id, splitter) = accesskit_author_node(&initial_update, "workspace.splitter");
        assert_eq!(splitter.role(), accesskit::Role::Splitter);
        assert_eq!(splitter.label(), Some("Resize editor and results"));
        assert_eq!(
            splitter.orientation(),
            Some(accesskit::Orientation::Horizontal)
        );
        assert_eq!(
            splitter.numeric_value_step(),
            Some(f64::from(NativeLayout::SPLITTER_KEYBOARD_STEP))
        );
        assert!(splitter.supports_action(accesskit::Action::Focus));
        let bounds = splitter.bounds().expect("splitter needs native bounds");
        assert!(
            bounds.y1 - bounds.y0 >= f64::from(NativeLayout::SPLITTER_ACCESSIBLE_HIT_EXTENT),
            "splitter hit area must be at least 44 points"
        );
        let initial_editor_extent = splitter
            .numeric_value()
            .expect("splitter needs an editor-extent value");
        let initial_total_extent = splitter
            .max_numeric_value()
            .expect("splitter needs a maximum value")
            + f64::from(NativeLayout::PANE_MIN_EXTENT);
        assert!(
            (initial_editor_extent
                - initial_total_extent * f64::from(NativeLayout::DEFAULT_EDITOR_SHARE))
            .abs()
                < 0.01,
            "the settled native splitter must start at the exact 60/40 geometry"
        );
        for expected in [
            "navigator",
            "object-editor-tabs",
            "result-history-tabs",
            "status-action-context",
            "workspace.split.reset",
        ] {
            assert!(
                initial_update
                    .nodes
                    .iter()
                    .any(|(_, node)| node.author_id() == Some(expected)),
                "splitter frame lost {expected}"
            );
        }

        let focused = render(
            &mut app,
            vec![Event::AccessKitActionRequest(accesskit::ActionRequest {
                action: accesskit::Action::Focus,
                target_tree: accesskit::TreeId::ROOT,
                target_node: splitter_id,
                data: None,
            })],
        );
        let focused_update = focused
            .platform_output
            .accesskit_update
            .expect("focused splitter frame must emit AccessKit");
        assert_eq!(
            focused_update
                .nodes
                .iter()
                .find_map(|(node_id, node)| {
                    (*node_id == focused_update.focus).then(|| node.author_id())
                })
                .flatten(),
            Some("workspace.splitter"),
            "native AX focus must settle on the rendered splitter"
        );
        let adjusted = render(
            &mut app,
            vec![Event::Key {
                key: Key::ArrowDown,
                physical_key: Some(Key::ArrowDown),
                pressed: true,
                repeat: false,
                modifiers: Modifiers::NONE,
            }],
        );
        let adjusted_update = adjusted
            .platform_output
            .accesskit_update
            .expect("adjusted splitter frame must emit AccessKit");
        let (_, adjusted_splitter) = accesskit_author_node(&adjusted_update, "workspace.splitter");
        let adjusted_editor_extent = adjusted_splitter
            .numeric_value()
            .expect("adjusted splitter needs a value");
        let adjusted_total_extent = adjusted_splitter
            .max_numeric_value()
            .expect("adjusted splitter needs a maximum value")
            + f64::from(NativeLayout::PANE_MIN_EXTENT);
        assert!(
            (adjusted_editor_extent
                - adjusted_total_extent * f64::from(NativeLayout::DEFAULT_EDITOR_SHARE)
                - f64::from(NativeLayout::SPLITTER_KEYBOARD_STEP))
            .abs()
                < 0.01,
            "ArrowDown must move the rendered splitter by exactly five points: initial={initial_editor_extent}, adjusted={adjusted_editor_extent}"
        );
        for expected in [
            "navigator",
            "object-editor-tabs",
            "result-history-tabs",
            "status-action-context",
        ] {
            assert!(
                adjusted_update
                    .nodes
                    .iter()
                    .any(|(_, node)| node.author_id() == Some(expected)),
                "splitter adjustment lost {expected}"
            );
        }

        let (reset_id, reset_node) =
            accesskit_author_node(&adjusted_update, "workspace.split.reset");
        assert_eq!(reset_node.role(), accesskit::Role::Button);
        assert_eq!(reset_node.label(), Some("Reset split to 60/40"));
        let _ = render(
            &mut app,
            vec![Event::AccessKitActionRequest(accesskit::ActionRequest {
                action: accesskit::Action::Focus,
                target_tree: accesskit::TreeId::ROOT,
                target_node: reset_id,
                data: None,
            })],
        );
        let _ = render(
            &mut app,
            vec![Event::Key {
                key: Key::Enter,
                physical_key: Some(Key::Enter),
                pressed: true,
                repeat: false,
                modifiers: Modifiers::NONE,
            }],
        );
        let reset = render(&mut app, Vec::new());
        let reset_update = reset
            .platform_output
            .accesskit_update
            .expect("reset splitter frame must emit AccessKit");
        let (_, reset_splitter) = accesskit_author_node(&reset_update, "workspace.splitter");
        let reset_total_extent = reset_splitter
            .max_numeric_value()
            .expect("reset splitter needs a maximum value")
            + f64::from(NativeLayout::PANE_MIN_EXTENT);
        assert!(
            (reset_splitter
                .numeric_value()
                .expect("reset splitter needs a value")
                - reset_total_extent * f64::from(NativeLayout::DEFAULT_EDITOR_SHARE))
            .abs()
                < 0.01,
            "Reset split must restore the rendered 60/40 geometry"
        );
        assert_eq!(
            app.workspace_geometries
                .get(&WorkspaceKey::new(profile.id, profile.generation))
                .map(|geometry| geometry.editor_share()),
            Some(NativeLayout::DEFAULT_EDITOR_SHARE),
            "Reset split must store the exact 60/40 geometry, independent of its initial value"
        );
    }

    #[test]
    fn actual_splitter_crossing_collapses_and_named_restore_returns_both_panes() {
        let (ui_port, mut service) = bounded_ports(4);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let profile = j2_classified_environment_profile();
        let key = install_j2_profile(&mut app, &profile);
        assert!(app.ensure_workspace_persistence_binding(&key, &profile));
        {
            let state = app
                .workspace_persistence
                .get_mut(&key)
                .expect("workspace persistence state");
            state.load = WorkspaceLoadPhase::Ready;
            state.mode = Some(WorkspaceStoreMode::ReadWrite);
        }
        app.workspace_geometries
            .insert(key.clone(), WorkspaceGeometry::restore(280.0, 0.30, true));

        let context = Context::default();
        context.enable_accesskit();
        let render = |app: &mut DbotterApp, events: Vec<Event>| {
            let geometry = app
                .workspace_geometries
                .get(&key)
                .copied()
                .expect("workspace geometry");
            context.run_ui(
                RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(800.0, 600.0),
                    )),
                    events,
                    ..RawInput::default()
                },
                |ui| app.show_editor_result_shell(ui, geometry),
            )
        };

        let initial = render(&mut app, Vec::new());
        let initial_update = initial
            .platform_output
            .accesskit_update
            .expect("initial split frame must emit AccessKit");
        let (splitter_id, _) = accesskit_author_node(&initial_update, "workspace.splitter");
        let _ = render(
            &mut app,
            vec![Event::AccessKitActionRequest(accesskit::ActionRequest {
                action: accesskit::Action::Focus,
                target_tree: accesskit::TreeId::ROOT,
                target_node: splitter_id,
                data: None,
            })],
        );

        let key_event = |key, pressed| Event::Key {
            key,
            physical_key: Some(key),
            pressed,
            repeat: false,
            modifiers: Modifiers::NONE,
        };
        let key_tap = |key| vec![key_event(key, true), key_event(key, false)];
        let collapse = |app: &mut DbotterApp, key, restore_id: &str| {
            (0..128).find_map(|_| {
                let settled = render(app, Vec::new());
                let settled = settled.platform_output.accesskit_update?;
                if settled
                    .nodes
                    .iter()
                    .any(|(_, node)| node.author_id() == Some(restore_id))
                {
                    return Some(settled);
                }
                let (splitter_id, _) = accesskit_author_node(&settled, "workspace.splitter");
                let _ = render(
                    app,
                    vec![Event::AccessKitActionRequest(accesskit::ActionRequest {
                        action: accesskit::Action::Focus,
                        target_tree: accesskit::TreeId::ROOT,
                        target_node: splitter_id,
                        data: None,
                    })],
                );
                let output = render(app, key_tap(key));
                let update = output.platform_output.accesskit_update?;
                update
                    .nodes
                    .iter()
                    .any(|(_, node)| node.author_id() == Some(restore_id))
                    .then_some(update)
            })
        };

        let editor_collapsed = collapse(&mut app, Key::ArrowUp, "workspace.editor.restore")
            .unwrap_or_else(|| {
                panic!(
                    "crossing 160 points must expose Restore editor: geometry={:?}, collapsed={:?}",
                    app.workspace_geometries.get(&key),
                    app.collapsed_workspace_panes.get(&key)
                )
            });
        assert_eq!(
            app.workspace_geometries.get(&key).copied(),
            Some(WorkspaceGeometry::restore(
                280.0,
                super::WORKSPACE_EDITOR_COLLAPSED_SHARE,
                true,
            )),
            "editor collapse must use the exact durable lower-bound sentinel"
        );
        let (restore_editor_id, restore_editor) =
            accesskit_author_node(&editor_collapsed, "workspace.editor.restore");
        assert_eq!(restore_editor.role(), accesskit::Role::Button);
        assert_eq!(restore_editor.label(), Some("Restore editor"));
        let _ = render(
            &mut app,
            vec![Event::AccessKitActionRequest(accesskit::ActionRequest {
                action: accesskit::Action::Focus,
                target_tree: accesskit::TreeId::ROOT,
                target_node: restore_editor_id,
                data: None,
            })],
        );
        let _ = render(&mut app, key_tap(Key::Enter));
        let restored = render(&mut app, Vec::new());
        let restored = restored
            .platform_output
            .accesskit_update
            .expect("restored split frame must emit AccessKit");
        let (splitter_id, _) = accesskit_author_node(&restored, "workspace.splitter");
        for expected in ["object-editor-tabs", "result-history-tabs"] {
            assert!(
                restored
                    .nodes
                    .iter()
                    .any(|(_, node)| node.author_id() == Some(expected)),
                "restoring editor lost {expected}"
            );
        }
        assert_eq!(
            app.workspace_geometries
                .get(&key)
                .map(|geometry| geometry.editor_share()),
            Some(NativeLayout::DEFAULT_EDITOR_SHARE)
        );

        let _ = render(
            &mut app,
            vec![Event::AccessKitActionRequest(accesskit::ActionRequest {
                action: accesskit::Action::Focus,
                target_tree: accesskit::TreeId::ROOT,
                target_node: splitter_id,
                data: None,
            })],
        );
        let results_collapsed = collapse(&mut app, Key::ArrowDown, "workspace.results.restore")
            .expect("crossing 160 points must expose Restore results/history");
        assert_eq!(
            app.workspace_geometries.get(&key).copied(),
            Some(WorkspaceGeometry::restore(
                280.0,
                super::WORKSPACE_RESULTS_COLLAPSED_SHARE,
                false,
            )),
            "results collapse must durably hide the inspector at the upper-bound sentinel"
        );
        assert!(app.flush_selected_workspace());
        let collapsed_snapshot = match service.try_next_command() {
            Some(UiCommand::CommitWorkspace { snapshot, .. }) => {
                assert_eq!(
                    snapshot.geometry().editor_share(),
                    super::WORKSPACE_RESULTS_COLLAPSED_SHARE
                );
                assert!(!snapshot.geometry().inspector_visible());
                snapshot
            }
            command => panic!("keyboard collapse must commit its exact geometry, got {command:?}"),
        };

        let (restored_port, mut restored_service) = bounded_ports(4);
        let mut restored_app = DbotterApp::new(restored_port);
        assert!(restored_service.try_next_command().is_some());
        let restored_key = install_j2_profile(&mut restored_app, &profile);
        assert_eq!(restored_key, key);
        assert!(restored_app.ensure_workspace_persistence_binding(&restored_key, &profile));
        restored_app.request_workspace_load(&restored_key);
        let (load_operation, load_identity, base_revision) =
            match restored_service.try_next_command() {
                Some(UiCommand::LoadWorkspace {
                    operation_id,
                    identity,
                    base_revision,
                }) => (operation_id, identity, base_revision),
                command => panic!("exact restore must begin with LoadWorkspace, got {command:?}"),
            };
        let collapsed_bytes = encoded_profile_bytes_at_generation(&collapsed_snapshot, 1)
            .expect("collapsed workspace bytes")
            .1;
        assert!(restored_service.try_emit(UiEvent::WorkspaceLoaded {
            operation_id: load_operation,
            identity: load_identity,
            base_revision,
            mode: WorkspaceStoreMode::ReadWrite,
            read_only_reason: None,
            generation: Some(1),
            committed_bytes: collapsed_bytes,
            snapshot: Some(collapsed_snapshot),
        }));
        restored_app.poll_events();
        assert_eq!(
            restored_app
                .workspace_geometries
                .get(&restored_key)
                .copied(),
            Some(WorkspaceGeometry::restore(
                280.0,
                super::WORKSPACE_RESULTS_COLLAPSED_SHARE,
                false,
            ))
        );
        assert_eq!(
            restored_app.collapsed_workspace_panes.get(&restored_key),
            Some(&Pane::Subordinate)
        );
        let restored_ids = shell_author_ids(&mut restored_app, 1440.0, 900.0);
        assert!(
            restored_ids.contains("workspace.results.restore"),
            "exact restored inspector=false geometry must expose its named restore action"
        );
        let (restore_results_id, restore_results) =
            accesskit_author_node(&results_collapsed, "workspace.results.restore");
        assert_eq!(restore_results.role(), accesskit::Role::Button);
        assert_eq!(restore_results.label(), Some("Restore results/history"));
        let _ = render(
            &mut app,
            vec![Event::AccessKitActionRequest(accesskit::ActionRequest {
                action: accesskit::Action::Focus,
                target_tree: accesskit::TreeId::ROOT,
                target_node: restore_results_id,
                data: None,
            })],
        );
        let _ = render(&mut app, key_tap(Key::Enter));
        let restored = render(&mut app, Vec::new());
        let restored = restored
            .platform_output
            .accesskit_update
            .expect("second restored frame must emit AccessKit");
        for expected in [
            "workspace.splitter",
            "object-editor-tabs",
            "result-history-tabs",
        ] {
            assert!(
                restored
                    .nodes
                    .iter()
                    .any(|(_, node)| node.author_id() == Some(expected)),
                "restoring results lost {expected}"
            );
        }
        assert_eq!(
            app.workspace_geometries
                .get(&key)
                .map(|geometry| geometry.editor_share()),
            Some(NativeLayout::DEFAULT_EDITOR_SHARE)
        );
    }

    #[test]
    fn actual_compact_shell_exposes_one_named_surface_and_keeps_status_at_840_by_560() {
        let (ui_port, mut service) = bounded_ports(4);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let profile = profile(DriverKind::MySql, DriverAvailability::Ready);
        app.model.profiles = vec![profile.clone()];
        app.model.selected_profile = Some(profile.id.clone());
        app.model
            .active_generations
            .insert(profile.id.clone(), profile.generation);
        app.model
            .workspace_mut(WorkspaceKey::new(profile.id.clone(), profile.generation));

        let editor = shell_author_ids(&mut app, 840.0, 560.0);
        for expected in [
            "navigator.open",
            "inspector.open",
            "object-editor-tabs",
            "status-action-context",
        ] {
            assert!(
                editor.contains(expected),
                "missing actual compact editor AX id {expected}"
            );
        }
        assert!(!editor.contains("navigator"));
        assert!(!editor.contains("result-history-tabs"));

        app.compact_fallback
            .open(super::FallbackSurface::Navigator, "object-editor-tabs");
        let navigator = shell_author_ids(&mut app, 840.0, 560.0);
        for expected in ["navigator", "fallback.close", "status-action-context"] {
            assert!(
                navigator.contains(expected),
                "missing compact navigator AX id {expected}"
            );
        }
        assert!(!navigator.contains("object-editor-tabs"));
        assert!(!navigator.contains("result-history-tabs"));

        app.compact_fallback
            .open(super::FallbackSurface::Inspector, "object-editor-tabs");
        let inspector = shell_author_ids(&mut app, 840.0, 560.0);
        for expected in [
            "result-history-tabs",
            "fallback.close",
            "status-action-context",
        ] {
            assert!(
                inspector.contains(expected),
                "missing compact inspector AX id {expected}"
            );
        }
        assert!(!inspector.contains("navigator"));
        assert!(!inspector.contains("object-editor-tabs"));
    }

    #[test]
    fn actual_profile_cards_expose_exact_lifecycle_delete_and_credential_actions() {
        let render_ids = |state: ConnectionState, session_profile: bool| {
            let (ui_port, mut service) = bounded_ports(4);
            let mut app = DbotterApp::new(ui_port);
            assert!(service.try_next_command().is_some());
            let mut profile = profile(DriverKind::MySql, DriverAvailability::Ready);
            if session_profile {
                profile.persisted.credential_mode = CredentialMode::Session;
            }
            app.model.selected_profile = Some(profile.id.clone());
            app.model
                .active_generations
                .insert(profile.id.clone(), profile.generation);
            app.model
                .connection_states
                .insert(profile.id.clone(), state);
            app.model.profiles = vec![profile];

            let context = Context::default();
            context.enable_accesskit();
            context
                .run_ui(RawInput::default(), |ui| app.connections(ui))
                .platform_output
                .accesskit_update
                .expect("actual lifecycle frame must emit AccessKit")
                .nodes
                .into_iter()
                .filter_map(|(_, node)| node.author_id().map(str::to_owned))
                .collect::<BTreeSet<_>>()
        };

        let disconnected = render_ids(ConnectionState::Disconnected, false);
        assert!(disconnected.contains("connection.connect"));
        assert!(disconnected.contains("profile.delete"));

        let connected = render_ids(
            ConnectionState::Connected {
                session_generation: SessionGeneration(8),
                elapsed_ms: 3,
            },
            false,
        );
        assert!(connected.contains("connection.disconnect"));
        assert!(connected.contains("connection.reconnect"));
        assert!(connected.contains("profile.delete"));

        let needs_credential = render_ids(ConnectionState::NeedsCredential, true);
        assert!(needs_credential.contains("connection.credential.open"));
        assert!(needs_credential.contains("profile.delete"));
    }

    #[test]
    fn lifecycle_submits_exact_commands_and_sets_pending_only_after_acceptance() {
        let (ui_port, mut service) = bounded_ports(4);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let profile = profile(DriverKind::MySql, DriverAvailability::Ready);
        app.model.profiles = vec![profile.clone()];
        app.model.selected_profile = Some(profile.id.clone());
        app.model
            .active_generations
            .insert(profile.id.clone(), profile.generation);

        app.submit_test(profile.id.clone());
        let connect_operation = match service.try_next_command() {
            Some(UiCommand::TestConnection {
                operation_id,
                profile_id,
                profile_generation,
                timeout_ms,
            }) => {
                assert_eq!(profile_id, profile.id);
                assert_eq!(profile_generation, profile.generation);
                assert_eq!(timeout_ms, super::DEFAULT_TIMEOUT_MS);
                operation_id
            }
            _ => panic!("Connect must submit one exact non-secret TestConnection"),
        };
        assert_eq!(
            app.model.connection_state(&profile.id),
            &ConnectionState::Pending(connect_operation)
        );
        assert_eq!(
            app.active_operations.get(&profile.id),
            Some(&ActiveOperation {
                operation_id: connect_operation,
                profile_generation: profile.generation,
                kind: OperationKind::ConnectProfile,
            })
        );

        assert!(service.try_emit(crate::ui::UiEvent::ConnectionReady {
            operation_id: connect_operation,
            profile_id: profile.id.clone(),
            profile_generation: profile.generation,
            session_generation: SessionGeneration(11),
            elapsed_ms: 4,
        }));
        app.poll_events();
        app.submit_disconnect(profile.id.clone());
        let disconnect_operation = match service.try_next_command() {
            Some(UiCommand::DisconnectProfile {
                operation_id,
                profile_id,
                profile_generation,
            }) => {
                assert_eq!(profile_id, profile.id);
                assert_eq!(profile_generation, profile.generation);
                operation_id
            }
            _ => panic!("Disconnect must submit one exact control command"),
        };
        assert_eq!(
            app.model.connection_state(&profile.id),
            &ConnectionState::Pending(disconnect_operation)
        );

        assert!(service.try_emit(crate::ui::UiEvent::ConnectionClosed {
            operation_id: disconnect_operation,
            profile_id: profile.id.clone(),
            profile_generation: profile.generation,
            post_close: crate::ui::PostCloseState::Disconnected,
        }));
        app.poll_events();
        app.submit_reconnect(profile.id.clone());
        let reconnect_operation = match service.try_next_command() {
            Some(UiCommand::ReconnectProfile {
                operation_id,
                profile_id,
                profile_generation,
                timeout_ms,
            }) => {
                assert_eq!(profile_id, profile.id);
                assert_eq!(profile_generation, profile.generation);
                assert_eq!(timeout_ms, super::DEFAULT_TIMEOUT_MS);
                operation_id
            }
            _ => panic!("Reconnect must submit one exact control command"),
        };
        assert_eq!(
            app.model.connection_state(&profile.id),
            &ConnectionState::Pending(reconnect_operation)
        );

        for lifecycle in [
            OperationKind::ConnectProfile,
            OperationKind::DisconnectProfile,
            OperationKind::ReconnectProfile,
        ] {
            let (ui_port, mut service) = bounded_ports(1);
            let mut blocked = DbotterApp::new(ui_port);
            assert!(service.try_next_command().is_some());
            blocked.model.profiles = vec![profile.clone()];
            blocked
                .model
                .active_generations
                .insert(profile.id.clone(), profile.generation);
            blocked.model.connection_states.insert(
                profile.id.clone(),
                ConnectionState::Connected {
                    session_generation: SessionGeneration(12),
                    elapsed_ms: 1,
                },
            );
            let filler = match lifecycle {
                OperationKind::ConnectProfile => UiCommand::TestConnection {
                    operation_id: OperationId(800),
                    profile_id: ProfileId("filler".to_owned()),
                    profile_generation: ProfileGeneration(1),
                    timeout_ms: 1,
                },
                OperationKind::DisconnectProfile | OperationKind::ReconnectProfile => {
                    UiCommand::CancelOperation {
                        operation_id: OperationId(801),
                    }
                }
                _ => unreachable!("closed lifecycle fixture"),
            };
            assert_eq!(blocked.port.try_submit(filler), Ok(()));
            match lifecycle {
                OperationKind::ConnectProfile => {
                    blocked
                        .model
                        .connection_states
                        .insert(profile.id.clone(), ConnectionState::Disconnected);
                    blocked.submit_test(profile.id.clone());
                    assert_eq!(
                        blocked.model.connection_state(&profile.id),
                        &ConnectionState::Disconnected
                    );
                }
                OperationKind::DisconnectProfile => {
                    blocked.submit_disconnect(profile.id.clone());
                    assert!(matches!(
                        blocked.model.connection_state(&profile.id),
                        ConnectionState::Connected { .. }
                    ));
                }
                OperationKind::ReconnectProfile => {
                    blocked.submit_reconnect(profile.id.clone());
                    assert!(matches!(
                        blocked.model.connection_state(&profile.id),
                        ConnectionState::Connected { .. }
                    ));
                }
                _ => unreachable!("closed lifecycle fixture"),
            }
            assert!(!blocked.active_operations.contains_key(&profile.id));
        }
    }

    #[test]
    fn credential_prompt_stores_exact_generation_then_retries_connect_exactly_once() {
        let (ui_port, mut service) = bounded_ports(4);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let mut session_profile = profile(DriverKind::MySql, DriverAvailability::Ready);
        session_profile.persisted.credential_mode = CredentialMode::Session;
        session_profile.has_current_session_secret = false;
        app.model.profiles = vec![session_profile.clone()];
        app.model
            .active_generations
            .insert(session_profile.id.clone(), session_profile.generation);
        app.model
            .connection_states
            .insert(session_profile.id.clone(), ConnectionState::NeedsCredential);

        app.submit_test(session_profile.id.clone());
        assert!(service.try_next_command().is_none());
        assert!(
            app.profile_editor.is_none(),
            "credential entry is not profile editing"
        );
        let prompt = app
            .credential_prompt
            .as_mut()
            .expect("protected credential prompt");
        prompt.secret =
            crate::secrets::ReplacementSecretBuffer::new("memory-only-secret".to_owned());
        app.submit_credential_prompt();
        let store_command = service
            .try_next_command()
            .expect("credential prompt must submit one command");
        assert!(
            !format!("{store_command:?}").contains("memory-only-secret"),
            "credential commands must redact their protected payload"
        );
        let store_operation = match store_command {
            UiCommand::StoreCredentials {
                operation_id,
                profile_id,
                profile_generation,
                source_operation,
                ..
            } => {
                assert_eq!(profile_id, session_profile.id);
                assert_eq!(profile_generation, session_profile.generation);
                assert_eq!(source_operation, OperationKind::ConnectProfile);
                operation_id
            }
            _ => panic!("credential prompt must submit one StoreCredentials command"),
        };
        assert!(service.try_next_command().is_none());
        assert!(service.try_emit(crate::ui::UiEvent::CredentialsStored {
            operation_id: store_operation,
            profile_id: session_profile.id.clone(),
            profile_generation: session_profile.generation,
        }));
        app.poll_events();
        let retry_operation = match service.try_next_command() {
            Some(UiCommand::TestConnection {
                operation_id,
                profile_id,
                profile_generation,
                timeout_ms,
            }) => {
                assert_eq!(profile_id, session_profile.id);
                assert_eq!(profile_generation, session_profile.generation);
                assert_eq!(timeout_ms, super::DEFAULT_TIMEOUT_MS);
                operation_id
            }
            _ => panic!("successful credential storage retries the exact connect recipe"),
        };
        assert_ne!(retry_operation, store_operation);
        assert_eq!(
            app.model.active_generation(&session_profile.id),
            Some(session_profile.generation),
            "credential storage must not mutate profile generation"
        );

        assert!(service.try_emit(crate::ui::UiEvent::CredentialsStored {
            operation_id: store_operation,
            profile_id: session_profile.id.clone(),
            profile_generation: session_profile.generation,
        }));
        app.poll_events();
        assert!(
            service.try_next_command().is_none(),
            "store ack retries only once"
        );
    }

    #[test]
    fn typed_open_credential_recovery_opens_prompt_only_with_the_exact_retry_recipe() {
        let (ui_port, mut service) = bounded_ports(4);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let mut session_profile = profile(DriverKind::MySql, DriverAvailability::Ready);
        session_profile.persisted.credential_mode = CredentialMode::Session;
        session_profile.has_current_session_secret = true;
        app.model.profiles = vec![session_profile.clone()];
        app.model
            .active_generations
            .insert(session_profile.id.clone(), session_profile.generation);

        app.submit_test(session_profile.id.clone());
        let operation_id = match service.try_next_command() {
            Some(UiCommand::TestConnection { operation_id, .. }) => operation_id,
            _ => panic!("connect command"),
        };
        let recipe_id = OperationRecipeId(operation_id.0);
        let error = PublicOperationError::new_or_internal(
            OperationKind::ConnectProfile,
            PublicSummary::AuthenticationFailed,
            PublicCode::SessionCredential,
            &SafeContext::profile_with_recipe(session_profile.id.clone(), operation_id, recipe_id),
        );
        assert!(service.try_emit(crate::ui::UiEvent::OperationFailed {
            operation_id,
            profile_id: session_profile.id.clone(),
            profile_generation: session_profile.generation,
            session_generation: None,
            kind: OperationKind::ConnectProfile,
            summary: error.summary,
            error: error.clone(),
            session_disposition: None,
            connection_outcome: crate::ui::ConnectionFailureOutcome::NeedsCredential,
        }));
        app.poll_events();

        let action = RecoveryAction::OpenCredentialPrompt(session_profile.id.clone());
        app.dispatch_error_recovery(operation_id, &error, action);

        assert!(app.profile_editor.is_none(), "secret entry is prompt-only");
        let prompt = app.credential_prompt.as_ref().expect("credential prompt");
        assert_eq!(prompt.profile_id, session_profile.id);
        assert_eq!(prompt.profile_generation, session_profile.generation);
        assert_eq!(prompt.source_operation, OperationKind::ConnectProfile);
        assert_eq!(prompt.retry_recipe_id, Some(recipe_id));
        assert!(service.try_next_command().is_none());
    }

    #[test]
    fn credential_cancel_store_failure_and_generation_change_never_retry() {
        let (ui_port, mut service) = bounded_ports(4);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let mut session_profile = profile(DriverKind::MySql, DriverAvailability::Ready);
        session_profile.persisted.credential_mode = CredentialMode::Session;
        app.model.profiles = vec![session_profile.clone()];
        app.model
            .active_generations
            .insert(session_profile.id.clone(), session_profile.generation);

        app.open_session_credential_prompt(session_profile.id.clone());
        app.credential_prompt.as_mut().expect("prompt").secret =
            crate::secrets::ReplacementSecretBuffer::new("cancelled-secret".to_owned());
        app.cancel_credential_prompt();
        assert!(app.credential_prompt.is_none());
        assert!(service.try_next_command().is_none());

        app.open_session_credential_prompt(session_profile.id.clone());
        app.credential_prompt.as_mut().expect("prompt").secret =
            crate::secrets::ReplacementSecretBuffer::new("stale-secret".to_owned());
        app.submit_credential_prompt();
        let store_operation = match service.try_next_command() {
            Some(UiCommand::StoreCredentials { operation_id, .. }) => operation_id,
            _ => panic!("StoreCredentials command"),
        };
        app.model.active_generations.insert(
            session_profile.id.clone(),
            ProfileGeneration(session_profile.generation.0 + 1),
        );
        assert!(service.try_emit(crate::ui::UiEvent::CredentialsStored {
            operation_id: store_operation,
            profile_id: session_profile.id.clone(),
            profile_generation: session_profile.generation,
        }));
        app.poll_events();
        assert!(app.credential_prompt.is_none());
        assert!(service.try_next_command().is_none());

        let (ui_port, mut service) = bounded_ports(4);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        app.model.profiles = vec![session_profile.clone()];
        app.model
            .active_generations
            .insert(session_profile.id.clone(), session_profile.generation);
        app.open_session_credential_prompt(session_profile.id.clone());
        app.credential_prompt.as_mut().expect("prompt").secret =
            crate::secrets::ReplacementSecretBuffer::new("failed-secret".to_owned());
        app.submit_credential_prompt();
        let store_operation = match service.try_next_command() {
            Some(UiCommand::StoreCredentials { operation_id, .. }) => operation_id,
            _ => panic!("StoreCredentials command"),
        };
        let error = PublicOperationError::new_or_internal(
            OperationKind::UpdateProfile,
            PublicSummary::ResourceStale,
            PublicCode::ProfileStale,
            &SafeContext::profile(session_profile.id.clone(), store_operation),
        );
        assert!(
            service.try_emit(crate::ui::UiEvent::CredentialsStoreFailed {
                operation_id: store_operation,
                profile_id: session_profile.id.clone(),
                profile_generation: session_profile.generation,
                summary: error.summary,
                error,
            })
        );
        app.poll_events();
        assert!(app.credential_prompt.is_none());
        assert!(service.try_next_command().is_none());
    }

    #[test]
    fn rejected_delete_submission_keeps_confirmation_and_pending_state_local() {
        let (ui_port, mut service) = bounded_ports(1);
        let mut blocked_delete = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let profile = profile(DriverKind::MySql, DriverAvailability::Ready);
        blocked_delete.model.profiles = vec![profile.clone()];
        blocked_delete
            .model
            .active_generations
            .insert(profile.id.clone(), profile.generation);
        blocked_delete.open_delete_confirmation(&profile);
        assert_eq!(
            blocked_delete.port.try_submit(UiCommand::RefreshProfiles {
                operation_id: OperationId(903),
            }),
            Ok(())
        );
        blocked_delete.confirm_delete_confirmation();
        assert!(blocked_delete.delete_confirmation.is_some());
        assert_eq!(
            blocked_delete.model.connection_state(&profile.id),
            &ConnectionState::Disconnected
        );
        assert!(!blocked_delete.active_operations.contains_key(&profile.id));
        assert!(matches!(
            service.try_next_command(),
            Some(UiCommand::RefreshProfiles {
                operation_id: OperationId(903)
            })
        ));
        assert!(service.try_next_command().is_none());
    }

    #[test]
    fn generation_checked_catalog_and_redis_recipes_replay_exact_requests() {
        let (ui_port, mut service) = bounded_ports(8);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let mysql = profile(DriverKind::MySql, DriverAvailability::Ready);
        app.model.profiles = vec![mysql.clone()];
        app.model.selected_profile = Some(mysql.id.clone());
        app.model
            .active_generations
            .insert(mysql.id.clone(), mysql.generation);
        app.submit_mysql_explorer_intent(
            &mysql,
            MySqlExplorerIntent::RefreshSchemas {
                prefix: Some("app".to_owned()),
            },
        );
        let request = match service.try_next_command() {
            Some(UiCommand::BrowseCatalog(request)) => request,
            _ => panic!("catalog request"),
        };
        let recipe_id = OperationRecipeId(request.operation_id().0);
        let error = PublicOperationError::new_or_internal(
            OperationKind::BrowseMySql,
            PublicSummary::ResourceStale,
            PublicCode::None,
            &SafeContext::profile_with_recipe(mysql.id.clone(), request.operation_id(), recipe_id),
        );
        assert!(service.try_emit(crate::ui::UiEvent::CatalogPageFailed {
            request: request.clone(),
            summary: error.summary,
            error: error.clone(),
            session_generation: None,
            session_disposition: None,
        }));
        app.poll_events();
        app.dispatch_error_recovery(
            request.operation_id(),
            &error,
            RecoveryAction::Retry(recipe_id),
        );
        let retried = match service.try_next_command() {
            Some(UiCommand::BrowseCatalog(request)) => request,
            _ => panic!("typed catalog Retry must replay one exact request"),
        };
        assert_eq!(retried.profile_id(), request.profile_id());
        assert_eq!(retried.profile_generation(), request.profile_generation());
        assert_ne!(retried.operation_id(), request.operation_id());

        let retried_recipe_id = OperationRecipeId(retried.operation_id().0);
        let retried_error = PublicOperationError::new_or_internal(
            OperationKind::BrowseMySql,
            PublicSummary::ResourceStale,
            PublicCode::None,
            &SafeContext::profile_with_recipe(
                mysql.id.clone(),
                retried.operation_id(),
                retried_recipe_id,
            ),
        );
        assert!(service.try_emit(crate::ui::UiEvent::CatalogPageFailed {
            request: retried,
            summary: retried_error.summary,
            error: retried_error.clone(),
            session_generation: None,
            session_disposition: None,
        }));
        app.poll_events();
        app.model
            .active_generations
            .insert(mysql.id.clone(), ProfileGeneration(mysql.generation.0 + 1));
        app.dispatch_error_recovery(
            OperationId(retried_recipe_id.0),
            &retried_error,
            RecoveryAction::Retry(retried_recipe_id),
        );
        assert!(service.try_next_command().is_none());

        let (ui_port, mut service) = bounded_ports(8);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let redis = profile(DriverKind::Redis, DriverAvailability::Ready);
        app.model.profiles = vec![redis.clone()];
        app.model.selected_profile = Some(redis.id.clone());
        app.model
            .active_generations
            .insert(redis.id.clone(), redis.generation);
        render_redis_explorer(&mut app);
        app.submit_redis_intent(RedisExplorerIntent::Scan {
            filter: RedisKeyFilter::LiteralPrefix("orders:".to_owned()),
            cursor: 41,
            restart: false,
        });
        let request = match service.try_next_command() {
            Some(UiCommand::ScanRedisKeys(request)) => request,
            _ => panic!("Redis scan request"),
        };
        let recipe_id = OperationRecipeId(request.operation_id().0);
        let error = PublicOperationError::new_or_internal(
            OperationKind::BrowseRedis,
            PublicSummary::ResourceStale,
            PublicCode::None,
            &SafeContext::profile_with_recipe(redis.id.clone(), request.operation_id(), recipe_id),
        );
        assert!(service.try_emit(crate::ui::UiEvent::RedisKeysFailed {
            request: request.clone(),
            error: error.clone(),
            session_generation: None,
            session_disposition: None,
            connection_outcome: crate::ui::ConnectionFailureOutcome::Preserve,
        }));
        app.poll_events();
        app.dispatch_error_recovery(
            request.operation_id(),
            &error,
            RecoveryAction::Retry(recipe_id),
        );
        let retried = match service.try_next_command() {
            Some(UiCommand::ScanRedisKeys(request)) => request,
            _ => panic!("typed Redis Retry must replay one exact request"),
        };
        assert_eq!(retried.filter, request.filter);
        assert_eq!(retried.cursor, request.cursor);
        assert_eq!(retried.profile_generation(), request.profile_generation());
        assert_ne!(retried.operation_id(), request.operation_id());
    }

    #[test]
    fn mysql_data_action_opens_a_new_tab_and_submits_the_exact_bounded_read() {
        let (ui_port, mut service) = bounded_ports(4);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let mysql = profile(DriverKind::MySql, DriverAvailability::Ready);
        let key = WorkspaceKey::new(mysql.id.clone(), mysql.generation);
        app.model.profiles = vec![mysql.clone()];
        app.model.selected_profile = Some(mysql.id.clone());
        app.model
            .active_generations
            .insert(mysql.id.clone(), mysql.generation);

        let original_tab = app
            .model
            .workspace_mut(key.clone())
            .create_editor_tab(QueryLanguage::Sql, "Draft", "SELECT draft_value")
            .expect("original tab");
        app.submit_mysql_explorer_intent(
            &mysql,
            MySqlExplorerIntent::InsertTemplate(
                "SELECT * FROM `app`.`widgets` LIMIT 200".to_owned(),
            ),
        );

        let workspace = app.model.workspace(&key).expect("workspace retained");
        assert_eq!(workspace.editor_tabs().len(), 2);
        assert_eq!(
            workspace
                .editor_tab(original_tab)
                .expect("original tab retained")
                .text(),
            "SELECT draft_value"
        );
        let selected = workspace
            .selected_editor_tab_id()
            .expect("data tab selected");
        assert_ne!(selected, original_tab);
        assert_eq!(
            workspace.editor_tab(selected).expect("data tab").text(),
            "SELECT * FROM `app`.`widgets` LIMIT 200"
        );
        let operation_id = match service
            .try_next_command()
            .expect("Data must submit the bounded read without a second action")
        {
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
                assert_eq!(profile_id, mysql.id);
                assert_eq!(profile_generation, mysql.generation);
                assert_eq!(editor_tab_id, Some(selected));
                assert_eq!(language, QueryLanguage::Sql);
                assert_eq!(text, "SELECT * FROM `app`.`widgets` LIMIT 200");
                assert_eq!(row_limit, 500);
                assert_eq!(timeout_ms, 30_000);
                operation_id
            }
            other => panic!("Data submitted the wrong command: {other:?}"),
        };
        assert_eq!(
            app.model
                .workspace(&key)
                .expect("workspace retained")
                .pending_execute,
            Some(operation_id)
        );
        assert_eq!(app.model.status, "Executing…");
    }

    #[test]
    fn mysql_new_editor_preserves_draft_and_selected_object_across_refresh_and_result_switch() {
        let (ui_port, mut service) = bounded_ports(8);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let mut mysql = profile(DriverKind::MySql, DriverAvailability::Ready);
        mysql.database = Some("app_db".to_owned());
        mysql.persisted.database = Some("app_db".to_owned());
        let key = WorkspaceKey::new(mysql.id.clone(), mysql.generation);
        app.model.profiles = vec![mysql.clone()];
        app.model.selected_profile = Some(mysql.id.clone());
        app.model
            .active_generations
            .insert(mysql.id.clone(), mysql.generation);
        let original_tab = app
            .model
            .workspace_mut(key.clone())
            .create_editor_tab(QueryLanguage::Sql, "Draft", "SELECT draft_value")
            .expect("original tab");

        let explorer_key = (mysql.id.clone(), mysql.generation);
        app.mysql_explorers
            .entry(explorer_key.clone())
            .or_default()
            .handle_loaded(CatalogPage {
                identity: RequestIdentity::new(mysql.id.clone(), mysql.generation, OperationId(80)),
                level: CatalogLevel::Schemas,
                parent: None,
                nodes: vec![CatalogNode {
                    identity: CatalogNodeIdentity::Schema {
                        schema: "app".to_owned(),
                    },
                    kind: CatalogNodeKind::Schema,
                    name: "app".to_owned(),
                    type_name: None,
                    nullable: None,
                    ordinal: None,
                }],
                next_token: None,
                retained_counts: CatalogRetainedCounts::default(),
                retained_utf8_bytes: 0,
                truncated: false,
                stale: false,
                loaded_at: "2026-07-16T00:00:00Z".to_owned(),
            });

        let context = Context::default();
        context.enable_accesskit();
        let initial = context.run_ui(RawInput::default(), |ui| app.explorer_contents(ui));
        let initial_update = initial
            .platform_output
            .accesskit_update
            .expect("schema explorer AccessKit tree");
        let show_relations = initial_update
            .nodes
            .iter()
            .find_map(|(node_id, node)| {
                (node.label() == Some("Show relations")).then_some(*node_id)
            })
            .expect("schema row must expose Show relations");
        let mut focus_relations = RawInput::default();
        focus_relations
            .events
            .push(Event::AccessKitActionRequest(accesskit::ActionRequest {
                action: accesskit::Action::Focus,
                target_tree: accesskit::TreeId::ROOT,
                target_node: show_relations,
                data: None,
            }));
        let _ = context.run_ui(focus_relations, |ui| app.explorer_contents(ui));
        let _ = context.run_ui(
            RawInput {
                events: vec![Event::Key {
                    key: Key::Enter,
                    physical_key: Some(Key::Enter),
                    pressed: true,
                    repeat: false,
                    modifiers: Modifiers::NONE,
                }],
                ..RawInput::default()
            },
            |ui| app.explorer_contents(ui),
        );
        let relation_request = match service.try_next_command() {
            Some(UiCommand::BrowseCatalog(request)) => request,
            other => panic!("Show relations submitted the wrong command: {other:?}"),
        };
        assert!(matches!(
            &relation_request,
            crate::model::CatalogRequest::Relations { schema, .. } if schema == "app"
        ));

        let relation_page = |operation_id| CatalogPage {
            identity: RequestIdentity::new(mysql.id.clone(), mysql.generation, operation_id),
            level: CatalogLevel::Relations,
            parent: Some(CatalogNodeIdentity::Schema {
                schema: "app".to_owned(),
            }),
            nodes: vec![CatalogNode {
                identity: CatalogNodeIdentity::Relation {
                    schema: "app".to_owned(),
                    relation: "widgets".to_owned(),
                },
                kind: CatalogNodeKind::Table,
                name: "widgets".to_owned(),
                type_name: None,
                nullable: None,
                ordinal: None,
            }],
            next_token: None,
            retained_counts: CatalogRetainedCounts::default(),
            retained_utf8_bytes: 0,
            truncated: false,
            stale: false,
            loaded_at: "2026-07-16T00:00:00Z".to_owned(),
        };
        app.mysql_explorers
            .get_mut(&explorer_key)
            .expect("MySQL explorer")
            .handle_loaded(relation_page(relation_request.operation_id()));

        let relation_output = context.run_ui(RawInput::default(), |ui| app.explorer_contents(ui));
        let relation_update = relation_output
            .platform_output
            .accesskit_update
            .expect("relation explorer AccessKit tree");
        let new_editor = relation_update
            .nodes
            .iter()
            .find_map(|(node_id, node)| {
                node.author_id()
                    .is_some_and(|id| id.starts_with("navigator.object.new-editor."))
                    .then_some(*node_id)
            })
            .expect("relation row must expose a stable New editor action");
        let mut focus_new_editor = RawInput::default();
        focus_new_editor
            .events
            .push(Event::AccessKitActionRequest(accesskit::ActionRequest {
                action: accesskit::Action::Focus,
                target_tree: accesskit::TreeId::ROOT,
                target_node: new_editor,
                data: None,
            }));
        let _ = context.run_ui(focus_new_editor, |ui| app.explorer_contents(ui));
        let _ = context.run_ui(
            RawInput {
                events: vec![Event::Key {
                    key: Key::Enter,
                    physical_key: Some(Key::Enter),
                    pressed: true,
                    repeat: false,
                    modifiers: Modifiers::NONE,
                }],
                ..RawInput::default()
            },
            |ui| app.explorer_contents(ui),
        );
        assert!(
            service.try_next_command().is_none(),
            "New editor is local workspace work and must not submit network I/O"
        );

        let workspace = app.model.workspace(&key).expect("workspace retained");
        assert_eq!(workspace.editor_tabs().len(), 2);
        assert_eq!(
            workspace
                .editor_tab(original_tab)
                .expect("original draft retained")
                .text(),
            "SELECT draft_value"
        );
        let selected_editor = workspace
            .selected_editor_tab_id()
            .expect("context editor selected");
        let selected_editor = workspace
            .editor_tab(selected_editor)
            .expect("selected context editor");
        assert_eq!(selected_editor.title(), "app.widgets");
        assert_eq!(selected_editor.text(), "");

        let first_result = app
            .model
            .workspace_mut(key.clone())
            .append_result_tab(Arc::new(result_snapshot(&mysql, "first")))
            .expect("first result");
        let _second_result = app
            .model
            .workspace_mut(key.clone())
            .append_result_tab(Arc::new(result_snapshot(&mysql, "second")))
            .expect("second result");
        app.model
            .workspace_mut(key.clone())
            .select_result_tab(first_result)
            .expect("result switch");
        app.mysql_explorers
            .get_mut(&explorer_key)
            .expect("MySQL explorer")
            .handle_loaded(relation_page(OperationId(81)));

        let explorer_after = context.run_ui(RawInput::default(), |ui| app.explorer_contents(ui));
        let explorer_update = explorer_after
            .platform_output
            .accesskit_update
            .expect("refreshed explorer AccessKit tree");
        let (_, selected_object) =
            accesskit_author_node(&explorer_update, "navigator.object.selected-context");
        assert_eq!(selected_object.value(), Some("app.widgets · Table"));

        let workspace_context = Context::default();
        workspace_context.enable_accesskit();
        let workspace_output =
            workspace_context.run_ui(RawInput::default(), |ui| app.editor_and_results(ui));
        let workspace_update = workspace_output
            .platform_output
            .accesskit_update
            .expect("workspace AccessKit tree");
        let (_, breadcrumb) = accesskit_author_node(&workspace_update, "workspace.context");
        assert_eq!(
            breadcrumb.value(),
            Some("Profile → app_db → app.widgets · Table")
        );
    }

    #[test]
    fn actual_app_renders_common_catalog_and_redis_typed_recovery_actions() {
        let render_ids = |app: &mut DbotterApp, explorer_only: bool| {
            let context = Context::default();
            context.enable_accesskit();
            context
                .run_ui(RawInput::default(), |ui| {
                    if explorer_only {
                        app.explorer_contents(ui);
                    } else {
                        app.show_native(ui);
                    }
                })
                .platform_output
                .accesskit_update
                .expect("actual recovery frame must emit AccessKit")
                .nodes
                .into_iter()
                .filter_map(|(_, node)| node.author_id().map(str::to_owned))
                .collect::<BTreeSet<_>>()
        };

        let (ui_port, mut service) = bounded_ports(8);
        let mut common = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let mysql = profile(DriverKind::MySql, DriverAvailability::Ready);
        common.model.profiles = vec![mysql.clone()];
        common.model.selected_profile = Some(mysql.id.clone());
        common
            .model
            .active_generations
            .insert(mysql.id.clone(), mysql.generation);
        common.submit_test(mysql.id.clone());
        let operation_id = match service.try_next_command() {
            Some(UiCommand::TestConnection { operation_id, .. }) => operation_id,
            _ => panic!("connect command"),
        };
        let recipe_id = OperationRecipeId(operation_id.0);
        let error = PublicOperationError::new_or_internal(
            OperationKind::ConnectProfile,
            PublicSummary::NetworkUnavailable,
            PublicCode::None,
            &SafeContext::profile_with_recipe(mysql.id.clone(), operation_id, recipe_id),
        );
        assert!(service.try_emit(crate::ui::UiEvent::OperationFailed {
            operation_id,
            profile_id: mysql.id.clone(),
            profile_generation: mysql.generation,
            session_generation: None,
            kind: OperationKind::ConnectProfile,
            summary: error.summary,
            error,
            session_disposition: None,
            connection_outcome: crate::ui::ConnectionFailureOutcome::Disconnected,
        }));
        common.poll_events();
        let ids = render_ids(&mut common, false);
        assert!(ids.contains("recovery.common.edit_profile"));
        assert!(ids.contains("recovery.common.reconnect"));
        assert!(ids.contains("recovery.common.retry"));

        let (ui_port, mut service) = bounded_ports(8);
        let mut catalog = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        catalog.model.profiles = vec![mysql.clone()];
        catalog.model.selected_profile = Some(mysql.id.clone());
        catalog
            .model
            .active_generations
            .insert(mysql.id.clone(), mysql.generation);
        catalog.submit_mysql_explorer_intent(
            &mysql,
            MySqlExplorerIntent::RefreshSchemas { prefix: None },
        );
        let request = match service.try_next_command() {
            Some(UiCommand::BrowseCatalog(request)) => request,
            _ => panic!("catalog command"),
        };
        let recipe_id = OperationRecipeId(request.operation_id().0);
        let error = PublicOperationError::new_or_internal(
            OperationKind::BrowseMySql,
            PublicSummary::ResourceStale,
            PublicCode::None,
            &SafeContext::profile_with_recipe(mysql.id.clone(), request.operation_id(), recipe_id),
        );
        assert!(service.try_emit(crate::ui::UiEvent::CatalogPageFailed {
            request,
            summary: error.summary,
            error,
            session_generation: None,
            session_disposition: None,
        }));
        catalog.poll_events();
        assert!(render_ids(&mut catalog, true).contains("recovery.catalog.retry"));

        let (ui_port, mut service) = bounded_ports(8);
        let mut redis_app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let redis = profile(DriverKind::Redis, DriverAvailability::Ready);
        redis_app.model.profiles = vec![redis.clone()];
        redis_app.model.selected_profile = Some(redis.id.clone());
        redis_app
            .model
            .active_generations
            .insert(redis.id.clone(), redis.generation);
        render_redis_explorer(&mut redis_app);
        redis_app.submit_redis_intent(RedisExplorerIntent::Scan {
            filter: RedisKeyFilter::Glob("*".to_owned()),
            cursor: 0,
            restart: true,
        });
        let request = match service.try_next_command() {
            Some(UiCommand::ScanRedisKeys(request)) => request,
            _ => panic!("Redis command"),
        };
        let recipe_id = OperationRecipeId(request.operation_id().0);
        let error = PublicOperationError::new_or_internal(
            OperationKind::BrowseRedis,
            PublicSummary::ResourceStale,
            PublicCode::None,
            &SafeContext::profile_with_recipe(redis.id.clone(), request.operation_id(), recipe_id),
        );
        assert!(service.try_emit(crate::ui::UiEvent::RedisKeysFailed {
            request,
            error,
            session_generation: None,
            session_disposition: None,
            connection_outcome: crate::ui::ConnectionFailureOutcome::Preserve,
        }));
        redis_app.poll_events();
        assert!(render_ids(&mut redis_app, true).contains("recovery.redis_scan.retry"));
    }

    #[test]
    fn mutation_execute_never_registers_or_dispatches_retry() {
        let (ui_port, mut service) = bounded_ports(4);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let mysql = profile(DriverKind::MySql, DriverAvailability::Ready);
        let key = WorkspaceKey::new(mysql.id.clone(), mysql.generation);
        app.model.profiles = vec![mysql.clone()];
        app.model.selected_profile = Some(mysql.id.clone());
        app.model
            .active_generations
            .insert(mysql.id.clone(), mysql.generation);
        app.model.workspace_mut(key.clone()).editor_text =
            "UPDATE inventory SET count = count + 1".to_owned();
        let intent = build_execute_intent(
            &mysql,
            app.model.workspace(&key).expect("workspace"),
            EditorCursor::caret(0),
        )
        .expect("mutation intent");
        assert_eq!(intent.operation_kind(), OperationKind::ExecuteMutation);
        app.submit_editor_intent(EditorIntent::Execute(intent));
        let operation_id = match service.try_next_command() {
            Some(UiCommand::Execute { operation_id, .. }) => operation_id,
            _ => panic!("mutation command"),
        };
        let recipe_id = OperationRecipeId(operation_id.0);
        assert!(!app.retry_recipes.contains(recipe_id));

        let forged = PublicOperationError {
            operation: OperationKind::ExecuteMutation,
            category: crate::public_error::ErrorCategory::Network,
            code: PublicCode::None,
            summary: PublicSummary::NetworkUnavailable,
            recovery: crate::public_error::NonEmpty::new(RecoveryAction::Retry(recipe_id)),
        };
        app.dispatch_error_recovery(operation_id, &forged, RecoveryAction::Retry(recipe_id));
        assert!(service.try_next_command().is_none());
    }

    #[test]
    fn retry_recipe_registry_is_bounded_and_expires_the_oldest_recipe() {
        let mut registry = super::RetryRecipeRegistry::default();
        let profile_id = ProfileId("bounded-recipes".to_owned());
        for value in 1..=(super::RETRY_RECIPE_LIMIT as u64 + 1) {
            registry.register(
                OperationId(value),
                super::RetryRecipe::Connect {
                    profile_id: profile_id.clone(),
                    profile_generation: ProfileGeneration(1),
                    timeout_ms: 30_000,
                },
            );
        }
        assert_eq!(registry.recipes.len(), super::RETRY_RECIPE_LIMIT);
        assert!(!registry.contains(OperationRecipeId(1)));
        assert!(registry.contains(OperationRecipeId(super::RETRY_RECIPE_LIMIT as u64 + 1)));
    }

    #[test]
    fn confirmed_delete_is_exact_cancel_is_pure_and_unknown_truth_is_visible() {
        let (ui_port, mut service) = bounded_ports(4);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let profile = profile(DriverKind::MySql, DriverAvailability::Ready);
        app.model.profiles = vec![profile.clone()];
        app.model.selected_profile = Some(profile.id.clone());
        app.model
            .active_generations
            .insert(profile.id.clone(), profile.generation);
        let workspace_key = WorkspaceKey::new(profile.id.clone(), profile.generation);
        app.model.workspace_mut(workspace_key.clone()).editor_text =
            "UPDATE inventory SET count = count + 1".to_owned();
        let execute = build_execute_intent(
            &profile,
            app.model.workspace(&workspace_key).expect("workspace"),
            EditorCursor::caret(0),
        )
        .expect("mutation intent");
        app.submit_editor_intent(EditorIntent::Execute(execute));
        let active_operation = match service.try_next_command() {
            Some(UiCommand::Execute { operation_id, .. }) => operation_id,
            _ => panic!("mutation must be active before delete opens"),
        };
        assert_eq!(
            app.active_operations.get(&profile.id),
            Some(&ActiveOperation {
                operation_id: active_operation,
                profile_generation: profile.generation,
                kind: OperationKind::ExecuteMutation,
            })
        );

        app.model.config = ConfigPresentation::for_source(
            ConfigSourceVersion::V1,
            std::path::Path::new("/tmp/dbotter-delete-config.toml"),
        );
        app.open_delete_confirmation(&profile);
        let context = Context::default();
        context.enable_accesskit();
        let output = context.run_ui(RawInput::default(), |ui| {
            app.show_delete_confirmation(ui, false)
        });
        let warning = output
            .platform_output
            .accesskit_update
            .expect("delete dialog AccessKit")
            .nodes
            .into_iter()
            .find_map(|(_, node)| {
                (node.author_id() == Some("profile.delete.active_warning"))
                    .then(|| node.label().map(str::to_owned))
            })
            .flatten()
            .expect("active delete warning");
        assert_eq!(
            warning,
            "ExecuteMutation is active. Dbotter will stop waiting; the server operation may continue."
        );

        app.cancel_delete_confirmation();
        assert!(service.try_next_command().is_none());
        assert_eq!(
            app.active_operations.get(&profile.id),
            Some(&ActiveOperation {
                operation_id: active_operation,
                profile_generation: profile.generation,
                kind: OperationKind::ExecuteMutation,
            })
        );

        app.open_delete_confirmation(&profile);
        app.delete_confirmation
            .as_mut()
            .expect("delete confirmation")
            .migration_confirmed = true;
        app.confirm_delete_confirmation();
        let delete_operation = match service.try_next_command() {
            Some(UiCommand::DeleteProfile(request)) => {
                assert_eq!(request.profile_id, profile.id);
                assert_eq!(request.expected_generation, profile.generation);
                assert_eq!(
                    request.migration_consent,
                    super::MigrationConsent::Confirmed
                );
                request.operation_id
            }
            _ => panic!("confirmed delete must submit exactly once"),
        };
        assert_eq!(
            app.model.connection_state(&profile.id),
            &ConnectionState::Closing
        );
        app.confirm_delete_confirmation();
        assert!(service.try_next_command().is_none());

        assert!(service.try_emit(crate::ui::UiEvent::ProfileDeleted {
            operation_id: delete_operation,
            profile_id: profile.id.clone(),
            profile_generation: ProfileGeneration(profile.generation.0 + 1),
            server_state_unknown: true,
        }));
        app.poll_events();
        assert_eq!(
            app.model.status,
            "Profile deleted; server state is unknown."
        );
    }

    #[test]
    fn uncommitted_delete_failure_restores_the_exact_prior_active_operation() {
        let (ui_port, mut service) = bounded_ports(4);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let profile = profile(DriverKind::MySql, DriverAvailability::Ready);
        app.model.profiles = vec![profile.clone()];
        app.model
            .active_generations
            .insert(profile.id.clone(), profile.generation);
        let prior = ActiveOperation {
            operation_id: OperationId(77),
            profile_generation: profile.generation,
            kind: OperationKind::ExecuteMutation,
        };
        app.active_operations.insert(profile.id.clone(), prior);

        app.open_delete_confirmation(&profile);
        app.confirm_delete_confirmation();
        let delete_operation = match service.try_next_command() {
            Some(UiCommand::DeleteProfile(request)) => request.operation_id,
            _ => panic!("confirmed delete command"),
        };
        let error = PublicOperationError::new_or_internal(
            OperationKind::DeleteProfile,
            PublicSummary::ConfigWriteNotCommitted,
            PublicCode::None,
            &SafeContext::profile(profile.id.clone(), delete_operation),
        );
        assert!(service.try_emit(crate::ui::UiEvent::OperationFailed {
            operation_id: delete_operation,
            profile_id: profile.id.clone(),
            profile_generation: profile.generation,
            session_generation: None,
            kind: OperationKind::DeleteProfile,
            summary: error.summary,
            error,
            session_disposition: None,
            connection_outcome: crate::ui::ConnectionFailureOutcome::Preserve,
        }));
        app.poll_events();

        assert_eq!(app.active_operations.get(&profile.id), Some(&prior));
        app.open_delete_confirmation(&profile);
        assert_eq!(
            app.delete_confirmation
                .as_ref()
                .and_then(|confirmation| confirmation.active_kind),
            Some(OperationKind::ExecuteMutation)
        );
    }

    #[test]
    fn durability_unknown_delete_failure_does_not_restore_prior_active_operation() {
        let (ui_port, mut service) = bounded_ports(4);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let profile = profile(DriverKind::MySql, DriverAvailability::Ready);
        app.model.profiles = vec![profile.clone()];
        app.model
            .active_generations
            .insert(profile.id.clone(), profile.generation);
        let prior = ActiveOperation {
            operation_id: OperationId(78),
            profile_generation: profile.generation,
            kind: OperationKind::ExecuteMutation,
        };
        app.active_operations.insert(profile.id.clone(), prior);

        app.open_delete_confirmation(&profile);
        app.confirm_delete_confirmation();
        let delete_operation = match service.try_next_command() {
            Some(UiCommand::DeleteProfile(request)) => request.operation_id,
            _ => panic!("confirmed delete command"),
        };
        let error = PublicOperationError::new_or_internal(
            OperationKind::DeleteProfile,
            PublicSummary::CommittedDurabilityUnknown,
            PublicCode::None,
            &SafeContext::profile(profile.id.clone(), delete_operation),
        );
        assert!(service.try_emit(crate::ui::UiEvent::OperationFailed {
            operation_id: delete_operation,
            profile_id: profile.id.clone(),
            profile_generation: profile.generation,
            session_generation: None,
            kind: OperationKind::DeleteProfile,
            summary: error.summary,
            error,
            session_disposition: None,
            connection_outcome: crate::ui::ConnectionFailureOutcome::Preserve,
        }));
        app.poll_events();

        assert!(!app.active_operations.contains_key(&profile.id));
        assert!(!app.pending_deletes.contains_key(&profile.id));
        app.open_delete_confirmation(&profile);
        assert_eq!(
            app.delete_confirmation
                .as_ref()
                .and_then(|confirmation| confirmation.active_kind),
            None
        );
    }

    #[test]
    fn active_tracking_ignores_stale_uncertainty_but_clears_on_current_uncertainty_and_shutdown() {
        let (ui_port, mut service) = bounded_ports(4);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let profile_id = ProfileId("lifecycle-prune".to_owned());
        let active = ActiveOperation {
            operation_id: OperationId(90),
            profile_generation: ProfileGeneration(1),
            kind: OperationKind::ExecuteRead,
        };
        app.model
            .active_generations
            .insert(profile_id.clone(), ProfileGeneration(2));
        app.active_operations.insert(profile_id.clone(), active);
        app.pending_deletes.insert(
            profile_id.clone(),
            PendingDelete {
                operation_id: OperationId(91),
                profile_generation: ProfileGeneration(1),
                prior_active: Some(active),
                prior_finished: false,
            },
        );
        app.prune_active_operations();
        assert!(app.active_operations.is_empty());
        assert!(app.pending_deletes.is_empty());

        let current = ActiveOperation {
            operation_id: OperationId(92),
            profile_generation: ProfileGeneration(2),
            kind: OperationKind::BrowseMySql,
        };
        app.active_operations.insert(profile_id.clone(), current);
        app.pending_deletes.insert(
            profile_id.clone(),
            PendingDelete {
                operation_id: OperationId(93),
                profile_generation: ProfileGeneration(2),
                prior_active: Some(current),
                prior_finished: false,
            },
        );
        app.finish_active_operation(&crate::ui::UiEvent::ConfigUncertain {
            operation_id: OperationId(94),
        });
        assert!(app.active_operations.is_empty());
        assert!(app.pending_deletes.is_empty());

        let mut refreshed = profile(DriverKind::MySql, DriverAvailability::Ready);
        refreshed.id = profile_id.clone();
        refreshed.persisted.id = profile_id.0.clone();
        refreshed.generation = ProfileGeneration(2);
        app.model.fold(crate::ui::UiEvent::ProfilesLoaded {
            operation_id: OperationId(100),
            profiles: vec![refreshed],
            config: Default::default(),
        });
        app.active_operations.insert(profile_id.clone(), current);
        assert!(service.try_emit(crate::ui::UiEvent::ConfigUncertain {
            operation_id: OperationId(99),
        }));
        app.poll_events();
        assert_eq!(app.active_operations.get(&profile_id), Some(&current));
        assert!(!app.model.is_config_uncertain());

        app.active_operations.insert(profile_id, current);
        app.finish_active_operation(&crate::ui::UiEvent::RuntimeShutdown {
            operation_id: OperationId(95),
        });
        assert!(app.active_operations.is_empty());
    }

    #[test]
    fn failed_save_connect_refresh_clears_follow_up_before_unrelated_reload() {
        let (ui_port, mut service) = bounded_ports(4);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let (_profile_id, refresh_operation) = prime_save_and_connect(&mut app, &mut service);

        let error = reload_failure(refresh_operation);
        assert!(service.try_emit(crate::ui::UiEvent::ProfilesFailed {
            operation_id: refresh_operation,
            summary: error.summary,
            error,
        }));
        app.poll_events();
        assert!(service.try_emit(crate::ui::UiEvent::ProfilesLoaded {
            operation_id: OperationId(refresh_operation.0 + 1),
            profiles: vec![profile(DriverKind::MySql, DriverAvailability::Ready)],
            config: Default::default(),
        }));
        app.poll_events();

        assert!(
            service.try_next_command().is_none(),
            "an unrelated later reload must not silently connect"
        );
    }

    #[test]
    fn busy_save_connect_refresh_submit_does_not_arm_a_later_reload() {
        let (ui_port, mut service) = bounded_ports(1);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());

        let mut editor = ProfileEditor::new(DraftId(402), DriverKind::MySql);
        editor.draft.name = "Profile".to_owned();
        let save_operation = app.model.next_operation();
        assert!(matches!(
            editor.try_save_with_connect(&app.port, save_operation, true),
            crate::ui::profile_form::SaveAttempt::Submitted(_)
        ));
        app.profile_editor = Some(editor);
        assert!(service.try_next_command().is_some());
        assert_eq!(
            app.port.try_submit(UiCommand::RefreshProfiles {
                operation_id: OperationId(999),
            }),
            Ok(())
        );
        assert!(service.try_emit(crate::ui::UiEvent::ProfileSaved {
            operation_id: save_operation,
            profile_id: ProfileId("profile".to_owned()),
            previous_generation: None,
            profile_generation: ProfileGeneration(1),
            session_retained: false,
            warning: None,
        }));
        app.poll_events();
        assert!(matches!(
            service.try_next_command(),
            Some(UiCommand::RefreshProfiles {
                operation_id: OperationId(999)
            })
        ));

        assert!(service.try_emit(crate::ui::UiEvent::ProfilesLoaded {
            operation_id: OperationId(1_000),
            profiles: vec![profile(DriverKind::MySql, DriverAvailability::Ready)],
            config: Default::default(),
        }));
        app.poll_events();
        assert!(
            service.try_next_command().is_none(),
            "a failed follow-up submit must not arm an unrelated later reload"
        );
    }

    #[test]
    fn actual_app_editor_shortcut_submits_exact_generation_and_accesskit_ids() {
        let (ui_port, mut service) = bounded_ports(4);
        let mut app = DbotterApp::new(ui_port);
        assert!(matches!(
            service.try_next_command(),
            Some(UiCommand::RefreshProfiles { .. })
        ));
        let profile = profile(DriverKind::MySql, DriverAvailability::Ready);
        let key = WorkspaceKey::new(profile.id.clone(), profile.generation);
        app.model.profiles = vec![profile];
        app.model.selected_profile = Some(ProfileId("profile".to_owned()));
        app.model
            .active_generations
            .insert(ProfileId("profile".to_owned()), ProfileGeneration(1));
        app.model.workspace_mut(key.clone()).editor_text = "SELECT 1".to_owned();

        #[cfg(target_os = "macos")]
        let modifiers = Modifiers {
            mac_cmd: true,
            command: true,
            ..Modifiers::default()
        };
        #[cfg(not(target_os = "macos"))]
        let modifiers = Modifiers {
            ctrl: true,
            command: true,
            ..Modifiers::default()
        };
        let input = RawInput {
            events: vec![Event::Key {
                key: Key::Enter,
                physical_key: Some(Key::Enter),
                pressed: true,
                repeat: false,
                modifiers,
            }],
            ..RawInput::default()
        };
        let context = Context::default();
        context.enable_accesskit();
        let output = context.run_ui(input, |ui| app.editor_and_results(ui));

        let Some(UiCommand::Execute {
            operation_id,
            profile_id,
            profile_generation,
            text,
            row_limit,
            timeout_ms,
            ..
        }) = service.try_next_command()
        else {
            panic!("actual app shortcut did not submit Execute");
        };
        assert_eq!(operation_id, OperationId(2));
        assert_eq!(profile_id, ProfileId("profile".to_owned()));
        assert_eq!(profile_generation, ProfileGeneration(1));
        assert_eq!(text, "SELECT 1");
        assert_eq!(row_limit, 500);
        assert_eq!(timeout_ms, 30_000);
        assert_eq!(
            app.model
                .workspace(&key)
                .and_then(|workspace| workspace.pending_execute),
            Some(operation_id)
        );

        let ids = output
            .platform_output
            .accesskit_update
            .expect("actual app frame must emit AccessKit")
            .nodes
            .into_iter()
            .filter_map(|(_, node)| node.author_id().map(str::to_owned))
            .collect::<BTreeSet<_>>();
        for expected in [
            "editor.target",
            "editor.input",
            "editor.row_limit",
            "editor.timeout",
            "editor.execute",
        ] {
            assert!(
                ids.contains(expected),
                "missing actual app AX id {expected}"
            );
        }
    }

    #[test]
    fn actual_app_accesskit_confines_sql_and_result_scalar_to_value_nodes() {
        const SQL: &str = "SELECT 'dbotter-sql-value-sentinel'";
        const SCALAR: &str = "dbotter-result-value-sentinel";

        let (ui_port, mut service) = bounded_ports(4);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let profile = profile(DriverKind::MySql, DriverAvailability::Ready);
        let key = WorkspaceKey::new(profile.id.clone(), profile.generation);
        app.model.profiles = vec![profile.clone()];
        app.model.selected_profile = Some(profile.id.clone());
        app.model
            .active_generations
            .insert(profile.id.clone(), profile.generation);
        let workspace = app.model.workspace_mut(key);
        workspace.editor_text = SQL.to_owned();
        workspace.result = Some(Arc::new(result_snapshot(&profile, SCALAR)));

        let context = Context::default();
        context.enable_accesskit();
        let update = context
            .run_ui(RawInput::default(), |ui| app.editor_and_results(ui))
            .platform_output
            .accesskit_update
            .expect("actual editor/results frame must emit AccessKit");

        assert_accesskit_value_confined(&update, "editor.input", SQL);
        assert_accesskit_value_confined(&update, "result.cell.0.0", SCALAR);

        let (_, input) = accesskit_author_node(&update, "editor.input");
        assert_eq!(input.role(), accesskit::Role::MultilineTextInput);
        assert_eq!(input.label(), Some("Statement or command"));
        assert!(input.supports_action(accesskit::Action::Focus));

        let (_, execute) = accesskit_author_node(&update, "editor.execute");
        assert_eq!(execute.author_id(), Some("editor.execute"));
        assert_eq!(execute.label(), Some("Run current or selection"));
        assert_eq!(execute.role(), accesskit::Role::Button);
        assert!(execute.supports_action(accesskit::Action::Focus));
        assert!(execute.supports_action(accesskit::Action::Click));
    }

    #[test]
    fn actual_result_tabs_expose_close_and_select_the_adjacent_output() {
        let (ui_port, mut service) = bounded_ports(4);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let profile = profile(DriverKind::MySql, DriverAvailability::Ready);
        let key = WorkspaceKey::new(profile.id.clone(), profile.generation);
        app.model.profiles = vec![profile.clone()];
        app.model.selected_profile = Some(profile.id.clone());
        app.model
            .active_generations
            .insert(profile.id.clone(), profile.generation);
        let first = app
            .model
            .workspace_mut(key.clone())
            .append_result_tab(Arc::new(result_snapshot(&profile, "first")))
            .expect("first result tab");
        let second = app
            .model
            .workspace_mut(key.clone())
            .append_result_tab(Arc::new(result_snapshot(&profile, "second")))
            .expect("second result tab");

        let context = Context::default();
        context.enable_accesskit();
        let initial = context.run_ui(RawInput::default(), |ui| app.show_result_surface(ui));
        let initial_update = initial
            .platform_output
            .accesskit_update
            .expect("actual result tabs must emit AccessKit");
        let (close_id, close) = accesskit_author_node(
            &initial_update,
            &format!("result.output.close.{}", second.0),
        );
        assert_eq!(close.role(), accesskit::Role::Button);
        assert_eq!(close.label(), Some("Close result tab"));
        assert!(close.supports_action(accesskit::Action::Focus));
        assert!(close.supports_action(accesskit::Action::Click));

        let _ = context.run_ui(
            RawInput {
                events: vec![Event::AccessKitActionRequest(accesskit::ActionRequest {
                    action: accesskit::Action::Focus,
                    target_tree: accesskit::TreeId::ROOT,
                    target_node: close_id,
                    data: None,
                })],
                ..RawInput::default()
            },
            |ui| app.show_result_surface(ui),
        );
        let _ = context.run_ui(
            RawInput {
                events: vec![Event::Key {
                    key: Key::Enter,
                    physical_key: Some(Key::Enter),
                    pressed: true,
                    repeat: false,
                    modifiers: Modifiers::NONE,
                }],
                ..RawInput::default()
            },
            |ui| app.show_result_surface(ui),
        );

        let workspace = app.model.workspace(&key).expect("workspace retained");
        assert_eq!(workspace.result_tabs().len(), 1);
        assert_eq!(workspace.selected_result_tab_id(), Some(first));
        assert_eq!(
            workspace
                .result
                .as_ref()
                .and_then(|result| result.rows.first())
                .and_then(|row| row.first()),
            Some(&Cell::Text("first".to_owned()))
        );
        assert!(service.try_next_command().is_none());
    }

    #[test]
    fn actual_status_strip_reports_and_clears_selected_result_metrics() {
        let (ui_port, mut service) = bounded_ports(4);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let profile = profile(DriverKind::MySql, DriverAvailability::Ready);
        let key = WorkspaceKey::new(profile.id.clone(), profile.generation);
        app.model.profiles = vec![profile.clone()];
        app.model.selected_profile = Some(profile.id.clone());
        app.model
            .active_generations
            .insert(profile.id.clone(), profile.generation);
        let result_tab = app
            .model
            .workspace_mut(key.clone())
            .append_result_tab(Arc::new(result_snapshot(&profile, "visible")))
            .expect("result tab");

        let context = Context::default();
        context.enable_accesskit();
        let with_result =
            context.run_ui(RawInput::default(), |ui| app.show_status_strip(ui, false));
        let with_result = with_result
            .platform_output
            .accesskit_update
            .expect("status strip must emit AccessKit");
        let (_, result_status) = accesskit_author_node(&with_result, "status.result");
        assert_eq!(result_status.label(), Some("Selected result summary"));
        assert_eq!(
            result_status.value(),
            Some("4 ms · 1 returned · 0 affected · Complete")
        );

        app.model
            .workspace_mut(key)
            .close_result_tab(result_tab)
            .expect("last result closes");
        let without_result =
            context.run_ui(RawInput::default(), |ui| app.show_status_strip(ui, false));
        let without_result = without_result
            .platform_output
            .accesskit_update
            .expect("empty status strip must emit AccessKit");
        let (_, result_status) = accesskit_author_node(&without_result, "status.result");
        assert_eq!(result_status.value(), Some("None"));
        assert!(service.try_next_command().is_none());
    }

    #[test]
    fn result_export_submission_owns_pending_state_and_commits_only_the_correlated_path() {
        let directory = tempfile::tempdir().expect("tempdir");
        let destination = directory.path().join("result.json");
        let (ui_port, mut service) = bounded_ports(4);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let profile = profile(DriverKind::MySql, DriverAvailability::Ready);
        let key = WorkspaceKey::new(profile.id.clone(), profile.generation);
        app.model.profiles = vec![profile.clone()];
        app.model.selected_profile = Some(profile.id.clone());
        app.model
            .active_generations
            .insert(profile.id.clone(), profile.generation);
        let result = Arc::new(result_snapshot(&profile, "exported"));
        app.model.workspace_mut(key).result = Some(result.clone());

        app.submit_result_export_to(result.clone(), ExportFormat::Json, destination.clone());
        let command = service.try_next_command().expect("export command");
        let UiCommand::ExportResult {
            request,
            confirmation,
        } = command
        else {
            panic!("expected export command");
        };
        assert_eq!(request.result_id, result.provenance.result_id);
        assert_eq!(request.format, ExportFormat::Json);
        assert_eq!(request.overwrite_policy, OverwritePolicy::DenyOverwrite);
        assert!(confirmation.is_none());
        assert_eq!(request.destination, destination);
        assert!(
            app.pending_export_destinations
                .contains_key(&request.operation_id)
        );

        assert!(service.try_emit(UiEvent::ResultExported {
            operation_id: request.operation_id,
            result_id: request.result_id,
            format: request.format,
            overwrite_policy: request.overwrite_policy,
            row_count: 1,
            bytes_written: 12,
        }));
        app.poll_events();
        assert!(app.pending_export_destinations.is_empty());
        assert_eq!(
            app.committed_export_destinations.get(&request.result_id),
            Some(&destination)
        );
    }

    #[test]
    fn actual_redis_accesskit_confines_key_display_to_exact_action_node() {
        const KEY: &[u8] = b"dbotter-redis-key-value-sentinel";
        const KEY_DISPLAY: &str = "dbotter-redis-key-value-sentinel";

        let (ui_port, mut service) = bounded_ports(8);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let profile = redis_profile("redis-disclosure", 3);
        let key = WorkspaceKey::new(profile.id.clone(), profile.generation);
        app.model.profiles = vec![profile.clone()];
        app.model
            .active_generations
            .insert(profile.id.clone(), profile.generation);
        load_redis_key(&mut app, &mut service, &key, KEY, SessionGeneration(31));

        let context = Context::default();
        context.enable_accesskit();
        let update = context
            .run_ui(RawInput::default(), |ui| app.explorer_contents(ui))
            .platform_output
            .accesskit_update
            .expect("actual Redis explorer frame must emit AccessKit");

        assert_accesskit_value_confined(&update, "redis.key.0", KEY_DISPLAY);
        let (_, key_node) = accesskit_author_node(&update, "redis.key.0");
        assert_eq!(key_node.author_id(), Some("redis.key.0"));
        assert_eq!(key_node.label(), Some("Redis key 1"));
        assert_eq!(key_node.role(), accesskit::Role::Button);
        assert!(key_node.supports_action(accesskit::Action::Focus));
        assert!(key_node.supports_action(accesskit::Action::Click));
    }

    #[test]
    fn keyboard_tabs_to_execute_and_activates_the_actual_control() {
        let (ui_port, mut service) = bounded_ports(4);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let profile = profile(DriverKind::MySql, DriverAvailability::Ready);
        let key = WorkspaceKey::new(profile.id.clone(), profile.generation);
        app.model.profiles = vec![profile.clone()];
        app.model.selected_profile = Some(profile.id.clone());
        app.model
            .active_generations
            .insert(profile.id.clone(), profile.generation);
        app.model.workspace_mut(key).editor_text = "SELECT 1".to_owned();

        let context = Context::default();
        context.enable_accesskit();
        let mut author_ids = HashMap::new();
        let mut focused_execute = false;
        for frame in 0..16 {
            let events = if frame > 0 {
                vec![
                    Event::Key {
                        key: Key::Tab,
                        physical_key: Some(Key::Tab),
                        pressed: true,
                        repeat: false,
                        modifiers: Modifiers::default(),
                    },
                    Event::Key {
                        key: Key::Tab,
                        physical_key: Some(Key::Tab),
                        pressed: false,
                        repeat: false,
                        modifiers: Modifiers::default(),
                    },
                ]
            } else {
                Vec::new()
            };
            let output = context.run_ui(
                RawInput {
                    events,
                    ..RawInput::default()
                },
                |ui| app.editor_and_results(ui),
            );
            let update = output
                .platform_output
                .accesskit_update
                .expect("keyboard frame must emit AccessKit");
            for (node_id, node) in &update.nodes {
                if let Some(author_id) = node.author_id() {
                    author_ids.insert(*node_id, author_id.to_owned());
                }
            }
            if author_ids.get(&update.focus).map(String::as_str) == Some("editor.execute") {
                focused_execute = true;
                break;
            }
        }
        assert!(
            focused_execute,
            "Tab must reach the actual editor.execute control"
        );

        let settled = context.run_ui(RawInput::default(), |ui| app.editor_and_results(ui));
        let settled_update = settled
            .platform_output
            .accesskit_update
            .expect("settled keyboard frame must emit AccessKit");
        for (node_id, node) in &settled_update.nodes {
            if let Some(author_id) = node.author_id() {
                author_ids.insert(*node_id, author_id.to_owned());
            }
        }
        assert_eq!(
            author_ids.get(&settled_update.focus).map(String::as_str),
            Some("editor.execute"),
            "keyboard focus readback must settle on the exact action id"
        );
        assert_eq!(
            context.memory(|memory| memory.focused().map(|id| id.accesskit_id())),
            Some(settled_update.focus),
            "AccessKit focus readback must match egui keyboard focus"
        );

        let _ = context.run_ui(
            RawInput {
                events: vec![
                    Event::Key {
                        key: Key::Space,
                        physical_key: Some(Key::Space),
                        pressed: true,
                        repeat: false,
                        modifiers: Modifiers::default(),
                    },
                    Event::Key {
                        key: Key::Space,
                        physical_key: Some(Key::Space),
                        pressed: false,
                        repeat: false,
                        modifiers: Modifiers::default(),
                    },
                ],
                ..RawInput::default()
            },
            |ui| app.editor_and_results(ui),
        );
        let command = service.try_next_command();
        assert!(
            matches!(&command, Some(UiCommand::Execute { text, .. }) if text == "SELECT 1"),
            "Space on the focused Execute control must submit the exact editor value, got {command:?}"
        );
    }

    #[test]
    fn editor_submission_sets_pending_only_after_success_and_cancel_is_exact() {
        let (ui_port, mut service) = bounded_ports(1);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let profile = profile(DriverKind::MySql, DriverAvailability::Ready);
        let key = WorkspaceKey::new(profile.id.clone(), profile.generation);
        app.model.profiles = vec![profile.clone()];
        app.model.selected_profile = Some(profile.id.clone());
        app.model
            .active_generations
            .insert(profile.id.clone(), profile.generation);
        app.model.workspace_mut(key.clone()).editor_text = "SELECT 1".to_owned();
        let intent = build_execute_intent(
            &profile,
            app.model.workspace(&key).expect("workspace"),
            EditorCursor::caret(0),
        )
        .expect("execute intent");

        app.port
            .try_submit(UiCommand::TestConnection {
                operation_id: OperationId(99),
                profile_id: profile.id.clone(),
                profile_generation: profile.generation,
                timeout_ms: 1_000,
            })
            .expect("fill work lane");
        app.submit_editor_intent(EditorIntent::Execute(intent.clone()));
        assert!(
            app.model
                .workspace(&key)
                .is_some_and(|workspace| workspace.pending_execute.is_none()),
            "a Busy submit must not fabricate pending state"
        );
        assert!(matches!(
            service.try_next_command(),
            Some(UiCommand::TestConnection {
                operation_id: OperationId(99),
                ..
            })
        ));

        app.submit_editor_intent(EditorIntent::Execute(intent));
        let Some(UiCommand::Execute { operation_id, .. }) = service.try_next_command() else {
            panic!("expected exact Execute after capacity became available");
        };
        assert_eq!(
            app.model
                .workspace(&key)
                .and_then(|workspace| workspace.pending_execute),
            Some(operation_id)
        );

        app.submit_editor_intent(EditorIntent::Cancel { operation_id });
        assert!(matches!(
            service.try_next_command(),
            Some(UiCommand::CancelOperation {
                operation_id: cancelled,
            }) if cancelled == operation_id
        ));
        assert_eq!(
            app.model
                .workspace(&key)
                .and_then(|workspace| workspace.pending_execute),
            Some(operation_id),
            "Cancel submission waits for the correlated terminal event"
        );
    }

    #[test]
    fn run_all_submits_one_batch_and_retains_every_successful_result() {
        let (ui_port, mut service) = bounded_ports(8);
        let mut app = DbotterApp::new(ui_port);
        assert!(matches!(
            service.try_next_command(),
            Some(UiCommand::RefreshProfiles { .. })
        ));
        let profile = profile(DriverKind::MySql, DriverAvailability::Ready);
        let key = WorkspaceKey::new(profile.id.clone(), profile.generation);
        app.model.profiles = vec![profile.clone()];
        app.model.selected_profile = Some(profile.id.clone());
        app.model
            .active_generations
            .insert(profile.id.clone(), profile.generation);
        let origin_editor_tab_id = app
            .model
            .workspace_mut(key.clone())
            .create_editor_tab(QueryLanguage::Sql, "Batch source", "SELECT 1;\nSELECT 2;")
            .expect("batch source tab");
        let batch =
            build_execute_all_intent(&profile, app.model.workspace(&key).expect("workspace"))
                .expect("read-only batch");

        app.submit_editor_intent(EditorIntent::ExecuteAll(batch));
        let (operation_id, command_editor_tab_id, source) = match service.try_next_command() {
            Some(UiCommand::ExecuteBatch {
                operation_id,
                editor_tab_id,
                text,
                ..
            }) => (operation_id, editor_tab_id, text),
            other => panic!("expected one batch command, got {other:?}"),
        };
        assert_eq!(command_editor_tab_id, Some(origin_editor_tab_id));
        assert_eq!(source, "SELECT 1;\nSELECT 2;");
        assert!(service.try_next_command().is_none());
        assert_eq!(app.model.status, "Run all: executing 2 targets…");

        let other_editor_tab_id = app
            .model
            .workspace_mut(key.clone())
            .create_editor_tab(QueryLanguage::Sql, "Other work", "SELECT 3")
            .expect("another tab can be selected while the batch runs");
        assert!(service.try_emit(UiEvent::QueryBatchFinished {
            operation_id,
            profile_id: profile.id.clone(),
            profile_generation: profile.generation,
            editor_tab_id: Some(origin_editor_tab_id),
            session_generation: SessionGeneration(7),
            target_count: 2,
            completed_targets: 2,
            discarded_results: 0,
            results: vec![
                result_snapshot_for_operation(&profile, "first", operation_id, ResultId(101)),
                result_snapshot_for_operation(&profile, "second", operation_id, ResultId(102)),
            ],
            error: None,
            session_disposition: SessionDisposition::Keep,
        }));
        app.poll_events();

        assert!(service.try_next_command().is_none());
        assert_eq!(app.model.status, "Run all finished: 2/2 targets in 8 ms.");
        {
            let workspace = app.model.workspace(&key).expect("workspace retained");
            assert_eq!(workspace.result_tabs().len(), 2);
            assert_eq!(
                workspace
                    .result_tabs()
                    .iter()
                    .map(|tab| tab.title())
                    .collect::<Vec<_>>(),
                ["Result 101", "Result 102"],
                "one batch operation still needs distinct result-tab labels"
            );
            assert!(
                workspace
                    .result_tabs()
                    .iter()
                    .all(|tab| tab.origin_editor_tab_id() == Some(origin_editor_tab_id))
            );
            assert_eq!(
                workspace
                    .result_tabs_for_editor(Some(other_editor_tab_id))
                    .count(),
                0,
                "the selected editor must not inherit another editor's results"
            );
            assert_eq!(
                workspace
                    .result_tabs_for_editor(Some(origin_editor_tab_id))
                    .count(),
                2
            );
            assert_eq!(
                workspace.selected_editor_tab_id(),
                Some(other_editor_tab_id),
                "a completion must not move the user away from their current editor"
            );
            assert_eq!(workspace.selected_result_tab_id(), None);
            assert!(workspace.result.is_none());
            assert!(workspace.pending_execute.is_none());
        }

        let context = Context::default();
        context.enable_accesskit();
        let other_editor_frame = context
            .run_ui(RawInput::default(), |ui| app.show_result_surface(ui))
            .platform_output
            .accesskit_update
            .expect("other editor result surface must emit AccessKit");
        assert!(
            other_editor_frame.nodes.iter().all(|(_, node)| node
                .author_id()
                .is_none_or(|id| !id.starts_with("result.output."))),
            "another editor's result tabs must not render in the selected editor"
        );
        assert!(
            other_editor_frame.nodes.iter().any(|(_, node)| {
                node.label() == Some("No result yet") || node.value() == Some("No result yet")
            }),
            "the selected editor without results must render its empty state"
        );

        app.model
            .workspace_mut(key.clone())
            .select_editor_tab(origin_editor_tab_id)
            .expect("return to the batch source tab");
        let expected_result_tab_ids = {
            let workspace = app.model.workspace(&key).expect("workspace retained");
            assert_eq!(
                workspace.selected_result_tab_id(),
                workspace
                    .result_tabs_for_editor(Some(origin_editor_tab_id))
                    .next_back()
                    .map(|tab| tab.id())
            );
            assert_eq!(
                workspace
                    .selected_result_tab()
                    .map(|tab| tab.snapshot().provenance.result_id),
                Some(ResultId(102)),
                "returning to the origin editor must activate its newest result"
            );
            assert_eq!(
                workspace
                    .result
                    .as_ref()
                    .map(|result| result.provenance.result_id),
                Some(ResultId(102))
            );
            workspace
                .result_tabs_for_editor(Some(origin_editor_tab_id))
                .map(|tab| format!("result.output.{}", tab.id().0))
                .collect::<Vec<_>>()
        };
        let origin_editor_frame = context
            .run_ui(RawInput::default(), |ui| app.show_result_surface(ui))
            .platform_output
            .accesskit_update
            .expect("origin editor result surface must emit AccessKit");
        let origin_editor_ids = origin_editor_frame
            .nodes
            .iter()
            .filter_map(|(_, node)| node.author_id())
            .collect::<BTreeSet<_>>();
        assert!(
            expected_result_tab_ids
                .iter()
                .all(|expected| origin_editor_ids.contains(expected.as_str())),
            "returning to the origin editor must render all of its result tabs"
        );
    }

    #[test]
    fn run_all_partial_failure_retains_origin_result_and_keeps_connected_session() {
        let (ui_port, mut service) = bounded_ports(8);
        let mut app = DbotterApp::new(ui_port);
        assert!(matches!(
            service.try_next_command(),
            Some(UiCommand::RefreshProfiles { .. })
        ));
        let profile = profile(DriverKind::MySql, DriverAvailability::Ready);
        let key = WorkspaceKey::new(profile.id.clone(), profile.generation);
        app.model.profiles = vec![profile.clone()];
        app.model.selected_profile = Some(profile.id.clone());
        app.model
            .active_generations
            .insert(profile.id.clone(), profile.generation);
        app.model.connection_states.insert(
            profile.id.clone(),
            ConnectionState::Connected {
                session_generation: SessionGeneration(9),
                elapsed_ms: 12,
            },
        );
        let origin_editor_tab_id = app
            .model
            .workspace_mut(key.clone())
            .create_editor_tab(
                QueryLanguage::Sql,
                "Partial batch",
                "SELECT 1;\nSELECT broken;\nSELECT 3;",
            )
            .expect("origin editor tab");
        let batch =
            build_execute_all_intent(&profile, app.model.workspace(&key).expect("workspace"))
                .expect("three-target batch");

        app.submit_editor_intent(EditorIntent::ExecuteAll(batch));
        let operation_id = match service.try_next_command() {
            Some(UiCommand::ExecuteBatch {
                operation_id,
                editor_tab_id,
                text,
                ..
            }) => {
                assert_eq!(editor_tab_id, Some(origin_editor_tab_id));
                assert_eq!(text, "SELECT 1;\nSELECT broken;\nSELECT 3;");
                operation_id
            }
            other => panic!("expected one batch command, got {other:?}"),
        };
        assert!(service.try_next_command().is_none());

        let error = PublicOperationError::new(
            OperationKind::ExecuteRead,
            PublicSummary::SyntaxRejected,
            PublicCode::StatementTarget,
            &SafeContext::profile(profile.id.clone(), operation_id),
        )
        .expect("typed public execution error");
        let terminal = UiEvent::QueryBatchFinished {
            operation_id,
            profile_id: profile.id.clone(),
            profile_generation: profile.generation,
            editor_tab_id: Some(origin_editor_tab_id),
            session_generation: SessionGeneration(9),
            target_count: 3,
            completed_targets: 1,
            discarded_results: 0,
            results: vec![result_snapshot_for_operation(
                &profile,
                "retained",
                operation_id,
                ResultId(201),
            )],
            error: Some(error),
            session_disposition: SessionDisposition::Keep,
        };
        assert!(service.try_emit(terminal.clone()));
        app.poll_events();

        let expected_status = "Run all stopped after 1/3 targets: The server rejected the syntax.";
        let result_tab_id = {
            let workspace = app.model.workspace(&key).expect("workspace retained");
            assert!(workspace.pending_execute.is_none());
            assert_eq!(workspace.result_tabs().len(), 1);
            assert_eq!(
                workspace
                    .result_tabs_for_editor(Some(origin_editor_tab_id))
                    .count(),
                1
            );
            let result_tab = workspace.result_tabs().first().expect("partial result tab");
            assert_eq!(
                result_tab.origin_editor_tab_id(),
                Some(origin_editor_tab_id)
            );
            assert_eq!(workspace.selected_result_tab_id(), Some(result_tab.id()));
            assert_eq!(
                workspace
                    .result
                    .as_ref()
                    .map(|result| result.provenance.result_id),
                Some(ResultId(201))
            );
            assert_eq!(
                workspace.error.as_ref().map(|error| error.summary),
                Some(PublicSummary::SyntaxRejected)
            );
            result_tab.id()
        };
        assert!(!app.active_operations.contains_key(&profile.id));
        assert_eq!(app.model.status, expected_status);
        assert_eq!(
            app.model.connection_state(&profile.id),
            &ConnectionState::Connected {
                session_generation: SessionGeneration(9),
                elapsed_ms: 0,
            }
        );
        assert!(service.try_next_command().is_none());

        let context = Context::default();
        context.enable_accesskit();
        let frame = context
            .run_ui(RawInput::default(), |ui| {
                app.show_result_surface(ui);
                app.show_status_strip(ui, false);
            })
            .platform_output
            .accesskit_update
            .expect("partial batch frame must emit AccessKit");
        let (_, retained_tab) =
            accesskit_author_node(&frame, &format!("result.output.{}", result_tab_id.0));
        assert_eq!(retained_tab.label(), Some("Execution result tab"));
        let (_, operation_status) = accesskit_author_node(&frame, "status.operation");
        assert_eq!(operation_status.value(), Some(expected_status));

        assert!(service.try_emit(terminal));
        app.poll_events();
        let workspace = app.model.workspace(&key).expect("workspace retained");
        assert!(workspace.pending_execute.is_none());
        assert_eq!(workspace.result_tabs().len(), 1);
        assert_eq!(workspace.selected_result_tab_id(), Some(result_tab_id));
        assert_eq!(app.model.status, expected_status);
        assert_eq!(
            app.model.connection_state(&profile.id),
            &ConnectionState::Connected {
                session_generation: SessionGeneration(9),
                elapsed_ms: 0,
            }
        );
        assert!(service.try_next_command().is_none());
    }

    #[test]
    fn run_all_failure_terminates_the_single_correlated_operation() {
        let (ui_port, mut service) = bounded_ports(8);
        let mut app = DbotterApp::new(ui_port);
        assert!(service.try_next_command().is_some());
        let profile = profile(DriverKind::MySql, DriverAvailability::Ready);
        let key = WorkspaceKey::new(profile.id.clone(), profile.generation);
        app.model.profiles = vec![profile.clone()];
        app.model.selected_profile = Some(profile.id.clone());
        app.model
            .active_generations
            .insert(profile.id.clone(), profile.generation);
        app.model.workspace_mut(key.clone()).editor_text =
            "SELECT 1;\nSELECT 2;\nSELECT 3;".to_owned();
        let batch =
            build_execute_all_intent(&profile, app.model.workspace(&key).expect("workspace"))
                .expect("read-only batch");
        app.submit_editor_intent(EditorIntent::ExecuteAll(batch));
        let operation_id = match service.try_next_command() {
            Some(UiCommand::ExecuteBatch {
                operation_id, text, ..
            }) => {
                assert_eq!(text, "SELECT 1;\nSELECT 2;\nSELECT 3;");
                operation_id
            }
            other => panic!("expected one batch command, got {other:?}"),
        };
        let error = PublicOperationError::new_or_internal(
            OperationKind::ExecuteRead,
            PublicSummary::OperationCancelled,
            PublicCode::None,
            &SafeContext::profile(profile.id.clone(), operation_id),
        );
        assert!(service.try_emit(UiEvent::OperationFailed {
            operation_id,
            profile_id: profile.id.clone(),
            profile_generation: profile.generation,
            session_generation: Some(SessionGeneration(8)),
            kind: OperationKind::ExecuteRead,
            summary: error.summary,
            error,
            session_disposition: None,
            connection_outcome: crate::ui::ConnectionFailureOutcome::Preserve,
        }));
        app.poll_events();

        assert!(service.try_next_command().is_none());
        assert!(!app.active_operations.contains_key(&profile.id));
        assert_eq!(app.model.status, "The operation was cancelled.");
        let workspace = app.model.workspace(&key).expect("workspace retained");
        assert_eq!(workspace.result_tabs().len(), 0);
        assert!(workspace.pending_execute.is_none());
    }

    #[test]
    fn double_execute_while_pending_submits_only_once() {
        let (ui, mut service) = bounded_ports(4);
        let mut app = DbotterApp::new(ui);
        assert!(matches!(
            service.try_next_command(),
            Some(UiCommand::RefreshProfiles { .. })
        ));
        app.model.profiles = vec![profile(DriverKind::MySql, DriverAvailability::Ready)];
        app.model.selected_profile = Some(ProfileId("profile".to_owned()));
        app.model
            .active_generations
            .insert(ProfileId("profile".to_owned()), ProfileGeneration(1));
        app.model
            .workspace_mut(WorkspaceKey::new(
                ProfileId("profile".to_owned()),
                ProfileGeneration(1),
            ))
            .editor_text = "SELECT 1".to_owned();

        let profile = app
            .model
            .selected_profile_snapshot()
            .cloned()
            .expect("selected profile");
        let intent = build_execute_intent(
            &profile,
            app.model
                .workspace(&WorkspaceKey::new(profile.id.clone(), profile.generation))
                .expect("workspace"),
            EditorCursor::caret(0),
        )
        .expect("execute intent");
        app.submit_editor_intent(EditorIntent::Execute(intent.clone()));
        app.submit_editor_intent(EditorIntent::Execute(intent));

        assert!(matches!(
            service.try_next_command(),
            Some(UiCommand::Execute { .. })
        ));
        assert!(service.try_next_command().is_none());
    }

    #[test]
    fn mongodb_planned_profile_submits_no_command() {
        let (ui, mut service) = bounded_ports(4);
        let mut app = DbotterApp::new(ui);
        assert!(service.try_next_command().is_some());
        app.model.profiles = vec![profile(DriverKind::MongoDb, DriverAvailability::Planned)];
        app.model.selected_profile = Some(ProfileId("profile".to_owned()));
        app.model
            .workspace_mut(WorkspaceKey::new(
                ProfileId("profile".to_owned()),
                ProfileGeneration(1),
            ))
            .editor_text = "{}".to_owned();

        let profile = app
            .model
            .selected_profile_snapshot()
            .cloned()
            .expect("selected profile");
        assert!(
            build_execute_intent(
                &profile,
                app.model
                    .workspace(&WorkspaceKey::new(profile.id.clone(), profile.generation,))
                    .expect("workspace"),
                EditorCursor::caret(0),
            )
            .is_err()
        );

        assert!(service.try_next_command().is_none());
    }

    #[test]
    fn config_uncertain_submits_neither_profile_network_work_nor_execute() {
        let (ui, mut service) = bounded_ports(4);
        let mut app = DbotterApp::new(ui);
        assert!(service.try_next_command().is_some());
        app.model.profiles = vec![profile(DriverKind::MySql, DriverAvailability::Ready)];
        app.model.selected_profile = Some(ProfileId("profile".to_owned()));
        app.model
            .active_generations
            .insert(ProfileId("profile".to_owned()), ProfileGeneration(1));
        app.model
            .workspace_mut(WorkspaceKey::new(
                ProfileId("profile".to_owned()),
                ProfileGeneration(1),
            ))
            .editor_text = "SELECT 1".to_owned();
        let profile = app
            .model
            .selected_profile_snapshot()
            .cloned()
            .expect("selected profile");
        let intent = build_execute_intent(
            &profile,
            app.model
                .workspace(&WorkspaceKey::new(profile.id.clone(), profile.generation))
                .expect("workspace"),
            EditorCursor::caret(0),
        )
        .expect("execute intent");
        app.model.fold(crate::ui::UiEvent::ConfigUncertain {
            operation_id: crate::model::OperationId(10),
        });

        app.submit_test(ProfileId("profile".to_owned()));
        app.submit_editor_intent(EditorIntent::Execute(intent));

        assert!(
            service.try_next_command().is_none(),
            "configuration uncertainty must block test and execute at the UI boundary"
        );
    }

    #[test]
    fn draft_ids_are_owned_by_the_app_and_monotonic() {
        let (ui, mut service) = bounded_ports(1);
        let mut app = DbotterApp::new(ui);
        assert!(service.try_next_command().is_some());

        assert_eq!(app.allocate_draft_id(), DraftId(1));
        assert_eq!(app.allocate_draft_id(), DraftId(2));
        assert_eq!(app.allocate_draft_id(), DraftId(3));
    }

    #[test]
    fn redis_scan_intent_submits_exact_profile_generation_filter_and_cursor() {
        let (ui, mut service) = bounded_ports(4);
        let mut app = DbotterApp::new(ui);
        assert!(service.try_next_command().is_some());
        app.model.profiles = vec![profile(DriverKind::Redis, DriverAvailability::Ready)];
        app.model.selected_profile = Some(ProfileId("profile".to_owned()));
        app.model
            .active_generations
            .insert(ProfileId("profile".to_owned()), ProfileGeneration(1));
        render_redis_explorer(&mut app);

        app.submit_redis_intent(RedisExplorerIntent::Scan {
            filter: RedisKeyFilter::LiteralPrefix("orders:[".to_owned()),
            cursor: 41,
            restart: false,
        });

        let Some(UiCommand::ScanRedisKeys(request)) = service.try_next_command() else {
            panic!("expected one Redis SCAN command");
        };
        assert_eq!(request.identity.profile_id, ProfileId("profile".to_owned()));
        assert_eq!(request.identity.profile_generation, ProfileGeneration(1));
        assert_eq!(
            request.filter,
            RedisKeyFilter::LiteralPrefix("orders:[".to_owned())
        );
        assert_eq!(request.cursor, 41);
        assert_eq!(request.count_hint, crate::model::DEFAULT_REDIS_SCAN_COUNT);
        assert!(service.try_next_command().is_none());
    }

    #[test]
    fn redis_tls_recovery_focuses_only_the_typed_field_and_preserves_the_same_ca() {
        let (ui, mut service) = bounded_ports(4);
        let mut app = DbotterApp::new(ui);
        assert!(service.try_next_command().is_some());
        let mut redis = profile(DriverKind::Redis, DriverAvailability::Ready);
        redis.persisted.tls = TlsMode::Required;
        redis.persisted.redis_tls.ca_file = Some("/tmp/same-ca.pem".into());
        app.model.profiles = vec![redis];

        let host_operation = OperationId(41);
        let host_error = PublicOperationError::new_or_internal(
            OperationKind::BrowseRedis,
            PublicSummary::TlsVerificationFailed,
            PublicCode::TlsHostnameMismatch,
            &SafeContext::profile(ProfileId("profile".to_owned()), host_operation),
        );
        let host_action =
            RecoveryAction::EditProfile(ProfileId("profile".to_owned()), ProfileFieldId::Host);
        app.dispatch_error_recovery(host_operation, &host_error, host_action);
        let host_editor = app.profile_editor.as_ref().expect("host editor");
        assert_eq!(host_editor.requested_focus(), Some(ProfileFieldId::Host));
        assert_eq!(host_editor.draft.redis_ca_file, "/tmp/same-ca.pem");

        app.profile_editor = None;
        let ca_operation = OperationId(42);
        let ca_error = PublicOperationError::new_or_internal(
            OperationKind::BrowseRedis,
            PublicSummary::TlsVerificationFailed,
            PublicCode::RedisTlsCaUntrustedIssuer,
            &SafeContext::profile(ProfileId("profile".to_owned()), ca_operation),
        );
        let ca_action = RecoveryAction::EditProfile(
            ProfileId("profile".to_owned()),
            ProfileFieldId::RedisCaFile,
        );
        app.dispatch_error_recovery(ca_operation, &ca_error, ca_action);
        let ca_editor = app.profile_editor.as_ref().expect("CA editor");
        assert_eq!(
            ca_editor.requested_focus(),
            Some(ProfileFieldId::RedisCaFile)
        );
        assert_eq!(ca_editor.draft.redis_ca_file, "/tmp/same-ca.pem");
        assert!(service.try_next_command().is_none());
    }

    #[test]
    fn redis_inspect_and_cancel_commands_preserve_raw_identity_and_operation_id() {
        let (ui, mut service) = bounded_ports(4);
        let mut app = DbotterApp::new(ui);
        assert!(service.try_next_command().is_some());
        app.model.profiles = vec![profile(DriverKind::Redis, DriverAvailability::Ready)];
        app.model.selected_profile = Some(ProfileId("profile".to_owned()));
        app.model
            .active_generations
            .insert(ProfileId("profile".to_owned()), ProfileGeneration(1));
        let raw_key = RedisKeyId(vec![b'b', 0, 0xff, b'k']);
        render_redis_explorer(&mut app);

        app.submit_redis_intent(RedisExplorerIntent::Inspect {
            key: raw_key.clone(),
        });
        let Some(UiCommand::InspectRedisKey(request)) = service.try_next_command() else {
            panic!("expected one Redis inspect command");
        };
        assert_eq!(request.key, raw_key);
        let operation_id = request.identity.operation_id;

        app.submit_redis_intent(RedisExplorerIntent::Cancel { operation_id });
        assert!(matches!(
            service.try_next_command(),
            Some(UiCommand::CancelOperation {
                operation_id: submitted
            }) if submitted == operation_id
        ));
        assert!(service.try_next_command().is_none());
    }

    #[test]
    fn redis_explorer_state_is_isolated_by_exact_workspace_key() {
        let (ui, mut service) = bounded_ports(8);
        let mut app = DbotterApp::new(ui);
        assert!(service.try_next_command().is_some());
        let (_, _, alpha_key, beta_key) = seed_two_redis_workspaces(&mut app, &mut service);

        assert_eq!(
            redis_keys_for(&app, &alpha_key),
            Some(vec![b"alpha:key".to_vec()])
        );
        assert_eq!(
            redis_keys_for(&app, &beta_key),
            Some(vec![b"beta:key".to_vec()]),
            "switching profiles must not discard another exact workspace"
        );
    }

    #[test]
    fn redis_intent_never_falls_back_to_a_changed_global_selection() {
        let (ui, mut service) = bounded_ports(4);
        let mut app = DbotterApp::new(ui);
        assert!(service.try_next_command().is_some());
        let alpha = redis_profile("redis-alpha", 1);
        let beta = redis_profile("redis-beta", 1);
        app.model.profiles = vec![alpha.clone(), beta.clone()];
        app.model
            .active_generations
            .insert(alpha.id.clone(), alpha.generation);
        app.model
            .active_generations
            .insert(beta.id.clone(), beta.generation);

        app.model.selected_profile = Some(alpha.id.clone());
        render_redis_explorer(&mut app);
        app.model.selected_profile = Some(beta.id.clone());
        app.submit_redis_intent(RedisExplorerIntent::Scan {
            filter: RedisKeyFilter::Glob("orders:*".to_owned()),
            cursor: 0,
            restart: true,
        });

        assert!(
            service.try_next_command().is_none(),
            "an intent bound to alpha must never be silently retargeted to selected beta"
        );
    }

    #[test]
    fn mismatched_redis_profile_generation_event_mutates_no_ui_state() {
        let (ui, mut service) = bounded_ports(4);
        let mut app = DbotterApp::new(ui);
        assert!(service.try_next_command().is_some());
        let profile = redis_profile("redis-current", 2);
        let key = WorkspaceKey::new(profile.id.clone(), profile.generation);
        app.model.profiles = vec![profile.clone()];
        app.model
            .active_generations
            .insert(profile.id.clone(), profile.generation);
        app.model.connection_states.insert(
            profile.id.clone(),
            ConnectionState::Connected {
                session_generation: SessionGeneration(9),
                elapsed_ms: 0,
            },
        );
        app.model.selected_profile = Some(profile.id.clone());
        render_redis_explorer(&mut app);
        app.submit_redis_intent(RedisExplorerIntent::Scan {
            filter: RedisKeyFilter::Glob("*".to_owned()),
            cursor: 0,
            restart: true,
        });
        let request = match service.try_next_command() {
            Some(UiCommand::ScanRedisKeys(request)) => request,
            _ => panic!("Redis scan command"),
        };
        let mut stale_page = redis_page(&request, b"stale-generation");
        stale_page.identity.profile_generation = ProfileGeneration(1);
        assert!(service.try_emit(crate::ui::UiEvent::RedisKeysLoaded {
            page: stale_page,
            session_generation: SessionGeneration(9),
            session_disposition: SessionDisposition::Keep,
        }));
        app.poll_events();

        assert!(
            app.active_operations
                .get(&profile.id)
                .is_some_and(|active| {
                    active.operation_id == request.operation_id()
                        && active.profile_generation == profile.generation
                })
        );
        assert_eq!(redis_keys_for(&app, &key), Some(Vec::new()));
        assert!(
            app.model
                .workspace(&key)
                .is_none_or(|workspace| workspace.redis_key_page.is_none())
        );
    }

    #[test]
    fn app_redis_session_correlation_matrix_is_fail_closed() {
        let (ui, mut service) = bounded_ports(2);
        let mut app = DbotterApp::new(ui);
        assert!(service.try_next_command().is_some());
        let profile = redis_profile("redis-matrix", 1);
        app.model.profiles = vec![profile.clone()];
        app.model
            .active_generations
            .insert(profile.id.clone(), profile.generation);
        let request = RedisScanRequest {
            identity: RequestIdentity::new(profile.id.clone(), profile.generation, OperationId(77)),
            filter: RedisKeyFilter::Glob("*".to_owned()),
            cursor: 0,
            count_hint: 100,
            timeout: Duration::from_secs(5),
        };
        app.active_operations.insert(
            profile.id.clone(),
            ActiveOperation {
                operation_id: request.operation_id(),
                profile_generation: profile.generation,
                kind: OperationKind::BrowseRedis,
            },
        );
        let loaded = |session_generation| crate::ui::UiEvent::RedisKeysLoaded {
            page: redis_page(&request, b"matrix"),
            session_generation,
            session_disposition: SessionDisposition::Keep,
        };
        let failed = || crate::ui::UiEvent::RedisKeysFailed {
            request: request.clone(),
            error: PublicOperationError::new_or_internal(
                OperationKind::BrowseRedis,
                PublicSummary::ResourceStale,
                PublicCode::None,
                &SafeContext::profile(profile.id.clone(), request.operation_id()),
            ),
            session_generation: None,
            session_disposition: None,
            connection_outcome: crate::ui::ConnectionFailureOutcome::Preserve,
        };

        app.model.connection_states.insert(
            profile.id.clone(),
            ConnectionState::Connected {
                session_generation: SessionGeneration(9),
                elapsed_ms: 0,
            },
        );
        assert_eq!(
            app.redis_resource_event_disposition(&loaded(SessionGeneration(9))),
            super::RedisResourceEventDisposition::Apply
        );
        assert_eq!(
            app.redis_resource_event_disposition(&loaded(SessionGeneration(8))),
            super::RedisResourceEventDisposition::StaleTerminal(request.operation_id())
        );
        assert_eq!(
            app.redis_resource_event_disposition(&failed()),
            super::RedisResourceEventDisposition::StaleTerminal(request.operation_id())
        );

        app.model
            .connection_states
            .insert(profile.id.clone(), ConnectionState::Disconnected);
        assert_eq!(
            app.redis_resource_event_disposition(&loaded(SessionGeneration(9))),
            super::RedisResourceEventDisposition::StaleTerminal(request.operation_id())
        );
        assert_eq!(
            app.redis_resource_event_disposition(&failed()),
            super::RedisResourceEventDisposition::Apply
        );
    }

    #[test]
    fn mismatched_redis_session_generation_event_mutates_no_ui_state() {
        let (ui, mut service) = bounded_ports(8);
        let mut app = DbotterApp::new(ui);
        assert!(service.try_next_command().is_some());
        let (alpha, _, alpha_key, beta_key) = seed_two_redis_workspaces(&mut app, &mut service);
        app.submit_redis_intent(RedisExplorerIntent::Scan {
            filter: RedisKeyFilter::Glob("*".to_owned()),
            cursor: 0,
            restart: true,
        });
        let request = match service.try_next_command() {
            Some(UiCommand::ScanRedisKeys(request)) => request,
            _ => panic!("Redis scan command"),
        };
        assert!(service.try_emit(crate::ui::UiEvent::RedisKeysLoaded {
            page: redis_page(&request, b"stale-session"),
            session_generation: SessionGeneration(10),
            session_disposition: SessionDisposition::Keep,
        }));
        app.poll_events();

        assert!(!app.active_operations.contains_key(&alpha.id));
        assert!(
            !app.retry_recipes
                .contains(OperationRecipeId(request.operation_id().0)),
            "a stale terminal must release its exact retry bookkeeping"
        );
        assert_eq!(
            redis_keys_for(&app, &alpha_key),
            Some(vec![b"alpha:key".to_vec()]),
            "stale session terminal cannot mutate the exact explorer"
        );
        assert_eq!(
            redis_keys_for(&app, &beta_key),
            Some(vec![b"beta:key".to_vec()]),
            "stale session terminal cannot mutate an unrelated explorer"
        );
        assert_eq!(
            app.model.connection_state(&alpha.id),
            &ConnectionState::Connected {
                session_generation: SessionGeneration(11),
                elapsed_ms: 0,
            }
        );
        assert_eq!(
            app.model
                .workspace(&alpha_key)
                .and_then(|workspace| workspace.redis_key_page.as_ref())
                .and_then(|page| page.keys.first())
                .map(|entry| entry.id.as_bytes()),
            Some(b"alpha:key".as_slice())
        );

        app.submit_redis_intent(RedisExplorerIntent::Scan {
            filter: RedisKeyFilter::Glob("next:*".to_owned()),
            cursor: 0,
            restart: true,
        });
        assert!(matches!(
            service.try_next_command(),
            Some(UiCommand::ScanRedisKeys(next))
                if next.operation_id() != request.operation_id()
                    && next.profile_id() == &alpha.id
        ));
    }

    #[test]
    fn redis_disconnect_and_reconnect_clear_only_the_exact_workspace() {
        let (ui, mut service) = bounded_ports(8);
        let mut disconnect_app = DbotterApp::new(ui);
        assert!(service.try_next_command().is_some());
        let (alpha, _, alpha_key, beta_key) =
            seed_two_redis_workspaces(&mut disconnect_app, &mut service);
        disconnect_app.submit_disconnect(alpha.id.clone());
        let operation_id = match service.try_next_command() {
            Some(UiCommand::DisconnectProfile { operation_id, .. }) => operation_id,
            _ => panic!("disconnect command"),
        };
        assert!(service.try_emit(crate::ui::UiEvent::ConnectionClosed {
            operation_id,
            profile_id: alpha.id.clone(),
            profile_generation: alpha.generation,
            post_close: crate::ui::PostCloseState::Disconnected,
        }));
        disconnect_app.poll_events();
        assert!(redis_keys_for(&disconnect_app, &alpha_key).is_none_or(|keys| keys.is_empty()));
        assert_eq!(
            redis_keys_for(&disconnect_app, &beta_key),
            Some(vec![b"beta:key".to_vec()])
        );

        let (ui, mut service) = bounded_ports(8);
        let mut reconnect_app = DbotterApp::new(ui);
        assert!(service.try_next_command().is_some());
        let (alpha, _, alpha_key, beta_key) =
            seed_two_redis_workspaces(&mut reconnect_app, &mut service);
        reconnect_app.submit_reconnect(alpha.id.clone());
        let operation_id = match service.try_next_command() {
            Some(UiCommand::ReconnectProfile { operation_id, .. }) => operation_id,
            _ => panic!("reconnect command"),
        };
        assert!(service.try_emit(crate::ui::UiEvent::ConnectionReady {
            operation_id,
            profile_id: alpha.id.clone(),
            profile_generation: alpha.generation,
            session_generation: SessionGeneration(12),
            elapsed_ms: 3,
        }));
        reconnect_app.poll_events();
        assert!(redis_keys_for(&reconnect_app, &alpha_key).is_none_or(|keys| keys.is_empty()));
        assert_eq!(
            redis_keys_for(&reconnect_app, &beta_key),
            Some(vec![b"beta:key".to_vec()])
        );
    }

    #[test]
    fn redis_delete_and_reload_prune_only_stale_exact_workspaces() {
        let (ui, mut service) = bounded_ports(8);
        let mut delete_app = DbotterApp::new(ui);
        assert!(service.try_next_command().is_some());
        let (alpha, _, alpha_key, beta_key) =
            seed_two_redis_workspaces(&mut delete_app, &mut service);
        assert!(service.try_emit(crate::ui::UiEvent::ProfileDeleted {
            operation_id: OperationId(800),
            profile_id: alpha.id.clone(),
            profile_generation: ProfileGeneration(alpha.generation.0 + 1),
            server_state_unknown: true,
        }));
        delete_app.poll_events();
        render_redis_explorer(&mut delete_app);
        assert!(redis_keys_for(&delete_app, &alpha_key).is_none());
        assert_eq!(
            redis_keys_for(&delete_app, &beta_key),
            Some(vec![b"beta:key".to_vec()])
        );

        let (ui, mut service) = bounded_ports(8);
        let mut reload_app = DbotterApp::new(ui);
        assert!(service.try_next_command().is_some());
        let (alpha, beta, alpha_key, beta_key) =
            seed_two_redis_workspaces(&mut reload_app, &mut service);
        let refreshed_alpha = redis_profile("redis-alpha", alpha.generation.0 + 1);
        assert!(service.try_emit(crate::ui::UiEvent::ProfilesLoaded {
            operation_id: OperationId(900),
            profiles: vec![refreshed_alpha.clone(), beta],
            config: Default::default(),
        }));
        reload_app.poll_events();
        render_redis_explorer(&mut reload_app);
        let refreshed_alpha_key =
            WorkspaceKey::new(refreshed_alpha.id.clone(), refreshed_alpha.generation);
        assert!(redis_keys_for(&reload_app, &alpha_key).is_none());
        assert_eq!(
            redis_keys_for(&reload_app, &refreshed_alpha_key),
            Some(Vec::new())
        );
        assert_eq!(
            redis_keys_for(&reload_app, &beta_key),
            Some(vec![b"beta:key".to_vec()])
        );
    }

    #[test]
    fn redis_browse_is_blocked_for_unready_profile_without_consuming_operation() {
        let (ui, mut service) = bounded_ports(4);
        let mut app = DbotterApp::new(ui);
        assert!(service.try_next_command().is_some());
        app.model.profiles = vec![profile(DriverKind::Redis, DriverAvailability::Planned)];
        app.model.selected_profile = Some(ProfileId("profile".to_owned()));

        app.submit_redis_intent(RedisExplorerIntent::Scan {
            filter: RedisKeyFilter::Glob("*".to_owned()),
            cursor: 0,
            restart: true,
        });

        assert!(service.try_next_command().is_none());
        assert_eq!(app.model.next_operation(), OperationId(2));
    }

    #[test]
    fn saved_environment_profile_reports_availability_without_exposing_a_value_and_gates_connect() {
        let (ui, mut service) = bounded_ports(4);
        let mut app = DbotterApp::new(ui);
        assert!(service.try_next_command().is_some());
        let mut profile = profile(DriverKind::MySql, DriverAvailability::Ready);
        profile.persisted.credential_mode = CredentialMode::Environment;
        profile.persisted.secret_env = Some("DBOTTER_G_DEFINITELY_MISSING".to_owned());

        for (availability, label, connect_disabled) in [
            (EnvironmentAvailability::Available, "Available", false),
            (EnvironmentAvailability::Missing, "Missing", true),
            (EnvironmentAvailability::Empty, "Empty", true),
        ] {
            profile.environment_availability = Some(availability);
            let context = Context::default();
            context.enable_accesskit();
            let output =
                context.run_ui(RawInput::default(), |ui| app.profile_card(ui, &profile, 0));
            let update = output
                .platform_output
                .accesskit_update
                .as_ref()
                .expect("saved environment profile AccessKit");
            let (_, availability) =
                accesskit_author_node(update, "profile.environment.availability");
            assert_eq!(availability.label(), Some("Environment credential"));
            assert_eq!(availability.value(), Some(label));
            assert_eq!(
                accesskit_author_node(update, "connection.connect")
                    .1
                    .is_disabled(),
                connect_disabled,
                "a saved Environment profile may connect only when its name is Available"
            );
        }
    }

    #[test]
    fn non_v1_profile_and_delete_surfaces_omit_migration_consent() {
        let mut editor = ProfileEditor::new(DraftId(990), DriverKind::MySql);
        let editor_context = Context::default();
        editor_context.enable_accesskit();
        let editor_output = editor_context.run_ui(RawInput::default(), |ui| {
            let _ = editor.show(ui);
        });
        let editor_update = editor_output
            .platform_output
            .accesskit_update
            .as_ref()
            .expect("profile editor AccessKit");
        assert!(
            editor_update
                .nodes
                .iter()
                .all(|(_, node)| node.author_id() != Some("profile.migration.confirm")),
            "migration consent must be absent unless the loaded config is version 1"
        );

        let (ui, mut service) = bounded_ports(4);
        let mut app = DbotterApp::new(ui);
        assert!(service.try_next_command().is_some());
        let profile = profile(DriverKind::MySql, DriverAvailability::Ready);
        app.model
            .active_generations
            .insert(profile.id.clone(), profile.generation);
        app.open_delete_confirmation(&profile);
        let delete_context = Context::default();
        delete_context.enable_accesskit();
        let _ = delete_context.run_ui(RawInput::default(), |ui| {
            app.show_delete_confirmation(ui, false)
        });
        let delete_output = delete_context.run_ui(RawInput::default(), |ui| {
            app.show_delete_confirmation(ui, false)
        });
        let delete_update = delete_output
            .platform_output
            .accesskit_update
            .as_ref()
            .expect("delete confirmation AccessKit");
        assert!(
            delete_update
                .nodes
                .iter()
                .all(|(_, node)| node.author_id() != Some("profile.delete.migration_confirm")),
            "delete migration consent must be absent unless the loaded config is version 1"
        );
    }

    #[test]
    fn migration_surfaces_declare_exact_backup_value_nodes() {
        let form_source = include_str!("profile_form.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("profile form production source");
        let app_source = include_str!("app.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("app production source");

        assert!(form_source.contains("\"profile.migration.backup\""));
        assert!(app_source.contains("\"profile.delete.migration_backup\""));
        assert!(form_source.contains("migration_required"));
    }

    #[test]
    fn v1_profile_and_delete_surfaces_expose_the_exact_confined_backup_path() {
        let config_path = PathBuf::from("/tmp/dbotter-g-v1-config.toml");
        let config = ConfigPresentation::for_source(ConfigSourceVersion::V1, &config_path);
        let backup = config
            .migration_backup()
            .expect("v1 fixed backup")
            .to_string_lossy()
            .into_owned();

        let mut editor = ProfileEditor::new(DraftId(991), DriverKind::MySql);
        editor.set_migration_presentation(config.migration_required(), config.migration_backup());
        let editor_context = Context::default();
        editor_context.enable_accesskit();
        let editor_output = editor_context.run_ui(RawInput::default(), |ui| {
            let _ = editor.show(ui);
        });
        let editor_update = editor_output
            .platform_output
            .accesskit_update
            .as_ref()
            .expect("v1 profile editor AccessKit");
        assert!(
            !accesskit_author_node(editor_update, "profile.migration.confirm")
                .1
                .is_disabled()
        );
        assert_accesskit_value_confined(editor_update, "profile.migration.backup", &backup);

        let (ui, mut service) = bounded_ports(4);
        let mut app = DbotterApp::new(ui);
        assert!(service.try_next_command().is_some());
        app.model.config = config;
        let profile = profile(DriverKind::MySql, DriverAvailability::Ready);
        app.model
            .active_generations
            .insert(profile.id.clone(), profile.generation);
        app.open_delete_confirmation(&profile);
        assert!(
            !format!("{:?}", app.delete_confirmation).contains(&backup),
            "delete confirmation Debug must redact the user-owned backup path"
        );
        let delete_context = Context::default();
        delete_context.enable_accesskit();
        let _ = delete_context.run_ui(RawInput::default(), |ui| {
            app.show_delete_confirmation(ui, false)
        });
        let delete_output = delete_context.run_ui(RawInput::default(), |ui| {
            app.show_delete_confirmation(ui, false)
        });
        let delete_update = delete_output
            .platform_output
            .accesskit_update
            .as_ref()
            .expect("v1 delete confirmation AccessKit");
        assert!(
            !accesskit_author_node(delete_update, "profile.delete.migration_confirm")
                .1
                .is_disabled()
        );
        assert_accesskit_value_confined(delete_update, "profile.delete.migration_backup", &backup);
    }

    #[test]
    fn accepted_reload_updates_already_open_migration_surfaces() {
        let (ui, mut service) = bounded_ports(4);
        let mut app = DbotterApp::new(ui);
        let operation_id = match service.try_next_command() {
            Some(UiCommand::RefreshProfiles { operation_id }) => operation_id,
            other => panic!("startup refresh expected, got {other:?}"),
        };
        app.profile_editor = Some(ProfileEditor::new(DraftId(992), DriverKind::MySql));
        let profile = profile(DriverKind::MySql, DriverAvailability::Ready);
        app.model
            .active_generations
            .insert(profile.id.clone(), profile.generation);
        app.open_delete_confirmation(&profile);

        let config_path = PathBuf::from("/tmp/dbotter-g-reloaded-v1.toml");
        let config = ConfigPresentation::for_source(ConfigSourceVersion::V1, &config_path);
        let backup = config
            .migration_backup()
            .expect("reload backup")
            .to_string_lossy()
            .into_owned();
        assert!(service.try_emit(crate::ui::UiEvent::ProfilesLoaded {
            operation_id,
            profiles: vec![profile],
            config,
        }));
        app.poll_events();

        assert_eq!(
            app.delete_confirmation
                .as_ref()
                .and_then(|confirmation| confirmation.migration_backup.as_deref())
                .map(|path| path.to_string_lossy().into_owned()),
            Some(backup.clone())
        );
        let context = Context::default();
        context.enable_accesskit();
        let output = context.run_ui(RawInput::default(), |ui| {
            let _ = app.profile_editor.as_mut().expect("open editor").show(ui);
        });
        let update = output
            .platform_output
            .accesskit_update
            .as_ref()
            .expect("reloaded v1 editor AccessKit");
        assert_accesskit_value_confined(update, "profile.migration.backup", &backup);
    }
}
