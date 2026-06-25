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

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;
use std::sync::Mutex;

use icode_core::error::{Error, Result};
use icode_core::model::*;
use icode_core::traits::{CodeReadStore, CodeWriteStore};
use regex::Regex;
use rusqlite::Connection;

/// Schema generation. Bumped whenever the on-disk layout changes incompatibly;
/// a per-project index with a different `PRAGMA user_version` is discarded and
/// rebuilt from source (the index is disposable).
const SCHEMA_VERSION: i64 = 1;

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
    ///
    /// If an existing db carries an incompatible `user_version` (an older iCode
    /// schema, or a stale `.icode` left by a different tool that reused the path),
    /// it is deleted and rebuilt rather than crashing on a schema mismatch — the
    /// per-project index is regenerable from source.
    pub fn open(root: &Path) -> Result<Self> {
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
        let conn = Connection::open_in_memory().map_err(store_err)?;
        Self::from_conn(conn)
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
        conn.execute_batch(schema::SCHEMA).map_err(store_err)?;
        // Stamp the schema generation so a future incompatible version is detected.
        conn.pragma_update(None, "user_version", SCHEMA_VERSION).map_err(store_err)?;
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
    /// Direct callers of `name`: rows in `calls` whose `callee` equals the name
    /// (or its qualified form). APPROXIMATE — matching is purely by name, so a
    /// homonym in another scope can over-match (typed resolution is M3b).
    fn get_callers(&self, name: &str, limit: usize) -> Result<Vec<Call>> {
        let conn = self.conn.lock().map_err(store_err)?;
        // Also match the bare last segment of a qualified callee, since the parser
        // stamps `callee` as the bare method name for most call shapes.
        let bare = last_name_segment(name);
        let mut stmt = conn
            .prepare(
                "SELECT path, caller, callee, receiver, line FROM calls \
                 WHERE callee = ?1 OR callee = ?2 \
                 ORDER BY path, line LIMIT ?3",
            )
            .map_err(store_err)?;
        let rows = stmt
            .query_map(rusqlite::params![name, bare, limit as i64], map_call)
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
        let mut stmt = conn
            .prepare(
                "SELECT path, caller, callee, receiver, line FROM calls \
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

        let edges = {
            let conn = self.conn.lock().map_err(store_err)?;
            load_call_edges(&conn)?
        };

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
        let conn = self.conn.lock().map_err(store_err)?;
        let edges = load_call_edges(&conn)?;
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

/// Map a `calls` row (path, caller, callee, receiver, line) into a `Call`.
fn map_call(row: &rusqlite::Row<'_>) -> rusqlite::Result<Call> {
    Ok(Call {
        path: row.get(0)?,
        caller: row.get(1)?,
        callee: row.get(2)?,
        receiver: row.get(3)?,
        line: row.get::<_, i64>(4)? as u32,
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
    // Handle both `::` (Rust/PHP) and `.` (Python/JS) qualifiers.
    let after_colons = name.rsplit("::").next().unwrap_or(name);
    after_colons.rsplit('.').next().unwrap_or(after_colons).to_string()
}

/// Resolve a function (then class) by `name`, optionally preferring `file_hint`.
/// Returns the wrapped definition, the symbol's path, and its qualified name.
impl SqliteCodeStore {
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
