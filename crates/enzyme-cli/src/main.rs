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

    /// Named workspace/collection (home-rooted; configured under [workspaces.<name>])
    #[arg(long, global = true, conflicts_with = "vault")]
    collection: Option<String>,

    /// Verbosity level
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    verbose: u8,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Compile, migrate, or preview readable .enzyme configuration
    Spec {
        #[command(subcommand)]
        command: commands::spec::SpecCommand,
    },
    /// Build or update the vault index and catalysts
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

        /// Return once a seed petri is available while a detached worker finishes init.
        #[arg(long)]
        voice_ready: bool,

        /// Number of catalyzed petri entities required before --voice-ready returns.
        #[arg(long, default_value_t = 1)]
        voice_entities: usize,

        /// Minimum catalysts each voice-ready entity must expose.
        #[arg(long, default_value_t = 1)]
        voice_min_catalysts: usize,

        /// Seconds to wait for seed petri readiness before failing --voice-ready.
        #[arg(long, default_value_t = 90)]
        voice_timeout_secs: u64,

        /// Internal worker mode: stream and persist complete catalyst records as they arrive.
        #[arg(long, hide = true)]
        stream_partial_catalysts: bool,

        /// Use OPENAI_*/OPENROUTER_* env LLM credentials instead of Enzyme hosted credits.
        #[arg(long)]
        use_env_llm: bool,

        /// Stop after a specific pipeline stage (e.g. "index")
        #[arg(long)]
        stage: Option<String>,

        /// Discard the existing index and regenerate every entity's catalysts
        #[arg(long, visible_alias = "regenerate")]
        force: bool,
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

        /// Use OPENAI_*/OPENROUTER_* env LLM credentials instead of Enzyme hosted credits.
        #[arg(long)]
        use_env_llm: bool,

        /// Force a complete catalyst rebase epoch now (still built detached).
        #[arg(long, visible_alias = "force")]
        full: bool,
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

        /// Deprecated no-op: presentation is controlled by installed agent instructions
        #[arg(long, default_value = "explore", hide = true)]
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

    /// Check for and install Enzyme binary updates
    Update {
        /// Check only and emit a JSON result
        #[arg(long, conflicts_with = "background")]
        check: bool,

        /// Internal silent background worker mode
        #[arg(long, hide = true)]
        background: bool,
    },

    /// Sign in to memory.enzyme.garden
    Login {
        /// Do not try to open a browser automatically
        #[arg(long)]
        no_open: bool,

        /// Emit JSONL events for agent/app mediated login
        #[arg(long)]
        json: bool,
    },

    /// Sign out of memory.enzyme.garden
    Logout,

    /// Start MCP (Model Context Protocol) server over stdio
    Mcp,

    /// Manage local GGUF models for offline catalyst generation
    Model {
        #[command(subcommand)]
        command: ModelCommands,
    },

    /// Scan vault structure and suggest config without initializing
    Scan {
        /// Write suggested entities to ~/.enzyme/config.toml if this vault has no entities yet
        #[arg(long)]
        write_config: bool,
    },

    /// Install or update this workspace's agent integration
    Install {
        /// Agent runtime to configure
        #[arg(value_enum, default_value = "hermes")]
        target: commands::agents::AgentTarget,

        /// Template source: auto fetches GitHub copy and falls back to packaged copy
        #[arg(long, value_enum, default_value = "auto")]
        source: commands::agents::InstallSource,
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

    /// Report vault diagnostic facts (read-only; never writes)
    Doctor {
        /// Emit structured JSON to stdout
        #[arg(long)]
        json: bool,
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
enum ModelCommands {
    /// Download a model into ~/.enzyme/models (default: catalyst-135m-v3)
    Install {
        /// Registry name, or the local name to give a `--url` download
        name: Option<String>,

        /// Download this GGUF directly instead of a registry entry
        #[arg(long)]
        url: Option<String>,

        /// Expected SHA-256 for `--url` downloads
        #[arg(long)]
        sha256: Option<String>,
    },

    /// List installed and available models
    List,

    /// Select an installed model and require local catalyst generation
    Use {
        /// Installed model name
        name: String,
    },

    /// Return to automatic selection (local when installed, otherwise hosted)
    Auto,

    /// Disable local catalyst generation and use Enzyme hosted
    Disable,

    /// Show the configured and effective catalyst-generation mode
    Status,

    /// Verify an installed model's file and registry checksum
    Verify {
        /// Model name (defaults to the selected model)
        name: Option<String>,
    },

    /// Remove an installed model
    Rm {
        /// Installed model name
        name: String,
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

    // Named workspace logs live beside their DB; legacy vault logs retain the
    // historic `.enzyme/enzyme.log` location.
    if let Some(vault) = vault_path {
        let enzyme_dir = match enzyme_core::config::paths::EnzymePaths::bound_workspace(vault) {
            Ok(Some(_)) => vault.to_path_buf(),
            _ => vault.join(".enzyme"),
        };
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
        Commands::Spec { command } => {
            return commands::spec::run(command).await;
        }
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
        Commands::Model { command } => {
            init_tracing(cli.verbose, None);
            return match command {
                ModelCommands::Install { name, url, sha256 } => {
                    commands::model::run_install(name.clone(), url.clone(), sha256.clone()).await
                }
                ModelCommands::List => commands::model::run_list(),
                ModelCommands::Use { name } => commands::model::run_use(name),
                ModelCommands::Auto => commands::model::run_auto(),
                ModelCommands::Disable => commands::model::run_disable(),
                ModelCommands::Status => commands::model::run_status(),
                ModelCommands::Verify { name } => commands::model::run_verify(name.as_deref()),
                ModelCommands::Rm { name } => commands::model::run_rm(name),
            };
        }
        _ => {}
    }

    let vault_path = if let Some(ref collection_id) = cli.collection {
        enzyme_core::config::paths::EnzymePaths::workspace_root(collection_id)?
    } else {
        resolve_vault_path(cli.vault)?
    };
    init_tracing(cli.verbose, Some(&vault_path));
    info!(?vault_path, "resolved vault path");

    // Validate readable configuration before any init/refresh stage can mutate the vault.
    if matches!(
        &cli.command,
        Commands::Init { .. } | Commands::Refresh { .. }
    ) {
        enzyme_core::config::EnzymeConfig::load(&vault_path)?;
    }

    match cli.command {
        Commands::Init {
            detached,
            quiet,
            json_progress,
            min_top_catalysts,
            voice_ready,
            voice_entities,
            voice_min_catalysts,
            voice_timeout_secs,
            stream_partial_catalysts,
            use_env_llm,
            stage,
            force,
        } => {
            if stage.as_deref() == Some("index") {
                commands::init::run_init_index_only(&vault_path, quiet).await?;
            } else if voice_ready {
                let output = commands::init::run_init_voice_ready(
                    &vault_path,
                    voice_entities,
                    voice_min_catalysts,
                    std::time::Duration::from_secs(voice_timeout_secs),
                    min_top_catalysts,
                    use_env_llm,
                )
                .await?;

                if quiet || json_progress {
                    println!("{}", serde_json::to_string(&output)?);
                } else {
                    println!("{}", serde_json::to_string_pretty(&output)?);
                }
            } else {
                let init_result = commands::init::run_init(
                    &vault_path,
                    detached,
                    quiet,
                    json_progress,
                    min_top_catalysts,
                    stream_partial_catalysts,
                    use_env_llm,
                    force,
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
            use_env_llm,
            full,
        } => {
            commands::init::run_refresh(
                &vault_path,
                detached,
                quiet,
                min_top_catalysts,
                use_env_llm,
                full,
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
        Commands::Status => {
            commands::status::run_status(&vault_path).await?;
        }
        Commands::Update { check, background } => {
            commands::update::run_update(&vault_path, check, background).await?;
        }
        Commands::Scan { write_config } => {
            commands::scan::run_scan(&vault_path, write_config)?;
        }
        Commands::Install { target, source } => {
            commands::agents::run_agents_install(&vault_path, target, source).await?;
        }
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
        Commands::Doctor { json } => {
            commands::doctor::run_doctor(&vault_path, json).await?;
        }
        Commands::Login { .. } | Commands::Logout => unreachable!(),
        Commands::Mcp | Commands::Model { .. } | Commands::Spec { .. } => unreachable!(),
    }

    Ok(())
}
