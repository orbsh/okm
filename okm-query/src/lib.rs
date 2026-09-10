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
