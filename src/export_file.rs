use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::export::{ExportEncodeError, write_export_with_cancel};
use crate::model::{ExportFormat, ExportResult, OverwritePolicy};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFileStage {
    TempCreated,
    Encoded,
    Flushed,
    FileSynced,
    BeforeCommit,
    Committed,
    BeforeDirectorySync,
    DirectorySynced,
}

pub trait ExportFileFaultInjector {
    fn check(&self, stage: ExportFileStage, destination: &Path) -> io::Result<()>;
}

#[derive(Debug, Default)]
pub struct NoExportFileFaults;

impl ExportFileFaultInjector for NoExportFileFaults {
    fn check(&self, _stage: ExportFileStage, _destination: &Path) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    device: u64,
    inode: u64,
    size: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
}

#[derive(Clone)]
pub struct ConfirmedDestination {
    path: PathBuf,
    identity: FileIdentity,
}

impl std::fmt::Debug for ConfirmedDestination {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ConfirmedDestination(<redacted>)")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExportFileOutcome {
    pub format: ExportFormat,
    pub overwrite_policy: OverwritePolicy,
    pub row_count: usize,
    pub bytes_written: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum ExportFileError {
    #[error("export was cancelled before commit")]
    Cancelled,
    #[error("the export destination already exists")]
    DestinationExists,
    #[error("the export destination must be a regular file")]
    InvalidDestinationType,
    #[error("confirmed replacement identity is required")]
    ConfirmationRequired,
    #[error("confirmed replacement identity does not match this destination")]
    ConfirmationMismatch,
    #[error("the confirmed export destination changed")]
    DestinationChanged,
    #[error("export result identity does not match its immutable snapshot")]
    ResultIdentityMismatch,
    #[error("export encoding failed before commit")]
    Encode {
        #[source]
        source: ExportEncodeError,
    },
    #[error("export did not commit at {stage:?}")]
    NotCommitted {
        stage: ExportFileStage,
        #[source]
        source: io::Error,
    },
    #[error("export committed but durability is unknown at {stage:?}")]
    CommittedDurabilityUnknown {
        stage: ExportFileStage,
        #[source]
        source: io::Error,
    },
}

pub fn confirm_replace(destination: &Path) -> Result<ConfirmedDestination, ExportFileError> {
    let metadata = symlink_regular_metadata(destination)?;
    Ok(ConfirmedDestination {
        path: destination.to_owned(),
        identity: file_identity(&metadata),
    })
}

pub fn export_result_to_file<F>(
    request: &ExportResult,
    confirmation: Option<&ConfirmedDestination>,
    is_cancelled: F,
) -> Result<ExportFileOutcome, ExportFileError>
where
    F: FnMut() -> bool,
{
    export_result_to_file_with_faults(request, confirmation, is_cancelled, &NoExportFileFaults)
}

pub fn export_result_to_file_with_faults<F>(
    request: &ExportResult,
    confirmation: Option<&ConfirmedDestination>,
    mut is_cancelled: F,
    faults: &dyn ExportFileFaultInjector,
) -> Result<ExportFileOutcome, ExportFileError>
where
    F: FnMut() -> bool,
{
    if request.result_id != request.snapshot.provenance.result_id {
        return Err(ExportFileError::ResultIdentityMismatch);
    }
    validate_commit_policy(request, confirmation)?;
    let directory = destination_directory(&request.destination);
    let (temp_path, mut temp_file) = create_private_temp(&directory)?;
    let cleanup = TempCleanup::new(temp_path.clone());
    precommit_fault(faults, ExportFileStage::TempCreated, &request.destination)?;

    match write_export_with_cancel(&request.snapshot, request.format, &mut temp_file, || {
        is_cancelled()
    }) {
        Ok(()) => {}
        Err(ExportEncodeError::Cancelled) => return Err(ExportFileError::Cancelled),
        Err(source) => return Err(ExportFileError::Encode { source }),
    }
    precommit_fault(faults, ExportFileStage::Encoded, &request.destination)?;

    temp_file
        .flush()
        .map_err(|source| ExportFileError::NotCommitted {
            stage: ExportFileStage::Flushed,
            source,
        })?;
    precommit_fault(faults, ExportFileStage::Flushed, &request.destination)?;
    temp_file
        .sync_all()
        .map_err(|source| ExportFileError::NotCommitted {
            stage: ExportFileStage::FileSynced,
            source,
        })?;
    precommit_fault(faults, ExportFileStage::FileSynced, &request.destination)?;
    let bytes_written = temp_file
        .metadata()
        .map_err(|source| ExportFileError::NotCommitted {
            stage: ExportFileStage::FileSynced,
            source,
        })?
        .len();

    if is_cancelled() {
        return Err(ExportFileError::Cancelled);
    }
    precommit_fault(faults, ExportFileStage::BeforeCommit, &request.destination)?;
    commit_destination(request, confirmation, &temp_path)?;
    cleanup.disarm();

    postcommit_fault(faults, ExportFileStage::Committed, &request.destination)?;
    postcommit_fault(
        faults,
        ExportFileStage::BeforeDirectorySync,
        &request.destination,
    )?;
    sync_directory(&directory).map_err(|source| ExportFileError::CommittedDurabilityUnknown {
        stage: ExportFileStage::BeforeDirectorySync,
        source,
    })?;
    postcommit_fault(
        faults,
        ExportFileStage::DirectorySynced,
        &request.destination,
    )?;

    Ok(ExportFileOutcome {
        format: request.format,
        overwrite_policy: request.overwrite_policy,
        row_count: request.snapshot.rows.len(),
        bytes_written,
    })
}

fn validate_commit_policy(
    request: &ExportResult,
    confirmation: Option<&ConfirmedDestination>,
) -> Result<(), ExportFileError> {
    match request.overwrite_policy {
        OverwritePolicy::DenyOverwrite => {
            if confirmation.is_some() {
                return Err(ExportFileError::ConfirmationMismatch);
            }
            match fs::symlink_metadata(&request.destination) {
                Ok(metadata) if metadata.file_type().is_file() => {
                    Err(ExportFileError::DestinationExists)
                }
                Ok(_) => Err(ExportFileError::InvalidDestinationType),
                Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(source) => Err(ExportFileError::NotCommitted {
                    stage: ExportFileStage::TempCreated,
                    source,
                }),
            }
        }
        OverwritePolicy::ReplaceConfirmed => {
            let confirmation = confirmation.ok_or(ExportFileError::ConfirmationRequired)?;
            if confirmation.path == request.destination {
                Ok(())
            } else {
                Err(ExportFileError::ConfirmationMismatch)
            }
        }
    }
}

fn commit_destination(
    request: &ExportResult,
    confirmation: Option<&ConfirmedDestination>,
    temp_path: &Path,
) -> Result<(), ExportFileError> {
    match request.overwrite_policy {
        OverwritePolicy::DenyOverwrite => {
            if let Err(source) = rename_no_replace(temp_path, &request.destination) {
                return match fs::symlink_metadata(&request.destination) {
                    Ok(metadata) if metadata.file_type().is_file() => {
                        Err(ExportFileError::DestinationExists)
                    }
                    Ok(_) => Err(ExportFileError::InvalidDestinationType),
                    Err(_) => Err(ExportFileError::NotCommitted {
                        stage: ExportFileStage::BeforeCommit,
                        source,
                    }),
                };
            }
        }
        OverwritePolicy::ReplaceConfirmed => {
            let confirmation = confirmation.ok_or(ExportFileError::ConfirmationRequired)?;
            verify_confirmed_destination(&request.destination, confirmation)?;
            fs::rename(temp_path, &request.destination).map_err(|source| {
                ExportFileError::NotCommitted {
                    stage: ExportFileStage::BeforeCommit,
                    source,
                }
            })?;
        }
    }
    Ok(())
}

fn verify_confirmed_destination(
    destination: &Path,
    confirmation: &ConfirmedDestination,
) -> Result<(), ExportFileError> {
    if confirmation.path != destination {
        return Err(ExportFileError::ConfirmationMismatch);
    }
    let metadata = match fs::symlink_metadata(destination) {
        Ok(metadata) if metadata.file_type().is_file() => metadata,
        Ok(_) => return Err(ExportFileError::DestinationChanged),
        Err(_) => return Err(ExportFileError::DestinationChanged),
    };
    if file_identity(&metadata) == confirmation.identity {
        Ok(())
    } else {
        Err(ExportFileError::DestinationChanged)
    }
}

fn symlink_regular_metadata(destination: &Path) -> Result<fs::Metadata, ExportFileError> {
    match fs::symlink_metadata(destination) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(metadata),
        Ok(_) => Err(ExportFileError::InvalidDestinationType),
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            Err(ExportFileError::ConfirmationRequired)
        }
        Err(source) => Err(ExportFileError::NotCommitted {
            stage: ExportFileStage::BeforeCommit,
            source,
        }),
    }
}

#[cfg(unix)]
fn file_identity(metadata: &fs::Metadata) -> FileIdentity {
    use std::os::unix::fs::MetadataExt as _;
    FileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
        size: metadata.size(),
        modified_seconds: metadata.mtime(),
        modified_nanoseconds: metadata.mtime_nsec(),
    }
}

#[cfg(not(unix))]
fn file_identity(metadata: &fs::Metadata) -> FileIdentity {
    use std::time::UNIX_EPOCH;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(UNIX_EPOCH).ok());
    FileIdentity {
        device: 0,
        inode: 0,
        size: metadata.len(),
        modified_seconds: modified.map_or(0, |value| value.as_secs() as i64),
        modified_nanoseconds: modified.map_or(0, |value| value.subsec_nanos() as i64),
    }
}

fn destination_directory(destination: &Path) -> PathBuf {
    destination
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .to_owned()
}

fn create_private_temp(directory: &Path) -> Result<(PathBuf, File), ExportFileError> {
    for _ in 0..32 {
        let mut random = [0_u8; 16];
        getrandom::fill(&mut random).map_err(|_| ExportFileError::NotCommitted {
            stage: ExportFileStage::TempCreated,
            source: io::Error::other("operating-system randomness unavailable"),
        })?;
        let mut suffix = String::with_capacity(random.len() * 2);
        for byte in random {
            use std::fmt::Write as _;
            let _ = write!(suffix, "{byte:02x}");
        }
        let path = directory.join(format!(
            ".dbotter-export.{}.{}.tmp",
            std::process::id(),
            suffix
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(file) => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt as _;
                    if let Err(source) = file.set_permissions(fs::Permissions::from_mode(0o600)) {
                        let _ = fs::remove_file(&path);
                        return Err(ExportFileError::NotCommitted {
                            stage: ExportFileStage::TempCreated,
                            source,
                        });
                    }
                }
                return Ok((path, file));
            }
            Err(source) if source.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(source) => {
                return Err(ExportFileError::NotCommitted {
                    stage: ExportFileStage::TempCreated,
                    source,
                });
            }
        }
    }
    Err(ExportFileError::NotCommitted {
        stage: ExportFileStage::TempCreated,
        source: io::Error::new(io::ErrorKind::AlreadyExists, "temporary-name collision"),
    })
}

#[cfg(any(target_vendor = "apple", target_os = "linux", target_os = "android"))]
fn rename_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    rustix::fs::renameat_with(
        rustix::fs::CWD,
        from,
        rustix::fs::CWD,
        to,
        rustix::fs::RenameFlags::NOREPLACE,
    )
    .map_err(io::Error::from)
}

#[cfg(not(any(target_vendor = "apple", target_os = "linux", target_os = "android")))]
fn rename_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    fs::hard_link(from, to)?;
    fs::remove_file(from)
}

fn sync_directory(directory: &Path) -> io::Result<()> {
    File::open(directory)?.sync_all()
}

fn precommit_fault(
    faults: &dyn ExportFileFaultInjector,
    stage: ExportFileStage,
    destination: &Path,
) -> Result<(), ExportFileError> {
    faults
        .check(stage, destination)
        .map_err(|source| ExportFileError::NotCommitted { stage, source })
}

fn postcommit_fault(
    faults: &dyn ExportFileFaultInjector,
    stage: ExportFileStage,
    destination: &Path,
) -> Result<(), ExportFileError> {
    faults
        .check(stage, destination)
        .map_err(|source| ExportFileError::CommittedDurabilityUnknown { stage, source })
}

struct TempCleanup {
    path: PathBuf,
    armed: bool,
}

impl TempCleanup {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for TempCleanup {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
        }
    }
}
