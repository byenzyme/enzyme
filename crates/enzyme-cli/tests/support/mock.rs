//! Mock HTTP servers for the e2e suite.
//!
//! Backed by `wiremock`. Both servers use a single dynamic [`wiremock::Respond`]
//! implementation per route wired to shared mutable state, which gives us
//! per-request response sequencing (e.g. "401 then 200") and request counting —
//! something a static request/response mock library cannot express.
//!
//! All APIs here are **synchronous**: the shared tokio runtime is owned by this
//! module, so test authors write plain `#[test] fn`.

#![allow(dead_code)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

/// Shared multi-threaded runtime that owns every mock server's background task.
/// Never dropped, so servers stay alive for the whole test binary.
fn rt() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("failed to build mock-server runtime")
    })
}

/// One canned HTTP response.
#[derive(Clone, Debug)]
pub struct Reply {
    pub status: u16,
    pub body: String,
}

impl Reply {
    pub fn new(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            body: body.into(),
        }
    }

    /// 200 with a JSON body.
    pub fn ok_json(body: impl Into<String>) -> Self {
        Self::new(200, body)
    }
}

/// A queue of replies. The final entry repeats forever once reached.
#[derive(Debug)]
struct ReplyQueue {
    replies: Vec<Reply>,
    served: usize,
}

impl ReplyQueue {
    fn new(reply: Reply) -> Self {
        Self {
            replies: vec![reply],
            served: 0,
        }
    }

    fn set(&mut self, replies: Vec<Reply>) {
        assert!(!replies.is_empty(), "reply sequence must be non-empty");
        self.replies = replies;
        self.served = 0;
    }

    fn next(&mut self) -> Reply {
        let idx = self.served.min(self.replies.len() - 1);
        self.served += 1;
        self.replies[idx].clone()
    }
}

fn template(reply: &Reply) -> ResponseTemplate {
    ResponseTemplate::new(reply.status)
        .insert_header("content-type", "application/json")
        .set_body_string(reply.body.clone())
}

// ---------------------------------------------------------------------------
// Fake Enzyme API (credits + free-config)
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct EnzymeApiState {
    /// Remaining credit balance reported by `/v1/credits/deduct`.
    balance: f64,
    /// When true, `/v1/credits/deduct` answers 402 regardless of `balance`.
    deduct_402: bool,
    /// Explicit override queue for `/v1/credits/deduct`; when set it wins.
    deduct_replies: Option<ReplyQueue>,
    /// Replies for the free-config endpoint.
    free_config: ReplyQueue,
    /// Bodies of every deduct request received, in order.
    deduct_requests: Vec<String>,
    /// Explicit override queue for `GET /v1/credits`; when unset the responder
    /// synthesizes `{ "credits": balance }`.
    credits_get_replies: Option<ReplyQueue>,
    /// `Authorization` header of every free-config request, in order.
    free_config_auth_headers: Vec<String>,
    /// `X-Enzyme-Bootstrap-Id` header of every free-config request, in order.
    free_config_bootstrap_ids: Vec<String>,
    /// When true, free-config answers 400 `Missing bootstrap id` if neither an
    /// `Authorization` nor an `X-Enzyme-Bootstrap-Id` header is present
    /// (emulates the deployed unauthenticated contract, matrix config 7).
    free_config_require_bootstrap: bool,
    /// When present, select the response from request identity instead of a
    /// preselected tier payload. This models Enzyme server-side tier routing.
    free_config_by_auth: Option<AuthRoutedFreeConfig>,
}

#[derive(Debug)]
struct AuthRoutedFreeConfig {
    expected_token: String,
    authenticated: Reply,
    anonymous: Reply,
}

/// Fake `api.enzyme.garden`: hosted credits + the free-config endpoint.
///
/// Point the CLI at it with `ENZYME_API_URL` (credits/login) and
/// `ENZYME_FREE_CONFIG_URL` (free-config) — see [`super::fixture::CliCommand::enzyme_api`].
pub struct MockEnzymeApi {
    server: MockServer,
    state: Arc<Mutex<EnzymeApiState>>,
    deduct_calls: Arc<AtomicUsize>,
    free_config_calls: Arc<AtomicUsize>,
    logout_calls: Arc<AtomicUsize>,
    credits_get_calls: Arc<AtomicUsize>,
    refund_calls: Arc<AtomicUsize>,
    account_calls: Arc<AtomicUsize>,
}

struct DeductResponder {
    state: Arc<Mutex<EnzymeApiState>>,
    calls: Arc<AtomicUsize>,
}

impl Respond for DeductResponder {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut state = self.state.lock().unwrap();
        state
            .deduct_requests
            .push(String::from_utf8_lossy(&request.body).to_string());

        if let Some(queue) = state.deduct_replies.as_mut() {
            return template(&queue.next());
        }

        let parsed = serde_json::from_slice::<serde_json::Value>(&request.body).ok();
        let amount = parsed
            .as_ref()
            .and_then(|v| v["amount"].as_f64())
            .unwrap_or(0.0);
        let reason = parsed
            .as_ref()
            .and_then(|v| v["reason"].as_str())
            .unwrap_or("")
            .to_string();

        if state.deduct_402 || state.balance < amount {
            // Deployed 402 contract: { error, credits, required }.
            let body = serde_json::json!({
                "error": "Insufficient credits",
                "credits": state.balance,
                "required": amount,
            });
            return template(&Reply::new(402, body.to_string()));
        }

        state.balance -= amount;
        // Deployed 200 contract: { credits, deducted, reason }.
        let body = serde_json::json!({
            "credits": state.balance,
            "deducted": amount,
            "reason": reason,
        });
        template(&Reply::ok_json(body.to_string()))
    }
}

struct FreeConfigResponder {
    state: Arc<Mutex<EnzymeApiState>>,
    calls: Arc<AtomicUsize>,
}

impl Respond for FreeConfigResponder {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut state = self.state.lock().unwrap();

        let header = |name: &str| {
            request
                .headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_string()
        };
        let auth = header("authorization");
        let bootstrap = header("x-enzyme-bootstrap-id");
        state.free_config_auth_headers.push(auth.clone());
        state.free_config_bootstrap_ids.push(bootstrap.clone());

        // Emulate the deployed unauthenticated contract: without a Bearer token
        // OR a bootstrap id the server refuses with 400 (matrix config 7).
        if state.free_config_require_bootstrap && auth.is_empty() && bootstrap.is_empty() {
            let body = serde_json::json!({
                "error": "Missing bootstrap id",
                "message": "Unauthenticated free-config requests must include x-enzyme-bootstrap-id.",
            });
            return template(&Reply::new(400, body.to_string()));
        }

        if let Some(routed) = &state.free_config_by_auth {
            if auth == format!("Bearer {}", routed.expected_token) && bootstrap.is_empty() {
                return template(&routed.authenticated);
            }
            if auth.is_empty() && !bootstrap.is_empty() {
                return template(&routed.anonymous);
            }
            return template(&Reply::new(
                401,
                r#"{"error":"invalid hosted identity headers"}"#,
            ));
        }

        template(&state.free_config.next())
    }
}

struct CreditsGetResponder {
    state: Arc<Mutex<EnzymeApiState>>,
    calls: Arc<AtomicUsize>,
}

impl Respond for CreditsGetResponder {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut state = self.state.lock().unwrap();
        if let Some(queue) = state.credits_get_replies.as_mut() {
            return template(&queue.next());
        }
        let body = serde_json::json!({ "credits": state.balance });
        template(&Reply::ok_json(body.to_string()))
    }
}

struct CountingResponder {
    calls: Arc<AtomicUsize>,
    reply: Reply,
}

impl Respond for CountingResponder {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        self.calls.fetch_add(1, Ordering::SeqCst);
        template(&self.reply)
    }
}

impl MockEnzymeApi {
    /// Path the free-config endpoint is mounted at.
    pub const FREE_CONFIG_PATH: &'static str = "/llm/free-config";

    /// Start a fake Enzyme API with a default balance of 100.0 credits and a
    /// free-config endpoint that hands back key `enz-free-test` pointing at
    /// `http://127.0.0.1:1/v1` (an unroutable placeholder — call
    /// [`MockEnzymeApi::set_free_config`] with a [`MockLlm::base_url`] for a
    /// usable LLM).
    pub fn start() -> Self {
        let state = Arc::new(Mutex::new(EnzymeApiState {
            balance: 100.0,
            deduct_402: false,
            deduct_replies: None,
            free_config: ReplyQueue::new(Reply::ok_json(
                serde_json::json!({
                    "api_key": "enz-free-test",
                    "base_url": "http://127.0.0.1:1/v1",
                    "model": "test-model",
                })
                .to_string(),
            )),
            deduct_requests: Vec::new(),
            credits_get_replies: None,
            free_config_auth_headers: Vec::new(),
            free_config_bootstrap_ids: Vec::new(),
            free_config_require_bootstrap: false,
            free_config_by_auth: None,
        }));
        let deduct_calls = Arc::new(AtomicUsize::new(0));
        let free_config_calls = Arc::new(AtomicUsize::new(0));
        let logout_calls = Arc::new(AtomicUsize::new(0));
        let credits_get_calls = Arc::new(AtomicUsize::new(0));
        let refund_calls = Arc::new(AtomicUsize::new(0));
        let account_calls = Arc::new(AtomicUsize::new(0));

        let server = rt().block_on(async {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/v1/credits/deduct"))
                .respond_with(DeductResponder {
                    state: state.clone(),
                    calls: deduct_calls.clone(),
                })
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path(Self::FREE_CONFIG_PATH))
                .respond_with(FreeConfigResponder {
                    state: state.clone(),
                    calls: free_config_calls.clone(),
                })
                .mount(&server)
                .await;
            Mock::given(method("DELETE"))
                .and(path("/v1/api-keys/current"))
                .respond_with(CountingResponder {
                    calls: logout_calls.clone(),
                    reply: Reply::ok_json("{}"),
                })
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path("/v1/credits"))
                .respond_with(CreditsGetResponder {
                    state: state.clone(),
                    calls: credits_get_calls.clone(),
                })
                .mount(&server)
                .await;
            // New-CLI-contract endpoints (R2). The current CLI must NEVER hit
            // these; they exist only so a test can assert a call count of 0 (or
            // observe a would-be new CLI tolerating the 404).
            Mock::given(method("POST"))
                .and(path("/v1/credits/refund"))
                .respond_with(CountingResponder {
                    calls: refund_calls.clone(),
                    reply: Reply::new(404, r#"{"error":"Not found"}"#),
                })
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path("/v1/account"))
                .respond_with(CountingResponder {
                    calls: account_calls.clone(),
                    reply: Reply::new(404, r#"{"error":"Not found"}"#),
                })
                .mount(&server)
                .await;
            server
        });

        Self {
            server,
            state,
            deduct_calls,
            free_config_calls,
            logout_calls,
            credits_get_calls,
            refund_calls,
            account_calls,
        }
    }

    /// Base URL for `ENZYME_API_URL`, e.g. `http://127.0.0.1:53412`.
    pub fn base_url(&self) -> String {
        self.server.uri()
    }

    /// Full URL for `ENZYME_FREE_CONFIG_URL`.
    pub fn free_config_url(&self) -> String {
        format!("{}{}", self.server.uri(), Self::FREE_CONFIG_PATH)
    }

    /// Set the balance `/v1/credits/deduct` starts from.
    pub fn set_credit_balance(&self, credits: f64) {
        self.state.lock().unwrap().balance = credits;
    }

    /// Force `/v1/credits/deduct` to always answer 402 Insufficient credits.
    pub fn set_deduct_always_402(&self, remaining: f64) {
        let mut state = self.state.lock().unwrap();
        state.deduct_402 = true;
        state.balance = remaining;
    }

    /// Fully override the `/v1/credits/deduct` replies; the last entry repeats.
    pub fn set_deduct_replies(&self, replies: Vec<Reply>) {
        self.state.lock().unwrap().deduct_replies = Some(ReplyQueue { replies, served: 0 });
    }

    /// Serve a successful free-config pointing at the given LLM.
    pub fn set_free_config(&self, api_key: &str, base_url: &str, model: &str) {
        let body = serde_json::json!({
            "api_key": api_key,
            "base_url": base_url,
            "model": model,
        })
        .to_string();
        self.state
            .lock()
            .unwrap()
            .free_config
            .set(vec![Reply::ok_json(body)]);
    }

    /// Serve exactly `body` as the free-config 200 response. The raw escape
    /// hatch behind every convenience setter below; use it for a body shape not
    /// otherwise expressible (arbitrary additive/renamed/future fields).
    pub fn set_free_config_json(&self, body: serde_json::Value) {
        self.state
            .lock()
            .unwrap()
            .free_config
            .set(vec![Reply::ok_json(body.to_string())]);
    }

    /// **OLD-server** deployed free-config success: the 3 core fields PLUS
    /// `provider` and `disclaimer`, and an optional `expires_at` (unix secs;
    /// present for daily/billing-period leases, omitted when `None`). This is
    /// the exact current-server body shape (R1). The old CLI's
    /// `FreeConfigResponse` ignores everything but api_key/base_url/model.
    pub fn set_free_config_full(
        &self,
        api_key: &str,
        base_url: &str,
        model: &str,
        provider: &str,
        disclaimer: &str,
        expires_at: Option<u64>,
    ) {
        let mut body = serde_json::json!({
            "api_key": api_key,
            "base_url": base_url,
            "model": model,
            "provider": provider,
            "disclaimer": disclaimer,
        });
        if let Some(exp) = expires_at {
            body["expires_at"] = serde_json::json!(exp);
        }
        self.set_free_config_json(body);
    }

    /// **NEW-server** additive free-config success: the deployed body PLUS
    /// unknown/future fields (`tier`, `reason`, `pool_status`). Proves R1/R5
    /// additive tolerance — this must still parse under the current CLI's
    /// `FreeConfigResponse` (no `deny_unknown_fields`).
    pub fn set_free_config_additive(&self, api_key: &str, base_url: &str, model: &str) {
        self.set_free_config_json(serde_json::json!({
            "api_key": api_key,
            "base_url": base_url,
            "model": model,
            "provider": "openrouter-free",
            "disclaimer": "Included AI. Quality may vary.",
            "expires_at": 4102444800u64,
            // Additive/future fields the current CLI has never seen:
            "tier": "free",
            "reason": "pool_headroom_ok",
            "pool_status": "funded",
        }));
    }

    /// Config 1/2 — a FREE-tier lease (`provider: openrouter-free`, $0.20/day).
    pub fn set_free_config_free_tier(&self, api_key: &str, base_url: &str, model: &str) {
        self.set_free_config_full(
            api_key,
            base_url,
            model,
            "openrouter-free",
            "Included AI. Quality may vary.",
            Some(4102444800),
        );
    }

    /// Config 3 — an active PRO lease (`provider: openrouter-pro-included`, $10/mo).
    pub fn set_free_config_pro_tier(&self, api_key: &str, base_url: &str, model: &str) {
        self.set_free_config_full(
            api_key,
            base_url,
            model,
            "openrouter-pro-included",
            "Pro AI.",
            Some(4102444800),
        );
    }

    /// Select an authenticated vs anonymous lease from the actual request
    /// headers, modeling `/llm/free-config` rather than preselecting a tier.
    pub fn set_free_config_by_auth(
        &self,
        expected_token: &str,
        authenticated_key: &str,
        anonymous_key: &str,
        base_url: &str,
        model: &str,
    ) {
        let response = |api_key: &str, provider: &str| {
            Reply::ok_json(
                serde_json::json!({
                    "api_key": api_key,
                    "base_url": base_url,
                    "model": model,
                    "provider": provider,
                })
                .to_string(),
            )
        };
        self.state.lock().unwrap().free_config_by_auth = Some(AuthRoutedFreeConfig {
            expected_token: expected_token.to_string(),
            authenticated: response(authenticated_key, "openrouter-pro-included"),
            anonymous: response(anonymous_key, "openrouter-free"),
        });
    }

    /// Config 4, OLD server — a lapsed Pro whose credits are stranded still
    /// draws a FREE lease (the deployed handler selects pro only on a live
    /// billing row; a lapsed row falls through to free even with credits>0).
    pub fn set_free_config_lapsed_pro_old(&self, api_key: &str, base_url: &str, model: &str) {
        self.set_free_config_free_tier(api_key, base_url, model);
    }

    /// Config 4, NEW server — entitlement reconciliation restores the PRO lease
    /// for a lapsed-but-credited account.
    pub fn set_free_config_lapsed_pro_new(&self, api_key: &str, base_url: &str, model: &str) {
        self.set_free_config_pro_tier(api_key, base_url, model);
    }

    /// Make the free-config endpoint answer 402 with `credits` remaining.
    ///
    /// NOTE: legacy shape — body is ONLY `{ "credits": n }`. Prefer
    /// [`set_free_config_402_full`](Self::set_free_config_402_full) for the true
    /// deployed contract; kept for back-compat with existing tests.
    pub fn set_free_config_402(&self, credits: f64) {
        let body = serde_json::json!({ "credits": credits }).to_string();
        self.state
            .lock()
            .unwrap()
            .free_config
            .set(vec![Reply::new(402, body)]);
    }

    /// Deployed free-config 402 contract (authed, credits<=0):
    /// `{ error: 'Insufficient credits', credits, message: 'Visit
    /// app.enzyme.garden/plans to top up.' }`.
    pub fn set_free_config_402_full(&self, credits: f64) {
        let body = serde_json::json!({
            "error": "Insufficient credits",
            "reason": "account_credits",
            "credits": credits,
            "message": "Visit app.enzyme.garden/plans to top up.",
        });
        self.state
            .lock()
            .unwrap()
            .free_config
            .set(vec![Reply::new(402, body.to_string())]);
    }

    /// POOL FUNDED — free-config mints a usable 200 lease pointing at `base_url`
    /// (pair with [`MockLlm::set_pool_funded`]). Matrix configs 1/3/pool-ok.
    pub fn set_pool_funded(&self, api_key: &str, base_url: &str, model: &str) {
        self.set_free_config_free_tier(api_key, base_url, model);
    }

    /// POOL DRY, **old-server behavior** (config 5a): the server still mints a
    /// valid 200 lease — the pool-dry 402 only surfaces later on the LLM
    /// completion. Pair with [`MockLlm::set_pool_dry_402`].
    pub fn set_pool_dry_old(&self, api_key: &str, base_url: &str, model: &str) {
        self.set_free_config_free_tier(api_key, base_url, model);
    }

    /// POOL DRY, **new-server behavior** (config 5b): a clean pre-key refusal —
    /// free-config itself 402s before handing out a lease. No LLM call happens.
    pub fn set_pool_dry_new(&self, credits: f64) {
        let body = serde_json::json!({
            "error": "Hosted capacity unavailable",
            "reason": "pool_exhausted",
            "credits": credits,
            "message": "Shared hosted LLM capacity is temporarily unavailable.",
        });
        self.state
            .lock()
            .unwrap()
            .free_config
            .set(vec![Reply::new(402, body.to_string())]);
    }

    /// Fully override the free-config replies; the last entry repeats.
    /// Use this to express "402 first, 200 on refresh" style sequences.
    pub fn set_free_config_replies(&self, replies: Vec<Reply>) {
        self.state.lock().unwrap().free_config.set(replies);
    }

    /// How many times `/v1/credits/deduct` was called.
    pub fn deduct_calls(&self) -> usize {
        self.deduct_calls.load(Ordering::SeqCst)
    }

    /// Whether `/v1/credits/deduct` was called at least once.
    pub fn deduct_called(&self) -> bool {
        self.deduct_calls() > 0
    }

    /// Raw bodies of every deduct request, in order.
    pub fn deduct_requests(&self) -> Vec<String> {
        self.state.lock().unwrap().deduct_requests.clone()
    }

    /// How many times the free-config endpoint was called.
    pub fn free_config_calls(&self) -> usize {
        self.free_config_calls.load(Ordering::SeqCst)
    }

    /// How many times `DELETE /v1/api-keys/current` (logout revoke) was called.
    pub fn logout_calls(&self) -> usize {
        self.logout_calls.load(Ordering::SeqCst)
    }

    /// Assert the deduct endpoint was hit exactly `expected` times.
    #[track_caller]
    pub fn assert_deduct_calls(&self, expected: usize) {
        assert_eq!(
            self.deduct_calls(),
            expected,
            "unexpected /v1/credits/deduct call count"
        );
    }

    // -- GET /v1/credits -----------------------------------------------------

    /// Make `GET /v1/credits` answer `200 { credits }`. (Without this the
    /// endpoint synthesizes `{ credits: <current balance> }`.)
    pub fn set_credits_get(&self, credits: f64) {
        self.state.lock().unwrap().credits_get_replies = Some(ReplyQueue::new(Reply::ok_json(
            serde_json::json!({ "credits": credits }).to_string(),
        )));
    }

    /// Fully override `GET /v1/credits` replies (e.g. a 500 or 401); the last
    /// entry repeats.
    pub fn set_credits_get_replies(&self, replies: Vec<Reply>) {
        self.state.lock().unwrap().credits_get_replies = Some(ReplyQueue { replies, served: 0 });
    }

    /// How many times `GET /v1/credits` was called.
    pub fn credits_get_calls(&self) -> usize {
        self.credits_get_calls.load(Ordering::SeqCst)
    }

    // -- free-config request-header capture ----------------------------------

    /// `Authorization` header of every free-config request, in order. Empty
    /// string when absent. Proves an authed CLI sends `Bearer <token>`.
    pub fn free_config_auth_headers(&self) -> Vec<String> {
        self.state.lock().unwrap().free_config_auth_headers.clone()
    }

    /// `X-Enzyme-Bootstrap-Id` header of every free-config request, in order.
    /// Empty string when absent. Proves an anonymous CLI sends a bootstrap id
    /// (matrix config 7) instead of a Bearer token.
    pub fn free_config_bootstrap_ids(&self) -> Vec<String> {
        self.state.lock().unwrap().free_config_bootstrap_ids.clone()
    }

    /// When enabled, free-config answers `400 { error: 'Missing bootstrap id' }`
    /// for a request carrying neither `Authorization` nor
    /// `X-Enzyme-Bootstrap-Id` (hosted broker contract, config 7).
    pub fn set_free_config_require_bootstrap(&self, require: bool) {
        self.state.lock().unwrap().free_config_require_bootstrap = require;
    }

    // -- new-CLI-contract endpoints (R2, config 9) ---------------------------

    /// How many times `POST /v1/credits/refund` was hit. The current CLI must
    /// never call it, so this should stay 0 (the mount 404s regardless).
    pub fn refund_calls(&self) -> usize {
        self.refund_calls.load(Ordering::SeqCst)
    }

    /// How many times `GET /v1/account` was hit. The current CLI must never
    /// call it, so this should stay 0 (the mount 404s regardless).
    pub fn account_calls(&self) -> usize {
        self.account_calls.load(Ordering::SeqCst)
    }
}

// ---------------------------------------------------------------------------
// Fake OpenAI-compatible LLM
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct LlmState {
    replies: ReplyQueue,
    requests: Vec<String>,
    auth_headers: Vec<String>,
}

/// Fake OpenAI-compatible chat-completions server.
///
/// Mounted at `POST /v1/chat/completions`; [`MockLlm::base_url`] returns the
/// `/v1` prefix, which is what `OPENAI_BASE_URL` / a free-config `base_url` want.
pub struct MockLlm {
    server: MockServer,
    state: Arc<Mutex<LlmState>>,
    calls: Arc<AtomicUsize>,
}

struct LlmResponder {
    state: Arc<Mutex<LlmState>>,
    calls: Arc<AtomicUsize>,
}

impl Respond for LlmResponder {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut state = self.state.lock().unwrap();
        state
            .requests
            .push(String::from_utf8_lossy(&request.body).to_string());
        state.auth_headers.push(
            request
                .headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_string(),
        );
        template(&state.replies.next())
    }
}

impl MockLlm {
    /// Start a fake LLM that answers every request with a valid, empty-ish
    /// chat completion (see [`MockLlm::completion_body`]).
    pub fn start() -> Self {
        let state = Arc::new(Mutex::new(LlmState {
            replies: ReplyQueue::new(Reply::ok_json(Self::completion_body("{}"))),
            requests: Vec::new(),
            auth_headers: Vec::new(),
        }));
        let calls = Arc::new(AtomicUsize::new(0));

        let server = rt().block_on(async {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/v1/chat/completions"))
                .respond_with(LlmResponder {
                    state: state.clone(),
                    calls: calls.clone(),
                })
                .mount(&server)
                .await;
            server
        });

        Self {
            server,
            state,
            calls,
        }
    }

    /// Build a well-formed OpenAI chat-completion JSON body wrapping `content`.
    pub fn completion_body(content: &str) -> String {
        // Most compatibility fixtures predate evidence-anchor output. Preserve
        // their intent by grounding every canned catalyst in the first evidence
        // label; dedicated grounding tests exercise invalid/missing labels.
        let content = serde_json::from_str::<serde_json::Value>(content)
            .ok()
            .map(|mut value| {
                if value.get("anchorIds").is_none()
                    && let Some(count) = value
                        .get("catalysts")
                        .and_then(|v| v.as_array())
                        .map(Vec::len)
                {
                    value["anchorIds"] =
                        serde_json::json!((0..count).map(|_| vec!["E1"]).collect::<Vec<_>>());
                }
                value.to_string()
            })
            .unwrap_or_else(|| content.to_string());
        serde_json::json!({
            "id": "chatcmpl-test",
            "object": "chat.completion",
            "model": "test-model",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": content },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
        })
        .to_string()
    }

    /// OpenAI-compatible base URL, e.g. `http://127.0.0.1:53413/v1`.
    pub fn base_url(&self) -> String {
        format!("{}/v1", self.server.uri())
    }

    /// Answer every request with this status + raw body.
    pub fn set_response(&self, status: u16, body: impl Into<String>) {
        self.state
            .lock()
            .unwrap()
            .replies
            .set(vec![Reply::new(status, body)]);
    }

    /// Answer requests from this queue in order; the last entry repeats.
    pub fn set_response_sequence(&self, replies: Vec<Reply>) {
        self.state.lock().unwrap().replies.set(replies);
    }

    /// Answer every request with a 200 completion whose message content is `content`.
    pub fn set_ok_content(&self, content: &str) {
        self.set_response(200, Self::completion_body(content));
    }

    /// Canonical OpenRouter pool-dry 402 body: contains both `402` and
    /// `Insufficient credits` plus the characteristic "can only afford N
    /// tokens" phrasing. Feeds `is_credits_exhausted_error` (D1).
    pub fn pool_dry_402_body() -> String {
        serde_json::json!({
            "error": {
                "code": 402,
                "message": "Insufficient credits. Add more using https://openrouter.ai/credits. \
                            This request requires 12000 tokens but your key can only afford 0 tokens.",
                "metadata": { "provider_name": "openrouter" }
            }
        })
        .to_string()
    }

    /// Google AI Studio 429 quota body whose ONLY occurrence of `402` is inside
    /// the number `4029` — drives the `is_credits_exhausted_error` substring
    /// misclassification (D1).
    pub fn google_429_body() -> String {
        serde_json::json!({
            "error": {
                "code": 429,
                "message": "Resource exhausted: quota_limit_value 4029 for quota metric \
                            'generate_requests_per_model_per_day'",
                "status": "RESOURCE_EXHAUSTED"
            }
        })
        .to_string()
    }

    /// POOL FUNDED — normal 200 completion (empty `{}` content). The healthy
    /// baseline to pair with [`MockEnzymeApi::set_pool_funded`].
    pub fn set_pool_funded(&self) {
        self.set_ok_content("{}");
    }

    /// POOL DRY — every completion 402s with the OpenRouter pool-dry body
    /// regardless of the leased key (config 5a old-server runtime failure).
    /// Alias of [`set_llm_402_pool_dry`](Self::set_llm_402_pool_dry).
    pub fn set_pool_dry_402(&self) {
        self.set_response(402, Self::pool_dry_402_body());
    }

    /// Every completion 402s with the OpenRouter pool-dry body (config 5a).
    pub fn set_llm_402_pool_dry(&self) {
        self.set_response(402, Self::pool_dry_402_body());
    }

    /// Every completion 429s with the Google `4029` quota body (BYOK provider
    /// rate limit; the D1 substring trap).
    pub fn set_llm_429_google_4029(&self) {
        self.set_response(429, Self::google_429_body());
    }

    /// How many chat-completion requests were received.
    pub fn request_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    /// Raw bodies of every chat-completion request, in order.
    pub fn requests(&self) -> Vec<String> {
        self.state.lock().unwrap().requests.clone()
    }

    /// `Authorization` header of every chat-completion request, in order.
    /// Useful for asserting generation uses only its configured key.
    pub fn auth_headers(&self) -> Vec<String> {
        self.state.lock().unwrap().auth_headers.clone()
    }

    /// Assert the LLM was hit exactly `expected` times.
    #[track_caller]
    pub fn assert_request_count(&self, expected: usize) {
        assert_eq!(
            self.request_count(),
            expected,
            "unexpected LLM request count"
        );
    }
}
