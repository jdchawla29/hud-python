//! Task authoring: templates mint task instances; instances run start → grade.

use async_trait::async_trait;
use futures::future::BoxFuture;
use hud_types::{EvaluationResult, PromptMessage};
use serde::de::DeserializeOwned;
use serde_json::{json, Map, Value};
use std::future::Future;
use std::sync::Arc;

/// A task-author failure. Surfaced to the client as a `-32000` error frame
/// with the message, exactly like an exception in a Python task generator.
pub type TaskError = Box<dyn std::error::Error + Send + Sync>;

/// The opening prompt a task yields.
#[derive(Debug, Clone)]
pub enum Prompt {
    Text(String),
    /// Chat-style / multi-turn prompt.
    Messages(Vec<PromptMessage>),
    /// A raw frame: an object already containing a `prompt` key passes
    /// through; anything else is wrapped as `{"prompt": <value>}`.
    Raw(Value),
}

impl Prompt {
    /// The `tasks.start` reply frame.
    pub(crate) fn into_frame(self) -> Map<String, Value> {
        let value = match self {
            Prompt::Text(text) => Value::String(text),
            Prompt::Messages(messages) => serde_json::to_value(messages).unwrap_or(Value::Null),
            Prompt::Raw(value) => {
                if let Value::Object(map) = &value {
                    if map.contains_key("prompt") {
                        return map.clone();
                    }
                }
                value
            }
        };
        let mut frame = Map::new();
        frame.insert("prompt".to_string(), value);
        frame
    }
}

impl From<&str> for Prompt {
    fn from(text: &str) -> Prompt {
        Prompt::Text(text.to_string())
    }
}

impl From<String> for Prompt {
    fn from(text: String) -> Prompt {
        Prompt::Text(text)
    }
}

/// The agent's answer, handed to `grade`.
///
/// `value` is the `answer` field of the `tasks.grade` payload (usually the
/// agent's final text); `payload` is the full grade params object for tasks
/// that read extra fields.
#[derive(Debug, Clone)]
pub struct Answer {
    pub value: Value,
    pub payload: Map<String, Value>,
}

impl Answer {
    pub(crate) fn from_payload(payload: Map<String, Value>) -> Answer {
        Answer {
            value: payload.get("answer").cloned().unwrap_or(Value::Null),
            payload,
        }
    }

    /// The answer as text: a string answer verbatim, anything else JSON-encoded.
    pub fn text(&self) -> String {
        match &self.value {
            Value::String(s) => s.clone(),
            Value::Null => String::new(),
            other => other.to_string(),
        }
    }

    /// Parse the answer into a structured type. A string answer is parsed as
    /// JSON (the structured-answer contract); other values deserialize directly.
    pub fn parse<T: DeserializeOwned>(&self) -> Result<T, serde_json::Error> {
        match &self.value {
            Value::String(s) => serde_json::from_str(s),
            other => serde_json::from_value(other.clone()),
        }
    }
}

/// What a task grades with — the analog of the Python generator's final yield:
/// a bare score, a structured [`EvaluationResult`], or a raw grade frame.
#[derive(Debug, Clone)]
pub enum Evaluation {
    Score(f64),
    Result(EvaluationResult),
    /// A raw frame; must contain a numeric `score`.
    Frame(Map<String, Value>),
}

impl Evaluation {
    /// The `tasks.grade` reply frame (`reward` renamed to `score`).
    pub(crate) fn into_frame(self, task_id: &str) -> Result<Map<String, Value>, String> {
        match self {
            Evaluation::Score(score) => {
                let mut frame = Map::new();
                frame.insert("score".to_string(), json!(score));
                Ok(frame)
            }
            Evaluation::Result(result) => Ok(result.to_grade_frame()),
            Evaluation::Frame(frame) => {
                if !frame.get("score").map(Value::is_number).unwrap_or(false) {
                    let mut keys: Vec<&String> = frame.keys().collect();
                    keys.sort_unstable();
                    return Err(format!(
                        "task '{task_id}' graded with a dict missing a numeric 'score' (keys: {keys:?})"
                    ));
                }
                Ok(frame)
            }
        }
    }
}

impl From<f64> for Evaluation {
    fn from(score: f64) -> Evaluation {
        Evaluation::Score(score)
    }
}

impl From<EvaluationResult> for Evaluation {
    fn from(result: EvaluationResult) -> Evaluation {
        Evaluation::Result(result)
    }
}

/// One running task: the two-phase replacement for Python's suspended
/// async-generator. `start` returns the opening prompt; `grade` consumes the
/// agent's answer and returns the evaluation. State lives on the value.
#[async_trait]
pub trait TaskInstance: Send {
    async fn start(&mut self) -> Result<Prompt, TaskError>;
    async fn grade(&mut self, answer: Answer) -> Result<Evaluation, TaskError>;
    /// Teardown for a task abandoned between start and grade (the analog of
    /// closing the Python generator). Default: nothing to clean up.
    async fn cancel(&mut self) {}
}

/// A registered task template: mints [`TaskInstance`]s from wire args and
/// publishes its manifest entry (`tasks.list`).
pub trait Template: Send + Sync {
    fn id(&self) -> &str;

    fn description(&self) -> &str {
        ""
    }

    /// JSON Schema for the task's args contract.
    fn args_schema(&self) -> Value {
        json!({"type": "object", "additionalProperties": true})
    }

    /// Schema of extra runtime input, when declared.
    fn input_schema(&self) -> Option<Value> {
        None
    }

    /// Schema of the structured answer, when declared.
    fn returns_schema(&self) -> Option<Value> {
        None
    }

    fn create(&self, args: Map<String, Value>) -> Result<Box<dyn TaskInstance>, TaskError>;

    /// The `tasks.list` entry: `{id, description, args, input?, returns?}`.
    fn manifest_entry(&self) -> Value {
        let mut entry = Map::new();
        entry.insert("id".to_string(), json!(self.id()));
        entry.insert("description".to_string(), json!(self.description()));
        entry.insert("args".to_string(), self.args_schema());
        if let Some(input) = self.input_schema() {
            entry.insert("input".to_string(), input);
        }
        if let Some(returns) = self.returns_schema() {
            entry.insert("returns".to_string(), returns);
        }
        Value::Object(entry)
    }
}

/// Build a [`Template`] from a pair of closures — the ergonomic authoring path,
/// recovering the Python generator's shape:
///
/// ```
/// # use hud_env::{template, Evaluation, Prompt};
/// # use serde::Deserialize;
/// #[derive(Deserialize)]
/// struct EchoArgs {
///     text: String,
/// }
///
/// let echo = template(
///     "echo",
///     "Repeat the text exactly.",
///     |args: EchoArgs| async move {
///         let expected = args.text.clone();
///         Ok((Prompt::Text(format!("Repeat exactly: {}", args.text)), expected))
///     },
///     |expected: String, answer| async move {
///         Ok(Evaluation::Score(if answer.text().trim() == expected { 1.0 } else { 0.0 }))
///     },
/// );
/// ```
///
/// `start` receives the deserialized args and returns `(prompt, state)`;
/// `grade` consumes the state and the agent's answer.
pub fn template<A, S, StartFut, GradeFut>(
    id: &str,
    description: &str,
    start: impl Fn(A) -> StartFut + Send + Sync + 'static,
    grade: impl Fn(S, Answer) -> GradeFut + Send + Sync + 'static,
) -> impl Template
where
    A: DeserializeOwned + Send + 'static,
    S: Send + 'static,
    StartFut: Future<Output = Result<(Prompt, S), TaskError>> + Send + 'static,
    GradeFut: Future<Output = Result<Evaluation, TaskError>> + Send + 'static,
{
    FnTemplate {
        id: id.to_string(),
        description: description.to_string(),
        args_schema: None,
        returns_schema: None,
        start: Arc::new(move |args| Box::pin(start(args))),
        grade: Arc::new(move |state, answer| Box::pin(grade(state, answer))),
    }
}

type StartFn<A, S> =
    Arc<dyn Fn(A) -> BoxFuture<'static, Result<(Prompt, S), TaskError>> + Send + Sync>;
type GradeFn<S> =
    Arc<dyn Fn(S, Answer) -> BoxFuture<'static, Result<Evaluation, TaskError>> + Send + Sync>;

struct FnTemplate<A, S> {
    id: String,
    description: String,
    args_schema: Option<Value>,
    returns_schema: Option<Value>,
    start: StartFn<A, S>,
    grade: GradeFn<S>,
}

impl<A, S> Template for FnTemplate<A, S>
where
    A: DeserializeOwned + Send + 'static,
    S: Send + 'static,
{
    fn id(&self) -> &str {
        &self.id
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn args_schema(&self) -> Value {
        self.args_schema
            .clone()
            .unwrap_or_else(|| json!({"type": "object", "additionalProperties": true}))
    }

    fn returns_schema(&self) -> Option<Value> {
        self.returns_schema.clone()
    }

    fn create(&self, args: Map<String, Value>) -> Result<Box<dyn TaskInstance>, TaskError> {
        let args = coerce_args::<A>(args)?;
        Ok(Box::new(FnTask {
            phase: FnTaskPhase::Created(args),
            start: Arc::clone(&self.start),
            grade: Arc::clone(&self.grade),
        }))
    }
}

/// Deserialize wire args with the Python server's leniency: on a direct
/// mismatch, string values that themselves parse as JSON are unwrapped and the
/// deserialize retried (JSON-RPC clients often send rich args as strings).
fn coerce_args<A: DeserializeOwned>(args: Map<String, Value>) -> Result<A, TaskError> {
    let direct = serde_json::from_value(Value::Object(args.clone()));
    match direct {
        Ok(parsed) => Ok(parsed),
        Err(first_err) => {
            let mut coerced = args;
            let mut changed = false;
            for (_, value) in coerced.iter_mut() {
                if let Value::String(s) = value {
                    if let Ok(parsed) = serde_json::from_str::<Value>(s) {
                        if !parsed.is_string() {
                            *value = parsed;
                            changed = true;
                        }
                    }
                }
            }
            if !changed {
                return Err(Box::new(first_err));
            }
            serde_json::from_value(Value::Object(coerced)).map_err(|_| Box::new(first_err).into())
        }
    }
}

enum FnTaskPhase<A, S> {
    Created(A),
    Started(S),
    Spent,
}

struct FnTask<A, S> {
    phase: FnTaskPhase<A, S>,
    start: StartFn<A, S>,
    grade: GradeFn<S>,
}

#[async_trait]
impl<A, S> TaskInstance for FnTask<A, S>
where
    A: Send + 'static,
    S: Send + 'static,
{
    async fn start(&mut self) -> Result<Prompt, TaskError> {
        match std::mem::replace(&mut self.phase, FnTaskPhase::Spent) {
            FnTaskPhase::Created(args) => {
                let (prompt, state) = (self.start)(args).await?;
                self.phase = FnTaskPhase::Started(state);
                Ok(prompt)
            }
            _ => Err("task already started".into()),
        }
    }

    async fn grade(&mut self, answer: Answer) -> Result<Evaluation, TaskError> {
        match std::mem::replace(&mut self.phase, FnTaskPhase::Spent) {
            FnTaskPhase::Started(state) => (self.grade)(state, answer).await,
            _ => Err("task not started".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Deserialize)]
    struct EchoArgs {
        text: String,
        #[serde(default)]
        n: u32,
    }

    fn echo_template() -> impl Template {
        template(
            "echo",
            "Repeat the text.",
            |args: EchoArgs| async move {
                let expected = args.text.repeat(args.n.max(1) as usize);
                Ok((Prompt::Text(format!("say: {expected}")), expected))
            },
            |expected: String, answer| async move {
                Ok(Evaluation::Score(if answer.text() == expected {
                    1.0
                } else {
                    0.0
                }))
            },
        )
    }

    fn obj(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    #[tokio::test]
    async fn start_then_grade() {
        let template = echo_template();
        let mut task = template.create(obj(json!({"text": "hi", "n": 2}))).unwrap();
        let prompt = task.start().await.unwrap();
        assert!(matches!(&prompt, Prompt::Text(t) if t == "say: hihi"));
        let answer = Answer::from_payload(obj(json!({"answer": "hihi"})));
        let eval = task.grade(answer).await.unwrap();
        assert!(matches!(eval, Evaluation::Score(s) if s == 1.0));
    }

    #[tokio::test]
    async fn string_args_coerce_like_python() {
        let template = echo_template();
        // "n" arrives as a JSON-encoded string; the coercion shim unwraps it.
        let mut task = template
            .create(obj(json!({"text": "a", "n": "3"})))
            .unwrap();
        let prompt = task.start().await.unwrap();
        assert!(matches!(&prompt, Prompt::Text(t) if t == "say: aaa"));
    }

    #[test]
    fn bad_args_fail_at_create() {
        let template = echo_template();
        assert!(template.create(obj(json!({"n": 1}))).is_err());
    }

    #[test]
    fn manifest_entry_shape() {
        let entry = echo_template().manifest_entry();
        assert_eq!(entry["id"], "echo");
        assert_eq!(entry["description"], "Repeat the text.");
        assert!(entry["args"].is_object());
        assert!(entry.get("returns").is_none());
    }

    #[test]
    fn prompt_frames() {
        assert_eq!(
            Value::Object(Prompt::Text("hi".into()).into_frame()),
            json!({"prompt": "hi"})
        );
        assert_eq!(
            Value::Object(Prompt::Raw(json!({"prompt": "p", "extra": 1})).into_frame()),
            json!({"prompt": "p", "extra": 1})
        );
        assert_eq!(
            Value::Object(Prompt::Raw(json!(["turn"])).into_frame()),
            json!({"prompt": ["turn"]})
        );
    }

    #[test]
    fn evaluation_frames() {
        assert_eq!(
            Value::Object(Evaluation::Score(0.5).into_frame("t").unwrap()),
            json!({"score": 0.5})
        );
        assert!(Evaluation::Frame(obj(json!({"reward": 1})))
            .into_frame("t")
            .is_err());
        assert_eq!(
            Value::Object(
                Evaluation::Frame(obj(json!({"score": 1, "note": "x"})))
                    .into_frame("t")
                    .unwrap()
            ),
            json!({"score": 1, "note": "x"})
        );
    }
}
