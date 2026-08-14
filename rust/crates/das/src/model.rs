use std::path::PathBuf;

use anyhow::{bail, Result};

#[derive(Clone, Debug)]
pub struct Project {
    pub id: i64,
    pub name: String,
    pub repo_path: PathBuf,
    pub base_ref: String,
    pub worktree_root: PathBuf,
}

#[derive(Clone, Debug)]
pub struct Workspace {
    pub id: i64,
    pub name: String,
    pub path: PathBuf,
    pub branch: String,
    pub base_ref: String,
    pub state: WorkspaceState,
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkspaceState {
    Creating,
    Ready,
    Error,
    Archived,
}

impl WorkspaceState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Creating => "creating",
            Self::Ready => "ready",
            Self::Error => "error",
            Self::Archived => "archived",
        }
    }

    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "creating" => Ok(Self::Creating),
            "ready" => Ok(Self::Ready),
            "error" => Ok(Self::Error),
            "archived" => Ok(Self::Archived),
            _ => bail!("invalid workspace state in database: {value}"),
        }
    }
}

pub fn validate_name(kind: &str, value: &str) -> Result<()> {
    let valid = !value.is_empty()
        && value.len() <= 80
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "-_.".contains(character))
        && value
            .chars()
            .next()
            .is_some_and(|character| character.is_ascii_alphanumeric());

    if !valid {
        bail!(
            "{kind} name must start with an ASCII letter or digit and contain only letters, digits, '-', '_', or '.'"
        );
    }
    Ok(())
}
