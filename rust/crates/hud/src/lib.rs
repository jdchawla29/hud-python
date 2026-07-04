//! The HUD SDK for Rust: environments, clients, and the rollout engine.
//!
//! A Rust port of the core of the Python SDK (`hud-python`), wire-compatible
//! with it over the `hud/1.0` protocol: a Rust client drives Python-served
//! environments and vice versa.
//!
//! - Author environments with [`Environment`] and serve them with
//!   [`env::serve`] (`hud-env`).
//! - Connect to a served env with [`connect`] (`hud-client`).
//! - Run tasks to graded [`Run`]s with [`rollout`] / [`Taskset::run`]
//!   (`hud-eval`).

pub use hud_client::{connect, ConnectOptions, HudClient, HudClientError, Manifest, ServerInfo};
pub use hud_env::{template, Answer, Environment, Evaluation, Prompt, TaskInstance, Template};
pub use hud_eval::{
    agent_fn, rollout, Agent, DockerRuntime, Grade, Job, LocalRuntime, Provider, RolloutOptions,
    Run, RunOptions, Runtime, RuntimeConfig, RuntimeGuard, Taskset,
};
pub use hud_types::{
    Capability, EvaluationResult, PromptMessage, Step, SubScore, TaskRow, Trace, TraceStatus,
    PROTOCOL_VERSION,
};

/// Environment authoring + serving (re-export of `hud-env`).
pub mod env {
    pub use hud_env::*;
}

/// Wire primitives (re-export of `hud-wire`).
pub mod wire {
    pub use hud_wire::*;
}
