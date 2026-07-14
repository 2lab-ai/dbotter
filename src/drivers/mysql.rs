use std::fmt::Write as _;
use std::time::{Duration, Instant};

use chrono::{NaiveDate, NaiveDateTime, NaiveTime};
use futures_util::TryStreamExt as _;
use rust_decimal::Decimal;
use secrecy::{ExposeSecret as _, SecretString};
use sqlparser::dialect::MySqlDialect;
use sqlparser::tokenizer::{Token, Tokenizer};
use sqlx::mysql::{
    MySqlConnectOptions, MySqlDatabaseError, MySqlPoolOptions, MySqlRow, MySqlSslMode,
};
use sqlx::{
    Column as _, Either, Executor as _, Row as _, SqlSafeStr as _, Statement as _, TypeInfo as _,
    ValueRef as _,
};

use crate::drivers::DriverError;
use crate::model::{
    Cell, Column, ConnectionProfile, DriverAvailability, DriverCapabilities, DriverDescriptor,
    DriverKind, ExecuteRequest, QueryLanguage, QueryResult, TlsMode,
};

pub const DESCRIPTOR: DriverDescriptor = DriverDescriptor {
    kind: DriverKind::MySql,
    display_name: "MySQL",
    default_port: 3306,
    availability: DriverAvailability::Ready,
    languages: &[QueryLanguage::Sql],
    capabilities: DriverCapabilities::CONNECT
        .union(DriverCapabilities::PING)
        .union(DriverCapabilities::SQL),
    planned_capabilities: DriverCapabilities::CATALOG,
    reason: None,
};

#[derive(Clone)]
pub struct MySqlSession {
    pool: sqlx::MySqlPool,
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

        let pool = timed(
            timeout,
            MySqlPoolOptions::new()
                .max_connections(4)
                .acquire_timeout(timeout)
                .connect_with(options),
        )
        .await?;
        Ok(Self { pool })
    }

    pub async fn ping(&self, timeout: Duration) -> Result<(), DriverError> {
        timed(timeout, sqlx::query("SELECT 1").execute(&self.pool))
            .await
            .map(|_| ())
    }

    pub async fn execute(&self, request: &ExecuteRequest) -> Result<QueryResult, DriverError> {
        let text = request.text.trim();
        if text.is_empty() {
            return Err(DriverError::InvalidConfig {
                driver: DriverKind::MySql,
                message: "SQL text is empty".to_owned(),
            });
        }
        validate_single_statement(text).map_err(|message| DriverError::InvalidConfig {
            driver: DriverKind::MySql,
            message,
        })?;
        let started = Instant::now();
        let result = timed(request.timeout, self.execute_one(text, request.row_limit)).await?;
        Ok(QueryResult {
            elapsed_ms: started.elapsed().as_millis(),
            ..result
        })
    }

    async fn execute_one(&self, text: &str, row_limit: u32) -> Result<QueryResult, sqlx::Error> {
        let limit = row_limit as usize;
        let mut connection = self.pool.acquire().await?;

        // The editor intentionally supplies SQL as SQL, so dynamic text is
        // explicitly audited for SQLx. Preparation lets MySQL determine the
        // result shape instead of relying on a leading-keyword heuristic.
        let statement = match (&mut *connection)
            .prepare(sqlx::AssertSqlSafe(text.to_owned()).into_sql_str())
            .await
        {
            Ok(statement) => statement,
            Err(error) if is_unsupported_prepared_statement(&error) => {
                return execute_raw(&mut connection, text, row_limit).await;
            }
            Err(error) => return Err(error),
        };
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
        let mut affected_rows = 0;
        let mut last_insert_id = None;
        let mut truncated = false;

        while let Some(step) = stream.try_next().await? {
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
                    }
                    if rows.len() == limit {
                        truncated = true;
                        break;
                    }
                    rows.push(decode_row(&row)?);
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
            notices: Vec::new(),
        })
    }
}

async fn execute_raw(
    connection: &mut sqlx::pool::PoolConnection<sqlx::MySql>,
    text: &str,
    row_limit: u32,
) -> Result<QueryResult, sqlx::Error> {
    let limit = row_limit as usize;
    let sql = sqlx::raw_sql(sqlx::AssertSqlSafe(text.to_owned()));
    let mut stream = (&mut **connection).fetch_many(sql);
    let mut columns = Vec::new();
    let mut rows = Vec::new();
    let mut affected_rows = 0;
    let mut last_insert_id = None;
    let mut truncated = false;
    let mut is_result_set = false;

    while let Some(step) = stream.try_next().await? {
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
                }
                if rows.len() == limit {
                    truncated = true;
                    break;
                }
                rows.push(decode_row(&row)?);
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
        notices: Vec::new(),
    })
}

fn validate_single_statement(text: &str) -> Result<(), String> {
    let dialect = MySqlDialect {};
    let tokens = Tokenizer::new(&dialect, text)
        .tokenize()
        .map_err(|error| format!("SQL tokenization failed: {error}"))?;
    let mut has_content = false;
    let mut terminated = false;

    for token in tokens {
        match token {
            Token::Whitespace(_) => {}
            Token::SemiColon if has_content && !terminated => terminated = true,
            Token::SemiColon => {
                return Err("exactly one MySQL statement is required".to_owned());
            }
            _ if terminated => {
                return Err("exactly one MySQL statement is required".to_owned());
            }
            _ => has_content = true,
        }
    }

    if has_content {
        Ok(())
    } else {
        Err("SQL text contains no statement".to_owned())
    }
}

fn is_unsupported_prepared_statement(error: &sqlx::Error) -> bool {
    matches!(
        error,
        sqlx::Error::Database(database)
            if database
                .try_downcast_ref::<MySqlDatabaseError>()
                .is_some_and(|error| is_unsupported_prepared_statement_number(error.number()))
    )
}

fn is_unsupported_prepared_statement_number(number: u16) -> bool {
    number == 1295
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
    let mut preview = String::new();
    for byte in bytes.iter().take(32) {
        let _ = write!(&mut preview, "{byte:02x}");
    }
    if bytes.len() > 32 {
        preview.push('…');
    }
    Cell::Bytes {
        preview,
        len: bytes.len(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_statement_validation_uses_mysql_lexing() {
        assert!(validate_single_statement("/* ; */ -- ;\n SELECT ';' AS value;").is_ok());
        assert!(validate_single_statement("SELECT 'a;b' AS value -- ;\n").is_ok());
        assert!(validate_single_statement("SELECT 1; SELECT 2").is_err());
        assert!(validate_single_statement("SELECT 1;;").is_err());
        assert!(validate_single_statement("/* only a comment ; */").is_err());
    }

    #[test]
    fn raw_fallback_is_scoped_to_mysql_error_1295() {
        assert!(is_unsupported_prepared_statement_number(1295));
        assert!(!is_unsupported_prepared_statement_number(1064));
    }

    #[test]
    fn bytes_are_bounded_and_counted() {
        let Cell::Bytes { preview, len } = bytes_cell(vec![0xab; 40]) else {
            panic!("bytes cell expected");
        };
        assert_eq!(len, 40);
        assert!(preview.ends_with('…'));
    }
}
