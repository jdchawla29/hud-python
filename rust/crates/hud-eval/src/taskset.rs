//! Taskset: a named, ordered collection of concrete tasks, and the scheduler
//! that fans the rollout engine out over them.

use crate::agent::Agent;
use crate::job::Job;
use crate::run::{rollout, RolloutOptions};
use crate::runtime::Provider;
use hud_types::TaskRow;
use indexmap::IndexMap;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;

#[derive(Debug, thiserror::Error)]
pub enum TasksetError {
    #[error("duplicate task slugs: {0}")]
    DuplicateSlugs(String),
    #[error("unsupported taskset source: {0} (use .json or .jsonl)")]
    UnsupportedSource(PathBuf),
    #[error("{path}: {detail}")]
    Malformed { path: PathBuf, detail: String },
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// A named, ordered collection of [`TaskRow`]s, indexed by slug.
pub struct Taskset {
    pub name: String,
    pub origin: Option<String>,
    tasks: IndexMap<String, TaskRow>,
}

impl Taskset {
    pub fn new(
        name: impl Into<String>,
        tasks: impl IntoIterator<Item = TaskRow>,
    ) -> Result<Taskset, TasksetError> {
        let mut indexed = IndexMap::new();
        let mut duplicates = Vec::new();
        for task in tasks {
            let slug = task.effective_slug();
            if indexed.insert(slug.clone(), task).is_some() {
                duplicates.push(slug);
            }
        }
        if !duplicates.is_empty() {
            duplicates.sort_unstable();
            duplicates.dedup();
            return Err(TasksetError::DuplicateSlugs(duplicates.join(", ")));
        }
        Ok(Taskset {
            name: name.into(),
            origin: None,
            tasks: indexed,
        })
    }

    /// Load portable rows from `.json` (object or array) or `.jsonl`.
    pub fn from_file(path: impl AsRef<Path>) -> Result<Taskset, TasksetError> {
        let path = path.as_ref();
        let malformed = |detail: String| TasksetError::Malformed {
            path: path.to_path_buf(),
            detail,
        };
        let text = std::fs::read_to_string(path)?;
        let entries: Vec<Value> = match path.extension().and_then(|e| e.to_str()) {
            Some("jsonl") => text
                .lines()
                .filter(|line| !line.trim().is_empty())
                .map(serde_json::from_str)
                .collect::<Result<_, _>>()
                .map_err(|e| malformed(e.to_string()))?,
            Some("json") => {
                match serde_json::from_str(&text).map_err(|e| malformed(e.to_string()))? {
                    Value::Array(entries) => entries,
                    entry @ Value::Object(_) => vec![entry],
                    _ => return Err(malformed("expected a JSON object or list".to_string())),
                }
            }
            _ => return Err(TasksetError::UnsupportedSource(path.to_path_buf())),
        };
        let tasks: Vec<TaskRow> = entries
            .into_iter()
            .map(serde_json::from_value)
            .collect::<Result<_, _>>()
            .map_err(|e| malformed(e.to_string()))?;
        let name = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "taskset".to_string());
        let mut taskset = Taskset::new(name, tasks)?;
        taskset.origin = Some(format!("file:{}", path.display()));
        Ok(taskset)
    }

    /// Write this taskset's portable rows to `.json` or `.jsonl`.
    pub fn to_file(&self, path: impl AsRef<Path>) -> Result<PathBuf, TasksetError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let rows: Vec<Value> = self
            .iter()
            .map(|task| serde_json::to_value(task).expect("task rows serialize"))
            .collect();
        let text = match path.extension().and_then(|e| e.to_str()) {
            Some("json") => format!("{}\n", serde_json::to_string_pretty(&rows).expect("json")),
            Some("jsonl") => {
                let mut lines: Vec<String> = rows
                    .iter()
                    .map(|row| serde_json::to_string(row).expect("json"))
                    .collect();
                lines.push(String::new());
                lines.join("\n")
            }
            _ => return Err(TasksetError::UnsupportedSource(path.to_path_buf())),
        };
        std::fs::write(path, text)?;
        Ok(path.to_path_buf())
    }

    pub fn len(&self) -> usize {
        self.tasks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &TaskRow> {
        self.tasks.values()
    }

    pub fn get(&self, slug: &str) -> Option<&TaskRow> {
        self.tasks.get(slug)
    }

    pub fn items(&self) -> impl Iterator<Item = (&String, &TaskRow)> {
        self.tasks.iter()
    }

    /// Env names referenced by tasks in this taskset.
    pub fn environment_names(&self) -> std::collections::BTreeSet<&str> {
        self.iter().map(|t| t.env.as_str()).collect()
    }

    pub fn filter<'a>(&self, slugs: impl IntoIterator<Item = &'a str>) -> Taskset {
        let selected: std::collections::HashSet<&str> = slugs.into_iter().collect();
        self.subset(|slug| selected.contains(slug))
    }

    pub fn exclude<'a>(&self, slugs: impl IntoIterator<Item = &'a str>) -> Taskset {
        let excluded: std::collections::HashSet<&str> = slugs.into_iter().collect();
        self.subset(|slug| !excluded.contains(slug))
    }

    fn subset(&self, keep: impl Fn(&str) -> bool) -> Taskset {
        Taskset {
            name: self.name.clone(),
            origin: self.origin.clone(),
            tasks: self
                .tasks
                .iter()
                .filter(|(slug, _)| keep(slug))
                .map(|(slug, task)| (slug.clone(), task.clone()))
                .collect(),
        }
    }

    /// Run every task × `group` with an optional concurrency cap.
    ///
    /// One shared (stateless) `agent` drives every run against `provider`'s
    /// placement. The `group` repeats of one task share a `group_id` (the
    /// GRPO group). Returned `job.runs` preserves expansion order (task-major,
    /// then group).
    pub async fn run(
        &self,
        agent: &dyn Agent,
        provider: &dyn Provider,
        options: RunOptions,
    ) -> Job {
        let group = options.group.max(1);
        let task_list: Vec<&TaskRow> = self.iter().collect();
        let mut expanded: Vec<(&TaskRow, String)> =
            Vec::with_capacity(task_list.len() * group as usize);
        for task in &task_list {
            let group_id = uuid::Uuid::new_v4().simple().to_string();
            expanded.extend((0..group).map(|_| (*task, group_id.clone())));
        }

        let mut job = options.job.unwrap_or_else(|| Job {
            id: uuid::Uuid::new_v4().simple().to_string(),
            name: job_name(&self.name, &task_list, group),
            runs: Vec::new(),
            group,
            taskset_id: None,
        });
        let job_id = job.id.clone();

        let semaphore = options
            .max_concurrent
            .map(|n| Arc::new(Semaphore::new(n.max(1))));
        tracing::info!(
            rollouts = expanded.len(),
            tasks = task_list.len(),
            group,
            "running taskset"
        );

        let runs = futures::future::join_all(expanded.iter().map(|(task, group_id)| {
            let semaphore = semaphore.clone();
            let job_id = job_id.clone();
            let rollout_timeout = options.rollout_timeout;
            async move {
                let _permit = match &semaphore {
                    Some(s) => Some(s.acquire().await.expect("semaphore never closed")),
                    None => None,
                };
                rollout(
                    task,
                    agent,
                    provider,
                    RolloutOptions {
                        job_id: Some(job_id),
                        group_id: Some(group_id.clone()),
                        trace_id: None,
                        rollout_timeout,
                    },
                )
                .await
            }
        }))
        .await;
        job.runs.extend(runs);
        job
    }
}

/// Scheduler options for [`Taskset::run`].
#[derive(Default)]
pub struct RunOptions {
    /// Rollouts per task, sharing a group_id (default 1).
    pub group: u32,
    pub max_concurrent: Option<usize>,
    /// Hard per-rollout wall-clock cap; a breached rollout is recorded as a
    /// failed/errored run so one wedged rollout cannot stall the batch.
    pub rollout_timeout: Option<Duration>,
    /// An open job to accumulate into (else one is minted for the batch).
    pub job: Option<Job>,
}

fn job_name(taskset_name: &str, tasks: &[&TaskRow], group: u32) -> String {
    let suffix = if group > 1 {
        format!(" ({group} times)")
    } else {
        String::new()
    };
    match tasks {
        [task] => format!("{}{suffix}", task.id),
        _ => format!("{taskset_name} ({} tasks){suffix}", tasks.len()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn task(id: &str, args: Value) -> TaskRow {
        TaskRow::new("env", id).with_args(args.as_object().unwrap().clone())
    }

    #[test]
    fn duplicate_slugs_rejected() {
        let result = Taskset::new("t", vec![task("a", json!({})), task("a", json!({}))]);
        assert!(matches!(result, Err(TasksetError::DuplicateSlugs(_))));
    }

    #[test]
    fn file_roundtrip_json_and_jsonl() {
        let dir = std::env::temp_dir().join(format!("hud-rs-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let taskset = Taskset::new(
            "pair",
            vec![task("a", json!({"x": 1})), task("b", json!({}))],
        )
        .unwrap();

        let a_slug = task("a", json!({"x": 1})).effective_slug();
        for name in ["set.json", "set.jsonl"] {
            let path = dir.join(name);
            taskset.to_file(&path).unwrap();
            let loaded = Taskset::from_file(&path).unwrap();
            assert_eq!(loaded.len(), 2);
            assert!(loaded.get(&a_slug).is_some());
            assert!(loaded.get("b").is_some());
            assert_eq!(loaded.get(&a_slug).unwrap().args["x"], json!(1));
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn filter_and_exclude() {
        let taskset = Taskset::new("t", vec![task("a", json!({})), task("b", json!({}))]).unwrap();
        assert_eq!(taskset.filter(["a"]).len(), 1);
        assert_eq!(taskset.exclude(["a"]).len(), 1);
        assert!(taskset.exclude(["a"]).get("b").is_some());
    }
}
