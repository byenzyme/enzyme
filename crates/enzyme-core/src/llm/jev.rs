//! Jev through OpenRouter's Decisions API. This shares the OpenAI-compatible
//! provider configuration with catalyst generation, but uses a distinct
//! endpoint and model setting.
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::llm::{LlmCredentialMode, resolve_api_config_for_mode, resolve_env_api_config};

pub const DEFAULT_JEV_MODEL: &str = "typesafe/jev-1.13";

fn decisions_endpoint(base: &str) -> Result<String> {
    let mut url = reqwest::Url::parse(base).context("Invalid provider base URL")?;
    if url.path().trim_end_matches('/') == "/api/v1" {
        url.set_path("/api/alpha/decisions");
    } else {
        let path = format!("{}/api/alpha/decisions", url.path().trim_end_matches('/'));
        url.set_path(&path);
    }
    Ok(url.to_string())
}

#[derive(Debug, Serialize)]
pub struct JevRequest {
    pub model: String,
    pub state: Value,
    pub questions: BTreeMap<String, JevQuestion>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum JevQuestion {
    Choice {
        instructions: String,
        criteria: BTreeMap<String, String>,
    },
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum JevAnswer {
    Choice {
        choice: String,
        probabilities: BTreeMap<String, f64>,
        confidence: f64,
    },
}

impl JevAnswer {
    pub fn choice(&self) -> &str {
        let Self::Choice { choice, .. } = self;
        choice
    }

    pub fn confidence(&self) -> f64 {
        let Self::Choice { confidence, .. } = self;
        *confidence
    }

    pub fn choice_matches_probabilities(&self) -> bool {
        let Self::Choice {
            choice,
            probabilities,
            ..
        } = self;
        probabilities
            .get(choice)
            .is_some_and(|chosen| probabilities.values().all(|p| *p <= chosen + 1e-6))
    }

    pub fn is_decisive(&self, threshold: f64) -> bool {
        self.confidence() >= threshold && self.choice_matches_probabilities()
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct JevResponse {
    pub model: String,
    pub answers: BTreeMap<String, JevAnswer>,
    #[serde(default)]
    pub usage: Value,
}

impl JevResponse {
    pub fn validate(&self, request: &JevRequest) -> Result<()> {
        ensure!(
            !self.model.is_empty(),
            "Jev response omitted model identity"
        );
        ensure!(
            self.answers.len() == request.questions.len(),
            "Jev returned an incomplete answer set"
        );
        for (id, question) in &request.questions {
            let answer = self
                .answers
                .get(id)
                .context("Jev omitted a requested answer")?;
            let JevQuestion::Choice { criteria, .. } = question;
            let JevAnswer::Choice {
                choice,
                probabilities,
                confidence,
            } = answer;
            ensure!(
                criteria.contains_key(choice),
                "Jev returned an unknown choice"
            );
            ensure!(
                confidence.is_finite() && (0.0..=1.0).contains(confidence),
                "Jev returned invalid confidence"
            );
            ensure!(
                probabilities.len() == criteria.len()
                    && criteria.keys().all(|key| probabilities.contains_key(key)),
                "Jev returned an invalid probability domain"
            );
            ensure!(
                probabilities
                    .values()
                    .all(|p| p.is_finite() && (0.0..=1.0).contains(p)),
                "Jev returned invalid probabilities"
            );
            // The live API rounds each probability to two decimal places.
            // Allow each entry its half-unit rounding error, but do not silently
            // normalize a malformed distribution or use it as calibrated truth.
            let rounding_tolerance = probabilities.len() as f64 * 0.005 + 1e-9;
            ensure!(
                (probabilities.values().sum::<f64>() - 1.0).abs() <= rounding_tolerance,
                "Jev probabilities do not sum to one within rounding tolerance"
            );
            // A live response can disagree with its own distribution. Preserve
            // that evidence; selection treats this answer as an abstention via
            // is_decisive(), rather than failing every other candidate.
        }
        Ok(())
    }
}

// Intentionally not Debug: this object owns a credential.
pub struct JevClient {
    http: reqwest::Client,
    endpoint: String,
    api_key: String,
    model: String,
}

impl JevClient {
    /// Prefer an explicit OpenAI-compatible environment override. Otherwise,
    /// fetch the same Enzyme-managed hosted lease used by catalyst generation.
    pub async fn resolve() -> Result<Self> {
        let config = match resolve_env_api_config()? {
            Some(config) => config,
            None => resolve_api_config_for_mode(LlmCredentialMode::Hosted)
                .await
                .context("Jev selection could not resolve Enzyme hosted access")?,
        };
        let model = std::env::var("ENZYME_JEV_MODEL").unwrap_or_else(|_| DEFAULT_JEV_MODEL.into());
        let endpoint = decisions_endpoint(&config.base_url)?;
        let url = reqwest::Url::parse(&endpoint).context("Invalid OpenRouter Decisions URL")?;
        ensure!(
            url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none(),
            "OpenRouter Decisions URL must not contain credentials, query, or fragment"
        );
        ensure!(
            url.scheme() == "https"
                || (url.scheme() == "http"
                    && matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"))),
            "OpenRouter Decisions URL requires HTTPS except for loopback testing"
        );
        Self::new(url.as_str(), config.api_key, model)
    }

    fn new(endpoint: &str, api_key: String, model: String) -> Result<Self> {
        ensure!(!api_key.trim().is_empty(), "Jev API key is empty");
        ensure!(!model.trim().is_empty(), "ENZYME_JEV_MODEL is empty");
        Ok(Self {
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(5))
                .timeout(Duration::from_secs(20))
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            endpoint: endpoint.to_string(),
            api_key,
            model,
        })
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    /// One request, no hidden retries: latency and failures remain observable.
    /// Neither credentials, note excerpts, nor provider error bodies are logged.
    pub async fn evaluate(&self, request: &JevRequest) -> Result<(JevResponse, u128)> {
        ensure!(
            !request.questions.is_empty(),
            "Jev needs at least one question"
        );
        ensure!(
            request.model == self.model,
            "Jev request used an unexpected model"
        );
        let start = Instant::now();
        let response = self
            .http
            .post(&self.endpoint)
            .bearer_auth(&self.api_key)
            .header("HTTP-Referer", "https://github.com/byenzyme/enzyme")
            .header("X-Title", "Enzyme")
            .json(request)
            .send()
            .await
            .context("Jev selection request failed")?;
        let status = response.status();
        if !status.is_success() {
            bail!("Jev selection returned HTTP {status}; no configuration was written");
        }
        let result: JevResponse = response
            .json()
            .await
            .context("Invalid Jev selection response")?;
        result.validate(request)?;
        Ok((result, start.elapsed().as_millis()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{body_json, header, method, path},
    };

    fn request() -> JevRequest {
        JevRequest {
            model: DEFAULT_JEV_MODEL.into(),
            state: json!({"sample":"ordinary notes"}),
            questions: BTreeMap::from([(
                "select".into(),
                JevQuestion::Choice {
                    instructions: "Select?".into(),
                    criteria: BTreeMap::from([
                        ("yes".into(), "Useful".into()),
                        ("no".into(), "Incidental".into()),
                    ]),
                },
            )]),
        }
    }

    #[test]
    fn openrouter_chat_base_maps_to_decisions_endpoint() {
        assert_eq!(
            decisions_endpoint("https://openrouter.ai/api/v1").unwrap(),
            "https://openrouter.ai/api/alpha/decisions"
        );
        assert_eq!(
            decisions_endpoint("http://localhost:3000").unwrap(),
            "http://localhost:3000/api/alpha/decisions"
        );
    }

    fn response() -> Value {
        json!({"model":DEFAULT_JEV_MODEL,"answers":{"select":{"type":"choice","choice":"yes","probabilities":{"yes":0.9,"no":0.1},"confidence":0.8}},"usage":{"input_tokens":42}})
    }

    #[tokio::test]
    async fn jev_client_uses_openrouter_decisions_api_and_validates_answers() {
        let server = MockServer::start().await;
        let request = request();
        Mock::given(method("POST"))
            .and(path("/api/alpha/decisions"))
            .and(header("authorization", "Bearer test-key"))
            .and(body_json(serde_json::to_value(&request).unwrap()))
            .respond_with(ResponseTemplate::new(200).set_body_json(response()))
            .expect(1)
            .mount(&server)
            .await;
        let client = JevClient::new(
            &format!("{}/api/alpha/decisions", server.uri()),
            "test-key".into(),
            DEFAULT_JEV_MODEL.into(),
        )
        .unwrap();
        let (result, _) = client.evaluate(&request).await.unwrap();
        assert_eq!(result.answers["select"].choice(), "yes");
    }

    #[tokio::test]
    async fn jev_client_does_not_retry_or_echo_error_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(429).set_body_string("private note and credential"))
            .expect(1)
            .mount(&server)
            .await;
        let client =
            JevClient::new(&server.uri(), "test-key".into(), DEFAULT_JEV_MODEL.into()).unwrap();
        let error = client.evaluate(&request()).await.unwrap_err().to_string();
        assert!(error.contains("429"));
        assert!(!error.contains("private"));
    }

    #[test]
    fn jev_accepts_provider_rounding_without_normalizing() {
        let mut request = request();
        let JevQuestion::Choice { criteria, .. } = request.questions.get_mut("select").unwrap();
        criteria.insert("uncertain".into(), "Unclear".into());
        let response: JevResponse = serde_json::from_value(json!({"model":"jev-1.13.0","answers":{
            "select":{"type":"choice","choice":"yes","probabilities":{"yes":0.34,"no":0.34,"uncertain":0.33},"confidence":0.01}
        }})).unwrap();
        response.validate(&request).unwrap();
    }

    #[test]
    fn jev_inconsistent_choice_is_preserved_but_not_decisive() {
        let mut value = response();
        value["answers"]["select"]["choice"] = json!("no");
        value["answers"]["select"]["confidence"] = json!(0.99);
        let response: JevResponse = serde_json::from_value(value).unwrap();
        response.validate(&request()).unwrap();
        assert_eq!(response.answers["select"].choice(), "no");
        assert!(!response.answers["select"].is_decisive(0.8));
    }

    #[test]
    fn jev_rejects_missing_unknown_and_inconsistent_answers() {
        for value in [
            json!({"model":"jev","answers":{}}),
            {
                let mut v = response();
                v["answers"]["select"]["choice"] = json!("invented");
                v
            },
            {
                let mut v = response();
                v["answers"]["select"]["probabilities"]["yes"] = json!(0.2);
                v
            },
            {
                let mut v = response();
                v["answers"]["select"]["confidence"] = json!(1.5);
                v
            },
        ] {
            let response: JevResponse = serde_json::from_value(value).unwrap();
            assert!(response.validate(&request()).is_err());
        }
    }
}
