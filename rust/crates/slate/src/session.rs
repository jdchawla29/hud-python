//! On-disk session store: transcript + conversation, for resume.
//!
//! Sessions live under `~/.slate/sessions/<id>/`:
//! - `meta.json`    — id, created, work_dir, models, task
//! - `messages.json`— the orchestrator conversation (rewritten each turn)
//! - `events.jsonl` — the UI event transcript (appended live), for replay
//!
//! Resuming loads the conversation so the orchestrator continues with full
//! context, and replays the transcript into the UI as history. Worker threads
//! are not restored — they are disposable tactical context; their durable
//! conclusions already live in the conversation as episodes.

use crate::agent::UiEvent;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: String,
    pub created: String,
    pub work_dir: String,
    pub orch_model: String,
    pub worker_model: String,
    pub task: String,
}

/// A live session's on-disk home.
pub struct SessionStore {
    id: String,
    dir: PathBuf,
    events: Mutex<std::fs::File>,
}

impl SessionStore {
    fn root() -> PathBuf {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        home.join(".slate/sessions")
    }

    /// A fresh short session id.
    pub fn new_id() -> String {
        uuid::Uuid::new_v4().simple().to_string()[..12].to_string()
    }

    /// Create (or truncate) the store for a new session and write its meta.
    pub fn create(meta: &SessionMeta) -> std::io::Result<SessionStore> {
        let dir = Self::root().join(&meta.id);
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
        let dir = Self::root().join(id);
        if !dir.join("meta.json").exists() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("no session {id:?} under {}", Self::root().display()),
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
        let path = Self::root().join(id).join("meta.json");
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

    /// List saved session ids, newest first by directory mtime.
    pub fn list() -> Vec<SessionMeta> {
        let Ok(entries) = std::fs::read_dir(Self::root()) else {
            return Vec::new();
        };
        let mut metas: Vec<(std::time::SystemTime, SessionMeta)> = entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let modified = entry.metadata().ok()?.modified().ok()?;
                let id = entry.file_name().to_string_lossy().into_owned();
                Some((modified, Self::load_meta(&id).ok()?))
            })
            .collect();
        metas.sort_by(|a, b| b.0.cmp(&a.0));
        metas.into_iter().map(|(_, meta)| meta).collect()
    }
}
