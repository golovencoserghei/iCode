use super::{schema, Storage};
use super::models::*;
use anyhow::{Context, Result};
use rusqlite::params;

impl Storage {
    // ── Files ────────────────────────────────────────────────────────────────

    /// Вставить или обновить запись файла; возвращает id строки
    pub fn upsert_file(&self, record: &FileRecord) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO files (path, content_hash, ast_hash, language, lines_total, indexed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(path) DO UPDATE SET
                 content_hash = excluded.content_hash,
                 ast_hash     = excluded.ast_hash,
                 language     = excluded.language,
                 lines_total  = excluded.lines_total,
                 indexed_at   = excluded.indexed_at",
            params![
                record.path,
                record.content_hash,
                record.ast_hash,
                record.language,
                record.lines_total as i64,
                record.indexed_at,
            ],
        )
        .context("upsert_file: ошибка выполнения запроса")?;

        let id: i64 = self.conn.query_row(
            "SELECT id FROM files WHERE path = ?1",
            params![record.path],
            |row| row.get(0),
        )?;
        Ok(id)
    }

    pub fn get_path_by_file_id(&self, id: i64) -> Result<Option<String>> {
        let r: Option<String> = self
            .conn
            .query_row(
                "SELECT path FROM files WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .optional()
            .context("get_path_by_file_id")?;
        Ok(r)
    }

    pub fn get_file_by_path(&self, path: &str) -> Result<Option<FileRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, path, content_hash, ast_hash, language, lines_total, indexed_at, mtime, file_size
             FROM files WHERE path = ?1",
        )?;
        let result = stmt.query_row(params![path], super::row_to_file);
        match result {
            Ok(r) => Ok(Some(r)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn get_all_files(&self) -> Result<Vec<FileRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, path, content_hash, ast_hash, language, lines_total, indexed_at, mtime, file_size
             FROM files ORDER BY path",
        )?;
        let rows = stmt.query_map([], super::row_to_file)?;
        rows.map(|r| r.map_err(Into::into)).collect()
    }

    pub fn update_file_metadata(&self, path: &str, mtime: i64, file_size: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE files SET mtime = ?1, file_size = ?2 WHERE path = ?3",
            params![mtime, file_size, path],
        )?;
        Ok(())
    }

    pub fn delete_file(&self, file_id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM files WHERE id = ?1", params![file_id])
            .context("delete_file: ошибка удаления")?;
        Ok(())
    }

    // ── Functions ────────────────────────────────────────────────────────────

    pub fn insert_functions(&self, records: &[FunctionRecord]) -> Result<()> {
        let mut stmt = self.conn.prepare(
            "INSERT INTO functions
                 (file_id, name, qualified_name, line_start, line_end,
                  args, return_type, docstring, body, is_async, node_hash,
                  override_type, override_target)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
        )?;
        for r in records {
            stmt.execute(params![
                r.file_id, r.name, r.qualified_name,
                r.line_start as i64, r.line_end as i64,
                r.args, r.return_type, r.docstring, r.body,
                r.is_async as i32, r.node_hash,
                r.override_type, r.override_target,
            ])
            .context("insert_functions: ошибка вставки строки")?;
        }
        Ok(())
    }

    pub fn delete_functions_by_file(&self, file_id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM functions WHERE file_id = ?1", params![file_id])
            .context("delete_functions_by_file")?;
        Ok(())
    }

    // ── Classes ──────────────────────────────────────────────────────────────

    pub fn insert_classes(&self, records: &[ClassRecord]) -> Result<()> {
        let mut stmt = self.conn.prepare(
            "INSERT INTO classes
                 (file_id, name, line_start, line_end, bases, docstring, body, node_hash)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
        )?;
        for r in records {
            stmt.execute(params![
                r.file_id, r.name,
                r.line_start as i64, r.line_end as i64,
                r.bases, r.docstring, r.body, r.node_hash,
            ])
            .context("insert_classes: ошибка вставки строки")?;
        }
        Ok(())
    }

    pub fn delete_classes_by_file(&self, file_id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM classes WHERE file_id = ?1", params![file_id])
            .context("delete_classes_by_file")?;
        Ok(())
    }

    // ── Imports ──────────────────────────────────────────────────────────────

    pub fn insert_imports(&self, records: &[ImportRecord]) -> Result<()> {
        let mut stmt = self.conn.prepare(
            "INSERT INTO imports (file_id, module, name, alias, line, kind)
             VALUES (?1,?2,?3,?4,?5,?6)",
        )?;
        for r in records {
            stmt.execute(params![
                r.file_id, r.module, r.name, r.alias, r.line as i64, r.kind,
            ])
            .context("insert_imports: ошибка вставки строки")?;
        }
        Ok(())
    }

    pub fn delete_imports_by_file(&self, file_id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM imports WHERE file_id = ?1", params![file_id])
            .context("delete_imports_by_file")?;
        Ok(())
    }

    // ── Calls ────────────────────────────────────────────────────────────────

    pub fn insert_calls(&self, records: &[CallRecord]) -> Result<()> {
        let mut stmt = self.conn.prepare(
            "INSERT INTO calls (file_id, caller, callee, line) VALUES (?1,?2,?3,?4)",
        )?;
        for r in records {
            stmt.execute(params![r.file_id, r.caller, r.callee, r.line as i64])
                .context("insert_calls: ошибка вставки строки")?;
        }
        Ok(())
    }

    pub fn delete_calls_by_file(&self, file_id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM calls WHERE file_id = ?1", params![file_id])
            .context("delete_calls_by_file")?;
        Ok(())
    }

    // ── Variables ────────────────────────────────────────────────────────────

    pub fn insert_variables(&self, records: &[VariableRecord]) -> Result<()> {
        let mut stmt = self.conn.prepare(
            "INSERT INTO variables (file_id, name, value, line) VALUES (?1,?2,?3,?4)",
        )?;
        for r in records {
            stmt.execute(params![r.file_id, r.name, r.value, r.line as i64])
                .context("insert_variables: ошибка вставки строки")?;
        }
        Ok(())
    }

    pub fn delete_variables_by_file(&self, file_id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM variables WHERE file_id = ?1", params![file_id])
            .context("delete_variables_by_file")?;
        Ok(())
    }

    // ── Text files ───────────────────────────────────────────────────────────

    pub fn insert_text_file(&self, record: &TextFileRecord) -> Result<()> {
        self.conn.execute(
            "INSERT INTO text_files (file_id, content) VALUES (?1, ?2)",
            params![record.file_id, record.content],
        )
        .context("insert_text_file")?;
        Ok(())
    }

    pub fn delete_text_file_by_file(&self, file_id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM text_files WHERE file_id = ?1", params![file_id])
            .context("delete_text_file_by_file")?;
        Ok(())
    }

    // ── Bulk-load ────────────────────────────────────────────────────────────

    /// Инициализировать БД для массовой загрузки: только таблицы, без индексов.
    pub fn initialize_for_bulk(&self) -> Result<()> {
        schema::initialize_tables_only(&self.conn)
            .context("initialize_for_bulk: ошибка создания таблиц без индексов")?;
        Ok(())
    }

    /// Подготовить БД к массовой загрузке: удалить индексы и FTS-триггеры.
    pub fn prepare_bulk_load(&self) -> Result<()> {
        schema::drop_indexes_and_triggers(&self.conn)
            .context("prepare_bulk_load: ошибка удаления индексов и триггеров")?;
        Ok(())
    }

    /// Завершить массовую загрузку: пересоздать индексы, триггеры и перестроить FTS.
    pub fn finish_bulk_load(&self) -> Result<()> {
        schema::rebuild_indexes_and_triggers(&self.conn)
            .context("finish_bulk_load: ошибка пересоздания индексов и триггеров")?;
        Ok(())
    }

    // ── Транзакции ───────────────────────────────────────────────────────────

    pub fn execute_in_transaction<F, T>(&mut self, f: F) -> Result<T>
    where
        F: FnOnce(&rusqlite::Transaction) -> Result<T>,
    {
        let tx = self.conn.transaction().context("Не удалось начать транзакцию")?;
        let result = f(&tx)?;
        tx.commit().context("Не удалось закоммитить транзакцию")?;
        Ok(result)
    }

    pub fn begin_batch(&self) -> Result<()> {
        self.conn
            .execute_batch("BEGIN TRANSACTION")
            .context("begin_batch: не удалось начать транзакцию")?;
        Ok(())
    }

    pub fn commit_batch(&self) -> Result<()> {
        self.conn
            .execute_batch("COMMIT")
            .context("commit_batch: не удалось закоммитить транзакцию")?;
        Ok(())
    }
}

use rusqlite::OptionalExtension;
