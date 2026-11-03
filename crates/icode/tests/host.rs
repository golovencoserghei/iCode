//! Bin-level tests of `icode host`, the one-shot JSON API an IDE host drives.
//!
//! Hermetic: `$HOME` is a tempdir (so the central db is `$HOME/.icode/icode.db`),
//! rows are seeded by hand through rusqlite (memories + the fts/bridge rows, no
//! vectors), and the embedder is either absent (`ICODE_EMBED_BACKEND=none`) or the
//! deterministic offline fake (`ICODE_EMBED_BACKEND=fake`) — never a live Ollama.
//! Every test asserts a concrete outcome.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use rusqlite::params;
use serde_json::{json, Value};

fn icode_bin() -> &'static str {
    env!("CARGO_BIN_EXE_icode")
}

struct Run {
    reply: Value,
    stdout: String,
    stderr: String,
    code: i32,
}

/// Run `icode host [extra…]` with `stdin`, `$HOME` = `home`, the given embed backend.
fn host_raw(home: &Path, backend: &str, extra: &[&str], stdin: &str) -> Run {
    let mut child = Command::new(icode_bin())
        .arg("host")
        .args(extra)
        .env("HOME", home)
        .env_remove("ICODE_DB_PATH")
        .env("ICODE_EMBED_BACKEND", backend)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn icode host");
    child.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
    let out = child.wait_with_output().expect("wait icode host");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let reply = serde_json::from_str(stdout.trim()).unwrap_or(Value::Null);
    Run {
        reply,
        stdout,
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        code: out.status.code().unwrap_or(-1),
    }
}

/// One op; asserts the transport invariants (exit 0, empty stderr, one JSON line).
fn host_with(home: &Path, backend: &str, op: &str, args: Value) -> Value {
    let req = json!({ "api": 1, "op": op, "args": args }).to_string();
    let r = host_raw(home, backend, &[], &req);
    assert_eq!(r.code, 0, "{op}: exit code");
    assert!(r.stderr.is_empty(), "{op}: stderr must be empty, got {:?}", r.stderr);
    assert_eq!(r.stdout.trim().lines().count(), 1, "{op}: exactly one reply line: {:?}", r.stdout);
    assert_eq!(r.reply["api"], 1, "{op}: {}", r.stdout);
    r.reply
}

fn host(home: &Path, op: &str, args: Value) -> Value {
    host_with(home, "none", op, args)
}

fn ok_data(v: Value) -> Value {
    assert_eq!(v["ok"], true, "expected ok reply, got {v}");
    v["data"].clone()
}

fn err_code(v: &Value) -> String {
    assert_eq!(v["ok"], false, "expected an error reply, got {v}");
    v["error"]["code"].as_str().unwrap_or_default().to_string()
}

fn db_path(home: &Path) -> PathBuf {
    home.join(".icode").join("icode.db")
}

/// Create the central db schema (the hook's open applies it), return its path.
fn ensure_db(home: &Path) -> PathBuf {
    let out = Command::new(icode_bin())
        .args(["hook", "precompact", "--project", "demo"])
        .env("HOME", home)
        .env_remove("ICODE_DB_PATH")
        .output()
        .expect("spawn hook");
    assert!(out.status.success());
    db_path(home)
}

struct Seed<'a> {
    id: &'a str,
    project: &'a str,
    content: &'a str,
    category: &'a str,
    importance: f64,
    tags: &'a [&'a str],
    created_at: &'a str,
}

impl Default for Seed<'_> {
    fn default() -> Self {
        Seed {
            id: "",
            project: "demo",
            content: "",
            category: "decision",
            importance: 3.0,
            tags: &[],
            created_at: "2026-10-01T00:00:00+00:00",
        }
    }
}

/// Insert a memory row plus its bridge and fts rows (what `add` writes, minus the
/// vector), so lexical retrieval sees it.
fn seed(db: &Path, s: Seed) {
    let conn = rusqlite::Connection::open(db).unwrap();
    let tags = serde_json::to_string(s.tags).unwrap();
    conn.execute(
        "INSERT INTO memories (id, project, content, category, tags, importance, status, \
         access_count, created_at, last_accessed_at, content_hash) \
         VALUES (?1,?2,?3,?4,?5,?6,'active',0,?7,?7,'seed')",
        params![s.id, s.project, s.content, s.category, tags, s.importance, s.created_at],
    )
    .unwrap();
    conn.execute("INSERT INTO mem_rowid (mem_id) VALUES (?1)", params![s.id]).unwrap();
    let rowid = conn.last_insert_rowid();
    conn.execute("INSERT INTO fts_memory (rowid, content) VALUES (?1, ?2)", params![rowid, s.content])
        .unwrap();
}

fn row<T: rusqlite::types::FromSql>(db: &Path, sql: &str, id: &str) -> T {
    let conn = rusqlite::Connection::open(db).unwrap();
    conn.query_row(sql, params![id], |r| r.get(0)).unwrap()
}

fn count(db: &Path, sql: &str) -> i64 {
    let conn = rusqlite::Connection::open(db).unwrap();
    conn.query_row(sql, [], |r| r.get(0)).unwrap()
}

// ──────────────────────────── transport ────────────────────────────

#[test]
fn capabilities_reports_api_version_and_ops() {
    let home = tempfile::tempdir().unwrap();
    let data = ok_data(host(home.path(), "capabilities", json!({})));
    assert_eq!(data["api"], 1);
    assert_eq!(data["version"], env!("CARGO_PKG_VERSION"));
    let ops: Vec<&str> = data["ops"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
    for op in ["capabilities", "project", "prime", "memory.list", "memory.get", "memory.update", "impact"] {
        assert!(ops.contains(&op), "missing {op} in {ops:?}");
    }
}

#[test]
fn garbage_input_and_stray_arguments_still_get_a_json_reply_with_exit_0() {
    let home = tempfile::tempdir().unwrap();
    for (extra, stdin, code) in [
        (&[][..], "", "bad_request"),
        (&[][..], "{not json", "bad_request"),
        (&[][..], r#"{"api":7,"op":"capabilities"}"#, "bad_api"),
        (&[][..], r#"{"api":1,"op":"drop-everything"}"#, "unknown_op"),
        (&["--bogus", "x"][..], r#"{"api":1,"op":"memory.list","args":{"project":"demo","limit":"ten"}}"#, "bad_args"),
    ] {
        let r = host_raw(home.path(), "none", extra, stdin);
        assert_eq!(r.code, 0, "{stdin}");
        assert!(r.stderr.is_empty(), "stderr for {stdin}: {}", r.stderr);
        assert_eq!(err_code(&r.reply), code, "{stdin}");
    }
}

// ──────────────────────────── read-only mode ────────────────────────────

#[test]
fn reads_never_create_the_central_db() {
    let home = tempfile::tempdir().unwrap();
    let data = ok_data(host(home.path(), "memory.list", json!({"project": "demo"})));
    assert_eq!(data["items"], json!([]));
    let data = ok_data(host(home.path(), "prime", json!({"project": "demo", "kind": "session-start"})));
    assert_eq!(data["count"], 0);
    assert_eq!(data["text"], "");
    assert!(!home.path().join(".icode").exists(), "a read must not create ~/.icode");
}

#[test]
fn prime_is_hook_text_without_drafts_instructions_or_writes() {
    let home = tempfile::tempdir().unwrap();
    let db = ensure_db(home.path());
    seed(&db, Seed { id: "mem_P__x", project: "__developer__", content: "always answer in Russian", importance: 5.0, tags: &["l0"], ..Default::default() });
    seed(&db, Seed { id: "mem_D__demo", content: "chose SQLite over Postgres for the central store", ..Default::default() });
    seed(&db, Seed { id: "mem_S__demo", content: "Авто-сводка сессии (Stop-хук): черновик", category: "general", importance: 1.0, tags: &["auto-session", "sid:abcd1234"], created_at: "2026-10-02T00:00:00+00:00", ..Default::default() });

    let data = ok_data(host(home.path(), "prime", json!({"project": "demo", "kind": "session-start"})));
    let text = data["text"].as_str().unwrap();
    assert!(text.contains("always answer in Russian"), "{text}");
    assert!(text.contains("chose SQLite over Postgres"), "{text}");
    assert!(!text.contains("Авто-сводка"), "a low-importance draft must not be primed: {text}");
    assert!(!text.contains("project=\""), "no MCP-call instructions for a host: {text}");
    assert_eq!(data["count"], 2);
    // Pure read: no access warm-up.
    let accessed: i64 = row(&db, "SELECT access_count FROM memories WHERE id = ?1", "mem_D__demo");
    assert_eq!(accessed, 0);

    let data = ok_data(host(home.path(), "prime", json!({"project": "demo", "kind": "precompact"})));
    assert_eq!(data["count"], 1);
    assert!(data["text"].as_str().unwrap().contains("always answer in Russian"));

    assert_eq!(err_code(&host(home.path(), "prime", json!({"project": "demo", "kind": "other"}))), "bad_args");
}

// ──────────────────────────── project ────────────────────────────

#[test]
fn project_reports_name_root_and_source() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();

    // A plain repo, asked from a subdirectory.
    let repo = tmp.path().join("Alpha");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    std::fs::create_dir_all(repo.join("src/deep")).unwrap();
    let data = ok_data(host(&home, "project", json!({"cwd": repo.join("src/deep")})));
    assert_eq!(data["name"], "Alpha");
    assert_eq!(data["source"], "basename");
    assert_eq!(data["shared"], false);
    assert_eq!(data["root"], std::fs::canonicalize(&repo).unwrap().to_string_lossy().as_ref());

    // A linked worktree of it maps to the main checkout.
    let gitdir = repo.join(".git/worktrees/wt");
    std::fs::create_dir_all(&gitdir).unwrap();
    std::fs::write(gitdir.join("commondir"), "../..\n").unwrap();
    let wt = tmp.path().join("alpha-wt");
    std::fs::create_dir_all(&wt).unwrap();
    std::fs::write(wt.join(".git"), format!("gitdir: {}\n", gitdir.display())).unwrap();
    let data = ok_data(host(&home, "project", json!({"cwd": wt})));
    assert_eq!(data["name"], "Alpha");
    assert_eq!(data["source"], "worktree");

    // A marker names a shared bucket.
    std::fs::write(repo.join(".icode-project"), "# ecosystem\nSuite\n").unwrap();
    let data = ok_data(host(&home, "project", json!({"cwd": repo})));
    assert_eq!(data["name"], "Suite");
    assert_eq!(data["source"], "marker");
    assert_eq!(data["shared"], true);
}

// ──────────────────────────── memory ops ────────────────────────────

fn seeded_home() -> (tempfile::TempDir, PathBuf) {
    let home = tempfile::tempdir().unwrap();
    let db = ensure_db(home.path());
    seed(&db, Seed { id: "mem_A__demo", content: "демон индексатора перезапускается при исчезновении корня", ..Default::default() });
    seed(&db, Seed { id: "mem_B__demo", content: "todo: add a clippy step to CI", category: "todo", importance: 1.0, ..Default::default() });
    seed(&db, Seed { id: "mem_F__other", project: "other", content: "foreign project secret plan", ..Default::default() });
    (home, db)
}

#[test]
fn list_and_get_are_scoped_to_the_project() {
    let (home, _db) = seeded_home();
    let data = ok_data(host(home.path(), "memory.list", json!({"project": "demo"})));
    let ids: Vec<&str> = data["items"].as_array().unwrap().iter().filter_map(|i| i["id"].as_str()).collect();
    assert_eq!(ids.len(), 2);
    assert!(!ids.contains(&"mem_F__other"));
    let data = ok_data(host(home.path(), "memory.list", json!({"project": "demo", "category": "todo"})));
    assert_eq!(data["items"].as_array().unwrap().len(), 1);
    assert_eq!(data["items"][0]["category"], "todo");
    assert!(data["items"][0]["ageDays"].is_number());

    let data = ok_data(host(home.path(), "memory.get", json!({"project": "demo", "ids": ["mem_A__demo", "mem_gone__demo"]})));
    assert_eq!(data["items"].as_array().unwrap().len(), 1);
    assert_eq!(data["items"][0]["id"], "mem_A__demo");
    assert_eq!(
        err_code(&host(home.path(), "memory.get", json!({"project": "demo", "ids": ["mem_F__other"]}))),
        "foreign_record"
    );
}

#[test]
fn search_matches_inflected_cyrillic_words() {
    let (home, _db) = seeded_home();
    let data = ok_data(host(home.path(), "memory.search", json!({"project": "demo", "query": "демона"})));
    assert_eq!(data["mode"], "lexical");
    let items = data["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "{data}");
    assert_eq!(items[0]["id"], "mem_A__demo");
    assert!(items[0]["score"].as_f64().unwrap() > 0.0);
    // Scoped: the other project's record is never searched.
    let data = ok_data(host(home.path(), "memory.search", json!({"project": "demo", "query": "secret plan"})));
    assert_eq!(data["items"], json!([]));
}

#[test]
fn update_resolve_delete_check_ownership_and_work_without_an_embedder() {
    let (home, db) = seeded_home();
    let h = home.path();
    for (op, args) in [
        ("memory.update", json!({"project": "demo", "id": "mem_F__other", "importance": 5})),
        ("memory.resolve", json!({"project": "demo", "id": "mem_F__other", "reason": "x"})),
        ("memory.delete", json!({"project": "demo", "id": "mem_F__other"})),
    ] {
        assert_eq!(err_code(&host(h, op, args)), "foreign_record", "{op}");
    }
    assert_eq!(count(&db, "SELECT COUNT(*) FROM memories WHERE id='mem_F__other' AND status='active'"), 1);
    assert_eq!(err_code(&host(h, "memory.delete", json!({"project": "demo", "id": "mem_nope__demo"}))), "not_found");

    // Metadata needs no embedder.
    let data = ok_data(host(h, "memory.update", json!({"project": "demo", "id": "mem_B__demo", "importance": 5, "category": "decision", "tags": ["pinned"]})));
    assert_eq!(data["item"]["importance"], 5.0);
    assert_eq!(data["item"]["category"], "decision");
    assert_eq!(data["item"]["tags"], json!(["pinned"]));
    // New content does.
    assert_eq!(
        err_code(&host(h, "memory.update", json!({"project": "demo", "id": "mem_B__demo", "content": "rewritten"}))),
        "no_embedder"
    );
    assert_eq!(err_code(&host(h, "memory.update", json!({"project": "demo", "id": "mem_B__demo", "importance": 9}))), "bad_args");

    let data = ok_data(host(h, "memory.resolve", json!({"project": "demo", "id": "mem_A__demo", "reason": "wrong recall"})));
    assert_eq!(data["item"]["status"], "resolved");
    assert_eq!(data["item"]["resolveReason"], "wrong recall");

    let data = ok_data(host(h, "memory.delete", json!({"project": "demo", "id": "mem_B__demo"})));
    assert_eq!(data["deleted"], true);
    assert_eq!(count(&db, "SELECT COUNT(*) FROM memories WHERE id='mem_B__demo'"), 0);
    assert_eq!(count(&db, "SELECT COUNT(*) FROM mem_rowid WHERE mem_id='mem_B__demo'"), 0);
    // Every write is journaled next to the db.
    let wal = std::fs::read_to_string(h.join(".icode/wal.jsonl")).unwrap();
    assert!(wal.contains("mem_A__demo") && wal.contains("mem_B__demo"), "{wal}");
}

#[test]
fn content_update_re_embeds_with_the_fake_backend() {
    let (home, db) = seeded_home();
    let data = ok_data(host_with(
        home.path(),
        "fake",
        "memory.update",
        json!({"project": "demo", "id": "mem_B__demo", "content": "todo: add clippy and fmt steps to CI"}),
    ));
    assert_eq!(data["item"]["content"], "todo: add clippy and fmt steps to CI");
    let fts: String = row(
        &db,
        "SELECT f.content FROM fts_memory f JOIN mem_rowid b ON b.rowid = f.rowid WHERE b.mem_id = ?1",
        "mem_B__demo",
    );
    assert_eq!(fts, "todo: add clippy and fmt steps to CI");
}

#[test]
fn curate_is_a_dry_run_by_default() {
    let (home, db) = seeded_home();
    seed(&db, Seed { id: "mem_C__demo", content: "todo: add a clippy step to CI", category: "todo", importance: 1.0, created_at: "2026-10-03T00:00:00+00:00", ..Default::default() });
    let data = ok_data(host(home.path(), "memory.curate", json!({"project": "demo"})));
    assert_eq!(data["applied"], false);
    assert_eq!(data["duplicates"].as_array().unwrap().len(), 1, "{data}");
    assert_eq!(count(&db, "SELECT COUNT(*) FROM memories WHERE project='demo' AND status='resolved'"), 0);
    let data = ok_data(host(home.path(), "memory.curate", json!({"project": "demo", "apply": true})));
    assert_eq!(data["applied"], true);
    assert_eq!(count(&db, "SELECT COUNT(*) FROM memories WHERE project='demo' AND status='resolved'"), 1);
}

// ──────────────────────────── impact ────────────────────────────

#[test]
fn impact_without_an_index_creates_nothing() {
    let home = tempfile::tempdir().unwrap();
    let proj = tempfile::tempdir().unwrap();
    std::fs::write(proj.path().join("lib.rs"), "pub fn a() {}\n").unwrap();
    let data = ok_data(host(home.path(), "impact", json!({"root": proj.path(), "files": ["lib.rs"]})));
    assert_eq!(data["indexed"], false);
    assert!(!proj.path().join(".icode").exists(), "impact must not create an index dir");
}

#[test]
fn impact_reports_symbols_callers_and_memories() {
    let home = tempfile::tempdir().unwrap();
    let db = ensure_db(home.path());
    let proj = tempfile::tempdir().unwrap();
    let root = proj.path().join("widget");
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::write(
        root.join("parser.rs"),
        "pub fn parse(x: u32) -> u32 { helper(x) }\nfn helper(x: u32) -> u32 { x + 1 }\n#[test]\nfn parse_works() { let y = parse(1); assert_eq!(y, 2); }\n",
    )
    .unwrap();
    let out = Command::new(icode_bin()).arg("index").arg(&root).env("HOME", home.path()).env_remove("ICODE_DB_PATH").output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    seed(&db, Seed { id: "mem_I__widget", project: "widget", content: "the parser module must stay allocation-free", ..Default::default() });

    let data = ok_data(host(home.path(), "impact", json!({"root": root, "files": ["parser.rs"]})));
    assert_eq!(data["indexed"], true);
    assert_eq!(data["project"], "widget");
    let file = &data["files"][0];
    let helper = file["symbols"].as_array().unwrap().iter().find(|s| s["name"] == "helper").expect("helper symbol");
    assert_eq!(helper["callers"], 1);
    assert!(helper["tests"].as_u64().unwrap() >= 1, "{helper}");
    assert_eq!(file["memories"][0]["id"], "mem_I__widget");
}
