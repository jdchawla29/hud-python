//! Interactive thread-weaving orchestration on the HUD Rust SDK.
//!
//! One orchestrator model plans and never touches the environment; it
//! dispatches single bounded actions to persistent Codex or Claude CLI worker
//! sessions, each acting on the same task workspace and returning an *episode*.
//! Unlike the one-shot cookbook, the HUD workspace and orchestrator conversation
//! stay live for follow-up messages, and in-flight workers can be interrupted.

use crate::harness::{WorkerError, WorkerHarness, WorkerReporter, WorkerSession};
use crate::interrupt::{Interrupt, InterruptRx};
use crate::orchestrator::{ChatResponse, Orchestrator, ToolUse};
use crate::session::SessionStore;
use async_trait::async_trait;
use hud_eval::{Agent, AgentError, Run};
use hud_types::{Step, StepSource};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

pub const ORCH_SYSTEM: &str = "You are the orchestrator of a thread-based coding agent.

You never act on the environment yourself. You dispatch single, bounded actions
to worker threads and receive back episodes: compressed records of what each
thread did and learned. One action is one tactic - inspect one area of the
code, run one command sequence, apply one edit, run one test suite. Keep
actions small enough that a thread finishes them in a handful of steps.

Threads are persistent: dispatching to an existing thread_id continues that
thread with everything it has already seen, so route follow-up work on the same
files or topic to the same thread. Start a new thread_id for independent work,
and pass seed_episodes to give a new thread the conclusions of earlier threads
without their full history. Several dispatch calls in one turn run in parallel;
use that for independent actions only.

Re-plan after every batch of episodes: integrate what was learned and adapt.
Verify work with follow-up actions instead of trusting a thread's claim of
success. This is an interactive session: when the current request is complete,
call finish(answer) with a short report; the user may then send a follow-up.";

fn dispatch_tool() -> Value {
    json!({
        "name": "dispatch",
        "description": "Execute one bounded action on a worker thread and get back its \
            episode. Reusing a thread_id continues that thread with its full \
            accumulated context; a new thread_id starts a fresh thread, \
            optionally seeded with prior episodes.",
        "input_schema": {
            "type": "object",
            "properties": {
                "thread_id": {"type": "string", "description": "Thread to run on. Existing id continues it; new id creates it."},
                "action": {"type": "string", "description": "One concrete, bounded action for the thread to execute."},
                "seed_episodes": {"type": "array", "items": {"type": "string"}, "description": "Episodes from prior threads to seed a NEW thread with."},
            },
            "required": ["thread_id", "action"],
        },
    })
}

fn finish_tool() -> Value {
    json!({
        "name": "finish",
        "description": "Report the current request complete and hand control back to the user.",
        "input_schema": {
            "type": "object",
            "properties": {"answer": {"type": "string"}},
            "required": ["answer"],
        },
    })
}

/// Why a turn's reply came back to the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReplyStop {
    /// The orchestrator called `finish`.
    Finished,
    /// The orchestrator stopped dispatching without calling `finish`.
    Stopped,
    /// The per-message turn budget was exhausted.
    MaxTurns,
}

/// Live progress events mirrored to the UI (and persisted for resume).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum UiEvent {
    /// A transient status line (not part of the durable transcript).
    Status(String),
    /// A user message opened a new turn.
    UserMessage(String),
    OrchTurn {
        turn: u32,
        text: String,
    },
    Dispatch {
        thread: String,
        action: String,
        seeded: bool,
    },
    WorkerText {
        thread: String,
        text: String,
    },
    Bash {
        thread: String,
        command: String,
        #[serde(default)]
        output: String,
        exit_status: u32,
    },
    Episode {
        thread: String,
        text: String,
    },
    /// A live workspace diff for one file.
    FileChanged {
        path: String,
        status: String,
        added: u32,
        removed: u32,
    },
    /// Cumulative-delta token usage from one model call.
    Tokens {
        input: u64,
        output: u64,
    },
    /// The orchestrator's answer for a turn.
    Reply {
        text: String,
        stop: ReplyStop,
    },
    /// The current turn was interrupted by the user.
    Interrupted,
    /// A harness notice (errors, hints).
    Notice(String),
}

#[derive(Debug, Clone)]
pub struct DasConfig {
    pub max_turns: u32,
    pub thread_timeout: Duration,
    pub file_tracking_interval: Duration,
}

impl Default for DasConfig {
    fn default() -> Self {
        DasConfig {
            max_turns: 40,
            thread_timeout: Duration::from_secs(600),
            file_tracking_interval: Duration::from_secs(2),
        }
    }
}

/// Interactive orchestrator. Holds configuration and the per-
/// session channels; the conversation and worker threads live inside `run`.
pub struct DasAgent {
    pub config: DasConfig,
    pub orchestrator: Arc<dyn Orchestrator>,
    pub worker_harness: Arc<dyn WorkerHarness>,
    pub events: UnboundedSender<UiEvent>,
    /// User messages after the first (which arrives as the task prompt).
    /// Closing the channel ends the session.
    pub inbox: Mutex<Option<UnboundedReceiver<String>>>,
    pub interrupt: Interrupt,
    /// Prior conversation to continue (resume); empty for a fresh session.
    pub seed_messages: Vec<Value>,
    pub session: Option<Arc<SessionStore>>,
}

impl DasAgent {
    fn emit(&self, event: UiEvent) {
        let _ = self.events.send(event);
    }
}

#[async_trait]
impl Agent for DasAgent {
    async fn run(&self, run: &mut Run) -> Result<(), AgentError> {
        // Live workspace diffs (best-effort): if the env published a
        // filetracking binding, poll it in the background and emit FileChanged.
        let file_tracking = self.start_file_tracking(run).await;

        let (steps_tx, steps_rx) = tokio::sync::mpsc::unbounded_channel::<Step>();
        let ctx = Arc::new(WorkerCtx {
            config: self.config.clone(),
            harness: Arc::clone(&self.worker_harness),
            events: self.events.clone(),
            steps: steps_tx,
        });

        let mut interrupt = self
            .interrupt
            .take_receiver()
            .expect("interrupt receiver taken once per session");
        interrupt.baseline();

        let mut session = SessionRunner {
            agent: self,
            run,
            ctx,
            threads: HashMap::new(),
            messages: self.seed_messages.clone(),
            interrupt,
            steps_rx,
            last_reply: String::new(),
        };

        // The first user message arrives as the task prompt (skipped on a
        // resumed session, which continues the loaded conversation instead).
        let first = session.run.prompt_text();
        if self.seed_messages.is_empty() && !first.trim().is_empty() {
            session.handle_message(first).await;
        }

        let mut inbox = self
            .inbox
            .lock()
            .expect("inbox lock")
            .take()
            .expect("inbox taken once per session");
        loop {
            self.emit(UiEvent::Status("ready".into()));
            let Some(message) = inbox.recv().await else {
                break; // channel closed: end session
            };
            session.interrupt.baseline();
            session.handle_message(message).await;
        }

        run.trace.content = Some(session.last_reply.clone());
        if let Some(ft) = file_tracking {
            ft.finish(&self.events).await;
        }
        Ok(())
    }

    fn model(&self) -> Option<String> {
        self.orchestrator.model().map(str::to_string)
    }
}

impl DasAgent {
    async fn start_file_tracking(&self, run: &mut Run) -> Option<FileTracking> {
        let url = run.client().binding("filetracking").ok()?.url.clone();
        let mut ft = match hud_client::filetracking::FileTrackingClient::connect(&url).await {
            Ok(ft) => ft,
            Err(e) => {
                tracing::debug!("file tracking unavailable: {e}");
                return None;
            }
        };
        // Re-baseline past scenario setup so the first diff is the agent's.
        if ft.advance().await.is_err() {
            return None;
        }
        let events = self.events.clone();
        let interval = self.config.file_tracking_interval;
        let stop = Arc::new(tokio::sync::Notify::new());
        let stop_poll = Arc::clone(&stop);
        let handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(interval) => {}
                    _ = stop_poll.notified() => return ft,
                }
                if let Ok(diff) = ft.diff().await {
                    emit_diff(&events, &diff);
                }
            }
        });
        Some(FileTracking { handle, stop })
    }
}

struct FileTracking {
    handle: tokio::task::JoinHandle<hud_client::filetracking::FileTrackingClient>,
    stop: Arc<tokio::sync::Notify>,
}

impl FileTracking {
    async fn finish(self, events: &UnboundedSender<UiEvent>) {
        self.stop.notify_waiters();
        if let Ok(mut ft) = self.handle.await {
            if let Ok(diff) = ft.diff().await {
                emit_diff(events, &diff);
            }
        }
    }
}

fn emit_diff(events: &UnboundedSender<UiEvent>, diff: &hud_client::filetracking::Diff) {
    for patch in &diff.patches {
        let (added, removed) = patch.line_delta();
        let _ = events.send(UiEvent::FileChanged {
            path: patch.path.clone(),
            status: patch.status.clone(),
            added,
            removed,
        });
    }
}

/// Per-session mutable state: the orchestrator conversation, live worker
/// threads, and the trace being filled.
struct SessionRunner<'a> {
    agent: &'a DasAgent,
    run: &'a mut Run,
    ctx: Arc<WorkerCtx>,
    threads: HashMap<String, WorkerThread>,
    messages: Vec<Value>,
    interrupt: InterruptRx,
    steps_rx: UnboundedReceiver<Step>,
    last_reply: String,
}

impl SessionRunner<'_> {
    fn emit(&self, event: UiEvent) {
        self.agent.emit(event);
    }

    fn persist(&self) {
        if let Some(session) = &self.agent.session {
            session.save_messages(&self.messages);
        }
    }

    /// Run the orchestrator to a reply for one user message, or until the user
    /// interrupts. Keeps the conversation valid on interrupt so the next
    /// message can continue.
    async fn handle_message(&mut self, message: String) {
        self.emit(UiEvent::UserMessage(message.clone()));
        self.messages
            .push(json!({"role": "user", "content": message}));

        for turn in 1..=self.agent.config.max_turns {
            if self.interrupt.tripped() {
                self.emit(UiEvent::Interrupted);
                self.persist();
                return;
            }

            let tools = [dispatch_tool(), finish_tool()];
            let resp = tokio::select! {
                biased;
                _ = self.interrupt.wait() => {
                    self.emit(UiEvent::Interrupted);
                    self.persist();
                    return;
                }
                resp = self.agent.orchestrator.messages(
                    ORCH_SYSTEM, &self.messages, &tools, 4096,
                ) => resp,
            };
            let resp = match resp {
                Ok(resp) => resp,
                Err(e) => {
                    self.emit(UiEvent::Notice(format!(
                        "orchestrator inference failed: {e}"
                    )));
                    return;
                }
            };
            self.emit_tokens(&resp);

            let uses = resp.tool_uses();
            let dispatches: Vec<ToolUse> = uses
                .iter()
                .filter(|u| u.name == "dispatch")
                .cloned()
                .collect();
            let finish = uses.iter().find(|u| u.name == "finish");
            let text = resp.text();
            if !text.is_empty() {
                self.last_reply = text.clone();
            }
            self.run.record(agent_step(
                self.agent
                    .orchestrator
                    .model()
                    .unwrap_or("configured default"),
                &resp,
                dispatches.is_empty(),
                [
                    ("role", json!("orchestrator")),
                    ("harness", json!(self.agent.orchestrator.kind())),
                ],
            ));
            if finish.is_none() && !dispatches.is_empty() {
                self.emit(UiEvent::OrchTurn {
                    turn,
                    text: text.clone(),
                });
            }
            self.messages
                .push(json!({"role": "assistant", "content": resp.content}));

            if let Some(finish) = finish {
                let answer = finish
                    .input
                    .get("answer")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                self.last_reply = answer.clone();
                self.emit(UiEvent::Reply {
                    text: answer,
                    stop: ReplyStop::Finished,
                });
                self.persist();
                return;
            }
            if dispatches.is_empty() {
                self.emit(UiEvent::Reply {
                    text,
                    stop: ReplyStop::Stopped,
                });
                self.persist();
                return;
            }

            let interrupted = self.run_dispatches(dispatches).await;
            self.drain_steps();
            self.persist();
            if interrupted {
                self.emit(UiEvent::Interrupted);
                return;
            }
        }

        self.emit(UiEvent::Reply {
            text: self.last_reply.clone(),
            stop: ReplyStop::MaxTurns,
        });
        self.persist();
    }

    /// Run a turn's dispatches, pushing matching `tool_result` blocks. On
    /// interrupt, fills placeholders so the conversation stays valid.
    /// Returns whether the user interrupted.
    async fn run_dispatches(&mut self, dispatches: Vec<ToolUse>) -> bool {
        // Group by thread: distinct threads run in parallel, several actions
        // for one thread run in order on it.
        let mut order: Vec<String> = Vec::new();
        let mut grouped: HashMap<String, Vec<Dispatch>> = HashMap::new();
        for use_ in &dispatches {
            let dispatch = Dispatch::from_tool_use(use_);
            if !grouped.contains_key(&dispatch.thread_id) {
                order.push(dispatch.thread_id.clone());
            }
            grouped
                .entry(dispatch.thread_id.clone())
                .or_default()
                .push(dispatch);
        }
        for dispatch in grouped.values().flatten() {
            self.emit(UiEvent::Dispatch {
                thread: dispatch.thread_id.clone(),
                action: dispatch.action.clone(),
                seeded: !dispatch.seed_episodes.is_empty(),
            });
        }

        let batch = futures::future::join_all(order.iter().map(|thread_id| {
            let dispatches = grouped.remove(thread_id).expect("grouped by order");
            let thread = self.threads.remove(thread_id);
            let ctx = Arc::clone(&self.ctx);
            let interrupt = self.interrupt.fork();
            run_thread_batch(thread, dispatches, ctx, interrupt)
        }));
        let batches = batch.await;
        let mut results: Vec<Value> = Vec::new();
        let mut interrupted = false;
        for batch in batches {
            interrupted |= batch.interrupted;
            if let Some(thread) = batch.thread {
                self.threads.insert(thread.id.clone(), thread);
            }
            for (call_id, episode) in batch.episodes {
                results.push(tool_result(&call_id, &episode));
            }
        }
        interrupted |= self.interrupt.tripped();
        if interrupted {
            for use_ in &dispatches {
                if !results
                    .iter()
                    .any(|result| result["tool_use_id"] == use_.id)
                {
                    results.push(tool_result(
                        &use_.id,
                        "[interrupted by user before completion]",
                    ));
                }
            }
        }
        self.messages
            .push(json!({"role": "user", "content": results}));
        interrupted
    }

    fn emit_tokens(&self, resp: &ChatResponse) {
        self.emit(UiEvent::Tokens {
            input: resp.usage.input_tokens,
            output: resp.usage.output_tokens,
        });
    }

    fn drain_steps(&mut self) {
        while let Ok(step) = self.steps_rx.try_recv() {
            self.run.record(step);
        }
    }
}

fn tool_result(call_id: &str, content: &str) -> Value {
    json!({"type": "tool_result", "tool_use_id": call_id, "content": content})
}

struct Dispatch {
    call_id: String,
    thread_id: String,
    action: String,
    seed_episodes: Vec<String>,
}

impl Dispatch {
    fn from_tool_use(use_: &ToolUse) -> Dispatch {
        Dispatch {
            call_id: use_.id.clone(),
            thread_id: use_
                .input
                .get("thread_id")
                .and_then(Value::as_str)
                .unwrap_or("main")
                .to_string(),
            action: use_
                .input
                .get("action")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            seed_episodes: use_
                .input
                .get("seed_episodes")
                .and_then(Value::as_array)
                .map(|seeds| {
                    seeds
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
        }
    }
}

struct WorkerCtx {
    config: DasConfig,
    harness: Arc<dyn WorkerHarness>,
    events: UnboundedSender<UiEvent>,
    steps: UnboundedSender<Step>,
}

struct ThreadBatch {
    thread: Option<WorkerThread>,
    episodes: Vec<(String, String)>,
    interrupted: bool,
}

async fn run_thread_batch(
    thread: Option<WorkerThread>,
    dispatches: Vec<Dispatch>,
    ctx: Arc<WorkerCtx>,
    mut interrupt: InterruptRx,
) -> ThreadBatch {
    let first = &dispatches[0];
    let mut thread = match thread {
        Some(thread) => thread,
        None => {
            let reporter = WorkerReporter::new(
                first.thread_id.clone(),
                ctx.harness.kind(),
                ctx.harness.model().map(str::to_string),
                ctx.events.clone(),
                ctx.steps.clone(),
            );
            let start = ctx
                .harness
                .start(&first.thread_id, &first.seed_episodes, reporter);
            tokio::pin!(start);
            let deadline = tokio::time::sleep(ctx.config.thread_timeout);
            tokio::pin!(deadline);
            match tokio::select! {
                biased;
                _ = interrupt.wait() => Err(WorkerError::Interrupted),
                _ = &mut deadline => Err(WorkerError::Timeout {
                    seconds: ctx.config.thread_timeout.as_secs(),
                }),
                result = &mut start => result,
            } {
                Ok(session) => WorkerThread {
                    id: first.thread_id.clone(),
                    session,
                },
                Err(error) => {
                    let interrupted = matches!(error, WorkerError::Interrupted);
                    return ThreadBatch {
                        thread: None,
                        episodes: dispatches
                            .iter()
                            .map(|dispatch| {
                                (
                                    dispatch.call_id.clone(),
                                    format!(
                                        "[thread {:?} failed to start: {error}]",
                                        dispatch.thread_id
                                    ),
                                )
                            })
                            .collect(),
                        interrupted,
                    };
                }
            }
        }
    };
    let mut episodes = Vec::new();
    let mut discarded = false;
    let mut interrupted = false;
    for dispatch in &dispatches {
        if discarded {
            episodes.push((
                dispatch.call_id.clone(),
                format!(
                    "[thread {:?} was discarded earlier this turn after a timeout; \
                     re-dispatch a narrower action.]",
                    dispatch.thread_id
                ),
            ));
            continue;
        }
        match thread
            .session
            .act(&dispatch.action, &mut interrupt, ctx.config.thread_timeout)
            .await
        {
            Ok(episode) => {
                let _ = ctx.events.send(UiEvent::Episode {
                    thread: thread.id.clone(),
                    text: episode.clone(),
                });
                episodes.push((dispatch.call_id.clone(), episode));
            }
            Err(error) => {
                discarded = true;
                interrupted = matches!(error, WorkerError::Interrupted);
                episodes.push((
                    dispatch.call_id.clone(),
                    format!(
                        "[thread {:?} failed while executing {:?}: {error}. The native session was \
                         discarded; re-dispatch with prior episodes if needed.]",
                        dispatch.thread_id,
                        dispatch.action
                    ),
                ));
            }
        }
    }
    ThreadBatch {
        thread: (!discarded).then_some(thread),
        episodes,
        interrupted,
    }
}

struct WorkerThread {
    id: String,
    session: Box<dyn WorkerSession>,
}

/// An `AgentStep`-shaped trace step (`hud.step.v1` payload fields).
fn agent_step<const N: usize>(
    model: &str,
    resp: &ChatResponse,
    done: bool,
    extra: [(&str, Value); N],
) -> Step {
    let mut step = Step::new(StepSource::Agent);
    let text = resp.text();
    let mut payload = Map::new();
    if !text.is_empty() {
        payload.insert("content".to_string(), json!(text));
    }
    payload.insert("model".to_string(), json!(model));
    let tool_calls: Vec<Value> = resp
        .tool_uses()
        .iter()
        .map(|u| json!({"id": u.id, "name": u.name, "arguments": u.input}))
        .collect();
    if !tool_calls.is_empty() {
        payload.insert("tool_calls".to_string(), json!(tool_calls));
    }
    payload.insert("done".to_string(), json!(done));
    payload.insert(
        "usage".to_string(),
        json!({"prompt_tokens": resp.usage.input_tokens, "completion_tokens": resp.usage.output_tokens}),
    );
    step.payload = payload;
    for (key, value) in extra {
        step.extra.insert(key.to_string(), value);
    }
    step
}
