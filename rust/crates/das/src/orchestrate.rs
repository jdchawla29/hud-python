//! Interactive thread-weaving orchestration for DAS.
//!
use crate::agent::{DasConfig, ReplyStop, UiEvent};
use crate::gateway::{Gateway, DEFAULT_GATEWAY_URL};
use crate::harness::{WorkerHarnessKind, WorkerHarnessOptions};
use crate::model::{Project, Workspace};
use crate::orchestrator::{
    ClaudeOrchestrator, GatewayOrchestrator, Orchestrator, OrchestratorKind, DEFAULT_GATEWAY_MODEL,
};
use crate::runner::{Launcher, Placement, SessionHandle};
use crate::session::{SessionMeta, SessionStore};
use crate::{harness, ui};
use anyhow::{bail, Context, Result};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;

pub struct OpenOptions {
    pub project: Project,
    pub workspace: Workspace,
    pub task: Option<String>,
    pub orch_harness: OrchestratorKind,
    pub orch_model: Option<String>,
    pub worker_harness: WorkerHarnessKind,
    pub worker_model: Option<String>,
    pub codex_socket: Option<PathBuf>,
    pub max_turns: u32,
    pub hud_python: Option<PathBuf>,
    pub resume: Option<String>,
    pub no_save: bool,
    pub headless: bool,
    pub messages: Vec<String>,
}

pub async fn open(options: OpenOptions) -> Result<()> {
    let resumed = match &options.resume {
        Some(id) => Some(SessionStore::load_meta(id).with_context(|| format!("resume {id:?}"))?),
        None => None,
    };
    if let Some(meta) = &resumed {
        if meta.project_id != options.project.id || meta.workspace_id != options.workspace.id {
            bail!(
                "session {:?} does not belong to {}/{}",
                meta.id,
                options.project.name,
                options.workspace.name
            );
        }
    }
    let (orch_harness_kind, orch_model, worker_harness_kind, worker_model, codex_socket) =
        match &resumed {
            Some(meta) => (
                meta.orch_harness,
                meta.orch_model.clone(),
                meta.worker_harness,
                meta.worker_model.clone(),
                meta.codex_socket.clone(),
            ),
            None => (
                options.orch_harness,
                options.orch_model.clone(),
                options.worker_harness,
                options.worker_model.clone(),
                options.codex_socket.clone(),
            ),
        };
    let orch_model = match (orch_harness_kind, orch_model) {
        (OrchestratorKind::Gateway, None) => Some(DEFAULT_GATEWAY_MODEL.to_string()),
        (_, model) => model,
    };
    let work_dir = std::fs::canonicalize(&options.workspace.path).with_context(|| {
        format!(
            "failed to resolve workspace {}",
            options.workspace.path.display()
        )
    })?;

    if worker_harness_kind == WorkerHarnessKind::Claude && codex_socket.is_some() {
        bail!("--codex-socket only applies to --worker-harness codex");
    }

    let placement = Placement {
        hud_python: crate::runner::resolve_hud_python(options.hud_python.clone())
            .map_err(anyhow::Error::msg)?,
        work_dir: work_dir.clone(),
    };

    let config = DasConfig {
        max_turns: options.max_turns,
        ..Default::default()
    };
    let orchestrator: Arc<dyn Orchestrator> = match orch_harness_kind {
        OrchestratorKind::Gateway => {
            let api_key = std::env::var("HUD_API_KEY")
                .context("HUD_API_KEY is required for --orch-harness gateway")?;
            let gateway_url = std::env::var("HUD_GATEWAY_URL")
                .unwrap_or_else(|_| DEFAULT_GATEWAY_URL.to_string());
            Arc::new(GatewayOrchestrator::new(
                Gateway::new(&gateway_url, &api_key),
                orch_model
                    .clone()
                    .expect("gateway model default was applied"),
            ))
        }
        OrchestratorKind::Claude => Arc::new(ClaudeOrchestrator::new(
            work_dir.clone(),
            orch_model.clone(),
        )),
    };
    let worker_harness = harness::build(WorkerHarnessOptions {
        kind: worker_harness_kind,
        workspace: work_dir.clone(),
        model: worker_model.clone(),
        codex_socket: codex_socket.clone(),
    });

    // Session persistence + resume seed.
    let (store, seed_messages, replay) = match (&resumed, options.no_save) {
        (Some(meta), _) => {
            let store = SessionStore::open(&meta.id)?;
            let seed = store.load_messages();
            let replay = store.load_events();
            (Some(Arc::new(store)), seed, replay)
        }
        (None, true) => (None, Vec::new(), Vec::new()),
        (None, false) => {
            let meta = SessionMeta {
                id: SessionStore::new_id(),
                project_id: options.project.id,
                workspace_id: options.workspace.id,
                created: hud_types::now_iso(),
                work_dir: work_dir.to_string_lossy().into_owned(),
                orch_harness: orch_harness_kind,
                orch_model: orch_model.clone(),
                worker_harness: worker_harness_kind,
                worker_model: worker_model.clone(),
                codex_socket,
                task: options.task.clone().unwrap_or_default(),
            };
            (
                Some(Arc::new(SessionStore::create(&meta)?)),
                Vec::new(),
                Vec::new(),
            )
        }
    };
    let session_id = store.as_ref().map(|s| s.id().to_string());

    let launcher = Launcher {
        config,
        orchestrator,
        worker_harness,
        placement,
        seed_messages,
        store,
    };
    let orch_model_label = orch_model.as_deref().unwrap_or("configured default");
    let worker_model_label = worker_model.as_deref().unwrap_or("configured default");
    let models = format!(
        "orch {orch_harness_kind}/{orch_model_label} | workers {worker_harness_kind}/{worker_model_label}"
    );

    if options.headless {
        let resuming = !launcher.seed_messages.is_empty();
        return headless(&launcher, options.task, options.messages, resuming).await;
    }

    let mut terminal = ratatui::init();
    let result = ui::run(
        &mut terminal,
        work_dir.display().to_string(),
        models,
        options.task,
        replay,
        |task| launcher.start(task),
    )
    .await;
    ratatui::restore();
    if let Some(id) = session_id {
        eprintln!(
            "session saved: das open {} {} --resume {id}",
            options.project.name, options.workspace.name
        );
    }
    result.map_err(Into::into)
}

/// The no-TUI path: drive one session with a fixed list of messages, printing
/// the weave. Exercises the interactive loop, filetracking, and persistence
/// without a TTY.
async fn headless(
    launcher: &Launcher,
    task: Option<String>,
    messages: Vec<String>,
    resuming: bool,
) -> Result<()> {
    let mut queue: Vec<String> = task.into_iter().chain(messages).collect();
    if queue.is_empty() {
        bail!("--headless requires --task or at least one --message");
    }

    println!("Working directory: {}", launcher_work_dir(launcher));
    println!(
        "Orchestrator: {}/{} | Workers: {}/{}",
        launcher.orchestrator.kind(),
        launcher
            .orchestrator
            .model()
            .unwrap_or("configured default"),
        launcher.worker_harness.kind(),
        launcher
            .worker_harness
            .model()
            .unwrap_or("configured default")
    );
    println!("{}", "=".repeat(60));

    // A fresh session's first message is the task prompt; a resumed session
    // ignores the prompt and takes every message over the inbox.
    let first = if resuming {
        String::new()
    } else {
        queue.remove(0)
    };
    let SessionHandle {
        mut events,
        input,
        interrupter,
        mut outcome,
    } = launcher.start(first);
    // Kept alive for the session (dropping it would close the interrupt
    // channel); headless never trips it.
    let _interrupter = interrupter;
    // Held while messages remain; set to None to end the session.
    let mut input: Option<UnboundedSender<String>> = Some(input);
    let mut remaining = queue.into_iter();
    // On resume nothing has been sent yet — prime the first inbox message.
    if resuming {
        if let (Some(tx), Some(next)) = (input.as_ref(), remaining.next()) {
            let _ = tx.send(next);
        }
    }

    loop {
        tokio::select! {
            event = events.recv() => {
                let Some(event) = event else { break };
                match event {
                    UiEvent::Status(_) | UiEvent::Tokens { .. } | UiEvent::WorkerText { .. } => {}
                    UiEvent::UserMessage(text) => println!("\nyou> {text}"),
                    UiEvent::OrchTurn { turn, text } => {
                        if text.is_empty() { println!("-- turn {turn} --"); }
                        else { println!("-- turn {turn} --\n{text}"); }
                    }
                    UiEvent::Dispatch { thread, action, seeded } => {
                        let seeded = if seeded { " (seeded)" } else { "" };
                        println!("  dispatch [{thread}]{seeded}: {action}");
                    }
                    UiEvent::Bash { thread, command, exit_status, .. } =>
                        println!("    [{thread}] $ {command} (exit {exit_status})"),
                    UiEvent::Episode { thread, text } => {
                        let head: String = text.lines().next().unwrap_or("").chars().take(110).collect();
                        println!("  episode [{thread}]: {head}");
                    }
                    UiEvent::FileChanged { path, status, added, removed } =>
                        println!("  ~ {status} {path} (+{added} -{removed})"),
                    UiEvent::Reply { text, stop } => {
                        let tag = match stop {
                            ReplyStop::Finished => "done",
                            ReplyStop::Stopped => "reply",
                            ReplyStop::MaxTurns => "reply (max turns)",
                        };
                        println!("[{tag}] {text}");
                        // Feed the next message, or end the session by dropping
                        // the last input sender.
                        match remaining.next() {
                            Some(next) => { if let Some(tx) = &input { let _ = tx.send(next); } }
                            None => { input = None; }
                        }
                    }
                    UiEvent::Interrupted => println!("[interrupted]"),
                    UiEvent::Notice(text) => println!("· {text}"),
                }
            }
            settled = &mut outcome => {
                let outcome = settled.context("session task dropped")?;
                println!("{}", "=".repeat(60));
                if let Some(error) = &outcome.error {
                    println!("Session failed: {error}");
                    std::process::exit(1);
                }
                println!("Session ended. Reward: {} | steps: {}", outcome.reward, outcome.steps);
                if let Some(answer) = &outcome.answer {
                    println!("Final answer:\n{answer}");
                }
                return Ok(());
            }
        }
    }

    drop(input);
    let _ = tokio::time::timeout(Duration::from_secs(5), outcome).await;
    Ok(())
}

fn launcher_work_dir(launcher: &Launcher) -> String {
    launcher.placement.work_dir.display().to_string()
}
