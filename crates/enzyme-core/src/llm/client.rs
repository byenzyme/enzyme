use std::pin::Pin;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures::Stream;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1";
pub const DEFAULT_MODEL: &str = "google/gemini-3-flash-preview";
const OPENAI_DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
const OPENAI_DEFAULT_MODEL: &str = "gpt-4o-mini";
const FREE_CONFIG_URL: &str = "https://api.enzyme.garden/enzyme-python/free-config";

/// Where the resolved API configuration came from.
#[derive(Debug, Clone)]
pub enum ConfigProvider {
    Env,
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

/// Which LLM credentials the caller explicitly wants to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LlmCredentialMode {
    /// Use Enzyme hosted bootstrap/auth and ignore inherited env LLM keys.
    Hosted,
    /// Use OPENAI_*/OPENROUTER_* env configuration only.
    Env,
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

    /// Model to use for LLM selection (one-shot, needs strong reasoning).
    ///
    /// For free-tier users the server may return a cheap catalyst model
    /// (e.g. flash-lite). Selection is a single call that benefits from a
    /// more capable model, so we upgrade to DEFAULT_MODEL in that case.
    pub fn selection_model(&self) -> &str {
        match self.provider {
            ConfigProvider::ServerFallback => DEFAULT_MODEL,
            _ => &self.model,
        }
    }

    /// Whether this config came from the free-tier server fallback.
    pub fn is_free_tier(&self) -> bool {
        matches!(self.provider, ConfigProvider::ServerFallback)
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
    pub fn cached(
        role: &str,
        cached_prefix: impl Into<String>,
        dynamic: impl Into<String>,
    ) -> Self {
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
    /// Optional `top_k` sampling parameter.
    ///
    /// Only a handful of providers accept this (OpenRouter, Together AI, vLLM,
    /// LM Studio, Anthropic).  OpenAI, Azure OpenAI, Groq, and Ollama reject
    /// unknown parameters with a 400 error, so we skip serialization when this
    /// is `None`.
    pub top_k: Option<u32>,
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
    JsonSchema { json_schema: JsonSchemaSpec },
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
            top_k: None,
            response_format: None,
        }
    }
}

#[derive(Clone)]
pub struct LlmClient {
    client: reqwest::Client,
    api_key: Arc<RwLock<String>>,
    base_url: String,
    /// Whether this client was created from the free-tier server fallback.
    /// When true, 401 errors trigger an automatic key refresh.
    free_tier: bool,
}

#[derive(Serialize)]
struct ChatCompletionRequest {
    model: String,
    messages: Vec<Message>,
    #[serde(flatten)]
    token_limit: CompletionTokenLimit,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_k: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<ResponseFormat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<bool>,
}

#[derive(Serialize)]
#[serde(untagged)]
enum CompletionTokenLimit {
    MaxTokens { max_tokens: u32 },
    MaxCompletionTokens { max_completion_tokens: u32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProviderKind {
    OpenAi,
    OpenRouter,
    Anthropic,
    Gemini,
    Local,
    Unknown,
}

impl ProviderKind {
    fn detect(base_url: &str) -> Self {
        let url = base_url.to_lowercase();
        if url.contains("openrouter.ai") {
            Self::OpenRouter
        } else if url.contains("api.openai.com") {
            Self::OpenAi
        } else if url.contains("api.anthropic.com") {
            Self::Anthropic
        } else if url.contains("generativelanguage.googleapis.com")
            || url.contains("googleapis.com")
        {
            Self::Gemini
        } else if url.contains("localhost") || url.contains("127.0.0.1") || url.contains("0.0.0.0")
        {
            Self::Local
        } else {
            Self::Unknown
        }
    }

    fn structured_output_support(self) -> StructuredOutputSupport {
        match self {
            // OpenRouter normalizes response_format across providers; preserve
            // existing behavior, including json_schema for catalyst extraction.
            Self::OpenRouter | Self::OpenAi | Self::Gemini | Self::Local | Self::Unknown => {
                StructuredOutputSupport::JsonSchema
            }
            // Anthropic's OpenAI-compatible endpoint documents response_format
            // as ignored, so sending it gives callers false confidence.
            Self::Anthropic => StructuredOutputSupport::None,
        }
    }

    fn supports_top_k(self) -> bool {
        matches!(self, Self::OpenRouter | Self::Local | Self::Unknown)
    }

    fn should_send_temperature(self, model: &str) -> bool {
        self != Self::Anthropic || !anthropic_model_rejects_sampling_parameters(model)
    }

    fn should_send_top_p(self, model: &str) -> bool {
        // OpenRouter accepts and normalizes top_p; keep backward compatibility
        // even when the routed model is Anthropic.
        if self == Self::OpenRouter {
            return true;
        }

        // Older Claude 4 endpoints reject the temperature/top_p combination,
        // while newer models reject all non-default sampling parameters.
        if self == Self::Anthropic
            && (is_claude_4_family(model) || anthropic_model_rejects_sampling_parameters(model))
        {
            return false;
        }

        true
    }
}

fn is_claude_4_family(model: &str) -> bool {
    let model = model.to_lowercase();
    model.contains("claude") && (model.contains("-4") || model.contains("opus-4"))
}

fn anthropic_model_rejects_sampling_parameters(model: &str) -> bool {
    let model = model.to_lowercase();
    [
        "claude-opus-4-7",
        "claude-opus-4-8",
        "claude-sonnet-5",
        "claude-fable-5",
    ]
    .iter()
    .any(|family| {
        model == *family
            || model
                .strip_prefix(family)
                .is_some_and(|suffix| suffix.starts_with('-'))
    })
}

fn completion_token_limit(base_url: &str, model: &str, max_tokens: u32) -> CompletionTokenLimit {
    if requires_max_completion_tokens(base_url, model) {
        CompletionTokenLimit::MaxCompletionTokens {
            max_completion_tokens: max_tokens,
        }
    } else {
        CompletionTokenLimit::MaxTokens { max_tokens }
    }
}

fn requires_max_completion_tokens(base_url: &str, model: &str) -> bool {
    let base_url = base_url.to_lowercase();
    let model = model.to_lowercase();

    base_url.contains("api.openai.com")
        && (model.starts_with("gpt-5")
            || model.starts_with("o1")
            || model.starts_with("o3")
            || model.starts_with("o4"))
}

fn chat_completion_request(
    base_url: &str,
    model: &str,
    messages: Vec<Message>,
    params: GenerationParams,
    stream: Option<bool>,
) -> ChatCompletionRequest {
    let provider = ProviderKind::detect(base_url);
    let response_format = if provider.structured_output_support() == StructuredOutputSupport::None {
        None
    } else {
        params.response_format
    };

    ChatCompletionRequest {
        model: model.to_string(),
        messages,
        token_limit: completion_token_limit(base_url, model, params.max_tokens),
        temperature: provider
            .should_send_temperature(model)
            .then_some(params.temperature),
        top_p: provider.should_send_top_p(model).then_some(params.top_p),
        top_k: provider.supports_top_k().then_some(params.top_k).flatten(),
        response_format,
        stream,
    }
}

fn chat_completions_url(base_url: &str) -> Result<String> {
    let mut url = reqwest::Url::parse(base_url)
        .with_context(|| format!("Invalid LLM base URL: {base_url}"))?;
    let path = url.path().trim_end_matches('/');
    url.set_path(&format!("{path}/chat/completions"));
    url.set_query(None);
    url.set_fragment(None);
    Ok(url.to_string())
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

/// How long a cached free-tier config is considered fresh (5 minutes).
/// OpenRouter free-tier keys rotate frequently; a short TTL ensures we
/// re-fetch before the key expires rather than discovering it mid-run via 401.
const FREE_CONFIG_CACHE_TTL_SECS: u64 = 300;

#[derive(Deserialize, Serialize)]
struct CachedFreeConfig {
    api_key: String,
    base_url: String,
    model: String,
    /// Unix timestamp (seconds) when the config was cached.
    #[serde(default)]
    cached_at: u64,
}

#[derive(Deserialize, Serialize)]
struct BootstrapIdentity {
    bootstrap_id: String,
}

impl LlmClient {
    pub fn new(api_key: &str) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key: Arc::new(RwLock::new(api_key.to_string())),
            base_url: DEFAULT_BASE_URL.to_string(),
            free_tier: false,
        }
    }

    pub fn with_base_url(api_key: &str, base_url: &str) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key: Arc::new(RwLock::new(api_key.to_string())),
            base_url: base_url.to_string(),
            free_tier: false,
        }
    }

    /// Create a client from a resolved config, preserving free-tier status
    /// so that 401 errors can trigger automatic key refresh.
    pub fn from_config(config: &ResolvedConfig) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key: Arc::new(RwLock::new(config.api_key.clone())),
            base_url: config.base_url.clone(),
            free_tier: config.is_free_tier(),
        }
    }

    /// Whether this client targets OpenRouter (enables prompt caching via cache_control).
    pub fn is_openrouter(&self) -> bool {
        self.base_url.contains("openrouter.ai")
    }

    /// Whether this client targets a local/fine-tuned model server.
    ///
    /// True for localhost URLs, or when `ENZYME_LOCAL_MODEL=1` is set
    /// (e.g. a fine-tuned model hosted remotely that uses the compact prompt format).
    pub fn is_local(&self) -> bool {
        if std::env::var("ENZYME_LOCAL_MODEL")
            .ok()
            .filter(|v| v == "1" || v == "true")
            .is_some()
        {
            return true;
        }
        let url = self.base_url.to_lowercase();
        url.contains("localhost") || url.contains("127.0.0.1") || url.contains("0.0.0.0")
    }

    /// Detect structured output support for this client's provider.
    pub fn structured_output_support(&self) -> StructuredOutputSupport {
        ProviderKind::detect(&self.base_url).structured_output_support()
    }

    /// Read the current API key.
    fn current_api_key(&self) -> String {
        self.api_key.read().unwrap().clone()
    }

    /// Attempt to refresh the API key by re-fetching from the free-config endpoint.
    /// Returns true if the key was actually updated.
    async fn try_refresh_key(&self) -> bool {
        if !self.free_tier {
            return false;
        }
        info!("free-tier key returned 401, refreshing from server");
        invalidate_free_config_cache();
        match fetch_free_config().await {
            Ok(cfg) => {
                *self.api_key.write().unwrap() = cfg.api_key.clone();
                write_free_config_cache(std::path::Path::new("/tmp/enzyme-api-cache"), &cfg);
                true
            }
            Err(e) => {
                warn!("failed to refresh free-tier key: {}", e);
                false
            }
        }
    }

    /// Send a single chat completion request with the given API key.
    async fn send_chat_completion(
        &self,
        url: &str,
        api_key: &str,
        request_body: &ChatCompletionRequest,
    ) -> Result<String> {
        let response = self
            .client
            .post(url)
            .header("Authorization", format!("Bearer {}", api_key))
            .header("Content-Type", "application/json")
            .header("HTTP-Referer", "https://github.com/jshph/enzyme-rust")
            .header("X-Title", "Enzyme")
            .json(request_body)
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
        info!(
            "Received chat completion response ({} chars)",
            content.len()
        );

        Ok(content)
    }

    async fn send_streaming_request(
        &self,
        url: &str,
        api_key: &str,
        request_body: &ChatCompletionRequest,
    ) -> Result<reqwest::Response> {
        self.client
            .post(url)
            .header("Authorization", format!("Bearer {}", api_key))
            .header("Content-Type", "application/json")
            .header("HTTP-Referer", "https://github.com/jshph/enzyme-rust")
            .header("X-Title", "Enzyme")
            .json(request_body)
            .send()
            .await
            .context("Failed to send streaming request to LLM API")
    }

    pub async fn chat_completion(
        &self,
        model: &str,
        messages: Vec<Message>,
        params: GenerationParams,
    ) -> Result<String> {
        let url = chat_completions_url(&self.base_url)?;

        let request_body = chat_completion_request(&self.base_url, model, messages, params, None);

        debug!(
            "Sending chat completion request to {} with model {}",
            url, model
        );

        let api_key = self.current_api_key();
        match self
            .send_chat_completion(&url, &api_key, &request_body)
            .await
        {
            Ok(content) => Ok(content),
            Err(e) if self.free_tier && e.to_string().contains("401") => {
                // Key likely rotated — refresh and retry once
                if self.try_refresh_key().await {
                    let new_key = self.current_api_key();
                    self.send_chat_completion(&url, &new_key, &request_body)
                        .await
                } else {
                    Err(e)
                }
            }
            Err(e) => Err(e),
        }
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
        let url = chat_completions_url(&self.base_url)?;

        let request_body =
            chat_completion_request(&self.base_url, model, messages, params, Some(true));

        debug!(
            "Sending streaming chat completion request to {} with model {}",
            url, model
        );

        let api_key = self.current_api_key();
        let mut response = self
            .send_streaming_request(&url, &api_key, &request_body)
            .await?;

        if self.free_tier && response.status() == reqwest::StatusCode::UNAUTHORIZED {
            if self.try_refresh_key().await {
                let new_key = self.current_api_key();
                response = self
                    .send_streaming_request(&url, &new_key, &request_body)
                    .await?;
            }
        }

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

/// Delete the cached free-tier config so the next resolve re-fetches.
fn invalidate_free_config_cache() {
    let _ = std::fs::remove_file("/tmp/enzyme-api-cache");
}

/// Write a resolved free-tier config to the cache with a timestamp.
fn write_free_config_cache(path: &std::path::Path, cfg: &ResolvedConfig) {
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let cached = CachedFreeConfig {
        api_key: cfg.api_key.clone(),
        base_url: cfg.base_url.clone(),
        model: cfg.model.clone(),
        cached_at: now_secs,
    };
    if let Ok(json) = serde_json::to_string(&cached) {
        let _ = std::fs::write(path, json);
    }
}

fn load_or_create_bootstrap_id(path: &std::path::Path) -> Result<String> {
    if path.exists() {
        let contents = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        if let Ok(identity) = serde_json::from_str::<BootstrapIdentity>(&contents) {
            if !identity.bootstrap_id.trim().is_empty() {
                return Ok(identity.bootstrap_id);
            }
        }
    }

    let identity = BootstrapIdentity {
        bootstrap_id: uuid::Uuid::new_v4().to_string(),
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let contents = serde_json::to_string_pretty(&identity)
        .context("failed to serialize bootstrap identity")?;
    std::fs::write(path, contents)
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(identity.bootstrap_id)
}

/// Fetch a free shared API key from the Enzyme server.
async fn fetch_free_config() -> Result<ResolvedConfig> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .context("failed to build HTTP client")?;

    debug!("Fetching free config from {}", FREE_CONFIG_URL);

    // If the user is logged in, attach auth so the server can gate/charge
    // account credits. First initialization is allowed to be loginless; in
    // that case the server may dispense a limited bootstrap config.
    let enzyme_home = crate::config::paths::EnzymePaths::enzyme_home()
        .context("failed to resolve enzyme home for auth lookup")?;
    let auth_path = enzyme_home.join("auth.json");
    let token = crate::config::auth::AuthToken::load(&auth_path).ok();

    let mut req = client.get(FREE_CONFIG_URL);
    if let Some(token) = &token {
        req = req.header("Authorization", format!("Bearer {}", token.api_key));
    } else {
        let bootstrap_id = load_or_create_bootstrap_id(&enzyme_home.join("bootstrap.json"))?;
        req = req.header("X-Enzyme-Bootstrap-Id", bootstrap_id);
    }

    let resp = req
        .send()
        .await
        .context("failed to reach Enzyme server for free config")?;

    if token.is_none() && matches!(resp.status().as_u16(), 401 | 403) {
        bail!(
            "Enzyme hosted bootstrap is unavailable without login. Run `enzyme login` and retry, or pass `--use-env-llm` to intentionally use OPENAI_API_KEY or OPENROUTER_API_KEY."
        );
    }

    if resp.status().as_u16() == 402 {
        let body: serde_json::Value = resp.json().await.unwrap_or_default();
        let credits = body["credits"].as_f64().unwrap_or(0.0);
        bail!(
            "Insufficient credits ({:.1} remaining). Visit app.enzyme.garden/plans to top up.",
            credits
        );
    }

    let resp: FreeConfigResponse = resp
        .error_for_status()
        .context("Enzyme server returned an error for free config")?
        .json()
        .await
        .context("failed to parse free config response")?;

    info!(
        "Resolved API config from Enzyme server (model={})",
        resp.model
    );

    Ok(ResolvedConfig {
        api_key: resp.api_key,
        base_url: resp.base_url,
        model: resp.model,
        provider: ConfigProvider::ServerFallback,
    })
}

fn env_value(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn first_env_value<'a>(
    names: impl IntoIterator<Item = &'a str>,
    get_env: &impl Fn(&str) -> Option<String>,
) -> Option<String> {
    names.into_iter().find_map(get_env)
}

fn resolved_env_config(get_env: impl Fn(&str) -> Option<String>) -> Result<Option<ResolvedConfig>> {
    let openai_key = first_env_value(["OPENAI_API_KEY"], &get_env);
    let openrouter_key = first_env_value(["OPENROUTER_API_KEY"], &get_env);

    if let Some(key) = openai_key {
        let base_url = first_env_value(["OPENAI_BASE_URL"], &get_env)
            .unwrap_or_else(|| OPENAI_DEFAULT_BASE_URL.to_string());
        let model = first_env_value(["OPENAI_MODEL"], &get_env)
            .unwrap_or_else(|| OPENAI_DEFAULT_MODEL.to_string());
        debug!(
            "Resolved API key from OPENAI_API_KEY (base_url={}, model={})",
            base_url, model
        );
        return Ok(Some(ResolvedConfig {
            api_key: key,
            base_url,
            model,
            provider: ConfigProvider::Env,
        }));
    }

    if let Some(key) = openrouter_key {
        let base_url = first_env_value(["OPENROUTER_BASE_URL"], &get_env)
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
        let model = first_env_value(["OPENROUTER_MODEL"], &get_env)
            .unwrap_or_else(|| DEFAULT_MODEL.to_string());
        debug!(
            "Resolved API key from OPENROUTER_API_KEY (base_url={}, model={})",
            base_url, model
        );
        return Ok(Some(ResolvedConfig {
            api_key: key,
            base_url,
            model,
            provider: ConfigProvider::Env,
        }));
    }

    for var in [
        "OPENAI_BASE_URL",
        "OPENAI_MODEL",
        "OPENROUTER_BASE_URL",
        "OPENROUTER_MODEL",
    ] {
        if get_env(var).is_some() {
            warn!("{var} is set but no LLM API key env var is set — ignoring partial env config");
            break;
        }
    }

    Ok(None)
}

async fn resolve_hosted_api_config() -> Result<ResolvedConfig> {
    // Reuse cached free-tier config from /tmp if present and not expired.
    let cache_path = std::path::Path::new("/tmp/enzyme-api-cache");
    if cache_path.exists() {
        let contents = std::fs::read_to_string(cache_path)
            .with_context(|| format!("Failed to read {}", cache_path.display()))?;
        let cached: CachedFreeConfig = serde_json::from_str(&contents)
            .with_context(|| format!("Failed to parse {}", cache_path.display()))?;

        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let age_secs = now_secs.saturating_sub(cached.cached_at);
        let expired = cached.cached_at == 0 || age_secs > FREE_CONFIG_CACHE_TTL_SECS;

        if !cached.api_key.is_empty() && !expired {
            debug!(
                "Resolved API config from {} (age={}s)",
                cache_path.display(),
                age_secs
            );
            return Ok(ResolvedConfig {
                api_key: cached.api_key,
                base_url: cached.base_url,
                model: cached.model,
                provider: ConfigProvider::ServerFallback,
            });
        }

        if expired {
            debug!(
                "Cached free-tier config expired (age={}s, ttl={}s), re-fetching",
                age_secs, FREE_CONFIG_CACHE_TTL_SECS
            );
        }
    }

    // Fetch shared key from Enzyme server.
    match fetch_free_config().await {
        Ok(cfg) => {
            write_free_config_cache(cache_path, &cfg);
            Ok(cfg)
        }
        Err(e) => bail!(hosted_config_unavailable_message(&e)),
    }
}

/// Keep the specific hosted failure (insufficient credits, login required,
/// server unreachable) visible in the single-line error the CLI prints,
/// since most callers format errors with `{}` and would otherwise show only
/// a generic message.
fn hosted_config_unavailable_message(cause: &anyhow::Error) -> String {
    format!(
        "{cause:#}. No hosted Enzyme LLM config available — run `enzyme login` and ensure \
         network access/credits, or pass `--use-env-llm` to intentionally use \
         OPENAI_API_KEY or OPENROUTER_API_KEY."
    )
}

/// Resolve the API key, base URL, and model for LLM requests.
///
/// Checks (in order):
/// 1. `OPENAI_API_KEY` + optional `OPENAI_BASE_URL` / `OPENAI_MODEL`
/// 2. `OPENROUTER_API_KEY` + optional `OPENROUTER_BASE_URL` / `OPENROUTER_MODEL`
/// 3. `/tmp/enzyme-api-cache`
/// 4. Enzyme server free-config endpoint (hosted credits)
pub async fn resolve_api_config() -> Result<ResolvedConfig> {
    if let Some(config) = resolved_env_config(env_value)? {
        return Ok(config);
    }
    resolve_hosted_api_config().await
}

/// Resolve API configuration in an explicit credential mode.
pub async fn resolve_api_config_for_mode(mode: LlmCredentialMode) -> Result<ResolvedConfig> {
    match mode {
        LlmCredentialMode::Hosted => resolve_hosted_api_config().await,
        LlmCredentialMode::Env => resolved_env_config(env_value)?.ok_or_else(|| {
            anyhow::anyhow!(
                "No env LLM config found. Set OPENAI_API_KEY or OPENROUTER_API_KEY, \
                 or omit `--use-env-llm` to use Enzyme hosted credits."
            )
        }),
    }
}

/// Resolve API configuration and construct an LLM client that preserves
/// provider metadata such as free-tier refresh behavior.
pub async fn resolve_llm_client() -> Result<(ResolvedConfig, LlmClient)> {
    let config = resolve_api_config().await?;
    let client = LlmClient::from_config(&config);
    Ok((config, client))
}

/// Resolve API configuration in an explicit credential mode and construct an
/// LLM client that preserves provider metadata such as free-tier behavior.
pub async fn resolve_llm_client_for_mode(
    mode: LlmCredentialMode,
) -> Result<(ResolvedConfig, LlmClient)> {
    let config = resolve_api_config_for_mode(mode).await?;
    let client = LlmClient::from_config(&config);
    Ok((config, client))
}

/// Backward-compatible: resolve just the API key.
pub async fn resolve_api_key() -> Result<String> {
    resolve_api_config().await.map(|c| c.api_key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let values: HashMap<String, String> = vars
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();
        move |key| values.get(key).cloned()
    }

    #[test]
    fn hosted_unavailable_message_preserves_specific_cause() {
        let cause = anyhow::anyhow!(
            "Insufficient credits (0.0 remaining). Visit app.enzyme.garden/plans to top up."
        );
        let msg = hosted_config_unavailable_message(&cause);
        assert!(msg.contains("Insufficient credits (0.0 remaining)"));
        assert!(msg.contains("--use-env-llm"));
    }

    #[test]
    fn openrouter_env_uses_openrouter_defaults() {
        let config = resolved_env_config(env(&[("OPENROUTER_API_KEY", "openrouter-key")]))
            .unwrap()
            .unwrap();

        assert_eq!(config.api_key, "openrouter-key");
        assert_eq!(config.base_url, DEFAULT_BASE_URL);
        assert_eq!(config.model, DEFAULT_MODEL);
        assert!(matches!(config.provider, ConfigProvider::Env));
    }

    #[test]
    fn openai_env_accepts_overrides() {
        let config = resolved_env_config(env(&[
            ("OPENAI_API_KEY", "openai-key"),
            ("OPENAI_BASE_URL", "https://api.openai.com/v1"),
            ("OPENAI_MODEL", "gpt-4o-mini"),
        ]))
        .unwrap()
        .unwrap();

        assert_eq!(config.api_key, "openai-key");
        assert_eq!(config.base_url, "https://api.openai.com/v1");
        assert_eq!(config.model, "gpt-4o-mini");
        assert!(matches!(config.provider, ConfigProvider::Env));
    }

    #[test]
    fn openai_env_uses_openai_defaults() {
        let config = resolved_env_config(env(&[("OPENAI_API_KEY", "openai-key")]))
            .unwrap()
            .unwrap();

        assert_eq!(config.api_key, "openai-key");
        assert_eq!(config.base_url, OPENAI_DEFAULT_BASE_URL);
        assert_eq!(config.model, OPENAI_DEFAULT_MODEL);
        assert!(matches!(config.provider, ConfigProvider::Env));
    }

    #[test]
    fn partial_env_config_does_not_block_fallback() {
        let config =
            resolved_env_config(env(&[("OPENAI_BASE_URL", "https://llm.example/v1")])).unwrap();

        assert!(config.is_none());
    }

    #[test]
    fn chat_completions_url_trims_trailing_slashes() {
        assert_eq!(
            chat_completions_url("https://api.openai.com/v1/").unwrap(),
            "https://api.openai.com/v1/chat/completions"
        );
        assert_eq!(
            chat_completions_url("https://api.anthropic.com/v1///").unwrap(),
            "https://api.anthropic.com/v1/chat/completions"
        );
    }

    #[test]
    fn chat_completions_url_preserves_base_path_and_drops_query() {
        assert_eq!(
            chat_completions_url("https://example.test/proxy/openai?debug=1#frag").unwrap(),
            "https://example.test/proxy/openai/chat/completions"
        );
    }

    #[test]
    fn openai_requests_use_max_completion_tokens() {
        let request = chat_completion_request(
            OPENAI_DEFAULT_BASE_URL,
            "gpt-5.4-nano",
            vec![Message::text("user", "hello")],
            GenerationParams {
                max_tokens: 123,
                ..GenerationParams::default()
            },
            None,
        );

        let json = serde_json::to_value(request).unwrap();
        assert_eq!(json["max_completion_tokens"], 123);
        assert!(json.get("max_tokens").is_none());
    }

    #[test]
    fn legacy_openai_requests_keep_max_tokens() {
        let request = chat_completion_request(
            OPENAI_DEFAULT_BASE_URL,
            "gpt-3.5-turbo",
            vec![Message::text("user", "hello")],
            GenerationParams {
                max_tokens: 123,
                ..GenerationParams::default()
            },
            None,
        );

        let json = serde_json::to_value(request).unwrap();
        assert_eq!(json["max_tokens"], 123);
        assert!(json.get("max_completion_tokens").is_none());
    }

    #[test]
    fn non_openai_requests_keep_max_tokens() {
        let request = chat_completion_request(
            DEFAULT_BASE_URL,
            DEFAULT_MODEL,
            vec![Message::text("user", "hello")],
            GenerationParams {
                max_tokens: 123,
                ..GenerationParams::default()
            },
            None,
        );

        let json = serde_json::to_value(request).unwrap();
        assert_eq!(json["max_tokens"], 123);
        assert!(json.get("max_completion_tokens").is_none());
    }

    #[test]
    fn compatible_anthropic_models_keep_temperature() {
        for model in ["claude-opus-4-6", "claude-sonnet-4-6", "claude-haiku-4-5"] {
            let request = chat_completion_request(
                "https://api.anthropic.com/v1",
                model,
                vec![Message::text("user", "hello")],
                GenerationParams {
                    max_tokens: 123,
                    temperature: 0.4,
                    top_p: 0.8,
                    response_format: Some(ResponseFormat::JsonObject),
                    ..GenerationParams::default()
                },
                None,
            );

            let json = serde_json::to_value(request).unwrap();
            assert!((json["temperature"].as_f64().unwrap() - 0.4).abs() < 1e-6);
            assert_eq!(json["max_tokens"], 123);
            assert!(json.get("top_p").is_none(), "model: {model}");
            assert!(json.get("response_format").is_none(), "model: {model}");
        }
    }

    #[test]
    fn current_anthropic_models_omit_sampling_parameters() {
        for model in [
            "claude-opus-4-7",
            "claude-opus-4-7-20260601",
            "claude-opus-4-8",
            "claude-opus-4-8-20260701",
            "claude-sonnet-5",
            "claude-sonnet-5-20260701",
            "claude-fable-5",
        ] {
            let request = chat_completion_request(
                "https://api.anthropic.com/v1",
                model,
                vec![Message::text("user", "hello")],
                GenerationParams {
                    temperature: 0.4,
                    top_p: 0.8,
                    top_k: Some(40),
                    ..GenerationParams::default()
                },
                None,
            );

            let json = serde_json::to_value(request).unwrap();
            assert!(json.get("temperature").is_none(), "model: {model}");
            assert!(json.get("top_p").is_none(), "model: {model}");
            assert!(json.get("top_k").is_none(), "model: {model}");
        }
    }

    #[test]
    fn openrouter_preserves_sampling_and_response_format() {
        let request = chat_completion_request(
            DEFAULT_BASE_URL,
            "anthropic/claude-opus-4-8",
            vec![Message::text("user", "hello")],
            GenerationParams {
                top_p: 0.8,
                top_k: Some(40),
                response_format: Some(ResponseFormat::JsonObject),
                ..GenerationParams::default()
            },
            None,
        );

        let json = serde_json::to_value(request).unwrap();
        assert!((json["temperature"].as_f64().unwrap() - 0.7).abs() < 1e-6);
        assert!((json["top_p"].as_f64().unwrap() - 0.8).abs() < 1e-6);
        assert_eq!(json["top_k"], 40);
        assert_eq!(json["response_format"]["type"], "json_object");
    }

    #[test]
    fn top_k_is_dropped_for_strict_openai_compatible_providers() {
        for base_url in [
            OPENAI_DEFAULT_BASE_URL,
            "https://api.anthropic.com/v1",
            "https://generativelanguage.googleapis.com/v1beta/openai/",
        ] {
            let request = chat_completion_request(
                base_url,
                "gpt-4o-mini",
                vec![Message::text("user", "hello")],
                GenerationParams {
                    top_k: Some(40),
                    ..GenerationParams::default()
                },
                None,
            );

            let json = serde_json::to_value(request).unwrap();
            assert!(json.get("top_k").is_none());
        }
    }
}
