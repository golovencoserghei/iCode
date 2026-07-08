//! `icode-engine` — parse / index / store / memory / search.
//!
//! M0.5 walking skeleton: `store` (SqliteCodeStore), `parse` (Rust functions),
//! and `index` (walk + upsert) are implemented against the frozen `icode-core`
//! contracts. Embeddings (M2), full parsers/OOP/graph (M3), memory (M4), and
//! recall (M5) fill in later.

pub mod chunk;
pub mod daemon;
pub mod doctor;
pub mod embed;
pub mod index;
pub mod memory;
pub mod parse;
pub mod recall;
pub mod search;
pub mod store;
pub mod verdict;

pub use chunk::{chunks_for_file, rel_display_path, CHUNK_BUDGET_BYTES};
pub use daemon::{run_daemon, DaemonLock};
pub use doctor::{diagnose, DiagnosticReport};
pub use embed::{embed_pending, EmbedStats};
pub use index::{
    index_one_file, index_path, is_in_excluded_dir, is_supported_source, walk_source_files,
    IndexStats,
};
pub use memory::{
    auto_summary_content, digest_transcript, effective_score, is_l0, parse_stop_input, rank,
    remove_auto_summary, sanitize_fts5, sanitize_nl, session_end, session_start,
    upsert_auto_summary, SessionEnd, SessionStart, SqliteMemoryStore, StopHookInput,
    TranscriptDigest, WalStore, DEFAULT_PROFILE_NOTES, DEFAULT_PROJECT_MEMORIES,
};
pub use recall::{code_to_memory, recall, why_this_exists};
pub use search::{find_similar, hybrid_search, rrf_fuse, semantic_search, RRF_K};
pub use store::{SqliteCodeStore, Vec0Index};
pub use verdict::{check_exists, ExistScope};
