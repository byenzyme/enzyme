//! Shared environment, local-model, and anonymous hosted credential resolution.
//!
//! Account login and credit billing live behind the separate `account` feature.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
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
const FREE_CONFIG_URL: &str = "https://api.enzyme.garden/margins/free-config";
const FREE_CONFIG_BOOTSTRAP_HEADER: &str = "X-Margins-Bootstrap-Id";
const DEFAULT_API_CACHE_PATH: &str = "/tmp/enzyme-api-cache";
const FREE_CONFIG_CACHE_TTL_SECS: u64 = 300;
/// Leave enough lease life for broker/request latency and modest clock skew.
const FREE_CONFIG_EXPIRY_SAFETY_MARGIN_SECS: u64 = 30;
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
    pub expires_at: Option<i64>,
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
    expires_at: Option<i64>,
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

pub(super) struct HostedRefresh {
    enzyme_home: PathBuf,
    cache_path: PathBuf,
    cache_profile: String,
    free_config_url: String,
}

impl HostedRefresh {
    async fn refresh_after_unauthorized(&self, rejected_key: &str) -> Result<ResolvedConfig> {
        invalidate_cache_if_owned(&self.cache_path, &self.cache_profile, rejected_key);
        let identity = hosted_identity_in(&self.enzyme_home)?;
        resolve_hosted_api_config_with_from(
            identity,
            self.cache_path.clone(),
            &self.free_config_url,
        )
        .await
    }
}

#[derive(Clone)]
pub(super) struct HostedConfigSnapshot {
    pub(super) config: ResolvedConfig,
    revision: u64,
}

struct HostedConfigState {
    config: ResolvedConfig,
    revision: u64,
    last_refresh_error: Option<String>,
}

/// Per-client hosted configuration. The async mutex makes 401 refresh a
/// singleflight operation; the synchronous lock only protects short snapshots.
pub(super) struct HostedClientState {
    current: RwLock<HostedConfigState>,
    refresh_lock: tokio::sync::Mutex<()>,
    refresh: HostedRefresh,
}

impl HostedClientState {
    pub(super) fn new(config: ResolvedConfig, refresh: HostedRefresh) -> Self {
        Self {
            current: RwLock::new(HostedConfigState {
                config,
                revision: 0,
                last_refresh_error: None,
            }),
            refresh_lock: tokio::sync::Mutex::new(()),
            refresh,
        }
    }

    pub(super) fn snapshot(&self) -> HostedConfigSnapshot {
        let state = self
            .current
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        HostedConfigSnapshot {
            config: state.config.clone(),
            revision: state.revision,
        }
    }

    pub(super) async fn refresh_after_unauthorized(
        &self,
        rejected: &HostedConfigSnapshot,
    ) -> Result<HostedConfigSnapshot> {
        let _refresh_guard = self.refresh_lock.lock().await;
        {
            let state = self
                .current
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.revision != rejected.revision {
                if let Some(error) = &state.last_refresh_error {
                    return Err(anyhow::anyhow!(error.clone()));
                }
                return Ok(HostedConfigSnapshot {
                    config: state.config.clone(),
                    revision: state.revision,
                });
            }
        }

        let refreshed = match self
            .refresh
            .refresh_after_unauthorized(&rejected.config.api_key)
            .await
        {
            Ok(refreshed) => refreshed,
            Err(error) => {
                let shared_error = format!("{error:#}");
                let mut state = self
                    .current
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                state.revision = state.revision.wrapping_add(1);
                state.last_refresh_error = Some(shared_error);
                return Err(error);
            }
        };
        let mut state = self
            .current
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.config = refreshed;
        state.revision = state.revision.wrapping_add(1);
        state.last_refresh_error = None;
        Ok(HostedConfigSnapshot {
            config: state.config.clone(),
            revision: state.revision,
        })
    }
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

fn configured_catalyst_mode_at(config_path: &Path) -> Result<CatalystMode> {
    Ok(EnzymeConfig::catalyst_llm_config_at(config_path)?.mode)
}
fn local_engine_disabled(value: Option<&str>) -> bool {
    value.is_some_and(|value| value == "0" || value.eq_ignore_ascii_case("false"))
}

fn local_engine_enabled(required: bool) -> Result<bool> {
    let local_engine = env_value("ENZYME_LOCAL_ENGINE");
    if !local_engine_disabled(local_engine.as_deref()) {
        return Ok(true);
    }
    if required {
        bail!(
            "local catalyst mode is enabled but ENZYME_LOCAL_ENGINE disables it; unset the variable or run `enzyme model disable`"
        );
    }
    Ok(false)
}

fn local_model_config(required: bool) -> Result<Option<ResolvedConfig>> {
    if !local_engine_enabled(required)? {
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

fn local_model_config_at(
    models_dir: &Path,
    config_path: &Path,
    required: bool,
) -> Result<Option<ResolvedConfig>> {
    if !local_engine_enabled(required)? {
        return Ok(None);
    }
    match local_model_config_inner_at(models_dir, config_path) {
        Some(config) => Ok(Some(config)),
        None if required => {
            let name = super::super::model_registry::selected_name_at(config_path);
            bail!(
                "local catalyst mode requires model '{name}', but it is not installed; run `enzyme model install {name}`"
            )
        }
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
        expires_at: None,
    })
}

#[cfg(not(feature = "local-llm"))]
fn local_model_config_inner_at(_models_dir: &Path, _config_path: &Path) -> Option<ResolvedConfig> {
    None
}

#[cfg(feature = "local-llm")]
fn local_model_config_inner_at(models_dir: &Path, config_path: &Path) -> Option<ResolvedConfig> {
    let (name, _path) = super::super::model_registry::active_in(models_dir, config_path)?;
    Some(ResolvedConfig {
        api_key: String::new(),
        base_url: format!("{LOCAL_BASE_URL_SCHEME}{name}"),
        model: name,
        provider: ConfigProvider::Local,
        expires_at: None,
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
    let models_dir = super::super::model_registry::models_dir()?;
    local_backend_in(base_url, &models_dir)
}

#[cfg(not(feature = "local-llm"))]
fn local_backend_in(base_url: &str, _models_dir: &Path) -> Result<Backend> {
    local_backend(base_url)
}

#[cfg(feature = "local-llm")]
fn local_backend_in(base_url: &str, models_dir: &Path) -> Result<Backend> {
    let Some(name) = base_url.strip_prefix(LOCAL_BASE_URL_SCHEME) else {
        return Ok(Backend::Http);
    };
    let path = models_dir.join(format!("{name}.gguf"));
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
    /// Create a client from a config that setup already resolved.
    pub fn from_config(config: &ResolvedConfig) -> Result<Self> {
        Ok(Self {
            client: completion_http_client(),
            api_key: config.api_key.clone(),
            base_url: config.base_url.clone(),
            hosted_state: None,
            backend: local_backend(&config.base_url)?,
        })
    }
    fn from_config_in(config: &ResolvedConfig, models_dir: &Path) -> Result<Self> {
        Ok(Self {
            client: completion_http_client(),
            api_key: config.api_key.clone(),
            base_url: config.base_url.clone(),
            hosted_state: None,
            backend: local_backend_in(&config.base_url, models_dir)?,
        })
    }

    fn with_hosted_refresh(mut self, config: ResolvedConfig, refresh: HostedRefresh) -> Self {
        self.hosted_state = Some(Arc::new(HostedClientState::new(config, refresh)));
        self
    }
}

/// Remove the cached hosted lease after logout or an account identity change.
pub fn invalidate_hosted_llm_config_cache() {
    let _ = std::fs::remove_file(api_cache_path());
}

fn write_free_config_cache(path: &Path, cfg: &ResolvedConfig, profile: &str) -> Result<()> {
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let cached = CachedFreeConfig {
        api_key: cfg.api_key.clone(),
        base_url: cfg.base_url.clone(),
        model: cfg.model.clone(),
        expires_at: cfg.expires_at,
        cached_at: now_secs,
        profile: profile.to_string(),
    };
    let json = serde_json::to_vec(&cached).context("failed to serialize hosted config cache")?;
    atomic_write_secret(path, &json)
}

fn atomic_write_secret(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)
        .with_context(|| format!("failed to create cache directory {}", parent.display()))?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("hosted-config");
    let temp_path = parent.join(format!(".{file_name}.{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temp_path)
            .with_context(|| format!("failed to create temporary cache in {}", parent.display()))?;
        file.write_all(contents)
            .context("failed to write temporary hosted config cache")?;
        file.sync_all()
            .context("failed to sync temporary hosted config cache")?;
        drop(file);
        fs::rename(&temp_path, path)
            .with_context(|| format!("failed to publish hosted config cache {}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).with_context(|| {
                format!("failed to secure hosted config cache {}", path.display())
            })?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

fn invalidate_cache_if_owned(path: &Path, profile: &str, rejected_key: &str) {
    let cached = fs::read_to_string(path)
        .ok()
        .and_then(|contents| serde_json::from_str::<CachedFreeConfig>(&contents).ok());
    if cached
        .as_ref()
        .is_some_and(|cached| cached.profile == profile && cached.api_key == rejected_key)
    {
        if let Err(error) = fs::remove_file(path) {
            if error.kind() != std::io::ErrorKind::NotFound {
                warn!(
                    "failed to invalidate hosted config cache {}: {error}",
                    path.display()
                );
            }
        }
    }
}

fn cache_entry_expired(cached: &CachedFreeConfig, now_secs: u64) -> bool {
    let age_secs = now_secs.saturating_sub(cached.cached_at);
    let ttl_expired = cached.cached_at == 0 || age_secs >= FREE_CONFIG_CACHE_TTL_SECS;
    let lease_expired = cached.expires_at.is_some_and(|expires_at| {
        let Ok(expires_at) = u64::try_from(expires_at) else {
            return true;
        };
        now_secs >= expires_at.saturating_sub(FREE_CONFIG_EXPIRY_SAFETY_MARGIN_SECS)
    });
    ttl_expired || lease_expired
}

fn identity_cache_profile(kind: &str, value: &str) -> String {
    format!(
        "{FREE_CONFIG_CACHE_PROFILE_VERSION}:{kind}:{:x}",
        Sha256::digest(value.as_bytes())
    )
}

fn anonymous_hosted_identity(enzyme_home: &std::path::Path) -> Result<HostedIdentity> {
    let bootstrap_id = load_or_create_bootstrap_id(&enzyme_home.join("bootstrap.json"))?;
    Ok(HostedIdentity {
        authorization: None,
        cache_profile: identity_cache_profile("bootstrap", &bootstrap_id),
        bootstrap_id: Some(bootstrap_id),
    })
}

fn hosted_identity() -> Result<HostedIdentity> {
    let enzyme_home = crate::config::paths::EnzymePaths::enzyme_home()
        .context("failed to resolve enzyme home for hosted identity")?;
    hosted_identity_in(&enzyme_home)
}

fn hosted_identity_in(enzyme_home: &Path) -> Result<HostedIdentity> {
    #[cfg(feature = "account")]
    {
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
    }

    anonymous_hosted_identity(enzyme_home)
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

/// Fetch an account-selected or anonymous bootstrap config from the Margins broker.
async fn fetch_free_config_from(
    identity: &HostedIdentity,
    free_config_url: &str,
) -> Result<FetchedFreeConfig> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .context("failed to build HTTP client")?;

    debug!("Fetching free config from {}", free_config_url);

    let mut request = client.get(free_config_url);
    if let Some(authorization) = &identity.authorization {
        request = request.header("Authorization", authorization);
    } else if let Some(bootstrap_id) = &identity.bootstrap_id {
        request = request.header(FREE_CONFIG_BOOTSTRAP_HEADER, bootstrap_id);
    }

    let resp = request
        .send()
        .await
        .context("failed to reach the Margins free-config broker")?;

    if identity.authorization.is_none() && matches!(resp.status().as_u16(), 401 | 403) {
        bail!(
            "Margins anonymous hosted bootstrap is unavailable; retry later or pass `--use-env-llm` to intentionally use OPENAI_API_KEY or OPENROUTER_API_KEY."
        );
    }

    if resp.status().as_u16() == 402 {
        let body: FreeConfigErrorResponse = resp.json().await.unwrap_or_default();
        return Err(classify_free_config_402(body));
    }

    let resp: FreeConfigResponse = resp
        .error_for_status()
        .context("Margins free-config broker returned an error")?
        .json()
        .await
        .context("failed to parse Margins free-config response")?;
    info!(
        "Resolved API config from Margins free-config broker (model={})",
        resp.model
    );

    Ok(FetchedFreeConfig::new(
        ResolvedConfig {
            api_key: resp.api_key,
            base_url: resp.base_url,
            model: resp.model,
            provider: ConfigProvider::ServerFallback,
            expires_at: resp.expires_at,
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
            expires_at: None,
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
            expires_at: None,
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
    resolve_hosted_api_config_with(hosted_identity()?, api_cache_path()).await
}

async fn resolve_hosted_api_config_in(enzyme_home: &Path) -> Result<ResolvedConfig> {
    resolve_hosted_api_config_with(
        hosted_identity_in(enzyme_home)?,
        enzyme_home.join("llm-config-cache.json"),
    )
    .await
}

async fn resolve_hosted_api_config_with(
    identity: HostedIdentity,
    cache_path: PathBuf,
) -> Result<ResolvedConfig> {
    let endpoint = free_config_url();
    resolve_hosted_api_config_with_from(identity, cache_path, &endpoint).await
}

async fn resolve_hosted_api_config_with_from(
    identity: HostedIdentity,
    cache_path: PathBuf,
    endpoint: &str,
) -> Result<ResolvedConfig> {
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
                let expired = cache_entry_expired(&cached, now_secs);

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
                        expires_at: cached.expires_at,
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

    match fetch_free_config_from(&identity, endpoint).await {
        Ok(fetched) => {
            if let Err(error) =
                write_free_config_cache(cache_path, &fetched.config, &fetched.cache_profile)
            {
                warn!(
                    "failed to publish hosted config cache {}: {error:#}",
                    cache_path.display()
                );
            }
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
    if let Some(config) = resolved_env_config(env_value)? {
        return Ok(config);
    }
    match configured_catalyst_mode()? {
        CatalystMode::Auto => {
            if let Some(config) = local_model_config(false)? {
                return Ok(config);
            }
        }
        CatalystMode::Local => {
            return local_model_config(true)?
                .ok_or_else(|| anyhow::anyhow!("selected local catalyst model is unavailable"));
        }
        CatalystMode::Hosted => {}
    }
    resolve_hosted_api_config().await
}

/// Resolve credentials and model selection relative to an embedder-owned home.
///
/// The home contains `config.toml`, `models/`, `bootstrap.json`, and the
/// anonymous hosted lease cache. Account files are considered only when the
/// `account` feature is enabled.
pub async fn resolve_api_config_in(enzyme_home: &Path) -> Result<ResolvedConfig> {
    if let Some(config) = resolved_env_config(env_value)? {
        return Ok(config);
    }

    let config_path = enzyme_home.join("config.toml");
    let models_dir = enzyme_home.join("models");
    match configured_catalyst_mode_at(&config_path)? {
        CatalystMode::Auto => {
            if let Some(config) = local_model_config_at(&models_dir, &config_path, false)? {
                return Ok(config);
            }
        }
        CatalystMode::Local => {
            return local_model_config_at(&models_dir, &config_path, true)?
                .ok_or_else(|| anyhow::anyhow!("selected local catalyst model is unavailable"));
        }
        CatalystMode::Hosted => {}
    }
    resolve_hosted_api_config_in(enzyme_home).await
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

pub async fn resolve_llm_client_in(enzyme_home: &Path) -> Result<(ResolvedConfig, LlmClient)> {
    let config = resolve_api_config_in(enzyme_home).await?;
    let refresh = hosted_refresh_for(
        enzyme_home,
        enzyme_home.join("llm-config-cache.json"),
        &config,
    )?;
    let mut client = LlmClient::from_config_in(&config, &enzyme_home.join("models"))?;
    if let Some(refresh) = refresh {
        client = client.with_hosted_refresh(config.clone(), refresh);
    }
    Ok((config, client))
}

fn hosted_refresh_for(
    enzyme_home: &Path,
    cache_path: PathBuf,
    config: &ResolvedConfig,
) -> Result<Option<HostedRefresh>> {
    if !matches!(config.provider, ConfigProvider::ServerFallback) {
        return Ok(None);
    }
    let identity = hosted_identity_in(enzyme_home)?;
    Ok(Some(HostedRefresh {
        enzyme_home: enzyme_home.to_path_buf(),
        cache_path,
        cache_profile: identity.cache_profile,
        free_config_url: free_config_url(),
    }))
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

    #[test]
    fn local_engine_legacy_opt_out_accepts_only_zero_or_false() {
        assert!(local_engine_disabled(Some("0")));
        assert!(local_engine_disabled(Some("false")));
        assert!(local_engine_disabled(Some("FALSE")));
        assert!(!local_engine_disabled(None));
        assert!(!local_engine_disabled(Some("")));
        assert!(!local_engine_disabled(Some("disabled")));
        assert!(!local_engine_disabled(Some("1")));
    }

    #[test]
    fn anonymous_identity_is_stable_and_account_free() {
        let home = tempfile::tempdir().unwrap();
        let first = anonymous_hosted_identity(home.path()).unwrap();
        let second = anonymous_hosted_identity(home.path()).unwrap();
        assert_eq!(first.bootstrap_id, second.bootstrap_id);
        assert!(first.authorization.is_none());
        assert_eq!(first.cache_profile, second.cache_profile);
        assert!(home.path().join("bootstrap.json").is_file());
    }

    #[tokio::test]
    async fn lean_build_fetches_anonymous_config_with_margins_header() {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut bytes = [0_u8; 8192];
            let read = stream.read(&mut bytes).unwrap();
            let request = String::from_utf8_lossy(&bytes[..read]).into_owned();
            let body = r#"{"api_key":"anonymous-key","base_url":"https://llm.invalid/v1","model":"anonymous-model"}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
            request
        });

        let home = tempfile::tempdir().unwrap();
        let identity = anonymous_hosted_identity(home.path()).unwrap();
        let bootstrap_id = identity.bootstrap_id.clone().unwrap();
        let fetched =
            fetch_free_config_from(&identity, &format!("http://{addr}/margins/free-config"))
                .await
                .unwrap();
        let request = server.join().unwrap().to_ascii_lowercase();

        assert_eq!(fetched.config.api_key, "anonymous-key");
        assert_eq!(fetched.config.model, "anonymous-model");
        assert!(matches!(
            fetched.config.provider,
            ConfigProvider::ServerFallback
        ));
        assert!(request.starts_with("get /margins/free-config "));
        assert!(request.contains(&format!(
            "\r\nx-margins-bootstrap-id: {}\r\n",
            bootstrap_id.to_ascii_lowercase()
        )));
        assert!(!request.contains("authorization:"));
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
        assert_eq!(
            FREE_CONFIG_URL,
            "https://api.enzyme.garden/margins/free-config"
        );
        assert_eq!(FREE_CONFIG_BOOTSTRAP_HEADER, "X-Margins-Bootstrap-Id");
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
    fn cached_fixture(cached_at: u64, expires_at: Option<i64>) -> CachedFreeConfig {
        CachedFreeConfig {
            api_key: "leased-key".to_string(),
            base_url: "https://llm.invalid/v1".to_string(),
            model: "fixture-model".to_string(),
            expires_at,
            cached_at,
            profile: "fixture-profile".to_string(),
        }
    }

    fn resolved_fixture(api_key: &str, expires_at: Option<i64>) -> ResolvedConfig {
        ResolvedConfig {
            api_key: api_key.to_string(),
            base_url: "https://llm.invalid/v1".to_string(),
            model: "fixture-model".to_string(),
            provider: ConfigProvider::ServerFallback,
            expires_at,
        }
    }

    #[test]
    fn cache_expires_at_earlier_of_ttl_and_lease_margin() {
        let now = 1_800_000_000_u64;
        assert!(!cache_entry_expired(
            &cached_fixture(now - 10, Some((now + 31) as i64)),
            now,
        ));
        assert!(cache_entry_expired(
            &cached_fixture(now - 10, Some((now + 30) as i64)),
            now,
        ));
        assert!(cache_entry_expired(
            &cached_fixture(now - FREE_CONFIG_CACHE_TTL_SECS, Some((now + 3_600) as i64)),
            now,
        ));
        assert!(!cache_entry_expired(&cached_fixture(now - 10, None), now,));
    }

    #[test]
    fn cache_publication_is_parseable_atomic_and_owner_only() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("llm-config-cache.json");
        let config = resolved_fixture("secret-key", Some(1_900_000_000));
        write_free_config_cache(&path, &config, "profile-a").unwrap();

        let cached: CachedFreeConfig =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(cached.api_key, "secret-key");
        assert_eq!(cached.expires_at, Some(1_900_000_000));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert!(std::fs::read_dir(home.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp")
        }));
    }

    #[test]
    fn concurrent_cache_publication_never_exposes_partial_json() {
        let home = tempfile::tempdir().unwrap();
        let path = Arc::new(home.path().join("llm-config-cache.json"));
        let writers: Vec<_> = (0..12)
            .map(|index| {
                let path = Arc::clone(&path);
                std::thread::spawn(move || {
                    write_free_config_cache(
                        &path,
                        &resolved_fixture(&format!("secret-{index}"), Some(1_900_000_000)),
                        "profile-a",
                    )
                    .unwrap();
                })
            })
            .collect();
        for writer in writers {
            writer.join().unwrap();
        }

        let cached: CachedFreeConfig =
            serde_json::from_slice(&std::fs::read(path.as_ref()).unwrap()).unwrap();
        assert!(cached.api_key.starts_with("secret-"));
        assert_eq!(cached.profile, "profile-a");
    }

    #[test]
    fn cache_failures_do_not_include_secret_material() {
        let home = tempfile::tempdir().unwrap();
        let secret = "must-not-appear-in-errors";
        let error = write_free_config_cache(
            home.path(),
            &resolved_fixture(secret, Some(1_900_000_000)),
            "profile-a",
        )
        .unwrap_err();
        assert!(!format!("{error:#}").contains(secret));
    }

    #[test]
    fn unauthorized_invalidation_requires_profile_and_rejected_key_ownership() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("llm-config-cache.json");
        write_free_config_cache(
            &path,
            &resolved_fixture("rejected-key", Some(1_900_000_000)),
            "profile-a",
        )
        .unwrap();

        invalidate_cache_if_owned(&path, "profile-b", "rejected-key");
        assert!(path.exists());
        invalidate_cache_if_owned(&path, "profile-a", "other-key");
        assert!(path.exists());
        invalidate_cache_if_owned(&path, "profile-a", "rejected-key");
        assert!(!path.exists());
    }
}
