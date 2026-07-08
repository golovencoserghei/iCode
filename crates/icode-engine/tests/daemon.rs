//! Daemon CORE tests — the deterministic pieces the live watch-loop is built on.
//!
//! The `notify` watch-loop itself is intentionally NOT driven here: real FS
//! notifications are flaky/timing-dependent in CI sandboxes. Instead we test the
//! two cores the loop calls into:
//!   1. `index_one_file` lifecycle — create → stats grow; edit → old symbols
//!      replaced by new; delete via `delete_file` → symbols gone. This is exactly
//!      what `process_batch` does per changed path, with the SAME path-key form.
//!   2. the single-writer PID-lock — first acquire ok, a second acquire of the
//!      same project is `Err("already running")`, and after the first is dropped
//!      the lock is free again.
//!
//! Path form: we feed `index_one_file` the canonical absolute path under the
//! canonical root — exactly what the daemon's `normalise_under_root` produces and
//! what `index_path` stores — so the keys match and delete/re-index land.

use std::fs;

use icode_core::traits::{CodeReadStore, CodeWriteStore};
use icode_engine::{index_one_file, DaemonLock, SqliteCodeStore};

/// Canonical absolute path of `root/name`, matching the indexer's stored key form.
fn keyed(root: &std::path::Path, name: &str) -> std::path::PathBuf {
    // canonicalize requires the file to exist; callers create it first.
    std::fs::canonicalize(root.join(name)).expect("canonicalize")
}

const V1: &str = r#"
pub fn original_marker() -> u32 { 1 }

pub struct Widget { n: u32 }
impl Widget {
    pub fn spin(&self) -> u32 { self.n }
}
"#;

const V2: &str = r#"
pub fn replaced_marker() -> u32 { 2 }
pub fn second_marker() -> u32 { 3 }
"#;

/// (1) index_one_file lifecycle: create grows stats; edit replaces the old symbols
/// with the new ones (no stale carry-over); delete_file removes them entirely.
#[test]
fn index_one_file_create_edit_delete() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = std::fs::canonicalize(dir.path()).expect("canon root");
    let file = root.join("lib.rs");

    let store = SqliteCodeStore::open(&root).expect("open store");

    // ── create ──────────────────────────────────────────────────────────────
    fs::write(&file, V1).expect("write v1");
    let path_v1 = keyed(&root, "lib.rs");
    let s = index_one_file(&path_v1, &store, &root).expect("index v1");
    assert_eq!(s.files_indexed, 1);

    let st = store.stats().expect("stats");
    assert_eq!(st.files, 1, "one file indexed");
    assert_eq!(st.functions, 2, "original_marker + Widget::spin");
    assert_eq!(st.classes, 1, "Widget");
    assert!(
        store.get_function("original_marker", None, false).expect("get fn").is_some(),
        "original symbol present after create"
    );

    // ── edit ────────────────────────────────────────────────────────────────
    // Rewrite the file: old symbols must be REPLACED, not accumulated.
    fs::write(&file, V2).expect("write v2");
    let path_v2 = keyed(&root, "lib.rs"); // same canonical key as v1
    assert_eq!(path_v1, path_v2, "edit keeps the same path key");
    index_one_file(&path_v2, &store, &root).expect("index v2");

    let st = store.stats().expect("stats after edit");
    assert_eq!(st.files, 1, "still one file (upsert replaced, not added)");
    assert_eq!(st.functions, 2, "replaced_marker + second_marker");
    assert_eq!(st.classes, 0, "Widget gone after edit");
    assert!(
        store.get_function("original_marker", None, false).expect("get fn").is_none(),
        "old symbol REPLACED after edit"
    );
    assert!(
        store.get_function("replaced_marker", None, false).expect("get fn").is_some(),
        "new symbol present after edit"
    );

    // ── delete ──────────────────────────────────────────────────────────────
    // The daemon keys delete_file by the same canonical path string.
    fs::remove_file(&file).expect("rm file");
    store.delete_file(&path_v2.to_string_lossy()).expect("delete_file");

    let st = store.stats().expect("stats after delete");
    assert_eq!(st.files, 0, "file row gone");
    assert_eq!(st.functions, 0, "symbols cascaded away");
    assert_eq!(st.classes, 0);
    assert!(
        store.get_function("replaced_marker", None, false).expect("get fn").is_none(),
        "symbols gone after delete"
    );
}

/// (2) PID-lock: first acquire succeeds, a second acquire of the SAME project is an
/// error ("already running"), and after the first guard drops the lock is free.
#[test]
fn pid_lock_single_writer() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = std::fs::canonicalize(dir.path()).expect("canon root");

    // First writer takes the lock.
    let first = DaemonLock::acquire(&root).expect("first acquire ok");

    // Second acquire of the same project must be rejected.
    let second = DaemonLock::acquire(&root);
    assert!(second.is_err(), "second acquire must fail while first is held");
    let msg = format!("{}", second.err().unwrap());
    assert!(
        msg.contains("already running"),
        "error should say 'already running', got: {msg}"
    );

    // Release the first; the lock must become available again.
    drop(first);
    let third = DaemonLock::acquire(&root).expect("re-acquire after drop");
    drop(third);
}

/// Drop must NOT unlink the lock file. Removing it is an unlink race (a fresh
/// inode could let two daemons both think they hold the single-writer lock), so
/// the file has to survive a drop while the lock still becomes re-acquirable and
/// keeps enforcing single-writer on the SAME inode.
#[test]
fn pid_lock_file_survives_drop_and_reacquires() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = std::fs::canonicalize(dir.path()).expect("canon root");
    let lock_path = root.join(".icode").join("daemon.lock");

    {
        let _guard = DaemonLock::acquire(&root).expect("acquire");
        assert!(lock_path.exists(), "lock file exists while held");
    } // guard dropped here → flock released, but the file must remain

    assert!(
        lock_path.exists(),
        "lock file must survive drop (Drop must not unlink — unlink race)"
    );

    // Re-acquire on the SAME surviving inode; single-writer is still enforced.
    let again = DaemonLock::acquire(&root).expect("re-acquire on surviving file");
    let contended = DaemonLock::acquire(&root);
    assert!(
        contended.is_err(),
        "single-writer must still hold on the reused lock file"
    );
    drop(again);
    assert!(lock_path.exists(), "lock file still present after the second drop");
}
