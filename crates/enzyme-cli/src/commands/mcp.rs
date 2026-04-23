use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use anyhow::Result;
use serde::Deserialize;

use rmcp::handler::server::router::prompt::PromptRouter;
use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::*;
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler, ServiceExt};
use rmcp::{prompt, prompt_handler, prompt_router, tool, tool_handler, tool_router};

use enzyme_core::config::paths::EnzymePaths;
use enzyme_core::db::Database;
use enzyme_core::embedding::get_embedding_service;
use enzyme_core::llm::ContentRegister;
use enzyme_core::search::context::ContextSearch;

use super::init::{ensure_fresh_vault, run_init};
use super::petri::build_petri_json;
use super::status::build_status_json;

const SKILL_MD: &str = include_str!("../../../../plugin/skills/enzyme/SKILL.md");
const PETRI_GUIDE: &str = include_str!("../../../../plugin/skills/enzyme/petri-guide.md");
const SEARCH_GUIDE: &str = include_str!("../../../../plugin/skills/enzyme/search-guide.md");

/// Strip YAML frontmatter (delimited by `---`) from a string.
fn strip_frontmatter(s: &str) -> &str {
    if !s.starts_with("---") {
        return s;
    }
    if let Some(end) = s[3..].find("\n---") {
        let after = end + 3 + 4; // skip past the closing "---\n"
        s[after..].trim_start_matches('\n')
    } else {
        s
    }
}

#[derive(Clone)]
struct EnzymeMcp {
    vault_path: Arc<RwLock<PathBuf>>,
    refreshed: Arc<AtomicBool>,
    tool_router: ToolRouter<Self>,
    prompt_router: PromptRouter<Self>,
}

impl EnzymeMcp {
    fn new(vault_path: PathBuf) -> Self {
        Self {
            vault_path: Arc::new(RwLock::new(vault_path)),
            refreshed: Arc::new(AtomicBool::new(false)),
            tool_router: Self::tool_router(),
            prompt_router: Self::prompt_router(),
        }
    }

    fn vault_path(&self) -> Result<PathBuf, McpError> {
        self.vault_path
            .read()
            .map(|p| p.clone())
            .map_err(|e| McpError::internal_error(format!("lock poisoned: {e}"), None))
    }

    /// Ensure vault freshness on first tool call.
    /// Uses spawn_blocking + Handle::block_on to bridge the non-Send
    /// future from ensure_fresh_vault into the Send context required by rmcp.
    async fn ensure_fresh(&self) -> Result<(), McpError> {
        if !self.refreshed.swap(true, Ordering::SeqCst) {
            let vault_path = self.vault_path()?;
            let handle = tokio::runtime::Handle::current();
            tokio::task::spawn_blocking(move || handle.block_on(ensure_fresh_vault(&vault_path)))
                .await
                .map_err(|e| McpError::internal_error(format!("spawn failed: {e}"), None))?
                .map_err(|e| McpError::internal_error(format!("refresh failed: {e}"), None))?;
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct InitParams {
    /// Path to the Obsidian/Markdown vault directory to initialize
    #[schemars(description = "Absolute path to the Obsidian/Markdown vault directory")]
    vault_path: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct PetriParams {
    /// Number of top entities to return
    #[schemars(description = "Number of top entities to return")]
    top: Option<u64>,

    /// Query to rank entities and catalysts by relevance to the user's prompt
    #[schemars(
        description = "Query to rank entities and catalysts by relevance to the user's prompt"
    )]
    query: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct CatalyzeParams {
    /// Conceptual search query
    #[schemars(description = "Conceptual search query")]
    query: String,

    /// Maximum results
    #[schemars(description = "Maximum results")]
    limit: Option<u64>,

    /// Presentation register: "explore" (default, profile-specific guidance for wonder/exploration),
    /// "continuity" (restore what the user knew, show trajectory, enable forward motion),
    /// "reference" (surface what drew attention, capture patterns)
    #[schemars(description = "Presentation register: explore (default), continuity, or reference")]
    register: Option<String>,
}

#[tool_router]
impl EnzymeMcp {
    #[tool(
        description = "Initialize or refresh the enzyme index for a vault. Call this first if the vault hasn't been set up yet."
    )]
    async fn init(
        &self,
        Parameters(p): Parameters<InitParams>,
    ) -> Result<CallToolResult, McpError> {
        let vault_path = PathBuf::from(&p.vault_path);
        if !vault_path.is_dir() {
            return Err(McpError::invalid_params(
                format!("Not a directory: {}", p.vault_path),
                None,
            ));
        }

        // Update the server's vault path
        {
            let mut vp = self
                .vault_path
                .write()
                .map_err(|e| McpError::internal_error(format!("lock poisoned: {e}"), None))?;
            *vp = vault_path.clone();
        }
        // Reset freshness so ensure_fresh re-checks
        self.refreshed.store(false, Ordering::SeqCst);

        let handle = tokio::runtime::Handle::current();
        let result = tokio::task::spawn_blocking(move || {
            handle.block_on(run_init(&vault_path, false, false, true, false, None, None))
        })
        .await
        .map_err(|e| McpError::internal_error(format!("spawn failed: {e}"), None))?
        .map_err(|e| McpError::internal_error(format!("init failed: {e}"), None))?;

        let vault_path = self.vault_path()?;
        let text = match result {
            Some(r) => {
                let json = r
                    .to_json(&vault_path)
                    .map_err(|e| McpError::internal_error(format!("json failed: {e}"), None))?;
                serde_json::to_string_pretty(&json).map_err(|e| {
                    McpError::internal_error(format!("json serialization failed: {e}"), None)
                })?
            }
            None => format!("Vault initialized at {}", vault_path.display()),
        };
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    #[tool(description = "Show trending entities and their catalysts")]
    async fn petri(
        &self,
        Parameters(p): Parameters<PetriParams>,
    ) -> Result<CallToolResult, McpError> {
        self.ensure_fresh().await?;
        let vault_path = self.vault_path()?;
        let default_top = if p.query.is_some() { 15 } else { usize::MAX };
        let top = p.top.map(|t| t as usize).unwrap_or(default_top);
        let json = build_petri_json(&vault_path, top, p.query.as_deref(), None)
            .map_err(|e| McpError::internal_error(format!("petri failed: {e}"), None))?;
        let text = serde_json::to_string_pretty(&json).map_err(|e| {
            McpError::internal_error(format!("json serialization failed: {e}"), None)
        })?;
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    #[tool(description = "Search vault by concept using catalyst-based matching")]
    async fn catalyze(
        &self,
        Parameters(p): Parameters<CatalyzeParams>,
    ) -> Result<CallToolResult, McpError> {
        self.ensure_fresh().await?;
        let vault_path = self.vault_path()?;
        let limit = p.limit.unwrap_or(10) as usize;
        let paths = EnzymePaths::new(&vault_path)
            .map_err(|e| McpError::internal_error(format!("paths failed: {e}"), None))?;
        let db = Database::open(&paths.vault_db)
            .map_err(|e| McpError::internal_error(format!("db open failed: {e}"), None))?;
        let service = get_embedding_service(&paths.home).map_err(|e| {
            McpError::internal_error(format!("embedding service failed: {e}"), None)
        })?;
        let search = ContextSearch::new(&db, service);
        let response = search
            .catalyze_search(&p.query, limit)
            .map_err(|e| McpError::internal_error(format!("search failed: {e}"), None))?;

        // Enrich with register-appropriate presentation guidance
        let register = p
            .register
            .as_deref()
            .and_then(ContentRegister::from_str_opt)
            .unwrap_or_default();
        let response = response.with_presentation_guidance(
            register,
            &std::collections::HashMap::new(),
            None,
        );

        let text = serde_json::to_string_pretty(&response).map_err(|e| {
            McpError::internal_error(format!("json serialization failed: {e}"), None)
        })?;
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    #[tool(description = "Show vault indexing stats")]
    async fn status(&self) -> Result<CallToolResult, McpError> {
        let vault_path = self.vault_path()?;
        let handle = tokio::runtime::Handle::current();
        let json =
            tokio::task::spawn_blocking(move || handle.block_on(build_status_json(&vault_path)))
                .await
                .map_err(|e| McpError::internal_error(format!("spawn failed: {e}"), None))?
                .map_err(|e| McpError::internal_error(format!("status failed: {e}"), None))?;

        let text = match json {
            Some(v) => serde_json::to_string_pretty(&v).map_err(|e| {
                McpError::internal_error(format!("json serialization failed: {e}"), None)
            })?,
            None => {
                "Vault not initialized. Run the 'init' tool with your vault path first.".to_string()
            }
        };
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }
}

#[prompt_router]
impl EnzymeMcp {
    #[prompt(name = "petri-guide", description = "How to present petri results")]
    async fn petri_guide(&self) -> Result<GetPromptResult, McpError> {
        Ok(GetPromptResult::new(vec![PromptMessage::new_text(
            PromptMessageRole::Assistant,
            PETRI_GUIDE,
        )]))
    }

    #[prompt(name = "search-guide", description = "How to present search results")]
    async fn search_guide(&self) -> Result<GetPromptResult, McpError> {
        Ok(GetPromptResult::new(vec![PromptMessage::new_text(
            PromptMessageRole::Assistant,
            SEARCH_GUIDE,
        )]))
    }
}

#[tool_handler]
#[prompt_handler]
impl ServerHandler for EnzymeMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_prompts()
                .build(),
        )
        .with_protocol_version(ProtocolVersion::LATEST)
        .with_server_info(Implementation::new("enzyme", env!("CARGO_PKG_VERSION")))
        .with_instructions(strip_frontmatter(SKILL_MD))
    }
}

pub async fn run_mcp(vault: Option<PathBuf>) -> Result<()> {
    let vault_path = vault
        .or_else(|| std::env::var("ENZYME_VAULT_PATH").ok().map(PathBuf::from))
        .unwrap_or(std::env::current_dir().unwrap_or_default());
    let server = EnzymeMcp::new(vault_path);
    let service = server.serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}
