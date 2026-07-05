//! `filetracking/1` capability client.
//!
//! A workspace served with file tracking publishes a `filetracking` binding: a
//! newline-delimited JSON-RPC daemon that snapshots the workspace tree and
//! reports unified diffs. Methods are argument-less: `advance` re-baselines to
//! "now" (drop setup churn), `snapshot` returns the current file manifest,
//! `diff` returns changes since the last baseline, `flush` returns the trailing
//! diff plus captured artifacts.

use hud_types::UrlParts;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;

/// A workspace diff can carry a large payload; cap a single frame at 160 MiB
/// (matching the Python client) so a runaway daemon can't exhaust memory.
const FRAME_LIMIT_BYTES: u64 = 160 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum FileTrackingError {
    #[error("filetracking capability missing host or port: {0:?}")]
    InvalidUrl(String),
    #[error("filetracking connection closed during {0:?}")]
    Closed(String),
    #[error("filetracking {method:?} error: {message}")]
    Rpc { method: String, message: String },
    #[error("filetracking {0:?}: result was not an object")]
    MalformedResult(String),
    #[error("filetracking frame exceeded {FRAME_LIMIT_BYTES} bytes")]
    FrameTooLarge,
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

/// One file's change between two snapshots.
#[derive(Debug, Clone, Deserialize)]
pub struct Patch {
    pub path: String,
    /// `"added"`, `"modified"`, or `"deleted"`.
    pub status: String,
    /// Unified diff text (or a placeholder for binary/redacted/over-limit).
    #[serde(default)]
    pub patch: String,
    #[serde(default)]
    pub size_before: u64,
    #[serde(default)]
    pub size_after: u64,
}

impl Patch {
    /// Added / removed line counts parsed from the unified diff body
    /// (hunk `+`/`-` lines, excluding the `+++`/`---` headers).
    pub fn line_delta(&self) -> (u32, u32) {
        let mut added = 0;
        let mut removed = 0;
        for line in self.patch.lines() {
            if line.starts_with("+++") || line.starts_with("---") {
                continue;
            }
            match line.as_bytes().first() {
                Some(b'+') => added += 1,
                Some(b'-') => removed += 1,
                _ => {}
            }
        }
        (added, removed)
    }
}

/// One diff sample.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct Diff {
    #[serde(default)]
    pub files_changed: u32,
    #[serde(default)]
    pub files_scanned: u32,
    #[serde(default)]
    pub patches: Vec<Patch>,
    #[serde(default)]
    pub truncated: bool,
}

/// Live `filetracking/1` connection.
pub struct FileTrackingClient {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    next_id: u64,
}

impl FileTrackingClient {
    /// Connect to the (already tunnel-resolved) capability URL.
    pub async fn connect(url: &str) -> Result<FileTrackingClient, FileTrackingError> {
        let parts =
            UrlParts::parse(url).map_err(|_| FileTrackingError::InvalidUrl(url.to_string()))?;
        let port = parts
            .port
            .ok_or_else(|| FileTrackingError::InvalidUrl(url.to_string()))?;
        let stream = TcpStream::connect((parts.host.as_str(), port)).await?;
        let (read_half, writer) = stream.into_split();
        Ok(FileTrackingClient {
            reader: BufReader::new(read_half),
            writer,
            next_id: 0,
        })
    }

    /// Re-baseline to the current tree — the first `diff` after this reports
    /// only changes made from here on (used to drop scenario-setup churn).
    pub async fn advance(&mut self) -> Result<(), FileTrackingError> {
        self.call("advance").await.map(|_| ())
    }

    /// The current file manifest (paths + hashes, no content).
    pub async fn snapshot(&mut self) -> Result<Value, FileTrackingError> {
        self.call("snapshot").await
    }

    /// Changes since the last baseline.
    pub async fn diff(&mut self) -> Result<Diff, FileTrackingError> {
        let result = self.call("diff").await?;
        Ok(serde_json::from_value(result)?)
    }

    /// The trailing diff plus captured artifacts, at teardown.
    pub async fn flush(&mut self) -> Result<Value, FileTrackingError> {
        self.call("flush").await
    }

    async fn call(&mut self, method: &str) -> Result<Value, FileTrackingError> {
        self.next_id += 1;
        let request = json!({
            "jsonrpc": "2.0",
            "id": self.next_id,
            "method": method,
            "params": {},
        });
        let mut line = serde_json::to_vec(&request)?;
        line.push(b'\n');
        self.writer.write_all(&line).await?;
        self.writer.flush().await?;

        let mut buf = Vec::new();
        let n = self.reader.read_until(b'\n', &mut buf).await?;
        if n == 0 {
            return Err(FileTrackingError::Closed(method.to_string()));
        }
        if buf.len() as u64 > FRAME_LIMIT_BYTES {
            return Err(FileTrackingError::FrameTooLarge);
        }
        let reply: Value = serde_json::from_slice(&buf)?;
        if let Some(error) = reply.get("error") {
            return Err(FileTrackingError::Rpc {
                method: method.to_string(),
                message: error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            });
        }
        match reply.get("result") {
            Some(Value::Object(_)) => Ok(reply["result"].clone()),
            _ => Err(FileTrackingError::MalformedResult(method.to_string())),
        }
    }
}
