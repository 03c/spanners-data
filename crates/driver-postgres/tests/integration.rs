//! Postgres integration tests.
//!
//! Gated on `SPANNERS_TEST_POSTGRES` (a `postgres://user:pass@host:port/db`
//! URL). Without it every test prints `skipped` and passes, so `cargo test`
//! stays green on machines and in jobs without a database.
//!
//! CI sets it from a service container; locally:
//!   docker run -d --rm -e POSTGRES_PASSWORD=pass -p 55432:5432 postgres:16-alpine

use std::time::{Duration, Instant};

use spanners_driver_api::{
    codes, ConnConfig, Driver, ExecOpts, QueryEvent, SchemaSnapshot, Session, SessionOpts, Value,
};
use spanners_driver_postgres::PostgresDriver;
use tokio::sync::mpsc;

fn url() -> Option<String> {
    std::env::var("SPANNERS_TEST_POSTGRES").ok()
}

/// Minimal `postgres://user:pass@host:port/db` parser. Deliberately not a
/// general URL parser: the test inputs are known-shaped.
fn parse(url: &str) -> ConnConfig {
    let rest = url
        .strip_prefix("postgres://")
        .or_else(|| url.strip_prefix("postgresql://"))
        .unwrap_or(url);
    let (authority, database) = match rest.split_once('/') {
        Some((a, d)) => (a, Some(d.to_string())),
        None => (rest, None),
    };
    let (creds, hostport) = match authority.rsplit_once('@') {
        Some((c, h)) => (Some(c), h),
        None => (None, authority),
    };
    let (username, password) = match creds.and_then(|c| c.split_once(':')) {
        Some((u, p)) => (Some(u.to_string()), Some(p.to_string())),
        None => (creds.map(str::to_string), None),
    };
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (Some(h.to_string()), p.parse().ok()),
        None => (Some(hostport.to_string()), None),
    };
    ConnConfig {
        driver: "postgres".into(),
        host,
        port,
        database,
        username,
        password,
        params: Vec::new(),
    }
}

async fn session(read_only: bool) -> Option<Box<dyn Session>> {
    let url = match url() {
        Some(u) => u,
        None => {
            eprintln!("skipped: SPANNERS_TEST_POSTGRES not set");
            return None;
        }
    };
    Some(
        PostgresDriver::new()
            .connect(
                &parse(&url),
                SessionOpts {
                    read_only,
                    timeout: None,
                },
            )
            .await
            .expect("connect"),
    )
}

#[derive(Default)]
struct Collected {
    columns: Vec<spanners_driver_api::Column>,
    rows: Vec<spanners_driver_api::Row>,
    done: Option<(u64, u64, bool)>,
    failed: Option<(String, Option<String>)>,
}

async fn run(s: &dyn Session, sql: &str, opts: ExecOpts) -> Collected {
    let (tx, mut rx) = mpsc::channel(8);
    s.execute(sql, opts, tx).await.expect("start query");
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

fn unique(prefix: &str) -> String {
    format!("{prefix}_{}", std::process::id())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn streams_typed_rows() {
    let Some(s) = session(false).await else {
        return;
    };
    let table = unique("spanners_types");
    run(
        &s,
        &format!("DROP TABLE IF EXISTS {table}"),
        ExecOpts::default(),
    )
    .await;
    run(
        &s,
        &format!(
            "CREATE TABLE {table} (
                id int, flag bool, name text, amount numeric(10,2),
                ts timestamptz, d date, t time, data bytea, doc jsonb
            )"
        ),
        ExecOpts::default(),
    )
    .await;
    run(
        &s,
        &format!(
            "INSERT INTO {table} VALUES
             (1, true, 'ada', 12.34, '2024-01-02 03:04:05+00', '2024-01-02', '03:04:05', '\\xdead', '{{\"a\":1}}')"
        ),
        ExecOpts::default(),
    )
    .await;

    let out = run(&s, &format!("SELECT * FROM {table}"), ExecOpts::default()).await;
    assert!(out.failed.is_none(), "{:?}", out.failed);
    assert_eq!(out.columns.len(), 9);
    assert_eq!(out.rows.len(), 1);

    let row = &out.rows[0].0;
    assert_eq!(row[0], Value::Int(1));
    assert_eq!(row[1], Value::Bool(true));
    assert_eq!(row[2], Value::Text("ada".into()));
    // Exact scale is the server's business; the value must survive.
    match &row[3] {
        Value::Decimal(d) => assert_eq!(d.parse::<f64>().unwrap(), 12.34),
        other => panic!("expected decimal, got {other:?}"),
    }
    // 2024-01-02T03:04:05Z
    assert_eq!(row[4], Value::Timestamp(1_704_164_645_000_000));
    assert_eq!(row[5], Value::Date(19_724));
    assert!(matches!(row[6], Value::Time(_)));
    assert_eq!(row[7], Value::Bytes(vec![0xde, 0xad]));
    assert!(matches!(row[8], Value::Json(_)));

    run(&s, &format!("DROP TABLE {table}"), ExecOpts::default()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn row_cap_stops_fetching() {
    let Some(s) = session(false).await else {
        return;
    };
    let out = run(
        &s,
        "SELECT generate_series(1, 10000) AS n",
        ExecOpts {
            max_rows: Some(10),
            ..ExecOpts::default()
        },
    )
    .await;
    assert_eq!(out.rows.len(), 10);
    let (_, count, truncated) = out.done.expect("done");
    assert_eq!(count, 10);
    assert!(truncated);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_capped_query_leaves_the_connection_usable() {
    let Some(s) = session(false).await else {
        return;
    };
    let first = run(
        &s,
        // Big enough that the server is still sending when we stop.
        "SELECT generate_series(1, 200000)",
        ExecOpts {
            max_rows: Some(5),
            ..ExecOpts::default()
        },
    )
    .await;
    assert!(first.done.is_some(), "{:?}", first.failed);

    let second = run(&s, "SELECT 42", ExecOpts::default()).await;
    assert!(second.failed.is_none(), "{:?}", second.failed);
    assert_eq!(second.rows[0].0[0], Value::Int(42));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn row_cap_stops_the_transfer_not_just_the_display() {
    let Some(s) = session(false).await else {
        return;
    };
    // ~200 MB if it were actually read. Capping must cost about one fetch.
    let started = Instant::now();
    let out = run(
        &s,
        "SELECT repeat('x', 1000) FROM generate_series(1, 200000)",
        ExecOpts {
            max_rows: Some(5),
            ..ExecOpts::default()
        },
    )
    .await;
    assert!(out.done.is_some(), "{:?}", out.failed);
    assert_eq!(out.rows.len(), 5);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "row cap took {:?}; it is draining the whole result set",
        started.elapsed()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_reaches_the_server() {
    let Some(s) = session(false).await else {
        return;
    };

    let (tx, mut rx) = mpsc::channel(8);
    s.execute("SELECT pg_sleep(30)", ExecOpts::default(), tx)
        .await
        .unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;
    let started = Instant::now();
    s.cancel(spanners_driver_api::QueryId(1)).await.unwrap();

    let mut code = None;
    while let Some(ev) = rx.recv().await {
        match ev {
            QueryEvent::Failed { code: c, .. } => {
                code = c;
                break;
            }
            QueryEvent::Done { .. } => panic!("pg_sleep finished instead of being cancelled"),
            _ => {}
        }
    }

    assert_eq!(code.as_deref(), Some("CANCELLED"));
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_per_statement_timeout_does_not_leak_into_the_next_statement() {
    let Some(s) = session(false).await else {
        return;
    };

    let timed = run(
        &s,
        "SELECT pg_sleep(5)",
        ExecOpts {
            timeout: Some(Duration::from_millis(300)),
            ..ExecOpts::default()
        },
    )
    .await;
    assert_eq!(
        timed.failed.map(|(_, code)| code),
        Some(Some(codes::TIMEOUT.to_string()))
    );

    // The session default is no deadline. If the 300 ms override leaked, this
    // second statement would be killed early.
    let started = Instant::now();
    let after = run(&s, "SELECT pg_sleep(1)", ExecOpts::default()).await;
    assert!(after.failed.is_none(), "{:?}", after.failed);
    assert!(
        started.elapsed() >= Duration::from_millis(900),
        "second statement did not sleep: {:?}",
        started.elapsed()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_only_session_rejects_ddl() {
    let Some(s) = session(true).await else { return };
    let out = run(&s, "CREATE TEMP TABLE nope (id int)", ExecOpts::default()).await;
    assert!(
        out.failed.is_some(),
        "read-only session must reject CREATE, got {:?}",
        out.done
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn schema_and_explain() {
    let Some(s) = session(false).await else {
        return;
    };
    let table = unique("spanners_schema");
    run(
        &s,
        &format!("DROP TABLE IF EXISTS {table}"),
        ExecOpts::default(),
    )
    .await;
    run(
        &s,
        &format!("CREATE TABLE {table} (id int PRIMARY KEY, label text NOT NULL)"),
        ExecOpts::default(),
    )
    .await;

    let snap: SchemaSnapshot = s.schema().await.unwrap();
    let found = snap
        .schemas
        .iter()
        .flat_map(|sc| sc.tables.iter())
        .find(|t| t.name == table)
        .expect("table in schema");
    let cols: Vec<&str> = found.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(cols, ["id", "label"]);
    assert!(!found.columns[1].nullable);

    let plan = s.explain(&format!("SELECT * FROM {table}")).await.unwrap();
    assert!(plan.json.is_some());
    assert!(plan.raw.contains("Plan"));

    run(&s, &format!("DROP TABLE {table}"), ExecOpts::default()).await;
}
