# iCode Changelog

Формат — [Keep a Changelog](https://keepachangelog.com/ru/1.0.0/).
Версионирование — [SemVer](https://semver.org/).

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
