use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Mutex as StdMutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

#[cfg(unix)]
use async_trait::async_trait;
#[cfg(unix)]
use nix::{
    sys::signal::{Signal, killpg},
    unistd::Pid,
};
#[cfg(unix)]
use serde::Deserialize;
use serde::Serialize;
#[cfg(unix)]
use serde_json::{Value, json};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::{Child, Command},
    sync::{Mutex, mpsc},
    time::{Instant, sleep_until},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[cfg(unix)]
use crate::model::ToolDefinition;

#[cfg(unix)]
use super::{
    DestructiveAction, ObservationStatus, Tool, ToolObservation, object_schema, parse_input,
};
use super::{ToolError, ToolPolicy, WorkspacePathPolicy};

const MAX_FULL_LOG_BYTES: usize = 8 * 1024 * 1024;
const MAX_COMMAND_BYTES: usize = 64 * 1024;
const RTK_REWRITE_TIMEOUT: Duration = Duration::from_secs(1);

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

#[cfg(unix)]
pub(super) struct BashTool {
    runner: BashRunner,
    policy: ToolPolicy,
}

#[cfg(unix)]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BashInput {
    command: String,
}

#[cfg(unix)]
impl BashTool {
    pub fn new(workspace: &Path, policy: ToolPolicy) -> Result<Self, ToolError> {
        let mut runner_policy = policy.clone();
        runner_policy.allow_process = true;
        runner_policy.allow_any_program = true;
        runner_policy.approvals = None;
        Ok(Self {
            runner: BashRunner::new(workspace, runner_policy)?,
            policy,
        })
    }

    async fn execute_inner(
        &self,
        input: Value,
        cancellation: Option<&CancellationToken>,
    ) -> Result<ToolObservation, ToolError> {
        let input: BashInput = parse_input("bash", input)?;
        let command = input.command.trim();
        if command.is_empty() {
            return Err(ToolError::InvalidArguments {
                tool: "bash".into(),
                message: "command must not be blank".into(),
            });
        }
        if !self.policy.agent_mode.automatically_approves()
            && let Some(approvals) = &self.policy.approvals
            && let Some(request) =
                approvals.requires_action_approval(DestructiveAction::ProcessExecution, command)?
        {
            return Err(ToolError::ApprovalRequired { request });
        }
        let result = if let Some(cancellation) = cancellation {
            self.runner
                .execute_cancellable(command, cancellation)
                .await?
        } else {
            self.runner.execute(command).await?
        };
        let (status, summary) = if result.cancelled {
            (ObservationStatus::Error, "bash command cancelled".into())
        } else if result.timed_out {
            (
                ObservationStatus::Error,
                format!(
                    "bash command timed out after {} ms",
                    self.policy.command_timeout.as_millis()
                ),
            )
        } else if result.exit_code == Some(0) {
            (ObservationStatus::Success, "bash command completed".into())
        } else {
            (
                ObservationStatus::Error,
                result.exit_code.map_or_else(
                    || "bash ended without an exit code".into(),
                    |code| format!("bash exited with code {code}"),
                ),
            )
        };
        Ok(ToolObservation {
            status,
            summary,
            next_actions: if result.timed_out {
                vec![
                    "For a long-running server, start it in the background and redirect stdout and stderr to a workspace log file"
                        .into(),
                ]
            } else {
                Vec::new()
            },
            artifacts: result.full_output_path.into_iter().collect(),
            content: result.output,
        })
    }
}

#[cfg(unix)]
#[async_trait]
impl Tool for BashTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "bash".into(),
            description: "Run a Bash command in $WORKSPACE. Supports pipes, redirects, command chaining, environment assignments, and background jobs. For a long-running server, background it and redirect stdout and stderr to a workspace log file. Default mode asks for approval before each command; auto mode runs commands immediately. Shell execution is not an OS sandbox.".into(),
            parameters: object_schema(
                &json!({
                    "command": {
                        "type": "string",
                        "description": "The Bash command to run in $WORKSPACE"
                    }
                }),
                &["command"],
            ),
        }
    }

    async fn execute(&self, input: Value) -> Result<ToolObservation, ToolError> {
        self.execute_inner(input, None).await
    }

    #[cfg(unix)]
    async fn execute_cancellable(
        &self,
        input: Value,
        cancellation: &CancellationToken,
    ) -> Result<ToolObservation, ToolError> {
        self.execute_inner(input, Some(cancellation)).await
    }
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
        self.execute_inner(command, None, None).await
    }

    async fn execute_cancellable(
        &self,
        command: &str,
        cancellation: &CancellationToken,
    ) -> Result<BashResult, ToolError> {
        self.execute_inner(command, None, Some(cancellation)).await
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
        self.execute_inner(command, Some(output), None).await
    }

    async fn execute_inner(
        &self,
        command: &str,
        output: Option<mpsc::Sender<String>>,
        external_cancellation: Option<&CancellationToken>,
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
        validate_allowlisted_command(
            command,
            self.policy.allowed_programs.as_deref(),
            self.policy.allow_any_program,
        )?;
        if !self.policy.agent_mode.automatically_approves()
            && let Some(approvals) = &self.policy.approvals
            && let Some(request) = approvals.requires_approval(command)?
        {
            return Err(ToolError::ApprovalRequired { request });
        }
        let _guard = self.run_lock.lock().await;
        let cancellation = CancellationToken::new();
        let cancellation_forwarder = external_cancellation.map(|external| {
            let external = external.clone();
            let cancellation = cancellation.clone();
            tokio::spawn(async move {
                external.cancelled().await;
                cancellation.cancel();
            })
        });
        *self
            .cancellation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = cancellation.clone();
        self.running.store(true, Ordering::Release);

        // RTK is an optional companion binary. Mimir owns authorization for
        // the original command; RTK only rewrites that approved command into
        // an output-filtering proxy invocation. Missing, outdated, denied, or
        // slow RTK installations therefore degrade to the original command.
        let rewritten = tokio::select! {
            biased;
            () = cancellation.cancelled() => None,
            rewritten = rewrite_with_rtk(command, self.paths.root()) => rewritten,
        };
        let command = rewritten.as_deref().unwrap_or(command);
        let result = if cancellation.is_cancelled() {
            Ok(BashResult {
                output: String::new(),
                exit_code: None,
                cancelled: true,
                truncated: false,
                full_output_path: None,
                timed_out: false,
            })
        } else {
            let child = spawn_shell(command, self.paths.root()).inspect_err(|_error| {
                self.running.store(false, Ordering::Release);
            })?;
            capture_shell(
                child,
                self.policy.command_timeout,
                self.policy.max_output_bytes,
                &cancellation,
                output.as_ref(),
            )
            .await
        };
        if let Some(forwarder) = cancellation_forwarder {
            forwarder.abort();
        }
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
    allow_any_program: bool,
) -> Result<(), ToolError> {
    if command.len() > MAX_COMMAND_BYTES {
        return Err(ToolError::InvalidArguments {
            tool: "bash".into(),
            message: format!("command exceeds the {MAX_COMMAND_BYTES}-byte limit"),
        });
    }
    if command
        .chars()
        .any(|character| character.is_control() && character != '\t' && character != '\n')
    {
        return Err(ToolError::InvalidArguments {
            tool: "bash".into(),
            message: "command contains control characters".into(),
        });
    }
    let program = command
        .split_whitespace()
        .find(|token| *token != "RTK_DISABLED=1")
        .unwrap_or_default();
    if program.is_empty() {
        return Err(ToolError::InvalidArguments {
            tool: "bash".into(),
            message: "command must start with a program".into(),
        });
    }
    if allow_any_program {
        return Ok(());
    }
    let allowed_programs = allowed_programs
        .filter(|programs| !programs.is_empty())
        .ok_or_else(|| ToolError::Disabled {
            tool: "bash: no programs were explicitly allowlisted".into(),
        })?;
    if !allowed_programs
        .iter()
        .any(|candidate| candidate == program)
    {
        return Err(ToolError::Disabled {
            tool: format!("bash program {program}"),
        });
    }
    Ok(())
}

fn spawn_shell(command: &str, workspace: &Path) -> Result<Child, ToolError> {
    let mut process = Command::new("/bin/bash");
    process
        .args(["-c", command])
        .current_dir(workspace)
        .env_clear()
        .env("PATH", shell_path())
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

fn shell_path() -> OsString {
    std::env::var_os("PATH")
        .filter(|path| !path.is_empty())
        .unwrap_or_else(|| OsString::from("/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin"))
}

async fn rewrite_with_rtk(command: &str, workspace: &Path) -> Option<String> {
    rewrite_with_rtk_binary(std::ffi::OsStr::new("rtk"), command, workspace).await
}

async fn rewrite_with_rtk_binary(
    binary: &std::ffi::OsStr,
    command: &str,
    workspace: &Path,
) -> Option<String> {
    let mut process = Command::new(binary);
    process
        .args(["rewrite", command])
        .current_dir(workspace)
        .env_clear()
        .env("PATH", shell_path())
        .env("LANG", "C.UTF-8")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(RTK_REWRITE_TIMEOUT, process.output())
        .await
        .ok()?
        .ok()?;
    if !matches!(output.status.code(), Some(0 | 3)) {
        return None;
    }
    let rewritten = String::from_utf8(output.stdout).ok()?;
    let rewritten = rewritten.trim();
    if rewritten.is_empty()
        || rewritten.len() > MAX_COMMAND_BYTES
        || rewritten
            .chars()
            .any(|character| character.is_control() && character != '\t' && character != '\n')
    {
        return None;
    }
    tracing::debug!(
        target: "mimir::tools::bash",
        original_bytes = command.len(),
        rewritten_bytes = rewritten.len(),
        "RTK rewrote bash command"
    );
    Some(rewritten.to_owned())
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
    #[cfg(unix)]
    if let Some(id) = child.id().and_then(|id| i32::try_from(id).ok()) {
        let _ = killpg(Pid::from_raw(id), Signal::SIGKILL);
    }
    let _ = child.kill().await;
    let _ = child.wait().await;
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use tempfile::TempDir;

    use super::{rewrite_with_rtk_binary, validate_allowlisted_command};

    fn fake_rtk(root: &TempDir, body: &str) -> std::path::PathBuf {
        let path = root.path().join("rtk");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("fake rtk");
        let mut permissions = std::fs::metadata(&path).expect("metadata").permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&path, permissions).expect("permissions");
        path
    }

    #[tokio::test]
    async fn rtk_rewrite_accepts_allow_and_ask_protocol_outcomes() {
        let root = TempDir::new().expect("tempdir");
        for (name, exit_code) in [("allow", 0), ("ask", 3)] {
            let directory = TempDir::new_in(root.path()).expect("case tempdir");
            let binary = fake_rtk(
                &directory,
                &format!(
                    "[ \"$1\" = rewrite ] || exit 9\n[ \"$2\" = 'cargo test' ] || exit 8\nprintf 'rtk cargo test'\nexit {exit_code}"
                ),
            );
            assert_eq!(
                rewrite_with_rtk_binary(binary.as_os_str(), "cargo test", root.path()).await,
                Some("rtk cargo test".into()),
                "{name} outcome"
            );
        }
    }

    #[tokio::test]
    async fn rtk_rewrite_falls_back_for_passthrough_and_invalid_output() {
        let missing_root = TempDir::new().expect("tempdir");
        assert_eq!(
            rewrite_with_rtk_binary(
                missing_root.path().join("missing-rtk").as_os_str(),
                "cargo test",
                missing_root.path(),
            )
            .await,
            None
        );

        let root = TempDir::new().expect("tempdir");
        let passthrough = fake_rtk(&root, "exit 1");
        assert_eq!(
            rewrite_with_rtk_binary(passthrough.as_os_str(), "printf ok", root.path()).await,
            None
        );

        let denied_root = TempDir::new().expect("tempdir");
        let denied = fake_rtk(&denied_root, "exit 2");
        assert_eq!(
            rewrite_with_rtk_binary(denied.as_os_str(), "cargo test", denied_root.path()).await,
            None
        );

        let invalid_root = TempDir::new().expect("tempdir");
        let invalid = fake_rtk(&invalid_root, "printf '\\001bad'\nexit 0");
        assert_eq!(
            rewrite_with_rtk_binary(invalid.as_os_str(), "cargo test", invalid_root.path()).await,
            None
        );
    }

    #[test]
    fn rtk_single_command_bypass_keeps_program_allowlist_enforced() {
        let allowed = ["cargo".to_owned()];
        assert!(
            validate_allowlisted_command("RTK_DISABLED=1 cargo test", Some(&allowed), false)
                .is_ok()
        );
        assert!(
            validate_allowlisted_command("RTK_DISABLED=1 git status", Some(&allowed), false)
                .is_err()
        );
    }
}
