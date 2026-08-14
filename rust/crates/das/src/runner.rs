//! Wires the DAS agent into the HUD rollout engine and exposes an
//! interactive session handle (events + input + interrupt + outcome).

use crate::agent::{DasAgent, DasConfig, UiEvent};
use crate::harness::WorkerHarness;
use crate::interrupt::{Interrupt, Interrupter};
use crate::orchestrator::Orchestrator;
use crate::session::SessionStore;
use hud_eval::{rollout, LocalRuntime, Provider, RolloutOptions, TaskRow};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot;

const ENV_SOURCE: &str = include_str!("../das_env.py");

/// Final result of one DAS session.
#[derive(Debug, Clone)]
pub struct RunOutcome {
    pub reward: f64,
    pub answer: Option<String>,
    pub error: Option<String>,
    pub steps: usize,
}

impl RunOutcome {
    pub fn internal_error(message: &str) -> RunOutcome {
        RunOutcome {
            reward: 0.0,
            answer: None,
            error: Some(message.to_string()),
            steps: 0,
        }
    }
}

/// A running DAS session: live events, the input channel for follow-up
/// messages, the interrupt trigger, and the eventual outcome.
pub struct SessionHandle {
    pub events: UnboundedReceiver<UiEvent>,
    pub input: UnboundedSender<String>,
    pub interrupter: Interrupter,
    pub outcome: oneshot::Receiver<RunOutcome>,
}

#[derive(Clone)]
pub struct Placement {
    pub hud_python: PathBuf,
    pub work_dir: PathBuf,
}

#[derive(Clone)]
pub struct Launcher {
    pub config: DasConfig,
    pub orchestrator: Arc<dyn Orchestrator>,
    pub worker_harness: Arc<dyn WorkerHarness>,
    pub placement: Placement,
    /// Prior conversation to continue (resume); empty for a fresh session.
    pub seed_messages: Vec<Value>,
    pub store: Option<Arc<SessionStore>>,
}

impl Launcher {
    /// Start an interactive session in the background. `first_task` is the
    /// initial task prompt (empty on a resumed session, which waits for the
    /// user's next message).
    pub fn start(&self, first_task: String) -> SessionHandle {
        let (raw_events_tx, raw_events_rx) = tokio::sync::mpsc::unbounded_channel();
        let (events_tx, events_rx) = tokio::sync::mpsc::unbounded_channel();
        let (input_tx, input_rx) = tokio::sync::mpsc::unbounded_channel();
        let (outcome_tx, outcome_rx) = oneshot::channel();
        let (interrupter, interrupt) = Interrupt::channel();

        // Tee raw agent events: persist to the transcript, then forward to the UI.
        let store = self.store.clone();
        tokio::spawn(tee_events(raw_events_rx, events_tx, store));

        let agent = DasAgent {
            config: self.config.clone(),
            orchestrator: Arc::clone(&self.orchestrator),
            worker_harness: Arc::clone(&self.worker_harness),
            events: raw_events_tx,
            inbox: std::sync::Mutex::new(Some(input_rx)),
            interrupt,
            seed_messages: self.seed_messages.clone(),
            session: self.store.clone(),
        };
        let provider = self.placement.provider();
        let task = task_row(&first_task);

        tokio::spawn(async move {
            let run = rollout(&task, &agent, provider.as_ref(), RolloutOptions::default()).await;
            let outcome = RunOutcome {
                reward: run.reward(),
                answer: run.trace.content.clone(),
                error: run
                    .trace
                    .is_error()
                    .then(|| run.trace.error().unwrap_or("session failed").to_string()),
                steps: run.trace.len(),
            };
            let _ = outcome_tx.send(outcome);
        });

        SessionHandle {
            events: events_rx,
            input: input_tx,
            interrupter,
            outcome: outcome_rx,
        }
    }
}

async fn tee_events(
    mut raw: UnboundedReceiver<UiEvent>,
    ui: UnboundedSender<UiEvent>,
    store: Option<Arc<SessionStore>>,
) {
    while let Some(event) = raw.recv().await {
        if let Some(store) = &store {
            store.append_event(&event);
        }
        if ui.send(event).is_err() {
            return;
        }
    }
}

impl Placement {
    fn provider(&self) -> Box<dyn Provider> {
        let env_file = write_env_source();
        Box::new(
            LocalRuntime::command([
                "uv".to_string(),
                "run".to_string(),
                "--project".to_string(),
                self.hud_python.to_string_lossy().into_owned(),
                "python".to_string(),
                "-m".to_string(),
                "hud.environment.server".to_string(),
                env_file.to_string_lossy().into_owned(),
            ])
            .env_var("DAS_WORK_DIR", self.work_dir.to_string_lossy()),
        )
    }
}

fn task_row(task_description: &str) -> TaskRow {
    let mut args = Map::new();
    args.insert("task_description".to_string(), json!(task_description));
    TaskRow::new("das-coding", "coding_task").with_args(args)
}

/// Materialize the embedded env source to a stable per-process temp path.
fn write_env_source() -> PathBuf {
    let path = std::env::temp_dir().join(format!("das-env-{}.py", std::process::id()));
    std::fs::write(&path, ENV_SOURCE).expect("write env source to temp dir");
    path
}

/// Locate the hud-python checkout: `--hud-python` flag, `$HUD_PYTHON_DIR`,
/// else this repo's root (the Rust workspace is embedded in the Python SDK).
pub fn resolve_hud_python(flag: Option<PathBuf>) -> Result<PathBuf, String> {
    let path = flag
        .or_else(|| std::env::var_os("HUD_PYTHON_DIR").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.."));
    if !Path::new(&path).join("pyproject.toml").exists() {
        return Err(format!(
            "{} does not look like a hud-python checkout (no pyproject.toml)",
            path.display()
        ));
    }
    Ok(path)
}
