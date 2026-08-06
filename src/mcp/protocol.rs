use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::error::{MimirError, Result};

pub const JSONRPC_VERSION: &str = "2.0";
pub const PROTOCOL_VERSION: &str = "2025-06-18";
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

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
