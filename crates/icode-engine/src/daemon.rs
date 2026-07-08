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

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::time::Duration;

use icode_core::error::{Error, Result};
use icode_core::traits::CodeWriteStore;
use notify::RecursiveMode;
use notify_debouncer_full::{new_debouncer, DebounceEventResult};

use crate::index::{index_one_file, index_path, is_in_excluded_dir, is_supported_source};
use crate::store::SqliteCodeStore;

/// Debounce window: collapse a burst of FS events (editor save dance, git
/// checkout, rename pairs) into one batch before we touch the store.
const DEBOUNCE: Duration = Duration::from_millis(1000);

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
    //    guard and store stay owned here; the callback only forwards paths).
    let (tx, rx) = mpsc::channel::<Vec<PathBuf>>();
    let mut debouncer = new_debouncer(DEBOUNCE, None, move |res: DebounceEventResult| {
        if let Ok(events) = res {
            let paths: Vec<PathBuf> =
                events.into_iter().flat_map(|e| e.event.paths.clone()).collect();
            if !paths.is_empty() {
                let _ = tx.send(paths);
            }
        }
        // A watcher backend error is non-fatal: drop the batch, keep watching.
    })
    .map_err(|e| Error::Io(format!("watcher init: {e}")))?;

    debouncer
        .watch(&root, RecursiveMode::Recursive)
        .map_err(|e| Error::Io(format!("watch {}: {e}", root.display())))?;
    log(&format!("daemon: watching {} (Ctrl-C to stop)", root.display()));

    // Main loop: pull debounced batches; poll the stop flag on each timeout tick so
    // Ctrl-C is responsive even with no FS activity.
    while running.load(Ordering::Relaxed) {
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(paths) => process_batch(&root, &store, paths),
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    log("daemon: stopping (lock released)");
    Ok(())
}

/// Handle one debounced batch: dedup paths, drop excluded/unsupported ones, then
/// delete vanished files and re-index the rest (GRAPH only — embedding is on-demand,
/// so changed chunks are simply left in the pending queue). Never returns an error —
/// a single bad file is recorded and skipped so the daemon keeps running.
fn process_batch(root: &Path, store: &SqliteCodeStore, mut paths: Vec<PathBuf>) {
    paths.sort();
    paths.dedup();

    let mut reindexed = 0u64;
    let mut deleted = 0u64;

    for raw in paths {
        // Normalise to the indexer's key form: an absolute path under the
        // canonical root. Events already carry absolute paths, but canonicalising
        // (when the file still exists) guarantees the same string the walk yields.
        let path = normalise_under_root(root, &raw);

        // Skip anything the indexer itself would skip — excluded dirs (incl.
        // `.icode`, so our own db writes never feed back) and non-source files.
        if is_in_excluded_dir(&path) || !is_supported_source(&path) {
            continue;
        }

        if path.exists() {
            // Created or modified → re-index (replaces the file's old rows).
            match index_one_file(&path, store, root) {
                Ok(_) => reindexed += 1,
                Err(e) => {
                    // A parse/read failure of one file must not kill the daemon.
                    let _ = store.record_parse_error(&path.to_string_lossy(), &e.to_string());
                    log(&format!("daemon: parse error {} ({e})", path.display()));
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
        return; // batch was all noise (excluded/unsupported paths)
    }

    // Graph only. The re-index nulled out the replaced chunks' vectors; they now
    // sit in the pending queue and are embedded on demand (cache-accelerated, so a
    // revert/branch-switch costs nothing) — never in this hot path.
    log(&format!(
        "daemon: reindexed {reindexed} files, deleted {deleted} (vectors refresh on demand)"
    ));
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
        // SAFETY: `fd` is a valid open descriptor we just created.
        let rc = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
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
