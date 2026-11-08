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
pub mod ident;
pub mod index;
pub mod memory;
pub mod minhash;
pub mod pagerank;
pub mod module;
pub mod parse;
pub mod recall;
pub mod search;
pub mod store;
pub mod verdict;
pub mod vault;
pub mod verify;

pub use chunk::{chunks_for_file, rel_display_path, CHUNK_BUDGET_BYTES};
pub use daemon::{
    classify_watch_result, idle_timeout, is_content_event, run_daemon, should_exit_idle, DaemonLock, WatchMsg, HEARTBEAT_EVERY,
    SERVE_HEARTBEAT,
};
pub use doctor::{diagnose, DiagnosticReport};
pub use embed::{embed_pending, embed_pending_capped, EmbedStats};
pub use index::{
    error_fingerprint, index_one_file, index_path, is_in_excluded_dir, is_in_linked_worktree, is_linked_worktree_below,
    is_supported_source, reconcile_by_stat, should_vacuum, walk_source_files,
    IndexStats,
};
pub use memory::{
    auto_summary_content, curate, Archived, CurateConfig, CurateReport, DuplicatePair, digest_transcript, effective_score, is_l0, parse_stop_input, rank,
    redact_secrets, remove_auto_summary, Redaction, sanitize_fts5, sanitize_nl, session_end, session_start,
    upsert_auto_summary, project_marker, validate_bucket_name, validate_project_name, PROJECT_MARKER, SessionEnd, SessionStart, RenameReport, SqliteMemoryStore, StopHookInput,
    TranscriptDigest, WalStore, DEFAULT_PROFILE_NOTES, DEFAULT_PROJECT_MEMORIES,
};
pub use recall::{code_to_memory, recall, why_this_exists};
pub use search::{find_similar, hybrid_search, rrf_fuse, semantic_search, RRF_K};
pub use store::{SqliteCodeStore, Vec0Index};
pub use verdict::{check_exists, ExistScope};
pub use vault::{Vault, VaultEntry};
pub use verify::hook::{
    classify_command, outcome_from_output, output_tail, parse_post_tool_input, PostToolInput,
    Recognised,
};
pub use verify::{stale_report, NewRun, Run, RunKind, StaleFile, StaleReport, VerifyLog};
