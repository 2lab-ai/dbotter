//! Private, bounded workspace snapshots and manifest-backed durable storage.

use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{Read as _, Write as _};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::model::{ProfileId, ProfileInstanceId};

pub const MAX_EDITOR_SOURCE_BYTES: usize = 256 * 1024;
pub const MAX_EDITOR_TABS_PER_PROFILE: usize = 20;
pub const MAX_EDITOR_TABS_TOTAL: usize = 100;
pub const MAX_HISTORY_SOURCE_BYTES: usize = 64 * 1024;
pub const MAX_HISTORY_ENTRIES_PER_PROFILE: usize = 2_000;
pub const MAX_HISTORY_ENTRIES_TOTAL: usize = 10_000;
pub const MAX_PROFILE_SHARD_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_WORKSPACE_STORE_BYTES: u64 = 128 * 1024 * 1024;

const MAX_EDITOR_TITLE_BYTES: usize = 120;
const MAX_DATABASE_BINDING_BYTES: usize = 1_024;
const MAX_PROFILE_ID_BYTES: usize = 256;
const MAX_MANIFEST_BYTES: usize = 16 * 1024;
const MAX_CORRUPT_STATE_BYTES: usize = 64;
const MAX_ROOT_PROFILE_CORRUPT_MARKER_BYTES: usize = 256;
const MANIFEST_SCHEMA: &str = "dbotter.workspace-manifest.v1";
const SHARD_SCHEMA: &str = "dbotter.workspace-shard.v1";
const CORRUPT_STATE_FILE: &str = "corrupt.state";
const CLEARED_TOMBSTONE_PREFIX: &str = ".cleared-";
const PROFILE_CORRUPT_MARKER_PREFIX: &str = ".corrupt-";
const PROFILE_CORRUPT_MARKER_SUFFIX: &str = ".state";
const PROFILE_CORRUPT_MARKER_TEMP_PREFIX: &str = ".dbotter-workspace.profile-corrupt-marker.tmp.";
pub const MAX_QUARANTINE_FILES: usize = 16;
pub const MAX_QUARANTINE_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkspaceLanguage {
    Sql,
    RedisCommand,
    MongoDocument,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EditorTabSnapshot {
    id: u64,
    title: String,
    language: WorkspaceLanguage,
    source: String,
    database: Option<String>,
    cursor_character_index: usize,
    selection_character_range: Option<Range<usize>>,
}

impl std::fmt::Debug for EditorTabSnapshot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EditorTabSnapshot")
            .field("id", &self.id)
            .field("title", &"<redacted>")
            .field("language", &self.language)
            .field("source", &"<redacted>")
            .field("database", &self.database.as_ref().map(|_| "<configured>"))
            .field("cursor_character_index", &self.cursor_character_index)
            .field("selection_character_range", &self.selection_character_range)
            .finish()
    }
}

impl EditorTabSnapshot {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: u64,
        title: impl Into<String>,
        language: WorkspaceLanguage,
        source: impl Into<String>,
        database: Option<&str>,
        cursor_character_index: usize,
        selection_character_range: Option<Range<usize>>,
    ) -> Result<Self, WorkspaceSnapshotError> {
        let value = Self {
            id,
            title: title.into(),
            language,
            source: source.into(),
            database: database.map(str::to_owned),
            cursor_character_index,
            selection_character_range,
        };
        value.validate()?;
        Ok(value)
    }

    pub const fn id(&self) -> u64 {
        self.id
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub const fn language(&self) -> WorkspaceLanguage {
        self.language
    }

    pub fn source(&self) -> &str {
        &self.source
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

    fn validate(&self) -> Result<(), WorkspaceSnapshotError> {
        if self.id == 0 {
            return Err(WorkspaceSnapshotError::InvalidEditorId);
        }
        if self.title.trim().is_empty() || self.title.len() > MAX_EDITOR_TITLE_BYTES {
            return Err(WorkspaceSnapshotError::InvalidEditorTitle);
        }
        if self.source.len() > MAX_EDITOR_SOURCE_BYTES {
            return Err(WorkspaceSnapshotError::EditorSourceTooLarge);
        }
        if self
            .database
            .as_ref()
            .is_some_and(|database| database.len() > MAX_DATABASE_BINDING_BYTES)
        {
            return Err(WorkspaceSnapshotError::DatabaseBindingTooLarge);
        }
        let character_count = self.source.chars().count();
        if self.cursor_character_index > character_count {
            return Err(WorkspaceSnapshotError::InvalidEditorCursor);
        }
        if self
            .selection_character_range
            .as_ref()
            .is_some_and(|range| range.start > range.end || range.end > character_count)
        {
            return Err(WorkspaceSnapshotError::InvalidEditorSelection);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkspaceRunTarget {
    Current,
    Selection,
    All,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkspaceHistoryCode {
    None,
    Admission,
    Authentication,
    Permission,
    Network,
    Timeout,
    Backend,
    Internal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", content = "code", rename_all = "kebab-case")]
pub enum WorkspaceHistoryStatus {
    Succeeded,
    Failed(WorkspaceHistoryCode),
    Cancelled,
    OutcomeUnknown,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceHistoryEntry {
    id: u64,
    source: Option<String>,
    source_omitted: bool,
    target: WorkspaceRunTarget,
    completed_at_unix_ms: i64,
    status: WorkspaceHistoryStatus,
    duration_ms: u64,
    returned_rows: u64,
    affected_rows: u64,
    truncated: bool,
}

impl std::fmt::Debug for WorkspaceHistoryEntry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WorkspaceHistoryEntry")
            .field("id", &self.id)
            .field("source", &self.source.as_ref().map(|_| "<redacted>"))
            .field("source_omitted", &self.source_omitted)
            .field("target", &self.target)
            .field("completed_at_unix_ms", &self.completed_at_unix_ms)
            .field("status", &self.status)
            .field("duration_ms", &self.duration_ms)
            .field("returned_rows", &self.returned_rows)
            .field("affected_rows", &self.affected_rows)
            .field("truncated", &self.truncated)
            .finish()
    }
}

impl WorkspaceHistoryEntry {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: u64,
        source: &str,
        target: WorkspaceRunTarget,
        completed_at_unix_ms: i64,
        status: WorkspaceHistoryStatus,
        duration_ms: u64,
        returned_rows: u64,
        affected_rows: u64,
        truncated: bool,
    ) -> Result<Self, WorkspaceSnapshotError> {
        if id == 0 {
            return Err(WorkspaceSnapshotError::InvalidHistoryId);
        }
        let source_omitted = source.len() > MAX_HISTORY_SOURCE_BYTES;
        Ok(Self {
            id,
            source: (!source_omitted).then(|| source.to_owned()),
            source_omitted,
            target,
            completed_at_unix_ms,
            status,
            duration_ms,
            returned_rows,
            affected_rows,
            truncated,
        })
    }

    pub const fn id(&self) -> u64 {
        self.id
    }

    pub fn source(&self) -> Option<&str> {
        self.source.as_deref()
    }

    pub const fn source_omitted(&self) -> bool {
        self.source_omitted
    }

    pub const fn is_reopenable(&self) -> bool {
        self.source.is_some() && !self.source_omitted
    }

    pub const fn target(&self) -> WorkspaceRunTarget {
        self.target
    }

    pub const fn status(&self) -> WorkspaceHistoryStatus {
        self.status
    }

    pub const fn completed_at_unix_ms(&self) -> i64 {
        self.completed_at_unix_ms
    }

    pub const fn duration_ms(&self) -> u64 {
        self.duration_ms
    }

    pub const fn returned_rows(&self) -> u64 {
        self.returned_rows
    }

    pub const fn affected_rows(&self) -> u64 {
        self.affected_rows
    }

    pub const fn truncated(&self) -> bool {
        self.truncated
    }

    fn validate(&self) -> Result<(), WorkspaceSnapshotError> {
        if self.id == 0 {
            return Err(WorkspaceSnapshotError::InvalidHistoryId);
        }
        match (&self.source, self.source_omitted) {
            (Some(source), false) if source.len() <= MAX_HISTORY_SOURCE_BYTES => Ok(()),
            (None, true) => Ok(()),
            _ => Err(WorkspaceSnapshotError::InvalidHistorySource),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceGeometrySnapshot {
    navigator_width: f32,
    editor_share: f32,
    inspector_visible: bool,
}

impl WorkspaceGeometrySnapshot {
    pub fn new(
        navigator_width: f32,
        editor_share: f32,
        inspector_visible: bool,
    ) -> Result<Self, WorkspaceSnapshotError> {
        let value = Self {
            navigator_width,
            editor_share,
            inspector_visible,
        };
        value.validate()?;
        Ok(value)
    }

    pub const fn navigator_width(&self) -> f32 {
        self.navigator_width
    }

    pub const fn editor_share(&self) -> f32 {
        self.editor_share
    }

    pub const fn inspector_visible(&self) -> bool {
        self.inspector_visible
    }

    fn validate(&self) -> Result<(), WorkspaceSnapshotError> {
        if !self.navigator_width.is_finite()
            || !self.editor_share.is_finite()
            || !(160.0..=720.0).contains(&self.navigator_width)
            || !(0.1..=0.9).contains(&self.editor_share)
        {
            return Err(WorkspaceSnapshotError::InvalidGeometry);
        }
        Ok(())
    }
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileWorkspaceSnapshot {
    instance_id: ProfileInstanceId,
    profile_id: ProfileId,
    persistence_enabled: bool,
    editor_tabs: Vec<EditorTabSnapshot>,
    selected_editor_tab_id: Option<u64>,
    geometry: WorkspaceGeometrySnapshot,
    history: Vec<WorkspaceHistoryEntry>,
}

impl std::fmt::Debug for ProfileWorkspaceSnapshot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProfileWorkspaceSnapshot")
            .field("instance_id", &"<redacted>")
            .field("profile_id", &"<redacted>")
            .field("persistence_enabled", &self.persistence_enabled)
            .field("editor_tab_count", &self.editor_tabs.len())
            .field("selected_editor_tab_id", &self.selected_editor_tab_id)
            .field("geometry", &self.geometry)
            .field("history_count", &self.history.len())
            .finish()
    }
}

impl ProfileWorkspaceSnapshot {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        instance_id: ProfileInstanceId,
        profile_id: ProfileId,
        persistence_enabled: bool,
        editor_tabs: Vec<EditorTabSnapshot>,
        selected_editor_tab_id: Option<u64>,
        geometry: WorkspaceGeometrySnapshot,
        history: Vec<WorkspaceHistoryEntry>,
    ) -> Result<Self, WorkspaceSnapshotError> {
        let value = Self {
            instance_id,
            profile_id,
            persistence_enabled,
            editor_tabs,
            selected_editor_tab_id,
            geometry,
            history,
        };
        value.validate()?;
        Ok(value)
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

    pub fn editor_tabs(&self) -> &[EditorTabSnapshot] {
        &self.editor_tabs
    }

    pub const fn selected_editor_tab_id(&self) -> Option<u64> {
        self.selected_editor_tab_id
    }

    pub const fn geometry(&self) -> WorkspaceGeometrySnapshot {
        self.geometry
    }

    pub fn history(&self) -> &[WorkspaceHistoryEntry] {
        &self.history
    }

    fn validate(&self) -> Result<(), WorkspaceSnapshotError> {
        if self.profile_id.0.trim().is_empty() || self.profile_id.0.len() > MAX_PROFILE_ID_BYTES {
            return Err(WorkspaceSnapshotError::InvalidProfileId);
        }
        if self.editor_tabs.len() > MAX_EDITOR_TABS_PER_PROFILE {
            return Err(WorkspaceSnapshotError::TooManyEditorTabs);
        }
        if self.history.len() > MAX_HISTORY_ENTRIES_PER_PROFILE {
            return Err(WorkspaceSnapshotError::TooManyHistoryEntries);
        }
        if !self.persistence_enabled && (!self.editor_tabs.is_empty() || !self.history.is_empty()) {
            return Err(WorkspaceSnapshotError::DisabledPersistenceHasContent);
        }
        self.geometry.validate()?;
        let mut tab_ids = HashSet::with_capacity(self.editor_tabs.len());
        for tab in &self.editor_tabs {
            tab.validate()?;
            if !tab_ids.insert(tab.id) {
                return Err(WorkspaceSnapshotError::DuplicateEditorId);
            }
        }
        if self
            .selected_editor_tab_id
            .is_some_and(|selected| !tab_ids.contains(&selected))
        {
            return Err(WorkspaceSnapshotError::UnknownSelectedEditor);
        }
        let mut history_ids = HashSet::with_capacity(self.history.len());
        for entry in &self.history {
            entry.validate()?;
            if !history_ids.insert(entry.id) {
                return Err(WorkspaceSnapshotError::DuplicateHistoryId);
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct WorkspaceHistoryEvictionIdentity {
    instance_id: ProfileInstanceId,
    history_id: u64,
}

impl std::fmt::Debug for WorkspaceHistoryEvictionIdentity {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WorkspaceHistoryEvictionIdentity")
            .field("instance_id", &"<redacted>")
            .field("history_id", &self.history_id)
            .finish()
    }
}

impl WorkspaceHistoryEvictionIdentity {
    pub const fn instance_id(&self) -> ProfileInstanceId {
        self.instance_id
    }

    pub const fn history_id(&self) -> u64 {
        self.history_id
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkspaceRetentionLimit {
    ProfileHistoryEntries,
    TotalHistoryEntries,
    ProfileShardBytes,
    TotalStoreBytes,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum WorkspaceRetentionError {
    #[error(transparent)]
    Snapshot(#[from] WorkspaceSnapshotError),
    #[error("protected workspace state prevents satisfying {0:?}")]
    RetentionExhausted(WorkspaceRetentionLimit),
    #[error("workspace retention byte accounting failed")]
    AccountingFailed,
}

impl WorkspaceRetentionError {
    const fn into_snapshot_error(self) -> WorkspaceSnapshotError {
        match self {
            Self::Snapshot(error) => error,
            Self::RetentionExhausted(
                WorkspaceRetentionLimit::ProfileHistoryEntries
                | WorkspaceRetentionLimit::ProfileShardBytes,
            ) => WorkspaceSnapshotError::TooManyHistoryEntries,
            Self::RetentionExhausted(
                WorkspaceRetentionLimit::TotalHistoryEntries
                | WorkspaceRetentionLimit::TotalStoreBytes,
            )
            | Self::AccountingFailed => WorkspaceSnapshotError::TooManyHistoryEntriesTotal,
        }
    }
}

#[derive(Clone, Copy)]
struct WorkspaceRetentionLimits {
    history_entries_per_profile: usize,
    history_entries_total: usize,
    profile_shard_bytes: usize,
    workspace_store_bytes: u64,
}

impl WorkspaceRetentionLimits {
    const PRODUCTION: Self = Self {
        history_entries_per_profile: MAX_HISTORY_ENTRIES_PER_PROFILE,
        history_entries_total: MAX_HISTORY_ENTRIES_TOTAL,
        profile_shard_bytes: MAX_PROFILE_SHARD_BYTES,
        workspace_store_bytes: MAX_WORKSPACE_STORE_BYTES,
    };
}

#[derive(Clone, Copy)]
struct HistoryRetentionCandidate {
    completed_at_unix_ms: i64,
    instance: [u8; 16],
    history_id: u64,
    profile_index: usize,
}

impl HistoryRetentionCandidate {
    const fn identity(self) -> WorkspaceHistoryEvictionIdentity {
        WorkspaceHistoryEvictionIdentity {
            instance_id: ProfileInstanceId::from_bytes(self.instance),
            history_id: self.history_id,
        }
    }
}

#[derive(Clone)]
pub struct WorkspaceSnapshotSet {
    profiles: Vec<ProfileWorkspaceSnapshot>,
    history_evictions: Vec<WorkspaceHistoryEvictionIdentity>,
}

impl std::fmt::Debug for WorkspaceSnapshotSet {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WorkspaceSnapshotSet")
            .field("profile_count", &self.profiles.len())
            .field("history_evicted", &self.history_evictions.len())
            .finish()
    }
}

impl WorkspaceSnapshotSet {
    pub fn new(profiles: Vec<ProfileWorkspaceSnapshot>) -> Result<Self, WorkspaceSnapshotError> {
        Self::new_with_retention(profiles).map_err(WorkspaceRetentionError::into_snapshot_error)
    }

    pub fn new_with_retention(
        profiles: Vec<ProfileWorkspaceSnapshot>,
    ) -> Result<Self, WorkspaceRetentionError> {
        Self::plan_with_limits(profiles, WorkspaceRetentionLimits::PRODUCTION)
    }

    fn plan_with_limits(
        mut profiles: Vec<ProfileWorkspaceSnapshot>,
        limits: WorkspaceRetentionLimits,
    ) -> Result<Self, WorkspaceRetentionError> {
        let mut instances = HashSet::with_capacity(profiles.len());
        let mut editor_tabs = 0_usize;
        for profile in &profiles {
            profile.validate()?;
            if !instances.insert(profile.instance_id()) {
                return Err(WorkspaceSnapshotError::DuplicateProfileInstance.into());
            }
            editor_tabs = editor_tabs
                .checked_add(profile.editor_tabs.len())
                .ok_or(WorkspaceSnapshotError::TooManyEditorTabsTotal)?;
        }
        if editor_tabs > MAX_EDITOR_TABS_TOTAL {
            return Err(WorkspaceSnapshotError::TooManyEditorTabsTotal.into());
        }

        let mut evicted = Vec::new();
        let mut profile_order = (0..profiles.len()).collect::<Vec<_>>();
        profile_order.sort_unstable_by_key(|index| *profiles[*index].instance_id().as_bytes());
        for profile_index in profile_order {
            let overflow = profiles[profile_index]
                .history
                .len()
                .saturating_sub(limits.history_entries_per_profile);
            if overflow == 0 {
                continue;
            }
            let candidates = history_retention_candidates(&profiles, Some(profile_index));
            if candidates.len() < overflow {
                return Err(WorkspaceRetentionError::RetentionExhausted(
                    WorkspaceRetentionLimit::ProfileHistoryEntries,
                ));
            }
            apply_history_evictions(&mut profiles, &candidates[..overflow], &mut evicted);
        }

        let history_entries = profiles.iter().try_fold(0_usize, |total, profile| {
            total.checked_add(profile.history.len()).ok_or(
                WorkspaceRetentionError::RetentionExhausted(
                    WorkspaceRetentionLimit::TotalHistoryEntries,
                ),
            )
        })?;
        let overflow = history_entries.saturating_sub(limits.history_entries_total);
        if overflow > 0 {
            let candidates = history_retention_candidates(&profiles, None);
            if candidates.len() < overflow {
                return Err(WorkspaceRetentionError::RetentionExhausted(
                    WorkspaceRetentionLimit::TotalHistoryEntries,
                ));
            }
            apply_history_evictions(&mut profiles, &candidates[..overflow], &mut evicted);
        }

        let mut encoded_profiles = profiles
            .iter()
            .map(ProfileEncodedAccounting::new)
            .collect::<Result<Vec<_>, _>>()?;
        let mut profile_order = (0..profiles.len()).collect::<Vec<_>>();
        profile_order.sort_unstable_by_key(|index| *profiles[*index].instance_id().as_bytes());
        for profile_index in profile_order {
            let profile_shard_limit = u64::try_from(limits.profile_shard_bytes)
                .map_err(|_| WorkspaceRetentionError::AccountingFailed)?;
            if encoded_profiles[profile_index].encoded_size()?.shard_bytes <= profile_shard_limit {
                continue;
            }
            let candidates = history_retention_candidates(&profiles, Some(profile_index));
            let removal_count = minimum_profile_evictions_to_fit(
                &encoded_profiles[profile_index],
                &candidates,
                profile_shard_limit,
            )?;
            apply_encoded_evictions(&mut encoded_profiles, &candidates[..removal_count])?;
            apply_history_evictions(&mut profiles, &candidates[..removal_count], &mut evicted);
        }

        if conservative_encoded_store_bytes(&encoded_profiles)? > limits.workspace_store_bytes {
            let candidates = history_retention_candidates(&profiles, None);
            let removal_count = minimum_total_evictions_to_fit(
                &encoded_profiles,
                &candidates,
                limits.workspace_store_bytes,
            )?;
            apply_encoded_evictions(&mut encoded_profiles, &candidates[..removal_count])?;
            apply_history_evictions(&mut profiles, &candidates[..removal_count], &mut evicted);
        }

        evicted.sort_unstable_by_key(|candidate| {
            (
                candidate.completed_at_unix_ms,
                candidate.instance,
                candidate.history_id,
            )
        });
        let history_evictions = evicted
            .into_iter()
            .map(HistoryRetentionCandidate::identity)
            .collect();
        Ok(Self {
            profiles,
            history_evictions,
        })
    }

    pub fn profiles(&self) -> &[ProfileWorkspaceSnapshot] {
        &self.profiles
    }

    pub fn into_profiles(self) -> Vec<ProfileWorkspaceSnapshot> {
        self.profiles
    }

    pub const fn history_evicted(&self) -> usize {
        self.history_evictions.len()
    }

    pub fn history_evictions(&self) -> &[WorkspaceHistoryEvictionIdentity] {
        &self.history_evictions
    }
}

const RETENTION_CHECKSUM_PLACEHOLDER: &str =
    "0000000000000000000000000000000000000000000000000000000000000000";
const JSON_NULL_BYTES: u64 = 4;

#[derive(Serialize)]
struct WorkspaceShardSizeProbe<'a> {
    schema: &'a str,
    instance_id: ProfileInstanceId,
    generation: u64,
    payload_length: u64,
    payload_checksum: &'a str,
    payload: (),
}

#[derive(Serialize)]
struct ProfileWorkspaceSizeProbe<'a> {
    instance_id: ProfileInstanceId,
    profile_id: &'a ProfileId,
    persistence_enabled: bool,
    editor_tabs: &'a [EditorTabSnapshot],
    selected_editor_tab_id: Option<u64>,
    geometry: WorkspaceGeometrySnapshot,
    history: &'a [WorkspaceHistoryEntry],
}

#[derive(Default)]
struct JsonByteCounter {
    bytes: u64,
}

impl std::io::Write for JsonByteCounter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let length = u64::try_from(buffer.len())
            .map_err(|_| std::io::Error::other("JSON byte count overflow"))?;
        self.bytes = self
            .bytes
            .checked_add(length)
            .ok_or_else(|| std::io::Error::other("JSON byte count overflow"))?;
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ConservativeEncodedProfileSize {
    shard_bytes: u64,
    committed_bytes: u64,
}

struct ProfileEncodedAccounting {
    payload_fixed_bytes: u64,
    retained_history_entries: usize,
    retained_history_json_bytes: u64,
    history_json_bytes: HashMap<u64, u64>,
    shard_fixed_bytes: u64,
    manifest_fixed_bytes: u64,
}

impl ProfileEncodedAccounting {
    fn new(snapshot: &ProfileWorkspaceSnapshot) -> Result<Self, WorkspaceRetentionError> {
        let payload_fixed_bytes = encoded_json_bytes(&ProfileWorkspaceSizeProbe {
            instance_id: snapshot.instance_id,
            profile_id: &snapshot.profile_id,
            persistence_enabled: snapshot.persistence_enabled,
            editor_tabs: &snapshot.editor_tabs,
            selected_editor_tab_id: snapshot.selected_editor_tab_id,
            geometry: snapshot.geometry,
            history: &[],
        })?;
        let mut history_json_bytes = HashMap::with_capacity(snapshot.history.len());
        let mut retained_history_json_bytes = 0_u64;
        for entry in &snapshot.history {
            let entry_bytes = encoded_json_bytes(entry)?;
            if history_json_bytes.insert(entry.id, entry_bytes).is_some() {
                return Err(WorkspaceRetentionError::AccountingFailed);
            }
            retained_history_json_bytes = retained_history_json_bytes
                .checked_add(entry_bytes)
                .ok_or(WorkspaceRetentionError::AccountingFailed)?;
        }
        let (shard_fixed_bytes, manifest_fixed_bytes) =
            conservative_encoded_overheads(snapshot.instance_id())?;
        Ok(Self {
            payload_fixed_bytes,
            retained_history_entries: snapshot.history.len(),
            retained_history_json_bytes,
            history_json_bytes,
            shard_fixed_bytes,
            manifest_fixed_bytes,
        })
    }

    fn encoded_size(&self) -> Result<ConservativeEncodedProfileSize, WorkspaceRetentionError> {
        self.encoded_size_after_removals(0, 0)
    }

    fn encoded_size_after_removals(
        &self,
        removed_entries: usize,
        removed_json_bytes: u64,
    ) -> Result<ConservativeEncodedProfileSize, WorkspaceRetentionError> {
        let retained_entries = self
            .retained_history_entries
            .checked_sub(removed_entries)
            .ok_or(WorkspaceRetentionError::AccountingFailed)?;
        let retained_json_bytes = self
            .retained_history_json_bytes
            .checked_sub(removed_json_bytes)
            .ok_or(WorkspaceRetentionError::AccountingFailed)?;
        let history_commas = u64::try_from(retained_entries.saturating_sub(1))
            .map_err(|_| WorkspaceRetentionError::AccountingFailed)?;
        let payload_bytes = self
            .payload_fixed_bytes
            .checked_add(retained_json_bytes)
            .and_then(|value| value.checked_add(history_commas))
            .ok_or(WorkspaceRetentionError::AccountingFailed)?;
        let shard_bytes = self
            .shard_fixed_bytes
            .checked_add(decimal_digits(payload_bytes))
            .and_then(|value| value.checked_add(payload_bytes))
            .ok_or(WorkspaceRetentionError::AccountingFailed)?;
        let manifest_bytes = self
            .manifest_fixed_bytes
            .checked_add(decimal_digits(shard_bytes))
            .ok_or(WorkspaceRetentionError::AccountingFailed)?;
        let committed_bytes = shard_bytes
            .checked_add(manifest_bytes)
            .ok_or(WorkspaceRetentionError::AccountingFailed)?;
        Ok(ConservativeEncodedProfileSize {
            shard_bytes,
            committed_bytes,
        })
    }

    fn history_json_bytes(&self, history_id: u64) -> Result<u64, WorkspaceRetentionError> {
        self.history_json_bytes
            .get(&history_id)
            .copied()
            .ok_or(WorkspaceRetentionError::AccountingFailed)
    }

    fn remove_history(&mut self, history_id: u64) -> Result<(), WorkspaceRetentionError> {
        let entry_bytes = self
            .history_json_bytes
            .remove(&history_id)
            .ok_or(WorkspaceRetentionError::AccountingFailed)?;
        self.retained_history_entries = self
            .retained_history_entries
            .checked_sub(1)
            .ok_or(WorkspaceRetentionError::AccountingFailed)?;
        self.retained_history_json_bytes = self
            .retained_history_json_bytes
            .checked_sub(entry_bytes)
            .ok_or(WorkspaceRetentionError::AccountingFailed)?;
        Ok(())
    }
}

fn history_retention_candidates(
    profiles: &[ProfileWorkspaceSnapshot],
    selected_profile_index: Option<usize>,
) -> Vec<HistoryRetentionCandidate> {
    let mut candidates = profiles
        .iter()
        .enumerate()
        .filter(|(profile_index, _)| {
            selected_profile_index.is_none_or(|selected| selected == *profile_index)
        })
        .flat_map(|(profile_index, profile)| {
            let instance = *profile.instance_id().as_bytes();
            profile
                .history
                .iter()
                .filter(|entry| !matches!(entry.status, WorkspaceHistoryStatus::OutcomeUnknown))
                .map(move |entry| HistoryRetentionCandidate {
                    completed_at_unix_ms: entry.completed_at_unix_ms,
                    instance,
                    history_id: entry.id,
                    profile_index,
                })
        })
        .collect::<Vec<_>>();
    candidates.sort_unstable_by_key(|candidate| {
        (
            candidate.completed_at_unix_ms,
            candidate.instance,
            candidate.history_id,
        )
    });
    candidates
}

fn apply_history_evictions(
    profiles: &mut [ProfileWorkspaceSnapshot],
    candidates: &[HistoryRetentionCandidate],
    evicted: &mut Vec<HistoryRetentionCandidate>,
) {
    let mut selected = HashMap::<usize, HashSet<u64>>::new();
    for candidate in candidates {
        selected
            .entry(candidate.profile_index)
            .or_default()
            .insert(candidate.history_id);
    }
    for (profile_index, history_ids) in selected {
        profiles[profile_index]
            .history
            .retain(|entry| !history_ids.contains(&entry.id));
    }
    evicted.extend_from_slice(candidates);
}

fn apply_encoded_evictions(
    profiles: &mut [ProfileEncodedAccounting],
    candidates: &[HistoryRetentionCandidate],
) -> Result<(), WorkspaceRetentionError> {
    for candidate in candidates {
        let profile = profiles
            .get_mut(candidate.profile_index)
            .ok_or(WorkspaceRetentionError::AccountingFailed)?;
        profile.remove_history(candidate.history_id)?;
    }
    Ok(())
}

fn minimum_profile_evictions_to_fit(
    profile: &ProfileEncodedAccounting,
    candidates: &[HistoryRetentionCandidate],
    limit: u64,
) -> Result<usize, WorkspaceRetentionError> {
    let prefix_sizes = profile_shard_bytes_after_prefixes(profile, candidates)?;
    minimum_prefix_to_fit(&prefix_sizes, limit).ok_or(WorkspaceRetentionError::RetentionExhausted(
        WorkspaceRetentionLimit::ProfileShardBytes,
    ))
}

fn minimum_total_evictions_to_fit(
    profiles: &[ProfileEncodedAccounting],
    candidates: &[HistoryRetentionCandidate],
    limit: u64,
) -> Result<usize, WorkspaceRetentionError> {
    let prefix_sizes = encoded_store_bytes_after_prefixes(profiles, candidates)?;
    minimum_prefix_to_fit(&prefix_sizes, limit).ok_or(WorkspaceRetentionError::RetentionExhausted(
        WorkspaceRetentionLimit::TotalStoreBytes,
    ))
}

fn minimum_prefix_to_fit(prefix_sizes: &[u64], limit: u64) -> Option<usize> {
    let index = prefix_sizes.partition_point(|size| *size > limit);
    (index < prefix_sizes.len()).then_some(index + 1)
}

fn profile_shard_bytes_after_prefixes(
    profile: &ProfileEncodedAccounting,
    candidates: &[HistoryRetentionCandidate],
) -> Result<Vec<u64>, WorkspaceRetentionError> {
    let mut removed_json_bytes = 0_u64;
    candidates
        .iter()
        .enumerate()
        .map(|(index, candidate)| {
            removed_json_bytes = removed_json_bytes
                .checked_add(profile.history_json_bytes(candidate.history_id)?)
                .ok_or(WorkspaceRetentionError::AccountingFailed)?;
            profile
                .encoded_size_after_removals(index + 1, removed_json_bytes)
                .map(|size| size.shard_bytes)
        })
        .collect()
}

fn conservative_encoded_store_bytes(
    profiles: &[ProfileEncodedAccounting],
) -> Result<u64, WorkspaceRetentionError> {
    profiles.iter().try_fold(0_u64, |total, profile| {
        total
            .checked_add(profile.encoded_size()?.committed_bytes)
            .ok_or(WorkspaceRetentionError::AccountingFailed)
    })
}

fn encoded_store_bytes_after_prefixes(
    profiles: &[ProfileEncodedAccounting],
    candidates: &[HistoryRetentionCandidate],
) -> Result<Vec<u64>, WorkspaceRetentionError> {
    let mut removed_entries = vec![0_usize; profiles.len()];
    let mut removed_json_bytes = vec![0_u64; profiles.len()];
    let mut profile_sizes = profiles
        .iter()
        .map(ProfileEncodedAccounting::encoded_size)
        .map(|result| result.map(|size| size.committed_bytes))
        .collect::<Result<Vec<_>, _>>()?;
    let mut total = profile_sizes.iter().try_fold(0_u64, |total, size| {
        total
            .checked_add(*size)
            .ok_or(WorkspaceRetentionError::AccountingFailed)
    })?;
    let mut prefix_sizes = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        let profile = profiles
            .get(candidate.profile_index)
            .ok_or(WorkspaceRetentionError::AccountingFailed)?;
        let entry_bytes = profile.history_json_bytes(candidate.history_id)?;
        removed_entries[candidate.profile_index] = removed_entries[candidate.profile_index]
            .checked_add(1)
            .ok_or(WorkspaceRetentionError::AccountingFailed)?;
        removed_json_bytes[candidate.profile_index] = removed_json_bytes[candidate.profile_index]
            .checked_add(entry_bytes)
            .ok_or(WorkspaceRetentionError::AccountingFailed)?;
        let prior = profile_sizes[candidate.profile_index];
        let next = profile
            .encoded_size_after_removals(
                removed_entries[candidate.profile_index],
                removed_json_bytes[candidate.profile_index],
            )?
            .committed_bytes;
        total = total
            .checked_sub(prior)
            .and_then(|value| value.checked_add(next))
            .ok_or(WorkspaceRetentionError::AccountingFailed)?;
        profile_sizes[candidate.profile_index] = next;
        prefix_sizes.push(total);
    }
    Ok(prefix_sizes)
}

fn conservative_encoded_overheads(
    instance_id: ProfileInstanceId,
) -> Result<(u64, u64), WorkspaceRetentionError> {
    encoded_overheads_at_generation(instance_id, u64::MAX)
}

fn encoded_overheads_at_generation(
    instance_id: ProfileInstanceId,
    generation: u64,
) -> Result<(u64, u64), WorkspaceRetentionError> {
    let shard_probe = WorkspaceShardSizeProbe {
        schema: SHARD_SCHEMA,
        instance_id,
        generation,
        payload_length: 0,
        payload_checksum: RETENTION_CHECKSUM_PLACEHOLDER,
        payload: (),
    };
    let shard_probe_bytes = encoded_json_bytes(&shard_probe)?;
    let shard_fixed_bytes = shard_probe_bytes
        .checked_sub(decimal_digits(0))
        .and_then(|value| value.checked_sub(JSON_NULL_BYTES))
        .ok_or(WorkspaceRetentionError::AccountingFailed)?;
    let manifest_probe = WorkspaceManifest {
        schema: MANIFEST_SCHEMA.to_owned(),
        instance_id,
        generation,
        shard: shard_name(generation),
        shard_length: 0,
        checksum: RETENTION_CHECKSUM_PLACEHOLDER.to_owned(),
    };
    let manifest_fixed_bytes = encoded_json_bytes(&manifest_probe)?
        .checked_sub(decimal_digits(0))
        .ok_or(WorkspaceRetentionError::AccountingFailed)?;
    Ok((shard_fixed_bytes, manifest_fixed_bytes))
}

fn encoded_json_bytes<T: Serialize>(value: &T) -> Result<u64, WorkspaceRetentionError> {
    let mut counter = JsonByteCounter::default();
    serde_json::to_writer(&mut counter, value)
        .map_err(|_| WorkspaceRetentionError::AccountingFailed)?;
    Ok(counter.bytes)
}

const fn decimal_digits(mut value: u64) -> u64 {
    let mut digits = 1_u64;
    while value >= 10 {
        value /= 10;
        digits += 1;
    }
    digits
}

#[cfg(test)]
fn conservative_encoded_profile_size_after_evictions(
    profile: &ProfileWorkspaceSnapshot,
    candidates: &[HistoryRetentionCandidate],
    removal_count: usize,
) -> Result<ConservativeEncodedProfileSize, WorkspaceRetentionError> {
    let removed = candidates
        .iter()
        .take(removal_count)
        .map(|candidate| candidate.history_id)
        .collect::<HashSet<_>>();
    let mut projected = profile.clone();
    projected
        .history
        .retain(|entry| !removed.contains(&entry.id));
    conservative_encoded_profile_size(&projected)
}

#[cfg(test)]
fn conservative_encoded_projected_store_bytes(
    profiles: &[ProfileWorkspaceSnapshot],
    candidates: &[HistoryRetentionCandidate],
    removal_count: usize,
) -> Result<u64, WorkspaceRetentionError> {
    let mut removed = HashMap::<usize, HashSet<u64>>::new();
    for candidate in candidates.iter().take(removal_count) {
        removed
            .entry(candidate.profile_index)
            .or_default()
            .insert(candidate.history_id);
    }
    profiles
        .iter()
        .enumerate()
        .try_fold(0_u64, |total, (profile_index, profile)| {
            let size = if let Some(history_ids) = removed.get(&profile_index) {
                let mut projected = profile.clone();
                projected
                    .history
                    .retain(|entry| !history_ids.contains(&entry.id));
                conservative_encoded_profile_size(&projected)?
            } else {
                conservative_encoded_profile_size(profile)?
            };
            total
                .checked_add(size.committed_bytes)
                .ok_or(WorkspaceRetentionError::AccountingFailed)
        })
}

#[cfg(test)]
fn conservative_encoded_profile_size(
    snapshot: &ProfileWorkspaceSnapshot,
) -> Result<ConservativeEncodedProfileSize, WorkspaceRetentionError> {
    ProfileEncodedAccounting::new(snapshot)?.encoded_size()
}

#[cfg(feature = "desktop")]
pub(crate) fn conservative_encoded_profile_bytes(
    snapshot: &ProfileWorkspaceSnapshot,
) -> Result<(u64, u64), WorkspaceRetentionError> {
    let size = ProfileEncodedAccounting::new(snapshot)?.encoded_size()?;
    Ok((size.shard_bytes, size.committed_bytes))
}

#[cfg(any(feature = "desktop", test))]
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct EncodedProfileByteAccounting {
    instance_id: ProfileInstanceId,
    payload_bytes: u64,
}

#[cfg(any(feature = "desktop", test))]
impl std::fmt::Debug for EncodedProfileByteAccounting {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EncodedProfileByteAccounting")
            .field("instance_id", &"<redacted>")
            .field("payload_bytes", &self.payload_bytes)
            .finish()
    }
}

#[cfg(any(feature = "desktop", test))]
impl EncodedProfileByteAccounting {
    pub(crate) fn new(
        snapshot: &ProfileWorkspaceSnapshot,
    ) -> Result<Self, WorkspaceRetentionError> {
        Ok(Self {
            instance_id: snapshot.instance_id(),
            payload_bytes: encoded_json_bytes(snapshot)?,
        })
    }

    pub(crate) fn encoded_bytes_at_generation(
        self,
        generation: u64,
    ) -> Result<(u64, u64), WorkspaceRetentionError> {
        if generation == 0 {
            return Err(WorkspaceRetentionError::AccountingFailed);
        }
        let (shard_fixed_bytes, manifest_fixed_bytes) =
            encoded_overheads_at_generation(self.instance_id, generation)?;
        let shard_bytes = shard_fixed_bytes
            .checked_add(decimal_digits(self.payload_bytes))
            .and_then(|value| value.checked_add(self.payload_bytes))
            .ok_or(WorkspaceRetentionError::AccountingFailed)?;
        let manifest_bytes = manifest_fixed_bytes
            .checked_add(decimal_digits(shard_bytes))
            .ok_or(WorkspaceRetentionError::AccountingFailed)?;
        let committed_bytes = shard_bytes
            .checked_add(manifest_bytes)
            .ok_or(WorkspaceRetentionError::AccountingFailed)?;
        Ok((shard_bytes, committed_bytes))
    }
}

#[cfg(test)]
pub(crate) fn encoded_profile_bytes_at_generation(
    snapshot: &ProfileWorkspaceSnapshot,
    generation: u64,
) -> Result<(u64, u64), WorkspaceRetentionError> {
    EncodedProfileByteAccounting::new(snapshot)?.encoded_bytes_at_generation(generation)
}

#[cfg(all(test, feature = "desktop"))]
pub(crate) fn conservative_encoded_profile_bytes_for_test(
    snapshot: &ProfileWorkspaceSnapshot,
) -> Result<(u64, u64), WorkspaceRetentionError> {
    let size = conservative_encoded_profile_size(snapshot)?;
    Ok((size.shard_bytes, size.committed_bytes))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum WorkspaceSnapshotError {
    #[error("editor id is invalid")]
    InvalidEditorId,
    #[error("editor title is invalid")]
    InvalidEditorTitle,
    #[error("editor source exceeds the retained bound")]
    EditorSourceTooLarge,
    #[error("database binding exceeds the retained bound")]
    DatabaseBindingTooLarge,
    #[error("editor cursor is outside the source")]
    InvalidEditorCursor,
    #[error("editor selection is outside the source")]
    InvalidEditorSelection,
    #[error("history id is invalid")]
    InvalidHistoryId,
    #[error("history source omission state is invalid")]
    InvalidHistorySource,
    #[error("workspace geometry is invalid")]
    InvalidGeometry,
    #[error("profile id is invalid")]
    InvalidProfileId,
    #[error("workspace has too many editor tabs")]
    TooManyEditorTabs,
    #[error("workspace store has too many editor tabs")]
    TooManyEditorTabsTotal,
    #[error("workspace has too many history entries")]
    TooManyHistoryEntries,
    #[error("workspace store has too many history entries")]
    TooManyHistoryEntriesTotal,
    #[error("disabled persistence retains content")]
    DisabledPersistenceHasContent,
    #[error("editor tab ids are not unique")]
    DuplicateEditorId,
    #[error("selected editor tab does not exist")]
    UnknownSelectedEditor,
    #[error("history ids are not unique")]
    DuplicateHistoryId,
    #[error("profile instance ids are not unique")]
    DuplicateProfileInstance,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkspaceStoreMode {
    ReadWrite,
    ReadOnly,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkspaceReadOnlyReason {
    WriterBusy,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkspaceIoKind {
    NotFound,
    PermissionDenied,
    AlreadyExists,
    WouldBlock,
    InvalidInput,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkspaceStoreWarning {
    CorruptProfileQuarantined,
}

#[derive(Clone, PartialEq, Eq)]
pub struct WorkspaceCommit {
    generation: u64,
    committed_bytes: u64,
    checksum: String,
    warnings: Vec<WorkspaceStoreWarning>,
}

impl std::fmt::Debug for WorkspaceCommit {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WorkspaceCommit")
            .field("generation", &self.generation)
            .field("committed_bytes", &self.committed_bytes)
            .field("checksum", &"<redacted>")
            .field("warnings", &self.warnings)
            .finish()
    }
}

impl WorkspaceCommit {
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    pub const fn committed_bytes(&self) -> u64 {
        self.committed_bytes
    }

    pub fn checksum(&self) -> &str {
        &self.checksum
    }

    pub fn warnings(&self) -> &[WorkspaceStoreWarning] {
        &self.warnings
    }
}

#[derive(Debug, thiserror::Error)]
pub enum WorkspaceStoreError {
    #[error("workspace config path is invalid")]
    InvalidConfigPath,
    #[error("workspace store is read-only")]
    ReadOnly,
    #[error("workspace path type or permissions are unsafe")]
    UnsafePath,
    #[error("workspace manifest is corrupt")]
    CorruptManifest,
    #[error("workspace shard is corrupt")]
    CorruptShard,
    #[error("workspace schema version is unsupported")]
    UnsupportedVersion,
    #[error("workspace snapshot is invalid")]
    Snapshot(#[from] WorkspaceSnapshotError),
    #[error("workspace shard exceeds the retained bound")]
    ShardTooLarge,
    #[error("workspace store exceeds the retained bound")]
    StoreTooLarge,
    #[error("workspace changed outside this writer")]
    ExternalChange,
    #[error("workspace commit is visible but durability is uncertain")]
    DurabilityUnknown,
    #[error("workspace writer needs reconciliation")]
    RecoveryRequired,
    #[error("workspace writer state is unavailable")]
    WriterUnavailable,
    #[error("workspace I/O failed: {0:?}")]
    Io(WorkspaceIoKind),
}

impl From<std::io::Error> for WorkspaceStoreError {
    fn from(source: std::io::Error) -> Self {
        let kind = match source.kind() {
            std::io::ErrorKind::NotFound => WorkspaceIoKind::NotFound,
            std::io::ErrorKind::PermissionDenied => WorkspaceIoKind::PermissionDenied,
            std::io::ErrorKind::AlreadyExists => WorkspaceIoKind::AlreadyExists,
            std::io::ErrorKind::WouldBlock => WorkspaceIoKind::WouldBlock,
            std::io::ErrorKind::InvalidInput => WorkspaceIoKind::InvalidInput,
            _ => WorkspaceIoKind::Other,
        };
        Self::Io(kind)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceManifest {
    schema: String,
    instance_id: ProfileInstanceId,
    generation: u64,
    shard: String,
    shard_length: u64,
    checksum: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceShard {
    schema: String,
    instance_id: ProfileInstanceId,
    generation: u64,
    payload_length: u64,
    payload_checksum: String,
    payload: ProfileWorkspaceSnapshot,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PrivateFileSnapshot {
    bytes: Vec<u8>,
    fingerprint: PrivateFileFingerprint,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PrivateFileFingerprint {
    device: u64,
    inode: u64,
    mode: u32,
    links: u64,
    length: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ManagedEntryKind {
    Directory,
    RegularFile,
    Symlink,
    Other,
}

impl ManagedEntryKind {
    const fn marker(self) -> &'static str {
        match self {
            Self::Directory => "d",
            Self::RegularFile => "f",
            Self::Symlink => "l",
            Self::Other => "o",
        }
    }

    fn parse(marker: &str) -> Result<Self, WorkspaceStoreError> {
        match marker {
            "d" => Ok(Self::Directory),
            "f" => Ok(Self::RegularFile),
            "l" => Ok(Self::Symlink),
            "o" => Ok(Self::Other),
            _ => Err(WorkspaceStoreError::UnsafePath),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ManagedEntryIdentity {
    device: u64,
    inode: u64,
    kind: ManagedEntryKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ClearedTombstone {
    instance_id: ProfileInstanceId,
    identity: ManagedEntryIdentity,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProfileCorruptState {
    Manifest,
    Shard,
    UnsupportedVersion,
    UnsafePath,
}

impl ProfileCorruptState {
    const fn bytes(self) -> &'static [u8] {
        match self {
            Self::Manifest => b"corrupt-manifest-v1\n",
            Self::Shard => b"corrupt-shard-v1\n",
            Self::UnsupportedVersion => b"unsupported-version-v1\n",
            Self::UnsafePath => b"unsafe-path-v1\n",
        }
    }

    fn parse(bytes: &[u8]) -> Result<Self, WorkspaceStoreError> {
        match bytes {
            b"corrupt-manifest-v1\n" => Ok(Self::Manifest),
            b"corrupt-shard-v1\n" => Ok(Self::Shard),
            b"unsupported-version-v1\n" => Ok(Self::UnsupportedVersion),
            b"unsafe-path-v1\n" => Ok(Self::UnsafePath),
            _ => Err(WorkspaceStoreError::CorruptManifest),
        }
    }

    const fn marker(self) -> &'static str {
        match self {
            Self::Manifest => "manifest",
            Self::Shard => "shard",
            Self::UnsupportedVersion => "unsupported",
            Self::UnsafePath => "unsafe",
        }
    }

    fn parse_marker(marker: &str) -> Result<Self, WorkspaceStoreError> {
        match marker {
            "manifest" => Ok(Self::Manifest),
            "shard" => Ok(Self::Shard),
            "unsupported" => Ok(Self::UnsupportedVersion),
            "unsafe" => Ok(Self::UnsafePath),
            _ => Err(WorkspaceStoreError::CorruptManifest),
        }
    }

    const fn error(self) -> WorkspaceStoreError {
        match self {
            Self::Manifest => WorkspaceStoreError::CorruptManifest,
            Self::Shard => WorkspaceStoreError::CorruptShard,
            Self::UnsupportedVersion => WorkspaceStoreError::UnsupportedVersion,
            Self::UnsafePath => WorkspaceStoreError::UnsafePath,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RootProfileCorruptMarker {
    ProfileEntry {
        state: ProfileCorruptState,
        entry: ManagedEntryIdentity,
    },
    InternalState {
        state: ProfileCorruptState,
        profile: ManagedEntryIdentity,
        entry: ManagedEntryIdentity,
    },
    Unbound(ProfileCorruptState),
}

impl RootProfileCorruptMarker {
    fn bytes(self) -> Vec<u8> {
        match self {
            Self::ProfileEntry { state, entry } => format!(
                "profile-entry-v2:{}:{}:{:016x}:{:016x}\n",
                state.marker(),
                entry.kind.marker(),
                entry.device,
                entry.inode
            )
            .into_bytes(),
            Self::InternalState {
                state,
                profile,
                entry,
            } => format!(
                "internal-state-v2:{}:{}:{:016x}:{:016x}:{}:{:016x}:{:016x}\n",
                state.marker(),
                profile.kind.marker(),
                profile.device,
                profile.inode,
                entry.kind.marker(),
                entry.device,
                entry.inode
            )
            .into_bytes(),
            Self::Unbound(state) => format!("unbound-v2:{}\n", state.marker()).into_bytes(),
        }
    }

    fn parse(bytes: &[u8]) -> Result<Self, WorkspaceStoreError> {
        let marker = std::str::from_utf8(bytes)
            .map_err(|_| WorkspaceStoreError::CorruptManifest)?
            .strip_suffix('\n')
            .ok_or(WorkspaceStoreError::CorruptManifest)?;
        let mut parts = marker.split(':');
        match parts.next() {
            Some("profile-entry-v2") => {
                let state = ProfileCorruptState::parse_marker(
                    parts.next().ok_or(WorkspaceStoreError::CorruptManifest)?,
                )?;
                let entry = parse_managed_entry_identity_parts(&mut parts)?;
                if parts.next().is_some() {
                    return Err(WorkspaceStoreError::CorruptManifest);
                }
                return Ok(Self::ProfileEntry { state, entry });
            }
            Some("internal-state-v2") => {
                let state = ProfileCorruptState::parse_marker(
                    parts.next().ok_or(WorkspaceStoreError::CorruptManifest)?,
                )?;
                let profile = parse_managed_entry_identity_parts(&mut parts)?;
                let entry = parse_managed_entry_identity_parts(&mut parts)?;
                if parts.next().is_some() {
                    return Err(WorkspaceStoreError::CorruptManifest);
                }
                return Ok(Self::InternalState {
                    state,
                    profile,
                    entry,
                });
            }
            Some("unbound-v2") => {
                let state = ProfileCorruptState::parse_marker(
                    parts.next().ok_or(WorkspaceStoreError::CorruptManifest)?,
                )?;
                if parts.next().is_some() {
                    return Err(WorkspaceStoreError::CorruptManifest);
                }
                return Ok(Self::Unbound(state));
            }
            _ => {}
        }
        if let Some(state) = bytes.strip_prefix(b"profile-entry:") {
            return ProfileCorruptState::parse(state).map(Self::Unbound);
        }
        if let Some(state) = bytes.strip_prefix(b"internal-state:") {
            return ProfileCorruptState::parse(state).map(Self::Unbound);
        }
        Err(WorkspaceStoreError::CorruptManifest)
    }

    const fn state(self) -> ProfileCorruptState {
        match self {
            Self::ProfileEntry { state, .. }
            | Self::InternalState { state, .. }
            | Self::Unbound(state) => state,
        }
    }
}

pub struct WorkspaceStore {
    profiles: PathBuf,
    root_name: OsString,
    parent_directory: fs::File,
    root_directory: fs::File,
    profiles_directory: fs::File,
    quarantine_directory: fs::File,
    mode: WorkspaceStoreMode,
    _lock: fs::File,
    writer: Mutex<()>,
    recovery_required: AtomicBool,
}

impl std::fmt::Debug for WorkspaceStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WorkspaceStore")
            .field("root", &"<redacted>")
            .field("mode", &self.mode)
            .finish_non_exhaustive()
    }
}

impl WorkspaceStore {
    pub fn open(config_path: &Path) -> Result<Self, WorkspaceStoreError> {
        let root = workspace_root_for_config(config_path)?;
        let root_name = root
            .file_name()
            .ok_or(WorkspaceStoreError::InvalidConfigPath)?
            .to_owned();
        let parent_path = root
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let parent_directory = open_directory_handle(parent_path)?;
        let root_created = ensure_private_directory(&root)?;
        let root_directory = open_private_directory(&root)?;
        if root_created {
            sync_directory_handle(&root_directory)?;
            sync_directory_handle(&parent_directory)?;
        }
        let profiles = root.join("profiles");
        let quarantine = root.join("quarantine");
        let profiles_directory =
            ensure_private_directory_at(&root_directory, "profiles", &profiles)?;
        let quarantine_directory =
            ensure_private_directory_at(&root_directory, "quarantine", &quarantine)?;
        let lock_path = root.join("writer.lock");
        let lock = open_private_lock_at(&root_directory, "writer.lock", &lock_path)?;
        let mode =
            match rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
                Ok(()) => WorkspaceStoreMode::ReadWrite,
                Err(source)
                    if std::io::Error::from(source).kind() == std::io::ErrorKind::WouldBlock =>
                {
                    WorkspaceStoreMode::ReadOnly
                }
                Err(source) => return Err(WorkspaceStoreError::from(std::io::Error::from(source))),
            };
        let store = Self {
            profiles,
            root_name,
            parent_directory,
            root_directory,
            profiles_directory,
            quarantine_directory,
            mode,
            _lock: lock,
            writer: Mutex::new(()),
            recovery_required: AtomicBool::new(false),
        };
        store.validate_store_chain()?;
        if mode == WorkspaceStoreMode::ReadWrite {
            store.cleanup_cleared_profiles()?;
            store.cleanup_root_profile_marker_temps()?;
            store.recover_root_profile_corrupt_markers()?;
            trim_quarantine(&store.quarantine_directory)?;
        }
        store.validate_store_chain()?;
        Ok(store)
    }

    pub const fn mode(&self) -> WorkspaceStoreMode {
        self.mode
    }

    pub const fn read_only_reason(&self) -> Option<WorkspaceReadOnlyReason> {
        match self.mode {
            WorkspaceStoreMode::ReadWrite => None,
            WorkspaceStoreMode::ReadOnly => Some(WorkspaceReadOnlyReason::WriterBusy),
        }
    }

    fn validate_store_chain(&self) -> Result<(), WorkspaceStoreError> {
        validate_directory_entry_os(
            &self.parent_directory,
            &self.root_name,
            &self.root_directory,
        )?;
        validate_directory_entry(&self.root_directory, "profiles", &self.profiles_directory)?;
        validate_directory_entry(
            &self.root_directory,
            "quarantine",
            &self.quarantine_directory,
        )?;
        validate_private_file_entry(&self.root_directory, "writer.lock", &self._lock)?;
        Ok(())
    }

    pub fn commit(
        &self,
        snapshot: &ProfileWorkspaceSnapshot,
    ) -> Result<WorkspaceCommit, WorkspaceStoreError> {
        if self.mode != WorkspaceStoreMode::ReadWrite {
            return Err(WorkspaceStoreError::ReadOnly);
        }
        let _writer = self
            .writer
            .lock()
            .map_err(|_| WorkspaceStoreError::WriterUnavailable)?;
        if self.recovery_required.load(Ordering::Acquire) {
            return Err(WorkspaceStoreError::RecoveryRequired);
        }
        self.validate_store_chain()?;
        snapshot.validate()?;
        if let Some(marker) = self.root_profile_corrupt_marker_for_use(snapshot.instance_id())? {
            self.recover_root_profile_corruption(snapshot.instance_id(), marker)?;
            return Err(marker.state().error());
        }
        let (other_editor_tabs, other_history_entries, other_committed_bytes, warnings) =
            self.committed_usage_except(snapshot.instance_id())?;
        if other_editor_tabs
            .checked_add(snapshot.editor_tabs.len())
            .is_none_or(|total| total > MAX_EDITOR_TABS_TOTAL)
        {
            return Err(WorkspaceSnapshotError::TooManyEditorTabsTotal.into());
        }
        if other_history_entries
            .checked_add(snapshot.history.len())
            .is_none_or(|total| total > MAX_HISTORY_ENTRIES_TOTAL)
        {
            return Err(WorkspaceSnapshotError::TooManyHistoryEntriesTotal.into());
        }
        let profile_name = snapshot.instance_id().to_string();
        let profile_directory = self.profile_directory(snapshot.instance_id());
        let profile_handle = ensure_private_directory_at(
            &self.profiles_directory,
            &profile_name,
            &profile_directory,
        )?;
        validate_directory_entry(&self.profiles_directory, &profile_name, &profile_handle)?;
        match read_profile_corrupt_state(&profile_handle) {
            Ok(Some(state)) => return Err(state.error()),
            Ok(None) => {}
            Err(error) => {
                let Some(state) = known_profile_corruption(&error) else {
                    return Err(error);
                };
                return Err(self.profile_corruption_error(
                    &profile_handle,
                    snapshot.instance_id(),
                    state,
                    &[CORRUPT_STATE_FILE.to_owned()],
                ));
            }
        }
        let prior_manifest = read_optional_private_file_snapshot_at(
            &profile_handle,
            "manifest.json",
            MAX_MANIFEST_BYTES,
        )?;
        let (prior_generation, prior_shard_name) = match prior_manifest.as_ref() {
            Some(manifest_snapshot) => {
                let bytes = manifest_snapshot.bytes.as_slice();
                let manifest = parse_manifest(bytes)?;
                validate_manifest_reference(&manifest, snapshot.instance_id())?;
                let shard_bytes = read_private_file_at(
                    &profile_handle,
                    &manifest.shard,
                    MAX_PROFILE_SHARD_BYTES,
                )?;
                if shard_bytes.len() as u64 != manifest.shard_length
                    || checksum(&shard_bytes) != manifest.checksum
                {
                    return Err(WorkspaceStoreError::ExternalChange);
                }
                (manifest.generation, Some(manifest.shard))
            }
            None => (0, None),
        };
        let generation = next_profile_generation(&profile_handle, prior_generation)?;
        cleanup_profile_entries(&profile_handle, prior_shard_name.as_deref())?;
        sync_directory_handle(&profile_handle)?;

        let payload =
            serde_json::to_vec(snapshot).map_err(|_| WorkspaceStoreError::CorruptShard)?;
        if payload.len() > MAX_PROFILE_SHARD_BYTES {
            return Err(WorkspaceStoreError::ShardTooLarge);
        }
        let payload_checksum = checksum(&payload);
        let envelope = WorkspaceShard {
            schema: SHARD_SCHEMA.to_owned(),
            instance_id: snapshot.instance_id(),
            generation,
            payload_length: payload.len() as u64,
            payload_checksum,
            payload: snapshot.clone(),
        };
        let shard_bytes =
            serde_json::to_vec(&envelope).map_err(|_| WorkspaceStoreError::CorruptShard)?;
        if shard_bytes.len() > MAX_PROFILE_SHARD_BYTES {
            return Err(WorkspaceStoreError::ShardTooLarge);
        }
        let shard_checksum = checksum(&shard_bytes);
        let shard_name = shard_name(generation);
        let manifest = WorkspaceManifest {
            schema: MANIFEST_SCHEMA.to_owned(),
            instance_id: snapshot.instance_id(),
            generation,
            shard: shard_name.clone(),
            shard_length: shard_bytes.len() as u64,
            checksum: shard_checksum.clone(),
        };
        let manifest_bytes =
            serde_json::to_vec(&manifest).map_err(|_| WorkspaceStoreError::CorruptManifest)?;
        if manifest_bytes.len() > MAX_MANIFEST_BYTES {
            return Err(WorkspaceStoreError::CorruptManifest);
        }
        let prospective = other_committed_bytes
            .checked_add(shard_bytes.len() as u64)
            .and_then(|value| value.checked_add(manifest_bytes.len() as u64))
            .ok_or(WorkspaceStoreError::StoreTooLarge)?;
        if prospective > MAX_WORKSPACE_STORE_BYTES {
            return Err(WorkspaceStoreError::StoreTooLarge);
        }

        let (shard_temp, mut shard_file) = create_private_temp_at(&profile_handle, "shard")?;
        let shard_cleanup = TempCleanup::new(&profile_handle, shard_temp.clone())?;
        shard_file.write_all(&shard_bytes)?;
        shard_file.flush()?;
        shard_file.sync_all()?;
        rename_no_replace_at(&profile_handle, &shard_temp, &shard_name).map_err(|source| {
            if source == rustix::io::Errno::EXIST {
                WorkspaceStoreError::ExternalChange
            } else {
                workspace_error_from_errno(source)
            }
        })?;
        shard_cleanup.disarm();
        let expected_shard_fingerprint = private_file_fingerprint(
            &rustix::fs::fstat(&shard_file).map_err(workspace_error_from_errno)?,
        )?;
        if optional_private_file_fingerprint_at(&profile_handle, &shard_name)?
            != Some(expected_shard_fingerprint)
        {
            return Err(WorkspaceStoreError::ExternalChange);
        }
        drop(shard_file);
        sync_directory_handle(&profile_handle)?;

        let (manifest_temp, mut manifest_file) =
            create_private_temp_at(&profile_handle, "manifest")?;
        let manifest_cleanup = TempCleanup::new(&profile_handle, manifest_temp.clone())?;
        manifest_file.write_all(&manifest_bytes)?;
        manifest_file.flush()?;
        manifest_file.sync_all()?;
        if read_optional_private_file_snapshot_at(
            &profile_handle,
            "manifest.json",
            MAX_MANIFEST_BYTES,
        )? != prior_manifest
        {
            return Err(WorkspaceStoreError::ExternalChange);
        }
        replace_at(&profile_handle, &manifest_temp, "manifest.json")?;
        manifest_cleanup.disarm();
        let expected_manifest_fingerprint = match rustix::fs::fstat(&manifest_file)
            .map_err(workspace_error_from_errno)
            .and_then(|stat| private_file_fingerprint(&stat))
        {
            Ok(fingerprint) => fingerprint,
            Err(_) => {
                self.recovery_required.store(true, Ordering::Release);
                return Err(WorkspaceStoreError::DurabilityUnknown);
            }
        };
        let manifest_destination_matches =
            optional_private_file_fingerprint_at(&profile_handle, "manifest.json")
                .is_ok_and(|fingerprint| fingerprint == Some(expected_manifest_fingerprint));
        if !manifest_destination_matches {
            self.recovery_required.store(true, Ordering::Release);
            return Err(WorkspaceStoreError::DurabilityUnknown);
        }
        drop(manifest_file);
        if sync_directory_handle(&profile_handle).is_err() {
            self.recovery_required.store(true, Ordering::Release);
            return Err(WorkspaceStoreError::DurabilityUnknown);
        }
        if cleanup_profile_entries(&profile_handle, Some(&shard_name)).is_err()
            || sync_directory_handle(&profile_handle).is_err()
        {
            self.recovery_required.store(true, Ordering::Release);
            return Err(WorkspaceStoreError::DurabilityUnknown);
        }
        if self.validate_store_chain().is_err()
            || validate_directory_entry(&self.profiles_directory, &profile_name, &profile_handle)
                .is_err()
        {
            self.recovery_required.store(true, Ordering::Release);
            return Err(WorkspaceStoreError::DurabilityUnknown);
        }
        let committed_files_match = (|| -> Result<bool, WorkspaceStoreError> {
            let Some(final_manifest) = read_optional_private_file_snapshot_at(
                &profile_handle,
                "manifest.json",
                MAX_MANIFEST_BYTES,
            )?
            else {
                return Ok(false);
            };
            if final_manifest.bytes != manifest_bytes
                || final_manifest.fingerprint != expected_manifest_fingerprint
            {
                return Ok(false);
            }
            let parsed = parse_manifest(&final_manifest.bytes)?;
            validate_manifest_reference(&parsed, snapshot.instance_id())?;
            let Some(final_shard) = read_optional_private_file_snapshot_at(
                &profile_handle,
                &shard_name,
                MAX_PROFILE_SHARD_BYTES,
            )?
            else {
                return Ok(false);
            };
            Ok(parsed.generation == generation
                && parsed.shard == shard_name
                && parsed.shard_length == shard_bytes.len() as u64
                && parsed.checksum == shard_checksum
                && final_shard.bytes == shard_bytes
                && final_shard.fingerprint == expected_shard_fingerprint)
        })();
        if !matches!(committed_files_match, Ok(true))
            || self.validate_store_chain().is_err()
            || validate_directory_entry(&self.profiles_directory, &profile_name, &profile_handle)
                .is_err()
        {
            self.recovery_required.store(true, Ordering::Release);
            return Err(WorkspaceStoreError::DurabilityUnknown);
        }
        Ok(WorkspaceCommit {
            generation,
            committed_bytes: (shard_bytes.len() as u64)
                .checked_add(manifest_bytes.len() as u64)
                .ok_or(WorkspaceStoreError::StoreTooLarge)?,
            checksum: shard_checksum,
            warnings,
        })
    }

    pub fn load(
        &self,
        instance_id: ProfileInstanceId,
    ) -> Result<Option<ProfileWorkspaceSnapshot>, WorkspaceStoreError> {
        self.load_with_metadata(instance_id)
            .map(|(snapshot, _, _)| snapshot)
    }

    pub(crate) fn load_with_metadata(
        &self,
        instance_id: ProfileInstanceId,
    ) -> Result<(Option<ProfileWorkspaceSnapshot>, Option<u64>, u64), WorkspaceStoreError> {
        let _writer = if self.mode == WorkspaceStoreMode::ReadWrite {
            Some(
                self.writer
                    .lock()
                    .map_err(|_| WorkspaceStoreError::WriterUnavailable)?,
            )
        } else {
            None
        };
        self.validate_store_chain()?;
        if let Some(marker) = self.root_profile_corrupt_marker_for_use(instance_id)? {
            self.recover_root_profile_corruption(instance_id, marker)?;
            return Err(marker.state().error());
        }
        let profile_name = instance_id.to_string();
        let profile_directory = self.profile_directory(instance_id);
        let profile_handle = match open_optional_private_directory_at(
            &self.profiles_directory,
            &profile_name,
            &profile_directory,
        ) {
            Ok(Some(profile_handle)) => profile_handle,
            Ok(None) => return Ok((None, None, 0)),
            Err(WorkspaceStoreError::UnsafePath) => {
                let state = if self.mode == WorkspaceStoreMode::ReadWrite {
                    self.isolate_unsafe_profile_entry(instance_id)?
                } else {
                    ProfileCorruptState::UnsafePath
                };
                return Err(state.error());
            }
            Err(error) => return Err(error),
        };
        if let Err(error) =
            validate_directory_entry(&self.profiles_directory, &profile_name, &profile_handle)
        {
            if matches!(error, WorkspaceStoreError::UnsafePath) {
                let state = if self.mode == WorkspaceStoreMode::ReadWrite {
                    self.isolate_unsafe_profile_entry(instance_id)?
                } else {
                    ProfileCorruptState::UnsafePath
                };
                return Err(state.error());
            }
            return Err(error);
        }
        match read_profile_corrupt_state(&profile_handle) {
            Ok(Some(state)) => {
                let candidates = profile_corruption_candidates(&profile_handle, instance_id);
                return Err(self.profile_corruption_error(
                    &profile_handle,
                    instance_id,
                    state,
                    &candidates,
                ));
            }
            Ok(None) => {}
            Err(error) => {
                let Some(state) = known_profile_corruption(&error) else {
                    return Err(error);
                };
                return Err(self.profile_corruption_error(
                    &profile_handle,
                    instance_id,
                    state,
                    &[CORRUPT_STATE_FILE.to_owned()],
                ));
            }
        }
        let manifest_bytes = match read_optional_private_file_at(
            &profile_handle,
            "manifest.json",
            MAX_MANIFEST_BYTES,
        ) {
            Ok(Some(bytes)) => bytes,
            Ok(None) => return Ok((None, None, 0)),
            Err(error @ WorkspaceStoreError::UnsafePath) => {
                let candidates = profile_corruption_candidates(&profile_handle, instance_id);
                return Err(self.profile_corruption_error(
                    &profile_handle,
                    instance_id,
                    known_profile_corruption(&error).unwrap_or(ProfileCorruptState::UnsafePath),
                    &candidates,
                ));
            }
            Err(WorkspaceStoreError::ShardTooLarge) => {
                let candidates = profile_corruption_candidates(&profile_handle, instance_id);
                return Err(self.profile_corruption_error(
                    &profile_handle,
                    instance_id,
                    ProfileCorruptState::Manifest,
                    &candidates,
                ));
            }
            Err(error) => return Err(error),
        };
        let manifest = match parse_manifest(&manifest_bytes) {
            Ok(manifest) => manifest,
            Err(error) => {
                let state =
                    known_profile_corruption(&error).unwrap_or(ProfileCorruptState::Manifest);
                let candidates = profile_corruption_candidates(&profile_handle, instance_id);
                return Err(self.profile_corruption_error(
                    &profile_handle,
                    instance_id,
                    state,
                    &candidates,
                ));
            }
        };
        if validate_manifest_reference(&manifest, instance_id).is_err() {
            let candidates = profile_corruption_candidates(&profile_handle, instance_id);
            return Err(self.profile_corruption_error(
                &profile_handle,
                instance_id,
                ProfileCorruptState::Manifest,
                &candidates,
            ));
        }
        let shard_bytes =
            match read_private_file_at(&profile_handle, &manifest.shard, MAX_PROFILE_SHARD_BYTES) {
                Ok(bytes) => bytes,
                Err(
                    WorkspaceStoreError::UnsafePath
                    | WorkspaceStoreError::ShardTooLarge
                    | WorkspaceStoreError::Io(WorkspaceIoKind::NotFound),
                ) => {
                    let candidates = profile_corruption_candidates(&profile_handle, instance_id);
                    return Err(self.profile_corruption_error(
                        &profile_handle,
                        instance_id,
                        ProfileCorruptState::Shard,
                        &candidates,
                    ));
                }
                Err(error) => return Err(error),
            };
        if shard_bytes.len() as u64 != manifest.shard_length
            || checksum(&shard_bytes) != manifest.checksum
        {
            let candidates = profile_corruption_candidates(&profile_handle, instance_id);
            return Err(self.profile_corruption_error(
                &profile_handle,
                instance_id,
                ProfileCorruptState::Shard,
                &candidates,
            ));
        }
        let shard = match serde_json::from_slice::<WorkspaceShard>(&shard_bytes) {
            Ok(value) => value,
            Err(_) => {
                let candidates = profile_corruption_candidates(&profile_handle, instance_id);
                return Err(self.profile_corruption_error(
                    &profile_handle,
                    instance_id,
                    ProfileCorruptState::Shard,
                    &candidates,
                ));
            }
        };
        if shard.schema != SHARD_SCHEMA {
            let candidates = profile_corruption_candidates(&profile_handle, instance_id);
            return Err(self.profile_corruption_error(
                &profile_handle,
                instance_id,
                ProfileCorruptState::UnsupportedVersion,
                &candidates,
            ));
        }
        let payload =
            serde_json::to_vec(&shard.payload).map_err(|_| WorkspaceStoreError::CorruptShard)?;
        if shard.instance_id != instance_id
            || shard.instance_id != shard.payload.instance_id()
            || shard.generation != manifest.generation
            || shard.payload_length != payload.len() as u64
            || shard.payload_checksum != checksum(&payload)
            || shard.payload.validate().is_err()
        {
            let candidates = profile_corruption_candidates(&profile_handle, instance_id);
            return Err(self.profile_corruption_error(
                &profile_handle,
                instance_id,
                ProfileCorruptState::Shard,
                &candidates,
            ));
        }
        self.validate_store_chain()?;
        validate_directory_entry(&self.profiles_directory, &profile_name, &profile_handle)?;
        let committed_bytes = (manifest_bytes.len() as u64)
            .checked_add(shard_bytes.len() as u64)
            .ok_or(WorkspaceStoreError::StoreTooLarge)?;
        Ok((
            Some(shard.payload),
            Some(manifest.generation),
            committed_bytes,
        ))
    }

    pub fn clear(&self, instance_id: ProfileInstanceId) -> Result<(), WorkspaceStoreError> {
        if self.mode != WorkspaceStoreMode::ReadWrite {
            return Err(WorkspaceStoreError::ReadOnly);
        }
        let _writer = self
            .writer
            .lock()
            .map_err(|_| WorkspaceStoreError::WriterUnavailable)?;
        if self.recovery_required.load(Ordering::Acquire) {
            return Err(WorkspaceStoreError::RecoveryRequired);
        }
        self.validate_store_chain()?;

        let profile_name = instance_id.to_string();
        if let Some(marker) = self.root_profile_corrupt_marker_for_use(instance_id)? {
            if self.clear_root_marked_profile(instance_id, marker).is_err()
                || self.validate_store_chain().is_err()
            {
                self.recovery_required.store(true, Ordering::Release);
                return Err(WorkspaceStoreError::DurabilityUnknown);
            }
            return Ok(());
        }
        let profile_path = self.profile_directory(instance_id);
        let profile_handle = match open_optional_private_directory_at(
            &self.profiles_directory,
            &profile_name,
            &profile_path,
        ) {
            Ok(profile_handle) => profile_handle,
            Err(WorkspaceStoreError::UnsafePath) => {
                let marker = self
                    .profile_entry_corrupt_marker(instance_id, ProfileCorruptState::UnsafePath)?;
                let marker = persist_root_profile_corrupt_marker(
                    &self.profiles_directory,
                    instance_id,
                    marker,
                )?;
                if self.clear_root_marked_profile(instance_id, marker).is_err()
                    || self.validate_store_chain().is_err()
                {
                    self.recovery_required.store(true, Ordering::Release);
                    return Err(WorkspaceStoreError::DurabilityUnknown);
                }
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        if let Some(profile_handle) = profile_handle {
            let nonce = ProfileInstanceId::generate()
                .map_err(|_| WorkspaceStoreError::WriterUnavailable)?;
            let identity = managed_entry_identity(
                &rustix::fs::fstat(&profile_handle).map_err(workspace_error_from_errno)?,
            )?;
            let tombstone_name = cleared_tombstone_name(instance_id, nonce, identity);
            validate_directory_entry(&self.profiles_directory, &profile_name, &profile_handle)?;
            rename_no_replace_at(&self.profiles_directory, &profile_name, &tombstone_name)
                .map_err(workspace_error_from_errno)?;
            if validate_bound_directory_entry(
                &self.profiles_directory,
                &tombstone_name,
                &profile_handle,
                identity,
            )
            .is_err()
            {
                self.recovery_required.store(true, Ordering::Release);
                return Err(WorkspaceStoreError::DurabilityUnknown);
            }
            if sync_directory_handle(&self.profiles_directory).is_err() {
                self.recovery_required.store(true, Ordering::Release);
                return Err(WorkspaceStoreError::DurabilityUnknown);
            }
            if self
                .finish_cleared_profile(
                    &tombstone_name,
                    instance_id,
                    identity,
                    Some(&profile_handle),
                )
                .is_err()
            {
                self.recovery_required.store(true, Ordering::Release);
                return Err(WorkspaceStoreError::DurabilityUnknown);
            }
        } else if self.clear_quarantine_scope(instance_id).is_err()
            || sync_directory_handle(&self.quarantine_directory).is_err()
        {
            self.recovery_required.store(true, Ordering::Release);
            return Err(WorkspaceStoreError::DurabilityUnknown);
        }
        if self.validate_store_chain().is_err() {
            self.recovery_required.store(true, Ordering::Release);
            return Err(WorkspaceStoreError::DurabilityUnknown);
        }
        Ok(())
    }

    fn finish_cleared_profile(
        &self,
        tombstone_name: &str,
        instance_id: ProfileInstanceId,
        identity: ManagedEntryIdentity,
        profile_handle: Option<&fs::File>,
    ) -> Result<(), WorkspaceStoreError> {
        validate_managed_entry_identity_at(&self.profiles_directory, tombstone_name, identity)?;
        self.clear_quarantine_scope(instance_id)?;
        sync_directory_handle(&self.quarantine_directory)?;
        let marker_name = profile_corrupt_marker_name(instance_id);
        remove_managed_entry_at(&self.profiles_directory, &marker_name)?;
        sync_directory_handle(&self.profiles_directory)?;
        match (identity.kind, profile_handle) {
            (ManagedEntryKind::Directory, Some(profile_handle)) => {
                remove_bound_profile_directory(
                    &self.profiles_directory,
                    tombstone_name,
                    profile_handle,
                    identity,
                )?;
            }
            (ManagedEntryKind::Directory, None) => {
                return Err(WorkspaceStoreError::ExternalChange);
            }
            (_, None) => {
                remove_bound_non_directory_entry(
                    &self.profiles_directory,
                    tombstone_name,
                    identity,
                )?;
            }
            (_, Some(_)) => {
                return Err(WorkspaceStoreError::ExternalChange);
            }
        }
        sync_directory_handle(&self.profiles_directory)
    }

    fn profile_directory(&self, instance_id: ProfileInstanceId) -> PathBuf {
        self.profiles.join(instance_id.to_string())
    }

    fn profile_entry_corrupt_marker(
        &self,
        instance_id: ProfileInstanceId,
        state: ProfileCorruptState,
    ) -> Result<RootProfileCorruptMarker, WorkspaceStoreError> {
        let profile_name = instance_id.to_string();
        Ok(
            match optional_managed_entry_identity_at(&self.profiles_directory, &profile_name)? {
                Some(entry) => RootProfileCorruptMarker::ProfileEntry { state, entry },
                None => RootProfileCorruptMarker::Unbound(state),
            },
        )
    }

    fn internal_state_corrupt_marker(
        &self,
        profile_handle: &fs::File,
        state: ProfileCorruptState,
    ) -> Result<RootProfileCorruptMarker, WorkspaceStoreError> {
        let profile = managed_entry_identity(
            &rustix::fs::fstat(profile_handle).map_err(workspace_error_from_errno)?,
        )?;
        Ok(
            match optional_managed_entry_identity_at(profile_handle, CORRUPT_STATE_FILE)? {
                Some(entry) => RootProfileCorruptMarker::InternalState {
                    state,
                    profile,
                    entry,
                },
                None => RootProfileCorruptMarker::Unbound(state),
            },
        )
    }

    fn inferred_root_profile_corrupt_marker(
        &self,
        _instance_id: ProfileInstanceId,
    ) -> Result<RootProfileCorruptMarker, WorkspaceStoreError> {
        Ok(RootProfileCorruptMarker::Unbound(
            ProfileCorruptState::UnsafePath,
        ))
    }

    fn root_profile_corrupt_marker_for_use(
        &self,
        instance_id: ProfileInstanceId,
    ) -> Result<Option<RootProfileCorruptMarker>, WorkspaceStoreError> {
        match read_root_profile_corrupt_marker(&self.profiles_directory, instance_id) {
            Ok(marker) => Ok(marker),
            Err(WorkspaceStoreError::CorruptManifest | WorkspaceStoreError::UnsafePath)
                if self.mode == WorkspaceStoreMode::ReadWrite =>
            {
                let marker = self.inferred_root_profile_corrupt_marker(instance_id)?;
                persist_root_profile_corrupt_marker(&self.profiles_directory, instance_id, marker)
                    .map(Some)
            }
            Err(error) => Err(error),
        }
    }

    fn root_profile_corrupt_markers(
        &self,
    ) -> Result<HashMap<ProfileInstanceId, RootProfileCorruptMarker>, WorkspaceStoreError> {
        let mut markers = HashMap::new();
        for name in directory_entry_names(&self.profiles_directory)? {
            if !name.starts_with(PROFILE_CORRUPT_MARKER_PREFIX) {
                continue;
            }
            let instance_id = parse_profile_corrupt_marker_instance(&name)?;
            let marker = self
                .root_profile_corrupt_marker_for_use(instance_id)?
                .ok_or(WorkspaceStoreError::ExternalChange)?;
            if markers.insert(instance_id, marker).is_some() {
                return Err(WorkspaceStoreError::UnsafePath);
            }
        }
        Ok(markers)
    }

    fn recover_root_profile_corruption(
        &self,
        instance_id: ProfileInstanceId,
        marker: RootProfileCorruptMarker,
    ) -> Result<(), WorkspaceStoreError> {
        if self.mode != WorkspaceStoreMode::ReadWrite {
            return Ok(());
        }
        if self.recovery_required.load(Ordering::Acquire) {
            return Err(WorkspaceStoreError::RecoveryRequired);
        }
        let profile_name = instance_id.to_string();
        match marker {
            RootProfileCorruptMarker::ProfileEntry { entry, .. } => {
                if optional_managed_entry_identity_at(&self.profiles_directory, &profile_name)?
                    == Some(entry)
                {
                    self.quarantine_bound_managed_entry(
                        &self.profiles_directory,
                        &profile_name,
                        instance_id,
                        entry,
                    )?;
                }
            }
            RootProfileCorruptMarker::InternalState { profile, entry, .. } => {
                if profile.kind == ManagedEntryKind::Directory
                    && optional_managed_entry_identity_at(&self.profiles_directory, &profile_name)?
                        == Some(profile)
                {
                    let profile_handle =
                        open_directory_entry_unchecked(&self.profiles_directory, &profile_name)?;
                    validate_bound_directory_entry(
                        &self.profiles_directory,
                        &profile_name,
                        &profile_handle,
                        profile,
                    )?;
                    if optional_managed_entry_identity_at(&profile_handle, CORRUPT_STATE_FILE)?
                        == Some(entry)
                    {
                        self.quarantine_bound_managed_entry(
                            &profile_handle,
                            CORRUPT_STATE_FILE,
                            instance_id,
                            entry,
                        )?;
                    }
                }
            }
            RootProfileCorruptMarker::Unbound(_) => {}
        }
        trim_quarantine(&self.quarantine_directory)
    }

    fn isolate_unsafe_profile_entry(
        &self,
        instance_id: ProfileInstanceId,
    ) -> Result<ProfileCorruptState, WorkspaceStoreError> {
        let marker =
            self.profile_entry_corrupt_marker(instance_id, ProfileCorruptState::UnsafePath)?;
        let marker =
            persist_root_profile_corrupt_marker(&self.profiles_directory, instance_id, marker)?;
        self.recover_root_profile_corruption(instance_id, marker)?;
        Ok(marker.state())
    }

    fn recover_root_profile_corrupt_markers(&self) -> Result<(), WorkspaceStoreError> {
        for (instance_id, marker) in self.root_profile_corrupt_markers()? {
            self.recover_root_profile_corruption(instance_id, marker)?;
        }
        Ok(())
    }

    fn clear_root_marked_profile(
        &self,
        instance_id: ProfileInstanceId,
        marker: RootProfileCorruptMarker,
    ) -> Result<(), WorkspaceStoreError> {
        let profile_name = instance_id.to_string();
        let identity = match marker {
            RootProfileCorruptMarker::ProfileEntry { entry, .. } => entry,
            RootProfileCorruptMarker::InternalState { profile, .. } => profile,
            RootProfileCorruptMarker::Unbound(_) => {
                return self.finish_profile_clear_without_entry(instance_id);
            }
        };
        if optional_managed_entry_identity_at(&self.profiles_directory, &profile_name)?
            != Some(identity)
        {
            return self.finish_profile_clear_without_entry(instance_id);
        }
        let profile_handle = if identity.kind == ManagedEntryKind::Directory {
            let profile_handle =
                open_directory_entry_unchecked(&self.profiles_directory, &profile_name)?;
            validate_bound_directory_entry(
                &self.profiles_directory,
                &profile_name,
                &profile_handle,
                identity,
            )?;
            Some(profile_handle)
        } else {
            None
        };
        let nonce =
            ProfileInstanceId::generate().map_err(|_| WorkspaceStoreError::WriterUnavailable)?;
        let tombstone_name = cleared_tombstone_name(instance_id, nonce, identity);
        validate_managed_entry_identity_at(&self.profiles_directory, &profile_name, identity)?;
        rename_no_replace_at(&self.profiles_directory, &profile_name, &tombstone_name)
            .map_err(workspace_error_from_errno)?;
        sync_directory_handle(&self.profiles_directory)?;
        validate_managed_entry_identity_at(&self.profiles_directory, &tombstone_name, identity)?;
        self.finish_cleared_profile(
            &tombstone_name,
            instance_id,
            identity,
            profile_handle.as_ref(),
        )
    }

    fn finish_profile_clear_without_entry(
        &self,
        instance_id: ProfileInstanceId,
    ) -> Result<(), WorkspaceStoreError> {
        self.clear_quarantine_scope(instance_id)?;
        sync_directory_handle(&self.quarantine_directory)?;
        let marker_name = profile_corrupt_marker_name(instance_id);
        remove_managed_entry_at(&self.profiles_directory, &marker_name)?;
        sync_directory_handle(&self.profiles_directory)
    }

    fn committed_usage_except(
        &self,
        excluded: ProfileInstanceId,
    ) -> Result<(usize, usize, u64, Vec<WorkspaceStoreWarning>), WorkspaceStoreError> {
        let mut editor_tabs = 0_usize;
        let mut history_entries = 0_usize;
        let mut committed_bytes = 0_u64;
        let mut warnings = Vec::new();
        let markers = self.root_profile_corrupt_markers()?;
        for (instance_id, marker) in &markers {
            self.recover_root_profile_corruption(*instance_id, *marker)?;
            if *instance_id != excluded {
                warnings.push(WorkspaceStoreWarning::CorruptProfileQuarantined);
            }
        }
        for name in directory_entry_names(&self.profiles_directory)? {
            if name.starts_with(CLEARED_TOMBSTONE_PREFIX)
                || name.starts_with(PROFILE_CORRUPT_MARKER_PREFIX)
            {
                continue;
            }
            let instance_id =
                ProfileInstanceId::parse(&name).map_err(|_| WorkspaceStoreError::UnsafePath)?;
            if instance_id == excluded || markers.contains_key(&instance_id) {
                continue;
            }
            let directory =
                match open_optional_private_directory_entry(&self.profiles_directory, &name) {
                    Ok(Some(directory)) => directory,
                    Ok(None) => continue,
                    Err(WorkspaceStoreError::UnsafePath) => {
                        self.isolate_unsafe_profile_entry(instance_id)?;
                        warnings.push(WorkspaceStoreWarning::CorruptProfileQuarantined);
                        continue;
                    }
                    Err(error) => return Err(error),
                };
            if let Err(error) =
                validate_directory_entry(&self.profiles_directory, &name, &directory)
            {
                if matches!(error, WorkspaceStoreError::UnsafePath) {
                    self.isolate_unsafe_profile_entry(instance_id)?;
                    warnings.push(WorkspaceStoreWarning::CorruptProfileQuarantined);
                    continue;
                }
                return Err(error);
            };
            let committed = match read_committed_snapshot(&directory, instance_id) {
                Ok(Some(committed)) => committed,
                Ok(None) => continue,
                Err(error) => {
                    let Some(state) = known_profile_corruption(&error) else {
                        return Err(error);
                    };
                    let candidates = profile_corruption_candidates(&directory, instance_id);
                    self.record_profile_corruption(&directory, instance_id, state, &candidates)?;
                    warnings.push(WorkspaceStoreWarning::CorruptProfileQuarantined);
                    continue;
                }
            };
            editor_tabs = editor_tabs
                .checked_add(committed.snapshot.editor_tabs.len())
                .ok_or(WorkspaceStoreError::StoreTooLarge)?;
            history_entries = history_entries
                .checked_add(committed.snapshot.history.len())
                .ok_or(WorkspaceStoreError::StoreTooLarge)?;
            committed_bytes = committed_bytes
                .checked_add(committed.committed_bytes)
                .ok_or(WorkspaceStoreError::StoreTooLarge)?;
        }
        validate_retained_store_entries(&self.root_directory)?;
        Ok((editor_tabs, history_entries, committed_bytes, warnings))
    }

    fn profile_corruption_error(
        &self,
        profile_handle: &fs::File,
        instance_id: ProfileInstanceId,
        state: ProfileCorruptState,
        candidates: &[String],
    ) -> WorkspaceStoreError {
        match self.record_profile_corruption(profile_handle, instance_id, state, candidates) {
            Ok(stored_state) => stored_state.error(),
            Err(error) => error,
        }
    }

    fn record_profile_corruption(
        &self,
        profile_handle: &fs::File,
        instance_id: ProfileInstanceId,
        state: ProfileCorruptState,
        candidates: &[String],
    ) -> Result<ProfileCorruptState, WorkspaceStoreError> {
        if self.mode != WorkspaceStoreMode::ReadWrite {
            return Ok(state);
        }
        if self.recovery_required.load(Ordering::Acquire) {
            return Err(WorkspaceStoreError::RecoveryRequired);
        }
        let stored_state = match read_profile_corrupt_state(profile_handle) {
            Ok(Some(stored_state)) => stored_state,
            Ok(None) => persist_profile_corrupt_state(profile_handle, state)?,
            Err(WorkspaceStoreError::CorruptManifest | WorkspaceStoreError::UnsafePath) => {
                let marker = self.internal_state_corrupt_marker(profile_handle, state)?;
                let marker = persist_root_profile_corrupt_marker(
                    &self.profiles_directory,
                    instance_id,
                    marker,
                )?;
                self.recover_root_profile_corruption(instance_id, marker)?;
                trim_quarantine(&self.quarantine_directory)?;
                return Ok(marker.state());
            }
            Err(error) => return Err(error),
        };
        for candidate in candidates {
            self.quarantine_corrupt(profile_handle, candidate, instance_id)?;
        }
        trim_quarantine(&self.quarantine_directory)?;
        Ok(stored_state)
    }

    fn quarantine_corrupt(
        &self,
        profile_handle: &fs::File,
        shard_name: &str,
        instance_id: ProfileInstanceId,
    ) -> Result<bool, WorkspaceStoreError> {
        self.quarantine_managed_entry(profile_handle, shard_name, instance_id)
    }

    fn quarantine_managed_entry(
        &self,
        source_directory: &fs::File,
        entry_name: &str,
        instance_id: ProfileInstanceId,
    ) -> Result<bool, WorkspaceStoreError> {
        if self.mode != WorkspaceStoreMode::ReadWrite {
            return Ok(false);
        }
        let nonce =
            ProfileInstanceId::generate().map_err(|_| WorkspaceStoreError::WriterUnavailable)?;
        let target_name = format!("q-{}-{nonce}.bin", quarantine_scope(instance_id));
        let entry_stat = match rustix::fs::statat(
            source_directory,
            entry_name,
            rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
        ) {
            Ok(stat) => stat,
            Err(source) if source == rustix::io::Errno::NOENT => return Ok(false),
            Err(source) => return Err(workspace_error_from_errno(source)),
        };
        let changed = if stat_is_private_regular(&entry_stat) {
            rename_between_no_replace(
                source_directory,
                entry_name,
                &self.quarantine_directory,
                &target_name,
            )
            .map_err(workspace_error_from_errno)?;
            true
        } else {
            write_quarantine_marker(&self.quarantine_directory, &target_name)?;
            sync_directory_handle(&self.quarantine_directory)?;
            remove_managed_entry_at(source_directory, entry_name)?;
            true
        };
        if changed {
            sync_directory_handle(&self.quarantine_directory)?;
            sync_directory_handle(source_directory)?;
        }
        Ok(changed)
    }

    fn quarantine_bound_managed_entry(
        &self,
        source_directory: &fs::File,
        entry_name: &str,
        instance_id: ProfileInstanceId,
        expected: ManagedEntryIdentity,
    ) -> Result<bool, WorkspaceStoreError> {
        if self.mode != WorkspaceStoreMode::ReadWrite {
            return Ok(false);
        }
        if optional_managed_entry_identity_at(source_directory, entry_name)? != Some(expected) {
            return Ok(false);
        }
        let entry_stat = rustix::fs::statat(
            source_directory,
            entry_name,
            rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
        )
        .map_err(workspace_error_from_errno)?;
        let nonce =
            ProfileInstanceId::generate().map_err(|_| WorkspaceStoreError::WriterUnavailable)?;
        if stat_is_private_regular(&entry_stat) {
            let target_name = bound_quarantine_entry_name(instance_id, nonce, expected);
            validate_managed_entry_identity_at(source_directory, entry_name, expected)?;
            rename_between_no_replace(
                source_directory,
                entry_name,
                &self.quarantine_directory,
                &target_name,
            )
            .map_err(workspace_error_from_errno)?;
            sync_directory_handle(&self.quarantine_directory)?;
            sync_directory_handle(source_directory)?;
            validate_managed_entry_identity_at(&self.quarantine_directory, &target_name, expected)?;
        } else {
            let target_name = format!("q-{}-{nonce}.bin", quarantine_scope(instance_id));
            let directory_handle = if expected.kind == ManagedEntryKind::Directory {
                let directory_handle =
                    open_directory_entry_unchecked(source_directory, entry_name)?;
                validate_bound_directory_entry(
                    source_directory,
                    entry_name,
                    &directory_handle,
                    expected,
                )?;
                Some(directory_handle)
            } else {
                None
            };
            write_quarantine_marker(&self.quarantine_directory, &target_name)?;
            sync_directory_handle(&self.quarantine_directory)?;
            match directory_handle.as_ref() {
                Some(directory_handle) => remove_bound_profile_directory(
                    source_directory,
                    entry_name,
                    directory_handle,
                    expected,
                )?,
                None => {
                    remove_bound_non_directory_entry(source_directory, entry_name, expected)?;
                }
            }
            sync_directory_handle(source_directory)?;
        }
        Ok(true)
    }

    fn clear_quarantine_scope(
        &self,
        instance_id: ProfileInstanceId,
    ) -> Result<(), WorkspaceStoreError> {
        let prefix = format!("q-{}-", quarantine_scope(instance_id));
        for name in directory_entry_names(&self.quarantine_directory)? {
            if !name.starts_with(&prefix) {
                continue;
            }
            let binding = parse_quarantine_entry_binding(&name)?;
            let stat = rustix::fs::statat(
                &self.quarantine_directory,
                &name,
                rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
            )
            .map_err(workspace_error_from_errno)?;
            if rustix::fs::FileType::from_raw_mode(stat.st_mode).is_dir() {
                return Err(WorkspaceStoreError::UnsafePath);
            }
            if let Some(expected) = binding
                && managed_entry_identity(&stat)? != expected
            {
                return Err(WorkspaceStoreError::UnsafePath);
            }
            rustix::fs::unlinkat(
                &self.quarantine_directory,
                &name,
                rustix::fs::AtFlags::empty(),
            )
            .map_err(workspace_error_from_errno)?;
        }
        Ok(())
    }

    fn cleanup_cleared_profiles(&self) -> Result<(), WorkspaceStoreError> {
        for name in directory_entry_names(&self.profiles_directory)? {
            if !name.starts_with(CLEARED_TOMBSTONE_PREFIX) {
                continue;
            }
            let tombstone = parse_cleared_tombstone(&name)?;
            let stat = match rustix::fs::statat(
                &self.profiles_directory,
                &name,
                rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
            ) {
                Ok(stat) => stat,
                Err(source) if source == rustix::io::Errno::NOENT => {
                    return Err(WorkspaceStoreError::ExternalChange);
                }
                Err(source) => return Err(workspace_error_from_errno(source)),
            };
            if managed_entry_identity(&stat)? != tombstone.identity {
                return Err(WorkspaceStoreError::UnsafePath);
            }
            let profile_handle = if tombstone.identity.kind == ManagedEntryKind::Directory {
                let profile_handle =
                    open_directory_entry_unchecked(&self.profiles_directory, &name)?;
                validate_bound_directory_entry(
                    &self.profiles_directory,
                    &name,
                    &profile_handle,
                    tombstone.identity,
                )?;
                Some(profile_handle)
            } else {
                None
            };
            self.finish_cleared_profile(
                &name,
                tombstone.instance_id,
                tombstone.identity,
                profile_handle.as_ref(),
            )?;
        }
        Ok(())
    }

    fn cleanup_root_profile_marker_temps(&self) -> Result<(), WorkspaceStoreError> {
        let mut changed = false;
        for name in directory_entry_names(&self.profiles_directory)? {
            if !name.starts_with(PROFILE_CORRUPT_MARKER_TEMP_PREFIX) {
                continue;
            }
            let stat = rustix::fs::statat(
                &self.profiles_directory,
                &name,
                rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
            )
            .map_err(workspace_error_from_errno)?;
            if !stat_is_private_regular(&stat) {
                return Err(WorkspaceStoreError::UnsafePath);
            }
            rustix::fs::unlinkat(
                &self.profiles_directory,
                &name,
                rustix::fs::AtFlags::empty(),
            )
            .map_err(workspace_error_from_errno)?;
            changed = true;
        }
        if changed {
            sync_directory_handle(&self.profiles_directory)?;
        }
        Ok(())
    }
}

pub fn workspace_root_for_config(config_path: &Path) -> Result<PathBuf, WorkspaceStoreError> {
    let file_name = config_path
        .file_name()
        .filter(|name| !name.is_empty())
        .ok_or(WorkspaceStoreError::InvalidConfigPath)?;
    let mut sibling = OsString::from(".");
    sibling.push(file_name);
    sibling.push(".workspace");
    Ok(config_path.with_file_name(sibling))
}

fn cleared_tombstone_name(
    instance_id: ProfileInstanceId,
    nonce: ProfileInstanceId,
    identity: ManagedEntryIdentity,
) -> String {
    format!(
        "{CLEARED_TOMBSTONE_PREFIX}{instance_id}.{nonce}.{}.{:016x}.{:016x}",
        identity.kind.marker(),
        identity.device,
        identity.inode
    )
}

fn parse_cleared_tombstone(name: &str) -> Result<ClearedTombstone, WorkspaceStoreError> {
    let value = name
        .strip_prefix(CLEARED_TOMBSTONE_PREFIX)
        .ok_or(WorkspaceStoreError::UnsafePath)?;
    let mut parts = value.split('.');
    let instance = parts.next().ok_or(WorkspaceStoreError::UnsafePath)?;
    let nonce = parts.next().ok_or(WorkspaceStoreError::UnsafePath)?;
    let kind = parts.next().ok_or(WorkspaceStoreError::UnsafePath)?;
    let device = parts.next().ok_or(WorkspaceStoreError::UnsafePath)?;
    let inode = parts.next().ok_or(WorkspaceStoreError::UnsafePath)?;
    if parts.next().is_some() || device.len() != 16 || inode.len() != 16 {
        return Err(WorkspaceStoreError::UnsafePath);
    }
    let instance_id =
        ProfileInstanceId::parse(instance).map_err(|_| WorkspaceStoreError::UnsafePath)?;
    ProfileInstanceId::parse(nonce).map_err(|_| WorkspaceStoreError::UnsafePath)?;
    let device = u64::from_str_radix(device, 16).map_err(|_| WorkspaceStoreError::UnsafePath)?;
    let inode = u64::from_str_radix(inode, 16).map_err(|_| WorkspaceStoreError::UnsafePath)?;
    Ok(ClearedTombstone {
        instance_id,
        identity: ManagedEntryIdentity {
            device,
            inode,
            kind: ManagedEntryKind::parse(kind)?,
        },
    })
}

fn parse_managed_entry_identity_parts<'a>(
    parts: &mut impl Iterator<Item = &'a str>,
) -> Result<ManagedEntryIdentity, WorkspaceStoreError> {
    let kind = parts.next().ok_or(WorkspaceStoreError::CorruptManifest)?;
    let device = parts.next().ok_or(WorkspaceStoreError::CorruptManifest)?;
    let inode = parts.next().ok_or(WorkspaceStoreError::CorruptManifest)?;
    if device.len() != 16 || inode.len() != 16 {
        return Err(WorkspaceStoreError::CorruptManifest);
    }
    Ok(ManagedEntryIdentity {
        device: u64::from_str_radix(device, 16)
            .map_err(|_| WorkspaceStoreError::CorruptManifest)?,
        inode: u64::from_str_radix(inode, 16).map_err(|_| WorkspaceStoreError::CorruptManifest)?,
        kind: ManagedEntryKind::parse(kind).map_err(|_| WorkspaceStoreError::CorruptManifest)?,
    })
}

fn profile_corrupt_marker_name(instance_id: ProfileInstanceId) -> String {
    format!("{PROFILE_CORRUPT_MARKER_PREFIX}{instance_id}{PROFILE_CORRUPT_MARKER_SUFFIX}")
}

fn parse_profile_corrupt_marker_instance(
    name: &str,
) -> Result<ProfileInstanceId, WorkspaceStoreError> {
    let instance = name
        .strip_prefix(PROFILE_CORRUPT_MARKER_PREFIX)
        .and_then(|value| value.strip_suffix(PROFILE_CORRUPT_MARKER_SUFFIX))
        .ok_or(WorkspaceStoreError::UnsafePath)?;
    ProfileInstanceId::parse(instance).map_err(|_| WorkspaceStoreError::UnsafePath)
}

fn known_profile_corruption(error: &WorkspaceStoreError) -> Option<ProfileCorruptState> {
    match error {
        WorkspaceStoreError::CorruptManifest => Some(ProfileCorruptState::Manifest),
        WorkspaceStoreError::CorruptShard
        | WorkspaceStoreError::ShardTooLarge
        | WorkspaceStoreError::Io(WorkspaceIoKind::NotFound) => Some(ProfileCorruptState::Shard),
        WorkspaceStoreError::UnsupportedVersion => Some(ProfileCorruptState::UnsupportedVersion),
        WorkspaceStoreError::UnsafePath => Some(ProfileCorruptState::UnsafePath),
        _ => None,
    }
}

fn profile_corruption_candidates(
    directory: &fs::File,
    instance_id: ProfileInstanceId,
) -> Vec<String> {
    let mut candidates = Vec::with_capacity(2);
    if let Ok(Some(bytes)) =
        read_optional_private_file_at(directory, "manifest.json", MAX_MANIFEST_BYTES)
        && let Ok(manifest) = serde_json::from_slice::<WorkspaceManifest>(&bytes)
        && manifest.instance_id == instance_id
        && manifest.generation > 0
        && manifest.shard == shard_name(manifest.generation)
    {
        candidates.push(manifest.shard);
    }
    candidates.push("manifest.json".to_owned());
    candidates
}

fn read_profile_corrupt_state(
    directory: &fs::File,
) -> Result<Option<ProfileCorruptState>, WorkspaceStoreError> {
    match read_optional_private_file_at(directory, CORRUPT_STATE_FILE, MAX_CORRUPT_STATE_BYTES) {
        Ok(Some(bytes)) => ProfileCorruptState::parse(&bytes).map(Some),
        Ok(None) => Ok(None),
        Err(WorkspaceStoreError::ShardTooLarge) => Err(WorkspaceStoreError::CorruptManifest),
        Err(error) => Err(error),
    }
}

fn read_root_profile_corrupt_marker(
    profiles_directory: &fs::File,
    instance_id: ProfileInstanceId,
) -> Result<Option<RootProfileCorruptMarker>, WorkspaceStoreError> {
    let name = profile_corrupt_marker_name(instance_id);
    match read_optional_private_file_at(
        profiles_directory,
        &name,
        MAX_ROOT_PROFILE_CORRUPT_MARKER_BYTES,
    ) {
        Ok(Some(bytes)) => RootProfileCorruptMarker::parse(&bytes).map(Some),
        Ok(None) => Ok(None),
        Err(WorkspaceStoreError::ShardTooLarge) => Err(WorkspaceStoreError::CorruptManifest),
        Err(error) => Err(error),
    }
}

fn persist_root_profile_corrupt_marker(
    profiles_directory: &fs::File,
    instance_id: ProfileInstanceId,
    marker: RootProfileCorruptMarker,
) -> Result<RootProfileCorruptMarker, WorkspaceStoreError> {
    let name = profile_corrupt_marker_name(instance_id);
    match read_root_profile_corrupt_marker(profiles_directory, instance_id) {
        Ok(Some(stored)) => {
            let file = open_optional_private_file_at(profiles_directory, &name)?
                .ok_or(WorkspaceStoreError::ExternalChange)?;
            file.sync_all()?;
            sync_directory_handle(profiles_directory)?;
            Ok(stored)
        }
        Ok(None) => {
            publish_root_profile_corrupt_marker(profiles_directory, instance_id, marker, false)
        }
        Err(WorkspaceStoreError::CorruptManifest | WorkspaceStoreError::UnsafePath) => {
            publish_root_profile_corrupt_marker(profiles_directory, instance_id, marker, true)
        }
        Err(error) => Err(error),
    }
}

fn publish_root_profile_corrupt_marker(
    profiles_directory: &fs::File,
    instance_id: ProfileInstanceId,
    marker: RootProfileCorruptMarker,
    replace_existing: bool,
) -> Result<RootProfileCorruptMarker, WorkspaceStoreError> {
    let name = profile_corrupt_marker_name(instance_id);
    let marker_bytes = marker.bytes();
    if marker_bytes.len() > MAX_ROOT_PROFILE_CORRUPT_MARKER_BYTES {
        return Err(WorkspaceStoreError::CorruptManifest);
    }
    let (temporary_name, mut file) =
        create_private_temp_at(profiles_directory, "profile-corrupt-marker")?;
    let cleanup = TempCleanup::new(profiles_directory, temporary_name.clone())?;
    file.write_all(&marker_bytes)?;
    file.flush()?;
    file.sync_all()?;
    if replace_existing {
        replace_at(profiles_directory, &temporary_name, &name)?;
    } else {
        match rename_no_replace_at(profiles_directory, &temporary_name, &name) {
            Ok(()) => {}
            Err(source) if source == rustix::io::Errno::EXIST => {
                return read_root_profile_corrupt_marker(profiles_directory, instance_id)?
                    .ok_or(WorkspaceStoreError::ExternalChange);
            }
            Err(source) => return Err(workspace_error_from_errno(source)),
        }
    }
    cleanup.disarm();
    let expected_fingerprint =
        private_file_fingerprint(&rustix::fs::fstat(&file).map_err(workspace_error_from_errno)?)?;
    if optional_private_file_fingerprint_at(profiles_directory, &name)?
        != Some(expected_fingerprint)
    {
        return Err(WorkspaceStoreError::ExternalChange);
    }
    drop(file);
    sync_directory_handle(profiles_directory)?;
    match read_root_profile_corrupt_marker(profiles_directory, instance_id)? {
        Some(stored) if stored == marker => Ok(stored),
        Some(_) => Err(WorkspaceStoreError::ExternalChange),
        None => Err(WorkspaceStoreError::DurabilityUnknown),
    }
}

fn persist_profile_corrupt_state(
    directory: &fs::File,
    state: ProfileCorruptState,
) -> Result<ProfileCorruptState, WorkspaceStoreError> {
    if let Some(stored_state) = read_profile_corrupt_state(directory)? {
        return Ok(stored_state);
    }

    let (temporary_name, mut file) = create_private_temp_at(directory, "corrupt-state")?;
    let cleanup = TempCleanup::new(directory, temporary_name.clone())?;
    file.write_all(state.bytes())?;
    file.flush()?;
    file.sync_all()?;
    drop(file);

    match rename_no_replace_at(directory, &temporary_name, CORRUPT_STATE_FILE) {
        Ok(()) => {
            cleanup.disarm();
            sync_directory_handle(directory)?;
        }
        Err(source) if source == rustix::io::Errno::EXIST => {}
        Err(source) => return Err(workspace_error_from_errno(source)),
    }

    read_profile_corrupt_state(directory)?.ok_or(WorkspaceStoreError::DurabilityUnknown)
}

fn parse_manifest(bytes: &[u8]) -> Result<WorkspaceManifest, WorkspaceStoreError> {
    let manifest = serde_json::from_slice::<WorkspaceManifest>(bytes)
        .map_err(|_| WorkspaceStoreError::CorruptManifest)?;
    if manifest.schema != MANIFEST_SCHEMA {
        return Err(WorkspaceStoreError::UnsupportedVersion);
    }
    if manifest.generation == 0 {
        return Err(WorkspaceStoreError::CorruptManifest);
    }
    Ok(manifest)
}

fn validate_manifest_reference(
    manifest: &WorkspaceManifest,
    instance_id: ProfileInstanceId,
) -> Result<(), WorkspaceStoreError> {
    if manifest.shard != shard_name(manifest.generation)
        || manifest.instance_id != instance_id
        || manifest.shard_length > MAX_PROFILE_SHARD_BYTES as u64
        || !is_checksum(&manifest.checksum)
    {
        return Err(WorkspaceStoreError::CorruptManifest);
    }
    Ok(())
}

struct CommittedWorkspaceSnapshot {
    snapshot: ProfileWorkspaceSnapshot,
    committed_bytes: u64,
}

fn read_committed_snapshot(
    directory: &fs::File,
    instance_id: ProfileInstanceId,
) -> Result<Option<CommittedWorkspaceSnapshot>, WorkspaceStoreError> {
    if let Some(state) = read_profile_corrupt_state(directory)? {
        return Err(state.error());
    }
    let manifest_snapshot = match read_optional_private_file_snapshot_at(
        directory,
        "manifest.json",
        MAX_MANIFEST_BYTES,
    ) {
        Ok(Some(snapshot)) => snapshot,
        Ok(None) => return Ok(None),
        Err(WorkspaceStoreError::ShardTooLarge) => {
            return Err(WorkspaceStoreError::CorruptManifest);
        }
        Err(error) => return Err(error),
    };
    let manifest = parse_manifest(&manifest_snapshot.bytes)?;
    validate_manifest_reference(&manifest, instance_id)?;
    let shard_snapshot = match read_optional_private_file_snapshot_at(
        directory,
        &manifest.shard,
        MAX_PROFILE_SHARD_BYTES,
    ) {
        Ok(Some(snapshot)) => snapshot,
        Ok(None) => return Err(WorkspaceStoreError::CorruptShard),
        Err(
            WorkspaceStoreError::ShardTooLarge
            | WorkspaceStoreError::UnsafePath
            | WorkspaceStoreError::Io(WorkspaceIoKind::NotFound),
        ) => return Err(WorkspaceStoreError::CorruptShard),
        Err(error) => return Err(error),
    };
    let shard_bytes = &shard_snapshot.bytes;
    if shard_bytes.len() as u64 != manifest.shard_length
        || checksum(shard_bytes) != manifest.checksum
    {
        return Err(WorkspaceStoreError::CorruptShard);
    }
    let shard = serde_json::from_slice::<WorkspaceShard>(shard_bytes)
        .map_err(|_| WorkspaceStoreError::CorruptShard)?;
    if shard.schema != SHARD_SCHEMA {
        return Err(WorkspaceStoreError::UnsupportedVersion);
    }
    let payload =
        serde_json::to_vec(&shard.payload).map_err(|_| WorkspaceStoreError::CorruptShard)?;
    if shard.instance_id != instance_id
        || shard.instance_id != shard.payload.instance_id()
        || shard.generation != manifest.generation
        || shard.payload_length != payload.len() as u64
        || shard.payload_checksum != checksum(&payload)
        || shard.payload.validate().is_err()
    {
        return Err(WorkspaceStoreError::CorruptShard);
    }
    let committed_bytes = (manifest_snapshot.bytes.len() as u64)
        .checked_add(shard_snapshot.bytes.len() as u64)
        .ok_or(WorkspaceStoreError::StoreTooLarge)?;
    Ok(Some(CommittedWorkspaceSnapshot {
        snapshot: shard.payload,
        committed_bytes,
    }))
}

fn shard_name(generation: u64) -> String {
    format!("shard-{generation:020}.json")
}

fn next_profile_generation(
    directory: &fs::File,
    committed_generation: u64,
) -> Result<u64, WorkspaceStoreError> {
    let observed_generation = directory_entry_names(directory)?
        .into_iter()
        .filter_map(|name| {
            name.strip_prefix("shard-")
                .and_then(|value| value.strip_suffix(".json"))
                .filter(|value| {
                    value.len() == 20 && value.bytes().all(|byte| byte.is_ascii_digit())
                })
                .and_then(|value| value.parse::<u64>().ok())
        })
        .fold(committed_generation, u64::max);
    observed_generation
        .checked_add(1)
        .ok_or(WorkspaceStoreError::StoreTooLarge)
}

fn checksum(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(64);
    for byte in digest {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

fn quarantine_scope(instance_id: ProfileInstanceId) -> String {
    checksum(instance_id.as_bytes())
}

fn bound_quarantine_entry_name(
    instance_id: ProfileInstanceId,
    nonce: ProfileInstanceId,
    identity: ManagedEntryIdentity,
) -> String {
    format!(
        "q-{}-{nonce}.{}.{:016x}.{:016x}.bin",
        quarantine_scope(instance_id),
        identity.kind.marker(),
        identity.device,
        identity.inode
    )
}

fn is_checksum(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn ensure_private_directory(path: &Path) -> Result<bool, WorkspaceStoreError> {
    match fs::symlink_metadata(path) {
        Ok(_) => {
            validate_private_directory(path)?;
            Ok(false)
        }
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt as _;
                let mut builder = fs::DirBuilder::new();
                builder.mode(0o700);
                match builder.create(path) {
                    Ok(()) => {}
                    Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => {
                        validate_private_directory(path)?;
                        return Ok(false);
                    }
                    Err(source) => return Err(source.into()),
                }
            }
            #[cfg(not(unix))]
            fs::create_dir(path)?;
            validate_private_directory(path)?;
            Ok(true)
        }
        Err(source) => Err(WorkspaceStoreError::from(source)),
    }
}

fn validate_private_directory(path: &Path) -> Result<(), WorkspaceStoreError> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(WorkspaceStoreError::UnsafePath);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if metadata.permissions().mode() & 0o7777 != 0o700 {
            return Err(WorkspaceStoreError::UnsafePath);
        }
    }
    Ok(())
}

fn open_directory_handle(path: &Path) -> Result<fs::File, WorkspaceStoreError> {
    Ok(fs::File::from(
        rustix::fs::open(
            path,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::CLOEXEC
                | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::empty(),
        )
        .map_err(workspace_error_from_errno)?,
    ))
}

fn open_private_directory(path: &Path) -> Result<fs::File, WorkspaceStoreError> {
    #[cfg(unix)]
    let file = fs::File::from(
        rustix::fs::open(
            path,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::CLOEXEC
                | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::empty(),
        )
        .map_err(workspace_error_from_errno)?,
    );
    #[cfg(not(unix))]
    let file = fs::File::open(path)?;
    validate_private_directory_metadata(&file.metadata()?)?;
    Ok(file)
}

fn ensure_private_directory_at(
    parent: &fs::File,
    name: &str,
    path: &Path,
) -> Result<fs::File, WorkspaceStoreError> {
    #[cfg(unix)]
    let created = match rustix::fs::mkdirat(parent, name, rustix::fs::Mode::RWXU) {
        Ok(()) => true,
        Err(source) if source == rustix::io::Errno::EXIST => false,
        Err(source) => return Err(workspace_error_from_errno(source)),
    };
    #[cfg(not(unix))]
    let created = ensure_private_directory(path)?;
    let directory = open_private_directory_at(parent, name, path)?;
    if created {
        sync_directory_handle(&directory)?;
        sync_directory_handle(parent)?;
    }
    Ok(directory)
}

fn open_private_directory_at(
    parent: &fs::File,
    name: &str,
    _path: &Path,
) -> Result<fs::File, WorkspaceStoreError> {
    open_optional_private_directory_at(parent, name, _path)?
        .ok_or(WorkspaceStoreError::Io(WorkspaceIoKind::NotFound))
}

fn open_optional_private_directory_at(
    parent: &fs::File,
    name: &str,
    _path: &Path,
) -> Result<Option<fs::File>, WorkspaceStoreError> {
    #[cfg(unix)]
    return open_optional_private_directory_entry(parent, name);
    #[cfg(not(unix))]
    {
        let file = match fs::File::open(_path) {
            Ok(file) => file,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(source.into()),
        };
        validate_private_directory_metadata(&file.metadata()?)?;
        Ok(Some(file))
    }
}

fn open_private_directory_entry(
    parent: &fs::File,
    name: &str,
) -> Result<fs::File, WorkspaceStoreError> {
    open_optional_private_directory_entry(parent, name)?
        .ok_or(WorkspaceStoreError::Io(WorkspaceIoKind::NotFound))
}

fn open_directory_entry_unchecked(
    parent: &fs::File,
    name: &str,
) -> Result<fs::File, WorkspaceStoreError> {
    Ok(fs::File::from(
        rustix::fs::openat(
            parent,
            name,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::CLOEXEC
                | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::empty(),
        )
        .map_err(workspace_error_from_errno)?,
    ))
}

fn open_optional_private_directory_entry(
    parent: &fs::File,
    name: &str,
) -> Result<Option<fs::File>, WorkspaceStoreError> {
    let file = match rustix::fs::openat(
        parent,
        name,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::DIRECTORY
            | rustix::fs::OFlags::CLOEXEC
            | rustix::fs::OFlags::NOFOLLOW,
        rustix::fs::Mode::empty(),
    ) {
        Ok(file) => fs::File::from(file),
        Err(source) if source == rustix::io::Errno::NOENT => return Ok(None),
        Err(source) => return Err(workspace_error_from_errno(source)),
    };
    validate_private_directory_metadata(&file.metadata()?)?;
    Ok(Some(file))
}

fn open_private_lock_at(
    parent: &fs::File,
    name: &str,
    _path: &Path,
) -> Result<fs::File, WorkspaceStoreError> {
    #[cfg(unix)]
    let file = fs::File::from(
        rustix::fs::openat(
            parent,
            name,
            rustix::fs::OFlags::RDWR
                | rustix::fs::OFlags::CREATE
                | rustix::fs::OFlags::CLOEXEC
                | rustix::fs::OFlags::NONBLOCK
                | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        )
        .map_err(workspace_error_from_errno)?,
    );
    #[cfg(not(unix))]
    let file = {
        let mut options = fs::OpenOptions::new();
        options.read(true).write(true).create(true);
        options.open(_path)?
    };
    validate_private_file_metadata(&file.metadata()?)?;
    Ok(file)
}

fn validate_private_directory_metadata(metadata: &fs::Metadata) -> Result<(), WorkspaceStoreError> {
    if !metadata.is_dir() {
        return Err(WorkspaceStoreError::UnsafePath);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if metadata.permissions().mode() & 0o7777 != 0o700 {
            return Err(WorkspaceStoreError::UnsafePath);
        }
    }
    Ok(())
}

fn validate_private_file_metadata(metadata: &fs::Metadata) -> Result<(), WorkspaceStoreError> {
    if !metadata.is_file() {
        return Err(WorkspaceStoreError::UnsafePath);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        if metadata.permissions().mode() & 0o7777 != 0o600 {
            return Err(WorkspaceStoreError::UnsafePath);
        }
        if metadata.nlink() != 1 {
            return Err(WorkspaceStoreError::UnsafePath);
        }
    }
    Ok(())
}

fn read_optional_private_file_at(
    directory: &fs::File,
    name: &str,
    maximum: usize,
) -> Result<Option<Vec<u8>>, WorkspaceStoreError> {
    let Some(file) = open_optional_private_file_at(directory, name)? else {
        return Ok(None);
    };
    read_private_file_handle(file, maximum).map(Some)
}

fn open_optional_private_file_at(
    directory: &fs::File,
    name: &str,
) -> Result<Option<fs::File>, WorkspaceStoreError> {
    #[cfg(unix)]
    let file = match rustix::fs::openat(
        directory,
        name,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::CLOEXEC
            | rustix::fs::OFlags::NONBLOCK
            | rustix::fs::OFlags::NOFOLLOW,
        rustix::fs::Mode::empty(),
    ) {
        Ok(file) => fs::File::from(file),
        Err(source) if source == rustix::io::Errno::NOENT => return Ok(None),
        Err(source) => return Err(workspace_error_from_errno(source)),
    };
    #[cfg(not(unix))]
    let file = match fs::File::open(name) {
        Ok(file) => file,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(source.into()),
    };
    Ok(Some(file))
}

fn read_private_file_at(
    directory: &fs::File,
    name: &str,
    maximum: usize,
) -> Result<Vec<u8>, WorkspaceStoreError> {
    read_optional_private_file_at(directory, name, maximum)?
        .ok_or(WorkspaceStoreError::Io(WorkspaceIoKind::NotFound))
}

fn read_private_file_handle(
    file: fs::File,
    maximum: usize,
) -> Result<Vec<u8>, WorkspaceStoreError> {
    let metadata = file.metadata()?;
    if metadata.len() > maximum as u64 {
        return Err(WorkspaceStoreError::ShardTooLarge);
    }
    validate_private_file_metadata(&metadata)?;
    let mut bytes = Vec::new();
    file.take(maximum as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > maximum {
        return Err(WorkspaceStoreError::ShardTooLarge);
    }
    Ok(bytes)
}

fn read_optional_private_file_snapshot_at(
    directory: &fs::File,
    name: &str,
    maximum: usize,
) -> Result<Option<PrivateFileSnapshot>, WorkspaceStoreError> {
    let Some(file) = open_optional_private_file_at(directory, name)? else {
        return Ok(None);
    };
    let initial =
        private_file_fingerprint(&rustix::fs::fstat(&file).map_err(workspace_error_from_errno)?)?;
    if initial.length > maximum as u64 {
        return Err(WorkspaceStoreError::ShardTooLarge);
    }
    let mut bytes = Vec::new();
    (&file).take(maximum as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > maximum {
        return Err(WorkspaceStoreError::ShardTooLarge);
    }
    let final_fingerprint =
        private_file_fingerprint(&rustix::fs::fstat(&file).map_err(workspace_error_from_errno)?)?;
    let path_fingerprint = optional_private_file_fingerprint_at(directory, name)?;
    if initial != final_fingerprint || path_fingerprint != Some(final_fingerprint) {
        return Err(WorkspaceStoreError::ExternalChange);
    }
    Ok(Some(PrivateFileSnapshot {
        bytes,
        fingerprint: final_fingerprint,
    }))
}

fn optional_private_file_fingerprint_at(
    directory: &fs::File,
    name: &str,
) -> Result<Option<PrivateFileFingerprint>, WorkspaceStoreError> {
    let stat = match rustix::fs::statat(directory, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => stat,
        Err(source) if source == rustix::io::Errno::NOENT => return Ok(None),
        Err(source) => return Err(workspace_error_from_errno(source)),
    };
    private_file_fingerprint(&stat).map(Some)
}

fn private_file_fingerprint(
    stat: &rustix::fs::Stat,
) -> Result<PrivateFileFingerprint, WorkspaceStoreError> {
    if !stat_is_private_regular(stat) {
        return Err(WorkspaceStoreError::UnsafePath);
    }
    Ok(PrivateFileFingerprint {
        device: stat
            .st_dev
            .try_into()
            .map_err(|_| WorkspaceStoreError::UnsafePath)?,
        inode: stat.st_ino,
        mode: stat.st_mode.into(),
        links: stat.st_nlink.into(),
        length: stat
            .st_size
            .try_into()
            .map_err(|_| WorkspaceStoreError::UnsafePath)?,
        modified_seconds: stat.st_mtime,
        modified_nanoseconds: stat.st_mtime_nsec,
        changed_seconds: stat.st_ctime,
        changed_nanoseconds: stat.st_ctime_nsec,
    })
}

fn create_private_temp_at(
    directory: &fs::File,
    purpose: &str,
) -> Result<(String, fs::File), WorkspaceStoreError> {
    let nonce =
        ProfileInstanceId::generate().map_err(|_| WorkspaceStoreError::WriterUnavailable)?;
    let name = format!(".dbotter-workspace.{purpose}.tmp.{nonce}",);
    let file = fs::File::from(
        rustix::fs::openat(
            directory,
            &name,
            rustix::fs::OFlags::WRONLY
                | rustix::fs::OFlags::CREATE
                | rustix::fs::OFlags::EXCL
                | rustix::fs::OFlags::CLOEXEC
                | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        )
        .map_err(workspace_error_from_errno)?,
    );
    validate_private_file_metadata(&file.metadata()?)?;
    Ok((name, file))
}

#[cfg(unix)]
fn workspace_error_from_errno(source: rustix::io::Errno) -> WorkspaceStoreError {
    if source == rustix::io::Errno::LOOP
        || source == rustix::io::Errno::NOTDIR
        || source == rustix::io::Errno::ISDIR
    {
        WorkspaceStoreError::UnsafePath
    } else {
        WorkspaceStoreError::from(std::io::Error::from(source))
    }
}

fn rename_no_replace_at(directory: &fs::File, from: &str, to: &str) -> rustix::io::Result<()> {
    rustix::fs::renameat_with(
        directory,
        from,
        directory,
        to,
        rustix::fs::RenameFlags::NOREPLACE,
    )
}

fn rename_between_no_replace(
    source_directory: &fs::File,
    source: &str,
    destination_directory: &fs::File,
    destination: &str,
) -> rustix::io::Result<()> {
    rustix::fs::renameat_with(
        source_directory,
        source,
        destination_directory,
        destination,
        rustix::fs::RenameFlags::NOREPLACE,
    )
}

fn replace_at(
    directory: &fs::File,
    source: &str,
    destination: &str,
) -> Result<(), WorkspaceStoreError> {
    rustix::fs::renameat(directory, source, directory, destination)
        .map_err(workspace_error_from_errno)
}

fn sync_directory_handle(directory: &fs::File) -> Result<(), WorkspaceStoreError> {
    rustix::fs::fsync(directory).map_err(workspace_error_from_errno)
}

fn directory_entry_names(directory: &fs::File) -> Result<Vec<String>, WorkspaceStoreError> {
    const MAX_DIRECTORY_ENTRIES: usize = 10_000;

    let mut stream = rustix::fs::Dir::read_from(directory).map_err(workspace_error_from_errno)?;
    let mut names = Vec::new();
    while let Some(entry) = stream.read() {
        let entry = entry.map_err(workspace_error_from_errno)?;
        let bytes = entry.file_name().to_bytes();
        if bytes == b"." || bytes == b".." {
            continue;
        }
        if names.len() == MAX_DIRECTORY_ENTRIES {
            return Err(WorkspaceStoreError::StoreTooLarge);
        }
        let name = std::str::from_utf8(bytes).map_err(|_| WorkspaceStoreError::UnsafePath)?;
        names.push(name.to_owned());
    }
    Ok(names)
}

fn validate_retained_store_entries(root: &fs::File) -> Result<(), WorkspaceStoreError> {
    fn walk(
        directory: &fs::File,
        depth: usize,
        entries: &mut usize,
    ) -> Result<(), WorkspaceStoreError> {
        if depth > 4 {
            return Err(WorkspaceStoreError::StoreTooLarge);
        }
        for name in directory_entry_names(directory)? {
            *entries = entries
                .checked_add(1)
                .ok_or(WorkspaceStoreError::StoreTooLarge)?;
            if *entries > 10_000 {
                return Err(WorkspaceStoreError::StoreTooLarge);
            }
            let stat = rustix::fs::statat(directory, &name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)
                .map_err(workspace_error_from_errno)?;
            let file_type = rustix::fs::FileType::from_raw_mode(stat.st_mode);
            if file_type.is_dir() {
                let child = open_private_directory_entry(directory, &name)?;
                walk(&child, depth + 1, entries)?;
            } else if !stat_is_private_regular(&stat) {
                return Err(WorkspaceStoreError::UnsafePath);
            }
        }
        Ok(())
    }
    walk(root, 0, &mut 0)
}

fn cleanup_profile_entries(
    directory_handle: &fs::File,
    retained_shard_name: Option<&str>,
) -> Result<(), WorkspaceStoreError> {
    for name_text in directory_entry_names(directory_handle)? {
        if name_text == "manifest.json" || retained_shard_name == Some(name_text.as_str()) {
            continue;
        }
        let is_shard = name_text.starts_with("shard-") && name_text.ends_with(".json");
        let is_temp = name_text.starts_with(".dbotter-workspace.") && name_text.contains(".tmp.");
        if is_shard || is_temp {
            let stat = rustix::fs::statat(
                directory_handle,
                &name_text,
                rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
            )
            .map_err(workspace_error_from_errno)?;
            if !stat_is_private_regular(&stat) {
                return Err(WorkspaceStoreError::UnsafePath);
            }
            rustix::fs::unlinkat(directory_handle, &name_text, rustix::fs::AtFlags::empty())
                .map_err(workspace_error_from_errno)?;
        } else {
            return Err(WorkspaceStoreError::UnsafePath);
        }
    }
    Ok(())
}

fn trim_quarantine(directory_handle: &fs::File) -> Result<(), WorkspaceStoreError> {
    let mut files = Vec::new();
    for name in directory_entry_names(directory_handle)? {
        let binding = parse_quarantine_entry_binding(&name)?;
        let stat = rustix::fs::statat(
            directory_handle,
            &name,
            rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
        )
        .map_err(workspace_error_from_errno)?;
        if !stat_is_private_regular(&stat) {
            return Err(WorkspaceStoreError::UnsafePath);
        }
        if let Some(expected) = binding
            && managed_entry_identity(&stat)? != expected
        {
            return Err(WorkspaceStoreError::UnsafePath);
        }
        let length = u64::try_from(stat.st_size).map_err(|_| WorkspaceStoreError::StoreTooLarge)?;
        files.push((name, binding, (stat.st_mtime, stat.st_mtime_nsec), length));
    }
    files.sort_by(|left, right| left.2.cmp(&right.2).then_with(|| left.0.cmp(&right.0)));
    let mut total = files.iter().try_fold(0_u64, |sum, item| {
        sum.checked_add(item.3)
            .ok_or(WorkspaceStoreError::StoreTooLarge)
    })?;
    let mut retained_count = files.len();
    let mut removal_index = 0_usize;
    while retained_count > MAX_QUARANTINE_FILES || total > MAX_QUARANTINE_BYTES {
        let (name, binding, _, length) = &files[removal_index];
        if let Some(expected) = binding {
            validate_managed_entry_identity_at(directory_handle, name, *expected)?;
        }
        rustix::fs::unlinkat(directory_handle, name, rustix::fs::AtFlags::empty())
            .map_err(workspace_error_from_errno)?;
        total = total
            .checked_sub(*length)
            .ok_or(WorkspaceStoreError::StoreTooLarge)?;
        retained_count -= 1;
        removal_index += 1;
    }
    sync_directory_handle(directory_handle)
}

fn parse_quarantine_entry_binding(
    name: &str,
) -> Result<Option<ManagedEntryIdentity>, WorkspaceStoreError> {
    let value = name
        .strip_prefix("q-")
        .and_then(|value| value.strip_suffix(".bin"))
        .ok_or(WorkspaceStoreError::UnsafePath)?;
    let bytes = value.as_bytes();
    if bytes.len() <= 65 || bytes[64] != b'-' {
        return Err(WorkspaceStoreError::UnsafePath);
    }
    let scope = std::str::from_utf8(&bytes[..64]).map_err(|_| WorkspaceStoreError::UnsafePath)?;
    let suffix = std::str::from_utf8(&bytes[65..]).map_err(|_| WorkspaceStoreError::UnsafePath)?;
    if !is_checksum(scope) {
        return Err(WorkspaceStoreError::UnsafePath);
    }
    if ProfileInstanceId::parse(suffix).is_ok() {
        return Ok(None);
    }
    let mut parts = suffix.split('.');
    let nonce = parts.next().ok_or(WorkspaceStoreError::UnsafePath)?;
    ProfileInstanceId::parse(nonce).map_err(|_| WorkspaceStoreError::UnsafePath)?;
    let identity = parse_managed_entry_identity_parts(&mut parts)
        .map_err(|_| WorkspaceStoreError::UnsafePath)?;
    if parts.next().is_some() {
        return Err(WorkspaceStoreError::UnsafePath);
    }
    Ok(Some(identity))
}

struct TempCleanup {
    directory: fs::File,
    name: String,
    armed: AtomicBool,
}

impl TempCleanup {
    fn new(directory: &fs::File, name: String) -> Result<Self, WorkspaceStoreError> {
        Ok(Self {
            directory: directory.try_clone()?,
            name,
            armed: AtomicBool::new(true),
        })
    }

    fn disarm(&self) {
        self.armed.store(false, Ordering::Relaxed);
    }
}

impl Drop for TempCleanup {
    fn drop(&mut self) {
        if self.armed.load(Ordering::Relaxed) {
            let _ = rustix::fs::unlinkat(&self.directory, &self.name, rustix::fs::AtFlags::empty());
        }
    }
}

fn write_quarantine_marker(directory: &fs::File, name: &str) -> Result<(), WorkspaceStoreError> {
    let mut file = fs::File::from(
        rustix::fs::openat(
            directory,
            name,
            rustix::fs::OFlags::WRONLY
                | rustix::fs::OFlags::CREATE
                | rustix::fs::OFlags::EXCL
                | rustix::fs::OFlags::CLOEXEC
                | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        )
        .map_err(workspace_error_from_errno)?,
    );
    validate_private_file_metadata(&file.metadata()?)?;
    file.write_all(b"unsafe workspace entry removed\n")?;
    file.sync_all()?;
    Ok(())
}

fn stat_is_private_regular(stat: &rustix::fs::Stat) -> bool {
    rustix::fs::FileType::from_raw_mode(stat.st_mode).is_file()
        && stat.st_mode & 0o7777 == 0o600
        && stat.st_nlink == 1
}

fn managed_entry_identity(
    stat: &rustix::fs::Stat,
) -> Result<ManagedEntryIdentity, WorkspaceStoreError> {
    let file_type = rustix::fs::FileType::from_raw_mode(stat.st_mode);
    let kind = if file_type.is_dir() {
        ManagedEntryKind::Directory
    } else if file_type.is_file() {
        ManagedEntryKind::RegularFile
    } else if file_type.is_symlink() {
        ManagedEntryKind::Symlink
    } else {
        ManagedEntryKind::Other
    };
    Ok(ManagedEntryIdentity {
        device: stat
            .st_dev
            .try_into()
            .map_err(|_| WorkspaceStoreError::UnsafePath)?,
        inode: stat.st_ino,
        kind,
    })
}

fn optional_managed_entry_identity_at(
    parent: &fs::File,
    name: &str,
) -> Result<Option<ManagedEntryIdentity>, WorkspaceStoreError> {
    let stat = match rustix::fs::statat(parent, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => stat,
        Err(source) if source == rustix::io::Errno::NOENT => return Ok(None),
        Err(source) => return Err(workspace_error_from_errno(source)),
    };
    managed_entry_identity(&stat).map(Some)
}

fn validate_managed_entry_identity_at(
    parent: &fs::File,
    name: &str,
    expected: ManagedEntryIdentity,
) -> Result<(), WorkspaceStoreError> {
    let stat = rustix::fs::statat(parent, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)
        .map_err(workspace_error_from_errno)?;
    if managed_entry_identity(&stat)? != expected {
        return Err(WorkspaceStoreError::UnsafePath);
    }
    Ok(())
}

fn remove_managed_entry_at(parent: &fs::File, name: &str) -> Result<bool, WorkspaceStoreError> {
    fn remove_bounded(
        parent: &fs::File,
        name: &str,
        depth: usize,
        entries: &mut usize,
    ) -> Result<bool, WorkspaceStoreError> {
        const MAX_REMOVAL_DEPTH: usize = 8;
        const MAX_REMOVAL_ENTRIES: usize = 10_000;

        let stat = match rustix::fs::statat(parent, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => stat,
            Err(source) if source == rustix::io::Errno::NOENT => return Ok(false),
            Err(source) => return Err(workspace_error_from_errno(source)),
        };
        *entries = entries
            .checked_add(1)
            .ok_or(WorkspaceStoreError::StoreTooLarge)?;
        if *entries > MAX_REMOVAL_ENTRIES {
            return Err(WorkspaceStoreError::StoreTooLarge);
        }
        if rustix::fs::FileType::from_raw_mode(stat.st_mode).is_dir() {
            if depth >= MAX_REMOVAL_DEPTH {
                return Err(WorkspaceStoreError::StoreTooLarge);
            }
            let directory = open_directory_entry_unchecked(parent, name)?;
            validate_directory_identity(parent, name, &directory)?;
            for child in directory_entry_names(&directory)? {
                remove_bounded(&directory, &child, depth + 1, entries)?;
            }
            sync_directory_handle(&directory)?;
            validate_directory_identity(parent, name, &directory)?;
            rustix::fs::unlinkat(parent, name, rustix::fs::AtFlags::REMOVEDIR)
                .map_err(workspace_error_from_errno)?;
        } else {
            rustix::fs::unlinkat(parent, name, rustix::fs::AtFlags::empty())
                .map_err(workspace_error_from_errno)?;
        }
        Ok(true)
    }

    remove_bounded(parent, name, 0, &mut 0)
}

fn remove_bound_profile_directory(
    parent: &fs::File,
    name: &str,
    directory: &fs::File,
    expected: ManagedEntryIdentity,
) -> Result<(), WorkspaceStoreError> {
    validate_bound_directory_entry(parent, name, directory, expected)?;
    for entry_name in directory_entry_names(directory)? {
        remove_managed_entry_at(directory, &entry_name)?;
    }
    sync_directory_handle(directory)?;
    validate_bound_directory_entry(parent, name, directory, expected)?;
    rustix::fs::unlinkat(parent, name, rustix::fs::AtFlags::REMOVEDIR)
        .map_err(workspace_error_from_errno)
}

fn remove_bound_non_directory_entry(
    parent: &fs::File,
    name: &str,
    expected: ManagedEntryIdentity,
) -> Result<(), WorkspaceStoreError> {
    if expected.kind == ManagedEntryKind::Directory {
        return Err(WorkspaceStoreError::UnsafePath);
    }
    validate_managed_entry_identity_at(parent, name, expected)?;
    rustix::fs::unlinkat(parent, name, rustix::fs::AtFlags::empty())
        .map_err(workspace_error_from_errno)
}

fn validate_bound_directory_entry(
    parent: &fs::File,
    name: &str,
    directory: &fs::File,
    expected: ManagedEntryIdentity,
) -> Result<(), WorkspaceStoreError> {
    if expected.kind != ManagedEntryKind::Directory {
        return Err(WorkspaceStoreError::UnsafePath);
    }
    let path_stat = rustix::fs::statat(parent, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)
        .map_err(workspace_error_from_errno)?;
    let descriptor_stat = rustix::fs::fstat(directory).map_err(workspace_error_from_errno)?;
    if managed_entry_identity(&path_stat)? != expected
        || managed_entry_identity(&descriptor_stat)? != expected
    {
        return Err(WorkspaceStoreError::UnsafePath);
    }
    Ok(())
}

fn validate_directory_entry(
    parent: &fs::File,
    name: &str,
    directory: &fs::File,
) -> Result<(), WorkspaceStoreError> {
    validate_directory_entry_os(parent, OsStr::new(name), directory)
}

fn validate_directory_entry_os(
    parent: &fs::File,
    name: &OsStr,
    directory: &fs::File,
) -> Result<(), WorkspaceStoreError> {
    let path_stat = rustix::fs::statat(parent, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)
        .map_err(workspace_error_from_errno)?;
    let descriptor_stat = rustix::fs::fstat(directory).map_err(workspace_error_from_errno)?;
    if !rustix::fs::FileType::from_raw_mode(path_stat.st_mode).is_dir()
        || path_stat.st_mode & 0o7777 != 0o700
        || path_stat.st_dev != descriptor_stat.st_dev
        || path_stat.st_ino != descriptor_stat.st_ino
    {
        return Err(WorkspaceStoreError::UnsafePath);
    }
    Ok(())
}

fn validate_directory_identity(
    parent: &fs::File,
    name: &str,
    directory: &fs::File,
) -> Result<(), WorkspaceStoreError> {
    let path_stat = rustix::fs::statat(parent, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)
        .map_err(workspace_error_from_errno)?;
    let descriptor_stat = rustix::fs::fstat(directory).map_err(workspace_error_from_errno)?;
    if !rustix::fs::FileType::from_raw_mode(path_stat.st_mode).is_dir()
        || path_stat.st_dev != descriptor_stat.st_dev
        || path_stat.st_ino != descriptor_stat.st_ino
    {
        return Err(WorkspaceStoreError::UnsafePath);
    }
    Ok(())
}

fn validate_private_file_entry(
    parent: &fs::File,
    name: &str,
    file: &fs::File,
) -> Result<(), WorkspaceStoreError> {
    let path_stat = rustix::fs::statat(parent, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)
        .map_err(workspace_error_from_errno)?;
    let descriptor_stat = rustix::fs::fstat(file).map_err(workspace_error_from_errno)?;
    if private_file_fingerprint(&path_stat)? != private_file_fingerprint(&descriptor_stat)? {
        return Err(WorkspaceStoreError::UnsafePath);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    fn retention_history(
        id: u64,
        completed_at_unix_ms: i64,
        status: WorkspaceHistoryStatus,
        source: &str,
    ) -> WorkspaceHistoryEntry {
        WorkspaceHistoryEntry::new(
            id,
            source,
            WorkspaceRunTarget::Current,
            completed_at_unix_ms,
            status,
            1,
            0,
            0,
            false,
        )
        .expect("retention history")
    }

    fn retention_profile(
        instance_byte: u8,
        history: Vec<WorkspaceHistoryEntry>,
    ) -> ProfileWorkspaceSnapshot {
        ProfileWorkspaceSnapshot::new(
            ProfileInstanceId::from_bytes([instance_byte; 16]),
            ProfileId(format!("retention-{instance_byte:02x}")),
            true,
            Vec::new(),
            None,
            WorkspaceGeometrySnapshot::new(300.0, 0.6, true).expect("retention geometry"),
            history,
        )
        .expect("retention profile")
    }

    fn unbounded_retention_limits() -> WorkspaceRetentionLimits {
        WorkspaceRetentionLimits {
            history_entries_per_profile: usize::MAX,
            history_entries_total: usize::MAX,
            profile_shard_bytes: usize::MAX,
            workspace_store_bytes: u64::MAX,
        }
    }

    fn actual_conservative_encoded_profile_size(
        snapshot: &ProfileWorkspaceSnapshot,
    ) -> ConservativeEncodedProfileSize {
        let generation = u64::MAX;
        let payload = serde_json::to_vec(snapshot).expect("actual payload encoding");
        let envelope = WorkspaceShard {
            schema: SHARD_SCHEMA.to_owned(),
            instance_id: snapshot.instance_id(),
            generation,
            payload_length: payload.len() as u64,
            payload_checksum: checksum(&payload),
            payload: snapshot.clone(),
        };
        let shard = serde_json::to_vec(&envelope).expect("actual shard encoding");
        let manifest = WorkspaceManifest {
            schema: MANIFEST_SCHEMA.to_owned(),
            instance_id: snapshot.instance_id(),
            generation,
            shard: shard_name(generation),
            shard_length: shard.len() as u64,
            checksum: checksum(&shard),
        };
        let manifest = serde_json::to_vec(&manifest).expect("actual manifest encoding");
        ConservativeEncodedProfileSize {
            shard_bytes: shard.len() as u64,
            committed_bytes: (shard.len() + manifest.len()) as u64,
        }
    }

    fn actual_conservative_encoded_profile_size_after_evictions(
        profile: &ProfileWorkspaceSnapshot,
        candidates: &[HistoryRetentionCandidate],
        removal_count: usize,
    ) -> ConservativeEncodedProfileSize {
        let removed = candidates
            .iter()
            .take(removal_count)
            .map(|candidate| candidate.history_id)
            .collect::<HashSet<_>>();
        let mut projected = profile.clone();
        projected
            .history
            .retain(|entry| !removed.contains(&entry.id));
        actual_conservative_encoded_profile_size(&projected)
    }

    #[test]
    fn precomputed_accounting_matches_actual_serde_at_escape_comma_and_digit_boundaries() {
        let base = retention_profile(
            0x61,
            vec![retention_history(
                1,
                1,
                WorkspaceHistoryStatus::Succeeded,
                "",
            )],
        );
        let base_payload_bytes = serde_json::to_vec(&base)
            .expect("base payload encoding")
            .len();
        assert!(base_payload_bytes < 999);
        for target_payload_bytes in [999_usize, 1_000, 9_999, 10_000] {
            let profile = retention_profile(
                0x61,
                vec![retention_history(
                    1,
                    1,
                    WorkspaceHistoryStatus::Succeeded,
                    &"a".repeat(target_payload_bytes - base_payload_bytes),
                )],
            );
            assert_eq!(
                serde_json::to_vec(&profile)
                    .expect("boundary payload encoding")
                    .len(),
                target_payload_bytes
            );
            assert_eq!(
                ProfileEncodedAccounting::new(&profile)
                    .expect("boundary accounting")
                    .encoded_size()
                    .expect("boundary encoded size"),
                actual_conservative_encoded_profile_size(&profile)
            );
        }

        let escaped = retention_profile(
            0x62,
            vec![
                retention_history(1, 1, WorkspaceHistoryStatus::Succeeded, "\"\\\nfirst"),
                retention_history(2, 2, WorkspaceHistoryStatus::OutcomeUnknown, "\tprotected"),
                retention_history(3, 3, WorkspaceHistoryStatus::Cancelled, "\r\nlast"),
            ],
        );
        let accounting = ProfileEncodedAccounting::new(&escaped).expect("escaped accounting");
        assert_eq!(
            accounting.encoded_size().expect("escaped encoded size"),
            actual_conservative_encoded_profile_size(&escaped)
        );
        let candidates = history_retention_candidates(std::slice::from_ref(&escaped), Some(0));
        let prefix_sizes =
            profile_shard_bytes_after_prefixes(&accounting, &candidates).expect("profile prefixes");
        for removal_count in 1..=candidates.len() {
            assert_eq!(
                prefix_sizes[removal_count - 1],
                actual_conservative_encoded_profile_size_after_evictions(
                    &escaped,
                    &candidates,
                    removal_count,
                )
                .shard_bytes
            );
        }

        let profiles = vec![
            escaped.clone(),
            retention_profile(
                0x63,
                vec![retention_history(
                    7,
                    0,
                    WorkspaceHistoryStatus::Failed(WorkspaceHistoryCode::Backend),
                    "\\global\noldest",
                )],
            ),
        ];
        let accounting = profiles
            .iter()
            .map(ProfileEncodedAccounting::new)
            .collect::<Result<Vec<_>, _>>()
            .expect("total accounting");
        let candidates = history_retention_candidates(&profiles, None);
        let prefix_sizes =
            encoded_store_bytes_after_prefixes(&accounting, &candidates).expect("store prefixes");
        for removal_count in 1..=candidates.len() {
            let removed = candidates
                .iter()
                .take(removal_count)
                .map(|candidate| (candidate.profile_index, candidate.history_id))
                .collect::<HashSet<_>>();
            let actual_total = profiles
                .iter()
                .enumerate()
                .map(|(profile_index, profile)| {
                    let mut projected = profile.clone();
                    projected
                        .history
                        .retain(|entry| !removed.contains(&(profile_index, entry.id)));
                    actual_conservative_encoded_profile_size(&projected).committed_bytes
                })
                .sum::<u64>();
            assert_eq!(prefix_sizes[removal_count - 1], actual_total);
        }

        let (_, manifest_fixed_bytes) =
            conservative_encoded_overheads(escaped.instance_id()).expect("manifest overhead");
        for shard_bytes in [9_u64, 10, 99, 100, 999, 1_000, 9_999, 10_000, u64::MAX] {
            let manifest = WorkspaceManifest {
                schema: MANIFEST_SCHEMA.to_owned(),
                instance_id: escaped.instance_id(),
                generation: u64::MAX,
                shard: shard_name(u64::MAX),
                shard_length: shard_bytes,
                checksum: RETENTION_CHECKSUM_PLACEHOLDER.to_owned(),
            };
            assert_eq!(
                manifest_fixed_bytes + decimal_digits(shard_bytes),
                serde_json::to_vec(&manifest)
                    .expect("manifest digit-boundary encoding")
                    .len() as u64
            );
        }
    }

    #[test]
    fn load_metadata_reports_raw_noncanonical_manifest_bytes() {
        let directory = tempfile::tempdir().expect("workspace metadata directory");
        let config_path = directory.path().join("config.toml");
        let snapshot = retention_profile(
            0x64,
            vec![retention_history(
                1,
                1,
                WorkspaceHistoryStatus::Succeeded,
                "SELECT raw_manifest_bytes",
            )],
        );
        let store = WorkspaceStore::open(&config_path).expect("metadata workspace store");
        let commit = store.commit(&snapshot).expect("canonical workspace commit");
        let canonical_bytes = encoded_profile_bytes_at_generation(&snapshot, commit.generation())
            .expect("canonical committed bytes")
            .1;
        assert_eq!(commit.committed_bytes(), canonical_bytes);

        let manifest_path = store
            .profile_directory(snapshot.instance_id())
            .join("manifest.json");
        let mut manifest_bytes = fs::read(&manifest_path).expect("canonical manifest");
        manifest_bytes.push(b'\n');
        fs::write(&manifest_path, &manifest_bytes).expect("valid noncanonical manifest");

        let (loaded, generation, committed_bytes) = store
            .load_with_metadata(snapshot.instance_id())
            .expect("load valid noncanonical manifest");
        assert_eq!(loaded.as_ref(), Some(&snapshot));
        assert_eq!(generation, Some(commit.generation()));
        assert_eq!(committed_bytes, canonical_bytes.saturating_add(1));
        assert_eq!(
            committed_bytes,
            fs::metadata(&manifest_path)
                .expect("raw manifest metadata")
                .len()
                .saturating_add(
                    fs::metadata(
                        store
                            .profile_directory(snapshot.instance_id())
                            .join(shard_name(commit.generation())),
                    )
                    .expect("raw shard metadata")
                    .len()
                )
        );
    }

    #[test]
    fn dropping_writer_unlocks_an_inherited_descriptor() {
        let directory = tempfile::tempdir().expect("inherited writer descriptor directory");
        let config_path = directory.path().join("config.toml");
        let store = WorkspaceStore::open(&config_path).expect("initial workspace writer");
        assert_eq!(store.mode(), WorkspaceStoreMode::ReadWrite);
        let inherited_descriptor = store
            ._lock
            .try_clone()
            .expect("simulate a descriptor inherited across fork");

        drop(store);
        let reopened = WorkspaceStore::open(&config_path).expect("reopen after writer drop");
        assert_eq!(reopened.mode(), WorkspaceStoreMode::ReadWrite);

        drop(inherited_descriptor);
    }

    #[test]
    fn tiny_count_limits_apply_per_profile_and_total_key_order_without_evicting_unknown() {
        let protected =
            retention_history(1, -100, WorkspaceHistoryStatus::OutcomeUnknown, "protected");
        let profiles = vec![
            retention_profile(
                0x22,
                vec![
                    retention_history(2, 10, WorkspaceHistoryStatus::Succeeded, "later-key"),
                    protected,
                    retention_history(3, 30, WorkspaceHistoryStatus::Succeeded, "retained"),
                ],
            ),
            retention_profile(
                0x11,
                vec![retention_history(
                    9,
                    10,
                    WorkspaceHistoryStatus::Cancelled,
                    "earlier-instance-key",
                )],
            ),
        ];
        let limits = WorkspaceRetentionLimits {
            history_entries_per_profile: 2,
            history_entries_total: 2,
            ..unbounded_retention_limits()
        };

        let planned =
            WorkspaceSnapshotSet::plan_with_limits(profiles, limits).expect("tiny count plan");

        assert_eq!(planned.history_evicted(), 2);
        assert_eq!(
            planned.history_evictions()[0].instance_id(),
            ProfileInstanceId::from_bytes([0x11; 16])
        );
        assert_eq!(planned.history_evictions()[0].history_id(), 9);
        assert_eq!(
            planned.history_evictions()[1].instance_id(),
            ProfileInstanceId::from_bytes([0x22; 16])
        );
        assert_eq!(planned.history_evictions()[1].history_id(), 2);
        assert!(
            planned
                .profiles()
                .iter()
                .flat_map(ProfileWorkspaceSnapshot::history)
                .any(|entry| entry.status() == WorkspaceHistoryStatus::OutcomeUnknown)
        );
    }

    #[test]
    fn tiny_profile_byte_limit_counts_json_escapes_and_evicts_the_minimum_oldest_prefix() {
        let source = "\n".repeat(512);
        let profile = retention_profile(
            0x31,
            vec![
                retention_history(1, 1, WorkspaceHistoryStatus::Succeeded, &source),
                retention_history(2, 2, WorkspaceHistoryStatus::Succeeded, &source),
                retention_history(3, 3, WorkspaceHistoryStatus::Succeeded, &source),
            ],
        );
        let candidates = history_retention_candidates(std::slice::from_ref(&profile), Some(0));
        let after_one = conservative_encoded_profile_size_after_evictions(&profile, &candidates, 1)
            .expect("encoded size after one eviction")
            .shard_bytes;
        let initial = conservative_encoded_profile_size(&profile)
            .expect("initial encoded size")
            .shard_bytes;
        assert!(initial > after_one);
        assert!(
            initial - after_one > u64::try_from(source.len()).expect("source length"),
            "escaped newlines occupy more encoded bytes than raw source bytes"
        );
        let limits = WorkspaceRetentionLimits {
            profile_shard_bytes: usize::try_from(after_one).expect("profile shard limit"),
            ..unbounded_retention_limits()
        };

        let planned = WorkspaceSnapshotSet::plan_with_limits(vec![profile], limits)
            .expect("tiny profile-byte plan");

        assert_eq!(planned.history_evicted(), 1);
        assert_eq!(planned.history_evictions()[0].history_id(), 1);
        assert_eq!(planned.profiles()[0].history().len(), 2);
    }

    #[test]
    fn tiny_total_byte_limit_evicts_the_minimum_global_oldest_prefix() {
        let profiles = vec![
            retention_profile(
                0x42,
                vec![retention_history(
                    1,
                    20,
                    WorkspaceHistoryStatus::Succeeded,
                    "newer",
                )],
            ),
            retention_profile(
                0x41,
                vec![retention_history(
                    7,
                    10,
                    WorkspaceHistoryStatus::Failed(WorkspaceHistoryCode::Backend),
                    "older",
                )],
            ),
        ];
        let candidates = history_retention_candidates(&profiles, None);
        let after_one = conservative_encoded_projected_store_bytes(&profiles, &candidates, 1)
            .expect("encoded store size after one eviction");
        assert!(
            conservative_encoded_projected_store_bytes(&profiles, &[], 0)
                .expect("initial encoded store size")
                > after_one
        );
        let limits = WorkspaceRetentionLimits {
            workspace_store_bytes: after_one,
            ..unbounded_retention_limits()
        };

        let planned =
            WorkspaceSnapshotSet::plan_with_limits(profiles, limits).expect("tiny total-byte plan");

        assert_eq!(planned.history_evicted(), 1);
        assert_eq!(
            planned.history_evictions()[0].instance_id(),
            ProfileInstanceId::from_bytes([0x41; 16])
        );
        assert_eq!(planned.history_evictions()[0].history_id(), 7);
    }

    #[test]
    fn protected_outcome_unknown_reports_the_exhausted_byte_limit() {
        let profile = retention_profile(
            0x51,
            vec![retention_history(
                1,
                1,
                WorkspaceHistoryStatus::OutcomeUnknown,
                "protected-private-source",
            )],
        );
        let encoded = conservative_encoded_profile_size(&profile)
            .expect("protected encoded size")
            .shard_bytes;
        let limits = WorkspaceRetentionLimits {
            profile_shard_bytes: usize::try_from(encoded - 1).expect("protected shard limit"),
            ..unbounded_retention_limits()
        };

        assert!(matches!(
            WorkspaceSnapshotSet::plan_with_limits(vec![profile], limits),
            Err(WorkspaceRetentionError::RetentionExhausted(
                WorkspaceRetentionLimit::ProfileShardBytes
            ))
        ));
    }

    #[test]
    fn bound_profile_removal_refuses_a_canonical_inode_replacement() {
        let temp = tempfile::tempdir().expect("bound clear parent");
        let parent = open_directory_handle(temp.path()).expect("open bound clear parent");
        let canonical_path = temp.path().join("canonical");
        let original = ensure_private_directory_at(&parent, "canonical", &canonical_path)
            .expect("create original profile");
        let original_identity =
            managed_entry_identity(&rustix::fs::fstat(&original).expect("stat original profile"))
                .expect("original profile identity");
        let original_file = canonical_path.join("original");
        fs::write(&original_file, b"original-private-data").expect("write original data");
        fs::set_permissions(&original_file, fs::Permissions::from_mode(0o600))
            .expect("private original data");

        let displaced = temp.path().join("displaced");
        fs::rename(&canonical_path, &displaced).expect("displace original profile");
        create_test_private_directory(&canonical_path);
        let replacement_file = canonical_path.join("replacement");
        fs::write(&replacement_file, b"replacement-must-survive").expect("write replacement data");
        fs::set_permissions(&replacement_file, fs::Permissions::from_mode(0o600))
            .expect("private replacement data");

        assert!(matches!(
            remove_bound_profile_directory(&parent, "canonical", &original, original_identity,),
            Err(WorkspaceStoreError::UnsafePath)
        ));
        assert_eq!(
            fs::read(&replacement_file).expect("replacement survives refused clear"),
            b"replacement-must-survive"
        );
        assert_eq!(
            fs::read(displaced.join("original")).expect("original remains at displaced identity"),
            b"original-private-data"
        );
    }

    #[test]
    fn bound_root_corruption_markers_round_trip_within_their_dedicated_cap() {
        let profile = ManagedEntryIdentity {
            device: 0x1122,
            inode: 0x3344,
            kind: ManagedEntryKind::Directory,
        };
        let entry = ManagedEntryIdentity {
            device: 0x5566,
            inode: 0x7788,
            kind: ManagedEntryKind::Symlink,
        };
        for marker in [
            RootProfileCorruptMarker::ProfileEntry {
                state: ProfileCorruptState::UnsafePath,
                entry,
            },
            RootProfileCorruptMarker::InternalState {
                state: ProfileCorruptState::Manifest,
                profile,
                entry,
            },
        ] {
            let bytes = marker.bytes();
            assert!(bytes.len() <= MAX_ROOT_PROFILE_CORRUPT_MARKER_BYTES);
            assert_eq!(
                RootProfileCorruptMarker::parse(&bytes).expect("round-trip bound root marker"),
                marker
            );
        }
        assert!(
            RootProfileCorruptMarker::InternalState {
                state: ProfileCorruptState::Manifest,
                profile,
                entry,
            }
            .bytes()
            .len()
                > MAX_CORRUPT_STATE_BYTES
        );
    }

    #[test]
    fn root_corruption_marker_reader_accepts_exact_cap_and_rejects_plus_one() {
        let temp = tempfile::tempdir().expect("root marker cap parent");
        let directory = open_directory_handle(temp.path()).expect("open root marker cap parent");
        let marker_path = temp.path().join("marker");
        fs::write(
            &marker_path,
            vec![b'x'; MAX_ROOT_PROFILE_CORRUPT_MARKER_BYTES],
        )
        .expect("write exact-cap root marker");
        fs::set_permissions(&marker_path, fs::Permissions::from_mode(0o600))
            .expect("private exact-cap root marker");
        assert_eq!(
            read_optional_private_file_at(
                &directory,
                "marker",
                MAX_ROOT_PROFILE_CORRUPT_MARKER_BYTES,
            )
            .expect("read exact-cap root marker")
            .expect("exact-cap root marker exists")
            .len(),
            MAX_ROOT_PROFILE_CORRUPT_MARKER_BYTES
        );

        fs::write(
            &marker_path,
            vec![b'x'; MAX_ROOT_PROFILE_CORRUPT_MARKER_BYTES + 1],
        )
        .expect("write plus-one root marker");
        assert!(matches!(
            read_optional_private_file_at(
                &directory,
                "marker",
                MAX_ROOT_PROFILE_CORRUPT_MARKER_BYTES,
            ),
            Err(WorkspaceStoreError::ShardTooLarge)
        ));
    }

    fn create_test_private_directory(path: &Path) {
        fs::create_dir(path).expect("create private test directory");
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .expect("private test directory");
    }
}
