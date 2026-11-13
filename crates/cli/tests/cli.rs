//! Tests that run the real `spanners` binary.
//!
//! A CLI is a promise about stdout, stderr and exit codes, so these exercise
//! the process rather than the helpers. `CARGO_BIN_EXE_spanners` is provided by
//! cargo, so no extra test dependency is needed.

use std::process::{Command, Output};

fn spanners(home: &std::path::Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_spanners"))
        .arg("--home")
        .arg(home)
        .args(args)
        .output()
        .expect("run spanners")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

struct Fixture {
    dir: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    fn home(&self) -> &std::path::Path {
        self.dir.path()
    }

    fn db(&self) -> String {
        self.dir
            .path()
            .join("demo.db")
            .to_string_lossy()
            .to_string()
    }

    /// Add a SQLite connection and return its id.
    fn add(&self, name: &str, profile: &str) -> String {
        let db = self.db();
        let out = spanners(
            self.home(),
            &[
                "connections",
                "add",
                name,
                "--driver",
                "sqlite",
                "--database",
                &db,
                "--profile",
                profile,
            ],
        );
        assert!(out.status.success(), "{}", stderr(&out));
        stdout(&out).trim().to_string()
    }

    fn query(&self, connection: &str, sql: &str) -> Output {
        spanners(
            self.home(),
            &["query", "--connection", connection, "--sql", sql],
        )
    }
}

#[test]
fn creates_a_connection_runs_sql_and_prints_a_table() {
    let f = Fixture::new();
    f.add("demo", "local");

    let create = f.query("demo", "CREATE TABLE t (id INTEGER, name TEXT)");
    assert!(create.status.success(), "{}", stderr(&create));

    let insert = f.query("demo", "INSERT INTO t VALUES (1, 'ada'), (2, NULL)");
    assert!(insert.status.success(), "{}", stderr(&insert));

    let out = f.query("demo", "SELECT * FROM t ORDER BY id");
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("id\tname"), "{text}");
    assert!(text.contains("1\tada"), "{text}");
    assert!(text.contains("2\tNULL"), "{text}");
    assert!(stderr(&out).contains("2 row(s)"), "{}", stderr(&out));
}

#[test]
fn csv_output_marks_null_and_quotes_commas() {
    let f = Fixture::new();
    f.add("demo", "local");
    f.query("demo", "CREATE TABLE t (id INTEGER, name TEXT)");
    f.query("demo", "INSERT INTO t VALUES (1, NULL), (2, 'a,b')");

    let out = spanners(
        f.home(),
        &[
            "query",
            "--connection",
            "demo",
            "--sql",
            "SELECT * FROM t ORDER BY id",
            "--format",
            "csv",
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stdout(&out), "id,name\n1,\\N\n2,\"a,b\"\n");
}

#[test]
fn connections_are_addressable_by_name() {
    let f = Fixture::new();
    f.add("analytics", "local");
    let out = f.query("analytics", "SELECT 1");
    assert!(out.status.success(), "{}", stderr(&out));
}

#[test]
fn an_unknown_connection_fails_with_a_message_not_a_backtrace() {
    let f = Fixture::new();
    let out = f.query("nope", "SELECT 1");
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("no connection matches 'nope'"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn production_refuses_ddl_and_exits_non_zero() {
    let f = Fixture::new();
    // Seed the file with a local connection first: a read-only open cannot
    // create a database, which is correct behaviour.
    f.add("seed", "local");
    f.query("seed", "SELECT 1");
    f.add("live", "production");

    let out = f.query("live", "CREATE TABLE nope (id INTEGER)");
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(err.contains("Blocked by the connection profile"), "{err}");
    assert!(err.contains("schema changes are blocked"), "{err}");
    assert!(stdout(&out).is_empty(), "{}", stdout(&out));
}

#[test]
fn staging_asks_for_confirmation_unless_told_yes() {
    let f = Fixture::new();
    f.add("seed", "local");
    f.query("seed", "CREATE TABLE t (id INTEGER)");
    f.query("seed", "INSERT INTO t VALUES (1)");
    f.add("stage", "staging");

    let refused = f.query("stage", "UPDATE t SET id = 2");
    assert!(!refused.status.success());
    assert!(
        stderr(&refused).contains("needs confirmation"),
        "{}",
        stderr(&refused)
    );

    let allowed = spanners(
        f.home(),
        &[
            "query",
            "--connection",
            "stage",
            "--sql",
            "UPDATE t SET id = 2",
            "--yes",
        ],
    );
    assert!(allowed.status.success(), "{}", stderr(&allowed));
}

#[test]
fn check_classifies_without_running() {
    let f = Fixture::new();
    f.add("stage", "staging");

    let blocked = spanners(
        f.home(),
        &[
            "check",
            "--connection",
            "stage",
            "--sql",
            "CREATE TABLE t (id int)",
        ],
    );
    assert!(!blocked.status.success(), "check should exit 2 for a block");
    assert!(stderr(&blocked).contains("blocked"), "{}", stderr(&blocked));

    let fine = spanners(
        f.home(),
        &["check", "--connection", "stage", "--sql", "SELECT 1"],
    );
    assert_eq!(stdout(&fine).trim(), "allow");
}

#[test]
fn export_writes_a_file_and_history_records_the_query() {
    let f = Fixture::new();
    f.add("demo", "local");
    f.query("demo", "CREATE TABLE t (id INTEGER)");
    f.query("demo", "INSERT INTO t VALUES (1), (2)");

    let path = f.home().join("out.csv");
    let out = spanners(
        f.home(),
        &[
            "export",
            "--connection",
            "demo",
            "--sql",
            "SELECT * FROM t ORDER BY id",
            "--out",
            path.to_str().unwrap(),
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let written = std::fs::read_to_string(&path).unwrap();
    assert_eq!(written, "id\n1\n2\n");

    let history = spanners(f.home(), &["history"]);
    assert!(history.status.success(), "{}", stderr(&history));
    let text = stdout(&history);
    assert!(text.contains("SELECT * FROM t"), "{text}");
}

#[test]
fn filtering_history_by_text_finds_the_query() {
    let f = Fixture::new();
    f.add("demo", "local");
    f.query("demo", "SELECT 1 AS alpha");
    f.query("demo", "SELECT 2 AS beta");

    let out = spanners(f.home(), &["history", "--search", "beta"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert_eq!(text.lines().count(), 1, "{text}");
    assert!(text.contains("beta"), "{text}");
}

#[test]
fn removing_a_connection_removes_it_from_the_list() {
    let f = Fixture::new();
    let id = f.add("demo", "local");
    let out = spanners(f.home(), &["connections", "remove", "--connection", &id]);
    assert!(out.status.success(), "{}", stderr(&out));

    let list = spanners(f.home(), &["connections", "list"]);
    assert!(!stdout(&list).contains(&id), "{}", stdout(&list));
}
