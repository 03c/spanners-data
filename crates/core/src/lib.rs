//! Core domain: what a connection *is*, and what it is allowed to do.

pub mod config;
pub mod executor;
pub mod policy;

pub use config::{AppConfig, ConfigError, ConfigStore, Connection};
pub use executor::{execute_guarded, Gate};
pub use policy::{Classification, Policy, Profile, StmtKind, Verdict, Violation};
