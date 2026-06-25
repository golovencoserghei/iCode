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
    /// "own" | "inherited" | "override" resolution metadata (set by OOP model).
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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Call {
    pub path: String,
    pub caller: String,
    pub callee: String,
    /// Receiver of a method call ($this/self/ClassName/$var) for OOP resolution.
    pub receiver: Option<String>,
    pub line: u32,
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
