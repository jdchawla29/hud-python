use super::{ChatResponse, Orchestrator, OrchestratorError, OrchestratorKind};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::Stdio;
use tokio::process::Command;
use tokio::sync::Mutex;

const DECISION_SCHEMA: &str = r#"{
  "type": "object",
  "properties": {
    "action": {"type": "string", "enum": ["dispatch", "finish"]},
    "message": {"type": "string"},
    "dispatches": {
      "type": "array",
      "items": {
        "type": "object",
        "properties": {
          "thread_id": {"type": "string"},
          "action": {"type": "string"},
          "seed_episodes": {"type": "array", "items": {"type": "string"}}
        },
        "required": ["thread_id", "action", "seed_episodes"],
        "additionalProperties": false
      }
    },
    "answer": {"type": "string"}
  },
  "required": ["action", "message", "dispatches", "answer"],
  "additionalProperties": false
}"#;

const PROTOCOL: &str = "You are running as DAS's tool-disabled orchestrator through Claude Code. \
Return one structured decision. For action=dispatch, provide one or more bounded worker actions and \
leave answer empty. For action=finish, provide no dispatches and put the complete user-facing answer \
in answer. message is optional progress text. Never attempt to inspect or modify the workspace yourself.";

pub struct ClaudeOrchestrator {
    workspace: PathBuf,
    model: Option<String>,
    state: Mutex<ClaudeState>,
}

#[derive(Default)]
struct ClaudeState {
    session_id: Option<String>,
    seen_messages: usize,
}

impl ClaudeOrchestrator {
    pub fn new(workspace: PathBuf, model: Option<String>) -> Self {
        Self {
            workspace,
            model,
            state: Mutex::new(ClaudeState::default()),
        }
    }
}

#[async_trait]
impl Orchestrator for ClaudeOrchestrator {
    fn kind(&self) -> OrchestratorKind {
        OrchestratorKind::Claude
    }

    fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    async fn messages(
        &self,
        system: &str,
        messages: &[Value],
        tools: &[Value],
        _max_tokens: u32,
    ) -> Result<ChatResponse, OrchestratorError> {
        let mut state = self.state.lock().await;
        let start = if state.session_id.is_some() {
            state.seen_messages.min(messages.len())
        } else {
            0
        };
        let prompt = serde_json::to_string_pretty(&json!({
            "conversation": &messages[start..],
            "available_actions": tools,
        }))
        .map_err(OrchestratorError::failed)?;

        let mut command = Command::new("claude");
        command
            .current_dir(&self.workspace)
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("ANTHROPIC_AUTH_TOKEN")
            .args(["--print", "--output-format", "json"])
            .arg("--safe-mode")
            .arg("--tools")
            .arg("")
            .args(["--permission-mode", "dontAsk"])
            .arg("--system-prompt")
            .arg(format!("{system}\n\n{PROTOCOL}"))
            .args(["--json-schema", DECISION_SCHEMA])
            .args(["--name", "das-orchestrator"]);
        if let Some(model) = &self.model {
            command.args(["--model", model]);
        }
        if let Some(session_id) = &state.session_id {
            command.args(["--resume", session_id]);
        }
        command
            .arg(prompt)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let output = command.output().await.map_err(|error| {
            OrchestratorError::failed(format!("failed to start Claude CLI: {error}"))
        })?;
        if !output.status.success() {
            return Err(OrchestratorError::failed(format!(
                "Claude CLI exited with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        let response =
            crate::claude_cli::decode_result(&output.stdout).map_err(OrchestratorError::failed)?;
        if response
            .get("is_error")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            return Err(OrchestratorError::failed(format!(
                "Claude CLI failed: {}",
                response.get("result").unwrap_or(&Value::Null)
            )));
        }
        let session_id = response
            .get("session_id")
            .and_then(Value::as_str)
            .ok_or_else(|| OrchestratorError::failed("Claude CLI returned no session_id"))?
            .to_string();
        let parsed = parse_response(&response)?;
        state.session_id = Some(session_id);
        state.seen_messages = messages.len().saturating_add(1);
        Ok(parsed)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum DecisionAction {
    Dispatch,
    Finish,
}

#[derive(Deserialize)]
struct Decision {
    action: DecisionAction,
    message: String,
    dispatches: Vec<Dispatch>,
    answer: String,
}

#[derive(Deserialize)]
struct Dispatch {
    thread_id: String,
    action: String,
    seed_episodes: Vec<String>,
}

fn parse_response(response: &Value) -> Result<ChatResponse, OrchestratorError> {
    let value = response
        .get("structured_output")
        .cloned()
        .or_else(|| {
            response
                .get("result")
                .filter(|value| value.is_object())
                .cloned()
        })
        .or_else(|| {
            response
                .get("result")
                .and_then(Value::as_str)
                .and_then(|result| serde_json::from_str(result).ok())
        })
        .ok_or_else(|| OrchestratorError::failed("Claude CLI returned no structured output"))?;
    let decision: Decision = serde_json::from_value(value).map_err(|error| {
        OrchestratorError::failed(format!("Claude CLI returned an invalid decision: {error}"))
    })?;
    let mut content = Vec::new();
    if !decision.message.trim().is_empty() {
        content.push(json!({"type": "text", "text": decision.message}));
    }
    match decision.action {
        DecisionAction::Dispatch => {
            if decision.dispatches.is_empty() || !decision.answer.trim().is_empty() {
                return Err(OrchestratorError::failed(
                    "Claude dispatch decisions require dispatches and an empty answer",
                ));
            }
            for dispatch in decision.dispatches {
                if dispatch.thread_id.trim().is_empty() || dispatch.action.trim().is_empty() {
                    return Err(OrchestratorError::failed(
                        "Claude returned a dispatch with an empty thread_id or action",
                    ));
                }
                content.push(json!({
                    "type": "tool_use",
                    "id": format!("claude-{}", uuid::Uuid::new_v4().simple()),
                    "name": "dispatch",
                    "input": {
                        "thread_id": dispatch.thread_id,
                        "action": dispatch.action,
                        "seed_episodes": dispatch.seed_episodes,
                    },
                }));
            }
        }
        DecisionAction::Finish => {
            if !decision.dispatches.is_empty() || decision.answer.trim().is_empty() {
                return Err(OrchestratorError::failed(
                    "Claude finish decisions require an answer and no dispatches",
                ));
            }
            content.push(json!({
                "type": "tool_use",
                "id": format!("claude-{}", uuid::Uuid::new_v4().simple()),
                "name": "finish",
                "input": {"answer": decision.answer},
            }));
        }
    }
    Ok(ChatResponse {
        content,
        usage: serde_json::from_value(response["usage"].clone()).unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_structured_dispatches_to_tool_uses() {
        let response = json!({
            "structured_output": {
                "action": "dispatch",
                "message": "checking",
                "dispatches": [{
                    "thread_id": "tests",
                    "action": "run the focused tests",
                    "seed_episodes": []
                }],
                "answer": ""
            },
            "usage": {"input_tokens": 4, "output_tokens": 2}
        });

        let parsed = parse_response(&response).unwrap();
        assert_eq!(parsed.text(), "checking");
        let tools = parsed.tool_uses();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "dispatch");
        assert_eq!(tools[0].input["thread_id"], "tests");
        assert_eq!(parsed.usage.input_tokens, 4);
    }

    #[test]
    fn rejects_ambiguous_finish_decisions() {
        let response = json!({
            "result": {
                "action": "finish",
                "message": "",
                "dispatches": [{
                    "thread_id": "tests",
                    "action": "run tests",
                    "seed_episodes": []
                }],
                "answer": "done"
            }
        });

        assert!(parse_response(&response).is_err());
    }
}
