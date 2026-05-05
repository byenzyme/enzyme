use std::time::Duration;
use std::{io::Write, time::Instant};

use anyhow::{Context, Result, bail};
use crossterm::style::{Color, ResetColor, SetForegroundColor};
use enzyme_core::config::auth::AuthToken;
use enzyme_core::config::paths::EnzymePaths;
use serde_json::json;

const DEFAULT_API_BASE: &str = "https://api.enzyme.garden";
const POLL_TIMEOUT: Duration = Duration::from_secs(300);

fn api_base_url() -> String {
    std::env::var("ENZYME_API_URL").unwrap_or_else(|_| DEFAULT_API_BASE.to_string())
}

#[derive(serde::Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
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

pub async fn run_login(no_open: bool, json_output: bool) -> Result<()> {
    let paths_home = EnzymePaths::enzyme_home()?;
    let auth_path = paths_home.join("auth.json");

    // Check if already logged in
    if let Ok(token) = AuthToken::load(&auth_path) {
        if json_output {
            println!(
                "{}",
                serde_json::to_string(&json!({
                    "event": "already_logged_in",
                    "email": token.email,
                    "auth_path": auth_path.display().to_string(),
                }))?
            );
            return Ok(());
        }
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

    let device_resp: DeviceCodeResponse = resp
        .json()
        .await
        .context("failed to parse device code response")?;

    // Step 2: Show instructions and open browser
    if json_output {
        println!(
            "{}",
            serde_json::to_string(&json!({
                "event": "device_authorization",
                "verification_uri": device_resp.verification_uri,
                "expires_in": device_resp.expires_in,
                "interval": device_resp.interval,
                "auth_path": auth_path.display().to_string(),
                "opened_browser": !no_open,
            }))?
        );
        std::io::stdout().flush().ok();
    } else {
        if no_open {
            eprintln!(
                "  {}Open this URL to authenticate Enzyme:{}",
                SetForegroundColor(Color::DarkGrey),
                ResetColor,
            );
        } else {
            eprintln!(
                "  {}Opening browser for authentication...{}",
                SetForegroundColor(Color::DarkGrey),
                ResetColor,
            );
        }
        eprintln!();
        eprintln!(
            "  Visit: {}{}{}",
            SetForegroundColor(crate::display::colors::ENZYME_PURPLE),
            device_resp.verification_uri,
            ResetColor,
        );
        eprintln!();
    }

    // Try to open the browser
    if !no_open {
        let _ = open_browser(&device_resp.verification_uri);
    }

    // Step 3: Poll for token
    let poll_interval = Duration::from_secs(device_resp.interval);
    let timeout = Duration::from_secs(device_resp.expires_in.min(POLL_TIMEOUT.as_secs()));
    let start = Instant::now();

    if !json_output {
        eprint!(
            "  {}Waiting for approval...{}",
            SetForegroundColor(Color::DarkGrey),
            ResetColor,
        );
    }

    loop {
        if start.elapsed() > timeout {
            if json_output {
                println!(
                    "{}",
                    serde_json::to_string(&json!({
                        "event": "expired",
                        "message": "Device code expired. Run `enzyme login` again.",
                    }))?
                );
            }
            eprintln!(" {}timed out{}", SetForegroundColor(Color::Red), ResetColor,);
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

            if json_output {
                println!(
                    "{}",
                    serde_json::to_string(&json!({
                        "event": "success",
                        "email": email,
                        "auth_path": auth_path.display().to_string(),
                    }))?
                );
            } else {
                eprintln!(" {}done{}", SetForegroundColor(Color::Green), ResetColor,);
                eprintln!();
                eprintln!(
                    "  {}Logged in as {}{}{}",
                    SetForegroundColor(Color::DarkGrey),
                    SetForegroundColor(Color::White),
                    email,
                    ResetColor,
                );
                eprintln!();
            }
            return Ok(());
        }

        // Check if it's a "pending" error (keep polling) or a real error
        let body = resp.text().await.unwrap_or_default();
        if let Ok(err) = serde_json::from_str::<DeviceTokenError>(&body) {
            match err.error.as_str() {
                "authorization_pending" | "slow_down" => {
                    if !json_output {
                        eprint!(".");
                    }
                    continue;
                }
                "expired_token" => {
                    if json_output {
                        println!(
                            "{}",
                            serde_json::to_string(&json!({
                                "event": "expired",
                                "message": "Device code expired. Run `enzyme login` again.",
                            }))?
                        );
                    }
                    eprintln!(" {}expired{}", SetForegroundColor(Color::Red), ResetColor,);
                    bail!("Device code expired. Run `enzyme login` again.");
                }
                _ => {
                    let error = err.error;
                    if json_output {
                        println!(
                            "{}",
                            serde_json::to_string(&json!({
                                "event": "error",
                                "error": error,
                            }))?
                        );
                    }
                    eprintln!(" {}failed{}", SetForegroundColor(Color::Red), ResetColor,);
                    bail!("Authentication failed: {}", error);
                }
            }
        }
    }
}

pub async fn run_logout() -> Result<()> {
    let paths_home = EnzymePaths::enzyme_home()?;
    let auth_path = paths_home.join("auth.json");

    if auth_path.exists() {
        // Revoke the API key server-side before deleting locally
        if let Ok(token) = AuthToken::load(&auth_path) {
            let base = api_base_url();
            let client = reqwest::Client::new();
            // Best-effort revoke — don't fail logout if server is unreachable
            let _ = client
                .delete(format!("{}/v1/api-keys/current", base))
                .header("Authorization", format!("Bearer {}", token.api_key))
                .send()
                .await;
        }
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
