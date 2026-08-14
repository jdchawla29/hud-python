//! Grading result shapes: `SubScore` and `EvaluationResult`.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// One component of the final reward, for debugging and transparency.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubScore {
    pub name: String,
    /// Weight for the weighted average; negative weights are penalties.
    #[serde(default = "default_weight")]
    pub weight: f64,
    /// 0.0 to 1.0.
    pub value: f64,
}

fn default_weight() -> f64 {
    1.0
}

fn default_done() -> bool {
    true
}

impl SubScore {
    pub fn new(name: impl Into<String>, value: f64) -> Self {
        SubScore {
            name: name.into(),
            weight: 1.0,
            value,
        }
    }

    pub fn weighted(name: impl Into<String>, value: f64, weight: f64) -> Self {
        SubScore {
            name: name.into(),
            weight,
            value,
        }
    }
}

/// Result of a task's evaluate phase.
///
/// Serializes to the same shape as the Python pydantic model (`extra="allow"`):
/// unknown fields ride along in `extra` and are flattened back on dump.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvaluationResult {
    /// Final score, usually 0.0 to 1.0.
    #[serde(default)]
    pub reward: f64,
    /// Whether the task/episode is complete.
    #[serde(default = "default_done")]
    pub done: bool,
    /// Human-readable explanation.
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub info: Map<String, Value>,
    /// Whether the evaluation itself failed.
    #[serde(rename = "isError", default)]
    pub is_error: bool,
    /// Optional breakdown of score components.
    #[serde(default)]
    pub subscores: Option<Vec<SubScore>>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Default for EvaluationResult {
    fn default() -> Self {
        EvaluationResult {
            reward: 0.0,
            done: true,
            content: None,
            info: Map::new(),
            is_error: false,
            subscores: None,
            extra: Map::new(),
        }
    }
}

impl EvaluationResult {
    pub fn with_reward(reward: f64) -> Self {
        EvaluationResult {
            reward,
            ..Default::default()
        }
    }

    pub fn content(mut self, content: impl Into<String>) -> Self {
        self.content = Some(content.into());
        self
    }

    pub fn error(mut self, message: impl Into<String>) -> Self {
        self.is_error = true;
        self.content = Some(message.into());
        self
    }

    pub fn subscores(mut self, subscores: Vec<SubScore>) -> Self {
        self.subscores = Some(subscores);
        self
    }

    /// The `tasks.grade` wire frame: the serialized model with `reward`
    /// renamed to `score`, exactly like the Python server does.
    pub fn to_grade_frame(&self) -> Map<String, Value> {
        let mut frame = match serde_json::to_value(self) {
            Ok(Value::Object(map)) => map,
            _ => Map::new(),
        };
        if let Some(reward) = frame.remove("reward") {
            frame.insert("score".to_string(), reward);
        }
        frame
    }
}

impl From<f64> for EvaluationResult {
    fn from(reward: f64) -> Self {
        EvaluationResult::with_reward(reward)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn grade_frame_renames_reward_to_score() {
        let result = EvaluationResult::with_reward(0.5)
            .content("half")
            .subscores(vec![SubScore::new("part", 0.5)]);
        let frame = Value::Object(result.to_grade_frame());
        assert_eq!(
            frame,
            json!({
                "score": 0.5, "done": true, "content": "half", "info": {}, "isError": false,
                "subscores": [{"name": "part", "weight": 1.0, "value": 0.5}],
            })
        );
    }

    #[test]
    fn extra_fields_flatten_through() {
        let parsed: EvaluationResult =
            serde_json::from_value(json!({"reward": 1.0, "done": true, "custom": "x"})).unwrap();
        assert_eq!(parsed.extra["custom"], json!("x"));
        let frame = parsed.to_grade_frame();
        assert_eq!(frame["custom"], json!("x"));
        assert_eq!(frame["score"], json!(1.0));
    }
}
