//! SQLite driver.
//!
//! Uses `rusqlite` rather than `sqlx` for one decisive reason: it exposes
//! [`rusqlite::InterruptHandle`], so `cancel()` actually stops the statement.
//! A query you cannot cancel is the thing that makes a client feel broken.
//!
//! Execution is synchronous (SQLite is), so each `execute` hands the connection
//! to `spawn_blocking` and streams rows back over a bounded channel. The
//! connection is behind a `std::sync::Mutex`; `cancel` deliberately does not
//! take that lock — it only needs the interrupt handle.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use rusqlite::types::ValueRef;
use rusqlite::{Connection, InterruptHandle, OpenFlags};
use tokio::sync::mpsc;

use spanners_driver_api::{
    codes, Column, ConnConfig, Dialect, Driver, DriverError, ExecOpts, Plan, QueryEvent, QueryId,
    Result, Row, Schema, SchemaSnapshot, Session, SessionOpts, Table, TableColumn, TableKind,
    Value,
};

const BATCH_ROWS: usize = 500;

pub struct SqliteDriver;

impl SqliteDriver {
    pub fn new() -> Self {
        Self
    }
}

impl Default for SqliteDriver {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Driver for SqliteDriver {
    fn id(&self) -> &'static str {
        "sqlite"
    }

    fn label(&self) -> &'static str {
        "SQLite"
    }

    fn dialect(&self) -> Dialect {
        Dialect::Sqlite
    }

    fn default_port(&self) -> Option<u16> {
        None
    }

    fn default_database(&self, cfg: &ConnConfig) -> String {
        cfg.database.clone().unwrap_or_else(|| ":memory:".into())
    }

    async fn connect(&self, cfg: &ConnConfig, opts: SessionOpts) -> Result<Box<dyn Session>> {
        let path = cfg
            .database
            .clone()
            .ok_or_else(|| DriverError::Connect("SQLite requires a database path".into()))?;

        // SQLite has no network TLS or connection parameters; refusing them is
        // clearer than pretending they took effect.
        if let Some((key, _)) = cfg.params.first() {
            return Err(DriverError::Connect(format!(
                "SQLite does not support the connection parameter '{key}'"
            )));
        }

        // A read-only open cannot create the file, and SQLite's own message
        // ("unable to open database file") does not say why. Say it plainly.
        if opts.read_only && path != ":memory:" && !std::path::Path::new(&path).exists() {
            return Err(DriverError::Connect(format!(
                "{path} does not exist; a read-only connection cannot create it"
            )));
        }

        // The strongest read-only guarantee available: the file is opened
        // read-only, so no client bug can turn it into a write. `query_only` is
        // belt and braces for paths that ignore the flag (e.g. shared cache).
        let flags = if opts.read_only {
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI
        } else {
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_URI
        };

        let conn = Connection::open_with_flags(&path, flags)
            .map_err(|e| DriverError::Connect(format!("{path}: {e}")))?;

        conn.busy_timeout(Duration::from_secs(5)).ok();
        if opts.read_only {
            conn.execute_batch("PRAGMA query_only = ON;").ok();
        }
        // WAL is a no-op for :memory: and read-only files; failure is not fatal.
        conn.execute_batch("PRAGMA foreign_keys = ON;").ok();

        let interrupt = conn.get_interrupt_handle();
        let version = format!("SQLite {}", rusqlite::version());

        Ok(Box::new(SqliteSession {
            conn: Arc::new(Mutex::new(conn)),
            interrupt,
            read_only: opts.read_only,
            default_timeout: opts.timeout,
            version,
            next_id: AtomicU64::new(1),
            flag: Mutex::new(None),
        }))
    }
}

pub struct SqliteSession {
    conn: Arc<Mutex<Connection>>,
    interrupt: InterruptHandle,
    read_only: bool,
    default_timeout: Option<Duration>,
    version: String,
    next_id: AtomicU64,
    /// Cancel flag for the execution currently in flight. Replaced per query so
    /// a cancel can never leak into the next one.
    flag: Mutex<Option<Arc<AtomicBool>>>,
}

#[async_trait]
impl Session for SqliteSession {
    fn driver_id(&self) -> &'static str {
        "sqlite"
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
        let id = QueryId(self.next_id.fetch_add(1, Ordering::Relaxed));

        let conn = Arc::clone(&self.conn);
        let cancelled_flag = Arc::new(AtomicBool::new(false));
        let flag_for_task = Arc::clone(&cancelled_flag);
        let timeout = opts.timeout.or(self.default_timeout);
        let max_rows = opts.max_rows;
        let sql = sql.to_owned();
        let read_only = self.read_only;

        // Replace the session-level flag with the one this execution watches.
        self.set_cancel_flag(flag_for_task);

        tokio::task::spawn_blocking(move || {
            run_blocking(
                conn,
                cancelled_flag,
                sql,
                max_rows,
                timeout,
                read_only,
                events,
            );
        });

        Ok(id)
    }

    async fn cancel(&self, _id: QueryId) -> Result<()> {
        // Interrupt is thread-safe and lock-free: it can fire while the
        // blocking task holds the connection mutex.
        if let Some(flag) = self.cancel_flag() {
            flag.store(true, Ordering::SeqCst);
        }
        self.interrupt.interrupt();
        Ok(())
    }

    async fn schema(&self) -> Result<SchemaSnapshot> {
        let conn = self.conn.lock().map_err(|_| poisoned())?;

        let mut stmt = conn
            .prepare(
                "SELECT name, type FROM sqlite_master \
                 WHERE type IN ('table','view') AND name NOT LIKE 'sqlite_%' \
                 ORDER BY name",
            )
            .map_err(query_err)?;

        let entries: Vec<(String, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .map_err(query_err)?
            .collect::<std::result::Result<_, _>>()
            .map_err(query_err)?;

        let mut tables = Vec::with_capacity(entries.len());
        for (name, kind) in entries {
            let mut cols = conn
                .prepare("SELECT name, type, \"notnull\" FROM pragma_table_info(?1)")
                .map_err(query_err)?;
            let columns: Vec<TableColumn> = cols
                .query_map([&name], |r| {
                    let notnull: i64 = r.get(2)?;
                    Ok(TableColumn {
                        name: r.get(0)?,
                        type_name: r.get(1)?,
                        nullable: notnull == 0,
                    })
                })
                .map_err(query_err)?
                .collect::<std::result::Result<_, _>>()
                .map_err(query_err)?;
            tables.push(Table {
                schema: "main".into(),
                name,
                kind: if kind == "view" {
                    TableKind::View
                } else {
                    TableKind::Table
                },
                columns,
            });
        }

        Ok(SchemaSnapshot {
            schemas: vec![Schema {
                name: "main".into(),
                tables,
            }],
        })
    }

    async fn explain(&self, sql: &str) -> Result<Plan> {
        let conn = self.conn.lock().map_err(|_| poisoned())?;
        let mut stmt = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .map_err(query_err)?;
        let mut rows = stmt.query([]).map_err(query_err)?;
        let mut lines = Vec::new();
        while let Some(row) = rows.next().map_err(query_err)? {
            let detail: String = row.get(3).map_err(query_err)?;
            lines.push(detail);
        }
        Ok(Plan {
            raw: lines.join("\n"),
            json: None,
        })
    }

    async fn in_transaction(&self) -> bool {
        match self.conn.lock() {
            Ok(c) => !c.is_autocommit(),
            Err(_) => false,
        }
    }

    async fn commit(&self) -> Result<()> {
        if self.read_only {
            return Err(DriverError::Query("session is read-only".into()));
        }
        self.conn
            .lock()
            .map_err(|_| poisoned())?
            .execute_batch("COMMIT")
            .map_err(query_err)
    }

    async fn rollback(&self) -> Result<()> {
        self.conn
            .lock()
            .map_err(|_| poisoned())?
            .execute_batch("ROLLBACK")
            .map_err(query_err)
    }

    async fn close(&self) -> Result<()> {
        Ok(())
    }
}

/// The cancel flag is per-execution: `cancel()` must not interrupt the *next*
/// query because a stale flag was left set. Held behind a tiny mutex because
/// `execute` replaces it.
impl SqliteSession {
    fn set_cancel_flag(&self, flag: Arc<AtomicBool>) {
        *self.flag.lock().unwrap_or_else(|e| e.into_inner()) = Some(flag);
    }
    fn cancel_flag(&self) -> Option<Arc<AtomicBool>> {
        self.flag.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

fn run_blocking(
    conn: Arc<Mutex<Connection>>,
    cancelled: Arc<AtomicBool>,
    sql: String,
    max_rows: Option<u32>,
    timeout: Option<Duration>,
    read_only: bool,
    events: mpsc::Sender<QueryEvent>,
) {
    let started = Instant::now();

    let guard = match conn.lock() {
        Ok(g) => g,
        Err(_) => {
            let _ = events.blocking_send(QueryEvent::Failed {
                message: "internal error: connection lock poisoned".into(),
                code: None,
                position: None,
            });
            return;
        }
    };

    if read_only && is_write_like(&sql) {
        // Server-side enforcement should already reject this; this makes the
        // error legible instead of "attempt to write a readonly database".
        let _ = events.blocking_send(QueryEvent::Failed {
            message: "this connection is read-only".into(),
            code: Some(codes::READ_ONLY.into()),
            position: None,
        });
        return;
    }

    let deadline = timeout.map(|t| Instant::now() + t);
    if let Some(deadline) = deadline {
        // SQLite calls this every N VM ops; returning true aborts the
        // statement with SQLITE_INTERRUPT.
        let _ = guard.progress_handler(1_000, Some(move || Instant::now() >= deadline));
    }

    let result = stream_query(&guard, &sql, max_rows, &cancelled, &events, started);

    let _ = guard.progress_handler(0, None::<fn() -> bool>);

    if let Err(err) = result {
        let code = if cancelled.load(Ordering::SeqCst) {
            Some(codes::CANCELLED.to_string())
        } else if deadline.is_some_and(|d| Instant::now() >= d) {
            Some(codes::TIMEOUT.to_string())
        } else if err.contains("readonly database") {
            // SQLITE_READONLY: the safety profile refused the write.
            Some(codes::READ_ONLY.to_string())
        } else {
            None
        };
        let _ = events.blocking_send(QueryEvent::Failed {
            message: err,
            code,
            position: None,
        });
    }
}

fn stream_query(
    conn: &Connection,
    sql: &str,
    max_rows: Option<u32>,
    cancelled: &AtomicBool,
    events: &mpsc::Sender<QueryEvent>,
    started: Instant,
) -> std::result::Result<(), String> {
    let mut stmt = conn.prepare(sql).map_err(|e| e.to_string())?;

    let cols: Vec<rusqlite::Column<'_>> = stmt.columns().into_iter().collect();
    let decltypes: Vec<String> = cols
        .iter()
        .map(|c| c.decl_type().unwrap_or("").to_owned())
        .collect();
    let columns: Vec<Column> = cols
        .iter()
        .map(|c| Column::new(c.name(), c.decl_type().unwrap_or("")))
        .collect();

    let column_count = columns.len();
    if events.blocking_send(QueryEvent::Meta { columns }).is_err() {
        return Ok(());
    }

    let mut rows = stmt.query([]).map_err(|e| e.to_string())?;
    let mut batch: Vec<Row> = Vec::with_capacity(BATCH_ROWS);
    let mut sent: u64 = 0;
    let mut truncated = false;

    while let Some(row) = rows.next().map_err(|e| e.to_string())? {
        if cancelled.load(Ordering::SeqCst) {
            truncated = true;
            break;
        }
        if let Some(cap) = max_rows {
            if sent >= cap as u64 {
                truncated = true;
                break;
            }
        }

        let mut values = Vec::with_capacity(column_count);
        for (i, decltype) in decltypes.iter().enumerate() {
            let v = row.get_ref(i).map_err(|e| e.to_string())?;
            values.push(map_value(v, decltype));
        }
        batch.push(Row(values));
        sent += 1;

        if batch.len() == BATCH_ROWS {
            let rows = std::mem::take(&mut batch);
            if events
                .blocking_send(QueryEvent::Batch {
                    rows,
                    truncated: false,
                })
                .is_err()
            {
                return Ok(());
            }
        }
    }

    if !batch.is_empty() {
        let _ = events.blocking_send(QueryEvent::Batch {
            rows: batch,
            truncated,
        });
    }

    let _ = events.blocking_send(QueryEvent::Done {
        elapsed_ms: started.elapsed().as_millis() as u64,
        row_count: sent,
        truncated,
    });

    Ok(())
}

fn map_value(v: ValueRef<'_>, decltype: &str) -> Value {
    match v {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => match decltype.to_ascii_lowercase().as_str() {
            // SQLite has no boolean type; 0/1 in a BOOLEAN column is a bool.
            "boolean" | "bool" => Value::Bool(i != 0),
            _ => Value::Int(i),
        },
        ValueRef::Real(f) => Value::Float(f),
        ValueRef::Text(t) => Value::Text(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => Value::Bytes(b.to_vec()),
    }
}

/// Cheap pre-check so a read-only session gives a clear error. Not a security
/// boundary — the file is opened read-only.
fn is_write_like(sql: &str) -> bool {
    let head = sql
        .trim_start_matches(|c: char| c.is_whitespace() || c == '(')
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_uppercase();
    matches!(
        head.as_str(),
        "INSERT" | "UPDATE" | "DELETE" | "REPLACE" | "CREATE" | "DROP" | "ALTER" | "VACUUM"
    )
}

fn poisoned() -> DriverError {
    DriverError::Other("internal error: connection lock poisoned".into())
}

fn query_err(e: rusqlite::Error) -> DriverError {
    DriverError::Query(e.to_string())
}

#[cfg(test)]
mod tests;
