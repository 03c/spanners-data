//! Cell values, kept lossless.
//!
//! Two deliberate choices:
//! - `Decimal` is a string. Round-tripping money through `f64` is a bug, not a
//!   formatting detail.
//! - timestamps are micros since the Unix epoch in UTC, with the driver doing
//!   the conversion, so the UI never re-parses dates.
//!
//! `Null` and `Text("")` are distinct all the way to the grid.

use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", content = "v", rename_all = "lowercase")]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
    /// Microseconds since the Unix epoch, UTC.
    Timestamp(i64),
    /// Days since the Unix epoch.
    Date(i32),
    /// Microseconds since midnight.
    Time(i64),
    Decimal(String),
    Json(String),
    Uuid(String),
    Bytes(Vec<u8>),
    /// A type the driver could not map; rendered as its text form.
    Unknown(String),
}

impl Value {
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// Text form for clipboard and CSV export. `None` for NULL so callers can
    /// distinguish it from an empty string.
    pub fn as_text(&self) -> Option<String> {
        match self {
            Value::Null => None,
            Value::Bool(b) => Some(b.to_string()),
            Value::Int(i) => Some(i.to_string()),
            Value::Float(f) => Some(format_float(*f)),
            Value::Text(s) | Value::Decimal(s) | Value::Json(s) | Value::Uuid(s) => Some(s.clone()),
            Value::Timestamp(us) => Some(format_timestamp(*us)),
            Value::Date(d) => Some(format_date(*d)),
            Value::Time(us) => Some(format_time(*us)),
            Value::Bytes(b) => Some(format!("<binary {} B>", b.len())),
            Value::Unknown(s) => Some(s.clone()),
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.as_text() {
            Some(s) => f.write_str(&s),
            None => f.write_str("NULL"),
        }
    }
}

/// Shortest representation that round-trips, and never `1.0` for `1`.
fn format_float(f: f64) -> String {
    if f.is_finite() && f.fract() == 0.0 && f.abs() < 1e15 {
        format!("{f:.0}")
    } else {
        f.to_string()
    }
}

/// Civil date from days since 1970-01-01 (Howard Hinnant's algorithm). Avoids a
/// date-library dependency in the value layer.
fn format_date(days: i32) -> String {
    let z = days as i64 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

fn format_timestamp(micros: i64) -> String {
    let (days, rem) = (
        micros.div_euclid(86_400_000_000),
        micros.rem_euclid(86_400_000_000),
    );
    format!("{} {}", format_date(days as i32), format_time(rem))
}

fn format_time(micros: i64) -> String {
    let secs = micros.rem_euclid(86_400_000_000) / 1_000_000;
    let frac = micros.rem_euclid(1_000_000);
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if frac == 0 {
        format!("{h:02}:{m:02}:{s:02}")
    } else {
        format!("{h:02}:{m:02}:{s:02}.{frac:06}")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Column {
    pub name: String,
    pub type_name: String,
    pub nullable: bool,
    pub is_numeric: bool,
}

impl Column {
    pub fn new(name: impl Into<String>, type_name: impl Into<String>) -> Self {
        let type_name = type_name.into();
        let is_numeric = is_numeric_type(&type_name);
        Self {
            name: name.into(),
            type_name,
            nullable: true,
            is_numeric,
        }
    }
}

fn is_numeric_type(t: &str) -> bool {
    let t = t.split('(').next().unwrap_or(t).trim().to_ascii_lowercase();
    matches!(
        t.as_str(),
        "int"
            | "int2"
            | "int4"
            | "int8"
            | "integer"
            | "smallint"
            | "bigint"
            | "real"
            | "float"
            | "float4"
            | "float8"
            | "double"
            | "double precision"
            | "numeric"
            | "decimal"
            | "number"
    )
}

/// One result row. Boxed slice keeps a batch to one allocation per row rather
/// than one per cell.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Row(pub Vec<Value>);

impl Row {
    pub fn new(values: Vec<Value>) -> Self {
        Self(values)
    }
    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    pub fn get(&self, i: usize) -> Option<&Value> {
        self.0.get(i)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_is_not_empty_string() {
        assert_eq!(Value::Null.as_text(), None);
        assert_eq!(Value::Text(String::new()).as_text().as_deref(), Some(""));
    }

    #[test]
    fn floats_do_not_gain_a_fake_fraction() {
        assert_eq!(Value::Float(1.0).to_string(), "1");
        assert_eq!(Value::Float(1.5).to_string(), "1.5");
    }

    #[test]
    fn epochs_format_correctly() {
        assert_eq!(format_date(0), "1970-01-01");
        assert_eq!(format_date(19_723), "2024-01-01");
        assert_eq!(format_time(0), "00:00:00");
        assert_eq!(format_timestamp(0), "1970-01-01 00:00:00");
        assert_eq!(format_time(3_661_000_000), "01:01:01");
    }

    #[test]
    fn numeric_types_are_detected_with_precision() {
        assert!(Column::new("a", "NUMERIC(10,2)").is_numeric);
        assert!(Column::new("a", "int8").is_numeric);
        assert!(!Column::new("a", "text").is_numeric);
    }
}
