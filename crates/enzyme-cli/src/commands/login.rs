use std::time::Duration;

use anyhow::{bail, Context, Result};
use crossterm::style::{Color, ResetColor, SetForegroundColor};
use enzyme_core::config::auth::AuthToken;
use enzyme_core::config::paths::EnzymePaths;

const DEFAULT_API_BASE: &str = "https://api.enzyme.garden";
const POLL_TIMEOUT: Duration = Duration::from_secs(300);

fn api_base_url() -> String {
    std::env::var("ENZYME_API_URL").unwrap_or_else(|_| DEFAULT_API_BASE.to_string())
}

#[derive(serde::Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    #[serde(default = "default_interval")]
    interval: u64,
    expires_in: u64,
}

fn default_interval() -> u64 {
    5
}

#[derive(serde::Deserialize)]
struct DeviceTokenResponse {
    api_key: String,
    user_id: String,
    #[serde(default)]
    email: String,
}

#[derive(serde::Deserialize)]
struct DeviceTokenError {
    error: String,
}

pub async fn run_login() -> Result<()> {
    let paths_home = EnzymePaths::enzyme_home()?;
    let auth_path = paths_home.join("auth.json");

    // Check if already logged in
    if let Ok(token) = AuthToken::load(&auth_path) {
        eprintln!(
            "  Already logged in as {}{}{}",
            SetForegroundColor(Color::White),
            token.email,
            ResetColor,
        );
        eprintln!(
            "  Run {}enzyme logout{} to sign out first.",
            SetForegroundColor(Color::DarkGrey),
            ResetColor,
        );
        return Ok(());
    }

    // Step 1: Request device code
    let base = api_base_url();
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/api/device/code", base))
        .send()
        .await
        .context("failed to request device code")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("device code request failed ({}): {}", status, body);
    }

    let device_resp: DeviceCodeResponse =
        resp.json().await.context("failed to parse device code response")?;

    // Step 2: Show instructions and open browser
    eprintln!(
        "  {}Opening browser for authentication...{}",
        SetForegroundColor(Color::DarkGrey),
        ResetColor,
    );
    eprintln!();
    eprintln!(
        "  Visit: {}{}{}",
        SetForegroundColor(crate::display::colors::ENZYME_PURPLE),
        device_resp.verification_uri,
        ResetColor,
    );
    eprintln!(
        "  Code:  {}{}{}",
        SetForegroundColor(Color::White),
        device_resp.user_code,
        ResetColor,
    );
    eprintln!();

    // Try to open the browser
    let _ = open_browser(&device_resp.verification_uri);

    // Step 3: Poll for token
    let poll_interval = Duration::from_secs(device_resp.interval);
    let timeout = Duration::from_secs(device_resp.expires_in.min(POLL_TIMEOUT.as_secs()));
    let start = std::time::Instant::now();

    eprint!(
        "  {}Waiting for approval...{}",
        SetForegroundColor(Color::DarkGrey),
        ResetColor,
    );

    loop {
        if start.elapsed() > timeout {
            eprintln!(
                " {}timed out{}",
                SetForegroundColor(Color::Red),
                ResetColor,
            );
            bail!("Device code expired. Run `enzyme login` again.");
        }

        tokio::time::sleep(poll_interval).await;

        let resp = client
            .post(format!("{}/api/device/token", base))
            .json(&serde_json::json!({ "device_code": device_resp.device_code }))
            .send()
            .await;

        let resp = match resp {
            Ok(r) => r,
            Err(_) => continue,
        };

        if resp.status().is_success() {
            let token_resp: DeviceTokenResponse = resp
                .json()
                .await
                .context("failed to parse token response")?;

            let email = token_resp.email.clone();

            let auth_token = AuthToken {
                api_key: token_resp.api_key,
                user_id: token_resp.user_id,
                email: email.clone(),
            };
            auth_token.save(&auth_path)?;

            eprintln!(
                " {}done{}",
                SetForegroundColor(Color::Green),
                ResetColor,
            );
            eprintln!();
            eprintln!(
                "  {}Logged in as {}{}{}",
                SetForegroundColor(Color::DarkGrey),
                SetForegroundColor(Color::White),
                email,
                ResetColor,
            );
            eprintln!();
            return Ok(());
        }

        // Check if it's a "pending" error (keep polling) or a real error
        let body = resp.text().await.unwrap_or_default();
        if let Ok(err) = serde_json::from_str::<DeviceTokenError>(&body) {
            match err.error.as_str() {
                "authorization_pending" | "slow_down" => {
                    eprint!(".");
                    continue;
                }
                "expired_token" => {
                    eprintln!(
                        " {}expired{}",
                        SetForegroundColor(Color::Red),
                        ResetColor,
                    );
                    bail!("Device code expired. Run `enzyme login` again.");
                }
                _ => {
                    eprintln!(
                        " {}failed{}",
                        SetForegroundColor(Color::Red),
                        ResetColor,
                    );
                    bail!("Authentication failed: {}", err.error);
                }
            }
        }
    }
}

pub async fn run_logout() -> Result<()> {
    let paths_home = EnzymePaths::enzyme_home()?;
    let auth_path = paths_home.join("auth.json");

    if auth_path.exists() {
        AuthToken::delete(&auth_path)?;
        eprintln!(
            "  {}Logged out.{}",
            SetForegroundColor(Color::DarkGrey),
            ResetColor,
        );
    } else {
        eprintln!(
            "  {}Not logged in.{}",
            SetForegroundColor(Color::DarkGrey),
            ResetColor,
        );
    }
    Ok(())
}

fn open_browser(url: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open").arg(url).spawn()?;
    }
    #[cfg(target_os = "linux")]
    {
        std::process::Command::new("xdg-open").arg(url).spawn()?;
    }
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("cmd")
            .args(["/c", "start", url])
            .spawn()?;
    }
    Ok(())
}
