//! The seam the UI calls: policy + driver, in one place.
//!
//! Kept separate from `policy` so the guardrails can be tested with a real
//! driver (SQLite) without a UI and without a network.

use spanners_driver_api::{Dialect, DriverError, QueryEvent, QueryId, Session};
use tokio::sync::mpsc;

use crate::policy::{Policy, Verdict, Violation};

/// What the caller should do next. `Run` already owns the stream, so the UI can
/// start rendering before the first row arrives.
#[derive(Debug)]
pub enum Gate {
    Run {
        query_id: QueryId,
        events: mpsc::Receiver<QueryEvent>,
    },
    /// The policy allows the statement only with explicit user consent.
    NeedsConfirmation { reasons: Vec<Violation> },
    /// The policy refuses the statement. Nothing was sent to the server.
    Blocked { reasons: Vec<Violation> },
}

/// Classify, then execute. `confirmed` must come from a user action on the
/// exact SQL being run — never default it to `true` in a caller.
pub async fn execute_guarded(
    session: &dyn Session,
    policy: &Policy,
    sql: &str,
    dialect: Dialect,
    confirmed: bool,
) -> Result<Gate, DriverError> {
    let in_transaction = session.in_transaction().await;
    match policy.judge_in(sql, dialect, in_transaction) {
        Verdict::Block { reasons } => Ok(Gate::Blocked { reasons }),
        Verdict::Confirm { reasons } if !confirmed => Ok(Gate::NeedsConfirmation { reasons }),
        Verdict::Confirm { .. } | Verdict::Allow => {
            let (tx, rx) = mpsc::channel(8);
            let query_id = session.execute(sql, policy.exec_opts(), tx).await?;
            Ok(Gate::Run {
                query_id,
                events: rx,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::Profile;
    use spanners_driver_api::{ConnConfig, Driver, ExecOpts, SessionOpts};
    use spanners_driver_sqlite::SqliteDriver;
    use tempfile::TempDir;

    async fn session(dir: &TempDir, read_only: bool) -> Box<dyn Session> {
        SqliteDriver::new()
            .connect(
                &ConnConfig::sqlite(dir.path().join("gate.db").to_string_lossy().to_string()),
                SessionOpts {
                    read_only,
                    timeout: None,
                },
            )
            .await
            .unwrap()
    }

    async fn seed(s: &dyn Session) {
        // Run through the gate with a permissive policy so the setup itself is
        // covered by the same path the app uses.
        let policy = Policy::preset(Profile::Local);
        if let Gate::Run { events, .. } = execute_guarded(
            s,
            &policy,
            "CREATE TABLE t (id INTEGER)",
            Dialect::Sqlite,
            false,
        )
        .await
        .unwrap()
        {
            let mut events = events;
            while events.recv().await.is_some() {}
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn production_never_reaches_the_driver() {
        let dir = TempDir::new().unwrap();
        let s = session(&dir, false).await;
        seed(s.as_ref()).await;

        let policy = Policy::preset(Profile::Production);
        let gate = execute_guarded(
            s.as_ref(),
            &policy,
            "INSERT INTO t VALUES (1)",
            Dialect::Sqlite,
            false,
        )
        .await
        .unwrap();

        match gate {
            Gate::Blocked { reasons } => assert_eq!(reasons[0], Violation::ReadOnlySession),
            other => panic!("expected Blocked, got {other:?}"),
        }
    }

    async fn begin(s: &dyn Session) {
        let (tx, mut rx) = mpsc::channel(8);
        s.execute("BEGIN", ExecOpts::default(), tx).await.unwrap();
        while rx.recv().await.is_some() {}
        assert!(s.in_transaction().await);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn staging_requires_confirmation_for_where_less_update() {
        let dir = TempDir::new().unwrap();
        let s = session(&dir, false).await;
        seed(s.as_ref()).await;

        let policy = Policy::preset(Profile::Staging);
        let sql = "UPDATE t SET id = 2";

        // Staging also requires an explicit transaction, and that is a block,
        // not a confirmation: there is no user answer that makes it safe.
        match execute_guarded(s.as_ref(), &policy, sql, Dialect::Sqlite, false)
            .await
            .unwrap()
        {
            Gate::Blocked { reasons } => assert!(
                reasons
                    .iter()
                    .any(|r| matches!(r, Violation::WriteOutsideTransaction)),
                "{reasons:?}"
            ),
            other => panic!("expected Blocked, got {other:?}"),
        }

        begin(s.as_ref()).await;

        match execute_guarded(s.as_ref(), &policy, sql, Dialect::Sqlite, false)
            .await
            .unwrap()
        {
            Gate::NeedsConfirmation { reasons } => {
                assert_eq!(reasons[0], Violation::WriteWithoutWhere)
            }
            other => panic!("expected NeedsConfirmation, got {other:?}"),
        }

        match execute_guarded(s.as_ref(), &policy, sql, Dialect::Sqlite, true)
            .await
            .unwrap()
        {
            Gate::Run { mut events, .. } => {
                assert!(events.recv().await.is_some());
            }
            other => panic!("confirmed statement must run, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn production_reads_run() {
        let dir = TempDir::new().unwrap();
        let s = session(&dir, false).await;
        seed(s.as_ref()).await;

        let policy = Policy::preset(Profile::Production);
        match execute_guarded(
            s.as_ref(),
            &policy,
            "SELECT * FROM t",
            Dialect::Sqlite,
            false,
        )
        .await
        .unwrap()
        {
            Gate::Run { mut events, .. } => {
                let first = events.recv().await.expect("meta event");
                assert!(matches!(first, QueryEvent::Meta { .. }));
            }
            other => panic!("expected Run, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn row_cap_is_derived_from_the_policy() {
        let policy = Policy::preset(Profile::Production);
        assert_eq!(policy.exec_opts().max_rows, Some(200));
        assert_eq!(
            policy.exec_opts(),
            ExecOpts {
                max_rows: Some(200),
                timeout: policy.timeout,
                read_only: true
            }
        );
    }
}
