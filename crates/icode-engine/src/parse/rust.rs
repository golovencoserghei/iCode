//! Rust parser (tree-sitter). Extracts top-level functions and `impl` methods
//! with name, qualified_name, span (1-based), args text, async flag, and body.

use icode_core::model::{FunctionDef, Language};
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
        return ParseResult { functions: vec![], lines_total, ast_hash };
    }
    let tree = match parser.parse(source, None) {
        Some(t) => t,
        None => return ParseResult { functions: vec![], lines_total, ast_hash },
    };

    let mut functions = Vec::new();
    let bytes = source.as_bytes();
    walk(tree.root_node(), bytes, path, None, &mut functions);

    ParseResult { functions, lines_total, ast_hash }
}

fn hash_source(source: &str) -> String {
    let mut h = Sha256::new();
    h.update(source.as_bytes());
    let digest = h.finalize();
    let mut out = String::with_capacity(64);
    for b in digest {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Recursively collect `function_item` nodes. `impl_type` carries the enclosing
/// `impl <Type>` name so methods get a `Type::method` qualified name.
fn walk(node: Node<'_>, src: &[u8], path: &str, impl_type: Option<&str>, out: &mut Vec<FunctionDef>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "function_item" => {
                if let Some(f) = extract_function(child, src, path, impl_type) {
                    out.push(f);
                }
            }
            "impl_item" => {
                let ty = impl_type_name(child, src);
                // Descend into the impl body to pick up its methods.
                if let Some(body) = child.child_by_field_name("body") {
                    walk(body, src, path, ty.as_deref(), out);
                }
            }
            // Modules / blocks nest free functions; recurse with the same context.
            "mod_item" | "source_file" | "declaration_list" | "block" => {
                walk(child, src, path, impl_type, out);
            }
            _ => {
                walk(child, src, path, impl_type, out);
            }
        }
    }
}

fn impl_type_name(impl_node: Node<'_>, src: &[u8]) -> Option<String> {
    // `impl <Type>` or `impl Trait for <Type>` — the `type` field is the Self type.
    impl_node
        .child_by_field_name("type")
        .and_then(|n| node_text(n, src))
}

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

    let return_type = node
        .child_by_field_name("return_type")
        .and_then(|n| node_text(n, src));

    // `async fn` → the `function_modifiers` child contains the `async` keyword.
    let is_async = has_async_modifier(node, src);

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

fn node_text(node: Node<'_>, src: &[u8]) -> Option<String> {
    node.utf8_text(src).ok().map(|s| s.to_string())
}
