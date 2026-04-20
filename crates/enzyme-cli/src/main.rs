use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing::info;
use tracing_subscriber::EnvFilter;

mod commands;
mod display;

#[derive(Parser)]
#[command(
    name = "enzyme",
    about = "Local-first knowledge indexing and approximate search"
)]
#[command(version)]
struct Cli {
    /// Path to the vault (defaults to current directory)
    #[arg(short = 'p', long, global = true)]
    vault: Option<PathBuf>,

    /// Verbosity level
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    verbose: u8,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Initialize or refresh the vault index
    Init {
        /// Force full re-index
        #[arg(long)]
        full: bool,

        /// Run in background
        #[arg(long)]
        detached: bool,

        /// Vault guide content (or pass via stdin)
        #[arg(long)]
        guide: Option<String>,

        /// Suppress all output (for agent/programmatic use)
        #[arg(short, long)]
        quiet: bool,

        /// Emit JSONL progress events to stderr (for GUI integration)
        #[arg(long)]
        json_progress: bool,

        /// Minimum catalysts for the largest entity (by volume).
        /// Raises the ceiling and boosts the top entity to at least this many.
        #[arg(long)]
        min_top_catalysts: Option<usize>,

        /// Stop after a specific pipeline stage (e.g. "index")
        #[arg(long)]
        stage: Option<String>,
    },

    /// Refresh the vault index (alias for init)
    Refresh {
        #[arg(long)]
        full: bool,

        #[arg(long)]
        detached: bool,

        /// Vault guide content (or pass via stdin)
        #[arg(long)]
        guide: Option<String>,

        /// Suppress all output (for agent/programmatic use)
        #[arg(short, long)]
        quiet: bool,

        /// Minimum catalysts for the largest entity (by volume).
        #[arg(long)]
        min_top_catalysts: Option<usize>,
    },

    /// Search using catalyst-based approximate matching
    Catalyze {
        /// Search query
        query: String,

        /// Maximum number of results
        #[arg(short = 'n', long, default_value = "10")]
        limit: usize,

        /// Target directory (from `enzyme apply`) to search instead of the vault itself
        #[arg(long)]
        target: Option<PathBuf>,

        /// Presentation register: explore (default), continuity, or reference
        #[arg(long, default_value = "explore")]
        register: String,
    },

    /// Show the current petri (top catalysts and entities)
    Petri {
        /// Number of top entities to show (default: all, or 15 with --query)
        #[arg(short = 'n', long)]
        top: Option<usize>,

        /// Query to rank entities and catalysts by relevance
        #[arg(long)]
        query: Option<String>,

        /// Maximum catalysts per entity in query mode (default: 3)
        #[arg(long)]
        catalyst_budget: Option<usize>,
    },

    /// Apply vault catalysts to an external directory
    Apply {
        /// Target directory to apply catalysts to
        target: PathBuf,

        /// Source vault (defaults to current vault)
        #[arg(long)]
        source: Option<PathBuf>,

        /// Run in background
        #[arg(long)]
        detached: bool,
    },

    /// Show vault status and configuration
    Status,

    /// Download the embedding model to ~/.enzyme/models/
    Setup {
        /// Check if model is present (exit 0) or missing (exit 1)
        #[arg(long)]
        check: bool,
    },

    /// Start MCP (Model Context Protocol) server over stdio
    Mcp,

    /// Quick vault scan — extract entities without initializing
    Scan {
        /// Output a one-line guide string instead of JSON
        #[arg(long)]
        guide: bool,
    },

    /// Start the HTTP API server for the explore UI
    Serve {
        /// Port to listen on
        #[arg(long, default_value = "8765")]
        port: u16,
    },

    /// Dump catalyst prompts to JSONL for training data generation
    DumpPrompts {
        /// Output file path (JSONL)
        #[arg(short, long, default_value = "prompts.jsonl")]
        output: PathBuf,

        /// Vault guide content
        #[arg(long)]
        guide: Option<String>,

        /// Minimum catalysts for the largest entity (by volume)
        #[arg(long)]
        min_top_catalysts: Option<usize>,
    },

}

fn init_tracing(verbose: u8, vault_path: Option<&Path>) {
    let filter = match verbose {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };

    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(filter));

    // Try to log to .enzyme/enzyme.log when a vault path is available.
    // Only use the file logger if .enzyme/ already exists (don't create it).
    if let Some(vault) = vault_path {
        let enzyme_dir = vault.join(".enzyme");
        if enzyme_dir.is_dir() {
            if let Ok(file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(enzyme_dir.join("enzyme.log"))
            {
                tracing_subscriber::fmt()
                    .with_env_filter(env_filter)
                    .with_writer(file)
                    .with_ansi(false)
                    .init();
                return;
            }
        }
    }

    // Fallback: stderr (non-vault commands, or if file creation failed).
    tracing_subscriber::fmt().with_env_filter(env_filter).init();
}

/// Read vault guide from --guide flag or stdin (if piped).
fn resolve_vault_guide(flag: Option<String>) -> Option<String> {
    if let Some(guide) = flag {
        if guide.is_empty() { None } else { Some(guide) }
    } else if !atty::is(atty::Stream::Stdin) {
        // stdin is piped — read it
        let mut buf = String::new();
        if std::io::stdin().read_to_string(&mut buf).is_ok() && !buf.trim().is_empty() {
            Some(buf)
        } else {
            None
        }
    } else {
        None
    }
}

fn resolve_vault_path(vault: Option<PathBuf>) -> Result<PathBuf> {
    match vault {
        Some(p) => Ok(p),
        None => {
            if let Ok(root) = std::env::var("ENZYME_VAULT_ROOT") {
                if !root.is_empty() {
                    return Ok(PathBuf::from(root));
                }
            }
            std::env::current_dir()
                .map_err(|e| anyhow::anyhow!("failed to get current directory: {e}"))
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // These commands don't need a vault path — log to stderr.
    if let Commands::Setup { check } = cli.command {
        init_tracing(cli.verbose, None);
        return commands::setup::run_setup(check).await;
    }
    if matches!(cli.command, Commands::Mcp) {
        init_tracing(cli.verbose, None);
        return commands::mcp::run_mcp(cli.vault).await;
    }

    let vault_path = resolve_vault_path(cli.vault)?;
    init_tracing(cli.verbose, Some(&vault_path));
    info!(?vault_path, "resolved vault path");

    match cli.command {
        Commands::Init {
            full,
            detached,
            guide,
            quiet,
            json_progress,
            min_top_catalysts,
            stage,
        } => {
            if stage.as_deref() == Some("index") {
                commands::init::run_init_index_only(&vault_path, full, quiet).await?;
            } else {
                let vault_guide = resolve_vault_guide(guide);
                let init_result = commands::init::run_init(
                    &vault_path,
                    full,
                    detached,
                    quiet,
                    vault_guide.as_deref(),
                    min_top_catalysts,
                )
                .await?;

                if quiet || json_progress {
                    if let Some(result) = init_result {
                        let output = result.to_json(&vault_path)?;
                        println!("{}", serde_json::to_string(&output)?);
                    }
                }
            }
        }
        Commands::Refresh {
            full,
            detached,
            guide,
            quiet,
            min_top_catalysts,
        } => {
            let vault_guide = resolve_vault_guide(guide);
            commands::init::run_refresh(
                &vault_path,
                full,
                detached,
                quiet,
                vault_guide.as_deref(),
                min_top_catalysts,
            )
            .await?;
        }
        Commands::Catalyze {
            query,
            limit,
            target,
            register,
        } => {
            commands::catalyze::run_catalyze(
                &vault_path,
                &query,
                limit,
                target.as_deref(),
                &register,
            )
            .await?;
        }
        Commands::Petri {
            top,
            query,
            catalyst_budget,
        } => {
            let default_top = if query.is_some() { 15 } else { usize::MAX };
            commands::petri::run_petri(
                &vault_path,
                top.unwrap_or(default_top),
                query.as_deref(),
                catalyst_budget,
            )
            .await?;
        }
        Commands::Apply {
            target,
            source,
            detached,
        } => {
            let source_path = source.as_deref().unwrap_or(&vault_path);
            commands::apply::run_apply(source_path, &target, false).await?;
        }
        Commands::Status => {
            commands::status::run_status(&vault_path).await?;
        }
        Commands::Scan { guide } => {
            commands::scan::run_scan(&vault_path, guide)?;
        }
        Commands::Serve { port } => {
            commands::serve::run_serve(&vault_path, port).await?;
        }
        Commands::DumpPrompts {
            output,
            guide,
            min_top_catalysts,
        } => {
            commands::dump_prompts::run_dump_prompts(
                &vault_path,
                &output,
                guide,
                min_top_catalysts,
            )
            .await?;
        }
        Commands::Setup { .. } | Commands::Mcp => unreachable!(),
    }

    Ok(())
}
