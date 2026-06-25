//! `icode-engine` — parse / index / store / memory / search.
//!
//! M0.5 walking skeleton: `store` (SqliteCodeStore), `parse` (Rust functions),
//! and `index` (walk + upsert) are implemented against the frozen `icode-core`
//! contracts. Embeddings (M2), full parsers/OOP/graph (M3), memory (M4), and
//! recall (M5) fill in later.

pub mod chunk;
pub mod embed;
pub mod index;
pub mod parse;
pub mod store;
// pub mod search;  // M2
// pub mod memory;  // M4

pub use chunk::{chunks_for_file, CHUNK_BUDGET_BYTES};
pub use embed::{embed_pending, EmbedStats};
pub use index::{index_path, IndexStats};
pub use store::{SqliteCodeStore, Vec0Index};
