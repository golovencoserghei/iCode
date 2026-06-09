/// grep_body (с/без контекста) и grep_text_filtered.
use super::{dyn_params_refs, normalize_glob, Storage};
use super::models::{ContextLine, GrepBodyMatch, GrepTextMatch};
use anyhow::{Context, Result};
use rusqlite::params;

impl Storage {
    /// Поиск подстроки или regex в телах функций и классов (без context_lines).
    pub fn grep_body(
        &self,
        pattern: Option<&str>,
        regex_pattern: Option<&str>,
        language: Option<&str>,
        limit: usize,
    ) -> Result<Vec<GrepBodyMatch>> {
        let (body_condition, body_param) = match (pattern, regex_pattern) {
            (Some(p), _) => ("body LIKE ?1".to_string(), format!("%{}%", p)),
            (_, Some(r)) => ("body REGEXP ?1".to_string(), r.to_string()),
            _ => anyhow::bail!("Необходимо указать pattern или regex"),
        };

        let sql = match language {
            Some(_) => format!(
                "SELECT fi.path, fn.name, 'function' as kind, fn.line_start, fn.line_end, fn.body
                 FROM functions fn JOIN files fi ON fi.id = fn.file_id
                 WHERE fn.{cond} AND fi.language = ?2
                 UNION ALL
                 SELECT fi.path, c.name, 'class' as kind, c.line_start, c.line_end, c.body
                 FROM classes c JOIN files fi ON fi.id = c.file_id
                 WHERE c.{cond} AND fi.language = ?2
                 ORDER BY 1, 4 LIMIT ?3",
                cond = body_condition
            ),
            None => format!(
                "SELECT fi.path, fn.name, 'function' as kind, fn.line_start, fn.line_end, fn.body
                 FROM functions fn JOIN files fi ON fi.id = fn.file_id
                 WHERE fn.{cond}
                 UNION ALL
                 SELECT fi.path, c.name, 'class' as kind, c.line_start, c.line_end, c.body
                 FROM classes c JOIN files fi ON fi.id = c.file_id
                 WHERE c.{cond}
                 ORDER BY 1, 4 LIMIT ?2",
                cond = body_condition
            ),
        };

        struct RawRow { file_path: String, name: String, kind: String, line_start: usize, line_end: usize, body: String }
        let row_mapper = |row: &rusqlite::Row| -> rusqlite::Result<RawRow> {
            Ok(RawRow {
                file_path:  row.get(0)?, name: row.get(1)?, kind: row.get(2)?,
                line_start: row.get::<_, i64>(3)? as usize,
                line_end:   row.get::<_, i64>(4)? as usize,
                body:       row.get(5)?,
            })
        };

        let mut stmt = self.conn.prepare(&sql)?;
        let raw: Vec<RawRow> = match language {
            Some(lang) => stmt.query_map(params![body_param, lang, limit as i64], row_mapper)?
                .map(|r| r.map_err(Into::into)).collect::<Result<Vec<_>>>()?,
            None => stmt.query_map(params![body_param, limit as i64], row_mapper)?
                .map(|r| r.map_err(Into::into)).collect::<Result<Vec<_>>>()?,
        };

        let compiled_re = regex_pattern
            .map(|r| regex::Regex::new(r)).transpose()
            .context("grep_body: невалидный regex")?;

        let results = raw.into_iter().map(|raw| {
            let mut all_match_lines = Vec::new();
            for (i, line) in raw.body.lines().enumerate() {
                let matched = if let Some(ref re) = compiled_re { re.is_match(line) }
                    else if let Some(p) = pattern { line.to_lowercase().contains(&p.to_lowercase()) }
                    else { false };
                if matched { all_match_lines.push(raw.line_start + i); }
            }
            let total = all_match_lines.len();
            let match_lines: Vec<usize> = all_match_lines.into_iter().take(3).collect();
            GrepBodyMatch {
                file_path: raw.file_path, name: raw.name, kind: raw.kind,
                line_start: raw.line_start, line_end: raw.line_end,
                match_lines,
                match_count: if total > 3 { Some(total) } else { None },
                context: Vec::new(),
            }
        }).collect();
        Ok(results)
    }

    /// grep_body с поддержкой context_lines, path_glob и byte-лимита.
    pub fn grep_body_with_options(
        &self,
        pattern: Option<&str>,
        regex_pattern: Option<&str>,
        language: Option<&str>,
        path_glob: Option<&str>,
        limit: usize,
        context_lines: usize,
        max_total_bytes: usize,
    ) -> Result<Vec<GrepBodyMatch>> {
        let (body_condition, body_param) = match (pattern, regex_pattern) {
            (Some(p), _) => ("body LIKE ?".to_string(), format!("%{}%", p)),
            (_, Some(r)) => ("body REGEXP ?".to_string(), r.to_string()),
            _ => anyhow::bail!("Необходимо указать pattern или regex"),
        };

        let mut extra_conds: Vec<String> = Vec::new();
        if language.is_some() { extra_conds.push("fi.language = ?".to_string()); }
        if path_glob.is_some() { extra_conds.push("fi.path GLOB ?".to_string()); }
        let extra = if extra_conds.is_empty() { String::new() }
            else { format!(" AND {}", extra_conds.join(" AND ")) };

        let lang_norm: Option<String> = language.map(|s| s.to_string());
        let glob_norm: Option<String> = path_glob.map(normalize_glob);
        let mut full_params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        for _ in 0..2 {
            full_params.push(Box::new(body_param.clone()));
            if let Some(ref l) = lang_norm { full_params.push(Box::new(l.clone())); }
            if let Some(ref g) = glob_norm  { full_params.push(Box::new(g.clone())); }
        }
        full_params.push(Box::new(limit as i64));

        let sql = format!(
            "SELECT fi.path, fn.name, 'function' as kind, fn.line_start, fn.line_end, fn.body
             FROM functions fn JOIN files fi ON fi.id = fn.file_id
             WHERE fn.{cond}{extra}
             UNION ALL
             SELECT fi.path, c.name, 'class' as kind, c.line_start, c.line_end, c.body
             FROM classes c JOIN files fi ON fi.id = c.file_id
             WHERE c.{cond}{extra}
             ORDER BY 1, 4 LIMIT ?",
            cond = body_condition, extra = extra
        );

        let mut stmt = self.conn.prepare(&sql)?;
        let params_refs = dyn_params_refs(&full_params);
        let raw: Vec<(String, String, String, i64, i64, String)> = stmt
            .query_map(params_refs.as_slice(), |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?, r.get::<_, i64>(4)?, r.get::<_, String>(5)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let compiled_re = regex_pattern.map(regex::Regex::new).transpose()
            .context("grep_body_with_options: невалидный regex")?;

        let mut total_bytes: usize = 0;
        let mut out: Vec<GrepBodyMatch> = Vec::new();
        for (file_path, name, kind, ls, le, body) in raw {
            let line_start = ls as usize;
            let line_end   = le as usize;
            let body_lines: Vec<&str> = body.lines().collect();
            let mut all_matches: Vec<usize> = Vec::new();
            for (i, line) in body_lines.iter().enumerate() {
                let matched = if let Some(ref re) = compiled_re { re.is_match(line) }
                    else if let Some(p) = pattern { line.to_lowercase().contains(&p.to_lowercase()) }
                    else { false };
                if matched { all_matches.push(i); }
            }
            let total = all_matches.len();
            let match_lines: Vec<usize> = all_matches.iter().take(3).map(|i| line_start + i).collect();
            let context = if context_lines > 0 {
                let mut included: std::collections::BTreeSet<usize> = Default::default();
                for &mi in all_matches.iter().take(3) {
                    let from = mi.saturating_sub(context_lines);
                    let to = (mi + context_lines + 1).min(body_lines.len());
                    for j in from..to { included.insert(j); }
                }
                included.into_iter().map(|j| ContextLine {
                    line: line_start + j, content: body_lines[j].to_string(),
                }).collect()
            } else { Vec::new() };

            let row_bytes = file_path.len() + name.len() + kind.len()
                + match_lines.len() * 8
                + context.iter().map(|c| c.content.len()).sum::<usize>();
            total_bytes = total_bytes.saturating_add(row_bytes);
            if total_bytes > max_total_bytes { return Ok(out); }
            out.push(GrepBodyMatch {
                file_path, name, kind, line_start, line_end, match_lines,
                match_count: if total > 3 { Some(total) } else { None },
                context,
            });
            if out.len() >= limit { return Ok(out); }
        }
        Ok(out)
    }

    /// Regex-поиск по содержимому text-файлов.
    pub fn grep_text_filtered(
        &self,
        regex_pattern: &str,
        path_glob: Option<&str>,
        language: Option<&str>,
        limit: usize,
        context_lines: usize,
        max_total_bytes: usize,
    ) -> Result<Vec<GrepTextMatch>> {
        let compiled = regex::Regex::new(regex_pattern).context("grep_text: невалидный regex")?;

        let mut conds: Vec<String> = vec!["tf.content REGEXP ?".to_string()];
        let mut params_dyn: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(regex_pattern.to_string())];
        if let Some(g) = path_glob {
            conds.push("fi.path GLOB ?".to_string());
            params_dyn.push(Box::new(normalize_glob(g)));
        }
        if let Some(l) = language {
            conds.push("fi.language = ?".to_string());
            params_dyn.push(Box::new(l.to_string()));
        }
        let sql = format!(
            "SELECT fi.path, tf.content FROM text_files tf JOIN files fi ON fi.id = tf.file_id
             WHERE {} ORDER BY fi.path",
            conds.join(" AND ")
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let params_refs = dyn_params_refs(&params_dyn);
        let candidate_rows: Vec<(String, String)> = stmt
            .query_map(params_refs.as_slice(), |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let mut results: Vec<GrepTextMatch> = Vec::new();
        let mut total_bytes: usize = 0;
        for (path, content) in candidate_rows {
            let lines: Vec<&str> = content.lines().collect();
            for (i, line) in lines.iter().enumerate() {
                if !compiled.is_match(line) { continue; }
                let context = if context_lines > 0 {
                    let from = i.saturating_sub(context_lines);
                    let to = (i + context_lines + 1).min(lines.len());
                    (from..to).map(|j| ContextLine { line: j + 1, content: lines[j].to_string() }).collect()
                } else { Vec::new() };
                let row_bytes = line.len() + context.iter().map(|c| c.content.len()).sum::<usize>() + path.len();
                total_bytes = total_bytes.saturating_add(row_bytes);
                if total_bytes > max_total_bytes { return Ok(results); }
                results.push(GrepTextMatch { path: path.clone(), line: i + 1, content: line.to_string(), context });
                if results.len() >= limit { return Ok(results); }
            }
        }
        Ok(results)
    }
}
