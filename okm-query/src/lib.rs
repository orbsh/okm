//! okm-query — consumer-side query operators over okm-core's ordered scan
//! streams. The core's obligation ends at "give an ordered iterator per
//! access method"; composing two streams (join), walking edge graphs
//! (one hop = one scan), and folding groups are algorithm-layer work and
//! live here. Zero core changes: every input is an existing `scan` /
//! `scan_index` output.

// Tests keep the vocabulary: a panicking test is a failing test with a stack trace
// (and the oracle for every failable path is "assert on the error VALUE"). This line
// is per-crate because `[lints]` is per-package and covers every target, with no way
// to scope a lint to lib/bins — the manifest cannot express `cfg(test)`.
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unwrap_in_result,
        clippy::panic_in_result_fn
    )
)]

use okm_core::VirtualStorage;

/// Merge join over two key-ordered entry streams: both sides must already
/// be sorted BY THE EXTRACTED KEYS. An equality-prefix scan of an index
/// whose tail carries the join key is already in that order — with the
/// leading field fixed, the tails are the main-key bytes in order. A scan
/// whose probe ranges ACROSS the leading field is NOT: its tails sort by
/// the varied dimension, so extract-and-sort (or change the declaration)
/// before joining.
///
/// Yields matched `(left, right)` pairs; keys compare via the full encoded
/// prefix the caller passes in. Equality-only: ranges belong to the caller's
/// scan prefix, not here.
///
/// The natural okm-core shape: index entries whose tail segment is the
/// primary key, so a join on the primary key is a merge join on the two
/// scan results' key tails. The caller supplies the key extractor
/// because only it knows the segment layout (`KEY_PREFIX` width).
///
/// Deliberately no hash join: a join key that is not either side's sort
/// dimension is a modeling gap — declare a `ok_index(fields(join_key))`
/// and the stream is ordered again. Sorting is a property of the store,
/// not a burden on the query operator (and merge join stays streaming
/// and memory-bounded, where hash join must materialize the build side).
pub fn merge_join<K: Ord, L: Clone, R: Clone>(
    left: impl IntoIterator<Item = (K, L)>,
    right: impl IntoIterator<Item = (K, R)>,
) -> Vec<(L, R)> {
    let mut out = Vec::new();
    let mut li = left.into_iter().peekable();
    let mut ri = right.into_iter().peekable();
    while li.peek().is_some() && ri.peek().is_some() {
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
                let Some((run_key, v0)) = li.next() else {
                    break;
                };
                let mut lrun = vec![v0];
                while li.peek().is_some_and(|(k, _)| *k == run_key) {
                    // `peek` just answered; `next` answers too. Spelled as an
                    // `if let` so the guarantee costs no panic arm.
                    if let Some((_, v)) = li.next() {
                        lrun.push(v);
                    }
                }
                while ri.peek().is_some_and(|(k, _)| *k == run_key) {
                    let Some((_, r)) = ri.next() else {
                        break;
                    };
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
/// (which every okm-core scan is) makes this a single pass — the KV-side
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
pub trait GraphEdge<S: VirtualStorage + Clone> {
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
pub fn walk<S: VirtualStorage + Clone>(
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
    use okm_core::{DocumentEncode, KeyEncode, TestStore};

    /// 端点文档：user（ns=1）
    #[derive(DocumentEncode, Clone, PartialEq, Debug)]
    #[ok_ref(UserKey)]
    #[ok_ns(1)]
    struct User {
        id: u32,
    }
    #[derive(KeyEncode, Clone, PartialEq, Debug)]
    struct UserKey {
        id: u32,
    }

    /// 关系表（ADR-0051）：键是两个身份，行上带关系自有的列，反向方向是声明索引。
    /// 不再有 junction 载体——「谁关注了谁」就是一张普通的表。
    #[derive(KeyEncode, Clone, PartialEq, Debug)]
    pub struct FollowsKey {
        user_id: u32,
        followee_id: u32,
    }

    #[derive(DocumentEncode, Clone, PartialEq, Debug)]
    #[ok_ref(FollowsKey)]
    #[ok_ns(114)]
    #[ok_index(by_followee { fields(followee_id) })]
    pub struct FollowsRow {
        followee_id: u32,
    }

    #[derive(KeyEncode, Clone, PartialEq, Debug)]
    pub struct MentionsKey {
        user_id: u32,
        mentioned_id: u32,
    }

    #[derive(DocumentEncode, Clone, PartialEq, Debug)]
    #[ok_ref(MentionsKey)]
    #[ok_ns(115)]
    #[ok_index(by_mentioned { fields(mentioned_id) })]
    pub struct MentionsRow {
        mentioned_id: u32,
    }

    fn mk(id: u32) -> UserKey {
        UserKey { id }
    }

    fn follows(
        store: &TestStore,
        user_id: u32,
        followee_id: u32,
    ) {
        let table: okm_core::Collection<TestStore, FollowsKey, FollowsRow> =
            okm_core::Collection::new(store.clone());
        table
            .put(
                &FollowsKey {
                    user_id,
                    followee_id,
                },
                &FollowsRow { followee_id },
            )
            .unwrap();
    }

    fn mentions(store: &TestStore, user_id: u32, mentioned_id: u32) {
        let table: okm_core::Collection<TestStore, MentionsKey, MentionsRow> =
            okm_core::Collection::new(store.clone());
        table
            .put(
                &MentionsKey {
                    user_id,
                    mentioned_id,
                },
                &MentionsRow { mentioned_id },
            )
            .unwrap();
    }

    #[test]
    fn peers_spans_both_directions_and_both_edge_types() {
        let store = TestStore::slatedb_mem();
        follows(&store, 1, 2);
        follows(&store, 3, 1); // incoming for node 1
        mentions(&store, 1, 4);

        let f = FollowsNeighbors;
        let m = MentionsNeighbors;
        let mut got = f.peers(store.clone(), &mk(1).encode());
        got.extend(m.peers(store.clone(), &mk(1).encode()));
        got.sort();
        assert_eq!(got, vec![mk(2).encode(), mk(3).encode(), mk(4).encode()]);
    }

    struct FollowsNeighbors;
    impl GraphEdge<TestStore> for FollowsNeighbors {
        fn peers(&self, store: TestStore, node: &[u8]) -> Vec<Vec<u8>> {
            let n = UserKey::decode(node);
            let table: okm_core::Collection<TestStore, FollowsKey, FollowsRow> =
                okm_core::Collection::new(store);
            let mut out = Vec::new();
            // 正向：主键第一列是 user_id，前缀扫描拿到他关注的人
            let probe = okm_core::PrefixProbe::<FollowsKey>::new(n.encode());
            for key in table.scan_keys(&probe).flatten() {
                out.push(mk(key.followee_id).encode());
            }
            // 反向：by_followee 索引，回表拿关系行
            for (key, _) in table
                .scan::<__OkmIndex_FollowsRow_by_followee>(n.encode().as_slice())
                .expect("an index read")
            {
                out.push(mk(key.decoded.user_id).encode());
            }
            out
        }
    }

    struct MentionsNeighbors;
    impl GraphEdge<TestStore> for MentionsNeighbors {
        fn peers(&self, store: TestStore, node: &[u8]) -> Vec<Vec<u8>> {
            let n = UserKey::decode(node);
            let table: okm_core::Collection<TestStore, MentionsKey, MentionsRow> =
                okm_core::Collection::new(store);
            let mut out = Vec::new();
            let probe = okm_core::PrefixProbe::<MentionsKey>::new(n.encode());
            for key in table.scan_keys(&probe).flatten() {
                out.push(mk(key.mentioned_id).encode());
            }
            for (key, _) in table
                .scan::<__OkmIndex_MentionsRow_by_mentioned>(n.encode().as_slice())
                .expect("an index read")
            {
                out.push(mk(key.decoded.user_id).encode());
            }
            out
        }
    }

    #[test]
    fn walk_two_hops_reaches_friends_of_friends() {
        let store = TestStore::slatedb_mem();
        // 1 → 2 → 3: two hops from 1 reach 3.
        follows(&store, 1, 2);
        follows(&store, 2, 3);

        let f = FollowsNeighbors;
        let got = walk(&[&f], store, &mk(1).encode(), 2);
        let mut ids: Vec<u32> = got.iter().map(|b| UserKey::decode(b).id).collect();
        ids.sort();
        assert_eq!(ids, vec![1, 2, 3]);
    }

    #[test]
    fn group_by_folds_consecutive_runs() {
        let rows = vec![(7u64, 10), (7, 20), (9, 30)];
        let grouped = group_by(rows, |r| r.0, || 0u64, |acc, (_, v)| acc + v);
        assert_eq!(grouped, vec![(7, 30), (9, 30)]);
    }
}
