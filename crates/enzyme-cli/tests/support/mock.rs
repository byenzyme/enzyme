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

        let amount = serde_json::from_slice::<serde_json::Value>(&request.body)
            .ok()
            .and_then(|v| v["amount"].as_f64())
            .unwrap_or(0.0);

        if state.deduct_402 || state.balance < amount {
            let body =
                serde_json::json!({ "credits": state.balance, "error": "Insufficient credits" });
            return template(&Reply::new(402, body.to_string()));
        }

        state.balance -= amount;
        let body = serde_json::json!({ "credits": state.balance });
        template(&Reply::ok_json(body.to_string()))
    }
}

struct FreeConfigResponder {
    state: Arc<Mutex<EnzymeApiState>>,
    calls: Arc<AtomicUsize>,
}

impl Respond for FreeConfigResponder {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut state = self.state.lock().unwrap();
        template(&state.free_config.next())
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
    pub const FREE_CONFIG_PATH: &'static str = "/enzyme-python/free-config";

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
        }));
        let deduct_calls = Arc::new(AtomicUsize::new(0));
        let free_config_calls = Arc::new(AtomicUsize::new(0));
        let logout_calls = Arc::new(AtomicUsize::new(0));

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
            server
        });

        Self {
            server,
            state,
            deduct_calls,
            free_config_calls,
            logout_calls,
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
        self.state.lock().unwrap().deduct_replies = Some(ReplyQueue {
            replies,
            served: 0,
        });
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

    /// Make the free-config endpoint answer 402 with `credits` remaining.
    pub fn set_free_config_402(&self, credits: f64) {
        let body = serde_json::json!({ "credits": credits }).to_string();
        self.state
            .lock()
            .unwrap()
            .free_config
            .set(vec![Reply::new(402, body)]);
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

    /// How many chat-completion requests were received.
    pub fn request_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    /// Raw bodies of every chat-completion request, in order.
    pub fn requests(&self) -> Vec<String> {
        self.state.lock().unwrap().requests.clone()
    }

    /// `Authorization` header of every chat-completion request, in order.
    /// Useful for asserting a key was (or was not) re-minted mid-run.
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
