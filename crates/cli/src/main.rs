//! `spanners` — the same engine the desktop app uses, from a terminal.
//!
//! This is not a second product: it opens the same config, the same history and
//! the same policy engine, so a `Production` connection refuses writes here for
//! exactly the same reasons it does in the UI. It exists because a SQL client
//! that can only be driven by a mouse is not useful in a shell, and because it
//! makes the whole stack testable end to end before any window exists.

use std::path::PathBuf;
use tokio::io::AsyncWriteExt;

use clap::{Parser, Subcommand, ValueEnum};
use spanners_app::{App, RunOutcome};
use spanners_core::config::Connection;
use spanners_core::export::{ExportOpts, Format, NullRepr};
use spanners_core::{Policy, Profile, Verdict, Violation};
use spanners_driver_api::{ExecOpts, QueryEvent, Value};

#[derive(Parser, Debug)]
#[command(
    name = "spanners",
    version,
    about = "Spanners Data — query Postgres, MySQL and SQLite from a terminal"
)]
struct Cli {
    /// Override the state directory (config.toml and spanners.db live here).
    #[arg(long, global = true, env = "SPANNERS_HOME")]
    home: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Manage saved connections.
    Connections {
        #[command(subcommand)]
        action: ConnectionAction,
    },
    /// Run a query and print the rows.
    Query {
        /// Connection id or name.
        #[arg(long, short)]
        connection: String,
        /// SQL to run. Use `-` to read it from stdin.
        #[arg(long, short)]
        sql: String,
        #[arg(long, value_enum, default_value_t = Output::Table)]
        format: Output,
        /// Answer yes to a confirmation the profile would otherwise ask for.
        #[arg(long)]
        yes: bool,
        /// Password for a server connection; also read from $SPANNERS_PASSWORD.
        #[arg(long, env = "SPANNERS_PASSWORD", hide_env_values = true)]
        password: Option<String>,
        /// Maximum rows to print, overriding the connection profile.
        #[arg(long)]
        max_rows: Option<u32>,
        /// Run only the Nth statement in the buffer (1-based).
        #[arg(long)]
        statement: Option<usize>,
        /// Wrap the statement in BEGIN/COMMIT, rolling back on error. Required
        /// for writes on a staging connection.
        #[arg(long)]
        transaction: bool,
        /// Lift the read-only lock on a production connection for this run.
        /// Pass the connection's name, exactly, as confirmation.
        #[arg(long, value_name = "CONNECTION_NAME")]
        unlock_writes: Option<String>,
        /// Statement timeout in seconds, overriding the profile when shorter.
        /// 0 keeps the profile's timeout.
        #[arg(long)]
        timeout: Option<u64>,
    },
    /// Stream a query straight to a file, honouring the profile's caps.
    Export {
        #[arg(long, short)]
        connection: String,
        #[arg(long, short)]
        sql: String,
        /// Output path. `-` writes to stdout.
        #[arg(long, short)]
        out: String,
        #[arg(long, value_enum, default_value_t = Output::Csv)]
        format: Output,
        /// Answer yes to a confirmation the profile would otherwise ask for.
        #[arg(long)]
        yes: bool,
        /// Export only the Nth statement in the buffer (1-based).
        #[arg(long)]
        statement: Option<usize>,
        /// Wrap the query in BEGIN/COMMIT, rolling back on error.
        #[arg(long)]
        transaction: bool,
        /// Statement timeout in seconds, overriding the profile when shorter.
        /// 0 keeps the profile's timeout.
        #[arg(long)]
        timeout: Option<u64>,
        #[arg(long)]
        password: Option<String>,
    },
    /// Show recent query history.
    History {
        #[arg(long, default_value_t = 20)]
        limit: u32,
        /// Only entries containing this text.
        #[arg(long)]
        search: Option<String>,
    },
    /// Classify a statement without running it.
    Check {
        #[arg(long, short)]
        connection: String,
        #[arg(long, short)]
        sql: String,
    },
    /// List tables, views and columns.
    Schema {
        #[arg(long, short)]
        connection: String,
        /// Only show tables whose name contains this text.
        #[arg(long)]
        table: Option<String>,
        /// Print one line per column instead of one line per table.
        #[arg(long)]
        columns: bool,
        #[arg(long, env = "SPANNERS_PASSWORD", hide_env_values = true)]
        password: Option<String>,
    },
    /// Show the query plan without running the query.
    Explain {
        #[arg(long, short)]
        connection: String,
        #[arg(long, short)]
        sql: String,
        /// Print the plan as JSON when the server provides one.
        #[arg(long)]
        json: bool,
        #[arg(long, env = "SPANNERS_PASSWORD", hide_env_values = true)]
        password: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
enum ConnectionAction {
    /// List saved connections.
    List,
    /// Add a connection.
    Add {
        /// Name to show in the UI and use from the CLI.
        name: String,
        /// Driver id: postgres, mysql or sqlite. Required unless --url is given.
        #[arg(long)]
        driver: Option<String>,
        /// Paste a connection URL instead of typing each field, e.g.
        /// postgres://user:pass@host:5432/db?sslmode=require
        #[arg(long)]
        url: Option<String>,
        /// Host, or the database file for sqlite.
        #[arg(long)]
        host: Option<String>,
        #[arg(long)]
        port: Option<u16>,
        #[arg(long)]
        database: Option<String>,
        #[arg(long)]
        username: Option<String>,
        /// local, staging or production.
        #[arg(long, default_value = "local")]
        profile: String,
        /// Use this id instead of generating one.
        #[arg(long)]
        id: Option<String>,
        /// Store this password for the connection (never written to config).
        #[arg(long)]
        password: Option<String>,
    },
    /// Change the safety profile of an existing connection.
    SetProfile {
        /// Connection id or name.
        #[arg(long, short)]
        connection: String,
        /// local, staging or production.
        profile: String,
    },
    /// Delete a connection and its saved password.
    Remove {
        #[arg(long, short)]
        connection: String,
    },
    /// Test a saved connection without running anything.
    Test {
        #[arg(long, short)]
        connection: String,
        #[arg(long)]
        password: Option<String>,
    },
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum Output {
    Table,
    Csv,
    Jsonl,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    if let Err(e) = run(cli).await {
        eprintln!("spanners: {e}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<(), String> {
    let dir = match cli.home {
        Some(dir) => dir,
        None => spanners_core::config::ConfigStore::default_dir()
            .ok_or_else(|| "could not find a state directory; pass --home".to_string())?,
    };
    let mut app = App::open(&dir).map_err(|e| format!("could not open {}: {e}", dir.display()))?;

    match cli.command {
        Command::Connections { action } => connections(&mut app, action).await,
        Command::Query {
            connection,
            sql,
            format,
            yes,
            password,
            max_rows,
            statement,
            transaction,
            unlock_writes,
            timeout,
        } => {
            query(
                &mut app,
                QueryArgs {
                    connection,
                    sql: read_sql(&sql)?,
                    format,
                    yes,
                    password,
                    max_rows,
                    statement,
                    transaction,
                    unlock_writes,
                    timeout,
                },
            )
            .await
        }
        Command::Export {
            connection,
            sql,
            out,
            format,
            yes,
            statement,
            transaction,
            timeout,
            password,
        } => {
            export(
                &mut app,
                ExportArgs {
                    connection,
                    sql: read_sql(&sql)?,
                    out,
                    format,
                    yes,
                    statement,
                    transaction,
                    timeout,
                    password,
                },
            )
            .await
        }
        Command::History { limit, search } => history(&app, limit, search.as_deref()),
        Command::Check { connection, sql } => check(&app, &connection, &sql).await,
        Command::Schema {
            connection,
            table,
            columns,
            password,
        } => schema(&mut app, &connection, table.as_deref(), columns, password).await,
        Command::Explain {
            connection,
            sql,
            json,
            password,
        } => explain(&mut app, &connection, &sql, json, password).await,
    }
}

async fn connections(app: &mut App, action: ConnectionAction) -> Result<(), String> {
    match action {
        ConnectionAction::List => {
            if app.connections().is_empty() {
                println!("No connections saved. Add one with `spanners connections add`.");
                return Ok(());
            }
            for c in app.connections() {
                println!(
                    "{}\t{}\t{}\t{}",
                    c.id,
                    c.name,
                    c.driver,
                    spanners_core::config::redact_dsn(&match (&c.host, &c.database) {
                        (Some(h), Some(d)) => format!("{h}/{d}"),
                        (Some(h), None) => h.clone(),
                        (None, Some(d)) => d.clone(),
                        (None, None) => String::new(),
                    })
                );
            }
            Ok(())
        }
        ConnectionAction::Add {
            name,
            driver,
            url,
            host,
            port,
            database,
            username,
            profile,
            id,
            password,
        } => {
            let profile = parse_profile(&profile)?;

            let (mut connection, url_password) = match url {
                Some(url) => Connection::from_url(name, &url, profile)
                    .map_err(|e| format!("could not read the URL: {e}"))?,
                None => {
                    let driver = driver.ok_or_else(|| {
                        "pass --driver (postgres, mysql or sqlite), or --url".to_string()
                    })?;
                    let mut c = Connection::new(name, driver, profile);
                    c.host = host;
                    c.port = port;
                    c.database = database;
                    c.username = username;
                    // A SQLite path is often typed as `--host`; accept either.
                    if c.driver == "sqlite" && c.database.is_none() {
                        c.database = c.host.take();
                    }
                    (c, None)
                }
            };
            if let Some(id) = id {
                connection.id = id;
            }

            let id = connection.id.clone();
            app.upsert_connection(connection)
                .map_err(|e| e.to_string())?;

            // Best effort: no keychain just means the user is asked next time.
            if let Some(pw) = url_password.or(password) {
                let _ = app.remember_password(&id, &pw);
            }
            println!("{id}");
            Ok(())
        }
        ConnectionAction::SetProfile {
            connection,
            profile,
        } => {
            let id = resolve(app, &connection)?;
            let mut c = app
                .connection(&id)
                .ok_or_else(|| format!("no connection {id}"))?
                .clone();
            c.profile = parse_profile(&profile)?;
            app.upsert_connection(c).map_err(|e| e.to_string())?;
            println!("{id}\t{profile}");
            Ok(())
        }
        ConnectionAction::Remove { connection } => {
            let id = resolve(app, &connection)?;
            app.remove_connection(&id)
                .await
                .map_err(|e| e.to_string())?;
            println!("removed {id}");
            Ok(())
        }
        ConnectionAction::Test {
            connection,
            password,
        } => {
            let id = resolve(app, &connection)?;
            let conn = app
                .connection(&id)
                .ok_or_else(|| format!("no connection {id}"))?
                .clone();
            let info = app
                .test_connection(&conn, password)
                .await
                .map_err(|e| e.to_string())?;
            println!(
                "ok\t{}\t{}\tread_only={}",
                info.server_version, info.driver, info.read_only
            );
            Ok(())
        }
    }
}

struct QueryArgs {
    connection: String,
    sql: String,
    format: Output,
    yes: bool,
    password: Option<String>,
    max_rows: Option<u32>,
    statement: Option<usize>,
    transaction: bool,
    unlock_writes: Option<String>,
    timeout: Option<u64>,
}

async fn query(app: &mut App, args: QueryArgs) -> Result<(), String> {
    let QueryArgs {
        connection,
        sql,
        format,
        yes,
        password,
        max_rows,
        statement,
        transaction,
        unlock_writes,
        timeout,
    } = args;
    let id = resolve(app, &connection)?;
    app.connect(&id, password)
        .await
        .map_err(|e| e.to_string())?;

    if let Some(confirmation) = unlock_writes {
        app.unlock_writes(&id, &confirmation)
            .await
            .map_err(|e| e.to_string())?;
        eprintln!("writes unlocked for this run");
    }

    // A buffer is run statement by statement, the way the editor will: a
    // driver executes exactly one statement, and running half a script while
    // reporting success is not acceptable.
    let statements = select_statements(app, &id, &sql, statement)?;
    let exec = ExecOpts {
        max_rows,
        timeout: timeout_opt(timeout),
        ..ExecOpts::default()
    };

    // CSV and JSON Lines go through the export engine, so a NULL, an empty
    // string and duplicate column names mean the same thing here as in a file.
    // One result set per file, so one statement.
    if format != Output::Table {
        let sql = match statements.as_slice() {
            [one] => one,
            _ => {
                return Err(format!(
                    "{} output takes one statement; this buffer has {} (use --statement N)",
                    if format == Output::Jsonl {
                        "JSON Lines"
                    } else {
                        "CSV"
                    },
                    statements.len()
                ))
            }
        };
        let opts = ExportOpts {
            format: if format == Output::Jsonl {
                Format::JsonLines
            } else {
                Format::Csv
            },
            header: true,
            null: NullRepr::Marker("\\N".into()),
        };
        if transaction {
            app.begin(&id).await.map_err(|e| e.to_string())?;
        }
        let mut stdout = tokio::io::stdout();
        let summary = match app
            .export_with_exec(&id, sql, exec, &opts, yes, &mut stdout)
            .await
        {
            Ok(summary) => summary,
            Err(e) => {
                if transaction {
                    let _ = app.rollback(&id).await;
                }
                return Err(describe(e));
            }
        };
        if transaction {
            app.commit(&id).await.map_err(|e| e.to_string())?;
        }
        stdout
            .flush()
            .await
            .map_err(|e| format!("could not write stdout: {e}"))?;
        eprintln!(
            "{} row(s) in {} ms{}",
            summary.rows,
            summary.elapsed_ms,
            if summary.truncated {
                " (truncated by the connection's row cap)"
            } else {
                ""
            }
        );
        return Ok(());
    }

    if transaction {
        app.begin(&id).await.map_err(|e| e.to_string())?;
    }
    let total = statements.len();
    for (i, sql) in statements.iter().enumerate() {
        if total > 1 {
            eprintln!("-- statement {}/{total}", i + 1);
        }
        if let Err(e) = run_table(app, &id, sql, exec.clone(), yes).await {
            if transaction {
                let _ = app.rollback(&id).await;
            }
            return Err(if total > 1 {
                format!("statement {}: {e}", i + 1)
            } else {
                e
            });
        }
    }
    if transaction {
        app.commit(&id).await.map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Run one statement and print it as a table, block by block.
async fn run_table(
    app: &mut App,
    id: &str,
    sql: &str,
    exec: ExecOpts,
    yes: bool,
) -> Result<(), String> {
    let cap = exec.max_rows;
    let mut run = match app
        .run_with_exec(id, sql, exec, yes)
        .await
        .map_err(|e| e.to_string())?
    {
        RunOutcome::Running(run) => run,
        RunOutcome::NeedsConfirmation { reasons } => {
            print_reasons("This statement needs confirmation", &reasons);
            return Err("re-run with --yes if that is what you meant".into());
        }
        RunOutcome::Blocked { reasons } => {
            print_reasons("Blocked by the connection profile", &reasons);
            return Err("refused before it reached the server".into());
        }
    };

    let mut printed: usize = 0;
    let mut error = None;
    let mut done = false;

    while let Some(event) = run.recv().await {
        match event {
            QueryEvent::Meta { columns: c } => {
                let columns: Vec<String> = c.into_iter().map(|c| c.name).collect();
                // DDL and other statements with no result set print nothing
                // rather than a stray header.
                if !columns.is_empty() {
                    println!("{}", columns.join("\t"));
                    println!("{}", "-".repeat(columns.join("\t").len().max(1)));
                }
            }
            QueryEvent::Batch { rows: batch, .. } => {
                for row in &batch {
                    if let Some(cap) = cap {
                        if printed >= cap as usize {
                            continue;
                        }
                    }
                    let cells: Vec<String> = row.0.iter().map(cell).collect();
                    println!("{}", cells.join("\t"));
                    printed += 1;
                }
            }
            QueryEvent::Done {
                elapsed_ms,
                row_count,
                truncated,
            } => {
                eprintln!(
                    "{row_count} row(s) in {elapsed_ms} ms{}",
                    if truncated {
                        " (truncated by the connection's row cap)"
                    } else {
                        ""
                    }
                );
                done = true;
            }
            QueryEvent::Failed { message, code, .. } => {
                error = Some(format!(
                    "{message}{}",
                    code.map(|c| format!(" [{c}]")).unwrap_or_default()
                ));
            }
        }
    }

    match error {
        Some(e) => Err(e),
        None if done => Ok(()),
        None => Err("query ended without a result".into()),
    }
}

/// The statements a run should execute: one chosen with `--statement`, or all
/// of them in order.
fn select_statements(
    app: &App,
    id: &str,
    sql: &str,
    statement: Option<usize>,
) -> Result<Vec<String>, String> {
    if let Some(n) = statement {
        return Ok(vec![pick_statement(app, id, sql, n)?]);
    }
    let statements = app
        .statements(id, sql)
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(|s| s.sql)
        .collect::<Vec<_>>();
    if statements.is_empty() {
        return Err("there is no statement to run".into());
    }
    Ok(statements)
}

struct ExportArgs {
    connection: String,
    sql: String,
    out: String,
    format: Output,
    yes: bool,
    statement: Option<usize>,
    transaction: bool,
    timeout: Option<u64>,
    password: Option<String>,
}

async fn export(app: &mut App, args: ExportArgs) -> Result<(), String> {
    let ExportArgs {
        connection,
        sql,
        out,
        format,
        yes,
        statement,
        transaction,
        timeout,
        password,
    } = args;
    let id = resolve(app, &connection)?;
    app.connect(&id, password)
        .await
        .map_err(|e| e.to_string())?;
    // One result set per file, so one statement.
    let statements = select_statements(app, &id, &sql, statement)?;
    let sql = match statements.as_slice() {
        [one] => one.as_str(),
        _ => {
            return Err(format!(
                "export takes one statement; this buffer has {} (use --statement N)",
                statements.len()
            ))
        }
    };
    let out = out.as_str();

    if transaction {
        app.begin(&id).await.map_err(|e| e.to_string())?;
    }

    let opts = ExportOpts {
        format: if format == Output::Jsonl {
            Format::JsonLines
        } else {
            Format::Csv
        },
        header: true,
        // `\N` so a NULL cannot be confused with an empty string once the file
        // leaves the tool.
        null: NullRepr::Marker("\\N".into()),
    };

    if out == "-" {
        let mut buf = spanners_core::export::Buffer::new();
        let summary = match app
            .export_with_exec(
                &id,
                sql,
                ExecOpts {
                    timeout: timeout_opt(timeout),
                    ..ExecOpts::default()
                },
                &opts,
                yes,
                &mut buf,
            )
            .await
        {
            Ok(summary) => summary,
            Err(e) => {
                if transaction {
                    let _ = app.rollback(&id).await;
                }
                return Err(describe(e));
            }
        };
        if transaction {
            app.commit(&id).await.map_err(|e| e.to_string())?;
        }
        let text = buf.into_string().map_err(|e| e.to_string())?;
        print!("{text}");
        eprintln!(
            "{} row(s){}",
            summary.rows,
            if summary.truncated {
                " (truncated by the connection's row cap)"
            } else {
                ""
            }
        );
        return Ok(());
    }

    // Write to a sibling temp file and rename on success. A blocked statement
    // or a failed query must not leave an empty file behind, and an existing
    // export must survive a failed re-run.
    let tmp = format!("{out}.spanners-{}", std::process::id());
    let mut file = tokio::fs::File::create(&tmp)
        .await
        .map_err(|e| format!("could not create {tmp}: {e}"))?;

    let result = app
        .export_with_exec(
            &id,
            sql,
            ExecOpts {
                timeout: timeout_opt(timeout),
                ..ExecOpts::default()
            },
            &opts,
            yes,
            &mut file,
        )
        .await;
    let flushed = file.flush().await;
    drop(file);

    let summary = match result {
        Ok(summary) => summary,
        Err(e) => {
            let _ = tokio::fs::remove_file(&tmp).await;
            if transaction {
                let _ = app.rollback(&id).await;
            }
            return Err(describe(e));
        }
    };
    if transaction {
        app.commit(&id).await.map_err(|e| e.to_string())?;
    }
    if let Err(e) = flushed {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(format!("could not write {tmp}: {e}"));
    }
    tokio::fs::rename(&tmp, out)
        .await
        .map_err(|e| format!("could not move {tmp} to {out}: {e}"))?;

    eprintln!(
        "wrote {} row(s) to {out}{}",
        summary.rows,
        if summary.truncated {
            " (truncated by the connection's row cap)"
        } else {
            ""
        }
    );
    Ok(())
}

fn history(app: &App, limit: u32, search: Option<&str>) -> Result<(), String> {
    let entries = match search {
        Some(text) => app
            .search(text, limit)
            .map_err(|e| e.to_string())?
            .into_iter()
            .map(|hit| {
                (
                    0,
                    spanners_core::HistoryEntry {
                        connection_id: hit.connection_id.unwrap_or_default(),
                        sql: hit.snippet,
                        started_at: hit.at,
                        duration_ms: 0,
                        row_count: None,
                        error: None,
                    },
                )
            })
            .collect::<Vec<_>>(),
        None => app.history(limit).map_err(|e| e.to_string())?,
    };

    if entries.is_empty() {
        println!("No history yet.");
        return Ok(());
    }
    for (_, entry) in entries {
        let status = match &entry.error {
            Some(e) => format!("error: {e}"),
            None => format!("{} row(s)", entry.row_count.unwrap_or(0)),
        };
        println!(
            "{}\t{}\t{}\t{}",
            entry.connection_id,
            status,
            oneline(&entry.sql),
            entry.duration_ms
        );
    }
    Ok(())
}

async fn check(app: &App, connection: &str, sql: &str) -> Result<(), String> {
    let id = resolve(app, connection)?;
    match app.judge(&id, sql).await.map_err(|e| e.to_string())? {
        Verdict::Allow => {
            println!("allow");
            Ok(())
        }
        Verdict::Confirm { reasons } => {
            print_reasons("needs confirmation", &reasons);
            Ok(())
        }
        Verdict::Block { reasons } => {
            print_reasons("blocked", &reasons);
            std::process::exit(2);
        }
    }
}

async fn schema(
    app: &mut App,
    connection: &str,
    filter: Option<&str>,
    one_line_per_column: bool,
    password: Option<String>,
) -> Result<(), String> {
    let id = resolve(app, connection)?;
    app.connect(&id, password)
        .await
        .map_err(|e| e.to_string())?;
    let schema = app.schema(&id).await.map_err(|e| e.to_string())?;

    let matches = |name: &str| match filter {
        Some(f) => name.to_ascii_lowercase().contains(&f.to_ascii_lowercase()),
        None => true,
    };

    for s in &schema.schemas {
        for table in &s.tables {
            if !matches(&table.name) {
                continue;
            }
            let kind = match table.kind {
                spanners_driver_api::TableKind::Table => "table",
                spanners_driver_api::TableKind::View => "view",
                spanners_driver_api::TableKind::Other => "other",
            };
            if one_line_per_column {
                for column in &table.columns {
                    println!(
                        "{}\t{}\t{}\t{}\t{}\t{}",
                        s.name,
                        table.name,
                        kind,
                        column.name,
                        column.type_name,
                        if column.nullable { "null" } else { "not null" }
                    );
                }
            } else {
                println!(
                    "{}\t{}\t{}\t{} column(s)",
                    s.name,
                    table.name,
                    kind,
                    table.columns.len()
                );
            }
        }
    }
    Ok(())
}

async fn explain(
    app: &mut App,
    connection: &str,
    sql: &str,
    as_json: bool,
    password: Option<String>,
) -> Result<(), String> {
    let id = resolve(app, connection)?;
    app.connect(&id, password)
        .await
        .map_err(|e| e.to_string())?;
    let plan = app.explain(&id, sql).await.map_err(|e| e.to_string())?;
    if as_json {
        if let Some(json) = plan.json {
            println!("{json}");
            return Ok(());
        }
    }
    print!("{}", plan.raw);
    if !plan.raw.ends_with('\n') {
        println!();
    }
    Ok(())
}

/// `--timeout 0` means "keep the profile's timeout", matching the config
/// convention that zero is "no override".
fn timeout_opt(seconds: Option<u64>) -> Option<std::time::Duration> {
    seconds
        .filter(|s| *s > 0)
        .map(std::time::Duration::from_secs)
}

/// Select one statement from a buffer, 1-based, the way a user counts them.
fn pick_statement(app: &App, id: &str, sql: &str, n: usize) -> Result<String, String> {
    let statements = app.statements(id, sql).map_err(|e| e.to_string())?;
    statements
        .get(n.saturating_sub(1))
        .map(|s| s.sql.clone())
        .ok_or_else(|| {
            format!(
                "statement {n} does not exist; the buffer has {}",
                statements.len()
            )
        })
}

/// Turn an app error into a message that says what happened, including the
/// policy reasons that a plain `Display` would drop.
fn describe(error: spanners_app::AppError) -> String {
    match error {
        spanners_app::AppError::Blocked { reasons } => {
            format!(
                "blocked by the connection profile:\n  - {}",
                reasons
                    .iter()
                    .map(Violation::message)
                    .collect::<Vec<_>>()
                    .join("\n  - ")
            )
        }
        spanners_app::AppError::NeedsConfirmation { reasons } => {
            format!(
                "this statement needs confirmation; re-run with --yes if that is what you meant:\n  - {}",
                reasons
                    .iter()
                    .map(Violation::message)
                    .collect::<Vec<_>>()
                    .join("\n  - ")
            )
        }
        other => other.to_string(),
    }
}

fn print_reasons(headline: &str, reasons: &[Violation]) {
    eprintln!("{headline}:");
    for reason in reasons {
        eprintln!("  - {}", reason.message());
    }
}

/// Accept either the id or the (unique) name, because typing `c1a2b3` is not a
/// user interface.
fn resolve(app: &App, key: &str) -> Result<String, String> {
    if app.connection(key).is_some() {
        return Ok(key.to_string());
    }
    let matches: Vec<&Connection> = app.connections().iter().filter(|c| c.name == key).collect();
    match matches.as_slice() {
        [one] => Ok(one.id.clone()),
        [] => Err(format!("no connection matches '{key}'")),
        many => Err(format!(
            "'{key}' matches {} connections; use the id",
            many.len()
        )),
    }
}

fn parse_profile(value: &str) -> Result<Profile, String> {
    match value.to_ascii_lowercase().as_str() {
        "local" => Ok(Profile::Local),
        "staging" => Ok(Profile::Staging),
        "production" | "prod" => Ok(Profile::Production),
        other => Err(format!(
            "unknown profile '{other}'; use local, staging or production"
        )),
    }
}

fn read_sql(input: &str) -> Result<String, String> {
    if input != "-" {
        return Ok(input.to_string());
    }
    let mut sql = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut sql)
        .map_err(|e| format!("could not read stdin: {e}"))?;
    Ok(sql)
}

fn cell(value: &Value) -> String {
    match value.as_text() {
        Some(text) => text.replace(['\t', '\n', '\r'], " "),
        None => "NULL".into(),
    }
}

fn oneline(sql: &str) -> String {
    let flat = sql.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > 100 {
        flat.chars().take(99).collect::<String>() + "…"
    } else {
        flat
    }
}

#[allow(dead_code)]
fn policy_summary(policy: &Policy) -> String {
    format!(
        "profile={:?} read_only={} max_rows={:?} timeout={:?}",
        policy.profile, policy.read_only, policy.max_rows, policy.timeout
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_renders_as_null_not_empty() {
        assert_eq!(cell(&Value::Null), "NULL");
        assert_eq!(cell(&Value::Text(String::new())), "");
    }

    #[test]
    fn profiles_parse_with_the_names_a_user_would_type() {
        assert_eq!(parse_profile("prod").unwrap(), Profile::Production);
        assert_eq!(parse_profile("Staging").unwrap(), Profile::Staging);
        assert!(parse_profile("yolo").is_err());
    }

    #[test]
    fn history_lines_stay_on_one_line() {
        let long = "SELECT *\n  FROM t\n WHERE ".to_string() + &"x".repeat(200);
        let flat = oneline(&long);
        assert!(!flat.contains('\n'));
        assert!(flat.chars().count() <= 100);
    }
}
