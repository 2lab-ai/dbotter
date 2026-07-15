use std::alloc::{GlobalAlloc, Layout, System};
use std::io::{self, Write};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use dbotter::export::{
    ExportEncodeError, MAX_EXPORT_TRANSIENT_BYTES, write_export, write_export_with_cancel,
};
use dbotter::model::{
    Cell, Column, DriverKind, ExportFormat, MAX_RESULT_CELL_BYTES, OperationId, ProfileGeneration,
    ProfileId, ResultId, ResultProvenance, ResultSnapshot, TransientAllocationQualification,
};

struct TrackingAllocator;

static OUTSTANDING: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static MEASURING: AtomicBool = AtomicBool::new(false);
static TEST_SERIAL: Mutex<()> = Mutex::new(());

fn record_allocation(size: usize) {
    let outstanding = OUTSTANDING.fetch_add(size, Ordering::SeqCst) + size;
    if MEASURING.load(Ordering::SeqCst) {
        let mut peak = PEAK.load(Ordering::SeqCst);
        while outstanding > peak {
            match PEAK.compare_exchange(peak, outstanding, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => break,
                Err(observed) => peak = observed,
            }
        }
    }
}

unsafe impl GlobalAlloc for TrackingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            record_allocation(layout.size());
        }
        pointer
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc_zeroed(layout) };
        if !pointer.is_null() {
            record_allocation(layout.size());
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) };
        OUTSTANDING.fetch_sub(layout.size(), Ordering::SeqCst);
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new_pointer = unsafe { System.realloc(pointer, layout, new_size) };
        if !new_pointer.is_null() {
            if new_size >= layout.size() {
                record_allocation(new_size - layout.size());
            } else {
                OUTSTANDING.fetch_sub(layout.size() - new_size, Ordering::SeqCst);
            }
        }
        new_pointer
    }
}

#[global_allocator]
static ALLOCATOR: TrackingAllocator = TrackingAllocator;

fn snapshot(rows: Vec<Vec<Cell>>) -> ResultSnapshot {
    let column_count = rows.first().map_or(1, Vec::len);
    ResultSnapshot {
        provenance: ResultProvenance {
            result_id: ResultId(71),
            profile_id: ProfileId("profile-allocation".to_owned()),
            profile_generation: ProfileGeneration(7),
            operation_id: OperationId(17),
            driver: DriverKind::MySql,
            completed_at_unix_ms: 1_700_000_000_123,
            duration_ms: 19,
        },
        columns: (0..column_count)
            .map(|index| Column {
                name: format!("column-{index}"),
                type_name: "VALUE".to_owned(),
            })
            .collect(),
        rows,
        affected_rows: 0,
        last_insert_id: None,
        truncated: false,
        notices: Vec::new(),
        retained_bytes: 0,
        transient_allocation: TransientAllocationQualification::MySqlCurrentRowOrCell,
        cell_truncations: Vec::new(),
    }
}

fn peak_while(encode: impl FnOnce()) -> usize {
    let baseline = OUTSTANDING.load(Ordering::SeqCst);
    PEAK.store(baseline, Ordering::SeqCst);
    MEASURING.store(true, Ordering::SeqCst);
    encode();
    MEASURING.store(false, Ordering::SeqCst);
    PEAK.load(Ordering::SeqCst).saturating_sub(baseline)
}

fn measured_peak(snapshot: &ResultSnapshot, format: ExportFormat) -> usize {
    peak_while(|| {
        let mut sink = io::sink();
        write_export(snapshot, format, &mut sink).expect("measured export");
    })
}

#[test]
fn transient_allocation_is_one_retained_value_and_never_scales_with_row_count() {
    let _serial = TEST_SERIAL
        .lock()
        .expect("serialize allocator measurements");
    let single_large = snapshot(vec![vec![
        Cell::Text("\u{1}".repeat(MAX_RESULT_CELL_BYTES)),
        Cell::Bytes {
            retained: vec![0xff; MAX_RESULT_CELL_BYTES],
            original_len: MAX_RESULT_CELL_BYTES,
        },
        Cell::JsonPreview {
            preview: "\u{1}".repeat(MAX_RESULT_CELL_BYTES),
            original_len: MAX_RESULT_CELL_BYTES + 1,
        },
    ]]);
    for format in [ExportFormat::Csv, ExportFormat::Tsv, ExportFormat::Json] {
        let peak = measured_peak(&single_large, format);
        assert!(
            peak <= MAX_EXPORT_TRANSIENT_BYTES,
            "{format:?} peak {peak} exceeded {MAX_EXPORT_TRANSIENT_BYTES}"
        );
    }

    let mut maximum_metadata = snapshot(Vec::new());
    maximum_metadata.columns[0].name =
        "\u{1}".repeat(MAX_RESULT_CELL_BYTES.saturating_sub("VALUE".len()));
    let metadata_peak = measured_peak(&maximum_metadata, ExportFormat::Json);
    assert!(
        metadata_peak <= MAX_EXPORT_TRANSIENT_BYTES,
        "metadata peak {metadata_peak} exceeded {MAX_EXPORT_TRANSIENT_BYTES}"
    );

    let one_row = snapshot(vec![vec![Cell::Text("x".repeat(1024))]]);
    let many_rows = snapshot(
        (0..4_000)
            .map(|_| vec![Cell::Text("x".repeat(1024))])
            .collect(),
    );
    for format in [ExportFormat::Csv, ExportFormat::Tsv, ExportFormat::Json] {
        let one_peak = measured_peak(&one_row, format);
        let many_peak = measured_peak(&many_rows, format);
        assert!(
            many_peak <= one_peak.saturating_add(128 * 1024),
            "{format:?} allocation scaled with rows: one={one_peak}, many={many_peak}"
        );
    }
}

struct AlwaysFails;

impl Write for AlwaysFails {
    fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
        Err(io::Error::other("injected writer failure"))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn writer_io_failure_and_cooperative_cancellation_remain_distinct() {
    let _serial = TEST_SERIAL.lock().expect("serialize allocator tests");
    let result = snapshot(vec![vec![Cell::Text("value".to_owned())]]);
    let io_error = write_export_with_cancel(&result, ExportFormat::Csv, &mut AlwaysFails, || false)
        .expect_err("writer failure");
    assert!(matches!(io_error, ExportEncodeError::Io(_)));

    let cancel_error =
        write_export_with_cancel(&result, ExportFormat::Csv, &mut AlwaysFails, || true)
            .expect_err("cancellation");
    assert!(matches!(cancel_error, ExportEncodeError::Cancelled));
}
