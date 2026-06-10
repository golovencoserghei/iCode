/// FTS поиск, точные lookup'ы, импорты, file summary/outline, статистика.
use super::{sanitize_fts_query, row_to_function, row_to_class, row_to_import, row_to_variable, Storage};
use super::models::*;
use anyhow::Result;
use rusqlite::params;

/// Возвращает максимальную позицию ≤ max_bytes, которая является границей UTF-8.
fn utf8_boundary(s: &str, max_bytes: usize) -> usize {
    if s.len() <= max_bytes { return s.len(); }
    let mut idx = max_bytes;
    while idx > 0 && !s.is_char_boundary(idx) { idx -= 1; }
    idx
}

/// Разбивает camelCase/snake_case идентификатор на токены длиной ≥ 3 символа.
/// Примеры: "installProject" → ["install","project"],
///          "get_file_summary" → ["get","file","summary"]
fn camel_snake_tokens(query: &str) -> Vec<String> {
    let mut tokens: Vec<String> = Vec::new();
    let mut cur = String::new();
    for ch in query.chars() {
        if ch == '_' || ch == '-' || ch == '.' {
            if !cur.is_empty() { tokens.push(cur.to_lowercase()); cur.clear(); }
        } else if ch.is_uppercase() && !cur.is_empty() {
            tokens.push(cur.to_lowercase());
            cur = ch.to_lowercase().collect();
        } else {
            cur.push(ch);
        }
    }
    if !cur.is_empty() { tokens.push(cur.to_lowercase()); }
    tokens.retain(|t| t.len() >= 3);
    tokens
}

impl Storage {
    // ── FTS поиск ────────────────────────────────────────────────────────────

    pub fn search_functions(&self, query: &str, limit: usize, language: Option<&str>) -> Result<Vec<FunctionRecord>> {
        let safe_query = sanitize_fts_query(query);
        match language {
            Some(lang) => {
                let mut stmt = self.conn.prepare(
                    "SELECT f.id, f.file_id, f.name, f.qualified_name, f.line_start, f.line_end,
                            f.args, f.return_type, f.docstring, f.body, f.is_async, f.node_hash
                     FROM fts_functions ft JOIN functions f ON f.id = ft.rowid
                     JOIN files fi ON fi.id = f.file_id
                     WHERE fts_functions MATCH ?1 AND fi.language = ?2
                     ORDER BY rank LIMIT ?3",
                )?;
                let rows = stmt.query_map(params![safe_query, lang, limit as i64], row_to_function)?;
                rows.map(|r| r.map_err(Into::into)).collect()
            }
            None => {
                let mut stmt = self.conn.prepare(
                    "SELECT f.id, f.file_id, f.name, f.qualified_name, f.line_start, f.line_end,
                            f.args, f.return_type, f.docstring, f.body, f.is_async, f.node_hash
                     FROM fts_functions ft JOIN functions f ON f.id = ft.rowid
                     WHERE fts_functions MATCH ?1 ORDER BY rank LIMIT ?2",
                )?;
                let rows = stmt.query_map(params![safe_query, limit as i64], row_to_function)?;
                rows.map(|r| r.map_err(Into::into)).collect()
            }
        }
    }

    /// Fuzzy-поиск функций: токенизирует camelCase/snake_case запрос и ищет по LIKE.
    /// Используется как fallback когда FTS5 MATCH возвращает 0 результатов.
    pub fn search_functions_fuzzy(&self, query: &str, limit: usize) -> Result<Vec<FunctionRecord>> {
        let tokens = camel_snake_tokens(query);
        if tokens.is_empty() { return Ok(vec![]); }
        let conds: Vec<String> = tokens.iter().map(|_| "name LIKE ?".to_string()).collect();
        let sql = format!(
            "SELECT id, file_id, name, qualified_name, line_start, line_end,
                    args, return_type, docstring, body, is_async, node_hash
             FROM functions WHERE {} ORDER BY length(name) LIMIT ?",
            conds.join(" OR ")
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = tokens.iter()
            .map(|t| Box::new(format!("%{}%", t)) as Box<dyn rusqlite::ToSql>)
            .collect();
        params.push(Box::new(limit as i64));
        let params_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|b| &**b as _).collect();
        let rows = stmt.query_map(params_refs.as_slice(), row_to_function)?;
        rows.map(|r| r.map_err(Into::into)).collect()
    }

    pub fn search_classes(&self, query: &str, limit: usize, language: Option<&str>) -> Result<Vec<ClassRecord>> {
        let safe_query = sanitize_fts_query(query);
        match language {
            Some(lang) => {
                let mut stmt = self.conn.prepare(
                    "SELECT c.id, c.file_id, c.name, c.line_start, c.line_end,
                            c.bases, c.docstring, c.body, c.node_hash
                     FROM fts_classes ft JOIN classes c ON c.id = ft.rowid
                     JOIN files fi ON fi.id = c.file_id
                     WHERE fts_classes MATCH ?1 AND fi.language = ?2
                     ORDER BY rank LIMIT ?3",
                )?;
                let rows = stmt.query_map(params![safe_query, lang, limit as i64], row_to_class)?;
                rows.map(|r| r.map_err(Into::into)).collect()
            }
            None => {
                let mut stmt = self.conn.prepare(
                    "SELECT c.id, c.file_id, c.name, c.line_start, c.line_end,
                            c.bases, c.docstring, c.body, c.node_hash
                     FROM fts_classes ft JOIN classes c ON c.id = ft.rowid
                     WHERE fts_classes MATCH ?1 ORDER BY rank LIMIT ?2",
                )?;
                let rows = stmt.query_map(params![safe_query, limit as i64], row_to_class)?;
                rows.map(|r| r.map_err(Into::into)).collect()
            }
        }
    }

    pub fn search_text(&self, query: &str, limit: usize, language: Option<&str>) -> Result<Vec<(String, String)>> {
        let safe_query = sanitize_fts_query(query);
        match language {
            Some(lang) => {
                let mut stmt = self.conn.prepare(
                    "SELECT fi.path, snippet(fts_text_files, 0, '[', ']', '...', 20)
                     FROM fts_text_files ft JOIN text_files tf ON tf.id = ft.rowid
                     JOIN files fi ON fi.id = tf.file_id
                     WHERE fts_text_files MATCH ?1 AND fi.language = ?2
                     ORDER BY rank LIMIT ?3",
                )?;
                let rows = stmt.query_map(params![safe_query, lang, limit as i64], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?;
                rows.map(|r| r.map_err(Into::into)).collect()
            }
            None => {
                let mut stmt = self.conn.prepare(
                    "SELECT fi.path, snippet(fts_text_files, 0, '[', ']', '...', 20)
                     FROM fts_text_files ft JOIN text_files tf ON tf.id = ft.rowid
                     JOIN files fi ON fi.id = tf.file_id
                     WHERE fts_text_files MATCH ?1 ORDER BY rank LIMIT ?2",
                )?;
                let rows = stmt.query_map(params![safe_query, limit as i64], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?;
                rows.map(|r| r.map_err(Into::into)).collect()
            }
        }
    }

    // ── Точные lookup'ы ──────────────────────────────────────────────────────

    pub fn get_function_by_name(&self, name: &str) -> Result<Vec<FunctionRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, file_id, name, qualified_name, line_start, line_end,
                    args, return_type, docstring, body, is_async, node_hash
             FROM functions WHERE name = ?1",
        )?;
        let rows = stmt.query_map(params![name], row_to_function)?;
        rows.map(|r| r.map_err(Into::into)).collect()
    }

    pub fn get_class_by_name(&self, name: &str) -> Result<Vec<ClassRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, file_id, name, line_start, line_end,
                    bases, docstring, body, node_hash
             FROM classes WHERE name = ?1",
        )?;
        let rows = stmt.query_map(params![name], row_to_class)?;
        rows.map(|r| r.map_err(Into::into)).collect()
    }

    pub fn get_callees(&self, function_name: &str, language: Option<&str>) -> Result<Vec<CallRecord>> {
        use super::row_to_call;
        match language {
            Some(lang) => {
                let mut stmt = self.conn.prepare(
                    "SELECT c.id, c.file_id, c.caller, c.callee, c.line, c.receiver
                     FROM calls c JOIN files fi ON fi.id = c.file_id
                     WHERE c.caller = ?1 AND fi.language = ?2",
                )?;
                let rows = stmt.query_map(params![function_name, lang], row_to_call)?;
                rows.map(|r| r.map_err(Into::into)).collect()
            }
            None => {
                let mut stmt = self.conn.prepare(
                    "SELECT id, file_id, caller, callee, line, receiver FROM calls WHERE caller = ?1",
                )?;
                let rows = stmt.query_map(params![function_name], row_to_call)?;
                rows.map(|r| r.map_err(Into::into)).collect()
            }
        }
    }

    pub fn get_callers(&self, function_name: &str, language: Option<&str>) -> Result<Vec<CallRecord>> {
        use super::row_to_call;
        match language {
            Some(lang) => {
                let mut stmt = self.conn.prepare(
                    "SELECT c.id, c.file_id, c.caller, c.callee, c.line, c.receiver
                     FROM calls c JOIN files fi ON fi.id = c.file_id
                     WHERE c.callee = ?1 AND fi.language = ?2",
                )?;
                let rows = stmt.query_map(params![function_name, lang], row_to_call)?;
                rows.map(|r| r.map_err(Into::into)).collect()
            }
            None => {
                let mut stmt = self.conn.prepare(
                    "SELECT id, file_id, caller, callee, line, receiver FROM calls WHERE callee = ?1",
                )?;
                let rows = stmt.query_map(params![function_name], row_to_call)?;
                rows.map(|r| r.map_err(Into::into)).collect()
            }
        }
    }

    /// Объединённый поиск символа по имени (функции + классы + переменные + импорты)
    pub fn find_symbol(&self, name: &str, language: Option<&str>) -> Result<SymbolSearchResult> {
        let functions = match language {
            Some(lang) => {
                let mut stmt = self.conn.prepare(
                    "SELECT f.id, f.file_id, f.name, f.qualified_name, f.line_start, f.line_end,
                            f.args, f.return_type, f.docstring, f.body, f.is_async, f.node_hash
                     FROM functions f JOIN files fi ON fi.id = f.file_id
                     WHERE (f.name = ?1 OR f.qualified_name = ?1) AND fi.language = ?2",
                )?;
                let r = stmt.query_map(params![name, lang], row_to_function)?
                    .map(|r| r.map_err(Into::into)).collect::<Result<Vec<_>>>()?; r
            }
            None => {
                let mut stmt = self.conn.prepare(
                    "SELECT id, file_id, name, qualified_name, line_start, line_end,
                            args, return_type, docstring, body, is_async, node_hash
                     FROM functions WHERE name = ?1 OR qualified_name = ?1",
                )?;
                let r = stmt.query_map(params![name], row_to_function)?
                    .map(|r| r.map_err(Into::into)).collect::<Result<Vec<_>>>()?; r
            }
        };
        let classes = match language {
            Some(lang) => {
                let mut stmt = self.conn.prepare(
                    "SELECT c.id, c.file_id, c.name, c.line_start, c.line_end,
                            c.bases, c.docstring, c.body, c.node_hash
                     FROM classes c JOIN files fi ON fi.id = c.file_id
                     WHERE c.name = ?1 AND fi.language = ?2",
                )?;
                let r = stmt.query_map(params![name, lang], row_to_class)?
                    .map(|r| r.map_err(Into::into)).collect::<Result<Vec<_>>>()?; r
            }
            None => {
                let mut stmt = self.conn.prepare(
                    "SELECT id, file_id, name, line_start, line_end, bases, docstring, body, node_hash
                     FROM classes WHERE name = ?1",
                )?;
                let r = stmt.query_map(params![name], row_to_class)?
                    .map(|r| r.map_err(Into::into)).collect::<Result<Vec<_>>>()?; r
            }
        };
        let variables = {
            let mut stmt = self.conn.prepare(
                "SELECT id, file_id, name, value, line FROM variables WHERE name = ?1",
            )?;
            let r = stmt.query_map(params![name], row_to_variable)?
                .map(|r| r.map_err(Into::into)).collect::<Result<Vec<_>>>()?; r
        };
        let imports = match language {
            Some(lang) => {
                let mut stmt = self.conn.prepare(
                    "SELECT i.id, i.file_id, i.module, i.name, i.alias, i.line, i.kind
                     FROM imports i JOIN files fi ON fi.id = i.file_id
                     WHERE (i.name = ?1 OR i.alias = ?1) AND fi.language = ?2",
                )?;
                let r = stmt.query_map(params![name, lang], row_to_import)?
                    .map(|r| r.map_err(Into::into)).collect::<Result<Vec<_>>>()?; r
            }
            None => {
                let mut stmt = self.conn.prepare(
                    "SELECT id, file_id, module, name, alias, line, kind
                     FROM imports WHERE name = ?1 OR alias = ?1",
                )?;
                let r = stmt.query_map(params![name], row_to_import)?
                    .map(|r| r.map_err(Into::into)).collect::<Result<Vec<_>>>()?; r
            }
        };
        Ok(SymbolSearchResult { functions, classes, variables, imports })
    }

    // ── Импорты ──────────────────────────────────────────────────────────────

    pub fn get_imports_by_file(&self, file_id: i64) -> Result<Vec<ImportRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, file_id, module, name, alias, line, kind
             FROM imports WHERE file_id = ?1 ORDER BY line",
        )?;
        let rows = stmt.query_map(params![file_id], row_to_import)?;
        rows.map(|r| r.map_err(Into::into)).collect()
    }

    pub fn get_imports_by_module(&self, module: &str, language: Option<&str>) -> Result<Vec<ImportRecord>> {
        match language {
            Some(lang) => {
                let mut stmt = self.conn.prepare(
                    "SELECT i.id, i.file_id, i.module, i.name, i.alias, i.line, i.kind
                     FROM imports i JOIN files fi ON fi.id = i.file_id
                     WHERE i.module = ?1 AND fi.language = ?2",
                )?;
                let rows = stmt.query_map(params![module, lang], row_to_import)?;
                rows.map(|r| r.map_err(Into::into)).collect()
            }
            None => {
                let mut stmt = self.conn.prepare(
                    "SELECT id, file_id, module, name, alias, line, kind
                     FROM imports WHERE module = ?1",
                )?;
                let rows = stmt.query_map(params![module], row_to_import)?;
                rows.map(|r| r.map_err(Into::into)).collect()
            }
        }
    }

    // ── File summary / outline / stats ───────────────────────────────────────

    /// Возвращает сводку файла. `body_cap` ограничивает длину тела каждой
    /// функции/класса в байтах (0 = без ограничения). При урезании
    /// `FileSummary::bodies_truncated` выставляется в `true`.
    pub fn get_file_summary(&self, path: &str, body_cap: usize) -> Result<Option<FileSummary>> {
        let file = match self.get_file_by_path(path)? { Some(f) => f, None => return Ok(None) };
        let file_id = file.id.unwrap();
        let mut functions = {
            let mut stmt = self.conn.prepare(
                "SELECT id, file_id, name, qualified_name, line_start, line_end,
                        args, return_type, docstring, body, is_async, node_hash
                 FROM functions WHERE file_id = ?1 ORDER BY line_start",
            )?;
            let r = stmt.query_map(params![file_id], row_to_function)?
                .map(|r| r.map_err(Into::into)).collect::<Result<Vec<_>>>()?; r
        };
        let mut classes = {
            let mut stmt = self.conn.prepare(
                "SELECT id, file_id, name, line_start, line_end, bases, docstring, body, node_hash
                 FROM classes WHERE file_id = ?1 ORDER BY line_start",
            )?;
            let r = stmt.query_map(params![file_id], row_to_class)?
                .map(|r| r.map_err(Into::into)).collect::<Result<Vec<_>>>()?; r
        };
        let imports = self.get_imports_by_file(file_id)?;
        let variables = {
            let mut stmt = self.conn.prepare(
                "SELECT id, file_id, name, value, line FROM variables WHERE file_id = ?1 ORDER BY line",
            )?;
            let r = stmt.query_map(params![file_id], row_to_variable)?
                .map(|r| r.map_err(Into::into)).collect::<Result<Vec<_>>>()?; r
        };

        let mut bodies_truncated = false;
        if body_cap > 0 {
            for f in &mut functions {
                if f.body.len() > body_cap {
                    let boundary = utf8_boundary(&f.body, body_cap);
                    f.body.truncate(boundary);
                    bodies_truncated = true;
                }
            }
            for c in &mut classes {
                if c.body.len() > body_cap {
                    let boundary = utf8_boundary(&c.body, body_cap);
                    c.body.truncate(boundary);
                    bodies_truncated = true;
                }
            }
        }

        Ok(Some(FileSummary { file, functions, classes, imports, variables, bodies_truncated }))
    }


    pub fn get_file_outline(&self, path: &str) -> Result<Option<FileOutline>> {
        let file = match self.get_file_by_path(path)? { Some(f) => f, None => return Ok(None) };
        let file_id = match file.id { Some(id) => id, None => return Ok(None) };

        let mut symbols: Vec<FileOutlineEntry> = Vec::new();

        let mut stmt = self.conn.prepare_cached(
            "SELECT name, qualified_name, line_start, line_end, is_async
             FROM functions WHERE file_id = ?1 ORDER BY line_start",
        )?;
        let rows = stmt.query_map(params![file_id], |row| {
            Ok(FileOutlineEntry {
                name: row.get(0)?, qualified_name: row.get(1)?,
                kind: "function".to_string(),
                line_start: row.get::<_, i64>(2)? as usize,
                line_end:   row.get::<_, i64>(3)? as usize,
                is_async: Some(row.get::<_, bool>(4)?),
            })
        })?;
        for r in rows { symbols.push(r?); }

        let mut stmt = self.conn.prepare_cached(
            "SELECT name, line_start, line_end FROM classes WHERE file_id = ?1 ORDER BY line_start",
        )?;
        let rows = stmt.query_map(params![file_id], |row| {
            Ok(FileOutlineEntry {
                name: row.get(0)?, qualified_name: None,
                kind: "class".to_string(),
                line_start: row.get::<_, i64>(1)? as usize,
                line_end:   row.get::<_, i64>(2)? as usize,
                is_async: None,
            })
        })?;
        for r in rows { symbols.push(r?); }

        let mut stmt = self.conn.prepare_cached(
            "SELECT COALESCE(name, module, '?'), line FROM imports WHERE file_id = ?1 ORDER BY line LIMIT 30",
        )?;
        let rows = stmt.query_map(params![file_id], |row| {
            let ln = row.get::<_, i64>(1)? as usize;
            Ok(FileOutlineEntry {
                name: row.get(0)?, qualified_name: None,
                kind: "import".to_string(),
                line_start: ln, line_end: ln, is_async: None,
            })
        })?;
        for r in rows { symbols.push(r?); }

        symbols.sort_by_key(|s| s.line_start);
        Ok(Some(FileOutline { path: file.path, language: file.language, lines_total: file.lines_total, symbols }))
    }

    pub fn get_stats(&self) -> Result<DbStats> {
        let count = |table: &str| -> Result<usize> {
            let n: i64 = self.conn.query_row(
                &format!("SELECT COUNT(*) FROM {table}"), [], |row| row.get(0),
            )?;
            Ok(n as usize)
        };
        Ok(DbStats {
            total_files:      count("files")?,
            total_functions:  count("functions")?,
            total_classes:    count("classes")?,
            total_imports:    count("imports")?,
            total_calls:      count("calls")?,
            total_variables:  count("variables")?,
            total_text_files: count("text_files")?,
            // Слепые зоны индекса: файлы, которые не удалось распарсить.
            parse_errors:     count("parse_errors").unwrap_or(0),
            indexing_status: None,
        })
    }
}
