mod claude;
mod codex;

use crate::agent::UiEvent;
use crate::interrupt::InterruptRx;
use async_trait::async_trait;
use clap::ValueEnum;
use hud_types::{Step, StepSource};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;

pub use claude::ClaudeHarness;
pub use codex::CodexHarness;

const WORKER_INSTRUCTIONS: &str = "You are a worker in a DAS-orchestrated coding session. \
Execute exactly the bounded action you receive in the current workspace. Use your normal coding \
tools, inspect before editing, and verify your work. Do not expand the task. End every turn with a \
compact factual episode describing what you changed or learned, including relevant paths, commands, \
test results, and blockers. That final response is returned to the orchestrator.";

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum WorkerHarnessKind {
    Codex,
    Claude,
}

impl fmt::Display for WorkerHarnessKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Codex => formatter.write_str("codex"),
            Self::Claude => formatter.write_str("claude"),
        }
    }
}

pub struct WorkerHarnessOptions {
    pub kind: WorkerHarnessKind,
    pub workspace: PathBuf,
    pub model: Option<String>,
    pub codex_socket: Option<PathBuf>,
}

pub fn build(options: WorkerHarnessOptions) -> Arc<dyn WorkerHarness> {
    match options.kind {
        WorkerHarnessKind::Codex => Arc::new(CodexHarness::new(
            options.workspace,
            options.model,
            options.codex_socket,
        )),
        WorkerHarnessKind::Claude => Arc::new(ClaudeHarness::new(options.workspace, options.model)),
    }
}

#[async_trait]
pub trait WorkerHarness: Send + Sync {
    fn kind(&self) -> WorkerHarnessKind;
    fn model(&self) -> Option<&str>;

    async fn start(
        &self,
        logical_id: &str,
        seed_episodes: &[String],
        reporter: WorkerReporter,
    ) -> Result<Box<dyn WorkerSession>, WorkerError>;
}

#[async_trait]
pub trait WorkerSession: Send {
    async fn act(
        &mut self,
        action: &str,
        interrupt: &mut InterruptRx,
        timeout: Duration,
    ) -> Result<String, WorkerError>;
}

#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    #[error("worker was interrupted")]
    Interrupted,
    #[error("worker timed out after {seconds}s")]
    Timeout { seconds: u64 },
    #[error("{0}")]
    Failed(String),
}

impl WorkerError {
    pub fn failed(error: impl fmt::Display) -> Self {
        Self::Failed(error.to_string())
    }
}

#[derive(Clone)]
pub struct WorkerReporter {
    thread: String,
    harness: WorkerHarnessKind,
    model: Option<String>,
    events: UnboundedSender<UiEvent>,
    steps: UnboundedSender<Step>,
}

impl WorkerReporter {
    pub fn new(
        thread: String,
        harness: WorkerHarnessKind,
        model: Option<String>,
        events: UnboundedSender<UiEvent>,
        steps: UnboundedSender<Step>,
    ) -> Self {
        Self {
            thread,
            harness,
            model,
            events,
            steps,
        }
    }

    pub fn text(&self, text: impl Into<String>) {
        let _ = self.events.send(UiEvent::WorkerText {
            thread: self.thread.clone(),
            text: text.into(),
        });
    }

    pub fn command(&self, id: &str, command: &str, output: &str, exit_code: i64) {
        let _ = self.events.send(UiEvent::Bash {
            thread: self.thread.clone(),
            command: command.to_string(),
            output: output.to_string(),
            exit_status: exit_code.max(0) as u32,
        });

        let mut step = Step::new(StepSource::Tool);
        let mut payload = Map::new();
        payload.insert(
            "call".to_string(),
            json!({"id": id, "name": "command", "arguments": {"command": command}}),
        );
        payload.insert(
            "result".to_string(),
            json!({
                "content": [{"type": "text", "text": output}],
                "isError": exit_code != 0,
            }),
        );
        step.payload = payload;
        step.extra.insert("thread".to_string(), json!(self.thread));
        step.extra
            .insert("harness".to_string(), json!(self.harness));
        let _ = self.steps.send(step);
    }

    pub fn completed(&self, content: &str, native_session_id: &str, raw: Option<Value>) {
        let mut step = Step::new(StepSource::Agent);
        let mut payload = Map::new();
        payload.insert("content".to_string(), json!(content));
        payload.insert("done".to_string(), json!(true));
        if let Some(model) = &self.model {
            payload.insert("model".to_string(), json!(model));
        }
        if let Some(raw) = raw {
            payload.insert("raw".to_string(), raw);
        }
        step.payload = payload;
        step.extra.insert("thread".to_string(), json!(self.thread));
        step.extra
            .insert("harness".to_string(), json!(self.harness));
        step.extra
            .insert("native_session_id".to_string(), json!(native_session_id));
        let _ = self.steps.send(step);
    }

    pub fn tokens(&self, input: u64, output: u64) {
        let _ = self.events.send(UiEvent::Tokens { input, output });
    }
}

fn worker_instructions(seed_episodes: &[String]) -> String {
    if seed_episodes.is_empty() {
        return WORKER_INSTRUCTIONS.to_string();
    }
    format!(
        "{WORKER_INSTRUCTIONS}\n\nEpisodes from other workers, supplied as initial context:\n\n{}",
        seed_episodes.join("\n\n---\n\n")
    )
}

fn action_prompt(action: &str) -> String {
    format!("Bounded action:\n\n{action}")
}
