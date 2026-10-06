//! Black-box tests for fire-and-forget binary auto-update.
//!
//! Harness: `crates/enzyme-cli/tests/support/` plus a local release server in
//! this file. Every scenario drives the real `enzyme` binary as a subprocess;
//! network access is either routed to the local mock or to the fixture's dead
//! port defaults.
//!
//! # Contract under test
//!
//! An executable installed at `~/.local/bin/enzyme` is the only self-swappable
//! channel. It discovers a release, verifies the archive checksum, stages the
//! replacement on the same filesystem, atomically swaps it, and records a
//! durable outcome in the vault DB. Policy guards, lock contention, and
//! failures must leave the installed bytes untouched. Agent hot paths expose
//! only the two-state refresh contract (`update.status` ok or action_required);
//! durable outcomes live in vault DB config keys and `enzyme status` for debugging.

mod support;

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use enzyme_core::db::Database;
use flate2::Compression;
use flate2::write::GzEncoder;
use fs2::FileExt;
use sha2::{Digest, Sha256};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use support::fixture::{CliOutput, VaultFixture};
use support::mock::MockLlm;

const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const CHECKED_AT_KEY: &str = "update.checked_at_ms";
const LAST_OUTCOME_KEY: &str = "update.last_outcome";

fn serial_test() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn mock_runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("build release-server runtime")
    })
}

fn platform_target() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "macos-arm64",
        ("macos", "x86_64") => "macos-x86_64",
        ("linux", "x86_64") => "linux-x86_64",
        ("linux", "aarch64") => "linux-arm64",
        pair => panic!("unsupported test platform {pair:?}"),
    }
}

fn newer_patch() -> String {
    let mut parts = CURRENT_VERSION.split('.');
    let major: u64 = parts.next().unwrap().parse().unwrap();
    let minor: u64 = parts.next().unwrap().parse().unwrap();
    let patch: u64 = parts.next().unwrap().parse().unwrap();
    format!("{major}.{minor}.{}", patch + 1)
}

fn next_major() -> String {
    let major: u64 = CURRENT_VERSION.split('.').next().unwrap().parse().unwrap();
    format!("{}.0.0", major + 1)
}

fn replacement_script(version: &str) -> Vec<u8> {
    format!("#!/bin/sh\nprintf 'enzyme-stub {version}\\n'\n").into_bytes()
}

fn release_archive(version: &str) -> Vec<u8> {
    let encoder = GzEncoder::new(Vec::new(), Compression::default());
    let mut archive = tar::Builder::new(encoder);
    let body = replacement_script(version);
    let mut header = tar::Header::new_gnu();
    header.set_path("enzyme").unwrap();
    header.set_size(body.len() as u64);
    header.set_mode(0o755);
    header.set_cksum();
    archive.append(&header, body.as_slice()).unwrap();
    archive.into_inner().unwrap().finish().unwrap()
}

fn archive_with_unexpected_entry() -> Vec<u8> {
    let encoder = GzEncoder::new(Vec::new(), Compression::default());
    let mut archive = tar::Builder::new(encoder);
    let body = b"not part of the release contract";
    let mut header = tar::Header::new_gnu();
    header.set_path("README-surprise").unwrap();
    header.set_size(body.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    archive.append(&header, body.as_slice()).unwrap();
    archive.into_inner().unwrap().finish().unwrap()
}

#[cfg(unix)]
fn archive_with_symlinked_enzyme(target: &PathBuf) -> Vec<u8> {
    let encoder = GzEncoder::new(Vec::new(), Compression::default());
    let mut archive = tar::Builder::new(encoder);
    let mut header = tar::Header::new_gnu();
    header.set_path("enzyme").unwrap();
    header.set_entry_type(tar::EntryType::Symlink);
    header.set_link_name(target).unwrap();
    header.set_size(0);
    header.set_mode(0o755);
    header.set_cksum();
    archive.append(&header, std::io::empty()).unwrap();
    archive.into_inner().unwrap().finish().unwrap()
}

/// Local stand-in for both `api.enzyme.garden` and GitHub Releases.
struct MockReleaseServer {
    server: MockServer,
}

impl MockReleaseServer {
    fn start(latest: &str, tamper_checksum: bool) -> Self {
        Self::start_with_archive(latest, release_archive(latest), tamper_checksum, None)
    }

    fn start_with_archive(
        latest: &str,
        archive: Vec<u8>,
        tamper_checksum: bool,
        asset_delay: Option<Duration>,
    ) -> Self {
        let server = mock_runtime().block_on(MockServer::start());
        let checksum = if tamper_checksum {
            format!("{}  enzyme-{}.tar.gz\n", "0".repeat(64), platform_target())
        } else {
            format!(
                "{:x}  enzyme-{}.tar.gz\n",
                Sha256::digest(&archive),
                platform_target()
            )
        };
        let asset_path = format!(
            "/releases/download/v{latest}/enzyme-{}.tar.gz",
            platform_target()
        );
        let checksum_path = format!("{asset_path}.sha256");
        let redirect = format!("{}/releases/tag/v{latest}", server.uri());

        mock_runtime().block_on(async {
            Mock::given(method("GET"))
                .and(path("/version"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "latest": latest,
                    "minimum_supported": CURRENT_VERSION,
                })))
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path("/releases/latest"))
                .respond_with(
                    ResponseTemplate::new(302).insert_header("location", redirect.as_str()),
                )
                .mount(&server)
                .await;
            let mut asset_response = ResponseTemplate::new(200).set_body_bytes(archive);
            if let Some(delay) = asset_delay {
                asset_response = asset_response.set_delay(delay);
            }
            Mock::given(method("GET"))
                .and(path(asset_path))
                .respond_with(asset_response)
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path(checksum_path))
                .respond_with(ResponseTemplate::new(200).set_body_string(checksum))
                .mount(&server)
                .await;
        });
        Self { server }
    }

    fn base_url(&self) -> String {
        self.server.uri()
    }

    fn request_count(&self) -> usize {
        mock_runtime()
            .block_on(self.server.received_requests())
            .expect("request recording enabled")
            .len()
    }
}

struct InstalledBinary {
    os_home: PathBuf,
    path: PathBuf,
    original_len: u64,
    original_digest: [u8; 32],
}

impl InstalledBinary {
    fn copy_from_test_binary(fixture: &VaultFixture) -> Self {
        let os_home = fixture.root().join("user-home");
        let bin_dir = os_home.join(".local/bin");
        fs::create_dir_all(&bin_dir).expect("create fixture ~/.local/bin");
        let path = bin_dir.join("enzyme");
        let test_binary = PathBuf::from(env!("CARGO_BIN_EXE_enzyme"));
        fs::copy(&test_binary, &path).expect("copy enzyme test binary");
        Self {
            os_home,
            path,
            original_len: fs::metadata(&test_binary).unwrap().len(),
            original_digest: test_binary_digest(),
        }
    }

    fn assert_unchanged(&self) {
        assert_eq!(
            fs::metadata(&self.path)
                .expect("stat installed enzyme")
                .len(),
            self.original_len,
            "failed or guarded update must preserve the installed binary length"
        );
        assert_eq!(
            sha256_path(&self.path),
            self.original_digest,
            "failed or guarded update must preserve the installed binary byte-for-byte"
        );
    }

    fn assert_no_partial_files(&self) {
        let mut entries = fs::read_dir(self.path.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        entries.sort();
        assert_eq!(entries, ["enzyme"], "staging files must be cleaned up");
    }
}

fn sha256_path(path: &PathBuf) -> [u8; 32] {
    let mut file = File::open(path).expect("open file for SHA-256");
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer).expect("read file for SHA-256");
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    hasher.finalize().into()
}

fn test_binary_digest() -> [u8; 32] {
    static DIGEST: OnceLock<[u8; 32]> = OnceLock::new();
    *DIGEST.get_or_init(|| sha256_path(&PathBuf::from(env!("CARGO_BIN_EXE_enzyme"))))
}

fn vault_db(fixture: &VaultFixture) -> Database {
    Database::open(fixture.vault_enzyme_dir().join("enzyme.db")).expect("open fixture vault DB")
}

fn create_vault_db(fixture: &VaultFixture) {
    fs::create_dir_all(fixture.vault_enzyme_dir()).expect("create vault .enzyme directory");
    drop(vault_db(fixture));
}

fn outcome(fixture: &VaultFixture) -> serde_json::Value {
    let raw = vault_db(fixture)
        .get_config(LAST_OUTCOME_KEY)
        .expect("read update outcome")
        .expect("update outcome was not recorded");
    serde_json::from_str(&raw).expect("parse update outcome")
}

fn run_installed(
    fixture: &VaultFixture,
    installed: &InstalledBinary,
    server: &MockReleaseServer,
    args: &[&str],
) -> CliOutput {
    fixture
        .cli()
        .executable(&installed.path)
        .env("HOME", installed.os_home.display().to_string())
        .env("ENZYME_UPDATE_API_BASE", server.base_url())
        .env("ENZYME_UPDATE_RELEASE_BASE", server.base_url())
        .args(args.iter().copied())
        .run()
}

fn assert_stub(installed: &InstalledBinary, version: &str) {
    let output = Command::new(&installed.path)
        .output()
        .expect("run swapped enzyme stub");
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!("enzyme-stub {version}\n")
    );
}

fn catalyst_payload() -> String {
    serde_json::json!({
        "catalysts": [
            "the moment a plan survives contact with the actual hardware",
            "keeping a running tally of what the surprise costs actually were",
            "returning to a half-finished build after the season changed",
        ],
        "topic_names": [
            "plans meeting hardware",
            "surprise costs",
            "seasonal returns",
        ],
    })
    .to_string()
}

fn init_vault() -> (VaultFixture, MockLlm) {
    let llm = MockLlm::start();
    llm.set_ok_content(&catalyst_payload());
    let fixture = VaultFixture::new();
    fixture
        .cli()
        .arg("init")
        .arg("--use-env-llm")
        .env_llm(&llm)
        .run()
        .assert_success();
    (fixture, llm)
}

struct SqliteWriteLock {
    child: Child,
}

impl SqliteWriteLock {
    fn acquire(database: &PathBuf) -> Self {
        let mut child = Command::new("sqlite3")
            .arg(database)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("start sqlite3 lock holder");
        let stdin = child.stdin.as_mut().unwrap();
        writeln!(stdin, "BEGIN EXCLUSIVE;").unwrap();
        writeln!(stdin, ".print lock-ready").unwrap();
        stdin.flush().unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let mut ready = String::new();
        stdout.read_line(&mut ready).unwrap();
        assert_eq!(ready.trim(), "lock-ready");
        Self { child }
    }
}

impl Drop for SqliteWriteLock {
    fn drop(&mut self) {
        if let Some(stdin) = self.child.stdin.as_mut() {
            let _ = writeln!(stdin, "ROLLBACK;");
            let _ = writeln!(stdin, ".quit");
            let _ = stdin.flush();
        }
        let _ = self.child.wait();
    }
}

/// Scenario 1: `update --check` reports the discoverable patch release and
/// classifies an invocation from the fixture's `~/.local/bin` as swappable.
#[test]
fn check_reports_available_patch_and_curl_installer_channel() {
    let _serial = serial_test();
    let fixture = VaultFixture::empty();
    let installed = InstalledBinary::copy_from_test_binary(&fixture);
    let latest = newer_patch();
    let server = MockReleaseServer::start(&latest, false);

    let output = run_installed(&fixture, &installed, &server, &["update", "--check"]);
    output.assert_success();
    let json: serde_json::Value = serde_json::from_str(output.stdout.trim()).unwrap();
    assert_eq!(json["current"], CURRENT_VERSION);
    assert_eq!(json["latest"], latest);
    assert_eq!(json["update_available"], true);
    assert_eq!(json["channel"], "curl-installer");
    assert_eq!(json["action"], "update automatically");
}

/// Scenario 2: the silent worker stages a verified replacement, records the
/// transition, logs it, and leaves the original binary able to advise that the
/// already-running process will observe the staged version next invocation.
#[test]
fn background_update_swaps_binary_records_outcome_and_surfaces_advisory() {
    let _serial = serial_test();
    let fixture = VaultFixture::empty();
    create_vault_db(&fixture);
    let installed = InstalledBinary::copy_from_test_binary(&fixture);
    let latest = newer_patch();
    let server = MockReleaseServer::start(&latest, false);

    let output = run_installed(&fixture, &installed, &server, &["update", "--background"]);
    output.assert_success();
    assert!(output.stdout.is_empty(), "background worker must be silent");
    assert!(output.stderr.is_empty(), "background worker must be silent");
    assert_stub(&installed, &latest);

    let saved = outcome(&fixture);
    assert_eq!(saved["status"], "updated");
    assert_eq!(saved["from"], CURRENT_VERSION);
    assert_eq!(saved["to"], latest);
    assert_eq!(saved["channel"], "curl-installer");
    let log = fs::read_to_string(fixture.enzyme_home().join("update.log")).unwrap();
    assert!(log.contains("background update check started"));
    assert!(log.contains("\"status\":\"updated\""));

    let status = fixture.cli().arg("status").run();
    status.assert_success();
    status.assert_contains("Binary update to");
    status.assert_contains("staged and takes effect on the next invocation");
}

/// Surfacing contract: installations that cannot replace themselves must turn
/// a durable available-version outcome into channel-specific status guidance.
#[test]
fn status_advises_package_manager_for_unmanaged_channel() {
    let _serial = serial_test();
    let fixture = VaultFixture::empty();
    create_vault_db(&fixture);
    let latest = newer_patch();
    let previous = serde_json::json!({
        "status": "skipped",
        "from": CURRENT_VERSION,
        "to": latest,
        "finished_at_ms": 123,
        "error": null,
        "channel": "unmanaged",
    });
    vault_db(&fixture)
        .set_config(LAST_OUTCOME_KEY, &previous.to_string())
        .unwrap();

    let status = fixture.cli().arg("status").run();
    status.assert_success();
    status.assert_contains("Binary update available");
    status.assert_contains("install the latest Enzyme release with your package manager");
}

/// Scenario 3: a checksum mismatch is durable failure evidence, never a
/// partially installed executable or leaked same-directory staging tree.
#[test]
fn checksum_tamper_records_failure_and_preserves_binary() {
    let _serial = serial_test();
    let fixture = VaultFixture::empty();
    create_vault_db(&fixture);
    let installed = InstalledBinary::copy_from_test_binary(&fixture);
    let latest = newer_patch();
    let server = MockReleaseServer::start(&latest, true);

    run_installed(&fixture, &installed, &server, &["update", "--background"]).assert_success();

    let saved = outcome(&fixture);
    assert_eq!(saved["status"], "failed");
    assert!(
        saved["error"]
            .as_str()
            .unwrap()
            .contains("SHA-256 mismatch")
    );
    installed.assert_unchanged();
    installed.assert_no_partial_files();
}

/// Review regression: a release server that accepts the request but stalls
/// the asset response must time out and release the updater without swapping.
#[test]
fn stalled_download_times_out_and_preserves_binary() {
    let _serial = serial_test();
    let fixture = VaultFixture::empty();
    create_vault_db(&fixture);
    let installed = InstalledBinary::copy_from_test_binary(&fixture);
    let latest = newer_patch();
    let server = MockReleaseServer::start_with_archive(
        &latest,
        release_archive(&latest),
        false,
        Some(Duration::from_millis(500)),
    );

    let started = Instant::now();
    let output = fixture
        .cli()
        .executable(&installed.path)
        .env("HOME", installed.os_home.display().to_string())
        .env("ENZYME_UPDATE_API_BASE", server.base_url())
        .env("ENZYME_UPDATE_RELEASE_BASE", server.base_url())
        .env("ENZYME_UPDATE_DOWNLOAD_TIMEOUT_MS", "100")
        .args(["update", "--background"])
        .run();
    output.assert_success();
    assert!(started.elapsed() < Duration::from_secs(5));
    let saved = outcome(&fixture);
    assert_eq!(saved["status"], "failed");
    assert!(saved["error"].as_str().unwrap().contains("timed out"));
    installed.assert_unchanged();
    installed.assert_no_partial_files();
}

/// Review regression: the archive is data, not a general filesystem payload;
/// entries outside the release allowlist fail before touching the live binary.
#[test]
fn unexpected_archive_entry_is_rejected() {
    let _serial = serial_test();
    let fixture = VaultFixture::empty();
    create_vault_db(&fixture);
    let installed = InstalledBinary::copy_from_test_binary(&fixture);
    let latest = newer_patch();
    let server = MockReleaseServer::start_with_archive(
        &latest,
        archive_with_unexpected_entry(),
        false,
        None,
    );

    run_installed(&fixture, &installed, &server, &["update", "--background"]).assert_success();
    let saved = outcome(&fixture);
    assert_eq!(saved["status"], "failed");
    assert!(
        saved["error"]
            .as_str()
            .unwrap()
            .contains("unexpected entry")
    );
    installed.assert_unchanged();
    installed.assert_no_partial_files();
}

/// Review regression: an `enzyme` archive entry must be a regular file. A
/// symlink must neither chmod its external target nor become the live command.
#[cfg(unix)]
#[test]
fn symlinked_archive_binary_is_rejected_without_touching_target() {
    let _serial = serial_test();
    let fixture = VaultFixture::empty();
    create_vault_db(&fixture);
    let installed = InstalledBinary::copy_from_test_binary(&fixture);
    let external = fixture.root().join("archive-symlink-target");
    fs::write(&external, "must stay unchanged").unwrap();
    let before_permissions = fs::metadata(&external).unwrap().permissions();
    let latest = newer_patch();
    let server = MockReleaseServer::start_with_archive(
        &latest,
        archive_with_symlinked_enzyme(&external),
        false,
        None,
    );

    run_installed(&fixture, &installed, &server, &["update", "--background"]).assert_success();
    assert_eq!(outcome(&fixture)["status"], "failed");
    assert_eq!(
        fs::read_to_string(&external).unwrap(),
        "must stay unchanged"
    );
    assert_eq!(
        fs::metadata(&external).unwrap().permissions(),
        before_permissions
    );
    installed.assert_unchanged();
}

/// Review regression: invoking a symlink installed in `~/.local/bin` replaces
/// the link itself and leaves its external target byte-identical.
#[cfg(unix)]
#[test]
fn local_bin_symlink_swap_replaces_link_not_external_target() {
    use std::os::unix::fs::symlink;

    let _serial = serial_test();
    let fixture = VaultFixture::empty();
    create_vault_db(&fixture);
    let installed = InstalledBinary::copy_from_test_binary(&fixture);
    let external = fixture.root().join("external-enzyme");
    fs::rename(&installed.path, &external).unwrap();
    symlink(&external, &installed.path).unwrap();
    let latest = newer_patch();
    let server = MockReleaseServer::start(&latest, false);

    fixture
        .cli()
        .executable("enzyme")
        .env(
            "PATH",
            installed.path.parent().unwrap().display().to_string(),
        )
        .env("HOME", installed.os_home.display().to_string())
        .env("ENZYME_UPDATE_API_BASE", server.base_url())
        .env("ENZYME_UPDATE_RELEASE_BASE", server.base_url())
        .args(["update", "--background"])
        .run()
        .assert_success();
    assert_stub(&installed, &latest);
    assert!(
        !fs::symlink_metadata(&installed.path)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        fs::metadata(&external).unwrap().len(),
        installed.original_len
    );
    assert_eq!(sha256_path(&external), installed.original_digest);
    let saved = outcome(&fixture);
    assert_eq!(saved["status"], "updated");
    assert_eq!(saved["channel"], "curl-installer");
}

/// Review regression: outcome persistence gets one bounded busy attempt. A
/// locked vault keeps its prior outcome and the worker records the failure.
#[test]
fn locked_vault_db_does_not_multiply_busy_timeout_retries() {
    let _serial = serial_test();
    let fixture = VaultFixture::empty();
    create_vault_db(&fixture);
    let previous = serde_json::json!({
        "status": "up_to_date",
        "from": CURRENT_VERSION,
        "to": CURRENT_VERSION,
        "finished_at_ms": 123,
        "error": null,
        "channel": "curl-installer",
    });
    vault_db(&fixture)
        .set_config(LAST_OUTCOME_KEY, &previous.to_string())
        .unwrap();
    let database = fixture.vault_enzyme_dir().join("enzyme.db");
    let database_lock = SqliteWriteLock::acquire(&database);
    let installed = InstalledBinary::copy_from_test_binary(&fixture);
    let server = MockReleaseServer::start(CURRENT_VERSION, false);

    let started = Instant::now();
    run_installed(&fixture, &installed, &server, &["update", "--background"]).assert_success();
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "best-effort outcome persistence must not stack long busy retries"
    );
    drop(database_lock);
    assert_eq!(outcome(&fixture), previous);
    let log = fs::read_to_string(fixture.enzyme_home().join("update.log")).unwrap();
    assert!(log.contains("failed to persist outcome"));
    installed.assert_unchanged();
}

/// Scenario 4: automatic major-version jumps are policy-skipped before any
/// release asset is downloaded or installed.
#[test]
fn major_version_guard_records_skip_and_preserves_binary() {
    let _serial = serial_test();
    let fixture = VaultFixture::empty();
    create_vault_db(&fixture);
    let installed = InstalledBinary::copy_from_test_binary(&fixture);
    let latest = next_major();
    let server = MockReleaseServer::start(&latest, false);

    run_installed(&fixture, &installed, &server, &["update", "--background"]).assert_success();

    let saved = outcome(&fixture);
    assert_eq!(saved["status"], "skipped");
    assert_eq!(saved["to"], latest);
    assert!(saved["error"].as_str().unwrap().contains("major-version"));
    installed.assert_unchanged();
    assert_eq!(server.request_count(), 1, "guard must run before downloads");
}

/// Scenario 5: the global opt-out governs only automatic workers; an explicit
/// foreground update remains an authorized request and performs the swap.
#[test]
fn auto_false_skips_background_but_foreground_still_swaps() {
    let _serial = serial_test();
    let fixture = VaultFixture::empty();
    create_vault_db(&fixture);
    fs::write(
        fixture.enzyme_home().join("config.toml"),
        "[update]\nauto = false\n",
    )
    .unwrap();
    let installed = InstalledBinary::copy_from_test_binary(&fixture);
    let latest = newer_patch();
    let server = MockReleaseServer::start(&latest, false);

    run_installed(&fixture, &installed, &server, &["update", "--background"]).assert_success();
    let saved = outcome(&fixture);
    assert_eq!(saved["status"], "skipped");
    assert!(
        saved["error"]
            .as_str()
            .unwrap()
            .contains("update.auto = false")
    );
    installed.assert_unchanged();
    assert_eq!(server.request_count(), 0, "opt-out must skip discovery too");

    run_installed(&fixture, &installed, &server, &["update"]).assert_success();
    assert_stub(&installed, &latest);
    assert_eq!(outcome(&fixture)["status"], "updated");
}

/// Scenario 6: another process holding `update.lock` makes the background
/// worker a successful silent no-op without overwriting prior durable state.
#[test]
fn lock_contention_exits_quietly_without_swap_or_outcome_overwrite() {
    let _serial = serial_test();
    let fixture = VaultFixture::empty();
    create_vault_db(&fixture);
    let previous = serde_json::json!({
        "status": "up_to_date",
        "from": CURRENT_VERSION,
        "to": CURRENT_VERSION,
        "finished_at_ms": 123,
        "error": null,
        "channel": "curl-installer",
    });
    vault_db(&fixture)
        .set_config(LAST_OUTCOME_KEY, &previous.to_string())
        .unwrap();
    let installed = InstalledBinary::copy_from_test_binary(&fixture);
    let latest = newer_patch();
    let server = MockReleaseServer::start(&latest, false);
    let lock_path = fixture.enzyme_home().join("update.lock");
    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(lock_path)
        .unwrap();
    lock.try_lock_exclusive().unwrap();

    let output = run_installed(&fixture, &installed, &server, &["update", "--background"]);
    output.assert_success();
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
    installed.assert_unchanged();
    assert_eq!(outcome(&fixture), previous);
    assert_eq!(server.request_count(), 0);
}

/// Scenario 7: a fresh check timestamp suppresses detached work on the agent
/// hot path while durable outcomes remain in the vault DB.
#[test]
fn refresh_throttle_avoids_network_and_exposes_last_outcome() {
    let _serial = serial_test();
    let (fixture, llm) = init_vault();
    let server = MockReleaseServer::start(&newer_patch(), false);
    let previous = serde_json::json!({
        "status": "up_to_date",
        "from": CURRENT_VERSION,
        "to": CURRENT_VERSION,
        "finished_at_ms": chrono::Utc::now().timestamp_millis(),
        "error": null,
        "channel": "unmanaged",
    });
    let db = vault_db(&fixture);
    db.set_config(
        CHECKED_AT_KEY,
        &chrono::Utc::now().timestamp_millis().to_string(),
    )
    .unwrap();
    db.set_config(LAST_OUTCOME_KEY, &previous.to_string())
        .unwrap();

    let output = fixture
        .cli()
        .args(["refresh", "--quiet", "--use-env-llm"])
        .env_llm(&llm)
        .env("ENZYME_UPDATE_API_BASE", server.base_url())
        .env("ENZYME_UPDATE_RELEASE_BASE", server.base_url())
        .run();
    output.assert_success();
    let json: serde_json::Value = serde_json::from_str(output.stdout.trim()).unwrap();
    assert_eq!(json["update"]["status"], "ok");
    assert!(json["update"].get("binary").is_none());
    assert_eq!(outcome(&fixture), previous);
    assert_eq!(
        server.request_count(),
        0,
        "fresh checks must not spawn a worker"
    );
}

/// Scenario 8: before any updater has recorded state, refresh JSON reports ok;
/// dead-port defaults keep the test fully offline even if the fire-and-forget
/// scheduler runs.
#[test]
fn fresh_vault_refresh_reports_current_version_and_null_outcome() {
    let _serial = serial_test();
    let (fixture, llm) = init_vault();
    // Disable the fire-and-forget scheduler: otherwise the spawned worker
    // races this test's DB read by recording a failed (dead-port) outcome.
    std::fs::write(
        fixture.enzyme_home().join("config.toml"),
        "[update]\nauto = false\n",
    )
    .unwrap();

    let output = fixture
        .cli()
        .args(["refresh", "--quiet", "--use-env-llm"])
        .env_llm(&llm)
        .run();
    output.assert_success();
    let json: serde_json::Value = serde_json::from_str(output.stdout.trim()).unwrap();
    assert_eq!(json["update"]["status"], "ok");
    assert!(json["update"].get("binary").is_none());
    assert!(vault_db(&fixture).get_config(LAST_OUTCOME_KEY).unwrap().is_none());
}

/// Scenario 9: a pending non-self-swappable update surfaces channel guidance
/// in the refresh JSON without exposing binary-update internals.
#[test]
fn refresh_reports_action_required_for_pending_homebrew_update() {
    let _serial = serial_test();
    let (fixture, llm) = init_vault();
    let latest = newer_patch();
    let previous = serde_json::json!({
        "status": "skipped",
        "from": CURRENT_VERSION,
        "to": latest,
        "finished_at_ms": 123,
        "error": null,
        "channel": "homebrew",
    });
    vault_db(&fixture)
        .set_config(LAST_OUTCOME_KEY, &previous.to_string())
        .unwrap();

    let output = fixture
        .cli()
        .args(["refresh", "--quiet", "--use-env-llm"])
        .env_llm(&llm)
        .run();
    output.assert_success();
    let json: serde_json::Value = serde_json::from_str(output.stdout.trim()).unwrap();
    assert_eq!(json["update"]["status"], "action_required");
    assert_eq!(json["update"]["action"], "brew upgrade enzyme");
    assert!(
        json["update"]["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains(CURRENT_VERSION) && reason.contains(&latest))
    );
    assert!(json["update"].get("binary").is_none());
}

/// Review regression (refresh --quiet "database is locked"): the update worker
/// records its outcome only under `workspace.lock`, so it never opens the vault
/// DB read-write while a foreground init/refresh is writing. While the lock is
/// held it gives up after a bounded wait and leaves the DB untouched.
#[test]
fn background_update_bookkeeping_waits_for_workspace_lock() {
    let _serial = serial_test();
    let fixture = VaultFixture::empty();
    create_vault_db(&fixture);
    let installed = InstalledBinary::copy_from_test_binary(&fixture);
    let server = MockReleaseServer::start(CURRENT_VERSION, false);

    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(fixture.vault_enzyme_dir().join("workspace.lock"))
        .unwrap();
    lock.lock_exclusive().unwrap();
    let output = fixture
        .cli()
        .executable(&installed.path)
        .env("HOME", installed.os_home.display().to_string())
        .env("ENZYME_UPDATE_API_BASE", server.base_url())
        .env("ENZYME_UPDATE_RELEASE_BASE", server.base_url())
        .env("ENZYME_UPDATE_OUTCOME_LOCK_WAIT_MS", "200")
        .args(["update", "--background"])
        .run();
    output.assert_success();
    assert!(
        vault_db(&fixture)
            .get_config(LAST_OUTCOME_KEY)
            .unwrap()
            .is_none(),
        "outcome must not be written while workspace.lock is held"
    );
    let log = fs::read_to_string(fixture.enzyme_home().join("update.log")).unwrap();
    assert!(log.contains("workspace busy"));

    fs2::FileExt::unlock(&lock).unwrap();
    run_installed(&fixture, &installed, &server, &["update", "--background"]).assert_success();
    assert_eq!(outcome(&fixture)["status"], "up_to_date");
    installed.assert_unchanged();
}

/// `--llm none` promises no network: a quiet refresh must not spawn the
/// update check even when the throttle would allow one.
#[test]
fn refresh_with_llm_none_spawns_no_update_check() {
    let _serial = serial_test();
    let (fixture, _llm) = init_vault();
    let server = MockReleaseServer::start(&newer_patch(), false);

    let output = fixture
        .cli()
        .args(["refresh", "--quiet", "--llm", "none"])
        .env("ENZYME_UPDATE_API_BASE", server.base_url())
        .env("ENZYME_UPDATE_RELEASE_BASE", server.base_url())
        .run();
    output.assert_success();
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(server.request_count(), 0, "--llm none must not check for updates");
    assert!(vault_db(&fixture).get_config(LAST_OUTCOME_KEY).unwrap().is_none());
}
