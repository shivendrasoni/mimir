use std::process::Stdio;

use async_trait::async_trait;
use nix::{
    sys::signal::{Signal, killpg},
    unistd::Pid,
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{
    io::AsyncReadExt,
    process::{Child, Command},
    sync::mpsc,
    time::{Instant, sleep_until},
};

use crate::model::ToolDefinition;

use super::{
    ObservationStatus, Tool, ToolError, ToolObservation, ToolPolicy, WorkspacePathPolicy,
    object_schema, parse_input, truncate_utf8,
};

pub struct ProcessTool {
    paths: WorkspacePathPolicy,
    policy: ToolPolicy,
}

impl ProcessTool {
    pub fn new(paths: WorkspacePathPolicy, policy: ToolPolicy) -> Self {
        Self { paths, policy }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProcessInput {
    program: String,
    #[serde(default)]
    args: Vec<String>,
}

#[async_trait]
impl Tool for ProcessTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "run_process".into(),
            description: "Run one program with an explicit argument vector in the workspace".into(),
            parameters: object_schema(
                &json!({
                    "program": {"type": "string"},
                    "args": {"type": "array", "items": {"type": "string"}}
                }),
                &["program"],
            ),
        }
    }

    async fn execute(&self, input: Value) -> Result<ToolObservation, ToolError> {
        if !self.policy.allow_process {
            return Err(ToolError::Disabled {
                tool: "run_process".into(),
            });
        }
        let input: ProcessInput = parse_input("run_process", input)?;
        if input.program.is_empty() {
            return Err(ToolError::InvalidArguments {
                tool: "run_process".into(),
                message: "program must not be empty".into(),
            });
        }
        if self
            .policy
            .allowed_programs
            .as_ref()
            .is_some_and(|allowed| !allowed.iter().any(|candidate| candidate == &input.program))
        {
            return Err(ToolError::Disabled {
                tool: format!("run_process:{}", input.program),
            });
        }
        let command = std::iter::once(input.program.as_str())
            .chain(input.args.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" ");
        if let Some(approvals) = &self.policy.approvals
            && let Some(request) = approvals.requires_approval(&command)?
        {
            return Err(ToolError::ApprovalRequired { request });
        }
        let mut command = Command::new(&input.program);
        command
            .args(&input.args)
            .current_dir(self.paths.root())
            .env_clear()
            .env("PATH", "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        {
            std::os::unix::process::CommandExt::process_group(command.as_std_mut(), 0);
        }
        let child = command.spawn().map_err(|error| ToolError::Execution {
            tool: "run_process".into(),
            message: error.to_string(),
        })?;
        let capture = capture_bounded(
            child,
            self.policy.command_timeout,
            self.policy.max_output_bytes,
        )
        .await?;
        if capture.timed_out {
            return Ok(ToolObservation {
                status: ObservationStatus::Error,
                summary: format!(
                    "process timed out after {} ms",
                    self.policy.command_timeout.as_millis()
                ),
                next_actions: vec![
                    "Retry with a narrower command or increase the configured timeout".into(),
                ],
                artifacts: Vec::new(),
                content: capture.content,
            });
        }
        let status = if capture.success && !capture.truncated {
            ObservationStatus::Success
        } else {
            ObservationStatus::Error
        };
        Ok(ToolObservation {
            status,
            summary: format!(
                "process exited with {}{}",
                capture.exit_label,
                if capture.truncated {
                    "; output truncated and process stopped"
                } else {
                    ""
                }
            ),
            next_actions: if status == ObservationStatus::Error {
                vec!["Inspect the bounded output and adjust the command".into()]
            } else {
                Vec::new()
            },
            artifacts: Vec::new(),
            content: capture.content,
        })
    }
}

struct Capture {
    content: String,
    exit_label: String,
    success: bool,
    truncated: bool,
    timed_out: bool,
}

async fn capture_bounded(
    mut child: Child,
    timeout: std::time::Duration,
    limit: usize,
) -> Result<Capture, ToolError> {
    let stdout = child.stdout.take().ok_or_else(|| ToolError::Execution {
        tool: "run_process".into(),
        message: "stdout pipe unavailable".into(),
    })?;
    let stderr = child.stderr.take().ok_or_else(|| ToolError::Execution {
        tool: "run_process".into(),
        message: "stderr pipe unavailable".into(),
    })?;
    let (events_tx, mut events_rx) = mpsc::channel(8);
    let stdout_task = tokio::spawn(read_pipe(stdout, events_tx.clone()));
    let stderr_task = tokio::spawn(read_pipe(stderr, events_tx));
    let deadline = sleep_until(Instant::now() + timeout);
    tokio::pin!(deadline);
    let mut status = None;
    let mut content = Vec::with_capacity(limit.min(64 * 1024));
    let mut timed_out = false;
    let mut truncated = false;
    let mut open_pipes = 2_u8;

    while status.is_none() || open_pipes != 0 {
        if status.is_none() {
            status = child.try_wait()?;
        }
        let poll_child = tokio::time::sleep(std::time::Duration::from_millis(10));
        tokio::pin!(poll_child);
        tokio::select! {
            () = &mut deadline => {
                timed_out = true;
                terminate_group(&mut child).await;
                status = child.wait().await.ok();
            }
            () = &mut poll_child, if status.is_none() => {}
            event = events_rx.recv(), if open_pipes != 0 => match event {
                Some(PipeEvent::Chunk(chunk)) => {
                    if append_bounded(&mut content, &chunk, limit) {
                        truncated = true;
                        terminate_group(&mut child).await;
                        status = child.wait().await.ok();
                    }
                }
                Some(PipeEvent::Closed) => {
                    open_pipes = open_pipes.saturating_sub(1);
                }
                None => {
                    open_pipes = 0;
                }
            }
        }
    }
    let _ = stdout_task.await;
    let _ = stderr_task.await;
    let status = status;
    let decoded = String::from_utf8_lossy(&content);
    let (content, _) = truncate_utf8(&decoded, limit);
    Ok(Capture {
        content,
        exit_label: status
            .and_then(|value| value.code())
            .map_or_else(|| "signal".into(), |code| code.to_string()),
        success: status.is_some_and(|value| value.success()),
        truncated,
        timed_out,
    })
}

enum PipeEvent {
    Chunk(Vec<u8>),
    Closed,
}

async fn read_pipe<R>(mut reader: R, events: mpsc::Sender<PipeEvent>) -> Result<(), std::io::Error>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut buffer = [0_u8; 4096];
    loop {
        let count = reader.read(&mut buffer).await?;
        if count == 0 {
            let _ = events.send(PipeEvent::Closed).await;
            return Ok(());
        }
        if events
            .send(PipeEvent::Chunk(buffer[..count].to_vec()))
            .await
            .is_err()
        {
            return Ok(());
        }
    }
}

fn append_bounded(target: &mut Vec<u8>, chunk: &[u8], limit: usize) -> bool {
    let remaining = limit.saturating_sub(target.len());
    target.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
    chunk.len() > remaining
}

async fn terminate_group(child: &mut Child) {
    if let Some(id) = child.id().and_then(|id| i32::try_from(id).ok()) {
        let _ = killpg(Pid::from_raw(id), Signal::SIGKILL);
    }
    let _ = child.kill().await;
    let _ = child.wait().await;
}
