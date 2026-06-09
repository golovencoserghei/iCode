use tracing::info;
/// Модуль хранилища — SQLite через rusqlite (bundled).
///
/// Реализация разбита по файлам:
///   write.rs    — INSERT / DELETE / UPDATE / bulk-load / транзакции
///   content.rs  — Phase 2: zstd-сжатое хранилище code-файлов, grep_code
///   search.rs   — FTS-поиск, точные lookup'ы, file summary/outline, stats
///   grep.rs     — grep_body, grep_body_with_options, grep_text_filtered
///   read.rs     — stat_file_meta, list_files_filtered, read_file_text
///   analysis.rs — глубокий анализ: transitive call-граф, implementations, dead code
pub mod memory;
pub mod models;
pub mod schema;

mod analysis;
mod content;
mod grep;
mod read;
mod search;
mod write;

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};
use std::path::Path;

use models::*;

fn register_regexp(conn: &Connection) -> Result<()> {
    use rusqlite::functions::FunctionFlags;
    use std::cell::RefCell;

    let cache: RefCell<Option<(String, regex::Regex)>> = RefCell::new(None);

    conn.create_scalar_function(
        "regexp",
        2,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        move |ctx| {
            let pattern: String = ctx.get(0)?;
            let text: String = ctx.get(1)?;

            let mut cached = cache.borrow_mut();
            let re = match cached.as_ref() {
                Some((p, re)) if *p == pattern => re,
                _ => {
                    let new_re = regex::Regex::new(&pattern)
                        .map_err(|e| rusqlite::Error::InvalidParameterName(e.to_string()))?;
                    *cached = Some((pattern, new_re));
                    &cached.as_ref().unwrap().1
                }
            };
            Ok(re.is_match(&text))
        },
    )
    .context("Не удалось зарегистрировать REGEXP")?;
    Ok(())
}

/// Основная структура хранилища — обёртка над SQLite-соединением.
pub struct Storage {
    conn: Connection,
}

impl Storage {
    // ── Конструкторы ────────────────────────────────────────────────────────

    pub fn open_file(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("Не удалось открыть БД: {}", path.display()))?;
        schema::initialize(&conn).context("Ошибка инициализации схемы БД")?;
        register_regexp(&conn)?;
        Ok(Self { conn })
    }

    pub fn apply_schema_extensions(&self, extensions: &[&str]) -> Result<()> {
        for ddl in extensions {
            self.conn
                .execute_batch(ddl)
                .with_context(|| format!("DDL-расширение схемы упало: {}", ddl))?;
        }
        Ok(())
    }

    pub fn conn(&self) -> &Connection { &self.conn }

    pub fn open_file_readonly(path: &Path) -> Result<Self> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_URI,
        )
        .with_context(|| format!("Не удалось открыть БД (readonly): {}", path.display()))?;
        schema::initialize_readonly(&conn).context("Ошибка инициализации readonly-схемы")?;
        register_regexp(&conn)?;
        Ok(Self { conn })
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().context("Не удалось создать in-memory БД")?;
        schema::initialize(&conn).context("Ошибка инициализации схемы in-memory БД")?;
        register_regexp(&conn)?;
        Ok(Self { conn })
    }

    pub fn open_auto(db_path: &Path, storage_config: &memory::StorageConfig) -> Result<Self> {
        let mode = memory::determine_storage_mode(storage_config, db_path);
        match mode {
            memory::StorageMode::InMemory => {
                info!("[storage] Режим: in-memory (БД загружена в RAM)");
                if db_path.exists() {
                    let disk_conn = Connection::open(db_path)
                        .with_context(|| format!("Не удалось открыть файл БД: {}", db_path.display()))?;
                    let mut memory_conn = Connection::open_in_memory()
                        .context("Не удалось создать in-memory БД")?;
                    {
                        let backup = rusqlite::backup::Backup::new(&disk_conn, &mut memory_conn)
                            .context("Не удалось инициализировать backup disk→memory")?;
                        backup
                            .run_to_completion(100, std::time::Duration::from_millis(0), None)
                            .context("Ошибка при копировании БД disk→memory")?;
                    }
                    schema::migrate_v2(&memory_conn).context("Ошибка миграции v2 (in-memory)")?;
                    schema::migrate_v3(&memory_conn).context("Ошибка миграции v3 (in-memory)")?;
                    register_regexp(&memory_conn)?;
                    Ok(Self { conn: memory_conn })
                } else {
                    Self::open_in_memory()
                }
            }
            memory::StorageMode::Disk => {
                info!("[storage] Режим: disk (WAL)");
                Self::open_file(db_path)
            }
        }
    }

    pub fn flush_to_disk(&self, db_path: &Path) -> Result<()> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Не удалось создать директорию: {}", parent.display()))?;
        }
        self.conn
            .backup(rusqlite::MAIN_DB, db_path, None)
            .with_context(|| format!("Ошибка flush_to_disk: {}", db_path.display()))?;
        Ok(())
    }

    pub fn checkpoint_truncate(&self) -> Result<(i64, i64, i64)> {
        self.conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE);", [], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .context("PRAGMA wal_checkpoint(TRUNCATE) failed")
    }
}

// ── Вспомогательные функции ───────────────────────────────────────────────────

fn sanitize_fts_query(query: &str) -> String {
    if query.contains('-') || query.contains('+') || query.contains('*') {
        format!("\"{}\"", query)
    } else {
        query.to_string()
    }
}

/// Превращает `Vec<Box<dyn ToSql>>` в слайс ссылок для `query_map` / `execute`.
pub(crate) fn dyn_params_refs<'a>(
    params: &'a [Box<dyn rusqlite::ToSql>],
) -> Vec<&'a dyn rusqlite::ToSql> {
    params.iter().map(|b| &**b as &dyn rusqlite::ToSql).collect()
}

pub(crate) fn normalize_glob(pattern: &str) -> String {
    let mut s = pattern.to_string();
    while s.contains("**") {
        s = s.replace("**", "*");
    }
    s
}

/// Нарезать содержимое файла по диапазону строк с применением soft/hard-cap.
/// Возвращает `(тело, кол-во строк, усечено?)`.
pub(crate) fn slice_with_caps(
    content: &str,
    line_start: Option<usize>,
    line_end: Option<usize>,
    soft_cap_lines: usize,
    soft_cap_bytes: usize,
    hard_cap_bytes: usize,
) -> Result<(String, usize, bool)> {
    if content.len() > hard_cap_bytes {
        anyhow::bail!(
            "Файл превышает hard-cap {} байт — слишком большой для read_file.",
            hard_cap_bytes
        );
    }
    let all_lines: Vec<&str> = content.lines().collect();
    let total = all_lines.len();

    let start_idx = line_start.map(|n| n.saturating_sub(1)).unwrap_or(0).min(total);
    let end_idx = line_end.unwrap_or(total).min(total);
    let slice_len = end_idx.saturating_sub(start_idx);

    let mut take_n = slice_len;
    if take_n > soft_cap_lines {
        take_n = soft_cap_lines;
    }
    let byte_take_n = {
        let mut acc = 0usize;
        let mut n = 0usize;
        for line in &all_lines[start_idx..start_idx + take_n] {
            acc += line.len() + 1;
            if acc > soft_cap_bytes {
                break;
            }
            n += 1;
        }
        n.max(1)
    };
    if byte_take_n < take_n {
        take_n = byte_take_n;
    }
    let truncated = take_n < slice_len;
    let body: String = all_lines[start_idx..start_idx + take_n].join("\n");
    Ok((body, take_n, truncated))
}

// ── Row-mappers (доступны дочерним модулям) ──────────────────────────────────

fn row_to_file(row: &rusqlite::Row<'_>) -> rusqlite::Result<FileRecord> {
    Ok(FileRecord {
        id:           Some(row.get(0)?),
        path:         row.get(1)?,
        content_hash: row.get(2)?,
        ast_hash:     row.get(3)?,
        language:     row.get(4)?,
        lines_total:  row.get::<_, i64>(5)? as usize,
        indexed_at:   row.get(6)?,
        mtime:        row.get(7)?,
        file_size:    row.get(8)?,
    })
}

fn row_to_function(row: &rusqlite::Row<'_>) -> rusqlite::Result<FunctionRecord> {
    Ok(FunctionRecord {
        id:              Some(row.get(0)?),
        file_id:         row.get(1)?,
        name:            row.get(2)?,
        qualified_name:  row.get(3)?,
        line_start:      row.get::<_, i64>(4)? as usize,
        line_end:        row.get::<_, i64>(5)? as usize,
        args:            row.get(6)?,
        return_type:     row.get(7)?,
        docstring:       row.get(8)?,
        body:            row.get(9)?,
        is_async:        row.get::<_, i32>(10)? != 0,
        node_hash:       row.get(11)?,
        override_type:   row.get(12).ok(),
        override_target: row.get(13).ok(),
    })
}

fn row_to_class(row: &rusqlite::Row<'_>) -> rusqlite::Result<ClassRecord> {
    Ok(ClassRecord {
        id:         Some(row.get(0)?),
        file_id:    row.get(1)?,
        name:       row.get(2)?,
        line_start: row.get::<_, i64>(3)? as usize,
        line_end:   row.get::<_, i64>(4)? as usize,
        bases:      row.get(5)?,
        docstring:  row.get(6)?,
        body:       row.get(7)?,
        node_hash:  row.get(8)?,
    })
}

fn row_to_import(row: &rusqlite::Row<'_>) -> rusqlite::Result<ImportRecord> {
    Ok(ImportRecord {
        id:      Some(row.get(0)?),
        file_id: row.get(1)?,
        module:  row.get(2)?,
        name:    row.get(3)?,
        alias:   row.get(4)?,
        line:    row.get::<_, i64>(5)? as usize,
        kind:    row.get(6)?,
    })
}

fn row_to_call(row: &rusqlite::Row<'_>) -> rusqlite::Result<CallRecord> {
    Ok(CallRecord {
        id:      Some(row.get(0)?),
        file_id: row.get(1)?,
        caller:  row.get(2)?,
        callee:  row.get(3)?,
        line:    row.get::<_, i64>(4)? as usize,
    })
}

fn row_to_variable(row: &rusqlite::Row<'_>) -> rusqlite::Result<VariableRecord> {
    Ok(VariableRecord {
        id:      Some(row.get(0)?),
        file_id: row.get(1)?,
        name:    row.get(2)?,
        value:   row.get(3)?,
        line:    row.get::<_, i64>(4)? as usize,
    })
}


#[cfg(test)]
mod tests;
