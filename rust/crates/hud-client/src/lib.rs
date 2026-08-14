//! `HudClient`: JSON-RPC client for the HUD wire protocol (`hud/1.0`).
//!
//! Transport for a served env's control channel: drives `hello` / `tasks.*`
//! and exposes capabilities via [`HudClient::binding`] (wire data). Use
//! [`connect`] to attach to a provisioned substrate with handshake retry.

mod client;
mod connect;
pub mod filetracking;
#[cfg(feature = "ssh")]
pub mod ssh;

pub use client::{HudClient, HudClientError, Manifest, ServerInfo};
pub use connect::{connect, ConnectOptions};
