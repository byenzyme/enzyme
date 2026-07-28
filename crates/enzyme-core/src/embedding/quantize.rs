//! Symmetric per-vector int8 quantization for embedding storage.
//!
//! Embeddings coming out of the ESE backend are L2-normalized f32 vectors
//! (components roughly in `[-1, 1]`). Storing them as raw little-endian f32
//! costs 4 bytes/component; this module implements a deterministic, versioned
//! 1-byte/component encoding (plus a 4-byte scale header) that is ~4x smaller
//! and round-trips with error bounded by half a quantization step.
//!
//! # Format (`int8_v1`)
//!
//! Per vector: `scale: f32 LE` (`max_abs / 127`, or `1.0` for the all-zero
//! vector) followed by `dim` `i8` bytes, where each byte is
//! `round(component / scale)` clamped to `[-127, 127]`. Dequantization is
//! `component = i8 as f32 * scale`.
//!
//! Two blob shapes build on this:
//! - **Per-vector** (`chunk_embeddings.vector`): `[scale:4][i8 * dim]`, i.e.
//!   `dim + 4` bytes total.
//! - **Packed** (`catalyst_embeddings_packed.vectors` and the epoch-scoped
//!   equivalent): all `count` scales first, then all `count * dim` i8 bytes:
//!   `[scale_0..scale_{count-1}: f32 LE][i8 data: count * dim]`.
//!
//! # Legacy detection
//!
//! Blobs written before this format existed are raw little-endian f32 with no
//! header (`dim * 4` bytes per vector). Callers that might still encounter
//! such rows (general readers, and the one-time migration) disambiguate by
//! exact length against the vault's *expected* embedding dimension
//! (`crate::models::EMBEDDING_DIMENSION` in production), falling back to a
//! best-effort length heuristic when the expected dimension doesn't match
//! either candidate exactly (e.g. tests using small synthetic dimensions).

/// Config key recording the on-disk vector encoding version.
pub const VECTOR_FORMAT_CONFIG_KEY: &str = "embedding.vector_format";
/// Value written to [`VECTOR_FORMAT_CONFIG_KEY`] once a vault's blobs are
/// entirely `int8_v1`.
pub const VECTOR_FORMAT_INT8_V1: &str = "int8_v1";

/// Quantize a single vector into the `int8_v1` per-vector blob shape:
/// `[scale:4][i8 * dim]`.
pub fn quantize_vector(v: &[f32]) -> Vec<u8> {
    let scale = vector_scale(v);
    let mut buf = Vec::with_capacity(4 + v.len());
    buf.extend_from_slice(&scale.to_le_bytes());
    for &x in v {
        buf.push(quantize_component(x, scale));
    }
    buf
}

/// Dequantize an `int8_v1`-encoded per-vector blob (`[scale:4][i8 * dim]`).
/// Returns an empty vector if `blob` is shorter than the 4-byte header.
pub fn dequantize_int8(blob: &[u8]) -> Vec<f32> {
    if blob.len() < 4 {
        return Vec::new();
    }
    let scale = f32::from_le_bytes(blob[0..4].try_into().unwrap());
    blob[4..].iter().map(|&b| (b as i8) as f32 * scale).collect()
}

/// Decode a raw little-endian f32 blob with no header (the pre-`int8_v1`
/// format).
pub fn dequantize_legacy_f32(blob: &[u8]) -> Vec<f32> {
    blob.chunks_exact(4)
        .map(|c| f32::from_le_bytes(c.try_into().unwrap()))
        .collect()
}

/// `true` if a per-vector blob of length `len` is exactly the legacy f32
/// encoding for `dim` components.
pub fn is_legacy_f32_len(len: usize, dim: usize) -> bool {
    dim > 0 && len == dim * 4
}

/// `true` if a per-vector blob of length `len` is exactly the `int8_v1`
/// encoding for `dim` components.
pub fn is_int8_v1_len(len: usize, dim: usize) -> bool {
    len == dim + 4
}

/// Decode a single per-vector blob, detecting `int8_v1` vs. legacy f32.
///
/// `expected_dim` is the vault's known embedding dimension (e.g.
/// [`crate::models::EMBEDDING_DIMENSION`]); it disambiguates the two formats
/// by exact length. If the blob's length doesn't match either candidate for
/// `expected_dim` exactly (e.g. synthetic test vectors with a different
/// dimension than the compiled-in backend), falls back to a length-only
/// heuristic: a clean multiple of 4 is treated as legacy f32, otherwise
/// `int8_v1`. This heuristic is unambiguous for any dimension not congruent
/// to 0 mod 4; production data always resolves via the exact-length branch.
pub fn decode_vector(blob: &[u8], expected_dim: usize) -> Vec<f32> {
    if is_int8_v1_len(blob.len(), expected_dim) {
        dequantize_int8(blob)
    } else if is_legacy_f32_len(blob.len(), expected_dim) {
        dequantize_legacy_f32(blob)
    } else if blob.len() >= 5 && blob.len() % 4 != 0 {
        dequantize_int8(blob)
    } else {
        dequantize_legacy_f32(blob)
    }
}

/// Quantize a batch of same-dimension vectors into the packed blob shape:
/// `[scale_0..scale_{count-1}: f32 LE][i8 data: count * dim]`.
///
/// Panics-free even for an empty slice (returns an empty buffer) or vectors
/// of differing lengths (each vector contributes its own length worth of i8
/// bytes; readers must know `count` and total length to recover per-vector
/// dimension, as with the legacy format).
pub fn quantize_packed<'a>(vectors: impl Iterator<Item = &'a [f32]> + Clone) -> Vec<u8> {
    let scales: Vec<f32> = vectors.clone().map(vector_scale).collect();
    let mut buf = Vec::with_capacity(scales.len() * 4);
    for scale in &scales {
        buf.extend_from_slice(&scale.to_le_bytes());
    }
    for (vector, &scale) in vectors.zip(scales.iter()) {
        for &x in vector {
            buf.push(quantize_component(x, scale));
        }
    }
    buf
}

/// Dequantize a packed `int8_v1` blob given the known `count` of vectors.
/// Returns one `Vec<f32>` per vector, each of dimension
/// `(blob.len() - count * 4) / count`.
pub fn dequantize_packed(blob: &[u8], count: usize) -> Vec<Vec<f32>> {
    if count == 0 {
        return Vec::new();
    }
    let scales_bytes = count * 4;
    if blob.len() < scales_bytes {
        return Vec::new();
    }
    let dim = (blob.len() - scales_bytes) / count;
    let scales: Vec<f32> = blob[..scales_bytes]
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes(c.try_into().unwrap()))
        .collect();
    let data = &blob[scales_bytes..];
    scales
        .into_iter()
        .zip(data.chunks(dim))
        .map(|(scale, chunk)| chunk.iter().map(|&b| (b as i8) as f32 * scale).collect())
        .collect()
}

/// `true` if a packed blob of length `len` for `count` vectors is exactly the
/// legacy f32 packed encoding for `dim` components per vector.
pub fn is_legacy_f32_packed_len(len: usize, count: usize, dim: usize) -> bool {
    count > 0 && dim > 0 && len == count * dim * 4
}

/// `true` if a packed blob of length `len` for `count` vectors is exactly the
/// `int8_v1` packed encoding for `dim` components per vector.
pub fn is_int8_v1_packed_len(len: usize, count: usize, dim: usize) -> bool {
    count > 0 && len == count * 4 + count * dim
}

/// Decode a packed blob, detecting `int8_v1` vs. legacy f32, given `count`
/// and the vault's `expected_dim`. Mirrors [`decode_vector`]'s
/// disambiguation strategy at the packed-blob granularity.
pub fn decode_packed(blob: &[u8], count: usize, expected_dim: usize) -> Vec<Vec<f32>> {
    if count == 0 {
        return Vec::new();
    }
    if is_int8_v1_packed_len(blob.len(), count, expected_dim) {
        return dequantize_packed(blob, count);
    }
    if is_legacy_f32_packed_len(blob.len(), count, expected_dim) {
        let bytes_per_vector = blob.len() / count;
        return blob
            .chunks(bytes_per_vector)
            .map(dequantize_legacy_f32)
            .collect();
    }
    // Unknown dimension relative to `expected_dim`: fall back to a
    // length-only heuristic consistent with `decode_vector`.
    let bytes_per_vector = if count > 0 { blob.len() / count } else { 0 };
    if bytes_per_vector >= 5 && (blob.len() - count * 4) % count == 0 {
        dequantize_packed(blob, count)
    } else {
        blob.chunks(bytes_per_vector.max(1))
            .map(dequantize_legacy_f32)
            .collect()
    }
}

fn vector_scale(v: &[f32]) -> f32 {
    let max_abs = v.iter().fold(0f32, |m, &x| m.max(x.abs()));
    if max_abs == 0.0 {
        1.0
    } else {
        max_abs / 127.0
    }
}

fn quantize_component(x: f32, scale: f32) -> u8 {
    let q = (x / scale).round().clamp(-127.0, 127.0);
    q as i8 as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx_eq(a: f32, b: f32, eps: f32) -> bool {
        (a - b).abs() <= eps
    }

    #[test]
    fn roundtrip_error_bounded_by_half_scale() {
        let v: Vec<f32> = (0..512)
            .map(|i| ((i as f32 * 0.017).sin()) * 0.9)
            .collect();
        let blob = quantize_vector(&v);
        let scale = f32::from_le_bytes(blob[0..4].try_into().unwrap());
        let decoded = dequantize_int8(&blob);
        assert_eq!(decoded.len(), v.len());
        for (orig, got) in v.iter().zip(decoded.iter()) {
            assert!(
                approx_eq(*orig, *got, scale / 2.0 + 1e-6),
                "orig={orig} got={got} scale={scale}"
            );
        }
    }

    #[test]
    fn zero_vector_uses_unit_scale_and_roundtrips_to_zero() {
        let v = vec![0.0f32; 16];
        let blob = quantize_vector(&v);
        let scale = f32::from_le_bytes(blob[0..4].try_into().unwrap());
        assert_eq!(scale, 1.0);
        let decoded = dequantize_int8(&blob);
        assert_eq!(decoded, vec![0.0f32; 16]);
    }

    #[test]
    fn all_negative_vector_roundtrips() {
        let v: Vec<f32> = (0..32).map(|i| -0.1 - (i as f32) * 0.01).collect();
        let blob = quantize_vector(&v);
        let scale = f32::from_le_bytes(blob[0..4].try_into().unwrap());
        let decoded = dequantize_int8(&blob);
        for (orig, got) in v.iter().zip(decoded.iter()) {
            assert!(approx_eq(*orig, *got, scale / 2.0 + 1e-6));
        }
    }

    #[test]
    fn clamps_outlier_components_to_i8_range() {
        // max_abs is 10.0, so scale = 10.0/127. A component equal to max_abs
        // should map to exactly +127/-127, never overflow.
        let v = vec![10.0f32, -10.0, 0.0, 5.0];
        let blob = quantize_vector(&v);
        let i8_bytes = &blob[4..];
        assert_eq!(i8_bytes[0] as i8, 127);
        assert_eq!(i8_bytes[1] as i8, -127);
    }

    #[test]
    fn blob_length_matches_dim_plus_four() {
        let v = vec![0.5f32; 512];
        let blob = quantize_vector(&v);
        assert_eq!(blob.len(), 512 + 4);
    }

    #[test]
    fn legacy_blob_detected_by_length_for_expected_dim() {
        let dim = 512;
        let legacy_len = dim * 4;
        let int8_len = dim + 4;
        assert!(is_legacy_f32_len(legacy_len, dim));
        assert!(!is_int8_v1_len(legacy_len, dim));
        assert!(is_int8_v1_len(int8_len, dim));
        assert!(!is_legacy_f32_len(int8_len, dim));
    }

    #[test]
    fn decode_vector_dispatches_legacy_f32_by_exact_length() {
        let dim = 8;
        let v: Vec<f32> = (0..dim).map(|i| i as f32 * 0.1).collect();
        let legacy_blob: Vec<u8> = v.iter().flat_map(|x| x.to_le_bytes()).collect();
        assert_eq!(legacy_blob.len(), dim * 4);
        let decoded = decode_vector(&legacy_blob, dim);
        assert_eq!(decoded, v);
    }

    #[test]
    fn decode_vector_dispatches_int8_v1_by_exact_length() {
        let dim = 8;
        let v: Vec<f32> = (0..dim).map(|i| (i as f32 * 0.1) - 0.3).collect();
        let blob = quantize_vector(&v);
        let decoded = decode_vector(&blob, dim);
        assert_eq!(decoded.len(), dim);
        for (orig, got) in v.iter().zip(decoded.iter()) {
            assert!(approx_eq(*orig, *got, 0.05));
        }
    }

    #[test]
    fn decode_vector_falls_back_when_expected_dim_does_not_match() {
        // Simulate a test-style small vector whose dim doesn't match the
        // caller's "expected" production dimension (e.g. 512).
        let v = vec![0.1f32, -0.2, 0.3];
        let int8_blob = quantize_vector(&v);
        let decoded = decode_vector(&int8_blob, 512);
        assert_eq!(decoded.len(), v.len());

        let legacy_blob: Vec<u8> = v.iter().flat_map(|x| x.to_le_bytes()).collect();
        let decoded_legacy = decode_vector(&legacy_blob, 512);
        assert_eq!(decoded_legacy, v);
    }

    #[test]
    fn empty_vector_quantizes_to_four_byte_header_only() {
        let blob = quantize_vector(&[]);
        assert_eq!(blob.len(), 4);
        assert!(dequantize_int8(&blob).is_empty());
    }

    #[test]
    fn packed_roundtrip() {
        let vectors: Vec<Vec<f32>> = (0..5)
            .map(|i| (0..16).map(|j| ((i * 16 + j) as f32 * 0.01) - 0.5).collect())
            .collect();
        let refs: Vec<&[f32]> = vectors.iter().map(|v| v.as_slice()).collect();
        let blob = quantize_packed(refs.into_iter());
        assert_eq!(blob.len(), 5 * 4 + 5 * 16);
        let decoded = dequantize_packed(&blob, 5);
        assert_eq!(decoded.len(), 5);
        for (orig, got) in vectors.iter().zip(decoded.iter()) {
            assert_eq!(orig.len(), got.len());
            for (o, g) in orig.iter().zip(got.iter()) {
                assert!(approx_eq(*o, *g, 0.05));
            }
        }
    }

    #[test]
    fn packed_empty_is_empty() {
        let blob = quantize_packed(std::iter::empty());
        assert!(blob.is_empty());
        assert!(dequantize_packed(&blob, 0).is_empty());
    }

    #[test]
    fn decode_packed_dispatches_legacy_by_exact_length() {
        let count = 3;
        let dim = 4;
        let vectors: Vec<Vec<f32>> = (0..count)
            .map(|i| (0..dim).map(|j| (i * dim + j) as f32 * 0.25).collect())
            .collect();
        let legacy_blob: Vec<u8> = vectors
            .iter()
            .flat_map(|v| v.iter().flat_map(|x| x.to_le_bytes()))
            .collect();
        assert!(is_legacy_f32_packed_len(legacy_blob.len(), count, dim));
        let decoded = decode_packed(&legacy_blob, count, dim);
        assert_eq!(decoded, vectors);
    }

    #[test]
    fn decode_packed_dispatches_int8_by_exact_length() {
        let count = 3;
        let dim = 4;
        let vectors: Vec<Vec<f32>> = (0..count)
            .map(|i| (0..dim).map(|j| (i * dim + j) as f32 * 0.1 - 0.2).collect())
            .collect();
        let refs: Vec<&[f32]> = vectors.iter().map(|v| v.as_slice()).collect();
        let blob = quantize_packed(refs.into_iter());
        assert!(is_int8_v1_packed_len(blob.len(), count, dim));
        let decoded = decode_packed(&blob, count, dim);
        assert_eq!(decoded.len(), count);
        for (orig, got) in vectors.iter().zip(decoded.iter()) {
            for (o, g) in orig.iter().zip(got.iter()) {
                assert!(approx_eq(*o, *g, 0.05));
            }
        }
    }

    /// End-to-end equivalence check against the real ESE backend: quantizing
    /// and dequantizing must barely perturb cosine similarity, and must not
    /// change which documents rank as a query's nearest neighbors.
    #[test]
    fn search_equivalence_with_real_ese_backend() {
        use crate::embedding::{EmbeddingBackend, EseBackend};
        use crate::models::Embedding;

        let docs = [
            "The quarterly earnings report showed strong revenue growth.",
            "Stock prices rallied after the central bank's rate decision.",
            "Investors are optimistic about the company's profit margins.",
            "The startup raised a new round of venture capital funding.",
            "Analysts upgraded the stock following the earnings beat.",
            "A gentle breeze drifted through the quiet mountain meadow.",
            "Wildflowers bloomed across the valley in early spring.",
            "The hikers watched the sunset paint the peaks orange.",
            "Snow melted slowly from the shaded side of the ridge.",
            "A river wound through the forest below the trailhead.",
            "The chef simmered the tomato sauce for several hours.",
            "Fresh basil and garlic gave the dish its aroma.",
            "The bakery's sourdough loaves sold out by noon.",
            "She seasoned the roast with rosemary and cracked pepper.",
            "The recipe called for a splash of red wine vinegar.",
            "The novel's protagonist wrestled with a difficult choice.",
            "Critics praised the film's haunting cinematography.",
            "The orchestra's performance drew a standing ovation.",
            "The museum's new exhibit features surrealist paintings.",
            "The playwright's latest work explores memory and loss.",
        ];

        let backend = EseBackend::new();
        let originals = backend
            .embed_batch(&docs)
            .expect("ese backend should embed synthetic docs");
        assert_eq!(originals.len(), docs.len());

        // (a) Per-vector cosine similarity between original and
        // quantize->dequantize round-tripped vectors must be near 1.0.
        let dequantized: Vec<Vec<f32>> = originals
            .iter()
            .map(|v| dequantize_int8(&quantize_vector(v)))
            .collect();
        for (i, (orig, deq)) in originals.iter().zip(dequantized.iter()).enumerate() {
            let sim = Embedding::cosine_similarity(orig, deq);
            assert!(
                sim >= 0.999,
                "doc {i} cosine similarity degraded too much after quantization: {sim}"
            );
        }

        // (b) Top-3 nearest-neighbor ordering (by cosine similarity) over the
        // whole synthetic corpus must be unchanged between f32 and
        // dequantized-int8 embeddings, for every doc used as a query.
        let top3 = |vectors: &[Vec<f32>], query_idx: usize| -> Vec<usize> {
            let mut scored: Vec<(usize, f32)> = vectors
                .iter()
                .enumerate()
                .filter(|(i, _)| *i != query_idx)
                .map(|(i, v)| (i, Embedding::cosine_similarity(&vectors[query_idx], v)))
                .collect();
            scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap().then_with(|| a.0.cmp(&b.0)));
            scored.into_iter().take(3).map(|(i, _)| i).collect()
        };

        for query_idx in 0..docs.len() {
            let f32_top3 = top3(&originals, query_idx);
            let int8_top3 = top3(&dequantized, query_idx);
            assert_eq!(
                f32_top3, int8_top3,
                "top-3 nearest neighbors for doc {query_idx} changed after quantization \
                 (f32={f32_top3:?} int8={int8_top3:?})"
            );
        }
    }
}
