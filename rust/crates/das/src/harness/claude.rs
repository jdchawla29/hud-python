use super::{
    action_prompt, worker_instructions, WorkerError, WorkerHarness, WorkerHarnessKind,
    WorkerReporter, WorkerSession,
};
use crate::interrupt::InterruptRx;
use async_trait::async_trait;
use serde_json::Value;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

pub struct ClaudeHarness {
    workspace: PathBuf,
    model: Option<String>,
}

impl ClaudeHarness {
    pub fn new(workspace: PathBuf, model: Option<String>) -> Self {
        Self { workspace, model }
    }
}

#[async_trait]
impl WorkerHarness for ClaudeHarness {
    fn kind(&self) -> WorkerHarnessKind {
        WorkerHarnessKind::Claude
    }

    fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    async fn start(
        &self,
        logical_id: &str,
        seed_episodes: &[String],
        reporter: WorkerReporter,
    ) -> Result<Box<dyn WorkerSession>, WorkerError> {
        Ok(Box::new(ClaudeSession {
            workspace: self.workspace.clone(),
            model: self.model.clone(),
            logical_id: logical_id.to_string(),
            instructions: worker_instructions(seed_episodes),
            session_id: None,
            reporter,
        }))
    }
}

struct ClaudeSession {
    workspace: PathBuf,
    model: Option<String>,
    logical_id: String,
    instructions: String,
    session_id: Option<String>,
    reporter: WorkerReporter,
}

#[async_trait]
impl WorkerSession for ClaudeSession {
    async fn act(
        &mut self,
        action: &str,
        interrupt: &mut InterruptRx,
        timeout: Duration,
    ) -> Result<String, WorkerError> {
        let mut command = Command::new("claude");
        command
            .current_dir(&self.workspace)
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("ANTHROPIC_AUTH_TOKEN")
            .args(["--print", "--output-format", "json"])
            .args(["--permission-mode", "auto"])
            .args(["--append-system-prompt", &self.instructions])
            .arg("--name")
            .arg(format!("das-{}", self.logical_id));
        if let Some(model) = &self.model {
            command.args(["--model", model]);
        }
        if let Some(session_id) = &self.session_id {
            command.args(["--resume", session_id]);
        }
        command
            .arg(action_prompt(action))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let child = command
            .spawn()
            .map_err(|error| WorkerError::failed(format!("failed to start Claude CLI: {error}")))?;
        let output = child.wait_with_output();
        tokio::pin!(output);
        let deadline = tokio::time::sleep(timeout);
        tokio::pin!(deadline);
        let output = tokio::select! {
            biased;
            _ = interrupt.wait() => return Err(WorkerError::Interrupted),
            _ = &mut deadline => return Err(WorkerError::Timeout {
                seconds: timeout.as_secs(),
            }),
            output = &mut output => output.map_err(WorkerError::failed)?,
        };
        if !output.status.success() {
            return Err(WorkerError::failed(format!(
                "Claude CLI exited with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        let response =
            crate::claude_cli::decode_result(&output.stdout).map_err(WorkerError::failed)?;
        if response
            .get("is_error")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            return Err(WorkerError::failed(format!(
                "Claude CLI failed: {}",
                response.get("result").unwrap_or(&Value::Null)
            )));
        }
        let session_id = response
            .get("session_id")
            .and_then(Value::as_str)
            .ok_or_else(|| WorkerError::failed("Claude CLI returned no session_id"))?
            .to_string();
        self.session_id = Some(session_id.clone());
        let episode = response
            .get("result")
            .and_then(Value::as_str)
            .filter(|result| !result.trim().is_empty())
            .ok_or_else(|| WorkerError::failed("Claude CLI returned no result"))?
            .to_string();
        if let Some(usage) = response.get("usage") {
            self.reporter.tokens(
                usage
                    .get("input_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                usage
                    .get("output_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
            );
        }
        self.reporter.text(&episode);
        self.reporter
            .completed(&episode, &session_id, Some(response));
        Ok(episode)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_claude_json_result_shape() {
        let response: Value = serde_json::from_str(
            r#"{"is_error":false,"session_id":"abc","result":"episode","usage":{"input_tokens":3,"output_tokens":2}}"#,
        )
        .unwrap();
        assert_eq!(response["session_id"], "abc");
        assert_eq!(response["result"], "episode");
    }
}
