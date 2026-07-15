use std::io::{self, Write};

use dbotter::export::{
    ExportEncodeError, clipboard_scalar, tsv_field, write_export, write_export_with_cancel,
};
use dbotter::model::{
    Cell, Column, DriverKind, ExportFormat, OperationId, ProfileGeneration, ProfileId, ResultId,
    ResultProvenance, ResultSnapshot, TransientAllocationQualification,
};

fn snapshot(columns: Vec<Column>, rows: Vec<Vec<Cell>>) -> ResultSnapshot {
    ResultSnapshot {
        provenance: ResultProvenance {
            result_id: ResultId(71),
            profile_id: ProfileId("profile-export".to_owned()),
            profile_generation: ProfileGeneration(7),
            operation_id: OperationId(17),
            driver: DriverKind::Redis,
            completed_at_unix_ms: 1_700_000_000_123,
            duration_ms: 19,
        },
        columns,
        rows,
        affected_rows: 0,
        last_insert_id: None,
        truncated: false,
        notices: Vec::new(),
        retained_bytes: 0,
        transient_allocation: TransientAllocationQualification::RedisWholeRespFrame,
        cell_truncations: Vec::new(),
    }
}

fn column(name: &str, type_name: &str) -> Column {
    Column {
        name: name.to_owned(),
        type_name: type_name.to_owned(),
    }
}

fn encode(snapshot: &ResultSnapshot, format: ExportFormat) -> Vec<u8> {
    let mut output = Vec::new();
    write_export(snapshot, format, &mut output).expect("encode golden export");
    output
}

#[test]
fn clipboard_scalar_and_tsv_field_cover_every_cell_and_control_character() {
    let cases = [
        (Cell::Null, ""),
        (Cell::Text("a\t\r\n\\한글".to_owned()), "a\t\r\n\\한글"),
        (
            Cell::TextPreview {
                preview: "앞".to_owned(),
                original_len: 9,
            },
            "앞…[dbotter-truncated;original_len=9]",
        ),
        (Cell::Bool(true), "true"),
        (Cell::Bool(false), "false"),
        (Cell::Int(-42), "-42"),
        (Cell::UInt(42), "42"),
        (Cell::Decimal("12.500".to_owned()), "12.5"),
        (Cell::Float(1.25), "1.25"),
        (Cell::Float(f64::NAN), "nan"),
        (Cell::Float(f64::INFINITY), "inf"),
        (Cell::Float(f64::NEG_INFINITY), "-inf"),
        (
            Cell::DateTime("2026-07-15T01:02:03.004Z".to_owned()),
            "2026-07-15T01:02:03.004Z",
        ),
        (
            Cell::Json(serde_json::json!({"z": 1, "a": {"d": 4, "b": 2}})),
            "{\"a\":{\"b\":2,\"d\":4},\"z\":1}",
        ),
        (
            Cell::JsonPreview {
                preview: "{\"a\":\"한".to_owned(),
                original_len: 99,
            },
            "json-preview:{\"a\":\"한;truncated=true;original_len=99",
        ),
        (
            Cell::Bytes {
                retained: vec![0, 0xff],
                original_len: 2,
            },
            "base64:AP8=",
        ),
        (
            Cell::Bytes {
                retained: vec![0, 1],
                original_len: 4,
            },
            "base64:AAE=;truncated=true;original_len=4",
        ),
    ];

    for (cell, expected) in cases {
        assert_eq!(clipboard_scalar(&cell), expected);
    }
    assert_eq!(tsv_field("a\\b\tc\rd\ne한"), "a\\\\b\\tc\\rd\\ne한");
}

#[test]
fn mysql_naive_date_time_datetime_and_rfc3339_offsets_share_normalized_iso_wire_text() {
    let date = Cell::DateTime("2016-11-15".to_owned());
    let time = Cell::DateTime("07:39:24.123456".to_owned());
    let naive = Cell::DateTime("2016-11-15 07:39:24.123456".to_owned());
    let offset = Cell::DateTime("2016-11-15T07:39:24.123456+02:00".to_owned());

    assert_eq!(clipboard_scalar(&date), "2016-11-15");
    assert_eq!(clipboard_scalar(&time), "07:39:24.123456");
    assert_eq!(clipboard_scalar(&offset), "2016-11-15T05:39:24.123456Z");
    assert_eq!(clipboard_scalar(&naive), "2016-11-15T07:39:24.123456");

    let result = snapshot(
        vec![column("naive", "DATETIME"), column("offset", "TIMESTAMP")],
        vec![vec![naive, offset]],
    );
    assert_eq!(
        encode(&result, ExportFormat::Csv),
        b"naive,offset\r\n2016-11-15T07:39:24.123456,2016-11-15T05:39:24.123456Z\r\n"
    );
    let json = String::from_utf8(encode(&result, ExportFormat::Json)).expect("UTF-8 JSON export");
    assert!(json.contains("{\"type\":\"datetime\",\"value\":\"2016-11-15T07:39:24.123456\"}"));
    assert!(json.contains("{\"type\":\"datetime\",\"value\":\"2016-11-15T05:39:24.123456Z\"}"));
}

#[test]
fn csv_and_tsv_are_exact_for_duplicate_headers_null_unicode_controls_and_zero_rows() {
    let columns = vec![
        column("dup", "NULL"),
        column("dup", "TEXT"),
        column("controls", "TEXT"),
    ];
    let populated = snapshot(
        columns.clone(),
        vec![vec![
            Cell::Null,
            Cell::Text("한,글".to_owned()),
            Cell::Text("a\tb\r\nc\\d\"".to_owned()),
        ]],
    );

    let csv = encode(&populated, ExportFormat::Csv);
    assert_eq!(
        csv,
        b"dup,dup,controls\r\n,\"\xed\x95\x9c,\xea\xb8\x80\",\"a\tb\r\nc\\d\"\"\"\r\n"
    );
    assert!(!csv.starts_with(&[0xef, 0xbb, 0xbf]));

    let tsv = encode(&populated, ExportFormat::Tsv);
    assert_eq!(
        tsv,
        "dup\tdup\tcontrols\n\t한,글\ta\\tb\\r\\nc\\\\d\"\n".as_bytes()
    );
    assert!(tsv.ends_with(b"\n"));
    assert!(!tsv.ends_with(b"\n\n"));

    let empty = snapshot(columns, Vec::new());
    assert_eq!(encode(&empty, ExportFormat::Csv), b"dup,dup,controls\r\n");
    assert_eq!(encode(&empty, ExportFormat::Tsv), b"dup\tdup\tcontrols\n");
}

#[test]
fn canonical_json_has_exact_order_all_cell_shapes_and_no_final_newline() {
    let columns = vec![
        column("dup", "NULL"),
        column("dup", "TEXT"),
        column("text_preview", "TEXT"),
        column("bool", "BOOL"),
        column("int", "INT"),
        column("uint", "UINT"),
        column("decimal", "DECIMAL"),
        column("float", "FLOAT"),
        column("datetime", "DATETIME"),
        column("json", "JSON"),
        column("json_preview", "JSON"),
        column("bytes", "BYTES"),
        column("bytes_preview", "BYTES"),
    ];
    let result = snapshot(
        columns,
        vec![vec![
            Cell::Null,
            Cell::Text("한\n글".to_owned()),
            Cell::TextPreview {
                preview: "앞".to_owned(),
                original_len: 9,
            },
            Cell::Bool(true),
            Cell::Int(-42),
            Cell::UInt(42),
            Cell::Decimal("12.500".to_owned()),
            Cell::Float(f64::NEG_INFINITY),
            Cell::DateTime("2026-07-15T01:02:03.004Z".to_owned()),
            Cell::Json(serde_json::json!({"z": 1, "a": {"d": 4, "b": 2}})),
            Cell::JsonPreview {
                preview: "{\"a\":\"한".to_owned(),
                original_len: 99,
            },
            Cell::Bytes {
                retained: vec![0, 0xff],
                original_len: 2,
            },
            Cell::Bytes {
                retained: vec![0, 1],
                original_len: 4,
            },
        ]],
    );

    let json = encode(&result, ExportFormat::Json);
    let expected = concat!(
        "{\"schema\":\"dbotter.result.v1\",",
        "\"provenance\":{\"operation_id\":17,\"profile_id\":\"profile-export\",",
        "\"profile_generation\":7,\"driver\":\"redis\",",
        "\"completed_at\":\"2023-11-14T22:13:20.123Z\",\"elapsed_ms\":19},",
        "\"columns\":[",
        "{\"index\":0,\"name\":\"dup\",\"type_name\":\"NULL\"},",
        "{\"index\":1,\"name\":\"dup\",\"type_name\":\"TEXT\"},",
        "{\"index\":2,\"name\":\"text_preview\",\"type_name\":\"TEXT\"},",
        "{\"index\":3,\"name\":\"bool\",\"type_name\":\"BOOL\"},",
        "{\"index\":4,\"name\":\"int\",\"type_name\":\"INT\"},",
        "{\"index\":5,\"name\":\"uint\",\"type_name\":\"UINT\"},",
        "{\"index\":6,\"name\":\"decimal\",\"type_name\":\"DECIMAL\"},",
        "{\"index\":7,\"name\":\"float\",\"type_name\":\"FLOAT\"},",
        "{\"index\":8,\"name\":\"datetime\",\"type_name\":\"DATETIME\"},",
        "{\"index\":9,\"name\":\"json\",\"type_name\":\"JSON\"},",
        "{\"index\":10,\"name\":\"json_preview\",\"type_name\":\"JSON\"},",
        "{\"index\":11,\"name\":\"bytes\",\"type_name\":\"BYTES\"},",
        "{\"index\":12,\"name\":\"bytes_preview\",\"type_name\":\"BYTES\"}],",
        "\"rows\":[[{\"type\":\"null\"},",
        "{\"type\":\"text\",\"value\":\"한\\n글\"},",
        "{\"type\":\"text\",\"value\":\"앞\",\"original_len\":9,\"truncated\":true},",
        "{\"type\":\"bool\",\"value\":true},",
        "{\"type\":\"int\",\"value\":\"-42\"},",
        "{\"type\":\"uint\",\"value\":\"42\"},",
        "{\"type\":\"decimal\",\"value\":\"12.5\"},",
        "{\"type\":\"float\",\"value\":\"-inf\"},",
        "{\"type\":\"datetime\",\"value\":\"2026-07-15T01:02:03.004Z\"},",
        "{\"type\":\"json\",\"value\":{\"a\":{\"b\":2,\"d\":4},\"z\":1}},",
        "{\"type\":\"json_preview\",\"value\":\"{\\\"a\\\":\\\"한\",\"original_len\":99,\"truncated\":true},",
        "{\"type\":\"bytes\",\"value\":{\"base64\":\"AP8=\",\"original_len\":2,\"truncated\":false}},",
        "{\"type\":\"bytes\",\"value\":{\"base64\":\"AAE=\",\"original_len\":4,\"truncated\":true}}]],",
        "\"affected_rows\":0,\"last_insert_id\":null,\"truncated\":false}"
    );
    assert_eq!(json, expected.as_bytes());
    assert!(!json.ends_with(b"\n"));
}

#[test]
fn mutation_only_result_is_json_only_and_preserves_counts() {
    let mut result = snapshot(Vec::new(), Vec::new());
    result.affected_rows = 7;
    result.last_insert_id = Some(8);

    let mut csv = Vec::new();
    assert!(matches!(
        write_export(&result, ExportFormat::Csv, &mut csv),
        Err(ExportEncodeError::TabularResultRequired)
    ));
    let mut tsv = Vec::new();
    assert!(matches!(
        write_export(&result, ExportFormat::Tsv, &mut tsv),
        Err(ExportEncodeError::TabularResultRequired)
    ));
    assert!(csv.is_empty());
    assert!(tsv.is_empty());

    let json = encode(&result, ExportFormat::Json);
    assert_eq!(
        json,
        concat!(
            "{\"schema\":\"dbotter.result.v1\",",
            "\"provenance\":{\"operation_id\":17,\"profile_id\":\"profile-export\",",
            "\"profile_generation\":7,\"driver\":\"redis\",",
            "\"completed_at\":\"2023-11-14T22:13:20.123Z\",\"elapsed_ms\":19},",
            "\"columns\":[],\"rows\":[],\"affected_rows\":7,",
            "\"last_insert_id\":8,\"truncated\":false}"
        )
        .as_bytes()
    );
}

struct BoundedWrite {
    bytes: usize,
    largest_write: usize,
    hard_limit: usize,
}

impl Write for BoundedWrite {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.largest_write = self.largest_write.max(buffer.len());
        if buffer.len() > self.hard_limit {
            return Err(io::Error::other("whole-result or oversized write detected"));
        }
        self.bytes += buffer.len();
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn encoder_streams_in_bounded_writes_and_checks_cancellation_between_chunks() {
    let result = snapshot(
        vec![column("payload", "TEXT")],
        (0..128)
            .map(|_| vec![Cell::Text("x".repeat(32 * 1024))])
            .collect(),
    );
    let mut writer = BoundedWrite {
        bytes: 0,
        largest_write: 0,
        hard_limit: 8 * 1024,
    };
    write_export(&result, ExportFormat::Csv, &mut writer).expect("stream bounded CSV");
    assert!(writer.bytes > 4 * 1024 * 1024);
    assert!(writer.largest_write <= writer.hard_limit);

    let mut cancelled_writer = Vec::new();
    let mut checks = 0_usize;
    let error = write_export_with_cancel(&result, ExportFormat::Tsv, &mut cancelled_writer, || {
        checks += 1;
        checks > 3
    })
    .expect_err("cooperative cancellation must stop encoding");
    assert!(matches!(error, ExportEncodeError::Cancelled));
    assert!(cancelled_writer.len() < writer.bytes);
}
