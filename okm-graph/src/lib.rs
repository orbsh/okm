//! okm-graph — graph algorithms over okm's edge entries and
//! okm-query's ordered-stream operators.
//!
//! The graph already exists in the store: an `EdgeEncode` type
//! double-writes both directions, so a node's neighbor list is one
//! prefix scan. Algorithms here only iterate that fact — no adjacency
//! materialization, no new primitive. Community detection (label
//! propagation) and PageRank are fold passes over neighbor scans.

/// One label-propagation iteration: every node adopts the most common
/// label among its neighbors (ties → keep current). `labels` maps node
/// identity → current label; `neighbors` supplies each node's neighbor
/// set (the caller's per-node prefix scan over the edge entry space).
/// Returns the next generation. Convergence check belongs to the
/// caller — compare generations and stop when unchanged.
pub fn label_propagation_step<N, L>(
    nodes: &[N],
    labels: &mut std::collections::HashMap<N, L>,
    mut neighbors: impl FnMut(&N) -> Vec<N>,
) where
    N: Clone + Eq + std::hash::Hash + Ord,
    L: Clone + Eq + std::hash::Hash + Ord,
{
    let current = labels.clone();
    for n in nodes {
        let mut votes: std::collections::HashMap<L, usize> = Default::default();
        for peer in neighbors(n) {
            if let Some(l) = current.get(&peer) {
                *votes.entry(l.clone()).or_default() += 1;
            }
        }
        // Deterministic tie-break: smallest label wins — synchronous LP
        // with "keep current" never converges on symmetric structures
        // (a perfect triangle ties forever); smallest-label lets the
        // community collapse deterministically.
        let best = votes
            .iter()
            .max_by(|a, b| a.1.cmp(b.1).then(b.0.cmp(a.0)))
            .map(|(l, _)| l);
        if let Some(b) = best {
            labels.insert(n.clone(), b.clone());
        }
    }
}

/// PageRank over the same neighbor scans: `damping` 0.85 standard,
/// `iterations` fixed-step (convergence tolerance is the caller's
/// policy). In-degree mass arrives through the same reverse scans the
/// edge layer already guarantees.
pub fn pagerank<N>(
    nodes: &[N],
    mut neighbors: impl FnMut(&N) -> Vec<N>,
    iterations: usize,
    damping: f64,
) -> std::collections::HashMap<N, f64>
where
    N: Clone + Eq + std::hash::Hash,
{
    let n = nodes.len() as f64;
    let mut rank: std::collections::HashMap<N, f64> =
        nodes.iter().map(|n| (n.clone(), 1.0 / nodes.len() as f64)).collect();
    for _ in 0..iterations {
        let mut mass: std::collections::HashMap<N, f64> = Default::default();
        for node in nodes {
            let outs = neighbors(node);
            if outs.is_empty() {
                // Dangling: spread uniformly (standard simplification).
                for t in nodes {
                    *mass.entry(t.clone()).or_default() += rank[node] / n;
                }
            } else {
                let share = rank[node] / outs.len() as f64;
                for t in outs {
                    *mass.entry(t).or_default() += share;
                }
            }
        }
        rank = nodes
            .iter()
            .map(|node| {
                let r = (1.0 - damping) / n
                    + damping * mass.get(node).copied().unwrap_or(0.0);
                (node.clone(), r)
            })
            .collect();
    }
    rank
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Triangle + tail: 1-2-3 form a dense community, 4 hangs off 1.
    fn edges(n: &u32) -> Vec<u32> {
        match *n {
            1 => vec![2, 3, 4],
            2 => vec![1, 3],
            3 => vec![1, 2],
            4 => vec![1],
            _ => vec![],
        }
    }

    #[test]
    fn label_propagation_groups_the_triangle() {
        let nodes = vec![1u32, 2, 3, 4];
        let mut labels: HashMap<u32, u32> =
            nodes.iter().map(|n| (*n, *n)).collect();
        // Three steps: triangle collapses, then 4's adopted label from
        // the transient step drains back to the community label.
        for _ in 0..3 {
            label_propagation_step(&nodes, &mut labels, edges);
        }
        let l1 = labels[&1];
        assert_eq!(labels[&2], l1);
        assert_eq!(labels[&3], l1);
        assert_eq!(labels[&4], l1);
    }

    #[test]
    fn pagerank_mass_flows_to_hub() {
        let nodes = vec![1u32, 2, 3, 4];
        let rank = pagerank(&nodes, edges, 20, 0.85);
        // Node 1 receives from 2, 3, and 4 — highest rank.
        let mut vals: Vec<(u32, f64)> =
            rank.into_iter().collect();
        vals.sort_by(|a, b| b.1.total_cmp(&a.1));
        assert_eq!(vals[0].0, 1);
        // Total mass conserved (with dangling spread folded in).
        let sum: f64 = vals.iter().map(|(_, r)| r).sum();
        assert!((sum - 1.0).abs() < 1e-9);
    }
}
