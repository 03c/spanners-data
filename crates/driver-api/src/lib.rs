//! The driver boundary.
//!
//! Everything here is deliberately database-agnostic and `serde`-friendly, so a
//! driver can later be moved out of process (or into a dynamic library) without
//! changing its implementation. A driver crate depends on this crate and
//! nothing else from the application.
//!
//! Design rules:
//! - one `Session` per UI tab; no pooling. A session owns its transaction, temp
//!   tables and `SET`s, and those are its identity.
//! - row caps are enforced by the *driver* (stop fetching), never by rewriting
//!   the user's SQL.
//! - `cancel` must reach the server, not just drop a future.

use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

pub use value::{Column, Row, Value};

mod value;

pub type Result<T, E = DriverError> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum DriverError {
    #[error("could not connect: {0}")]
    Connect(String),
    #[error("query failed: {0}")]
    Query(String),
    #[error("query cancelled")]
    Cancelled,
    #[error("timed out after {0:?}")]
    Timeout(Duration),
    #[error("not supported by this driver: {0}")]
    Unsupported(String),
    #[error("{0}")]
    Other(String),
}

/// The SQL dialect a connection speaks. Drives highlighting, parsing and
/// server-side safety settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Dialect {
    Postgres,
    MySql,
    Sqlite,
}

impl Dialect {
    pub fn as_str(self) -> &'static str {
        match self {
            Dialect::Postgres => "postgres",
            Dialect::MySql => "mysql",
            Dialect::Sqlite => "sqlite",
        }
    }
}

/// A unique id for one execution on a session. Used to cancel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct QueryId(pub u64);

/// Everything a driver needs to know to start an execution. Derived from the
/// connection's policy profile by the core — drivers do not make policy.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExecOpts {
    /// Stop fetching after this many rows. `None` means no cap.
    pub max_rows: Option<u32>,
    /// Server-side or client-side deadline. `None` means no deadline.
    pub timeout: Option<Duration>,
    /// The session is already read-only; drivers may assert this too.
    pub read_only: bool,
}

/// Streamed as the driver produces rows. Batches are bounded (target: <= 500
/// rows) so the UI and the IPC boundary never see an unbounded allocation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum QueryEvent {
    /// Sent once, before any rows: lets the grid build its header immediately.
    Meta {
        columns: Vec<Column>,
    },
    Batch {
        rows: Vec<Row>,
        truncated: bool,
    },
    Done {
        elapsed_ms: u64,
        row_count: u64,
        truncated: bool,
    },
    Failed {
        message: String,
        /// One of [`codes`] when the failure has a machine-readable meaning.
        /// The message is for humans; this is what the UI branches on.
        code: Option<String>,
        position: Option<u32>,
    },
}

/// Values used in [`QueryEvent::Failed::code`]. Drivers translate their own
/// vendor error codes into these so the UI does not have to know that Postgres
/// calls a timeout `57014` and MySQL calls it `3024`.
pub mod codes {
    /// The user asked to stop; the query did not finish.
    pub const CANCELLED: &str = "CANCELLED";
    /// The statement deadline expired, either server-side or client-side.
    pub const TIMEOUT: &str = "TIMEOUT";
    /// The server refused a write because the session is read-only. This is the
    /// safety profile doing its job, not a bug.
    pub const READ_ONLY: &str = "READ_ONLY";
    /// The connection is gone and the session has to be reconnected.
    pub const DISCONNECTED: &str = "DISCONNECTED";
}

/// Session-level settings, applied by the driver *at connect time* so the
/// server enforces them. Drivers should prefer server-side settings
/// (`SET default_transaction_read_only`, `PRAGMA query_only`, ...) over
/// client-side checks.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SessionOpts {
    pub read_only: bool,
    /// Default deadline for every execution on this session.
    pub timeout: Option<Duration>,
}

/// The database-agnostic driver contract. Implementations are statically linked
/// today (see `spanners-registry`); the trait carries no Rust-specific types
/// beyond this crate's own structs, so out-of-process drivers are possible later.
#[async_trait]
pub trait Driver: Send + Sync {
    /// Stable id used in config files: `"postgres"`, `"mysql"`, `"sqlite"`.
    fn id(&self) -> &'static str;
    /// Human label for the connection dialog.
    fn label(&self) -> &'static str;
    fn dialect(&self) -> Dialect;
    /// `None` for embedded databases.
    fn default_port(&self) -> Option<u16>;
    fn default_database(&self, cfg: &ConnConfig) -> String;

    async fn connect(&self, cfg: &ConnConfig, opts: SessionOpts) -> Result<Box<dyn Session>>;
}

/// A live connection. One per tab.
#[async_trait]
pub trait Session: Send + Sync {
    fn driver_id(&self) -> &'static str;
    /// e.g. `"PostgreSQL 16.2"`, shown in the status bar.
    fn server_version(&self) -> &str;

    /// Starts an execution and returns immediately. Rows flow to `events`.
    /// Errors that happen before the first row are returned here; everything
    /// after goes out as [`QueryEvent::Failed`].
    async fn execute(
        &self,
        sql: &str,
        opts: ExecOpts,
        events: mpsc::Sender<QueryEvent>,
    ) -> Result<QueryId>;

    /// Must reach the server. Implementations that cannot cancel must return
    /// [`DriverError::Unsupported`] rather than pretending.
    async fn cancel(&self, id: QueryId) -> Result<()>;

    async fn schema(&self) -> Result<SchemaSnapshot>;

    async fn explain(&self, sql: &str) -> Result<Plan>;

    async fn in_transaction(&self) -> bool;
    async fn commit(&self) -> Result<()>;
    async fn rollback(&self) -> Result<()>;

    async fn close(&self) -> Result<()>;
}

/// A `Box<dyn Session>` is itself a session. Without this, every call site has
/// to spell `s.as_ref()`, which is noise that obscures the actual work.
#[async_trait]
impl Session for Box<dyn Session> {
    fn driver_id(&self) -> &'static str {
        (**self).driver_id()
    }
    fn server_version(&self) -> &str {
        (**self).server_version()
    }
    async fn execute(
        &self,
        sql: &str,
        opts: ExecOpts,
        events: mpsc::Sender<QueryEvent>,
    ) -> Result<QueryId> {
        (**self).execute(sql, opts, events).await
    }
    async fn cancel(&self, id: QueryId) -> Result<()> {
        (**self).cancel(id).await
    }
    async fn schema(&self) -> Result<SchemaSnapshot> {
        (**self).schema().await
    }
    async fn explain(&self, sql: &str) -> Result<Plan> {
        (**self).explain(sql).await
    }
    async fn in_transaction(&self) -> bool {
        (**self).in_transaction().await
    }
    async fn commit(&self) -> Result<()> {
        (**self).commit().await
    }
    async fn rollback(&self) -> Result<()> {
        (**self).rollback().await
    }
    async fn close(&self) -> Result<()> {
        (**self).close().await
    }
}

/// Connection fields. Passwords never live here — they come from the keychain
/// and are passed separately at connect time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnConfig {
    pub driver: String,
    /// Embedded databases use this as a file path.
    pub host: Option<String>,
    pub port: Option<u16>,
    pub database: Option<String>,
    pub username: Option<String>,
    /// Supplied at connect time from the keychain. `ConnConfig` is the runtime
    /// shape; the persisted `Connection` never contains this.
    #[serde(skip)]
    pub password: Option<String>,
    pub params: Vec<(String, String)>,
}

impl ConnConfig {
    pub fn sqlite(path: impl Into<String>) -> Self {
        Self {
            driver: "sqlite".into(),
            host: None,
            port: None,
            database: Some(path.into()),
            username: None,
            password: None,
            params: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SchemaSnapshot {
    pub schemas: Vec<Schema>,
}

impl SchemaSnapshot {
    pub fn is_empty(&self) -> bool {
        self.schemas.is_empty()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Schema {
    pub name: String,
    pub tables: Vec<Table>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Table {
    pub schema: String,
    pub name: String,
    pub kind: TableKind,
    pub columns: Vec<TableColumn>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TableKind {
    Table,
    View,
    Other,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableColumn {
    pub name: String,
    pub type_name: String,
    pub nullable: bool,
}

/// Rendered explain output: raw text for fidelity, plus a JSON tree when the
/// driver can produce one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    pub raw: String,
    pub json: Option<String>,
}
