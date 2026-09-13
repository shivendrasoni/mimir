use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, Ordering},
    },
};

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use futures::StreamExt;
use serde_json::{Map, Value, json};
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt},
    sync::Mutex,
    task::JoinSet,
};
use tokio_util::codec::{FramedRead, LinesCodec};
use uuid::Uuid;

use crate::{
    error::{MimirError, Result},
    model::{Content, Message, Role, StopReason},
    runtime::{AgentRuntime, EventSink, RuntimeEvent},
    runtime_events::{RuntimeEventEnvelope, RuntimeEventSource},
    tools::{ObservationStatus, ToolObservation},
};

pub const ACP_PROTOCOL_VERSION: u16 = 1;
pub const MIMIR_META_NAMESPACE: &str = "ai.mimir";
const MAX_ACP_FRAME_BYTES: usize = 8 * 1024 * 1024;
const MAX_ACP_IMAGE_BYTES: usize = 5 * 1024 * 1024;
const MAX_ERROR_BYTES: usize = 4 * 1024;

struct JsonLineWriter<W> {
    inner: Arc<Mutex<W>>,
}

impl<W> Clone for JsonLineWriter<W> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<W> JsonLineWriter<W>
where
    W: AsyncWrite + Unpin,
{
    async fn send(&self, value: &Value) -> Result<()> {
        let mut encoded = serde_json::to_vec(value)?;
        encoded.push(b'\n');
        let mut writer = self.inner.lock().await;
        writer.write_all(&encoded).await?;
        writer.flush().await?;
        Ok(())
    }
}

#[derive(Default)]
struct ConnectionState {
    session: Option<AcpSession>,
}

struct AcpSession {
    id: String,
    active: Option<ActivePrompt>,
}

#[derive(Clone)]
struct ActivePrompt {
    id: Uuid,
    source: RuntimeEventSource,
    cancelled: Arc<AtomicBool>,
    autonomous: Arc<StdMutex<Option<AcpAutonomousStatus>>>,
}

#[derive(Debug, Clone)]
struct AcpAutonomousStatus {
    enabled: bool,
    limit: Option<String>,
}

/// Serves Agent Client Protocol JSON-RPC over a bounded NDJSON transport.
///
/// One transport owns one live session at a time, matching the reference
/// harness. Prompt turns run independently from the read loop so a client can
/// cancel a live provider request without waiting for it to finish.
///
/// # Errors
///
/// Returns only transport failures. Malformed or unsupported client requests
/// receive sanitized JSON-RPC errors and do not terminate the connection.
#[allow(
    clippy::too_many_lines,
    reason = "keeping the small ACP method matrix linear makes notification and response ordering auditable"
)]
pub async fn serve_acp<R, W>(
    runtime: Arc<AgentRuntime>,
    cwd: PathBuf,
    reader: R,
    writer: W,
) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Send + Unpin + 'static,
{
    let writer = JsonLineWriter {
        inner: Arc::new(Mutex::new(writer)),
    };
    let state = Arc::new(Mutex::new(ConnectionState::default()));
    let mut prompts = JoinSet::new();
    let mut frames = FramedRead::new(reader, LinesCodec::new_with_max_length(MAX_ACP_FRAME_BYTES));
    let mut runtime_events = runtime.subscribe_events();

    loop {
        let frame = tokio::select! {
            frame = frames.next() => {
                let Some(frame) = frame else { break; };
                frame
            }
            event = runtime_events.recv() => {
                match event {
                    Ok(event) => forward_broadcast_event(&state, &writer, event).await,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(count)) => {
                        forward_lag_update(&state, &writer, count).await;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
                continue;
            }
        };
        let line = match frame {
            Ok(line) => line,
            Err(error) => {
                writer
                    .send(&rpc_error(
                        Value::Null,
                        -32_700,
                        &format!("parse error: {error}"),
                    ))
                    .await?;
                continue;
            }
        };
        let value = match serde_json::from_str::<Value>(&line) {
            Ok(value) => value,
            Err(error) => {
                writer
                    .send(&rpc_error(
                        Value::Null,
                        -32_700,
                        &format!("parse error: {error}"),
                    ))
                    .await?;
                continue;
            }
        };
        let Some(object) = value.as_object() else {
            writer
                .send(&rpc_error(Value::Null, -32_600, "invalid request"))
                .await?;
            continue;
        };
        let id = object.get("id").cloned();
        let notification = id.is_none();
        let response_id = id.clone().unwrap_or(Value::Null);
        if object.get("jsonrpc") != Some(&Value::String("2.0".into())) {
            if !notification {
                writer
                    .send(&rpc_error(response_id, -32_600, "invalid request"))
                    .await?;
            }
            continue;
        }
        let Some(method) = object.get("method").and_then(Value::as_str) else {
            if !notification {
                writer
                    .send(&rpc_error(response_id, -32_600, "method must be a string"))
                    .await?;
            }
            continue;
        };
        let params = object.get("params").cloned().unwrap_or_else(|| json!({}));

        match method {
            "initialize" => {
                if let Some(id) = id {
                    writer.send(&rpc_success(id, initialize_result())).await?;
                }
            }
            "session/new" => {
                if notification {
                    continue;
                }
                let result = create_session(&state, &cwd, &params).await;
                let response = match result {
                    Ok(result) => rpc_success(response_id, result),
                    Err(error) => rpc_error(response_id, -32_000, &error.to_string()),
                };
                writer.send(&response).await?;
            }
            "session/prompt" => {
                if notification {
                    continue;
                }
                let prepared = prepare_prompt(&state, &params).await;
                let (session_id, active, message) = match prepared {
                    Ok(prepared) => prepared,
                    Err(error) => {
                        writer
                            .send(&rpc_error(response_id, -32_000, &error.to_string()))
                            .await?;
                        continue;
                    }
                };
                let runtime = Arc::clone(&runtime);
                let state = Arc::clone(&state);
                let prompt_writer = writer.clone();
                prompts.spawn(async move {
                    run_prompt(
                        runtime,
                        state,
                        prompt_writer,
                        response_id,
                        session_id,
                        active,
                        message,
                    )
                    .await;
                });
            }
            "session/cancel" => {
                cancel_prompt(&runtime, &state, &params).await;
            }
            "session/close" => {
                if notification {
                    continue;
                }
                let result = close_session(&runtime, &state, &params).await;
                let response = match result {
                    Ok(()) => rpc_success(response_id, json!({})),
                    Err(error) => rpc_error(response_id, -32_000, &error.to_string()),
                };
                writer.send(&response).await?;
            }
            _ if !notification => {
                writer
                    .send(&rpc_error(response_id, -32_601, "method not found"))
                    .await?;
            }
            _ => {}
        }
    }

    cancel_active(&runtime, &state).await;
    prompts.shutdown().await;
    Ok(())
}

fn initialize_result() -> Value {
    json!({
        "protocolVersion": ACP_PROTOCOL_VERSION,
        "agentCapabilities": {
            "loadSession": false,
            "promptCapabilities": {"image": true, "embeddedContext": true},
            "sessionCapabilities": {"close": {}}
        },
        "agentInfo": {
            "name": "mimir",
            "title": "Mimir",
            "version": env!("CARGO_PKG_VERSION")
        },
        "_meta": mimir_meta(json!({}))
    })
}

async fn create_session(
    state: &Mutex<ConnectionState>,
    cwd: &Path,
    params: &Value,
) -> Result<Value> {
    let object = require_params_object(params)?;
    let mut state = state.lock().await;
    if state.session.is_some() {
        return Err(MimirError::Protocol(
            "mimir ACP mode hosts one session per connection; start another mimir process for a second session"
                .into(),
        ));
    }
    let session_id = Uuid::new_v4().to_string();
    state.session = Some(AcpSession {
        id: session_id.clone(),
        active: None,
    });
    let cwd_meta = object
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|requested| !requested.is_empty())
        .filter(|requested| !same_path(Path::new(requested), cwd))
        .map(|requested| {
            mimir_meta(json!({
                "cwd": {
                    "requested": requested,
                    "actual": display_path(cwd)
                }
            }))
        });
    Ok(cwd_meta.map_or_else(
        || json!({"sessionId": session_id}),
        |meta| json!({"sessionId": session_id, "_meta": meta}),
    ))
}

async fn prepare_prompt(
    state: &Mutex<ConnectionState>,
    params: &Value,
) -> Result<(String, ActivePrompt, Message)> {
    let object = require_params_object(params)?;
    let session_id = required_string(object, "sessionId")?;
    let prompt = object
        .get("prompt")
        .and_then(Value::as_array)
        .ok_or_else(|| MimirError::Protocol("prompt must be an array".into()))?;
    let message = prompt_message(prompt)?;
    let mut state = state.lock().await;
    let session = state
        .session
        .as_mut()
        .filter(|session| session.id == session_id)
        .ok_or_else(|| MimirError::Protocol(format!("Unknown ACP session: {session_id}")))?;
    if session.active.is_some() {
        return Err(MimirError::Protocol(
            "A prompt turn is already running for this ACP session".into(),
        ));
    }
    let active = ActivePrompt {
        id: Uuid::new_v4(),
        source: RuntimeEventSource::new(),
        cancelled: Arc::new(AtomicBool::new(false)),
        autonomous: Arc::new(StdMutex::new(None)),
    };
    session.active = Some(active.clone());
    Ok((session_id.to_owned(), active, message))
}

async fn run_prompt<W>(
    runtime: Arc<AgentRuntime>,
    state: Arc<Mutex<ConnectionState>>,
    writer: JsonLineWriter<W>,
    response_id: Value,
    session_id: String,
    active: ActivePrompt,
    message: Message,
) where
    W: AsyncWrite + Send + Unpin + 'static,
{
    let sink = AcpEventSink {
        session_id: session_id.clone(),
        state: Arc::clone(&state),
        writer: writer.clone(),
    };
    let outcome = runtime
        .run_batch_messages_with_source(&[message], active.source, &sink)
        .await;
    let cancelled = active.cancelled.load(Ordering::Acquire);
    let assistant_stop_reason = if outcome.is_ok() {
        runtime
            .messages_snapshot()
            .await
            .iter()
            .rev()
            .find(|message| message.role == Role::Assistant)
            .and_then(|message| message.stop_reason)
    } else {
        None
    };
    let autonomous = active
        .autonomous
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    clear_active(&state, &session_id, active.id).await;
    let response = match (cancelled, assistant_stop_reason, outcome) {
        (true, _, _) | (_, Some(StopReason::Aborted), _) => {
            rpc_success(response_id, json!({"stopReason": "cancelled"}))
        }
        (false, Some(StopReason::Error), Ok(_)) => rpc_error(
            response_id,
            -32_603,
            "mimir turn failed: model returned an error stop reason",
        ),
        (false, _, Err(error)) => rpc_error(response_id, -32_603, &error.to_string()),
        (false, reason, Ok(_)) => rpc_success(
            response_id,
            json!({
                "stopReason": acp_stop_reason_with_autonomous(reason, autonomous.as_ref())
            }),
        ),
    };
    let _ = writer.send(&response).await;
}

async fn forward_broadcast_event<W>(
    state: &Mutex<ConnectionState>,
    writer: &JsonLineWriter<W>,
    envelope: RuntimeEventEnvelope,
) where
    W: AsyncWrite + Send + Unpin,
{
    let (session_id, duplicate, autonomous) = {
        let state = state.lock().await;
        let Some(session) = state.session.as_ref() else {
            return;
        };
        (
            session.id.clone(),
            session
                .active
                .as_ref()
                .is_some_and(|active| Some(active.source) == envelope.source),
            session
                .active
                .as_ref()
                .map(|active| Arc::clone(&active.autonomous)),
        )
    };
    if duplicate {
        return;
    }
    if let RuntimeEvent::SessionEvent { event } = &envelope.event
        && let Some(status) = autonomous_status(event)
        && let Some(active) = autonomous
    {
        *active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(status);
    }
    send_session_updates(
        writer,
        &session_id,
        acp_updates_for_runtime_event(&envelope.event),
    )
    .await;
}

async fn forward_lag_update<W>(
    state: &Mutex<ConnectionState>,
    writer: &JsonLineWriter<W>,
    count: u64,
) where
    W: AsyncWrite + Send + Unpin,
{
    let session_id = state
        .lock()
        .await
        .session
        .as_ref()
        .map(|session| session.id.clone());
    let Some(session_id) = session_id else {
        return;
    };
    send_session_updates(
        writer,
        &session_id,
        vec![json!({
            "sessionUpdate": "session_info_update",
            "_meta": mimir_meta(json!({"runtimeEventsLagged": count}))
        })],
    )
    .await;
}

async fn send_session_updates<W>(writer: &JsonLineWriter<W>, session_id: &str, updates: Vec<Value>)
where
    W: AsyncWrite + Send + Unpin,
{
    for update in updates {
        let _ = writer
            .send(&json!({
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": {"sessionId": session_id, "update": update}
            }))
            .await;
    }
}

async fn clear_active(state: &Mutex<ConnectionState>, session_id: &str, active_id: Uuid) {
    let mut state = state.lock().await;
    let Some(session) = state
        .session
        .as_mut()
        .filter(|session| session.id == session_id)
    else {
        return;
    };
    if session
        .active
        .as_ref()
        .is_some_and(|active| active.id == active_id)
    {
        session.active = None;
    }
}

async fn cancel_prompt(runtime: &AgentRuntime, state: &Mutex<ConnectionState>, params: &Value) {
    let Ok(object) = require_params_object(params) else {
        return;
    };
    let Ok(session_id) = required_string(object, "sessionId") else {
        return;
    };
    let state = state.lock().await;
    let Some(active) = state
        .session
        .as_ref()
        .filter(|session| session.id == session_id)
        .and_then(|session| session.active.as_ref())
    else {
        return;
    };
    active.cancelled.store(true, Ordering::Release);
    runtime.cancel();
}

async fn close_session(
    runtime: &AgentRuntime,
    state: &Mutex<ConnectionState>,
    params: &Value,
) -> Result<()> {
    let object = require_params_object(params)?;
    let session_id = required_string(object, "sessionId")?;
    let mut state = state.lock().await;
    let Some(session) = state.session.as_ref() else {
        return Err(MimirError::Protocol(format!(
            "Unknown ACP session: {session_id}"
        )));
    };
    if session.id != session_id {
        return Err(MimirError::Protocol(format!(
            "Unknown ACP session: {session_id}"
        )));
    }
    let closing = state
        .session
        .take()
        .ok_or_else(|| MimirError::Protocol("ACP session closed concurrently".into()))?;
    if let Some(active) = closing.active {
        active.cancelled.store(true, Ordering::Release);
        runtime.cancel();
    }
    Ok(())
}

async fn cancel_active(runtime: &AgentRuntime, state: &Mutex<ConnectionState>) {
    let mut state = state.lock().await;
    if let Some(active) = state.session.take().and_then(|session| session.active) {
        active.cancelled.store(true, Ordering::Release);
        runtime.cancel();
    }
}

fn prompt_message(blocks: &[Value]) -> Result<Message> {
    let mut text = Vec::new();
    let mut images = Vec::new();
    let mut image_bytes = 0_usize;
    for block in blocks {
        let Some(object) = block.as_object() else {
            continue;
        };
        match object.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(value) = object.get("text").and_then(Value::as_str) {
                    text.push(value.to_owned());
                }
            }
            Some("image") => {
                let data = required_string(object, "data")?;
                let mime_type = required_string(object, "mimeType")?;
                if !mime_type.starts_with("image/") || mime_type.len() > 128 {
                    return Err(MimirError::Protocol(
                        "image mimeType must be a bounded image MIME type".into(),
                    ));
                }
                let decoded = BASE64_STANDARD
                    .decode(data)
                    .map_err(|_| MimirError::Protocol("image data must be valid base64".into()))?;
                image_bytes = image_bytes.saturating_add(decoded.len());
                if image_bytes > MAX_ACP_IMAGE_BYTES {
                    return Err(MimirError::Protocol(format!(
                        "prompt images exceed {MAX_ACP_IMAGE_BYTES} bytes"
                    )));
                }
                images.push(Content::Image {
                    data: data.to_owned(),
                    mime_type: mime_type.to_owned(),
                });
            }
            Some("resource") => {
                let Some(resource) = object.get("resource").and_then(Value::as_object) else {
                    continue;
                };
                let Some(body) = resource.get("text").and_then(Value::as_str) else {
                    continue;
                };
                let uri = resource.get("uri").and_then(Value::as_str).unwrap_or("");
                text.push(if uri.is_empty() {
                    body.to_owned()
                } else {
                    format!("{uri}\n{body}")
                });
            }
            Some("resource_link") => {
                if let Some(uri) = object.get("uri").and_then(Value::as_str) {
                    text.push(uri.to_owned());
                }
            }
            _ => {}
        }
    }
    let mut content = Vec::with_capacity(usize::from(!text.is_empty()) + images.len());
    if !text.is_empty() {
        content.push(Content::Text {
            text: text.join("\n"),
        });
    }
    content.extend(images);
    if content.is_empty() {
        return Err(MimirError::Protocol(
            "prompt must contain text, an image, or embedded context".into(),
        ));
    }
    Ok(Message::user_content(content))
}

fn require_params_object(params: &Value) -> Result<&Map<String, Value>> {
    params
        .as_object()
        .ok_or_else(|| MimirError::Protocol("params must be an object".into()))
}

fn required_string<'a>(object: &'a Map<String, Value>, field: &str) -> Result<&'a str> {
    object
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| MimirError::Protocol(format!("{field} must be a non-empty string")))
}

fn same_path(left: &Path, right: &Path) -> bool {
    if canonical_path(left) == canonical_path(right) {
        return true;
    }
    same_file_identity(left, right)
}

#[cfg(unix)]
fn same_file_identity(left: &Path, right: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;

    let Ok(left) = left.metadata() else {
        return false;
    };
    let Ok(right) = right.metadata() else {
        return false;
    };
    left.dev() != 0 && left.ino() != 0 && left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(not(unix))]
fn same_file_identity(_left: &Path, _right: &Path) -> bool {
    false
}

fn canonical_path(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir().map_or_else(|_| path.to_path_buf(), |cwd| cwd.join(path))
        }
    })
}

fn display_path(path: &Path) -> String {
    canonical_path(path).to_string_lossy().into_owned()
}

fn acp_stop_reason(reason: Option<StopReason>) -> &'static str {
    match reason {
        Some(StopReason::Length | StopReason::BudgetExhausted) => "max_tokens",
        Some(StopReason::Aborted) => "cancelled",
        Some(StopReason::Error) => "refusal",
        Some(StopReason::Stop | StopReason::ToolUse) | None => "end_turn",
    }
}

fn acp_stop_reason_with_autonomous(
    reason: Option<StopReason>,
    autonomous: Option<&AcpAutonomousStatus>,
) -> &'static str {
    match reason {
        Some(StopReason::Length | StopReason::BudgetExhausted) => return "max_tokens",
        Some(StopReason::Aborted) => return "cancelled",
        Some(StopReason::Error) => return "refusal",
        Some(StopReason::Stop | StopReason::ToolUse) | None => {}
    }
    let Some(status) = autonomous.filter(|status| status.enabled) else {
        return acp_stop_reason(reason);
    };
    match status.limit.as_deref() {
        Some("maxTokens" | "max_tokens") => "max_tokens",
        Some(_) => "max_turn_requests",
        None => "end_turn",
    }
}

fn autonomous_status(event: &Value) -> Option<AcpAutonomousStatus> {
    let event_type = event.get("type")?.as_str()?;
    if !matches!(event_type, "autonomous" | "autonomous_status") {
        return None;
    }
    let status = event.get("status").unwrap_or(event);
    Some(AcpAutonomousStatus {
        enabled: status
            .get("enabled")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        limit: status
            .get("limit")
            .or_else(|| status.get("limitReason"))
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

fn rpc_success(id: Value, result: Value) -> Value {
    Value::Object(Map::from_iter([
        ("jsonrpc".into(), Value::String("2.0".into())),
        ("id".into(), id),
        ("result".into(), result),
    ]))
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    let message = truncate_utf8(message, MAX_ERROR_BYTES);
    Value::Object(Map::from_iter([
        ("jsonrpc".into(), Value::String("2.0".into())),
        ("id".into(), id),
        ("error".into(), json!({"code": code, "message": message})),
    ]))
}

fn truncate_utf8(value: &str, limit: usize) -> String {
    if value.len() <= limit {
        return value.to_owned();
    }
    let mut end = limit;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &value[..end])
}

fn mimir_meta(payload: Value) -> Value {
    Value::Object(Map::from_iter([(MIMIR_META_NAMESPACE.into(), payload)]))
}

struct AcpEventSink<W> {
    session_id: String,
    state: Arc<Mutex<ConnectionState>>,
    writer: JsonLineWriter<W>,
}

#[async_trait]
impl<W> EventSink for AcpEventSink<W>
where
    W: AsyncWrite + Send + Unpin,
{
    async fn emit(&self, event: RuntimeEvent) {
        let active_session = self
            .state
            .lock()
            .await
            .session
            .as_ref()
            .is_some_and(|session| session.id == self.session_id);
        if !active_session {
            return;
        }
        send_session_updates(
            &self.writer,
            &self.session_id,
            acp_updates_for_runtime_event(&event),
        )
        .await;
    }
}

/// Maps one Rust runtime event to standard ACP session updates.
///
/// Mimir concepts without a standard ACP representation are carried only
/// under the reverse-domain `_meta` namespace.
pub fn acp_updates_for_runtime_event(event: &RuntimeEvent) -> Vec<Value> {
    match event {
        RuntimeEvent::TextDelta { text } if !text.is_empty() => vec![json!({
            "sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": text}
        })],
        RuntimeEvent::MessageCompleted { message } if message.role == Role::Assistant => message
            .content
            .iter()
            .filter_map(|content| match content {
                Content::Thinking {
                    text,
                    redacted: false,
                    ..
                } if !text.is_empty() => Some(json!({
                    "sessionUpdate": "agent_thought_chunk",
                    "content": {"type": "text", "text": text}
                })),
                _ => None,
            })
            .collect(),
        RuntimeEvent::ToolStarted {
            id,
            name,
            arguments,
        } => vec![json!({
            "sessionUpdate": "tool_call",
            "toolCallId": id,
            "title": tool_title(name),
            "kind": acp_tool_kind(name),
            "status": "in_progress",
            "rawInput": tool_raw_input(name, arguments)
        })],
        RuntimeEvent::ToolFinished {
            id,
            name,
            observation,
        } => vec![tool_finished_update(id, name, observation)],
        RuntimeEvent::SessionEvent { event } => acp_updates_for_session_event(event),
        _ => Vec::new(),
    }
}

fn acp_updates_for_session_event(event: &Value) -> Vec<Value> {
    let Some(event_type) = event.get("type").and_then(Value::as_str) else {
        return Vec::new();
    };
    let metadata = match event_type {
        "heartbeats_changed" => json!({"heartbeatsChanged": true}),
        "cron_changed" | "schedules_changed" => json!({"schedulesChanged": true}),
        "compaction_end" => {
            let result = event.get("result").cloned().unwrap_or(Value::Null);
            json!({"compaction": {
                "tokensBefore": result.get("tokensBefore"),
                "summary": result.get("summary")
            }})
        }
        "rlm_child_update" | "subagent_update" => {
            let child = event.get("child").cloned().unwrap_or(Value::Null);
            json!({"subagents": [{
                "id": child.get("id"),
                "sessionName": child.get("sessionName"),
                "status": child.get("status"),
                "model": child.get("model"),
                "tokenCount": child.get("tokenCount"),
                "error": child.get("error")
            }]})
        }
        "goal_update" => json!({"goal": event.get("goal").cloned().unwrap_or(Value::Null)}),
        "refine_complete" => {
            let result = event.get("result").cloned().unwrap_or(Value::Null);
            json!({"refinement": {
                "status": "complete",
                "summary": result.get("summary"),
                "changes": applied_refinement_changes(&result)
            }})
        }
        "refine_failed" => json!({"refinement": {
            "status": "failed",
            "error": event.get("error")
        }}),
        "ipython_sent_agent_message" | "inbound_agent_message" => json!({
            "agentMessage": event.get("message").cloned().unwrap_or(Value::Null)
        }),
        "autonomous" | "autonomous_status" => {
            let status = event.get("status").unwrap_or(event);
            json!({"autonomous": status})
        }
        _ => return Vec::new(),
    };
    vec![json!({
        "sessionUpdate": "session_info_update",
        "_meta": mimir_meta(metadata)
    })]
}

fn applied_refinement_changes(result: &Value) -> Vec<String> {
    result
        .get("appliedEdits")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|edit| {
            edit.get("applied")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        })
        .map(|edit| {
            format!(
                "{} {}:{}",
                edit.get("action")
                    .and_then(Value::as_str)
                    .unwrap_or("update"),
                edit.get("kind").and_then(Value::as_str).unwrap_or("item"),
                edit.get("id").and_then(Value::as_str).unwrap_or("unknown")
            )
        })
        .collect()
}

fn acp_tool_kind(name: &str) -> &'static str {
    match name {
        "read" | "read_file" => "read",
        "edit" | "edit_file" | "write" | "write_file" => "edit",
        "delete" | "delete_file" => "delete",
        "move" | "move_file" => "move",
        "search" | "list_files" => "search",
        "ipython" | "bash" | "run_process" | "execute" => "execute",
        "spawn_agent" | "rlm_spawn" | "rlm_wait" => "think",
        "fetch" => "fetch",
        _ => "other",
    }
}

fn tool_title(name: &str) -> &str {
    if name == "ipython" {
        "IPython cell"
    } else {
        name
    }
}

fn tool_raw_input(name: &str, arguments: &Value) -> Value {
    if name == "ipython"
        && let Some(code) = arguments.get("code").and_then(Value::as_str)
    {
        return json!({"code": code});
    }
    arguments.clone()
}

fn tool_finished_update(id: &str, name: &str, observation: &ToolObservation) -> Value {
    let mut update = Map::from_iter([
        ("sessionUpdate".into(), json!("tool_call_update")),
        ("toolCallId".into(), json!(id)),
        (
            "status".into(),
            json!(if observation.status == ObservationStatus::Error {
                "failed"
            } else {
                "completed"
            }),
        ),
    ]);
    let text = tool_result_text(observation);
    if !text.is_empty() {
        update.insert(
            "content".into(),
            json!([{"type": "content", "content": {"type": "text", "text": text}}]),
        );
    }
    if name == "ipython"
        && let Some(meta) = ipython_rich_output(observation)
    {
        update.insert("_meta".into(), mimir_meta(json!({"ipython": meta})));
    }
    Value::Object(update)
}

fn tool_result_text(observation: &ToolObservation) -> String {
    let parsed = serde_json::from_str::<Value>(&observation.content).ok();
    parsed
        .as_ref()
        .and_then(|value| value.get("output"))
        .and_then(Value::as_str)
        .map_or_else(|| observation.content.clone(), str::to_owned)
}

fn ipython_rich_output(observation: &ToolObservation) -> Option<Value> {
    let mut meta = Map::new();
    let parsed = serde_json::from_str::<Value>(&observation.content).ok();
    let details = parsed
        .as_ref()
        .and_then(|value| value.get("details"))
        .and_then(Value::as_object);
    let mut attachments = details
        .and_then(|details| details.get("attachments"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_object)
        .map(|attachment| {
            let mut value = Map::new();
            if let Some(mime_type) = attachment.get("mimeType").and_then(Value::as_str) {
                value.insert("mimeType".into(), json!(mime_type));
            }
            if let Some(path) = attachment.get("path").and_then(Value::as_str) {
                value.insert("path".into(), json!(path));
            }
            if let Some(data) = attachment.get("data").and_then(Value::as_str) {
                value.insert("bytes".into(), json!(base64_decoded_len(data)));
            }
            Value::Object(value)
        })
        .collect::<Vec<_>>();
    attachments.extend(observation.artifacts.iter().map(|path| {
        let mime_type = match path.extension().and_then(|extension| extension.to_str()) {
            Some("png") => Some("image/png"),
            Some("jpg" | "jpeg") => Some("image/jpeg"),
            _ => None,
        };
        let bytes = std::fs::metadata(path).ok().map(|metadata| metadata.len());
        json!({
            "path": path,
            "mimeType": mime_type,
            "bytes": bytes
        })
    }));
    if !attachments.is_empty() {
        meta.insert("attachments".into(), Value::Array(attachments));
    }
    if let Some(diffs) = details
        .and_then(|details| details.get("diffs"))
        .and_then(Value::as_array)
        && !diffs.is_empty()
    {
        meta.insert("diffCount".into(), json!(diffs.len()));
    }
    (!meta.is_empty()).then_some(Value::Object(meta))
}

fn base64_decoded_len(data: &str) -> usize {
    let padding = if data.ends_with("==") {
        2
    } else {
        usize::from(data.ends_with('='))
    };
    (data.len().saturating_mul(3) / 4).saturating_sub(padding)
}
