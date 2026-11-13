# Spanners Data — lightweight cross-platform SQL client

Plan of record. Status: draft for review.

## 0. Status (as built)

| Item | State |
| --- | --- |
| `spanners-driver-api` | Driver/Session traits, lossless `Value`, streamed `QueryEvent` |
| `spanners-core` | policy presets + overrides, SQL classification, `execute_guarded` gate, config store, history + saved-query store, streaming CSV/JSONL export |
| `spanners-driver-sqlite` | streaming, interrupt cancel, timeout, fetch-stop caps, read-only |
| `spanners-driver-postgres` | streaming, `pg_cancel_backend` cancel, `statement_timeout`, fetch-stop caps with bounded resync, schema, `EXPLAIN (FORMAT JSON)` |
| `spanners-driver-mysql` | streaming, `KILL QUERY` cancel, `max_execution_time`, fetch-stop caps with bounded resync, schema, `EXPLAIN FORMAT=JSON` |
| `spanners-registry` | compile-time driver registry (`mysql`, `postgres`, `sqlite`) |
| `spanners-app` | app services: connect/sessions, guarded runs, cancel, schema, explain, export, history, saved queries, secret store |
| `spanners-cli` | `spanners` binary: connections, query (table/csv/jsonl), export, history, check; same config, history and policy as the app |
| Tests | 175: 84 core, 29 app, 3 app end-to-end against real servers, 20 CLI (17 process-level), 17 driver integration, 22 driver unit/registry |
| M-1 spikes | both build in CI; harness validated headless under Xvfb |
| CI | green: fmt, clippy, tests, both spikes on ubuntu-22.04 + windows-latest; Linux job with Postgres 16 + MySQL 8.4 service containers |
| UI | not started — blocked on the shell decision (M-1) |
| Shell | undecided — M-1 measurements pending on your machines |


## 1. Goal

A SQL client that opens instantly, never blocks the UI, and cannot accidentally
wreck a production database. Windows + Linux.

Explicit success criteria (measured, not felt):

| Metric | Target |
| --- | --- |
| Cold start to usable window | ≤ 700 ms (Tauri) / ≤ 200 ms (egui), per the §3.2 bar |
| Connect to local Postgres | < 150 ms |
| First rows on screen after query returns | < 100 ms |
| Scroll at 100k rows | 60 fps, no jank |
| Idle RSS, one connection, empty grid | < 120 MB |
| Installer size / binary | < 25 MB |
| Clean CI build (Linux + Windows) | < 8 min |

## 2. Why the current tool feels slow (and what we copy)

DBeaver's pain is structural, not cosmetic:

1. **JVM + JDBC + OSGi plugin runtime** — hundreds of ms before anything is
   drawn, and a large resident heap.
2. **Plugins loaded eagerly** — every driver contributes to startup even if
   unused.
3. **Result grids materialize the full result set** — a `SELECT *` on a big
   table allocates millions of rows before the first pixel.
4. **Everything on one thread** — a slow query blocks the shell.

Spanners Data inverts each: native binary, lazy driver init, bounded streaming
with a row cap, and a fully async core where the UI only ever renders a window
of rows.

## 3. Shell — the one decision still open

Everything that talks to a database is Rust in every option below. The only
question is **what draws pixels**, and it trades roughly 250 ms of startup
against two months of schedule.

| Shell | Cold start\* | Idle RSS\* | SQL editor | Extra work vs Tauri |
| --- | --- | --- | --- | --- |
| **Tauri 2** + Svelte + CodeMirror 6 | 300–600 ms | 110–180 MB | mature: per-dialect grammar, schema autocomplete, vim, multi-cursor, IME | — |
| **egui** | 50–120 ms | 40–90 MB | must be built: `ropey` + `cosmic-text` + `tree-sitter`, then multi-cursor, IME, undo, autocomplete | +8–12 wk |
| **Slint** | 60–150 ms | 50–100 MB | must be built, on a weaker text-input stack | +10–14 wk |
| **Native per-OS** (`gtk-rs` / WinUI) | 40–100 ms | 40–80 MB | two independent implementations | +16 wk |

\* Order of magnitude, from experience — **not measured on your hardware**. The
M-1 spike below replaces this column with real numbers before we commit.

### 3.1 Why the 250 ms is probably not the 250 ms you feel

DBeaver's lag is ~85% architecture and ~15% rendering: 3–8 s of JVM+OSGi before
the window paints, eager plugin loading, synchronous JDBC on the UI thread, and
a grid that allocates every row of the result before showing the first one. A
Tauri build fixing only those lands at ~400 ms and ~150 MB — a 10–20×
improvement, and the difference between "clicks and feels instant" and "wait for
it".

The remaining delta is real but bounded, and it is *not* JavaScript being slow.
The design keeps JS strictly O(viewport): the Rust core parses, streams and
enforces policy; the UI holds at most a few hundred DOM cells and never sees more
than a screen of data. Batches cross the IPC boundary as **binary frames**
(`tauri::ipc::Channel` + `InvokeResponseBody::Raw`, bincode/MessagePack, decoded
off the main thread), not as one 100k-row JSON blob. JSON-per-row IPC is the one
place a webview client genuinely does become slow, and it is designed out.

The genuine risk is narrower than "Tauri is slow": **WebKitGTK on Linux** has
worse scroll compositing and first-paint than WebView2 on Windows. That is
measurable in a day, so it should be measured in a day.

### 3.2 M-1 spike: decide with numbers, not vibes

Two to three days, two throwaway apps, the same three screens in each:

1. SQL editor loaded with 20k lines, syntax highlighting, one keystroke →
   repaint.
2. Virtual grid at 100k rows: scroll, select, copy 1k cells.
3. Query-result flow: 500-row binary batches streaming in, first row painted.

Metrics recorded on Linux **and** Windows: cold start to first usable paint,
idle RSS, RSS at 100k rows, p95 frame time while scrolling, time-to-first-row,
copy latency. The winner becomes the shell; the loser is deleted. The Rust core
(driver trait, policy engine, streaming) is UI-agnostic and carries over
unchanged either way — which is why this decision can safely wait.

Pre-registered pass bar, so the spike can't be argued with afterwards:

| Metric | Tauri must hit | egui must hit |
| --- | --- | --- |
| Cold start to usable paint | ≤ 700 ms | ≤ 200 ms |
| p95 frame time, 100k-row scroll | ≤ 16.7 ms, no visible stutter | ≤ 16.7 ms, no visible stutter |
| Time to first row after first batch | ≤ 150 ms | ≤ 150 ms |
| Idle RSS (1 connection, empty grid) | ≤ 200 MB | ≤ 100 MB |

Decision rule: **if Tauri passes every row, take Tauri** — it wins on editor
quality and schedule, and the extra milliseconds are inside the noise floor of
human perception for a tool you keep open all day. egui is chosen only if Tauri
fails a row *and* egui passes it with an editor that is usable for real SQL. If
neither passes, we stop and reconsider the scope rather than the shell.

If "cold start under 200 ms" turns out to be a hard requirement rather than a
nice-to-have, egui wins by default and we budget the editor work up front. If
"the editor must feel like a modern editor", Tauri wins even at 400 ms.

### 3.3 Driver and storage stack (identical in both shells)

| Layer | Choice | Why |
| --- | --- | --- |
| Core | **Rust** (edition 2021, `tokio`) | one async engine, no GC pauses |
| SQL parsing | `sqlparser` (PostgreSql/MySql/SQLite dialects) | statement classification for the guardrails |
| Postgres / MySQL | `sqlx` 0.9 (pure-Rust wire protocols) | no libpq/libmysqlclient to install; trivial CI |
| SQLite | `rusqlite` 0.42 (`bundled`) | real `interrupt()` handle; sqlx's SQLite has none |
| Secrets | `keyring` 3 | Windows Credential Manager / Secret Service |
| History store | SQLite via `rusqlite` | dogfooding, searchable, no server |

**Driver mix is deliberate.** A single `sqlx` everything would be tidier, but
sqlx does not expose SQLite's interrupt handle, and Postgres cancellation goes
through `pg_cancel_backend()` on a second connection. Our `Driver` trait hides
this, so per-driver choice is an implementation detail, not an architectural
dependency.

**No compile-time query macros.** `sqlx::query!` requires a live database at
build time; we use the runtime `sqlx::query()` API exclusively so CI needs no DB.

## 4. Architecture

```
┌──────────────────────────────────────────────┐
│ UI shell  (tabs, editor, virtual grid)       │
└───────────────┬──────────────────────────────┘
                │ commands + binary batch channel
┌───────────────▼──────────────────────────────┐
│ spanners-desktop  (thin command surface)     │
├──────────────────────────────────────────────┤
│ spanners-core                                │
│  config · policy engine · session manager     │
│  query executor · history · export · keychain │
├──────────────────────────────────────────────┤
│ spanners-driver-api  (trait, Value, Page)    │
├──────────────────────────────────────────────┤
│ driver-postgres · driver-mysql · driver-sqlite│
└──────────────────────────────────────────────┘
```

Workspace layout:

```
spanners/
├── Cargo.toml                 # workspace
├── crates/
│   ├── driver-api/            # trait + shared types, zero DB deps
│   ├── driver-postgres/
│   ├── driver-mysql/
│   ├── driver-sqlite/
│   ├── registry/              # statically links drivers, maps id -> Driver
│   └── core/
├── apps/desktop/
│   ├── src-tauri/             # shell app + commands
│   ├── src/                   # UI
│   └── src-tauri/capabilities/
├── .github/workflows/{ci,release}.yml
└── PLAN.md
```

### 4.1 Driver trait (the "plugin" boundary)

Compile-time plugins: a new database is a new crate plus one line in
`registry`. The trait carries only serializable types so a `.dll` loader can be
added in v2 without touching driver code.

```rust
// crates/driver-api/src/lib.rs
#[async_trait::async_trait]
pub trait Driver: Send + Sync {
    fn id(&self) -> &'static str;            // "postgres" | "mysql" | "sqlite"
    fn label(&self) -> &'static str;
    fn dialect(&self) -> Dialect;            // drives SQL highlighting
    fn default_port(&self) -> u16;
    fn build_dsn(&self, c: &ConnConfig) -> Result<Dsn, DriverError>;
    async fn connect(&self, c: &ConnConfig) -> Result<Box<dyn Session>, DriverError>;
}

#[async_trait::async_trait]
pub trait Session: Send + Sync {
    fn server_version(&self) -> &str;

    /// Starts execution. Returns immediately; rows flow on the channel.
    async fn execute(
        &self,
        sql: &str,
        opts: ExecOpts,                      // max_rows, timeout, readonly
        rows: mpsc::Sender<QueryEvent>,      // bounded; back-pressure is real
    ) -> Result<QueryId, DriverError>;

    /// Postgres: pg_cancel_backend(pid) from a second conn.
    /// MySQL:    KILL QUERY <id> from a second conn.
    /// SQLite:   InterruptHandle::interrupt().
    async fn cancel(&self, id: QueryId) -> Result<(), DriverError>;

    async fn schema(&self) -> Result<SchemaSnapshot, DriverError>;
    async fn explain(&self, sql: &str) -> Result<Plan, DriverError>;
    async fn in_transaction(&self) -> bool;
    async fn commit(&self) -> Result<(), DriverError>;
    async fn rollback(&self) -> Result<(), DriverError>;
    async fn close(self: Box<Self>) -> Result<(), DriverError>;
}

pub enum QueryEvent {
    Meta { columns: Vec<Column>, query_id: QueryId },
    Batch { rows: Vec<Row>, truncated: bool },   // <= 500 rows, <= 64 KiB
    Done { elapsed_ms: u64, row_count: u64, truncated: bool },
    Failed { message: String, code: Option<String>, position: Option<u32> },
}
```

One `Session` per UI tab. No shared pool: a session's transaction, temp tables
and `SET`s are its identity, and pooling silently corrupts that.

### 4.2 Streaming and memory

- Driver task pushes bounded batches (≤ 500 rows) over an `mpsc` (depth 4) into
  a Tauri `ipc::Channel<QueryEvent>`. Back-pressure means a 10M-row query never
  outruns the UI.
- The grid keeps rows in a flat store; it renders only the visible window.
- **Row cap is a fetch stop, not a `LIMIT` rewrite.** We fetch `cap + 1`; if the
  extra row arrives we stop reading, cancel the cursor, and show
  `200 rows shown · more available`. We never rewrite user SQL — rewriting
  breaks CTEs, `EXPLAIN`, and multi-statement input.
- Local profile defaults to unlimited with a 100k *in-memory soft cap*: on
  breach we stop and offer "Export full result". A spill-to-disk result cache is
  a v1.1 item, not v1 — measuring shows the cap covers real usage.
- `NULL` and empty string are visually distinct; numerics right-aligned;
  `Decimal` travels as string (no float precision loss); `bytea`/`BLOB` renders
  as `<binary 1.2 KB>` with a hex preview.

### 4.3 Safety: enforcement, not warnings

Three layers. Layer 1 is the actual guarantee; 2 and 3 are UX.

**Layer 1 — server-side session settings (cannot be bypassed by the UI):**

| Profile | Postgres | MySQL | SQLite |
| --- | --- | --- | --- |
| local | — | — | — |
| staging | `statement_timeout=60s` | `max_execution_time=60000` | progress handler |
| production | `default_transaction_read_only=on`, `statement_timeout=15s` | `SET SESSION TRANSACTION READ ONLY`, `max_execution_time=15000` | `PRAGMA query_only=ON` |

**Layer 2 — statement inspection** (`sqlparser`) blocks multi-statement input
and DDL on prod, and warns on `UPDATE`/`DELETE` without `WHERE`. Unparseable
input is treated as unknown → confirmation required, never as safe.

**Layer 3 — row cap + transaction gate**, as described above.

Presets (per connection, overridable):

| | local | staging | production |
| --- | --- | --- | --- |
| row cap | unlimited (100k mem cap) | 5,000 | **200** |
| transactions | auto-commit | manual, explicit Commit | read-only session |
| writes | allowed | allowed | **locked** |
| DDL | allowed | warn | blocked |
| multi-statement | allowed | allowed | blocked |
| statement timeout | none | 60 s | 15 s |

Production writes require an explicit **"Unlock writes for this session"**
action, confirmed by typing the connection name; the unlock auto-expires after
15 minutes idle, on tab close, and on disconnect. Read-only is enforced by the
server, so even a bug in our classifier cannot write.

`COMMIT` is never implicit on staging or production. Closing a tab or
disconnecting with an open transaction rolls back (with a confirm prompt).

### 4.4 Config, secrets, history

- Config: TOML in the platform config dir (`~/.config/dev.spanners.data` /
  `%APPDATA%\dev.spanners.data`), atomic write (temp + rename), versioned with a
  `schema_version` key. Contains connection metadata and profile overrides —
  **never passwords**.
- Secrets: `keyring`, keyed `spanners:<connection-id>`. If no keyring daemon is
  available (headless Linux), fall back to prompting per session; never fail the
  connect flow and never write a plaintext fallback.
- History and saved queries live in the same SQLite file
  (`app_data_dir()/spanners.db`, WAL), so search spans both. History rows carry
  query text, connection id, timestamp, duration, row count and error, capped at
  10k rows / 90 days. Saved queries are a folder tree (`folder` + `query` tables,
  `ON DELETE SET NULL` on folder deletion) searchable by name and body. Secrets
  are redacted from any logged DSN.

### 4.5 UI surface (v1)

- **Left sidebar** — two collapsible panes. *Connections*: expand to schemas →
  tables → columns; click a table to open a `SELECT *` preview tab (respecting
  the row cap). *Queries*: per-connection folder tree of saved queries with
  drag-to-reorganise, plus one search box covering saved queries and history.
- **Tabs** — one per connection, several editor tabs each, each with its own
  result pane.
- **Editor** — CodeMirror 6 with the connection's dialect; autocomplete from
  keywords + live schema; `Ctrl/Cmd+Enter` run, `Cmd+Shift+Enter` run selection,
  `Cmd+.` cancel.
- **Result pane** — virtual grid, copy as TSV (selection) or CSV (all loaded);
  row/column counts; elapsed time; truncation banner.
- **Status bar** — connection, transaction state, row cap, timeout, read-only
  shield.
- **Explain** — `EXPLAIN (FORMAT JSON)` / `EXPLAIN FORMAT=JSON` /
  `EXPLAIN QUERY PLAN`; raw text + collapsible plan tree.
- **Export** — stream a fresh cursor straight to CSV/JSON without materializing
  in memory; export honours the same profile caps.
- **Command palette** (`Cmd+K`) — switch connection/tab, run, export, history.
- Light/dark following the OS.

## 5. Non-goals for v1

No inline table editing, no ER diagrams, no SSH tunnels, no cloud IAM auth, no
custom CA/mTLS UI, no cross-database joins, no plugin marketplace, no team
sync/sharing, no MSI installer. Each is a deliberate v2 candidate; the trait and
config schema leave room.

## 6. Milestones

Estimates assume one developer, order-of-magnitude only.

| # | Deliverable | Exit criteria | Est. |
| --- | --- | --- | --- |
| **M-1** | Shell spike: same 3 screens in Tauri and egui, measured on both OSes | Startup/RSS/frame-time table; shell chosen; losing app deleted | 2–3 d |
| M0 | Workspace skeleton, app boots, CI green on Linux+Windows | Empty window launches from CI artifact | 3–5 d |
| M1 | Postgres vertical slice: connect, run, stream, cancel, cap | 1M-row query cancels in < 300 ms; first rows < 100 ms | 2 wk |
| M2 | SQLite + MySQL drivers, registry, driver tests | Same three drivers, one UI, same semantics | 1.5 wk |
| M3 | Policy engine: presets, read-only enforcement, txn gate, unlock flow | Prod connection provably rejects a write at the server | 1 wk |
| M4 | Schema browser, autocomplete, history, export | Table preview and CSV export on all three drivers | 1.5 wk |
| M5 | Explain viewer, keyboard/palette, packaging, perf pass | All §1 targets measured in CI artifact | 1.5 wk |

Build order is deliberate: M1 proves the risky part (streaming + cancel) before
breadth, and M3 lands before anyone points this at a real database.

## 7. Testing + CI

**Rust**
- Unit: policy engine and SQL classification as table-driven cases (including
  unparseable input, `SELECT 1; DROP TABLE x`, `UPDATE` without `WHERE`).
- Integration: `testcontainers` for Postgres + MySQL (Linux runners only — no
  Docker on `windows-latest`); `tempfile` SQLite tests on both OSes.
- Cancel tests are mandatory: start `pg_sleep(30)` / `SELECT SLEEP(30)`, cancel,
  assert elapsed < 1 s and that the server-side query is gone.

**UI**
- Vitest for logic/store.
- Playwright against the Vite dev server with a **mocked IPC layer** (fast, no
  Tauri-in-CI flakiness). Two `tauri-driver` smoke tests on Linux only.

**`.github/workflows/ci.yml`**
- Job 1 (both OSes): `cargo fmt --check`, `clippy -D warnings`, `cargo test`,
  `pnpm lint && pnpm typecheck && pnpm test`.
- Job 2 (matrix `ubuntu-22.04`, `windows-latest`): `tauri build`.
  Linux deps: `libwebkit2gtk-4.1-dev`, `libappindicator3-dev`, `librsvg2-dev`,
  `patchelf`. Bundles: `deb,appimage` on Linux; `nsis` on Windows, plus the raw
  `spanners.exe` uploaded as a portable artifact.
- Caching: `Swatinem/rust-cache` + pnpm store. `ubuntu-22.04` (not `latest`) for
  glibc compatibility.
- `release.yml`: tag-driven, reuses the matrix, uploads artifacts + SHA256s.

## 8. Risks

| Risk | Impact | Mitigation |
| --- | --- | --- |
| WebKitGTK jank/quirks on Linux (scroll, blur) | High — kills "fast" promise | M-1 spike measures it before anything is built on top; egui fallback is pre-scoped |
| Pure-Rust UI (egui/Slint) costs 8–12 wk of editor work | High — delays v1 | Only taken if M-1 shows the startup/RSS win is worth it; scope agreed as a separate workstream |
| sqlx cancellation gaps | High — uncancellable query = broken app | `pg_cancel_backend` / `KILL QUERY` on side connections; rusqlite interrupt for SQLite; cancel tests in CI |
| `keyring` unavailable on headless Linux | Medium | Prompt-per-session fallback; never plaintext |
| Scope creep into table editing | High — doubles the work | §5 non-goals; edit is v2+ |
| Result sets exceeding memory | Medium | Fetch-stop cap; export streams from a fresh cursor; spill cache in v1.1 |
| WebView2 version drift on Win10 | Low | Tauri's evergreen bootstrapper; pin in `tauri.conf.json` |

## 9. Settled decisions

| Decision | Value |
| --- | --- |
| Name | **Spanners Data**; UI short form "Spanners"; binary `spanners` |
| App identifier | `dev.spanners.data` (reverse-DNS of the owned `spanners.dev`; also the config dir and keychain namespace) |
| Shell | **Open — gated on the M-1 spike.** Tauri 2 + CodeMirror is the default; egui is the pure-Rust challenger |
| Drivers | Compile-time crates behind `Driver`/`Session`; sqlx for Postgres/MySQL, rusqlite for SQLite |
| SQLite driver | rusqlite, for a real `interrupt()` handle |
| Row caps | Fetch-stop at `cap + 1`, never a `LIMIT` rewrite |
| Prod posture | Server-enforced read-only; writes via 15-min type-to-confirm session unlock |
| Secrets | OS keychain, prompt-per-session fallback |
| Windows | NSIS installer + portable `.exe`; no MSI in v1 |
| Linux | `.deb` + `.AppImage` |
| Saved queries | Per-connection folders, drag-to-reorganise, shared search with history |
| Deployment | Desktop only; no local-server/browser mode |

## 10. Open

Blocking M0:

- **Shell** — decided by the M-1 spike (§3.2). Everything below the UI is
  shell-independent, so this delays nothing except the UI layer.

Revisit after M1 measurements:

- **Spill-to-disk result cache** — only if the 100k in-memory soft cap proves too
  low in practice. Arrow IPC would also give Parquet export for free.
- **MSI** — add if corporate/Intune distribution becomes a requirement (needs
  WiX in CI).
