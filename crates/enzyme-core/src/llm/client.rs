use std::collections::HashSet;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures::Stream;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::config::enzyme_config::{CatalystMode, EnzymeConfig};

const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1";
pub const DEFAULT_MODEL: &str = "google/gemini-3-flash-preview";
const OPENAI_DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
const OPENAI_DEFAULT_MODEL: &str = "gpt-4o-mini";
const FREE_CONFIG_URL: &str = "https://api.enzyme.garden/margins/free-config";
const FREE_CONFIG_BOOTSTRAP_HEADER: &str = "X-Margins-Bootstrap-Id";
const DEFAULT_API_CACHE_PATH: &str = "/tmp/enzyme-api-cache";

/// URL of the hosted free-config endpoint.
///
/// Overridable via `ENZYME_FREE_CONFIG_URL` so tests can point at a local
/// mock server; defaults to the production endpoint.
fn free_config_url() -> String {
    std::env::var("ENZYME_FREE_CONFIG_URL")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| FREE_CONFIG_URL.to_string())
}

/// Path of the on-disk free-tier config cache.
///
/// Overridable via `ENZYME_API_CACHE_PATH` so tests can run hermetically in
/// parallel; defaults to `/tmp/enzyme-api-cache`.
fn api_cache_path() -> std::path::PathBuf {
    std::env::var("ENZYME_API_CACHE_PATH")
        .ok()
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(DEFAULT_API_CACHE_PATH))
}

/// Pseudo-scheme marking a `ResolvedConfig` that runs in-process rather than
/// over HTTP. `local://<model-name>`.
const LOCAL_BASE_URL_SCHEME: &str = "local://";

/// Resolve a config that points the pipeline at the in-process engine, if one
/// is available. Always `None` without the `local-llm` feature.
///
/// Selection is controlled by the persisted catalyst mode. Auto prefers an
/// installed local model and otherwise uses hosted; local requires one;
/// hosted bypasses it. `ENZYME_LOCAL_ENGINE=0` is a legacy process opt-out.
fn configured_catalyst_mode() -> Result<CatalystMode> {
    Ok(EnzymeConfig::catalyst_llm_config()?.mode)
}

fn local_model_config(required: bool) -> Result<Option<ResolvedConfig>> {
    if env_value("ENZYME_LOCAL_ENGINE").is_some_and(|v| v == "0" || v.eq_ignore_ascii_case("false"))
    {
        if required {
            bail!(
                "local catalyst mode is enabled but ENZYME_LOCAL_ENGINE disables it; unset the variable or run `enzyme model disable`"
            );
        }
        return Ok(None);
    }
    match local_model_config_inner() {
        Some(config) => Ok(Some(config)),
        None if required => bail!(
            "local catalyst mode requires model '{}', but it is not installed; run `enzyme model install {}`",
            super::model_registry::selected_name(),
            super::model_registry::selected_name()
        ),
        None => Ok(None),
    }
}

#[cfg(not(feature = "local-llm"))]
fn local_model_config_inner() -> Option<ResolvedConfig> {
    None
}

#[cfg(feature = "local-llm")]
fn local_model_config_inner() -> Option<ResolvedConfig> {
    let (name, _path) = super::model_registry::active()?;
    Some(ResolvedConfig {
        api_key: String::new(),
        base_url: format!("{LOCAL_BASE_URL_SCHEME}{name}"),
        model: name,
        provider: ConfigProvider::Local,
    })
}

#[cfg(not(feature = "local-llm"))]
fn local_backend(base_url: &str) -> Result<Backend> {
    if base_url.starts_with(LOCAL_BASE_URL_SCHEME) {
        bail!("this Enzyme binary was built without local-model support");
    }
    Ok(Backend::Http)
}

/// Load (once per process) and bind the in-process engine for a `local://` URL.
#[cfg(feature = "local-llm")]
fn local_backend(base_url: &str) -> Result<Backend> {
    use std::sync::Arc;

    let Some(name) = base_url.strip_prefix(LOCAL_BASE_URL_SCHEME) else {
        return Ok(Backend::Http);
    };
    let path = super::model_registry::model_path(name)?;
    let engine = super::local_engine::LocalEngine::load(
        &path,
        name,
        super::model_registry::context_window(name),
    )
    .with_context(|| {
        format!(
            "failed to load local model '{name}'; run `enzyme model verify {name}` or select another provider with `enzyme model disable`"
        )
    })?;
    Ok(Backend::Local(Arc::new(engine)))
}

/// Translate an OpenAI `response_format` into a GBNF grammar for local
/// decoding. `json_object` has no schema to constrain, so it decodes freely.
#[cfg(feature = "local-llm")]
fn grammar_for(response_format: Option<&ResponseFormat>) -> Option<String> {
    match response_format? {
        ResponseFormat::JsonSchema { json_schema } => {
            match super::gbnf::json_schema_to_gbnf(&json_schema.schema) {
                Ok(grammar) => Some(grammar),
                Err(e) => {
                    warn!("could not convert response schema to GBNF, decoding unconstrained: {e:#}");
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

/// Typed error for an HTTP 402 from the hosted free-config endpoint: the
/// account's Enzyme credits are exhausted. Billing errors can only come from
/// billing endpoints (this fetch and `POST /v1/credits/deduct`), never from a
/// completions call — so this is one of the only places top-up guidance is
/// correct.
#[derive(Debug, thiserror::Error)]
#[error(
    "Insufficient Enzyme credits ({credits:.1} remaining) — top up at \
     app.enzyme.garden/plans or use --use-env-llm"
)]
pub struct EnzymeCreditsExhausted {
    pub credits: f64,
}

/// Where the resolved API configuration came from.
#[derive(Debug, Clone)]
pub enum ConfigProvider {
    Env,
    Local,
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
    /// Apply the persisted auto/local/hosted preference.
    Auto,
    /// Require the selected installed local model.
    Local,
    /// Use Enzyme hosted bootstrap/auth, bypassing installed local models.
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
    /// Margins leases currently omit expiry because they are persistent. Keep
    /// accepting an epoch-seconds expiry for forward compatibility with the
    /// broker's shared lease contract; the existing five-minute client cache
    /// remains the freshness authority.
    #[serde(default)]
    expires_at: Option<i64>,
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
    api_key: Arc<RwLock<String>>,
    base_url: String,
    /// Whether this client was created from the free-tier server fallback.
    /// When true, 401 errors trigger an automatic key refresh.
    free_tier: bool,
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
            backend: Backend::Http,
        }
    }

    pub fn with_base_url(api_key: &str, base_url: &str) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key: Arc::new(RwLock::new(api_key.to_string())),
            base_url: base_url.to_string(),
            free_tier: false,
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
            client: reqwest::Client::new(),
            api_key: Arc::new(RwLock::new(String::new())),
            base_url: format!("{LOCAL_BASE_URL_SCHEME}{model}"),
            free_tier: false,
            backend: Backend::Local(Arc::new(engine)),
        })
    }

    #[cfg(not(feature = "local-llm"))]
    pub fn from_local_model(_path: &std::path::Path, _model: &str) -> Result<Self> {
        bail!("this Enzyme build does not include local-model support")
    }

    /// Create a client from a resolved config, preserving free-tier status
    /// so that 401 errors can trigger automatic key refresh.
    ///
    /// A `local://<model>` base URL binds the in-process engine instead of the
    /// HTTP client; only [`resolve_api_config`] mints such a config, and only
    /// when the binary was built with `local-llm`.
    pub fn from_config(config: &ResolvedConfig) -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::new(),
            api_key: Arc::new(RwLock::new(config.api_key.clone())),
            base_url: config.base_url.clone(),
            free_tier: config.is_free_tier(),
            backend: local_backend(&config.base_url)?,
        })
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
                write_free_config_cache(&api_cache_path(), &cfg);
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
                self.send_chat_completion(&url, &api_key, &request_body)
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
                    .send_streaming_request(&url, &self.current_api_key(), &request_body)
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

/// Delete the cached free-tier config so the next resolve re-fetches.
fn invalidate_free_config_cache() {
    let _ = std::fs::remove_file(api_cache_path());
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

/// Fetch an anonymous, usage-limited API key from the Margins orchestrator.
async fn fetch_free_config() -> Result<ResolvedConfig> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .context("failed to build HTTP client")?;

    let free_config_url = free_config_url();
    debug!("Fetching free config from {}", free_config_url);

    // Hosted catalyst credentials are brokered independently from Enzyme
    // account login and credits. A stable installation id lets the Margins
    // orchestrator reuse the same usage-limited lease.
    let enzyme_home = crate::config::paths::EnzymePaths::enzyme_home()
        .context("failed to resolve enzyme home for bootstrap identity")?;
    let bootstrap_id = load_or_create_bootstrap_id(&enzyme_home.join("bootstrap.json"))?;

    let resp = client
        .get(&free_config_url)
        .header(FREE_CONFIG_BOOTSTRAP_HEADER, bootstrap_id)
        .send()
        .await
        .context("failed to reach Margins orchestrator for free config")?;

    if resp.status().as_u16() == 402 {
        let body: serde_json::Value = resp.json().await.unwrap_or_default();
        let credits = body["credits"].as_f64().unwrap_or(0.0);
        // Typed so callers (e.g. `init`'s skip diagnostic) can distinguish a
        // reachable server's credit refusal from a genuine network failure.
        return Err(EnzymeCreditsExhausted { credits }.into());
    }

    let resp: FreeConfigResponse = resp
        .error_for_status()
        .context("Margins orchestrator returned an error for free config")?
        .json()
        .await
        .context("failed to parse free config response")?;

    // The orchestrator may add an epoch-seconds expiry in the future. It is
    // intentionally not persisted yet: the existing cache TTL and 401 remint
    // path remain unchanged by this endpoint-only migration.
    let _ = resp.expires_at;

    info!(
        "Resolved API config from Margins orchestrator (model={})",
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
    let cache_path = api_cache_path();
    let cache_path = cache_path.as_path();
    if cache_path.exists() {
        // The cache file may be shared with a co-located (possibly newer) CLI.
        // A file that cannot be read or parsed is treated as ABSENT — fall
        // through to a fresh fetch, which overwrites the unusable file.
        // Propagating the parse error would leave no recovery path even though
        // the server is reachable and can mint a usable lease.
        let cached: Option<CachedFreeConfig> = std::fs::read_to_string(cache_path)
            .ok()
            .and_then(|contents| serde_json::from_str(&contents).ok());

        match cached {
            Some(cached) => {
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
            None => {
                warn!(
                    "Unusable API cache at {}; discarding and re-fetching",
                    cache_path.display()
                );
            }
        }
    }

    // Fetch a usage-limited lease from the Margins orchestrator.
    match fetch_free_config().await {
        Ok(cfg) => {
            write_free_config_cache(cache_path, &cfg);
            Ok(cfg)
        }
        Err(e) => {
            // `context` rather than `bail!` so typed causes (e.g.
            // `EnzymeCreditsExhausted`) remain downcastable by callers.
            let msg = hosted_config_unavailable_message(&e);
            Err(e.context(msg))
        }
    }
}

/// Keep the specific hosted failure (capacity exhausted, request refused, or
/// server unreachable) visible in the single-line error the CLI prints,
/// since most callers format errors with `{}` and would otherwise show only
/// a generic message.
fn hosted_config_unavailable_message(cause: &anyhow::Error) -> String {
    format!(
        "{cause:#}. No hosted LLM config is available from the Margins orchestrator — \
         check network access and try again, or pass `--use-env-llm` to intentionally \
         use OPENAI_API_KEY or OPENROUTER_API_KEY."
    )
}

/// Resolve the API key, base URL, and model for LLM requests.
///
/// Checks (in order):
/// 0. the in-process local engine, when built with `local-llm`, a model is
///    installed, and no explicit `*_BASE_URL` override is set
/// 1. `OPENAI_API_KEY` + optional `OPENAI_BASE_URL` / `OPENAI_MODEL`
/// 2. `OPENROUTER_API_KEY` + optional `OPENROUTER_BASE_URL` / `OPENROUTER_MODEL`
/// 3. the on-disk API cache (`ENZYME_API_CACHE_PATH`, default `/tmp/enzyme-api-cache`)
/// 4. Margins orchestrator free-config endpoint (hosted fallback)
pub async fn resolve_api_config() -> Result<ResolvedConfig> {
    let mode = match configured_catalyst_mode()? {
        CatalystMode::Auto => LlmCredentialMode::Auto,
        CatalystMode::Local => LlmCredentialMode::Local,
        CatalystMode::Hosted => LlmCredentialMode::Hosted,
    };
    if !matches!(mode, LlmCredentialMode::Auto) {
        return resolve_api_config_for_mode(mode).await;
    }
    if let Some(config) = local_model_config(false)? {
        return Ok(config);
    }
    if let Some(config) = resolved_env_config(env_value)? {
        return Ok(config);
    }
    resolve_hosted_api_config().await
}

/// Resolve API configuration in an explicit credential mode.
pub async fn resolve_api_config_for_mode(mode: LlmCredentialMode) -> Result<ResolvedConfig> {
    match mode {
        LlmCredentialMode::Auto => match configured_catalyst_mode()? {
            CatalystMode::Auto => match local_model_config(false)? {
                // An installed local model means the machine can generate offline;
                // prefer it over spending hosted credits.
                Some(config) => Ok(config),
                None => resolve_hosted_api_config().await,
            },
            CatalystMode::Local => local_model_config(true)?
                .ok_or_else(|| anyhow::anyhow!("selected local catalyst model is unavailable")),
            CatalystMode::Hosted => resolve_hosted_api_config().await,
        },
        LlmCredentialMode::Local => local_model_config(true)?
            .ok_or_else(|| anyhow::anyhow!("selected local catalyst model is unavailable")),
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
    let client = LlmClient::from_config(&config)?;
    Ok((config, client))
}

/// Synchronously load and construct the preferred in-process local client.
///
/// This is intentionally separate from async provider resolution so callers
/// can run expensive llama.cpp/Metal startup on a blocking worker while doing
/// unrelated work. It returns `None` when local inference is unavailable,
/// explicitly overridden, or disabled; it never consults hosted credentials.
pub fn preload_preferred_local_llm_client() -> Result<Option<(ResolvedConfig, LlmClient)>> {
    let mode = configured_catalyst_mode()?;
    if matches!(mode, CatalystMode::Hosted) {
        return Ok(None);
    }
    let config = match local_model_config(matches!(mode, CatalystMode::Local))? {
        Some(config) => config,
        None => return Ok(None),
    };
    let started = std::time::Instant::now();
    let client = LlmClient::from_config(&config)?;
    info!(
        model = config.model,
        duration_ms = started.elapsed().as_millis(),
        "local LLM preload complete"
    );
    Ok(Some((config, client)))
}

/// Resolve API configuration in an explicit credential mode and construct an
/// LLM client that preserves provider metadata such as free-tier behavior.
pub async fn resolve_llm_client_for_mode(
    mode: LlmCredentialMode,
) -> Result<(ResolvedConfig, LlmClient)> {
    let config = resolve_api_config_for_mode(mode).await?;
    let client = LlmClient::from_config(&config)?;
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

    /// The typed credit-exhaustion error must stay downcastable through the
    /// context wrapper `resolve_hosted_api_config` adds, since `init` branches
    /// on it to pick the skip diagnostic.
    #[test]
    fn credits_exhausted_survives_context_wrapping() {
        let err = anyhow::Error::from(EnzymeCreditsExhausted { credits: 0.0 });
        let wrapped = err.context(hosted_config_unavailable_message(&anyhow::anyhow!("x")));
        assert!(wrapped.downcast_ref::<EnzymeCreditsExhausted>().is_some());
        assert!(
            EnzymeCreditsExhausted { credits: 0.0 }
                .to_string()
                .contains("app.enzyme.garden/plans")
        );
    }

    #[test]
    fn hosted_unavailable_message_preserves_specific_cause() {
        let cause = anyhow::anyhow!(
            "Insufficient credits (0.0 remaining). Visit app.enzyme.garden/plans to top up."
        );
        let msg = hosted_config_unavailable_message(&cause);
        assert!(msg.contains("Insufficient credits (0.0 remaining)"));
        assert!(msg.contains("Margins orchestrator"));
        assert!(msg.contains("--use-env-llm"));
    }

    #[test]
    fn margins_free_config_contract_accepts_missing_or_epoch_expiry() {
        let persistent: FreeConfigResponse = serde_json::from_str(
            r#"{"api_key":"leased","base_url":"https://openrouter.ai/api/v1","model":"fixture"}"#,
        )
        .unwrap();
        assert_eq!(persistent.expires_at, None);

        let expiring: FreeConfigResponse = serde_json::from_str(
            r#"{"api_key":"leased","base_url":"https://openrouter.ai/api/v1","model":"fixture","expires_at":1800000000}"#,
        )
        .unwrap();
        assert_eq!(expiring.expires_at, Some(1_800_000_000));
        assert_eq!(FREE_CONFIG_BOOTSTRAP_HEADER, "X-Margins-Bootstrap-Id");
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
        assert!(!should_retry_without_sampling(&sampling_free, &sampling_400));

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
