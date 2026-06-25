//! Stable identifiers. Memory ids are project-encoded + ULID so the project is
//! recoverable from the id (O(1) routing, like meMCP's EncodedIdStrategy) and
//! ids sort by creation time. Code-chunk ids are plain integer rowids (owned by
//! the per-project store), so they live in the store layer, not here.

use serde::{Deserialize, Serialize};
use ulid::Ulid;

/// Reserved project name holding the cross-project developer profile.
/// Excluded from `list_projects` and normal listings by the namespace guard.
pub const DEVELOPER_PROJECT: &str = "__developer__";

/// Prefix marking reserved/internal projects (namespace guard).
pub const RESERVED_PREFIX: &str = "__";

/// Returns true if `project` is a reserved/internal namespace.
pub fn is_reserved_project(project: &str) -> bool {
    project.starts_with(RESERVED_PREFIX)
}

/// A memory id of the form `mem_<ulid>__<project>`.
///
/// The ULID comes first so lexical order ~= creation order; the project suffix
/// makes the owning project recoverable without a table lookup.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MemoryId(pub String);

impl MemoryId {
    /// Generate a fresh id for `project`.
    pub fn generate(project: &str) -> Self {
        MemoryId(format!("mem_{}__{}", Ulid::new(), project))
    }

    /// Recover the owning project from the id, if encoded.
    pub fn project(&self) -> Option<&str> {
        self.0.split_once("__").map(|(_, p)| p)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for MemoryId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for MemoryId {
    fn from(s: String) -> Self {
        MemoryId(s)
    }
}
