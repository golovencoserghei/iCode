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
    #[serde(default, serialize_with = "ser_confidence")]
    pub confidence: f32,
}

/// How far an agent can lean on one call edge, as a word instead of a float.
///
/// `calls.confidence` grades HOW an edge resolved (see [`Call::confidence`]), but a
/// bare `0.6` means nothing to a reader. Three buckets carry the decision that
/// matters: `exact` — the receiver pinned a single definition; `inferred` — a
/// homonym-class match or a unique bare name, usually right; `ambiguous` — a bare
/// name with several candidates, or nothing resolved at all. An edge read before
/// the resolve pass (confidence 0.0) is ambiguous by definition.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Certainty {
    Ambiguous,
    Inferred,
    Exact,
}

impl Certainty {
    /// Bucket a stored confidence. Thresholds sit BETWEEN the grades of the scale
    /// (0.9 / 0.7 / 0.6 / 0.4 / 0.3) so an f32 round-trip through SQLite REAL can
    /// never move an edge across a bucket boundary. NaN reads as ambiguous.
    pub fn from_confidence(confidence: f32) -> Self {
        if confidence >= 0.85 {
            Certainty::Exact
        } else if confidence >= 0.5 {
            Certainty::Inferred
        } else {
            Certainty::Ambiguous
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Certainty::Exact => "exact",
            Certainty::Inferred => "inferred",
            Certainty::Ambiguous => "ambiguous",
        }
    }

    /// Parse a user-facing level (`exact|inferred|ambiguous`, case-insensitive).
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "exact" => Some(Certainty::Exact),
            "inferred" => Some(Certainty::Inferred),
            "ambiguous" | "all" | "" => Some(Certainty::Ambiguous),
            _ => None,
        }
    }
}

impl std::fmt::Display for Certainty {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A path is only as trustworthy as its weakest hop: the minimum confidence over
/// `hops`. An empty path (source == target) is trivially exact (1.0). NaN hops are
/// treated as 0.0 so one corrupt row cannot make a path look certain.
pub fn weakest_link(hops: &[f32]) -> f32 {
    hops.iter()
        .map(|c| if c.is_nan() { 0.0 } else { *c })
        .fold(1.0f32, f32::min)
}

impl Call {
    /// The edge's [`Certainty`] bucket.
    pub fn certainty(&self) -> Certainty {
        Certainty::from_confidence(self.confidence)
    }
}

/// One hop of a call chain with the confidence of the edge that carries it (the
/// best edge between the two names when several exist).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChainHop {
    pub from: String,
    pub to: String,
    #[serde(serialize_with = "ser_confidence")]
    pub confidence: f32,
    pub certainty: Certainty,
}

/// A detected subsystem: a community of files that call each other more than they
/// call the rest of the project (see `icode_engine::cluster`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Subsystem {
    pub id: usize,
    /// Common directory prefix of its files (relative to the project root); when
    /// the files share none, the directory holding most of them, suffixed `+`.
    pub name: String,
    /// Highest-ranked symbol inside it — what the subsystem is "about".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_symbol: Option<String>,
    pub files: usize,
    pub functions: usize,
    /// Test files attached to it (the subsystem they call most). Tests do not
    /// shape the partition — they call into everything.
    pub tests: usize,
    /// Up to three most central files (relative paths).
    pub key_files: Vec<String>,
    /// The heaviest links to other subsystems: (subsystem id, edge weight).
    pub links: Vec<SubsystemLink>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SubsystemLink {
    pub to: usize,
    /// Sum of the confidence-weighted call/import edges between the two.
    pub weight: f64,
}

/// A hub: a symbol the rest of the code leans on (high centrality), with how many
/// distinct resolved call sites reach it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Hub {
    pub symbol: String,
    pub path: String,
    /// Resolved call sites from production code (tests excluded).
    pub callers: usize,
    pub rank: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subsystem: Option<usize>,
}

/// Subsystems + hubs for one scope, as cached in the index.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SubsystemMap {
    pub subsystems: Vec<Subsystem>,
    /// Files that ended up alone (no edges strong enough to join anything).
    pub unclustered_files: usize,
    pub hubs: Vec<Hub>,
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

/// Edge confidence on the wire: two decimals. The stored grades are 0.3..0.9 steps,
/// and an f32 widened to JSON prints them as `0.6000000238`.
pub fn ser_confidence<S: serde::Serializer>(v: &f32, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_f64(round_confidence(*v))
}

/// [`ser_confidence`]'s rounding, for responses built with `json!`.
pub fn round_confidence(v: f32) -> f64 {
    (f64::from(v) * 100.0).round() / 100.0
}

#[cfg(test)]
mod certainty_tests {
    use super::*;

    #[test]
    fn confidence_scale_maps_to_three_buckets() {
        assert_eq!(Certainty::from_confidence(0.9), Certainty::Exact);
        assert_eq!(Certainty::from_confidence(1.0), Certainty::Exact);
        assert_eq!(Certainty::from_confidence(0.7), Certainty::Inferred);
        assert_eq!(Certainty::from_confidence(0.6), Certainty::Inferred);
        assert_eq!(Certainty::from_confidence(0.4), Certainty::Ambiguous);
        assert_eq!(Certainty::from_confidence(0.3), Certainty::Ambiguous);
        // Before the resolve pass every edge is 0.0: ambiguous, never a panic.
        assert_eq!(Certainty::from_confidence(0.0), Certainty::Ambiguous);
        assert_eq!(Certainty::from_confidence(f32::NAN), Certainty::Ambiguous);
        // A REAL round-trip (f64 -> f32) must not move a grade across a boundary.
        assert_eq!(Certainty::from_confidence(0.9f64 as f32), Certainty::Exact);
        assert_eq!(Certainty::from_confidence(0.6f64 as f32), Certainty::Inferred);
    }

    #[test]
    fn certainty_orders_and_parses() {
        assert!(Certainty::Exact > Certainty::Inferred && Certainty::Inferred > Certainty::Ambiguous);
        assert_eq!(Certainty::parse("EXACT"), Some(Certainty::Exact));
        assert_eq!(Certainty::parse("inferred"), Some(Certainty::Inferred));
        assert_eq!(Certainty::parse("ambiguous"), Some(Certainty::Ambiguous));
        assert_eq!(Certainty::parse("maybe"), None);
        assert_eq!(serde_json::to_string(&Certainty::Exact).unwrap(), "\"exact\"");
    }

    #[test]
    fn a_path_is_as_strong_as_its_weakest_hop() {
        assert_eq!(weakest_link(&[0.9, 0.6, 0.9]), 0.6);
        assert_eq!(weakest_link(&[0.9, 0.3]), 0.3);
        assert_eq!(weakest_link(&[0.9]), 0.9);
        assert_eq!(weakest_link(&[]), 1.0);
        assert_eq!(weakest_link(&[0.9, f32::NAN]), 0.0);
        assert_eq!(Certainty::from_confidence(weakest_link(&[0.9, 0.0])), Certainty::Ambiguous);
    }
}
