//! Streaming export to CSV and JSON Lines.
//!
//! Export runs its own cursor instead of reusing whatever the grid is holding,
//! so a 10M-row table never has to fit in memory: rows are formatted as they
//! arrive and written straight out.
//!
//! The same formatting is used for "copy selection", so what lands in the
//! clipboard and what lands in a file cannot drift apart.

use std::pin::Pin;
use std::task::{Context, Poll};

use spanners_driver_api::{Column, ExecOpts, QueryEvent, Row, Session, Value};
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    #[error("{0}")]
    Driver(#[from] spanners_driver_api::DriverError),
    #[error("{message}")]
    Query {
        message: String,
        code: Option<String>,
    },
    #[error("could not write export: {0}")]
    Io(#[from] std::io::Error),
    /// The event stream ended without `Done` or `Failed`. A driver bug, but
    /// the file on disk is still valid up to the last complete batch.
    #[error("export ended early after {0} rows")]
    Incomplete(u64),
}

pub type Result<T> = std::result::Result<T, ExportError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Csv,
    JsonLines,
}

/// How `NULL` is written in CSV. JSON Lines always writes `null`, so only CSV
/// needs this choice.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum NullRepr {
    /// Empty field. What spreadsheet users expect, but indistinguishable from
    /// an empty string.
    #[default]
    Empty,
    /// An explicit marker, e.g. `\N` (Postgres' `COPY` convention) or `NULL`.
    Marker(String),
}

impl NullRepr {
    fn render(&self) -> &str {
        match self {
            NullRepr::Empty => "",
            NullRepr::Marker(m) => m,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ExportOpts {
    pub format: Format,
    /// Header row. CSV only; JSON Lines carries keys per row.
    pub header: bool,
    /// CSV only.
    pub null: NullRepr,
    /// Same contract as [`ExecOpts::max_rows`]: the profile's cap applies to
    /// exports too, so a "read-only, 200 rows" connection cannot be turned into
    /// a full dump by exporting.
    pub max_rows: Option<u32>,
}

impl Default for ExportOpts {
    fn default() -> Self {
        Self {
            format: Format::Csv,
            header: true,
            null: NullRepr::Empty,
            max_rows: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExportSummary {
    pub rows: u64,
    pub columns: usize,
    pub truncated: bool,
    pub elapsed_ms: u64,
}

/// Run `sql` and stream its rows into `out`.
///
/// Stops at `opts.max_rows` exactly like the grid does, via the driver's
/// fetch-stop, so the summary reports truncation rather than silently
/// pretending the export was complete.
pub async fn export<W: AsyncWrite + Unpin>(
    session: &dyn Session,
    sql: &str,
    opts: &ExportOpts,
    out: &mut W,
) -> Result<ExportSummary> {
    let (tx, mut rx) = mpsc::channel::<QueryEvent>(16);
    session
        .execute(
            sql,
            ExecOpts {
                max_rows: opts.max_rows,
                timeout: None,
                read_only: false,
            },
            tx,
        )
        .await?;

    let mut keys: Vec<String> = Vec::new();
    let mut rows_written: u64 = 0;
    let mut columns = 0usize;
    // Reused per batch: formatting into one buffer keeps the syscall count low
    // without holding more than a batch in memory.
    let mut buf = String::with_capacity(64 * 1024);

    while let Some(event) = rx.recv().await {
        match event {
            QueryEvent::Meta {
                columns: meta_columns,
            } => {
                columns = meta_columns.len();
                keys = unique_keys(&meta_columns);
                if opts.format == Format::Csv && opts.header && !meta_columns.is_empty() {
                    for (i, col) in meta_columns.iter().enumerate() {
                        if i > 0 {
                            buf.push(',');
                        }
                        push_csv_field(&mut buf, &col.name);
                    }
                    buf.push('\n');
                    out.write_all(buf.as_bytes()).await?;
                    buf.clear();
                }
            }
            QueryEvent::Batch { rows, .. } => {
                for row in &rows {
                    match opts.format {
                        Format::Csv => write_csv_row(&mut buf, row, &opts.null),
                        Format::JsonLines => write_json_row(&mut buf, row, &keys),
                    }
                    rows_written += 1;
                }
                if !buf.is_empty() {
                    out.write_all(buf.as_bytes()).await?;
                    buf.clear();
                }
            }
            QueryEvent::Done {
                elapsed_ms,
                row_count,
                truncated,
            } => {
                out.flush().await?;
                return Ok(ExportSummary {
                    // Trust the rows we wrote, not the count: a driver that
                    // miscounts must not produce a summary that disagrees with
                    // the file.
                    rows: rows_written.max(row_count),
                    columns,
                    truncated,
                    elapsed_ms,
                });
            }
            QueryEvent::Failed { message, code, .. } => {
                out.flush().await?;
                return Err(ExportError::Query { message, code });
            }
        }
    }

    // The channel closed without a terminal event: the driver dropped the
    // sender instead of reporting. Never report a clean export.
    out.flush().await?;
    Err(ExportError::Incomplete(rows_written))
}

fn write_csv_row(buf: &mut String, row: &Row, null: &NullRepr) {
    for (i, value) in row.0.iter().enumerate() {
        if i > 0 {
            buf.push(',');
        }
        match value.as_text() {
            // NULL and the empty string both render as an empty field unless
            // the caller asked for a marker; `Value::as_text` keeps them apart
            // for JSON, which does not have the ambiguity.
            None => push_csv_field(buf, null.render()),
            Some(text) => push_csv_field(buf, &text),
        }
    }
    buf.push('\n');
}

/// RFC 4180 quoting. Only quotes when needed, so small numeric tables stay
/// readable and small.
fn push_csv_field(buf: &mut String, field: &str) {
    let needs_quotes = field
        .bytes()
        .any(|b| matches!(b, b',' | b'"' | b'\n' | b'\r'))
        || field.starts_with(' ')
        || field.ends_with(' ');

    if !needs_quotes {
        buf.push_str(field);
        return;
    }
    buf.push('"');
    for ch in field.chars() {
        if ch == '"' {
            buf.push('"');
        }
        buf.push(ch);
    }
    buf.push('"');
}

fn write_json_row(buf: &mut String, row: &Row, keys: &[String]) {
    buf.push('{');
    for (i, value) in row.0.iter().enumerate() {
        if i > 0 {
            buf.push(',');
        }
        let key = keys.get(i).map(String::as_str).unwrap_or("column");
        push_json_string(buf, key);
        buf.push(':');
        push_json_value(buf, value);
    }
    buf.push('}');
    buf.push('\n');
}

/// JSON has no width limit, but export files are line-oriented: keep each
/// record on one line so `jq` and friends can stream them.
fn push_json_value(buf: &mut String, value: &Value) {
    match value {
        Value::Null => buf.push_str("null"),
        Value::Bool(b) => buf.push_str(if *b { "true" } else { "false" }),
        Value::Int(i) => buf.push_str(&i.to_string()),
        Value::Float(f) if f.is_finite() => buf.push_str(&f.to_string()),
        // JSON cannot represent NaN or infinity.
        Value::Float(_) => buf.push_str("null"),
        // Decimals stay strings: `12.30` must not become `12.3`, and JSON
        // numbers round-trip through f64 in most consumers.
        Value::Decimal(d) => push_json_string(buf, d),
        Value::Json(raw) => {
            let compact = raw.trim();
            // Pass real JSON through so nested documents stay queryable.
            if serde_json::from_str::<serde_json::Value>(compact).is_ok() {
                for ch in compact.chars() {
                    match ch {
                        '\n' | '\r' | '\t' => buf.push(' '),
                        _ => buf.push(ch),
                    }
                }
            } else {
                push_json_string(buf, raw);
            }
        }
        Value::Bytes(bytes) => {
            // Explicit and dependency-free: hex in a named field, so a reader
            // can tell binary from a base64-looking string.
            buf.push_str("{\"$binary\":\"");
            const HEX: &[u8; 16] = b"0123456789abcdef";
            for byte in bytes {
                buf.push(HEX[(byte >> 4) as usize] as char);
                buf.push(HEX[(byte & 0x0f) as usize] as char);
            }
            buf.push_str("\"}");
        }
        Value::Text(s) | Value::Uuid(s) | Value::Unknown(s) => push_json_string(buf, s),
        Value::Timestamp(_) | Value::Date(_) | Value::Time(_) => match value.as_text() {
            Some(s) => push_json_string(buf, &s),
            None => buf.push_str("null"),
        },
    }
}

fn push_json_string(buf: &mut String, s: &str) {
    buf.push('"');
    for ch in s.chars() {
        match ch {
            '"' => buf.push_str("\\\""),
            '\\' => buf.push_str("\\\\"),
            '\n' => buf.push_str("\\n"),
            '\r' => buf.push_str("\\r"),
            '\t' => buf.push_str("\\t"),
            c if (c as u32) < 0x20 => buf.push_str(&format!("\\u{:04x}", c as u32)),
            c => buf.push(c),
        }
    }
    buf.push('"');
}

/// JSON objects cannot hold duplicate keys, and `SELECT *` on a join produces
/// them constantly. Suffix rather than drop, so no column disappears.
fn unique_keys(columns: &[Column]) -> Vec<String> {
    let mut seen: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    columns
        .iter()
        .map(|c| {
            let count = seen.entry(c.name.as_str()).or_insert(0);
            *count += 1;
            if *count == 1 {
                c.name.clone()
            } else {
                format!("{}_{}", c.name, count)
            }
        })
        .collect()
}

/// An in-memory [`AsyncWrite`], for "copy as CSV" and for tests.
#[derive(Debug, Default)]
pub struct Buffer {
    bytes: Vec<u8>,
}

impl Buffer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn as_str(&self) -> std::result::Result<&str, std::str::Utf8Error> {
        std::str::from_utf8(&self.bytes)
    }

    pub fn into_string(self) -> std::result::Result<String, std::string::FromUtf8Error> {
        String::from_utf8(self.bytes)
    }
}

impl AsyncWrite for Buffer {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        self.get_mut().bytes.extend_from_slice(data);
        Poll::Ready(Ok(data.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spanners_driver_api::{Column, Driver as _};

    // ---- formatting helpers --------------------------------------------

    fn csv_of(columns: &[Column], rows: Vec<Row>) -> String {
        let mut buf = String::new();
        for col in columns {
            if !buf.is_empty() {
                buf.push(',');
            }
            push_csv_field(&mut buf, &col.name);
        }
        buf.push('\n');
        for row in &rows {
            write_csv_row(&mut buf, row, &NullRepr::Empty);
        }
        buf
    }

    fn col(name: &str) -> Column {
        Column::new(name, "text")
    }

    #[test]
    fn csv_quotes_only_what_needs_quoting() {
        let mut out = String::new();
        push_csv_field(&mut out, "plain");
        assert_eq!(out, "plain");

        out.clear();
        push_csv_field(&mut out, "a,b");
        assert_eq!(out, "\"a,b\"");

        out.clear();
        push_csv_field(&mut out, "say \"hi\"");
        assert_eq!(out, "\"say \"\"hi\"\"\"");

        out.clear();
        push_csv_field(&mut out, "two\nlines");
        assert_eq!(out, "\"two\nlines\"");

        out.clear();
        push_csv_field(&mut out, " padded");
        assert_eq!(out, "\" padded\"");
    }

    #[test]
    fn csv_keeps_null_and_empty_string_apart_when_marked() {
        let columns = [col("a"), col("b")];
        let rows = [Row(vec![Value::Null, Value::Text(String::new())])];

        let mut out = String::new();
        write_csv_row(&mut out, &rows[0], &NullRepr::Marker("\\N".into()));
        assert_eq!(out, "\\N,\n");

        // Default rendering is the spreadsheet-friendly one, which cannot tell
        // them apart. That is why JSON exists.
        let mut out = String::new();
        write_csv_row(&mut out, &rows[0], &NullRepr::Empty);
        assert_eq!(out, ",\n");

        let mut out = String::new();
        write_csv_row(&mut out, &rows[0], &NullRepr::Marker("NULL".into()));
        assert_eq!(out, "NULL,\n");
        assert_eq!(columns.len(), 2);
    }

    #[test]
    fn json_rows_keep_types_and_escape_text() {
        let keys = unique_keys(&[col("id"), col("name")]);
        let mut out = String::new();
        write_json_row(
            &mut out,
            &Row(vec![
                Value::Int(7),
                Value::Text("a \"quoted\"\nname".into()),
            ]),
            &keys,
        );
        assert_eq!(out, "{\"id\":7,\"name\":\"a \\\"quoted\\\"\\nname\"}\n");
    }

    #[test]
    fn json_passes_documents_through_instead_of_double_encoding() {
        let keys = unique_keys(&[col("doc")]);
        let mut out = String::new();
        write_json_row(
            &mut out,
            &Row(vec![Value::Json("{\"a\": [1, 2]}".into())]),
            &keys,
        );
        assert_eq!(out, "{\"doc\":{\"a\": [1, 2]}}\n");
    }

    #[test]
    fn json_decimal_is_a_string_and_bytes_are_named_hex() {
        let keys = unique_keys(&[col("amount"), col("blob")]);
        let mut out = String::new();
        write_json_row(
            &mut out,
            &Row(vec![
                Value::Decimal("12.30".into()),
                Value::Bytes(vec![0xde, 0xad]),
            ]),
            &keys,
        );
        assert_eq!(
            out,
            "{\"amount\":\"12.30\",\"blob\":{\"$binary\":\"dead\"}}\n"
        );
    }

    #[test]
    fn non_finite_floats_become_null_because_json_has_no_nan() {
        let keys = unique_keys(&[col("x")]);
        let mut out = String::new();
        write_json_row(&mut out, &Row(vec![Value::Float(f64::NAN)]), &keys);
        assert_eq!(out, "{\"x\":null}\n");
    }

    #[test]
    fn duplicate_column_names_are_suffixed_not_dropped() {
        let keys = unique_keys(&[col("id"), col("id"), col("id")]);
        assert_eq!(keys, ["id", "id_2", "id_3"]);
    }

    #[test]
    fn csv_row_matches_header_columns() {
        let columns = [col("a"), col("b")];
        let rows = vec![Row(vec![Value::Int(1), Value::Text("x,y".into())])];
        assert_eq!(csv_of(&columns, rows), "a,b\n1,\"x,y\"\n");
    }

    // ---- end to end against the real SQLite driver ----------------------

    async fn sqlite() -> Box<dyn Session> {
        let path = std::env::temp_dir().join(format!(
            "spanners-export-{}-{:?}.db",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_file(&path);
        let driver = spanners_driver_sqlite::SqliteDriver::new();
        let session = driver
            .connect(
                &spanners_driver_api::ConnConfig::sqlite(path.to_string_lossy().to_string()),
                spanners_driver_api::SessionOpts::default(),
            )
            .await
            .expect("connect");
        session
    }

    async fn run(session: &dyn Session, sql: &str) {
        let (tx, mut rx) = mpsc::channel(8);
        session
            .execute(sql, ExecOpts::default(), tx)
            .await
            .expect("execute");
        while let Some(event) = rx.recv().await {
            match event {
                QueryEvent::Done { .. } => return,
                QueryEvent::Failed { message, .. } => panic!("{message}"),
                _ => {}
            }
        }
    }

    #[tokio::test]
    async fn exports_a_real_query_to_csv() {
        let s = sqlite().await;
        run(&s, "CREATE TABLE t (id INTEGER, name TEXT, note TEXT)").await;
        run(
            &s,
            "INSERT INTO t VALUES (1, 'ada', NULL), (2, 'grace, hopper', 'ok')",
        )
        .await;

        let mut out = Buffer::new();
        let summary = export(
            &*s,
            "SELECT id, name, note FROM t ORDER BY id",
            &ExportOpts::default(),
            &mut out,
        )
        .await
        .unwrap();

        assert_eq!(summary.rows, 2);
        assert_eq!(summary.columns, 3);
        assert!(!summary.truncated);
        assert_eq!(
            out.as_str().unwrap(),
            "id,name,note\n1,ada,\n2,\"grace, hopper\",ok\n"
        );
    }

    #[tokio::test]
    async fn exports_json_lines_with_nulls() {
        let s = sqlite().await;
        run(&s, "CREATE TABLE t (id INTEGER, name TEXT)").await;
        run(&s, "INSERT INTO t VALUES (1, NULL)").await;

        let mut out = Buffer::new();
        let opts = ExportOpts {
            format: Format::JsonLines,
            header: false,
            null: NullRepr::default(),
            max_rows: None,
        };
        export(&*s, "SELECT id, name FROM t", &opts, &mut out)
            .await
            .unwrap();

        assert_eq!(out.as_str().unwrap(), "{\"id\":1,\"name\":null}\n");
    }

    #[tokio::test]
    async fn export_honours_the_row_cap_and_reports_truncation() {
        let s = sqlite().await;
        run(&s, "CREATE TABLE t (n INTEGER)").await;
        run(
            &s,
            "WITH RECURSIVE seq(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM seq WHERE n < 1000)
             INSERT INTO t SELECT n FROM seq",
        )
        .await;

        let mut out = Buffer::new();
        let opts = ExportOpts {
            max_rows: Some(10),
            null: NullRepr::Empty,
            ..ExportOpts::default()
        };
        let summary = export(&*s, "SELECT n FROM t", &opts, &mut out)
            .await
            .unwrap();

        assert_eq!(summary.rows, 10);
        assert!(summary.truncated);
        assert_eq!(out.as_str().unwrap().lines().count(), 11); // header + 10
    }

    #[tokio::test]
    async fn a_failing_query_returns_the_server_error() {
        let s = sqlite().await;
        let mut out = Buffer::new();
        let err = export(&*s, "SELECT * FROM nope", &ExportOpts::default(), &mut out)
            .await
            .unwrap_err();
        match err {
            ExportError::Query { message, .. } => {
                assert!(message.contains("no such table"), "{message}")
            }
            other => panic!("expected a query error, got {other:?}"),
        }
    }
}
