//! Архитектурная аналитика «за один дешёвый вызов»: карта репозитория,
//! функции-сложности (hotspots), видимость слепых зон (parse errors).
//!
//! Цель — дать агенту глубокую ментальную модель проекта в сотнях токенов
//! вместо десятков grep/read: где сосредоточена сложность, что вызывается чаще
//! всего, откуда начинается выполнение, что вообще НЕ проиндексировано.

use std::collections::BTreeMap;

use anyhow::Result;
use rusqlite::params;

use super::{normalize_glob, Storage};
use super::models::*;

/// Ключ модуля: первые ≤3 компонента пути (без имени файла).
fn dir_key(path: &str) -> String {
    let parts: Vec<&str> = path.split('/').collect();
    if parts.len() <= 1 {
        return ".".to_string();
    }
    let take = (parts.len() - 1).min(3);
    parts[..take].join("/")
}

impl Storage {
    fn count_of(&self, sql: &str) -> usize {
        self.conn
            .query_row(sql, [], |r| r.get::<_, i64>(0))
            .map(|n| n as usize)
            .unwrap_or(0)
    }

    fn distinct_count(&self, sql: &str, key: &str) -> usize {
        self.conn
            .query_row(sql, params![key], |r| r.get::<_, i64>(0))
            .map(|n| n as usize)
            .unwrap_or(0)
    }

    /// Число файлов с ошибкой парсинга (слепые зоны индекса).
    pub fn count_parse_errors(&self) -> usize {
        self.count_of("SELECT COUNT(*) FROM parse_errors")
    }

    /// Пути всех файлов с ошибкой парсинга (для прунинга удалённых с диска).
    pub fn parse_error_paths(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare("SELECT path FROM parse_errors")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.map(|r| r.map_err(Into::into)).collect()
    }

    /// Список файлов с ошибками парсинга (path, error).
    pub fn get_parse_errors(&self, limit: usize) -> Result<Vec<(String, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT path, error FROM parse_errors ORDER BY path LIMIT ?1")?;
        let rows = stmt.query_map(params![limit as i64], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        rows.map(|r| r.map_err(Into::into)).collect()
    }

    /// Функции, отранжированные по сложности (длина + fan-out + fan-in).
    /// Сначала берём пул крупнейших по длине, затем досчитываем связность —
    /// дёшево даже на больших репо (точечные индексные lookup'ы по calls).
    ///
    /// Замечание: fan_out/callers считаются по ГОЛОМУ имени (`calls.caller`/
    /// `calls.callee`), поэтому одноимённые методы разных классов (index/store/
    /// handle в контроллерах) схлопываются в одну корзину — это эвристический
    /// proxy сложности, а не точная метрика.
    pub fn find_complex_functions(
        &self,
        limit: usize,
        path_glob: Option<&str>,
        language: Option<&str>,
    ) -> Result<Vec<ComplexFunction>> {
        let mut conds: Vec<String> = vec!["f.name != '<module>'".to_string()];
        let mut dyn_params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(g) = path_glob {
            conds.push("fi.path GLOB ?".to_string());
            dyn_params.push(Box::new(normalize_glob(g)));
        }
        if let Some(l) = language {
            conds.push("fi.language = ?".to_string());
            dyn_params.push(Box::new(l.to_string()));
        }
        // Пул кандидатов по длине — с запасом, дальше переранжируем по полному score.
        let pool = (limit.saturating_mul(5)).max(60) as i64;
        dyn_params.push(Box::new(pool));
        let sql = format!(
            "SELECT f.name, f.qualified_name, fi.path, f.line_start, f.line_end
             FROM functions f JOIN files fi ON fi.id = f.file_id
             WHERE {} ORDER BY (f.line_end - f.line_start) DESC LIMIT ?",
            conds.join(" AND ")
        );
        let refs: Vec<&dyn rusqlite::ToSql> = dyn_params.iter().map(|b| &**b as &dyn rusqlite::ToSql).collect();
        let mut stmt = self.conn.prepare(&sql)?;
        let mut pool_rows: Vec<ComplexFunction> = stmt
            .query_map(refs.as_slice(), |row| {
                let ls = row.get::<_, i64>(3)? as usize;
                let le = row.get::<_, i64>(4)? as usize;
                Ok(ComplexFunction {
                    name: row.get(0)?,
                    qualified_name: row.get(1)?,
                    file_path: row.get(2)?,
                    line_start: ls,
                    line_end: le,
                    span: le.saturating_sub(ls),
                    fan_out: 0,
                    callers: 0,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        for f in &mut pool_rows {
            f.fan_out = self.distinct_count(
                "SELECT COUNT(DISTINCT callee) FROM calls WHERE caller = ?1",
                &f.name,
            );
            f.callers = self.distinct_count(
                "SELECT COUNT(DISTINCT caller) FROM calls WHERE callee = ?1",
                &f.name,
            );
        }
        // Score сложности: длина + ответственность (fan-out) + связанность (fan-in).
        pool_rows.sort_by_key(|f| std::cmp::Reverse(f.span + f.fan_out * 5 + f.callers * 2));
        pool_rows.truncate(limit);
        Ok(pool_rows)
    }

    /// Архитектурная карта репозитория за один вызов.
    pub fn repo_map(&self, top: usize) -> Result<RepoMap> {
        let top = top.clamp(3, 50);

        let files = self.count_of("SELECT COUNT(*) FROM files");
        let functions = self.count_of("SELECT COUNT(*) FROM functions");
        let classes = self.count_of("SELECT COUNT(*) FROM classes");
        let calls = self.count_of("SELECT COUNT(*) FROM calls");
        let imports = self.count_of("SELECT COUNT(*) FROM imports");

        // Языки.
        let mut langs = self
            .conn
            .prepare("SELECT language, COUNT(*) FROM files GROUP BY language ORDER BY 2 DESC")?;
        let languages: Vec<LangStat> = langs
            .query_map([], |r| {
                Ok(LangStat { language: r.get(0)?, files: r.get::<_, i64>(1)? as usize })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        // Модули (директории): агрегируем функции/классы/файлы по dir-префиксу.
        let mut mods: BTreeMap<String, (usize, usize, usize)> = BTreeMap::new();
        {
            let mut s = self.conn.prepare(
                "SELECT fi.path, COUNT(*) FROM functions f JOIN files fi ON fi.id=f.file_id GROUP BY fi.path",
            )?;
            for row in s.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as usize)))? {
                let (p, n) = row?;
                mods.entry(dir_key(&p)).or_default().1 += n;
            }
        }
        {
            let mut s = self.conn.prepare(
                "SELECT fi.path, COUNT(*) FROM classes c JOIN files fi ON fi.id=c.file_id GROUP BY fi.path",
            )?;
            for row in s.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as usize)))? {
                let (p, n) = row?;
                mods.entry(dir_key(&p)).or_default().2 += n;
            }
        }
        {
            let mut s = self.conn.prepare("SELECT path FROM files")?;
            for row in s.query_map([], |r| r.get::<_, String>(0))? {
                mods.entry(dir_key(&row?)).or_default().0 += 1;
            }
        }
        let mut modules: Vec<ModuleStat> = mods
            .into_iter()
            .map(|(dir, (f, fn_, cl))| ModuleStat { dir, files: f, functions: fn_, classes: cl })
            .collect();
        modules.sort_by_key(|m| std::cmp::Reverse(m.functions + m.classes));
        modules.truncate(top);

        // Сложность.
        let complex_functions = self.find_complex_functions(top, None, None)?;

        // Горячие точки call-графа: только ПРОЕКТНЫЕ функции (callee есть среди
        // определений) — отсекает stdlib/builtin-шум (Some/Ok/Vec::new и т.п.).
        let mut hs = self.conn.prepare(
            "SELECT callee, COUNT(*) n FROM calls
             WHERE callee IN (SELECT name FROM functions)
             GROUP BY callee ORDER BY n DESC LIMIT ?1",
        )?;
        let mut call_hotspots: Vec<CallHotspot> = hs
            .query_map(params![top as i64], |r| {
                Ok(CallHotspot { name: r.get(0)?, calls: r.get::<_, i64>(1)? as usize, file_path: None })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for h in &mut call_hotspots {
            h.file_path = self
                .conn
                .query_row(
                    "SELECT fi.path FROM functions f JOIN files fi ON fi.id=f.file_id WHERE f.name=?1 LIMIT 1",
                    params![h.name],
                    |r| r.get::<_, String>(0),
                )
                .ok();
        }

        // Точки входа: функции без вызывателей, но с заметным fan-out (корни/оркестраторы).
        let mut ep = self.conn.prepare(
            "SELECT f.name, f.qualified_name, fi.path, f.line_start, f.line_end
             FROM functions f JOIN files fi ON fi.id=f.file_id
             WHERE f.name != '<module>'
               AND f.name NOT IN (SELECT DISTINCT callee FROM calls)
             ORDER BY (f.line_end - f.line_start) DESC LIMIT ?1",
        )?;
        let mut entry_pool: Vec<ComplexFunction> = ep
            .query_map(params![(top * 5).max(40) as i64], |row| {
                let ls = row.get::<_, i64>(3)? as usize;
                let le = row.get::<_, i64>(4)? as usize;
                Ok(ComplexFunction {
                    name: row.get(0)?,
                    qualified_name: row.get(1)?,
                    file_path: row.get(2)?,
                    line_start: ls,
                    line_end: le,
                    span: le.saturating_sub(ls),
                    fan_out: 0,
                    callers: 0,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for f in &mut entry_pool {
            f.fan_out = self.distinct_count(
                "SELECT COUNT(DISTINCT callee) FROM calls WHERE caller = ?1",
                &f.name,
            );
        }
        // Реальные оркестраторы что-то вызывают; сортируем по fan-out.
        entry_pool.retain(|f| f.fan_out > 0);
        entry_pool.sort_by_key(|f| std::cmp::Reverse(f.fan_out));
        entry_pool.truncate(top);

        Ok(RepoMap {
            files,
            functions,
            classes,
            calls,
            imports,
            languages,
            modules,
            complex_functions,
            call_hotspots,
            entry_points: entry_pool,
            parse_errors: self.count_parse_errors(),
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::indexer::Indexer;
    use crate::storage::Storage;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn repo_map_and_complexity_on_small_project() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("app")).unwrap();
        // «Толстая» оркестрирующая функция + мелкие helpers.
        fs::write(
            tmp.path().join("app/main.py"),
            "def helper_a():\n    return 1\n\ndef helper_b():\n    return 2\n\n\
             def orchestrate():\n    helper_a()\n    helper_b()\n    helper_a()\n    return helper_b()\n",
        ).unwrap();

        let mut storage = Storage::open_in_memory().unwrap();
        Indexer::new(&mut storage).full_reindex(tmp.path(), false).unwrap();

        let map = storage.repo_map(10).unwrap();
        assert!(map.functions >= 3);
        assert!(!map.languages.is_empty());
        assert!(!map.modules.is_empty());

        // helper_a/helper_b — проектные функции, вызываются → попадают в hotspots.
        assert!(map.call_hotspots.iter().any(|h| h.name == "helper_a"));
        // orchestrate никто не вызывает, но fan-out>0 → точка входа.
        assert!(map.entry_points.iter().any(|e| e.name == "orchestrate"));

        let complex = storage.find_complex_functions(5, None, None).unwrap();
        assert!(!complex.is_empty());
        // orchestrate имеет наибольший fan-out среди определённых.
        assert!(complex.iter().any(|c| c.name == "orchestrate" && c.fan_out >= 2));
    }
}
