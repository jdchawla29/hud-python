//! Cross-language interop against the reference Python SDK, both directions:
//! Rust client ↔ Python server, and Python client ↔ Rust server.
//!
//! The Python SDK is this repo's root project (the Rust workspace lives under
//! `rust/`), so the tests run against it directly via `uv run --project`.

use hud_client::{connect, ConnectOptions};
use hud_env::{template, Environment, Evaluation, Prompt};
use hud_types::Capability;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

/// The hud-python project root: this repo.
fn python_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..")
}

fn interop_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/interop")
}

fn obj(value: Value) -> Map<String, Value> {
    value.as_object().unwrap().clone()
}

#[tokio::test]
async fn rust_client_drives_python_server() {
    let python_dir = python_dir();
    let fixture = interop_dir().join("fixture_env.py");

    let mut child = tokio::process::Command::new("uv")
        .args(["run", "--project"])
        .arg(&python_dir)
        .args(["python", "-m", "hud.environment.server"])
        .arg(&fixture)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn uv run python -m hud.environment.server");

    // Read HUD_SERVE_PORT=<port> from the child's stdout.
    let stdout = child.stdout.take().unwrap();
    let port = tokio::time::timeout(Duration::from_secs(120), async {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if let Some(port) = line.trim().strip_prefix("HUD_SERVE_PORT=") {
                return port.trim().parse::<u16>().ok();
            }
        }
        None
    })
    .await
    .expect("python env served within 120s")
    .expect("port announcement");

    let mut client = connect(
        &format!("tcp://127.0.0.1:{port}"),
        ConnectOptions::default(),
    )
    .await
    .expect("connect to python env");

    // Manifest shape.
    let manifest = client.manifest.as_ref().unwrap();
    assert_eq!(manifest.server_info.name, "echo-env");
    assert_eq!(manifest.server_info.version, "0.1.0");
    assert!(manifest.session_id.starts_with("sess-"));

    // Task manifest from the Python signature-derived schema.
    let tasks = client.list_tasks().await.unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0]["id"], "echo");
    assert_eq!(tasks[0]["description"], "Repeat the given text exactly.");
    assert_eq!(tasks[0]["args"]["properties"]["text"]["type"], "string");

    // Full task lifecycle.
    let started = client
        .start_task("echo", obj(json!({"text": "bonjour"})))
        .await
        .unwrap();
    assert_eq!(started["prompt"], "Repeat exactly: bonjour");
    let graded = client
        .grade(obj(json!({"answer": "bonjour"})))
        .await
        .unwrap();
    assert_eq!(graded["score"], 1.0);

    // Error shapes match the reference server.
    let err = client.grade(obj(json!({"answer": "x"}))).await.unwrap_err();
    match err {
        hud_client::HudClientError::Rpc(e) => {
            assert_eq!(e.code, -32600);
            assert_eq!(e.message, "no task in progress");
        }
        other => panic!("expected rpc error, got {other:?}"),
    }

    // Tunnel bytes through the Python server's control port.
    let binding = client.binding("echo-bytes").unwrap().clone();
    let parts = hud_types::UrlParts::parse(&binding.url).unwrap();
    let mut stream = tokio::net::TcpStream::connect((parts.host.as_str(), parts.port.unwrap()))
        .await
        .unwrap();
    stream.write_all(b"across languages").await.unwrap();
    let mut buf = [0u8; 16];
    stream.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"across languages");

    client.close().await;
    let _ = child.kill().await;
}

#[derive(Deserialize)]
struct EchoArgs {
    text: String,
}

#[tokio::test]
async fn python_client_drives_rust_server() {
    let python_dir = python_dir();

    // The same env shape as the Python fixture, served from Rust.
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
    env.on_start(|| async {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
        let addr = listener.local_addr()?;
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
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

    let script = interop_dir().join("py_client_vs_rust_server.py");
    let output = tokio::time::timeout(
        Duration::from_secs(120),
        tokio::process::Command::new("uv")
            .args(["run", "--project"])
            .arg(&python_dir)
            .arg("python")
            .arg(&script)
            .arg(server.port().to_string())
            .output(),
    )
    .await
    .expect("python client finished within 120s")
    .expect("spawn uv run python");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success() && stdout.contains("PY-INTEROP-OK"),
        "python client failed:\nstdout:\n{stdout}\nstderr:\n{stderr}",
    );

    server.shutdown().await;
}
