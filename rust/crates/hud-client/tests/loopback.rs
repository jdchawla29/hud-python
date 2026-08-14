//! Rust client ↔ Rust server over a real TCP loopback: the full control
//! session (hello, tasks.*), session-state semantics across connections, and
//! capability tunneling.

use hud_client::{connect, ConnectOptions, HudClient, HudClientError};
use hud_env::{template, BoundServer, Environment, Evaluation, Prompt};
use hud_types::Capability;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[derive(Deserialize)]
struct EchoArgs {
    text: String,
}

async fn serve_echo_env() -> (BoundServer, String) {
    let mut env = Environment::new("echo-env").version("0.1.0");
    env.add_template(template(
        "echo",
        "Repeat the given text exactly.",
        |args: EchoArgs| async move {
            let expected = args.text.clone();
            Ok((
                Prompt::Text(format!("Repeat exactly: {}", args.text)),
                expected,
            ))
        },
        |expected: String, answer| async move {
            Ok(Evaluation::Score(if answer.text().trim() == expected {
                1.0
            } else {
                0.0
            }))
        },
    ));

    // A loopback byte-echo daemon published as a capability, to exercise
    // tunnel.open through the control port.
    env.on_start(|| async {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let addr = listener.local_addr()?;
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = [0u8; 1024];
                    while let Ok(n) = stream.read(&mut buf).await {
                        if n == 0 || stream.write_all(&buf[..n]).await.is_err() {
                            break;
                        }
                    }
                });
            }
        });
        Ok(vec![Capability::tcp(
            "echo-bytes",
            &format!("127.0.0.1:{}", addr.port()),
            "raw/1",
        )?])
    });

    let mut env = env;
    env.start().await.unwrap();
    let server = hud_env::bind(Arc::new(env), "127.0.0.1", 0).await.unwrap();
    let url = format!("tcp://127.0.0.1:{}", server.port());
    (server, url)
}

fn obj(value: Value) -> Map<String, Value> {
    value.as_object().unwrap().clone()
}

#[tokio::test]
async fn full_control_session() {
    let (_server, url) = serve_echo_env().await;
    let mut client = connect(&url, ConnectOptions::default()).await.unwrap();

    let manifest = client.manifest.as_ref().unwrap();
    assert!(manifest.session_id.starts_with("sess-"));
    assert_eq!(manifest.protocol_version, "hud/1.0");
    assert_eq!(manifest.server_info.name, "echo-env");
    assert_eq!(manifest.server_info.version, "0.1.0");

    let tasks = client.list_tasks().await.unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0]["id"], "echo");
    assert!(tasks[0]["args"].is_object());

    let started = client
        .start_task("echo", obj(json!({"text": "hello"})))
        .await
        .unwrap();
    assert_eq!(started["prompt"], "Repeat exactly: hello");

    let graded = client.grade(obj(json!({"answer": "hello"}))).await.unwrap();
    assert_eq!(graded["score"], 1.0);

    // Grading consumed the session.
    let err = client.grade(obj(json!({"answer": "x"}))).await.unwrap_err();
    match err {
        HudClientError::Rpc(e) => {
            assert_eq!(e.code, -32600);
            assert_eq!(e.message, "no task in progress");
        }
        other => panic!("expected rpc error, got {other:?}"),
    }

    // Unknown method and unknown task shapes.
    let err = client.start_task("nope", Map::new()).await.unwrap_err();
    match err {
        HudClientError::Rpc(e) => {
            assert_eq!(e.code, -32602);
            assert_eq!(e.message, "unknown task: 'nope'");
        }
        other => panic!("expected rpc error, got {other:?}"),
    }

    client.close().await;
}

#[tokio::test]
async fn disconnect_leaves_session_for_regrade() {
    // The split start/grade flow: one connection starts, disconnects without
    // `bye`; a second connection grades the held session.
    let (_server, url) = serve_echo_env().await;

    let mut first = connect(&url, ConnectOptions::default()).await.unwrap();
    first
        .start_task("echo", obj(json!({"text": "persist"})))
        .await
        .unwrap();
    first.close().await;

    let mut second = connect(&url, ConnectOptions::default()).await.unwrap();
    let graded = second
        .grade(obj(json!({"answer": "persist"})))
        .await
        .unwrap();
    assert_eq!(graded["score"], 1.0);
    second.close().await;
}

#[tokio::test]
async fn cancel_clears_session() {
    let (_server, url) = serve_echo_env().await;
    let mut client: HudClient = connect(&url, ConnectOptions::default()).await.unwrap();
    client
        .start_task("echo", obj(json!({"text": "x"})))
        .await
        .unwrap();
    client.cancel().await.unwrap();
    let err = client.grade(obj(json!({"answer": "x"}))).await.unwrap_err();
    assert!(matches!(err, HudClientError::Rpc(e) if e.code == -32600));
    client.close().await;
}

#[tokio::test]
async fn loopback_binding_tunnels_through_control_port() {
    let (_server, url) = serve_echo_env().await;
    let client = connect(&url, ConnectOptions::default()).await.unwrap();

    // The env's loopback daemon must have been rewritten to a local forwarder
    // address (a different port than the daemon's own).
    let binding = client.binding("echo-bytes").unwrap().clone();
    assert!(binding.url.starts_with("tcp://127.0.0.1:"));

    // Resolution by protocol and family also works.
    assert_eq!(client.binding("raw/1").unwrap().name, "echo-bytes");
    assert_eq!(client.binding("raw").unwrap().name, "echo-bytes");
    assert!(matches!(
        client.binding("nope"),
        Err(HudClientError::UnknownCapability(..))
    ));

    // Bytes echo through: client -> forwarder -> control port (tunnel.open)
    // -> daemon and back.
    let parts = hud_types::UrlParts::parse(&binding.url).unwrap();
    let mut stream = tokio::net::TcpStream::connect((parts.host.as_str(), parts.port.unwrap()))
        .await
        .unwrap();
    stream.write_all(b"ping through the tunnel").await.unwrap();
    let mut buf = [0u8; 23];
    stream.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"ping through the tunnel");

    client.close().await;
}

#[tokio::test]
async fn connect_retries_until_ready() {
    // Nothing is listening yet; bind the env after a delay on a known port.
    let placeholder = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = placeholder.local_addr().unwrap().port();
    drop(placeholder);

    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(700)).await;
        let mut env = Environment::new("late-env");
        env.add_template(template(
            "noop",
            "",
            |_: Map<String, Value>| async move { Ok((Prompt::Text("go".into()), ())) },
            |_: (), _| async move { Ok(Evaluation::Score(1.0)) },
        ));
        env.start().await.unwrap();
        let server = hud_env::bind(Arc::new(env), "127.0.0.1", port)
            .await
            .unwrap();
        // Keep serving until the test process exits.
        std::mem::forget(server);
    });

    let client = connect(
        &format!("tcp://127.0.0.1:{port}"),
        ConnectOptions {
            ready_timeout: std::time::Duration::from_secs(10),
            retry_interval: std::time::Duration::from_millis(100),
        },
    )
    .await
    .unwrap();
    assert_eq!(
        client.manifest.as_ref().unwrap().server_info.name,
        "late-env"
    );
    client.close().await;
}

#[tokio::test]
async fn connect_rejects_non_tcp() {
    match connect("ws://127.0.0.1:1", ConnectOptions::default()).await {
        Err(HudClientError::UnsupportedTransport(s)) => assert_eq!(s, "ws"),
        Err(other) => panic!("unexpected error: {other:?}"),
        Ok(_) => panic!("expected transport error"),
    }
}

#[tokio::test]
async fn raw_client_dial_works_without_manifest() {
    let (_server, url) = serve_echo_env().await;
    let parts = hud_types::UrlParts::parse(&url).unwrap();
    let mut client = HudClient::dial(&parts.host, parts.port.unwrap())
        .await
        .unwrap();
    assert!(matches!(
        client.binding("x"),
        Err(HudClientError::NoManifest)
    ));
    client.hello().await.unwrap();
    assert!(client.binding("echo-bytes").is_ok());
    client.close().await;
}
