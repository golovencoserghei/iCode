//! Python parser (tree-sitter). Extracts the full M1 node set:
//!
//! - functions: top-level `def` and class methods (`Class.method`, dotted for
//!   nested classes), `is_async` from an `async def`,
//! - classes:   `class` definitions (bases from the superclass argument list),
//! - imports:   `import a.b as c` (kind `"import"`) and `from x import y as z`
//!   (kind `"from"`), one `Import` per imported leaf,
//! - calls:     free (`f(...)`) and method (`obj.method(...)`) calls, attributed
//!   to the enclosing function/method; method calls carry the object as receiver.
//!
//! - routes:    the WIRING that a call graph cannot see — `@router.post("/x")`,
//!   `@app.route(...)` (Flask) and event/hook decorators (`@client.event`,
//!   `@bot.command`). The framework invokes these handlers, so no call edge exists;
//!   without reading the decorator an agent concludes the endpoint is not there.
//!
//! Docstrings are the first string literal in a function/class body block.

use icode_core::model::{Call, ClassDef, FunctionDef, Import, Language, Route};
use sha2::{Digest, Sha256};
use tree_sitter::{Node, Parser};

use super::ParseResult;

/// Parse Python `source` (logical `path` used to stamp symbols) into a `ParseResult`.
/// On a tree-sitter init/parse failure, returns an empty result (the indexer
/// records a parse error separately); never panics on malformed input.
pub fn parse_python(source: &str, path: &str) -> ParseResult {
    let lines_total = source.lines().count().max(1) as u32;
    let ast_hash = hash_source(source);

    let mut parser = Parser::new();
    if parser.set_language(&tree_sitter_python::LANGUAGE.into()).is_err() {
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
/// boundaries — class/def entry) to keep the recursion lifetime-free.
#[derive(Clone, Default)]
struct Ctx {
    /// Enclosing class name(s), outermost first, so a method gets a
    /// `Outer.Inner.method` qualified name (just `method`'s class when flat).
    class_stack: Vec<String>,
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
/// `Ctx` threads the enclosing class stack and caller so methods and calls are
/// attributed correctly. `decorated_definition` is transparent — it wraps a
/// `function_definition`/`class_definition` we descend into normally.
fn walk(node: Node<'_>, src: &[u8], path: &str, ctx: &Ctx, acc: &mut Acc) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            // A decorated def is where Python does its WIRING. `@router.post("/x")`
            // attaches a handler to an HTTP route; `@client.event` attaches one to an
            // event. Neither produces a call edge — the framework invokes the handler
            // — so without reading the decorator the graph shows these functions as
            // called by nobody, and an agent reading the graph concludes the endpoint
            // does not exist. Measured on a real FastAPI service: 133 endpoints, ALL
            // invisible.
            "decorated_definition" => {
                extract_routes(child, src, path, &mut acc.routes);
                walk(child, src, path, ctx, acc);
            }
            "function_definition" => {
                if let Some(f) = extract_function(child, src, path, &ctx.class_stack) {
                    let qname = f.qualified_name.clone();
                    acc.functions.push(f);
                    // Recurse into the body with this function as the caller; a
                    // function's body opens a fresh class scope for any nested
                    // class/def defined inside it.
                    let inner = Ctx { class_stack: Vec::new(), caller: Some(qname) };
                    if let Some(body) = child.child_by_field_name("body") {
                        walk(body, src, path, &inner, acc);
                    }
                }
            }
            "class_definition" => {
                if let Some(c) = extract_class(child, src, path, &ctx.class_stack) {
                    let qname = c.qualified_name.clone();
                    acc.classes.push(c);
                    // Descend the class body with the class pushed onto the stack
                    // (methods become `Class.method`); the caller does not change
                    // until a method is entered.
                    let mut class_stack = ctx.class_stack.clone();
                    class_stack.push(qname);
                    let inner = Ctx { class_stack, caller: ctx.caller.clone() };
                    if let Some(body) = child.child_by_field_name("body") {
                        walk(body, src, path, &inner, acc);
                    }
                }
            }
            "import_statement" => {
                extract_import(child, src, path, &mut acc.imports);
            }
            "import_from_statement" => {
                extract_import_from(child, src, path, &mut acc.imports);
            }
            // A dict of `"key": obj.method` is a DISPATCH TABLE — the other way Python
            // wires handlers, and the other thing a call graph cannot see. The registry
            // holds a REFERENCE; the invocation happens later as `self._handlers[name](args)`,
            // which names no callee at all. So every handler in the table looks like it
            // is called by nobody.
            //
            // Measured on a real agent service: 34 tool handlers dispatched this way, all
            // of them reported as dead code. An agent reading that graph concludes the
            // feature is not implemented — the exact failure this is here to stop.
            //
            // We emit a reference edge from the function that BUILDS the table, which is
            // what reachability actually needs: the handler is reachable from wherever
            // the registry is constructed.
            "dictionary" => {
                extract_dispatch_table(child, src, path, ctx.caller.as_deref(), &mut acc.calls);
                walk(child, src, path, ctx, acc);
            }
            "call" => {
                if let Some(c) = extract_call(child, src, path, ctx.caller.as_deref()) {
                    acc.calls.push(c);
                }
                walk(child, src, path, ctx, acc);
            }
            _ => walk(child, src, path, ctx, acc),
        }
    }
}

// ──────────────────────────── functions ────────────────────────────

/// `function_definition` → `FunctionDef`. `class_stack` (if any) prefixes the
/// qualified name as `Class.method` (dotted for nested classes). `args` is the
/// parameter-list text; `docstring` is the body's leading string literal.
fn extract_function(node: Node<'_>, src: &[u8], path: &str, class_stack: &[String]) -> Option<FunctionDef> {
    let name = node.child_by_field_name("name").and_then(|n| node_text(n, src))?;
    // pytest/unittest convention: `def test_*`.
    let is_test = name.starts_with("test");

    let qualified_name = if class_stack.is_empty() {
        name.clone()
    } else {
        format!("{}.{name}", class_stack.join("."))
    };

    let args = node
        .child_by_field_name("parameters")
        .and_then(|n| node_text(n, src))
        .unwrap_or_else(|| "()".to_string());

    let return_type = node.child_by_field_name("return_type").and_then(|n| node_text(n, src));
    let is_async = has_async(node, src);
    let docstring = node.child_by_field_name("body").and_then(|b| block_docstring(b, src));
    let body = node_text(node, src).unwrap_or_default();

    // tree-sitter rows are 0-based; the contract wants 1-based inclusive lines.
    let line_start = node.start_position().row as u32 + 1;
    let line_end = node.end_position().row as u32 + 1;

    Some(FunctionDef {
        name,
        qualified_name,
        path: path.to_string(),
        language: Language::Python,
        line_start,
        line_end,
        args,
        return_type,
        docstring,
        body,
        is_async,
        is_test,
        override_type: None,
        override_target: None,
    })
}

/// `async def` surfaces an `async` token as a direct child of the definition.
fn has_async(node: Node<'_>, src: &[u8]) -> bool {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "async" {
            return true;
        }
        // Stop at the name — `async` precedes `def`/name; no need to scan the body.
        if child.kind() == "identifier" {
            break;
        }
    }
    let _ = src;
    false
}

// ──────────────────────────── classes ────────────────────────────

/// `class_definition` → `ClassDef`. `bases` are the superclass names from the
/// `superclasses` argument list (`identifier`/`attribute` entries). `qualified_name`
/// is dotted with any enclosing classes (`Outer.Inner`).
fn extract_class(node: Node<'_>, src: &[u8], path: &str, class_stack: &[String]) -> Option<ClassDef> {
    let name = node.child_by_field_name("name").and_then(|n| node_text(n, src))?;

    let qualified_name = if class_stack.is_empty() {
        name.clone()
    } else {
        format!("{}.{name}", class_stack.join("."))
    };

    let bases = class_bases(node, src);
    let docstring = node.child_by_field_name("body").and_then(|b| block_docstring(b, src));
    let body = node_text(node, src).unwrap_or_default();
    let line_start = node.start_position().row as u32 + 1;
    let line_end = node.end_position().row as u32 + 1;

    Some(ClassDef {
        name,
        qualified_name,
        path: path.to_string(),
        language: Language::Python,
        line_start,
        line_end,
        bases,
        docstring,
        body,
    })
}

/// Superclass names from the `superclasses` argument list. `identifier` →
/// the name; `attribute` (`pkg.Base`) → its full dotted text. Keyword args
/// (`metaclass=...`) and other nodes are skipped.
fn class_bases(class_node: Node<'_>, src: &[u8]) -> Vec<String> {
    let args = match class_node.child_by_field_name("superclasses") {
        Some(a) => a,
        None => return Vec::new(),
    };
    let mut out = Vec::new();
    let mut cursor = args.walk();
    for child in args.children(&mut cursor) {
        match child.kind() {
            "identifier" | "attribute" => {
                if let Some(t) = node_text(child, src) {
                    out.push(t);
                }
            }
            _ => {}
        }
    }
    out
}

/// First string literal of a body `block`, as the docstring (PEP 257). The
/// leading statement must be an `expression_statement` wrapping a `string`;
/// otherwise there is no docstring.
fn block_docstring(block: Node<'_>, src: &[u8]) -> Option<String> {
    let mut cursor = block.walk();
    for child in block.children(&mut cursor) {
        match child.kind() {
            "comment" => continue,
            "expression_statement" => {
                let inner = child.child(0)?;
                if inner.kind() == "string" {
                    return string_content(inner, src);
                }
                return None;
            }
            _ => return None,
        }
    }
    None
}

/// The text *inside* a `string` node (between the quote markers), preferring the
/// `string_content` child; falls back to the raw literal if unstructured.
fn string_content(string_node: Node<'_>, src: &[u8]) -> Option<String> {
    let mut cursor = string_node.walk();
    for child in string_node.children(&mut cursor) {
        if child.kind() == "string_content" {
            return node_text(child, src);
        }
    }
    node_text(string_node, src)
}

// ──────────────────────────── imports ────────────────────────────

/// `import a, b.c as d` → one `Import` per name (kind `"import"`). `module` and
/// `path`-style `name` both carry the dotted module path; `alias` is the rename.
fn extract_import(node: Node<'_>, src: &[u8], path: &str, out: &mut Vec<Import>) {
    let line = node.start_position().row as u32 + 1;
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "dotted_name" => {
                let module = node_text(child, src).unwrap_or_default();
                let name = last_segment(&module);
                push_import(out, path, &module, name, None, "import", line);
            }
            "aliased_import" => {
                let module = child
                    .child_by_field_name("name")
                    .and_then(|n| node_text(n, src))
                    .unwrap_or_default();
                let alias = child.child_by_field_name("alias").and_then(|n| node_text(n, src));
                let name = last_segment(&module);
                push_import(out, path, &module, name, alias, "import", line);
            }
            _ => {}
        }
    }
}

/// `from a.b import c as d, e` → one `Import` per imported leaf (kind `"from"`).
/// `module` is the `from` path; `name` the imported identifier; `alias` the rename.
fn extract_import_from(node: Node<'_>, src: &[u8], path: &str, out: &mut Vec<Import>) {
    let line = node.start_position().row as u32 + 1;
    let module = node
        .child_by_field_name("module_name")
        .and_then(|n| node_text(n, src))
        .unwrap_or_default();

    // Names are the `name`-field children after the `import` keyword (the
    // module_name is also a `name` field on some grammars, so match by node).
    let module_name_id = node.child_by_field_name("module_name").map(|n| n.id());
    let mut wildcard = false;
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if Some(child.id()) == module_name_id {
            continue;
        }
        match child.kind() {
            "dotted_name" => {
                let imported = node_text(child, src).unwrap_or_default();
                let name = last_segment(&imported);
                push_import(out, path, &module, name, None, "from", line);
            }
            "aliased_import" => {
                let imported = child
                    .child_by_field_name("name")
                    .and_then(|n| node_text(n, src))
                    .unwrap_or_default();
                let alias = child.child_by_field_name("alias").and_then(|n| node_text(n, src));
                let name = last_segment(&imported);
                push_import(out, path, &module, name, alias, "from", line);
            }
            "wildcard_import" => wildcard = true,
            _ => {}
        }
    }

    // `from x import *` — record the glob with no specific name.
    if wildcard {
        push_import(out, path, &module, None, None, "from", line);
    }
}

#[allow(clippy::too_many_arguments)]
fn push_import(
    out: &mut Vec<Import>,
    path: &str,
    module: &str,
    name: Option<String>,
    alias: Option<String>,
    kind: &str,
    line: u32,
) {
    out.push(Import {
        path: path.to_string(),
        module: module.to_string(),
        name,
        alias,
        line,
        kind: kind.to_string(),
    });
}

fn last_segment(s: &str) -> Option<String> {
    s.rsplit('.').next().map(|x| x.trim().to_string()).filter(|x| !x.is_empty())
}

// ──────────────────────────── calls ────────────────────────────

/// `call` → `Call`. The `function` child decides the shape:
///
/// - `identifier` → free call (callee = name).
/// - `attribute`  → method call (callee = the `attribute` field, receiver = the
///   `object` field text, e.g. `self` / `obj` / `a.b`).
///
/// `caller` is the qualified name of the enclosing fn (skipped if unknown, i.e.
/// a top-level call outside any function/method).
fn extract_call(node: Node<'_>, src: &[u8], path: &str, caller: Option<&str>) -> Option<Call> {
    let caller = caller?; // top-level calls (outside any fn) are dropped
    let func = node.child_by_field_name("function")?;
    let line = node.start_position().row as u32 + 1;

    let (callee, receiver) = match func.kind() {
        "identifier" => (node_text(func, src)?, None),
        "attribute" => {
            let receiver = func.child_by_field_name("object").and_then(|n| node_text(n, src));
            let callee = func.child_by_field_name("attribute").and_then(|n| node_text(n, src))?;
            (callee, receiver)
        }
        // call chains (`f()()`), subscripts, etc.: fall back to the raw text.
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

// ──────────────────────────── helpers ────────────────────────────

fn node_text(node: Node<'_>, src: &[u8]) -> Option<String> {
    node.utf8_text(src).ok().map(|s| s.to_string())
}

// ──────────────────────────── routes / event wiring ────────────────────────────

/// HTTP verbs a router/app decorator can carry (`@router.get`, `@app.post`, …).
const HTTP_VERBS: &[&str] = &["get", "post", "put", "delete", "patch", "head", "options", "websocket"];

/// Decorators that bind a handler to an EVENT rather than a URL: Discord
/// (`@client.event`, `@bot.command`), pytest-style hooks, generic pub/sub. These are
/// the "hooks/actions" of a codebase — invoked by a framework, never by a call — so
/// they are exactly the handlers that look orphaned in a pure call graph.
const EVENT_DECORATORS: &[&str] = &["event", "command", "on", "listen", "listener", "hook", "subscribe", "task", "step"];

/// Read the wiring off a `decorated_definition`: the decorators attached to a
/// function tell us what invokes it.
///
/// Handles the two shapes that actually occur:
///   * `@router.post("/x")`      → METHOD=POST, route=/x
///   * `@app.route("/x", methods=["POST"])` (Flask) → the `methods=` kwarg, else GET
///   * `@client.event` / `@bot.command(...)` → METHOD=EVENT, route = the handler name
///
/// A decorator we do not recognise (`@property`, `@staticmethod`, `@dataclass`) is
/// skipped: guessing would be worse than saying nothing.
fn extract_routes(node: Node<'_>, src: &[u8], path: &str, out: &mut Vec<Route>) {
    // The function this decorator stack is attached to.
    let mut cursor = node.walk();
    let def = node
        .children(&mut cursor)
        .find(|c| c.kind() == "function_definition");
    let Some(def) = def else { return };
    let Some(handler) = def.child_by_field_name("name").and_then(|n| node_text(n, src)) else {
        return;
    };

    let mut cursor = node.walk();
    for dec in node.children(&mut cursor) {
        if dec.kind() != "decorator" {
            continue;
        }
        let line = dec.start_position().row as u32 + 1;
        // The decorator payload: either a bare `attribute` (`@client.event`) or a
        // `call` whose function is an attribute (`@router.post("/x")`).
        let mut inner = dec.walk();
        let Some(payload) = dec.children(&mut inner).find(|c| c.kind() == "call" || c.kind() == "attribute") else {
            continue;
        };

        let (attr_node, args) = match payload.kind() {
            "call" => (payload.child_by_field_name("function"), payload.child_by_field_name("arguments")),
            _ => (Some(payload), None),
        };
        let Some(attr) = attr_node else { continue };
        if attr.kind() != "attribute" {
            continue;
        }
        let Some(verb) = attr.child_by_field_name("attribute").and_then(|n| node_text(n, src)) else {
            continue;
        };
        let verb_lc = verb.to_lowercase();

        // ── HTTP verb decorator: `@router.get("/x")` ──
        if HTTP_VERBS.contains(&verb_lc.as_str()) {
            let route = args.and_then(|a| first_string_arg(a, src)).unwrap_or_default();
            if route.is_empty() {
                continue;
            }
            out.push(Route {
                path: path.to_string(),
                method: verb_lc.to_uppercase(),
                route,
                handler_class: None,
                handler_method: Some(handler.clone()),
                name: None,
                line,
            });
            continue;
        }

        // ── Flask: `@app.route("/x", methods=["POST"])` ──
        if verb_lc == "route" {
            let route = args.and_then(|a| first_string_arg(a, src)).unwrap_or_default();
            if route.is_empty() {
                continue;
            }
            let methods = args
                .and_then(|a| node_text(a, src))
                .map(|t| {
                    let t = t.to_uppercase();
                    ["POST", "PUT", "DELETE", "PATCH", "HEAD", "OPTIONS"]
                        .iter()
                        .filter(|m| t.contains(**m))
                        .map(|m| m.to_string())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let methods = if methods.is_empty() { vec!["GET".to_string()] } else { methods };
            for m in methods {
                out.push(Route {
                    path: path.to_string(),
                    method: m,
                    route: route.clone(),
                    handler_class: None,
                    handler_method: Some(handler.clone()),
                    name: None,
                    line,
                });
            }
            continue;
        }

        // ── Event/hook decorator: the framework calls this, nothing else does ──
        if EVENT_DECORATORS.contains(&verb_lc.as_str()) {
            // `@bot.command(name="x")` names the event; a bare `@client.event` uses
            // the handler's own name (`on_message`).
            let named = args.and_then(|a| first_string_arg(a, src));
            out.push(Route {
                path: path.to_string(),
                method: "EVENT".to_string(),
                route: named.unwrap_or_else(|| handler.clone()),
                handler_class: None,
                handler_method: Some(handler.clone()),
                name: Some(verb_lc.clone()),
                line,
            });
        }
    }
}

/// The first STRING literal among a call's arguments (`("/x", tags=[...])` → `/x`).
/// Quotes and any `f`/`r`/`b` prefix are stripped.
fn first_string_arg(args: Node<'_>, src: &[u8]) -> Option<String> {
    let mut cursor = args.walk();
    for arg in args.children(&mut cursor) {
        if arg.kind() != "string" {
            continue;
        }
        let raw = node_text(arg, src)?;
        let trimmed = raw
            .trim_start_matches(|c: char| c.is_ascii_alphabetic())
            .trim_matches(|c| c == '"' || c == '\'');
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    None
}

/// A dict literal whose values are METHOD REFERENCES is a dispatch table:
/// `{"list_tasks": h._list_tasks, ...}`. Emit a reference edge to each handler so the
/// graph stops reporting it as uncalled.
///
/// Only `"string": obj.attr` pairs qualify. A value that is a call (`f()`), a literal,
/// or a comprehension is not a handler reference and is ignored — a wrong edge is worse
/// than a missing one, because every consumer trusts it.
///
/// The edge is attributed to the enclosing function (the one that builds the table).
/// `is_method` is true: the value is `obj.attr`, so the free-function soundness rule in
/// `get_callers` applies exactly as it does to a real method call.
fn extract_dispatch_table(
    node: Node<'_>,
    src: &[u8],
    path: &str,
    caller: Option<&str>,
    out: &mut Vec<Call>,
) {
    let Some(caller) = caller else { return };
    let mut cursor = node.walk();
    for pair in node.children(&mut cursor) {
        if pair.kind() != "pair" {
            continue;
        }
        // Key must be a string literal — that is what makes it a NAMED dispatch table
        // rather than an arbitrary mapping.
        let Some(key) = pair.child_by_field_name("key") else { continue };
        if key.kind() != "string" {
            continue;
        }
        let Some(value) = pair.child_by_field_name("value") else { continue };
        if value.kind() != "attribute" {
            continue;
        }
        let Some(callee) = value.child_by_field_name("attribute").and_then(|n| node_text(n, src))
        else {
            continue;
        };
        let receiver = value.child_by_field_name("object").and_then(|n| node_text(n, src));
        out.push(Call {
            path: path.to_string(),
            caller: caller.to_string(),
            callee,
            receiver,
            is_method: true,
            line: value.start_position().row as u32 + 1,
            ..Default::default()
        });
    }
}
