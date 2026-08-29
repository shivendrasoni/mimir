//! Session-scoped implementations for public daemon runtime operations.
//!
//! The service owns only transient operation bookkeeping. Durable conversation,
//! RLM, refinement, and preference state remain owned by their existing audited
//! Rust services.

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
};

use serde_json::{Value, json};
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{
    model::{Content, Message, Role},
    refinement::{self, RefineOptions},
    runtime::AgentRuntime,
    tools::BashRunner,
    tui::{
        SideQuestionSession, SideQuestionTurn, ask_side_question_cancellable,
        load_tui_rlm_max_depth_status, set_tui_rlm_max_depth,
    },
};

use super::{DaemonError, PublicDaemonCommand};

const MAX_COMMAND_BYTES: usize = 32 * 1024;
const MAX_OPERATION_ID_BYTES: usize = 128;
const MAX_ACTIVE_SIDE_QUESTIONS: usize = 128;
const MAX_BRANCH_SUMMARY_TRANSCRIPT_BYTES: usize = 192 * 1024;
const MAX_BRANCH_SUMMARY_MESSAGE_BYTES: usize = 32 * 1024;
const BRANCH_SUMMARY_OUTPUT_TOKENS: u32 = 8_000;
const BRANCH_SUMMARY_SYSTEM_PROMPT: &str = "You summarize an abandoned conversation branch for another coding agent. Preserve decisions, constraints, completed work, exact identifiers, file paths, failures, and remaining actions. Be concise, factual, and do not invent details.";

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct SideQuestionKey {
    client: String,
    session: String,
    question: String,
}

struct SideQuestionRun {
    cancellation: CancellationToken,
}

/// Bounded process-local coordinator for the public daemon's non-turn runtime
/// operations.
pub struct RuntimeOperations {
    state_root: PathBuf,
    side_questions: Mutex<HashMap<SideQuestionKey, SideQuestionRun>>,
    branch_summaries: Mutex<HashMap<String, CancellationToken>>,
    bash_sessions: Mutex<HashSet<String>>,
}

impl RuntimeOperations {
    #[must_use]
    pub fn new(state_root: PathBuf) -> Self {
        Self {
            state_root,
            side_questions: Mutex::new(HashMap::new()),
            branch_summaries: Mutex::new(HashMap::new()),
            bash_sessions: Mutex::new(HashSet::new()),
        }
    }

    /// Handles one recognized runtime operation, returning `None` for commands
    /// owned by another dispatcher tranche.
    ///
    /// # Errors
    ///
    /// Returns a protocol error for invalid fields, conflicting operation state,
    /// denied process execution, or a typed underlying runtime failure.
    #[cfg(test)]
    pub async fn handle(
        self: &Arc<Self>,
        session_id: &str,
        command: &PublicDaemonCommand,
        runtime: Arc<AgentRuntime>,
        bash_runner: Option<Arc<BashRunner>>,
    ) -> Result<Option<Value>, DaemonError> {
        self.handle_for_client("local", session_id, command, runtime, bash_runner)
            .await
    }

    /// Client-aware form used by the public daemon dispatcher. Transient side
    /// questions can only be aborted by the client/session pair that started
    /// them.
    pub async fn handle_for_client(
        self: &Arc<Self>,
        client_id: &str,
        session_id: &str,
        command: &PublicDaemonCommand,
        runtime: Arc<AgentRuntime>,
        bash_runner: Option<Arc<BashRunner>>,
    ) -> Result<Option<Value>, DaemonError> {
        validate_operation_id("client_id", client_id)?;
        let result = match command.command_type() {
            "execute_bash" => {
                self.start_bash(session_id, command, runtime, bash_runner)
                    .await?;
                Value::Null
            }
            "start_side_question" => {
                self.start_side_question(client_id, session_id, command, runtime)
                    .await?;
                Value::Null
            }
            "abort_side_question" => {
                self.abort_side_question(client_id, session_id, command)
                    .await?
            }
            "cancel_rlm_child" => {
                let target = required_bounded_string(command, "childId")?;
                json!({"cancelled": runtime.cancel_rlm_child(target).await.map_err(protocol)?})
            }
            "delete_rlm_subagent" => {
                let target = required_bounded_string(command, "childId")?;
                let deleted = runtime.delete_rlm_child(target).await.map_err(protocol)?;
                json!({"deleted": deleted.is_some()})
            }
            "get_rlm_max_depth_status" => {
                let status = load_tui_rlm_max_depth_status(&self.state_root, session_id)
                    .await
                    .map_err(protocol)?;
                rlm_depth_status_value(&status, None)
            }
            "set_rlm_max_depth" => {
                let depth = required_u32(command, "maxDepth")?;
                let global = optional_bool(command, "global")?.unwrap_or(false);
                set_tui_rlm_max_depth(&self.state_root, session_id, depth, global)
                    .await
                    .map_err(protocol)?;
                let status = load_tui_rlm_max_depth_status(&self.state_root, session_id)
                    .await
                    .map_err(protocol)?;
                rlm_depth_status_value(&status, Some(global))
            }
            "refine" => self.refine(session_id, command, runtime).await?,
            "abort_compaction" => {
                runtime.abort_control_operation();
                Value::Null
            }
            "abort_branch_summary" => {
                json!({"aborted": self.abort_branch_summary(session_id).await})
            }
            _ => return Ok(None),
        };
        Ok(Some(result))
    }

    async fn start_bash(
        self: &Arc<Self>,
        session_id: &str,
        command: &PublicDaemonCommand,
        runtime: Arc<AgentRuntime>,
        bash_runner: Option<Arc<BashRunner>>,
    ) -> Result<(), DaemonError> {
        let shell_command = required_bounded_string(command, "command")?.to_owned();
        let runner = bash_runner.ok_or_else(|| {
            DaemonError::Protocol("execute_bash requires an enabled session process policy".into())
        })?;
        let mut sessions = self.bash_sessions.lock().await;
        if runner.is_running() || !sessions.insert(session_id.to_owned()) {
            return Err(DaemonError::Protocol(
                "a bash command is already running for this session".into(),
            ));
        }
        drop(sessions);
        let exclude = optional_bool(command, "excludeFromContext")?.unwrap_or(false);
        let transient = optional_bool(command, "transient")?.unwrap_or(false);
        let run_id = optional_bounded_string(command, "runId")?
            .map_or_else(|| Uuid::new_v4().to_string(), str::to_owned);
        if let Err(error) = runtime.publish_transient_session_event(json!({
            "type": "bash_start",
            "runId": run_id,
            "command": shell_command,
            "excludeFromContext": exclude,
            "transient": transient,
        })) {
            self.bash_sessions.lock().await.remove(session_id);
            return Err(protocol(error));
        }
        let operations = Arc::clone(self);
        let owned_session_id = session_id.to_owned();
        tokio::spawn(async move {
            let (output_tx, mut output_rx) = mpsc::channel(8);
            let output_runtime = Arc::clone(&runtime);
            let output_publisher = tokio::spawn(async move {
                while let Some(chunk) = output_rx.recv().await {
                    let _ = output_runtime.publish_transient_session_event(json!({
                        "type": "bash_output",
                        "chunk": chunk,
                    }));
                }
            });
            let execution = runner.execute_streaming(&shell_command, output_tx).await;
            let _ = output_publisher.await;
            match execution {
                Ok(result) => {
                    if !exclude && !transient {
                        let _ = runtime.record_bash_execution(&shell_command, &result).await;
                    }
                    let _ = runtime.publish_transient_session_event(json!({
                        "type": "bash_end",
                        "runId": run_id,
                        "output": result.output,
                        "exitCode": result.exit_code,
                        "cancelled": result.cancelled,
                        "truncated": result.truncated,
                        "fullOutputPath": result.full_output_path,
                    }));
                }
                Err(_) => {
                    let _ = runtime.publish_transient_session_event(json!({
                        "type": "bash_end",
                        "runId": run_id,
                        "cancelled": false,
                        "truncated": false,
                        "errorMessage": "bash execution failed",
                    }));
                }
            }
            operations
                .bash_sessions
                .lock()
                .await
                .remove(&owned_session_id);
        });
        Ok(())
    }

    async fn start_side_question(
        self: &Arc<Self>,
        client_id: &str,
        session_id: &str,
        command: &PublicDaemonCommand,
        runtime: Arc<AgentRuntime>,
    ) -> Result<(), DaemonError> {
        let id = required_bounded_string(command, "sideQuestionId")?.to_owned();
        let key = SideQuestionKey {
            client: client_id.into(),
            session: session_id.into(),
            question: id.clone(),
        };
        let question = required_bounded_string(command, "question")?.to_owned();
        let previous_turns = command
            .field("previousTurns")
            .cloned()
            .map(serde_json::from_value::<Vec<SideQuestionTurn>>)
            .transpose()?
            .unwrap_or_default();
        let mut side_session = SideQuestionSession::from_turns(previous_turns).map_err(protocol)?;
        let cancellation = {
            let mut runs = self.side_questions.lock().await;
            if runs.contains_key(&key) {
                return Err(DaemonError::Protocol(format!(
                    "side question already exists: {id}"
                )));
            }
            if runs
                .keys()
                .any(|candidate| candidate.client == client_id && candidate.session == session_id)
            {
                return Err(DaemonError::Protocol(
                    "a side question is already running for this client and session".into(),
                ));
            }
            if runs.len() >= MAX_ACTIVE_SIDE_QUESTIONS {
                return Err(DaemonError::Protocol(
                    "active side-question limit reached".into(),
                ));
            }
            let cancellation = CancellationToken::new();
            runs.insert(
                key.clone(),
                SideQuestionRun {
                    cancellation: cancellation.clone(),
                },
            );
            cancellation
        };
        if let Err(error) =
            runtime.publish_transient_session_event(side_question_event(&id, "running", None))
        {
            self.side_questions.lock().await.remove(&key);
            return Err(protocol(error));
        }
        let operations = Arc::clone(self);
        tokio::spawn(async move {
            let answer = ask_side_question_cancellable(
                runtime.as_ref(),
                &mut side_session,
                &question,
                &cancellation,
            )
            .await;
            let status = if cancellation.is_cancelled() {
                "cancelled"
            } else if answer.is_ok() {
                "completed"
            } else {
                "failed"
            };
            let payload = answer.ok();
            let _ = runtime.publish_transient_session_event(side_question_event(
                &id,
                status,
                payload.as_deref(),
            ));
            operations.side_questions.lock().await.remove(&key);
        });
        Ok(())
    }

    async fn abort_side_question(
        &self,
        client_id: &str,
        session_id: &str,
        command: &PublicDaemonCommand,
    ) -> Result<Value, DaemonError> {
        let id = required_bounded_string(command, "sideQuestionId")?;
        let key = SideQuestionKey {
            client: client_id.into(),
            session: session_id.into(),
            question: id.into(),
        };
        let runs = self.side_questions.lock().await;
        let Some(run) = runs.get(&key) else {
            return Ok(json!({"aborted": false}));
        };
        run.cancellation.cancel();
        Ok(json!({"aborted": true}))
    }

    async fn refine(
        &self,
        session_id: &str,
        command: &PublicDaemonCommand,
        runtime: Arc<AgentRuntime>,
    ) -> Result<Value, DaemonError> {
        let instructions = optional_bounded_string(command, "instructions")?;
        let rollback_id = optional_bounded_string(command, "rollbackId")?;
        let global = optional_bool(command, "global")?.unwrap_or(false);
        let result = refinement::refine(
            runtime.as_ref(),
            &self.state_root,
            session_id,
            RefineOptions {
                instructions,
                rollback_id,
                global,
            },
        )
        .await
        .map_err(protocol)?;
        let value = serde_json::to_value(&result)?;
        runtime
            .record_runtime_event("refinement", &value.to_string())
            .await
            .map_err(protocol)?;
        runtime
            .set_harness_context(
                refinement::load_harness_context(&self.state_root, session_id)
                    .await
                    .map_err(protocol)?,
            )
            .await;
        Ok(value)
    }

    /// Generates a bounded summary for the active branch without mutating the
    /// conversation. One summary may run per session, and
    /// [`Self::abort_branch_summary`] cancels only that operation.
    pub async fn summarize_branch(
        &self,
        session_id: &str,
        command: &PublicDaemonCommand,
        runtime: Arc<AgentRuntime>,
        messages: &[Message],
    ) -> Result<String, DaemonError> {
        validate_operation_id("session_id", session_id)?;
        if messages.is_empty() {
            return Err(DaemonError::Protocol(
                "branch summary requires at least one abandoned message".into(),
            ));
        }
        let custom_instructions = optional_bounded_string(command, "customInstructions")?;
        let replace_instructions = optional_bool(command, "replaceInstructions")?.unwrap_or(false);
        if replace_instructions && custom_instructions.is_none() {
            return Err(DaemonError::Protocol(
                "replaceInstructions requires customInstructions".into(),
            ));
        }
        let prompt =
            build_branch_summary_prompt(messages, custom_instructions, replace_instructions);
        let cancellation = {
            let mut summaries = self.branch_summaries.lock().await;
            if summaries.contains_key(session_id) {
                return Err(DaemonError::Protocol(
                    "a branch summary is already running for this session".into(),
                ));
            }
            let cancellation = CancellationToken::new();
            summaries.insert(session_id.into(), cancellation.clone());
            cancellation
        };

        let completion = runtime.complete_control_request(
            BRANCH_SUMMARY_SYSTEM_PROMPT,
            &prompt,
            BRANCH_SUMMARY_OUTPUT_TOKENS,
        );
        tokio::pin!(completion);
        let result = tokio::select! {
            result = &mut completion => result.map_err(protocol),
            () = cancellation.cancelled() => {
                Err(DaemonError::Protocol("branch summary cancelled".into()))
            }
        };

        let mut summaries = self.branch_summaries.lock().await;
        if summaries
            .get(session_id)
            .is_some_and(|active| active == &cancellation)
        {
            summaries.remove(session_id);
        }
        result
    }

    /// Cancels only the branch-summary service owned by `session_id`.
    pub async fn abort_branch_summary(&self, session_id: &str) -> bool {
        let Some(cancellation) = self.branch_summaries.lock().await.get(session_id).cloned() else {
            return false;
        };
        cancellation.cancel();
        true
    }
}

fn build_branch_summary_prompt(
    messages: &[Message],
    custom_instructions: Option<&str>,
    replace_instructions: bool,
) -> String {
    let instructions = if replace_instructions {
        custom_instructions.unwrap_or_default().to_owned()
    } else {
        let mut instructions = String::from(
            "Summarize the branch transcript below so work can continue from a different branch.",
        );
        if let Some(custom) = custom_instructions {
            instructions.push_str("\n\nAdditional instructions:\n");
            instructions.push_str(custom);
        }
        instructions
    };
    let transcript = bounded_branch_transcript(messages);
    format!("{instructions}\n\nBranch transcript:\n{transcript}")
}

fn bounded_branch_transcript(messages: &[Message]) -> String {
    let mut retained = Vec::new();
    let mut used = 0_usize;
    for message in messages.iter().rev() {
        let rendered = render_branch_message(message);
        let remaining = MAX_BRANCH_SUMMARY_TRANSCRIPT_BYTES.saturating_sub(used);
        if remaining == 0 {
            break;
        }
        let rendered = if rendered.len() > remaining {
            bounded_suffix(&rendered, remaining)
        } else {
            rendered
        };
        used = used.saturating_add(rendered.len());
        retained.push(rendered);
    }
    retained.reverse();
    retained.join("\n\n")
}

fn render_branch_message(message: &Message) -> String {
    let role = match message.role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    };
    let mut rendered = format!("[{role}]");
    for content in &message.content {
        let addition = match content {
            Content::Text { text } => format!("\n{text}"),
            Content::Image { mime_type, .. } => format!("\n[image: {mime_type}]"),
            Content::Thinking { .. } => String::new(),
            Content::ToolCall(call) => {
                format!("\n[tool call: {} {}]", call.name, call.arguments)
            }
            Content::ToolResult(result) => format!(
                "\n[tool result: {}{}]\n{}",
                result.tool_name,
                if result.is_error { " (error)" } else { "" },
                result.content
            ),
        };
        let remaining = MAX_BRANCH_SUMMARY_MESSAGE_BYTES.saturating_sub(rendered.len());
        if remaining == 0 {
            break;
        }
        rendered.push_str(&bounded_prefix(&addition, remaining));
    }
    rendered
}

fn bounded_prefix(value: &str, limit: usize) -> String {
    if value.len() <= limit {
        return value.into();
    }
    let end = value
        .char_indices()
        .map(|(index, _)| index)
        .take_while(|index| *index <= limit)
        .last()
        .unwrap_or(0);
    value[..end].into()
}

fn bounded_suffix(value: &str, limit: usize) -> String {
    if value.len() <= limit {
        return value.into();
    }
    let start = value.len().saturating_sub(limit);
    let start = value
        .char_indices()
        .map(|(index, _)| index)
        .find(|index| *index >= start)
        .unwrap_or(value.len());
    value[start..].into()
}

fn rlm_depth_status_value(status: &crate::tui::RlmMaxDepthStatus, global: Option<bool>) -> Value {
    let mut value = json!({
        "maxDepth": status.effective_max_depth,
        "effectiveMaxDepth": status.effective_max_depth,
        "globalMaxDepth": status.global_max_depth,
        "sessionMaxDepth": status.session_max_depth,
        "source": status.source,
    });
    if let Some(global) = global {
        value["global"] = Value::Bool(global);
        value["requiresReload"] = Value::Bool(true);
    }
    value
}

fn side_question_event(id: &str, status: &str, answer: Option<&str>) -> Value {
    json!({
        "type": "side_question_event",
        "event": {
            "id": id,
            "status": status,
            "answer": answer,
        }
    })
}

fn required_bounded_string<'a>(
    command: &'a PublicDaemonCommand,
    field: &str,
) -> Result<&'a str, DaemonError> {
    optional_bounded_string(command, field)?
        .ok_or_else(|| DaemonError::Protocol(format!("{field} must be a non-empty bounded string")))
}

fn validate_operation_id(field: &str, value: &str) -> Result<(), DaemonError> {
    if value.is_empty() || value.trim() != value {
        return Err(DaemonError::Protocol(format!(
            "{field} must be a non-empty string without surrounding whitespace"
        )));
    }
    if value.len() > MAX_OPERATION_ID_BYTES {
        return Err(DaemonError::Protocol(format!(
            "{field} exceeds the {MAX_OPERATION_ID_BYTES}-byte limit"
        )));
    }
    Ok(())
}

fn optional_bounded_string<'a>(
    command: &'a PublicDaemonCommand,
    field: &str,
) -> Result<Option<&'a str>, DaemonError> {
    let Some(value) = command.field(field) else {
        return Ok(None);
    };
    let value = value
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| DaemonError::Protocol(format!("{field} must be a non-empty string")))?;
    let limit = if matches!(field, "command" | "question" | "instructions") {
        MAX_COMMAND_BYTES
    } else {
        MAX_OPERATION_ID_BYTES
    };
    if value.len() > limit {
        return Err(DaemonError::Protocol(format!(
            "{field} exceeds the {limit}-byte limit"
        )));
    }
    Ok(Some(value))
}

fn optional_bool(command: &PublicDaemonCommand, field: &str) -> Result<Option<bool>, DaemonError> {
    command
        .field(field)
        .map(|value| {
            value
                .as_bool()
                .ok_or_else(|| DaemonError::Protocol(format!("{field} must be a boolean")))
        })
        .transpose()
}

fn required_u32(command: &PublicDaemonCommand, field: &str) -> Result<u32, DaemonError> {
    let value = command
        .field(field)
        .and_then(Value::as_u64)
        .ok_or_else(|| DaemonError::Protocol(format!("{field} must be a non-negative integer")))?;
    u32::try_from(value)
        .map_err(|_| DaemonError::Protocol(format!("{field} exceeds the supported range")))
}

fn protocol(error: impl std::fmt::Display) -> DaemonError {
    DaemonError::Protocol(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use crate::{
        daemon::PublicDaemonCommand,
        model::{Content, Message, ModelResponse, StopReason},
        provider::FakeProvider,
        runtime::{RuntimeConfig, RuntimeEvent},
        session::InMemorySessionStore,
        tools::{ToolPolicy, ToolRegistry},
        tui::load_tui_rlm_max_depth,
    };

    async fn fake_runtime(workspace: &std::path::Path, answer: &str) -> Arc<AgentRuntime> {
        fake_runtime_with_delay(workspace, answer, Duration::ZERO).await
    }

    async fn fake_runtime_with_delay(
        workspace: &std::path::Path,
        answer: &str,
        delay: Duration,
    ) -> Arc<AgentRuntime> {
        let provider = Arc::new(
            FakeProvider::new(vec![ModelResponse {
                message: Message::assistant(
                    vec![Content::Text {
                        text: answer.into(),
                    }],
                    StopReason::Stop,
                ),
                response_id: None,
            }])
            .with_delay(delay),
        );
        Arc::new(
            AgentRuntime::resume(
                provider,
                Arc::new(
                    ToolRegistry::with_default_tools(workspace, ToolPolicy::default())
                        .expect("tools"),
                ),
                Arc::new(InMemorySessionStore::default()),
                RuntimeConfig::default_for_model("fake"),
            )
            .await
            .expect("runtime"),
        )
    }

    async fn next_session_event(
        events: &mut tokio::sync::broadcast::Receiver<crate::runtime_events::RuntimeEventEnvelope>,
        expected_type: &str,
    ) -> Value {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let RuntimeEvent::SessionEvent { event } =
                    events.recv().await.expect("runtime event").event
                    && event.get("type").and_then(Value::as_str) == Some(expected_type)
                {
                    return event;
                }
            }
        })
        .await
        .expect("event timeout")
    }

    #[test]
    fn bounded_fields_and_types_fail_closed() {
        let blank = PublicDaemonCommand::new(
            "start_side_question",
            [("question".into(), Value::String(" ".into()))],
        )
        .expect("command");
        assert!(required_bounded_string(&blank, "question").is_err());

        let invalid = PublicDaemonCommand::new(
            "set_rlm_max_depth",
            [("maxDepth".into(), Value::String("3".into()))],
        )
        .expect("command");
        assert!(required_u32(&invalid, "maxDepth").is_err());
    }

    #[tokio::test]
    async fn rlm_depth_settings_are_session_scoped_and_bounded() {
        let state = tempfile::TempDir::new().expect("state");
        set_tui_rlm_max_depth(state.path(), "alpha", 5, false)
            .await
            .expect("set session depth");
        assert_eq!(
            load_tui_rlm_max_depth(state.path(), "alpha")
                .await
                .expect("alpha depth"),
            5
        );
        assert_ne!(
            load_tui_rlm_max_depth(state.path(), "beta")
                .await
                .expect("beta depth"),
            5
        );
        assert!(
            set_tui_rlm_max_depth(state.path(), "alpha", 33, false)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn side_question_is_transient_and_emits_completion() {
        let workspace = tempfile::TempDir::new().expect("workspace");
        let state = tempfile::TempDir::new().expect("state");
        let runtime = fake_runtime(workspace.path(), "isolated answer").await;
        let mut events = runtime.subscribe_events();
        let operations = Arc::new(RuntimeOperations::new(state.path().to_owned()));
        let command = PublicDaemonCommand::new(
            "start_side_question",
            [
                ("sideQuestionId".into(), Value::String("question-1".into())),
                ("question".into(), Value::String("What changed?".into())),
            ],
        )
        .expect("command");
        operations
            .handle("alpha", &command, Arc::clone(&runtime), None)
            .await
            .expect("start side question");
        let event = next_session_event(&mut events, "side_question_event").await;
        let event = if event["event"]["status"] == "running" {
            next_session_event(&mut events, "side_question_event").await
        } else {
            event
        };
        assert_eq!(event["event"]["status"], "completed");
        assert_eq!(event["event"]["answer"], "isolated answer");
        assert!(runtime.messages_snapshot().await.is_empty());
    }

    #[tokio::test]
    async fn abort_side_question_is_scoped_and_cancels_before_provider_completion() {
        let workspace = tempfile::TempDir::new().expect("workspace");
        let state = tempfile::TempDir::new().expect("state");
        let runtime =
            fake_runtime_with_delay(workspace.path(), "too late", Duration::from_secs(30)).await;
        let mut events = runtime.subscribe_events();
        let operations = Arc::new(RuntimeOperations::new(state.path().to_owned()));
        let start = PublicDaemonCommand::new(
            "start_side_question",
            [
                ("sideQuestionId".into(), Value::String("question-1".into())),
                ("question".into(), Value::String("What changed?".into())),
            ],
        )
        .expect("start command");
        operations
            .handle("alpha", &start, Arc::clone(&runtime), None)
            .await
            .expect("start side question");
        let running = next_session_event(&mut events, "side_question_event").await;
        assert_eq!(running["event"]["status"], "running");

        let abort = PublicDaemonCommand::new(
            "abort_side_question",
            [("sideQuestionId".into(), Value::String("question-1".into()))],
        )
        .expect("abort command");
        let response = operations
            .handle("alpha", &abort, Arc::clone(&runtime), None)
            .await
            .expect("abort side question")
            .expect("handled");
        assert_eq!(response["aborted"], true);
        let cancelled = next_session_event(&mut events, "side_question_event").await;
        assert_eq!(cancelled["event"]["status"], "cancelled");
    }

    #[tokio::test]
    async fn side_question_abort_requires_owning_client_and_session() {
        let workspace = tempfile::TempDir::new().expect("workspace");
        let state = tempfile::TempDir::new().expect("state");
        let runtime =
            fake_runtime_with_delay(workspace.path(), "too late", Duration::from_secs(30)).await;
        let mut events = runtime.subscribe_events();
        let operations = Arc::new(RuntimeOperations::new(state.path().to_owned()));
        let start = PublicDaemonCommand::new(
            "start_side_question",
            [
                ("sideQuestionId".into(), Value::String("question-1".into())),
                ("question".into(), Value::String("What changed?".into())),
            ],
        )
        .expect("start command");
        operations
            .handle_for_client("client-a", "alpha", &start, Arc::clone(&runtime), None)
            .await
            .expect("start side question");
        let running = next_session_event(&mut events, "side_question_event").await;
        assert_eq!(running["event"]["status"], "running");

        let abort = PublicDaemonCommand::new(
            "abort_side_question",
            [("sideQuestionId".into(), Value::String("question-1".into()))],
        )
        .expect("abort command");
        let foreign_client = operations
            .handle_for_client("client-b", "alpha", &abort, Arc::clone(&runtime), None)
            .await
            .expect("foreign abort")
            .expect("handled");
        assert_eq!(foreign_client["aborted"], false);
        let foreign_session = operations
            .handle_for_client("client-a", "beta", &abort, Arc::clone(&runtime), None)
            .await
            .expect("foreign session abort")
            .expect("handled");
        assert_eq!(foreign_session["aborted"], false);
        let owner = operations
            .handle_for_client("client-a", "alpha", &abort, Arc::clone(&runtime), None)
            .await
            .expect("owner abort")
            .expect("handled");
        assert_eq!(owner["aborted"], true);
        let cancelled = next_session_event(&mut events, "side_question_event").await;
        assert_eq!(cancelled["event"]["status"], "cancelled");
    }

    #[tokio::test]
    async fn asynchronous_bash_uses_policy_and_transient_mode_avoids_context() {
        let workspace = tempfile::TempDir::new().expect("workspace");
        let state = tempfile::TempDir::new().expect("state");
        let runtime = fake_runtime(workspace.path(), "unused").await;
        let mut events = runtime.subscribe_events();
        let runner = Arc::new(
            BashRunner::new(
                workspace.path(),
                ToolPolicy {
                    allow_process: true,
                    allowed_programs: Some(vec!["printf".into()]),
                    command_timeout: Duration::from_secs(2),
                    ..ToolPolicy::default()
                },
            )
            .expect("bash runner"),
        );
        let operations = Arc::new(RuntimeOperations::new(state.path().to_owned()));
        let command = PublicDaemonCommand::new(
            "execute_bash",
            [
                ("command".into(), Value::String("printf daemon-ok".into())),
                ("transient".into(), Value::Bool(true)),
                ("runId".into(), Value::String("bash-1".into())),
            ],
        )
        .expect("command");
        operations
            .handle("alpha", &command, Arc::clone(&runtime), Some(runner))
            .await
            .expect("start bash");
        let output = next_session_event(&mut events, "bash_output").await;
        assert_eq!(output["chunk"], "daemon-ok");
        let event = next_session_event(&mut events, "bash_end").await;
        assert_eq!(event["runId"], "bash-1");
        assert_eq!(event["output"], "daemon-ok");
        assert!(runtime.messages_snapshot().await.is_empty());
    }

    #[tokio::test]
    async fn branch_summary_abort_is_distinct_and_truthful_when_idle() {
        let state = tempfile::TempDir::new().expect("state");
        let operations = Arc::new(RuntimeOperations::new(state.path().to_owned()));
        assert!(!operations.abort_branch_summary("alpha").await);
        assert!(!operations.abort_branch_summary("beta").await);
    }

    #[tokio::test]
    async fn branch_summary_uses_isolated_bounded_control_completion() {
        let workspace = tempfile::TempDir::new().expect("workspace");
        let state = tempfile::TempDir::new().expect("state");
        let runtime = fake_runtime(workspace.path(), "durable branch summary").await;
        let operations = Arc::new(RuntimeOperations::new(state.path().to_owned()));
        let command = PublicDaemonCommand::new(
            "navigate_tree",
            [
                (
                    "customInstructions".into(),
                    Value::String("Focus on files".into()),
                ),
                ("replaceInstructions".into(), Value::Bool(false)),
            ],
        )
        .expect("navigate command");
        let abandoned_messages = vec![Message::user("abandoned branch only")];
        let summary = operations
            .summarize_branch("alpha", &command, Arc::clone(&runtime), &abandoned_messages)
            .await
            .expect("branch summary");
        assert_eq!(summary, "durable branch summary");
        assert!(runtime.messages_snapshot().await.is_empty());
        assert!(!operations.abort_branch_summary("alpha").await);
    }

    #[tokio::test]
    async fn branch_summary_abort_cancels_only_active_summary() {
        let workspace = tempfile::TempDir::new().expect("workspace");
        let state = tempfile::TempDir::new().expect("state");
        let runtime =
            fake_runtime_with_delay(workspace.path(), "too late", Duration::from_secs(30)).await;
        let operations = Arc::new(RuntimeOperations::new(state.path().to_owned()));
        let command = PublicDaemonCommand::new(
            "navigate_tree",
            [("replaceInstructions".into(), Value::Bool(false))],
        )
        .expect("navigate command");
        let running_operations = Arc::clone(&operations);
        let running_runtime = Arc::clone(&runtime);
        let abandoned_messages = vec![Message::user("abandoned branch only")];
        let task = tokio::spawn(async move {
            running_operations
                .summarize_branch("alpha", &command, running_runtime, &abandoned_messages)
                .await
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if operations
                    .branch_summaries
                    .lock()
                    .await
                    .contains_key("alpha")
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("summary did not start");
        assert!(operations.abort_branch_summary("alpha").await);
        let result = tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("summary did not stop")
            .expect("summary task panicked");
        assert!(result.is_err());
        assert!(!operations.abort_branch_summary("alpha").await);
    }

    #[tokio::test]
    async fn rlm_depth_status_reports_global_session_and_effective_values() {
        let workspace = tempfile::TempDir::new().expect("workspace");
        let state = tempfile::TempDir::new().expect("state");
        let runtime = fake_runtime(workspace.path(), "unused").await;
        let operations = Arc::new(RuntimeOperations::new(state.path().to_owned()));
        set_tui_rlm_max_depth(state.path(), "alpha", 7, true)
            .await
            .expect("set global depth");
        set_tui_rlm_max_depth(state.path(), "alpha", 3, false)
            .await
            .expect("set session depth");
        let status =
            PublicDaemonCommand::new("get_rlm_max_depth_status", Vec::<(String, Value)>::new())
                .expect("status command");
        let value = operations
            .handle("alpha", &status, runtime, None)
            .await
            .expect("depth status")
            .expect("handled");
        assert_eq!(value["maxDepth"], 3);
        assert_eq!(value["effectiveMaxDepth"], 3);
        assert_eq!(value["globalMaxDepth"], 7);
        assert_eq!(value["sessionMaxDepth"], 3);
        assert_eq!(value["source"], "session");
    }
}
