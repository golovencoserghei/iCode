//! `verify` — a ledger of what was actually PROVEN about this working tree.
//!
//! The gap it closes: an agent edits code, says "done", and the claim rests on
//! nothing. iCode already knows two halves of the answer — which tests reach a
//! symbol (`find_tests_covering`) and what the source tree looks like — but it
//! never knew whether those tests were ever RUN, or when. This module records the
//! runs so the two halves can be joined:
//!
//! > `src/auth.rs` changed 4 minutes ago; the 3 tests covering it last ran BEFORE
//! > that change.
//!
//! Design, in the order the constraints bind:
//!
//! * **Passive.** Nothing here runs a test, blocks a turn, or judges a claim. It
//!   records what happened and reports what that implies. A ledger that decides
//!   when work is finished would be wrong at the worst moment.
//! * **Its own database.** The log lives in `~/.icode/verify.db`, not in the
//!   per-project `index.db` (which is DISCARDED on a schema bump — evidence must
//!   not evaporate because the graph was rebuilt) and not in the central memory db
//!   (whose writes need an embedder; verification has to work with Ollama down).
//! * **Honest scope.** A run either names the files it exercised or names none.
//!   Naming none means "project-wide" (`cargo test`, `pytest`), which covers every
//!   file. A targeted run (`cargo test auth::`) covers only what it names. We never
//!   upgrade a targeted run into repo-green — that is the single dishonesty this
//!   module exists to prevent.
//! * **Failures are evidence too.** A failed run is recorded with `ok = false` and
//!   never counts as coverage: "the tests ran and were red" must not read as
//!   "verified".

pub mod hook;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use icode_core::error::{Error, Result};

use crate::store::SqliteCodeStore;

fn log_err<E: std::fmt::Display>(e: E) -> Error {
    Error::Store(e.to_string())
}

/// Schema for the evidence log. Standalone and tiny: one row per run, one row per
/// (run, file) pair. `ran_at` is unix milliseconds — the same unit the rest of the
/// product timestamps with, and directly comparable to a file mtime.
const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS verify_runs (
    id       INTEGER PRIMARY KEY AUTOINCREMENT,
    project  TEXT    NOT NULL,
    kind     TEXT    NOT NULL,          -- test | build | lint | typecheck | other
    command  TEXT    NOT NULL,
    ok       INTEGER NOT NULL,          -- 1 = the command exited 0
    -- 1 = the run exercised the WHOLE project (`cargo test`, `pytest`).
    -- 0 = it was narrowed (a filter, a single package) — history, but not coverage
    -- unless it also named files. Without this column a filtered run with no known
    -- file list is indistinguishable from a full one, and the ledger would report a
    -- green repo off the back of `cargo test one_test_name`.
    wide     INTEGER NOT NULL DEFAULT 1,
    ran_at   INTEGER NOT NULL,          -- unix ms
    scope    TEXT,                      -- free-form: what the run targeted, if narrow
    summary  TEXT                       -- short tail of output, for the report
);

CREATE INDEX IF NOT EXISTS idx_verify_runs_project ON verify_runs(project, ran_at);

-- Files a run exercised. NO rows for a run = project-wide (covers every file).
CREATE TABLE IF NOT EXISTS verify_run_files (
    run_id INTEGER NOT NULL REFERENCES verify_runs(id) ON DELETE CASCADE,
    path   TEXT    NOT NULL,
    PRIMARY KEY (run_id, path)
);

CREATE INDEX IF NOT EXISTS idx_verify_run_files_path ON verify_run_files(path);
"#;

/// How many runs a project keeps. The ledger answers "recently", not "ever": an
/// eight-month-old test run is not evidence about today's tree, and an unbounded
/// log on a busy repo is a file that only grows.
const MAX_RUNS_PER_PROJECT: i64 = 500;

/// Longest summary we store per run. Enough for a failure's last lines; short
/// enough that the log stays a log.
const MAX_SUMMARY_CHARS: usize = 2000;

/// Longest command we store. The ledger identifies a run; it is not a shell
/// history. An agent's Bash call is routinely a whole multi-line script, and
/// storing it verbatim turned `verify status` into a wall of pasted code with the
/// verdicts lost inside it.
const MAX_COMMAND_CHARS: usize = 200;

/// One line, bounded: newlines and runs of spaces collapse to single spaces, and
/// anything past [`MAX_COMMAND_CHARS`] is cut. Keeps a report readable without
/// losing which command it was.
fn one_line(cmd: &str) -> String {
    let flat = cmd.split_whitespace().collect::<Vec<_>>().join(" ");
    match flat.char_indices().nth(MAX_COMMAND_CHARS) {
        Some((idx, _)) => format!("{}…", &flat[..idx]),
        None => flat,
    }
}

/// What kind of check a run was. Free-form at the edges (anything unrecognised is
/// `Other`), because the shape of "a check" differs per ecosystem and a closed
/// enum would silently drop evidence it did not anticipate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunKind {
    Test,
    Build,
    Lint,
    Typecheck,
    Other,
}

impl RunKind {
    pub fn as_str(self) -> &'static str {
        match self {
            RunKind::Test => "test",
            RunKind::Build => "build",
            RunKind::Lint => "lint",
            RunKind::Typecheck => "typecheck",
            RunKind::Other => "other",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "test" | "tests" => RunKind::Test,
            "build" | "compile" => RunKind::Build,
            "lint" | "clippy" | "format" | "fmt" => RunKind::Lint,
            "typecheck" | "types" | "check" => RunKind::Typecheck,
            _ => RunKind::Other,
        }
    }
}

/// A run about to be recorded.
#[derive(Clone, Debug)]
pub struct NewRun {
    pub project: String,
    pub kind: RunKind,
    pub command: String,
    pub ok: bool,
    /// Files the run exercised. Empty + `wide` = project-wide.
    pub files: Vec<String>,
    /// Did this run exercise the whole project? A narrowed run (a test filter, one
    /// package) must set this false, or it will be credited with coverage it never had.
    pub wide: bool,
    pub scope: Option<String>,
    pub summary: Option<String>,
}

/// A recorded run, as read back.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Run {
    pub id: i64,
    pub kind: String,
    pub command: String,
    pub ok: bool,
    /// Unix milliseconds.
    pub ran_at: i64,
    /// Files the run named. Empty + `wide` = project-wide.
    pub files: Vec<String>,
    /// Whether the run exercised the whole project.
    pub wide: bool,
    pub scope: Option<String>,
    pub summary: Option<String>,
}

/// One file whose content is newer than the last green run covering it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StaleFile {
    pub path: String,
    /// File mtime, unix ms.
    pub changed_at: i64,
    /// When a green run last covered this file (`None` = never).
    pub last_verified_at: Option<i64>,
    /// The command of that run, when there was one.
    pub last_command: Option<String>,
    /// Tests that reach this file's symbols, so the report says what to RUN, not
    /// just that something is unproven. Empty means the graph knows of no test
    /// reaching it — which is itself worth seeing.
    pub covering_tests: Vec<String>,
}

/// The answer to "what is unproven right now?".
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StaleReport {
    /// Files changed since the last green run covering them, newest change first.
    pub stale: Vec<StaleFile>,
    /// Exact count (`stale` is capped by the caller's limit).
    pub stale_count: usize,
    /// Files considered (indexed, still on disk).
    pub files_checked: usize,
    /// The most recent green project-wide run, if any.
    pub last_green_run: Option<Run>,
    /// True when nothing is stale — every indexed file is older than a green run
    /// covering it. NOT a claim that the tests are good, only that they ran after
    /// the last edit.
    pub all_verified: bool,
}

/// The evidence log.
pub struct VerifyLog {
    conn: Connection,
}

impl VerifyLog {
    /// Open (creating if needed) the log at `path`. WAL so a daemon writing a run
    /// never blocks a tool call reading one.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| Error::Io(e.to_string()))?;
        }
        let conn = Connection::open(path).map_err(log_err)?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(log_err)?;
        conn.busy_timeout(std::time::Duration::from_millis(5_000))
            .map_err(log_err)?;
        conn.execute_batch("PRAGMA foreign_keys = ON;")
            .map_err(log_err)?;
        conn.execute_batch(SCHEMA).map_err(log_err)?;
        Ok(Self { conn })
    }

    /// Open the default log (`~/.icode/verify.db`), honouring `ICODE_VERIFY_DB`
    /// for tests and ops.
    pub fn open_default() -> Result<Self> {
        Self::open(&default_path())
    }

    /// In-memory log (tests).
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().map_err(log_err)?;
        conn.execute_batch("PRAGMA foreign_keys = ON;")
            .map_err(log_err)?;
        conn.execute_batch(SCHEMA).map_err(log_err)?;
        Ok(Self { conn })
    }

    /// Re-file every run of project `old` under `new` (a project rename). Returns
    /// how many runs moved. Runs keep their ids, files and timestamps.
    pub fn rename_project(&self, old: &str, new: &str) -> Result<usize> {
        self.conn
            .execute("UPDATE verify_runs SET project = ?1 WHERE project = ?2", [new, old])
            .map_err(log_err)
    }

    /// Record one run. Returns its id.
    ///
    /// Paths are stored exactly as given: the caller normalises them to the same
    /// form the index uses (relative to the project root), because a report that
    /// mixes `src/x.rs` with `/abs/src/x.rs` silently matches nothing.
    pub fn record(&self, run: &NewRun) -> Result<i64> {
        let ran_at = now_ms();
        let summary = run.summary.as_deref().map(|s| {
            let s = s.trim();
            match s.char_indices().nth(MAX_SUMMARY_CHARS) {
                Some((idx, _)) => format!("{}…", &s[..idx]),
                None => s.to_string(),
            }
        });
        self.conn
            .execute(
                "INSERT INTO verify_runs (project, kind, command, ok, wide, ran_at, scope, summary) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                rusqlite::params![
                    run.project,
                    run.kind.as_str(),
                    one_line(&run.command),
                    i64::from(run.ok),
                    i64::from(run.wide && run.files.is_empty()),
                    ran_at,
                    run.scope,
                    summary,
                ],
            )
            .map_err(log_err)?;
        let id = self.conn.last_insert_rowid();

        for path in &run.files {
            let path = path.trim();
            if path.is_empty() {
                continue;
            }
            self.conn
                .execute(
                    "INSERT OR IGNORE INTO verify_run_files (run_id, path) VALUES (?1, ?2)",
                    rusqlite::params![id, path],
                )
                .map_err(log_err)?;
        }

        self.prune(&run.project)?;
        Ok(id)
    }

    /// Drop runs beyond [`MAX_RUNS_PER_PROJECT`], oldest first.
    fn prune(&self, project: &str) -> Result<()> {
        self.conn
            .execute(
                "DELETE FROM verify_runs WHERE project = ?1 AND id NOT IN \
                 (SELECT id FROM verify_runs WHERE project = ?1 ORDER BY id DESC LIMIT ?2)",
                rusqlite::params![project, MAX_RUNS_PER_PROJECT],
            )
            .map_err(log_err)?;
        Ok(())
    }

    /// The most recent runs for a project, newest first.
    pub fn recent(&self, project: &str, limit: usize) -> Result<Vec<Run>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, kind, command, ok, wide, ran_at, scope, summary FROM verify_runs \
                 WHERE project = ?1 ORDER BY ran_at DESC, id DESC LIMIT ?2",
            )
            .map_err(log_err)?;
        let rows = stmt
            .query_map(rusqlite::params![project, limit as i64], |r| {
                Ok(Run {
                    id: r.get(0)?,
                    kind: r.get(1)?,
                    command: r.get(2)?,
                    ok: r.get::<_, i64>(3)? != 0,
                    wide: r.get::<_, i64>(4)? != 0,
                    ran_at: r.get(5)?,
                    files: Vec::new(),
                    scope: r.get(6)?,
                    summary: r.get(7)?,
                })
            })
            .map_err(log_err)?;
        let mut out: Vec<Run> = rows.collect::<rusqlite::Result<_>>().map_err(log_err)?;
        for run in &mut out {
            run.files = self.files_of(run.id)?;
        }
        Ok(out)
    }

    fn files_of(&self, run_id: i64) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT path FROM verify_run_files WHERE run_id = ?1 ORDER BY path")
            .map_err(log_err)?;
        let rows = stmt
            .query_map(rusqlite::params![run_id], |r| r.get::<_, String>(0))
            .map_err(log_err)?;
        rows.collect::<rusqlite::Result<_>>().map_err(log_err)
    }

    /// The newest GREEN project-wide run (one that named no files).
    pub fn last_green_project_run(&self, project: &str) -> Result<Option<Run>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, kind, command, ok, wide, ran_at, scope, summary FROM verify_runs r \
                 WHERE r.project = ?1 AND r.ok = 1 AND r.wide = 1 \
                   AND NOT EXISTS (SELECT 1 FROM verify_run_files f WHERE f.run_id = r.id) \
                 ORDER BY r.ran_at DESC, r.id DESC LIMIT 1",
            )
            .map_err(log_err)?;
        let mut rows = stmt
            .query_map(rusqlite::params![project], |r| {
                Ok(Run {
                    id: r.get(0)?,
                    kind: r.get(1)?,
                    command: r.get(2)?,
                    ok: r.get::<_, i64>(3)? != 0,
                    wide: r.get::<_, i64>(4)? != 0,
                    ran_at: r.get(5)?,
                    files: Vec::new(),
                    scope: r.get(6)?,
                    summary: r.get(7)?,
                })
            })
            .map_err(log_err)?;
        match rows.next() {
            Some(row) => Ok(Some(row.map_err(log_err)?)),
            None => Ok(None),
        }
    }

    /// For every file a GREEN run named explicitly: when it was last covered, and
    /// by which command.
    fn green_file_coverage(&self, project: &str) -> Result<HashMap<String, (i64, String)>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT f.path, MAX(r.ran_at), r.command FROM verify_run_files f \
                 JOIN verify_runs r ON r.id = f.run_id \
                 WHERE r.project = ?1 AND r.ok = 1 GROUP BY f.path",
            )
            .map_err(log_err)?;
        let rows = stmt
            .query_map(rusqlite::params![project], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })
            .map_err(log_err)?;
        let mut out = HashMap::new();
        for row in rows {
            let (path, ts, cmd) = row.map_err(log_err)?;
            out.insert(path, (ts, cmd));
        }
        Ok(out)
    }
}

/// Default log path: `$ICODE_VERIFY_DB`, else `~/.icode/verify.db`.
pub fn default_path() -> PathBuf {
    if let Ok(p) = std::env::var("ICODE_VERIFY_DB") {
        if !p.trim().is_empty() {
            return PathBuf::from(p);
        }
    }
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".icode").join("verify.db")
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Which source files changed since the last green run that covered them.
///
/// Truth about "changed" comes from the **disk** mtime, not the index: the index
/// is only as fresh as the last sync, and a report that trusts it would call an
/// edited file verified simply because the graph had not caught up. Files the
/// index knows but disk no longer has are skipped (they are the indexer's problem,
/// not evidence).
///
/// `limit` caps the returned list; `stale_count` stays exact.
///
/// KNOWN WINDOW: coverage is compared against when the run was RECORDED, which is
/// when it finished. A file edited WHILE a long suite was running therefore reads
/// as covered by it, though it was not. Closing that needs the run's start time,
/// which the recording caller does not always know; until it does, this is a
/// documented limit rather than a silent one.
pub fn stale_report(
    log: &VerifyLog,
    store: &SqliteCodeStore,
    root: &Path,
    project: &str,
    limit: usize,
) -> Result<StaleReport> {
    use icode_core::traits::CodeReadStore;

    let files = store.list_files(None, None, 100_000)?;
    let green_all = log.last_green_project_run(project)?;
    let green_at = green_all.as_ref().map(|r| r.ran_at).unwrap_or(i64::MIN);
    let per_file = log.green_file_coverage(project)?;

    let mut stale: Vec<StaleFile> = Vec::new();
    let mut checked = 0usize;

    for f in &files {
        let abs = if Path::new(&f.path).is_absolute() {
            PathBuf::from(&f.path)
        } else {
            root.join(&f.path)
        };
        let Ok(meta) = std::fs::metadata(&abs) else {
            continue; // gone from disk — drift, not evidence
        };
        checked += 1;
        let changed_at = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);

        let targeted = per_file.get(&f.path);
        let (covered_at, command) = match (targeted, &green_all) {
            (Some((ts, cmd)), Some(all)) if *ts >= all.ran_at => (*ts, Some(cmd.clone())),
            (Some((ts, cmd)), None) => (*ts, Some(cmd.clone())),
            (_, Some(all)) => (green_at, Some(all.command.clone())),
            (None, None) => (i64::MIN, None),
        };

        // `>=`, not `>`: when a file's mtime lands on the same millisecond as the
        // run, we call it UNVERIFIED. Ambiguity resolves toward showing the file —
        // a spurious "re-run this" costs a moment, a spurious all-clear costs the
        // thing this module exists to prevent.
        if changed_at >= covered_at {
            stale.push(StaleFile {
                path: f.path.clone(),
                changed_at,
                last_verified_at: (covered_at != i64::MIN).then_some(covered_at),
                last_command: command,
                covering_tests: Vec::new(),
            });
        }
    }

    stale.sort_by_key(|f| std::cmp::Reverse(f.changed_at));
    let stale_count = stale.len();
    stale.truncate(limit);

    // Fill in what to run, only for the files actually reported — resolving tests
    // is a graph walk per file, and doing it for a whole unverified repo would turn
    // a status call into a scan.
    for s in &mut stale {
        s.covering_tests = store
            .find_tests_covering_file(&s.path, 5)
            .map(|tests| {
                let mut names: Vec<String> = tests.into_iter().map(|t| t.qualified_name).collect();
                names.sort();
                names.dedup();
                names.truncate(10);
                names
            })
            .unwrap_or_default();
    }

    Ok(StaleReport {
        all_verified: stale_count == 0,
        stale,
        stale_count,
        files_checked: checked,
        last_green_run: green_all,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(project: &str, cmd: &str, ok: bool, files: &[&str]) -> NewRun {
        NewRun {
            project: project.to_string(),
            kind: RunKind::Test,
            command: cmd.to_string(),
            ok,
            files: files.iter().map(|s| s.to_string()).collect(),
            wide: files.is_empty(),
            scope: None,
            summary: None,
        }
    }

    #[test]
    fn records_and_reads_back_a_run_with_its_files() {
        let log = VerifyLog::open_in_memory().unwrap();
        log.record(&run("demo", "cargo test -p x", true, &["src/a.rs", "src/b.rs"]))
            .unwrap();
        let runs = log.recent("demo", 10).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].command, "cargo test -p x");
        assert!(runs[0].ok);
        assert_eq!(runs[0].files, vec!["src/a.rs", "src/b.rs"]);
    }

    #[test]
    fn a_targeted_run_is_never_read_as_project_wide() {
        let log = VerifyLog::open_in_memory().unwrap();
        log.record(&run("demo", "cargo test auth::", true, &["src/auth.rs"]))
            .unwrap();
        // Naming files means the run covered THOSE files only.
        assert!(log.last_green_project_run("demo").unwrap().is_none());
        let cov = log.green_file_coverage("demo").unwrap();
        assert!(cov.contains_key("src/auth.rs"));
        assert!(!cov.contains_key("src/other.rs"));
    }

    #[test]
    fn a_red_run_is_recorded_but_never_counts_as_coverage() {
        let log = VerifyLog::open_in_memory().unwrap();
        log.record(&run("demo", "cargo test", false, &[])).unwrap();
        assert_eq!(log.recent("demo", 10).unwrap().len(), 1, "kept as history");
        assert!(
            log.last_green_project_run("demo").unwrap().is_none(),
            "a failing run must not read as verified"
        );
    }

    #[test]
    fn a_narrowed_run_naming_no_files_gives_no_coverage() {
        let log = VerifyLog::open_in_memory().unwrap();
        let mut r = run("demo", "cargo test my_one_test", true, &[]);
        r.wide = false; // a filter was passed: this proves nothing about the repo
        log.record(&r).unwrap();
        assert_eq!(log.recent("demo", 10).unwrap().len(), 1, "kept as history");
        assert!(
            log.last_green_project_run("demo").unwrap().is_none(),
            "a filtered run must never read as a green project"
        );
    }

    #[test]
    fn projects_are_isolated() {
        let log = VerifyLog::open_in_memory().unwrap();
        log.record(&run("a", "cargo test", true, &[])).unwrap();
        assert!(log.last_green_project_run("b").unwrap().is_none());
        assert_eq!(log.recent("b", 10).unwrap().len(), 0);
    }

    /// An agent's Bash call is often a multi-line script. Stored verbatim it turns
    /// the status report into pasted code with the verdicts buried in it.
    #[test]
    fn a_multiline_command_is_stored_as_one_bounded_line() {
        let log = VerifyLog::open_in_memory().expect("log");
        let script = "export FOO=1\ncargo test --workspace 2>&1 | tail -20\necho done";
        log.record(&run("p", script, true, &[])).expect("record");

        let stored = &log.recent("p", 1).expect("recent")[0].command;
        assert!(!stored.contains('\n'), "no newlines survive: {stored:?}");
        assert!(stored.contains("cargo test --workspace"), "still identifiable: {stored:?}");
        assert!(stored.chars().count() <= MAX_COMMAND_CHARS + 1, "bounded: {stored:?}");
    }

    #[test]
    fn summary_is_truncated_not_rejected() {
        let log = VerifyLog::open_in_memory().unwrap();
        let long = "x".repeat(MAX_SUMMARY_CHARS * 2);
        let mut r = run("demo", "cargo test", false, &[]);
        r.summary = Some(long);
        log.record(&r).unwrap();
        let stored = log.recent("demo", 1).unwrap().remove(0).summary.unwrap();
        assert!(stored.chars().count() <= MAX_SUMMARY_CHARS + 1);
        assert!(stored.ends_with('…'));
    }

    #[test]
    fn run_kind_parses_loosely_and_falls_back_to_other() {
        assert_eq!(RunKind::parse("TESTS"), RunKind::Test);
        assert_eq!(RunKind::parse("clippy"), RunKind::Lint);
        assert_eq!(RunKind::parse("deploy"), RunKind::Other);
    }
}
