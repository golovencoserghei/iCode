use anyhow::{anyhow, Result};

use super::types::{
    hash_ast, sha256_hex, ParseResult, ParsedCall, ParsedClass, ParsedFunction, ParsedImport,
    ParsedVariable,
};
use super::LanguageParser;

pub struct PhpParser;

impl PhpParser {
    pub fn new() -> Self {
        PhpParser
    }
}

impl LanguageParser for PhpParser {
    fn language_name(&self) -> &str {
        "php"
    }

    fn file_extensions(&self) -> &[&str] {
        &["php", "phtml", "php8", "php7"]
    }

    fn parse(&self, source: &str, _file_path: &str) -> Result<ParseResult> {
        parse_php(source)
    }
}

fn node_text<'a>(node: tree_sitter::Node<'a>, source: &'a [u8]) -> &'a str {
    node.utf8_text(source).unwrap_or("")
}

fn find_child_by_kind<'a>(
    node: tree_sitter::Node<'a>,
    kind: &str,
) -> Option<tree_sitter::Node<'a>> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == kind {
            return Some(child);
        }
    }
    None
}

struct VisitContext<'a> {
    source: &'a [u8],
    functions: Vec<ParsedFunction>,
    classes: Vec<ParsedClass>,
    imports: Vec<ParsedImport>,
    calls: Vec<ParsedCall>,
    variables: Vec<ParsedVariable>,
}

impl<'a> VisitContext<'a> {
    fn new(source: &'a [u8]) -> Self {
        VisitContext {
            source,
            functions: Vec::new(),
            classes: Vec::new(),
            imports: Vec::new(),
            calls: Vec::new(),
            variables: Vec::new(),
        }
    }
}

/// Ищем PHPDoc-комментарий (`/** ... */`) непосредственно перед узлом.
fn extract_phpdoc(node: tree_sitter::Node, source: &[u8]) -> Option<String> {
    let mut prev = node.prev_sibling();
    while let Some(p) = prev {
        match p.kind() {
            "comment" => {
                let text = node_text(p, source);
                if text.starts_with("/**") {
                    return Some(text.to_string());
                }
                // Обычный // или /* */ — пропускаем, смотрим дальше
                prev = p.prev_sibling();
            }
            // Пропускаем пустые узлы (whitespace, newline)
            _ if p.is_extra() || !p.is_named() => {
                prev = p.prev_sibling();
            }
            _ => break,
        }
    }
    None
}

fn visit_node(
    node: tree_sitter::Node,
    ctx: &mut VisitContext,
    class_name: Option<&str>,
    current_func: Option<&str>,
) {
    match node.kind() {
        "function_definition" => {
            visit_function(node, ctx, class_name, current_func);
        }
        "method_declaration" => {
            visit_method(node, ctx, class_name);
        }
        "class_declaration" | "interface_declaration" | "trait_declaration"
        | "enum_declaration" => {
            visit_class(node, ctx);
        }
        "namespace_use_declaration" => {
            visit_use(node, ctx);
        }
        "function_call_expression" => {
            visit_function_call(node, ctx, current_func);
            recurse_into_arguments(node, ctx, class_name, current_func);
        }
        "member_call_expression"
        | "nullsafe_member_call_expression"
        | "scoped_call_expression" => {
            visit_method_call(node, ctx, current_func);
            // Рекурсия в object-часть для цепочек: $this->a()->b()->c()
            // AST: member_call[object=member_call[object=...], name=c]
            if let Some(obj) = node.child_by_field_name("object") {
                visit_node(obj, ctx, class_name, current_func);
            }
            recurse_into_arguments(node, ctx, class_name, current_func);
        }
        "expression_statement" => {
            // Переменные на уровне файла/программы
            if class_name.is_none() && current_func.is_none() {
                let mut c = node.walk();
                for child in node.children(&mut c) {
                    if child.kind() == "assignment_expression" {
                        visit_assignment(child, ctx);
                    }
                }
            }
            let mut c = node.walk();
            for child in node.children(&mut c) {
                visit_node(child, ctx, class_name, current_func);
            }
        }
        "const_declaration" => {
            visit_const(node, ctx);
        }
        _ => {
            let mut c = node.walk();
            for child in node.children(&mut c) {
                visit_node(child, ctx, class_name, current_func);
            }
        }
    }
}

fn recurse_into_arguments(
    call_node: tree_sitter::Node,
    ctx: &mut VisitContext,
    class_name: Option<&str>,
    current_func: Option<&str>,
) {
    if let Some(args) = call_node.child_by_field_name("arguments")
        .or_else(|| find_child_by_kind(call_node, "arguments"))
    {
        let mut c = args.walk();
        for child in args.children(&mut c) {
            visit_node(child, ctx, class_name, current_func);
        }
    }
}

fn visit_function(
    node: tree_sitter::Node,
    ctx: &mut VisitContext,
    class_name: Option<&str>,
    _current_func: Option<&str>,
) {
    let source = ctx.source;

    let name = node
        .child_by_field_name("name")
        .map(|n| node_text(n, source).to_string())
        .unwrap_or_default();

    if name.is_empty() {
        return;
    }

    let qualified_name = class_name.map(|cn| format!("{}::{}", cn, name));
    let line_start = node.start_position().row + 1;
    let line_end = node.end_position().row + 1;

    let args = node
        .child_by_field_name("parameters")
        .map(|n| node_text(n, source).to_string());

    let return_type = node
        .child_by_field_name("return_type")
        .map(|n| node_text(n, source).to_string());

    let docstring = extract_phpdoc(node, source);
    let body = node_text(node, source).to_string();
    let node_hash = sha256_hex(&body);

    ctx.functions.push(ParsedFunction {
        name: name.clone(),
        qualified_name,
        line_start,
        line_end,
        args,
        return_type,
        docstring,
        body,
        is_async: false,
        node_hash,
        ..Default::default()
    });

    // Рекурсия в тело функции
    if let Some(body_node) = node
        .child_by_field_name("body")
        .or_else(|| find_child_by_kind(node, "compound_statement"))
    {
        let mut c = body_node.walk();
        for child in body_node.children(&mut c) {
            visit_node(child, ctx, class_name, Some(&name));
        }
    }
}

fn visit_method(
    node: tree_sitter::Node,
    ctx: &mut VisitContext,
    class_name: Option<&str>,
) {
    let source = ctx.source;

    let name = node
        .child_by_field_name("name")
        .map(|n| node_text(n, source).to_string())
        .unwrap_or_default();

    if name.is_empty() {
        return;
    }

    let qualified_name = class_name.map(|cn| format!("{}::{}", cn, name));
    let line_start = node.start_position().row + 1;
    let line_end = node.end_position().row + 1;

    let args = node
        .child_by_field_name("parameters")
        .map(|n| node_text(n, source).to_string());

    let return_type = node
        .child_by_field_name("return_type")
        .map(|n| node_text(n, source).to_string());

    let docstring = extract_phpdoc(node, source);
    let body = node_text(node, source).to_string();
    let node_hash = sha256_hex(&body);

    ctx.functions.push(ParsedFunction {
        name: name.clone(),
        qualified_name,
        line_start,
        line_end,
        args,
        return_type,
        docstring,
        body,
        is_async: false,
        node_hash,
        ..Default::default()
    });

    // Рекурсия в тело метода (если не abstract)
    if let Some(body_node) = node
        .child_by_field_name("body")
        .or_else(|| find_child_by_kind(node, "compound_statement"))
    {
        let mut c = body_node.walk();
        for child in body_node.children(&mut c) {
            visit_node(child, ctx, class_name, Some(&name));
        }
    }
}

fn visit_class(node: tree_sitter::Node, ctx: &mut VisitContext) {
    let source = ctx.source;
    let kind = node.kind();

    let name = node
        .child_by_field_name("name")
        .map(|n| node_text(n, source).to_string())
        .unwrap_or_default();

    if name.is_empty() {
        return;
    }

    let line_start = node.start_position().row + 1;
    let line_end = node.end_position().row + 1;

    // extends (base_clause) или implements
    let bases = node
        .child_by_field_name("base_clause")
        .or_else(|| node.child_by_field_name("extends"))
        .map(|n| node_text(n, source).to_string());

    let docstring = extract_phpdoc(node, source);
    let body = node_text(node, source).to_string();
    let node_hash = sha256_hex(&body);

    // Префикс для интерфейсов/трейтов
    let display_name = match kind {
        "interface_declaration" => format!("interface {}", name),
        "trait_declaration" => format!("trait {}", name),
        "enum_declaration" => format!("enum {}", name),
        _ => name.clone(),
    };

    ctx.classes.push(ParsedClass {
        name: display_name,
        line_start,
        line_end,
        bases,
        docstring,
        body,
        node_hash,
    });

    // Рекурсия в тело класса
    if let Some(body_node) = node
        .child_by_field_name("body")
        .or_else(|| find_child_by_kind(node, "declaration_list"))
        .or_else(|| find_child_by_kind(node, "enum_declaration_list"))
    {
        let mut c = body_node.walk();
        for child in body_node.children(&mut c) {
            visit_node(child, ctx, Some(&name), None);
        }
    }
}

fn visit_use(node: tree_sitter::Node, ctx: &mut VisitContext) {
    let source = ctx.source;
    let line = node.start_position().row + 1;

    // Тип use: "function" | "const" | (пусто = класс)
    let kind = find_child_by_kind(node, "use_type")
        .map(|n| node_text(n, source).to_string())
        .unwrap_or_else(|| "class".to_string());

    // Для сгруппированного use (`use A\B\{C, D}`) prefix — это `namespace_name`,
    // который является прямым потомком `namespace_use_declaration` (не группы).
    let group_prefix = find_child_by_kind(node, "namespace_name")
        .map(|n| node_text(n, source).to_string());

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "namespace_use_clause" => {
                // Простой use: `use App\Models\User;`
                let qname = child
                    .child_by_field_name("name")
                    .or_else(|| find_child_by_kind(child, "qualified_name"))
                    .or_else(|| find_child_by_kind(child, "name"))
                    .map(|n| node_text(n, source).to_string());

                let alias = child
                    .child_by_field_name("alias")
                    .map(|n| node_text(n, source).to_string());

                let (module, name) = split_fqn(qname.as_deref());

                ctx.imports.push(ParsedImport {
                    module,
                    name,
                    alias,
                    line,
                    kind: kind.clone(),
                });
            }
            "namespace_use_group" => {
                // Сгруппированный use: `use App\Services\{AuthService, CacheService}`
                // prefix лежит в `group_prefix` (сиблинг группы)
                let mut c2 = child.walk();
                for clause in child.children(&mut c2) {
                    if clause.kind() == "namespace_use_clause" {
                        let name_str = clause
                            .child_by_field_name("name")
                            .or_else(|| find_child_by_kind(clause, "qualified_name"))
                            .or_else(|| find_child_by_kind(clause, "name"))
                            .map(|n| node_text(n, source).to_string());

                        let alias = clause
                            .child_by_field_name("alias")
                            .map(|n| node_text(n, source).to_string());

                        ctx.imports.push(ParsedImport {
                            module: group_prefix.clone(),
                            name: name_str,
                            alias,
                            line,
                            kind: kind.clone(),
                        });
                    }
                }
            }
            _ => {}
        }
    }
}

/// Разбить полное имя `App\Models\User` → (`Some("App\Models")`, `Some("User")`)
fn split_fqn(fqn: Option<&str>) -> (Option<String>, Option<String>) {
    match fqn {
        None => (None, None),
        Some(s) => {
            if let Some(pos) = s.rfind('\\') {
                (Some(s[..pos].to_string()), Some(s[pos + 1..].to_string()))
            } else {
                (None, Some(s.to_string()))
            }
        }
    }
}

fn visit_function_call(
    node: tree_sitter::Node,
    ctx: &mut VisitContext,
    current_func: Option<&str>,
) {
    let source = ctx.source;
    let line = node.start_position().row + 1;

    let callee = node
        .child_by_field_name("function")
        .map(|n| node_text(n, source).to_string())
        .unwrap_or_default();

    if !callee.is_empty() {
        ctx.calls.push(ParsedCall {
            caller: current_func.unwrap_or("<module>").to_string(),
            callee,
            line,
        });
    }
}

fn visit_method_call(
    node: tree_sitter::Node,
    ctx: &mut VisitContext,
    current_func: Option<&str>,
) {
    let source = ctx.source;
    let line = node.start_position().row + 1;

    // member_call: $obj->name(...)  /  scoped_call: Cls::name(...)
    let callee = node
        .child_by_field_name("name")
        .map(|n| node_text(n, source).to_string())
        .unwrap_or_default();

    if !callee.is_empty() {
        ctx.calls.push(ParsedCall {
            caller: current_func.unwrap_or("<module>").to_string(),
            callee,
            line,
        });
    }
}

fn visit_assignment(node: tree_sitter::Node, ctx: &mut VisitContext) {
    let source = ctx.source;
    let line = node.start_position().row + 1;

    let name = node
        .child_by_field_name("left")
        .or_else(|| node.child(0))
        .map(|n| node_text(n, source).to_string())
        .unwrap_or_default();

    if name.is_empty() || name == "=" {
        return;
    }

    let value = node
        .child_by_field_name("right")
        .or_else(|| node.child(2))
        .map(|n| {
            let t = node_text(n, source).to_string();
            if t.chars().count() > 200 {
                t.chars().take(200).collect()
            } else {
                t
            }
        });

    ctx.variables.push(ParsedVariable { name, value, line });
}

fn visit_const(node: tree_sitter::Node, ctx: &mut VisitContext) {
    let source = ctx.source;
    let line = node.start_position().row + 1;

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "const_element" {
            let name = child
                .child_by_field_name("name")
                .or_else(|| find_child_by_kind(child, "name"))
                .map(|n| node_text(n, source).to_string())
                .unwrap_or_default();

            let value = child
                .child_by_field_name("value")
                .map(|n| node_text(n, source).to_string());

            if !name.is_empty() {
                ctx.variables.push(ParsedVariable { name, value, line });
            }
        }
    }
}

fn parse_php(source: &str) -> Result<ParseResult> {
    let mut ts_parser = tree_sitter::Parser::new();
    ts_parser
        .set_language(&tree_sitter_php::LANGUAGE_PHP.into())
        .map_err(|e| anyhow!("Ошибка установки языка tree-sitter-php: {}", e))?;

    let tree = ts_parser
        .parse(source, None)
        .ok_or_else(|| anyhow!("tree-sitter не смог распарсить PHP файл"))?;

    let root = tree.root_node();
    let source_bytes = source.as_bytes();

    let ast_hash = hash_ast(root);
    let lines_total = source.lines().count();

    let mut ctx = VisitContext::new(source_bytes);

    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        visit_node(child, &mut ctx, None, None);
    }

    Ok(ParseResult {
        functions: ctx.functions,
        classes: ctx.classes,
        imports: ctx.imports,
        calls: ctx.calls,
        variables: ctx.variables,
        lines_total,
        ast_hash,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::LanguageParser;

    #[test]
    fn test_parse_simple_function() {
        let parser = PhpParser::new();
        let source = r#"<?php
/**
 * Приветствие пользователя.
 */
function greet(string $name): string {
    return "Hello, $name!";
}
"#;
        let result = parser.parse(source, "test.php").unwrap();
        assert_eq!(result.functions.len(), 1);
        assert_eq!(result.functions[0].name, "greet");
        assert!(result.functions[0].docstring.is_some());
        // tree-sitter-php возвращает только тип без двоеточия
        assert_eq!(result.functions[0].return_type.as_deref(), Some("string"));
    }

    #[test]
    fn test_parse_class_with_methods() {
        let parser = PhpParser::new();
        let source = r#"<?php
class UserRepository extends BaseRepository {
    public function find(int $id): ?User {
        return $this->db->find($id);
    }

    private function buildQuery(): QueryBuilder {
        return new QueryBuilder();
    }
}
"#;
        let result = parser.parse(source, "test.php").unwrap();
        assert_eq!(result.classes.len(), 1);
        assert_eq!(result.classes[0].name, "UserRepository");
        assert_eq!(result.functions.len(), 2);
        assert_eq!(
            result.functions[0].qualified_name,
            Some("UserRepository::find".to_string())
        );
    }

    #[test]
    fn test_parse_interface_and_trait() {
        let parser = PhpParser::new();
        let source = r#"<?php
interface Renderable {
    public function render(): string;
}

trait Singleton {
    private static ?self $instance = null;

    public static function getInstance(): static {
        return static::$instance ??= new static();
    }
}
"#;
        let result = parser.parse(source, "test.php").unwrap();
        assert_eq!(result.classes.len(), 2);
        assert!(result.classes[0].name.starts_with("interface "));
        assert!(result.classes[1].name.starts_with("trait "));
    }

    #[test]
    fn test_parse_use_statements() {
        let parser = PhpParser::new();
        let source = r#"<?php
use App\Models\User;
use App\Services\{AuthService, CacheService};
use function array_map;
use const PHP_EOL;
"#;
        let result = parser.parse(source, "test.php").unwrap();
        assert!(result.imports.len() >= 4);
        let user_import = result.imports.iter().find(|i| i.name.as_deref() == Some("User"));
        assert!(user_import.is_some());
        assert_eq!(user_import.unwrap().module.as_deref(), Some("App\\Models"));
    }

    #[test]
    fn test_parse_calls() {
        let parser = PhpParser::new();
        let source = r#"<?php
function process(array $items): void {
    $filtered = array_filter($items, fn($x) => $x > 0);
    $this->save($filtered);
    Logger::info('done');
}
"#;
        let result = parser.parse(source, "test.php").unwrap();
        assert!(!result.calls.is_empty());
        let save_call = result.calls.iter().find(|c| c.callee == "save");
        assert!(save_call.is_some());
    }

    #[test]
    fn test_file_extensions() {
        let parser = PhpParser::new();
        assert!(parser.file_extensions().contains(&"php"));
        assert!(parser.file_extensions().contains(&"phtml"));
    }

    // ── Расширенные тесты графа вызовов ───────────────────────────────────

    /// $this->method() — instance call
    #[test]
    fn test_call_instance_method() {
        let parser = PhpParser::new();
        let source = r#"<?php
class Service {
    public function run(): void {
        $result = $this->compute();
        $this->save($result);
    }
    private function compute(): int { return 42; }
    private function save(int $v): void {}
}
"#;
        let result = parser.parse(source, "test.php").unwrap();
        let callee_names: Vec<&str> = result.calls.iter().map(|c| c.callee.as_str()).collect();
        assert!(callee_names.contains(&"compute"), "должен захватить $this->compute()");
        assert!(callee_names.contains(&"save"),    "должен захватить $this->save()");
        // caller — метод run
        let compute_call = result.calls.iter().find(|c| c.callee == "compute").unwrap();
        assert_eq!(compute_call.caller, "run");
    }

    /// Class::method() — static call
    #[test]
    fn test_call_static_method() {
        let parser = PhpParser::new();
        let source = r#"<?php
class Controller {
    public function handle(): void {
        $data = RequestUtils::normalize($this->request->getContent());
        Logger::info('done');
    }
}
"#;
        let result = parser.parse(source, "test.php").unwrap();
        let callee_names: Vec<&str> = result.calls.iter().map(|c| c.callee.as_str()).collect();
        assert!(callee_names.contains(&"normalize"), "Class::normalize() должен захватиться");
        assert!(callee_names.contains(&"info"),      "Logger::info() должен захватиться");
        let norm = result.calls.iter().find(|c| c.callee == "normalize").unwrap();
        assert_eq!(norm.caller, "handle");
    }

    /// Цепочки: $this->a()->b() — оба вызова должны быть захвачены
    #[test]
    fn test_call_chained() {
        let parser = PhpParser::new();
        let source = r#"<?php
class Repo {
    public function findUser(int $id): void {
        $user = $this->getDoctrine()->getManager()->getRepository(User::class)->find($id);
    }
}
"#;
        let result = parser.parse(source, "test.php").unwrap();
        let callee_names: Vec<&str> = result.calls.iter().map(|c| c.callee.as_str()).collect();
        assert!(callee_names.contains(&"getDoctrine"),  "должен захватить getDoctrine()");
        assert!(callee_names.contains(&"getManager"),   "должен захватить getManager()");
        assert!(callee_names.contains(&"getRepository"),"должен захватить getRepository()");
        assert!(callee_names.contains(&"find"),         "должен захватить find()");
    }

    /// parent::method() — вызов родителя
    #[test]
    fn test_call_parent() {
        let parser = PhpParser::new();
        let source = r#"<?php
class Child extends Base {
    public function __construct(Dep $dep) {
        parent::__construct($dep);
        $this->init();
    }
}
"#;
        let result = parser.parse(source, "test.php").unwrap();
        let callee_names: Vec<&str> = result.calls.iter().map(|c| c.callee.as_str()).collect();
        assert!(callee_names.contains(&"__construct"), "parent::__construct() должен захватиться");
        assert!(callee_names.contains(&"init"),        "init() должен захватиться");
    }

    /// Caller — правильно назначается при вложенных классах
    #[test]
    fn test_caller_correctness() {
        let parser = PhpParser::new();
        let source = r#"<?php
class A {
    public function foo(): void {
        $this->bar();
    }
    public function baz(): void {
        $this->qux();
    }
}
"#;
        let result = parser.parse(source, "test.php").unwrap();
        let bar_call = result.calls.iter().find(|c| c.callee == "bar").unwrap();
        let qux_call = result.calls.iter().find(|c| c.callee == "qux").unwrap();
        assert_eq!(bar_call.caller, "foo", "bar() должен вызываться из foo");
        assert_eq!(qux_call.caller, "baz", "qux() должен вызываться из baz");
    }

    /// global-level вызовы получают caller="<module>"
    #[test]
    fn test_caller_module_level() {
        let parser = PhpParser::new();
        let source = r#"<?php
require_once __DIR__ . '/vendor/autoload.php';
$app = new Application();
$app->run();
"#;
        let result = parser.parse(source, "test.php").unwrap();
        // require_once это не function_call, run() и т.п. — module level
        let run_call = result.calls.iter().find(|c| c.callee == "run");
        if let Some(c) = run_call {
            assert_eq!(c.caller, "<module>", "вызов на уровне файла должен иметь caller=<module>");
        }
    }

    /// Nullsafe: $obj?->method()
    #[test]
    fn test_call_nullsafe() {
        let parser = PhpParser::new();
        let source = r#"<?php
class Handler {
    public function handle(?User $user): void {
        $name = $user?->getName();
        $email = $user?->getEmail();
    }
}
"#;
        let result = parser.parse(source, "test.php").unwrap();
        let callee_names: Vec<&str> = result.calls.iter().map(|c| c.callee.as_str()).collect();
        assert!(callee_names.contains(&"getName"),  "$user?->getName() должен захватиться");
        assert!(callee_names.contains(&"getEmail"), "$user?->getEmail() должен захватиться");
    }

    /// PHPDoc захватывается как docstring
    #[test]
    fn test_phpdoc_docstring() {
        let parser = PhpParser::new();
        let source = r#"<?php
class UserService {
    /**
     * Найти пользователя по ID.
     *
     * @param int $id
     * @return User|null
     */
    public function find(int $id): ?User {
        return $this->repo->find($id);
    }
}
"#;
        let result = parser.parse(source, "test.php").unwrap();
        assert_eq!(result.functions.len(), 1);
        let f = &result.functions[0];
        assert!(f.docstring.is_some(), "PHPDoc должен стать docstring");
        assert!(f.docstring.as_deref().unwrap().contains("Найти пользователя"),
                "docstring должен содержать текст комментария");
    }

    /// Namespace + класс: qualified_name правильно формируется
    #[test]
    fn test_qualified_name_in_class() {
        let parser = PhpParser::new();
        let source = r#"<?php
namespace App\Service;

class EmailService {
    public function send(string $to): void {}
    public function queue(string $to): void {}
}
"#;
        let result = parser.parse(source, "test.php").unwrap();
        assert_eq!(result.functions.len(), 2);
        let send = result.functions.iter().find(|f| f.name == "send").unwrap();
        assert_eq!(send.qualified_name.as_deref(), Some("EmailService::send"),
                   "qualified_name должен быть ClassName::method");
    }
}
