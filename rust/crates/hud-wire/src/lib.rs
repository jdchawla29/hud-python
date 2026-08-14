//! JSON-RPC 2.0 framing + byte splicing for the HUD control channel.
//!
//! The wire format is newline-delimited compact JSON, one JSON-RPC object per
//! line — `serde_json::to_string` produces exactly Python's
//! `json.dumps(..., separators=(",", ":"))` layout.

use serde_json::{json, Value};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt};

/// JSON-RPC error codes used by the control channel.
pub mod code {
    /// Malformed request (e.g. `tasks.grade` with no task in progress).
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    /// Bad params: wrong types, unknown task id, bad tunnel target.
    pub const INVALID_PARAMS: i64 = -32602;
    /// Malformed reply from the peer.
    pub const INTERNAL: i64 = -32603;
    /// Server-side task/handler failure.
    pub const SERVER_ERROR: i64 = -32000;
}

/// An error frame from the peer.
#[derive(Debug, Clone, thiserror::Error)]
#[error("hud rpc error {code}: {message}")]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

impl RpcError {
    pub fn from_frame(error: &Value) -> RpcError {
        RpcError {
            code: error
                .get("code")
                .and_then(Value::as_i64)
                .unwrap_or(code::SERVER_ERROR),
            message: error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        }
    }
}

/// Write one newline-delimited JSON frame and flush.
pub async fn send_frame<W: AsyncWrite + Unpin>(writer: &mut W, msg: &Value) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(msg)?;
    line.push(b'\n');
    writer.write_all(&line).await?;
    writer.flush().await
}

/// Read one frame; `Ok(None)` on EOF.
pub async fn read_frame<R: AsyncBufRead + Unpin>(reader: &mut R) -> std::io::Result<Option<Value>> {
    let mut line = String::new();
    let n = reader.read_line(&mut line).await?;
    if n == 0 {
        return Ok(None);
    }
    let value = serde_json::from_str(&line)?;
    Ok(Some(value))
}

/// JSON-RPC 2.0 request.
pub fn request(id: u64, method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

/// JSON-RPC 2.0 success response.
pub fn reply(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

/// JSON-RPC 2.0 error response.
pub fn error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// Pipe two byte streams into each other until both directions hit EOF.
///
/// Resets and aborts are a normal way for tunneled streams to end (an SSH
/// client hanging up, a container dying); they end the splice, not the world.
pub async fn splice<A, B>(a: &mut A, b: &mut B) -> std::io::Result<()>
where
    A: AsyncRead + AsyncWrite + Unpin,
    B: AsyncRead + AsyncWrite + Unpin,
{
    match tokio::io::copy_bidirectional(a, b).await {
        Ok(_) => Ok(()),
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::UnexpectedEof
                    | std::io::ErrorKind::NotConnected
            ) =>
        {
            Ok(())
        }
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frame_roundtrip_is_compact_json() {
        let mut buf = Vec::new();
        send_frame(&mut buf, &request(1, "hello", json!({})))
            .await
            .unwrap();
        assert_eq!(
            String::from_utf8(buf.clone()).unwrap(),
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"hello\",\"params\":{}}\n"
        );
        let mut reader = std::io::Cursor::new(buf);
        let frame = read_frame(&mut reader).await.unwrap().unwrap();
        assert_eq!(frame["method"], "hello");
        assert!(read_frame(&mut reader).await.unwrap().is_none());
    }

    #[test]
    fn error_frame_shape() {
        let frame = error(json!(4), code::METHOD_NOT_FOUND, "method not found: x");
        assert_eq!(frame["error"]["code"], -32601);
        assert_eq!(frame["id"], 4);
    }
}
