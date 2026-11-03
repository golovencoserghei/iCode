//! Cross-session memory: the central store (`~/.icode/icode.db`) and its M4 base
//! `SqliteMemoryStore` (the bottom of the `MemoryStore` decorator chain).
//!
//! Distinct from `store` (the per-project, disposable code-graph db): this db is
//! durable and cross-project. It reuses the per-process sqlite-vec registration
//! (`store::register_sqlite_vec`) and the vec0 f32-LE/cosine conventions, but
//! bridges vec0's INTEGER rowid to the TEXT memory id via `mem_rowid`, and keeps a
//! STANDALONE fts5 index (not external-content) since the owning id is textual.

pub mod curate;
pub mod ranking;
pub mod redact;
pub mod sanitizer;
mod schema;
pub mod session;
pub mod stop_hook;
mod store;
mod wal;

/// File at a repo's working-tree root that names the memory bucket the repo shares
/// with its siblings (e.g. several repos forming ONE product ecosystem). Without it a
/// repo's bucket is its directory name. First non-empty, non-`#` line wins.
pub const PROJECT_MARKER: &str = ".icode-project";

/// The shared bucket name declared by `<root>/.icode-project`, if any. Names with a
/// path separator, control characters or more than 64 chars are ignored (a malformed
/// marker must degrade to the directory name, never to a junk bucket).
pub fn project_marker(root: &std::path::Path) -> Option<String> {
    let text = std::fs::read_to_string(root.join(PROJECT_MARKER)).ok()?;
    let name = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('#'))?;
    let ok = name.chars().count() <= 64 && !name.chars().any(|c| c.is_control() || c == '/' || c == '\\');
    ok.then(|| name.to_string())
}

pub use curate::{curate, Archived, CurateConfig, CurateReport, DuplicatePair};
pub use ranking::{effective_score, is_l0, rank};
pub use redact::{redact_secrets, Redaction};
pub use sanitizer::{sanitize_fts5, sanitize_nl};
pub use session::{
    session_end, session_start, SessionEnd, SessionStart, DEFAULT_PROFILE_NOTES,
    DEFAULT_PROJECT_MEMORIES,
};
pub use stop_hook::{
    auto_summary_content, digest_transcript, parse_stop_input, remove_auto_summary,
    upsert_auto_summary, StopHookInput, TranscriptDigest,
};
pub use store::{validate_project_name, RenameReport, SqliteMemoryStore};
pub use wal::WalStore;
