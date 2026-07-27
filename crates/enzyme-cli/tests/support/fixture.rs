//! Vault + environment fixture and the subprocess runner for the `enzyme` binary.
//!
//! Each [`VaultFixture`] owns a tempdir containing:
//!   * `vault/`   — a copy of `tests/fixtures/canary-vault` (or empty)
//!   * `home/`    — an isolated `ENZYME_HOME` (auth.json, bootstrap.json, config)
//!   * `api-cache`— an isolated `ENZYME_API_CACHE_PATH`
//!
//! The CLI is always driven as a subprocess with an explicit environment, so
//! tests never mutate the test process's env and are safe to run in parallel.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::mock::{MockEnzymeApi, MockLlm};

/// Env vars that must never leak from the developer's shell into a test run.
const SCRUBBED_ENV: &[&str] = &[
    "OPENAI_API_KEY",
    "OPENAI_BASE_URL",
    "OPENAI_MODEL",
    "OPENROUTER_API_KEY",
    "OPENROUTER_BASE_URL",
    "OPENROUTER_MODEL",
    "ANTHROPIC_API_KEY",
    "ENZYME_HOME",
    "ENZYME_API_URL",
    "ENZYME_FREE_CONFIG_URL",
    "ENZYME_API_CACHE_PATH",
    "ENZYME_LOCAL_MODEL",
];

/// Absolute path to `tests/fixtures/canary-vault` in the repo.
pub fn canary_vault_source() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/canary-vault")
        .canonicalize()
        .expect("canary-vault fixture not found")
}

fn copy_dir(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).expect("create dir");
    for entry in std::fs::read_dir(src).expect("read fixture dir") {
        let entry = entry.expect("dir entry");
        let target = dst.join(entry.file_name());
        if entry.file_type().expect("file type").is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).expect("copy fixture file");
        }
    }
}

/// An isolated vault + `ENZYME_HOME` + API cache for one test.
pub struct VaultFixture {
    root: tempfile::TempDir,
}

impl VaultFixture {
    /// Vault seeded from `tests/fixtures/canary-vault`.
    pub fn new() -> Self {
        let fixture = Self::empty();
        copy_dir(&canary_vault_source(), &fixture.vault_path());
        fixture
    }

    /// Vault directory that exists but contains no markdown (zero-entities path).
    pub fn empty() -> Self {
        let root = tempfile::tempdir().expect("create tempdir");
        let fixture = Self { root };
        std::fs::create_dir_all(fixture.vault_path()).expect("create vault dir");
        std::fs::create_dir_all(fixture.enzyme_home()).expect("create enzyme home");
        fixture
    }

    /// Root tempdir holding `vault/`, `home/` and `api-cache`.
    pub fn root(&self) -> PathBuf {
        self.root.path().to_path_buf()
    }

    /// The vault directory the CLI is pointed at via `--vault`.
    pub fn vault_path(&self) -> PathBuf {
        self.root.path().join("vault")
    }

    /// The isolated `ENZYME_HOME`.
    pub fn enzyme_home(&self) -> PathBuf {
        self.root.path().join("home")
    }

    /// The isolated free-config cache file (`ENZYME_API_CACHE_PATH`).
    pub fn api_cache_path(&self) -> PathBuf {
        self.root.path().join("api-cache")
    }

    /// `<vault>/.enzyme` — where the index db and collection state live.
    pub fn vault_enzyme_dir(&self) -> PathBuf {
        self.vault_path().join(".enzyme")
    }

    /// Write a file into the vault (creating parent dirs).
    pub fn write_note(&self, rel_path: &str, contents: &str) {
        let path = self.vault_path().join(rel_path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create note parent");
        }
        std::fs::write(path, contents).expect("write note");
    }

    /// Seed `$ENZYME_HOME/auth.json` so the CLI believes it is logged in.
    pub fn login(&self, api_key: &str) {
        let home = self.enzyme_home();
        std::fs::create_dir_all(&home).expect("create enzyme home");
        let body = serde_json::json!({
            "api_key": api_key,
            "user_id": "user-test",
            "email": "test@example.com",
        });
        std::fs::write(home.join("auth.json"), body.to_string()).expect("write auth.json");
    }

    /// Whether `$ENZYME_HOME/auth.json` currently exists.
    pub fn is_logged_in(&self) -> bool {
        self.enzyme_home().join("auth.json").exists()
    }

    /// Whether the isolated free-config cache file currently exists.
    pub fn api_cache_exists(&self) -> bool {
        self.api_cache_path().exists()
    }

    /// Raw contents of the isolated free-config cache, if present.
    pub fn read_api_cache(&self) -> Option<String> {
        std::fs::read_to_string(self.api_cache_path()).ok()
    }

    /// Write a free-config cache entry with the given age, so tests can control
    /// hit/miss against `FREE_CONFIG_CACHE_TTL_SECS` (300s).
    pub fn seed_api_cache(&self, api_key: &str, base_url: &str, model: &str, age_secs: u64) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let body = serde_json::json!({
            "api_key": api_key,
            "base_url": base_url,
            "model": model,
            "cached_at": now.saturating_sub(age_secs),
        });
        std::fs::write(self.api_cache_path(), body.to_string()).expect("write api cache");
    }

    /// Start building an `enzyme` subprocess invocation against this fixture.
    ///
    /// Pre-wired: `--vault <vault_path>`, `ENZYME_HOME`, `ENZYME_API_CACHE_PATH`,
    /// and a scrubbed environment (no ambient `OPENAI_*` / `ENZYME_*` leakage).
    pub fn cli(&self) -> CliCommand {
        CliCommand {
            vault: self.vault_path(),
            envs: BTreeMap::from([
                (
                    "ENZYME_HOME".to_string(),
                    self.enzyme_home().display().to_string(),
                ),
                (
                    "ENZYME_API_CACHE_PATH".to_string(),
                    self.api_cache_path().display().to_string(),
                ),
            ]),
            args: Vec::new(),
        }
    }
}

impl Default for VaultFixture {
    fn default() -> Self {
        Self::new()
    }
}

/// Builder for one `enzyme` subprocess invocation.
pub struct CliCommand {
    vault: PathBuf,
    envs: BTreeMap<String, String>,
    args: Vec<String>,
}

impl CliCommand {
    /// Append a single CLI argument.
    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    /// Append several CLI arguments.
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// Set an environment variable for the subprocess.
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.envs.insert(key.into(), value.into());
        self
    }

    /// Point hosted credits/login (`ENZYME_API_URL`) and the free-config
    /// endpoint (`ENZYME_FREE_CONFIG_URL`) at the mock Enzyme API.
    pub fn enzyme_api(self, api: &MockEnzymeApi) -> Self {
        self.env("ENZYME_API_URL", api.base_url())
            .env("ENZYME_FREE_CONFIG_URL", api.free_config_url())
    }

    /// Point the env-LLM path (`OPENAI_API_KEY` / `OPENAI_BASE_URL` /
    /// `OPENAI_MODEL`) at the mock LLM. Pair with `--use-env-llm`.
    pub fn env_llm(self, llm: &MockLlm) -> Self {
        self.env("OPENAI_API_KEY", "sk-test-env-key")
            .env("OPENAI_BASE_URL", llm.base_url())
            .env("OPENAI_MODEL", "test-model")
    }

    /// Override the vault path passed via `--vault`.
    pub fn vault(mut self, path: impl Into<PathBuf>) -> Self {
        self.vault = path.into();
        self
    }

    /// Run the CLI to completion and capture stdout/stderr/exit code.
    pub fn run(self) -> CliOutput {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_enzyme"));
        for var in SCRUBBED_ENV {
            cmd.env_remove(var);
        }
        for (key, value) in &self.envs {
            cmd.env(key, value);
        }
        cmd.arg("--vault").arg(&self.vault);
        cmd.args(&self.args);

        let output = cmd.output().expect("failed to run enzyme binary");
        CliOutput {
            code: output.status.code(),
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        }
    }
}

/// Captured result of one CLI subprocess run.
#[derive(Debug, Clone)]
pub struct CliOutput {
    pub code: Option<i32>,
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

impl CliOutput {
    /// stdout and stderr concatenated — most CLI messages are worth matching
    /// against either stream.
    pub fn combined(&self) -> String {
        format!("{}{}", self.stdout, self.stderr)
    }

    /// Whether `needle` appears in stdout or stderr.
    pub fn contains(&self, needle: &str) -> bool {
        self.combined().contains(needle)
    }

    #[track_caller]
    pub fn assert_success(&self) -> &Self {
        assert!(
            self.success,
            "expected success, got {:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
            self.code, self.stdout, self.stderr
        );
        self
    }

    #[track_caller]
    pub fn assert_failure(&self) -> &Self {
        assert!(
            !self.success,
            "expected failure, got success\n--- stdout ---\n{}\n--- stderr ---\n{}",
            self.stdout, self.stderr
        );
        self
    }

    #[track_caller]
    pub fn assert_contains(&self, needle: &str) -> &Self {
        assert!(
            self.contains(needle),
            "expected output to contain {needle:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
            self.stdout,
            self.stderr
        );
        self
    }

    #[track_caller]
    pub fn assert_not_contains(&self, needle: &str) -> &Self {
        assert!(
            !self.contains(needle),
            "expected output NOT to contain {needle:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
            self.stdout,
            self.stderr
        );
        self
    }
}
