use super::*;
use spanners_core::export::{Format, NullRepr};
use spanners_core::HitKind;
use spanners_core::Profile;
use spanners_driver_api::ExecOpts;
use std::time::Duration;
use tempfile::TempDir;

fn local_sqlite(dir: &TempDir) -> Connection {
    let mut c = Connection::new("scratch", "sqlite", Profile::Local);
    c.database = Some(dir.path().join("app.db").to_string_lossy().to_string());
    c
}

async fn app_with(dir: &TempDir) -> App {
    App::open_with_secrets(dir.path(), Box::new(MemorySecrets::new())).expect("open")
}

async fn drain(run: &mut Run) -> Vec<QueryEvent> {
    let mut events = Vec::new();
    while let Some(ev) = run.recv().await {
        events.push(ev);
    }
    events
}

#[tokio::test]
async fn opens_with_no_state_and_creates_nothing_until_asked() {
    let dir = TempDir::new().unwrap();
    let app = app_with(&dir).await;
    assert!(app.connections().is_empty());
    assert!(app.history(10).unwrap().is_empty());
    assert!(app.library().unwrap().folders.is_empty());
}

#[tokio::test]
async fn connections_survive_a_restart() {
    let dir = TempDir::new().unwrap();
    let conn = local_sqlite(&dir);
    let id = conn.id.clone();
    {
        let mut app = app_with(&dir).await;
        app.upsert_connection(conn).unwrap();
    }
    let app = app_with(&dir).await;
    assert_eq!(app.connections().len(), 1);
    assert_eq!(app.connection(&id).unwrap().name, "scratch");
}

#[tokio::test]
async fn connecting_a_local_sqlite_file_works_without_a_password() {
    let dir = TempDir::new().unwrap();
    let conn = local_sqlite(&dir);
    let id = conn.id.clone();
    let mut app = app_with(&dir).await;
    app.upsert_connection(conn).unwrap();

    let info = app.connect(&id, None).await.unwrap();
    assert_eq!(info.driver, "sqlite");
    assert!(!info.read_only);
    assert!(app.is_connected(&id));

    // Idempotent: a second connect returns the live session.
    let again = app.connect(&id, None).await.unwrap();
    assert_eq!(again.server_version, info.server_version);
    app.disconnect(&id).await.unwrap();
    assert!(!app.is_connected(&id));
}

#[tokio::test]
async fn a_server_connection_without_a_password_asks_for_one() {
    let dir = TempDir::new().unwrap();
    let mut conn = Connection::new("live", "postgres", Profile::Production);
    conn.host = Some("127.0.0.1".into());
    conn.port = Some(1);
    conn.database = Some("nope".into());
    conn.username = Some("ada".into());
    let id = conn.id.clone();

    let mut app = app_with(&dir).await;
    app.upsert_connection(conn).unwrap();

    match app.connect(&id, None).await {
        Err(AppError::PasswordRequired { id: asked }) => assert_eq!(asked, id),
        other => panic!("expected PasswordRequired, got {other:?}"),
    }
    assert!(!app.is_connected(&id));
}

#[tokio::test]
async fn connecting_an_unknown_connection_is_an_error_not_a_panic() {
    let dir = TempDir::new().unwrap();
    let mut app = app_with(&dir).await;
    assert!(matches!(
        app.connect("nope", None).await,
        Err(AppError::UnknownConnection(_))
    ));
}

#[tokio::test]
async fn running_sql_streams_rows_and_records_history() {
    let dir = TempDir::new().unwrap();
    let conn = local_sqlite(&dir);
    let id = conn.id.clone();
    let mut app = app_with(&dir).await;
    app.upsert_connection(conn).unwrap();
    app.connect(&id, None).await.unwrap();

    let mut run = match app
        .run(
            &id,
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 3)
             SELECT i FROM n",
            false,
        )
        .await
        .unwrap()
    {
        RunOutcome::Running(run) => run,
        other => panic!("expected to run, got {other:?}"),
    };

    let events = drain(&mut run).await;
    let rows = events
        .iter()
        .map(|e| match e {
            QueryEvent::Batch { rows, .. } => rows.len(),
            _ => 0,
        })
        .sum::<usize>();
    assert_eq!(rows, 3);

    let history = app.history(10).unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].1.row_count, Some(3));
    assert!(history[0].1.error.is_none());
}

#[tokio::test]
async fn a_failed_query_is_recorded_with_its_error() {
    let dir = TempDir::new().unwrap();
    let conn = local_sqlite(&dir);
    let id = conn.id.clone();
    let mut app = app_with(&dir).await;
    app.upsert_connection(conn).unwrap();
    app.connect(&id, None).await.unwrap();

    let mut run = match app.run(&id, "SELECT * FROM missing", false).await.unwrap() {
        RunOutcome::Running(run) => run,
        other => panic!("expected to run, got {other:?}"),
    };
    drain(&mut run).await;

    let history = app.history(10).unwrap();
    assert_eq!(history.len(), 1);
    let entry = &history[0].1;
    assert!(entry.row_count.is_none());
    assert!(
        entry
            .error
            .as_deref()
            .unwrap_or("")
            .contains("no such table"),
        "{:?}",
        entry.error
    );
}

#[tokio::test]
async fn history_records_a_query_whose_run_is_dropped() {
    let dir = TempDir::new().unwrap();
    let conn = local_sqlite(&dir);
    let id = conn.id.clone();
    let mut app = app_with(&dir).await;
    app.upsert_connection(conn).unwrap();
    app.connect(&id, None).await.unwrap();

    let run = app.run(&id, "SELECT 1", false).await.unwrap();
    assert!(matches!(run, RunOutcome::Running(_)));
    drop(run);

    assert_eq!(app.history(10).unwrap().len(), 1);
}

#[tokio::test]
async fn the_production_profile_blocks_writes_before_they_reach_the_server() {
    let dir = TempDir::new().unwrap();
    let mut conn = local_sqlite(&dir);
    conn.profile = Profile::Production;
    let id = conn.id.clone();
    // A read-only open cannot create a database file, which is the correct
    // behaviour; seed one so the test is about the policy, not the file.
    std::fs::File::create(conn.database.as_ref().unwrap()).unwrap();
    let mut app = app_with(&dir).await;
    app.upsert_connection(conn).unwrap();
    app.connect(&id, None).await.unwrap();

    match app
        .run(&id, "CREATE TABLE t (id int)", false)
        .await
        .unwrap()
    {
        RunOutcome::Blocked { reasons } => {
            assert!(!reasons.is_empty());
        }
        other => panic!("expected Blocked, got {other:?}"),
    }
    // Nothing ran, so nothing was recorded.
    assert!(app.history(10).unwrap().is_empty());
}

#[tokio::test]
async fn running_without_a_session_is_an_error() {
    let dir = TempDir::new().unwrap();
    let mut app = app_with(&dir).await;
    assert!(matches!(
        app.run("nope", "SELECT 1", false).await,
        Err(AppError::NotConnected(_))
    ));
}

#[tokio::test]
async fn export_uses_the_connections_limits() {
    let dir = TempDir::new().unwrap();
    let mut conn = local_sqlite(&dir);
    conn.overrides.max_rows = Some(2);
    let id = conn.id.clone();
    let mut app = app_with(&dir).await;
    app.upsert_connection(conn).unwrap();
    app.connect(&id, None).await.unwrap();

    let sql = "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 10)
               SELECT i FROM n";
    let (text, summary) = app
        .export_to_string(
            &id,
            sql,
            &ExportOpts {
                format: Format::Csv,
                header: true,
                null: NullRepr::Empty,
            },
        )
        .await
        .unwrap();

    assert_eq!(summary.rows, 2);
    assert!(summary.truncated);
    assert_eq!(text.lines().count(), 3);
}

#[tokio::test]
async fn saved_queries_and_history_share_one_search() {
    let dir = TempDir::new().unwrap();
    let conn = local_sqlite(&dir);
    let id = conn.id.clone();
    let mut app = app_with(&dir).await;
    app.upsert_connection(conn).unwrap();
    app.connect(&id, None).await.unwrap();

    let mut run = match app.run(&id, "SELECT 1 AS revenue", false).await.unwrap() {
        RunOutcome::Running(run) => run,
        other => panic!("expected to run, got {other:?}"),
    };
    drain(&mut run).await;

    let folder = app.create_folder("Reports", None).unwrap();
    app.save_query(Some(folder), "monthly revenue", "SELECT 2", Some(&id))
        .unwrap();

    let hits = app.search("revenue", 20).unwrap();
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].kind, HitKind::SavedQuery);
    assert_eq!(hits[1].kind, HitKind::History);
}

#[tokio::test]
async fn removing_a_connection_closes_it_and_forgets_its_password() {
    let dir = TempDir::new().unwrap();
    let conn = local_sqlite(&dir);
    let id = conn.id.clone();
    let mut app = app_with(&dir).await;
    app.upsert_connection(conn).unwrap();
    app.connect(&id, None).await.unwrap();
    app.remember_password(&id, "secret").unwrap();
    assert!(app.has_remembered_password(&id));

    app.remove_connection(&id).await.unwrap();

    assert!(app.connections().is_empty());
    assert!(!app.is_connected(&id));
    assert!(!app.has_remembered_password(&id));
}

#[tokio::test]
async fn a_remembered_password_is_used_for_the_next_connect() {
    let dir = TempDir::new().unwrap();
    let mut conn = Connection::new("live", "postgres", Profile::Production);
    conn.host = Some("127.0.0.1".into());
    conn.port = Some(1);
    conn.database = Some("nope".into());
    conn.username = Some("ada".into());
    let id = conn.id.clone();
    let mut app = app_with(&dir).await;
    app.upsert_connection(conn).unwrap();

    app.remember_password(&id, "hunter2").unwrap();
    // With a password present the driver is reached, so the failure is a
    // connection error rather than a prompt.
    match app.connect(&id, None).await {
        Err(AppError::Driver(_)) => {}
        other => panic!("expected a driver error, got {other:?}"),
    }
}

#[tokio::test]
async fn testing_a_connection_does_not_adopt_or_save_it() {
    let dir = TempDir::new().unwrap();
    let app = app_with(&dir).await;
    let conn = local_sqlite(&dir);

    let info = app.test_connection(&conn, None).await.unwrap();
    assert_eq!(info.driver, "sqlite");

    // Nothing was persisted and no session was kept.
    assert!(app.connections().is_empty());
    assert!(!app.is_connected(&conn.id));
}

#[tokio::test]
async fn testing_an_unknown_driver_fails_clearly() {
    let dir = TempDir::new().unwrap();
    let app = app_with(&dir).await;
    let conn = Connection::new("nope", "oracle", Profile::Local);
    assert!(matches!(
        app.test_connection(&conn, None).await,
        Err(AppError::UnknownDriver(_))
    ));
}

#[tokio::test]
async fn reconnect_opens_a_fresh_session() {
    let dir = TempDir::new().unwrap();
    let conn = local_sqlite(&dir);
    let id = conn.id.clone();
    let mut app = app_with(&dir).await;
    app.upsert_connection(conn).unwrap();
    app.connect(&id, None).await.unwrap();

    let info = app.reconnect(&id).await.unwrap();
    assert_eq!(info.connection_id, id);
    assert!(app.is_connected(&id));

    // And it is actually usable.
    let mut run = match app.run(&id, "SELECT 1", false).await.unwrap() {
        RunOutcome::Running(run) => run,
        other => panic!("expected to run, got {other:?}"),
    };
    let events = drain(&mut run).await;
    assert!(events
        .iter()
        .any(|e| matches!(e, QueryEvent::Done { row_count: 1, .. })));
}

#[tokio::test]
async fn judge_answers_without_running_anything() {
    let dir = TempDir::new().unwrap();
    let mut conn = local_sqlite(&dir);
    conn.profile = Profile::Staging;
    let id = conn.id.clone();
    let mut app = app_with(&dir).await;
    app.upsert_connection(conn).unwrap();

    // Staging writes need an explicit transaction, so an unconnected (and
    // therefore certainly not in a transaction) session is blocked.
    let verdict = app.judge(&id, "UPDATE t SET x = 1").await.unwrap();
    match verdict {
        spanners_core::Verdict::Block { reasons } => assert!(
            reasons
                .iter()
                .any(|r| matches!(r, spanners_core::Violation::WriteOutsideTransaction)),
            "{reasons:?}"
        ),
        other => panic!("expected Block, got {other:?}"),
    }
    // Judging does not require a live session.
    assert!(!app.is_connected(&id));
}

#[test]
fn tsv_keeps_cells_on_one_line() {
    let cols = vec![spanners_driver_api::Column::new("a", "text")];
    let rows = vec![spanners_driver_api::Row(vec![
        spanners_driver_api::Value::Text("two\nlines\there".into()),
    ])];
    assert_eq!(tsv(&cols, &rows), "a\ntwo lines here\n");
}

#[test]
fn connection_labels_never_include_a_password() {
    let mut c = Connection::new("live", "postgres", Profile::Production);
    c.host = Some("db.example".into());
    c.port = Some(5432);
    c.database = Some("app".into());
    assert_eq!(connection_label(&c), "postgres://db.example:5432/app");
}

#[tokio::test]
async fn cancel_on_an_idle_session_is_not_fatal() {
    let dir = TempDir::new().unwrap();
    let conn = local_sqlite(&dir);
    let id = conn.id.clone();
    let mut app = app_with(&dir).await;
    app.upsert_connection(conn).unwrap();
    app.connect(&id, None).await.unwrap();

    // Nothing is running; the driver may return an error or a no-op. What must
    // not happen is a panic or a broken session.
    let _ = app.cancel(&id, QueryId(1)).await;
    let mut run = match app.run(&id, "SELECT 1", false).await.unwrap() {
        RunOutcome::Running(run) => run,
        other => panic!("expected to run, got {other:?}"),
    };
    drain(&mut run).await;
    let _ = Duration::from_millis(0);
}

#[tokio::test]
async fn export_goes_through_the_policy_gate() {
    let dir = TempDir::new().unwrap();
    let mut conn = local_sqlite(&dir);
    conn.profile = Profile::Production;
    let id = conn.id.clone();
    // A read-only open cannot create the file, which is correct.
    std::fs::File::create(conn.database.as_ref().unwrap()).unwrap();

    let mut app = app_with(&dir).await;
    app.upsert_connection(conn).unwrap();
    app.connect(&id, None).await.unwrap();

    let opts = ExportOpts {
        format: Format::Csv,
        header: true,
        null: NullRepr::Empty,
    };
    let mut buf = spanners_core::export::Buffer::new();
    let err = app
        .export(&id, "CREATE TABLE nope (id int)", &opts, &mut buf)
        .await
        .unwrap_err();
    match err {
        AppError::Blocked { reasons } => assert!(!reasons.is_empty()),
        other => panic!("expected Blocked, got {other:?}"),
    }
    // Refused before it ran, so nothing was exported or recorded.
    assert!(buf.as_str().unwrap().is_empty());
    assert!(app.history(10).unwrap().is_empty());
}

#[tokio::test]
async fn export_asks_before_a_statement_the_profile_only_confirms() {
    let dir = TempDir::new().unwrap();
    let conn = local_sqlite(&dir);
    let id = conn.id.clone();
    let mut app = app_with(&dir).await;
    app.upsert_connection(conn).unwrap();
    app.connect(&id, None).await.unwrap();

    // Create the table while the profile is still Local, then switch: Staging
    // would block the DDL.
    let mut run = match app.run(&id, "CREATE TABLE t (x int)", false).await.unwrap() {
        RunOutcome::Running(run) => run,
        other => panic!("expected to run, got {other:?}"),
    };
    drain(&mut run).await;

    // Give Staging something to confirm: an UPDATE with no WHERE.
    app.upsert_connection({
        let mut c = app.connection(&id).unwrap().clone();
        c.profile = Profile::Staging;
        c
    })
    .unwrap();
    app.disconnect(&id).await.unwrap();
    app.connect(&id, None).await.unwrap();
    // Staging requires an explicit transaction for writes.
    app.begin(&id).await.unwrap();

    let opts = ExportOpts {
        format: Format::Csv,
        header: true,
        null: NullRepr::Empty,
    };
    let mut buf = spanners_core::export::Buffer::new();
    let err = app
        .export(&id, "UPDATE t SET x = 1", &opts, &mut buf)
        .await
        .unwrap_err();
    assert!(matches!(err, AppError::NeedsConfirmation { .. }), "{err:?}");

    // With consent the same statement is allowed through.
    let mut buf = spanners_core::export::Buffer::new();
    let summary = app
        .export_with_exec(
            &id,
            "UPDATE t SET x = 1",
            ExecOpts::default(),
            &opts,
            true,
            &mut buf,
        )
        .await
        .unwrap();
    assert_eq!(summary.rows, 0);
}

#[tokio::test]
async fn an_export_lands_in_history() {
    let dir = TempDir::new().unwrap();
    let conn = local_sqlite(&dir);
    let id = conn.id.clone();
    let mut app = app_with(&dir).await;
    app.upsert_connection(conn).unwrap();
    app.connect(&id, None).await.unwrap();

    app.export_to_string(
        &id,
        "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 3)\n SELECT i FROM n",
        &ExportOpts::default(),
    )
    .await
    .unwrap();

    let history = app.history(10).unwrap();
    assert_eq!(history.len(), 1);
    assert!(history[0].1.sql.contains("WITH RECURSIVE"));
    assert_eq!(history[0].1.row_count, Some(3));
    assert!(history[0].1.error.is_none());
}

#[tokio::test]
async fn the_editor_can_run_the_statement_under_the_cursor() {
    let dir = TempDir::new().unwrap();
    let conn = local_sqlite(&dir);
    let id = conn.id.clone();
    let mut app = app_with(&dir).await;
    app.upsert_connection(conn).unwrap();
    app.connect(&id, None).await.unwrap();

    let buffer = "CREATE TABLE t (id int);\nINSERT INTO t VALUES (7);\nSELECT id FROM t";
    // A cursor inside the SELECT resolves to the SELECT, not the whole buffer.
    let offset = buffer.find("SELECT id").unwrap() + 3;
    let statement = app.statement_at(&id, buffer, offset).unwrap().unwrap();
    assert_eq!(statement.sql, "SELECT id FROM t");

    // Run the first two statements one at a time, the way a user stepping
    // through a script would.
    for needle in ["CREATE TABLE", "INSERT INTO"] {
        let at = buffer.find(needle).unwrap() + 2;
        let mut run = match app.run_statement_at(&id, buffer, at, false).await.unwrap() {
            RunOutcome::Running(run) => run,
            other => panic!("expected to run, got {other:?}"),
        };
        drain(&mut run).await;
    }

    let mut run = match app
        .run_statement_at(&id, buffer, offset, false)
        .await
        .unwrap()
    {
        RunOutcome::Running(run) => run,
        other => panic!("expected to run, got {other:?}"),
    };
    let events = drain(&mut run).await;
    let rows = events
        .iter()
        .map(|e| match e {
            QueryEvent::Batch { rows, .. } => rows.len(),
            _ => 0,
        })
        .sum::<usize>();
    assert_eq!(rows, 1);

    assert_eq!(app.history(10).unwrap().len(), 3);
    assert_eq!(app.statements(&id, buffer).unwrap().len(), 3);
}

#[tokio::test]
async fn running_with_no_statement_under_the_cursor_sends_nothing() {
    let dir = TempDir::new().unwrap();
    let conn = local_sqlite(&dir);
    let id = conn.id.clone();
    let mut app = app_with(&dir).await;
    app.upsert_connection(conn).unwrap();
    app.connect(&id, None).await.unwrap();

    let outcome = app.run_statement_at(&id, "  \n  ", 1, false).await.unwrap();
    assert!(matches!(outcome, RunOutcome::Blocked { .. }));
    assert!(app.history(10).unwrap().is_empty());
}

#[tokio::test]
async fn statement_offsets_are_dialect_aware() {
    let dir = TempDir::new().unwrap();
    let conn = local_sqlite(&dir);
    let id = conn.id.clone();
    let mut app = app_with(&dir).await;
    app.upsert_connection(conn).unwrap();

    // SQLite does not dollar-quote, so the `;` splits; the point is only that
    // the app asks the connection's dialect rather than assuming Postgres.
    let sql = "SELECT 1; SELECT 2";
    assert_eq!(app.statements(&id, sql).unwrap().len(), 2);
}

#[tokio::test]
async fn a_staging_write_is_blocked_outside_a_transaction_and_allowed_inside() {
    let dir = TempDir::new().unwrap();
    let conn = local_sqlite(&dir);
    let id = conn.id.clone();
    let mut app = app_with(&dir).await;
    app.upsert_connection(conn).unwrap();
    app.connect(&id, None).await.unwrap();

    // Create the table while the connection is still Local: Staging would
    // block the DDL.
    let mut run = match app.run(&id, "CREATE TABLE t (x int)", false).await.unwrap() {
        RunOutcome::Running(run) => run,
        other => panic!("expected to run, got {other:?}"),
    };
    drain(&mut run).await;

    app.upsert_connection({
        let mut c = app.connection(&id).unwrap().clone();
        c.profile = Profile::Staging;
        c
    })
    .unwrap();

    // Outside a transaction: blocked, and nothing reached the server.
    match app
        .run(&id, "INSERT INTO t VALUES (1)", false)
        .await
        .unwrap()
    {
        RunOutcome::Blocked { reasons } => assert!(
            reasons
                .iter()
                .any(|r| matches!(r, spanners_core::Violation::WriteOutsideTransaction)),
            "{reasons:?}"
        ),
        other => panic!("expected Blocked, got {other:?}"),
    }
    assert_eq!(row_count(&mut app, &id, "SELECT x FROM t").await, 0);

    // Reads are still allowed outside a transaction.
    assert_eq!(row_count(&mut app, &id, "SELECT x FROM t").await, 0);

    // Inside one, the write runs and the transaction sees it.
    app.begin(&id).await.unwrap();
    let mut run = match app
        .run(&id, "INSERT INTO t VALUES (1)", false)
        .await
        .unwrap()
    {
        RunOutcome::Running(run) => run,
        other => panic!("expected to run, got {other:?}"),
    };
    drain(&mut run).await;
    assert_eq!(row_count(&mut app, &id, "SELECT x FROM t").await, 1);

    // Rolling back discards it, and the gate applies again afterwards.
    app.rollback(&id).await.unwrap();
    assert_eq!(row_count(&mut app, &id, "SELECT x FROM t").await, 0);
    assert!(matches!(
        app.run(&id, "INSERT INTO t VALUES (2)", false)
            .await
            .unwrap(),
        RunOutcome::Blocked { .. }
    ));
}

/// Run a read and return its row count, so a test can assert what a session
/// actually sees.
async fn row_count(app: &mut App, id: &str, sql: &str) -> u64 {
    let mut run = match app.run(id, sql, false).await.unwrap() {
        RunOutcome::Running(run) => run,
        other => panic!("expected to run, got {other:?}"),
    };
    let mut rows = 0;
    while let Some(event) = run.recv().await {
        if let QueryEvent::Batch { rows: batch, .. } = event {
            rows += batch.len() as u64;
        }
    }
    rows
}
