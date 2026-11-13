//! End-to-end application tests against real servers.
//!
//! Gated on `SPANNERS_TEST_POSTGRES` / `SPANNERS_TEST_MYSQL`, same as the
//! driver suites, so the default `cargo test` stays green without Docker.
//!
//! These exist to prove the wiring, not the wire protocol: the driver suites
//! already cover streaming, cancel and caps. Here the question is whether the
//! app turns a `Connection` into a live session with the right policy, records
//! history, and refuses a write before it reaches a production server.

use std::sync::atomic::{AtomicU64, Ordering};

use spanners_app::{App, MemorySecrets, RunOutcome};
use spanners_core::export::{ExportOpts, Format, NullRepr};
use spanners_core::{Connection, Profile};
use spanners_driver_api::QueryEvent;
use tempfile::TempDir;

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique(prefix: &str) -> String {
    format!(
        "{prefix}_{}_{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// `scheme://user:pass@host:port/db` → a `Connection` plus the password a
/// prompt would have supplied. Reuses the known-shaped parser from the driver
/// suites.
fn connection(name: &str, url: &str, profile: Profile) -> (Connection, Option<String>) {
    let (scheme, rest) = url.split_once("://").unwrap_or(("postgres", url));
    let (authority, database) = rest.split_once('/').unwrap_or((rest, ""));
    let (creds, hostport) = match authority.rsplit_once('@') {
        Some((c, h)) => (Some(c), h),
        None => (None, authority),
    };
    let (username, password) = match creds.and_then(|c| c.split_once(':')) {
        Some((u, p)) => (Some(u.to_string()), Some(p.to_string())),
        None => (creds.map(str::to_string), None),
    };
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().ok()),
        None => (hostport.to_string(), None),
    };

    let mut c = Connection::new(name, scheme, profile);
    c.host = Some(host);
    c.port = port;
    c.database = Some(database.to_string());
    c.username = username;
    c.id = unique("conn");
    (c, password)
}

async fn app() -> (TempDir, App) {
    let dir = tempfile::tempdir().unwrap();
    let app = App::open_with_secrets(dir.path(), Box::new(MemorySecrets::new())).unwrap();
    (dir, app)
}

async fn drain(run: &mut spanners_app::Run) -> (usize, Option<String>) {
    let mut rows = 0;
    let mut error = None;
    while let Some(ev) = run.recv().await {
        match ev {
            QueryEvent::Batch { rows: batch, .. } => rows += batch.len(),
            QueryEvent::Failed { message, .. } => error = Some(message),
            _ => {}
        }
    }
    (rows, error)
}

async fn exercise(url: &str, profile: Profile) {
    let (_dir, mut app) = app().await;
    let (conn, password) = connection("live", url, profile);
    let id = conn.id.clone();
    app.upsert_connection(conn).unwrap();

    let info = app.connect(&id, password).await.expect("connect");
    assert!(!info.server_version.is_empty());
    assert_eq!(info.read_only, profile == Profile::Production);

    // A read works and lands in history.
    let mut run = match app.run(&id, "SELECT 1 AS one", false).await.unwrap() {
        RunOutcome::Running(run) => run,
        other => panic!("expected to run, got {other:?}"),
    };
    let (rows, error) = drain(&mut run).await;
    assert_eq!(rows, 1);
    assert!(error.is_none(), "{error:?}");

    let history = app.history(10).unwrap();
    assert_eq!(history.len(), 1, "history: {history:?}");
    assert_eq!(history[0].1.connection_id, id);
    assert_eq!(history[0].1.row_count, Some(1));

    // Production refuses the write before the server sees it; Local runs it.
    let table = unique("app_e2e");
    let create = format!("CREATE TABLE {table} (id int)");
    let outcome = app.run(&id, &create, false).await.unwrap();
    match profile {
        Profile::Production => {
            match outcome {
                RunOutcome::Blocked { reasons } => assert!(!reasons.is_empty()),
                other => panic!("expected Blocked for {profile:?}, got {other:?}"),
            }
            assert_eq!(
                app.history(10).unwrap().len(),
                1,
                "blocked SQL is not history"
            );
        }
        _ => {
            let mut run = match outcome {
                RunOutcome::Running(run) => run,
                other => panic!("expected to run for {profile:?}, got {other:?}"),
            };
            let (_, error) = drain(&mut run).await;
            assert!(error.is_none(), "{error:?}");
            assert_eq!(app.history(10).unwrap().len(), 2);
        }
    }

    // Export goes through the same session and the same caps.
    let (text, summary) = app
        .export_to_string(
            &id,
            "SELECT 1 AS n",
            &ExportOpts {
                format: Format::Csv,
                header: true,
                null: NullRepr::Empty,
            },
        )
        .await
        .unwrap();
    assert_eq!(summary.rows, 1);
    assert_eq!(text, "n\n1\n");

    app.disconnect(&id).await.unwrap();
    assert!(!app.is_connected(&id));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_end_to_end() {
    let Ok(url) = std::env::var("SPANNERS_TEST_POSTGRES") else {
        eprintln!("skipped: SPANNERS_TEST_POSTGRES not set");
        return;
    };
    exercise(&url, Profile::Local).await;
    exercise(&url, Profile::Production).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mysql_end_to_end() {
    let Ok(url) = std::env::var("SPANNERS_TEST_MYSQL") else {
        eprintln!("skipped: SPANNERS_TEST_MYSQL not set");
        return;
    };
    exercise(&url, Profile::Local).await;
    exercise(&url, Profile::Production).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_export_streams_a_real_table() {
    let Ok(url) = std::env::var("SPANNERS_TEST_POSTGRES") else {
        eprintln!("skipped: SPANNERS_TEST_POSTGRES not set");
        return;
    };
    let (_dir, mut app) = app().await;
    let (conn, password) = connection("local", &url, Profile::Local);
    let id = conn.id.clone();
    app.upsert_connection(conn).unwrap();
    app.connect(&id, password).await.unwrap();

    let (text, summary) = app
        .export_to_string(
            &id,
            "SELECT * FROM (VALUES (1, 'ada'), (2, NULL)) AS t(id, name) ORDER BY id",
            &ExportOpts {
                format: Format::Csv,
                header: true,
                null: NullRepr::Marker("\\N".into()),
            },
        )
        .await
        .unwrap();

    assert_eq!(summary.rows, 2);
    assert_eq!(text, "id,name\n1,ada\n2,\\N\n");
}
