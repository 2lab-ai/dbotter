use std::fs;
use std::ops::Range;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use dbotter::model::{ProfileId, ProfileInstanceId};
use dbotter::workspace::{
    EditorTabSnapshot, MAX_EDITOR_SOURCE_BYTES, MAX_EDITOR_TABS_PER_PROFILE,
    MAX_HISTORY_SOURCE_BYTES, ProfileWorkspaceSnapshot, WorkspaceCommit, WorkspaceGeometrySnapshot,
    WorkspaceHistoryEntry, WorkspaceHistoryStatus, WorkspaceLanguage, WorkspaceRunTarget,
    WorkspaceSnapshotError, WorkspaceStore, WorkspaceStoreError, WorkspaceStoreMode,
    workspace_root_for_config,
};

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
        .map(|path| fs::read(path).expect("read private retained file"))
        .flatten()
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
