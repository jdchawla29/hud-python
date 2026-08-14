//! The protocol server for an [`Environment`] — the substrate side of the
//! runtime contract.
//!
//! Owns per-connection protocol dispatch and serving-time state, and the full
//! serving lifecycle: backing daemons up (start hooks), control channel bound
//! (announcing the port on stdout as `HUD_SERVE_PORT=<port>`), daemons down.
//!
//! The accept point owns the transport's connection grammar: the first frame
//! decides what a connection is. A `tunnel.open` frame opens one capability
//! stream (a single reply, then raw bytes — the CONNECT analog); anything else
//! begins a JSON-RPC control session.

use crate::environment::Environment;
use crate::task::{Answer, TaskInstance};
use hud_types::UrlParts;
use hud_wire::{code, error, read_frame, reply, send_frame, splice};
use serde_json::{json, Map, Value};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncWrite, BufReader};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{watch, Mutex};
use tokio::task::JoinSet;

/// Line a serving process prints once its control channel is bound; spawn
/// providers read it from the child's stdout.
pub const PORT_ANNOUNCEMENT: &str = "HUD_SERVE_PORT=";

/// Serving-time state shared by every connection of one bound server: at most
/// one suspended task at a time. `tasks.start` replaces it, `tasks.grade`
/// consumes it, `tasks.cancel` clears it, and a connection drop leaves it in
/// place — the split start/grade flow (a verifier reconnecting to grade).
struct Shared {
    env: Arc<Environment>,
    runner: Mutex<Option<(String, Box<dyn TaskInstance>)>>,
}

/// A bound, serving control channel. Dropped or [`BoundServer::shutdown`],
/// it stops accepting and aborts live connection handlers.
pub struct BoundServer {
    addr: SocketAddr,
    stop: watch::Sender<bool>,
    accept_loop: tokio::task::JoinHandle<()>,
}

impl BoundServer {
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    /// Stop accepting and cancel live connection handlers.
    pub async fn shutdown(self) {
        let _ = self.stop.send(true);
        let _ = self.accept_loop.await;
    }
}

/// Bind a control-channel server for `env` (already started) and begin serving.
///
/// Callers read the assigned port from [`BoundServer::addr`].
pub async fn bind(env: Arc<Environment>, host: &str, port: u16) -> std::io::Result<BoundServer> {
    let listener = TcpListener::bind((host, port)).await?;
    let addr = listener.local_addr()?;
    let shared = Arc::new(Shared {
        env,
        runner: Mutex::new(None),
    });
    let (stop, mut stopped) = watch::channel(false);

    let accept_loop = tokio::spawn(async move {
        // Live connection handlers, so teardown cancels them instead of
        // abandoning mid-splice tunnels.
        let mut handlers: JoinSet<()> = JoinSet::new();
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let Ok((stream, _)) = accepted else { break };
                    let shared = Arc::clone(&shared);
                    handlers.spawn(handle_connection(shared, stream));
                }
                _ = stopped.changed() => break,
                // Reap finished handlers so the set doesn't grow unbounded.
                Some(_) = handlers.join_next(), if !handlers.is_empty() => {}
            }
        }
        handlers.shutdown().await;
    });

    tracing::info!(%addr, "env bound");
    Ok(BoundServer {
        addr,
        stop,
        accept_loop,
    })
}

/// Start `env`'s daemons and serve its control channel until SIGTERM/ctrl-c.
///
/// The full serving lifecycle, and the analog of
/// `python -m hud.environment.server`: prints `HUD_SERVE_PORT=<port>` to
/// stdout once bound so spawn providers can read it.
pub async fn serve(
    mut env: Environment,
    host: &str,
    port: u16,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    env.start().await?;
    let env = Arc::new(env);
    let server = match bind(Arc::clone(&env), host, port).await {
        Ok(server) => server,
        Err(e) => {
            env.stop().await;
            return Err(e.into());
        }
    };
    println!("{}{}", PORT_ANNOUNCEMENT, server.port());
    use std::io::Write;
    let _ = std::io::stdout().flush();

    wait_for_termination().await;
    server.shutdown().await;
    env.stop().await;
    Ok(())
}

async fn wait_for_termination() {
    #[cfg(unix)]
    {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("SIGTERM handler installs");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = sigterm.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

async fn handle_connection(shared: Arc<Shared>, stream: TcpStream) {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let Ok(Some(first)) = read_frame(&mut reader).await else {
        return;
    };
    if first.get("method").and_then(Value::as_str) == Some("tunnel.open") {
        tunnel_stream(&shared, &first, reader, write_half).await;
    } else {
        control_session(&shared, first, &mut reader, &mut write_half).await;
    }
    // Writer half drops here, closing the connection.
}

/// One capability stream: dial the resolved daemon and splice raw bytes. The
/// client opens one such connection per capability stream, so the control port
/// is the only address a substrate ever needs to expose.
async fn tunnel_stream(
    shared: &Shared,
    msg: &Value,
    reader: BufReader<OwnedReadHalf>,
    mut write_half: OwnedWriteHalf,
) {
    let msg_id = msg.get("id").cloned();
    let refuse = |code: i64, message: String| {
        let msg_id = msg_id.clone();
        async move {
            tracing::warn!(message, "refusing capability stream");
            (msg_id, code, message)
        }
    };

    let name = msg
        .get("params")
        .and_then(Value::as_object)
        .and_then(|p| p.get("capability"))
        .and_then(Value::as_str);
    let outcome = match name {
        None => Err(refuse(
            code::INVALID_PARAMS,
            "tunnel.open: 'capability' must be a string".to_string(),
        )
        .await),
        Some(name) => match shared.env.find_capability(name) {
            Err(e) => Err(refuse(code::SERVER_ERROR, e.to_string()).await),
            Ok(cap) => match UrlParts::parse(&cap.url) {
                Ok(parts) if parts.port.is_some() => {
                    match TcpStream::connect((parts.host.as_str(), parts.port.unwrap())).await {
                        Ok(backend) => Ok((name.to_string(), backend)),
                        Err(e) => Err(refuse(code::SERVER_ERROR, e.to_string()).await),
                    }
                }
                _ => Err(refuse(
                    code::INVALID_PARAMS,
                    format!("capability '{name}' has no host:port to tunnel to"),
                )
                .await),
            },
        },
    };

    match outcome {
        Err((msg_id, code, message)) => {
            if let Some(id) = msg_id {
                let _ = send_frame(&mut write_half, &error(id, code, &message)).await;
            }
        }
        Ok((name, mut backend)) => {
            if let Some(id) = msg_id {
                if send_frame(&mut write_half, &reply(id, json!({"capability": name})))
                    .await
                    .is_err()
                {
                    return;
                }
            }
            let mut client = tokio::io::join(reader, write_half);
            let _ = splice(&mut client, &mut backend).await;
        }
    }
}

/// One control session: JSON-RPC dispatch for the connection's lifetime.
async fn control_session(
    shared: &Shared,
    first: Value,
    reader: &mut BufReader<OwnedReadHalf>,
    writer: &mut OwnedWriteHalf,
) {
    let session_id = format!("sess-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
    let mut msg = Some(first);
    while let Some(frame) = msg.take() {
        let method = frame.get("method").and_then(Value::as_str).unwrap_or("");
        let params = frame
            .get("params")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let msg_id = frame.get("id").cloned();

        let outcome = dispatch(shared, &session_id, method, params).await;
        match outcome {
            Dispatch::Reply(result) => {
                if reply_to(writer, msg_id, result).await.is_err() {
                    return;
                }
            }
            Dispatch::Error(code, message) => {
                if error_to(writer, msg_id, code, &message).await.is_err() {
                    return;
                }
            }
            Dispatch::Bye(result) => {
                let _ = reply_to(writer, msg_id, result).await;
                return;
            }
        }

        msg = match read_frame(reader).await {
            Ok(next) => next,
            Err(_) => return,
        };
    }
}

enum Dispatch {
    Reply(Value),
    Error(i64, String),
    Bye(Value),
}

async fn dispatch(
    shared: &Shared,
    session_id: &str,
    method: &str,
    params: Map<String, Value>,
) -> Dispatch {
    match method {
        "hello" => {
            // Start hooks ran before serving, so hook-published capabilities
            // (e.g. a workspace's ssh address) are already concrete here.
            let env = &shared.env;
            Dispatch::Reply(json!({
                "session_id": session_id,
                "env": {"name": env.name, "version": env.version},
                "bindings": env.capabilities(),
            }))
        }
        "tasks.list" => {
            let tasks: Vec<Value> = shared.env.templates().map(|t| t.manifest_entry()).collect();
            Dispatch::Reply(json!({"tasks": tasks}))
        }
        "tasks.start" => {
            let Some(task_id) = params.get("id").and_then(Value::as_str) else {
                return Dispatch::Error(
                    code::INVALID_PARAMS,
                    "tasks.start: 'id' must be a string".to_string(),
                );
            };
            let args = match params.get("args") {
                None | Some(Value::Null) => Map::new(),
                Some(Value::Object(args)) => args.clone(),
                Some(_) => {
                    return Dispatch::Error(
                        code::INVALID_PARAMS,
                        "tasks.start: 'args' must be an object".to_string(),
                    );
                }
            };
            let Some(template) = shared.env.find_template(task_id) else {
                return Dispatch::Error(code::INVALID_PARAMS, format!("unknown task: '{task_id}'"));
            };

            let mut runner = shared.runner.lock().await;
            if let Some((_, mut previous)) = runner.take() {
                previous.cancel().await;
            }
            let mut instance = match template.create(args) {
                Ok(instance) => instance,
                Err(e) => return Dispatch::Error(code::SERVER_ERROR, e.to_string()),
            };
            match instance.start().await {
                Ok(prompt) => {
                    *runner = Some((task_id.to_string(), instance));
                    Dispatch::Reply(Value::Object(prompt.into_frame()))
                }
                Err(e) => Dispatch::Error(code::SERVER_ERROR, e.to_string()),
            }
        }
        "tasks.grade" => {
            let taken = shared.runner.lock().await.take();
            let Some((task_id, mut instance)) = taken else {
                return Dispatch::Error(code::INVALID_REQUEST, "no task in progress".to_string());
            };
            let graded = instance.grade(Answer::from_payload(params)).await;
            instance.cancel().await;
            match graded {
                Ok(evaluation) => match evaluation.into_frame(&task_id) {
                    Ok(frame) => Dispatch::Reply(Value::Object(frame)),
                    Err(message) => Dispatch::Error(code::SERVER_ERROR, message),
                },
                Err(e) => Dispatch::Error(code::SERVER_ERROR, e.to_string()),
            }
        }
        "tasks.cancel" => {
            if let Some((_, mut instance)) = shared.runner.lock().await.take() {
                instance.cancel().await;
            }
            Dispatch::Reply(json!({"cancelled": true}))
        }
        "bye" => {
            if let Some((_, mut instance)) = shared.runner.lock().await.take() {
                instance.cancel().await;
            }
            Dispatch::Bye(json!({"goodbye": true}))
        }
        other => Dispatch::Error(code::METHOD_NOT_FOUND, format!("method not found: {other}")),
    }
}

async fn reply_to<W: AsyncWrite + Unpin>(
    writer: &mut W,
    msg_id: Option<Value>,
    result: Value,
) -> std::io::Result<()> {
    match msg_id {
        Some(id) if !id.is_null() => send_frame(writer, &reply(id, result)).await,
        _ => Ok(()),
    }
}

async fn error_to<W: AsyncWrite + Unpin>(
    writer: &mut W,
    msg_id: Option<Value>,
    code: i64,
    message: &str,
) -> std::io::Result<()> {
    match msg_id {
        Some(id) if !id.is_null() => send_frame(writer, &error(id, code, message)).await,
        _ => Ok(()),
    }
}
