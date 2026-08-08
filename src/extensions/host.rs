#![allow(
    clippy::missing_errors_doc,
    reason = "extension host operations return bounded typed protocol and process errors"
)]

use std::{path::Path, time::Duration};

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
};

use crate::error::{MimirError, Result};

use super::manifest::{ExtensionEntrypoint, ExtensionManifest};

#[derive(Debug, Clone, Copy)]
pub struct HostLimits {
    pub max_request_bytes: usize,
    pub max_response_bytes: usize,
    pub timeout: Duration,
}

impl Default for HostLimits {
    fn default() -> Self {
        Self {
            max_request_bytes: 16 * 1024,
            max_response_bytes: 64 * 1024,
            timeout: Duration::from_secs(5),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostRequest {
    pub schema_version: u16,
    pub id: String,
    pub command: String,
    pub payload: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostResponseStatus {
    Ok,
    Error,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostResponse {
    pub schema_version: u16,
    pub id: String,
    pub status: HostResponseStatus,
    #[serde(default)]
    pub message: Option<String>,
    pub output: Value,
}

#[derive(Debug, Clone)]
pub struct JsonLineExtensionHost {
    manifest: ExtensionManifest,
    workspace_root: std::path::PathBuf,
    limits: HostLimits,
}

impl JsonLineExtensionHost {
    pub fn new(
        manifest: ExtensionManifest,
        workspace_root: &Path,
        limits: HostLimits,
    ) -> Result<Self> {
        manifest.validate()?;
        Ok(Self {
            manifest,
            workspace_root: workspace_root.to_path_buf(),
            limits,
        })
    }

    pub async fn invoke(&self, request: HostRequest) -> Result<HostResponse> {
        validate_request(&request, self.limits.max_request_bytes)?;
        let ExtensionEntrypoint::NativeProcess { program, args } = &self.manifest.entrypoint else {
            return Err(MimirError::Configuration(
                "the native subprocess host requires a native process entrypoint".into(),
            ));
        };
        let mut child = Command::new(program)
            .args(args)
            .current_dir(&self.workspace_root)
            .env_clear()
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        if let Some(mut stdin) = child.stdin.take() {
            let mut encoded = serde_json::to_vec(&request)?;
            encoded.push(b'\n');
            stdin.write_all(&encoded).await?;
            stdin.shutdown().await?;
        }
        let limits = self.limits;
        let outcome = tokio::time::timeout(limits.timeout, capture(child, limits)).await;
        let (stdout, stderr) = match outcome {
            Ok(result) => result?,
            Err(_) => {
                return Err(MimirError::Protocol(format!(
                    "extension '{}' timed out after {} ms",
                    self.manifest.name,
                    limits.timeout.as_millis()
                )));
            }
        };
        let response: HostResponse = serde_json::from_slice(&stdout).map_err(|error| {
            MimirError::Protocol(format!(
                "extension '{}' returned invalid JSON: {error}; stderr: {}",
                self.manifest.name, stderr
            ))
        })?;
        validate_response(&request, &response)?;
        Ok(response)
    }
}

async fn capture(
    mut child: tokio::process::Child,
    limits: HostLimits,
) -> Result<(Vec<u8>, String)> {
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| MimirError::Protocol("extension stdout pipe unavailable".into()))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| MimirError::Protocol("extension stderr pipe unavailable".into()))?;
    let mut stdout_bytes = Vec::with_capacity(limits.max_response_bytes.min(2048));
    let mut stderr_bytes = Vec::with_capacity(limits.max_response_bytes.min(1024));
    let mut stdout_done = false;
    let mut stderr_done = false;
    let mut status = None;
    let mut out_buffer = [0_u8; 256];
    let mut err_buffer = [0_u8; 256];

    while !stdout_done || !stderr_done || status.is_none() {
        tokio::select! {
            read = stdout.read(&mut out_buffer), if !stdout_done => {
                let count = read?;
                if count == 0 {
                    stdout_done = true;
                } else {
                    if append_bounded(&mut stdout_bytes, &out_buffer[..count], limits.max_response_bytes) {
                        return Err(MimirError::Protocol("extension response exceeded the configured response limit".into()));
                    }
                    if stdout_bytes.contains(&b'\n') {
                        stdout_done = true;
                    }
                }
            }
            read = stderr.read(&mut err_buffer), if !stderr_done => {
                let count = read?;
                if count == 0
                    || append_bounded(
                        &mut stderr_bytes,
                        &err_buffer[..count],
                        limits.max_response_bytes,
                    )
                {
                    stderr_done = true;
                }
            }
            waited = child.wait(), if status.is_none() => {
                status = Some(waited?);
            }
        }
    }
    if !status.is_some_and(|value| value.success()) {
        return Err(MimirError::Protocol(format!(
            "extension process exited unsuccessfully: {}",
            String::from_utf8_lossy(&stderr_bytes)
        )));
    }
    let newline = stdout_bytes
        .iter()
        .position(|byte| *byte == b'\n')
        .unwrap_or(stdout_bytes.len());
    Ok((
        stdout_bytes[..newline].to_vec(),
        String::from_utf8_lossy(&stderr_bytes)
            .chars()
            .take(512)
            .collect(),
    ))
}

fn validate_request(request: &HostRequest, max_bytes: usize) -> Result<()> {
    if request.schema_version != 1 {
        return Err(MimirError::Protocol(format!(
            "unsupported host request schema version {}",
            request.schema_version
        )));
    }
    let pattern = Regex::new(r"^[A-Za-z0-9._:-]{1,64}$").expect("host identifier regex is valid");
    if !pattern.is_match(&request.id) {
        return Err(MimirError::Protocol(
            "extension host request id is invalid".into(),
        ));
    }
    if !pattern.is_match(&request.command) {
        return Err(MimirError::Protocol(
            "extension host command is invalid".into(),
        ));
    }
    let encoded = serde_json::to_vec(request)?;
    if encoded.len() > max_bytes {
        return Err(MimirError::Protocol(format!(
            "extension host request exceeds the configured request limit of {max_bytes} bytes"
        )));
    }
    Ok(())
}

fn validate_response(request: &HostRequest, response: &HostResponse) -> Result<()> {
    if response.schema_version != 1 {
        return Err(MimirError::Protocol(format!(
            "unsupported extension response schema version {}",
            response.schema_version
        )));
    }
    if response.id != request.id {
        return Err(MimirError::Protocol(
            "extension response id does not match the request".into(),
        ));
    }
    Ok(())
}

fn append_bounded(target: &mut Vec<u8>, chunk: &[u8], limit: usize) -> bool {
    let remaining = limit.saturating_sub(target.len());
    target.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
    chunk.len() > remaining
}
