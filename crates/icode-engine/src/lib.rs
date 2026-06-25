//! `icode-engine` — parse / index / store / memory / search.
//!
//! M0.5 walking skeleton: `store` (SqliteCodeStore), `parse` (Rust functions),
//! and `index` (walk + upsert) are implemented against the frozen `icode-core`
//! contracts. Embeddings (M2), full parsers/OOP/graph (M3), memory (M4), and
//! recall (M5) fill in later.

pub mod chunk;
pub mod doctor;
pub mod embed;
pub mod index;
pub mod memory;
pub mod parse;
pub mod recall;
pub mod search;
pub mod store;

pub use chunk::{chunk_text_for_class, chunk_text_for_function, chunks_for_file, CHUNK_BUDGET_BYTES};
pub use doctor::{diagnose, DiagnosticReport};
pub use embed::{embed_pending, EmbedStats};
pub use index::{index_path, walk_source_files, IndexStats};
pub use memory::{
    effective_score, is_l0, rank, sanitize_fts5, sanitize_nl, session_end, session_start,
    SessionEnd, SessionStart, SqliteMemoryStore, WalStore, DEFAULT_PROFILE_NOTES,
    DEFAULT_PROJECT_MEMORIES,
};
pub use recall::{code_to_memory, recall, why_this_exists};
pub use search::{find_similar, hybrid_search, rrf_fuse, semantic_search, RRF_K};
pub use store::{SqliteCodeStore, Vec0Index};
