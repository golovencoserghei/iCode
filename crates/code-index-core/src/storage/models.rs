use serde::{Deserialize, Serialize};

fn is_false(b: &bool) -> bool { !*b }

/// Запись файла в индексе
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileRecord {
    pub id: Option<i64>,
    pub path: String,
    pub content_hash: String,
    pub ast_hash: Option<String>,
    pub language: String,
    pub lines_total: usize,
    pub indexed_at: String,
    pub mtime: Option<i64>,      // Unix timestamp секунды (fs::metadata)
    pub file_size: Option<i64>,  // размер файла в байтах
}

/// Запись функции
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FunctionRecord {
    pub id: Option<i64>,
    pub file_id: i64,
    pub name: String,
    pub qualified_name: Option<String>,
    pub line_start: usize,
    pub line_end: usize,
    pub args: Option<String>,
    pub return_type: Option<String>,
    pub docstring: Option<String>,
    pub body: String,
    pub is_async: bool,
    pub node_hash: String,
    /// Тип переопределения: "Перед", "После", "Вместо" (только BSL-расширения)
    pub override_type: Option<String>,
    /// Имя оригинальной процедуры, которую переопределяет аннотация
    pub override_target: Option<String>,
}

/// Запись класса
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClassRecord {
    pub id: Option<i64>,
    pub file_id: i64,
    pub name: String,
    pub line_start: usize,
    pub line_end: usize,
    pub bases: Option<String>,
    pub docstring: Option<String>,
    pub body: String,
    pub node_hash: String,
}

/// Запись импорта
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportRecord {
    pub id: Option<i64>,
    pub file_id: i64,
    pub module: Option<String>,
    pub name: Option<String>,
    pub alias: Option<String>,
    pub line: usize,
    pub kind: String,
}

/// Запись вызова
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CallRecord {
    pub id: Option<i64>,
    pub file_id: i64,
    pub caller: String,
    pub callee: String,
    pub line: usize,
    /// Получатель вызова ($this/self/parent/static/класс/переменная) для ООП-резолва.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receiver: Option<String>,
}

/// Запись переменной
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VariableRecord {
    pub id: Option<i64>,
    pub file_id: i64,
    pub name: String,
    pub value: Option<String>,
    pub line: usize,
}

/// Запись текстового файла
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextFileRecord {
    pub id: Option<i64>,
    pub file_id: i64,
    pub content: String,
}

/// Запись веб-маршрута (framework-aware routing).
/// Связывает HTTP-метод + URL с хендлером (контроллер@метод).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RouteRecord {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<i64>,
    /// file_id скрыт из ответа — в выдаче полезнее file_path (см. RouteHit).
    #[serde(skip)]
    pub file_id: i64,
    pub method: String,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub handler_class: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub handler_method: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub line: usize,
}

/// Маршрут с путём к файлу — формат выдачи `find_routes` и обогащения symbol_context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteHit {
    pub method: String,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub handler_class: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub handler_method: Option<String>,
    pub file_path: String,
    pub line: usize,
}

/// Результат поиска символа (объединённый)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolSearchResult {
    pub functions: Vec<FunctionRecord>,
    pub classes: Vec<ClassRecord>,
    pub variables: Vec<VariableRecord>,
    pub imports: Vec<ImportRecord>,
}

/// Лёгкая проекция функции для поиска (locate): сигнатура + строки + путь, БЕЗ тела.
/// Экономит токены — тело тянут точечно через `get_function`/`read_file` по line_start/line_end.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionHit {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub qualified_name: Option<String>,
    pub file_path: String,
    pub line_start: usize,
    pub line_end: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub return_type: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub is_async: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub docstring: Option<String>,
}

impl FunctionHit {
    pub fn from_record(f: &FunctionRecord, file_path: String) -> Self {
        Self {
            name: f.name.clone(),
            qualified_name: f.qualified_name.clone(),
            file_path,
            line_start: f.line_start,
            line_end: f.line_end,
            args: f.args.clone(),
            return_type: f.return_type.clone(),
            is_async: f.is_async,
            docstring: f.docstring.clone(),
        }
    }
}

/// Лёгкая проекция класса для поиска (locate): имя + строки + путь + bases, БЕЗ тела.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClassHit {
    pub name: String,
    pub file_path: String,
    pub line_start: usize,
    pub line_end: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bases: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub docstring: Option<String>,
}

impl ClassHit {
    pub fn from_record(c: &ClassRecord, file_path: String) -> Self {
        Self {
            name: c.name.clone(),
            file_path,
            line_start: c.line_start,
            line_end: c.line_end,
            bases: c.bases.clone(),
            docstring: c.docstring.clone(),
        }
    }
}

/// Лёгкий результат `find_symbol` (locate): функции/классы без тел + переменные/импорты.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolSearchLean {
    pub functions: Vec<FunctionHit>,
    pub classes: Vec<ClassHit>,
    pub variables: Vec<VariableRecord>,
    pub imports: Vec<ImportRecord>,
}

/// Сводка по файлу
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileSummary {
    pub file: FileRecord,
    pub functions: Vec<FunctionRecord>,
    pub classes: Vec<ClassRecord>,
    pub imports: Vec<ImportRecord>,
    pub variables: Vec<VariableRecord>,
    /// true если тела функций/классов были урезаны по лимиту символов
    #[serde(default, skip_serializing_if = "is_false")]
    pub bodies_truncated: bool,
}

/// Лёгкий скелет файла: имена символов и строки без тел.
/// Стоит ~10–50 токенов против 1000–20000 у FileSummary.
/// Используй для навигации и принятия решения — нужен ли полный read_file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileOutlineEntry {
    pub name: String,
    pub kind: String,        // "function" | "class" | "import" | "variable"
    pub line_start: usize,
    pub line_end: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub qualified_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_async: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileOutline {
    pub path: String,
    pub language: String,
    pub lines_total: usize,
    pub symbols: Vec<FileOutlineEntry>,
}

/// Результат grep_body — функция/класс, содержащая паттерн
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrepBodyMatch {
    /// Путь к файлу
    pub file_path: String,
    /// Имя функции или класса
    pub name: String,
    /// Тип: "function" или "class"
    pub kind: String,
    /// Начальная строка
    pub line_start: usize,
    /// Конечная строка
    pub line_end: usize,
    /// Номера строк в файле, где найдено совпадение (первые 3)
    pub match_lines: Vec<usize>,
    /// Общее количество совпадений (только если > 3)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub match_count: Option<usize>,
    /// Контекст вокруг каждого совпадения (если запрошен через context_lines).
    /// Ключ — номер строки в файле, значение — текст. Пуст когда context_lines=0.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub context: Vec<ContextLine>,
}

/// Одна строка контекста для grep_body / grep_text.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextLine {
    pub line: usize,
    pub content: String,
}

/// Результат `read_file` — содержимое (целиком или по диапазону строк) +
/// метаданные индекса.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadFileResult {
    /// Содержимое (плоский текст с переносами строк).
    /// Для oversize-файлов — пустая строка (см. `oversize`).
    pub content: String,
    /// Сколько строк реально вернулось.
    pub lines_returned: usize,
    /// Всего строк в файле.
    pub lines_total: usize,
    /// Пришлось ли усечь по soft-cap.
    pub truncated: bool,
    /// ISO-время последней индексации (для контроля свежести).
    pub indexed_at: String,
    /// Категория файла: "text" — содержимое из БД доступно;
    /// "code" — content из `file_contents` (Phase 2). Если v0.8.0 ещё не
    /// успел сделать backfill для этого файла — content пуст и `oversize=false`.
    pub category: String,
    /// `true` — файл превышает `max_code_file_size_bytes`, content
    /// намеренно не сохранён в индексе. Используйте `get_function`/
    /// `get_class`/`grep_body` для целевого чтения, либо читайте файл
    /// напрямую с диска.
    #[serde(default, skip_serializing_if = "is_false")]
    pub oversize: bool,
    /// Размер файла в байтах (если известен из таблицы `files`).
    /// Полезно вместе с `oversize=true` для понимания насколько файл велик.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_size: Option<i64>,
    /// Эффективный лимит, по которому был принят `oversize` для этого репо
    /// (per-path > [indexer] > hardcoded 5 МБ). Помогает оператору быстро
    /// понять, нужно ли увеличивать лимит в `daemon.toml`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_limit: Option<i64>,
    /// Человекочитаемая подсказка вызывающей стороне. Заполняется только
    /// для `oversize=true` либо когда content code-файла ещё не наполнен
    /// (backfill в процессе).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

/// Запись из `list_files` — метаданные файла без полей хеша.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListedFile {
    pub path: String,
    pub language: String,
    pub lines_total: usize,
    pub size: Option<i64>,
    pub mtime: Option<i64>,
}

/// Результат `stat_file` — метаданные одного файла + флаг наличия content.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatFileResult {
    pub exists: bool,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mtime: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lines_total: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub indexed_at: Option<String>,
    /// Доступен ли content через `read_file`:
    ///   * `"text"` — да, из `text_files`.
    ///   * `"code"` — да, из `file_contents` (Phase 2). См. `oversize`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    /// `true` — файл превышает лимит и content не сохранён в индексе.
    /// Поле появляется только для code-файлов (Phase 2) — для text всегда отсутствует.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oversize: Option<bool>,
}

/// Один матч `grep_text` — строка в text-файле, удовлетворяющая regex.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrepTextMatch {
    pub path: String,
    pub line: usize,
    pub content: String,
    /// Контекст до/после матча, если запрошен. Пуст когда context_lines=0.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub context: Vec<ContextLine>,
}

/// Статус фоновой индексации
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "state")]
pub enum IndexingStatus {
    /// БД ещё не открыта — сервер только что запустился
    Initializing,
    /// Индексация не идёт, данные актуальны
    Ready,
    /// Индексация в процессе
    Indexing {
        /// Текущая фаза
        phase: String,
        /// Обработано файлов
        files_done: usize,
        /// Всего файлов
        files_total: usize,
    },
    /// Индексация завершена
    Completed {
        /// Проиндексировано файлов
        files_indexed: usize,
        /// Время в миллисекундах
        elapsed_ms: u64,
    },
    /// Индексация провалилась
    Failed {
        /// Текст ошибки
        error: String,
    },
}

/// Кандидат при disambiguation имени символа (несколько символов с одинаковым именем)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolCandidate {
    pub name: String,
    /// "function" | "class"
    pub kind: String,
    pub file_path: String,
    pub line_start: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub qualified_name: Option<String>,
}

/// Вызыватель символа
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallerInfo {
    pub caller: String,
    pub file_path: String,
    pub line: usize,
    /// Provenance ребра: `exact` — имя уникально в индексе; `by_name` — несколько
    /// определений (возможна коллизия). Опускается если пусто.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub resolution: String,
}

/// Вызываемый символ
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CalleeInfo {
    pub callee: String,
    pub file_path: String,
    pub line: usize,
    /// Provenance/ООП-резолв ребра:
    ///   * `own`       — метод объявлен в самом классе символа;
    ///   * `inherited` — резолвится по иерархии в предке (через наследование/трейт);
    ///   * `exact`     — имя уникально в индексе (надёжно);
    ///   * `by_name`   — несколько определений (эвристика, возможна коллизия);
    ///   * пусто       — внешний/builtin (определения в индексе нет).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub resolution: String,
    /// Куда резолвится (qualified_name цели), когда удалось однозначно.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_to: Option<String>,
}

/// ООП-контекст метода: кого переопределяет/реализует и кто переопределяет его.
/// Заполняется для методов (qualified_name = Class::method) на основе иерархии типов.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct InheritanceInfo {
    /// Имя типа-владельца метода.
    pub class: String,
    /// Что этот метод переопределяет/реализует (`Ancestor::method`). Реализация
    /// интерфейсного/абстрактного метода тоже здесь — значит метод вызывается полиморфно.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub overrides: Vec<String>,
    /// Кто переопределяет этот метод ниже по иерархии (`Descendant::method`).
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub overridden_by: Vec<String>,
}

/// Полный контекст символа — definition + callers + callees + file_outline + imports
/// за один вызов вместо 7 отдельных запросов.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolContext {
    /// "function" | "class" | "ambiguous" | "not_found"
    pub kind: String,
    /// Список кандидатов при kind="ambiguous" — уточните вызов с file_hint
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub candidates: Vec<SymbolCandidate>,
    /// Полное тело функции или класса (FunctionRecord / ClassRecord)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub definition: Option<serde_json::Value>,
    /// Прямые вызыватели (до 30)
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub callers: Vec<CallerInfo>,
    /// Прямые вызываемые (до 30)
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub callees: Vec<CalleeInfo>,
    /// Скелет файла где определён символ (без тел функций)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_outline: Option<FileOutline>,
    /// Импорты файла где определён символ
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub file_imports: Vec<ImportRecord>,
    /// Веб-маршруты, чей хендлер — этот символ (framework-aware routing).
    /// Пусто если символ не является контроллер-методом или роутинг не распознан.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub routes: Vec<RouteHit>,
    /// ООП-контекст: что метод переопределяет/реализует и кто его переопределяет.
    /// None для не-методов (свободных функций) и неизвестных типов.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub inheritance: Option<InheritanceInfo>,
}

/// Узел транзитивного call-дерева (результат get_callers_tree / get_callees_tree)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallTreeNode {
    pub name: String,
    pub file_path: String,
    pub line: usize,
    /// Глубина от исходной функции (1 = прямой caller/callee)
    pub depth: usize,
}

/// Потенциально мёртвый код — функция без callers в индексе
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeadCodeEntry {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub qualified_name: Option<String>,
    pub file_path: String,
    pub line_start: usize,
    pub line_end: usize,
}

/// Класс, реализующий/наследующий заданный базовый класс или интерфейс
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImplementationRecord {
    pub name: String,
    pub file_path: String,
    pub line_start: usize,
    pub line_end: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bases: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub docstring: Option<String>,
}

/// Функция с метриками сложности (proxy для рефакторинга/ревью).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComplexFunction {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub qualified_name: Option<String>,
    pub file_path: String,
    pub line_start: usize,
    pub line_end: usize,
    /// Длина в строках (line_end - line_start).
    pub span: usize,
    /// Сколько РАЗНЫХ функций вызывает (fan-out) — связность/ответственность.
    pub fan_out: usize,
    /// Сколько РАЗНЫХ функций её вызывают (fan-in).
    pub callers: usize,
}

/// Сводка по модулю (директории) — для архитектурной карты.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModuleStat {
    pub dir: String,
    pub files: usize,
    pub functions: usize,
    pub classes: usize,
}

/// Горячая точка call-графа — часто вызываемый символ.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallHotspot {
    pub name: String,
    /// Число call-site (fan-in).
    pub calls: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_path: Option<String>,
}

/// Язык + число файлов.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LangStat {
    pub language: String,
    pub files: usize,
}

/// Архитектурная карта репозитория — глубокий обзор за ОДИН дешёвый вызов.
/// Цель: дать агенту полную ментальную модель проекта в сотнях токенов вместо
/// десятков grep/read. Все секции усечены до топ-N — компактно по токенам.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoMap {
    pub files: usize,
    pub functions: usize,
    pub classes: usize,
    pub calls: usize,
    pub imports: usize,
    /// Разбивка по языкам.
    pub languages: Vec<LangStat>,
    /// Крупнейшие модули (директории) по числу символов.
    pub modules: Vec<ModuleStat>,
    /// Самые сложные функции (длина + fan-out) — где сосредоточена сложность.
    pub complex_functions: Vec<ComplexFunction>,
    /// Горячие точки call-графа: самые вызываемые ПРОЕКТНЫЕ функции (без stdlib-шума).
    pub call_hotspots: Vec<CallHotspot>,
    /// Кандидаты в точки входа: ничего не вызывает их, но они вызывают многое
    /// (оркестраторы/корни — откуда начинается выполнение).
    pub entry_points: Vec<ComplexFunction>,
    /// Файлы, которые не удалось распарсить (слепая зона индекса). 0 — полное покрытие.
    #[serde(default)]
    pub parse_errors: usize,
}

/// Статистика базы данных
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DbStats {
    pub total_files: usize,
    pub total_functions: usize,
    pub total_classes: usize,
    pub total_imports: usize,
    pub total_calls: usize,
    pub total_variables: usize,
    pub total_text_files: usize,
    /// Файлы, которые не удалось распарсить (слепые зоны индекса). 0 — полное покрытие.
    #[serde(default)]
    pub parse_errors: usize,
    /// Статус фоновой индексации (заполняется MCP-сервером)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub indexing_status: Option<IndexingStatus>,
}

#[cfg(test)]
mod lean_tests {
    use super::*;

    #[test]
    fn function_hit_omits_body_keeps_location() {
        let rec = FunctionRecord {
            id: Some(1),
            file_id: 7,
            name: "do_thing".into(),
            qualified_name: Some("Svc::do_thing".into()),
            line_start: 10,
            line_end: 42,
            args: Some("(x: i32)".into()),
            return_type: Some("bool".into()),
            docstring: Some("does the thing".into()),
            body: "fn do_thing() { /* huge body that must NOT leak */ }".into(),
            is_async: false,
            node_hash: "h".into(),
            override_type: None,
            override_target: None,
        };
        let hit = FunctionHit::from_record(&rec, "src/svc.rs".into());
        let json = serde_json::to_string(&hit).unwrap();
        assert!(!json.contains("\"body\""), "lean-проекция не должна содержать body");
        assert!(!json.contains("huge body"), "тело не должно протекать в lean");
        assert!(json.contains("\"file_path\":\"src/svc.rs\""), "должен быть file_path");
        assert!(json.contains("\"line_start\":10") && json.contains("\"line_end\":42"));
        assert!(json.contains("Svc::do_thing"));
    }

    #[test]
    fn class_hit_omits_body() {
        let rec = ClassRecord {
            id: Some(2),
            file_id: 3,
            name: "Widget".into(),
            line_start: 1,
            line_end: 99,
            bases: Some("Base, Trait".into()),
            docstring: None,
            body: "class Widget { secret }".into(),
            node_hash: "h".into(),
        };
        let hit = ClassHit::from_record(&rec, "src/w.rs".into());
        let json = serde_json::to_string(&hit).unwrap();
        assert!(!json.contains("\"body\""));
        assert!(!json.contains("secret"));
        assert!(json.contains("\"bases\":\"Base, Trait\""));
    }
}
