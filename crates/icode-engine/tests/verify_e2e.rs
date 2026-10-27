//! End-to-end tests for the verification ledger against a real indexed tree.
//!
//! The unit tests in `verify.rs` cover the log's own bookkeeping. What matters
//! here is the JOIN the feature exists for: disk mtime × recorded runs × the call
//! graph. These run without Ollama — the ledger is lexical/structural only.

use std::fs;
use std::path::Path;

use icode_engine::{index_path, stale_report, NewRun, RunKind, SqliteCodeStore, VerifyLog};

const SRC: &str = r#"
pub fn charge_card(amount: u32) -> u32 { amount * 2 }

pub fn checkout(amount: u32) -> u32 { charge_card(amount) }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkout_doubles() {
        assert_eq!(checkout(2), 4);
    }
}
"#;

/// A fresh indexed project holding one source file.
fn project(rel: &str, src: &str) -> (tempfile::TempDir, SqliteCodeStore) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("mkdir");
    }
    fs::write(&path, src).expect("write");
    let store = SqliteCodeStore::open(dir.path()).expect("open");
    index_path(dir.path(), &store).expect("index");
    (dir, store)
}

fn touch(path: &Path) {
    // Timestamps are millisecond-grained on both sides, and these tests run in
    // microseconds — without a beat, "edited after the run" and "run after the
    // edit" collapse into the same instant and the test would assert nothing.
    std::thread::sleep(std::time::Duration::from_millis(5));
    // Rewriting the file moves its mtime to now — the same signal an edit gives.
    let body = fs::read_to_string(path).expect("read");
    fs::write(path, format!("{body}\n// edited\n")).expect("write");
}

fn wide_run(project: &str, command: &str, ok: bool) -> NewRun {
    NewRun {
        project: project.to_string(),
        kind: RunKind::Test,
        command: command.to_string(),
        ok,
        files: Vec::new(),
        wide: true,
        scope: None,
        summary: None,
    }
}

#[test]
fn a_file_with_no_run_at_all_is_reported_unverified() {
    let (dir, store) = project("src/pay.rs", SRC);
    let log = VerifyLog::open_in_memory().expect("log");

    let rep = stale_report(&log, &store, dir.path(), "demo", 20).expect("report");

    assert_eq!(rep.files_checked, 1);
    assert_eq!(rep.stale_count, 1, "nothing has ever been run");
    assert!(!rep.all_verified);
    assert!(rep.last_green_run.is_none());
    assert_eq!(
        rep.stale[0].path,
        dir.path().join("src/pay.rs").to_string_lossy(),
        "the report speaks the index's path form (absolute), so a caller can \
         hand a path straight back to the other tools"
    );
    assert!(
        rep.stale[0].last_verified_at.is_none(),
        "never verified → no timestamp"
    );
}

#[test]
fn a_green_project_run_clears_everything_until_the_next_edit() {
    let (dir, store) = project("src/pay.rs", SRC);
    let log = VerifyLog::open_in_memory().expect("log");

    std::thread::sleep(std::time::Duration::from_millis(5));
    log.record(&wide_run("demo", "cargo test", true)).expect("record");
    let rep = stale_report(&log, &store, dir.path(), "demo", 20).expect("report");
    assert!(rep.all_verified, "a green full run covers the tree");
    assert_eq!(rep.stale_count, 0);

    // ...and the moment the file changes, the evidence is stale again.
    touch(&dir.path().join("src/pay.rs"));
    let rep = stale_report(&log, &store, dir.path(), "demo", 20).expect("report");
    assert_eq!(rep.stale_count, 1, "the edit outdates the run");
    assert!(rep.stale[0].last_verified_at.is_some());
    assert_eq!(rep.stale[0].last_command.as_deref(), Some("cargo test"));
}

#[test]
fn the_report_names_the_tests_to_re_run() {
    let (dir, store) = project("src/pay.rs", SRC);
    let log = VerifyLog::open_in_memory().expect("log");

    let rep = stale_report(&log, &store, dir.path(), "demo", 20).expect("report");
    let tests = &rep.stale[0].covering_tests;

    // `checkout_doubles` calls `checkout`, which calls `charge_card` — the test
    // reaches the file transitively AND is defined in it. Either way it must be
    // the thing the report tells you to run.
    assert!(
        tests.iter().any(|t| t.contains("checkout_doubles")),
        "expected the covering test in {tests:?}"
    );
}

#[test]
fn a_red_run_does_not_clear_anything() {
    let (dir, store) = project("src/pay.rs", SRC);
    let log = VerifyLog::open_in_memory().expect("log");

    log.record(&wide_run("demo", "cargo test", false)).expect("record");

    let rep = stale_report(&log, &store, dir.path(), "demo", 20).expect("report");
    assert_eq!(rep.stale_count, 1, "a failing run is not verification");
    assert!(rep.last_green_run.is_none());
}

#[test]
fn a_targeted_run_clears_only_the_files_it_named() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::create_dir_all(dir.path().join("src")).expect("mkdir");
    fs::write(dir.path().join("src/pay.rs"), SRC).expect("write");
    fs::write(dir.path().join("src/other.rs"), "pub fn untouched() -> u32 { 7 }\n").expect("write");
    let store = SqliteCodeStore::open(dir.path()).expect("open");
    index_path(dir.path(), &store).expect("index");
    let log = VerifyLog::open_in_memory().expect("log");
    std::thread::sleep(std::time::Duration::from_millis(5));

    log.record(&NewRun {
        project: "demo".into(),
        kind: RunKind::Test,
        command: "cargo test pay::".into(),
        ok: true,
        files: vec![dir.path().join("src/pay.rs").to_string_lossy().to_string()],
        wide: false,
        scope: Some("pay::".into()),
        summary: None,
    })
    .expect("record");

    let rep = stale_report(&log, &store, dir.path(), "demo", 20).expect("report");
    let stale: Vec<&str> = rep.stale.iter().map(|s| s.path.as_str()).collect();
    assert_eq!(
        stale,
        vec![dir.path().join("src/other.rs").to_string_lossy().as_ref()],
        "only the named file is covered; the rest stays unproven"
    );
}

#[test]
fn a_file_deleted_from_disk_is_drift_not_evidence() {
    let (dir, store) = project("src/pay.rs", SRC);
    let log = VerifyLog::open_in_memory().expect("log");
    fs::remove_file(dir.path().join("src/pay.rs")).expect("rm");

    let rep = stale_report(&log, &store, dir.path(), "demo", 20).expect("report");
    assert_eq!(rep.files_checked, 0, "a vanished file is the indexer's problem");
    assert_eq!(rep.stale_count, 0);
}

#[test]
fn evidence_is_per_project() {
    let (dir, store) = project("src/pay.rs", SRC);
    let log = VerifyLog::open_in_memory().expect("log");

    log.record(&wide_run("other-project", "cargo test", true)).expect("record");

    let rep = stale_report(&log, &store, dir.path(), "demo", 20).expect("report");
    assert_eq!(
        rep.stale_count, 1,
        "another project's green run must not vouch for this one"
    );
}

#[test]
fn a_homonym_in_another_file_does_not_pull_in_its_tests() {
    // The call graph is keyed by NAME: two files defining `rank` are one node.
    // Reporting `graph.rs` as needing the memory tests re-run — because both
    // define `rank` — is the wrong answer that makes a report unreadable.
    let dir = tempfile::tempdir().expect("tempdir");
    fs::create_dir_all(dir.path().join("src")).expect("mkdir");
    fs::write(
        dir.path().join("src/graph.rs"),
        "pub fn rank(x: u32) -> u32 { x }\npub fn centrality(x: u32) -> u32 { rank(x) }\n",
    )
    .expect("write");
    fs::write(
        dir.path().join("src/memory.rs"),
        r#"
pub fn rank(x: u32) -> u32 { x + 1 }

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn memory_rank_adds_one() { assert_eq!(rank(1), 2); }
}
"#,
    )
    .expect("write");
    let store = SqliteCodeStore::open(dir.path()).expect("open");
    index_path(dir.path(), &store).expect("index");

    let tests = store
        .find_tests_covering_file(
            dir.path().join("src/graph.rs").to_string_lossy().as_ref(),
            5,
        )
        .expect("coverage");
    let names: Vec<&str> = tests.iter().map(|t| t.test.as_str()).collect();
    assert!(
        !names.contains(&"memory_rank_adds_one"),
        "the other file's test must not be credited to this one: {names:?}"
    );
}
