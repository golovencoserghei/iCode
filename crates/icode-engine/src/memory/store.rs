//! `SqliteMemoryStore` — the central cross-session memory store (the M4 base of
//! the `MemoryStore` decorator chain).
//!
//! Backed by a single SQLite file (`~/.icode/icode.db`) shared by the memory
//! rows, their vectors (`vec_memory`), the lexical index (`fts_memory`), and the
//! project registry. The store OWNS the embedder (`Arc<dyn Embedder>`): unlike the
//! per-project code store — which is lexical-only and embeds via a free function —
//! a memory's content vector is computed here at write time, so add/update embed
//! inline. One `Mutex<Connection>` is the single writer/serialization point.
//!
//! The `mem_rowid` bridge maps the TEXT memory id to the INTEGER rowid vec0/fts5
//! need; the invariant `count(vec_memory) == count(fts_memory) == count(memories)
//! == count(mem_rowid)` is preserved by doing add/delete in one transaction and
//! mirroring vec/fts by hand (vec0 and the standalone fts have no FK cascade).
//!
//! Deferred to M4.2 (intentionally NOT here): the dedup gate (`add` always
//! returns `Added`), usage-aware ranking decay / L0 floor, and the NL/FTS5 query
//! sanitizers. `search` fuses dense + lexical with RRF and annotates age only.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use icode_core::error::{Error, Result};
use icode_core::ids::{is_reserved_project, MemoryId};
use icode_core::model::*;
use icode_core::traits::{Embedder, ReadableMemoryStore, WritableMemoryStore};
use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};

use super::schema;
use crate::store::register_sqlite_vec;

// ──────────────────────────── age buckets ────────────────────────────

/// Age thresholds (days) for the `AgeStatus` buckets. ≤7 Fresh, ≤30 Recent,
/// ≤90 Aging, else Stale.
const AGE_FRESH_MAX: i64 = 7;
const AGE_RECENT_MAX: i64 = 30;
const AGE_AGING_MAX: i64 = 90;

/// RRF reciprocal constant (Cormack et al.; the de-facto 60). Mirrors
/// `search::RRF_K` but kept local — memory fusion is over `MemoryId`, not code.
const RRF_K: f64 = 60.0;

/// Oversample target for KNN/FTS before fusion: `4*n` capped at 200 (matches the
/// code path's oversample ceiling). Keeps a small `n` from starving fusion.
fn oversample(n: usize) -> usize {
    n.saturating_mul(4).clamp(8, 200)
}

/// RFC3339/ISO-8601 timestamp for "now" (UTC), the format every `*_at` column
/// stores (so `created_at`/`last_accessed_at` round-trip through chrono cleanly).
fn now_iso() -> String {
    Utc::now().to_rfc3339()
}

/// SHA-256 hex digest of `content` — the dedup/re-embed gate key.
fn content_hash(content: &str) -> String {
    let mut h = Sha256::new();
    h.update(content.as_bytes());
    format!("{:x}", h.finalize())
}

/// Encode an f32 slice as a little-endian byte blob (vec0's expected format).
fn f32_blob(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for f in v {
        out.extend_from_slice(&f.to_le_bytes());
    }
    out
}

/// Map any rusqlite error into the framework-free `Error::Store` variant.
fn store_err<E: std::fmt::Display>(e: E) -> Error {
    Error::Store(e.to_string())
}

// ──────────────────────────── category mapping ────────────────────────────

fn category_str(c: Category) -> &'static str {
    match c {
        Category::Decision => "decision",
        Category::Progress => "progress",
        Category::Context => "context",
        Category::Bug => "bug",
        Category::Todo => "todo",
        Category::Code => "code",
        Category::General => "general",
    }
}

fn parse_category(s: &str) -> Category {
    match s {
        "decision" => Category::Decision,
        "progress" => Category::Progress,
        "context" => Category::Context,
        "bug" => Category::Bug,
        "todo" => Category::Todo,
        "code" => Category::Code,
        _ => Category::General,
    }
}

fn parse_status(s: &str) -> MemoryStatus {
    match s {
        "resolved" => MemoryStatus::Resolved,
        _ => MemoryStatus::Active,
    }
}

// ──────────────────────────── the store ────────────────────────────

/// Central memory store. Cheap to share (the connection is behind an `Arc`).
pub struct SqliteMemoryStore {
    conn: Arc<Mutex<Connection>>,
    embedder: Arc<dyn Embedder>,
    dim: usize,
}

impl SqliteMemoryStore {
    /// Open (creating if needed) the central memory db at `central_db_path`.
    /// `~` in the path is expanded against `$HOME`. Registers sqlite-vec (reusing
    /// the per-process `Once` from the code store), sets WAL + foreign_keys, and
    /// applies the schema. A db with an INCOMPATIBLE `user_version` is a hard
    /// error — the central db holds the only copy of memories and is never wiped.
    pub fn open(central_db_path: &str, embedder: Arc<dyn Embedder>) -> Result<Self> {
        // vec0 must be registered BEFORE opening any connection that sees
        // `vec_memory`. Shared with the code store (registered once per process).
        register_sqlite_vec();

        let path = expand_tilde(central_db_path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::Io(e.to_string()))?;
        }

        let conn = Connection::open(&path).map_err(store_err)?;
        conn.pragma_update(None, "journal_mode", "WAL").map_err(store_err)?;
        conn.pragma_update(None, "foreign_keys", "ON").map_err(store_err)?;

        // Guard the schema generation. A FRESH db reports user_version 0 → stamp
        // it. A db at the current version is fine. Any OTHER version is an
        // incompatible central db → refuse (never silently delete memories).
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .map_err(store_err)?;
        if version != 0 && version != schema::SCHEMA_VERSION {
            return Err(Error::Store(format!(
                "central memory db at {} has incompatible schema_version {} (expected {}); \
                 refusing to touch it — back it up / migrate manually",
                path.display(),
                version,
                schema::SCHEMA_VERSION
            )));
        }

        conn.execute_batch(schema::SCHEMA).map_err(store_err)?;
        conn.pragma_update(None, "user_version", schema::SCHEMA_VERSION).map_err(store_err)?;

        // The `vec_memory` column width is FIXED at `schema::VEC_DIM` (vec0 has no
        // dim-templating). An embedder of a different dim would have every vector
        // silently rejected by vec0 — fail loudly instead.
        let dim = embedder.dim();
        if dim != schema::VEC_DIM {
            return Err(Error::DimMismatch {
                index: schema::VEC_DIM,
                got: dim,
            });
        }
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            embedder,
            dim,
        })
    }

    /// Embed one piece of text to a single vector, validating the dim against the
    /// `vec_memory` column width (a mismatch would be silently rejected by vec0).
    fn embed_one(&self, text: &str) -> Result<Vec<f32>> {
        let mut vecs = self.embedder.embed(&[text])?;
        let v = vecs
            .pop()
            .ok_or_else(|| Error::Embed("embedder returned no vector".into()))?;
        if v.len() != self.dim {
            return Err(Error::DimMismatch {
                index: self.dim,
                got: v.len(),
            });
        }
        Ok(v)
    }

    /// Resolve a TEXT mem id to its bridge rowid (`None` if unknown).
    fn rowid_for(conn: &Connection, id: &str) -> Result<Option<i64>> {
        conn.query_row(
            "SELECT rowid FROM mem_rowid WHERE mem_id = ?1",
            params![id],
            |r| r.get::<_, i64>(0),
        )
        .optional()
        .map_err(store_err)
    }
}

// ──────────────────────────── write side ────────────────────────────

impl WritableMemoryStore for SqliteMemoryStore {
    fn add(&self, mem: NewMemory) -> Result<AddOutcome> {
        // Embed OUTSIDE the lock (network call) so the write transaction is short.
        let vector = self.embed_one(&mem.content)?;
        let id = MemoryId::generate(&mem.project);
        let hash = content_hash(&mem.content);
        let tags_json = serde_json::to_string(&mem.tags).map_err(store_err)?;
        let now = now_iso();
        let model = self.embedder.model_id().to_string();

        let mut conn = self.conn.lock().map_err(store_err)?;
        let tx = conn.transaction().map_err(store_err)?;

        // (M4.2: a dedup gate would KNN here and short-circuit to Duplicate; for
        // now every add is a real insert.)
        tx.execute(
            "INSERT INTO memories \
             (id, project, content, category, tags, importance, status, \
              session_id, access_count, created_at, last_accessed_at, \
              embed_model, embed_dim, content_hash) \
             VALUES (?1,?2,?3,?4,?5,?6,'active',?7,0,?8,?8,?9,?10,?11)",
            params![
                id.as_str(),
                mem.project,
                mem.content,
                category_str(mem.category),
                tags_json,
                mem.importance as f64,
                mem.session_id,
                now,
                model,
                self.dim as i64,
                hash,
            ],
        )
        .map_err(store_err)?;

        // Bridge: TEXT id → INTEGER rowid for vec0/fts5.
        tx.execute("INSERT INTO mem_rowid (mem_id) VALUES (?1)", params![id.as_str()])
            .map_err(store_err)?;
        let rowid = tx.last_insert_rowid();

        tx.execute(
            "INSERT INTO vec_memory (rowid, embedding) VALUES (?1, ?2)",
            params![rowid, f32_blob(&vector)],
        )
        .map_err(store_err)?;
        tx.execute(
            "INSERT INTO fts_memory (rowid, content) VALUES (?1, ?2)",
            params![rowid, mem.content],
        )
        .map_err(store_err)?;

        tx.commit().map_err(store_err)?;
        Ok(AddOutcome::Added { id })
    }

    fn update(&self, id: &MemoryId, content: Option<&str>, tags: Option<&[String]>) -> Result<()> {
        // Re-embed only when the content actually changed (hash gate); compute the
        // new vector outside the lock.
        let new_content = match content {
            Some(c) => {
                let new_hash = content_hash(c);
                let old_hash: Option<String> = {
                    let conn = self.conn.lock().map_err(store_err)?;
                    conn.query_row(
                        "SELECT content_hash FROM memories WHERE id = ?1",
                        params![id.as_str()],
                        |r| r.get::<_, String>(0),
                    )
                    .optional()
                    .map_err(store_err)?
                };
                match old_hash {
                    None => return Err(Error::NotFound(id.to_string())),
                    Some(h) if h == new_hash => None, // unchanged → skip re-embed
                    Some(_) => Some((c.to_string(), new_hash, self.embed_one(c)?)),
                }
            }
            None => None,
        };

        let tags_json = match tags {
            Some(t) => Some(serde_json::to_string(t).map_err(store_err)?),
            None => None,
        };
        let now = now_iso();

        let mut conn = self.conn.lock().map_err(store_err)?;
        let tx = conn.transaction().map_err(store_err)?;

        if let Some((c, hash, vector)) = &new_content {
            let rowid = Self::rowid_for(&tx, id.as_str())?
                .ok_or_else(|| Error::NotFound(id.to_string()))?;
            tx.execute(
                "UPDATE memories SET content = ?1, content_hash = ?2, updated_at = ?3 WHERE id = ?4",
                params![c, hash, now, id.as_str()],
            )
            .map_err(store_err)?;
            tx.execute(
                "UPDATE vec_memory SET embedding = ?1 WHERE rowid = ?2",
                params![f32_blob(vector), rowid],
            )
            .map_err(store_err)?;
            // Standalone fts5: re-create the row (delete + insert by rowid).
            tx.execute("DELETE FROM fts_memory WHERE rowid = ?1", params![rowid])
                .map_err(store_err)?;
            tx.execute(
                "INSERT INTO fts_memory (rowid, content) VALUES (?1, ?2)",
                params![rowid, c],
            )
            .map_err(store_err)?;
        }

        if let Some(tj) = &tags_json {
            let n = tx
                .execute(
                    "UPDATE memories SET tags = ?1, updated_at = ?2 WHERE id = ?3",
                    params![tj, now, id.as_str()],
                )
                .map_err(store_err)?;
            if n == 0 && new_content.is_none() {
                return Err(Error::NotFound(id.to_string()));
            }
        }

        tx.commit().map_err(store_err)
    }

    fn delete(&self, id: &MemoryId) -> Result<()> {
        let mut conn = self.conn.lock().map_err(store_err)?;
        let tx = conn.transaction().map_err(store_err)?;

        // Resolve the rowid up front; mirror the delete into vec/fts/bridge in the
        // SAME transaction so no orphan vector or fts row can survive.
        let rowid = Self::rowid_for(&tx, id.as_str())?;
        tx.execute("DELETE FROM memories WHERE id = ?1", params![id.as_str()])
            .map_err(store_err)?;
        if let Some(r) = rowid {
            tx.execute("DELETE FROM vec_memory WHERE rowid = ?1", params![r])
                .map_err(store_err)?;
            tx.execute("DELETE FROM fts_memory WHERE rowid = ?1", params![r])
                .map_err(store_err)?;
            tx.execute("DELETE FROM mem_rowid WHERE rowid = ?1", params![r])
                .map_err(store_err)?;
        }

        tx.commit().map_err(store_err)
    }

    fn resolve(&self, id: &MemoryId, reason: &str) -> Result<()> {
        let now = now_iso();
        let conn = self.conn.lock().map_err(store_err)?;
        let n = conn
            .execute(
                "UPDATE memories SET status = 'resolved', resolved_at = ?1, \
                 resolve_reason = ?2, updated_at = ?1 WHERE id = ?3",
                params![now, reason, id.as_str()],
            )
            .map_err(store_err)?;
        if n == 0 {
            return Err(Error::NotFound(id.to_string()));
        }
        Ok(())
    }

    fn record_access(&self, ids: &[MemoryId]) -> Result<()> {
        // Best-effort warm-up: must never fail a read. Swallow lock/SQL errors.
        if ids.is_empty() {
            return Ok(());
        }
        let now = now_iso();
        let Ok(conn) = self.conn.lock() else {
            return Ok(());
        };
        for id in ids {
            let _ = conn.execute(
                "UPDATE memories SET access_count = access_count + 1, last_accessed_at = ?1 \
                 WHERE id = ?2",
                params![now, id.as_str()],
            );
        }
        Ok(())
    }

    fn upsert_project(&self, name: &str, root_path: &str) -> Result<()> {
        let now = now_iso();
        let conn = self.conn.lock().map_err(store_err)?;
        // Preserve created_at on an existing row; refresh root_path + onboarded_at.
        conn.execute(
            "INSERT INTO projects (name, root_path, created_at, onboarded_at) \
             VALUES (?1, ?2, ?3, ?3) \
             ON CONFLICT(name) DO UPDATE SET root_path = ?2, onboarded_at = ?3",
            params![name, root_path, now],
        )
        .map_err(store_err)?;
        Ok(())
    }

    fn touch_session(&self, project: &str, _session_id: &str) -> Result<()> {
        let now = now_iso();
        let conn = self.conn.lock().map_err(store_err)?;
        // Register the project on first touch, then stamp last_session_at.
        conn.execute(
            "INSERT INTO projects (name, created_at, last_session_at) VALUES (?1, ?2, ?2) \
             ON CONFLICT(name) DO UPDATE SET last_session_at = ?2",
            params![project, now],
        )
        .map_err(store_err)?;
        Ok(())
    }
}

// ──────────────────────────── read side ────────────────────────────

impl ReadableMemoryStore for SqliteMemoryStore {
    fn search(
        &self,
        project: &str,
        query: &str,
        n: usize,
        category: Option<Category>,
        include_resolved: bool,
    ) -> Result<Vec<MemoryHit>> {
        if query.trim().is_empty() || n == 0 {
            return Ok(vec![]);
        }
        let qvec = self.embed_one(query)?;
        let over = oversample(n);
        let conn = self.conn.lock().map_err(store_err)?;

        let semantic = knn_records(&conn, &qvec, over, Some(project), category, include_resolved)?;
        let lexical = fts_records(&conn, query, over, Some(project), category, include_resolved)?;
        drop(conn);

        Ok(rrf_fuse_memory(&[semantic, lexical], n))
    }

    fn search_all(&self, query: &str, n: usize, include_resolved: bool) -> Result<Vec<MemoryHit>> {
        if query.trim().is_empty() || n == 0 {
            return Ok(vec![]);
        }
        let qvec = self.embed_one(query)?;
        let over = oversample(n);
        let conn = self.conn.lock().map_err(store_err)?;

        // No project filter — one KNN over the whole space — but reserved
        // (`__*`) projects are excluded from cross-project recall.
        let semantic = knn_records(&conn, &qvec, over, None, None, include_resolved)?;
        let lexical = fts_records(&conn, query, over, None, None, include_resolved)?;
        drop(conn);

        let semantic = semantic.into_iter().filter(|r| !is_reserved_project(&r.project)).collect();
        let lexical = lexical.into_iter().filter(|r| !is_reserved_project(&r.project)).collect();

        Ok(rrf_fuse_memory(&[semantic, lexical], n))
    }

    fn list(
        &self,
        project: &str,
        category: Option<Category>,
        limit: usize,
        include_resolved: bool,
    ) -> Result<Vec<MemoryRecord>> {
        let conn = self.conn.lock().map_err(store_err)?;
        let mut sql = String::from(
            "SELECT id, project, content, category, tags, importance, status, resolved_at, \
                    resolve_reason, session_id, access_count, created_at, last_accessed_at, updated_at \
             FROM memories WHERE project = ?1",
        );
        let mut binds: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(project.to_string())];
        if let Some(c) = category {
            sql.push_str(" AND category = ?");
            binds.push(Box::new(category_str(c).to_string()));
        }
        if !include_resolved {
            sql.push_str(" AND status = 'active'");
        }
        sql.push_str(" ORDER BY created_at DESC LIMIT ?");
        binds.push(Box::new(limit as i64));

        let mut stmt = conn.prepare(&sql).map_err(store_err)?;
        let refs: Vec<&dyn rusqlite::types::ToSql> = binds.iter().map(|b| b.as_ref()).collect();
        let rows = stmt.query_map(refs.as_slice(), map_record).map_err(store_err)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(store_err)?);
        }
        Ok(out)
    }

    fn get(&self, id: &MemoryId) -> Result<Option<MemoryRecord>> {
        let conn = self.conn.lock().map_err(store_err)?;
        conn.query_row(
            "SELECT id, project, content, category, tags, importance, status, resolved_at, \
                    resolve_reason, session_id, access_count, created_at, last_accessed_at, updated_at \
             FROM memories WHERE id = ?1",
            params![id.as_str()],
            map_record,
        )
        .optional()
        .map_err(store_err)
    }

    fn list_projects(&self) -> Result<Vec<(String, u64)>> {
        let conn = self.conn.lock().map_err(store_err)?;
        let mut stmt = conn
            .prepare("SELECT project, COUNT(*) FROM memories GROUP BY project ORDER BY project")
            .map_err(store_err)?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64)))
            .map_err(store_err)?;
        let mut out = Vec::new();
        for r in rows {
            let (name, count) = r.map_err(store_err)?;
            // Namespace guard: reserved (`__*`) projects never appear in listings.
            if !is_reserved_project(&name) {
                out.push((name, count));
            }
        }
        Ok(out)
    }
}

// ──────────────────────────── retrieval helpers ────────────────────────────

/// KNN the memory vector space, re-hydrate neighbour rowids into `MemoryRecord`s
/// (via the bridge), and post-filter by project / category / status. vec0 cannot
/// pre-filter, so the project filter happens AFTER the KNN pull (hence oversample).
/// Records are returned best-first (ascending vec0 distance == descending
/// similarity), preserving rank for RRF.
fn knn_records(
    conn: &Connection,
    qvec: &[f32],
    k: usize,
    project: Option<&str>,
    category: Option<Category>,
    include_resolved: bool,
) -> Result<Vec<MemoryRecord>> {
    if k == 0 {
        return Ok(vec![]);
    }
    // vec0 KNN over the memory space, joined to the owning memory via the bridge.
    let mut stmt = conn
        .prepare(
            "SELECT m.id, m.project, m.content, m.category, m.tags, m.importance, m.status, \
                    m.resolved_at, m.resolve_reason, m.session_id, m.access_count, \
                    m.created_at, m.last_accessed_at, m.updated_at, v.distance \
             FROM vec_memory v \
             JOIN mem_rowid b ON b.rowid = v.rowid \
             JOIN memories m ON m.id = b.mem_id \
             WHERE v.embedding MATCH ?1 AND k = ?2 \
             ORDER BY v.distance",
        )
        .map_err(store_err)?;
    let rows = stmt
        .query_map(params![f32_blob(qvec), k as i64], map_record)
        .map_err(store_err)?;

    let mut out = Vec::new();
    for r in rows {
        let rec = r.map_err(store_err)?;
        if passes_filters(&rec, project, category, include_resolved) {
            out.push(rec);
        }
    }
    Ok(out)
}

/// Lexical (fts5 MATCH) retrieval over `fts_memory`, re-hydrated to records via
/// the bridge and post-filtered the same way as the dense path. bm25() orders
/// best-first (most-negative = most relevant). A malformed fts query yields no
/// hits rather than an error (M4.2 adds an FTS sanitizer).
fn fts_records(
    conn: &Connection,
    query: &str,
    k: usize,
    project: Option<&str>,
    category: Option<Category>,
    include_resolved: bool,
) -> Result<Vec<MemoryRecord>> {
    if k == 0 {
        return Ok(vec![]);
    }
    let mut stmt = conn
        .prepare(
            "SELECT m.id, m.project, m.content, m.category, m.tags, m.importance, m.status, \
                    m.resolved_at, m.resolve_reason, m.session_id, m.access_count, \
                    m.created_at, m.last_accessed_at, m.updated_at \
             FROM fts_memory f \
             JOIN mem_rowid b ON b.rowid = f.rowid \
             JOIN memories m ON m.id = b.mem_id \
             WHERE fts_memory MATCH ?1 \
             ORDER BY bm25(fts_memory) LIMIT ?2",
        )
        .map_err(store_err)?;

    let match_expr = fts_match_expr(query);
    let rows = match stmt.query_map(params![match_expr, k as i64], map_record) {
        Ok(rows) => rows,
        // A query fts5 cannot parse is a soft miss (the dense list still answers).
        Err(_) => return Ok(vec![]),
    };

    let mut out = Vec::new();
    for r in rows {
        match r {
            Ok(rec) if passes_filters(&rec, project, category, include_resolved) => out.push(rec),
            Ok(_) => {}
            Err(_) => return Ok(out), // tolerate a mid-stream fts error
        }
    }
    Ok(out)
}

/// Post-KNN/-FTS predicate: project (if scoped), category (if scoped), and the
/// active-only filter (unless `include_resolved`).
fn passes_filters(
    rec: &MemoryRecord,
    project: Option<&str>,
    category: Option<Category>,
    include_resolved: bool,
) -> bool {
    if let Some(p) = project {
        if rec.project != p {
            return false;
        }
    }
    if let Some(c) = category {
        if rec.category != c {
            return false;
        }
    }
    if !include_resolved && rec.status != MemoryStatus::Active {
        return false;
    }
    true
}

/// Build a safe fts5 MATCH expression: split the query into alphanumeric tokens,
/// quote each as a literal phrase, OR-join them. Empty → a token that matches
/// nothing. This avoids fts5 syntax errors from punctuation in NL queries (a full
/// NL/FTS sanitizer is M4.2).
fn fts_match_expr(query: &str) -> String {
    let tokens: Vec<String> = query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| format!("\"{}\"", t.to_lowercase()))
        .collect();
    if tokens.is_empty() {
        // A phrase that cannot match any real content.
        "\"\u{0}\"".to_string()
    } else {
        tokens.join(" OR ")
    }
}

/// Reciprocal-Rank-Fusion over several best-first record lists, keyed by memory
/// id. An item's fused score is `Σ 1/(RRF_K + rank)` (0-based rank per list). The
/// first/best record seen for an id carries it (deterministic representative);
/// the result is annotated into `MemoryHit`s (age + fused score), sorted by score
/// desc (ties → id), truncated to `top`.
fn rrf_fuse_memory(lists: &[Vec<MemoryRecord>], top: usize) -> Vec<MemoryHit> {
    use std::collections::HashMap;
    let now = Utc::now();
    // id -> (accumulated score, representative record)
    let mut acc: HashMap<String, (f64, MemoryRecord)> = HashMap::new();
    for list in lists {
        for (rank, rec) in list.iter().enumerate() {
            let contrib = 1.0 / (RRF_K + rank as f64);
            acc.entry(rec.id.0.clone())
                .and_modify(|(s, _)| *s += contrib)
                .or_insert_with(|| (contrib, rec.clone()));
        }
    }

    let mut fused: Vec<MemoryHit> = acc
        .into_iter()
        .map(|(_, (score, rec))| annotate(rec, score as f32, now))
        .collect();

    fused.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.record.id.0.cmp(&b.record.id.0))
    });
    fused.truncate(top);
    fused
}

/// Annotate a record with a fused/effective `score` and its age bucket. Age is
/// measured from `created_at` (falling back to `last_accessed_at`, then `now`).
fn annotate(record: MemoryRecord, score: f32, now: DateTime<Utc>) -> MemoryHit {
    let (age_days, age_status) = age_of(&record.created_at, &record.last_accessed_at, now);
    MemoryHit {
        record,
        score,
        age_days,
        age_status,
    }
}

/// Days since `created_at` (or `last_accessed_at` if `created_at` won't parse),
/// plus the bucket: ≤7 Fresh, ≤30 Recent, ≤90 Aging, else Stale. Unparseable
/// timestamps fall back to Fresh / 0 days (a malformed row must not crash recall).
fn age_of(created_at: &str, last_accessed_at: &str, now: DateTime<Utc>) -> (i64, AgeStatus) {
    let ts = parse_ts(created_at)
        .or_else(|| parse_ts(last_accessed_at))
        .unwrap_or(now);
    let days = (now - ts).num_days().max(0);
    let status = if days <= AGE_FRESH_MAX {
        AgeStatus::Fresh
    } else if days <= AGE_RECENT_MAX {
        AgeStatus::Recent
    } else if days <= AGE_AGING_MAX {
        AgeStatus::Aging
    } else {
        AgeStatus::Stale
    };
    (days, status)
}

fn parse_ts(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s).ok().map(|dt| dt.with_timezone(&Utc))
}

/// Map a `memories` SELECT row (14 columns, in the order every query above uses)
/// into a `MemoryRecord`. A trailing `distance` column (the dense path) is simply
/// not read, so the same mapper serves both retrieval and list/get.
fn map_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<MemoryRecord> {
    let tags_json: String = row.get(4)?;
    let tags: Vec<String> = serde_json::from_str(&tags_json).unwrap_or_default();
    let category: String = row.get(3)?;
    let status: String = row.get(6)?;
    Ok(MemoryRecord {
        id: MemoryId(row.get::<_, String>(0)?),
        project: row.get(1)?,
        content: row.get(2)?,
        category: parse_category(&category),
        tags,
        importance: row.get::<_, f64>(5)? as f32,
        status: parse_status(&status),
        resolved_at: row.get(7)?,
        resolve_reason: row.get(8)?,
        session_id: row.get(9)?,
        access_count: row.get::<_, i64>(10)? as u64,
        created_at: row.get(11)?,
        last_accessed_at: row.get(12)?,
        updated_at: row.get(13)?,
    })
}

/// Expand a leading `~` against `$HOME`. A non-`~` path is returned as-is. Used so
/// the default `~/.icode/icode.db` resolves without a shell.
fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return Path::new(&home).join(rest);
        }
    }
    if path == "~" {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home);
        }
    }
    PathBuf::from(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fts_match_expr_tokenizes_and_quotes() {
        assert_eq!(fts_match_expr("race condition"), "\"race\" OR \"condition\"");
        // Punctuation is stripped; NL noise can't break fts5 syntax.
        assert_eq!(fts_match_expr("auth: race-condition!"), "\"auth\" OR \"race\" OR \"condition\"");
        // No real tokens → an unmatchable phrase (not an fts syntax error).
        assert_eq!(fts_match_expr("!!! ???"), "\"\u{0}\"");
    }

    #[test]
    fn age_buckets_from_days() {
        let now = Utc::now();
        let mk = |days: i64| (now - chrono::Duration::days(days)).to_rfc3339();
        assert_eq!(age_of(&mk(0), "", now).1, AgeStatus::Fresh);
        assert_eq!(age_of(&mk(7), "", now).1, AgeStatus::Fresh);
        assert_eq!(age_of(&mk(8), "", now).1, AgeStatus::Recent);
        assert_eq!(age_of(&mk(30), "", now).1, AgeStatus::Recent);
        assert_eq!(age_of(&mk(31), "", now).1, AgeStatus::Aging);
        assert_eq!(age_of(&mk(90), "", now).1, AgeStatus::Aging);
        assert_eq!(age_of(&mk(91), "", now).1, AgeStatus::Stale);
    }

    #[test]
    fn unparseable_created_at_falls_back_to_last_accessed() {
        let now = Utc::now();
        let la = (now - chrono::Duration::days(40)).to_rfc3339();
        let (days, status) = age_of("not-a-date", &la, now);
        assert_eq!(days, 40);
        assert_eq!(status, AgeStatus::Aging);
    }

    #[test]
    fn rrf_promotes_shared_id() {
        let rec = |id: &str| MemoryRecord {
            id: MemoryId(id.into()),
            project: "p".into(),
            content: "c".into(),
            category: Category::General,
            tags: vec![],
            importance: 0.0,
            status: MemoryStatus::Active,
            resolved_at: None,
            resolve_reason: None,
            session_id: None,
            access_count: 0,
            created_at: Utc::now().to_rfc3339(),
            last_accessed_at: Utc::now().to_rfc3339(),
            updated_at: None,
        };
        // "x" appears in both lists → must outrank single-list heads.
        let a = vec![rec("x"), rec("a"), rec("b")];
        let b = vec![rec("y"), rec("x"), rec("c")];
        let fused = rrf_fuse_memory(&[a, b], 10);
        assert_eq!(fused[0].record.id.0, "x");
    }
}
