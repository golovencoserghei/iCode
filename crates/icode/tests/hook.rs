//! Bin-level integration test for the Claude Code lifecycle hooks
//! (`icode hook session-start` / `precompact` / `config`).
//!
//! These hooks must (1) print VALID `hookSpecificOutput` JSON, (2) NEVER fail
//! (exit 0 even with no Ollama / no db), and — critically — (3) re-inject L0
//! "always-on" rules at compaction WITHOUT an embedding backend, by reading the
//! central db's `memories` rows directly (read-only `list`, no vectors).
//!
//! The test deliberately needs NO Ollama: it points the binary's central-db path
//! at a temp `$HOME/.icode/icode.db`, lets the binary create the schema, seeds one
//! L0 row by hand via rusqlite (the `add` write path WOULD need an embedder), and
//! then drives the real binary over it. That proves the no-Ollama L0 retrieval.

use std::path::{Path, PathBuf};
use std::process::Command;

use rusqlite::params;

/// Path to the built `icode` binary under test (Cargo sets this for us).
fn icode_bin() -> &'static str {
    env!("CARGO_BIN_EXE_icode")
}

/// Run `icode hook <args…>` with `$HOME` overridden to `home`, returning
/// (stdout, exit_code). `$HOME/.icode/icode.db` is the central db the binary uses.
fn run_hook(home: &Path, args: &[&str]) -> (String, i32) {
    let out = Command::new(icode_bin())
        .args(["hook"])
        .args(args)
        .env("HOME", home)
        .output()
        .expect("spawn icode hook");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    (stdout, out.status.code().unwrap_or(-1))
}

/// Materialise the central db (schema) by running a hook once, then return the
/// db path. Running any hook against a fresh `$HOME` calls `open_readonly`, which
/// applies the schema — so afterwards the `memories` table exists to seed.
fn ensure_db(home: &Path) -> PathBuf {
    // A precompact over an empty db is harmless and creates `~/.icode/icode.db`.
    let _ = run_hook(home, &["precompact", "--project", "demo"]);
    home.join(".icode").join("icode.db")
}

/// Insert one memory row directly into `memories` (no vectors needed: the hook
/// `list` path reads this table only). Mirrors the columns the store writes.
fn seed_memory(db: &Path, project: &str, content: &str, importance: f64, tags: &[&str]) {
    let conn = rusqlite::Connection::open(db).expect("open central db for seeding");
    let tags_json = format!(
        "[{}]",
        tags.iter().map(|t| format!("\"{t}\"")).collect::<Vec<_>>().join(",")
    );
    let now = "2026-06-25T00:00:00+00:00";
    let id = format!("mem_TESTSEED__{project}");
    conn.execute(
        "INSERT INTO memories \
         (id, project, content, category, tags, importance, status, \
          access_count, created_at, last_accessed_at, content_hash) \
         VALUES (?1,?2,?3,'general',?4,?5,'active',0,?6,?6,'seedhash')",
        params![id, project, content, tags_json, importance, now],
    )
    .expect("seed memory row");
}

/// Minimal check that a hook line is a JSON object exposing `hookSpecificOutput`
/// with a string `additionalContext`. We avoid a JSON dep in the bin's tests by
/// asserting the structural markers are present and the braces balance.
fn assert_valid_hook_json(s: &str, event: &str) {
    let s = s.trim();
    assert!(s.starts_with('{') && s.ends_with('}'), "not a JSON object: {s}");
    assert!(s.contains("\"hookSpecificOutput\""), "missing hookSpecificOutput: {s}");
    assert!(
        s.contains(&format!("\"hookEventName\":\"{event}\"")),
        "missing/wrong hookEventName ({event}): {s}"
    );
    assert!(s.contains("\"additionalContext\""), "missing additionalContext: {s}");
    // Braces balance (the hand-built JSON has no unescaped stray braces).
    let opens = s.matches('{').count();
    let closes = s.matches('}').count();
    assert_eq!(opens, closes, "unbalanced braces: {s}");
}

#[test]
fn precompact_reinjects_l0_without_ollama() {
    let home = tempfile::tempdir().expect("tempdir");
    let db = ensure_db(home.path());

    // An L0 developer-profile rule: importance 5 + tag "l0".
    seed_memory(
        &db,
        icode_core::ids::DEVELOPER_PROJECT,
        "always answer in Russian",
        5.0,
        &["l0"],
    );
    // A non-L0 project note that must NOT be re-injected (importance 1, no L0 tag).
    seed_memory(&db, "demo", "ordinary project note, not critical", 1.0, &[]);

    let (stdout, code) = run_hook(home.path(), &["precompact", "--project", "demo"]);
    assert_eq!(code, 0, "precompact must exit 0");
    assert_valid_hook_json(&stdout, "PreCompact");
    // The L0 rule survives compaction…
    assert!(
        stdout.contains("always answer in Russian"),
        "L0 rule must be re-injected, got: {stdout}"
    );
    // …and was retrieved WITHOUT any embedder (we never ran Ollama; the row has no
    // vector — only the read-only `list` path could have surfaced it).
    // The ordinary note is filtered out by the L0 gate.
    assert!(
        !stdout.contains("ordinary project note"),
        "non-L0 note must NOT be re-injected, got: {stdout}"
    );
}

#[test]
fn precompact_empty_store_is_valid_json_exit_0() {
    let home = tempfile::tempdir().expect("tempdir");
    let _ = ensure_db(home.path());
    // No L0 rows seeded → additionalContext is empty, still valid JSON, exit 0.
    let (stdout, code) = run_hook(home.path(), &["precompact", "--project", "demo"]);
    assert_eq!(code, 0);
    assert_valid_hook_json(&stdout, "PreCompact");
    assert!(stdout.contains("\"additionalContext\":\"\""), "expected empty context: {stdout}");
}

#[test]
fn session_start_emits_valid_json() {
    let home = tempfile::tempdir().expect("tempdir");
    let db = ensure_db(home.path());
    seed_memory(
        &db,
        icode_core::ids::DEVELOPER_PROJECT,
        "always answer in Russian",
        5.0,
        &["l0"],
    );
    seed_memory(&db, "demo", "implemented the hook subcommands", 0.0, &[]);

    let (stdout, code) = run_hook(home.path(), &["session-start", "--project", "demo"]);
    assert_eq!(code, 0, "session-start must exit 0");
    assert_valid_hook_json(&stdout, "SessionStart");
    assert!(
        stdout.contains("always answer in Russian"),
        "profile rule should appear in session start: {stdout}"
    );
    assert!(
        stdout.contains("implemented the hook subcommands"),
        "project memory should appear in session start: {stdout}"
    );
}

#[test]
fn session_start_empty_falls_back_to_trigger() {
    let home = tempfile::tempdir().expect("tempdir");
    let _ = ensure_db(home.path());
    // No memory at all → behavioural fallback, still valid JSON, exit 0.
    let (stdout, code) = run_hook(home.path(), &["session-start", "--project", "demo"]);
    assert_eq!(code, 0);
    assert_valid_hook_json(&stdout, "SessionStart");
    assert!(
        stdout.contains("iCode cross-session memory") || stdout.contains("session_start"),
        "expected behavioural fallback, got: {stdout}"
    );
}

#[test]
fn session_start_resolves_project_from_cwd() {
    let home = tempfile::tempdir().expect("tempdir");
    let db = ensure_db(home.path());
    // Seed under a project named after a cwd basename; pass --cwd, not --project.
    seed_memory(&db, "myproj", "context for myproj resolved from cwd", 0.0, &[]);
    let cwd = home.path().join("workspaces").join("myproj");
    std::fs::create_dir_all(&cwd).unwrap();

    let (stdout, code) = run_hook(
        home.path(),
        &["session-start", "--cwd", cwd.to_str().unwrap()],
    );
    assert_eq!(code, 0);
    assert_valid_hook_json(&stdout, "SessionStart");
    assert!(
        stdout.contains("context for myproj resolved from cwd"),
        "project must be resolved from --cwd basename: {stdout}"
    );
}

#[test]
fn hook_config_prints_valid_json_snippet() {
    let home = tempfile::tempdir().expect("tempdir");
    let (stdout, code) = run_hook(home.path(), &["config"]);
    assert_eq!(code, 0);
    let s = stdout.trim();
    assert!(s.starts_with('{') && s.ends_with('}'), "config must be a JSON object: {s}");
    assert!(s.contains("\"hooks\""), "config must contain a hooks block: {s}");
    assert!(s.contains("SessionStart"), "config must wire SessionStart: {s}");
    assert!(s.contains("PreCompact"), "config must wire PreCompact: {s}");
    let opens = s.matches('{').count();
    let closes = s.matches('}').count();
    assert_eq!(opens, closes, "config JSON braces must balance: {s}");
}
