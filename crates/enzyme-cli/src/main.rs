use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing::info;
use tracing_subscriber::EnvFilter;

mod commands;

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
    },

    /// Refresh the vault index (alias for init)
    Refresh {
        #[arg(long)]
        full: bool,

        #[arg(long)]
        detached: bool,
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

    /// Search vault content
    Context {
        /// Search query
        query: String,

        /// Maximum number of results
        #[arg(short = 'n', long, default_value = "5")]
        limit: usize,
    },

    /// Show vault status and configuration
    Status,
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

    let vault_path = resolve_vault_path(cli.vault)?;
    info!(?vault_path, "resolved vault path");

    match cli.command {
        Commands::Init { full, detached } => {
            commands::init::run_init(&vault_path, full, detached).await?;
        }
        Commands::Refresh { full, detached } => {
            // Refresh is an alias for init
            commands::init::run_init(&vault_path, full, detached).await?;
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
        Commands::Context { query, limit } => {
            commands::context::run_context(&vault_path, &query, limit).await?;
        }
        Commands::Status => {
            commands::status::run_status(&vault_path).await?;
        }
    }

    Ok(())
}
