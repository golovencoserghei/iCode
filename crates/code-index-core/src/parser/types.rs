use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Вычислить SHA-256 хеш строки → hex
pub fn sha256_hex(data: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data.as_bytes());
    hex::encode(hasher.finalize())
}

/// Инкрементальный SHA-256 хеш дерева AST — без материализации S-expression.
/// Обходит дерево рекурсивно, кормит хешер kind + позициями каждого узла.
/// Для файла 80K строк: ~100x быстрее чем to_sexp() + sha256.
pub fn hash_ast(node: tree_sitter::Node) -> String {
    let mut hasher = Sha256::new();
    hash_ast_node(node, &mut hasher);
    hex::encode(hasher.finalize())
}

fn hash_ast_node(node: tree_sitter::Node, hasher: &mut Sha256) {
    // Кормим kind узла + границы (start_byte, end_byte)
    hasher.update(node.kind().as_bytes());
    hasher.update(&node.start_byte().to_le_bytes());
    hasher.update(&node.end_byte().to_le_bytes());
    // Рекурсивно обходим дочерние узлы
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        hash_ast_node(child, hasher);
    }
}

/// Извлечённая функция из AST
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ParsedFunction {
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

/// Извлечённый класс
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParsedClass {
    pub name: String,
    pub line_start: usize,
    pub line_end: usize,
    pub bases: Option<String>,
    pub docstring: Option<String>,
    pub body: String,
    pub node_hash: String,
}

/// Извлечённый импорт
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParsedImport {
    pub module: Option<String>,
    pub name: Option<String>,
    pub alias: Option<String>,
    pub line: usize,
    /// Тип импорта: "import" или "from"
    pub kind: String,
}

/// Извлечённый вызов функции
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ParsedCall {
    pub caller: String,
    pub callee: String,
    pub line: usize,
    /// Получатель вызова (для точного ООП-резолва). Нормализованные значения:
    ///   * `"$this"` / `"self"` / `"parent"` / `"static"` — вызов в пределах своего класса
    ///     (резолвится по иерархии класса вызывателя);
    ///   * `"Имя"` — статический вызов `Имя::m()` или `$имя->m()` (имя переменной/класса);
    ///   * `None` — свободный вызов `f()` либо язык, где receiver не извлекается.
    pub receiver: Option<String>,
}

/// Извлечённая переменная
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParsedVariable {
    pub name: String,
    pub value: Option<String>,
    pub line: usize,
}

/// Извлечённый веб-маршрут фреймворка (framework-aware routing).
/// Связывает HTTP-метод + URL-путь с хендлером (контроллер@метод).
///
/// Пример (Laravel): `Route::get('/users', [UserController::class, 'index'])`
/// → method="GET", path="/users", handler_class="UserController", handler_method="index".
///
/// Для closure-хендлеров `handler_class`/`handler_method` = None (анонимный обработчик).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ParsedRoute {
    /// HTTP-метод в верхнем регистре: GET, POST, PUT, PATCH, DELETE, ANY, MATCH…
    pub method: String,
    /// URL-шаблон как записан в коде (`/users/{id}`). Без раскрытия group-prefix (v1).
    pub path: String,
    /// Класс-контроллер (`UserController`) либо None для closure.
    pub handler_class: Option<String>,
    /// Метод контроллера (`index`) либо None для closure / invokable.
    pub handler_method: Option<String>,
    /// Имя маршрута (`->name('users.index')`), если задано. Пока не извлекается (зарезервировано).
    pub name: Option<String>,
    /// Строка определения маршрута.
    pub line: usize,
}

/// Результат парсинга одного файла
#[derive(Debug, Clone)]
pub struct ParseResult {
    pub functions: Vec<ParsedFunction>,
    pub classes: Vec<ParsedClass>,
    pub imports: Vec<ParsedImport>,
    pub calls: Vec<ParsedCall>,
    pub variables: Vec<ParsedVariable>,
    /// Веб-маршруты фреймворка (framework-aware routing). Пусто для языков/файлов
    /// без распознанного роутинга.
    pub routes: Vec<ParsedRoute>,
    pub lines_total: usize,
    pub ast_hash: String,
}

/// Результат парсинга текстового файла
#[derive(Debug, Clone)]
pub struct TextParseResult {
    pub content: String,
    pub lines_total: usize,
}
