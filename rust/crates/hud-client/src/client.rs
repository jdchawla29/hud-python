//! The JSON-RPC client for a served env's control channel.

use hud_types::{Capability, UrlParts, PROTOCOL_VERSION};
use hud_wire::{read_frame, request, send_frame, splice, RpcError};
use serde_json::{json, Map, Value};
use tokio::io::BufReader;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::{JoinHandle, JoinSet};

#[derive(Debug, thiserror::Error)]
pub enum HudClientError {
    /// The env returned a JSON-RPC error frame.
    #[error(transparent)]
    Rpc(#[from] RpcError),
    /// The peer hung up without answering (e.g. a proxied port whose backend
    /// isn't up) — a connection-level event, not a protocol error.
    #[error("env closed connection during {0:?}")]
    Eof(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("{method:?}: result was not an object")]
    MalformedResult { method: String },
    #[error("call hello() before accessing bindings")]
    NoManifest,
    #[error("no capability {0:?} (available: {1})")]
    UnknownCapability(String, String),
    #[error("ambiguous capability {0:?}; matches: {1}")]
    AmbiguousCapability(String, String),
    #[error("control transport {0:?} not supported yet (only tcp://)")]
    UnsupportedTransport(String),
    #[error("invalid runtime url: {0}")]
    InvalidUrl(String),
    #[error("env not ready within {0:.0?}")]
    NotReady(std::time::Duration),
}

/// Identity of the env serving this session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerInfo {
    pub name: String,
    pub version: String,
}

/// Env welcome frame returned by [`HudClient::hello`].
///
/// `bindings` carry concrete, *client-reachable* connection data: the client
/// transparently forwards any substrate-local (loopback) address through the
/// control port, so a binding's url always works from here.
#[derive(Debug, Clone)]
pub struct Manifest {
    pub session_id: String,
    pub protocol_version: String,
    pub server_info: ServerInfo,
    pub bindings: Vec<Capability>,
}

/// JSON-RPC client for a served env's control channel.
///
/// Prefer [`crate::connect`], which owns readiness (connect → `hello` retry)
/// and returns one of these with `manifest` populated. Task lifecycle wrapping
/// (start → grade) lives in `hud-eval`'s `Run`.
pub struct HudClient {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    /// Control-channel address, for tunnel connections. `None` disables
    /// loopback forwarding (bindings pass through).
    endpoint: Option<(String, u16)>,
    next_id: u64,
    pub manifest: Option<Manifest>,
    forwarders: Vec<JoinHandle<()>>,
}

impl HudClient {
    /// Wrap an already-connected control-channel stream.
    pub fn new(stream: TcpStream, endpoint: Option<(String, u16)>) -> HudClient {
        let (read_half, write_half) = stream.into_split();
        HudClient {
            reader: BufReader::new(read_half),
            writer: write_half,
            endpoint,
            next_id: 0,
            manifest: None,
            forwarders: Vec::new(),
        }
    }

    /// Dial `host:port` and wrap the connection (no handshake yet).
    pub async fn dial(host: &str, port: u16) -> std::io::Result<HudClient> {
        let stream = TcpStream::connect((host, port)).await?;
        Ok(HudClient::new(stream, Some((host.to_string(), port))))
    }

    // ─── handshake ────────────────────────────────────────────────────

    /// Send `hello`; cache and return the parsed [`Manifest`].
    pub async fn hello(&mut self) -> Result<&Manifest, HudClientError> {
        let result = self.call("hello", json!({})).await?;
        let env = result.get("env").and_then(Value::as_object);
        let mut bindings = Vec::new();
        for raw in result
            .get("bindings")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let cap: Capability = serde_json::from_value(raw.clone()).map_err(|e| {
                HudClientError::MalformedResult {
                    method: format!("hello binding: {e}"),
                }
            })?;
            bindings.push(self.reachable(cap).await?);
        }
        self.manifest = Some(Manifest {
            session_id: result
                .get("session_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            protocol_version: PROTOCOL_VERSION.to_string(),
            server_info: ServerInfo {
                name: env
                    .and_then(|e| e.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_string(),
                version: env
                    .and_then(|e| e.get("version"))
                    .and_then(Value::as_str)
                    .unwrap_or("0.0.0")
                    .to_string(),
            },
            bindings,
        });
        Ok(self.manifest.as_ref().expect("just set"))
    }

    // ─── capability tunneling ─────────────────────────────────────────
    //
    // A loopback address in the manifest is the *substrate's* loopback — the
    // daemon lives in its network namespace, which may not be ours. For that
    // case the client runs a local forwarder (`ssh -L` style): each accepted
    // connection is one fresh TCP connection to the control port, opened with
    // a `tunnel.open` preface frame and spliced raw from there.

    async fn reachable(&mut self, cap: Capability) -> Result<Capability, HudClientError> {
        let Some((host, port)) = self.endpoint.clone() else {
            return Ok(cap);
        };
        let Ok(parts) = UrlParts::parse(&cap.url) else {
            return Ok(cap);
        };
        if !parts.is_loopback() {
            return Ok(cap);
        }

        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let local_port = listener.local_addr()?.port();
        let name = cap.name.clone();
        self.forwarders.push(tokio::spawn(async move {
            let mut tunnels: JoinSet<()> = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let Ok((stream, _)) = accepted else { break };
                        let host = host.clone();
                        let name = name.clone();
                        tunnels.spawn(async move {
                            forward(stream, &host, port, &name).await;
                        });
                    }
                    Some(_) = tunnels.join_next(), if !tunnels.is_empty() => {}
                }
            }
            // Dropping the JoinSet aborts live tunnels with the forwarder.
        }));

        let mut cap = cap;
        cap.url = parts.with_address("127.0.0.1", local_port);
        Ok(cap)
    }

    // ─── capability access ────────────────────────────────────────────

    /// Find the capability matching `ref_` (name, protocol family, or
    /// protocol). Returns the wire data — bring your own connection.
    /// Ambiguous refs (multiple matches) error; use names to disambiguate.
    pub fn binding(&self, ref_: &str) -> Result<&Capability, HudClientError> {
        let manifest = self.manifest.as_ref().ok_or(HudClientError::NoManifest)?;
        let matches: Vec<&Capability> = manifest
            .bindings
            .iter()
            .filter(|c| ref_ == c.name || ref_ == c.protocol || ref_ == c.protocol_family())
            .collect();
        let describe = |caps: &[&Capability]| {
            caps.iter()
                .map(|c| format!("{} ({})", c.name, c.protocol))
                .collect::<Vec<_>>()
                .join(", ")
        };
        match matches.len() {
            1 => Ok(matches[0]),
            0 => {
                let available: Vec<&Capability> = manifest.bindings.iter().collect();
                let listed = if available.is_empty() {
                    "<none>".to_string()
                } else {
                    describe(&available)
                };
                Err(HudClientError::UnknownCapability(ref_.to_string(), listed))
            }
            _ => Err(HudClientError::AmbiguousCapability(
                ref_.to_string(),
                describe(&matches),
            )),
        }
    }

    // ─── tasks ────────────────────────────────────────────────────────

    /// `[{id, description, args, ...}, ...]` for every registered task.
    pub async fn list_tasks(&mut self) -> Result<Vec<Value>, HudClientError> {
        let result = self.call("tasks.list", json!({})).await?;
        match result.get("tasks") {
            Some(Value::Array(tasks)) => Ok(tasks.clone()),
            _ => Err(HudClientError::MalformedResult {
                method: "tasks.list: 'tasks' must be a list".to_string(),
            }),
        }
    }

    /// Start a task; returns the first yield (`{"prompt": ...}`).
    pub async fn start_task(
        &mut self,
        task_id: &str,
        args: Map<String, Value>,
    ) -> Result<Map<String, Value>, HudClientError> {
        self.call("tasks.start", json!({"id": task_id, "args": args}))
            .await
    }

    /// Send `tasks.grade`; returns the evaluation dict (`{"score": ...}`).
    pub async fn grade(
        &mut self,
        payload: Map<String, Value>,
    ) -> Result<Map<String, Value>, HudClientError> {
        self.call("tasks.grade", Value::Object(payload)).await
    }

    pub async fn cancel(&mut self) -> Result<(), HudClientError> {
        self.call("tasks.cancel", json!({})).await?;
        Ok(())
    }

    // ─── lifecycle ────────────────────────────────────────────────────

    /// Close the connection and stop forwarders.
    ///
    /// Sends no `bye`: a plain disconnect leaves the env's held session for a
    /// later connection to grade; `grade` itself clears the session.
    pub async fn close(mut self) {
        for forwarder in self.forwarders.drain(..) {
            forwarder.abort();
        }
        use tokio::io::AsyncWriteExt;
        let _ = self.writer.shutdown().await;
    }

    // ─── JSON-RPC plumbing ────────────────────────────────────────────

    async fn call(
        &mut self,
        method: &str,
        params: Value,
    ) -> Result<Map<String, Value>, HudClientError> {
        self.next_id += 1;
        send_frame(&mut self.writer, &request(self.next_id, method, params)).await?;
        let reply = read_frame(&mut self.reader)
            .await?
            .ok_or_else(|| HudClientError::Eof(method.to_string()))?;
        if let Some(error) = reply.get("error") {
            return Err(RpcError::from_frame(error).into());
        }
        match reply.get("result") {
            Some(Value::Object(result)) => Ok(result.clone()),
            _ => Err(HudClientError::MalformedResult {
                method: method.to_string(),
            }),
        }
    }
}

impl Drop for HudClient {
    fn drop(&mut self) {
        for forwarder in self.forwarders.drain(..) {
            forwarder.abort();
        }
    }
}

/// One forwarded connection: dial the control endpoint, send the
/// `tunnel.open` preface, check the reply, then splice raw bytes.
async fn forward(mut client: TcpStream, host: &str, port: u16, capability: &str) {
    let Ok(upstream) = TcpStream::connect((host, port)).await else {
        return;
    };
    let (up_read, mut up_write) = upstream.into_split();
    let mut up_reader = BufReader::new(up_read);
    let preface = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tunnel.open",
        "params": {"capability": capability},
    });
    if send_frame(&mut up_write, &preface).await.is_err() {
        return;
    }
    match read_frame(&mut up_reader).await {
        Ok(Some(opened)) if opened.get("error").is_none() => {}
        other => {
            tracing::warn!(capability, ?other, "tunnel.open refused");
            return;
        }
    }
    let mut upstream = tokio::io::join(up_reader, up_write);
    let _ = splice(&mut client, &mut upstream).await;
}
