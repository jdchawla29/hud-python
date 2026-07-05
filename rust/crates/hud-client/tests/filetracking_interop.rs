//! `filetracking/1` client against a real Python workspace env, reached
//! through the control-port tunnel forwarder.
//!
//! Gated on `HUD_PYTHON_DIR` and the `ssh` feature (the fixture serves a
//! tracked workspace):
//!
//! ```sh
//! HUD_PYTHON_DIR=~/dev/hud-python cargo test -p hud-client --features ssh --test filetracking_interop
//! ```

#![cfg(feature = "ssh")]

use hud_client::filetracking::FileTrackingClient;
use hud_client::ssh::SshClient;
use hud_client::{connect, ConnectOptions};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};

#[tokio::test]
async fn diffs_track_workspace_edits() {
    let Some(python_dir) = std::env::var_os("HUD_PYTHON_DIR").map(PathBuf::from) else {
        eprintln!("skipping: set HUD_PYTHON_DIR to run Python interop tests");
        return;
    };
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/interop/tracked_workspace_env.py");
    let work_dir = std::env::temp_dir().join(format!("hud-rs-ft-{}", std::process::id()));
    std::fs::create_dir_all(&work_dir).unwrap();

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
    .expect("served in time")
    .expect("port announcement");

    let client = connect(
        &format!("tcp://127.0.0.1:{port}"),
        ConnectOptions::default(),
    )
    .await
    .expect("connect");

    let ft_url = client
        .binding("filetracking")
        .expect("filetracking binding")
        .url
        .clone();
    let ssh_cap = client.binding("ssh").expect("ssh binding").clone();

    let mut ft = FileTrackingClient::connect(&ft_url)
        .await
        .expect("ft connect");
    ft.advance().await.expect("advance");

    // Make an edit through the workspace shell, then observe it in a diff.
    let ssh = SshClient::connect(&ssh_cap).await.expect("ssh connect");
    ssh.run("printf 'line one\\nline two\\n' > notes.txt")
        .await
        .unwrap();

    // The tracker samples on its own cadence; poll a few times.
    let mut seen = None;
    for _ in 0..20 {
        let diff = ft.diff().await.expect("diff");
        if diff.files_changed > 0 {
            seen = Some(diff);
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let diff = seen.expect("a diff for notes.txt");
    let patch = diff
        .patches
        .iter()
        .find(|p| p.path == "notes.txt")
        .expect("notes.txt patch");
    assert_eq!(patch.status, "added");
    let (added, removed) = patch.line_delta();
    assert_eq!(added, 2, "patch:\n{}", patch.patch);
    assert_eq!(removed, 0);

    ssh.close().await;
    client.close().await;
    let _ = child.kill().await;
    let _ = std::fs::remove_dir_all(&work_dir);
}
