use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use enzyme_core::kernel::Generator;
use enzyme_core::llm::{ConfigProvider, GenerationParams, Message};
use futures::StreamExt;

fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

struct HostedEnv {
    saved: Vec<(&'static str, Option<String>)>,
}

impl HostedEnv {
    fn install(free_config_url: &str) -> Self {
        let names = [
            "ENZYME_FREE_CONFIG_URL",
            "OPENAI_API_KEY",
            "OPENAI_BASE_URL",
            "OPENAI_MODEL",
            "OPENROUTER_API_KEY",
            "OPENROUTER_BASE_URL",
            "OPENROUTER_MODEL",
            "ENZYME_LOCAL_ENGINE",
        ];
        let saved = names
            .into_iter()
            .map(|name| (name, std::env::var(name).ok()))
            .collect();
        unsafe {
            for name in names {
                std::env::remove_var(name);
            }
            std::env::set_var("ENZYME_FREE_CONFIG_URL", free_config_url);
        }
        Self { saved }
    }
}

impl Drop for HostedEnv {
    fn drop(&mut self) {
        unsafe {
            for (name, value) in self.saved.drain(..) {
                match value {
                    Some(value) => std::env::set_var(name, value),
                    None => std::env::remove_var(name),
                }
            }
        }
    }
}

fn read_request(stream: &mut std::net::TcpStream) -> String {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        let read = stream.read(&mut buffer).unwrap();
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..read]);
        let Some(header_end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let headers = String::from_utf8_lossy(&bytes[..header_end + 4]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                line.split_once(':').and_then(|(name, value)| {
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
            })
            .unwrap_or(0);
        if bytes.len() >= header_end + 4 + content_length {
            break;
        }
    }
    String::from_utf8(bytes).unwrap()
}

fn respond(stream: &mut std::net::TcpStream, status: &str, content_type: &str, body: &str) {
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
}

struct HttpResponse {
    status: &'static str,
    content_type: &'static str,
    body: String,
}

struct RecordingServer {
    base_url: String,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl RecordingServer {
    fn start(handler: impl Fn(&str) -> HttpResponse + Send + Sync + 'static) -> RecordingServer {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&requests);
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = Arc::clone(&stop);
        let handler = Arc::new(handler);
        let thread = std::thread::spawn(move || {
            let mut connections = Vec::new();
            while !stopped.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        if stopped.load(Ordering::Acquire) {
                            break;
                        }
                        let captured = Arc::clone(&captured);
                        let handler = Arc::clone(&handler);
                        connections.push(std::thread::spawn(move || {
                            let request = read_request(&mut stream);
                            captured.lock().unwrap().push(request.clone());
                            let response = handler(&request);
                            respond(
                                &mut stream,
                                response.status,
                                response.content_type,
                                &response.body,
                            );
                        }));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("test server accept failed: {error}"),
                }
            }
            for connection in connections {
                connection.join().unwrap();
            }
        });
        Self {
            base_url: format!("http://{address}"),
            requests,
            stop,
            thread: Some(thread),
        }
    }

    fn finish(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}

impl Drop for RecordingServer {
    fn drop(&mut self) {
        self.finish();
    }
}

fn request_header<'a>(request: &'a str, name: &str) -> Option<&'a str> {
    request.lines().find_map(|line| {
        let (candidate, value) = line.split_once(':')?;
        candidate.eq_ignore_ascii_case(name).then(|| value.trim())
    })
}

fn request_model(request: &str) -> String {
    let body = request.split_once("\r\n\r\n").unwrap().1;
    let body: serde_json::Value = serde_json::from_str(body).unwrap();
    body["model"].as_str().unwrap().to_string()
}

fn completion_response(request: &str) -> HttpResponse {
    let body = request.split_once("\r\n\r\n").unwrap().1;
    let body: serde_json::Value = serde_json::from_str(body).unwrap();
    if body["stream"].as_bool() == Some(true) {
        HttpResponse {
            status: "200 OK",
            content_type: "text/event-stream",
            body: "data: {\"choices\":[{\"delta\":{\"content\":\"rotated\"}}]}\n\ndata: [DONE]\n\n"
                .to_string(),
        }
    } else {
        HttpResponse {
            status: "200 OK",
            content_type: "application/json",
            body: r#"{"choices":[{"message":{"content":"rotated"},"finish_reason":"stop"}]}"#
                .to_string(),
        }
    }
}

fn start_broker(old_base_url: String, new_base_url: String) -> (RecordingServer, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let captured_calls = Arc::clone(&calls);
    let server = RecordingServer::start(move |request| {
        assert!(request.starts_with("GET /margins/free-config "));
        assert!(request_header(request, "X-Margins-Bootstrap-Id").is_some());
        assert!(request_header(request, "Authorization").is_none());
        let call = captured_calls.fetch_add(1, Ordering::SeqCst);
        let (api_key, base_url, model) = if call == 0 {
            ("lease-old", old_base_url.as_str(), "old-model")
        } else {
            ("lease-new", new_base_url.as_str(), "new-model")
        };
        HttpResponse {
            status: "200 OK",
            content_type: "application/json",
            body: serde_json::json!({
                "api_key": api_key,
                "base_url": format!("{base_url}/v1"),
                "model": model,
                "expires_at": 4_102_444_800_i64,
            })
            .to_string(),
        }
    });
    (server, calls)
}

fn rejected_response() -> HttpResponse {
    HttpResponse {
        status: "401 Unauthorized",
        content_type: "application/json",
        body: r#"{"error":"expired lease"}"#.to_string(),
    }
}

async fn complete(generator: &Generator, streaming: bool) -> String {
    let model = generator.model();
    let messages = vec![Message::text("user", "rotate")];
    if streaming {
        let stream = generator
            .client()
            .chat_completion_streaming(&model, messages, GenerationParams::default())
            .await
            .unwrap();
        stream
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .map(Result::unwrap)
            .collect()
    } else {
        generator
            .client()
            .chat_completion(&model, messages, GenerationParams::default())
            .await
            .unwrap()
    }
}

fn assert_requests(
    requests: &Arc<Mutex<Vec<String>>>,
    expected_count: usize,
    key: &str,
    model: &str,
) {
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), expected_count, "requests: {requests:#?}");
    let authorization = format!("Bearer {key}");
    for request in requests.iter() {
        assert_eq!(
            request_header(request, "Authorization"),
            Some(authorization.as_str())
        );
        assert_eq!(request_model(request), model);
    }
}

async fn assert_sequential_rotation(streaming: bool) {
    let _lock = env_lock().lock().unwrap();
    let mut old = RecordingServer::start(|_| rejected_response());
    let mut new = RecordingServer::start(completion_response);
    let (mut broker, broker_calls) = start_broker(old.base_url.clone(), new.base_url.clone());
    let _env = HostedEnv::install(&format!("{}/margins/free-config", broker.base_url));
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join("config.toml"),
        "[llm]\nmode = \"hosted\"\n",
    )
    .unwrap();

    let generator = Generator::resolve_in(home.path()).await.unwrap();
    assert_eq!(generator.model(), "old-model");
    assert_eq!(complete(&generator, streaming).await, "rotated");
    assert_eq!(generator.model(), "new-model");
    assert_eq!(complete(&generator, streaming).await, "rotated");

    old.finish();
    new.finish();
    broker.finish();
    assert_eq!(broker_calls.load(Ordering::SeqCst), 2);
    assert_requests(&old.requests, 1, "lease-old", "old-model");
    assert_requests(&new.requests, 2, "lease-new", "new-model");
    let cache: serde_json::Value =
        serde_json::from_slice(&std::fs::read(home.path().join("llm-config-cache.json")).unwrap())
            .unwrap();
    assert_eq!(cache["api_key"], "lease-new");
    assert_eq!(cache["base_url"], format!("{}/v1", new.base_url));
    assert_eq!(cache["model"], "new-model");
}

fn start_near_expiry_server() -> (String, std::thread::JoinHandle<()>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let handle = std::thread::spawn(move || {
        for call in 0..2 {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request(&mut stream);
            assert!(request.starts_with("GET /margins/free-config "));
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs();
            let body = serde_json::json!({
                "api_key": if call == 0 { "near-expiry" } else { "replacement" },
                "base_url": format!("http://{address}/v1"),
                "model": if call == 0 { "near-expiry-model" } else { "replacement-model" },
                "expires_at": if call == 0 {
                    (now + 30) as i64
                } else {
                    (now + 3_600) as i64
                },
            })
            .to_string();
            respond(&mut stream, "200 OK", "application/json", &body);
        }
    });
    (format!("http://{address}/margins/free-config"), handle)
}

#[tokio::test]
async fn explicit_home_env_credentials_win_over_persisted_local_mode() {
    let _lock = env_lock().lock().unwrap();
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join("config.toml"),
        "[llm]\nmode = \"local\"\nlocal_model = \"missing-model\"\n",
    )
    .unwrap();

    let previous_key = std::env::var("OPENAI_API_KEY").ok();
    let previous_model = std::env::var("OPENAI_MODEL").ok();
    unsafe {
        std::env::set_var("OPENAI_API_KEY", "fixture-key");
        std::env::set_var("OPENAI_MODEL", "fixture-model");
    }
    let result = Generator::resolve_in(home.path()).await;
    unsafe {
        match previous_key {
            Some(value) => std::env::set_var("OPENAI_API_KEY", value),
            None => std::env::remove_var("OPENAI_API_KEY"),
        }
        match previous_model {
            Some(value) => std::env::set_var("OPENAI_MODEL", value),
            None => std::env::remove_var("OPENAI_MODEL"),
        }
    }

    let generator = result.unwrap();
    assert!(matches!(generator.provider(), ConfigProvider::Env));
    assert_eq!(generator.model(), "fixture-model");
    assert!(!home.path().join("bootstrap.json").exists());
}

#[tokio::test]
async fn sequential_non_streaming_calls_publish_rotated_endpoint_key_and_model() {
    assert_sequential_rotation(false).await;
}

#[tokio::test]
async fn sequential_streaming_calls_publish_rotated_endpoint_key_and_model() {
    assert_sequential_rotation(true).await;
}

#[tokio::test]
async fn concurrent_non_streaming_401s_singleflight_one_broker_refresh() {
    const CALLERS: usize = 8;
    let _lock = env_lock().lock().unwrap();
    let gate = Arc::new((Mutex::new(0_usize), Condvar::new()));
    let handler_gate = Arc::clone(&gate);
    let mut old = RecordingServer::start(move |_| {
        let (count, ready) = &*handler_gate;
        let mut count = count.lock().unwrap();
        *count += 1;
        if *count == CALLERS {
            ready.notify_all();
        }
        while *count < CALLERS {
            let (next, timeout) = ready.wait_timeout(count, Duration::from_secs(5)).unwrap();
            count = next;
            assert!(
                !timeout.timed_out(),
                "concurrent callers did not reach old endpoint"
            );
        }
        rejected_response()
    });
    let mut new = RecordingServer::start(completion_response);
    let (mut broker, broker_calls) = start_broker(old.base_url.clone(), new.base_url.clone());
    let _env = HostedEnv::install(&format!("{}/margins/free-config", broker.base_url));
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join("config.toml"),
        "[llm]\nmode = \"hosted\"\n",
    )
    .unwrap();
    let generator = Arc::new(Generator::resolve_in(home.path()).await.unwrap());

    let results = futures::future::join_all((0..CALLERS).map(|_| {
        let generator = Arc::clone(&generator);
        async move { complete(&generator, false).await }
    }))
    .await;
    assert!(results.iter().all(|result| result == "rotated"));
    assert_eq!(generator.model(), "new-model");
    assert_eq!(complete(&generator, false).await, "rotated");

    old.finish();
    new.finish();
    broker.finish();
    assert_eq!(broker_calls.load(Ordering::SeqCst), 2);
    assert_requests(&old.requests, CALLERS, "lease-old", "old-model");
    assert_requests(&new.requests, CALLERS + 1, "lease-new", "new-model");
}

#[tokio::test]
async fn resolve_in_refetches_cache_inside_lease_safety_margin() {
    let _lock = env_lock().lock().unwrap();
    let (broker_url, server) = start_near_expiry_server();
    let _env = HostedEnv::install(&broker_url);
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join("config.toml"),
        "[llm]\nmode = \"hosted\"\n",
    )
    .unwrap();

    let first = Generator::resolve_in(home.path()).await.unwrap();
    assert_eq!(first.model(), "near-expiry-model");
    let second = Generator::resolve_in(home.path()).await.unwrap();
    assert_eq!(second.model(), "replacement-model");
    server.join().unwrap();

    let cache: serde_json::Value =
        serde_json::from_slice(&std::fs::read(home.path().join("llm-config-cache.json")).unwrap())
            .unwrap();
    assert_eq!(cache["api_key"], "replacement");
}
