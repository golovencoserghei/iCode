// Граф-слой: Neo4j / Memgraph через Bolt (neo4rs).
//
// Архитектура: dual-write.
//   * SQLite остаётся основным хранилищем для FTS, символов и контента.
//   * Граф-БД хранит только связи: CALLS, IMPORTS, DEFINED_IN, INHERITS.
//   * Граф-слой опционален — отсутствие секции `[graph]` в daemon.toml
//     означает «выключено», все существующие инструменты работают как раньше.
//
// Интеграция в индексатор:
//   Indexer получает `GraphStoreHandle` — тонкую обёртку над
//   `mpsc::UnboundedSender<GraphCommand>`. При записи файла в SQLite
//   он отправляет команды в канал. Отдельная tokio-задача (`GraphStore::run`)
//   вычитывает команды и пишет в граф-БД. Это не блокирует rayon-потоки.

pub mod client;

use std::sync::Arc;

use anyhow::Result;
use tokio::sync::mpsc;
use tracing::{info, warn};

pub use client::GraphClient;

// ── Команды ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum GraphCommand {
    UpsertFile {
        repo: String,
        path: String,
        language: String,
        hash: String,
    },
    UpsertFunction {
        repo: String,
        file_path: String,
        name: String,
        qualified_name: Option<String>,
        line_start: usize,
        line_end: usize,
    },
    UpsertClass {
        repo: String,
        file_path: String,
        name: String,
        line_start: usize,
        line_end: usize,
        bases: Option<String>,
    },
    AddCall {
        repo: String,
        file_path: String,
        caller: String,
        callee: String,
        line: usize,
    },
    AddImport {
        repo: String,
        file_path: String,
        module: String,
        kind: String,
    },
    /// Framework-aware routing: Route-узел + ребро HANDLED_BY к функции-хендлеру.
    AddRoute {
        repo: String,
        file_path: String,
        method: String,
        path: String,
        handler_class: Option<String>,
        handler_method: String,
        line: usize,
    },
    DeleteFileData {
        repo: String,
        path: String,
    },
}

// ── Handle (клонируемый, отправляет в канал) ──────────────────────────────────

/// Дёшево клонируемый хэндл для отправки команд в фоновый граф-воркер.
/// `None` внутри = граф-слой выключен (секция `[graph]` отсутствует в конфиге).
#[derive(Clone, Default)]
pub struct GraphStoreHandle(Option<mpsc::Sender<GraphCommand>>);

impl GraphStoreHandle {
    pub fn new(tx: mpsc::Sender<GraphCommand>) -> Self {
        Self(Some(tx))
    }

    pub fn disabled() -> Self {
        Self(None)
    }

    pub fn is_enabled(&self) -> bool {
        self.0.is_some()
    }

    /// Отправить команду в канал. Если граф выключен или канал закрыт — тихо игнорируем.
    /// Использует try_send с backpressure: при полном канале команда отбрасывается с предупреждением.
    pub fn send(&self, cmd: GraphCommand) {
        if let Some(tx) = &self.0 {
            match tx.try_send(cmd) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => {
                    tracing::warn!("graph_store: queue full, dropping command")
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    tracing::error!("graph_store: worker dead, graph writes disabled")
                }
            }
        }
    }
}

// ── Store (фоновый воркер) ────────────────────────────────────────────────────

/// Фоновый воркер, который вычитывает `GraphCommand` и пишет в Neo4j/Memgraph.
pub struct GraphStore {
    client: Arc<GraphClient>,
    rx: mpsc::Receiver<GraphCommand>,
}

impl GraphStore {
    /// Подключиться к граф-БД с exponential backoff (макс. 3 попытки).
    /// Handle раздаётся индексаторам; store запускается через `tokio::spawn(store.run())`.
    pub async fn connect(
        bolt_url: &str,
        username: &str,
        password: &str,
    ) -> Result<(Self, GraphStoreHandle)> {
        let client = Self::connect_with_retry(bolt_url, username, password).await?;
        client.ensure_schema().await;
        let (tx, rx) = mpsc::channel(50_000);
        info!("graph_store: подключён к {}", bolt_url);
        Ok((Self { client: Arc::new(client), rx }, GraphStoreHandle::new(tx)))
    }

    async fn connect_with_retry(bolt_url: &str, username: &str, password: &str) -> Result<GraphClient> {
        let mut delay = std::time::Duration::from_secs(2);
        let max_attempts = 3;
        let mut last_err = None;
        for attempt in 1..=max_attempts {
            match GraphClient::connect(bolt_url, username, password).await {
                Ok(c) => return Ok(c),
                Err(e) => {
                    warn!("graph_store: попытка {}/{} не удалась: {}", attempt, max_attempts, e);
                    last_err = Some(e);
                    if attempt < max_attempts {
                        tokio::time::sleep(delay).await;
                        delay *= 2;
                    }
                }
            }
        }
        Err(last_err.unwrap())
    }

    /// Запустить цикл чтения команд. Вызывать через `tokio::spawn(store.run())`.
    pub async fn run(mut self) {
        while let Some(cmd) = self.rx.recv().await {
            if let Err(e) = self.client.handle_command(cmd).await {
                warn!("graph_store: ошибка команды: {}", e);
            }
        }
        info!("graph_store: канал закрыт, воркер завершён");
    }

    /// Получить Arc на клиент для использования в MCP-инструментах.
    pub fn client(&self) -> Arc<GraphClient> {
        Arc::clone(&self.client)
    }
}
