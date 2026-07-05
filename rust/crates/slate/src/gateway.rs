//! Minimal Anthropic Messages client against the HUD inference gateway.
//!
//! The gateway speaks the Anthropic API shape at a different base URL with a
//! HUD API key — the same trick the Python SDK's `AsyncAnthropic(base_url=
//! settings.hud_gateway_url)` uses. Message content is kept as raw JSON blocks
//! so assistant turns replay verbatim into the next request.

use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::time::Duration;

pub const DEFAULT_GATEWAY_URL: &str = "https://inference.beta.hud.ai";

#[derive(Debug, thiserror::Error)]
pub enum GatewayError {
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    #[error("gateway returned {status}: {body}")]
    Api { status: u16, body: String },
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
}

/// One tool invocation requested by the model.
#[derive(Debug, Clone)]
pub struct ToolUse {
    pub id: String,
    pub name: String,
    pub input: Map<String, Value>,
}

/// A model response: raw content blocks plus usage.
#[derive(Debug, Clone)]
pub struct ChatResponse {
    pub content: Vec<Value>,
    pub usage: Usage,
}

impl ChatResponse {
    /// All text blocks joined, like the cookbook's `_text_of`.
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter(|b| b["type"] == "text")
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_string()
    }

    pub fn tool_uses(&self) -> Vec<ToolUse> {
        self.content
            .iter()
            .filter(|b| b["type"] == "tool_use")
            .map(|b| ToolUse {
                id: b["id"].as_str().unwrap_or_default().to_string(),
                name: b["name"].as_str().unwrap_or_default().to_string(),
                input: b["input"].as_object().cloned().unwrap_or_default(),
            })
            .collect()
    }
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
