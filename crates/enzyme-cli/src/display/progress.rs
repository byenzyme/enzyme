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

#[derive(Debug, Clone)]
#[allow(dead_code)]
struct DiscoveryEntry {
    entity_name: String,
    entity_type: EntityType,
    catalyst_text: Option<String>,
    failed: bool,
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

    // Keep indexing messages for display
    let mut _indexing_messages: Vec<String> = Vec::new();

    loop {
        // Non-blocking receive with timeout for animation
        match rx.recv_timeout(FRAME_INTERVAL) {
            Ok(event) => {
                match event {
                    DisplayEvent::IndexingStart => {
                        stage = DisplayStage::Indexing;
                    }
                    DisplayEvent::IndexingProgress(msg) => {
                        _indexing_messages.push(msg);
                    }
                    DisplayEvent::IndexingComplete {
                        files_discovered,
                        files_new,
                        files_updated,
                        files_deleted,
                    } => {
                        // Clear animated lines and print final indexing summary
                        clear_lines(&mut out, lines_written);
                        lines_written = 0;

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
                    DisplayEvent::SelectionComplete { count, source } => {
                        stage = DisplayStage::Selecting;
                        let source_label = match source {
                            SelectionSource::Config => "config",
                            SelectionSource::Trending => "trending",
                        };
                        let _ = writeln!(
                            out,
                            "  {}Selected:{} {} entities ({})",
                            crossterm::style::SetForegroundColor(colors::ENZYME_PURPLE),
                            crossterm::style::ResetColor,
                            count,
                            source_label,
                        );
                    }
                    DisplayEvent::DiscoveryStart { total } => {
                        stage = DisplayStage::Discovery;
                        discovery_total = total;
                        discovery_completed = 0;
                        rolling_window.clear();
                    }
                    DisplayEvent::EntityDiscovered {
                        entity_name,
                        entity_type,
                        catalyst_texts,
                        completed,
                        total,
                    } => {
                        discovery_completed = completed;
                        discovery_total = total;
                        // Take first catalyst text as preview
                        let preview = catalyst_texts.first().cloned();
                        rolling_window.push(DiscoveryEntry {
                            entity_name,
                            entity_type,
                            catalyst_text: preview,
                            failed: false,
                        });
                        if rolling_window.len() > ROLLING_WINDOW_SIZE {
                            rolling_window.remove(0);
                        }
                    }
                    DisplayEvent::EntitySkipped {
                        entity_name,
                        completed,
                        total,
                    } => {
                        discovery_completed = completed;
                        discovery_total = total;
                        rolling_window.push(DiscoveryEntry {
                            entity_name,
                            entity_type: EntityType::Tag,
                            catalyst_text: Some("(skipped)".to_string()),
                            failed: false,
                        });
                        if rolling_window.len() > ROLLING_WINDOW_SIZE {
                            rolling_window.remove(0);
                        }
                    }
                    DisplayEvent::EntityFailed {
                        entity_name,
                        error,
                        completed,
                        total,
                    } => {
                        discovery_completed = completed;
                        discovery_total = total;
                        // Truncate error for display
                        let short_err = if error.len() > 80 {
                            format!("{}…", &error[..80])
                        } else {
                            error
                        };
                        rolling_window.push(DiscoveryEntry {
                            entity_name,
                            entity_type: EntityType::Tag,
                            catalyst_text: Some(format!("(failed: {})", short_err)),
                            failed: true,
                        });
                        if rolling_window.len() > ROLLING_WINDOW_SIZE {
                            rolling_window.remove(0);
                        }
                    }
                    DisplayEvent::DiscoveryComplete {
                        entities_processed: _,
                        catalysts_generated,
                    } => {
                        clear_lines(&mut out, lines_written);
                        lines_written = 0;
                        stage = DisplayStage::Embedding;

                        let _ = writeln!(
                            out,
                            "  {}Catalysts:{} {} generated",
                            crossterm::style::SetForegroundColor(colors::CATALYZING_ORANGE),
                            crossterm::style::ResetColor,
                            catalysts_generated,
                        );
                    }
                    DisplayEvent::EmbeddingStart => {
                        stage = DisplayStage::Embedding;
                    }
                    DisplayEvent::EmbeddingProgress { done, total } => {
                        if total > 0 {
                            clear_lines(&mut out, lines_written);
                            lines_written = 0;
                            let frame = spinner.tick();
                            let _ = writeln!(
                                out,
                                "  {}{}{} Embedding [{}/{} docs]",
                                crossterm::style::SetForegroundColor(colors::ENZYME_PURPLE),
                                frame,
                                crossterm::style::ResetColor,
                                done,
                                total,
                            );
                            lines_written = 1;
                        }
                    }
                    DisplayEvent::EmbeddingComplete { documents, pruned, duration_ms } => {
                        clear_lines(&mut out, lines_written);
                        lines_written = 0;
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
                        let _ = writeln!(
                            out,
                            "  {}Embedding skipped:{} {}",
                            crossterm::style::SetForegroundColor(crossterm::style::Color::DarkGrey),
                            crossterm::style::ResetColor,
                            reason,
                        );
                    }
                    DisplayEvent::EmbeddingFailed(err) => {
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
                        // Build full prompt with citation suffix, escape for shell
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
                        stage = DisplayStage::Similarity;
                    }
                    DisplayEvent::SimilarityProgress { done, total } => {
                        if total > 0 && done % 10 == 0 {
                            clear_lines(&mut out, lines_written);
                            lines_written = 0;
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
                            lines_written = 1;
                        }
                    }
                    DisplayEvent::SimilarityComplete {
                        catalysts,
                        documents,
                    } => {
                        clear_lines(&mut out, lines_written);
                        lines_written = 0;
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
                        clear_lines(&mut out, lines_written);
                        let secs = duration_ms as f64 / 1000.0;
                        let _ = writeln!(
                            out,
                            "  {}Done!{} ({:.1}s)",
                            crossterm::style::SetForegroundColor(colors::FOREST_GREEN),
                            crossterm::style::ResetColor,
                            secs,
                        );
                        let _ = out.flush();
                        return;
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Render animation frame
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                // Channel closed — cleanup and exit
                clear_lines(&mut out, lines_written);
                let _ = out.flush();
                return;
            }
        }

        // Render current state for discovery stage
        if stage == DisplayStage::Discovery && !rolling_window.is_empty() {
            clear_lines(&mut out, lines_written);
            lines_written = 0;

            // Header line with spinner
            let frame = spinner.tick();
            let _ = tree::render_discovery_header(
                &mut out,
                discovery_completed,
                discovery_total,
                frame,
            );
            lines_written += 1;

            // Rolling window entries with heatmap colors
            let window_len = rolling_window.len();
            for (i, entry) in rolling_window.iter().enumerate() {
                let is_last = i == window_len - 1;
                let _ = tree::render_entity_node(
                    &mut out,
                    &entry.entity_name,
                    &entry.entity_type,
                    entry.catalyst_text.as_deref(),
                    i,
                    window_len,
                    is_last,
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
                DisplayEvent::SelectionComplete { count, source } => {
                    let source_label = match source {
                        SelectionSource::Config => "config",
                        SelectionSource::Trending => "trending",
                    };
                    println!("  Selected: {} entities ({})", count, source_label);
                }
                DisplayEvent::DiscoveryStart { total } => {
                    println!("Generating catalysts for {} entities...", total);
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
                    entity_name,
                    completed,
                    total,
                } => {
                    println!("  [catalyst] [{}/{}] {} (skipped)", completed, total, entity_name);
                }
                DisplayEvent::EntityFailed {
                    entity_name,
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
