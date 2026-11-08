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

/// Whether `name` can be a memory bucket: not empty, not in the reserved `__*`
/// namespace (the developer profile lives there), no control characters, at most
/// 200 characters. Spaces are allowed — a directory named `My Project` is a
/// legitimate bucket. The single rule for a host's `project` argument, the
/// `.icode-project` marker and every name the resolver derives from a path.
pub fn validate_bucket_name(name: &str) -> std::result::Result<(), String> {
    if name.trim().is_empty() {
        return Err("project name is empty".into());
    }
    if icode_core::ids::is_reserved_project(name) {
        return Err(format!("project name {name:?} is in the reserved `__` namespace"));
    }
    if name.chars().any(char::is_control) {
        return Err(format!("project name {name:?} contains control characters"));
    }
    if name.chars().count() > 200 {
        return Err("project name is longer than 200 characters".into());
    }
    Ok(())
}

/// The shared bucket name declared by `<root>/.icode-project`, if any. A name
/// that fails [`validate_bucket_name`], has a path separator or more than 64 chars
/// is ignored (a malformed marker must degrade to the directory name, never to a
/// junk or reserved bucket).
pub fn project_marker(root: &std::path::Path) -> Option<String> {
    let text = std::fs::read_to_string(root.join(PROJECT_MARKER)).ok()?;
    let name = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('#'))?;
    let ok = name.chars().count() <= 64
        && !name.contains(['/', '\\'])
        && validate_bucket_name(name).is_ok();
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

#[cfg(test)]
mod bucket_name_tests {
    use super::{project_marker, validate_bucket_name};

    #[test]
    fn bucket_names_reject_empty_reserved_and_control_chars() {
        for bad in ["", "   ", "__developer__", "__x", "a\u{7}b", "line\nbreak", &"x".repeat(201)] {
            assert!(validate_bucket_name(bad).is_err(), "{bad:?} must be rejected");
        }
        for good in ["iCode", "My Project", "Proekt Bot_Phantom 2", "a_b", "проект"] {
            assert!(validate_bucket_name(good).is_ok(), "{good:?} must be accepted");
        }
    }

    #[test]
    fn a_reserved_or_malformed_marker_is_ignored() {
        let td = tempfile::tempdir().unwrap();
        for bad in ["__developer__\n", "a/b\n", "\n# only a comment\n"] {
            std::fs::write(td.path().join(".icode-project"), bad).unwrap();
            assert_eq!(project_marker(td.path()), None, "{bad:?}");
        }
        std::fs::write(td.path().join(".icode-project"), "# shared\nSuite One\n").unwrap();
        assert_eq!(project_marker(td.path()).as_deref(), Some("Suite One"));
    }
}
