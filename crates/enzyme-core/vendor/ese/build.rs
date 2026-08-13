use std::{
    collections::HashSet,
    env,
    fs::{self},
    path::{Path, PathBuf},
};

mod build_support;

use build_support::resolve_shared_model_dir;

static MODEL_URL: &'static str = "https://huggingface.co/sentence-transformers/static-retrieval-mrl-en-v1/resolve/main/0_StaticEmbedding/model.safetensors";
static TOKENIZER_URL: &'static str = "https://huggingface.co/sentence-transformers/static-retrieval-mrl-en-v1/resolve/main/0_StaticEmbedding/tokenizer.json";

fn main() {
    println!("cargo:rerun-if-env-changed=ESE_MODEL_CACHE");

    let out = Path::new(&env::var("OUT_DIR").unwrap()).to_path_buf();

    let dtype = dtype();

    let model_dir = shared_model_dir(&out);
    let model_path = &model_dir.join("model.safetensors");
    let tokenizer_path = &model_dir.join("tokenizer.json");

    download_if_missing(model_path, MODEL_URL);
    download_if_missing(tokenizer_path, TOKENIZER_URL);
    let (weights, dims) = parse_safetensors(model_path);

    println!(
        "cargo:rerun-if-changed={}",
        tokenizer_path.to_str().unwrap()
    );
    let tokenizer_data = fs::read_to_string(tokenizer_path).expect("failed to read tokenizer file");
    let tk: serde_json::Value = serde_json::from_str(tokenizer_data.as_str()).unwrap();
    let vocab = tk["model"]["vocab"]
        .as_object()
        .expect("missing model.vocab");

    let mut unk: Vec<f32> = Default::default();
    let mut cls: Vec<f32> = Default::default();
    let mut sep: Vec<f32> = Default::default();

    // Collect (token, weight_index) pairs
    let mut entries: Vec<(String, usize)> = Vec::new();
    for (token, id_val) in vocab {
        let i = id_val.as_u64().unwrap() as usize;
        if i >= weights.len() {
            continue;
        }
        entries.push((token.clone(), i));
        match i {
            100 => unk = weights[i].clone(),
            101 => cls = weights[i].clone(),
            102 => sep = weights[i].clone(),
            _ => {}
        }
    }

    // hash table
    let vocab_size = entries.len();
    let table_size = vocab_size.next_power_of_two();
    let table_mask = table_size - 1;
    let num_buckets = vocab_size;

    // bucket entries by primary hash
    let mut buckets: Vec<Vec<usize>> = vec![vec![]; num_buckets];
    for (i, (token, _)) in entries.iter().enumerate() {
        let b = (phf_hash(token.as_bytes(), 0) as usize) % num_buckets;
        buckets[b].push(i);
    }

    let mut bucket_order: Vec<usize> = (0..num_buckets).collect();
    bucket_order.sort_by(|a, b| buckets[*b].len().cmp(&buckets[*a].len()));

    let mut seeds: Vec<u32> = vec![0; num_buckets];
    let mut verify: Vec<u64> = vec![0; table_size];
    let mut slot_to_weight: Vec<usize> = vec![0; table_size];
    let mut occupied: Vec<bool> = vec![false; table_size];

    for &bi in &bucket_order {
        if buckets[bi].is_empty() {
            continue;
        }

        let mut attempts = 0;
        let mut i: u32 = 0;
        'search: loop {
            let seed = i | 1;
            i += 1;

            attempts += 1;
            assert!(
                attempts < 1_000_000,
                "PHF construction stuck on bucket {bi} ({} entries)",
                buckets[bi].len()
            );

            let mut trial_slots: Vec<usize> = Vec::with_capacity(buckets[bi].len());
            let mut seen = HashSet::with_capacity(buckets[bi].len());

            for &entry_idx in &buckets[bi] {
                let slot =
                    (phf_hash(entries[entry_idx].0.as_bytes(), seed as u64) as usize) & table_mask;
                if occupied[slot] || !seen.insert(slot) {
                    continue 'search;
                }
                trial_slots.push(slot);
            }

            // all placed
            seeds[bi] = seed;
            for (j, &entry_idx) in buckets[bi].iter().enumerate() {
                let slot = trial_slots[j];
                occupied[slot] = true;
                verify[slot] = phf_hash(entries[entry_idx].0.as_bytes(), u64::MAX);
                slot_to_weight[slot] = entries[entry_idx].1;
            }
            break;
        }
    }

    // compute global min/max for quantization
    let (global_min, global_max) = if cfg!(feature = "quant-8") || cfg!(feature = "quant-16") {
        let mut mn = f32::INFINITY;
        let mut mx = f32::NEG_INFINITY;
        for row in &weights {
            for &v in row {
                mn = mn.min(v);
                mx = mx.max(v);
            }
        }
        (mn, mx)
    } else {
        (0.0, 1.0) // unused
    };

    let quant_max = if cfg!(feature = "quant-8") {
        255.0
    } else if cfg!(feature = "quant-16") {
        65535.0
    } else {
        1.0
    };

    let quant_scale = if (global_max - global_min).abs() < f32::EPSILON {
        1.0
    } else {
        (global_max - global_min) / quant_max
    };

    // reorder weights into PHF slot with verify hash prepended
    // && zero-fill empty slots
    let zero_row = vec![0.0f32; dims as usize];
    let slot_size = 8 + dims as usize * dtype_sz();
    let mut phf_slots: Vec<u8> = Vec::with_capacity(table_size * slot_size);
    for slot in 0..table_size {
        phf_slots.extend_from_slice(&verify[slot].to_le_bytes());
        let row = if occupied[slot] {
            &weights[slot_to_weight[slot]]
        } else {
            &zero_row
        };
        phf_slots.extend_from_slice(&quantize_f32_to_bytes(row, global_min, quant_scale));
    }
    fs::write(out.join("weights.bin"), &phf_slots).unwrap();

    // emit seeds as [u64; NUM_BUCKETS]
    let mut seeds_bin: Vec<u8> = Vec::with_capacity(num_buckets * 8);
    for &s in &seeds {
        seeds_bin.extend_from_slice(&s.to_le_bytes());
    }
    fs::write(out.join("seeds.bin"), &seeds_bin).unwrap();

    // emit verify hashes as [u64; TABLE_SIZE]
    let mut verify_bin: Vec<u8> = Vec::with_capacity(table_size * 8);
    for &v in &verify {
        verify_bin.extend_from_slice(&v.to_le_bytes());
    }
    fs::write(out.join("verify.bin"), &verify_bin).unwrap();

    let unk_str = format_quantized_param(&unk, global_min, quant_scale);
    let cls_str = format_quantized_param(&cls, global_min, quant_scale);
    let sep_str = format_quantized_param(&sep, global_min, quant_scale);

    let dtype_sz = dtype_sz();

    // emit constants
    fs::write(
        out.join("model_constants.rs"),
        format!(
            "
pub const DIMENSIONS: usize = {dims};\n\
pub type Param = [{dtype}; DIMENSIONS];\n\
pub const TABLE_SIZE: usize = {table_size};\n\
pub const TABLE_MASK: usize = {table_mask};\n\
pub const NUM_BUCKETS: usize = {num_buckets};\n\
pub const SLOT_SIZE: usize = 8 + DIMENSIONS * DTYPE_SIZE;\n\
pub const UNK: Param = {unk_str};\n\
pub const CLS: Param = {cls_str};\n\
pub const SEP: Param = {sep_str};\n\
pub const MAX_WORD_LEN: usize = 100;\n\
pub const DTYPE_SIZE: usize = {dtype_sz};\n\
pub const QUANT_MIN: f32 = {global_min:?};\n\
pub const QUANT_SCALE: f32 = {quant_scale:?};\n\
",
        ),
    )
    .unwrap();

    #[cfg(feature = "tests")]
    generate_testdata(&out, &weights, dims as usize, tokenizer_path);
}

fn phf_hash(key: &[u8], seed: u64) -> u64 {
    let mut h = seed ^ 0x517cc1b727220a95;
    for &b in key {
        h = (h ^ b as u64).wrapping_mul(0x2127599bf4325c37);
    }
    h ^ (h >> 32)
}

fn parse_safetensors(model_path: &PathBuf) -> (Vec<Vec<f32>>, u64) {
    println!("cargo:rerun-if-changed={}", model_path.to_str().unwrap());

    let data = fs::read(model_path).expect("failed to read safetensors file");
    let meta_len = u64::from_le_bytes(data[..8].try_into().unwrap()) as usize;
    let meta: serde_json::Value = serde_json::from_slice(&data[8..8 + meta_len]).unwrap();

    let tensor_key = meta
        .as_object()
        .and_then(|m| m.keys().find(|k| !k.starts_with("__")).cloned())
        .unwrap_or_else(|| "embedding.weight".to_string());

    let tensor = &meta[&tensor_key];
    let dtype = tensor["dtype"].as_str().unwrap_or("F32");
    let shape = tensor["shape"].as_array().unwrap();
    let params = shape[0].as_u64().unwrap();
    let full_dims = shape[1].as_u64().unwrap();

    let dims = full_dims.min(trunc_dims());

    let offsets = tensor["data_offsets"].as_array().unwrap();
    let start = 8 + meta_len + offsets[0].as_u64().unwrap() as usize;

    let src_elem_size: usize = match dtype {
        "F16" | "BF16" => 2,
        "F32" => 4,
        "F64" => 8,
        other => panic!("unsupported dtype: {other}"),
    };

    let row_src_bytes = full_dims as usize * src_elem_size;
    let mut tensors: Vec<Vec<f32>> = Vec::with_capacity(params as usize);
    let mut embedding: Vec<f32> = Vec::with_capacity(dims as usize);

    for row in 0..params as usize {
        let row_start = start + row * row_src_bytes;
        for col in 0..dims as usize {
            let elem_start = row_start + col * src_elem_size;
            let f32_val = match dtype {
                "F16" => {
                    let bits =
                        u16::from_le_bytes(data[elem_start..elem_start + 2].try_into().unwrap());
                    f16_to_f32(bits)
                }
                "BF16" => {
                    let bits =
                        u16::from_le_bytes(data[elem_start..elem_start + 2].try_into().unwrap());
                    bf16_to_f32(bits)
                }
                "F32" => f32::from_le_bytes(data[elem_start..elem_start + 4].try_into().unwrap()),
                "F64" => {
                    f64::from_le_bytes(data[elem_start..elem_start + 8].try_into().unwrap()) as f32
                }
                _ => unreachable!(),
            };
            embedding.push(f32_val);
        }
        tensors.push(embedding.clone());
        embedding.clear();
    }

    (tensors, dims)
}

fn f16_to_f32(h: u16) -> f32 {
    let sign = ((h >> 15) & 1) as u32;
    let exp = ((h >> 10) & 0x1F) as u32;
    let mant = (h & 0x3FF) as u32;

    let (f_exp, f_mant) = match exp {
        0 => {
            if mant == 0 {
                (0u32, 0u32)
            } else {
                let mut m = mant;
                let mut e = 1u32;
                while (m & 0x400) == 0 {
                    m <<= 1;
                    e += 1;
                }
                ((127 - 15 + 1 - e), (m & 0x3FF) << 13)
            }
        }
        31 => (255u32, mant << 13),
        _ => ((exp + 127 - 15), mant << 13),
    };

    f32::from_bits((sign << 31) | (f_exp << 23) | f_mant)
}

fn bf16_to_f32(b: u16) -> f32 {
    f32::from_bits((b as u32) << 16)
}

fn quantize_f32_to_bytes(values: &[f32], min: f32, scale: f32) -> Vec<u8> {
    if cfg!(feature = "quant-8") {
        values
            .iter()
            .map(|&v| (((v - min) / scale).round() as u8).clamp(0, 255))
            .flat_map(|q| q.to_le_bytes())
            .collect()
    } else if cfg!(feature = "quant-16") {
        values
            .iter()
            .map(|&v| (((v - min) / scale).round() as u16).clamp(0, 65535))
            .flat_map(|q| q.to_le_bytes())
            .collect()
    } else {
        values.iter().flat_map(|v| v.to_le_bytes()).collect()
    }
}

fn format_quantized_param(values: &[f32], min: f32, scale: f32) -> String {
    if cfg!(feature = "quant-8") {
        let q: Vec<u8> = values
            .iter()
            .map(|&v| ((v - min) / scale).round() as u8)
            .collect();
        format!("{q:?}")
    } else if cfg!(feature = "quant-16") {
        let q: Vec<u16> = values
            .iter()
            .map(|&v| ((v - min) / scale).round() as u16)
            .collect();
        format!("{q:?}")
    } else {
        format!("{values:?}")
    }
}

fn download_if_missing(path: &Path, url: &str) {
    if path.exists() {
        return;
    }
    eprintln!("cargo:warning=Downloading {}...", path.display());
    let resp = minreq::get(url).send().expect("download failed");
    assert!(resp.status_code == 200, "HTTP {}", resp.status_code);
    fs::write(path, resp.as_bytes()).unwrap();
}

fn trunc_dims() -> u64 {
    if cfg!(feature = "dim-1024") {
        1024
    } else if cfg!(feature = "dim-768") {
        768
    } else if cfg!(feature = "dim-512") {
        512
    } else if cfg!(feature = "dim-256") {
        256
    } else if cfg!(feature = "dim-128") {
        128
    } else if cfg!(feature = "dim-64") {
        64
    } else if cfg!(feature = "dim-32") {
        32
    } else {
        512
    }
}

fn dtype() -> &'static str {
    if cfg!(feature = "quant-16") {
        "u16"
    } else if cfg!(feature = "quant-8") {
        "u8"
    } else {
        "f32"
    }
}

fn dtype_sz() -> usize {
    if cfg!(feature = "quant-16") {
        2
    } else if cfg!(feature = "quant-8") {
        1
    } else {
        4
    }
}

fn shared_model_dir(out: &Path) -> PathBuf {
    let model_cache = env::var_os("ESE_MODEL_CACHE").map(PathBuf::from);
    let dir = resolve_shared_model_dir(
        out,
        model_cache.as_deref(),
        concat!(env!("CARGO_PKG_NAME"), "-cache"),
    )
    .unwrap_or_else(|error| panic!("{error}; set ESE_MODEL_CACHE to override"));
    fs::create_dir_all(&dir)
        .unwrap_or_else(|error| panic!("failed to create model cache {}: {error}", dir.display()));
    dir
}

#[cfg(feature = "tests")]
fn generate_testdata(out: &Path, weights: &[Vec<f32>], _dims: usize, tokenizer_path: &PathBuf) {
    let tokenizer =
        tokenizers::Tokenizer::from_file(tokenizer_path).expect("failed to load tokenizer");

    let inputs: Vec<&str> = vec![
        "Hello world",
        "The quick brown fox jumps over the lazy dog",
        "",
        "   ",
        "café résumé naïve",
        "你好世界",
        "Hello, world! How are you?",
        "UPPER CASE text",
        "multiple   spaces   between",
        "a]b[c{d}e",
        "\x00\x01\x02\x03",
    ];

    let mut code = String::from("const TEST_INPUTS: &[&str] = &[\n");
    let mut outputs: Vec<Vec<f32>> = Vec::new();

    for &input in &inputs {
        code.push_str(&format!("    {:?},\n", input));
        let tokens = tokenizer.encode(input, true).unwrap();
        let token_ids = tokens.get_ids();
        let scale = 1.0 / token_ids.len() as f32;
        let mut vector = vec![0.0f32; _dims];
        for &tok_id in token_ids {
            for (v, &w) in vector
                .iter_mut()
                .zip(unsafe { weights.get_unchecked(tok_id as usize) }.iter())
            {
                *v += w
            }
        }
        for v in vector.iter_mut() {
            *v *= scale
        }

        outputs.push(vector);
    }

    code.push_str("];\n\n");
    code.push_str(&format!(
        "const TEST_OUTPUTS: &[[f32; {}]; {}] = &[\n",
        _dims,
        outputs.len()
    ));
    for vec in &outputs {
        code.push_str(&format!("    {:?},\n", vec.as_slice()));
    }
    code.push_str("];\n");

    fs::write(out.join("optional_testdata.rs"), code).unwrap();
}
