//! Application services: the layer both shells call.
//!
//! Everything a UI needs to do — connect, run guarded SQL, cancel, browse
//! schema, export, search history — lives here, with no dependency on Tauri,
//! egui or any toolkit. That keeps the shell decision (see PLAN §3) from
//! blocking this work, and keeps the logic testable without a window.

pub mod secrets;

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use spanners_core::config::{redact_dsn, AppConfig, ConfigError, ConfigStore, Connection};
use spanners_core::export::{self, Buffer, ExportOpts, ExportSummary};
use spanners_core::history::{
    HistoryEntry, Library, SavedQuery, SearchHit, Store, StoreError, HISTORY_MAX_DAYS,
    HISTORY_MAX_ROWS,
};
use spanners_core::{Completion, Gate, Policy};
use spanners_driver_api::{
    Dialect, DriverError, ExecOpts, Plan, QueryEvent, QueryId, SchemaSnapshot, Session,
    SessionOpts, Value,
};
use spanners_registry::Registry;
use tokio::sync::mpsc;

pub use secrets::{MemorySecrets, SecretError, Secrets};

#[cfg(feature = "os-keyring")]
pub use secrets::KeyringSecrets;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("no connection with id {0}")]
    UnknownConnection(String),
    #[error("driver {0} is not registered; use postgres, mysql or sqlite")]
    UnknownDriver(String),
    #[error("connection {0} is not open")]
    NotConnected(String),
    #[error("connection {id} needs a password")]
    PasswordRequired { id: String },
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Driver(#[from] DriverError),
    #[error(transparent)]
    Export(#[from] spanners_core::export::ExportError),
    /// The connection profile refuses this SQL. Returned by paths that have no
    /// natural place for a [`RunOutcome`], such as export.
    #[error("blocked by the connection profile")]
    Blocked {
        reasons: Vec<spanners_core::Violation>,
    },
    #[error("this statement needs confirmation")]
    NeedsConfirmation {
        reasons: Vec<spanners_core::Violation>,
    },
    /// A production unlock expired while a transaction was open.
    #[error(
        "the write unlock expired; finish the open transaction with COMMIT or ROLLBACK, then the connection re-locks"
    )]
    UnlockExpired,
    /// `unlock_writes` was given the wrong confirmation text.
    #[error("that does not match the connection name; writes stay locked")]
    ConfirmationMismatch,
    /// `unlock_writes` on a connection that is not read-only.
    #[error("this connection already allows writes")]
    WritesAlreadyAllowed,
    /// The connection is missing a field it needs, or has one that would
    /// weaken a prompt.
    #[error("{0}")]
    InvalidConnection(String),
    #[error("there is already a connection called {name}")]
    DuplicateName { name: String },
    #[error("{0}")]
    Io(String),
}

pub type Result<T> = std::result::Result<T, AppError>;

/// What the UI shows after a successful connect.
#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub connection_id: String,
    pub driver: String,
    pub dialect: Dialect,
    pub server_version: String,
    pub read_only: bool,
    pub max_rows: Option<u32>,
}

struct Live {
    session: Box<dyn Session>,
    policy: Policy,
    info: SessionInfo,
    /// When an explicit write unlock expires. `None` means writes are locked
    /// (or the profile never locked them in the first place).
    unlock_until: Option<Instant>,
    /// Cached schema, used for autocomplete. Fetched on demand rather than at
    /// connect time, because a schema query costs a round trip the user did
    /// not ask for, and a connection that is never queried needs no schema.
    schema: Option<SchemaSnapshot>,
}

/// One running statement. Holding this is what keeps a query streamable.
///
/// `recv` is the only supported way to consume it: that is where history gets
/// recorded, so every query the user runs lands in the history without the UI
/// having to remember.
pub struct Run {
    pub query_id: QueryId,
    events: mpsc::Receiver<QueryEvent>,
    recorder: Recorder,
    finished: bool,
}

struct Recorder {
    store: Arc<Mutex<Store>>,
    connection_id: String,
    sql: String,
    started_at: i64,
    started: Instant,
}

impl Run {
    /// Next event, recording the outcome when the statement ends.
    pub async fn recv(&mut self) -> Option<QueryEvent> {
        let event = self.events.recv().await;
        match &event {
            Some(QueryEvent::Done {
                elapsed_ms,
                row_count,
                ..
            }) => self.finish(Some(*row_count), None, *elapsed_ms),
            Some(QueryEvent::Failed { message, .. }) => self.finish(None, Some(message.clone()), 0),
            None => {
                // The producer vanished without a terminal event. Record it:
                // silence in the history would look like the query never ran.
                self.finish(None, Some("query stream ended unexpectedly".into()), 0);
            }
            _ => {}
        }
        event
    }

    /// Rows the driver already delivered, for a caller that switched to a
    /// different tab and only wants the count.
    fn finish(&mut self, row_count: Option<u64>, error: Option<String>, elapsed_ms: u64) {
        if self.finished {
            return;
        }
        self.finished = true;
        let entry = HistoryEntry {
            connection_id: self.recorder.connection_id.clone(),
            sql: self.recorder.sql.clone(),
            started_at: self.recorder.started_at,
            duration_ms: if elapsed_ms > 0 {
                elapsed_ms
            } else {
                self.recorder.started.elapsed().as_millis() as u64
            },
            row_count,
            error,
        };
        if let Ok(store) = self.recorder.store.lock() {
            // History is a convenience, never a reason to fail a query.
            let _ = store.record(&entry);
            let _ = store.prune(HISTORY_MAX_ROWS, HISTORY_MAX_DAYS);
        }
    }
}

impl Drop for Run {
    fn drop(&mut self) {
        // Abandoned without a terminal event (tab closed, app shutting down).
        // Still cheaper to have the row than a hole in the history.
        self.finish(None, None, 0);
    }
}

/// A production write unlock expires after this much idle time. Long enough to
/// do a careful repair, short enough that a forgotten unlock is not a standing
/// invitation.
const DEFAULT_UNLOCK_IDLE: Duration = Duration::from_secs(15 * 60);

pub struct App {
    config_path: ConfigStore,
    config: AppConfig,
    registry: Registry,
    history: Arc<Mutex<Store>>,
    secrets: Box<dyn Secrets>,
    live: HashMap<String, Live>,
    /// How long an explicit production write unlock survives without use.
    unlock_idle: Duration,
}

impl App {
    /// Open the app's state in `dir` (`config.toml` + `spanners.db`).
    pub fn open(dir: impl AsRef<Path>) -> Result<Self> {
        let dir = dir.as_ref();
        // A first run has no directory yet, and `--home` may point anywhere.
        // Creating it is part of opening: asking a user to `mkdir` before the
        // first command is a papercut with no upside.
        std::fs::create_dir_all(dir)
            .map_err(|e| AppError::Io(format!("could not create {}: {e}", dir.display())))?;
        let config_path = ConfigStore::new(dir.join("config.toml"));
        let config = config_path.load()?;
        let history = Store::open(dir.join("spanners.db"))?;
        Ok(Self {
            config_path,
            config,
            registry: Registry::default_registry(),
            history: Arc::new(Mutex::new(history)),
            secrets: default_secrets(),
            live: HashMap::new(),
            unlock_idle: DEFAULT_UNLOCK_IDLE,
        })
    }

    /// Same as [`App::open`], with an explicit secret store. Tests use this so
    /// they never touch the developer's real keychain.
    pub fn open_with_secrets(dir: impl AsRef<Path>, secrets: Box<dyn Secrets>) -> Result<Self> {
        let mut app = Self::open(dir)?;
        app.secrets = secrets;
        Ok(app)
    }

    pub fn config_path(&self) -> &Path {
        self.config_path.path()
    }

    pub fn connections(&self) -> &[Connection] {
        &self.config.connections
    }

    pub fn connection(&self, id: &str) -> Option<&Connection> {
        self.config.connections.iter().find(|c| c.id == id)
    }

    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    /// Insert or replace a connection. Saving is immediate and atomic: a crash
    /// must not lose a connection the user just typed in.
    pub fn upsert_connection(&mut self, connection: Connection) -> Result<()> {
        connection.validate().map_err(AppError::InvalidConnection)?;
        // Names are how the CLI and the command palette address a connection,
        // so two connections with one name would be ambiguous. Compared
        // case-insensitively for the same reason.
        if let Some(other) = self
            .config
            .connections
            .iter()
            .find(|c| c.id != connection.id && c.name.eq_ignore_ascii_case(&connection.name))
        {
            return Err(AppError::DuplicateName {
                name: other.name.clone(),
            });
        }
        // A connection with an unknown driver would only fail later, at
        // connect time, with the user wondering what they typed. The registry
        // is the authority on what exists.
        if self.registry.get(&connection.driver).is_none() {
            return Err(AppError::UnknownDriver(connection.driver.clone()));
        }
        match self
            .config
            .connections
            .iter_mut()
            .find(|c| c.id == connection.id)
        {
            Some(existing) => *existing = connection.clone(),
            None => self.config.connections.push(connection.clone()),
        }
        self.config_path.save(&self.config)?;

        // A live session keeps its own copy of the policy. Applying an edit
        // immediately means tightening a profile takes effect at once rather
        // than at the next reconnect, which is the direction that matters: a
        // user who switches a connection to Production expects the next
        // statement to be guarded, not the one after the next reconnect.
        //
        // Relaxing is only partial: server-side read-only was set when the
        // session opened and needs a reconnect. That fails safe, because the
        // server keeps refusing writes the new policy would allow.
        if let Some(live) = self.live.get_mut(&connection.id) {
            live.policy = connection.policy();
            // Editing the connection revokes an explicit unlock: the user has
            // just restated what this connection is, and the old consent was
            // given for the old profile.
            live.unlock_until = None;
        }
        Ok(())
    }

    /// Remove a connection, its password, and any open session.
    pub async fn remove_connection(&mut self, id: &str) -> Result<()> {
        if let Some(live) = self.live.remove(id) {
            let _ = live.session.close().await;
        }
        self.config.connections.retain(|c| c.id != id);
        self.config_path.save(&self.config)?;
        // A keychain that refuses to forget must not block the removal.
        let _ = self.secrets.delete(id);
        Ok(())
    }

    /// Store a password now, so the next connect does not prompt.
    pub fn remember_password(&mut self, id: &str, password: &str) -> Result<()> {
        if self.connection(id).is_none() {
            return Err(AppError::UnknownConnection(id.into()));
        }
        // Best effort by design: no keychain, no problem, the user retypes.
        let _ = self.secrets.set(id, password);
        Ok(())
    }

    pub fn has_remembered_password(&self, id: &str) -> bool {
        matches!(self.secrets.get(id), Ok(Some(_)))
    }

    /// Open a session. Idempotent: connecting twice returns the live session.
    ///
    /// `password` is the value from a prompt. It is remembered in the secret
    /// store only if one is available.
    pub async fn connect(&mut self, id: &str, password: Option<String>) -> Result<SessionInfo> {
        if let Some(live) = self.live.get(id) {
            return Ok(live.info.clone());
        }
        self.open_session(id, password, None).await
    }

    /// Open a session, optionally overriding the profile's read-only setting.
    ///
    /// `read_only` is `None` for a normal connect, `Some(false)` for an
    /// explicit production unlock, and `Some(true)` when re-locking. The
    /// override never changes the saved connection: it lives on this session.
    async fn open_session(
        &mut self,
        id: &str,
        password: Option<String>,
        read_only: Option<bool>,
    ) -> Result<SessionInfo> {
        let connection = self
            .connection(id)
            .ok_or_else(|| AppError::UnknownConnection(id.into()))?
            .clone();

        let driver = self
            .registry
            .get(&connection.driver)
            .ok_or_else(|| AppError::UnknownDriver(connection.driver.clone()))?;

        let password = match password {
            Some(p) => {
                let _ = self.secrets.set(id, &p);
                Some(p)
            }
            None => self.secrets.get(id).unwrap_or(None),
        };

        if password.is_none() && needs_password(&connection) {
            return Err(AppError::PasswordRequired { id: id.into() });
        }

        let mut policy = connection.policy();
        let read_only = read_only.unwrap_or(policy.read_only);
        policy.read_only = read_only;
        let dialect = connection.dialect().unwrap_or_else(|| driver.dialect());

        let session = driver
            .connect(
                &connection.to_conn_config(password),
                SessionOpts {
                    read_only,
                    timeout: policy.timeout,
                },
            )
            .await?;

        let info = SessionInfo {
            connection_id: id.to_string(),
            driver: connection.driver.clone(),
            dialect,
            server_version: session.server_version().to_string(),
            read_only,
            max_rows: policy.max_rows,
        };

        self.live.insert(
            id.to_string(),
            Live {
                session,
                policy,
                info: info.clone(),
                unlock_until: None,
                schema: None,
            },
        );
        Ok(info)
    }

    /// Reopen a session with a different read-only setting.
    ///
    /// This drops the current session, so an open transaction is rolled back.
    /// That is deliberate: changing the write mode mid-transaction would let a
    /// half-finished unit of work straddle the boundary.
    async fn reopen(&mut self, id: &str, read_only: bool) -> Result<SessionInfo> {
        if let Some(live) = self.live.remove(id) {
            live.session.close().await?;
        }
        self.open_session(id, None, Some(read_only)).await
    }

    /// Lift the read-only lock on a production connection for this session.
    ///
    /// `confirmation` must be the connection's name, exactly: the point is to
    /// make the user read what they are unlocking. The unlock is not saved and
    /// expires after `unlock_idle` without a statement. Everything else the
    /// profile enforces still applies — DDL stays blocked and a write without a
    /// `WHERE` still asks for confirmation.
    pub async fn unlock_writes(&mut self, id: &str, confirmation: &str) -> Result<SessionInfo> {
        let connection = self
            .connection(id)
            .ok_or_else(|| AppError::UnknownConnection(id.into()))?;
        if !connection.policy().read_only {
            return Err(AppError::WritesAlreadyAllowed);
        }
        if confirmation.trim() != connection.name {
            return Err(AppError::ConfirmationMismatch);
        }
        if !self.is_connected(id) {
            return Err(AppError::NotConnected(id.into()));
        }

        let info = self.reopen(id, false).await?;
        if let Some(live) = self.live.get_mut(id) {
            live.unlock_until = Some(Instant::now() + self.unlock_idle);
        }
        Ok(info)
    }

    /// Put the read-only lock back, closing and reopening the session.
    pub async fn lock_writes(&mut self, id: &str) -> Result<SessionInfo> {
        let locked_by_profile = self
            .connection(id)
            .ok_or_else(|| AppError::UnknownConnection(id.into()))?
            .policy()
            .read_only;
        if !locked_by_profile {
            return Err(AppError::WritesAlreadyAllowed);
        }
        if !self.is_connected(id) {
            return Err(AppError::NotConnected(id.into()));
        }
        self.reopen(id, true).await
    }

    /// How long the current write unlock has left; `None` when writes are
    /// locked.
    pub fn unlock_remaining(&self, id: &str) -> Option<Duration> {
        let until = self.live.get(id)?.unlock_until?;
        Some(until.saturating_duration_since(Instant::now()))
    }

    /// Change how long an unlock survives. Exposed for tests and for a future
    /// preference; the default is 15 minutes.
    pub fn set_unlock_idle(&mut self, idle: Duration) {
        self.unlock_idle = idle;
    }

    /// Re-lock if the unlock has expired. Called before every statement, so an
    /// abandoned unlock cannot outlive its window just because no timer fired.
    /// Extend an active unlock, so the window measures idle time rather than
    /// total time.
    fn touch_unlock(&mut self, id: &str) {
        let idle = self.unlock_idle;
        if let Some(live) = self.live.get_mut(id) {
            if live.unlock_until.is_some() {
                live.unlock_until = Some(Instant::now() + idle);
            }
        }
    }

    async fn expire_unlock(&mut self, id: &str) -> Result<()> {
        let expired = self
            .live
            .get(id)
            .and_then(|live| live.unlock_until)
            .is_some_and(|until| until <= Instant::now());
        if !expired {
            return Ok(());
        }
        // Re-locking reopens the session, which would silently roll back an
        // open transaction. Refuse the statement instead and let the user end
        // the transaction deliberately; the next statement re-locks.
        let in_transaction = match self.live.get(id) {
            Some(live) => live.session.in_transaction().await,
            None => false,
        };
        if in_transaction {
            return Err(AppError::UnlockExpired);
        }
        self.reopen(id, true).await?;
        Ok(())
    }

    /// Try a connection without adopting it: the "Test" button in the
    /// connection dialog. Nothing is saved, so a typo cannot overwrite the
    /// working configuration.
    pub async fn test_connection(
        &self,
        connection: &Connection,
        password: Option<String>,
    ) -> Result<SessionInfo> {
        let driver = self
            .registry
            .get(&connection.driver)
            .ok_or_else(|| AppError::UnknownDriver(connection.driver.clone()))?;
        let policy = connection.policy();
        let dialect = connection.dialect().unwrap_or_else(|| driver.dialect());

        // A test uses the remembered password like a connect does; otherwise
        // "Test" fails on a connection that works, which is worse than useless.
        let password = match password {
            Some(p) => Some(p),
            None => self.secrets.get(&connection.id).unwrap_or(None),
        };

        let session = driver
            .connect(
                &connection.to_conn_config(password),
                SessionOpts {
                    read_only: policy.read_only,
                    timeout: policy.timeout,
                },
            )
            .await?;

        let info = SessionInfo {
            connection_id: connection.id.clone(),
            driver: connection.driver.clone(),
            dialect,
            server_version: session.server_version().to_string(),
            read_only: policy.read_only,
            max_rows: policy.max_rows,
        };
        session.close().await?;
        Ok(info)
    }

    /// Drop the session and open a new one, keeping the stored password. Used
    /// after a driver reports `DISCONNECTED`.
    pub async fn reconnect(&mut self, id: &str) -> Result<SessionInfo> {
        self.disconnect(id).await?;
        self.connect(id, None).await
    }

    pub async fn disconnect(&mut self, id: &str) -> Result<()> {
        match self.live.remove(id) {
            Some(live) => {
                live.session.close().await?;
                Ok(())
            }
            None => Ok(()),
        }
    }

    pub fn is_connected(&self, id: &str) -> bool {
        self.live.contains_key(id)
    }

    pub fn session_info(&self, id: &str) -> Option<&SessionInfo> {
        self.live.get(id).map(|l| &l.info)
    }

    /// Split a buffer into statements for the editor. Uses the connection's
    /// dialect so dollar-quoting and backslash escapes are read correctly.
    pub fn statements(&self, id: &str, sql: &str) -> Result<Vec<spanners_core::Statement>> {
        Ok(spanners_core::split(sql, self.dialect_of(id)?))
    }

    /// The statement under the cursor. This is the "run what I am looking at"
    /// action; it never guesses across a string or comment boundary.
    pub fn statement_at(
        &self,
        id: &str,
        sql: &str,
        offset: usize,
    ) -> Result<Option<spanners_core::Statement>> {
        Ok(spanners_core::statement_at(
            sql,
            offset,
            self.dialect_of(id)?,
        ))
    }

    /// Run only the statement under the cursor, through the same gate as
    /// [`App::run`].
    pub async fn run_statement_at(
        &mut self,
        id: &str,
        sql: &str,
        offset: usize,
        confirmed: bool,
    ) -> Result<RunOutcome> {
        match self.statement_at(id, sql, offset)? {
            Some(statement) => self.run(id, &statement.sql, confirmed).await,
            // Nothing to run: report it as blocked-by-nothing rather than
            // sending an empty string to the server.
            None => Ok(RunOutcome::Blocked {
                reasons: Vec::new(),
            }),
        }
    }

    /// Classify without executing, for editor linting.
    pub async fn judge(&self, id: &str, sql: &str) -> Result<spanners_core::Verdict> {
        let connection = self
            .connection(id)
            .ok_or_else(|| AppError::UnknownConnection(id.into()))?;
        let dialect = connection
            .dialect()
            .or_else(|| self.session_info(id).map(|i| i.dialect))
            .unwrap_or(Dialect::Postgres);

        // A connection that is not open is judged as if no transaction were
        // open, which is the conservative answer for the staging profile.
        // A live session is judged with *its* policy: an unlocked production
        // session must not be reported as read-only.
        let (mut policy, in_transaction) = match self.live.get(id) {
            Some(live) => (live.policy.clone(), live.session.in_transaction().await),
            None => (connection.policy(), false),
        };
        // Judging cannot reopen the session, so an expired unlock is treated
        // as locked here and actually re-locked by the next `run`.
        if !policy.read_only
            && self
                .live
                .get(id)
                .and_then(|live| live.unlock_until)
                .is_some_and(|until| until <= Instant::now())
        {
            policy.read_only = true;
        }
        Ok(policy.judge_in(sql, dialect, in_transaction))
    }

    /// Run one buffer through the policy gate.
    ///
    /// `confirmed` must come from the user accepting a confirmation for this
    /// exact SQL; never default it to `true`.
    pub async fn run(&mut self, id: &str, sql: &str, confirmed: bool) -> Result<RunOutcome> {
        self.run_with_exec(id, sql, ExecOpts::default(), confirmed)
            .await
    }

    /// Run with per-statement limits. `exec` can only tighten what the profile
    /// allows; see [`spanners_core::execute_guarded_with`].
    pub async fn run_with_exec(
        &mut self,
        id: &str,
        sql: &str,
        exec: ExecOpts,
        confirmed: bool,
    ) -> Result<RunOutcome> {
        if !self.is_connected(id) {
            return Err(AppError::NotConnected(id.into()));
        }
        self.expire_unlock(id).await?;
        let live = self
            .live
            .get(id)
            .ok_or_else(|| AppError::NotConnected(id.into()))?;

        let gate = spanners_core::execute_guarded_with(
            live.session.as_ref(),
            &live.policy,
            sql,
            live.info.dialect,
            exec,
            confirmed,
        )
        .await?;

        // A DDL statement invalidates the cached schema. The cache is dropped
        // even if the statement fails: a partially applied change is exactly
        // when a stale column list is most misleading.
        let changes_schema = matches!(gate, Gate::Run { .. })
            && spanners_core::policy::classify(sql, live.info.dialect)
                .statements
                .iter()
                .any(|kind| matches!(kind, spanners_core::StmtKind::Ddl));

        let outcome = match gate {
            Gate::Blocked { reasons } => RunOutcome::Blocked { reasons },
            Gate::NeedsConfirmation { reasons } => RunOutcome::NeedsConfirmation { reasons },
            Gate::Run { query_id, events } => RunOutcome::Running(Run {
                query_id,
                events,
                recorder: Recorder {
                    store: Arc::clone(&self.history),
                    connection_id: id.to_string(),
                    sql: sql.to_string(),
                    started_at: now_ms(),
                    started: Instant::now(),
                },
                finished: false,
            }),
        };

        if changes_schema {
            if let Some(live) = self.live.get_mut(id) {
                live.schema = None;
            }
        }
        // Any statement counts as activity for the unlock window.
        if matches!(outcome, RunOutcome::Running(_)) {
            self.touch_unlock(id);
        }
        Ok(outcome)
    }

    pub async fn cancel(&mut self, id: &str, query_id: QueryId) -> Result<()> {
        let live = self
            .live
            .get(id)
            .ok_or_else(|| AppError::NotConnected(id.into()))?;
        Ok(live.session.cancel(query_id).await?)
    }

    /// The schema, from the cache when possible.
    ///
    /// The cache is dropped when a statement that changes it succeeds, so the
    /// editor does not offer a column that was just removed. `refresh_schema`
    /// is the explicit "reload" action.
    pub async fn schema(&mut self, id: &str) -> Result<SchemaSnapshot> {
        let live = self
            .live
            .get(id)
            .ok_or_else(|| AppError::NotConnected(id.into()))?;
        if let Some(schema) = &live.schema {
            return Ok(schema.clone());
        }
        let schema = live.session.schema().await?;
        if let Some(live) = self.live.get_mut(id) {
            live.schema = Some(schema.clone());
        }
        Ok(schema)
    }

    /// Fetch the schema again, ignoring the cache.
    pub async fn refresh_schema(&mut self, id: &str) -> Result<SchemaSnapshot> {
        if let Some(live) = self.live.get_mut(id) {
            live.schema = None;
        }
        self.schema(id).await
    }

    /// Autocomplete for the cursor at `offset`.
    ///
    /// Synchronous and cache-only: completion runs on every keystroke, so it
    /// must never wait for a round trip. Call [`App::schema`] when the editor
    /// opens so the first keystroke has something to work with.
    pub fn complete(&self, id: &str, sql: &str, offset: usize) -> Result<Vec<Completion>> {
        let dialect = self.dialect_of(id)?;
        let empty = SchemaSnapshot::default();
        let schema = self
            .live
            .get(id)
            .and_then(|live| live.schema.as_ref())
            .unwrap_or(&empty);
        Ok(spanners_core::complete(schema, dialect, sql, offset))
    }

    pub async fn explain(&mut self, id: &str, sql: &str) -> Result<Plan> {
        let live = self
            .live
            .get(id)
            .ok_or_else(|| AppError::NotConnected(id.into()))?;
        Ok(live.session.explain(sql).await?)
    }

    pub async fn in_transaction(&self, id: &str) -> Result<bool> {
        let live = self
            .live
            .get(id)
            .ok_or_else(|| AppError::NotConnected(id.into()))?;
        Ok(live.session.in_transaction().await)
    }

    /// Start an explicit transaction.
    ///
    /// Staging connections require one for writes, so this is a normal action
    /// rather than an advanced one. It is not recorded in history: `BEGIN` is
    /// plumbing, not a query the user wants to find again.
    pub async fn begin(&mut self, id: &str) -> Result<()> {
        let live = self
            .live
            .get(id)
            .ok_or_else(|| AppError::NotConnected(id.into()))?;
        let (tx, mut rx) = mpsc::channel(8);
        live.session
            .execute("BEGIN", ExecOpts::default(), tx)
            .await?;
        while let Some(event) = rx.recv().await {
            match event {
                QueryEvent::Done { .. } => return Ok(()),
                QueryEvent::Failed { message, .. } => {
                    return Err(AppError::Driver(DriverError::Query(message)))
                }
                _ => {}
            }
        }
        Err(AppError::Driver(DriverError::Query(
            "BEGIN ended without a result".into(),
        )))
    }

    pub async fn commit(&mut self, id: &str) -> Result<()> {
        let live = self
            .live
            .get(id)
            .ok_or_else(|| AppError::NotConnected(id.into()))?;
        Ok(live.session.commit().await?)
    }

    pub async fn rollback(&mut self, id: &str) -> Result<()> {
        let live = self
            .live
            .get(id)
            .ok_or_else(|| AppError::NotConnected(id.into()))?;
        Ok(live.session.rollback().await?)
    }

    /// Export to any writer, honouring the connection's profile caps.
    ///
    /// Goes through the same policy gate as [`App::run`]: exporting must not be
    /// a way to run SQL the profile would refuse.
    pub async fn export<W: tokio::io::AsyncWrite + Unpin>(
        &mut self,
        id: &str,
        sql: &str,
        opts: &ExportOpts,
        out: &mut W,
    ) -> Result<ExportSummary> {
        self.export_with_exec(id, sql, ExecOpts::default(), opts, false, out)
            .await
    }

    /// Export with a caller-supplied lower cap (a "limit this export" control).
    ///
    /// The request can only tighten the profile's limits: asking for more rows
    /// than the profile allows still yields the profile's cap, and the deadline
    /// is never removed.
    pub async fn export_with_exec<W: tokio::io::AsyncWrite + Unpin>(
        &mut self,
        id: &str,
        sql: &str,
        exec: ExecOpts,
        opts: &ExportOpts,
        confirmed: bool,
        out: &mut W,
    ) -> Result<ExportSummary> {
        if !self.is_connected(id) {
            return Err(AppError::NotConnected(id.into()));
        }
        self.expire_unlock(id).await?;
        let live = self
            .live
            .get(id)
            .ok_or_else(|| AppError::NotConnected(id.into()))?;

        let in_transaction = live.session.in_transaction().await;
        match live.policy.judge_in(sql, live.info.dialect, in_transaction) {
            spanners_core::Verdict::Allow => {}
            spanners_core::Verdict::Block { reasons } => return Err(AppError::Blocked { reasons }),
            spanners_core::Verdict::Confirm { reasons } if !confirmed => {
                return Err(AppError::NeedsConfirmation { reasons })
            }
            spanners_core::Verdict::Confirm { .. } => {}
        }

        // Same rule as a guarded run: a caller may tighten the profile's
        // limits, never loosen them. The timeout used to be "caller wins",
        // which let `--timeout 600` lift a production connection's 15 s.
        let exec = exec.tightened_by(&live.policy.exec_opts());

        let started_at = now_ms();
        let started = Instant::now();
        let result = export::export(live.session.as_ref(), sql, exec, opts, out).await;
        let elapsed = started.elapsed().as_millis() as u64;
        let summary = result?;

        // An export is still a query the user ran; it belongs in history.
        self.record(id, sql, started_at, elapsed, Some(summary.rows), None);
        self.touch_unlock(id);
        Ok(summary)
    }

    fn record(
        &self,
        connection_id: &str,
        sql: &str,
        started_at: i64,
        duration_ms: u64,
        row_count: Option<u64>,
        error: Option<String>,
    ) {
        let entry = HistoryEntry {
            connection_id: connection_id.to_string(),
            sql: sql.to_string(),
            started_at,
            duration_ms,
            row_count,
            error,
        };
        if let Ok(store) = self.history.lock() {
            // History is a convenience, never a reason to fail a query.
            let _ = store.record(&entry);
            let _ = store.prune(HISTORY_MAX_ROWS, HISTORY_MAX_DAYS);
        }
    }

    /// Copy-ready text, using the same formatting as file export.
    pub async fn export_to_string(
        &mut self,
        id: &str,
        sql: &str,
        opts: &ExportOpts,
    ) -> Result<(String, ExportSummary)> {
        let mut buf = Buffer::new();
        let summary = self.export(id, sql, opts, &mut buf).await?;
        let text = buf.into_string().map_err(|e| AppError::Io(e.to_string()))?;
        Ok((text, summary))
    }

    // ---- history and saved queries -------------------------------------

    pub fn history(&self, limit: u32) -> Result<Vec<(i64, HistoryEntry)>> {
        let store = self.lock_store()?;
        Ok(store.recent(limit)?)
    }

    pub fn search(&self, text: &str, limit: u32) -> Result<Vec<SearchHit>> {
        let store = self.lock_store()?;
        Ok(store.search(text, limit)?)
    }

    pub fn library(&self) -> Result<Library> {
        let store = self.lock_store()?;
        Ok(store.library()?)
    }

    pub fn saved_query(&self, id: i64) -> Result<Option<SavedQuery>> {
        let store = self.lock_store()?;
        Ok(store.query(id)?)
    }

    pub fn save_query(
        &self,
        folder: Option<i64>,
        name: &str,
        body: &str,
        connection_id: Option<&str>,
    ) -> Result<i64> {
        let store = self.lock_store()?;
        Ok(store.save_query(folder, name, body, connection_id.map(str::to_string))?)
    }

    pub fn create_folder(&self, name: &str, parent: Option<i64>) -> Result<i64> {
        let store = self.lock_store()?;
        Ok(store.create_folder(name, parent)?)
    }

    pub fn update_query(&self, id: i64, name: &str, body: &str) -> Result<()> {
        let store = self.lock_store()?;
        Ok(store.update_query(id, name, body)?)
    }

    pub fn move_query(&self, id: i64, folder: Option<i64>) -> Result<()> {
        let store = self.lock_store()?;
        Ok(store.move_query(id, folder)?)
    }

    pub fn delete_query(&self, id: i64) -> Result<()> {
        let store = self.lock_store()?;
        Ok(store.delete_query(id)?)
    }

    pub fn rename_folder(&self, id: i64, name: &str) -> Result<()> {
        let store = self.lock_store()?;
        Ok(store.rename_folder(id, name)?)
    }

    pub fn move_folder(&self, id: i64, parent: Option<i64>) -> Result<()> {
        let store = self.lock_store()?;
        Ok(store.move_folder(id, parent)?)
    }

    pub fn delete_folder(&self, id: i64) -> Result<()> {
        let store = self.lock_store()?;
        Ok(store.delete_folder(id)?)
    }

    /// The dialect of a connection, even before it is open.
    fn dialect_of(&self, id: &str) -> Result<Dialect> {
        if let Some(info) = self.session_info(id) {
            return Ok(info.dialect);
        }
        let connection = self
            .connection(id)
            .ok_or_else(|| AppError::UnknownConnection(id.into()))?;
        connection.dialect().map_or_else(
            || {
                self.registry
                    .get(&connection.driver)
                    .map(|d| d.dialect())
                    .ok_or_else(|| AppError::UnknownDriver(connection.driver.clone()))
            },
            Ok,
        )
    }

    fn lock_store(&self) -> Result<std::sync::MutexGuard<'_, Store>> {
        self.history
            .lock()
            .map_err(|_| AppError::Io("history store lock poisoned".into()))
    }
}

/// What to do with the SQL after the gate has seen it.
pub enum RunOutcome {
    Running(Run),
    NeedsConfirmation {
        reasons: Vec<spanners_core::Violation>,
    },
    Blocked {
        reasons: Vec<spanners_core::Violation>,
    },
}

impl std::fmt::Debug for RunOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunOutcome::Running(run) => f
                .debug_struct("Running")
                .field("query_id", &run.query_id)
                .finish_non_exhaustive(),
            RunOutcome::NeedsConfirmation { reasons } => f
                .debug_struct("NeedsConfirmation")
                .field("reasons", reasons)
                .finish(),
            RunOutcome::Blocked { reasons } => {
                f.debug_struct("Blocked").field("reasons", reasons).finish()
            }
        }
    }
}

fn needs_password(connection: &Connection) -> bool {
    !connection.is_embedded() && connection.username.is_some()
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(feature = "os-keyring")]
fn default_secrets() -> Box<dyn Secrets> {
    Box::new(KeyringSecrets::new())
}

#[cfg(not(feature = "os-keyring"))]
fn default_secrets() -> Box<dyn Secrets> {
    Box::new(MemorySecrets::new())
}

/// Convenience for the "copy selection" menu: TSV of a whole result, produced
/// from the same row formatting the grid uses.
pub fn tsv(columns: &[spanners_driver_api::Column], rows: &[spanners_driver_api::Row]) -> String {
    let mut out = String::new();
    let header: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
    out.push_str(&header.join("\t"));
    out.push('\n');
    for row in rows {
        let fields: Vec<String> = row
            .0
            .iter()
            .map(|v| v.as_text().unwrap_or_default().replace(['\t', '\n'], " "))
            .collect();
        out.push_str(&fields.join("\t"));
        out.push('\n');
    }
    out
}

/// Render one value for the grid's copy action.
pub fn cell_text(value: &Value) -> String {
    value.as_text().unwrap_or_else(|| "NULL".into())
}

/// Human-readable label for a connection row: host/database, with the
/// password never present because it is never stored there.
pub fn connection_label(connection: &Connection) -> String {
    match (&connection.host, &connection.database) {
        (Some(host), Some(db)) => {
            let port = connection.port.map(|p| format!(":{p}")).unwrap_or_default();
            redact_dsn(&format!("{}://{host}{port}/{db}", connection.driver))
        }
        (None, Some(db)) => db.clone(),
        _ => connection.name.clone(),
    }
}

#[cfg(test)]
mod tests;
