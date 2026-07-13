//! `SqliteCodeStore` — the per-project code-graph store (rusqlite, bundled).
//!
//! M0.5 walking skeleton: implements the real `files`/`functions`/FTS5 paths of
//! the frozen `CodeReadStore`/`CodeWriteStore` contracts; everything outside the
//! Rust-function vertical slice is a `not implemented (M1+)` stub so the seams
//! compile and the smoke test (index → stats → search) runs end to end.
//!
//! `rusqlite::Connection` is `Send` but not `Sync`; the contract traits require
//! `Send + Sync`, so the connection is guarded by a `Mutex`. Single-writer SQLite
//! makes this the correct serialization point anyway.

mod schema;
mod vector;

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Once};

use icode_core::error::{Error, Result};
use icode_core::model::*;
use icode_core::traits::{CodeReadStore, CodeWriteStore};
use regex::Regex;
use rusqlite::Connection;

pub use vector::Vec0Index;

/// Schema generation. Bumped whenever the on-disk layout changes incompatibly;
/// a per-project index with a different `PRAGMA user_version` is discarded and
/// rebuilt from source (the index is disposable).
///
/// v2 (M2): adds the semantic layer — `meta`, `code_chunks`, the `vec_code` vec0
/// virtual table, and the delete-mirror trigger. A v1 db is wiped and rebuilt.
///
/// v3 (Wave 1): receiver-aware call resolution — `calls.resolved_callee`
/// (qualified target) + `calls.confidence` (how the edge resolved). Discard-and-
/// rebuild handles the added columns; no in-place migration.
const SCHEMA_VERSION: i64 = 5;

/// Gate the one-time sqlite-vec auto-extension registration.
static VEC_INIT: Once = Once::new();

/// Register the sqlite-vec (vec0) extension as a SQLite auto-extension exactly
/// ONCE per process. Auto-extensions run for every `Connection` opened *after*
/// registration, so this MUST be called before any connection that touches a
/// `vec0` table — hence it runs at the top of `open()` / `open_in_memory()`.
///
/// Mirrors the proven `spikes/vec_spike.rs` path: register the C init function
/// via `sqlite3_auto_extension`. `Once` makes a double registration impossible
/// even across threads (a second registration is harmless but wasteful).
pub(crate) fn register_sqlite_vec() {
    VEC_INIT.call_once(|| {
        // SAFETY: transmuting the well-typed `sqlite3_vec_init` to the
        // `sqlite3_auto_extension` entry-point signature is the documented usage
        // for sqlite-vec; it is the exact pattern validated by the M0 spike. The
        // target type is spelled out so clippy's missing-transmute-annotations
        // lint is satisfied.
        type AutoExtFn = unsafe extern "C" fn(
            *mut rusqlite::ffi::sqlite3,
            *mut *mut std::os::raw::c_char,
            *const rusqlite::ffi::sqlite3_api_routines,
        ) -> std::os::raw::c_int;
        unsafe {
            rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute::<*const (), AutoExtFn>(
                sqlite_vec::sqlite3_vec_init as *const (),
            )));
        }
    });
}

/// Map any rusqlite error into the framework-free `Error::Store` variant.
fn store_err<E: std::fmt::Display>(e: E) -> Error {
    Error::Store(e.to_string())
}

/// Read an `i64` tuning knob from the environment, falling back to `default` when
/// unset or unparseable. Used for the optional PRAGMA overrides.
fn env_i64(key: &str, default: i64) -> i64 {
    std::env::var(key)
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok())
        .unwrap_or(default)
}

/// Per-project code store backed by a single SQLite file at `<root>/.icode/index.db`.
///
/// The connection is an `Arc<Mutex<…>>` so the vector index ([`Vec0Index`]) can
/// share it: one connection, one writer, one serialization point for both the
/// code graph and its vectors.
/// The call-graph adjacency as returned by [`load_call_edges`]: bare `caller`
/// name → deduped list of bare `callee` names. Cached behind an [`Arc`] so graph
/// queries share ONE materialization instead of rebuilding it from the entire
/// `calls` table (full in-RAM adjacency + dedup `HashSet`) on every request.
type CachedAdjacency = HashMap<String, Vec<String>>;

pub struct SqliteCodeStore {
    conn: Arc<Mutex<Connection>>,
    /// Optional handle to the SHARED code-embedding cache in the central db
    /// (`~/.icode/icode.db`). When present, `embed_cache_lookup`/`embed_cache_store`
    /// route to it instead of this project's `index.db`, so an identical chunk is
    /// embedded ONCE per machine and reused across every project, git worktree, and
    /// re-clone. `None` (tests, or `open` without `with_embed_cache_db`) keeps the
    /// legacy per-project cache. Its own `Mutex` — never held with `conn` — so a
    /// cache hit never blocks a graph write.
    cache_conn: Option<Arc<Mutex<Connection>>>,
    /// Monotonic write-epoch of the call graph. Every mutation that can change the
    /// `calls` table (`upsert_file`, `delete_file`, `resolve_call_edges`) bumps
    /// this INSIDE its `conn`-lock critical section; the adjacency cache below is
    /// valid only while its stored epoch equals the current one. The graph is
    /// otherwise immutable between index runs, so a stable epoch means a stable
    /// adjacency — the invariant the cache rides on.
    graph_epoch: AtomicU64,
    /// Cached call-graph adjacency tagged with the epoch it was built at. Guarded
    /// by its OWN mutex — never held together with `conn` — so reading the cache
    /// never blocks a db write and vice-versa. `None` until first materialized.
    adjacency_cache: Mutex<Option<(u64, Arc<CachedAdjacency>)>>,
    /// Test-only tally of how many times the adjacency was rebuilt from SQL (a
    /// cache HIT does not increment it), so tests can prove the cache is used.
    #[cfg(test)]
    rebuild_count: AtomicU64,
}

impl SqliteCodeStore {
    /// Open (creating if needed) the index db under `<root>/.icode/index.db`.
    /// Creates the `.icode` directory, sets WAL + foreign_keys, applies schema.
    ///
    /// If an existing db carries an incompatible `user_version` (an older iCode
    /// schema, or a stale `.icode` left by a different tool that reused the path),
    /// it is deleted and rebuilt rather than crashing on a schema mismatch — the
    /// per-project index is regenerable from source.
    pub fn open(root: &Path) -> Result<Self> {
        // Register vec0 BEFORE opening any connection that will see `vec_code`.
        register_sqlite_vec();

        let dir = root.join(".icode");
        std::fs::create_dir_all(&dir).map_err(|e| Error::Io(e.to_string()))?;
        let db_path = dir.join("index.db");

        if db_path.exists() && !Self::db_is_current(&db_path)? {
            // Discard the incompatible db and its WAL/SHM sidecars.
            let _ = std::fs::remove_file(&db_path);
            let _ = std::fs::remove_file(dir.join("index.db-wal"));
            let _ = std::fs::remove_file(dir.join("index.db-shm"));
        }

        let conn = Connection::open(&db_path).map_err(store_err)?;
        Self::from_conn(conn)
    }

    /// Open an in-memory store (tests / ephemeral use). Always fresh.
    pub fn open_in_memory() -> Result<Self> {
        // Register vec0 BEFORE opening the connection (auto-extensions only apply
        // to connections opened after registration).
        register_sqlite_vec();
        let conn = Connection::open_in_memory().map_err(store_err)?;
        Self::from_conn(conn)
    }

    /// Attach the SHARED code-embedding cache in the central db at `central_db_path`
    /// (`~/` expanded against `$HOME`). Once attached, `embed_cache_lookup` /
    /// `embed_cache_store` route to it, so an identical chunk is embedded ONCE per
    /// machine and reused across every project, git worktree, and re-clone — the
    /// biggest lever against embedding cost (a worktree of an indexed repo re-embeds
    /// nothing). Best-effort folds this project's existing per-project cache into the
    /// shared one on first attach. Builder style.
    pub fn with_embed_cache_db(mut self, central_db_path: &str) -> Result<Self> {
        fn expand_tilde(path: &str) -> PathBuf {
            if let Some(rest) = path.strip_prefix("~/") {
                if let Ok(home) = std::env::var("HOME") {
                    return Path::new(&home).join(rest);
                }
            }
            PathBuf::from(path)
        }
        // Same shape as the per-project `embed_cache`, so the lookup/store SQL is
        // identical whichever connection backs it.
        const DDL: &str = "CREATE TABLE IF NOT EXISTS embed_cache (\
            content_hash TEXT NOT NULL, embed_model TEXT NOT NULL, \
            embed_dim INTEGER NOT NULL, vec BLOB NOT NULL, \
            PRIMARY KEY (content_hash, embed_model)) WITHOUT ROWID;";

        let path = expand_tilde(central_db_path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::Io(e.to_string()))?;
        }
        let conn = Connection::open(&path).map_err(store_err)?;
        conn.pragma_update(None, "journal_mode", "WAL").map_err(store_err)?;
        conn.busy_timeout(std::time::Duration::from_millis(5_000)).map_err(store_err)?;
        conn.execute_batch(DDL).map_err(store_err)?;
        let cache = Arc::new(Mutex::new(conn));
        // Fold this project's local cache into the shared one (once) so chunks
        // already embedded here are not re-embedded after the switch.
        self.migrate_local_cache_into(&cache)?;
        self.cache_conn = Some(cache);
        Ok(self)
    }

    /// One-time copy of this project's per-project `embed_cache` into the shared
    /// `cache` (INSERT OR IGNORE), gated by a `meta` flag so it runs at most once.
    fn migrate_local_cache_into(&self, cache: &Arc<Mutex<Connection>>) -> Result<()> {
        let done: i64 = {
            let local = self.conn.lock().map_err(store_err)?;
            local
                .query_row(
                    "SELECT COUNT(*) FROM meta WHERE key = 'embed_cache_migrated'",
                    [],
                    |r| r.get(0),
                )
                .map_err(store_err)?
        };
        if done > 0 {
            return Ok(());
        }
        let rows: Vec<(String, String, i64, Vec<u8>)> = {
            let local = self.conn.lock().map_err(store_err)?;
            let mut stmt = local
                .prepare("SELECT content_hash, embed_model, embed_dim, vec FROM embed_cache")
                .map_err(store_err)?;
            let it = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
                .map_err(store_err)?;
            let mut v = Vec::new();
            for row in it {
                v.push(row.map_err(store_err)?);
            }
            v
        };
        if !rows.is_empty() {
            let mut c = cache.lock().map_err(store_err)?;
            let tx = c.transaction().map_err(store_err)?;
            {
                let mut stmt = tx
                    .prepare(
                        "INSERT OR IGNORE INTO embed_cache \
                         (content_hash, embed_model, embed_dim, vec) VALUES (?1,?2,?3,?4)",
                    )
                    .map_err(store_err)?;
                for (h, m, d, v) in &rows {
                    stmt.execute(rusqlite::params![h, m, d, v]).map_err(store_err)?;
                }
            }
            tx.commit().map_err(store_err)?;
        }
        let local = self.conn.lock().map_err(store_err)?;
        local
            .execute(
                "INSERT OR REPLACE INTO meta (key, value) VALUES ('embed_cache_migrated', '1')",
                [],
            )
            .map_err(store_err)?;
        Ok(())
    }

    /// The connection backing the embed cache: the shared central db when attached,
    /// else this project's own `index.db` (legacy per-project cache).
    fn cache_connection(&self) -> &Arc<Mutex<Connection>> {
        self.cache_conn.as_ref().unwrap_or(&self.conn)
    }

    /// A vector index sharing this store's connection (single-writer). `dim` is
    /// fixed at [`schema::VEC_DIM`] — the `vec_code` column width.
    pub fn vector_index(&self) -> Vec0Index {
        Vec0Index::new(Arc::clone(&self.conn), schema::VEC_DIM)
    }

    /// "Code like this" — WITHOUT an embedding model.
    ///
    /// Ranks every symbol in the project against `qualified_name`'s MinHash signature
    /// (see [`crate::minhash`]): the mean of a lexical and a structural Jaccard
    /// estimate. The structural half is normalised over identifiers, so a copy-pasted
    /// clone whose variables were all renamed still scores high — which a dense vector
    /// captures poorly, since it encodes topic rather than shape.
    ///
    /// A full scan: signatures are 512 B and a repo has ~1k symbols, so this is a
    /// microsecond-scale pass and needs no ANN index. The symbol itself is excluded.
    /// Returns hits best-first; an unknown symbol (or one with no signature, e.g. a
    /// stale row from before this column existed) yields an empty list, not an error.
    pub fn find_similar_minhash(&self, qualified_name: &str, limit: usize) -> Result<Vec<CodeHit>> {
        if limit == 0 {
            return Ok(vec![]);
        }
        let conn = self.conn.lock().map_err(store_err)?;

        // The query symbol's signature (functions first, then classes).
        let target: Option<Vec<u8>> = match conn.query_row(
            "SELECT minhash FROM functions WHERE qualified_name = ?1 OR name = ?1 \
             UNION ALL \
             SELECT minhash FROM classes WHERE qualified_name = ?1 OR name = ?1 \
             LIMIT 1",
            rusqlite::params![qualified_name],
            |r| r.get::<_, Option<Vec<u8>>>(0),
        ) {
            Ok(v) => v,
            Err(rusqlite::Error::QueryReturnedNoRows) => None,
            Err(e) => return Err(store_err(e)),
        };
        let Some(target) = target else {
            return Ok(vec![]);
        };
        let target = crate::minhash::from_blob(&target);

        let mut scored: Vec<(f32, CodeHit)> = Vec::new();
        for (kind, table) in [
            (SymbolKind::Function, "functions"),
            (SymbolKind::Class, "classes"),
        ] {
            let sql = format!(
                "SELECT name, qualified_name, path, line_start, line_end, minhash \
                 FROM {table} WHERE minhash IS NOT NULL"
            );
            let mut stmt = conn.prepare(&sql).map_err(store_err)?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, i64>(3)?,
                        r.get::<_, i64>(4)?,
                        r.get::<_, Vec<u8>>(5)?,
                    ))
                })
                .map_err(store_err)?;
            for row in rows {
                let (name, qname, path, ls, le, blob) = row.map_err(store_err)?;
                // Never return the query symbol as its own neighbour.
                if qname == qualified_name || name == qualified_name {
                    continue;
                }
                let score = crate::minhash::similarity(&target, &crate::minhash::from_blob(&blob));
                if score <= 0.0 {
                    continue;
                }
                scored.push((
                    score,
                    CodeHit {
                        kind,
                        name,
                        qualified_name: qname,
                        path,
                        line_start: ls as u32,
                        line_end: le as u32,
                        score,
                        snippet: None,
                        stale: false,
                    },
                ));
            }
        }

        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(limit);
        Ok(scored.into_iter().map(|(_, h)| h).collect())
    }

    /// The stored embeddable `chunk_text` for a symbol (its FIRST chunk, by id), or
    /// `None` when the symbol has no chunk yet. `find_similar` embeds THIS exact text
    /// as its query, so the query vector is byte-identical to the symbol's indexed
    /// vector — no header-format re-derivation, and it inherits the checkout-invariant
    /// relative path the indexer wrote.
    pub fn chunk_text_for_symbol(&self, qualified_name: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().map_err(store_err)?;
        match conn.query_row(
            "SELECT chunk_text FROM code_chunks WHERE qualified_name = ?1 ORDER BY id LIMIT 1",
            rusqlite::params![qualified_name],
            |r| r.get::<_, String>(0),
        ) {
            Ok(t) => Ok(Some(t)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(store_err(e)),
        }
    }

    /// Stamp a chunk as embedded with `model`/`dim` after its vector has been
    /// written via [`Vec0Index`]. This is what drains the `pending_chunks` queue:
    /// the queue keys on a missing vector OR a NULL/stale `embed_model`, so the
    /// indexer must record the model it used. Inherent (not part of the frozen
    /// `CodeWriteStore` contract) — the embed loop owns this final stamp.
    pub fn mark_chunk_embedded(&self, rowid: i64, model: &str, dim: usize) -> Result<()> {
        let conn = self.conn.lock().map_err(store_err)?;
        conn.execute(
            "UPDATE code_chunks SET embed_model = ?1, embed_dim = ?2 WHERE id = ?3",
            rusqlite::params![model, dim as i64, rowid],
        )
        .map_err(store_err)?;
        Ok(())
    }

    /// Batch variant of [`mark_chunk_embedded`]: stamp a whole embed batch in ONE
    /// transaction (Wave 2). The old per-row stamp meant one autocommit — one WAL
    /// fsync — per chunk; wrapping the batch collapses that to a single commit, so
    /// the embed drain does O(batches) fsyncs instead of O(chunks).
    pub fn mark_chunks_embedded(&self, rowids: &[i64], model: &str, dim: usize) -> Result<()> {
        if rowids.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn.lock().map_err(store_err)?;
        let tx = conn.transaction().map_err(store_err)?;
        {
            let mut stmt = tx
                .prepare("UPDATE code_chunks SET embed_model = ?1, embed_dim = ?2 WHERE id = ?3")
                .map_err(store_err)?;
            for rowid in rowids {
                stmt.execute(rusqlite::params![model, dim as i64, rowid])
                    .map_err(store_err)?;
            }
        }
        tx.commit().map_err(store_err)?;
        Ok(())
    }

    /// Refresh ONLY the `files`-row metadata (`mtime`/`file_size`) for a path whose
    /// content is unchanged (Wave 2 incremental index). Used when a file's bytes
    /// hash identically to the index but its on-disk mtime/size drifted (a `touch`,
    /// a branch checkout that rewrote-then-restored it): a clean reindex would carry
    /// the fresh mtime, so we mirror that WITHOUT re-parsing, re-chunking, or nulling
    /// any vector — keeping `doctor`'s mtime/size drift signal consistent with a full
    /// reindex while leaving the embed queue untouched.
    pub fn touch_file_meta(&self, path: &str, mtime: i64, file_size: u64) -> Result<()> {
        let conn = self.conn.lock().map_err(store_err)?;
        conn.execute(
            "UPDATE files SET mtime = ?1, file_size = ?2 WHERE path = ?3",
            rusqlite::params![mtime, file_size as i64, path],
        )
        .map_err(store_err)?;
        Ok(())
    }

    /// Chunks whose embedding is missing/stale, WITH each chunk's `content_hash` so
    /// the embed pass can consult the content-addressed `embed_cache` before calling
    /// the embedder. Returns `(rowid, content_hash, chunk_text)` ordered by id.
    ///
    /// KEYSET CURSOR (Wave 2): `after_id` is a strictly-increasing high-water mark —
    /// the caller passes the max id of the previous batch, so each call resumes with
    /// `c.id > after_id` and rides the `code_chunks` primary-key index forward
    /// instead of re-scanning from row 0. The embed loop stamps every returned row
    /// (hit or miss) before the next call, so no still-pending row is ever left
    /// behind the cursor: the whole drain is O(N), not the old O(N²) full-scan.
    ///
    /// The pending predicate (no `vec_code` row / NULL / stale `embed_model`) is kept
    /// so a resume after a partial run still skips already-embedded rows. Inherent
    /// (the embed loop owns the concrete store), so the frozen contract is unchanged.
    pub fn pending_chunks_with_hash(
        &self,
        embed_model: &str,
        after_id: i64,
        limit: usize,
    ) -> Result<Vec<(i64, String, String)>> {
        let conn = self.conn.lock().map_err(store_err)?;
        let mut stmt = conn
            .prepare(
                "SELECT c.id, c.content_hash, c.chunk_text FROM code_chunks c \
                 WHERE c.id > ?1 \
                   AND (NOT EXISTS (SELECT 1 FROM vec_code v WHERE v.rowid = c.id) \
                        OR c.embed_model IS NULL \
                        OR c.embed_model != ?2) \
                 ORDER BY c.id LIMIT ?3",
            )
            .map_err(store_err)?;
        let rows = stmt
            .query_map(rusqlite::params![after_id, embed_model, limit as i64], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .map_err(store_err)?;
        collect(rows)
    }

    /// Look up cached vectors for `hashes` under `model` from the content-addressed
    /// `embed_cache`. Returns a `content_hash → vector` map holding only the hashes
    /// that hit; absent keys are cache misses for the caller to embed. Decodes the
    /// stored LE-f32 blob via [`vector::blob_to_f32`] (inverse of vec0's encoder).
    pub fn embed_cache_lookup(
        &self,
        hashes: &[&str],
        model: &str,
    ) -> Result<HashMap<String, Vec<f32>>> {
        if hashes.is_empty() {
            return Ok(HashMap::new());
        }
        let conn = self.cache_connection().lock().map_err(store_err)?;
        // ?1 = model; ?2.. = the hashes. The batch is ≤ the embed batch size, far
        // under SQLite's bound-variable limit, so one IN-list query is fine.
        let placeholders: String = (0..hashes.len())
            .map(|i| format!("?{}", i + 2))
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT content_hash, vec FROM embed_cache \
             WHERE embed_model = ?1 AND content_hash IN ({placeholders})"
        );
        let mut params: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(hashes.len() + 1);
        params.push(&model);
        for h in hashes {
            params.push(h);
        }
        let mut stmt = conn.prepare(&sql).map_err(store_err)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(params), |row| {
                let hash: String = row.get(0)?;
                let blob: Vec<u8> = row.get(1)?;
                Ok((hash, blob))
            })
            .map_err(store_err)?;
        let mut out = HashMap::new();
        for r in rows {
            let (hash, blob) = r.map_err(store_err)?;
            out.insert(hash, vector::blob_to_f32(&blob));
        }
        Ok(out)
    }

    /// Insert/replace `(content_hash, model) → vector` rows in the content-addressed
    /// `embed_cache` after a fresh embed, so a later re-index of identical text (a
    /// branch switch back, an edit-then-undo) rehydrates with no embedder call.
    /// Encodes with the SAME LE-f32 layout vec0 stores. One transaction.
    pub fn embed_cache_store(
        &self,
        model: &str,
        dim: usize,
        items: &[(String, Vec<f32>)],
    ) -> Result<()> {
        if items.is_empty() {
            return Ok(());
        }
        let mut conn = self.cache_connection().lock().map_err(store_err)?;
        let tx = conn.transaction().map_err(store_err)?;
        {
            let mut stmt = tx
                .prepare(
                    "INSERT OR REPLACE INTO embed_cache \
                     (content_hash, embed_model, embed_dim, vec) VALUES (?1,?2,?3,?4)",
                )
                .map_err(store_err)?;
            for (hash, v) in items {
                stmt.execute(rusqlite::params![hash, model, dim as i64, vector::f32_blob(v)])
                    .map_err(store_err)?;
            }
        }
        tx.commit().map_err(store_err)?;
        Ok(())
    }

    /// Lean listing of all classes (no `body`) ordered by `path, name`, capped at
    /// `limit`. Inherent (not part of the frozen `CodeReadStore` contract) — the
    /// web dashboard's "Map" tab builds an inheritance overview from `bases`, for
    /// which the heavy `body` is dead weight. Mirrors `get_class`'s column/JSON
    /// handling: `bases` is a JSON array (empty on bad data), `body` is left blank.
    pub fn list_classes(&self, limit: usize) -> Result<Vec<ClassDef>> {
        let conn = self.conn.lock().map_err(store_err)?;
        let mut stmt = conn
            .prepare(
                "SELECT name, qualified_name, path, language, line_start, line_end, \
                        bases, docstring \
                 FROM classes ORDER BY path, name LIMIT ?1",
            )
            .map_err(store_err)?;
        let rows = stmt
            .query_map(rusqlite::params![limit as i64], |row| {
                let lang_str: String = row.get(3)?;
                let bases_json: String = row.get(6)?;
                // `bases` is stored as a JSON array; fall back to empty on bad data.
                let bases: Vec<String> = serde_json::from_str(&bases_json).unwrap_or_default();
                Ok(ClassDef {
                    name: row.get(0)?,
                    qualified_name: row.get(1)?,
                    path: row.get(2)?,
                    language: parse_language(&lang_str),
                    line_start: row.get::<_, i64>(4)? as u32,
                    line_end: row.get::<_, i64>(5)? as u32,
                    bases,
                    docstring: row.get(7)?,
                    // Lean: no body (dead weight for the Map/inheritance view).
                    body: String::new(),
                })
            })
            .map_err(store_err)?;
        collect(rows)
    }

    /// Read `PRAGMA user_version` from an existing db file via a throwaway
    /// connection (dropped before the caller may delete the file).
    fn db_is_current(db_path: &Path) -> Result<bool> {
        let conn = Connection::open(db_path).map_err(store_err)?;
        let v: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .map_err(store_err)?;
        Ok(v == SCHEMA_VERSION)
    }

    fn from_conn(conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL").map_err(store_err)?;
        conn.pragma_update(None, "foreign_keys", "ON").map_err(store_err)?;
        // Multiple processes share one `.icode/index.db` (daemon writes the graph;
        // `serve`/`web` now write vectors on-demand). WAL allows one writer at a
        // time; without a busy timeout a concurrent writer fails fast with
        // SQLITE_BUSY. Wait briefly instead so cross-process writes serialise
        // cleanly rather than erroring.
        conn.busy_timeout(std::time::Duration::from_millis(5_000)).map_err(store_err)?;

        // ── Performance PRAGMAs (Wave 2) ──────────────────────────────────────
        // Under WAL the journal default is `synchronous=FULL`, which fsyncs on
        // EVERY commit — the bulk indexer / embed drain does thousands of small
        // commits, so that fsync dominates. `NORMAL` is the documented-safe WAL
        // setting: it fsyncs only at checkpoint, and a crash can lose at most the
        // last (uncheckpointed) transactions — never corrupt the db. The per-project
        // index is regenerable from source anyway, so this trade is free here.
        conn.pragma_update(None, "synchronous", "NORMAL").map_err(store_err)?;
        // Page cache (negative = KiB, not pages). 64 MiB keeps the graph + FTS hot
        // during a bulk index without an unbounded footprint. Env-overridable.
        let cache_kib = env_i64("ICODE_CACHE_KIB", 65_536);
        conn.pragma_update(None, "cache_size", -cache_kib).map_err(store_err)?;
        // Memory-map the db for the read-heavy graph queries (256 MiB default; 0
        // disables). mmap avoids a userspace copy on every page read.
        let mmap = env_i64("ICODE_MMAP_BYTES", 268_435_456);
        conn.pragma_update(None, "mmap_size", mmap).map_err(store_err)?;
        // Checkpoint the WAL roughly every 1000 pages (~4 MiB) so it can't grow
        // unbounded under the bulk-write indexer, while keeping commit-time fsync
        // off (synchronous=NORMAL only fsyncs at these checkpoints).
        conn.pragma_update(None, "wal_autocheckpoint", 1000).map_err(store_err)?;

        conn.execute_batch(schema::SCHEMA).map_err(store_err)?;
        // Stamp the schema generation so a future incompatible version is detected.
        conn.pragma_update(None, "user_version", SCHEMA_VERSION).map_err(store_err)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            cache_conn: None,
            graph_epoch: AtomicU64::new(0),
            adjacency_cache: Mutex::new(None),
            #[cfg(test)]
            rebuild_count: AtomicU64::new(0),
        })
    }

    /// Bump the call-graph write-epoch, invalidating the cached adjacency. MUST be
    /// called by every mutation that can change the `calls` table, WHILE the caller
    /// still holds the `conn` lock, so a concurrent reader that samples the epoch
    /// under the same lock always sees a consistent (epoch, calls) snapshot.
    fn bump_graph_epoch(&self) {
        self.graph_epoch.fetch_add(1, Ordering::Release);
    }

    /// The call-graph adjacency (bare caller → bare callees), served from an in-RAM
    /// cache keyed by the write-epoch. On a HIT (cached epoch == current) it clones
    /// a cheap [`Arc`]; on a MISS it rebuilds from the `calls` table exactly once
    /// and caches the result. Correctness: every graph mutation bumps the epoch, so
    /// the first graph query after an index run always rebuilds against the fresh
    /// graph — the cache can never serve a stale adjacency.
    ///
    /// Locking: the fast path takes ONLY the cache lock; the rebuild path takes the
    /// `conn` lock (and releases it) BEFORE taking the cache lock — the two locks
    /// are never held simultaneously, so no lock-order deadlock is possible. The
    /// cache lock is separate from `conn`, so serving a cached graph never blocks a
    /// db writer.
    fn call_edges(&self) -> Result<Arc<CachedAdjacency>> {
        // Fast path: cache hit under the current epoch.
        {
            let cache = self.adjacency_cache.lock().map_err(store_err)?;
            if let Some((cached_epoch, arc)) = cache.as_ref() {
                if *cached_epoch == self.graph_epoch.load(Ordering::Acquire) {
                    return Ok(Arc::clone(arc));
                }
            }
        }
        // Miss: rebuild. Sample the epoch AND the `calls` rows under the SAME conn
        // lock; writers bump the epoch inside their own conn-lock section, so the
        // (epoch, adjacency) pair read here is always an internally consistent
        // snapshot — never a new epoch paired with pre-write rows or vice-versa.
        let (epoch, built) = {
            let conn = self.conn.lock().map_err(store_err)?;
            let epoch = self.graph_epoch.load(Ordering::Acquire);
            (epoch, load_call_edges(&conn)?)
        };
        #[cfg(test)]
        self.rebuild_count.fetch_add(1, Ordering::Relaxed);
        let arc = Arc::new(built);
        // Install, unless another thread already cached an equal-or-newer epoch
        // (don't clobber a fresher snapshot with our older one).
        {
            let mut cache = self.adjacency_cache.lock().map_err(store_err)?;
            let should_install = match cache.as_ref() {
                Some((cached_epoch, _)) => *cached_epoch < epoch,
                None => true,
            };
            if should_install {
                *cache = Some((epoch, Arc::clone(&arc)));
            }
        }
        Ok(arc)
    }

    fn count_table(conn: &Connection, table: &str) -> Result<u64> {
        let sql = format!("SELECT COUNT(*) FROM {table}");
        conn.query_row(&sql, [], |r| r.get::<_, i64>(0))
            .map(|n| n as u64)
            .map_err(store_err)
    }

    /// Read-only: chunks with NO vector yet (`code_chunks` rows whose id has no
    /// matching `vec_code` rowid). This is the embed backlog — a normal, transient
    /// background state (it does NOT make the index unhealthy). Used by `doctor`.
    pub fn pending_embeddings_count(&self) -> Result<u64> {
        let conn = self.conn.lock().map_err(store_err)?;
        conn.query_row(
            "SELECT COUNT(*) FROM code_chunks c \
             WHERE NOT EXISTS (SELECT 1 FROM vec_code v WHERE v.rowid = c.id)",
            [],
            |r| r.get::<_, i64>(0),
        )
        .map(|n| n as u64)
        .map_err(store_err)
    }

    /// Read-only: orphan vectors — `vec_code` rowids with no backing `code_chunks`
    /// row. The `code_chunks_ad` delete-mirror trigger keeps this at 0; a non-zero
    /// value means the invariant broke (e.g. a manual delete that bypassed the
    /// trigger). Used by `doctor` as a hard health signal.
    pub fn orphan_vectors_count(&self) -> Result<u64> {
        let conn = self.conn.lock().map_err(store_err)?;
        conn.query_row(
            "SELECT COUNT(*) FROM vec_code v \
             WHERE NOT EXISTS (SELECT 1 FROM code_chunks c WHERE c.id = v.rowid)",
            [],
            |r| r.get::<_, i64>(0),
        )
        .map(|n| n as u64)
        .map_err(store_err)
    }
}

// ──────────────────────────── read side ────────────────────────────

impl CodeReadStore for SqliteCodeStore {
    fn stats(&self) -> Result<DbStats> {
        let conn = self.conn.lock().map_err(store_err)?;
        Ok(DbStats {
            files: Self::count_table(&conn, "files")?,
            functions: Self::count_table(&conn, "functions")?,
            classes: Self::count_table(&conn, "classes")?,
            calls: Self::count_table(&conn, "calls")?,
            imports: Self::count_table(&conn, "imports")?,
            routes: Self::count_table(&conn, "routes")?,
            code_chunks: Self::count_table(&conn, "code_chunks")?,
            // Doctor invariant: vec_rows should track embedded code_chunks.
            vec_rows: Self::count_table(&conn, "vec_code")?,
            parse_errors: Self::count_table(&conn, "parse_errors")?,
        })
    }

    fn search_code(&self, q: &CodeQuery) -> Result<Vec<CodeHit>> {
        // No embeddings yet (M2): Semantic has nothing to search.
        // Lexical and Hybrid both resolve to FTS5/BM25 here.
        if q.mode == SearchMode::Semantic || q.text.trim().is_empty() {
            return Ok(vec![]);
        }

        let conn = self.conn.lock().map_err(store_err)?;
        let match_expr = fts_match_expr(&q.text);
        // Exact name to boost (case-insensitive); used for the name-priority fix.
        let needle = q.text.trim().to_lowercase();

        let mut hits: Vec<CodeHit> = Vec::new();
        // `kind=None` searches BOTH symbol tables; an explicit kind narrows it.
        let want_fn = matches!(q.kind, None | Some(SymbolKind::Function));
        let want_cls = matches!(q.kind, None | Some(SymbolKind::Class));

        if want_fn {
            collect_symbol_hits(
                &conn,
                "fts_functions",
                "functions",
                SymbolKind::Function,
                &match_expr,
                &needle,
                q.limit,
                q.with_body,
                &mut hits,
            )?;
        }
        if want_cls {
            collect_symbol_hits(
                &conn,
                "fts_classes",
                "classes",
                SymbolKind::Class,
                &match_expr,
                &needle,
                q.limit,
                q.with_body,
                &mut hits,
            )?;
        }

        // Fuse: higher score first (name-match boost already folded in), then
        // truncate to the requested limit across both kinds.
        hits.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        hits.truncate(q.limit);
        Ok(hits)
    }

    fn get_function(&self, name: &str, lang: Option<Language>, with_body: bool) -> Result<Option<FunctionDef>> {
        let conn = self.conn.lock().map_err(store_err)?;
        let mut sql = String::from(
            "SELECT name, qualified_name, path, language, line_start, line_end, \
                    args, return_type, docstring, body, is_async \
             FROM functions WHERE name = ?1",
        );
        if lang.is_some() {
            sql.push_str(" AND language = ?2");
        }
        sql.push_str(" LIMIT 1");
        let mut stmt = conn.prepare(&sql).map_err(store_err)?;

        let map_row = |row: &rusqlite::Row<'_>| -> rusqlite::Result<FunctionDef> {
            let lang_str: String = row.get(3)?;
            let body: String = row.get(9)?;
            Ok(FunctionDef {
                name: row.get(0)?,
                qualified_name: row.get(1)?,
                path: row.get(2)?,
                language: parse_language(&lang_str),
                line_start: row.get::<_, i64>(4)? as u32,
                line_end: row.get::<_, i64>(5)? as u32,
                args: row.get(6)?,
                return_type: row.get(7)?,
                docstring: row.get(8)?,
                body: if with_body { body } else { String::new() },
                is_async: row.get::<_, i64>(10)? != 0,
                override_type: None,
                override_target: None,
            })
        };

        let res = if let Some(l) = lang {
            stmt.query_row(rusqlite::params![name, l.as_str()], map_row)
        } else {
            stmt.query_row(rusqlite::params![name], map_row)
        };
        match res {
            Ok(f) => Ok(Some(f)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(store_err(e)),
        }
    }

    // ── remaining read surface: M1+ stubs (declared up front by the contract) ──

    /// Architecture overview in one call: stats, per-language counts, per-module
    /// aggregates (directory prefix ≤3 path segments), the `top` complex functions,
    /// call hotspots (most-called names that are themselves indexed functions), and
    /// entry points (`main` + route handlers). Module/hotspot aggregation is
    /// path/name-based (APPROXIMATE for hotspots, M3b).
    fn repo_map(&self, top: usize) -> Result<RepoMap> {
        let stats = self.stats()?;
        let complex_functions = self.find_complex_functions(None, top)?;

        let conn = self.conn.lock().map_err(store_err)?;

        // languages: GROUP BY over files.
        let languages = {
            let mut stmt = conn
                .prepare("SELECT language, COUNT(*) FROM files GROUP BY language ORDER BY COUNT(*) DESC")
                .map_err(store_err)?;
            let rows = stmt
                .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as u64)))
                .map_err(store_err)?;
            collect(rows)?
        };

        // modules: aggregate files/functions/classes by directory prefix (≤3 segments).
        let modules = aggregate_modules(&conn, top)?;

        // call hotspots: most-frequent callees that are themselves indexed functions.
        let call_hotspots = {
            let mut stmt = conn
                .prepare(
                    "SELECT c.callee, COUNT(*) AS n FROM calls c \
                     WHERE c.callee IN (SELECT name FROM functions) \
                     GROUP BY c.callee ORDER BY n DESC, c.callee LIMIT ?1",
                )
                .map_err(store_err)?;
            let rows = stmt
                .query_map(rusqlite::params![top as i64], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as u64))
                })
                .map_err(store_err)?;
            collect(rows)?
        };

        // entry points: main + distinct route handler methods.
        let entry_points: Vec<String> = {
            let mut set = entry_point_names(&conn)?;
            let mut v: Vec<String> = set.drain().collect();
            v.sort();
            v
        };

        Ok(RepoMap {
            stats,
            languages,
            modules,
            complex_functions,
            call_hotspots,
            entry_points,
        })
    }
    fn find_similar(&self, _qualified_name: &str, _limit: usize) -> Result<Vec<CodeHit>> {
        Ok(vec![])
    }
    fn get_class(&self, name: &str, lang: Option<Language>, with_body: bool) -> Result<Option<ClassDef>> {
        let conn = self.conn.lock().map_err(store_err)?;
        let mut sql = String::from(
            "SELECT name, qualified_name, path, language, line_start, line_end, \
                    bases, docstring, body \
             FROM classes WHERE name = ?1",
        );
        if lang.is_some() {
            sql.push_str(" AND language = ?2");
        }
        sql.push_str(" LIMIT 1");
        let mut stmt = conn.prepare(&sql).map_err(store_err)?;

        let map_row = |row: &rusqlite::Row<'_>| -> rusqlite::Result<ClassDef> {
            let lang_str: String = row.get(3)?;
            let bases_json: String = row.get(6)?;
            let body: String = row.get(8)?;
            // `bases` is stored as a JSON array; fall back to empty on bad data.
            let bases: Vec<String> = serde_json::from_str(&bases_json).unwrap_or_default();
            Ok(ClassDef {
                name: row.get(0)?,
                qualified_name: row.get(1)?,
                path: row.get(2)?,
                language: parse_language(&lang_str),
                line_start: row.get::<_, i64>(4)? as u32,
                line_end: row.get::<_, i64>(5)? as u32,
                bases,
                docstring: row.get(7)?,
                body: if with_body { body } else { String::new() },
            })
        };

        let res = if let Some(l) = lang {
            stmt.query_row(rusqlite::params![name, l.as_str()], map_row)
        } else {
            stmt.query_row(rusqlite::params![name], map_row)
        };
        match res {
            Ok(c) => Ok(Some(c)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(store_err(e)),
        }
    }
    fn file_outline(&self, path: &str) -> Result<Vec<CodeHit>> {
        let conn = self.conn.lock().map_err(store_err)?;
        // Functions + classes of one file as lean hits (no body), ordered by line.
        let mut stmt = conn
            .prepare(
                "SELECT name, qualified_name, path, line_start, line_end, 'function' AS k \
                 FROM functions WHERE path = ?1 \
                 UNION ALL \
                 SELECT name, qualified_name, path, line_start, line_end, 'class' AS k \
                 FROM classes WHERE path = ?1 \
                 ORDER BY line_start ASC",
            )
            .map_err(store_err)?;
        let rows = stmt
            .query_map(rusqlite::params![path], |row| {
                let k: String = row.get(5)?;
                let kind = if k == "class" { SymbolKind::Class } else { SymbolKind::Function };
                Ok(CodeHit {
                    kind,
                    name: row.get(0)?,
                    qualified_name: row.get(1)?,
                    path: row.get(2)?,
                    line_start: row.get::<_, i64>(3)? as u32,
                    line_end: row.get::<_, i64>(4)? as u32,
                    score: 0.0,
                    snippet: None,
                    stale: false,
                })
            })
            .map_err(store_err)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(store_err)?);
        }
        Ok(out)
    }
    /// Everything an agent needs about a symbol in one call. Resolution is by
    /// NAME (`get_function` then `get_class`), so a `file_hint` narrows the search
    /// to one path. Callers/callees come from the by-name call graph (approximate
    /// — typed receiver resolution is M3b); `similar_symbols` stays empty until
    /// the vector index lands (M2).
    fn symbol_context(&self, name: &str, file_hint: Option<&str>) -> Result<SymbolContext> {
        // Resolve the definition (function first, then class). When a file hint is
        // given we prefer a definition in that file.
        let (definition, sym_path, qualified) = self.resolve_symbol(name, file_hint)?;

        let callers = self.get_callers(name, CALL_LIMIT)?;
        // Callees keyed by the symbol's qualified name (the parser stamps `caller`
        // as the qualified name) plus the bare name as a fallback.
        let callees = self.get_callees(qualified.as_deref().unwrap_or(name), CALL_LIMIT)?;

        let conn = self.conn.lock().map_err(store_err)?;
        // Imports of the file the symbol lives in (if we resolved a path).
        let imports = match sym_path.as_deref() {
            Some(p) => query_imports_for_path(&conn, p)?,
            None => Vec::new(),
        };
        // Routes whose handler_method is this symbol.
        let routes = query_routes_for_handler(&conn, name)?;
        drop(conn);

        let implementations = self.find_implementations(name)?;

        Ok(SymbolContext {
            definition,
            callers,
            callees,
            imports,
            routes,
            implementations,
            similar_symbols: Vec::new(),
        })
    }
    /// Direct callers of `name`: rows in `calls` targeting the symbol. RECEIVER-
    /// AWARE, with query shape deciding precision vs recall:
    ///  * a QUALIFIED `name` (`ServiceA::handle`) matches only edges whose receiver
    ///    resolved to exactly that target (`resolved_callee = name`) — so it no
    ///    longer collects every `->handle()` in the repo; homonyms in other classes
    ///    resolved to their own type are excluded, and an unattributable dynamic
    ///    `$x->handle()` (resolved_callee NULL) is NOT falsely credited to it.
    ///  * a BARE `name` (`handle`) matches every bare callee (resolved or not),
    ///    preserving the old approximate recall.
    fn get_callers(&self, name: &str, limit: usize) -> Result<Vec<Call>> {
        let conn = self.conn.lock().map_err(store_err)?;
        let qualified_query = name != last_name_segment(name);
        let sql = if qualified_query {
            // Precise: only edges receiver-resolved to this exact qualified target.
            "SELECT path, caller, callee, receiver, line, resolved_callee, confidence FROM calls \
             WHERE resolved_callee = ?1 OR callee = ?1 \
             ORDER BY path, line LIMIT ?2"
        } else {
            // Bare name: every edge with this bare callee (approximate recall).
            "SELECT path, caller, callee, receiver, line, resolved_callee, confidence FROM calls \
             WHERE callee = ?1 OR resolved_callee = ?1 \
             ORDER BY path, line LIMIT ?2"
        };
        let mut stmt = conn.prepare(sql).map_err(store_err)?;
        let rows = stmt
            .query_map(rusqlite::params![name, limit as i64], map_call)
            .map_err(store_err)?;
        collect(rows)
    }
    /// Direct callees of `name`: rows in `calls` whose `caller` equals the name or
    /// the qualified name of the resolved symbol. APPROXIMATE (name-based, M3b).
    fn get_callees(&self, name: &str, limit: usize) -> Result<Vec<Call>> {
        // The parser records `caller` as the enclosing symbol's qualified_name
        // (e.g. `Service::run`), so resolve the qualified form and match either.
        let qualified = {
            let conn = self.conn.lock().map_err(store_err)?;
            lookup_qualified_name(&conn, name)?
        };
        let conn = self.conn.lock().map_err(store_err)?;
        let q = qualified.unwrap_or_else(|| name.to_string());
        // Callees are keyed by the CALLER (already qualified), so they are
        // naturally class-scoped; each row now also carries `resolved_callee`
        // (the receiver-aware target) and `confidence` for the edge.
        let mut stmt = conn
            .prepare(
                "SELECT path, caller, callee, receiver, line, resolved_callee, confidence FROM calls \
                 WHERE caller = ?1 OR caller = ?2 \
                 ORDER BY path, line LIMIT ?3",
            )
            .map_err(store_err)?;
        let rows = stmt
            .query_map(rusqlite::params![name, q, limit as i64], map_call)
            .map_err(store_err)?;
        collect(rows)
    }
    /// Shortest call path from `from` to `to` as a sequence of symbol names
    /// (inclusive of both ends; empty if unreachable). BFS over the by-name call
    /// graph (`caller`→`callee`), bounded by `max_depth` edges. APPROXIMATE: edges
    /// are matched by name, so the path may traverse a homonym (M3b for typed).
    fn call_chain(&self, from: &str, to: &str, max_depth: usize) -> Result<Vec<String>> {
        if max_depth == 0 {
            return Ok(vec![]);
        }
        let from_bare = last_name_segment(from);
        let to_bare = last_name_segment(to);
        if from_bare == to_bare {
            return Ok(vec![from_bare]);
        }

        // Cached adjacency (rebuilt only when the graph's write-epoch changed).
        let edges = self.call_edges()?;

        // BFS by bare name; record predecessors to reconstruct the path.
        let mut visited: HashSet<String> = HashSet::new();
        let mut prev: HashMap<String, String> = HashMap::new();
        let mut queue: VecDeque<(String, usize)> = VecDeque::new();
        visited.insert(from_bare.clone());
        queue.push_back((from_bare.clone(), 0));

        while let Some((node, depth)) = queue.pop_front() {
            if depth >= max_depth {
                continue;
            }
            if let Some(neighbours) = edges.get(&node) {
                for next in neighbours {
                    if next == &to_bare {
                        // Reconstruct from→…→node→to.
                        let mut chain = vec![to_bare.clone(), node.clone()];
                        let mut cur = node.clone();
                        while let Some(p) = prev.get(&cur) {
                            chain.push(p.clone());
                            cur = p.clone();
                        }
                        chain.reverse();
                        return Ok(chain);
                    }
                    if visited.insert(next.clone()) {
                        prev.insert(next.clone(), node.clone());
                        queue.push_back((next.clone(), depth + 1));
                    }
                }
            }
        }
        Ok(vec![])
    }
    /// Modules this file depends on. APPROXIMATE: returns the raw `import.module`
    /// strings of the file. There is no reliable module→file resolver in M1, so
    /// transitive depth is best-effort — we resolve a module to another indexed
    /// file only when an `import.module` is a path-suffix of a known file, then
    /// follow that file's imports up to `depth` hops. Direct (depth 1) results are
    /// always exact; deeper hops depend on the suffix heuristic.
    fn find_dependencies(&self, path: &str, depth: usize) -> Result<Vec<String>> {
        let conn = self.conn.lock().map_err(store_err)?;
        let all_paths = load_all_paths(&conn)?;
        let depth = depth.max(1);

        let mut seen: HashSet<String> = HashSet::new();
        let mut out: Vec<String> = Vec::new();
        let mut frontier: Vec<String> = vec![path.to_string()];
        let mut visited_files: HashSet<String> = HashSet::new();
        visited_files.insert(path.to_string());

        for _ in 0..depth {
            let mut next_frontier: Vec<String> = Vec::new();
            for f in &frontier {
                let modules = query_imports_modules(&conn, f)?;
                for m in modules {
                    if seen.insert(m.clone()) {
                        out.push(m.clone());
                    }
                    // Best-effort module→file resolution by path suffix for the
                    // next transitive hop.
                    if let Some(resolved) = resolve_module_to_file(&m, &all_paths) {
                        if visited_files.insert(resolved.clone()) {
                            next_frontier.push(resolved);
                        }
                    }
                }
            }
            if next_frontier.is_empty() {
                break;
            }
            frontier = next_frontier;
        }
        out.sort();
        out.dedup();
        Ok(out)
    }
    /// Files that import `path` (reverse dependencies). APPROXIMATE: a file is an
    /// importer when one of its `import.module` strings is a path-suffix match of
    /// the target file (module names rarely carry the full on-disk path). Followed
    /// transitively up to `depth` hops over the same suffix heuristic.
    fn impact_analysis(&self, path: &str, depth: usize) -> Result<Vec<String>> {
        let conn = self.conn.lock().map_err(store_err)?;
        let depth = depth.max(1);

        let mut seen: HashSet<String> = HashSet::new();
        let mut out: Vec<String> = Vec::new();
        let mut frontier: Vec<String> = vec![path.to_string()];
        seen.insert(path.to_string());

        for _ in 0..depth {
            let mut next_frontier: Vec<String> = Vec::new();
            for target in &frontier {
                let importers = query_importers_of(&conn, target)?;
                for imp in importers {
                    if seen.insert(imp.clone()) {
                        out.push(imp.clone());
                        next_frontier.push(imp);
                    }
                }
            }
            if next_frontier.is_empty() {
                break;
            }
            frontier = next_frontier;
        }
        out.sort();
        out.dedup();
        Ok(out)
    }
    /// Classes that implement / extend `name`: their JSON `bases` array contains
    /// the name. Returns the `qualified_name` of each such class. APPROXIMATE: the
    /// match is a substring over the stored JSON (`"name"`), so a base recorded
    /// with a different namespace prefix won't match (full MRO resolution is M3b).
    fn find_implementations(&self, name: &str) -> Result<Vec<String>> {
        let conn = self.conn.lock().map_err(store_err)?;
        // `bases` is a JSON array of strings; a base equal to `name` serialises as
        // the substring `"name"`. LIKE that to find implementors.
        let pattern = format!("%\"{}\"%", name.replace('"', ""));
        let mut stmt = conn
            .prepare("SELECT qualified_name FROM classes WHERE bases LIKE ?1 ORDER BY qualified_name")
            .map_err(store_err)?;
        let rows = stmt
            .query_map(rusqlite::params![pattern], |row| row.get::<_, String>(0))
            .map_err(store_err)?;
        collect(rows)
    }
    /// Functions whose `name` never appears as a `callee` — likely uncalled.
    /// APPROXIMATE: this is the by-name in-degree only; entry points (`main` and
    /// any function whose name matches a route's `handler_method`) are excluded,
    /// but dynamic/reflective calls and external callers are invisible, so a hit
    /// is a *candidate* for dead code, not proof.
    fn find_dead_code(&self, lang: Option<Language>, limit: usize) -> Result<Vec<CodeHit>> {
        let conn = self.conn.lock().map_err(store_err)?;
        let entry = entry_point_names(&conn)?;
        let mut sql = String::from(
            "SELECT name, qualified_name, path, line_start, line_end FROM functions f \
             WHERE NOT EXISTS (SELECT 1 FROM calls c WHERE c.callee = f.name)",
        );
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        if let Some(l) = lang {
            sql.push_str(" AND f.language = ?");
            params.push(Box::new(l.as_str().to_string()));
        }
        sql.push_str(" ORDER BY f.path, f.line_start");
        let mut stmt = conn.prepare(&sql).map_err(store_err)?;
        let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|b| b.as_ref()).collect();
        let rows = stmt
            .query_map(param_refs.as_slice(), map_symbol_hit_fn)
            .map_err(store_err)?;
        let mut out = Vec::new();
        for r in rows {
            let hit = r.map_err(store_err)?;
            if entry.contains(&hit.name) {
                continue;
            }
            out.push(hit);
            if out.len() >= limit {
                break;
            }
        }
        Ok(out)
    }
    /// Functions UNREACHABLE from any entry point, via a recursive walk of the
    /// by-name call graph seeded at `main` and the route handlers. APPROXIMATE:
    /// reachability is computed over name-matched `caller`→`callee` edges (M3b for
    /// typed resolution), so a function reached only through a dynamic dispatch
    /// will be falsely reported. Distinct from `find_dead_code` in that it catches
    /// dead *clusters* (a→b→c where a is itself dead), not just zero-in-degree fns.
    fn find_unreachable(&self, lang: Option<Language>, limit: usize) -> Result<Vec<CodeHit>> {
        // Materialize the adjacency FIRST (it does its own locking internally),
        // THEN take the conn lock for the rest — never nest the two, since
        // `call_edges` may itself acquire `conn` and std `Mutex` is not reentrant.
        let edges = self.call_edges()?;
        let conn = self.conn.lock().map_err(store_err)?;
        let seeds = entry_point_names(&conn)?;

        // Mark everything reachable from the seeds (BFS over bare-name edges).
        let mut reachable: HashSet<String> = HashSet::new();
        let mut queue: VecDeque<String> = VecDeque::new();
        for s in &seeds {
            if reachable.insert(s.clone()) {
                queue.push_back(s.clone());
            }
        }
        while let Some(node) = queue.pop_front() {
            if let Some(neighbours) = edges.get(&node) {
                for next in neighbours {
                    if reachable.insert(next.clone()) {
                        queue.push_back(next.clone());
                    }
                }
            }
        }

        // Any function whose name is not reachable is reported.
        let mut sql = String::from(
            "SELECT name, qualified_name, path, line_start, line_end FROM functions f WHERE 1=1",
        );
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        if let Some(l) = lang {
            sql.push_str(" AND f.language = ?");
            params.push(Box::new(l.as_str().to_string()));
        }
        sql.push_str(" ORDER BY f.path, f.line_start");
        let mut stmt = conn.prepare(&sql).map_err(store_err)?;
        let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|b| b.as_ref()).collect();
        let rows = stmt
            .query_map(param_refs.as_slice(), map_symbol_hit_fn)
            .map_err(store_err)?;
        let mut out = Vec::new();
        for r in rows {
            let hit = r.map_err(store_err)?;
            if reachable.contains(&hit.name) {
                continue;
            }
            out.push(hit);
            if out.len() >= limit {
                break;
            }
        }
        Ok(out)
    }
    /// Functions ranked by a cheap complexity proxy:
    /// `score = span + fan_out*5 + callers*2`, where `span = line_end - line_start`,
    /// `fan_out = COUNT(DISTINCT callee)` for calls made by the function, and
    /// `callers = COUNT(*)` of calls targeting the function's name. APPROXIMATE
    /// (the call counts are name-based, M3b for typed).
    fn find_complex_functions(&self, lang: Option<Language>, limit: usize) -> Result<Vec<ComplexFunction>> {
        let conn = self.conn.lock().map_err(store_err)?;
        let mut sql = String::from(
            "SELECT f.qualified_name, f.name, f.path, f.line_start, f.line_end, \
                    (SELECT COUNT(DISTINCT c.callee) FROM calls c \
                       WHERE c.caller = f.qualified_name OR c.caller = f.name) AS fan_out, \
                    (SELECT COUNT(*) FROM calls c2 WHERE c2.callee = f.name) AS callers \
             FROM functions f WHERE 1=1",
        );
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        if let Some(l) = lang {
            sql.push_str(" AND f.language = ?");
            params.push(Box::new(l.as_str().to_string()));
        }
        let mut stmt = conn.prepare(&sql).map_err(store_err)?;
        let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|b| b.as_ref()).collect();
        let rows = stmt
            .query_map(param_refs.as_slice(), |row| {
                let line_start = row.get::<_, i64>(3)? as u32;
                let line_end = row.get::<_, i64>(4)? as u32;
                let fan_out = row.get::<_, i64>(5)? as u32;
                let callers = row.get::<_, i64>(6)? as u32;
                let span = line_end.saturating_sub(line_start);
                let score = span as f32 + fan_out as f32 * 5.0 + callers as f32 * 2.0;
                Ok(ComplexFunction {
                    qualified_name: row.get(0)?,
                    path: row.get(2)?,
                    line_start,
                    line_end,
                    span,
                    fan_out,
                    callers,
                    score,
                })
            })
            .map_err(store_err)?;
        let mut out: Vec<ComplexFunction> = Vec::new();
        for r in rows {
            out.push(r.map_err(store_err)?);
        }
        out.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        out.truncate(limit);
        Ok(out)
    }
    fn find_routes(
        &self,
        method: Option<&str>,
        path: Option<&str>,
        handler: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Route>> {
        let conn = self.conn.lock().map_err(store_err)?;
        // Optional filters: method (exact, upper-cased), path (LIKE substring),
        // handler (LIKE over class/method). Built dynamically with bound params.
        let mut sql = String::from(
            "SELECT path, method, route, handler_class, handler_method, name, line FROM routes WHERE 1=1",
        );
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        if let Some(m) = method {
            sql.push_str(" AND UPPER(method) = UPPER(?)");
            params.push(Box::new(m.to_string()));
        }
        if let Some(p) = path {
            sql.push_str(" AND route LIKE ?");
            params.push(Box::new(format!("%{p}%")));
        }
        if let Some(h) = handler {
            sql.push_str(" AND (handler_class LIKE ? OR handler_method LIKE ?)");
            params.push(Box::new(format!("%{h}%")));
            params.push(Box::new(format!("%{h}%")));
        }
        sql.push_str(" ORDER BY path, line LIMIT ?");
        params.push(Box::new(limit as i64));

        let mut stmt = conn.prepare(&sql).map_err(store_err)?;
        let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|b| b.as_ref()).collect();
        let rows = stmt
            .query_map(param_refs.as_slice(), |row| {
                Ok(Route {
                    path: row.get(0)?,
                    method: row.get(1)?,
                    route: row.get(2)?,
                    handler_class: row.get(3)?,
                    handler_method: row.get(4)?,
                    name: row.get(5)?,
                    line: row.get::<_, i64>(6)? as u32,
                })
            })
            .map_err(store_err)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(store_err)?);
        }
        Ok(out)
    }
    /// Regex search over the stored bodies of functions and classes. The pattern
    /// is compiled ONCE; an invalid pattern is an `Error::Invalid`. Each matching
    /// line yields a `GrepHit` whose `line` is the symbol's `line_start` plus the
    /// 0-based offset of the line within its body, and whose `text` is the matched
    /// line (trimmed of trailing newline). Searches stored bodies, not disk, so it
    /// only covers indexed symbols (use `read_file` for raw file text).
    fn grep_code(&self, pattern: &str, lang: Option<Language>, limit: usize) -> Result<Vec<GrepHit>> {
        let re = Regex::new(pattern).map_err(|e| Error::Invalid(format!("bad regex: {e}")))?;
        let conn = self.conn.lock().map_err(store_err)?;

        let mut out: Vec<GrepHit> = Vec::new();
        for table in ["functions", "classes"] {
            if out.len() >= limit {
                break;
            }
            let mut sql = format!("SELECT path, line_start, body FROM {table} WHERE 1=1");
            let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
            if let Some(l) = lang {
                sql.push_str(" AND language = ?");
                params.push(Box::new(l.as_str().to_string()));
            }
            sql.push_str(" ORDER BY path, line_start");
            let mut stmt = conn.prepare(&sql).map_err(store_err)?;
            let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|b| b.as_ref()).collect();
            let rows = stmt
                .query_map(param_refs.as_slice(), |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as u32, row.get::<_, String>(2)?))
                })
                .map_err(store_err)?;
            for r in rows {
                let (path, line_start, body) = r.map_err(store_err)?;
                for (offset, line) in body.lines().enumerate() {
                    if re.is_match(line) {
                        out.push(GrepHit {
                            path: path.clone(),
                            line: line_start + offset as u32,
                            text: line.to_string(),
                        });
                        if out.len() >= limit {
                            break;
                        }
                    }
                }
                if out.len() >= limit {
                    break;
                }
            }
        }
        Ok(out)
    }
    fn list_files(&self, pattern: Option<&str>, lang: Option<Language>, limit: usize) -> Result<Vec<FileRecord>> {
        let conn = self.conn.lock().map_err(store_err)?;
        let mut sql = String::from(
            "SELECT path, language, content_hash, ast_hash, lines_total, mtime, file_size \
             FROM files WHERE 1=1",
        );
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        if let Some(p) = pattern {
            sql.push_str(" AND path LIKE ?");
            params.push(Box::new(format!("%{p}%")));
        }
        if let Some(l) = lang {
            sql.push_str(" AND language = ?");
            params.push(Box::new(l.as_str().to_string()));
        }
        sql.push_str(" ORDER BY path LIMIT ?");
        params.push(Box::new(limit as i64));

        let mut stmt = conn.prepare(&sql).map_err(store_err)?;
        let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|b| b.as_ref()).collect();
        let rows = stmt
            .query_map(param_refs.as_slice(), map_file_record)
            .map_err(store_err)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(store_err)?);
        }
        Ok(out)
    }
    fn stat_file(&self, path: &str) -> Result<Option<FileRecord>> {
        let conn = self.conn.lock().map_err(store_err)?;
        let mut stmt = conn
            .prepare(
                "SELECT path, language, content_hash, ast_hash, lines_total, mtime, file_size \
                 FROM files WHERE path = ?1 LIMIT 1",
            )
            .map_err(store_err)?;
        match stmt.query_row(rusqlite::params![path], map_file_record) {
            Ok(f) => Ok(Some(f)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(store_err(e)),
        }
    }
    /// Read `path` from disk, returning lines `[start, end]` (1-based, inclusive;
    /// `None` = from the first / to the last line). Capped at [`READ_MAX_LINES`]
    /// lines and [`READ_MAX_BYTES`] bytes; when the requested range is truncated
    /// by either cap, a trailing `… [truncated]` marker line is appended. Reads
    /// the live file, not the index, so it reflects on-disk content.
    fn read_file(&self, path: &str, start: Option<u32>, end: Option<u32>) -> Result<String> {
        let content = std::fs::read_to_string(path).map_err(|e| Error::Io(e.to_string()))?;

        // 1-based inclusive window; default to the whole file.
        let start = start.unwrap_or(1).max(1);
        let end = end.unwrap_or(u32::MAX);
        if end < start {
            return Ok(String::new());
        }

        let mut out = String::new();
        let mut emitted = 0u32;
        let mut bytes = 0usize;
        let mut truncated = false;
        for (idx, line) in content.lines().enumerate() {
            let lineno = idx as u32 + 1;
            if lineno < start {
                continue;
            }
            if lineno > end {
                break;
            }
            // Apply the line / byte caps.
            if emitted >= READ_MAX_LINES || bytes + line.len() + 1 > READ_MAX_BYTES {
                truncated = true;
                break;
            }
            out.push_str(line);
            out.push('\n');
            emitted += 1;
            bytes += line.len() + 1;
        }
        if truncated {
            out.push_str("… [truncated]\n");
        }
        Ok(out)
    }
    /// Re-hydrate KNN rowids (== `code_chunks.id`) into lean `CodeHit`s. Returns
    /// one `(rowid, hit)` per matched chunk; missing rowids are silently dropped.
    /// `score` is a `0.0` placeholder — the caller folds in the KNN distance — and
    /// `snippet` is `None` (lean). `name` is the last segment of `qualified_name`.
    fn chunk_hits(&self, rowids: &[i64]) -> Result<Vec<(i64, CodeHit)>> {
        if rowids.is_empty() {
            return Ok(vec![]);
        }
        let conn = self.conn.lock().map_err(store_err)?;
        // Bind each rowid as a positional param to avoid string-built IN lists.
        let placeholders = std::iter::repeat_n("?", rowids.len()).collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT id, symbol_kind, qualified_name, path, line_start, line_end \
             FROM code_chunks WHERE id IN ({placeholders})"
        );
        let mut stmt = conn.prepare(&sql).map_err(store_err)?;
        let params: Vec<&dyn rusqlite::types::ToSql> =
            rowids.iter().map(|r| r as &dyn rusqlite::types::ToSql).collect();
        let rows = stmt
            .query_map(params.as_slice(), |row| {
                let id: i64 = row.get(0)?;
                let kind_str: Option<String> = row.get(1)?;
                let qualified_name: Option<String> = row.get(2)?;
                let path: Option<String> = row.get(3)?;
                let qn = qualified_name.unwrap_or_default();
                Ok((
                    id,
                    CodeHit {
                        kind: parse_symbol_kind(kind_str.as_deref()),
                        name: last_name_segment(&qn),
                        qualified_name: qn,
                        path: path.unwrap_or_default(),
                        line_start: row.get::<_, Option<i64>>(4)?.unwrap_or(0) as u32,
                        line_end: row.get::<_, Option<i64>>(5)?.unwrap_or(0) as u32,
                        score: 0.0,
                        snippet: None,
                        stale: false,
                    },
                ))
            })
            .map_err(store_err)?;
        collect(rows)
    }
}

// ──────────────────────────── write side ────────────────────────────

impl CodeWriteStore for SqliteCodeStore {
    fn upsert_file(
        &self,
        file: &FileRecord,
        functions: &[FunctionDef],
        classes: &[ClassDef],
        imports: &[Import],
        calls: &[Call],
        routes: &[Route],
    ) -> Result<()> {
        // M1: the whole code graph for one file is written in a single transaction.
        let mut conn = self.conn.lock().map_err(store_err)?;
        let tx = conn.transaction().map_err(store_err)?;

        // Replace the file row (ON DELETE CASCADE drops all its old graph rows).
        tx.execute("DELETE FROM files WHERE path = ?1", rusqlite::params![file.path])
            .map_err(store_err)?;
        tx.execute(
            "INSERT INTO files (path, language, content_hash, ast_hash, lines_total, mtime, file_size) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                file.path,
                file.language.as_str(),
                file.content_hash,
                file.ast_hash,
                file.lines_total as i64,
                file.mtime,
                file.file_size as i64,
            ],
        )
        .map_err(store_err)?;
        let file_id = tx.last_insert_rowid();

        {
            let mut stmt = tx
                .prepare(
                    "INSERT INTO functions \
                     (file_id, name, qualified_name, path, language, line_start, line_end, \
                      args, return_type, docstring, body, is_async, search_text, minhash) \
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
                )
                .map_err(store_err)?;
            for f in functions {
                // Derived identifier-split text (see `ident::search_text`): what makes
                // `Handler` match `HttpRequestHandler` without an embedding model.
                let search_text = crate::ident::search_text(&[
                    &f.name,
                    &f.qualified_name,
                    f.docstring.as_deref().unwrap_or(""),
                    &f.body,
                ]);
                stmt.execute(rusqlite::params![
                    file_id,
                    f.name,
                    f.qualified_name,
                    f.path,
                    f.language.as_str(),
                    f.line_start as i64,
                    f.line_end as i64,
                    f.args,
                    f.return_type,
                    f.docstring,
                    f.body,
                    f.is_async as i64,
                    search_text,
                    crate::minhash::to_blob(&crate::minhash::signature(&f.body)),
                ])
                .map_err(store_err)?;
            }
        }

        {
            let mut stmt = tx
                .prepare(
                    "INSERT INTO classes \
                     (file_id, name, qualified_name, path, language, line_start, line_end, \
                      bases, docstring, body, node_hash, search_text, minhash) \
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
                )
                .map_err(store_err)?;
            for c in classes {
                // `bases` is stored as a JSON array of strings (serde_json).
                let bases_json = serde_json::to_string(&c.bases).map_err(store_err)?;
                // Derived identifier-split text — see the note in the functions insert.
                let search_text = crate::ident::search_text(&[
                    &c.name,
                    &c.qualified_name,
                    c.docstring.as_deref().unwrap_or(""),
                    &c.body,
                ]);
                stmt.execute(rusqlite::params![
                    file_id,
                    c.name,
                    c.qualified_name,
                    c.path,
                    c.language.as_str(),
                    c.line_start as i64,
                    c.line_end as i64,
                    bases_json,
                    c.docstring,
                    c.body,
                    Option::<String>::None,
                    search_text,
                    crate::minhash::to_blob(&crate::minhash::signature(&c.body)),
                ])
                .map_err(store_err)?;
            }
        }

        {
            let mut stmt = tx
                .prepare(
                    "INSERT INTO imports (file_id, path, module, name, alias, line, kind) \
                     VALUES (?1,?2,?3,?4,?5,?6,?7)",
                )
                .map_err(store_err)?;
            for im in imports {
                stmt.execute(rusqlite::params![
                    file_id,
                    im.path,
                    im.module,
                    im.name,
                    im.alias,
                    im.line as i64,
                    im.kind,
                ])
                .map_err(store_err)?;
            }
        }

        {
            let mut stmt = tx
                .prepare(
                    "INSERT INTO calls \
                     (file_id, path, caller, callee, receiver, resolved_callee, confidence, line) \
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
                )
                .map_err(store_err)?;
            for c in calls {
                // Receiver-aware qualification guess (file-local, deterministic):
                // `EnclosingType::method` when the receiver names a class, else NULL.
                // Validated + confidence-graded later by `resolve_call_edges` (which
                // needs the *complete* functions table — unavailable mid-index).
                let guess =
                    resolve_callee_guess(&c.caller, &c.callee, c.receiver.as_deref(), file.language, classes);
                stmt.execute(rusqlite::params![
                    file_id,
                    c.path,
                    c.caller,
                    c.callee,
                    c.receiver,
                    guess,
                    CONF_UNRESOLVED as f64, // placeholder until resolve_call_edges runs
                    c.line as i64,
                ])
                .map_err(store_err)?;
            }
        }

        {
            let mut stmt = tx
                .prepare(
                    "INSERT INTO routes \
                     (file_id, path, method, route, handler_class, handler_method, name, line) \
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
                )
                .map_err(store_err)?;
            for r in routes {
                stmt.execute(rusqlite::params![
                    file_id,
                    r.path,
                    r.method,
                    r.route,
                    r.handler_class,
                    r.handler_method,
                    r.name,
                    r.line as i64,
                ])
                .map_err(store_err)?;
            }
        }

        tx.commit().map_err(store_err)?;
        // The file's `calls` (and functions) rows just changed → invalidate the
        // cached adjacency. Bump while still inside the conn-lock critical section.
        self.bump_graph_epoch();
        Ok(())
    }

    fn delete_file(&self, path: &str) -> Result<()> {
        let conn = self.conn.lock().map_err(store_err)?;
        conn.execute("DELETE FROM files WHERE path = ?1", rusqlite::params![path])
            .map_err(store_err)?;
        // ON DELETE CASCADE dropped the file's `calls` rows → invalidate the cache.
        self.bump_graph_epoch();
        Ok(())
    }

    /// Replace all chunks of one `path` (idempotent per file): delete the old
    /// rows (the `code_chunks_ad` trigger drops their vectors too), insert the new
    /// ones, and return the assigned rowids in INPUT ORDER so the caller can pair
    /// them with freshly-computed embeddings. Does NOT embed — vectors are written
    /// separately via the [`Vec0Index`].
    fn upsert_chunks(&self, path: &str, chunks: &[CodeChunk]) -> Result<Vec<i64>> {
        let mut conn = self.conn.lock().map_err(store_err)?;
        let tx = conn.transaction().map_err(store_err)?;

        // Drop the file's old chunks (trigger mirrors the delete into vec_code).
        tx.execute("DELETE FROM code_chunks WHERE path = ?1", rusqlite::params![path])
            .map_err(store_err)?;

        let mut rowids = Vec::with_capacity(chunks.len());
        {
            let mut stmt = tx
                .prepare(
                    "INSERT INTO code_chunks \
                     (symbol_kind, symbol_id, qualified_name, path, line_start, line_end, \
                      chunk_text, content_hash, embed_model, embed_dim) \
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,NULL,NULL)",
                )
                .map_err(store_err)?;
            for c in chunks {
                stmt.execute(rusqlite::params![
                    symbol_kind_str(c.symbol_kind),
                    c.symbol_id,
                    c.qualified_name,
                    c.path,
                    c.line_start as i64,
                    c.line_end as i64,
                    c.chunk_text,
                    c.content_hash,
                ])
                .map_err(store_err)?;
                rowids.push(tx.last_insert_rowid());
            }
        }

        tx.commit().map_err(store_err)?;
        Ok(rowids)
    }

    /// Chunks whose embedding is missing or stamped with a different model: no
    /// `vec_code` row yet, OR `embed_model` is NULL, OR it differs from the active
    /// model. Returns `(rowid, chunk_text)` (capped at `limit`) for the embed queue.
    fn pending_chunks(&self, embed_model: &str, limit: usize) -> Result<Vec<(i64, String)>> {
        let conn = self.conn.lock().map_err(store_err)?;
        let mut stmt = conn
            .prepare(
                "SELECT c.id, c.chunk_text FROM code_chunks c \
                 WHERE NOT EXISTS (SELECT 1 FROM vec_code v WHERE v.rowid = c.id) \
                    OR c.embed_model IS NULL \
                    OR c.embed_model != ?1 \
                 ORDER BY c.id LIMIT ?2",
            )
            .map_err(store_err)?;
        let rows = stmt
            .query_map(rusqlite::params![embed_model, limit as i64], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(store_err)?;
        collect(rows)
    }

    fn record_parse_error(&self, path: &str, error: &str) -> Result<()> {
        let conn = self.conn.lock().map_err(store_err)?;
        conn.execute(
            "INSERT INTO parse_errors (path, error) VALUES (?1, ?2)",
            rusqlite::params![path, error],
        )
        .map_err(store_err)?;
        Ok(())
    }
}

// ──────────────────────────── helpers ────────────────────────────

/// Per-call cap on callers/callees collected for `symbol_context`.
const CALL_LIMIT: usize = 200;
/// `read_file` truncation caps (≈5000 lines / 500 KB).
const READ_MAX_LINES: u32 = 5000;
const READ_MAX_BYTES: usize = 500 * 1024;

/// Collect a `query_map` iterator into a `Vec`, surfacing the first row error.
fn collect<T>(rows: impl Iterator<Item = rusqlite::Result<T>>) -> Result<Vec<T>> {
    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(store_err)?);
    }
    Ok(out)
}

/// Map a `calls` row (path, caller, callee, receiver, line, resolved_callee,
/// confidence) into a `Call`.
fn map_call(row: &rusqlite::Row<'_>) -> rusqlite::Result<Call> {
    Ok(Call {
        path: row.get(0)?,
        caller: row.get(1)?,
        callee: row.get(2)?,
        receiver: row.get(3)?,
        line: row.get::<_, i64>(4)? as u32,
        resolved_callee: row.get(5)?,
        confidence: row.get::<_, f64>(6)? as f32,
    })
}

/// Map a `functions`/`classes` row (name, qualified_name, path, line_start,
/// line_end) into a lean `CodeHit` of kind `Function`.
fn map_symbol_hit_fn(row: &rusqlite::Row<'_>) -> rusqlite::Result<CodeHit> {
    Ok(CodeHit {
        kind: SymbolKind::Function,
        name: row.get(0)?,
        qualified_name: row.get(1)?,
        path: row.get(2)?,
        line_start: row.get::<_, i64>(3)? as u32,
        line_end: row.get::<_, i64>(4)? as u32,
        score: 0.0,
        snippet: None,
        stale: false,
    })
}

/// The bare last segment of a possibly-qualified name (`Service::run` → `run`,
/// `Class.method` → `method`). Used to bridge qualified callers to bare callees.
fn last_name_segment(name: &str) -> String {
    // Handle `::` (Rust/PHP), `.` (Python/JS) and `\` (PHP namespaces) qualifiers.
    let after_colons = name.rsplit("::").next().unwrap_or(name);
    let after_dot = after_colons.rsplit('.').next().unwrap_or(after_colons);
    after_dot.rsplit('\\').next().unwrap_or(after_dot).to_string()
}

// ──────────────────────────── receiver-aware call resolution (Wave 1) ─────────
//
// Confidence scale, graded by HOW a call edge resolved (deterministic):
//   0.9  qualified target matched exactly ONE definition   (best)
//   0.7  qualified target matched several defs (homonym)    (still class-scoped)
//   0.6  bare-name matched exactly one definition in-project
//   0.4  bare-name collided with several definitions        (ambiguous)
//   0.3  unresolved / dynamic receiver / external target    (worst)
const CONF_QUALIFIED_UNIQUE: f32 = 0.9;
const CONF_QUALIFIED_MULTI: f32 = 0.7;
const CONF_BARE_UNIQUE: f32 = 0.6;
const CONF_BARE_COLLISION: f32 = 0.4;
const CONF_UNRESOLVED: f32 = 0.3;

/// The qualifier separator a language uses in its `qualified_name` shape
/// (`Type::method` for Rust/PHP, `Type.method` everywhere else). Building the
/// receiver guess with the SAME separator lets it match `functions.qualified_name`.
fn qualifier_sep(lang: Language) -> &'static str {
    match lang {
        Language::Rust | Language::Php => "::",
        _ => ".",
    }
}

/// The enclosing type of a caller's qualified name (`Service::run` → `Service`,
/// `A.B.method` → `A.B`). `None` for a free function (no type prefix).
fn enclosing_type<'a>(caller_qual: &'a str, sep: &str) -> Option<&'a str> {
    caller_qual.rsplit_once(sep).map(|(ty, _)| ty).filter(|t| !t.is_empty())
}

/// True when a receiver is a dynamic value / expression we cannot cheaply pin to a
/// class: a `$var`, a chain (`a.b`), a call (`f()`), a path (`std::mem`), etc. Only
/// a single bare identifier (`Model`, `User`, `T`) is treated as an explicit class.
fn is_dynamic_receiver(r: &str) -> bool {
    r.is_empty()
        || r.starts_with('$')
        || r.chars().any(|c| {
            matches!(
                c,
                '.' | ':' | '\\' | '(' | ')' | '[' | ']' | '{' | '}'
                    | ' ' | '\t' | '\n' | '"' | '\'' | '<' | '>' | '-' | '&' | '*'
            )
        })
}

/// Receiver-aware qualified-callee guess: the callee's fully-qualified name in the
/// language's `qualified_name` shape, when the receiver lets us name a class.
/// Returns `None` to leave the edge bare (dynamic/unqualifiable receiver, a free
/// call, or a `parent::` with no known base). Purely file-local and deterministic;
/// [`SqliteCodeStore::resolve_call_edges`] later validates the guess against real
/// definitions (nulling it on a miss) and grades `confidence`.
fn resolve_callee_guess(
    caller_qual: &str,
    callee: &str,
    receiver: Option<&str>,
    lang: Language,
    classes: &[ClassDef],
) -> Option<String> {
    let recv = receiver.map(str::trim).filter(|r| !r.is_empty())?;
    let sep = qualifier_sep(lang);
    match recv {
        // Self-referential receivers → the caller's own enclosing type.
        "self" | "$this" | "this" | "cls" | "static" => {
            enclosing_type(caller_qual, sep).map(|ty| format!("{ty}{sep}{callee}"))
        }
        // `parent::` → the enclosing class's first declared base (when known here).
        "parent" => {
            let ty = enclosing_type(caller_qual, sep)?;
            let leaf = last_name_segment(ty);
            classes
                .iter()
                .find(|c| c.name == leaf || c.qualified_name == ty)
                .and_then(|c| c.bases.first())
                .map(|base| format!("{}{sep}{callee}", last_name_segment(base)))
        }
        // An explicit single-identifier class name (`Model::find`, `T.method`).
        _ if !is_dynamic_receiver(recv) => Some(format!("{recv}{sep}{callee}")),
        // Dynamic / expression receiver → bare-name resolution.
        _ => None,
    }
}

/// Resolve a function (then class) by `name`, optionally preferring `file_hint`.
/// Returns the wrapped definition, the symbol's path, and its qualified name.
impl SqliteCodeStore {
    /// Finalize receiver-aware call resolution against the COMPLETE `functions`
    /// table: validate each `resolved_callee` guess (null it out on a miss so the
    /// edge falls back to bare-name) and grade `confidence` by the way it resolved.
    /// Must run AFTER the target definitions are indexed — the indexer calls it
    /// once at the end of a full run; the daemon passes `Some(path)` to re-grade
    /// just one file's outgoing edges (its own defs are already in place).
    ///
    /// Idempotent. Deterministic: the confidence CASE reads the pre-null
    /// `resolved_callee`, so grading a qualified guess that matched no definition
    /// still correctly demotes it to the bare-name band.
    pub fn resolve_call_edges(&self, path: Option<&str>) -> Result<()> {
        let conn = self.conn.lock().map_err(store_err)?;
        let filter = if path.is_some() { " AND calls.path = ?1" } else { "" };
        let args: Vec<&dyn rusqlite::types::ToSql> = match &path {
            Some(p) => vec![p],
            None => vec![],
        };
        // 1) Grade confidence by resolution method (uses the pre-null guess).
        let conf_sql = format!(
            "UPDATE calls SET confidence = CASE \
               WHEN resolved_callee IS NOT NULL \
                    AND (SELECT COUNT(*) FROM functions f WHERE f.qualified_name = calls.resolved_callee) = 1 THEN {q1} \
               WHEN resolved_callee IS NOT NULL \
                    AND (SELECT COUNT(*) FROM functions f WHERE f.qualified_name = calls.resolved_callee) > 1 THEN {q2} \
               WHEN (SELECT COUNT(*) FROM functions f WHERE f.name = calls.callee) = 1 THEN {b1} \
               WHEN (SELECT COUNT(*) FROM functions f WHERE f.name = calls.callee) > 1 THEN {b2} \
               ELSE {u} END \
             WHERE 1=1{filter}",
            q1 = CONF_QUALIFIED_UNIQUE,
            q2 = CONF_QUALIFIED_MULTI,
            b1 = CONF_BARE_UNIQUE,
            b2 = CONF_BARE_COLLISION,
            u = CONF_UNRESOLVED,
        );
        conn.execute(&conf_sql, args.as_slice()).map_err(store_err)?;
        // 2) Drop qualified guesses that matched no real definition → the edge
        //    reverts to bare-name (resolved_callee IS NULL) for the read queries.
        let null_sql = format!(
            "UPDATE calls SET resolved_callee = NULL \
             WHERE resolved_callee IS NOT NULL \
               AND NOT EXISTS (SELECT 1 FROM functions f WHERE f.qualified_name = calls.resolved_callee){filter}"
        );
        conn.execute(&null_sql, args.as_slice()).map_err(store_err)?;
        // `calls` was re-graded. This pass only touches `confidence`/`resolved_callee`
        // — not `caller`/`callee`, the only columns the adjacency reads — so the
        // adjacency is unchanged in practice, but we invalidate CONSERVATIVELY (an
        // extra rebuild is cheaper than risking a stale graph).
        self.bump_graph_epoch();
        Ok(())
    }

    fn resolve_symbol(
        &self,
        name: &str,
        file_hint: Option<&str>,
    ) -> Result<(Option<FunctionOrClass>, Option<String>, Option<String>)> {
        // Prefer a definition in the hinted file when one is given.
        if let Some(hint) = file_hint {
            if let Some(f) = self.get_function_in_file(name, hint)? {
                let (p, q) = (Some(f.path.clone()), Some(f.qualified_name.clone()));
                return Ok((Some(FunctionOrClass::Function(f)), p, q));
            }
            if let Some(c) = self.get_class_in_file(name, hint)? {
                let (p, q) = (Some(c.path.clone()), Some(c.qualified_name.clone()));
                return Ok((Some(FunctionOrClass::Class(c)), p, q));
            }
        }
        if let Some(f) = self.get_function(name, None, true)? {
            let (p, q) = (Some(f.path.clone()), Some(f.qualified_name.clone()));
            return Ok((Some(FunctionOrClass::Function(f)), p, q));
        }
        if let Some(c) = self.get_class(name, None, true)? {
            let (p, q) = (Some(c.path.clone()), Some(c.qualified_name.clone()));
            return Ok((Some(FunctionOrClass::Class(c)), p, q));
        }
        Ok((None, None, None))
    }

    fn get_function_in_file(&self, name: &str, path: &str) -> Result<Option<FunctionDef>> {
        let conn = self.conn.lock().map_err(store_err)?;
        let mut stmt = conn
            .prepare(
                "SELECT name, qualified_name, path, language, line_start, line_end, \
                        args, return_type, docstring, body, is_async \
                 FROM functions WHERE name = ?1 AND path = ?2 LIMIT 1",
            )
            .map_err(store_err)?;
        match stmt.query_row(rusqlite::params![name, path], |row| {
            let lang_str: String = row.get(3)?;
            Ok(FunctionDef {
                name: row.get(0)?,
                qualified_name: row.get(1)?,
                path: row.get(2)?,
                language: parse_language(&lang_str),
                line_start: row.get::<_, i64>(4)? as u32,
                line_end: row.get::<_, i64>(5)? as u32,
                args: row.get(6)?,
                return_type: row.get(7)?,
                docstring: row.get(8)?,
                body: row.get(9)?,
                is_async: row.get::<_, i64>(10)? != 0,
                override_type: None,
                override_target: None,
            })
        }) {
            Ok(f) => Ok(Some(f)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(store_err(e)),
        }
    }

    fn get_class_in_file(&self, name: &str, path: &str) -> Result<Option<ClassDef>> {
        let conn = self.conn.lock().map_err(store_err)?;
        let mut stmt = conn
            .prepare(
                "SELECT name, qualified_name, path, language, line_start, line_end, \
                        bases, docstring, body \
                 FROM classes WHERE name = ?1 AND path = ?2 LIMIT 1",
            )
            .map_err(store_err)?;
        match stmt.query_row(rusqlite::params![name, path], |row| {
            let lang_str: String = row.get(3)?;
            let bases_json: String = row.get(6)?;
            let bases: Vec<String> = serde_json::from_str(&bases_json).unwrap_or_default();
            Ok(ClassDef {
                name: row.get(0)?,
                qualified_name: row.get(1)?,
                path: row.get(2)?,
                language: parse_language(&lang_str),
                line_start: row.get::<_, i64>(4)? as u32,
                line_end: row.get::<_, i64>(5)? as u32,
                bases,
                docstring: row.get(7)?,
                body: row.get(8)?,
            })
        }) {
            Ok(c) => Ok(Some(c)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(store_err(e)),
        }
    }
}

/// Look up the `qualified_name` for a bare symbol name (function first, then
/// class). Returns `None` if the name is unknown.
fn lookup_qualified_name(conn: &Connection, name: &str) -> Result<Option<String>> {
    let res: rusqlite::Result<String> = conn.query_row(
        "SELECT qualified_name FROM functions WHERE name = ?1 \
         UNION ALL SELECT qualified_name FROM classes WHERE name = ?1 LIMIT 1",
        rusqlite::params![name],
        |row| row.get(0),
    );
    match res {
        Ok(q) => Ok(Some(q)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(store_err(e)),
    }
}

/// Imports of one file as `Import` rows.
fn query_imports_for_path(conn: &Connection, path: &str) -> Result<Vec<Import>> {
    let mut stmt = conn
        .prepare(
            "SELECT path, module, name, alias, line, kind FROM imports \
             WHERE path = ?1 ORDER BY line",
        )
        .map_err(store_err)?;
    let rows = stmt
        .query_map(rusqlite::params![path], |row| {
            Ok(Import {
                path: row.get(0)?,
                module: row.get(1)?,
                name: row.get(2)?,
                alias: row.get(3)?,
                line: row.get::<_, i64>(4)? as u32,
                kind: row.get(5)?,
            })
        })
        .map_err(store_err)?;
    collect(rows)
}

/// Distinct `import.module` strings of one file.
fn query_imports_modules(conn: &Connection, path: &str) -> Result<Vec<String>> {
    let mut stmt = conn
        .prepare("SELECT DISTINCT module FROM imports WHERE path = ?1 ORDER BY module")
        .map_err(store_err)?;
    let rows = stmt
        .query_map(rusqlite::params![path], |row| row.get::<_, String>(0))
        .map_err(store_err)?;
    collect(rows)
}

/// Files whose `import.module` is a path-suffix match of `target` (reverse deps).
fn query_importers_of(conn: &Connection, target: &str) -> Result<Vec<String>> {
    // Candidate module tokens that could name `target`: its full path and each
    // trailing path/segment suffix (with the extension stripped). We compare in
    // Rust to keep the suffix logic explicit.
    let mut stmt = conn
        .prepare("SELECT DISTINCT path, module FROM imports")
        .map_err(store_err)?;
    let rows = stmt
        .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))
        .map_err(store_err)?;
    let mut out = Vec::new();
    for r in rows {
        let (path, module) = r.map_err(store_err)?;
        if path == target {
            continue; // a file importing itself is not an external importer
        }
        if module_matches_file(&module, target) {
            out.push(path);
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

/// Routes whose `handler_method` equals `name`.
fn query_routes_for_handler(conn: &Connection, name: &str) -> Result<Vec<Route>> {
    let mut stmt = conn
        .prepare(
            "SELECT path, method, route, handler_class, handler_method, name, line \
             FROM routes WHERE handler_method = ?1 ORDER BY path, line",
        )
        .map_err(store_err)?;
    let rows = stmt
        .query_map(rusqlite::params![name], |row| {
            Ok(Route {
                path: row.get(0)?,
                method: row.get(1)?,
                route: row.get(2)?,
                handler_class: row.get(3)?,
                handler_method: row.get(4)?,
                name: row.get(5)?,
                line: row.get::<_, i64>(6)? as u32,
            })
        })
        .map_err(store_err)?;
    collect(rows)
}

/// All indexed file paths (for module→file suffix resolution).
fn load_all_paths(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT path FROM files").map_err(store_err)?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0)).map_err(store_err)?;
    collect(rows)
}

/// Best-effort module→file resolution: a module string resolves to a file when it
/// is a path-suffix of that file (after normalising `::`/`.` to `/` and dropping
/// the file extension). Returns the first match, if any.
fn resolve_module_to_file(module: &str, all_paths: &[String]) -> Option<String> {
    all_paths.iter().find(|p| module_matches_file(module, p)).cloned()
}

/// True when `module` plausibly names the file at `file_path` by path suffix.
fn module_matches_file(module: &str, file_path: &str) -> bool {
    // Normalise a module path (`a::b::c` / `a.b.c` / `a/b/c`) to `a/b/c`.
    let norm = module.replace("::", "/").replace('.', "/");
    let norm = norm.trim_matches('/');
    if norm.is_empty() {
        return false;
    }
    // Strip the file extension so `foo/bar` can match `…/foo/bar.rs`.
    let stem = match file_path.rsplit_once('.') {
        Some((s, _)) => s,
        None => file_path,
    };
    let stem = stem.replace('\\', "/");
    // Suffix match on segment boundary: stem ends with `/norm` or equals `norm`.
    stem == norm || stem.ends_with(&format!("/{norm}"))
}

/// Load the by-name call graph as bare-name adjacency (`caller_bare` → set of
/// `callee_bare`). Callers are stored qualified, callees bare; we reduce both to
/// their last segment so the BFS/reachability runs in one name space.
fn load_call_edges(conn: &Connection) -> Result<HashMap<String, Vec<String>>> {
    let mut stmt = conn.prepare("SELECT caller, callee FROM calls").map_err(store_err)?;
    let rows = stmt
        .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))
        .map_err(store_err)?;
    let mut edges: HashMap<String, Vec<String>> = HashMap::new();
    let mut seen: HashSet<(String, String)> = HashSet::new();
    for r in rows {
        let (caller, callee) = r.map_err(store_err)?;
        let from = last_name_segment(&caller);
        let to = last_name_segment(&callee);
        if from == to {
            continue; // drop self-edges
        }
        if seen.insert((from.clone(), to.clone())) {
            edges.entry(from).or_default().push(to);
        }
    }
    Ok(edges)
}

/// Entry-point function names: `main` plus every distinct route `handler_method`.
fn entry_point_names(conn: &Connection) -> Result<HashSet<String>> {
    let mut set: HashSet<String> = HashSet::new();
    // `main` is an entry point if it exists as a function.
    let has_main: i64 = conn
        .query_row("SELECT COUNT(*) FROM functions WHERE name = 'main'", [], |r| r.get(0))
        .map_err(store_err)?;
    if has_main > 0 {
        set.insert("main".to_string());
    }
    let mut stmt = conn
        .prepare("SELECT DISTINCT handler_method FROM routes WHERE handler_method IS NOT NULL")
        .map_err(store_err)?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(store_err)?;
    for r in rows {
        set.insert(r.map_err(store_err)?);
    }
    Ok(set)
}

/// Aggregate files/functions/classes by directory-prefix module (≤3 path
/// segments of the file's parent directory). Returns the busiest `top` modules.
fn aggregate_modules(conn: &Connection, top: usize) -> Result<Vec<ModuleStat>> {
    // Pull every file path plus its function/class counts, bucket by module key.
    let mut stmt = conn
        .prepare(
            "SELECT f.path, \
                    (SELECT COUNT(*) FROM functions fn WHERE fn.path = f.path), \
                    (SELECT COUNT(*) FROM classes cl WHERE cl.path = f.path) \
             FROM files f",
        )
        .map_err(store_err)?;
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as u64, row.get::<_, i64>(2)? as u64))
        })
        .map_err(store_err)?;

    let mut acc: HashMap<String, ModuleStat> = HashMap::new();
    for r in rows {
        let (path, fns, classes) = r.map_err(store_err)?;
        let key = module_key(&path);
        let entry = acc.entry(key.clone()).or_insert_with(|| ModuleStat {
            module: key,
            files: 0,
            functions: 0,
            classes: 0,
        });
        entry.files += 1;
        entry.functions += fns;
        entry.classes += classes;
    }
    let mut modules: Vec<ModuleStat> = acc.into_values().collect();
    // Busiest first (by functions+classes), then by name for stability.
    modules.sort_by(|a, b| {
        (b.functions + b.classes)
            .cmp(&(a.functions + a.classes))
            .then_with(|| a.module.cmp(&b.module))
    });
    modules.truncate(top);
    Ok(modules)
}

/// Directory-prefix module key for a file path: the parent directory truncated to
/// its first ≤3 path segments. Files in the root map to `"."`.
fn module_key(path: &str) -> String {
    let norm = path.replace('\\', "/");
    // Drop the filename to get the directory.
    let dir = match norm.rsplit_once('/') {
        Some((d, _)) => d,
        None => return ".".to_string(),
    };
    let segs: Vec<&str> = dir.split('/').filter(|s| !s.is_empty()).collect();
    if segs.is_empty() {
        return ".".to_string();
    }
    segs.into_iter().take(3).collect::<Vec<_>>().join("/")
}

/// Run one FTS5 search over a symbol table and push hits into `out`, applying the
/// name-priority boost. `bm25()` returns lower = better, so we negate it into the
/// `CodeHit` score (higher = more relevant). The fix from M0.5: a hit whose
/// `name` exactly equals the query gets a large bonus, and a prefix match a
/// smaller one — so a name match always outranks a body-only match.
#[allow(clippy::too_many_arguments)]
fn collect_symbol_hits(
    conn: &Connection,
    fts: &str,
    base: &str,
    kind: SymbolKind,
    match_expr: &str,
    needle: &str,
    limit: usize,
    with_body: bool,
    out: &mut Vec<CodeHit>,
) -> Result<()> {
    // Over-fetch a little so the post-boost re-rank has candidates to reorder.
    let fetch = (limit.max(1) * 4).min(200) as i64;
    let sql = format!(
        "SELECT s.name, s.qualified_name, s.path, s.line_start, s.line_end, s.body, \
                bm25({fts}) AS rank \
         FROM {fts} \
         JOIN {base} s ON s.id = {fts}.rowid \
         WHERE {fts} MATCH ?1 \
         ORDER BY rank ASC \
         LIMIT ?2"
    );
    let mut stmt = conn.prepare(&sql).map_err(store_err)?;
    let rows = stmt
        .query_map(rusqlite::params![match_expr, fetch], |row| {
            let name: String = row.get(0)?;
            let body: String = row.get(5)?;
            let rank: f64 = row.get(6)?;
            let lname = name.to_lowercase();
            // Base relevance from BM25 (negated: higher = better).
            let mut score = -rank as f32;
            // Name-priority boost: exact > prefix. Magnitudes are well above the
            // typical BM25 spread so a name hit always beats a body-only hit.
            if lname == needle {
                score += 1000.0;
            } else if lname.starts_with(needle) || needle.starts_with(&lname) {
                score += 500.0;
            }
            Ok(CodeHit {
                kind,
                name,
                qualified_name: row.get(1)?,
                path: row.get(2)?,
                line_start: row.get::<_, i64>(3)? as u32,
                line_end: row.get::<_, i64>(4)? as u32,
                score,
                snippet: if with_body { Some(body) } else { None },
                stale: false,
            })
        })
        .map_err(store_err)?;
    for hit in rows {
        out.push(hit.map_err(store_err)?);
    }
    Ok(())
}

/// Map a `files` row into a `FileRecord` (shared by `list_files`/`stat_file`).
fn map_file_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<FileRecord> {
    let lang_str: String = row.get(1)?;
    Ok(FileRecord {
        path: row.get(0)?,
        language: parse_language(&lang_str),
        content_hash: row.get(2)?,
        ast_hash: row.get(3)?,
        lines_total: row.get::<_, i64>(4)? as u32,
        mtime: row.get(5)?,
        file_size: row.get::<_, i64>(6)? as u64,
    })
}

/// Stable lowercase tag for a `SymbolKind` (stored in `code_chunks.symbol_kind`).
/// `icode-core` is frozen, so the mapping lives here next to its inverse.
fn symbol_kind_str(kind: SymbolKind) -> &'static str {
    match kind {
        SymbolKind::Function => "function",
        SymbolKind::Class => "class",
        SymbolKind::FileWindow => "filewindow",
    }
}

/// Inverse of [`symbol_kind_str`]. Unknown/NULL maps to `Function` (the common
/// case for symbol chunks); kept total so a re-hydrated hit never errors.
fn parse_symbol_kind(s: Option<&str>) -> SymbolKind {
    match s {
        Some("class") => SymbolKind::Class,
        Some("filewindow") => SymbolKind::FileWindow,
        _ => SymbolKind::Function,
    }
}

/// Reverse of `Language::as_str` for the few languages the skeleton stores.
/// Anything unknown maps to `Text` (the contract's catch-all).
fn parse_language(s: &str) -> Language {
    match s {
        "php" => Language::Php,
        "python" => Language::Python,
        "javascript" => Language::JavaScript,
        "typescript" => Language::TypeScript,
        "go" => Language::Go,
        "java" => Language::Java,
        "rust" => Language::Rust,
        "html" => Language::Html,
        _ => Language::Text,
    }
}

/// Build a safe FTS5 MATCH expression from free-form user text: split into
/// alphanumeric tokens and OR them as prefix terms, each quoted to neutralize
/// FTS5 operators. Empty input yields a no-match expression.
fn fts_match_expr(text: &str) -> String {
    let terms: Vec<String> = text
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|t| !t.is_empty())
        .map(|t| format!("\"{}\"*", t.replace('"', "")))
        .collect();
    if terms.is_empty() {
        // Matches nothing rather than erroring on an empty MATCH.
        "\"\"".to_string()
    } else {
        terms.join(" OR ")
    }
}

// ──────────────────────────── tests ────────────────────────────

#[cfg(test)]
mod adjacency_cache_tests {
    use super::*;
    use icode_core::traits::{CodeReadStore, CodeWriteStore};

    fn mk_file(path: &str) -> FileRecord {
        FileRecord {
            path: path.to_string(),
            language: Language::Rust,
            content_hash: format!("hash-{path}"),
            ast_hash: format!("ast-{path}"),
            lines_total: 10,
            mtime: 0,
            file_size: 100,
        }
    }

    fn mk_fn(path: &str, name: &str) -> FunctionDef {
        FunctionDef {
            name: name.to_string(),
            qualified_name: name.to_string(),
            path: path.to_string(),
            language: Language::Rust,
            line_start: 1,
            line_end: 2,
            args: String::new(),
            return_type: None,
            docstring: None,
            body: String::new(),
            is_async: false,
            override_type: None,
            override_target: None,
        }
    }

    fn mk_call(path: &str, caller: &str, callee: &str) -> Call {
        Call {
            path: path.to_string(),
            caller: caller.to_string(),
            callee: callee.to_string(),
            receiver: None,
            line: 1,
            resolved_callee: None,
            confidence: 0.0,
        }
    }

    /// Upsert one file with the given function names and `(caller, callee)` edges.
    fn upsert(store: &SqliteCodeStore, path: &str, fns: &[&str], edges: &[(&str, &str)]) {
        let file = mk_file(path);
        let functions: Vec<FunctionDef> = fns.iter().map(|n| mk_fn(path, n)).collect();
        let calls: Vec<Call> = edges.iter().map(|(a, b)| mk_call(path, a, b)).collect();
        store.upsert_file(&file, &functions, &[], &[], &calls, &[]).unwrap();
    }

    fn rebuilds(store: &SqliteCodeStore) -> u64 {
        store.rebuild_count.load(Ordering::Relaxed)
    }

    // (а) Two back-to-back graph queries with no mutation return the SAME
    // materialization, and the second one is a cache HIT (no extra rebuild).
    #[test]
    fn second_query_hits_cache_without_rebuild() {
        let store = SqliteCodeStore::open_in_memory().unwrap();
        upsert(&store, "a.rs", &["main", "foo", "bar"], &[("main", "foo"), ("foo", "bar")]);

        assert_eq!(rebuilds(&store), 0, "no query yet → nothing built");
        let first = store.call_edges().unwrap();
        assert_eq!(rebuilds(&store), 1, "first query materializes the adjacency once");
        let second = store.call_edges().unwrap();
        assert_eq!(rebuilds(&store), 1, "second query is served from cache");

        // Same backing allocation and identical content.
        assert!(Arc::ptr_eq(&first, &second), "cache returns the same Arc");
        assert_eq!(*first, *second);
        // Sanity: the edge really is there.
        assert!(first.get("main").is_some_and(|v| v.contains(&"foo".to_string())));
    }

    // (б) After upsert_file AND delete_file the next query sees the change
    // (write-epoch invalidation forces a rebuild against the fresh graph).
    #[test]
    fn upsert_and_delete_invalidate_cache() {
        let store = SqliteCodeStore::open_in_memory().unwrap();
        upsert(&store, "a.rs", &["main", "foo", "bar"], &[("main", "foo"), ("foo", "bar")]);

        let _ = store.call_edges().unwrap();
        assert_eq!(rebuilds(&store), 1);
        let _ = store.call_edges().unwrap();
        assert_eq!(rebuilds(&store), 1, "still cached");

        // upsert a new file adding edge bar→baz.
        upsert(&store, "b.rs", &["baz"], &[("bar", "baz")]);
        let edges = store.call_edges().unwrap();
        assert_eq!(rebuilds(&store), 2, "upsert_file bumped the epoch → rebuild");
        assert!(
            edges.get("bar").is_some_and(|v| v.contains(&"baz".to_string())),
            "new edge visible after upsert"
        );

        // delete_file must invalidate too (CASCADE drops b.rs's calls).
        store.delete_file("b.rs").unwrap();
        let edges2 = store.call_edges().unwrap();
        assert_eq!(rebuilds(&store), 3, "delete_file bumped the epoch → rebuild");
        assert!(
            !edges2.get("bar").is_some_and(|v| v.contains(&"baz".to_string())),
            "deleted edge gone after delete"
        );
    }

    // resolve_call_edges is a graph mutator too — prove it bumps the epoch and
    // invalidates the cache (conservative: it only re-grades confidence).
    #[test]
    fn resolve_call_edges_invalidates_cache() {
        let store = SqliteCodeStore::open_in_memory().unwrap();
        upsert(&store, "a.rs", &["main", "foo"], &[("main", "foo")]);

        let epoch_before = store.graph_epoch.load(Ordering::Acquire);
        let _ = store.call_edges().unwrap();
        let builds_before = rebuilds(&store);

        store.resolve_call_edges(None).unwrap();
        assert!(
            store.graph_epoch.load(Ordering::Acquire) > epoch_before,
            "resolve_call_edges bumps the write-epoch"
        );
        let _ = store.call_edges().unwrap();
        assert_eq!(rebuilds(&store), builds_before + 1, "resolve invalidated the cache");
    }

    // (в) No regression: call_chain / find_unreachable produce the expected
    // results over the cached adjacency, and repeated calls are stable.
    #[test]
    fn call_chain_and_find_unreachable_regression() {
        let store = SqliteCodeStore::open_in_memory().unwrap();
        // main→foo→bar reachable; `orphan` is defined but never called.
        upsert(
            &store,
            "a.rs",
            &["main", "foo", "bar", "orphan"],
            &[("main", "foo"), ("foo", "bar")],
        );

        let chain = store.call_chain("main", "bar", 5).unwrap();
        assert_eq!(
            chain,
            vec!["main".to_string(), "foo".to_string(), "bar".to_string()],
        );
        // Second call rides the cache; identical path.
        assert_eq!(store.call_chain("main", "bar", 5).unwrap(), chain);

        let dead = store.find_unreachable(None, 100).unwrap();
        let names: Vec<&str> = dead.iter().map(|h| h.name.as_str()).collect();
        assert!(names.contains(&"orphan"), "orphan unreachable, got {names:?}");
        assert!(!names.contains(&"main"));
        assert!(!names.contains(&"foo"));
        assert!(!names.contains(&"bar"));
    }
}
