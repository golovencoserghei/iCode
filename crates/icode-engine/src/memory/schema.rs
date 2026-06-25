//! SQLite schema for the CENTRAL memory db (`~/.icode/icode.db`).
//!
//! Distinct from the per-project code-graph schema (`store::schema`): this db is
//! the durable, cross-project store of memories, their vectors, the project
//! registry, and (later) the developer profile + knowledge graph. It is NOT
//! disposable — a schema mismatch must NOT silently wipe it (unlike a per-project
//! index, which is regenerable from source), so the open path guards on
//! `PRAGMA user_version` and errors rather than deleting on a downgrade.
//!
//! Memory ids are TEXT (`mem_<ulid>__<project>`), but vec0 keys on an INTEGER
//! rowid. The `mem_rowid` bridge maps the two 1:1 (an AUTOINCREMENT int rowid ↔
//! the TEXT mem id). The FTS index is STANDALONE (not external-content) for the
//! same reason — fts5 external-content assumes an integer `content_rowid` over the
//! owning table, which `memories(id TEXT)` is not — so rows are inserted/deleted
//! into `fts_memory` BY HAND keyed on the bridge rowid.

/// Embedding dimensionality baked into the `vec_memory` vec0 column. Matches the
/// qwen3-embedding:0.6b default (the M2 `Embedder`); FIXED at table-creation time
/// (vec0 has no dim-templating). A different embedder dim requires a rebuild.
pub const VEC_DIM: usize = 1024;

/// Central-db schema generation. Bumped on an incompatible on-disk layout change.
/// Unlike the per-project index, a mismatch is a hard error (the central db holds
/// the only copy of memories — it is never silently discarded).
pub const SCHEMA_VERSION: i64 = 1;

/// DDL applied once at open (idempotent, `IF NOT EXISTS`).
pub const SCHEMA: &str = r#"
-- ──────────────────────────── memories ────────────────────────────

CREATE TABLE IF NOT EXISTS memories (
    id               TEXT PRIMARY KEY,
    project          TEXT NOT NULL,
    content          TEXT NOT NULL,
    category         TEXT NOT NULL,
    tags             TEXT NOT NULL DEFAULT '[]',   -- JSON array of strings
    importance       REAL NOT NULL DEFAULT 0,
    status           TEXT NOT NULL DEFAULT 'active',
    resolved_at      TEXT,
    resolve_reason   TEXT,
    session_id       TEXT,
    access_count     INTEGER NOT NULL DEFAULT 0,
    created_at       TEXT NOT NULL,
    last_accessed_at TEXT NOT NULL,
    updated_at       TEXT,
    embed_model      TEXT,
    embed_dim        INTEGER,
    content_hash     TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_memories_project ON memories(project);
CREATE INDEX IF NOT EXISTS idx_memories_status ON memories(status);

-- ──────────────────────────── int-rowid bridge ────────────────────────────

-- vec0 / fts5 key on an INTEGER rowid; memories.id is TEXT. This bridge maps the
-- two 1:1. `rowid` (the AUTOINCREMENT pk) is what vec_memory.rowid and the
-- fts_memory rowid reference; `mem_id` is the owning TEXT memory id.
CREATE TABLE IF NOT EXISTS mem_rowid (
    rowid  INTEGER PRIMARY KEY AUTOINCREMENT,
    mem_id TEXT NOT NULL UNIQUE
);

-- ──────────────────────────── vectors ────────────────────────────

-- vec0 virtual table: f32 little-endian blobs, cosine distance. rowid ==
-- mem_rowid.rowid. Requires sqlite-vec registered on the connection BEFORE open
-- (see store::register_sqlite_vec, reused by the central db).
CREATE VIRTUAL TABLE IF NOT EXISTS vec_memory USING vec0(embedding float[1024] distance_metric=cosine);

-- ──────────────────────────── full-text ────────────────────────────

-- STANDALONE fts5 (not external-content): the owning table's id is TEXT, so there
-- is no integer content_rowid to mirror. Rows are inserted/deleted by hand using
-- the mem_rowid.rowid as the fts rowid, keeping it in lock-step with vec_memory.
CREATE VIRTUAL TABLE IF NOT EXISTS fts_memory USING fts5(content);

-- ──────────────────────────── project registry ────────────────────────────

CREATE TABLE IF NOT EXISTS projects (
    name            TEXT PRIMARY KEY,
    root_path       TEXT,
    created_at      TEXT,
    onboarded_at    TEXT,
    last_session_at TEXT
);

-- ──────────────────────────── store metadata ────────────────────────────

CREATE TABLE IF NOT EXISTS schema_meta (
    key   TEXT PRIMARY KEY,
    value TEXT
);
"#;
