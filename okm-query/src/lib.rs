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

/// The MULTIPLICITY step: a merge join over two streams of `(key, value)` pairs,
/// in ANY order, answering m×n pairs for a key that both sides repeat.
///
/// The key-set half of a join is [`intersect`] — lazy, keys only, no pairs. Reach
/// for that first: intersecting entries and fetching only the survivors is cheaper
/// than fetching both sides fully and pairing them, unless nearly every key is
/// shared. This shape is for "give me both sides' rows for the keys that meet",
/// which materializes by nature, because the answer is a cross product.
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
/// Set algebra over key streams that are already ordered by key, as methods:
/// `a.merge(b)`, `a.intersect(b)`, `a.difference(b)`.
///
/// One alignment rule, three projections. [`merge`](OrderedSets::merge) walks both
/// streams by key and answers ONE tuple per distinct key — `(Some, None)` where
/// only the left has it, `(Some, Some)` where both do, `(None, Some)` where only
/// the right does. That is the general form: an outer join's three answers, a
/// full outer join in one pass, and "what does each side have that the other does
/// not" without a second operator. `intersect` and `difference` are filters over
/// it, named because those two questions are asked far more often than the rest.
///
/// SET semantics, not multiset, at every level: a key carried by several entries
/// is answered once, and one representative per side is handed back. Multiplicity
/// — m×n for a key both sides repeat — is the `merge_join` step's business, one
/// layer up.
///
/// The trait is on `IntoIterator`, so a `Vec`, an array, a scan iterator — anything
/// that yields keys — is a source, and no `.into_iter()` noise appears in a
/// chain. Chaining is the point: the item stays a key, so
/// `a.intersect(b).intersect(c)` and `a.intersect(b).difference(c)` are just calls
/// on the result. A source that must be read twice is cloned or held in an `Rc` —
/// these streams are read-only.
///
/// Ordered is a CONTRACT, not a runtime check. Verifying it would mean reading a
/// stream to its end before answering, which is the materialization these adapters
/// exist to avoid; so unordered input does not error, it silently misses keys. The
/// contract is free to meet: an index scan's order is the entry tuple
/// `(fields…, key)`, so a projection onto the leading fields is still ordered —
/// join on the first declared field and nothing needs sorting.
///
/// Lazy and allocation-free: `take(1)` consumes only what one answer needs.
pub trait OrderedSets: IntoIterator + Sized {
    /// Align two ordered key streams: one tuple per distinct key, `None` on the
    /// side that does not have it. The general form — an outer join reads it
    /// directly, and the two projections below are spelled in terms of it.
    fn merge<R>(self, right: R) -> impl Iterator<Item = (Option<Self::Item>, Option<Self::Item>)>
    where
        R: IntoIterator<Item = Self::Item>,
        Self::Item: Ord,
    {
        Merge {
            left: self.into_iter().peekable(),
            right: right.into_iter().peekable(),
        }
    }

    /// The keys on both sides, once each.
    fn intersect<R>(self, right: R) -> impl Iterator<Item = Self::Item>
    where
        R: IntoIterator<Item = Self::Item>,
        Self::Item: Ord,
    {
        self.merge(right)
            .filter_map(|(left, right)| right.and(left))
    }

    /// The keys on the left with no match on the right (`a \ b`) — the same as
    /// `a − (a ∩ b)`, without computing the intersection.
    fn difference<R>(self, right: R) -> impl Iterator<Item = Self::Item>
    where
        R: IntoIterator<Item = Self::Item>,
        Self::Item: Ord,
    {
        self.merge(right)
            .filter_map(|(left, right)| if right.is_none() { left } else { None })
    }
}

impl<I: IntoIterator> OrderedSets for I {}

/// The alignment behind every operator here: two ordered streams, one tuple per
/// distinct key.
struct Merge<LI: Iterator, RI: Iterator> {
    left: std::iter::Peekable<LI>,
    right: std::iter::Peekable<RI>,
}

impl<LI, RI> Iterator for Merge<LI, RI>
where
    LI: Iterator,
    RI: Iterator<Item = LI::Item>,
    LI::Item: Ord,
{
    type Item = (Option<LI::Item>, Option<LI::Item>);

    fn next(&mut self) -> Option<Self::Item> {
        let (left, right) = match self.right.peek() {
            // Nothing left to align against: the rest of the left is left-only.
            None => return Some((Some(self.left.next()?), None)),
            Some(right) => match self.left.peek()?.cmp(right) {
                std::cmp::Ordering::Less => {
                    let key = self.left.next()?;
                    drain_this_key(&mut self.left, &key);
                    return Some((Some(key), None));
                }
                std::cmp::Ordering::Greater => {
                    let key = self.right.next()?;
                    drain_this_key(&mut self.right, &key);
                    return Some((None, Some(key)));
                }
                std::cmp::Ordering::Equal => (self.left.next()?, self.right.next()?),
            },
        };
        // One representative from each side (the key is moved out, so no `Clone`
        // is needed), then both runs are emptied so the key cannot come back a
        // second time.
        drain_this_key(&mut self.left, &left);
        drain_this_key(&mut self.right, &right);
        Some((Some(left), Some(right)))
    }
}

/// Drop the remaining copies of `key` (the one on top, if it is `key`).
fn drain_this_key<I: Iterator>(it: &mut std::iter::Peekable<I>, key: &I::Item)
where
    I::Item: Ord,
{
    while it.peek().is_some_and(|peeked| peeked == key) {
        it.next();
    }
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
                .collect::<okm_core::error::Result<Vec<_>>>()
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
                .collect::<okm_core::error::Result<Vec<_>>>()
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

    /// Set semantics: a key carried by several entries on both sides comes out
    /// once. Multiplicity is the row step's business, not the key step's.
    #[test]
    fn intersect_yields_each_shared_key_once() {
        let left = vec![1u8, 2, 4, 4, 7];
        let right = vec![4u8, 4, 4, 5, 7];
        assert_eq!(
            left.into_iter().intersect(right).collect::<Vec<_>>(),
            vec![4, 7],
            "duplicates on either side do not multiply the answer"
        );
    }

    /// The fold IS the multi-way form: the item type stays a key, so N sides need
    /// no second operator.
    #[test]
    fn intersect_folds_across_three_sides() {
        let a = vec![1u8, 2, 3, 5, 8];
        let b = vec![2u8, 3, 4, 8];
        let c = vec![3u8, 8, 9];
        assert_eq!(
            a.into_iter().intersect(b).intersect(c).collect::<Vec<_>>(),
            vec![3, 8]
        );
    }

    /// Lazy: taking one answer consumes only what that answer needs. A counting
    /// iterator is the only way to see this from outside.
    #[test]
    fn intersect_takes_only_what_the_answer_needs() {
        struct Counted<I> {
            inner: I,
            pulled: std::rc::Rc<std::cell::Cell<usize>>,
        }
        impl<I: Iterator> Iterator for Counted<I> {
            type Item = I::Item;
            fn next(&mut self) -> Option<I::Item> {
                let item = self.inner.next();
                self.pulled.set(self.pulled.get() + 1);
                item
            }
        }
        let pulled = std::rc::Rc::new(std::cell::Cell::new(0));
        let left = Counted {
            inner: (1u32..=1000).collect::<Vec<_>>().into_iter(),
            pulled: pulled.clone(),
        };
        let right = Counted {
            inner: vec![7u32, 9].into_iter(),
            pulled: pulled.clone(),
        };
        assert_eq!(left.intersect(right).take(1).collect::<Vec<_>>(), vec![7]);
        assert!(
            pulled.get() < 20,
            "a lazy merge walks to the first shared key; it does not drain 1000 items (pulled {})",
            pulled.get()
        );
    }

    #[test]
    fn difference_yields_the_left_keys_the_right_does_not_have() {
        let left = vec![1u8, 2, 4, 4, 7];
        let right = vec![4u8, 5];
        assert_eq!(
            left.clone()
                .into_iter()
                .difference(right)
                .collect::<Vec<_>>(),
            vec![1, 2, 7],
            "a key on the right is not a difference, and a key twice on the left answers once"
        );
        assert_eq!(
            vec![9u8, 10]
                .into_iter()
                .difference(left)
                .collect::<Vec<_>>(),
            vec![9, 10],
            "an exhausted right side leaves the rest of the left as left-only"
        );
        assert!(
            Vec::<u8>::new()
                .into_iter()
                .difference(vec![1u8])
                .next()
                .is_none()
        );
    }

    /// The general form: one tuple per distinct key, `None` on the absent side.
    /// An outer join and both "what is extra" questions read from this one stream.
    #[test]
    fn merge_aligns_both_sides_key_by_key() {
        let left = vec![1u8, 2, 4];
        let right = vec![2u8, 3, 4];
        assert_eq!(
            left.into_iter().merge(right).collect::<Vec<_>>(),
            vec![
                (Some(1), None),
                (Some(2), Some(2)),
                (None, Some(3)),
                (Some(4), Some(4)),
            ]
        );
    }

    /// Multiple sides, in the order the caller wants: intersect twice, then ask
    /// what the result lacks against a third collection.
    #[test]
    fn operators_chain_across_more_than_two_sides() {
        let a = vec![1u8, 2, 3, 5];
        let b = vec![2u8, 3, 4, 5];
        let c = vec![3u8, 5, 9];
        // (a ∩ b) ∩ c
        assert_eq!(
            a.clone()
                .intersect(b.clone())
                .intersect(c.clone())
                .collect::<Vec<_>>(),
            vec![3, 5]
        );
        // a ∩ b, minus what c also has: the keys c does not carry.
        assert_eq!(a.intersect(b).difference(c).collect::<Vec<_>>(), vec![2]);
    }

    /// The join's two halves against the same streams: the intersection plus both
    /// differences reconstruct the two sides, with nothing double-counted.
    #[test]
    fn intersect_and_difference_partition_the_two_sides() {
        let left = vec![1u8, 2, 3, 5];
        let right = vec![2u8, 3, 4];
        let mut common: Vec<u8> = left.clone().into_iter().intersect(right.clone()).collect();
        let mut left_only: Vec<u8> = left.clone().into_iter().difference(right.clone()).collect();
        let mut right_only: Vec<u8> = right.clone().into_iter().difference(left.clone()).collect();
        common.sort();
        left_only.sort();
        right_only.sort();
        assert_eq!(common, vec![2, 3]);
        assert_eq!(left_only, vec![1, 5]);
        assert_eq!(right_only, vec![4]);

        let mut rebuilt = left_only.clone();
        rebuilt.extend(common.clone());
        rebuilt.sort();
        assert_eq!(rebuilt, left, "left = left-only + common");
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
            (
                OrderKey { id: 10 },
                Order {
                    name: "ada".into(),
                    total: 5,
                },
            ),
            (
                OrderKey { id: 11 },
                Order {
                    name: "bob".into(),
                    total: 7,
                },
            ),
            (
                OrderKey { id: 12 },
                Order {
                    name: "ada".into(),
                    total: 9,
                },
            ),
        ] {
            orders.put(&k, &r).unwrap();
        }

        // Field segment as the merge key: no fetch-back, no includes, no sort.
        let left: Vec<(String, PersonKey)> = users
            .scan::<__OkmIndex_Person_by_name>(&[])
            .collect::<okm_core::error::Result<Vec<_>>>()
            .unwrap()
            .into_iter()
            .map(|(fields, key, _)| (fields.name, key.decoded))
            .collect();
        let right: Vec<(String, OrderKey)> = orders
            .scan::<__OkmIndex_Order_by_name>(&[])
            .collect::<okm_core::error::Result<Vec<_>>>()
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
