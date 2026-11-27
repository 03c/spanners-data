//! Where connection passwords live.
//!
//! The contract that matters: **secrets are never written to disk in plaintext
//! and a missing keychain never blocks a connection.** If the OS store is
//! unavailable (headless Linux, locked keyring, CI), the app still connects —
//! the user just gets asked for the password again next time.

use std::collections::HashMap;
use std::sync::Mutex;

#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    #[error("secret store unavailable: {0}")]
    Unavailable(String),
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, SecretError>;

/// Keyed by connection id. Connection ids are never reused, so a deleted
/// connection can never inherit a stale password.
pub trait Secrets: Send + Sync {
    fn get(&self, connection_id: &str) -> Result<Option<String>>;
    fn set(&self, connection_id: &str, password: &str) -> Result<()>;
    fn delete(&self, connection_id: &str) -> Result<()>;
}

/// Process-lifetime store. Used by tests, and by the app when no OS keychain is
/// reachable: better to hold the password in memory for this session than to
/// refuse to connect or to write it to disk.
#[derive(Debug, Default)]
pub struct MemorySecrets {
    entries: Mutex<HashMap<String, String>>,
}

impl MemorySecrets {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Secrets for MemorySecrets {
    fn get(&self, connection_id: &str) -> Result<Option<String>> {
        Ok(self
            .entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(connection_id)
            .cloned())
    }

    fn set(&self, connection_id: &str, password: &str) -> Result<()> {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(connection_id.to_string(), password.to_string());
        Ok(())
    }

    fn delete(&self, connection_id: &str) -> Result<()> {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(connection_id);
        Ok(())
    }
}

/// OS keychain, namespaced under the app id.
#[cfg(feature = "os-keyring")]
pub struct KeyringSecrets {
    service: String,
}

#[cfg(feature = "os-keyring")]
impl KeyringSecrets {
    pub fn new() -> Self {
        Self {
            service: "dev.spanners.data".into(),
        }
    }

    fn entry(&self, connection_id: &str) -> Result<keyring::Entry> {
        keyring::Entry::new(&self.service, connection_id)
            .map_err(|e| SecretError::Unavailable(e.to_string()))
    }
}

#[cfg(feature = "os-keyring")]
impl Default for KeyringSecrets {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "os-keyring")]
impl Secrets for KeyringSecrets {
    fn get(&self, connection_id: &str) -> Result<Option<String>> {
        match self.entry(connection_id)?.get_password() {
            Ok(password) => Ok(Some(password)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(SecretError::Other(e.to_string())),
        }
    }

    fn set(&self, connection_id: &str, password: &str) -> Result<()> {
        self.entry(connection_id)?
            .set_password(password)
            .map_err(|e| SecretError::Other(e.to_string()))
    }

    fn delete(&self, connection_id: &str) -> Result<()> {
        match self.entry(connection_id)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(SecretError::Other(e.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passwords_round_trip_per_connection() {
        let s = MemorySecrets::new();
        s.set("c1", "one").unwrap();
        s.set("c2", "two").unwrap();
        assert_eq!(s.get("c1").unwrap().as_deref(), Some("one"));
        assert_eq!(s.get("c2").unwrap().as_deref(), Some("two"));
        assert_eq!(s.get("missing").unwrap(), None);
    }

    #[test]
    fn deleting_a_connection_forgets_its_password() {
        let s = MemorySecrets::new();
        s.set("c1", "one").unwrap();
        s.delete("c1").unwrap();
        assert_eq!(s.get("c1").unwrap(), None);
        // Deleting something that was never stored is not an error.
        s.delete("c1").unwrap();
    }
}
