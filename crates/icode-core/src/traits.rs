//! Frozen contract surface (DIP/ISP). Multi-implementation seams — `Embedder`,
//! `VectorIndex`, `Reranker`, and the memory-store traits (decorator chain) —
//! must stay stable; concrete impls live in other crates and depend only here.
//!
//! All traits are SYNC: storage is rusqlite (sync) and the embedder uses a
//! blocking HTTP client. The async MCP layer calls into these via
//! `spawn_blocking`, keeping the contract object-safe without `async-trait`.
//! Batch entry points keep the embed/index hot path allocation-light.

use crate::error::Result;
use crate::ids::MemoryId;
use crate::model::*;

// ──────────────────────────── embeddings ────────────────────────────

/// Turns text into vectors. Default impl = Ollama HTTP; fallback = fastembed ONNX.
pub trait Embedder: Send + Sync {
    /// Stable model identifier (stamped per chunk as `embed_model`).
    fn model_id(&self) -> &str;
    /// Output dimensionality (fixes the vec0 column width).
    fn dim(&self) -> usize;
    /// Backend name ("ollama" | "fastembed") — lets doctor/recall report degraded mode.
    fn backend(&self) -> &str {
        "unknown"
    }
    /// Embed a batch. Output order matches input; each vector has length `dim()`.
    /// Takes `&[&str]` so callers pass cheap pointer slices over owned chunk text.
    fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>>;
    /// Liveness probe (e.g. Ollama reachable + model present).
    fn health(&self) -> Result<()>;
}

// ──────────────────────────── vector index ────────────────────────────

/// Nearest-neighbour over a fixed-dim space. Default impl = sqlite-vec (vec0);
/// fallback for >ann_threshold chunks/project = usearch. The rowid maps to the
/// owning row (code_chunks.id, or memory via the mem_rowid bridge owned by the
/// store — the bridge is deliberately NOT part of this contract).
pub trait VectorIndex: Send + Sync {
    fn dim(&self) -> usize;
    fn upsert(&self, rowid: i64, embedding: &[f32]) -> Result<()>;
    /// Bulk upsert in one transaction (index hot path; avoids per-vector vtable+SQL).
    fn upsert_batch(&self, items: &[(i64, Vec<f32>)]) -> Result<()>;
    fn delete(&self, rowid: i64) -> Result<()>;
    /// Total vectors stored (doctor invariant: == owning table row count).
    fn count(&self) -> Result<u64>;
    /// Drop all vectors (used when rebuilding after a dim/model change).
    fn clear(&self) -> Result<()>;
    /// k nearest rowids to `query`, ascending by distance (lower = closer).
    fn knn(&self, query: &[f32], k: usize) -> Result<Vec<Neighbor>>;
}

#[derive(Clone, Copy, Debug)]
pub struct Neighbor {
    pub rowid: i64,
    pub distance: f32,
}

// ──────────────────────────── reranker ────────────────────────────

/// Cross-encoder reranker (default = fastembed TextRerank for code/recall;
/// `NoopReranker` returns identity scores).
pub trait Reranker: Send + Sync {
    /// Score each candidate against the query; returns (index, score) pairs,
    /// higher score = more relevant. Indices refer into `candidates`.
    fn rerank(&self, query: &str, candidates: &[&str]) -> Result<Vec<(usize, f32)>>;
}

// ──────────────────────────── code store (split per ISP) ────────────────────────────

/// Read side of the per-project code graph (consumed by the MCP code tools).
/// One impl (SqliteCodeStore); the full group-A surface is declared up front so
/// later milestones don't reshape a live trait.
pub trait CodeReadStore: Send + Sync {
    fn stats(&self) -> Result<DbStats>;
    fn repo_map(&self, top: usize) -> Result<RepoMap>;

    // search (lexical / semantic / hybrid unified)
    fn search_code(&self, q: &CodeQuery) -> Result<Vec<CodeHit>>;
    fn find_similar(&self, qualified_name: &str, limit: usize) -> Result<Vec<CodeHit>>;

    // precise fetch
    fn get_function(&self, name: &str, lang: Option<Language>, with_body: bool) -> Result<Option<FunctionDef>>;
    fn get_class(&self, name: &str, lang: Option<Language>, with_body: bool) -> Result<Option<ClassDef>>;
    fn file_outline(&self, path: &str) -> Result<Vec<CodeHit>>;
    fn symbol_context(&self, name: &str, file_hint: Option<&str>) -> Result<SymbolContext>;

    // call graph (recursive-CTE backed)
    fn get_callers(&self, name: &str, limit: usize) -> Result<Vec<Call>>;
    fn get_callees(&self, name: &str, limit: usize) -> Result<Vec<Call>>;
    fn call_chain(&self, from: &str, to: &str, max_depth: usize) -> Result<Vec<String>>;
    fn find_dependencies(&self, path: &str, depth: usize) -> Result<Vec<String>>;
    fn impact_analysis(&self, path: &str, depth: usize) -> Result<Vec<String>>;
    fn find_implementations(&self, name: &str) -> Result<Vec<String>>;

    // analysis
    fn find_dead_code(&self, lang: Option<Language>, limit: usize) -> Result<Vec<CodeHit>>;
    fn find_unreachable(&self, lang: Option<Language>, limit: usize) -> Result<Vec<CodeHit>>;
    fn find_complex_functions(&self, lang: Option<Language>, limit: usize) -> Result<Vec<ComplexFunction>>;
    fn find_routes(&self, method: Option<&str>, path: Option<&str>, handler: Option<&str>, limit: usize) -> Result<Vec<Route>>;

    // text / files
    fn grep_code(&self, pattern: &str, lang: Option<Language>, limit: usize) -> Result<Vec<GrepHit>>;
    fn list_files(&self, pattern: Option<&str>, lang: Option<Language>, limit: usize) -> Result<Vec<FileRecord>>;
    fn stat_file(&self, path: &str) -> Result<Option<FileRecord>>;
    fn read_file(&self, path: &str, start: Option<u32>, end: Option<u32>) -> Result<String>;

    // KNN re-hydration: turn vec rowids back into lean code hits
    fn chunk_hits(&self, rowids: &[i64]) -> Result<Vec<(i64, CodeHit)>>;
}

/// Write side of the per-project code graph (consumed by the indexer).
pub trait CodeWriteStore: Send + Sync {
    /// Replace all rows for one file (idempotent upsert keyed by path).
    fn upsert_file(
        &self,
        file: &FileRecord,
        functions: &[FunctionDef],
        classes: &[ClassDef],
        imports: &[Import],
        calls: &[Call],
        routes: &[Route],
    ) -> Result<()>;
    fn delete_file(&self, path: &str) -> Result<()>;
    /// Persist chunk rows; returns assigned rowids in input order (to pair with
    /// embeddings for the vector index).
    fn upsert_chunks(&self, path: &str, chunks: &[CodeChunk]) -> Result<Vec<i64>>;
    /// Chunks whose embedding is missing or stamped with a different model
    /// (feeds the async embed queue). Returns (rowid, text).
    fn pending_chunks(&self, embed_model: &str, limit: usize) -> Result<Vec<(i64, String)>>;
    fn record_parse_error(&self, path: &str, error: &str) -> Result<()>;
}

// ──────────────────────────── memory store (decorator chain) ────────────────────────────

/// Read side. Implemented by the base store AND every decorator (delegating).
pub trait ReadableMemoryStore: Send + Sync {
    fn search(
        &self,
        project: &str,
        query: &str,
        n: usize,
        category: Option<Category>,
        include_resolved: bool,
    ) -> Result<Vec<MemoryHit>>;
    /// Cross-project semantic search (one KNN over the whole memory space).
    fn search_all(&self, query: &str, n: usize, include_resolved: bool) -> Result<Vec<MemoryHit>>;
    fn list(
        &self,
        project: &str,
        category: Option<Category>,
        limit: usize,
        include_resolved: bool,
    ) -> Result<Vec<MemoryRecord>>;
    fn get(&self, id: &MemoryId) -> Result<Option<MemoryRecord>>;
    /// Project name + memory count (reserved/internal projects excluded).
    fn list_projects(&self) -> Result<Vec<(String, u64)>>;
}

/// Write side. The decorator chain (Wal → Lock → Dedup → base) wraps this.
pub trait WritableMemoryStore: Send + Sync {
    /// Add a memory. A near-duplicate is a normal `AddOutcome::Duplicate`, not
    /// an error; `Err` is reserved for real failures (store/embed).
    fn add(&self, mem: NewMemory) -> Result<AddOutcome>;
    fn update(&self, id: &MemoryId, content: Option<&str>, tags: Option<&[String]>) -> Result<()>;
    fn delete(&self, id: &MemoryId) -> Result<()>;
    fn resolve(&self, id: &MemoryId, reason: &str) -> Result<()>;
    /// Bump access_count / last_accessed_at (warm-up; feeds usage-aware ranking).
    /// Best-effort — must not fail reads.
    fn record_access(&self, ids: &[MemoryId]) -> Result<()>;
    /// Register/refresh a project in the central registry.
    fn upsert_project(&self, name: &str, root_path: &str) -> Result<()>;
    /// Record a session touch (updates last_session_at).
    fn touch_session(&self, project: &str, session_id: &str) -> Result<()>;
}

/// Full memory store = read + write (object-safe; used as `Arc<dyn MemoryStore>`).
/// Each decorator must implement BOTH sides; the blanket impl assembles them.
pub trait MemoryStore: ReadableMemoryStore + WritableMemoryStore {}
impl<T: ReadableMemoryStore + WritableMemoryStore> MemoryStore for T {}
