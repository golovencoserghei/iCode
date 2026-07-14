//! Go parser (tree-sitter). Extracts the full M1 node set:
//!
//! - functions: `function_declaration` (free functions) and `method_declaration`
//!   (methods with a receiver → `RecvType.method` qualified name; the receiver
//!   type is read from the method's `receiver` parameter list). Go has no
//!   `async`, so `is_async` is always `false`. `args` is the parameter-list text.
//! - classes:   `type_declaration` whose spec is a `struct_type` or
//!   `interface_type` → `ClassDef`. `qualified_name` is the type name. For a
//!   struct, `bases` carries embedded (anonymous) field types; for an interface,
//!   `bases` carries embedded interface names. Named fields are NOT bases.
//! - imports:   `import_declaration` → one `Import` per `import_spec` (kind
//!   `"import"`). `module` is the package import path (the string literal), `name`
//!   is the explicit alias or the path's last segment, `alias` is the explicit
//!   `as`-style rename when present.
//! - calls:     `call_expression` → free (`Func()` → callee = identifier) or
//!   selector (`pkg.Func()` / `obj.Method()` → callee = the selected field,
//!   receiver = the operand text), attributed to the enclosing function/method
//!   (top-level calls are dropped).
//!
//! Routes stay empty (no Go web-framework route DSL is in scope here).
//!
//! HONEST BOUNDARIES: the struct/interface `bases` are *embedded* types only, a
//! cheap structural approximation of "is-a"; Go has no inheritance. The method
//! receiver type is taken as written (pointer `*T` is normalised to `T`); generic
//! type parameters on the receiver are not stripped beyond the leading `*`.

use icode_core::model::{Call, ClassDef, FunctionDef, Import, Language};
use sha2::{Digest, Sha256};
use tree_sitter::{Node, Parser};

use super::ParseResult;

/// Parse Go `source` (logical `path` used to stamp symbols) into a `ParseResult`.
/// On a tree-sitter init/parse failure, returns an empty result (the indexer
/// records a parse error separately); never panics on malformed input.
pub fn parse_go(source: &str, path: &str) -> ParseResult {
    let lines_total = source.lines().count().max(1) as u32;
    let ast_hash = hash_source(source);

    let mut parser = Parser::new();
    if parser.set_language(&tree_sitter_go::LANGUAGE.into()).is_err() {
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
/// boundaries — fn entry) to keep the recursion lifetime-free.
#[derive(Clone, Default)]
struct Ctx {
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
/// `Ctx` threads the enclosing caller so calls are attributed correctly.
fn walk(node: Node<'_>, src: &[u8], path: &str, ctx: &Ctx, acc: &mut Acc) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "function_declaration" | "method_declaration" => {
                if let Some(f) = extract_function(child, src, path) {
                    let qname = f.qualified_name.clone();
                    acc.functions.push(f);
                    let inner = Ctx { caller: Some(qname) };
                    if let Some(body) = child.child_by_field_name("body") {
                        walk(body, src, path, &inner, acc);
                    }
                }
            }
            "type_declaration" => {
                extract_types(child, src, path, &mut acc.classes);
            }
            "import_declaration" => {
                extract_imports(child, src, path, &mut acc.imports);
            }
            "call_expression" => {
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

/// `function_declaration` / `method_declaration` → `FunctionDef`. A method's
/// qualified name is `RecvType.method`, where `RecvType` is read from the
/// `receiver` parameter list (a leading `*` is stripped).
fn extract_function(node: Node<'_>, src: &[u8], path: &str) -> Option<FunctionDef> {
    let name = node.child_by_field_name("name").and_then(|n| node_text(n, src))?;
    // Go marks tests by NAME: `func TestXxx(t *testing.T)` / `BenchmarkXxx`.
    let is_test = name.starts_with("Test") || name.starts_with("Benchmark") || name.starts_with("Fuzz");

    let qualified_name = match receiver_type(node, src) {
        Some(ty) => format!("{ty}.{name}"),
        None => name.clone(),
    };

    let args = node
        .child_by_field_name("parameters")
        .and_then(|n| node_text(n, src))
        .unwrap_or_else(|| "()".to_string());

    // Go's result is a separate `result` field (a type or parameter_list).
    let return_type = node.child_by_field_name("result").and_then(|n| node_text(n, src));
    let body = node_text(node, src).unwrap_or_default();

    // tree-sitter rows are 0-based; the contract wants 1-based inclusive lines.
    let line_start = node.start_position().row as u32 + 1;
    let line_end = node.end_position().row as u32 + 1;

    Some(FunctionDef {
        name,
        qualified_name,
        path: path.to_string(),
        language: Language::Go,
        line_start,
        line_end,
        args,
        return_type,
        docstring: None,
        body,
        is_async: false, // Go has no `async` functions.
        is_test,
        override_type: None,
        override_target: None,
    })
}

/// The receiver type of a `method_declaration`, read from its `receiver`
/// parameter list (`func (r *T) m()` → `T`). A leading pointer `*` is stripped so
/// pointer and value receivers share one qualified-name prefix.
fn receiver_type(node: Node<'_>, src: &[u8]) -> Option<String> {
    let recv = node.child_by_field_name("receiver")?;
    // The receiver is a `parameter_list` with a single `parameter_declaration`
    // whose `type` field is the receiver type (possibly a `pointer_type`).
    let mut cursor = recv.walk();
    for child in recv.children(&mut cursor) {
        if child.kind() == "parameter_declaration" {
            if let Some(ty) = child.child_by_field_name("type") {
                return type_base_name(ty, src);
            }
        }
    }
    None
}

/// Reduce a type node to its base name: a `pointer_type` unwraps to the pointee;
/// a `generic_type` uses its head type; otherwise the node's text (trimmed of a
/// leading `*`).
fn type_base_name(node: Node<'_>, src: &[u8]) -> Option<String> {
    match node.kind() {
        "pointer_type" => {
            // `*T` — the named child is the pointee type.
            node.named_child(0).and_then(|n| type_base_name(n, src))
        }
        "generic_type" => node.child_by_field_name("type").and_then(|n| type_base_name(n, src)),
        _ => node_text(node, src).map(|t| t.trim_start_matches('*').trim().to_string()),
    }
}

// ──────────────────────────── types (struct / interface) ───────────────────

/// `type_declaration` → zero or more `ClassDef`s, one per `type_spec` whose
/// underlying type is a `struct_type` or `interface_type`. Other type aliases
/// (`type ID = int`, function types, …) are not class-like and are skipped.
fn extract_types(node: Node<'_>, src: &[u8], path: &str, out: &mut Vec<ClassDef>) {
    let mut cursor = node.walk();
    for spec in node.children(&mut cursor) {
        if spec.kind() != "type_spec" {
            continue;
        }
        let Some(name) = spec.child_by_field_name("name").and_then(|n| node_text(n, src)) else {
            continue;
        };
        let Some(ty) = spec.child_by_field_name("type") else { continue };

        let bases = match ty.kind() {
            "struct_type" => struct_embedded(ty, src),
            "interface_type" => interface_embedded(ty, src),
            _ => continue, // not a class-like type
        };

        let body = node_text(spec, src).unwrap_or_default();
        let line_start = spec.start_position().row as u32 + 1;
        let line_end = spec.end_position().row as u32 + 1;

        out.push(ClassDef {
            name: name.clone(),
            qualified_name: name,
            path: path.to_string(),
            language: Language::Go,
            line_start,
            line_end,
            bases,
            docstring: None,
            body,
        });
    }
}

/// Embedded (anonymous) field types of a `struct_type` — a `field_declaration`
/// with no `name` field is an embedded field whose `type` is the embedded type.
fn struct_embedded(struct_node: Node<'_>, src: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let Some(list) = find_child(struct_node, "field_declaration_list") else { return out };
    let mut cursor = list.walk();
    for field in list.children(&mut cursor) {
        if field.kind() != "field_declaration" {
            continue;
        }
        // Named fields have a `name` field; embedded fields do not.
        if field.child_by_field_name("name").is_some() {
            continue;
        }
        if let Some(ty) = field.child_by_field_name("type") {
            if let Some(name) = type_base_name(ty, src) {
                if !name.is_empty() {
                    out.push(name);
                }
            }
        }
    }
    out
}

/// Embedded interface names of an `interface_type` — a bare `type_identifier` (or
/// `qualified_type`) element (not a `method_elem`) is an embedded interface.
fn interface_embedded(iface_node: Node<'_>, src: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut cursor = iface_node.walk();
    for child in iface_node.children(&mut cursor) {
        match child.kind() {
            // Older/newer grammar names for an embedded interface element.
            "type_identifier" | "qualified_type" => {
                if let Some(t) = node_text(child, src) {
                    let t = t.trim().to_string();
                    if !t.is_empty() {
                        out.push(t);
                    }
                }
            }
            "type_elem" => {
                // `type_elem` wraps the embedded type(s) in newer grammars.
                if let Some(t) = node_text(child, src) {
                    let t = t.trim().to_string();
                    if !t.is_empty() {
                        out.push(t);
                    }
                }
            }
            _ => {}
        }
    }
    out
}

// ──────────────────────────── imports ────────────────────────────

/// `import_declaration` → one `Import` per `import_spec` (kind `"import"`). The
/// spec holds the package path as a string literal and an optional alias
/// (identifier / `.` / `_`). Grouped imports (`import ( ... )`) are flattened.
fn extract_imports(node: Node<'_>, src: &[u8], path: &str, out: &mut Vec<Import>) {
    // Specs may sit directly under the declaration or inside an `import_spec_list`.
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "import_spec" => push_import_spec(child, src, path, out),
            "import_spec_list" => {
                let mut c = child.walk();
                for spec in child.children(&mut c) {
                    if spec.kind() == "import_spec" {
                        push_import_spec(spec, src, path, out);
                    }
                }
            }
            _ => {}
        }
    }
}

/// Emit one `Import` from an `import_spec`. `module` is the package path (quotes
/// stripped); `alias` is the explicit rename (`name` field) when present; `name`
/// is the alias, or the path's last segment otherwise.
fn push_import_spec(spec: Node<'_>, src: &[u8], path: &str, out: &mut Vec<Import>) {
    let line = spec.start_position().row as u32 + 1;
    let module = match spec.child_by_field_name("path").and_then(|n| node_text(n, src)) {
        Some(p) => strip_quotes(&p),
        None => return,
    };
    let alias = spec.child_by_field_name("name").and_then(|n| node_text(n, src));
    let name = alias.clone().or_else(|| last_segment(&module));

    out.push(Import {
        path: path.to_string(),
        module,
        name,
        alias,
        line,
        kind: "import".to_string(),
    });
}

fn strip_quotes(s: &str) -> String {
    let s = s.trim();
    let bytes = s.as_bytes();
    if bytes.len() >= 2 {
        let (first, last) = (bytes[0], bytes[bytes.len() - 1]);
        if (first == b'"' || first == b'`') && first == last {
            return s[1..s.len() - 1].to_string();
        }
    }
    s.to_string()
}

fn last_segment(s: &str) -> Option<String> {
    s.rsplit('/').next().map(|x| x.trim().to_string()).filter(|x| !x.is_empty())
}

// ──────────────────────────── calls ────────────────────────────

/// `call_expression` → `Call`. The `function` child decides the shape:
/// - `identifier`          → free call (callee = name, no receiver).
/// - `selector_expression` → `pkg.Func` / `obj.Method` (callee = the selected
///   `field`, receiver = the `operand` text).
///
/// `caller` is the enclosing fn's qualified name (top-level calls dropped).
fn extract_call(node: Node<'_>, src: &[u8], path: &str, caller: Option<&str>) -> Option<Call> {
    let caller = caller?; // top-level calls (outside any fn) are dropped
    let func = node.child_by_field_name("function")?;
    let line = node.start_position().row as u32 + 1;

    let (callee, receiver) = match func.kind() {
        "identifier" => (node_text(func, src)?, None),
        "selector_expression" => {
            let receiver = func.child_by_field_name("operand").and_then(|n| node_text(n, src));
            let callee = func.child_by_field_name("field").and_then(|n| node_text(n, src))?;
            (callee, receiver)
        }
        // Conversions, method-value chains, etc.: fall back to the raw text.
        _ => (node_text(func, src)?, None),
    };
    if callee.is_empty() {
        return None;
    }

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
