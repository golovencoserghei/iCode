//! Semantic + hybrid code search service.
//!
//! `SqliteCodeStore::search_code` is and stays LEXICAL — the store holds no
//! embedder. The dense (vector) and fused (RRF) retrieval lives here, as free
//! functions that take `&dyn Embedder` (the frozen `icode-core` contract). This
//! keeps `icode-engine` decoupled from any concrete embedder crate: the binary
//! (or a test) supplies the embedder; we never depend on `icode-embed`.
//!
//! Three entry points:
//!   * [`semantic_search`] — pure dense KNN, cosine-similarity scored.
//!   * [`hybrid_search`]    — RRF fusion of a dense list and the lexical list.
//!   * [`find_similar`]     — "more like this symbol" via the symbol's own vector.
//!
//! All three are SYNC (they call the sync store + the sync `Embedder`); the MCP
//! layer wraps them in `spawn_blocking`.

use std::collections::HashMap;

use icode_core::error::{Error, Result};
use icode_core::model::{CodeHit, CodeQuery, SearchMode};
use icode_core::traits::{CodeReadStore, Embedder, VectorIndex};

use crate::chunk::{chunk_text_for_class, chunk_text_for_function};
use crate::store::SqliteCodeStore;

/// RRF reciprocal constant (the classic Cormack et al. value). Larger = flatter
/// (rank position matters less); 60 is the de-facto default and what
/// `SearchConfig::rrf_k` defaults to.
pub const RRF_K: f64 = 60.0;

/// How many candidates to pull from each retriever before fusing/truncating,
/// relative to the requested `k` (so a symbol just outside the top-k of one list
/// still has a chance to win on fusion). Capped by [`OVERSAMPLE_CAP`].
const OVERSAMPLE_MULT: usize = 4;
/// Absolute ceiling on a single retriever's candidate pull (mirrors
/// `SearchConfig::oversample_cap`). Keeps a huge `k` from yanking the world.
const OVERSAMPLE_CAP: usize = 200;

/// Oversample target for a requested `k`: `max(4*k, 50)` capped at 200. The floor
/// keeps small `k` (e.g. a 5-hit request) from starving fusion.
fn oversample(k: usize) -> usize {
    // 50 < OVERSAMPLE_CAP always holds (both consts), so clamp never panics.
    k.saturating_mul(OVERSAMPLE_MULT).clamp(50, OVERSAMPLE_CAP)
}

/// Reciprocal-Rank-Fusion of several ranked lists into one.
///
/// Each input list is assumed already sorted best-first. An item's fused score is
/// `Σ_lists 1/(k_const + rank)` where `rank` is its **0-based** position in that
/// list (so the top item of a list contributes `1/(k_const+0)`). Items are keyed
/// by `(qualified_name, path)`; the highest-quality `CodeHit` seen for a key (we
/// keep the FIRST occurrence, i.e. the best-ranked across the earliest list it
/// appears in, preferring an earlier list on ties) carries the key, with its
/// `.score` overwritten by the final RRF total. Output is sorted by fused score
/// descending (ties broken by `qualified_name` then `path` for determinism),
/// truncated to `top`.
pub fn rrf_fuse(lists: &[Vec<CodeHit>], k_const: f64, top: usize) -> Vec<CodeHit> {
    // key -> (accumulated rrf score, representative hit)
    let mut acc: HashMap<(String, String), (f64, CodeHit)> = HashMap::new();
    // Preserve first-seen order so a deterministic representative is chosen.
    for list in lists {
        for (rank, hit) in list.iter().enumerate() {
            let key = (hit.qualified_name.clone(), hit.path.clone());
            let contrib = 1.0 / (k_const + rank as f64);
            acc.entry(key)
                .and_modify(|(s, _)| *s += contrib)
                .or_insert_with(|| (contrib, hit.clone()));
        }
    }

    let mut fused: Vec<CodeHit> = acc
        .into_iter()
        .map(|(_, (score, mut hit))| {
            hit.score = score as f32;
            hit
        })
        .collect();

    // Deterministic order: score desc, then qualified_name, then path.
    fused.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.qualified_name.cmp(&b.qualified_name))
            .then_with(|| a.path.cmp(&b.path))
    });
    fused.truncate(top);
    fused
}

/// Pure dense retrieval: embed `query`, KNN the code-vector index, re-hydrate the
/// neighbour rowids into `CodeHit`s scored by cosine similarity (`1 - distance`,
/// vec0 stores cosine distance in `[0,2]`), dedup to the best hit per
/// `qualified_name`, return the top `k`.
///
/// Empty / whitespace query short-circuits to no hits (no wasted embed call).
pub fn semantic_search(
    store: &SqliteCodeStore,
    embedder: &dyn Embedder,
    query: &str,
    k: usize,
) -> Result<Vec<CodeHit>> {
    if query.trim().is_empty() || k == 0 {
        return Ok(vec![]);
    }
    let hits = dense_candidates(store, embedder, query, oversample(k))?;
    Ok(dedup_by_qualified_name(hits, k))
}

/// Fused retrieval: RRF over the dense (KNN) list and the lexical (FTS/BM25) list.
/// Both are oversampled to `oversample(k)` before fusion so a symbol strong in
/// only one retriever can still surface. Falls back gracefully — if the embedder
/// errors at the KNN step the error propagates (the caller, e.g. the MCP layer,
/// decides whether to degrade to lexical-only); a healthy embedder with an empty
/// dense list still returns the lexical hits via fusion.
pub fn hybrid_search(
    store: &SqliteCodeStore,
    embedder: &dyn Embedder,
    query: &str,
    k: usize,
) -> Result<Vec<CodeHit>> {
    if query.trim().is_empty() || k == 0 {
        return Ok(vec![]);
    }
    let over = oversample(k);

    let semantic = dense_candidates(store, embedder, query, over)?;
    let lexical = store.search_code(&CodeQuery {
        text: query.to_string(),
        kind: None,
        lang: None,
        limit: over,
        mode: SearchMode::Lexical,
        with_body: false,
    })?;

    Ok(rrf_fuse(&[semantic, lexical], RRF_K, k))
}

/// "More like this symbol": resolve `qualified_name` to its definition (function
/// then class), rebuild its embeddable chunk text with the SAME builder the index
/// uses ([`chunk_text_for_function`] / [`chunk_text_for_class`]), embed it, KNN the
/// index, drop the symbol itself from the results, and return the top `k` distinct
/// symbols by cosine similarity.
///
/// Resolution is by bare last segment (the store's `get_function`/`get_class` key
/// on the bare name); a `qualified_name` like `TreeWalker::walk` resolves on
/// `walk`. Returns an empty list (not an error) when the symbol is unknown.
pub fn find_similar(
    store: &SqliteCodeStore,
    embedder: &dyn Embedder,
    qualified_name: &str,
    k: usize,
) -> Result<Vec<CodeHit>> {
    if k == 0 {
        return Ok(vec![]);
    }
    let bare = bare_name(qualified_name);

    // Resolve the symbol and build its query text via the shared chunk builder.
    let query_text = if let Some(f) = store.get_function(bare, None, true)? {
        chunk_text_for_function(&f)
    } else if let Some(c) = store.get_class(bare, None, true)? {
        chunk_text_for_class(&c)
    } else {
        return Ok(vec![]);
    };

    // Oversample by one extra slot so excluding the symbol itself can't shrink us
    // below k.
    let over = oversample(k).saturating_add(1);
    let candidates = dense_candidates(store, embedder, &query_text, over)?;

    // Drop the symbol itself (match on qualified_name), then dedup + top-k.
    let filtered: Vec<CodeHit> = candidates
        .into_iter()
        .filter(|h| h.qualified_name != qualified_name && h.qualified_name != bare)
        .collect();
    Ok(dedup_by_qualified_name(filtered, k))
}

// ──────────────────────────── helpers ────────────────────────────

/// Embed `text` and KNN the code-vector index, re-hydrating neighbours into
/// `CodeHit`s scored by cosine similarity. Neighbours are returned best-first
/// (ascending distance == descending similarity); `chunk_hits` may drop rowids
/// whose chunk row vanished, so we re-pair scores by rowid rather than by index.
fn dense_candidates(
    store: &SqliteCodeStore,
    embedder: &dyn Embedder,
    text: &str,
    pull: usize,
) -> Result<Vec<CodeHit>> {
    let qvec = embedder.embed(&[text])?;
    let qvec = qvec
        .into_iter()
        .next()
        .ok_or_else(|| Error::Embed("embedder returned no vector for the query".into()))?;

    let neighbors = store.vector_index().knn(&qvec, pull)?;
    if neighbors.is_empty() {
        return Ok(vec![]);
    }

    let rowids: Vec<i64> = neighbors.iter().map(|n| n.rowid).collect();
    // rowid -> cosine distance (vec0 distance; lower is closer).
    let dist: HashMap<i64, f32> = neighbors.iter().map(|n| (n.rowid, n.distance)).collect();

    let mut hits: Vec<CodeHit> = store
        .chunk_hits(&rowids)?
        .into_iter()
        .map(|(rowid, mut hit)| {
            // Cosine similarity in [-1, 1]; clamp so a tiny numerical overshoot
            // can't print a >1 score.
            let d = dist.get(&rowid).copied().unwrap_or(1.0);
            hit.score = (1.0f32 - d).clamp(-1.0, 1.0);
            hit
        })
        .collect();

    // chunk_hits returns IN-clause order, not KNN order — re-sort by score desc so
    // the list is ranked best-first for the dedup/fuse stages.
    hits.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    Ok(hits)
}

/// Collapse a best-first hit list to one hit per `qualified_name` (the first/best
/// occurrence wins), truncated to `k`. A symbol can yield several chunk rows
/// (oversized split) — this keeps its strongest chunk only.
fn dedup_by_qualified_name(hits: Vec<CodeHit>, k: usize) -> Vec<CodeHit> {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut out: Vec<CodeHit> = Vec::with_capacity(k);
    for hit in hits {
        if seen.insert(hit.qualified_name.clone()) {
            out.push(hit);
            if out.len() >= k {
                break;
            }
        }
    }
    out
}

/// The bare last segment of a (possibly) qualified name (`A::b` → `b`,
/// `A.b` → `b`). Mirrors the store's resolution key.
fn bare_name(name: &str) -> &str {
    let after_colons = name.rsplit("::").next().unwrap_or(name);
    after_colons.rsplit('.').next().unwrap_or(after_colons)
}

#[cfg(test)]
mod tests {
    use super::*;
    use icode_core::model::SymbolKind;

    fn hit(qn: &str, path: &str) -> CodeHit {
        CodeHit {
            kind: SymbolKind::Function,
            name: qn.rsplit("::").next().unwrap_or(qn).to_string(),
            qualified_name: qn.to_string(),
            path: path.to_string(),
            line_start: 1,
            line_end: 2,
            score: 0.0,
            snippet: None,
            stale: false,
        }
    }

    #[test]
    fn rrf_promotes_a_shared_item_and_is_deterministic() {
        // List A: [x, a, b]   List B: [y, x, c]   (0-based ranks)
        // x appears in both (rank 0 in A, rank 1 in B → 1/60 + 1/61), so it must
        // outrank every item that appears in only one list (best of those is the
        // rank-0 head of a list: 1/60). 1/60+1/61 ≈ 0.03306 > 1/60 ≈ 0.01667.
        let a = vec![hit("x", "p"), hit("a", "p"), hit("b", "p")];
        let b = vec![hit("y", "p"), hit("x", "p"), hit("c", "p")];

        let fused = rrf_fuse(&[a, b], RRF_K, 10);

        assert_eq!(fused[0].qualified_name, "x", "shared item rises to the top");

        // Deterministic: identical inputs → identical order, run twice.
        let a2 = vec![hit("x", "p"), hit("a", "p"), hit("b", "p")];
        let b2 = vec![hit("y", "p"), hit("x", "p"), hit("c", "p")];
        let fused2 = rrf_fuse(&[a2, b2], RRF_K, 10);
        let order1: Vec<&str> = fused.iter().map(|h| h.qualified_name.as_str()).collect();
        let order2: Vec<&str> = fused2.iter().map(|h| h.qualified_name.as_str()).collect();
        assert_eq!(order1, order2, "fusion order is deterministic");

        // x's fused score = 1/(60+0) + 1/(60+1) (rank 0 in A, rank 1 in B).
        let expected = 1.0f32 / 60.0 + 1.0f32 / 61.0;
        assert!(
            (fused[0].score - expected).abs() < 1e-6,
            "x score {} ≈ {}",
            fused[0].score,
            expected
        );
    }

    #[test]
    fn rrf_dedups_by_qualified_name_and_path() {
        // Same (qn, path) in one list twice must collapse to a single entry whose
        // score sums both ranks.
        let a = vec![hit("dup", "p"), hit("other", "p"), hit("dup", "p")];
        let fused = rrf_fuse(&[a], RRF_K, 10);
        let dup_count = fused.iter().filter(|h| h.qualified_name == "dup").count();
        assert_eq!(dup_count, 1, "duplicate key collapses to one hit");
        // Same qn but DIFFERENT path is a distinct key.
        let b = vec![hit("dup", "p1"), hit("dup", "p2")];
        let fused_b = rrf_fuse(&[b], RRF_K, 10);
        assert_eq!(fused_b.len(), 2, "qn+path keying keeps distinct paths separate");
    }

    #[test]
    fn rrf_truncates_to_top() {
        let a = vec![hit("a", "p"), hit("b", "p"), hit("c", "p")];
        let fused = rrf_fuse(&[a], RRF_K, 2);
        assert_eq!(fused.len(), 2);
    }

    #[test]
    fn oversample_floor_and_cap() {
        assert_eq!(oversample(1), 50, "small k hits the 50 floor");
        assert_eq!(oversample(20), 80, "4*k when between floor and cap");
        assert_eq!(oversample(1000), OVERSAMPLE_CAP, "huge k is capped");
    }
}
