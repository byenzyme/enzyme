use std::collections::HashSet;
use std::pin::Pin;
#[cfg(feature = "local-llm")]
use std::sync::Arc;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures::Stream;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

#[cfg(feature = "hosted-credentials")]
mod credentials;
#[cfg(feature = "hosted-credentials")]
pub use credentials::*;

const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1";
pub const DEFAULT_MODEL: &str = "google/gemini-3-flash-preview";
#[cfg(test)]
const OPENAI_DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
const LLM_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// Pseudo-scheme marking a `ResolvedConfig` that runs in-process rather than
/// over HTTP. `local://<model-name>`.
const LOCAL_BASE_URL_SCHEME: &str = "local://";

/// Translate an OpenAI `response_format` into a GBNF grammar for local
/// decoding. `json_object` has no schema to constrain, so it decodes freely.
#[cfg(feature = "local-llm")]
fn grammar_for(response_format: Option<&ResponseFormat>) -> Option<String> {
    match response_format? {
        ResponseFormat::JsonSchema { json_schema } => {
            match super::gbnf::json_schema_to_gbnf(&json_schema.schema) {
                Ok(grammar) => Some(grammar),
                Err(e) => {
                    warn!(
                        "could not convert response schema to GBNF, decoding unconstrained: {e:#}"
                    );
                    None
                }
            }
        }
        ResponseFormat::JsonObject => None,
    }
}

/// Typed error for a non-success HTTP response from an LLM completions
/// endpoint. Downcastable through `anyhow` so callers can classify on the
/// actual HTTP status (e.g. the catalyst pipeline's 402 circuit breaker)
/// instead of substring-matching the provider body. The `Display` format stays
/// close to the historical `bail!` string so log output remains familiar.
#[derive(Debug, thiserror::Error)]
#[error("LLM API returned status {status}: {body}")]
pub struct LlmApiError {
    pub status: u16,
    pub body: String,
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

/// Where completions actually run.
///
/// `Local` only exists with the `local-llm` feature; without it the enum has a
/// single variant and every code path below compiles to the HTTP client
/// unchanged.
#[derive(Clone)]
enum Backend {
    Http,
    #[cfg(feature = "local-llm")]
    Local(Arc<super::local_engine::LocalEngine>),
}

#[derive(Clone)]
pub struct LlmClient {
    client: reqwest::Client,
    api_key: String,
    base_url: String,
    /// Only read on the `local-llm` paths; always `Http` otherwise.
    #[cfg_attr(not(feature = "local-llm"), allow(dead_code))]
    backend: Backend,
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

impl ChatCompletionRequest {
    /// Whether any sampling parameter would be serialized on this request.
    fn has_sampling_parameters(&self) -> bool {
        self.temperature.is_some() || self.top_p.is_some() || self.top_k.is_some()
    }

    /// Remove all sampling parameters (temperature, top_p, top_k) so the
    /// rebuilt request cannot trigger a sampling-parameter rejection again.
    fn strip_sampling_parameters(&mut self) {
        self.temperature = None;
        self.top_p = None;
        self.top_k = None;
    }
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
        if url.starts_with(LOCAL_BASE_URL_SCHEME) {
            Self::Local
        } else if url.contains("openrouter.ai") {
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
        "claude-opus-5",
        "claude-sonnet-5",
        "claude-haiku-5",
        "claude-fable-5",
        "claude-mythos-5",
    ]
    .iter()
    .any(|family| {
        model == *family
            || model
                .strip_prefix(family)
                .is_some_and(|suffix| suffix.starts_with('-'))
    })
}

/// Process-wide memo of `(base_url, model)` pairs observed to reject sampling
/// parameters at runtime (HTTP 400 mentioning temperature/top_p/top_k).
///
/// The hardcoded `anthropic_model_rejects_sampling_parameters` list above is an
/// optimization that avoids a wasted request for known models; this memo is the
/// reactive fallback that keeps generation working when a brand-new model id is
/// missing from the list. Once a rejection is observed, every subsequent
/// request in the same process omits sampling parameters up front — one wasted
/// request per run, not per call.
fn sampling_rejections() -> &'static Mutex<HashSet<(String, String)>> {
    static REJECTIONS: OnceLock<Mutex<HashSet<(String, String)>>> = OnceLock::new();
    REJECTIONS.get_or_init(|| Mutex::new(HashSet::new()))
}

fn sampling_rejection_memoized(base_url: &str, model: &str) -> bool {
    sampling_rejections()
        .lock()
        .unwrap()
        .contains(&(base_url.to_string(), model.to_string()))
}

fn memoize_sampling_rejection(base_url: &str, model: &str) {
    sampling_rejections()
        .lock()
        .unwrap()
        .insert((base_url.to_string(), model.to_string()));
}

/// Whether an LLM API error body indicates the provider rejected a sampling
/// parameter (temperature/top_p/top_k).
///
/// Deliberately conservative but format-tolerant: the status must be exactly
/// 400 (a 402 is billing, a 401 is auth — never a parameter rejection), and
/// the body must mention one of the sampling parameter names. Provider
/// phrasing varies ("`temperature` is deprecated for this model", "top_p is
/// not supported", ...) so we do not pin the exact message.
fn is_sampling_parameter_rejection(status: u16, body: &str) -> bool {
    if status != 400 {
        return false;
    }
    let body = body.to_lowercase();
    ["temperature", "top_p", "top_k"]
        .iter()
        .any(|param| body.contains(param))
}

/// Whether a failed completion should be retried once with all sampling
/// parameters omitted. Requires that the request actually INCLUDED at least
/// one sampling parameter — a sampling-free request that 400s propagates
/// normally, so the retry can never loop.
fn should_retry_without_sampling(request: &ChatCompletionRequest, error: &anyhow::Error) -> bool {
    request.has_sampling_parameters()
        && error
            .downcast_ref::<LlmApiError>()
            .is_some_and(|api_err| is_sampling_parameter_rejection(api_err.status, &api_err.body))
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

    let mut request = ChatCompletionRequest {
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
    };

    // A previous request in this process observed a sampling-parameter
    // rejection from this (base_url, model) — omit them up front.
    if request.has_sampling_parameters() && sampling_rejection_memoized(base_url, model) {
        request.strip_sampling_parameters();
    }

    request
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

fn completion_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(LLM_REQUEST_TIMEOUT)
        .build()
        .expect("valid completion HTTP client configuration")
}

impl LlmClient {
    pub fn new(api_key: &str) -> Self {
        Self {
            client: completion_http_client(),
            api_key: api_key.to_string(),
            base_url: DEFAULT_BASE_URL.to_string(),
            backend: Backend::Http,
        }
    }

    pub fn with_base_url(api_key: &str, base_url: &str) -> Self {
        Self {
            client: completion_http_client(),
            api_key: api_key.to_string(),
            base_url: base_url.to_string(),
            backend: Backend::Http,
        }
    }

    /// Bind the in-process engine to one explicit GGUF file.
    ///
    /// This bypasses config, environment, registry selection, and every hosted
    /// resolver. It is the local-model entry point for embedded search users.
    #[cfg(feature = "local-llm")]
    pub fn from_local_model(path: &std::path::Path, model: &str) -> Result<Self> {
        let engine = super::local_engine::LocalEngine::load(
            path,
            model,
            super::model_registry::context_window(model),
        )
        .with_context(|| {
            format!(
                "failed to load local model '{}' from {}",
                model,
                path.display()
            )
        })?;
        Ok(Self {
            client: completion_http_client(),
            api_key: String::new(),
            base_url: format!("{LOCAL_BASE_URL_SCHEME}{model}"),
            backend: Backend::Local(Arc::new(engine)),
        })
    }

    #[cfg(not(feature = "local-llm"))]
    pub fn from_local_model(_path: &std::path::Path, _model: &str) -> Result<Self> {
        bail!("this Enzyme build does not include local-model support")
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
        if self.base_url.starts_with(LOCAL_BASE_URL_SCHEME) {
            return true;
        }
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
            .header("HTTP-Referer", "https://github.com/useenzyme/enzyme-rust")
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
            return Err(LlmApiError {
                status: status.as_u16(),
                body: error_body,
            }
            .into());
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
            .header("HTTP-Referer", "https://github.com/useenzyme/enzyme-rust")
            .header("X-Title", "Enzyme")
            .json(request_body)
            .send()
            .await
            .context("Failed to send streaming request to LLM API")
    }

    /// Run a completion on the in-process engine (blocking work moved off the
    /// async runtime).
    #[cfg(feature = "local-llm")]
    async fn local_chat_completion(
        engine: Arc<super::local_engine::LocalEngine>,
        messages: Vec<Message>,
        params: GenerationParams,
    ) -> Result<String> {
        let grammar = grammar_for(params.response_format.as_ref());
        tokio::task::spawn_blocking(move || {
            engine.complete(
                &messages,
                grammar.as_deref(),
                params.max_tokens,
                params.temperature,
            )
        })
        .await
        .context("local generation task panicked")?
    }

    pub async fn chat_completion(
        &self,
        model: &str,
        messages: Vec<Message>,
        params: GenerationParams,
    ) -> Result<String> {
        #[cfg(feature = "local-llm")]
        if let Backend::Local(engine) = &self.backend {
            return Self::local_chat_completion(Arc::clone(engine), messages, params).await;
        }

        let url = chat_completions_url(&self.base_url)?;

        let mut request_body =
            chat_completion_request(&self.base_url, model, messages, params, None);

        debug!(
            "Sending chat completion request to {} with model {}",
            url, model
        );

        let api_key = &self.api_key;
        match self
            .send_chat_completion(&url, api_key, &request_body)
            .await
        {
            Ok(content) => Ok(content),
            Err(e) if should_retry_without_sampling(&request_body, &e) => {
                // The provider rejected a sampling parameter this model no
                // longer accepts (a model id missing from the hardcoded omit
                // list). Retry once with all sampling parameters removed and
                // remember the outcome so subsequent calls omit them up front.
                warn!(
                    model,
                    "provider rejected sampling parameters; retrying without them: {e}"
                );
                memoize_sampling_rejection(&self.base_url, model);
                request_body.strip_sampling_parameters();
                self.send_chat_completion(&url, api_key, &request_body)
                    .await
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
        // The in-process engine has no token callback; emit the finished
        // completion as a single chunk. Partial-catalyst parsing still works,
        // it just fires once at the end.
        #[cfg(feature = "local-llm")]
        if let Backend::Local(engine) = &self.backend {
            let text = Self::local_chat_completion(Arc::clone(engine), messages, params).await?;
            return Ok(Box::pin(futures::stream::once(async move { Ok(text) })));
        }

        let url = chat_completions_url(&self.base_url)?;

        let mut request_body =
            chat_completion_request(&self.base_url, model, messages, params, Some(true));

        debug!(
            "Sending streaming chat completion request to {} with model {}",
            url, model
        );

        let api_key = &self.api_key;
        let mut response = self
            .send_streaming_request(&url, api_key, &request_body)
            .await?;

        let status = response.status();
        if !status.is_success() {
            let error_body = response
                .text()
                .await
                .unwrap_or_else(|_| "Failed to read error body".to_string());
            warn!("LLM API returned error status {}: {}", status, error_body);

            if request_body.has_sampling_parameters()
                && is_sampling_parameter_rejection(status.as_u16(), &error_body)
            {
                // Same reactive fallback as the non-streaming path: rebuild
                // the request with all sampling parameters omitted, retry
                // once, and memoize so subsequent calls omit them up front.
                warn!(
                    model,
                    "provider rejected sampling parameters; retrying stream without them"
                );
                memoize_sampling_rejection(&self.base_url, model);
                request_body.strip_sampling_parameters();
                response = self
                    .send_streaming_request(&url, &self.api_key, &request_body)
                    .await?;

                let retry_status = response.status();
                if !retry_status.is_success() {
                    let retry_body = response
                        .text()
                        .await
                        .unwrap_or_else(|_| "Failed to read error body".to_string());
                    warn!(
                        "LLM API returned error status {} on sampling-free retry: {}",
                        retry_status, retry_body
                    );
                    return Err(LlmApiError {
                        status: retry_status.as_u16(),
                        body: retry_body,
                    }
                    .into());
                }
            } else {
                return Err(LlmApiError {
                    status: status.as_u16(),
                    body: error_body,
                }
                .into());
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins the `LlmApiError` Display prefix. Any consumer that (as a
    /// fallback) matches the stringified error must agree with this format;
    /// the format is owned here and nowhere else.
    #[test]
    fn llm_api_error_display_format_is_stable() {
        let err = LlmApiError {
            status: 402,
            body: r#"{"error":{"message":"Insufficient credits"}}"#.to_string(),
        };
        assert_eq!(
            err.to_string(),
            r#"LLM API returned status 402: {"error":{"message":"Insufficient credits"}}"#
        );
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
            "claude-opus-5",
            "claude-opus-5-20260801",
            "claude-haiku-5",
            "claude-fable-5",
            "claude-mythos-5",
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
    fn sampling_rejection_detection_accepts_provider_phrasing_variants() {
        // Anthropic's exact wording varies by model generation; any 400 that
        // mentions a sampling parameter name qualifies.
        for body in [
            r#"{"error":{"type":"invalid_request_error","message":"temperature is deprecated for this model"}}"#,
            r#"{"error":{"type":"invalid_request_error","message":"`top_p` is not supported by this model"}}"#,
            r#"{"error":{"type":"invalid_request_error","message":"top_k may not be set"}}"#,
            "Temperature is not supported on this endpoint",
        ] {
            assert!(is_sampling_parameter_rejection(400, body), "body: {body}");
        }
    }

    #[test]
    fn sampling_rejection_detection_rejects_non_matches() {
        // 402 is billing, never a parameter rejection — even if the body
        // happens to mention temperature.
        assert!(!is_sampling_parameter_rejection(
            402,
            r#"{"error":{"message":"temperature is deprecated for this model"}}"#
        ));
        // A 400 about a different parameter must propagate unchanged.
        assert!(!is_sampling_parameter_rejection(
            400,
            r#"{"error":{"type":"invalid_request_error","message":"max_tokens: value too large"}}"#
        ));
        assert!(!is_sampling_parameter_rejection(400, "malformed JSON body"));
        // 5xx errors are transient server failures, not parameter rejections.
        assert!(!is_sampling_parameter_rejection(
            500,
            "temperature service unavailable"
        ));
    }

    #[test]
    fn strip_sampling_parameters_removes_all_three_and_nothing_else() {
        let mut request = chat_completion_request(
            DEFAULT_BASE_URL,
            "anthropic/claude-opus-4-8",
            vec![Message::text("user", "hello")],
            GenerationParams {
                max_tokens: 123,
                top_k: Some(40),
                response_format: Some(ResponseFormat::JsonObject),
                ..GenerationParams::default()
            },
            None,
        );
        assert!(request.has_sampling_parameters());

        request.strip_sampling_parameters();
        assert!(!request.has_sampling_parameters());

        let json = serde_json::to_value(request).unwrap();
        assert!(json.get("temperature").is_none());
        assert!(json.get("top_p").is_none());
        assert!(json.get("top_k").is_none());
        // Everything else survives the rebuild.
        assert_eq!(json["max_tokens"], 123);
        assert_eq!(json["response_format"]["type"], "json_object");
        assert_eq!(json["messages"][0]["content"], "hello");
    }

    #[test]
    fn retry_eligibility_requires_sampling_params_and_typed_400() {
        let with_sampling = chat_completion_request(
            DEFAULT_BASE_URL,
            "anthropic/claude-next-9",
            vec![Message::text("user", "hello")],
            GenerationParams::default(),
            None,
        );
        let sampling_400 = anyhow::Error::from(LlmApiError {
            status: 400,
            body: r#"{"error":{"type":"invalid_request_error","message":"temperature is deprecated for this model"}}"#.to_string(),
        });
        assert!(should_retry_without_sampling(&with_sampling, &sampling_400));

        // A request that already omitted all sampling params must never be
        // retried — this is the no-infinite-retry guarantee.
        let mut sampling_free = chat_completion_request(
            DEFAULT_BASE_URL,
            "anthropic/claude-next-9",
            vec![Message::text("user", "hello")],
            GenerationParams::default(),
            None,
        );
        sampling_free.strip_sampling_parameters();
        assert!(!should_retry_without_sampling(
            &sampling_free,
            &sampling_400
        ));

        // Untyped errors (transport failures etc.) are never retried here.
        let untyped = anyhow::anyhow!("temperature 400 mentioned in a non-API error");
        assert!(!should_retry_without_sampling(&with_sampling, &untyped));

        // Typed non-400s are never retried here.
        let billing_402 = anyhow::Error::from(LlmApiError {
            status: 402,
            body: "temperature aside, you are out of credits".to_string(),
        });
        assert!(!should_retry_without_sampling(&with_sampling, &billing_402));
    }

    /// Once a rejection is memoized for a (base_url, model) pair, requests
    /// built for that pair omit sampling parameters up front, while other
    /// models on the same base URL are unaffected.
    ///
    /// Uses a base URL unique to this test: the memo is process-global and
    /// tests run in parallel.
    #[test]
    fn memoized_rejection_strips_sampling_parameters_up_front() {
        let base_url = "https://memo-isolated.test/v1";
        let params = || GenerationParams {
            top_k: Some(40),
            ..GenerationParams::default()
        };

        // Before memoization: sampling params are sent (Unknown provider).
        let before = chat_completion_request(
            base_url,
            "claude-next-9",
            vec![Message::text("user", "hello")],
            params(),
            None,
        );
        let json = serde_json::to_value(before).unwrap();
        assert!(json.get("temperature").is_some());
        assert!(json.get("top_p").is_some());
        assert!(json.get("top_k").is_some());

        memoize_sampling_rejection(base_url, "claude-next-9");
        assert!(sampling_rejection_memoized(base_url, "claude-next-9"));

        // After memoization: all sampling params omitted up front.
        let after = chat_completion_request(
            base_url,
            "claude-next-9",
            vec![Message::text("user", "hello")],
            params(),
            None,
        );
        let json = serde_json::to_value(after).unwrap();
        assert!(json.get("temperature").is_none());
        assert!(json.get("top_p").is_none());
        assert!(json.get("top_k").is_none());
        assert_eq!(json["max_tokens"], 8192);

        // A different model on the same base URL is unaffected.
        let other = chat_completion_request(
            base_url,
            "some-other-model",
            vec![Message::text("user", "hello")],
            params(),
            None,
        );
        let json = serde_json::to_value(other).unwrap();
        assert!(json.get("temperature").is_some());
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
