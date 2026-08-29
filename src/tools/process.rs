use std::{process::Stdio, time::Duration};

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
    time::{Instant, sleep_until, timeout as tokio_timeout},
};
use tokio_util::sync::CancellationToken;

use crate::model::ToolDefinition;

use super::{
    ObservationStatus, Tool, ToolError, ToolObservation, ToolPolicy, WorkspacePathPolicy,
    object_schema, parse_input, truncate_utf8,
};

pub struct ProcessTool {
    paths: WorkspacePathPolicy,
    policy: ToolPolicy,
}

const PIPE_DRAIN_TIMEOUT: Duration = Duration::from_millis(500);

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
        self.execute_inner(input, &CancellationToken::new()).await
    }

    async fn execute_cancellable(
        &self,
        input: Value,
        cancellation: &CancellationToken,
    ) -> Result<ToolObservation, ToolError> {
        self.execute_inner(input, cancellation).await
    }
}

impl ProcessTool {
    async fn execute_inner(
        &self,
        input: Value,
        cancellation: &CancellationToken,
    ) -> Result<ToolObservation, ToolError> {
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
            cancellation,
        )
        .await?;
        let status = match capture.outcome {
            CaptureOutcome::Exited if capture.success => ObservationStatus::Success,
            CaptureOutcome::DrainTimedOut if capture.success => ObservationStatus::Warning,
            CaptureOutcome::Exited
            | CaptureOutcome::ExecutionTimedOut
            | CaptureOutcome::DrainTimedOut
            | CaptureOutcome::OutputLimit
            | CaptureOutcome::Cancelled => ObservationStatus::Error,
        };
        let summary = match capture.outcome {
            CaptureOutcome::Exited => format!("process exited with {}", capture.exit_label),
            CaptureOutcome::ExecutionTimedOut => format!(
                "process execution timed out after {} ms",
                self.policy.command_timeout.as_millis()
            ),
            CaptureOutcome::DrainTimedOut => format!(
                "process exited with {}; output pipes remained open past the {} ms drain deadline; lingering process group stopped",
                capture.exit_label,
                PIPE_DRAIN_TIMEOUT.as_millis()
            ),
            CaptureOutcome::OutputLimit => format!(
                "process output exceeded the {} byte limit; output truncated and process group stopped",
                self.policy.max_output_bytes
            ),
            CaptureOutcome::Cancelled => "process cancelled; process group stopped".into(),
        };
        tracing::info!(
            target: "mimir::tools::process",
            outcome = capture.outcome.as_str(),
            elapsed_ms = capture.elapsed.as_millis(),
            timeout_ms = self.policy.command_timeout.as_millis(),
            drain_timeout_ms = PIPE_DRAIN_TIMEOUT.as_millis(),
            output_bytes = capture.output_bytes,
            output_truncated = capture.truncated,
            exit_code = capture.exit_code,
            "run_process completed"
        );
        Ok(ToolObservation {
            status,
            summary,
            next_actions: match capture.outcome {
                CaptureOutcome::ExecutionTimedOut => {
                    vec!["Retry with a narrower command or increase the configured timeout".into()]
                }
                CaptureOutcome::DrainTimedOut => vec![
                    "Inspect the command for descendant processes that inherited stdout or stderr"
                        .into(),
                ],
                CaptureOutcome::OutputLimit => {
                    vec!["Reduce command output or increase the configured output limit".into()]
                }
                CaptureOutcome::Cancelled => vec!["Retry the command if it is still needed".into()],
                CaptureOutcome::Exited if status == ObservationStatus::Error => {
                    vec!["Inspect the bounded output and adjust the command".into()]
                }
                CaptureOutcome::Exited => Vec::new(),
            },
            artifacts: Vec::new(),
            content: capture.content,
        })
    }
}

struct Capture {
    content: String,
    exit_label: String,
    exit_code: Option<i32>,
    success: bool,
    truncated: bool,
    output_bytes: usize,
    elapsed: Duration,
    outcome: CaptureOutcome,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CaptureOutcome {
    Exited,
    ExecutionTimedOut,
    DrainTimedOut,
    OutputLimit,
    Cancelled,
}

impl CaptureOutcome {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Exited => "exited",
            Self::ExecutionTimedOut => "execution_timeout",
            Self::DrainTimedOut => "drain_timeout",
            Self::OutputLimit => "output_limit",
            Self::Cancelled => "cancelled",
        }
    }
}

async fn capture_bounded(
    mut child: Child,
    timeout: Duration,
    limit: usize,
    cancellation: &CancellationToken,
) -> Result<Capture, ToolError> {
    let started = Instant::now();
    let process_group = child.id().and_then(|id| i32::try_from(id).ok());
    let stdout = child.stdout.take().ok_or_else(|| ToolError::Execution {
        tool: "run_process".into(),
        message: "stdout pipe unavailable".into(),
    })?;
    let stderr = child.stderr.take().ok_or_else(|| ToolError::Execution {
        tool: "run_process".into(),
        message: "stderr pipe unavailable".into(),
    })?;
    let (events_tx, mut events_rx) = mpsc::channel(8);
    let stdout_task = tokio::spawn(read_pipe(stdout, events_tx.clone(), "stdout"));
    let stderr_task = tokio::spawn(read_pipe(stderr, events_tx, "stderr"));
    let deadline = sleep_until(Instant::now() + timeout);
    tokio::pin!(deadline);
    let mut status = None;
    let mut content = Vec::with_capacity(limit.min(64 * 1024));
    let mut truncated = false;
    let mut open_pipes = 2_u8;
    let mut outcome = CaptureOutcome::Exited;
    let mut pipe_error = None;

    while status.is_none() {
        let event = tokio::select! {
            biased;
            result = child.wait() => CaptureEvent::Exited(result),
            () = cancellation.cancelled() => CaptureEvent::Cancelled,
            () = &mut deadline => {
                CaptureEvent::TimedOut
            }
            event = events_rx.recv(), if open_pipes != 0 => CaptureEvent::Pipe(event),
        };
        match event {
            CaptureEvent::Exited(result) => status = Some(result?),
            CaptureEvent::Cancelled => {
                outcome = CaptureOutcome::Cancelled;
                status = terminate_group(&mut child, process_group).await;
            }
            CaptureEvent::TimedOut => {
                outcome = CaptureOutcome::ExecutionTimedOut;
                status = terminate_group(&mut child, process_group).await;
            }
            CaptureEvent::Pipe(event) => match event {
                Some(PipeEvent::Chunk(chunk)) => {
                    if append_bounded(&mut content, &chunk, limit) {
                        truncated = true;
                        outcome = CaptureOutcome::OutputLimit;
                        status = terminate_group(&mut child, process_group).await;
                    }
                }
                Some(PipeEvent::Closed) => {
                    open_pipes = open_pipes.saturating_sub(1);
                }
                Some(PipeEvent::Failed(error)) => {
                    open_pipes = open_pipes.saturating_sub(1);
                    pipe_error.get_or_insert(error);
                }
                None => {
                    open_pipes = 0;
                }
            },
        }
    }

    if open_pipes != 0 && !matches!(outcome, CaptureOutcome::OutputLimit) {
        let drained = tokio_timeout(
            PIPE_DRAIN_TIMEOUT,
            drain_pipes(
                &mut events_rx,
                &mut open_pipes,
                &mut content,
                limit,
                &mut pipe_error,
            ),
        )
        .await;
        match drained {
            Ok(true) => {
                truncated = true;
                if outcome == CaptureOutcome::Exited {
                    outcome = CaptureOutcome::OutputLimit;
                }
                kill_process_group(process_group);
            }
            Ok(false) => {}
            Err(_) => {
                if outcome == CaptureOutcome::Exited {
                    outcome = CaptureOutcome::DrainTimedOut;
                }
                kill_process_group(process_group);
            }
        }
    }

    if open_pipes == 0 {
        let _ = stdout_task.await;
        let _ = stderr_task.await;
    } else {
        stdout_task.abort();
        stderr_task.abort();
        let _ = stdout_task.await;
        let _ = stderr_task.await;
    }
    if let Some(error) = pipe_error {
        return Err(ToolError::Execution {
            tool: "run_process".into(),
            message: error,
        });
    }

    let status = status;
    let output_bytes = content.len();
    let decoded = String::from_utf8_lossy(&content);
    let (content, _) = truncate_utf8(&decoded, limit);
    Ok(Capture {
        content,
        exit_label: status
            .as_ref()
            .and_then(|value| value.code())
            .map_or_else(|| "signal".into(), |code| code.to_string()),
        exit_code: status.as_ref().and_then(std::process::ExitStatus::code),
        success: status
            .as_ref()
            .is_some_and(std::process::ExitStatus::success),
        truncated,
        output_bytes,
        elapsed: started.elapsed(),
        outcome,
    })
}

enum CaptureEvent {
    Exited(std::io::Result<std::process::ExitStatus>),
    TimedOut,
    Cancelled,
    Pipe(Option<PipeEvent>),
}

enum PipeEvent {
    Chunk(Vec<u8>),
    Closed,
    Failed(String),
}

async fn read_pipe<R>(mut reader: R, events: mpsc::Sender<PipeEvent>, label: &'static str)
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut buffer = [0_u8; 4096];
    loop {
        let count = match reader.read(&mut buffer).await {
            Ok(count) => count,
            Err(error) => {
                let _ = events
                    .send(PipeEvent::Failed(format!(
                        "failed to read {label}: {error}"
                    )))
                    .await;
                return;
            }
        };
        if count == 0 {
            let _ = events.send(PipeEvent::Closed).await;
            return;
        }
        if events
            .send(PipeEvent::Chunk(buffer[..count].to_vec()))
            .await
            .is_err()
        {
            return;
        }
    }
}

async fn drain_pipes(
    events: &mut mpsc::Receiver<PipeEvent>,
    open_pipes: &mut u8,
    content: &mut Vec<u8>,
    limit: usize,
    pipe_error: &mut Option<String>,
) -> bool {
    while *open_pipes != 0 {
        match events.recv().await {
            Some(PipeEvent::Chunk(chunk)) => {
                if append_bounded(content, &chunk, limit) {
                    return true;
                }
            }
            Some(PipeEvent::Closed) => *open_pipes = open_pipes.saturating_sub(1),
            Some(PipeEvent::Failed(error)) => {
                *open_pipes = open_pipes.saturating_sub(1);
                pipe_error.get_or_insert(error);
            }
            None => *open_pipes = 0,
        }
    }
    false
}

fn append_bounded(target: &mut Vec<u8>, chunk: &[u8], limit: usize) -> bool {
    let remaining = limit.saturating_sub(target.len());
    target.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
    chunk.len() > remaining
}

async fn terminate_group(
    child: &mut Child,
    process_group: Option<i32>,
) -> Option<std::process::ExitStatus> {
    kill_process_group(process_group);
    let _ = child.start_kill();
    child.wait().await.ok()
}

fn kill_process_group(process_group: Option<i32>) {
    if let Some(id) = process_group {
        let _ = killpg(Pid::from_raw(id), Signal::SIGKILL);
    }
}
