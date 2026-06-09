# iCode

Мгновенный поиск по коду для AI-моделей. Заменяет grep на запросы за миллисекунды.

[Русская версия](README_RU.md)

## Что это

**iCode** — скомпилированный Rust-бинарник с архитектурой «один писатель / много читателей»:

1. Парсит исходный код в AST через tree-sitter
2. Индексирует всё в SQLite с FTS5 для полнотекстового поиска
3. **Фоновый демон** — единственный писатель: один процесс следит за папками и держит `.icode/index.db` актуальным
4. **MCP-сервер** — тонкий read-only клиент: сколько угодно параллельных сессий Claude Code / VS Code / субагентов

## Проблема которую решает

AI-модели тратят десятки вызовов `grep`/`find` для навигации по большим проектам. Найти `RuntimeErrorProcessing` в Java-проекте — 14 последовательных grep-вызовов, каждый сканирует тысячи файлов. С iCode — один запрос, результат за <1 мс.

## Поддерживаемые языки

| Язык | Расширения |
|------|------------|
| Python | `.py` |
| JavaScript | `.js`, `.jsx` |
| TypeScript | `.ts`, `.tsx` |
| Java | `.java` |
| Rust | `.rs` |
| Go | `.go` |
| PHP | `.php`, `.phtml`, `.php8`, `.php7` |
| HTML | `.html`, `.htm` |

Текстовые файлы (`.md`, `.json`, `.yaml`, `.toml`, `.xml`, `.sql`, `.env` и др.) индексируются для полнотекстового поиска.

## Быстрый старт

### Сборка из исходников

```bash
git clone <your-repo-url>
cd icode
cargo build --release -p icode
```

Бинарник: `target/release/icode`

### Настройка демона

1. Создайте `daemon.toml`:

```toml
[daemon]
http_port = 0

[[paths]]
path = "/path/to/your/project"
```

2. Запустите демон:

```bash
icode daemon run
```

3. Подключите Claude Code через `.mcp.json` в корне проекта:

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

### Однократная индексация (без демона)

```bash
icode index /path/to/project
```

### Конфигурация проекта

Создаётся автоматически в `.icode/config.json`:

```json
{
  "exclude_dirs": ["vendor", "node_modules", ".git", "var", "cache"],
  "languages": ["php"],
  "storage_mode": "disk",
  "debounce_ms": 1500,
  "batch_ms": 2000
}
```

## MCP-инструменты

### Поиск и навигация

| Инструмент | Описание |
|-----------|----------|
| `search_function` | Полнотекстовый поиск функций по имени/телу |
| `search_class` | Полнотекстовый поиск классов |
| `search_text` | Поиск по текстовым файлам |
| `find_symbol` | Точный поиск по имени (функции, классы, переменные) |
| `grep_body` | Regex/literal поиск внутри тел функций и классов |
| `grep_text` | Regex поиск по текстовым файлам |
| `grep_code` | Поиск по сохранённому содержимому файлов |

### Точечные запросы

| Инструмент | Описание |
|-----------|----------|
| `get_function` | Получить функцию по точному имени |
| `get_class` | Получить класс по точному имени |
| `get_callers` | Кто вызывает данную функцию |
| `get_callees` | Что вызывает данная функция |
| `get_imports` | Импорты файла или модуля |
| `get_file_summary` | Полная карта файла: функции, классы, импорты, переменные |

### Обзор репозитория

| Инструмент | Описание |
|-----------|----------|
| `list_files` | Список файлов с фильтрацией |
| `stat_file` | Метаданные файла |
| `get_stats` | Статистика индекса |
| `read_file` | Прочитать файл (по строкам) |
| `health` | Статус MCP-сервера и демона |

## Формат ответов

Все инструменты возвращают данные обёрнутые в:

```json
{
  "result": [...],
  "_meta": {
    "dependent_files": ["src/foo.php", "src/bar.php"]
  }
}
```

`_meta.dependent_files` — список файлов, от которых зависит ответ. Используется для точечной инвалидации кэша.

## CLI-команды

```bash
icode serve --path .                  # запустить MCP-сервер
icode serve --path alias=/path        # с алиасом
icode index /path                     # однократная индексация
icode index --force /path             # принудительная переиндексация
icode stats                           # статистика индекса
icode search-function <name>          # поиск функции
icode get-callers <name>              # граф вызовов
icode daemon run                      # запустить демон
icode daemon status                   # статус демона
icode daemon stop                     # остановить демон
```

## Архитектура

```
[Файловая система]
       ↓ (notify / inotify)
[Daemon — фоновый индексатор]    ← единственный писатель
       ↓ (SQLite + FTS5)
[MCP Server — read-only]         ← много читателей параллельно
       ↓ (stdio / HTTP)
[Claude Code / VS Code / субагенты]
```

### Структура проекта

```
crates/
  code-index-core/   # ядро: парсеры, storage, MCP-сервер, daemon
  code-index/        # бинарник icode
```

## Технологии

- **Rust** — язык реализации
- **tree-sitter** — AST-парсинг (8 языков)
- **SQLite + FTS5** — индекс и полнотекстовый поиск
- **tokio** — async runtime
- **rayon** — параллельный парсинг
- **rmcp** — Rust MCP SDK
- **zstd** — сжатие содержимого файлов
- **notify** — отслеживание изменений файловой системы
