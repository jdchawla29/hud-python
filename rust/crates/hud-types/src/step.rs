//! The trajectory contract: `Trace` is an ordered collection of `Step`s.
//!
//! `Step` here is the shared skeleton every agent family and the run harness
//! speak — ordering, source, timing, error. Family payloads (LLM responses,
//! tool calls) ride in `payload`, which flattens into the serialized step so
//! subclass-style extension fields survive round-trips with the Python SDK's
//! polymorphic pydantic steps.

use crate::now_iso;
use crate::prompt::PromptMessage;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Schema tag of the core step stream.
pub const STEP_SCHEMA: &str = "hud.step.v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StepSource {
    User,
    Agent,
    Tool,
    Task,
    Subagent,
    System,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskPhase {
    Setup,
    Evaluate,
}

/// The task-lifecycle RPC a `task` step records: `setup` is `tasks.start`
/// (result carries the opening prompt payload); `evaluate` is `tasks.grade`
/// (result carries the evaluation dict).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskCall {
    pub phase: TaskPhase,
    pub name: String,
    #[serde(default)]
    pub arguments: Value,
    #[serde(default)]
    pub result: Value,
}

/// One ordered interaction unit in a task run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Step {
    /// Sequential position in the trace, assigned by `Trace::record` (1-based).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_id: Option<u64>,
    pub source: StepSource,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub messages: Vec<PromptMessage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_call: Option<TaskCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<String>,
    /// Free-form metadata with no structured home.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub extra: Map<String, Value>,
    /// Agent-family extension fields (the Python SDK's `Step` subclasses),
    /// flattened into the serialized step so they survive round-trips.
    #[serde(flatten)]
    pub payload: Map<String, Value>,
}

impl Step {
    pub fn new(source: StepSource) -> Step {
        Step {
            step_id: None,
            source,
            messages: Vec::new(),
            task_call: None,
            error: None,
            started_at: None,
            ended_at: None,
            extra: Map::new(),
            payload: Map::new(),
        }
    }

    pub fn user(messages: Vec<PromptMessage>) -> Step {
        Step {
            messages,
            ..Step::new(StepSource::User)
        }
    }

    pub fn task(task_call: TaskCall, started_at: impl Into<String>) -> Step {
        Step {
            task_call: Some(task_call),
            started_at: Some(started_at.into()),
            ..Step::new(StepSource::Task)
        }
    }

    pub fn system_error(error: impl Into<String>) -> Step {
        Step {
            error: Some(error.into()),
            ..Step::new(StepSource::System)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TraceStatus {
    Completed,
    Error,
    Cancelled,
}

/// The agent's trajectory for one rollout: ordered steps plus the run summary.
///
/// `record` is the single write path; everything else the summary exposes is
/// derived from the steps via [`Trace::final_by`] / [`Trace::collect`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Trace {
    #[serde(default)]
    pub steps: Vec<Step>,
    #[serde(default)]
    pub status: Option<TraceStatus>,
    /// The final content (the graded answer).
    #[serde(default)]
    pub content: Option<String>,
    /// Trajectory metadata with no structured home; never load-bearing.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub extra: Map<String, Value>,
    /// Keys the server-side-collected trajectory; `None` for eval-only runs.
    #[serde(default)]
    pub trace_id: Option<String>,
}

impl Trace {
    /// Append one step: numbers it and stamps `ended_at` when unset (a step
    /// ends when it's recorded).
    pub fn record(&mut self, mut step: Step) {
        step.step_id = Some(self.steps.len() as u64 + 1);
        if step.ended_at.is_none() {
            step.ended_at = Some(now_iso());
        }
        self.steps.push(step);
    }

    /// The newest step's answer to `get` — the finalized-field query.
    pub fn final_by<'a, T>(&'a self, get: impl Fn(&'a Step) -> Option<T>) -> Option<T> {
        self.steps.iter().rev().find_map(get)
    }

    /// Every step's answer to `get`, in step order — the gathering query.
    pub fn collect<'a, T>(&'a self, get: impl Fn(&'a Step) -> Option<T>) -> Vec<T> {
        self.steps.iter().filter_map(get).collect()
    }

    pub fn is_error(&self) -> bool {
        self.status == Some(TraceStatus::Error)
    }

    /// The most recent step error, if any (errors live on steps).
    pub fn error(&self) -> Option<&str> {
        self.final_by(|step| step.error.as_deref())
    }

    pub fn len(&self) -> usize {
        self.steps.len()
    }

    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn record_numbers_and_stamps() {
        let mut trace = Trace::default();
        trace.record(Step::system_error("boom"));
        trace.record(Step::user(vec![PromptMessage::user("hi")]));
        assert_eq!(trace.steps[0].step_id, Some(1));
        assert_eq!(trace.steps[1].step_id, Some(2));
        assert!(trace.steps[0].ended_at.is_some());
        assert_eq!(trace.error(), Some("boom"));
    }

    #[test]
    fn family_payload_fields_survive_roundtrip() {
        let raw = json!({
            "step_id": 1, "source": "agent", "content": "answer",
            "tool_calls": [{"name": "bash"}], "done": true
        });
        let step: Step = serde_json::from_value(raw.clone()).unwrap();
        assert_eq!(step.payload["content"], json!("answer"));
        assert_eq!(serde_json::to_value(&step).unwrap(), raw);
    }

    #[test]
    fn final_by_prefers_newest() {
        let mut trace = Trace::default();
        trace.record(Step::system_error("first"));
        trace.record(Step::new(StepSource::Agent));
        trace.record(Step::system_error("second"));
        assert_eq!(trace.error(), Some("second"));
    }
}
