//! The structured result from grading one run, parsed from the wire grade frame.

use crate::py_truthy;
use serde_json::{Map, Value};

/// Structured result from grading one run.
///
/// Parsed from the `tasks.grade` reply with the same leniency as the Python
/// SDK's `Grade.from_dict`: missing/falsy `score` is 0.0, `done` defaults to
/// true, non-string `content` and non-object `info` are dropped, and the raw
/// frame is kept verbatim.
#[derive(Debug, Clone)]
pub struct Grade {
    pub reward: f64,
    pub done: bool,
    pub content: Option<String>,
    pub info: Map<String, Value>,
    pub is_error: bool,
    /// The wire frame as received.
    pub raw: Map<String, Value>,
}

impl Default for Grade {
    fn default() -> Self {
        Grade {
            reward: 0.0,
            done: true,
            content: None,
            info: Map::new(),
            is_error: false,
            raw: Map::new(),
        }
    }
}

impl Grade {
    /// Parse the wire grade frame (canonical keys: `score`, `done`, `content`,
    /// `info`, `isError`).
    pub fn from_wire(data: Map<String, Value>) -> Grade {
        // Python: `float(data.get("score") or 0.0)` — numbers pass through,
        // numeric strings parse, `true` is 1.0, everything falsy is 0.0.
        let reward = match data.get("score") {
            Some(v) if !py_truthy(v) => 0.0,
            Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
            Some(Value::String(s)) => s.trim().parse().unwrap_or(0.0),
            Some(Value::Bool(_)) => 1.0,
            _ => 0.0,
        };
        let done = data.get("done").map(py_truthy).unwrap_or(true);
        let content = data
            .get("content")
            .and_then(Value::as_str)
            .map(str::to_string);
        let info = data
            .get("info")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let is_error = data.get("isError").map(py_truthy).unwrap_or(false);
        Grade {
            reward,
            done,
            content,
            info,
            is_error,
            raw: data,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn obj(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    #[test]
    fn parses_full_frame() {
        let grade = Grade::from_wire(obj(json!({
            "score": 0.75, "done": false, "content": "close", "info": {"k": 1}, "isError": false
        })));
        assert_eq!(grade.reward, 0.75);
        assert!(!grade.done);
        assert_eq!(grade.content.as_deref(), Some("close"));
        assert_eq!(grade.info["k"], json!(1));
        assert!(!grade.is_error);
    }

    #[test]
    fn defaults_match_python_leniency() {
        let grade = Grade::from_wire(obj(json!({})));
        assert_eq!(grade.reward, 0.0);
        assert!(grade.done);
        assert!(grade.content.is_none());
        assert!(!grade.is_error);

        // `score: null` and non-string content are tolerated like Python.
        let grade = Grade::from_wire(obj(json!({"score": null, "content": 5, "info": []})));
        assert_eq!(grade.reward, 0.0);
        assert!(grade.content.is_none());
        assert!(grade.info.is_empty());
    }
}
