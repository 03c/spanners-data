//! Import connections from another SQL client.
//!
//! The only source today is DBeaver, because that is the tool this one is
//! trying to replace: a client that makes you retype twenty connections is one
//! you try once and uninstall.
//!
//! Two rules shape the mapping:
//!
//! * Never import a password. DBeaver stores them encrypted, and a file that
//!   happens to be readable must not become a credential leak. The first
//!   connect prompts.
//! * Never import a connection into `Local` just because the file did not say
//!   otherwise. A non-loopback host gets `Staging`, so an imported production
//!   database is not writable by default. The CLI prints the profile it chose
//!   so the user can tighten it further.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::config::Connection;
use crate::policy::Profile;

/// One connection that could not be imported, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    pub name: String,
    pub reason: String,
}

/// The result of reading a DBeaver `data-sources.json`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Imported {
    pub connections: Vec<Connection>,
    pub skipped: Vec<Skipped>,
    /// Things the user should know about a connection that *was* imported,
    /// e.g. JDBC parameters that have no equivalent here.
    pub notes: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    #[error("could not read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path} is not DBeaver's data-sources.json: {source}")]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("there is no 'connections' object in this file")]
    NoConnections,
}

/// Read a DBeaver data-sources file.
pub fn dbeaver_from_file(path: impl AsRef<Path>) -> Result<Imported, ImportError> {
    dbeaver_from_file_with(path, None)
}

/// Read a DBeaver data-sources file, optionally forcing one profile.
pub fn dbeaver_from_file_with(
    path: impl AsRef<Path>,
    profile: Option<Profile>,
) -> Result<Imported, ImportError> {
    let path = path.as_ref();
    let text = std::fs::read_to_string(path).map_err(|source| ImportError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    dbeaver_from_json_with(&text, profile).map_err(|e| match e {
        ImportError::Json { source, .. } => ImportError::Json {
            path: path.to_path_buf(),
            source,
        },
        other => other,
    })
}

/// Read DBeaver's JSON. `--profile` overrides the host-based guess.
pub fn dbeaver_from_json_with(
    text: &str,
    profile: Option<Profile>,
) -> Result<Imported, ImportError> {
    let root: Value = serde_json::from_str(text).map_err(|source| ImportError::Json {
        path: PathBuf::new(),
        source,
    })?;
    let entries = root
        .get("connections")
        .and_then(Value::as_object)
        .ok_or(ImportError::NoConnections)?;

    let mut out = Imported::default();
    let mut names: Vec<String> = Vec::new();
    for (key, value) in entries {
        match connection_from_entry(key, value) {
            Ok((mut connection, notes)) => {
                if let Some(profile) = profile {
                    connection.profile = profile;
                }
                // DBeaver allows the same name in two folders; the CLI
                // addresses connections by name, so make it unique rather
                // than dropping one.
                let base = connection.name.clone();
                let mut n = 2;
                while names
                    .iter()
                    .any(|n| n.eq_ignore_ascii_case(&connection.name))
                {
                    connection.name = format!("{base} ({n})");
                    n += 1;
                }
                if let Err(reason) = connection.validate() {
                    out.skipped.push(Skipped { name: base, reason });
                    continue;
                }
                names.push(connection.name.clone());
                out.notes.extend(notes);
                out.connections.push(connection);
            }
            Err(reason) => out.skipped.push(Skipped {
                name: value
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(key)
                    .to_string(),
                reason,
            }),
        }
    }
    Ok(Imported {
        connections: sorted_by_name(out.connections),
        skipped: out.skipped,
        notes: out.notes,
    })
}

/// DBeaver stores connections in a JSON object, whose key order is not the
/// order a user sees in its sidebar. Sorting by name makes the CLI's output
/// stable and puts related connections together.
fn sorted_by_name(mut connections: Vec<Connection>) -> Vec<Connection> {
    connections.sort_by(|a, b| {
        a.name
            .to_ascii_lowercase()
            .cmp(&b.name.to_ascii_lowercase())
            .then_with(|| a.name.cmp(&b.name))
    });
    connections
}

/// [`dbeaver_from_json_with`] without a profile override.
pub fn dbeaver_from_json(text: &str) -> Result<Imported, ImportError> {
    dbeaver_from_json_with(text, None)
}

fn connection_from_entry(key: &str, value: &Value) -> Result<(Connection, Vec<String>), String> {
    let provider = value
        .get("provider")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    let driver = match provider.as_str() {
        "postgresql" | "postgres" => "postgres",
        "mysql" | "mariadb" => "mysql",
        "sqlite" => "sqlite",
        "" => return Err("the entry has no provider".into()),
        other => return Err(format!("unsupported provider '{other}'")),
    };

    let cfg = value.get("configuration").cloned().unwrap_or(Value::Null);
    let name = value
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| key.to_string());

    let jdbc_url = cfg.get("url").and_then(Value::as_str);
    let mut notes = Vec::new();

    let mut connection = Connection::new(name.clone(), driver, Profile::Local);

    if driver == "sqlite" {
        let path = sqlite_path(&cfg, jdbc_url)
            .ok_or_else(|| "the SQLite entry has no file path".to_string())?;
        connection.database = Some(path);
        connection.profile = Profile::Local;
        return Ok((connection, notes));
    }

    // Prefer DBeaver's discrete fields, then fall back to its JDBC URL for
    // whatever is missing. A URL is sometimes the only place the host lives.
    let mut host = string_field(&cfg, "host");
    let mut port = port_field(&cfg, "port");
    let mut database = string_field(&cfg, "database");
    let mut username = string_field(&cfg, "user").or_else(|| string_field(&cfg, "username"));
    let mut params: Vec<(String, String)> = Vec::new();

    if let Some(url) = jdbc_url {
        let stripped = url.strip_prefix("jdbc:").unwrap_or(url);
        match crate::url::parse(stripped) {
            Ok(parsed) => {
                host = host.or(parsed.host);
                port = port.or(parsed.port);
                database = database.or(parsed.database);
                username = username.or(parsed.username);
                let (kept, dropped) = split_params(parsed.params);
                params = kept;
                if !dropped.is_empty() {
                    notes.push(format!(
                        "{name}: ignored JDBC parameters with no equivalent ({})",
                        dropped.join(", ")
                    ));
                }
            }
            Err(e) => notes.push(format!("{name}: could not read its JDBC URL ({e})")),
        }
    }

    // DBeaver keeps driver properties in a map, separate from the URL.
    if let Some(props) = cfg.get("properties").and_then(Value::as_object) {
        let mut dropped = Vec::new();
        for (key, value) in props {
            if let Some(value) = value.as_str() {
                let lower = key.to_ascii_lowercase();
                if lower == "sslmode" || lower == "ssl-mode" {
                    params.push(("sslmode".into(), value.to_string()));
                } else {
                    dropped.push(key.clone());
                }
            }
        }
        if !dropped.is_empty() {
            notes.push(format!(
                "{name}: ignored DBeaver properties with no equivalent ({})",
                dropped.join(", ")
            ));
        }
    }

    connection.host = host;
    connection.port = port;
    connection.database = database;
    connection.username = username;
    connection.params = params;
    // A loopback host is the one case where "anything goes" is a safe guess.
    connection.profile = if is_local_host(connection.host.as_deref()) {
        Profile::Local
    } else {
        Profile::Staging
    };
    Ok((connection, notes))
}

/// DBeaver writes `jdbc:sqlite:/path/db.sqlite`, sometimes with a query string.
fn sqlite_path(cfg: &Value, jdbc_url: Option<&str>) -> Option<String> {
    let raw = jdbc_url
        .map(|u| u.strip_prefix("jdbc:sqlite:").unwrap_or(u))
        .map(|u| u.split('?').next().unwrap_or(u))
        .map(str::to_string)
        .or_else(|| string_field(cfg, "database"))
        .or_else(|| string_field(cfg, "path"))?;
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    // `jdbc:sqlite::memory:` and `jdbc:sqlite:file:...` survive as-is; a
    // `///abs/path` from a URI-style URL loses the authority slashes.
    if raw.starts_with(":memory:") || raw.starts_with("file:") {
        return Some(raw.to_string());
    }
    Some(raw.trim_start_matches("//").to_string())
}

/// Keep only the parameters the drivers understand; report the rest.
fn split_params(params: Vec<(String, String)>) -> (Vec<(String, String)>, Vec<String>) {
    let mut kept = Vec::new();
    let mut dropped = Vec::new();
    for (key, value) in params {
        match key.to_ascii_lowercase().as_str() {
            "sslmode" | "ssl-mode" => kept.push(("sslmode".to_string(), value)),
            "application_name" | "options" => kept.push((key, value)),
            _ => dropped.push(key),
        }
    }
    (kept, dropped)
}

fn string_field(value: &Value, key: &str) -> Option<String> {
    match value.get(key)? {
        Value::String(s) if !s.trim().is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn port_field(value: &Value, key: &str) -> Option<u16> {
    match value.get(key)? {
        Value::Number(n) => n.as_u64().and_then(|n| u16::try_from(n).ok()),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn is_local_host(host: Option<&str>) -> bool {
    match host {
        None => true,
        Some(h) => matches!(
            h.to_ascii_lowercase().as_str(),
            "localhost" | "127.0.0.1" | "::1" | "[::1]"
        ),
    }
}

/// Places DBeaver keeps `data-sources.json`, newest layout first.
///
/// It is a search rather than a fixed path because the workspace directory is
/// versioned (`workspace6`) and has moved between releases.
pub fn dbeaver_candidates() -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Some(home) = directories::UserDirs::new().map(|u| u.home_dir().to_path_buf()) {
        roots.push(home.join(".local").join("share"));
        roots.push(home.join("Library").join("Application Support"));
        roots.push(home.join(".dbeaver"));
    }
    if let Some(appdata) = std::env::var_os("APPDATA") {
        roots.push(PathBuf::from(appdata));
    }

    let mut out = Vec::new();
    for root in roots {
        // Old layout: ~/.dbeaver/data-sources.json
        let legacy = root.join("data-sources.json");
        if legacy.is_file() {
            out.push(legacy);
        }
        // New layout: <root>/DBeaverData/workspace*/General/.dbeaver/data-sources.json
        let data = root.join("DBeaverData");
        let Ok(entries) = std::fs::read_dir(&data) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if !name.starts_with("workspace") {
                continue;
            }
            for candidate in [
                path.join("General")
                    .join(".dbeaver")
                    .join("data-sources.json"),
                path.join(".dbeaver").join("data-sources.json"),
            ] {
                if candidate.is_file() {
                    out.push(candidate);
                }
            }
        }
    }
    out
}

/// The first existing DBeaver file, if any.
pub fn find_dbeaver_file() -> Option<PathBuf> {
    dbeaver_candidates().into_iter().next()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
    {
      "folders": {},
      "connections": {
        "postgres-jdbc-1": {
          "provider": "postgresql",
          "driver": "postgres-jdbc",
          "name": "app dev",
          "configuration": {
            "host": "localhost",
            "port": 5432,
            "database": "app",
            "user": "me",
            "url": "jdbc:postgresql://localhost:5432/app"
          }
        },
        "mysql-2": {
          "provider": "mysql",
          "driver": "mysql5",
          "name": "reporting",
          "configuration": {
            "host": "db.internal",
            "port": "3306",
            "database": "reports",
            "user": "analyst",
            "url": "jdbc:mysql://db.internal:3306/reports?useSSL=false&serverTimezone=UTC"
          }
        },
        "sqlite-3": {
          "provider": "sqlite",
          "driver": "sqlite_jdbc",
          "name": "scratch",
          "configuration": {
            "url": "jdbc:sqlite:/home/me/scratch.db"
          }
        },
        "oracle-4": {
          "provider": "oracle",
          "name": "legacy",
          "configuration": {}
        },
        "broken-5": {
          "provider": "sqlite",
          "name": "nowhere",
          "configuration": {}
        }
      }
    }
    "#;

    #[test]
    fn maps_providers_and_skips_the_rest() {
        let imported = dbeaver_from_json(SAMPLE).unwrap();
        let names: Vec<&str> = imported
            .connections
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        assert_eq!(names, ["app dev", "reporting", "scratch"]);

        let pg = &imported.connections[0];
        assert_eq!(pg.driver, "postgres");
        assert_eq!(pg.host.as_deref(), Some("localhost"));
        assert_eq!(pg.port, Some(5432));
        assert_eq!(pg.database.as_deref(), Some("app"));
        assert_eq!(pg.username.as_deref(), Some("me"));

        assert_eq!(imported.skipped.len(), 2);
        assert!(imported
            .skipped
            .iter()
            .any(|s| s.name == "legacy" && s.reason.contains("oracle")));
        assert!(imported
            .skipped
            .iter()
            .any(|s| s.name == "nowhere" && s.reason.contains("path")));
    }

    #[test]
    fn a_loopback_host_is_local_and_anything_else_is_staging() {
        let imported = dbeaver_from_json(SAMPLE).unwrap();
        assert_eq!(imported.connections[0].profile, Profile::Local);
        // A database on a network host is not a scratch database.
        assert_eq!(imported.connections[1].profile, Profile::Staging);
        assert_eq!(imported.connections[2].profile, Profile::Local);
    }

    #[test]
    fn an_explicit_profile_overrides_the_guess() {
        let imported = dbeaver_from_json_with(SAMPLE, Some(Profile::Production)).unwrap();
        assert!(imported
            .connections
            .iter()
            .all(|c| c.profile == Profile::Production));
    }

    #[test]
    fn jdbc_parameters_with_no_equivalent_are_dropped_and_reported() {
        let imported = dbeaver_from_json(SAMPLE).unwrap();
        let mysql = &imported.connections[1];
        // `useSSL` and `serverTimezone` are JDBC-ism, not driver options; the
        // drivers refuse parameters they do not understand.
        assert!(mysql.params.is_empty(), "{:?}", mysql.params);
        assert!(
            imported
                .notes
                .iter()
                .any(|n| n.contains("reporting") && n.contains("useSSL")),
            "{:?}",
            imported.notes
        );
    }

    #[test]
    fn sslmode_survives_and_is_normalised() {
        let json = r#"{"connections":{"x":{"provider":"postgresql","name":"live",
            "configuration":{"host":"db.example.com","port":5432,"database":"app",
            "url":"jdbc:postgresql://db.example.com:5432/app?sslmode=require"}}}}"#;
        let imported = dbeaver_from_json(json).unwrap();
        assert_eq!(
            imported.connections[0].params,
            [("sslmode".to_string(), "require".to_string())]
        );
    }

    #[test]
    fn a_sqlite_url_becomes_a_path_without_its_query() {
        let json = r#"{"connections":{"x":{"provider":"sqlite","name":"s",
            "configuration":{"url":"jdbc:sqlite:/tmp/a.db?mode=rw"}}}}"#;
        let imported = dbeaver_from_json(json).unwrap();
        assert_eq!(
            imported.connections[0].database.as_deref(),
            Some("/tmp/a.db")
        );
    }

    #[test]
    fn duplicate_names_are_made_unique_rather_than_dropped() {
        let json = r#"{"connections":{
            "a":{"provider":"sqlite","name":"same","configuration":{"url":"jdbc:sqlite:/a.db"}},
            "b":{"provider":"sqlite","name":"same","configuration":{"url":"jdbc:sqlite:/b.db"}}
        }}"#;
        let imported = dbeaver_from_json(json).unwrap();
        let names: Vec<&str> = imported
            .connections
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        assert_eq!(names, ["same", "same (2)"]);
    }

    #[test]
    fn a_missing_connections_object_is_an_error_not_an_empty_import() {
        assert!(matches!(
            dbeaver_from_json(r#"{"folders":{}}"#),
            Err(ImportError::NoConnections)
        ));
        assert!(matches!(
            dbeaver_from_json("not json"),
            Err(ImportError::Json { .. })
        ));
    }
}
