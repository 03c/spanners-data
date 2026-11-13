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
| SQLite driver | done: streaming, cancel, timeouts, row caps, read-only |
| Postgres driver | done: streaming, `pg_cancel_backend`, statement timeout, caps |
| MySQL driver | done: streaming, `KILL QUERY`, `max_execution_time`, caps |
| App services (`spanners-app`) | done: connect, guarded run, cancel, schema, explain, export, history, saved queries |
| History + saved queries | done: one SQLite file, shared search, 10k/90-day prune |
| Export | done: streaming CSV and JSON Lines, cap-aware, backs clipboard copy |
| Dialect keywords | done: per-driver lists for highlighting and autocomplete |
| Shell decision | awaiting M-1 measurements from your machines |
| Tauri UI | spike only, throwaway |
| CI | Linux + Windows: fmt, clippy, tests, both spikes; Linux job with real Postgres + MySQL |

## Layout

```
crates/
  driver-api/       trait + value types; no database dependency
  core/             policy, config, history, export, dialect data
  app/              application services: sessions, guarded runs, secrets
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
