//! A run: its record ([`Run`]) and the local driver that produces one
//! ([`rollout`]).
//!
//! `rollout` connects to a substrate's control channel (wherever it is —
//! loopback, a container, a cloud sandbox), starts the task, drives the agent,
//! grades, and tears down, filling a [`Run`] along the way. It is the
//! *client-here* path: the agent loop runs in this process against a
//! [`Provider`]'s channel.

use crate::agent::Agent;
use crate::runtime::{Provider, RuntimeGuard};
use hud_client::{connect, ConnectOptions, HudClient, HudClientError};
use hud_types::{
    coerce_prompt_message, now_iso, Content, Grade, PromptMessage, Step, TaskCall, TaskPhase,
    TaskRow, Trace, TraceStatus,
};
use serde_json::{json, Map, Value};
use std::time::Duration;
use tokio::time::Instant;

/// Live handle for one task: the task lifecycle plus the agent's [`Trace`].
pub struct Run {
    client: Option<HudClient>,
    task_id: String,
    args: Map<String, Value>,
    /// The task's opening prompt as `tasks.start` returned it: plain text, or
    /// a list of message dicts for chat-style prompts. Agents consume the
    /// normalized views [`Run::prompt_messages`] / [`Run::prompt_text`].
    pub prompt: Option<Value>,
    /// The structured grading result (all-default until graded on exit).
    pub grade: Grade,
    pub trace: Trace,
    /// Batch this run belongs to (set by the runner).
    pub job_id: Option<String>,
    pub group_id: Option<String>,
    /// The task slug this run came from; keys runs back to their task.
    pub slug: Option<String>,
    runtime_url: Option<String>,
}

impl Run {
    fn new(client: HudClient, task_id: &str, args: Map<String, Value>) -> Run {
        Run {
            client: Some(client),
            task_id: task_id.to_string(),
            args,
            prompt: None,
            grade: Grade::default(),
            trace: Trace::default(),
            job_id: None,
            group_id: None,
            slug: None,
            runtime_url: None,
        }
    }

    /// A spent run representing a rollout that failed before launching.
    pub fn failed(error: impl Into<String>) -> Run {
        let mut run = Run {
            client: None,
            task_id: String::new(),
            args: Map::new(),
            prompt: None,
            grade: Grade::default(),
            trace: Trace::default(),
            job_id: None,
            group_id: None,
            slug: None,
            runtime_url: None,
        };
        run.trace.status = Some(TraceStatus::Error);
        run.trace.record(Step::system_error(error.into()));
        run
    }

    /// The live client driving this run.
    ///
    /// # Panics
    /// On a [`Run::failed`] run (no live client), matching the Python SDK's
    /// `RuntimeError` — accessing a dead run's client is an authoring bug.
    pub fn client(&mut self) -> &mut HudClient {
        self.client
            .as_mut()
            .expect("this run has no live client (it failed before launch)")
    }

    pub fn has_client(&self) -> bool {
        self.client.is_some()
    }

    /// The graded reward (`grade.reward`).
    pub fn reward(&self) -> f64 {
        self.grade.reward
    }

    /// The raw evaluation dict the env returned (`grade.raw`).
    pub fn evaluation(&self) -> &Map<String, Value> {
        &self.grade.raw
    }

    /// Keys the agent's trajectory.
    pub fn trace_id(&self) -> Option<&str> {
        self.trace.trace_id.as_deref()
    }

    /// Control-channel url of the runtime this run executed against; `None`
    /// on a run that failed before a substrate came up.
    pub fn runtime(&self) -> Option<&str> {
        self.runtime_url.as_deref()
    }

    /// The prompt as normalized [`PromptMessage`] turns: a text prompt (or
    /// none) is one user turn; chat-style lists map turn by turn.
    pub fn prompt_messages(&self) -> Vec<PromptMessage> {
        match &self.prompt {
            None | Some(Value::Null) => vec![PromptMessage::user("")],
            Some(Value::String(text)) => vec![PromptMessage::user(text.clone())],
            Some(Value::Array(items)) => items.iter().map(coerce_prompt_message).collect(),
            Some(other) => vec![coerce_prompt_message(other)],
        }
    }

    /// The prompt flattened to plain text, for string-only agent backends.
    /// Non-text content (images, resources) is dropped.
    pub fn prompt_text(&self) -> String {
        self.prompt_messages()
            .iter()
            .filter_map(|m| match &m.content {
                Content::Text { text } if !text.is_empty() => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    /// Record one step on the trace.
    pub fn record(&mut self, step: Step) {
        self.trace.record(step);
    }

    /// Start the task: `tasks.start`, then record the setup + prompt steps.
    async fn start(&mut self) -> Result<(), HudClientError> {
        let started_at = now_iso();
        let task_id = self.task_id.clone();
        let args = self.args.clone();
        let started = self.client().start_task(&task_id, args.clone()).await?;
        self.prompt = match started.get("prompt") {
            None | Some(Value::Null) => None,
            Some(prompt) => Some(prompt.clone()),
        };
        self.record(Step::task(
            TaskCall {
                phase: TaskPhase::Setup,
                name: task_id,
                arguments: Value::Object(args),
                result: Value::Object(started),
            },
            started_at,
        ));
        if self.prompt.is_some() {
            self.record(Step::user(self.prompt_messages()));
        }
        Ok(())
    }

    /// Grade the run (`tasks.grade` with the trace's final content).
    ///
    /// A mid-run error grades best-effort (capture a salvageable reward, keep
    /// `status = error`) and never masks the original error; a clean run
    /// grades normally — a grader fault propagates.
    async fn finish(&mut self, after_error: bool) -> Result<(), HudClientError> {
        let mut answer = Map::new();
        answer.insert(
            "answer".to_string(),
            self.trace
                .content
                .clone()
                .map(Value::String)
                .unwrap_or(Value::Null),
        );
        let started_at = now_iso();

        let evaluation = match self.client().grade(answer.clone()).await {
            Ok(evaluation) => evaluation,
            Err(e) if after_error => {
                tracing::warn!("grade failed after mid-run error: {e}");
                return Ok(());
            }
            Err(e) => return Err(e),
        };

        self.grade = Grade::from_wire(evaluation.clone());
        let error = if self.grade.is_error {
            self.grade.content.clone()
        } else {
            None
        };
        let task_id = self.task_id.clone();
        self.record(Step {
            error,
            ..Step::task(
                TaskCall {
                    phase: TaskPhase::Evaluate,
                    name: task_id,
                    arguments: Value::Object(answer),
                    result: Value::Object(evaluation),
                },
                started_at,
            )
        });
        if self.trace.status.is_none() {
            self.trace.status = Some(TraceStatus::Completed);
        }
        Ok(())
    }

    async fn close_client(&mut self) {
        if let Some(client) = self.client.take() {
            client.close().await;
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct RolloutOptions {
    /// Batch identity threaded by the scheduler; minted per rollout when
    /// absent (there are no standalone runs).
    pub job_id: Option<String>,
    pub group_id: Option<String>,
    pub trace_id: Option<String>,
    /// Hard wall-clock cap for the whole rollout: one shared deadline across
    /// provision, connect, and the agent loop — not a fresh budget per phase.
    pub rollout_timeout: Option<Duration>,
}

/// Drive one task to a graded [`Run`] here, against `provider`'s channel.
///
/// The local driver (*client-here*): acquire the provider's substrate,
/// connect, start the task, let `agent` fill `run.trace`, grade on exit,
/// tear down.
///
/// Failures are isolated so one bad rollout never collapses a batch, without
/// erasing evidence: a failure *before* the run is live (provision, connect,
/// start) yields a synthesized [`Run::failed`]; a failure *mid-run* keeps the
/// real run — prompt, placement record, and the partial trace — marked as
/// errored, and still graded best-effort. Either way the recorded error names
/// the lifecycle phase (`provisioning`, `starting task`, `agent loop`,
/// `grading`).
pub async fn rollout(
    task: &TaskRow,
    agent: &dyn Agent,
    provider: &dyn Provider,
    options: RolloutOptions,
) -> Run {
    let job_id = options
        .job_id
        .unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string());
    let trace_id = options
        .trace_id
        .unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string());
    let deadline = options.rollout_timeout.map(|t| Instant::now() + t);

    let mut run = drive(task, agent, provider, deadline, options.rollout_timeout).await;

    run.trace.trace_id = Some(trace_id);
    run.job_id = Some(job_id);
    run.group_id = options.group_id;
    run.slug = Some(task.effective_slug());
    run
}

async fn drive(
    task: &TaskRow,
    agent: &dyn Agent,
    provider: &dyn Provider,
    deadline: Option<Instant>,
    rollout_timeout: Option<Duration>,
) -> Run {
    let timeout_detail = || match rollout_timeout {
        Some(t) => format!("timed out after {:.0}s", t.as_secs_f64()),
        None => "timed out".to_string(),
    };

    // Setup (provision + connect + start) is bounded but not gradable: a
    // timeout fires before the run is live, so it surfaces as a pre-launch
    // failure. The guard's teardown still runs for a half-acquired substrate.
    let guard: RuntimeGuard = match bounded(deadline, provider.acquire(task)).await {
        Ok(Ok(guard)) => guard,
        Ok(Err(e)) => return pre_launch_failure("provisioning", e.to_string()),
        Err(Elapsed) => return pre_launch_failure("provisioning", timeout_detail()),
    };

    let connect_options = ConnectOptions {
        ready_timeout: guard
            .runtime
            .params
            .get("ready_timeout")
            .and_then(Value::as_f64)
            .map(Duration::from_secs_f64)
            .unwrap_or(ConnectOptions::default().ready_timeout),
        ..Default::default()
    };
    let client = match bounded(deadline, connect(&guard.runtime.url, connect_options)).await {
        Ok(Ok(client)) => client,
        Ok(Err(e)) => {
            guard.close().await;
            return pre_launch_failure("starting task", e.to_string());
        }
        Err(Elapsed) => {
            guard.close().await;
            return pre_launch_failure("starting task", timeout_detail());
        }
    };

    let mut run = Run::new(client, &task.id, task.args.clone());
    run.runtime_url = Some(guard.runtime.url.clone());
    match bounded(deadline, run.start()).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            run.close_client().await;
            guard.close().await;
            return pre_launch_failure("starting task", e.to_string());
        }
        Err(Elapsed) => {
            run.close_client().await;
            guard.close().await;
            return pre_launch_failure("starting task", timeout_detail());
        }
    }

    // The agent loop. An agent error marks the run and still grades
    // best-effort below; a deadline breach records the truncation and falls
    // through to the normal grade path — the partial trajectory is worth
    // grading. (grade() itself blocks on an unbounded read, like Python.)
    let mut agent_error: Option<String> = None;
    match bounded(deadline, agent.run(&mut run)).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            run.trace.status = Some(TraceStatus::Error);
            agent_error = Some(e.to_string());
        }
        Err(Elapsed) => {
            tracing::warn!("rollout agent loop {}; grading partial", timeout_detail());
            run.trace
                .extra
                .insert("stop_reason".to_string(), json!("timeout"));
            run.record(Step::system_error(format!(
                "agent loop {}",
                timeout_detail()
            )));
        }
    }

    if let Some(error) = agent_error {
        tracing::warn!("rollout failed mid-run (agent loop): {error}");
        let _ = run.finish(true).await;
        run.record(Step::system_error(format!("[agent loop] {error}")));
    } else if let Err(e) = run.finish(false).await {
        tracing::warn!("rollout failed mid-run (grading): {e}");
        run.trace.status = Some(TraceStatus::Error);
        run.record(Step::system_error(format!("[grading] {e}")));
    }

    run.close_client().await;
    guard.close().await;
    run
}

fn pre_launch_failure(phase: &str, detail: String) -> Run {
    tracing::warn!("rollout failed before launch ({phase}): {detail}");
    Run::failed(format!("[{phase}] {detail}"))
}

struct Elapsed;

async fn bounded<T>(
    deadline: Option<Instant>,
    fut: impl std::future::Future<Output = T>,
) -> Result<T, Elapsed> {
    match deadline {
        None => Ok(fut.await),
        Some(deadline) => tokio::time::timeout_at(deadline, fut)
            .await
            .map_err(|_| Elapsed),
    }
}
