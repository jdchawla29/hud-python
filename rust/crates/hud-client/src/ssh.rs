//! `ssh/2` capability client (feature `ssh`).
//!
//! Dials the daemon a `Capability` publishes (typically a workspace's SSH
//! server, reached through the client's tunnel forwarder), authenticates with
//! the client key carried in the capability params, and runs commands.

use hud_types::{Capability, UrlParts};
use russh::client::{self, Config};
use russh::keys::{decode_secret_key, PrivateKeyWithHashAlg};
use russh::ChannelMsg;
use std::sync::Arc;

#[derive(Debug, thiserror::Error)]
pub enum SshError {
    #[error("invalid ssh capability url {0:?}")]
    InvalidUrl(String),
    #[error("ssh capability has no client key (params.client_key/client_key_path)")]
    NoClientKey,
    #[error("ssh authentication failed for user {0:?}")]
    AuthFailed(String),
    #[error(transparent)]
    Russh(#[from] russh::Error),
    #[error(transparent)]
    Key(#[from] russh::keys::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Result of one executed command.
#[derive(Debug, Clone)]
pub struct ExecResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_status: u32,
}

impl ExecResult {
    pub fn success(&self) -> bool {
        self.exit_status == 0
    }
}

struct AcceptAll;

impl client::Handler for AcceptAll {
    type Error = russh::Error;

    // The host key is published out-of-band in the capability manifest
    // (params.host_pubkey), and connections ride the authenticated control
    // tunnel — matching the Python client's `known_hosts=None`.
    async fn check_server_key(
        &mut self,
        _server_public_key: &russh::keys::PublicKey,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }
}

/// Live connection to an `ssh/2` capability.
///
/// Commands multiplex as channels over one session, so an `Arc<SshClient>`
/// can serve many concurrent `run` calls.
pub struct SshClient {
    session: client::Handle<AcceptAll>,
}

impl SshClient {
    /// Connect and authenticate using the capability's wire params
    /// (`user`, `client_key` content or `client_key_path`).
    pub async fn connect(cap: &Capability) -> Result<SshClient, SshError> {
        let parts = UrlParts::parse(&cap.url).map_err(|_| SshError::InvalidUrl(cap.url.clone()))?;
        let port = parts
            .port
            .ok_or_else(|| SshError::InvalidUrl(cap.url.clone()))?;

        let user = cap
            .params
            .get("user")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .or(parts.userinfo.clone())
            .unwrap_or_else(|| "agent".to_string());
        let key_pem = match cap
            .params
            .get("client_key")
            .and_then(serde_json::Value::as_str)
        {
            Some(content) => content.to_string(),
            None => {
                let path = cap
                    .params
                    .get("client_key_path")
                    .and_then(serde_json::Value::as_str)
                    .ok_or(SshError::NoClientKey)?;
                std::fs::read_to_string(path)?
            }
        };
        let key = decode_secret_key(&key_pem, None)?;

        let config = Arc::new(Config::default());
        let mut session = client::connect(config, (parts.host.as_str(), port), AcceptAll).await?;
        let auth = session
            .authenticate_publickey(&user, PrivateKeyWithHashAlg::new(Arc::new(key), None))
            .await?;
        if !auth.success() {
            return Err(SshError::AuthFailed(user));
        }
        Ok(SshClient { session })
    }

    /// Run one command; collects stdout/stderr and the exit status.
    pub async fn run(&self, command: &str) -> Result<ExecResult, SshError> {
        let mut channel = self.session.channel_open_session().await?;
        channel.exec(true, command).await?;

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut exit_status = 0u32;
        while let Some(msg) = channel.wait().await {
            match msg {
                ChannelMsg::Data { ref data } => stdout.extend_from_slice(data),
                ChannelMsg::ExtendedData { ref data, ext: 1 } => stderr.extend_from_slice(data),
                ChannelMsg::ExitStatus { exit_status: code } => exit_status = code,
                _ => {}
            }
        }
        Ok(ExecResult {
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
            exit_status,
        })
    }

    /// Close the underlying session.
    pub async fn close(&self) {
        let _ = self
            .session
            .disconnect(russh::Disconnect::ByApplication, "", "en")
            .await;
    }
}
