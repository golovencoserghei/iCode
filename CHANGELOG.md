# iCode Changelog

Формат — [Keep a Changelog](https://keepachangelog.com/ru/1.0.0/).
Версионирование — [SemVer](https://semver.org/).

## [0.11.0] — Unreleased

Идеи, перенятые из проекта **codegraph** и адаптированные под архитектуру iCode.

### Добавлено
- **`get_repo_map`** — глубокая архитектурная карта репозитория за ОДИН дешёвый
  вызов (~2500 токенов на проект в 2751 файл вместо десятков grep/read): counts,
  languages, крупнейшие modules, complex_functions (где сосредоточена сложность),
  call_hotspots (самые вызываемые ПРОЕКТНЫЕ функции — без stdlib-шума),
  entry_points (оркестраторы/корни), parse_errors (слепые зоны). CLI: `icode repo-map`.
- **`find_complex_functions`** — функции по сложности (длина + fan-out + fan-in)
  для рефакторинга/ревью. CLI: `icode complex`.
- **Видимость слепых зон** — таблица `parse_errors` (миграция v6): файлы, которые
  не удалось распарсить, теперь видны (в `get_repo_map.parse_errors`), «ничего не
  упускаем». Самоочищается: файл снова распарсился → отметка снимается.
- **Чистый сигнал call-графа** — `call_hotspots` считает только вызовы ПРОЕКТНЫХ
  функций (есть определение в индексе), отсекая stdlib/builtin-шум (Some/Ok/Vec::new).
- **`icode doctor`** — сверка индекса с рабочим деревом (read-only): что пропущено,
  что устарело (по mtime/size), что осталось от удалённых файлов (фантомы), и какие
  файлы не распарсились (слепые зоны). Делает доверие к индексу проверяемым; учитывает
  активный `max_files`. CLI: `icode doctor [--json]`.
- **Framework-aware routing** — распознавание веб-маршрутов фреймворка и связывание
  HTTP-метод + URL с контроллером@методом:
  - PHP/Laravel: `Route::get/post/put/patch/delete/options/any/match(...)` с
    хендлером `[Ctrl::class, 'm']`, `'Ctrl@m'` или closure (включая цепочку `->name(...)`)
  - Таблица `routes` в SQLite (миграция v5), узел `Route` + ребро `HANDLED_BY`
    в граф-слое (Neo4j/Memgraph)
  - Новый MCP-инструмент **`find_routes`** (фильтры: method, path, handler)
  - `get_symbol_context` теперь возвращает поле `routes` — маршруты, чей хендлер
    этот символ (для контроллер-методов)
- **Staleness / connect-time reconciliation** — навигационные инструменты
  (`read_file`, `get_function`, `get_class`, `get_file_summary`, `get_file_outline`,
  `get_symbol_context`, `find_routes`) сверяют `(size, mtime)` файлов с рабочим
  деревом и добавляют `_meta.stale` + `_meta.stale_warning` ⚠️, если файл изменился
  на диске после индексации (ловит окно debounce без IPC к демону)
- **Provenance / OOP-резолв вызовов** — `callers`/`callees` в `get_symbol_context`
  имеют поле `resolution` (`own`/`inherited`/`exact`/`by_name`) + `resolved_to`.
  PHP-парсер захватывает ПОЛУЧАТЕЛЯ вызова (`calls.receiver`, миграция v7:
  `$this`/`self`/`parent`/`static`/имя класса/переменная), и резолв точный:
  `$this->m()`/`self::`/`static::` → MRO своего класса, `parent::` → только предки,
  `Foo::m()` → класс Foo; `$other->m()` НЕ приписывается своему классу.
- **scripts/benchmark.py** — воспроизводимый бенчмарк iCode vs «grep + чтение файлов»
  (оценка токенов, операций, времени), как методология codegraph

### Исправлено (по итогам сеньор-ревью v0.11)
- **CRITICAL: in-memory режим над БД до v0.11** не прогонял миграции v4/v5/v6 —
  индексация падала `no such table: routes`. `open_auto` теперь зовёт
  `initialize_tables_only` (идемпотентный прогон v2→v6) на загруженной в RAM БД.
- `find_routes`: добавлен `ESCAPE '\'` в LIKE (без него `%`/`_`/`\` в запросе
  ломали поиск; SQL-инъекции не было — параметры биндятся).
- `parse_errors` больше не «утекают» для удалённых с диска непарсящихся файлов
  (прунинг по `seen` в phase_cleanup; у таких файлов нет строки в `files`).
- `get_symbol_context` резолвит provenance callee через лёгкий `function_defs_lite`
  (без вытягивания тел — раньше тянул полные тела до 30 функций впустую).
- `get_stats` теперь включает `parse_errors` (соответствует доку и repo_map).

### Исправлено (второй проход ревью — receiver-capture)
- `parent::method()` резолвится в РОДИТЕЛЯ, а не в свой класс (новый
  `OopModel::resolve_in_ancestors`, минующий сам класс) — раньше override-метод,
  вызванный через `parent::`, помечался `own` с указанием на себя.
- `row_to_call` пробрасывает реальные ошибки типа, толерантен только к
  отсутствию колонки `receiver` (раньше `.unwrap_or(None)` глушил всё).
- `upsert_file` теперь персистит `mtime`/`file_size` сразу при вставке (раньше —
  NULL до отдельного прохода): mtime-префильтр пропускает неизменённые файлы уже
  со 2-го прогона без перехеширования, и `icode doctor` детектит «устарело» корректно.

### Изменено
- **`icode init`** теперь полноценный инициализатор проекта: создаёт конфиг,
  строит индекс и пишет `.mcp.json` за один запуск (флаги `--no-index`, `--no-mcp`,
  `--force`). Раньше создавал только `.icode/config.json`. Идемпотентен — существующие
  конфиг/`.mcp.json` не перезаписываются, индексация пропускает неизменённое.
- **Token-efficiency pass** (дёшево по умолчанию, дорого — по запросу):
  - `search_function` / `search_class` / `find_symbol` по умолчанию возвращают
    **lean-проекцию без тел** (имя, qualified_name, file_path, строки, сигнатура,
    docstring) — символ локализуется, тело тянется точечно через `get_function`/
    `read_file`. `include_body=true` (CLI `--include-body`) возвращает прежний
    полный вид. На «толстых» функциях экономит 5–10× токенов.
  - `get_callers` / `get_callees` получили дефолтный кап (50 рёбер) с пометкой
    `_meta.note` при усечении (CLI `--limit`). Раньше отдавали все call-site —
    «толстая» функция раздувала ответ.

  Замер на одном репо/символах (см. `scripts/compare_codegraph.py`): суммарные
  токены retrieval упали ~на 45%, iCode стал легче codegraph на ~15% (медиана −33%),
  сохранив ~30× преимущество по скорости запросов.
- **OOP-aware анализ** (наследование/трейты учитываются в графе):
  - **Исправлен баг PHP-парсера**: `extends`/`implements` теперь захватываются
    (`base_clause` + `class_interface_clause` — раньше искались как несуществующие
    поля, `bases` почти всегда был пуст). На реальном Laravel — 9→129 классов
    с наследованием. Это «оживляет» `get_implementations`, граф `INHERITS` и
    inheritance-анализ ниже.
  - **Rust-парсер**: `impl Trait for Type` теперь пишет трейт в `bases` типа
    (8 типов реализуют `LanguageParser` в самом iCode) — без дублирования записей.
  - **OopModel** (`storage/oop.rs`): иерархия типов + методы из индекса, query-time,
    без изменения схемы. Даёт `is_override`/`overrides_of`/`overridden_by`.
  - **`get_symbol_context`** возвращает поле `inheritance` — какие методы предков
    метод переопределяет/реализует и кто переопределяет его.
  - **`find_dead_code`** больше не помечает мёртвыми методы, переопределяющие/
    реализующие метод предка (включая интерфейс/трейт) — они вызываются полиморфно.
  - **vendor signatures-only индексация** (`dependency_dirs` в config): из папок
    зависимостей (vendor/node_modules) индексируются ТОЛЬКО сигнатуры (классы+bases,
    методы — без тел) в изолированные таблицы `ext_*` (миграция v8), читаемые только
    OopModel. Закрывает наследование от фреймворка (Laravel `Controller`/`Model`):
    dead-code и `get_symbol_context` резолвят framework-методы. Не засоряет
    search/FTS/repo_map. Opt-in; пересканируется при `--force`.

## [0.9.1] — 2026-05-29

### Добавлено
- Поддержка PHP (tree-sitter-php): функции, классы, интерфейсы, трейты, граф вызовов, use-импорты, сгруппированные use
- Все data-tools возвращают `{result, _meta: {dependent_files: [...]}}` для точечной инвалидации кэша
- `CacheClient` — POST /invalidate после commit_batch демона

### Языки
- Python, JavaScript, TypeScript, Java, Rust, Go, PHP, HTML

### MCP-инструменты (18 штук)
- search_function, search_class, search_text, find_symbol
- get_function, get_class, get_callers, get_callees
- get_imports, get_file_summary
- grep_body, grep_text, grep_code
- list_files, stat_file, get_stats, read_file, health
