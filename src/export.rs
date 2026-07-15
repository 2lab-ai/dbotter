use std::fmt::Write as _;
use std::io::{self, Write};

use base64::Engine as _;
use chrono::{DateTime, SecondsFormat, Utc};
use rust_decimal::Decimal;

use crate::model::{Cell, ExportFormat, MAX_RESULT_CELL_BYTES, ResultSnapshot};

const STREAM_CHUNK_BYTES: usize = 4 * 1024;

/// Maximum transient encoder allocation for one already-retained cell or
/// metadata value. JSON control escaping can expand it sixfold; the power-of-
/// two `String` growth ceiling is eightfold plus one streaming chunk.
pub const MAX_EXPORT_TRANSIENT_BYTES: usize = MAX_RESULT_CELL_BYTES
    .saturating_mul(8)
    .saturating_add(STREAM_CHUNK_BYTES);

#[derive(Debug, thiserror::Error)]
pub enum ExportEncodeError {
    #[error("export was cancelled")]
    Cancelled,
    #[error("export encoding failed")]
    Io(#[source] io::Error),
    #[error("CSV and TSV require a tabular result")]
    TabularResultRequired,
    #[error("result row does not match the exported schema")]
    InvalidRowWidth,
    #[error("result completion timestamp is outside the supported UTC range")]
    InvalidCompletedAt,
}

impl From<io::Error> for ExportEncodeError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// The exact scalar shared by clipboard, CSV, and TSV output.
pub fn clipboard_scalar(cell: &Cell) -> String {
    match cell {
        Cell::Null => String::new(),
        Cell::Bool(value) => value.to_string(),
        Cell::Int(value) => value.to_string(),
        Cell::UInt(value) => value.to_string(),
        Cell::Float(value) => canonical_float(*value),
        Cell::Decimal(value) => canonical_decimal(value),
        Cell::Text(value) => value.clone(),
        Cell::TextPreview {
            preview,
            original_len,
        } => format!("{preview}…[dbotter-truncated;original_len={original_len}]"),
        Cell::Bytes {
            retained,
            original_len,
        } => {
            let mut scalar = String::from("base64:");
            base64::engine::general_purpose::STANDARD.encode_string(retained, &mut scalar);
            if retained.len() < *original_len {
                let _ = write!(scalar, ";truncated=true;original_len={original_len}");
            }
            scalar
        }
        Cell::Json(value) => canonical_json(value),
        Cell::JsonPreview {
            preview,
            original_len,
        } => format!("json-preview:{preview};truncated=true;original_len={original_len}"),
        Cell::DateTime(value) => normalize_datetime(value),
    }
}

/// Escapes one TSV field character-wise without changing any other Unicode.
pub fn tsv_field(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '\t' => escaped.push_str("\\t"),
            '\r' => escaped.push_str("\\r"),
            '\n' => escaped.push_str("\\n"),
            _ => escaped.push(character),
        }
    }
    escaped
}

pub fn write_export<W: Write>(
    snapshot: &ResultSnapshot,
    format: ExportFormat,
    writer: &mut W,
) -> Result<(), ExportEncodeError> {
    write_export_with_cancel(snapshot, format, writer, || false)
}

pub fn write_export_with_cancel<W, F>(
    snapshot: &ResultSnapshot,
    format: ExportFormat,
    writer: &mut W,
    is_cancelled: F,
) -> Result<(), ExportEncodeError>
where
    W: Write,
    F: FnMut() -> bool,
{
    let mut writer = StreamingWriter::new(writer, is_cancelled);
    let result = match format {
        ExportFormat::Csv => write_csv(snapshot, &mut writer),
        ExportFormat::Tsv => write_tsv(snapshot, &mut writer),
        ExportFormat::Json => write_json(snapshot, &mut writer),
    };

    if writer.cancelled {
        Err(ExportEncodeError::Cancelled)
    } else {
        result
    }
}

struct StreamingWriter<'a, W, F> {
    inner: &'a mut W,
    is_cancelled: F,
    cancelled: bool,
}

impl<'a, W, F> StreamingWriter<'a, W, F> {
    fn new(inner: &'a mut W, is_cancelled: F) -> Self {
        Self {
            inner,
            is_cancelled,
            cancelled: false,
        }
    }
}

impl<W, F> Write for StreamingWriter<'_, W, F>
where
    W: Write,
    F: FnMut() -> bool,
{
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if (self.is_cancelled)() {
            self.cancelled = true;
            return Err(io::Error::other("export cancelled"));
        }
        let chunk_len = bytes.len().min(STREAM_CHUNK_BYTES);
        self.inner.write(&bytes[..chunk_len])
    }

    fn flush(&mut self) -> io::Result<()> {
        if (self.is_cancelled)() {
            self.cancelled = true;
            return Err(io::Error::other("export cancelled"));
        }
        self.inner.flush()
    }
}

fn write_csv<W: Write, F: FnMut() -> bool>(
    snapshot: &ResultSnapshot,
    writer: &mut StreamingWriter<'_, W, F>,
) -> Result<(), ExportEncodeError> {
    require_tabular(snapshot)?;
    for (index, column) in snapshot.columns.iter().enumerate() {
        if index != 0 {
            writer.write_all(b",")?;
        }
        write_csv_field(writer, &column.name)?;
    }
    writer.write_all(b"\r\n")?;

    for row in &snapshot.rows {
        require_row_width(snapshot, row)?;
        for (index, cell) in row.iter().enumerate() {
            if index != 0 {
                writer.write_all(b",")?;
            }
            write_csv_field(writer, &clipboard_scalar(cell))?;
        }
        writer.write_all(b"\r\n")?;
    }
    Ok(())
}

fn write_csv_field<W: Write, F: FnMut() -> bool>(
    writer: &mut StreamingWriter<'_, W, F>,
    field: &str,
) -> Result<(), ExportEncodeError> {
    let quoted = field
        .bytes()
        .any(|byte| matches!(byte, b',' | b'"' | b'\r' | b'\n'));
    if !quoted {
        writer.write_all(field.as_bytes())?;
        return Ok(());
    }

    writer.write_all(b"\"")?;
    let mut remainder = field;
    while let Some(index) = remainder.find('"') {
        writer.write_all(&remainder.as_bytes()[..index])?;
        writer.write_all(b"\"\"")?;
        remainder = &remainder[index + 1..];
    }
    writer.write_all(remainder.as_bytes())?;
    writer.write_all(b"\"")?;
    Ok(())
}

fn write_tsv<W: Write, F: FnMut() -> bool>(
    snapshot: &ResultSnapshot,
    writer: &mut StreamingWriter<'_, W, F>,
) -> Result<(), ExportEncodeError> {
    require_tabular(snapshot)?;
    for (index, column) in snapshot.columns.iter().enumerate() {
        if index != 0 {
            writer.write_all(b"\t")?;
        }
        writer.write_all(tsv_field(&column.name).as_bytes())?;
    }
    writer.write_all(b"\n")?;

    for row in &snapshot.rows {
        require_row_width(snapshot, row)?;
        for (index, cell) in row.iter().enumerate() {
            if index != 0 {
                writer.write_all(b"\t")?;
            }
            writer.write_all(tsv_field(&clipboard_scalar(cell)).as_bytes())?;
        }
        writer.write_all(b"\n")?;
    }
    Ok(())
}

fn require_tabular(snapshot: &ResultSnapshot) -> Result<(), ExportEncodeError> {
    if snapshot.columns.is_empty() {
        Err(ExportEncodeError::TabularResultRequired)
    } else {
        Ok(())
    }
}

fn require_row_width(snapshot: &ResultSnapshot, row: &[Cell]) -> Result<(), ExportEncodeError> {
    if row.len() == snapshot.columns.len() {
        Ok(())
    } else {
        Err(ExportEncodeError::InvalidRowWidth)
    }
}

fn write_json<W: Write, F: FnMut() -> bool>(
    snapshot: &ResultSnapshot,
    writer: &mut StreamingWriter<'_, W, F>,
) -> Result<(), ExportEncodeError> {
    let completed_at =
        DateTime::<Utc>::from_timestamp_millis(snapshot.provenance.completed_at_unix_ms)
            .ok_or(ExportEncodeError::InvalidCompletedAt)?
            .to_rfc3339_opts(SecondsFormat::Millis, true);

    writer.write_all(b"{\"schema\":\"dbotter.result.v1\",\"provenance\":{\"operation_id\":")?;
    write_display(writer, snapshot.provenance.operation_id.0)?;
    writer.write_all(b",\"profile_id\":")?;
    write_json_string(writer, snapshot.provenance.profile_id.as_str())?;
    writer.write_all(b",\"profile_generation\":")?;
    write_display(writer, snapshot.provenance.profile_generation.0)?;
    writer.write_all(b",\"driver\":")?;
    write_json_string(writer, &snapshot.provenance.driver.to_string())?;
    writer.write_all(b",\"completed_at\":")?;
    write_json_string(writer, &completed_at)?;
    writer.write_all(b",\"elapsed_ms\":")?;
    write_display(writer, snapshot.provenance.duration_ms)?;
    writer.write_all(b"},\"columns\":[")?;

    for (index, column) in snapshot.columns.iter().enumerate() {
        if index != 0 {
            writer.write_all(b",")?;
        }
        writer.write_all(b"{\"index\":")?;
        write_display(writer, index)?;
        writer.write_all(b",\"name\":")?;
        write_json_string(writer, &column.name)?;
        writer.write_all(b",\"type_name\":")?;
        write_json_string(writer, &column.type_name)?;
        writer.write_all(b"}")?;
    }
    writer.write_all(b"],\"rows\":[")?;

    for (row_index, row) in snapshot.rows.iter().enumerate() {
        require_row_width(snapshot, row)?;
        if row_index != 0 {
            writer.write_all(b",")?;
        }
        writer.write_all(b"[")?;
        for (cell_index, cell) in row.iter().enumerate() {
            if cell_index != 0 {
                writer.write_all(b",")?;
            }
            write_json_cell(writer, cell)?;
        }
        writer.write_all(b"]")?;
    }

    writer.write_all(b"],\"affected_rows\":")?;
    write_display(writer, snapshot.affected_rows)?;
    writer.write_all(b",\"last_insert_id\":")?;
    match snapshot.last_insert_id {
        Some(value) => write_display(writer, value)?,
        None => writer.write_all(b"null")?,
    }
    writer.write_all(b",\"truncated\":")?;
    writer.write_all(if snapshot.truncated {
        b"true"
    } else {
        b"false"
    })?;
    writer.write_all(b"}")?;
    Ok(())
}

fn write_json_cell<W: Write, F: FnMut() -> bool>(
    writer: &mut StreamingWriter<'_, W, F>,
    cell: &Cell,
) -> Result<(), ExportEncodeError> {
    match cell {
        Cell::Null => writer.write_all(b"{\"type\":\"null\"}")?,
        Cell::Bool(value) => {
            writer.write_all(b"{\"type\":\"bool\",\"value\":")?;
            writer.write_all(if *value { b"true" } else { b"false" })?;
            writer.write_all(b"}")?;
        }
        Cell::Int(value) => write_string_cell(writer, "int", &value.to_string())?,
        Cell::UInt(value) => write_string_cell(writer, "uint", &value.to_string())?,
        Cell::Float(value) => write_string_cell(writer, "float", &canonical_float(*value))?,
        Cell::Decimal(value) => write_string_cell(writer, "decimal", &canonical_decimal(value))?,
        Cell::Text(value) => write_string_cell(writer, "text", value)?,
        Cell::TextPreview {
            preview,
            original_len,
        } => {
            writer.write_all(b"{\"type\":\"text\",\"value\":")?;
            write_json_string(writer, preview)?;
            writer.write_all(b",\"original_len\":")?;
            write_display(writer, original_len)?;
            writer.write_all(b",\"truncated\":true}")?;
        }
        Cell::Bytes {
            retained,
            original_len,
        } => {
            writer.write_all(b"{\"type\":\"bytes\",\"value\":{\"base64\":")?;
            let encoded = base64::engine::general_purpose::STANDARD.encode(retained);
            write_json_string(writer, &encoded)?;
            writer.write_all(b",\"original_len\":")?;
            write_display(writer, original_len)?;
            writer.write_all(b",\"truncated\":")?;
            writer.write_all(if retained.len() < *original_len {
                b"true"
            } else {
                b"false"
            })?;
            writer.write_all(b"}}")?;
        }
        Cell::Json(value) => {
            writer.write_all(b"{\"type\":\"json\",\"value\":")?;
            writer.write_all(canonical_json(value).as_bytes())?;
            writer.write_all(b"}")?;
        }
        Cell::JsonPreview {
            preview,
            original_len,
        } => {
            writer.write_all(b"{\"type\":\"json_preview\",\"value\":")?;
            write_json_string(writer, preview)?;
            writer.write_all(b",\"original_len\":")?;
            write_display(writer, original_len)?;
            writer.write_all(b",\"truncated\":true}")?;
        }
        Cell::DateTime(value) => {
            write_string_cell(writer, "datetime", &normalize_datetime(value))?;
        }
    }
    Ok(())
}

fn write_string_cell<W: Write, F: FnMut() -> bool>(
    writer: &mut StreamingWriter<'_, W, F>,
    cell_type: &str,
    value: &str,
) -> Result<(), ExportEncodeError> {
    writer.write_all(b"{\"type\":")?;
    write_json_string(writer, cell_type)?;
    writer.write_all(b",\"value\":")?;
    write_json_string(writer, value)?;
    writer.write_all(b"}")?;
    Ok(())
}

fn write_display<W, F, T>(
    writer: &mut StreamingWriter<'_, W, F>,
    value: T,
) -> Result<(), ExportEncodeError>
where
    W: Write,
    F: FnMut() -> bool,
    T: std::fmt::Display,
{
    writer.write_all(value.to_string().as_bytes())?;
    Ok(())
}

fn write_json_string<W: Write, F: FnMut() -> bool>(
    writer: &mut StreamingWriter<'_, W, F>,
    value: &str,
) -> Result<(), ExportEncodeError> {
    let mut encoded = String::with_capacity(value.len().saturating_add(2));
    push_json_string(&mut encoded, value);
    writer.write_all(encoded.as_bytes())?;
    Ok(())
}

fn canonical_float(value: f64) -> String {
    if value.is_nan() {
        "nan".to_owned()
    } else if value == f64::INFINITY {
        "inf".to_owned()
    } else if value == f64::NEG_INFINITY {
        "-inf".to_owned()
    } else {
        value.to_string()
    }
}

fn canonical_decimal(value: &str) -> String {
    value.parse::<Decimal>().map_or_else(
        |_| value.to_owned(),
        |decimal| decimal.normalize().to_string(),
    )
}

fn normalize_datetime(value: &str) -> String {
    DateTime::parse_from_rfc3339(value).map_or_else(
        |_| value.to_owned(),
        |datetime| {
            datetime
                .with_timezone(&Utc)
                .to_rfc3339_opts(SecondsFormat::AutoSi, true)
        },
    )
}

fn canonical_json(value: &serde_json::Value) -> String {
    let mut output = String::new();
    push_canonical_json(&mut output, value);
    output
}

fn push_canonical_json(output: &mut String, value: &serde_json::Value) {
    match value {
        serde_json::Value::Null => output.push_str("null"),
        serde_json::Value::Bool(value) => {
            output.push_str(if *value { "true" } else { "false" });
        }
        serde_json::Value::Number(value) => {
            let _ = write!(output, "{value}");
        }
        serde_json::Value::String(value) => push_json_string(output, value),
        serde_json::Value::Array(values) => {
            output.push('[');
            for (index, value) in values.iter().enumerate() {
                if index != 0 {
                    output.push(',');
                }
                push_canonical_json(output, value);
            }
            output.push(']');
        }
        serde_json::Value::Object(values) => {
            output.push('{');
            let mut fields: Vec<_> = values.iter().collect();
            fields.sort_unstable_by_key(|(key, _)| *key);
            for (index, (key, value)) in fields.into_iter().enumerate() {
                if index != 0 {
                    output.push(',');
                }
                push_json_string(output, key);
                output.push(':');
                push_canonical_json(output, value);
            }
            output.push('}');
        }
    }
}

fn push_json_string(output: &mut String, value: &str) {
    output.push('"');
    for character in value.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\u{08}' => output.push_str("\\b"),
            '\u{0c}' => output.push_str("\\f"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            character if character <= '\u{1f}' => {
                let _ = write!(output, "\\u{:04x}", character as u32);
            }
            _ => output.push(character),
        }
    }
    output.push('"');
}
