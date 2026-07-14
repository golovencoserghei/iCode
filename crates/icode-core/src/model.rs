//! Domain model — the stable data vocabulary shared across all crates.
//! Serializable so it can cross the store and MCP boundaries unchanged.

use crate::ids::MemoryId;
use serde::{Deserialize, Serialize};

// ──────────────────────────── languages ────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    Php,
    Python,
    JavaScript,
    TypeScript,
    Go,
    Java,
    Rust,
    Html,
    /// Non-AST text (md/json/yaml/toml/sql/env/…), indexed for FTS + window chunks.
    Text,
}

impl Language {
    pub fn as_str(self) -> &'static str {
        match self {
            Language::Php => "php",
            Language::Python => "python",
            Language::JavaScript => "javascript",
            Language::TypeScript => "typescript",
            Language::Go => "go",
            Language::Java => "java",
            Language::Rust => "rust",
            Language::Html => "html",
            Language::Text => "text",
        }
    }
}

// ──────────────────────────── code graph ────────────────────────────

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FileRecord {
    pub path: String,
    pub language: Language,
    pub content_hash: String,
    pub ast_hash: String,
    pub lines_total: u32,
    pub mtime: i64,
    pub file_size: u64,
    /// Sub-project this file belongs to, relative to the indexed root (`""` = the root
    /// module). In a monorepo this is the boundary that stops a call resolving to a
    /// sibling project's homonym — see `icode_engine::module`.
    #[serde(default)]
    pub module: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SymbolKind {
    Function,
    Class,
    /// A line-window chunk for symbol-less files (config/sql/docs).
    FileWindow,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FunctionDef {
    pub name: String,
    pub qualified_name: String,
    pub path: String,
    pub language: Language,
    pub line_start: u32,
    pub line_end: u32,
    pub args: String,
    pub return_type: Option<String>,
    pub docstring: Option<String>,
    /// Empty in lean projections; populated only when a body was requested.
    pub body: String,
    pub is_async: bool,
    /// TRUE when this function is a TEST.
    ///
    /// Set from the language's own marker where one exists (Rust `#[test]` /
    /// `#[tokio::test]` / `#[bench]`), and from file/name conventions otherwise. The
    /// attribute matters: Rust marks tests with an ATTRIBUTE, not a naming convention,
    /// so an inline `#[cfg(test)] mod tests` function can be called
    /// `contains_identifier_respects_token_boundaries`. A name/path heuristic alone
    /// would therefore miss most of a Rust project's test suite.
    #[serde(default)]
    pub is_test: bool,
    /// Reserved: "own" | "inherited" | "override" resolution metadata. NOT yet
    /// populated — the OOP resolution model is unimplemented, so no parser sets
    /// these; both fields are always None today.
    pub override_type: Option<String>,
    pub override_target: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClassDef {
    pub name: String,
    pub qualified_name: String,
    pub path: String,
    pub language: Language,
    pub line_start: u32,
    pub line_end: u32,
    pub bases: Vec<String>,
    pub docstring: Option<String>,
    pub body: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Import {
    pub path: String,
    pub module: String,
    pub name: Option<String>,
    pub alias: Option<String>,
    pub line: u32,
    pub kind: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Call {
    pub path: String,
    pub caller: String,
    pub callee: String,
    /// Receiver of a method call ($this/self/ClassName/$var) for OOP resolution.
    pub receiver: Option<String>,
    /// TRUE when the call was written with method syntax (`a.b()`, `$a->b()`), FALSE
    /// for a free call (`b()`) or a path/associated call (`A::b()`, `mod::b()`).
    ///
    /// The parsers always knew this — tree-sitter hands them a `field_expression` for
    /// `a.b()` and a `scoped_identifier` for `a::b()` — but the model used to flatten
    /// both into `receiver`, and the distinction was lost. It is load-bearing: a
    /// METHOD call can never target a FREE function, so without it every `.collect()`
    /// in the repo linked to a local `fn collect`, every `.join()` to a local `fn
    /// join`. Measured on this project: 133 false callers on `join`, 82 on `collect`,
    /// 38 on `walk`. `receiver` alone cannot recover it — `icode_engine::index_path()`
    /// (a real path call) and `store.add()` (a method call) both leave a bare
    /// lowercase word behind.
    #[serde(default)]
    pub is_method: bool,
    pub line: u32,
    /// Receiver-aware resolved target: the callee's fully-qualified name
    /// (`EnclosingType::method`) when the receiver let us qualify it against a real
    /// definition; `None` means the edge stays bare-name (dynamic/unresolvable
    /// receiver). Populated at index time; see `store::resolve_call_edges`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_callee: Option<String>,
    /// Deterministic confidence of this call edge, by the WAY it resolved:
    /// 0.9 exact qualified (1 def) · 0.7 qualified (>1 defs) · 0.6 bare unique ·
    /// 0.4 bare collision · 0.3 unresolved/dynamic. 0.0 before the resolve pass.
    #[serde(default)]
    pub confidence: f32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Route {
    pub path: String,
    pub method: String,
    pub route: String,
    pub handler_class: Option<String>,
    pub handler_method: Option<String>,
    pub name: Option<String>,
    pub line: u32,
}

/// One unit of text fed to the embedder. `content_hash` gates incremental
/// re-embedding; `symbol_id` ties window/overflow sub-chunks back to a symbol.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CodeChunk {
    pub symbol_kind: SymbolKind,
    pub symbol_id: Option<i64>,
    pub qualified_name: Option<String>,
    pub path: String,
    pub line_start: u32,
    pub line_end: u32,
    pub chunk_text: String,
    pub content_hash: String,
}

// ──────────────────────────── search results ────────────────────────────

/// Lean code hit (no body unless explicitly requested). `score` is the fused
/// relevance (RRF/weighted); `stale` flags drift vs disk.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CodeHit {
    pub kind: SymbolKind,
    pub name: String,
    pub qualified_name: String,
    pub path: String,
    pub line_start: u32,
    pub line_end: u32,
    pub score: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
    pub stale: bool,
}

/// Everything an agent needs about a symbol in one call.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SymbolContext {
    pub definition: Option<FunctionOrClass>,
    pub callers: Vec<Call>,
    pub callees: Vec<Call>,
    pub imports: Vec<Import>,
    pub routes: Vec<Route>,
    pub implementations: Vec<String>,
    /// Semantically nearest symbols (from the vector index).
    pub similar_symbols: Vec<CodeHit>,
}

// ──────────────────────── existence verdict (check_exists) ────────────────────────

/// A calibrated answer to "does a feature/symbol matching this query actually
/// exist in the index?". Plain search always returns the K nearest neighbours, so
/// an agent can read "closest" as "found" (a string literal `"calendar"` in a
/// permissions list is NOT a calendar feature). This commits to a verdict so the
/// answer can be quoted, not vibed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Verdict {
    /// A real DEFINED symbol (function/class/route) matches the query.
    Exists,
    /// Something related is nearby, but not a clear match — verify before trusting.
    Weak,
    /// Nothing in the index matches. NOT proof it can't exist elsewhere
    /// (dynamic/external/generated code is invisible to a static index).
    Absent,
}

/// WHY a hit matched — the discriminator that stops a string-literal mention from
/// being read as a real feature.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchKind {
    /// A query term equals a defined function/class/route name (case-insensitive).
    ExactSymbol,
    /// A query term is a segment of a defined symbol's name.
    NameToken,
    /// Vector similarity only — no lexical overlap (a paraphrase match).
    Semantic,
    /// The term appears ONLY inside code bodies / string literals / comments, with
    /// no symbol of that name. A mention, not a feature.
    BodyOrString,
}

/// One supporting hit behind a verdict, tagged with why it matched. The `hit`'s
/// own `snippet` carries the quotable text (a signature, or the literal line for
/// `body_or_string`) so the agent cites rather than paraphrases.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Evidence {
    pub hit: CodeHit,
    pub match_kind: MatchKind,
}

/// The full grounded answer for `check_exists`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExistenceVerdict {
    pub verdict: Verdict,
    /// 0..1 calibrated confidence in the verdict. Heuristic (thresholds tuned on
    /// real code), NOT a proof.
    pub confidence: f32,
    /// Human-readable one-liner an agent can quote back verbatim.
    pub reason: String,
    /// The single best-matching symbol, if any (`null` on ABSENT with no neighbour).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub best_match: Option<CodeHit>,
    /// The `match_kind` of `best_match`, if present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub match_kind: Option<MatchKind>,
    /// True iff a query term equals a DEFINED symbol's name (the strongest signal).
    pub exact_name_hit: bool,
    /// Up to a few supporting hits with their match kinds.
    pub evidence: Vec<Evidence>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum FunctionOrClass {
    Function(FunctionDef),
    Class(ClassDef),
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DbStats {
    pub files: u64,
    pub functions: u64,
    pub classes: u64,
    pub calls: u64,
    pub imports: u64,
    pub routes: u64,
    pub code_chunks: u64,
    /// Rows present in the vector index (doctor invariant: == code_chunks).
    pub vec_rows: u64,
    pub parse_errors: u64,
}

/// Search mode for `CodeReadStore::search_code`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SearchMode {
    /// FTS5/BM25 only.
    Lexical,
    /// Vector KNN only.
    Semantic,
    /// Fused dense + lexical (+ optional rerank). Default for find_existing.
    Hybrid,
}

/// Unified code-search request (covers search_function/class, semantic_search_code,
/// find_existing — one method, no per-variant trait methods).
#[derive(Clone, Debug)]
pub struct CodeQuery {
    pub text: String,
    pub kind: Option<SymbolKind>,
    pub lang: Option<Language>,
    pub limit: usize,
    pub mode: SearchMode,
    /// Include body/snippet in hits (lean by default).
    pub with_body: bool,
}

/// Architecture map in one cheap call.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RepoMap {
    pub stats: DbStats,
    pub languages: Vec<(String, u64)>,
    pub modules: Vec<ModuleStat>,
    pub complex_functions: Vec<ComplexFunction>,
    pub call_hotspots: Vec<(String, u64)>,
    pub entry_points: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModuleStat {
    pub module: String,
    pub files: u64,
    pub functions: u64,
    pub classes: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ComplexFunction {
    pub qualified_name: String,
    pub path: String,
    pub line_start: u32,
    pub line_end: u32,
    pub span: u32,
    pub fan_out: u32,
    pub callers: u32,
    pub score: f32,
}

/// A regex/literal match in stored content.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GrepHit {
    pub path: String,
    pub line: u32,
    pub text: String,
}

// ──────────────────────────── references ────────────────────────────

/// What a reference to a symbol actually IS. Grep cannot tell these apart — it hands
/// back a pile of line hits and leaves the agent to guess which one is the definition,
/// which is a call, and which is a word inside a comment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RefKind {
    /// Where the symbol is defined.
    Definition,
    /// A call site, taken from the resolved call graph (so the method-vs-free-function
    /// soundness rule applies — a `.foo()` is never credited to a free `fn foo`).
    Call,
    /// An import / use site.
    Import,
    /// The identifier appears in a symbol's body but is not a call — a type
    /// annotation, a struct literal, a path, a trait bound. Matched on IDENTIFIER
    /// boundaries, so `walk` never matches inside `walk_source_files`.
    Mention,
}

/// A test that reaches a symbol through the call graph.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TestCoverage {
    /// The test function's bare name.
    pub test: String,
    pub qualified_name: String,
    pub path: String,
    /// How many calls separate the test from the symbol. 1 = the test calls it
    /// directly; 3 = it reaches it three hops down, which grepping the test suite for
    /// the symbol's name would never find.
    pub depth: u32,
}

/// One place a symbol is referenced, classified.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Reference {
    pub kind: RefKind,
    pub path: String,
    pub line: u32,
    /// The enclosing symbol (for a call: the caller; for a mention: the symbol whose
    /// body contains it). Empty for a definition.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub context: String,
    /// The source line, trimmed.
    pub text: String,
}

// ──────────────────────────── memory ────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Category {
    Decision,
    Progress,
    Context,
    Bug,
    Todo,
    Code,
    General,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryStatus {
    Active,
    Resolved,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgeStatus {
    Fresh,
    Recent,
    Aging,
    Stale,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MemoryRecord {
    pub id: MemoryId,
    pub project: String,
    pub content: String,
    pub category: Category,
    pub tags: Vec<String>,
    pub importance: f32,
    pub status: MemoryStatus,
    pub resolved_at: Option<String>,
    pub resolve_reason: Option<String>,
    pub session_id: Option<String>,
    pub access_count: u64,
    pub created_at: String,
    pub last_accessed_at: String,
    pub updated_at: Option<String>,
}

/// A memory annotated for retrieval: fused/effective score + age bucket.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MemoryHit {
    #[serde(flatten)]
    pub record: MemoryRecord,
    pub score: f32,
    pub age_days: i64,
    pub age_status: AgeStatus,
}

/// New memory to persist (id/timestamps assigned by the store).
#[derive(Clone, Debug)]
pub struct NewMemory {
    pub project: String,
    pub content: String,
    pub category: Category,
    pub tags: Vec<String>,
    pub importance: f32,
    pub session_id: Option<String>,
}

/// Result of `add`: a near-duplicate is a normal outcome (the dedup gate), not
/// an error — the MCP layer surfaces the existing memory to the agent.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "lowercase")]
pub enum AddOutcome {
    Added { id: MemoryId },
    Duplicate { existing: MemoryId, distance: f32 },
}

// ──────────────────────────── knowledge graph (M8) ────────────────────────────

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Triple {
    pub id: String,
    pub subject: String,
    pub predicate: String,
    pub object: String,
    pub valid_from: Option<String>,
    pub valid_to: Option<String>,
    pub confidence: f32,
    pub source: Option<String>,
}

// ──────────────────────────── unified recall ────────────────────────────

/// The flagship synergy result: code + memory + facts for one task query,
/// returned as SEPARATE sections (each ranked in its own space — never merged
/// into one mixed list, since code-similarity and memory-effective-score are
/// not directly comparable).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RecallResult {
    pub relevant_code: Vec<CodeHit>,
    pub relevant_memory: Vec<MemoryHit>,
    pub facts: Vec<Triple>,
}
