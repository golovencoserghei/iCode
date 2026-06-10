/// Модуль индексатора — обход директорий, определение типов файлов, хеширование
pub mod config;
pub mod file_types;
pub mod hasher;

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::Result;
use rayon::prelude::*;
use walkdir::WalkDir;

use crate::graph_store::{GraphCommand, GraphStoreHandle};
use crate::parser::types::ParseResult;
use tracing::{debug, info, warn};
use crate::parser::ParserRegistry;
use crate::parser::text::TextParser;
use crate::storage::models::*;
use crate::storage::Storage;
use config::IndexConfig;
use file_types::{categorize_file, FileCategory};

/// Результат одного прохода индексации
#[derive(Debug, Default)]
pub struct IndexResult {
    /// Сколько файлов просмотрено (не считая бинарных)
    pub files_scanned: usize,
    /// Сколько файлов реально записано в БД (новые или изменённые)
    pub files_indexed: usize,
    /// Сколько файлов пропущено (хеш не изменился)
    pub files_skipped: usize,
    /// Сколько файлов удалено из БД (больше не существуют на диске)
    pub files_deleted: usize,
    /// Список ошибок: (путь, сообщение)
    pub errors: Vec<(String, String)>,
    /// Время работы в миллисекундах
    pub elapsed_ms: u64,
}

/// Результат параллельного парсинга одного файла
pub enum ParsedFile {
    /// Файл с исходным кодом успешно распаршен
    Code {
        rel_path: String,
        content_hash: String,
        language: String,
        lines_total: usize,
        ast_hash: String,
        parse_result: ParseResult,
        mtime: i64,
        file_size: i64,
        /// Для языков с двойной индексацией (html в v0.7.1) — raw-content
        /// для дополнительной записи в text_files (FTS+regex+read_file).
        /// Для остальных языков — None.
        text_for_fts: Option<String>,
        /// Phase 2 (v0.8.0): исходный content для записи в `file_contents`
        /// с zstd-сжатием. Хранится здесь, а не вычисляется на лету, потому
        /// что после parse-этапа исходный буфер `candidate_files` теряется.
        raw_content: String,
    },
    /// Текстовый файл (без AST)
    Text {
        rel_path: String,
        content_hash: String,
        lines_total: usize,
        content: String,
        language: String,
        mtime: i64,
        file_size: i64,
    },
    /// Ошибка парсинга
    Error {
        rel_path: String,
        error: String,
    },
}

/// Индексатор файловой системы
pub struct Indexer<'a> {
    storage: &'a mut Storage,
    /// Конфигурация индексатора
    config: IndexConfig,
    /// Хэндл для отправки граф-команд (None = граф выключен)
    graph: GraphStoreHandle,
    /// Алиас репозитория — метка в граф-узлах (repo-свойство)
    repo: String,
}

impl<'a> Indexer<'a> {
    /// Создать индексатор с уже открытым хранилищем и конфигурацией по умолчанию
    pub fn new(storage: &'a mut Storage) -> Self {
        Self {
            storage,
            config: IndexConfig::default(),
            graph: GraphStoreHandle::disabled(),
            repo: String::new(),
        }
    }

    /// Создать индексатор с явно переданной конфигурацией
    pub fn with_config(storage: &'a mut Storage, config: IndexConfig) -> Self {
        Self {
            storage,
            config,
            graph: GraphStoreHandle::disabled(),
            repo: String::new(),
        }
    }

    /// Добавить граф-хэндл и алиас репо (включает dual-write в граф-БД).
    pub fn with_graph(mut self, graph: GraphStoreHandle, repo: impl Into<String>) -> Self {
        self.graph = graph;
        self.repo = repo.into();
        self
    }

    /// Полная переиндексация директории `root`.
    pub fn full_reindex(&mut self, root: &Path, force: bool) -> Result<IndexResult> {
        let start = std::time::Instant::now();
        let mut result = IndexResult::default();

        let existing_files = self.load_existing_files()?;
        let is_fresh_db = existing_files.is_empty();

        let t = std::time::Instant::now();
        let (candidates, seen_paths, meta_updates) =
            self.collect_candidates(root, force, &existing_files, &mut result)?;
        debug!("[timing] collect: {}ms ({} files)", t.elapsed().as_millis(), candidates.len());

        let bulk_mode = candidates.len() > self.config.bulk_threshold;
        if bulk_mode {
            info!("[bulk] {} files (threshold {}): dropping indexes", candidates.len(), self.config.bulk_threshold);
            self.storage.prepare_bulk_load()?;
        }

        let t = std::time::Instant::now();
        let parsed = self.phase_parse(&candidates);
        debug!("[timing] parse: {}ms ({} files)", t.elapsed().as_millis(), parsed.len());

        let t = std::time::Instant::now();
        self.phase_write(&parsed, is_fresh_db, &mut result)?;
        debug!("[timing] write: {}ms ({} files)", t.elapsed().as_millis(), result.files_indexed);

        self.phase_update_metadata(&meta_updates)?;

        if bulk_mode {
            let t = std::time::Instant::now();
            info!("[bulk] rebuilding indexes + FTS...");
            self.storage.finish_bulk_load()?;
            debug!("[timing] index rebuild: {}ms", t.elapsed().as_millis());
        }

        let t = std::time::Instant::now();
        self.phase_cleanup(&existing_files, &seen_paths, &mut result)?;
        if result.files_deleted > 0 {
            debug!("[timing] cleanup: {}ms ({} deleted)", t.elapsed().as_millis(), result.files_deleted);
        }

        self.phase_backfill(root)?;

        // Сигнатуры зависимостей (vendor/node_modules) для ООП-резолва — opt-in.
        if !self.config.dependency_dirs.is_empty() {
            if let Err(e) = self.phase_signatures(root, force) {
                warn!("[signatures] пропущено из-за ошибки: {}", e);
            }
        }

        result.elapsed_ms = start.elapsed().as_millis() as u64;
        info!("[timing] total: {}ms (indexed={} skipped={} deleted={} errors={})",
            result.elapsed_ms, result.files_indexed, result.files_skipped,
            result.files_deleted, result.errors.len());
        Ok(result)
    }

    fn load_existing_files(&self) -> Result<HashMap<String, (i64, String, Option<i64>, Option<i64>)>> {
        Ok(self.storage
            .get_all_files()?
            .into_iter()
            .filter_map(|f| f.id.map(|id| (f.path.clone(), (id, f.content_hash.clone(), f.mtime, f.file_size))))
            .collect())
    }

    fn phase_parse(&self, candidates: &[(String, String, String, FileCategory, i64, i64)]) -> Vec<ParsedFile> {
        let registry = ParserRegistry::from_languages(&self.config.languages);
        candidates.par_iter().map(|(rel_path, content, hash, category, mtime, file_size)| {
            match category {
                FileCategory::Code(language) => {
                    let ext = Path::new(rel_path.as_str())
                        .extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
                    match registry.get_parser(&ext) {
                        Some(parser) => match parser.parse(content, rel_path) {
                            Ok(pr) => ParsedFile::Code {
                                rel_path: rel_path.clone(),
                                content_hash: hash.clone(),
                                language: language.clone(),
                                lines_total: pr.lines_total,
                                ast_hash: pr.ast_hash.clone(),
                                text_for_fts: file_types::is_dual_indexed_language(language).then(|| content.clone()),
                                raw_content: content.clone(),
                                parse_result: pr,
                                mtime: *mtime,
                                file_size: *file_size,
                            },
                            Err(e) => ParsedFile::Error { rel_path: rel_path.clone(), error: e.to_string() },
                        },
                        None => ParsedFile::Error {
                            rel_path: rel_path.clone(),
                            error: format!("no parser for extension: {}", ext),
                        },
                    }
                }
                FileCategory::Text => {
                    let tr = TextParser::parse(content);
                    let ext = Path::new(rel_path.as_str())
                        .extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
                    let language = file_types::text_language_for_ext(&ext).to_string();
                    ParsedFile::Text { rel_path: rel_path.clone(), content_hash: hash.clone(),
                        lines_total: tr.lines_total, content: tr.content, language, mtime: *mtime, file_size: *file_size }
                }
                FileCategory::Binary => unreachable!("binary files must be filtered before parse"),
            }
        }).collect()
    }

    fn phase_write(&mut self, parsed: &[ParsedFile], is_fresh_db: bool, result: &mut IndexResult) -> Result<()> {
        let batch_size = self.config.batch_size;
        let mut batch_count = 0usize;
        self.storage.begin_batch()?;
        for p in parsed {
            match p {
                ParsedFile::Code { rel_path, content_hash, language, lines_total, ast_hash,
                    parse_result, mtime, file_size, text_for_fts, raw_content } => {
                    match self.write_code_to_db(rel_path, content_hash, language, *lines_total,
                        ast_hash, parse_result, is_fresh_db, Some(*mtime), Some(*file_size),
                        text_for_fts.as_deref(), Some(raw_content.as_str())) {
                        Ok(_) => {
                            result.files_indexed += 1; batch_count += 1;
                            let _ = self.storage.clear_parse_error(rel_path); // снова парсится — снять отметку
                        }
                        Err(e) => result.errors.push((rel_path.clone(), e.to_string())),
                    }
                }
                ParsedFile::Text { rel_path, content_hash, lines_total, content, language, mtime, file_size } => {
                    match self.write_text_to_db(rel_path, content_hash, *lines_total, content,
                        language, is_fresh_db, Some(*mtime), Some(*file_size)) {
                        Ok(_) => {
                            result.files_indexed += 1; batch_count += 1;
                            let _ = self.storage.clear_parse_error(rel_path);
                        }
                        Err(e) => result.errors.push((rel_path.clone(), e.to_string())),
                    }
                }
                ParsedFile::Error { rel_path, error } => {
                    result.errors.push((rel_path.clone(), error.clone()));
                    // Сделать слепую зону видимой: запомнить непарсящийся файл.
                    let _ = self.storage.upsert_parse_error(rel_path, error);
                }
            }
            if batch_count >= batch_size {
                self.storage.commit_batch()?;
                self.storage.begin_batch()?;
                batch_count = 0;
            }
        }
        self.storage.commit_batch()?;
        Ok(())
    }

    fn phase_update_metadata(&mut self, updates: &[(String, i64, i64)]) -> Result<()> {
        if updates.is_empty() { return Ok(()); }
        self.storage.begin_batch()?;
        for (path, mtime, file_size) in updates {
            let _ = self.storage.update_file_metadata(path, *mtime, *file_size);
        }
        self.storage.commit_batch()
    }

    fn phase_cleanup(
        &mut self,
        existing: &HashMap<String, (i64, String, Option<i64>, Option<i64>)>,
        seen: &HashSet<String>,
        result: &mut IndexResult,
    ) -> Result<()> {
        self.storage.begin_batch()?;
        for (path, (id, _, _, _)) in existing {
            if !seen.contains(path) {
                self.storage.delete_file(*id)?;
                result.files_deleted += 1;
            }
        }
        // Прунинг parse_errors удалённых с диска файлов: у непарсящихся файлов нет
        // строки в `files`, поэтому чистим по `seen` (всё, что реально просмотрено
        // в этом проходе). Иначе строки-ошибки «утекают» навсегда.
        for path in self.storage.parse_error_paths().unwrap_or_default() {
            if !seen.contains(&path) {
                let _ = self.storage.clear_parse_error(&path);
            }
        }
        self.storage.commit_batch()
    }

    /// Индексация СИГНАТУР зависимостей (vendor/node_modules): классы+bases и
    /// методы (без тел) в таблицы `ext_*` для ООП-резолва наследования от
    /// фреймворка. Пересканирует только при `force` или если ext пуст (deps
    /// редко меняются). Парсит параллельно, держит в памяти только сигнатуры.
    fn phase_signatures(&mut self, root: &Path, force: bool) -> Result<()> {
        // Гейтим по факту «просканировано» (а не по числу строк): иначе при
        // нулевом результате (пустой vendor / непарсимые файлы) скан повторялся бы
        // на каждом reindex. Обновление deps → `icode index --force`.
        if !force && self.storage.meta_get("ext_scanned").is_some() {
            return Ok(());
        }
        let t = std::time::Instant::now();
        self.storage.clear_ext_signatures()?;
        let registry = ParserRegistry::from_languages(&self.config.languages);

        let mut ext_classes: Vec<(String, Option<String>)> = Vec::new();
        let mut ext_methods: Vec<(String, String)> = Vec::new();

        for dep in &self.config.dependency_dirs {
            let dir = root.join(dep);
            if !dir.is_dir() {
                continue;
            }
            let files: Vec<std::path::PathBuf> = WalkDir::new(&dir)
                .into_iter()
                .filter_entry(|e| {
                    !(e.file_type().is_dir()
                        && e.file_name().to_str().map(|n| n == ".git").unwrap_or(false))
                })
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().is_file())
                .map(|e| e.path().to_path_buf())
                .collect();

            // Параллельный парсинг: на каждый файл извлекаем только сигнатуры,
            // ParseResult (с телами) сразу отбрасывается — память не растёт.
            let sigs: Vec<(Vec<(String, Option<String>)>, Vec<(String, String)>)> = files
                .par_iter()
                .filter_map(|p| {
                    let ext = p.extension().and_then(|e| e.to_str())?.to_lowercase();
                    let parser = registry.get_parser(&ext)?;
                    let content = std::fs::read_to_string(p).ok()?;
                    let pr = parser.parse(&content, &p.to_string_lossy()).ok()?;
                    let classes = pr
                        .classes
                        .iter()
                        .map(|c| (c.name.clone(), c.bases.clone()))
                        .collect();
                    let methods = pr
                        .functions
                        .iter()
                        .filter_map(|f| {
                            f.qualified_name.as_ref().and_then(|qn| {
                                qn.rsplit_once("::").map(|(cls, m)| (cls.to_string(), m.to_string()))
                            })
                        })
                        .collect();
                    Some((classes, methods))
                })
                .collect();

            for (c, m) in sigs {
                ext_classes.extend(c);
                ext_methods.extend(m);
            }
        }

        self.storage.begin_batch()?;
        let r1 = self.storage.insert_ext_classes(&ext_classes);
        let r2 = self.storage.insert_ext_methods(&ext_methods);
        // Маркер «просканировано» — чтобы не пересканировать при пустом результате.
        let r3 = self.storage.meta_set("ext_scanned", "1");
        self.storage.commit_batch()?;
        r1?;
        r2?;
        r3?;
        info!(
            "[signatures] зависимости: {} типов, {} методов за {} мс",
            ext_classes.len(),
            ext_methods.len(),
            t.elapsed().as_millis()
        );
        Ok(())
    }

    fn phase_backfill(&mut self, root: &Path) -> Result<()> {
        let candidates = self.storage.list_code_files_without_content()?;
        if candidates.is_empty() { return Ok(()); }
        let t = std::time::Instant::now();
        let batch_size = self.config.batch_size.max(500);
        let mut done = 0usize;
        let mut in_batch = 0usize;
        self.storage.begin_batch()?;
        for (file_id, path) in &candidates {
            match std::fs::read_to_string(root.join(path)) {
                Ok(content) => {
                    match self.storage.upsert_file_content(*file_id, &content, self.config.max_code_file_size_bytes) {
                        Ok(_) => done += 1,
                        Err(e) => warn!("[backfill] file_id={} path={}: {}", file_id, path, e),
                    }
                }
                Err(e) => warn!("[backfill] file_id={} path={}: read error: {}", file_id, path, e),
            }
            in_batch += 1;
            if in_batch >= batch_size {
                self.storage.commit_batch()?;
                self.storage.begin_batch()?;
                in_batch = 0;
            }
        }
        self.storage.commit_batch()?;
        debug!("[timing] backfill: {}ms ({}/{} filled)", t.elapsed().as_millis(), done, candidates.len());
        Ok(())
    }

    /// Записать код-файл в БД: метаданные + символы (функции, классы, импорты и т.д.)
    /// skip_delete: при первичной индексации пропускать DELETE (БД пуста, удалять нечего)
    /// raw_content: Phase 2 (v0.8.0). Если задан — content сохраняется в `file_contents`
    /// с zstd-сжатием. Файлы крупнее `config.max_code_file_size_bytes` получают
    /// «oversize»-запись (см. `Storage::upsert_file_content`). `None` отключает
    /// сохранение (используется в тестах и местах, где content недоступен).
    pub fn write_code_to_db(
        &self,
        rel_path: &str,
        content_hash: &str,
        language: &str,
        lines_total: usize,
        ast_hash: &str,
        parse_result: &ParseResult,
        skip_delete: bool,
        mtime: Option<i64>,
        file_size: Option<i64>,
        // Для языков с двойной индексацией (html в v0.7.1) — raw-content,
        // который дополнительно записывается в text_files. Для остальных — None.
        text_for_fts: Option<&str>,
        // Phase 2: исходный content для записи в `file_contents` (zstd).
        raw_content: Option<&str>,
    ) -> Result<()> {
        // Сохраняем запись о файле
        let file_record = FileRecord {
            id: None,
            path: rel_path.to_string(),
            content_hash: content_hash.to_string(),
            ast_hash: Some(ast_hash.to_string()),
            language: language.to_string(),
            lines_total,
            indexed_at: chrono::Utc::now()
                .format("%Y-%m-%d %H:%M:%S")
                .to_string(),
            mtime,
            file_size,
        };
        let file_id = self.storage.upsert_file(&file_record)?;

        // Удаляем старые данные перед вставкой новых
        // При первичной индексации (skip_delete) — пропускаем, БД пуста
        if !skip_delete {
            // Граф: удаляем старые узлы/рёбра файла перед upsert новых
            if self.graph.is_enabled() {
                self.graph.send(GraphCommand::DeleteFileData {
                    repo: self.repo.clone(),
                    path: rel_path.to_string(),
                });
            }

            self.storage.delete_functions_by_file(file_id)?;
            self.storage.delete_classes_by_file(file_id)?;
            self.storage.delete_imports_by_file(file_id)?;
            self.storage.delete_calls_by_file(file_id)?;
            self.storage.delete_variables_by_file(file_id)?;
            self.storage.delete_routes_by_file(file_id)?;
            // Для языков с двойной индексацией убираем старую запись text_files,
            // чтобы не дублировать при upsert.
            if text_for_fts.is_some() {
                self.storage.delete_text_file_by_file(file_id)?;
            }
        }

        // Конвертируем и сохраняем функции
        let functions: Vec<FunctionRecord> = parse_result
            .functions
            .iter()
            .map(|f| FunctionRecord {
                id: None,
                file_id,
                name: f.name.clone(),
                qualified_name: f.qualified_name.clone(),
                line_start: f.line_start,
                line_end: f.line_end,
                args: f.args.clone(),
                return_type: f.return_type.clone(),
                docstring: f.docstring.clone(),
                body: f.body.clone(),
                is_async: f.is_async,
                node_hash: f.node_hash.clone(),
                // Поля переопределения BSL-расширения (для других языков = None)
                override_type: f.override_type.clone(),
                override_target: f.override_target.clone(),
            })
            .collect();
        self.storage.insert_functions(&functions)?;

        // Конвертируем и сохраняем классы
        let classes: Vec<ClassRecord> = parse_result
            .classes
            .iter()
            .map(|c| ClassRecord {
                id: None,
                file_id,
                name: c.name.clone(),
                line_start: c.line_start,
                line_end: c.line_end,
                bases: c.bases.clone(),
                docstring: c.docstring.clone(),
                body: c.body.clone(),
                node_hash: c.node_hash.clone(),
            })
            .collect();
        self.storage.insert_classes(&classes)?;

        // Конвертируем и сохраняем импорты
        let imports: Vec<ImportRecord> = parse_result
            .imports
            .iter()
            .map(|i| ImportRecord {
                id: None,
                file_id,
                module: i.module.clone(),
                name: i.name.clone(),
                alias: i.alias.clone(),
                line: i.line,
                kind: i.kind.clone(),
            })
            .collect();
        self.storage.insert_imports(&imports)?;

        // Конвертируем и сохраняем вызовы функций
        let calls: Vec<CallRecord> = parse_result
            .calls
            .iter()
            .map(|c| CallRecord {
                id: None,
                file_id,
                caller: c.caller.clone(),
                callee: c.callee.clone(),
                line: c.line,
                receiver: c.receiver.clone(),
            })
            .collect();
        self.storage.insert_calls(&calls)?;

        // Конвертируем и сохраняем переменные
        let variables: Vec<VariableRecord> = parse_result
            .variables
            .iter()
            .map(|v| VariableRecord {
                id: None,
                file_id,
                name: v.name.clone(),
                value: v.value.clone(),
                line: v.line,
            })
            .collect();
        self.storage.insert_variables(&variables)?;

        // Конвертируем и сохраняем веб-маршруты (framework-aware routing)
        let routes: Vec<RouteRecord> = parse_result
            .routes
            .iter()
            .map(|r| RouteRecord {
                id: None,
                file_id,
                method: r.method.clone(),
                path: r.path.clone(),
                handler_class: r.handler_class.clone(),
                handler_method: r.handler_method.clone(),
                name: r.name.clone(),
                line: r.line,
            })
            .collect();
        self.storage.insert_routes(&routes)?;

        // Двойная индексация: для html (и других языков из is_dual_indexed_language)
        // дополнительно сохраняем сырой контент в text_files, чтобы продолжали
        // работать search_text/grep_text/read_file как раньше.
        if let Some(content) = text_for_fts {
            self.storage.insert_text_file(&crate::storage::models::TextFileRecord {
                id: None,
                file_id,
                content: content.to_string(),
            })?;
        }

        // Phase 2: сохраняем исходный content в `file_contents` (zstd).
        // Файлы крупнее `max_code_file_size_bytes` получают oversize-запись (без blob).
        if let Some(raw) = raw_content {
            self.storage
                .upsert_file_content(file_id, raw, self.config.max_code_file_size_bytes)?;
        }

        // Граф dual-write: отправляем команды в канал (fire-and-forget).
        if self.graph.is_enabled() {
            self.graph.send(GraphCommand::UpsertFile {
                repo: self.repo.clone(),
                path: rel_path.to_string(),
                language: language.to_string(),
                hash: content_hash.to_string(),
            });

            for f in &parse_result.functions {
                self.graph.send(GraphCommand::UpsertFunction {
                    repo: self.repo.clone(),
                    file_path: rel_path.to_string(),
                    name: f.name.clone(),
                    qualified_name: f.qualified_name.clone(),
                    line_start: f.line_start,
                    line_end: f.line_end,
                });
            }

            for c in &parse_result.classes {
                self.graph.send(GraphCommand::UpsertClass {
                    repo: self.repo.clone(),
                    file_path: rel_path.to_string(),
                    name: c.name.clone(),
                    line_start: c.line_start,
                    line_end: c.line_end,
                    bases: c.bases.clone(),
                });
            }

            for call in &parse_result.calls {
                self.graph.send(GraphCommand::AddCall {
                    repo: self.repo.clone(),
                    file_path: rel_path.to_string(),
                    caller: call.caller.clone(),
                    callee: call.callee.clone(),
                    line: call.line,
                });
            }

            for imp in &parse_result.imports {
                if let Some(module) = &imp.module {
                    self.graph.send(GraphCommand::AddImport {
                        repo: self.repo.clone(),
                        file_path: rel_path.to_string(),
                        module: module.clone(),
                        kind: imp.kind.clone(),
                    });
                }
            }

            // Framework-aware routing: Route-узел + ребро HANDLED_BY к контроллер-методу.
            for route in &parse_result.routes {
                if let Some(handler_method) = &route.handler_method {
                    self.graph.send(GraphCommand::AddRoute {
                        repo: self.repo.clone(),
                        file_path: rel_path.to_string(),
                        method: route.method.clone(),
                        path: route.path.clone(),
                        handler_class: route.handler_class.clone(),
                        handler_method: handler_method.clone(),
                        line: route.line,
                    });
                }
            }
        }

        Ok(())
    }

    /// Записать текстовый файл в БД: метаданные + полное содержимое для FTS
    pub fn write_text_to_db(
        &self,
        rel_path: &str,
        content_hash: &str,
        lines_total: usize,
        content: &str,
        language: &str,
        skip_delete: bool,
        mtime: Option<i64>,
        file_size: Option<i64>,
    ) -> Result<()> {
        let file_record = FileRecord {
            id: None,
            path: rel_path.to_string(),
            content_hash: content_hash.to_string(),
            ast_hash: None,
            language: language.to_string(),
            lines_total,
            indexed_at: chrono::Utc::now()
                .format("%Y-%m-%d %H:%M:%S")
                .to_string(),
            mtime,
            file_size,
        };
        let file_id = self.storage.upsert_file(&file_record)?;

        // Удаляем старую запись текстового файла и вставляем новую
        if !skip_delete {
            self.storage.delete_text_file_by_file(file_id)?;
        }

        let text_record = TextFileRecord {
            id: None,
            file_id,
            content: content.to_string(),
        };
        self.storage.insert_text_file(&text_record)?;

        Ok(())
    }

    /// Первый проход: обойти директорию, собрать список файлов для индексации.
    ///
    /// Трёхфазный сбор:
    /// 1a. WalkDir — быстрый обход, собрать пути + metadata (mtime/size) без чтения содержимого
    /// 1b. mtime/size pre-filter — пропустить файлы, где mtime+size совпадают с БД
    /// 1c. rayon par_iter — параллельное чтение + SHA-256 хеш только изменённых файлов
    /// 1d. hash comparison — пропустить файлы с неизменённым хешем, собрать metadata_updates
    ///
    /// Возвращает (candidates, seen_paths, metadata_updates).
    /// seen_paths используется для очистки удалённых файлов без повторного обхода дерева.
    /// metadata_updates содержит файлы, у которых хеш не изменился, но mtime/size обновились.
    fn collect_candidates(
        &self,
        root: &Path,
        force: bool,
        existing_files: &HashMap<String, (i64, String, Option<i64>, Option<i64>)>,
        result: &mut IndexResult,
    ) -> Result<(Vec<(String, String, String, FileCategory, i64, i64)>, HashSet<String>, Vec<(String, i64, i64)>)> {
        let config_for_filter = self.config.clone();
        let file_matcher = self.config.build_file_exclude_matcher();

        // ── Фаза 1a: WalkDir — собрать пути + metadata (без чтения содержимого) ──
        let walker = WalkDir::new(root).into_iter().filter_entry(move |e| {
            if e.file_type().is_dir() {
                if let Some(name) = e.file_name().to_str() {
                    return !config_for_filter.is_excluded_dir(name);
                }
            }
            true
        });

        struct FileEntry {
            abs_path: std::path::PathBuf,
            rel_path: String,
            category: FileCategory,
            mtime: i64,
            file_size: i64,
        }
        let mut entries: Vec<FileEntry> = Vec::new();
        let mut seen_paths: HashSet<String> = HashSet::new();

        for entry in walker.filter_map(|e| e.ok()) {
            if !entry.file_type().is_file() {
                continue;
            }

            // Проверяем лимит количества файлов (0 = без лимита)
            if self.config.max_files > 0 && result.files_scanned >= self.config.max_files {
                break;
            }

            let path = entry.path();

            // Проверяем exclude_file_patterns по имени файла
            if let Some(fname) = path.file_name().and_then(|f| f.to_str()) {
                if file_matcher.is_match(fname) {
                    continue;
                }
            }

            let category = categorize_file(path);

            if matches!(category, FileCategory::Binary) {
                continue;
            }

            // Получаем метаданные для всех файлов
            let meta = entry.metadata().ok();

            // Лимит размера — только для текстовых файлов, код индексируем всегда
            if !matches!(category, FileCategory::Code(_)) {
                if let Some(ref m) = meta {
                    if m.len() as usize > self.config.max_file_size {
                        result.files_skipped += 1;
                        continue;
                    }
                }
            }

            // mtime и file_size для быстрой проверки изменений
            let mtime = meta.as_ref()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let file_size_val = meta.as_ref().map(|m| m.len() as i64).unwrap_or(0);

            let rel_path = path
                .strip_prefix(root)
                .unwrap_or(path)
                .to_string_lossy()
                .replace('\\', "/");

            result.files_scanned += 1;
            seen_paths.insert(rel_path.clone());
            entries.push(FileEntry {
                abs_path: path.to_path_buf(),
                rel_path,
                category,
                mtime,
                file_size: file_size_val,
            });
        }

        // ── Фаза 1b: быстрая фильтрация по mtime+size (без чтения файлов) ──
        let (entries_to_read, mtime_skipped): (Vec<&FileEntry>, usize) = if force {
            (entries.iter().collect(), 0)
        } else {
            let mut to_read = Vec::new();
            let mut skipped = 0usize;
            for entry in &entries {
                match existing_files.get(&entry.rel_path) {
                    Some((_, _, Some(stored_mtime), Some(stored_size)))
                        if *stored_mtime == entry.mtime && *stored_size == entry.file_size =>
                    {
                        skipped += 1;
                    }
                    _ => to_read.push(entry),
                }
            }
            (to_read, skipped)
        };
        result.files_skipped += mtime_skipped;

        // ── Фаза 1c: параллельное чтение + хеш изменённых файлов (rayon) ────
        let read_results: Vec<_> = entries_to_read
            .par_iter()
            .map(|entry| {
                match hasher::file_hash(&entry.abs_path) {
                    Ok((content, hash)) => Ok((entry.rel_path.clone(), content, hash, entry.category.clone(), entry.mtime, entry.file_size)),
                    Err(e) => Err((entry.rel_path.clone(), e.to_string())),
                }
            })
            .collect();

        // ── Фаза 1d: фильтрация по hash + metadata-only updates ────────────
        let mut candidates = Vec::new();
        let mut metadata_updates: Vec<(String, i64, i64)> = Vec::new();
        for item in read_results {
            match item {
                Ok((rel_path, content, hash, category, mtime, file_size)) => {
                    if !force {
                        if let Some((_, existing_hash, _, _)) = existing_files.get(&rel_path) {
                            if *existing_hash == hash {
                                // Содержимое не изменилось, но mtime/size мог — обновим метаданные
                                metadata_updates.push((rel_path, mtime, file_size));
                                result.files_skipped += 1;
                                continue;
                            }
                        }
                    }
                    candidates.push((rel_path, content, hash, category, mtime, file_size));
                }
                Err((rel_path, error)) => {
                    result.errors.push((rel_path, error));
                }
            }
        }

        Ok((candidates, seen_paths, metadata_updates))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_categorize_file() {
        assert_eq!(
            file_types::categorize_file(Path::new("test.py")),
            FileCategory::Code("python".to_string())
        );
        assert_eq!(
            file_types::categorize_file(Path::new("readme.md")),
            FileCategory::Text
        );
        assert_eq!(
            file_types::categorize_file(Path::new("image.png")),
            FileCategory::Binary
        );
    }

    #[test]
    fn test_full_reindex() {
        let tmp = TempDir::new().unwrap();

        // Создаём Python-файл с функцией и классом
        fs::write(
            tmp.path().join("main.py"),
            r#"
def hello():
    """Приветствие."""
    print("Hello!")

class App:
    def run(self):
        pass
"#,
        )
        .unwrap();

        // Создаём текстовый файл
        fs::write(tmp.path().join("readme.md"), "# Project\nDescription").unwrap();

        let mut storage = Storage::open_in_memory().unwrap();
        let mut indexer = Indexer::new(&mut storage);
        let result = indexer.full_reindex(tmp.path(), false).unwrap();

        assert_eq!(result.files_indexed, 2, "оба файла должны быть проиндексированы");
        assert_eq!(result.files_skipped, 0, "пропущенных файлов быть не должно");
        assert_eq!(result.errors.len(), 0, "ошибок быть не должно");

        // Проверяем, что данные сохранились в БД
        let stats = storage.get_stats().unwrap();
        assert!(stats.total_functions >= 2, "минимум 2 функции: hello + run");
        assert!(stats.total_classes >= 1, "минимум 1 класс: App");
        assert!(stats.total_text_files >= 1, "минимум 1 текстовый файл: readme.md");
    }

    #[test]
    fn test_reindex_skips_unchanged() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("test.py"), "def foo():\n    pass\n").unwrap();

        let mut storage = Storage::open_in_memory().unwrap();

        // Первая индексация
        {
            let mut indexer = Indexer::new(&mut storage);
            let r1 = indexer.full_reindex(tmp.path(), false).unwrap();
            assert_eq!(r1.files_indexed, 1, "первый проход должен проиндексировать файл");
        }

        // Второй проход без изменений — файл должен быть пропущен
        {
            let mut indexer = Indexer::new(&mut storage);
            let r2 = indexer.full_reindex(tmp.path(), false).unwrap();
            assert_eq!(r2.files_indexed, 0, "повторная индексация не должна записывать файл");
            assert_eq!(r2.files_skipped, 1, "файл должен быть пропущен как неизменённый");
        }
    }

    #[test]
    fn test_reindex_force_reindexes() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("test.py"), "def foo():\n    pass\n").unwrap();

        let mut storage = Storage::open_in_memory().unwrap();

        {
            let mut indexer = Indexer::new(&mut storage);
            indexer.full_reindex(tmp.path(), false).unwrap();
        }

        // Force-режим — файл должен быть переиндексирован, даже если не изменился
        {
            let mut indexer = Indexer::new(&mut storage);
            let r = indexer.full_reindex(tmp.path(), true).unwrap();
            assert_eq!(r.files_indexed, 1, "force=true должен переиндексировать файл");
            assert_eq!(r.files_skipped, 0, "при force=true пропущенных быть не должно");
        }
    }

    #[test]
    fn test_deleted_files_removed_from_db() {
        let tmp = TempDir::new().unwrap();
        let py_path = tmp.path().join("temp.py");
        fs::write(&py_path, "def bar():\n    pass\n").unwrap();

        let mut storage = Storage::open_in_memory().unwrap();

        // Индексируем файл
        {
            let mut indexer = Indexer::new(&mut storage);
            let r = indexer.full_reindex(tmp.path(), false).unwrap();
            assert_eq!(r.files_indexed, 1);
        }

        // Удаляем файл с диска
        fs::remove_file(&py_path).unwrap();

        // Повторная индексация — запись должна исчезнуть из БД
        {
            let mut indexer = Indexer::new(&mut storage);
            let r = indexer.full_reindex(tmp.path(), false).unwrap();
            assert_eq!(r.files_deleted, 1, "удалённый файл должен быть убран из БД");
        }

        let stats = storage.get_stats().unwrap();
        assert_eq!(stats.total_files, 0, "БД должна быть пуста после удаления файла");
    }

    #[test]
    fn test_excludes_binary_files() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("test.py"), "x = 1\n").unwrap();
        // Бинарный файл — не должен попасть в индекс
        fs::write(tmp.path().join("image.png"), b"\x89PNG\r\n\x1a\n").unwrap();

        let mut storage = Storage::open_in_memory().unwrap();
        let mut indexer = Indexer::new(&mut storage);
        let r = indexer.full_reindex(tmp.path(), false).unwrap();

        // Только Python-файл проиндексирован, PNG пропущен (бинарный)
        assert_eq!(r.files_scanned, 1, "бинарные файлы не должны попасть в files_scanned");
        assert_eq!(r.files_indexed, 1);
    }

    #[test]
    fn test_excludes_target_dir() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("target")).unwrap();
        fs::write(tmp.path().join("target").join("debug.py"), "x = 1\n").unwrap();
        fs::write(tmp.path().join("main.py"), "y = 2\n").unwrap();

        let mut storage = Storage::open_in_memory().unwrap();
        let mut indexer = Indexer::new(&mut storage);
        let r = indexer.full_reindex(tmp.path(), false).unwrap();

        // Файл в target/ должен быть исключён
        assert_eq!(r.files_indexed, 1, "только main.py должен быть проиндексирован");
    }

    #[test]
    fn test_hasher_deterministic() {
        let hash1 = hasher::content_hash(b"hello world");
        let hash2 = hasher::content_hash(b"hello world");
        assert_eq!(hash1, hash2, "хеш должен быть детерминированным");

        let hash3 = hasher::content_hash(b"different content");
        assert_ne!(hash1, hash3, "разные данные дают разные хеши");
    }

    #[test]
    fn test_with_config_custom_exclude() {
        let tmp = TempDir::new().unwrap();
        // Создаём директорию vendor с файлом
        fs::create_dir(tmp.path().join("vendor")).unwrap();
        fs::write(tmp.path().join("vendor").join("lib.py"), "x = 1\n").unwrap();
        // Основной файл проекта
        fs::write(tmp.path().join("app.py"), "y = 2\n").unwrap();

        let mut storage = Storage::open_in_memory().unwrap();
        let config = IndexConfig {
            exclude_dirs: vec!["vendor".to_string()],
            ..Default::default()
        };
        let mut indexer = Indexer::with_config(&mut storage, config);
        let r = indexer.full_reindex(tmp.path(), false).unwrap();

        // vendor/ исключён через конфиг — только app.py
        assert_eq!(r.files_indexed, 1, "vendor должен быть исключён через конфиг");
    }

    #[test]
    fn test_bulk_load_mode() {
        let tmp = TempDir::new().unwrap();

        // Создаём 15 Python-файлов с уникальными функциями
        for i in 0..15 {
            fs::write(
                tmp.path().join(format!("module_{i}.py")),
                format!(
                    "def func_{i}(x):\n    \"\"\"Функция номер {i}.\"\"\"\n    return x + {i}\n"
                ),
            )
            .unwrap();
        }

        let mut storage = Storage::open_in_memory().unwrap();

        // Устанавливаем порог 10 — при 15 файлах должен включиться bulk-load
        let config = IndexConfig {
            bulk_threshold: 10,
            ..Default::default()
        };

        // Первый проход: индексируем все 15 файлов в bulk-load режиме
        {
            let mut indexer = Indexer::with_config(&mut storage, config.clone());
            let result = indexer.full_reindex(tmp.path(), false).unwrap();
            assert_eq!(result.files_indexed, 15, "все 15 файлов должны быть проиндексированы");
            assert_eq!(result.files_skipped, 0, "пропущенных файлов быть не должно");
            assert_eq!(result.errors.len(), 0, "ошибок быть не должно");
        }

        // Проверяем статистику в БД (indexer уже дропнут)
        let stats = storage.get_stats().unwrap();
        assert_eq!(stats.total_files, 15, "в БД должно быть 15 файлов");
        assert_eq!(stats.total_functions, 15, "по одной функции на файл");

        // Проверяем, что FTS работает после rebuild
        let found = storage.search_functions("func_0", 10, None).unwrap();
        assert!(!found.is_empty(), "FTS должен находить func_0 после bulk-load rebuild");

        let found_5 = storage.search_functions("func_5", 10, None).unwrap();
        assert!(!found_5.is_empty(), "FTS должен находить func_5 после bulk-load rebuild");

        // Второй проход: повторная индексация — все файлы должны быть пропущены
        {
            let mut indexer = Indexer::with_config(&mut storage, config);
            let result2 = indexer.full_reindex(tmp.path(), false).unwrap();
            assert_eq!(result2.files_skipped, 15, "при повторной индексации все файлы неизменны");
            assert_eq!(result2.files_indexed, 0, "ни одного файла не должно быть переиндексировано");
        }
    }

    #[test]
    fn test_with_config_max_file_size() {
        let tmp = TempDir::new().unwrap();
        // Маленький текстовый файл — пройдёт
        fs::write(tmp.path().join("small.txt"), "x = 1\n").unwrap();
        // Большой текстовый файл — пропустим (лимит 10 байт)
        // Лимит max_file_size действует только на Text-файлы, код индексируется всегда
        fs::write(tmp.path().join("big.txt"), "y = 'a very long string that exceeds limit'\n").unwrap();
        // Большой код-файл — НЕ пропускается (код индексируется независимо от размера)
        fs::write(tmp.path().join("big.py"), "y = 'a very long string that exceeds limit'\n").unwrap();

        let mut storage = Storage::open_in_memory().unwrap();
        let config = IndexConfig {
            max_file_size: 10, // 10 байт
            ..Default::default()
        };
        let mut indexer = Indexer::with_config(&mut storage, config);
        let r = indexer.full_reindex(tmp.path(), false).unwrap();

        // big.txt пропущен из-за лимита размера, big.py — нет (код не ограничен)
        assert_eq!(r.files_indexed, 2, "small.txt + big.py (код не ограничен размером)");
        assert_eq!(r.files_skipped, 1, "big.txt пропущен по размеру");
    }

    #[test]
    fn test_batch_transactions() {
        let tmp = TempDir::new().unwrap();

        // Создаём 20 Python-файлов с уникальными функциями
        for i in 0..20 {
            fs::write(
                tmp.path().join(format!("module_{i}.py")),
                format!(
                    "def batch_func_{i}(x):\n    \"\"\"Функция батча {i}.\"\"\"\n    return x * {i}\n"
                ),
            )
            .unwrap();
        }

        let mut storage = Storage::open_in_memory().unwrap();

        // Устанавливаем маленький batch_size = 5, чтобы проверить несколько коммитов
        let config = IndexConfig {
            batch_size: 5,
            bulk_threshold: 100, // отключаем bulk-mode, чтобы проверять именно батч-транзакции
            ..Default::default()
        };

        let result = {
            let mut indexer = Indexer::with_config(&mut storage, config);
            indexer.full_reindex(tmp.path(), false).unwrap()
        };

        // Все 20 файлов должны быть успешно проиндексированы
        assert_eq!(result.files_indexed, 20, "все 20 файлов должны быть проиндексированы");
        assert_eq!(result.files_skipped, 0, "пропущенных файлов быть не должно");
        assert_eq!(result.errors.len(), 0, "ошибок быть не должно");

        // Данные реально записаны в БД — проверяем через get_stats
        let stats = storage.get_stats().unwrap();
        assert_eq!(stats.total_files, 20, "в БД должно быть 20 файлов");
        assert_eq!(stats.total_functions, 20, "по одной функции на файл");

        // FTS должен находить функции
        let found = storage.search_functions("batch_func_0", 10, None).unwrap();
        assert!(!found.is_empty(), "FTS должен находить batch_func_0");

        let found_19 = storage.search_functions("batch_func_19", 10, None).unwrap();
        assert!(!found_19.is_empty(), "FTS должен находить batch_func_19 (последний батч)");
    }

    #[test]
    fn test_parallel_reindex() {
        let tmp = TempDir::new().unwrap();

        // Создаём 30 Python-файлов с разными функциями
        for i in 0..30 {
            fs::write(
                tmp.path().join(format!("parallel_{i}.py")),
                format!(
                    "def parallel_func_{i}(a, b):\n    \"\"\"Параллельная функция {i}.\"\"\"\n    return a + b + {i}\n\ndef helper_{i}(x):\n    return x * {i}\n"
                ),
            )
            .unwrap();
        }

        let mut storage = Storage::open_in_memory().unwrap();
        let mut indexer = Indexer::new(&mut storage);
        let result = indexer.full_reindex(tmp.path(), false).unwrap();

        // Все 30 файлов проиндексированы
        assert_eq!(result.files_indexed, 30, "все 30 файлов должны быть проиндексированы");
        assert_eq!(result.files_skipped, 0, "пропущенных файлов быть не должно");
        assert_eq!(result.errors.len(), 0, "ошибок при параллельном парсинге быть не должно");

        // Проверяем что все функции на месте (по 2 на файл = 60 итого)
        let stats = storage.get_stats().unwrap();
        assert_eq!(stats.total_files, 30, "в БД должно быть 30 файлов");
        assert_eq!(stats.total_functions, 60, "по 2 функции на файл = 60 итого");

        // FTS находит функции из разных файлов (порядок парсинга не важен)
        let found_0 = storage.search_functions("parallel_func_0", 10, None).unwrap();
        assert!(!found_0.is_empty(), "FTS должен находить parallel_func_0");

        let found_15 = storage.search_functions("parallel_func_15", 10, None).unwrap();
        assert!(!found_15.is_empty(), "FTS должен находить parallel_func_15");

        let found_29 = storage.search_functions("parallel_func_29", 10, None).unwrap();
        assert!(!found_29.is_empty(), "FTS должен находить parallel_func_29");

        // helper-функции тоже проиндексированы
        let found_helper = storage.search_functions("helper_0", 10, None).unwrap();
        assert!(!found_helper.is_empty(), "FTS должен находить helper_0");
    }

    /// Тест: первичная индексация пустой БД в bulk-режиме.
    ///
    /// Проверяет, что при is_fresh_db=true + bulk_mode=true:
    /// - все файлы проиндексированы корректно
    /// - FTS-поиск работает после rebuild индексов
    /// - повторная индексация пропускает все неизменённые файлы
    #[test]
    fn test_bulk_fresh_db() {
        let tmp = TempDir::new().unwrap();

        // Создаём 20 Python-файлов с уникальными функциями
        for i in 0..20 {
            fs::write(
                tmp.path().join(format!("fresh_{i}.py")),
                format!(
                    "def fresh_func_{i}(x):\n    \"\"\"Свежая функция {i}.\"\"\"\n    return x + {i}\n"
                ),
            )
            .unwrap();
        }

        // Порог bulk_threshold=5 — при 20 файлах гарантированно активируется bulk-режим
        let config = IndexConfig {
            bulk_threshold: 5,
            ..Default::default()
        };

        let mut storage = Storage::open_in_memory().unwrap();

        // Первичная индексация пустой БД (is_fresh_db = true)
        let result = {
            let mut indexer = Indexer::with_config(&mut storage, config.clone());
            indexer.full_reindex(tmp.path(), false).unwrap()
        };

        assert_eq!(result.files_indexed, 20, "все 20 файлов должны быть проиндексированы");
        assert_eq!(result.files_skipped, 0, "пропущенных файлов быть не должно");
        assert_eq!(result.errors.len(), 0, "ошибок быть не должно");

        // Проверяем статистику
        let stats = storage.get_stats().unwrap();
        assert_eq!(stats.total_files, 20, "в БД должно быть 20 файлов");
        assert_eq!(stats.total_functions, 20, "по одной функции на файл");

        // Проверяем FTS-поиск после bulk rebuild
        let found_0 = storage.search_functions("fresh_func_0", 10, None).unwrap();
        assert!(!found_0.is_empty(), "FTS должен находить fresh_func_0 после bulk-load rebuild");

        let found_19 = storage.search_functions("fresh_func_19", 10, None).unwrap();
        assert!(!found_19.is_empty(), "FTS должен находить fresh_func_19 после bulk-load rebuild");

        // Повторная индексация (is_fresh_db = false) — все файлы должны быть пропущены
        let result2 = {
            let mut indexer = Indexer::with_config(&mut storage, config);
            indexer.full_reindex(tmp.path(), false).unwrap()
        };

        assert_eq!(result2.files_skipped, 20, "при повторной индексации все 20 файлов неизменны");
        assert_eq!(result2.files_indexed, 0, "ни одного файла не должно быть переиндексировано");

        // FTS по-прежнему работает после повторного прохода
        let found_after = storage.search_functions("fresh_func_10", 10, None).unwrap();
        assert!(!found_after.is_empty(), "FTS должен работать и после повторной индексации");
    }

    /// E2E framework-aware routing: индексация PHP-проекта (routes + controller),
    /// затем find_routes и обогащение get_symbol_context связанными маршрутами.
    #[test]
    fn test_framework_routing_end_to_end() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("routes")).unwrap();
        fs::create_dir_all(tmp.path().join("app")).unwrap();
        fs::write(
            tmp.path().join("routes/web.php"),
            "<?php\nuse Illuminate\\Support\\Facades\\Route;\n\
             Route::get('/users', [UserController::class, 'index']);\n\
             Route::post('/users', [UserController::class, 'store']);\n",
        ).unwrap();
        fs::write(
            tmp.path().join("app/UserController.php"),
            "<?php\nclass UserController {\n\
                 public function index() { return []; }\n\
                 public function store() { return null; }\n}\n",
        ).unwrap();

        let mut storage = Storage::open_in_memory().unwrap();
        {
            let mut indexer = Indexer::new(&mut storage);
            let r = indexer.full_reindex(tmp.path(), false).unwrap();
            assert!(r.errors.is_empty(), "ошибки индексации: {:?}", r.errors);
        }

        // find_routes возвращает оба маршрута
        let routes = storage.find_routes(None, None, None, 100).unwrap();
        assert_eq!(routes.len(), 2, "должно быть 2 маршрута");
        let get = routes.iter().find(|r| r.method == "GET").unwrap();
        assert_eq!(get.path, "/users");
        assert_eq!(get.handler_class.as_deref(), Some("UserController"));
        assert_eq!(get.handler_method.as_deref(), Some("index"));

        // фильтр по handler
        let by_handler = storage.find_routes(None, None, Some("store"), 100).unwrap();
        assert_eq!(by_handler.len(), 1);
        assert_eq!(by_handler[0].method, "POST");

        // get_symbol_context для контроллер-метода содержит связанный маршрут
        let ctx = storage.get_symbol_context("index", None, None).unwrap();
        assert_eq!(ctx.kind, "function");
        assert_eq!(ctx.routes.len(), 1, "index должен иметь 1 связанный маршрут");
        assert_eq!(ctx.routes[0].method, "GET");
        assert_eq!(ctx.routes[0].path, "/users");
    }

    /// vendor signatures: наследование от framework-класса (в vendor) резолвится
    /// для ООП-анализа, при этом vendor НЕ попадает в основной индекс (search/dead-code).
    #[test]
    fn test_vendor_signatures_resolve_framework_inheritance() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("app")).unwrap();
        fs::create_dir_all(tmp.path().join("vendor/framework")).unwrap();
        fs::write(
            tmp.path().join("vendor/framework/Controller.php"),
            "<?php\nclass Controller {\n public function boot() {}\n public function middleware() {}\n}\n",
        ).unwrap();
        fs::write(
            tmp.path().join("app/UserController.php"),
            "<?php\nclass UserController extends Controller {\n \
                 public function boot() { return 1; }\n \
                 public function customAction() { return 2; }\n}\n",
        ).unwrap();

        let mut storage = Storage::open_in_memory().unwrap();
        let config = IndexConfig { dependency_dirs: vec!["vendor".into()], ..Default::default() };
        Indexer::with_config(&mut storage, config).full_reindex(tmp.path(), false).unwrap();

        // vendor-сигнатуры наполнены, но базовый класс НЕ в основном индексе.
        assert!(storage.count_ext_classes() > 0, "ext_classes должны наполниться из vendor");
        assert!(storage.get_class_by_name("Controller").unwrap().is_empty(),
            "vendor-класс не должен попасть в основной индекс (search его не видит)");

        // OopModel резолвит framework-наследование через ext-сигнатуры.
        let oop = storage.build_oop_model().unwrap();
        assert!(oop.is_override("UserController", "boot"),
            "boot — override framework-метода Controller::boot (через vendor-сигнатуры)");
        assert!(!oop.is_override("UserController", "customAction"),
            "customAction не объявлен в предке → не override");

        // dead-code не помечает boot (полиморфный framework-hook).
        let dead = storage.find_dead_code(200, None, None).unwrap();
        assert!(!dead.iter().any(|d| d.qualified_name.as_deref() == Some("UserController::boot")),
            "boot не должен попадать в dead-code (override framework-метода)");
    }

    /// Переиндексация удаляет устаревшие маршруты файла (нет «фантомных» routes).
    #[test]
    fn test_routing_reindex_removes_stale_routes() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("routes")).unwrap();
        let web = tmp.path().join("routes/web.php");
        fs::write(&web, "<?php\nRoute::get('/a', [C::class, 'a']);\nRoute::get('/b', [C::class, 'b']);\n").unwrap();

        let mut storage = Storage::open_in_memory().unwrap();
        { let mut ix = Indexer::new(&mut storage); ix.full_reindex(tmp.path(), false).unwrap(); }
        assert_eq!(storage.find_routes(None, None, None, 100).unwrap().len(), 2);

        // Переписываем файл — остаётся один маршрут
        fs::write(&web, "<?php\nRoute::get('/a', [C::class, 'a']);\n").unwrap();
        { let mut ix = Indexer::new(&mut storage); ix.full_reindex(tmp.path(), true).unwrap(); }

        let routes = storage.find_routes(None, None, None, 100).unwrap();
        assert_eq!(routes.len(), 1, "после переиндексации остаётся 1 маршрут (старые удалены)");
        assert_eq!(routes[0].path, "/a");
    }
}
