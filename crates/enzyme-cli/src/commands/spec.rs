use anyhow::{Context, Result};
use clap::Subcommand;
use enzyme_core::config::{EnzymeConfig, EnzymePaths};
use std::{net::SocketAddr, path::PathBuf};

#[derive(Debug, Subcommand)]
pub enum SpecCommand {
    /// Parse and resolve a source file (no vault access or model calls)
    Compile {
        file: PathBuf,
        #[arg(long)]
        instructions: bool,
        #[arg(long)]
        normalized: bool,
    },
    /// Export full built-in profile definitions
    Profiles {
        /// Emit the versioned `enzyme.profiles.v1` JSON envelope
        #[arg(long)]
        json: bool,
    },
    /// Migrate config.toml to configs/*.enzyme, retaining a timestamped backup
    Migrate {
        #[arg(long)]
        home: Option<PathBuf>,
        #[arg(long)]
        embed_profiles_in: Option<PathBuf>,
    },
    /// Print the effective runtime config, or its compiled agent policy
    Inspect {
        vault: PathBuf,
        #[arg(long)]
        instructions: bool,
    },
    /// Preview what a program would do on an indexed vault (read-only; no model calls)
    ///
    /// Shows, per reading, the resolved entities, generation jobs, evidence
    /// periods with excerpt and question allocation, and why readings are empty.
    /// Pass the vault with the global `-p/--vault`; without it every vault the
    /// file declares is planned.
    Plan {
        file: PathBuf,
        /// Emit the plan as structured JSON
        #[arg(long)]
        json: bool,
        /// Also write each job's rendered system and user prompt into this directory
        #[arg(long)]
        prompts: Option<PathBuf>,
        /// Build local-model prompts instead of hosted ones
        #[arg(long)]
        local: bool,
    },
    /// Serve the syntax playground backed by the Rust runtime parser
    Serve {
        #[arg(long, default_value = "127.0.0.1")]
        bind: String,
        #[arg(long, default_value_t = 4178)]
        port: u16,
    },
}
pub async fn run(command: &SpecCommand, vault: Option<&std::path::Path>) -> Result<()> {
    match command {
        SpecCommand::Compile {
            file,
            instructions,
            normalized,
        } => {
            let source = std::fs::read_to_string(file)?;
            let ast =
                enzyme_spec::parse(&source).with_context(|| format!("parse {}", file.display()))?;
            let home = std::env::var_os("HOME")
                .map(PathBuf::from)
                .context("HOME missing")?;
            let plan = enzyme_spec::resolve(vec![ast.clone()], &home)?;
            if *instructions {
                for v in &plan.vaults {
                    println!("{}", enzyme_spec::instructions(v));
                }
            } else if *normalized {
                print!("{}", enzyme_spec::render_program(&ast));
            } else {
                println!("{}", serde_json::to_string_pretty(&plan)?);
            }
        }
        SpecCommand::Profiles { json } => {
            if *json {
                println!("{}", serde_json::to_string(&profiles_json())?);
            } else {
                for (name, p) in enzyme_core::config::readable_builtin_profiles() {
                    print!("{}", enzyme_spec::render_profile(&name, &p));
                }
            }
        }
        SpecCommand::Migrate {
            home,
            embed_profiles_in,
        } => {
            let home = home
                .clone()
                .map(Ok)
                .unwrap_or_else(EnzymePaths::enzyme_home)?;
            let paths =
                enzyme_core::config::migrate_readable_configs(&home, embed_profiles_in.as_deref())?;
            println!("{}", serde_json::to_string_pretty(&paths)?);
        }
        SpecCommand::Inspect {
            vault,
            instructions,
        } => {
            let config = EnzymeConfig::load(vault)?.context("vault has no config")?;
            if *instructions {
                let spec = config
                    .reading_spec
                    .context("vault uses legacy config; no compiled policy")?;
                print!("{}", enzyme_spec::instructions(&spec));
            } else {
                let mut output = serde_json::to_value(&config)?;
                if let Some(spec) = &config.reading_spec {
                    output["reading_program"] = enzyme_spec::render_vault(spec).into();
                }
                println!("{}", serde_json::to_string_pretty(&output)?);
            }
        }
        SpecCommand::Plan {
            file,
            json,
            prompts,
            local,
        } => super::spec_plan::run(super::spec_plan::PlanOptions {
            file,
            vault,
            json: *json,
            prompts: prompts.as_deref(),
            local: *local,
        })?,
        SpecCommand::Serve { bind, port } => serve(bind, *port).await?,
    }
    Ok(())
}
async fn serve(bind: &str, port: u16) -> Result<()> {
    use axum::{
        Json, Router,
        http::{StatusCode, header},
        response::IntoResponse,
        routing::{get, post},
    };
    async fn compile(source: String) -> Json<serde_json::Value> {
        Json(enzyme_spec::playground(&source))
    }
    async fn asset(uri: axum::http::Uri) -> impl IntoResponse {
        let (mime, body) = match uri.path() {
            "/" | "/index.html" => (
                "text/html",
                include_str!("../../../../tools/reading-spec/index.html"),
            ),
            "/app.mjs" => (
                "text/javascript",
                include_str!("../../../../tools/reading-spec/app.mjs"),
            ),
            "/style.css" => (
                "text/css",
                include_str!("../../../../tools/reading-spec/style.css"),
            ),
            "/examples/vault.enzyme" => (
                "text/plain",
                include_str!("../../../../tools/reading-spec/examples/vault.enzyme"),
            ),
            "/examples/memory.enzyme" => (
                "text/plain",
                include_str!("../../../../tools/reading-spec/examples/memory.enzyme"),
            ),
            "/examples/current-engine.enzyme" => (
                "text/plain",
                include_str!("../../../../tools/reading-spec/examples/current-engine.enzyme"),
            ),
            "/examples/overrides.enzyme" => (
                "text/plain",
                include_str!("../../../../tools/reading-spec/examples/overrides.enzyme"),
            ),
            _ => {
                return (
                    StatusCode::NOT_FOUND,
                    [
                        (header::CONTENT_TYPE, "text/plain"),
                        (header::CACHE_CONTROL, "no-store"),
                    ],
                    "Not found",
                );
            }
        };
        (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, mime),
                (header::CACHE_CONTROL, "no-store"),
            ],
            body,
        )
    }
    let app = Router::new()
        .route("/api/compile", post(compile))
        .fallback(get(asset))
        .layer(axum::extract::DefaultBodyLimit::max(1024 * 1024));
    let address: SocketAddr = format!("{bind}:{port}").parse()?;
    let listener = tokio::net::TcpListener::bind(address).await?;
    eprintln!(
        "Reading Spec listening on {} (Rust parser; compilation only)",
        listener.local_addr()?
    );
    axum::serve(listener, app).await?;
    Ok(())
}

pub const PROFILES_SCHEMA: &str = "enzyme.profiles.v1";

/// Built-in catalyst profiles and formats, for setup UIs.
fn profiles_json() -> serde_json::Value {
    use enzyme_core::llm::{
        CATALYST_FORMATS, CATALYST_PROFILES, DEFAULT_FORMAT_KEY, DEFAULT_PROFILE_KEY,
    };
    let definitions = enzyme_core::config::readable_builtin_profiles();
    let profiles: Vec<_> = CATALYST_PROFILES
        .iter()
        .map(|p| {
            serde_json::json!({
                "key": p.key,
                "display_name": p.display_name,
                "description": p.user_description,
                "default": p.key == DEFAULT_PROFILE_KEY,
                "definition": definitions.get(p.key),
            })
        })
        .collect();
    let formats: Vec<_> = CATALYST_FORMATS
        .iter()
        .map(|f| {
            serde_json::json!({
                "key": f.key,
                "display_name": f.display_name,
                "description": f.description,
                "default": f.key == DEFAULT_FORMAT_KEY,
            })
        })
        .collect();
    serde_json::json!({
        "schema": PROFILES_SCHEMA,
        "profiles": profiles,
        "formats": formats,
    })
}
