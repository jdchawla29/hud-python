//! One task row: an env name, a task id, bound args, and metadata.
//!
//! The struct *is* the wire format — field names are the wire keys, so serde
//! is the whole codec, matching the Python SDK's pydantic `Task` model.

use crate::canonical::python_json_sorted;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha1::{Digest, Sha1};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskRow {
    /// The environment's *name*: the join key between the row and whatever
    /// placement can bring that environment up.
    pub env: String,
    pub id: String,
    #[serde(default)]
    pub args: Map<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slug: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validation: Option<Vec<Map<String, Value>>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_config: Option<Map<String, Value>>,
    /// Arbitrary metadata surfaced as filterable columns on the platform.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub columns: Option<Map<String, Value>>,
    /// Row-level runtime construction input (image, resources, limits), kept
    /// as raw JSON here; `hud-eval` parses it into its typed `RuntimeConfig`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_config: Option<Value>,
}

impl TaskRow {
    pub fn new(env: impl Into<String>, id: impl Into<String>) -> Self {
        TaskRow {
            env: env.into(),
            id: id.into(),
            args: Map::new(),
            slug: None,
            validation: None,
            agent_config: None,
            columns: None,
            runtime_config: None,
        }
    }

    pub fn with_args(mut self, args: Map<String, Value>) -> Self {
        self.args = args;
        self
    }

    /// A stable slug from the task id, disambiguated by an args hash when
    /// present. Byte-compatible with the Python SDK's `Task.default_slug`.
    pub fn default_slug(&self) -> String {
        if self.args.is_empty() {
            return self.id.clone();
        }
        let canonical = python_json_sorted(&Value::Object(self.args.clone()));
        let digest = Sha1::digest(canonical.as_bytes());
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        format!("{}-{}", self.id, &hex[..8])
    }

    /// The effective slug: the explicit one, else `default_slug`.
    pub fn effective_slug(&self) -> String {
        self.slug.clone().unwrap_or_else(|| self.default_slug())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn slug_without_args_is_id() {
        assert_eq!(TaskRow::new("env", "checkout").default_slug(), "checkout");
    }

    #[test]
    fn slug_matches_python_sha1() {
        // python3 -c 'import hashlib, json; print(hashlib.sha1(json.dumps(
        //   {"n": 3, "text": "hello"}, sort_keys=True, default=str
        // ).encode()).hexdigest()[:8])'  -> 361bc756
        let mut args = Map::new();
        args.insert("text".to_string(), json!("hello"));
        args.insert("n".to_string(), json!(3));
        let task = TaskRow::new("env", "echo").with_args(args);
        assert_eq!(task.default_slug(), "echo-361bc756");
    }

    #[test]
    fn row_roundtrips_compactly() {
        let row: TaskRow =
            serde_json::from_value(json!({"env": "browser", "id": "buy", "args": {"q": 1}}))
                .unwrap();
        let dumped = serde_json::to_value(&row).unwrap();
        assert_eq!(
            dumped,
            json!({"env": "browser", "id": "buy", "args": {"q": 1}})
        );
    }
}
