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

/// Merge join over two streams of `(key, value)` pairs, in ANY order.
///
/// The operator sorts both sides itself, so a caller never has to prove that
/// its stream is ordered. An already-ordered stream costs one linear pass (the
/// sort detects the run); an unordered one costs `O(n log n)`. That price is
/// taken deliberately: deciding which scan shapes come out ordered is subtle
/// enough to be wrong — an index scan is ordered by the index fields first and
/// by the primary key only INSIDE one equality group, a ranged probe is ordered
/// by the varied dimension, a multi-value index repeats keys, and a truncated
/// `key(...)` makes tails tie — and a join that works only on the shapes
/// someone remembered is worse than one that costs a sort. "Every caller works"
/// beats "the best case is one comparison cheaper".
///
/// A declared index still pays: it removes the sort, which is the difference
/// between `O(n)` and `O(n log n)`. It just stops being a correctness
/// precondition for this operator.
///
/// Yields matched `(left, right)` pairs; keys compare via the full encoded
/// prefix the caller passes in. Equality-only: ranges belong to the caller's
/// scan prefix, not here. The caller supplies the key extractor because only it
/// knows the segment layout (`KEY_PREFIX` width).
///
/// Still no hash join: this is a sort-merge join, so it holds `O(n)` memory for
/// the two sides (the join itself is a single pass over them) and needs no
/// hashing of the key type.
pub fn merge_join<K: Ord, L: Clone, R: Clone>(
    left: impl IntoIterator<Item = (K, L)>,
    right: impl IntoIterator<Item = (K, R)>,
) -> Vec<(L, R)> {
    let mut left: Vec<(K, L)> = left.into_iter().collect();
    let mut right: Vec<(K, R)> = right.into_iter().collect();
    // Stable, so equal keys keep the caller's order on either side — the pair
    // order inside one key run is a caller-visible detail, and preserving it
    // costs nothing here.
    left.sort_by(|a, b| a.0.cmp(&b.0));
    right.sort_by(|a, b| a.0.cmp(&b.0));

    let mut out = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < left.len() && j < right.len() {
        match left[i].0.cmp(&right[j].0) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                // Both runs of equal keys, then their cross product — SQL join
                // semantics when keys duplicate. Runs are found by comparison,
                // so `K` needs no `Clone`.
                let mut ie = i + 1;
                while ie < left.len() && left[ie].0 == left[i].0 {
                    ie += 1;
                }
                let mut je = j + 1;
                while je < right.len() && right[je].0 == right[j].0 {
                    je += 1;
                }
                for (_, l) in &left[i..ie] {
                    for (_, r) in &right[j..je] {
                        out.push((l.clone(), r.clone()));
                    }
                }
                i = ie;
                j = je;
            }
        }
    }
    out
}
/// Set difference over two `(key, value)` streams: the LEFT entries whose key has
/// no match on the right.
///
/// The other half of an outer join. `merge_join` answers "which keys meet", this
/// answers "which keys do not", and the pair covers both sides:
///
/// ```ignore
/// let common = merge_join(left.iter().cloned(), right.iter().cloned());
/// let left_only  = difference(left, right);   // left-only rows, key included
/// let right_only = difference(right, left);
/// ```
///
/// Sorts both sides, for the same reason `merge_join` does: which scan shapes come
/// out ordered is subtle, and a difference computed on an unordered stream is
/// silently wrong rather than slow. An already-ordered side costs one linear pass.
///
/// Yields `(key, value)` — the key on purpose, because "which one is exclusive" is
/// the whole answer of a difference (a `()` value would be indistinguishable).
/// A key present twice on the left is present twice on the right of `out`; this is
/// a multiset difference, not a set one.
pub fn difference<K: Ord, L, R>(
    left: impl IntoIterator<Item = (K, L)>,
    right: impl IntoIterator<Item = (K, R)>,
) -> Vec<(K, L)> {
    let mut left: Vec<(K, L)> = left.into_iter().collect();
    left.sort_by(|a, b| a.0.cmp(&b.0));
    let mut right: Vec<(K, R)> = right.into_iter().collect();
    right.sort_by(|a, b| a.0.cmp(&b.0));

    let mut right = right.into_iter().peekable();
    let mut out = Vec::new();
    for (key, value) in left {
        // Right is sorted, so a single forward cursor skips everything below the
        // current key — one pass over both sides in total.
        while right.peek().is_some_and(|(k, _)| *k < key) {
            right.next();
        }
        let matched = right.peek().is_some_and(|(k, _)| *k == key);
        if !matched {
            out.push((key, value));
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
    use okm_async::TestStore;
    use okm_core::{Document, DocumentEncode, KeyEncode};

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

    fn follows(store: &TestStore, user_id: u32, followee_id: u32) {
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
            for (_, key, _) in table
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
            for (_, key, _) in table
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

    /// Two tables that share a column: the join key is `name`, and each side
    /// declares an index on it, so both streams arrive ordered by it.
    #[derive(KeyEncode, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
    pub struct PersonKey {
        pub id: u32,
    }

    #[derive(DocumentEncode, Clone, PartialEq, Debug)]
    #[ok_ref(PersonKey)]
    #[ok_ns(116)]
    #[ok_index(by_name { fields(name) })]
    pub struct Person {
        pub name: String,
    }

    #[derive(KeyEncode, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
    pub struct OrderKey {
        pub id: u32,
    }

    #[derive(DocumentEncode, Clone, PartialEq, Debug)]
    #[ok_ref(OrderKey)]
    #[ok_ns(117)]
    #[ok_index(by_name { fields(name) })]
    pub struct Order {
        pub name: String,
        pub total: u32,
    }

    #[test]
    fn merge_join_does_not_care_what_order_the_inputs_are_in() {
        // The shape a ranged index scan actually produces: ordered by the index
        // field, so the joined key jumps back and forth — and, with a multi-value
        // index, repeats at unrelated positions. Sorting is the operator's job.
        let left = vec![(3u32, "c"), (1, "a"), (3, "c2"), (2, "b")];
        let right = vec![(2u32, 20), (3, 30), (3, 31), (9, 90)];

        let mut got = merge_join(left.clone(), right.clone());
        got.sort();
        assert_eq!(
            got,
            vec![("b", 20), ("c", 30), ("c", 31), ("c2", 30), ("c2", 31)],
            "equal keys on both sides join as a cross product of their runs"
        );

        // Same answer when both sides happen to arrive ordered: the sort is a
        // linear pass, never a precondition the caller has to establish.
        let mut sorted_left = left.clone();
        sorted_left.sort_by_key(|(k, _)| *k);
        let mut sorted_right = right.clone();
        sorted_right.sort_by_key(|(k, _)| *k);
        let mut got_sorted = merge_join(sorted_left, sorted_right);
        got_sorted.sort();
        assert_eq!(got_sorted, got);
    }

    #[test]
    fn merge_join_pairs_duplicate_runs_and_skips_unmatched_keys() {
        let got = merge_join(
            vec![(1u8, 'a'), (1, 'b'), (5, 'z')],
            vec![(1u8, 'x'), (1, 'y'), (4, 'w')],
        );
        let mut sorted = got.clone();
        sorted.sort();
        assert_eq!(sorted, vec![('a', 'x'), ('a', 'y'), ('b', 'x'), ('b', 'y')]);
        assert!(
            !got.iter().any(|(_, r)| *r == 'w') && !got.iter().any(|(l, _)| *l == 'z'),
            "keys present on one side only produce no pair"
        );
    }

    #[test]
    fn difference_answers_which_keys_do_not_meet() {
        let left = vec![(1u8, "a"), (2, "b"), (4, "d"), (4, "d2")];
        let right = vec![(4u8, 'x'), (3, 'c')];
        let mut only = difference(left.clone(), right);
        only.sort();
        assert_eq!(
            only,
            vec![(1, "a"), (2, "b")],
            "keys present on the right are dropped, and the key comes back with the value"
        );

        // Neither 3 nor 9 is on the left, so both survive from this side.
        let mut other_way = difference(vec![(3u8, 'c'), (9, 'z')], left);
        other_way.sort();
        assert_eq!(other_way, vec![(3, 'c'), (9, 'z')]);
    }

    /// A join on a column the scan ORDERS BY needs no sort on either side: an index
    /// scan is ordered by its fields first, so projecting onto the leading field is
    /// still ordered — the primary keys underneath are not in key order, and that is
    /// exactly why the merge key is the field and not the key.
    #[test]
    fn join_on_the_scanned_field_pairs_rows_by_that_field() {
        let store = TestStore::default();
        let users = <Person as Document>::collection(store.clone());
        let orders = <Order as Document>::collection(store.clone());
        for (k, r) in [
            (PersonKey { id: 1 }, Person { name: "ada".into() }),
            (PersonKey { id: 2 }, Person { name: "bob".into() }),
        ] {
            users.put(&k, &r).unwrap();
        }
        for (k, r) in [
            (OrderKey { id: 10 }, Order { name: "ada".into(), total: 5 }),
            (OrderKey { id: 11 }, Order { name: "bob".into(), total: 7 }),
            (OrderKey { id: 12 }, Order { name: "ada".into(), total: 9 }),
        ] {
            orders.put(&k, &r).unwrap();
        }

        // Field segment as the merge key: no fetch-back, no includes, no sort.
        let left: Vec<(String, PersonKey)> = users
            .scan::<__OkmIndex_Person_by_name>(&[])
            .unwrap()
            .into_iter()
            .map(|(fields, key, _)| (fields.name, key.decoded))
            .collect();
        let right: Vec<(String, OrderKey)> = orders
            .scan::<__OkmIndex_Order_by_name>(&[])
            .unwrap()
            .into_iter()
            .map(|(fields, key, _)| (fields.name, key.decoded))
            .collect();

        let mut pairs = merge_join(left, right);
        pairs.sort();
        assert_eq!(
            pairs,
            vec![
                (PersonKey { id: 1 }, OrderKey { id: 10 }),
                (PersonKey { id: 1 }, OrderKey { id: 12 }),
                (PersonKey { id: 2 }, OrderKey { id: 11 }),
            ],
            "each order pairs with the person of the same name"
        );
    }

    #[test]
    fn group_by_folds_consecutive_runs() {
        let rows = vec![(7u64, 10), (7, 20), (9, 30)];
        let grouped = group_by(rows, |r| r.0, || 0u64, |acc, (_, v)| acc + v);
        assert_eq!(grouped, vec![(7, 30), (9, 30)]);
    }
}
