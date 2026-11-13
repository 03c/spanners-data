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
fn staging_requires_a_transaction_for_writes() {
    let f = Fixture::new();
    f.add("seed", "local");
    f.query("seed", "CREATE TABLE t (id INTEGER)");
    f.query("seed", "INSERT INTO t VALUES (1)");
    f.add("stage", "staging");

    // A write outside a transaction is refused before it reaches the server,
    // even though it names its rows and the user said yes.
    let refused = spanners(
        f.home(),
        &[
            "query",
            "--connection",
            "stage",
            "--sql",
            "UPDATE t SET id = 2 WHERE id = 1",
            "--yes",
        ],
    );
    assert!(!refused.status.success());
    assert!(
        stderr(&refused).contains("explicit transaction"),
        "{}",
        stderr(&refused)
    );

    // Inside one, the same targeted write runs.
    let allowed = spanners(
        f.home(),
        &[
            "query",
            "--connection",
            "stage",
            "--sql",
            "UPDATE t SET id = 2 WHERE id = 1",
            "--transaction",
        ],
    );
    assert!(allowed.status.success(), "{}", stderr(&allowed));
}

#[test]
fn staging_asks_for_confirmation_unless_told_yes() {
    let f = Fixture::new();
    f.add("seed", "local");
    f.query("seed", "CREATE TABLE t (id INTEGER)");
    f.query("seed", "INSERT INTO t VALUES (1)");
    f.add("stage", "staging");

    // A write with no WHERE still needs consent, even inside a transaction.
    let refused = spanners(
        f.home(),
        &[
            "query",
            "--connection",
            "stage",
            "--sql",
            "UPDATE t SET id = 2",
            "--transaction",
        ],
    );
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
            "--transaction",
            "--yes",
        ],
    );
    assert!(allowed.status.success(), "{}", stderr(&allowed));
}

#[test]
fn check_classifies_without_running() {
    let f = Fixture::new();
    f.add("stage", "staging");
    f.add("live", "production");

    // Staging asks before a schema change rather than refusing it.
    let asks = spanners(
        f.home(),
        &[
            "check",
            "--connection",
            "stage",
            "--sql",
            "CREATE TABLE t (id int)",
        ],
    );
    assert!(asks.status.success(), "{}", stderr(&asks));
    assert!(
        stderr(&asks).contains("needs confirmation"),
        "{}",
        stderr(&asks)
    );

    // Production refuses it, and `check` exits 2 so a script can branch.
    let blocked = spanners(
        f.home(),
        &[
            "check",
            "--connection",
            "live",
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

#[test]
fn statement_selection_runs_only_the_named_statement() {
    let f = Fixture::new();
    f.add("demo", "local");

    // The first statement creates the table; the second is the one we ask for.
    let out = spanners(
        f.home(),
        &[
            "query",
            "--connection",
            "demo",
            "--sql",
            "CREATE TABLE t (id INTEGER); INSERT INTO t VALUES (1);",
            "--statement",
            "1",
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));

    let insert = spanners(
        f.home(),
        &[
            "query",
            "--connection",
            "demo",
            "--sql",
            "CREATE TABLE t (id INTEGER); INSERT INTO t VALUES (1);",
            "--statement",
            "2",
        ],
    );
    // Statement 2 is the INSERT; the CREATE in the same buffer is not re-run.
    assert!(insert.status.success(), "{}", stderr(&insert));

    let select = spanners(
        f.home(),
        &[
            "query",
            "--connection",
            "demo",
            "--sql",
            "SELECT id FROM t",
            "--statement",
            "1",
        ],
    );
    assert!(select.status.success(), "{}", stderr(&select));
    assert!(stdout(&select).contains("1"), "{}", stdout(&select));
}

#[test]
fn asking_for_a_statement_that_does_not_exist_is_a_clear_error() {
    let f = Fixture::new();
    f.add("demo", "local");
    let out = spanners(
        f.home(),
        &[
            "query",
            "--connection",
            "demo",
            "--sql",
            "SELECT 1;",
            "--statement",
            "5",
        ],
    );
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("statement 5 does not exist"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn export_refuses_a_statement_the_profile_blocks() {
    let f = Fixture::new();
    f.add("seed", "local");
    f.query("seed", "SELECT 1");
    f.add("live", "production");

    let path = f.home().join("out.csv");
    let out = spanners(
        f.home(),
        &[
            "export",
            "--connection",
            "live",
            "--sql",
            "CREATE TABLE nope (id int)",
            "--out",
            path.to_str().unwrap(),
        ],
    );
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("blocked by the connection profile"),
        "{}",
        stderr(&out)
    );
    assert!(!path.exists(), "a blocked export must not create the file");
}

#[test]
fn export_can_confirm_a_statement_with_yes() {
    let f = Fixture::new();
    f.add("seed", "local");
    f.query("seed", "CREATE TABLE t (id INTEGER)");
    f.query("seed", "INSERT INTO t VALUES (1), (2)");
    f.add("stage", "staging");

    let path = f.home().join("out.csv");
    let refused = spanners(
        f.home(),
        &[
            "export",
            "--connection",
            "stage",
            "--sql",
            "UPDATE t SET id = id",
            "--out",
            path.to_str().unwrap(),
            "--transaction",
        ],
    );
    assert!(!refused.status.success());
    assert!(
        stderr(&refused).contains("needs confirmation"),
        "{}",
        stderr(&refused)
    );

    let allowed = spanners(
        f.home(),
        &[
            "export",
            "--connection",
            "stage",
            "--sql",
            "UPDATE t SET id = id",
            "--out",
            path.to_str().unwrap(),
            "--transaction",
            "--yes",
        ],
    );
    assert!(allowed.status.success(), "{}", stderr(&allowed));
}

#[test]
fn a_connection_can_be_added_from_a_url() {
    let f = Fixture::new();
    let url = format!("sqlite:{}", f.db());
    let out = spanners(f.home(), &["connections", "add", "pasted", "--url", &url]);
    assert!(out.status.success(), "{}", stderr(&out));
    let id = stdout(&out).trim().to_string();

    let list = spanners(f.home(), &["connections", "list"]);
    assert!(stdout(&list).contains(&id), "{}", stdout(&list));

    // And the pasted connection is usable.
    let q = f.query("pasted", "SELECT 1");
    assert!(q.status.success(), "{}", stderr(&q));
}

#[test]
fn a_bad_url_is_refused_and_saves_nothing() {
    let f = Fixture::new();
    let out = spanners(
        f.home(),
        &["connections", "add", "bad", "--url", "oracle://host/db"],
    );
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("unsupported scheme"),
        "{}",
        stderr(&out)
    );

    let list = spanners(f.home(), &["connections", "list"]);
    assert!(!stdout(&list).contains("bad"), "{}", stdout(&list));
}

#[test]
fn adding_without_a_driver_or_url_explains_the_choice() {
    let f = Fixture::new();
    let out = spanners(f.home(), &["connections", "add", "x", "--host", "h"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("pass --driver"), "{}", stderr(&out));
}

#[test]
fn schema_lists_tables_and_columns() {
    let f = Fixture::new();
    f.add("demo", "local");
    f.query(
        "demo",
        "CREATE TABLE people (id INTEGER, name TEXT NOT NULL)",
    );
    f.query("demo", "CREATE VIEW names AS SELECT name FROM people");

    let out = spanners(f.home(), &["schema", "--connection", "demo"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("people\ttable"), "{text}");
    assert!(text.contains("names\tview"), "{text}");

    let columns = spanners(
        f.home(),
        &[
            "schema",
            "--connection",
            "demo",
            "--columns",
            "--table",
            "people",
        ],
    );
    assert!(columns.status.success(), "{}", stderr(&columns));
    let text = stdout(&columns);
    assert!(text.contains("id\tINTEGER"), "{text}");
    assert!(text.contains("name\tTEXT\tnot null"), "{text}");
}

#[test]
fn schema_can_filter_to_one_table() {
    let f = Fixture::new();
    f.add("demo", "local");
    f.query("demo", "CREATE TABLE people (id INTEGER)");
    f.query("demo", "CREATE TABLE places (id INTEGER)");

    let out = spanners(
        f.home(),
        &["schema", "--connection", "demo", "--table", "peop"],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("people"), "{text}");
    assert!(!text.contains("places"), "{text}");
}

#[test]
fn explain_shows_a_plan_without_running_the_query() {
    let f = Fixture::new();
    f.add("demo", "local");
    f.query("demo", "CREATE TABLE people (id INTEGER)");

    let out = spanners(
        f.home(),
        &[
            "explain",
            "--connection",
            "demo",
            "--sql",
            "SELECT * FROM people",
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stdout(&out).to_ascii_uppercase().contains("SCAN"),
        "{}",
        stdout(&out)
    );
}

#[test]
fn production_writes_need_the_name_typed() {
    let f = Fixture::new();
    f.add("seed", "local");
    f.query("seed", "CREATE TABLE t (x INTEGER)");
    f.add("live", "production");

    // The production connection points at the same file; `add` uses the same
    // database path for every local fixture.
    let refused = f.query("live", "INSERT INTO t VALUES (1)");
    assert!(!refused.status.success());
    assert!(
        stderr(&refused).contains("read-only"),
        "{}",
        stderr(&refused)
    );

    let wrong = spanners(
        f.home(),
        &[
            "query",
            "--connection",
            "live",
            "--sql",
            "INSERT INTO t VALUES (1)",
            "--unlock-writes",
            "Live",
        ],
    );
    assert!(!wrong.status.success());
    assert!(
        stderr(&wrong).contains("does not match the connection name"),
        "{}",
        stderr(&wrong)
    );

    let allowed = spanners(
        f.home(),
        &[
            "query",
            "--connection",
            "live",
            "--sql",
            "INSERT INTO t VALUES (1)",
            "--unlock-writes",
            "live",
        ],
    );
    assert!(allowed.status.success(), "{}", stderr(&allowed));

    let count = f.query("seed", "SELECT count(*) FROM t");
    assert!(stdout(&count).contains('1'), "{}", stdout(&count));
}

#[test]
fn a_connection_can_be_tightened_to_production() {
    let f = Fixture::new();
    f.add("demo", "local");
    f.query("demo", "CREATE TABLE t (x INTEGER)");

    let out = spanners(
        f.home(),
        &[
            "connections",
            "set-profile",
            "--connection",
            "demo",
            "production",
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));

    // The profile is visible without asking for it: it is the thing that
    // decides whether a write is refused.
    let list = spanners(f.home(), &["connections", "list"]);
    assert!(
        stdout(&list).contains("demo\tsqlite\tproduction"),
        "{}",
        stdout(&list)
    );

    // Reads still work, writes are refused at the gate.
    let read = f.query("demo", "SELECT count(*) FROM t");
    assert!(read.status.success(), "{}", stderr(&read));

    let write = f.query("demo", "INSERT INTO t VALUES (1)");
    assert!(!write.status.success());
    assert!(stderr(&write).contains("read-only"), "{}", stderr(&write));
}

#[test]
fn a_short_timeout_is_honoured() {
    let f = Fixture::new();
    f.add("demo", "local");

    // A recursive CTE long enough to outlast a 50 ms deadline.
    let out = spanners(
        f.home(),
        &[
            "query",
            "--connection",
            "demo",
            "--sql",
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 200000000) SELECT count(*) FROM n",
            "--timeout",
            "1",
        ],
    );
    // The deadline must interrupt the statement rather than let it finish.
    assert!(!out.status.success(), "{}", stdout(&out));
    assert!(
        stderr(&out).to_ascii_lowercase().contains("time")
            || stderr(&out).to_ascii_lowercase().contains("interrupt"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn a_buffer_runs_statement_by_statement() {
    let f = Fixture::new();
    f.add("demo", "local");

    let out = spanners(
        f.home(),
        &[
            "query",
            "--connection",
            "demo",
            "--sql",
            "CREATE TABLE t (x INTEGER); INSERT INTO t VALUES (1), (2); SELECT count(*) AS n FROM t",
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    // The last statement's result is printed, and the earlier two ran.
    assert!(stdout(&out).contains("n\n-\n2"), "{}", stdout(&out));
    assert!(stderr(&out).contains("statement 3/3"), "{}", stderr(&out));
}

#[test]
fn a_failing_statement_stops_the_buffer_and_says_which_one() {
    let f = Fixture::new();
    f.add("demo", "local");
    f.query("demo", "CREATE TABLE t (x INTEGER)");

    let out = spanners(
        f.home(),
        &[
            "query",
            "--connection",
            "demo",
            "--sql",
            "INSERT INTO t VALUES (1); INSERT INTO missing VALUES (2); INSERT INTO t VALUES (3)",
        ],
    );
    assert!(!out.status.success());
    assert!(stderr(&out).contains("statement 2:"), "{}", stderr(&out));

    // Statement 3 never ran.
    let count = f.query("demo", "SELECT count(*) FROM t");
    assert!(stdout(&count).contains('1'), "{}", stdout(&count));
}

#[test]
fn a_failing_script_rolls_back_inside_a_transaction() {
    let f = Fixture::new();
    f.add("demo", "local");
    f.query("demo", "CREATE TABLE t (x INTEGER)");

    let out = spanners(
        f.home(),
        &[
            "query",
            "--connection",
            "demo",
            "--sql",
            "INSERT INTO t VALUES (1); INSERT INTO missing VALUES (2)",
            "--transaction",
        ],
    );
    assert!(!out.status.success());

    let count = f.query("demo", "SELECT count(*) FROM t");
    assert!(stdout(&count).contains('0'), "{}", stdout(&count));
}

#[test]
fn csv_output_refuses_a_multi_statement_buffer() {
    let f = Fixture::new();
    f.add("demo", "local");
    let out = spanners(
        f.home(),
        &[
            "query",
            "--connection",
            "demo",
            "--sql",
            "SELECT 1; SELECT 2",
            "--format",
            "csv",
        ],
    );
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("takes one statement"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn a_nameless_or_duplicate_connection_is_refused() {
    let f = Fixture::new();
    let db = f.db();

    let nameless = spanners(
        f.home(),
        &[
            "connections",
            "add",
            "",
            "--driver",
            "sqlite",
            "--database",
            &db,
        ],
    );
    assert!(!nameless.status.success());
    assert!(
        stderr(&nameless).contains("needs a name"),
        "{}",
        stderr(&nameless)
    );

    f.add("demo", "local");
    let duplicate = spanners(
        f.home(),
        &[
            "connections",
            "add",
            "Demo",
            "--driver",
            "sqlite",
            "--database",
            &db,
        ],
    );
    assert!(!duplicate.status.success());
    assert!(
        stderr(&duplicate).contains("already a connection"),
        "{}",
        stderr(&duplicate)
    );

    // Exactly one connection survived.
    let list = spanners(f.home(), &["connections", "list"]);
    assert_eq!(stdout(&list).lines().count(), 1, "{}", stdout(&list));
}

#[test]
fn staging_asks_before_a_schema_change() {
    let f = Fixture::new();
    f.add("stage", "staging");

    let refused = f.query("stage", "CREATE TABLE t (x INTEGER)");
    assert!(!refused.status.success());
    assert!(
        stderr(&refused).contains("need confirmation"),
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
            "CREATE TABLE t (x INTEGER)",
            "--yes",
        ],
    );
    assert!(allowed.status.success(), "{}", stderr(&allowed));
}

#[test]
fn an_unknown_driver_is_refused_before_it_is_saved() {
    let f = Fixture::new();
    let out = spanners(
        f.home(),
        &[
            "connections",
            "add",
            "x",
            "--driver",
            "oracle",
            "--database",
            "y",
        ],
    );
    assert!(!out.status.success());
    assert!(stderr(&out).contains("not registered"), "{}", stderr(&out));

    let list = spanners(f.home(), &["connections", "list"]);
    assert!(
        stdout(&list).contains("No connections saved"),
        "{}",
        stdout(&list)
    );
}

#[test]
fn saved_queries_can_be_added_listed_shown_and_run() {
    let f = Fixture::new();
    f.add("demo", "local");

    let added = spanners(
        f.home(),
        &[
            "saved",
            "add",
            "row count",
            "--sql",
            "SELECT 7 AS n",
            "--folder",
            "reports",
            "--connection",
            "demo",
        ],
    );
    assert!(added.status.success(), "{}", stderr(&added));
    let id = stdout(&added).trim().to_string();

    let list = spanners(f.home(), &["saved", "list"]);
    assert!(list.status.success(), "{}", stderr(&list));
    let text = stdout(&list);
    assert!(text.contains("reports\trow count"), "{text}");
    assert!(text.starts_with(&id), "{text}");

    let show = spanners(f.home(), &["saved", "show", &id]);
    assert_eq!(stdout(&show).trim(), "SELECT 7 AS n");

    // By id and by name, through the same engine as `query`.
    for key in [id.as_str(), "row count"] {
        let run = spanners(f.home(), &["saved", "run", key, "-c", "demo"]);
        assert!(run.status.success(), "{}", stderr(&run));
        assert!(stdout(&run).contains('7'), "{}", stdout(&run));
    }

    let removed = spanners(f.home(), &["saved", "rm", &id]);
    assert!(removed.status.success(), "{}", stderr(&removed));
    let list = spanners(f.home(), &["saved", "list"]);
    assert!(
        stdout(&list).contains("No saved queries"),
        "{}",
        stdout(&list)
    );
}

#[test]
fn an_ambiguous_saved_query_name_asks_for_the_id() {
    let f = Fixture::new();
    for folder in ["one", "two"] {
        let out = spanners(
            f.home(),
            &[
                "saved", "add", "report", "--sql", "SELECT 1", "--folder", folder,
            ],
        );
        assert!(out.status.success(), "{}", stderr(&out));
    }
    let out = spanners(f.home(), &["saved", "show", "report"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("matches 2 saved queries"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn check_judges_each_statement_in_a_buffer() {
    let f = Fixture::new();
    f.add("live", "production");

    // Both reads are fine: production's "one statement per buffer" rule is
    // about a single submission, and the CLI submits them one at a time.
    let reads = spanners(
        f.home(),
        &["check", "-c", "live", "--sql", "SELECT 1; SELECT 2"],
    );
    assert!(reads.status.success(), "{}", stderr(&reads));
    assert_eq!(stdout(&reads).lines().count(), 2, "{}", stdout(&reads));

    // The chained DDL is caught, and named.
    let mixed = spanners(
        f.home(),
        &["check", "-c", "live", "--sql", "SELECT 1; DROP TABLE t"],
    );
    assert!(!mixed.status.success());
    assert!(stderr(&mixed).contains("2/2 blocked"), "{}", stderr(&mixed));
    assert!(stdout(&mixed).contains("1/2 allow"), "{}", stdout(&mixed));
}

#[test]
fn a_fresh_home_directory_is_created() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("does").join("not").join("exist");
    let out = spanners(&home, &["connections", "list"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stdout(&out).contains("No connections saved"),
        "{}",
        stdout(&out)
    );
    assert!(home.join("spanners.db").exists());
}

#[test]
fn a_corrupt_history_file_says_which_file_and_what_to_do() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("spanners.db"), b"not sqlite").unwrap();
    let out = spanners(dir.path(), &["connections", "list"]);
    assert!(!out.status.success());
    let message = stderr(&out);
    assert!(message.contains("spanners.db"), "{message}");
    assert!(message.contains("move it aside"), "{message}");
}

const DBEAVER_SAMPLE: &str = r#"{
  "connections": {
    "pg": {
      "provider": "postgresql",
      "name": "app dev",
      "configuration": {
        "host": "localhost", "port": 5432, "database": "app", "user": "me",
        "url": "jdbc:postgresql://localhost:5432/app"
      }
    },
    "my": {
      "provider": "mysql",
      "name": "shop",
      "configuration": {
        "host": "db.example.com", "port": 3306, "database": "shop",
        "url": "jdbc:mysql://db.example.com:3306/shop?useSSL=false"
      }
    },
    "lite": {
      "provider": "sqlite",
      "name": "notes",
      "configuration": { "url": "jdbc:sqlite:/tmp/notes.db" }
    },
    "ora": { "provider": "oracle", "name": "legacy", "configuration": {} }
  }
}"#;

#[test]
fn dbeaver_connections_are_imported_with_a_safe_profile_guess() {
    let f = Fixture::new();
    let file = f.home().join("data-sources.json");
    std::fs::write(&file, DBEAVER_SAMPLE).unwrap();
    let path = file.to_str().unwrap();

    // A dry run reports what it would do and saves nothing.
    let dry = spanners(
        f.home(),
        &["connections", "import", "--path", path, "--dry-run"],
    );
    assert!(dry.status.success(), "{}", stderr(&dry));
    let text = stdout(&dry);
    assert!(text.contains("app dev\tpostgres\tlocal"), "{text}");
    assert!(text.contains("shop\tmysql\tstaging"), "{text}");
    assert!(text.contains("notes\tsqlite\tlocal"), "{text}");
    assert!(stderr(&dry).contains("skipped legacy"), "{}", stderr(&dry));
    let list = spanners(f.home(), &["connections", "list"]);
    assert!(
        stdout(&list).contains("No connections saved"),
        "{}",
        stdout(&list)
    );

    let real = spanners(f.home(), &["connections", "import", "--path", path]);
    assert!(real.status.success(), "{}", stderr(&real));
    let list = spanners(f.home(), &["connections", "list"]);
    assert_eq!(stdout(&list).lines().count(), 3, "{}", stdout(&list));

    // Importing twice does not duplicate anything.
    let again = spanners(f.home(), &["connections", "import", "--path", path]);
    assert!(again.status.success(), "{}", stderr(&again));
    assert!(
        stderr(&again).contains("already a connection"),
        "{}",
        stderr(&again)
    );
    let list = spanners(f.home(), &["connections", "list"]);
    assert_eq!(stdout(&list).lines().count(), 3, "{}", stdout(&list));
}

#[test]
fn import_can_force_a_profile() {
    let f = Fixture::new();
    let file = f.home().join("data-sources.json");
    std::fs::write(&file, DBEAVER_SAMPLE).unwrap();

    let out = spanners(
        f.home(),
        &[
            "connections",
            "import",
            "--path",
            file.to_str().unwrap(),
            "--profile",
            "production",
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stdout(&out).contains("app dev\tpostgres\tproduction"),
        "{}",
        stdout(&out)
    );

    // The imported connection really is read-only: a write is refused.
    let write = spanners(
        f.home(),
        &[
            "query",
            "--connection",
            "notes",
            "--sql",
            "CREATE TABLE t (x int)",
        ],
    );
    assert!(!write.status.success());
}

#[test]
fn importing_a_file_without_connections_is_an_error() {
    let f = Fixture::new();
    let file = f.home().join("data-sources.json");
    std::fs::write(&file, r#"{"folders":{}}"#).unwrap();
    let out = spanners(
        f.home(),
        &["connections", "import", "--path", file.to_str().unwrap()],
    );
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("no 'connections' object"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn check_and_explain_read_sql_from_stdin() {
    let f = Fixture::new();
    f.add("demo", "local");
    f.query("demo", "CREATE TABLE t (x INTEGER)");

    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_spanners"))
        .arg("--home")
        .arg(f.home())
        .args(["check", "-c", "demo", "--sql", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn");
    {
        use std::io::Write;
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(b"SELECT 1")
            .unwrap();
    }
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stdout(&out).trim(), "allow");

    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_spanners"))
        .arg("--home")
        .arg(f.home())
        .args(["explain", "-c", "demo", "--sql", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn");
    {
        use std::io::Write;
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(b"SELECT * FROM t")
            .unwrap();
    }
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stdout(&out).to_ascii_uppercase().contains("SCAN"),
        "{}",
        stdout(&out)
    );
}

#[cfg(unix)]
#[test]
fn ctrl_c_cancels_the_statement_instead_of_killing_the_client() {
    let f = Fixture::new();
    f.add("demo", "local");

    // Long enough that the signal arrives while it is still running.
    let child = std::process::Command::new(env!("CARGO_BIN_EXE_spanners"))
        .arg("--home")
        .arg(f.home())
        .args([
            "query",
            "--connection",
            "demo",
            "--sql",
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 200000000) SELECT i FROM n",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn");

    std::thread::sleep(std::time::Duration::from_millis(800));
    let killed = std::process::Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .expect("send SIGINT");
    assert!(killed.success());

    let out = child.wait_with_output().expect("wait");
    let err = stderr(&out);
    // The client stayed alive, asked the server to stop, and said so.
    assert!(err.contains("cancelling"), "{err}");
    assert!(err.contains("cancelled"), "{err}");
    assert!(!out.status.success());
}

#[test]
fn export_to_stdout_streams_the_same_csv_as_a_file() {
    let f = Fixture::new();
    f.add("demo", "local");
    f.query("demo", "CREATE TABLE t (x INTEGER)");
    f.query("demo", "INSERT INTO t VALUES (1), (2), (NULL)");

    let out = spanners(
        f.home(),
        &[
            "export",
            "--connection",
            "demo",
            "--sql",
            "SELECT * FROM t",
            "--out",
            "-",
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    // A NULL stays distinct from an empty string once it leaves the tool.
    assert_eq!(stdout(&out), "x\n1\n2\n\\N\n");
    assert!(stderr(&out).contains("3 row(s)"), "{}", stderr(&out));
}

#[test]
fn export_refuses_table_output_instead_of_writing_csv() {
    let f = Fixture::new();
    f.add("demo", "local");
    f.query("demo", "CREATE TABLE t (x INTEGER)");
    let path = f.home().join("out.csv");

    let out = spanners(
        f.home(),
        &[
            "export",
            "--connection",
            "demo",
            "--sql",
            "SELECT * FROM t",
            "--out",
            path.to_str().unwrap(),
            "--format",
            "table",
        ],
    );
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("export writes csv or jsonl"),
        "{}",
        stderr(&out)
    );
    assert!(!path.exists());
}

#[test]
fn a_missing_password_says_how_to_supply_it() {
    let f = Fixture::new();
    let added = spanners(
        f.home(),
        &[
            "connections",
            "add",
            "pg",
            "--url",
            "postgres://someone@127.0.0.1:1/db",
        ],
    );
    assert!(added.status.success(), "{}", stderr(&added));

    // stdin is /dev/null, so there is no terminal to prompt on: the command
    // must fail with instructions rather than wait for input or attempt a
    // connection it cannot authenticate.
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_spanners"))
        .arg("--home")
        .arg(f.home())
        .args(["query", "-c", "pg", "--sql", "SELECT 1"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .expect("run spanners");
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(err.contains("needs a password"), "{err}");
    assert!(err.contains("SPANNERS_PASSWORD"), "{err}");
}

#[test]
fn a_saved_query_remembers_its_connection_and_can_be_edited() {
    let f = Fixture::new();
    f.add("demo", "local");

    let added = spanners(
        f.home(),
        &[
            "saved",
            "add",
            "count",
            "--sql",
            "SELECT 1 AS n",
            "-c",
            "demo",
        ],
    );
    assert!(added.status.success(), "{}", stderr(&added));
    let id = stdout(&added).trim().to_string();

    // No -c: the connection it was saved against is used.
    let run = spanners(f.home(), &["saved", "run", "count"]);
    assert!(run.status.success(), "{}", stderr(&run));
    assert!(stdout(&run).contains('1'), "{}", stdout(&run));

    let edited = spanners(
        f.home(),
        &[
            "saved",
            "edit",
            &id,
            "--name",
            "row count",
            "--sql",
            "SELECT 2 AS n",
            "--folder",
            "reports",
        ],
    );
    assert!(edited.status.success(), "{}", stderr(&edited));

    let list = spanners(f.home(), &["saved", "list"]);
    assert!(
        stdout(&list).contains("reports\trow count"),
        "{}",
        stdout(&list)
    );
    let show = spanners(f.home(), &["saved", "show", "row count"]);
    assert_eq!(stdout(&show).trim(), "SELECT 2 AS n");

    let run = spanners(f.home(), &["saved", "run", "row count"]);
    assert!(run.status.success(), "{}", stderr(&run));
    assert!(stdout(&run).contains('2'), "{}", stdout(&run));
}

#[test]
fn a_saved_query_without_a_connection_says_what_to_pass() {
    let f = Fixture::new();
    f.add("demo", "local");
    let added = spanners(f.home(), &["saved", "add", "loose", "--sql", "SELECT 1"]);
    assert!(added.status.success(), "{}", stderr(&added));

    let run = spanners(f.home(), &["saved", "run", "loose"]);
    assert!(!run.status.success());
    assert!(
        stderr(&run).contains("pass --connection"),
        "{}",
        stderr(&run)
    );

    // Supplying one works.
    let run = spanners(f.home(), &["saved", "run", "loose", "-c", "demo"]);
    assert!(run.status.success(), "{}", stderr(&run));
}

// ---------------------------------------------------------------------------
// Against real servers.
//
// Gated on the same variables as the driver and app suites, so the default
// `cargo test` needs no Docker. These prove the whole CLI path — URL parsing,
// saved profile, password from the environment, policy verdict, output — not
// the wire protocol, which the driver suites already cover.
// ---------------------------------------------------------------------------

/// `scheme://user:pass@host:port/db` → a URL without the password, plus the
/// password. Passing the password on the command line would store it in the
/// OS keychain and leak it into the process list; tests use the documented
/// environment variable instead.
fn split_password(url: &str) -> (String, String) {
    let (scheme, rest) = url.split_once("://").expect("a URL with a scheme");
    let (authority, tail) = rest.split_once('@').expect("credentials in the URL");
    let (user, password) = authority.split_once(':').expect("a password in the URL");
    (format!("{scheme}://{user}@{tail}"), password.to_string())
}

fn spanners_with_password(
    home: &std::path::Path,
    args: &[&str],
    variable: &str,
    password: &str,
) -> Output {
    Command::new(env!("CARGO_BIN_EXE_spanners"))
        .arg("--home")
        .arg(home)
        .args(args)
        .env(variable, password)
        .output()
        .expect("run spanners")
}

#[test]
fn postgres_end_to_end_through_the_cli() {
    let Ok(url) = std::env::var("SPANNERS_TEST_POSTGRES") else {
        eprintln!("skipped: SPANNERS_TEST_POSTGRES not set");
        return;
    };
    let (url, password) = split_password(&url);
    let f = Fixture::new();

    let added = spanners(f.home(), &["connections", "add", "pg", "--url", &url]);
    assert!(added.status.success(), "{}", stderr(&added));

    let q = spanners_with_password(
        f.home(),
        &["query", "-c", "pg", "--sql", "SELECT 1 AS n"],
        "SPANNERS_PASSWORD",
        &password,
    );
    assert!(q.status.success(), "{}", stderr(&q));
    // Rows go to stdout, the summary to stderr, so a pipe gets clean data.
    assert!(stdout(&q).contains('1'), "{}", stdout(&q));
    assert!(stderr(&q).contains("1 row(s)"), "{}", stderr(&q));

    // A script runs one statement at a time on a real server.
    let script = spanners_with_password(
        f.home(),
        &["query", "-c", "pg", "--sql", "SELECT 1 AS n; SELECT 2 AS n"],
        "SPANNERS_PASSWORD",
        &password,
    );
    assert!(script.status.success(), "{}", stderr(&script));
    // The per-statement progress marker is diagnostics, so it goes to stderr
    // too and stdout stays concatenated data.
    assert!(
        stderr(&script).contains("statement 2/2"),
        "{}",
        stderr(&script)
    );
    assert!(stdout(&script).contains('2'), "{}", stdout(&script));

    // Tightened to production, a write is refused before it reaches the server.
    let tightened = spanners(
        f.home(),
        &[
            "connections",
            "set-profile",
            "--connection",
            "pg",
            "production",
        ],
    );
    assert!(tightened.status.success(), "{}", stderr(&tightened));

    let read = spanners_with_password(
        f.home(),
        &["query", "-c", "pg", "--sql", "SELECT 1"],
        "SPANNERS_PASSWORD",
        &password,
    );
    assert!(read.status.success(), "{}", stderr(&read));

    let write = spanners_with_password(
        f.home(),
        &[
            "query",
            "-c",
            "pg",
            "--sql",
            "CREATE TABLE should_not_exist (x int)",
        ],
        "SPANNERS_PASSWORD",
        &password,
    );
    assert!(!write.status.success());
    assert!(stderr(&write).contains("blocked"), "{}", stderr(&write));
}

#[test]
fn mysql_end_to_end_through_the_cli() {
    let Ok(url) = std::env::var("SPANNERS_TEST_MYSQL") else {
        eprintln!("skipped: SPANNERS_TEST_MYSQL not set");
        return;
    };
    let (url, password) = split_password(&url);
    let f = Fixture::new();

    let added = spanners(f.home(), &["connections", "add", "my", "--url", &url]);
    assert!(added.status.success(), "{}", stderr(&added));

    let q = spanners_with_password(
        f.home(),
        &["query", "-c", "my", "--sql", "SELECT 1 AS n"],
        "SPANNERS_PASSWORD",
        &password,
    );
    assert!(q.status.success(), "{}", stderr(&q));
    assert!(stdout(&q).contains('1'), "{}", stdout(&q));
    assert!(stderr(&q).contains("1 row(s)"), "{}", stderr(&q));

    // A failed statement exits non-zero and says what the server said.
    let bad = spanners_with_password(
        f.home(),
        &[
            "query",
            "-c",
            "my",
            "--sql",
            "SELECT * FROM no_such_table_here",
        ],
        "SPANNERS_PASSWORD",
        &password,
    );
    assert!(!bad.status.success());
    assert!(
        stderr(&bad)
            .to_ascii_lowercase()
            .contains("no_such_table_here"),
        "{}",
        stderr(&bad)
    );
}

#[test]
fn connections_show_prints_the_rules_that_will_apply() {
    let f = Fixture::new();
    let added = spanners(
        f.home(),
        &[
            "connections",
            "add",
            "live",
            "--url",
            "postgres://reader@db.example.com:5432/app",
            "--profile",
            "production",
        ],
    );
    assert!(added.status.success(), "{}", stderr(&added));

    // Positional and -c are the same thing; the family's other commands use
    // the flag, a person typing it expects the name.
    let out = spanners(f.home(), &["connections", "show", "live"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("profile       production"), "{text}");
    assert!(text.contains("read only     yes"), "{text}");
    assert!(text.contains("row cap       200"), "{text}");
    assert!(text.contains("timeout       15s"), "{text}");
    assert!(text.contains("ddl           blocked"), "{text}");

    let flag = spanners(f.home(), &["connections", "show", "-c", "live"]);
    assert_eq!(stdout(&flag), text);
}

#[test]
fn connections_show_reports_overrides_and_a_local_profile() {
    let f = Fixture::new();
    // There is no command to set overrides yet; they are config-file only, and
    // this is what the UI will write.
    std::fs::write(
        f.home().join("config.toml"),
        r#"schema_version = 1

[[connections]]
id = "abc"
name = "tuned"
driver = "sqlite"
database = "/tmp/x.db"
profile = "production"

[connections.overrides]
max_rows = 50
timeout_ms = 2000
read_only = true
"#,
    )
    .unwrap();

    let out = spanners(f.home(), &["connections", "show", "tuned"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    // The effective limits, not just the preset: an override that is invisible
    // is one nobody can debug.
    assert!(text.contains("row cap 50"), "{text}");
    assert!(text.contains("timeout 2s"), "{text}");
    assert!(text.contains("row cap       50"), "{text}");

    // A local connection is unrestricted, and says so rather than omitting the
    // rows: "no row cap" and "we did not print it" must not look alike.
    let local = Fixture::new();
    local.add("scratch", "local");
    let out = spanners(local.home(), &["connections", "show", "scratch"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("row cap       none"), "{text}");
    assert!(text.contains("timeout       none"), "{text}");
    assert!(text.contains("ddl           allowed"), "{text}");
}

#[test]
fn history_says_when_and_can_be_narrowed_to_one_connection() {
    let f = Fixture::new();
    let one = f.add("one", "local");
    f.add("two", "local");
    f.query("one", "SELECT 1 AS alpha");
    f.query("two", "SELECT 2 AS beta");

    let all = spanners(f.home(), &["history"]);
    assert!(all.status.success(), "{}", stderr(&all));
    let text = stdout(&all);
    assert_eq!(text.lines().count(), 2, "{text}");
    // A list without a time cannot answer "when did I run that".
    assert!(text.starts_with("just now\t"), "{text}");

    let narrowed = spanners(f.home(), &["history", "-c", "one"]);
    assert!(narrowed.status.success(), "{}", stderr(&narrowed));
    let text = stdout(&narrowed);
    assert_eq!(text.lines().count(), 1, "{text}");
    assert!(text.contains("alpha"), "{text}");
    assert!(!text.contains("beta"), "{text}");

    // Nothing matching is not the same as no history at all.
    let none = spanners(f.home(), &["history", "-c", "one", "--search", "beta"]);
    assert!(none.status.success(), "{}", stderr(&none));
    assert_eq!(stdout(&none).trim(), "No history matches.");

    // A typo names the connection, not the history.
    let bad = spanners(f.home(), &["history", "-c", "nope"]);
    assert!(!bad.status.success());
    assert!(
        stderr(&bad).contains("no connection matches"),
        "{}",
        stderr(&bad)
    );

    // The filter addresses the connection by id too.
    let by_id = spanners(f.home(), &["history", "-c", &one]);
    assert!(by_id.status.success(), "{}", stderr(&by_id));
    assert_eq!(stdout(&by_id), text);
}

#[test]
fn clearing_history_keeps_saved_queries() {
    let f = Fixture::new();
    f.add("demo", "local");
    f.query("demo", "SELECT 1 AS alpha");
    let saved = spanners(
        f.home(),
        &["saved", "add", "keep", "--sql", "SELECT 1", "-c", "demo"],
    );
    assert!(saved.status.success(), "{}", stderr(&saved));

    let cleared = spanners(f.home(), &["history", "--clear"]);
    assert!(cleared.status.success(), "{}", stderr(&cleared));
    assert!(
        stderr(&cleared).contains("cleared 1 history entry"),
        "{}",
        stderr(&cleared)
    );

    assert_eq!(
        stdout(&spanners(f.home(), &["history"])).trim(),
        "No history yet."
    );
    // A saved query was written on purpose; clearing history must not eat it.
    let list = spanners(f.home(), &["saved", "list"]);
    assert!(stdout(&list).contains("keep"), "{}", stdout(&list));
}
