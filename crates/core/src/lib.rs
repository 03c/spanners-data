//! Core domain: what a connection *is*, and what it is allowed to do.

pub mod config;
pub mod dialect;
pub mod executor;
pub mod export;
pub mod history;
pub mod policy;

pub use config::{AppConfig, ConfigError, ConfigStore, Connection};
pub use executor::{execute_guarded, Gate};
pub use export::{export, Buffer, ExportError, ExportOpts, ExportSummary, Format, NullRepr};
pub use history::{
    Folder, HistoryEntry, HitKind, Library, SavedQuery, SearchHit, Store, StoreError,
};
pub use policy::{Classification, Policy, Profile, StmtKind, Verdict, Violation};
