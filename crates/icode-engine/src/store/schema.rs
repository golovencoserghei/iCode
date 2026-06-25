//! SQLite schema for the per-project code graph (M1: full node set —
//! files + functions + classes + imports + calls + routes, with FTS5 over
//! functions and classes). Vector/`code_chunks` tables land in M2.

/// DDL applied once at store open. Idempotent (`IF NOT EXISTS`). The FTS5 indexes
/// are contentless-external over `functions`/`classes` (rowid == table id), so a
/// search `MATCH` yields the owning rows without duplicating body text. All child
/// tables cascade-delete from `files`, so re-indexing a file (DELETE + INSERT)
/// transparently clears its old graph rows.
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
    is_async       INTEGER NOT NULL DEFAULT 0
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
    node_hash      TEXT
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

CREATE TABLE IF NOT EXISTS calls (
    id       INTEGER PRIMARY KEY,
    file_id  INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    path     TEXT NOT NULL,
    caller   TEXT NOT NULL,
    callee   TEXT NOT NULL,
    receiver TEXT,
    line     INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_calls_file ON calls(file_id);
CREATE INDEX IF NOT EXISTS idx_calls_callee ON calls(callee);
CREATE INDEX IF NOT EXISTS idx_calls_caller ON calls(caller);

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
    content='functions',
    content_rowid='id'
);

CREATE TRIGGER IF NOT EXISTS functions_ai AFTER INSERT ON functions BEGIN
    INSERT INTO fts_functions(rowid, name, qualified_name, docstring, body)
    VALUES (new.id, new.name, new.qualified_name, new.docstring, new.body);
END;

CREATE TRIGGER IF NOT EXISTS functions_ad AFTER DELETE ON functions BEGIN
    INSERT INTO fts_functions(fts_functions, rowid, name, qualified_name, docstring, body)
    VALUES ('delete', old.id, old.name, old.qualified_name, old.docstring, old.body);
END;

CREATE TRIGGER IF NOT EXISTS functions_au AFTER UPDATE ON functions BEGIN
    INSERT INTO fts_functions(fts_functions, rowid, name, qualified_name, docstring, body)
    VALUES ('delete', old.id, old.name, old.qualified_name, old.docstring, old.body);
    INSERT INTO fts_functions(rowid, name, qualified_name, docstring, body)
    VALUES (new.id, new.name, new.qualified_name, new.docstring, new.body);
END;

-- ──────────────────────────── FTS5: classes ────────────────────────────

CREATE VIRTUAL TABLE IF NOT EXISTS fts_classes USING fts5(
    name,
    qualified_name,
    docstring,
    body,
    content='classes',
    content_rowid='id'
);

CREATE TRIGGER IF NOT EXISTS classes_ai AFTER INSERT ON classes BEGIN
    INSERT INTO fts_classes(rowid, name, qualified_name, docstring, body)
    VALUES (new.id, new.name, new.qualified_name, new.docstring, new.body);
END;

CREATE TRIGGER IF NOT EXISTS classes_ad AFTER DELETE ON classes BEGIN
    INSERT INTO fts_classes(fts_classes, rowid, name, qualified_name, docstring, body)
    VALUES ('delete', old.id, old.name, old.qualified_name, old.docstring, old.body);
END;

CREATE TRIGGER IF NOT EXISTS classes_au AFTER UPDATE ON classes BEGIN
    INSERT INTO fts_classes(fts_classes, rowid, name, qualified_name, docstring, body)
    VALUES ('delete', old.id, old.name, old.qualified_name, old.docstring, old.body);
    INSERT INTO fts_classes(rowid, name, qualified_name, docstring, body)
    VALUES (new.id, new.name, new.qualified_name, new.docstring, new.body);
END;
"#;
