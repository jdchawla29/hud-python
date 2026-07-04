//! The HUD rollout engine: drive a task to a graded [`Run`] against a
//! provisioned substrate.
//!
//! The execution atom is [`rollout`]; [`Taskset::run`] schedules over it.
//! Placement is a [`Provider`]: bring the env's control channel up anywhere —
//! a local subprocess ([`LocalRuntime`]), a container ([`DockerRuntime`]), or
//! a substrate someone else owns (a bare [`Runtime`]) — and the agent loop
//! drives it from this process.

mod agent;
mod job;
mod run;
mod runtime;
mod taskset;

pub use agent::{agent_fn, Agent, AgentError};
pub use hud_types::{Grade, TaskRow};
pub use job::Job;
pub use run::{rollout, RolloutOptions, Run};
pub use runtime::{
    DockerRuntime, LocalRuntime, Provider, ProviderError, Runtime, RuntimeConfig, RuntimeGpu,
    RuntimeGuard, RuntimeLimits, RuntimeResources,
};
pub use taskset::{RunOptions, Taskset, TasksetError};
