//! Connection definitions and their on-disk representation.
//!
//! Config holds connection *metadata* only. Passwords live in the OS keychain;
//! if you ever see a password in this file it is a bug.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use spanners_driver_api::{ConnConfig, Dialect};

use crate::policy::{Policy, PolicyOverrides, Profile};

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("could not read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not write {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid config: {0}")]
    Parse(String),
    #[error("could not serialise config: {0}")]
    Serialize(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AppConfig {
    pub schema_version: u32,
    pub connections: Vec<Connection>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            connections: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Connection {
    pub id: String,
    pub name: String,
    /// Driver id: `"postgres"`, `"mysql"`, `"sqlite"`.
    pub driver: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// Database name, or a file path for SQLite.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    pub profile: Profile,
    #[serde(default)]
    pub overrides: PolicyOverrides,
}

impl Connection {
    pub fn new(name: impl Into<String>, driver: impl Into<String>, profile: Profile) -> Self {
        let driver = driver.into();
        Self {
            id: new_id(),
            name: name.into(),
            driver,
            host: None,
            port: None,
            database: None,
            username: None,
            profile,
            overrides: PolicyOverrides::default(),
        }
    }

    pub fn dialect(&self) -> Option<Dialect> {
        match self.driver.as_str() {
            "postgres" => Some(Dialect::Postgres),
            "mysql" => Some(Dialect::MySql),
            "sqlite" => Some(Dialect::Sqlite),
            _ => None,
        }
    }

    pub fn policy(&self) -> Policy {
        Policy::preset(self.profile).with_overrides(&self.overrides)
    }

    pub fn is_embedded(&self) -> bool {
        self.driver == "sqlite"
    }

    /// Runtime connection parameters. The password is supplied by the caller
    /// from the keychain, or by a prompt when no keychain is available; it is
    /// never read from the persisted [`Connection`].
    pub fn to_conn_config(&self, password: Option<String>) -> ConnConfig {
        ConnConfig {
            driver: self.driver.clone(),
            host: self.host.clone(),
            port: self.port,
            // SQLite keeps its file path in `database`; drivers treat it as such.
            database: self.database.clone(),
            username: self.username.clone(),
            password,
            params: Vec::new(),
        }
    }
}

/// Replace anything secret in a connection string before it reaches a log.
///
/// Deliberately conservative: passwords in the authority and any `password=`
/// query parameter are replaced, but the rest of the string is left alone so
/// the log still says which host failed.
pub fn redact_dsn(input: &str) -> String {
    let mut out = String::with_capacity(input.len());

    // Credentials live between "scheme://" and the first '/' or '?' after it.
    let authority_start = input.find("://").map(|i| i + 3);
    match authority_start {
        Some(start) => {
            let rest = &input[start..];
            let end = rest
                .find(['/', '?'])
                .map(|i| start + i)
                .unwrap_or(input.len());
            let authority = &input[start..end];
            out.push_str(&input[..start]);
            match authority.rsplit_once('@') {
                Some((creds, host)) => {
                    match creds.split_once(':') {
                        Some((user, _password)) => {
                            out.push_str(user);
                            out.push_str(":***@");
                        }
                        // No colon means no password to hide.
                        None => {
                            out.push_str(creds);
                            out.push('@');
                        }
                    }
                    out.push_str(host);
                }
                None => out.push_str(authority),
            }
            out.push_str(&input[end..]);
        }
        None => out.push_str(input),
    }

    redact_password_params(&out)
}

fn redact_password_params(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(pos) = rest.to_ascii_lowercase().find("password=") {
        let (head, tail) = rest.split_at(pos + "password=".len());
        out.push_str(head);
        // Value ends at the next separator or the end of the string.
        let value_end = tail.find(['&', ';', ' ']).unwrap_or(tail.len());
        out.push_str("***");
        rest = &tail[value_end..];
    }
    out.push_str(rest);
    out
}

/// Ids are opaque and stable; they key the keychain entry and the history rows,
/// so they must never be reused.
fn new_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("c{nanos:x}{n:x}")
}

#[derive(Debug, Clone)]
pub struct ConfigStore {
    path: PathBuf,
}

impl ConfigStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// `~/.config/dev.spanners.data/config.toml` (or the platform equivalent).
    pub fn default_path() -> Option<PathBuf> {
        directories::ProjectDirs::from("dev", "spanners", "data")
            .map(|d| d.config_dir().join("config.toml"))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// A missing file is a first run, not an error.
    pub fn load(&self) -> Result<AppConfig, ConfigError> {
        match fs::read_to_string(&self.path) {
            Ok(text) => toml::from_str(&text).map_err(|e| ConfigError::Parse(e.to_string())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(AppConfig::default()),
            Err(e) => Err(ConfigError::Read {
                path: self.path.clone(),
                source: e,
            }),
        }
    }

    /// Atomic: write a sibling temp file, fsync, then rename over the target.
    /// A crash mid-write leaves the previous config intact.
    pub fn save(&self, cfg: &AppConfig) -> Result<(), ConfigError> {
        let text =
            toml::to_string_pretty(cfg).map_err(|e| ConfigError::Serialize(e.to_string()))?;

        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|e| ConfigError::Write {
                path: parent.to_path_buf(),
                source: e,
            })?;
        }

        let tmp = self.path.with_extension("toml.tmp");
        {
            let mut f = fs::File::create(&tmp).map_err(|e| ConfigError::Write {
                path: tmp.clone(),
                source: e,
            })?;
            f.write_all(text.as_bytes())
                .and_then(|_| f.sync_all())
                .map_err(|e| ConfigError::Write {
                    path: tmp.clone(),
                    source: e,
                })?;
        }
        fs::rename(&tmp, &self.path).map_err(|e| ConfigError::Write {
            path: self.path.clone(),
            source: e,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spanners_driver_api::Dialect;
    use tempfile::TempDir;

    fn store(dir: &TempDir) -> ConfigStore {
        ConfigStore::new(dir.path().join("nested/config.toml"))
    }

    #[test]
    fn a_password_never_leaks_into_the_persisted_config() {
        let c = Connection::new("live", "postgres", Profile::Production);
        let runtime = c.to_conn_config(Some("hunter2".into()));
        assert_eq!(runtime.password.as_deref(), Some("hunter2"));
        assert_eq!(c.dialect(), Some(Dialect::Postgres));

        let toml = toml::to_string(&c).unwrap();
        assert!(!toml.contains("hunter2"));
    }

    #[test]
    fn redacts_passwords_in_dsns() {
        assert_eq!(
            redact_dsn("postgres://ada:hunter2@db.example:5432/app"),
            "postgres://ada:***@db.example:5432/app"
        );
        assert_eq!(
            redact_dsn("mysql://root:secret@127.0.0.1:3306/spanners?ssl=true"),
            "mysql://root:***@127.0.0.1:3306/spanners?ssl=true"
        );
        assert_eq!(
            redact_dsn("postgres://db.example/app?user=ada&password=hunter2&sslmode=require"),
            "postgres://db.example/app?user=ada&password=***&sslmode=require"
        );
        // Nothing to hide, nothing changed.
        assert_eq!(redact_dsn("sqlite:///tmp/app.db"), "sqlite:///tmp/app.db");
        assert_eq!(
            redact_dsn("postgres://db.example/app"),
            "postgres://db.example/app"
        );
    }

    #[test]
    fn first_run_is_not_an_error() {
        let dir = TempDir::new().unwrap();
        let cfg = store(&dir).load().unwrap();
        assert_eq!(cfg.schema_version, SCHEMA_VERSION);
        assert!(cfg.connections.is_empty());
    }

    #[test]
    fn round_trips_through_disk() {
        let dir = TempDir::new().unwrap();
        let s = store(&dir);

        let mut c = Connection::new("prod-eu", "postgres", Profile::Production);
        c.host = Some("db.eu.example".into());
        c.port = Some(5432);
        c.database = Some("app".into());
        c.username = Some("reader".into());
        c.overrides.max_rows = Some(50);
        let cfg = AppConfig {
            schema_version: SCHEMA_VERSION,
            connections: vec![c.clone()],
        };

        s.save(&cfg).unwrap();
        assert_eq!(s.load().unwrap(), cfg);
        assert!(!s.path().with_extension("toml.tmp").exists());
    }

    #[test]
    fn policy_preset_and_override_compose() {
        let mut c = Connection::new("live", "postgres", Profile::Production);
        c.overrides.max_rows = Some(50);
        let p = c.policy();
        assert!(p.read_only);
        assert_eq!(p.max_rows, Some(50));
        assert_eq!(p.timeout, Some(std::time::Duration::from_secs(15)));
    }

    #[test]
    fn zero_means_unlimited() {
        let mut c = Connection::new("local", "sqlite", Profile::Local);
        c.overrides.max_rows = Some(0);
        assert_eq!(c.policy().max_rows, None);
    }

    #[test]
    fn unknown_driver_has_no_dialect() {
        let c = Connection::new("x", "oracle", Profile::Local);
        assert!(c.dialect().is_none());
    }

    #[test]
    fn corrupt_file_is_reported_not_ignored() {
        let dir = TempDir::new().unwrap();
        let s = store(&dir);
        std::fs::create_dir_all(s.path().parent().unwrap()).unwrap();
        std::fs::write(s.path(), "this is not toml [").unwrap();
        assert!(matches!(s.load(), Err(ConfigError::Parse(_))));
    }
}
