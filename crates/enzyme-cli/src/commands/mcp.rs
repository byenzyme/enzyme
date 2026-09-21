use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use anyhow::Result;
use serde::Deserialize;

use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::*;
use rmcp::{ErrorData as McpError, ServerHandler, ServiceExt};
use rmcp::{tool, tool_handler, tool_router};

use enzyme_core::config::paths::EnzymePaths;
use enzyme_core::db::Database;
use enzyme_core::embedding::get_embedding_service;
use enzyme_core::search::context::{ContextSearch, TARGET_PROJECTION_NOT_READY};

use super::init::{ensure_fresh_vault, run_init};
use super::petri::build_petri_json;
use super::status::build_status_json;

const SKILL_MD: &str = include_str!("../../../../plugin/skills/enzyme-workspace-setup/SKILL.md");
const KNOWLEDGE_PRACTICE_REVIEW: &str = include_str!(
    "../../../../plugin/skills/enzyme-workspace-setup/references/knowledge-practice-review.md"
);

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

fn setup_instructions() -> String {
    format!(
        "{}\n\n{}",
        strip_frontmatter(SKILL_MD).trim_end(),
        KNOWLEDGE_PRACTICE_REVIEW.trim()
    )
}

#[derive(Clone)]
struct EnzymeMcp {
    vault_path: Arc<RwLock<PathBuf>>,
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
}

impl EnzymeMcp {
    fn new(vault_path: PathBuf) -> Self {
        Self {
            vault_path: Arc::new(RwLock::new(vault_path)),
            tool_router: Self::tool_router(),
        }
    }

    fn vault_path(&self) -> Result<PathBuf, McpError> {
        self.vault_path
            .read()
            .map(|p| p.clone())
            .map_err(|e| McpError::internal_error(format!("lock poisoned: {e}"), None))
    }

    /// Ensure vault freshness before every read tool call.
    ///
    /// MCP servers are commonly long-lived. A once-per-process gate leaves
    /// edits made after the first query invisible until the client restarts.
    /// Uses spawn_blocking + Handle::block_on to bridge the non-Send
    /// future from ensure_fresh_vault into the Send context required by rmcp.
    async fn ensure_fresh(&self) -> Result<(), McpError> {
        self.ensure_fresh_with(|vault_path, handle| {
            handle.block_on(ensure_fresh_vault(&vault_path))
        })
        .await
    }

    async fn ensure_fresh_with<F>(&self, refresh: F) -> Result<(), McpError>
    where
        F: FnOnce(PathBuf, tokio::runtime::Handle) -> Result<()> + Send + 'static,
    {
        let vault_path = self.vault_path()?;
        let handle = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || refresh(vault_path, handle))
            .await
            .map_err(|e| McpError::internal_error(format!("spawn failed: {e}"), None))?
            .map_err(|e| McpError::internal_error(format!("refresh failed: {e}"), None))?;
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

    /// Optional target corpus to prepare and search instead of the source vault.
    /// The source vault still supplies catalysts.
    #[schemars(description = "Optional path to a corpus to prepare and search")]
    target: Option<String>,
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
        let handle = tokio::runtime::Handle::current();
        let result = tokio::task::spawn_blocking(move || {
            handle.block_on(run_init(
                &vault_path,
                false,
                true,
                false,
                None,
                false,
                false,
                false,
            ))
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

    #[tool(
        description = "Search vault or a target corpus by concept using catalyst-based matching"
    )]
    async fn catalyze(
        &self,
        Parameters(p): Parameters<CatalyzeParams>,
    ) -> Result<CallToolResult, McpError> {
        self.ensure_fresh().await?;
        let vault_path = self.vault_path()?;
        let limit = p.limit.unwrap_or(10) as usize;
        let _register = p.register.as_deref().unwrap_or("explore");
        let paths = EnzymePaths::new(&vault_path)
            .map_err(|e| McpError::internal_error(format!("paths failed: {e}"), None))?;
        let db = Database::open(&paths.vault_db)
            .map_err(|e| McpError::internal_error(format!("db open failed: {e}"), None))?;
        let service = get_embedding_service(&paths.home).map_err(|e| {
            McpError::internal_error(format!("embedding service failed: {e}"), None)
        })?;
        let response = if let Some(target) = p.target.as_deref() {
            let target_path = PathBuf::from(target);
            if !target_path.is_dir() {
                return Err(McpError::invalid_params(
                    format!("Not a directory: {}", target_path.display()),
                    None,
                ));
            }
            let target_db_path = paths.target_db(&target_path);
            let mut response = None;
            for _ in 0..4 {
                super::target::prepare_target(&vault_path, &target_path)
                    .await
                    .map_err(|e| {
                        McpError::internal_error(format!("target preparation failed: {e}"), None)
                    })?;
                let target_db = Database::open(&target_db_path).map_err(|e| {
                    McpError::internal_error(format!("target db open failed: {e}"), None)
                })?;
                match ContextSearch::with_target(&db, &target_db, service)
                    .catalyze_search(&p.query, limit)
                {
                    Ok(result) => {
                        response = Some(result);
                        break;
                    }
                    Err(error) if error.to_string().contains(TARGET_PROJECTION_NOT_READY) => {
                        continue;
                    }
                    Err(error) => {
                        return Err(McpError::internal_error(
                            format!("search failed: {error}"),
                            None,
                        ));
                    }
                }
            }
            response.ok_or_else(|| {
                McpError::internal_error(
                    "target projection changed repeatedly during preparation",
                    None,
                )
            })?
        } else {
            ContextSearch::new(&db, service)
                .catalyze_search(&p.query, limit)
                .map_err(|e| McpError::internal_error(format!("search failed: {e}"), None))?
        };

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

#[tool_handler]
impl ServerHandler for EnzymeMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_protocol_version(ProtocolVersion::LATEST)
            .with_server_info(Implementation::new("enzyme", env!("CARGO_PKG_VERSION")))
            .with_instructions(setup_instructions())
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::{EnzymeMcp, setup_instructions};

    #[test]
    fn setup_instructions_compose_the_operational_skill_and_shared_review() {
        let instructions = setup_instructions();
        assert!(instructions.contains("# Enzyme Workspace Setup"));
        assert!(instructions.contains("# Knowledge Practice Review Contract"));
        assert!(instructions.contains("grants no authority to write"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn living_knowledge_base_persistent_server_refreshes_before_each_read() {
        let vault = tempfile::tempdir().unwrap();
        let server = EnzymeMcp::new(vault.path().to_path_buf());
        let attempts = Arc::new(AtomicUsize::new(0));

        for _ in 0..2 {
            let attempts = Arc::clone(&attempts);
            server
                .ensure_fresh_with(move |_vault, _handle| {
                    attempts.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
                .await
                .unwrap();
        }

        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }
}
