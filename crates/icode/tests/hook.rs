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
    assert!(s.contains("\"Stop\""), "config must wire Stop: {s}");
    let opens = s.matches('{').count();
    let closes = s.matches('}').count();
    assert_eq!(opens, closes, "config JSON braces must balance: {s}");
}

// ──────────────────────────── hook stop (session_end safety net) ────────────────────────────

/// Run `icode hook stop <args…>` feeding `stdin`; returns (stdout, exit_code).
fn run_hook_stop(home: &Path, args: &[&str], stdin: &str) -> (String, i32) {
    use std::io::Write;
    let mut child = Command::new(icode_bin())
        .args(["hook", "stop"])
        .args(args)
        .env("HOME", home)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn icode hook stop");
    child
        .stdin
        .take()
        .expect("stdin piped")
        .write_all(stdin.as_bytes())
        .expect("write hook stdin");
    let out = child.wait_with_output().expect("wait icode hook stop");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        out.status.code().unwrap_or(-1),
    )
}

/// Count `demo`-project rows tagged with this session's auto-summary marker.
fn count_auto_rows(db: &Path, sid_tag: &str) -> i64 {
    let conn = rusqlite::Connection::open(db).expect("open central db");
    conn.query_row(
        "SELECT COUNT(*) FROM memories WHERE project='demo' AND tags LIKE ?1",
        params![format!("%{sid_tag}%")],
        |r| r.get(0),
    )
    .expect("count auto rows")
}

#[test]
fn stop_hook_is_silent_noop_on_garbage_stdin() {
    let home = tempfile::tempdir().expect("tempdir");
    for bad in ["", "not json", r#"{"session_id":"x"}"#] {
        let (stdout, code) = run_hook_stop(home.path(), &["--project", "demo"], bad);
        assert_eq!(code, 0, "stop must exit 0 on bad stdin {bad:?}");
        assert!(stdout.is_empty(), "stop must print nothing, got: {stdout}");
    }
}

#[test]
fn stop_hook_session_end_deletes_the_draft_without_ollama() {
    let home = tempfile::tempdir().expect("tempdir");
    let db = ensure_db(home.path());

    // A draft auto-summary left by an earlier Stop firing of session abc12345-….
    seed_memory(
        &db,
        "demo",
        "Авто-сводка сессии: черновик, должен быть удалён",
        1.0,
        &["auto-session", "sid:abc12345"],
    );
    assert_eq!(count_auto_rows(&db, "sid:abc12345"), 1);

    // The transcript shows the agent DID call session_end this time.
    let transcript = home.path().join("t.jsonl");
    std::fs::write(
        &transcript,
        r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"mcp__icode__session_end","input":{}}]}}"#,
    )
    .unwrap();

    let stdin = format!(
        r#"{{"session_id":"abc12345-0000","transcript_path":"{}","cwd":"/tmp/demo"}}"#,
        transcript.display()
    );
    let (stdout, code) = run_hook_stop(home.path(), &["--project", "demo"], &stdin);
    assert_eq!(code, 0);
    assert!(stdout.is_empty(), "stop must print nothing, got: {stdout}");
    // The delete path needs NO embedder — the draft is gone even without Ollama.
    assert_eq!(count_auto_rows(&db, "sid:abc12345"), 0, "draft must be deleted");
}

#[test]
fn stop_hook_upserts_one_auto_summary_per_session() {
    let home = tempfile::tempdir().expect("tempdir");
    let db = ensure_db(home.path());

    let transcript = home.path().join("t.jsonl");
    let long = "Починил парсер конфига: пустой файл теперь трактуется как пустой словарь настроек вместо падения с TypeError; добавлен регрессионный тест на пустой ввод и обновлена документация.";
    let user_line = r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"почини баг с пустым конфигом"}]}}"#;
    let assistant_line = format!(
        r#"{{"type":"assistant","message":{{"content":[{{"type":"text","text":"{long}"}}]}}}}"#
    );
    std::fs::write(&transcript, format!("{user_line}\n{assistant_line}\n")).unwrap();

    let stdin = format!(
        r#"{{"session_id":"feed1234-0000","transcript_path":"{}","cwd":"/tmp/demo"}}"#,
        transcript.display()
    );

    // Exit 0 in EVERY environment. With a live embedder (Ollama up) the summary
    // must exist and stay a SINGLE row across repeated Stop firings; with Ollama
    // down the hook degrades to a silent no-op (0 rows) — both are correct.
    let (stdout, code) = run_hook_stop(home.path(), &["--project", "demo"], &stdin);
    assert_eq!(code, 0);
    assert!(stdout.is_empty(), "stop must print nothing, got: {stdout}");
    let after_first = count_auto_rows(&db, "sid:feed1234");
    assert!(after_first <= 1, "at most one auto-summary, got {after_first}");

    let (_, code) = run_hook_stop(home.path(), &["--project", "demo"], &stdin);
    assert_eq!(code, 0);
    let after_second = count_auto_rows(&db, "sid:feed1234");
    assert_eq!(
        after_first, after_second,
        "repeated Stop firings must upsert, not multiply rows"
    );

    if after_first == 1 {
        let conn = rusqlite::Connection::open(&db).unwrap();
        let content: String = conn
            .query_row(
                "SELECT content FROM memories WHERE project='demo' AND tags LIKE '%sid:feed1234%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(content.contains("Задача: «почини баг"), "summary must cite the task: {content}");
        assert!(content.contains("Итог:"), "summary must carry the outcome: {content}");
    }
}
