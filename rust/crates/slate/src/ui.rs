//! The ratatui front-end: a Claude-Code-style interactive session — a live
//! weave, a threads pane, a live file-changes pane, a persistent input line,
//! slash commands, Esc-to-interrupt, and a token/cost readout.

use crate::agent::{ReplyStop, UiEvent};
use crate::runner::{RunOutcome, SessionHandle};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use indexmap::IndexMap;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};
use std::time::Duration;

const HELP: &[&str] = &[
    "/help              show this help",
    "/diff              list current workspace changes",
    "/threads           list worker threads",
    "/clear             clear the weave view (history is kept on disk)",
    "/quit              end the session",
    "",
    "Enter  send a message      Esc  interrupt the current turn",
    "up/down PgUp/PgDn scroll    End  resume auto-follow",
];

#[derive(Debug, Clone, Copy, PartialEq)]
enum Phase {
    /// Waiting for the user's first task (no session started yet).
    FirstInput,
    /// A turn is in flight.
    Working,
    /// Waiting for the next user message.
    Ready,
    /// The session ended.
    Ended,
}

#[derive(Default, Clone)]
struct ThreadInfo {
    actions: u32,
    commands: u32,
    episodes: u32,
    busy: bool,
    last_action: String,
}

#[derive(Clone)]
struct FileInfo {
    status: String,
    added: u32,
    removed: u32,
}

struct App {
    phase: Phase,
    input: String,
    status: String,
    weave: Vec<Line<'static>>,
    threads: IndexMap<String, ThreadInfo>,
    changes: IndexMap<String, FileInfo>,
    input_tokens: u64,
    output_tokens: u64,
    scroll: u16,
    auto_scroll: bool,
    spinner: usize,
    work_dir: String,
    models: String,
}

const SPINNER: [&str; 4] = ["|", "/", "-", "\\"];

/// Run the interactive TUI. `initial_task` starts a session immediately;
/// `replay` seeds the weave with a resumed transcript; `start` launches a
/// session and returns its handle.
pub async fn run(
    terminal: &mut DefaultTerminal,
    work_dir: String,
    models: String,
    initial_task: Option<String>,
    replay: Vec<UiEvent>,
    start: impl Fn(String) -> SessionHandle,
) -> std::io::Result<()> {
    let mut app = App {
        phase: Phase::FirstInput,
        input: String::new(),
        status: "type a task and press Enter  (/help for commands)".to_string(),
        weave: Vec::new(),
        threads: IndexMap::new(),
        changes: IndexMap::new(),
        input_tokens: 0,
        output_tokens: 0,
        scroll: 0,
        auto_scroll: true,
        spinner: 0,
        work_dir,
        models,
    };
    for event in replay {
        app.on_event(event);
    }

    let (key_tx, mut key_rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    std::thread::spawn(move || {
        while let Ok(event) = crossterm::event::read() {
            if key_tx.send(event).is_err() {
                return;
            }
        }
    });

    let mut handle: Option<SessionHandle> = None;
    // A resumed session waits for the user's first message; a fresh --task
    // starts immediately.
    if let Some(task) = initial_task {
        app.begin_session(&task);
        handle = Some(start(task));
    } else if !app.weave.is_empty() {
        // Resumed: start the session process now; it waits on input.
        app.phase = Phase::Ready;
        app.status = "resumed - type a follow-up".to_string();
        handle = Some(start(String::new()));
    }

    loop {
        terminal.draw(|frame| draw(frame, &mut app))?;

        tokio::select! {
            key = key_rx.recv() => {
                let Some(key) = key else { break };
                let Event::Key(key) = key else { continue };
                match app.on_key(key) {
                    KeyOutcome::Quit => break,
                    KeyOutcome::Submit(message) => {
                        app.submit();
                        match &handle {
                            Some(h) => { let _ = h.input.send(message); }
                            None => { app.begin_session(&message); handle = Some(start(message)); }
                        }
                    }
                    KeyOutcome::Interrupt => {
                        if let Some(h) = &handle {
                            h.interrupter.trip();
                            app.status = "interrupting…".to_string();
                        }
                    }
                    KeyOutcome::Handled => {}
                }
            }
            event = recv_event(&mut handle) => {
                match event {
                    Some(HandleEvent::Ui(event)) => app.on_event(event),
                    Some(HandleEvent::Done(outcome)) => app.end_session(outcome),
                    None => {}
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(120)) => {
                app.spinner = app.spinner.wrapping_add(1);
            }
        }
    }
    Ok(())
}

enum HandleEvent {
    Ui(UiEvent),
    Done(RunOutcome),
}

async fn recv_event(handle: &mut Option<SessionHandle>) -> Option<HandleEvent> {
    match handle {
        None => {
            std::future::pending::<()>().await;
            unreachable!()
        }
        Some(session) => tokio::select! {
            event = session.events.recv() => event.map(HandleEvent::Ui),
            outcome = &mut session.outcome => Some(HandleEvent::Done(
                outcome.unwrap_or_else(|_| RunOutcome::internal_error("session task dropped")),
            )),
        },
    }
}

enum KeyOutcome {
    Quit,
    Submit(String),
    Interrupt,
    Handled,
}

impl App {
    fn on_key(&mut self, key: KeyEvent) -> KeyOutcome {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return KeyOutcome::Quit;
        }
        match key.code {
            KeyCode::Esc => match self.phase {
                Phase::Working => KeyOutcome::Interrupt,
                _ => KeyOutcome::Quit,
            },
            KeyCode::Enter => {
                let text = self.input.trim().to_string();
                self.input.clear();
                if text.is_empty() {
                    KeyOutcome::Handled
                } else if let Some(command) = text.strip_prefix('/') {
                    self.slash(command)
                } else {
                    KeyOutcome::Submit(text)
                }
            }
            KeyCode::Char(c) => {
                self.input.push(c);
                KeyOutcome::Handled
            }
            KeyCode::Backspace => {
                self.input.pop();
                KeyOutcome::Handled
            }
            KeyCode::Up => {
                self.auto_scroll = false;
                self.scroll = self.scroll.saturating_sub(1);
                KeyOutcome::Handled
            }
            KeyCode::Down => {
                self.scroll = self.scroll.saturating_add(1);
                KeyOutcome::Handled
            }
            KeyCode::PageUp => {
                self.auto_scroll = false;
                self.scroll = self.scroll.saturating_sub(20);
                KeyOutcome::Handled
            }
            KeyCode::PageDown => {
                self.scroll = self.scroll.saturating_add(20);
                KeyOutcome::Handled
            }
            KeyCode::End => {
                self.auto_scroll = true;
                KeyOutcome::Handled
            }
            _ => KeyOutcome::Handled,
        }
    }

    fn slash(&mut self, command: &str) -> KeyOutcome {
        match command.split_whitespace().next().unwrap_or("") {
            "quit" | "exit" | "q" => KeyOutcome::Quit,
            "help" | "h" => {
                self.note("commands");
                for line in HELP {
                    self.push(Line::styled(format!("  {line}"), dim()));
                }
                KeyOutcome::Handled
            }
            "clear" => {
                self.weave.clear();
                KeyOutcome::Handled
            }
            "diff" => {
                if self.changes.is_empty() {
                    self.note("no workspace changes tracked yet");
                } else {
                    self.note("workspace changes");
                    let entries: Vec<(String, FileInfo)> = self
                        .changes
                        .iter()
                        .map(|(p, i)| (p.clone(), i.clone()))
                        .collect();
                    for (path, info) in entries {
                        self.push(change_line(&path, &info));
                    }
                }
                KeyOutcome::Handled
            }
            "threads" => {
                if self.threads.is_empty() {
                    self.note("no worker threads yet");
                } else {
                    self.note("worker threads");
                    let entries: Vec<(String, ThreadInfo)> = self
                        .threads
                        .iter()
                        .map(|(id, i)| (id.clone(), i.clone()))
                        .collect();
                    for (id, info) in entries {
                        self.push(Line::styled(
                            format!(
                                "  {id}  {}a/{}c/{}e  {}",
                                info.actions, info.commands, info.episodes, info.last_action
                            ),
                            dim(),
                        ));
                    }
                }
                KeyOutcome::Handled
            }
            other => {
                self.note(&format!("unknown command /{other} (try /help)"));
                KeyOutcome::Handled
            }
        }
    }

    fn begin_session(&mut self, task: &str) {
        self.phase = Phase::Working;
        self.status = "provisioning workspace env…".to_string();
        self.push_user(task);
    }

    fn submit(&mut self) {
        if self.phase == Phase::Ready {
            self.phase = Phase::Working;
        }
        self.status = "working…".to_string();
    }

    fn end_session(&mut self, outcome: RunOutcome) {
        self.phase = Phase::Ended;
        self.status = match &outcome.error {
            Some(error) => format!("session ended with error: {error}   [q quits]"),
            None => format!("session ended - reward {:.2}   [q quits]", outcome.reward),
        };
    }

    fn on_event(&mut self, event: UiEvent) {
        match event {
            UiEvent::Status(status) => self.status = status,
            UiEvent::UserMessage(text) => {
                // The first message is already shown by begin_session; echo
                // only later ones (avoid a duplicate line).
                if self.weave.last().map(user_echoed) != Some(true) {
                    self.push_user(&text);
                }
                self.phase = Phase::Working;
            }
            UiEvent::OrchTurn { turn, text } => {
                self.push(Line::default());
                self.push(Line::styled(
                    format!("orchestrator · turn {turn}"),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ));
                for line in text.lines().take(12) {
                    self.push(Line::raw(format!("  {line}")));
                }
            }
            UiEvent::Dispatch {
                thread,
                action,
                seeded,
            } => {
                let info = self.threads.entry(thread.clone()).or_default();
                info.actions += 1;
                info.busy = true;
                info.last_action = truncate(action.clone(), 40);
                let tag = if seeded { " (seeded)" } else { "" };
                self.push(Line::from(vec![
                    Span::styled(
                        format!("  → [{thread}]{tag} "),
                        Style::default().fg(Color::Yellow),
                    ),
                    Span::raw(action),
                ]));
            }
            UiEvent::WorkerText { thread, text } => {
                let head = text.lines().next().unwrap_or("").to_string();
                self.status = truncate(format!("[{thread}] {head}"), 120);
            }
            UiEvent::Bash {
                thread,
                command,
                exit_status,
            } => {
                if let Some(info) = self.threads.get_mut(&thread) {
                    info.commands += 1;
                }
                let style = if exit_status == 0 {
                    dim()
                } else {
                    Style::default().fg(Color::Red)
                };
                let mut line = format!("     [{thread}] $ {command}");
                if exit_status != 0 {
                    line.push_str(&format!("  (exit {exit_status})"));
                }
                self.push(Line::styled(truncate(line, 160), style));
            }
            UiEvent::Episode { thread, text } => {
                if let Some(info) = self.threads.get_mut(&thread) {
                    info.episodes += 1;
                    info.busy = false;
                }
                self.push(Line::styled(
                    format!("  ← episode [{thread}]"),
                    Style::default().fg(Color::Magenta),
                ));
                for line in text.lines().take(6) {
                    self.push(Line::styled(
                        format!("     {}", truncate(line.to_string(), 160)),
                        dim(),
                    ));
                }
            }
            UiEvent::FileChanged {
                path,
                status,
                added,
                removed,
            } => {
                self.changes.insert(
                    path,
                    FileInfo {
                        status,
                        added,
                        removed,
                    },
                );
            }
            UiEvent::Tokens { input, output } => {
                self.input_tokens += input;
                self.output_tokens += output;
            }
            UiEvent::Reply { text, stop } => {
                self.push(Line::default());
                let tag = match stop {
                    ReplyStop::Finished => "done",
                    ReplyStop::Stopped => "reply",
                    ReplyStop::MaxTurns => "reply (max turns)",
                };
                self.push(Line::styled(
                    tag,
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ));
                for line in text.lines() {
                    self.push(Line::raw(format!("  {line}")));
                }
                self.phase = Phase::Ready;
                self.status = "ready - type a follow-up  (Esc quits)".to_string();
                for info in self.threads.values_mut() {
                    info.busy = false;
                }
            }
            UiEvent::Interrupted => {
                self.push(Line::styled(
                    "  interrupted",
                    Style::default().fg(Color::Red),
                ));
                self.phase = Phase::Ready;
                self.status = "interrupted - type a message  (Esc quits)".to_string();
                for info in self.threads.values_mut() {
                    info.busy = false;
                }
            }
            UiEvent::Notice(text) => self.note(&text),
        }
    }

    fn push_user(&mut self, text: &str) {
        self.push(Line::default());
        self.push(Line::from(vec![
            Span::styled(
                "you  ",
                Style::default()
                    .fg(Color::Blue)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(text.to_string()),
        ]));
    }

    fn note(&mut self, text: &str) {
        self.push(Line::styled(format!("· {text}"), dim()));
    }

    fn push(&mut self, line: Line<'static>) {
        self.weave.push(line);
    }
}

fn user_echoed(line: &Line) -> bool {
    line.spans
        .first()
        .map(|s| s.content == "you  ")
        .unwrap_or(false)
}

fn dim() -> Style {
    Style::default().fg(Color::DarkGray)
}

fn change_line(path: &str, info: &FileInfo) -> Line<'static> {
    let mark = match info.status.as_str() {
        "added" => Span::styled("A ", Style::default().fg(Color::Green)),
        "deleted" => Span::styled("D ", Style::default().fg(Color::Red)),
        _ => Span::styled("M ", Style::default().fg(Color::Yellow)),
    };
    Line::from(vec![
        Span::raw("  "),
        mark,
        Span::raw(truncate(path.to_string(), 26)),
        Span::styled(format!("  +{} -{}", info.added, info.removed), dim()),
    ])
}

fn truncate(mut s: String, max: usize) -> String {
    if s.chars().count() > max {
        s = s.chars().take(max.saturating_sub(1)).collect();
        s.push('…');
    }
    s
}

fn draw(frame: &mut Frame, app: &mut App) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(3),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(frame.area());

    let spin = if app.phase == Phase::Working {
        format!("{} ", SPINNER[app.spinner % SPINNER.len()])
    } else {
        String::new()
    };
    let tokens = format!(
        "{spin}in {} out {} tok",
        compact(app.input_tokens),
        compact(app.output_tokens)
    );
    let title = Paragraph::new(Line::from(vec![
        Span::styled(
            "slate",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  thread-weaving coding agent on hud-rs   "),
        Span::styled(app.models.clone(), dim()),
        Span::raw("   "),
        Span::styled(tokens, Style::default().fg(Color::Green)),
    ]))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!(" {} ", app.work_dir)),
    );
    frame.render_widget(title, rows[0]);

    if app.phase == Phase::FirstInput && app.weave.is_empty() {
        draw_welcome(frame, rows[1]);
    } else {
        draw_body(frame, app, rows[1]);
    }

    let prompt = Paragraph::new(Line::from(vec![
        Span::styled("> ", Style::default().fg(Color::Cyan)),
        Span::raw(app.input.clone()),
        Span::styled("_", dim()),
    ]))
    .block(Block::default().borders(Borders::ALL).title(" message "));
    frame.render_widget(prompt, rows[2]);

    frame.render_widget(
        Paragraph::new(Span::styled(app.status.clone(), dim())),
        rows[3],
    );
}

fn draw_welcome(frame: &mut Frame, area: Rect) {
    let text = vec![
        Line::default(),
        Line::styled(
            "  slate",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Line::raw("  A Slate-style thread-weaving coding agent on the HUD Rust SDK."),
        Line::default(),
        Line::styled("  Describe a coding task below and press Enter.", dim()),
        Line::styled(
            "  /help for commands · Esc interrupts a turn · Ctrl-C quits.",
            dim(),
        ),
    ];
    frame.render_widget(Paragraph::new(text), area);
}

fn draw_body(frame: &mut Frame, app: &mut App, area: Rect) {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(32), Constraint::Min(20)])
        .split(area);
    let side = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(columns[0]);

    let threads: Vec<ListItem> = app
        .threads
        .iter()
        .map(|(id, info)| {
            let marker = if info.busy { "*" } else { " " };
            ListItem::new(vec![
                Line::from(vec![
                    Span::styled(
                        format!("{marker} {id}"),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!("  {}a/{}c/{}e", info.actions, info.commands, info.episodes),
                        dim(),
                    ),
                ]),
                Line::styled(format!("  {}", info.last_action), dim()),
            ])
        })
        .collect();
    frame.render_widget(
        List::new(threads).block(Block::default().borders(Borders::ALL).title(" threads ")),
        side[0],
    );

    let changes: Vec<ListItem> = app
        .changes
        .iter()
        .map(|(path, info)| ListItem::new(change_line(path, info)))
        .collect();
    frame.render_widget(
        List::new(changes).block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" changes ({}) ", app.changes.len())),
        ),
        side[1],
    );

    let height = columns[1].height.saturating_sub(2);
    let max_scroll = (app.weave.len() as u16).saturating_sub(height);
    if app.auto_scroll {
        app.scroll = max_scroll;
    } else {
        app.scroll = app.scroll.min(max_scroll);
        if app.scroll == max_scroll {
            app.auto_scroll = true;
        }
    }
    frame.render_widget(
        Paragraph::new(app.weave.clone())
            .wrap(Wrap { trim: false })
            .scroll((app.scroll, 0))
            .block(Block::default().borders(Borders::ALL).title(" weave ")),
        columns[1],
    );
}

fn compact(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}
