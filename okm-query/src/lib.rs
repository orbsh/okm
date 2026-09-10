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
pub fn merge_join<K: Ord + Clone, L: Clone, R: Clone>(
    left: impl IntoIterator<Item = (K, L)>,
    right: impl IntoIterator<Item = (K, R)>,
) -> Vec<(L, R)> {
    let mut out = Vec::new();
    let mut li = left.into_iter().peekable();
    let mut ri = right.into_iter().peekable();
    loop {
        match (li.peek(), ri.peek()) {
            (Some((lk, _)), Some((rk, _))) => match lk.cmp(rk) {
                std::cmp::Ordering::Less => {
                    li.next();
                }
                std::cmp::Ordering::Greater => {
                    ri.next();
                }
                std::cmp::Ordering::Equal => {
                    // Drain the run of equal keys on both sides — a
                    // nested-loop over the equal runs, matching SQL join
                    // semantics when keys duplicate.
                    let lk = (*lk).clone();
                    let mut lrun = Vec::new();
                    while let Some((k, _)) = li.peek() {
                        if k == &lk {
                            lrun.push(li.next().unwrap().1);
                        } else {
                            break;
                        }
                    }
                    let mut rrun = Vec::new();
                    while let Some((k, _)) = ri.peek() {
                        if k == &lk {
                            rrun.push(ri.next().unwrap().1);
                        } else {
                            break;
                        }
                    }
                    for l in &lrun {
                        for r in &rrun {
                            out.push((l.clone(), r.clone()));
                        }
                    }
                }
            },
            _ => break,
        }
    }
    out
}

/// Hash join: build on the (smaller) right stream, probe with the left.
/// No ordering requirement on either side — the complement to
/// [`merge_join`] when one side is unsorted or much smaller.
pub fn hash_join<K: Ord + std::hash::Hash + Clone, L, R>(
    left: impl IntoIterator<Item = (K, L)>,
    right: impl IntoIterator<Item = (K, R)>,
) -> Vec<(L, R)>
where
    L: Clone,
    R: Clone,
{
    let mut table: std::collections::HashMap<K, Vec<R>> = std::collections::HashMap::new();
    for (k, r) in right {
        table.entry(k).or_default().push(r);
    }
    let mut out = Vec::new();
    for (k, l) in left {
        if let Some(rs) = table.get(&k) {
            for r in rs {
                out.push((l.clone(), r.clone()));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_join_matches_on_sorted_keys() {
        let left = vec![(1u64, "a1"), (2, "a2"), (4, "a4")];
        let right = vec![(2u64, "b2"), (3, "b3"), (4, "b4")];
        let joined = merge_join(left, right);
        assert_eq!(joined, vec![("a2", "b2"), ("a4", "b4")]);
    }

    #[test]
    fn merge_join_runs_duplicate_keys() {
        let left = vec![(1u64, "a"), (1, "b")];
        let right = vec![(1u64, "x"), (1, "y")];
        let mut joined = merge_join(left, right);
        joined.sort();
        assert_eq!(joined.len(), 4);
    }

    #[test]
    fn hash_join_needs_no_order() {
        let left = vec![(9u64, "a9"), (2, "a2")];
        let right = vec![(2u64, "b2")];
        assert_eq!(hash_join(left, right), vec![("a2", "b2")]);
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
