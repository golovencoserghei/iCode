//! Parsing: source text → extracted symbols. M0.5 covers Rust functions only
//! (top-level `fn` and `impl` methods). Other languages and symbol kinds (classes,
//! imports, calls, routes) arrive in M3.

pub mod rust;

use icode_core::model::FunctionDef;

/// Output of parsing one source file: the symbols plus file-level facts the
/// indexer needs to build a `FileRecord`.
#[derive(Clone, Debug)]
pub struct ParseResult {
    pub functions: Vec<FunctionDef>,
    pub lines_total: u32,
    /// Hash of the AST-relevant input. M0.5 uses a content hash of the source,
    /// which is a sufficient drift signal until structural hashing lands.
    pub ast_hash: String,
}

pub use rust::parse_rust;
