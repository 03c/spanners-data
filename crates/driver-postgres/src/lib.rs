//! Postgres driver.
//!
//! Cancellation goes through `pg_cancel_backend(pid)` on a second connection,
//! because sqlx exposes no cancel token. That is the same mechanism `psql`
//! uses and it reaches the server, which is the point.
//!
//! Read-only and timeouts are server-side settings (`default_transaction_read_only`,
//! `statement_timeout`), so the policy layer cannot be bypassed by a bug here.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Timelike, Utc};
use futures_util::TryStreamExt;
use sqlx::postgres::{PgConnectOptions, PgConnection, PgRow, PgSslMode};
use sqlx::{Column as _, Connection as _, Row as _, TypeInfo as _, ValueRef as _};
use tokio::sync::mpsc;

use spanners_driver_api::{
    codes, Column, ConnConfig, Dialect, Driver, DriverError, ExecOpts, Plan, QueryEvent, QueryId,
    Result, Row, Schema, SchemaSnapshot, Session, SessionOpts, Table, TableColumn, TableKind,
    Value,
};

const BATCH_ROWS: usize = 500;

pub struct PostgresDriver;

/// Apply a URL parameter. Unknown parameters are an error: ignoring
/// `sslmode=require` would silently connect in plaintext.
fn apply_param(o: PgConnectOptions, key: &str, value: &str) -> Result<PgConnectOptions> {
    match key.to_ascii_lowercase().as_str() {
        "sslmode" => {
            let mode = match value.to_ascii_lowercase().as_str() {
                "disable" => PgSslMode::Disable,
                "allow" => PgSslMode::Allow,
                "prefer" => PgSslMode::Prefer,
                "require" => PgSslMode::Require,
                "verify-ca" => PgSslMode::VerifyCa,
                "verify-full" => PgSslMode::VerifyFull,
                other => {
                    return Err(DriverError::Connect(format!(
                        "unknown sslmode '{other}'; use disable, allow, prefer, require, verify-ca or verify-full"
                    )))
                }
            };
            Ok(o.ssl_mode(mode))
        }
        "application_name" => Ok(o.application_name(value)),
        "options" => Ok(o.options([(key.to_string(), value.to_string())])),
        other => Err(DriverError::Connect(format!(
            "unsupported connection parameter '{other}'"
        ))),
    }
}

impl PostgresDriver {
    pub fn new() -> Self {
        Self
    }
}

impl Default for PostgresDriver {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Driver for PostgresDriver {
    fn id(&self) -> &'static str {
        "postgres"
    }

    fn label(&self) -> &'static str {
        "PostgreSQL"
    }

    fn dialect(&self) -> Dialect {
        Dialect::Postgres
    }

    fn default_port(&self) -> Option<u16> {
        Some(5432)
    }

    fn default_database(&self, cfg: &ConnConfig) -> String {
        cfg.database.clone().unwrap_or_else(|| "postgres".into())
    }

    async fn connect(&self, cfg: &ConnConfig, opts: SessionOpts) -> Result<Box<dyn Session>> {
        let mut o = PgConnectOptions::new().application_name("spanners-data");
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
        for (key, value) in &cfg.params {
            o = apply_param(o, key, value)?;
        }

        let mut conn = PgConnection::connect_with(&o)
            .await
            .map_err(|e| DriverError::Connect(e.to_string()))?;

        if opts.read_only {
            sqlx::query("SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY")
                .execute(&mut conn)
                .await
                .map_err(|e| DriverError::Connect(e.to_string()))?;
            sqlx::query("SET default_transaction_read_only = on")
                .execute(&mut conn)
                .await
                .map_err(|e| DriverError::Connect(e.to_string()))?;
        }
        if let Some(t) = opts.timeout {
            let ms = t.as_millis().max(1);
            sqlx::query("SELECT set_config('statement_timeout', $1, false)")
                .bind(ms.to_string())
                .execute(&mut conn)
                .await
                .map_err(|e| DriverError::Connect(e.to_string()))?;
        }

        let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut conn)
            .await
            .map_err(|e| DriverError::Connect(e.to_string()))?;

        let version: String = sqlx::query_scalar("SHOW server_version")
            .fetch_one(&mut conn)
            .await
            .map_err(|e| DriverError::Connect(e.to_string()))?;

        Ok(Box::new(PostgresSession {
            conn: Arc::new(tokio::sync::Mutex::new(conn)),
            opts: o,
            read_only: opts.read_only,
            default_timeout: opts.timeout,
            pid,
            version: format!("PostgreSQL {version}"),
            next_id: AtomicU64::new(1),
            flag: Mutex::new(None),
            in_txn: AtomicBool::new(false),
            broken: Arc::new(AtomicBool::new(false)),
        }))
    }
}

pub struct PostgresSession {
    conn: Arc<tokio::sync::Mutex<PgConnection>>,
    /// Cloned to open the cancellation connection.
    opts: PgConnectOptions,
    read_only: bool,
    default_timeout: Option<Duration>,
    version: String,
    pid: i32,
    next_id: AtomicU64,
    flag: Mutex<Option<Arc<AtomicBool>>>,
    in_txn: AtomicBool,
    /// Set when a stream could not be resynchronised. The next execution fails
    /// fast instead of hanging on a socket full of unread rows.
    broken: Arc<AtomicBool>,
}

#[async_trait]
impl Session for PostgresSession {
    fn driver_id(&self) -> &'static str {
        "postgres"
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
        let in_txn = self.in_txn.load(Ordering::Relaxed);
        let id = QueryId(self.next_id.fetch_add(1, Ordering::Relaxed));
        self.note_transaction(sql);
        let cancelled = Arc::new(AtomicBool::new(false));
        *self.flag.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::clone(&cancelled));

        let conn = Arc::clone(&self.conn);
        let sql = sql.to_owned();
        let max_rows = opts.max_rows;
        // `None` means "the session default applies", never "remove the
        // profile's deadline" — otherwise passing no timeout would silently
        // disable a production connection's 15s limit. `connect` already
        // applied the default, so only a different value needs a round trip.
        let effective = opts.timeout.or(self.default_timeout);
        let timeout = if effective == self.default_timeout {
            Timeout::Keep
        } else {
            Timeout::Set {
                value: effective,
                session_default: self.default_timeout,
            }
        };
        let canceller = Canceller {
            opts: self.opts.clone(),
            pid: self.pid,
            broken: Arc::clone(&self.broken),
        };

        tokio::spawn(async move {
            let mut conn = conn.lock().await;
            let job = Job {
                sql: &sql,
                max_rows,
                timeout,
                in_txn,
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

        cancel_backend(&self.opts, self.pid)
            .await
            .map_err(|e| DriverError::Query(format!("cancel failed: {e}")))
    }

    async fn schema(&self) -> Result<SchemaSnapshot> {
        const SQL: &str = r#"
            SELECT n.nspname,
                   c.relname,
                   c.relkind::text,
                   a.attname,
                   format_type(a.atttypid, a.atttypmod),
                   NOT a.attnotnull
            FROM pg_class c
            JOIN pg_namespace n ON n.oid = c.relnamespace
            JOIN pg_attribute a ON a.attrelid = c.oid AND a.attnum > 0 AND NOT a.attisdropped
            WHERE c.relkind IN ('r', 'v', 'm', 'p')
              AND n.nspname NOT IN ('pg_catalog', 'information_schema')
              AND n.nspname NOT LIKE 'pg_toast%'
            ORDER BY n.nspname, c.relname, a.attnum
        "#;

        let mut conn = self.conn.lock().await;
        let rows: Vec<PgRow> = sqlx::query(SQL)
            .fetch_all(&mut *conn)
            .await
            .map_err(|e| DriverError::Query(e.to_string()))?;

        Ok(group_schema(&rows))
    }

    async fn explain(&self, sql: &str) -> Result<Plan> {
        let sql = sql.trim().trim_end_matches(';');
        let mut conn = self.conn.lock().await;
        let row: PgRow = sqlx::query(sqlx::AssertSqlSafe(format!("EXPLAIN (FORMAT JSON) {sql}")))
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
        // Tracked client-side: asking the server makes our own connection show
        // as `active`, so `pg_stat_activity` cannot answer this mid-statement.
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

impl PostgresSession {
    /// Cheap client-side transaction tracking. Only statements that change the
    /// transaction state are inspected; anything else leaves it alone.
    fn note_transaction(&self, sql: &str) {
        let head = sql.split_whitespace().next().unwrap_or("");
        if head.eq_ignore_ascii_case("BEGIN") || head.eq_ignore_ascii_case("START") {
            self.in_txn.store(true, Ordering::Relaxed);
        } else if head.eq_ignore_ascii_case("COMMIT") || head.eq_ignore_ascii_case("ROLLBACK") {
            self.in_txn.store(false, Ordering::Relaxed);
        }
    }
}

/// Everything needed to interrupt this backend from the side, which is the
/// only way to stop a result set that is already streaming.
#[derive(Clone)]
struct Canceller {
    opts: PgConnectOptions,
    pid: i32,
    broken: Arc<AtomicBool>,
}

async fn cancel_backend(opts: &PgConnectOptions, pid: i32) -> std::result::Result<(), String> {
    let mut side = PgConnection::connect_with(opts)
        .await
        .map_err(|e| e.to_string())?;
    let res = sqlx::query("SELECT pg_cancel_backend($1)")
        .bind(pid)
        .execute(&mut side)
        .await
        .map_err(|e| e.to_string());
    let _ = side.close().await;
    res.map(|_| ())
}

/// One execution's inputs. Bundled because a query needs its SQL, its limits,
/// its cancellation flag, its output channel and its escape hatch, and passing
/// eight positional arguments invites mixing them up.
struct Job<'a> {
    sql: &'a str,
    max_rows: Option<u32>,
    timeout: Timeout,
    in_txn: bool,
    cancelled: &'a AtomicBool,
    events: &'a mpsc::Sender<QueryEvent>,
    canceller: &'a Canceller,
}

/// `statement_timeout` is a session setting, so a per-statement override would
/// otherwise leak into every later statement on the connection — including
/// lengthening a deadline past what the profile allows.
#[derive(Debug, Clone, Copy)]
enum Timeout {
    /// `connect` already applied the session default; nothing to do.
    Keep,
    /// Apply for this statement, then restore `session_default`.
    Set {
        value: Option<Duration>,
        session_default: Option<Duration>,
    },
}

impl Timeout {
    fn wants_change(self) -> bool {
        !matches!(self, Timeout::Keep)
    }
}

async fn run(conn: &mut PgConnection, job: &Job<'_>) {
    let started = Instant::now();

    if job.timeout.wants_change() {
        let Timeout::Set { value, .. } = job.timeout else {
            unreachable!("wants_change is true only for Set")
        };
        if let Err(e) = set_timeout(conn, value).await {
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

    let outcome = stream(conn, job, started).await;

    // Restore even after a failure or a cancel: the next statement must start
    // from the profile's deadline, not this statement's.
    if let Timeout::Set {
        session_default, ..
    } = job.timeout
    {
        let _ = set_timeout(conn, session_default).await;
    }

    if let Err(e) = outcome {
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

async fn set_timeout(conn: &mut PgConnection, timeout: Option<Duration>) -> sqlx::Result<()> {
    let ms = timeout.map(|t| t.as_millis().max(1)).unwrap_or(0);
    sqlx::query("SELECT set_config('statement_timeout', $1, false)")
        .bind(ms.to_string())
        .execute(&mut *conn)
        .await
        .map(|_| ())
}

/// `57014` covers both "user cancelled" and "statement_timeout", so the cancel
/// flag is what distinguishes them. Everything else is left uncoded rather
/// than guessed at from message text.
fn classify(err: &sqlx::Error, cancelled: bool) -> Option<String> {
    if cancelled {
        return Some(codes::CANCELLED.to_string());
    }
    match err {
        sqlx::Error::Database(db) => match db.code().as_deref() {
            Some("57014") => Some(codes::TIMEOUT.to_string()),
            // read_only_sql_transaction / cannot execute in a read-only tx.
            Some("25006") => Some(codes::READ_ONLY.to_string()),
            _ => None,
        },
        sqlx::Error::Io(_) | sqlx::Error::PoolClosed | sqlx::Error::PoolTimedOut => {
            Some(codes::DISCONNECTED.to_string())
        }
        _ => None,
    }
}

async fn stream(
    conn: &mut PgConnection,
    job: &Job<'_>,
    started: Instant,
) -> std::result::Result<(), sqlx::Error> {
    // The SQL is the user's, by design; `AssertSqlSafe` is the explicit audit
    // marker sqlx 0.9 requires for dynamic statements.
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
        // sqlx will otherwise read every remaining row before the next command,
        // which quietly defeats the row cap. Cancel instead — but not inside an
        // explicit transaction: Postgres aborts the whole transaction when its
        // statement is cancelled, and losing the user's transaction is worse
        // than transferring the rest.
        if !job.in_txn {
            if let Err(e) = cancel_backend(&job.canceller.opts, job.canceller.pid).await {
                eprintln!("spanners: could not stop abandoned result set: {e}");
            }
        }
        drain(&mut stream, job.canceller).await;
    }

    if stop == Stop::Detached {
        return Ok(());
    }

    // A statement with no rows (INSERT/UPDATE/DDL) still needs its Meta so the
    // UI can show "0 rows" rather than a spinner.
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

/// Read the tail of an aborted result set until the protocol is back at a
/// command boundary. After a successful cancel this is at most one socket
/// buffer. If the server never stops, the session is marked broken so the next
/// statement reports a reconnect instead of hanging forever.
async fn drain(
    stream: &mut futures_util::stream::BoxStream<'_, std::result::Result<PgRow, sqlx::Error>>,
    canceller: &Canceller,
) {
    const MAX_DRAIN_ROWS: usize = 1_000_000;

    let mut drained = 0usize;
    loop {
        match stream.try_next().await {
            Ok(None) => return,
            // The cancellation itself arrives as an error: that is the boundary.
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

fn map_value(row: &PgRow, i: usize) -> Value {
    if row.try_get_raw(i).map(|r| r.is_null()).unwrap_or(false) {
        return Value::Null;
    }
    let ty = row.column(i).type_info().name();

    macro_rules! get {
        ($t:ty, $map:expr) => {
            row.try_get::<Option<$t>, _>(i).ok().flatten().map($map)
        };
    }

    let mapped: Option<Value> = match ty {
        "BOOL" => get!(bool, Value::Bool),
        "INT2" => get!(i16, |v| Value::Int(v as i64)),
        "INT4" => get!(i32, |v| Value::Int(v as i64)),
        "INT8" => get!(i64, Value::Int),
        "OID" => get!(i64, Value::Int),
        "FLOAT4" => get!(f32, |v| Value::Float(v as f64)),
        "FLOAT8" => get!(f64, Value::Float),
        "NUMERIC" => get!(sqlx::types::BigDecimal, |v| Value::Decimal(v.to_string())),
        "TEXT" | "VARCHAR" | "BPCHAR" | "NAME" | "CHAR" | "UNKNOWN" => {
            get!(String, Value::Text)
        }
        "UUID" => get!(sqlx::types::Uuid, |v| Value::Uuid(v.to_string())),
        "JSON" | "JSONB" => get!(serde_json::Value, |v| Value::Json(v.to_string())),
        "BYTEA" => get!(Vec<u8>, Value::Bytes),
        "DATE" => get!(NaiveDate, |v| { Value::Date(epoch_days(v)) }),
        "TIMESTAMP" => get!(NaiveDateTime, |v| {
            Value::Timestamp(v.and_utc().timestamp_micros())
        }),
        "TIMESTAMPTZ" => get!(DateTime<Utc>, |v| {
            Value::Timestamp(v.timestamp_micros())
        }),
        "TIME" => get!(NaiveTime, |v| Value::Time(time_micros(v))),
        _ => None,
    };

    mapped.unwrap_or_else(|| {
        // Enums and extension types arrive as text; anything undecodable is
        // still shown rather than silently dropped.
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

fn group_schema(rows: &[PgRow]) -> SchemaSnapshot {
    let mut schemas: Vec<Schema> = Vec::new();

    for row in rows {
        let snap: (String, String, String, String, String, bool) = match (
            row.try_get(0),
            row.try_get(1),
            row.try_get(2),
            row.try_get(3),
            row.try_get(4),
            row.try_get(5),
        ) {
            (Ok(a), Ok(b), Ok(c), Ok(d), Ok(e), Ok(f)) => (a, b, c, d, e, f),
            _ => continue,
        };
        let (schema_name, table_name, relkind, col_name, type_name, nullable) = snap;

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
                    name: table_name.clone(),
                    kind: match relkind.as_str() {
                        "v" => TableKind::View,
                        "m" => TableKind::View,
                        _ => TableKind::Table,
                    },
                    columns: Vec::new(),
                });
                schema.tables.last_mut().expect("just pushed")
            }
        };

        table.columns.push(TableColumn {
            name: col_name,
            type_name,
            nullable,
        });
    }

    SchemaSnapshot { schemas }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(params: Vec<(&str, &str)>) -> ConnConfig {
        ConnConfig {
            driver: "postgres".into(),
            host: Some("127.0.0.1".into()),
            port: Some(1),
            database: None,
            username: None,
            password: None,
            params: params
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    #[tokio::test]
    async fn an_unknown_connection_parameter_is_refused_before_connecting() {
        let err = match PostgresDriver::new()
            .connect(&config(vec![("bogus", "1")]), SessionOpts::default())
            .await
        {
            Ok(_) => panic!("expected the parameter to be refused"),
            Err(e) => e,
        };
        assert!(
            err.to_string().contains("unsupported connection parameter"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn an_invalid_sslmode_is_named_in_the_error() {
        let err = match PostgresDriver::new()
            .connect(
                &config(vec![("sslmode", "sometimes")]),
                SessionOpts::default(),
            )
            .await
        {
            Ok(_) => panic!("expected the sslmode to be refused"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("unknown sslmode"), "{err}");
    }

    #[test]
    fn epoch_conversion_matches_known_dates() {
        assert_eq!(epoch_days(NaiveDate::from_ymd_opt(1970, 1, 1).unwrap()), 0);
        assert_eq!(
            epoch_days(NaiveDate::from_ymd_opt(2024, 1, 1).unwrap()),
            19723
        );
        assert_eq!(
            time_micros(NaiveTime::from_hms_opt(1, 1, 1).unwrap()),
            3_661_000_000
        );
    }
}
