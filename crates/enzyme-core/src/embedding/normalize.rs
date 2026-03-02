/// L2-normalize a vector in place.
///
/// After normalization the vector has unit length (L2 norm = 1.0).
/// A zero vector is left unchanged.
pub fn l2_normalize(v: &mut [f32]) {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

/// Mean pool over token embeddings using an attention mask.
///
/// `token_embeddings` has shape `[seq_len][hidden_dim]`.
/// `attention_mask` has shape `[seq_len]` — 1.0 for real tokens, 0.0 for padding.
///
/// Returns a single vector of length `hidden_dim` representing the masked mean
/// of the token embeddings.
///
/// # Panics
///
/// Panics if `token_embeddings` is empty or if `attention_mask` length differs
/// from `token_embeddings` length.
pub fn mean_pool(token_embeddings: &[Vec<f32>], attention_mask: &[f32]) -> Vec<f32> {
    assert!(
        !token_embeddings.is_empty(),
        "token_embeddings must not be empty"
    );
    assert_eq!(
        token_embeddings.len(),
        attention_mask.len(),
        "token_embeddings and attention_mask must have the same length"
    );

    let hidden_dim = token_embeddings[0].len();
    let mut sum = vec![0.0f32; hidden_dim];
    let mut mask_sum: f32 = 0.0;

    for (embedding, &mask) in token_embeddings.iter().zip(attention_mask.iter()) {
        debug_assert_eq!(embedding.len(), hidden_dim, "inconsistent hidden_dim");
        if mask > 0.0 {
            mask_sum += mask;
            for (s, &e) in sum.iter_mut().zip(embedding.iter()) {
                *s += e * mask;
            }
        }
    }

    // Avoid division by zero when mask is all zeros.
    if mask_sum > 0.0 {
        for s in sum.iter_mut() {
            *s /= mask_sum;
        }
    }

    sum
}

/// Mean pool directly from a flat `[seq_len * hidden_dim]` buffer, avoiding
/// per-token `Vec` allocations.
///
/// `hidden_states` is a contiguous slice of `seq_len * hidden_dim` floats
/// (row-major: token 0's hidden_dim values, then token 1's, etc.).
/// `attention_mask` has `seq_len` entries — 1.0 for real tokens, 0.0 for padding.
///
/// Returns a single vector of length `hidden_dim`.
pub fn mean_pool_flat(
    hidden_states: &[f32],
    attention_mask: &[f32],
    seq_len: usize,
    hidden_dim: usize,
) -> Vec<f32> {
    debug_assert_eq!(hidden_states.len(), seq_len * hidden_dim);
    debug_assert!(attention_mask.len() >= seq_len);

    let mut sum = vec![0.0f32; hidden_dim];
    let mut mask_sum: f32 = 0.0;

    for s in 0..seq_len {
        let mask = attention_mask[s];
        if mask > 0.0 {
            mask_sum += mask;
            let offset = s * hidden_dim;
            let token = &hidden_states[offset..offset + hidden_dim];
            for (acc, &val) in sum.iter_mut().zip(token.iter()) {
                *acc += val * mask;
            }
        }
    }

    if mask_sum > 0.0 {
        for s in sum.iter_mut() {
            *s /= mask_sum;
        }
    }

    sum
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- l2_normalize tests ----

    #[test]
    fn test_normalize_produces_unit_vector() {
        let mut v = vec![3.0, 4.0];
        l2_normalize(&mut v);
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!(
            (norm - 1.0).abs() < 1e-6,
            "expected unit norm, got {norm}"
        );
        assert!((v[0] - 0.6).abs() < 1e-6);
        assert!((v[1] - 0.8).abs() < 1e-6);
    }

    #[test]
    fn test_normalize_already_unit() {
        let mut v = vec![1.0, 0.0, 0.0];
        l2_normalize(&mut v);
        assert!((v[0] - 1.0).abs() < 1e-6);
        assert!(v[1].abs() < 1e-6);
        assert!(v[2].abs() < 1e-6);
    }

    #[test]
    fn test_normalize_zero_vector_unchanged() {
        let mut v = vec![0.0, 0.0, 0.0];
        l2_normalize(&mut v);
        assert!(v.iter().all(|&x| x == 0.0));
    }

    #[test]
    fn test_normalize_negative_values() {
        let mut v = vec![-3.0, 4.0];
        l2_normalize(&mut v);
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!(
            (norm - 1.0).abs() < 1e-6,
            "expected unit norm, got {norm}"
        );
    }

    #[test]
    fn test_normalize_single_element() {
        let mut v = vec![5.0];
        l2_normalize(&mut v);
        assert!((v[0] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_normalize_high_dimensional() {
        let mut v: Vec<f32> = (1..=384).map(|i| i as f32).collect();
        l2_normalize(&mut v);
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!(
            (norm - 1.0).abs() < 1e-5,
            "expected unit norm, got {norm}"
        );
    }

    // ---- mean_pool tests ----

    #[test]
    fn test_mean_pool_uniform_mask() {
        let embeddings = vec![vec![1.0, 2.0, 3.0], vec![3.0, 4.0, 5.0]];
        let mask = vec![1.0, 1.0];
        let result = mean_pool(&embeddings, &mask);
        assert_eq!(result.len(), 3);
        assert!((result[0] - 2.0).abs() < 1e-6);
        assert!((result[1] - 3.0).abs() < 1e-6);
        assert!((result[2] - 4.0).abs() < 1e-6);
    }

    #[test]
    fn test_mean_pool_with_padding_mask() {
        // Second token is padding (mask = 0)
        let embeddings = vec![
            vec![10.0, 20.0],
            vec![99.0, 99.0], // should be ignored
        ];
        let mask = vec![1.0, 0.0];
        let result = mean_pool(&embeddings, &mask);
        assert!((result[0] - 10.0).abs() < 1e-6);
        assert!((result[1] - 20.0).abs() < 1e-6);
    }

    #[test]
    fn test_mean_pool_partial_mask() {
        let embeddings = vec![
            vec![2.0, 4.0],
            vec![6.0, 8.0],
            vec![99.0, 99.0], // padding
        ];
        let mask = vec![1.0, 1.0, 0.0];
        let result = mean_pool(&embeddings, &mask);
        assert!((result[0] - 4.0).abs() < 1e-6);
        assert!((result[1] - 6.0).abs() < 1e-6);
    }

    #[test]
    fn test_mean_pool_all_masked() {
        let embeddings = vec![vec![1.0, 2.0], vec![3.0, 4.0]];
        let mask = vec![0.0, 0.0];
        let result = mean_pool(&embeddings, &mask);
        // All masked out — result is zero vector.
        assert!(result.iter().all(|&x| x == 0.0));
    }

    #[test]
    fn test_mean_pool_single_token() {
        let embeddings = vec![vec![5.0, 10.0, 15.0]];
        let mask = vec![1.0];
        let result = mean_pool(&embeddings, &mask);
        assert!((result[0] - 5.0).abs() < 1e-6);
        assert!((result[1] - 10.0).abs() < 1e-6);
        assert!((result[2] - 15.0).abs() < 1e-6);
    }

    #[test]
    #[should_panic(expected = "token_embeddings must not be empty")]
    fn test_mean_pool_empty_embeddings() {
        let embeddings: Vec<Vec<f32>> = vec![];
        let mask: Vec<f32> = vec![];
        mean_pool(&embeddings, &mask);
    }

    #[test]
    #[should_panic(expected = "same length")]
    fn test_mean_pool_mismatched_lengths() {
        let embeddings = vec![vec![1.0, 2.0]];
        let mask = vec![1.0, 1.0]; // wrong length
        mean_pool(&embeddings, &mask);
    }

    // ---- mean_pool_flat tests ----

    #[test]
    fn test_mean_pool_flat_uniform_mask() {
        let hidden = vec![1.0, 2.0, 3.0, 3.0, 4.0, 5.0];
        let mask = vec![1.0, 1.0];
        let result = mean_pool_flat(&hidden, &mask, 2, 3);
        assert!((result[0] - 2.0).abs() < 1e-6);
        assert!((result[1] - 3.0).abs() < 1e-6);
        assert!((result[2] - 4.0).abs() < 1e-6);
    }

    #[test]
    fn test_mean_pool_flat_with_padding() {
        let hidden = vec![10.0, 20.0, 99.0, 99.0];
        let mask = vec![1.0, 0.0];
        let result = mean_pool_flat(&hidden, &mask, 2, 2);
        assert!((result[0] - 10.0).abs() < 1e-6);
        assert!((result[1] - 20.0).abs() < 1e-6);
    }

    #[test]
    fn test_mean_pool_flat_all_masked() {
        let hidden = vec![1.0, 2.0, 3.0, 4.0];
        let mask = vec![0.0, 0.0];
        let result = mean_pool_flat(&hidden, &mask, 2, 2);
        assert!(result.iter().all(|&x| x == 0.0));
    }

    #[test]
    fn test_mean_pool_flat_matches_vec_version() {
        // Verify mean_pool_flat produces the same result as mean_pool
        let hidden = vec![2.0, 4.0, 6.0, 8.0, 99.0, 99.0];
        let mask = vec![1.0, 1.0, 0.0];
        let flat_result = mean_pool_flat(&hidden, &mask, 3, 2);

        let embeddings = vec![vec![2.0, 4.0], vec![6.0, 8.0], vec![99.0, 99.0]];
        let vec_result = mean_pool(&embeddings, &mask);

        for (a, b) in flat_result.iter().zip(vec_result.iter()) {
            assert!((a - b).abs() < 1e-6);
        }
    }
}
