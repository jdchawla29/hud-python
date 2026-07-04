//! HUD environment authoring and the protocol server.
//!
//! An [`Environment`] declares what exists: identity, capabilities, and task
//! templates. [`server::serve`] puts one on the wire — TCP, newline-delimited
//! JSON-RPC 2.0, wire-compatible with the Python SDK (`hud/1.0`).
//!
//! Where the Python SDK expresses a task as a suspended async generator
//! (`yield prompt ... reward = yield`), Rust splits the same lifecycle into a
//! two-phase [`TaskInstance`]: `start()` returns the prompt, `grade(answer)`
//! returns the evaluation, with state living on the value between the two.

mod environment;
pub mod server;
mod task;

pub use environment::{Environment, EnvironmentError};
pub use server::{bind, serve, BoundServer};
pub use task::{template, Answer, Evaluation, Prompt, TaskError, TaskInstance, Template};
