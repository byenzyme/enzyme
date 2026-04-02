use std::pin::Pin;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use futures::Stream;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1";
pub const DEFAULT_MODEL: &str = "google/gemini-3-flash-preview";
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

impl ResolvedConfig {
    /// Format a masked display of the API key (e.g. `sk-••••••••abcd`).
    pub fn masked_api_key(&self) -> String {
        let key = &self.api_key;
        if key.len() <= 4 {
            "••••".to_string()
        } else {
            let visible = &key[key.len() - 4..];
            format!("{}••••{}", &key[..3.min(key.len())], visible)
        }
    }
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
    /// Always "text" for now.
    #[serde(rename = "type")]
    content_type: ContentType,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum ContentType {
    #[serde(rename = "text")]
    Text,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheControl {
    /// Always "ephemeral" for OpenRouter prompt caching.
    #[serde(rename = "type")]
    control_type: CacheControlType,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum CacheControlType {
    #[serde(rename = "ephemeral")]
    Ephemeral,
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
                    content_type: ContentType::Text,
                    text: cached_prefix.into(),
                    cache_control: Some(CacheControl {
                        control_type: CacheControlType::Ephemeral,
                    }),
                },
                ContentPart {
                    content_type: ContentType::Text,
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

/// What level of structured output a provider supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StructuredOutputSupport {
    /// Full `json_schema` with strict mode (OpenAI, OpenRouter, LM Studio, Venice, vLLM).
    JsonSchema,
    /// `json_object` only — valid JSON but no shape guarantee (fallback).
    JsonObject,
    /// No `response_format` support — rely on prompt instructions + parse fallback.
    None,
}

impl StructuredOutputSupport {
    /// Detect the best structured output mode from a base URL.
    pub fn detect(base_url: &str) -> Self {
        let url = base_url.to_lowercase();

        // Providers with full json_schema support
        if url.contains("openrouter.ai")
            || url.contains("api.openai.com")
            || url.contains("api.venice.ai")
        {
            return Self::JsonSchema;
        }

        // LM Studio only supports json_schema (rejects json_object)
        if url.contains("localhost") || url.contains("127.0.0.1") || url.contains("0.0.0.0") {
            // Local servers: LM Studio, vLLM, llama.cpp, Ollama all support
            // json_schema to varying degrees. json_schema is the safest common
            // denominator — LM Studio rejects json_object, and llama.cpp/Ollama
            // handle json_schema more reliably than json_object.
            return Self::JsonSchema;
        }

        // Unknown provider — try json_schema, parsing fallback catches failures
        Self::JsonSchema
    }

    /// Build the appropriate `ResponseFormat` for this provider's capabilities.
    ///
    /// If a schema is provided and the provider supports it, uses `json_schema`.
    /// Otherwise falls back to `json_object` or `None`.
    pub fn build_response_format(
        &self,
        schema_name: &str,
        schema: serde_json::Value,
    ) -> Option<ResponseFormat> {
        match self {
            Self::JsonSchema => Some(ResponseFormat::JsonSchema {
                json_schema: JsonSchemaSpec {
                    name: schema_name.to_string(),
                    schema,
                    strict: true,
                },
            }),
            Self::JsonObject => Some(ResponseFormat::JsonObject),
            Self::None => None,
        }
    }
}

impl Default for GenerationParams {
    fn default() -> Self {
        Self {
            max_tokens: 8192,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<bool>,
}

#[derive(Deserialize)]
struct ChatCompletionResponse {
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: ChoiceMessage,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct ChoiceMessage {
    content: String,
}

/// SSE streaming response chunk.
#[derive(Deserialize)]
struct StreamingChunkResponse {
    choices: Vec<StreamingChoice>,
}

#[derive(Deserialize)]
struct StreamingChoice {
    delta: StreamingDelta,
}

#[derive(Deserialize)]
struct StreamingDelta {
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

    /// Detect structured output support for this client's provider.
    pub fn structured_output_support(&self) -> StructuredOutputSupport {
        StructuredOutputSupport::detect(&self.base_url)
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
            stream: None,
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

        let choice = response_body
            .choices
            .into_iter()
            .next()
            .context("LLM API response contained no choices")?;

        if let Some(ref reason) = choice.finish_reason {
            if reason != "stop" {
                debug!(finish_reason = %reason, "LLM response did not finish normally — output may be truncated");
            }
        }

        let content = choice.message.content;
        info!("Received chat completion response ({} chars)", content.len());

        Ok(content)
    }



    /// Streaming chat completion that returns a stream of token strings.
    ///
    /// Uses the OpenAI-compatible SSE streaming protocol (`stream: true`).
    /// Each yielded item is a content delta string (a token or small chunk of text).
    pub async fn chat_completion_streaming(
        &self,
        model: &str,
        messages: Vec<Message>,
        params: GenerationParams,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<String>> + Send>>> {
        let url = format!("{}/chat/completions", self.base_url);

        let request_body = ChatCompletionRequest {
            model: model.to_string(),
            messages,
            max_tokens: params.max_tokens,
            temperature: params.temperature,
            top_p: params.top_p,
            top_k: params.top_k,
            response_format: params.response_format,
            stream: Some(true),
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

        let byte_stream = response.bytes_stream();

        let stream = async_stream::try_stream! {
            use futures::StreamExt;

            let mut buffer = String::new();
            let mut byte_stream = std::pin::pin!(byte_stream);

            while let Some(chunk_result) = byte_stream.next().await {
                let chunk = chunk_result.context("Error reading streaming response")?;
                buffer.push_str(&String::from_utf8_lossy(&chunk));

                // Process complete SSE lines from the buffer
                while let Some(newline_pos) = buffer.find('\n') {
                    let line = buffer[..newline_pos].trim_end_matches('\r').to_string();
                    buffer = buffer[newline_pos + 1..].to_string();

                    if line.is_empty() || line.starts_with(':') {
                        continue;
                    }

                    if let Some(data) = line.strip_prefix("data: ") {
                        let data = data.trim();
                        if data == "[DONE]" {
                            return;
                        }

                        if let Ok(parsed) = serde_json::from_str::<StreamingChunkResponse>(data) {
                            if let Some(choice) = parsed.choices.first() {
                                if let Some(content) = &choice.delta.content {
                                    if !content.is_empty() {
                                        yield content.clone();
                                    }
                                }
                            }
                        }
                    }
                }
            }
        };

        Ok(Box::pin(stream))
    }

    /// Chat completion with provider-aware prompt caching.
    ///
    /// For OpenRouter: packs system + user into a single user message with
    /// `cache_control: ephemeral` on the system prefix so it's cached across
    /// parallel calls. For other providers: standard system + user format.
    pub async fn chat_completion_cached(
        &self,
        model: &str,
        system_prompt: &str,
        user_prompt: &str,
        params: GenerationParams,
    ) -> Result<String> {
        let messages = if self.is_openrouter() {
            vec![Message::cached("user", system_prompt, user_prompt)]
        } else {
            vec![
                Message::text("system", system_prompt),
                Message::text("user", user_prompt),
            ]
        };
        self.chat_completion(model, messages, params).await
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
/// 1. `OPENAI_API_KEY` + `OPENAI_BASE_URL` env vars (both required) + optional `OPENAI_MODEL`
/// 2. `~/.enzyme/auth.json` → `{"api_key": "..."}`
/// 3. Enzyme server free-config endpoint (shared key for first-time users)
pub async fn resolve_api_config() -> Result<ResolvedConfig> {
    // 1. OPENAI_API_KEY + OPENAI_BASE_URL (both must be set for env override)
    let has_key = std::env::var("OPENAI_API_KEY").ok().filter(|v| !v.is_empty());
    let has_url = std::env::var("OPENAI_BASE_URL").ok().filter(|v| !v.is_empty());

    match (&has_key, &has_url) {
        (Some(key), Some(base_url)) => {
            let model = std::env::var("OPENAI_MODEL")
                .unwrap_or_else(|_| DEFAULT_MODEL.to_string());
            debug!("Resolved API key from OPENAI_API_KEY (base_url={}, model={})", base_url, model);
            return Ok(ResolvedConfig {
                api_key: key.clone(),
                base_url: base_url.clone(),
                model,
                provider: ConfigProvider::OpenAiEnv,
            });
        }
        (Some(_), None) => {
            warn!("OPENAI_API_KEY is set but OPENAI_BASE_URL is not — ignoring partial env config");
        }
        (None, Some(_)) => {
            warn!("OPENAI_BASE_URL is set but OPENAI_API_KEY is not — ignoring partial env config");
        }
        (None, None) => {}
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
            let model = std::env::var("OPENAI_MODEL")
                .unwrap_or_else(|_| DEFAULT_MODEL.to_string());
            debug!("Resolved API key from {}", auth_path.display());
            return Ok(ResolvedConfig {
                api_key: config.api_key,
                base_url: DEFAULT_BASE_URL.to_string(),
                model,
                provider: ConfigProvider::AuthFile,
            });
        }
    }

    // 3. Fetch shared key from Enzyme server
    match fetch_free_config().await {
        Ok(cfg) => return Ok(cfg),
        Err(e) => {
            debug!("Failed to fetch free config from server: {}", e);
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
