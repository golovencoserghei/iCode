/// Инструменты глубокого анализа: транзитивный call-граф, реализации, мёртвый код.
use super::{normalize_glob, Storage};
use super::models::*;
use anyhow::Result;
use rusqlite::params;

impl Storage {
    /// Транзитивные вызыватели функции (BFS, защита от циклов).
    /// Возвращает плоский список с полем `depth` (1 = прямой caller).
    pub fn get_callers_transitive(
        &self,
        function_name: &str,
        max_depth: usize,
        language: Option<&str>,
    ) -> Result<Vec<CallTreeNode>> {
        use std::collections::{HashSet, VecDeque};
        let mut result: Vec<CallTreeNode> = Vec::new();
        let mut visited: HashSet<String> = HashSet::new();
        let mut queue: VecDeque<(String, usize)> = VecDeque::new();

        visited.insert(function_name.to_string());
        queue.push_back((function_name.to_string(), 0));

        while let Some((name, depth)) = queue.pop_front() {
            if depth >= max_depth { continue; }
            for call in self.get_callers(&name, language)? {
                let path = self.get_path_by_file_id(call.file_id)?.unwrap_or_default();
                result.push(CallTreeNode { name: call.caller.clone(), file_path: path, line: call.line, depth: depth + 1 });
                if !visited.contains(&call.caller) {
                    visited.insert(call.caller.clone());
                    queue.push_back((call.caller, depth + 1));
                }
            }
        }
        Ok(result)
    }

    /// Транзитивные вызываемые функции (BFS, защита от циклов).
    pub fn get_callees_transitive(
        &self,
        function_name: &str,
        max_depth: usize,
        language: Option<&str>,
    ) -> Result<Vec<CallTreeNode>> {
        use std::collections::{HashSet, VecDeque};
        let mut result: Vec<CallTreeNode> = Vec::new();
        let mut visited: HashSet<String> = HashSet::new();
        let mut queue: VecDeque<(String, usize)> = VecDeque::new();

        visited.insert(function_name.to_string());
        queue.push_back((function_name.to_string(), 0));

        while let Some((name, depth)) = queue.pop_front() {
            if depth >= max_depth { continue; }
            for call in self.get_callees(&name, language)? {
                let path = self.get_path_by_file_id(call.file_id)?.unwrap_or_default();
                result.push(CallTreeNode { name: call.callee.clone(), file_path: path, line: call.line, depth: depth + 1 });
                if !visited.contains(&call.callee) {
                    visited.insert(call.callee.clone());
                    queue.push_back((call.callee, depth + 1));
                }
            }
        }
        Ok(result)
    }

    /// Найти классы, наследующие / реализующие данный базовый класс или интерфейс.
    /// LIKE-поиск по полю `bases` с точным word-match в post-filter.
    pub fn get_implementations(
        &self,
        class_name: &str,
        language: Option<&str>,
    ) -> Result<Vec<ImplementationRecord>> {
        let like_pattern = format!("%{}%", class_name);
        let sql = match language {
            Some(_) =>
                "SELECT c.name, fi.path, c.line_start, c.line_end, c.bases, c.docstring
                 FROM classes c JOIN files fi ON fi.id = c.file_id
                 WHERE c.bases IS NOT NULL AND c.bases LIKE ?1 AND fi.language = ?2
                 ORDER BY fi.path, c.line_start",
            None =>
                "SELECT c.name, fi.path, c.line_start, c.line_end, c.bases, c.docstring
                 FROM classes c JOIN files fi ON fi.id = c.file_id
                 WHERE c.bases IS NOT NULL AND c.bases LIKE ?1
                 ORDER BY fi.path, c.line_start",
        };
        let row_mapper = |row: &rusqlite::Row| -> rusqlite::Result<ImplementationRecord> {
            Ok(ImplementationRecord {
                name:       row.get(0)?,
                file_path:  row.get(1)?,
                line_start: row.get::<_, i64>(2)? as usize,
                line_end:   row.get::<_, i64>(3)? as usize,
                bases:      row.get(4)?,
                docstring:  row.get(5)?,
            })
        };
        let raw: Vec<ImplementationRecord> = match language {
            Some(lang) => {
                let mut stmt = self.conn.prepare(sql)?;
                let result = stmt.query_map(params![like_pattern, lang], row_mapper)?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                result
            }
            None => {
                let mut stmt = self.conn.prepare(sql)?;
                let result = stmt.query_map(params![like_pattern], row_mapper)?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                result
            }
        };
        // Post-filter: точный word-match в списке bases (разделитель — запятая)
        let filtered = raw.into_iter().filter(|r| {
            r.bases.as_deref().map(|bases| {
                bases.split(',').map(|s| s.trim()).any(|token| {
                    token == class_name
                        || token.ends_with(&format!("::{}", class_name))
                        || token.ends_with(&format!("\\{}", class_name))
                        || token.ends_with(&format!("/{}", class_name))
                })
            }).unwrap_or(false)
        }).collect();
        Ok(filtered)
    }

    /// Полный контекст символа за один вызов: definition + callers + callees +
    /// file_outline + file_imports. При неоднозначном имени — kind="ambiguous" + candidates.
    pub fn get_symbol_context(
        &self,
        name: &str,
        file_hint: Option<&str>,
        language: Option<&str>,
    ) -> Result<SymbolContext> {
        let mut functions = self.get_function_by_name(name)?;
        let mut classes = self.get_class_by_name(name)?;

        if let Some(hint) = file_hint {
            let matcher = globset::Glob::new(&normalize_glob(hint)).ok().map(|g| g.compile_matcher());
            functions.retain(|f| {
                let path = self.get_path_by_file_id(f.file_id).ok().flatten().unwrap_or_default();
                if let Some(ref m) = matcher { m.is_match(&path) } else { path.contains(hint) }
            });
            classes.retain(|c| {
                let path = self.get_path_by_file_id(c.file_id).ok().flatten().unwrap_or_default();
                if let Some(ref m) = matcher { m.is_match(&path) } else { path.contains(hint) }
            });
        }

        let total = functions.len() + classes.len();
        if total == 0 {
            return Ok(SymbolContext {
                kind: "not_found".to_string(), candidates: vec![], definition: None,
                callers: vec![], callees: vec![], file_outline: None, file_imports: vec![],
            });
        }
        if total > 1 {
            let mut candidates = Vec::new();
            for f in &functions {
                let path = self.get_path_by_file_id(f.file_id)?.unwrap_or_default();
                candidates.push(SymbolCandidate { name: f.name.clone(), kind: "function".to_string(), file_path: path, line_start: f.line_start, qualified_name: f.qualified_name.clone() });
            }
            for c in &classes {
                let path = self.get_path_by_file_id(c.file_id)?.unwrap_or_default();
                candidates.push(SymbolCandidate { name: c.name.clone(), kind: "class".to_string(), file_path: path, line_start: c.line_start, qualified_name: None });
            }
            return Ok(SymbolContext {
                kind: "ambiguous".to_string(), candidates, definition: None,
                callers: vec![], callees: vec![], file_outline: None, file_imports: vec![],
            });
        }

        let (file_id, kind, definition) = if !functions.is_empty() {
            let f = functions.into_iter().next().unwrap();
            (f.file_id, "function".to_string(), Some(serde_json::to_value(&f).unwrap_or_default()))
        } else {
            let c = classes.into_iter().next().unwrap();
            (c.file_id, "class".to_string(), Some(serde_json::to_value(&c).unwrap_or_default()))
        };

        let callers = self.get_callers(name, language)?.into_iter().take(30).map(|rec| {
            let path = self.get_path_by_file_id(rec.file_id).ok().flatten().unwrap_or_default();
            CallerInfo { caller: rec.caller, file_path: path, line: rec.line }
        }).collect();

        let callees = self.get_callees(name, language)?.into_iter().take(30).map(|rec| {
            let path = self.get_path_by_file_id(rec.file_id).ok().flatten().unwrap_or_default();
            CalleeInfo { callee: rec.callee, file_path: path, line: rec.line }
        }).collect();

        let file_path = self.get_path_by_file_id(file_id)?.unwrap_or_default();
        let file_outline = if file_path.is_empty() { None } else { self.get_file_outline(&file_path)? };
        let file_imports = self.get_imports_by_file(file_id)?;

        Ok(SymbolContext { kind, candidates: vec![], definition, callers, callees, file_outline, file_imports })
    }

    /// Найти функции без callers в индексе (потенциально мёртвый код).
    /// Исключает тесты, конструкторы, точки входа. Результат приблизителен.
    pub fn find_dead_code(
        &self,
        limit: usize,
        path_glob: Option<&str>,
        language: Option<&str>,
    ) -> Result<Vec<DeadCodeEntry>> {
        let mut conds: Vec<String> = vec![
            "f.name NOT IN (SELECT DISTINCT callee FROM calls)".to_string(),
            "f.name NOT LIKE '__init__%'".to_string(),
            "f.name NOT LIKE 'test%'".to_string(),
            "f.name NOT LIKE '%_test'".to_string(),
            "f.name NOT IN ('main','run','start','execute','handle','setup','teardown','new','init','__init__','__new__','create','destroy','delete')".to_string(),
        ];
        let mut params_dyn: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(g) = path_glob {
            conds.push("fi.path GLOB ?".to_string());
            params_dyn.push(Box::new(normalize_glob(g)));
        }
        if let Some(l) = language {
            conds.push("fi.language = ?".to_string());
            params_dyn.push(Box::new(l.to_string()));
        }
        params_dyn.push(Box::new(limit as i64));
        let sql = format!(
            "SELECT f.name, f.qualified_name, fi.path, f.line_start, f.line_end
             FROM functions f JOIN files fi ON fi.id = f.file_id
             WHERE {} ORDER BY fi.path, f.line_start LIMIT ?",
            conds.join(" AND ")
        );
        let params_refs: Vec<&dyn rusqlite::ToSql> =
            params_dyn.iter().map(|b| &**b as &dyn rusqlite::ToSql).collect();
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params_refs.as_slice(), |row| {
            Ok(DeadCodeEntry {
                name:           row.get(0)?,
                qualified_name: row.get(1)?,
                file_path:      row.get(2)?,
                line_start:     row.get::<_, i64>(3)? as usize,
                line_end:       row.get::<_, i64>(4)? as usize,
            })
        })?;
        rows.map(|r| r.map_err(Into::into)).collect()
    }
}
