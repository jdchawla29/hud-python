//! Runtime providers: server placement for the rollout engine.
//!
//! A [`Provider`] brings up one fresh env substrate for a task row and returns
//! its connectable [`Runtime`] inside a [`RuntimeGuard`] that owns teardown —
//! the Rust shape of the Python SDK's async-context-manager providers.

use async_trait::async_trait;
use futures::future::BoxFuture;
use hud_types::TaskRow;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::Mutex;

/// Line a serving process prints once its control channel is bound.
pub const PORT_ANNOUNCEMENT: &str = "HUD_SERVE_PORT=";

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("{0}")]
    Provision(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Requested GPU resources, provider-neutral where possible.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeGpu {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub r#type: Option<String>,
    #[serde(default = "one")]
    pub count: u32,
}

fn one() -> u32 {
    1
}

/// Requested compute resources for a runtime.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeResources {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_mb: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu: Option<RuntimeGpu>,
}

/// Runtime lifecycle limits in seconds.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeLimits {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub startup_timeout_s: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_timeout_s: Option<u64>,
}

/// Portable task-environment launch requirements. `TaskRow::runtime_config`
/// is requested construction input; `Runtime::config` is the effective config
/// used to construct a runtime.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<RuntimeResources>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<RuntimeLimits>,
}

impl RuntimeConfig {
    /// Merge with row-level overrides (set fields in `override_` win).
    pub fn with_overrides(&self, override_: Option<&RuntimeConfig>) -> RuntimeConfig {
        let Some(other) = override_ else {
            return self.clone();
        };
        RuntimeConfig {
            image: other.image.clone().or_else(|| self.image.clone()),
            resources: other.resources.clone().or_else(|| self.resources.clone()),
            limits: other.limits.clone().or_else(|| self.limits.clone()),
        }
    }

    /// Parse a task row's raw `runtime_config` JSON.
    pub fn from_row(task: &TaskRow) -> Result<Option<RuntimeConfig>, ProviderError> {
        match &task.runtime_config {
            None => Ok(None),
            Some(raw) => serde_json::from_value(raw.clone())
                .map(Some)
                .map_err(|e| ProviderError::Provision(format!("invalid task runtime_config: {e}"))),
        }
    }
}

/// The connectable address of a provisioned substrate.
///
/// `url` is the control-channel address (`tcp://127.0.0.1:7000` for a local
/// process). `params` carries connection-time data a transport may need (e.g.
/// `ready_timeout`). Constructed directly, it is also a provider — the
/// borrowed, shared case: it yields itself with a no-op lifecycle, since
/// whoever provisioned the substrate owns its teardown.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Runtime {
    pub url: String,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub params: Map<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<RuntimeConfig>,
}

impl Runtime {
    pub fn new(url: impl Into<String>) -> Runtime {
        Runtime {
            url: url.into(),
            params: Map::new(),
            config: None,
        }
    }
}

/// A provisioned substrate plus its teardown.
///
/// Call [`RuntimeGuard::close`] on the normal path; `Drop` is only a safety
/// net that spawns the teardown best-effort (a cancelled rollout must not
/// leak containers or child processes).
pub struct RuntimeGuard {
    pub runtime: Runtime,
    teardown: Option<BoxFuture<'static, ()>>,
}

impl RuntimeGuard {
    /// A substrate someone else owns: no-op teardown.
    pub fn borrowed(runtime: Runtime) -> RuntimeGuard {
        RuntimeGuard {
            runtime,
            teardown: None,
        }
    }

    pub fn with_teardown(runtime: Runtime, teardown: BoxFuture<'static, ()>) -> RuntimeGuard {
        RuntimeGuard {
            runtime,
            teardown: Some(teardown),
        }
    }

    /// Tear the substrate down (idempotent).
    pub async fn close(mut self) {
        if let Some(teardown) = self.teardown.take() {
            teardown.await;
        }
    }
}

impl Drop for RuntimeGuard {
    fn drop(&mut self) {
        if let Some(teardown) = self.teardown.take() {
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(teardown);
            }
        }
    }
}

/// Server placement: called with the task row being placed, acquire one fresh
/// env substrate for it and return its connectable [`Runtime`].
#[async_trait]
pub trait Provider: Send + Sync {
    async fn acquire(&self, task: &TaskRow) -> Result<RuntimeGuard, ProviderError>;
}

/// A bare [`Runtime`] is a provider — the borrowed substrate case.
#[async_trait]
impl Provider for Runtime {
    async fn acquire(&self, _task: &TaskRow) -> Result<RuntimeGuard, ProviderError> {
        Ok(RuntimeGuard::borrowed(self.clone()))
    }
}

// ─── LocalRuntime ─────────────────────────────────────────────────────

/// The local provider: serve the placed row's env in a child process.
///
/// Each acquisition spawns the configured command, reads the ephemeral port
/// from the child's stdout (`HUD_SERVE_PORT=<port>`), yields its [`Runtime`],
/// and terminates the child on teardown (SIGTERM, then SIGKILL after a grace
/// period). Works for any serving entry point: a compiled Rust env binary, or
/// the Python SDK's `python -m hud.environment.server <path>`.
#[derive(Debug, Clone)]
pub struct LocalRuntime {
    argv: Vec<String>,
    /// Append `--env <task.env>` to the command (the Python server's flag for
    /// sources defining several environments).
    pass_env_flag: bool,
    cwd: Option<PathBuf>,
    envs: Vec<(String, String)>,
    ready_timeout: Duration,
}

impl LocalRuntime {
    /// Serve with an explicit command, e.g. a compiled env binary.
    pub fn command(argv: impl IntoIterator<Item = impl Into<String>>) -> LocalRuntime {
        LocalRuntime {
            argv: argv.into_iter().map(Into::into).collect(),
            pass_env_flag: false,
            cwd: None,
            envs: Vec::new(),
            ready_timeout: Duration::from_secs(120),
        }
    }

    /// Serve a Python env source via `uv run python -m hud.environment.server`.
    ///
    /// The child's working directory is the source's directory, so sibling
    /// imports and relative data paths resolve; the served env is the placed
    /// task's `env` name.
    pub fn python_source(path: impl Into<PathBuf>) -> LocalRuntime {
        let path: PathBuf = path.into();
        let cwd = if path.is_dir() {
            path.clone()
        } else {
            path.parent()
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."))
        };
        LocalRuntime {
            argv: vec![
                "uv".to_string(),
                "run".to_string(),
                "python".to_string(),
                "-m".to_string(),
                "hud.environment.server".to_string(),
                path.to_string_lossy().into_owned(),
            ],
            pass_env_flag: true,
            cwd: Some(cwd),
            envs: Vec::new(),
            ready_timeout: Duration::from_secs(120),
        }
    }

    pub fn cwd(mut self, cwd: impl Into<PathBuf>) -> LocalRuntime {
        self.cwd = Some(cwd.into());
        self
    }

    /// Set an environment variable on the spawned serving process.
    pub fn env_var(mut self, key: impl Into<String>, value: impl Into<String>) -> LocalRuntime {
        self.envs.push((key.into(), value.into()));
        self
    }

    pub fn ready_timeout(mut self, timeout: Duration) -> LocalRuntime {
        self.ready_timeout = timeout;
        self
    }
}

#[async_trait]
impl Provider for LocalRuntime {
    async fn acquire(&self, task: &TaskRow) -> Result<RuntimeGuard, ProviderError> {
        if task.runtime_config.is_some() {
            return Err(ProviderError::Provision(
                "LocalRuntime does not support task runtime_config".to_string(),
            ));
        }
        let (program, rest) = self
            .argv
            .split_first()
            .ok_or_else(|| ProviderError::Provision("LocalRuntime: empty command".to_string()))?;
        let mut command = tokio::process::Command::new(program);
        command
            .args(rest)
            .stdout(Stdio::piped())
            // Capture stderr (don't inherit it): under concurrent rollouts an
            // inherited fd interleaves every child's output unattributably.
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if self.pass_env_flag {
            command.args(["--env", &task.env]);
        }
        if let Some(cwd) = &self.cwd {
            command.current_dir(cwd);
        }
        for (key, value) in &self.envs {
            command.env(key, value);
        }
        #[cfg(unix)]
        command.process_group(0);

        let mut child = command.spawn()?;
        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");

        // Drain stderr into a bounded tail from the start: it never blocks on
        // a full pipe, and the last lines survive if the child dies early.
        let stderr_tail: Arc<Mutex<VecDeque<String>>> = Arc::new(Mutex::new(VecDeque::new()));
        let capture_tail = Arc::clone(&stderr_tail);
        let capture = tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let mut tail = capture_tail.lock().await;
                if tail.len() >= 50 {
                    tail.pop_front();
                }
                tail.push_back(line);
            }
        });

        let port = tokio::time::timeout(self.ready_timeout, read_port(stdout)).await;
        let port = match port {
            Ok(Some(port)) => port,
            Ok(None) | Err(_) => {
                let detail = match port {
                    Err(_) => format!("no {PORT_ANNOUNCEMENT} within {:?}", self.ready_timeout),
                    _ => "exited before serving".to_string(),
                };
                capture.abort();
                let tail: Vec<String> = stderr_tail.lock().await.iter().cloned().collect();
                terminate(&mut child).await;
                return Err(ProviderError::Provision(format!(
                    "env process {:?} failed to serve ({detail}):\n{}",
                    self.argv.join(" "),
                    tail.join("\n"),
                )));
            }
        };

        let teardown = Box::pin(async move {
            capture.abort();
            let mut child = child;
            terminate(&mut child).await;
        });
        Ok(RuntimeGuard::with_teardown(
            Runtime::new(format!("tcp://127.0.0.1:{port}")),
            teardown,
        ))
    }
}

/// Read the child's stdout until the port announcement; `None` on EOF first.
/// Keeps draining lines that aren't the announcement (env logging to stdout).
async fn read_port(stdout: tokio::process::ChildStdout) -> Option<u16> {
    let mut lines = BufReader::new(stdout).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if let Some(port) = line.trim().strip_prefix(PORT_ANNOUNCEMENT) {
            let port = port.trim().parse().ok()?;
            // Keep the pipe drained so the child never blocks on stdout.
            tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });
            return Some(port);
        }
    }
    None
}

/// SIGTERM the child's process group, wait up to 10s, then SIGKILL.
async fn terminate(child: &mut tokio::process::Child) {
    if child.try_wait().ok().flatten().is_some() {
        return;
    }
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        // The child leads its own process group (`process_group(0)`), so this
        // reaches daemons it spawned too.
        unsafe { libc::killpg(pid as i32, libc::SIGTERM) };
        if tokio::time::timeout(Duration::from_secs(10), child.wait())
            .await
            .is_ok()
        {
            return;
        }
        unsafe { libc::killpg(pid as i32, libc::SIGKILL) };
    }
    let _ = child.kill().await;
    let _ = child.wait().await;
}

// ─── DockerRuntime ────────────────────────────────────────────────────

/// The container provider: each acquisition `docker run`s a fresh image.
///
/// The image's CMD serves the env's control channel on `port` inside the
/// container. Each acquisition publishes that port on an ephemeral loopback
/// port, yields its [`Runtime`], and force-removes the container on teardown.
/// Acquisition returns as soon as the port mapping exists — the env may still
/// be importing behind it; protocol-level readiness is the client's job.
#[derive(Debug, Clone)]
pub struct DockerRuntime {
    port: u16,
    run_args: Vec<String>,
    runtime_config: Option<RuntimeConfig>,
}

impl DockerRuntime {
    pub fn new(image: impl Into<String>) -> DockerRuntime {
        DockerRuntime {
            port: 8765,
            run_args: Vec::new(),
            runtime_config: Some(RuntimeConfig {
                image: Some(image.into()),
                ..Default::default()
            }),
        }
    }

    /// The container-side control-channel port (default 8765).
    pub fn port(mut self, port: u16) -> DockerRuntime {
        self.port = port;
        self
    }

    /// Extra `docker run` flags (`-e`, volumes, ...).
    pub fn run_args(mut self, args: impl IntoIterator<Item = impl Into<String>>) -> DockerRuntime {
        self.run_args = args.into_iter().map(Into::into).collect();
        self
    }
}

#[async_trait]
impl Provider for DockerRuntime {
    async fn acquire(&self, task: &TaskRow) -> Result<RuntimeGuard, ProviderError> {
        let base = self.runtime_config.clone().unwrap_or_default();
        let config = base.with_overrides(RuntimeConfig::from_row(task)?.as_ref());
        let image = config.image.clone().ok_or_else(|| {
            ProviderError::Provision("DockerRuntime requires runtime_config.image".to_string())
        })?;
        if config
            .limits
            .as_ref()
            .is_some_and(|l| *l != RuntimeLimits::default())
        {
            return Err(ProviderError::Provision(
                "DockerRuntime does not support runtime_config limits".to_string(),
            ));
        }

        let mut resource_args: Vec<String> = Vec::new();
        if let Some(resources) = &config.resources {
            if let Some(cpu) = resources.cpu {
                let cpu = if cpu.fract() == 0.0 {
                    format!("{}", cpu as u64)
                } else {
                    format!("{cpu}")
                };
                resource_args.extend(["--cpus".to_string(), cpu]);
            }
            if let Some(memory_mb) = resources.memory_mb {
                resource_args.extend(["--memory".to_string(), format!("{memory_mb}m")]);
            }
            if let Some(gpu) = &resources.gpu {
                if gpu.r#type.is_some() {
                    return Err(ProviderError::Provision(
                        "DockerRuntime cannot select GPUs by type".to_string(),
                    ));
                }
                resource_args.extend(["--gpus".to_string(), gpu.count.to_string()]);
            }
        }

        let mut args: Vec<String> = vec!["run".to_string(), "--detach".to_string()];
        args.extend(self.run_args.iter().cloned());
        args.extend(resource_args);
        args.extend([
            "--publish".to_string(),
            format!("127.0.0.1::{}", self.port),
            image.clone(),
        ]);
        let (out, _) = docker(&args, true).await?;
        let container = out.trim().to_string();

        let mapping = docker(
            &["port".to_string(), container.clone(), self.port.to_string()],
            true,
        )
        .await;
        let host_port = match mapping {
            Ok((mapping, _)) if !mapping.trim().is_empty() => mapping
                .trim()
                .lines()
                .next()
                .and_then(|line| line.rsplit(':').next())
                .and_then(|p| p.parse::<u16>().ok()),
            _ => None,
        };
        let Some(host_port) = host_port else {
            let (logs_out, logs_err) = docker(
                &[
                    "logs".to_string(),
                    "--tail".to_string(),
                    "40".to_string(),
                    container.clone(),
                ],
                false,
            )
            .await
            .unwrap_or_default();
            remove_container(&container).await;
            return Err(ProviderError::Provision(format!(
                "container for image {image:?} exited before serving port {}:\n{}",
                self.port,
                if logs_err.trim().is_empty() {
                    logs_out
                } else {
                    logs_err
                }
                .trim(),
            )));
        };

        let teardown = Box::pin(async move {
            remove_container(&container).await;
        });
        Ok(RuntimeGuard::with_teardown(
            Runtime {
                url: format!("tcp://127.0.0.1:{host_port}"),
                params: Map::new(),
                config: Some(config),
            },
            teardown,
        ))
    }
}

async fn remove_container(container: &str) {
    // check=false: teardown must not shadow the run's own error, and `rm -f`
    // only fails when the daemon itself is broken.
    let _ = docker(
        &[
            "rm".to_string(),
            "--force".to_string(),
            container.to_string(),
        ],
        false,
    )
    .await;
}

async fn docker(args: &[String], check: bool) -> Result<(String, String), ProviderError> {
    let output = tokio::process::Command::new("docker")
        .args(args)
        .output()
        .await?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    if check && !output.status.success() {
        return Err(ProviderError::Provision(format!(
            "docker {} failed: {}",
            args.first().map(String::as_str).unwrap_or(""),
            stderr.trim(),
        )));
    }
    Ok((stdout, stderr))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn config_overrides_merge_set_fields() {
        let base = RuntimeConfig {
            image: Some("base:1".to_string()),
            resources: Some(RuntimeResources {
                cpu: Some(2.0),
                ..Default::default()
            }),
            limits: None,
        };
        let over = RuntimeConfig {
            image: Some("over:2".to_string()),
            ..Default::default()
        };
        let merged = base.with_overrides(Some(&over));
        assert_eq!(merged.image.as_deref(), Some("over:2"));
        assert_eq!(merged.resources.unwrap().cpu, Some(2.0));
    }

    #[test]
    fn runtime_row_config_parses() {
        let mut task = TaskRow::new("env", "t");
        task.runtime_config = Some(json!({"image": "img:1"}));
        let config = RuntimeConfig::from_row(&task).unwrap().unwrap();
        assert_eq!(config.image.as_deref(), Some("img:1"));

        task.runtime_config = Some(json!({"bogus": 1}));
        assert!(RuntimeConfig::from_row(&task).is_err());
    }

    #[tokio::test]
    async fn bare_runtime_is_borrowed_provider() {
        let runtime = Runtime::new("tcp://127.0.0.1:7000");
        let guard = runtime.acquire(&TaskRow::new("env", "t")).await.unwrap();
        assert_eq!(guard.runtime.url, "tcp://127.0.0.1:7000");
        guard.close().await;
    }
}
