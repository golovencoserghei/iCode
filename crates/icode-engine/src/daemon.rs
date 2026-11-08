//! Live daemon + file-watcher: keep a project's code-graph index in lock-step with
//! the source tree as files change. One foreground process per project.
//!
//! Pipeline:
//!   1. acquire a single-writer PID-lock (`<root>/.icode/daemon.lock`) so two
//!      daemons never race on the same db;
//!   2. initial sync — run a full [`index_path`] to catch any edits made while the
//!      daemon was down;
//!   3. watch `root` recursively via `notify` + `notify-debouncer-full` (≈1 s
//!      debounce, so a save-storm / rename pair collapses into one batch);
//!   4. per batch: delete vanished files, re-index changed/created ones through
//!      [`index_one_file`].
//!
//! Embedding is DELIBERATELY NOT done here. The graph (parse → symbols → SQLite)
//! is cheap and always-correct, so keeping it live on every change is free; but
//! embedding is the slow part, and running it in the watch loop means a `git
//! checkout` that rewrites files — and every switch back — would churn the
//! embedder continuously (and keep the machine from idling). Instead vectors are
//! refreshed ON DEMAND when a semantic tool is actually used (the MCP/web server
//! drains the pending queue, cache-accelerated), or explicitly via `icode embed`.
//! The daemon just leaves freshly-changed chunks in the pending queue.
//!
//! Path form is load-bearing: see [`index_one_file`] — the watcher reconstructs
//! each changed path under the *canonicalised* `root` so the row keys match what
//! the indexer wrote (otherwise `delete_file`/`upsert` would miss).
//!
//! Graceful: a single file's parse error is recorded and skipped (the daemon never
//! dies on bad input); SIGINT (Ctrl-C) stops the watch loop and releases the lock.
//! After a long idle period with no live client the daemon exits on its own — see
//! [`should_exit_idle`].

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use icode_core::error::{Error, Result};
use icode_core::traits::CodeWriteStore;
use notify::event::{AccessKind, AccessMode};
use notify::{EventKind, RecursiveMode};
use notify_debouncer_full::{new_debouncer, DebounceEventResult};

use crate::index::{
    error_fingerprint, index_one_file, index_path, is_in_excluded_dir, is_in_linked_worktree, is_supported_source,
};
use crate::store::SqliteCodeStore;

/// Debounce window: collapse a burst of FS events (editor save dance, git
/// checkout, rename pairs) into one batch before we touch the store.
const DEBOUNCE: Duration = Duration::from_millis(1000);

/// File under `<root>/.icode/` whose mtime a running `icode serve` refreshes (at
/// start and about once a minute) so the daemon can tell a client is still using
/// the graph. See [`should_exit_idle`].
pub const SERVE_HEARTBEAT: &str = "serve.heartbeat";

/// How often a running `serve` refreshes [`SERVE_HEARTBEAT`].
pub const HEARTBEAT_EVERY: Duration = Duration::from_secs(60);

/// Default idle period after which a daemon with no events and no live client exits.
const DEFAULT_IDLE: Duration = Duration::from_secs(6 * 60 * 60);

/// How often the watch loop re-evaluates the idle rule once the idle period is up.
const IDLE_CHECK_EVERY: Duration = Duration::from_secs(30);

/// The idle period: `ICODE_DAEMON_IDLE_SECS` (seconds; `0` disables the idle exit),
/// else 6 hours.
pub fn idle_timeout() -> Option<Duration> {
    match std::env::var("ICODE_DAEMON_IDLE_SECS").ok().and_then(|v| v.trim().parse::<u64>().ok()) {
        Some(0) => None,
        Some(secs) => Some(Duration::from_secs(secs)),
        None => Some(DEFAULT_IDLE),
    }
}

/// Whether an idle daemon should exit now.
///
/// A daemon used to live forever: one per project ever served, each holding an
/// inotify watch on the whole tree and an open SQLite connection (which in turn
/// kept the WAL from shrinking), long after anyone last asked it anything. It now
/// exits when BOTH hold for at least `idle`:
///   * no batch of source-file events arrived (`last_activity`), and
///   * no client is alive — the [`SERVE_HEARTBEAT`] mtime is older than `idle`
///     (or the file does not exist).
///
/// The next `serve` finds no daemon, runs its own startup pass and spawns a fresh
/// one, so exiting costs one pass, never correctness. A clock that went backwards
/// (a timestamp in the future) counts as "just now": it keeps the daemon alive.
pub fn should_exit_idle(
    now: SystemTime,
    last_activity: SystemTime,
    heartbeat: Option<SystemTime>,
    idle: Duration,
) -> bool {
    let age = |t: SystemTime| now.duration_since(t).unwrap_or(Duration::ZERO);
    if age(last_activity) < idle {
        return false;
    }
    match heartbeat {
        Some(beat) => age(beat) >= idle,
        None => true,
    }
}

/// mtime of `<root>/.icode/serve.heartbeat`, if it exists.
fn heartbeat_mtime(root: &Path) -> Option<SystemTime> {
    std::fs::metadata(root.join(".icode").join(SERVE_HEARTBEAT)).ok()?.modified().ok()
}

/// Run the live indexing daemon for `root` (foreground; blocks until SIGINT).
///
/// `store` is the already-opened per-project code store. The daemon keeps the
/// GRAPH in lock-step with the source tree but does NOT embed — vectors are
/// refreshed on demand by the semantic tools (or `icode embed`). See the module
/// docs for why embedding stays out of the watch loop.
///
/// Errors out early if another daemon already holds this project's lock.
pub fn run_daemon(root: &Path, store: SqliteCodeStore) -> Result<()> {
    // Canonicalise once: every watched path is rebuilt under this so the stored
    // keys match `index_path`'s (which walks from this same root).
    let root = std::fs::canonicalize(root).map_err(|e| Error::Io(e.to_string()))?;

    // 1) Single-writer guard. Held for the whole run; released on drop (also on
    //    crash — the kernel drops the flock when the fd closes).
    let _lock = DaemonLock::acquire(&root)?;
    log(&format!("daemon: lock acquired (pid {})", std::process::id()));

    // 2) Initial sync: catch graph edits made while we were down. No embedding —
    //    freshly-changed chunks wait in the pending queue for an on-demand pass.
    let stats = index_path(&root, &store)?;
    log(&format!(
        "daemon: initial sync — {} reindexed, {} unchanged, {} deleted, {} functions, {} classes, {} chunks ({} errors)",
        stats.files_indexed,
        stats.files_skipped,
        stats.files_deleted,
        stats.functions,
        stats.classes,
        stats.code_chunks,
        stats.errors
    ));
    log("daemon: graph live; embedding is on-demand (semantic tools / `icode embed`)");

    // 3) SIGINT → stop flag. The watch loop polls it between batches.
    let running = Arc::new(AtomicBool::new(true));
    install_sigint(running.clone());

    // 4) Debounced watcher → batches over a channel into this thread (so the lock
    //    guard and store stay owned here; the callback only classifies).
    let (tx, rx) = mpsc::channel::<WatchMsg>();
    let mut debouncer = new_debouncer(DEBOUNCE, None, move |res: DebounceEventResult| {
        let msg = classify_watch_result(res);
        if msg != WatchMsg::Noise {
            let _ = tx.send(msg);
        }
    })
    .map_err(|e| Error::Io(format!("watcher init: {e}")))?;

    debouncer
        .watch(&root, RecursiveMode::Recursive)
        .map_err(|e| Error::Io(format!("watch {}: {e}", root.display())))?;
    log(&format!("daemon: watching {} (Ctrl-C to stop)", root.display()));

    // Main loop: pull debounced batches; poll the stop flag on each timeout tick so
    // Ctrl-C is responsive even with no FS activity.
    let idle = idle_timeout();
    let mut last_activity = SystemTime::now();
    let mut next_idle_check = Instant::now();
    let mut rescan_pending = false;
    let mut last_rescan = Instant::now()
        .checked_sub(MIN_RESCAN_GAP)
        .unwrap_or_else(Instant::now);
    while running.load(Ordering::Relaxed) {
        // A daemon whose tree is gone (a deleted worktree, a cleaned-up scratch
        // dir) has nothing left to watch: exit instead of lingering forever.
        if !root.is_dir() {
            log(&format!("daemon: root {} disappeared — exiting", root.display()));
            break;
        }
        // A pending rescan whose rate-limit gap has passed runs now.
        if rescan_pending && last_rescan.elapsed() >= MIN_RESCAN_GAP {
            rescan_pending = false;
            last_rescan = Instant::now();
            full_rescan(&root, &store);
            last_activity = SystemTime::now();
        }
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(WatchMsg::Paths(paths)) => {
                if process_batch(&root, &store, paths) {
                    last_activity = SystemTime::now();
                }
            }
            Ok(WatchMsg::Noise) => {}
            Ok(WatchMsg::Rescan(why)) => {
                // Events were LOST (kernel queue overflow, a watch that could not be
                // added): the tree may differ from the graph in ways no later event
                // will reveal. Only a walk can tell — `index_path` hash-skips what
                // did not change. Rate-limited so a persistent watch error (an
                // exhausted `max_user_watches`) cannot turn into a walk per event.
                if !rescan_pending {
                    log(&format!("daemon: events lost ({why}) — scheduling a full rescan"));
                }
                rescan_pending = true;
            }
            Err(RecvTimeoutError::Timeout) => {
                let Some(idle) = idle else { continue };
                if Instant::now() < next_idle_check {
                    continue;
                }
                next_idle_check = Instant::now() + IDLE_CHECK_EVERY.min(idle);
                if should_exit_idle(SystemTime::now(), last_activity, heartbeat_mtime(&root), idle) {
                    log(&format!(
                        "daemon: no changes and no client for {}s — exiting (the next `icode serve` restarts it)",
                        idle.as_secs()
                    ));
                    break;
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    log("daemon: stopping (lock released)");
    Ok(())
}

/// Minimum spacing between two full rescans triggered by lost events.
const MIN_RESCAN_GAP: Duration = Duration::from_secs(10);

/// What the watcher callback hands the main loop.
#[derive(Debug, PartialEq, Eq)]
pub enum WatchMsg {
    /// Paths whose content or existence may have changed.
    Paths(Vec<PathBuf>),
    /// Events were lost or the backend failed: the graph can only be trusted again
    /// after a full walk. Carries a short reason for the log.
    Rescan(String),
    /// Nothing that can change the graph (reads, `Other` without a rescan flag).
    Noise,
}

/// Turn one debounced watcher delivery into a [`WatchMsg`].
///
/// Both failure shapes used to be dropped on the floor, and either one left the
/// daemon "alive" over a graph it no longer tracked — while `serve` trusted that
/// daemon and skipped its own startup pass:
///   * an inotify queue overflow arrives as an `Other` event flagged `need_rescan`
///     (the kernel discarded events; `max_queued_events` is 16384), which the
///     content filter rejected;
///   * a backend `Err` (e.g. `max_user_watches` exhausted while adding a watch for
///     a new directory) was ignored outright, so nothing under that directory was
///     ever watched.
pub fn classify_watch_result(res: DebounceEventResult) -> WatchMsg {
    match res {
        Err(errors) => WatchMsg::Rescan(format!(
            "watcher error: {}",
            errors.first().map(|e| e.to_string()).unwrap_or_default()
        )),
        Ok(events) => {
            if events.iter().any(|e| e.event.need_rescan()) {
                return WatchMsg::Rescan("event queue overflowed".into());
            }
            let paths: Vec<PathBuf> = events
                .into_iter()
                .filter(|e| is_content_event(&e.event.kind))
                .flat_map(|e| e.event.paths.clone())
                .collect();
            if paths.is_empty() {
                WatchMsg::Noise
            } else {
                WatchMsg::Paths(paths)
            }
        }
    }
}

/// Re-walk the whole tree after lost events. Never fails the daemon.
fn full_rescan(root: &Path, store: &SqliteCodeStore) {
    match index_path(root, store) {
        Ok(s) => log(&format!(
            "daemon: rescan — {} reindexed, {} unchanged, {} deleted ({} errors)",
            s.files_indexed, s.files_skipped, s.files_deleted, s.errors
        )),
        Err(e) => log(&format!("daemon: rescan failed ({e}) — will retry on the next lost event")),
    }
}

/// Handle one debounced batch: dedup paths, drop excluded/unsupported ones, then
/// delete vanished files and re-index the rest (GRAPH only — embedding is on-demand,
/// so changed chunks are simply left in the pending queue). Never returns an error —
/// a single bad file is recorded and skipped so the daemon keeps running.
///
/// Returns whether the batch touched any source file this tree indexes (even one
/// whose bytes turned out unchanged) — the activity signal for the idle exit. A
/// batch of only excluded paths (our own `.icode` writes, `target/`) is not activity.
fn process_batch(root: &Path, store: &SqliteCodeStore, mut paths: Vec<PathBuf>) -> bool {
    paths.sort();
    paths.dedup();

    let mut relevant = false;
    let mut reindexed = 0u64;
    let mut deleted = 0u64;

    for raw in paths {
        // Normalise to the indexer's key form: an absolute path under the
        // canonical root. Events already carry absolute paths, but canonicalising
        // (when the file still exists) guarantees the same string the walk yields.
        let path = normalise_under_root(root, &raw);

        // Skip anything the indexer itself would skip — excluded dirs (incl.
        // `.icode`, so our own db writes never feed back) and non-source files.
        // Linked worktrees below the root are other checkouts, not this tree's
        // source — the same rule the indexer walk applies.
        if is_in_excluded_dir(&path) || !is_supported_source(&path) || is_in_linked_worktree(root, &path) {
            continue;
        }
        relevant = true;

        if path.exists() {
            // Created or modified → re-index (replaces the file's old rows).
            match index_one_file(&path, store, root) {
                // A watcher event does NOT mean the bytes changed (atomic saves,
                // `touch`, a container remounting the tree). `index_one_file` now
                // hash-skips those and reports them as skipped, so an idle project
                // costs a read + a hash instead of a reparse.
                Ok(s) if s.files_indexed == 0 => {}
                Ok(_) => reindexed += 1,
                Err(e) => {
                    // A parse/read failure of one file must not kill the daemon. Each
                    // version of a failing file is recorded and logged ONCE: the same
                    // bytes failing again on the next event is not news.
                    let fp = error_fingerprint(&path);
                    let fresh = store
                        .record_parse_error_once(&path.to_string_lossy(), &fp, &e.to_string())
                        .unwrap_or(true);
                    if fresh {
                        log(&format!("daemon: parse error {} ({e})", path.display()));
                    }
                }
            }
        } else {
            // Deleted (or moved away) → drop its rows by the stored key.
            match store.delete_file(&path.to_string_lossy()) {
                Ok(()) => deleted += 1,
                Err(e) => log(&format!("daemon: delete failed {} ({e})", path.display())),
            }
        }
    }

    if reindexed == 0 && deleted == 0 {
        return relevant; // nothing changed (noise, or unchanged bytes)
    }

    // Graph centrality is a WHOLE-GRAPH property: editing one file can change the rank
    // of symbols in another. `index_one_file` only resolves the touched file's edges,
    // so without this the `rank` column silently drifts away from the graph it is meant
    // to describe — and search ranking quietly degrades the longer the daemon runs.
    // Cheap: a few passes over the edge list, no parse, no I/O.
    if let Err(e) = store.compute_symbol_ranks() {
        log(&format!("daemon: rank refresh failed ({e}) — ranking may be stale"));
    }

    // The batch's writes are in the WAL; fold them back and shrink the file so a
    // long-lived daemon does not keep the WAL at its high-water mark. Best-effort:
    // a busy reader just defers it to the next batch.
    store.checkpoint_truncate();

    // Graph only. The re-index nulled out the replaced chunks' vectors; they now
    // sit in the pending queue and are embedded on demand (cache-accelerated, so a
    // revert/branch-switch costs nothing) — never in this hot path.
    log(&format!(
        "daemon: reindexed {reindexed} files, deleted {deleted} (vectors refresh on demand)"
    ));
    true
}

/// True for watcher events that can mean a file's bytes or existence changed.
///
/// `notify`'s inotify backend also subscribes to `IN_OPEN`, so merely READING a
/// file yields an `Access(Open)` event. Forwarding those made the daemon feed
/// itself: re-indexing a file reads it, the read is an open, the open is the next
/// batch — once any file had been read by anyone (an agent's grep, an editor,
/// git) the daemon re-read it every debounce tick forever, and a whole project
/// ended up in that loop (measured: 5k files, ~12k opens/s, half a core idle).
/// Only creations, modifications (data, metadata, renames) and removals count;
/// `Close(Write)` is kept because some writers only surface on close.
pub fn is_content_event(kind: &EventKind) -> bool {
    match kind {
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_) => true,
        EventKind::Access(AccessKind::Close(AccessMode::Write)) => true,
        EventKind::Access(_) | EventKind::Other => false,
        // Unknown kind from a backend that cannot classify: re-index to be safe;
        // the content-hash skip makes a spurious one cheap.
        EventKind::Any => true,
    }
}

/// Rebuild a watched path into the indexer's key form: the canonical absolute path
/// when the file still exists, else the raw event path joined logic-cleaned. The
/// canonical root is a prefix of both, so the result matches `walk_source_files`.
fn normalise_under_root(root: &Path, raw: &Path) -> PathBuf {
    // For an existing file, canonicalize resolves symlinks/`..` to the exact form
    // the walk produced. For a deleted file we can't canonicalize, so fall back to
    // the raw path (already absolute under the watched root).
    match std::fs::canonicalize(raw) {
        Ok(c) => c,
        Err(_) => {
            if raw.is_absolute() {
                raw.to_path_buf()
            } else {
                root.join(raw)
            }
        }
    }
}

// ──────────────────────────── PID-lock ────────────────────────────

/// Single-writer guard over `<root>/.icode/daemon.lock`.
///
/// Mechanism: an advisory **exclusive** `flock(LOCK_EX | LOCK_NB)` on the lock
/// file's fd. The lock lives with the open fd, so the kernel releases it the moment
/// the process exits — crash, SIGKILL, or clean drop — leaving no stale lock to
/// clean up (unlike a bare PID file, which survives a crash and needs liveness
/// probing). We also write our PID into the file purely so a *second* daemon can
/// print a helpful "already running (pid N)".
///
/// Public so the daemon test can exercise lock contention without spinning up the
/// (blocking) watch loop: acquire → second `acquire` is `Err` → drop → re-acquire.
pub struct DaemonLock {
    fd: libc::c_int,
}

impl DaemonLock {
    /// Acquire the project's single-writer lock under `<root>/.icode/daemon.lock`.
    /// `Err` if another live process already holds it (message names its PID).
    pub fn acquire(root: &Path) -> Result<Self> {
        let dir = root.join(".icode");
        std::fs::create_dir_all(&dir).map_err(|e| Error::Io(e.to_string()))?;
        let path = dir.join("daemon.lock");

        // Open (create) the lock file for read+write.
        let c_path = std::ffi::CString::new(path.as_os_str().to_string_lossy().as_bytes())
            .map_err(|e| Error::Io(e.to_string()))?;
        // SAFETY: standard libc open with a valid NUL-terminated path and mode.
        let fd = unsafe { libc::open(c_path.as_ptr(), libc::O_RDWR | libc::O_CREAT, 0o644) };
        if fd < 0 {
            return Err(Error::Io(format!(
                "cannot open lock file {}: {}",
                path.display(),
                std::io::Error::last_os_error()
            )));
        }

        // Non-blocking exclusive advisory lock. EWOULDBLOCK ⇒ someone else holds it.
        // A few short retries: [`DaemonLock::is_held`] probes with a momentary
        // SHARED lock, and a daemon starting during that instant must not mistake
        // the probe for a rival daemon. A real holder outlasts the retries.
        let mut rc = -1;
        for attempt in 0..10 {
            // SAFETY: `fd` is a valid open descriptor we just created.
            rc = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
            if rc == 0 {
                break;
            }
            if attempt < 9 {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            // Read the holder's PID (best-effort) for a friendly message.
            let holder = std::fs::read_to_string(&path)
                .ok()
                .and_then(|s| s.trim().parse::<i32>().ok());
            // SAFETY: closing our own fd; we error out right after.
            unsafe { libc::close(fd) };
            return match holder {
                Some(pid) => Err(Error::Invalid(format!(
                    "daemon already running (pid {pid}) for this project"
                ))),
                None => Err(Error::Invalid(format!(
                    "daemon already running for this project (lock held: {err})"
                ))),
            };
        }

        // We own the lock — stamp our PID for the next contender's message.
        // Truncate first so a shorter PID can't leave stale trailing bytes.
        // SAFETY: `fd` is valid and we hold the lock.
        unsafe {
            libc::ftruncate(fd, 0);
        }
        let _ = std::fs::write(&path, format!("{}\n", std::process::id()));

        Ok(Self { fd })
    }
}

impl DaemonLock {
    /// Whether a live daemon holds `<root>/.icode/daemon.lock` right now.
    ///
    /// The kernel's flock IS the liveness signal: it is held exactly as long as the
    /// daemon process lives (released on exit, crash or SIGKILL; a zombie has
    /// already closed its fds). The PID written in the file is NOT — pids are
    /// reused, and with daemons now exiting when idle a stale pid naming some
    /// unrelated live process is the normal case, not a corner. Probed with a
    /// non-blocking SHARED lock: it succeeds only if nobody holds the exclusive one,
    /// and is dropped immediately.
    pub fn is_held(root: &Path) -> bool {
        let path = root.join(".icode").join("daemon.lock");
        let Ok(c_path) = std::ffi::CString::new(path.as_os_str().to_string_lossy().as_bytes()) else {
            return false;
        };
        // SAFETY: valid NUL-terminated path; read-only open creates nothing.
        let fd = unsafe { libc::open(c_path.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
        if fd < 0 {
            return false; // no lock file: no daemon ever ran here
        }
        // SAFETY: `fd` is the descriptor just opened; closed before returning.
        unsafe {
            let rc = libc::flock(fd, libc::LOCK_SH | libc::LOCK_NB);
            if rc == 0 {
                libc::flock(fd, libc::LOCK_UN);
            }
            libc::close(fd);
            rc != 0
        }
    }
}

impl Drop for DaemonLock {
    fn drop(&mut self) {
        // Release the advisory lock and close the fd. We deliberately DO NOT unlink
        // the lock file. Removing it is an unlink race: once we release the flock,
        // another process can open+flock the SAME inode, and a `remove_file` here
        // would then delete the file that process holds — so the next daemon creates
        // a FRESH inode whose flock no longer conflicts, yielding two "single"
        // writers. The kernel drops our flock on close/exit regardless, and the
        // lingering file is harmless: its PID line is best-effort only and is
        // re-truncated + rewritten on the next `acquire`.
        // SAFETY: `self.fd` is the descriptor we opened and still own.
        unsafe {
            libc::flock(self.fd, libc::LOCK_UN);
            libc::close(self.fd);
        }
    }
}

// ──────────────────────────── signals / logging ────────────────────────────

/// Install a SIGINT (Ctrl-C) handler that flips `running` to `false`, letting the
/// watch loop exit and the lock guard drop cleanly. Best-effort: a failure to set
/// the handler just means Ctrl-C keeps its default (terminate) behaviour, which
/// still releases the flock via process teardown.
fn install_sigint(running: Arc<AtomicBool>) {
    // notify-debouncer-full has no signal facility; use a raw sigaction with a
    // static flag, then bridge it onto the per-run `running` flag via a watcher
    // thread. We keep it dependency-free (no `ctrlc` crate).
    static SIGINT_FIRED: AtomicBool = AtomicBool::new(false);

    extern "C" fn handle(_sig: libc::c_int) {
        SIGINT_FIRED.store(true, Ordering::SeqCst);
    }

    // SAFETY: installing a signal handler that only does an atomic store (async-
    // signal-safe). `sigaction` with a zeroed mask/flags is the documented setup.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = handle as *const () as usize;
        libc::sigemptyset(&mut action.sa_mask);
        action.sa_flags = 0;
        libc::sigaction(libc::SIGINT, &action, std::ptr::null_mut());
        libc::sigaction(libc::SIGTERM, &action, std::ptr::null_mut());
    }

    // Bridge the async-signal flag onto the run flag from a normal thread.
    std::thread::spawn(move || loop {
        if SIGINT_FIRED.load(Ordering::SeqCst) {
            running.store(false, Ordering::Relaxed);
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    });
}

/// Daemon log line → stderr (stdout is reserved for any future structured output;
/// keeping logs on stderr matches the rest of the CLI's best-effort notes). Uses
/// `tracing` if a subscriber is installed, else a plain `eprintln!`.
fn log(msg: &str) {
    tracing::info!("{msg}");
    eprintln!("{msg}");
}
