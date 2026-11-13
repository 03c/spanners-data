# Spanners Data

A lightweight, fast, cross-platform SQL client for Postgres, MySQL and SQLite.

The plan of record is [PLAN.md](PLAN.md). This README is the working state.

## Status

M-1 (shell spike) and everything below the UI is built. The UI is parked until
the shell decision, which is the only remaining blocker.

| Area | State |
| --- | --- |
| Driver boundary (`Driver`/`Session`) | done, object-safe, serde-friendly |
| Safety policy (Local/Staging/Production) | done: presets, overrides, classification, guarded execution |
| Production write unlock | done: name confirmation, 15-minute idle expiry, re-locks on disconnect or expiry |
| SQLite driver | done: streaming, cancel, timeouts, row caps, read-only |
| Postgres driver | done: streaming, `pg_cancel_backend`, statement timeout, caps |
| MySQL driver | done: streaming, `KILL QUERY`, `max_execution_time`, caps |
| App services (`spanners-app`) | done: connect, guarded run, cancel, schema, explain, export, history, saved queries |
| CLI (`spanners`) | done: connections (add from URL, show the resolved rules, set-profile, test, import from DBeaver), query (single statement or a whole script), export, history, saved queries, check, schema, explain — same config, history and policy as the app |
| History + saved queries | done: one SQLite file, shared search, filter by connection, clear, 10k/90-day prune |
| DBeaver import | done: `connections import` reads `data-sources.json`; never passwords; loopback = local, network = staging |
| Export | done: streaming CSV and JSON Lines, cap-aware, backs clipboard copy |
| Dialect keywords | done: per-driver lists for highlighting and autocomplete |
| Autocomplete | done: schema-aware ranking (columns of tables in scope, then tables, functions, keywords) |
| Shell decision | awaiting M-1 measurements from your machines |
| Tauri UI | spike only, throwaway |
| CI | Linux + Windows: fmt, clippy, tests, both spikes; Linux job with real Postgres + MySQL |

## Try it

There is no window yet, so the CLI is how to see the engine work. It opens the
same config, history and policy as the app will.

```bash
cargo build --release -p spanners-cli
S=target/release/spanners

# A connection from a URL, e.g. one already exported in your shell. Postgres,
# MySQL and SQLite URLs all work; on Postgres, `?sslmode=require` insists on
# TLS. The password goes to the OS keychain when there is one, and is never
# written to config.toml. A connection without one prompts on the first
# command, unless SPANNERS_PASSWORD or --password supplies it.
$S connections add local --url "$DATABASE_URL"

# Or bring the ones you already have. Found automatically, or pass --path.
$S connections import --from dbeaver --dry-run

# What will actually happen: the profile preset plus this connection's overrides.
$S connections show local

# Query, script, export, browse, explain.
$S query -c local --sql "SELECT * FROM users LIMIT 5"
$S query -c local --sql "CREATE TABLE t (id int); INSERT INTO t VALUES (1); SELECT * FROM t"
$S export -c local --sql "SELECT * FROM events" --out events.csv
$S schema -c local --columns --table users
$S explain -c local --sql "SELECT * FROM users WHERE id = 1"

# Staging: writes only inside an explicit transaction, DDL asks first.
$S query -c stage --sql "UPDATE users SET x = 1 WHERE id = 2" --transaction

# Production: read-only at the server. Unlock one run by typing the name.
$S query -c live --sql "UPDATE users SET x = 1 WHERE id = 2" --unlock-writes live

$S saved add "top users" --sql "SELECT * FROM users ORDER BY score DESC LIMIT 20" --folder reports
$S saved run "top users" -c local

# Ctrl-C asks the server to stop the statement and drains the session, so the
# connection is not left mid-stream. A second Ctrl-C exits immediately.

$S history -c live    # what you ran, on this connection, and when
$S check -c live --sql "SELECT 1; DROP TABLE users"  # judges each statement; exit 2 = blocked
```

`--home <dir>` keeps config and history in one place, which is handy for demos
and for tests that must not touch your real state.

## Layout

```
crates/
  driver-api/       trait + value types; no database dependency
  core/             policy, config, history, export, dialect data
  app/              application services: sessions, guarded runs, secrets
  cli/              the `spanners` binary (same engine, terminal surface)
  driver-sqlite/    rusqlite driver, real interrupt-based cancel
  driver-postgres/  sqlx driver, server-side read-only + timeouts
  driver-mysql/     sqlx driver, server-side read-only + timeouts
  registry/         compile-time driver registry
apps/
  spike-egui/       M-1: pure-Rust shell candidate
  spike-tauri/      M-1: Tauri + CodeMirror shell candidate
bench/              measurement harness and results
```

## Build and test

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

Database integration tests are env-gated so a plain `cargo test` never needs
Docker. To run them:

```bash
docker run -d --rm --name spanners-pg -e POSTGRES_PASSWORD=pass -p 55432:5432 postgres:16-alpine
docker run -d --rm --name spanners-mysql -e MYSQL_ROOT_PASSWORD=pass \
  -e MYSQL_DATABASE=spanners -p 53306:3306 mysql:8.4

SPANNERS_TEST_POSTGRES="postgres://postgres:pass@127.0.0.1:55432/postgres" \
SPANNERS_TEST_MYSQL="mysql://root:pass@127.0.0.1:53306/spanners" \
cargo test --workspace -- --test-threads=1
```

The spikes are excluded from the workspace because they need GUI system
libraries and must not slow down the core loop:

```bash
cargo build --release --manifest-path apps/spike-egui/Cargo.toml
(cd apps/spike-tauri && pnpm install && pnpm build)
cargo build --release --manifest-path apps/spike-tauri/src-tauri/Cargo.toml
```

## Decide the shell

```bash
bench/measure.sh
```

It prints cold start, p95 frame time while scrolling 100k rows, p95 frame time
while typing in a 20k-line buffer, and peak RSS for both shells. Run it on
Linux and Windows; the pass bar and decision rule are in PLAN.md §3.2.

## Safety model in one paragraph

Production connections are opened read-only *at the server* (`PRAGMA
query_only`, `SET default_transaction_read_only`, `SET SESSION TRANSACTION READ
ONLY`), so a bug in our SQL classifier cannot write. On top of that, statements
are classified before execution (multi-statement, DDL, `UPDATE`/`DELETE`
without `WHERE`, writes hidden in CTEs) and unparseable input is treated as
unsafe rather than safe. Row caps are a fetch stop — the user's SQL is never
rewritten.
