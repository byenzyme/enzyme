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

    /// Collection ID (resolves to ~/.enzyme-sdk/collections/<id>/.enzyme/enzyme.db)
    #[arg(long, global = true)]
    collection: Option<String>,

    /// Verbosity level
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    verbose: u8,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Reset and rebuild the vault index
    Init {
        /// Internal background worker mode
        #[arg(long, hide = true)]
        detached: bool,

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

    /// Incrementally refresh the vault index
    Refresh {
        /// Internal background worker mode
        #[arg(long, hide = true)]
        detached: bool,

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

        /// Target directory to prepare and search instead of the vault itself
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

    /// Show vault status and configuration
    Status,

    /// Sign in to enzyme.garden
    Login {
        /// Do not try to open a browser automatically
        #[arg(long)]
        no_open: bool,

        /// Emit JSONL events for agent/app mediated login
        #[arg(long)]
        json: bool,
    },

    /// Sign out of enzyme.garden
    Logout,

    /// Publish vault index to enzyme.garden for hosted access
    Publish {
        /// Emit publish result as JSON on stdout
        #[arg(long)]
        json: bool,
    },

    /// Start MCP (Model Context Protocol) server over stdio
    Mcp,

    /// Scan vault structure and suggest config without initializing
    Scan {
        /// Write suggested entities to ~/.enzyme/config.toml if this vault has no entities yet
        #[arg(long)]
        write_config: bool,
    },

    /// Install or update agent integrations for this vault
    Agents {
        #[command(subcommand)]
        command: AgentsCommand,
    },

    /// Start the HTTP API server for the explore UI
    Serve {
        /// Port to listen on
        #[arg(long, default_value = "8765")]
        port: u16,
    },

    /// Ingest structured data directly into the enzyme DB
    Ingest {
        /// Suppress human output, emit JSON
        #[arg(short, long)]
        quiet: bool,
    },

    /// Embed structured entries without creating an enzyme database
    EmbedEntries {
        /// JSON file containing an array, {"entries": [...]}, or {"entry": ...}; reads stdin if omitted
        #[arg(short, long)]
        input: Option<PathBuf>,
    },

    /// Benchmark retrieval quality across embedding backends and search modes
    Benchmark {
        /// Path to queries file (JSONL or plain text, one query per line)
        queries: PathBuf,

        /// Maximum results per query
        #[arg(short = 'n', long, default_value = "10")]
        limit: usize,

        /// Conditions to run (default: all). Options: ese_direct, ese_catalysts, arctic_direct, arctic_catalysts
        #[arg(long, value_delimiter = ',')]
        conditions: Vec<String>,
    },

    /// Dump catalyst prompts to JSONL for training data generation
    DumpPrompts {
        /// Output file path (JSONL)
        #[arg(short, long, default_value = "prompts.jsonl")]
        output: PathBuf,

        /// Minimum catalysts for the largest entity (by volume)
        #[arg(long)]
        min_top_catalysts: Option<usize>,
    },
}

#[derive(Subcommand)]
enum AgentsCommand {
    /// Install or update the Codex skill in .agents/skills/enzyme
    Codex {
        /// Template source: auto fetches GitHub copy and falls back to packaged copy
        #[arg(long, value_enum, default_value = "auto")]
        source: commands::agents::InstallSource,
    },

    /// Install or update the Claude skill in .claude/skills/enzyme
    Claude {
        /// Template source: auto fetches GitHub copy and falls back to packaged copy
        #[arg(long, value_enum, default_value = "auto")]
        source: commands::agents::InstallSource,
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
    match &cli.command {
        Commands::Mcp => {
            init_tracing(cli.verbose, None);
            return commands::mcp::run_mcp(cli.vault).await;
        }
        Commands::Login { no_open, json } => {
            init_tracing(cli.verbose, None);
            return commands::login::run_login(*no_open, *json).await;
        }
        Commands::Logout => {
            init_tracing(cli.verbose, None);
            return commands::login::run_logout().await;
        }
        _ => {}
    }

    let vault_path = if let Some(ref collection_id) = cli.collection {
        let db_path = enzyme_core::config::paths::EnzymePaths::collection_db_path(collection_id)?;
        let vault = db_path
            .parent()
            .and_then(|p| p.parent())
            .ok_or_else(|| anyhow::anyhow!("invalid collection DB path"))?
            .to_path_buf();
        std::fs::create_dir_all(&vault)?;
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        vault
    } else {
        resolve_vault_path(cli.vault)?
    };
    init_tracing(cli.verbose, Some(&vault_path));
    info!(?vault_path, "resolved vault path");

    match cli.command {
        Commands::Init {
            detached,
            quiet,
            json_progress,
            min_top_catalysts,
            stage,
        } => {
            if stage.as_deref() == Some("index") {
                commands::init::run_init_index_only(&vault_path, quiet).await?;
            } else {
                let init_result = commands::init::run_init(
                    &vault_path,
                    detached,
                    quiet,
                    json_progress,
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
            detached,
            quiet,
            min_top_catalysts,
        } => {
            commands::init::run_refresh(&vault_path, detached, quiet, min_top_catalysts).await?;
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
        Commands::Status => {
            commands::status::run_status(&vault_path).await?;
        }
        Commands::Scan { write_config } => {
            commands::scan::run_scan(&vault_path, write_config)?;
        }
        Commands::Agents { command } => match command {
            AgentsCommand::Codex { source } => {
                commands::agents::run_agents_install(
                    &vault_path,
                    commands::agents::AgentTarget::Codex,
                    source,
                )
                .await?;
            }
            AgentsCommand::Claude { source } => {
                commands::agents::run_agents_install(
                    &vault_path,
                    commands::agents::AgentTarget::Claude,
                    source,
                )
                .await?;
            }
        },
        Commands::Serve { port } => {
            commands::serve::run_serve(&vault_path, port).await?;
        }
        Commands::Ingest { quiet } => {
            commands::ingest::run_ingest(&vault_path, quiet).await?;
        }
        Commands::EmbedEntries { input } => {
            commands::embed_entries::run_embed_entries(input.as_deref())?;
        }
        Commands::Benchmark {
            queries,
            limit,
            conditions,
        } => {
            commands::benchmark::run_benchmark(&vault_path, &queries, limit, &conditions).await?;
        }
        Commands::DumpPrompts {
            output,
            min_top_catalysts,
        } => {
            commands::dump_prompts::run_dump_prompts(&vault_path, &output, min_top_catalysts)
                .await?;
        }
        Commands::Publish { json } => {
            commands::publish::run_publish(&vault_path, json).await?;
        }
        Commands::Login { .. } | Commands::Logout => unreachable!(),
        Commands::Mcp => unreachable!(),
    }

    Ok(())
}
