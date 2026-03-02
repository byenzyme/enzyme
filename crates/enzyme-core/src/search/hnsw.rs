use anyhow::{Context, Result, bail};
use instant_distance::{Builder, HnswMap, Search};
use tracing::{debug, info};

use crate::models::Embedding;

// ---------------------------------------------------------------------------
// Point — a wrapper around a dense vector that implements instant_distance::Point
// ---------------------------------------------------------------------------

/// A dense embedding vector used as a point in the HNSW index.
#[derive(Clone, Debug)]
pub struct Point(pub Vec<f32>);

impl instant_distance::Point for Point {
    /// Cosine distance: `1.0 - cosine_similarity`.
    ///
    /// `instant-distance` treats *lower* distance as *more similar*, so we use
    /// `1 - cos_sim` which ranges from 0 (identical) to 2 (opposite).
    fn distance(&self, other: &Self) -> f32 {
        1.0 - Embedding::cosine_similarity(&self.0, &other.0)
    }
}

// ---------------------------------------------------------------------------
// SearchResult
// ---------------------------------------------------------------------------

/// The result of a nearest-neighbor search in the HNSW index.
#[derive(Debug, Clone)]
pub struct SearchResult {
    /// Composite identifier: `file_path:chunk_index`.
    pub id: String,
    /// Cosine distance (0.0 = identical, 2.0 = opposite).
    pub distance: f32,
    /// Cosine similarity (1.0 = identical, -1.0 = opposite).
    pub similarity: f32,
}

// ---------------------------------------------------------------------------
// HnswIndex
// ---------------------------------------------------------------------------

/// Wrapper around an `instant_distance::HnswMap` for embedding-based search.
///
/// Each indexed point carries a `String` identifier of the form
/// `"<file_path>:<chunk_index>"`, which can be used to look up the original
/// document chunk in the database.
pub struct HnswIndex {
    map: Option<HnswMap<Point, String>>,
}

impl HnswIndex {
    /// Create an empty, unbuilt index.
    pub fn new() -> Self {
        Self { map: None }
    }

    /// Build the HNSW index from a set of `(embedding_vector, identifier)` pairs.
    ///
    /// The `identifier` should be in the form `"file_path:chunk_index"`.
    ///
    /// # Errors
    ///
    /// Returns an error if `points` is empty.
    pub fn build(&mut self, points: Vec<(Vec<f32>, String)>) -> Result<()> {
        if points.is_empty() {
            bail!("cannot build HNSW index from an empty point set");
        }

        let n = points.len();
        let dim = points[0].0.len();

        let (raw_points, values): (Vec<Point>, Vec<String>) = points
            .into_iter()
            .map(|(vec, id)| (Point(vec), id))
            .unzip();

        info!(
            num_points = n,
            dimension = dim,
            "building HNSW index"
        );

        let map = Builder::default().build(raw_points, values);

        debug!(num_points = n, "HNSW index built successfully");
        self.map = Some(map);
        Ok(())
    }

    /// Find the `k` nearest neighbors to `query`.
    ///
    /// Results are ordered by ascending distance (most similar first).
    ///
    /// # Errors
    ///
    /// Returns an error if the index has not been built yet.
    pub fn search(&self, query: &[f32], k: usize) -> Result<Vec<SearchResult>> {
        let map = self
            .map
            .as_ref()
            .context("HNSW index has not been built yet")?;

        if k == 0 {
            return Ok(Vec::new());
        }

        let query_point = Point(query.to_vec());
        let mut search = Search::default();

        let results: Vec<SearchResult> = map
            .search(&query_point, &mut search)
            .take(k)
            .map(|item| SearchResult {
                id: item.value.clone(),
                distance: item.distance,
                similarity: 1.0 - item.distance,
            })
            .collect();

        debug!(
            k,
            returned = results.len(),
            "HNSW search completed"
        );

        Ok(results)
    }

    /// Returns `true` if the index has been built and contains points.
    pub fn is_built(&self) -> bool {
        self.map.is_some()
    }
}

impl Default for HnswIndex {
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

    /// Helper: create a unit vector along the given axis in `dim` dimensions.
    fn axis_vector(dim: usize, axis: usize) -> Vec<f32> {
        let mut v = vec![0.0; dim];
        v[axis] = 1.0;
        v
    }

    #[test]
    fn test_build_and_search_returns_nearest() {
        let mut index = HnswIndex::new();

        // Three orthogonal unit vectors in 3-D.
        let points = vec![
            (axis_vector(3, 0), "a.md:0".to_string()), // [1,0,0]
            (axis_vector(3, 1), "b.md:0".to_string()), // [0,1,0]
            (axis_vector(3, 2), "c.md:0".to_string()), // [0,0,1]
        ];

        index.build(points).expect("build should succeed");

        // Query very close to [1,0,0] — should return "a.md:0" first.
        let query = vec![0.95, 0.05, 0.0];
        let results = index.search(&query, 2).expect("search should succeed");

        assert!(!results.is_empty(), "should return at least one result");
        assert_eq!(results[0].id, "a.md:0");
        assert!(
            results[0].similarity > 0.9,
            "similarity to the nearest point should be high, got {}",
            results[0].similarity
        );
    }

    #[test]
    fn test_search_returns_correct_order() {
        let mut index = HnswIndex::new();

        let points = vec![
            (vec![1.0, 0.0, 0.0], "x.md:0".to_string()),
            (vec![0.0, 1.0, 0.0], "y.md:0".to_string()),
            (vec![0.0, 0.0, 1.0], "z.md:0".to_string()),
        ];
        index.build(points).unwrap();

        // Query exactly along x-axis.
        let results = index.search(&[1.0, 0.0, 0.0], 3).unwrap();

        assert_eq!(results.len(), 3);
        // First result should be x.md:0 with distance ~0.
        assert_eq!(results[0].id, "x.md:0");
        assert!(results[0].distance < 1e-5);
        assert!((results[0].similarity - 1.0).abs() < 1e-5);

        // Remaining results should have distance ~1 (orthogonal).
        for r in &results[1..] {
            assert!((r.distance - 1.0).abs() < 1e-5);
        }
    }

    #[test]
    fn test_search_k_zero_returns_empty() {
        let mut index = HnswIndex::new();
        let points = vec![(vec![1.0, 0.0], "doc:0".to_string())];
        index.build(points).unwrap();

        let results = index.search(&[1.0, 0.0], 0).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn test_search_k_larger_than_index_size() {
        let mut index = HnswIndex::new();
        let points = vec![
            (vec![1.0, 0.0], "a:0".to_string()),
            (vec![0.0, 1.0], "b:0".to_string()),
        ];
        index.build(points).unwrap();

        // Ask for 100 results when only 2 exist.
        let results = index.search(&[1.0, 0.0], 100).unwrap();
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn test_search_on_unbuilt_index_returns_error() {
        let index = HnswIndex::new();
        let err = index.search(&[1.0, 0.0, 0.0], 5);
        assert!(err.is_err());
    }

    #[test]
    fn test_build_empty_points_returns_error() {
        let mut index = HnswIndex::new();
        let err = index.build(Vec::new());
        assert!(err.is_err());
    }

    #[test]
    fn test_is_built() {
        let mut index = HnswIndex::new();
        assert!(!index.is_built());

        index
            .build(vec![(vec![1.0, 0.0], "doc:0".to_string())])
            .unwrap();
        assert!(index.is_built());
    }

    #[test]
    fn test_distance_and_similarity_are_consistent() {
        let mut index = HnswIndex::new();
        let points = vec![
            (vec![1.0, 0.0], "a:0".to_string()),
            (vec![0.7071, 0.7071], "b:0".to_string()),
        ];
        index.build(points).unwrap();

        let results = index.search(&[1.0, 0.0], 2).unwrap();
        for r in &results {
            let expected_sim = 1.0 - r.distance;
            assert!(
                (r.similarity - expected_sim).abs() < 1e-5,
                "similarity should equal 1 - distance"
            );
        }
    }

    #[test]
    fn test_cosine_distance_for_identical_points() {
        let a = Point(vec![0.5, 0.5, 0.5]);
        let b = a.clone();
        let dist = instant_distance::Point::distance(&a, &b);
        assert!(
            dist.abs() < 1e-5,
            "distance between identical points should be ~0, got {dist}",
        );
    }

    #[test]
    fn test_cosine_distance_for_opposite_points() {
        let a = Point(vec![1.0, 0.0]);
        let b = Point(vec![-1.0, 0.0]);
        let dist = instant_distance::Point::distance(&a, &b);
        assert!(
            (dist - 2.0).abs() < 1e-5,
            "distance between opposite points should be ~2, got {dist}",
        );
    }

    #[test]
    fn test_rebuild_replaces_old_index() {
        let mut index = HnswIndex::new();

        // First build.
        index
            .build(vec![(vec![1.0, 0.0, 0.0], "old:0".to_string())])
            .unwrap();
        let results = index.search(&[1.0, 0.0, 0.0], 1).unwrap();
        assert_eq!(results[0].id, "old:0");

        // Rebuild with different data.
        index
            .build(vec![(vec![0.0, 1.0, 0.0], "new:0".to_string())])
            .unwrap();
        let results = index.search(&[0.0, 1.0, 0.0], 1).unwrap();
        assert_eq!(results[0].id, "new:0");
    }

    #[test]
    fn test_higher_dimensional_vectors() {
        let mut index = HnswIndex::new();
        let dim = 384; // same as EMBEDDING_DIMENSION

        let mut points = Vec::new();
        for i in 0..10 {
            let mut v = vec![0.0f32; dim];
            v[i] = 1.0;
            points.push((v, format!("doc{i}:0")));
        }

        index.build(points).unwrap();

        // Query along axis 3.
        let mut query = vec![0.0f32; dim];
        query[3] = 1.0;

        let results = index.search(&query, 1).unwrap();
        assert_eq!(results[0].id, "doc3:0");
        assert!(results[0].similarity > 0.99);
    }

    #[test]
    fn test_default_impl() {
        let index = HnswIndex::default();
        assert!(!index.is_built());
    }
}
