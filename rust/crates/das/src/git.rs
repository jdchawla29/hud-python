use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Output};

use anyhow::{bail, Context, Result};

pub fn repository_root(path: &Path) -> Result<PathBuf> {
    let root = run_git(path, &["rev-parse", "--show-toplevel"])?;
    fs::canonicalize(root.trim()).with_context(|| format!("failed to resolve repository at {root}"))
}

pub fn default_base(repo: &Path) -> Result<String> {
    if let Ok(reference) = run_git(
        repo,
        &[
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ],
    ) {
        return Ok(reference.trim().to_owned());
    }
    Ok(
        run_git(repo, &["symbolic-ref", "--quiet", "--short", "HEAD"])?
            .trim()
            .to_owned(),
    )
}

pub fn ensure_commit(repo: &Path, reference: &str) -> Result<()> {
    run_git(
        repo,
        &["rev-parse", "--verify", &format!("{reference}^{{commit}}")],
    )?;
    Ok(())
}

pub fn create_worktree(repo: &Path, path: &Path, branch: &str, base: &str) -> Result<()> {
    let parent = path.parent().context("workspace path has no parent")?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    if path.exists() {
        bail!("workspace path already exists: {}", path.display());
    }

    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["worktree", "add", "-b", branch])
        .arg(path)
        .arg(base)
        .output()
        .context("failed to execute git worktree add")?;
    require_success(output, "git worktree add")?;
    Ok(())
}

pub fn archive_worktree(
    repo: &Path,
    path: &Path,
    branch: &str,
    base: &str,
    force: bool,
) -> Result<()> {
    if !path.exists() {
        bail!("workspace path does not exist: {}", path.display());
    }

    if !force {
        let dirty = run_git(path, &["status", "--porcelain"])?;
        if !dirty.is_empty() {
            bail!("workspace has uncommitted changes; commit them or pass --force");
        }

        let status = git_status(repo, &["merge-base", "--is-ancestor", branch, base])?;
        if !status.success() {
            if status.code() == Some(1) {
                bail!(
                    "branch '{branch}' is not merged into '{base}'; merge it or pass --force (the branch will be retained)"
                );
            }
            bail!("git merge-base failed with {status}");
        }
    }

    let mut command = Command::new("git");
    command.arg("-C").arg(repo).args(["worktree", "remove"]);
    if force {
        command.arg("--force");
    }
    let output = command
        .arg(path)
        .output()
        .context("failed to execute git worktree remove")?;
    require_success(output, "git worktree remove")?;
    Ok(())
}

fn run_git(cwd: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .with_context(|| format!("failed to execute git {}", args.join(" ")))?;
    let output = require_success(output, &format!("git {}", args.join(" ")))?;
    String::from_utf8(output.stdout).context("git output was not UTF-8")
}

fn git_status(cwd: &Path, args: &[&str]) -> Result<ExitStatus> {
    Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .status()
        .with_context(|| format!("failed to execute git {}", args.join(" ")))
}

fn require_success(output: Output, operation: &str) -> Result<Output> {
    if output.status.success() {
        return Ok(output);
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    bail!("{operation} failed: {stderr}")
}
