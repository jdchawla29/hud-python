//! `ssh/2` client against a real Python workspace env (asyncssh server),
//! reached through the control-port tunnel forwarder. The Python SDK is this
//! repo's root project.
//!
//! ```sh
//! cargo test -p hud-client --features ssh --test ssh_interop
//! ```

#![cfg(feature = "ssh")]

use hud_client::ssh::SshClient;
use hud_client::{connect, ConnectOptions};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};

#[tokio::test]
async fn ssh_exec_against_python_workspace() {
    let python_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..");
    let fixture =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/interop/workspace_env.py");
    let work_dir = std::env::temp_dir().join(format!("hud-rs-ssh-test-{}", std::process::id()));
    std::fs::create_dir_all(&work_dir).unwrap();
    std::fs::write(work_dir.join("marker.txt"), "from the test\n").unwrap();

    let mut child = tokio::process::Command::new("uv")
        .args(["run", "--project"])
        .arg(&python_dir)
        .args(["python", "-m", "hud.environment.server"])
        .arg(&fixture)
        .env("SLATE_WORK_DIR", &work_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn python workspace env");

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
    .expect("env served in time")
    .expect("port announcement");

    let client = connect(
        &format!("tcp://127.0.0.1:{port}"),
        ConnectOptions::default(),
    )
    .await
    .expect("connect");

    // Workspace publishes ssh/2 with a loopback URL; the manifest binding is
    // already rewritten to the client's tunnel forwarder.
    let cap = client.binding("ssh").expect("ssh binding").clone();
    assert_eq!(cap.protocol, "ssh/2");
    let ssh = SshClient::connect(&cap).await.expect("ssh connect");

    let result = ssh.run("cat marker.txt").await.expect("exec");
    assert!(result.success(), "stderr: {}", result.stderr);
    assert_eq!(result.stdout.trim(), "from the test");

    // Write through the shell, read back — and nonzero exits surface.
    let result = ssh.run("echo hi > out.txt && cat out.txt").await.unwrap();
    assert_eq!(result.stdout.trim(), "hi");
    let result = ssh.run("exit 3").await.unwrap();
    assert_eq!(result.exit_status, 3);

    // Concurrent commands multiplex over one session.
    let ssh = std::sync::Arc::new(ssh);
    let mut handles = Vec::new();
    for i in 0..4 {
        let ssh = std::sync::Arc::clone(&ssh);
        handles.push(tokio::spawn(async move {
            ssh.run(&format!("echo par-{i}")).await.unwrap().stdout
        }));
    }
    for (i, handle) in handles.into_iter().enumerate() {
        assert_eq!(handle.await.unwrap().trim(), format!("par-{i}"));
    }

    ssh.close().await;
    client.close().await;
    let _ = child.kill().await;
    let _ = std::fs::remove_dir_all(&work_dir);
}
