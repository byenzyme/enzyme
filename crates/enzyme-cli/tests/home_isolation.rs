//! Launch contract for hosts that run the CLI with their own `ENZYME_HOME`
//! (Margins sets `ENZYME_HOME=$MARGINS_HOME`): a named-workspace init, refresh
//! (with its detached worker), search, and status write nothing outside
//! `ENZYME_HOME`, never touch the user's own `~/.enzyme`, and with `--llm env`
//! reach no network except the configured `OPENAI_BASE_URL`.

mod support;

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime};

use fs2::FileExt;
use support::mock::MockLlm;

const CATALYSTS: &str = r#"{"catalysts":["Which migration tradeoff preserves source evidence?","What rejected alternative changes the audit decision?","Which constraint made the migration reversible?"],"topicNames":["migration evidence","audit alternatives","reversible migration"]}"#;

/// Accepts and counts every connection: stands in for every Enzyme endpoint
/// (hosted config, API, update checks) so any request is detected offline.
struct Sentinel {
    url: String,
    hits: Arc<AtomicUsize>,
}

impl Sentinel {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                counter.fetch_add(1, Ordering::SeqCst);
                if let Ok(mut stream) = stream {
                    let mut buf = [0u8; 1024];
                    let _ = stream.read(&mut buf);
                }
            }
        });
        Self { url, hits }
    }
}

type Snapshot = BTreeMap<PathBuf, (u64, Option<SystemTime>)>;

fn snapshot(root: &Path, skip: &[&Path], out: &mut Snapshot) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if skip.iter().any(|skip| path.starts_with(skip)) {
            continue;
        }
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.is_dir() {
            out.insert(path.clone(), (0, None));
            snapshot(&path, skip, out);
        } else {
            out.insert(path, (meta.len(), meta.modified().ok()));
        }
    }
}

fn text(output: &Output) -> String {
    format!(
        "exit {:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn json(output: &Output) -> serde_json::Value {
    assert!(output.status.success(), "{}", text(output));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout
        .lines()
        .rev()
        .find(|line| line.starts_with('{'))
        .unwrap_or_else(|| panic!("no JSON on stdout\n{}", text(output)));
    serde_json::from_str(line).unwrap()
}

#[test]
fn named_workspace_run_writes_only_inside_enzyme_home() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path().canonicalize().unwrap();
    let os_home = root.join("os-home");
    let tmpdir = root.join("tmp");
    let enzyme_home = root.join("enzyme-home");
    let notes = root.join("notes");
    for dir in [&os_home, &tmpdir, &enzyme_home.join("configs"), &notes] {
        fs::create_dir_all(dir).unwrap();
    }

    // The user's own Enzyme home, which a host-scoped run must never read
    // credentials from or write to.
    let canary = os_home.join(".enzyme");
    fs::create_dir_all(&canary).unwrap();
    fs::write(
        canary.join("auth.json"),
        r#"{"token":"canary-token","email":"canary@example.com"}"#,
    )
    .unwrap();
    fs::write(canary.join("config.toml"), "[update]\nauto = true\n").unwrap();

    for i in 0..6 {
        fs::write(
            notes.join(format!("note-{i}.md")),
            format!(
                "---\ntags: [focus]\n---\n# Migration evidence {i}\n{}",
                "The team chose a reversible migration with auditable evidence. ".repeat(20)
            ),
        )
        .unwrap();
    }
    fs::write(
        enzyme_home.join("configs/host.enzyme"),
        format!(
            "workspace \"meeting\" {{ source markdown \"notes\" {{ path \"{}\" }} }}\n",
            notes.display()
        ),
    )
    .unwrap();

    let llm = MockLlm::start();
    llm.set_ok_content(CATALYSTS);
    let sentinel = Sentinel::start();
    let shared_tmp_cache = Path::new("/tmp/enzyme-api-cache");
    let shared_tmp_before = fs::metadata(shared_tmp_cache)
        .ok()
        .map(|meta| (meta.len(), meta.modified().ok()));

    let run = |args: &[&str]| -> Output {
        Command::new(env!("CARGO_BIN_EXE_enzyme"))
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &os_home)
            .env("TMPDIR", &tmpdir)
            .env("ENZYME_HOME", &enzyme_home)
            .env("OPENAI_API_KEY", "sk-test-env-key")
            .env("OPENAI_BASE_URL", llm.base_url())
            .env("OPENAI_MODEL", "test-model")
            .env("ENZYME_FREE_CONFIG_URL", format!("{}/free-config", sentinel.url))
            .env("ENZYME_API_URL", &sentinel.url)
            .env("ENZYME_UPDATE_API_BASE", &sentinel.url)
            .env("ENZYME_UPDATE_RELEASE_BASE", &sentinel.url)
            .current_dir(&root)
            .args(["--workspace", "meeting"])
            .args(args)
            .output()
            .unwrap()
    };

    let mut before = Snapshot::new();
    snapshot(&root, &[&enzyme_home], &mut before);

    let init = json(&run(&["init", "--quiet", "--llm", "env"]));
    assert_ne!(init["status"], "failed", "{init:#}");
    assert!(llm.request_count() > 0, "init never reached the mock LLM");
    fs::write(
        notes.join("note-new.md"),
        "# Later note\nA new reversible migration decision with evidence.\n",
    )
    .unwrap();
    json(&run(&["refresh", "--quiet", "--llm", "env", "--full"]));

    // Wait for any detached catalyst worker the refresh spawned.
    let store = enzyme_home.join("workspaces/meeting");
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        let worker_idle = fs::File::open(store.join("refresh.lock"))
            .map(|lock| {
                let free = lock.try_lock_exclusive().is_ok();
                if free {
                    let _ = fs2::FileExt::unlock(&lock);
                }
                free
            })
            .unwrap_or(true);
        let status = json(&run(&["status", "--json"]));
        if worker_idle && status["index"]["epoch"]["build_epoch_id"].is_null() {
            break;
        }
        assert!(Instant::now() < deadline, "background worker never finished");
        std::thread::sleep(Duration::from_millis(250));
    }

    let search = json(&run(&["search", "migration evidence", "--json"]));
    assert_eq!(search["schema"], "enzyme.search.v1");
    let status = json(&run(&["status", "--json"]));
    assert_eq!(status["initialized"], true, "{status:#}");
    assert_eq!(status["workspace"], "meeting");

    let mut after = Snapshot::new();
    snapshot(&root, &[&enzyme_home], &mut after);
    // The note this test wrote is the only expected change outside the home.
    after.remove(&notes.join("note-new.md"));
    let changed: Vec<_> = after
        .iter()
        .filter(|(path, meta)| before.get(*path) != Some(meta))
        .map(|(path, _)| path.display().to_string())
        .chain(
            before
                .keys()
                .filter(|path| !after.contains_key(*path))
                .map(|path| format!("{} (removed)", path.display())),
        )
        .collect();
    assert!(
        changed.is_empty(),
        "wrote outside ENZYME_HOME (HOME, TMPDIR, notes, cwd): {changed:#?}"
    );
    assert_eq!(
        fs::read_to_string(canary.join("auth.json")).unwrap(),
        r#"{"token":"canary-token","email":"canary@example.com"}"#
    );
    assert!(store.join("enzyme.db").is_file());
    assert!(!notes.join(".enzyme").exists());
    assert_eq!(
        fs::metadata(shared_tmp_cache)
            .ok()
            .map(|meta| (meta.len(), meta.modified().ok())),
        shared_tmp_before,
        "touched the shared /tmp/enzyme-api-cache"
    );
    assert_eq!(
        sentinel.hits.load(Ordering::SeqCst),
        0,
        "--llm env contacted an Enzyme endpoint (hosted config, API, or update check)"
    );
}
