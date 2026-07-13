//! PageRank over the call graph — a free, structural prior on "which symbol matters".
//!
//! BM25 answers "which symbol mentions these words". It cannot answer "which of these
//! six equally-worded matches is the one you actually want". Graph centrality can, and
//! it costs nothing but a few passes over edges the indexer already stored.
//!
//! Direction matters, and the two directions mean OPPOSITE things:
//!
//!   * **authority** (edges caller → callee) — a symbol is important when important
//!     symbols CALL it. This finds the load-bearing core: the utilities and services
//!     everything routes through. Left alone it over-rewards trivial leaves — a
//!     one-line `store_err` called from 400 places outranks the orchestrator.
//!   * **hub** (edges reversed, callee → caller) — a symbol is important when it calls
//!     important symbols. This finds entry points and orchestrators (`main`,
//!     `run_serve`) — the places you start reading a codebase from.
//!
//! [`centrality`] returns both, so a caller can decide (and [`blend`] gives the
//! search-ranking default). Standard damping 0.85, iterated to convergence.

use std::collections::HashMap;

/// Damping factor — the classic 0.85: the chance a random walker follows an edge
/// rather than teleporting to a random node.
const DAMPING: f32 = 0.85;

/// Convergence threshold on the total absolute change across one iteration. Reached
/// in ~20-30 passes on a real call graph.
const EPSILON: f32 = 1e-6;

/// Hard iteration cap so a pathological graph cannot spin.
const MAX_ITERS: usize = 100;

/// Both centrality views of one call graph.
#[derive(Clone, Debug, Default)]
pub struct Centrality {
    /// Called-by-important-things: the load-bearing core.
    pub authority: HashMap<String, f32>,
    /// Calls-important-things: entry points and orchestrators.
    pub hub: HashMap<String, f32>,
}

/// PageRank over `edges` (`from → to`), restricted to `nodes`.
///
/// Edges whose endpoints are not both in `nodes` are dropped — the call table stores
/// edges to external/std symbols we have no definition for, and letting them absorb
/// rank would silently drain it out of the project. Dangling nodes (no outgoing edge)
/// redistribute their rank uniformly, the standard fix that keeps the vector summing
/// to 1 instead of leaking mass every iteration.
///
/// Scores sum to ~1.0 across `nodes`; an empty graph yields an empty map.
fn rank(edges: &[(&str, &str)], nodes: &[String]) -> HashMap<String, f32> {
    let n = nodes.len();
    if n == 0 {
        return HashMap::new();
    }
    // Index the nodes so the hot loop works on integers, not strings.
    let idx: HashMap<&str, usize> = nodes.iter().enumerate().map(|(i, s)| (s.as_str(), i)).collect();

    let mut out_links: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (from, to) in edges {
        if let (Some(&f), Some(&t)) = (idx.get(from), idx.get(to)) {
            if f != t {
                // Self-recursion adds no information about relative importance.
                out_links[f].push(t);
            }
        }
    }

    let init = 1.0 / n as f32;
    let mut score = vec![init; n];
    let mut next = vec![0.0f32; n];

    for _ in 0..MAX_ITERS {
        // Rank held by nodes with no outgoing edge — spread uniformly, not dropped.
        let dangling: f32 = (0..n)
            .filter(|&i| out_links[i].is_empty())
            .map(|i| score[i])
            .sum();
        let base = (1.0 - DAMPING) / n as f32 + DAMPING * dangling / n as f32;
        next.iter_mut().for_each(|s| *s = base);

        for i in 0..n {
            let outs = &out_links[i];
            if outs.is_empty() {
                continue;
            }
            let share = DAMPING * score[i] / outs.len() as f32;
            for &j in outs {
                next[j] += share;
            }
        }

        let delta: f32 = score.iter().zip(&next).map(|(a, b)| (a - b).abs()).sum();
        score.copy_from_slice(&next);
        if delta < EPSILON {
            break;
        }
    }

    nodes.iter().cloned().zip(score).collect()
}

/// Both centrality views over one call graph. `edges` are `(caller, callee)` pairs of
/// qualified names; `nodes` is every symbol we have a definition for.
pub fn centrality(edges: &[(&str, &str)], nodes: &[String]) -> Centrality {
    let reversed: Vec<(&str, &str)> = edges.iter().map(|(f, t)| (*t, *f)).collect();
    Centrality {
        authority: rank(edges, nodes),
        hub: rank(&reversed, nodes),
    }
}

/// The single score search ranking uses: the geometric-ish mean of the two views.
///
/// Authority alone floats trivial leaves (a one-line error helper called everywhere);
/// hub alone floats `main` and little else. Requiring a symbol to score on BOTH — to
/// be *reached from* important code AND to *reach* important code — is what selects
/// the meaty middle of a codebase, which is what a search is almost always after.
///
/// `sqrt(a * h)` is 0 unless both are non-trivial, which is exactly the AND we want.
pub fn blend(c: &Centrality, node: &str) -> f32 {
    let a = c.authority.get(node).copied().unwrap_or(0.0);
    let h = c.hub.get(node).copied().unwrap_or(0.0);
    (a * h).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nodes(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn scores_sum_to_one_and_a_hub_beats_a_leaf_in_the_hub_view() {
        // main -> a -> util ; main -> b -> util
        let ns = nodes(&["main", "a", "b", "util"]);
        let edges = [("main", "a"), ("main", "b"), ("a", "util"), ("b", "util")];
        let c = centrality(&edges, &ns);

        let sum: f32 = c.authority.values().sum();
        assert!((sum - 1.0).abs() < 0.01, "authority must sum to ~1, got {sum}");

        // `util` is called by everything → highest AUTHORITY.
        let top_auth = c
            .authority
            .iter()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
            .unwrap()
            .0;
        assert_eq!(top_auth, "util", "the most-called symbol is the top authority");

        // `main` calls everything → highest HUB.
        let top_hub = c.hub.iter().max_by(|a, b| a.1.partial_cmp(b.1).unwrap()).unwrap().0;
        assert_eq!(top_hub, "main", "the entry point is the top hub");
    }

    #[test]
    fn blend_demotes_both_the_trivial_leaf_and_the_bare_entry_point() {
        // `mid` is both called by main AND calls util → it should win the blend, while
        // `util` (pure leaf) and `main` (pure root) each score on only one view.
        let ns = nodes(&["main", "mid", "util"]);
        let edges = [("main", "mid"), ("mid", "util")];
        let c = centrality(&edges, &ns);

        let mid = blend(&c, "mid");
        let util = blend(&c, "util");
        let main = blend(&c, "main");
        assert!(mid > util, "mid ({mid}) must beat the leaf util ({util})");
        assert!(mid > main, "mid ({mid}) must beat the bare root main ({main})");
    }

    #[test]
    fn edges_to_unknown_symbols_do_not_drain_rank() {
        // `println` has no definition in the project; its edge must be dropped rather
        // than absorbing rank that belongs to project symbols.
        let ns = nodes(&["a", "b"]);
        let edges = [("a", "b"), ("a", "println"), ("b", "println")];
        let c = centrality(&edges, &ns);
        let sum: f32 = c.authority.values().sum();
        assert!((sum - 1.0).abs() < 0.01, "rank must stay inside the project, got {sum}");
    }

    #[test]
    fn empty_graph_is_not_a_panic() {
        let c = centrality(&[], &[]);
        assert!(c.authority.is_empty() && c.hub.is_empty());
        assert_eq!(blend(&c, "nope"), 0.0);
    }

    #[test]
    fn self_recursion_does_not_inflate_a_symbol() {
        let ns = nodes(&["rec", "other"]);
        let plain = centrality(&[("rec", "other")], &ns);
        let with_self = centrality(&[("rec", "other"), ("rec", "rec")], &ns);
        let a = plain.authority["rec"];
        let b = with_self.authority["rec"];
        assert!((a - b).abs() < 0.01, "self-call must not boost rank: {a} vs {b}");
    }
}
