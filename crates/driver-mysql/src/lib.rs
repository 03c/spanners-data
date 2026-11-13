//! MySQL driver.
//!
//! Cancellation is `KILL QUERY <connection_id>` on a second connection — the
//! same thing the `mysql` CLI does. Read-only and statement timeouts are
//! session settings, so they hold even if the policy classifier is wrong.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::{NaiveDate, NaiveDateTime, NaiveTime, Timelike};
use futures_util::TryStreamExt;
use sqlx::mysql::{MySqlConnectOptions, MySqlConnection, MySqlRow};
use sqlx::{Column as _, Connection as _, Row as _, TypeInfo as _, ValueRef as _};
use tokio::sync::mpsc;

use spanners_driver_api::{
    codes, Column, ConnConfig, Dialect, Driver, DriverError, ExecOpts, Plan, QueryEvent, QueryId,
    Result, Row, Schema, SchemaSnapshot, Session, SessionOpts, Table, TableColumn, TableKind,
    Value,
};

const BATCH_ROWS: usize = 500;

pub struct MySqlDriver;

impl MySqlDriver {
    pub fn new() -> Self {
        Self
    }
}

impl Default for MySqlDriver {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Driver for MySqlDriver {
    fn id(&self) -> &'static str {
        "mysql"
    }

    fn label(&self) -> &'static str {
        "MySQL"
    }

    fn dialect(&self) -> Dialect {
        Dialect::MySql
    }

    fn default_port(&self) -> Option<u16> {
        Some(3306)
    }

    fn default_database(&self, cfg: &ConnConfig) -> String {
        cfg.database.clone().unwrap_or_default()
    }

    async fn connect(&self, cfg: &ConnConfig, opts: SessionOpts) -> Result<Box<dyn Session>> {
        let mut o = MySqlConnectOptions::new();
        if let Some(h) = &cfg.host {
            o = o.host(h);
        }
        if let Some(p) = cfg.port {
            o = o.port(p);
        }
        if let Some(d) = &cfg.database {
            o = o.database(d);
        }
        if let Some(u) = &cfg.username {
            o = o.username(u);
        }
        if let Some(pw) = &cfg.password {
            o = o.password(pw);
        }

        let mut conn = MySqlConnection::connect_with(&o)
            .await
            .map_err(|e| DriverError::Connect(e.to_string()))?;

        if opts.read_only {
            // Applies to every following transaction; with autocommit each
            // statement is its own transaction, so writes are refused.
            sqlx::query("SET SESSION TRANSACTION READ ONLY")
                .execute(&mut conn)
                .await
                .map_err(|e| DriverError::Connect(e.to_string()))?;
        }
        if let Some(t) = opts.timeout {
            let ms = t.as_millis().max(1);
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "SET SESSION max_execution_time = {ms}"
            )))
            .execute(&mut conn)
            .await
            .map_err(|e| DriverError::Connect(e.to_string()))?;
        }

        let id: u64 = sqlx::query_scalar("SELECT CONNECTION_ID()")
            .fetch_one(&mut conn)
            .await
            .map_err(|e| DriverError::Connect(e.to_string()))?;

        let version: String = sqlx::query_scalar("SELECT VERSION()")
            .fetch_one(&mut conn)
            .await
            .map_err(|e| DriverError::Connect(e.to_string()))?;

        Ok(Box::new(MySqlSession {
            conn: Arc::new(tokio::sync::Mutex::new(conn)),
            opts: o,
            read_only: opts.read_only,
            default_timeout: opts.timeout,
            connection_id: id,
            version: format!("MySQL {version}"),
            next_id: AtomicU64::new(1),
            flag: Mutex::new(None),
            in_txn: AtomicBool::new(false),
            broken: Arc::new(AtomicBool::new(false)),
        }))
    }
}

pub struct MySqlSession {
    conn: Arc<tokio::sync::Mutex<MySqlConnection>>,
    opts: MySqlConnectOptions,
    read_only: bool,
    default_timeout: Option<Duration>,
    connection_id: u64,
    version: String,
    next_id: AtomicU64,
    flag: Mutex<Option<Arc<AtomicBool>>>,
    in_txn: AtomicBool,
    /// Set when a stream could not be resynchronised. The next execution fails
    /// fast instead of hanging on a socket full of unread rows.
    broken: Arc<AtomicBool>,
}

#[async_trait]
impl Session for MySqlSession {
    fn driver_id(&self) -> &'static str {
        "mysql"
    }

    fn server_version(&self) -> &str {
        &self.version
    }

    async fn execute(
        &self,
        sql: &str,
        opts: ExecOpts,
        events: mpsc::Sender<QueryEvent>,
    ) -> Result<QueryId> {
        if self.broken.load(Ordering::SeqCst) {
            return Err(DriverError::Query(
                "connection is out of sync after an abandoned result set; reconnect".into(),
            ));
        }
        let id = QueryId(self.next_id.fetch_add(1, Ordering::Relaxed));
        self.note_transaction(sql);

        let cancelled = Arc::new(AtomicBool::new(false));
        *self.flag.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::clone(&cancelled));

        let conn = Arc::clone(&self.conn);
        let sql = sql.to_owned();
        let max_rows = opts.max_rows;
        let timeout = opts.timeout.or(self.default_timeout);
        let canceller = Canceller {
            opts: self.opts.clone(),
            connection_id: self.connection_id,
            broken: Arc::clone(&self.broken),
        };

        tokio::spawn(async move {
            let mut conn = conn.lock().await;
            let job = Job {
                sql: &sql,
                max_rows,
                timeout,
                cancelled: &cancelled,
                events: &events,
                canceller: &canceller,
            };
            run(&mut conn, &job).await;
        });

        Ok(id)
    }

    async fn cancel(&self, _id: QueryId) -> Result<()> {
        if let Some(flag) = self.flag.lock().unwrap_or_else(|e| e.into_inner()).clone() {
            flag.store(true, Ordering::SeqCst);
        }

        kill_query(&self.opts, self.connection_id)
            .await
            .map_err(|e| DriverError::Query(format!("cancel failed: {e}")))
    }

    async fn schema(&self) -> Result<SchemaSnapshot> {
        const SQL: &str = r#"
            SELECT c.table_schema, c.table_name, t.table_type, c.column_name,
                   c.column_type, c.is_nullable
            FROM information_schema.columns c
            JOIN information_schema.tables t
              ON t.table_schema = c.table_schema AND t.table_name = c.table_name
            WHERE c.table_schema NOT IN ('mysql', 'information_schema', 'performance_schema', 'sys')
            ORDER BY c.table_schema, c.table_name, c.ordinal_position
        "#;

        let mut conn = self.conn.lock().await;
        let rows: Vec<MySqlRow> = sqlx::query(SQL)
            .fetch_all(&mut *conn)
            .await
            .map_err(|e| DriverError::Query(e.to_string()))?;

        let mut schemas: Vec<Schema> = Vec::new();
        for row in rows {
            let schema_name: String = row.try_get(0).unwrap_or_default();
            let table_name: String = row.try_get(1).unwrap_or_default();
            let table_type: String = row.try_get(2).unwrap_or_default();
            let col_name: String = row.try_get(3).unwrap_or_default();
            let type_name: String = row.try_get(4).unwrap_or_default();
            let nullable: String = row.try_get(5).unwrap_or_default();

            let schema = match schemas.iter().position(|s| s.name == schema_name) {
                Some(i) => &mut schemas[i],
                None => {
                    schemas.push(Schema {
                        name: schema_name.clone(),
                        tables: Vec::new(),
                    });
                    schemas.last_mut().expect("just pushed")
                }
            };
            let table = match schema.tables.iter().position(|t| t.name == table_name) {
                Some(i) => &mut schema.tables[i],
                None => {
                    schema.tables.push(Table {
                        schema: schema_name,
                        name: table_name,
                        kind: if table_type.eq_ignore_ascii_case("VIEW") {
                            TableKind::View
                        } else {
                            TableKind::Table
                        },
                        columns: Vec::new(),
                    });
                    schema.tables.last_mut().expect("just pushed")
                }
            };
            table.columns.push(TableColumn {
                name: col_name,
                type_name,
                nullable: nullable.eq_ignore_ascii_case("YES"),
            });
        }

        Ok(SchemaSnapshot { schemas })
    }

    async fn explain(&self, sql: &str) -> Result<Plan> {
        let sql = sql.trim().trim_end_matches(';');
        let mut conn = self.conn.lock().await;
        let row: MySqlRow = sqlx::query(sqlx::AssertSqlSafe(format!("EXPLAIN FORMAT=JSON {sql}")))
            .fetch_one(&mut *conn)
            .await
            .map_err(|e| DriverError::Query(e.to_string()))?;
        let json: serde_json::Value = row
            .try_get(0)
            .map_err(|e| DriverError::Query(e.to_string()))?;
        Ok(Plan {
            raw: serde_json::to_string_pretty(&json).unwrap_or_else(|_| json.to_string()),
            json: Some(json.to_string()),
        })
    }

    async fn in_transaction(&self) -> bool {
        self.in_txn.load(Ordering::Relaxed)
    }

    async fn commit(&self) -> Result<()> {
        if self.read_only {
            return Err(DriverError::Query("session is read-only".into()));
        }
        let mut conn = self.conn.lock().await;
        self.in_txn.store(false, Ordering::Relaxed);
        sqlx::query("COMMIT")
            .execute(&mut *conn)
            .await
            .map(|_| ())
            .map_err(|e| DriverError::Query(e.to_string()))
    }

    async fn rollback(&self) -> Result<()> {
        let mut conn = self.conn.lock().await;
        self.in_txn.store(false, Ordering::Relaxed);
        sqlx::query("ROLLBACK")
            .execute(&mut *conn)
            .await
            .map(|_| ())
            .map_err(|e| DriverError::Query(e.to_string()))
    }

    async fn close(&self) -> Result<()> {
        Ok(())
    }
}

impl MySqlSession {
    fn note_transaction(&self, sql: &str) {
        let head = sql.split_whitespace().next().unwrap_or("");
        if head.eq_ignore_ascii_case("BEGIN") || head.eq_ignore_ascii_case("START") {
            self.in_txn.store(true, Ordering::Relaxed);
        } else if head.eq_ignore_ascii_case("COMMIT") || head.eq_ignore_ascii_case("ROLLBACK") {
            self.in_txn.store(false, Ordering::Relaxed);
        }
    }
}

/// Everything needed to interrupt this connection from the side, which is the
/// only way to stop a result set that is already streaming.
#[derive(Clone)]
struct Canceller {
    opts: MySqlConnectOptions,
    connection_id: u64,
    broken: Arc<AtomicBool>,
}

async fn kill_query(
    opts: &MySqlConnectOptions,
    connection_id: u64,
) -> std::result::Result<(), String> {
    let mut side = MySqlConnection::connect_with(opts)
        .await
        .map_err(|e| e.to_string())?;
    let res = sqlx::query(sqlx::AssertSqlSafe(format!("KILL QUERY {connection_id}")))
        .execute(&mut side)
        .await
        .map_err(|e| e.to_string());
    let _ = side.close().await;
    res.map(|_| ())
}

/// One execution's inputs. Bundled because a query needs its SQL, its limits,
/// its cancellation flag, its output channel and its escape hatch, and passing
/// five positional arguments invites mixing them up.
struct Job<'a> {
    sql: &'a str,
    max_rows: Option<u32>,
    timeout: Option<Duration>,
    cancelled: &'a AtomicBool,
    events: &'a mpsc::Sender<QueryEvent>,
    canceller: &'a Canceller,
}

async fn run(conn: &mut MySqlConnection, job: &Job<'_>) {
    let started = Instant::now();

    if let Some(t) = job.timeout {
        let ms = t.as_millis().max(1);
        if let Err(e) = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SET SESSION max_execution_time = {ms}"
        )))
        .execute(&mut *conn)
        .await
        {
            let _ = job
                .events
                .send(QueryEvent::Failed {
                    message: e.to_string(),
                    code: None,
                    position: None,
                })
                .await;
            return;
        }
    }

    match stream(conn, job, started).await {
        Ok(()) => {}
        Err(e) => {
            let code = classify(&e, job.cancelled.load(Ordering::SeqCst));
            let _ = job
                .events
                .send(QueryEvent::Failed {
                    message: e.to_string(),
                    code,
                    position: None,
                })
                .await;
        }
    }
}

/// MySQL reports these as ordinary errors; the UI needs to tell "you hit stop"
/// and "your profile blocked that write" apart from a genuine failure.
fn classify(err: &sqlx::Error, cancelled: bool) -> Option<String> {
    if cancelled {
        return Some(codes::CANCELLED.to_string());
    }
    match err {
        sqlx::Error::Database(db) => {
            // `DatabaseError::code()` is the SQLSTATE here; the granular MySQL
            // error number is what distinguishes a timeout from a cancel.
            if let Some(mysql) = db
                .as_error()
                .downcast_ref::<sqlx::mysql::MySqlDatabaseError>()
            {
                match mysql.number() {
                    // ER_QUERY_TIMEOUT, ER_QUERY_INTERRUPTED.
                    3024 => return Some(codes::TIMEOUT.to_string()),
                    1317 => return Some(codes::CANCELLED.to_string()),
                    // ER_CANT_EXECUTE_IN_READ_ONLY_TRANSACTION, ER_READ_ONLY_MODE,
                    // ER_OPTION_PREVENTS_STATEMENT.
                    1792 | 1836 | 1290 => return Some(codes::READ_ONLY.to_string()),
                    _ => {}
                }
            }
            match db.code().as_deref() {
                // READ ONLY SQL TRANSACTION.
                Some("25006") => Some(codes::READ_ONLY.to_string()),
                Some("70100") => Some(codes::CANCELLED.to_string()),
                _ => None,
            }
        }
        sqlx::Error::Io(_) | sqlx::Error::PoolClosed | sqlx::Error::PoolTimedOut => {
            Some(codes::DISCONNECTED.to_string())
        }
        _ => None,
    }
}

async fn stream(
    conn: &mut MySqlConnection,
    job: &Job<'_>,
    started: Instant,
) -> std::result::Result<(), sqlx::Error> {
    let mut stream = sqlx::query(sqlx::AssertSqlSafe(job.sql.to_owned())).fetch(&mut *conn);
    let mut batch: Vec<Row> = Vec::with_capacity(BATCH_ROWS);
    let mut columns: Vec<Column> = Vec::new();
    let mut sent: u64 = 0;
    let mut meta_sent = false;

    let stop = loop {
        if job.cancelled.load(Ordering::SeqCst) {
            break Stop::Cancelled;
        }
        if let Some(cap) = job.max_rows {
            if sent >= cap as u64 {
                break Stop::Capped;
            }
        }

        let row = match stream.try_next().await? {
            Some(r) => r,
            None => break Stop::Natural,
        };

        if !meta_sent {
            columns = row
                .columns()
                .iter()
                .map(|c| Column::new(c.name(), c.type_info().name()))
                .collect();
            if job
                .events
                .send(QueryEvent::Meta {
                    columns: columns.clone(),
                })
                .await
                .is_err()
            {
                break Stop::Detached;
            }
            meta_sent = true;
        }

        batch.push(Row((0..columns.len())
            .map(|i| map_value(&row, i))
            .collect()));
        sent += 1;

        if batch.len() == BATCH_ROWS {
            let rows = std::mem::take(&mut batch);
            if job
                .events
                .send(QueryEvent::Batch {
                    rows,
                    truncated: false,
                })
                .await
                .is_err()
            {
                break Stop::Detached;
            }
        }
    };

    if stop.is_early() {
        // MySQL keeps pushing rows until the client reads them. If we just walk
        // away, the next command deadlocks behind the unread tail — so ask the
        // server to stop, then discard what was already in flight.
        if let Err(e) = kill_query(&job.canceller.opts, job.canceller.connection_id).await {
            eprintln!("spanners: could not stop abandoned result set: {e}");
        }
        resync(&mut stream, job.canceller).await;
    }

    if stop == Stop::Detached {
        return Ok(());
    }

    if !meta_sent {
        let _ = job
            .events
            .send(QueryEvent::Meta {
                columns: Vec::new(),
            })
            .await;
    }
    if !batch.is_empty() {
        let _ = job
            .events
            .send(QueryEvent::Batch {
                rows: batch,
                truncated: stop != Stop::Natural,
            })
            .await;
    }

    if stop == Stop::Cancelled {
        // Rows already sent stay in the grid; the UI still needs to know the
        // user stopped it rather than the result being complete.
        let _ = job
            .events
            .send(QueryEvent::Failed {
                message: "query cancelled".into(),
                code: Some(codes::CANCELLED.into()),
                position: None,
            })
            .await;
    } else {
        let _ = job
            .events
            .send(QueryEvent::Done {
                elapsed_ms: started.elapsed().as_millis() as u64,
                row_count: sent,
                truncated: stop == Stop::Capped,
            })
            .await;
    }
    Ok(())
}

/// Why a row stream ended before its natural end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    /// The server finished sending.
    Natural,
    /// The row cap was reached.
    Capped,
    /// The user asked to stop.
    Cancelled,
    /// The consumer dropped the receiver.
    Detached,
}

impl Stop {
    fn is_early(self) -> bool {
        !matches!(self, Stop::Natural)
    }
}

/// Discard the tail of an aborted result set until the protocol is back at a
/// command boundary. Bounded: after a successful `KILL QUERY` this is at most
/// one socket buffer. If the server never stops, the session is marked broken
/// so the next statement reports a reconnect instead of hanging forever.
async fn resync(
    stream: &mut futures_util::stream::BoxStream<'_, std::result::Result<MySqlRow, sqlx::Error>>,
    canceller: &Canceller,
) {
    const MAX_DRAIN_ROWS: usize = 1_000_000;

    let mut drained = 0usize;
    loop {
        match stream.try_next().await {
            // EOF: everything was already consumed.
            Ok(None) => return,
            // The interruption itself arrives as an error; that is the
            // boundary we wanted.
            Err(_) => return,
            Ok(Some(_)) => {
                drained += 1;
                if drained >= MAX_DRAIN_ROWS {
                    canceller.broken.store(true, Ordering::SeqCst);
                    return;
                }
            }
        }
    }
}

fn map_value(row: &MySqlRow, i: usize) -> Value {
    if row.try_get_raw(i).map(|r| r.is_null()).unwrap_or(false) {
        return Value::Null;
    }
    let ty = row.column(i).type_info().name();

    macro_rules! get {
        ($t:ty, $map:expr) => {
            row.try_get::<Option<$t>, _>(i).ok().flatten().map($map)
        };
    }

    let unsigned = ty.ends_with("UNSIGNED");
    let mapped: Option<Value> =
        match ty {
            "BOOLEAN" => get!(bool, Value::Bool),
            "TINYINT" => get!(i8, |v| Value::Int(v as i64)),
            "TINYINT UNSIGNED" => get!(u8, |v| Value::Int(v as i64)),
            "SMALLINT" => get!(i16, |v| Value::Int(v as i64)),
            "SMALLINT UNSIGNED" => get!(u16, |v| Value::Int(v as i64)),
            "MEDIUMINT" | "INT" => get!(i32, |v| Value::Int(v as i64)),
            "MEDIUMINT UNSIGNED" | "INT UNSIGNED" => get!(u32, |v| Value::Int(v as i64)),
            "BIGINT" => get!(i64, Value::Int),
            "BIGINT UNSIGNED" => get!(u64, |v| {
                if v <= i64::MAX as u64 {
                    Value::Int(v as i64)
                } else {
                    Value::Decimal(v.to_string())
                }
            }),
            "FLOAT" => get!(f32, |v| Value::Float(v as f64)),
            "DOUBLE" => get!(f64, Value::Float),
            "DECIMAL" => get!(sqlx::types::BigDecimal, |v| Value::Decimal(v.to_string())),
            "VARCHAR" | "CHAR" | "TEXT" | "TINYTEXT" | "MEDIUMTEXT" | "LONGTEXT" | "ENUM"
            | "SET" => get!(String, Value::Text),
            "JSON" => get!(serde_json::Value, |v| Value::Json(v.to_string())),
            "BINARY" | "VARBINARY" | "BLOB" | "TINYBLOB" | "MEDIUMBLOB" | "LONGBLOB"
            | "GEOMETRY" => get!(Vec<u8>, Value::Bytes),
            "DATE" => get!(NaiveDate, |v| Value::Date(epoch_days(v))),
            "DATETIME" | "TIMESTAMP" => get!(NaiveDateTime, |v| {
                Value::Timestamp(v.and_utc().timestamp_micros())
            }),
            "TIME" => get!(NaiveTime, |v| Value::Time(time_micros(v))),
            "YEAR" => get!(i32, |v| Value::Int(v as i64)),
            "BIT" => get!(u64, |v| Value::Int(v as i64)),
            _ => None,
        };

    mapped
        .or_else(|| {
            // Covers unsigned variants and anything else that arrives text-like.
            if unsigned {
                row.try_get::<Option<u64>, _>(i)
                    .ok()
                    .flatten()
                    .map(|v| Value::Int(v as i64))
            } else {
                None
            }
        })
        .unwrap_or_else(|| {
            row.try_get::<Option<String>, _>(i)
                .ok()
                .flatten()
                .map(Value::Text)
                .unwrap_or_else(|| Value::Unknown(ty.to_string()))
        })
}

fn epoch_days(d: NaiveDate) -> i32 {
    let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).expect("epoch");
    (d - epoch).num_days() as i32
}

fn time_micros(t: NaiveTime) -> i64 {
    t.num_seconds_from_midnight() as i64 * 1_000_000 + t.nanosecond() as i64 / 1_000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_conversion_matches_known_dates() {
        assert_eq!(epoch_days(NaiveDate::from_ymd_opt(1970, 1, 1).unwrap()), 0);
        assert_eq!(
            epoch_days(NaiveDate::from_ymd_opt(2024, 1, 1).unwrap()),
            19723
        );
    }
}
