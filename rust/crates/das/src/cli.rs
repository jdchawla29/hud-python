use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};

use crate::git;
use crate::harness::WorkerHarnessKind;
use crate::model::{validate_name, WorkspaceState};
use crate::orchestrate::OpenOptions;
use crate::orchestrator::OrchestratorKind;
use crate::session::SessionStore;
use crate::state::{NewProject, NewWorkspace, State};

#[derive(Parser)]
#[command(
    name = "das",
    version,
    about = "Project, worktree, and coding-agent manager"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Manage registered projects.
    Project(ProjectArgs),
    /// Manage Git worktrees.
    Workspace(WorkspaceArgs),
    /// Open the interactive coding-agent TUI in a managed worktree.
    Open(OpenArgs),
    /// List coding sessions associated with a worktree.
    Sessions { project: String, workspace: String },
}

#[derive(Args)]
struct ProjectArgs {
    #[command(subcommand)]
    command: ProjectCommand,
}

#[derive(Subcommand)]
enum ProjectCommand {
    /// Register an existing Git repository.
    Add {
        name: String,
        path: PathBuf,
        #[arg(long)]
        base: Option<String>,
        #[arg(long)]
        worktree_root: Option<PathBuf>,
    },
    /// List registered projects.
    List,
}

#[derive(Args)]
struct WorkspaceArgs {
    #[command(subcommand)]
    command: WorkspaceCommand,
}

#[derive(Subcommand)]
enum WorkspaceCommand {
    /// Create a branch and managed Git worktree.
    Create {
        project: String,
        name: String,
        #[arg(long)]
        from: Option<String>,
        #[arg(long)]
        branch: Option<String>,
    },
    /// List managed worktrees.
    List { project: Option<String> },
    /// Remove a worktree while retaining its branch.
    Archive {
        project: String,
        workspace: String,
        /// Allow removal with uncommitted changes or an unmerged branch.
        #[arg(long)]
        force: bool,
    },
}

#[derive(Args)]
struct OpenArgs {
    project: String,
    workspace: String,
    /// The coding task; omit it to type one in the TUI.
    #[arg(long)]
    task: Option<String>,
    /// Resume a saved session belonging to this worktree.
    #[arg(long)]
    resume: Option<String>,
    /// Harness used by the orchestrator.
    #[arg(long, value_enum)]
    orch_harness: Option<OrchestratorKind>,
    /// Orchestrator model; omit it to use the selected harness default.
    #[arg(long)]
    orch_model: Option<String>,
    /// CLI harness used by persistent worker threads.
    #[arg(long, value_enum)]
    worker_harness: Option<WorkerHarnessKind>,
    /// Worker model; omit it to use the selected CLI's configured default.
    #[arg(long)]
    worker_model: Option<String>,
    /// Codex app-server daemon socket; omit it to use the managed default.
    #[arg(long)]
    codex_socket: Option<PathBuf>,
    /// Maximum orchestrator turns per message.
    #[arg(long, default_value_t = 40)]
    max_turns: u32,
    /// Path to a hud-python checkout (default: $HUD_PYTHON_DIR, else this repo).
    #[arg(long)]
    hud_python: Option<PathBuf>,
    /// Do not persist this session under DAS_HOME.
    #[arg(long)]
    no_save: bool,
    /// Run without the TUI.
    #[arg(long)]
    headless: bool,
    /// Follow-up message for headless mode; repeatable.
    #[arg(long)]
    message: Vec<String>,
}

pub async fn run() -> Result<()> {
    let command = Cli::parse().command;
    let home = crate::paths::home()?;
    let state = State::open(home.join("state.sqlite3"))?;
    match command {
        Some(Command::Project(args)) => project_command(&state, &home, args.command),
        Some(Command::Workspace(args)) => workspace_command(&state, args.command),
        Some(Command::Open(args)) => open(&state, args).await,
        Some(Command::Sessions { project, workspace }) => sessions(&state, &project, &workspace),
        None => crate::dashboard::run(&state, &home).await,
    }
}

fn project_command(state: &State, home: &Path, command: ProjectCommand) -> Result<()> {
    match command {
        ProjectCommand::Add {
            name,
            path,
            base,
            worktree_root,
        } => {
            let project = register_project(state, home, &name, &path, base, worktree_root)?;
            println!(
                "registered {} ({})\nbase: {}\nworktrees: {}",
                project.name,
                project.repo_path.display(),
                project.base_ref,
                project.worktree_root.display()
            );
            Ok(())
        }
        ProjectCommand::List => {
            let projects = state.projects()?;
            if projects.is_empty() {
                println!("no projects");
            }
            for project in projects {
                println!(
                    "{}\t{}\t{}\t{}",
                    project.name,
                    project.base_ref,
                    project.repo_path.display(),
                    project.worktree_root.display()
                );
            }
            Ok(())
        }
    }
}

fn workspace_command(state: &State, command: WorkspaceCommand) -> Result<()> {
    match command {
        WorkspaceCommand::Create {
            project,
            name,
            from,
            branch,
        } => {
            let (project, workspace) = provision_workspace(state, &project, &name, from, branch)?;
            println!(
                "created {}/{}\npath: {}\nbranch: {}\nopen: das open {} {}",
                project.name,
                workspace.name,
                workspace.path.display(),
                workspace.branch,
                project.name,
                workspace.name
            );
            Ok(())
        }
        WorkspaceCommand::List { project } => {
            let project_id = project
                .as_deref()
                .map(|name| state.project(name).map(|project| project.id))
                .transpose()?;
            let workspaces = state.workspaces(project_id)?;
            if workspaces.is_empty() {
                println!("no workspaces");
            }
            for (project, workspace) in workspaces {
                let detail = workspace
                    .error
                    .as_deref()
                    .map(|error| format!("\t{error}"))
                    .unwrap_or_default();
                println!(
                    "{project}/{}\t{}\t{}\t{}{}",
                    workspace.name,
                    workspace.state.as_str(),
                    workspace.branch,
                    workspace.path.display(),
                    detail
                );
            }
            Ok(())
        }
        WorkspaceCommand::Archive {
            project,
            workspace,
            force,
        } => {
            let (project, workspace) = lookup_workspace(state, &project, &workspace)?;
            require_ready(&workspace)?;
            archive_workspace(state, &project, &workspace, force)?;
            println!(
                "archived {}/{}; branch '{}' was retained",
                project.name, workspace.name, workspace.branch
            );
            Ok(())
        }
    }
}

async fn open(state: &State, args: OpenArgs) -> Result<()> {
    if args.resume.is_some()
        && (args.task.is_some()
            || args.orch_model.is_some()
            || args.orch_harness.is_some()
            || args.worker_harness.is_some()
            || args.worker_model.is_some()
            || args.codex_socket.is_some())
    {
        bail!("task, model, and harness options cannot override a resumed session");
    }
    let (project, workspace) = lookup_workspace(state, &args.project, &args.workspace)?;
    require_ready(&workspace)?;
    crate::orchestrate::open(OpenOptions {
        project,
        workspace,
        task: args.task,
        orch_harness: args.orch_harness.unwrap_or_default(),
        orch_model: args.orch_model,
        worker_harness: args.worker_harness.unwrap_or(WorkerHarnessKind::Codex),
        worker_model: args.worker_model,
        codex_socket: args.codex_socket,
        max_turns: args.max_turns,
        hud_python: args.hud_python,
        resume: args.resume,
        no_save: args.no_save,
        headless: args.headless,
        messages: args.message,
    })
    .await
}

fn sessions(state: &State, project: &str, workspace: &str) -> Result<()> {
    let (_, workspace) = lookup_workspace(state, project, workspace)?;
    require_ready(&workspace)?;
    let sessions = SessionStore::list_for_workspace(workspace.id)?;
    if sessions.is_empty() {
        println!("no sessions for {}", workspace.path.display());
    }
    for session in sessions {
        let socket = session
            .codex_socket
            .as_ref()
            .map(|path| format!(" @ {}", path.display()))
            .unwrap_or_default();
        println!(
            "{}\t{}\t{}/{} | {}/{}{}\t{}",
            session.id,
            session.created,
            session.orch_harness,
            session
                .orch_model
                .as_deref()
                .unwrap_or("configured default"),
            session.worker_harness,
            session
                .worker_model
                .as_deref()
                .unwrap_or("configured default"),
            socket,
            single_line(&session.task)
        );
    }
    Ok(())
}

pub(crate) fn register_project(
    state: &State,
    home: &Path,
    name: &str,
    path: &Path,
    base: Option<String>,
    worktree_root: Option<PathBuf>,
) -> Result<crate::model::Project> {
    validate_name("project", name)?;
    let repo_path = git::repository_root(path)?;
    let base_ref = base.map_or_else(|| git::default_base(&repo_path), Ok)?;
    git::ensure_commit(&repo_path, &base_ref)?;
    let worktree_root = worktree_root.unwrap_or_else(|| home.join("worktrees").join(name));
    fs::create_dir_all(&worktree_root)
        .with_context(|| format!("failed to create worktree root {}", worktree_root.display()))?;
    let worktree_root = fs::canonicalize(&worktree_root).with_context(|| {
        format!(
            "failed to resolve worktree root {}",
            worktree_root.display()
        )
    })?;
    if worktree_root.starts_with(&repo_path) {
        bail!(
            "worktree root {} must be outside the source repository {}",
            worktree_root.display(),
            repo_path.display()
        );
    }
    state.add_project(NewProject {
        name,
        repo_path: &repo_path,
        base_ref: &base_ref,
        worktree_root: &worktree_root,
    })
}

pub(crate) fn provision_workspace(
    state: &State,
    project_name: &str,
    name: &str,
    from: Option<String>,
    branch: Option<String>,
) -> Result<(crate::model::Project, crate::model::Workspace)> {
    validate_name("workspace", name)?;
    let project = state.project(project_name)?;
    let base_ref = from.unwrap_or_else(|| project.base_ref.clone());
    let branch = branch.unwrap_or_else(|| format!("das/{name}"));
    git::ensure_commit(&project.repo_path, &base_ref)?;
    let path = project.worktree_root.join(name);
    let workspace = state.reserve_workspace(NewWorkspace {
        project_id: project.id,
        name,
        path: &path,
        branch: &branch,
        base_ref: &base_ref,
    })?;

    match git::create_worktree(&project.repo_path, &path, &branch, &base_ref) {
        Ok(()) => {
            state.set_workspace_state(workspace.id, WorkspaceState::Ready, None)?;
            Ok((project.clone(), state.workspace(project.id, name)?))
        }
        Err(error) => {
            let message = format!("{error:#}");
            state.set_workspace_state(workspace.id, WorkspaceState::Error, Some(&message))?;
            Err(error).context("workspace record was retained in the error state")
        }
    }
}

pub(crate) fn archive_workspace(
    state: &State,
    project: &crate::model::Project,
    workspace: &crate::model::Workspace,
    force: bool,
) -> Result<()> {
    require_ready(workspace)?;
    git::archive_worktree(
        &project.repo_path,
        &workspace.path,
        &workspace.branch,
        &workspace.base_ref,
        force,
    )?;
    state.set_workspace_state(workspace.id, WorkspaceState::Archived, None)
}

fn lookup_workspace(
    state: &State,
    project_name: &str,
    workspace_name: &str,
) -> Result<(crate::model::Project, crate::model::Workspace)> {
    let project = state.project(project_name)?;
    let workspace = state.workspace(project.id, workspace_name)?;
    Ok((project, workspace))
}

fn require_ready(workspace: &crate::model::Workspace) -> Result<()> {
    if workspace.state != WorkspaceState::Ready {
        bail!(
            "workspace '{}' is {}, not ready",
            workspace.name,
            workspace.state.as_str()
        );
    }
    Ok(())
}

fn single_line(value: &str) -> String {
    const LIMIT: usize = 80;
    let value = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if value.chars().count() <= LIMIT {
        value
    } else {
        format!("{}…", value.chars().take(LIMIT - 1).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_is_single_line_and_bounded() {
        assert_eq!(single_line("one\n  two"), "one two");
        assert_eq!(single_line(&"x".repeat(100)).chars().count(), 80);
    }

    #[test]
    fn parses_orchestrator_and_worker_selection_independently() {
        let cli = Cli::try_parse_from([
            "das",
            "open",
            "hud",
            "feature",
            "--orch-harness",
            "claude",
            "--orch-model",
            "opus",
            "--worker-harness",
            "codex",
            "--worker-model",
            "gpt-5.6",
        ])
        .unwrap();
        let Some(Command::Open(args)) = cli.command else {
            panic!("expected open command");
        };

        assert_eq!(args.orch_harness, Some(OrchestratorKind::Claude));
        assert_eq!(args.orch_model.as_deref(), Some("opus"));
        assert_eq!(args.worker_harness, Some(WorkerHarnessKind::Codex));
        assert_eq!(args.worker_model.as_deref(), Some("gpt-5.6"));
    }
}
