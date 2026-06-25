//! `icode-core` — the framework-free, acyclic center of iCode v2.
//!
//! Holds the domain vocabulary, configuration, and the FROZEN trait contracts
//! every other crate implements or consumes. Depends on no I/O framework
//! (no rusqlite/rmcp/reqwest), so it can be reasoned about and tested in
//! isolation and never participates in a dependency cycle.

pub mod config;
pub mod error;
pub mod ids;
pub mod model;
pub mod traits;

pub use config::IcodeConfig;
pub use error::{Error, Result};
pub use ids::MemoryId;

/// The vocabulary most consumers want in one `use`.
pub mod prelude {
    pub use crate::config::*;
    pub use crate::error::{Error, Result};
    pub use crate::ids::{is_reserved_project, MemoryId, DEVELOPER_PROJECT};
    pub use crate::model::*;
    pub use crate::traits::*;
}
