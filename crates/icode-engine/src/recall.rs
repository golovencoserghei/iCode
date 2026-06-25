//! Unified `recall` — the flagship code+memory synergy in one call.
//!
//! `recall` answers a task query against BOTH the per-project code graph and the
//! cross-session memory store, returning a [`RecallResult`] with THREE SEPARATE,
//! independently-ranked sections (`relevant_code`, `relevant_memory`, `facts`).
//! The sections are deliberately NOT merged into one mixed list: a code hit's
//! cosine/RRF score and a memory hit's usage-aware effective score live in
//! different spaces and are not directly comparable, so fusing them would only
//! invent a meaningless joint ranking. Each section is ranked in its own space.
//!
//! Decoupling: like `search`, this module depends only on the `icode-core`
//! contracts (`&dyn Embedder`, `&dyn MemoryStore`) — never on a concrete embedder
//! or store crate. The binary (or a dev test) supplies the implementations.
//!
//! Graceful degradation:
//!   * no `embedder` → code falls back to LEXICAL search (`search_code`),
//!   * no `memory`   → `relevant_memory` is empty (code is still returned).
//!
//! `facts` is always empty for now — the knowledge graph (M8) fills it later; the
//! field is kept in the result so the wire shape is stable across that milestone.
//!
//! All functions are SYNC (they call the sync store + sync `Embedder`/`MemoryStore`);
//! the MCP layer wraps them in `spawn_blocking`.

use icode_core::error::Result;
use icode_core::model::{Category, CodeQuery, RecallResult, SearchMode};
use icode_core::traits::{CodeReadStore, Embedder, MemoryStore};

use crate::search;
use crate::store::SqliteCodeStore;

/// One-call code + memory recall for a task `query`.
///
/// * `relevant_code` — `hybrid_search` (dense+lexical RRF) when an `embedder` is
///   present, else a LEXICAL `search_code` (so the code section always answers,
///   degraded but useful, when Ollama is down).
/// * `relevant_memory` — `memory.search(project, query, k, …)` when a memory
///   store is present, else empty.
/// * `facts` — always empty (KG is M8; see module docs).
///
/// The three sections are independent and each ranked in its own space — they are
/// NOT normalised against or merged into one another.
pub fn recall(
    code: &SqliteCodeStore,
    embedder: Option<&dyn Embedder>,
    memory: Option<&dyn MemoryStore>,
    project: &str,
    query: &str,
    k: usize,
) -> Result<RecallResult> {
    // ── code section (its own ranking space) ──
    let relevant_code = match embedder {
        Some(emb) => search::hybrid_search(code, emb, query, k)?,
        None => code.search_code(&CodeQuery {
            text: query.to_string(),
            kind: None,
            lang: None,
            limit: k,
            mode: SearchMode::Lexical,
            with_body: false,
        })?,
    };

    // ── memory section (its own ranking space; empty when no store) ──
    let relevant_memory = match memory {
        Some(mem) => mem.search(project, query, k, None, false)?,
        None => vec![],
    };

    // ── facts section (knowledge graph, M8) — intentionally empty for now ──
    let facts = vec![];

    Ok(RecallResult {
        relevant_code,
        relevant_memory,
        facts,
    })
}

/// Memory semantically related to a code symbol: "what do we remember about this
/// symbol?". The symbol name is used as the search query (any category, active
/// only). Returns an empty list (not an error) when nothing matches.
///
/// The query is just the bare symbol string for now; a future enrichment could
/// expand it with the symbol's docstring/qualified name, but the symbol name is
/// the load-bearing signal and keeps this decoupled from the code store.
pub fn code_to_memory(
    memory: &dyn MemoryStore,
    project: &str,
    symbol: &str,
    k: usize,
) -> Result<Vec<icode_core::model::MemoryHit>> {
    memory.search(project, symbol, k, None, false)
}

/// "Why does this exist?" — DECISION-category memory about a symbol (the rationale
/// behind it). Same as [`code_to_memory`] but filtered to `Category::Decision`,
/// surfacing the architectural reasoning rather than progress/context notes.
pub fn why_this_exists(
    memory: &dyn MemoryStore,
    project: &str,
    symbol: &str,
    k: usize,
) -> Result<Vec<icode_core::model::MemoryHit>> {
    memory.search(project, symbol, k, Some(Category::Decision), false)
}
