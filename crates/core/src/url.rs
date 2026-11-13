//! Parsing connection URLs, so a DSN can be pasted instead of retyped.
//!
//! Deliberately strict: an unknown scheme or a malformed port is an error, not
//! a guess. A connection dialog that silently connects to the wrong host is
//! worse than one that refuses.
//!
//! The password is returned separately and never stored on the [`Connection`];
//! the caller hands it to the secret store or uses it for one session.

use std::fmt;

/// The pieces of a connection URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedUrl {
    pub driver: String,
    pub host: Option<String>,
    pub port: Option<u16>,
    /// Database name, or a file path for SQLite.
    pub database: Option<String>,
    pub username: Option<String>,
    /// Runtime only; never persisted.
    pub password: Option<String>,
    /// Query parameters, e.g. `sslmode=require`.
    pub params: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UrlError {
    Empty,
    /// No `scheme://`, and not a `sqlite:` path.
    MissingScheme,
    UnsupportedScheme(String),
    /// A `%` not followed by two hex digits.
    BadEscape(String),
    BadPort(String),
    /// Credentials in the authority but no host after them.
    MissingHost,
}

impl fmt::Display for UrlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UrlError::Empty => write!(f, "the connection URL is empty"),
            UrlError::MissingScheme => write!(
                f,
                "expected a URL like postgres://user:pass@host:5432/db (or a sqlite: path)"
            ),
            UrlError::UnsupportedScheme(s) => write!(
                f,
                "unsupported scheme '{s}'; use postgres://, mysql:// or sqlite:"
            ),
            UrlError::BadEscape(s) => write!(f, "invalid percent-escape in '{s}'"),
            UrlError::BadPort(s) => write!(f, "'{s}' is not a valid port"),
            UrlError::MissingHost => write!(f, "the URL has credentials but no host"),
        }
    }
}

impl std::error::Error for UrlError {}

/// Parse a connection URL.
///
/// Accepted forms:
/// - `postgres://user:pass@host:5432/db?sslmode=require`
/// - `postgresql://…`, `mysql://…`, `mariadb://…`
/// - `sqlite::memory:`, `sqlite:///abs/path.db`, `sqlite:relative.db`
pub fn parse(input: &str) -> Result<ParsedUrl, UrlError> {
    let input = input.trim();
    if input.is_empty() {
        return Err(UrlError::Empty);
    }

    if let Some(rest) = input.strip_prefix("sqlite:") {
        return parse_sqlite(rest);
    }

    let (scheme, rest) = input.split_once("://").ok_or(UrlError::MissingScheme)?;
    let driver = match scheme.to_ascii_lowercase().as_str() {
        "postgres" | "postgresql" => "postgres",
        "mysql" | "mariadb" => "mysql",
        other => return Err(UrlError::UnsupportedScheme(other.to_string())),
    };

    // Split off the query string first so a `?` cannot be mistaken for a path.
    let (before_query, query) = match rest.split_once('?') {
        Some((a, b)) => (a, Some(b)),
        None => (rest, None),
    };

    let (authority, path) = match before_query.split_once('/') {
        Some((a, b)) => (a, Some(b)),
        None => (before_query, None),
    };

    let (username, password) = match authority.rsplit_once('@') {
        Some((creds, _host)) => {
            let (user, pass) = match creds.split_once(':') {
                Some((u, p)) => (Some(u), Some(p)),
                None => (Some(creds), None),
            };
            (
                user.map(percent_decode).transpose()?,
                pass.map(percent_decode).transpose()?,
            )
        }
        None => (None, None),
    };

    // The host is everything after the last '@'; recompute rather than trusting
    // a split, because a password may itself contain '@'.
    let hostport = match authority.rsplit_once('@') {
        Some((_, h)) => h,
        None => authority,
    };
    let (host, port) = parse_host_port(hostport)?;
    if host.is_none() && (username.is_some() || password.is_some()) {
        return Err(UrlError::MissingHost);
    }

    let database = match path {
        Some(p) if !p.is_empty() => Some(percent_decode(p)?),
        _ => None,
    };

    Ok(ParsedUrl {
        driver: driver.to_string(),
        host,
        port,
        database,
        username,
        password,
        params: parse_query(query)?,
    })
}

fn parse_sqlite(rest: &str) -> Result<ParsedUrl, UrlError> {
    // `sqlite::memory:` is the documented in-memory form.
    let (before_query, query) = match rest.split_once('?') {
        Some((a, b)) => (a, Some(b)),
        None => (rest, None),
    };
    let database = if before_query == ":memory:" || before_query == "//:memory:" {
        ":memory:".to_string()
    } else if let Some(path) = before_query.strip_prefix("///") {
        // `sqlite:///abs/path` → `/abs/path`.
        format!("/{path}")
    } else if let Some(path) = before_query.strip_prefix("//") {
        // `sqlite://relative` → `relative`.
        path.to_string()
    } else {
        before_query.to_string()
    };
    if database.is_empty() {
        return Err(UrlError::MissingScheme);
    }
    Ok(ParsedUrl {
        driver: "sqlite".into(),
        host: None,
        port: None,
        database: Some(percent_decode(&database)?),
        username: None,
        password: None,
        params: parse_query(query)?,
    })
}

fn parse_host_port(input: &str) -> Result<(Option<String>, Option<u16>), UrlError> {
    if input.is_empty() {
        return Ok((None, None));
    }
    // IPv6 literals: `[::1]:5432`.
    if let Some(rest) = input.strip_prefix('[') {
        let (host, tail) = rest
            .split_once(']')
            .ok_or_else(|| UrlError::BadPort(input.to_string()))?;
        let port = match tail.strip_prefix(':') {
            Some(p) if !p.is_empty() => Some(parse_port(p)?),
            _ => None,
        };
        return Ok((Some(host.to_string()), port));
    }
    match input.rsplit_once(':') {
        Some((host, "")) => Ok((Some(host.to_string()), None)),
        // A colon that is not followed by a port is a typo, not part of a
        // hostname; IPv6 literals are handled by the bracketed branch above.
        Some((_, port)) if !port.bytes().all(|b| b.is_ascii_digit()) => {
            Err(UrlError::BadPort(port.to_string()))
        }
        Some((host, port)) => Ok((Some(host.to_string()), Some(parse_port(port)?))),
        None => Ok((Some(percent_decode(input)?), None)),
    }
}

fn parse_port(input: &str) -> Result<u16, UrlError> {
    input
        .parse::<u16>()
        .map_err(|_| UrlError::BadPort(input.to_string()))
}

fn parse_query(query: Option<&str>) -> Result<Vec<(String, String)>, UrlError> {
    let Some(query) = query else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = match pair.split_once('=') {
            Some((k, v)) => (k, v),
            None => (pair, ""),
        };
        out.push((percent_decode(key)?, percent_decode(value)?));
    }
    Ok(out)
}

/// Decode `%XX` escapes. `+` is left alone: in a DSN it is a literal plus, not
/// a space, and guessing otherwise would corrupt passwords.
fn percent_decode(input: &str) -> Result<String, UrlError> {
    if !input.contains('%') {
        return Ok(input.to_string());
    }
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = bytes
                .get(i + 1..i + 3)
                .ok_or_else(|| UrlError::BadEscape(input.to_string()))?;
            let text =
                std::str::from_utf8(hex).map_err(|_| UrlError::BadEscape(input.to_string()))?;
            let byte =
                u8::from_str_radix(text, 16).map_err(|_| UrlError::BadEscape(input.to_string()))?;
            out.push(byte);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| UrlError::BadEscape(input.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_full_postgres_url() {
        let p = parse("postgres://ada:hunter2@db.example:5432/app?sslmode=require").unwrap();
        assert_eq!(p.driver, "postgres");
        assert_eq!(p.host.as_deref(), Some("db.example"));
        assert_eq!(p.port, Some(5432));
        assert_eq!(p.database.as_deref(), Some("app"));
        assert_eq!(p.username.as_deref(), Some("ada"));
        assert_eq!(p.password.as_deref(), Some("hunter2"));
        assert_eq!(p.params, [("sslmode".to_string(), "require".to_string())]);
    }

    #[test]
    fn accepts_the_postgresql_alias_and_mariadb() {
        assert_eq!(parse("postgresql://host/db").unwrap().driver, "postgres");
        assert_eq!(parse("mariadb://host/db").unwrap().driver, "mysql");
    }

    #[test]
    fn omits_missing_pieces() {
        let p = parse("postgres://db.example/app").unwrap();
        assert_eq!(p.port, None);
        assert_eq!(p.username, None);
        assert_eq!(p.password, None);
    }

    #[test]
    fn a_password_may_contain_an_at_sign() {
        let p = parse("postgres://ada:p@ss@db.example:5432/app").unwrap();
        assert_eq!(p.username.as_deref(), Some("ada"));
        assert_eq!(p.password.as_deref(), Some("p@ss"));
        assert_eq!(p.host.as_deref(), Some("db.example"));
    }

    #[test]
    fn percent_escapes_are_decoded() {
        let p = parse("postgres://ada:p%40ss%2Fword@host/db%20name").unwrap();
        assert_eq!(p.password.as_deref(), Some("p@ss/word"));
        assert_eq!(p.database.as_deref(), Some("db name"));
    }

    #[test]
    fn a_plus_is_not_a_space() {
        let p = parse("postgres://ada:a+b@host/db").unwrap();
        assert_eq!(p.password.as_deref(), Some("a+b"));
    }

    #[test]
    fn parses_ipv6_hosts() {
        let p = parse("postgres://ada@[::1]:6543/app").unwrap();
        assert_eq!(p.host.as_deref(), Some("::1"));
        assert_eq!(p.port, Some(6543));

        let p = parse("postgres://ada@[::1]/app").unwrap();
        assert_eq!(p.host.as_deref(), Some("::1"));
        assert_eq!(p.port, None);
    }

    #[test]
    fn parses_sqlite_forms() {
        assert_eq!(
            parse("sqlite::memory:").unwrap().database.as_deref(),
            Some(":memory:")
        );
        assert_eq!(
            parse("sqlite:///tmp/app.db").unwrap().database.as_deref(),
            Some("/tmp/app.db")
        );
        assert_eq!(
            parse("sqlite:relative.db").unwrap().database.as_deref(),
            Some("relative.db")
        );
        assert_eq!(parse("sqlite:app.db").unwrap().driver, "sqlite");
    }

    #[test]
    fn rejects_what_it_cannot_understand() {
        assert_eq!(parse("").unwrap_err(), UrlError::Empty);
        assert_eq!(
            parse("db.example/app").unwrap_err(),
            UrlError::MissingScheme
        );
        assert!(matches!(
            parse("oracle://host/db").unwrap_err(),
            UrlError::UnsupportedScheme(_)
        ));
        assert!(matches!(
            parse("postgres://host:notaport/db").unwrap_err(),
            UrlError::BadPort(_)
        ));
        assert!(matches!(
            parse("postgres://host:99999/db").unwrap_err(),
            UrlError::BadPort(_)
        ));
        assert!(matches!(
            parse("postgres://host/db?x=%zz").unwrap_err(),
            UrlError::BadEscape(_)
        ));
    }

    #[test]
    fn a_missing_database_is_allowed() {
        let p = parse("postgres://host").unwrap();
        assert_eq!(p.database, None);
        assert_eq!(p.host.as_deref(), Some("host"));
    }

    #[test]
    fn query_pairs_without_values_are_kept() {
        let p = parse("postgres://host/db?foo&bar=1").unwrap();
        assert_eq!(
            p.params,
            [
                ("foo".to_string(), String::new()),
                ("bar".to_string(), "1".to_string())
            ]
        );
    }
}
