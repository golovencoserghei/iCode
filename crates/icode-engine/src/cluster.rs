//! Deterministic community detection (Louvain) over a small weighted graph.
//!
//! Used to split a project's FILE graph into subsystems for `map`. No external
//! crate and no randomness: nodes are visited in index order, candidate
//! communities in first-seen order, and a tie keeps the current community (or the
//! first-seen candidate), so two runs over the same graph return the same partition — a
//! subsystem list that reshuffles between calls would be worse than none.
//!
//! The full multi-level algorithm is cheap at this size (a few thousand file
//! nodes): local moving until no node improves modularity, collapse communities
//! into super-nodes, repeat until a level merges nothing.

use std::collections::BTreeMap;

/// Partition `n` nodes joined by undirected weighted `edges` (`(a, b, w)`; any
/// order, duplicates summed, self-loops allowed, non-positive weights ignored).
/// Returns a community id per node, numbered `0..k` in order of each community's
/// first (lowest-index) node.
pub fn louvain(n: usize, edges: &[(usize, usize, f64)]) -> Vec<usize> {
    if n == 0 {
        return Vec::new();
    }
    // Merge parallel edges deterministically: key (min, max).
    let mut merged: BTreeMap<(usize, usize), f64> = BTreeMap::new();
    for &(a, b, w) in edges {
        if a >= n || b >= n || w <= 0.0 || !w.is_finite() {
            continue;
        }
        *merged.entry((a.min(b), a.max(b))).or_insert(0.0) += w;
    }
    let mut graph = Graph::from_merged(n, &merged);
    // node (original) -> current super-node
    let mut assignment: Vec<usize> = (0..n).collect();

    for _level in 0..16 {
        let comm = graph.local_moving();
        let k = 1 + comm.iter().copied().max().unwrap_or(0);
        if k == graph.n {
            break; // nothing merged at this level
        }
        for a in assignment.iter_mut() {
            *a = comm[*a];
        }
        graph = graph.aggregate(&comm, k);
        if k == 1 {
            break;
        }
    }
    renumber(&assignment)
}

/// Renumber labels to `0..k` by first appearance.
fn renumber(labels: &[usize]) -> Vec<usize> {
    let mut map: BTreeMap<usize, usize> = BTreeMap::new();
    let mut out = Vec::with_capacity(labels.len());
    for &l in labels {
        let next = map.len();
        out.push(*map.entry(l).or_insert(next));
    }
    out
}

struct Graph {
    n: usize,
    /// Neighbours (excluding self) with weights, sorted by neighbour index.
    adj: Vec<Vec<(usize, f64)>>,
    /// Self-loop weight per node (internal weight of a collapsed community).
    self_w: Vec<f64>,
    /// Weighted degree: sum of incident weights, a self-loop counted twice.
    degree: Vec<f64>,
    /// Sum of all degrees (= 2m).
    total: f64,
}

impl Graph {
    fn from_merged(n: usize, merged: &BTreeMap<(usize, usize), f64>) -> Self {
        let mut adj = vec![Vec::new(); n];
        let mut self_w = vec![0.0; n];
        for (&(a, b), &w) in merged {
            if a == b {
                self_w[a] += w;
            } else {
                adj[a].push((b, w));
                adj[b].push((a, w));
            }
        }
        for list in adj.iter_mut() {
            list.sort_by_key(|&(j, _)| j);
        }
        let degree: Vec<f64> = (0..n)
            .map(|i| adj[i].iter().map(|&(_, w)| w).sum::<f64>() + 2.0 * self_w[i])
            .collect();
        let total = degree.iter().sum();
        Graph { n, adj, self_w, degree, total }
    }

    /// One Louvain level: move each node to the neighbouring community with the best
    /// modularity gain until a full sweep moves nothing. Returns compact labels.
    fn local_moving(&self) -> Vec<usize> {
        let n = self.n;
        let mut comm: Vec<usize> = (0..n).collect();
        if self.total <= 0.0 {
            return comm;
        }
        let mut tot: Vec<f64> = self.degree.clone();
        // Scratch: weight from the current node to each community, plus the list of
        // communities touched (in first-seen order, which keeps the scan deterministic).
        let mut w_to: Vec<f64> = vec![0.0; n];
        let mut touched: Vec<usize> = Vec::new();
        let m2 = self.total;

        for _sweep in 0..64 {
            let mut moved = false;
            for i in 0..n {
                let ki = self.degree[i];
                let old = comm[i];
                for &(j, w) in &self.adj[i] {
                    let c = comm[j];
                    if w_to[c] == 0.0 {
                        touched.push(c);
                    }
                    w_to[c] += w;
                }
                tot[old] -= ki;
                // Gain of joining community c (constant terms dropped):
                //   w_to[c] - tot[c] * ki / 2m
                let mut best = old;
                let mut best_gain = w_to[old] - tot[old] * ki / m2;
                for &c in &touched {
                    if c == old {
                        continue;
                    }
                    let gain = w_to[c] - tot[c] * ki / m2;
                    if gain > best_gain + 1e-12 {
                        best = c;
                        best_gain = gain;
                    }
                }
                tot[best] += ki;
                if best != old {
                    comm[i] = best;
                    moved = true;
                }
                for &c in &touched {
                    w_to[c] = 0.0;
                }
                touched.clear();
            }
            if !moved {
                break;
            }
        }
        renumber(&comm)
    }

    /// Collapse each community into one node; internal weight becomes a self-loop.
    fn aggregate(&self, comm: &[usize], k: usize) -> Graph {
        let mut merged: BTreeMap<(usize, usize), f64> = BTreeMap::new();
        for i in 0..self.n {
            let ci = comm[i];
            if self.self_w[i] > 0.0 {
                *merged.entry((ci, ci)).or_insert(0.0) += self.self_w[i];
            }
            for &(j, w) in &self.adj[i] {
                if j < i {
                    continue; // each undirected edge once
                }
                let cj = comm[j];
                *merged.entry((ci.min(cj), ci.max(cj))).or_insert(0.0) += w;
            }
        }
        Graph::from_merged(k, &merged)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two dense triangles joined by one thin bridge.
    fn two_cliques() -> Vec<(usize, usize, f64)> {
        vec![
            (0, 1, 3.0),
            (1, 2, 3.0),
            (0, 2, 3.0),
            (3, 4, 3.0),
            (4, 5, 3.0),
            (3, 5, 3.0),
            (2, 3, 0.2),
        ]
    }

    #[test]
    fn two_obvious_clusters_are_found() {
        let c = louvain(6, &two_cliques());
        assert_eq!(c, vec![0, 0, 0, 1, 1, 1]);
    }

    #[test]
    fn the_partition_is_deterministic_and_order_independent_in_edges() {
        let e = two_cliques();
        let a = louvain(6, &e);
        let b = louvain(6, &e);
        assert_eq!(a, b);
        let mut rev = e.clone();
        rev.reverse();
        let rev: Vec<_> = rev.into_iter().map(|(x, y, w)| (y, x, w)).collect();
        assert_eq!(louvain(6, &rev), a, "edge order/orientation must not change the result");
    }

    #[test]
    fn isolated_nodes_stay_alone_and_empty_graphs_work() {
        assert_eq!(louvain(0, &[]), Vec::<usize>::new());
        assert_eq!(louvain(3, &[]), vec![0, 1, 2]);
        let c = louvain(4, &[(0, 1, 1.0)]);
        assert_eq!(c[0], c[1]);
        assert_ne!(c[2], c[3]);
        // Garbage weights are ignored, not propagated.
        let c = louvain(2, &[(0, 1, f64::NAN), (0, 1, -1.0), (0, 9, 1.0)]);
        assert_eq!(c, vec![0, 1]);
    }

    #[test]
    fn a_ring_of_cliques_splits_into_its_cliques() {
        // 5 cliques of 4, each joined to the next by a single light edge.
        let mut e = Vec::new();
        for k in 0..5 {
            let base = k * 4;
            for a in 0..4 {
                for b in (a + 1)..4 {
                    e.push((base + a, base + b, 1.0));
                }
            }
            e.push((base + 3, ((k + 1) % 5) * 4, 0.1));
        }
        let c = louvain(20, &e);
        for k in 0..5 {
            let base = k * 4;
            assert!((base..base + 4).all(|i| c[i] == c[base]), "clique {k} split: {c:?}");
        }
        let distinct: std::collections::BTreeSet<_> = c.iter().collect();
        assert_eq!(distinct.len(), 5, "{c:?}");
    }
}
