---
# iCode — Обзор архитектуры

## Что такое iCode

iCode — высокопроизводительный индексатор кода с MCP-протоколом для AI-моделей. Скомпилированный Rust-бинарник `icode` решает одну задачу: дать AI-агентам мгновенный структурированный доступ к коду вместо медленных grep/find вызовов.

## Архитектура: один писатель / много читателей

```
┌─────────────────────────────────────────────────────┐
│  Файловая система                                   │
│  (PHP, Python, JS, TS, Java, Rust, Go, HTML файлы) │
└──────────────────────┬──────────────────────────────┘
                       │ inotify / FSEvents / ReadDirectoryChanges
                       ▼
┌─────────────────────────────────────────────────────┐
│  Daemon (фоновый процесс)                           │
│  • Единственный писатель                            │
│  • Обходит папки из daemon.toml                     │
│  • Парсит файлы через tree-sitter (rayon, parallel) │
│  • Пишет в .icode/index.db (SQLite + FTS5)         │
│  • HTTP-сервер: GET /health, POST /stop, /invalidate│
└──────────────────────┬──────────────────────────────┘
                       │ SQLite WAL (много читателей параллельно)
                       ▼
┌─────────────────────────────────────────────────────┐
│  MCP Server (read-only процесс)                     │
│  • Запускается Claude Code / VS Code / субагентом   │
│  • Подключается к той же .icode/index.db            │
│  • Только читает — никаких конфликтов записи        │
│  • Транспорт: stdio или HTTP                        │
└──────────────────────┬──────────────────────────────┘
                       │ MCP protocol
                       ▼
┌─────────────────────────────────────────────────────┐
│  AI-клиенты                                         │
│  • Claude Code                                      │
│  • VS Code extension                                │
│  • Custom AI agents                                 │
└─────────────────────────────────────────────────────┘
```

## Структура воркспейса

```
crates/
  code-index-core/     # библиотека — всё ядро
    src/
      cli.rs           # CLI-диспетчер (Commands enum)
      parser/          # tree-sitter парсеры (8 языков)
      indexer/         # обход файлов, хеширование, запись в БД
      storage/         # SQLite CRUD + схема + FTS5
      mcp/             # MCP-сервер (18 инструментов)
      daemon_core/     # фоновый демон: конфиг, IPC, watcher, HTTP
      extension/       # trait API: LanguageProcessor, IndexTool
      federation/      # федерация нескольких машин (serve.toml)
      watcher.rs       # notify + debounce
  code-index/          # бинарник icode
    src/main.rs        # регистрирует процессоры → вызывает cli::run()
```

## Схема БД (SQLite)

```sql
files          — все проиндексированные файлы (path, hash, language, mtime)
functions      — функции и методы (name, qualified_name, args, body, line_start, line_end)
classes        — классы, интерфейсы, трейты (name, bases, body)
imports        — импорты/use (module, name, alias, kind)
calls          — граф вызовов (caller, callee, file_id, line)
variables      — переменные уровня файла/модуля
text_files     — FTS5 таблица для полнотекстового поиска
file_contents  — содержимое файлов (zstd-сжатие, для read_file)
```

## Парсеры (tree-sitter)

| Язык | Крейт | Что извлекается |
|------|-------|-----------------|
| Python | tree-sitter-python | функции, классы, импорты, вызовы, переменные |
| JavaScript | tree-sitter-javascript | функции, классы, импорты, вызовы |
| TypeScript | tree-sitter-typescript | функции, классы, импорты, вызовы |
| Java | tree-sitter-java | методы, классы, импорты, вызовы |
| Rust | tree-sitter-rust | функции, структуры, импорты, вызовы |
| Go | tree-sitter-go | функции, типы, импорты, вызовы |
| PHP | tree-sitter-php | функции, методы, классы, interfaces, traits, use, вызовы |
| HTML | tree-sitter-html | элементы с id, формы, ссылки, inline скрипты |

## Конфигурация

### daemon.toml (глобальный)

```toml
[daemon]
http_port = 0              # 0 = автовыбор порта
max_concurrent_initial = 1 # макс. параллельных initial reindex

[[paths]]
path = "/path/to/project"
debounce_ms = 1500         # задержка перед реакцией на изменения
batch_ms = 2000            # время накопления batch событий
```

### .icode/config.json (per-project)

```json
{
  "exclude_dirs": ["vendor", "node_modules", ".git", "var", "cache"],
  "languages": ["php"],
  "storage_mode": "disk",
  "memory_max_percent": 25,
  "debounce_ms": 1500,
  "batch_ms": 2000,
  "max_file_size": 1048576
}
```

### .mcp.json (Claude Code интеграция)

```json
{
  "mcpServers": {
    "icode": {
      "type": "stdio",
      "command": "/path/to/icode",
      "args": ["serve", "--path", "."]
    }
  }
}
```

## Производительность

| Метрика | Значение |
|---------|---------|
| Индексация 93K файлов (mtime fast-path) | 4 сек |
| Поиск среди 282K функций | < 1 мс |
| Параллельный парсинг | все CPU ядра (rayon) |
| In-memory режим vs disk | 1.5–13× быстрее |

## Extension API

Для добавления нового языка достаточно реализовать два трейта:

```rust
// Парсер языка
impl LanguageParser for MyParser {
    fn language_name(&self) -> &str { "mylang" }
    fn file_extensions(&self) -> &[&str] { &["ml"] }
    fn parse(&self, source: &str, path: &str) -> Result<ParseResult> { ... }
}

// Процессор (регистрируется в main.rs)
let proc = StandardLanguageProcessor::new("mylang", Box::new(MyParser::new()), detect_fn);
reg.register(Arc::new(proc));
```

## Формат ответов MCP

Все data-tools возвращают:

```json
{
  "result": [ ... данные ... ],
  "_meta": {
    "dependent_files": ["src/Controller.php", "src/Entity/User.php"]
  }
}
```

`dependent_files` — файлы от которых зависит ответ. Используется для event-based инвалидации кэша: после изменения файла cache-ci получает POST /invalidate и точечно сносит устаревшие записи.
