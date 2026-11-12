//! Safety policy.
//!
//! Three layers, of which only the first is a guarantee:
//!
//! 1. the driver applies server-side read-only/timeout settings before the user
//!    can type (`SET default_transaction_read_only`, `PRAGMA query_only`, ...);
//! 2. this module classifies SQL and produces a [`Verdict`] — advisory, and
//!    treated as *unsafe* when parsing fails;
//! 3. the row cap is enforced by the driver as a fetch stop.
//!
//! Nothing in this file should ever be the only thing standing between a user
//! and a production table.

use std::ops::ControlFlow;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sqlparser::ast::{visit_statements, Statement};
use sqlparser::dialect::{Dialect as SqlDialect, MySqlDialect, PostgreSqlDialect, SQLiteDialect};
use sqlparser::parser::Parser;

use spanners_driver_api::{Dialect, ExecOpts};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Profile {
    /// Your laptop. Anything goes.
    Local,
    /// Shared but not customer-facing. Writes allowed, transaction required.
    Staging,
    /// Live data. Read-only unless explicitly unlocked for this session.
    Production,
}

impl Profile {
    pub fn as_str(self) -> &'static str {
        match self {
            Profile::Local => "local",
            Profile::Staging => "staging",
            Profile::Production => "production",
        }
    }
}

/// The effective rules for one session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    pub profile: Profile,
    /// `None` = no cap.
    pub max_rows: Option<u32>,
    pub timeout: Option<Duration>,
    pub read_only: bool,
    pub allow_ddl: bool,
    pub allow_multi_statement: bool,
    pub require_transaction: bool,
}

impl Policy {
    pub fn preset(profile: Profile) -> Self {
        match profile {
            Profile::Local => Self {
                profile,
                max_rows: None,
                timeout: None,
                read_only: false,
                allow_ddl: true,
                allow_multi_statement: true,
                require_transaction: false,
            },
            Profile::Staging => Self {
                profile,
                max_rows: Some(5_000),
                timeout: Some(Duration::from_secs(60)),
                read_only: false,
                allow_ddl: false,
                allow_multi_statement: true,
                require_transaction: true,
            },
            Profile::Production => Self {
                profile,
                max_rows: Some(200),
                timeout: Some(Duration::from_secs(15)),
                read_only: true,
                allow_ddl: false,
                allow_multi_statement: false,
                require_transaction: false,
            },
        }
    }

    /// Applies a connection's overrides on top of its profile preset.
    pub fn with_overrides(mut self, o: &PolicyOverrides) -> Self {
        if let Some(n) = o.max_rows {
            self.max_rows = if n == 0 { None } else { Some(n) };
        }
        if let Some(ms) = o.timeout_ms {
            self.timeout = if ms == 0 {
                None
            } else {
                Some(Duration::from_millis(ms))
            };
        }
        if let Some(ro) = o.read_only {
            self.read_only = ro;
        }
        self
    }

    pub fn exec_opts(&self) -> ExecOpts {
        ExecOpts {
            max_rows: self.max_rows,
            timeout: self.timeout,
            read_only: self.read_only,
        }
    }

    /// Classify and judge one editor buffer.
    pub fn judge(&self, sql: &str, dialect: Dialect) -> Verdict {
        let c = classify(sql, dialect);
        let mut blocking = Vec::new();
        let mut confirming = Vec::new();

        // An unparseable buffer cannot be proven safe, so it is never *silently*
        // run on a guarded profile.
        if let Some(err) = &c.parse_error {
            if self.profile != Profile::Local {
                confirming.push(Violation::Unparseable(err.clone()));
            }
        }

        if c.statements.len() > 1 && !self.allow_multi_statement {
            blocking.push(Violation::MultipleStatements(c.statements.len()));
        }

        for kind in &c.statements {
            match kind {
                StmtKind::Read | StmtKind::Other => {}
                StmtKind::Ddl => {
                    if !self.allow_ddl {
                        blocking.push(Violation::Ddl);
                    }
                }
                StmtKind::Write { targeted } => {
                    if self.read_only {
                        blocking.push(Violation::ReadOnlySession);
                    } else if !targeted {
                        confirming.push(Violation::WriteWithoutWhere);
                    }
                }
            }
        }

        blocking.sort_by_key(|v| v.code());
        blocking.dedup_by_key(|v| v.code());
        confirming.sort_by_key(|v| v.code());
        confirming.dedup_by_key(|v| v.code());

        if !blocking.is_empty() {
            Verdict::Block { reasons: blocking }
        } else if !confirming.is_empty() {
            Verdict::Confirm {
                reasons: confirming,
            }
        } else {
            Verdict::Allow
        }
    }
}

/// Per-connection overrides of the profile preset. `None` means "use the preset".
/// `max_rows = 0` and `timeout_ms = 0` mean "no limit".
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PolicyOverrides {
    pub max_rows: Option<u32>,
    pub timeout_ms: Option<u64>,
    pub read_only: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StmtKind {
    Read,
    /// A row-mutating statement. `targeted` is true when the statement names
    /// the rows it touches (INSERT, UPDATE ... WHERE); false for UPDATE/DELETE
    /// without a predicate and for bulk operations.
    Write {
        targeted: bool,
    },
    Ddl,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Classification {
    pub statements: Vec<StmtKind>,
    pub parse_error: Option<String>,
}

impl Classification {
    pub fn is_read_only(&self) -> bool {
        self.parse_error.is_none()
            && self
                .statements
                .iter()
                .all(|s| matches!(s, StmtKind::Read | StmtKind::Other))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum Verdict {
    Allow,
    Confirm { reasons: Vec<Violation> },
    Block { reasons: Vec<Violation> },
}

impl Verdict {
    pub fn is_allowed(&self) -> bool {
        matches!(self, Verdict::Allow)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum Violation {
    MultipleStatements(usize),
    Ddl,
    WriteWithoutWhere,
    ReadOnlySession,
    Unparseable(String),
}

impl Violation {
    /// Stable code for the UI to key on; keeps messages out of logic.
    pub fn code(&self) -> u8 {
        match self {
            Violation::MultipleStatements(_) => 0,
            Violation::ReadOnlySession => 1,
            Violation::Ddl => 2,
            Violation::WriteWithoutWhere => 3,
            Violation::Unparseable(_) => 4,
        }
    }

    pub fn message(&self) -> String {
        match self {
            Violation::MultipleStatements(n) => {
                format!("{n} statements in one buffer; this connection allows one")
            }
            Violation::Ddl => "schema changes are blocked on this connection".into(),
            Violation::WriteWithoutWhere => {
                "UPDATE/DELETE without a WHERE clause affects every row".into()
            }
            Violation::ReadOnlySession => "this connection is read-only".into(),
            Violation::Unparseable(e) => {
                format!("could not parse the statement ({e}); confirm before running")
            }
        }
    }
}

fn sql_dialect(dialect: Dialect) -> Box<dyn SqlDialect> {
    match dialect {
        Dialect::Postgres => Box::new(PostgreSqlDialect {}),
        Dialect::MySql => Box::new(MySqlDialect {}),
        Dialect::Sqlite => Box::new(SQLiteDialect {}),
    }
}

pub fn classify(sql: &str, dialect: Dialect) -> Classification {
    if sql.trim().is_empty() {
        return Classification::default();
    }
    match Parser::parse_sql(sql_dialect(dialect).as_ref(), sql) {
        Ok(stmts) => Classification {
            statements: stmts.iter().map(kind_of).collect(),
            parse_error: None,
        },
        Err(e) => Classification {
            statements: Vec::new(),
            parse_error: Some(e.to_string()),
        },
    }
}

fn kind_of(stmt: &Statement) -> StmtKind {
    if let Some(k) = direct_write(stmt) {
        return k;
    }

    // A CTE can hide DML: `WITH x AS (DELETE FROM t RETURNING *) SELECT * FROM x`.
    // A leading `WITH` must not launder a write into a "read".
    let mut nested: Option<StmtKind> = None;
    let _ = visit_statements(stmt, |inner| match direct_write(inner) {
        Some(k @ StmtKind::Write { .. }) => {
            nested = Some(k);
            ControlFlow::Break(())
        }
        _ => ControlFlow::Continue(()),
    });
    if let Some(k) = nested {
        return k;
    }

    match head_keyword(stmt).as_str() {
        // Reads and session control.
        "SELECT" | "WITH" | "VALUES" | "TABLE" | "SHOW" | "DESCRIBE" | "DESC" | "EXPLAIN"
        | "PRAGMA" | "SET" | "BEGIN" | "START" | "COMMIT" | "ROLLBACK" | "SAVEPOINT"
        | "RELEASE" | "USE" | "RESET" | "DISCARD" => StmtKind::Read,

        // Row-mutating statements that name their rows.
        "INSERT" | "REPLACE" | "MERGE" => StmtKind::Write { targeted: true },

        // Bulk or opaque mutations: cannot be proven targeted, so ask.
        "UPDATE" | "DELETE" | "COPY" | "LOAD" | "CALL" | "EXEC" | "EXECUTE" => {
            StmtKind::Write { targeted: false }
        }

        "CREATE" | "ALTER" | "DROP" | "TRUNCATE" | "GRANT" | "REVOKE" | "COMMENT" | "RENAME" => {
            StmtKind::Ddl
        }

        _ => StmtKind::Other,
    }
}

fn direct_write(stmt: &Statement) -> Option<StmtKind> {
    match stmt {
        Statement::Update(u) => Some(StmtKind::Write {
            targeted: u.selection.is_some(),
        }),
        Statement::Delete(d) => Some(StmtKind::Write {
            targeted: d.selection.is_some(),
        }),
        // INSERT/MERGE name their rows by construction, so they are not the
        // "you are about to touch every row" hazard.
        Statement::Insert(_) | Statement::Merge(_) => Some(StmtKind::Write { targeted: true }),
        _ => None,
    }
}

/// The statement's leading keyword, from its normalised text. Robust across
/// `sqlparser` AST reshuffles, which is why it is used only for the long tail of
/// classification and never for WHERE detection.
fn head_keyword(stmt: &Statement) -> String {
    let text = stmt.to_string();
    let text = text.trim_start_matches(|c: char| c.is_whitespace() || c == '(');
    text.split_whitespace()
        .next()
        .unwrap_or("")
        .trim_matches(|c: char| !c.is_ascii_alphabetic())
        .to_ascii_uppercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn judge(sql: &str, profile: Profile) -> Verdict {
        Policy::preset(profile).judge(sql, Dialect::Postgres)
    }

    #[test]
    fn local_allows_everything() {
        assert!(judge("DROP TABLE users", Profile::Local).is_allowed());
        assert!(judge("SELECT 1; SELECT 2", Profile::Local).is_allowed());
    }

    #[test]
    fn production_blocks_writes_at_the_policy_layer() {
        assert!(matches!(
            judge("UPDATE users SET x = 1", Profile::Production),
            Verdict::Block { .. }
        ));
        assert!(matches!(
            judge("INSERT INTO users VALUES (1)", Profile::Production),
            Verdict::Block { .. }
        ));
    }

    #[test]
    fn production_blocks_statement_chaining() {
        let v = judge("SELECT 1; DROP TABLE users", Profile::Production);
        match v {
            Verdict::Block { reasons } => {
                assert!(reasons.contains(&Violation::MultipleStatements(2)));
                assert!(reasons.contains(&Violation::Ddl));
            }
            other => panic!("expected block, got {other:?}"),
        }
    }

    #[test]
    fn production_allows_reads() {
        let v = judge("SELECT * FROM users WHERE id = $1", Profile::Production);
        assert!(v.is_allowed(), "got {v:?}");
    }

    #[test]
    fn cte_writes_are_not_mistaken_for_reads() {
        // A `WITH` prefix must not launder a write.
        let v = judge(
            "WITH gone AS (DELETE FROM users RETURNING *) SELECT * FROM gone",
            Profile::Staging,
        );
        assert!(!v.is_allowed(), "got {v:?}");
    }

    #[test]
    fn staging_confirms_on_update_without_where() {
        match judge("UPDATE users SET active = false", Profile::Staging) {
            Verdict::Confirm { reasons } => {
                assert!(reasons.contains(&Violation::WriteWithoutWhere))
            }
            other => panic!("expected confirm, got {other:?}"),
        }
    }

    #[test]
    fn staging_allows_targeted_update() {
        assert!(judge(
            "UPDATE users SET active = false WHERE id = 3",
            Profile::Staging
        )
        .is_allowed());
    }

    #[test]
    fn staging_blocks_ddl() {
        assert!(matches!(
            judge("ALTER TABLE users ADD COLUMN x int", Profile::Staging),
            Verdict::Block { .. }
        ));
    }

    #[test]
    fn unparseable_is_unsafe_outside_local() {
        match judge("SELEC * FORM users", Profile::Production) {
            Verdict::Confirm { reasons } => {
                assert!(matches!(reasons[0], Violation::Unparseable(_)))
            }
            other => panic!("expected confirm, got {other:?}"),
        }
        assert!(judge("SELEC * FORM users", Profile::Local).is_allowed());
    }

    #[test]
    fn mysql_and_sqlite_dialects_parse_their_own_syntax() {
        assert!(classify("SELECT `a` FROM `t` LIMIT 1", Dialect::MySql)
            .parse_error
            .is_none());
        assert!(
            classify("SELECT * FROM t LIMIT 1 OFFSET 2", Dialect::Sqlite)
                .parse_error
                .is_none()
        );
    }
}
