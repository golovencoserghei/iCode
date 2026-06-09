// Публичные модули icode-core

pub mod cli;
pub mod storage;        // SQLite-хранилище индекса
pub mod parser;         // tree-sitter парсеры
pub mod indexer;        // Обход и индексация файлов
pub mod mcp;            // MCP-сервер (read-only, v0.5+)
pub mod watcher;        // File watcher на базе notify
pub mod daemon_core;    // Ядро фонового демона: конфиг, IPC, состояние, HTTP-сервер
pub mod federation;     // Федеративный serve (v0.5.0-rc6+): serve.toml, форвард tool-call
pub mod extension;      // Trait-API для расширений (v0.6+): LanguageProcessor, IndexTool, ToolContext
pub mod graph_store;    // Граф-слой Neo4j/Memgraph (v0.10+, опциональный)
