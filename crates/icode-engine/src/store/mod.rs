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
            classes: Self::count_table(&conn, "classes")?,
            calls: Self::count_table(&conn, "calls")?,
            imports: Self::count_table(&conn, "imports")?,
            routes: Self::count_table(&conn, "routes")?,
            parse_errors: Self::count_table(&conn, "parse_errors")?,
            ..DbStats::default()
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

    fn repo_map(&self, _top: usize) -> Result<RepoMap> {
        Err(not_impl())
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
    fn grep_code(&self, _pattern: &str, _lang: Option<Language>, _limit: usize) -> Result<Vec<GrepHit>> {
        Err(not_impl())
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

        {
            let mut stmt = tx
                .prepare(
                    "INSERT INTO classes \
                     (file_id, name, qualified_name, path, language, line_start, line_end, \
                      bases, docstring, body, node_hash) \
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
                )
                .map_err(store_err)?;
            for c in classes {
                // `bases` is stored as a JSON array of strings (serde_json).
                let bases_json = serde_json::to_string(&c.bases).map_err(store_err)?;
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
                    "INSERT INTO calls (file_id, path, caller, callee, receiver, line) \
                     VALUES (?1,?2,?3,?4,?5,?6)",
                )
                .map_err(store_err)?;
            for c in calls {
                stmt.execute(rusqlite::params![
                    file_id,
                    c.path,
                    c.caller,
                    c.callee,
                    c.receiver,
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
