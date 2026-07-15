use std::io::Write;

use crate::model::{Cell, ExportFormat, ResultSnapshot};

#[derive(Debug, thiserror::Error)]
pub enum ExportEncodeError {
    #[error("export encoding is not implemented")]
    EncodingUnavailable,
    #[error("export was cancelled")]
    Cancelled,
}

pub fn clipboard_scalar(_cell: &Cell) -> String {
    String::new()
}

pub fn tsv_field(_value: &str) -> String {
    String::new()
}

pub fn write_export<W: Write>(
    _snapshot: &ResultSnapshot,
    _format: ExportFormat,
    _writer: &mut W,
) -> Result<(), ExportEncodeError> {
    Err(ExportEncodeError::EncodingUnavailable)
}

pub fn write_export_with_cancel<W, F>(
    _snapshot: &ResultSnapshot,
    _format: ExportFormat,
    _writer: &mut W,
    _is_cancelled: F,
) -> Result<(), ExportEncodeError>
where
    W: Write,
    F: FnMut() -> bool,
{
    Err(ExportEncodeError::EncodingUnavailable)
}
