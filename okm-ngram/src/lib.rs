//! okm-ngram — n-gram full-text search over okm's multi-value function
//! indexes.
//!
//! The whole capability is one tokenizer: character n-grams of a
//! configurable width `n`. Each n-gram is a posting entry via
//! `func(ngrams)` returning `Vec<String>` — one row fans out to N
//! entries, `scan::<I>(gram)` is the posting list. BM25 rides on top of
//! the recall (term frequency = how many entries share the token).
//!
//! That is deliberately the whole feature. anything beyond n-gram +
//! BM25 (linguistic tokenization, fuzzy match, incremental indexing)
//! is either the caller's own tokenizer against the same
//! multi-value-func contract, or a dedicated engine (Tantivy et al.)
//! next to okm — not this crate growing a second opinion.

/// Character n-grams of width `n`. Panics on `n == 0` — a zero-width
/// gram is a modeling error, not an empty result.
pub fn ngrams(text: &str, n: usize) -> Vec<String> {
    assert!(n >= 1, "ngram width must be >= 1");
    let chars: Vec<char> = text.chars().collect();
    if chars.len() < n {
        return vec![text.to_string()];
    }
    chars
        .windows(n)
        .map(|w| w.iter().collect())
        .collect()
}

/// BM25 (Okapi) scoring over recalled candidates. The caller supplies,
/// per document: its n-gram count (doc length), the corpus average, and
/// per candidate-gram the term frequency plus posting-list size (doc
/// freq — one scan of the gram's entry range). `k1`/`b` are the
/// standard constants.
pub fn bm25_score(
    term_freq: f64,
    doc_len: f64,
    avg_doc_len: f64,
    doc_freq: f64,
    total_docs: f64,
) -> f64 {
    const K1: f64 = 1.2;
    const B: f64 = 0.75;
    let idf = ((total_docs - doc_freq + 0.5) / (doc_freq + 0.5) + 1.0).ln();
    let tf_norm = term_freq * (K1 + 1.0)
        / (term_freq + K1 * (1.0 - B + B * doc_len / avg_doc_len));
    idf * tf_norm
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bigrams_of_a_ascii_word() {
        assert_eq!(ngrams("abcd", 2), ["ab", "bc", "cd"].map(String::from));
    }

    #[test]
    fn ngram_is_char_not_byte() {
        // CJK: 2 chars = exactly 2 bigrams, not byte-window noise.
        assert_eq!(ngrams("数据", 2), ["数据"].map(String::from));
        assert_eq!(ngrams("数据库", 2).len(), 2);
    }

    #[test]
    fn short_text_returns_whole_text() {
        assert_eq!(ngrams("ab", 3), vec!["ab".to_string()]);
    }

    #[test]
    #[should_panic]
    fn zero_width_panics() {
        ngrams("ab", 0);
    }

    #[test]
    fn bm25_ranks_common_grams_lower() {
        let rare = bm25_score(1.0, 10.0, 10.0, 1.0, 100.0);
        let common = bm25_score(1.0, 10.0, 10.0, 50.0, 100.0);
        assert!(rare > common);
    }

    #[test]
    fn bm25_saturates_term_frequency() {
        let t1 = bm25_score(1.0, 10.0, 10.0, 1.0, 100.0);
        let t10 = bm25_score(10.0, 10.0, 10.0, 1.0, 100.0);
        let t100 = bm25_score(100.0, 10.0, 10.0, 1.0, 100.0);
        assert!(t10 > t1 && t100 > t10);
        assert!(t100 - t10 < t10 - t1);
    }
}
