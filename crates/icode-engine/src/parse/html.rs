//! HTML parser (tree-sitter) — **metadata-only stub by design**.
//!
//! HTML has no functions, classes, imports, or calls in our code-graph sense, so
//! this parser deliberately returns a `ParseResult` with **empty** `functions` /
//! `classes` / `imports` / `calls` / `routes`. What it *does* provide are correct
//! file-level metadata: `lines_total` and a non-empty `ast_hash` (the drift
//! signal the indexer stamps onto the `FileRecord`).
//!
//! Why a stub and not full extraction: an HTML document's meaningful content is
//! its text and structure, which is indexed for FTS + window chunks in a separate
//! text layer — not as code symbols. Embedded `<script>` / `<style>` blocks are
//! intentionally NOT descended here (a JS/CSS sub-parse is out of scope for this
//! layer). The single job of this parser is to make `.html` / `.htm` files parse
//! cleanly so they are NOT counted as `parse_error`s, while still carrying the
//! metadata the indexer needs.
//!
//! We still run tree-sitter-html so a malformed document is handled by the
//! grammar's error recovery rather than by us, and so a future revision can start
//! extracting structure (links, forms, …) without changing the entry point.

use sha2::{Digest, Sha256};
use tree_sitter::Parser;

use super::ParseResult;

/// Parse HTML `source` into a metadata-only `ParseResult` (no extracted symbols).
/// `path` is accepted for signature symmetry with the other parsers but is unused
/// — there are no symbols to stamp. Never panics on malformed input.
pub fn parse_html(source: &str, _path: &str) -> ParseResult {
    let lines_total = source.lines().count().max(1) as u32;
    let ast_hash = hash_source(source);

    // Run the grammar so malformed input is absorbed by tree-sitter's error
    // recovery (and to keep the door open for future structural extraction). The
    // resulting tree is intentionally not walked — this layer extracts no symbols.
    let mut parser = Parser::new();
    if parser.set_language(&tree_sitter_html::LANGUAGE.into()).is_ok() {
        let _ = parser.parse(source, None);
    }

    ParseResult { lines_total, ast_hash, ..Default::default() }
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
