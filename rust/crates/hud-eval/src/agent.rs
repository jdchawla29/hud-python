//! The agent contract: fill `run.trace` by driving the task.

use crate::run::Run;
use async_trait::async_trait;
use futures::future::BoxFuture;

pub type AgentError = Box<dyn std::error::Error + Send + Sync>;

/// An agent harness: drives one live [`Run`] (read the prompt, work the
/// capabilities, record steps, set `run.trace.content` to the final answer).
///
/// Stateless by contract — one instance drives many concurrent rollouts.
#[async_trait]
pub trait Agent: Send + Sync {
    async fn run(&self, run: &mut Run) -> Result<(), AgentError>;

    /// The inference-model slug this agent samples, when known (trace
    /// attribution metadata).
    fn model(&self) -> Option<String> {
        None
    }
}

/// Wrap a closure as an [`Agent`]:
///
/// ```ignore
/// let agent = agent_fn(|run| Box::pin(async move {
///     run.trace.content = Some(run.prompt_text());
///     Ok(())
/// }));
/// ```
pub fn agent_fn<F>(f: F) -> impl Agent
where
    F: for<'a> Fn(&'a mut Run) -> BoxFuture<'a, Result<(), AgentError>> + Send + Sync,
{
    FnAgent(f)
}

struct FnAgent<F>(F);

#[async_trait]
impl<F> Agent for FnAgent<F>
where
    F: for<'a> Fn(&'a mut Run) -> BoxFuture<'a, Result<(), AgentError>> + Send + Sync,
{
    async fn run(&self, run: &mut Run) -> Result<(), AgentError> {
        (self.0)(run).await
    }
}
