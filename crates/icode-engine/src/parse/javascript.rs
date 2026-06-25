//! JavaScript parser (tree-sitter). A thin wrapper over the shared
//! [`super::jsts`] core — it only selects the JavaScript grammar and stamps
//! `Language::JavaScript`; all extraction logic lives in `jsts` (also reused by
//! the TypeScript parser). See that module for the extracted node set and the
//! best-effort boundaries (arrow functions, default/namespace imports, Express
//! routes).

use icode_core::model::Language;

use super::{jsts, ParseResult};

/// Parse JavaScript `source` (logical `path` stamps symbols) into a `ParseResult`.
/// On a tree-sitter init/parse failure returns an empty result; never panics.
pub fn parse_javascript(source: &str, path: &str) -> ParseResult {
    jsts::parse(source, path, tree_sitter_javascript::LANGUAGE.into(), Language::JavaScript)
}
