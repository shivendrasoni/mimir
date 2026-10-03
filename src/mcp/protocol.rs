use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::error::{MimirError, Result};

pub const JSONRPC_VERSION: &str = "2.0";
pub const PROTOCOL_VERSION: &str = "2025-06-18";
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

pub async fn write_json_line<W>(writer: &mut W, value: &impl Serialize) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    let payload = serde_json::to_vec(value)?;
    if payload.len() > MAX_FRAME_BYTES {
        return Err(MimirError::Protocol(
            "MCP JSON line exceeds hard cap".into(),
        ));
    }
    writer.write_all(&payload).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
async fn read_json_line<R>(reader: &mut R, max_frame_bytes: usize) -> Result<Value>
where
    R: AsyncBufRead + Unpin,
{
    Ok(read_json_line_frame(reader, max_frame_bytes).await?.0)
}

async fn read_json_line_frame<R>(reader: &mut R, max_frame_bytes: usize) -> Result<(Value, usize)>
where
    R: AsyncBufRead + Unpin,
{
    let limit = max_frame_bytes.min(MAX_FRAME_BYTES);
    let mut payload = Vec::new();
    let read = reader
        .take(limit as u64 + 2)
        .read_until(b'\n', &mut payload)
        .await?;
    if read == 0 || !payload.ends_with(b"\n") || payload.len() > limit + 1 {
        return Err(MimirError::Protocol(
            "MCP JSON line is incomplete or exceeds limit".into(),
        ));
    }
    Ok((serde_json::from_slice(&payload)?, payload.len()))
}

pub async fn read_json_line_response<R>(reader: &mut R, max_frame_bytes: usize) -> Result<Value>
where
    R: AsyncBufRead + Unpin,
{
    let mut total = 0_usize;
    for _ in 0..=32 {
        let (frame, received_bytes) = read_json_line_frame(reader, max_frame_bytes).await?;
        total = total.saturating_add(received_bytes);
        if total > MAX_FRAME_BYTES {
            break;
        }
        if frame.get("id").is_some() {
            return Ok(frame);
        }
        if frame["jsonrpc"] != JSONRPC_VERSION
            || !frame["method"]
                .as_str()
                .is_some_and(|method| method.starts_with("notifications/"))
        {
            return Err(MimirError::Protocol(
                "Unexpected MCP message before response".into(),
            ));
        }
    }
    Err(MimirError::Protocol(
        "MCP notification budget exceeded".into(),
    ))
}

#[derive(Debug, Clone, Serialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: &'static str,
    pub id: u64,
    pub method: String,
    pub params: Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct JsonRpcNotification {
    pub jsonrpc: &'static str,
    pub method: String,
    pub params: Value,
}

#[derive(Debug, Clone, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: Value,
    #[serde(default)]
    pub result: Option<Value>,
    #[serde(default)]
    pub error: Option<JsonRpcError>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
}

pub async fn write_message<W>(writer: &mut W, value: &impl Serialize) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    let payload = serde_json::to_vec(value)?;
    if payload.len() > MAX_FRAME_BYTES {
        return Err(MimirError::Protocol(format!(
            "MCP frame exceeds the hard cap of {MAX_FRAME_BYTES} bytes"
        )));
    }
    writer
        .write_all(format!("Content-Length: {}\r\n\r\n", payload.len()).as_bytes())
        .await?;
    writer.write_all(&payload).await?;
    writer.flush().await?;
    Ok(())
}

pub async fn read_message<R>(reader: &mut R, max_frame_bytes: usize) -> Result<Value>
where
    R: AsyncBufRead + Unpin,
{
    let mut content_length = None;
    loop {
        let mut line = Vec::new();
        let read = reader.read_until(b'\n', &mut line).await?;
        if read == 0 {
            return Err(MimirError::Protocol(
                "unexpected EOF while reading the MCP header".into(),
            ));
        }
        let line = String::from_utf8(line).map_err(|error| {
            MimirError::Protocol(format!("MCP header line is not valid UTF-8: {error}"))
        })?;
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }
        let lower = line.to_ascii_lowercase();
        if let Some((_, value)) = lower.split_once(':')
            && lower.starts_with("content-length:")
        {
            content_length = Some(value.trim().parse::<usize>().map_err(|error| {
                MimirError::Protocol(format!("invalid MCP content length: {error}"))
            })?);
        }
    }
    let content_length = content_length
        .ok_or_else(|| MimirError::Protocol("missing Content-Length header in MCP frame".into()))?;
    if content_length > max_frame_bytes {
        return Err(MimirError::Protocol(format!(
            "MCP frame size {content_length} exceeds the configured limit of {max_frame_bytes} bytes"
        )));
    }
    let mut payload = vec![0_u8; content_length];
    reader.read_exact(&mut payload).await?;
    Ok(serde_json::from_slice(&payload)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn managed_stdio_accepts_newline_frames_and_rejects_unbounded_input() {
        let mut valid = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n".as_slice();
        assert_eq!(read_json_line(&mut valid, 128).await.unwrap()["id"], 1);
        let mut oversized = b"{\"value\":\"xxxxxxxxxxxxxxxxxxxxxxxx\"}\n".as_slice();
        assert!(read_json_line(&mut oversized, 8).await.is_err());
        let mut incomplete = b"{\"id\":1}".as_slice();
        assert!(read_json_line(&mut incomplete, 128).await.is_err());
    }
    #[tokio::test]
    async fn managed_stdio_skips_notifications_with_a_finite_budget() {
        let notification =
            "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/tools/list_changed\"}\n";
        let response = "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n";
        let wire = format!("{notification}{response}");
        assert_eq!(
            read_json_line_response(&mut wire.as_bytes(), 128)
                .await
                .unwrap()["id"],
            1
        );
        let flood = format!("{}{response}", notification.repeat(33));
        assert!(
            read_json_line_response(&mut flood.as_bytes(), 128)
                .await
                .is_err()
        );
        let padded = format!("{}{}", " ".repeat(600_000), notification);
        let padded_flood = format!("{padded}{padded}{response}");
        assert!(
            read_json_line_response(&mut padded_flood.as_bytes(), MAX_FRAME_BYTES)
                .await
                .is_err()
        );
    }
}
