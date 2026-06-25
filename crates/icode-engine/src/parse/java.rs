//! Java parser (tree-sitter). Extracts the full M1 node set:
//!
//! - functions: `method_declaration` and `constructor_declaration` inside a
//!   class/interface/enum/record → a `Class.method` qualified name (the enclosing
//!   type is threaded through `Ctx`). Java has no `async`, so `is_async` is always
//!   `false`. `args` is the formal-parameter-list text.
//! - classes:   `class_declaration` (bases = `extends` superclass +
//!   `implements` interfaces), `interface_declaration` (bases = extended
//!   interfaces), `enum_declaration`, and `record_declaration` → `ClassDef`.
//! - imports:   `import_declaration` → one `Import` (kind `"import"`). `module` is
//!   the full dotted name, `name` is its last segment. `import static` and
//!   wildcard `import a.b.*` are recorded as-written.
//! - calls:     `method_invocation` → callee = the invoked `name`, receiver = the
//!   `object` text when present (`this` / a variable / a type), attributed to the
//!   enclosing method (top-level — there is none in Java — dropped).
//!
//! Routes stay empty (Spring `@RequestMapping`/`@GetMapping` annotations are out
//! of scope here; they could be added later as a best-effort pass).
//!
//! HONEST BOUNDARIES: `args` keeps generics verbatim (`List<T>`); nested/inner
//! types are flattened (a nested class's methods are `Inner.method`, not
//! `Outer.Inner.method`) — the enclosing type is the *nearest* type. The receiver
//! is the object expression as written and is not resolved to a declared type.

use icode_core::model::{Call, ClassDef, FunctionDef, Import, Language};
use sha2::{Digest, Sha256};
use tree_sitter::{Node, Parser};

use super::ParseResult;

/// Parse Java `source` (logical `path` used to stamp symbols) into a `ParseResult`.
/// On a tree-sitter init/parse failure, returns an empty result (the indexer
/// records a parse error separately); never panics on malformed input.
pub fn parse_java(source: &str, path: &str) -> ParseResult {
    let lines_total = source.lines().count().max(1) as u32;
    let ast_hash = hash_source(source);

    let mut parser = Parser::new();
    if parser.set_language(&tree_sitter_java::LANGUAGE.into()).is_err() {
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
/// boundaries — type/fn entry) to keep the recursion lifetime-free.
#[derive(Clone, Default)]
struct Ctx {
    /// Enclosing type name (class/interface/enum/record) so methods get a
    /// `Type.method` qualified name.
    type_name: Option<String>,
    /// Qualified name of the enclosing method/constructor (the call `caller`).
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
/// attributed correctly.
fn walk(node: Node<'_>, src: &[u8], path: &str, ctx: &Ctx, acc: &mut Acc) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "method_declaration" | "constructor_declaration" => {
                if let Some(f) = extract_function(child, src, path, ctx.type_name.as_deref()) {
                    let qname = f.qualified_name.clone();
                    acc.functions.push(f);
                    let inner = Ctx { type_name: ctx.type_name.clone(), caller: Some(qname) };
                    if let Some(body) = child.child_by_field_name("body") {
                        walk(body, src, path, &inner, acc);
                    }
                }
            }
            "class_declaration" | "interface_declaration" | "enum_declaration"
            | "record_declaration" => {
                if let Some(c) = extract_class(child, src, path) {
                    let type_name = c.name.clone();
                    acc.classes.push(c);
                    // Descend the type body with the type pushed into scope, so
                    // methods become `Type.method`. The caller resets — a method
                    // entered inside sets it.
                    let inner = Ctx { type_name: Some(type_name), caller: ctx.caller.clone() };
                    if let Some(body) = child.child_by_field_name("body") {
                        walk(body, src, path, &inner, acc);
                    }
                }
            }
            "import_declaration" => {
                if let Some(i) = extract_import(child, src, path) {
                    acc.imports.push(i);
                }
            }
            "method_invocation" => {
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

/// `method_declaration` / `constructor_declaration` → `FunctionDef`. `type_name`
/// (if any) prefixes the qualified name as `Type.method`. `args` is the formal
/// parameter-list text; `return_type` is the declared type (constructors have
/// none).
fn extract_function(node: Node<'_>, src: &[u8], path: &str, type_name: Option<&str>) -> Option<FunctionDef> {
    let name = node.child_by_field_name("name").and_then(|n| node_text(n, src))?;

    let qualified_name = match type_name {
        Some(ty) => format!("{ty}.{name}"),
        None => name.clone(),
    };

    let args = node
        .child_by_field_name("parameters")
        .and_then(|n| node_text(n, src))
        .unwrap_or_else(|| "()".to_string());

    let return_type = node.child_by_field_name("type").and_then(|n| node_text(n, src));
    let body = node_text(node, src).unwrap_or_default();

    // tree-sitter rows are 0-based; the contract wants 1-based inclusive lines.
    let line_start = node.start_position().row as u32 + 1;
    let line_end = node.end_position().row as u32 + 1;

    Some(FunctionDef {
        name,
        qualified_name,
        path: path.to_string(),
        language: Language::Java,
        line_start,
        line_end,
        args,
        return_type,
        docstring: None,
        body,
        is_async: false, // Java has no `async` functions.
        override_type: None,
        override_target: None,
    })
}

// ──────────────────────────── classes ────────────────────────────

/// class/interface/enum/record declaration → `ClassDef`. `bases` merges the
/// `superclass` (`extends`) and the `interfaces` (`implements`) for a class; the
/// `extends_interfaces` for an interface; enum/record have no inheritance bases.
fn extract_class(node: Node<'_>, src: &[u8], path: &str) -> Option<ClassDef> {
    let name = node.child_by_field_name("name").and_then(|n| node_text(n, src))?;

    let mut bases = Vec::new();
    match node.kind() {
        "class_declaration" => {
            // `extends Base` — the `superclass` field wraps the base type.
            if let Some(sc) = node.child_by_field_name("superclass") {
                collect_type_names(sc, src, &mut bases);
            }
            // `implements A, B` — the `interfaces` field wraps a `type_list`.
            if let Some(ifs) = node.child_by_field_name("interfaces") {
                collect_type_names(ifs, src, &mut bases);
            }
        }
        "interface_declaration" => {
            // `interface X extends A, B` — the extended interfaces.
            if let Some(ext) = find_child(node, "extends_interfaces") {
                collect_type_names(ext, src, &mut bases);
            }
        }
        // enum / record: `implements` is possible — capture it when present.
        _ => {
            if let Some(ifs) = node.child_by_field_name("interfaces") {
                collect_type_names(ifs, src, &mut bases);
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
        language: Language::Java,
        line_start,
        line_end,
        bases,
        docstring: None,
        body,
    })
}

/// Collect type names found anywhere under `node` — used for `superclass` /
/// `superinterfaces` / `extends_interfaces` clauses, which wrap one or more
/// `type_identifier` / `generic_type` / `scoped_type_identifier` nodes (possibly
/// inside a `type_list`). A `generic_type` contributes its head type name only.
fn collect_type_names(node: Node<'_>, src: &[u8], out: &mut Vec<String>) {
    match node.kind() {
        "type_identifier" | "scoped_type_identifier" => {
            if let Some(t) = node_text(node, src) {
                let t = t.trim().to_string();
                if !t.is_empty() {
                    out.push(t);
                }
            }
        }
        "generic_type" => {
            // `Foo<Bar>` — the leading type child is the raw type name.
            if let Some(head) = node.named_child(0) {
                collect_type_names(head, src, out);
            }
        }
        _ => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                collect_type_names(child, src, out);
            }
        }
    }
}

// ──────────────────────────── imports ────────────────────────────

/// `import_declaration` → one `Import` (kind `"import"`). `module` is the full
/// dotted name as written (a trailing `.*` wildcard is kept); `name` is the last
/// segment. `import static a.b.C.m;` keeps the dotted name intact.
fn extract_import(node: Node<'_>, src: &[u8], path: &str) -> Option<Import> {
    let line = node.start_position().row as u32 + 1;

    // The imported reference is a `scoped_identifier` (or bare `identifier`); a
    // wildcard import has an extra `*` token after it.
    let reference = node
        .child_by_field_name("name")
        .or_else(|| find_child(node, "scoped_identifier"))
        .or_else(|| find_child(node, "identifier"))
        .and_then(|n| node_text(n, src))?;

    let is_wildcard = find_child(node, "asterisk").is_some()
        || node_text(node, src).map(|t| t.trim_end().ends_with('*')).unwrap_or(false);

    let module = if is_wildcard { format!("{reference}.*") } else { reference.clone() };
    let name = last_segment(&reference);

    Some(Import {
        path: path.to_string(),
        module,
        name,
        alias: None, // Java imports have no `as` rename.
        line,
        kind: "import".to_string(),
    })
}

fn last_segment(s: &str) -> Option<String> {
    s.rsplit('.').next().map(|x| x.trim().to_string()).filter(|x| !x.is_empty())
}

// ──────────────────────────── calls ────────────────────────────

/// `method_invocation` → `Call`. `callee` is the invoked `name`; `receiver` is
/// the `object` expression text when present (`this`, a variable, a type), else
/// `None` for an unqualified call. `caller` is the enclosing method (dropped if
/// outside any method — which does not occur in well-formed Java).
fn extract_call(node: Node<'_>, src: &[u8], path: &str, caller: Option<&str>) -> Option<Call> {
    let caller = caller?;
    let callee = node.child_by_field_name("name").and_then(|n| node_text(n, src))?;
    if callee.is_empty() {
        return None;
    }
    let receiver = node.child_by_field_name("object").and_then(|n| node_text(n, src)).map(|t| t.trim().to_string());

    Some(Call {
        path: path.to_string(),
        caller: caller.to_string(),
        callee,
        receiver,
        line: node.start_position().row as u32 + 1,
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
