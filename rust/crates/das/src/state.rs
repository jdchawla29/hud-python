use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};

use crate::model::{Project, Workspace, WorkspaceState};

pub struct State {
    connection: Connection,
}

pub struct NewProject<'a> {
    pub name: &'a str,
    pub repo_path: &'a Path,
    pub base_ref: &'a str,
    pub worktree_root: &'a Path,
}

pub struct NewWorkspace<'a> {
    pub project_id: i64,
    pub name: &'a str,
    pub path: &'a Path,
    pub branch: &'a str,
    pub base_ref: &'a str,
}

impl State {
    pub fn open(path: PathBuf) -> Result<Self> {
        let parent = path.parent().context("state database path has no parent")?;
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
        let connection = Connection::open(&path)
            .with_context(|| format!("failed to open {}", path.display()))?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        connection.execute_batch(
            "PRAGMA foreign_keys = ON;
             PRAGMA journal_mode = WAL;
             CREATE TABLE IF NOT EXISTS projects (
                 id INTEGER PRIMARY KEY,
                 name TEXT NOT NULL UNIQUE,
                 repo_path TEXT NOT NULL UNIQUE,
                 base_ref TEXT NOT NULL,
                 worktree_root TEXT NOT NULL UNIQUE,
                 created_at INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS workspaces (
                 id INTEGER PRIMARY KEY,
                 project_id INTEGER NOT NULL REFERENCES projects(id),
                 name TEXT NOT NULL,
                 path TEXT NOT NULL UNIQUE,
                 branch TEXT NOT NULL,
                 base_ref TEXT NOT NULL,
                 state TEXT NOT NULL CHECK (state IN ('creating', 'ready', 'error', 'archived')),
                 error TEXT,
                 created_at INTEGER NOT NULL,
                 updated_at INTEGER NOT NULL,
                 UNIQUE(project_id, name),
                 UNIQUE(project_id, branch)
             );",
        )?;
        Ok(Self { connection })
    }

    pub fn add_project(&self, project: NewProject<'_>) -> Result<Project> {
        self.connection
            .execute(
                "INSERT INTO projects (name, repo_path, base_ref, worktree_root, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    project.name,
                    path_text(project.repo_path),
                    project.base_ref,
                    path_text(project.worktree_root),
                    now(),
                ],
            )
            .with_context(|| format!("failed to register project '{}'", project.name))?;
        self.project(project.name)
    }

    pub fn projects(&self) -> Result<Vec<Project>> {
        let mut statement = self.connection.prepare(
            "SELECT id, name, repo_path, base_ref, worktree_root
             FROM projects ORDER BY name",
        )?;
        let projects = statement
            .query_map([], project_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(projects)
    }

    pub fn project(&self, name: &str) -> Result<Project> {
        self.connection
            .query_row(
                "SELECT id, name, repo_path, base_ref, worktree_root
                 FROM projects WHERE name = ?1",
                [name],
                project_from_row,
            )
            .optional()?
            .with_context(|| format!("unknown project '{name}'"))
    }

    pub fn reserve_workspace(&self, workspace: NewWorkspace<'_>) -> Result<Workspace> {
        let timestamp = now();
        self.connection
            .execute(
                "INSERT INTO workspaces
                 (project_id, name, path, branch, base_ref, state, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'creating', ?6, ?6)",
                params![
                    workspace.project_id,
                    workspace.name,
                    path_text(workspace.path),
                    workspace.branch,
                    workspace.base_ref,
                    timestamp,
                ],
            )
            .with_context(|| format!("failed to reserve workspace '{}'", workspace.name))?;
        self.workspace_by_id(self.connection.last_insert_rowid())
    }

    pub fn set_workspace_state(
        &self,
        id: i64,
        state: WorkspaceState,
        error: Option<&str>,
    ) -> Result<()> {
        self.connection.execute(
            "UPDATE workspaces SET state = ?1, error = ?2, updated_at = ?3 WHERE id = ?4",
            params![state.as_str(), error, now(), id],
        )?;
        Ok(())
    }

    pub fn workspace(&self, project_id: i64, name: &str) -> Result<Workspace> {
        self.connection
            .query_row(
                "SELECT id, project_id, name, path, branch, base_ref, state, error
                 FROM workspaces WHERE project_id = ?1 AND name = ?2",
                params![project_id, name],
                workspace_from_row,
            )
            .optional()?
            .with_context(|| format!("unknown workspace '{name}'"))
    }

    pub fn workspaces(&self, project_id: Option<i64>) -> Result<Vec<(String, Workspace)>> {
        let query =
            "SELECT p.name, w.id, w.project_id, w.name, w.path, w.branch, w.base_ref, w.state, w.error
             FROM workspaces w JOIN projects p ON p.id = w.project_id
             WHERE (?1 IS NULL OR w.project_id = ?1)
             ORDER BY p.name, w.created_at DESC";
        let mut statement = self.connection.prepare(query)?;
        let workspaces = statement
            .query_map([project_id], |row| {
                Ok((
                    row.get(0)?,
                    Workspace {
                        id: row.get(1)?,
                        name: row.get(3)?,
                        path: PathBuf::from(row.get::<_, String>(4)?),
                        branch: row.get(5)?,
                        base_ref: row.get(6)?,
                        state: parse_state(row, 7)?,
                        error: row.get(8)?,
                    },
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(workspaces)
    }

    fn workspace_by_id(&self, id: i64) -> Result<Workspace> {
        Ok(self.connection.query_row(
            "SELECT id, project_id, name, path, branch, base_ref, state, error
             FROM workspaces WHERE id = ?1",
            [id],
            workspace_from_row,
        )?)
    }
}

fn project_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Project> {
    Ok(Project {
        id: row.get(0)?,
        name: row.get(1)?,
        repo_path: PathBuf::from(row.get::<_, String>(2)?),
        base_ref: row.get(3)?,
        worktree_root: PathBuf::from(row.get::<_, String>(4)?),
    })
}

fn workspace_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Workspace> {
    Ok(Workspace {
        id: row.get(0)?,
        name: row.get(2)?,
        path: PathBuf::from(row.get::<_, String>(3)?),
        branch: row.get(4)?,
        base_ref: row.get(5)?,
        state: parse_state(row, 6)?,
        error: row.get(7)?,
    })
}

fn parse_state(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<WorkspaceState> {
    WorkspaceState::parse(&row.get::<_, String>(index)?).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(index, rusqlite::types::Type::Text, error.into())
    })
}

fn path_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time is before the Unix epoch")
        .as_secs() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stores_projects_and_workspaces() {
        let root = std::env::temp_dir().join(format!("das-state-test-{}", std::process::id()));
        let database = root.join("state.sqlite3");
        let _ = fs::remove_dir_all(&root);
        let state = State::open(database).unwrap();
        let project = state
            .add_project(NewProject {
                name: "demo",
                repo_path: Path::new("/tmp/demo"),
                base_ref: "main",
                worktree_root: Path::new("/tmp/demo-worktrees"),
            })
            .unwrap();
        let workspace = state
            .reserve_workspace(NewWorkspace {
                project_id: project.id,
                name: "feature",
                path: Path::new("/tmp/demo-worktrees/feature"),
                branch: "codex/feature",
                base_ref: "main",
            })
            .unwrap();
        state
            .set_workspace_state(workspace.id, WorkspaceState::Ready, None)
            .unwrap();

        assert_eq!(state.projects().unwrap().len(), 1);
        let stored = state.workspace(project.id, "feature").unwrap();
        assert_eq!(stored.state, WorkspaceState::Ready);
        let _ = fs::remove_dir_all(root);
    }
}
