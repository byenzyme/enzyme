use std::io::{self, Write, stdout};
use std::sync::mpsc;
use std::time::Duration;

use crossterm::{
    ExecutableCommand, cursor,
    terminal::{self, ClearType},
};

use enzyme_core::models::EntityType;
use enzyme_core::pipeline::selection::SelectionSource;

use super::colors;
use super::spinner::HelixSpinner;
use super::tree;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionStatus {
    Done,
    Degraded,
    Failed,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Format an epoch-millisecond timestamp as "Mon YYYY" (e.g. "Mar 2024").
fn format_epoch_ms_short(ms: i64) -> String {
    use chrono::{DateTime, Utc};
    let dt = DateTime::from_timestamp_millis(ms).unwrap_or_else(|| Utc::now());
    dt.format("%b %Y").to_string()
}

// ---------------------------------------------------------------------------
// Display events (sent from pipeline stages)
// ---------------------------------------------------------------------------

/// Events sent to the display thread from pipeline stages.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum DisplayEvent {
    // API config
    ApiConfig {
        base_url: String,
        model: String,
        masked_key: String,
    },

    // Indexing
    IndexingStart,
    IndexingProgress(String),
    IndexingComplete {
        files_discovered: usize,
        files_new: usize,
        files_updated: usize,
        files_deleted: usize,
    },

    // Selection
    SelectionComplete {
        count: usize,
        source: SelectionSource,
    },

    // Discovery (catalyst generation)
    DiscoveryStart {
        total: usize,
    },
    EntityStarted {
        entity_name: String,
        entity_type: EntityType,
    },
    EntityDiscovered {
        entity_name: String,
        entity_type: EntityType,
        catalyst_texts: Vec<String>,
        topic_names: Vec<String>,
        completed: usize,
        total: usize,
    },
    EntitySkipped {
        entity_name: String,
        completed: usize,
        total: usize,
    },
    EntityFailed {
        entity_name: String,
        entity_type: EntityType,
        error: String,
        completed: usize,
        total: usize,
    },
    DiscoveryComplete {
        entities_processed: usize,
        catalysts_generated: usize,
        errors_count: usize,
        rate_limited: usize,
    },

    // Embedding
    EmbeddingStart,
    EmbeddingProgress {
        done: usize,
        total: usize,
        /// `created_at` (epoch ms) of the document currently being processed.
        current_date_ms: Option<i64>,
    },
    EmbeddingComplete {
        chunks: usize,
        pruned: usize,
        /// `created_at` (epoch ms) of the oldest embedded document.
        oldest_date_ms: Option<i64>,
        encode_ms: u64,
    },
    EmbeddingSkipped(String),
    EmbeddingFailed(String),

    // Similarity
    SimilarityStart,
    SimilarityProgress {
        done: usize,
        total: usize,
    },
    SimilarityComplete {
        catalysts: usize,
        documents: usize,
    },

    // API
    ApiSkipped(String),

    // Done
    Done {
        duration_ms: u64,
        status: CompletionStatus,
        message: Option<String>,
    },
}

// ---------------------------------------------------------------------------
// Rolling window entry for discovery display
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct DiscoveryEntry {
    entity_name: String,
    entity_type: EntityType,
    catalysts: Vec<String>,
}

// ---------------------------------------------------------------------------
// Display state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)]
enum DisplayStage {
    Idle,
    Indexing,
    Selecting,
    Discovery,
    Embedding,
    Similarity,
    Complete,
}

// ---------------------------------------------------------------------------
// InitDisplay
// ---------------------------------------------------------------------------

/// Orchestrates animated terminal output for the init pipeline.
///
/// Spawns a background thread that receives `DisplayEvent`s and renders
/// an animated display with helix spinner, heatmap rolling window, and
/// tree connectors.
#[allow(dead_code)]
pub struct InitDisplay {
    sender: Option<mpsc::Sender<DisplayEvent>>,
    handle: Option<std::thread::JoinHandle<()>>,
    is_tty: bool,
}

impl InitDisplay {
    /// Start the display. If stdout is not a TTY, falls back to plain text.
    pub fn start() -> Self {
        let is_tty = atty::is(atty::Stream::Stdout);
        let (tx, rx) = mpsc::channel();

        let handle = std::thread::spawn(move || {
            if is_tty {
                run_animated_display(rx);
            } else {
                run_plain_display(rx);
            }
        });

        Self {
            sender: Some(tx),
            handle: Some(handle),
            is_tty,
        }
    }

    /// Start in quiet mode — drains all events without any output.
    pub fn start_quiet() -> Self {
        let (tx, rx) = mpsc::channel();

        let handle = std::thread::spawn(move || {
            // Drain events silently until Done or channel close
            while let Ok(event) = rx.recv() {
                if matches!(event, DisplayEvent::Done { .. }) {
                    return;
                }
            }
        });

        Self {
            sender: Some(tx),
            handle: Some(handle),
            is_tty: false,
        }
    }

    /// Start in JSON streaming mode — emits one JSON object per line to stdout.
    pub fn start_json() -> Self {
        let (tx, rx) = mpsc::channel();

        let handle = std::thread::spawn(move || {
            run_json_display(rx);
        });

        Self {
            sender: Some(tx),
            handle: Some(handle),
            is_tty: false,
        }
    }

    /// Get a clone of the sender for sending events.
    pub fn sender(&self) -> mpsc::Sender<DisplayEvent> {
        self.sender
            .as_ref()
            .expect("display already finished")
            .clone()
    }

    /// Send an event to the display.
    #[allow(dead_code)]
    pub fn send(&self, event: DisplayEvent) {
        if let Some(ref tx) = self.sender {
            let _ = tx.send(event);
        }
    }

    /// Whether the display is rendering to a TTY.
    #[allow(dead_code)]
    pub fn is_tty(&self) -> bool {
        self.is_tty
    }

    /// Signal completion and wait for the display thread to finish.
    pub fn finish(mut self) {
        // Drop sender to signal the display thread to exit
        self.sender.take();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

// ---------------------------------------------------------------------------
// Animated display (TTY)
// ---------------------------------------------------------------------------

const ROLLING_WINDOW_SIZE: usize = 10;
const FRAME_INTERVAL: Duration = Duration::from_millis(100); // ~10fps

#[allow(unused_assignments)]
fn run_animated_display(rx: mpsc::Receiver<DisplayEvent>) {
    let mut out = stdout();
    let mut spinner = HelixSpinner::new();
    let mut stage = DisplayStage::Idle;
    let mut rolling_window: Vec<DiscoveryEntry> = Vec::new();
    let mut failed_entries: Vec<DiscoveryEntry> = Vec::new();
    let mut discovery_completed: usize = 0;
    let mut discovery_total: usize = 0;
    let mut lines_written: u16 = 0;
    let mut frame_counter: usize = 0;
    let mut catalysts_generated: Option<usize> = None;
    let mut catalyst_errors: Option<(usize, usize)> = None; // (errors_count, rate_limited)
    let mut embedding_status: Option<String> = None;
    let mut embedding_done_line: Option<String> = None;

    // Keep indexing messages for display
    let mut _indexing_messages: Vec<String> = Vec::new();

    // Helper: process a single event, updating state. Returns true if Done.
    #[allow(unused_assignments)]
    let handle_event = |event: DisplayEvent,
                        out: &mut io::Stdout,
                        stage: &mut DisplayStage,
                        rolling_window: &mut Vec<DiscoveryEntry>,
                        failed_entries: &mut Vec<DiscoveryEntry>,
                        discovery_completed: &mut usize,
                        discovery_total: &mut usize,
                        lines_written: &mut u16,
                        spinner: &mut HelixSpinner,
                        catalysts_generated: &mut Option<usize>,
                        catalyst_errors: &mut Option<(usize, usize)>,
                        embedding_status: &mut Option<String>,
                        embedding_done_line: &mut Option<String>|
     -> bool {
        match event {
            DisplayEvent::ApiConfig {
                base_url,
                model,
                masked_key,
            } => {
                let _ = writeln!(
                    out,
                    "  {}LLM:{} {} @ {} (key: {})",
                    crossterm::style::SetForegroundColor(colors::ENZYME_PURPLE),
                    crossterm::style::ResetColor,
                    model,
                    base_url,
                    masked_key,
                );
            }
            DisplayEvent::IndexingStart => {
                *stage = DisplayStage::Indexing;
            }
            DisplayEvent::IndexingProgress(msg) => {
                let _ = msg; // stored if needed later
            }
            DisplayEvent::IndexingComplete {
                files_discovered,
                files_new,
                files_updated,
                files_deleted,
            } => {
                clear_lines(out, *lines_written);
                *lines_written = 0;
                *stage = DisplayStage::Idle;
                let _ = writeln!(
                    out,
                    "  {}Indexed:{} {} discovered, {} new, {} updated, {} deleted",
                    crossterm::style::SetForegroundColor(colors::FOREST_GREEN),
                    crossterm::style::ResetColor,
                    files_discovered,
                    files_new,
                    files_updated,
                    files_deleted,
                );
            }
            DisplayEvent::SelectionComplete {
                count: _,
                source: _,
            } => {
                *stage = DisplayStage::Selecting;
            }
            DisplayEvent::DiscoveryStart { total } => {
                *stage = DisplayStage::Discovery;
                *discovery_total = total;
                *discovery_completed = 0;
                rolling_window.clear();
                failed_entries.clear();
            }
            DisplayEvent::EntityStarted {
                entity_name: _,
                entity_type: _,
            } => {
                // Don't add to window yet — wait for first catalyst partial
                // or completion so entities appear when they have content.
            }
            DisplayEvent::EntityDiscovered {
                entity_name,
                entity_type,
                catalyst_texts,
                topic_names: _,
                completed,
                total,
            } => {
                *discovery_completed = completed;
                *discovery_total = total;
                rolling_window.push(DiscoveryEntry {
                    entity_name,
                    entity_type,
                    catalysts: catalyst_texts,
                });
                if rolling_window.len() > ROLLING_WINDOW_SIZE {
                    rolling_window.remove(0);
                }
            }
            DisplayEvent::EntitySkipped {
                entity_name: _,
                completed,
                total,
            } => {
                *discovery_completed = completed;
                *discovery_total = total;
            }
            DisplayEvent::EntityFailed {
                entity_name,
                entity_type,
                error,
                completed,
                total,
            } => {
                *discovery_completed = completed;
                *discovery_total = total;
                let short_err = if error.chars().count() > 60 {
                    let end = error.char_indices().nth(57).map_or(error.len(), |(i, _)| i);
                    format!("{}…", &error[..end])
                } else {
                    error
                };
                failed_entries.push(DiscoveryEntry {
                    entity_name,
                    entity_type,
                    catalysts: vec![short_err],
                });
            }
            DisplayEvent::DiscoveryComplete {
                entities_processed: _,
                catalysts_generated: count,
                errors_count,
                rate_limited,
            } => {
                // Flush the cycling entity tree and print catalyst summary immediately
                clear_lines(out, *lines_written);
                *lines_written = 0;

                let err_suffix = if errors_count > 0 && rate_limited > 0 {
                    format!(" ({} failed, {} rate-limited)", errors_count, rate_limited)
                } else if errors_count > 0 {
                    format!(" ({} failed)", errors_count)
                } else {
                    String::new()
                };
                let _ = writeln!(
                    out,
                    "  {}Catalysts:{} {} generated{}",
                    crossterm::style::SetForegroundColor(colors::CATALYZING_ORANGE),
                    crossterm::style::ResetColor,
                    count,
                    err_suffix,
                );
                // Mark catalysts as printed so EmbeddingComplete doesn't re-print
                *catalysts_generated = None;
                *catalyst_errors = None;
                *stage = DisplayStage::Embedding;
            }
            DisplayEvent::EmbeddingStart => {
                embedding_status.replace("Embedding...".to_string());
            }
            DisplayEvent::EmbeddingProgress {
                done,
                total,
                current_date_ms,
            } => {
                let label = current_date_ms
                    .map(|ms| {
                        format!(
                            "Embedding docs [{}/{}] [\u{2190} {}]",
                            done,
                            total,
                            format_epoch_ms_short(ms)
                        )
                    })
                    .unwrap_or_else(|| format!("Embedding docs [{}/{}]", done, total));
                embedding_status.replace(label);
            }
            DisplayEvent::EmbeddingComplete {
                chunks,
                pruned,
                oldest_date_ms,
                encode_ms,
            } => {
                embedding_status.take();
                let summary =
                    format_embedding_complete_summary(chunks, pruned, oldest_date_ms, encode_ms);
                *embedding_done_line = Some(format!(
                    "  {}Embedded:{} {}",
                    crossterm::style::SetForegroundColor(colors::FOREST_GREEN),
                    crossterm::style::ResetColor,
                    summary,
                ));

                // Print immediately only when discovery is not active.
                // During discovery, cache the finished line so it renders once
                // above the tree without double-printing.
                if should_print_embedding_done_immediately(stage) {
                    clear_lines(out, *lines_written);
                    *lines_written = 0;
                    *stage = DisplayStage::Embedding;
                    if let Some(line) = embedding_done_line.as_ref() {
                        let _ = writeln!(out, "{}", line);
                        *lines_written = 1;
                    }
                }
            }
            DisplayEvent::EmbeddingSkipped(reason) => {
                // EmbeddingSkipped fires early (before discovery), so flush
                // any cycling display if it's active.
                if *stage == DisplayStage::Discovery && !rolling_window.is_empty() {
                    clear_lines(out, *lines_written);
                    *lines_written = 0;
                }
                embedding_status.take();
                let _ = writeln!(
                    out,
                    "  {}Embedding skipped:{} {}",
                    crossterm::style::SetForegroundColor(crossterm::style::Color::DarkGrey),
                    crossterm::style::ResetColor,
                    reason,
                );
            }
            DisplayEvent::EmbeddingFailed(err) => {
                clear_lines(out, *lines_written);
                *lines_written = 0;
                *stage = DisplayStage::Embedding;
                embedding_status.take();

                let _ = writeln!(
                    out,
                    "  {}Embedding failed:{} {}",
                    crossterm::style::SetForegroundColor(crossterm::style::Color::Red),
                    crossterm::style::ResetColor,
                    err,
                );
            }
            DisplayEvent::ApiSkipped(reason) => {
                let _ = writeln!(
                    out,
                    "  {}API:{} {}",
                    crossterm::style::SetForegroundColor(crossterm::style::Color::DarkGrey),
                    crossterm::style::ResetColor,
                    reason,
                );
            }
            DisplayEvent::SimilarityStart => {
                *stage = DisplayStage::Similarity;
            }
            DisplayEvent::SimilarityProgress { done, total } => {
                if total > 0 && done % 10 == 0 {
                    clear_lines(out, *lines_written);
                    *lines_written = 0;
                    let frame = spinner.tick();
                    let _ = writeln!(
                        out,
                        "  {}{}{} Similarities [{}/{}]",
                        crossterm::style::SetForegroundColor(colors::ENZYME_PURPLE),
                        frame,
                        crossterm::style::ResetColor,
                        done,
                        total,
                    );
                    *lines_written = 1;
                }
            }
            DisplayEvent::SimilarityComplete {
                catalysts,
                documents,
            } => {
                clear_lines(out, *lines_written);
                *lines_written = 0;
                let _ = writeln!(
                    out,
                    "  {}Similarities:{} {} catalysts \u{00d7} {} documents",
                    crossterm::style::SetForegroundColor(colors::FOREST_GREEN),
                    crossterm::style::ResetColor,
                    catalysts,
                    documents,
                );
            }
            DisplayEvent::Done {
                duration_ms,
                status,
                message,
            } => {
                clear_lines(out, *lines_written);
                let secs = duration_ms as f64 / 1000.0;
                let (label, color) = match status {
                    CompletionStatus::Done => ("Done!", colors::FOREST_GREEN),
                    CompletionStatus::Degraded => {
                        ("Completed with warnings", crossterm::style::Color::Yellow)
                    }
                    CompletionStatus::Failed => ("Failed", crossterm::style::Color::Red),
                };
                let _ = writeln!(
                    out,
                    "  {}{}{} ({:.1}s)",
                    crossterm::style::SetForegroundColor(color),
                    label,
                    crossterm::style::ResetColor,
                    secs,
                );
                if let Some(message) = message {
                    let _ = writeln!(out, "  {}", message);
                }
                let _ = out.flush();
                return true;
            }
        }
        false
    };

    loop {
        // Wait for at least one event or a frame timeout
        match rx.recv_timeout(FRAME_INTERVAL) {
            Ok(event) => {
                if handle_event(
                    event,
                    &mut out,
                    &mut stage,
                    &mut rolling_window,
                    &mut failed_entries,
                    &mut discovery_completed,
                    &mut discovery_total,
                    &mut lines_written,
                    &mut spinner,
                    &mut catalysts_generated,
                    &mut catalyst_errors,
                    &mut embedding_status,
                    &mut embedding_done_line,
                ) {
                    return;
                }
                // Drain all pending events before rendering
                while let Ok(event) = rx.try_recv() {
                    if handle_event(
                        event,
                        &mut out,
                        &mut stage,
                        &mut rolling_window,
                        &mut failed_entries,
                        &mut discovery_completed,
                        &mut discovery_total,
                        &mut lines_written,
                        &mut spinner,
                        &mut catalysts_generated,
                        &mut catalyst_errors,
                        &mut embedding_status,
                        &mut embedding_done_line,
                    ) {
                        return;
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Fall through to render
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                clear_lines(&mut out, lines_written);
                let _ = out.flush();
                return;
            }
        }

        // Render once per frame
        frame_counter = frame_counter.wrapping_add(1);
        if stage == DisplayStage::Discovery && !rolling_window.is_empty() {
            clear_lines(&mut out, lines_written);
            lines_written = 0;

            // Show embedding completion above the entity tree
            if let Some(line) = embedding_done_line.as_ref() {
                let _ = writeln!(&mut out, "{}", line);
                lines_written += 1;
            }

            let frame = spinner.tick();

            // Show discovery header while entities are still generating,
            // otherwise show the catalysts summary
            if catalysts_generated.is_none() {
                let _ = tree::render_discovery_header(
                    &mut out,
                    discovery_completed,
                    discovery_total,
                    frame,
                );
                lines_written += 1;
            }

            let window_len = rolling_window.len();
            let has_failed = !failed_entries.is_empty();
            for (i, entry) in rolling_window.iter().enumerate() {
                let is_last = i == window_len - 1 && !has_failed && embedding_status.is_none();
                let display_text = if entry.catalysts.is_empty() {
                    None
                } else {
                    let idx = (frame_counter + i * 3) % entry.catalysts.len();
                    Some(entry.catalysts[idx].as_str())
                };
                let _ = tree::render_entity_node(
                    &mut out,
                    &entry.entity_name,
                    &entry.entity_type,
                    display_text,
                    i,
                    window_len,
                    is_last,
                    false,
                );
                lines_written += 1;
            }
            // Failed entities pinned at bottom in red
            let failed_len = failed_entries.len();
            for (i, entry) in failed_entries.iter().enumerate() {
                let is_last = i == failed_len - 1 && embedding_status.is_none();
                let display_text = entry.catalysts.first().map(|s| s.as_str());
                let _ = tree::render_entity_node(
                    &mut out,
                    &entry.entity_name,
                    &entry.entity_type,
                    display_text,
                    i,
                    failed_len,
                    is_last,
                    true,
                );
                lines_written += 1;
            }

            // Show embedding progress below the entity tree
            if let Some(ref status) = embedding_status {
                let _ = writeln!(
                    &mut out,
                    "  {} {}{}{} {}",
                    tree::PIPE,
                    crossterm::style::SetForegroundColor(colors::ENZYME_PURPLE),
                    frame,
                    crossterm::style::ResetColor,
                    status,
                );
                lines_written += 1;
            }

            let _ = out.flush();
        } else if stage == DisplayStage::Indexing {
            clear_lines(&mut out, lines_written);
            lines_written = 0;

            let frame = spinner.tick();
            let _ = writeln!(
                out,
                "  {}{}{}  Indexing vault...",
                crossterm::style::SetForegroundColor(colors::ENZYME_PURPLE),
                frame,
                crossterm::style::ResetColor,
            );
            lines_written = 1;

            let _ = out.flush();
        } else if embedding_status.is_some() {
            // Standalone embedding spinner (no discovery tree, e.g. target preparation)
            clear_lines(&mut out, lines_written);
            lines_written = 0;

            let frame = spinner.tick();
            let _ = writeln!(
                out,
                "  {}{}{}  {}",
                crossterm::style::SetForegroundColor(colors::ENZYME_PURPLE),
                frame,
                crossterm::style::ResetColor,
                embedding_status.as_deref().unwrap_or("Embedding..."),
            );
            lines_written = 1;

            let _ = out.flush();
        }
    }
}

// ---------------------------------------------------------------------------
// Plain display (non-TTY / piped)
// ---------------------------------------------------------------------------

fn run_plain_display(rx: mpsc::Receiver<DisplayEvent>) {
    loop {
        match rx.recv() {
            Ok(event) => match event {
                DisplayEvent::ApiConfig {
                    base_url,
                    model,
                    masked_key,
                } => {
                    println!("  LLM: {} @ {} (key: {})", model, base_url, masked_key);
                }
                DisplayEvent::IndexingStart => {
                    println!("Indexing vault...");
                }
                DisplayEvent::IndexingProgress(msg) => {
                    println!("  {}", msg);
                }
                DisplayEvent::IndexingComplete {
                    files_discovered,
                    files_new,
                    files_updated,
                    files_deleted,
                } => {
                    println!(
                        "  Indexed: {} discovered, {} new, {} updated, {} deleted",
                        files_discovered, files_new, files_updated, files_deleted
                    );
                }
                DisplayEvent::SelectionComplete { .. } => {}
                DisplayEvent::DiscoveryStart { total } => {
                    println!("Generating catalysts for {} entities...", total);
                }
                DisplayEvent::EntityStarted { .. } => {
                    // Not shown in plain mode
                }
                DisplayEvent::EntityDiscovered {
                    entity_name,
                    completed,
                    total,
                    ..
                } => {
                    println!("  [catalyst] [{}/{}] {}", completed, total, entity_name);
                }
                DisplayEvent::EntitySkipped {
                    completed, total, ..
                } => {
                    println!("  [skipped] [{}/{}]", completed, total);
                }
                DisplayEvent::EntityFailed {
                    entity_name,
                    entity_type: _,
                    error,
                    completed,
                    total,
                } => {
                    println!(
                        "  [catalyst] [{}/{}] {} (failed: {})",
                        completed, total, entity_name, error
                    );
                }
                DisplayEvent::DiscoveryComplete {
                    entities_processed: _,
                    catalysts_generated,
                    errors_count,
                    rate_limited,
                } => {
                    let err_suffix = if errors_count > 0 && rate_limited > 0 {
                        format!(" ({} failed, {} rate-limited)", errors_count, rate_limited)
                    } else if errors_count > 0 {
                        format!(" ({} failed)", errors_count)
                    } else {
                        String::new()
                    };
                    println!(
                        "  Catalysts: {} generated{}",
                        catalysts_generated, err_suffix,
                    );
                }
                DisplayEvent::EmbeddingStart => {
                    println!("Generating embeddings...");
                }
                DisplayEvent::EmbeddingProgress {
                    done,
                    total,
                    current_date_ms,
                } => {
                    if let Some(ms) = current_date_ms {
                        println!(
                            "  Embedding docs [{}/{}] [← {}]",
                            done,
                            total,
                            format_epoch_ms_short(ms)
                        );
                    } else {
                        println!("  Embedding docs [{}/{}]", done, total);
                    }
                }
                DisplayEvent::EmbeddingComplete {
                    chunks,
                    pruned,
                    oldest_date_ms,
                    encode_ms,
                } => {
                    let summary = format_embedding_complete_summary(
                        chunks,
                        pruned,
                        oldest_date_ms,
                        encode_ms,
                    );
                    println!("  Embedded: {}", summary,);
                }
                DisplayEvent::EmbeddingSkipped(reason) => {
                    println!("  Embedding skipped: {}", reason);
                }
                DisplayEvent::EmbeddingFailed(err) => {
                    println!("  Embedding failed: {}", err);
                }
                DisplayEvent::ApiSkipped(reason) => {
                    println!("  {}", reason);
                }
                DisplayEvent::SimilarityStart => {
                    println!("Computing similarities...");
                }
                DisplayEvent::SimilarityProgress { done, total } => {
                    if total > 0 && done % 10 == 0 {
                        println!("  [{}/{}] catalysts processed", done, total);
                    }
                }
                DisplayEvent::SimilarityComplete {
                    catalysts,
                    documents,
                } => {
                    println!(
                        "  Similarities: {} catalysts \u{00d7} {} documents",
                        catalysts, documents
                    );
                }
                DisplayEvent::Done {
                    duration_ms,
                    status,
                    message,
                } => {
                    let secs = duration_ms as f64 / 1000.0;
                    let label = match status {
                        CompletionStatus::Done => "Done!",
                        CompletionStatus::Degraded => "Completed with warnings",
                        CompletionStatus::Failed => "Failed",
                    };
                    println!("{} ({:.1}s)", label, secs);
                    if let Some(message) = message {
                        println!("  {}", message);
                    }
                    return;
                }
            },
            Err(_) => return,
        }
    }
}

// ---------------------------------------------------------------------------
// JSON streaming display (for desktop app / programmatic consumers)
// ---------------------------------------------------------------------------

fn run_json_display(rx: mpsc::Receiver<DisplayEvent>) {
    use serde_json::json;

    loop {
        match rx.recv() {
            Ok(event) => {
                let obj = match event {
                    DisplayEvent::ApiConfig {
                        base_url,
                        model,
                        masked_key,
                    } => {
                        json!({"stage": "api_config", "base_url": base_url, "model": model, "key": masked_key})
                    }
                    DisplayEvent::IndexingStart => {
                        json!({"stage": "indexing", "status": "start"})
                    }
                    DisplayEvent::IndexingProgress(msg) => {
                        json!({"stage": "indexing", "status": "progress", "message": msg})
                    }
                    DisplayEvent::IndexingComplete {
                        files_discovered,
                        files_new,
                        files_updated,
                        files_deleted,
                    } => {
                        json!({"stage": "indexing", "status": "complete", "files": files_discovered, "new": files_new, "updated": files_updated, "deleted": files_deleted})
                    }
                    DisplayEvent::SelectionComplete { count, .. } => {
                        json!({"stage": "selection", "status": "complete", "entities": count})
                    }
                    DisplayEvent::DiscoveryStart { total } => {
                        json!({"stage": "catalysts", "status": "start", "total": total})
                    }
                    DisplayEvent::EntityStarted {
                        entity_name,
                        entity_type,
                    } => {
                        json!({"stage": "catalysts", "status": "entity_started", "entity": entity_name, "type": entity_type.as_str()})
                    }
                    DisplayEvent::EntityDiscovered {
                        entity_name,
                        entity_type,
                        catalyst_texts,
                        topic_names,
                        completed,
                        total,
                    } => {
                        json!({"stage": "catalysts", "status": "entity_done", "entity": entity_name, "type": entity_type.as_str(), "catalysts": catalyst_texts, "topics": topic_names, "completed": completed, "total": total})
                    }
                    DisplayEvent::EntitySkipped {
                        entity_name,
                        completed,
                        total,
                    } => {
                        json!({"stage": "catalysts", "status": "entity_skipped", "entity": entity_name, "completed": completed, "total": total})
                    }
                    DisplayEvent::EntityFailed {
                        entity_name,
                        entity_type,
                        error,
                        completed,
                        total,
                    } => {
                        json!({"stage": "catalysts", "status": "entity_failed", "entity": entity_name, "type": entity_type.as_str(), "error": error, "completed": completed, "total": total})
                    }
                    DisplayEvent::DiscoveryComplete {
                        entities_processed,
                        catalysts_generated,
                        errors_count,
                        rate_limited,
                    } => {
                        json!({"stage": "catalysts", "status": "complete", "entities": entities_processed, "catalysts": catalysts_generated, "errors": errors_count, "rate_limited": rate_limited})
                    }
                    DisplayEvent::EmbeddingStart => {
                        json!({"stage": "embedding", "status": "start"})
                    }
                    DisplayEvent::EmbeddingProgress {
                        done,
                        total,
                        current_date_ms,
                    } => {
                        json!({"stage": "embedding", "status": "progress", "documents_done": done, "documents_total": total, "current_date_ms": current_date_ms})
                    }
                    DisplayEvent::EmbeddingComplete {
                        chunks,
                        pruned,
                        oldest_date_ms,
                        encode_ms,
                    } => {
                        json!({"stage": "embedding", "status": "complete", "chunks": chunks, "pruned": pruned, "oldest_date_ms": oldest_date_ms, "encode_ms": encode_ms})
                    }
                    DisplayEvent::EmbeddingSkipped(reason) => {
                        json!({"stage": "embedding", "status": "skipped", "reason": reason})
                    }
                    DisplayEvent::EmbeddingFailed(err) => {
                        json!({"stage": "embedding", "status": "failed", "error": err})
                    }
                    DisplayEvent::ApiSkipped(reason) => {
                        json!({"stage": "api", "status": "skipped", "reason": reason})
                    }
                    DisplayEvent::SimilarityStart => {
                        json!({"stage": "similarity", "status": "start"})
                    }
                    DisplayEvent::SimilarityProgress { done, total } => {
                        json!({"stage": "similarity", "status": "progress", "done": done, "total": total})
                    }
                    DisplayEvent::SimilarityComplete {
                        catalysts,
                        documents,
                    } => {
                        json!({"stage": "similarity", "status": "complete", "catalysts": catalysts, "documents": documents})
                    }
                    DisplayEvent::Done {
                        duration_ms,
                        status,
                        message,
                    } => {
                        let status = match status {
                            CompletionStatus::Done => "done",
                            CompletionStatus::Degraded => "degraded",
                            CompletionStatus::Failed => "failed",
                        };
                        let obj = json!({"stage": "done", "status": status, "message": message, "duration_ms": duration_ms});
                        eprintln!("{}", serde_json::to_string(&obj).unwrap());
                        return;
                    }
                };
                eprintln!("{}", serde_json::to_string(&obj).unwrap());
            }
            Err(_) => return,
        }
    }
}

// ---------------------------------------------------------------------------
// Terminal helpers
// ---------------------------------------------------------------------------

fn clear_lines(out: &mut io::Stdout, count: u16) {
    if count == 0 {
        return;
    }
    let _ = out.execute(cursor::MoveUp(count));
    for _ in 0..count {
        let _ = out.execute(terminal::Clear(ClearType::CurrentLine));
        let _ = out.execute(cursor::MoveDown(1));
    }
    let _ = out.execute(cursor::MoveUp(count));
}

fn should_print_embedding_done_immediately(stage: &DisplayStage) -> bool {
    *stage != DisplayStage::Discovery
}

fn format_chunk_rate(chunks: usize, encode_ms: u64) -> String {
    let encode_secs = encode_ms as f64 / 1000.0;
    let rate = if encode_secs > 0.0 {
        chunks as f64 / encode_secs
    } else {
        0.0
    };
    if rate >= 1_000_000.0 {
        format!("{:.1}M chunks/sec", rate / 1_000_000.0)
    } else if rate >= 1_000.0 {
        format!("{:.0}K chunks/sec", rate / 1_000.0)
    } else {
        format!("{:.0} chunks/sec", rate)
    }
}

fn format_embedding_complete_summary(
    chunks: usize,
    pruned: usize,
    oldest_date_ms: Option<i64>,
    encode_ms: u64,
) -> String {
    let date_label = oldest_date_ms
        .map(|ms| format!("back to {}", format_epoch_ms_short(ms)))
        .unwrap_or_else(|| "up to date".to_string());

    let mut summary = date_label;
    if pruned > 0 {
        summary.push_str(&format!(", {} pruned", pruned));
    }
    summary.push_str(&format!(" ({})", format_chunk_rate(chunks, encode_ms)));
    summary
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_rate_formats_large_values() {
        assert_eq!(format_chunk_rate(2_500, 1_000), "2K chunks/sec");
        assert_eq!(format_chunk_rate(1_500_000, 1_000), "1.5M chunks/sec");
    }

    #[test]
    fn embedding_summary_uses_full_payload() {
        let summary = format_embedding_complete_summary(12_345, 3, Some(0), 500);

        assert!(summary.contains("back to"));
        assert!(summary.contains("3 pruned"));
        assert!(summary.contains("25K chunks/sec"));
    }

    #[test]
    fn embedding_progress_label_is_doc_oriented() {
        let done = 4;
        let total = 9;
        let label = Some(0)
            .map(|ms| {
                format!(
                    "Embedding docs [{}/{}] [← {}]",
                    done,
                    total,
                    format_epoch_ms_short(ms)
                )
            })
            .unwrap();

        assert!(label.contains("Embedding docs [4/9]"));
    }

    #[test]
    fn embedding_done_prints_immediately_only_outside_discovery() {
        assert!(should_print_embedding_done_immediately(
            &DisplayStage::Embedding
        ));
        assert!(should_print_embedding_done_immediately(
            &DisplayStage::Similarity
        ));
        assert!(!should_print_embedding_done_immediately(
            &DisplayStage::Discovery
        ));
    }
}
