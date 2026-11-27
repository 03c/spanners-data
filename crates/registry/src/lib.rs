//! The driver "plugin" registry.
//!
//! Drivers are compile-time plugins: adding a database means adding a crate
//! that implements [`Driver`] and one line in [`Registry::default_registry`].
//! That keeps the 90% benefit of plugins (a stable boundary, independent
//! crates, independent tests) without a dynamic ABI, which Rust does not
//! provide safely.
//!
//! If drivers ever need to load at runtime, the boundary already only uses
//! `serde`-friendly types, so an out-of-process host can be added without
//! touching driver code.

use std::collections::BTreeMap;
use std::sync::Arc;

use spanners_driver_api::Driver;

#[derive(Default)]
pub struct Registry {
    drivers: BTreeMap<&'static str, Arc<dyn Driver>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Every driver the app can build. Ordering is stable, which keeps menus
    /// and error messages deterministic.
    pub fn default_registry() -> Self {
        let mut r = Self::new();
        r.register(Arc::new(spanners_driver_sqlite::SqliteDriver::new()));
        r.register(Arc::new(spanners_driver_postgres::PostgresDriver::new()));
        r.register(Arc::new(spanners_driver_mysql::MySqlDriver::new()));
        r
    }

    pub fn register(&mut self, driver: Arc<dyn Driver>) {
        self.drivers.insert(driver.id(), driver);
    }

    pub fn get(&self, id: &str) -> Option<Arc<dyn Driver>> {
        self.drivers.get(id).cloned()
    }

    pub fn ids(&self) -> Vec<&'static str> {
        self.drivers.keys().copied().collect()
    }

    pub fn all(&self) -> Vec<Arc<dyn Driver>> {
        self.drivers.values().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spanners_driver_api::{ConnConfig, Dialect, Result, Session};

    struct Fake;

    #[async_trait::async_trait]
    impl Driver for Fake {
        fn id(&self) -> &'static str {
            "fake"
        }
        fn label(&self) -> &'static str {
            "Fake"
        }
        fn dialect(&self) -> Dialect {
            Dialect::Postgres
        }
        fn default_port(&self) -> Option<u16> {
            Some(1234)
        }
        fn default_database(&self, _: &ConnConfig) -> String {
            "fake".into()
        }
        async fn connect(
            &self,
            _: &ConnConfig,
            _: spanners_driver_api::SessionOpts,
        ) -> Result<Box<dyn Session>> {
            Err(spanners_driver_api::DriverError::Unsupported("test".into()))
        }
    }

    #[test]
    fn ships_with_every_built_in_driver() {
        let r = Registry::default_registry();
        assert_eq!(r.ids(), ["mysql", "postgres", "sqlite"]);
    }

    #[test]
    fn drivers_report_their_own_defaults() {
        let r = Registry::default_registry();
        assert_eq!(r.get("postgres").unwrap().default_port(), Some(5432));
        assert_eq!(r.get("mysql").unwrap().default_port(), Some(3306));
        assert_eq!(r.get("sqlite").unwrap().default_port(), None);
    }

    #[test]
    fn a_new_driver_is_one_register_call() {
        let mut r = Registry::new();
        r.register(Arc::new(Fake));
        assert_eq!(r.get("fake").unwrap().label(), "Fake");
    }
}
