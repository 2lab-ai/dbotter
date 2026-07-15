use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use dbotter::export::write_export;
use dbotter::export_file::{
    ExportFileError, ExportFileFaultInjector, ExportFileStage, confirm_replace,
    export_result_to_file, export_result_to_file_with_faults,
};
use dbotter::model::{
    Cell, Column, DriverKind, ExportFormat, ExportResult, OperationId, OverwritePolicy,
    ProfileGeneration, ProfileId, ResultId, ResultProvenance, ResultSnapshot,
    TransientAllocationQualification,
};

fn snapshot(rows: usize) -> Arc<ResultSnapshot> {
    Arc::new(ResultSnapshot {
        provenance: ResultProvenance {
            result_id: ResultId(71),
            profile_id: ProfileId("profile-file".to_owned()),
            profile_generation: ProfileGeneration(7),
            operation_id: OperationId(17),
            driver: DriverKind::MySql,
            completed_at_unix_ms: 1_700_000_000_123,
            duration_ms: 19,
        },
        columns: vec![Column {
            name: "value".to_owned(),
            type_name: "TEXT".to_owned(),
        }],
        rows: (0..rows)
            .map(|index| vec![Cell::Text(format!("row-{index}"))])
            .collect(),
        affected_rows: 0,
        last_insert_id: None,
        truncated: false,
        notices: Vec::new(),
        retained_bytes: 0,
        transient_allocation: TransientAllocationQualification::MySqlCurrentRowOrCell,
        cell_truncations: Vec::new(),
    })
}

fn request(
    directory: &Path,
    format: ExportFormat,
    overwrite_policy: OverwritePolicy,
    rows: usize,
) -> ExportResult {
    ExportResult {
        result_id: ResultId(71),
        operation_id: OperationId(91),
        snapshot: snapshot(rows),
        format,
        destination: directory.join(match format {
            ExportFormat::Csv => "result.csv",
            ExportFormat::Tsv => "result.tsv",
            ExportFormat::Json => "result.json",
        }),
        overwrite_policy,
    }
}

fn expected_bytes(request: &ExportResult) -> Vec<u8> {
    let mut bytes = Vec::new();
    write_export(&request.snapshot, request.format, &mut bytes).expect("reference app encoder");
    bytes
}

fn temp_entries(directory: &Path) -> Vec<PathBuf> {
    fs::read_dir(directory)
        .expect("read export directory")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(".dbotter-export."))
        })
        .collect()
}

#[cfg(unix)]
fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt as _;
    fs::metadata(path)
        .expect("destination metadata")
        .permissions()
        .mode()
        & 0o777
}

#[derive(Default)]
struct Recorder {
    stages: Mutex<Vec<ExportFileStage>>,
    fail_at: Option<ExportFileStage>,
    create_competitor_at: Option<ExportFileStage>,
}

impl Recorder {
    fn failing(stage: ExportFileStage) -> Self {
        Self {
            stages: Mutex::new(Vec::new()),
            fail_at: Some(stage),
            create_competitor_at: None,
        }
    }

    fn competing(stage: ExportFileStage) -> Self {
        Self {
            stages: Mutex::new(Vec::new()),
            fail_at: None,
            create_competitor_at: Some(stage),
        }
    }

    fn observed(&self) -> Vec<ExportFileStage> {
        self.stages.lock().expect("stage recorder").clone()
    }
}

impl ExportFileFaultInjector for Recorder {
    fn check(&self, stage: ExportFileStage, destination: &Path) -> io::Result<()> {
        self.stages.lock().expect("stage recorder").push(stage);
        if self.create_competitor_at == Some(stage) {
            fs::write(destination, b"competitor")?;
        }
        if self.fail_at == Some(stage) {
            Err(io::Error::other("injected export-file failure"))
        } else {
            Ok(())
        }
    }
}

#[test]
fn deny_overwrite_streams_to_same_directory_private_temp_and_syncs_in_order() {
    let directory = tempfile::tempdir().expect("tempdir");
    let request = request(
        directory.path(),
        ExportFormat::Csv,
        OverwritePolicy::DenyOverwrite,
        3,
    );
    let expected = expected_bytes(&request);
    let recorder = Recorder::default();

    let outcome = export_result_to_file_with_faults(&request, None, || false, &recorder)
        .expect("deny-overwrite export");
    assert_eq!(
        fs::read(&request.destination).expect("destination"),
        expected
    );
    assert_eq!(outcome.bytes_written as usize, expected.len());
    assert_eq!(outcome.row_count, 3);
    #[cfg(unix)]
    assert_eq!(mode(&request.destination), 0o600);
    assert!(temp_entries(directory.path()).is_empty());
    assert_eq!(
        recorder.observed(),
        vec![
            ExportFileStage::TempCreated,
            ExportFileStage::Encoded,
            ExportFileStage::Flushed,
            ExportFileStage::FileSynced,
            ExportFileStage::BeforeCommit,
            ExportFileStage::Committed,
            ExportFileStage::BeforeDirectorySync,
            ExportFileStage::DirectorySynced,
        ]
    );
}

#[test]
fn atomic_deny_overwrite_never_clobbers_existing_or_competing_destination() {
    let directory = tempfile::tempdir().expect("tempdir");
    let request = request(
        directory.path(),
        ExportFormat::Tsv,
        OverwritePolicy::DenyOverwrite,
        2,
    );
    fs::write(&request.destination, b"existing").expect("existing destination");
    assert!(matches!(
        export_result_to_file(&request, None, || false),
        Err(ExportFileError::DestinationExists)
    ));
    assert_eq!(
        fs::read(&request.destination).expect("existing"),
        b"existing"
    );

    fs::remove_file(&request.destination).expect("remove existing fixture");
    let competitor = Recorder::competing(ExportFileStage::BeforeCommit);
    assert!(matches!(
        export_result_to_file_with_faults(&request, None, || false, &competitor),
        Err(ExportFileError::DestinationExists)
    ));
    assert_eq!(
        fs::read(&request.destination).expect("competitor"),
        b"competitor"
    );
    assert!(temp_entries(directory.path()).is_empty());
}

#[test]
fn confirmed_replace_requires_same_regular_file_identity_and_rejects_symlink_or_directory() {
    let directory = tempfile::tempdir().expect("tempdir");
    let request = request(
        directory.path(),
        ExportFormat::Json,
        OverwritePolicy::ReplaceConfirmed,
        1,
    );
    fs::write(&request.destination, b"old").expect("old destination");
    let confirmation = confirm_replace(&request.destination).expect("confirm regular file");
    fs::write(&request.destination, b"changed-and-longer").expect("change destination");
    assert!(matches!(
        export_result_to_file(&request, Some(&confirmation), || false),
        Err(ExportFileError::DestinationChanged)
    ));
    assert_eq!(
        fs::read(&request.destination).expect("changed destination"),
        b"changed-and-longer"
    );
    assert!(temp_entries(directory.path()).is_empty());

    let other = directory.path().join("other.json");
    fs::write(&other, b"other").expect("other destination");
    let other_confirmation = confirm_replace(&other).expect("other confirmation");
    assert!(matches!(
        export_result_to_file(&request, Some(&other_confirmation), || false),
        Err(ExportFileError::ConfirmationMismatch)
    ));

    let subdirectory = directory.path().join("directory");
    fs::create_dir(&subdirectory).expect("subdirectory");
    assert!(matches!(
        confirm_replace(&subdirectory),
        Err(ExportFileError::InvalidDestinationType)
    ));

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        let link = directory.path().join("link.json");
        symlink(&request.destination, &link).expect("destination symlink");
        assert!(matches!(
            confirm_replace(&link),
            Err(ExportFileError::InvalidDestinationType)
        ));
    }

    let fresh_confirmation = confirm_replace(&request.destination).expect("fresh confirmation");
    let expected = expected_bytes(&request);
    export_result_to_file(&request, Some(&fresh_confirmation), || false)
        .expect("confirmed replacement");
    assert_eq!(
        fs::read(&request.destination).expect("replacement"),
        expected
    );
}

#[test]
fn cancellation_and_precommit_failpoints_clean_temp_without_claiming_commit() {
    let directory = tempfile::tempdir().expect("tempdir");
    let mut request = request(
        directory.path(),
        ExportFormat::Csv,
        OverwritePolicy::DenyOverwrite,
        512,
    );
    request.snapshot = Arc::new(ResultSnapshot {
        rows: (0..512)
            .map(|_| vec![Cell::Text("x".repeat(32 * 1024))])
            .collect(),
        ..(*request.snapshot).clone()
    });
    let mut checks = 0_usize;
    assert!(matches!(
        export_result_to_file(&request, None, || {
            checks += 1;
            checks > 5
        }),
        Err(ExportFileError::Cancelled)
    ));
    assert!(!request.destination.exists());
    assert!(temp_entries(directory.path()).is_empty());

    for stage in [
        ExportFileStage::TempCreated,
        ExportFileStage::Encoded,
        ExportFileStage::Flushed,
        ExportFileStage::FileSynced,
        ExportFileStage::BeforeCommit,
    ] {
        let fault = Recorder::failing(stage);
        assert!(matches!(
            export_result_to_file_with_faults(&request, None, || false, &fault),
            Err(ExportFileError::NotCommitted)
        ));
        assert!(!request.destination.exists(), "stage {stage:?}");
        assert!(temp_entries(directory.path()).is_empty(), "stage {stage:?}");
    }
}

#[test]
fn postcommit_failures_preserve_file_and_report_committed_durability_unknown() {
    let directory = tempfile::tempdir().expect("tempdir");
    let request = request(
        directory.path(),
        ExportFormat::Csv,
        OverwritePolicy::DenyOverwrite,
        2,
    );
    let expected = expected_bytes(&request);

    for stage in [
        ExportFileStage::Committed,
        ExportFileStage::BeforeDirectorySync,
        ExportFileStage::DirectorySynced,
    ] {
        if request.destination.exists() {
            fs::remove_file(&request.destination).expect("reset destination");
        }
        let fault = Recorder::failing(stage);
        assert!(matches!(
            export_result_to_file_with_faults(&request, None, || false, &fault),
            Err(ExportFileError::CommittedDurabilityUnknown)
        ));
        assert_eq!(
            fs::read(&request.destination).expect("committed destination"),
            expected,
            "stage {stage:?}"
        );
        assert!(temp_entries(directory.path()).is_empty());
    }
}
