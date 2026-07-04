//! Connect to a provisioned substrate's control channel, retrying until ready.

use crate::client::{HudClient, HudClientError};
use hud_types::UrlParts;
use std::time::Duration;
use tokio::time::Instant;

#[derive(Debug, Clone)]
pub struct ConnectOptions {
    /// How long to retry connect + handshake until the env is serving.
    pub ready_timeout: Duration,
    pub retry_interval: Duration,
}

impl Default for ConnectOptions {
    fn default() -> Self {
        ConnectOptions {
            ready_timeout: Duration::from_secs(240),
            retry_interval: Duration::from_millis(500),
        }
    }
}

/// Connect a [`HudClient`] to a control channel by URL (`tcp://host:port`)
/// and complete `hello`, retrying until the env is ready.
///
/// Readiness is protocol-level, and the client owns waiting for it: a
/// freshly-provisioned substrate may refuse the connect, and a proxied port
/// (`docker -p`, a port-forward) can *accept* before the env behind it is
/// serving — that connection just dies at the handshake. Both mean
/// not-ready-yet. Returns a client whose `manifest` is populated. Does not
/// tear the substrate down — lifecycle belongs to whichever provider brought
/// it up.
pub async fn connect(url: &str, options: ConnectOptions) -> Result<HudClient, HudClientError> {
    let parts =
        UrlParts::parse(url).map_err(|e| HudClientError::InvalidUrl(format!("{url:?}: {e}")))?;
    if parts.scheme != "tcp" {
        return Err(HudClientError::UnsupportedTransport(parts.scheme));
    }
    let host = parts.host.clone();
    let port = parts
        .port
        .ok_or_else(|| HudClientError::InvalidUrl(format!("{url:?}: no port")))?;

    let deadline = Instant::now() + options.ready_timeout;
    loop {
        match try_handshake(&host, port).await {
            Ok(client) => return Ok(client),
            Err(e) => {
                if Instant::now() >= deadline {
                    return Err(match e {
                        HudClientError::Io(_) | HudClientError::Eof(_) => {
                            HudClientError::NotReady(options.ready_timeout)
                        }
                        other => other,
                    });
                }
                match e {
                    // Not-ready-yet shapes: refused connect, EOF/reset racing
                    // the hello. Anything else (an error frame) is a real
                    // protocol failure and propagates.
                    HudClientError::Io(_) | HudClientError::Eof(_) => {
                        tokio::time::sleep(options.retry_interval).await;
                    }
                    other => return Err(other),
                }
            }
        }
    }
}

async fn try_handshake(host: &str, port: u16) -> Result<HudClient, HudClientError> {
    let mut client = HudClient::dial(host, port).await?;
    match client.hello().await {
        Ok(_) => Ok(client),
        Err(e) => {
            client.close().await;
            Err(e)
        }
    }
}
