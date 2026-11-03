//! `icode host` — the one-shot JSON API an IDE host (FlowCode) drives.
//!
//! Contract (api = 1):
//!
//! * request: ONE JSON object on stdin, `{"api":1,"op":"<op>","args":{…}}`;
//! * reply: ONE JSON object on stdout, `{"api":1,"ok":true,"data":…}` or
//!   `{"api":1,"ok":false,"error":{"code":"…","message":"…"}}`;
//! * the exit code is ALWAYS 0 and stderr stays empty — a malformed request, a
//!   missing db, a panic: every failure is an `ok:false` reply. The host treats any
//!   reply it cannot use as "no memory this turn", so iCode can never break a turn.
//!
//! Reads open the central db `SQLITE_OPEN_READ_ONLY` with a short busy timeout and
//! never write anything back (no DDL, no `user_version`, no access warm-up); the
//! code index is opened read-only and never created. Writes use a longer busy
//! timeout. Every op that touches one record checks that it belongs to
//! `args.project` (`foreign_record` otherwise).

use std::collections::HashSet;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use chrono::{DateTime, Utc};
use icode_core::ids::MemoryId;
use icode_core::model::{Category, MemoryRecord, MemoryStatus};
use icode_core::traits::{CodeReadStore, Embedder, ReadableMemoryStore, WritableMemoryStore};
use icode_engine::{SqliteCodeStore, SqliteMemoryStore, WalStore};
use serde_json::{json, Map, Value};

/// The contract generation this binary speaks.
pub const API: u64 = 1;
/// Busy timeout of a read: a host call sits on the hot path of a chat turn.
const READ_BUSY_MS: u64 = 200;
/// Busy timeout of a write.
const WRITE_BUSY_MS: u64 = 2000;
/// Upper bound on the request size.
const MAX_REQUEST_BYTES: u64 = 1 << 20;
/// HTTP timeout for an embed call made by a write (`memory.update` content).
const WRITE_EMBED_TIMEOUT_MS: u64 = 8000;
/// HTTP timeout for the query embed of a semantic recall — the host's whole turn
/// budget is 1.5 s, so a slow backend degrades to lexical instead of waiting.
const RECALL_EMBED_TIMEOUT_MS: u64 = 1000;

/// The ops this binary implements, in the order `capabilities` lists them.
const OPS: &[&str] = &[
    "capabilities",
    "project",
    "prime",
    "recall",
    "memory.list",
    "memory.get",
    "memory.search",
    "memory.update",
    "memory.resolve",
    "memory.delete",
    "memory.curate",
    "session-end",
    "impact",
    "gc",
];

/// A failed op: a stable machine code and a human message.
#[derive(Debug)]
struct HostError {
    code: &'static str,
    message: String,
}

fn fail(code: &'static str, message: impl Into<String>) -> HostError {
    HostError { code, message: message.into() }
}

type HostResult = Result<Value, HostError>;

/// Entry point: read the request, answer it, print exactly one reply. Never
/// panics outward and never writes to stderr.
pub fn run() {
    // A panic must not print its message to stderr (the contract keeps it empty);
    // it is caught below and turned into an `internal` reply.
    std::panic::set_hook(Box::new(|_| {}));
    let mut raw = String::new();
    let reply = match std::io::stdin().take(MAX_REQUEST_BYTES).read_to_string(&mut raw) {
        Err(e) => envelope(Err(fail("bad_request", format!("cannot read stdin: {e}")))),
        Ok(_) => match std::panic::catch_unwind(|| handle(&raw)) {
            Ok(reply) => reply,
            Err(_) => envelope(Err(fail("internal", "icode host panicked"))),
        },
    };
    // A closed stdout (the host timed out and went away) is not our problem; a
    // `println!` would panic on EPIPE, so write and ignore the error.
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{reply}");
    let _ = out.flush();
}

/// Parse one raw request and produce its serialized reply.
fn handle(raw: &str) -> String {
    let result = parse_request(raw).and_then(|(op, args)| dispatch(&op, &args));
    envelope(result)
}

fn envelope(result: HostResult) -> String {
    let v = match result {
        Ok(data) => json!({ "api": API, "ok": true, "data": data }),
        Err(e) => json!({ "api": API, "ok": false, "error": { "code": e.code, "message": e.message } }),
    };
    v.to_string()
}

fn parse_request(raw: &str) -> Result<(String, Map<String, Value>), HostError> {
    let req: Value = serde_json::from_str(raw.trim())
        .map_err(|e| fail("bad_request", format!("request is not JSON: {e}")))?;
    let obj = req
        .as_object()
        .ok_or_else(|| fail("bad_request", "request must be a JSON object"))?;
    match obj.get("api").and_then(Value::as_u64) {
        Some(API) => {}
        _ => return Err(fail("bad_api", format!("unsupported api (this binary speaks api {API})"))),
    }
    let op = obj
        .get("op")
        .and_then(Value::as_str)
        .ok_or_else(|| fail("bad_request", "missing string field `op`"))?
        .to_string();
    let args = match obj.get("args") {
        None | Some(Value::Null) => Map::new(),
        Some(Value::Object(m)) => m.clone(),
        Some(_) => return Err(fail("bad_request", "`args` must be an object")),
    };
    Ok((op, args))
}

fn dispatch(op: &str, args: &Map<String, Value>) -> HostResult {
    match op {
        "capabilities" => Ok(json!({ "api": API, "version": env!("CARGO_PKG_VERSION"), "ops": OPS })),
        "project" => op_project(args),
        "prime" => op_prime(args),
        "recall" => op_recall(args),
        "memory.list" => op_memory_list(args),
        "memory.get" => op_memory_get(args),
        "memory.search" => op_memory_search(args),
        "memory.update" => op_memory_update(args),
        "memory.resolve" => op_memory_resolve(args),
        "memory.delete" => op_memory_delete(args),
        "memory.curate" => op_memory_curate(args),
        "session-end" => op_session_end(args),
        "impact" => op_impact(args),
        "gc" => op_gc(args),
        other => Err(fail("unknown_op", format!("unknown op `{other}`"))),
    }
}

// ──────────────────────────── argument helpers ────────────────────────────

fn opt_str<'a>(args: &'a Map<String, Value>, key: &str) -> Result<Option<&'a str>, HostError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.as_str())),
        Some(_) => Err(fail("bad_args", format!("`{key}` must be a string"))),
    }
}

fn req_str<'a>(args: &'a Map<String, Value>, key: &str) -> Result<&'a str, HostError> {
    opt_str(args, key)?
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| fail("bad_args", format!("missing `{key}`")))
}

fn opt_bool(args: &Map<String, Value>, key: &str) -> Result<Option<bool>, HostError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(fail("bad_args", format!("`{key}` must be a boolean"))),
    }
}

fn opt_f64(args: &Map<String, Value>, key: &str) -> Result<Option<f64>, HostError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_f64()
            .filter(|f| f.is_finite())
            .map(Some)
            .ok_or_else(|| fail("bad_args", format!("`{key}` must be a number"))),
    }
}

/// An optional count, clamped to `1..=max` (`default` when absent).
fn opt_limit(args: &Map<String, Value>, key: &str, default: usize, max: usize) -> Result<usize, HostError> {
    Ok(match opt_f64(args, key)? {
        None => default,
        Some(n) => (n.max(1.0) as usize).min(max),
    })
}

fn opt_str_list(args: &Map<String, Value>, key: &str) -> Result<Option<Vec<String>>, HostError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| fail("bad_args", format!("`{key}` must be an array of strings")))
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Some),
        Some(_) => Err(fail("bad_args", format!("`{key}` must be an array of strings"))),
    }
}

fn opt_category(args: &Map<String, Value>) -> Result<Option<Category>, HostError> {
    match opt_str(args, "category")? {
        None => Ok(None),
        Some(c) => serde_json::from_value::<Category>(Value::String(c.to_string()))
            .map(Some)
            .map_err(|_| {
                fail(
                    "bad_args",
                    format!("unknown category `{c}` (decision|progress|context|bug|todo|code|general)"),
                )
            }),
    }
}

/// `args.project`: required, and a name that can be a memory bucket.
fn project_arg(args: &Map<String, Value>) -> Result<String, HostError> {
    let p = opt_str(args, "project")?.ok_or_else(|| fail("bad_project", "missing `project`"))?;
    check_project(p)
}

/// The bucket rule ([`icode_engine::validate_bucket_name`]): not empty, not in
/// the reserved `__*` namespace, no control characters.
fn check_project(p: &str) -> Result<String, HostError> {
    icode_engine::validate_bucket_name(p).map_err(|m| fail("bad_project", m))?;
    Ok(p.to_string())
}

fn id_arg(args: &Map<String, Value>) -> Result<MemoryId, HostError> {
    Ok(MemoryId(req_str(args, "id")?.to_string()))
}

// ──────────────────────────── stores ────────────────────────────

fn db_error(e: impl std::fmt::Display) -> HostError {
    let msg = e.to_string();
    if msg.contains("locked") || msg.contains("busy") {
        fail("db_busy", msg)
    } else {
        fail("db_error", msg)
    }
}

/// The central db, read-only. `None` when there is no db yet (an empty store).
fn open_read() -> Result<Option<SqliteMemoryStore>, HostError> {
    match SqliteMemoryStore::open_strict_readonly(&super::central_db_path(), READ_BUSY_MS) {
        Ok(s) => Ok(Some(s)),
        Err(icode_core::Error::NotFound(_)) => Ok(None),
        Err(e) => Err(db_error(e)),
    }
}

/// The central db for an embed-free write. A missing db is `not_found`: there is
/// nothing to change.
fn open_write() -> Result<Arc<SqliteMemoryStore>, HostError> {
    let path = super::central_db_path();
    if !Path::new(&path).is_file() {
        return Err(fail("not_found", "no central memory db yet"));
    }
    SqliteMemoryStore::open_write_no_embedder(&path, WRITE_BUSY_MS)
        .map(Arc::new)
        .map_err(db_error)
}

/// The central db for a write that embeds; `no_embedder` without a backend.
fn open_write_embedding() -> Result<Arc<SqliteMemoryStore>, HostError> {
    let embedder = host_embedder(WRITE_EMBED_TIMEOUT_MS)
        .ok_or_else(|| fail("no_embedder", "no embedding backend available (is Ollama running?)"))?;
    SqliteMemoryStore::open_with_busy_timeout(&super::central_db_path(), embedder, WRITE_BUSY_MS)
        .map(Arc::new)
        .map_err(db_error)
}

/// The embedder for host calls, probed for health; `None` when it is
/// unavailable. `ICODE_EMBED_BACKEND` applies (see `embed_config_from_env`).
fn host_embedder(timeout_ms: u64) -> Option<Arc<dyn Embedder>> {
    let mut cfg = super::embed_config_from_env()?;
    cfg.timeout_ms = timeout_ms;
    let embedder = icode_embed::build_embedder(&cfg).ok()?;
    embedder.health().ok()?;
    Some(Arc::from(embedder))
}

/// The audit-log decorator every host write goes through, journaling next to
/// the central db (`~/.icode/wal.jsonl` in production).
fn audited(base: &Arc<SqliteMemoryStore>) -> WalStore {
    let db = PathBuf::from(super::central_db_path());
    let wal = db.parent().map(|p| p.join("wal.jsonl")).unwrap_or_else(|| PathBuf::from("wal.jsonl"));
    WalStore::new(base.clone(), wal)
}

/// Load one record of `project`: `not_found` / `foreign_record` otherwise.
fn owned_record(store: &SqliteMemoryStore, project: &str, id: &MemoryId) -> Result<MemoryRecord, HostError> {
    let rec = store
        .get(id)
        .map_err(db_error)?
        .ok_or_else(|| fail("not_found", format!("no memory {id}")))?;
    if rec.project != project {
        return Err(fail("foreign_record", format!("memory {id} does not belong to project `{project}`")));
    }
    Ok(rec)
}

// ──────────────────────────── record rendering ────────────────────────────

fn age_days(rec: &MemoryRecord, now: DateTime<Utc>) -> i64 {
    DateTime::parse_from_rfc3339(&rec.created_at)
        .or_else(|_| DateTime::parse_from_rfc3339(&rec.last_accessed_at))
        .map(|t| (now - t.with_timezone(&Utc)).num_days().max(0))
        .unwrap_or(0)
}

fn category_name(c: Category) -> String {
    serde_json::to_value(c)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "general".to_string())
}

fn record_json(rec: &MemoryRecord, now: DateTime<Utc>) -> Value {
    json!({
        "id": rec.id.as_str(),
        "project": rec.project,
        "category": category_name(rec.category),
        "content": rec.content,
        "importance": rec.importance,
        "tags": rec.tags,
        "status": if rec.status == MemoryStatus::Resolved { "resolved" } else { "active" },
        "createdAt": rec.created_at,
        "updatedAt": rec.updated_at,
        "lastAccessedAt": rec.last_accessed_at,
        "accessCount": rec.access_count,
        "resolvedAt": rec.resolved_at,
        "resolveReason": rec.resolve_reason,
        "ageDays": age_days(rec, now),
    })
}

fn scored_json(rec: &MemoryRecord, score: f64, now: DateTime<Utc>) -> Value {
    let mut v = record_json(rec, now);
    v["score"] = json!((score * 1000.0).round() / 1000.0);
    v
}

// ──────────────────────────── project / prime ────────────────────────────

/// `project {cwd}` → which memory bucket a directory belongs to.
fn op_project(args: &Map<String, Value>) -> HostResult {
    let cwd = PathBuf::from(req_str(args, "cwd")?);
    let cwd = std::fs::canonicalize(&cwd).unwrap_or(cwd);
    let (name, root, source) = match super::repo_bucket(&cwd) {
        Some(b) => (b.name, b.root, b.source.as_str()),
        None => (super::hook_project(None, Some(&cwd)), cwd.clone(), "basename"),
    };
    let shared = bucket_is_shared(&name, &root, source);
    Ok(json!({ "name": name, "root": root.to_string_lossy(), "source": source, "shared": shared }))
}

/// Whether the bucket is known to hold another repository's memory too: a marker
/// that names something other than this directory, or a registry root that is a
/// DIFFERENT existing repo resolving to the same bucket. Unknown → false.
fn bucket_is_shared(name: &str, root: &Path, source: &str) -> bool {
    if source == "marker" && super::basename_str(root).as_deref() != Some(name) {
        return true;
    }
    let Ok(Some(store)) = open_read() else {
        return false;
    };
    let Ok(Some(registered)) = store.registry_root(name) else {
        return false;
    };
    let registered = PathBuf::from(registered);
    let Ok(registered) = std::fs::canonicalize(&registered) else {
        return false;
    };
    let here = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    if registered == here {
        return false;
    }
    super::repo_bucket(&registered).is_some_and(|b| {
        b.name == name && std::fs::canonicalize(&b.root).unwrap_or(b.root) != here
    })
}

/// `prime {cwd, project?, kind}` → the facts `hook <kind>` would inject, without
/// any write and without the MCP-call instructions (a host's agent may have no
/// iCode MCP at all).
fn op_prime(args: &Map<String, Value>) -> HostResult {
    let kind = req_str(args, "kind")?;
    if kind != "session-start" && kind != "precompact" {
        return Err(fail("bad_args", format!("unknown kind `{kind}` (session-start|precompact)")));
    }
    let project = match opt_str(args, "project")? {
        Some(p) => check_project(p)?,
        None => {
            let cwd = req_str(args, "cwd")?;
            super::hook_project(None, Some(Path::new(cwd)))
        }
    };
    let Some(store) = open_read()? else {
        return Ok(json!({ "project": project, "text": "", "count": 0 }));
    };
    let prime = if kind == "session-start" {
        super::session_prime(&store, &project, true).map_err(db_error)?
    } else {
        super::precompact_prime(&store, &project)
    };
    Ok(json!({ "project": project, "text": prime.lines.join("\n"), "count": prime.count }))
}

// ──────────────────────────── memory ────────────────────────────

fn op_memory_list(args: &Map<String, Value>) -> HostResult {
    let project = project_arg(args)?;
    let category = opt_category(args)?;
    let include_resolved = opt_bool(args, "includeResolved")?.unwrap_or(false);
    let limit = opt_limit(args, "limit", 50, 200)?;
    let Some(store) = open_read()? else {
        return Ok(json!({ "items": [] }));
    };
    let now = Utc::now();
    let items: Vec<Value> = store
        .list(&project, category, limit, include_resolved)
        .map_err(db_error)?
        .iter()
        .map(|r| record_json(r, now))
        .collect();
    Ok(json!({ "items": items }))
}

fn op_memory_get(args: &Map<String, Value>) -> HostResult {
    let project = project_arg(args)?;
    let ids = opt_str_list(args, "ids")?.ok_or_else(|| fail("bad_args", "missing `ids`"))?;
    if ids.len() > 50 {
        return Err(fail("bad_args", "at most 50 ids per call"));
    }
    let Some(store) = open_read()? else {
        return Ok(json!({ "items": [] }));
    };
    let now = Utc::now();
    let mut items = Vec::new();
    for id in ids {
        let id = MemoryId(id);
        match store.get(&id).map_err(db_error)? {
            None => continue, // gone since the host saw it: just omitted
            Some(rec) if rec.project != project => {
                return Err(fail(
                    "foreign_record",
                    format!("memory {id} does not belong to project `{project}`"),
                ))
            }
            Some(rec) => items.push(record_json(&rec, now)),
        }
    }
    Ok(json!({ "items": items }))
}

/// `memory.search {project, query, limit?, category?, includeResolved?}` — the
/// panel's search box: lexical, no relevance floor and no recall exclusions.
fn op_memory_search(args: &Map<String, Value>) -> HostResult {
    let started = Instant::now();
    let project = project_arg(args)?;
    let query = req_str(args, "query")?;
    let limit = opt_limit(args, "limit", 20, 100)?;
    let category = opt_category(args)?;
    let include_resolved = opt_bool(args, "includeResolved")?.unwrap_or(false);
    let tokens = query_tokens(query, false);
    let mut items = Vec::new();
    if let (Some(store), false) = (open_read()?, tokens.is_empty()) {
        let now = Utc::now();
        let hits = store
            .lexical_scored(&project, &fts_expr(&tokens), limit * 4 + 20, include_resolved)
            .map_err(db_error)?;
        for (rec, bm25) in hits {
            if category.is_some_and(|c| c != rec.category) {
                continue;
            }
            items.push(scored_json(&rec, -bm25, now));
            if items.len() >= limit {
                break;
            }
        }
    }
    Ok(json!({
        "mode": "lexical",
        "degraded": false,
        "ms": started.elapsed().as_millis() as u64,
        "items": items,
    }))
}

fn op_memory_update(args: &Map<String, Value>) -> HostResult {
    let project = project_arg(args)?;
    let id = id_arg(args)?;
    let content = opt_str(args, "content")?;
    if content.is_some_and(|c| c.trim().is_empty()) {
        return Err(fail("bad_args", "`content` must not be empty"));
    }
    let importance = opt_f64(args, "importance")?;
    if importance.is_some_and(|i| !(0.0..=5.0).contains(&i)) {
        return Err(fail("bad_args", "`importance` must be within 0..5"));
    }
    let category = opt_category(args)?;
    let tags = opt_str_list(args, "tags")?;
    if let Some(tags) = &tags {
        if tags.len() > 32 || tags.iter().any(|t| t.trim().is_empty() || t.chars().any(char::is_control)) {
            return Err(fail("bad_args", "`tags`: at most 32 non-empty tags without control characters"));
        }
    }

    // Peek first (read-only) so an unchanged content does not demand an embedder
    // and a foreign id fails before anything is opened for writing.
    let current = {
        let store = open_read()?.ok_or_else(|| fail("not_found", format!("no memory {id}")))?;
        owned_record(&store, &project, &id)?
    };
    let content = content.filter(|c| *c != current.content);
    let base = if content.is_some() { open_write_embedding()? } else { open_write()? };
    // Re-check under the write connection: the record may have moved meanwhile.
    owned_record(&base, &project, &id)?;
    if content.is_some() || tags.is_some() {
        audited(&base)
            .update(&id, content, tags.as_deref())
            .map_err(write_error)?;
    }
    base.set_importance_category(&id, importance.map(|i| i as f32), category)
        .map_err(write_error)?;
    let rec = owned_record(&base, &project, &id)?;
    Ok(json!({ "item": record_json(&rec, Utc::now()) }))
}

fn write_error(e: icode_core::Error) -> HostError {
    match e {
        icode_core::Error::NotFound(m) => fail("not_found", m),
        icode_core::Error::Embed(m) => fail("no_embedder", m),
        other => db_error(other),
    }
}

fn op_memory_resolve(args: &Map<String, Value>) -> HostResult {
    let project = project_arg(args)?;
    let id = id_arg(args)?;
    let reason = opt_str(args, "reason")?
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .unwrap_or("archived from the IDE");
    let reason: String = reason.chars().filter(|c| !c.is_control()).take(500).collect();
    let base = open_write()?;
    owned_record(&base, &project, &id)?;
    audited(&base).resolve(&id, &reason).map_err(write_error)?;
    let rec = owned_record(&base, &project, &id)?;
    Ok(json!({ "item": record_json(&rec, Utc::now()) }))
}

fn op_memory_delete(args: &Map<String, Value>) -> HostResult {
    let project = project_arg(args)?;
    let id = id_arg(args)?;
    let base = open_write()?;
    owned_record(&base, &project, &id)?;
    audited(&base).delete(&id).map_err(write_error)?;
    Ok(json!({ "deleted": true }))
}

/// `memory.curate {project, apply?, staleDays?, dupSimilarity?}` — the curator's
/// report; dry run unless `apply`, and even then it only archives (resolve).
fn op_memory_curate(args: &Map<String, Value>) -> HostResult {
    let project = project_arg(args)?;
    let apply = opt_bool(args, "apply")?.unwrap_or(false);
    let defaults = icode_engine::CurateConfig::default();
    let cfg = icode_engine::CurateConfig {
        apply,
        stale_days: opt_f64(args, "staleDays")?.unwrap_or(defaults.stale_days).max(1.0),
        dup_similarity: opt_f64(args, "dupSimilarity")?
            .unwrap_or(defaults.dup_similarity)
            .clamp(0.3, 1.0),
        ..defaults
    };
    let ranking = icode_core::config::RankingConfig::default();
    let report = if apply {
        let base = open_write()?;
        icode_engine::curate(&audited(&base), &project, cfg, &ranking).map_err(db_error)?
    } else {
        match open_read()? {
            Some(store) => icode_engine::curate(&store, &project, cfg, &ranking).map_err(db_error)?,
            None => icode_engine::CurateReport::default(),
        }
    };
    serde_json::to_value(report).map_err(|e| fail("internal", e.to_string()))
}

// ──────────────────────────── session-end ────────────────────────────

/// Tag every host session summary carries, next to its `chat:<sessionKey>` key.
const SESSION_TAG: &str = "flowcode-session";

/// `session-end {project, sessionKey, title, files[], summary, nativeSessionId?}` —
/// ONE summary record per host chat, upserted by its `chat:<sessionKey>` tag (found
/// in SQL, however many newer records the project has). Importance 1, category
/// progress. With `nativeSessionId`, the Stop hook's draft for that native session
/// (`sid:<first 8>` + `auto-session`) is deleted: the host's summary supersedes it.
/// Needs an embedder (`no_embedder` otherwise — and then nothing is changed).
fn op_session_end(args: &Map<String, Value>) -> HostResult {
    let project = project_arg(args)?;
    let key = req_str(args, "sessionKey")?.trim();
    if key.chars().count() > 200 || key.chars().any(char::is_control) {
        return Err(fail("bad_args", "`sessionKey`: at most 200 characters, no control characters"));
    }
    let title = opt_str(args, "title")?.unwrap_or_default();
    let summary = opt_str(args, "summary")?.unwrap_or_default();
    let files = opt_str_list(args, "files")?.unwrap_or_default();
    let native = opt_str(args, "nativeSessionId")?.map(str::trim).filter(|s| !s.is_empty());
    let content = session_content(title, &files, summary)
        .ok_or_else(|| fail("bad_args", "nothing to save: `title`, `summary` and `files` are all empty"))?;

    let base = open_write_embedding()?;
    let store = audited(&base);
    let chat_tag = format!("chat:{key}");
    let tags = vec![SESSION_TAG.to_string(), chat_tag.clone()];

    let existing = base.find_by_tag(Some(&project), &chat_tag, true).map_err(db_error)?;
    let (id, action) = match existing.first() {
        Some(rec) => {
            let mut keep = rec.tags.clone();
            for t in &tags {
                if !keep.contains(t) {
                    keep.push(t.clone());
                }
            }
            store.update(&rec.id, Some(&content), Some(&keep)).map_err(write_error)?;
            (rec.id.clone(), "updated")
        }
        None => {
            let outcome = store
                .add(icode_core::model::NewMemory {
                    project: project.clone(),
                    content,
                    category: Category::Progress,
                    tags,
                    importance: 1.0,
                    session_id: native.map(str::to_string),
                })
                .map_err(write_error)?;
            match outcome {
                icode_core::model::AddOutcome::Added { id } => (id, "created"),
                // The dedup gate found the same fact already stored: nothing new.
                icode_core::model::AddOutcome::Duplicate { existing, .. } => (existing, "duplicate"),
            }
        }
    };

    let mut drafts_removed = 0;
    if let Some(native) = native {
        let sid = icode_engine::memory::stop_hook::sid_tag(native);
        for draft in base.find_by_tag(Some(&project), &sid, true).map_err(db_error)? {
            if draft.tags.iter().any(|t| t == icode_engine::memory::stop_hook::AUTO_TAG) {
                store.delete(&draft.id).map_err(write_error)?;
                drafts_removed += 1;
            }
        }
    }
    Ok(json!({ "id": id.as_str(), "action": action, "draftsRemoved": drafts_removed }))
}

/// The deterministic summary text: `Чат FlowCode «title». Итог: … Файлы: …`, with
/// the host's prompt markers stripped and every part capped. `None` when there is
/// nothing to say.
fn session_content(title: &str, files: &[String], summary: &str) -> Option<String> {
    let title = cap_chars(&strip_host_markers(title), 160);
    let summary = cap_chars(&strip_host_markers(summary), 600);
    let files: Vec<String> = files
        .iter()
        .map(|f| cap_chars(&strip_host_markers(f), 160))
        .filter(|f| !f.is_empty())
        .collect();
    if title.is_empty() && summary.is_empty() && files.is_empty() {
        return None;
    }
    let mut out = if title.is_empty() { "Чат FlowCode.".to_string() } else { format!("Чат FlowCode «{title}».") };
    if !summary.is_empty() {
        out.push_str(&format!(" Итог: {summary}"));
    }
    if !files.is_empty() {
        const SHOWN: usize = 12;
        out.push_str(&format!(" Файлы: {}", files.iter().take(SHOWN).cloned().collect::<Vec<_>>().join(", ")));
        if files.len() > SHOWN {
            out.push_str(&format!(" (+{})", files.len() - SHOWN));
        }
        out.push('.');
    }
    Some(out)
}

/// Remove the host's own prompt plumbing (`[FlowCode: …]` blocks and the resume
/// preamble) so it never becomes "knowledge"; whitespace collapsed.
fn strip_host_markers(s: &str) -> String {
    let mut text = s.replace("Continue the conversation below", " ");
    while let Some(start) = text.find("[FlowCode:") {
        let end = text[start..].find(']').map(|e| start + e + 1).unwrap_or(text.len());
        text.replace_range(start..end, " ");
    }
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn cap_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

// ──────────────────────────── recall ────────────────────────────

/// A recalled record must share at least this many significant tokens with the
/// query. One shared word is how unrelated records used to leak into a turn.
const RECALL_MIN_MATCHED: usize = 2;

/// Tokens a record must share with a query of `n` significant tokens: at least
/// [`RECALL_MIN_MATCHED`], and half of a longer query — two common words out of
/// six ("панель", "памяти") are not what the message is about.
fn required_matches(n: usize) -> usize {
    RECALL_MIN_MATCHED.max(n.div_ceil(2))
}
/// Candidates pulled per arm before filtering.
const RECALL_CANDIDATES: usize = 40;
/// Lexical-only floor on the RAW fts5 bm25 (more negative = more relevant).
/// Calibrated on a copy of a real 6k-record base: on-topic hits score -10 … -28,
/// records that merely share two common words -4 … -9.
const RECALL_LEXICAL_MAX_BM25: f64 = -5.0;
/// Hybrid mode: a record qualifies on EITHER arm's own raw score. The lexical
/// floor is stricter than in lexical-only mode, since a weak lexical hit that is
/// genuinely relevant is caught by the dense arm instead…
const RECALL_HYBRID_MAX_BM25: f64 = -6.0;
/// …whose floor is the RAW cosine distance (qwen3-embedding:0.6b: on-topic
/// records sit at 0.18–0.31, loosely related ones from 0.35 up).
const RECALL_HYBRID_MAX_DISTANCE: f32 = 0.35;
/// RRF constant for fusing the two arms in hybrid mode.
const RECALL_RRF_K: f64 = 60.0;

/// `recall {project, query, limit?≤5, budgetChars?=1200, excludeIds?, allowSemantic?}`
/// — the few records worth putting in front of the agent for THIS message.
///
/// Precision over recall: the block lands in every turn's prompt, so a wrong
/// record costs more than a missing one. Lexical by default; `allowSemantic`
/// adds the dense arm when an embedder answers (else `degraded:true`, lexical).
fn op_recall(args: &Map<String, Value>) -> HostResult {
    let started = Instant::now();
    let project = project_arg(args)?;
    let query = opt_str(args, "query")?.ok_or_else(|| fail("bad_args", "missing `query`"))?;
    let limit = opt_limit(args, "limit", 3, 5)?;
    let budget = opt_limit(args, "budgetChars", 1200, 20_000)?;
    let exclude: HashSet<String> = opt_str_list(args, "excludeIds")?.unwrap_or_default().into_iter().collect();
    let allow_semantic = opt_bool(args, "allowSemantic")?.unwrap_or(false);

    let reply = |mode: &str, degraded: bool, items: Vec<Value>| {
        json!({
            "mode": mode,
            "degraded": degraded,
            "ms": started.elapsed().as_millis() as u64,
            "items": items,
        })
    };

    let tokens = query_tokens(query, true);
    if tokens.len() < RECALL_MIN_MATCHED {
        return Ok(reply("lexical", false, Vec::new()));
    }
    let Some(store) = open_read()? else {
        return Ok(reply("lexical", false, Vec::new()));
    };

    let lexical = store
        .lexical_scored(&project, &fts_expr(&tokens), RECALL_CANDIDATES, false)
        .map_err(db_error)?;

    // Dense arm, only when asked for and only if the backend answers in time.
    let mut degraded = false;
    let dense = if allow_semantic {
        // The host asks for the dense arm only while the model is resident; the
        // embedder's light default (`keep_alive=0s`) would evict it with this very
        // call and push every following turn to the cold path.
        if std::env::var_os("ICODE_OLLAMA_KEEP_ALIVE").is_none() {
            std::env::set_var("ICODE_OLLAMA_KEEP_ALIVE", "5m");
        }
        let clean = icode_engine::sanitize_nl(query);
        let qvec = host_embedder(RECALL_EMBED_TIMEOUT_MS)
            .and_then(|e| e.embed(&[clean.as_str()]).ok())
            .and_then(|mut v| v.pop())
            .filter(|v| v.len() == store.vector_dim());
        match qvec {
            Some(q) => Some(store.dense_scored(&project, &q, RECALL_CANDIDATES, false).map_err(db_error)?),
            None => {
                degraded = true;
                None
            }
        }
    } else {
        None
    };

    let need = required_matches(tokens.len());
    let eligible = |rec: &MemoryRecord| {
        !exclude.contains(rec.id.as_str())
            && !is_noise_for_recall(rec)
            && matched_tokens(&rec.content, &tokens) >= need
    };

    // (record, ordering score, raw bm25, raw cosine distance)
    type Ranked = (MemoryRecord, f64, Option<f64>, Option<f32>);
    let (mode, ranked): (&str, Vec<Ranked>) = match dense {
        None => {
            let ranked = lexical
                .into_iter()
                .filter(|(rec, bm25)| *bm25 <= RECALL_LEXICAL_MAX_BM25 && eligible(rec))
                .map(|(rec, bm25)| (rec, -bm25, Some(bm25), None))
                .collect();
            ("lexical", ranked)
        }
        Some(dense) => {
            // Each arm qualifies on its OWN raw score; RRF only orders survivors.
            let mut fused: std::collections::HashMap<String, Ranked> = Default::default();
            for (rank, (rec, bm25)) in lexical.into_iter().enumerate() {
                if bm25 <= RECALL_HYBRID_MAX_BM25 && eligible(&rec) {
                    let s = 1.0 / (RECALL_RRF_K + rank as f64);
                    let e = fused.entry(rec.id.0.clone()).or_insert((rec, 0.0, None, None));
                    e.1 += s;
                    e.2 = Some(bm25);
                }
            }
            for (rank, (rec, dist)) in dense.into_iter().enumerate() {
                if dist <= RECALL_HYBRID_MAX_DISTANCE && eligible(&rec) {
                    let s = 1.0 / (RECALL_RRF_K + rank as f64);
                    let e = fused.entry(rec.id.0.clone()).or_insert((rec, 0.0, None, None));
                    e.1 += s;
                    e.3 = Some(dist);
                }
            }
            let mut ranked: Vec<Ranked> = fused.into_values().collect();
            ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            ("hybrid", ranked)
        }
    };

    // Budget by whole records: a record that does not fit is skipped, never cut.
    let now = Utc::now();
    let mut used = 0usize;
    let mut items = Vec::new();
    for (rec, score, bm25, distance) in ranked {
        if items.len() >= limit {
            break;
        }
        let len = rec.content.chars().count();
        if used + len > budget {
            continue;
        }
        used += len;
        let mut item = scored_json(&rec, score, now);
        item["bm25"] = json!(bm25.map(|b| (b * 1000.0).round() / 1000.0));
        item["distance"] = json!(distance.map(|d| (f64::from(d) * 1000.0).round() / 1000.0));
        items.push(item);
    }
    Ok(reply(mode, degraded, items))
}

// ──────────────────────────── query tokens ────────────────────────────

/// Words too common to carry a query (RU + EN). Only tokens of 3+ characters are
/// ever compared, so shorter stop words need no entry.
const STOP_WORDS: &[&str] = &[
    // English
    "the", "and", "for", "with", "that", "this", "these", "those", "what", "how", "why", "who",
    "whom", "which", "when", "where", "are", "was", "were", "been", "being", "you", "your",
    "yours", "can", "could", "not", "but", "from", "have", "has", "had", "will", "would",
    "should", "shall", "about", "into", "onto", "there", "their", "them", "they", "then", "than",
    "all", "any", "some", "our", "ours", "out", "use", "get", "got", "make", "also", "just",
    "like", "does", "did", "done", "its", "it's", "let", "lets", "please", "need", "want",
    "now", "here", "very", "more", "most", "much", "many", "only", "other", "such", "same",
    "each", "every", "both", "few", "own", "too", "via", "per", "yes", "okay", "ok", "thanks",
    "her", "his", "him", "she", "one", "two", "may", "might", "must", "again", "still",
    // Russian
    "что", "как", "это", "эта", "этот", "эти", "этой", "этого", "этом", "для", "его", "её",
    "она", "они", "оно", "так", "вот", "был", "была", "были", "было", "быть", "есть", "или",
    "ещё", "еще", "уже", "все", "всё", "всех", "весь", "вся", "только", "тоже", "также",
    "чтобы", "если", "когда", "где", "почему", "зачем", "который", "которая", "которые",
    "которое", "которых", "того", "тот", "там", "тут", "здесь", "надо", "нужно", "можно",
    "давай", "давайте", "сделай", "сделать", "пожалуйста", "мне", "меня", "мной", "тебя",
    "тебе", "нам", "нас", "вам", "вас", "при", "про", "над", "под", "без", "через", "после",
    "перед", "между", "сейчас", "потом", "теперь", "очень", "более", "менее", "чем", "нет",
    "даже", "будет", "будут", "мой", "моя", "мои", "твой", "свой", "своё", "свои", "наш",
    "ваш", "себя", "себе", "кто", "чего", "чему", "нибудь", "либо", "какой", "какая", "какие",
    "какое", "такой", "такая", "такие", "раз", "ещe", "просто", "вообще", "ли", "ага", "окей",
    "спасибо", "привет", "посмотри", "проверь", "скажи", "хочу", "могу", "может", "нему",
    // Imperative fillers that open a request but say nothing about its subject.
    "добавь", "добавить", "почини", "починить", "исправь", "исправить", "убери", "убрать",
    "поменяй", "поменять", "найди", "найти", "покажи", "показать", "напиши", "написать",
    "создай", "создать", "удали", "удалить", "обнови", "обновить", "запусти", "запустить",
    "сделал", "сделали", "работает", "работать", "дела", "fix", "add", "show", "find",
    "remove", "update", "create", "write", "run", "check", "look", "help", "work", "works",
];

/// Lower-cased word tokens of a query: Unicode-aware split (Cyrillic survives,
/// like the curator's tokenizer), tokens of 3+ characters, stop words dropped
/// when `drop_stop`, deduplicated (`ё` = `е`) in order, at most 16.
fn query_tokens(query: &str, drop_stop: bool) -> Vec<String> {
    let clean = icode_engine::sanitize_nl(query);
    let mut seen = HashSet::new();
    clean
        .split(|c: char| !c.is_alphanumeric())
        .map(str::to_lowercase)
        .filter(|t| t.chars().count() >= 3)
        .filter(|t| !(drop_stop && STOP_WORDS.contains(&fold(t).as_str())))
        .filter(|t| seen.insert(fold(t)))
        .take(16)
        .collect()
}

/// `ё` → `е` for comparisons. fts5's `unicode61` folds case but NOT `ё`, so the
/// MATCH expression carries both spellings (see [`fts_expr`]).
fn fold(t: &str) -> String {
    t.to_lowercase().replace('ё', "е")
}

/// The prefix a token is matched by: long words lose their last one or two
/// characters so an inflected form (`демона`, `индексатора`) still matches the
/// stem the query used. Never shorter than 4 characters; short tokens match
/// exactly.
fn stem(t: &str) -> String {
    let n = t.chars().count();
    let keep = match n {
        0..=4 => n,
        5 => 4,
        _ => n - 2,
    };
    t.chars().take(keep).collect()
}

/// One token as an fts5 term: a quoted prefix phrase when it was stemmed.
fn fts_term(t: &str) -> String {
    let s = stem(t);
    if s.chars().count() < t.chars().count() {
        format!("\"{s}\"*")
    } else {
        format!("\"{s}\"")
    }
}

/// fts5 MATCH expression: every token as a quoted (prefix) phrase, OR-joined, a
/// token with `ё` also in its `е` spelling. Tokens are alphanumeric only, so no
/// fts5 syntax can leak in.
fn fts_expr(tokens: &[String]) -> String {
    let mut terms = Vec::new();
    for t in tokens {
        terms.push(fts_term(t));
        let folded = fold(t);
        if folded != *t {
            terms.push(fts_term(&folded));
        }
    }
    terms.join(" OR ")
}

/// How many of the query `tokens` occur in `content` (a content word starting
/// with the token's stem counts; `ё` = `е` on both sides).
fn matched_tokens(content: &str, tokens: &[String]) -> usize {
    let words: Vec<String> = content
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() >= 3)
        .map(fold)
        .collect();
    tokens
        .iter()
        .filter(|t| {
            let s = stem(&fold(t));
            words.iter().any(|w| w.starts_with(&s))
        })
        .count()
}

// ──────────────────────────── impact ────────────────────────────

/// `impact {root, files[]}` — what a set of changed files touches: each file's
/// symbols with caller/test counts, and the memories that mention the file.
fn op_impact(args: &Map<String, Value>) -> HostResult {
    let root = PathBuf::from(req_str(args, "root")?);
    let root = std::fs::canonicalize(&root).map_err(|_| fail("bad_args", "`root` is not a directory"))?;
    if !root.is_dir() {
        return Err(fail("bad_args", "`root` is not a directory"));
    }
    let files = opt_str_list(args, "files")?.ok_or_else(|| fail("bad_args", "missing `files`"))?;
    let files: Vec<String> = files.into_iter().filter(|f| !f.trim().is_empty()).take(20).collect();
    let project = match opt_str(args, "project")? {
        Some(p) => check_project(p)?,
        None => super::hook_project(None, Some(&root)),
    };

    let code = SqliteCodeStore::open_existing_readonly(&root, READ_BUSY_MS).map_err(db_error)?;
    let memory = open_read().ok().flatten();
    let now = Utc::now();
    let mut out = Vec::new();
    for f in &files {
        let rel = f.trim().trim_start_matches("./");
        let abs = if Path::new(rel).is_absolute() { PathBuf::from(rel) } else { root.join(rel) };
        let abs = std::fs::canonicalize(&abs).unwrap_or(abs);
        let path = abs.to_string_lossy().to_string();

        let mut symbols = Vec::new();
        if let Some(code) = &code {
            for hit in code.file_outline(&path).unwrap_or_default().into_iter().take(25) {
                let callers = code.get_callers(&hit.name, 200).map(|c| c.len()).unwrap_or(0);
                let tests = code.find_tests_covering(&hit.name, 3).unwrap_or_default();
                symbols.push(json!({
                    "name": hit.name,
                    "kind": serde_json::to_value(hit.kind).unwrap_or(Value::Null),
                    "callers": callers,
                    "tests": tests.len(),
                    "testNames": tests.iter().take(5).map(|t| t.test.clone()).collect::<Vec<_>>(),
                }));
            }
        }

        let mut memories = Vec::new();
        if let Some(store) = &memory {
            let stem = Path::new(rel)
                .file_stem()
                .and_then(|s| s.to_str())
                .map(fold)
                .filter(|s| s.chars().count() >= 3 && s.chars().all(char::is_alphanumeric));
            if let Some(stem) = stem {
                let hits = store
                    .lexical_scored(&project, &format!("\"{stem}\""), 20, false)
                    .unwrap_or_default();
                for (rec, _) in hits.into_iter().filter(|(r, _)| !is_noise_for_recall(r)).take(3) {
                    memories.push(record_json(&rec, now));
                }
            }
        }
        out.push(json!({ "path": path, "symbols": symbols, "memories": memories }));
    }
    Ok(json!({ "indexed": code.is_some(), "project": project, "files": out }))
}

/// Records a host must never surface as recalled knowledge: Stop-hook
/// auto-summaries, low-importance general notes, and records that captured the
/// host's own prompt plumbing.
fn is_noise_for_recall(rec: &MemoryRecord) -> bool {
    rec.tags.iter().any(|t| t == "auto-session" || t.starts_with("sid:"))
        || (rec.importance < 2.0 && rec.category == Category::General)
        || rec.content.contains("[FlowCode:")
        || rec.content.contains("Continue the conversation below")
}

// ──────────────────────────── gc ────────────────────────────

/// A Stop-hook draft idle longer than this is garbage.
const GC_DRAFT_DAYS: i64 = 14;

/// `gc {apply?=false}` — what is left over and safe to clean:
///
/// * `daemons`: running `icode daemon run` processes whose root is gone
///   (`root_missing`) or is a throwaway scratch dir under the temp dir
///   (`root_in_tmp_scratch`). Reported only — gc NEVER signals a process; the
///   host shows the pids. Linux only (read from `/proc`); elsewhere the list is
///   empty.
/// * `drafts`: Stop-hook drafts (`sid:*` tag, importance ≤ 1, not a
///   todo/bug/decision) untouched for more than 14 days.
///
/// Dry run by default; `apply` archives the drafts (resolve, reversible) and
/// does nothing else.
fn op_gc(args: &Map<String, Value>) -> HostResult {
    let apply = opt_bool(args, "apply")?.unwrap_or(false);
    let tmp = std::env::temp_dir();
    let daemons: Vec<Value> = icode_daemons()
        .into_iter()
        .filter_map(|(pid, root)| {
            daemon_gc_reason(&root, &tmp)
                .map(|reason| json!({ "pid": pid, "root": root.to_string_lossy(), "reason": reason }))
        })
        .collect();

    let now = Utc::now();
    let drafts: Vec<MemoryRecord> = match open_read()? {
        Some(store) => store
            .find_by_tag_prefix(None, "sid:", false)
            .map_err(db_error)?
            .into_iter()
            .filter(|r| is_stale_draft(r, now))
            .collect(),
        None => Vec::new(),
    };
    if apply && !drafts.is_empty() {
        let base = open_write()?;
        let store = audited(&base);
        for d in &drafts {
            store
                .resolve(&d.id, &format!("gc: Stop-hook draft idle > {GC_DRAFT_DAYS} days — archived, not deleted"))
                .map_err(write_error)?;
        }
    }
    let drafts: Vec<Value> = drafts
        .iter()
        .map(|d| json!({ "id": d.id.as_str(), "project": d.project, "ageDays": idle_days(d, now) }))
        .collect();
    Ok(json!({ "daemons": daemons, "drafts": drafts, "applied": apply }))
}

/// Days since the record last changed (`updated_at`, else `created_at`): a draft
/// the Stop hook keeps refreshing is not idle.
fn idle_days(rec: &MemoryRecord, now: DateTime<Utc>) -> i64 {
    let ts = rec.updated_at.as_deref().unwrap_or(&rec.created_at);
    DateTime::parse_from_rfc3339(ts)
        .map(|t| (now - t.with_timezone(&Utc)).num_days().max(0))
        .unwrap_or(0)
}

fn is_stale_draft(rec: &MemoryRecord, now: DateTime<Utc>) -> bool {
    rec.status == MemoryStatus::Active
        && rec.importance <= 1.0
        && rec.tags.iter().any(|t| t.starts_with("sid:"))
        && !matches!(rec.category, Category::Todo | Category::Bug | Category::Decision)
        && idle_days(rec, now) > GC_DRAFT_DAYS
}

/// Why a daemon watching `root` is garbage, if it is.
fn daemon_gc_reason(root: &Path, tmp: &Path) -> Option<&'static str> {
    if !root.is_dir() {
        return Some("root_missing");
    }
    let in_tmp = root.starts_with(tmp) || root.starts_with("/tmp");
    let scratch = root.components().any(|c| c.as_os_str() == "scratchpad");
    (in_tmp && scratch).then_some("root_in_tmp_scratch")
}

/// `(pid, root)` of every live `icode daemon run <root>` process.
#[cfg(target_os = "linux")]
fn icode_daemons() -> Vec<(i32, PathBuf)> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse::<i32>().ok()) else {
            continue;
        };
        let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
            continue;
        };
        if super::is_zombie(pid) {
            continue;
        }
        let cwd = std::fs::read_link(format!("/proc/{pid}/cwd")).ok();
        if let Some(root) = daemon_root_from_cmdline(&cmdline, cwd.as_deref()) {
            out.push((pid, root));
        }
    }
    out.sort();
    out
}

#[cfg(not(target_os = "linux"))]
fn icode_daemons() -> Vec<(i32, PathBuf)> {
    Vec::new()
}

/// The root of an `icode daemon run <root>` command line (NUL-separated argv,
/// as in `/proc/<pid>/cmdline`); a relative root is resolved against the
/// process's `cwd`. `None` for any other command.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn daemon_root_from_cmdline(cmdline: &[u8], cwd: Option<&Path>) -> Option<PathBuf> {
    if cmdline.is_empty() || !super::cmdline_is_icode(cmdline) {
        return None;
    }
    let argv: Vec<String> = cmdline
        .split(|b| *b == 0)
        .filter(|a| !a.is_empty())
        .map(|a| String::from_utf8_lossy(a).into_owned())
        .collect();
    let at = argv.windows(2).position(|w| w[0] == "daemon" && w[1] == "run")?;
    let root = PathBuf::from(argv.get(at + 2)?);
    Some(match (root.is_absolute(), cwd) {
        (false, Some(cwd)) => cwd.join(root),
        _ => root,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_requests_become_json_errors() {
        for (raw, code) in [
            ("", "bad_request"),
            ("not json", "bad_request"),
            ("[1,2]", "bad_request"),
            (r#"{"op":"capabilities"}"#, "bad_api"),
            (r#"{"api":2,"op":"capabilities"}"#, "bad_api"),
            (r#"{"api":1}"#, "bad_request"),
            (r#"{"api":1,"op":"capabilities","args":[1]}"#, "bad_request"),
            (r#"{"api":1,"op":"nope"}"#, "unknown_op"),
        ] {
            let v: Value = serde_json::from_str(&handle(raw)).unwrap();
            assert_eq!(v["api"], 1, "{raw}");
            assert_eq!(v["ok"], false, "{raw}");
            assert_eq!(v["error"]["code"], code, "{raw}");
        }
    }

    #[test]
    fn capabilities_lists_every_dispatched_op() {
        let v: Value = serde_json::from_str(&handle(r#"{"api":1,"op":"capabilities"}"#)).unwrap();
        assert_eq!(v["ok"], true);
        let ops: Vec<&str> = v["data"]["ops"].as_array().unwrap().iter().map(|o| o.as_str().unwrap()).collect();
        assert_eq!(ops, OPS);
        for op in OPS {
            let raw = format!(r#"{{"api":1,"op":"{op}","args":{{}}}}"#);
            let v: Value = serde_json::from_str(&handle(&raw)).unwrap();
            assert_ne!(v["error"]["code"], "unknown_op", "{op} is listed but not dispatched");
        }
    }

    #[test]
    fn tokens_keep_cyrillic_and_drop_stop_words() {
        let t = query_tokens("Почему демон не видит изменения в индексаторе?", true);
        assert_eq!(t, vec!["демон", "видит", "изменения", "индексаторе"]);
        let t = query_tokens("what is the daemon doing with the index", true);
        assert_eq!(t, vec!["daemon", "doing", "index"]);
        assert_eq!(query_tokens("Ёлка елка", true), vec!["ёлка"]);
        assert_eq!(fts_expr(&["ёлка".into()]), "\"ёлка\" OR \"елка\"");
    }

    #[test]
    fn session_content_strips_markers_and_caps() {
        let c = session_content(
            "Fix [FlowCode: Work mode] the chip",
            &["lib/runs.mjs".into(), "  ".into()],
            "Continue the conversation below: done [FlowCode: Project memory] x",
        )
        .unwrap();
        assert_eq!(c, "Чат FlowCode «Fix the chip». Итог: : done x Файлы: lib/runs.mjs.");
        assert!(session_content("", &[], "  ").is_none());
        let long = session_content("t", &[], &"я".repeat(2000)).unwrap();
        assert!(long.chars().count() < 700);
    }

    #[test]
    fn daemon_cmdlines_and_gc_reasons() {
        let cmd = b"/home/u/.local/bin/icode\0daemon\0run\0/tmp/x/scratchpad/live\0";
        assert_eq!(daemon_root_from_cmdline(cmd, None), Some(PathBuf::from("/tmp/x/scratchpad/live")));
        let rel = b"./target/debug/icode\0daemon\0run\0proj\0";
        assert_eq!(daemon_root_from_cmdline(rel, Some(Path::new("/work"))), Some(PathBuf::from("/work/proj")));
        assert_eq!(daemon_root_from_cmdline(b"/usr/bin/icode\0serve\0", None), None);
        assert_eq!(daemon_root_from_cmdline(b"/usr/bin/python\0daemon\0run\0/x\0", None), None);

        let td = tempfile::tempdir().unwrap();
        let tmp = td.path();
        let scratch = tmp.join("sess/scratchpad/demo");
        std::fs::create_dir_all(&scratch).unwrap();
        let plain = tmp.join("icode-host");
        std::fs::create_dir_all(&plain).unwrap();
        assert_eq!(daemon_gc_reason(&tmp.join("gone"), tmp), Some("root_missing"));
        assert_eq!(daemon_gc_reason(&scratch, tmp), Some("root_in_tmp_scratch"));
        assert_eq!(daemon_gc_reason(&plain, tmp), None, "a /tmp project that is not scratch stays");
    }

    #[test]
    fn stems_keep_at_least_four_characters() {
        assert_eq!(stem("демон"), "демо");
        assert_eq!(stem("индексаторе"), "индексато");
        assert_eq!(stem("api"), "api");
        assert_eq!(stem("hook"), "hook");
        assert_eq!(fts_expr(&["hook".into(), "демона".into()]), "\"hook\" OR \"демо\"*");
    }
}
