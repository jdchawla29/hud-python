//! Core wire types for the HUD protocol (`hud/1.0`).
//!
//! Pure data: everything here is serde-serializable and wire-compatible with
//! the Python SDK (`hud-python`). No I/O and no async runtime.

pub mod canonical;
pub mod capability;
pub mod grade;
pub mod prompt;
pub mod result;
pub mod step;
pub mod task;
pub mod url;

pub use capability::{normalize_url, Capability};
pub use grade::Grade;
pub use prompt::{coerce_prompt_message, Content, PromptMessage, Role};
pub use result::{EvaluationResult, SubScore};
pub use step::{Step, StepSource, TaskCall, TaskPhase, Trace, TraceStatus};
pub use task::TaskRow;
pub use url::UrlParts;

/// Protocol version spoken over the control channel.
pub const PROTOCOL_VERSION: &str = "hud/1.0";

/// Current UTC time as an ISO-8601 / RFC 3339 string (the Python SDK's `now_iso`).
pub fn now_iso() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .expect("UTC timestamp always formats")
}

/// Python-style truthiness for a JSON value (`bool(x)` semantics), used when
/// parsing lenient wire fields exactly like the Python SDK does.
pub(crate) fn py_truthy(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Null => false,
        serde_json::Value::Bool(b) => *b,
        serde_json::Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        serde_json::Value::String(s) => !s.is_empty(),
        serde_json::Value::Array(a) => !a.is_empty(),
        serde_json::Value::Object(o) => !o.is_empty(),
    }
}
