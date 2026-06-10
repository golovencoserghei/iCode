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
  "exclude_dirs": ["var", "cache"],
  "languages": ["php"],
  "storage_mode": "disk",
  "debounce_ms": 1500,
  "batch_ms": 2000,
  "dependency_dirs": ["vendor"]
}
```

`dependency_dirs` (опционально) — папки зависимостей, из которых индексируются
**только сигнатуры** (классы + `extends`/`implements`, методы — без тел) в отдельные
таблицы `ext_*`. Это даёт ООП-резолв наследования от фреймворка (Laravel
`Controller`/`Model`): `find_dead_code` не считает мёртвыми переопределения
framework-методов, а `get_symbol_context` резолвит унаследованные методы. Эти папки
автоматически исключаются из основного индекса (не засоряют поиск/FTS).
Пересканируются при `icode index --force` (после `composer`/`npm update`).

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
| `get_symbol_context` | Полный контекст символа за 1 вызов: definition + callers + callees + outline + imports + **routes** + **inheritance** |
| `get_repo_map` | 🗺️ Архитектурная карта репо за 1 дешёвый вызов: модули, сложность, hotspots, точки входа, слепые зоны |
| `find_complex_functions` | Функции по сложности (длина + fan-out + fan-in) — что рефакторить/ревьюить |
| `find_routes` | 🌐 Веб-маршруты фреймворка: HTTP-метод + URL → контроллер@метод (Laravel/PHP) |

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

Навигационные инструменты дополнительно сверяют файлы с рабочим деревом и при
рассинхроне добавляют:

```json
{
  "result": {...},
  "_meta": {
    "dependent_files": ["src/foo.php"],
    "stale": ["src/foo.php"],
    "stale_warning": "⚠️ Эти файлы изменились на диске после индексации — данные могут быть устаревшими. Прочитайте их напрямую с диска для актуального содержимого."
  }
}
```

`_meta.stale` — файлы, чьи `(size, mtime)` на диске разошлись с индексом (файл
изменён/удалён после индексации). Сигнал агенту прочитать их напрямую.

## Framework-aware routing

Для веб-проектов «точка входа» — это маршрут, а не вызов функции. iCode распознаёт
определения маршрутов и связывает HTTP-метод + URL с контроллером@методом.

Сейчас поддерживается **Laravel/PHP**:

```php
Route::get('/users', [UserController::class, 'index']);   // → GET /users → UserController::index
Route::post('/users', 'UserController@store');            // → POST /users → UserController::store
Route::delete('/users/{id}', [UserController::class, 'destroy'])->name('users.destroy');
```

- `find_routes` — поиск по маршрутам (фильтры `method` / `path` / `handler`).
- `get_symbol_context("index")` — для контроллер-метода вернёт связанные маршруты
  в поле `routes`, то есть путь `URL → Controller::method` проходится за один вызов.
- При включённом граф-слое `[graph]` создаются узлы `Route` и рёбра `HANDLED_BY`.

## Бенчмарк

`scripts/benchmark.py` измеряет выигрыш индекса против наивного «grep + чтение файлов»
(оценка токенов, число операций, время) — методология как у codegraph:

```bash
cargo build --release -p icode
python3 scripts/benchmark.py --repo /path/to/project
python3 scripts/benchmark.py --repo . --symbols UserService,handle,index --json out.json
```

## CLI-команды

### Старт за одну команду

```bash
cd /path/to/project
icode init                            # конфиг + индекс + .mcp.json — всё за раз
```

`icode init` идемпотентен: создаёт `.icode/config.json` (если нет), строит индекс
и пишет `.mcp.json` для Claude Code (если ещё нет). Флаги: `--no-index`, `--no-mcp`,
`--force`. После него сразу работают query-команды ниже.

### Запросы к индексу (демон не нужен — читают `.icode/index.db` напрямую)

```bash
icode query <symbol>                  # где определён символ (функции/классы/...)
icode get-callers <name>              # кто вызывает функцию
icode get-callees <name>              # что вызывает функция
icode search-function <name>          # FTS-поиск функций
icode search-class <name>             # FTS-поиск классов
icode get-function <name>             # тело функции по имени
icode grep-body --pattern <p>         # поиск по телам функций/классов
icode search-text <q>                 # FTS по текстовым файлам
icode stats                           # статистика индекса
icode clean                           # убрать из индекса удалённые файлы
icode index /path                     # однократная (пере)индексация (--force)
```

### Фоновый режим и MCP

```bash
icode setup                           # daemon.toml + .mcp.json + autostart
icode serve --path .                  # MCP-сервер (stdio) для AI-клиента
icode serve --path alias=/path        # с алиасом
icode daemon run                      # фоновый индексатор (единственный писатель)
icode daemon status                   # статус демона
icode daemon reload                   # перечитать daemon.toml
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
