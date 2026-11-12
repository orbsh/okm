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

/// Set algebra over key streams that are already ordered by key, as methods:
/// `a.merge(b)`, `a.intersect(b)`, `a.difference(b)`, `a.union(b)`.
///
/// One alignment rule, four projections. [`merge`](OrderedSets::merge) walks both
/// streams by key and answers ONE tuple per distinct key — `(Some, None)` where
/// only the left has it, `(Some, Some)` where both do, `(None, Some)` where only
/// the right does. That is the general form: an outer join's three answers, a
/// full outer join in one pass, and "what does each side have that the other does
/// not" without a second operator. `intersect`, `difference` and `union` are
/// filters over it, named because those questions are asked far more often than
/// the rest.
///
/// SET semantics, not multiset, at every level: a key carried by several entries
/// is answered once, and one representative per side is handed back. Multiplicity
/// — m×n for a key both sides repeat — is NOT a separate operator: `merge` is the
/// one-pass form (its `(Some, Some)` tuple is the key that meets) and expanding
/// that tuple into pairs is the caller's loop, which is also where rows get read.
/// That is the whole join: set algebra over keys here, one fetch per surviving
/// key there. A dedicated pair-expanding join would be a second semantics for
/// the same thing — and it is the shape that carries k² clones for two runs of
/// length k and a `Vec` that `LIMIT` cannot cut short.
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
    ///
    /// Same ordered contract as the trait: the input is ordered by key, and
    /// unordered input silently misses keys.
    ///
    /// Why this cannot gallop: an `Iterator` only moves forward, so there is no
    /// seek to jump with — "skipping ahead" to the other side's key still costs
    /// one `next()` per key skipped, which is exactly the walk the merge already
    /// does. The 12.7× gallop belongs to inputs that can be INDEXED, and it is
    /// [`gallop_intersect`] over slices (a seekable source re-seeks instead of
    /// advancing; that is a different operator, not a branch in this one).
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

    /// The keys on either side, once each (`a ∪ b`) — the third filter over the
    /// same alignment, and the one that answers with the most keys: every tuple
    /// `merge` produces carries at least one side, so nothing is dropped and the
    /// set discipline comes for free. A key both sides repeat still answers once,
    /// and the left representative wins when both sides have one (`left.or(right)`)
    /// — the order of the answer stays ordered because `merge` walks the keys in
    /// order.
    ///
    /// `union` is not `chain` plus a dedup: `chain` answers the whole left run
    /// before the first right key, so deduplicating it needs either a full
    /// materialization or a `HashSet` — both defeat `take(1)`. Here the same
    /// single pass that drops duplicates also interleaves the two sides.
    fn union<R>(self, right: R) -> impl Iterator<Item = Self::Item>
    where
        R: IntoIterator<Item = Self::Item>,
        Self::Item: Ord,
    {
        self.merge(right).filter_map(|(left, right)| left.or(right))
    }
}

impl<I: IntoIterator> OrderedSets for I {}

/// Set intersection over two ORDERED SLICES, galloping instead of walking — the
/// random-access half of [`intersect`](OrderedSets::intersect), for inputs that
/// can be indexed.
///
/// Same answer as the stream operator: one `&T` per shared key, in ascending
/// order, set semantics (a repeated key answers once). Zero copies — the items
/// are borrowed from the caller's slices, so this is also the shape to reach for
/// when the keys are expensive to clone.
///
/// The difference is the skip. A stream can only be advanced, so when one side
/// has to catch up it pays one `next()` per key it passes; a slice can be
/// SEARCHED, so catching up costs `O(log gap)` comparisons — an exponential
/// probe (1, 2, 4, 8 … keys ahead) to bracket the target, then a binary search
/// inside the bracket (`slice::partition_point`). Lopsided input is where that
/// pays: `benches/set_ops.rs` has 1 000 000 against 1 000 at 42 µs here versus
/// 1.49 ms for the merge form (35×), while balanced input (1 000 000 against
/// 500 000) comes out the same or marginally ahead (1.36 ms versus 1.55 ms) —
/// the probe's constant is paid exactly where the merge also spends a
/// comparison, so the shape that cannot win is also the shape that cannot
/// lose much.
///
/// Ordered is a CONTRACT, not a check — the same one the stream operators have:
/// both slices ascending. Unordered input does not error, it silently misses
/// keys. (Sorted slices are what a caller holds after sorting a scan; the scan
/// itself is a stream and goes through `intersect`.)
pub fn gallop_intersect<'a, T: Ord>(left: &'a [T], right: &'a [T]) -> impl Iterator<Item = &'a T> {
    GallopIntersect {
        left,
        right,
        i: 0,
        j: 0,
    }
}

/// [`gallop_intersect`]'s lazy cursor: two indices into the caller's slices.
struct GallopIntersect<'a, T> {
    left: &'a [T],
    right: &'a [T],
    i: usize,
    j: usize,
}

impl<'a, T: Ord> Iterator for GallopIntersect<'a, T> {
    type Item = &'a T;

    fn next(&mut self) -> Option<&'a T> {
        loop {
            // Copied out so the returned item borrows the caller's data, not
            // `self` — that is what keeps the signature `&'a T`.
            let (left, right) = (self.left, self.right);
            let (l, r) = (left.get(self.i)?, right.get(self.j)?);
            match l.cmp(r) {
                // One side is behind: seek it to the other's key instead of
                // stepping through every key in between.
                std::cmp::Ordering::Less => self.i = seek(left, self.i + 1, r),
                std::cmp::Ordering::Greater => self.j = seek(right, self.j + 1, l),
                std::cmp::Ordering::Equal => {
                    // The key is in both: emit one representative and step past
                    // every copy on both sides (set semantics, and the reason a
                    // repeated key cannot come back).
                    self.i = past_run(left, self.i, l);
                    self.j = past_run(right, self.j, l);
                    return Some(l);
                }
            }
        }
    }
}

/// The first index at or after `from` whose key is `>= target`: a galloping
/// lower bound.
///
/// Adjacent keys are the common case in a mostly-aligned merge and cost one
/// comparison; a real gap costs an exponential probe (doubling until the key at
/// `lo + step` is no longer below the target) plus one binary search over the
/// bracket, so `O(log(gap))` instead of `O(gap)`.
///
/// Finds the bracket, not the answer, by walking: every probe only proves that
/// the keys before it are below the target, which is what makes a slice's
/// random access cheaper than a stream's single step.
fn seek<T: Ord>(s: &[T], from: usize, target: &T) -> usize {
    let n = s.len();
    if from >= n {
        return n;
    }
    if s[from] >= *target {
        return from;
    }
    let mut lo = from; // s[lo] < target
    let mut step = 1usize;
    let mut hi = n;
    loop {
        let probe = lo + step;
        if probe >= n {
            break;
        }
        if s[probe] >= *target {
            hi = probe + 1;
            break;
        }
        lo = probe;
        step = step.saturating_mul(2);
    }
    lo + s[lo..hi].partition_point(|key| key < target)
}

/// The first index at or after `from` whose key is not `key` (the end of one
/// run of equal keys).
fn past_run<T: Ord>(s: &[T], from: usize, key: &T) -> usize {
    let mut i = from;
    while i < s.len() && s[i] == *key {
        i += 1;
    }
    i
}

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
        match (self.left.peek(), self.right.peek()) {
            // Nothing on either side: the stream is done.
            (None, None) => None,
            // One side exhausted is NOT the end: the other side's tail is
            // one-sided answers, and `merge` is the general form, so both tails
            // are emitted (a left-only intersection is a `filter_map` on top).
            (Some(_), None) => {
                let key = self.left.next()?;
                drain_this_key(&mut self.left, &key);
                Some((Some(key), None))
            }
            (None, Some(_)) => {
                let key = self.right.next()?;
                drain_this_key(&mut self.right, &key);
                Some((None, Some(key)))
            }
            (Some(left), Some(right)) => match left.cmp(right) {
                std::cmp::Ordering::Less => {
                    let key = self.left.next()?;
                    drain_this_key(&mut self.left, &key);
                    Some((Some(key), None))
                }
                std::cmp::Ordering::Greater => {
                    let key = self.right.next()?;
                    drain_this_key(&mut self.right, &key);
                    Some((None, Some(key)))
                }
                std::cmp::Ordering::Equal => {
                    let left = self.left.next()?;
                    let right = self.right.next()?;
                    // One representative from each side (the key is moved out, so
                    // no `Clone` is needed), then both runs are emptied so the key
                    // cannot come back a second time.
                    drain_this_key(&mut self.left, &left);
                    drain_this_key(&mut self.right, &right);
                    Some((Some(left), Some(right)))
                }
            },
        }
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
/// extracted group key matches fold into one accumulator, one `(group, acc)`
/// per run. Ordered input (which every okm-core scan is) makes this a single
/// pass — the KV-side equivalent of SQL GROUP BY when the index already sorts
/// by the group dimension (put the group field first in `fields` and the sort
/// does the grouping).
///
/// Lazy, like the scan verbs and the set operators: a group is answered when
/// the input moves past its run, so `.take(n)` walks only the runs it returns
/// and a chain never materializes. `.collect::<Vec<_>>()` IS the eager mirror
/// — there is no eager twin of this verb.
///
/// A fold over rows ALREADY IN HAND, and nothing more: it cannot answer
/// "which rows are in group X", because until those rows are read it has no
/// input at all. Grouping as a *maintained* answer is the reduce path's job
/// (`#[ok_reduce]`: the group lives in the entry key and `put` folds the row
/// into it). This shapes a result the caller has already produced — the
/// survivors of an intersection, one fetched page.
pub fn group_by<G, T, A>(
    items: impl IntoIterator<Item = T>,
    key: impl Fn(&T) -> G,
    init: impl Fn() -> A,
    mut fold: impl FnMut(A, T) -> A,
) -> impl Iterator<Item = (G, A)>
where
    G: PartialEq,
{
    // `peekable` holds the item that opened the NEXT run, so a group boundary is
    // seen without a pending slot of our own — and the item is not lost, only
    // answered by the following `next()`.
    let mut items = items.into_iter().peekable();
    std::iter::from_fn(move || {
        let first = items.next()?;
        let group = key(&first);
        let mut acc = fold(init(), first);
        while items.peek().is_some_and(|next| key(next) == group) {
            if let Some(item) = items.next() {
                acc = fold(acc, item);
            }
        }
        Some((group, acc))
    })
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

    /// `union` is a set: disjoint sides answer both, and the answer is ordered.
    #[test]
    fn union_answers_both_sides_once_in_order() {
        assert_eq!(
            vec![1u8, 4]
                .into_iter()
                .union(vec![2u8, 3])
                .collect::<Vec<_>>(),
            vec![1, 2, 3, 4],
            "disjoint sides interleave into one ordered stream"
        );
        assert_eq!(
            vec![1u8, 3, 5]
                .into_iter()
                .union(vec![2u8, 3, 4])
                .collect::<Vec<_>>(),
            vec![1, 2, 3, 4, 5],
            "a shared key is one answer, not two"
        );
        // Duplicates on BOTH sides: the run is one key, and it appears where it
        // sorts — not at the end, as `chain` plus dedup would leave it.
        assert_eq!(
            vec![2u8, 2, 2, 7]
                .into_iter()
                .union(vec![2u8, 2, 5, 5])
                .collect::<Vec<_>>(),
            vec![2, 5, 7]
        );
        assert_eq!(
            vec![1u8, 2]
                .into_iter()
                .union(Vec::<u8>::new())
                .collect::<Vec<_>>(),
            vec![1, 2],
            "half an empty pair answers the other side"
        );
        assert_eq!(
            Vec::<u8>::new()
                .into_iter()
                .union(vec![9u8])
                .collect::<Vec<_>>(),
            vec![9]
        );
        assert_eq!(
            Vec::<u8>::new()
                .into_iter()
                .union(Vec::<u8>::new())
                .collect::<Vec<_>>(),
            Vec::<u8>::new()
        );
    }

    /// The item stays a key, so N sides fold: `a ∪ b ∪ c`, then the other
    /// operators compose on top of it without a second type.
    #[test]
    fn union_folds_across_sides_and_chains_into_filters() {
        let a = vec![1u8, 4, 7];
        let b = vec![2u8, 4, 8];
        let c = vec![3u8, 8, 9];
        assert_eq!(
            a.clone()
                .union(b.clone())
                .union(c.clone())
                .collect::<Vec<_>>(),
            vec![1, 2, 3, 4, 7, 8, 9]
        );
        // (a ∪ b) ∩ c — the union's answer is still ordered input for the next filter.
        assert_eq!(
            a.clone()
                .union(b.clone())
                .intersect(c.clone())
                .collect::<Vec<_>>(),
            vec![8]
        );
        // (a ∪ b) − c
        assert_eq!(
            a.union(b).difference(c).collect::<Vec<_>>(),
            vec![1, 2, 4, 7]
        );
    }

    /// Lazy like the other two: `take(1)` pulls only as far as the first key, and
    /// an abandoned union does no work at all past it.
    #[test]
    fn union_takes_only_what_the_answer_needs() {
        struct Counted<I> {
            inner: I,
            pulled: std::rc::Rc<std::cell::Cell<usize>>,
        }
        impl<I: Iterator> Iterator for Counted<I> {
            type Item = I::Item;
            fn next(&mut self) -> Option<I::Item> {
                self.pulled.set(self.pulled.get() + 1);
                self.inner.next()
            }
        }
        let pulled = std::rc::Rc::new(std::cell::Cell::new(0));
        let left = Counted {
            inner: (10u32..1000).collect::<Vec<_>>().into_iter(),
            pulled: pulled.clone(),
        };
        let right = Counted {
            inner: (1000u32..2000).collect::<Vec<_>>().into_iter(),
            pulled: pulled.clone(),
        };
        assert_eq!(left.union(right).take(1).collect::<Vec<_>>(), vec![10]);
        assert!(
            pulled.get() <= 3,
            "a merged head pulls one key per side (plus one to prove the left run \
             ended), not 1990 (pulled {})",
            pulled.get()
        );
    }

    /// An exhausted side is not the end of the alignment: the other side's tail
    /// is one-sided answers, and `merge` promises ALL distinct keys. (Both shapes
    /// were silently truncated before `union` made them visible.)
    #[test]
    fn merge_keeps_the_tail_of_a_side_that_outlives_the_other() {
        assert_eq!(
            Vec::<u8>::new()
                .into_iter()
                .merge(vec![1u8, 2])
                .collect::<Vec<_>>(),
            vec![(None, Some(1)), (None, Some(2))],
            "an exhausted left still answers the right tail"
        );
        assert_eq!(
            vec![1u8, 2, 2, 3]
                .into_iter()
                .merge(Vec::<u8>::new())
                .collect::<Vec<_>>(),
            vec![(Some(1), None), (Some(2), None), (Some(3), None)],
            "the left tail is answered once per distinct key, not once per entry"
        );
        assert_eq!(
            vec![1u8, 1]
                .into_iter()
                .difference(Vec::<u8>::new())
                .collect::<Vec<_>>(),
            vec![1],
            "so the filters over it are set-semantic to the last key"
        );
    }

    thread_local! {
        /// Comparisons spent by [`Probe`] on this thread.
        static PROBE_CMPS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    }

    /// A key that counts its own comparisons. The gallop's claim is about how
    /// many comparisons an intersection spends, which is invisible from the
    /// answer — so the key type is the instrument. Equality is free (it is not
    /// what the merge costs); `Ord` is what both paths call.
    #[derive(Clone, Debug, PartialEq, Eq)]
    struct Probe(u32);

    impl PartialOrd for Probe {
        fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
            Some(self.cmp(other))
        }
    }

    impl Ord for Probe {
        fn cmp(&self, other: &Self) -> std::cmp::Ordering {
            PROBE_CMPS.with(|c| c.set(c.get() + 1));
            self.0.cmp(&other.0)
        }
    }

    fn probe_cmps(reset: bool) -> u64 {
        PROBE_CMPS.with(|c| {
            let seen = c.get();
            if reset {
                c.set(0);
            }
            seen
        })
    }

    /// The slice operator is the stream operator's answer, borrowed: the same keys,
    /// the same order, set semantics on both sides' duplicates.
    #[test]
    fn gallop_intersect_answers_what_the_stream_form_answers() {
        let cases: Vec<(Vec<u32>, Vec<u32>)> = vec![
            (vec![], vec![]),
            (vec![], vec![1, 2, 3]),
            (vec![1, 2, 3], vec![]),
            (vec![1, 3, 5, 7, 9], vec![2, 4, 6, 8, 10]),
            (vec![1, 2, 3], vec![1, 2, 3]),
            (vec![1, 1, 1], vec![1, 1]),
            (vec![1, 1, 2, 4, 4, 7], vec![4, 4, 4, 5, 7]),
            (vec![9], vec![0, 1, 2, 3, 4, 5, 6, 7, 8]),
            (vec![0, 1, 2, 3, 4, 5, 6, 7, 8], vec![9]),
            (
                (0..200_000u32).collect(),
                (0..200_000u32).step_by(997).collect(),
            ),
            (
                (0..200_000u32).step_by(997).collect(),
                (0..200_000u32).collect(),
            ),
        ];
        for (left, right) in cases {
            let got: Vec<u32> = gallop_intersect(&left, &right).copied().collect();
            let want: Vec<u32> = left.clone().into_iter().intersect(right.clone()).collect();
            assert_eq!(
                got, want,
                "slices disagree with the stream on {left:?} / {right:?}"
            );
        }
    }

    /// The seek is TAKEN, not just correct: on lopsided slices the gallop spends
    /// far fewer comparisons than the merge, and on balanced ones it stays within
    /// a small constant (the probe's price).
    #[test]
    fn gallop_intersect_seeks_on_lopsided_slices_and_stays_flat_on_balanced_ones() {
        let lopsided_long: Vec<Probe> = (0u32..200_000).map(Probe).collect();
        let lopsided_short: Vec<Probe> = (0u32..200_000).step_by(193).map(Probe).collect();

        probe_cmps(true);
        let galloped: Vec<u32> = gallop_intersect(&lopsided_long, &lopsided_short)
            .map(|p| p.0)
            .collect();
        let gallop_cmps = probe_cmps(true);

        let streamed: Vec<u32> = lopsided_long
            .iter()
            .intersect(lopsided_short.iter())
            .map(|p| p.0)
            .collect();
        let stream_cmps = probe_cmps(true);

        assert_eq!(galloped, streamed);
        assert!(
            gallop_cmps * 4 < stream_cmps,
            "the slice operator must seek, not walk (gallop {gallop_cmps}, merge {stream_cmps})"
        );

        // Balanced: every key of the long slice advances by one, so the gallop's
        // advantage is gone — but it must not cost a multiple of the merge.
        let left: Vec<Probe> = (0u32..20_000).map(Probe).collect();
        let right: Vec<Probe> = (0u32..20_000).step_by(2).map(Probe).collect();
        probe_cmps(true);
        let got: Vec<u32> = gallop_intersect(&left, &right).map(|p| p.0).collect();
        let balanced_gallop = probe_cmps(true);
        let want: Vec<u32> = left.intersect(right).map(|p| p.0).collect();
        let balanced_stream = probe_cmps(true);
        assert_eq!(got, want);
        assert_eq!(got.len(), 10_000);
        assert!(
            balanced_gallop <= balanced_stream * 3,
            "a balanced pair must stay near the merge's price \
             (gallop {balanced_gallop}, merge {balanced_stream})"
        );
    }

    /// Lazy and borrowed: `take(1)` answers from the front, and the items are
    /// references into the caller's slices — no clone of a key, ever.
    #[test]
    fn gallop_intersect_is_lazy_and_borrows() {
        let left: Vec<String> = ["a", "b", "c"].map(String::from).to_vec();
        let right: Vec<String> = ["b", "c", "d"].map(String::from).to_vec();
        let first: Vec<&String> = gallop_intersect(&left, &right).take(1).collect();
        assert_eq!(first, vec![&left[1]]);
        assert_eq!(
            gallop_intersect(&left, &right).collect::<Vec<_>>(),
            vec![&left[1], &left[2]],
            "the answers point into the left slice, which is what makes the \
             operator clone-free"
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
    /// exactly why the join key is the field and not the key.
    ///
    /// The shape is the two stages: `intersect` over the ordered key streams, then
    /// one fetch (here: a filter over the already-held streams, which is what an
    /// index probe per surviving key does against the store) per surviving key. The
    /// m×n cross product belongs to that second loop.
    #[test]
    fn join_on_the_scanned_field_needs_no_sort_and_pairs_in_the_caller() {
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

        // Stage 1: set algebra over the field the scans are ordered by. Nothing to
        // sort — the field segment comes off the entry, and the projection keeps the
        // scan's order.
        let names: Vec<String> = left
            .iter()
            .map(|(name, _)| name.clone())
            .intersect(right.iter().map(|(name, _)| name.clone()))
            .collect();
        assert_eq!(names, vec!["ada".to_string(), "bob".to_string()]);

        // Stage 2: one fetch per surviving key. This is where a repeated key
        // multiplies — one person's rows against that key's orders — and it is a
        // loop, not an operator.
        let mut pairs = Vec::new();
        for name in &names {
            let persons: Vec<PersonKey> = left
                .iter()
                .filter(|(n, _)| n == name)
                .map(|(_, key)| key.clone())
                .collect();
            let orders_for: Vec<OrderKey> = right
                .iter()
                .filter(|(n, _)| n == name)
                .map(|(_, key)| key.clone())
                .collect();
            for person in &persons {
                for order in &orders_for {
                    pairs.push((person.clone(), order.clone()));
                }
            }
        }
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
        let grouped: Vec<_> = group_by(rows, |r| r.0, || 0u64, |acc, (_, v)| acc + v).collect();
        assert_eq!(grouped, vec![(7, 30), (9, 30)]);
    }

    /// Laziness: answering the first group must not drain the input. Three runs of
    /// two; `pulled` is how many items the source actually handed out.
    #[test]
    fn group_by_pulls_only_the_run_it_answers() {
        let mut pulled = 0;
        let input = (0..6u64).map(|i| {
            pulled += 1;
            (i / 2, i)
        });

        let first: Vec<_> = group_by(input, |r| r.0, || 0u64, |acc, (_, v)| acc + v)
            .take(1)
            .collect();

        assert_eq!(first, vec![(0, 1)], "one group, the first run folded");
        assert!(
            pulled < 6,
            "take(1) must stop at the first run, pulled {pulled} of 6"
        );
    }
}
