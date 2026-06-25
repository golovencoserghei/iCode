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
//! Routes stay empty (no framework route DSL extracted here). Docstrings are the
//! first string literal in a function/class body block, when present.

use icode_core::model::{Call, ClassDef, FunctionDef, Import, Language};
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
        routes: Vec::new(),
    }
}

/// Accumulated nodes for one file.
#[derive(Default)]
struct Acc {
    functions: Vec<FunctionDef>,
    classes: Vec<ClassDef>,
    imports: Vec<Import>,
    calls: Vec<Call>,
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
        line,
    })
}

// ──────────────────────────── helpers ────────────────────────────

fn node_text(node: Node<'_>, src: &[u8]) -> Option<String> {
    node.utf8_text(src).ok().map(|s| s.to_string())
}
