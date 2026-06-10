// Точка входа CLI — общая для бинарей `code-index` (публичный) и
// `bsl-indexer` (приватный, с BSL-расширением). Каждый бинарь зовёт
// `run(registry)`, передавая свой `ProcessorRegistry`: code-index —
// только встроенные процессоры, bsl-indexer — те же плюс
// `BslLanguageProcessor` из crate'а bsl-extension.

use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::extension::ProcessorRegistry;
use crate::graph_store::GraphClient;
use crate::indexer::config::IndexConfig;
use crate::indexer::Indexer;
use crate::storage::memory::StorageConfig;
use crate::storage::Storage;

#[derive(Parser)]
#[command(name = "icode", version, about = "Высокопроизводительный индексатор кода с MCP-протоколом")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Запустить MCP-сервер (read-only). Индексацию ведёт отдельный демон;
    /// этот режим используется Claude Code и другими клиентами как MCP-транспорт.
    ///
    /// Multi-repo: --path можно указать несколько раз в формате `alias=dir`,
    /// тогда в каждом tool-call LLM передаёт параметр `repo=<alias>` для выбора репо.
    /// Без `=` — одиночный репо под alias `default` (старый контракт).
    ///
    /// Примеры:
    ///   icode serve --path C:\RepoUT                          # single, alias=default
    ///   icode serve --path ut=C:\RepoUT --path bp=C:\RepoBP   # multi, alias=ut/bp
    Serve {
        /// Корневые директории проектов. Формат: `alias=dir` или просто `dir` (alias="default").
        /// Можно указать несколько раз. Если не указан ни `--path`, ни `--config` —
        /// берётся текущая директория с alias=default.
        #[arg(short, long, value_name = "ALIAS=DIR")]
        path: Vec<String>,

        /// Транспорт: `stdio` (per-session) или `http` (shared process под mcp-supervisor).
        #[arg(short, long, default_value = "stdio")]
        transport: String,

        /// HTTP: адрес биндинга. Если не задан и есть `serve.toml` —
        /// берётся `[me].ip`. Иначе по умолчанию `127.0.0.1`.
        #[arg(long)]
        host: Option<String>,

        /// HTTP: порт биндинга (используется только при `--transport http`).
        /// По умолчанию 8011 — следующий свободный после 8001/8002/8007/8010.
        #[arg(long, default_value_t = 8011)]
        port: u16,

        /// Путь к `daemon.toml` — подтянуть список репо и их алиасов из секции `[[paths]]`.
        /// Если указан и `--path` — CLI-пути имеют приоритет и конфиг игнорируется.
        #[arg(long, value_name = "FILE")]
        config: Option<PathBuf>,

        /// Путь к глобальному `serve.toml` (rc6+). Если не задан, ищется
        /// `$ICODE_HOME/serve.toml`. Если файл существует — включает
        /// федеративный режим: bind на `[me].ip`, IP-whitelist, форвард
        /// tool-call для удалённых репо.
        #[arg(long, value_name = "FILE")]
        serve_config: Option<PathBuf>,
    },

    /// Проиндексировать директорию (однократно)
    Index {
        /// Путь к директории
        path: String,

        /// Принудительная полная переиндексация (игнорировать хеши)
        #[arg(short, long)]
        force: bool,
    },

    /// Показать статистику базы данных
    Stats {
        /// Путь к корню проекта
        #[arg(short, long, default_value = ".")]
        path: String,

        /// Вывод в JSON вместо текста
        #[arg(long)]
        json: bool,
    },

    /// Быстрый поиск символа (функции, классы, переменные, импорты по точному имени)
    Query {
        /// Имя символа для поиска
        symbol: String,

        /// Путь к корню проекта
        #[arg(short, long, default_value = ".")]
        path: String,

        /// Фильтр по языку
        #[arg(short, long)]
        language: Option<String>,

        /// Вывод в JSON вместо текста
        #[arg(long)]
        json: bool,

        /// В JSON вернуть полные тела (по умолчанию lean: без тел функций/классов)
        #[arg(long)]
        include_body: bool,
    },

    /// Инициализировать проект для iCode: конфиг + индекс (+ .mcp.json).
    ///
    /// Достаточно запустить один раз в корне проекта:
    ///
    ///   cd ~/my-project && icode init
    ///
    /// После этого сразу работают `icode query`, `icode get-callers` и др.
    /// Для фонового авто-обновления индекса — `icode daemon run` / `icode setup`.
    Init {
        /// Путь к проекту
        #[arg(short, long, default_value = ".")]
        path: String,
        /// Не строить индекс (только создать конфиг и .mcp.json)
        #[arg(long)]
        no_index: bool,
        /// Не создавать .mcp.json (интеграция с Claude Code / MCP-клиентом)
        #[arg(long)]
        no_mcp: bool,
        /// Принудительно переиндексировать всё, даже неизменённые файлы
        #[arg(long)]
        force: bool,
    },

    /// Удалить из индекса файлы, которых нет на диске
    Clean {
        /// Путь к проекту
        #[arg(short, long, default_value = ".")]
        path: String,
    },

    /// Полнотекстовый поиск функций по имени/телу (FTS)
    SearchFunction {
        /// Поисковый запрос
        query: String,

        /// Путь к проекту
        #[arg(short, long, default_value = ".")]
        path: String,

        /// Фильтр по языку
        #[arg(short, long)]
        language: Option<String>,

        /// Максимум результатов
        #[arg(long, default_value = "20")]
        limit: usize,

        /// Вернуть полные тела (по умолчанию lean: сигнатура+строки+путь без тел)
        #[arg(long)]
        include_body: bool,
    },

    /// Полнотекстовый поиск классов по имени/телу (FTS)
    SearchClass {
        /// Поисковый запрос
        query: String,

        /// Путь к проекту
        #[arg(short, long, default_value = ".")]
        path: String,

        /// Фильтр по языку
        #[arg(short, long)]
        language: Option<String>,

        /// Максимум результатов
        #[arg(long, default_value = "20")]
        limit: usize,

        /// Вернуть полные тела (по умолчанию lean: имя+строки+путь+bases без тел)
        #[arg(long)]
        include_body: bool,
    },

    /// Получить функцию по точному имени
    GetFunction {
        /// Имя функции
        name: String,

        /// Путь к проекту
        #[arg(short, long, default_value = ".")]
        path: String,

        /// Фильтр по языку (не используется при точном поиске, для совместимости)
        #[arg(short, long)]
        language: Option<String>,
    },

    /// Получить класс по точному имени
    GetClass {
        /// Имя класса
        name: String,

        /// Путь к проекту
        #[arg(short, long, default_value = ".")]
        path: String,

        /// Фильтр по языку (не используется при точном поиске, для совместимости)
        #[arg(short, long)]
        language: Option<String>,
    },

    /// Кто вызывает данную функцию (callers)
    GetCallers {
        /// Имя функции
        function_name: String,

        /// Путь к проекту
        #[arg(short, long, default_value = ".")]
        path: String,

        /// Фильтр по языку
        #[arg(short, long)]
        language: Option<String>,

        /// Максимум рёбер (по умолчанию 50)
        #[arg(long, default_value = "50")]
        limit: usize,
    },

    /// Что вызывает данная функция (callees)
    GetCallees {
        /// Имя функции
        function_name: String,

        /// Путь к проекту
        #[arg(short, long, default_value = ".")]
        path: String,

        /// Фильтр по языку
        #[arg(short, long)]
        language: Option<String>,

        /// Максимум рёбер (по умолчанию 50)
        #[arg(long, default_value = "50")]
        limit: usize,
    },

    /// Получить импорты файла или модуля
    GetImports {
        /// Путь к проекту
        #[arg(short, long, default_value = ".")]
        path: String,

        /// ID файла в индексе
        #[arg(long)]
        file_id: Option<i64>,

        /// Имя модуля
        #[arg(short, long)]
        module: Option<String>,

        /// Фильтр по языку
        #[arg(short, long)]
        language: Option<String>,
    },

    /// Карта файла: все функции, классы, импорты, переменные
    GetFileSummary {
        /// Путь к файлу (как в индексе)
        file: String,

        /// Путь к проекту
        #[arg(short, long, default_value = ".")]
        path: String,
    },

    /// Полнотекстовый поиск по текстовым файлам
    SearchText {
        /// Поисковый запрос
        query: String,

        /// Путь к проекту
        #[arg(short, long, default_value = ".")]
        path: String,

        /// Фильтр по языку
        #[arg(short, long)]
        language: Option<String>,

        /// Максимум результатов
        #[arg(long, default_value = "20")]
        limit: usize,
    },

    /// Поиск подстроки или regex в телах функций и классов (в отличие от FTS, поддерживает точки и спецсимволы)
    GrepBody {
        /// Путь к проекту
        #[arg(short, long, default_value = ".")]
        path: String,

        /// Буквальная подстрока для поиска (LIKE). Поддерживает точки и спецсимволы.
        #[arg(long)]
        pattern: Option<String>,

        /// Регулярное выражение для поиска (REGEXP). Альтернатива --pattern.
        #[arg(long)]
        regex: Option<String>,

        /// Фильтр по языку (bsl, python, rust, java, go, javascript, typescript)
        #[arg(short, long)]
        language: Option<String>,

        /// Максимум результатов
        #[arg(long, default_value = "100")]
        limit: usize,
    },

    /// Архитектурная карта репозитория за один вызов (counts, modules,
    /// complex_functions, call_hotspots, entry_points, parse_errors)
    RepoMap {
        /// Путь к проекту
        #[arg(short, long, default_value = ".")]
        path: String,
        /// Размер каждой секции
        #[arg(long, default_value = "12")]
        top: usize,
    },

    /// Проверить, нет ли уже такой функции/класса (перед написанием нового кода)
    FindExisting {
        /// Имя или описание того, что собираешься написать
        query: String,
        /// Путь к проекту
        #[arg(short, long, default_value = ".")]
        path: String,
        /// function | class | all
        #[arg(short, long, default_value = "all")]
        kind: String,
        /// Фильтр по языку
        #[arg(short, long)]
        language: Option<String>,
        /// Максимум результатов
        #[arg(long, default_value = "15")]
        limit: usize,
    },

    /// Недостижимый код: обход call-графа от точек входа (маршруты/main/тесты)
    Unreachable {
        /// Путь к проекту
        #[arg(short, long, default_value = ".")]
        path: String,
        /// Максимум результатов
        #[arg(long, default_value = "50")]
        limit: usize,
        /// Фильтр по языку
        #[arg(short, long)]
        language: Option<String>,
        /// Glob по пути (`app/**/*.php`)
        #[arg(long)]
        path_glob: Option<String>,
    },

    /// Функции по сложности (длина + fan-out + fan-in) — что рефакторить/ревьюить
    Complex {
        /// Путь к проекту
        #[arg(short, long, default_value = ".")]
        path: String,
        /// Максимум результатов
        #[arg(long, default_value = "15")]
        limit: usize,
        /// Фильтр по языку
        #[arg(short, long)]
        language: Option<String>,
        /// Glob по пути (`app/**/*.php`)
        #[arg(long)]
        path_glob: Option<String>,
    },

    /// Сверить индекс с рабочим деревом: пропущено / устарело / удалено + слепые зоны
    Doctor {
        /// Путь к проекту
        #[arg(short, long, default_value = ".")]
        path: String,
        /// Вывод в JSON
        #[arg(long)]
        json: bool,
    },

    /// Управление фоновым демоном индексации
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },

    /// Интерактивная настройка iCode для проекта.
    ///
    /// Создаёт ICODE_HOME, daemon.toml, .mcp.json, устанавливает autostart-сервис.
    /// Достаточно запустить один раз в корне проекта:
    ///
    ///   cd ~/my-project && icode setup
    Setup {
        /// Корень проекта (по умолчанию — текущая директория)
        #[arg(short, long, default_value = ".")]
        path: String,

        /// Папка для данных iCode.
        /// По умолчанию: ~/.local/icode (Linux/macOS), %APPDATA%\icode (Windows).
        /// Можно переопределить через переменную окружения ICODE_HOME.
        #[arg(long, value_name = "DIR")]
        icode_home: Option<String>,
    },
}

#[derive(Subcommand)]
enum DaemonAction {
    /// Запустить демон в foreground (вызывается Scheduled Task / systemd / launchd)
    Run,

    /// Показать статус демона (GET /health)
    Status {
        /// Вывод в JSON вместо человекочитаемого текста
        #[arg(long)]
        json: bool,
    },

    /// Попросить демон перечитать конфиг (POST /reload)
    Reload,

    /// Остановить демон (POST /stop)
    Stop,
}

/// Получить путь к БД для проекта
fn get_db_path(project_path: &str) -> PathBuf {
    let root = Path::new(project_path)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(project_path));
    root.join(".icode").join("index.db")
}

/// Собрать список (alias, root, db_path) для MCP-сервера.
///
/// Порядок источников:
/// 1. Если передан `--path` — используем CLI-аргументы (старый контракт).
/// 2. Иначе если указан `--config` — берём секцию `[[paths]]` из daemon.toml,
///    алиас вычисляется через [`PathEntry::effective_alias`].
/// 3. Иначе — текущая директория под alias=default.
///
/// Параллельно создаём пустую `.code-index/index.db` со схемой, чтобы MCP-сервер
/// мог открыть read-only до того, как демон проиндексирует путь.
fn build_repo_entries(
    cli_paths: Vec<String>,
    config_path: Option<&Path>,
) -> anyhow::Result<Vec<(String, PathBuf, PathBuf)>> {
    // (alias, dir)
    let pairs: Vec<(String, String)> = if !cli_paths.is_empty() {
        let mut out = Vec::with_capacity(cli_paths.len());
        for raw in cli_paths {
            if let Some(eq_idx) = raw.find('=') {
                let alias = raw[..eq_idx].trim().to_string();
                let dir = raw[eq_idx + 1..].to_string();
                if alias.is_empty() {
                    return Err(anyhow::anyhow!(
                        "Пустой alias в --path '{}'. Формат: alias=dir.",
                        raw
                    ));
                }
                out.push((alias, dir));
            } else {
                out.push(("default".to_string(), raw));
            }
        }
        out
    } else if let Some(cfg_path) = config_path {
        let cfg = crate::daemon_core::config::load_from(cfg_path)?;
        if cfg.paths.is_empty() {
            return Err(anyhow::anyhow!(
                "В {} нет ни одной секции [[paths]] — укажите --path или добавьте пути в конфиг.",
                cfg_path.display()
            ));
        }
        cfg.paths
            .iter()
            .map(|p| (p.effective_alias(), p.path.to_string_lossy().into_owned()))
            .collect()
    } else {
        vec![("default".to_string(), ".".to_string())]
    };

    let mut entries: Vec<(String, PathBuf, PathBuf)> = Vec::with_capacity(pairs.len());
    let mut seen_aliases = std::collections::HashSet::new();
    for (alias, dir) in pairs {
        if !seen_aliases.insert(alias.clone()) {
            return Err(anyhow::anyhow!(
                "Алиас репо '{}' указан дважды — алиасы должны быть уникальны.",
                alias
            ));
        }

        let root = Path::new(&dir)
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(&dir));
        let db_path = root.join(".icode").join("index.db");

        // Если БД ещё нет — создаём пустую со схемой, чтобы сервер мог стартовать.
        // Данные появятся, когда демон проиндексирует путь.
        if !db_path.exists() {
            if let Some(parent) = db_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let storage = Storage::open_file(&db_path)?;
            drop(storage);
        }

        tracing::info!("MCP repo: {} -> {}", alias, root.display());
        entries.push((alias, root, db_path));
    }

    Ok(entries)
}

/// Запуск MCP-сервера по HTTP (Streamable HTTP) на `host:port`.
///
/// Роут `/mcp` — точка подключения MCP-клиента (url из `.mcp.json`).
/// При федеративном режиме (`federate_router = Some(...)`) добавляется
/// `/federate/<tool>` и оба роута оборачиваются IP-whitelist middleware.
/// `LocalSessionManager` держит сессии in-memory. На каждую сессию фабрика
/// клонирует уже собранный `CodeIndexServer` (он реализует `Clone`), так что
/// все сессии разделяют общий набор открытых SQLite-баз.
async fn handle_serve(
    path: Vec<String>,
    transport: String,
    host: Option<String>,
    port: u16,
    config: Option<PathBuf>,
    serve_config: Option<PathBuf>,
    registry: &mut Option<crate::extension::ProcessorRegistry>,
) -> anyhow::Result<()> {
    use crate::federation;
    use crate::mcp::CodeIndexServer;

    let serve_cfg_path: Option<PathBuf> = if transport == "http" && path.is_empty() {
        if let Some(p) = serve_config.clone() {
            if !p.exists() {
                return Err(anyhow::anyhow!("--serve-config={} не существует.", p.display()));
            }
            Some(p)
        } else {
            let p = federation::config::default_path()?;
            if p.exists() { Some(p) } else { None }
        }
    } else {
        None
    };

    if let Some(serve_cfg_path) = serve_cfg_path {
        tracing::info!("Федеративный режим: serve.toml={}", serve_cfg_path.display());
        let serve_cfg = federation::config::load_from(&serve_cfg_path)?;
        let daemon_cfg = match config.as_deref() {
            Some(p) => crate::daemon_core::config::load_from(p)?,
            None => crate::daemon_core::config::load_or_default()?,
        };
        for daemon_entry in &daemon_cfg.paths {
            let root = daemon_entry.path.canonicalize().unwrap_or_else(|_| daemon_entry.path.clone());
            let db_path = root.join(".icode").join("index.db");
            if !db_path.exists() {
                if let Some(parent) = db_path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                drop(Storage::open_file(&db_path)?);
            }
        }
        let repos = federation::repos::merge(&serve_cfg, &daemon_cfg)?;
        tracing::info!("Реестр федерации: {} репо ({} local): {:?}",
            repos.len(), repos.iter().filter(|r| r.is_local).count(),
            repos.iter().map(|r| r.alias.as_str()).collect::<Vec<_>>());
        let local_languages = daemon_cfg.paths.iter()
            .filter_map(|p| p.language.as_ref().map(|l| (p.effective_alias(), l.clone())))
            .collect();
        let mut server = CodeIndexServer::from_federated(repos, serve_cfg.me.ip.clone(), registry.take(), local_languages)?;
        if let Some(gc) = connect_graph_client(config.as_deref()).await {
            server = server.with_graph_client(gc);
        }
        let federate_router = federation::server::federate_router(server.clone());
        let allowed = std::sync::Arc::new(federation::whitelist::build(&serve_cfg));
        let _config_watch = config.as_deref().map(|p| crate::mcp::config_watch::spawn_watch(server.clone(), p.into()));
        let bind_host = host.unwrap_or_else(|| serve_cfg.me.ip.clone());
        serve_http(server, &bind_host, port, Some(federate_router), Some(allowed)).await?;
        return Ok(());
    }

    // Моно-режим
    let entries = build_repo_entries(path, config.as_deref())?;
    tracing::info!("MCP read-only ({}), репо: {:?}", transport,
        entries.iter().map(|(a, _, _)| a.as_str()).collect::<Vec<_>>());
    let server = match registry.take() {
        Some(reg) => {
            // Build alias -> language map from daemon.toml (if --config provided)
            let lang_map: std::collections::HashMap<String, String> = if let Some(cfg_path) = config.as_deref() {
                match crate::daemon_core::config::load_from(cfg_path) {
                    Ok(daemon_cfg) => daemon_cfg.paths.iter()
                        .filter_map(|p| p.language.as_ref().map(|l| (p.effective_alias(), l.clone())))
                        .collect(),
                    Err(_) => std::collections::HashMap::new(),
                }
            } else {
                std::collections::HashMap::new()
            };
            let mut map = std::collections::BTreeMap::new();
            for (alias, root_path, db_path) in entries {
                let storage = Storage::open_file_readonly(&db_path)?;
                let language = lang_map.get(&alias).cloned();
                map.insert(alias.clone(), crate::mcp::RepoEntry {
                    alias, root_path: Some(root_path),
                    storage: Some(std::sync::Arc::new(tokio::sync::Mutex::new(storage))),
                    ip: "127.0.0.1".to_string(),
                    port: crate::federation::client::DEFAULT_REMOTE_PORT,
                    is_local: true, language,
                });
            }
            CodeIndexServer::with_repos_and_registry(map, reg)
        }
        None => CodeIndexServer::open_readonly_multi(entries)?,
    };
    let server = if let Some(gc) = connect_graph_client(config.as_deref()).await {
        server.with_graph_client(gc)
    } else { server };
    let bind_host = host.unwrap_or_else(|| "127.0.0.1".to_string());
    let _config_watch = config.as_deref().map(|p| crate::mcp::config_watch::spawn_watch(server.clone(), p.into()));

    match transport.as_str() {
        "stdio" => {
            use rmcp::ServiceExt;
            server.serve(rmcp::transport::io::stdio()).await
                .map_err(|e| anyhow::anyhow!("MCP serve error: {}", e))?
                .waiting().await
                .map_err(|e| anyhow::anyhow!("MCP wait error: {}", e))?;
        }
        "http" => serve_http(server, &bind_host, port, None, None).await?,
        other => return Err(anyhow::anyhow!(
            "Транспорт '{}' не поддерживается. Используйте 'stdio' или 'http'.", other
        )),
    }
    Ok(())
}

async fn handle_index(
    path: String,
    force: bool,
    registry: &Option<crate::extension::ProcessorRegistry>,
) -> anyhow::Result<()> {
    tracing::info!("Индексация: path={}, force={}", path, force);
    let abs_path = Path::new(&path).canonicalize().unwrap_or_else(|_| PathBuf::from(&path));
    let db_dir = abs_path.join(".icode");
    std::fs::create_dir_all(&db_dir)
        .map_err(|e| anyhow::anyhow!("Не удалось создать директорию {:?}: {}", db_dir, e))?;
    let db_path = db_dir.join("index.db");
    let config = IndexConfig::load(&abs_path)?;
    let storage_config = StorageConfig { mode: config.storage_mode.clone(), memory_max_percent: config.memory_max_percent };
    let mut storage = Storage::open_auto(&db_path, &storage_config)?;

    if let Some(reg) = registry {
        if let Some(proc) = reg.resolve(None, &abs_path) {
            let exts = proc.schema_extensions();
            if !exts.is_empty() {
                storage.apply_schema_extensions(exts)?;
                tracing::info!("schema_extensions '{}' applied ({} DDL)", proc.name(), exts.len());
            }
        }
    }

    let result = Indexer::with_config(&mut storage, config).full_reindex(&abs_path, force)?;

    if let Some(reg) = registry {
        if let Some(proc) = reg.resolve(None, &abs_path) {
            if let Err(e) = proc.index_extras(&abs_path, &mut storage) {
                tracing::warn!("index_extras '{}': {} (базовая индексация сохранена)", proc.name(), e);
            }
        }
    }

    storage.flush_to_disk(&db_path)?;

    println!("Индексация завершена за {} мс", result.elapsed_ms);
    println!("  Найдено:       {}", result.files_scanned);
    println!("  Записано:      {}", result.files_indexed);
    println!("  Пропущено:     {}", result.files_skipped);
    println!("  Удалено:       {}", result.files_deleted);
    if !result.errors.is_empty() {
        println!("  Ошибок:        {}", result.errors.len());
        for (f, e) in &result.errors { println!("    [ERR] {}: {}", f, e); }
    }
    Ok(())
}

/// Разрешить ICODE_HOME: явный аргумент → переменная окружения → дефолт по ОС.
fn resolve_icode_home(arg: Option<&str>) -> PathBuf {
    if let Some(h) = arg {
        return PathBuf::from(h);
    }
    if let Ok(env_val) = std::env::var("ICODE_HOME") {
        return PathBuf::from(env_val);
    }
    #[cfg(windows)]
    let base = std::env::var("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("C:\\icode"));
    #[cfg(not(windows))]
    let base = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".local");
    base.join("icode")
}

/// `icode init` — одношаговая инициализация проекта: конфиг + .mcp.json + индекс.
///
/// Идемпотентна: существующие конфиг/`.mcp.json` не перезаписываются, индексация
/// пропускает неизменённые файлы. После запуска сразу работают query-команды.
async fn handle_init(
    path: String,
    no_index: bool,
    no_mcp: bool,
    force: bool,
    registry: &Option<crate::extension::ProcessorRegistry>,
) -> anyhow::Result<()> {
    let abs_path = Path::new(&path)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(&path));
    println!("\n🚀 iCode init — {}\n", abs_path.display());

    // 1. Конфиг проекта (.icode/config.json)
    let config_path = abs_path.join(".icode").join("config.json");
    if config_path.exists() {
        println!("✓ конфиг уже существует: {}", config_path.display());
    } else {
        IndexConfig::default().save(&abs_path)?;
        println!("✓ создан конфиг: {}", config_path.display());
    }

    // 2. .mcp.json для интеграции с Claude Code / MCP-клиентом (skip если есть).
    if no_mcp {
        println!("⏭  .mcp.json пропущен (--no-mcp)");
    } else {
        let icode_home = resolve_icode_home(None);
        let _ = std::fs::create_dir_all(&icode_home);
        let binary = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("icode"));
        setup_mcp_json(&abs_path, &binary, &icode_home)?;
    }

    // 3. Индекс (по умолчанию строим сразу, чтобы query-команды работали).
    if no_index {
        println!("⏭  индекс пропущен (--no-index). Постройте позже: icode index .");
    } else {
        println!("\n▶ строим индекс...");
        handle_index(path, force, registry).await?;
    }

    // 4. Подсказки по дальнейшим командам.
    println!("\n✅ Проект инициализирован.\n");
    println!("Команды поверх индекса (читают .icode/index.db напрямую, демон не нужен):");
    println!("  icode query <symbol>           — где определён символ (функции/классы/...)");
    println!("  icode get-callers <fn>         — кто вызывает функцию");
    println!("  icode get-callees <fn>         — что вызывает функция");
    println!("  icode search-function <q>      — FTS-поиск функций");
    println!("  icode grep-body --pattern <p>  — поиск по телам функций/классов");
    println!("  icode stats                    — статистика индекса");
    println!("  icode clean                    — убрать из индекса удалённые файлы");
    println!();
    println!("Фоновый режим (авто-обновление индекса + MCP-сервер):");
    println!("  icode daemon run               — фоновый индексатор (один писатель)");
    println!("  icode setup                    — daemon.toml + autostart (systemd/launchd)");
    println!("  icode serve --path .           — MCP-сервер (stdio) для AI-клиента");
    println!();
    Ok(())
}

/// Подключиться к граф-БД если в конфиге есть секция `[graph]`.
/// Возвращает `Arc<GraphClient>` или `None` при ошибке (не фатально).
async fn connect_graph_client(
    config_path: Option<&Path>,
) -> Option<Arc<GraphClient>> {
    let cfg = match config_path {
        Some(p) => crate::daemon_core::config::load_from(p).ok()?,
        None => crate::daemon_core::config::load_or_default().ok()?,
    };
    let graph_cfg = cfg.graph?;
    if graph_cfg.password.is_empty() {
        tracing::warn!("граф-БД: password не задан");
    }
    match GraphClient::connect(&graph_cfg.bolt_url, &graph_cfg.username, &graph_cfg.password).await {
        Ok(client) => {
            tracing::info!("граф-БД: подключён к {}", redact_url(&graph_cfg.bolt_url));
            Some(Arc::new(client))
        }
        Err(e) => {
            tracing::warn!("граф-БД: ошибка подключения к {} — {}. Граф-инструменты недоступны.", redact_url(&graph_cfg.bolt_url), e);
            None
        }
    }
}

fn redact_url(url: &str) -> String {
    if let Some(at) = url.rfind('@') {
        format!("bolt://***{}", &url[at..])
    } else {
        url.to_string()
    }
}

async fn serve_http(
    server: crate::mcp::CodeIndexServer,
    host: &str,
    port: u16,
    federate_router: Option<axum::Router>,
    whitelist: Option<std::sync::Arc<std::collections::HashSet<std::net::IpAddr>>>,
) -> anyhow::Result<()> {
    use rmcp::transport::streamable_http_server::{
        session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
    };
    use std::net::SocketAddr;
    use std::sync::Arc;

    let session_manager = Arc::new(LocalSessionManager::default());
    let svc_server = server.clone();
    let http_service = StreamableHttpService::new(
        move || Ok(svc_server.clone()),
        session_manager,
        StreamableHttpServerConfig::default(),
    );

    let mut app = axum::Router::new().nest_service("/mcp", http_service);
    if let Some(fr) = federate_router {
        app = app.merge(fr);
    }
    if let Some(allowed) = whitelist {
        let count = allowed.len();
        app = app.layer(axum::middleware::from_fn_with_state(
            allowed,
            crate::federation::whitelist::middleware,
        ));
        tracing::info!("IP-whitelist активен ({} адресов, включая loopback).", count);
    }

    let addr: SocketAddr = format!("{}:{}", host, port)
        .parse()
        .map_err(|e| anyhow::anyhow!("Некорректный host:port '{}:{}': {}", host, port, e))?;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| anyhow::anyhow!("Не удалось привязаться к {}: {}", addr, e))?;

    tracing::info!("MCP HTTP слушает http://{}/mcp", addr);
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .map_err(|e| anyhow::anyhow!("axum serve error: {}", e))?;
    Ok(())
}

/// Точка входа для бинарных wrapper'ов. Принимает уже собранный реестр
/// `LanguageProcessor`-ов: каждый bin собирает его сам (`code-index` —
/// только встроенные, `bsl-indexer` — встроенные + BSL).
///
/// Регистрируется логирование, парсятся CLI-аргументы и происходит
/// выполнение соответствующей подкоманды. Не возвращает Ok пока
/// демон/сервер живут — это long-running процесс.
pub async fn run(registry: ProcessorRegistry) -> anyhow::Result<()> {
    // Инициализация логирования. tracing_subscriber idempotent при
    // повторных вызовах — если bin уже что-то настроил, второй вызов
    // вернёт ошибку, которую мы игнорируем (для тестов это норма).
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .with_writer(std::io::stderr)
        .try_init();

    let cli = Cli::parse();
    // На текущем этапе registry приходит только в Serve через
    // `with_repos_and_registry` или `from_federated`. Остальные команды
    // (Index/Stats/...) её не используют. Чтобы не плодить копии,
    // сохраняем в локальной переменной и тащим в Serve. `mut` нужен,
    // чтобы федеративная ветка могла забрать registry через `take()`,
    // не ломая компиляцию `match registry` в моно-ветке.
    let mut registry = Some(registry);

    match cli.command {
        Commands::Serve { path, transport, host, port, config, serve_config } => {
            handle_serve(path, transport, host, port, config, serve_config, &mut registry).await?;
        }

        Commands::Index { path, force } => {
            handle_index(path, force, &registry).await?;
        }

        Commands::Stats { path, json } => {
            tracing::info!("Статистика: path={}", path);

            // 1. Открыть БД (только чтение — не конкурирует с MCP-демоном)
            let db_path = get_db_path(&path);
            let storage = Storage::open_file_readonly(&db_path)?;

            // 2. Получить статистику
            let stats = storage.get_stats()?;

            if json {
                // JSON-формат для программного использования
                println!("{}", serde_json::to_string_pretty(&stats)?);
            } else {
                // Текстовый формат для человека
                println!("Статистика индекса: {}", db_path.display());
                println!("─────────────────────────────────────");
                println!("  Файлов:        {}", stats.total_files);
                println!("  Функций:       {}", stats.total_functions);
                println!("  Классов:       {}", stats.total_classes);
                println!("  Импортов:      {}", stats.total_imports);
                println!("  Вызовов:       {}", stats.total_calls);
                println!("  Переменных:    {}", stats.total_variables);
                println!("  Текст. файлов: {}", stats.total_text_files);
            }
        }

        Commands::Query { symbol, path, language, json, include_body } => {
            tracing::info!("Поиск символа '{}': path={}", symbol, path);

            // 1. Открыть БД (только чтение — не конкурирует с MCP-демоном)
            let db_path = get_db_path(&path);
            let storage = Storage::open_file_readonly(&db_path)?;

            // 2. Поиск символа
            let result = storage.find_symbol(&symbol, language.as_deref())?;

            if json {
                if include_body {
                    println!("{}", serde_json::to_string_pretty(&result)?);
                } else {
                    // lean по умолчанию: без тел — экономит токены
                    let lean = crate::storage::models::SymbolSearchLean {
                        functions: result.functions.iter()
                            .map(|f| crate::storage::models::FunctionHit::from_record(
                                f, storage.get_path_by_file_id(f.file_id).ok().flatten().unwrap_or_default()))
                            .collect(),
                        classes: result.classes.iter()
                            .map(|c| crate::storage::models::ClassHit::from_record(
                                c, storage.get_path_by_file_id(c.file_id).ok().flatten().unwrap_or_default()))
                            .collect(),
                        variables: result.variables.clone(),
                        imports: result.imports.clone(),
                    };
                    println!("{}", serde_json::to_string_pretty(&lean)?);
                }
                return Ok(());
            }

            let total = result.functions.len()
                + result.classes.len()
                + result.variables.len()
                + result.imports.len();

            if total == 0 {
                println!("Символ '{}' не найден в индексе.", symbol);
                return Ok(());
            }

            println!("Результаты поиска символа '{}':", symbol);

            // 3. Функции
            if !result.functions.is_empty() {
                println!("\n  Функции ({}):", result.functions.len());
                for f in &result.functions {
                    let qname = f.qualified_name.as_deref().unwrap_or(&f.name);
                    let async_mark = if f.is_async { " [async]" } else { "" };
                    let args = f.args.as_deref().unwrap_or("()");
                    println!(
                        "    {}{}  {}  строки {}-{}  (file_id={})",
                        qname, async_mark, args, f.line_start, f.line_end, f.file_id
                    );
                }
            }

            // 4. Классы
            if !result.classes.is_empty() {
                println!("\n  Классы ({}):", result.classes.len());
                for c in &result.classes {
                    let bases = c.bases.as_deref().unwrap_or("");
                    let bases_str = if bases.is_empty() {
                        String::new()
                    } else {
                        format!(" ({})", bases)
                    };
                    println!(
                        "    {}{}  строки {}-{}  (file_id={})",
                        c.name, bases_str, c.line_start, c.line_end, c.file_id
                    );
                }
            }

            // 5. Переменные
            if !result.variables.is_empty() {
                println!("\n  Переменные ({}):", result.variables.len());
                for v in &result.variables {
                    let val = v.value.as_deref().unwrap_or("<нет значения>");
                    println!(
                        "    {}  =  {}  строка {}  (file_id={})",
                        v.name, val, v.line, v.file_id
                    );
                }
            }

            // 6. Импорты
            if !result.imports.is_empty() {
                println!("\n  Импорты ({}):", result.imports.len());
                for i in &result.imports {
                    let module = i.module.as_deref().unwrap_or("?");
                    let name = i.name.as_deref().unwrap_or("*");
                    let alias_str = match &i.alias {
                        Some(a) => format!(" as {}", a),
                        None => String::new(),
                    };
                    println!(
                        "    {} from {}{}  строка {}  (file_id={})",
                        name, module, alias_str, i.line, i.file_id
                    );
                }
            }
        }

        Commands::Clean { path } => {
            tracing::info!("Очистка индекса: path={}", path);

            // 1. Открыть БД
            let db_path = get_db_path(&path);
            let storage = Storage::open_file(&db_path)?;

            // 2. Разрешить корневой путь проекта
            let project_root = std::path::Path::new(&path)
                .canonicalize()
                .unwrap_or_else(|_| std::path::PathBuf::from(&path));

            // 3. Получить все файлы из индекса
            let files = storage.get_all_files()?;
            let total = files.len();
            let mut deleted = 0usize;

            // 4. Для каждого файла проверить существование на диске
            for file in files {
                // Путь в индексе может быть абсолютным или относительным от корня проекта
                let on_disk = if std::path::Path::new(&file.path).is_absolute() {
                    std::path::PathBuf::from(&file.path)
                } else {
                    project_root.join(&file.path)
                };

                if !on_disk.exists() {
                    if let Some(id) = file.id {
                        storage.delete_file(id)?;
                        deleted += 1;
                        println!("  Удалён: {}", file.path);
                    }
                }
            }

            // 5. Итог
            println!(
                "Очистка завершена: проверено {} файлов, удалено {} записей.",
                total, deleted
            );
        }

        Commands::Init { path, no_index, no_mcp, force } => {
            handle_init(path, no_index, no_mcp, force, &registry).await?;
        }

        // ── Новые команды: JSON-вывод ─────────────────────────────────────────

        Commands::SearchFunction { query, path, language, limit, include_body } => {
            let db_path = get_db_path(&path);
            let storage = Storage::open_file_readonly(&db_path)?;
            let results = storage.search_functions(&query, limit, language.as_deref())?;
            if include_body {
                println!("{}", serde_json::to_string_pretty(&results)?);
            } else {
                // lean: без тел — экономит токены; тело тянут точечно get-function
                let hits: Vec<crate::storage::models::FunctionHit> = results
                    .iter()
                    .map(|f| crate::storage::models::FunctionHit::from_record(
                        f, storage.get_path_by_file_id(f.file_id).ok().flatten().unwrap_or_default(),
                    ))
                    .collect();
                println!("{}", serde_json::to_string_pretty(&hits)?);
            }
        }

        Commands::SearchClass { query, path, language, limit, include_body } => {
            let db_path = get_db_path(&path);
            let storage = Storage::open_file_readonly(&db_path)?;
            let results = storage.search_classes(&query, limit, language.as_deref())?;
            if include_body {
                println!("{}", serde_json::to_string_pretty(&results)?);
            } else {
                let hits: Vec<crate::storage::models::ClassHit> = results
                    .iter()
                    .map(|c| crate::storage::models::ClassHit::from_record(
                        c, storage.get_path_by_file_id(c.file_id).ok().flatten().unwrap_or_default(),
                    ))
                    .collect();
                println!("{}", serde_json::to_string_pretty(&hits)?);
            }
        }

        Commands::GetFunction { name, path, language: _ } => {
            let db_path = get_db_path(&path);
            let storage = Storage::open_file_readonly(&db_path)?;
            let results = storage.get_function_by_name(&name)?;
            println!("{}", serde_json::to_string_pretty(&results)?);
        }

        Commands::GetClass { name, path, language: _ } => {
            let db_path = get_db_path(&path);
            let storage = Storage::open_file_readonly(&db_path)?;
            let results = storage.get_class_by_name(&name)?;
            println!("{}", serde_json::to_string_pretty(&results)?);
        }

        Commands::GetCallers { function_name, path, language, limit } => {
            let db_path = get_db_path(&path);
            let storage = Storage::open_file_readonly(&db_path)?;
            let mut results = storage.get_callers(&function_name, language.as_deref())?;
            results.truncate(limit);
            println!("{}", serde_json::to_string_pretty(&results)?);
        }

        Commands::GetCallees { function_name, path, language, limit } => {
            let db_path = get_db_path(&path);
            let storage = Storage::open_file_readonly(&db_path)?;
            let mut results = storage.get_callees(&function_name, language.as_deref())?;
            results.truncate(limit);
            println!("{}", serde_json::to_string_pretty(&results)?);
        }

        Commands::GetImports { path, file_id, module, language } => {
            let db_path = get_db_path(&path);
            let storage = Storage::open_file_readonly(&db_path)?;

            // Приоритет: file_id > module; если ничего не указано — ошибка
            let results = if let Some(fid) = file_id {
                storage.get_imports_by_file(fid)?
            } else if let Some(mod_name) = &module {
                storage.get_imports_by_module(mod_name, language.as_deref())?
            } else {
                return Err(anyhow::anyhow!(
                    "Укажите --file-id <ID> или --module <имя_модуля>"
                ));
            };
            println!("{}", serde_json::to_string_pretty(&results)?);
        }

        Commands::GetFileSummary { file, path } => {
            let db_path = get_db_path(&path);
            let storage = Storage::open_file_readonly(&db_path)?;
            let result = storage.get_file_summary(&file, 0)?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }

        Commands::SearchText { query, path, language, limit } => {
            let db_path = get_db_path(&path);
            let storage = Storage::open_file_readonly(&db_path)?;
            let results = storage.search_text(&query, limit, language.as_deref())?;

            // Результат — Vec<(String, String)>: путь + сниппет
            // Преобразуем в удобный JSON-массив объектов
            let json_results: Vec<serde_json::Value> = results
                .into_iter()
                .map(|(file_path, snippet)| {
                    serde_json::json!({
                        "path": file_path,
                        "snippet": snippet
                    })
                })
                .collect();
            println!("{}", serde_json::to_string_pretty(&json_results)?);
        }

        Commands::GrepBody { path, pattern, regex, language, limit } => {
            if pattern.is_none() && regex.is_none() {
                return Err(anyhow::anyhow!(
                    "Укажите --pattern <подстрока> или --regex <выражение>"
                ));
            }
            let db_path = get_db_path(&path);
            let storage = Storage::open_file_readonly(&db_path)?;
            let results = storage.grep_body(
                pattern.as_deref(),
                regex.as_deref(),
                language.as_deref(),
                limit,
            )?;
            println!("{}", serde_json::to_string_pretty(&results)?);
        }

        Commands::RepoMap { path, top } => {
            let db_path = get_db_path(&path);
            let storage = Storage::open_file_readonly(&db_path)?;
            println!("{}", serde_json::to_string_pretty(&storage.repo_map(top)?)?);
        }

        Commands::Complex { path, limit, language, path_glob } => {
            let db_path = get_db_path(&path);
            let storage = Storage::open_file_readonly(&db_path)?;
            let r = storage.find_complex_functions(limit, path_glob.as_deref(), language.as_deref())?;
            println!("{}", serde_json::to_string_pretty(&r)?);
        }

        Commands::FindExisting { query, path, kind, language, limit } => {
            let db_path = get_db_path(&path);
            let storage = Storage::open_file_readonly(&db_path)?;
            let r = storage.find_existing(&query, Some(&kind), language.as_deref(), limit)?;
            println!("{}", serde_json::to_string_pretty(&r)?);
        }

        Commands::Unreachable { path, limit, language, path_glob } => {
            let db_path = get_db_path(&path);
            let storage = Storage::open_file_readonly(&db_path)?;
            let r = storage.find_unreachable(limit, path_glob.as_deref(), language.as_deref())?;
            println!("{}", serde_json::to_string_pretty(&r)?);
        }

        Commands::Doctor { path, json } => {
            let abs = Path::new(&path).canonicalize().unwrap_or_else(|_| PathBuf::from(&path));
            let db_path = get_db_path(&path);
            if !db_path.exists() {
                return Err(anyhow::anyhow!(
                    "Индекс не найден: {}. Запустите `icode index .` или `icode init`.",
                    db_path.display()
                ));
            }
            let config = IndexConfig::load(&abs)?;
            let storage = Storage::open_file_readonly(&db_path)?;
            let report = crate::indexer::diagnose(&abs, &config, &storage).map_err(|e| {
                anyhow::anyhow!(
                    "Не удалось прочитать индекс ({}). Возможно он пуст или повреждён — \
                     запустите `icode index .`.",
                    e
                )
            })?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                let mark = if report.healthy { "✅" } else { "⚠️" };
                println!("{} iCode doctor — {}", mark, abs.display());
                if let Some(note) = &report.note {
                    println!("  ⓘ {}", note);
                }
                println!("  Индексировано: {}   на диске: {}", report.indexed_files, report.disk_files);
                println!("  Пропущено (нет в индексе):   {}", report.missing_count);
                println!("  Устарело (изменилось):       {}", report.outdated_count);
                println!("  Фантомы (удалены с диска):   {}", report.stale_count);
                println!("  Не распарсилось (слепые):    {}", report.parse_error_count);
                let show = |title: &str, v: &[String]| {
                    if !v.is_empty() {
                        println!("  {} (до 50):", title);
                        for p in v {
                            println!("    {}", p);
                        }
                    }
                };
                show("Пропущено", &report.missing_sample);
                show("Устарело", &report.outdated_sample);
                show("Фантомы", &report.stale_sample);
                show("Не распарсилось", &report.parse_error_sample);
                if !report.healthy {
                    println!("\n  Починить: `icode index .` (или `--force` для полной переиндексации).");
                }
            }
        }

        Commands::Setup { path, icode_home } => {
            handle_setup(path, icode_home)?;
            return Ok(());
        }

        Commands::Daemon { action } => {
            // Передаём registry в handle_daemon, чтобы daemon-режим мог
            // применять schema_extensions / index_extras для BSL и других
            // языков-расширений. registry.take() — чтобы не клонировать
            // и сохранить совместимость с веткой Serve выше.
            let reg = registry.take().map(Arc::new);
            handle_daemon(action, reg).await?;
        }
    }

    Ok(())
}

/// На Windows Rust собирается как console-subsystem приложение. При запуске
/// в пользовательской сессии (Scheduled Task LogonType=Interactive, ручной
/// вызов в cmd/powershell) процесс получает консольное окно и становится
/// привязанным к нему: закрытие окна шлёт CTRL_CLOSE_EVENT и убивает демон.
///
/// Чтобы демон переживал любой способ запуска, при `daemon run` смотрим
/// переменную окружения `CODE_INDEX_DAEMON_DETACHED`. Если её нет —
/// перезапускаем себя с флагами DETACHED_PROCESS | CREATE_NO_WINDOW
/// и немедленно выходим; detached-клон живёт без консоли до явного
/// `daemon stop` / `daemon reload`.
///
/// На Unix self-detach не нужен — демонизацией управляет systemd/launchd.
#[cfg(windows)]
fn detach_from_console_if_needed() -> anyhow::Result<bool> {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const ENV_FLAG: &str = "CODE_INDEX_DAEMON_DETACHED";

    if std::env::var_os(ENV_FLAG).is_some() {
        return Ok(false);
    }

    let exe = std::env::current_exe()?;
    std::process::Command::new(exe)
        .arg("daemon")
        .arg("run")
        .env(ENV_FLAG, "1")
        .creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW)
        .spawn()?;
    Ok(true)
}

#[cfg(not(windows))]
fn detach_from_console_if_needed() -> anyhow::Result<bool> {
    Ok(false)
}

async fn handle_daemon(
    action: DaemonAction,
    processor_registry: Option<Arc<ProcessorRegistry>>,
) -> anyhow::Result<()> {
    use crate::daemon_core::{client, runner};

    match action {
        DaemonAction::Run => {
            if detach_from_console_if_needed()? {
                return Ok(());
            }
            tracing::info!("Запуск фонового демона icode");
            runner::run(processor_registry).await?;
        }
        DaemonAction::Status { json } => match client::health().await {
            Ok(h) => {
                if json {
                    println!("{}", serde_json::to_string_pretty(&h)?);
                } else {
                    print_status_text(&h);
                }
            }
            Err(e) => {
                tracing::error!("Демон недоступен: {}", e);
                std::process::exit(1);
            }
        },
        DaemonAction::Reload => {
            let r = client::reload().await?;
            println!("{}", serde_json::to_string_pretty(&r)?);
        }
        DaemonAction::Stop => {
            let r = client::stop().await?;
            println!("{}", serde_json::to_string_pretty(&r)?);
        }
    }
    Ok(())
}

fn print_status_text(h: &crate::daemon_core::ipc::HealthResponse) {
    println!("Демон icode");
    println!("  статус:    {}", h.status);
    println!("  версия:    {}", h.version);
    println!("  PID:       {}", h.pid);
    println!("  старт:     {}", h.started_at);
    println!("  uptime:    {}с", h.uptime_sec);
    println!("  папок:     {}", h.paths.len());
    for p in &h.paths {
        let status_s = serde_json::to_string(&p.status)
            .unwrap_or_else(|_| "\"?\"".into());
        let status_s = status_s.trim_matches('"');
        let progress_s = match &p.progress {
            Some(pr) => match pr.percent {
                Some(pct) => format!(" {}/{} ({}%)", pr.files_done, pr.files_total, pct),
                None => format!(" {}/{}", pr.files_done, pr.files_total),
            },
            None => String::new(),
        };
        let err_s = p.error.as_ref().map(|e| format!(" err: {}", e)).unwrap_or_default();
        println!("    - [{}] {}{}{}", status_s, p.path.display(), progress_s, err_s);
    }
}

// ── icode setup ──────────────────────────────────────────────────────────────

fn handle_setup(path: String, icode_home_arg: Option<String>) -> anyhow::Result<()> {
    let project_path = std::path::Path::new(&path)
        .canonicalize()
        .unwrap_or_else(|_| std::path::PathBuf::from(&path));

    // 1. Определить ICODE_HOME (явный аргумент → ENV → дефолт по ОС)
    let icode_home: std::path::PathBuf = resolve_icode_home(icode_home_arg.as_deref());

    println!("\n🔧 iCode setup");
    println!("   Проект:    {}", project_path.display());
    println!("   ICODE_HOME: {}\n", icode_home.display());

    // 2. Создать ICODE_HOME
    std::fs::create_dir_all(&icode_home)?;

    // 3. Создать или обновить daemon.toml
    setup_daemon_toml(&icode_home, &project_path)?;

    // 4. Записать .mcp.json в проект
    let binary_path = std::env::current_exe()
        .unwrap_or_else(|_| std::path::PathBuf::from("icode"));
    setup_mcp_json(&project_path, &binary_path, &icode_home)?;

    // 5. Установить autostart (systemd / launchd / NSSM)
    let autostart_ok = setup_autostart(&icode_home, &binary_path);

    // 6. Добавить ICODE_HOME в shell profile (Linux/macOS)
    #[cfg(not(windows))]
    setup_shell_profile(&icode_home);

    // 7. Запустить демон
    println!("▶ Запускаем демон...");
    let daemon_started = std::process::Command::new(&binary_path)
        .args(["daemon", "run"])
        .env("ICODE_HOME", &icode_home)
        .spawn()
        .is_ok();

    // 8. Итог
    println!("\n✅ Готово!\n");
    println!("  {} daemon.toml       создан/обновлён", icode_home.join("daemon.toml").display());
    println!("  {} .mcp.json         создан", project_path.join(".mcp.json").display());
    if autostart_ok { println!("  autostart           установлен — демон запустится при загрузке"); }
    if daemon_started { println!("  daemon              запущен в фоне"); }
    println!();
    println!("Следующий шаг: откройте проект в Claude Code — iCode уже работает.");
    println!("Инструменты: find_symbol, search_function, grep_code, get_file_summary ...\n");

    Ok(())
}

fn setup_daemon_toml(icode_home: &std::path::Path, project: &std::path::Path) -> anyhow::Result<()> {
    let toml_path = icode_home.join("daemon.toml");
    let project_str = project.to_string_lossy();

    if toml_path.exists() {
        // Уже есть — проверим, не добавлен ли уже этот путь
        let content = std::fs::read_to_string(&toml_path)?;
        if content.contains(project_str.as_ref()) {
            println!("✓ daemon.toml уже содержит этот проект");
            return Ok(());
        }
        // Добавляем новую [[paths]] секцию в конец
        let appended = format!("{}\n[[paths]]\npath = \"{}\"\n", content.trim_end(), project_str);
        std::fs::write(&toml_path, appended)?;
        println!("✓ daemon.toml: добавлен путь {}", project_str);
    } else {
        // Создаём с нуля
        let content = format!(
            "[daemon]\nhttp_port = 0\n\n[[paths]]\npath = \"{}\"\n",
            project_str
        );
        std::fs::write(&toml_path, &content)?;
        println!("✓ daemon.toml создан");
    }
    Ok(())
}

fn setup_mcp_json(project: &std::path::Path, binary: &std::path::Path, icode_home: &std::path::Path) -> anyhow::Result<()> {
    let mcp_path = project.join(".mcp.json");
    let binary_str = binary.to_string_lossy();
    let home_str = icode_home.to_string_lossy();

    let content = format!(
        r#"{{
  "mcpServers": {{
    "icode": {{
      "type": "stdio",
      "command": "{}",
      "args": ["serve", "--path", "."],
      "env": {{
        "ICODE_HOME": "{}"
      }}
    }}
  }}
}}
"#,
        binary_str, home_str
    );

    if mcp_path.exists() {
        println!("✓ .mcp.json уже существует — пропускаем (отредактируйте вручную при необходимости)");
    } else {
        std::fs::write(&mcp_path, &content)?;
        println!("✓ .mcp.json создан");
    }
    Ok(())
}

/// Возвращает true если autostart успешно установлен
fn setup_autostart(icode_home: &std::path::Path, binary: &std::path::Path) -> bool {
    #[cfg(target_os = "linux")]
    return setup_systemd(icode_home, binary);

    #[cfg(target_os = "macos")]
    return setup_launchd(icode_home, binary);

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (icode_home, binary);
        false
    }
}

#[cfg(target_os = "linux")]
fn setup_systemd(icode_home: &std::path::Path, binary: &std::path::Path) -> bool {
    let systemd_dir = dirs::home_dir()
        .unwrap_or_default()
        .join(".config/systemd/user");
    if std::fs::create_dir_all(&systemd_dir).is_err() { return false; }

    let unit = format!(
        "[Unit]\nDescription=iCode indexing daemon\nAfter=network.target\n\n\
         [Service]\nType=simple\nExecStart={} daemon run\nRestart=on-failure\nRestartSec=5\n\
         Environment=ICODE_HOME={}\n\n\
         [Install]\nWantedBy=default.target\n",
        binary.display(), icode_home.display()
    );
    let unit_path = systemd_dir.join("icode.service");
    if std::fs::write(&unit_path, unit).is_err() { return false; }

    // systemctl --user enable + start
    let _ = std::process::Command::new("systemctl")
        .args(["--user", "daemon-reload"]).status();
    let enabled = std::process::Command::new("systemctl")
        .args(["--user", "enable", "icode.service"]).status()
        .map(|s| s.success()).unwrap_or(false);

    if enabled {
        println!("✓ systemd user service установлен: {}", unit_path.display());
    }
    enabled
}

#[cfg(target_os = "macos")]
fn setup_launchd(icode_home: &std::path::Path, binary: &std::path::Path) -> bool {
    let launch_dir = dirs::home_dir()
        .unwrap_or_default()
        .join("Library/LaunchAgents");
    if std::fs::create_dir_all(&launch_dir).is_err() { return false; }

    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>sh.icode.daemon</string>
  <key>ProgramArguments</key>
  <array><string>{}</string><string>daemon</string><string>run</string></array>
  <key>EnvironmentVariables</key>
  <dict><key>ICODE_HOME</key><string>{}</string></dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
</dict></plist>
"#,
        binary.display(), icode_home.display()
    );
    let plist_path = launch_dir.join("sh.icode.daemon.plist");
    if std::fs::write(&plist_path, plist).is_err() { return false; }

    let loaded = std::process::Command::new("launchctl")
        .args(["load", "-w", plist_path.to_str().unwrap_or("")])
        .status().map(|s| s.success()).unwrap_or(false);

    if loaded {
        println!("✓ launchd agent установлен: {}", plist_path.display());
    }
    loaded
}

#[cfg(not(windows))]
fn setup_shell_profile(icode_home: &std::path::Path) {
    let export_line = format!("\nexport ICODE_HOME=\"{}\"\n", icode_home.display());

    // Пробуем .zshrc, .bashrc, .profile — по порядку приоритета
    let home = dirs::home_dir().unwrap_or_default();
    let candidates = [".zshrc", ".bashrc", ".profile"];
    for rc in &candidates {
        let rc_path = home.join(rc);
        if rc_path.exists() {
            let content = std::fs::read_to_string(&rc_path).unwrap_or_default();
            if content.contains("ICODE_HOME") {
                println!("✓ ICODE_HOME уже задан в ~/{}", rc);
                return;
            }
            if std::fs::OpenOptions::new().append(true).open(&rc_path)
                .and_then(|mut f| { use std::io::Write; f.write_all(export_line.as_bytes()) })
                .is_ok()
            {
                println!("✓ ICODE_HOME добавлен в ~/{}", rc);
                println!("  Перезапустите терминал или выполните: source ~/{}", rc);
            }
            return;
        }
    }
}
