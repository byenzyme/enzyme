use std::collections::HashMap;

use tracing::debug;

// ---------------------------------------------------------------------------
// BM25 parameters
// ---------------------------------------------------------------------------

/// Term-frequency saturation parameter (standard default).
const K1: f32 = 1.2;
/// Length normalization parameter (standard default).
const B: f32 = 0.75;

// ---------------------------------------------------------------------------
// Bm25Result
// ---------------------------------------------------------------------------

/// A single result from a BM25 search.
#[derive(Debug, Clone)]
pub struct Bm25Result {
    /// The document identifier.
    pub doc_id: String,
    /// BM25 relevance score (higher is more relevant).
    pub score: f32,
}

// ---------------------------------------------------------------------------
// Bm25Index
// ---------------------------------------------------------------------------

/// A simple in-memory BM25 index for lexical pre-filtering.
///
/// Usage:
/// 1. Call [`add_document`] for each document.
/// 2. Call [`build`] once all documents have been added.
/// 3. Call [`search`] to retrieve the top-k documents for a query.
pub struct Bm25Index {
    /// Inverted index: term -> vec of (doc_internal_index, term_frequency).
    index: HashMap<String, Vec<(usize, f32)>>,
    /// Per-document token count.
    doc_lengths: Vec<f32>,
    /// Average document length (computed by [`build`]).
    avg_dl: f32,
    /// Total number of documents.
    n_docs: usize,
    /// Document identifiers (external IDs), indexed by internal doc index.
    doc_ids: Vec<String>,
    /// Whether [`build`] has been called.
    finalized: bool,
}

impl Bm25Index {
    /// Create an empty BM25 index.
    pub fn new() -> Self {
        Self {
            index: HashMap::new(),
            doc_lengths: Vec::new(),
            avg_dl: 0.0,
            n_docs: 0,
            doc_ids: Vec::new(),
            finalized: false,
        }
    }

    /// Add a document to the index.
    ///
    /// Documents can only be added before [`build`] is called.
    ///
    /// # Panics
    ///
    /// Panics if called after [`build`].
    pub fn add_document(&mut self, doc_id: String, text: &str) {
        assert!(
            !self.finalized,
            "cannot add documents after the index has been built"
        );

        let doc_idx = self.n_docs;
        self.doc_ids.push(doc_id);
        self.n_docs += 1;

        let tokens = Self::tokenize(text);
        let doc_len = tokens.len() as f32;
        self.doc_lengths.push(doc_len);

        // Count term frequencies within this document.
        let mut tf_map: HashMap<String, f32> = HashMap::new();
        for token in &tokens {
            *tf_map.entry(token.clone()).or_insert(0.0) += 1.0;
        }

        // Populate the inverted index.
        for (term, tf) in tf_map {
            self.index
                .entry(term)
                .or_insert_with(Vec::new)
                .push((doc_idx, tf));
        }
    }

    /// Finalize the index after all documents have been added.
    ///
    /// This precomputes the average document length used by the BM25 formula.
    pub fn build(&mut self) {
        if self.n_docs > 0 {
            let total_len: f32 = self.doc_lengths.iter().sum();
            self.avg_dl = total_len / self.n_docs as f32;
        }
        self.finalized = true;
        debug!(
            n_docs = self.n_docs,
            avg_dl = self.avg_dl,
            unique_terms = self.index.len(),
            "BM25 index built"
        );
    }

    /// Search for documents matching `query`, returning the top `k` results
    /// sorted by descending BM25 score.
    ///
    /// If the index has not been built yet, returns an empty vector.
    pub fn search(&self, query: &str, k: usize) -> Vec<Bm25Result> {
        if !self.finalized || self.n_docs == 0 || k == 0 {
            return Vec::new();
        }

        let query_tokens = Self::tokenize(query);
        if query_tokens.is_empty() {
            return Vec::new();
        }

        // Accumulate BM25 scores per document.
        let mut scores = vec![0.0f32; self.n_docs];

        for token in &query_tokens {
            if let Some(postings) = self.index.get(token) {
                let df = postings.len() as f32;
                let idf = Self::idf(self.n_docs as f32, df);

                for &(doc_idx, tf) in postings {
                    let dl = self.doc_lengths[doc_idx];
                    let tf_component = Self::tf_score(tf, dl, self.avg_dl);
                    scores[doc_idx] += idf * tf_component;
                }
            }
        }

        // Collect documents with non-zero scores and sort by descending score.
        let mut results: Vec<Bm25Result> = scores
            .iter()
            .copied()
            .enumerate()
            .filter(|(_, score)| *score > 0.0)
            .map(|(idx, score)| Bm25Result {
                doc_id: self.doc_ids[idx].clone(),
                score,
            })
            .collect();

        results.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        results.truncate(k);

        debug!(
            query_terms = query_tokens.len(),
            results = results.len(),
            "BM25 search completed"
        );

        results
    }

    /// Returns the total number of documents in the index.
    pub fn num_documents(&self) -> usize {
        self.n_docs
    }

    // -----------------------------------------------------------------------
    // Private helpers
    // -----------------------------------------------------------------------

    /// BM25 inverse document frequency.
    ///
    /// `idf(t) = ln((N - df(t) + 0.5) / (df(t) + 0.5) + 1)`
    ///
    /// We add 1 inside the log to ensure IDF is always non-negative, even when
    /// a term appears in every document.
    fn idf(n: f32, df: f32) -> f32 {
        ((n - df + 0.5) / (df + 0.5) + 1.0).ln()
    }

    /// BM25 term-frequency component.
    ///
    /// `tf_score = (tf * (k1 + 1)) / (tf + k1 * (1 - b + b * dl / avgdl))`
    fn tf_score(tf: f32, dl: f32, avg_dl: f32) -> f32 {
        let numerator = tf * (K1 + 1.0);
        let denominator = tf + K1 * (1.0 - B + B * dl / avg_dl);
        numerator / denominator
    }

    /// Tokenize text into lowercase terms, splitting on whitespace and
    /// punctuation. Single-character tokens are dropped to reduce noise.
    fn tokenize(text: &str) -> Vec<String> {
        text.to_lowercase()
            .split(|c: char| !c.is_alphanumeric())
            .filter(|s| s.len() > 1)
            .map(String::from)
            .collect()
    }
}

impl Default for Bm25Index {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: build a simple index from `(doc_id, text)` pairs.
    fn build_index(docs: &[(&str, &str)]) -> Bm25Index {
        let mut index = Bm25Index::new();
        for (id, text) in docs {
            index.add_document(id.to_string(), text);
        }
        index.build();
        index
    }

    #[test]
    fn test_basic_ranking() {
        let index = build_index(&[
            ("doc1", "the quick brown fox jumps over the lazy dog"),
            ("doc2", "the cat sat on the mat"),
            ("doc3", "fox fox fox fox fox"), // high TF for "fox"
        ]);

        let results = index.search("fox", 10);
        assert!(!results.is_empty());

        // "doc3" has the highest term frequency for "fox" and should rank first.
        assert_eq!(results[0].doc_id, "doc3");
        // "doc1" also contains "fox".
        assert!(results.iter().any(|r| r.doc_id == "doc1"));
        // "doc2" does not contain "fox" and should not appear.
        assert!(!results.iter().any(|r| r.doc_id == "doc2"));
    }

    #[test]
    fn test_document_with_query_term_scores_higher() {
        let index = build_index(&[
            ("relevant", "rust programming language systems"),
            ("irrelevant", "the weather is nice today"),
        ]);

        let results = index.search("rust programming", 10);
        assert!(!results.is_empty());
        assert_eq!(results[0].doc_id, "relevant");

        // "irrelevant" should not appear since it has no matching terms.
        assert!(!results.iter().any(|r| r.doc_id == "irrelevant"));
    }

    #[test]
    fn test_idf_weighting() {
        // "the" appears in all 3 docs (low IDF), "quantum" only in doc3 (high IDF).
        let index = build_index(&[
            ("doc1", "the cat the dog the bird"),
            ("doc2", "the sun the moon the stars"),
            ("doc3", "the quantum mechanics lecture"),
        ]);

        let results = index.search("quantum", 10);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].doc_id, "doc3");
        let quantum_score = results[0].score;

        // Searching for "the" should return all docs but with lower per-term IDF.
        let results_the = index.search("the", 10);
        assert_eq!(results_the.len(), 3);
        // The IDF for "the" (appears in 3/3 docs) should make scores lower than
        // the IDF for "quantum" (appears in 1/3 docs), given similar TF.
        // doc3 has "the" once, same TF as "quantum"; the score should be lower.
        let doc3_the_score = results_the
            .iter()
            .find(|r| r.doc_id == "doc3")
            .unwrap()
            .score;
        assert!(
            quantum_score > doc3_the_score,
            "rare term 'quantum' (score={quantum_score}) should score higher than common term 'the' (score={doc3_the_score})"
        );
    }

    #[test]
    fn test_empty_query_returns_empty() {
        let index = build_index(&[("doc1", "hello world")]);
        let results = index.search("", 10);
        assert!(results.is_empty());
    }

    #[test]
    fn test_query_with_no_matching_terms() {
        let index = build_index(&[("doc1", "hello world")]);
        let results = index.search("quantum entanglement", 10);
        assert!(results.is_empty());
    }

    #[test]
    fn test_empty_index_returns_empty() {
        let mut index = Bm25Index::new();
        index.build();
        let results = index.search("anything", 10);
        assert!(results.is_empty());
    }

    #[test]
    fn test_search_before_build_returns_empty() {
        let mut index = Bm25Index::new();
        index.add_document("doc1".to_string(), "hello world");
        // Intentionally not calling build().
        let results = index.search("hello", 10);
        assert!(results.is_empty());
    }

    #[test]
    fn test_k_limits_results() {
        let index = build_index(&[("a", "word word word"), ("b", "word word"), ("c", "word")]);

        let results = index.search("word", 2);
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn test_k_zero_returns_empty() {
        let index = build_index(&[("a", "hello world")]);
        let results = index.search("hello", 0);
        assert!(results.is_empty());
    }

    #[test]
    fn test_tokenize_lowercases() {
        let tokens = Bm25Index::tokenize("Hello WORLD Rust");
        assert_eq!(tokens, vec!["hello", "world", "rust"]);
    }

    #[test]
    fn test_tokenize_splits_on_punctuation() {
        let tokens = Bm25Index::tokenize("hello-world, foo.bar! baz_qux");
        assert!(tokens.contains(&"hello".to_string()));
        assert!(tokens.contains(&"world".to_string()));
        assert!(tokens.contains(&"foo".to_string()));
        assert!(tokens.contains(&"bar".to_string()));
        assert!(tokens.contains(&"baz".to_string()));
        assert!(tokens.contains(&"qux".to_string()));
    }

    #[test]
    fn test_tokenize_drops_single_chars() {
        let tokens = Bm25Index::tokenize("I am a test");
        // "I", "a" are single chars and should be dropped.
        assert!(!tokens.contains(&"i".to_string()));
        assert!(!tokens.contains(&"a".to_string()));
        assert!(tokens.contains(&"am".to_string()));
        assert!(tokens.contains(&"test".to_string()));
    }

    #[test]
    fn test_multi_term_query_combines_scores() {
        let index = build_index(&[
            ("doc1", "rust systems programming"),
            ("doc2", "rust web framework"),
            ("doc3", "python machine learning"),
        ]);

        // "rust" matches doc1 and doc2; "systems" only matches doc1.
        let results = index.search("rust systems", 10);
        assert!(!results.is_empty());
        // doc1 should rank highest because it matches both terms.
        assert_eq!(results[0].doc_id, "doc1");
    }

    #[test]
    fn test_scores_are_positive() {
        let index = build_index(&[
            ("a", "hello world"),
            ("b", "hello there"),
            ("c", "hello everyone"),
        ]);

        let results = index.search("hello world", 10);
        for r in &results {
            assert!(r.score > 0.0, "scores should be positive, got {}", r.score);
        }
    }

    #[test]
    fn test_document_length_normalization() {
        // "rust" appears once in both docs, but doc2 is much longer.
        // BM25's length normalization should penalize the longer doc slightly.
        let index = build_index(&[
            ("short", "rust"),
            (
                "long",
                "rust is one of many programming languages that exist in the world today",
            ),
        ]);

        let results = index.search("rust", 10);
        assert_eq!(results.len(), 2);
        // The shorter document should score higher due to length normalization.
        assert_eq!(results[0].doc_id, "short");
        assert!(results[0].score > results[1].score);
    }

    #[test]
    fn test_num_documents() {
        let mut index = Bm25Index::new();
        assert_eq!(index.num_documents(), 0);

        index.add_document("a".to_string(), "hello");
        assert_eq!(index.num_documents(), 1);

        index.add_document("b".to_string(), "world");
        assert_eq!(index.num_documents(), 2);
    }

    #[test]
    fn test_default_impl() {
        let index = Bm25Index::default();
        assert_eq!(index.num_documents(), 0);
    }

    #[test]
    fn test_idf_formula() {
        // With N=10, df=1: idf = ln((10-1+0.5)/(1+0.5) + 1)
        let idf_rare = Bm25Index::idf(10.0, 1.0);
        // With N=10, df=9: idf = ln((10-9+0.5)/(9+0.5) + 1)
        let idf_common = Bm25Index::idf(10.0, 9.0);

        assert!(
            idf_rare > idf_common,
            "rare terms should have higher IDF: rare={idf_rare}, common={idf_common}"
        );
        // IDF should always be non-negative with our formula.
        assert!(idf_rare > 0.0);
        assert!(idf_common > 0.0);
    }

    #[test]
    fn test_tf_saturation() {
        // TF score should saturate — increasing TF yields diminishing returns.
        let score_tf1 = Bm25Index::tf_score(1.0, 10.0, 10.0);
        let score_tf10 = Bm25Index::tf_score(10.0, 10.0, 10.0);
        let score_tf100 = Bm25Index::tf_score(100.0, 10.0, 10.0);

        assert!(score_tf10 > score_tf1);
        assert!(score_tf100 > score_tf10);
        // The gap should shrink (diminishing returns).
        let delta1 = score_tf10 - score_tf1;
        let delta2 = score_tf100 - score_tf10;
        assert!(
            delta1 > delta2,
            "TF should show diminishing returns: delta(1->10)={delta1}, delta(10->100)={delta2}"
        );
    }

    #[test]
    #[should_panic(expected = "cannot add documents after")]
    fn test_add_after_build_panics() {
        let mut index = Bm25Index::new();
        index.add_document("a".to_string(), "hello");
        index.build();
        index.add_document("b".to_string(), "world"); // should panic
    }

    #[test]
    fn test_duplicate_doc_ids_are_independent() {
        // Adding two documents with the same doc_id should treat them as
        // separate documents (the index doesn't deduplicate).
        let index = build_index(&[("doc", "hello"), ("doc", "world")]);
        assert_eq!(index.num_documents(), 2);

        let results = index.search("hello", 10);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].doc_id, "doc");
    }
}
