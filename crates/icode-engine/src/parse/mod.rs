//! Parsing: source text → extracted symbols.
//!
//! [`ParseResult`] is the **internal contract every language parser fills**. It
//! is deliberately the full code-graph shape from day one (M1): even parsers that
//! cannot populate a field yet return it empty rather than reshaping the struct,
//! so that adding a parser (Python, JS/TS, PHP …) never forces a change here or
//! in the indexer. The fields mirror the frozen `icode-core` node types so the
//! indexer can forward them straight into `CodeWriteStore::upsert_file`.

pub mod php;
pub mod python;
pub mod rust;

use icode_core::model::{Call, ClassDef, FunctionDef, Import, Route};

/// Output of parsing one source file: the extracted code-graph nodes plus the
/// file-level facts the indexer needs to build a `FileRecord`.
///
/// Field order matches `CodeWriteStore::upsert_file`'s node arguments so a parser
/// result maps onto the store call without reordering.
#[derive(Clone, Debug, Default)]
pub struct ParseResult {
    /// Total source lines (≥ 1). Stamped onto the `FileRecord`.
    pub lines_total: u32,
    /// Hash of the AST-relevant input — the file-drift signal. M1 hashes the raw
    /// source (sufficient for re-index gating); structural hashing can replace
    /// the implementation later without changing this contract.
    pub ast_hash: String,
    /// Top-level functions and methods (impl methods carry `Type::method`).
    pub functions: Vec<FunctionDef>,
    /// Type-like declarations: Rust struct/enum/trait/union, Python/PHP/JS
    /// classes, etc. `bases` holds supertypes when cheaply available, else empty.
    pub classes: Vec<ClassDef>,
    /// Module/use/import statements.
    pub imports: Vec<Import>,
    /// Call-graph edges (`caller` → `callee`), with a `receiver` for method calls.
    pub calls: Vec<Call>,
    /// HTTP routes. Empty for languages without a web layer (e.g. Rust); web
    /// frameworks (Laravel, Flask/FastAPI, Express …) populate it later.
    pub routes: Vec<Route>,
}

pub use php::parse_php;
pub use python::parse_python;
pub use rust::parse_rust;
