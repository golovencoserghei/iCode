//! PHP parser (tree-sitter). Extracts the full M1 node set plus Laravel routes:
//!
//! - functions: free `function_definition` and `method_declaration` inside a
//!   class/trait/interface/enum (methods get a `Type::method` qualified name;
//!   the enclosing type is threaded through `Ctx`). PHP has no `async`, so
//!   `is_async` is always `false`. `docstring` is the leading `/** ... */`
//!   PHPDoc block when present.
//! - classes:   `class_declaration` / `interface_declaration` / `trait_declaration`
//!   / `enum_declaration` → `ClassDef`. `bases` merges `extends` (`base_clause`)
//!   and `implements` (`class_interface_clause`) type names.
//! - imports:   `namespace_use_declaration` → one `Import` per leaf (kind `"use"`);
//!   grouped `use A\B\{C, D as E}` is flattened to individual imports.
//! - calls:     `function_call_expression` (free, no receiver),
//!   `member_call_expression` (`$obj->m()` → receiver = object text e.g. `$this`),
//!   and `scoped_call_expression` (`Cls::m()` → receiver = `Cls`/`self`/`static`/
//!   `parent`), attributed to the enclosing function/method (top-level dropped).
//! - routes:    Laravel `Route::<verb>(...)` facade calls — **best-effort**. See
//!   the routing section below for exactly which forms are recognised and which
//!   are approximated or skipped.

use icode_core::model::{Call, ClassDef, FunctionDef, Import, Language, Route};
use sha2::{Digest, Sha256};
use tree_sitter::{Node, Parser};

use super::ParseResult;

/// Parse PHP `source` (logical `path` used to stamp symbols) into a `ParseResult`.
/// On a tree-sitter init/parse failure, returns an empty result (the indexer
/// records a parse error separately); never panics on malformed input.
pub fn parse_php(source: &str, path: &str) -> ParseResult {
    let lines_total = source.lines().count().max(1) as u32;
    let ast_hash = hash_source(source);

    let mut parser = Parser::new();
    // tree-sitter-php exposes `LANGUAGE_PHP` (the full PHP grammar, with the
    // `<?php` text wrapper) and `LANGUAGE_PHP_ONLY` (code-only). We want the
    // former so real `.php`/`.phtml` files parse.
    if parser.set_language(&tree_sitter_php::LANGUAGE_PHP.into()).is_err() {
        return ParseResult { lines_total, ast_hash, ..Default::default() };
    }
    let tree = match parser.parse(source, None) {
        Some(t) => t,
        None => return ParseResult { lines_total, ast_hash, ..Default::default() },
    };

    let bytes = source.as_bytes();
    let mut acc = Acc::default();
    walk(tree.root_node(), bytes, path, &Ctx::default(), &mut acc);

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
/// boundaries — type/fn entry) to keep the recursion lifetime-free.
#[derive(Clone, Default)]
struct Ctx {
    /// Enclosing type name (class/interface/trait/enum) so methods get a
    /// `Type::method` qualified name.
    type_name: Option<String>,
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
/// `Ctx` threads the enclosing type and caller so methods and calls are
/// attributed correctly. Route extraction piggy-backs on `scoped_call_expression`.
fn walk(node: Node<'_>, src: &[u8], path: &str, ctx: &Ctx, acc: &mut Acc) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "function_definition" | "method_declaration" => {
                if let Some(f) = extract_function(child, src, path, ctx.type_name.as_deref()) {
                    let qname = f.qualified_name.clone();
                    acc.functions.push(f);
                    // Recurse into the body with this function as the caller. The
                    // enclosing type stays in scope for nested calls (`self::`).
                    let inner = Ctx { type_name: ctx.type_name.clone(), caller: Some(qname) };
                    if let Some(body) = child.child_by_field_name("body") {
                        walk(body, src, path, &inner, acc);
                    }
                }
            }
            "class_declaration" | "interface_declaration" | "trait_declaration"
            | "enum_declaration" => {
                if let Some(c) = extract_class(child, src, path) {
                    let type_name = c.name.clone();
                    acc.classes.push(c);
                    // Descend the type body with the type pushed into scope, so
                    // methods become `Type::method`. The caller resets — a method
                    // entered inside sets it.
                    let inner = Ctx { type_name: Some(type_name), caller: ctx.caller.clone() };
                    if let Some(body) = type_body(child) {
                        walk(body, src, path, &inner, acc);
                    }
                }
            }
            "namespace_use_declaration" => {
                extract_use(child, src, path, &mut acc.imports);
            }
            "function_call_expression" => {
                if let Some(c) = extract_function_call(child, src, path, ctx.caller.as_deref()) {
                    acc.calls.push(c);
                }
                walk(child, src, path, ctx, acc);
            }
            "member_call_expression" | "nullsafe_member_call_expression" => {
                if let Some(c) = extract_member_call(child, src, path, ctx.caller.as_deref()) {
                    acc.calls.push(c);
                }
                walk(child, src, path, ctx, acc);
            }
            "scoped_call_expression" => {
                if let Some(c) = extract_scoped_call(child, src, path, ctx.caller.as_deref()) {
                    acc.calls.push(c);
                }
                // Laravel routing: `Route::get('/x', ...)` etc.
                try_extract_routes(child, src, path, &mut acc.routes);
                walk(child, src, path, ctx, acc);
            }
            _ => walk(child, src, path, ctx, acc),
        }
    }
}

/// The body node of a type declaration (`declaration_list` for class/interface/
/// trait; `enum_declaration_list` for enums).
fn type_body(node: Node<'_>) -> Option<Node<'_>> {
    node.child_by_field_name("body")
        .or_else(|| find_child(node, "declaration_list"))
        .or_else(|| find_child(node, "enum_declaration_list"))
}

// ──────────────────────────── functions ────────────────────────────

/// `function_definition` / `method_declaration` → `FunctionDef`. `type_name`
/// (if any) prefixes the qualified name as `Type::method`. `args` is the
/// parameter-list text; `docstring` is a leading `/** ... */` PHPDoc block.
fn extract_function(node: Node<'_>, src: &[u8], path: &str, type_name: Option<&str>) -> Option<FunctionDef> {
    let name = node.child_by_field_name("name").and_then(|n| node_text(n, src))?;
    // PHPUnit convention: `public function testXxx()`.
    let is_test = name.starts_with("test");

    let qualified_name = match type_name {
        Some(ty) => format!("{ty}::{name}"),
        None => name.clone(),
    };

    let args = node
        .child_by_field_name("parameters")
        .and_then(|n| node_text(n, src))
        .unwrap_or_else(|| "()".to_string());

    let return_type = node.child_by_field_name("return_type").and_then(|n| node_text(n, src));
    let docstring = phpdoc(node, src);
    let body = node_text(node, src).unwrap_or_default();

    // tree-sitter rows are 0-based; the contract wants 1-based inclusive lines.
    let line_start = node.start_position().row as u32 + 1;
    let line_end = node.end_position().row as u32 + 1;

    Some(FunctionDef {
        name,
        qualified_name,
        path: path.to_string(),
        language: Language::Php,
        line_start,
        line_end,
        args,
        return_type,
        docstring,
        body,
        is_async: false, // PHP has no `async` functions.
        is_test,
        override_type: None,
        override_target: None,
    })
}

/// A leading `/** ... */` PHPDoc comment immediately preceding `node`, if any.
/// Skips intervening whitespace; a plain `//`/`/* */` comment stops the search.
fn phpdoc(node: Node<'_>, src: &[u8]) -> Option<String> {
    let mut prev = node.prev_sibling();
    while let Some(p) = prev {
        match p.kind() {
            "comment" => {
                let text = node_text(p, src)?;
                if text.starts_with("/**") {
                    return Some(text);
                }
                return None;
            }
            _ if !p.is_named() || p.is_extra() => prev = p.prev_sibling(),
            _ => return None,
        }
    }
    None
}

// ──────────────────────────── classes ────────────────────────────

/// class/interface/trait/enum declaration → `ClassDef`. `bases` merges `extends`
/// (`base_clause`) and `implements` (`class_interface_clause`) type names; both
/// are child nodes (not fields) in tree-sitter-php.
fn extract_class(node: Node<'_>, src: &[u8], path: &str) -> Option<ClassDef> {
    let name = node.child_by_field_name("name").and_then(|n| node_text(n, src))?;

    let mut bases = Vec::new();
    if let Some(c) = find_child(node, "base_clause") {
        bases.extend(clause_type_names(c, src));
    }
    if let Some(c) = find_child(node, "class_interface_clause") {
        bases.extend(clause_type_names(c, src));
    }

    let docstring = phpdoc(node, src);
    let body = node_text(node, src).unwrap_or_default();
    let line_start = node.start_position().row as u32 + 1;
    let line_end = node.end_position().row as u32 + 1;

    Some(ClassDef {
        name: name.clone(),
        qualified_name: name,
        path: path.to_string(),
        language: Language::Php,
        line_start,
        line_end,
        bases,
        docstring,
        body,
    })
}

/// Type names listed in a `base_clause`/`class_interface_clause`: the `name` and
/// `qualified_name` children are the extended/implemented types.
fn clause_type_names(clause: Node<'_>, src: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut cursor = clause.walk();
    for child in clause.children(&mut cursor) {
        if matches!(child.kind(), "name" | "qualified_name") {
            if let Some(t) = node_text(child, src) {
                let t = t.trim();
                if !t.is_empty() {
                    out.push(t.to_string());
                }
            }
        }
    }
    out
}

// ──────────────────────────── imports ────────────────────────────

/// `namespace_use_declaration` → one `Import` per leaf (kind `"use"`). Handles
/// both the simple form (`use App\Models\User as U;`) and the grouped form
/// (`use App\{Models\User, Services\Auth};`), flattening groups into individual
/// imports. `module` is the full namespace path; `name` is the last segment (or
/// the alias's target); `alias` is the `as` rename.
fn extract_use(node: Node<'_>, src: &[u8], path: &str, out: &mut Vec<Import>) {
    let line = node.start_position().row as u32 + 1;

    // For a grouped use the leading `namespace_name` is the shared prefix; it sits
    // directly under the declaration (a sibling of the `namespace_use_group`).
    let group_prefix = find_child(node, "namespace_name").and_then(|n| node_text(n, src));

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "namespace_use_clause" => {
                // Simple form — the clause carries the full path itself.
                push_use(child, src, path, None, line, out);
            }
            "namespace_use_group" => {
                // Grouped form — each inner clause is one leaf, prefixed by `group_prefix`.
                let mut gc = child.walk();
                for clause in child.children(&mut gc) {
                    if clause.kind() == "namespace_use_clause" {
                        push_use(clause, src, path, group_prefix.as_deref(), line, out);
                    }
                }
            }
            _ => {}
        }
    }
}

/// Emit one `Import` from a `namespace_use_clause`. `prefix` (a grouped `use`'s
/// shared namespace) is prepended to the clause path when present.
fn push_use(clause: Node<'_>, src: &[u8], path: &str, prefix: Option<&str>, line: u32, out: &mut Vec<Import>) {
    let qname = clause
        .child_by_field_name("name")
        .or_else(|| find_child(clause, "qualified_name"))
        .or_else(|| find_child(clause, "name"))
        .and_then(|n| node_text(n, src));
    let alias = clause.child_by_field_name("alias").and_then(|n| node_text(n, src));

    let full = match (prefix, qname.as_deref()) {
        (Some(p), Some(q)) => format!("{}\\{}", p.trim_end_matches('\\'), q),
        (None, Some(q)) => q.to_string(),
        (Some(p), None) => p.to_string(),
        (None, None) => return,
    };

    let name = last_segment(&full);
    out.push(Import {
        path: path.to_string(),
        module: full,
        name,
        alias,
        line,
        kind: "use".to_string(),
    });
}

fn last_segment(s: &str) -> Option<String> {
    s.rsplit('\\').next().map(|x| x.trim().to_string()).filter(|x| !x.is_empty())
}

/// Short class name from a possibly-namespaced path: `App\Models\User` → `User`.
fn short_name(s: &str) -> String {
    s.rsplit('\\').next().unwrap_or(s).trim().to_string()
}

// ──────────────────────────── calls ────────────────────────────

/// `function_call_expression` → free `Call` (no receiver). `callee` is the
/// called function's short name (last `\`-segment of a namespaced call), so it
/// matches the stored definition. `caller` is the enclosing fn (top-level dropped).
fn extract_function_call(node: Node<'_>, src: &[u8], path: &str, caller: Option<&str>) -> Option<Call> {
    let caller = caller?;
    let raw = node.child_by_field_name("function").and_then(|n| node_text(n, src))?;
    if raw.is_empty() {
        return None;
    }
    // A free call may be namespace-qualified (`\App\helper()`, `App\helper()`),
    // but `function_definition` records only the short name (`helper`). Normalise
    // the callee to the last `\`-segment so the call edge resolves to the def.
    // Dynamic callees (`$fn`) have no `\` and pass through unchanged.
    let callee = short_name(&raw);
    if callee.is_empty() {
        return None;
    }
    Some(Call {
        path: path.to_string(),
        caller: caller.to_string(),
        callee,
        receiver: None,
        line: node.start_position().row as u32 + 1,
        ..Default::default()
    })
}

/// `member_call_expression` (`$obj->m()`) → `Call`. `callee` is the method name;
/// `receiver` is the object text (`$this`, `$user`, …), normalised.
fn extract_member_call(node: Node<'_>, src: &[u8], path: &str, caller: Option<&str>) -> Option<Call> {
    let caller = caller?;
    let callee = node.child_by_field_name("name").and_then(|n| node_text(n, src))?;
    if callee.is_empty() {
        return None;
    }
    let receiver = node
        .child_by_field_name("object")
        .and_then(|n| node_text(n, src))
        .and_then(|t| normalize_receiver(&t));
    Some(Call {
        path: path.to_string(),
        caller: caller.to_string(),
        callee,
        receiver,
        line: node.start_position().row as u32 + 1,
        ..Default::default()
    })
}

/// `scoped_call_expression` (`Cls::m()`) → `Call`. `callee` is the method name;
/// `receiver` is the scope: `self`/`static`/`parent` verbatim, else the class's
/// short name (last `\`-segment).
fn extract_scoped_call(node: Node<'_>, src: &[u8], path: &str, caller: Option<&str>) -> Option<Call> {
    let caller = caller?;
    let callee = node.child_by_field_name("name").and_then(|n| node_text(n, src))?;
    if callee.is_empty() {
        return None;
    }
    let receiver = node
        .child_by_field_name("scope")
        .and_then(|n| node_text(n, src))
        .and_then(|t| normalize_receiver(&t));
    Some(Call {
        path: path.to_string(),
        caller: caller.to_string(),
        callee,
        receiver,
        line: node.start_position().row as u32 + 1,
        ..Default::default()
    })
}

/// Normalise a call receiver to a stable resolution key:
/// - `$this` / `self` / `static` / `parent` → verbatim;
/// - a bare variable `$user` → kept as `$user`;
/// - a class path `App\Foo` / `Foo` → short name `Foo`;
/// - a complex expression (chain/index/call) → `None` (no cheap resolution).
fn normalize_receiver(text: &str) -> Option<String> {
    let t = text.trim();
    if t.is_empty() {
        return None;
    }
    match t {
        "$this" | "self" | "parent" | "static" => Some(t.to_string()),
        _ if t.contains("->") || t.contains("::") || t.contains('(') || t.contains('[') || t.contains('{') => None,
        _ if t.starts_with('$') => Some(t.to_string()),
        _ => Some(short_name(t)),
    }
}

// ──────────────────────────── Laravel routing (best-effort) ───────────────────
//
// Recognises route definitions of the form `Route::<verb>(path, handler)` and
// maps the HTTP method + URL to a controller@method (or marks a closure handler
// with no class/method). What is extracted reliably vs. approximated:
//
// RELIABLE:
//   - `Route::get|post|put|patch|delete|options('/path', handler)` — one route.
//   - `Route::any('/path', handler)`            → method "ANY".
//   - `Route::match(['get','post'], '/p', h)`   → one route per listed method.
//   - handler `'Ctrl@method'`                   → class `Ctrl`, method `method`.
//   - handler `[Ctrl::class, 'method']`         → class `Ctrl`, method `method`.
//   - handler `'InvokableController'` (no `@`)  → class only, method None.
//   - Closure handler                          → class/method both None.
//   - `->name('foo')` anywhere in the chain     → route name (best-effort).
//
// APPROXIMATED / LIMITED (documented gaps, never panics — falls back to skip):
//   - `Route::resource|apiResource('users', Ctrl::class)` → a single synthetic
//     row with method "RESOURCE" (NOT expanded into index/store/show/…). Deep
//     CRUD expansion is out of scope here.
//   - Group prefixes (`Route::prefix('admin')->group(...)`) are NOT applied —
//     the path is recorded as written at the leaf call.
//   - The class is reduced to its short name (last `\`-segment), matching how
//     `FunctionDef.qualified_name` stores controller methods (`Ctrl::method`).
//   - Only the `Route` facade (last scope segment == "Route") is matched.

const HTTP_VERBS: &[&str] = &["get", "post", "put", "patch", "delete", "options"];

/// Try to recognise a `Route::<verb>(...)` call and push the resulting route(s).
/// Non-route scoped calls and unrecognised verbs are silently ignored.
fn try_extract_routes(node: Node<'_>, src: &[u8], path: &str, out: &mut Vec<Route>) {
    // Scope before `::` must be the `Route` facade (accept a namespaced path).
    let scope = node.child_by_field_name("scope").and_then(|n| node_text(n, src)).unwrap_or_default();
    if short_name(&scope) != "Route" {
        return;
    }
    let verb = node
        .child_by_field_name("name")
        .and_then(|n| node_text(n, src))
        .unwrap_or_default()
        .to_ascii_lowercase();

    let args = match node.child_by_field_name("arguments") {
        Some(a) => collect_arg_exprs(a),
        None => return,
    };
    let line = node.start_position().row as u32 + 1;
    let name = chain_route_name(node, src);

    let str_arg = |i: usize| args.get(i).and_then(|n| string_literal(*n, src));
    let handler_arg = |i: usize| args.get(i).map(|n| extract_handler(*n, src)).unwrap_or((None, None));

    if HTTP_VERBS.contains(&verb.as_str()) || verb == "any" {
        let Some(route) = str_arg(0) else { return };
        let method = if verb == "any" { "ANY".to_string() } else { verb.to_ascii_uppercase() };
        let (hc, hm) = handler_arg(1);
        out.push(make_route(path, &method, &route, hc, hm, name, line));
    } else if verb == "match" {
        // Route::match(['get','post'], '/p', handler) → one route per method.
        let methods = args.first().map(|n| array_string_values(*n, src)).unwrap_or_default();
        let Some(route) = str_arg(1) else { return };
        let (hc, hm) = handler_arg(2);
        for m in methods {
            out.push(make_route(path, &m.to_ascii_uppercase(), &route, hc.clone(), hm.clone(), name.clone(), line));
        }
    } else if verb == "resource" || verb == "apiresource" {
        // Single synthetic row — deep CRUD expansion is out of scope.
        let Some(route) = str_arg(0) else { return };
        let hc = args.get(1).and_then(|n| class_const_name(*n, src));
        out.push(make_route(path, "RESOURCE", &route, hc, None, name, line));
    }
}

#[allow(clippy::too_many_arguments)]
fn make_route(
    path: &str,
    method: &str,
    route: &str,
    handler_class: Option<String>,
    handler_method: Option<String>,
    name: Option<String>,
    line: u32,
) -> Route {
    Route {
        path: path.to_string(),
        method: method.to_string(),
        route: route.to_string(),
        handler_class,
        handler_method,
        name,
        line,
    }
}

/// Collect the positional argument expressions of a call's `arguments` node,
/// unwrapping the `argument` wrapper (the payload is its last named child, which
/// also skips a leading name on named args like `path: '/x'`).
fn collect_arg_exprs<'a>(args: Node<'a>) -> Vec<Node<'a>> {
    let mut out = Vec::new();
    let mut cursor = args.walk();
    for ch in args.named_children(&mut cursor) {
        if ch.kind() == "comment" {
            continue;
        }
        if ch.kind() == "argument" {
            let n = ch.named_child_count();
            if n > 0 {
                if let Some(v) = ch.named_child(n - 1) {
                    out.push(v);
                }
            }
        } else {
            out.push(ch);
        }
    }
    out
}

/// Text of a string literal (`string` / `encapsed_string`) with quotes stripped.
fn string_literal(node: Node<'_>, src: &[u8]) -> Option<String> {
    match node.kind() {
        "string" | "encapsed_string" => node_text(node, src).map(|s| strip_quotes(&s)),
        _ => None,
    }
}

fn strip_quotes(s: &str) -> String {
    let s = s.trim();
    let bytes = s.as_bytes();
    if bytes.len() >= 2 {
        let (first, last) = (bytes[0], bytes[bytes.len() - 1]);
        if (first == b'\'' || first == b'"') && first == last {
            return s[1..s.len() - 1].to_string();
        }
    }
    s.to_string()
}

/// String values of an array literal (`['get','post']`) — for `Route::match`.
fn array_string_values(node: Node<'_>, src: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    if node.kind() == "array_creation_expression" {
        let mut cursor = node.walk();
        for el in node.named_children(&mut cursor) {
            if el.kind() == "array_element_initializer" {
                let n = el.named_child_count();
                if n > 0 {
                    if let Some(v) = el.named_child(n - 1) {
                        if let Some(s) = string_literal(v, src) {
                            out.push(s);
                        }
                    }
                }
            }
        }
    }
    out
}

/// Short class name from `Ctrl::class` / a `name`/`qualified_name` / a string.
fn class_const_name(node: Node<'_>, src: &[u8]) -> Option<String> {
    match node.kind() {
        "class_constant_access_expression" => node.named_child(0).and_then(|n| node_text(n, src)).map(|s| short_name(&s)),
        "name" | "qualified_name" => node_text(node, src).map(|s| short_name(&s)),
        "string" | "encapsed_string" => string_literal(node, src).map(|s| short_name(&s)),
        _ => None,
    }
}

/// Resolve a route handler argument into `(class, method)`. The class is reduced
/// to its short name (matches stored `Ctrl::method`):
///   - `[Ctrl::class, 'method']` → `(Ctrl, method)`
///   - `'Ctrl@method'`           → `(Ctrl, method)`
///   - `'InvokableController'`   → `(InvokableController, None)`
///   - closure / anything else   → `(None, None)`
fn extract_handler(node: Node<'_>, src: &[u8]) -> (Option<String>, Option<String>) {
    match node.kind() {
        "string" | "encapsed_string" => match string_literal(node, src) {
            Some(s) => match s.split_once('@') {
                Some((cls, m)) => (Some(short_name(cls)), Some(m.trim().to_string())),
                None => (Some(short_name(&s)), None),
            },
            None => (None, None),
        },
        "array_creation_expression" => {
            let mut elems = Vec::new();
            let mut cursor = node.walk();
            for ch in node.named_children(&mut cursor) {
                if ch.kind() == "array_element_initializer" {
                    let n = ch.named_child_count();
                    if n > 0 {
                        if let Some(v) = ch.named_child(n - 1) {
                            elems.push(v);
                        }
                    }
                }
            }
            let class = elems.first().and_then(|n| class_const_name(*n, src));
            let method = elems.get(1).and_then(|n| string_literal(*n, src));
            (class, method)
        }
        _ => (None, None),
    }
}

/// Best-effort route name: walk up the call chain looking for `->name('foo')`.
/// `Route::get(...)->name('x')` parses as `member_call_expression{ object:
/// scoped_call, name: name }`, so the name call is an *ancestor* of the verb
/// call. Returns the first `name(...)` string argument found.
fn chain_route_name(verb_call: Node<'_>, src: &[u8]) -> Option<String> {
    let mut cur = verb_call.parent();
    while let Some(n) = cur {
        match n.kind() {
            "member_call_expression" | "nullsafe_member_call_expression" => {
                let m = n.child_by_field_name("name").and_then(|x| node_text(x, src)).unwrap_or_default();
                if m == "name" {
                    if let Some(args) = n.child_by_field_name("arguments") {
                        if let Some(first) = collect_arg_exprs(args).into_iter().next() {
                            if let Some(s) = string_literal(first, src) {
                                return Some(s);
                            }
                        }
                    }
                }
                cur = n.parent();
            }
            // Stop once we leave the fluent chain.
            _ => break,
        }
    }
    None
}

// ──────────────────────────── helpers ────────────────────────────

fn find_child<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    // Index-based (not a `children()` iterator) so the returned node does not
    // borrow a local cursor — mirrors `rust.rs::find_child`.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespaced_free_call_callee_is_short_name() {
        // `\App\helper()` must resolve to the def stored under the short `helper`.
        let src = "<?php\nfunction caller() { \\App\\helper(); }\n";
        let res = parse_php(src, "a.php");
        let call = res
            .calls
            .iter()
            .find(|c| c.caller == "caller")
            .expect("call recorded");
        assert_eq!(call.callee, "helper");
        assert!(call.receiver.is_none());
    }

    #[test]
    fn plain_free_call_callee_unchanged() {
        let src = "<?php\nfunction caller() { helper(); }\n";
        let res = parse_php(src, "b.php");
        let call = res
            .calls
            .iter()
            .find(|c| c.caller == "caller")
            .expect("call recorded");
        assert_eq!(call.callee, "helper");
    }
}
