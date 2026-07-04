//! Capability declarations: `(name, protocol, url, params)`.

use crate::url::{UrlError, UrlParts};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Concrete wire data for one slice of env access — what the manifest
/// publishes and what a capability client dials.
///
/// Serde (de)serialization is the manifest codec: field names are the wire keys.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Capability {
    pub name: String,
    /// Versioned protocol id, e.g. `ssh/2`, `cdp/1.3`, `rfb/3.8`, `mcp/2025-11-25`.
    pub protocol: String,
    pub url: String,
    #[serde(default)]
    pub params: Map<String, Value>,
}

/// Coerce shorthand `host[:port]` into a full `scheme://host:port[/path]` URL
/// (the Python SDK's `normalize_url`).
pub fn normalize_url(
    url: &str,
    default_scheme: &str,
    default_port: Option<u16>,
) -> Result<String, UrlError> {
    let full = if url.contains("://") {
        url.to_string()
    } else {
        format!("{default_scheme}://{url}")
    };
    let mut parts = UrlParts::parse(&full)?;
    if parts.port.is_none() {
        parts.port = default_port;
    }
    Ok(parts.to_url())
}

impl Capability {
    pub fn new(
        name: impl Into<String>,
        protocol: impl Into<String>,
        url: impl Into<String>,
        params: Map<String, Value>,
    ) -> Self {
        Capability {
            name: name.into(),
            protocol: protocol.into(),
            url: url.into(),
            params,
        }
    }

    /// The protocol family (the part before `/`), e.g. `ssh` for `ssh/2`.
    pub fn protocol_family(&self) -> &str {
        self.protocol.split('/').next().unwrap_or(&self.protocol)
    }

    /// `ssh/2` — SSH daemon with publickey auth.
    pub fn ssh(url: &str, host_pubkey: &str) -> Result<SshCapability, UrlError> {
        Ok(SshCapability {
            name: "shell".to_string(),
            url: normalize_url(url, "ssh", Some(22))?,
            user: "agent".to_string(),
            host_pubkey: host_pubkey.to_string(),
            client_key: None,
            client_key_path: None,
            shell: "bash".to_string(),
        })
    }

    /// `cdp/1.3` — Chromium DevTools over WebSocket.
    pub fn cdp(url: &str, target_id: Option<&str>) -> Result<Capability, UrlError> {
        let mut params = Map::new();
        if let Some(target) = target_id {
            params.insert("target_id".to_string(), Value::String(target.to_string()));
        }
        Ok(Capability::new(
            "browser",
            "cdp/1.3",
            normalize_url(url, "ws", Some(9222))?,
            params,
        ))
    }

    /// `rfb/3.8` — VNC/RFB pixel + HID server. Display `n` defaults to port `5900 + n`.
    pub fn rfb(url: &str, display: u16, password: Option<&str>) -> Result<Capability, UrlError> {
        let mut params = Map::new();
        params.insert("display".to_string(), Value::from(display));
        if let Some(password) = password {
            params.insert("password".to_string(), Value::String(password.to_string()));
        }
        Ok(Capability::new(
            "screen",
            "rfb/3.8",
            normalize_url(url, "rfb", Some(5900 + display))?,
            params,
        ))
    }

    /// `mcp/2025-11-25` — MCP server over ws/wss/http/https (no stdio).
    pub fn mcp(url: &str, auth_token: Option<&str>) -> Result<Capability, CapabilityError> {
        let normalized = normalize_url(url, "ws", None)?;
        let scheme = UrlParts::parse(&normalized)?.scheme;
        if !matches!(scheme.as_str(), "ws" | "wss" | "http" | "https") {
            return Err(CapabilityError::UnsupportedMcpScheme(scheme));
        }
        let mut params = Map::new();
        if let Some(token) = auth_token {
            params.insert("auth_token".to_string(), Value::String(token.to_string()));
        }
        Ok(Capability::new(
            "tools",
            "mcp/2025-11-25",
            normalized,
            params,
        ))
    }

    /// Plain TCP service with no dedicated protocol client (still tunnelable).
    pub fn tcp(name: &str, url: &str, protocol: &str) -> Result<Capability, UrlError> {
        Ok(Capability::new(
            name,
            protocol,
            normalize_url(url, "tcp", None)?,
            Map::new(),
        ))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CapabilityError {
    #[error(transparent)]
    Url(#[from] UrlError),
    #[error("mcp/2025-11-25: only ws/wss/http/https URLs are supported, got {0:?}")]
    UnsupportedMcpScheme(String),
}

/// Builder for `ssh/2` capabilities (several optional params).
#[derive(Debug, Clone)]
pub struct SshCapability {
    name: String,
    url: String,
    user: String,
    host_pubkey: String,
    client_key: Option<String>,
    client_key_path: Option<String>,
    shell: String,
}

impl SshCapability {
    pub fn name(mut self, name: &str) -> Self {
        self.name = name.to_string();
        self
    }

    pub fn user(mut self, user: &str) -> Self {
        self.user = user.to_string();
        self
    }

    /// Private key *content* (valid in any network namespace).
    pub fn client_key(mut self, key: &str) -> Self {
        self.client_key = Some(key.to_string());
        self
    }

    /// Path to a key file (only works when client and daemon share a filesystem).
    pub fn client_key_path(mut self, path: &str) -> Self {
        self.client_key_path = Some(path.to_string());
        self
    }

    /// Remote shell type: `bash`, `powershell`, or `cmd`.
    pub fn shell(mut self, shell: &str) -> Self {
        self.shell = shell.to_string();
        self
    }

    pub fn build(self) -> Capability {
        let mut params = Map::new();
        params.insert("user".to_string(), Value::String(self.user));
        params.insert("host_pubkey".to_string(), Value::String(self.host_pubkey));
        params.insert("shell".to_string(), Value::String(self.shell));
        if let Some(key) = self.client_key {
            params.insert("client_key".to_string(), Value::String(key));
        }
        if let Some(path) = self.client_key_path {
            params.insert("client_key_path".to_string(), Value::String(path));
        }
        Capability::new(self.name, "ssh/2", self.url, params)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn manifest_roundtrip_matches_python_shape() {
        let cap = Capability::ssh("10.0.0.5", "ssh-ed25519 AAAA")
            .unwrap()
            .build();
        let manifest = serde_json::to_value(&cap).unwrap();
        assert_eq!(
            manifest,
            json!({
                "name": "shell",
                "protocol": "ssh/2",
                "url": "ssh://10.0.0.5:22",
                "params": {"user": "agent", "host_pubkey": "ssh-ed25519 AAAA", "shell": "bash"},
            })
        );
        let back: Capability = serde_json::from_value(manifest).unwrap();
        assert_eq!(back, cap);
    }

    #[test]
    fn from_manifest_defaults_params() {
        let cap: Capability = serde_json::from_value(json!({
            "name": "svc", "protocol": "x/1", "url": "tcp://127.0.0.1:9"
        }))
        .unwrap();
        assert!(cap.params.is_empty());
        assert_eq!(cap.protocol_family(), "x");
    }

    #[test]
    fn normalize_url_adds_scheme_and_port() {
        assert_eq!(
            normalize_url("example.com", "ssh", Some(22)).unwrap(),
            "ssh://example.com:22"
        );
        assert_eq!(
            normalize_url("ws://example.com/mcp", "ws", None).unwrap(),
            "ws://example.com/mcp"
        );
        assert_eq!(
            normalize_url("user@host:2222", "ssh", Some(22)).unwrap(),
            "ssh://user@host:2222"
        );
    }

    #[test]
    fn mcp_rejects_non_websocket_schemes() {
        assert!(Capability::mcp("tcp://host:1", None).is_err());
        assert!(Capability::mcp("wss://host/mcp", None).is_ok());
    }
}
