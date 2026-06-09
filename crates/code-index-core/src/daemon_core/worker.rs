use tracing::{debug, info, warn};
// Worker одной отслеживаемой папки. Делает initial reindex + держит watcher-цикл.
//
// Работа полностью блокирующая (tree-sitter, rayon, notify), поэтому worker
// запускается из runner'а через `tokio::task::spawn_blocking`. Взаимодействие с
// tokio-миром только через `DaemonState` (асинхронный RwLock) и через
// `shutdown_rx` (broadcast).

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::Semaphore;

use crate::extension::ProcessorRegistry;
use crate::indexer::config::IndexConfig;
use crate::indexer::file_types::{categorize_file, FileCategory};
use crate::indexer::hasher;
use crate::indexer::Indexer;
use crate::parser::text::TextParser;
use crate::parser::ParserRegistry;
use crate::storage::memory::StorageConfig;
use crate::storage::Storage;
use crate::watcher::{create_watcher, poll_batch, FileEvent, WatcherConfig};

use super::cache_client::CacheClient;
use super::config::{IndexerSection, PathEntry};
use super::ipc::{PathStatus, Progress};
use super::state::DaemonState;
use crate::graph_store::{GraphCommand, GraphStoreHandle};

/// Выполнить initial reindex и запустить watcher-цикл для одной папки.
///
/// Функция блокирующая. Runner вызывает её через `spawn_blocking`. По завершении
/// (включая ошибку) статус папки уже записан в `DaemonState`.
///
/// `processor_registry` — список зарегистрированных `LanguageProcessor`-ов.
/// `None` — registry не задан, extension-tools отключены. В этом
/// случае пропускаем `apply_schema_extensions` / `index_extras`. В сборке
/// благодаря чему создаются специфичные таблицы (metadata_objects/...).
///
/// `cache_client` — клиент `mcp-cache-ci` для event-based invalidation
/// (этап 3, v0.9.1+). Если `None` или `is_empty()` — событийный канал не
/// используется, cache-ci работает только по TTL fallback. Если задан — после
/// каждого успешного `commit_batch()` worker асинхронно шлёт
/// `POST /invalidate {file_paths: [...]}` со списком файлов batch'а.
pub fn run_worker(
    entry: PathEntry,
    state: DaemonState,
    mut shutdown_rx: tokio::sync::broadcast::Receiver<()>,
    initial_limiter: Option<Arc<Semaphore>>,
    indexer_section: IndexerSection,
    processor_registry: Option<Arc<ProcessorRegistry>>,
    cache_client: Option<Arc<CacheClient>>,
    graph_handle: GraphStoreHandle,
) {
    let path = match entry.path.canonicalize() {
        Ok(p) => p,
        Err(e) => {
            tokio_block_on(async {
                state
                    .set_error(&entry.path, format!("Не удалось разрешить путь: {}", e))
                    .await;
            });
            return;
        }
    };

    // 1. Открыть/создать .icode/index.db
    let db_dir = path.join(".icode");
    if let Err(e) = std::fs::create_dir_all(&db_dir) {
        tokio_block_on(async {
            state
                .set_error(&path, format!("Создание .icode/: {}", e))
                .await;
        });
        return;
    }
    let db_path = db_dir.join("index.db");

    // 2. Загрузить конфигурацию проекта (для exclude_dirs, debounce и т.п.)
    let mut index_config = match IndexConfig::load(&path) {
        Ok(c) => c,
        Err(e) => {
            tokio_block_on(async {
                state
                    .set_error(&path, format!("Загрузка IndexConfig: {}", e))
                    .await;
            });
            return;
        }
    };
    // Phase 2 (v0.8.0): эффективный лимит для file_contents.
    // Приоритет: per-path (`[[paths]].max_code_file_size_bytes`) →
    // глобальный `[indexer].max_code_file_size_bytes` → hardcoded 5 МБ.
    // Перетираем дефолт IndexConfig — переоформленные правила сильнее JSON-конфига проекта.
    index_config.max_code_file_size_bytes = entry.effective_max_code_file_size(&indexer_section);
    let storage_config = StorageConfig {
        mode: index_config.storage_mode.clone(),
        memory_max_percent: index_config.memory_max_percent,
    };

    // 3. Взять permit из семафора. Permit держится на всё время initial reindex,
    // включая открытие in-memory Storage — чтобы в памяти одновременно жил
    // максимум ОДИН in-memory storage (ограничено max_concurrent_initial).
    let _permit = initial_limiter.as_ref().map(|sem| {
        info!("[worker:{}] ждём слота initial reindex (доступно {})", path.display(), sem.available_permits());
        let sem = sem.clone();
        tokio_block_on_value(async move { sem.acquire_owned().await.expect("semaphore closed") })
    });

    // 4. Выставить статус InitialIndexing ПОСЛЕ получения permit — иначе
    // папки-кандидаты показываются как активно индексируются, хотя на самом
    // деле ждут своей очереди.
    tokio_block_on(async {
        state.set_status(&path, PathStatus::InitialIndexing).await;
        state.set_progress(&path, Progress::new(0, 0)).await;
    });

    // 5. Открыть Storage.
    //    * Если БД уже существует — сразу disk-режим. fast-path почти ничего
    //      не пишет, нет лишнего backup memory→disk (WAL не раздувается).
    //    * Если БД новая (первый запуск на этой папке) — in-memory для
    //      скорости, потом flush на диск и reopen в disk для watcher'а.
    let db_existed_before = db_path.exists()
        && std::fs::metadata(&db_path).map(|m| m.len() > 0).unwrap_or(false);

    let mut storage = if db_existed_before {
        info!("[worker:{}] БД уже существует — открываем сразу в disk", path.display());
        match Storage::open_file(&db_path) {
            Ok(s) => s,
            Err(e) => {
                tokio_block_on(async {
                    state.set_error(&path, format!("Storage::open_file: {}", e)).await;
                });
                return;
            }
        }
    } else {
        info!("[worker:{}] новая БД — открываем в {}", path.display(), storage_config.mode);
        match Storage::open_auto(&db_path, &storage_config) {
            Ok(s) => s,
            Err(e) => {
                tokio_block_on(async {
                    state.set_error(&path, format!("Storage::open_auto: {}", e)).await;
                });
                return;
            }
        }
    };

    // 5a. Применить schema_extensions процессора, соответствующего этому репо.
    //     Двухступенчатый resolve: явный `language` из daemon.toml → fallback
    //     на auto-detect по маркерам корня. DDL идемпотентен (`IF NOT EXISTS`),
    //     повторный вызов на каждом старте безопасен.
    //
    //     `no such table: metadata_objects` (см. v0.8.0 регрессия —
    //     apply_schema_extensions раньше вызывался только в CLI-команде Index).
    let resolved_processor = processor_registry
        .as_ref()
        .and_then(|reg| reg.resolve(entry.language.as_deref(), &path).cloned());
    if let Some(proc) = resolved_processor.as_ref() {
        let exts = proc.schema_extensions();
        if !exts.is_empty() {
            if let Err(e) = storage.apply_schema_extensions(exts) {
                warn!(
                    "[worker:{}] apply_schema_extensions ('{}') упал: {}. \
                     Базовая индексация продолжится, но extension-tools могут не работать.",
                    path.display(), proc.name(), e
                );
            } else {
                info!(
                    "[worker:{}] schema_extensions процессора '{}' применены ({} DDL)",
                    path.display(), proc.name(), exts.len()
                );
            }
        }
    }

    info!("[worker:{}] initial reindex", path.display());

    // Версия билда: "major.minor.patch.build_number" (например "0.9.1.1748866200").
    // BUILD_NUMBER — Unix timestamp момента компиляции, всегда растёт.
    // Меняется при каждом cargo build → автоматическая переиндексация без ручного бампа.
    const MCP_VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), ".", env!("BUILD_NUMBER"));

    // Прочитать сохранённую версию из last_index.json (если есть).
    let saved_mcp_version: Option<String> = {
        let marker_path = db_dir.join("last_index.json");
        std::fs::read_to_string(&marker_path).ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .and_then(|v| v.get("mcp_version").and_then(|x| x.as_str()).map(String::from))
    };

    // 6. Принудительная переиндексация если:
    // а) БД пустая (прерванная индексация / неверный конфиг)
    // б) версия MCP изменилась с момента последней полной индексации
    let (force_reindex, force_reason) = if db_existed_before {
        let is_empty_db = match storage.get_stats() {
            Ok(stats) => stats.total_files == 0,
            Err(_) => true,
        };
        if is_empty_db {
            let has_files = walkdir::WalkDir::new(&path)
                .max_depth(3)
                .into_iter()
                .filter_entry(|e| {
                    let name = e.file_name().to_string_lossy();
                    !matches!(name.as_ref(), ".git" | "vendor" | "node_modules" | ".icode" | ".code-index" | "target")
                })
                .filter_map(|e| e.ok())
                .any(|e| e.file_type().is_file());
            if has_files {
                info!(
                    "[worker:{}] БД пустая (файлов: 0), но проект не пуст → принудительная переиндексация",
                    path.display()
                );
                (true, "empty_database_auto_reindex")
            } else {
                (false, "incremental")
            }
        } else if saved_mcp_version.as_deref() != Some(MCP_VERSION) {
            info!(
                "[worker:{}] версия MCP изменилась ({} → {}) → полная переиндексация",
                path.display(),
                saved_mcp_version.as_deref().unwrap_or("unknown"),
                MCP_VERSION,
            );
            (true, "version_upgrade")
        } else {
            (false, "incremental")
        }
    } else {
        (false, "incremental")
    };

    let indexer_result = {
        let repo_alias = entry.effective_alias();
        let mut indexer = Indexer::with_config(&mut storage, index_config.clone())
            .with_graph(graph_handle.clone(), repo_alias);
        indexer.full_reindex(&path, force_reindex)
    };
    let reindex_result = match indexer_result {
        Ok(result) => {
            info!(
                "[worker:{}] initial reindex: {} файлов за {} мс (записано {}, пропущено {}, удалено {})",
                path.display(),
                result.files_scanned,
                result.elapsed_ms,
                result.files_indexed,
                result.files_skipped,
                result.files_deleted
            );
            result
        }
        Err(e) => {
            tokio_block_on(async {
                state.set_error(&path, format!("full_reindex: {}", e)).await;
            });
            return;
        }
    };

    // 6b. Записать маркер последней полной индексации в .icode/last_index.json.
    //     Содержит время, статистику, причину и версию MCP.
    //     При следующем старте mcp_version сравнивается с текущей → автоматическая
    //     переиндексация если версия изменилась.
    {
        let reason = if !db_existed_before { "new_database" } else { force_reason };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let marker = format!(
            "{{\n  \"mcp_version\": \"{}\",\n  \"indexed_at_unix\": {},\n  \"files_scanned\": {},\n  \"files_indexed\": {},\n  \"files_skipped\": {},\n  \"elapsed_ms\": {},\n  \"reason\": \"{}\"\n}}\n",
            MCP_VERSION,
            now,
            reindex_result.files_scanned,
            reindex_result.files_indexed,
            reindex_result.files_skipped,
            reindex_result.elapsed_ms,
            reason,
        );
        let marker_path = db_dir.join("last_index.json");
        if let Err(e) = std::fs::write(&marker_path, &marker) {
            info!("[worker:{}] не удалось записать last_index.json: {}", path.display(), e);
        } else {
            info!("[worker:{}] last_index.json: version={}, reason={}, indexed={}", path.display(), MCP_VERSION, reason, reindex_result.files_indexed);
        }
    }

    // 6a. index_extras процессора (опционально)
    //     Forms / EventSubscriptions и заполнение metadata_*-таблиц.
    //
    //     ВАЖНО: вызывается ДО flush_to_disk. Если БД была новой и открыта
    //     in-memory — записи extras должны попасть в snapshot до сброса на
    //     диск, иначе исчезнут при reopen. Для disk-режима порядок не важен,
    //     но единый код проще.
    //
    //     Ошибка не фатальна: базовая индексация уже сохранена. Логируем и
    //     продолжаем — например, для репо без Configuration.xml (старая
    //     выгрузка обработок) парсер может ничего не найти и это нормально.
    if let Some(proc) = resolved_processor.as_ref() {
        if let Err(e) = proc.index_extras(&path, &mut storage) {
            warn!(
                "[worker:{}] index_extras процессора '{}' упал: {}. \
                 Базовая индексация при этом сохранена.",
                path.display(), proc.name(), e
            );
        } else {
            info!(
                "[worker:{}] index_extras процессора '{}' выполнен",
                path.display(), proc.name()
            );
        }
    }

    // 7. Если БД была новой и открылась в памяти — flush + reopen в disk.
    //    Если уже был disk — ничего делать не нужно, изменения уже на диске.
    if !db_existed_before {
        if let Err(e) = storage.flush_to_disk(&db_path) {
            warn!("[worker:{}] предупреждение: flush_to_disk: {}", path.display(), e);
        }
        drop(storage);
        storage = match Storage::open_file(&db_path) {
            Ok(s) => s,
            Err(e) => {
                tokio_block_on(async {
                    state.set_error(&path, format!("Storage::open_file (disk reopen): {}", e)).await;
                });
                return;
            }
        };
        info!("[worker:{}] переоткрыт в disk-режиме", path.display());
    }

    // Initial reindex мог накопить много страниц в WAL (особенно для больших
    // репо с 90k+ файлов). `PRAGMA wal_autocheckpoint=500` не гарантирует
    // физическое уменьшение файла — нужен явный TRUNCATE.
    match storage.checkpoint_truncate() {
        Ok((busy, log_pages, _)) if busy == 0 => {
            info!(
                "[worker:{}] post-initial WAL checkpoint: {} страниц вытеснено",
                path.display(), log_pages
            );
        }
        Ok((busy, _, _)) => {
            info!(
                "[worker:{}] post-initial WAL checkpoint: busy={} (частичный)",
                path.display(), busy
            );
        }
        Err(e) => {
            info!("[worker:{}] post-initial checkpoint_truncate: {}", path.display(), e);
        }
    }

    // 9. Отпустить permit — следующий worker может начинать initial reindex.
    drop(_permit);

    // 10. Перевести в Ready и запустить watcher
    tokio_block_on(async {
        state.set_status(&path, PathStatus::Ready).await;
    });

    // 8. Watcher-цикл
    let debounce_ms = entry.debounce_ms.unwrap_or(index_config.debounce_ms);
    let batch_ms = entry.batch_ms.unwrap_or(index_config.batch_ms);
    let watcher_config = WatcherConfig {
        debounce_ms,
        batch_ms,
        exclude_dirs: index_config.exclude_dirs.clone(),
        exclude_file_patterns: index_config.exclude_file_patterns.clone(),
    };
    let (watcher, rx) = match create_watcher(&path, &watcher_config) {
        Ok(pair) => pair,
        Err(e) => {
            tokio_block_on(async {
                state.set_error(&path, format!("create_watcher: {}", e)).await;
            });
            return;
        }
    };
    // Держим watcher на стеке — при drop watcher остановится.
    let _watcher = watcher;

    info!("[worker:{}] watcher активен (debounce={}ms, batch={}ms)",
        path.display(), debounce_ms, batch_ms);

    let registry = ParserRegistry::from_languages(&index_config.languages);
    // Эффективный лимит для file_contents — пробросим в apply_event,
    // чтобы Indexer::with_config не пересоздавался на каждое событие.
    let max_code_file_size = index_config.max_code_file_size_bytes;

    // Основной цикл обработки батчей. Idle-таймаут 500 мс даёт шанс проверить
    // shutdown-сигнал даже если файлов давно не меняли.
    const IDLE_POLL_MS: u64 = 500;
    loop {
        if shutdown_received(&mut shutdown_rx) {
            break;
        }

        let batch = match poll_batch(&rx, IDLE_POLL_MS, debounce_ms, batch_ms) {
            Ok(Some(b)) => {
                debug!("[worker:{}] batch: {} events", path.display(), b.len());
                b
            }
            Ok(None) => continue, // idle timeout — проверим shutdown на следующей итерации
            Err(_) => break,      // канал закрыт — watcher дропнут
        };
        if batch.is_empty() {
            continue;
        }

        tokio_block_on(async {
            state.set_status(&path, PathStatus::ReindexingBatch).await;
            state
                .set_progress(&path, Progress::new(0, batch.len()))
                .await;
        });

        if let Err(e) = storage.begin_batch() {
            warn!("[worker:{}] begin_batch: {}", path.display(), e);
            tokio_block_on(async {
                state.set_status(&path, PathStatus::Ready).await;
            });
            continue;
        }

        let repo_alias = entry.effective_alias();
        let mut done = 0usize;
        let batch_len = batch.len();
        for event in &batch {
            apply_event(&mut storage, &path, event, &registry, max_code_file_size, &graph_handle, &repo_alias);
            done += 1;
            if done % 50 == 0 || done == batch_len {
                tokio_block_on(async {
                    state
                        .set_progress(&path, Progress::new(done, batch_len))
                        .await;
                });
            }
        }

        let commit_ok = match storage.commit_batch() {
            Ok(()) => true,
            Err(e) => {
                warn!("[worker:{}] commit_batch: {}", path.display(), e);
                false
            }
        };
        // В disk-режиме (а worker сюда попадает всегда в disk после reopen на шаге 7)
        // flush_to_disk через Connection::backup() — бесполезное копирование БД самой
        // в себя, WAL не уменьшает. checkpoint_truncate реально схлопывает WAL.
        if let Err(e) = storage.checkpoint_truncate() {
            info!("[worker:{}] checkpoint_truncate: {}", path.display(), e);
        }

        // Event-based cache invalidation (v0.9.1+): после успешного commit
        // отправляем cache-ci список затронутых относительных путей. Если
        // commit упал — invalidate не шлём (новых данных в индексе нет;
        // cache-ci пусть отдаёт что было, TTL подстрахует).
        if commit_ok {
            if let Some(cc) = &cache_client {
                if !cc.is_empty() {
                    let paths_to_invalidate = collect_invalidate_paths(&path, &batch);
                    if !paths_to_invalidate.is_empty() {
                        let cc_clone = cc.clone();
                        tokio_block_on(async move {
                            cc_clone.invalidate_files(&paths_to_invalidate).await;
                        });
                    }
                }
            }
        }

        tokio_block_on(async {
            state.set_status(&path, PathStatus::Ready).await;
        });
    }

    info!("[worker:{}] shutdown, финальный checkpoint", path.display());
    if let Err(e) = storage.checkpoint_truncate() {
        info!("[worker:{}] финальный checkpoint_truncate: {}", path.display(), e);
    }
}

fn shutdown_received(rx: &mut tokio::sync::broadcast::Receiver<()>) -> bool {
    matches!(rx.try_recv(), Ok(()))
}

/// Собрать список относительных file_path из batch'а FS-событий для отправки
/// в `cache-ci` через `POST /invalidate {file_paths}`.
///
/// Используются ВСЕ типы событий — Modified/Created/Deleted: cache_entries,
/// зависящие от удалённого файла, также должны быть снесены. Дубликаты
/// (несколько событий по одному файлу в одном batch) дедуплицируются.
/// Пути приводятся к forward-slash формату (совпадает с тем, что daemon
/// записал в SQLite через `rel_path.replace('\\', "/")`).
fn collect_invalidate_paths(root: &PathBuf, batch: &[FileEvent]) -> Vec<String> {
    use std::collections::HashSet;
    let mut set: HashSet<String> = HashSet::new();
    for event in batch {
        let abs = match event {
            FileEvent::Modified(p) | FileEvent::Created(p) | FileEvent::Deleted(p) => p,
        };
        let rel = abs
            .strip_prefix(root)
            .unwrap_or(abs)
            .to_string_lossy()
            .replace('\\', "/");
        if !rel.is_empty() {
            set.insert(rel);
        }
    }
    set.into_iter().collect()
}

fn tokio_block_on<F: std::future::Future<Output = ()>>(fut: F) {
    tokio_block_on_value::<(), F>(fut);
}

fn tokio_block_on_value<T, F: std::future::Future<Output = T>>(fut: F) -> T {
    // Worker запускается внутри spawn_blocking, поэтому tokio runtime существует
    // и мы можем получить текущий handle.
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.block_on(fut)
    } else {
        // На случай запуска вне tokio (тесты) — собираем одноразовый runtime.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("create fallback tokio runtime");
        rt.block_on(fut)
    }
}

/// Обработать одно событие файловой системы: пересчитать хеш, записать/удалить в БД.
fn apply_event(
    storage: &mut Storage,
    root: &PathBuf,
    event: &FileEvent,
    registry: &ParserRegistry,
    max_code_file_size: usize,
    graph_handle: &GraphStoreHandle,
    repo_alias: &str,
) {
    match event {
        FileEvent::Modified(abs) | FileEvent::Created(abs) => {
            let (content, hash) = match hasher::file_hash(abs) {
                Ok(pair) => pair,
                Err(e) => {
                    // Частый случай: atomic-save через .tmp → rename. Watcher увидел
                    // событие на .tmp, но к моменту хэширования файл уже переименован.
                    // NotFound — не ошибка, тихо игнорируем.
                    if let Some(io_err) = e.downcast_ref::<std::io::Error>() {
                        if io_err.kind() == std::io::ErrorKind::NotFound {
                            return;
                        }
                    }
                    warn!("[worker:{}] file_hash {}: {}", root.display(), abs.display(), e);
                    return;
                }
            };

            let meta = std::fs::metadata(abs).ok();
            let mtime = meta.as_ref()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64);
            let file_size = meta.as_ref().map(|m| m.len() as i64);

            let rel_path = abs
                .strip_prefix(root)
                .unwrap_or(abs)
                .to_string_lossy()
                .replace('\\', "/");

            let category = categorize_file(abs);
            match category {
                FileCategory::Code(language) => {
                    let ext = abs
                        .extension()
                        .and_then(|e| e.to_str())
                        .unwrap_or("")
                        .to_lowercase();
                    if let Some(parser) = registry.get_parser(&ext) {
                        match parser.parse(&content, &rel_path) {
                            Ok(pr) => {
                                let indexer = Indexer::with_config(
                                    storage,
                                    IndexConfig {
                                        max_code_file_size_bytes: max_code_file_size,
                                        ..IndexConfig::default()
                                    },
                                )
                                .with_graph(graph_handle.clone(), repo_alias);
                                // v0.7.1: для html (и других dual-indexed языков) дополнительно пишем
                                // raw-content в text_files — чтобы search_text/grep_text/read_file
                                // продолжали работать как для обычного text-файла.
                                let text_for_fts = if crate::indexer::file_types::is_dual_indexed_language(&language) {
                                    Some(content.as_str())
                                } else {
                                    None
                                };
                                if let Err(e) = indexer.write_code_to_db(
                                    &rel_path,
                                    &hash,
                                    &language,
                                    pr.lines_total,
                                    &pr.ast_hash,
                                    &pr,
                                    false,
                                    mtime,
                                    file_size,
                                    text_for_fts,
                                    Some(content.as_str()),
                                ) {
                                    warn!("[worker:{}] write_code {}: {}",
                                        root.display(), rel_path, e);
                                }
                            }
                            Err(e) => warn!("[worker:{}] parse {}: {}",
                                root.display(), rel_path, e),
                        }
                    }
                }
                FileCategory::Text => {
                    let indexed_as_code = false;
                    if !indexed_as_code {
                        let tr = TextParser::parse(&content);
                        let text_lang = {
                            let ext = std::path::Path::new(&rel_path)
                                .extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
                            crate::indexer::file_types::text_language_for_ext(&ext).to_string()
                        };
                        let indexer = Indexer::new(storage);
                        if let Err(e) = indexer.write_text_to_db(
                            &rel_path,
                            &hash,
                            tr.lines_total,
                            &tr.content,
                            &text_lang,
                            false,
                            mtime,
                            file_size,
                        ) {
                            warn!("[worker:{}] write_text {}: {}",
                                root.display(), rel_path, e);
                        } else if graph_handle.is_enabled() {
                            graph_handle.send(GraphCommand::UpsertFile {
                                repo: repo_alias.to_string(),
                                path: rel_path.clone(),
                                language: text_lang.clone(),
                                hash: hash.clone(),
                            });
                        }
                    }
                }
                FileCategory::Binary => {}
            }
        }
        FileEvent::Deleted(abs) => {
            let rel_path = abs
                .strip_prefix(root)
                .unwrap_or(abs)
                .to_string_lossy()
                .replace('\\', "/");
            if let Ok(Some(file)) = storage.get_file_by_path(&rel_path) {
                if let Some(id) = file.id {
                    let _ = storage.delete_file(id);
                }
            }
        }
    }
}
