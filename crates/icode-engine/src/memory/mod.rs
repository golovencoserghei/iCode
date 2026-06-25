//! Cross-session memory: the central store (`~/.icode/icode.db`) and its M4 base
//! `SqliteMemoryStore` (the bottom of the `MemoryStore` decorator chain).
//!
//! Distinct from `store` (the per-project, disposable code-graph db): this db is
//! durable and cross-project. It reuses the per-process sqlite-vec registration
//! (`store::register_sqlite_vec`) and the vec0 f32-LE/cosine conventions, but
//! bridges vec0's INTEGER rowid to the TEXT memory id via `mem_rowid`, and keeps a
//! STANDALONE fts5 index (not external-content) since the owning id is textual.

pub mod ranking;
pub mod sanitizer;
mod schema;
mod store;
mod wal;

pub use ranking::{effective_score, is_l0, rank};
pub use sanitizer::{sanitize_fts5, sanitize_nl};
pub use store::SqliteMemoryStore;
pub use wal::WalStore;
