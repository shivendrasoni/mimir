use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Mutex as StdMutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use nix::{
    sys::signal::{Signal, killpg},
    unistd::Pid,
};
use serde::Serialize;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::{Child, Command},
    sync::{Mutex, mpsc},
    time::{Instant, sleep_until},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{ToolError, ToolPolicy, WorkspacePathPolicy};

const MAX_FULL_LOG_BYTES: usize = 8 * 1024 * 1024;
const MAX_COMMAND_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BashResult {
    pub output: String,
    pub exit_code: Option<i32>,
    pub cancelled: bool,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub full_output_path: Option<PathBuf>,
    #[serde(skip_serializing)]
    pub timed_out: bool,
}

pub struct BashRunner {
    paths: WorkspacePathPolicy,
    policy: ToolPolicy,
    run_lock: Mutex<()>,
    cancellation: StdMutex<CancellationToken>,
    running: AtomicBool,
}

impl BashRunner {
    /// Creates a shell runner rooted at the canonical workspace.
    ///
    /// # Errors
    ///
    /// Returns an error when the workspace cannot be canonicalized.
    pub fn new(workspace: &Path, policy: ToolPolicy) -> Result<Self, ToolError> {
        Ok(Self {
            paths: WorkspacePathPolicy::new(workspace)?,
            policy,
            run_lock: Mutex::new(()),
            cancellation: StdMutex::new(CancellationToken::new()),
            running: AtomicBool::new(false),
        })
    }

    /// Executes one explicit shell command with bounded resources.
    ///
    /// # Errors
    ///
    /// Returns a disabled, validation, spawn, or I/O error.
    pub async fn execute(&self, command: &str) -> Result<BashResult, ToolError> {
        self.execute_inner(command, None).await
    }

    /// Executes one bounded shell command while forwarding sanitized output
    /// chunks through a bounded channel.
    ///
    /// # Errors
    ///
    /// Returns the same disabled, validation, spawn, timeout, or I/O errors as
    /// [`Self::execute`]. A closed output receiver does not abort the command.
    pub async fn execute_streaming(
        &self,
        command: &str,
        output: mpsc::Sender<String>,
    ) -> Result<BashResult, ToolError> {
        self.execute_inner(command, Some(output)).await
    }

    async fn execute_inner(
        &self,
        command: &str,
        output: Option<mpsc::Sender<String>>,
    ) -> Result<BashResult, ToolError> {
        if !self.policy.allow_process {
            return Err(ToolError::Disabled {
                tool: "bash".into(),
            });
        }
        let command = command.trim();
        if command.is_empty() {
            return Err(ToolError::InvalidArguments {
                tool: "bash".into(),
                message: "command must not be blank".into(),
            });
        }
        validate_allowlisted_command(command, self.policy.allowed_programs.as_deref())?;
        if let Some(approvals) = &self.policy.approvals
            && let Some(request) = approvals.requires_approval(command)?
        {
            return Err(ToolError::ApprovalRequired { request });
        }
        let _guard = self.run_lock.lock().await;
        let cancellation = CancellationToken::new();
        *self
            .cancellation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = cancellation.clone();
        self.running.store(true, Ordering::Release);

        let child = spawn_shell(command, self.paths.root()).inspect_err(|_error| {
            self.running.store(false, Ordering::Release);
        })?;
        let result = capture_shell(
            child,
            self.policy.command_timeout,
            self.policy.max_output_bytes,
            &cancellation,
            output.as_ref(),
        )
        .await;
        self.running.store(false, Ordering::Release);
        *self
            .cancellation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = CancellationToken::new();
        result
    }

    pub fn abort(&self) {
        if self.running.load(Ordering::Acquire) {
            self.cancellation
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .cancel();
        }
    }

    #[must_use]
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }
}

fn validate_allowlisted_command(
    command: &str,
    allowed_programs: Option<&[String]>,
) -> Result<(), ToolError> {
    if command.len() > MAX_COMMAND_BYTES {
        return Err(ToolError::InvalidArguments {
            tool: "bash".into(),
            message: format!("command exceeds the {MAX_COMMAND_BYTES}-byte limit"),
        });
    }
    let allowed_programs = allowed_programs.filter(|programs| !programs.is_empty());

    if command
        .chars()
        .any(|character| character.is_control() && character != '\t' && character != '\n')
    {
        return Err(ToolError::InvalidArguments {
            tool: "bash".into(),
            message: "command contains control characters".into(),
        });
    }
    let program = command.split_whitespace().next().unwrap_or_default();
    if program.is_empty() {
        return Err(ToolError::InvalidArguments {
            tool: "bash".into(),
            message: "command must start with a program".into(),
        });
    }
    if allowed_programs.is_some_and(|allowed| !allowed.iter().any(|candidate| candidate == program))
    {
        return Err(ToolError::Disabled {
            tool: format!("bash program {program}"),
        });
    }
    Ok(())
}

fn spawn_shell(command: &str, workspace: &Path) -> Result<Child, ToolError> {
    let mut process = Command::new("/bin/sh");
    process
        .args(["-c", command])
        .current_dir(workspace)
        .env_clear()
        .env("PATH", "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin")
        .env("LANG", "C.UTF-8")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    {
        std::os::unix::process::CommandExt::process_group(process.as_std_mut(), 0);
    }
    process.spawn().map_err(|error| ToolError::Execution {
        tool: "bash".into(),
        message: error.to_string(),
    })
}

async fn capture_shell(
    mut child: Child,
    timeout: Duration,
    display_limit: usize,
    cancellation: &CancellationToken,
    output: Option<&mpsc::Sender<String>>,
) -> Result<BashResult, ToolError> {
    let stdout = child.stdout.take().ok_or_else(|| ToolError::Execution {
        tool: "bash".into(),
        message: "stdout pipe unavailable".into(),
    })?;
    let stderr = child.stderr.take().ok_or_else(|| ToolError::Execution {
        tool: "bash".into(),
        message: "stderr pipe unavailable".into(),
    })?;
    let (events_tx, mut events_rx) = mpsc::channel(8);
    let stdout_task = tokio::spawn(read_pipe(stdout, events_tx.clone()));
    let stderr_task = tokio::spawn(read_pipe(stderr, events_tx));
    let deadline = sleep_until(Instant::now() + timeout);
    tokio::pin!(deadline);
    let mut status = None;
    let mut display = Vec::with_capacity(display_limit.min(64 * 1024));
    let mut total_bytes = 0_usize;
    let mut full_log: Option<(PathBuf, tokio::fs::File)> = None;
    let mut cancelled = false;
    let mut timed_out = false;
    let mut truncated = false;
    let mut open_pipes = 2_u8;

    while status.is_none() || open_pipes != 0 {
        let event = tokio::select! {
            biased;
            () = cancellation.cancelled(), if status.is_none() && !cancelled && !timed_out => {
                CaptureEvent::Cancelled
            }
            () = &mut deadline, if status.is_none() && !cancelled && !timed_out => {
                CaptureEvent::TimedOut
            }
            result = child.wait(), if status.is_none() => CaptureEvent::Exited(result),
            event = events_rx.recv(), if open_pipes != 0 => CaptureEvent::Pipe(event),
        };
        match event {
            CaptureEvent::Cancelled => {
                cancelled = true;
                terminate_group(&mut child).await;
                status = child.wait().await.ok();
            }
            CaptureEvent::TimedOut => {
                timed_out = true;
                terminate_group(&mut child).await;
                status = child.wait().await.ok();
            }
            CaptureEvent::Exited(result) => status = Some(result?),
            CaptureEvent::Pipe(event) => match event {
                Some(PipeEvent::Chunk(chunk)) => {
                    if let Some(output) = output {
                        let sanitized = sanitize_output(&chunk);
                        if !sanitized.is_empty() {
                            let _ = output.send(sanitized).await;
                        }
                    }
                    if total_bytes.saturating_add(chunk.len()) > display_limit {
                        truncated = true;
                        if full_log.is_none() {
                            full_log = create_full_log(&display).await;
                        }
                    }
                    let remaining_log = MAX_FULL_LOG_BYTES.saturating_sub(total_bytes);
                    let accepted = &chunk[..chunk.len().min(remaining_log)];
                    if let Some((_, file)) = &mut full_log {
                        file.write_all(accepted).await?;
                    }
                    total_bytes = total_bytes.saturating_add(chunk.len());
                    append_tail(&mut display, &chunk, display_limit);
                    if chunk.len() > remaining_log && status.is_none() {
                        truncated = true;
                        terminate_group(&mut child).await;
                        status = child.wait().await.ok();
                    }
                }
                Some(PipeEvent::Closed) => open_pipes = open_pipes.saturating_sub(1),
                None => open_pipes = 0,
            },
        }
    }
    let _ = stdout_task.await;
    let _ = stderr_task.await;
    if let Some((_, file)) = &mut full_log {
        file.sync_all().await?;
    }
    Ok(BashResult {
        output: sanitize_output(&display),
        exit_code: if cancelled || timed_out {
            None
        } else {
            status.and_then(|value| value.code())
        },
        cancelled,
        truncated,
        full_output_path: full_log.map(|(path, _)| path),
        timed_out,
    })
}

async fn create_full_log(existing: &[u8]) -> Option<(PathBuf, tokio::fs::File)> {
    let path = std::env::temp_dir().join(format!("mimir-bash-{}.log", Uuid::new_v4()));
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .await
        .ok()?;
    file.write_all(existing).await.ok()?;
    Some((path, file))
}

fn append_tail(target: &mut Vec<u8>, chunk: &[u8], limit: usize) {
    if limit == 0 {
        target.clear();
    } else if chunk.len() >= limit {
        target.clear();
        target.extend_from_slice(&chunk[chunk.len() - limit..]);
    } else {
        let overflow = target
            .len()
            .saturating_add(chunk.len())
            .saturating_sub(limit);
        if overflow > 0 {
            target.drain(..overflow.min(target.len()));
        }
        target.extend_from_slice(chunk);
    }
}

fn sanitize_output(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .chars()
        .filter(|character| *character == '\n' || *character == '\t' || !character.is_control())
        .collect()
}

enum PipeEvent {
    Chunk(Vec<u8>),
    Closed,
}

enum CaptureEvent {
    Cancelled,
    TimedOut,
    Exited(std::io::Result<std::process::ExitStatus>),
    Pipe(Option<PipeEvent>),
}

async fn read_pipe<R>(mut reader: R, events: mpsc::Sender<PipeEvent>) -> std::io::Result<()>
where
    R: AsyncRead + Unpin,
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

async fn terminate_group(child: &mut Child) {
    if let Some(id) = child.id().and_then(|id| i32::try_from(id).ok()) {
        let _ = killpg(Pid::from_raw(id), Signal::SIGKILL);
    }
    let _ = child.kill().await;
    let _ = child.wait().await;
}
