use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use nix::{
    sys::signal::{Signal, killpg},
    unistd::Pid,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::Mutex,
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

use crate::model::ToolDefinition;

use super::{
    BashRunner, ObservationStatus, Tool, ToolError, ToolObservation, ToolPolicy,
    WorkspacePathPolicy, object_schema, parse_input, truncate_utf8,
};

const MAX_CODE_BYTES: usize = 256 * 1024;
const MAX_PROTOCOL_BYTES: usize = 1024 * 1024;
const KERNEL_BOOT_TIMEOUT: Duration = Duration::from_secs(10);
const INTERRUPT_GRACE: Duration = Duration::from_secs(2);

const PYTHON_BRIDGE: &str = r#"
import ast, asyncio, base64, contextlib, inspect, io, json, os, sys, time, traceback

LIMIT = max(1024, int(os.environ.get("MIMIR_KERNEL_OUTPUT_LIMIT", "65536")))
NS = {"__name__": "__main__"}

class BoundedText(io.TextIOBase):
    def __init__(self):
        self.parts = []
        self.size = 0
        self.truncated = False
    def write(self, value):
        value = str(value)
        remaining = LIMIT - self.size
        if remaining > 0:
            encoded = value.encode("utf-8", "replace")
            accepted = encoded[:remaining].decode("utf-8", "ignore")
            self.parts.append(accepted)
            self.size += len(accepted.encode("utf-8"))
        if len(value.encode("utf-8", "replace")) > max(remaining, 0):
            self.truncated = True
        return len(value)
    def flush(self):
        return None
    def value(self):
        return "".join(self.parts)

def bounded(value):
    value = str(value)
    encoded = value.encode("utf-8", "replace")
    return encoded[:LIMIT].decode("utf-8", "ignore"), len(encoded) > LIMIT

def rich(value):
    outputs = []
    for method, mime, binary in [
        ("_repr_markdown_", "text/markdown", False),
        ("_repr_html_", "text/html", False),
        ("_repr_svg_", "image/svg+xml", False),
        ("_repr_png_", "image/png", True),
        ("_repr_jpeg_", "image/jpeg", True),
    ]:
        callback = getattr(value, method, None)
        if not callable(callback):
            continue
        try:
            data = callback()
            if data is None:
                continue
            if isinstance(data, tuple):
                data = data[0]
            if binary:
                if not isinstance(data, (bytes, bytearray)):
                    continue
                data = bytes(data)[:LIMIT]
                outputs.append({"mimeType": mime, "data": base64.b64encode(data).decode("ascii"), "base64": True})
            else:
                data, was_truncated = bounded(data)
                outputs.append({"mimeType": mime, "data": data, "base64": False, "truncated": was_truncated})
        except Exception:
            pass
    return outputs

def run_code(code):
    stdout, stderr = BoundedText(), BoundedText()
    started = time.monotonic()
    result = None
    rich_outputs = []
    status = "ok"
    error = None
    try:
        with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
            tree = ast.parse(code, "<mimir-ipython>", "exec")
            if tree.body and isinstance(tree.body[-1], ast.Expr):
                prefix = ast.Module(body=tree.body[:-1], type_ignores=[])
                if prefix.body:
                    pending = eval(compile(prefix, "<mimir-ipython>", "exec", flags=ast.PyCF_ALLOW_TOP_LEVEL_AWAIT), NS, NS)
                    if inspect.isawaitable(pending):
                        asyncio.run(pending)
                expression = ast.Expression(tree.body[-1].value)
                value = eval(compile(expression, "<mimir-ipython>", "eval", flags=ast.PyCF_ALLOW_TOP_LEVEL_AWAIT), NS, NS)
                if inspect.isawaitable(value):
                    value = asyncio.run(value)
                if value is not None:
                    result, result_truncated = bounded(repr(value))
                    rich_outputs = rich(value)
            else:
                pending = eval(compile(tree, "<mimir-ipython>", "exec", flags=ast.PyCF_ALLOW_TOP_LEVEL_AWAIT), NS, NS)
                if inspect.isawaitable(pending):
                    asyncio.run(pending)
    except KeyboardInterrupt:
        status = "aborted"
        error = {"ename": "KeyboardInterrupt", "evalue": "execution interrupted", "traceback": []}
    except BaseException as exc:
        status = "error"
        error = {"ename": type(exc).__name__, "evalue": str(exc)[:LIMIT], "traceback": traceback.format_exc().splitlines()[-64:]}
    return {
        "status": status,
        "stdout": stdout.value(),
        "stderr": stderr.value(),
        "result": result,
        "richOutputs": rich_outputs,
        "durationMs": int((time.monotonic() - started) * 1000),
        "truncated": stdout.truncated or stderr.truncated or bool(locals().get("result_truncated", False)),
        "error": error,
    }

sys.stdout.write(json.dumps({"type": "ready", "version": 1}) + "\n")
sys.stdout.flush()
for line in sys.stdin:
    try:
        request = json.loads(line)
        if request.get("type") == "shutdown":
            break
        response = run_code(request["code"])
        response["id"] = request["id"]
    except BaseException as exc:
        response = {"id": request.get("id") if isinstance(request, dict) else None, "status": "error", "stdout": "", "stderr": "", "result": None, "richOutputs": [], "durationMs": 0, "truncated": False, "error": {"ename": type(exc).__name__, "evalue": str(exc), "traceback": traceback.format_exc().splitlines()[-64:]}}
    sys.stdout.write(json.dumps(response, separators=(",", ":")) + "\n")
    sys.stdout.flush()
"#;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct IpythonInput {
    code: String,
}

#[derive(Debug, Serialize)]
struct KernelRequest<'a> {
    id: u64,
    code: &'a str,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct KernelResponse {
    id: u64,
    status: String,
    #[serde(default)]
    stdout: String,
    #[serde(default)]
    stderr: String,
    result: Option<String>,
    #[serde(default)]
    rich_outputs: Vec<RichOutput>,
    duration_ms: u64,
    #[serde(default)]
    truncated: bool,
    error: Option<KernelError>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RichOutput {
    mime_type: String,
    data: String,
    #[serde(default)]
    base64: bool,
    #[serde(default)]
    truncated: bool,
}

#[derive(Debug, Deserialize)]
struct KernelError {
    ename: String,
    evalue: String,
    #[serde(default)]
    traceback: Vec<String>,
}

enum KernelWaitResult {
    Response(Result<Vec<u8>, ToolError>),
    Cancelled,
    TimedOut,
}

pub(super) struct IpythonTool {
    kernel: PersistentPythonKernel,
    bash: BashRunner,
    policy: ToolPolicy,
    artifacts: PathBuf,
}

impl IpythonTool {
    pub(super) fn new(
        workspace: &Path,
        state_root: &Path,
        session: &str,
        policy: ToolPolicy,
    ) -> Result<Self, ToolError> {
        if !policy.allow_process || policy.allowed_programs.as_ref().is_some_and(Vec::is_empty) {
            return Err(ToolError::Disabled {
                tool: "ipython".into(),
            });
        }
        if session.is_empty()
            || session.len() > 128
            || !session
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
            || matches!(session, "." | "..")
        {
            return Err(ToolError::InvalidArguments {
                tool: "ipython".into(),
                message: "unsafe session identifier".into(),
            });
        }
        let paths = WorkspacePathPolicy::new(workspace)?;
        let python = resolve_python()?;
        let artifacts = crate::atomic::canonical_state_root(state_root)
            .join("kernels")
            .join(session);
        Ok(Self {
            kernel: PersistentPythonKernel::new(
                paths.root().to_owned(),
                python,
                policy.command_timeout,
                policy.max_output_bytes,
            ),
            bash: BashRunner::new(paths.root(), policy.clone())?,
            policy,
            artifacts,
        })
    }

    async fn execute_inner(
        &self,
        input: Value,
        cancellation: &CancellationToken,
    ) -> Result<ToolObservation, ToolError> {
        let input: IpythonInput = parse_input("ipython", input)?;
        if input.code.trim().is_empty() {
            return Err(ToolError::InvalidArguments {
                tool: "ipython".into(),
                message: "code must not be blank".into(),
            });
        }
        if input.code.len() > MAX_CODE_BYTES {
            return Err(ToolError::InvalidArguments {
                tool: "ipython".into(),
                message: format!("code exceeds the {MAX_CODE_BYTES}-byte limit"),
            });
        }
        if let Some(shell) = bash_cell(&input.code) {
            return self.execute_bash(shell, cancellation).await;
        }
        let response = self.kernel.execute(&input.code, cancellation).await?;
        self.response_observation(response).await
    }

    async fn execute_bash(
        &self,
        code: &str,
        cancellation: &CancellationToken,
    ) -> Result<ToolObservation, ToolError> {
        let execution = self.bash.execute(code);
        tokio::pin!(execution);
        let result = tokio::select! {
            result = &mut execution => result?,
            () = cancellation.cancelled() => {
                self.bash.abort();
                execution.await?
            }
        };
        let status = if result.cancelled || result.timed_out || result.exit_code != Some(0) {
            ObservationStatus::Error
        } else {
            ObservationStatus::Success
        };
        Ok(ToolObservation {
            status,
            summary: if result.cancelled {
                "IPython bash cell aborted".into()
            } else if result.timed_out {
                format!(
                    "IPython bash cell timed out after {} ms",
                    self.policy.command_timeout.as_millis()
                )
            } else {
                format!(
                    "IPython bash cell exited with {}",
                    result
                        .exit_code
                        .map_or_else(|| "signal".into(), |code| code.to_string())
                )
            },
            next_actions: Vec::new(),
            artifacts: result.full_output_path.into_iter().collect(),
            content: result.output,
        })
    }

    async fn response_observation(
        &self,
        response: KernelResponse,
    ) -> Result<ToolObservation, ToolError> {
        let mut sections = Vec::new();
        if !response.stdout.is_empty() {
            sections.push(response.stdout.clone());
        }
        if !response.stderr.is_empty() {
            sections.push(response.stderr.clone());
        }
        if let Some(result) = &response.result {
            sections.push(result.clone());
        }
        let mut artifacts = Vec::new();
        for (index, rich) in response.rich_outputs.iter().enumerate() {
            if rich.base64 && matches!(rich.mime_type.as_str(), "image/png" | "image/jpeg") {
                if let Some(path) = self.persist_rich_image(response.id, index, rich).await? {
                    sections.push(format!(
                        "[{} output saved to {}]",
                        rich.mime_type,
                        path.display()
                    ));
                    artifacts.push(path);
                }
            } else {
                sections.push(format!(
                    "<rich_output mime_type=\"{}\"{}>\n{}\n</rich_output>",
                    rich.mime_type,
                    if rich.truncated {
                        " truncated=\"true\""
                    } else {
                        ""
                    },
                    rich.data
                ));
            }
        }
        if let Some(error) = &response.error {
            if error.traceback.is_empty() {
                sections.push(format!("{}: {}", error.ename, error.evalue));
            } else {
                sections.push(error.traceback.join("\n"));
            }
        }
        let (content, content_truncated) =
            truncate_utf8(&sections.join("\n"), self.policy.max_output_bytes);
        let truncated = response.truncated || content_truncated;
        let status = if response.status == "ok" && !truncated {
            ObservationStatus::Success
        } else {
            ObservationStatus::Error
        };
        Ok(ToolObservation {
            status,
            summary: format!(
                "IPython execution {} in {} ms{}",
                response.status,
                response.duration_ms,
                if truncated { "; output truncated" } else { "" }
            ),
            next_actions: if truncated {
                vec!["Produce a smaller output or write large data to a workspace file".into()]
            } else {
                Vec::new()
            },
            artifacts,
            content,
        })
    }

    async fn persist_rich_image(
        &self,
        request_id: u64,
        index: usize,
        output: &RichOutput,
    ) -> Result<Option<PathBuf>, ToolError> {
        let bytes = BASE64
            .decode(&output.data)
            .map_err(|error| ToolError::Execution {
                tool: "ipython".into(),
                message: format!("invalid rich image encoding: {error}"),
            })?;
        if bytes.len() > self.policy.max_output_bytes {
            return Ok(None);
        }
        let extension = if output.mime_type == "image/png" {
            "png"
        } else {
            "jpg"
        };
        let path = self
            .artifacts
            .join(format!("output-{request_id}-{index}.{extension}"));
        let state_root = self
            .artifacts
            .parent()
            .and_then(Path::parent)
            .ok_or_else(|| ToolError::Execution {
                tool: "ipython".into(),
                message: "kernel artifact root is invalid".into(),
            })?;
        crate::atomic::prepare_state_path(state_root, &path)
            .await
            .map_err(|error| ToolError::Execution {
                tool: "ipython".into(),
                message: error.to_string(),
            })?;
        tokio::fs::write(&path, bytes).await?;
        Ok(Some(path))
    }
}

#[async_trait]
impl Tool for IpythonTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "ipython".into(),
            description: "Execute Python scratchpad code or %%bash cells in a persistent, session-scoped kernel. Variables and imports persist across calls. Process execution must be explicitly enabled.".into(),
            parameters: object_schema(
                &json!({
                    "code": {
                        "type": "string",
                        "description": "Python scratchpad code or a %%bash shell cell"
                    }
                }),
                &["code"],
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

fn bash_cell(code: &str) -> Option<&str> {
    let trimmed = code.trim_start();
    let rest = trimmed.strip_prefix("%%bash")?;
    let newline = rest.find('\n')?;
    (rest[..newline].trim().is_empty()).then(|| &rest[newline + 1..])
}

fn resolve_python() -> Result<PathBuf, ToolError> {
    if let Some(configured) = std::env::var_os("MIMIR_KERNEL_PYTHON") {
        let path = PathBuf::from(configured);
        if !path.is_absolute() {
            return Err(ToolError::Execution {
                tool: "ipython".into(),
                message: "MIMIR_KERNEL_PYTHON must be an absolute path".into(),
            });
        }
        return validate_python(&path);
    }
    for path in [
        "/opt/homebrew/bin/python3",
        "/usr/local/bin/python3",
        "/usr/bin/python3",
    ] {
        if Path::new(path).is_file() {
            return validate_python(Path::new(path));
        }
    }
    Err(ToolError::Execution {
        tool: "ipython".into(),
        message: "no supported Python 3 executable found; set MIMIR_KERNEL_PYTHON".into(),
    })
}

fn validate_python(path: &Path) -> Result<PathBuf, ToolError> {
    let canonical = std::fs::canonicalize(path).map_err(|error| ToolError::Execution {
        tool: "ipython".into(),
        message: format!(
            "Python executable {} is inaccessible: {error}",
            path.display()
        ),
    })?;
    if !canonical.is_file() {
        return Err(ToolError::Execution {
            tool: "ipython".into(),
            message: "configured Python executable is not a regular file".into(),
        });
    }
    Ok(canonical)
}

struct PersistentPythonKernel {
    workspace: PathBuf,
    python: PathBuf,
    timeout: Duration,
    output_limit: usize,
    next_id: AtomicU64,
    process: Mutex<Option<KernelProcess>>,
}

impl PersistentPythonKernel {
    fn new(workspace: PathBuf, python: PathBuf, timeout: Duration, output_limit: usize) -> Self {
        Self {
            workspace,
            python,
            timeout,
            output_limit: output_limit.clamp(1024, 256 * 1024),
            next_id: AtomicU64::new(1),
            process: Mutex::new(None),
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "the serialized request, interrupt, timeout, protocol reset, and response-id checks form one kernel transaction"
    )]
    async fn execute(
        &self,
        code: &str,
        cancellation: &CancellationToken,
    ) -> Result<KernelResponse, ToolError> {
        let mut process = self.process.lock().await;
        if process.is_none() {
            *process = Some(self.start().await?);
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request = serde_json::to_vec(&KernelRequest { id, code }).map_err(|error| {
            ToolError::Execution {
                tool: "ipython".into(),
                message: error.to_string(),
            }
        })?;
        let running = process.as_mut().expect("kernel initialized");
        if let Err(error) = async {
            running.stdin.write_all(&request).await?;
            running.stdin.write_all(b"\n").await?;
            running.stdin.flush().await
        }
        .await
        {
            terminate_kernel(process.take()).await;
            return Err(ToolError::Execution {
                tool: "ipython".into(),
                message: format!("failed to send kernel request; kernel was restarted: {error}"),
            });
        }

        let wait = tokio::select! {
            response = read_bounded_line(&mut running.stdout, MAX_PROTOCOL_BYTES) => KernelWaitResult::Response(response),
            () = cancellation.cancelled() => KernelWaitResult::Cancelled,
            () = tokio::time::sleep(self.timeout) => KernelWaitResult::TimedOut,
        };
        let bytes = match wait {
            KernelWaitResult::Response(response) => match response {
                Ok(response) => response,
                Err(error) => {
                    terminate_kernel(process.take()).await;
                    return Err(ToolError::Execution {
                        tool: "ipython".into(),
                        message: format!(
                            "failed to read kernel response; kernel was restarted: {error}"
                        ),
                    });
                }
            },
            KernelWaitResult::Cancelled => {
                interrupt_kernel(&mut running.child);
                match tokio::time::timeout(
                    INTERRUPT_GRACE,
                    read_bounded_line(&mut running.stdout, MAX_PROTOCOL_BYTES),
                )
                .await
                {
                    Ok(Ok(response)) => response,
                    Ok(Err(error)) => {
                        terminate_kernel(process.take()).await;
                        return Err(ToolError::Execution {
                            tool: "ipython".into(),
                            message: format!(
                                "execution aborted and the kernel protocol failed; kernel was restarted: {error}"
                            ),
                        });
                    }
                    Err(_) => {
                        terminate_kernel(process.take()).await;
                        return Err(ToolError::Execution {
                            tool: "ipython".into(),
                            message: "execution aborted; unresponsive kernel was restarted".into(),
                        });
                    }
                }
            }
            KernelWaitResult::TimedOut => {
                terminate_kernel(process.take()).await;
                return Err(ToolError::Execution {
                    tool: "ipython".into(),
                    message: format!(
                        "execution timed out after {} ms; kernel was restarted",
                        self.timeout.as_millis()
                    ),
                });
            }
        };
        let response: KernelResponse = match serde_json::from_slice(&bytes) {
            Ok(response) => response,
            Err(error) => {
                terminate_kernel(process.take()).await;
                return Err(ToolError::Execution {
                    tool: "ipython".into(),
                    message: format!("invalid kernel response; kernel was restarted: {error}"),
                });
            }
        };
        if response.id != id {
            terminate_kernel(process.take()).await;
            return Err(ToolError::Execution {
                tool: "ipython".into(),
                message: "kernel response id mismatch; kernel was restarted".into(),
            });
        }
        Ok(response)
    }

    async fn start(&self) -> Result<KernelProcess, ToolError> {
        let mut command = Command::new(&self.python);
        command
            .args(["-u", "-c", PYTHON_BRIDGE])
            .current_dir(&self.workspace)
            .env_clear()
            .env("PATH", "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin")
            .env("LANG", "C.UTF-8")
            .env("PYTHONNOUSERSITE", "1")
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .env("MIMIR_KERNEL_OUTPUT_LIMIT", self.output_limit.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        {
            std::os::unix::process::CommandExt::process_group(command.as_std_mut(), 0);
        }
        let mut child = command.spawn().map_err(|error| ToolError::Execution {
            tool: "ipython".into(),
            message: format!("failed to start Python kernel: {error}"),
        })?;
        let stdin = child.stdin.take().ok_or_else(|| ToolError::Execution {
            tool: "ipython".into(),
            message: "kernel stdin is unavailable".into(),
        })?;
        let stdout = child.stdout.take().ok_or_else(|| ToolError::Execution {
            tool: "ipython".into(),
            message: "kernel stdout is unavailable".into(),
        })?;
        let stderr = child.stderr.take().ok_or_else(|| ToolError::Execution {
            tool: "ipython".into(),
            message: "kernel stderr is unavailable".into(),
        })?;
        let stderr_task = tokio::spawn(async move {
            let mut stderr = BufReader::new(stderr);
            let mut sink = Vec::new();
            let _ = tokio::io::AsyncReadExt::read_to_end(
                &mut tokio::io::AsyncReadExt::take(&mut stderr, MAX_PROTOCOL_BYTES as u64),
                &mut sink,
            )
            .await;
        });
        let mut process = KernelProcess {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            stderr_task,
        };
        let ready = tokio::time::timeout(
            KERNEL_BOOT_TIMEOUT,
            read_bounded_line(&mut process.stdout, 4096),
        )
        .await
        .map_err(|_| ToolError::Execution {
            tool: "ipython".into(),
            message: "Python kernel startup timed out".into(),
        })??;
        let ready: Value =
            serde_json::from_slice(&ready).map_err(|error| ToolError::Execution {
                tool: "ipython".into(),
                message: format!("invalid Python kernel handshake: {error}"),
            })?;
        if ready != json!({"type": "ready", "version": 1}) {
            return Err(ToolError::Execution {
                tool: "ipython".into(),
                message: "unexpected Python kernel handshake".into(),
            });
        }
        Ok(process)
    }
}

struct KernelProcess {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    stderr_task: JoinHandle<()>,
}

impl Drop for KernelProcess {
    fn drop(&mut self) {
        interrupt_kernel(&mut self.child);
        let _ = self.child.start_kill();
        self.stderr_task.abort();
    }
}

async fn read_bounded_line(
    reader: &mut BufReader<ChildStdout>,
    limit: usize,
) -> Result<Vec<u8>, ToolError> {
    let mut line = Vec::with_capacity(limit.min(64 * 1024));
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Err(ToolError::Execution {
                tool: "ipython".into(),
                message: "Python kernel closed its protocol stream".into(),
            });
        }
        let count = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |index| index + 1);
        if line.len().saturating_add(count) > limit {
            return Err(ToolError::Execution {
                tool: "ipython".into(),
                message: format!("kernel response exceeds the {limit}-byte limit"),
            });
        }
        line.extend_from_slice(&available[..count]);
        reader.consume(count);
        if line.last() == Some(&b'\n') {
            line.pop();
            return Ok(line);
        }
    }
}

fn interrupt_kernel(child: &mut Child) {
    #[cfg(unix)]
    if let Some(id) = child.id().and_then(|id| i32::try_from(id).ok()) {
        let _ = killpg(Pid::from_raw(id), Signal::SIGINT);
    }
}

async fn terminate_kernel(process: Option<KernelProcess>) {
    if let Some(mut process) = process {
        #[cfg(unix)]
        if let Some(id) = process.child.id().and_then(|id| i32::try_from(id).ok()) {
            let _ = killpg(Pid::from_raw(id), Signal::SIGKILL);
        }
        let _ = process.child.kill().await;
        let _ = process.child.wait().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bash_magic_requires_a_bare_first_line() {
        assert_eq!(bash_cell("%%bash\nprintf hi"), Some("printf hi"));
        assert_eq!(bash_cell("  %%bash\necho ok"), Some("echo ok"));
        assert_eq!(bash_cell("%%bash -e\necho no"), None);
        assert_eq!(bash_cell("print('python')"), None);
    }
}
