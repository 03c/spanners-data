# Spanners Data

A lightweight, fast, cross-platform SQL client for Postgres, MySQL and SQLite.

The plan of record is [PLAN.md](PLAN.md). This README is the working state.

## Status

M-1 (shell spike) and the first slice of M0 (core engine) are built.

| Area | State |
| --- | --- |
| Driver boundary (`Driver`/`Session`) | done, object-safe, serde-friendly |
| Safety policy (Local/Staging/Production) | done, 9 classification tests |
| SQLite driver | done: streaming, cancel, timeouts, row caps, read-only |
| Postgres / MySQL | next (M2) |
| Shell decision | awaiting M-1 measurements from your machines |
| Tauri UI | spike only, throwaway |
| CI | Linux + Windows: fmt, clippy, tests, both spikes |

## Layout

```
crates/
  driver-api/       trait + value types; no database dependency
  core/             policy, config, profiles
  driver-sqlite/    rusqlite driver, real interrupt-based cancel
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
