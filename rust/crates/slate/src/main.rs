//! slate — a thread-weaving coding agent TUI on the HUD Rust SDK.
//!
//! One orchestrator model plans; persistent worker threads act on the task
//! workspace over `ssh/2`; episodes weave the two together. The session is
//! interactive (multi-turn, interruptible) and resumable. The workspace env is
//! served by the reference Python SDK (`hud.environment.server`), driven
//! entirely from Rust over the `hud/1.0` wire protocol.

mod agent;
mod gateway;
mod interrupt;
mod runner;
mod session;
mod ui;

use agent::{ReplyStop, SlateConfig, UiEvent};
use clap::Parser;
use gateway::{Gateway, DEFAULT_GATEWAY_URL};
use runner::{Launcher, Placement, SessionHandle};
use session::{SessionMeta, SessionStore};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;

#[derive(Parser)]
#[command(
    name = "slate",
    about = "Slate-style thread-weaving coding agent on the HUD Rust SDK"
)]
struct Args {
    /// The coding task; omit it to type one in the TUI.
    #[arg(long)]
    task: Option<String>,

    /// Working directory served as the task workspace.
    #[arg(long, default_value = ".")]
    work_dir: PathBuf,

    /// Orchestrator model - strategy.
    #[arg(long, default_value = "claude-opus-4-8")]
    orch_model: String,

    /// Worker thread model - tactics.
    #[arg(long, default_value = "claude-sonnet-4-6")]
    worker_model: String,

    /// Maximum orchestrator turns per message.
    #[arg(long, default_value_t = 40)]
    max_turns: u32,

    /// Path to a hud-python checkout (default: $HUD_PYTHON_DIR).
    #[arg(long)]
    hud_python: Option<PathBuf>,

    /// Serve the workspace from a container image instead of a local process.
    /// The image must serve a `slate-coding` env on port 8765 (experimental,
    /// unverified: no image is built here).
    #[arg(long)]
    docker: Option<String>,

    /// Resume a saved session by id (continues its conversation; reuses its
    /// work_dir and models).
    #[arg(long)]
    resume: Option<String>,

    /// List saved sessions and exit.
    #[arg(long)]
    list_sessions: bool,

    /// Don't persist this session under ~/.slate/sessions.
    #[arg(long)]
    no_save: bool,

    /// Run without the TUI. --task and each --message run as turns in one
    /// session, in order; the weave prints to stdout.
    #[arg(long)]
    headless: bool,

    /// A message for headless mode; repeatable to drive a multi-turn session.
    #[arg(long)]
    message: Vec<String>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Layered like the Python SDK's settings: process env wins, then ./.env,
    // then ~/.hud/.env.
    let _ = dotenvy::dotenv();
    if let Some(home) = std::env::var_os("HOME") {
        let _ = dotenvy::from_path(PathBuf::from(home).join(".hud/.env"));
    }
    let args = Args::parse();

    if args.list_sessions {
        return list_sessions();
    }

    let api_key = std::env::var("HUD_API_KEY").map_err(|_| {
        "HUD_API_KEY is required (https://hud.ai/project/api-keys); export it or put it in ~/.hud/.env"
    })?;
    let gateway_url =
        std::env::var("HUD_GATEWAY_URL").unwrap_or_else(|_| DEFAULT_GATEWAY_URL.to_string());
    let gateway = Gateway::new(&gateway_url, &api_key);

    // Resume loads prior conversation, models, and work_dir; a fresh run reads
    // them from flags.
    let resumed = match &args.resume {
        Some(id) => Some(SessionStore::load_meta(id).map_err(|e| format!("resume {id:?}: {e}"))?),
        None => None,
    };
    let (orch_model, worker_model, work_dir) = match &resumed {
        Some(meta) => (
            meta.orch_model.clone(),
            meta.worker_model.clone(),
            PathBuf::from(&meta.work_dir),
        ),
        None => (
            args.orch_model.clone(),
            args.worker_model.clone(),
            std::fs::canonicalize(&args.work_dir)
                .map_err(|e| format!("--work-dir {}: {e}", args.work_dir.display()))?,
        ),
    };

    let placement = match &args.docker {
        Some(image) => Placement::Docker {
            image: image.clone(),
        },
        None => Placement::Local {
            hud_python: runner::resolve_hud_python(args.hud_python.clone())?,
            work_dir: work_dir.clone(),
        },
    };

    let config = SlateConfig {
        orch_model: orch_model.clone(),
        worker_model: worker_model.clone(),
        max_turns: args.max_turns,
        ..Default::default()
    };

    // Session persistence + resume seed.
    let (store, seed_messages, replay) = match (&resumed, args.no_save) {
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
                created: hud_types::now_iso(),
                work_dir: work_dir.to_string_lossy().into_owned(),
                orch_model: orch_model.clone(),
                worker_model: worker_model.clone(),
                task: args.task.clone().unwrap_or_default(),
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
        gateway,
        placement,
        seed_messages,
        store,
    };
    let models = format!("orch {orch_model} | workers {worker_model}");

    if args.headless {
        let resuming = !launcher.seed_messages.is_empty();
        return headless(&launcher, args.task, args.message, resuming).await;
    }

    let mut terminal = ratatui::init();
    let result = ui::run(
        &mut terminal,
        work_dir.display().to_string(),
        models,
        args.task,
        replay,
        |task| launcher.start(task),
    )
    .await;
    ratatui::restore();
    if let Some(id) = session_id {
        eprintln!("session saved: slate --resume {id}");
    }
    result.map_err(Into::into)
}

fn list_sessions() -> Result<(), Box<dyn std::error::Error>> {
    let sessions = SessionStore::list();
    if sessions.is_empty() {
        println!("no saved sessions (they appear here after a run)");
        return Ok(());
    }
    println!("{:<14} {:<26} task", "id", "created");
    for meta in sessions {
        let task: String = meta
            .task
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(50)
            .collect();
        println!("{:<14} {:<26} {}", meta.id, meta.created, task);
    }
    Ok(())
}

/// The no-TUI path: drive one session with a fixed list of messages, printing
/// the weave. Exercises the interactive loop, filetracking, and persistence
/// without a TTY.
async fn headless(
    launcher: &Launcher,
    task: Option<String>,
    messages: Vec<String>,
    resuming: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut queue: Vec<String> = task.into_iter().chain(messages).collect();
    if queue.is_empty() {
        return Err("--headless requires --task or at least one --message".into());
    }

    println!("Working directory: {}", launcher_work_dir(launcher));
    println!(
        "Orchestrator: {} | Workers: {} (HUD gateway)",
        launcher.config.orch_model, launcher.config.worker_model
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
                    UiEvent::Bash { thread, command, exit_status } =>
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
                let outcome = settled.map_err(|_| "session task dropped")?;
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
    match &launcher.placement {
        Placement::Local { work_dir, .. } => work_dir.display().to_string(),
        Placement::Docker { image } => format!("(docker: {image})"),
    }
}
