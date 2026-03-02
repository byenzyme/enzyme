use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Clone)]
pub struct GenerationParams {
    pub max_tokens: u32,
    pub temperature: f32,
    pub top_p: f32,
    pub top_k: u32,
    pub response_format: Option<ResponseFormat>,
}

/// OpenAI-compatible `response_format` for structured outputs.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum ResponseFormat {
    /// `{"type": "json_object"}` — model must return valid JSON.
    #[serde(rename = "json_object")]
    JsonObject,
    /// `{"type": "json_schema", "json_schema": { ... }}` — strict schema.
    #[serde(rename = "json_schema")]
    JsonSchema {
        json_schema: JsonSchemaSpec,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct JsonSchemaSpec {
    pub name: String,
    pub schema: serde_json::Value,
    pub strict: bool,
}

impl Default for GenerationParams {
    fn default() -> Self {
        Self {
            max_tokens: 1024,
            temperature: 0.7,
            top_p: 0.8,
            top_k: 20,
            response_format: None,
        }
    }
}

#[derive(Clone)]
pub struct LlmClient {
    client: reqwest::Client,
    api_key: String,
    base_url: String,
}

#[derive(Serialize)]
struct ChatCompletionRequest {
    model: String,
    messages: Vec<Message>,
    max_tokens: u32,
    temperature: f32,
    top_p: f32,
    top_k: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<ResponseFormat>,
}

#[derive(Deserialize)]
struct ChatCompletionResponse {
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: ChoiceMessage,
}

#[derive(Deserialize)]
struct ChoiceMessage {
    content: String,
}

#[derive(Deserialize)]
struct AuthConfig {
    openrouter_api_key: String,
}

impl LlmClient {
    pub fn new(api_key: &str) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key: api_key.to_string(),
            base_url: DEFAULT_BASE_URL.to_string(),
        }
    }

    pub fn with_base_url(api_key: &str, base_url: &str) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key: api_key.to_string(),
            base_url: base_url.to_string(),
        }
    }

    pub async fn chat_completion(
        &self,
        model: &str,
        messages: Vec<Message>,
        params: GenerationParams,
    ) -> Result<String> {
        let url = format!("{}/chat/completions", self.base_url);

        let request_body = ChatCompletionRequest {
            model: model.to_string(),
            messages,
            max_tokens: params.max_tokens,
            temperature: params.temperature,
            top_p: params.top_p,
            top_k: params.top_k,
            response_format: params.response_format,
        };

        debug!("Sending chat completion request to {} with model {}", url, model);

        let response = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .header("HTTP-Referer", "https://github.com/jshph/enzyme-rust")
            .header("X-Title", "Enzyme")
            .json(&request_body)
            .send()
            .await
            .context("Failed to send request to LLM API")?;

        let status = response.status();
        if !status.is_success() {
            let error_body = response
                .text()
                .await
                .unwrap_or_else(|_| "Failed to read error body".to_string());
            warn!("LLM API returned error status {}: {}", status, error_body);
            bail!("LLM API returned status {}: {}", status, error_body);
        }

        let response_body: ChatCompletionResponse = response
            .json()
            .await
            .context("Failed to parse LLM API response as JSON")?;

        let content = response_body
            .choices
            .into_iter()
            .next()
            .context("LLM API response contained no choices")?
            .message
            .content;

        info!("Received chat completion response ({} chars)", content.len());

        Ok(content)
    }
}

/// Resolve the API key and base URL for LLM requests.
///
/// Checks (in order):
/// 1. `OPENAI_API_KEY` env var + `OPENAI_BASE_URL` (defaults to OpenRouter)
/// 2. `OPENROUTER_API_KEY` env var (legacy, uses OpenRouter base URL)
/// 3. `~/.enzyme/auth.json`
pub fn resolve_api_config() -> Result<(String, String)> {
    // 1. OPENAI_API_KEY + optional OPENAI_BASE_URL (primary)
    if let Ok(key) = std::env::var("OPENAI_API_KEY") {
        if !key.is_empty() {
            let base_url = std::env::var("OPENAI_BASE_URL")
                .unwrap_or_else(|_| DEFAULT_BASE_URL.to_string());
            debug!("Resolved API key from OPENAI_API_KEY (base_url={})", base_url);
            return Ok((key, base_url));
        }
    }

    // 2. OPENROUTER_API_KEY (legacy fallback)
    if let Ok(key) = std::env::var("OPENROUTER_API_KEY") {
        if !key.is_empty() {
            debug!("Resolved API key from OPENROUTER_API_KEY environment variable");
            return Ok((key, DEFAULT_BASE_URL.to_string()));
        }
    }

    // 3. Fall back to ~/.enzyme/auth.json
    let home_dir = dirs::home_dir().context("Could not determine home directory")?;
    let auth_path = home_dir.join(".enzyme").join("auth.json");

    if auth_path.exists() {
        let contents = std::fs::read_to_string(&auth_path)
            .with_context(|| format!("Failed to read auth config at {}", auth_path.display()))?;

        let config: AuthConfig = serde_json::from_str(&contents)
            .with_context(|| format!("Failed to parse auth config at {}", auth_path.display()))?;

        if !config.openrouter_api_key.is_empty() {
            debug!("Resolved API key from {}", auth_path.display());
            return Ok((config.openrouter_api_key, DEFAULT_BASE_URL.to_string()));
        }
    }

    bail!(
        "No API key found. Set OPENAI_API_KEY (+ OPENAI_BASE_URL), OPENROUTER_API_KEY, \
         or create ~/.enzyme/auth.json with {{\"openrouter_api_key\": \"...\"}}"
    )
}

/// Backward-compatible: resolve just the API key.
pub fn resolve_api_key() -> Result<String> {
    resolve_api_config().map(|(key, _)| key)
}
