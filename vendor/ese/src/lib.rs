mod lookup;
mod pretokenizer;
mod wordpiece;

/// DIMENSIONS is the number of dimensions per encoding.
/// This can be changed with the `dim-*` crate features.
///
/// # Examples
///
/// ```
/// let encoding = ese::encode_single("hello world");
/// assert_eq!(encoding.len(), ese::DIMENSIONS);
/// ```
pub const DIMENSIONS: usize = lookup::DIMENSIONS;

/// Encodes a batch of texts into embedding vectors.
///
/// Each input string is independently normalized, tokenized, and encoded.
/// Enable the `rayon` feature for higher throughput on large batches (>16 inputs).
///
/// # Examples
///
/// ```
/// let texts = vec![
///     "potato".to_string(),
///     "root vegetable".to_string(),
/// ];
///
/// let embeddings = ese::encode(texts);
/// assert_eq!(embeddings.len(), 2);
/// assert_eq!(embeddings[0].len(), ese::DIMENSIONS);
/// ```
#[cfg(not(feature = "rayon"))]
pub fn encode(inputs: impl IntoIterator<Item = impl AsRef<str>>) -> Vec<[f32; DIMENSIONS]> {
    inputs
        .into_iter()
        .map(|input| encode_single(input))
        .collect()
}

/// Encodes a batch of texts into embedding vectors.
///
/// Each input string is independently normalized, tokenized, and encoded.
/// Batches of 16 or more inputs are processed in parallel.
///
/// # Examples
///
/// ```
/// let texts = vec![
///     "potato".to_string(),
///     "root vegetable".to_string(),
/// ];
///
/// let embeddings = ese::encode(texts);
/// assert_eq!(embeddings.len(), 2);
/// assert_eq!(embeddings[0].len(), ese::DIMENSIONS);
/// ```
#[cfg(feature = "rayon")]
pub fn encode(
    inputs: impl IntoIterator<Item = impl AsRef<str> + Send + Sync>,
) -> Vec<[f32; DIMENSIONS]> {
    use rayon::prelude::*;

    let inputs: Vec<_> = inputs.into_iter().collect();
    let map_fn = |input: &_| encode_single(input);

    if inputs.len() >= 16 {
        return inputs.par_iter().map(map_fn).collect();
    }

    inputs.iter().map(map_fn).collect()
}

/// Encodes a single text into an embedding vector.
///
/// # Examples
///
/// ```
/// let a = ese::encode_single("rustacean");
/// let b = ese::encode_single("Rustacean");
/// assert_eq!(a, b, "encoding is case-insensitive");
///
/// let v = ese::encode_single("hello world");
/// assert_eq!(v.len(), ese::DIMENSIONS);
/// ```
#[inline]
pub fn encode_single(input: impl AsRef<str>) -> [f32; DIMENSIONS] {
    let text = input.as_ref();
    let mut vector = [0.0f32; DIMENSIONS];
    wordpiece::accumulate(&mut vector, &lookup::CLS);
    let mut token_count = 0;
    let mut normalized = String::with_capacity(text.len());
    pretokenizer::normalize_into(text, &mut normalized);
    let mut wp_buf = String::with_capacity(64);
    pretokenizer::for_each_pre_token(&normalized, |token| {
        wordpiece::wordpiece_accumulate(token, &mut vector, &mut token_count, &mut wp_buf);
    });
    let scale = if token_count != 0 {
        1.0 / token_count as f32
    } else {
        1.0
    };
    let mut sep = [0.0f32; DIMENSIONS];
    wordpiece::accumulate(&mut sep, &lookup::SEP);
    for (v, s) in vector.iter_mut().zip(sep.iter()) {
        *v = (*v + *s) * scale;
    }
    vector
}

#[cfg(test)]
mod test;
