use std::io::Read;
use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing::info;
use tracing_subscriber::EnvFilter;

mod commands;
mod display;

#[derive(Parser)]
#[command(name = "enzyme", about = "Local-first knowledge indexing and approximate search")]
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
    },

    /// Search using catalyst-based approximate matching
    Catalyze {
        /// Search query
        query: String,

        /// Maximum number of results
        #[arg(short = 'n', long, default_value = "10")]
        limit: usize,
    },

    /// Show the current petri (top catalysts and entities)
    Petri {
        /// Number of top entities to show
        #[arg(short = 'n', long, default_value = "10")]
        top: usize,
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

    /// Internal: run the embedding daemon (auto-started by CLI, not user-facing)
    #[command(name = "__daemon", hide = true)]
    Daemon,
}

fn init_tracing(verbose: u8) {
    let filter = match verbose {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(filter)),
        )
        .init();
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
        None => std::env::current_dir().map_err(|e| anyhow::anyhow!("failed to get current directory: {e}")),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    // These commands don't need a vault path.
    if matches!(cli.command, Commands::Daemon) {
        return commands::daemon::run_daemon().await;
    }
    if let Commands::Setup { check } = cli.command {
        return commands::setup::run_setup(check).await;
    }
    if matches!(cli.command, Commands::Mcp) {
        return commands::mcp::run_mcp(cli.vault).await;
    }

    let vault_path = resolve_vault_path(cli.vault)?;
    info!(?vault_path, "resolved vault path");

    match cli.command {
        Commands::Init { full, detached, guide, quiet } => {
            let vault_guide = resolve_vault_guide(guide);
            let init_result = commands::init::run_init(&vault_path, full, detached, quiet, vault_guide.as_deref()).await?;

            if quiet {
                if let Some(result) = init_result {
                    let output = result.to_json(&vault_path)?;
                    println!("{}", serde_json::to_string(&output)?);
                }
            }
        }
        Commands::Refresh { full, detached, guide, quiet } => {
            let vault_guide = resolve_vault_guide(guide);
            commands::init::run_refresh(&vault_path, full, detached, quiet, vault_guide.as_deref()).await?;
        }
        Commands::Catalyze { query, limit } => {
            commands::catalyze::run_catalyze(&vault_path, &query, limit).await?;
        }
        Commands::Petri { top } => {
            commands::petri::run_petri(&vault_path, top).await?;
        }
        Commands::Apply {
            target,
            source,
            detached,
        } => {
            let source_path = source.as_deref().unwrap_or(&vault_path);
            commands::apply::run_apply(source_path, &target, detached).await?;
        }
        Commands::Status => {
            commands::status::run_status(&vault_path).await?;
        }
        Commands::Setup { .. } | Commands::Daemon | Commands::Mcp => unreachable!(),
    }

    Ok(())
}
