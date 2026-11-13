//! Query history and saved queries, in one SQLite file.
//!
//! Both live in the same database on purpose: the search box in the sidebar
//! covers "that query I ran last week" and "that query I saved" with one
//! query, and keeping them in one file means one WAL, one backup, one lock.
//!
//! Nothing here touches a user database. This is the app's own store, opened
//! directly with `rusqlite` — the driver layer is not involved.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension};

/// How many history rows are kept. The UI is a search box, not an archive.
pub const HISTORY_MAX_ROWS: u64 = 10_000;
/// How old a history row may be, in days.
pub const HISTORY_MAX_DAYS: i64 = 90;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("could not open {path}: {source}")]
    Open {
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },
    /// The file is not a SQLite database, or its image is malformed. The
    /// message has to say what to do: the user cannot open the app to fix it,
    /// and "file is not a database" with no filename sends them hunting
    /// through the wrong directory. Nothing is deleted automatically — history
    /// is expendable, but discarding it silently is not the user's call.
    #[error(
        "{path} is not a usable history database ({source}); move it aside and Spanners will create a new one"
    )]
    Corrupt {
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },
    #[error(transparent)]
    Sql(#[from] rusqlite::Error),
    #[error("{0}")]
    Invalid(String),
}

pub type Result<T> = std::result::Result<T, StoreError>;

/// True only for the two codes that mean "this file is not a database we can
/// read". Everything else — busy, full, read-only, permissions — is a problem
/// to fix, not a file to throw away.
fn is_corruption(error: &rusqlite::Error) -> bool {
    matches!(
        error.sqlite_error_code(),
        Some(rusqlite::ffi::ErrorCode::DatabaseCorrupt | rusqlite::ffi::ErrorCode::NotADatabase)
    )
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryEntry {
    pub connection_id: String,
    pub sql: String,
    /// Unix milliseconds. Kept explicit so imported/older entries sort right.
    pub started_at: i64,
    pub duration_ms: u64,
    /// `None` when the statement failed.
    pub row_count: Option<u64>,
    /// The error message, if it failed. Success is `None`.
    pub error: Option<String>,
}

impl HistoryEntry {
    pub fn succeeded(connection_id: impl Into<String>, sql: impl Into<String>) -> Self {
        Self {
            connection_id: connection_id.into(),
            sql: sql.into(),
            started_at: now_ms(),
            duration_ms: 0,
            row_count: Some(0),
            error: None,
        }
    }

    pub fn failed(
        connection_id: impl Into<String>,
        sql: impl Into<String>,
        error: impl Into<String>,
    ) -> Self {
        Self {
            connection_id: connection_id.into(),
            sql: sql.into(),
            started_at: now_ms(),
            duration_ms: 0,
            row_count: None,
            error: Some(error.into()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedQuery {
    pub id: i64,
    /// `None` means the query sits at the root because its folder was deleted.
    pub folder_id: Option<i64>,
    /// The connection this query was written against, if any. Informational:
    /// saved queries are searchable across connections.
    pub connection_id: Option<String>,
    pub name: String,
    pub body: String,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Folder {
    pub id: i64,
    pub parent_id: Option<i64>,
    pub name: String,
    pub folders: Vec<Folder>,
    pub queries: Vec<SavedQuery>,
}

/// Everything in the saved-query sidebar.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Library {
    pub folders: Vec<Folder>,
    pub root_queries: Vec<SavedQuery>,
}

impl Library {
    /// Every saved query with its folder path, root queries first.
    ///
    /// The sidebar renders the tree, but the CLI and search want a flat list;
    /// doing the walk here keeps the two views from disagreeing.
    pub fn walk(&self) -> Vec<(String, &SavedQuery)> {
        let mut out = Vec::new();
        for query in &self.root_queries {
            out.push((String::new(), query));
        }
        fn descend<'a>(folder: &'a Folder, prefix: &str, out: &mut Vec<(String, &'a SavedQuery)>) {
            let path = if prefix.is_empty() {
                folder.name.clone()
            } else {
                format!("{prefix}/{}", folder.name)
            };
            for query in &folder.queries {
                out.push((path.clone(), query));
            }
            for child in &folder.folders {
                descend(child, &path, out);
            }
        }
        for folder in &self.folders {
            descend(folder, "", &mut out);
        }
        out
    }

    /// Find a saved query by id, or by name when the name is unambiguous.
    pub fn find(&self, key: &str) -> std::result::Result<&SavedQuery, String> {
        let all = self.walk();
        if let Ok(id) = key.parse::<i64>() {
            if let Some((_, query)) = all.iter().find(|(_, q)| q.id == id) {
                return Ok(query);
            }
        }
        let matches: Vec<&SavedQuery> = all
            .iter()
            .filter(|(_, q)| q.name.eq_ignore_ascii_case(key))
            .map(|(_, q)| *q)
            .collect();
        match matches.as_slice() {
            [one] => Ok(one),
            [] => Err(format!("no saved query called '{key}'")),
            _ => Err(format!(
                "'{key}' matches {} saved queries; use the id from `saved list`",
                matches.len()
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitKind {
    History,
    SavedQuery,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    pub kind: HitKind,
    pub id: i64,
    /// Folder name for saved queries, timestamp label for history.
    pub title: String,
    /// The query text, trimmed to the match.
    pub snippet: String,
    pub connection_id: Option<String>,
    pub at: i64,
}

/// The app's own SQLite store.
pub struct Store {
    conn: Connection,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store").finish_non_exhaustive()
    }
}

impl Store {
    /// Open (or create) the store at `path`. WAL keeps reads from blocking the
    /// history write that happens right after every query.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let conn = Connection::open(&path).map_err(|source| StoreError::Open {
            path: path.clone(),
            source,
        })?;
        // Pragma and migration failures happen after a successful open, so they
        // arrive as a bare `Sql` error. A truncated file or something that is
        // not SQLite at all fails here, and the path is what makes the message
        // actionable. A locked, full or read-only file arrives the same way and
        // must *not* be called corrupt: telling someone to move a healthy
        // database aside because the disk was full would lose their history.
        Self::from_connection(conn).map_err(|e| match e {
            StoreError::Sql(source) if is_corruption(&source) => {
                StoreError::Corrupt { path, source }
            }
            StoreError::Sql(source) => StoreError::Open { path, source },
            other => other,
        })
    }

    /// In-memory store, for tests and for the "no config dir" fallback.
    pub fn in_memory() -> Result<Self> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL").ok();
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let store = Self { conn };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<()> {
        self.conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS history (
                id            INTEGER PRIMARY KEY,
                connection_id TEXT NOT NULL,
                sql           TEXT NOT NULL,
                started_at    INTEGER NOT NULL,
                duration_ms   INTEGER NOT NULL,
                row_count     INTEGER,
                error         TEXT
            );
            CREATE INDEX IF NOT EXISTS history_started
                ON history (started_at DESC);

            CREATE TABLE IF NOT EXISTS folder (
                id        INTEGER PRIMARY KEY,
                parent_id INTEGER REFERENCES folder(id) ON DELETE CASCADE,
                name      TEXT NOT NULL,
                position  INTEGER NOT NULL DEFAULT 0
            );

            CREATE TABLE IF NOT EXISTS saved_query (
                id            INTEGER PRIMARY KEY,
                -- Deleting a folder keeps its queries, at the root. Losing
                -- someone's SQL because they tidied up is not acceptable.
                folder_id     INTEGER REFERENCES folder(id) ON DELETE SET NULL,
                connection_id TEXT,
                name          TEXT NOT NULL,
                body          TEXT NOT NULL,
                updated_at    INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS saved_query_folder ON saved_query (folder_id);
            "#,
        )?;
        Ok(())
    }

    // ---- history -------------------------------------------------------

    pub fn record(&self, entry: &HistoryEntry) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO history (connection_id, sql, started_at, duration_ms, row_count, error)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                entry.connection_id,
                entry.sql,
                entry.started_at,
                entry.duration_ms as i64,
                entry.row_count.map(|v| v as i64),
                entry.error,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Most recent first.
    pub fn recent(&self, limit: u32) -> Result<Vec<(i64, HistoryEntry)>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, connection_id, sql, started_at, duration_ms, row_count, error
             FROM history ORDER BY started_at DESC, id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit], |r| {
            Ok((
                r.get(0)?,
                HistoryEntry {
                    connection_id: r.get(1)?,
                    sql: r.get(2)?,
                    started_at: r.get(3)?,
                    duration_ms: r.get::<_, i64>(4)?.max(0) as u64,
                    row_count: r.get::<_, Option<i64>>(5)?.map(|v| v.max(0) as u64),
                    error: r.get(6)?,
                },
            ))
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Drop rows beyond `max_rows` or older than `max_days`. Called after each
    /// recorded query; cheap because the index is on `started_at`.
    pub fn prune(&self, max_rows: u64, max_days: i64) -> Result<u64> {
        let cutoff = now_ms() - max_days.saturating_mul(24 * 60 * 60 * 1000);
        let old = self
            .conn
            .execute("DELETE FROM history WHERE started_at < ?1", [cutoff])?;

        let over = self.conn.execute(
            "DELETE FROM history WHERE id NOT IN (
                 SELECT id FROM history ORDER BY started_at DESC, id DESC LIMIT ?1
             )",
            [max_rows as i64],
        )?;

        Ok((old + over) as u64)
    }

    // ---- saved queries -------------------------------------------------

    pub fn create_folder(&self, name: impl Into<String>, parent: Option<i64>) -> Result<i64> {
        let name = name.into();
        if name.trim().is_empty() {
            return Err(StoreError::Invalid("folder name is empty".into()));
        }
        if let Some(parent) = parent {
            self.expect_folder(parent)?;
        }
        self.conn.execute(
            "INSERT INTO folder (parent_id, name) VALUES (?1, ?2)",
            params![parent, name],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn rename_folder(&self, id: i64, name: impl Into<String>) -> Result<()> {
        let name = name.into();
        if name.trim().is_empty() {
            return Err(StoreError::Invalid("folder name is empty".into()));
        }
        let changed = self.conn.execute(
            "UPDATE folder SET name = ?1 WHERE id = ?2",
            params![name, id],
        )?;
        if changed == 0 {
            return Err(StoreError::Invalid(format!("no folder {id}")));
        }
        Ok(())
    }

    /// Move a folder under another, or to the root with `parent = None`.
    /// Refuses to move a folder into itself or one of its descendants.
    pub fn move_folder(&self, id: i64, parent: Option<i64>) -> Result<()> {
        self.expect_folder(id)?;
        if let Some(parent) = parent {
            if parent == id {
                return Err(StoreError::Invalid(
                    "cannot move a folder into itself".into(),
                ));
            }
            self.expect_folder(parent)?;
            if self.is_descendant(parent, id)? {
                return Err(StoreError::Invalid(
                    "cannot move a folder into its own descendant".into(),
                ));
            }
        }
        self.conn.execute(
            "UPDATE folder SET parent_id = ?1 WHERE id = ?2",
            params![parent, id],
        )?;
        Ok(())
    }

    /// Delete a folder. Subfolders go with it; queries inside it survive at the
    /// root because `folder_id` is `ON DELETE SET NULL`.
    pub fn delete_folder(&self, id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM folder WHERE id = ?1", [id])?;
        Ok(())
    }

    pub fn folders(&self) -> Result<Vec<(i64, Option<i64>, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, parent_id, name FROM folder ORDER BY position, name")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn save_query(
        &self,
        folder: Option<i64>,
        name: impl Into<String>,
        body: impl Into<String>,
        connection_id: Option<String>,
    ) -> Result<i64> {
        let name = name.into();
        if name.trim().is_empty() {
            return Err(StoreError::Invalid("query name is empty".into()));
        }
        if let Some(folder) = folder {
            self.expect_folder(folder)?;
        }
        self.conn.execute(
            "INSERT INTO saved_query (folder_id, connection_id, name, body, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![folder, connection_id, name, body.into(), now_ms()],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn update_query(&self, id: i64, name: &str, body: &str) -> Result<()> {
        if name.trim().is_empty() {
            return Err(StoreError::Invalid("query name is empty".into()));
        }
        let changed = self.conn.execute(
            "UPDATE saved_query SET name = ?1, body = ?2, updated_at = ?3 WHERE id = ?4",
            params![name, body, now_ms(), id],
        )?;
        if changed == 0 {
            return Err(StoreError::Invalid(format!("no saved query {id}")));
        }
        Ok(())
    }

    pub fn move_query(&self, id: i64, folder: Option<i64>) -> Result<()> {
        if let Some(folder) = folder {
            self.expect_folder(folder)?;
        }
        let changed = self.conn.execute(
            "UPDATE saved_query SET folder_id = ?1, updated_at = ?2 WHERE id = ?3",
            params![folder, now_ms(), id],
        )?;
        if changed == 0 {
            return Err(StoreError::Invalid(format!("no saved query {id}")));
        }
        Ok(())
    }

    pub fn delete_query(&self, id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM saved_query WHERE id = ?1", [id])?;
        Ok(())
    }

    pub fn query(&self, id: i64) -> Result<Option<SavedQuery>> {
        let found = self
            .conn
            .query_row(
                "SELECT id, folder_id, connection_id, name, body, updated_at
                 FROM saved_query WHERE id = ?1",
                [id],
                row_to_query,
            )
            .optional()?;
        Ok(found)
    }

    /// The whole library, for the sidebar: nested folders plus the queries
    /// that live at the root. Orphans (folder deleted elsewhere while the
    /// store was open) appear at the root rather than disappearing.
    pub fn library(&self) -> Result<Library> {
        let mut stmt = self.conn.prepare(
            "SELECT id, folder_id, connection_id, name, body, updated_at
             FROM saved_query ORDER BY name COLLATE NOCASE",
        )?;
        let queries = stmt
            .query_map([], row_to_query)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(stmt);

        let flat = self.folders()?;
        let mut by_folder: std::collections::HashMap<i64, Vec<SavedQuery>> =
            std::collections::HashMap::new();
        let mut library = Library::default();

        for query in queries {
            match query.folder_id {
                Some(id) if flat.iter().any(|(fid, _, _)| *fid == id) => {
                    by_folder.entry(id).or_default().push(query)
                }
                _ => library.root_queries.push(query),
            }
        }

        for (id, parent, _) in &flat {
            let is_root = parent.is_none_or(|p| !flat.iter().any(|(fid, _, _)| *fid == p));
            if is_root {
                library
                    .folders
                    .push(build_folder(*id, &flat, &mut by_folder));
            }
        }

        Ok(library)
    }

    // ---- search --------------------------------------------------------

    /// One search over both history and saved queries. `pattern` is matched
    /// case-insensitively as a substring; `%` and `_` are literal.
    pub fn search(&self, pattern: &str, limit: u32) -> Result<Vec<SearchHit>> {
        let escaped = format!(
            "%{}%",
            pattern
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_")
        );

        let mut stmt = self.conn.prepare(
            r#"
            SELECT id, connection_id, sql, started_at
            FROM history
            WHERE sql LIKE ?1 ESCAPE '\'
            ORDER BY started_at DESC
            LIMIT ?2
            "#,
        )?;
        let mut hits: Vec<SearchHit> = stmt
            .query_map(params![escaped, limit], |r| {
                Ok(SearchHit {
                    kind: HitKind::History,
                    id: r.get(0)?,
                    title: String::new(),
                    snippet: r.get(2)?,
                    connection_id: r.get(1)?,
                    at: r.get(3)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(stmt);

        let mut stmt = self.conn.prepare(
            r#"
            SELECT q.id, q.connection_id, q.name, q.body, q.updated_at, f.name
            FROM saved_query q
            LEFT JOIN folder f ON f.id = q.folder_id
            WHERE q.name LIKE ?1 ESCAPE '\' OR q.body LIKE ?1 ESCAPE '\'
            ORDER BY q.updated_at DESC
            LIMIT ?2
            "#,
        )?;
        hits.extend(
            stmt.query_map(params![escaped, limit], |r| {
                let folder: Option<String> = r.get(5)?;
                let name: String = r.get(2)?;
                Ok(SearchHit {
                    kind: HitKind::SavedQuery,
                    id: r.get(0)?,
                    title: match folder {
                        Some(f) => format!("{f} / {name}"),
                        None => name,
                    },
                    snippet: r.get(3)?,
                    connection_id: r.get(1)?,
                    at: r.get(4)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?,
        );

        // Saved queries first: when you type a name you are usually looking for
        // something you kept, not something you ran.
        hits.sort_by(|a, b| {
            let rank = |k: HitKind| match k {
                HitKind::SavedQuery => 0,
                HitKind::History => 1,
            };
            rank(a.kind).cmp(&rank(b.kind)).then(b.at.cmp(&a.at))
        });
        Ok(hits)
    }

    // ---- helpers -------------------------------------------------------

    fn expect_folder(&self, id: i64) -> Result<()> {
        let exists: Option<i64> = self
            .conn
            .query_row("SELECT id FROM folder WHERE id = ?1", [id], |r| r.get(0))
            .optional()?;
        match exists {
            Some(_) => Ok(()),
            None => Err(StoreError::Invalid(format!("no folder {id}"))),
        }
    }

    /// True if `candidate` is `ancestor` or lives beneath it.
    fn is_descendant(&self, candidate: i64, ancestor: i64) -> Result<bool> {
        let mut current = Some(candidate);
        // Bound the walk: a corrupted cycle must not hang the UI.
        for _ in 0..64 {
            match current {
                Some(id) if id == ancestor => return Ok(true),
                Some(id) => {
                    current = self
                        .conn
                        .query_row("SELECT parent_id FROM folder WHERE id = ?1", [id], |r| {
                            r.get::<_, Option<i64>>(0)
                        })
                        .optional()?
                        .flatten();
                }
                None => return Ok(false),
            }
        }
        Ok(false)
    }
}

/// Depth-first build of one folder and its descendants. The folder list is a
/// personal library, so the per-level scan is cheaper than an index.
fn build_folder(
    id: i64,
    flat: &[(i64, Option<i64>, String)],
    by_folder: &mut std::collections::HashMap<i64, Vec<SavedQuery>>,
) -> Folder {
    let (_, parent_id, name) = flat
        .iter()
        .find(|(fid, _, _)| *fid == id)
        .expect("caller checked the id exists");

    let mut folder = Folder {
        id,
        parent_id: *parent_id,
        name: name.clone(),
        folders: Vec::new(),
        queries: by_folder.remove(&id).unwrap_or_default(),
    };

    for (child_id, child_parent, _) in flat {
        if *child_parent == Some(id) {
            folder
                .folders
                .push(build_folder(*child_id, flat, by_folder));
        }
    }

    folder
}

fn row_to_query(r: &rusqlite::Row<'_>) -> rusqlite::Result<SavedQuery> {
    Ok(SavedQuery {
        id: r.get(0)?,
        folder_id: r.get(1)?,
        connection_id: r.get(2)?,
        name: r.get(3)?,
        body: r.get(4)?,
        updated_at: r.get(5)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        Store::in_memory().expect("store")
    }

    #[test]
    fn records_and_reads_back_newest_first() {
        let s = store();
        let mut first = HistoryEntry::succeeded("dev", "SELECT 1");
        first.started_at = 1_000;
        let mut second = HistoryEntry::succeeded("dev", "SELECT 2");
        second.started_at = 2_000;
        s.record(&first).unwrap();
        s.record(&second).unwrap();

        let recent = s.recent(10).unwrap();
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].1.sql, "SELECT 2");
        assert_eq!(recent[1].1.sql, "SELECT 1");
    }

    #[test]
    fn failed_entries_keep_the_error_and_no_row_count() {
        let s = store();
        s.record(&HistoryEntry::failed(
            "dev",
            "SELECT nope",
            "no such column",
        ))
        .unwrap();
        let (_, entry) = s.recent(1).unwrap().remove(0);
        assert_eq!(entry.error.as_deref(), Some("no such column"));
        assert_eq!(entry.row_count, None);
    }

    #[test]
    fn the_library_walks_folders_and_finds_queries_by_name_or_id() {
        let store = store();
        let reports = store.create_folder("reports", None).unwrap();
        let weekly = store.create_folder("weekly", Some(reports)).unwrap();
        let root = store
            .save_query(None, "all users", "SELECT 1", None)
            .unwrap();
        let nested = store
            .save_query(Some(weekly), "signups", "SELECT 2", None)
            .unwrap();
        let top = store
            .save_query(Some(reports), "revenue", "SELECT 3", None)
            .unwrap();

        let library = store.library().unwrap();
        let walked = library.walk();
        let paths: Vec<(String, &str)> = walked
            .iter()
            .map(|(p, q)| (p.clone(), q.name.as_str()))
            .collect();
        assert_eq!(
            paths,
            [
                (String::new(), "all users"),
                ("reports".to_string(), "revenue"),
                ("reports/weekly".to_string(), "signups"),
            ]
        );

        assert_eq!(library.find("signups").unwrap().id, nested);
        assert_eq!(library.find(&top.to_string()).unwrap().name, "revenue");
        assert_eq!(library.find(&root.to_string()).unwrap().name, "all users");
        assert!(library.find("nope").unwrap_err().contains("no saved query"));
    }

    #[test]
    fn an_ambiguous_name_asks_for_an_id() {
        let store = store();
        let a = store.create_folder("a", None).unwrap();
        let b = store.create_folder("b", None).unwrap();
        store
            .save_query(Some(a), "report", "SELECT 1", None)
            .unwrap();
        store
            .save_query(Some(b), "report", "SELECT 2", None)
            .unwrap();

        let library = store.library().unwrap();
        let err = library.find("report").unwrap_err();
        assert!(err.contains("matches 2 saved queries"), "{err}");
        assert!(err.contains("use the id"), "{err}");
    }

    #[test]
    fn a_corrupt_file_names_itself_and_the_fix() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("spanners.db");
        std::fs::write(&path, b"not a database, just some bytes").unwrap();

        let message = Store::open(&path).unwrap_err().to_string();
        assert!(
            message.contains("not a usable history database"),
            "{message}"
        );
        assert!(message.contains("spanners.db"), "{message}");
        assert!(message.contains("move it aside"), "{message}");
    }

    #[test]
    fn only_corruption_codes_are_called_corruption() {
        let failure =
            |code: i32| rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(code), None);
        use rusqlite::ffi;
        assert!(is_corruption(&failure(ffi::SQLITE_CORRUPT)));
        assert!(is_corruption(&failure(ffi::SQLITE_NOTADB)));
        // A healthy database that is merely busy, full or read-only is not a
        // file to discard.
        assert!(!is_corruption(&failure(ffi::SQLITE_BUSY)));
        assert!(!is_corruption(&failure(ffi::SQLITE_FULL)));
        assert!(!is_corruption(&failure(ffi::SQLITE_READONLY)));
        assert!(!is_corruption(&failure(ffi::SQLITE_PERM)));
    }

    #[test]
    fn a_directory_is_reported_as_unopenable_not_as_corrupt() {
        // A path that is a directory cannot be opened as a database. That is a
        // wrong path to fix, not a file to move aside, so it must not get the
        // corruption message.
        let dir = tempfile::TempDir::new().unwrap();
        let message = Store::open(dir.path()).unwrap_err().to_string();
        assert!(message.contains("could not open"), "{message}");
        assert!(!message.contains("move it aside"), "{message}");
    }

    #[test]
    fn prune_enforces_row_and_age_limits() {
        let s = store();
        let now = now_ms();
        for i in 0..20 {
            let mut e = HistoryEntry::succeeded("dev", format!("SELECT {i}"));
            e.started_at = now - i * 1_000;
            s.record(&e).unwrap();
        }
        let mut ancient = HistoryEntry::succeeded("dev", "SELECT old");
        ancient.started_at = now - (HISTORY_MAX_DAYS + 1) * 24 * 60 * 60 * 1000;
        s.record(&ancient).unwrap();

        let removed = s.prune(5, HISTORY_MAX_DAYS).unwrap();
        // 21 rows: 1 too old, 15 over the limit.
        assert_eq!(removed, 16);
        let left = s.recent(100).unwrap();
        assert_eq!(left.len(), 5);
        assert!(left.iter().all(|(_, e)| e.sql != "SELECT old"));
    }

    #[test]
    fn folders_nest_and_queries_attach_to_them() {
        let s = store();
        let reports = s.create_folder("reports", None).unwrap();
        let daily = s.create_folder("daily", Some(reports)).unwrap();
        s.save_query(Some(daily), "revenue", "SELECT 1", Some("prod".into()))
            .unwrap();
        s.save_query(None, "scratch", "SELECT 2", None).unwrap();

        let lib = s.library().unwrap();
        assert_eq!(lib.root_queries.len(), 1);
        assert_eq!(lib.root_queries[0].name, "scratch");
        assert_eq!(lib.folders.len(), 1);
        let root = &lib.folders[0];
        assert_eq!(root.name, "reports");
        assert_eq!(root.folders.len(), 1);
        assert_eq!(root.folders[0].name, "daily");
        assert_eq!(root.folders[0].queries.len(), 1);
        assert_eq!(root.folders[0].queries[0].name, "revenue");
    }

    #[test]
    fn deleting_a_folder_keeps_its_queries_at_the_root() {
        let s = store();
        let folder = s.create_folder("temp", None).unwrap();
        let id = s
            .save_query(Some(folder), "keep me", "SELECT 1", None)
            .unwrap();

        s.delete_folder(folder).unwrap();

        let q = s.query(id).unwrap().expect("query survives");
        assert_eq!(q.folder_id, None);
        let lib = s.library().unwrap();
        assert!(lib.folders.is_empty());
        assert_eq!(lib.root_queries.len(), 1);
    }

    #[test]
    fn deleting_a_folder_takes_its_subfolders_but_not_their_queries() {
        let s = store();
        let parent = s.create_folder("parent", None).unwrap();
        let child = s.create_folder("child", Some(parent)).unwrap();
        let id = s.save_query(Some(child), "deep", "SELECT 1", None).unwrap();

        s.delete_folder(parent).unwrap();

        assert!(s.folders().unwrap().is_empty());
        assert_eq!(s.query(id).unwrap().unwrap().folder_id, None);
    }

    #[test]
    fn a_folder_cannot_move_into_its_own_descendant() {
        let s = store();
        let a = s.create_folder("a", None).unwrap();
        let b = s.create_folder("b", Some(a)).unwrap();
        let c = s.create_folder("c", Some(b)).unwrap();

        let err = s.move_folder(a, Some(c)).unwrap_err();
        assert!(err.to_string().contains("descendant"), "{err}");

        let err = s.move_folder(a, Some(a)).unwrap_err();
        assert!(err.to_string().contains("itself"), "{err}");

        // The legal direction still works.
        s.move_folder(c, None).unwrap();
        let lib = s.library().unwrap();
        assert_eq!(lib.folders.len(), 2);
    }

    #[test]
    fn empty_names_are_rejected() {
        let s = store();
        assert!(s.create_folder("  ", None).is_err());
        assert!(s.save_query(None, "", "SELECT 1", None).is_err());
    }

    #[test]
    fn saving_into_a_missing_folder_is_an_error() {
        let s = store();
        assert!(s.save_query(Some(999), "x", "SELECT 1", None).is_err());
        assert!(s.create_folder("x", Some(999)).is_err());
    }

    #[test]
    fn search_covers_history_and_saved_queries() {
        let s = store();
        s.record(&HistoryEntry::succeeded(
            "dev",
            "SELECT * FROM customers WHERE region = 'emea'",
        ))
        .unwrap();
        let folder = s.create_folder("Ops", None).unwrap();
        s.save_query(
            Some(folder),
            "churn by region",
            "SELECT region, count(*) FROM accounts GROUP BY 1",
            Some("dev".into()),
        )
        .unwrap();

        let hits = s.search("region", 20).unwrap();
        assert_eq!(hits.len(), 2);
        // Saved queries rank above history: a name match is usually the intent.
        assert_eq!(hits[0].kind, HitKind::SavedQuery);
        assert_eq!(hits[0].title, "Ops / churn by region");
        assert_eq!(hits[1].kind, HitKind::History);
    }

    #[test]
    fn search_matches_names_and_is_case_insensitive() {
        let s = store();
        s.save_query(None, "Monthly Revenue", "SELECT 1", None)
            .unwrap();
        let hits = s.search("monthly", 20).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].kind, HitKind::SavedQuery);
    }

    #[test]
    fn search_treats_like_wildcards_as_literal_text() {
        let s = store();
        s.record(&HistoryEntry::succeeded("dev", "SELECT '100%'"))
            .unwrap();
        s.record(&HistoryEntry::succeeded("dev", "SELECT '1000'"))
            .unwrap();

        let hits = s.search("100%", 20).unwrap();
        assert_eq!(hits.len(), 1, "got {hits:?}");
        assert!(hits[0].snippet.contains("100%"));
    }

    #[test]
    fn search_respects_the_limit() {
        let s = store();
        for i in 0..30 {
            s.record(&HistoryEntry::succeeded(
                "dev",
                format!("SELECT {i} FROM t"),
            ))
            .unwrap();
        }
        assert_eq!(s.search("FROM t", 5).unwrap().len(), 5);
    }

    #[test]
    fn updating_a_query_changes_the_body_and_timestamp() {
        let s = store();
        let id = s.save_query(None, "q", "SELECT 1", None).unwrap();
        let before = s.query(id).unwrap().unwrap().updated_at;

        std::thread::sleep(std::time::Duration::from_millis(2));
        s.update_query(id, "q2", "SELECT 2").unwrap();

        let after = s.query(id).unwrap().unwrap();
        assert_eq!(after.name, "q2");
        assert_eq!(after.body, "SELECT 2");
        assert!(after.updated_at >= before);
    }

    #[test]
    fn a_store_on_disk_survives_reopening() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("spanners.db");
        {
            let s = Store::open(&path).unwrap();
            s.record(&HistoryEntry::succeeded("dev", "SELECT 1"))
                .unwrap();
        }
        let s = Store::open(&path).unwrap();
        assert_eq!(s.recent(10).unwrap().len(), 1);
    }
}
