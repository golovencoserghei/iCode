/// stat_file_meta, list_files_filtered, read_file_text (Phase 1, v0.7.0).
use super::{normalize_glob, slice_with_caps, Storage};
use super::models::*;
use anyhow::{Context, Result};
use rusqlite::{params, OptionalExtension};

impl Storage {
    /// Метаданные одного файла из таблицы `files`. `exists=false` если не индексирован.
    pub fn stat_file_meta(&self, path: &str) -> Result<StatFileResult> {
        let row: Option<(String, String, i64, String, Option<i64>, Option<i64>, String)> = self
            .conn
            .query_row(
                "SELECT language, content_hash, lines_total, indexed_at, mtime, file_size, path
                 FROM files WHERE path = ?1",
                params![path],
                |r| Ok((
                    r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?,
                    r.get::<_, String>(3)?, r.get::<_, Option<i64>>(4)?,
                    r.get::<_, Option<i64>>(5)?, r.get::<_, String>(6)?,
                )),
            )
            .optional()
            .context("stat_file_meta: ошибка SELECT files")?;

        let Some((language, hash, lines_total, indexed_at, mtime, size, path_db)) = row else {
            return Ok(StatFileResult {
                exists: false, path: path.to_string(),
                language: None, size: None, mtime: None, lines_total: None,
                content_hash: None, indexed_at: None, category: None, oversize: None,
            });
        };

        let has_text: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM text_files tf JOIN files fi ON fi.id = tf.file_id WHERE fi.path = ?1",
            params![path_db], |r| r.get(0),
        ).context("stat_file_meta: проверка text_files")?;
        let category = if has_text > 0 { "text" } else { "code" };

        let oversize_opt = if category == "code" {
            self.conn.query_row(
                "SELECT oversize FROM file_contents fc JOIN files fi ON fi.id = fc.file_id WHERE fi.path = ?1",
                params![path_db], |r| r.get::<_, i64>(0),
            ).optional().context("stat_file_meta: проверка file_contents")?.map(|i| i != 0)
        } else { None };

        Ok(StatFileResult {
            exists: true, path: path_db, language: Some(language), size, mtime,
            lines_total: Some(lines_total as usize), content_hash: Some(hash),
            indexed_at: Some(indexed_at), category: Some(category.to_string()),
            oversize: oversize_opt,
        })
    }

    /// Список файлов с опциональными фильтрами (glob, path_prefix, language).
    pub fn list_files_filtered(
        &self,
        pattern: Option<&str>,
        path_prefix: Option<&str>,
        language: Option<&str>,
        limit: usize,
    ) -> Result<Vec<ListedFile>> {
        let mut conds: Vec<String> = Vec::new();
        let mut params_dyn: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(g) = pattern {
            conds.push("path GLOB ?".to_string());
            params_dyn.push(Box::new(normalize_glob(g)));
        }
        if let Some(p) = path_prefix {
            conds.push("path LIKE ?".to_string());
            let escaped = p.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
            params_dyn.push(Box::new(format!("{}%", escaped)));
        }
        if let Some(l) = language {
            conds.push("language = ?".to_string());
            params_dyn.push(Box::new(l.to_string()));
        }
        let where_clause = if conds.is_empty() { String::new() }
            else { format!("WHERE {}", conds.join(" AND ")) };
        let sql = format!(
            "SELECT path, language, lines_total, file_size, mtime FROM files {} ORDER BY path LIMIT ?",
            where_clause
        );
        params_dyn.push(Box::new(limit as i64));
        let params_refs: Vec<&dyn rusqlite::ToSql> =
            params_dyn.iter().map(|b| &**b as &dyn rusqlite::ToSql).collect();
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params_refs.as_slice(), |row| {
            Ok(ListedFile {
                path: row.get::<_, String>(0)?, language: row.get::<_, String>(1)?,
                lines_total: row.get::<_, i64>(2)? as usize,
                size: row.get::<_, Option<i64>>(3)?, mtime: row.get::<_, Option<i64>>(4)?,
            })
        })?;
        rows.map(|r| r.map_err(Into::into)).collect()
    }

    /// Прочитать содержимое файла из индекса. Поддерживает text-файлы и code-файлы (Phase 2).
    pub fn read_file_text(
        &self,
        path: &str,
        line_start: Option<usize>,
        line_end: Option<usize>,
        soft_cap_lines: usize,
        soft_cap_bytes: usize,
        hard_cap_bytes: usize,
        size_limit_bytes: Option<i64>,
    ) -> Result<Option<ReadFileResult>> {
        let meta: Option<(i64, i64, String, Option<i64>)> = self.conn
            .query_row(
                "SELECT id, lines_total, indexed_at, file_size FROM files WHERE path = ?1",
                params![path],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?, r.get::<_, Option<i64>>(3)?)),
            )
            .optional()
            .context("read_file_text: ошибка SELECT files")?;

        let Some((file_id, lines_total_i, indexed_at, file_size)) = meta else { return Ok(None); };
        let lines_total = lines_total_i as usize;

        let content_opt: Option<String> = self.conn
            .query_row("SELECT content FROM text_files WHERE file_id = ?1", params![file_id], |r| r.get(0))
            .optional()
            .context("read_file_text: ошибка SELECT text_files")?;

        if let Some(content) = content_opt {
            let (sliced, lines_returned, truncated) =
                slice_with_caps(&content, line_start, line_end, soft_cap_lines, soft_cap_bytes, hard_cap_bytes)?;
            return Ok(Some(ReadFileResult {
                content: sliced, lines_returned, lines_total, truncated, indexed_at,
                category: "text".to_string(), oversize: false, file_size, size_limit: None, hint: None,
            }));
        }

        match self.read_file_content(file_id)? {
            None => Ok(Some(ReadFileResult {
                content: String::new(), lines_returned: 0, lines_total, truncated: false, indexed_at,
                category: "code".to_string(), oversize: false, file_size, size_limit: None,
                hint: Some("Content code-файла ещё не наполнен (backfill в процессе после v0.8.0). \
                             Используйте get_function/get_class/grep_body для целевого чтения.".to_string()),
            })),
            Some((None, true)) => {
                let hint = match (file_size, size_limit_bytes) {
                    (Some(fs), Some(lim)) => format!(
                        "Файл превышает лимит сохранения content ({} байт > {} байт). \
                         Используйте get_function/get_class/grep_body, либо увеличьте \
                         `[indexer].max_code_file_size_bytes` в daemon.toml.", fs, lim
                    ),
                    _ => "Файл oversize: content не сохранён в индексе. Используйте get_function/get_class/grep_body.".to_string(),
                };
                Ok(Some(ReadFileResult {
                    content: String::new(), lines_returned: 0, lines_total, truncated: false, indexed_at,
                    category: "code".to_string(), oversize: true, file_size,
                    size_limit: size_limit_bytes, hint: Some(hint),
                }))
            }
            Some((Some(content), _)) => {
                let (sliced, lines_returned, truncated) =
                    slice_with_caps(&content, line_start, line_end, soft_cap_lines, soft_cap_bytes, hard_cap_bytes)?;
                Ok(Some(ReadFileResult {
                    content: sliced, lines_returned, lines_total, truncated, indexed_at,
                    category: "code".to_string(), oversize: false, file_size, size_limit: None, hint: None,
                }))
            }
            Some((None, false)) => Ok(Some(ReadFileResult {
                content: String::new(), lines_returned: 0, lines_total, truncated: false, indexed_at,
                category: "code".to_string(), oversize: false, file_size, size_limit: None,
                hint: Some("Битая запись file_contents (blob=NULL без oversize). Перезапустите индексацию репо.".to_string()),
            })),
        }
    }
}
