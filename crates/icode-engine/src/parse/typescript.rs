//! TypeScript parser (tree-sitter). A thin wrapper over the shared
//! [`super::jsts`] core (the same engine the JavaScript parser uses): it selects
//! a TypeScript grammar and stamps `Language::TypeScript`. Two grammars are
//! exposed — `parse_typescript` uses the plain TypeScript grammar (`.ts`), and
//! `parse_tsx` uses the TSX grammar (`.tsx`, JSX-aware). TS-specific shapes
//! (`interface_declaration`, `enum_declaration`, `implements`, typed params) are
//! handled inside `jsts`, which the JavaScript grammar simply never produces.

use icode_core::model::Language;

use super::{jsts, ParseResult};

/// Parse TypeScript `source` (`.ts`) into a `ParseResult`. On a tree-sitter
/// init/parse failure returns an empty result; never panics.
pub fn parse_typescript(source: &str, path: &str) -> ParseResult {
    jsts::parse(source, path, tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(), Language::TypeScript)
}

/// Parse TSX `source` (`.tsx`) into a `ParseResult` using the JSX-aware TSX
/// grammar. Symbols are stamped `Language::TypeScript` (TSX is a TypeScript
/// dialect, not a separate `Language` variant).
pub fn parse_tsx(source: &str, path: &str) -> ParseResult {
    jsts::parse(source, path, tree_sitter_typescript::LANGUAGE_TSX.into(), Language::TypeScript)
}
