//! Binary update discovery, staging, and durable outcome reporting.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use flate2::read::GzDecoder;
use fs2::FileExt;
use sha2::{Digest, Sha256};

use enzyme_core::config::enzyme_config::EnzymeConfig;
use enzyme_core::config::paths::EnzymePaths;
use enzyme_core::db::Database;
use serde::{Deserialize, Serialize};

pub const UPDATE_CHECKED_AT_CONFIG_KEY: &str = "update.checked_at_ms";
pub const UPDATE_LAST_OUTCOME_CONFIG_KEY: &str = "update.last_outcome";
pub const UPDATE_CHECK_INTERVAL_MS: i64 = 24 * 60 * 60 * 1_000;

pub(crate) const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const DEFAULT_UPDATE_API_BASE: &str = "https://api.enzyme.garden";
const DEFAULT_UPDATE_RELEASE_BASE: &str = "https://github.com/useenzyme/enzyme";
const DEFAULT_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(60);
const OUTCOME_BUSY_TIMEOUT_MS: u64 = 2_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateChannel {
    #[serde(rename = "curl-installer")]
    CurlInstaller,
    Plugin,
    Homebrew,
    Unmanaged,
}

impl UpdateChannel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CurlInstaller => "curl-installer",
            Self::Plugin => "plugin",
            Self::Homebrew => "homebrew",
            Self::Unmanaged => "unmanaged",
        }
    }

    pub fn advisory_action(self) -> &'static str {
        match self {
            Self::CurlInstaller => "update automatically",
            Self::Plugin => "update Enzyme via the plugin marketplace",
            Self::Homebrew => "brew upgrade enzyme",
            Self::Unmanaged => "install the latest Enzyme release with your package manager",
        }
    }

    pub fn is_self_swappable(self) -> bool {
        self == Self::CurlInstaller
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BinaryUpdateStatus {
    Updated,
    UpToDate,
    Skipped,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BinaryUpdateOutcome {
    pub status: BinaryUpdateStatus,
    pub from: String,
    #[serde(default)]
    pub to: Option<String>,
    pub finished_at_ms: i64,
    #[serde(default)]
    pub error: Option<String>,
    pub channel: UpdateChannel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct ParsedVersion {
    major: u64,
    minor: u64,
    patch: u64,
}

fn parse_version(raw: &str) -> Result<ParsedVersion> {
    let raw = raw.trim().strip_prefix('v').unwrap_or(raw.trim());
    let core = raw.split(['-', '+']).next().unwrap_or(raw);
    let mut parts = core.split('.');
    let major = parts
        .next()
        .ok_or_else(|| anyhow::anyhow!("missing major version"))?
        .parse()?;
    let minor = parts
        .next()
        .ok_or_else(|| anyhow::anyhow!("missing minor version"))?
        .parse()?;
    let patch = parts
        .next()
        .ok_or_else(|| anyhow::anyhow!("missing patch version"))?
        .parse()?;
    if parts.next().is_some() {
        anyhow::bail!("invalid version {raw}");
    }
    Ok(ParsedVersion {
        major,
        minor,
        patch,
    })
}

pub(crate) fn update_available(current: &str, latest: &str) -> Result<bool> {
    Ok(parse_version(latest)? > parse_version(current)?)
}

pub(crate) fn automatic_major_allowed(current: &str, latest: &str) -> Result<bool> {
    Ok(parse_version(current)?.major == parse_version(latest)?.major)
}

pub fn detect_channel(exe: &Path) -> UpdateChannel {
    detect_channel_with_home(exe, std::env::var_os("HOME").as_deref().map(Path::new))
}

fn detect_channel_with_home(exe: &Path, home: Option<&Path>) -> UpdateChannel {
    channel_for_path(exe, home).unwrap_or_else(|| {
        exe.canonicalize()
            .ok()
            .and_then(|canonical| channel_for_path(&canonical, home))
            .unwrap_or(UpdateChannel::Unmanaged)
    })
}

fn channel_for_path(exe: &Path, home: Option<&Path>) -> Option<UpdateChannel> {
    let rendered = exe.to_string_lossy().replace('\\', "/");
    if rendered.contains("/.claude/plugins/") || rendered.contains("/.codex/") {
        return Some(UpdateChannel::Plugin);
    }
    if exe.starts_with("/opt/homebrew")
        || exe.starts_with("/usr/local/Cellar")
        || exe.starts_with("/home/linuxbrew")
    {
        return Some(UpdateChannel::Homebrew);
    }
    if home.is_some_and(|home| exe.starts_with(home.join(".local/bin"))) {
        return Some(UpdateChannel::CurlInstaller);
    }
    None
}

#[derive(Debug, Deserialize)]
struct VersionResponse {
    latest: String,
    #[serde(rename = "minimum_supported")]
    _minimum_supported: String,
}

fn update_api_base() -> String {
    std::env::var("ENZYME_UPDATE_API_BASE").unwrap_or_else(|_| DEFAULT_UPDATE_API_BASE.to_string())
}

fn update_release_base() -> String {
    std::env::var("ENZYME_UPDATE_RELEASE_BASE")
        .unwrap_or_else(|_| DEFAULT_UPDATE_RELEASE_BASE.to_string())
}

async fn discover_latest() -> Result<String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .context("build update discovery client")?;
    let api_url = format!("{}/version", update_api_base().trim_end_matches('/'));
    let api_result = async {
        let response = client.get(&api_url).send().await?.error_for_status()?;
        let version: VersionResponse = response.json().await?;
        parse_version(&version.latest).context("API returned invalid latest version")?;
        parse_version(&version._minimum_supported)
            .context("API returned invalid minimum_supported version")?;
        Ok::<_, anyhow::Error>(version.latest.trim_start_matches('v').to_string())
    }
    .await;
    if let Ok(latest) = api_result {
        return Ok(latest);
    }

    // GitHub's /releases/latest endpoint redirects to /releases/tag/vX.Y.Z.
    // Reading the final URL avoids parsing any HTML and is easy to mock in
    // subprocess tests through ENZYME_UPDATE_RELEASE_BASE.
    let latest_url = format!(
        "{}/releases/latest",
        update_release_base().trim_end_matches('/')
    );
    let response = client
        .get(&latest_url)
        .send()
        .await
        .context("version API failed and release redirect fallback failed")?
        .error_for_status()
        .context("release redirect fallback returned an error")?;
    let segment = response
        .url()
        .path_segments()
        .and_then(|mut parts| parts.next_back())
        .filter(|part| !part.is_empty() && *part != "latest")
        .context("release redirect did not identify a version")?;
    let latest = segment.trim_start_matches('v').to_string();
    parse_version(&latest).context("release redirect returned an invalid version")?;
    Ok(latest)
}

fn action_for(channel: UpdateChannel, current: &str, latest: &str) -> Result<String> {
    if !update_available(current, latest)? {
        return Ok("none".to_string());
    }
    if !automatic_major_allowed(current, latest)? {
        return Ok("install this major-version update manually".to_string());
    }
    Ok(channel.advisory_action().to_string())
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn persist_outcome(vault_path: &Path, outcome: &BinaryUpdateOutcome) -> Result<()> {
    let paths = EnzymePaths::new(vault_path)?;
    if !paths.vault_db.exists() {
        return Ok(());
    }
    // Give way to a foreground init/refresh: wait briefly for `workspace.lock`
    // (the refresh that spawned this worker releases it as it exits) and hold
    // it while writing, so the open below never races their writes. Still
    // held after the wait → skip; the next refresh re-checks.
    let store = paths
        .vault_db
        .parent()
        .context("vault DB has no parent")?;
    let Some(_workspace_lock) = super::lock::try_workspace_lock_within(
        store,
        std::time::Duration::from_millis(outcome_lock_wait_ms()),
    )?
    else {
        return Err(super::exit::CliFailure::busy(anyhow::anyhow!(
            "workspace busy; update outcome not recorded"
        )));
    };
    let _sync_guard = paths.sync_on_drop();
    let db = Database::open_with_busy_timeout(&paths.vault_db, OUTCOME_BUSY_TIMEOUT_MS)
        .context("open vault DB to record update outcome")?;
    // This is best-effort background bookkeeping. Do not stack Database's
    // general 30-second busy timeout with an outer retry loop while holding
    // update.lock; one short attempt is enough and failures are logged.
    record_outcome(&db, outcome)
}

/// How long the update worker waits for `workspace.lock` before skipping its
/// bookkeeping (`ENZYME_UPDATE_OUTCOME_LOCK_WAIT_MS` overrides, for tests).
fn outcome_lock_wait_ms() -> u64 {
    std::env::var("ENZYME_UPDATE_OUTCOME_LOCK_WAIT_MS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(5_000)
}

fn record_outcome(db: &Database, outcome: &BinaryUpdateOutcome) -> Result<()> {
    db.set_config(
        UPDATE_CHECKED_AT_CONFIG_KEY,
        &outcome.finished_at_ms.to_string(),
    )?;
    db.set_config(
        UPDATE_LAST_OUTCOME_CONFIG_KEY,
        &serde_json::to_string(outcome).context("serialize binary update outcome")?,
    )?;
    Ok(())
}

pub fn load_outcome(db: &Database) -> Option<BinaryUpdateOutcome> {
    let raw = db.get_config(UPDATE_LAST_OUTCOME_CONFIG_KEY).ok()??;
    serde_json::from_str(&raw).ok()
}

/// Hot-path scheduler for `refresh --quiet`. This performs only local reads
/// and process creation; all network and update work belongs to the child.
pub fn maybe_spawn_background(vault_path: &Path, db: &Database) -> bool {
    match EnzymeConfig::automatic_updates_enabled() {
        Ok(true) => {}
        Ok(false) => return false,
        Err(error) => {
            tracing::debug!("could not read automatic-update policy: {error:#}");
            return false;
        }
    }

    let checked_at = db
        .get_config(UPDATE_CHECKED_AT_CONFIG_KEY)
        .ok()
        .flatten()
        .and_then(|value| value.parse::<i64>().ok());
    if checked_at.is_some_and(|checked| now_ms().saturating_sub(checked) < UPDATE_CHECK_INTERVAL_MS)
    {
        return false;
    }

    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(error) => {
            tracing::debug!("failed to locate executable for background update: {error}");
            return false;
        }
    };
    let mut command = std::process::Command::new(exe);
    command
        .args(["update", "--background", "-p"])
        .arg(vault_path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
        unsafe {
            command.pre_exec(|| {
                libc::setsid();
                match libc::fork() {
                    -1 => Err(std::io::Error::last_os_error()),
                    0 => Ok(()),
                    _ => libc::_exit(0),
                }
            });
        }
    }

    match command.spawn() {
        Ok(mut child) => {
            // The only process reaped here is the immediately exiting
            // double-fork intermediary; the update worker is never waited on.
            let _ = child.wait();
            true
        }
        Err(error) => {
            tracing::debug!("failed to spawn background update: {error}");
            false
        }
    }
}

fn write_background_log(home: &Path, message: &str) {
    if fs::create_dir_all(home).is_err() {
        return;
    }
    if let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(home.join("update.log"))
    {
        let _ = writeln!(file, "{} {}", now_ms(), message);
    }
}

fn acquire_update_lock(home: &Path) -> Result<Option<File>> {
    fs::create_dir_all(home).with_context(|| format!("create {}", home.display()))?;
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(home.join("update.lock"))
        .context("open update.lock")?;
    match file.try_lock_exclusive() {
        Ok(()) => Ok(Some(file)),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
        Err(error) => Err(error).context("lock update.lock"),
    }
}

fn platform_target() -> Result<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Ok("macos-arm64"),
        ("macos", "x86_64") => Ok("macos-x86_64"),
        ("linux", "x86_64") => Ok("linux-x86_64"),
        ("linux", "aarch64") => Ok("linux-arm64"),
        (os, arch) => anyhow::bail!("unsupported update platform {os}-{arch}"),
    }
}

async fn download_file(url: &str, destination: &Path) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(download_timeout())
        .build()
        .context("build update download client")?;
    let mut response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("download {url}"))?
        .error_for_status()
        .with_context(|| format!("download {url}"))?;
    let mut output =
        File::create(destination).with_context(|| format!("create {}", destination.display()))?;
    while let Some(chunk) = response.chunk().await.context("read update download")? {
        output
            .write_all(&chunk)
            .with_context(|| format!("write {}", destination.display()))?;
    }
    output.sync_all()?;
    Ok(())
}

fn download_timeout() -> Duration {
    std::env::var("ENZYME_UPDATE_DOWNLOAD_TIMEOUT_MS")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .filter(|milliseconds| *milliseconds > 0)
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_DOWNLOAD_TIMEOUT)
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn copy_bundled_libraries(extracted: &Path, target_dir: &Path) -> Result<()> {
    let source = extracted.join("lib");
    if !source.is_dir() {
        return Ok(());
    }
    let destination = target_dir.join("lib");
    fs::create_dir_all(&destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            fs::copy(entry.path(), destination.join(entry.file_name()))?;
        }
    }
    Ok(())
}

async fn download_and_swap(exe: &Path, latest: &str) -> Result<()> {
    let target = platform_target()?;
    let filename = format!("enzyme-{target}.tar.gz");
    let release_url = format!(
        "{}/releases/download/v{latest}",
        update_release_base().trim_end_matches('/')
    );
    let target_dir = exe.parent().context("current executable has no parent")?;
    let temp = tempfile::Builder::new()
        .prefix(".enzyme-update-")
        .tempdir_in(target_dir)
        .context("create same-filesystem update directory")?;
    let archive_path = temp.path().join(&filename);
    let checksum_path = temp.path().join(format!("{filename}.sha256"));
    download_file(&format!("{release_url}/{filename}"), &archive_path).await?;
    download_file(&format!("{release_url}/{filename}.sha256"), &checksum_path).await?;

    let checksum_text = fs::read_to_string(&checksum_path).context("read update checksum")?;
    let expected = checksum_text
        .split_whitespace()
        .next()
        .context("empty update checksum")?
        .to_ascii_lowercase();
    let actual = sha256_file(&archive_path)?;
    if expected.len() != 64 || expected != actual {
        anyhow::bail!("SHA-256 mismatch: expected {expected}, got {actual}");
    }

    let extracted = temp.path().join("extracted");
    fs::create_dir(&extracted)?;
    let archive = File::open(&archive_path)?;
    let mut archive = tar::Archive::new(GzDecoder::new(archive));
    let mut found_replacement = false;
    for entry in archive.entries().context("read update archive")? {
        let mut entry = entry.context("read update archive entry")?;
        let path = entry
            .path()
            .context("read update archive entry path")?
            .into_owned();
        let kind = entry.header().entry_type();
        let allowed = if path == Path::new("enzyme") {
            if found_replacement {
                anyhow::bail!("update archive contained duplicate enzyme entries");
            }
            found_replacement = true;
            kind.is_file()
        } else if path == Path::new("lib") {
            kind.is_dir()
        } else {
            path.parent() == Some(Path::new("lib")) && kind.is_file()
        };
        if !allowed {
            anyhow::bail!(
                "update archive contained unexpected entry {}",
                path.display()
            );
        }
        if !entry
            .unpack_in(&extracted)
            .with_context(|| format!("extract update archive entry {}", path.display()))?
        {
            anyhow::bail!("update archive entry escaped extraction root");
        }
    }
    let replacement = extracted.join("enzyme");
    let replacement_metadata = fs::symlink_metadata(&replacement)
        .context("update archive did not contain an enzyme binary")?;
    if !found_replacement || !replacement_metadata.file_type().is_file() {
        anyhow::bail!("update archive enzyme entry was not a regular file");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&replacement, fs::Permissions::from_mode(0o755))?;
    }
    copy_bundled_libraries(&extracted, target_dir)?;
    fs::rename(&replacement, exe).context("atomically replace current executable")?;
    Ok(())
}

async fn perform_update(
    exe: &Path,
    channel: UpdateChannel,
    background: bool,
) -> BinaryUpdateOutcome {
    let mut outcome = BinaryUpdateOutcome {
        status: BinaryUpdateStatus::Failed,
        from: CURRENT_VERSION.to_string(),
        to: None,
        finished_at_ms: 0,
        error: None,
        channel,
    };

    if background {
        match EnzymeConfig::automatic_updates_enabled() {
            Ok(true) => {}
            Ok(false) => {
                outcome.status = BinaryUpdateStatus::Skipped;
                outcome.error =
                    Some("automatic updates disabled by update.auto = false".to_string());
                outcome.finished_at_ms = now_ms();
                return outcome;
            }
            Err(error) => {
                outcome.error = Some(format!("could not read update.auto policy: {error:#}"));
                outcome.finished_at_ms = now_ms();
                return outcome;
            }
        }
    }

    let latest = match discover_latest().await {
        Ok(latest) => latest,
        Err(error) => {
            outcome.error = Some(format!("{error:#}"));
            outcome.finished_at_ms = now_ms();
            return outcome;
        }
    };
    outcome.to = Some(latest.clone());
    if !background {
        println!("Current version: {}", outcome.from);
        println!("Latest version: {latest}");
    }

    match update_available(CURRENT_VERSION, &latest) {
        Ok(false) => outcome.status = BinaryUpdateStatus::UpToDate,
        Err(error) => outcome.error = Some(format!("invalid discovered version: {error:#}")),
        Ok(true) => match automatic_major_allowed(CURRENT_VERSION, &latest) {
            Ok(false) => {
                outcome.status = BinaryUpdateStatus::Skipped;
                outcome.error = Some("major-version updates require manual installation".into());
            }
            Err(error) => outcome.error = Some(format!("invalid discovered version: {error:#}")),
            Ok(true) if !channel.is_self_swappable() => {
                outcome.status = BinaryUpdateStatus::Skipped;
            }
            Ok(true) => match download_and_swap(exe, &latest).await {
                Ok(()) => outcome.status = BinaryUpdateStatus::Updated,
                Err(error) => outcome.error = Some(format!("{error:#}")),
            },
        },
    }
    outcome.finished_at_ms = now_ms();
    outcome
}

async fn run_check() -> Result<()> {
    let channel = std::env::current_exe()
        .ok()
        .map(|path| detect_channel(&invocation_update_target(&path)))
        .unwrap_or(UpdateChannel::Unmanaged);
    let shape = match discover_latest().await {
        Ok(latest) => serde_json::json!({
            "current": CURRENT_VERSION,
            "latest": latest,
            "update_available": update_available(CURRENT_VERSION, &latest).unwrap_or(false),
            "channel": channel.as_str(),
            "action": action_for(channel, CURRENT_VERSION, &latest)
                .unwrap_or_else(|error| format!("unable to compare versions: {error:#}")),
        }),
        Err(error) => serde_json::json!({
            "current": CURRENT_VERSION,
            "latest": null,
            "update_available": false,
            "channel": channel.as_str(),
            "action": format!("update check failed: {error:#}"),
        }),
    };
    println!("{}", serde_json::to_string(&shape)?);
    Ok(())
}

pub async fn run_update(vault_path: &Path, check: bool, background: bool) -> Result<()> {
    if check {
        return run_check().await;
    }

    let home = EnzymePaths::enzyme_home()?;
    let Some(_lock) = acquire_update_lock(&home)? else {
        if !background {
            println!("Another Enzyme update is already running.");
        }
        return Ok(());
    };
    if background {
        write_background_log(&home, "background update check started");
    }

    let current_exe = std::env::current_exe().context("locate current executable")?;
    let exe = invocation_update_target(&current_exe);
    let channel = detect_channel(&exe);
    let outcome = perform_update(&exe, channel, background).await;
    let persist_result = persist_outcome(vault_path, &outcome);

    if background {
        let message = serde_json::to_string(&outcome)
            .unwrap_or_else(|error| format!("failed to serialize update outcome: {error}"));
        write_background_log(&home, &message);
        if let Err(error) = persist_result {
            write_background_log(&home, &format!("failed to persist outcome: {error:#}"));
        }
        return Ok(());
    }

    persist_result?;
    match outcome.status {
        BinaryUpdateStatus::Updated => println!("Updated Enzyme to {}.", outcome.to.unwrap()),
        BinaryUpdateStatus::UpToDate => println!("Enzyme is already up to date."),
        BinaryUpdateStatus::Skipped if !channel.is_self_swappable() => {
            println!("Update available: {}.", channel.advisory_action())
        }
        BinaryUpdateStatus::Skipped => println!(
            "Update skipped: {}.",
            outcome
                .error
                .as_deref()
                .unwrap_or("policy prevented the update")
        ),
        BinaryUpdateStatus::Failed => anyhow::bail!(
            "update failed: {}",
            outcome.error.as_deref().unwrap_or("unknown error")
        ),
    }
    Ok(())
}

fn invocation_update_target(current_exe: &Path) -> PathBuf {
    let Some(invoked) = std::env::args_os().next().map(PathBuf::from) else {
        return current_exe.to_path_buf();
    };
    let invoked = if invoked.is_absolute() {
        invoked
    } else if invoked.components().count() > 1 {
        match std::env::current_dir() {
            Ok(cwd) => cwd.join(invoked),
            Err(_) => return current_exe.to_path_buf(),
        }
    } else {
        let Some(resolved) = std::env::var_os("PATH").and_then(|path| {
            std::env::split_paths(&path)
                .map(|directory| directory.join(&invoked))
                .find(|candidate| fs::symlink_metadata(candidate).is_ok())
        }) else {
            return current_exe.to_path_buf();
        };
        resolved
    };
    if detect_channel(&invoked) == UpdateChannel::CurlInstaller {
        invoked
    } else {
        current_exe.to_path_buf()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_comparison_and_major_guard() {
        assert!(update_available("0.8.0", "0.8.1").unwrap());
        assert!(!update_available("0.8.1", "0.8.1").unwrap());
        assert!(!update_available("0.9.0", "0.8.9").unwrap());
        assert!(automatic_major_allowed("0.8.0", "0.99.0").unwrap());
        assert!(!automatic_major_allowed("0.8.0", "1.0.0").unwrap());
    }

    #[test]
    fn channel_detection_from_paths() {
        let home = Path::new("/Users/alice");
        assert_eq!(
            detect_channel_with_home(Path::new("/Users/alice/.local/bin/enzyme"), Some(home)),
            UpdateChannel::CurlInstaller
        );
        assert_eq!(
            detect_channel_with_home(Path::new("/opt/homebrew/bin/enzyme"), Some(home)),
            UpdateChannel::Homebrew
        );
        assert_eq!(
            detect_channel_with_home(
                Path::new("/usr/local/Cellar/enzyme/1/bin/enzyme"),
                Some(home)
            ),
            UpdateChannel::Homebrew
        );
        assert_eq!(
            detect_channel_with_home(Path::new("/tmp/.claude/plugins/cache/enzyme"), Some(home)),
            UpdateChannel::Plugin
        );
        assert_eq!(
            detect_channel_with_home(
                Path::new("/tmp/.codex/plugins/enzyme/bin/enzyme"),
                Some(home)
            ),
            UpdateChannel::Plugin
        );
        assert_eq!(
            detect_channel_with_home(Path::new("/workspace/target/debug/enzyme"), Some(home)),
            UpdateChannel::Unmanaged
        );
    }

    #[test]
    fn outcome_serialization_shape_is_stable() {
        let outcome = BinaryUpdateOutcome {
            status: BinaryUpdateStatus::Failed,
            from: "0.8.0".into(),
            to: Some("0.8.1".into()),
            finished_at_ms: 123,
            error: Some("checksum mismatch".into()),
            channel: UpdateChannel::CurlInstaller,
        };
        let json = serde_json::to_value(outcome).unwrap();
        assert_eq!(json["status"], "failed");
        assert_eq!(json["from"], "0.8.0");
        assert_eq!(json["to"], "0.8.1");
        assert_eq!(json["finished_at_ms"], 123);
        assert_eq!(json["error"], "checksum mismatch");
        assert_eq!(json["channel"], "curl-installer");

        let up_to_date = BinaryUpdateOutcome {
            status: BinaryUpdateStatus::UpToDate,
            from: "0.8.0".into(),
            to: Some("0.8.0".into()),
            finished_at_ms: 456,
            error: None,
            channel: UpdateChannel::Unmanaged,
        };
        let json = serde_json::to_value(up_to_date).unwrap();
        assert!(json["error"].is_null());
        assert_eq!(json["to"], "0.8.0");
    }

    #[test]
    fn outcome_and_check_timestamp_round_trip_through_config() {
        let db = Database::open_in_memory().unwrap();
        let outcome = BinaryUpdateOutcome {
            status: BinaryUpdateStatus::Updated,
            from: "0.8.0".into(),
            to: Some("0.8.1".into()),
            finished_at_ms: 789,
            error: None,
            channel: UpdateChannel::CurlInstaller,
        };
        record_outcome(&db, &outcome).unwrap();
        assert_eq!(load_outcome(&db), Some(outcome));
        assert_eq!(
            db.get_config(UPDATE_CHECKED_AT_CONFIG_KEY)
                .unwrap()
                .as_deref(),
            Some("789")
        );
    }
}
