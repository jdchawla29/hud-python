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
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};
use std::process::Command;

const HELP: &[&str] = &[
    "/help              show this help",
    "/diff              show the tracked Git patch",
    "/threads           list worker threads",
    "/thread <id>       show the latest episode for one worker",
    "/activity          collapse or expand command activity",
    "/clear             clear the weave view (history is kept on disk)",
    "/quit              end the session",
    "",
    "Tab changes pane           : opens the command palette",
    "Enter opens/sends           Esc interrupts or closes",
    "up/down PgUp/PgDn scroll    End resumes auto-follow",
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SessionFocus {
    Threads,
    Changes,
    Weave,
    Input,
}

impl SessionFocus {
    fn next(self) -> Self {
        match self {
            Self::Threads => Self::Changes,
            Self::Changes => Self::Weave,
            Self::Weave => Self::Input,
            Self::Input => Self::Threads,
        }
    }

    fn previous(self) -> Self {
        match self {
            Self::Threads => Self::Input,
            Self::Changes => Self::Threads,
            Self::Weave => Self::Changes,
            Self::Input => Self::Weave,
        }
    }
}

struct Viewer {
    title: String,
    lines: Vec<Line<'static>>,
    scroll: u16,
    scroll_cache: Option<ScrollCache>,
}

impl Viewer {
    fn new(title: String, lines: Vec<Line<'static>>) -> Self {
        Self {
            title,
            lines,
            scroll: 0,
            scroll_cache: None,
        }
    }
}

struct Palette {
    query: String,
    selected: usize,
}

#[derive(Clone, Copy)]
enum PaletteAction {
    Slash(&'static str),
    Focus(SessionFocus),
}

const PALETTE_ACTIONS: &[(&str, PaletteAction)] = &[
    ("Show help", PaletteAction::Slash("help")),
    ("Open workspace diff", PaletteAction::Slash("diff")),
    ("List worker threads", PaletteAction::Slash("threads")),
    ("Toggle command outputs", PaletteAction::Slash("activity")),
    ("Clear conversation view", PaletteAction::Slash("clear")),
    ("Focus threads", PaletteAction::Focus(SessionFocus::Threads)),
    ("Focus changes", PaletteAction::Focus(SessionFocus::Changes)),
    (
        "Focus conversation",
        PaletteAction::Focus(SessionFocus::Weave),
    ),
    (
        "Focus message input",
        PaletteAction::Focus(SessionFocus::Input),
    ),
    ("Quit session", PaletteAction::Slash("quit")),
];

#[derive(Default, Clone)]
struct ThreadInfo {
    actions: u32,
    commands: u32,
    episodes: u32,
    busy: bool,
    last_action: String,
    last_episode: String,
    command_history: Vec<CommandInfo>,
    episode_history: Vec<String>,
}

#[derive(Clone)]
struct CommandInfo {
    command: String,
    output: String,
    exit_status: u32,
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
    weave: Vec<WeaveLine>,
    threads: IndexMap<String, ThreadInfo>,
    changes: IndexMap<String, FileInfo>,
    input_tokens: u64,
    output_tokens: u64,
    scroll: u16,
    auto_scroll: bool,
    spinner: usize,
    work_dir: String,
    models: String,
    provisional_turn: Option<(usize, String)>,
    activity_expanded: bool,
    focus: SessionFocus,
    thread_index: usize,
    change_index: usize,
    viewer: Option<Viewer>,
    palette: Option<Palette>,
    weave_cache: Option<WeaveCache>,
}

#[derive(Clone)]
struct WeaveLine {
    line: Line<'static>,
    activity: bool,
}

struct WeaveCache {
    width: u16,
    height: u16,
    activity_expanded: bool,
    lines: Vec<Line<'static>>,
    max_scroll: u16,
}

#[derive(Clone, Copy)]
struct ScrollCache {
    width: u16,
    height: u16,
    max_scroll: u16,
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
        provisional_turn: None,
        activity_expanded: false,
        focus: SessionFocus::Input,
        thread_index: 0,
        change_index: 0,
        viewer: None,
        palette: None,
        weave_cache: None,
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
                    Some(HandleEvent::Ui(event)) => {
                        app.spinner = app.spinner.wrapping_add(1);
                        app.on_event(event);
                    }
                    Some(HandleEvent::Done(outcome)) => app.end_session(outcome),
                    None => {}
                }
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
        if self.palette.is_some() {
            return self.palette_key(key);
        }
        if self.viewer.is_some() {
            return self.viewer_key(key);
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return KeyOutcome::Quit;
        }
        match key.code {
            KeyCode::Esc => match self.phase {
                Phase::Working => KeyOutcome::Interrupt,
                _ => KeyOutcome::Quit,
            },
            KeyCode::Tab => {
                self.focus = self.focus.next();
                KeyOutcome::Handled
            }
            KeyCode::BackTab => {
                self.focus = self.focus.previous();
                KeyOutcome::Handled
            }
            KeyCode::Char(':') if self.input.is_empty() => {
                self.palette = Some(Palette {
                    query: String::new(),
                    selected: 0,
                });
                KeyOutcome::Handled
            }
            KeyCode::Char('/') if self.focus != SessionFocus::Input => {
                self.focus = SessionFocus::Input;
                self.input.push('/');
                KeyOutcome::Handled
            }
            _ => match self.focus {
                SessionFocus::Input => self.input_key(key),
                SessionFocus::Weave => self.weave_key(key),
                SessionFocus::Threads => self.threads_key(key),
                SessionFocus::Changes => self.changes_key(key),
            },
        }
    }

    fn input_key(&mut self, key: KeyEvent) -> KeyOutcome {
        match key.code {
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
            KeyCode::Char(character) => {
                self.input.push(character);
                KeyOutcome::Handled
            }
            KeyCode::Backspace => {
                self.input.pop();
                KeyOutcome::Handled
            }
            KeyCode::Up | KeyCode::PageUp => {
                self.focus = SessionFocus::Weave;
                self.weave_key(key)
            }
            _ => KeyOutcome::Handled,
        }
    }

    fn weave_key(&mut self, key: KeyEvent) -> KeyOutcome {
        match key.code {
            KeyCode::Up => {
                self.auto_scroll = false;
                self.scroll = self.scroll.saturating_sub(1);
            }
            KeyCode::Down => self.scroll = self.scroll.saturating_add(1),
            KeyCode::PageUp => {
                self.auto_scroll = false;
                self.scroll = self.scroll.saturating_sub(20);
            }
            KeyCode::PageDown => self.scroll = self.scroll.saturating_add(20),
            KeyCode::End => self.auto_scroll = true,
            _ => {}
        }
        KeyOutcome::Handled
    }

    fn threads_key(&mut self, key: KeyEvent) -> KeyOutcome {
        match key.code {
            KeyCode::Up => {
                self.thread_index = self.thread_index.saturating_sub(1);
            }
            KeyCode::Down => {
                self.thread_index = self
                    .thread_index
                    .saturating_add(1)
                    .min(self.threads.len().saturating_sub(1));
            }
            KeyCode::Enter => self.open_selected_thread(),
            _ => {}
        }
        KeyOutcome::Handled
    }

    fn changes_key(&mut self, key: KeyEvent) -> KeyOutcome {
        match key.code {
            KeyCode::Up => {
                self.change_index = self.change_index.saturating_sub(1);
            }
            KeyCode::Down => {
                self.change_index = self
                    .change_index
                    .saturating_add(1)
                    .min(self.changes.len().saturating_sub(1));
            }
            KeyCode::Enter => self.open_selected_diff(),
            _ => {}
        }
        KeyOutcome::Handled
    }

    fn viewer_key(&mut self, key: KeyEvent) -> KeyOutcome {
        let Some(viewer) = self.viewer.as_mut() else {
            return KeyOutcome::Handled;
        };
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.viewer = None,
            KeyCode::Up => viewer.scroll = viewer.scroll.saturating_sub(1),
            KeyCode::Down => viewer.scroll = viewer.scroll.saturating_add(1),
            KeyCode::PageUp => viewer.scroll = viewer.scroll.saturating_sub(20),
            KeyCode::PageDown => viewer.scroll = viewer.scroll.saturating_add(20),
            KeyCode::Home => viewer.scroll = 0,
            _ => {}
        }
        KeyOutcome::Handled
    }

    fn palette_key(&mut self, key: KeyEvent) -> KeyOutcome {
        let Some(mut palette) = self.palette.take() else {
            return KeyOutcome::Handled;
        };
        match key.code {
            KeyCode::Esc => KeyOutcome::Handled,
            KeyCode::Up => {
                palette.selected = palette.selected.saturating_sub(1);
                self.palette = Some(palette);
                KeyOutcome::Handled
            }
            KeyCode::Down => {
                palette.selected = palette
                    .selected
                    .saturating_add(1)
                    .min(palette_matches(&palette.query).len().saturating_sub(1));
                self.palette = Some(palette);
                KeyOutcome::Handled
            }
            KeyCode::Backspace => {
                palette.query.pop();
                palette.selected = 0;
                self.palette = Some(palette);
                KeyOutcome::Handled
            }
            KeyCode::Char(character) => {
                palette.query.push(character);
                palette.selected = 0;
                self.palette = Some(palette);
                KeyOutcome::Handled
            }
            KeyCode::Enter => {
                let matches = palette_matches(&palette.query);
                let Some((_, action)) = matches.get(palette.selected).copied() else {
                    return KeyOutcome::Handled;
                };
                match action {
                    PaletteAction::Slash(command) => self.slash(command),
                    PaletteAction::Focus(focus) => {
                        self.focus = focus;
                        KeyOutcome::Handled
                    }
                }
            }
            _ => {
                self.palette = Some(palette);
                KeyOutcome::Handled
            }
        }
    }

    fn slash(&mut self, command: &str) -> KeyOutcome {
        let mut parts = command.split_whitespace();
        match parts.next().unwrap_or("") {
            "quit" | "exit" | "q" => KeyOutcome::Quit,
            "help" | "h" => {
                self.viewer = Some(Viewer::new(
                    "help".to_string(),
                    HELP.iter()
                        .map(|line| Line::styled((*line).to_string(), dim()))
                        .collect(),
                ));
                KeyOutcome::Handled
            }
            "clear" => {
                self.weave.clear();
                self.invalidate_weave();
                KeyOutcome::Handled
            }
            "diff" => {
                self.open_diff(None);
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
            "thread" => {
                let Some(id) = parts.next() else {
                    self.note("usage: /thread <id>");
                    return KeyOutcome::Handled;
                };
                if !self.threads.contains_key(id) {
                    self.note(&format!("unknown worker thread {id:?}"));
                    return KeyOutcome::Handled;
                }
                self.open_thread(id);
                KeyOutcome::Handled
            }
            "activity" => {
                self.activity_expanded = !self.activity_expanded;
                self.note(if self.activity_expanded {
                    "command activity expanded"
                } else {
                    "command activity collapsed"
                });
                KeyOutcome::Handled
            }
            other => {
                self.note(&format!("unknown command /{other} (try /help)"));
                KeyOutcome::Handled
            }
        }
    }

    fn open_selected_thread(&mut self) {
        let id = self
            .threads
            .get_index(self.thread_index)
            .map(|(id, _)| id.clone());
        if let Some(id) = id {
            self.open_thread(&id);
        }
    }

    fn open_thread(&mut self, id: &str) {
        let Some(info) = self.threads.get(id).cloned() else {
            return;
        };
        let mut lines = vec![
            Line::styled(
                format!(
                    "{id} · {} actions · {} commands · {} episodes",
                    info.actions, info.commands, info.episodes
                ),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Line::styled(format!("last action: {}", info.last_action), dim()),
        ];
        for (index, command) in info.command_history.iter().enumerate() {
            lines.push(Line::default());
            let style = if command.exit_status == 0 {
                dim()
            } else {
                Style::default().fg(Color::Red)
            };
            lines.push(Line::styled(
                format!("command {} · exit {}", index + 1, command.exit_status),
                style,
            ));
            lines.push(Line::raw(format!("$ {}", command.command)));
            for output in command.output.lines() {
                lines.push(Line::styled(format!("  {output}"), dim()));
            }
        }
        for (index, episode) in info.episode_history.iter().enumerate() {
            lines.push(Line::default());
            lines.push(Line::styled(
                format!("episode {}", index + 1),
                Style::default().fg(Color::Magenta),
            ));
            lines.extend(markdown_lines(episode, "  ", Style::default()));
        }
        self.viewer = Some(Viewer::new(format!("thread {id}"), lines));
    }

    fn open_selected_diff(&mut self) {
        let path = self
            .changes
            .get_index(self.change_index)
            .map(|(path, _)| path.clone());
        if let Some(path) = path {
            self.open_diff(Some(&path));
        }
    }

    fn open_diff(&mut self, path: Option<&str>) {
        match workspace_diff(&self.work_dir, path) {
            Ok(diff) if diff.trim().is_empty() => self.note("no tracked workspace diff"),
            Ok(diff) => {
                self.viewer = Some(Viewer::new(
                    path.map_or_else(|| "workspace diff".to_string(), |path| path.to_string()),
                    markdown_lines(&format!("```diff\n{diff}\n```"), "", dim()),
                ));
            }
            Err(error) => self.note(&format!("could not read diff: {error}")),
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
                self.provisional_turn = None;
                // The first message is already shown by begin_session; echo
                // only later ones (avoid a duplicate line).
                if self.weave.last().map(|line| user_echoed(&line.line)) != Some(true) {
                    self.push_user(&text);
                }
                self.phase = Phase::Working;
            }
            UiEvent::OrchTurn { turn, text } => {
                let start = self.weave.len();
                self.push(Line::default());
                self.push(Line::styled(
                    format!("orchestrator · turn {turn}"),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ));
                for line in markdown_lines(&text, "  ", Style::default()) {
                    self.push(line);
                }
                self.provisional_turn = Some((start, text));
            }
            UiEvent::Dispatch {
                thread,
                action,
                seeded,
            } => {
                self.provisional_turn = None;
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
                self.provisional_turn = None;
                let head = text.lines().next().unwrap_or("").to_string();
                self.status = truncate(format!("[{thread}] {head}"), 120);
            }
            UiEvent::Bash {
                thread,
                command,
                output,
                exit_status,
            } => {
                self.provisional_turn = None;
                if let Some(info) = self.threads.get_mut(&thread) {
                    info.commands += 1;
                    info.command_history.push(CommandInfo {
                        command: command.clone(),
                        output: output.clone(),
                        exit_status,
                    });
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
                self.push(Line::styled(line, style));
                for output_line in output.lines() {
                    self.push_activity(Line::styled(format!("       │ {output_line}"), dim()));
                }
            }
            UiEvent::Episode { thread, text } => {
                self.provisional_turn = None;
                if let Some(info) = self.threads.get_mut(&thread) {
                    info.episodes += 1;
                    info.busy = false;
                    info.last_episode = text.clone();
                    info.episode_history.push(text.clone());
                }
                self.push(Line::styled(
                    format!("  ← episode [{thread}]"),
                    Style::default().fg(Color::Magenta),
                ));
                for line in markdown_lines(&text, "     ", dim()) {
                    self.push(line);
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
                if let Some((start, provisional)) = self.provisional_turn.take() {
                    if provisional == text {
                        self.weave.truncate(start);
                        self.invalidate_weave();
                    }
                }
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
                for line in markdown_lines(&text, "  ", Style::default()) {
                    self.push(line);
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
        self.weave.push(WeaveLine {
            line,
            activity: false,
        });
        self.invalidate_weave();
    }

    fn push_activity(&mut self, line: Line<'static>) {
        self.weave.push(WeaveLine {
            line,
            activity: true,
        });
        self.invalidate_weave();
    }

    fn invalidate_weave(&mut self) {
        self.weave_cache = None;
    }

    fn ensure_weave_cache(&mut self, area: Rect) {
        let cache_matches = self.weave_cache.as_ref().is_some_and(|cache| {
            cache.width == area.width
                && cache.height == area.height
                && cache.activity_expanded == self.activity_expanded
        });
        if cache_matches {
            return;
        }

        let lines: Vec<Line<'static>> = self
            .weave
            .iter()
            .filter(|line| self.activity_expanded || !line.activity)
            .map(|line| line.line.clone())
            .collect();
        let max_scroll = max_scroll(&lines, area);
        self.weave_cache = Some(WeaveCache {
            width: area.width,
            height: area.height,
            activity_expanded: self.activity_expanded,
            lines,
            max_scroll,
        });
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

fn workspace_diff(work_dir: &str, path: Option<&str>) -> Result<String, String> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(work_dir)
        .args(["diff", "--no-ext-diff", "HEAD", "--"]);
    if let Some(path) = path {
        command.arg(path);
    }
    let output = command.output().map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    String::from_utf8(output.stdout).map_err(|error| error.to_string())
}

fn markdown_lines(text: &str, prefix: &str, base: Style) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut code = false;
    for source in text.lines() {
        if let Some(language) = source.trim_start().strip_prefix("```") {
            code = !code;
            let marker = if code { "┌" } else { "└" };
            let label = language.trim();
            lines.push(Line::styled(
                format!("{prefix}{marker} {label}"),
                base.fg(Color::DarkGray),
            ));
            continue;
        }
        if code {
            lines.push(Line::styled(
                format!("{prefix}│ {source}"),
                base.fg(Color::Green),
            ));
            continue;
        }
        let trimmed = source.trim_start();
        let heading_level = trimmed
            .chars()
            .take_while(|character| *character == '#')
            .count();
        if heading_level > 0
            && heading_level <= 6
            && trimmed.chars().nth(heading_level) == Some(' ')
        {
            let heading = trimmed[heading_level + 1..].to_string();
            lines.push(Line::styled(
                format!("{prefix}{heading}"),
                base.fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ));
        } else if let Some(item) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
        {
            let mut spans = vec![Span::styled(format!("{prefix}• "), base.fg(Color::Yellow))];
            spans.extend(inline_spans(item, base));
            lines.push(Line::from(spans));
        } else if let Some(quote) = trimmed.strip_prefix("> ") {
            let mut spans = vec![Span::styled(format!("{prefix}│ "), dim())];
            spans.extend(inline_spans(quote, base.add_modifier(Modifier::ITALIC)));
            lines.push(Line::from(spans));
        } else {
            let mut spans = vec![Span::styled(prefix.to_string(), base)];
            spans.extend(inline_spans(source, base));
            lines.push(Line::from(spans));
        }
    }
    lines
}

fn inline_spans(text: &str, base: Style) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for (code_index, segment) in text.split('`').enumerate() {
        if code_index % 2 == 1 {
            spans.push(Span::styled(segment.to_string(), base.fg(Color::Yellow)));
            continue;
        }
        for (bold_index, part) in segment.split("**").enumerate() {
            let style = if bold_index % 2 == 1 {
                base.add_modifier(Modifier::BOLD)
            } else {
                base
            };
            spans.push(Span::styled(part.to_string(), style));
        }
    }
    spans
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
            "das",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  thread-weaving coding agent on hud-rs   "),
        Span::styled(app.models.clone(), dim()),
        Span::raw("   "),
        Span::styled(tokens, Style::default().fg(Color::Green)),
        Span::raw("   "),
        Span::styled(
            if app.activity_expanded {
                "outputs expanded"
            } else {
                "outputs collapsed"
            },
            dim(),
        ),
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
    .block(focused_block(" message ", app.focus == SessionFocus::Input));
    frame.render_widget(prompt, rows[2]);

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(app.status.clone(), dim()),
            Span::styled("   Tab changes focus · / commands", dim()),
        ])),
        rows[3],
    );
    if let Some(viewer) = app.viewer.as_mut() {
        draw_viewer(frame, viewer);
    } else if let Some(palette) = app.palette.as_ref() {
        draw_palette(frame, palette);
    }
}

fn draw_welcome(frame: &mut Frame, area: Rect) {
    let text = vec![
        Line::default(),
        Line::styled(
            "  das",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Line::raw("  Project-aware thread-weaving coding agent on the HUD Rust SDK."),
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
    if area.width < 100 {
        match app.focus {
            SessionFocus::Threads => draw_threads(frame, app, area),
            SessionFocus::Changes => draw_changes(frame, app, area),
            SessionFocus::Weave | SessionFocus::Input => draw_weave(frame, app, area),
        }
        return;
    }
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(32), Constraint::Min(20)])
        .split(area);
    let side = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(columns[0]);

    draw_threads(frame, app, side[0]);
    draw_changes(frame, app, side[1]);
    draw_weave(frame, app, columns[1]);
}

fn draw_threads(frame: &mut Frame, app: &App, area: Rect) {
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
    let mut state = ListState::default().with_selected(
        (!threads.is_empty()).then_some(app.thread_index.min(threads.len().saturating_sub(1))),
    );
    frame.render_stateful_widget(
        List::new(threads)
            .block(focused_block(
                " threads ",
                app.focus == SessionFocus::Threads,
            ))
            .highlight_style(Style::default().bg(Color::DarkGray))
            .highlight_symbol("› "),
        area,
        &mut state,
    );
}

fn draw_changes(frame: &mut Frame, app: &App, area: Rect) {
    let changes: Vec<ListItem> = app
        .changes
        .iter()
        .map(|(path, info)| ListItem::new(change_line(path, info)))
        .collect();
    let mut state = ListState::default().with_selected(
        (!changes.is_empty()).then_some(app.change_index.min(changes.len().saturating_sub(1))),
    );
    frame.render_stateful_widget(
        List::new(changes)
            .block(focused_block(
                &format!(" changes ({}) ", app.changes.len()),
                app.focus == SessionFocus::Changes,
            ))
            .highlight_style(Style::default().bg(Color::DarkGray))
            .highlight_symbol("› "),
        area,
        &mut state,
    );
}

fn draw_weave(frame: &mut Frame, app: &mut App, area: Rect) {
    app.ensure_weave_cache(area);
    let max_scroll = app.weave_cache.as_ref().map_or(0, |cache| cache.max_scroll);
    if app.auto_scroll {
        app.scroll = max_scroll;
    } else {
        app.scroll = app.scroll.min(max_scroll);
        if app.scroll == max_scroll {
            app.auto_scroll = true;
        }
    }
    let weave = app
        .weave_cache
        .as_ref()
        .map(|cache| cache.lines.clone())
        .unwrap_or_default();
    frame.render_widget(
        Paragraph::new(weave)
            .wrap(Wrap { trim: false })
            .scroll((app.scroll, 0))
            .block(focused_block(" weave ", app.focus == SessionFocus::Weave)),
        area,
    );
}

fn draw_viewer(frame: &mut Frame, viewer: &mut Viewer) {
    let frame_area = frame.area();
    let area = Rect::new(
        frame_area.x.saturating_add(2),
        frame_area.y.saturating_add(1),
        frame_area.width.saturating_sub(4),
        frame_area.height.saturating_sub(2),
    );
    let cache_matches = viewer
        .scroll_cache
        .is_some_and(|cache| cache.width == area.width && cache.height == area.height);
    if !cache_matches {
        viewer.scroll_cache = Some(ScrollCache {
            width: area.width,
            height: area.height,
            max_scroll: max_scroll(&viewer.lines, area),
        });
    }
    let max = viewer.scroll_cache.map_or(0, |cache| cache.max_scroll);
    viewer.scroll = viewer.scroll.min(max);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(viewer.lines.clone())
            .wrap(Wrap { trim: false })
            .scroll((viewer.scroll, 0))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Cyan))
                    .title(format!(" {} · Esc closes ", viewer.title)),
            ),
        area,
    );
}

fn draw_palette(frame: &mut Frame, palette: &Palette) {
    let frame_area = frame.area();
    let width = 64.min(frame_area.width.saturating_sub(4));
    let height = 16.min(frame_area.height.saturating_sub(4));
    let area = Rect::new(
        frame_area.x + frame_area.width.saturating_sub(width) / 2,
        frame_area.y + frame_area.height.saturating_sub(height) / 3,
        width,
        height,
    );
    frame.render_widget(Clear, area);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(3)])
        .split(area);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(": ", Style::default().fg(Color::Cyan)),
            Span::raw(palette.query.clone()),
            Span::styled("_", dim()),
        ]))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan))
                .title(" command palette "),
        ),
        rows[0],
    );
    let matches = palette_matches(&palette.query);
    let items: Vec<ListItem> = matches
        .iter()
        .map(|(label, _)| ListItem::new(Line::raw(*label)))
        .collect();
    let mut state = ListState::default().with_selected(
        (!items.is_empty()).then_some(palette.selected.min(items.len().saturating_sub(1))),
    );
    frame.render_stateful_widget(
        List::new(items)
            .block(Block::default().borders(Borders::ALL).title(" actions "))
            .highlight_style(Style::default().bg(Color::DarkGray))
            .highlight_symbol("› "),
        rows[1],
        &mut state,
    );
}

fn focused_block<'a>(title: &'a str, active: bool) -> Block<'a> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(if active {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default()
        })
        .title(title)
}

fn palette_matches(query: &str) -> Vec<(&'static str, PaletteAction)> {
    let query = query.to_ascii_lowercase();
    PALETTE_ACTIONS
        .iter()
        .copied()
        .filter(|(label, _)| label.to_ascii_lowercase().contains(&query))
        .collect()
}

fn max_scroll(lines: &[Line<'static>], area: Rect) -> u16 {
    let width = area.width.saturating_sub(2);
    let height = area.height.saturating_sub(2) as usize;
    let rendered_height = Paragraph::new(lines.to_vec())
        .wrap(Wrap { trim: false })
        .line_count(width);
    rendered_height
        .saturating_sub(height)
        .min(u16::MAX as usize) as u16
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

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> App {
        App {
            phase: Phase::Working,
            input: String::new(),
            status: String::new(),
            weave: Vec::new(),
            threads: IndexMap::new(),
            changes: IndexMap::new(),
            input_tokens: 0,
            output_tokens: 0,
            scroll: 0,
            auto_scroll: true,
            spinner: 0,
            work_dir: String::new(),
            models: String::new(),
            provisional_turn: None,
            activity_expanded: false,
            focus: SessionFocus::Input,
            thread_index: 0,
            change_index: 0,
            viewer: None,
            palette: None,
            weave_cache: None,
        }
    }

    #[test]
    fn scroll_range_accounts_for_wrapped_rows() {
        let lines = vec![Line::raw("word ".repeat(20))];
        let area = Rect::new(0, 0, 22, 5);

        assert_eq!(max_scroll(&lines, area), 2);
    }

    #[test]
    fn weave_layout_is_cached_until_content_changes() {
        let mut app = app();
        let area = Rect::new(0, 0, 22, 5);
        app.push(Line::raw("word ".repeat(20)));

        app.ensure_weave_cache(area);
        assert_eq!(app.weave_cache.as_ref().unwrap().max_scroll, 2);
        assert!(app.weave_cache.is_some());

        app.push(Line::raw("more"));
        assert!(app.weave_cache.is_none());
    }

    #[test]
    fn final_reply_replaces_identical_provisional_turn() {
        let mut app = app();
        app.on_event(UiEvent::OrchTurn {
            turn: 1,
            text: "finished".to_string(),
        });
        app.on_event(UiEvent::Reply {
            text: "finished".to_string(),
            stop: ReplyStop::Finished,
        });

        let text = app
            .weave
            .iter()
            .flat_map(|line| &line.line.spans)
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert_eq!(text.matches("finished").count(), 1);
        assert!(!text.contains("orchestrator"));
    }

    #[test]
    fn markdown_styles_headings_lists_and_inline_code() {
        let lines = markdown_lines("## Heading\n- use `cargo test`", "  ", Style::default());

        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].spans[0].content, "  Heading");
        assert!(lines[0].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(lines[1].spans[0].content, "  • ");
        assert!(lines[1]
            .spans
            .iter()
            .any(|span| span.content == "cargo test"));
    }

    #[test]
    fn tab_moves_focus_and_colon_opens_palette() {
        let mut app = app();
        assert_eq!(app.focus, SessionFocus::Input);
        app.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.focus, SessionFocus::Threads);
        app.on_key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE));
        assert!(app.palette.is_some());
    }

    #[test]
    fn command_output_is_retained_as_collapsible_activity() {
        let mut app = app();
        app.on_event(UiEvent::Dispatch {
            thread: "worker".to_string(),
            action: "inspect".to_string(),
            seeded: false,
        });
        app.on_event(UiEvent::Bash {
            thread: "worker".to_string(),
            command: "cargo test".to_string(),
            output: "all tests passed".to_string(),
            exit_status: 0,
        });

        assert_eq!(app.threads["worker"].command_history.len(), 1);
        assert!(app.weave.iter().any(|line| line.activity));
    }
}
