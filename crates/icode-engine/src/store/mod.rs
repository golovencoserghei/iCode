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

use std::path::Path;
use std::sync::Mutex;

use icode_core::error::{Error, Result};
use icode_core::model::*;
use icode_core::traits::{CodeReadStore, CodeWriteStore};
use rusqlite::Connection;

/// Map any rusqlite error into the framework-free `Error::Store` variant.
fn store_err<E: std::fmt::Display>(e: E) -> Error {
    Error::Store(e.to_string())
}

/// Per-project code store backed by a single SQLite file at `<root>/.icode/index.db`.
pub struct SqliteCodeStore {
    conn: Mutex<Connection>,
}

impl SqliteCodeStore {
    /// Open (creating if needed) the index db under `<root>/.icode/index.db`.
    /// Creates the `.icode` directory, sets WAL + foreign_keys, applies schema.
    pub fn open(root: &Path) -> Result<Self> {
        let dir = root.join(".icode");
        std::fs::create_dir_all(&dir).map_err(|e| Error::Io(e.to_string()))?;
        let db_path = dir.join("index.db");
        let conn = Connection::open(&db_path).map_err(store_err)?;
        Self::from_conn(conn)
    }

    /// Open an in-memory store (tests / ephemeral use).
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().map_err(store_err)?;
        Self::from_conn(conn)
    }

    fn from_conn(conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL").map_err(store_err)?;
        conn.pragma_update(None, "foreign_keys", "ON").map_err(store_err)?;
        conn.execute_batch(schema::SCHEMA).map_err(store_err)?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    fn count_table(conn: &Connection, table: &str) -> Result<u64> {
        let sql = format!("SELECT COUNT(*) FROM {table}");
        conn.query_row(&sql, [], |r| r.get::<_, i64>(0))
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
            parse_errors: Self::count_table(&conn, "parse_errors")?,
            ..DbStats::default()
        })
    }

    fn search_code(&self, q: &CodeQuery) -> Result<Vec<CodeHit>> {
        // M0.5: only the Function vertical slice. Class/FileWindow → empty.
        match q.kind {
            None | Some(SymbolKind::Function) => {}
            Some(_) => return Ok(vec![]),
        }
        // No embeddings in the skeleton: Semantic has nothing to search.
        // Lexical and Hybrid both resolve to FTS5/BM25 here.
        if q.mode == SearchMode::Semantic {
            return Ok(vec![]);
        }
        if q.text.trim().is_empty() {
            return Ok(vec![]);
        }

        let conn = self.conn.lock().map_err(store_err)?;
        // FTS5 over name/qualified_name. `bm25()` returns lower = better; negate
        // so higher `score` = more relevant (matches the CodeHit contract).
        let mut stmt = conn
            .prepare(
                "SELECT f.name, f.qualified_name, f.path, f.line_start, f.line_end, f.body, \
                        bm25(fts_functions) AS rank \
                 FROM fts_functions \
                 JOIN functions f ON f.id = fts_functions.rowid \
                 WHERE fts_functions MATCH ?1 \
                 ORDER BY rank ASC \
                 LIMIT ?2",
            )
            .map_err(store_err)?;

        let match_expr = fts_match_expr(&q.text);
        let with_body = q.with_body;
        let limit = q.limit as i64;
        let rows = stmt
            .query_map(rusqlite::params![match_expr, limit], |row| {
                let body: String = row.get(5)?;
                let rank: f64 = row.get(6)?;
                Ok(CodeHit {
                    kind: SymbolKind::Function,
                    name: row.get(0)?,
                    qualified_name: row.get(1)?,
                    path: row.get(2)?,
                    line_start: row.get::<_, i64>(3)? as u32,
                    line_end: row.get::<_, i64>(4)? as u32,
                    score: -rank as f32,
                    snippet: if with_body { Some(body) } else { None },
                    stale: false,
                })
            })
            .map_err(store_err)?;

        let mut hits = Vec::new();
        for hit in rows {
            hits.push(hit.map_err(store_err)?);
        }
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

    fn repo_map(&self, _top: usize) -> Result<RepoMap> {
        Err(not_impl())
    }
    fn find_similar(&self, _qualified_name: &str, _limit: usize) -> Result<Vec<CodeHit>> {
        Ok(vec![])
    }
    fn get_class(&self, _name: &str, _lang: Option<Language>, _with_body: bool) -> Result<Option<ClassDef>> {
        Ok(None)
    }
    fn file_outline(&self, _path: &str) -> Result<Vec<CodeHit>> {
        Ok(vec![])
    }
    fn symbol_context(&self, _name: &str, _file_hint: Option<&str>) -> Result<SymbolContext> {
        Err(not_impl())
    }
    fn get_callers(&self, _name: &str, _limit: usize) -> Result<Vec<Call>> {
        Ok(vec![])
    }
    fn get_callees(&self, _name: &str, _limit: usize) -> Result<Vec<Call>> {
        Ok(vec![])
    }
    fn call_chain(&self, _from: &str, _to: &str, _max_depth: usize) -> Result<Vec<String>> {
        Ok(vec![])
    }
    fn find_dependencies(&self, _path: &str, _depth: usize) -> Result<Vec<String>> {
        Ok(vec![])
    }
    fn impact_analysis(&self, _path: &str, _depth: usize) -> Result<Vec<String>> {
        Ok(vec![])
    }
    fn find_implementations(&self, _name: &str) -> Result<Vec<String>> {
        Ok(vec![])
    }
    fn find_dead_code(&self, _lang: Option<Language>, _limit: usize) -> Result<Vec<CodeHit>> {
        Ok(vec![])
    }
    fn find_unreachable(&self, _lang: Option<Language>, _limit: usize) -> Result<Vec<CodeHit>> {
        Ok(vec![])
    }
    fn find_complex_functions(&self, _lang: Option<Language>, _limit: usize) -> Result<Vec<ComplexFunction>> {
        Ok(vec![])
    }
    fn find_routes(
        &self,
        _method: Option<&str>,
        _path: Option<&str>,
        _handler: Option<&str>,
        _limit: usize,
    ) -> Result<Vec<Route>> {
        Ok(vec![])
    }
    fn grep_code(&self, _pattern: &str, _lang: Option<Language>, _limit: usize) -> Result<Vec<GrepHit>> {
        Err(not_impl())
    }
    fn list_files(&self, _pattern: Option<&str>, _lang: Option<Language>, _limit: usize) -> Result<Vec<FileRecord>> {
        Ok(vec![])
    }
    fn stat_file(&self, _path: &str) -> Result<Option<FileRecord>> {
        Ok(None)
    }
    fn read_file(&self, _path: &str, _start: Option<u32>, _end: Option<u32>) -> Result<String> {
        Err(not_impl())
    }
    fn chunk_hits(&self, _rowids: &[i64]) -> Result<Vec<(i64, CodeHit)>> {
        Ok(vec![])
    }
}

// ──────────────────────────── write side ────────────────────────────

impl CodeWriteStore for SqliteCodeStore {
    fn upsert_file(
        &self,
        file: &FileRecord,
        functions: &[FunctionDef],
        _classes: &[ClassDef],
        _imports: &[Import],
        _calls: &[Call],
        _routes: &[Route],
    ) -> Result<()> {
        // M0.5: only files + functions are persisted; OOP/graph rows arrive in M1+.
        let mut conn = self.conn.lock().map_err(store_err)?;
        let tx = conn.transaction().map_err(store_err)?;

        // Replace the file row (ON DELETE CASCADE drops its old functions).
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
                      args, return_type, docstring, body, is_async) \
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
                )
                .map_err(store_err)?;
            for f in functions {
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
                ])
                .map_err(store_err)?;
            }
        }

        tx.commit().map_err(store_err)
    }

    fn delete_file(&self, path: &str) -> Result<()> {
        let conn = self.conn.lock().map_err(store_err)?;
        conn.execute("DELETE FROM files WHERE path = ?1", rusqlite::params![path])
            .map_err(store_err)?;
        Ok(())
    }

    fn upsert_chunks(&self, _path: &str, _chunks: &[CodeChunk]) -> Result<Vec<i64>> {
        // No embeddings/chunks in the skeleton.
        Ok(vec![])
    }

    fn pending_chunks(&self, _embed_model: &str, _limit: usize) -> Result<Vec<(i64, String)>> {
        Ok(vec![])
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

fn not_impl() -> Error {
    Error::Other("not implemented (M1+)".into())
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
