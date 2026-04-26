use anyhow::{bail, Context, Result};

const DEFAULT_API_BASE: &str = "https://api.enzyme.garden";

fn api_base_url() -> String {
    std::env::var("ENZYME_API_URL").unwrap_or_else(|_| DEFAULT_API_BASE.to_string())
}

/// Deduct credits from the user's account.
/// Returns the remaining balance, or an error if insufficient.
pub async fn deduct_credits(api_key: &str, amount: f64, reason: &str) -> Result<f64> {
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/v1/credits/deduct", api_base_url()))
        .header("Authorization", format!("Bearer {}", api_key))
        .json(&serde_json::json!({ "amount": amount, "reason": reason }))
        .send()
        .await
        .context("failed to contact credits API")?;

    if resp.status().as_u16() == 402 {
        let body: serde_json::Value = resp.json().await.unwrap_or_default();
        let current = body["credits"].as_f64().unwrap_or(0.0);
        bail!(
            "Insufficient credits ({:.1} remaining, {:.1} required). Visit enzyme.garden/dashboard for details.",
            current,
            amount
        );
    }

    if !resp.status().is_success() {
        let body = resp.text().await.unwrap_or_default();
        bail!("Credits API error: {}", body);
    }

    let body: serde_json::Value = resp.json().await.context("failed to parse credits response")?;
    Ok(body["credits"].as_f64().unwrap_or(0.0))
}
