use std::time::Duration;

use tempfile::TempDir;
use tokio::sync::mpsc;

use super::*;

fn db_path(dir: &TempDir) -> String {
    dir.path().join("test.db").to_string_lossy().into_owned()
}

async fn open(path: &str, read_only: bool) -> Box<dyn Session> {
    SqliteDriver::new()
        .connect(
            &ConnConfig::sqlite(path),
            SessionOpts {
                read_only,
                timeout: None,
            },
        )
        .await
        .expect("connect")
}

#[derive(Default)]
struct Collected {
    columns: Vec<Column>,
    rows: Vec<Row>,
    done: Option<(u64, u64, bool)>,
    failed: Option<(String, Option<String>)>,
}

async fn run(session: &dyn Session, sql: &str, opts: ExecOpts) -> Collected {
    let (tx, mut rx) = mpsc::channel(8);
    session.execute(sql, opts, tx).await.expect("start query");

    let mut out = Collected::default();
    while let Some(ev) = rx.recv().await {
        match ev {
            QueryEvent::Meta { columns } => out.columns = columns,
            QueryEvent::Batch { rows, .. } => out.rows.extend(rows),
            QueryEvent::Done {
                elapsed_ms,
                row_count,
                truncated,
            } => {
                out.done = Some((elapsed_ms, row_count, truncated));
                break;
            }
            QueryEvent::Failed { message, code, .. } => {
                out.failed = Some((message, code));
                break;
            }
        }
    }
    out
}

async fn seed(path: &str) -> Box<dyn Session> {
    let s = open(path, false).await;
    run(
        s.as_ref(),
        "CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT NOT NULL, score REAL)",
        ExecOpts::default(),
    )
    .await;
    run(
        s.as_ref(),
        "INSERT INTO people VALUES (1,'ada',9.5),(2,'grace',9.75),(3,'alan',8.0)",
        ExecOpts::default(),
    )
    .await;
    s
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn streams_meta_rows_and_done() {
    let dir = TempDir::new().unwrap();
    let path = db_path(&dir);
    let s = seed(&path).await;

    let out = run(
        s.as_ref(),
        "SELECT id, name, score FROM people ORDER BY id",
        ExecOpts::default(),
    )
    .await;

    assert!(out.failed.is_none(), "{:?}", out.failed);
    assert_eq!(out.columns.len(), 3);
    assert_eq!(out.columns[0].name, "id");
    assert!(out.columns[0].is_numeric);
    assert_eq!(out.rows.len(), 3);
    assert_eq!(out.rows[0].0[0], Value::Int(1));
    assert_eq!(out.rows[0].0[1], Value::Text("ada".into()));
    assert_eq!(out.rows[0].0[2], Value::Float(9.5));
    let (_, count, truncated) = out.done.expect("done");
    assert_eq!(count, 3);
    assert!(!truncated);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn row_cap_stops_fetching_and_flags_truncation() {
    let dir = TempDir::new().unwrap();
    let path = db_path(&dir);
    let s = seed(&path).await;

    let out = run(
        s.as_ref(),
        "SELECT id FROM people ORDER BY id",
        ExecOpts {
            max_rows: Some(2),
            ..ExecOpts::default()
        },
    )
    .await;

    assert_eq!(out.rows.len(), 2);
    let (_, count, truncated) = out.done.expect("done");
    assert_eq!(count, 2);
    assert!(truncated, "cap must be reported, not silently applied");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_only_session_rejects_writes() {
    let dir = TempDir::new().unwrap();
    let path = db_path(&dir);
    let _seeded = seed(&path).await;

    let ro = open(&path, true).await;
    let read = run(
        ro.as_ref(),
        "SELECT count(*) FROM people",
        ExecOpts::default(),
    )
    .await;
    assert!(read.failed.is_none());
    assert_eq!(read.rows[0].0[0], Value::Int(3));

    let write = run(
        ro.as_ref(),
        "INSERT INTO people VALUES (4,'edsger',7.0)",
        ExecOpts::default(),
    )
    .await;
    let (message, code) = write.failed.expect("write must fail");
    assert_eq!(code.as_deref(), Some("READ_ONLY"));
    assert!(message.contains("read-only"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_stops_a_runaway_query() {
    let dir = TempDir::new().unwrap();
    let path = db_path(&dir);
    let s = seed(&path).await;

    let (tx, mut rx) = mpsc::channel(8);
    s.execute(
        "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c) SELECT count(*) FROM c",
        ExecOpts::default(),
        tx,
    )
    .await
    .unwrap();

    tokio::time::sleep(Duration::from_millis(100)).await;
    let started = std::time::Instant::now();
    s.cancel(QueryId(1)).await.unwrap();

    let mut failed = None;
    while let Some(ev) = rx.recv().await {
        match ev {
            QueryEvent::Failed { message, code, .. } => {
                failed = Some((message, code));
                break;
            }
            QueryEvent::Done { .. } => panic!("runaway query finished instead of cancelling"),
            _ => {}
        }
    }

    let (_, code) = failed.expect("cancel must produce a failure event");
    assert_eq!(code.as_deref(), Some("CANCELLED"));
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "cancel took {:?}",
        started.elapsed()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timeout_interrupts_and_reports_failure() {
    let dir = TempDir::new().unwrap();
    let path = db_path(&dir);
    let s = seed(&path).await;

    let out = run(
        s.as_ref(),
        "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c) SELECT count(*) FROM c",
        ExecOpts {
            timeout: Some(Duration::from_millis(150)),
            ..ExecOpts::default()
        },
    )
    .await;

    assert!(out.failed.is_some(), "timeout must fail the query");
    assert!(out.done.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rollback_discards_writes() {
    let dir = TempDir::new().unwrap();
    let path = db_path(&dir);
    let s = seed(&path).await;

    run(s.as_ref(), "BEGIN", ExecOpts::default()).await;
    assert!(s.in_transaction().await);
    run(
        s.as_ref(),
        "INSERT INTO people VALUES (99,'temp',1.0)",
        ExecOpts::default(),
    )
    .await;
    s.rollback().await.unwrap();
    assert!(!s.in_transaction().await);

    let out = run(
        s.as_ref(),
        "SELECT count(*) FROM people",
        ExecOpts::default(),
    )
    .await;
    assert_eq!(out.rows[0].0[0], Value::Int(3));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn schema_lists_tables_and_columns() {
    let dir = TempDir::new().unwrap();
    let path = db_path(&dir);
    let s = seed(&path).await;

    let snap = s.schema().await.unwrap();
    let main = &snap.schemas[0];
    let people = main
        .tables
        .iter()
        .find(|t| t.name == "people")
        .expect("people");
    assert_eq!(people.kind, TableKind::Table);
    let names: Vec<&str> = people.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["id", "name", "score"]);
    assert!(
        people.columns[0].nullable,
        "INTEGER PRIMARY KEY is nullable in SQLite"
    );
    assert!(!people.columns[1].nullable);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explain_returns_a_plan() {
    let dir = TempDir::new().unwrap();
    let path = db_path(&dir);
    let s = seed(&path).await;

    let plan = s
        .explain("SELECT * FROM people WHERE id = 1")
        .await
        .unwrap();
    assert!(!plan.raw.is_empty());
}
