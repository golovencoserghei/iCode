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

use super::ranking::effective_score;
use super::sanitizer::{sanitize_fts5, sanitize_nl};
use super::schema;
use crate::store::register_sqlite_vec;

/// Tunable for how strongly the usage-aware ranking signal nudges RRF relevance.
/// The fused score is `rrf_score + RANK_BLEND * effective_score`, so this is the
/// weight of a *tiebreak/nudge*, NOT a co-equal term — RRF still dominates
/// ordering, ranking only lifts important/L0 records that are otherwise close in
/// relevance so they don't sink. See `rrf_fuse_memory`.
const RANK_BLEND: f32 = 0.1;

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

/// Decode a little-endian f32 byte blob back into a vector (inverse of
/// [`f32_blob`]). On a plain (non-`MATCH`) select, vec0 returns a stored vector as
/// exactly this raw blob, which the project-scoped retrieval decodes to score
/// cosine distance in Rust. Trailing bytes that don't form a full f32 are ignored.
fn blob_to_f32(blob: &[u8]) -> Vec<f32> {
    blob.chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect()
}

/// Cosine distance between two vectors, matching sqlite-vec's vec0
/// `distance_metric=cosine` EXACTLY: `1 - dot/(|a|*|b|)`, accumulated in f32 so a
/// distance computed here is directly comparable to a vec0 KNN distance — the
/// `dedup_distance` gate was calibrated against vec0's own value. A zero-norm
/// operand yields the maximal distance `1.0` rather than a NaN that would poison
/// ordering. `a`/`b` are expected to be the same length (the store's fixed dim);
/// any excess tail is ignored.
fn cosine_distance(a: &[f32], b: &[f32]) -> f32 {
    let n = a.len().min(b.len());
    let mut dot = 0f32;
    let mut a_mag = 0f32;
    let mut b_mag = 0f32;
    for i in 0..n {
        dot += a[i] * b[i];
        a_mag += a[i] * a[i];
        b_mag += b[i] * b[i];
    }
    let denom = a_mag.sqrt() * b_mag.sqrt();
    if denom == 0.0 {
        return 1.0;
    }
    1.0 - dot / denom
}

/// Map any rusqlite error into the framework-free `Error::Store` variant.
fn store_err<E: std::fmt::Display>(e: E) -> Error {
    Error::Store(e.to_string())
}

// ──────────────────────────── read-only stub embedder ────────────────────────────

/// A do-nothing embedder for [`SqliteMemoryStore::open_readonly`].
///
/// Its `dim()` equals `schema::VEC_DIM` so the `open` dim guard passes, but it owns
/// no backend: `embed` always errors. This lets the embed-FREE read methods
/// (`list` / `get` / `list_projects` / `record_access`) work with no Ollama, while
/// any path that would actually vectorise (`search` / `add` / `update` content)
/// fails loudly with `Error::Embed` instead of producing bogus vectors.
struct NoEmbedder;

impl Embedder for NoEmbedder {
    fn model_id(&self) -> &str {
        "noop-readonly"
    }
    fn dim(&self) -> usize {
        schema::VEC_DIM
    }
    fn backend(&self) -> &str {
        "noop"
    }
    fn embed(&self, _texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        Err(Error::Embed(
            "memory store opened read-only (no embedder): semantic search and writes are unavailable"
                .into(),
        ))
    }
    fn health(&self) -> Result<()> {
        Err(Error::Embed("read-only memory store has no embedding backend".into()))
    }
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
    /// Near-duplicate gate distance. An `add` whose nearest same-project neighbour
    /// sits at a vec0 cosine distance BELOW this is rejected as a `Duplicate`
    /// rather than inserted. Calibrated against the legacy embedding model; for
    /// qwen3-embedding:0.6b the `IndexConfig::dedup_distance` default (0.12) is a
    /// reasonable starting point — tune via `with_dedup_distance` / config.
    dedup_distance: f32,
    /// Usage-aware ranking weights (importance/access boost/decay/L0 floor). Used
    /// in `search`/`search_all` to nudge RRF relevance so important/L0 records do
    /// not sink. Tune via `with_ranking` / config.
    ranking: icode_core::config::RankingConfig,
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
        // Inter-process write contention guard: the central db is shared by the
        // daemon and any number of CLI/MCP sessions. WAL lets readers proceed
        // during a writer, but two writers still serialize on the db lock — wait
        // up to 5s for it instead of failing fast with SQLITE_BUSY. (In-process
        // the `Mutex<Connection>` already serializes; this is the cross-process
        // belt-and-braces.)
        conn.pragma_update(None, "busy_timeout", 5000_i64).map_err(store_err)?;

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
            // Default to the shared IndexConfig gate (0.12). Override with
            // `with_dedup_distance` from the resolved IcodeConfig at wiring time.
            dedup_distance: icode_core::config::IndexConfig::default().dedup_distance,
            ranking: icode_core::config::RankingConfig::default(),
        })
    }

    /// Open the central memory db for READ-ONLY access, WITHOUT a live embedder.
    ///
    /// The hooks (`icode hook precompact`, and the degraded `session-start`) only
    /// need the embed-free read methods (`list` / `get` / `list_projects` /
    /// `record_access`), which never touch the embedder — so they must work even
    /// when Ollama is down. `open` requires an `Arc<dyn Embedder>` and validates its
    /// dim against `vec_memory`; here we install an internal [`NoEmbedder`] stub
    /// whose `dim()` matches the vec0 column width (so the dim guard passes) but
    /// whose `embed()` always errors. The result: the read list path is fully
    /// functional, while `search` / `add` / `update` (the embed paths) return an
    /// `Error::Embed` — exactly the "read-only, no semantic search" contract.
    ///
    /// Used by the lifecycle hooks so L0 rules can be re-injected at compaction
    /// even with no embedding backend available. The db itself is opened normally
    /// (it is never wiped on a version mismatch — same guard as `open`).
    pub fn open_readonly(central_db_path: &str) -> Result<Self> {
        Self::open(central_db_path, Arc::new(NoEmbedder))
    }

    /// Set the near-duplicate gate distance (builder style). Wire the resolved
    /// `IcodeConfig.index.dedup_distance` here so the threshold is config-driven
    /// rather than the hardcoded default.
    pub fn with_dedup_distance(mut self, dedup_distance: f32) -> Self {
        self.dedup_distance = dedup_distance;
        self
    }

    /// Set the usage-aware ranking config (builder style). Wire the resolved
    /// `IcodeConfig.ranking` here so search blending is config-driven.
    pub fn with_ranking(mut self, ranking: icode_core::config::RankingConfig) -> Self {
        self.ranking = ranking;
        self
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

    /// Best-effort access warm-up after a search: bump access_count /
    /// last_accessed_at for the returned ids so usage-aware ranking actually
    /// moves (without this, access_count stays 0 and the access boost is dead).
    /// Swallows errors — a warm-up must never fail a read.
    fn warm_up(&self, hits: &[MemoryHit]) {
        if hits.is_empty() {
            return;
        }
        let ids: Vec<MemoryId> = hits.iter().map(|h| h.record.id.clone()).collect();
        let _ = self.record_access(&ids);
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

        // Dedup gate: KNN the SAME-project memory space for the nearest existing
        // vector. If it sits below the gate distance, this content is a near-
        // duplicate — return the existing id WITHOUT inserting (a normal outcome,
        // not an error). vec0 cannot pre-filter, so we oversample a few neighbours
        // and post-filter to the project, taking the closest survivor. The
        // threshold is calibrated for the legacy model; for qwen3 the 0.12 default
        // is a reasonable start (configurable via `with_dedup_distance`).
        if let Some((existing, distance)) = nearest_same_project(&conn, &vector, &mem.project)? {
            if distance < self.dedup_distance {
                return Ok(AddOutcome::Duplicate { existing, distance });
            }
        }

        let tx = conn.transaction().map_err(store_err)?;

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
        // Defend recall: a prompt-polluted NL query is reduced to its real ask
        // BEFORE embedding (a 2000-char system prompt otherwise swamps the dense
        // vector). The lexical arm sanitizes FTS5 syntax separately (in `fts_records`).
        let clean = sanitize_nl(query);
        let qvec = self.embed_one(&clean)?;
        let over = oversample(n);
        let conn = self.conn.lock().map_err(store_err)?;

        // Per-project dense recall is a PROJECT-SCOPED brute force, NOT a global
        // vec0 KNN + post-filter. vec0 cannot pre-filter by project, so in the
        // shared central db the global top-`over` can be dominated by OTHER
        // projects' vectors — starving this project's true-nearest rows even though
        // they sit just past the cutoff. Scanning only this project's vectors makes
        // recall independent of how large the cross-project db grows.
        let semantic =
            knn_records_in_project(&conn, &qvec, over, project, category, include_resolved)?;
        // The lexical arm is project-scoped IN SQL (the project filter is pushed
        // below the bm25 LIMIT), immune to the same starvation — see `fts_records`.
        let lexical = fts_records(&conn, &clean, over, Some(project), category, include_resolved)?;
        drop(conn);

        let hits = rrf_fuse_memory(&[semantic, lexical], n, &self.ranking);
        self.warm_up(&hits);
        Ok(hits)
    }

    fn search_all(&self, query: &str, n: usize, include_resolved: bool) -> Result<Vec<MemoryHit>> {
        if query.trim().is_empty() || n == 0 {
            return Ok(vec![]);
        }
        let clean = sanitize_nl(query);
        let qvec = self.embed_one(&clean)?;
        let over = oversample(n);
        let conn = self.conn.lock().map_err(store_err)?;

        // No project filter — one KNN over the whole space — but reserved
        // (`__*`) projects are excluded from cross-project recall.
        let semantic = knn_records(&conn, &qvec, over, None, None, include_resolved)?;
        let lexical = fts_records(&conn, &clean, over, None, None, include_resolved)?;
        drop(conn);

        let semantic = semantic.into_iter().filter(|r| !is_reserved_project(&r.project)).collect();
        let lexical = lexical.into_iter().filter(|r| !is_reserved_project(&r.project)).collect();

        let hits = rrf_fuse_memory(&[semantic, lexical], n, &self.ranking);
        self.warm_up(&hits);
        Ok(hits)
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

// ──────────────────────────── dedup gate ────────────────────────────

/// Nearest existing SAME-project memory to `vector`: `(id, distance)` of the
/// closest record after a PROJECT-SCOPED brute-force scan, or `None` if the
/// project has no vectors yet. Used by the `add` dedup gate.
///
/// Why brute force instead of a vec0 KNN + post-filter: vec0 cannot pre-filter by
/// project, so a global k-NN over the shared central db could return only OTHER
/// projects' vectors — arbitrarily many of them closer than this project's true
/// near-duplicate — and never surface it. The gate would then miss the dup and let
/// duplicates accumulate as the cross-project db grows. Scanning ONLY this
/// project's vectors makes the gate independent of the rest of the db; rows-per-
/// project are few, so the scan is cheap. Resolved memories still count as
/// duplicates (re-adding a resolved fact is still a dup), so no status filter.
fn nearest_same_project(
    conn: &Connection,
    vector: &[f32],
    project: &str,
) -> Result<Option<(MemoryId, f32)>> {
    let mut best: Option<(MemoryId, f32)> = None;
    for (rec, v) in project_vectors(conn, project, None, true)? {
        let dist = cosine_distance(vector, &v);
        if best.as_ref().map_or(true, |(_, d)| dist < *d) {
            best = Some((rec.id, dist));
        }
    }
    Ok(best)
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

/// Decode EVERY same-project memory vector into `(record, vector)` pairs, applying
/// the optional category / active-only filters IN SQL so the candidate set is
/// bounded by the PROJECT — never by the size of the shared central db. This is
/// the shared primitive behind the project-scoped dense search
/// (`knn_records_in_project`) and the dedup gate (`nearest_same_project`).
///
/// The vectors are read straight out of the vec0 `vec_memory` column: on a plain
/// (non-`MATCH`) select vec0 returns each stored vector as its raw little-endian
/// f32 blob, which `blob_to_f32` inverts. vec0 cannot pre-filter by project, but
/// the join drives FROM `memories` (project-indexed) and looks the vector up by
/// `rowid` — a vec0 POINT query — so ONLY this project's vectors are ever decoded.
fn project_vectors(
    conn: &Connection,
    project: &str,
    category: Option<Category>,
    include_resolved: bool,
) -> Result<Vec<(MemoryRecord, Vec<f32>)>> {
    let mut sql = String::from(
        "SELECT m.id, m.project, m.content, m.category, m.tags, m.importance, m.status, \
                m.resolved_at, m.resolve_reason, m.session_id, m.access_count, \
                m.created_at, m.last_accessed_at, m.updated_at, v.embedding \
         FROM memories m \
         JOIN mem_rowid b ON b.mem_id = m.id \
         JOIN vec_memory v ON v.rowid = b.rowid \
         WHERE m.project = ?1",
    );
    let mut binds: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(project.to_string())];
    if let Some(c) = category {
        sql.push_str(" AND m.category = ?");
        binds.push(Box::new(category_str(c).to_string()));
    }
    if !include_resolved {
        sql.push_str(" AND m.status = 'active'");
    }
    let mut stmt = conn.prepare(&sql).map_err(store_err)?;
    let refs: Vec<&dyn rusqlite::types::ToSql> = binds.iter().map(|b| b.as_ref()).collect();
    let rows = stmt
        .query_map(refs.as_slice(), |row| {
            let rec = map_record(row)?;
            let blob: Vec<u8> = row.get(14)?;
            Ok((rec, blob))
        })
        .map_err(store_err)?;
    let mut out = Vec::new();
    for r in rows {
        let (rec, blob) = r.map_err(store_err)?;
        out.push((rec, blob_to_f32(&blob)));
    }
    Ok(out)
}

/// Project-scoped dense KNN: rank THIS project's vectors by cosine distance to
/// `qvec` in Rust and return the `k` nearest, best-first (ascending distance ==
/// descending similarity), preserving rank for RRF. Unlike a global vec0 KNN with
/// a project post-filter, the result cannot be starved by other projects' rows as
/// the shared db grows — see `project_vectors`.
fn knn_records_in_project(
    conn: &Connection,
    qvec: &[f32],
    k: usize,
    project: &str,
    category: Option<Category>,
    include_resolved: bool,
) -> Result<Vec<MemoryRecord>> {
    if k == 0 {
        return Ok(vec![]);
    }
    let mut scored: Vec<(f32, MemoryRecord)> =
        project_vectors(conn, project, category, include_resolved)?
            .into_iter()
            .map(|(rec, v)| (cosine_distance(qvec, &v), rec))
            .collect();
    // Ascending cosine distance == descending similarity → best-first for RRF.
    scored.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(k);
    Ok(scored.into_iter().map(|(_, rec)| rec).collect())
}

/// Lexical (fts5 MATCH) retrieval over `fts_memory`, re-hydrated to records via the
/// bridge. bm25() orders best-first (most-negative = most relevant). A malformed
/// fts query yields no hits rather than an error.
///
/// When `project`/`category`/status is scoped, the filter is pushed INTO the SQL
/// (below the bm25 `LIMIT`), NOT applied after — otherwise the lexical arm would
/// degrade exactly like a global dense KNN: the `k` best-bm25 rows could all belong
/// to OTHER projects in a large shared db, leaving this project's lexical hits
/// unreturned. Scoping in SQL keeps per-project lexical recall independent of the
/// cross-project db size. `project = None` (cross-project `search_all`) is a plain
/// global bm25 pull.
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
    let mut sql = String::from(
        "SELECT m.id, m.project, m.content, m.category, m.tags, m.importance, m.status, \
                m.resolved_at, m.resolve_reason, m.session_id, m.access_count, \
                m.created_at, m.last_accessed_at, m.updated_at \
         FROM fts_memory f \
         JOIN mem_rowid b ON b.rowid = f.rowid \
         JOIN memories m ON m.id = b.mem_id \
         WHERE fts_memory MATCH ?",
    );
    let mut binds: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(sanitize_fts5(query))];
    if let Some(p) = project {
        sql.push_str(" AND m.project = ?");
        binds.push(Box::new(p.to_string()));
    }
    if let Some(c) = category {
        sql.push_str(" AND m.category = ?");
        binds.push(Box::new(category_str(c).to_string()));
    }
    if !include_resolved {
        sql.push_str(" AND m.status = 'active'");
    }
    sql.push_str(" ORDER BY bm25(fts_memory) LIMIT ?");
    binds.push(Box::new(k as i64));

    let mut stmt = conn.prepare(&sql).map_err(store_err)?;
    let refs: Vec<&dyn rusqlite::types::ToSql> = binds.iter().map(|b| b.as_ref()).collect();
    let rows = match stmt.query_map(refs.as_slice(), map_record) {
        Ok(rows) => rows,
        // A query fts5 cannot parse is a soft miss (the dense list still answers).
        Err(_) => return Ok(vec![]),
    };

    let mut out = Vec::new();
    for r in rows {
        match r {
            Ok(rec) => out.push(rec),
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

/// Reciprocal-Rank-Fusion over several best-first record lists, keyed by memory
/// id, with a usage-aware ranking nudge. An item's RRF relevance is
/// `Σ 1/(RRF_K + rank)` (0-based rank per list); the FINAL fused score is
/// `rrf_score + RANK_BLEND * effective_score(rec)`.
///
/// Why blend rather than replace: RRF measures relevance to *this* query, while
/// `effective_score` (importance + access boost − decay, L0-immune) measures
/// standing relevance. We keep RRF as the dominant term and add a small
/// (`RANK_BLEND = 0.1`) ranking lift so an important/L0 record that is already
/// close in relevance does not sink below near-tied noise — but a genuinely
/// irrelevant high-importance record (absent from both retrieval lists) never
/// appears here at all, so it can't be force-promoted. The first/best record seen
/// for an id is the representative; results are annotated into `MemoryHit`s
/// (age + final score), sorted by score desc (ties → id), truncated to `top`.
fn rrf_fuse_memory(
    lists: &[Vec<MemoryRecord>],
    top: usize,
    ranking: &icode_core::config::RankingConfig,
) -> Vec<MemoryHit> {
    use std::collections::HashMap;
    let now = Utc::now();
    // id -> (accumulated rrf score, representative record)
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
        .map(|(_, (rrf_score, rec))| {
            // Final score = RRF relevance + small usage-aware nudge.
            let final_score = rrf_score as f32 + RANK_BLEND * effective_score(&rec, ranking, now);
            annotate(rec, final_score, now)
        })
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

    // (fts MATCH expression building moved to `sanitizer::sanitize_fts5`, tested
    // in that module.)

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
        // "x" appears in both lists → must outrank single-list heads. All records
        // share importance 0 / access 0, so the ranking blend is uniform and RRF
        // alone decides the order here.
        let a = vec![rec("x"), rec("a"), rec("b")];
        let b = vec![rec("y"), rec("x"), rec("c")];
        let fused = rrf_fuse_memory(&[a, b], 10, &icode_core::config::RankingConfig::default());
        assert_eq!(fused[0].record.id.0, "x");
    }

    #[test]
    fn ranking_blend_lifts_l0_among_near_ties() {
        // Two records sharing rank 0 in one list each (equal RRF). One is L0
        // (importance 5), the other importance 0. The 0.1*effective_score blend
        // must tip the tie to the L0 record.
        let mk = |id: &str, importance: f32| MemoryRecord {
            id: MemoryId(id.into()),
            project: "p".into(),
            content: "c".into(),
            category: Category::General,
            tags: vec![],
            importance,
            status: MemoryStatus::Active,
            resolved_at: None,
            resolve_reason: None,
            session_id: None,
            access_count: 0,
            created_at: Utc::now().to_rfc3339(),
            last_accessed_at: Utc::now().to_rfc3339(),
            updated_at: None,
        };
        let a = vec![mk("low", 0.0)];
        let b = vec![mk("l0", 5.0)];
        let fused = rrf_fuse_memory(&[a, b], 10, &icode_core::config::RankingConfig::default());
        assert_eq!(fused[0].record.id.0, "l0", "L0 record wins the RRF tie via the ranking blend");
    }

    // ─────────────── project-scoped retrieval / dedup (fake embedder) ───────────────
    //
    // These exercise the FULL store (open → add → embed → vec0/fts → search/dedup)
    // WITHOUT Ollama, using a deterministic angle embedder so records sit at exact
    // cosine distances. They prove the fix for the shared-db degradation: with the
    // legacy GLOBAL vec0 KNN + post-filter, a project's true-nearest rows are lost
    // once enough OTHER projects' vectors crowd the top-k; the project-scoped scans
    // here are independent of that.

    /// Ollama-free embedder for the retrieval tests. It reads an angle directive
    /// `#a<radians>` from the text and returns the unit vector `(cos θ, sin θ, 0, …)`
    /// in `VEC_DIM` space. Two texts then sit at vec0 cosine distance
    /// `1 - cos(θ₁-θ₂)`, so a test can place records at EXACT distances from a query
    /// and from each other — no embedding backend required. Absent directive → θ=0.
    struct AngleEmbedder;

    impl AngleEmbedder {
        fn angle_of(text: &str) -> f32 {
            if let Some(pos) = text.find("#a") {
                let rest = &text[pos + 2..];
                let end = rest.find(' ').unwrap_or(rest.len());
                if let Ok(v) = rest[..end].parse::<f32>() {
                    return v;
                }
            }
            0.0
        }
    }

    impl Embedder for AngleEmbedder {
        fn model_id(&self) -> &str {
            "angle-fake"
        }
        fn dim(&self) -> usize {
            schema::VEC_DIM
        }
        fn backend(&self) -> &str {
            "fake"
        }
        fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
            Ok(texts
                .iter()
                .map(|t| {
                    let theta = Self::angle_of(t);
                    let mut v = vec![0f32; schema::VEC_DIM];
                    v[0] = theta.cos();
                    v[1] = theta.sin();
                    v
                })
                .collect())
        }
        fn health(&self) -> Result<()> {
            Ok(())
        }
    }

    fn open_fake_store() -> (SqliteMemoryStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("icode.db");
        let store = SqliteMemoryStore::open(db.to_str().unwrap(), Arc::new(AngleEmbedder))
            .expect("open fake store");
        (store, dir)
    }

    fn add_mem(store: &SqliteMemoryStore, project: &str, content: &str) -> AddOutcome {
        store
            .add(NewMemory {
                project: project.into(),
                content: content.into(),
                category: Category::General,
                tags: vec![],
                importance: 0.0,
                session_id: None,
            })
            .expect("add")
    }

    #[test]
    fn per_project_search_survives_a_large_foreign_db() {
        let (store, _dir) = open_fake_store();

        // Target project P: 3 DISTINCT relevant records (well-separated angles so
        // the project-scoped dedup gate does not reject them as near-duplicates of
        // each other). P's BEST match to the query sits at angle 0.12.
        let mut target_ids = Vec::new();
        for (w, a) in [("alpha", "0.12"), ("beta", "0.90"), ("gamma", "1.60")] {
            match add_mem(&store, "P", &format!("topicword {w} #a{a}")) {
                AddOutcome::Added { id } => target_ids.push(id),
                other => panic!("expected Added for {w}, got {other:?}"),
            }
        }
        assert_eq!(target_ids.len(), 3);

        // 40 FOREIGN records, EACH IN ITS OWN project (iCode's premise: many
        // projects share one vector space) and each STRICTLY closer to the query
        // than P's best match (angles 0.001..0.040 → distance < P's 0.0072). Under
        // the legacy global oversample (`over == 20`) these crowd P's true-nearest
        // rows out of the KNN top-k. Distinct projects also keep the per-project
        // dedup gate from rejecting them.
        for i in 0..40 {
            let theta = 0.001 * (i as f32 + 1.0);
            match add_mem(&store, &format!("other{i}"), &format!("noise{i} #a{theta}")) {
                AddOutcome::Added { .. } => {}
                other => panic!("expected Added for noise{i}, got {other:?}"),
            }
        }

        let query = "topicword #a0.0";
        let over = oversample(5); // == 20, the legacy KNN pull width

        // Prove the legacy failure mode: a GLOBAL vec0 KNN (project = None) — what
        // the old per-project `search` post-filtered — returns ZERO rows of P; they
        // are all past position `over`.
        {
            let qvec = store.embed_one(query).unwrap();
            let conn = store.conn.lock().unwrap();
            let global = knn_records(&conn, &qvec, over, None, None, false).unwrap();
            assert_eq!(global.len(), over, "global KNN fills up entirely on foreign rows");
            assert!(
                !global.iter().any(|r| r.project == "P"),
                "legacy global top-{over} is 100% foreign — P's hits would be lost"
            );
        }

        // The fixed, project-scoped search still returns P's relevant records.
        let hits = store.search("P", query, 5, None, false).expect("search");
        assert!(!hits.is_empty(), "project-scoped recall is non-empty");
        assert!(hits.iter().all(|h| h.record.project == "P"), "only P rows returned");
        let got: std::collections::HashSet<_> =
            hits.iter().map(|h| h.record.id.clone()).collect();
        for id in &target_ids {
            assert!(got.contains(id), "P's relevant record {id:?} must survive recall");
        }
    }

    #[test]
    fn dedup_is_project_scoped_against_foreign_neighbours() {
        let (store, _dir) = open_fake_store();

        // An existing canonical record in project P at angle 0.2010.
        let existing = match add_mem(&store, "P", "canonical fact #a0.2010") {
            AddOutcome::Added { id } => id,
            other => panic!("expected Added, got {other:?}"),
        };

        // FOREIGN records (each in its OWN project, so the per-project dedup does
        // not reject them) sitting even CLOSER to the incoming vector (angle 0.2000)
        // than P's own record: angles 0.20001..0.20004 (Δ ≤ 0.00004) beat P's
        // Δ = 0.001. The legacy dedup pulled DEDUP_K = 3 GLOBAL neighbours and
        // post-filtered to P — these foreign rows would fill that top-3 and hide P's
        // true near-duplicate, letting the dup slip in.
        for (i, a) in ["0.20001", "0.20002", "0.20003", "0.20004"].iter().enumerate() {
            match add_mem(&store, &format!("other{i}"), &format!("foreign{i} #a{a}")) {
                AddOutcome::Added { .. } => {}
                other => panic!("expected Added for foreign{i}, got {other:?}"),
            }
        }

        // Prove the legacy failure mode: the 3 GLOBAL nearest to the incoming vector
        // are all foreign, so a global-K dedup would see no P row → no duplicate.
        {
            let incoming = store.embed_one("near duplicate #a0.2000").unwrap();
            let conn = store.conn.lock().unwrap();
            let mut stmt = conn
                .prepare(
                    "SELECT m.project FROM vec_memory v \
                     JOIN mem_rowid b ON b.rowid = v.rowid \
                     JOIN memories m ON m.id = b.mem_id \
                     WHERE v.embedding MATCH ?1 AND k = ?2 ORDER BY v.distance",
                )
                .unwrap();
            let projs: Vec<String> = stmt
                .query_map(rusqlite::params![f32_blob(&incoming), 3_i64], |r| {
                    r.get::<_, String>(0)
                })
                .unwrap()
                .map(|r| r.unwrap())
                .collect();
            assert_eq!(projs.len(), 3);
            assert!(
                projs.iter().all(|p| p != "P"),
                "legacy global top-3 is all foreign (no P row): {projs:?}"
            );
        }

        // The fixed, project-scoped dedup DOES catch the near-duplicate in P.
        match add_mem(&store, "P", "near duplicate #a0.2000") {
            AddOutcome::Duplicate { existing: hit, distance } => {
                assert_eq!(hit, existing, "dup resolves to P's canonical record");
                assert!(
                    distance < store.dedup_distance,
                    "distance {distance} must be under the gate {}",
                    store.dedup_distance
                );
            }
            other => panic!("expected Duplicate, got {other:?}"),
        }
    }
}
