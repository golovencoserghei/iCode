//! Rust parser (tree-sitter). Extracts the full M1 node set:
//!
//! - functions: top-level `fn` and `impl` methods (`Type::method`),
//! - classes:   struct / enum / trait / union items (trait → supertraits as bases),
//! - imports:   `use` declarations,
//! - calls:     free / method / associated calls, attributed to the enclosing fn.
//!
//! Routes stay empty (Rust has no framework route DSL in scope here).

use icode_core::model::{Call, ClassDef, FunctionDef, Import, Language};
use sha2::{Digest, Sha256};
use tree_sitter::{Node, Parser};

use super::ParseResult;

/// Parse Rust `source` (logical `path` used to stamp symbols) into a `ParseResult`.
/// On a tree-sitter init/parse failure, returns an empty result (the indexer
/// records a parse error separately); never panics on malformed input.
pub fn parse_rust(source: &str, path: &str) -> ParseResult {
    let lines_total = source.lines().count().max(1) as u32;
    let ast_hash = hash_source(source);

    let mut parser = Parser::new();
    if parser.set_language(&tree_sitter_rust::LANGUAGE.into()).is_err() {
        return ParseResult { lines_total, ast_hash, ..Default::default() };
    }
    let tree = match parser.parse(source, None) {
        Some(t) => t,
        None => return ParseResult { lines_total, ast_hash, ..Default::default() },
    };

    let bytes = source.as_bytes();
    let mut acc = Acc::default();
    walk(tree.root_node(), bytes, path, &Ctx::default(), &mut acc);
    apply_impl_heritage(&mut acc, path);

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
    /// `impl Trait for Type` heritage links `(type, trait, line_start, line_end)`,
    /// reconciled into `classes` after the walk (see `apply_impl_heritage`).
    impls: Vec<(String, String, u32, u32)>,
}

/// Lexical context carried down the walk. Owned (cloned at the few nesting
/// boundaries — impl/fn entry) to keep the recursion lifetime-free.
#[derive(Clone, Default)]
struct Ctx {
    /// Enclosing `impl <Type>` name (so methods get a `Type::method` qualified name).
    impl_type: Option<String>,
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
/// `Ctx` threads the enclosing impl type and caller so methods and calls are
/// attributed correctly.
fn walk(node: Node<'_>, src: &[u8], path: &str, ctx: &Ctx, acc: &mut Acc) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "function_item" => {
                if let Some(f) = extract_function(child, src, path, ctx.impl_type.as_deref()) {
                    let qname = f.qualified_name.clone();
                    acc.functions.push(f);
                    // Recurse into the body with this function as the caller.
                    let inner = Ctx { impl_type: ctx.impl_type.clone(), caller: Some(qname) };
                    walk(child, src, path, &inner, acc);
                }
            }
            "impl_item" => {
                let ty = impl_type_name(child, src);
                // `impl Trait for Type` — record the trait as a base of the
                // implementing type so struct→trait heritage becomes queryable
                // (`find_implementations`). Inherent `impl Type` has no `trait`
                // field and is skipped. Method qualification below is unchanged.
                if let (Some(self_ty), Some(trait_node)) =
                    (ty.as_deref(), child.child_by_field_name("trait"))
                {
                    if let Some(tr) = node_text(trait_node, src) {
                        let ty_base = base_type_name(self_ty);
                        let tr_base = base_type_name(&tr);
                        if !ty_base.is_empty() && !tr_base.is_empty() {
                            let ls = child.start_position().row as u32 + 1;
                            let le = child.end_position().row as u32 + 1;
                            acc.impls.push((ty_base, tr_base, ls, le));
                        }
                    }
                }
                if let Some(body) = child.child_by_field_name("body") {
                    let inner = Ctx { impl_type: ty, caller: ctx.caller.clone() };
                    walk(body, src, path, &inner, acc);
                }
            }
            "struct_item" | "enum_item" | "trait_item" | "union_item" => {
                if let Some(c) = extract_class(child, src, path) {
                    acc.classes.push(c);
                }
                // A trait body holds method signatures (no bodies) — descend for
                // any default-method calls; struct/enum/union bodies have none.
                walk(child, src, path, ctx, acc);
            }
            "use_declaration" => {
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

fn impl_type_name(impl_node: Node<'_>, src: &[u8]) -> Option<String> {
    // `impl <Type>` or `impl Trait for <Type>` — the `type` field is the Self type.
    impl_node.child_by_field_name("type").and_then(|n| node_text(n, src))
}

/// The bare identifier of a type reference: generic args and path qualifiers are
/// stripped so it matches a stored struct/enum/trait `name`. `Foo<T>` → `Foo`,
/// `crate::Bar` → `Bar`, `fmt::Display` → `Display`.
fn base_type_name(text: &str) -> String {
    let no_generics = text.split('<').next().unwrap_or(text).trim();
    no_generics.rsplit("::").next().unwrap_or(no_generics).trim().to_string()
}

/// Fold `impl Trait for Type` links into `classes`: append the trait to the
/// implementing type's `bases` (creating a lightweight class row when the type
/// itself is not defined in this file), so `find_implementations` resolves it.
fn apply_impl_heritage(acc: &mut Acc, path: &str) {
    let impls = std::mem::take(&mut acc.impls);
    for (ty, tr, line_start, line_end) in impls {
        if let Some(class) = acc.classes.iter_mut().find(|c| c.name == ty) {
            if !class.bases.contains(&tr) {
                class.bases.push(tr);
            }
        } else {
            // Type defined elsewhere (or not parsed as a class here): emit a
            // minimal class row that carries only the heritage edge. A later
            // impl of the same type appends to this row's `bases`.
            acc.classes.push(ClassDef {
                name: ty.clone(),
                qualified_name: ty,
                path: path.to_string(),
                language: Language::Rust,
                line_start,
                line_end,
                bases: vec![tr],
                docstring: None,
                body: String::new(),
            });
        }
    }
}

// ──────────────────────────── functions ────────────────────────────

fn extract_function(node: Node<'_>, src: &[u8], path: &str, impl_type: Option<&str>) -> Option<FunctionDef> {
    let name = node.child_by_field_name("name").and_then(|n| node_text(n, src))?;

    let qualified_name = match impl_type {
        Some(ty) => format!("{ty}::{name}"),
        None => name.clone(),
    };

    let args = node
        .child_by_field_name("parameters")
        .and_then(|n| node_text(n, src))
        .unwrap_or_else(|| "()".to_string());

    let return_type = node.child_by_field_name("return_type").and_then(|n| node_text(n, src));
    let is_async = has_async_modifier(node, src);
    let is_test = has_test_attribute(node, src);
    let body = node_text(node, src).unwrap_or_default();

    // tree-sitter rows are 0-based; the contract wants 1-based inclusive lines.
    let line_start = node.start_position().row as u32 + 1;
    let line_end = node.end_position().row as u32 + 1;

    Some(FunctionDef {
        name,
        qualified_name,
        path: path.to_string(),
        language: Language::Rust,
        line_start,
        line_end,
        args,
        return_type,
        docstring: None,
        body,
        is_async,
        is_test,
        override_type: None,
        override_target: None,
    })
}

fn has_async_modifier(node: Node<'_>, src: &[u8]) -> bool {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "function_modifiers" {
            if let Some(text) = node_text(child, src) {
                if text.contains("async") {
                    return true;
                }
            }
        }
    }
    false
}

// ──────────────────────────── classes ────────────────────────────

/// struct/enum/trait/union → `ClassDef`. `bases` carries trait supertraits
/// (`trait T: A + B`) when present; struct/enum/union have an empty `bases`.
fn extract_class(node: Node<'_>, src: &[u8], path: &str) -> Option<ClassDef> {
    let name = node.child_by_field_name("name").and_then(|n| node_text(n, src))?;
    let bases = match node.kind() {
        "trait_item" => trait_bases(node, src),
        _ => Vec::new(),
    };
    let body = node_text(node, src).unwrap_or_default();
    let line_start = node.start_position().row as u32 + 1;
    let line_end = node.end_position().row as u32 + 1;

    Some(ClassDef {
        name: name.clone(),
        qualified_name: name,
        path: path.to_string(),
        language: Language::Rust,
        line_start,
        line_end,
        bases,
        docstring: None,
        body,
    })
}

/// Supertraits from a `trait_bounds` node (`: A + B + ...`), as type names.
fn trait_bases(trait_node: Node<'_>, src: &[u8]) -> Vec<String> {
    let bounds = match trait_node.child_by_field_name("bounds") {
        Some(b) => b,
        None => return Vec::new(),
    };
    let mut out = Vec::new();
    let mut cursor = bounds.walk();
    for child in bounds.children(&mut cursor) {
        if child.kind() == "type_identifier" || child.kind() == "scoped_type_identifier" {
            if let Some(t) = node_text(child, src) {
                out.push(t);
            }
        }
    }
    out
}

// ──────────────────────────── imports ────────────────────────────

/// `use_declaration` → one `Import` per leaf brought into scope. `module` is the
/// full use path; `name` the imported identifier; `alias` the `as` rename; the
/// `kind` is always `"use"` for Rust.
fn extract_imports(node: Node<'_>, src: &[u8], path: &str, out: &mut Vec<Import>) {
    let line = node.start_position().row as u32 + 1;
    // The argument is the single significant child after the `use` keyword.
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "scoped_identifier" | "identifier" | "scoped_use_list" | "use_as_clause" | "use_wildcard"
            | "use_list" => {
                collect_use(child, src, path, line, "", out);
            }
            _ => {}
        }
    }
}

/// Walk a `use` tree, flattening lists into individual imports. `prefix` is the
/// scope accumulated from parent `scoped_use_list` segments.
fn collect_use(node: Node<'_>, src: &[u8], path: &str, line: u32, prefix: &str, out: &mut Vec<Import>) {
    match node.kind() {
        "use_wildcard" => {
            // `path::*` — record the glob with the leading path as module.
            let text = node_text(node, src).unwrap_or_default();
            let module = join(prefix, text.trim_end_matches("::*"));
            push_import(out, path, &module, None, None, line);
        }
        "use_as_clause" => {
            let inner = node.child_by_field_name("path").and_then(|n| node_text(n, src)).unwrap_or_default();
            let alias = node.child_by_field_name("alias").and_then(|n| node_text(n, src));
            let module = join(prefix, &inner);
            let name = last_segment(&inner);
            push_import(out, path, &module, name, alias, line);
        }
        "scoped_use_list" => {
            // `a::b::{X, Y as Z}` — the path field is the prefix, list the leaves.
            let scope = node.child_by_field_name("path").and_then(|n| node_text(n, src)).unwrap_or_default();
            let new_prefix = join(prefix, &scope);
            if let Some(list) = find_child(node, "use_list") {
                let mut c = list.walk();
                for item in list.children(&mut c) {
                    match item.kind() {
                        "identifier" | "scoped_identifier" | "use_as_clause" | "scoped_use_list" | "use_wildcard" => {
                            collect_use(item, src, path, line, &new_prefix, out);
                        }
                        _ => {}
                    }
                }
            }
        }
        "use_list" => {
            let mut c = node.walk();
            for item in node.children(&mut c) {
                match item.kind() {
                    "identifier" | "scoped_identifier" | "use_as_clause" | "scoped_use_list" | "use_wildcard" => {
                        collect_use(item, src, path, line, prefix, out);
                    }
                    _ => {}
                }
            }
        }
        "scoped_identifier" | "identifier" => {
            let text = node_text(node, src).unwrap_or_default();
            let module = join(prefix, &text);
            let name = last_segment(&text);
            push_import(out, path, &module, name, None, line);
        }
        _ => {}
    }
}

fn push_import(out: &mut Vec<Import>, path: &str, module: &str, name: Option<String>, alias: Option<String>, line: u32) {
    out.push(Import {
        path: path.to_string(),
        module: module.to_string(),
        name,
        alias,
        line,
        kind: "use".to_string(),
    });
}

fn join(prefix: &str, seg: &str) -> String {
    if prefix.is_empty() {
        seg.to_string()
    } else if seg.is_empty() {
        prefix.to_string()
    } else {
        format!("{prefix}::{seg}")
    }
}

fn last_segment(s: &str) -> Option<String> {
    s.rsplit("::").next().map(|x| x.trim().to_string()).filter(|x| !x.is_empty())
}

// ──────────────────────────── calls ────────────────────────────

/// `call_expression` → `Call`. The function child decides the shape:
///
/// - `identifier`        → free fn call (callee = name).
/// - `scoped_identifier` → associated call (callee = last segment, receiver =
///   the leading path, e.g. `Service`).
/// - `field_expression`  → method call (callee = field, receiver = object text).
///
/// `caller` is the qualified name of the enclosing fn (skipped if unknown).
/// Is this `function_item` a TEST?
///
/// Rust marks tests with an ATTRIBUTE, not a naming convention: a unit test can be
/// called `contains_identifier_respects_token_boundaries`. So a name/path heuristic
/// misses essentially every inline `#[cfg(test)] mod tests` function — which in a Rust
/// project is most of the test suite. tree-sitter emits `#[test]` as an
/// `attribute_item` SIBLING preceding the function, so we walk back over the
/// attributes attached to this item.
///
/// Matches `#[test]`, `#[tokio::test]`, `#[async_std::test]`, `#[bench]` and the
/// `#[rstest]`/`#[test_case]` families — anything whose attribute path ends in `test`
/// or `bench`.
fn has_test_attribute(node: Node<'_>, src: &[u8]) -> bool {
    let mut sib = node.prev_sibling();
    while let Some(n) = sib {
        if n.kind() != "attribute_item" {
            break;
        }
        if let Some(text) = node_text(n, src) {
            let t = text.to_lowercase();
            let t = t.trim_start_matches("#[").trim_end_matches(']');
            // The attribute PATH only (drop any arguments): `test_case(1, 2)` -> `test_case`.
            let head = t.split('(').next().unwrap_or(t).trim();
            let last = head.rsplit("::").next().unwrap_or(head).trim();
            if last == "test" || last == "bench" || last.starts_with("test_") || last == "rstest" {
                return true;
            }
        }
        sib = n.prev_sibling();
    }
    false
}

fn extract_call(node: Node<'_>, src: &[u8], path: &str, caller: Option<&str>) -> Option<Call> {
    let caller = caller?; // top-level calls (outside any fn) are dropped
    let func = node.child_by_field_name("function")?;
    let line = node.start_position().row as u32 + 1;

    // `is_method` is the `.` vs `::` distinction, which Rust makes unambiguously:
    // `a.b()` is a method on a value and can NEVER be a free `fn b`, while `a::b()`
    // is a path and can. Flattening both into `receiver` is what let every
    // `.collect()` in the repo link to a local `fn collect`.
    let (callee, receiver, is_method) = match func.kind() {
        "identifier" => (node_text(func, src)?, None, false),
        "scoped_identifier" => {
            let receiver = func.child_by_field_name("path").and_then(|n| node_text(n, src));
            let callee = func.child_by_field_name("name").and_then(|n| node_text(n, src))?;
            (callee, receiver, false)
        }
        "field_expression" => {
            let receiver = func.child_by_field_name("value").and_then(|n| node_text(n, src));
            let callee = func.child_by_field_name("field").and_then(|n| node_text(n, src))?;
            (callee, receiver, true)
        }
        // Turbofish: `foo::<T>()`, `x.collect::<Vec<_>>()`, `Ok::<_, E>(v)`. tree-sitter
        // wraps the real callee in a `generic_function` whose `function` field holds it,
        // so recurse into that field rather than stringifying the whole expression.
        //
        // The old fallback took `node_text(func)` — the RAW SOURCE of the call
        // expression — as the callee name. That put 132 junk rows in the call graph,
        // 8 of them multi-line chunks of code sitting in a `callee` column, e.g.
        // `"(0..hashes.len())\n    .map(|i| format!(\"?{}\", i + 2)"`. Every consumer
        // (callers, impact, centrality, dead-code) had to carry that noise.
        "generic_function" => {
            let inner = func.child_by_field_name("function")?;
            match inner.kind() {
                "field_expression" => {
                    let receiver = inner.child_by_field_name("value").and_then(|n| node_text(n, src));
                    let callee = inner.child_by_field_name("field").and_then(|n| node_text(n, src))?;
                    (callee, receiver, true)
                }
                "scoped_identifier" => {
                    let receiver = inner.child_by_field_name("path").and_then(|n| node_text(n, src));
                    let callee = inner.child_by_field_name("name").and_then(|n| node_text(n, src))?;
                    (callee, receiver, false)
                }
                _ => (node_text(inner, src)?, None, false),
            }
        }
        // Anything else (a call on a parenthesised expression, an index, …) has no
        // nameable callee. Dropping it is correct: a bogus name is worse than a
        // missing edge, because every downstream consumer trusts the name.
        _ => return None,
    };

    Some(Call {
        path: path.to_string(),
        caller: caller.to_string(),
        callee,
        receiver,
        is_method,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn class_named<'a>(res: &'a ParseResult, name: &str) -> Option<&'a ClassDef> {
        res.classes.iter().find(|c| c.name == name)
    }

    #[test]
    fn base_type_name_strips_generics_and_path() {
        assert_eq!(base_type_name("Foo"), "Foo");
        assert_eq!(base_type_name("Foo<T>"), "Foo");
        assert_eq!(base_type_name("crate::Bar"), "Bar");
        assert_eq!(base_type_name("fmt::Display"), "Display");
    }

    #[test]
    fn impl_trait_for_local_type_records_base() {
        let src = "struct Foo;\ntrait Greet { fn hi(&self); }\nimpl Greet for Foo { fn hi(&self) {} }\n";
        let res = parse_rust(src, "a.rs");
        let foo = class_named(&res, "Foo").expect("Foo class present");
        assert!(
            foo.bases.iter().any(|b| b == "Greet"),
            "Foo should list Greet as a base: {:?}",
            foo.bases
        );
        // Method qualification inside the impl block is unchanged (`Foo::hi`).
        assert!(res.functions.iter().any(|f| f.qualified_name == "Foo::hi"));
    }

    #[test]
    fn impl_trait_for_external_type_emits_stub_class() {
        // `Vec` is not defined here; a stub class row carries the heritage edge.
        let src = "trait Greet {}\nimpl Greet for Vec<u8> {}\n";
        let res = parse_rust(src, "b.rs");
        let v = class_named(&res, "Vec").expect("stub Vec class present");
        assert_eq!(v.bases, vec!["Greet".to_string()]);
    }

    #[test]
    fn inherent_impl_adds_no_base() {
        let src = "struct Foo;\nimpl Foo { fn go(&self) {} }\n";
        let res = parse_rust(src, "c.rs");
        let foo = class_named(&res, "Foo").expect("Foo class present");
        assert!(foo.bases.is_empty(), "inherent impl must not add a base");
    }
}
