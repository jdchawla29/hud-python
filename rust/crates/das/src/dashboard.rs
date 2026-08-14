use crate::cli::{archive_workspace, provision_workspace, register_project};
use crate::harness::WorkerHarnessKind;
use crate::model::{Project, Workspace, WorkspaceState};
use crate::orchestrate::OpenOptions;
use crate::orchestrator::{OrchestratorKind, DEFAULT_GATEWAY_MODEL};
use crate::session::{SessionMeta, SessionStore};
use crate::state::State;
use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};
use std::path::{Path, PathBuf};

pub async fn run(state: &State, home: &Path) -> Result<()> {
    let mut app = Dashboard::load(state, home.to_path_buf())?;
    loop {
        match run_screen(&mut app, state)? {
            DashboardAction::Quit => return Ok(()),
            DashboardAction::Open(options) => {
                let result = crate::orchestrate::open(*options).await;
                app.status = match result {
                    Ok(()) => "session closed".to_string(),
                    Err(error) => format!("session failed: {error:#}"),
                };
                app.refresh(state)?;
            }
        }
    }
}

fn run_screen(app: &mut Dashboard, state: &State) -> Result<DashboardAction> {
    let mut terminal = ratatui::init();
    let result = run_loop(&mut terminal, app, state);
    ratatui::restore();
    result
}

fn run_loop(
    terminal: &mut DefaultTerminal,
    app: &mut Dashboard,
    state: &State,
) -> Result<DashboardAction> {
    loop {
        terminal.draw(|frame| draw(frame, app))?;
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        if let Some(action) = app.on_key(state, key)? {
            return Ok(action);
        }
    }
}

enum DashboardAction {
    Quit,
    Open(Box<OpenOptions>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Focus {
    Projects,
    Workspaces,
    Sessions,
}

impl Focus {
    fn next(self) -> Self {
        match self {
            Self::Projects => Self::Workspaces,
            Self::Workspaces => Self::Sessions,
            Self::Sessions => Self::Projects,
        }
    }

    fn previous(self) -> Self {
        match self {
            Self::Projects => Self::Sessions,
            Self::Workspaces => Self::Projects,
            Self::Sessions => Self::Workspaces,
        }
    }
}

enum Overlay {
    None,
    Help,
    AddProject(ProjectForm),
    CreateWorkspace(TextInput),
    NewSession(SessionForm),
    ConfirmArchive,
}

struct Dashboard {
    home: PathBuf,
    projects: Vec<Project>,
    workspaces: Vec<Workspace>,
    sessions: Vec<SessionMeta>,
    project_index: usize,
    workspace_index: usize,
    session_index: usize,
    focus: Focus,
    overlay: Overlay,
    status: String,
}

impl Dashboard {
    fn load(state: &State, home: PathBuf) -> Result<Self> {
        let mut app = Self {
            home,
            projects: Vec::new(),
            workspaces: Vec::new(),
            sessions: Vec::new(),
            project_index: 0,
            workspace_index: 0,
            session_index: 0,
            focus: Focus::Projects,
            overlay: Overlay::None,
            status: "p add project · w create workspace · o new session · ? help".to_string(),
        };
        app.refresh(state)?;
        Ok(app)
    }

    fn refresh(&mut self, state: &State) -> Result<()> {
        let project_id = self.project().map(|project| project.id);
        let workspace_id = self.workspace().map(|workspace| workspace.id);
        self.projects = state.projects()?;
        self.project_index = selected_index(&self.projects, project_id, |project| project.id);
        self.refresh_workspaces(state, workspace_id)
    }

    fn refresh_workspaces(&mut self, state: &State, selected: Option<i64>) -> Result<()> {
        self.workspaces = match self.project() {
            Some(project) => state
                .workspaces(Some(project.id))?
                .into_iter()
                .map(|(_, workspace)| workspace)
                .collect(),
            None => Vec::new(),
        };
        self.workspace_index = selected_index(&self.workspaces, selected, |workspace| workspace.id);
        self.refresh_sessions()
    }

    fn refresh_sessions(&mut self) -> Result<()> {
        let selected = self.session().map(|session| session.id.clone());
        self.sessions = match self.workspace() {
            Some(workspace) => SessionStore::list_for_workspace(workspace.id)?,
            None => Vec::new(),
        };
        self.session_index = self
            .sessions
            .iter()
            .position(|session| Some(&session.id) == selected.as_ref())
            .unwrap_or(0)
            .min(self.sessions.len().saturating_sub(1));
        Ok(())
    }

    fn project(&self) -> Option<&Project> {
        self.projects.get(self.project_index)
    }

    fn workspace(&self) -> Option<&Workspace> {
        self.workspaces.get(self.workspace_index)
    }

    fn session(&self) -> Option<&SessionMeta> {
        self.sessions.get(self.session_index)
    }

    fn on_key(&mut self, state: &State, key: KeyEvent) -> Result<Option<DashboardAction>> {
        if !matches!(self.overlay, Overlay::None) {
            return self.on_overlay_key(state, key);
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Ok(Some(DashboardAction::Quit));
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(Some(DashboardAction::Quit)),
            KeyCode::Tab | KeyCode::Right => self.focus = self.focus.next(),
            KeyCode::BackTab | KeyCode::Left => self.focus = self.focus.previous(),
            KeyCode::Up => self.move_selection(state, -1)?,
            KeyCode::Down => self.move_selection(state, 1)?,
            KeyCode::Enter => return self.activate(),
            KeyCode::Char('?') => self.overlay = Overlay::Help,
            KeyCode::Char('r') => {
                self.refresh(state)?;
                self.status = "refreshed".to_string();
            }
            KeyCode::Char('p') => self.overlay = Overlay::AddProject(ProjectForm::default()),
            KeyCode::Char('w') => {
                if self.project().is_some() {
                    self.overlay = Overlay::CreateWorkspace(TextInput::default());
                } else {
                    self.status = "register a project first".to_string();
                }
            }
            KeyCode::Char('o') => {
                if self.workspace().is_some_and(ready) {
                    self.overlay = Overlay::NewSession(SessionForm::default());
                } else {
                    self.status = "select a ready workspace".to_string();
                }
            }
            KeyCode::Char('a') => {
                if self.workspace().is_some_and(ready) {
                    self.overlay = Overlay::ConfirmArchive;
                } else {
                    self.status = "select a ready workspace".to_string();
                }
            }
            _ => {}
        }
        Ok(None)
    }

    fn move_selection(&mut self, state: &State, delta: isize) -> Result<()> {
        match self.focus {
            Focus::Projects => {
                self.project_index = moved(self.project_index, self.projects.len(), delta);
                self.refresh_workspaces(state, None)?;
            }
            Focus::Workspaces => {
                self.workspace_index = moved(self.workspace_index, self.workspaces.len(), delta);
                self.refresh_sessions()?;
            }
            Focus::Sessions => {
                self.session_index = moved(self.session_index, self.sessions.len(), delta);
            }
        }
        Ok(())
    }

    fn activate(&mut self) -> Result<Option<DashboardAction>> {
        match self.focus {
            Focus::Projects => self.focus = Focus::Workspaces,
            Focus::Workspaces => {
                if self.workspace().is_some_and(ready) {
                    self.overlay = Overlay::NewSession(SessionForm::default());
                } else {
                    self.status = "workspace is not ready".to_string();
                }
            }
            Focus::Sessions => {
                if let Some(session) = self.session() {
                    return Ok(Some(DashboardAction::Open(Box::new(self.open_options(
                        Some(session.id.clone()),
                        SessionForm::default(),
                    )?))));
                }
            }
        }
        Ok(None)
    }

    fn on_overlay_key(&mut self, state: &State, key: KeyEvent) -> Result<Option<DashboardAction>> {
        let overlay = std::mem::replace(&mut self.overlay, Overlay::None);
        match overlay {
            Overlay::None => {}
            Overlay::Help => {
                if !matches!(key.code, KeyCode::Esc | KeyCode::Char('?') | KeyCode::Enter) {
                    self.overlay = Overlay::Help;
                }
            }
            Overlay::AddProject(mut form) => match form.on_key(key) {
                FormAction::Keep => self.overlay = Overlay::AddProject(form),
                FormAction::Cancel => {}
                FormAction::Submit => {
                    let base = nonempty(form.base.clone());
                    match register_project(
                        state,
                        &self.home,
                        form.name.trim(),
                        Path::new(form.path.trim()),
                        base,
                        None,
                    ) {
                        Ok(project) => {
                            self.status = format!("registered {}", project.name);
                            self.refresh(state)?;
                            self.project_index = self
                                .projects
                                .iter()
                                .position(|candidate| candidate.id == project.id)
                                .unwrap_or(0);
                            self.refresh_workspaces(state, None)?;
                        }
                        Err(error) => {
                            self.status = format!("project error: {error:#}");
                            self.overlay = Overlay::AddProject(form);
                        }
                    }
                }
            },
            Overlay::CreateWorkspace(mut input) => match input.on_key(key) {
                FormAction::Keep => self.overlay = Overlay::CreateWorkspace(input),
                FormAction::Cancel => {}
                FormAction::Submit => {
                    let Some(project) = self.project().cloned() else {
                        return Ok(None);
                    };
                    match provision_workspace(state, &project.name, input.value.trim(), None, None)
                    {
                        Ok((_, workspace)) => {
                            self.status = format!("created {}/{}", project.name, workspace.name);
                            self.refresh_workspaces(state, Some(workspace.id))?;
                            self.focus = Focus::Workspaces;
                        }
                        Err(error) => {
                            self.status = format!("workspace error: {error:#}");
                            self.overlay = Overlay::CreateWorkspace(input);
                        }
                    }
                }
            },
            Overlay::NewSession(mut form) => match form.on_key(key) {
                FormAction::Keep => self.overlay = Overlay::NewSession(form),
                FormAction::Cancel => {}
                FormAction::Submit => {
                    return Ok(Some(DashboardAction::Open(Box::new(
                        self.open_options(None, form)?,
                    ))));
                }
            },
            Overlay::ConfirmArchive => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    let Some(project) = self.project().cloned() else {
                        return Ok(None);
                    };
                    let Some(workspace) = self.workspace().cloned() else {
                        return Ok(None);
                    };
                    match archive_workspace(state, &project, &workspace, false) {
                        Ok(()) => {
                            self.status = format!(
                                "archived {}/{}; branch retained",
                                project.name, workspace.name
                            );
                            self.refresh_workspaces(state, None)?;
                        }
                        Err(error) => {
                            self.status = format!("archive refused: {error:#}");
                        }
                    }
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {}
                _ => self.overlay = Overlay::ConfirmArchive,
            },
        }
        Ok(None)
    }

    fn open_options(&self, resume: Option<String>, form: SessionForm) -> Result<OpenOptions> {
        let project = self.project().context("no project selected")?.clone();
        let workspace = self.workspace().context("no workspace selected")?.clone();
        if !ready(&workspace) {
            anyhow::bail!("workspace '{}' is not ready", workspace.name);
        }
        Ok(OpenOptions {
            project,
            workspace,
            task: None,
            orch_harness: form.orch_harness,
            orch_model: nonempty(form.orch_model),
            worker_harness: form.worker_harness,
            worker_model: nonempty(form.worker_model),
            codex_socket: None,
            max_turns: 40,
            hud_python: None,
            resume,
            no_save: false,
            headless: false,
            messages: Vec::new(),
        })
    }
}

#[derive(Default)]
struct ProjectForm {
    name: String,
    path: String,
    base: String,
    field: usize,
}

impl ProjectForm {
    fn on_key(&mut self, key: KeyEvent) -> FormAction {
        match key.code {
            KeyCode::Esc => FormAction::Cancel,
            KeyCode::Tab | KeyCode::Down => {
                self.field = (self.field + 1) % 3;
                FormAction::Keep
            }
            KeyCode::BackTab | KeyCode::Up => {
                self.field = (self.field + 2) % 3;
                FormAction::Keep
            }
            KeyCode::Enter if self.field < 2 => {
                self.field += 1;
                FormAction::Keep
            }
            KeyCode::Enter => FormAction::Submit,
            KeyCode::Backspace => {
                self.current().pop();
                FormAction::Keep
            }
            KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.current().push(character);
                FormAction::Keep
            }
            _ => FormAction::Keep,
        }
    }

    fn current(&mut self) -> &mut String {
        match self.field {
            0 => &mut self.name,
            1 => &mut self.path,
            _ => &mut self.base,
        }
    }
}

#[derive(Default)]
struct TextInput {
    value: String,
}

impl TextInput {
    fn on_key(&mut self, key: KeyEvent) -> FormAction {
        match key.code {
            KeyCode::Esc => FormAction::Cancel,
            KeyCode::Enter => FormAction::Submit,
            KeyCode::Backspace => {
                self.value.pop();
                FormAction::Keep
            }
            KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.value.push(character);
                FormAction::Keep
            }
            _ => FormAction::Keep,
        }
    }
}

struct SessionForm {
    orch_harness: OrchestratorKind,
    orch_model: String,
    worker_harness: WorkerHarnessKind,
    worker_model: String,
    field: usize,
}

impl Default for SessionForm {
    fn default() -> Self {
        Self {
            orch_harness: OrchestratorKind::Gateway,
            orch_model: DEFAULT_GATEWAY_MODEL.to_string(),
            worker_harness: WorkerHarnessKind::Codex,
            worker_model: String::new(),
            field: 0,
        }
    }
}

impl SessionForm {
    fn on_key(&mut self, key: KeyEvent) -> FormAction {
        match key.code {
            KeyCode::Esc => FormAction::Cancel,
            KeyCode::Tab | KeyCode::Down => {
                self.field = (self.field + 1) % 4;
                FormAction::Keep
            }
            KeyCode::BackTab | KeyCode::Up => {
                self.field = (self.field + 3) % 4;
                FormAction::Keep
            }
            KeyCode::Enter => FormAction::Submit,
            KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') if self.field == 0 => {
                self.orch_harness = match self.orch_harness {
                    OrchestratorKind::Gateway => OrchestratorKind::Claude,
                    OrchestratorKind::Claude => OrchestratorKind::Gateway,
                };
                self.orch_model = match self.orch_harness {
                    OrchestratorKind::Gateway => DEFAULT_GATEWAY_MODEL.to_string(),
                    OrchestratorKind::Claude => String::new(),
                };
                FormAction::Keep
            }
            KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') if self.field == 2 => {
                self.worker_harness = match self.worker_harness {
                    WorkerHarnessKind::Codex => WorkerHarnessKind::Claude,
                    WorkerHarnessKind::Claude => WorkerHarnessKind::Codex,
                };
                self.worker_model.clear();
                FormAction::Keep
            }
            KeyCode::Backspace if self.field == 1 => {
                self.orch_model.pop();
                FormAction::Keep
            }
            KeyCode::Backspace if self.field == 3 => {
                self.worker_model.pop();
                FormAction::Keep
            }
            KeyCode::Char(character)
                if self.field == 1 && !key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                self.orch_model.push(character);
                FormAction::Keep
            }
            KeyCode::Char(character)
                if self.field == 3 && !key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                self.worker_model.push(character);
                FormAction::Keep
            }
            _ => FormAction::Keep,
        }
    }
}

enum FormAction {
    Keep,
    Cancel,
    Submit,
}

fn draw(frame: &mut Frame, app: &Dashboard) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(8),
            Constraint::Length(2),
        ])
        .split(frame.area());
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                "das",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  projects · worktrees · coding sessions"),
        ]))
        .block(Block::default().borders(Borders::ALL).title(" dashboard ")),
        rows[0],
    );

    if rows[1].width >= 96 {
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(24),
                Constraint::Percentage(34),
                Constraint::Percentage(42),
            ])
            .split(rows[1]);
        draw_projects(frame, app, columns[0]);
        draw_workspaces(frame, app, columns[1]);
        draw_sessions(frame, app, columns[2]);
    } else {
        match app.focus {
            Focus::Projects => draw_projects(frame, app, rows[1]),
            Focus::Workspaces => draw_workspaces(frame, app, rows[1]),
            Focus::Sessions => draw_sessions(frame, app, rows[1]),
        }
    }

    frame.render_widget(
        Paragraph::new(vec![
            Line::styled(app.status.clone(), Style::default().fg(Color::Yellow)),
            Line::styled(
                "Tab/←/→ focus  ↑/↓ select  Enter open  p project  w workspace  o session  a archive  q quit",
                dim(),
            ),
        ]),
        rows[2],
    );
    draw_overlay(frame, app);
}

fn draw_projects(frame: &mut Frame, app: &Dashboard, area: Rect) {
    let items = app
        .projects
        .iter()
        .map(|project| {
            ListItem::new(vec![
                Line::styled(
                    project.name.clone(),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Line::styled(
                    format!("{} · {}", project.base_ref, project.repo_path.display()),
                    dim(),
                ),
            ])
        })
        .collect();
    draw_list(
        frame,
        area,
        items,
        " projects ",
        app.focus == Focus::Projects,
        app.project_index,
    );
}

fn draw_workspaces(frame: &mut Frame, app: &Dashboard, area: Rect) {
    let items = app
        .workspaces
        .iter()
        .map(|workspace| {
            let state_style = match workspace.state {
                WorkspaceState::Ready => Style::default().fg(Color::Green),
                WorkspaceState::Error => Style::default().fg(Color::Red),
                WorkspaceState::Creating => Style::default().fg(Color::Yellow),
                WorkspaceState::Archived => dim(),
            };
            ListItem::new(vec![
                Line::from(vec![
                    Span::styled(
                        workspace.name.clone(),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Span::raw("  "),
                    Span::styled(workspace.state.as_str(), state_style),
                ]),
                Line::styled(
                    format!("{} · {}", workspace.branch, workspace.path.display()),
                    dim(),
                ),
            ])
        })
        .collect();
    draw_list(
        frame,
        area,
        items,
        " workspaces ",
        app.focus == Focus::Workspaces,
        app.workspace_index,
    );
}

fn draw_sessions(frame: &mut Frame, app: &Dashboard, area: Rect) {
    let items = app
        .sessions
        .iter()
        .map(|session| {
            ListItem::new(vec![
                Line::from(vec![
                    Span::styled(
                        session.id.clone(),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(format!("  {}", session.created), dim()),
                ]),
                Line::styled(
                    format!(
                        "{}/{} → {}/{}",
                        session.orch_harness,
                        session
                            .orch_model
                            .as_deref()
                            .unwrap_or("configured default"),
                        session.worker_harness,
                        session
                            .worker_model
                            .as_deref()
                            .unwrap_or("configured default")
                    ),
                    Style::default().fg(Color::Cyan),
                ),
                Line::styled(preview(&session.task), dim()),
            ])
        })
        .collect();
    draw_list(
        frame,
        area,
        items,
        " sessions ",
        app.focus == Focus::Sessions,
        app.session_index,
    );
}

fn draw_list(
    frame: &mut Frame,
    area: Rect,
    mut items: Vec<ListItem<'static>>,
    title: &str,
    active: bool,
    selected: usize,
) {
    if items.is_empty() {
        items.push(ListItem::new(Line::styled("  none", dim())));
    }
    let border = if active {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default()
    };
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(border)
                .title(title),
        )
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("› ");
    let mut state = ListState::default().with_selected(Some(selected));
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_overlay(frame: &mut Frame, app: &Dashboard) {
    match &app.overlay {
        Overlay::None => {}
        Overlay::Help => {
            let area = centered(frame.area(), 72, 18);
            frame.render_widget(Clear, area);
            frame.render_widget(
                Paragraph::new(vec![
                    Line::styled("Navigation", heading()),
                    Line::raw("  Tab or ←/→   change pane"),
                    Line::raw("  ↑/↓          change selection"),
                    Line::raw("  Enter        open workspace or resume session"),
                    Line::default(),
                    Line::styled("Actions", heading()),
                    Line::raw("  p  register project       w  create workspace"),
                    Line::raw("  o  configure new session  a  safely archive workspace"),
                    Line::raw("  r  refresh                q  quit"),
                    Line::default(),
                    Line::styled("Esc, Enter, or ? closes this help.", dim()),
                ])
                .block(Block::default().borders(Borders::ALL).title(" help "))
                .wrap(Wrap { trim: false }),
                area,
            );
        }
        Overlay::AddProject(form) => {
            let area = centered(frame.area(), 76, 11);
            frame.render_widget(Clear, area);
            frame.render_widget(
                Paragraph::new(vec![
                    form_line("name", &form.name, form.field == 0),
                    form_line("repository path", &form.path, form.field == 1),
                    form_line("base ref (optional)", &form.base, form.field == 2),
                    Line::default(),
                    Line::styled(
                        "Tab/↑/↓ fields · Enter advances/submits · Esc cancels",
                        dim(),
                    ),
                ])
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(" register project "),
                ),
                area,
            );
        }
        Overlay::CreateWorkspace(input) => {
            let area = centered(frame.area(), 64, 7);
            frame.render_widget(Clear, area);
            frame.render_widget(
                Paragraph::new(vec![
                    form_line("workspace name", &input.value, true),
                    Line::default(),
                    Line::styled("Enter creates from the project base · Esc cancels", dim()),
                ])
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(" create workspace "),
                ),
                area,
            );
        }
        Overlay::NewSession(form) => {
            let area = centered(frame.area(), 76, 12);
            frame.render_widget(Clear, area);
            frame.render_widget(
                Paragraph::new(vec![
                    choice_line(
                        "orchestrator harness",
                        &form.orch_harness.to_string(),
                        form.field == 0,
                    ),
                    form_line("orchestrator model", &form.orch_model, form.field == 1),
                    choice_line(
                        "worker harness",
                        &form.worker_harness.to_string(),
                        form.field == 2,
                    ),
                    form_line("worker model", &form.worker_model, form.field == 3),
                    Line::default(),
                    Line::styled(
                        "Tab/↑/↓ fields · ←/→ toggles harness · Enter launches · Esc cancels",
                        dim(),
                    ),
                ])
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(" new coding session "),
                ),
                area,
            );
        }
        Overlay::ConfirmArchive => {
            let area = centered(frame.area(), 70, 7);
            frame.render_widget(Clear, area);
            let target = match (app.project(), app.workspace()) {
                (Some(project), Some(workspace)) => {
                    format!("Archive {}/{}?", project.name, workspace.name)
                }
                _ => "Archive selected workspace?".to_string(),
            };
            frame.render_widget(
                Paragraph::new(vec![
                    Line::styled(target, heading()),
                    Line::raw("The branch is retained. Dirty or unmerged workspaces are refused."),
                    Line::default(),
                    Line::styled("y archive · n/Esc cancel", dim()),
                ])
                .block(Block::default().borders(Borders::ALL).title(" confirm ")),
                area,
            );
        }
    }
}

fn form_line(label: &str, value: &str, active: bool) -> Line<'static> {
    let style = if active {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default()
    };
    Line::from(vec![
        Span::styled(format!("{label:24}"), style.add_modifier(Modifier::BOLD)),
        Span::styled(value.to_string(), style),
        Span::styled(if active { "_" } else { "" }, dim()),
    ])
}

fn choice_line(label: &str, value: &str, active: bool) -> Line<'static> {
    form_line(label, &format!("‹ {value} ›"), active)
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width.saturating_sub(2));
    let height = height.min(area.height.saturating_sub(2));
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

fn selected_index<T>(items: &[T], id: Option<i64>, key: impl Fn(&T) -> i64) -> usize {
    items
        .iter()
        .position(|item| Some(key(item)) == id)
        .unwrap_or(0)
        .min(items.len().saturating_sub(1))
}

fn moved(current: usize, len: usize, delta: isize) -> usize {
    if len == 0 {
        return 0;
    }
    current
        .saturating_add_signed(delta)
        .min(len.saturating_sub(1))
}

fn ready(workspace: &Workspace) -> bool {
    workspace.state == WorkspaceState::Ready
}

fn nonempty(value: String) -> Option<String> {
    let value = value.trim().to_string();
    (!value.is_empty()).then_some(value)
}

fn preview(value: &str) -> String {
    const LIMIT: usize = 90;
    let value = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if value.chars().count() <= LIMIT {
        value
    } else {
        format!("{}…", value.chars().take(LIMIT - 1).collect::<String>())
    }
}

fn heading() -> Style {
    Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD)
}

fn dim() -> Style {
    Style::default().fg(Color::DarkGray)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_movement_stays_in_bounds() {
        assert_eq!(moved(0, 0, 1), 0);
        assert_eq!(moved(0, 3, -1), 0);
        assert_eq!(moved(2, 3, 1), 2);
        assert_eq!(moved(1, 3, -1), 0);
    }

    #[test]
    fn session_defaults_keep_harnesses_independent() {
        let mut form = SessionForm::default();
        form.on_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(form.orch_harness, OrchestratorKind::Claude);
        assert_eq!(form.worker_harness, WorkerHarnessKind::Codex);
        assert!(form.orch_model.is_empty());
    }
}
