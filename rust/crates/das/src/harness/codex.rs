use super::{
    action_prompt, worker_instructions, WorkerError, WorkerHarness, WorkerHarnessKind,
    WorkerReporter, WorkerSession,
};
use crate::interrupt::InterruptRx;
use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::Join;
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{client_async, WebSocketStream};

pub struct CodexHarness {
    workspace: PathBuf,
    model: Option<String>,
    socket: Option<PathBuf>,
}

impl CodexHarness {
    pub fn new(workspace: PathBuf, model: Option<String>, socket: Option<PathBuf>) -> Self {
        Self {
            workspace,
            model,
            socket,
        }
    }
}

#[async_trait]
impl WorkerHarness for CodexHarness {
    fn kind(&self) -> WorkerHarnessKind {
        WorkerHarnessKind::Codex
    }

    fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    async fn start(
        &self,
        _logical_id: &str,
        seed_episodes: &[String],
        reporter: WorkerReporter,
    ) -> Result<Box<dyn WorkerSession>, WorkerError> {
        let mut connection = CodexConnection::connect(self.socket.as_ref()).await?;
        let mut params = json!({
            "cwd": self.workspace,
            "approvalPolicy": "never",
            "sandbox": "workspace-write",
            "developerInstructions": worker_instructions(seed_episodes),
            "serviceName": "das",
        });
        if let Some(model) = &self.model {
            params["model"] = json!(model);
        }
        let result = connection.request("thread/start", params).await?;
        let thread_id = result
            .pointer("/thread/id")
            .and_then(Value::as_str)
            .ok_or_else(|| WorkerError::failed("thread/start returned no thread id"))?
            .to_string();
        Ok(Box::new(CodexSession {
            connection,
            thread_id,
            reporter,
        }))
    }
}

struct CodexSession {
    connection: CodexConnection,
    thread_id: String,
    reporter: WorkerReporter,
}

#[async_trait]
impl WorkerSession for CodexSession {
    async fn act(
        &mut self,
        action: &str,
        interrupt: &mut InterruptRx,
        timeout: Duration,
    ) -> Result<String, WorkerError> {
        let request_id = self.connection.next_id();
        self.connection
            .send(
                request_id,
                "turn/start",
                json!({
                    "threadId": self.thread_id,
                    "input": [{"type": "text", "text": action_prompt(action)}],
                }),
            )
            .await?;
        let result = self.connection.response(request_id).await?;
        let turn_id = result
            .pointer("/turn/id")
            .and_then(Value::as_str)
            .ok_or_else(|| WorkerError::failed("turn/start returned no turn id"))?
            .to_string();

        let mut deadline = Box::pin(tokio::time::sleep(timeout));
        let mut messages = Vec::new();
        loop {
            tokio::select! {
                biased;
                _ = interrupt.wait() => {
                    self.connection.interrupt(&self.thread_id, &turn_id).await?;
                    self.connection.wait_for_turn(&self.thread_id, &turn_id, None).await?;
                    return Err(WorkerError::Interrupted);
                }
                _ = &mut deadline => {
                    self.connection.interrupt(&self.thread_id, &turn_id).await?;
                    self.connection.wait_for_turn(&self.thread_id, &turn_id, None).await?;
                    return Err(WorkerError::Timeout {
                        seconds: timeout.as_secs(),
                    });
                }
                message = self.connection.next_message() => {
                    let message = message?;
                    if let Some(done) = handle_notification(
                        &message,
                        &self.thread_id,
                        &turn_id,
                        Some(&self.reporter),
                        &mut messages,
                    )? {
                        if done != "completed" {
                            return Err(WorkerError::failed(format!("Codex turn ended with status {done}")));
                        }
                        let episode = messages.last().cloned().ok_or_else(|| {
                            WorkerError::failed("Codex completed without an agent message")
                        })?;
                        self.reporter.completed(&episode, &self.thread_id, None);
                        return Ok(episode);
                    }
                }
            }
        }
    }
}

struct CodexConnection {
    child: Child,
    socket: WebSocketStream<Join<ChildStdout, ChildStdin>>,
    request_ids: AtomicU64,
}

impl CodexConnection {
    async fn connect(socket: Option<&PathBuf>) -> Result<Self, WorkerError> {
        let mut command = Command::new("codex");
        command.args(["app-server", "proxy"]);
        if let Some(socket) = socket {
            command.arg("--sock").arg(socket);
        }
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(|error| {
            WorkerError::failed(format!("failed to start codex app-server proxy: {error}"))
        })?;
        let input = child
            .stdin
            .take()
            .ok_or_else(|| WorkerError::failed("codex proxy stdin unavailable"))?;
        let output = child
            .stdout
            .take()
            .ok_or_else(|| WorkerError::failed("codex proxy stdout unavailable"))?;
        let stream = tokio::io::join(output, input);
        let (socket, _) = client_async("ws://localhost/", stream)
            .await
            .map_err(|error| {
                WorkerError::failed(format!(
                    "failed to open WebSocket through codex app-server proxy: {error}"
                ))
            })?;
        let mut connection = Self {
            child,
            socket,
            request_ids: AtomicU64::new(1),
        };
        let initialized = connection
            .request(
                "initialize",
                json!({
                    "clientInfo": {
                        "name": "das",
                        "title": "DAS",
                        "version": env!("CARGO_PKG_VERSION"),
                    }
                }),
            )
            .await;
        if let Err(error) = initialized {
            return Err(WorkerError::failed(format!(
                "failed to initialize Codex daemon connection: {error}; ensure `codex app-server daemon start` succeeds"
            )));
        }
        connection.notification("initialized", json!({})).await?;
        Ok(connection)
    }

    fn next_id(&self) -> u64 {
        self.request_ids.fetch_add(1, Ordering::Relaxed)
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value, WorkerError> {
        let id = self.next_id();
        self.send(id, method, params).await?;
        self.response(id).await
    }

    async fn send(&mut self, id: u64, method: &str, params: Value) -> Result<(), WorkerError> {
        self.write(json!({"method": method, "id": id, "params": params}))
            .await
    }

    async fn notification(&mut self, method: &str, params: Value) -> Result<(), WorkerError> {
        self.write(json!({"method": method, "params": params}))
            .await
    }

    async fn write(&mut self, message: Value) -> Result<(), WorkerError> {
        let text = serde_json::to_string(&message).map_err(WorkerError::failed)?;
        self.socket
            .send(Message::Text(text.into()))
            .await
            .map_err(WorkerError::failed)
    }

    async fn response(&mut self, expected_id: u64) -> Result<Value, WorkerError> {
        loop {
            let message = self.next_message().await?;
            if message.get("id").and_then(Value::as_u64) != Some(expected_id) {
                continue;
            }
            if let Some(error) = message.get("error") {
                return Err(WorkerError::failed(format!(
                    "Codex request {expected_id} failed: {error}"
                )));
            }
            return message
                .get("result")
                .cloned()
                .ok_or_else(|| WorkerError::failed("Codex response had no result"));
        }
    }

    async fn next_message(&mut self) -> Result<Value, WorkerError> {
        loop {
            let message = self
                .socket
                .next()
                .await
                .transpose()
                .map_err(WorkerError::failed)?;
            let decoded = match message {
                Some(Message::Text(text)) => {
                    serde_json::from_str(text.as_ref()).map_err(WorkerError::failed)?
                }
                Some(Message::Binary(bytes)) => {
                    serde_json::from_slice(&bytes).map_err(WorkerError::failed)?
                }
                Some(Message::Close(frame)) => {
                    return Err(WorkerError::failed(format!(
                        "codex app-server proxy closed the WebSocket ({frame:?})"
                    )));
                }
                Some(_) => continue,
                None => {
                    let status = self.child.try_wait().ok().flatten();
                    return Err(WorkerError::failed(format!(
                        "codex app-server proxy closed ({status:?})"
                    )));
                }
            };
            if let Some(response) = server_request_response(&decoded) {
                self.write(response).await?;
                continue;
            }
            return Ok(decoded);
        }
    }

    async fn interrupt(&mut self, thread_id: &str, turn_id: &str) -> Result<(), WorkerError> {
        self.send(
            self.next_id(),
            "turn/interrupt",
            json!({"threadId": thread_id, "turnId": turn_id}),
        )
        .await
    }

    async fn wait_for_turn(
        &mut self,
        thread_id: &str,
        turn_id: &str,
        reporter: Option<&WorkerReporter>,
    ) -> Result<(), WorkerError> {
        let mut messages = Vec::new();
        let deadline = tokio::time::sleep(Duration::from_secs(30));
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                _ = &mut deadline => {
                    return Err(WorkerError::failed("Codex did not acknowledge interruption within 30s"));
                }
                message = self.next_message() => {
                    let message = message?;
                    if handle_notification(
                        &message,
                        thread_id,
                        turn_id,
                        reporter,
                        &mut messages,
                    )?.is_some() {
                        return Ok(());
                    }
                }
            }
        }
    }
}

fn server_request_response(message: &Value) -> Option<Value> {
    let id = message.get("id")?.clone();
    let method = message.get("method")?.as_str()?;
    let result = match method {
        "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
            json!({"decision": "cancel"})
        }
        "item/tool/requestUserInput" => json!({"answers": {}}),
        "mcpServer/elicitation/request" => json!({"action": "decline", "content": null}),
        "currentTime/read" => json!({
            "currentTimeAt": SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
        }),
        _ => {
            return Some(json!({
                "id": id,
                "error": {
                    "code": -32601,
                    "message": format!("DAS workers do not support app-server request {method}"),
                }
            }));
        }
    };
    Some(json!({"id": id, "result": result}))
}

impl Drop for CodexConnection {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

fn handle_notification(
    message: &Value,
    thread_id: &str,
    turn_id: &str,
    reporter: Option<&WorkerReporter>,
    messages: &mut Vec<String>,
) -> Result<Option<String>, WorkerError> {
    let method = message.get("method").and_then(Value::as_str);
    let params = &message["params"];
    let message_turn_id = params
        .get("turnId")
        .and_then(Value::as_str)
        .or_else(|| params.pointer("/turn/id").and_then(Value::as_str));
    if params.get("threadId").and_then(Value::as_str) != Some(thread_id)
        || message_turn_id != Some(turn_id)
    {
        return Ok(None);
    }

    match method {
        Some("item/completed") => {
            let item = &params["item"];
            match item.get("type").and_then(Value::as_str) {
                Some("agentMessage") => {
                    if let Some(text) = item.get("text").and_then(Value::as_str) {
                        if let Some(reporter) = reporter {
                            reporter.text(text);
                        }
                        messages.push(text.to_string());
                    }
                }
                Some("commandExecution") => {
                    let id = item.get("id").and_then(Value::as_str).unwrap_or("command");
                    let command = item.get("command").and_then(Value::as_str).unwrap_or("");
                    let output = item
                        .get("aggregatedOutput")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let exit_code = item.get("exitCode").and_then(Value::as_i64).unwrap_or(1);
                    if let Some(reporter) = reporter {
                        reporter.command(id, command, output, exit_code);
                    }
                }
                _ => {}
            }
            Ok(None)
        }
        Some("turn/completed") => Ok(params
            .pointer("/turn/status")
            .and_then(Value::as_str)
            .map(str::to_string)),
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "requires a running local Codex app-server daemon"]
    async fn connects_to_managed_daemon() {
        CodexConnection::connect(None).await.unwrap();
    }

    #[tokio::test]
    #[ignore = "requires a running local Codex app-server daemon"]
    async fn starts_managed_thread() {
        let mut connection = CodexConnection::connect(None).await.unwrap();
        let result = connection
            .request(
                "thread/start",
                json!({
                    "cwd": std::env::temp_dir(),
                    "approvalPolicy": "never",
                    "sandbox": "workspace-write",
                    "developerInstructions": worker_instructions(&[]),
                    "serviceName": "das-test",
                }),
            )
            .await
            .unwrap();
        let thread_id = result.pointer("/thread/id").unwrap().as_str().unwrap();
        assert!(!thread_id.is_empty());
    }

    #[test]
    fn extracts_agent_message_and_completion() {
        let (events, _) = tokio::sync::mpsc::unbounded_channel();
        let (steps, _) = tokio::sync::mpsc::unbounded_channel();
        let reporter = WorkerReporter::new(
            "thread".to_string(),
            WorkerHarnessKind::Codex,
            None,
            events,
            steps,
        );
        let mut messages = Vec::new();
        let item = json!({
            "method": "item/completed",
            "params": {
                "threadId": "thread",
                "turnId": "turn",
                "item": {"type": "agentMessage", "id": "item", "text": "episode"}
            }
        });
        assert!(
            handle_notification(&item, "thread", "turn", Some(&reporter), &mut messages)
                .unwrap()
                .is_none()
        );
        assert_eq!(messages, ["episode"]);

        let completed = json!({
            "method": "turn/completed",
            "params": {
                "threadId": "thread",
                "turn": {"id": "turn", "items": [], "status": "completed"}
            }
        });
        assert_eq!(
            handle_notification(&completed, "thread", "turn", Some(&reporter), &mut messages)
                .unwrap(),
            Some("completed".to_string())
        );
    }

    #[test]
    fn cancels_interactive_server_requests() {
        let response = server_request_response(&json!({
            "id": 9,
            "method": "item/commandExecution/requestApproval",
            "params": {}
        }))
        .unwrap();
        assert_eq!(response, json!({"id": 9, "result": {"decision": "cancel"}}));
    }
}
