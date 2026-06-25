//! SQLite schema for the per-project code graph (M0.5 walking-skeleton subset:
//! files + functions + FTS5). Classes/imports/calls/routes land in M1+.

/// DDL applied once at store open. Idempotent (`IF NOT EXISTS`). The FTS5 index
/// is contentless-external over `functions` (rowid == functions.id), so a search
/// `MATCH` yields the owning function rows without duplicating body text.
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

CREATE TABLE IF NOT EXISTS parse_errors (
    id    INTEGER PRIMARY KEY,
    path  TEXT NOT NULL,
    error TEXT NOT NULL
);

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

-- Keep the FTS index in sync with the base table via triggers.
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
"#;
