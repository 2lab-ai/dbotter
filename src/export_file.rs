use std::io;
use std::path::{Path, PathBuf};

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
    fn check(&self, stage: ExportFileStage, path: &Path) -> io::Result<()>;
}

#[derive(Debug, Default)]
pub struct NoExportFileFaults;

impl ExportFileFaultInjector for NoExportFileFaults {
    fn check(&self, _stage: ExportFileStage, _path: &Path) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Clone)]
pub struct ConfirmedDestination {
    path: PathBuf,
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
    #[error("export file support is not implemented")]
    Unavailable,
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
    #[error("export did not commit")]
    NotCommitted,
    #[error("export committed but directory durability is unknown")]
    CommittedDurabilityUnknown,
}

pub fn confirm_replace(_destination: &Path) -> Result<ConfirmedDestination, ExportFileError> {
    Err(ExportFileError::Unavailable)
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
    _request: &ExportResult,
    _confirmation: Option<&ConfirmedDestination>,
    _is_cancelled: F,
    _faults: &dyn ExportFileFaultInjector,
) -> Result<ExportFileOutcome, ExportFileError>
where
    F: FnMut() -> bool,
{
    Err(ExportFileError::Unavailable)
}
