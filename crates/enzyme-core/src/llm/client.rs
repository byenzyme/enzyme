use std::time::Duration;

use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1";
pub const DEFAULT_MODEL: &str = "google/gemini-2.5-flash-lite";
const FREE_CONFIG_URL: &str =
    "https://enzyme-server-production.up.railway.app/enzyme-python/free-config";

/// Where the resolved API configuration came from.
#[derive(Debug, Clone)]
pub enum ConfigProvider {
    OpenAiEnv,
    AuthFile,
    ServerFallback,
}

/// Fully resolved LLM configuration (key, URL, model, source).
#[derive(Debug, Clone)]
pub struct ResolvedConfig {
    pub api_key: String,
    pub base_url: String,
    pub model: String,
    pub provider: ConfigProvider,
}

/// Shape of the JSON returned by the free-config endpoint.
#[derive(Deserialize)]
struct FreeConfigResponse {
    api_key: String,
    base_url: String,
    model: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: MessageContent,
}

/// Message content: either a plain string or an array of content parts
/// (used for OpenRouter prompt caching with `cache_control`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Parts(Vec<ContentPart>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContentPart {
    #[serde(rename = "type")]
    pub content_type: String,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheControl {
    #[serde(rename = "type")]
    pub control_type: String,
}

impl From<String> for MessageContent {
    fn from(s: String) -> Self {
        Self::Text(s)
    }
}

impl From<&str> for MessageContent {
    fn from(s: &str) -> Self {
        Self::Text(s.to_string())
    }
}

impl Message {
    /// Simple text message.
    pub fn text(role: &str, content: impl Into<String>) -> Self {
        Self {
            role: role.to_string(),
            content: MessageContent::Text(content.into()),
        }
    }

    /// Message with a cached static prefix and dynamic suffix (for OpenRouter).
    /// Packs both into a single content array with `cache_control: ephemeral` on the prefix.
    pub fn cached(role: &str, cached_prefix: impl Into<String>, dynamic: impl Into<String>) -> Self {
        Self {
            role: role.to_string(),
            content: MessageContent::Parts(vec![
                ContentPart {
                    content_type: "text".to_string(),
                    text: cached_prefix.into(),
                    cache_control: Some(CacheControl {
                        control_type: "ephemeral".to_string(),
                    }),
                },
                ContentPart {
                    content_type: "text".to_string(),
                    text: dynamic.into(),
                    cache_control: None,
                },
            ]),
        }
    }
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

// SSE streaming response types
#[derive(Serialize)]
struct ChatCompletionStreamRequest {
    model: String,
    messages: Vec<Message>,
    max_tokens: u32,
    temperature: f32,
    top_p: f32,
    top_k: u32,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<ResponseFormat>,
}

#[derive(Deserialize)]
struct StreamChunk {
    choices: Vec<StreamChoice>,
}

#[derive(Deserialize)]
struct StreamChoice {
    delta: StreamDelta,
}

#[derive(Deserialize)]
struct StreamDelta {
    content: Option<String>,
}

#[derive(Deserialize)]
struct AuthConfig {
    api_key: String,
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

    /// Whether this client targets OpenRouter (enables prompt caching via cache_control).
    pub fn is_openrouter(&self) -> bool {
        self.base_url.contains("openrouter.ai")
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

    /// Streaming chat completion that calls `on_delta` with accumulated text after each SSE chunk.
    ///
    /// Returns the full accumulated text when the stream completes.
    pub async fn chat_completion_stream(
        &self,
        model: &str,
        messages: Vec<Message>,
        params: GenerationParams,
        mut on_delta: impl FnMut(&str) + Send,
    ) -> Result<String> {
        let url = format!("{}/chat/completions", self.base_url);

        let request_body = ChatCompletionStreamRequest {
            model: model.to_string(),
            messages,
            max_tokens: params.max_tokens,
            temperature: params.temperature,
            top_p: params.top_p,
            top_k: params.top_k,
            stream: true,
            response_format: params.response_format,
        };

        debug!("Sending streaming chat completion request to {} with model {}", url, model);

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
            .context("Failed to send streaming request to LLM API")?;

        let status = response.status();
        if !status.is_success() {
            let error_body = response
                .text()
                .await
                .unwrap_or_else(|_| "Failed to read error body".to_string());
            warn!("LLM API returned error status {}: {}", status, error_body);
            bail!("LLM API returned status {}: {}", status, error_body);
        }

        let mut accumulated = String::new();
        let mut stream = response.bytes_stream();
        let mut line_buf = String::new();

        // Timeout per chunk: if no data arrives for 30s, assume stream is done
        let chunk_timeout = Duration::from_secs(30);

        loop {
            let chunk_result = tokio::time::timeout(chunk_timeout, stream.next()).await;

            match chunk_result {
                Ok(Some(Ok(bytes))) => {
                    let text = String::from_utf8_lossy(&bytes);
                    line_buf.push_str(&text);
                }
                Ok(Some(Err(e))) => {
                    debug!("Error reading stream chunk: {}", e);
                    break;
                }
                Ok(None) => {
                    // Stream ended
                    break;
                }
                Err(_) => {
                    debug!("Stream chunk timeout after {}s", chunk_timeout.as_secs());
                    break;
                }
            }

            // Process complete SSE lines
            while let Some(newline_pos) = line_buf.find('\n') {
                let line = line_buf[..newline_pos].trim_end_matches('\r').to_string();
                line_buf = line_buf[newline_pos + 1..].to_string();

                if line.is_empty() {
                    continue;
                }

                let Some(data) = line.strip_prefix("data: ") else {
                    continue;
                };

                if data == "[DONE]" {
                    info!("Stream complete ({} chars)", accumulated.len());
                    return Ok(accumulated);
                }

                if let Ok(chunk) = serde_json::from_str::<StreamChunk>(data) {
                    for choice in chunk.choices {
                        if let Some(content) = choice.delta.content {
                            accumulated.push_str(&content);
                            on_delta(&accumulated);
                        }
                    }
                }
            }
        }

        // Process any remaining content in the line buffer (no trailing newline)
        let remaining = line_buf.trim();
        if !remaining.is_empty() {
            if let Some(data) = remaining.strip_prefix("data: ") {
                if let Ok(chunk) = serde_json::from_str::<StreamChunk>(data) {
                    for choice in chunk.choices {
                        if let Some(content) = choice.delta.content {
                            accumulated.push_str(&content);
                            on_delta(&accumulated);
                        }
                    }
                }
            } else if accumulated.is_empty() {
                // Not SSE at all — provider may have returned a regular JSON response.
                // Try to extract content from a standard ChatCompletionResponse.
                if let Ok(resp) = serde_json::from_str::<ChatCompletionResponse>(remaining) {
                    if let Some(choice) = resp.choices.into_iter().next() {
                        accumulated = choice.message.content;
                    }
                }
            }
        }

        info!("Stream ended ({} chars)", accumulated.len());
        Ok(accumulated)
    }
}

/// Fetch a free shared API key from the Enzyme server.
async fn fetch_free_config() -> Result<ResolvedConfig> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .context("failed to build HTTP client")?;

    debug!("Fetching free config from {}", FREE_CONFIG_URL);

    let resp: FreeConfigResponse = client
        .get(FREE_CONFIG_URL)
        .send()
        .await
        .context("failed to reach Enzyme server for free config")?
        .error_for_status()
        .context("Enzyme server returned an error for free config")?
        .json()
        .await
        .context("failed to parse free config response")?;

    info!("Resolved API config from Enzyme server (model={})", resp.model);

    Ok(ResolvedConfig {
        api_key: resp.api_key,
        base_url: resp.base_url,
        model: resp.model,
        provider: ConfigProvider::ServerFallback,
    })
}

/// Resolve the API key, base URL, and model for LLM requests.
///
/// Checks (in order):
/// 1. `OPENAI_API_KEY` env var + `OPENAI_BASE_URL` (defaults to OpenRouter)
/// 2. `~/.enzyme/auth.json` → `{"api_key": "..."}`
/// 3. Enzyme server free-config endpoint (shared key for first-time users)
pub async fn resolve_api_config() -> Result<ResolvedConfig> {
    // 1. OPENAI_API_KEY + optional OPENAI_BASE_URL (primary)
    if let Ok(key) = std::env::var("OPENAI_API_KEY") {
        if !key.is_empty() {
            let base_url = std::env::var("OPENAI_BASE_URL")
                .unwrap_or_else(|_| DEFAULT_BASE_URL.to_string());
            debug!("Resolved API key from OPENAI_API_KEY (base_url={})", base_url);
            return Ok(ResolvedConfig {
                api_key: key,
                base_url,
                model: DEFAULT_MODEL.to_string(),
                provider: ConfigProvider::OpenAiEnv,
            });
        }
    }

    // 2. Fall back to ~/.enzyme/auth.json
    let home_dir = dirs::home_dir().context("Could not determine home directory")?;
    let auth_path = home_dir.join(".enzyme").join("auth.json");

    if auth_path.exists() {
        let contents = std::fs::read_to_string(&auth_path)
            .with_context(|| format!("Failed to read auth config at {}", auth_path.display()))?;

        let config: AuthConfig = serde_json::from_str(&contents)
            .with_context(|| format!("Failed to parse auth config at {}", auth_path.display()))?;

        if !config.api_key.is_empty() {
            debug!("Resolved API key from {}", auth_path.display());
            return Ok(ResolvedConfig {
                api_key: config.api_key,
                base_url: DEFAULT_BASE_URL.to_string(),
                model: DEFAULT_MODEL.to_string(),
                provider: ConfigProvider::AuthFile,
            });
        }
    }

    // 3. Fetch shared key from Enzyme server
    match fetch_free_config().await {
        Ok(cfg) => return Ok(cfg),
        Err(e) => {
            warn!("Failed to fetch free config from server: {}", e);
        }
    }

    bail!(
        "No API key found. Checked: OPENAI_API_KEY, \
         ~/.enzyme/auth.json, and Enzyme server. Set one of the above or \
         ensure network access for the free tier."
    )
}

/// Backward-compatible: resolve just the API key.
pub async fn resolve_api_key() -> Result<String> {
    resolve_api_config().await.map(|c| c.api_key)
}
