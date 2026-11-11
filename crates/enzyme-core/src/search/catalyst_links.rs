//! Catalyst links computed on demand from stored vectors.
//!
//! A link is the cosine between a catalyst vector and a chunk vector. Nothing
//! about links is stored: catalyst vectors live in `catalyst_embeddings_packed`
//! and chunk vectors in `chunk_embeddings`, and each reader scores only what it
//! needs. Annotating search results compares every current catalyst with the
//! chunks of a handful of notes; catalyst routing and the explore pool rank
//! every chunk for a few catalysts.

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use rayon::prelude::*;

use crate::db::Database;
use crate::embedding::quantize;
use crate::models::{EMBEDDING_DIMENSION, Embedding};

/// Chunks kept per catalyst when a caller asks for a catalyst's best matches
/// (catalyst routing, the explore pool).
pub const CATALYST_TOP_K: usize = 50;

/// A chunk and its cosine to one catalyst.
#[derive(Debug, Clone, PartialEq)]
pub struct ChunkLink {
    pub file_path: String,
    pub chunk_index: i64,
    pub similarity: f64,
}

/// Chunk vectors held flat for scoring: one row of `dim` components per chunk,
/// with each row's norm computed once.
pub struct ChunkVectors {
    keys: Vec<(String, i64)>,
    dim: usize,
    data: Vec<f32>,
    norms: Vec<f32>,
}

impl ChunkVectors {
    /// Every stored chunk vector in `db`, in storage order.
    ///
    /// Rows are read as raw blobs and decoded in parallel afterwards; the
    /// database scan is the serial part.
    pub fn load_all(db: &Database) -> Result<Self> {
        let count = db.count_chunk_embeddings()?;
        let mut keys: Vec<(String, i64)> = Vec::with_capacity(count);
        let mut blobs: Vec<u8> = Vec::with_capacity(count * (EMBEDDING_DIMENSION + 4));
        let mut ranges: Vec<(usize, usize)> = Vec::with_capacity(count);
        db.scan_chunk_embedding_blobs(|path, index, blob| {
            ranges.push((blobs.len(), blob.len()));
            blobs.extend_from_slice(blob);
            keys.push((path, index));
        })?;
        let dim = ranges
            .first()
            .map(|(start, len)| decode(&blobs[*start..start + len]).len())
            .unwrap_or(0);
        let mut data = vec![0.0f32; keys.len() * dim];
        let valid: Vec<bool> = if dim == 0 {
            Vec::new()
        } else {
            data.par_chunks_mut(dim)
                .zip(ranges.par_iter())
                .map(|(row, (start, len))| decode_into(&blobs[*start..start + len], row))
                .collect()
        };
        let mut vectors = Self {
            keys,
            dim,
            data,
            norms: Vec::new(),
        };
        // A vector of another width (a stale row from a different backend)
        // cannot be compared; drop it rather than misalign the flat buffer.
        if valid.iter().any(|ok| !ok) {
            vectors.retain_rows(&valid);
        }
        vectors.norms = vectors
            .data
            .par_chunks(dim.max(1))
            .map(|row| dot(row, row).sqrt())
            .collect();
        Ok(vectors)
    }

    fn retain_rows(&mut self, keep: &[bool]) {
        let dim = self.dim;
        let mut write = 0;
        for read in 0..self.keys.len() {
            if keep[read] {
                self.keys.swap(write, read);
                self.data
                    .copy_within(read * dim..(read + 1) * dim, write * dim);
                write += 1;
            }
        }
        self.keys.truncate(write);
        self.data.truncate(write * dim);
    }

    /// The rows of the given notes only, as their own set.
    pub fn for_paths(&self, paths: &[String]) -> Self {
        let wanted: HashSet<&str> = paths.iter().map(String::as_str).collect();
        let mut subset = Self {
            keys: Vec::new(),
            dim: self.dim,
            data: Vec::new(),
            norms: Vec::new(),
        };
        for (i, key) in self.keys.iter().enumerate() {
            if wanted.contains(key.0.as_str()) {
                subset.keys.push(key.clone());
                subset.data.extend_from_slice(self.row(i));
                subset.norms.push(self.norms[i]);
            }
        }
        subset
    }

    /// Each note's best chunk for `query`: note path -> (chunk index,
    /// cosine). Uses [`Embedding::cosine_similarity`], the dense side of
    /// hybrid search, so scores match it exactly; on equal scores the chunk
    /// stored first wins.
    pub fn best_chunk_per_note(&self, query: &[f32]) -> HashMap<String, (i64, f64)> {
        let sims: Vec<f64> = (0..self.len())
            .into_par_iter()
            .map(|i| Embedding::cosine_similarity(query, self.row(i)) as f64)
            .collect();
        let mut hits: HashMap<String, (i64, f64)> = HashMap::new();
        for ((path, chunk_index), sim) in self.keys.iter().zip(sims) {
            let slot = hits.entry(path.clone()).or_insert((*chunk_index, f64::MIN));
            if sim > slot.1 {
                *slot = (*chunk_index, sim);
            }
        }
        hits
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    fn row(&self, i: usize) -> &[f32] {
        &self.data[i * self.dim..(i + 1) * self.dim]
    }

    /// Cosine of `query` against every chunk, in load order.
    fn cosines(&self, query: &[f32]) -> Vec<f32> {
        if query.len() != self.dim {
            return vec![0.0; self.len()];
        }
        let query_norm = dot(query, query).sqrt();
        (0..self.len())
            .map(|i| {
                let denom = query_norm * self.norms[i];
                if denom == 0.0 {
                    0.0
                } else {
                    dot(query, self.row(i)) / denom
                }
            })
            .collect()
    }

    /// The `k` chunks closest to each catalyst, best first; ties break by path
    /// then chunk index so results are deterministic.
    pub fn top_chunks(
        &self,
        catalysts: &[(&str, &[f32])],
        k: usize,
    ) -> HashMap<String, Vec<ChunkLink>> {
        catalysts
            .par_iter()
            .map(|(id, vector)| {
                let scores = self.cosines(vector);
                let mut order: Vec<usize> = (0..scores.len()).collect();
                let cmp = |a: &usize, b: &usize| {
                    scores[*b]
                        .total_cmp(&scores[*a])
                        .then_with(|| self.keys[*a].cmp(&self.keys[*b]))
                };
                if order.len() > k && k > 0 {
                    order.select_nth_unstable_by(k - 1, cmp);
                }
                order.truncate(k);
                order.sort_by(cmp);
                let links = order
                    .into_iter()
                    .map(|i| ChunkLink {
                        file_path: self.keys[i].0.clone(),
                        chunk_index: self.keys[i].1,
                        similarity: scores[i] as f64,
                    })
                    .collect();
                (id.to_string(), links)
            })
            .collect()
    }

    /// For each note, the best chunk cosine to each catalyst, in catalyst
    /// order. Notes without stored vectors are absent.
    pub fn note_links(&self, catalysts: &[(&str, &[f32])]) -> HashMap<String, Vec<f64>> {
        let per_catalyst: Vec<Vec<f32>> = catalysts
            .par_iter()
            .map(|(_, vector)| self.cosines(vector))
            .collect();
        let mut links: HashMap<String, Vec<f64>> = HashMap::new();
        for (row, (path, _)) in self.keys.iter().enumerate() {
            let best = links
                .entry(path.clone())
                .or_insert_with(|| vec![f64::MIN; catalysts.len()]);
            for (c, scores) in per_catalyst.iter().enumerate() {
                best[c] = best[c].max(scores[row] as f64);
            }
        }
        links
    }

    /// Each catalyst's `k`-th best chunk cosine: the score a chunk needs to be
    /// among that catalyst's `k` closest chunks. With `k` or fewer chunks
    /// every chunk qualifies, so the floor is `f64::MIN`.
    pub fn top_k_floors(&self, catalysts: &[&[f32]], k: usize) -> Vec<f64> {
        catalysts
            .par_iter()
            .map(|vector| {
                if k == 0 || self.len() <= k {
                    return f64::MIN;
                }
                let mut scores = self.cosines(vector);
                let (_, kth, _) = scores.select_nth_unstable_by(k - 1, |a, b| b.total_cmp(a));
                *kth as f64
            })
            .collect()
    }
}

/// Catalysts considered per note, best fit first, before a note is left
/// without a link. Bounds the corpus scans a note no catalyst ranks highly
/// can cost; across real queries every annotated note was settled well
/// within this.
const MAX_CANDIDATES_PER_NOTE: usize = 64;
/// Catalyst floors scored per note in each round.
const CANDIDATES_PER_ROUND: usize = 8;

/// Pick, for each note in `paths`, the catalyst that best connects it to a
/// query: the highest `query similarity + link similarity` among catalysts
/// that rank the note within their [`CATALYST_TOP_K`] closest chunks across
/// the whole corpus. Returns note path -> position in `catalysts`.
///
/// Requiring a note to stand out for the catalyst keeps broad catalysts,
/// which sit close to everything, from claiming every result. Only the floors
/// of catalysts that could win are scored, best fit first, so a query scans
/// the corpus for a few dozen catalysts rather than all of them.
pub fn best_linked_catalysts(
    corpus: &ChunkVectors,
    catalysts: &[(&str, &[f32])],
    query_sims: &[f64],
    paths: &[String],
) -> HashMap<String, usize> {
    let links = corpus.for_paths(paths).note_links(catalysts);
    if links.is_empty() || catalysts.is_empty() {
        return HashMap::new();
    }
    // Each note's catalysts by descending fit.
    let mut pending: Vec<(&String, &Vec<f64>, Vec<usize>)> = links
        .iter()
        .map(|(path, link_sims)| {
            let mut order: Vec<usize> = (0..catalysts.len()).collect();
            order.sort_by(|a, b| {
                (query_sims[*b] + link_sims[*b])
                    .total_cmp(&(query_sims[*a] + link_sims[*a]))
                    .then(a.cmp(b))
            });
            order.truncate(MAX_CANDIDATES_PER_NOTE);
            (path, link_sims, order)
        })
        .collect();
    let mut floors: HashMap<usize, f64> = HashMap::new();
    let mut chosen = HashMap::new();
    while !pending.is_empty() {
        // Settle every note whose next candidates have known floors.
        let mut needed: Vec<usize> = Vec::new();
        pending.retain_mut(|(path, link_sims, order)| {
            while let Some(&pos) = order.first() {
                let Some(&floor) = floors.get(&pos) else {
                    for &next in order.iter().take(CANDIDATES_PER_ROUND) {
                        if !floors.contains_key(&next) && !needed.contains(&next) {
                            needed.push(next);
                        }
                    }
                    return true;
                };
                if link_sims[pos] >= floor {
                    chosen.insert((*path).clone(), pos);
                    return false;
                }
                order.remove(0);
            }
            false
        });
        if needed.is_empty() {
            break;
        }
        let vectors: Vec<&[f32]> = needed.iter().map(|&pos| catalysts[pos].1).collect();
        for (pos, floor) in needed
            .iter()
            .zip(corpus.top_k_floors(&vectors, CATALYST_TOP_K))
        {
            floors.insert(*pos, floor);
        }
    }
    chosen
}

/// Current catalyst vectors: stored vectors whose id is still in the
/// `catalysts` table, in stored order. Vectors left from an earlier catalyst
/// generation are skipped.
pub fn current_catalyst_vectors(db: &Database) -> Result<Vec<(String, Vec<f32>)>> {
    let current: std::collections::HashSet<String> =
        db.get_all_catalysts()?.into_iter().map(|c| c.id).collect();
    if current.is_empty() {
        return Ok(Vec::new());
    }
    Ok(db
        .get_all_catalyst_embeddings()?
        .into_iter()
        .filter(|(id, _)| current.contains(id))
        .collect())
}

/// Borrowed `(id, vector)` pairs for the scoring functions.
pub fn as_refs(vectors: &[(String, Vec<f32>)]) -> Vec<(&str, &[f32])> {
    vectors
        .iter()
        .map(|(id, v)| (id.as_str(), v.as_slice()))
        .collect()
}

/// Decode a stored chunk blob (see [`quantize::decode_vector`]).
fn decode(blob: &[u8]) -> Vec<f32> {
    quantize::decode_vector(blob, EMBEDDING_DIMENSION)
}

/// Decode a stored chunk blob into `row`, returning whether it had exactly
/// that width. The int8 shape is dequantized in place with the same
/// arithmetic as [`quantize::dequantize_int8`].
fn decode_into(blob: &[u8], row: &mut [f32]) -> bool {
    if quantize::is_int8_v1_len(blob.len(), EMBEDDING_DIMENSION) && row.len() == EMBEDDING_DIMENSION
    {
        let scale = f32::from_le_bytes(blob[0..4].try_into().unwrap());
        for (out, &b) in row.iter_mut().zip(&blob[4..]) {
            *out = (b as i8) as f32 * scale;
        }
        return true;
    }
    let vector = decode(blob);
    if vector.len() != row.len() {
        return false;
    }
    row.copy_from_slice(&vector);
    true
}

/// Dot product with independent accumulators so the compiler can vectorize.
fn dot(a: &[f32], b: &[f32]) -> f32 {
    let mut acc = [0.0f32; 8];
    let chunks_a = a.chunks_exact(8);
    let chunks_b = b.chunks_exact(8);
    let tail: f32 = chunks_a
        .remainder()
        .iter()
        .zip(chunks_b.remainder())
        .map(|(x, y)| x * y)
        .sum();
    for (x, y) in chunks_a.zip(chunks_b) {
        for i in 0..8 {
            acc[i] += x[i] * y[i];
        }
    }
    acc.iter().sum::<f32>() + tail
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Catalyst, Embedding};

    fn chunks(db: &Database, rows: &[(&str, u32, [f32; 2])]) {
        let rows = rows
            .iter()
            .map(|(p, i, v)| (p.to_string(), *i, v.to_vec()))
            .collect();
        db.replace_chunk_embeddings_for_documents(vec![], rows, vec![])
            .unwrap();
    }

    fn catalyst(id: &str) -> Catalyst {
        Catalyst {
            id: id.into(),
            text: format!("question {id}"),
            entity: "entity".into(),
            context: "default".into(),
            generated_at: chrono::Utc::now(),
            metadata: serde_json::json!({}),
        }
    }

    #[test]
    fn dot_matches_a_plain_loop_on_odd_lengths() {
        let a: Vec<f32> = (0..19).map(|i| i as f32 * 0.5 - 3.0).collect();
        let b: Vec<f32> = (0..19).map(|i| 1.0 - i as f32 * 0.25).collect();
        let plain: f32 = a.iter().zip(&b).map(|(x, y)| x * y).sum();
        assert!((dot(&a, &b) - plain).abs() < 1e-4);
    }

    #[test]
    fn top_chunks_rank_by_cosine_and_break_ties_by_path_then_index() {
        let db = Database::open_in_memory().unwrap();
        chunks(
            &db,
            &[
                ("b.md", 0, [1.0, 0.0]),
                ("a.md", 1, [2.0, 0.0]),
                ("a.md", 0, [1.0, 0.0]),
                ("c.md", 0, [0.0, 1.0]),
                ("d.md", 0, [1.0, 1.0]),
            ],
        );
        let vectors = ChunkVectors::load_all(&db).unwrap();
        let top = vectors.top_chunks(&[("x", &[1.0, 0.0][..]), ("y", &[0.0, 1.0][..])], 3);
        let keys = |id: &str| {
            top[id]
                .iter()
                .map(|l| (l.file_path.as_str(), l.chunk_index))
                .collect::<Vec<_>>()
        };
        assert_eq!(keys("x"), vec![("a.md", 0), ("a.md", 1), ("b.md", 0)]);
        assert_eq!(keys("y"), vec![("c.md", 0), ("d.md", 0), ("a.md", 0)]);
        assert!((top["y"][1].similarity - std::f64::consts::FRAC_1_SQRT_2).abs() < 1e-6);
    }

    #[test]
    fn top_chunks_match_the_cosine_used_elsewhere() {
        let db = Database::open_in_memory().unwrap();
        chunks(&db, &[("a.md", 0, [0.3, 0.9]), ("b.md", 0, [-0.5, 0.2])]);
        let vectors = ChunkVectors::load_all(&db).unwrap();
        let query = [0.8_f32, 0.1];
        let top = vectors.top_chunks(&[("x", &query[..])], 10);
        let stored = db.get_all_chunk_embeddings().unwrap();
        for link in &top["x"] {
            let (_, _, vector) = stored
                .iter()
                .find(|(path, _, _)| *path == link.file_path)
                .unwrap();
            let expected = Embedding::cosine_similarity(&query, vector) as f64;
            assert!((link.similarity - expected).abs() < 1e-6);
        }
        // The dense side of hybrid search reads the same rows with the
        // shared cosine, so its scores are exact.
        let best = vectors.best_chunk_per_note(&query);
        for (path, _, vector) in &stored {
            let expected = Embedding::cosine_similarity(&query, vector) as f64;
            assert_eq!(best[path].1, expected);
        }
    }

    #[test]
    fn note_links_take_each_notes_best_chunk_and_skip_other_notes() {
        let db = Database::open_in_memory().unwrap();
        chunks(
            &db,
            &[
                ("a.md", 0, [0.0, 1.0]),
                ("a.md", 1, [1.0, 0.0]),
                ("b.md", 0, [0.0, 1.0]),
                ("c.md", 0, [1.0, 0.0]),
            ],
        );
        let vectors = ChunkVectors::load_all(&db).unwrap().for_paths(&[
            "a.md".into(),
            "b.md".into(),
            "x.md".into(),
        ]);
        assert_eq!(vectors.len(), 3);
        let links = vectors.note_links(&[("x", &[1.0, 0.0][..]), ("y", &[0.0, 1.0][..])]);
        assert_eq!(links.len(), 2);
        assert!((links["a.md"][0] - 1.0).abs() < 1e-6);
        assert!((links["a.md"][1] - 1.0).abs() < 1e-6);
        assert!(links["b.md"][0].abs() < 1e-6);
        assert!(!links.contains_key("c.md"));
    }

    #[test]
    fn rows_of_another_width_are_dropped() {
        let db = Database::open_in_memory().unwrap();
        db.replace_chunk_embeddings_for_documents(
            vec![],
            vec![
                ("a.md".into(), 0, vec![1.0, 0.0]),
                ("b.md".into(), 0, vec![1.0, 0.0, 0.0]),
                ("c.md".into(), 0, vec![0.0, 1.0]),
            ],
            vec![],
        )
        .unwrap();
        let vectors = ChunkVectors::load_all(&db).unwrap();
        assert_eq!(vectors.len(), 2);
        let top = vectors.top_chunks(&[("y", &[0.0, 1.0][..])], 1);
        assert_eq!(top["y"][0].file_path, "c.md");
    }

    #[test]
    fn top_k_floors_are_each_catalysts_kth_best_score() {
        let db = Database::open_in_memory().unwrap();
        chunks(
            &db,
            &[
                ("a.md", 0, [1.0, 0.0]),
                ("b.md", 0, [1.0, 1.0]),
                ("c.md", 0, [0.0, 1.0]),
            ],
        );
        let vectors = ChunkVectors::load_all(&db).unwrap();
        let floors = vectors.top_k_floors(&[&[1.0, 0.0][..], &[0.0, 1.0][..]], 2);
        for floor in &floors {
            assert!((floor - std::f64::consts::FRAC_1_SQRT_2).abs() < 1e-6);
        }
        // With no more chunks than k, every chunk is in every top k.
        assert_eq!(vectors.top_k_floors(&[&[1.0, 0.0][..]], 3), vec![f64::MIN]);
    }

    /// A note is linked only to catalysts that rank it among their closest
    /// chunks across the corpus, and among those the best fit to the query
    /// and the note wins. A broad catalyst near everything loses to one that
    /// singles the note out, even when the broad one fits the query better.
    #[test]
    fn best_linked_catalysts_require_the_note_to_stand_out_for_the_catalyst() {
        let db = Database::open_in_memory().unwrap();
        // 60 filler chunks along the broad catalyst's direction, plus the
        // note, which sits between the broad and the specific direction.
        let mut rows: Vec<(String, u32, Vec<f32>)> = (0..60)
            .map(|i| (format!("filler-{i:02}.md"), 0, vec![1.0, 0.0, 0.0]))
            .collect();
        rows.push(("note.md".into(), 0, vec![0.6, 0.0, 0.8]));
        db.replace_chunk_embeddings_for_documents(vec![], rows, vec![])
            .unwrap();
        let corpus = ChunkVectors::load_all(&db).unwrap();
        let broad: &[f32] = &[1.0, 0.0, 0.0];
        let specific: &[f32] = &[0.0, 0.0, 1.0];
        let unrelated: &[f32] = &[0.0, 1.0, 0.0];
        let catalysts = [
            ("broad", broad),
            ("specific", specific),
            ("unrelated", unrelated),
        ];
        let paths = vec!["note.md".to_string(), "missing.md".to_string()];

        // Fit favors broad (0.9 + 0.6 vs 0.1 + 0.8), but 60 chunks outrank
        // the note for broad, so it is not among broad's top 50.
        let chosen = best_linked_catalysts(&corpus, &catalysts, &[0.9, 0.1, 0.0], &paths);
        assert_eq!(chosen.get("note.md"), Some(&1));
        assert!(!chosen.contains_key("missing.md"));

        // With only the broad catalyst, the note has no link at all.
        let chosen = best_linked_catalysts(&corpus, &[("broad", broad)], &[0.9], &paths);
        assert!(chosen.is_empty());
    }

    #[test]
    fn current_catalyst_vectors_skip_ids_no_longer_in_the_catalyst_table() {
        let db = Database::open_in_memory().unwrap();
        db.insert_catalysts(&[catalyst("kept")]).unwrap();
        db.upsert_catalyst_embeddings(&[("stale", &[1.0_f32, 0.0][..]), ("kept", &[0.0, 1.0][..])])
            .unwrap();
        let current = current_catalyst_vectors(&db).unwrap();
        assert_eq!(current.len(), 1);
        assert_eq!(current[0].0, "kept");
    }
}
