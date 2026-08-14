//! A minimal reference environment: one `echo` task and a tunnelable TCP
//! capability. The Rust counterpart of the Python interop fixture env.
//!
//! Run: `cargo run -p hud-env --example echo_env [-- <port>]`

use hud_env::{template, Environment, Evaluation, Prompt};
use hud_types::{Capability, EvaluationResult, SubScore};
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[derive(Deserialize)]
struct EchoArgs {
    text: String,
}

#[derive(Deserialize)]
struct CountArgs {
    #[serde(default = "default_n")]
    n: u32,
}

fn default_n() -> u32 {
    3
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let port: u16 = std::env::args()
        .nth(1)
        .and_then(|p| p.parse().ok())
        .unwrap_or(0);

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
            let ok = answer.text().trim() == expected;
            Ok(Evaluation::Score(if ok { 1.0 } else { 0.0 }))
        },
    ));

    env.add_template(template(
        "count",
        "Count from 1 to n, comma-separated.",
        |args: CountArgs| async move {
            Ok((
                Prompt::Text(format!("Count from 1 to {}, comma-separated.", args.n)),
                args.n,
            ))
        },
        |n: u32, answer| async move {
            let expected: Vec<String> = (1..=n).map(|i| i.to_string()).collect();
            let expected = expected.join(",");
            let got = answer.text().replace(' ', "");
            let exact = got.trim() == expected;
            let result = EvaluationResult::with_reward(if exact { 1.0 } else { 0.0 })
                .content(if exact { "exact match" } else { "mismatch" })
                .subscores(vec![SubScore::new("exact", if exact { 1.0 } else { 0.0 })]);
            Ok(Evaluation::Result(result))
        },
    ));

    // A loopback TCP echo daemon, published as a capability so clients can
    // exercise `tunnel.open` through the control port.
    env.on_start(|| async {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
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

    hud_env::serve(env, "127.0.0.1", port).await
}
