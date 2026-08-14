//! On-disk session store: transcript + conversation, for resume.
//!
//! Sessions live under `$DAS_HOME/sessions/<id>/` or `~/.das/sessions/<id>/`:
//! - `meta.json`    — id, created, work_dir, models, task
//! - `messages.json`— the orchestrator conversation (rewritten each turn)
//! - `events.jsonl` — the UI event transcript (appended live), for replay
//!
//! Resuming loads the conversation so the orchestrator continues with full
//! context, and replays the transcript into the UI as history. Worker threads
//! are not restored — they are disposable tactical context; their durable
//! conclusions already live in the conversation as episodes.

use crate::agent::UiEvent;
use crate::harness::WorkerHarnessKind;
use crate::orchestrator::OrchestratorKind;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: String,
    pub project_id: i64,
    pub workspace_id: i64,
    pub created: String,
    pub work_dir: String,
    #[serde(default)]
    pub orch_harness: OrchestratorKind,
    pub orch_model: Option<String>,
    pub worker_harness: WorkerHarnessKind,
    pub worker_model: Option<String>,
    pub codex_socket: Option<PathBuf>,
    pub task: String,
}

/// A live session's on-disk home.
pub struct SessionStore {
    id: String,
    dir: PathBuf,
    events: Mutex<std::fs::File>,
}

impl SessionStore {
    fn root() -> std::io::Result<PathBuf> {
        Ok(crate::paths::home()?.join("sessions"))
    }

    /// A fresh short session id.
    pub fn new_id() -> String {
        uuid::Uuid::new_v4().simple().to_string()[..12].to_string()
    }

    /// Create (or truncate) the store for a new session and write its meta.
    pub fn create(meta: &SessionMeta) -> std::io::Result<SessionStore> {
        let dir = Self::root()?.join(&meta.id);
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join("meta.json"), serde_json::to_vec_pretty(meta)?)?;
        let events = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("events.jsonl"))?;
        Ok(SessionStore {
            id: meta.id.clone(),
            dir,
            events: Mutex::new(events),
        })
    }

    /// Reopen an existing session for appending (resume).
    pub fn open(id: &str) -> std::io::Result<SessionStore> {
        let root = Self::root()?;
        let dir = root.join(id);
        if !dir.join("meta.json").exists() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("no session {id:?} under {}", root.display()),
            ));
        }
        let events = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("events.jsonl"))?;
        Ok(SessionStore {
            id: id.to_string(),
            dir,
            events: Mutex::new(events),
        })
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn load_meta(id: &str) -> std::io::Result<SessionMeta> {
        let path = Self::root()?.join(id).join("meta.json");
        let bytes = std::fs::read(path)?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    /// The orchestrator conversation to resume, or empty if none saved.
    pub fn load_messages(&self) -> Vec<Value> {
        std::fs::read(self.dir.join("messages.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    /// The recorded transcript, for replay into the UI.
    pub fn load_events(&self) -> Vec<UiEvent> {
        let Ok(text) = std::fs::read_to_string(self.dir.join("events.jsonl")) else {
            return Vec::new();
        };
        text.lines()
            .filter(|line| !line.trim().is_empty())
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    pub fn save_messages(&self, messages: &[Value]) {
        if let Ok(bytes) = serde_json::to_vec(messages) {
            let _ = std::fs::write(self.dir.join("messages.json"), bytes);
        }
    }

    /// Append one event to the transcript. Status lines are transient and not
    /// persisted; everything else is durable history.
    pub fn append_event(&self, event: &UiEvent) {
        if matches!(event, UiEvent::Status(_)) {
            return;
        }
        if let Ok(mut line) = serde_json::to_vec(event) {
            line.push(b'\n');
            if let Ok(mut file) = self.events.lock() {
                let _ = file.write_all(&line);
            }
        }
    }

    pub fn list_for_workspace(workspace_id: i64) -> std::io::Result<Vec<SessionMeta>> {
        let root = Self::root()?;
        let entries = match std::fs::read_dir(root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let mut metas = Vec::new();
        for entry in entries {
            let entry = entry?;
            let modified = entry.metadata()?.modified()?;
            let id = entry.file_name().to_string_lossy().into_owned();
            let meta = Self::load_meta(&id)?;
            if meta.workspace_id == workspace_id {
                metas.push((modified, meta));
            }
        }
        metas.sort_by(|a, b| b.0.cmp(&a.0));
        Ok(metas.into_iter().map(|(_, meta)| meta).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_records_project_and_workspace_ownership() {
        let meta: SessionMeta = serde_json::from_value(serde_json::json!({
            "id": "session",
            "project_id": 7,
            "workspace_id": 11,
            "created": "2026-08-06T00:00:00Z",
            "work_dir": "/tmp/workspace",
            "orch_model": "orchestrator",
            "worker_harness": "codex",
            "worker_model": null,
            "codex_socket": null,
            "task": "inspect"
        }))
        .unwrap();

        assert_eq!(meta.project_id, 7);
        assert_eq!(meta.workspace_id, 11);
        assert_eq!(meta.orch_harness, OrchestratorKind::Gateway);
        assert_eq!(meta.orch_model.as_deref(), Some("orchestrator"));
    }

    #[test]
    fn old_bash_events_default_missing_output() {
        let event: UiEvent =
            serde_json::from_str(r#"{"Bash":{"thread":"worker","command":"pwd","exit_status":0}}"#)
                .unwrap();

        let UiEvent::Bash { output, .. } = event else {
            panic!("expected bash event");
        };
        assert!(output.is_empty());
    }
}
