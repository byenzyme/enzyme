use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use ignore::WalkBuilder;
use ignore::overrides::OverrideBuilder;
use libsql::{Builder, Connection, OpenFlags};
use serde::Serialize;

use enzyme_core::config::enzyme_config::EnzymeConfig;
use enzyme_core::config::paths::EnzymePaths;
use enzyme_core::document::{DocumentProcessor, FileDiscovery, parse_frontmatter};

// Human-mode truncation limit for long lists.
const HUMAN_LIST_LIMIT: usize = 20;
// Human-mode truncation limit for duplicate_sets.
const DUP_LIST_LIMIT: usize = 10;

// ---------------------------------------------------------------------------
// Data model
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct InvisibleGroup {
    extension: String,
    top_level_dir: String,
    count: u64,
    cause: &'static str,
}

#[derive(Debug, Serialize)]
struct InvisibleFiles {
    total: u64,
    groups: Vec<InvisibleGroup>,
}

#[derive(Debug, Serialize)]
struct Coverage {
    total_files: u64,
    md_files: u64,
    docs_in_db: u64,
    invisible_files: InvisibleFiles,
}

#[derive(Debug, Serialize)]
struct AbsentFile {
    path: String,
    cause: &'static str,
}

#[derive(Debug, Serialize)]
struct FrontmatterAnomaly {
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    db_created_source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    frontmatter_created_present: Option<bool>,
    cause: &'static str,
}

#[derive(Debug, Serialize)]
struct UndatedDoc {
    path: String,
    cause: &'static str,
}

#[derive(Debug, Serialize)]
struct CapDropout {
    path: String,
    created_at: i64,
    cause: &'static str,
    window_size: u64,
}

#[derive(Debug, Serialize)]
struct EmbeddingFacts {
    embedded_docs: i64,
    docs_needing_embedding: i64,
    embedding_window_size: u64,
    cap_dropouts: Vec<CapDropout>,
}

#[derive(Debug, Serialize)]
struct StaleEntry {
    path: String,
    cause: &'static str,
}

#[derive(Debug, Serialize)]
struct StaleIndex {
    disk_newer_than_db: Vec<StaleEntry>,
    db_rows_for_deleted_files: Vec<StaleEntry>,
}

#[derive(Debug, Serialize)]
struct DuplicateSet {
    content_hash: String,
    count: u64,
    paths: Vec<String>,
}

#[derive(Debug, Serialize)]
struct DuplicateSets {
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
struct FolderDateEntry {
    folder: String,
    doc_count: u64,
    vault_share_percent: String,
    dated_fraction: String,
    created_source: CreatedSourceCounts,
}

#[derive(Debug, Serialize)]
struct FolderDateCoverage {
    min_folder_size: usize,
    folders: Vec<FolderDateEntry>,
}

#[derive(Debug, Serialize)]
struct DoctorReport {
    vault: String,
    database: String,
    coverage: Coverage,
    md_files_absent_from_db: Vec<AbsentFile>,
    frontmatter_anomalies: Vec<FrontmatterAnomaly>,
    undated_docs: Vec<UndatedDoc>,
    embedding: EmbeddingFacts,
    stale_index: StaleIndex,
    duplicate_sets: DuplicateSets,
    folder_date_coverage: FolderDateCoverage,
}

// ---------------------------------------------------------------------------
// Read-only DB open
// ---------------------------------------------------------------------------

/// Open a SQLite database in strictly read-only mode.
/// No WAL pragma, no migrations, no sidecar files created.
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
// Helper: run a query and collect rows
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
// Helper: folder of a source_ref
// ---------------------------------------------------------------------------

/// Return the top-level folder component of a source_ref as a display string.
/// Top-level files (no directory) return "(root)".
/// Otherwise return the parent directory with a trailing slash, e.g. "misc/".
fn folder_of(source_ref: &str) -> String {
    let p = std::path::Path::new(source_ref);
    match p.parent() {
        None => "(root)".to_string(),
        Some(p) if p.as_os_str().is_empty() => "(root)".to_string(),
        Some(parent) => {
            let s = parent.to_string_lossy().to_string();
            let s = s.replace('\\', "/");
            if s.is_empty() {
                "(root)".to_string()
            } else {
                format!("{s}/")
            }
        }
    }
}

/// Format a float as a fixed-decimal string.
fn fmt_fixed(x: f64, decimals: usize) -> String {
    format!("{:.*}", decimals, x)
}

// ---------------------------------------------------------------------------
// Collect: coverage (total files + invisible non-md breakdown)
// ---------------------------------------------------------------------------

fn collect_coverage(vault_path: &Path, md_files: &[PathBuf], docs_in_db: u64) -> Result<Coverage> {
    // Reuse the same exclude patterns as FileDiscovery, but with NO include pattern
    // so all file types are counted.
    let vault_canon = vault_path
        .canonicalize()
        .map_err(|e| anyhow::anyhow!("vault canonicalize: {e}"))?;

    let exclude_patterns = vec![
        ".agents/**",
        ".claude/**",
        ".codex/**",
        ".codex-work/**",
        ".conversations/**",
        ".enzyme/**",
        ".enzyme-embeddings/**",
        ".git/**",
        ".hermes/**",
        ".local/**",
        ".obsidian/**",
        ".pi/**",
        ".trash/**",
        "__pycache__/**",
        "build/**",
        "dist/**",
        "node_modules/**",
        "target/**",
        "ENZYME_GUIDE.md",
        "enzyme-config.yaml",
    ];

    let mut overrides = OverrideBuilder::new(&vault_canon);
    // Positive glob — accept all files
    overrides
        .add("**")
        .map_err(|e| anyhow::anyhow!("override add: {e}"))?;
    for pat in &exclude_patterns {
        overrides
            .add(&format!("!{pat}"))
            .map_err(|e| anyhow::anyhow!("override add: {e}"))?;
    }
    let overrides = overrides
        .build()
        .map_err(|e| anyhow::anyhow!("override build: {e}"))?;

    let walker = WalkBuilder::new(&vault_canon)
        .overrides(overrides)
        .hidden(false)
        .build();

    let mut total_files: u64 = 0;
    // BTreeMap<(extension, top_level_dir), count>
    let mut invisible_map: BTreeMap<(String, String), u64> = BTreeMap::new();

    for entry in walker {
        let entry = entry.map_err(|e| anyhow::anyhow!("walk error: {e}"))?;
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        total_files += 1;

        // Only count non-md files as invisible
        let rel = entry
            .path()
            .strip_prefix(&vault_canon)
            .unwrap_or(entry.path());
        let ext = rel
            .extension()
            .and_then(|e| e.to_str())
            .map(|s| format!(".{}", s.to_lowercase()))
            .unwrap_or_else(|| "(none)".to_string());

        if ext == ".md" {
            continue;
        }

        let top = rel
            .components()
            .next()
            .and_then(|c| match c {
                std::path::Component::Normal(s) => s.to_str().map(|s| s.to_string()),
                _ => None,
            })
            .unwrap_or_else(|| "(root)".to_string());

        // If there's only one component (file in root), top_level_dir is "(root)"
        let top_level_dir = if rel.components().count() == 1 {
            "(root)".to_string()
        } else {
            top
        };

        *invisible_map.entry((ext, top_level_dir)).or_insert(0) += 1;
    }

    let invisible_total: u64 = invisible_map.values().sum();
    let groups: Vec<InvisibleGroup> = invisible_map
        .into_iter()
        .map(|((extension, top_level_dir), count)| InvisibleGroup {
            extension,
            top_level_dir,
            count,
            cause: "not_markdown",
        })
        .collect();

    Ok(Coverage {
        total_files,
        md_files: md_files.len() as u64,
        docs_in_db,
        invisible_files: InvisibleFiles {
            total: invisible_total,
            groups,
        },
    })
}

// ---------------------------------------------------------------------------
// Collect: md files absent from DB
// ---------------------------------------------------------------------------

fn collect_absent(
    md_files: &[PathBuf],
    db_source_refs: &std::collections::BTreeSet<String>,
) -> Vec<AbsentFile> {
    let mut absent = Vec::new();
    for path in md_files {
        let source_ref = path.to_string_lossy().to_string();
        if !db_source_refs.contains(&source_ref) {
            absent.push(AbsentFile {
                path: source_ref,
                cause: "not_indexed",
            });
        }
    }
    absent.sort_by(|a, b| a.path.cmp(&b.path));
    absent
}

// ---------------------------------------------------------------------------
// Collect: frontmatter anomalies
// ---------------------------------------------------------------------------

fn collect_frontmatter(
    vault_path: &Path,
    md_files: &[PathBuf],
    db_docs: &[(String, String)], // (source_ref, created_source)
) -> Result<Vec<FrontmatterAnomaly>> {
    // Build lookup: source_ref -> created_source
    let db_map: std::collections::HashMap<&str, &str> = db_docs
        .iter()
        .map(|(sr, cs)| (sr.as_str(), cs.as_str()))
        .collect();

    let mut anomalies = Vec::new();

    for rel_path in md_files {
        let abs_path = vault_path.join(rel_path);
        let source_ref = rel_path.to_string_lossy().to_string();

        let content = match std::fs::read_to_string(&abs_path) {
            Ok(c) => c,
            Err(_) => continue, // unreadable; skip
        };

        // Detect structural frontmatter presence
        let has_opening = content.trim_start().starts_with("---");
        if !has_opening {
            continue; // no frontmatter at all — not anomalous
        }

        // Check for unclosed block
        let trimmed = content.trim_start();
        let after_opening = &trimmed[3..];
        let after_first_newline = after_opening.find('\n').map(|i| &after_opening[i + 1..]);
        let has_closing = after_first_newline
            .map(|s| s.find("\n---").is_some() || s.starts_with("---"))
            .unwrap_or(false);

        if !has_closing {
            // Unclosed frontmatter block
            anomalies.push(FrontmatterAnomaly {
                path: source_ref.clone(),
                db_created_source: None,
                frontmatter_created_present: None,
                cause: "frontmatter_unclosed",
            });
            continue;
        }

        // Try parsing the frontmatter
        let parsed = parse_frontmatter(&content);
        if parsed.is_none() {
            // Has a block (opening + closing) but parse failed (YAML error)
            anomalies.push(FrontmatterAnomaly {
                path: source_ref.clone(),
                db_created_source: None,
                frontmatter_created_present: None,
                cause: "frontmatter_parse_failed",
            });
            continue;
        }

        // Block parsed successfully. Now check date source in DB.
        let fm = parsed.unwrap();
        if let Some(db_cs) = db_map.get(source_ref.as_str()) {
            let db_created_source = *db_cs;
            // "date did not come from frontmatter" = source is filename or filesystem
            if db_created_source == "filename" || db_created_source == "filesystem" {
                let fm_created_present = fm.created.is_some();
                let cause = if fm_created_present {
                    "db_date_source_mismatch"
                } else {
                    "frontmatter_has_no_date_field"
                };
                anomalies.push(FrontmatterAnomaly {
                    path: source_ref.clone(),
                    db_created_source: Some(db_created_source.to_string()),
                    frontmatter_created_present: Some(fm_created_present),
                    cause,
                });
            }
        }
    }

    anomalies.sort_by(|a, b| a.path.cmp(&b.path).then(a.cause.cmp(&b.cause)));
    Ok(anomalies)
}

// ---------------------------------------------------------------------------
// Collect: undated docs (created_source = 'filesystem')
// ---------------------------------------------------------------------------

fn collect_undated(db_docs: &[(String, String)]) -> Vec<UndatedDoc> {
    let mut undated: Vec<UndatedDoc> = db_docs
        .iter()
        .filter(|(_, cs)| cs == "filesystem")
        .map(|(sr, _)| UndatedDoc {
            path: sr.clone(),
            cause: "date_fell_through_to_filesystem",
        })
        .collect();
    undated.sort_by(|a, b| a.path.cmp(&b.path));
    undated
}

// ---------------------------------------------------------------------------
// Collect: embedding cap dropouts
// ---------------------------------------------------------------------------

fn collect_embedding(
    all_needing: &[(String, i64)], // (source_ref, created_at) ordered DESC by created_at
    window_size: Option<usize>,
    docs_in_db: i64,
) -> EmbeddingFacts {
    let needing_count = all_needing.len() as i64;
    let embedded_docs = docs_in_db - needing_count;

    let effective_window = window_size.unwrap_or(usize::MAX);
    let window_label = match window_size {
        Some(n) => n as u64,
        None => 0, // 0 = unlimited in display
    };

    // The first `effective_window` rows (already sorted DESC) are "within window".
    // Everything at index >= effective_window is a dropout (oldest ones).
    let dropouts: Vec<CapDropout> = if effective_window < all_needing.len() {
        let mut d: Vec<CapDropout> = all_needing[effective_window..]
            .iter()
            .map(|(sr, ca)| CapDropout {
                path: sr.clone(),
                created_at: *ca,
                cause: "outside_embedding_recency_window",
                window_size: window_label,
            })
            .collect();
        d.sort_by(|a, b| a.path.cmp(&b.path));
        d
    } else {
        Vec::new()
    };

    EmbeddingFacts {
        embedded_docs,
        docs_needing_embedding: needing_count,
        embedding_window_size: window_label,
        cap_dropouts: dropouts,
    }
}

// ---------------------------------------------------------------------------
// Collect: stale index
// ---------------------------------------------------------------------------

fn collect_stale(
    vault_path: &Path,
    md_files: &[PathBuf],
    db_source_refs: &std::collections::BTreeSet<String>,
    db_hashes: &std::collections::HashMap<String, String>,
) -> Result<StaleIndex> {
    let processor = DocumentProcessor::new(vault_path.to_path_buf());

    let mut disk_newer: Vec<StaleEntry> = Vec::new();
    let mut deleted: Vec<StaleEntry> = Vec::new();

    // Files on disk but with different hash in DB (content changed)
    for rel_path in md_files {
        let source_ref = rel_path.to_string_lossy().to_string();
        if let Some(db_hash) = db_hashes.get(&source_ref) {
            // Recompute hash via processor
            match processor.process_file(rel_path) {
                Ok(doc) => {
                    if &doc.content_hash != db_hash {
                        disk_newer.push(StaleEntry {
                            path: source_ref,
                            cause: "content_hash_differs",
                        });
                    }
                }
                Err(_) => {} // skip unreadable files
            }
        }
    }

    // DB rows for files no longer on disk
    for sr in db_source_refs {
        let rel_path = PathBuf::from(sr);
        let discovered_ref = rel_path.to_string_lossy().to_string();
        // Check if this source_ref is in the current discovery set
        let on_disk = md_files
            .iter()
            .any(|p| p.to_string_lossy() == discovered_ref.as_str());
        if !on_disk {
            deleted.push(StaleEntry {
                path: sr.clone(),
                cause: "file_absent_on_disk",
            });
        }
    }

    disk_newer.sort_by(|a, b| a.path.cmp(&b.path));
    deleted.sort_by(|a, b| a.path.cmp(&b.path));

    Ok(StaleIndex {
        disk_newer_than_db: disk_newer,
        db_rows_for_deleted_files: deleted,
    })
}

// ---------------------------------------------------------------------------
// Collect: duplicate_sets
// Groups of docs sharing content_hash.
// Sets sorted by (count DESC, first path ASC); paths within each set sorted ASC.
// ---------------------------------------------------------------------------

async fn collect_duplicate_sets(conn: &Connection) -> Result<DuplicateSets> {
    // Hashes with count > 1, sorted by (count DESC, content_hash ASC) for determinism
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
            content_hash: hash.clone(),
            count: *count,
            paths,
        });
    }

    // sets are already sorted by (count DESC, content_hash ASC) from the SQL ORDER BY.
    // Within equal count, sort by first path ASC as a secondary tiebreak.
    sets.sort_by(|a, b| {
        b.count.cmp(&a.count).then_with(|| {
            let pa = a.paths.first().map(|s| s.as_str()).unwrap_or("");
            let pb = b.paths.first().map(|s| s.as_str()).unwrap_or("");
            pa.cmp(pb)
        })
    });

    Ok(DuplicateSets {
        duplicate_set_count: sets.len() as u64,
        duplicated_doc_count,
        sets,
    })
}

// ---------------------------------------------------------------------------
// Collect: folder_date_coverage
// For each folder with >= min_folder_size docs: doc count, vault share %,
// dated fraction (created_source != 'filesystem'), and created_source split.
// Sorted by doc count DESC, then folder ASC for determinism.
// ---------------------------------------------------------------------------

fn collect_folder_date_coverage(
    db_docs: &[(String, String)], // (source_ref, created_source)
    docs_total: u64,
    min_folder_size: usize,
) -> FolderDateCoverage {
    // Accumulate per folder using BTreeMap for deterministic order during insertion
    struct FolderAccum {
        doc_count: u64,
        frontmatter: u64,
        filename: u64,
        filesystem: u64,
    }

    let mut by_folder: BTreeMap<String, FolderAccum> = BTreeMap::new();

    for (source_ref, created_source) in db_docs {
        let folder = folder_of(source_ref);
        let entry = by_folder.entry(folder).or_insert(FolderAccum {
            doc_count: 0,
            frontmatter: 0,
            filename: 0,
            filesystem: 0,
        });
        entry.doc_count += 1;
        match created_source.as_str() {
            "frontmatter" => entry.frontmatter += 1,
            "filename" => entry.filename += 1,
            _ => entry.filesystem += 1, // filesystem or unknown
        }
    }

    let mut folders: Vec<FolderDateEntry> = by_folder
        .into_iter()
        .filter(|(_, a)| a.doc_count >= min_folder_size as u64)
        .map(|(folder, a)| {
            let dated = a.frontmatter + a.filename;
            let dated_frac = if a.doc_count > 0 {
                dated as f64 / a.doc_count as f64
            } else {
                0.0
            };
            let vault_share = if docs_total > 0 {
                a.doc_count as f64 / docs_total as f64 * 100.0
            } else {
                0.0
            };
            FolderDateEntry {
                folder,
                doc_count: a.doc_count,
                vault_share_percent: fmt_fixed(vault_share, 1),
                dated_fraction: fmt_fixed(dated_frac, 3),
                created_source: CreatedSourceCounts {
                    frontmatter: a.frontmatter,
                    filename: a.filename,
                    filesystem: a.filesystem,
                },
            }
        })
        .collect();

    // Sort by doc_count DESC, then folder ASC for determinism
    folders.sort_by(|a, b| b.doc_count.cmp(&a.doc_count).then(a.folder.cmp(&b.folder)));

    FolderDateCoverage {
        min_folder_size,
        folders,
    }
}

// ---------------------------------------------------------------------------
// Human-readable rendering
// ---------------------------------------------------------------------------

fn render_human(report: &DoctorReport) -> String {
    let mut out = String::new();

    out.push_str(&format!("vault: {}\n", report.vault));
    out.push_str(&format!("database: {}\n", report.database));
    out.push('\n');

    // Coverage
    out.push_str("coverage\n");
    out.push_str(&format!("  total_files: {}\n", report.coverage.total_files));
    out.push_str(&format!("  md_files: {}\n", report.coverage.md_files));
    out.push_str(&format!("  docs_in_db: {}\n", report.coverage.docs_in_db));
    let inv_total = report.coverage.invisible_files.total;
    out.push_str(&format!("  invisible_files: {}\n", inv_total));
    if inv_total > 0 {
        out.push_str("    cause=not_markdown\n");
        let groups = &report.coverage.invisible_files.groups;
        let shown = groups.len().min(HUMAN_LIST_LIMIT);
        for g in &groups[..shown] {
            out.push_str(&format!(
                "      {}  {}: {}\n",
                g.extension, g.top_level_dir, g.count
            ));
        }
        if groups.len() > HUMAN_LIST_LIMIT {
            out.push_str(&format!(
                "      ... ({} more; use --json for full list)\n",
                groups.len() - HUMAN_LIST_LIMIT
            ));
        }
    }
    out.push('\n');

    // Absent
    out.push_str(&format!(
        "md_files_absent_from_db: {}\n",
        report.md_files_absent_from_db.len()
    ));
    let shown = report.md_files_absent_from_db.len().min(HUMAN_LIST_LIMIT);
    for f in &report.md_files_absent_from_db[..shown] {
        out.push_str(&format!("  {}\n", f.path));
    }
    if report.md_files_absent_from_db.len() > HUMAN_LIST_LIMIT {
        out.push_str(&format!(
            "  ... ({} more; use --json for full list)\n",
            report.md_files_absent_from_db.len() - HUMAN_LIST_LIMIT
        ));
    }
    out.push('\n');

    // Frontmatter anomalies
    out.push_str(&format!(
        "frontmatter_anomalies: {}\n",
        report.frontmatter_anomalies.len()
    ));
    // Group by cause for human output
    let mut by_cause: BTreeMap<&str, Vec<&FrontmatterAnomaly>> = BTreeMap::new();
    for a in &report.frontmatter_anomalies {
        by_cause.entry(a.cause).or_default().push(a);
    }
    for (cause, entries) in &by_cause {
        out.push_str(&format!("  cause={cause}\n"));
        let shown = entries.len().min(HUMAN_LIST_LIMIT);
        for e in &entries[..shown] {
            if let (Some(db_cs), Some(fm_present)) =
                (&e.db_created_source, e.frontmatter_created_present)
            {
                out.push_str(&format!(
                    "    {}  (db_created_source={}, frontmatter_created_present={})\n",
                    e.path, db_cs, fm_present
                ));
            } else {
                out.push_str(&format!("    {}\n", e.path));
            }
        }
        if entries.len() > HUMAN_LIST_LIMIT {
            out.push_str(&format!(
                "    ... ({} more; use --json for full list)\n",
                entries.len() - HUMAN_LIST_LIMIT
            ));
        }
    }
    out.push('\n');

    // Undated docs
    out.push_str(&format!("undated_docs: {}\n", report.undated_docs.len()));
    if !report.undated_docs.is_empty() {
        out.push_str("  cause=date_fell_through_to_filesystem\n");
        let shown = report.undated_docs.len().min(HUMAN_LIST_LIMIT);
        for d in &report.undated_docs[..shown] {
            out.push_str(&format!("    {}\n", d.path));
        }
        if report.undated_docs.len() > HUMAN_LIST_LIMIT {
            out.push_str(&format!(
                "    ... ({} more; use --json for full list)\n",
                report.undated_docs.len() - HUMAN_LIST_LIMIT
            ));
        }
    }
    out.push('\n');

    // Embedding
    out.push_str("embedding\n");
    out.push_str(&format!(
        "  embedded_docs: {}\n",
        report.embedding.embedded_docs
    ));
    out.push_str(&format!(
        "  docs_needing_embedding: {}\n",
        report.embedding.docs_needing_embedding
    ));
    out.push_str(&format!(
        "  embedding_window_size: {}\n",
        report.embedding.embedding_window_size
    ));
    out.push_str(&format!(
        "  embedding_cap_dropouts: {}\n",
        report.embedding.cap_dropouts.len()
    ));
    if !report.embedding.cap_dropouts.is_empty() {
        let shown = report.embedding.cap_dropouts.len().min(HUMAN_LIST_LIMIT);
        for d in &report.embedding.cap_dropouts[..shown] {
            out.push_str(&format!("    {}\n", d.path));
        }
        if report.embedding.cap_dropouts.len() > HUMAN_LIST_LIMIT {
            out.push_str(&format!(
                "    ... ({} more; use --json for full list)\n",
                report.embedding.cap_dropouts.len() - HUMAN_LIST_LIMIT
            ));
        }
    }
    out.push('\n');

    // Stale index
    out.push_str("stale_index\n");
    out.push_str(&format!(
        "  disk_newer_than_db: {}\n",
        report.stale_index.disk_newer_than_db.len()
    ));
    let shown = report
        .stale_index
        .disk_newer_than_db
        .len()
        .min(HUMAN_LIST_LIMIT);
    for e in &report.stale_index.disk_newer_than_db[..shown] {
        out.push_str(&format!("    {}\n", e.path));
    }
    if report.stale_index.disk_newer_than_db.len() > HUMAN_LIST_LIMIT {
        out.push_str(&format!(
            "    ... ({} more; use --json for full list)\n",
            report.stale_index.disk_newer_than_db.len() - HUMAN_LIST_LIMIT
        ));
    }
    out.push_str(&format!(
        "  db_rows_for_deleted_files: {}\n",
        report.stale_index.db_rows_for_deleted_files.len()
    ));
    let shown = report
        .stale_index
        .db_rows_for_deleted_files
        .len()
        .min(HUMAN_LIST_LIMIT);
    for e in &report.stale_index.db_rows_for_deleted_files[..shown] {
        out.push_str(&format!("    {}\n", e.path));
    }
    if report.stale_index.db_rows_for_deleted_files.len() > HUMAN_LIST_LIMIT {
        out.push_str(&format!(
            "    ... ({} more; use --json for full list)\n",
            report.stale_index.db_rows_for_deleted_files.len() - HUMAN_LIST_LIMIT
        ));
    }
    out.push('\n');

    // Duplicate sets
    out.push_str("duplicate_sets\n");
    out.push_str(&format!(
        "  duplicate_set_count: {}\n",
        report.duplicate_sets.duplicate_set_count
    ));
    out.push_str(&format!(
        "  duplicated_doc_count: {}\n",
        report.duplicate_sets.duplicated_doc_count
    ));
    if !report.duplicate_sets.sets.is_empty() {
        let sets = &report.duplicate_sets.sets;
        let shown = sets.len().min(DUP_LIST_LIMIT);
        for s in &sets[..shown] {
            let hash_short = if s.content_hash.len() > 8 {
                format!("{}...", &s.content_hash[..8])
            } else {
                s.content_hash.clone()
            };
            let paths_str = s.paths.join(", ");
            out.push_str(&format!(
                "  n={}  hash={}  {}\n",
                s.count, hash_short, paths_str
            ));
        }
        if sets.len() > DUP_LIST_LIMIT {
            out.push_str(&format!(
                "  ... {} more; use --json\n",
                sets.len() - DUP_LIST_LIMIT
            ));
        }
    }
    out.push('\n');

    // Folder date coverage
    out.push_str("folder_date_coverage\n");
    out.push_str(&format!(
        "  min_folder_size: {}\n",
        report.folder_date_coverage.min_folder_size
    ));
    if report.folder_date_coverage.folders.is_empty() {
        out.push_str("  (no folders meet min_folder_size threshold)\n");
    } else {
        for f in &report.folder_date_coverage.folders {
            out.push_str(&format!(
                "  {}  n={}  share={}%  dated={}  (frontmatter={} filename={} filesystem={})\n",
                f.folder,
                f.doc_count,
                f.vault_share_percent,
                f.dated_fraction,
                f.created_source.frontmatter,
                f.created_source.filename,
                f.created_source.filesystem,
            ));
        }
    }

    out
}

// ---------------------------------------------------------------------------
// Main entry point
// ---------------------------------------------------------------------------

pub async fn run_doctor(vault_path: &Path, json: bool) -> Result<()> {
    let paths = EnzymePaths::new(vault_path)?;
    // Do NOT call paths.sync_on_drop() — read-only command

    if !paths.vault_db.exists() {
        let vault_str = vault_path.display().to_string();
        if json {
            println!(
                "{{\"error\":\"vault_not_initialized\",\"vault\":\"{}\"}}",
                vault_str.replace('"', "\\\"")
            );
        } else {
            eprintln!(
                "enzyme doctor: vault not initialized (no .enzyme/enzyme.db at {}); run 'enzyme init' first",
                vault_str
            );
        }
        std::process::exit(1);
    }

    // Load config to get effective embedding window (read-only).
    let enzyme_config = EnzymeConfig::load(vault_path)?.unwrap_or_default();
    let window_size: Option<usize> = enzyme_config.effective_max_embedding_files();

    // Open DB strictly read-only.
    let conn = open_read_only_conn(&paths.vault_db).await?;

    // --- DB queries ---

    // All docs: (source_ref, created_source, content_hash)
    let db_docs_full: Vec<(String, String, String)> = query_all(
        &conn,
        "SELECT source_ref, created_source, content_hash FROM docs ORDER BY source_ref",
        |row| {
            let source_ref: String = row.get(0).map_err(|e| anyhow::anyhow!("{e}"))?;
            let created_source: String = row.get(1).map_err(|e| anyhow::anyhow!("{e}"))?;
            let content_hash: String = row.get(2).map_err(|e| anyhow::anyhow!("{e}"))?;
            Ok((source_ref, created_source, content_hash))
        },
    )
    .await?;

    let docs_in_db = db_docs_full.len() as u64;

    // source_ref + created_source pairs for frontmatter/undated/folder-date checks
    let db_docs: Vec<(String, String)> = db_docs_full
        .iter()
        .map(|(sr, cs, _)| (sr.clone(), cs.clone()))
        .collect();

    // source_ref set (for O(1) lookup)
    let db_source_refs: std::collections::BTreeSet<String> =
        db_docs_full.iter().map(|(sr, _, _)| sr.clone()).collect();

    // content_hash map: source_ref -> hash
    let db_hashes: std::collections::HashMap<String, String> = db_docs_full
        .iter()
        .map(|(sr, _, h)| (sr.clone(), h.clone()))
        .collect();

    // Docs needing embedding (ordered DESC by created_at for cap dropout logic)
    let needing_embedding: Vec<(String, i64)> = query_all(
        &conn,
        "SELECT source_ref, created_at FROM docs \
         WHERE (embedded_content_hash IS NULL OR embedded_content_hash != content_hash OR embedded_at IS NULL) \
         ORDER BY created_at DESC, source_ref DESC",
        |row| {
            let sr: String = row.get(0).map_err(|e| anyhow::anyhow!("{e}"))?;
            let ca: i64 = row.get(1).map_err(|e| anyhow::anyhow!("{e}"))?;
            Ok((sr, ca))
        },
    )
    .await?;

    // --- Filesystem discovery ---
    // Honor `exclude` so doctor doesn't report excluded-folder files as
    // perpetually "not_indexed" (indexing skips them by design).
    let md_files = FileDiscovery::new(vault_path.to_path_buf())
        .with_excluded_folders(EnzymeConfig::excluded_folders_at(vault_path))
        .discover()?;

    // --- Collect each fact category ---

    let coverage = collect_coverage(vault_path, &md_files, docs_in_db)?;
    let md_files_absent_from_db = collect_absent(&md_files, &db_source_refs);
    let frontmatter_anomalies = collect_frontmatter(vault_path, &md_files, &db_docs)?;
    let undated_docs = collect_undated(&db_docs);
    let embedding = collect_embedding(&needing_embedding, window_size, docs_in_db as i64);
    let stale_index = collect_stale(vault_path, &md_files, &db_source_refs, &db_hashes)?;
    let duplicate_sets = collect_duplicate_sets(&conn).await?;
    let folder_date_coverage = collect_folder_date_coverage(&db_docs, docs_in_db, 5);

    let report = DoctorReport {
        vault: vault_path.display().to_string(),
        database: paths.vault_db.display().to_string(),
        coverage,
        md_files_absent_from_db,
        frontmatter_anomalies,
        undated_docs,
        embedding,
        stale_index,
        duplicate_sets,
        folder_date_coverage,
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", render_human(&report));
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    // Helper: create a minimal SQLite file (via libsql write path) and then
    // open it with our read-only helper to verify no WAL is created.
    async fn make_minimal_db(path: &Path) {
        let db = Builder::new_local(path.to_str().unwrap())
            .build()
            .await
            .unwrap();
        let conn = db.connect().unwrap();
        conn.execute(
            "CREATE TABLE IF NOT EXISTS test (id INTEGER PRIMARY KEY)",
            (),
        )
        .await
        .unwrap();
    }

    /// Create a docs table in an in-memory-style temp DB.
    async fn make_test_db(dir: &Path) -> libsql::Connection {
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

    #[tokio::test]
    async fn read_only_open_cannot_write() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        make_minimal_db(&db_path).await;

        // WAL file must not exist yet (we haven't opened with WAL mode)
        let wal_path = dir.path().join("test.db-wal");
        assert!(!wal_path.exists(), "WAL should not exist before RO open");

        let conn = open_read_only_conn(&db_path).await.unwrap();

        // Write should fail
        let result = conn.execute("INSERT INTO test DEFAULT VALUES", ()).await;
        assert!(result.is_err(), "write on read-only conn should fail");

        // WAL must still not exist after RO open
        assert!(
            !wal_path.exists(),
            "doctor must not create WAL sidecar: {:?}",
            wal_path
        );
    }

    #[test]
    fn coverage_groups_non_md_by_ext_and_dir() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        fs::write(root.join("a.json"), "{}").unwrap();
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("sub/b.png"), "data").unwrap();
        fs::write(root.join("c.md"), "# note").unwrap();

        let md_files = vec![PathBuf::from("c.md")];
        let coverage = collect_coverage(root, &md_files, 1).unwrap();

        assert_eq!(coverage.total_files, 3);
        assert_eq!(coverage.md_files, 1);
        assert_eq!(coverage.docs_in_db, 1);
        assert_eq!(coverage.invisible_files.total, 2);

        let exts: Vec<&str> = coverage
            .invisible_files
            .groups
            .iter()
            .map(|g| g.extension.as_str())
            .collect();
        assert!(exts.contains(&".json"), "should include .json");
        assert!(exts.contains(&".png"), "should include .png");
        assert!(!exts.contains(&".md"), ".md must not appear in invisible");
    }

    #[test]
    fn frontmatter_parse_failed_detected() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();

        // File with unclosed bracket (YAML error) but a valid opening + closing fence
        fs::write(root.join("bad.md"), "---\ntitle: [unclosed\n---\n").unwrap();
        // File with opening but no closing fence
        fs::write(root.join("unclosed.md"), "---\ntitle: x\nno close").unwrap();

        let md_files = vec![PathBuf::from("bad.md"), PathBuf::from("unclosed.md")];
        // No DB rows → db_docs empty; these are not in DB, so the date-source check won't trigger.
        // We only check the causes here.
        let anomalies = collect_frontmatter(root, &md_files, &[]).unwrap();

        let bad = anomalies.iter().find(|a| a.path == "bad.md").unwrap();
        assert_eq!(bad.cause, "frontmatter_parse_failed");

        let unc = anomalies.iter().find(|a| a.path == "unclosed.md").unwrap();
        assert_eq!(unc.cause, "frontmatter_unclosed");
    }

    #[test]
    fn frontmatter_no_date_field() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        // Valid FM without any created key
        fs::write(root.join("no_date.md"), "---\ntitle: Hello\n---\nbody\n").unwrap();

        let md_files = vec![PathBuf::from("no_date.md")];
        let db_docs = vec![("no_date.md".to_string(), "filesystem".to_string())];
        let anomalies = collect_frontmatter(root, &md_files, &db_docs).unwrap();

        let entry = anomalies.iter().find(|a| a.path == "no_date.md").unwrap();
        assert_eq!(entry.cause, "frontmatter_has_no_date_field");
        assert_eq!(entry.frontmatter_created_present, Some(false));
    }

    #[test]
    fn db_date_source_mismatch() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        // FM with a valid date
        fs::write(
            root.join("mismatch.md"),
            "---\ncreated: 2024-01-01\ntitle: X\n---\nbody\n",
        )
        .unwrap();

        let md_files = vec![PathBuf::from("mismatch.md")];
        let db_docs = vec![("mismatch.md".to_string(), "filesystem".to_string())];
        let anomalies = collect_frontmatter(root, &md_files, &db_docs).unwrap();

        let entry = anomalies.iter().find(|a| a.path == "mismatch.md").unwrap();
        assert_eq!(entry.cause, "db_date_source_mismatch");
        assert_eq!(entry.frontmatter_created_present, Some(true));
    }

    #[test]
    fn undated_docs_from_filesystem_source() {
        let db_docs = vec![
            ("a.md".to_string(), "frontmatter".to_string()),
            ("b.md".to_string(), "filesystem".to_string()),
            ("c.md".to_string(), "filename".to_string()),
        ];
        let undated = collect_undated(&db_docs);
        assert_eq!(undated.len(), 1);
        assert_eq!(undated[0].path, "b.md");
        assert_eq!(undated[0].cause, "date_fell_through_to_filesystem");
    }

    #[test]
    fn embedding_dropout_beyond_window() {
        // 5 docs all needing embedding, sorted DESC by created_at already
        let needing = vec![
            ("e.md".to_string(), 500i64),
            ("d.md".to_string(), 400),
            ("c.md".to_string(), 300),
            ("b.md".to_string(), 200), // dropout (oldest 2 beyond window=3)
            ("a.md".to_string(), 100), // dropout
        ];
        let facts = collect_embedding(&needing, Some(3), 10);
        assert_eq!(facts.docs_needing_embedding, 5);
        assert_eq!(facts.embedding_window_size, 3);
        assert_eq!(facts.cap_dropouts.len(), 2);
        // Sorted by path
        assert_eq!(facts.cap_dropouts[0].path, "a.md");
        assert_eq!(facts.cap_dropouts[1].path, "b.md");
        for d in &facts.cap_dropouts {
            assert_eq!(d.cause, "outside_embedding_recency_window");
        }
    }

    #[test]
    fn stale_content_hash_and_deleted() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        fs::write(root.join("current.md"), "content").unwrap();
        fs::write(root.join("changed.md"), "new content").unwrap();
        // "deleted.md" is in DB but not on disk

        let md_files = vec![PathBuf::from("changed.md"), PathBuf::from("current.md")];

        // Compute actual hash for current.md
        let processor = DocumentProcessor::new(root.to_path_buf());
        let current_doc = processor.process_file(Path::new("current.md")).unwrap();
        let changed_doc = processor.process_file(Path::new("changed.md")).unwrap();

        let db_source_refs: std::collections::BTreeSet<String> = vec![
            "changed.md".to_string(),
            "current.md".to_string(),
            "deleted.md".to_string(),
        ]
        .into_iter()
        .collect();

        let mut db_hashes = std::collections::HashMap::new();
        db_hashes.insert("current.md".to_string(), current_doc.content_hash.clone());
        // Stale hash for changed.md
        db_hashes.insert("changed.md".to_string(), "stalehash".to_string());
        db_hashes.insert("deleted.md".to_string(), changed_doc.content_hash.clone());

        let stale = collect_stale(root, &md_files, &db_source_refs, &db_hashes).unwrap();

        assert_eq!(stale.disk_newer_than_db.len(), 1);
        assert_eq!(stale.disk_newer_than_db[0].path, "changed.md");
        assert_eq!(stale.disk_newer_than_db[0].cause, "content_hash_differs");

        assert_eq!(stale.db_rows_for_deleted_files.len(), 1);
        assert_eq!(stale.db_rows_for_deleted_files[0].path, "deleted.md");
        assert_eq!(
            stale.db_rows_for_deleted_files[0].cause,
            "file_absent_on_disk"
        );
    }

    // ---------------------------------------------------------------------------
    // duplicate_sets tests
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn duplicate_sets_grouped_by_hash() {
        let dir = TempDir::new().unwrap();
        let conn = make_test_db(dir.path()).await;

        insert_doc(&conn, "a.md", "abc", "filename").await;
        insert_doc(&conn, "b.md", "abc", "filename").await;
        insert_doc(&conn, "c.md", "xyz", "filename").await;

        let db_path = dir.path().join("enzyme.db");
        let ro_conn = open_read_only_conn(&db_path).await.unwrap();
        let result = collect_duplicate_sets(&ro_conn).await.unwrap();

        assert_eq!(result.duplicate_set_count, 1);
        assert_eq!(result.duplicated_doc_count, 2);
        assert_eq!(result.sets[0].content_hash, "abc");
        assert_eq!(result.sets[0].count, 2);
        assert_eq!(result.sets[0].paths, vec!["a.md", "b.md"]);
    }

    #[tokio::test]
    async fn duplicate_sets_three_way_before_two_way() {
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
        let result = collect_duplicate_sets(&ro_conn).await.unwrap();

        assert_eq!(result.sets.len(), 2);
        assert_eq!(result.sets[0].count, 3); // 3-way first
        assert_eq!(result.sets[1].count, 2);
    }

    #[tokio::test]
    async fn duplicate_sets_tie_broken_by_first_path() {
        let dir = TempDir::new().unwrap();
        let conn = make_test_db(dir.path()).await;

        // Two 2-way sets: "zzz" group has first path "z1.md", "aaa" group has "a1.md"
        // After sort by (count DESC, first_path ASC): "aaa" set should come first
        insert_doc(&conn, "z1.md", "zzz", "filename").await;
        insert_doc(&conn, "z2.md", "zzz", "filename").await;
        insert_doc(&conn, "a1.md", "aaa", "filename").await;
        insert_doc(&conn, "a2.md", "aaa", "filename").await;

        let db_path = dir.path().join("enzyme.db");
        let ro_conn = open_read_only_conn(&db_path).await.unwrap();
        let result = collect_duplicate_sets(&ro_conn).await.unwrap();

        assert_eq!(result.sets.len(), 2);
        // Both have count=2; tiebreak by first path: "a1.md" < "z1.md"
        assert_eq!(result.sets[0].paths[0], "a1.md");
        assert_eq!(result.sets[1].paths[0], "z1.md");
    }

    // ---------------------------------------------------------------------------
    // folder_date_coverage tests
    // ---------------------------------------------------------------------------

    #[test]
    fn folder_date_coverage_dated_fraction_math() {
        // misc/ has 6 docs: 4 filename, 1 frontmatter, 1 filesystem
        // dated = 5, fraction = 5/6 ≈ 0.833
        let db_docs: Vec<(String, String)> = vec![
            ("misc/a.md".to_string(), "filename".to_string()),
            ("misc/b.md".to_string(), "filename".to_string()),
            ("misc/c.md".to_string(), "filename".to_string()),
            ("misc/d.md".to_string(), "filename".to_string()),
            ("misc/e.md".to_string(), "frontmatter".to_string()),
            ("misc/f.md".to_string(), "filesystem".to_string()),
        ];
        let coverage = collect_folder_date_coverage(&db_docs, 6, 5);
        assert_eq!(coverage.folders.len(), 1);
        let f = &coverage.folders[0];
        assert_eq!(f.folder, "misc/");
        assert_eq!(f.doc_count, 6);
        assert_eq!(f.created_source.frontmatter, 1);
        assert_eq!(f.created_source.filename, 4);
        assert_eq!(f.created_source.filesystem, 1);
        // dated_fraction = 5/6
        let frac: f64 = f.dated_fraction.parse().unwrap();
        assert!((frac - 5.0 / 6.0).abs() < 0.001, "fraction was {frac}");
    }

    #[test]
    fn folder_date_coverage_min_size_filter() {
        // stuff/ has 5 docs (meets min), small/ has 4 (excluded)
        let mut db_docs: Vec<(String, String)> = Vec::new();
        for i in 0..5 {
            db_docs.push((format!("stuff/s{i}.md"), "filename".to_string()));
        }
        for i in 0..4 {
            db_docs.push((format!("small/x{i}.md"), "filename".to_string()));
        }
        let coverage = collect_folder_date_coverage(&db_docs, 9, 5);
        assert_eq!(coverage.folders.len(), 1);
        assert_eq!(coverage.folders[0].folder, "stuff/");
    }

    #[test]
    fn folder_date_coverage_sorted_by_count_desc() {
        // stuff/ has 10 docs, misc/ has 5 docs → stuff/ first
        let mut db_docs: Vec<(String, String)> = Vec::new();
        for i in 0..10 {
            db_docs.push((format!("stuff/s{i}.md"), "filename".to_string()));
        }
        for i in 0..5 {
            db_docs.push((format!("misc/m{i}.md"), "frontmatter".to_string()));
        }
        let coverage = collect_folder_date_coverage(&db_docs, 15, 5);
        assert_eq!(coverage.folders.len(), 2);
        assert_eq!(coverage.folders[0].folder, "stuff/");
        assert_eq!(coverage.folders[0].doc_count, 10);
        assert_eq!(coverage.folders[1].folder, "misc/");
        assert_eq!(coverage.folders[1].doc_count, 5);
    }

    #[test]
    fn folder_date_coverage_vault_share_percent() {
        let mut db_docs: Vec<(String, String)> = Vec::new();
        for i in 0..5 {
            db_docs.push((format!("logs/l{i}.md"), "filename".to_string()));
        }
        // 5 docs total, logs/ has all 5 → 100%
        let coverage = collect_folder_date_coverage(&db_docs, 5, 5);
        assert_eq!(coverage.folders[0].vault_share_percent, "100.0");
    }

    // ---------------------------------------------------------------------------
    // Determinism byte-identity (including new sections)
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn determinism_byte_identical() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();

        // Write files that exercise multiple sections
        fs::write(
            root.join("a.md"),
            "---\ntitle: A\ncreated: 2024-01-01\n---\nbody",
        )
        .unwrap();
        fs::write(root.join("b.md"), "---\ntitle: [bad\n---\nbody").unwrap();
        fs::write(root.join("c.json"), "{}").unwrap();

        let conn = make_test_db(root).await;
        insert_doc(&conn, "a.md", "hash_a", "frontmatter").await;
        insert_doc(&conn, "b.md", "hash_dup", "filesystem").await;

        let db_docs = vec![
            ("a.md".to_string(), "frontmatter".to_string()),
            ("b.md".to_string(), "filesystem".to_string()),
        ];
        let db_source_refs: std::collections::BTreeSet<String> =
            db_docs.iter().map(|(sr, _)| sr.clone()).collect();

        let md_files = vec![PathBuf::from("a.md"), PathBuf::from("b.md")];

        let processor = DocumentProcessor::new(root.to_path_buf());
        let mut db_hashes = std::collections::HashMap::new();
        for (sr, _) in &db_docs {
            if let Ok(doc) = processor.process_file(Path::new(sr.as_str())) {
                db_hashes.insert(sr.clone(), doc.content_hash);
            }
        }

        let needing: Vec<(String, i64)> = Vec::new();

        let db_path = root.join("enzyme.db");
        let build_report = || {
            let db_docs = db_docs.clone();
            let db_source_refs = db_source_refs.clone();
            let md_files = md_files.clone();
            let db_hashes = db_hashes.clone();
            let needing = needing.clone();
            let root = root.to_path_buf();
            let db_path = db_path.clone();
            async move {
                let coverage = collect_coverage(&root, &md_files, 2).unwrap();
                let absent = collect_absent(&md_files, &db_source_refs);
                let anomalies = collect_frontmatter(&root, &md_files, &db_docs).unwrap();
                let undated = collect_undated(&db_docs);
                let embedding = collect_embedding(&needing, Some(1024), 2);
                let stale = collect_stale(&root, &md_files, &db_source_refs, &db_hashes).unwrap();
                let ro_conn = open_read_only_conn(&db_path).await.unwrap();
                let dup_sets = collect_duplicate_sets(&ro_conn).await.unwrap();
                let folder_date = collect_folder_date_coverage(&db_docs, 2, 5);
                DoctorReport {
                    vault: root.to_string_lossy().to_string(),
                    database: root.join(".enzyme/enzyme.db").to_string_lossy().to_string(),
                    coverage,
                    md_files_absent_from_db: absent,
                    frontmatter_anomalies: anomalies,
                    undated_docs: undated,
                    embedding,
                    stale_index: stale,
                    duplicate_sets: dup_sets,
                    folder_date_coverage: folder_date,
                }
            }
        };

        let r1 = build_report().await;
        let r2 = build_report().await;

        let json1 = serde_json::to_string_pretty(&r1).unwrap();
        let json2 = serde_json::to_string_pretty(&r2).unwrap();
        assert_eq!(json1, json2, "JSON output must be deterministic");

        let human1 = render_human(&r1);
        let human2 = render_human(&r2);
        assert_eq!(human1, human2, "human output must be deterministic");
    }

    #[tokio::test]
    async fn not_initialized_errors_cleanly() {
        let dir = TempDir::new().unwrap();
        // No .enzyme directory → vault_db won't exist
        let paths = EnzymePaths::new(dir.path()).unwrap();
        assert!(
            !paths.vault_db.exists(),
            "vault_db should not exist for empty dir"
        );
    }
}
