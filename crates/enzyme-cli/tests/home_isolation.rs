//! Launch contract for hosts that run the CLI with their own `ENZYME_HOME`
//! (Margins sets `ENZYME_HOME=$MARGINS_HOME`): a named-workspace init, refresh
//! (with its detached worker), search, and status write nothing outside
//! `ENZYME_HOME`, never read or write the user's own `~/.enzyme` (made
//! unreadable here), and with `--llm env` reach no network except the
//! configured `OPENAI_BASE_URL`. In the default (hosted) mode the lease cache
//! lands in `$ENZYME_HOME/api-cache`.

mod support;

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Output};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime};

use fs2::FileExt;
use support::mock::{MockEnzymeApi, MockLlm};

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

const CANARY_AUTH: &str = r#"{"token":"canary-token","email":"canary@example.com"}"#;

/// Separate HOME, TMPDIR, ENZYME_HOME, and notes folder under one temp root,
/// with a named workspace over the notes and a mode-000 canary `~/.enzyme`:
/// any read of it fails the run.
struct Isolated {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    os_home: PathBuf,
    tmpdir: PathBuf,
    enzyme_home: PathBuf,
    notes: PathBuf,
    canary: PathBuf,
    canary_mtime: SystemTime,
    sentinel: Sentinel,
}

impl Isolated {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let os_home = root.join("os-home");
        let tmpdir = root.join("tmp");
        let enzyme_home = root.join("enzyme-home");
        let notes = root.join("notes");
        for dir in [&os_home, &tmpdir, &enzyme_home.join("configs"), &notes] {
            fs::create_dir_all(dir).unwrap();
        }
        let canary = os_home.join(".enzyme");
        fs::create_dir_all(&canary).unwrap();
        fs::write(canary.join("auth.json"), CANARY_AUTH).unwrap();
        fs::write(canary.join("config.toml"), "[update]\nauto = true\n").unwrap();
        fs::set_permissions(&canary, fs::Permissions::from_mode(0o000)).unwrap();
        let canary_mtime = fs::metadata(&canary).unwrap().modified().unwrap();
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
        Self {
            _tmp: tmp,
            root,
            os_home,
            tmpdir,
            enzyme_home,
            notes,
            canary,
            canary_mtime,
            sentinel: Sentinel::start(),
        }
    }

    /// Every Enzyme endpoint defaults to the sentinel; callers override.
    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_enzyme"));
        command
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &self.os_home)
            .env("TMPDIR", &self.tmpdir)
            .env("ENZYME_HOME", &self.enzyme_home)
            .env("ENZYME_FREE_CONFIG_URL", format!("{}/free-config", self.sentinel.url))
            .env("ENZYME_API_URL", &self.sentinel.url)
            .env("ENZYME_UPDATE_API_BASE", &self.sentinel.url)
            .env("ENZYME_UPDATE_RELEASE_BASE", &self.sentinel.url)
            .current_dir(&self.root)
            .args(["--workspace", "meeting"])
            .args(args);
        command
    }

    fn snapshot(&self) -> Snapshot {
        let mut out = Snapshot::new();
        snapshot(&self.root, &[&self.enzyme_home, &self.canary], &mut out);
        out
    }

    fn store(&self) -> PathBuf {
        self.enzyme_home.join("workspaces/meeting")
    }

    /// Nothing changed outside ENZYME_HOME (besides `expected`), the canary
    /// was neither modified nor (being unreadable) read, and the shared
    /// `/tmp/enzyme-api-cache` is untouched.
    fn assert_contained(&self, before: &Snapshot, expected: &[PathBuf], shared_tmp: &Metadata) {
        let mut after = self.snapshot();
        for path in expected {
            after.remove(path);
        }
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
            fs::metadata(&self.canary).unwrap().modified().unwrap(),
            self.canary_mtime,
            "~/.enzyme was modified"
        );
        fs::set_permissions(&self.canary, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(fs::read_to_string(self.canary.join("auth.json")).unwrap(), CANARY_AUTH);
        assert_eq!(
            fs::read_dir(&self.canary).unwrap().count(),
            2,
            "files appeared in ~/.enzyme"
        );
        fs::set_permissions(&self.canary, fs::Permissions::from_mode(0o000)).unwrap();
        assert!(self.store().join("enzyme.db").is_file());
        assert!(!self.notes.join(".enzyme").exists());
        assert_eq!(&shared_tmp_metadata(), shared_tmp, "touched /tmp/enzyme-api-cache");
    }
}

impl Drop for Isolated {
    fn drop(&mut self) {
        // Let the temp dir clean up.
        let _ = fs::set_permissions(&self.canary, fs::Permissions::from_mode(0o755));
    }
}

type Metadata = Option<(u64, Option<SystemTime>)>;

fn shared_tmp_metadata() -> Metadata {
    fs::metadata("/tmp/enzyme-api-cache")
        .ok()
        .map(|meta| (meta.len(), meta.modified().ok()))
}

#[test]
fn named_workspace_run_writes_only_inside_enzyme_home() {
    let env = Isolated::new();
    let llm = MockLlm::start();
    llm.set_ok_content(CATALYSTS);
    let shared_tmp = shared_tmp_metadata();
    let run = |args: &[&str]| -> Output {
        env.command(args)
            .env("OPENAI_API_KEY", "sk-test-env-key")
            .env("OPENAI_BASE_URL", llm.base_url())
            .env("OPENAI_MODEL", "test-model")
            .output()
            .unwrap()
    };

    let before = env.snapshot();
    let init = json(&run(&["init", "--quiet", "--llm", "env"]));
    assert_ne!(init["status"], "failed", "{init:#}");
    assert!(llm.request_count() > 0, "init never reached the mock LLM");
    let new_note = env.notes.join("note-new.md");
    fs::write(
        &new_note,
        "# Later note\nA new reversible migration decision with evidence.\n",
    )
    .unwrap();
    json(&run(&["refresh", "--quiet", "--llm", "env", "--full"]));

    // Wait for any detached catalyst worker the refresh spawned.
    let store = env.store();
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

    env.assert_contained(&before, &[new_note], &shared_tmp);
    assert_eq!(
        env.sentinel.hits.load(Ordering::SeqCst),
        0,
        "--llm env contacted an Enzyme endpoint (hosted config, API, or update check)"
    );
}

/// Default mode resolves hosted generation through the free-config endpoint
/// (the test server); its lease cache must land in `$ENZYME_HOME/api-cache`.
#[test]
fn hosted_default_run_caches_lease_inside_enzyme_home() {
    let env = Isolated::new();
    let llm = MockLlm::start();
    llm.set_ok_content(CATALYSTS);
    let api = MockEnzymeApi::start();
    api.set_free_config("sk-hosted-lease", &llm.base_url(), "test-model");
    let shared_tmp = shared_tmp_metadata();

    let before = env.snapshot();
    let output = env
        .command(&["init", "--quiet"])
        .env("ENZYME_FREE_CONFIG_URL", api.free_config_url())
        .env("ENZYME_API_URL", api.base_url())
        .output()
        .unwrap();
    let init = json(&output);
    assert_ne!(init["status"], "failed", "{init:#}\n{}", text(&output));
    assert!(api.free_config_calls() > 0, "never fetched the hosted lease");
    assert!(llm.request_count() > 0, "hosted generation never reached the mock LLM");
    let cache = env.enzyme_home.join("api-cache");
    assert!(
        fs::read_to_string(&cache)
            .unwrap_or_default()
            .contains("sk-hosted-lease"),
        "hosted lease not cached at {}",
        cache.display()
    );

    env.assert_contained(&before, &[], &shared_tmp);
    assert_eq!(
        env.sentinel.hits.load(Ordering::SeqCst),
        0,
        "init contacted an endpoint other than the configured Enzyme API"
    );
}
