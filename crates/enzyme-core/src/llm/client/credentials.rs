//! CLI-only credential, account, and hosted free-tier resolution.
//!
//! This module is compiled only with `hosted-credentials`. Embedded search
//! callers construct [`LlmClient`] or [`super::super::Generator`] explicitly,
//! so their default build never includes lease minting, bootstrap identity, or
//! credential-cache access.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::{debug, info, warn};

use super::{
    Backend, DEFAULT_BASE_URL, DEFAULT_MODEL, LOCAL_BASE_URL_SCHEME, LlmClient,
    completion_http_client,
};
use crate::config::enzyme_config::{CatalystMode, EnzymeConfig};

const OPENAI_DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
const OPENAI_DEFAULT_MODEL: &str = "gpt-4o-mini";
const FREE_CONFIG_URL: &str = "https://api.enzyme.garden/llm/free-config";
const FREE_CONFIG_BOOTSTRAP_HEADER: &str = "X-Enzyme-Bootstrap-Id";
const DEFAULT_API_CACHE_PATH: &str = "/tmp/enzyme-api-cache";
const FREE_CONFIG_CACHE_TTL_SECS: u64 = 300;
const FREE_CONFIG_CACHE_PROFILE_VERSION: &str = "enzyme-llm-v1";

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

/// Typed error for an HTTP 402 from the hosted free-config endpoint.
#[derive(Debug, thiserror::Error)]
#[error(
    "Insufficient Enzyme credits ({credits:.1} remaining) — top up at \
     app.enzyme.garden/plans or use --use-env-llm"
)]
pub struct EnzymeCreditsExhausted {
    pub credits: f64,
}

/// Typed refusal when Enzyme's shared hosted capacity is temporarily unavailable.
#[derive(Debug, thiserror::Error)]
#[error(
    "Enzyme hosted LLM capacity is temporarily unavailable — try again later or use --use-env-llm"
)]
pub struct EnzymeHostedCapacityUnavailable;

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
    pub fn selection_model(&self) -> &str {
        match self.provider {
            ConfigProvider::ServerFallback => DEFAULT_MODEL,
            _ => &self.model,
        }
    }

    pub fn is_free_tier(&self) -> bool {
        matches!(self.provider, ConfigProvider::ServerFallback)
    }
}

#[derive(Deserialize)]
struct FreeConfigResponse {
    api_key: String,
    base_url: String,
    model: String,
    #[serde(default)]
    expires_at: Option<i64>,
}

#[derive(Deserialize, Serialize)]
struct CachedFreeConfig {
    api_key: String,
    base_url: String,
    model: String,
    #[serde(default)]
    cached_at: u64,
    profile: String,
}

#[derive(Deserialize, Serialize)]
struct BootstrapIdentity {
    bootstrap_id: String,
}

struct HostedIdentity {
    authorization: Option<String>,
    bootstrap_id: Option<String>,
    cache_profile: String,
}

struct FetchedFreeConfig {
    config: ResolvedConfig,
    cache_profile: String,
}

impl FetchedFreeConfig {
    fn new(config: ResolvedConfig, identity: &HostedIdentity) -> Self {
        Self {
            config,
            cache_profile: identity.cache_profile.clone(),
        }
    }
}

#[derive(Default, Deserialize)]
struct FreeConfigErrorResponse {
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    credits: Option<f64>,
}

fn free_config_url() -> String {
    std::env::var("ENZYME_FREE_CONFIG_URL")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| FREE_CONFIG_URL.to_string())
}

fn api_cache_path() -> std::path::PathBuf {
    std::env::var("ENZYME_API_CACHE_PATH")
        .ok()
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(DEFAULT_API_CACHE_PATH))
}

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
            super::super::model_registry::selected_name(),
            super::super::model_registry::selected_name()
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
    let (name, _path) = super::super::model_registry::active()?;
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

#[cfg(feature = "local-llm")]
fn local_backend(base_url: &str) -> Result<Backend> {
    let Some(name) = base_url.strip_prefix(LOCAL_BASE_URL_SCHEME) else {
        return Ok(Backend::Http);
    };
    let path = super::super::model_registry::model_path(name)?;
    let engine = super::super::local_engine::LocalEngine::load(
        &path,
        name,
        super::super::model_registry::context_window(name),
    )
    .with_context(|| {
        format!(
            "failed to load local model '{name}'; run `enzyme model verify {name}` or select another provider with `enzyme model disable`"
        )
    })?;
    Ok(Backend::Local(Arc::new(engine)))
}

impl LlmClient {
    /// Create a client from a resolved config, preserving free-tier refresh.
    pub fn from_config(config: &ResolvedConfig) -> Result<Self> {
        Ok(Self {
            client: completion_http_client(),
            api_key: Arc::new(RwLock::new(config.api_key.clone())),
            base_url: config.base_url.clone(),
            free_tier: config.is_free_tier(),
            backend: local_backend(&config.base_url)?,
        })
    }

    pub(super) async fn try_refresh_key(&self) -> bool {
        if !self.free_tier {
            return false;
        }
        info!("free-tier key returned 401, refreshing from server");
        let identity = match hosted_identity() {
            Ok(identity) => identity,
            Err(e) => {
                warn!("failed to resolve hosted identity for key refresh: {e}");
                return false;
            }
        };
        invalidate_hosted_llm_config_cache();
        match fetch_free_config(&identity).await {
            Ok(fetched) => {
                *self.api_key.write().unwrap() = fetched.config.api_key.clone();
                write_free_config_cache(&api_cache_path(), &fetched.config, &fetched.cache_profile);
                true
            }
            Err(e) => {
                warn!("failed to refresh free-tier key: {e}");
                false
            }
        }
    }
}

/// Remove the cached hosted lease, for example after logout or a lease 401.
pub fn invalidate_hosted_llm_config_cache() {
    let _ = std::fs::remove_file(api_cache_path());
}

fn write_free_config_cache(path: &std::path::Path, cfg: &ResolvedConfig, profile: &str) {
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let cached = CachedFreeConfig {
        api_key: cfg.api_key.clone(),
        base_url: cfg.base_url.clone(),
        model: cfg.model.clone(),
        cached_at: now_secs,
        profile: profile.to_string(),
    };
    if let Ok(json) = serde_json::to_string(&cached) {
        let _ = std::fs::write(path, json);
    }
}

fn identity_cache_profile(kind: &str, value: &str) -> String {
    format!(
        "{FREE_CONFIG_CACHE_PROFILE_VERSION}:{kind}:{:x}",
        Sha256::digest(value.as_bytes())
    )
}

fn hosted_identity() -> Result<HostedIdentity> {
    let enzyme_home = crate::config::paths::EnzymePaths::enzyme_home()
        .context("failed to resolve enzyme home for hosted identity")?;
    let auth_path = enzyme_home.join("auth.json");
    if let Ok(token) = crate::config::auth::AuthToken::load(&auth_path) {
        let account_identity = if token.user_id.trim().is_empty() {
            token.api_key.as_str()
        } else {
            token.user_id.as_str()
        };
        return Ok(HostedIdentity {
            authorization: Some(format!("Bearer {}", token.api_key)),
            bootstrap_id: None,
            cache_profile: identity_cache_profile("account", account_identity),
        });
    }

    let bootstrap_id = load_or_create_bootstrap_id(&enzyme_home.join("bootstrap.json"))?;
    Ok(HostedIdentity {
        authorization: None,
        cache_profile: identity_cache_profile("bootstrap", &bootstrap_id),
        bootstrap_id: Some(bootstrap_id),
    })
}

fn classify_free_config_402(body: FreeConfigErrorResponse) -> anyhow::Error {
    match body.reason.as_deref() {
        Some("account_credits") => EnzymeCreditsExhausted {
            credits: body.credits.unwrap_or(0.0),
        }
        .into(),
        Some("pool_exhausted") => EnzymeHostedCapacityUnavailable.into(),
        // Older Enzyme servers returned only `credits` on account refusals.
        None if body.credits.is_some() => EnzymeCreditsExhausted {
            credits: body.credits.unwrap_or(0.0),
        }
        .into(),
        _ => EnzymeHostedCapacityUnavailable.into(),
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

/// Fetch an account-selected or anonymous bootstrap config from Enzyme.
async fn fetch_free_config(identity: &HostedIdentity) -> Result<FetchedFreeConfig> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .context("failed to build HTTP client")?;

    let free_config_url = free_config_url();
    debug!("Fetching free config from {}", free_config_url);

    let mut request = client.get(&free_config_url);
    if let Some(authorization) = &identity.authorization {
        request = request.header("Authorization", authorization);
    } else if let Some(bootstrap_id) = &identity.bootstrap_id {
        request = request.header(FREE_CONFIG_BOOTSTRAP_HEADER, bootstrap_id);
    }

    let resp = request
        .send()
        .await
        .context("failed to reach Enzyme server for free config")?;

    if identity.authorization.is_none() && matches!(resp.status().as_u16(), 401 | 403) {
        bail!(
            "Enzyme hosted bootstrap is unavailable without login. Run `enzyme login` and retry, or pass `--use-env-llm` to intentionally use OPENAI_API_KEY or OPENROUTER_API_KEY."
        );
    }

    if resp.status().as_u16() == 402 {
        let body: FreeConfigErrorResponse = resp.json().await.unwrap_or_default();
        return Err(classify_free_config_402(body));
    }

    let resp: FreeConfigResponse = resp
        .error_for_status()
        .context("Enzyme server returned an error for free config")?
        .json()
        .await
        .context("failed to parse free config response")?;
    let _ = resp.expires_at;

    info!(
        "Resolved API config from Enzyme server (model={})",
        resp.model
    );

    Ok(FetchedFreeConfig::new(
        ResolvedConfig {
            api_key: resp.api_key,
            base_url: resp.base_url,
            model: resp.model,
            provider: ConfigProvider::ServerFallback,
        },
        identity,
    ))
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
        debug!("Resolved API key from OPENAI_API_KEY (base_url={base_url}, model={model})");
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
        debug!("Resolved API key from OPENROUTER_API_KEY (base_url={base_url}, model={model})");
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
    let identity = hosted_identity()?;
    let cache_path = api_cache_path();
    let cache_path = cache_path.as_path();
    if cache_path.exists() {
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

                let wrong_profile = cached.profile != identity.cache_profile;

                if !cached.api_key.is_empty() && !expired && !wrong_profile {
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
                if wrong_profile {
                    debug!(
                        "Cached free-tier config belongs to another resolver profile; re-fetching"
                    );
                }
            }
            None => warn!(
                "Unusable API cache at {}; discarding and re-fetching",
                cache_path.display()
            ),
        }
    }

    match fetch_free_config(&identity).await {
        Ok(fetched) => {
            write_free_config_cache(cache_path, &fetched.config, &fetched.cache_profile);
            Ok(fetched.config)
        }
        Err(e) => {
            let msg = hosted_config_unavailable_message(&e);
            Err(e.context(msg))
        }
    }
}

fn hosted_config_unavailable_message(cause: &anyhow::Error) -> String {
    format!(
        "{cause:#}. No hosted Enzyme LLM config is available — run `enzyme login` and \
         check network access, or pass `--use-env-llm` to intentionally \
         use OPENAI_API_KEY or OPENROUTER_API_KEY."
    )
}

/// Resolve the API key, base URL, and model for LLM requests.
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

pub async fn resolve_api_config_for_mode(mode: LlmCredentialMode) -> Result<ResolvedConfig> {
    match mode {
        LlmCredentialMode::Auto => match configured_catalyst_mode()? {
            CatalystMode::Auto => match local_model_config(false)? {
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

pub async fn resolve_llm_client() -> Result<(ResolvedConfig, LlmClient)> {
    let config = resolve_api_config().await?;
    let client = LlmClient::from_config(&config)?;
    Ok((config, client))
}

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

pub async fn resolve_llm_client_for_mode(
    mode: LlmCredentialMode,
) -> Result<(ResolvedConfig, LlmClient)> {
    let config = resolve_api_config_for_mode(mode).await?;
    let client = LlmClient::from_config(&config)?;
    Ok((config, client))
}

pub async fn resolve_api_key() -> Result<String> {
    resolve_api_config().await.map(|c| c.api_key)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn env(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let values: HashMap<String, String> = vars
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();
        move |key| values.get(key).cloned()
    }

    fn account_identity(token: &str, user_id: &str) -> HostedIdentity {
        HostedIdentity {
            authorization: Some(format!("Bearer {token}")),
            bootstrap_id: None,
            cache_profile: identity_cache_profile("account", user_id),
        }
    }

    fn server_config(api_key: &str) -> ResolvedConfig {
        ResolvedConfig {
            api_key: api_key.to_string(),
            base_url: "https://llm.invalid/v1".to_string(),
            model: "test-model".to_string(),
            provider: ConfigProvider::ServerFallback,
        }
    }

    fn assert_fetched_cache_profile(
        captured: &HostedIdentity,
        transitioned: &HostedIdentity,
        api_key: &str,
    ) {
        assert_ne!(captured.cache_profile, transitioned.cache_profile);
        let fetched = FetchedFreeConfig::new(server_config(api_key), captured);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hosted-cache.json");
        write_free_config_cache(&path, &fetched.config, &fetched.cache_profile);
        let cached: CachedFreeConfig =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(cached.profile, captured.cache_profile);
        assert_ne!(cached.profile, transitioned.cache_profile);
        assert_eq!(cached.api_key, api_key);
    }

    #[test]
    fn resolve_cache_keeps_identity_captured_before_account_transition() {
        let captured_a = account_identity("token-a", "account-a");
        let transitioned_b = account_identity("token-b", "account-b");
        assert_eq!(captured_a.authorization.as_deref(), Some("Bearer token-a"));
        assert_fetched_cache_profile(&captured_a, &transitioned_b, "lease-for-a");
    }

    #[test]
    fn refresh_cache_keeps_identity_captured_before_inverse_transition() {
        let captured_b = account_identity("token-b", "account-b");
        let transitioned_a = account_identity("token-a", "account-a");
        assert_eq!(captured_b.authorization.as_deref(), Some("Bearer token-b"));
        assert_fetched_cache_profile(&captured_b, &transitioned_a, "refreshed-lease-for-b");
    }

    #[test]
    fn credits_exhausted_survives_context_wrapping() {
        let err = anyhow::Error::from(EnzymeCreditsExhausted { credits: 0.0 });
        let wrapped = err.context(hosted_config_unavailable_message(&anyhow::anyhow!("x")));
        assert!(wrapped.downcast_ref::<EnzymeCreditsExhausted>().is_some());
    }

    #[test]
    fn hosted_unavailable_message_preserves_specific_cause() {
        let cause = anyhow::anyhow!("Insufficient credits (0.0 remaining)");
        let msg = hosted_config_unavailable_message(&cause);
        assert!(msg.contains("Insufficient credits (0.0 remaining)"));
        assert!(msg.contains("hosted Enzyme LLM config"));
        assert!(msg.contains("--use-env-llm"));
    }

    #[test]
    fn enzyme_free_config_contract_accepts_missing_or_epoch_expiry() {
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
        assert_eq!(FREE_CONFIG_BOOTSTRAP_HEADER, "X-Enzyme-Bootstrap-Id");
    }

    #[test]
    fn free_config_402_reason_distinguishes_credits_from_capacity() {
        let credits = classify_free_config_402(FreeConfigErrorResponse {
            reason: Some("account_credits".to_string()),
            credits: Some(2.5),
        });
        assert!(credits.downcast_ref::<EnzymeCreditsExhausted>().is_some());

        let pool = classify_free_config_402(FreeConfigErrorResponse {
            reason: Some("pool_exhausted".to_string()),
            credits: Some(100.0),
        });
        assert!(
            pool.downcast_ref::<EnzymeHostedCapacityUnavailable>()
                .is_some()
        );

        let legacy = classify_free_config_402(FreeConfigErrorResponse {
            reason: None,
            credits: Some(0.0),
        });
        assert!(legacy.downcast_ref::<EnzymeCreditsExhausted>().is_some());
    }

    #[test]
    fn openrouter_env_uses_openrouter_defaults() {
        let config = resolved_env_config(env(&[("OPENROUTER_API_KEY", "openrouter-key")]))
            .unwrap()
            .unwrap();
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
    }

    #[test]
    fn openai_env_uses_openai_defaults() {
        let config = resolved_env_config(env(&[("OPENAI_API_KEY", "openai-key")]))
            .unwrap()
            .unwrap();
        assert_eq!(config.base_url, OPENAI_DEFAULT_BASE_URL);
        assert_eq!(config.model, OPENAI_DEFAULT_MODEL);
    }

    #[test]
    fn partial_env_config_does_not_block_fallback() {
        let config =
            resolved_env_config(env(&[("OPENAI_BASE_URL", "https://llm.example/v1")])).unwrap();
        assert!(config.is_none());
    }
}
