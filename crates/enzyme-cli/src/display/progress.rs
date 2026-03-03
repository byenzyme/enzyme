use std::io::{self, Write, stdout};
use std::sync::mpsc;
use std::time::Duration;

use crossterm::{
    cursor,
    terminal::{self, ClearType},
    ExecutableCommand,
};

use enzyme_core::models::EntityType;
use enzyme_core::pipeline::selection::SelectionSource;

use super::colors;
use super::spinner::HelixSpinner;
use super::tree;

// ---------------------------------------------------------------------------
// Display events (sent from pipeline stages)
// ---------------------------------------------------------------------------

/// Events sent to the display thread from pipeline stages.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum DisplayEvent {
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
    CatalystPartial {
        entity_name: String,
        catalyst_text: String,
        catalyst_index: usize,
    },
    EntityDiscovered {
        entity_name: String,
        entity_type: EntityType,
        catalyst_texts: Vec<String>,
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
    },

    // Embedding
    EmbeddingStart,
    EmbeddingProgress {
        done: usize,
        total: usize,
    },
    EmbeddingComplete {
        documents: usize,
        pruned: usize,
        duration_ms: u64,
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

    // Starter prompt
    StarterPrompt(String),

    // Done
    Done {
        duration_ms: u64,
    },
}

// ---------------------------------------------------------------------------
// Rolling window entry for discovery display
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum EntryStatus {
    Completed,
    Failed,
}

#[derive(Debug, Clone)]
struct DiscoveryEntry {
    entity_name: String,
    entity_type: EntityType,
    catalysts: Vec<String>,
    status: EntryStatus,
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
            // Drain events silently until the channel closes
            while rx.recv().is_ok() {}
        });

        Self {
            sender: Some(tx),
            handle: Some(handle),
            is_tty: false,
        }
    }

    /// Get a clone of the sender for sending events.
    pub fn sender(&self) -> mpsc::Sender<DisplayEvent> {
        self.sender.as_ref().expect("display already finished").clone()
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
    let mut discovery_completed: usize = 0;
    let mut discovery_total: usize = 0;
    let mut lines_written: u16 = 0;
    let mut frame_counter: usize = 0;
    let mut catalysts_generated: Option<usize> = None;
    let mut embedding_status: Option<String> = None;

    // Keep indexing messages for display
    let mut _indexing_messages: Vec<String> = Vec::new();

    // Helper: process a single event, updating state. Returns true if Done.
    #[allow(unused_assignments)]
    let handle_event = |event: DisplayEvent,
                            out: &mut io::Stdout,
                            stage: &mut DisplayStage,
                            rolling_window: &mut Vec<DiscoveryEntry>,
                            discovery_completed: &mut usize,
                            discovery_total: &mut usize,
                            lines_written: &mut u16,
                            spinner: &mut HelixSpinner,
                            catalysts_generated: &mut Option<usize>,
                            embedding_status: &mut Option<String>|
     -> bool {
        match event {
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
            DisplayEvent::SelectionComplete { count: _, source: _ } => {
                *stage = DisplayStage::Selecting;
            }
            DisplayEvent::DiscoveryStart { total } => {
                *stage = DisplayStage::Discovery;
                *discovery_total = total;
                *discovery_completed = 0;
                rolling_window.clear();
            }
            DisplayEvent::EntityStarted {
                entity_name: _,
                entity_type: _,
            } => {
                // Don't add to window yet — wait for first catalyst partial
                // or completion so entities appear when they have content.
            }
            DisplayEvent::CatalystPartial {
                entity_name,
                catalyst_text,
                catalyst_index: _,
            } => {
                if let Some(entry) = rolling_window.iter_mut().find(|e| e.entity_name == entity_name) {
                    entry.catalysts.push(catalyst_text);
                }
                // Entity not in window yet — it will appear on EntityDiscovered/EntityFailed
            }
            DisplayEvent::EntityDiscovered {
                entity_name,
                entity_type,
                catalyst_texts,
                completed,
                total,
            } => {
                *discovery_completed = completed;
                *discovery_total = total;
                if let Some(entry) = rolling_window.iter_mut().find(|e| e.entity_name == entity_name) {
                    if entry.catalysts.is_empty() {
                        entry.catalysts = catalyst_texts;
                    }
                    entry.status = EntryStatus::Completed;
                } else {
                    // Entity wasn't in window yet — add it now with final texts
                    rolling_window.push(DiscoveryEntry {
                        entity_name,
                        entity_type,
                        catalysts: catalyst_texts,
                        status: EntryStatus::Completed,
                    });
                    if rolling_window.len() > ROLLING_WINDOW_SIZE {
                        rolling_window.remove(0);
                    }
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
                let short_err = if error.len() > 80 {
                    format!("{}…", &error[..80])
                } else {
                    error
                };
                let fail_text = format!("(failed: {})", short_err);
                if let Some(entry) = rolling_window.iter_mut().find(|e| e.entity_name == entity_name) {
                    entry.catalysts = vec![fail_text];
                    entry.status = EntryStatus::Failed;
                } else {
                    rolling_window.push(DiscoveryEntry {
                        entity_name,
                        entity_type,
                        catalysts: vec![fail_text],
                        status: EntryStatus::Failed,
                    });
                    if rolling_window.len() > ROLLING_WINDOW_SIZE {
                        rolling_window.remove(0);
                    }
                }
            }
            DisplayEvent::DiscoveryComplete {
                entities_processed: _,
                catalysts_generated: count,
            } => {
                catalysts_generated.replace(count);
                // Stay in Discovery stage — entity tree keeps cycling
            }
            DisplayEvent::EmbeddingStart => {
                embedding_status.replace("Embedding...".to_string());
            }
            DisplayEvent::EmbeddingProgress { done, total } => {
                if total > 0 {
                    embedding_status.replace(format!("Embedding [{}/{} docs]", done, total));
                }
            }
            DisplayEvent::EmbeddingComplete { documents, pruned, duration_ms } => {
                // Flush the cycling entity display and print final summary lines
                clear_lines(out, *lines_written);
                *lines_written = 0;
                *stage = DisplayStage::Embedding;
                embedding_status.take();

                if let Some(count) = catalysts_generated.take() {
                    let _ = writeln!(
                        out,
                        "  {}Catalysts:{} {} generated",
                        crossterm::style::SetForegroundColor(colors::CATALYZING_ORANGE),
                        crossterm::style::ResetColor,
                        count,
                    );
                }

                let secs = duration_ms as f64 / 1000.0;
                let docs_per_sec = if secs > 0.0 { documents as f64 / secs } else { 0.0 };
                let pruned_suffix = if pruned > 0 {
                    format!(", {} pruned", pruned)
                } else {
                    String::new()
                };
                let _ = writeln!(
                    out,
                    "  {}Embedded:{} {} recent documents{} ({:.0} docs/sec)",
                    crossterm::style::SetForegroundColor(colors::FOREST_GREEN),
                    crossterm::style::ResetColor,
                    documents,
                    pruned_suffix,
                    docs_per_sec,
                );
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

                if let Some(count) = catalysts_generated.take() {
                    let _ = writeln!(
                        out,
                        "  {}Catalysts:{} {} generated",
                        crossterm::style::SetForegroundColor(colors::CATALYZING_ORANGE),
                        crossterm::style::ResetColor,
                        count,
                    );
                }

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
            DisplayEvent::StarterPrompt(prompt) => {
                let _ = writeln!(out);
                let _ = writeln!(
                    out,
                    "  {}Try this prompt:{}",
                    crossterm::style::SetForegroundColor(crossterm::style::Color::DarkGrey),
                    crossterm::style::ResetColor,
                );
                let full = format!("{} {}", prompt, enzyme_core::llm::obsidian_citation_suffix());
                let escaped = full
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
                    .replace('"', "\\\"")
                    .replace('!', "\\!");
                let _ = writeln!(
                    out,
                    "    {}claude \"{}\"{}\n",
                    crossterm::style::SetForegroundColor(colors::ENZYME_PURPLE),
                    escaped,
                    crossterm::style::ResetColor,
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
            DisplayEvent::Done { duration_ms } => {
                clear_lines(out, *lines_written);
                let secs = duration_ms as f64 / 1000.0;
                let _ = writeln!(
                    out,
                    "  {}Done!{} ({:.1}s)",
                    crossterm::style::SetForegroundColor(colors::FOREST_GREEN),
                    crossterm::style::ResetColor,
                    secs,
                );
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
                    event, &mut out, &mut stage, &mut rolling_window,
                    &mut discovery_completed, &mut discovery_total,
                    &mut lines_written, &mut spinner,
                    &mut catalysts_generated, &mut embedding_status,
                ) {
                    return;
                }
                // Drain all pending events before rendering
                while let Ok(event) = rx.try_recv() {
                    if handle_event(
                        event, &mut out, &mut stage, &mut rolling_window,
                        &mut discovery_completed, &mut discovery_total,
                        &mut lines_written, &mut spinner,
                        &mut catalysts_generated, &mut embedding_status,
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
            for (i, entry) in rolling_window.iter().enumerate() {
                let is_last = i == window_len - 1 && embedding_status.is_none();
                let display_text = if entry.catalysts.is_empty() {
                    None
                } else {
                    // Cycle through catalysts for completed/failed entities
                    Some(entry.catalysts[frame_counter % entry.catalysts.len()].as_str())
                };
                let _ = tree::render_entity_node(
                    &mut out,
                    &entry.entity_name,
                    &entry.entity_type,
                    display_text,
                    i,
                    window_len,
                    is_last,
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
                DisplayEvent::SelectionComplete { .. } => {
                }
                DisplayEvent::DiscoveryStart { total } => {
                    println!("Generating catalysts for {} entities...", total);
                }
                DisplayEvent::EntityStarted { .. } => {
                    // Not shown in plain mode
                }
                DisplayEvent::CatalystPartial { .. } => {
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
                    completed,
                    total,
                    ..
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
                    println!("  [catalyst] [{}/{}] {} (failed: {})", completed, total, entity_name, error);
                }
                DisplayEvent::DiscoveryComplete {
                    entities_processed: _,
                    catalysts_generated,
                } => {
                    println!(
                        "  Catalysts: {} generated",
                        catalysts_generated
                    );
                }
                DisplayEvent::EmbeddingStart => {
                    println!("Generating embeddings...");
                }
                DisplayEvent::EmbeddingProgress { done, total } => {
                    if total > 0 {
                        println!("  [embed] {}/{} documents", done, total);
                    }
                }
                DisplayEvent::EmbeddingComplete { documents, pruned, duration_ms } => {
                    let secs = duration_ms as f64 / 1000.0;
                    let docs_per_sec = if secs > 0.0 { documents as f64 / secs } else { 0.0 };
                    let pruned_suffix = if pruned > 0 {
                        format!(", {} pruned", pruned)
                    } else {
                        String::new()
                    };
                    println!("  Embedded: {} recent documents{} ({:.0} docs/sec)", documents, pruned_suffix, docs_per_sec);
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
                DisplayEvent::StarterPrompt(prompt) => {
                    let full = format!("{} {}", prompt, enzyme_core::llm::obsidian_citation_suffix());
                    let escaped = full
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" ")
                        .replace('"', "\\\"")
                        .replace('!', "\\!");
                    println!();
                    println!("  Try this prompt:");
                    println!("    claude \"{}\"", escaped);
                    println!();
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
                DisplayEvent::Done { duration_ms } => {
                    let secs = duration_ms as f64 / 1000.0;
                    println!("Done! ({:.1}s)", secs);
                    return;
                }
            },
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
