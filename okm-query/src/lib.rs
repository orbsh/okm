//! okm-query — consumer-side query operators over okm's ordered scan
//! streams. The core's obligation ends at "give an ordered iterator per
//! access method"; composing two streams (join), walking edge graphs
//! (one hop = one scan), and folding groups are algorithm-layer work and
//! live here. Zero core changes: every input is an existing `scan` /
//! `scan_index` output.

/// Merge join over two key-ordered entry streams (both sides already
/// sorted — `scan_index` returns entries in key order, which is the
/// merge order). Yields matched `(left, right)` pairs; keys compare via
/// the full encoded prefix the caller passes in. Equality-only: ranges
/// belong to the caller's scan prefix, not here.
///
/// The natural okm shape: index entries whose tail segment is the
/// primary key, so a join on the primary key is a merge join on the two
/// scan results' key tails. The caller supplies the key extractor
/// because only it knows the segment layout (`KEY_PREFIX` width).
///
/// Deliberately no hash join: a join key that is not either side's sort
/// dimension is a modeling gap — declare a `kv_index(fields(join_key))`
/// and the stream is ordered again. Sorting is a property of the store,
/// not a burden on the query operator (and merge join stays streaming
/// and memory-bounded, where hash join must materialize the build side).

use okm::KvEngine;

pub fn merge_join<K: Ord, L: Clone, R: Clone>(
    left: impl IntoIterator<Item = (K, L)>,
    right: impl IntoIterator<Item = (K, R)>,
) -> Vec<(L, R)> {
    let mut out = Vec::new();
    let mut li = left.into_iter().peekable();
    let mut ri = right.into_iter().peekable();
    loop {
        let ord = match (li.peek(), ri.peek()) {
            (Some((lk, _)), Some((rk, _))) => lk.cmp(rk),
            _ => break,
        };
        match ord {
            std::cmp::Ordering::Less => {
                li.next();
            }
            std::cmp::Ordering::Greater => {
                ri.next();
            }
            std::cmp::Ordering::Equal => {
                // Drain the run of equal keys on both sides — a
                // nested-loop over the equal runs, matching SQL join
                // semantics when keys duplicate. The run key is the
                // run's first key, taken by value — no `K: Clone`.
                let (run_key, v0) = li.next().unwrap();
                let mut lrun = vec![v0];
                loop {
                    let same = li.peek().is_some_and(|(k, _)| *k == run_key);
                    if !same {
                        break;
                    }
                    lrun.push(li.next().unwrap().1);
                }
                loop {
                    let same = ri.peek().is_some_and(|(k, _)| *k == run_key);
                    if !same {
                        break;
                    }
                    let r = ri.next().unwrap().1;
                    for l in &lrun {
                        out.push((l.clone(), r.clone()));
                    }
                }
            }
        }
    }
    out
}
/// Group-fold over an ordered entry stream: consecutive entries whose
/// extracted group key matches fold into one accumulator. Ordered input
/// (which every okm scan is) makes this a single pass — the KV-side
/// equivalent of SQL GROUP BY when the index already sorts by the group
/// dimension (put the group field first in `fields` and the sort does
/// the grouping).
pub fn group_by<G, T, A>(
    items: impl IntoIterator<Item = T>,
    key: impl Fn(&T) -> G,
    init: impl Fn() -> A,
    mut fold: impl FnMut(A, T) -> A,
) -> Vec<(G, A)>
where
    G: PartialEq,
{
    let mut out: Vec<(G, A)> = Vec::new();
    for item in items {
        match out.last_mut() {
            Some((g, acc)) if *g == key(&item) => {
                let a = std::mem::replace(acc, init());
                *acc = fold(a, item);
            }
            _ => {
                let g = key(&item);
                let acc = fold(init(), item);
                out.push((g, acc));
            }
        }
    }
    out
}

/// One hop across one edge type, keyed by the node's encoded identity.
/// The unifying currency of the graph layer is encoded key bytes — peer
/// identities cross edge-type boundaries as bytes and are decoded by
/// whoever consumes them (the node type is known at the call site).
///
/// A hop is exactly one prefix scan per direction: the edge layer
/// already guarantees both directions exist (double write), so
/// "neighbors" is the concatenation of the FWD and REV suffix scans —
/// no deduplication here: FWD and REV entries of one link cannot both
/// match one node's prefix unless the link is a self-loop, which the
/// caller models or filters.
pub trait GraphEdge<S: KvEngine + Clone> {
    /// All peers of `node` (its encoded identity) across this edge
    /// type, in both directions, as encoded peer-identity bytes.
    fn peers(&self, store: S, node: &[u8]) -> Vec<Vec<u8>>;
}

/// Walk: breadth-first from `start` up to `max_hops` hops across the
/// given edge types. `start`'s identity enters the frontier as bytes;
/// every discovered node is returned as its encoded identity (deduped —
/// a visited set over bytes is the graph discipline; revisiting is the
/// KV-side way to spend an unbounded scan budget).
///
/// Each hop is one prefix scan per edge type per direction — the cost
/// model is identical to `forward`/`reverse` in the edge layer; the
/// only added structure is the frontier and the visited set.
pub fn walk<S: KvEngine + Clone>(
    edges: &[&dyn GraphEdge<S>],
    store: S,
    start: &[u8],
    max_hops: usize,
) -> Vec<Vec<u8>> {
    let mut visited: Vec<Vec<u8>> = vec![start.to_vec()];
    let mut frontier: Vec<Vec<u8>> = visited.clone();
    for _ in 0..max_hops {
        let mut next: Vec<Vec<u8>> = Vec::new();
        for node in &frontier {
            for e in edges {
                for peer in e.peers(store.clone(), node) {
                    if !visited.contains(&peer) && !next.contains(&peer) {
                        next.push(peer);
                    }
                }
            }
        }
        if next.is_empty() {
            break;
        }
        visited.extend(next.iter().cloned());
        frontier = next;
    }
    visited
}

#[cfg(test)]
mod tests {
    use super::*;
    use okm::{EdgeEncode, KeyEncode, MockStore};

    /// Two edge types over one node type — the multi-relation graph a
    /// real model has (follows + mentions over User).
    #[derive(EdgeEncode, Clone, PartialEq, Debug)]
    #[kv_ns(4)]
    struct FollowsEdge {
        user_id: UserKey,
        followee_id: UserKey,
    }
    #[derive(EdgeEncode, Clone, PartialEq, Debug)]
    #[kv_ns(5)]
    struct MentionsEdge {
        user_id: UserKey,
        mentioned_id: UserKey,
    }
    #[derive(KeyEncode, Clone, PartialEq, Debug)]
    struct UserKey {
        id: u32,
    }

    fn mk(id: u32) -> UserKey {
        UserKey { id }
    }

    #[test]
    fn peers_spans_both_directions_and_both_edge_types() {
        // EdgeTable owns its engine; link phase returns the store.
        let mut follows: okm::EdgeTable<MockStore, FollowsEdge> =
            okm::EdgeTable::new(MockStore::default());
        follows.link(&mk(1), &mk(2));
        follows.link(&mk(3), &mk(1)); // incoming for node 1
        let mut store = follows.store;
        let mut mentions: okm::EdgeTable<MockStore, MentionsEdge> =
            okm::EdgeTable::new(std::mem::take(&mut store));
        mentions.link(&mk(1), &mk(4));
        let store = mentions.store;

        let f = FollowsNeighbors;
        let m = MentionsNeighbors;
        let mut got = f.peers(store.clone(), &mk(1).encode());
        got.extend(m.peers(store.clone(), &mk(1).encode()));
        got.sort();
        assert_eq!(
            got,
            vec![mk(2).encode(), mk(3).encode(), mk(4).encode()]
        );
    }

    struct FollowsNeighbors;
    impl GraphEdge<MockStore> for FollowsNeighbors {
        fn peers(&self, store: MockStore, node: &[u8]) -> Vec<Vec<u8>> {
            // Typed adapter over EdgeTable — the call site owns the
            // edge type and the node encoding.
            let t = okm::EdgeTable::<MockStore, FollowsEdge>::new(store);
            // Raw byte hop: decode the node, run both directions.
            let n = UserKey::decode(node);
            let mut out = t
                .forward(&n)
                .into_iter()
                .map(|p| p.encode())
                .collect::<Vec<_>>();
            out.extend(
                t.reverse(&n)
                    .into_iter()
                    .map(|p| p.encode()),
            );
            out
        }
    }
    struct MentionsNeighbors;
    impl GraphEdge<MockStore> for MentionsNeighbors {
        fn peers(&self, store: MockStore, node: &[u8]) -> Vec<Vec<u8>> {
            let t = okm::EdgeTable::<MockStore, MentionsEdge>::new(store);
            let n = UserKey::decode(node);
            let mut out = t
                .forward(&n)
                .into_iter()
                .map(|p| p.encode())
                .collect::<Vec<_>>();
            out.extend(
                t.reverse(&n)
                    .into_iter()
                    .map(|p| p.encode()),
            );
            out
        }
    }

    #[test]
    fn walk_two_hops_reaches_friends_of_friends() {
        let mut follows: okm::EdgeTable<MockStore, FollowsEdge> =
            okm::EdgeTable::new(MockStore::default());
        // 1 → 2 → 3: two hops from 1 reach 3.
        follows.link(&mk(1), &mk(2));
        follows.link(&mk(2), &mk(3));
        let store = follows.store;

        let f = FollowsNeighbors;
        let got = walk(&[&f], store, &mk(1).encode(), 2);
        let mut ids: Vec<u32> = got
            .iter()
            .map(|b| UserKey::decode(b).id)
            .collect();
        ids.sort();
        assert_eq!(ids, vec![1, 2, 3]);
    }

    #[test]
    fn group_by_folds_consecutive_runs() {
        let rows = vec![(7u64, 10), (7, 20), (9, 30)];
        let grouped = group_by(
            rows,
            |r| r.0,
            || 0u64,
            |acc, (_, v)| acc + v,
        );
        assert_eq!(grouped, vec![(7, 30), (9, 30)]);
    }
}
