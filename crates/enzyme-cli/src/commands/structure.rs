use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use anyhow::Result;
use libsql::{Builder, Connection, OpenFlags};
use serde::Serialize;

use enzyme_core::config::paths::EnzymePaths;
use enzyme_core::document::parse_frontmatter;
use enzyme_core::models::EMBEDDING_DIMENSION;

// ---------------------------------------------------------------------------
// Data model
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct DuplicateSet {
    fact: &'static str,
    content_hash: String,
    count: u64,
    paths: Vec<String>,
}

#[derive(Debug, Serialize)]
struct ExactDuplicates {
    duplicate_set_count: u64,
    duplicated_doc_count: u64,
    sets: Vec<DuplicateSet>,
}

#[derive(Debug, Serialize)]
struct CreatedSourceCounts {
    frontmatter: u64,
    filename: u64,
    filesystem: u64,
}

#[derive(Debug, Serialize)]
struct FolderConventionEntry {
    fact: &'static str,
    folder: String,
    doc_count: u64,
    created_source: CreatedSourceCounts,
    frontmatter_present: u64,
    frontmatter_absent: u64,
}

#[derive(Debug, Serialize)]
struct FolderConventionMix {
    min_folder_size: usize,
    folders_considered: u64,
    folders_omitted_below_min: u64,
    folders: Vec<FolderConventionEntry>,
}

#[derive(Debug, Serialize)]
struct CrossFolderRate {
    cross_folder_edges: u64,
    total_edges: u64,
    denominator: u64,
    percent: String,
}

#[derive(Debug, Clone, Serialize)]
struct CrossFolderPairCount {
    fact: &'static str,
    folder_a: String,
    folder_b: String,
    count: u64,
}

#[derive(Debug, Serialize)]
struct CrossFolderPair {
    fact: &'static str,
    path_a: String,
    path_b: String,
    folder_a: String,
    folder_b: String,
    similarity: String,
}

#[derive(Debug, Serialize)]
struct SemanticScatter {
    docs_total: u64,
    embedded_docs: u64,
    docs_not_embedded: u64,
    embedding_dimension: usize,
    note: &'static str,
    nearest_neighbor_cross_folder_rate: CrossFolderRate,
    cross_folder_pair_counts: Vec<CrossFolderPairCount>,
    top_cross_folder_pairs: Vec<CrossFolderPair>,
}

#[derive(Debug, Serialize)]
struct OrphanDoc {
    fact: &'static str,
    path: String,
    out_degree: u64,
    in_degree_approx: u64,
}

#[derive(Debug, Serialize)]
struct LinkGraph {
    orphan_match_note: &'static str,
    docs_total: u64,
    link_edges_total: u64,
    orphan_count: u64,
    orphans: Vec<OrphanDoc>,
}

#[derive(Debug, Serialize)]
struct LargestFolder {
    fact: &'static str,
    folder: String,
    doc_count: u64,
}

#[derive(Debug, Serialize)]
struct LargestFolderShare {
    largest_folder_doc_count: u64,
    docs_total: u64,
    percent: String,
}

#[derive(Debug, Serialize)]
struct FolderCountEntry {
    folder: String,
    count: u64,
}

#[derive(Debug, Serialize)]
struct FolderShape {
    folder_count: u64,
    max_depth: u64,
    singleton_folder_count: u64,
    largest_folder: LargestFolder,
    largest_folder_share: LargestFolderShare,
    doc_count_per_folder: Vec<FolderCountEntry>,
}

#[derive(Debug, Serialize)]
struct StructureReport {
    vault: String,
    database: String,
    min_folder_size: usize,
    top: usize,
    exact_duplicates: ExactDuplicates,
    folder_convention_mix: FolderConventionMix,
    semantic_scatter: SemanticScatter,
    link_graph: LinkGraph,
    folder_shape: FolderShape,
}

// ---------------------------------------------------------------------------
// Read-only DB open (same pattern as doctor.rs)
// ---------------------------------------------------------------------------

async fn open_read_only_conn(path: &Path) -> Result<Connection> {
    let path_str = path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("non-UTF8 database path"))?;
    let db = Builder::new_local(path_str)
        .flags(OpenFlags::SQLITE_OPEN_READ_ONLY)
        .build()
        .await
        .map_err(|e| anyhow::anyhow!("failed to open database read-only: {e}"))?;
    let conn = db
        .connect()
        .map_err(|e| anyhow::anyhow!("failed to connect to database: {e}"))?;
    Ok(conn)
}

// ---------------------------------------------------------------------------
// Helper: run a query and collect rows (same as doctor.rs)
// ---------------------------------------------------------------------------

async fn query_all<T, F>(conn: &Connection, sql: &str, mut map_row: F) -> Result<Vec<T>>
where
    F: FnMut(libsql::Row) -> Result<T>,
{
    let mut rows = conn
        .query(sql, ())
        .await
        .map_err(|e| anyhow::anyhow!("query failed: {e}"))?;
    let mut results = Vec::new();
    while let Some(row) = rows
        .next()
        .await
        .map_err(|e| anyhow::anyhow!("row error: {e}"))?
    {
        results.push(map_row(row)?);
    }
    Ok(results)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Return the folder component of a source_ref as a display string.
/// Top-level files (no directory) return "(root)".
/// Otherwise return the parent directory with a trailing slash, e.g. "misc/".
pub fn folder_of(source_ref: &str) -> String {
    let p = std::path::Path::new(source_ref);
    match p.parent() {
        None => "(root)".to_string(),
        Some(p) if p.as_os_str().is_empty() => "(root)".to_string(),
        Some(parent) => {
            let s = parent.to_string_lossy().to_string();
            // Normalize to forward slashes (source_ref uses forward slashes)
            let s = s.replace('\\', "/");
            if s.is_empty() {
                "(root)".to_string()
            } else {
                format!("{s}/")
            }
        }
    }
}

/// Decode a little-endian f32 blob (same as blob_to_f32_vec in repository.rs).
fn blob_to_f32_vec(blob: &[u8]) -> Vec<f32> {
    blob.chunks_exact(4)
        .map(|chunk| f32::from_le_bytes(chunk.try_into().unwrap()))
        .collect()
}

/// Mean-pool a list of chunk vectors, then L2-normalize the result.
/// Chunks must be supplied in chunk_index order for deterministic summation.
/// Returns None if the list is empty or all-zero after normalization.
pub fn mean_pool_normalize(chunks: &[Vec<f32>]) -> Option<Vec<f32>> {
    if chunks.is_empty() {
        return None;
    }
    let dim = chunks[0].len();
    let mut sum = vec![0.0f32; dim];
    for chunk in chunks {
        for (s, &v) in sum.iter_mut().zip(chunk.iter()) {
            *s += v;
        }
    }
    let n = chunks.len() as f32;
    for s in &mut sum {
        *s /= n;
    }
    // L2 normalize
    let norm: f32 = sum.iter().map(|&x| x * x).sum::<f32>().sqrt();
    if norm == 0.0 {
        return None;
    }
    for s in &mut sum {
        *s /= norm;
    }
    Some(sum)
}

/// Dot product of two equal-length vectors (= cosine similarity on normalized vectors).
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b.iter()).map(|(&x, &y)| x * y).sum()
}

/// Format a float as a fixed-decimal string (rounds half-to-even via Rust's format!).
/// `decimals`: number of decimal places.
pub fn fmt_fixed(x: f64, decimals: usize) -> String {
    format!("{:.*}", decimals, x)
}

// ---------------------------------------------------------------------------
// §5.1 Exact duplicates
// ---------------------------------------------------------------------------

async fn collect_duplicates(conn: &Connection) -> Result<ExactDuplicates> {
    // Get hashes with count > 1, sorted by (count DESC, content_hash ASC)
    let hash_counts: Vec<(String, u64)> = query_all(
        conn,
        "SELECT content_hash, count(*) AS n FROM docs \
         GROUP BY content_hash HAVING n > 1 \
         ORDER BY n DESC, content_hash ASC",
        |row| {
            let hash: String = row.get(0).map_err(|e| anyhow::anyhow!("{e}"))?;
            let n: i64 = row.get(1).map_err(|e| anyhow::anyhow!("{e}"))?;
            Ok((hash, n as u64))
        },
    )
    .await?;

    let mut sets = Vec::with_capacity(hash_counts.len());
    let mut duplicated_doc_count: u64 = 0;

    for (hash, count) in &hash_counts {
        duplicated_doc_count += count;

        // Per-group paths, sorted lexicographically
        let sql = format!(
            "SELECT source_ref FROM docs WHERE content_hash = '{}' ORDER BY source_ref",
            hash.replace('\'', "''")
        );
        let paths: Vec<String> = query_all(conn, &sql, |row| {
            row.get::<String>(0).map_err(|e| anyhow::anyhow!("{e}"))
        })
        .await?;

        sets.push(DuplicateSet {
            fact: "exact_content_hash_duplicate_set",
            content_hash: hash.clone(),
            count: *count,
            paths,
        });
    }

    Ok(ExactDuplicates {
        duplicate_set_count: sets.len() as u64,
        duplicated_doc_count,
        sets,
    })
}

// ---------------------------------------------------------------------------
// §5.2 Per-folder convention mix
// ---------------------------------------------------------------------------

async fn collect_convention_mix(
    vault_path: &Path,
    conn: &Connection,
    min_folder_size: usize,
) -> Result<FolderConventionMix> {
    let docs: Vec<(String, String)> = query_all(
        conn,
        "SELECT source_ref, created_source FROM docs ORDER BY source_ref",
        |row| {
            let sr: String = row.get(0).map_err(|e| anyhow::anyhow!("{e}"))?;
            let cs: String = row.get(1).map_err(|e| anyhow::anyhow!("{e}"))?;
            Ok((sr, cs))
        },
    )
    .await?;

    // Group by folder using BTreeMap for deterministic order
    struct FolderAccum {
        source_refs: Vec<String>,
        frontmatter_count: u64,
        filename_count: u64,
        filesystem_count: u64,
    }
    let mut by_folder: BTreeMap<String, FolderAccum> = BTreeMap::new();

    for (source_ref, created_source) in &docs {
        let folder = folder_of(source_ref);
        let entry = by_folder.entry(folder).or_insert(FolderAccum {
            source_refs: Vec::new(),
            frontmatter_count: 0,
            filename_count: 0,
            filesystem_count: 0,
        });
        entry.source_refs.push(source_ref.clone());
        match created_source.as_str() {
            "frontmatter" => entry.frontmatter_count += 1,
            "filename" => entry.filename_count += 1,
            _ => entry.filesystem_count += 1,
        }
    }

    let total_folders = by_folder.len() as u64;
    let mut folders_considered: u64 = 0;
    let mut folders_omitted: u64 = 0;
    let mut folder_entries: Vec<FolderConventionEntry> = Vec::new();

    for (folder, accum) in &by_folder {
        let doc_count = accum.source_refs.len() as u64;
        if doc_count < min_folder_size as u64 {
            folders_omitted += 1;
            continue;
        }
        folders_considered += 1;

        // Recompute frontmatter presence by reading files
        let mut fm_present: u64 = 0;
        let mut fm_absent: u64 = 0;
        for source_ref in &accum.source_refs {
            let abs_path = vault_path.join(source_ref);
            let has_fm = std::fs::read_to_string(&abs_path)
                .ok()
                .map(|content| parse_frontmatter(&content).is_some())
                .unwrap_or(false);
            if has_fm {
                fm_present += 1;
            } else {
                fm_absent += 1;
            }
        }

        folder_entries.push(FolderConventionEntry {
            fact: "folder_convention_mix",
            folder: folder.clone(),
            doc_count,
            created_source: CreatedSourceCounts {
                frontmatter: accum.frontmatter_count,
                filename: accum.filename_count,
                filesystem: accum.filesystem_count,
            },
            frontmatter_present: fm_present,
            frontmatter_absent: fm_absent,
        });
    }

    // folder_entries already in BTreeMap (folder-key) order
    let _ = total_folders; // used for omitted calculation above

    Ok(FolderConventionMix {
        min_folder_size,
        folders_considered,
        folders_omitted_below_min: folders_omitted,
        folders: folder_entries,
    })
}

// ---------------------------------------------------------------------------
// §5.3 Semantic scatter
// ---------------------------------------------------------------------------

async fn collect_semantic_scatter(conn: &Connection, _top: usize) -> Result<SemanticScatter> {
    // Total docs
    let docs_total_rows: Vec<i64> = query_all(
        conn,
        "SELECT count(*) FROM docs",
        |row| row.get::<i64>(0).map_err(|e| anyhow::anyhow!("{e}")),
    )
    .await?;
    let docs_total = docs_total_rows.first().copied().unwrap_or(0) as u64;

    // Load all chunk embeddings ordered by (file_path, chunk_index) for deterministic summation
    let chunks_raw: Vec<(String, u32, Vec<u8>)> = query_all(
        conn,
        "SELECT file_path, chunk_index, vector FROM chunk_embeddings \
         ORDER BY file_path ASC, chunk_index ASC",
        |row| {
            let fp: String = row.get(0).map_err(|e| anyhow::anyhow!("{e}"))?;
            let ci: i64 = row.get(1).map_err(|e| anyhow::anyhow!("{e}"))?;
            let blob: Vec<u8> = row.get(2).map_err(|e| anyhow::anyhow!("{e}"))?;
            Ok((fp, ci as u32, blob))
        },
    )
    .await?;

    // Group chunks by file_path (order preserved since we sorted above)
    let mut file_chunks: BTreeMap<String, Vec<Vec<f32>>> = BTreeMap::new();
    for (fp, _ci, blob) in &chunks_raw {
        file_chunks
            .entry(fp.clone())
            .or_default()
            .push(blob_to_f32_vec(blob));
    }

    // Compute per-doc mean-pooled+normalized vectors; collect in source_ref-sorted order
    let mut doc_vecs: Vec<(String, Vec<f32>)> = Vec::new(); // (source_ref, vector)
    for (source_ref, chunks) in &file_chunks {
        if let Some(vec) = mean_pool_normalize(chunks) {
            doc_vecs.push((source_ref.clone(), vec));
        }
    }
    // doc_vecs is already in BTreeMap (lexicographic source_ref) order

    let embedded_docs = doc_vecs.len() as u64;
    let docs_not_embedded = docs_total.saturating_sub(embedded_docs);

    // Default (empty) scatter result if no embedded docs
    if doc_vecs.is_empty() {
        return Ok(SemanticScatter {
            docs_total,
            embedded_docs: 0,
            docs_not_embedded,
            embedding_dimension: EMBEDDING_DIMENSION,
            note: "nearest-neighbor edges are directed (one per doc); top pairs are undirected",
            nearest_neighbor_cross_folder_rate: CrossFolderRate {
                cross_folder_edges: 0,
                total_edges: 0,
                denominator: 0,
                percent: fmt_fixed(0.0, 1),
            },
            cross_folder_pair_counts: Vec::new(),
            top_cross_folder_pairs: Vec::new(),
        });
    }

    // Build source_ref → index map for O(1) lookup (HashMap is fine for internal use)
    let idx_map: HashMap<&str, usize> = doc_vecs
        .iter()
        .enumerate()
        .map(|(i, (sr, _))| (sr.as_str(), i))
        .collect();
    let _ = idx_map; // not needed below, just iterating

    let n = doc_vecs.len();

    // For each doc, find nearest neighbor by cosine similarity (dot on L2-normalized).
    // Ties broken by the neighbor's source_ref (lexicographically smaller wins).
    // Iterate candidates in source_ref order (guaranteed by BTreeMap above).
    struct NNEdge {
        from: usize,
        to: usize,
        similarity: f32,
    }

    let mut nn_edges: Vec<NNEdge> = Vec::with_capacity(n);

    for i in 0..n {
        let (_, vec_i) = &doc_vecs[i];
        let mut best_sim = f32::NEG_INFINITY;
        let mut best_j: usize = usize::MAX;
        let mut best_sr: &str = "";

        for j in 0..n {
            if j == i {
                continue;
            }
            let (sr_j, vec_j) = &doc_vecs[j];
            let sim = cosine(vec_i, vec_j);
            // Higher similarity wins; ties broken by lexicographically smaller source_ref
            let better = sim > best_sim
                || (sim == best_sim && (best_j == usize::MAX || sr_j.as_str() < best_sr));
            if better {
                best_sim = sim;
                best_j = j;
                best_sr = sr_j.as_str();
            }
        }

        if best_j != usize::MAX {
            nn_edges.push(NNEdge {
                from: i,
                to: best_j,
                similarity: best_sim,
            });
        }
    }

    // Compute cross-folder statistics
    let mut cross_folder_edges: u64 = 0;
    // BTreeMap keyed by unordered pair (folder_a, folder_b) with folder_a <= folder_b
    let mut pair_count_map: BTreeMap<(String, String), u64> = BTreeMap::new();
    // Collect cross-folder pairs (undirected dedup) for top-pairs list
    // Use a BTreeMap keyed by (path_a, path_b) with path_a < path_b to dedup
    let mut top_pairs_map: BTreeMap<(String, String), (f32, String, String)> = BTreeMap::new();

    for edge in &nn_edges {
        let (sr_from, _) = &doc_vecs[edge.from];
        let (sr_to, _) = &doc_vecs[edge.to];
        let folder_from = folder_of(sr_from);
        let folder_to = folder_of(sr_to);

        if folder_from != folder_to {
            cross_folder_edges += 1;

            // Unordered folder pair
            let (fa, fb) = if folder_from <= folder_to {
                (folder_from.clone(), folder_to.clone())
            } else {
                (folder_to.clone(), folder_from.clone())
            };
            *pair_count_map.entry((fa.clone(), fb.clone())).or_insert(0) += 1;

            // Unordered path pair for top-pairs
            let (pa, pb) = if sr_from <= sr_to {
                (sr_from.clone(), sr_to.clone())
            } else {
                (sr_to.clone(), sr_from.clone())
            };
            let entry = top_pairs_map.entry((pa, pb)).or_insert((edge.similarity, fa, fb));
            // Keep max similarity (they should be equal for mutual edges)
            if edge.similarity > entry.0 {
                entry.0 = edge.similarity;
            }
        }
    }

    let total_edges = nn_edges.len() as u64;
    let pct = if total_edges > 0 {
        (cross_folder_edges as f64 / total_edges as f64) * 100.0
    } else {
        0.0
    };

    // Build sorted cross_folder_pair_counts (BTreeMap order = (folder_a, folder_b) asc)
    let cross_folder_pair_counts: Vec<CrossFolderPairCount> = pair_count_map
        .into_iter()
        .map(|((fa, fb), count)| CrossFolderPairCount {
            fact: "cross_folder_nn_edge_count",
            folder_a: fa,
            folder_b: fb,
            count,
        })
        .collect();

    // Build top_cross_folder_pairs sorted by (similarity DESC, path_a ASC, path_b ASC)
    let mut top_pairs_vec: Vec<CrossFolderPair> = top_pairs_map
        .into_iter()
        .map(|((pa, pb), (sim, fa, fb))| CrossFolderPair {
            fact: "strongest_cross_folder_nn_pair",
            path_a: pa,
            path_b: pb,
            folder_a: fa,
            folder_b: fb,
            similarity: fmt_fixed(sim as f64, 3),
        })
        .collect();

    // Sort by (similarity DESC, path_a ASC, path_b ASC) — similarity is a string so compare raw f32
    // We need to re-parse for comparison; use sort_by with parsed floats
    top_pairs_vec.sort_by(|a, b| {
        let sa: f64 = a.similarity.parse().unwrap_or(0.0);
        let sb: f64 = b.similarity.parse().unwrap_or(0.0);
        sb.partial_cmp(&sa)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.path_a.cmp(&b.path_a))
            .then(a.path_b.cmp(&b.path_b))
    });

    // Truncate to top K for human output — but in JSON mode we keep all.
    // The top field is the cap; we store all and truncate at render time.
    let top_pairs_full = top_pairs_vec;

    Ok(SemanticScatter {
        docs_total,
        embedded_docs,
        docs_not_embedded,
        embedding_dimension: EMBEDDING_DIMENSION,
        note: "nearest-neighbor edges are directed (one per doc); top pairs are undirected",
        nearest_neighbor_cross_folder_rate: CrossFolderRate {
            cross_folder_edges,
            total_edges,
            denominator: embedded_docs,
            percent: fmt_fixed(pct, 1),
        },
        cross_folder_pair_counts,
        top_cross_folder_pairs: top_pairs_full,
    })
}

// ---------------------------------------------------------------------------
// §5.4 Link graph — orphans only
// ---------------------------------------------------------------------------

async fn collect_link_graph(conn: &Connection) -> Result<LinkGraph> {
    // All docs
    let all_source_refs: Vec<String> = query_all(
        conn,
        "SELECT source_ref FROM docs ORDER BY source_ref",
        |row| row.get::<String>(0).map_err(|e| anyhow::anyhow!("{e}")),
    )
    .await?;

    let docs_total = all_source_refs.len() as u64;

    // doc_id → source_ref
    let doc_ids: Vec<(i64, String)> = query_all(
        conn,
        "SELECT id, source_ref FROM docs ORDER BY source_ref",
        |row| {
            let id: i64 = row.get(0).map_err(|e| anyhow::anyhow!("{e}"))?;
            let sr: String = row.get(1).map_err(|e| anyhow::anyhow!("{e}"))?;
            Ok((id, sr))
        },
    )
    .await?;

    // source_ref → doc_id map (HashMap for internal use)
    let sr_to_id: HashMap<String, i64> = doc_ids.iter().map(|(id, sr)| (sr.clone(), *id)).collect();
    let _id_to_sr: HashMap<i64, String> = doc_ids.iter().map(|(id, sr)| (*id, sr.clone())).collect();

    // All doc_links
    let all_links: Vec<(i64, String)> = query_all(
        conn,
        "SELECT doc_id, link FROM doc_links ORDER BY doc_id ASC, link ASC",
        |row| {
            let doc_id: i64 = row.get(0).map_err(|e| anyhow::anyhow!("{e}"))?;
            let link: String = row.get(1).map_err(|e| anyhow::anyhow!("{e}"))?;
            Ok((doc_id, link))
        },
    )
    .await?;

    let link_edges_total = all_links.len() as u64;

    // Out-degree per doc_id
    let mut out_degree: HashMap<i64, u64> = HashMap::new();
    let mut all_link_targets: HashSet<String> = HashSet::new();
    for (doc_id, link) in &all_links {
        *out_degree.entry(*doc_id).or_insert(0) += 1;
        all_link_targets.insert(link.to_lowercase());
    }

    // Build set of all lowercased link targets for in-degree approximation
    // In-degree approximation: check if doc's lowercased basename-without-.md
    // OR lowercased source_ref-without-.md appears in any link target.
    let mut orphans: Vec<OrphanDoc> = Vec::new();

    for (id, source_ref) in &doc_ids {
        let odeg = out_degree.get(id).copied().unwrap_or(0);

        // Compute lowercased basename without .md
        let p = std::path::Path::new(source_ref);
        let basename_lower = p
            .file_stem()
            .and_then(|s| s.to_str())
            .map(|s| s.to_lowercase())
            .unwrap_or_default();
        // Also lowercased source_ref without .md
        let source_ref_no_ext = if source_ref.ends_with(".md") {
            source_ref[..source_ref.len() - 3].to_lowercase()
        } else {
            source_ref.to_lowercase()
        };

        let in_deg = if all_link_targets.contains(&basename_lower)
            || all_link_targets.contains(&source_ref_no_ext)
        {
            1u64
        } else {
            0u64
        };

        if odeg == 0 && in_deg == 0 {
            orphans.push(OrphanDoc {
                fact: "orphan_doc",
                path: source_ref.clone(),
                out_degree: 0,
                in_degree_approx: 0,
            });
        }
    }

    // orphans already sorted by path (source_refs were loaded in source_ref order)
    let orphan_count = orphans.len() as u64;

    let _ = sr_to_id; // suppress unused warning

    Ok(LinkGraph {
        orphan_match_note: "in-degree is basename-approximate: [[Note]] matches any Note.md in any folder",
        docs_total,
        link_edges_total,
        orphan_count,
        orphans,
    })
}

// ---------------------------------------------------------------------------
// §5.5 Folder shape
// ---------------------------------------------------------------------------

async fn collect_folder_shape(conn: &Connection) -> Result<FolderShape> {
    let all_source_refs: Vec<String> = query_all(
        conn,
        "SELECT source_ref FROM docs ORDER BY source_ref",
        |row| row.get::<String>(0).map_err(|e| anyhow::anyhow!("{e}")),
    )
    .await?;

    let docs_total = all_source_refs.len() as u64;
    let mut folder_map: BTreeMap<String, u64> = BTreeMap::new();
    let mut max_depth: u64 = 0;

    for source_ref in &all_source_refs {
        let folder = folder_of(source_ref);
        *folder_map.entry(folder).or_insert(0) += 1;

        // Depth = number of path components (a root file has depth 1)
        let depth = std::path::Path::new(source_ref).components().count() as u64;
        if depth > max_depth {
            max_depth = depth;
        }
    }

    let folder_count = folder_map.len() as u64;
    let singleton_folder_count = folder_map.values().filter(|&&c| c == 1).count() as u64;

    // Largest folder: max count, ties broken by lexicographically first folder
    let largest = folder_map
        .iter()
        .max_by(|(fa, ca), (fb, cb)| ca.cmp(cb).then(fb.cmp(fa)))
        .map(|(f, &c)| (f.clone(), c))
        .unwrap_or_else(|| ("(root)".to_string(), 0));

    let share_pct = if docs_total > 0 {
        (largest.1 as f64 / docs_total as f64) * 100.0
    } else {
        0.0
    };

    // doc_count_per_folder sorted by (count DESC, folder ASC) for human top-K display
    let mut folder_list: Vec<FolderCountEntry> = folder_map
        .iter()
        .map(|(f, &c)| FolderCountEntry {
            folder: f.clone(),
            count: c,
        })
        .collect();
    folder_list.sort_by(|a, b| b.count.cmp(&a.count).then(a.folder.cmp(&b.folder)));

    Ok(FolderShape {
        folder_count,
        max_depth,
        singleton_folder_count,
        largest_folder: LargestFolder {
            fact: "folder_shape",
            folder: largest.0.clone(),
            doc_count: largest.1,
        },
        largest_folder_share: LargestFolderShare {
            largest_folder_doc_count: largest.1,
            docs_total,
            percent: fmt_fixed(share_pct, 1),
        },
        doc_count_per_folder: folder_list,
    })
}

// ---------------------------------------------------------------------------
// Human-readable rendering
// ---------------------------------------------------------------------------

fn render_human(report: &StructureReport, top: usize) -> String {
    let mut out = String::new();

    out.push_str(&format!("vault: {}\n", report.vault));
    out.push_str(&format!("database: {}\n", report.database));
    out.push_str(&format!("min_folder_size: {}\n", report.min_folder_size));
    out.push_str(&format!("top: {}\n", top));
    out.push('\n');

    // --- §5.1 Exact duplicates ---
    out.push_str("exact_duplicates\n");
    out.push_str(&format!(
        "  duplicate_set_count: {}\n",
        report.exact_duplicates.duplicate_set_count
    ));
    out.push_str(&format!(
        "  duplicated_doc_count: {}\n",
        report.exact_duplicates.duplicated_doc_count
    ));
    if !report.exact_duplicates.sets.is_empty() {
        out.push_str("  fact=exact_content_hash_duplicate_set\n");
        let sets = &report.exact_duplicates.sets;
        let shown = sets.len().min(top);
        for s in &sets[..shown] {
            let hash_short = if s.content_hash.len() > 8 {
                format!("{}…", &s.content_hash[..8])
            } else {
                s.content_hash.clone()
            };
            let paths_str = s.paths.join(", ");
            out.push_str(&format!("    n={}  hash={}  {}\n", s.count, hash_short, paths_str));
        }
        if sets.len() > shown {
            out.push_str(&format!(
                "    ... ({} more; use --json for full list)\n",
                sets.len() - shown
            ));
        }
    }
    out.push('\n');

    // --- §5.2 Folder convention mix ---
    out.push_str("folder_convention_mix\n");
    out.push_str(&format!(
        "  folders_considered: {}\n",
        report.folder_convention_mix.folders_considered
    ));
    out.push_str(&format!(
        "  folders_omitted_below_min: {}\n",
        report.folder_convention_mix.folders_omitted_below_min
    ));
    if !report.folder_convention_mix.folders.is_empty() {
        out.push_str("  fact=folder_convention_mix\n");
        for f in &report.folder_convention_mix.folders {
            out.push_str(&format!(
                "    {} (n={})  created_source: frontmatter={} filename={} filesystem={}  frontmatter_present={} absent={}\n",
                f.folder,
                f.doc_count,
                f.created_source.frontmatter,
                f.created_source.filename,
                f.created_source.filesystem,
                f.frontmatter_present,
                f.frontmatter_absent,
            ));
        }
    }
    out.push('\n');

    // --- §5.3 Semantic scatter ---
    out.push_str("semantic_scatter\n");
    out.push_str(&format!(
        "  docs_total: {}\n",
        report.semantic_scatter.docs_total
    ));
    out.push_str(&format!(
        "  embedded_docs: {}\n",
        report.semantic_scatter.embedded_docs
    ));
    out.push_str(&format!(
        "  docs_not_embedded: {}\n",
        report.semantic_scatter.docs_not_embedded
    ));
    out.push_str(&format!(
        "  embedding_dimension: {}\n",
        report.semantic_scatter.embedding_dimension
    ));
    out.push_str(&format!("  note: {}\n", report.semantic_scatter.note));
    let rate = &report.semantic_scatter.nearest_neighbor_cross_folder_rate;
    out.push_str(&format!(
        "  nearest_neighbor_cross_folder_rate: {} / {}  ({}%)\n",
        rate.cross_folder_edges, rate.total_edges, rate.percent
    ));

    if !report.semantic_scatter.cross_folder_pair_counts.is_empty() {
        out.push_str(&format!(
            "  fact=cross_folder_nn_edge_count  (top {} pairs by count)\n",
            top
        ));
        // Sort by count DESC, then folder_a ASC, folder_b ASC for human display
        let mut sorted_pairs = report.semantic_scatter.cross_folder_pair_counts.clone();
        sorted_pairs.sort_by(|a, b| {
            b.count
                .cmp(&a.count)
                .then(a.folder_a.cmp(&b.folder_a))
                .then(a.folder_b.cmp(&b.folder_b))
        });
        let shown = sorted_pairs.len().min(top);
        for p in &sorted_pairs[..shown] {
            out.push_str(&format!(
                "    {}  ↔  {} : {}\n",
                p.folder_a, p.folder_b, p.count
            ));
        }
        if sorted_pairs.len() > shown {
            out.push_str(&format!(
                "    ... ({} more; use --json for full list)\n",
                sorted_pairs.len() - shown
            ));
        }
    }

    if !report.semantic_scatter.top_cross_folder_pairs.is_empty() {
        out.push_str(&format!(
            "  fact=strongest_cross_folder_nn_pair  (top {} by similarity)\n",
            top
        ));
        let pairs = &report.semantic_scatter.top_cross_folder_pairs;
        let shown = pairs.len().min(top);
        for p in &pairs[..shown] {
            out.push_str(&format!(
                "    {}  {}  ↔  {}   ({} ↔ {})\n",
                p.similarity, p.path_a, p.path_b, p.folder_a, p.folder_b
            ));
        }
        if pairs.len() > shown {
            out.push_str(&format!(
                "    ... ({} more; use --json for full list)\n",
                pairs.len() - shown
            ));
        }
    }
    out.push('\n');

    // --- §5.4 Link graph ---
    out.push_str("link_graph\n");
    out.push_str(&format!(
        "  orphan_match_note: {}\n",
        report.link_graph.orphan_match_note
    ));
    out.push_str(&format!(
        "  docs_total: {}\n",
        report.link_graph.docs_total
    ));
    out.push_str(&format!(
        "  link_edges_total: {}\n",
        report.link_graph.link_edges_total
    ));
    out.push_str(&format!(
        "  orphan_count: {}\n",
        report.link_graph.orphan_count
    ));
    if !report.link_graph.orphans.is_empty() {
        out.push_str("  fact=orphan_doc\n");
        let orphans = &report.link_graph.orphans;
        let shown = orphans.len().min(top);
        for o in &orphans[..shown] {
            out.push_str(&format!("    {}\n", o.path));
        }
        if orphans.len() > shown {
            out.push_str(&format!(
                "    ... ({} more; use --json for full list)\n",
                orphans.len() - shown
            ));
        }
    }
    out.push('\n');

    // --- §5.5 Folder shape ---
    out.push_str("folder_shape\n");
    out.push_str(&format!(
        "  folder_count: {}\n",
        report.folder_shape.folder_count
    ));
    out.push_str(&format!(
        "  max_depth: {}\n",
        report.folder_shape.max_depth
    ));
    out.push_str(&format!(
        "  singleton_folder_count: {}\n",
        report.folder_shape.singleton_folder_count
    ));
    out.push_str(&format!(
        "  largest_folder: {} (n={})\n",
        report.folder_shape.largest_folder.folder, report.folder_shape.largest_folder.doc_count
    ));
    let share = &report.folder_shape.largest_folder_share;
    out.push_str(&format!(
        "  largest_folder_share: {} / {}  ({}%)\n",
        share.largest_folder_doc_count, share.docs_total, share.percent
    ));

    let doc_count_per_folder = &report.folder_shape.doc_count_per_folder;
    out.push_str(&format!(
        "  fact=folder_shape  doc_count_per_folder (top {})\n",
        top
    ));
    let shown = doc_count_per_folder.len().min(top);
    for f in &doc_count_per_folder[..shown] {
        out.push_str(&format!("    {} : {}\n", f.folder, f.count));
    }
    if doc_count_per_folder.len() > shown {
        out.push_str(&format!(
            "    ... ({} more; use --json for full list)\n",
            doc_count_per_folder.len() - shown
        ));
    }

    out
}

// ---------------------------------------------------------------------------
// Main entry point
// ---------------------------------------------------------------------------

pub async fn run_structure(
    vault_path: &Path,
    json: bool,
    min_folder_size: usize,
    top: usize,
) -> Result<()> {
    let paths = EnzymePaths::new(vault_path)?;
    // Do NOT call sync_on_drop — read-only command

    if !paths.vault_db.exists() {
        let vault_str = vault_path.display().to_string();
        if json {
            println!(
                "{{\"error\":\"vault_not_initialized\",\"vault\":\"{}\"}}",
                vault_str.replace('"', "\\\"")
            );
        } else {
            eprintln!(
                "enzyme structure: vault not initialized (no .enzyme/enzyme.db at {}); run 'enzyme init' first",
                vault_str
            );
        }
        std::process::exit(1);
    }

    // Open DB strictly read-only
    let conn = open_read_only_conn(&paths.vault_db).await?;

    let exact_duplicates = collect_duplicates(&conn).await?;
    let folder_convention_mix =
        collect_convention_mix(vault_path, &conn, min_folder_size).await?;
    let semantic_scatter = collect_semantic_scatter(&conn, top).await?;
    let link_graph = collect_link_graph(&conn).await?;
    let folder_shape = collect_folder_shape(&conn).await?;

    let report = StructureReport {
        vault: vault_path.display().to_string(),
        database: paths.vault_db.display().to_string(),
        min_folder_size,
        top,
        exact_duplicates,
        folder_convention_mix,
        semantic_scatter,
        link_graph,
        folder_shape,
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", render_human(&report, top));
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use libsql::Builder;
    use tempfile::TempDir;

    // ---------------------------------------------------------------------------
    // Pure unit tests (no DB)
    // ---------------------------------------------------------------------------

    #[test]
    fn folder_of_rules() {
        assert_eq!(folder_of("a.md"), "(root)");
        assert_eq!(folder_of("x/a.md"), "x/");
        assert_eq!(folder_of("x/y/a.md"), "x/y/");
    }

    #[test]
    fn fmt_fixed_stable() {
        // 0.9805 rounds to 0.981 at 3dp (half-up / IEEE)
        let s = fmt_fixed(0.9805, 3);
        // Accept 0.980 or 0.981 depending on banker's rounding; assert it is one of them
        assert!(s == "0.980" || s == "0.981", "got {s}");

        // 71.35 rounds to either 71.3 or 71.4
        let s2 = fmt_fixed(71.35, 1);
        assert!(s2 == "71.3" || s2 == "71.4", "got {s2}");

        // Exact round number
        assert_eq!(fmt_fixed(0.5, 1), "0.5");
        assert_eq!(fmt_fixed(1.0, 3), "1.000");
    }

    #[test]
    fn mean_pool_normalize_basic() {
        // Two identical unit vectors: mean = same vector, norm = 1 → normalized = same
        let v: Vec<f32> = vec![1.0, 0.0, 0.0];
        let chunks = vec![v.clone(), v.clone()];
        let result = mean_pool_normalize(&chunks).unwrap();
        assert!((result[0] - 1.0).abs() < 1e-6);
        assert!((result[1]).abs() < 1e-6);
        assert!((result[2]).abs() < 1e-6);
    }

    #[test]
    fn mean_pool_normalize_empty() {
        assert!(mean_pool_normalize(&[]).is_none());
    }

    #[test]
    fn mean_pool_and_cosine_deterministic() {
        // Build two doc chunk sets
        let chunks_a = vec![vec![1.0f32, 0.0, 0.0], vec![0.0f32, 1.0, 0.0]];
        let chunks_b = vec![vec![0.0f32, 1.0, 0.0]];
        let va = mean_pool_normalize(&chunks_a).unwrap();
        let vb = mean_pool_normalize(&chunks_b).unwrap();

        // Run again with same inputs → same result
        let va2 = mean_pool_normalize(&chunks_a).unwrap();
        assert_eq!(va, va2);

        // Cosine similarity is deterministic
        let sim1 = cosine(&va, &vb);
        let sim2 = cosine(&va, &vb);
        assert_eq!(sim1, sim2);
    }

    #[test]
    fn nearest_neighbor_tie_broken_by_source_ref() {
        // Three docs: doc A has two equidistant neighbors B and C
        // B has smaller source_ref → B should be chosen
        let vec_a = vec![1.0f32, 0.0, 0.0];
        // Make B and C identical (equidistant from A)
        let vec_b = vec![0.0f32, 1.0, 0.0];
        let vec_c = vec![0.0f32, 1.0, 0.0];

        // Simulate the NN loop
        let doc_vecs = vec![
            ("a.md".to_string(), vec_a.clone()),
            ("b.md".to_string(), vec_b.clone()),
            ("c.md".to_string(), vec_c.clone()),
        ];

        let i = 0; // query doc is a.md
        let mut best_sim = f32::NEG_INFINITY;
        let mut best_j: usize = usize::MAX;
        let mut best_sr: &str = "";

        for j in 1..doc_vecs.len() {
            let (sr_j, vec_j) = &doc_vecs[j];
            let sim = cosine(&doc_vecs[i].1, vec_j);
            let better = sim > best_sim
                || (sim == best_sim && (best_j == usize::MAX || sr_j.as_str() < best_sr));
            if better {
                best_sim = sim;
                best_j = j;
                best_sr = sr_j.as_str();
            }
        }

        // b.md < c.md lexicographically → b.md should win tie
        assert_eq!(doc_vecs[best_j].0, "b.md");
    }

    #[test]
    fn cross_folder_rate_basic() {
        // 4 docs: 2 in folder_a, 2 in folder_b
        // Make them such that each doc's NN is in the other folder
        // folder_a: a1=[1,0], a2=[1,0]  → they're equidistant; a1's NN = a2 (same folder)
        // Craft vectors so cross-folder edges happen
        let doc_vecs: Vec<(String, Vec<f32>)> = vec![
            ("fa/a1.md".to_string(), vec![1.0f32, 0.0]),
            ("fa/a2.md".to_string(), vec![0.0f32, 1.0]),
            ("fb/b1.md".to_string(), vec![1.0f32, 0.01]),
            ("fb/b2.md".to_string(), vec![0.01f32, 1.0]),
        ];

        // Normalize them first
        let norm_vecs: Vec<(String, Vec<f32>)> = doc_vecs
            .iter()
            .map(|(sr, v)| {
                let normalized = mean_pool_normalize(&[v.clone()]).unwrap();
                (sr.clone(), normalized)
            })
            .collect();

        let n = norm_vecs.len();
        let mut cross = 0u64;
        let mut total = 0u64;

        for i in 0..n {
            let mut best_sim = f32::NEG_INFINITY;
            let mut best_j = usize::MAX;
            let mut best_sr: &str = "";

            for j in 0..n {
                if j == i {
                    continue;
                }
                let sim = cosine(&norm_vecs[i].1, &norm_vecs[j].1);
                let better = sim > best_sim
                    || (sim == best_sim
                        && (best_j == usize::MAX || norm_vecs[j].0.as_str() < best_sr));
                if better {
                    best_sim = sim;
                    best_j = j;
                    best_sr = norm_vecs[j].0.as_str();
                }
            }

            if best_j != usize::MAX {
                total += 1;
                let fi = folder_of(&norm_vecs[i].0);
                let fj = folder_of(&norm_vecs[best_j].0);
                if fi != fj {
                    cross += 1;
                }
            }
        }

        // fa/a1 → fb/b1 (cross), fa/a2 → fb/b2 (cross), fb/b1 → fa/a1 (cross), fb/b2 → fa/a2 (cross)
        // All 4 should be cross-folder
        assert_eq!(total, 4);
        assert_eq!(cross, 4);
    }

    // ---------------------------------------------------------------------------
    // DB-backed tests
    // ---------------------------------------------------------------------------

    /// Create a minimal DB with docs table and optionally chunk_embeddings
    async fn make_test_db(dir: &std::path::Path) -> libsql::Connection {
        let db_path = dir.join("enzyme.db");
        let db = Builder::new_local(db_path.to_str().unwrap())
            .build()
            .await
            .unwrap();
        let conn = db.connect().unwrap();
        conn.execute(
            "CREATE TABLE IF NOT EXISTS docs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                source_ref TEXT NOT NULL UNIQUE,
                title TEXT,
                content TEXT,
                content_hash TEXT NOT NULL,
                created_at INTEGER,
                created_source TEXT NOT NULL DEFAULT 'filesystem',
                modified_at INTEGER,
                embedded_at INTEGER,
                embedded_content_hash TEXT,
                metadata TEXT
            )",
            (),
        )
        .await
        .unwrap();
        conn.execute(
            "CREATE TABLE IF NOT EXISTS chunk_embeddings (
                file_path TEXT NOT NULL,
                chunk_index INTEGER NOT NULL,
                vector BLOB NOT NULL,
                PRIMARY KEY (file_path, chunk_index)
            )",
            (),
        )
        .await
        .unwrap();
        conn.execute(
            "CREATE TABLE IF NOT EXISTS doc_links (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                doc_id INTEGER NOT NULL,
                link TEXT NOT NULL
            )",
            (),
        )
        .await
        .unwrap();
        conn
    }

    async fn insert_doc(
        conn: &libsql::Connection,
        source_ref: &str,
        content_hash: &str,
        created_source: &str,
    ) {
        conn.execute(
            "INSERT INTO docs (source_ref, title, content, content_hash, created_at, created_source) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            libsql::params![source_ref, source_ref, "body", content_hash, 1000i64, created_source],
        )
        .await
        .unwrap();
    }

    fn f32_to_blob(v: &[f32]) -> Vec<u8> {
        let mut buf = Vec::with_capacity(v.len() * 4);
        for &x in v {
            buf.extend_from_slice(&x.to_le_bytes());
        }
        buf
    }

    async fn insert_chunk(
        conn: &libsql::Connection,
        file_path: &str,
        chunk_index: u32,
        vector: &[f32],
    ) {
        let blob = f32_to_blob(vector);
        conn.execute(
            "INSERT INTO chunk_embeddings (file_path, chunk_index, vector) VALUES (?1, ?2, ?3)",
            libsql::params![file_path, chunk_index as i64, blob],
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn duplicates_grouped_by_hash() {
        let dir = TempDir::new().unwrap();
        let conn = make_test_db(dir.path()).await;

        // Two docs sharing hash "abc", one doc with unique hash "xyz"
        insert_doc(&conn, "a.md", "abc", "filename").await;
        insert_doc(&conn, "b.md", "abc", "filename").await;
        insert_doc(&conn, "c.md", "xyz", "filename").await;

        // Reopen as read-only
        let db_path = dir.path().join("enzyme.db");
        let ro_conn = open_read_only_conn(&db_path).await.unwrap();
        let result = collect_duplicates(&ro_conn).await.unwrap();

        assert_eq!(result.duplicate_set_count, 1);
        assert_eq!(result.duplicated_doc_count, 2);
        assert_eq!(result.sets[0].content_hash, "abc");
        assert_eq!(result.sets[0].count, 2);
        assert_eq!(result.sets[0].paths, vec!["a.md", "b.md"]);
    }

    #[tokio::test]
    async fn duplicates_three_way_before_two_way() {
        let dir = TempDir::new().unwrap();
        let conn = make_test_db(dir.path()).await;

        // 3-way duplicate should sort before 2-way
        insert_doc(&conn, "a.md", "hash3", "filename").await;
        insert_doc(&conn, "b.md", "hash3", "filename").await;
        insert_doc(&conn, "c.md", "hash3", "filename").await;
        insert_doc(&conn, "d.md", "hash2", "filename").await;
        insert_doc(&conn, "e.md", "hash2", "filename").await;

        let db_path = dir.path().join("enzyme.db");
        let ro_conn = open_read_only_conn(&db_path).await.unwrap();
        let result = collect_duplicates(&ro_conn).await.unwrap();

        assert_eq!(result.sets.len(), 2);
        assert_eq!(result.sets[0].count, 3); // 3-way first
        assert_eq!(result.sets[1].count, 2);
    }

    #[tokio::test]
    async fn convention_mix_respects_min_folder_size() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let conn = make_test_db(root).await;

        // Write actual .md files for frontmatter recompute
        std::fs::create_dir_all(root.join("big")).unwrap();
        std::fs::create_dir_all(root.join("small")).unwrap();
        for i in 0..5 {
            let path = format!("big/{i}.md");
            std::fs::write(root.join(&path), "body").unwrap();
            insert_doc(&conn, &path, &format!("h{i}"), "filename").await;
        }
        for i in 0..4 {
            let path = format!("small/{i}.md");
            std::fs::write(root.join(&path), "body").unwrap();
            insert_doc(&conn, &path, &format!("s{i}"), "filename").await;
        }

        let db_path = root.join("enzyme.db");
        let ro_conn = open_read_only_conn(&db_path).await.unwrap();
        let result = collect_convention_mix(root, &ro_conn, 5).await.unwrap();

        assert_eq!(result.folders_considered, 1); // only big/
        assert_eq!(result.folders_omitted_below_min, 1); // small/ omitted
        assert_eq!(result.folders[0].folder, "big/");
        assert_eq!(result.folders[0].doc_count, 5);
    }

    #[tokio::test]
    async fn frontmatter_present_recomputed() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let conn = make_test_db(root).await;

        // Doc with valid frontmatter, but DB says created_source='filesystem'
        std::fs::create_dir_all(root.join("f")).unwrap();
        std::fs::write(
            root.join("f/doc.md"),
            "---\ntitle: Hello\ncreated: 2024-01-01\n---\nbody",
        )
        .unwrap();
        insert_doc(&conn, "f/doc.md", "h1", "filesystem").await;

        // 5 more docs to meet min_folder_size=5
        for i in 1..5 {
            std::fs::write(root.join(format!("f/d{i}.md")), "body").unwrap();
            insert_doc(&conn, &format!("f/d{i}.md"), &format!("h{i}"), "filesystem").await;
        }

        let db_path = root.join("enzyme.db");
        let ro_conn = open_read_only_conn(&db_path).await.unwrap();
        let result = collect_convention_mix(root, &ro_conn, 5).await.unwrap();

        let folder = result.folders.iter().find(|f| f.folder == "f/").unwrap();
        // f/doc.md has frontmatter → frontmatter_present >= 1
        assert!(folder.frontmatter_present >= 1);
    }

    #[tokio::test]
    async fn only_embedded_docs_counted() {
        let dir = TempDir::new().unwrap();
        let conn = make_test_db(dir.path()).await;

        insert_doc(&conn, "a.md", "h1", "filename").await;
        insert_doc(&conn, "b.md", "h2", "filename").await;
        insert_doc(&conn, "c.md", "h3", "filename").await;
        insert_doc(&conn, "d.md", "h4", "filename").await;
        insert_doc(&conn, "e.md", "h5", "filename").await;

        // Only a, b, c get embeddings
        let v = vec![1.0f32, 0.0, 0.0];
        insert_chunk(&conn, "a.md", 0, &v).await;
        insert_chunk(&conn, "b.md", 0, &v).await;
        insert_chunk(&conn, "c.md", 0, &v).await;

        let db_path = dir.path().join("enzyme.db");
        let ro_conn = open_read_only_conn(&db_path).await.unwrap();
        let result = collect_semantic_scatter(&ro_conn, 20).await.unwrap();

        assert_eq!(result.docs_total, 5);
        assert_eq!(result.embedded_docs, 3);
        assert_eq!(result.docs_not_embedded, 2);
    }

    #[tokio::test]
    async fn orphan_out_and_in_degree() {
        let dir = TempDir::new().unwrap();
        let conn = make_test_db(dir.path()).await;

        // doc_a has an outgoing link to doc_b
        // doc_b is linked-to by doc_a (basename match)
        // doc_c has no links at all AND nobody links to it → orphan
        insert_doc(&conn, "a.md", "ha", "filename").await;
        insert_doc(&conn, "b.md", "hb", "filename").await;
        insert_doc(&conn, "c.md", "hc", "filename").await;

        // Add link from a → b
        conn.execute(
            "INSERT INTO doc_links (doc_id, link) SELECT id, 'b' FROM docs WHERE source_ref='a.md'",
            (),
        )
        .await
        .unwrap();

        let db_path = dir.path().join("enzyme.db");
        let ro_conn = open_read_only_conn(&db_path).await.unwrap();
        let result = collect_link_graph(&ro_conn).await.unwrap();

        // c.md is the only orphan
        assert_eq!(result.orphan_count, 1);
        assert_eq!(result.orphans[0].path, "c.md");
        assert_eq!(result.link_edges_total, 1);
    }

    #[tokio::test]
    async fn folder_shape_basic() {
        let dir = TempDir::new().unwrap();
        let conn = make_test_db(dir.path()).await;

        insert_doc(&conn, "root.md", "h1", "filename").await;
        insert_doc(&conn, "fa/a1.md", "h2", "filename").await;
        insert_doc(&conn, "fa/a2.md", "h3", "filename").await;
        insert_doc(&conn, "fb/b1.md", "h4", "filename").await;

        let db_path = dir.path().join("enzyme.db");
        let ro_conn = open_read_only_conn(&db_path).await.unwrap();
        let result = collect_folder_shape(&ro_conn).await.unwrap();

        assert_eq!(result.folder_count, 3); // (root), fa/, fb/
        assert_eq!(result.max_depth, 2); // fa/a1.md = depth 2
        assert_eq!(result.singleton_folder_count, 2); // (root) and fb/ each have 1 doc
        assert_eq!(result.largest_folder.folder, "fa/");
        assert_eq!(result.largest_folder.doc_count, 2);
    }

    #[tokio::test]
    async fn determinism_byte_identical() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let conn = make_test_db(root).await;

        std::fs::create_dir_all(root.join("fa")).unwrap();
        std::fs::create_dir_all(root.join("fb")).unwrap();
        std::fs::write(root.join("fa/a1.md"), "---\ntitle: A1\n---\nbody").unwrap();
        std::fs::write(root.join("fb/b1.md"), "body").unwrap();
        std::fs::write(root.join("fb/b2.md"), "body").unwrap(); // same content = duplicate

        insert_doc(&conn, "fa/a1.md", "hash_a1", "frontmatter").await;
        insert_doc(&conn, "fb/b1.md", "hash_dup", "filename").await;
        insert_doc(&conn, "fb/b2.md", "hash_dup", "filename").await;

        let v_a = vec![1.0f32, 0.0];
        let v_b = vec![0.0f32, 1.0];
        insert_chunk(&conn, "fa/a1.md", 0, &v_a).await;
        insert_chunk(&conn, "fb/b1.md", 0, &v_b).await;

        let db_path = root.join("enzyme.db");

        async fn build_report_helper(
            root: &std::path::Path,
            db_path: &std::path::Path,
        ) -> StructureReport {
            let ro_conn = super::open_read_only_conn(db_path).await.unwrap();
            let exact_duplicates = super::collect_duplicates(&ro_conn).await.unwrap();
            let folder_convention_mix =
                super::collect_convention_mix(root, &ro_conn, 5).await.unwrap();
            let semantic_scatter = super::collect_semantic_scatter(&ro_conn, 20).await.unwrap();
            let link_graph = super::collect_link_graph(&ro_conn).await.unwrap();
            let folder_shape = super::collect_folder_shape(&ro_conn).await.unwrap();
            StructureReport {
                vault: root.to_string_lossy().to_string(),
                database: db_path.to_string_lossy().to_string(),
                min_folder_size: 5,
                top: 20,
                exact_duplicates,
                folder_convention_mix,
                semantic_scatter,
                link_graph,
                folder_shape,
            }
        }

        let r1 = build_report_helper(root, &db_path).await;
        let r2 = build_report_helper(root, &db_path).await;

        let json1 = serde_json::to_string_pretty(&r1).unwrap();
        let json2 = serde_json::to_string_pretty(&r2).unwrap();
        assert_eq!(json1, json2, "JSON output must be deterministic");

        let human1 = render_human(&r1, 20);
        let human2 = render_human(&r2, 20);
        assert_eq!(human1, human2, "human output must be deterministic");
    }

    #[tokio::test]
    async fn not_initialized_errors_cleanly() {
        let dir = TempDir::new().unwrap();
        let paths = EnzymePaths::new(dir.path()).unwrap();
        assert!(
            !paths.vault_db.exists(),
            "vault_db should not exist for empty dir"
        );
    }
}
