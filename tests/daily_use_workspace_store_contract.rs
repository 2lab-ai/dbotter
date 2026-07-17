use std::fs;
use std::ops::Range;
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use dbotter::model::{ProfileId, ProfileInstanceId};
use dbotter::workspace::{
    EditorTabSnapshot, MAX_EDITOR_SOURCE_BYTES, MAX_EDITOR_TABS_PER_PROFILE,
    MAX_HISTORY_SOURCE_BYTES, ProfileWorkspaceSnapshot, WorkspaceCommit, WorkspaceGeometrySnapshot,
    WorkspaceHistoryEntry, WorkspaceHistoryStatus, WorkspaceLanguage, WorkspaceReadOnlyReason,
    WorkspaceRunTarget, WorkspaceSnapshotError, WorkspaceStore, WorkspaceStoreError,
    WorkspaceStoreMode, workspace_root_for_config,
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
