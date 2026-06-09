/// Phase 2 (v0.8.0): хранение и поиск содержимого code-файлов с zstd-сжатием.
///
/// Для text-файлов content живёт в `text_files.content` (без сжатия — нужен для FTS5).
/// Для code-файлов — в `file_contents.content_blob` с zstd-сжатием.
/// Файлы крупнее лимита получают `oversize=1`, `content_blob=NULL`.
use super::{dyn_params_refs, normalize_glob, Storage};
use super::models::GrepTextMatch;
use anyhow::{Context, Result};
use rusqlite::{params, OptionalExtension};

impl Storage {
    const FILE_CONTENTS_ZSTD_LEVEL: i32 = 3;

    /// 256 МБ — защита от zstd-bomb: вредоносный blob не аллоцирует произвольно много RAM.
    const FILE_CONTENTS_MAX_DECOMPRESSED_BYTES: usize = 256 * 1024 * 1024;

    /// Безопасный zstd-decode с лимитом на размер выходного буфера.
    pub(super) fn decode_zstd_safe(blob: &[u8]) -> Result<Vec<u8>> {
        use std::io::Read;
        let mut decoder = zstd::stream::read::Decoder::new(blob)
            .context("decode_zstd_safe: открыть zstd-decoder")?;
        let mut out = Vec::new();
        let limit = Self::FILE_CONTENTS_MAX_DECOMPRESSED_BYTES as u64;
        let read = (&mut decoder).take(limit + 1).read_to_end(&mut out)
            .context("decode_zstd_safe: чтение разжатого потока")?;
        if read as u64 > limit {
            anyhow::bail!(
                "decode_zstd_safe: разжатый размер превысил лимит {} байт (zstd-bomb?)",
                Self::FILE_CONTENTS_MAX_DECOMPRESSED_BYTES
            );
        }
        Ok(out)
    }

    /// Сохранить content code-файла с zstd-сжатием. Idempotent через INSERT OR REPLACE.
    pub fn upsert_file_content(&self, file_id: i64, content: &str, max_size_bytes: usize) -> Result<()> {
        if content.len() > max_size_bytes {
            self.conn
                .execute(
                    "INSERT OR REPLACE INTO file_contents (file_id, content_blob, oversize)
                     VALUES (?1, NULL, 1)",
                    params![file_id],
                )
                .context("upsert_file_content: INSERT oversize")?;
            return Ok(());
        }
        let blob = zstd::encode_all(content.as_bytes(), Self::FILE_CONTENTS_ZSTD_LEVEL)
            .context("upsert_file_content: zstd encode")?;
        self.conn
            .execute(
                "INSERT OR REPLACE INTO file_contents (file_id, content_blob, oversize)
                 VALUES (?1, ?2, 0)",
                params![file_id, blob],
            )
            .context("upsert_file_content: INSERT blob")?;
        Ok(())
    }

    pub fn get_file_id_by_path(&self, path: &str) -> Result<Option<i64>> {
        self.conn
            .query_row("SELECT id FROM files WHERE path = ?1", params![path], |r| r.get::<_, i64>(0))
            .optional()
            .context("get_file_id_by_path")
    }

    /// Список code-файлов без записи в `file_contents` — кандидаты для backfill.
    pub fn list_code_files_without_content(&self) -> Result<Vec<(i64, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT fi.id, fi.path FROM files fi
             WHERE fi.id NOT IN (SELECT file_id FROM file_contents)
               AND fi.id NOT IN (SELECT file_id FROM text_files)
             ORDER BY fi.path",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
        rows.map(|r| r.map_err(Into::into)).collect()
    }

    pub fn has_text_file(&self, file_id: i64) -> Result<bool> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM text_files WHERE file_id = ?1",
            params![file_id],
            |r| r.get(0),
        ).context("has_text_file")?;
        Ok(n > 0)
    }

    pub fn delete_file_content(&self, file_id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM file_contents WHERE file_id = ?1", params![file_id])
            .context("delete_file_content")?;
        Ok(())
    }

    pub fn has_file_content(&self, file_id: i64) -> Result<bool> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM file_contents WHERE file_id = ?1",
            params![file_id],
            |r| r.get(0),
        ).context("has_file_content")?;
        Ok(n > 0)
    }

    /// Прочитать content code-файла и oversize-флаг.
    /// `None` — записи нет; `Some((Some(text), false))` — нормальная; `Some((None, true))` — oversize.
    pub fn read_file_content(&self, file_id: i64) -> Result<Option<(Option<String>, bool)>> {
        let row: Option<(Option<Vec<u8>>, i64)> = self.conn
            .query_row(
                "SELECT content_blob, oversize FROM file_contents WHERE file_id = ?1",
                params![file_id],
                |r| Ok((r.get::<_, Option<Vec<u8>>>(0)?, r.get::<_, i64>(1)?)),
            )
            .optional()
            .context("read_file_content: SELECT")?;
        let Some((blob_opt, oversize_int)) = row else { return Ok(None); };
        let oversize = oversize_int != 0;
        let content_opt = match blob_opt {
            None => None,
            Some(blob) => {
                let bytes = Self::decode_zstd_safe(&blob).context("read_file_content: zstd decode")?;
                let text = String::from_utf8(bytes).context("read_file_content: UTF-8 из zstd-blob")?;
                Some(text)
            }
        };
        Ok(Some((content_opt, oversize)))
    }

    /// Regex-поиск по содержимому code-файлов через `file_contents` (zstd).
    /// SQL делает pre-filter по path_glob/language; regex применяется к разжатому тексту.
    pub fn grep_code_filtered(
        &self,
        regex_pattern: &str,
        path_glob: Option<&str>,
        language: Option<&str>,
        limit: usize,
        context_lines: usize,
        max_total_bytes: usize,
    ) -> Result<Vec<GrepTextMatch>> {
        use super::models::ContextLine;
        if path_glob.is_none() && language.is_none() {
            tracing::warn!(
                pattern = regex_pattern,
                "grep_code: full-scan без path_glob и language — zstd-decode всех code-файлов. \
                 Укажите path_glob или language для ускорения."
            );
        }
        let compiled = regex::Regex::new(regex_pattern).context("grep_code: невалидный regex")?;

        let mut conds: Vec<String> = vec![
            "fc.oversize = 0".to_string(),
            "fc.content_blob IS NOT NULL".to_string(),
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
        let sql = format!(
            "SELECT fi.path, fc.content_blob FROM file_contents fc
             JOIN files fi ON fi.id = fc.file_id
             WHERE {} ORDER BY fi.path",
            conds.join(" AND ")
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let params_refs = dyn_params_refs(&params_dyn);
        let candidate_rows: Vec<(String, Vec<u8>)> = stmt
            .query_map(params_refs.as_slice(), |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let mut results: Vec<GrepTextMatch> = Vec::new();
        let mut total_bytes: usize = 0;
        for (path, blob) in candidate_rows {
            let bytes = match Self::decode_zstd_safe(&blob) { Ok(b) => b, Err(_) => continue };
            let content = match String::from_utf8(bytes) { Ok(s) => s, Err(_) => continue };
            if !compiled.is_match(&content) { continue; }
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
