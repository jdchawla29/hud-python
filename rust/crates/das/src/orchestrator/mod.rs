mod claude;

use crate::gateway::{Gateway, GatewayError};
use async_trait::async_trait;
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::fmt;

pub use claude::ClaudeOrchestrator;

pub const DEFAULT_GATEWAY_MODEL: &str = "claude-opus-4-8";

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum OrchestratorKind {
    #[default]
    Gateway,
    Claude,
}

impl fmt::Display for OrchestratorKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Gateway => formatter.write_str("gateway"),
            Self::Claude => formatter.write_str("claude"),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum OrchestratorError {
    #[error(transparent)]
    Gateway(#[from] GatewayError),
    #[error("{0}")]
    Failed(String),
}

impl OrchestratorError {
    pub fn failed(error: impl fmt::Display) -> Self {
        Self::Failed(error.to_string())
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
}

#[derive(Debug, Clone)]
pub struct ToolUse {
    pub id: String,
    pub name: String,
    pub input: Map<String, Value>,
}

#[derive(Debug, Clone)]
pub struct ChatResponse {
    pub content: Vec<Value>,
    pub usage: Usage,
}

impl ChatResponse {
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter(|block| block["type"] == "text")
            .filter_map(|block| block["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_string()
    }

    pub fn tool_uses(&self) -> Vec<ToolUse> {
        self.content
            .iter()
            .filter(|block| block["type"] == "tool_use")
            .map(|block| ToolUse {
                id: block["id"].as_str().unwrap_or_default().to_string(),
                name: block["name"].as_str().unwrap_or_default().to_string(),
                input: block["input"].as_object().cloned().unwrap_or_default(),
            })
            .collect()
    }
}

#[async_trait]
pub trait Orchestrator: Send + Sync {
    fn kind(&self) -> OrchestratorKind;
    fn model(&self) -> Option<&str>;

    async fn messages(
        &self,
        system: &str,
        messages: &[Value],
        tools: &[Value],
        max_tokens: u32,
    ) -> Result<ChatResponse, OrchestratorError>;
}

pub struct GatewayOrchestrator {
    gateway: Gateway,
    model: String,
}

impl GatewayOrchestrator {
    pub fn new(gateway: Gateway, model: String) -> Self {
        Self { gateway, model }
    }
}

#[async_trait]
impl Orchestrator for GatewayOrchestrator {
    fn kind(&self) -> OrchestratorKind {
        OrchestratorKind::Gateway
    }

    fn model(&self) -> Option<&str> {
        Some(&self.model)
    }

    async fn messages(
        &self,
        system: &str,
        messages: &[Value],
        tools: &[Value],
        max_tokens: u32,
    ) -> Result<ChatResponse, OrchestratorError> {
        Ok(self
            .gateway
            .messages(&self.model, system, messages, tools, max_tokens)
            .await?)
    }
}
