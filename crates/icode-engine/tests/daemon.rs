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

/// (3) A daemon whose root disappears exits on its own instead of watching a
/// deleted tree forever (deleted worktrees and scratch dirs used to leave one
/// orphaned `icode daemon run` each).
#[test]
fn daemon_exits_when_its_root_disappears() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = std::fs::canonicalize(dir.path()).expect("canon root").join("proj");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("lib.rs"), V1).unwrap();
    // The store lives OUTSIDE the root so removing the tree does not pull the db
    // out from under the test's own assertions.
    let store_dir = tempfile::tempdir().expect("store dir");
    let store = SqliteCodeStore::open(store_dir.path()).expect("open store");

    let (tx, rx) = std::sync::mpsc::channel();
    let watched = root.clone();
    std::thread::spawn(move || {
        let r = icode_engine::run_daemon(&watched, store);
        let _ = tx.send(r.is_ok());
    });
    // Wait until the daemon holds its lock (it is watching), then delete the tree.
    let lock = root.join(".icode/daemon.lock");
    for _ in 0..100 {
        if fs::read_to_string(&lock).map(|s| !s.trim().is_empty()).unwrap_or(false) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    std::thread::sleep(std::time::Duration::from_millis(300));
    fs::remove_dir_all(&root).unwrap();
    let exited = rx.recv_timeout(std::time::Duration::from_secs(10));
    assert_eq!(exited, Ok(true), "daemon must exit cleanly once its root is gone");
}

/// (5) Reads are not changes. The inotify backend reports every open as
/// `Access(Open)`; if the daemon re-indexed on those, its own read of a file would
/// schedule the next read forever. Only events that can change bytes or existence
/// may reach the re-index path.
#[test]
fn reads_do_not_trigger_reindex_but_changes_do() {
    use icode_engine::is_content_event;
    use notify::event::{
        AccessKind, AccessMode, CreateKind, DataChange, EventKind, MetadataKind, ModifyKind,
        RemoveKind, RenameMode,
    };

    for read in [
        EventKind::Access(AccessKind::Open(AccessMode::Any)),
        EventKind::Access(AccessKind::Read),
        EventKind::Access(AccessKind::Close(AccessMode::Read)),
        EventKind::Access(AccessKind::Any),
        EventKind::Other,
    ] {
        assert!(!is_content_event(&read), "{read:?} must not re-index");
    }
    for change in [
        EventKind::Create(CreateKind::File),
        EventKind::Modify(ModifyKind::Data(DataChange::Any)),
        EventKind::Modify(ModifyKind::Metadata(MetadataKind::Any)),
        EventKind::Modify(ModifyKind::Name(RenameMode::To)),
        EventKind::Remove(RemoveKind::File),
        EventKind::Access(AccessKind::Close(AccessMode::Write)),
        EventKind::Any,
    ] {
        assert!(is_content_event(&change), "{change:?} must re-index");
    }
}

// ──────────────────────────── idle exit ────────────────────────────

mod idle_exit {
    use icode_engine::should_exit_idle;
    use std::time::{Duration, SystemTime};

    const IDLE: Duration = Duration::from_secs(6 * 3600);
    fn ago(now: SystemTime, secs: u64) -> SystemTime {
        now - Duration::from_secs(secs)
    }

    #[test]
    fn recent_events_keep_the_daemon_alive_even_without_a_client() {
        let now = SystemTime::now();
        assert!(!should_exit_idle(now, ago(now, 60), None, IDLE));
    }

    #[test]
    fn a_fresh_heartbeat_keeps_an_idle_daemon_alive() {
        let now = SystemTime::now();
        assert!(!should_exit_idle(now, ago(now, 7 * 3600), Some(ago(now, 30)), IDLE));
    }

    #[test]
    fn no_events_and_a_stale_heartbeat_mean_exit() {
        let now = SystemTime::now();
        assert!(should_exit_idle(now, ago(now, 7 * 3600), Some(ago(now, 7 * 3600)), IDLE));
        assert!(should_exit_idle(now, ago(now, 6 * 3600), Some(ago(now, 6 * 3600)), IDLE), "boundary is inclusive");
    }

    #[test]
    fn no_events_and_no_heartbeat_file_mean_exit() {
        let now = SystemTime::now();
        assert!(should_exit_idle(now, ago(now, 7 * 3600), None, IDLE));
    }

    #[test]
    fn timestamps_from_the_future_count_as_now() {
        let now = SystemTime::now();
        let future = now + Duration::from_secs(3600);
        assert!(!should_exit_idle(now, future, None, IDLE));
        assert!(!should_exit_idle(now, ago(now, 7 * 3600), Some(future), IDLE));
    }
}

// ──────────────────────────── lost events / liveness ────────────────────────────

mod lost_events {
    use icode_engine::{classify_watch_result, WatchMsg};
    use notify::event::{AccessKind, CreateKind, Event, EventKind, Flag};
    use notify_debouncer_full::DebouncedEvent;
    use std::path::PathBuf;
    use std::time::Instant;

    fn ev(kind: EventKind, path: &str) -> DebouncedEvent {
        DebouncedEvent::new(Event::new(kind).add_path(PathBuf::from(path)), Instant::now())
    }

    #[test]
    fn a_queue_overflow_asks_for_a_rescan() {
        let overflow = DebouncedEvent::new(Event::new(EventKind::Other).set_flag(Flag::Rescan), Instant::now());
        let msg = classify_watch_result(Ok(vec![ev(EventKind::Create(CreateKind::File), "/t/a.rs"), overflow]));
        assert!(matches!(msg, WatchMsg::Rescan(_)), "{msg:?}");
    }

    #[test]
    fn a_backend_error_asks_for_a_rescan() {
        let msg = classify_watch_result(Err(vec![notify::Error::generic("max_user_watches reached")]));
        assert!(matches!(msg, WatchMsg::Rescan(ref why) if why.contains("max_user_watches")), "{msg:?}");
    }

    #[test]
    fn reads_are_noise_and_changes_are_paths() {
        assert_eq!(classify_watch_result(Ok(vec![ev(EventKind::Access(AccessKind::Any), "/t/a.rs")])), WatchMsg::Noise);
        assert_eq!(
            classify_watch_result(Ok(vec![ev(EventKind::Create(CreateKind::File), "/t/a.rs")])),
            WatchMsg::Paths(vec![PathBuf::from("/t/a.rs")])
        );
    }
}

#[test]
fn lock_liveness_follows_the_flock_not_the_pid() {
    let dir = tempfile::tempdir().unwrap();
    assert!(!DaemonLock::is_held(dir.path()), "no lock file: no daemon");
    let lock = DaemonLock::acquire(dir.path()).unwrap();
    assert!(DaemonLock::is_held(dir.path()));
    drop(lock);
    // The file still names our (very much alive) pid — that must not count.
    assert!(!DaemonLock::is_held(dir.path()), "a released lock means no daemon, whatever pid the file holds");
}

/// A directory moved INTO the tree arrives as one event for the directory; its
/// files must still land in the graph, and leave it when the directory moves out.
#[test]
fn a_directory_moved_in_and_out_is_reflected_in_the_graph() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = std::fs::canonicalize(dir.path()).unwrap().join("proj");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("lib.rs"), V1).unwrap();
    let store = SqliteCodeStore::open(&root).expect("open store");
    let reader = SqliteCodeStore::open(&root).expect("open reader");

    let watched = root.clone();
    std::thread::spawn(move || {
        let _ = icode_engine::run_daemon(&watched, store);
    });
    let lock = root.join(".icode/daemon.lock");
    for _ in 0..100 {
        if fs::read_to_string(&lock).map(|s| !s.trim().is_empty()).unwrap_or(false) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    std::thread::sleep(std::time::Duration::from_millis(500));

    let outside = tempfile::tempdir().unwrap();
    let pkg = outside.path().join("pkg");
    fs::create_dir_all(pkg.join("deep")).unwrap();
    fs::write(pkg.join("deep/moved.rs"), "pub fn moved_in() {}\n").unwrap();
    fs::rename(&pkg, root.join("pkg")).unwrap();
    let moved = root.join("pkg/deep/moved.rs").to_string_lossy().to_string();
    let wait_for = |want: bool| {
        for _ in 0..60 {
            if reader.stat_file(&moved).unwrap().is_some() == want {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        false
    };
    assert!(wait_for(true), "a moved-in directory's files must be indexed");
    fs::rename(root.join("pkg"), outside.path().join("back")).unwrap();
    assert!(wait_for(false), "a moved-out directory's files must leave the index");
    fs::remove_dir_all(&root).unwrap(); // stops the daemon
}
