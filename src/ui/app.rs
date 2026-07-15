//! Native three-zone UI. Rendering and state folding perform no I/O.

use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::path::PathBuf;
use std::time::Duration;

use eframe::egui;
use egui_extras::{Column as TableColumn, TableBuilder};

use crate::config::MigrationConsent;
use crate::model::{
    CatalogRequest, Cell, CredentialMode, DEFAULT_CATALOG_PAGE_SIZE, DEFAULT_CATALOG_TIMEOUT,
    DEFAULT_REDIS_SCAN_COUNT, DraftId, DriverAvailability, DriverCapabilities, DriverKind,
    OperationId, OperationKind, OperationRecipeId, ProfileFieldId, ProfileGeneration, ProfileId,
    PublicCode, PublicSummary, RedisKeyInspectRequest, RedisScanRequest, RequestIdentity,
    SessionGeneration,
};
use crate::public_error::{
    PublicOperationError, RecoveryAction, RecoveryCommand, RecoveryCommandDispatcher,
    dispatch_recovery,
};
use crate::secrets::{EnvironmentAvailability, ReplacementSecretBuffer};
use crate::service::DeleteProfileRequest;

use super::accessibility::{
    named_author_id, named_author_id_with_label, named_dynamic_author_id,
    named_dynamic_value_author_id,
};
use super::adapter::{SubmitError, UiCommand, UiPort};
use super::editor::{
    EDITOR_INPUT_ID, EDITOR_ROW_LIMIT_ID, EDITOR_TIMEOUT_ID, EditorIntent, EditorSurface,
};
use super::layout::NativeLayout;
use super::model::{ConnectionState, ProfileSnapshot, UiEvent, UiModel, WorkspaceKey};
use super::mysql_explorer::{MySqlExplorerIntent, MySqlExplorerState};
use super::profile_form::{
    DraftTestAttempt, FormAction, ProfileEditor, ProfileEventResult, SaveAttempt,
};
use super::redis_explorer::{RedisExplorer, RedisExplorerIntent};
use super::theme::OpenAiTheme;

const EVENT_DRAIN_LIMIT: usize = 128;
const RETRY_RECIPE_LIMIT: usize = 64;
pub const DEFAULT_EXECUTE_ROW_LIMIT: u32 = 500;
const DEFAULT_TIMEOUT_MS: u64 = 30_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ActiveOperation {
    operation_id: OperationId,
    profile_generation: ProfileGeneration,
    kind: OperationKind,
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
    next_draft_id: u64,
    pending_connect_after_refresh: Option<(ProfileId, OperationId)>,
}

impl DbotterApp {
    pub fn new(port: UiPort) -> Self {
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
            next_draft_id: 1,
            pending_connect_after_refresh: None,
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

    fn poll_events(&mut self) {
        for mut event in self.port.drain_events(EVENT_DRAIN_LIMIT) {
            match self.redis_resource_event_disposition(&event) {
                RedisResourceEventDisposition::Ignore => continue,
                RedisResourceEventDisposition::StaleTerminal(operation_id) => {
                    self.finish_active_operation(&event);
                    self.retry_recipes.remove(OperationRecipeId(operation_id.0));
                    continue;
                }
                RedisResourceEventDisposition::NotRedis | RedisResourceEventDisposition::Apply => {}
            }
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
            self.model.fold(event);
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
        self.prune_active_operations();
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
                let profile_id = intent.profile_id().clone();
                let profile_generation = intent.profile_generation();
                let operation_kind = intent.operation_kind();
                let workspace_key =
                    super::model::WorkspaceKey::new(profile_id.clone(), profile_generation);
                if self.model.active_generation(intent.profile_id())
                    != Some(intent.profile_generation())
                {
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
                    self.model.status =
                        "Another operation is active for this connection".to_owned();
                    return;
                }
                let operation_id = self.model.next_operation();
                match self.port.try_submit(intent.into_ui_command(operation_id)) {
                    Ok(()) => {
                        self.model.workspace_mut(workspace_key).pending_execute =
                            Some(operation_id);
                        self.active_operations.insert(
                            profile_id,
                            ActiveOperation {
                                operation_id,
                                profile_generation,
                                kind: operation_kind,
                            },
                        );
                        self.model.status = "Executing…".to_owned();
                    }
                    Err(error) => self.report_submit_error(error),
                }
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
            RecoveryCommand::ChooseResultExportDestination(_result_id) => {
                self.model.status = "Export destination selection is not available yet.".to_owned();
            }
            RecoveryCommand::RevealResultExportDestination(_result_id) => {
                self.model.status = "No committed export destination is available.".to_owned();
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
        if let MySqlExplorerIntent::InsertTemplate(template) = intent {
            let key = super::model::WorkspaceKey::new(profile.id.clone(), profile.generation);
            self.model.workspace_mut(key).editor_text = template;
            self.model.status = "Bounded SELECT template inserted; it was not executed".to_owned();
            return;
        }
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
            MySqlExplorerIntent::InsertTemplate(_) => return,
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

    fn show_delete_confirmation(&mut self, root_ui: &mut egui::Ui) {
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
                    if named_author_id(
                        cancel,
                        "profile.delete.cancel",
                        "Cancel profile deletion",
                    )
                    .clicked()
                    {
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

    fn show_credential_prompt(&mut self, root_ui: &mut egui::Ui) {
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
                named_author_id(
                    credential,
                    "connection.credential.value",
                    "Protected session credential",
                );
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

    fn show_native(&mut self, ui: &mut egui::Ui) {
        OpenAiTheme::apply(ui.ctx());
        if self.model.profile_load_succeeded()
            && self.model.profiles.is_empty()
            && self.profile_editor.is_none()
        {
            egui::CentralPanel::default().show(ui, |ui| self.show_first_run(ui));
            return;
        }

        if NativeLayout::columns_for_width(ui.available_width()) == 3 {
            self.connections(ui);
            self.explorer(ui);
        } else {
            self.narrow_navigation(ui);
        }
        self.editor_and_results(ui);
        self.show_delete_confirmation(ui);
        self.show_credential_prompt(ui);
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

    fn narrow_navigation(&mut self, root_ui: &mut egui::Ui) {
        egui::Panel::top("narrow-navigation").show(root_ui, |ui| {
            egui::CollapsingHeader::new("Connections")
                .default_open(false)
                .show(ui, |ui| self.connections_contents(ui));
            egui::CollapsingHeader::new("Explorer")
                .default_open(false)
                .show(ui, |ui| self.explorer_contents(ui));
        });
    }

    fn connections(&mut self, root_ui: &mut egui::Ui) {
        egui::Panel::left("connections")
            .resizable(false)
            .exact_size(NativeLayout::CONNECTIONS_WIDTH)
            .show(root_ui, |ui| self.connections_contents(ui));
    }

    fn connections_contents(&mut self, ui: &mut egui::Ui) {
        ui.heading("Connections");
        let actions_enabled = !self.model.is_config_uncertain();
        let mysql = ui.add_enabled(
            actions_enabled,
            egui::Button::new("+ MySQL")
                .min_size(egui::vec2(104.0, OpenAiTheme::MIN_CONTROL_HEIGHT)),
        );
        if named_author_id(mysql, "connection.new.mysql", "New MySQL connection").clicked() {
            let draft_id = self.allocate_draft_id();
            let editor = ProfileEditor::new(draft_id, DriverKind::MySql);
            self.profile_editor = Some(self.configured_profile_editor(editor));
        }
        let redis = ui.add_enabled(
            actions_enabled,
            egui::Button::new("+ Redis")
                .min_size(egui::vec2(104.0, OpenAiTheme::MIN_CONTROL_HEIGHT)),
        );
        if named_author_id(redis, "connection.new.redis", "New Redis connection").clicked() {
            let draft_id = self.allocate_draft_id();
            let editor = ProfileEditor::new(draft_id, DriverKind::Redis);
            self.profile_editor = Some(self.configured_profile_editor(editor));
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
        if ui
            .add_sized(
                [104.0, OpenAiTheme::MIN_CONTROL_HEIGHT],
                egui::Button::new("Reload"),
            )
            .clicked()
        {
            self.submit_refresh();
        }
        ui.separator();
        egui::ScrollArea::vertical().show(ui, |ui| {
            for profile in self.model.profiles.clone() {
                self.profile_card(ui, &profile);
                ui.add_space(8.0);
            }
        });
    }

    fn explorer(&mut self, root_ui: &mut egui::Ui) {
        egui::Panel::right("explorer")
            .resizable(false)
            .exact_size(NativeLayout::EXPLORER_WIDTH)
            .show(root_ui, |ui| {
                ui.heading("Explorer");
                ui.separator();
                self.explorer_contents(ui);
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

    fn profile_card(&mut self, ui: &mut egui::Ui, profile: &ProfileSnapshot) {
        let selected = self.model.selected_profile.as_ref() == Some(&profile.id);
        if ui
            .selectable_label(selected, format!("{} · {}", profile.name, profile.driver))
            .clicked()
        {
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

    fn editor_and_results(&mut self, root_ui: &mut egui::Ui) {
        if self.profile_editor.is_some() {
            let mut action = FormAction::None;
            egui::CentralPanel::default().show(root_ui, |ui| {
                if let Some(editor) = self.profile_editor.as_mut() {
                    action = editor.show(ui);
                }
            });
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
            return;
        }
        let mut editor_intent = None;
        let mut recovery = None;
        egui::CentralPanel::default().show(root_ui, |ui| {
            let selected_workspace_key = self.model.selected_workspace_key();
            if let Some(profile) = self.model.selected_profile_snapshot().cloned() {
                let editor_enabled = !self.model.is_config_uncertain();
                let key = super::model::WorkspaceKey::new(profile.id.clone(), profile.generation);
                let workspace = self.model.workspace_mut(key);
                editor_intent = self.editor_surface.show(
                    ui,
                    &profile,
                    workspace,
                    editor_enabled && profile.is_ready(),
                );
            } else {
                ui.weak("Select a connection to edit a statement or command.");
            }
            ui.horizontal(|ui| {
                ui.label(&self.model.status);
            });
            if let Some(visible) = self.common_error.clone()
                && let Some(action) = render_recovery_error(ui, "common", &visible)
            {
                recovery = Some((visible, action));
            }
            ui.separator();
            ui.heading("Results");
            if let Some(result) = selected_workspace_key
                .as_ref()
                .and_then(|key| self.model.workspace(key))
                .and_then(|workspace| workspace.result.as_ref())
            {
                render_result(ui, result.as_ref());
            } else {
                ui.weak("No result yet");
            }
        });
        if let Some(intent) = editor_intent {
            self.submit_editor_intent(intent);
        }
        if let Some((visible, action)) = recovery {
            self.dispatch_error_recovery(visible.operation_id, &visible.error, action);
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
        context.request_repaint_after(Duration::from_millis(50));
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.show_native(ui);
    }
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
        ConnectionState::Disconnected => "● Disconnected".to_owned(),
        ConnectionState::Pending(_) => "◌ Connecting…".to_owned(),
        ConnectionState::Connected { elapsed_ms, .. } => {
            format!("● Connected · {elapsed_ms} ms")
        }
        ConnectionState::NeedsCredential => "● Credential required".to_owned(),
        ConnectionState::Failed { summary } => {
            format!("● Failed · {}", summary.message())
        }
        ConnectionState::Closing => "◌ Closing…".to_owned(),
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

fn render_result(ui: &mut egui::Ui, result: &crate::model::ResultSnapshot) {
    ui.horizontal_wrapped(|ui| {
        ui.label(format!("{} rows", result.rows.len()));
        ui.label(format!("{} affected", result.affected_rows));
        ui.label(format!("{} ms", result.provenance.duration_ms));
        if let Some(last_insert_id) = result.last_insert_id {
            ui.label(format!("last insert id {last_insert_id}"));
        }
        if result.truncated {
            ui.strong("Warning: result is truncated");
        }
    });
    for notice in &result.notices {
        ui.small(notice.message());
    }
    if result.columns.is_empty() {
        return;
    }
    let column_count = result.columns.len();
    let mut table = TableBuilder::new(ui)
        .striped(true)
        .resizable(true)
        .column(TableColumn::auto());
    if column_count > 1 {
        table = table.columns(TableColumn::remainder(), column_count - 1);
    }
    table
        .header(24.0, |mut header| {
            for column in &result.columns {
                header.col(|ui| {
                    ui.strong(&column.name);
                    ui.small(&column.type_name);
                });
            }
        })
        .body(|body| {
            body.rows(22.0, result.rows.len(), |mut row| {
                let row_index = row.index();
                let cells = &result.rows[row_index];
                for index in 0..column_count {
                    row.col(|ui| match cells.get(index) {
                        Some(cell) => {
                            let value = display_cell(cell);
                            let response = ui.label(&value);
                            named_dynamic_value_author_id(
                                response,
                                format!("result.cell.{row_index}.{index}"),
                                format!("Result row {} column {}", row_index + 1, index + 1),
                                value,
                            );
                        }
                        None => {
                            ui.strong("Error: <missing>");
                        }
                    });
                }
            });
        });
}

fn display_cell(cell: &Cell) -> String {
    match cell {
        Cell::Null => "NULL".to_owned(),
        Cell::Bool(value) => value.to_string(),
        Cell::Int(value) => value.to_string(),
        Cell::UInt(value) => value.to_string(),
        Cell::Float(value) => value.to_string(),
        Cell::Decimal(value) | Cell::Text(value) | Cell::DateTime(value) => value.clone(),
        Cell::Bytes { preview, len } => format!("{preview} ({len} bytes)"),
        Cell::Json(value) => value.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeSet, HashMap};
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;

    use super::{
        ActiveOperation, ConnectionState, DbotterApp, MySqlExplorerIntent, PendingDelete,
        ProfileEditor,
    };
    use crate::config::ConfigSourceVersion;
    use crate::model::{
        Cell, Column, ConnectionProfile, CredentialMode, DraftId, DriverAvailability, DriverKind,
        OperationId, OperationKind, OperationRecipeId, ProfileFieldId, ProfileGeneration,
        ProfileId, PublicCode, PublicSummary, QueryResult, RedisKeyEntry, RedisKeyFilter,
        RedisKeyId, RedisKeyPage, RedisScanConsistency, RedisScanRequest, RedisTlsConfig,
        RequestIdentity, ResultId, ResultProvenance, ResultRetentionPolicy, ResultSnapshot,
        SessionGeneration, TlsMode,
    };
    use crate::public_error::{PublicOperationError, RecoveryAction, SafeContext};
    use crate::secrets::EnvironmentAvailability;
    use crate::service::SessionDisposition;
    use crate::ui::accessibility::{accesskit_author_node, assert_accesskit_value_confined};
    use crate::ui::adapter::{ServicePort, UiCommand, bounded_ports};
    use crate::ui::editor::{EditorCursor, EditorIntent, build_execute_intent};
    use crate::ui::model::{ConfigPresentation, ProfileSnapshot, WorkspaceKey};
    use crate::ui::redis_explorer::RedisExplorerIntent;
    use eframe::egui::{Context, Event, Key, Modifiers, RawInput, accesskit};

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
        let output = context.run_ui(RawInput::default(), |ui| app.show_delete_confirmation(ui));
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
    fn stale_active_tracking_clears_on_generation_change_uncertainty_and_shutdown() {
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
        assert_eq!(execute.label(), Some("Execute selected or current target"));
        assert_eq!(execute.role(), accesskit::Role::Button);
        assert!(execute.supports_action(accesskit::Action::Focus));
        assert!(execute.supports_action(accesskit::Action::Click));
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
            let output = context.run_ui(RawInput::default(), |ui| app.profile_card(ui, &profile));
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
        let _ = delete_context.run_ui(RawInput::default(), |ui| app.show_delete_confirmation(ui));
        let delete_output =
            delete_context.run_ui(RawInput::default(), |ui| app.show_delete_confirmation(ui));
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
        let _ = delete_context.run_ui(RawInput::default(), |ui| app.show_delete_confirmation(ui));
        let delete_output =
            delete_context.run_ui(RawInput::default(), |ui| app.show_delete_confirmation(ui));
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
