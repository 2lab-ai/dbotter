use std::time::{Duration, Instant};

use chrono::{NaiveDate, NaiveDateTime, NaiveTime};
use futures_util::TryStreamExt as _;
use rust_decimal::Decimal;
use secrecy::{ExposeSecret as _, SecretString};
use sqlx::mysql::{
    MySqlConnectOptions, MySqlConnection, MySqlDatabaseError, MySqlPoolOptions, MySqlRow,
    MySqlSslMode,
};
use sqlx::{
    Column as _, Connection as _, Either, Executor as _, Row as _, SqlSafeStr as _, Statement as _,
    TypeInfo as _, ValueRef as _,
};

use crate::drivers::{DriverError, mysql_catalog};
use crate::model::{
    CatalogPage, CatalogRequest, Cell, Column, ConnectionProfile, DriverAvailability,
    DriverCapabilities, DriverDescriptor, DriverKind, MAX_RESULT_BYTES, MAX_RESULT_CELL_BYTES,
    MAX_RESULT_COLUMNS, PreparedMySqlRequest, QueryLanguage, QueryResult, TlsMode,
};

const ER_UNSUPPORTED_PS: u16 = 1295;

pub const DESCRIPTOR: DriverDescriptor = DriverDescriptor {
    kind: DriverKind::MySql,
    display_name: "MySQL",
    default_port: 3306,
    availability: DriverAvailability::Ready,
    languages: &[QueryLanguage::Sql],
    capabilities: DriverCapabilities::CONNECT
        .union(DriverCapabilities::PING)
        .union(DriverCapabilities::SQL)
        .union(DriverCapabilities::CATALOG),
    planned_capabilities: DriverCapabilities::empty(),
    reason: None,
};

#[derive(Clone)]
pub struct MySqlSession {
    pool: sqlx::MySqlPool,
    configured_database: Option<String>,
}

impl MySqlSession {
    pub async fn connect(
        profile: &ConnectionProfile,
        secret: Option<&SecretString>,
        timeout: Duration,
    ) -> Result<Self, DriverError> {
        let mut options = MySqlConnectOptions::new()
            .host(&profile.host)
            .port(profile.port);
        if let Some(username) = profile.username.as_deref() {
            options = options.username(username);
        }
        if let Some(database) = profile.database.as_deref() {
            options = options.database(database);
        }
        if let Some(password) = secret {
            options = options.password(password.expose_secret());
        }
        options = options.ssl_mode(match profile.tls {
            TlsMode::Disabled => MySqlSslMode::Disabled,
            TlsMode::Preferred => MySqlSslMode::Preferred,
            TlsMode::Required => MySqlSslMode::Required,
        });

        let started = Instant::now();
        let authentication_probe = timed(timeout, MySqlConnection::connect_with(&options)).await?;
        drop(authentication_probe);
        let remaining = timeout
            .checked_sub(started.elapsed())
            .filter(|remaining| !remaining.is_zero())
            .ok_or(DriverError::Timeout {
                driver: DriverKind::MySql,
                seconds: timeout.as_secs(),
            })?;
        let pool = timed(
            remaining,
            MySqlPoolOptions::new()
                .max_connections(4)
                .acquire_timeout(remaining)
                .connect_with(options),
        )
        .await?;
        Ok(Self {
            pool,
            configured_database: profile.database.clone(),
        })
    }

    pub async fn ping(&self, timeout: Duration) -> Result<(), DriverError> {
        timed(timeout, sqlx::query("SELECT 1").execute(&self.pool))
            .await
            .map(|_| ())
    }

    pub async fn close(&self) {
        self.pool.close().await;
    }

    pub async fn execute_prepared(
        &self,
        request: &PreparedMySqlRequest,
    ) -> Result<QueryResult, DriverError> {
        let started = Instant::now();
        let result = tokio::time::timeout(
            request.timeout,
            self.execute_one(&request.statement, request.row_limit),
        )
        .await
        .map_err(|_| DriverError::Timeout {
            driver: DriverKind::MySql,
            seconds: request.timeout.as_secs(),
        })??;
        Ok(QueryResult {
            elapsed_ms: started.elapsed().as_millis(),
            ..result
        })
    }

    pub async fn load_page(
        &self,
        request: &CatalogRequest,
        token_key: &mysql_catalog::CatalogTokenKey,
    ) -> Result<CatalogPage, DriverError> {
        mysql_catalog::load_page(
            &self.pool,
            self.configured_database.as_deref(),
            request,
            token_key,
        )
        .await
    }

    async fn execute_one(&self, text: &str, row_limit: u32) -> Result<QueryResult, DriverError> {
        let limit = row_limit as usize;
        let mut connection = self.pool.acquire().await.map_err(DriverError::from)?;

        // Every caller-supplied statement crosses the wire through the server
        // prepared protocol. Failure to prepare is terminal for this request.
        let statement = (&mut *connection)
            .prepare(sqlx::AssertSqlSafe(text.to_owned()).into_sql_str())
            .await
            .map_err(map_prepare_error)?;
        let mut columns = statement
            .columns()
            .iter()
            .map(|column| Column {
                name: column.name().to_owned(),
                type_name: column.type_info().name().to_owned(),
            })
            .collect::<Vec<_>>();
        let mut is_result_set = !columns.is_empty();

        let query = statement.query();
        let mut stream = (&mut *connection).fetch_many(query);
        let mut rows = Vec::new();
        let mut decoded_budget = DecodedRowBudget::default();
        decoded_budget.add_columns(&columns);
        let mut affected_rows = 0;
        let mut last_insert_id = None;
        let mut truncated = false;

        while let Some(step) = stream.try_next().await.map_err(DriverError::from)? {
            match step {
                Either::Right(row) => {
                    is_result_set = true;
                    if columns.is_empty() {
                        columns = row
                            .columns()
                            .iter()
                            .map(|column| Column {
                                name: column.name().to_owned(),
                                type_name: column.type_info().name().to_owned(),
                            })
                            .collect();
                        decoded_budget.add_columns(&columns);
                    }
                    if rows.len() == limit {
                        truncated = true;
                        break;
                    }
                    let decoded = decode_row(&row).map_err(DriverError::from)?;
                    if decoded_budget.push_row(&mut rows, decoded) {
                        truncated = true;
                        break;
                    }
                }
                Either::Left(result) => {
                    affected_rows = result.rows_affected();
                    last_insert_id = Some(result.last_insert_id()).filter(|id| *id != 0);
                }
            }
        }

        if is_result_set {
            affected_rows = 0;
            last_insert_id = None;
        }

        Ok(QueryResult {
            columns,
            rows,
            affected_rows,
            last_insert_id,
            elapsed_ms: 0,
            truncated,
            backend_notices_present: false,
        })
    }
}

#[derive(Default)]
struct DecodedRowBudget {
    retained_heap_bytes: usize,
}

impl DecodedRowBudget {
    fn add_columns(&mut self, columns: &[Column]) {
        let bytes = std::mem::size_of_val(columns).saturating_add(
            columns
                .iter()
                .map(|column| {
                    column
                        .name
                        .capacity()
                        .saturating_add(column.type_name.capacity())
                })
                .fold(0_usize, usize::saturating_add),
        );
        self.retained_heap_bytes = self.retained_heap_bytes.saturating_add(bytes);
    }

    /// Retain bounded prior rows plus at most the one currently decoded row.
    /// Returning true tells the stream loop to stop immediately; that final
    /// row is left intact only long enough for `ResultSnapshot::retain` to
    /// create an explicit preview with original-length metadata.
    fn push_row(&mut self, rows: &mut Vec<Vec<Cell>>, mut row: Vec<Cell>) -> bool {
        row.truncate(MAX_RESULT_COLUMNS);
        let mut payload_bytes = 0_usize;
        let mut crosses_cell_cap = false;
        for cell in &row {
            let bytes = cell_heap_bytes(cell);
            payload_bytes = payload_bytes.saturating_add(bytes);
            crosses_cell_cap |= bytes > MAX_RESULT_CELL_BYTES;
        }
        let row_bytes = std::mem::size_of::<Vec<Cell>>()
            .saturating_add(std::mem::size_of_val(row.as_slice()))
            .saturating_add(payload_bytes);
        let crosses_snapshot_cap =
            self.retained_heap_bytes.saturating_add(row_bytes) > MAX_RESULT_BYTES;
        rows.push(row);
        if crosses_cell_cap || crosses_snapshot_cap {
            true
        } else {
            self.retained_heap_bytes = self.retained_heap_bytes.saturating_add(row_bytes);
            false
        }
    }
}

fn cell_heap_bytes(cell: &Cell) -> usize {
    match cell {
        Cell::Decimal(value) | Cell::Text(value) | Cell::DateTime(value) => value.capacity(),
        Cell::TextPreview { preview, .. } | Cell::JsonPreview { preview, .. } => preview.capacity(),
        Cell::Bytes { retained, .. } => retained.capacity(),
        Cell::Json(value) => json_heap_bytes(value),
        Cell::Null | Cell::Bool(_) | Cell::Int(_) | Cell::UInt(_) | Cell::Float(_) => 0,
    }
}

fn json_heap_bytes(value: &serde_json::Value) -> usize {
    match value {
        serde_json::Value::String(value) => value.capacity(),
        serde_json::Value::Array(values) => std::mem::size_of_val(values.as_slice())
            .saturating_add(
                values
                    .iter()
                    .map(json_heap_bytes)
                    .fold(0_usize, usize::saturating_add),
            ),
        serde_json::Value::Object(values) => values.iter().fold(0_usize, |total, (key, value)| {
            total
                .saturating_add(std::mem::size_of::<(String, serde_json::Value)>())
                .saturating_add(key.capacity())
                .saturating_add(json_heap_bytes(value))
        }),
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => 0,
    }
}

fn map_prepare_error(error: sqlx::Error) -> DriverError {
    if mysql_error_number(&error).is_some_and(is_prepare_protocol_unsupported) {
        DriverError::PreparedStatementUnsupported {
            session_healthy: true,
        }
    } else {
        DriverError::MySql(error)
    }
}

fn is_prepare_protocol_unsupported(number: u16) -> bool {
    number == ER_UNSUPPORTED_PS
}

fn mysql_error_number(error: &sqlx::Error) -> Option<u16> {
    let sqlx::Error::Database(database) = error else {
        return None;
    };
    database
        .try_downcast_ref::<MySqlDatabaseError>()
        .map(MySqlDatabaseError::number)
}

async fn timed<T>(
    timeout: Duration,
    future: impl Future<Output = Result<T, sqlx::Error>>,
) -> Result<T, DriverError> {
    tokio::time::timeout(timeout, future)
        .await
        .map_err(|_| DriverError::Timeout {
            driver: DriverKind::MySql,
            seconds: timeout.as_secs(),
        })?
        .map_err(DriverError::from)
}

fn decode_row(row: &MySqlRow) -> Result<Vec<Cell>, sqlx::Error> {
    (0..row.len())
        .map(|index| decode_cell(row, index))
        .collect()
}

fn decode_cell(row: &MySqlRow, index: usize) -> Result<Cell, sqlx::Error> {
    if row.try_get_raw(index)?.is_null() {
        return Ok(Cell::Null);
    }
    let type_name = row.column(index).type_info().name().to_ascii_uppercase();
    let (base_type, unsigned) = type_name
        .strip_suffix(" UNSIGNED")
        .map_or((type_name.as_str(), false), |base| (base, true));
    let cell = match base_type {
        "TINYINT" | "SMALLINT" | "MEDIUMINT" | "INT" | "INTEGER" | "BIGINT" => {
            if unsigned {
                Cell::UInt(row.try_get::<u64, _>(index)?)
            } else {
                Cell::Int(row.try_get::<i64, _>(index)?)
            }
        }
        "FLOAT" | "DOUBLE" | "REAL" => Cell::Float(row.try_get::<f64, _>(index)?),
        "DECIMAL" | "NUMERIC" => Cell::Decimal(row.try_get::<Decimal, _>(index)?.to_string()),
        "DATE" => Cell::DateTime(row.try_get::<NaiveDate, _>(index)?.to_string()),
        "TIME" => Cell::DateTime(row.try_get::<NaiveTime, _>(index)?.to_string()),
        "DATETIME" | "TIMESTAMP" => {
            Cell::DateTime(row.try_get::<NaiveDateTime, _>(index)?.to_string())
        }
        "JSON" => {
            let value = row.try_get::<sqlx::types::Json<serde_json::Value>, _>(index)?;
            Cell::Json(value.0)
        }
        "BINARY" | "VARBINARY" | "TINYBLOB" | "BLOB" | "MEDIUMBLOB" | "LONGBLOB" | "BIT" => {
            bytes_cell(row.try_get::<Vec<u8>, _>(index)?)
        }
        _ => Cell::Text(row.try_get::<String, _>(index)?),
    };
    Ok(cell)
}

fn bytes_cell(bytes: Vec<u8>) -> Cell {
    let original_len = bytes.len();
    Cell::Bytes {
        retained: bytes,
        original_len,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepared_protocol_unsupported_code_is_exactly_scoped() {
        assert!(is_prepare_protocol_unsupported(ER_UNSUPPORTED_PS));
        assert!(!is_prepare_protocol_unsupported(1064));
    }

    #[test]
    fn decoded_bytes_remain_complete_until_the_snapshot_retention_boundary() {
        let Cell::Bytes {
            retained,
            original_len,
        } = bytes_cell(vec![0xab; 40])
        else {
            panic!("bytes cell expected");
        };
        assert_eq!(original_len, 40);
        assert_eq!(retained, vec![0xab; 40]);
    }

    #[test]
    fn decoded_stream_keeps_bounded_prior_rows_plus_only_the_crossing_row() {
        let columns = vec![Column {
            name: "value".to_owned(),
            type_name: "TEXT".to_owned(),
        }];
        let mut budget = DecodedRowBudget::default();
        budget.add_columns(&columns);
        let mut rows = Vec::new();

        for _ in 0..10_000 {
            if budget.push_row(
                &mut rows,
                vec![Cell::Text("x".repeat(MAX_RESULT_CELL_BYTES + 1))],
            ) {
                break;
            }
        }

        assert_eq!(rows.len(), 1, "the crossing current row stops the stream");
        assert!(budget.retained_heap_bytes <= MAX_RESULT_BYTES);
        let Cell::Text(value) = &rows[0][0] else {
            panic!("text row expected");
        };
        assert_eq!(value.len(), MAX_RESULT_CELL_BYTES + 1);
    }

    #[test]
    fn decoded_stream_drops_columns_beyond_the_retained_cap_per_row() {
        let mut budget = DecodedRowBudget::default();
        let mut rows = Vec::new();
        assert!(!budget.push_row(&mut rows, vec![Cell::Null; MAX_RESULT_COLUMNS + 1],));
        assert_eq!(rows[0].len(), MAX_RESULT_COLUMNS);
    }
}
