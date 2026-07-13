//! Shared JavaScript/TypeScript parser core (tree-sitter).
//!
//! TypeScript's grammar is a strict superset of JavaScript's, so both languages
//! are parsed by the same recursive descent here; the public `parse_javascript`
//! / `parse_typescript` wrappers only differ in the tree-sitter grammar they
//! pass in and the `Language` tag they stamp on extracted symbols. This keeps the
//! extraction logic (qualified-name threading, call receivers, import flattening,
//! Express route detection) in one place rather than duplicated per language.
//!
//! Extracted node set (mirrors the other parsers):
//!
//! - functions: `function_declaration`, `generator_function_declaration`,
//!   `function_expression`, `method_definition` (a class method →
//!   `Class.method`), and arrow / function expressions bound to a
//!   `const`/`let`/`var` (`const foo = () => {}` → name from the declarator).
//!   `is_async` comes from a leading `async` token.
//! - classes:   `class_declaration` / `class` → `ClassDef` (bases = `extends`
//!   plus, in TS, `implements`). TS-only: `interface_declaration` (bases =
//!   extended interfaces) and `enum_declaration` → `ClassDef`.
//! - imports:   `import_statement` → one `Import` per imported binding — named
//!   (`{a, b as c}`), default, and namespace (`* as ns`) — with the source
//!   string as `module` (kind `"import"`). Only ESM `import_statement` is
//!   extracted; CommonJS `require(...)` is NOT handled.
//! - calls:     `call_expression` → free (`f()`) or member (`obj.m()`, receiver =
//!   the object text, e.g. `this` / `obj` / `a.b`), attributed to the enclosing
//!   function/method (top-level calls dropped).
//! - routes:    best-effort Express — `app.<verb>(...)` / `router.<verb>(...)`
//!   where verb ∈ get/post/put/patch/delete/all.

use icode_core::model::{Call, ClassDef, FunctionDef, Import, Language, Route};
use sha2::{Digest, Sha256};
use tree_sitter::{Language as TsLanguage, Node, Parser};

use super::ParseResult;

/// Parse `source` with the given tree-sitter `grammar`, stamping extracted
/// symbols with `language` (JavaScript or TypeScript). On a tree-sitter
/// init/parse failure returns an empty result (the indexer records the parse
/// error separately); never panics on malformed input.
pub(super) fn parse(source: &str, path: &str, grammar: TsLanguage, language: Language) -> ParseResult {
    let lines_total = source.lines().count().max(1) as u32;
    let ast_hash = hash_source(source);

    let mut parser = Parser::new();
    if parser.set_language(&grammar).is_err() {
        return ParseResult { lines_total, ast_hash, ..Default::default() };
    }
    let tree = match parser.parse(source, None) {
        Some(t) => t,
        None => return ParseResult { lines_total, ast_hash, ..Default::default() },
    };

    let bytes = source.as_bytes();
    let mut acc = Acc::default();
    let ctx = Ctx { language, class_name: None, caller: None };
    walk(tree.root_node(), bytes, path, &ctx, &mut acc);

    ParseResult {
        lines_total,
        ast_hash,
        functions: acc.functions,
        classes: acc.classes,
        imports: acc.imports,
        calls: acc.calls,
        routes: acc.routes,
    }
}

/// Accumulated nodes for one file.
#[derive(Default)]
struct Acc {
    functions: Vec<FunctionDef>,
    classes: Vec<ClassDef>,
    imports: Vec<Import>,
    calls: Vec<Call>,
    routes: Vec<Route>,
}

/// Lexical context carried down the walk. Owned (cloned at the few nesting
/// boundaries — class/fn entry) to keep the recursion lifetime-free. No `Default`
/// — `language` (from `icode-core`) has none; the root `Ctx` is built in `parse`.
#[derive(Clone)]
struct Ctx {
    /// JavaScript vs TypeScript — stamped onto every extracted symbol.
    language: Language,
    /// Enclosing class name, so a method gets a `Class.method` qualified name.
    class_name: Option<String>,
    /// Qualified name of the enclosing function/method (the call `caller`).
    caller: Option<String>,
}

fn hash_source(source: &str) -> String {
    let mut h = Sha256::new();
    h.update(source.as_bytes());
    hex(&h.finalize())
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Recursive descent. Functions/classes/imports are extracted on the way down;
/// `Ctx` threads the enclosing class and caller so methods and calls are
/// attributed correctly. Route detection piggy-backs on `call_expression`.
fn walk(node: Node<'_>, src: &[u8], path: &str, ctx: &Ctx, acc: &mut Acc) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "function_declaration"
            | "generator_function_declaration"
            | "function_expression"
            | "method_definition" => {
                enter_function(child, child, None, src, path, ctx, acc);
            }
            "class_declaration" | "class" => {
                enter_class(child, src, path, ctx, acc);
            }
            "import_statement" => {
                extract_import(child, src, path, &mut acc.imports);
            }
            "interface_declaration" => {
                if let Some(c) = extract_interface(child, src, path, ctx.language) {
                    acc.classes.push(c);
                }
                walk(child, src, path, ctx, acc);
            }
            "enum_declaration" => {
                if let Some(c) = extract_enum(child, src, path, ctx.language) {
                    acc.classes.push(c);
                }
                walk(child, src, path, ctx, acc);
            }
            // `const foo = (...) => {}` / `const foo = function () {}` — the
            // function value is named after the declarator. Other declarators
            // are walked normally.
            "lexical_declaration" | "variable_declaration" => {
                walk_declaration(child, src, path, ctx, acc);
            }
            "call_expression" => {
                if let Some(c) = extract_call(child, src, path, ctx.caller.as_deref()) {
                    acc.calls.push(c);
                }
                try_extract_route(child, src, path, &mut acc.routes);
                walk(child, src, path, ctx, acc);
            }
            _ => walk(child, src, path, ctx, acc),
        }
    }
}

/// A `lexical_declaration`/`variable_declaration` may bind an arrow/function
/// expression to a name (`const f = () => {}`). For each declarator with such a
/// value, extract a named function; otherwise descend normally so any nested
/// definitions/calls inside the initialiser are still visited.
fn walk_declaration(node: Node<'_>, src: &[u8], path: &str, ctx: &Ctx, acc: &mut Acc) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() != "variable_declarator" {
            walk(child, src, path, ctx, acc);
            continue;
        }
        let value = child.child_by_field_name("value");
        match value.map(|v| v.kind()) {
            Some("arrow_function") | Some("function_expression") => {
                let name_node = child.child_by_field_name("name");
                let func = value.unwrap();
                enter_function(func, func, name_node, src, path, ctx, acc);
            }
            _ => walk(child, src, path, ctx, acc),
        }
    }
}

/// Extract a function/method/arrow and recurse into its body with this function
/// as the caller. `def_node` is the node used for `args`/`is_async`/span;
/// `name_node` (when given) overrides the name — used for `const f = () => {}`,
/// where the name lives on the declarator, not the arrow.
fn enter_function(
    def_node: Node<'_>,
    body_owner: Node<'_>,
    name_node: Option<Node<'_>>,
    src: &[u8],
    path: &str,
    ctx: &Ctx,
    acc: &mut Acc,
) {
    if let Some(f) = extract_function(def_node, name_node, src, path, ctx) {
        let qname = f.qualified_name.clone();
        acc.functions.push(f);
        // The body opens a fresh class scope (a class defined inside a function
        // is flat), with this function as the caller for nested calls.
        let inner = Ctx { language: ctx.language, class_name: None, caller: Some(qname) };
        if let Some(body) = body_owner.child_by_field_name("body") {
            walk(body, src, path, &inner, acc);
        } else {
            // Arrow with an expression body (`x => x + 1`) has no `body` field
            // as a block; descend the whole node to catch any calls.
            walk(body_owner, src, path, &inner, acc);
        }
    }
}

/// Enter a class: record the `ClassDef`, then descend the body with the class
/// name pushed so methods become `Class.method`.
fn enter_class(node: Node<'_>, src: &[u8], path: &str, ctx: &Ctx, acc: &mut Acc) {
    if let Some(c) = extract_class(node, src, path, ctx.language) {
        let class_name = c.name.clone();
        acc.classes.push(c);
        let inner = Ctx {
            language: ctx.language,
            class_name: Some(class_name),
            caller: ctx.caller.clone(),
        };
        if let Some(body) = node.child_by_field_name("body") {
            walk(body, src, path, &inner, acc);
        }
    }
}

// ──────────────────────────── functions ────────────────────────────

/// Build a `FunctionDef`. The name is `name_node` when supplied (const-bound
/// arrows), else the node's own `name` field. Anonymous functions with no name
/// from either source are skipped (returns `None`). A class context prefixes the
/// qualified name as `Class.method`.
fn extract_function(
    node: Node<'_>,
    name_node: Option<Node<'_>>,
    src: &[u8],
    path: &str,
    ctx: &Ctx,
) -> Option<FunctionDef> {
    let name = name_node
        .or_else(|| node.child_by_field_name("name"))
        .and_then(|n| node_text(n, src))?;

    let qualified_name = match &ctx.class_name {
        Some(cls) => format!("{cls}.{name}"),
        None => name.clone(),
    };

    let args = node
        .child_by_field_name("parameters")
        .and_then(|n| node_text(n, src))
        .or_else(|| {
            // Arrow with a single unparenthesised parameter (`x => ...`) exposes
            // it as the `parameter` field instead of `formal_parameters`.
            node.child_by_field_name("parameter").and_then(|n| node_text(n, src)).map(|p| format!("({p})"))
        })
        .unwrap_or_else(|| "()".to_string());

    let return_type = node.child_by_field_name("return_type").and_then(|n| node_text(n, src));
    let is_async = has_async(node, src);
    let body = node_text(node, src).unwrap_or_default();

    // tree-sitter rows are 0-based; the contract wants 1-based inclusive lines.
    let line_start = node.start_position().row as u32 + 1;
    let line_end = node.end_position().row as u32 + 1;

    Some(FunctionDef {
        name,
        qualified_name,
        path: path.to_string(),
        language: ctx.language,
        line_start,
        line_end,
        args,
        return_type,
        docstring: None,
        body,
        is_async,
        override_type: None,
        override_target: None,
    })
}

/// `async` surfaces as an `async` token among the definition's direct children.
fn has_async(node: Node<'_>, src: &[u8]) -> bool {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "async" {
            return true;
        }
    }
    let _ = src;
    false
}

// ──────────────────────────── classes ────────────────────────────

/// `class_declaration` / `class` → `ClassDef`. `bases` collects the `extends`
/// superclass and (TS) each `implements` interface from the `class_heritage`.
fn extract_class(node: Node<'_>, src: &[u8], path: &str, language: Language) -> Option<ClassDef> {
    let name = node.child_by_field_name("name").and_then(|n| node_text(n, src))?;
    let bases = class_bases(node, src);

    let body = node_text(node, src).unwrap_or_default();
    let line_start = node.start_position().row as u32 + 1;
    let line_end = node.end_position().row as u32 + 1;

    Some(ClassDef {
        name: name.clone(),
        qualified_name: name,
        path: path.to_string(),
        language,
        line_start,
        line_end,
        bases,
        docstring: None,
        body,
    })
}

/// Base types from a class's `class_heritage`. Handles both shapes the grammar
/// produces:
/// - JS: `class_heritage` holds the bare extended `identifier` directly.
/// - TS: `class_heritage` wraps an `extends_clause` (`value` = base) and an
///   `implements_clause` (each `type_identifier` = an interface).
fn class_bases(class_node: Node<'_>, src: &[u8]) -> Vec<String> {
    let heritage = match find_child(class_node, "class_heritage") {
        Some(h) => h,
        None => return Vec::new(),
    };
    let mut out = Vec::new();
    let mut cursor = heritage.walk();
    for child in heritage.children(&mut cursor) {
        match child.kind() {
            // JS form: the extended class is a bare identifier child.
            "identifier" => push_text(&mut out, child, src),
            "extends_clause" => {
                // TS: `extends Base` — the base is the `value` field; fall back
                // to any identifier children for safety.
                if let Some(v) = child.child_by_field_name("value") {
                    push_text(&mut out, v, src);
                } else {
                    push_named_idents(child, src, &mut out);
                }
            }
            "implements_clause" => push_named_idents(child, src, &mut out),
            _ => {}
        }
    }
    out
}

/// TS `interface_declaration` → `ClassDef` (bases = extended interfaces from the
/// `extends_type_clause`).
fn extract_interface(node: Node<'_>, src: &[u8], path: &str, language: Language) -> Option<ClassDef> {
    let name = node.child_by_field_name("name").and_then(|n| node_text(n, src))?;

    let mut bases = Vec::new();
    if let Some(clause) = find_child(node, "extends_type_clause") {
        let mut cursor = clause.walk();
        for child in clause.children(&mut cursor) {
            if matches!(child.kind(), "type_identifier" | "generic_type" | "nested_type_identifier") {
                push_text(&mut bases, child, src);
            }
        }
    }

    let body = node_text(node, src).unwrap_or_default();
    let line_start = node.start_position().row as u32 + 1;
    let line_end = node.end_position().row as u32 + 1;
    Some(ClassDef {
        name: name.clone(),
        qualified_name: name,
        path: path.to_string(),
        language,
        line_start,
        line_end,
        bases,
        docstring: None,
        body,
    })
}

/// TS `enum_declaration` → `ClassDef` (no bases).
fn extract_enum(node: Node<'_>, src: &[u8], path: &str, language: Language) -> Option<ClassDef> {
    let name = node.child_by_field_name("name").and_then(|n| node_text(n, src))?;
    let body = node_text(node, src).unwrap_or_default();
    let line_start = node.start_position().row as u32 + 1;
    let line_end = node.end_position().row as u32 + 1;
    Some(ClassDef {
        name: name.clone(),
        qualified_name: name,
        path: path.to_string(),
        language,
        line_start,
        line_end,
        bases: Vec::new(),
        docstring: None,
        body,
    })
}

/// Push every named `identifier`/`type_identifier` child's text (skips the
/// `implements`/`extends` keyword tokens, which are unnamed).
fn push_named_idents(node: Node<'_>, src: &[u8], out: &mut Vec<String>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if matches!(child.kind(), "identifier" | "type_identifier" | "nested_type_identifier" | "generic_type") {
            push_text(out, child, src);
        }
    }
}

fn push_text(out: &mut Vec<String>, node: Node<'_>, src: &[u8]) {
    if let Some(t) = node_text(node, src) {
        let t = t.trim();
        if !t.is_empty() {
            out.push(t.to_string());
        }
    }
}

// ──────────────────────────── imports ────────────────────────────

/// `import_statement` → one `Import` per imported binding (kind `"import"`).
/// `module` is the source string. Handles named (`{a, b as c}`), default
/// (`import x from`), and namespace (`* as ns`) clauses; a side-effect-only
/// import (`import 'x'`) records a single module-only `Import`.
fn extract_import(node: Node<'_>, src: &[u8], path: &str, out: &mut Vec<Import>) {
    let line = node.start_position().row as u32 + 1;
    let module = match node.child_by_field_name("source").and_then(|n| string_text(n, src)) {
        Some(m) => m,
        None => return,
    };

    let clause = match find_child(node, "import_clause") {
        Some(c) => c,
        None => {
            // `import 'side-effect';` — record the module with no name.
            push_import(out, path, &module, None, None, line);
            return;
        }
    };

    let mut any = false;
    let mut cursor = clause.walk();
    for child in clause.children(&mut cursor) {
        match child.kind() {
            // `import def from 'x'` — bare identifier is the default binding.
            "identifier" => {
                let name = node_text(child, src);
                push_import(out, path, &module, name, None, line);
                any = true;
            }
            "named_imports" => {
                let mut c = child.walk();
                for spec in child.children(&mut c) {
                    if spec.kind() == "import_specifier" {
                        let name = spec.child_by_field_name("name").and_then(|n| node_text(n, src));
                        let alias = spec.child_by_field_name("alias").and_then(|n| node_text(n, src));
                        push_import(out, path, &module, name, alias, line);
                        any = true;
                    }
                }
            }
            "namespace_import" => {
                // `* as ns` — the alias is the trailing identifier; namespace
                // import has no specific imported name.
                let alias = find_child(child, "identifier").and_then(|n| node_text(n, src));
                push_import(out, path, &module, None, alias, line);
                any = true;
            }
            _ => {}
        }
    }

    if !any {
        push_import(out, path, &module, None, None, line);
    }
}

#[allow(clippy::too_many_arguments)]
fn push_import(out: &mut Vec<Import>, path: &str, module: &str, name: Option<String>, alias: Option<String>, line: u32) {
    out.push(Import {
        path: path.to_string(),
        module: module.to_string(),
        name,
        alias,
        line,
        kind: "import".to_string(),
    });
}

// ──────────────────────────── calls ────────────────────────────

/// `call_expression` → `Call`. The `function` child decides the shape:
/// - `identifier` → free call (callee = name, no receiver).
/// - `member_expression` → method call (callee = the `property`, receiver = the
///   `object` text, e.g. `this` / `obj` / `a.b`).
///
/// `caller` is the enclosing function's qualified name (top-level calls dropped).
fn extract_call(node: Node<'_>, src: &[u8], path: &str, caller: Option<&str>) -> Option<Call> {
    let caller = caller?; // top-level calls (outside any fn) are dropped
    let func = node.child_by_field_name("function")?;
    let line = node.start_position().row as u32 + 1;

    let (callee, receiver) = match func.kind() {
        "identifier" => (node_text(func, src)?, None),
        "member_expression" => {
            let receiver = func.child_by_field_name("object").and_then(|n| node_text(n, src));
            let callee = func.child_by_field_name("property").and_then(|n| node_text(n, src))?;
            (callee, receiver)
        }
        // call chains (`f()()`), subscripts, `new`, etc.: fall back to raw text.
        _ => (node_text(func, src)?, None),
    };

    Some(Call {
        path: path.to_string(),
        caller: caller.to_string(),
        callee,
        receiver,
        // NOT marked as a method call: in this language `a.b()` is ambiguous —
        // `obj.method()` and `module.func()` share one syntax node, so claiming
        // "method" here would delete real calls to module-level free functions.
        // Only Rust (`.` vs `::`) and PHP (`->` vs `::`) separate them in the grammar.
        is_method: false,
        line,
        ..Default::default()
    })
}

// ──────────────────────────── routes (Express, best-effort) ──────────────────
//
// Recognises `app.<verb>(...)` / `router.<verb>(...)` where the object is an
// identifier literally named `app` or `router` and the verb is a known HTTP
// method. The first string argument is the route path; a trailing `identifier`
// handler argument is recorded as `handler_method` (a named handler reference),
// otherwise `None`. This is intentionally conservative — non-Express projects
// produce no routes, and chained / mounted routers are not resolved.

const HTTP_VERBS: &[&str] = &["get", "post", "put", "patch", "delete", "all"];

/// Try to recognise an Express route definition and push a `Route` for it.
fn try_extract_route(node: Node<'_>, src: &[u8], path: &str, out: &mut Vec<Route>) {
    let func = match node.child_by_field_name("function") {
        Some(f) if f.kind() == "member_expression" => f,
        _ => return,
    };
    let object = func.child_by_field_name("object").and_then(|n| node_text(n, src)).unwrap_or_default();
    if object != "app" && object != "router" {
        return;
    }
    let verb = func
        .child_by_field_name("property")
        .and_then(|n| node_text(n, src))
        .unwrap_or_default()
        .to_ascii_lowercase();
    if !HTTP_VERBS.contains(&verb.as_str()) {
        return;
    }

    let args = match node.child_by_field_name("arguments") {
        Some(a) => a,
        None => return,
    };
    let positional: Vec<Node<'_>> = {
        let mut v = Vec::new();
        let mut c = args.walk();
        for ch in args.named_children(&mut c) {
            if ch.kind() != "comment" {
                v.push(ch);
            }
        }
        v
    };

    // First positional must be a string literal route path.
    let route = match positional.first().and_then(|n| string_text(*n, src)) {
        Some(r) => r,
        None => return,
    };
    // A named handler reference (identifier) → its name; inline/array/other → None.
    let handler_method = positional
        .iter()
        .skip(1)
        .find_map(|n| if n.kind() == "identifier" { node_text(*n, src) } else { None });

    out.push(Route {
        path: path.to_string(),
        method: verb.to_ascii_uppercase(),
        route,
        handler_class: None,
        handler_method,
        name: None,
        line: node.start_position().row as u32 + 1,
    });
}

// ──────────────────────────── helpers ────────────────────────────

/// Text of a `string` literal with surrounding quotes stripped (the
/// `string_fragment` child holds the unquoted content). Non-string nodes → None.
fn string_text(node: Node<'_>, src: &[u8]) -> Option<String> {
    if node.kind() != "string" {
        return None;
    }
    if let Some(frag) = find_child(node, "string_fragment") {
        return node_text(frag, src);
    }
    // Empty string `''` has no fragment child — strip the quotes from raw text.
    node_text(node, src).map(|s| {
        let b = s.as_bytes();
        if b.len() >= 2 && (b[0] == b'\'' || b[0] == b'"' || b[0] == b'`') {
            s[1..s.len() - 1].to_string()
        } else {
            s
        }
    })
}

fn find_child<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    let count = node.child_count();
    for i in 0..count {
        if let Some(ch) = node.child(i) {
            if ch.kind() == kind {
                return Some(ch);
            }
        }
    }
    None
}

fn node_text(node: Node<'_>, src: &[u8]) -> Option<String> {
    node.utf8_text(src).ok().map(|s| s.to_string())
}
