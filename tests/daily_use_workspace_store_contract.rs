use std::fs;
use std::ops::Range;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use dbotter::model::{ProfileId, ProfileInstanceId};
use dbotter::workspace::{
    EditorTabSnapshot, MAX_EDITOR_SOURCE_BYTES, MAX_EDITOR_TABS_PER_PROFILE, MAX_EDITOR_TABS_TOTAL,
    MAX_HISTORY_ENTRIES_PER_PROFILE, MAX_HISTORY_ENTRIES_TOTAL, MAX_HISTORY_SOURCE_BYTES,
    MAX_QUARANTINE_BYTES, MAX_QUARANTINE_FILES, ProfileWorkspaceSnapshot, WorkspaceCommit,
    WorkspaceGeometrySnapshot, WorkspaceHistoryCode, WorkspaceHistoryEntry, WorkspaceHistoryStatus,
    WorkspaceLanguage, WorkspaceReadOnlyReason, WorkspaceRunTarget, WorkspaceSnapshotError,
    WorkspaceSnapshotSet, WorkspaceStore, WorkspaceStoreError, WorkspaceStoreMode,
    WorkspaceStoreWarning, workspace_root_for_config,
};
use sha2::{Digest as _, Sha256};

const PRIVATE_SQL: &str = "SELECT j2_store_private_source";
const RESULT_SENTINEL: &str = "j2_result_cell_must_not_persist";
const BACKEND_SENTINEL: &str = "j2_backend_prose_must_not_persist";
const CREDENTIAL_SENTINEL: &str = "j2_credential_value_must_not_persist";

fn editor(id: u64, title: &str, source: &str) -> EditorTabSnapshot {
    EditorTabSnapshot::new(
        id,
        title,
        WorkspaceLanguage::Sql,
        source,
        Some("application"),
        source.chars().count(),
        Some(Range { start: 0, end: 6 }),
    )
    .expect("valid editor snapshot")
}

fn history(id: u64, source: &str) -> WorkspaceHistoryEntry {
    WorkspaceHistoryEntry::new(
        id,
        source,
        WorkspaceRunTarget::Current,
        1_721_234_567_890,
        WorkspaceHistoryStatus::Succeeded,
        14,
        1,
        0,
        false,
    )
    .expect("valid history snapshot")
}

fn snapshot(instance_byte: u8, profile_id: &str, source: &str) -> ProfileWorkspaceSnapshot {
    ProfileWorkspaceSnapshot::new(
        ProfileInstanceId::from_bytes([instance_byte; 16]),
        ProfileId(profile_id.to_owned()),
        true,
        vec![
            editor(1, "Orders", "SELECT 1"),
            editor(2, "Private", source),
        ],
        Some(2),
        WorkspaceGeometrySnapshot::new(312.0, 0.62, true).expect("valid geometry"),
        vec![history(41, source)],
    )
    .expect("valid profile workspace")
}

fn sized_snapshot(
    instance_byte: u8,
    editor_tabs: usize,
    history_entries: usize,
    first_timestamp: i64,
) -> ProfileWorkspaceSnapshot {
    sized_snapshot_with_status(
        instance_byte,
        editor_tabs,
        history_entries,
        first_timestamp,
        WorkspaceHistoryStatus::Succeeded,
    )
}

fn sized_snapshot_with_status(
    instance_byte: u8,
    editor_tabs: usize,
    history_entries: usize,
    first_timestamp: i64,
    status: WorkspaceHistoryStatus,
) -> ProfileWorkspaceSnapshot {
    let editors = (0..editor_tabs)
        .map(|index| editor(index as u64 + 1, "Bounded", "SELECT bounded"))
        .collect::<Vec<_>>();
    let history = (0..history_entries)
        .map(|index| {
            WorkspaceHistoryEntry::new(
                index as u64 + 1,
                "SELECT bounded_history",
                WorkspaceRunTarget::Current,
                first_timestamp + index as i64,
                status,
                1,
                1,
                0,
                false,
            )
            .expect("bounded history entry")
        })
        .collect::<Vec<_>>();
    ProfileWorkspaceSnapshot::new(
        ProfileInstanceId::from_bytes([instance_byte; 16]),
        ProfileId(format!("bounded-{instance_byte:02x}")),
        true,
        editors,
        (editor_tabs > 0).then_some(1),
        WorkspaceGeometrySnapshot::new(300.0, 0.6, true).expect("bounded geometry"),
        history,
    )
    .expect("bounded profile workspace")
}

fn retained_files(root: &Path) -> Vec<PathBuf> {
    fn walk(path: &Path, files: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(path).expect("read private workspace directory") {
            let entry = entry.expect("workspace entry");
            let file_type = entry.file_type().expect("workspace entry type");
            if file_type.is_dir() {
                walk(&entry.path(), files);
            } else if file_type.is_file() {
                files.push(entry.path());
            } else {
                panic!("workspace store must not retain symlinks or special files");
            }
        }
    }

    let mut files = Vec::new();
    walk(root, &mut files);
    files
}

fn assert_private_tree(root: &Path) {
    fn walk(path: &Path) {
        let metadata = fs::symlink_metadata(path).expect("workspace metadata");
        assert!(!metadata.file_type().is_symlink());
        if metadata.is_dir() {
            assert_eq!(metadata.permissions().mode() & 0o7777, 0o700);
            for entry in fs::read_dir(path).expect("private directory") {
                walk(&entry.expect("private entry").path());
            }
        } else {
            assert!(metadata.is_file());
            assert_eq!(metadata.permissions().mode() & 0o7777, 0o600);
        }
    }

    walk(root);
}

fn create_private_directory(path: &Path) {
    fs::create_dir(path).expect("create replacement private directory");
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .expect("set replacement private directory permissions");
}

fn quarantine_entry_name(instance_id: ProfileInstanceId, nonce_byte: u8) -> String {
    let scope = lower_hex(&Sha256::digest(instance_id.as_bytes()));
    let nonce = ProfileInstanceId::from_bytes([nonce_byte; 16]);
    format!("q-{scope}-{nonce}.bin")
}

fn assert_commit(commit: &WorkspaceCommit, generation: u64) {
    assert_eq!(commit.generation(), generation);
    assert_eq!(commit.checksum().len(), 64);
    assert!(
        commit
            .checksum()
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    );
}

#[test]
fn private_manifest_round_trip_is_single_writer_and_reopens_exact_saved_state() {
    let temp = tempfile::tempdir().expect("temporary workspace parent");
    let config = temp.path().join("dbotter.toml");
    let root = workspace_root_for_config(&config).expect("workspace sibling path");
    let expected = snapshot(0x11, "daily", PRIVATE_SQL);

    let writer = WorkspaceStore::open(&config).expect("first workspace store");
    assert_eq!(writer.mode(), WorkspaceStoreMode::ReadWrite);
    let second = WorkspaceStore::open(&config).expect("second workspace store");
    assert_eq!(second.mode(), WorkspaceStoreMode::ReadOnly);
    assert_eq!(
        second.read_only_reason(),
        Some(WorkspaceReadOnlyReason::WriterBusy)
    );
    assert!(matches!(
        second.commit(&expected),
        Err(WorkspaceStoreError::ReadOnly)
    ));

    let committed = writer.commit(&expected).expect("durable workspace commit");
    assert_commit(&committed, 1);
    assert_eq!(
        writer
            .load(expected.instance_id())
            .expect("load current generation"),
        Some(expected.clone())
    );
    assert_private_tree(&root);

    let retained = retained_files(&root)
        .into_iter()
        .flat_map(|path| fs::read(path).expect("read private retained file"))
        .collect::<Vec<_>>();
    let retained = String::from_utf8_lossy(&retained);
    assert!(retained.contains(PRIVATE_SQL));
    for forbidden in [RESULT_SENTINEL, BACKEND_SENTINEL, CREDENTIAL_SENTINEL] {
        assert!(!retained.contains(forbidden));
    }

    drop(second);
    drop(writer);
    let reopened = WorkspaceStore::open(&config).expect("reopen after process exit");
    assert_eq!(reopened.mode(), WorkspaceStoreMode::ReadWrite);
    assert_eq!(
        reopened
            .load(expected.instance_id())
            .expect("reopen exact saved generation"),
        Some(expected)
    );
}

#[test]
fn bounds_fail_before_commit_and_history_plus_one_is_metadata_only() {
    let exact_source = "x".repeat(MAX_EDITOR_SOURCE_BYTES);
    assert!(editor(1, "Exact", &exact_source).source().ends_with('x'));
    let oversized_source = "x".repeat(MAX_EDITOR_SOURCE_BYTES + 1);
    assert!(matches!(
        EditorTabSnapshot::new(
            1,
            "Oversized",
            WorkspaceLanguage::Sql,
            &oversized_source,
            None,
            0,
            None,
        ),
        Err(WorkspaceSnapshotError::EditorSourceTooLarge)
    ));

    let history_exact = history(1, &"h".repeat(MAX_HISTORY_SOURCE_BYTES));
    assert!(history_exact.is_reopenable());
    let history_oversized = history(2, &"h".repeat(MAX_HISTORY_SOURCE_BYTES + 1));
    assert!(!history_oversized.is_reopenable());
    assert!(history_oversized.source().is_none());

    let tabs = (0..=MAX_EDITOR_TABS_PER_PROFILE)
        .map(|index| editor(index as u64 + 1, "Bounded", "SELECT 1"))
        .collect::<Vec<_>>();
    assert!(matches!(
        ProfileWorkspaceSnapshot::new(
            ProfileInstanceId::from_bytes([0x22; 16]),
            ProfileId("too-many".to_owned()),
            true,
            tabs,
            Some(1),
            WorkspaceGeometrySnapshot::new(300.0, 0.6, true).expect("geometry"),
            Vec::new(),
        ),
        Err(WorkspaceSnapshotError::TooManyEditorTabs)
    ));
}

#[test]
fn total_tab_and_history_bounds_are_exact_and_oldest_terminal_history_is_evicted_first() {
    let exact_tabs = (0..5)
        .map(|index| {
            sized_snapshot(
                u8::try_from(0x90 + index).expect("tab profile instance"),
                MAX_EDITOR_TABS_PER_PROFILE,
                0,
                0,
            )
        })
        .collect::<Vec<_>>();
    let exact_tab_set = WorkspaceSnapshotSet::new(exact_tabs.clone()).expect("exact total tabs");
    assert_eq!(
        exact_tab_set
            .profiles()
            .iter()
            .map(|profile| profile.editor_tabs().len())
            .sum::<usize>(),
        MAX_EDITOR_TABS_TOTAL
    );
    let mut tabs_plus_one = exact_tabs.clone();
    tabs_plus_one.push(sized_snapshot(0x95, 1, 0, 0));
    assert!(matches!(
        WorkspaceSnapshotSet::new(tabs_plus_one),
        Err(WorkspaceSnapshotError::TooManyEditorTabsTotal)
    ));

    let exact_history = (0..5)
        .map(|index| {
            sized_snapshot(
                u8::try_from(0xa0 + index).expect("history profile instance"),
                0,
                MAX_HISTORY_ENTRIES_PER_PROFILE,
                1_000 + (index as i64 * MAX_HISTORY_ENTRIES_PER_PROFILE as i64),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        WorkspaceSnapshotSet::new(exact_history.clone())
            .expect("exact total history")
            .history_evicted(),
        0
    );
    let mut history_plus_one = exact_history.clone();
    history_plus_one.push(sized_snapshot(0xa5, 0, 1, -1));
    let bounded_history =
        WorkspaceSnapshotSet::new(history_plus_one).expect("history plus one is evicted");
    assert_eq!(bounded_history.history_evicted(), 1);
    assert_eq!(
        bounded_history
            .profiles()
            .iter()
            .map(|profile| profile.history().len())
            .sum::<usize>(),
        MAX_HISTORY_ENTRIES_TOTAL
    );
    assert!(
        bounded_history
            .profiles()
            .iter()
            .find(|profile| profile.instance_id() == ProfileInstanceId::from_bytes([0xa5; 16]))
            .expect("plus-one profile")
            .history()
            .is_empty(),
        "the globally oldest terminal entry is evicted"
    );

    let tabs_temp = tempfile::tempdir().expect("tab bound workspace parent");
    let tabs_config = tabs_temp.path().join("dbotter.toml");
    let tabs_store = WorkspaceStore::open(&tabs_config).expect("tab bound store");
    for profile in &exact_tabs {
        tabs_store.commit(profile).expect("commit exact total tabs");
    }
    assert!(matches!(
        tabs_store.commit(&sized_snapshot(0x95, 1, 0, 0)),
        Err(WorkspaceStoreError::Snapshot(
            WorkspaceSnapshotError::TooManyEditorTabsTotal
        ))
    ));

    let history_temp = tempfile::tempdir().expect("history bound workspace parent");
    let history_config = history_temp.path().join("dbotter.toml");
    let history_store = WorkspaceStore::open(&history_config).expect("history bound store");
    for profile in &exact_history {
        history_store
            .commit(profile)
            .expect("commit exact total history");
    }
    assert!(matches!(
        history_store.commit(&sized_snapshot(0xa5, 0, 1, -1)),
        Err(WorkspaceStoreError::Snapshot(
            WorkspaceSnapshotError::TooManyHistoryEntriesTotal
        ))
    ));
}

#[test]
fn outcome_unknown_history_is_never_evicted_and_unsatisfied_overflow_is_rejected() {
    let mut mixed_profiles = (0..4)
        .map(|index| {
            sized_snapshot(
                u8::try_from(0xb0 + index).expect("mixed history instance"),
                0,
                MAX_HISTORY_ENTRIES_PER_PROFILE,
                10_000 + index as i64 * MAX_HISTORY_ENTRIES_PER_PROFILE as i64,
            )
        })
        .collect::<Vec<_>>();
    mixed_profiles.push(sized_snapshot(0xb4, 0, 1_999, 20_000));
    let protected_instance = ProfileInstanceId::from_bytes([0xb5; 16]);
    let protected = ProfileWorkspaceSnapshot::new(
        protected_instance,
        ProfileId("outcome-unknown-protected".to_owned()),
        true,
        Vec::new(),
        None,
        WorkspaceGeometrySnapshot::new(300.0, 0.6, true).expect("protected geometry"),
        vec![
            WorkspaceHistoryEntry::new(
                1,
                "SELECT outcome_unknown",
                WorkspaceRunTarget::Current,
                -2,
                WorkspaceHistoryStatus::OutcomeUnknown,
                1,
                0,
                0,
                false,
            )
            .expect("outcome unknown history"),
            WorkspaceHistoryEntry::new(
                2,
                "SELECT terminal_failure",
                WorkspaceRunTarget::Current,
                -1,
                WorkspaceHistoryStatus::Failed(WorkspaceHistoryCode::Backend),
                1,
                0,
                0,
                false,
            )
            .expect("terminal failed history"),
        ],
    )
    .expect("protected history profile");
    mixed_profiles.push(protected);

    let bounded = WorkspaceSnapshotSet::new(mixed_profiles).expect("evict terminal overflow");
    assert_eq!(bounded.history_evicted(), 1);
    let retained = bounded
        .profiles()
        .iter()
        .find(|profile| profile.instance_id() == protected_instance)
        .expect("protected profile retained")
        .history();
    assert_eq!(retained.len(), 1);
    assert_eq!(retained[0].status(), WorkspaceHistoryStatus::OutcomeUnknown);

    let mut all_unknown = (0..5)
        .map(|index| {
            sized_snapshot_with_status(
                u8::try_from(0xc0 + index).expect("unknown history instance"),
                0,
                MAX_HISTORY_ENTRIES_PER_PROFILE,
                index as i64 * MAX_HISTORY_ENTRIES_PER_PROFILE as i64,
                WorkspaceHistoryStatus::OutcomeUnknown,
            )
        })
        .collect::<Vec<_>>();
    all_unknown.push(sized_snapshot_with_status(
        0xc5,
        0,
        1,
        -1,
        WorkspaceHistoryStatus::OutcomeUnknown,
    ));
    assert!(matches!(
        WorkspaceSnapshotSet::new(all_unknown),
        Err(WorkspaceSnapshotError::TooManyHistoryEntriesTotal)
    ));
}

#[test]
fn corrupt_current_shard_is_quarantined_without_hiding_another_profile() {
    let temp = tempfile::tempdir().expect("temporary workspace parent");
    let config = temp.path().join("dbotter.toml");
    let root = workspace_root_for_config(&config).expect("workspace sibling path");
    let corrupted = snapshot(0x33, "corrupted", "SELECT corrupt_this_shard");
    let survivor = snapshot(0x44, "survivor", "SELECT survivor_stays_available");
    let store = WorkspaceStore::open(&config).expect("workspace store");
    store.commit(&corrupted).expect("corrupted fixture commit");
    store.commit(&survivor).expect("survivor fixture commit");

    let corrupt_path = retained_files(&root)
        .into_iter()
        .find(|path| {
            fs::read(path).ok().is_some_and(|bytes| {
                bytes
                    .windows(b"corrupt_this_shard".len())
                    .any(|window| window == b"corrupt_this_shard")
            })
        })
        .expect("current shard containing fixture source");
    fs::write(&corrupt_path, b"corrupt current generation").expect("corrupt shard");
    fs::set_permissions(&corrupt_path, fs::Permissions::from_mode(0o600))
        .expect("retain private corrupt permissions");

    let error = store
        .load(corrupted.instance_id())
        .expect_err("checksum mismatch must fail closed");
    assert!(matches!(error, WorkspaceStoreError::CorruptShard));
    let debug = format!("{error:?}");
    assert!(!debug.contains("corrupt_this_shard"));
    assert!(!debug.contains(temp.path().to_string_lossy().as_ref()));
    assert_eq!(
        store
            .load(survivor.instance_id())
            .expect("unrelated profile remains available"),
        Some(survivor)
    );
    assert!(
        retained_files(&root).iter().any(|path| path
            .components()
            .any(|part| part.as_os_str() == "quarantine")),
        "the corrupt bounded shard must move into the private quarantine"
    );
}

#[test]
fn corrupt_profile_is_quarantined_with_a_safe_warning_without_blocking_another_commit() {
    let temp = tempfile::tempdir().expect("temporary workspace parent");
    let config = temp.path().join("dbotter.toml");
    let root = workspace_root_for_config(&config).expect("workspace sibling path");
    let corrupted = snapshot(0x35, "corrupted-before-save", "SELECT private_corrupt_a");
    let survivor = snapshot(0x45, "saved-after-corruption", "SELECT private_survivor_b");
    let store = WorkspaceStore::open(&config).expect("workspace store");
    store.commit(&corrupted).expect("corrupted fixture commit");

    let (_, _, shard_path, _) = current_generation_paths(&root, corrupted.instance_id());
    let mut bytes = fs::read(&shard_path).expect("read current shard");
    bytes[0] ^= 1;
    fs::write(&shard_path, bytes).expect("corrupt current shard");
    fs::set_permissions(&shard_path, fs::Permissions::from_mode(0o600))
        .expect("retain private corrupt permissions");

    let commit = store
        .commit(&survivor)
        .expect("unrelated profile commit continues");
    assert_eq!(
        commit.warnings(),
        &[WorkspaceStoreWarning::CorruptProfileQuarantined]
    );
    let debug = format!("{commit:?}");
    assert!(!debug.contains("private_corrupt_a"));
    assert!(!debug.contains(temp.path().to_string_lossy().as_ref()));
    assert_eq!(
        store
            .load(survivor.instance_id())
            .expect("saved survivor remains loadable"),
        Some(survivor)
    );
    assert!(matches!(
        store.load(corrupted.instance_id()),
        Err(WorkspaceStoreError::CorruptShard)
    ));
}

#[test]
fn corrupt_and_unsupported_profiles_remain_typed_after_reopen_until_clear() {
    let temp = tempfile::tempdir().expect("temporary workspace parent");
    let config = temp.path().join("dbotter.toml");
    let root = workspace_root_for_config(&config).expect("workspace sibling path");
    let corrupt = snapshot(0x36, "corrupt-marker", "SELECT corrupt_marker_source");
    let unsupported = snapshot(
        0x46,
        "unsupported-marker",
        "SELECT unsupported_marker_source",
    );
    let store = WorkspaceStore::open(&config).expect("workspace store");
    store.commit(&corrupt).expect("corrupt fixture commit");
    store
        .commit(&unsupported)
        .expect("unsupported fixture commit");

    let (corrupt_directory, _, corrupt_shard, _) =
        current_generation_paths(&root, corrupt.instance_id());
    let mut corrupt_bytes = fs::read(&corrupt_shard).expect("read corrupt fixture shard");
    corrupt_bytes[0] ^= 1;
    fs::write(&corrupt_shard, corrupt_bytes).expect("corrupt fixture shard");
    fs::set_permissions(&corrupt_shard, fs::Permissions::from_mode(0o600))
        .expect("private corrupt fixture shard");

    let (unsupported_directory, unsupported_manifest_path, _, mut manifest) =
        current_generation_paths(&root, unsupported.instance_id());
    manifest["schema"] = serde_json::Value::String("dbotter.workspace-manifest.v999".to_owned());
    fs::write(
        &unsupported_manifest_path,
        serde_json::to_vec(&manifest).expect("encode unsupported manifest"),
    )
    .expect("write unsupported manifest");
    fs::set_permissions(
        &unsupported_manifest_path,
        fs::Permissions::from_mode(0o600),
    )
    .expect("private unsupported manifest");

    for _ in 0..2 {
        assert!(matches!(
            store.load(corrupt.instance_id()),
            Err(WorkspaceStoreError::CorruptShard)
        ));
        assert!(matches!(
            store.load(unsupported.instance_id()),
            Err(WorkspaceStoreError::UnsupportedVersion)
        ));
    }
    assert!(corrupt_directory.join("corrupt.state").is_file());
    assert!(unsupported_directory.join("corrupt.state").is_file());

    drop(store);
    let reopened = WorkspaceStore::open(&config).expect("reopen workspace store");
    assert!(matches!(
        reopened.load(corrupt.instance_id()),
        Err(WorkspaceStoreError::CorruptShard)
    ));
    assert!(matches!(
        reopened.load(unsupported.instance_id()),
        Err(WorkspaceStoreError::UnsupportedVersion)
    ));

    reopened
        .clear(corrupt.instance_id())
        .expect("clear corrupt profile");
    reopened
        .clear(unsupported.instance_id())
        .expect("clear unsupported profile");
    assert_eq!(
        reopened
            .load(corrupt.instance_id())
            .expect("load cleared corrupt profile"),
        None
    );
    assert_eq!(
        reopened
            .load(unsupported.instance_id())
            .expect("load cleared unsupported profile"),
        None
    );
}

#[test]
fn managed_symlinks_fail_closed_and_public_errors_redact_paths_and_payloads() {
    let temp = tempfile::tempdir().expect("temporary workspace parent");
    let config = temp.path().join("dbotter.toml");
    let root = workspace_root_for_config(&config).expect("workspace sibling path");
    let outside = temp.path().join("outside");
    fs::create_dir(&outside).expect("outside directory");
    fs::set_permissions(&outside, fs::Permissions::from_mode(0o700))
        .expect("private outside directory");

    symlink(&outside, &root).expect("workspace root symlink fixture");
    let root_error = WorkspaceStore::open(&config).expect_err("root symlink must fail closed");
    assert!(matches!(root_error, WorkspaceStoreError::UnsafePath));
    assert_error_redacted(&root_error, temp.path());
    fs::remove_file(&root).expect("remove root symlink fixture");

    let writer = WorkspaceStore::open(&config).expect("create private workspace tree");
    drop(writer);
    let outside_file = outside.join("sentinel-file");
    fs::write(
        &outside_file,
        format!("{PRIVATE_SQL}{RESULT_SENTINEL}{BACKEND_SENTINEL}{CREDENTIAL_SENTINEL}"),
    )
    .expect("outside sentinel file");
    fs::set_permissions(&outside_file, fs::Permissions::from_mode(0o600))
        .expect("private outside file");

    let lock = root.join("writer.lock");
    fs::remove_file(&lock).expect("remove managed lock");
    symlink(&outside_file, &lock).expect("lock symlink fixture");
    let lock_error = WorkspaceStore::open(&config).expect_err("lock symlink must fail closed");
    assert!(matches!(lock_error, WorkspaceStoreError::UnsafePath));
    assert_error_redacted(&lock_error, temp.path());
    fs::remove_file(&lock).expect("remove lock symlink fixture");

    let store = WorkspaceStore::open(&config).expect("reopen safe workspace tree");
    let expected = snapshot(0x55, "symlink-safe", PRIVATE_SQL);
    let profile_directory = root
        .join("profiles")
        .join(expected.instance_id().to_string());
    symlink(&outside, &profile_directory).expect("profile directory symlink fixture");
    let profile_error = store
        .commit(&expected)
        .expect_err("profile directory symlink must fail closed");
    assert!(matches!(profile_error, WorkspaceStoreError::UnsafePath));
    assert_error_redacted(&profile_error, temp.path());
    fs::remove_file(&profile_directory).expect("remove profile directory symlink fixture");

    store.commit(&expected).expect("safe profile commit");
    let manifest = profile_directory.join("manifest.json");
    fs::remove_file(&manifest).expect("remove managed manifest");
    symlink(&outside_file, &manifest).expect("manifest symlink fixture");
    let manifest_error = store
        .load(expected.instance_id())
        .expect_err("manifest symlink must fail closed");
    assert!(matches!(manifest_error, WorkspaceStoreError::UnsafePath));
    assert_error_redacted(&manifest_error, temp.path());
}

#[test]
fn unsafe_profile_entry_is_isolated_without_blocking_healthy_commit_and_clear_is_exact() {
    let temp = tempfile::tempdir().expect("unsafe profile entry parent");
    let config = temp.path().join("dbotter.toml");
    let root = workspace_root_for_config(&config).expect("unsafe profile entry root");
    let store = WorkspaceStore::open(&config).expect("workspace store");
    let unsafe_instance = ProfileInstanceId::from_bytes([0x57; 16]);
    let unsafe_entry = root.join("profiles").join(unsafe_instance.to_string());
    let outside = temp.path().join("outside-profile-target");
    create_private_directory(&outside);
    let outside_file = outside.join("outside-private-source");
    fs::write(&outside_file, CREDENTIAL_SENTINEL).expect("outside profile sentinel");
    fs::set_permissions(&outside_file, fs::Permissions::from_mode(0o600))
        .expect("private outside profile sentinel");
    symlink(&outside, &unsafe_entry).expect("unsafe profile directory symlink");

    let healthy = snapshot(0x58, "healthy-after-unsafe", "SELECT healthy_after_unsafe");
    let commit = store
        .commit(&healthy)
        .expect("unsafe profile does not block healthy commit");
    assert_eq!(
        commit.warnings(),
        &[WorkspaceStoreWarning::CorruptProfileQuarantined]
    );
    assert!(!unsafe_entry.exists());
    assert_eq!(
        fs::read_to_string(&outside_file).expect("outside profile target survives isolation"),
        CREDENTIAL_SENTINEL
    );
    assert!(matches!(
        store.load(unsafe_instance),
        Err(WorkspaceStoreError::UnsafePath)
    ));

    store
        .clear(unsafe_instance)
        .expect("clear isolated unsafe profile");
    assert_eq!(
        store
            .load(unsafe_instance)
            .expect("cleared unsafe profile is absent"),
        None
    );
    assert_eq!(
        fs::read_to_string(&outside_file).expect("outside target survives exact clear"),
        CREDENTIAL_SENTINEL
    );
}

#[test]
fn ordinary_reopen_never_applies_a_stale_profile_entry_marker_to_a_valid_replacement() {
    if std::env::var_os("DBOTTER_J2_STALE_PROFILE_MARKER_CONFIG").is_some() {
        return;
    }

    let temp = tempfile::tempdir().expect("stale profile marker parent");
    let config = temp.path().join("dbotter.toml");
    let root = workspace_root_for_config(&config).expect("stale profile marker root");
    let store = WorkspaceStore::open(&config).expect("stale profile marker store");
    let replacement = snapshot(
        0x67,
        "stale-marker-replacement",
        "SELECT valid_replacement_must_survive_reopen",
    );
    let survivor = snapshot(
        0x68,
        "stale-marker-survivor",
        "SELECT other_profile_must_survive_reopen",
    );
    store
        .commit(&replacement)
        .expect("commit replacement workspace fixture");
    let profiles = root.join("profiles");
    let canonical = profiles.join(replacement.instance_id().to_string());
    let staged_replacement = temp.path().join("staged-valid-replacement");
    fs::rename(&canonical, &staged_replacement).expect("stage valid replacement workspace");

    let outside = temp.path().join("outside-stale-marker-target");
    create_private_directory(&outside);
    let outside_file = outside.join("outside-private-source");
    fs::write(&outside_file, CREDENTIAL_SENTINEL).expect("outside stale marker sentinel");
    fs::set_permissions(&outside_file, fs::Permissions::from_mode(0o600))
        .expect("private outside stale marker sentinel");
    symlink(&outside, &canonical).expect("unsafe profile entry for durable marker");
    let commit = store
        .commit(&survivor)
        .expect("isolate unsafe entry and retain another profile");
    assert_eq!(
        commit.warnings(),
        &[WorkspaceStoreWarning::CorruptProfileQuarantined]
    );
    assert!(!canonical.exists());
    fs::rename(&staged_replacement, &canonical)
        .expect("restore valid replacement at canonical path");
    fs::File::open(&profiles)
        .expect("open profiles for stale marker durability")
        .sync_all()
        .expect("durably restore valid replacement");
    drop(store);

    let output = Command::new(std::env::current_exe().expect("current test executable"))
        .arg("--exact")
        .arg("workspace_store_subprocess_reopens_with_stale_profile_entry_marker")
        .arg("--nocapture")
        .env("DBOTTER_J2_STALE_PROFILE_MARKER_CONFIG", &config)
        .output()
        .expect("spawn ordinary stale marker reopen");
    assert!(
        output.status.success(),
        "ordinary reopen helper failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(canonical.is_dir(), "stale marker deleted valid replacement");
    let replacement_bytes = retained_files(&canonical)
        .into_iter()
        .flat_map(|path| fs::read(path).expect("read retained valid replacement"))
        .collect::<Vec<_>>();
    assert!(
        String::from_utf8_lossy(&replacement_bytes)
            .contains("valid_replacement_must_survive_reopen")
    );
    assert_eq!(
        fs::read_to_string(&outside_file).expect("outside stale marker target survives"),
        CREDENTIAL_SENTINEL
    );
    let recovery = WorkspaceStore::open(&config).expect("reopen stale marker for exact retirement");
    recovery
        .clear(replacement.instance_id())
        .expect("retire stale marker without clearing replacement");
    assert!(canonical.is_dir());
    assert_eq!(
        recovery
            .load(replacement.instance_id())
            .expect("replacement enters normal validation after stale marker retirement"),
        Some(replacement)
    );
}

#[test]
fn workspace_store_subprocess_reopens_with_stale_profile_entry_marker() {
    let Some(config) = std::env::var_os("DBOTTER_J2_STALE_PROFILE_MARKER_CONFIG") else {
        return;
    };
    let store = WorkspaceStore::open(Path::new(&config)).expect("ordinary stale marker reopen");
    let survivor = snapshot(
        0x68,
        "stale-marker-survivor",
        "SELECT other_profile_must_survive_reopen",
    );
    assert_eq!(
        store
            .load(survivor.instance_id())
            .expect("load other profile after stale marker reopen"),
        Some(survivor)
    );
    assert!(matches!(
        store.load(ProfileInstanceId::from_bytes([0x67; 16])),
        Err(WorkspaceStoreError::UnsafePath)
    ));
    let after_reopen = snapshot(
        0x6d,
        "stale-marker-after-reopen",
        "SELECT unrelated_commit_after_stale_marker",
    );
    let commit = store
        .commit(&after_reopen)
        .expect("unrelated commit continues after stale marker reopen");
    assert_eq!(
        commit.warnings(),
        &[WorkspaceStoreWarning::CorruptProfileQuarantined]
    );
    assert_eq!(
        store
            .load(after_reopen.instance_id())
            .expect("load unrelated commit after stale marker reopen"),
        Some(after_reopen)
    );
}

#[test]
fn internal_state_marker_binds_both_the_profile_and_corrupt_state_entry_across_reopen() {
    for (replace_profile, target_byte, survivor_byte) in [(false, 0x69, 0x6a), (true, 0x6b, 0x6c)] {
        let temp = tempfile::tempdir().expect("internal state binding parent");
        let config = temp.path().join("dbotter.toml");
        let root = workspace_root_for_config(&config).expect("internal state binding root");
        let store = WorkspaceStore::open(&config).expect("internal state binding store");
        let target = snapshot(
            target_byte,
            "internal-state-original",
            "SELECT internal_state_original",
        );
        let survivor = snapshot(
            survivor_byte,
            "internal-state-survivor",
            "SELECT internal_state_other_profile",
        );
        store.commit(&target).expect("commit internal state target");
        store
            .commit(&survivor)
            .expect("commit internal state survivor");
        let profiles = root.join("profiles");
        let profile = profiles.join(target.instance_id().to_string());
        let state_path = profile.join("corrupt.state");
        fs::write(&state_path, b"malformed-state").expect("write malformed internal state");
        fs::set_permissions(&state_path, fs::Permissions::from_mode(0o600))
            .expect("private malformed internal state");
        assert!(matches!(
            store.load(target.instance_id()),
            Err(WorkspaceStoreError::CorruptManifest)
        ));
        assert!(!state_path.exists());

        let marker = profiles.join(format!(".corrupt-{}.state", target.instance_id()));
        let marker_bytes = fs::read(&marker).expect("read bound internal state marker");
        assert!(marker_bytes.starts_with(b"internal-state-v2:"));
        assert!(marker_bytes.len() > 64);
        assert!(marker_bytes.len() <= 256);

        if replace_profile {
            let displaced = temp.path().join("displaced-internal-state-original");
            fs::rename(&profile, &displaced).expect("displace bound internal state profile");
            let replacement_config = temp.path().join("replacement.toml");
            let replacement_store =
                WorkspaceStore::open(&replacement_config).expect("replacement workspace store");
            let replacement = snapshot(
                target_byte,
                "internal-state-replacement",
                "SELECT internal_state_replacement_must_survive",
            );
            replacement_store
                .commit(&replacement)
                .expect("commit internal state replacement");
            let replacement_root = workspace_root_for_config(&replacement_config)
                .expect("internal state replacement root");
            let replacement_profile = replacement_root
                .join("profiles")
                .join(target.instance_id().to_string());
            drop(replacement_store);
            fs::rename(&replacement_profile, &profile)
                .expect("restore different profile identity at canonical path");
        }

        let replacement_state = b"corrupt-shard-v1\n";
        fs::write(&state_path, replacement_state).expect("write replacement internal state");
        fs::set_permissions(&state_path, fs::Permissions::from_mode(0o600))
            .expect("private replacement internal state");
        drop(store);

        let reopened = WorkspaceStore::open(&config).expect("reopen bound internal state store");
        assert_eq!(
            fs::read(&state_path).expect("replacement internal state survives reopen"),
            replacement_state
        );
        assert_eq!(
            reopened
                .load(survivor.instance_id())
                .expect("unrelated profile survives internal marker reopen"),
            Some(survivor)
        );
        assert!(matches!(
            reopened.load(target.instance_id()),
            Err(WorkspaceStoreError::CorruptManifest)
        ));
        if replace_profile {
            let replacement_bytes = retained_files(&profile)
                .into_iter()
                .flat_map(|path| fs::read(path).expect("read retained replacement profile"))
                .collect::<Vec<_>>();
            assert!(
                String::from_utf8_lossy(&replacement_bytes)
                    .contains("internal_state_replacement_must_survive")
            );
        }
    }
}

#[test]
fn root_marked_clear_binds_entry_type_and_never_follows_outside_targets() {
    for (case, instance_byte) in [("symlink", 0x64), ("file", 0x65), ("directory", 0x66)] {
        let temp = tempfile::tempdir().expect("root marked clear parent");
        let config = temp.path().join("dbotter.toml");
        let root = workspace_root_for_config(&config).expect("root marked clear root");
        let store = WorkspaceStore::open(&config).expect("root marked clear store");
        let instance_id = ProfileInstanceId::from_bytes([instance_byte; 16]);
        let profiles = root.join("profiles");
        let profile_entry = profiles.join(instance_id.to_string());
        let outside_directory = temp.path().join("outside-root-marked-clear");
        create_private_directory(&outside_directory);
        let outside_file = outside_directory.join("outside-private-source");
        fs::write(&outside_file, CREDENTIAL_SENTINEL).expect("outside root marked sentinel");
        fs::set_permissions(&outside_file, fs::Permissions::from_mode(0o600))
            .expect("private outside root marked sentinel");

        match case {
            "symlink" => {
                symlink(&outside_directory, &profile_entry).expect("root marked profile symlink");
            }
            "file" => {
                fs::hard_link(&outside_file, &profile_entry)
                    .expect("root marked profile hard link");
            }
            "directory" => {
                create_private_directory(&profile_entry);
                symlink(&outside_file, profile_entry.join("outside-link"))
                    .expect("root marked nested outside link");
            }
            _ => unreachable!("frozen root marked clear fixture"),
        }

        let marker = profiles.join(format!(".corrupt-{instance_id}.state"));
        let entry_metadata =
            fs::symlink_metadata(&profile_entry).expect("root marked entry identity");
        let entry_kind = match case {
            "symlink" => "l",
            "file" => "f",
            "directory" => "d",
            _ => unreachable!("frozen root marked marker fixture"),
        };
        fs::write(
            &marker,
            format!(
                "profile-entry-v2:unsafe:{entry_kind}:{:016x}:{:016x}\n",
                entry_metadata.dev(),
                entry_metadata.ino()
            ),
        )
        .expect("root marked clear marker");
        fs::set_permissions(&marker, fs::Permissions::from_mode(0o600))
            .expect("private root marked clear marker");

        store
            .clear(instance_id)
            .expect("clear exact root marked entry");
        assert!(!profile_entry.exists());
        assert!(!marker.exists());
        assert_eq!(
            fs::read_to_string(&outside_file).expect("outside root marked target survives"),
            CREDENTIAL_SENTINEL
        );
        assert_eq!(
            store
                .load(instance_id)
                .expect("root marked profile is cleared"),
            None
        );
    }
}

#[test]
fn truncated_root_corruption_marker_is_atomically_repaired_and_temp_is_recovered() {
    let temp = tempfile::tempdir().expect("truncated root marker parent");
    let config = temp.path().join("dbotter.toml");
    let root = workspace_root_for_config(&config).expect("truncated root marker root");
    let store = WorkspaceStore::open(&config).expect("workspace store");
    let corrupted_instance = ProfileInstanceId::from_bytes([0x60; 16]);
    let unsafe_entry = root.join("profiles").join(corrupted_instance.to_string());
    let outside = temp.path().join("outside-root-marker-target");
    create_private_directory(&outside);
    symlink(&outside, &unsafe_entry).expect("unsafe root marker fixture");
    let first_healthy = snapshot(0x61, "root-marker-first", "SELECT root_marker_first");
    store
        .commit(&first_healthy)
        .expect("create durable root marker");
    drop(store);

    let marker_path = root
        .join("profiles")
        .join(format!(".corrupt-{corrupted_instance}.state"));
    fs::write(&marker_path, b"").expect("truncate authoritative root marker");
    fs::set_permissions(&marker_path, fs::Permissions::from_mode(0o600))
        .expect("private truncated root marker");
    let temp_name = format!(
        ".dbotter-workspace.profile-corrupt-marker.tmp.{}",
        ProfileInstanceId::from_bytes([0x62; 16])
    );
    let abandoned_temp = root.join("profiles").join(temp_name);
    fs::write(&abandoned_temp, b"abandoned-private-marker-temp").expect("abandoned marker temp");
    fs::set_permissions(&abandoned_temp, fs::Permissions::from_mode(0o600))
        .expect("private abandoned marker temp");

    let reopened = WorkspaceStore::open(&config).expect("repair truncated root marker");
    assert!(!abandoned_temp.exists());
    assert!(
        !fs::read(&marker_path)
            .expect("read repaired root marker")
            .is_empty()
    );
    let second_healthy = snapshot(0x63, "root-marker-second", "SELECT root_marker_second");
    reopened
        .commit(&second_healthy)
        .expect("repaired marker does not block unrelated commit");
    assert!(matches!(
        reopened.load(corrupted_instance),
        Err(WorkspaceStoreError::UnsafePath)
    ));
    reopened
        .clear(corrupted_instance)
        .expect("clear repaired root marker");
    assert!(!marker_path.exists());
}

#[test]
fn malformed_oversized_and_symlinked_corrupt_state_are_isolated_and_clearable() {
    for (case, instance_byte, healthy_byte) in [
        ("malformed", 0x59, 0x5a),
        ("oversized", 0x5b, 0x5c),
        ("symlink", 0x5d, 0x5e),
    ] {
        let temp = tempfile::tempdir().expect("unsafe corrupt state parent");
        let config = temp.path().join("dbotter.toml");
        let root = workspace_root_for_config(&config).expect("unsafe corrupt state root");
        let store = WorkspaceStore::open(&config).expect("workspace store");
        let corrupted = snapshot(
            instance_byte,
            "unsafe-corrupt-state",
            "SELECT unsafe_corrupt_state",
        );
        store
            .commit(&corrupted)
            .expect("corrupt state fixture commit");
        let state_path = root
            .join("profiles")
            .join(corrupted.instance_id().to_string())
            .join("corrupt.state");
        let outside = temp.path().join("outside-state-target");
        fs::write(&outside, CREDENTIAL_SENTINEL).expect("outside state target");
        fs::set_permissions(&outside, fs::Permissions::from_mode(0o600))
            .expect("private outside state target");
        match case {
            "malformed" => {
                fs::write(&state_path, b"malformed-state").expect("malformed corrupt state");
                fs::set_permissions(&state_path, fs::Permissions::from_mode(0o600))
                    .expect("private malformed corrupt state");
            }
            "oversized" => {
                fs::write(&state_path, vec![b'x'; 65]).expect("oversized corrupt state");
                fs::set_permissions(&state_path, fs::Permissions::from_mode(0o600))
                    .expect("private oversized corrupt state");
            }
            "symlink" => {
                symlink(&outside, &state_path).expect("symlinked corrupt state");
            }
            _ => unreachable!("closed corrupt-state fixture matrix"),
        }

        let healthy = snapshot(
            healthy_byte,
            "healthy-after-state",
            "SELECT healthy_after_state",
        );
        let commit = store
            .commit(&healthy)
            .expect("unsafe state does not block healthy commit");
        assert_eq!(
            commit.warnings(),
            &[WorkspaceStoreWarning::CorruptProfileQuarantined]
        );
        let load = store.load(corrupted.instance_id());
        if case == "symlink" {
            assert!(matches!(load, Err(WorkspaceStoreError::UnsafePath)));
        } else {
            assert!(matches!(load, Err(WorkspaceStoreError::CorruptManifest)));
        }
        store
            .clear(corrupted.instance_id())
            .expect("clear root-marked corrupt state");
        assert_eq!(
            store
                .load(corrupted.instance_id())
                .expect("cleared corrupt state profile"),
            None
        );
        assert_eq!(
            fs::read_to_string(&outside).expect("outside state target survives"),
            CREDENTIAL_SENTINEL
        );
    }
}

#[test]
fn nested_managed_corruption_and_recursive_clear_never_follow_outside_links() {
    let temp = tempfile::tempdir().expect("nested corruption parent");
    let config = temp.path().join("dbotter.toml");
    let root = workspace_root_for_config(&config).expect("nested corruption root");
    let store = WorkspaceStore::open(&config).expect("workspace store");
    let expected = snapshot(0x5f, "nested-corruption", "SELECT nested_corruption");
    store.commit(&expected).expect("nested fixture commit");
    let profile = root
        .join("profiles")
        .join(expected.instance_id().to_string());
    let manifest = profile.join("manifest.json");
    fs::remove_file(&manifest).expect("remove manifest fixture");
    create_private_directory(&manifest);
    let outside = temp.path().join("outside-nested-target");
    fs::write(&outside, CREDENTIAL_SENTINEL).expect("outside nested target");
    fs::set_permissions(&outside, fs::Permissions::from_mode(0o600))
        .expect("private outside nested target");
    symlink(&outside, manifest.join("outside-link")).expect("nested outside symlink");

    assert!(matches!(
        store.load(expected.instance_id()),
        Err(WorkspaceStoreError::UnsafePath)
    ));
    let nested_again = profile.join("nested-before-clear");
    create_private_directory(&nested_again);
    symlink(&outside, nested_again.join("outside-link")).expect("clear nested outside symlink");
    store
        .clear(expected.instance_id())
        .expect("recursive exact profile clear");
    assert_eq!(
        store
            .load(expected.instance_id())
            .expect("nested corrupt profile cleared"),
        None
    );
    assert_eq!(
        fs::read_to_string(&outside).expect("outside nested target survives"),
        CREDENTIAL_SENTINEL
    );
}

#[test]
fn concurrent_commits_are_serialized_into_monotonic_generations() {
    let temp = tempfile::tempdir().expect("temporary workspace parent");
    let config = temp.path().join("dbotter.toml");
    let root = workspace_root_for_config(&config).expect("workspace sibling path");
    let store = Arc::new(WorkspaceStore::open(&config).expect("workspace store"));
    let barrier = Arc::new(Barrier::new(3));
    let first = snapshot(0x66, "concurrent", "SELECT first_serialized_commit");
    let second = snapshot(0x66, "concurrent", "SELECT second_serialized_commit");

    let first_thread = {
        let barrier = Arc::clone(&barrier);
        let store = Arc::clone(&store);
        let snapshot = first.clone();
        thread::spawn(move || {
            barrier.wait();
            store.commit(&snapshot).expect("first concurrent commit")
        })
    };
    let second_thread = {
        let barrier = Arc::clone(&barrier);
        let store = Arc::clone(&store);
        let snapshot = second.clone();
        thread::spawn(move || {
            barrier.wait();
            store.commit(&snapshot).expect("second concurrent commit")
        })
    };
    barrier.wait();

    let mut generations = [
        first_thread
            .join()
            .expect("first commit thread")
            .generation(),
        second_thread
            .join()
            .expect("second commit thread")
            .generation(),
    ];
    generations.sort_unstable();
    assert_eq!(generations, [1, 2]);
    let loaded = store
        .load(first.instance_id())
        .expect("load serialized current generation")
        .expect("current generation exists");
    assert!(loaded == first || loaded == second);

    let profile_directory = root.join("profiles").join(first.instance_id().to_string());
    let shard_count = fs::read_dir(profile_directory)
        .expect("read profile directory")
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with("shard-") && name.ends_with(".json"))
        })
        .count();
    assert_eq!(
        shard_count, 1,
        "only the manifest-referenced shard is retained"
    );
}

#[test]
fn exact_profile_clear_is_idempotent_and_read_only_observer_cannot_clear() {
    let temp = tempfile::tempdir().expect("temporary workspace parent");
    let config = temp.path().join("dbotter.toml");
    let first = snapshot(0x71, "clear-first", "SELECT clear_only_this_profile");
    let survivor = snapshot(0x72, "clear-survivor", "SELECT keep_this_profile");
    let writer = WorkspaceStore::open(&config).expect("workspace writer");
    writer.commit(&first).expect("first profile commit");
    writer.commit(&survivor).expect("survivor profile commit");
    let observer = WorkspaceStore::open(&config).expect("read-only observer");

    assert!(matches!(
        observer.clear(first.instance_id()),
        Err(WorkspaceStoreError::ReadOnly)
    ));
    writer
        .clear(first.instance_id())
        .expect("clear exact profile");
    writer
        .clear(first.instance_id())
        .expect("repeat clear is idempotent");
    assert_eq!(
        writer
            .load(first.instance_id())
            .expect("load cleared profile"),
        None
    );
    assert_eq!(
        writer
            .load(survivor.instance_id())
            .expect("load surviving profile"),
        Some(survivor)
    );
}

#[test]
fn clear_removes_only_the_exact_profiles_quarantined_private_payload() {
    let temp = tempfile::tempdir().expect("temporary workspace parent");
    let config = temp.path().join("dbotter.toml");
    let root = workspace_root_for_config(&config).expect("workspace sibling path");
    let store = WorkspaceStore::open(&config).expect("workspace store");
    let cleared_source = "SELECT quarantined_private_source_to_clear";
    let survivor_source = "SELECT quarantined_private_source_to_keep";
    let cleared = snapshot(0x75, "quarantine-clear", cleared_source);
    let survivor = snapshot(0x76, "quarantine-survivor", survivor_source);

    for expected in [&cleared, &survivor] {
        store.commit(expected).expect("workspace commit");
        let (_, _, shard_path, _) = current_generation_paths(&root, expected.instance_id());
        let mut bytes = fs::read(&shard_path).expect("read current shard");
        bytes[0] ^= 1;
        fs::write(&shard_path, bytes).expect("corrupt shard without removing private source");
        fs::set_permissions(&shard_path, fs::Permissions::from_mode(0o600))
            .expect("private corrupt shard");
        assert!(matches!(
            store.load(expected.instance_id()),
            Err(WorkspaceStoreError::CorruptShard)
        ));
    }

    let quarantine = root.join("quarantine");
    let before = retained_files(&quarantine)
        .into_iter()
        .flat_map(|path| fs::read(path).expect("read quarantined file"))
        .collect::<Vec<_>>();
    let before = String::from_utf8_lossy(&before);
    assert!(before.contains(cleared_source));
    assert!(before.contains(survivor_source));

    store
        .clear(cleared.instance_id())
        .expect("clear exact quarantined profile payload");
    let after = retained_files(&quarantine)
        .into_iter()
        .flat_map(|path| fs::read(path).expect("read remaining quarantine"))
        .collect::<Vec<_>>();
    let after = String::from_utf8_lossy(&after);
    assert!(!after.contains(cleared_source));
    assert!(after.contains(survivor_source));
}

#[test]
fn startup_finishes_tombstoned_profile_and_exact_quarantine_purge_after_clear_crash() {
    let temp = tempfile::tempdir().expect("temporary workspace parent");
    let config = temp.path().join("dbotter.toml");
    let root = workspace_root_for_config(&config).expect("workspace sibling path");
    let cleared_source = "SELECT crash_recovered_quarantine_clear";
    let survivor_source = "SELECT crash_recovered_quarantine_survivor";
    let cleared = snapshot(0x77, "clear-crash", cleared_source);
    let survivor = snapshot(0x78, "clear-crash-survivor", survivor_source);
    let store = WorkspaceStore::open(&config).expect("workspace store");
    store.commit(&cleared).expect("cleared fixture commit");
    store.commit(&survivor).expect("survivor fixture commit");

    for expected in [&cleared, &survivor] {
        let (_, _, shard_path, _) = current_generation_paths(&root, expected.instance_id());
        let mut bytes = fs::read(&shard_path).expect("read fixture shard");
        bytes[0] ^= 1;
        fs::write(&shard_path, bytes).expect("corrupt fixture shard");
        fs::set_permissions(&shard_path, fs::Permissions::from_mode(0o600))
            .expect("private corrupt fixture");
        assert!(matches!(
            store.load(expected.instance_id()),
            Err(WorkspaceStoreError::CorruptShard)
        ));
    }
    drop(store);

    let profile_directory = root
        .join("profiles")
        .join(cleared.instance_id().to_string());
    let profile_metadata = fs::metadata(&profile_directory).expect("profile tombstone identity");
    let tombstone_name = format!(
        ".cleared-{}.{}.d.{:016x}.{:016x}",
        cleared.instance_id(),
        ProfileInstanceId::from_bytes([0xdd; 16]),
        profile_metadata.dev(),
        profile_metadata.ino()
    );
    let tombstone = root.join("profiles").join(&tombstone_name);
    fs::rename(&profile_directory, &tombstone).expect("simulate durable profile tombstone");

    let reopened = WorkspaceStore::open(&config).expect("startup completes clear recovery");
    assert!(!tombstone.exists());
    assert_eq!(
        reopened
            .load(cleared.instance_id())
            .expect("cleared profile remains absent"),
        None
    );
    assert!(matches!(
        reopened.load(survivor.instance_id()),
        Err(WorkspaceStoreError::CorruptShard)
    ));

    let quarantine = root.join("quarantine");
    let retained = retained_files(&quarantine)
        .into_iter()
        .flat_map(|path| fs::read(path).expect("read recovered quarantine"))
        .collect::<Vec<_>>();
    let retained = String::from_utf8_lossy(&retained);
    assert!(!retained.contains(cleared_source));
    assert!(retained.contains(survivor_source));
}

#[test]
fn crash_restart_never_deletes_unbound_or_identity_mismatched_replacement_tombstones() {
    if std::env::var_os("DBOTTER_J2_UNBOUND_CLEAR_CONFIG").is_some() {
        return;
    }

    for identity_bound in [false, true] {
        let temp = tempfile::tempdir().expect("unbound clear crash parent");
        let config = temp.path().join("dbotter.toml");
        let output = Command::new(std::env::current_exe().expect("current test executable"))
            .arg("--exact")
            .arg("workspace_store_subprocess_crashes_with_replacement_tombstone")
            .arg("--nocapture")
            .env("DBOTTER_J2_UNBOUND_CLEAR_CONFIG", &config)
            .env(
                "DBOTTER_J2_BOUND_CLEAR_TOMBSTONE",
                if identity_bound { "1" } else { "0" },
            )
            .output()
            .expect("spawn unbound clear crash helper");
        assert_eq!(
            output.status.code(),
            Some(86),
            "crash helper did not stop at the simulated clear crash: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        let root = workspace_root_for_config(&config).expect("unbound clear root");
        let profiles = root.join("profiles");
        let tombstone = fs::read_dir(&profiles)
            .expect("read profiles after crash")
            .map(|entry| entry.expect("crash profile entry").path())
            .find(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(".cleared-"))
            })
            .expect("replacement tombstone survives helper crash");
        let displaced_original = temp.path().join("displaced-original-workspace");
        let outside_target = temp.path().join("outside-clear-target");

        let error = WorkspaceStore::open(&config)
            .expect_err("restart must fail closed without a matching durable identity");
        assert!(matches!(error, WorkspaceStoreError::UnsafePath));
        let replacement_bytes = fs::read_dir(&tombstone)
            .expect("read retained replacement workspace")
            .map(|entry| entry.expect("replacement workspace entry"))
            .filter(|entry| {
                entry
                    .file_type()
                    .expect("replacement workspace entry type")
                    .is_file()
            })
            .flat_map(|entry| fs::read(entry.path()).expect("read replacement workspace file"))
            .collect::<Vec<_>>();
        assert!(
            String::from_utf8_lossy(&replacement_bytes)
                .contains("replacement_must_survive_restart")
        );
        let original_bytes = retained_files(&displaced_original)
            .into_iter()
            .flat_map(|path| fs::read(path).expect("read displaced original workspace"))
            .collect::<Vec<_>>();
        assert!(String::from_utf8_lossy(&original_bytes).contains("original_must_survive_restart"));
        assert_eq!(
            fs::read_link(tombstone.join("outside-link"))
                .expect("replacement outside link survives"),
            outside_target
        );
        assert_eq!(
            fs::read_to_string(&outside_target).expect("outside clear target survives"),
            CREDENTIAL_SENTINEL
        );
    }
}

#[test]
fn workspace_store_subprocess_crashes_with_replacement_tombstone() {
    let Some(config) = std::env::var_os("DBOTTER_J2_UNBOUND_CLEAR_CONFIG") else {
        return;
    };
    let config = PathBuf::from(config);
    let root = workspace_root_for_config(&config).expect("unbound helper root");
    let original = snapshot(
        0x79,
        "clear-race-original",
        "SELECT original_must_survive_restart",
    );
    let replacement = snapshot(
        0x79,
        "clear-race-replacement",
        "SELECT replacement_must_survive_restart",
    );
    let store = WorkspaceStore::open(&config).expect("unbound helper store");
    store.commit(&original).expect("commit original workspace");
    let profiles = root.join("profiles");
    let canonical = profiles.join(original.instance_id().to_string());
    let original_metadata = fs::metadata(&canonical).expect("original workspace identity");
    let displaced_original = config
        .parent()
        .expect("unbound helper config parent")
        .join("displaced-original-workspace");
    fs::rename(&canonical, &displaced_original).expect("displace validated original workspace");
    store
        .commit(&replacement)
        .expect("commit replacement workspace at canonical path");
    let outside_target = config
        .parent()
        .expect("unbound helper config parent")
        .join("outside-clear-target");
    fs::write(&outside_target, CREDENTIAL_SENTINEL).expect("write outside clear target");
    fs::set_permissions(&outside_target, fs::Permissions::from_mode(0o600))
        .expect("private outside clear target");
    symlink(&outside_target, canonical.join("outside-link"))
        .expect("replacement outside link fixture");

    let nonce = ProfileInstanceId::from_bytes([0xda; 16]);
    let tombstone_name = if std::env::var_os("DBOTTER_J2_BOUND_CLEAR_TOMBSTONE").as_deref()
        == Some(std::ffi::OsStr::new("1"))
    {
        format!(
            ".cleared-{}.{nonce}.d.{:016x}.{:016x}",
            original.instance_id(),
            original_metadata.dev(),
            original_metadata.ino()
        )
    } else {
        format!(".cleared-{}.{nonce}", original.instance_id())
    };
    let tombstone = profiles.join(tombstone_name);
    fs::rename(&canonical, &tombstone).expect("simulate unbound clear rename");
    fs::File::open(&profiles)
        .expect("open profiles for crash durability")
        .sync_all()
        .expect("durably publish unbound tombstone");
    std::process::exit(86);
}

#[test]
fn manifest_checksum_is_exact_and_unreferenced_generation_is_ignored_then_collected() {
    let temp = tempfile::tempdir().expect("temporary workspace parent");
    let config = temp.path().join("dbotter.toml");
    let expected = snapshot(0x73, "checksum", "SELECT exact_shard_checksum");
    let store = WorkspaceStore::open(&config).expect("workspace store");
    let commit = store.commit(&expected).expect("workspace commit");
    let root = workspace_root_for_config(&config).expect("workspace sibling path");
    let (profile_directory, _, shard_path, manifest) =
        current_generation_paths(&root, expected.instance_id());
    let shard_bytes = fs::read(&shard_path).expect("read manifest-referenced shard");
    let exact_checksum = lower_hex(&Sha256::digest(&shard_bytes));
    assert_eq!(manifest["checksum"].as_str(), Some(exact_checksum.as_str()));
    assert_eq!(commit.checksum(), exact_checksum);

    let orphan = profile_directory.join("shard-00000000000000000002.json");
    fs::write(&orphan, b"unreferenced corrupt generation").expect("write orphan generation");
    fs::set_permissions(&orphan, fs::Permissions::from_mode(0o600))
        .expect("private orphan generation");
    assert_eq!(
        store
            .load(expected.instance_id())
            .expect("unreferenced shard is ignored"),
        Some(expected.clone())
    );

    store.commit(&expected).expect("next generation commit");
    assert!(
        !orphan.exists(),
        "next commit collects unreferenced generation"
    );
}

#[test]
fn symlinked_current_shard_is_removed_without_retaining_or_following_the_link() {
    let temp = tempfile::tempdir().expect("temporary workspace parent");
    let config = temp.path().join("dbotter.toml");
    let expected = snapshot(0x74, "shard-symlink", PRIVATE_SQL);
    let store = WorkspaceStore::open(&config).expect("workspace store");
    store.commit(&expected).expect("workspace commit");
    let root = workspace_root_for_config(&config).expect("workspace sibling path");
    let (_, _, shard_path, _) = current_generation_paths(&root, expected.instance_id());
    let outside = temp.path().join("outside-shard-target");
    fs::write(&outside, CREDENTIAL_SENTINEL).expect("outside symlink target");
    fs::set_permissions(&outside, fs::Permissions::from_mode(0o600))
        .expect("private outside target");
    fs::remove_file(&shard_path).expect("remove current shard fixture");
    symlink(&outside, &shard_path).expect("current shard symlink fixture");

    assert!(matches!(
        store.load(expected.instance_id()),
        Err(WorkspaceStoreError::CorruptShard)
    ));
    assert_eq!(
        fs::read_to_string(&outside).expect("outside target remains readable"),
        CREDENTIAL_SENTINEL
    );
    assert_private_tree(&root);
}

#[test]
fn quarantine_is_bounded_by_frozen_file_and_byte_caps() {
    let temp = tempfile::tempdir().expect("temporary workspace parent");
    let config = temp.path().join("dbotter.toml");
    let root = workspace_root_for_config(&config).expect("workspace sibling path");
    let store = WorkspaceStore::open(&config).expect("workspace store");

    for index in 0..(MAX_QUARANTINE_FILES + 3) {
        let expected = snapshot(
            u8::try_from(0x80 + index).expect("bounded instance byte"),
            "quarantine-bound",
            "SELECT bounded_quarantine_fixture",
        );
        store.commit(&expected).expect("workspace commit");
        let (_, _, shard_path, _) = current_generation_paths(&root, expected.instance_id());
        fs::write(&shard_path, vec![b'x'; 1024]).expect("corrupt bounded shard");
        fs::set_permissions(&shard_path, fs::Permissions::from_mode(0o600))
            .expect("private corrupt shard");
        assert!(matches!(
            store.load(expected.instance_id()),
            Err(WorkspaceStoreError::CorruptShard)
        ));
    }

    let quarantine = root.join("quarantine");
    let files = retained_files(&quarantine);
    let bytes = files.iter().fold(0_u64, |total, path| {
        total + fs::metadata(path).expect("quarantine metadata").len()
    });
    assert!(files.len() <= MAX_QUARANTINE_FILES);
    assert!(bytes <= MAX_QUARANTINE_BYTES);
}

#[test]
fn read_write_startup_retries_quarantine_trim_and_propagates_unsafe_entries() {
    let retry_temp = tempfile::tempdir().expect("quarantine trim retry parent");
    let retry_config = retry_temp.path().join("dbotter.toml");
    let retry_root = workspace_root_for_config(&retry_config).expect("quarantine trim retry root");
    drop(WorkspaceStore::open(&retry_config).expect("initialize retry workspace"));
    let retry_quarantine = retry_root.join("quarantine");
    let retry_instance = ProfileInstanceId::from_bytes([0xe0; 16]);
    for index in 0..(MAX_QUARANTINE_FILES + 3) {
        let name = quarantine_entry_name(
            retry_instance,
            u8::try_from(index + 1).expect("quarantine retry nonce"),
        );
        let path = retry_quarantine.join(name);
        fs::write(&path, vec![b'x'; 1024]).expect("write retry quarantine entry");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
            .expect("private retry quarantine entry");
    }
    let reopened = WorkspaceStore::open(&retry_config).expect("startup retries quarantine trim");
    drop(reopened);
    let files = retained_files(&retry_quarantine);
    let bytes = files.iter().try_fold(0_u64, |total, path| {
        total.checked_add(fs::metadata(path).expect("retry quarantine metadata").len())
    });
    assert!(files.len() <= MAX_QUARANTINE_FILES);
    assert!(bytes.is_some_and(|total| total <= MAX_QUARANTINE_BYTES));

    let unsafe_temp = tempfile::tempdir().expect("unsafe quarantine parent");
    let unsafe_config = unsafe_temp.path().join("dbotter.toml");
    let unsafe_root = workspace_root_for_config(&unsafe_config).expect("unsafe quarantine root");
    drop(WorkspaceStore::open(&unsafe_config).expect("initialize unsafe workspace"));
    let outside = unsafe_temp.path().join("outside-quarantine-target");
    fs::write(&outside, CREDENTIAL_SENTINEL).expect("write outside quarantine target");
    fs::set_permissions(&outside, fs::Permissions::from_mode(0o600))
        .expect("private outside quarantine target");
    let unsafe_name = quarantine_entry_name(ProfileInstanceId::from_bytes([0xe1; 16]), 0x01);
    symlink(&outside, unsafe_root.join("quarantine").join(unsafe_name))
        .expect("unsafe quarantine symlink");
    let error = WorkspaceStore::open(&unsafe_config)
        .expect_err("unsafe quarantine entry must fail startup closed");
    assert!(matches!(error, WorkspaceStoreError::UnsafePath));
    assert_eq!(
        fs::read_to_string(&outside).expect("outside quarantine target survives"),
        CREDENTIAL_SENTINEL
    );
}

#[test]
fn retained_root_profiles_and_writer_lock_replacement_fail_closed() {
    let root_temp = tempfile::tempdir().expect("root replacement parent");
    let root_config = root_temp.path().join("dbotter.toml");
    let root = workspace_root_for_config(&root_config).expect("root replacement path");
    let root_store = WorkspaceStore::open(&root_config).expect("root replacement store");
    let root_snapshot = snapshot(0xd1, "root-replacement", "SELECT root_replacement");
    root_store
        .commit(&root_snapshot)
        .expect("root replacement fixture commit");
    fs::rename(&root, root_temp.path().join("detached-workspace"))
        .expect("detach retained workspace root");
    create_private_directory(&root);
    assert!(matches!(
        root_store.load(root_snapshot.instance_id()),
        Err(WorkspaceStoreError::UnsafePath)
    ));

    let profiles_temp = tempfile::tempdir().expect("profiles replacement parent");
    let profiles_config = profiles_temp.path().join("dbotter.toml");
    let profiles_root =
        workspace_root_for_config(&profiles_config).expect("profiles replacement path");
    let profiles_store =
        WorkspaceStore::open(&profiles_config).expect("profiles replacement store");
    let profiles_snapshot = snapshot(0xd2, "profiles-replacement", "SELECT profiles_replacement");
    profiles_store
        .commit(&profiles_snapshot)
        .expect("profiles replacement fixture commit");
    let profiles = profiles_root.join("profiles");
    fs::rename(&profiles, profiles_root.join("profiles.detached"))
        .expect("detach retained profiles directory");
    create_private_directory(&profiles);
    assert!(matches!(
        profiles_store.load(profiles_snapshot.instance_id()),
        Err(WorkspaceStoreError::UnsafePath)
    ));

    let lock_temp = tempfile::tempdir().expect("lock replacement parent");
    let lock_config = lock_temp.path().join("dbotter.toml");
    let lock_root = workspace_root_for_config(&lock_config).expect("lock replacement path");
    let lock_store = WorkspaceStore::open(&lock_config).expect("lock replacement store");
    let lock_snapshot = snapshot(0xd3, "lock-replacement", "SELECT lock_replacement");
    lock_store
        .commit(&lock_snapshot)
        .expect("lock replacement fixture commit");
    let lock_path = lock_root.join("writer.lock");
    fs::remove_file(&lock_path).expect("unlink retained writer lock");
    fs::write(&lock_path, b"").expect("create replacement writer lock");
    fs::set_permissions(&lock_path, fs::Permissions::from_mode(0o600))
        .expect("set replacement writer lock permissions");
    assert!(matches!(
        lock_store.load(lock_snapshot.instance_id()),
        Err(WorkspaceStoreError::UnsafePath)
    ));
}

#[test]
fn relative_config_path_is_resolved_from_the_process_working_directory() {
    let temp = tempfile::tempdir().expect("relative config process directory");
    let output = Command::new(std::env::current_exe().expect("current test executable"))
        .arg("--exact")
        .arg("workspace_store_subprocess_relative_config")
        .arg("--nocapture")
        .current_dir(temp.path())
        .env("DBOTTER_J2_RELATIVE_CONFIG", "1")
        .output()
        .expect("spawn relative config test process");
    assert!(
        output.status.success(),
        "relative config helper failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(temp.path().join(".dbotter.toml.workspace").is_dir());
}

#[test]
fn workspace_store_subprocess_relative_config() {
    if std::env::var_os("DBOTTER_J2_RELATIVE_CONFIG").is_none() {
        return;
    }
    let expected = snapshot(0xd4, "relative-config", "SELECT relative_config");
    let store =
        WorkspaceStore::open(Path::new("dbotter.toml")).expect("open relative config workspace");
    store
        .commit(&expected)
        .expect("commit relative config workspace");
    assert_eq!(
        store
            .load(expected.instance_id())
            .expect("load relative config workspace"),
        Some(expected)
    );
}

#[test]
fn separate_process_observes_writer_busy_without_mutating_the_store() {
    if std::env::var_os("DBOTTER_J2_LOCK_HOLDER_CONFIG").is_some() {
        return;
    }

    let temp = tempfile::tempdir().expect("temporary workspace parent");
    let config = temp.path().join("dbotter.toml");
    let ready = temp.path().join("holder.ready");
    let stop = temp.path().join("holder.stop");
    let mut child = Command::new(std::env::current_exe().expect("current test executable"))
        .arg("--exact")
        .arg("workspace_store_subprocess_lock_holder")
        .arg("--nocapture")
        .env("DBOTTER_J2_LOCK_HOLDER_CONFIG", &config)
        .env("DBOTTER_J2_LOCK_HOLDER_READY", &ready)
        .env("DBOTTER_J2_LOCK_HOLDER_STOP", &stop)
        .spawn()
        .expect("spawn lock holder test process");

    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(ready.exists(), "lock holder did not become ready");
    let observer = WorkspaceStore::open(&config).expect("open read-only observer");
    assert_eq!(observer.mode(), WorkspaceStoreMode::ReadOnly);
    assert_eq!(
        observer.read_only_reason(),
        Some(WorkspaceReadOnlyReason::WriterBusy)
    );

    fs::write(&stop, b"stop").expect("signal lock holder");
    let status = child.wait().expect("wait for lock holder");
    assert!(status.success(), "lock holder process failed");
}

#[test]
fn workspace_store_subprocess_lock_holder() {
    let Some(config) = std::env::var_os("DBOTTER_J2_LOCK_HOLDER_CONFIG") else {
        return;
    };
    let ready = PathBuf::from(
        std::env::var_os("DBOTTER_J2_LOCK_HOLDER_READY").expect("lock holder ready path"),
    );
    let stop = PathBuf::from(
        std::env::var_os("DBOTTER_J2_LOCK_HOLDER_STOP").expect("lock holder stop path"),
    );
    let store = WorkspaceStore::open(Path::new(&config)).expect("subprocess writer store");
    assert_eq!(store.mode(), WorkspaceStoreMode::ReadWrite);
    fs::write(&ready, b"ready").expect("publish lock holder readiness");

    let deadline = Instant::now() + Duration::from_secs(10);
    while !stop.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(stop.exists(), "parent did not release lock holder");
}

fn assert_error_redacted(error: &WorkspaceStoreError, private_root: &Path) {
    use std::error::Error as _;

    let display = error.to_string();
    let debug = format!("{error:?}");
    for forbidden in [
        private_root.to_string_lossy().as_ref(),
        PRIVATE_SQL,
        RESULT_SENTINEL,
        BACKEND_SENTINEL,
        CREDENTIAL_SENTINEL,
    ] {
        assert!(!display.contains(forbidden));
        assert!(!debug.contains(forbidden));
    }
    assert!(error.source().is_none());
}

fn current_generation_paths(
    root: &Path,
    instance_id: ProfileInstanceId,
) -> (PathBuf, PathBuf, PathBuf, serde_json::Value) {
    let profile_directory = root.join("profiles").join(instance_id.to_string());
    let manifest_path = profile_directory.join("manifest.json");
    let manifest = serde_json::from_slice::<serde_json::Value>(
        &fs::read(&manifest_path).expect("read current manifest"),
    )
    .expect("parse current manifest");
    let shard_path = profile_directory.join(
        manifest["shard"]
            .as_str()
            .expect("manifest shard file name"),
    );
    (profile_directory, manifest_path, shard_path, manifest)
}

fn lower_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}
