//! Pure UI snapshots and event folding. No driver or network client belongs here.

use std::collections::HashMap;
use std::fmt;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::config::ConfigSourceVersion;
use crate::model::{
    CatalogPage, CatalogRequest, ConnectionProfile, CredentialMode, DraftId, DriverAvailability,
    DriverKind, ExportFormat, OperationId, OperationKind, OverwritePolicy, ProfileGeneration,
    ProfileId, PublicSummary, QueryLanguage, RedisKeyInspectRequest, RedisKeyPage,
    RedisScanRequest, RedisValuePreview, ResultId, ResultSnapshot, SessionGeneration,
};
use crate::public_error::PublicOperationError;
use crate::secrets::EnvironmentAvailability;
use crate::service::SessionDisposition;

use super::result_view::ResultViewState;

#[derive(Clone, PartialEq, Eq)]
pub struct ConfigPresentation {
    source_version: ConfigSourceVersion,
    migration_backup: Option<PathBuf>,
}

impl ConfigPresentation {
    pub fn for_source(source_version: ConfigSourceVersion, config_path: &Path) -> Self {
        Self {
            source_version,
            migration_backup: match source_version {
                ConfigSourceVersion::V1 | ConfigSourceVersion::V2 => Some(
                    crate::config::migration_backup_path_for_source(config_path, source_version),
                ),
                ConfigSourceVersion::Missing | ConfigSourceVersion::V3 => None,
            },
        }
    }

    pub const fn source_version(&self) -> ConfigSourceVersion {
        self.source_version
    }

    pub const fn migration_required(&self) -> bool {
        matches!(
            self.source_version,
            ConfigSourceVersion::V1 | ConfigSourceVersion::V2
        )
    }

    pub fn migration_backup(&self) -> Option<&Path> {
        self.migration_backup.as_deref()
    }
}

impl Default for ConfigPresentation {
    fn default() -> Self {
        Self {
            source_version: ConfigSourceVersion::Missing,
            migration_backup: None,
        }
    }
}

impl fmt::Debug for ConfigPresentation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ConfigPresentation")
            .field("source_version", &self.source_version)
            .field("migration_required", &self.migration_required())
            .field(
                "migration_backup",
                &self.migration_backup.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WorkspaceKey {
    pub profile_id: ProfileId,
    pub profile_generation: ProfileGeneration,
}

impl WorkspaceKey {
    pub fn new(profile_id: ProfileId, profile_generation: ProfileGeneration) -> Self {
        Self {
            profile_id,
            profile_generation,
        }
    }
}

pub const MAX_EDITOR_TABS: usize = 20;
const MAX_EDITOR_TAB_TITLE_BYTES: usize = 120;
const MAX_EDITOR_TAB_TEXT_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EditorTabId(pub u64);

#[derive(Clone)]
pub struct EditorTab {
    id: EditorTabId,
    title: String,
    language: QueryLanguage,
    text: String,
}

impl std::fmt::Debug for EditorTab {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EditorTab")
            .field("id", &self.id)
            .field("title", &"<redacted>")
            .field("language", &self.language)
            .field("text", &"<redacted>")
            .finish()
    }
}

impl EditorTab {
    pub const fn id(&self) -> EditorTabId {
        self.id
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub const fn language(&self) -> QueryLanguage {
        self.language
    }

    pub fn text(&self) -> &str {
        &self.text
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditorTabError {
    LimitReached,
    NotFound,
    InvalidTitle,
    TextTooLarge,
}

impl std::fmt::Display for EditorTabError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::LimitReached => "close a tab before creating another",
            Self::NotFound => "editor tab is no longer available",
            Self::InvalidTitle => "tab title must be 1 to 120 UTF-8 bytes",
            Self::TextTooLarge => "editor tab text exceeds 1 MiB",
        })
    }
}

impl std::error::Error for EditorTabError {}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ResultAreaTab {
    #[default]
    Results,
    History,
}

#[derive(Clone)]
pub struct ProfileWorkspace {
    pub editor_text: String,
    pub caret_character_index: usize,
    pub selection_character_range: Option<Range<usize>>,
    pub row_limit: String,
    pub timeout_seconds: String,
    pub pending_execute: Option<OperationId>,
    pub result: Option<Arc<ResultSnapshot>>,
    pub(crate) result_view: ResultViewState,
    pub error: Option<PublicOperationError>,
    pub catalog_page: Option<CatalogPage>,
    pub catalog_retry: Option<CatalogRequest>,
    pub catalog_error: Option<PublicOperationError>,
    pub redis_key_page: Option<RedisKeyPage>,
    pub redis_scan_retry: Option<RedisScanRequest>,
    pub redis_scan_error: Option<PublicOperationError>,
    pub redis_value_preview: Option<RedisValuePreview>,
    pub redis_inspect_retry: Option<RedisKeyInspectRequest>,
    pub redis_inspect_error: Option<PublicOperationError>,
    editor_tabs: Vec<EditorTab>,
    selected_editor_tab: Option<EditorTabId>,
    next_editor_tab_id: u64,
    result_area_tab: ResultAreaTab,
}

impl std::fmt::Debug for ProfileWorkspace {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProfileWorkspace")
            .field("editor_text", &"<redacted>")
            .field("editor_tabs", &self.editor_tabs)
            .field("selected_editor_tab", &self.selected_editor_tab)
            .field("result_area_tab", &self.result_area_tab)
            .field("pending_execute", &self.pending_execute)
            .field("result", &self.result.as_ref().map(|_| "<retained>"))
            .finish_non_exhaustive()
    }
}

impl Default for ProfileWorkspace {
    fn default() -> Self {
        Self {
            editor_text: String::new(),
            caret_character_index: 0,
            selection_character_range: None,
            row_limit: String::new(),
            timeout_seconds: String::new(),
            pending_execute: None,
            result: None,
            result_view: ResultViewState::default(),
            error: None,
            catalog_page: None,
            catalog_retry: None,
            catalog_error: None,
            redis_key_page: None,
            redis_scan_retry: None,
            redis_scan_error: None,
            redis_value_preview: None,
            redis_inspect_retry: None,
            redis_inspect_error: None,
            editor_tabs: Vec::new(),
            selected_editor_tab: None,
            next_editor_tab_id: 1,
            result_area_tab: ResultAreaTab::Results,
        }
    }
}

impl ProfileWorkspace {
    pub fn has_catalog_retry(&self) -> bool {
        self.catalog_retry.is_some()
    }

    pub fn editor_tabs(&self) -> &[EditorTab] {
        &self.editor_tabs
    }

    pub const fn selected_editor_tab_id(&self) -> Option<EditorTabId> {
        self.selected_editor_tab
    }

    pub fn editor_tab(&self, tab_id: EditorTabId) -> Option<&EditorTab> {
        self.editor_tabs.iter().find(|tab| tab.id == tab_id)
    }

    pub fn create_editor_tab(
        &mut self,
        language: QueryLanguage,
        title: impl Into<String>,
        text: impl Into<String>,
    ) -> Result<EditorTabId, EditorTabError> {
        if self.editor_tabs.len() >= MAX_EDITOR_TABS {
            return Err(EditorTabError::LimitReached);
        }
        let title = title.into();
        validate_editor_tab_title(&title)?;
        let text = text.into();
        if text.len() > MAX_EDITOR_TAB_TEXT_BYTES {
            return Err(EditorTabError::TextTooLarge);
        }
        self.sync_selected_editor_tab_from_surface();
        let id = EditorTabId(self.next_editor_tab_id.max(1));
        self.next_editor_tab_id = id.0.saturating_add(1);
        self.editor_tabs.push(EditorTab {
            id,
            title,
            language,
            text,
        });
        self.selected_editor_tab = Some(id);
        self.load_editor_tab_into_surface(id);
        Ok(id)
    }

    pub fn rename_editor_tab(
        &mut self,
        tab_id: EditorTabId,
        title: impl Into<String>,
    ) -> Result<(), EditorTabError> {
        let title = title.into();
        validate_editor_tab_title(&title)?;
        let Some(tab) = self.editor_tabs.iter_mut().find(|tab| tab.id == tab_id) else {
            return Err(EditorTabError::NotFound);
        };
        tab.title = title;
        Ok(())
    }

    pub fn duplicate_editor_tab(
        &mut self,
        tab_id: EditorTabId,
    ) -> Result<EditorTabId, EditorTabError> {
        self.sync_selected_editor_tab_from_surface();
        let Some(source) = self.editor_tab(tab_id).cloned() else {
            return Err(EditorTabError::NotFound);
        };
        let mut title = format!("{} copy", source.title);
        if title.len() > MAX_EDITOR_TAB_TITLE_BYTES {
            title.truncate(MAX_EDITOR_TAB_TITLE_BYTES);
            while !title.is_char_boundary(title.len()) {
                let _ = title.pop();
            }
        }
        self.create_editor_tab(source.language, title, source.text)
    }

    pub fn select_editor_tab(&mut self, tab_id: EditorTabId) -> Result<(), EditorTabError> {
        if self.editor_tab(tab_id).is_none() {
            return Err(EditorTabError::NotFound);
        }
        self.sync_selected_editor_tab_from_surface();
        self.selected_editor_tab = Some(tab_id);
        self.load_editor_tab_into_surface(tab_id);
        Ok(())
    }

    pub fn close_editor_tab(&mut self, tab_id: EditorTabId) -> Result<(), EditorTabError> {
        let Some(index) = self.editor_tabs.iter().position(|tab| tab.id == tab_id) else {
            return Err(EditorTabError::NotFound);
        };
        self.sync_selected_editor_tab_from_surface();
        let closing_selected = self.selected_editor_tab == Some(tab_id);
        self.editor_tabs.remove(index);
        if closing_selected {
            self.selected_editor_tab = self
                .editor_tabs
                .get(index.min(self.editor_tabs.len().saturating_sub(1)))
                .map(EditorTab::id);
            if let Some(selected) = self.selected_editor_tab {
                self.load_editor_tab_into_surface(selected);
            } else {
                self.editor_text.clear();
                self.caret_character_index = 0;
                self.selection_character_range = None;
            }
        }
        Ok(())
    }

    pub fn sync_selected_editor_tab_from_surface(&mut self) {
        let Some(selected) = self.selected_editor_tab else {
            return;
        };
        let Some(tab) = self.editor_tabs.iter_mut().find(|tab| tab.id == selected) else {
            return;
        };
        if self.editor_text.len() <= MAX_EDITOR_TAB_TEXT_BYTES {
            tab.text.clone_from(&self.editor_text);
        }
    }

    pub const fn result_area_tab(&self) -> ResultAreaTab {
        self.result_area_tab
    }

    pub const fn select_result_area_tab(&mut self, tab: ResultAreaTab) {
        self.result_area_tab = tab;
    }

    fn load_editor_tab_into_surface(&mut self, tab_id: EditorTabId) {
        let Some(tab) = self.editor_tab(tab_id) else {
            return;
        };
        let text = tab.text.clone();
        self.editor_text = text;
        self.caret_character_index = self.editor_text.chars().count();
        self.selection_character_range = None;
    }
}

fn validate_editor_tab_title(title: &str) -> Result<(), EditorTabError> {
    if title.trim().is_empty() || title.len() > MAX_EDITOR_TAB_TITLE_BYTES {
        Err(EditorTabError::InvalidTitle)
    } else {
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PostCloseState {
    Disconnected,
    NeedsCredential,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionFailureOutcome {
    Preserve,
    Disconnected,
    Unknown,
    NeedsCredential,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfileSnapshot {
    pub id: ProfileId,
    pub generation: ProfileGeneration,
    pub name: String,
    pub driver: DriverKind,
    pub endpoint: String,
    pub database: Option<String>,
    pub availability: DriverAvailability,
    pub planned_reason: Option<String>,
    pub has_current_session_secret: bool,
    pub environment_availability: Option<EnvironmentAvailability>,
    pub persisted: ConnectionProfile,
}

impl ProfileSnapshot {
    pub fn from_profile(
        profile: &ConnectionProfile,
        generation: ProfileGeneration,
        has_current_session_secret: bool,
        environment_availability: Option<EnvironmentAvailability>,
    ) -> Self {
        let descriptor = crate::drivers::descriptors()
            .into_iter()
            .find(|descriptor| descriptor.kind == profile.driver);
        let availability =
            descriptor.map_or(DriverAvailability::Planned, |value| value.availability);
        Self {
            id: ProfileId(profile.id.clone()),
            generation,
            name: profile.name.clone(),
            driver: profile.driver,
            endpoint: profile.redacted_endpoint(),
            database: profile.database.clone(),
            availability,
            planned_reason: descriptor.and_then(|value| value.reason).map(str::to_owned),
            has_current_session_secret,
            environment_availability,
            persisted: profile.clone(),
        }
    }

    pub fn is_ready(&self) -> bool {
        self.availability == DriverAvailability::Ready
    }

    pub fn can_connect(&self) -> bool {
        self.is_ready()
            && (self.persisted.credential_mode != CredentialMode::Environment
                || self.environment_availability == Some(EnvironmentAvailability::Available))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectionState {
    Disconnected,
    Pending(OperationId),
    Connected {
        session_generation: SessionGeneration,
        elapsed_ms: u64,
    },
    NeedsCredential,
    Failed {
        summary: PublicSummary,
    },
    Closing,
}

impl ConnectionState {
    pub fn is_pending(&self) -> bool {
        matches!(self, Self::Pending(_))
    }

    pub(crate) fn accepts_redis_event_session(
        &self,
        event_session: Option<SessionGeneration>,
    ) -> bool {
        match (self, event_session) {
            (
                Self::Connected {
                    session_generation: current,
                    ..
                },
                Some(event),
            ) => *current == event,
            (Self::Connected { .. }, None) | (_, Some(_)) => false,
            (_, None) => true,
        }
    }
}

#[derive(Clone, Debug)]
pub enum UiEvent {
    ProfilesLoaded {
        operation_id: OperationId,
        profiles: Vec<ProfileSnapshot>,
        config: ConfigPresentation,
    },
    ProfilesFailed {
        operation_id: OperationId,
        summary: PublicSummary,
        error: PublicOperationError,
    },
    ProfileSaved {
        operation_id: OperationId,
        profile_id: ProfileId,
        previous_generation: Option<ProfileGeneration>,
        profile_generation: ProfileGeneration,
        session_retained: bool,
        warning: Option<PublicSummary>,
    },
    ProfileCreateFailed {
        operation_id: OperationId,
        draft_id: DraftId,
        summary: PublicSummary,
        error: PublicOperationError,
    },
    ProfileUpdateFailed {
        operation_id: OperationId,
        profile_id: ProfileId,
        profile_generation: ProfileGeneration,
        summary: PublicSummary,
        error: PublicOperationError,
    },
    CredentialsStored {
        operation_id: OperationId,
        profile_id: ProfileId,
        profile_generation: ProfileGeneration,
    },
    CredentialsStoreFailed {
        operation_id: OperationId,
        profile_id: ProfileId,
        profile_generation: ProfileGeneration,
        summary: PublicSummary,
        error: PublicOperationError,
    },
    ConnectionReady {
        operation_id: OperationId,
        profile_id: ProfileId,
        profile_generation: ProfileGeneration,
        session_generation: SessionGeneration,
        elapsed_ms: u64,
    },
    ConnectionClosed {
        operation_id: OperationId,
        profile_id: ProfileId,
        profile_generation: ProfileGeneration,
        post_close: PostCloseState,
    },
    DraftConnectionReady {
        operation_id: OperationId,
        draft_id: DraftId,
        elapsed_ms: u64,
    },
    DraftOperationFailed {
        operation_id: OperationId,
        draft_id: DraftId,
        summary: PublicSummary,
        error: PublicOperationError,
    },
    QueryFinished {
        operation_id: OperationId,
        profile_id: ProfileId,
        profile_generation: ProfileGeneration,
        session_generation: SessionGeneration,
        result: ResultSnapshot,
    },
    ResultExported {
        operation_id: OperationId,
        result_id: ResultId,
        format: ExportFormat,
        overwrite_policy: OverwritePolicy,
        row_count: usize,
        bytes_written: u64,
    },
    ResultExportFailed {
        operation_id: OperationId,
        result_id: ResultId,
        format: ExportFormat,
        overwrite_policy: OverwritePolicy,
        summary: PublicSummary,
        error: PublicOperationError,
        destination_committed: bool,
    },
    CatalogPageLoaded {
        page: CatalogPage,
        session_generation: SessionGeneration,
        session_disposition: SessionDisposition,
    },
    CatalogPageFailed {
        request: CatalogRequest,
        summary: PublicSummary,
        error: PublicOperationError,
        session_generation: Option<SessionGeneration>,
        session_disposition: Option<SessionDisposition>,
    },
    RedisKeysLoaded {
        page: RedisKeyPage,
        session_generation: SessionGeneration,
        session_disposition: SessionDisposition,
    },
    RedisKeysFailed {
        request: RedisScanRequest,
        error: PublicOperationError,
        session_generation: Option<SessionGeneration>,
        session_disposition: Option<SessionDisposition>,
        connection_outcome: ConnectionFailureOutcome,
    },
    RedisKeyInspected {
        preview: RedisValuePreview,
        session_generation: SessionGeneration,
        session_disposition: SessionDisposition,
    },
    RedisKeyInspectFailed {
        request: RedisKeyInspectRequest,
        error: PublicOperationError,
        session_generation: Option<SessionGeneration>,
        session_disposition: Option<SessionDisposition>,
        connection_outcome: ConnectionFailureOutcome,
    },
    OperationFailed {
        operation_id: OperationId,
        profile_id: ProfileId,
        profile_generation: ProfileGeneration,
        session_generation: Option<SessionGeneration>,
        kind: OperationKind,
        summary: PublicSummary,
        error: PublicOperationError,
        session_disposition: Option<SessionDisposition>,
        connection_outcome: ConnectionFailureOutcome,
    },
    ExecuteUnavailable {
        operation_id: OperationId,
        profile_id: ProfileId,
        profile_generation: ProfileGeneration,
        summary: PublicSummary,
    },
    ProfileDeleted {
        operation_id: OperationId,
        profile_id: ProfileId,
        profile_generation: ProfileGeneration,
        server_state_unknown: bool,
    },
    ConfigUncertain {
        operation_id: OperationId,
    },
    RuntimeShutdown {
        operation_id: OperationId,
    },
}

pub struct UiModel {
    pub profiles: Vec<ProfileSnapshot>,
    pub selected_profile: Option<ProfileId>,
    pub active_generations: HashMap<ProfileId, ProfileGeneration>,
    pub tombstones: HashMap<ProfileId, ProfileGeneration>,
    pub connection_states: HashMap<ProfileId, ConnectionState>,
    pub workspaces: HashMap<WorkspaceKey, ProfileWorkspace>,
    pub status: String,
    pub config: ConfigPresentation,
    config_uncertain: bool,
    profile_load_succeeded: bool,
    last_profiles_operation: Option<OperationId>,
    pending_retags: HashMap<ProfileId, (ProfileGeneration, ProfileGeneration)>,
    next_operation_id: u64,
}

impl Default for UiModel {
    fn default() -> Self {
        Self {
            profiles: Vec::new(),
            selected_profile: None,
            active_generations: HashMap::new(),
            tombstones: HashMap::new(),
            connection_states: HashMap::new(),
            workspaces: HashMap::new(),
            status: "Loading profiles…".to_owned(),
            config: ConfigPresentation::default(),
            config_uncertain: false,
            profile_load_succeeded: false,
            last_profiles_operation: None,
            pending_retags: HashMap::new(),
            next_operation_id: 1,
        }
    }
}

impl UiModel {
    pub fn workspace(&self, key: &WorkspaceKey) -> Option<&ProfileWorkspace> {
        self.workspaces.get(key)
    }

    pub fn workspace_mut(&mut self, key: WorkspaceKey) -> &mut ProfileWorkspace {
        self.workspaces
            .entry(key)
            .or_insert_with(|| ProfileWorkspace {
                row_limit: "500".to_owned(),
                timeout_seconds: "30".to_owned(),
                ..ProfileWorkspace::default()
            })
    }

    pub fn selected_workspace_key(&self) -> Option<WorkspaceKey> {
        let profile = self.selected_profile_snapshot()?;
        Some(WorkspaceKey::new(profile.id.clone(), profile.generation))
    }

    pub fn selected_workspace(&self) -> Option<&ProfileWorkspace> {
        let key = self.selected_workspace_key()?;
        self.workspace(&key)
    }

    pub fn selected_workspace_mut(&mut self) -> Option<&mut ProfileWorkspace> {
        let key = self.selected_workspace_key()?;
        Some(self.workspace_mut(key))
    }

    fn exact_workspace_mut(
        &mut self,
        profile_id: &ProfileId,
        profile_generation: ProfileGeneration,
    ) -> &mut ProfileWorkspace {
        self.workspace_mut(WorkspaceKey::new(profile_id.clone(), profile_generation))
    }

    pub fn next_operation(&mut self) -> OperationId {
        let operation_id = OperationId(self.next_operation_id);
        self.next_operation_id = self.next_operation_id.saturating_add(1);
        operation_id
    }

    pub fn selected_profile_snapshot(&self) -> Option<&ProfileSnapshot> {
        let selected = self.selected_profile.as_ref()?;
        self.profiles.iter().find(|profile| profile.id == *selected)
    }

    pub fn connection_state(&self, profile_id: &ProfileId) -> &ConnectionState {
        self.connection_states
            .get(profile_id)
            .unwrap_or(&ConnectionState::Disconnected)
    }

    pub fn active_generation(&self, profile_id: &ProfileId) -> Option<ProfileGeneration> {
        self.active_generations.get(profile_id).copied()
    }

    pub fn tombstone_generation(&self, profile_id: &ProfileId) -> Option<ProfileGeneration> {
        self.tombstones.get(profile_id).copied()
    }

    pub fn is_config_uncertain(&self) -> bool {
        self.config_uncertain
    }

    pub(crate) fn profile_load_succeeded(&self) -> bool {
        self.profile_load_succeeded
    }

    pub(crate) fn profiles_operation_is_newer(&self, operation_id: OperationId) -> bool {
        self.last_profiles_operation
            .is_none_or(|latest| operation_id.0 > latest.0)
    }

    pub fn fold(&mut self, event: UiEvent) {
        match event {
            UiEvent::ProfilesLoaded {
                operation_id,
                profiles,
                config,
            } => {
                if !self.accept_profiles_operation(operation_id) {
                    return;
                }
                self.profile_load_succeeded = true;
                self.config = config;
                self.fold_profiles(profiles);
            }
            UiEvent::ProfilesFailed {
                operation_id,
                error,
                ..
            } => {
                if !self.accept_profiles_operation(operation_id) {
                    return;
                }
                self.status = error.summary.message().to_owned();
            }
            UiEvent::ProfileSaved {
                profile_id,
                previous_generation,
                profile_generation,
                session_retained,
                warning,
                ..
            } => {
                let save_is_current = self
                    .tombstones
                    .get(&profile_id)
                    .is_none_or(|tombstone| profile_generation.0 > tombstone.0)
                    && match previous_generation {
                        Some(previous) => {
                            self.active_generations
                                .get(&profile_id)
                                .is_some_and(|active| {
                                    *active == previous || *active == profile_generation
                                })
                        }
                        None => self
                            .active_generations
                            .get(&profile_id)
                            .is_none_or(|active| active.0 <= profile_generation.0),
                    };
                if !save_is_current {
                    return;
                }
                if session_retained && let Some(previous_generation) = previous_generation {
                    self.pending_retags.insert(
                        profile_id.clone(),
                        (previous_generation, profile_generation),
                    );
                } else {
                    self.pending_retags.remove(&profile_id);
                    self.connection_states.remove(&profile_id);
                }
                self.active_generations
                    .insert(profile_id, profile_generation);
                if let Some(summary) = warning {
                    self.status = summary.message().to_owned();
                }
            }
            UiEvent::ProfileCreateFailed { error, .. }
            | UiEvent::ProfileUpdateFailed { error, .. } => {
                self.status = error.summary.message().to_owned();
            }
            UiEvent::CredentialsStored {
                profile_id,
                profile_generation,
                ..
            } => {
                if self.event_is_current(&profile_id, profile_generation) {
                    if let Some(profile) =
                        self.profiles.iter_mut().find(|item| item.id == profile_id)
                    {
                        profile.has_current_session_secret = true;
                    }
                    if matches!(
                        self.connection_states.get(&profile_id),
                        Some(ConnectionState::NeedsCredential)
                    ) {
                        self.connection_states
                            .insert(profile_id, ConnectionState::Disconnected);
                    }
                    self.status = "Session credential stored.".to_owned();
                }
            }
            UiEvent::CredentialsStoreFailed {
                profile_id,
                profile_generation,
                error,
                ..
            } => {
                if self.event_is_current(&profile_id, profile_generation) {
                    self.status = error.summary.message().to_owned();
                }
            }
            UiEvent::ConnectionReady {
                operation_id,
                profile_id,
                profile_generation,
                session_generation,
                elapsed_ms,
            } => {
                if self.event_is_current(&profile_id, profile_generation)
                    && self.connection_states.get(&profile_id)
                        == Some(&ConnectionState::Pending(operation_id))
                {
                    self.connection_states.insert(
                        profile_id,
                        ConnectionState::Connected {
                            session_generation,
                            elapsed_ms,
                        },
                    );
                    self.status = format!("Connection ready in {elapsed_ms} ms");
                }
            }
            UiEvent::ConnectionClosed {
                operation_id,
                profile_id,
                profile_generation,
                post_close,
            } => {
                if self.event_is_current(&profile_id, profile_generation)
                    && self.connection_states.get(&profile_id)
                        == Some(&ConnectionState::Pending(operation_id))
                {
                    self.connection_states.insert(
                        profile_id,
                        match post_close {
                            PostCloseState::Disconnected => ConnectionState::Disconnected,
                            PostCloseState::NeedsCredential => ConnectionState::NeedsCredential,
                        },
                    );
                    self.status = "Disconnected".to_owned();
                }
            }
            UiEvent::DraftConnectionReady { elapsed_ms, .. } => {
                self.status = format!("Draft connection ready in {elapsed_ms} ms");
            }
            UiEvent::DraftOperationFailed { error, .. } => {
                self.status = error.summary.message().to_owned();
            }
            UiEvent::ExecuteUnavailable {
                operation_id,
                profile_id,
                profile_generation,
                summary,
            } => {
                if self.event_is_current(&profile_id, profile_generation) {
                    let workspace = self.exact_workspace_mut(&profile_id, profile_generation);
                    if workspace.pending_execute == Some(operation_id) {
                        workspace.pending_execute = None;
                    }
                    self.status = summary.message().to_owned();
                }
            }
            UiEvent::QueryFinished {
                operation_id,
                profile_id,
                profile_generation,
                session_generation,
                result,
            } => {
                if self.event_is_current(&profile_id, profile_generation) {
                    let duration_ms = result.provenance.duration_ms;
                    {
                        let workspace = self.exact_workspace_mut(&profile_id, profile_generation);
                        if workspace.pending_execute != Some(operation_id) {
                            return;
                        }
                        workspace.pending_execute = None;
                        workspace.error = None;
                        workspace.result_view.reset_for(result.provenance.result_id);
                        workspace.result = Some(Arc::new(result));
                    }
                    self.status = format!("Query finished in {duration_ms} ms");
                    self.connection_states.insert(
                        profile_id,
                        ConnectionState::Connected {
                            session_generation,
                            elapsed_ms: 0,
                        },
                    );
                }
            }
            UiEvent::ResultExported {
                row_count,
                bytes_written,
                ..
            } => {
                self.status = format!("Exported {row_count} rows ({bytes_written} bytes).");
            }
            UiEvent::ResultExportFailed { error, .. } => {
                self.status = error.summary.message().to_owned();
            }
            UiEvent::CatalogPageLoaded {
                page,
                session_generation,
                session_disposition,
            } => {
                let profile_id = page.identity.profile_id.clone();
                if self.event_is_current(&profile_id, page.identity.profile_generation) {
                    self.fold_catalog_session(
                        &profile_id,
                        Some(session_generation),
                        Some(session_disposition),
                    );
                    let workspace =
                        self.exact_workspace_mut(&profile_id, page.identity.profile_generation);
                    workspace.catalog_retry = None;
                    workspace.catalog_error = None;
                    workspace.catalog_page = Some(page);
                    self.status = "Catalog page loaded".to_owned();
                }
            }
            UiEvent::CatalogPageFailed {
                request,
                error,
                session_generation,
                session_disposition,
                ..
            } => {
                let profile_id = request.profile_id().clone();
                if self.event_is_current(&profile_id, request.profile_generation()) {
                    let status = error.summary.message().to_owned();
                    self.fold_catalog_session(&profile_id, session_generation, session_disposition);
                    let workspace =
                        self.exact_workspace_mut(&profile_id, request.profile_generation());
                    if let Some(page) = workspace.catalog_page.as_mut() {
                        page.stale = true;
                    }
                    workspace.catalog_retry = Some(request);
                    workspace.catalog_error = Some(error);
                    self.status = status;
                }
            }
            UiEvent::RedisKeysLoaded {
                page,
                session_generation,
                session_disposition,
            } => {
                let profile_id = page.identity.profile_id.clone();
                if self.redis_event_is_current(
                    &profile_id,
                    page.identity.profile_generation,
                    Some(session_generation),
                ) {
                    let workspace =
                        self.exact_workspace_mut(&profile_id, page.identity.profile_generation);
                    workspace.redis_scan_retry = None;
                    workspace.redis_scan_error = None;
                    workspace.redis_key_page = Some(page);
                    self.apply_redis_session_truth(
                        profile_id,
                        Some(session_generation),
                        Some(session_disposition),
                        ConnectionFailureOutcome::Preserve,
                    );
                    self.status = "Redis keys loaded".to_owned();
                }
            }
            UiEvent::RedisKeysFailed {
                request,
                error,
                session_generation,
                session_disposition,
                connection_outcome,
            } => {
                let profile_id = request.profile_id().clone();
                if self.redis_event_is_current(
                    &profile_id,
                    request.profile_generation(),
                    session_generation,
                ) {
                    let status = error.summary.message().to_owned();
                    let workspace =
                        self.exact_workspace_mut(&profile_id, request.profile_generation());
                    if let Some(page) = workspace.redis_key_page.as_mut() {
                        page.stale = true;
                    }
                    workspace.redis_scan_retry = Some(request);
                    workspace.redis_scan_error = Some(error);
                    self.status = status;
                    self.apply_redis_session_truth(
                        profile_id,
                        session_generation,
                        session_disposition,
                        connection_outcome,
                    );
                }
            }
            UiEvent::RedisKeyInspected {
                preview,
                session_generation,
                session_disposition,
            } => {
                let profile_id = preview.identity.profile_id.clone();
                if self.redis_event_is_current(
                    &profile_id,
                    preview.identity.profile_generation,
                    Some(session_generation),
                ) {
                    let workspace =
                        self.exact_workspace_mut(&profile_id, preview.identity.profile_generation);
                    workspace.redis_inspect_retry = None;
                    workspace.redis_inspect_error = None;
                    workspace.redis_value_preview = Some(preview);
                    self.apply_redis_session_truth(
                        profile_id,
                        Some(session_generation),
                        Some(session_disposition),
                        ConnectionFailureOutcome::Preserve,
                    );
                    self.status = "Redis key inspected".to_owned();
                }
            }
            UiEvent::RedisKeyInspectFailed {
                request,
                error,
                session_generation,
                session_disposition,
                connection_outcome,
            } => {
                let profile_id = request.profile_id().clone();
                if self.redis_event_is_current(
                    &profile_id,
                    request.profile_generation(),
                    session_generation,
                ) {
                    let status = error.summary.message().to_owned();
                    let workspace =
                        self.exact_workspace_mut(&profile_id, request.profile_generation());
                    if let Some(preview) = workspace.redis_value_preview.as_mut() {
                        preview.stale = true;
                    }
                    workspace.redis_inspect_retry = Some(request);
                    workspace.redis_inspect_error = Some(error);
                    self.status = status;
                    self.apply_redis_session_truth(
                        profile_id,
                        session_generation,
                        session_disposition,
                        connection_outcome,
                    );
                }
            }
            UiEvent::OperationFailed {
                operation_id,
                profile_id,
                profile_generation,
                kind,
                error,
                connection_outcome,
                ..
            } => {
                if !self.event_is_current(&profile_id, profile_generation) {
                    return;
                }
                let is_connection_attempt = matches!(
                    kind,
                    OperationKind::ConnectProfile | OperationKind::ReconnectProfile
                );
                let connection_outcome_is_correlated = !matches!(
                    self.connection_states.get(&profile_id),
                    Some(ConnectionState::Pending(pending)) if *pending != operation_id
                );
                let summary = error.summary;
                match kind {
                    OperationKind::ConnectProfile | OperationKind::ReconnectProfile => {
                        if self.connection_states.get(&profile_id)
                            == Some(&ConnectionState::Pending(operation_id))
                        {
                            let visible_state = if summary == PublicSummary::CredentialRequired
                                && connection_outcome == ConnectionFailureOutcome::NeedsCredential
                            {
                                ConnectionState::NeedsCredential
                            } else {
                                ConnectionState::Failed { summary }
                            };
                            self.connection_states
                                .insert(profile_id.clone(), visible_state);
                            self.status = summary.message().to_owned();
                        }
                    }
                    OperationKind::DisconnectProfile => {
                        if self.connection_states.get(&profile_id)
                            == Some(&ConnectionState::Pending(operation_id))
                        {
                            self.connection_states
                                .insert(profile_id.clone(), ConnectionState::Failed { summary });
                            self.status = summary.message().to_owned();
                        }
                    }
                    OperationKind::ExecuteRead | OperationKind::ExecuteMutation => {
                        let workspace = self.exact_workspace_mut(&profile_id, profile_generation);
                        if workspace.pending_execute == Some(operation_id) {
                            workspace.pending_execute = None;
                            workspace.error = Some(error);
                            self.status = summary.message().to_owned();
                        }
                    }
                    _ => self.status = summary.message().to_owned(),
                }
                if !is_connection_attempt && connection_outcome_is_correlated {
                    match connection_outcome {
                        ConnectionFailureOutcome::Preserve => {}
                        ConnectionFailureOutcome::Disconnected
                        | ConnectionFailureOutcome::Unknown => {
                            self.connection_states
                                .insert(profile_id.clone(), ConnectionState::Disconnected);
                        }
                        ConnectionFailureOutcome::NeedsCredential => {
                            self.connection_states
                                .insert(profile_id, ConnectionState::NeedsCredential);
                        }
                    }
                }
            }
            UiEvent::ProfileDeleted {
                profile_id,
                profile_generation,
                server_state_unknown,
                ..
            } => self.fold_deleted(profile_id, profile_generation, server_state_unknown),
            UiEvent::ConfigUncertain { operation_id } => {
                if !self.accept_profiles_operation(operation_id) {
                    return;
                }
                self.config_uncertain = true;
                self.pending_retags.clear();
                self.connection_states.clear();
                for workspace in self.workspaces.values_mut() {
                    workspace.pending_execute = None;
                }
                self.status = "Configuration state is uncertain.".to_owned();
            }
            UiEvent::RuntimeShutdown { .. } => {
                for profile_id in self.active_generations.keys() {
                    self.connection_states
                        .insert(profile_id.clone(), ConnectionState::Closing);
                }
                for workspace in self.workspaces.values_mut() {
                    workspace.pending_execute = None;
                }
                self.status = "Runtime shut down".to_owned();
            }
        }
    }

    fn apply_redis_session_truth(
        &mut self,
        profile_id: ProfileId,
        session_generation: Option<SessionGeneration>,
        session_disposition: Option<SessionDisposition>,
        connection_outcome: ConnectionFailureOutcome,
    ) {
        match (session_generation, session_disposition) {
            (Some(session_generation), Some(SessionDisposition::Keep)) => {
                self.connection_states.insert(
                    profile_id,
                    ConnectionState::Connected {
                        session_generation,
                        elapsed_ms: 0,
                    },
                );
            }
            (_, Some(SessionDisposition::Evict)) => {
                self.connection_states
                    .insert(profile_id, ConnectionState::Disconnected);
            }
            _ => match connection_outcome {
                ConnectionFailureOutcome::Preserve => {}
                ConnectionFailureOutcome::Disconnected | ConnectionFailureOutcome::Unknown => {
                    self.connection_states
                        .insert(profile_id, ConnectionState::Disconnected);
                }
                ConnectionFailureOutcome::NeedsCredential => {
                    self.connection_states
                        .insert(profile_id, ConnectionState::NeedsCredential);
                }
            },
        }
    }

    fn accept_profiles_operation(&mut self, operation_id: OperationId) -> bool {
        if !self.profiles_operation_is_newer(operation_id) {
            return false;
        }
        self.last_profiles_operation = Some(operation_id);
        true
    }

    fn event_is_current(&self, profile_id: &ProfileId, generation: ProfileGeneration) -> bool {
        !self.config_uncertain
            && self.active_generations.get(profile_id).copied() == Some(generation)
            && self
                .tombstones
                .get(profile_id)
                .is_none_or(|tombstone| generation.0 > tombstone.0)
    }

    fn redis_event_is_current(
        &self,
        profile_id: &ProfileId,
        generation: ProfileGeneration,
        session_generation: Option<SessionGeneration>,
    ) -> bool {
        self.event_is_current(profile_id, generation)
            && self
                .connection_state(profile_id)
                .accepts_redis_event_session(session_generation)
    }

    fn fold_catalog_session(
        &mut self,
        profile_id: &ProfileId,
        session_generation: Option<SessionGeneration>,
        disposition: Option<SessionDisposition>,
    ) {
        let (Some(session_generation), Some(disposition)) = (session_generation, disposition)
        else {
            return;
        };
        let current = self.connection_states.get(profile_id);
        let protected = matches!(
            current,
            Some(ConnectionState::Pending(_) | ConnectionState::Closing)
        ) || matches!(
            current,
            Some(ConnectionState::Connected {
                session_generation: visible,
                ..
            }) if *visible != session_generation
        );
        if protected {
            return;
        }
        let next = match disposition {
            SessionDisposition::Keep => ConnectionState::Connected {
                session_generation,
                elapsed_ms: 0,
            },
            SessionDisposition::Evict => ConnectionState::Disconnected,
        };
        self.connection_states.insert(profile_id.clone(), next);
    }

    fn fold_deleted(
        &mut self,
        profile_id: ProfileId,
        deletion_generation: ProfileGeneration,
        server_state_unknown: bool,
    ) {
        if self
            .active_generations
            .get(&profile_id)
            .is_some_and(|active| active.0 >= deletion_generation.0)
        {
            return;
        }
        self.tombstones
            .entry(profile_id.clone())
            .and_modify(|current| {
                if deletion_generation.0 > current.0 {
                    *current = deletion_generation;
                }
            })
            .or_insert(deletion_generation);
        self.active_generations.remove(&profile_id);
        self.profiles.retain(|profile| profile.id != profile_id);
        self.connection_states.remove(&profile_id);
        self.workspaces
            .retain(|key, _| key.profile_id != profile_id);
        if self.selected_profile.as_ref() == Some(&profile_id) {
            self.selected_profile = self.profiles.first().map(|profile| profile.id.clone());
        }
        self.status = if server_state_unknown {
            "Profile deleted; server state is unknown.".to_owned()
        } else {
            "Profile deleted".to_owned()
        };
    }

    fn fold_profiles(&mut self, profiles: Vec<ProfileSnapshot>) {
        for (profile_id, generation) in self.active_generations.clone() {
            if profiles.iter().all(|profile| profile.id != profile_id) {
                self.tombstones
                    .entry(profile_id)
                    .and_modify(|current| {
                        if generation.0 > current.0 {
                            *current = generation;
                        }
                    })
                    .or_insert(generation);
            }
        }
        let profiles = profiles
            .into_iter()
            .filter(|profile| {
                self.tombstones
                    .get(&profile.id)
                    .is_none_or(|tombstone| profile.generation.0 > tombstone.0)
            })
            .collect::<Vec<_>>();
        self.connection_states.retain(|profile_id, _| {
            let previous = self
                .profiles
                .iter()
                .find(|profile| profile.id == *profile_id);
            let refreshed = profiles.iter().find(|profile| profile.id == *profile_id);
            matches!((previous, refreshed), (Some(previous), Some(refreshed)) if
                (previous.generation == refreshed.generation
                    && previous.persisted == refreshed.persisted)
                || self.pending_retags.get(profile_id)
                    == Some(&(previous.generation, refreshed.generation)))
        });
        self.active_generations = profiles
            .iter()
            .filter(|profile| {
                self.tombstones
                    .get(&profile.id)
                    .is_none_or(|tombstone| profile.generation.0 > tombstone.0)
            })
            .map(|profile| (profile.id.clone(), profile.generation))
            .collect();
        self.workspaces.retain(|key, _| {
            self.active_generations.get(&key.profile_id).copied() == Some(key.profile_generation)
        });
        if self
            .selected_profile
            .as_ref()
            .is_none_or(|selected| !profiles.iter().any(|profile| profile.id == *selected))
        {
            self.selected_profile = profiles.first().map(|profile| profile.id.clone());
        }
        self.profiles = profiles;
        for profile in &self.profiles {
            if profile.persisted.credential_mode == CredentialMode::Session
                && !profile.has_current_session_secret
            {
                self.connection_states
                    .insert(profile.id.clone(), ConnectionState::NeedsCredential);
            }
        }
        self.pending_retags.clear();
        self.config_uncertain = false;
        self.status = format!("{} profiles loaded", self.profiles.len());
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use super::{
        ConnectionFailureOutcome, ConnectionState, PostCloseState, ProfileSnapshot, UiEvent,
        UiModel, WorkspaceKey,
    };
    use crate::model::{
        CatalogLevel, CatalogPage, CatalogRequest, CatalogRetainedCounts, ConnectionProfile,
        CredentialMode, DriverAvailability, DriverKind, OperationId, OperationKind, ProfileAccess,
        ProfileEnvironment, ProfileGeneration, ProfileId, ProfileSafetyPosture, PublicCode,
        PublicSummary, QueryResult, RedisKeyFilter, RedisKeyPage, RedisScanConsistency,
        RedisScanRequest, RedisTlsConfig, RequestIdentity, ResultId, ResultProvenance,
        ResultRetentionPolicy, ResultSnapshot, SessionGeneration, TlsMode,
    };
    use crate::public_error::{PublicOperationError, SafeContext};
    use crate::service::SessionDisposition;

    fn result(elapsed_ms: u128) -> ResultSnapshot {
        let raw = QueryResult {
            columns: Vec::new(),
            rows: Vec::new(),
            affected_rows: 0,
            last_insert_id: None,
            elapsed_ms,
            truncated: false,
            backend_notices_present: false,
        };
        ResultSnapshot::retain(
            raw,
            ResultProvenance {
                result_id: ResultId(1),
                profile_id: ProfileId("mysql-local".to_owned()),
                profile_generation: ProfileGeneration(1),
                operation_id: OperationId(1),
                driver: DriverKind::MySql,
                completed_at_unix_ms: 0,
                duration_ms: elapsed_ms,
            },
            ResultRetentionPolicy::mysql(500),
        )
    }

    fn identity(operation_id: u64) -> RequestIdentity {
        RequestIdentity {
            profile_id: ProfileId("mysql-local".to_owned()),
            profile_generation: ProfileGeneration(1),
            operation_id: OperationId(operation_id),
        }
    }

    #[test]
    fn redis_session_correlation_matrix_is_fail_closed() {
        let connected = ConnectionState::Connected {
            session_generation: SessionGeneration(7),
            elapsed_ms: 0,
        };
        assert!(connected.accepts_redis_event_session(Some(SessionGeneration(7))));
        assert!(!connected.accepts_redis_event_session(Some(SessionGeneration(8))));
        assert!(!connected.accepts_redis_event_session(None));

        for state in [
            ConnectionState::Disconnected,
            ConnectionState::Pending(OperationId(1)),
            ConnectionState::NeedsCredential,
            ConnectionState::Failed {
                summary: PublicSummary::NetworkUnavailable,
            },
            ConnectionState::Closing,
        ] {
            assert!(state.accepts_redis_event_session(None));
            assert!(!state.accepts_redis_event_session(Some(SessionGeneration(7))));
        }
    }

    #[test]
    fn resource_failures_preserve_last_pages_as_stale_and_success_clears_retry() {
        let profile_id = ProfileId("mysql-local".to_owned());
        let generation = ProfileGeneration(1);
        let catalog_request = CatalogRequest::Schemas {
            identity: identity(10),
            prefix: None,
            page_token: None,
            page_size: 100,
            timeout: Duration::from_secs(5),
        };
        let redis_request = RedisScanRequest {
            identity: identity(11),
            filter: RedisKeyFilter::LiteralPrefix(String::new()),
            cursor: 0,
            count_hint: 100,
            timeout: Duration::from_secs(5),
        };
        let catalog_page = CatalogPage {
            identity: identity(10),
            level: CatalogLevel::Schemas,
            parent: None,
            nodes: Vec::new(),
            next_token: None,
            retained_counts: CatalogRetainedCounts::default(),
            retained_utf8_bytes: 0,
            truncated: false,
            stale: false,
            loaded_at: "2026-07-15T00:00:00Z".to_owned(),
        };
        let redis_page = RedisKeyPage {
            identity: identity(11),
            next_cursor: 0,
            keys: Vec::new(),
            retained_count: 0,
            skipped_oversize: 0,
            retained_bytes: 0,
            consistency: RedisScanConsistency::Weak,
            truncated: false,
            stale: false,
        };
        let mut model = UiModel {
            active_generations: [(profile_id.clone(), generation)].into(),
            ..UiModel::default()
        };
        let key = WorkspaceKey::new(profile_id.clone(), generation);
        {
            let workspace = model.workspace_mut(key.clone());
            workspace.catalog_page = Some(catalog_page.clone());
            workspace.redis_key_page = Some(redis_page.clone());
        }

        model.fold(UiEvent::CatalogPageFailed {
            request: catalog_request.clone(),
            summary: PublicSummary::PermissionDenied,
            error: PublicOperationError::new_or_internal(
                OperationKind::BrowseMySql,
                PublicSummary::PermissionDenied,
                PublicCode::None,
                &SafeContext::profile(profile_id.clone(), OperationId(10)),
            ),
            session_generation: Some(SessionGeneration(31)),
            session_disposition: Some(SessionDisposition::Keep),
        });
        model.fold(UiEvent::RedisKeysFailed {
            request: redis_request.clone(),
            error: PublicOperationError::new_or_internal(
                OperationKind::BrowseRedis,
                PublicSummary::UnsupportedFeature,
                PublicCode::None,
                &SafeContext::profile(profile_id.clone(), OperationId(11)),
            ),
            session_generation: Some(SessionGeneration(31)),
            session_disposition: Some(SessionDisposition::Keep),
            connection_outcome: ConnectionFailureOutcome::Preserve,
        });

        let workspace = model.workspace(&key).expect("profile workspace");
        assert!(
            workspace
                .catalog_page
                .as_ref()
                .is_some_and(|page| page.stale)
        );
        assert_eq!(workspace.catalog_retry.as_ref(), Some(&catalog_request));
        assert!(
            workspace
                .redis_key_page
                .as_ref()
                .is_some_and(|page| page.stale)
        );
        assert_eq!(workspace.redis_scan_retry.as_ref(), Some(&redis_request));

        model.fold(UiEvent::CatalogPageLoaded {
            page: catalog_page,
            session_generation: SessionGeneration(31),
            session_disposition: SessionDisposition::Keep,
        });
        model.fold(UiEvent::RedisKeysLoaded {
            page: redis_page,
            session_generation: SessionGeneration(31),
            session_disposition: SessionDisposition::Keep,
        });

        let workspace = model.workspace(&key).expect("profile workspace");
        assert!(
            workspace
                .catalog_page
                .as_ref()
                .is_some_and(|page| !page.stale)
        );
        assert!(workspace.catalog_retry.is_none());
        assert!(
            workspace
                .redis_key_page
                .as_ref()
                .is_some_and(|page| !page.stale)
        );
        assert!(workspace.redis_scan_retry.is_none());
        assert_eq!(
            model.connection_state(&profile_id),
            &ConnectionState::Connected {
                session_generation: SessionGeneration(31),
                elapsed_ms: 0,
            }
        );
    }

    #[test]
    fn redis_failure_fold_uses_exact_session_disposition_and_retains_typed_code() {
        let profile_id = ProfileId("redis-session-truth".to_owned());
        let generation = ProfileGeneration(8);
        let request = RedisScanRequest {
            identity: RequestIdentity::new(profile_id.clone(), generation, OperationId(31)),
            filter: RedisKeyFilter::LiteralPrefix(String::new()),
            cursor: 0,
            count_hint: 100,
            timeout: Duration::from_secs(5),
        };
        let mut model = UiModel {
            active_generations: [(profile_id.clone(), generation)].into(),
            connection_states: [(
                profile_id.clone(),
                ConnectionState::Connected {
                    session_generation: SessionGeneration(41),
                    elapsed_ms: 0,
                },
            )]
            .into(),
            ..UiModel::default()
        };

        model.fold(UiEvent::RedisKeysFailed {
            request: request.clone(),
            error: PublicOperationError::new_or_internal(
                OperationKind::BrowseRedis,
                PublicSummary::ResourceStale,
                PublicCode::None,
                &SafeContext::profile(profile_id.clone(), OperationId(31)),
            ),
            session_generation: Some(SessionGeneration(41)),
            session_disposition: Some(SessionDisposition::Keep),
            connection_outcome: ConnectionFailureOutcome::Preserve,
        });
        assert_eq!(
            model.connection_state(&profile_id),
            &ConnectionState::Connected {
                session_generation: SessionGeneration(41),
                elapsed_ms: 0,
            }
        );

        model.fold(UiEvent::RedisKeysFailed {
            request,
            error: PublicOperationError::new_or_internal(
                OperationKind::BrowseRedis,
                PublicSummary::TlsVerificationFailed,
                PublicCode::TlsHostnameMismatch,
                &SafeContext::profile(profile_id.clone(), OperationId(31)),
            ),
            session_generation: Some(SessionGeneration(41)),
            session_disposition: Some(SessionDisposition::Evict),
            connection_outcome: ConnectionFailureOutcome::Disconnected,
        });
        assert_eq!(
            model
                .workspace(&WorkspaceKey::new(profile_id.clone(), generation))
                .and_then(|workspace| workspace.redis_scan_error.as_ref())
                .map(|error| error.code),
            Some(PublicCode::TlsHostnameMismatch)
        );
        assert_eq!(
            model.connection_state(&profile_id),
            &ConnectionState::Disconnected
        );
    }

    #[test]
    fn stale_query_event_does_not_overwrite_result() {
        let profile_id = ProfileId("mysql-local".to_owned());
        let generation = ProfileGeneration(1);
        let mut model = UiModel {
            active_generations: [(profile_id.clone(), generation)].into(),
            ..UiModel::default()
        };
        let key = WorkspaceKey::new(profile_id.clone(), generation);
        {
            let workspace = model.workspace_mut(key.clone());
            workspace.pending_execute = Some(OperationId(2));
            workspace.result = Some(Arc::new(result(7)));
        }

        model.fold(UiEvent::QueryFinished {
            operation_id: OperationId(1),
            profile_id,
            profile_generation: generation,
            session_generation: SessionGeneration(1),
            result: result(99),
        });

        assert_eq!(
            model
                .workspace(&key)
                .and_then(|workspace| workspace.pending_execute),
            Some(OperationId(2))
        );
        assert_eq!(
            model
                .workspace(&key)
                .and_then(|workspace| workspace.result.as_ref())
                .map(|value| value.provenance.duration_ms),
            Some(7)
        );
    }

    #[test]
    fn predecessor_connection_closed_cannot_replace_newer_pending_or_connected_state() {
        let profile_id = ProfileId("mysql-local".to_owned());
        let generation = ProfileGeneration(1);
        let reconnect = OperationId(12);
        let predecessor = OperationId(11);
        let mut model = UiModel {
            active_generations: [(profile_id.clone(), generation)].into(),
            connection_states: [(profile_id.clone(), ConnectionState::Pending(reconnect))].into(),
            ..UiModel::default()
        };

        model.fold(UiEvent::ConnectionClosed {
            operation_id: predecessor,
            profile_id: profile_id.clone(),
            profile_generation: generation,
            post_close: PostCloseState::Disconnected,
        });
        assert_eq!(
            model.connection_state(&profile_id),
            &ConnectionState::Pending(reconnect),
            "a predecessor close must not replace the newer pending reconnect"
        );

        model.fold(UiEvent::ConnectionReady {
            operation_id: reconnect,
            profile_id: profile_id.clone(),
            profile_generation: generation,
            session_generation: SessionGeneration(4),
            elapsed_ms: 7,
        });
        let connected = ConnectionState::Connected {
            session_generation: SessionGeneration(4),
            elapsed_ms: 7,
        };
        assert_eq!(model.connection_state(&profile_id), &connected);

        model.fold(UiEvent::ConnectionClosed {
            operation_id: predecessor,
            profile_id: profile_id.clone(),
            profile_generation: generation,
            post_close: PostCloseState::NeedsCredential,
        });
        assert_eq!(
            model.connection_state(&profile_id),
            &connected,
            "a predecessor close arriving after ready must not replace connected state"
        );
    }

    #[test]
    fn non_connect_failure_outcome_cannot_replace_another_pending_connection() {
        let profile_id = ProfileId("mysql-local".to_owned());
        let generation = ProfileGeneration(1);
        let reconnect = OperationId(22);
        let predecessor = OperationId(21);

        for outcome in [
            ConnectionFailureOutcome::Unknown,
            ConnectionFailureOutcome::Disconnected,
            ConnectionFailureOutcome::NeedsCredential,
        ] {
            let mut disconnect_model = UiModel {
                active_generations: [(profile_id.clone(), generation)].into(),
                connection_states: [(profile_id.clone(), ConnectionState::Pending(reconnect))]
                    .into(),
                status: "Reconnecting…".to_owned(),
                ..UiModel::default()
            };
            disconnect_model.fold(UiEvent::OperationFailed {
                operation_id: predecessor,
                profile_id: profile_id.clone(),
                profile_generation: generation,
                session_generation: None,
                kind: OperationKind::DisconnectProfile,
                summary: PublicSummary::OperationCancelled,
                error: PublicOperationError::new_or_internal(
                    OperationKind::DisconnectProfile,
                    PublicSummary::OperationCancelled,
                    PublicCode::None,
                    &SafeContext::profile(profile_id.clone(), predecessor),
                ),
                session_disposition: None,
                connection_outcome: outcome,
            });
            assert_eq!(
                disconnect_model.connection_state(&profile_id),
                &ConnectionState::Pending(reconnect),
                "a predecessor disconnect outcome must not replace a newer pending reconnect"
            );
            assert_eq!(disconnect_model.status, "Reconnecting…");

            let execute = OperationId(23);
            let mut execute_model = UiModel {
                active_generations: [(profile_id.clone(), generation)].into(),
                connection_states: [(profile_id.clone(), ConnectionState::Pending(reconnect))]
                    .into(),
                status: "Executing…".to_owned(),
                ..UiModel::default()
            };
            let key = WorkspaceKey::new(profile_id.clone(), generation);
            execute_model.workspace_mut(key.clone()).pending_execute = Some(execute);
            execute_model.fold(UiEvent::OperationFailed {
                operation_id: execute,
                profile_id: profile_id.clone(),
                profile_generation: generation,
                session_generation: None,
                kind: OperationKind::ExecuteRead,
                summary: PublicSummary::OperationCancelled,
                error: PublicOperationError::new_or_internal(
                    OperationKind::ExecuteRead,
                    PublicSummary::OperationCancelled,
                    PublicCode::None,
                    &SafeContext::profile(profile_id.clone(), execute),
                ),
                session_disposition: None,
                connection_outcome: outcome,
            });
            assert_eq!(
                execute_model.connection_state(&profile_id),
                &ConnectionState::Pending(reconnect),
                "a correlated execute terminal may clear execute state but not another operation's pending connection"
            );
            assert!(
                execute_model
                    .workspace(&key)
                    .is_some_and(|workspace| workspace.pending_execute.is_none())
            );
            assert_eq!(
                execute_model.status,
                PublicSummary::OperationCancelled.message()
            );

            let mut matching = UiModel {
                active_generations: [(profile_id.clone(), generation)].into(),
                connection_states: [(profile_id.clone(), ConnectionState::Pending(reconnect))]
                    .into(),
                ..UiModel::default()
            };
            matching.fold(UiEvent::OperationFailed {
                operation_id: reconnect,
                profile_id: profile_id.clone(),
                profile_generation: generation,
                session_generation: None,
                kind: OperationKind::DisconnectProfile,
                summary: PublicSummary::OperationCancelled,
                error: PublicOperationError::new_or_internal(
                    OperationKind::DisconnectProfile,
                    PublicSummary::OperationCancelled,
                    PublicCode::None,
                    &SafeContext::profile(profile_id.clone(), reconnect),
                ),
                session_disposition: None,
                connection_outcome: outcome,
            });
            let expected = match outcome {
                ConnectionFailureOutcome::Unknown | ConnectionFailureOutcome::Disconnected => {
                    ConnectionState::Disconnected
                }
                ConnectionFailureOutcome::NeedsCredential => ConnectionState::NeedsCredential,
                ConnectionFailureOutcome::Preserve => unreachable!("fixture excludes Preserve"),
            };
            assert_eq!(
                matching.connection_state(&profile_id),
                &expected,
                "the matching operation outcome may update its own pending state"
            );
        }
    }

    #[test]
    fn refreshed_changed_profile_clears_stale_connection_state() {
        let profile_id = ProfileId("mysql-local".to_owned());
        let original = profile(3306);
        let mut model = UiModel {
            profiles: vec![original],
            connection_states: [(
                profile_id.clone(),
                ConnectionState::Connected {
                    session_generation: SessionGeneration(1),
                    elapsed_ms: 5,
                },
            )]
            .into(),
            ..UiModel::default()
        };

        model.fold(UiEvent::ProfilesLoaded {
            operation_id: OperationId(1),
            profiles: vec![profile(3307)],
            config: Default::default(),
        });

        assert_eq!(
            model.connection_state(&profile_id),
            &ConnectionState::Disconnected
        );
    }

    fn profile(port: u16) -> ProfileSnapshot {
        let persisted = ConnectionProfile {
            id: "mysql-local".to_owned(),
            name: "MySQL".to_owned(),
            driver: DriverKind::MySql,
            host: "127.0.0.1".to_owned(),
            port,
            database: None,
            username: None,
            safety: ProfileSafetyPosture::new(
                ProfileEnvironment::Development,
                ProfileAccess::ReadWrite,
            ),
            tls: TlsMode::Disabled,
            credential_mode: CredentialMode::None,
            secret_env: None,
            redis_tls: RedisTlsConfig::default(),
        };
        ProfileSnapshot {
            id: ProfileId(persisted.id.clone()),
            generation: ProfileGeneration(1),
            name: persisted.name.clone(),
            driver: persisted.driver,
            endpoint: persisted.redacted_endpoint(),
            database: None,
            availability: DriverAvailability::Ready,
            planned_reason: None,
            has_current_session_secret: false,
            environment_availability: None,
            persisted,
        }
    }
}
