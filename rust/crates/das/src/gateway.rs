//! Minimal Anthropic Messages client against the HUD inference gateway.
//!
//! The gateway speaks the Anthropic API shape at a different base URL with a
//! HUD API key — the same trick the Python SDK's `AsyncAnthropic(base_url=
//! settings.hud_gateway_url)` uses. Message content is kept as raw JSON blocks
//! so assistant turns replay verbatim into the next request.

use crate::orchestrator::ChatResponse;
use serde_json::{json, Value};
use std::time::Duration;

pub const DEFAULT_GATEWAY_URL: &str = "https://inference.beta.hud.ai";

#[derive(Debug, thiserror::Error)]
pub enum GatewayError {
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    #[error("gateway returned {status}: {body}")]
    Api { status: u16, body: String },
}

#[derive(Clone)]
pub struct Gateway {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
}

impl Gateway {
    pub fn new(base_url: &str, api_key: &str) -> Gateway {
        Gateway {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(600))
                .build()
                .expect("reqwest client builds"),
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.to_string(),
        }
    }

    /// One `messages` call. Retries transient failures (429/5xx/transport)
    /// twice with backoff.
    pub async fn messages(
        &self,
        model: &str,
        system: &str,
        messages: &[Value],
        tools: &[Value],
        max_tokens: u32,
    ) -> Result<ChatResponse, GatewayError> {
        let mut body = json!({
            "model": model,
            "max_tokens": max_tokens,
            "system": system,
            "messages": messages,
        });
        if !tools.is_empty() {
            body["tools"] = json!(tools);
        }

        let mut last: Option<GatewayError> = None;
        for attempt in 0..3u32 {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_secs(1 << attempt)).await;
            }
            let sent = self
                .http
                .post(format!("{}/v1/messages", self.base_url))
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", "2023-06-01")
                .json(&body)
                .send()
                .await;
            let response = match sent {
                Ok(response) => response,
                Err(e) => {
                    last = Some(e.into());
                    continue;
                }
            };
            let status = response.status();
            if status.is_success() {
                let parsed: Value = response.json().await?;
                return Ok(ChatResponse {
                    content: parsed["content"].as_array().cloned().unwrap_or_default(),
                    usage: serde_json::from_value(parsed["usage"].clone()).unwrap_or_default(),
                });
            }
            let body_text = response.text().await.unwrap_or_default();
            let error = GatewayError::Api {
                status: status.as_u16(),
                body: body_text,
            };
            if status.as_u16() == 429 || status.is_server_error() {
                last = Some(error);
                continue;
            }
            return Err(error);
        }
        Err(last.expect("retry loop records an error"))
    }
}
