#[inline(always)]
pub fn accumulate(vector: &mut [f32; crate::DIMENSIONS], param: &crate::lookup::Param) {
    for (v, &p) in vector.iter_mut().zip(param.iter()) {
        *v += crate::lookup::QUANT_MIN + (p as f32) * crate::lookup::QUANT_SCALE;
    }
}

#[inline]
pub fn wordpiece_accumulate(
    word: &str,
    vector: &mut [f32; crate::DIMENSIONS],
    token_count: &mut u32,
    wp_buf: &mut String,
) {
    if word.chars().count() > crate::lookup::MAX_WORD_LEN {
        *token_count += 1;
        accumulate(vector, &crate::lookup::UNK);
        return;
    }
    let mut start = 0;
    while start < word.len() {
        let mut end = word.len();
        let mut matched = false;
        while end > start {
            while end < word.len() && !word.is_char_boundary(end) {
                end += 1;
            }
            let embedding = if start == 0 {
                crate::lookup::lookup(&word[start..end])
            } else {
                wp_buf.clear();
                wp_buf.push_str("##");
                wp_buf.push_str(&word[start..end]);
                crate::lookup::lookup(wp_buf.as_str())
            };
            if let Some(emb) = embedding {
                *token_count += 1;
                accumulate(vector, emb);
                start = end;
                matched = true;
                break;
            }
            end -= 1;
            while end > start && !word.is_char_boundary(end) {
                end -= 1;
            }
        }
        if !matched {
            *token_count += 1;
            accumulate(vector, &crate::lookup::UNK);
            return;
        }
    }
}
