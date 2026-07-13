//! SQLite schema for the per-project code graph (M1: full node set —
//! files + functions + classes + imports + calls + routes, with FTS5 over
//! functions and classes). M2 adds the semantic layer: `meta`, `code_chunks`,
//! and the sqlite-vec `vec_code` virtual table.

/// Embedding dimensionality baked into the `vec_code` vec0 column.
///
/// 1024 is the default for the qwen3-embedding:0.6b model (the M2 default
/// `Embedder`). The vec0 column width is FIXED at table-creation time — vec0 has
/// no dim-templating, so a different embedder dim requires a schema rebuild. This
/// is hard-coded for now; templating `[VEC_DIM]` from `core::config` is a future
/// refinement (the `meta` row `embed_dim`/`embed_model` records what was actually
/// used so `doctor` can detect a mismatch).
pub const VEC_DIM: usize = 1024;

/// DDL applied once at store open. Idempotent (`IF NOT EXISTS`). The FTS5 indexes
/// are contentless-external over `functions`/`classes` (rowid == table id), so a
/// search `MATCH` yields the owning rows without duplicating body text. All child
/// tables cascade-delete from `files`, so re-indexing a file (DELETE + INSERT)
/// transparently clears its old graph rows.
///
/// vec0 has NO foreign-key cascade: deleting a `code_chunks` row does not drop its
/// vector. The `code_chunks_ad` trigger mirrors deletes into `vec_code` manually
/// so `count(vec_code) == count(code_chunks-with-embedding)` stays an invariant.
pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS files (
    id           INTEGER PRIMARY KEY,
    path         TEXT NOT NULL UNIQUE,
    language     TEXT NOT NULL,
    content_hash TEXT NOT NULL,
    ast_hash     TEXT NOT NULL,
    lines_total  INTEGER NOT NULL,
    mtime        INTEGER NOT NULL,
    file_size    INTEGER NOT NULL
);

-- ──────────────────────────── functions ────────────────────────────

CREATE TABLE IF NOT EXISTS functions (
    id             INTEGER PRIMARY KEY,
    file_id        INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    name           TEXT NOT NULL,
    qualified_name TEXT NOT NULL,
    path           TEXT NOT NULL,
    language       TEXT NOT NULL,
    line_start     INTEGER NOT NULL,
    line_end       INTEGER NOT NULL,
    args           TEXT NOT NULL,
    return_type    TEXT,
    docstring      TEXT,
    body           TEXT NOT NULL,
    is_async       INTEGER NOT NULL DEFAULT 0,
    -- Derived (see `ident::search_text`): every identifier in the symbol, kept
    -- verbatim AND exploded into its words, so FTS5's word tokenizer can match
    -- `Handler` against `HttpRequestHandler`. Indexed as an extra FTS column; the
    -- raw `body` stays indexed too, so nothing is lost.
    search_text    TEXT NOT NULL DEFAULT '',
    -- MinHash signature of the body (see `minhash`): 512 B, lexical + structural
    -- views. Powers "find similar code" with NO embedding model — a renamed clone
    -- still matches on the structural half.
    minhash        BLOB
);

CREATE INDEX IF NOT EXISTS idx_functions_file ON functions(file_id);
CREATE INDEX IF NOT EXISTS idx_functions_name ON functions(name);
CREATE INDEX IF NOT EXISTS idx_functions_qname ON functions(qualified_name);

-- ──────────────────────────── classes ────────────────────────────

CREATE TABLE IF NOT EXISTS classes (
    id             INTEGER PRIMARY KEY,
    file_id        INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    name           TEXT NOT NULL,
    qualified_name TEXT NOT NULL,
    path           TEXT NOT NULL,
    language       TEXT NOT NULL,
    line_start     INTEGER NOT NULL,
    line_end       INTEGER NOT NULL,
    bases          TEXT NOT NULL DEFAULT '[]',   -- JSON array of base/super names
    docstring      TEXT,
    body           TEXT NOT NULL,
    node_hash      TEXT,
    -- Derived identifier-split text — see the note on `functions.search_text`.
    search_text    TEXT NOT NULL DEFAULT '',
    -- MinHash signature — see the note on `functions.minhash`.
    minhash        BLOB
);

CREATE INDEX IF NOT EXISTS idx_classes_file ON classes(file_id);
CREATE INDEX IF NOT EXISTS idx_classes_name ON classes(name);
CREATE INDEX IF NOT EXISTS idx_classes_qname ON classes(qualified_name);

-- ──────────────────────────── imports ────────────────────────────

CREATE TABLE IF NOT EXISTS imports (
    id      INTEGER PRIMARY KEY,
    file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    path    TEXT NOT NULL,
    module  TEXT NOT NULL,
    name    TEXT,
    alias   TEXT,
    line    INTEGER NOT NULL,
    kind    TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_imports_file ON imports(file_id);
CREATE INDEX IF NOT EXISTS idx_imports_module ON imports(module);

-- ──────────────────────────── calls ────────────────────────────

-- `receiver` is the raw call receiver captured by the parser ($this/self/static/
-- parent/ClassName/$var). `resolved_callee` is the receiver-aware qualified target
-- (`EnclosingType::method`) once it validated against a real definition (NULL =
-- edge stays bare-name). `confidence` grades HOW the edge resolved (0.3..0.9).
-- Both are filled by the resolve pass (store::resolve_call_edges), not the parser.
CREATE TABLE IF NOT EXISTS calls (
    id              INTEGER PRIMARY KEY,
    file_id         INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    path            TEXT NOT NULL,
    caller          TEXT NOT NULL,
    callee          TEXT NOT NULL,
    receiver        TEXT,
    -- 1 when the call was written with METHOD syntax (`a.b()`, `$a->b()`). A method
    -- call can never target a FREE function, so `get_callers` uses this to stop every
    -- `.collect()` in the repo from being credited to a local `fn collect`. Only set
    -- where the grammar is unambiguous (Rust `.` vs `::`, PHP `->` vs `::`); in
    -- Python/Go/JS a dot also means module access, so it stays 0 there.
    is_method       INTEGER NOT NULL DEFAULT 0,
    resolved_callee TEXT,
    confidence      REAL NOT NULL DEFAULT 0.3,
    line            INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_calls_file ON calls(file_id);
CREATE INDEX IF NOT EXISTS idx_calls_callee ON calls(callee);
CREATE INDEX IF NOT EXISTS idx_calls_caller ON calls(caller);
CREATE INDEX IF NOT EXISTS idx_calls_resolved ON calls(resolved_callee);

-- ──────────────────────────── routes ────────────────────────────

CREATE TABLE IF NOT EXISTS routes (
    id             INTEGER PRIMARY KEY,
    file_id        INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    path           TEXT NOT NULL,
    method         TEXT NOT NULL,
    route          TEXT NOT NULL,
    handler_class  TEXT,
    handler_method TEXT,
    name           TEXT,
    line           INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_routes_file ON routes(file_id);
CREATE INDEX IF NOT EXISTS idx_routes_method ON routes(method);

-- ──────────────────────────── parse errors ────────────────────────────

CREATE TABLE IF NOT EXISTS parse_errors (
    id    INTEGER PRIMARY KEY,
    path  TEXT NOT NULL,
    error TEXT NOT NULL
);

-- ──────────────────────────── FTS5: functions ────────────────────────────

-- External-content FTS5: row content lives in `functions`, indexed columns are
-- referenced by `content_rowid`. Search returns functions.id via the rowid.
CREATE VIRTUAL TABLE IF NOT EXISTS fts_functions USING fts5(
    name,
    qualified_name,
    docstring,
    body,
    search_text,
    content='functions',
    content_rowid='id'
);

CREATE TRIGGER IF NOT EXISTS functions_ai AFTER INSERT ON functions BEGIN
    INSERT INTO fts_functions(rowid, name, qualified_name, docstring, body, search_text)
    VALUES (new.id, new.name, new.qualified_name, new.docstring, new.body, new.search_text);
END;

CREATE TRIGGER IF NOT EXISTS functions_ad AFTER DELETE ON functions BEGIN
    INSERT INTO fts_functions(fts_functions, rowid, name, qualified_name, docstring, body, search_text)
    VALUES ('delete', old.id, old.name, old.qualified_name, old.docstring, old.body, old.search_text);
END;

CREATE TRIGGER IF NOT EXISTS functions_au AFTER UPDATE ON functions BEGIN
    INSERT INTO fts_functions(fts_functions, rowid, name, qualified_name, docstring, body, search_text)
    VALUES ('delete', old.id, old.name, old.qualified_name, old.docstring, old.body, old.search_text);
    INSERT INTO fts_functions(rowid, name, qualified_name, docstring, body, search_text)
    VALUES (new.id, new.name, new.qualified_name, new.docstring, new.body, new.search_text);
END;

-- ──────────────────────────── FTS5: classes ────────────────────────────

CREATE VIRTUAL TABLE IF NOT EXISTS fts_classes USING fts5(
    name,
    qualified_name,
    docstring,
    body,
    search_text,
    content='classes',
    content_rowid='id'
);

CREATE TRIGGER IF NOT EXISTS classes_ai AFTER INSERT ON classes BEGIN
    INSERT INTO fts_classes(rowid, name, qualified_name, docstring, body, search_text)
    VALUES (new.id, new.name, new.qualified_name, new.docstring, new.body, new.search_text);
END;

CREATE TRIGGER IF NOT EXISTS classes_ad AFTER DELETE ON classes BEGIN
    INSERT INTO fts_classes(fts_classes, rowid, name, qualified_name, docstring, body, search_text)
    VALUES ('delete', old.id, old.name, old.qualified_name, old.docstring, old.body, old.search_text);
END;

CREATE TRIGGER IF NOT EXISTS classes_au AFTER UPDATE ON classes BEGIN
    INSERT INTO fts_classes(fts_classes, rowid, name, qualified_name, docstring, body, search_text)
    VALUES ('delete', old.id, old.name, old.qualified_name, old.docstring, old.body, old.search_text);
    INSERT INTO fts_classes(rowid, name, qualified_name, docstring, body, search_text)
    VALUES (new.id, new.name, new.qualified_name, new.docstring, new.body, new.search_text);
END;

-- ──────────────────────────── meta (key/value) ────────────────────────────

-- Free-form store metadata. M2 stamps `embed_model` / `embed_dim` here once the
-- indexer embeds the first chunk (so `doctor` can detect a model/dim change vs
-- the fixed `vec_code` column width). Empty until then.
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT
);

-- ──────────────────────────── code chunks ────────────────────────────

-- One row per embeddable unit of text. `id` is the rowid the vector index keys
-- on (vec_code.rowid == code_chunks.id). `embed_model`/`embed_dim` are stamped
-- when a vector is written; NULL means "not yet embedded" (feeds pending_chunks).
CREATE TABLE IF NOT EXISTS code_chunks (
    id             INTEGER PRIMARY KEY,
    symbol_kind    TEXT,
    symbol_id      INTEGER,
    qualified_name TEXT,
    path           TEXT,
    line_start     INTEGER,
    line_end       INTEGER,
    chunk_text     TEXT NOT NULL,
    content_hash   TEXT NOT NULL,
    embed_model    TEXT,
    embed_dim      INTEGER
);

CREATE INDEX IF NOT EXISTS idx_code_chunks_hash ON code_chunks(content_hash);
CREATE INDEX IF NOT EXISTS idx_code_chunks_path ON code_chunks(path);

-- vec0 virtual table: f32 little-endian blobs, cosine distance. The column width
-- is FIXED (see VEC_DIM). Requires the sqlite-vec extension to be registered on
-- the connection BEFORE it is opened (see store::register_sqlite_vec).
CREATE VIRTUAL TABLE IF NOT EXISTS vec_code USING vec0(embedding float[1024] distance_metric=cosine);

-- vec0 has no FK cascade: mirror code_chunks deletes into vec_code by hand so the
-- two stay in lock-step (re-indexing a path DELETEs its chunks → drops vectors).
CREATE TRIGGER IF NOT EXISTS code_chunks_ad AFTER DELETE ON code_chunks BEGIN
    DELETE FROM vec_code WHERE rowid = old.id;
END;

-- ──────────────────────────── embed cache (content-addressed) ────────────────────────────

-- Persistent (content_hash, model) → vector cache, decoupled from `code_chunks`
-- rowids. Re-indexing a file DELETEs its chunks (and their `vec_code` rows) and
-- re-inserts fresh rows with NULL `embed_model`, so without this cache every
-- re-index — crucially every `git checkout` that rewrites a file, then every
-- switch back — would re-embed byte-identical text through the (slow) embedder.
--
-- The embed pass consults this table FIRST: a hit rehydrates the vector with zero
-- model calls; only genuine cache misses hit the embedder, and their result is
-- written back here. Keyed by content (not rowid), so a chunk that reverts to a
-- previously-seen state is free. The table is additive and survives re-indexing
-- (only a schema-version bump wipes the db); it grows with the set of distinct
-- chunk texts ever embedded for a model.
CREATE TABLE IF NOT EXISTS embed_cache (
    content_hash TEXT NOT NULL,
    embed_model  TEXT NOT NULL,
    embed_dim    INTEGER NOT NULL,
    vec          BLOB NOT NULL,
    PRIMARY KEY (content_hash, embed_model)
) WITHOUT ROWID;
"#;
