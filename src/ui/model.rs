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
    DriverKind, ExportFormat, MAX_PROFILE_RESULT_BYTES, OperationId, OperationKind,
    OverwritePolicy, ProfileGeneration, ProfileId, ProfileInstanceId, PublicSummary, QueryLanguage,
    RedisKeyInspectRequest, RedisKeyPage, RedisScanRequest, RedisValuePreview, ResultId,
    ResultSnapshot, SessionGeneration,
};
use crate::public_error::PublicOperationError;
use crate::secrets::EnvironmentAvailability;
use crate::service::SessionDisposition;
use crate::workspace::{
    EditorTabSnapshot, ProfileWorkspaceSnapshot, WorkspaceGeometrySnapshot, WorkspaceHistoryEntry,
    WorkspaceIoKind, WorkspaceLanguage, WorkspaceReadOnlyReason, WorkspaceSnapshotError,
    WorkspaceStoreMode, WorkspaceStoreWarning,
};

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

#[derive(Clone, PartialEq, Eq, Hash)]
pub struct WorkspaceIdentity {
    profile_id: ProfileId,
    profile_generation: ProfileGeneration,
    instance_id: ProfileInstanceId,
}

impl WorkspaceIdentity {
    pub fn new(
        profile_id: ProfileId,
        profile_generation: ProfileGeneration,
        instance_id: ProfileInstanceId,
    ) -> Self {
        Self {
            profile_id,
            profile_generation,
            instance_id,
        }
    }

    pub fn profile_id(&self) -> &ProfileId {
        &self.profile_id
    }

    pub const fn profile_generation(&self) -> ProfileGeneration {
        self.profile_generation
    }

    pub const fn instance_id(&self) -> ProfileInstanceId {
        self.instance_id
    }
}

impl fmt::Debug for WorkspaceIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkspaceIdentity")
            .field("profile_id", &"<redacted>")
            .field("profile_generation", &self.profile_generation)
            .field("instance_id", &"<redacted>")
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkspaceAction {
    Load,
    Commit,
    Clear,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkspaceFailureCode {
    Unavailable,
    Busy,
    Stale,
    InvalidIdentity,
    ReadOnly(WorkspaceReadOnlyReason),
    InvalidSnapshot,
    LimitExceeded,
    UnsafeStorage,
    Corrupt,
    UnsupportedVersion,
    ExternalChange,
    DurabilityUnknown,
    RecoveryRequired,
    Io(WorkspaceIoKind),
    Internal,
}

pub const MAX_EDITOR_TABS: usize = 20;
pub const MAX_RESULT_TABS_PER_EDITOR: usize = 10;
pub const MAX_RESULT_TABS_PER_PROFILE: usize = 40;
const MAX_EDITOR_TAB_TITLE_BYTES: usize = 120;
const MAX_EDITOR_TAB_DATABASE_BYTES: usize = 1_024;
pub const MAX_EDITOR_TAB_TEXT_BYTES: usize = 256 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EditorTabId(pub u64);

#[derive(Clone)]
pub struct EditorTab {
    id: EditorTabId,
    title: String,
    language: QueryLanguage,
    text: String,
    database: Option<String>,
    cursor_character_index: usize,
    selection_character_range: Option<Range<usize>>,
    dirty: bool,
}

impl std::fmt::Debug for EditorTab {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EditorTab")
            .field("id", &self.id)
            .field("title", &"<redacted>")
            .field("language", &self.language)
            .field("text", &"<redacted>")
            .field("database", &self.database.as_ref().map(|_| "<configured>"))
            .field("cursor_character_index", &self.cursor_character_index)
            .field("selection_character_range", &self.selection_character_range)
            .field("dirty", &self.dirty)
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

    pub fn database(&self) -> Option<&str> {
        self.database.as_deref()
    }

    pub const fn cursor_character_index(&self) -> usize {
        self.cursor_character_index
    }

    pub fn selection_character_range(&self) -> Option<Range<usize>> {
        self.selection_character_range.clone()
    }

    pub const fn is_dirty(&self) -> bool {
        self.dirty
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditorTabError {
    LimitReached,
    NotFound,
    InvalidTitle,
    TextTooLarge,
    DatabaseBindingTooLarge,
    InvalidCursor,
    InvalidSelection,
    InvalidPosition,
    IdExhausted,
    RevisionExhausted,
    Dirty,
}

impl std::fmt::Display for EditorTabError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::LimitReached => "close a tab before creating another",
            Self::NotFound => "editor tab is no longer available",
            Self::InvalidTitle => "tab title must be 1 to 120 UTF-8 bytes",
            Self::TextTooLarge => "editor tab text exceeds 256 KiB",
            Self::DatabaseBindingTooLarge => "editor database binding exceeds 1024 UTF-8 bytes",
            Self::InvalidCursor => "editor cursor is outside the tab source",
            Self::InvalidSelection => "editor selection is outside the tab source",
            Self::InvalidPosition => "editor tab position is outside the tab strip",
            Self::IdExhausted => "editor tab identity space is exhausted",
            Self::RevisionExhausted => "workspace revision space is exhausted",
            Self::Dirty => "discard the unsaved query before closing this tab",
        })
    }
}

impl std::error::Error for EditorTabError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum WorkspaceModelError {
    #[error("workspace persistence requires a classified version 3 profile")]
    UnclassifiedProfile,
    #[error("workspace persistence identity is not configured")]
    PersistenceNotConfigured,
    #[error(transparent)]
    Editor(#[from] EditorTabError),
    #[error(transparent)]
    Snapshot(#[from] WorkspaceSnapshotError),
}

#[derive(Clone, PartialEq)]
pub struct ProfileWorkspacePersistence {
    instance_id: ProfileInstanceId,
    profile_id: ProfileId,
    persistence_enabled: bool,
    geometry: WorkspaceGeometrySnapshot,
    history: Vec<WorkspaceHistoryEntry>,
}

impl std::fmt::Debug for ProfileWorkspacePersistence {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProfileWorkspacePersistence")
            .field("instance_id", &"<redacted>")
            .field("profile_id", &"<redacted>")
            .field("persistence_enabled", &self.persistence_enabled)
            .field("geometry", &self.geometry)
            .field("history_count", &self.history.len())
            .finish()
    }
}

impl ProfileWorkspacePersistence {
    pub fn for_classified_profile(
        profile: &ConnectionProfile,
        persistence_enabled: bool,
        geometry: WorkspaceGeometrySnapshot,
        history: Vec<WorkspaceHistoryEntry>,
    ) -> Result<Self, WorkspaceModelError> {
        let instance_id = profile
            .safety
            .instance_id()
            .ok_or(WorkspaceModelError::UnclassifiedProfile)?;
        Ok(Self {
            instance_id,
            profile_id: ProfileId(profile.id.clone()),
            persistence_enabled,
            geometry,
            history,
        })
    }

    pub const fn instance_id(&self) -> ProfileInstanceId {
        self.instance_id
    }

    pub fn profile_id(&self) -> &ProfileId {
        &self.profile_id
    }

    pub const fn persistence_enabled(&self) -> bool {
        self.persistence_enabled
    }

    pub const fn geometry(&self) -> WorkspaceGeometrySnapshot {
        self.geometry
    }

    pub fn history(&self) -> &[WorkspaceHistoryEntry] {
        &self.history
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ResultAreaTab {
    #[default]
    Results,
    History,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ResultTabId(pub u64);

#[derive(Clone)]
pub struct ResultTab {
    id: ResultTabId,
    origin_editor_tab_id: Option<EditorTabId>,
    content: ResultTabContent,
}

#[derive(Clone)]
enum ResultTabContent {
    Result {
        snapshot: Arc<ResultSnapshot>,
        view: ResultViewState,
    },
    Error(PublicOperationError),
}

impl ResultTabContent {
    fn surface(
        &self,
    ) -> (
        Option<Arc<ResultSnapshot>>,
        ResultViewState,
        Option<PublicOperationError>,
    ) {
        match self {
            Self::Result { snapshot, view } => (Some(snapshot.clone()), view.clone(), None),
            Self::Error(error) => (None, ResultViewState::default(), Some(error.clone())),
        }
    }
}

impl std::fmt::Debug for ResultTab {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ResultTab")
            .field("id", &self.id)
            .field("origin_editor_tab_id", &self.origin_editor_tab_id)
            .field(
                "content",
                &match &self.content {
                    ResultTabContent::Result { .. } => "<retained-result>",
                    ResultTabContent::Error(_) => "<retained-error>",
                },
            )
            .finish_non_exhaustive()
    }
}

impl ResultTab {
    pub const fn id(&self) -> ResultTabId {
        self.id
    }

    pub fn snapshot(&self) -> Option<&Arc<ResultSnapshot>> {
        match &self.content {
            ResultTabContent::Result { snapshot, .. } => Some(snapshot),
            ResultTabContent::Error(_) => None,
        }
    }

    pub fn error(&self) -> Option<&PublicOperationError> {
        match &self.content {
            ResultTabContent::Result { .. } => None,
            ResultTabContent::Error(error) => Some(error),
        }
    }

    pub const fn origin_editor_tab_id(&self) -> Option<EditorTabId> {
        self.origin_editor_tab_id
    }

    pub fn title(&self) -> String {
        match &self.content {
            ResultTabContent::Result { snapshot, .. } => {
                format!("Result {}", snapshot.provenance.result_id.0)
            }
            ResultTabContent::Error(_) => format!("Error {}", self.id.0),
        }
    }

    pub const fn is_error(&self) -> bool {
        matches!(&self.content, ResultTabContent::Error(_))
    }

    pub(crate) const fn can_close(&self) -> bool {
        match &self.content {
            ResultTabContent::Result { view, .. } => !view.has_pending_export(),
            ResultTabContent::Error(_) => true,
        }
    }

    fn retained_bytes(&self) -> usize {
        self.snapshot()
            .map_or(0, |snapshot| snapshot.retained_bytes)
    }

    fn surface(
        &self,
    ) -> (
        Option<Arc<ResultSnapshot>>,
        ResultViewState,
        Option<PublicOperationError>,
    ) {
        self.content.surface()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResultTabError {
    NotFound,
    Busy,
    CapacityProtected,
}

impl std::fmt::Display for ResultTabError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::NotFound => "result tab is no longer available",
            Self::Busy => "cancel the active result operation before closing this tab",
            Self::CapacityProtected => {
                "result capacity is occupied by the selected or active result"
            }
        })
    }
}

impl std::error::Error for ResultTabError {}

#[derive(Clone)]
pub struct ProfileWorkspace {
    pub editor_text: String,
    pub editor_database: Option<String>,
    pub caret_character_index: usize,
    pub selection_character_range: Option<Range<usize>>,
    pub row_limit: String,
    pub timeout_seconds: String,
    pub pending_execute: Option<OperationId>,
    pending_execute_editor_tab_id: Option<EditorTabId>,
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
    persistence: Option<ProfileWorkspacePersistence>,
    revision: u64,
    saved_revision: Option<u64>,
    result_area_tab: ResultAreaTab,
    result_tabs: Vec<ResultTab>,
    selected_result_tab: Option<ResultTabId>,
    next_result_tab_id: u64,
}

impl std::fmt::Debug for ProfileWorkspace {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProfileWorkspace")
            .field("editor_text", &"<redacted>")
            .field(
                "editor_database",
                &self.editor_database.as_ref().map(|_| "<configured>"),
            )
            .field("editor_tabs", &self.editor_tabs)
            .field("selected_editor_tab", &self.selected_editor_tab)
            .field("persistence", &self.persistence)
            .field("revision", &self.revision)
            .field("saved_revision", &self.saved_revision)
            .field("result_area_tab", &self.result_area_tab)
            .field("result_tabs", &self.result_tabs)
            .field("selected_result_tab", &self.selected_result_tab)
            .field("pending_execute", &self.pending_execute)
            .field("result", &self.result.as_ref().map(|_| "<retained>"))
            .finish_non_exhaustive()
    }
}

impl Default for ProfileWorkspace {
    fn default() -> Self {
        Self {
            editor_text: String::new(),
            editor_database: None,
            caret_character_index: 0,
            selection_character_range: None,
            row_limit: String::new(),
            timeout_seconds: String::new(),
            pending_execute: None,
            pending_execute_editor_tab_id: None,
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
            persistence: None,
            revision: 0,
            saved_revision: None,
            result_area_tab: ResultAreaTab::Results,
            result_tabs: Vec::new(),
            selected_result_tab: None,
            next_result_tab_id: 1,
        }
    }
}

impl ProfileWorkspace {
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    pub const fn saved_revision(&self) -> Option<u64> {
        self.saved_revision
    }

    pub fn is_saved(&self) -> bool {
        self.saved_revision == Some(self.revision)
    }

    pub fn mark_saved_if_revision(&mut self, expected_revision: u64) -> bool {
        if self.persistence.is_none() || self.revision != expected_revision {
            return false;
        }
        self.saved_revision = Some(expected_revision);
        if self
            .persistence
            .as_ref()
            .is_some_and(|persistence| persistence.persistence_enabled)
        {
            for tab in &mut self.editor_tabs {
                tab.dirty = false;
            }
        }
        true
    }

    pub fn persistence(&self) -> Option<&ProfileWorkspacePersistence> {
        self.persistence.as_ref()
    }

    pub fn bind_persistence(
        &mut self,
        persistence: ProfileWorkspacePersistence,
    ) -> Result<(), WorkspaceModelError> {
        if self.persistence.as_ref() == Some(&persistence) {
            return Ok(());
        }
        self.bump_revision()?;
        self.persistence = Some(persistence);
        Ok(())
    }

    pub fn set_persistence_enabled(
        &mut self,
        persistence_enabled: bool,
    ) -> Result<(), WorkspaceModelError> {
        let Some(current) = self.persistence.as_ref() else {
            return Err(WorkspaceModelError::PersistenceNotConfigured);
        };
        if current.persistence_enabled == persistence_enabled {
            return Ok(());
        }
        self.bump_revision()?;
        let Some(persistence) = self.persistence.as_mut() else {
            return Err(WorkspaceModelError::PersistenceNotConfigured);
        };
        persistence.persistence_enabled = persistence_enabled;
        if !persistence_enabled {
            persistence.history.clear();
        }
        Ok(())
    }

    pub fn set_persistence_geometry(
        &mut self,
        geometry: WorkspaceGeometrySnapshot,
    ) -> Result<(), WorkspaceModelError> {
        let Some(current) = self.persistence.as_ref() else {
            return Err(WorkspaceModelError::PersistenceNotConfigured);
        };
        if current.geometry == geometry {
            return Ok(());
        }
        self.bump_revision()?;
        let Some(persistence) = self.persistence.as_mut() else {
            return Err(WorkspaceModelError::PersistenceNotConfigured);
        };
        persistence.geometry = geometry;
        Ok(())
    }

    pub fn replace_persistence_history(
        &mut self,
        history: Vec<WorkspaceHistoryEntry>,
    ) -> Result<(), WorkspaceModelError> {
        let Some(current) = self.persistence.as_ref() else {
            return Err(WorkspaceModelError::PersistenceNotConfigured);
        };
        if current.history == history {
            return Ok(());
        }
        self.bump_revision()?;
        let Some(persistence) = self.persistence.as_mut() else {
            return Err(WorkspaceModelError::PersistenceNotConfigured);
        };
        persistence.history = history;
        Ok(())
    }

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

    fn bump_revision(&mut self) -> Result<(), EditorTabError> {
        self.revision = self
            .revision
            .checked_add(1)
            .ok_or(EditorTabError::RevisionExhausted)?;
        Ok(())
    }

    pub fn create_editor_tab(
        &mut self,
        language: QueryLanguage,
        title: impl Into<String>,
        text: impl Into<String>,
    ) -> Result<EditorTabId, EditorTabError> {
        let text = text.into();
        let cursor_character_index = text.chars().count();
        self.create_editor_tab_with_state(language, title, text, None, cursor_character_index, None)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_editor_tab_with_state(
        &mut self,
        language: QueryLanguage,
        title: impl Into<String>,
        text: impl Into<String>,
        database: Option<String>,
        cursor_character_index: usize,
        selection_character_range: Option<Range<usize>>,
    ) -> Result<EditorTabId, EditorTabError> {
        if self.editor_tabs.len() >= MAX_EDITOR_TABS {
            return Err(EditorTabError::LimitReached);
        }
        let title = title.into();
        validate_editor_tab_title(&title)?;
        let text = text.into();
        validate_editor_tab_state(
            &text,
            database.as_deref(),
            cursor_character_index,
            selection_character_range.as_ref(),
        )?;
        self.sync_selected_editor_tab_from_surface()?;
        let had_editor_tabs = !self.editor_tabs.is_empty();
        let id = self.next_editor_tab_id()?;
        self.bump_revision()?;
        self.next_editor_tab_id = id.0.checked_add(1).unwrap_or(id.0);
        let dirty = !text.is_empty();
        self.editor_tabs.push(EditorTab {
            id,
            title,
            language,
            text,
            database,
            cursor_character_index,
            selection_character_range,
            dirty,
        });
        self.selected_editor_tab = Some(id);
        self.load_editor_tab_into_surface(id);
        if had_editor_tabs {
            self.activate_result_for_editor(Some(id));
        }
        Ok(id)
    }

    pub fn rename_editor_tab(
        &mut self,
        tab_id: EditorTabId,
        title: impl Into<String>,
    ) -> Result<(), EditorTabError> {
        let title = title.into();
        validate_editor_tab_title(&title)?;
        let Some(index) = self.editor_tabs.iter().position(|tab| tab.id == tab_id) else {
            return Err(EditorTabError::NotFound);
        };
        if self.editor_tabs[index].title != title {
            self.bump_revision()?;
            self.editor_tabs[index].title = title;
            self.editor_tabs[index].dirty = true;
        }
        Ok(())
    }

    pub fn duplicate_editor_tab(
        &mut self,
        tab_id: EditorTabId,
    ) -> Result<EditorTabId, EditorTabError> {
        self.sync_selected_editor_tab_from_surface()?;
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
        self.create_editor_tab_with_state(
            source.language,
            title,
            source.text,
            source.database,
            source.cursor_character_index,
            source.selection_character_range,
        )
    }

    pub fn select_editor_tab(&mut self, tab_id: EditorTabId) -> Result<(), EditorTabError> {
        if self.editor_tab(tab_id).is_none() {
            return Err(EditorTabError::NotFound);
        }
        self.sync_selected_editor_tab_from_surface()?;
        if self.selected_editor_tab != Some(tab_id) {
            self.bump_revision()?;
            self.selected_editor_tab = Some(tab_id);
            self.load_editor_tab_into_surface(tab_id);
            self.activate_result_for_editor(Some(tab_id));
        }
        Ok(())
    }

    pub fn reorder_editor_tab(
        &mut self,
        tab_id: EditorTabId,
        target_index: usize,
    ) -> Result<(), EditorTabError> {
        if target_index >= self.editor_tabs.len() {
            return Err(EditorTabError::InvalidPosition);
        }
        let Some(source_index) = self.editor_tabs.iter().position(|tab| tab.id == tab_id) else {
            return Err(EditorTabError::NotFound);
        };
        self.sync_selected_editor_tab_from_surface()?;
        if source_index != target_index {
            self.bump_revision()?;
            let tab = self.editor_tabs.remove(source_index);
            self.editor_tabs.insert(target_index, tab);
        }
        Ok(())
    }

    pub fn close_editor_tab(&mut self, tab_id: EditorTabId) -> Result<(), EditorTabError> {
        let Some(index) = self.editor_tabs.iter().position(|tab| tab.id == tab_id) else {
            return Err(EditorTabError::NotFound);
        };
        self.sync_selected_editor_tab_from_surface()?;
        if self.editor_tabs.get(index).is_some_and(EditorTab::is_dirty) {
            return Err(EditorTabError::Dirty);
        }
        self.bump_revision()?;
        self.remove_editor_tab(index, tab_id);
        Ok(())
    }

    pub fn discard_editor_tab(&mut self, tab_id: EditorTabId) -> Result<(), EditorTabError> {
        let Some(index) = self.editor_tabs.iter().position(|tab| tab.id == tab_id) else {
            return Err(EditorTabError::NotFound);
        };
        self.bump_revision()?;
        self.remove_editor_tab(index, tab_id);
        Ok(())
    }

    fn remove_editor_tab(&mut self, index: usize, tab_id: EditorTabId) {
        let closing_selected = self.selected_editor_tab == Some(tab_id);
        self.editor_tabs.remove(index);
        if closing_selected {
            self.selected_editor_tab = self
                .editor_tabs
                .get(index.min(self.editor_tabs.len().saturating_sub(1)))
                .map(EditorTab::id);
            if let Some(selected) = self.selected_editor_tab {
                self.load_editor_tab_into_surface(selected);
                self.activate_result_for_editor(Some(selected));
            } else {
                self.editor_text.clear();
                self.editor_database = None;
                self.caret_character_index = 0;
                self.selection_character_range = None;
                self.activate_result_for_editor(None);
            }
        }
    }

    pub fn sync_selected_editor_tab_from_surface(&mut self) -> Result<(), EditorTabError> {
        validate_editor_tab_state(
            &self.editor_text,
            self.editor_database.as_deref(),
            self.caret_character_index,
            self.selection_character_range.as_ref(),
        )?;
        let Some(selected) = self.selected_editor_tab else {
            return Ok(());
        };
        let Some(index) = self.editor_tabs.iter().position(|tab| tab.id == selected) else {
            return Ok(());
        };
        let content_changed = self.editor_tabs[index].text != self.editor_text
            || self.editor_tabs[index].database != self.editor_database;
        let durable_changed = content_changed
            || self.editor_tabs[index].cursor_character_index != self.caret_character_index
            || self.editor_tabs[index].selection_character_range != self.selection_character_range;
        if durable_changed {
            self.bump_revision()?;
        }
        let tab = &mut self.editor_tabs[index];
        if content_changed {
            tab.text.clone_from(&self.editor_text);
            tab.database.clone_from(&self.editor_database);
            tab.dirty = true;
        }
        tab.cursor_character_index = self.caret_character_index;
        tab.selection_character_range = self.selection_character_range.clone();
        Ok(())
    }

    fn next_editor_tab_id(&self) -> Result<EditorTabId, EditorTabError> {
        let id = EditorTabId(self.next_editor_tab_id.max(1));
        if self.editor_tab(id).is_some() {
            return Err(EditorTabError::IdExhausted);
        }
        Ok(id)
    }

    pub fn to_persistence_snapshot(
        &mut self,
    ) -> Result<ProfileWorkspaceSnapshot, WorkspaceModelError> {
        self.sync_selected_editor_tab_from_surface()?;
        let persistence = self
            .persistence
            .clone()
            .ok_or(WorkspaceModelError::PersistenceNotConfigured)?;
        let (editor_tabs, selected_editor_tab_id, history) = if persistence.persistence_enabled {
            let editor_tabs = self
                .editor_tabs
                .iter()
                .map(|tab| {
                    EditorTabSnapshot::new(
                        tab.id.0,
                        tab.title.clone(),
                        workspace_language(tab.language),
                        tab.text.clone(),
                        tab.database.as_deref(),
                        tab.cursor_character_index,
                        tab.selection_character_range.clone(),
                    )
                })
                .collect::<Result<Vec<_>, _>>()?;
            (
                editor_tabs,
                self.selected_editor_tab.map(|tab_id| tab_id.0),
                persistence.history.clone(),
            )
        } else {
            (Vec::new(), None, Vec::new())
        };
        ProfileWorkspaceSnapshot::new(
            persistence.instance_id,
            persistence.profile_id.clone(),
            persistence.persistence_enabled,
            editor_tabs,
            selected_editor_tab_id,
            persistence.geometry,
            history,
        )
        .map_err(WorkspaceModelError::from)
    }

    pub fn from_persistence_snapshot(
        snapshot: ProfileWorkspaceSnapshot,
    ) -> Result<Self, WorkspaceModelError> {
        let snapshot = ProfileWorkspaceSnapshot::new(
            snapshot.instance_id(),
            snapshot.profile_id().clone(),
            snapshot.persistence_enabled(),
            snapshot.editor_tabs().to_vec(),
            snapshot.selected_editor_tab_id(),
            snapshot.geometry(),
            snapshot.history().to_vec(),
        )?;
        let mut workspace = Self {
            row_limit: "500".to_owned(),
            timeout_seconds: "30".to_owned(),
            ..Self::default()
        };
        workspace.editor_tabs = snapshot
            .editor_tabs()
            .iter()
            .map(|tab| EditorTab {
                id: EditorTabId(tab.id()),
                title: tab.title().to_owned(),
                language: query_language(tab.language()),
                text: tab.source().to_owned(),
                database: tab.database().map(str::to_owned),
                cursor_character_index: tab.cursor_character_index(),
                selection_character_range: tab.selection_character_range(),
                dirty: false,
            })
            .collect();
        workspace.selected_editor_tab = snapshot.selected_editor_tab_id().map(EditorTabId);
        workspace.next_editor_tab_id = workspace
            .editor_tabs
            .iter()
            .map(|tab| tab.id.0)
            .max()
            .map_or(1, |id| id.checked_add(1).unwrap_or(id));
        if let Some(selected) = workspace.selected_editor_tab {
            workspace.load_editor_tab_into_surface(selected);
        }
        workspace.persistence = Some(ProfileWorkspacePersistence {
            instance_id: snapshot.instance_id(),
            profile_id: snapshot.profile_id().clone(),
            persistence_enabled: snapshot.persistence_enabled(),
            geometry: snapshot.geometry(),
            history: snapshot.history().to_vec(),
        });
        workspace.saved_revision = Some(workspace.revision);
        Ok(workspace)
    }

    fn retain_editor_context_for_profile_retag(&mut self) {
        let _ = self.sync_selected_editor_tab_from_surface();
        self.pending_execute = None;
        self.pending_execute_editor_tab_id = None;
        self.result = None;
        self.result_view = ResultViewState::default();
        self.error = None;
        self.catalog_page = None;
        self.catalog_retry = None;
        self.catalog_error = None;
        self.redis_key_page = None;
        self.redis_scan_retry = None;
        self.redis_scan_error = None;
        self.redis_value_preview = None;
        self.redis_inspect_retry = None;
        self.redis_inspect_error = None;
        self.result_area_tab = ResultAreaTab::Results;
        self.result_tabs.clear();
        self.selected_result_tab = None;
        self.next_result_tab_id = 1;
    }

    pub const fn result_area_tab(&self) -> ResultAreaTab {
        self.result_area_tab
    }

    pub const fn select_result_area_tab(&mut self, tab: ResultAreaTab) {
        self.result_area_tab = tab;
    }

    pub fn result_tabs(&self) -> &[ResultTab] {
        &self.result_tabs
    }

    pub fn result_tabs_for_editor(
        &self,
        editor_tab_id: Option<EditorTabId>,
    ) -> impl DoubleEndedIterator<Item = &ResultTab> {
        self.result_tabs.iter().filter(move |tab| {
            tab.origin_editor_tab_id.is_none() || tab.origin_editor_tab_id == editor_tab_id
        })
    }

    pub fn retained_result_bytes(&self) -> usize {
        self.result_tabs.iter().fold(0_usize, |total, tab| {
            total.saturating_add(tab.retained_bytes())
        })
    }

    pub(crate) fn begin_execute(
        &mut self,
        operation_id: OperationId,
        editor_tab_id: Option<EditorTabId>,
    ) {
        self.pending_execute = Some(operation_id);
        self.pending_execute_editor_tab_id = editor_tab_id;
    }

    pub const fn selected_result_tab_id(&self) -> Option<ResultTabId> {
        self.selected_result_tab
    }

    pub fn selected_result_tab(&self) -> Option<&ResultTab> {
        let selected = self.selected_result_tab?;
        self.result_tabs.iter().find(|tab| tab.id == selected)
    }

    pub fn append_result_tab(
        &mut self,
        snapshot: Arc<ResultSnapshot>,
    ) -> Result<ResultTabId, ResultTabError> {
        self.append_result_tab_for_editor(snapshot, None)
    }

    pub(crate) fn append_result_tab_for_editor(
        &mut self,
        snapshot: Arc<ResultSnapshot>,
        origin_editor_tab_id: Option<EditorTabId>,
    ) -> Result<ResultTabId, ResultTabError> {
        let mut view = ResultViewState::default();
        view.reset_for(snapshot.provenance.result_id);
        self.append_output_tab_for_editor(
            ResultTabContent::Result { snapshot, view },
            origin_editor_tab_id,
        )
    }

    pub(crate) fn append_error_tab_for_editor(
        &mut self,
        error: PublicOperationError,
        origin_editor_tab_id: Option<EditorTabId>,
    ) -> Result<ResultTabId, ResultTabError> {
        self.append_output_tab_for_editor(ResultTabContent::Error(error), origin_editor_tab_id)
    }

    fn append_output_tab_for_editor(
        &mut self,
        content: ResultTabContent,
        origin_editor_tab_id: Option<EditorTabId>,
    ) -> Result<ResultTabId, ResultTabError> {
        self.sync_selected_result_tab_from_surface();
        let mut evictions = vec![false; self.result_tabs.len()];
        let mut retained_count = self.result_tabs.len();
        let mut retained_bytes = self.retained_result_bytes();
        let new_retained_bytes = match &content {
            ResultTabContent::Result { snapshot, .. } => snapshot.retained_bytes,
            ResultTabContent::Error(_) => 0,
        };

        if let Some(editor_tab_id) = origin_editor_tab_id {
            let editor_result_count = self
                .result_tabs
                .iter()
                .filter(|tab| tab.origin_editor_tab_id == Some(editor_tab_id))
                .count();
            let mut required_evictions = editor_result_count
                .saturating_add(1)
                .saturating_sub(MAX_RESULT_TABS_PER_EDITOR);
            for (index, tab) in self.result_tabs.iter().enumerate() {
                if required_evictions == 0 {
                    break;
                }
                if tab.origin_editor_tab_id == Some(editor_tab_id)
                    && Some(tab.id) != self.selected_result_tab
                    && tab.can_close()
                {
                    evictions[index] = true;
                    retained_count = retained_count.saturating_sub(1);
                    retained_bytes = retained_bytes.saturating_sub(tab.retained_bytes());
                    required_evictions -= 1;
                }
            }
            if required_evictions > 0 {
                return Err(ResultTabError::CapacityProtected);
            }
        }

        while retained_count.saturating_add(1) > MAX_RESULT_TABS_PER_PROFILE
            || retained_bytes.saturating_add(new_retained_bytes) > MAX_PROFILE_RESULT_BYTES
        {
            let Some((index, tab)) = self.result_tabs.iter().enumerate().find(|(index, tab)| {
                !evictions[*index] && Some(tab.id) != self.selected_result_tab && tab.can_close()
            }) else {
                return Err(ResultTabError::CapacityProtected);
            };
            evictions[index] = true;
            retained_count = retained_count.saturating_sub(1);
            retained_bytes = retained_bytes.saturating_sub(tab.retained_bytes());
        }

        for index in (0..evictions.len()).rev() {
            if evictions[index] {
                self.result_tabs.remove(index);
            }
        }
        let id = ResultTabId(self.next_result_tab_id.max(1));
        self.next_result_tab_id = id.0.saturating_add(1);
        let activate =
            origin_editor_tab_id.is_none() || origin_editor_tab_id == self.selected_editor_tab;
        let surface = content.surface();
        self.result_tabs.push(ResultTab {
            id,
            origin_editor_tab_id,
            content,
        });
        if activate {
            self.selected_result_tab = Some(id);
            self.result = surface.0;
            self.result_view = surface.1;
            self.error = surface.2;
            self.result_area_tab = ResultAreaTab::Results;
        }
        Ok(id)
    }

    fn activate_result_for_editor(&mut self, editor_tab_id: Option<EditorTabId>) {
        self.sync_selected_result_tab_from_surface();
        let replacement = self
            .result_tabs_for_editor(editor_tab_id)
            .next_back()
            .map(|tab| (tab.id, tab.surface()));
        if let Some((id, surface)) = replacement {
            self.selected_result_tab = Some(id);
            self.result = surface.0;
            self.result_view = surface.1;
            self.error = surface.2;
        } else {
            self.selected_result_tab = None;
            self.result = None;
            self.result_view = ResultViewState::default();
            self.error = None;
        }
    }

    pub fn select_result_tab(&mut self, tab_id: ResultTabId) -> Result<(), ResultTabError> {
        self.sync_selected_result_tab_from_surface();
        let Some(tab) = self.result_tabs.iter().find(|tab| tab.id == tab_id) else {
            return Err(ResultTabError::NotFound);
        };
        let surface = tab.surface();
        self.selected_result_tab = Some(tab_id);
        self.result = surface.0;
        self.result_view = surface.1;
        self.error = surface.2;
        self.result_area_tab = ResultAreaTab::Results;
        Ok(())
    }

    pub fn close_result_tab(&mut self, tab_id: ResultTabId) -> Result<(), ResultTabError> {
        self.sync_selected_result_tab_from_surface();
        let Some(index) = self.result_tabs.iter().position(|tab| tab.id == tab_id) else {
            return Err(ResultTabError::NotFound);
        };
        if !self.result_tabs[index].can_close() {
            return Err(ResultTabError::Busy);
        }
        let was_selected = self.selected_result_tab == Some(tab_id);
        self.result_tabs.remove(index);
        if !was_selected {
            return Ok(());
        }

        let selected_editor_tab = self.selected_editor_tab;
        let replacement = self
            .result_tabs
            .iter()
            .skip(index)
            .find(|tab| {
                tab.origin_editor_tab_id.is_none()
                    || tab.origin_editor_tab_id == selected_editor_tab
            })
            .or_else(|| {
                self.result_tabs[..index.min(self.result_tabs.len())]
                    .iter()
                    .rev()
                    .find(|tab| {
                        tab.origin_editor_tab_id.is_none()
                            || tab.origin_editor_tab_id == selected_editor_tab
                    })
            })
            .map(|tab| (tab.id, tab.surface()));
        if let Some((id, surface)) = replacement {
            self.selected_result_tab = Some(id);
            self.result = surface.0;
            self.result_view = surface.1;
            self.error = surface.2;
        } else {
            self.selected_result_tab = None;
            self.result = None;
            self.result_view = ResultViewState::default();
            self.error = None;
        }
        Ok(())
    }

    pub(crate) fn sync_selected_result_tab_from_surface(&mut self) {
        let Some(selected) = self.selected_result_tab else {
            return;
        };
        let Some(tab) = self.result_tabs.iter_mut().find(|tab| tab.id == selected) else {
            return;
        };
        if let ResultTabContent::Result { snapshot, view } = &mut tab.content
            && self
                .result
                .as_ref()
                .is_some_and(|result| Arc::ptr_eq(result, snapshot))
        {
            *view = self.result_view.clone();
        }
    }

    pub(crate) fn result_snapshot(&self, result_id: ResultId) -> Option<Arc<ResultSnapshot>> {
        self.result_tabs
            .iter()
            .filter_map(ResultTab::snapshot)
            .find(|snapshot| snapshot.provenance.result_id == result_id)
            .cloned()
            .or_else(|| {
                self.result
                    .as_ref()
                    .filter(|result| result.provenance.result_id == result_id)
                    .cloned()
            })
    }

    pub(crate) fn begin_result_export(
        &mut self,
        result_id: ResultId,
        operation_id: OperationId,
    ) -> bool {
        self.sync_selected_result_tab_from_surface();
        let mut accepted = false;
        for tab in &mut self.result_tabs {
            if let ResultTabContent::Result { snapshot, view } = &mut tab.content
                && snapshot.provenance.result_id == result_id
            {
                accepted = view.begin_export(result_id, operation_id);
                if accepted && self.selected_result_tab == Some(tab.id) {
                    self.result_view = view.clone();
                }
                break;
            }
        }
        if accepted {
            return true;
        }
        if self
            .result
            .as_ref()
            .is_some_and(|result| result.provenance.result_id == result_id)
        {
            if self.result_view.begin_export(result_id, operation_id) {
                return true;
            }
            self.result_view.reset_for(result_id);
            return self.result_view.begin_export(result_id, operation_id);
        }
        false
    }

    pub(crate) fn finish_result_export(&mut self, result_id: ResultId, operation_id: OperationId) {
        for tab in &mut self.result_tabs {
            if let ResultTabContent::Result { snapshot, view } = &mut tab.content
                && snapshot.provenance.result_id == result_id
            {
                let _ = view.finish_export(result_id, operation_id);
                if self.selected_result_tab == Some(tab.id) {
                    self.result_view = view.clone();
                }
            }
        }
        let _ = self.result_view.finish_export(result_id, operation_id);
    }

    fn load_editor_tab_into_surface(&mut self, tab_id: EditorTabId) {
        let Some(tab) = self.editor_tab(tab_id) else {
            return;
        };
        let text = tab.text.clone();
        let database = tab.database.clone();
        let cursor_character_index = tab.cursor_character_index;
        let selection_character_range = tab.selection_character_range.clone();
        self.editor_text = text;
        self.editor_database = database;
        self.caret_character_index = cursor_character_index;
        self.selection_character_range = selection_character_range;
    }
}

fn validate_editor_tab_title(title: &str) -> Result<(), EditorTabError> {
    if title.trim().is_empty() || title.len() > MAX_EDITOR_TAB_TITLE_BYTES {
        Err(EditorTabError::InvalidTitle)
    } else {
        Ok(())
    }
}

fn validate_editor_tab_state(
    text: &str,
    database: Option<&str>,
    cursor_character_index: usize,
    selection_character_range: Option<&Range<usize>>,
) -> Result<(), EditorTabError> {
    if text.len() > MAX_EDITOR_TAB_TEXT_BYTES {
        return Err(EditorTabError::TextTooLarge);
    }
    if database.is_some_and(|database| database.len() > MAX_EDITOR_TAB_DATABASE_BYTES) {
        return Err(EditorTabError::DatabaseBindingTooLarge);
    }
    let character_count = text.chars().count();
    if cursor_character_index > character_count {
        return Err(EditorTabError::InvalidCursor);
    }
    if selection_character_range
        .is_some_and(|range| range.start > range.end || range.end > character_count)
    {
        return Err(EditorTabError::InvalidSelection);
    }
    Ok(())
}

const fn workspace_language(language: QueryLanguage) -> WorkspaceLanguage {
    match language {
        QueryLanguage::Sql => WorkspaceLanguage::Sql,
        QueryLanguage::RedisCommand => WorkspaceLanguage::RedisCommand,
        QueryLanguage::MongoDocument => WorkspaceLanguage::MongoDocument,
    }
}

const fn query_language(language: WorkspaceLanguage) -> QueryLanguage {
    match language {
        WorkspaceLanguage::Sql => QueryLanguage::Sql,
        WorkspaceLanguage::RedisCommand => QueryLanguage::RedisCommand,
        WorkspaceLanguage::MongoDocument => QueryLanguage::MongoDocument,
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
        editor_tab_id: Option<EditorTabId>,
        session_generation: SessionGeneration,
        result: ResultSnapshot,
    },
    QueryBatchFinished {
        operation_id: OperationId,
        profile_id: ProfileId,
        profile_generation: ProfileGeneration,
        editor_tab_id: Option<EditorTabId>,
        session_generation: SessionGeneration,
        target_count: usize,
        completed_targets: usize,
        discarded_results: usize,
        results: Vec<ResultSnapshot>,
        error: Option<PublicOperationError>,
        session_disposition: SessionDisposition,
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
    WorkspaceLoaded {
        operation_id: OperationId,
        identity: WorkspaceIdentity,
        base_revision: u64,
        mode: WorkspaceStoreMode,
        read_only_reason: Option<WorkspaceReadOnlyReason>,
        generation: Option<u64>,
        committed_bytes: u64,
        snapshot: Option<Box<ProfileWorkspaceSnapshot>>,
    },
    WorkspaceCommitted {
        operation_id: OperationId,
        identity: WorkspaceIdentity,
        revision: u64,
        generation: u64,
        committed_bytes: u64,
        warnings: Vec<WorkspaceStoreWarning>,
    },
    WorkspaceCleared {
        operation_id: OperationId,
        identity: WorkspaceIdentity,
        base_revision: u64,
    },
    WorkspaceCommitSuperseded {
        operation_id: OperationId,
        identity: WorkspaceIdentity,
        revision: u64,
        superseded_by: OperationId,
        superseded_by_revision: u64,
    },
    WorkspaceOperationFailed {
        operation_id: OperationId,
        identity: WorkspaceIdentity,
        revision: u64,
        action: WorkspaceAction,
        code: WorkspaceFailureCode,
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
                        workspace.pending_execute_editor_tab_id = None;
                    }
                    self.status = summary.message().to_owned();
                }
            }
            UiEvent::QueryFinished {
                operation_id,
                profile_id,
                profile_generation,
                editor_tab_id,
                session_generation,
                result,
            } => {
                if self.event_is_current(&profile_id, profile_generation) {
                    let duration_ms = result.provenance.duration_ms;
                    let (origin_missing, result_retained) = {
                        let workspace = self.exact_workspace_mut(&profile_id, profile_generation);
                        if workspace.pending_execute != Some(operation_id) {
                            return;
                        }
                        workspace.pending_execute = None;
                        workspace.pending_execute_editor_tab_id = None;
                        if editor_tab_id
                            .is_some_and(|tab_id| workspace.editor_tab(tab_id).is_none())
                        {
                            (true, false)
                        } else {
                            (
                                false,
                                workspace
                                    .append_result_tab_for_editor(Arc::new(result), editor_tab_id)
                                    .is_ok(),
                            )
                        }
                    };
                    self.status = if origin_missing {
                        "The originating editor tab is no longer available.".to_owned()
                    } else if !result_retained {
                        "Query finished, but its result was not retained because active results occupy the 32 MiB profile limit."
                            .to_owned()
                    } else {
                        format!("Query finished in {duration_ms} ms")
                    };
                    self.connection_states.insert(
                        profile_id,
                        ConnectionState::Connected {
                            session_generation,
                            elapsed_ms: 0,
                        },
                    );
                }
            }
            UiEvent::QueryBatchFinished {
                operation_id,
                profile_id,
                profile_generation,
                editor_tab_id,
                session_generation,
                target_count,
                completed_targets,
                mut discarded_results,
                results,
                error,
                session_disposition,
            } => {
                if self.event_is_current(&profile_id, profile_generation) {
                    let error_summary = error.as_ref().map(|error| error.summary);
                    let duration_ms = results.iter().fold(0_u128, |total, result| {
                        total.saturating_add(result.provenance.duration_ms)
                    });
                    let (origin_missing, error_not_retained) = {
                        let workspace = self.exact_workspace_mut(&profile_id, profile_generation);
                        if workspace.pending_execute != Some(operation_id) {
                            return;
                        }
                        workspace.pending_execute = None;
                        workspace.pending_execute_editor_tab_id = None;
                        if editor_tab_id
                            .is_some_and(|tab_id| workspace.editor_tab(tab_id).is_none())
                        {
                            (true, error.is_some())
                        } else {
                            for result in results {
                                if workspace
                                    .append_result_tab_for_editor(Arc::new(result), editor_tab_id)
                                    .is_err()
                                {
                                    discarded_results = discarded_results.saturating_add(1);
                                }
                            }
                            let error_not_retained = if let Some(error) = error {
                                if workspace
                                    .append_error_tab_for_editor(error.clone(), editor_tab_id)
                                    .is_err()
                                {
                                    if editor_tab_id.is_none()
                                        || editor_tab_id == workspace.selected_editor_tab
                                    {
                                        workspace.error = Some(error);
                                    }
                                    true
                                } else {
                                    false
                                }
                            } else {
                                false
                            };
                            (false, error_not_retained)
                        }
                    };
                    self.status = if origin_missing {
                        "The originating editor tab is no longer available.".to_owned()
                    } else if let Some(summary) = error_summary {
                        let mut status = format!(
                            "Run all stopped after {completed_targets}/{target_count} targets: {}",
                            summary.message()
                        );
                        if discarded_results > 0 {
                            status.push_str(&format!(
                                " ({discarded_results} completed results were not retained.)"
                            ));
                        }
                        if error_not_retained {
                            status.push_str(" The typed error could not be retained.");
                        }
                        status
                    } else {
                        let mut status = format!(
                            "Run all finished: {completed_targets}/{target_count} targets in {duration_ms} ms."
                        );
                        if discarded_results > 0 {
                            status.push_str(&format!(
                                " {discarded_results} results were not retained at the 32 MiB profile limit."
                            ));
                        }
                        status
                    };
                    self.connection_states.insert(
                        profile_id,
                        match session_disposition {
                            SessionDisposition::Keep => ConnectionState::Connected {
                                session_generation,
                                elapsed_ms: 0,
                            },
                            SessionDisposition::Evict => ConnectionState::Disconnected,
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
                        let (origin_missing, error_not_retained) = {
                            let workspace =
                                self.exact_workspace_mut(&profile_id, profile_generation);
                            if workspace.pending_execute != Some(operation_id) {
                                return;
                            }
                            workspace.pending_execute = None;
                            let editor_tab_id = workspace.pending_execute_editor_tab_id.take();
                            if editor_tab_id
                                .is_some_and(|tab_id| workspace.editor_tab(tab_id).is_none())
                            {
                                (true, true)
                            } else if workspace
                                .append_error_tab_for_editor(error.clone(), editor_tab_id)
                                .is_err()
                            {
                                if editor_tab_id.is_none()
                                    || editor_tab_id == workspace.selected_editor_tab
                                {
                                    workspace.error = Some(error);
                                }
                                (false, true)
                            } else {
                                (false, false)
                            }
                        };
                        self.status = if origin_missing {
                            "The originating editor tab is no longer available.".to_owned()
                        } else if error_not_retained {
                            format!(
                                "{} The typed error could not be retained.",
                                summary.message()
                            )
                        } else {
                            summary.message().to_owned()
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
            UiEvent::WorkspaceLoaded { .. }
            | UiEvent::WorkspaceCommitted { .. }
            | UiEvent::WorkspaceCleared { .. }
            | UiEvent::WorkspaceCommitSuperseded { .. }
            | UiEvent::WorkspaceOperationFailed { .. } => {}
            UiEvent::ConfigUncertain { operation_id } => {
                if !self.accept_profiles_operation(operation_id) {
                    return;
                }
                self.config_uncertain = true;
                self.pending_retags.clear();
                self.connection_states.clear();
                for workspace in self.workspaces.values_mut() {
                    workspace.pending_execute = None;
                    workspace.pending_execute_editor_tab_id = None;
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
                    workspace.pending_execute_editor_tab_id = None;
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
        let editor_retages = self
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
            .collect::<Vec<_>>();
        for (previous, refreshed) in editor_retages {
            if self.workspaces.contains_key(&refreshed) {
                continue;
            }
            if let Some(mut workspace) = self.workspaces.remove(&previous) {
                workspace.retain_editor_context_for_profile_retag();
                self.workspaces.insert(refreshed, workspace);
            }
        }
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
        ConnectionFailureOutcome, ConnectionState, EditorTabError, EditorTabId,
        MAX_EDITOR_TAB_TEXT_BYTES, MAX_RESULT_TABS_PER_EDITOR, MAX_RESULT_TABS_PER_PROFILE,
        PostCloseState, ProfileSnapshot, ProfileWorkspace, ProfileWorkspacePersistence,
        ResultTabError, UiEvent, UiModel, WorkspaceKey, WorkspaceModelError,
    };
    use crate::model::{
        CatalogLevel, CatalogPage, CatalogRequest, CatalogRetainedCounts, ConnectionProfile,
        CredentialMode, DriverAvailability, DriverKind, LegacyConfigVersion,
        MAX_PROFILE_RESULT_BYTES, OperationId, OperationKind, ProfileAccess, ProfileEnvironment,
        ProfileGeneration, ProfileId, ProfileInstanceId, ProfileSafetyPosture, PublicCode,
        PublicSummary, QueryLanguage, QueryResult, RedisKeyFilter, RedisKeyPage,
        RedisScanConsistency, RedisScanRequest, RedisTlsConfig, RequestIdentity, ResultId,
        ResultProvenance, ResultRetentionPolicy, ResultSnapshot, SessionGeneration, TlsMode,
    };
    use crate::public_error::{PublicOperationError, SafeContext};
    use crate::service::SessionDisposition;
    use crate::workspace::{
        ProfileWorkspaceSnapshot, WorkspaceGeometrySnapshot, WorkspaceHistoryEntry,
        WorkspaceHistoryStatus, WorkspaceRunTarget, WorkspaceSnapshotError,
    };

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

    fn result_with_id(result_id: u64) -> Arc<ResultSnapshot> {
        let mut snapshot = result(result_id as u128);
        snapshot.provenance.result_id = ResultId(result_id);
        Arc::new(snapshot)
    }

    fn classified_workspace_profile() -> ConnectionProfile {
        ConnectionProfile {
            id: "workspace-profile".to_owned(),
            name: "Workspace profile".to_owned(),
            driver: DriverKind::MySql,
            host: "127.0.0.1".to_owned(),
            port: 3306,
            database: Some("app".to_owned()),
            username: None,
            safety: ProfileSafetyPosture::classified(
                ProfileEnvironment::Development,
                ProfileAccess::ReadWrite,
                ProfileInstanceId::from_bytes([0x42; 16]),
            ),
            tls: TlsMode::Required,
            credential_mode: CredentialMode::Environment,
            secret_env: Some("DBOTTER_WORKSPACE_TEST_PASSWORD".to_owned()),
            redis_tls: RedisTlsConfig::default(),
        }
    }

    fn workspace_history(id: u64, source: &str) -> WorkspaceHistoryEntry {
        WorkspaceHistoryEntry::new(
            id,
            source,
            WorkspaceRunTarget::Current,
            1_752_710_400_000,
            WorkspaceHistoryStatus::Succeeded,
            17,
            1,
            0,
            false,
        )
        .expect("bounded history fixture")
    }

    fn workspace_geometry(editor_share: f32) -> WorkspaceGeometrySnapshot {
        WorkspaceGeometrySnapshot::new(320.0, editor_share, true).expect("bounded geometry fixture")
    }

    #[test]
    fn editor_tabs_round_trip_exact_persistent_state_order_selection_and_history() {
        let profile = classified_workspace_profile();
        let history = vec![workspace_history(91, "SELECT history_value")];
        let persistence = ProfileWorkspacePersistence::for_classified_profile(
            &profile,
            true,
            workspace_geometry(0.61),
            history.clone(),
        )
        .expect("classified profile persistence");
        let mut workspace = ProfileWorkspace::default();
        workspace
            .bind_persistence(persistence)
            .expect("bind classified persistence");
        let first = workspace
            .create_editor_tab_with_state(
                QueryLanguage::Sql,
                "First",
                "SELECT 1",
                Some("primary".to_owned()),
                8,
                Some(7..8),
            )
            .expect("first tab");
        let second = workspace
            .create_editor_tab_with_state(
                QueryLanguage::Sql,
                "Second",
                "SELECT 2",
                Some("warehouse".to_owned()),
                8,
                Some(7..8),
            )
            .expect("second tab");
        workspace
            .select_editor_tab(first)
            .expect("select first tab");
        workspace.editor_text = "SELECT 11".to_owned();
        workspace.editor_database = Some("analytics".to_owned());
        workspace.caret_character_index = 9;
        workspace.selection_character_range = Some(7..9);
        workspace
            .sync_selected_editor_tab_from_surface()
            .expect("sync exact selected state");
        workspace
            .rename_editor_tab(first, "First renamed")
            .expect("rename first tab");
        workspace
            .reorder_editor_tab(second, 0)
            .expect("move second tab before first");

        let revision = workspace.revision();
        let snapshot = workspace
            .to_persistence_snapshot()
            .expect("workspace snapshot");
        assert_eq!(
            workspace.revision(),
            revision,
            "snapshotting is not a change"
        );
        assert_eq!(
            snapshot
                .editor_tabs()
                .iter()
                .map(|tab| tab.id())
                .collect::<Vec<_>>(),
            vec![second.0, first.0]
        );
        assert_eq!(snapshot.selected_editor_tab_id(), Some(first.0));
        assert_eq!(snapshot.history(), history);

        let mut restored = ProfileWorkspace::from_persistence_snapshot(snapshot.clone())
            .expect("restore validated snapshot");
        assert_eq!(restored.selected_editor_tab_id(), Some(first));
        assert_eq!(restored.editor_tabs()[0].id(), second);
        let restored_first = restored.editor_tab(first).expect("restored first tab");
        assert_eq!(restored_first.title(), "First renamed");
        assert_eq!(restored_first.text(), "SELECT 11");
        assert_eq!(restored_first.database(), Some("analytics"));
        assert_eq!(restored_first.cursor_character_index(), 9);
        assert_eq!(restored_first.selection_character_range(), Some(7..9));
        assert_eq!(restored.editor_text, "SELECT 11");
        assert_eq!(restored.editor_database.as_deref(), Some("analytics"));
        assert_eq!(restored.caret_character_index, 9);
        assert_eq!(restored.selection_character_range, Some(7..9));
        assert!(restored.is_saved());
        assert_eq!(
            restored
                .persistence()
                .expect("restored persistence")
                .history(),
            history
        );
        assert_eq!(
            restored
                .to_persistence_snapshot()
                .expect("round-trip snapshot"),
            snapshot
        );
    }

    #[test]
    fn workspace_revision_rejects_stale_save_and_ignores_result_only_changes() {
        let profile = classified_workspace_profile();
        let persistence = ProfileWorkspacePersistence::for_classified_profile(
            &profile,
            true,
            workspace_geometry(0.60),
            Vec::new(),
        )
        .expect("classified profile persistence");
        let mut workspace = ProfileWorkspace::default();
        workspace
            .bind_persistence(persistence)
            .expect("bind persistence");
        let first = workspace
            .create_editor_tab(QueryLanguage::Sql, "First", "SELECT 1")
            .expect("first tab");
        let second = workspace
            .create_editor_tab(QueryLanguage::Sql, "Second", "SELECT 2")
            .expect("second tab");
        let save_started_at = workspace.revision();

        workspace
            .select_editor_tab(first)
            .expect("selected identity changes");
        workspace.editor_text = "SELECT 10".to_owned();
        workspace.editor_database = Some("analytics".to_owned());
        workspace.caret_character_index = 9;
        workspace.selection_character_range = Some(7..9);
        workspace
            .sync_selected_editor_tab_from_surface()
            .expect("source and cursor change");
        workspace
            .rename_editor_tab(first, "Renamed")
            .expect("title change");
        workspace
            .reorder_editor_tab(second, 0)
            .expect("order change");
        workspace
            .set_persistence_geometry(workspace_geometry(0.65))
            .expect("geometry change");
        workspace
            .replace_persistence_history(vec![workspace_history(92, "SELECT 10")])
            .expect("history change");
        assert!(workspace.revision() > save_started_at);
        assert!(
            !workspace.mark_saved_if_revision(save_started_at),
            "an old save completion cannot clean newer durable state"
        );
        assert!(
            workspace
                .editor_tab(first)
                .is_some_and(|tab| tab.is_dirty())
        );

        let current_revision = workspace.revision();
        assert!(workspace.mark_saved_if_revision(current_revision));
        assert!(workspace.is_saved());
        assert!(
            workspace
                .editor_tab(first)
                .is_some_and(|tab| !tab.is_dirty())
        );

        let before_result = workspace.revision();
        workspace
            .append_result_tab(result_with_id(501))
            .expect("result-only state");
        workspace.select_result_area_tab(super::ResultAreaTab::History);
        assert_eq!(
            workspace.revision(),
            before_result,
            "result payload and result/history view selection are not durable draft changes"
        );
        assert!(workspace.is_saved());
    }

    #[test]
    fn editor_snapshot_conversion_rejects_unclassified_invalid_and_oversize_state() {
        let mut workspace = ProfileWorkspace::default();
        assert!(matches!(
            workspace.to_persistence_snapshot(),
            Err(WorkspaceModelError::PersistenceNotConfigured)
        ));
        assert_eq!(
            workspace.create_editor_tab_with_state(
                QueryLanguage::Sql,
                "Oversize",
                "x".repeat(MAX_EDITOR_TAB_TEXT_BYTES + 1),
                None,
                0,
                None,
            ),
            Err(EditorTabError::TextTooLarge)
        );
        assert_eq!(
            workspace.create_editor_tab_with_state(
                QueryLanguage::Sql,
                "Database",
                "SELECT 1",
                Some("d".repeat(1_025)),
                8,
                None,
            ),
            Err(EditorTabError::DatabaseBindingTooLarge)
        );
        assert_eq!(
            workspace.create_editor_tab_with_state(
                QueryLanguage::Sql,
                "Cursor",
                "SELECT 1",
                None,
                9,
                None,
            ),
            Err(EditorTabError::InvalidCursor)
        );
        assert_eq!(
            workspace.create_editor_tab_with_state(
                QueryLanguage::Sql,
                "Selection",
                "SELECT 1",
                None,
                8,
                Some(4..9),
            ),
            Err(EditorTabError::InvalidSelection)
        );

        for safety in [
            ProfileSafetyPosture::new(ProfileEnvironment::Development, ProfileAccess::ReadWrite),
            ProfileSafetyPosture::unclassified_legacy(LegacyConfigVersion::V2),
        ] {
            let mut profile = classified_workspace_profile();
            profile.safety = safety;
            assert!(matches!(
                ProfileWorkspacePersistence::for_classified_profile(
                    &profile,
                    true,
                    workspace_geometry(0.60),
                    Vec::new(),
                ),
                Err(WorkspaceModelError::UnclassifiedProfile)
            ));
        }

        let profile = classified_workspace_profile();
        let persistence = ProfileWorkspacePersistence::for_classified_profile(
            &profile,
            true,
            workspace_geometry(0.60),
            Vec::new(),
        )
        .expect("classified profile");
        workspace
            .bind_persistence(persistence)
            .expect("bind persistence");
        workspace
            .create_editor_tab(QueryLanguage::Sql, "Valid", "SELECT 1")
            .expect("valid tab");
        let snapshot = workspace.to_persistence_snapshot().expect("valid snapshot");
        let mut invalid_json = serde_json::to_value(snapshot).expect("snapshot JSON");
        invalid_json["editor_tabs"][0]["cursor_character_index"] = serde_json::json!(99);
        let invalid =
            serde_json::from_value::<ProfileWorkspaceSnapshot>(invalid_json).expect("wire shape");
        assert!(matches!(
            ProfileWorkspace::from_persistence_snapshot(invalid),
            Err(WorkspaceModelError::Snapshot(
                WorkspaceSnapshotError::InvalidEditorCursor
            ))
        ));
    }

    #[test]
    fn result_tab_per_editor_cap_evicts_oldest_from_same_editor() {
        assert_eq!(MAX_RESULT_TABS_PER_EDITOR, 10);
        let mut workspace = ProfileWorkspace::default();
        let editor_a = EditorTabId(11);
        let editor_b = EditorTabId(12);

        for result_id in 1..=MAX_RESULT_TABS_PER_EDITOR as u64 {
            workspace
                .append_result_tab_for_editor(result_with_id(result_id), Some(editor_a))
                .expect("editor A result fits");
        }
        workspace
            .append_result_tab_for_editor(result_with_id(100), Some(editor_b))
            .expect("editor B result fits");
        workspace
            .append_result_tab_for_editor(result_with_id(11), Some(editor_a))
            .expect("editor A oldest inactive result is evicted");

        assert_eq!(
            workspace
                .result_tabs()
                .iter()
                .filter(|tab| tab.origin_editor_tab_id() == Some(editor_a))
                .count(),
            MAX_RESULT_TABS_PER_EDITOR
        );
        assert_eq!(
            workspace
                .result_tabs()
                .iter()
                .filter(|tab| tab.origin_editor_tab_id() == Some(editor_b))
                .count(),
            1
        );
        assert!(workspace.result_snapshot(ResultId(1)).is_none());
        assert!(workspace.result_snapshot(ResultId(2)).is_some());
        assert!(workspace.result_snapshot(ResultId(11)).is_some());
        assert!(workspace.result_snapshot(ResultId(100)).is_some());
    }

    #[test]
    fn result_tab_profile_cap_evicts_oldest_global_inactive() {
        assert_eq!(MAX_RESULT_TABS_PER_PROFILE, 40);
        let mut workspace = ProfileWorkspace::default();
        for result_id in 1..=MAX_RESULT_TABS_PER_PROFILE as u64 {
            workspace
                .append_result_tab(result_with_id(result_id))
                .expect("profile result fits");
        }

        workspace
            .append_result_tab(result_with_id(41))
            .expect("global oldest inactive result is evicted");

        assert_eq!(workspace.result_tabs().len(), MAX_RESULT_TABS_PER_PROFILE);
        assert!(workspace.result_snapshot(ResultId(1)).is_none());
        assert!(workspace.result_snapshot(ResultId(2)).is_some());
        assert!(workspace.result_snapshot(ResultId(40)).is_some());
        assert!(workspace.result_snapshot(ResultId(41)).is_some());
    }

    #[test]
    fn closing_selected_result_never_activates_another_editors_result() {
        let mut workspace = ProfileWorkspace::default();
        let editor_a = workspace
            .create_editor_tab(QueryLanguage::Sql, "Editor A", "")
            .expect("editor A");
        let result_a = workspace
            .append_result_tab_for_editor(result_with_id(1), Some(editor_a))
            .expect("editor A result");
        let editor_b = workspace
            .create_editor_tab(QueryLanguage::Sql, "Editor B", "")
            .expect("editor B");
        let result_b = workspace
            .append_result_tab_for_editor(result_with_id(2), Some(editor_b))
            .expect("editor B result");

        workspace
            .close_result_tab(result_b)
            .expect("close editor B result");

        assert_eq!(workspace.selected_editor_tab_id(), Some(editor_b));
        assert_eq!(workspace.selected_result_tab_id(), None);
        assert!(workspace.result.is_none());
        assert!(workspace.result_snapshot(ResultId(1)).is_some());

        workspace
            .select_editor_tab(editor_a)
            .expect("return to editor A");
        assert_eq!(workspace.selected_result_tab_id(), Some(result_a));
        assert_eq!(
            workspace
                .result
                .as_ref()
                .map(|result| result.provenance.result_id),
            Some(ResultId(1))
        );
    }

    #[test]
    fn result_tab_capacity_rejects_when_every_candidate_is_protected() {
        let mut workspace = ProfileWorkspace::default();
        let editor = EditorTabId(21);
        let mut tab_ids = Vec::new();
        for result_id in 1..=MAX_RESULT_TABS_PER_EDITOR as u64 {
            tab_ids.push(
                workspace
                    .append_result_tab_for_editor(result_with_id(result_id), Some(editor))
                    .expect("editor result fits"),
            );
        }
        for result_id in 1..MAX_RESULT_TABS_PER_EDITOR as u64 {
            assert!(
                workspace.begin_result_export(ResultId(result_id), OperationId(500 + result_id),)
            );
        }
        workspace
            .select_result_tab(*tab_ids.last().expect("last result tab"))
            .expect("last result is selected");

        assert_eq!(
            workspace.append_result_tab_for_editor(result_with_id(11), Some(editor)),
            Err(ResultTabError::CapacityProtected)
        );
        assert_eq!(workspace.result_tabs().len(), MAX_RESULT_TABS_PER_EDITOR);
        assert!(workspace.result_snapshot(ResultId(1)).is_some());
        assert!(workspace.result_snapshot(ResultId(10)).is_some());
        assert!(workspace.result_snapshot(ResultId(11)).is_none());
    }

    #[test]
    fn aggregate_result_bytes_evict_oldest_inactive_and_never_active_exports() {
        let mut workspace = ProfileWorkspace::default();
        let mut result_ids = Vec::new();
        for result_id in 31..=34 {
            let mut snapshot = result(result_id);
            snapshot.provenance.result_id = ResultId(result_id as u64);
            snapshot.retained_bytes = 8 * 1024 * 1024;
            workspace
                .append_result_tab(Arc::new(snapshot))
                .expect("four exact-cap results fit");
            result_ids.push(ResultId(result_id as u64));
        }
        assert_eq!(workspace.retained_result_bytes(), MAX_PROFILE_RESULT_BYTES);

        for (index, result_id) in result_ids.iter().take(3).enumerate() {
            assert!(workspace.begin_result_export(*result_id, OperationId(300 + index as u64)));
        }
        let mut rejected = result(35);
        rejected.provenance.result_id = ResultId(35);
        rejected.retained_bytes = 8 * 1024 * 1024;
        assert_eq!(
            workspace.append_result_tab(Arc::new(rejected)),
            Err(ResultTabError::CapacityProtected),
            "the selected result and three active exports cannot be evicted"
        );
        assert_eq!(workspace.retained_result_bytes(), MAX_PROFILE_RESULT_BYTES);

        workspace.finish_result_export(ResultId(31), OperationId(300));
        let mut replacement = result(36);
        replacement.provenance.result_id = ResultId(36);
        replacement.retained_bytes = 8 * 1024 * 1024;
        workspace
            .append_result_tab(Arc::new(replacement))
            .expect("the oldest inactive result is now evictable");
        assert_eq!(workspace.retained_result_bytes(), MAX_PROFILE_RESULT_BYTES);
        assert!(workspace.result_snapshot(ResultId(31)).is_none());
        assert!(workspace.result_snapshot(ResultId(32)).is_some());
        assert!(workspace.result_snapshot(ResultId(36)).is_some());
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
            editor_tab_id: None,
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
