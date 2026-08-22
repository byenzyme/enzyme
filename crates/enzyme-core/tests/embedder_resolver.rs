use std::io::{Read, Write};
use std::sync::{Arc, Mutex, OnceLock};
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

fn start_rotation_server(
    streaming: bool,
) -> (String, Arc<Mutex<Vec<String>>>, std::thread::JoinHandle<()>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&requests);
    let handle = std::thread::spawn(move || {
        let mut broker_calls = 0;
        for _ in 0..4 {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request(&mut stream);
            captured.lock().unwrap().push(request.clone());
            let first_line = request.lines().next().unwrap_or_default();
            if first_line.contains("/margins/free-config") {
                broker_calls += 1;
                let key = if broker_calls == 1 {
                    "lease-old"
                } else {
                    "lease-new"
                };
                let body = serde_json::json!({
                    "api_key": key,
                    "base_url": format!("http://{address}/v1"),
                    "model": "rotation-model",
                    "expires_at": 4_102_444_800_i64,
                })
                .to_string();
                respond(&mut stream, "200 OK", "application/json", &body);
            } else if request
                .to_ascii_lowercase()
                .contains("\r\nauthorization: bearer lease-old\r\n")
            {
                respond(
                    &mut stream,
                    "401 Unauthorized",
                    "application/json",
                    r#"{"error":"expired lease"}"#,
                );
            } else if streaming {
                respond(
                    &mut stream,
                    "200 OK",
                    "text/event-stream",
                    r#"data: {"choices":[{"delta":{"content":"rotated"}}]}

data: [DONE]

"#,
                );
            } else {
                respond(
                    &mut stream,
                    "200 OK",
                    "application/json",
                    r#"{"choices":[{"message":{"content":"rotated"},"finish_reason":"stop"}]}"#,
                );
            }
        }
    });
    (
        format!("http://{address}/margins/free-config"),
        requests,
        handle,
    )
}

async fn assert_owned_401_rotation(streaming: bool) {
    let _lock = env_lock().lock().unwrap();
    let (broker_url, requests, server) = start_rotation_server(streaming);
    let _env = HostedEnv::install(&broker_url);
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join("config.toml"),
        r#"[llm]
mode = "hosted"
"#,
    )
    .unwrap();

    let generator = Generator::resolve_in(home.path()).await.unwrap();
    let messages = vec![Message::text("user", "rotate")];
    if streaming {
        let stream = generator
            .client()
            .chat_completion_streaming(generator.model(), messages, GenerationParams::default())
            .await
            .unwrap();
        let chunks: Vec<_> = stream.collect::<Vec<_>>().await;
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].as_ref().unwrap(), "rotated");
    } else {
        let response = generator
            .client()
            .chat_completion(generator.model(), messages, GenerationParams::default())
            .await
            .unwrap();
        assert_eq!(response, "rotated");
    }
    server.join().unwrap();

    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 4);
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.starts_with("GET /margins/free-config "))
            .count(),
        2,
    );
    let broker_requests: Vec<_> = requests
        .iter()
        .filter(|request| request.starts_with("GET /margins/free-config "))
        .collect();
    assert!(broker_requests.iter().all(|request| {
        request
            .to_ascii_lowercase()
            .contains("\r\nx-margins-bootstrap-id: ")
    }));
    assert!(
        broker_requests
            .iter()
            .all(|request| { !request.to_ascii_lowercase().contains("\r\nauthorization:") })
    );
    assert!(requests.iter().any(|request| {
        request
            .to_ascii_lowercase()
            .contains("\r\nauthorization: bearer lease-old\r\n")
    }));
    assert!(requests.iter().any(|request| {
        request
            .to_ascii_lowercase()
            .contains("\r\nauthorization: bearer lease-new\r\n")
    }));

    let cache: serde_json::Value =
        serde_json::from_slice(&std::fs::read(home.path().join("llm-config-cache.json")).unwrap())
            .unwrap();
    assert_eq!(cache["api_key"], "lease-new");
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
async fn resolve_in_retries_non_streaming_401_with_home_owned_rotation() {
    assert_owned_401_rotation(false).await;
}

#[tokio::test]
async fn resolve_in_retries_streaming_401_with_home_owned_rotation() {
    assert_owned_401_rotation(true).await;
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
