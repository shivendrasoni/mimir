use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, RwLock};
use tokio_util::sync::CancellationToken;

use crate::{
    budget::{Budget, BudgetPause, BudgetUsage},
    error::{MimirError, Result},
    extensions::{
        ExtensionCommandInfo, ExtensionContextUsage, ExtensionFlagValue, ExtensionHostAction,
        ExtensionHostSnapshot, ExtensionManager, ExtensionToolInfo, LifecycleEvent,
        LifecycleInterception, LifecycleMutation, LifecycleReplacement, SessionStartReason,
        UiRequest,
    },
    model::{Content, Message, ModelRequest, Role, StopReason, ThinkingLevel},
    provider::{
        AuthenticationRefreshStatus, Provider, ProviderError, ProviderEvent, ProviderEventSink,
    },
    runtime_events::{RuntimeEventBus, RuntimeEventEnvelope, RuntimeEventSource},
    session::{SessionPayload, SessionRecord, SessionStore},
    session_integrity,
    skills::{SkillInvocationError, SkillRuntime},
    tools::{ObservationStatus, PermissionRequest, ToolObservation, ToolRegistry},
};

const AGENT_MESSAGE_PREFIX: &str = "Agent-to-agent message received.\nSource: agent_message\n";
const MAX_PENDING_AGENT_MESSAGES: usize = 20;
const PROVENANCE_SYSTEM_GUIDANCE: &str = "Evidence rule: when write_file or edit_file content is derived from a source file, you MUST include provenance.required=true and provenance.derivedFrom entries containing the successful read_file toolCallId and exact path. A failed or unavailable read is not evidence. Do not claim copied, preserved, or source-derived content without those references; read the source successfully or explain that the mutation cannot be completed faithfully.";

#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    pub provider: String,
    pub model: String,
    pub thinking_level: ThinkingLevel,
    pub supported_thinking_levels: Vec<ThinkingLevel>,
    pub thinking_level_map: Option<BTreeMap<ThinkingLevel, Option<String>>>,
    pub system_prompt: String,
    pub budget: Budget,
    pub provider_timeout: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionDetails {
    pub read_files: Vec<String>,
    pub modified_files: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionResult {
    pub summary: String,
    pub first_kept_entry_id: String,
    pub tokens_before: u64,
    pub details: CompactionDetails,
}

impl RuntimeConfig {
    pub fn default_for_model(model: impl Into<String>) -> Self {
        Self {
            provider: "unknown".into(),
            model: model.into(),
            thinking_level: ThinkingLevel::Off,
            supported_thinking_levels: vec![ThinkingLevel::Off],
            thinking_level_map: None,
            system_prompt: String::new(),
            budget: Budget::default(),
            provider_timeout: Duration::from_secs(15 * 60),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum QueueMode {
    #[serde(rename = "all")]
    All,
    #[default]
    #[serde(rename = "one-at-a-time")]
    OneAtATime,
}

impl QueueMode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::OneAtATime => "one-at-a-time",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub base_delay: Duration,
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            base_delay: Duration::from_secs(2),
            max_delay: Duration::from_secs(30),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RuntimeEvent {
    RunStarted,
    ProviderRequest {
        turn: u32,
        estimated_context_tokens: u64,
    },
    MessageStarted {
        message: Message,
    },
    MessageCompleted {
        message: Message,
    },
    TurnCompleted {
        message: Message,
        tool_results: Vec<Message>,
    },
    ToolStarted {
        id: String,
        name: String,
        arguments: serde_json::Value,
    },
    PermissionRequested {
        request: PermissionRequest,
    },
    ToolUpdated {
        id: String,
        name: String,
        arguments: serde_json::Value,
        observation: ToolObservation,
    },
    ToolFinished {
        id: String,
        name: String,
        observation: ToolObservation,
    },
    TextDelta {
        text: String,
    },
    AutoRetryStarted {
        attempt: u32,
        max_attempts: u32,
        delay_ms: u64,
        error_message: String,
    },
    AutoRetryFinished {
        success: bool,
        attempt: u32,
        final_error: Option<String>,
    },
    Completed {
        text: String,
    },
    Failed {
        message: String,
    },
    BudgetPaused {
        pause: BudgetPause,
    },
    ExtensionUi {
        extension: String,
        request: UiRequest,
    },
    ExtensionRendered {
        custom_type: String,
        lines: Vec<String>,
    },
    ExtensionError {
        extension_path: String,
        event: String,
        error: String,
    },
    /// A control-plane/session event that has no standard provider-loop shape.
    SessionEvent {
        event: serde_json::Value,
    },
}

impl RuntimeEvent {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::RunStarted => "run_started",
            Self::ProviderRequest { .. } => "provider_request",
            Self::MessageStarted { .. } => "message_started",
            Self::MessageCompleted { .. } => "message_completed",
            Self::TurnCompleted { .. } => "turn_completed",
            Self::ToolStarted { .. } => "tool_started",
            Self::PermissionRequested { .. } => "permission_requested",
            Self::ToolUpdated { .. } => "tool_updated",
            Self::ToolFinished { .. } => "tool_finished",
            Self::TextDelta { .. } => "text_delta",
            Self::AutoRetryStarted { .. } => "auto_retry_started",
            Self::AutoRetryFinished { .. } => "auto_retry_finished",
            Self::Completed { .. } => "completed",
            Self::Failed { .. } => "failed",
            Self::BudgetPaused { .. } => "budget_paused",
            Self::ExtensionUi { .. } => "extension_ui",
            Self::ExtensionRendered { .. } => "extension_rendered",
            Self::ExtensionError { .. } => "extension_error",
            Self::SessionEvent { .. } => "session_event",
        }
    }
}

#[async_trait]
pub trait EventSink: Send + Sync {
    async fn emit(&self, event: RuntimeEvent);
}

struct RuntimeProviderSink<'a> {
    sink: &'a dyn EventSink,
    emitted: &'a AtomicBool,
}

struct BroadcastingEventSink<'a> {
    downstream: &'a dyn EventSink,
    bus: &'a RuntimeEventBus,
    source: RuntimeEventSource,
}

#[async_trait]
impl EventSink for BroadcastingEventSink<'_> {
    async fn emit(&self, event: RuntimeEvent) {
        self.bus.publish(Some(self.source), event.clone());
        self.downstream.emit(event).await;
    }
}

#[async_trait]
impl ProviderEventSink for RuntimeProviderSink<'_> {
    async fn emit(&self, event: ProviderEvent) {
        match event {
            ProviderEvent::TextDelta(text) => {
                self.emitted.store(true, Ordering::Release);
                self.sink.emit(RuntimeEvent::TextDelta { text }).await;
            }
            ProviderEvent::ThinkingDelta(_) => {
                self.emitted.store(true, Ordering::Release);
            }
            ProviderEvent::AuthenticationRefresh { provider, status } => {
                let status = match status {
                    AuthenticationRefreshStatus::Started => "started",
                    AuthenticationRefreshStatus::Succeeded => "succeeded",
                    AuthenticationRefreshStatus::Failed => "failed",
                };
                self.sink
                    .emit(RuntimeEvent::SessionEvent {
                        event: serde_json::json!({
                            "type": "oauth_refresh",
                            "provider": provider,
                            "status": status,
                        }),
                    })
                    .await;
            }
        }
    }
}

#[derive(Default)]
pub struct VecEventSink {
    events: Mutex<Vec<RuntimeEvent>>,
}

impl VecEventSink {
    pub async fn events(&self) -> Vec<RuntimeEvent> {
        self.events.lock().await.clone()
    }
}

#[async_trait]
impl EventSink for VecEventSink {
    async fn emit(&self, event: RuntimeEvent) {
        self.events.lock().await.push(event);
    }
}

pub struct AgentRuntime {
    selection: RwLock<RuntimeSelection>,
    tools: Arc<ToolRegistry>,
    store: Arc<dyn SessionStore>,
    config: RuntimeConfig,
    harness_context: RwLock<String>,
    messages: Mutex<Vec<Message>>,
    steering: Mutex<VecDeque<Message>>,
    steering_mode: Mutex<QueueMode>,
    auto_compaction: AtomicBool,
    auto_retry: AtomicBool,
    max_output_tokens: AtomicU32,
    retrying: AtomicBool,
    retry_attempt: AtomicU32,
    compacting: AtomicBool,
    service_tier: RwLock<Option<String>>,
    retry_policy: StdMutex<RetryPolicy>,
    run_lock: Mutex<()>,
    control_lock: Mutex<()>,
    running: AtomicBool,
    cancellation: StdMutex<CancellationToken>,
    control_cancellation: StdMutex<CancellationToken>,
    retry_cancellation: StdMutex<CancellationToken>,
    skills: StdMutex<SkillRuntime>,
    extensions: RwLock<Option<Arc<ExtensionManager>>>,
    extension_session_id: RwLock<String>,
    extension_session_name: RwLock<Option<String>>,
    extension_flags: RwLock<BTreeMap<String, serde_json::Value>>,
    active_tools: RwLock<Option<BTreeSet<String>>>,
    extension_session_started: AtomicBool,
    events: RuntimeEventBus,
}

struct RuntimeSelection {
    provider: Arc<dyn Provider>,
    provider_id: String,
    model: String,
    thinking_level: ThinkingLevel,
    supported_thinking_levels: Vec<ThinkingLevel>,
    thinking_level_map: Option<BTreeMap<ThinkingLevel, Option<String>>>,
}

struct RunningFlag<'a>(&'a AtomicBool);

impl<'a> RunningFlag<'a> {
    fn new(flag: &'a AtomicBool) -> Self {
        flag.store(true, Ordering::Release);
        Self(flag)
    }
}

impl Drop for RunningFlag<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

struct RetryStateGuard<'a> {
    retrying: &'a AtomicBool,
    attempt: &'a AtomicU32,
}

impl Drop for RetryStateGuard<'_> {
    fn drop(&mut self) {
        self.retrying.store(false, Ordering::Release);
        self.attempt.store(0, Ordering::Release);
    }
}

impl AgentRuntime {
    /// Returns a bounded snapshot of recursive children owned by this session.
    ///
    /// # Errors
    ///
    /// Returns a configuration error when `limit` exceeds the persisted RLM
    /// child bound, or a persistence error when the RLM state cannot be read.
    pub async fn context_children(
        &self,
        limit: usize,
    ) -> Result<Vec<crate::extensions::RlmSubagent>> {
        const MAX_CONTEXT_CHILDREN: usize = 64;
        if limit > MAX_CONTEXT_CHILDREN {
            return Err(MimirError::Configuration(format!(
                "context child limit must not exceed {MAX_CONTEXT_CHILDREN}"
            )));
        }
        self.tools.rlm_subagents(limit).await
    }

    /// Cancels one active recursive child owned by this session.
    ///
    /// # Errors
    ///
    /// Returns a persistence or runtime error when the child cannot be resolved or cancelled.
    pub async fn cancel_rlm_child(&self, target: &str) -> Result<bool> {
        self.tools.cancel_rlm_subagent(target).await
    }

    /// Deletes one inactive recursive child owned by this session.
    ///
    /// # Errors
    ///
    /// Returns a persistence or policy error when the child cannot be resolved or is still active.
    pub async fn delete_rlm_child(
        &self,
        target: &str,
    ) -> Result<Option<crate::extensions::RlmDeleteResult>> {
        self.tools.delete_rlm_subagent(target).await
    }

    /// Restores a runtime from its versioned session records.
    ///
    /// # Errors
    ///
    /// Returns a persistence error when the session cannot be loaded.
    pub async fn resume(
        provider: Arc<dyn Provider>,
        tools: Arc<ToolRegistry>,
        store: Arc<dyn SessionStore>,
        config: RuntimeConfig,
    ) -> Result<Self> {
        if config.model.trim().is_empty() {
            return Err(MimirError::Configuration("model must not be blank".into()));
        }
        let loaded = store.load().await?;
        let mut messages = Vec::new();
        for record in loaded.records {
            match record.payload {
                SessionPayload::Message(message) => messages.push(message),
                SessionPayload::Compaction {
                    summary,
                    retained_message_count,
                    ..
                } => {
                    let split = messages.len().saturating_sub(retained_message_count);
                    let retained = messages.split_off(split);
                    messages = vec![Message::system(summary)];
                    messages.extend(retained);
                }
                SessionPayload::RuntimeEvent { .. } => {}
            }
        }
        repair_message_integrity(
            store.as_ref(),
            &mut messages,
            "session resumed after an interrupted tool turn",
        )
        .await?;
        let supported_thinking_levels =
            normalize_thinking_levels(&config.supported_thinking_levels);
        let thinking_level =
            clamp_thinking_level(config.thinking_level, &supported_thinking_levels);
        Ok(Self {
            selection: RwLock::new(RuntimeSelection {
                provider,
                provider_id: config.provider.clone(),
                model: config.model.clone(),
                thinking_level,
                supported_thinking_levels,
                thinking_level_map: config.thinking_level_map.clone(),
            }),
            tools,
            store,
            config,
            harness_context: RwLock::new(String::new()),
            messages: Mutex::new(messages),
            steering: Mutex::new(VecDeque::new()),
            steering_mode: Mutex::new(QueueMode::default()),
            auto_compaction: AtomicBool::new(true),
            auto_retry: AtomicBool::new(true),
            max_output_tokens: AtomicU32::new(16_384),
            retrying: AtomicBool::new(false),
            retry_attempt: AtomicU32::new(0),
            compacting: AtomicBool::new(false),
            service_tier: RwLock::new(None),
            retry_policy: StdMutex::new(RetryPolicy::default()),
            run_lock: Mutex::new(()),
            control_lock: Mutex::new(()),
            running: AtomicBool::new(false),
            cancellation: StdMutex::new(CancellationToken::new()),
            control_cancellation: StdMutex::new(CancellationToken::new()),
            retry_cancellation: StdMutex::new(CancellationToken::new()),
            skills: StdMutex::new(SkillRuntime::default()),
            extensions: RwLock::new(None),
            extension_session_id: RwLock::new(String::new()),
            extension_session_name: RwLock::new(None),
            extension_flags: RwLock::new(BTreeMap::new()),
            active_tools: RwLock::new(None),
            extension_session_started: AtomicBool::new(false),
            events: RuntimeEventBus::default(),
        })
    }

    /// Subscribes to the bounded session-lifetime runtime event stream.
    #[must_use]
    pub fn subscribe_events(&self) -> tokio::sync::broadcast::Receiver<RuntimeEventEnvelope> {
        self.events.subscribe()
    }

    /// Publishes a validated out-of-band session event for daemon and ACP clients.
    ///
    /// # Errors
    ///
    /// Returns a protocol error when the event lacks a non-empty `type` or its
    /// serialized representation exceeds 256 KiB.
    pub fn publish_session_event(&self, event: serde_json::Value) -> Result<()> {
        const MAX_SESSION_EVENT_BYTES: usize = 256 * 1024;
        let event_type = event
            .get("type")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                MimirError::Protocol("session event type must be a non-empty string".into())
            })?;
        if serde_json::to_vec(&event)?.len() > MAX_SESSION_EVENT_BYTES {
            return Err(MimirError::Protocol(format!(
                "session event {event_type} exceeds {MAX_SESSION_EVENT_BYTES} bytes"
            )));
        }
        self.events.publish_session_event(event);
        Ok(())
    }

    pub async fn attach_extension_manager(&self, manager: Arc<ExtensionManager>, session_id: &str) {
        *self.extensions.write().await = Some(manager);
        *self.extension_session_id.write().await = session_id.to_owned();
        self.extension_session_started
            .store(false, Ordering::Release);
    }

    /// Replaces the immutable skill catalog used to expand explicit invocations.
    pub fn attach_skill_runtime(&self, skills: SkillRuntime) {
        *self
            .skills
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = skills;
    }

    pub async fn extension_manager(&self) -> Option<Arc<ExtensionManager>> {
        self.extensions.read().await.clone()
    }

    /// Returns the bounded runtime state made visible to extension callbacks.
    /// The snapshot deliberately contains no provider credentials or filesystem
    /// handles.
    pub async fn extension_host_snapshot(&self) -> ExtensionHostSnapshot {
        let manager = self.extensions.read().await.clone();
        let definitions = self.tools.definitions();
        let configured = definitions
            .iter()
            .map(|definition| definition.name.clone())
            .collect::<BTreeSet<_>>();
        let active_tools = self
            .active_tools
            .read()
            .await
            .clone()
            .unwrap_or_else(|| configured.clone());
        let selection = self.selection.read().await;
        let messages = self.messages.lock().await;
        let estimated_tokens = messages
            .iter()
            .map(Message::text)
            .map(|text| u64::try_from(text.len().div_ceil(4)).unwrap_or(u64::MAX))
            .fold(0_u64, u64::saturating_add);
        let all_tools = definitions
            .into_iter()
            .map(|definition| ExtensionToolInfo {
                name: definition.name,
                description: definition.description,
                parameters: definition.parameters,
                source: "runtime".into(),
            })
            .collect();
        let commands = manager
            .as_ref()
            .map(|manager| {
                manager
                    .commands()
                    .into_iter()
                    .map(|command| ExtensionCommandInfo {
                        name: command.name,
                        description: command.description,
                        source: "extension".into(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let configured_flags = self.extension_flags.read().await;
        let flags = manager
            .as_ref()
            .map(|manager| {
                manager
                    .flags()
                    .into_iter()
                    .filter_map(|flag| {
                        configured_flags
                            .get(&flag.name)
                            .cloned()
                            .or(flag.default)
                            .map(|value| ExtensionFlagValue {
                                name: flag.name,
                                value,
                            })
                    })
                    .collect()
            })
            .unwrap_or_default();
        ExtensionHostSnapshot {
            cwd: manager
                .as_ref()
                .map(|manager| manager.workspace_root().display().to_string())
                .unwrap_or_default(),
            session_id: self.extension_session_id.read().await.clone(),
            session_name: self.extension_session_name.read().await.clone(),
            provider: selection.provider_id.clone(),
            model: selection.model.clone(),
            thinking_level: selection.thinking_level,
            active_tools: active_tools.into_iter().collect(),
            all_tools,
            commands,
            flags,
            is_idle: !self.is_running(),
            has_pending_messages: !self.steering.lock().await.is_empty(),
            system_prompt: self.combined_system_prompt(None).await,
            context_usage: ExtensionContextUsage {
                messages: messages.len(),
                estimated_tokens,
                max_messages: self.config.budget.max_context_messages,
            },
        }
    }

    /// Applies post-isolate actions after validating their bounds and requested
    /// runtime targets. JavaScript never mutates runtime state directly.
    ///
    /// # Errors
    ///
    /// Returns a protocol error for malformed actions, unknown tools/models, or
    /// a batch above the configured safety bound.
    pub async fn apply_extension_actions(&self, actions: &[ExtensionHostAction]) -> Result<()> {
        const MAX_ACTIONS: usize = 64;
        const MAX_ACTION_BYTES: usize = 256 * 1024;
        if actions.len() > MAX_ACTIONS {
            return Err(MimirError::Protocol(format!(
                "extension returned more than {MAX_ACTIONS} host actions"
            )));
        }
        if serde_json::to_vec(actions)?.len() > MAX_ACTION_BYTES {
            return Err(MimirError::Protocol(format!(
                "extension host actions exceed {MAX_ACTION_BYTES} bytes"
            )));
        }
        for action in actions {
            self.apply_extension_action(action).await?;
        }
        Ok(())
    }

    #[allow(
        clippy::too_many_lines,
        reason = "the exhaustive host-action match keeps every extension authority transition in one auditable gate"
    )]
    async fn apply_extension_action(&self, action: &ExtensionHostAction) -> Result<()> {
        match action {
            ExtensionHostAction::SendMessage {
                custom_type,
                content,
                display,
                details,
                trigger_turn,
                deliver_as,
            } => {
                validate_extension_name("custom message type", custom_type)?;
                self.record_runtime_event_raw(
                    "extension_message",
                    &serde_json::json!({
                        "type": "extension_message",
                        "customType": custom_type,
                        "content": content,
                        "display": display,
                        "details": details,
                        "deliverAs": deliver_as,
                    })
                    .to_string(),
                )
                .await?;
                if *trigger_turn {
                    let text = extension_content_text(content)?;
                    self.steer(&text).await?;
                }
            }
            ExtensionHostAction::SendUserMessage {
                content,
                deliver_as: _,
            } => self.steer_message(extension_user_message(content)?).await?,
            ExtensionHostAction::AppendEntry { custom_type, data } => {
                validate_extension_name("entry type", custom_type)?;
                self.record_runtime_event_raw(
                    "extension_entry",
                    &serde_json::json!({
                        "type": "extension_entry",
                        "customType": custom_type,
                        "data": data,
                    })
                    .to_string(),
                )
                .await?;
            }
            ExtensionHostAction::SetSessionName { name } => {
                let name = name.trim();
                if name.is_empty() || name.len() > 256 {
                    return Err(MimirError::Protocol(
                        "extension session name must contain 1 to 256 bytes".into(),
                    ));
                }
                *self.extension_session_name.write().await = Some(name.into());
                self.record_runtime_event_raw("session_name", name).await?;
            }
            ExtensionHostAction::SetLabel { entry_id, label } => {
                validate_extension_name("entry id", entry_id)?;
                if label.as_ref().is_some_and(|label| label.len() > 256) {
                    return Err(MimirError::Protocol(
                        "extension entry label exceeds 256 bytes".into(),
                    ));
                }
                self.record_runtime_event_raw(
                    "entry_label",
                    &serde_json::json!({"entryId": entry_id, "label": label}).to_string(),
                )
                .await?;
            }
            ExtensionHostAction::SetActiveTools { names } => {
                let available = self
                    .tools
                    .definitions()
                    .into_iter()
                    .map(|definition| definition.name)
                    .collect::<BTreeSet<_>>();
                let selected = names.iter().cloned().collect::<BTreeSet<_>>();
                if selected.len() != names.len() {
                    return Err(MimirError::Protocol(
                        "extension active tool list contains duplicates".into(),
                    ));
                }
                if let Some(unknown) = selected.iter().find(|name| !available.contains(*name)) {
                    return Err(MimirError::Protocol(format!(
                        "extension selected unknown tool '{unknown}'"
                    )));
                }
                *self.active_tools.write().await = Some(selected);
            }
            ExtensionHostAction::SetModel { provider, model } => {
                let selection = self.selection.read().await;
                if provider != &selection.provider_id {
                    drop(selection);
                    let manager = self.extensions.read().await.clone().ok_or_else(|| {
                        MimirError::Configuration(
                            "extension provider runtime is unavailable".into(),
                        )
                    })?;
                    let (provider_runtime, levels) =
                        manager.activate_provider(provider, model).await?;
                    self.select_model(provider_runtime, provider, model, levels, None)
                        .await?;
                    return Ok(());
                }
                let active_provider = Arc::clone(&selection.provider);
                let levels = selection.supported_thinking_levels.clone();
                let level_map = selection.thinking_level_map.clone();
                drop(selection);
                self.select_model(active_provider, provider, model, levels, level_map)
                    .await?;
            }
            ExtensionHostAction::SetThinkingLevel { level } => {
                self.set_thinking_level(*level).await?;
            }
            ExtensionHostAction::PublishEvent { topic, data } => {
                validate_extension_name("event topic", topic)?;
                self.publish_session_event(serde_json::json!({
                    "type": "extension_event",
                    "topic": topic,
                    "data": data,
                }))?;
                if let Some(manager) = self.extensions.read().await.clone() {
                    manager.publish_event(topic, data.clone()).await?;
                }
            }
            ExtensionHostAction::Session { action } => {
                let action_value = serde_json::to_value(action)?;
                self.publish_session_event(serde_json::json!({
                    "type": "extension_session_action",
                    "action": action_value,
                }))?;
                self.record_runtime_event_raw(
                    "extension_session_action",
                    &serde_json::to_string(action)?,
                )
                .await?;
            }
            ExtensionHostAction::Abort => self.cancel_run(),
            ExtensionHostAction::Shutdown => {
                self.cancel();
                self.record_runtime_event_raw("extension_shutdown", "requested")
                    .await?;
            }
        }
        Ok(())
    }

    async fn active_tool_definitions(&self) -> Vec<crate::model::ToolDefinition> {
        let definitions = self.tools.definitions();
        let active = self.active_tools.read().await;
        active.as_ref().map_or(definitions.clone(), |active| {
            definitions
                .into_iter()
                .filter(|definition| active.contains(&definition.name))
                .collect()
        })
    }

    async fn is_tool_active(&self, name: &str) -> bool {
        self.active_tools
            .read()
            .await
            .as_ref()
            .is_none_or(|active| active.contains(name))
    }

    /// Renders a registered custom message and emits its bounded terminal lines.
    ///
    /// # Errors
    ///
    /// Returns an extension protocol error for an unknown renderer, stale runtime,
    /// timeout, malformed response, or output above the configured bounds.
    pub async fn render_extension_message(
        &self,
        custom_type: &str,
        message: serde_json::Value,
        expanded: bool,
        sink: &dyn EventSink,
    ) -> Result<bool> {
        let Some(manager) = self.extensions.read().await.clone() else {
            return Ok(false);
        };
        let output = manager
            .render_message(custom_type, message, expanded)
            .await?;
        sink.emit(RuntimeEvent::ExtensionRendered {
            custom_type: custom_type.to_owned(),
            lines: output.lines,
        })
        .await;
        Ok(true)
    }

    /// Invokes a registered extension command with a current runtime snapshot,
    /// then applies its validated post-isolate actions.
    ///
    /// # Errors
    ///
    /// Returns an extension protocol, capability, timeout, or host-action error.
    pub async fn invoke_extension_command(
        &self,
        name: &str,
        args: &str,
    ) -> Result<crate::extensions::CommandInvocationResult> {
        let manager = self.extensions.read().await.clone().ok_or_else(|| {
            MimirError::Configuration("extension command runtime is unavailable".into())
        })?;
        let snapshot = self.extension_host_snapshot().await;
        let result = manager
            .invoke_command_with_snapshot(name, args, snapshot)
            .await?;
        let actions = manager.drain_host_actions().await;
        self.apply_extension_actions(&actions).await?;
        Ok(result)
    }

    /// Invokes a registered extension shortcut with the same bounded context
    /// and post-isolate action semantics as extension commands.
    ///
    /// # Errors
    ///
    /// Returns an extension protocol, capability, timeout, or host-action error.
    pub async fn invoke_extension_shortcut(
        &self,
        shortcut: &str,
    ) -> Result<crate::extensions::CommandInvocationResult> {
        let manager = self.extensions.read().await.clone().ok_or_else(|| {
            MimirError::Configuration("extension shortcut runtime is unavailable".into())
        })?;
        let result = manager
            .invoke_shortcut_with_snapshot(shortcut, self.extension_host_snapshot().await)
            .await?;
        self.apply_extension_actions(&manager.drain_host_actions().await)
            .await?;
        Ok(result)
    }

    /// Sets one registered extension flag for subsequent callback snapshots.
    ///
    /// # Errors
    ///
    /// Returns a protocol error for an unknown flag or a value of the wrong type.
    pub async fn set_extension_flag(&self, name: &str, value: serde_json::Value) -> Result<()> {
        let manager = self.extensions.read().await.clone().ok_or_else(|| {
            MimirError::Configuration("extension flag runtime is unavailable".into())
        })?;
        let descriptor = manager
            .flags()
            .into_iter()
            .find(|flag| flag.name == name)
            .ok_or_else(|| {
                MimirError::Protocol(format!("extension flag '{name}' is not registered"))
            })?;
        let valid = match descriptor.kind {
            crate::extensions::FlagKind::Boolean => value.is_boolean(),
            crate::extensions::FlagKind::String => value.is_string(),
        };
        if !valid {
            return Err(MimirError::Protocol(format!(
                "extension flag '{name}' value does not match its registered type"
            )));
        }
        self.extension_flags
            .write()
            .await
            .insert(name.into(), value);
        Ok(())
    }

    /// Delivers a correlated client response to one pending extension UI request.
    ///
    /// # Errors
    ///
    /// Returns a protocol error for an unknown, expired, stale, oversized, or
    /// type-incompatible response, or when the extension rejects delivery.
    pub async fn respond_extension_ui(
        &self,
        request_id: &str,
        response: serde_json::Value,
    ) -> Result<()> {
        let manager = self.extensions.read().await.clone().ok_or_else(|| {
            MimirError::Configuration("extension UI runtime is unavailable".into())
        })?;
        let continuation = manager.respond_ui(request_id, response).await?;
        self.apply_extension_actions(&manager.drain_host_actions().await)
            .await?;
        for (extension, request) in manager.drain_ui_requests().await {
            self.publish_session_event(serde_json::json!({
                "type": "extension_ui_request",
                "extension": extension,
                "request": request,
            }))?;
        }
        if let Some(result) = continuation {
            self.publish_session_event(serde_json::json!({
                "type": "extension_command_continuation",
                "message": result.message,
                "output": result.output,
            }))?;
        }
        Ok(())
    }

    pub fn cancel(&self) {
        self.cancel_run();
        self.control_cancellation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .cancel();
    }

    /// Cancels only the current compaction/refinement/side-question operation,
    /// leaving a normal provider turn untouched.
    pub fn abort_control_operation(&self) {
        self.control_cancellation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .cancel();
    }

    /// Emits a process-local session event without writing a session record.
    ///
    /// # Errors
    ///
    /// Returns a protocol error if event sequencing overflows.
    pub fn publish_transient_session_event(&self, event: serde_json::Value) -> Result<()> {
        self.publish_session_event(event)
    }

    fn cancel_run(&self) {
        self.cancellation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .cancel();
    }

    #[must_use]
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }

    /// Waits until the active or already-queued provider run releases the
    /// session's serialized execution slot.
    pub async fn wait_for_idle(&self) {
        let _guard = self.run_lock.lock().await;
    }

    /// Returns the active provider, model, and effective thinking level as one
    /// consistent snapshot.
    pub async fn model_selection(&self) -> (String, String, ThinkingLevel) {
        let selection = self.selection.read().await;
        (
            selection.provider_id.clone(),
            selection.model.clone(),
            selection.thinking_level,
        )
    }

    /// Updates the active provider's request service tier without rebuilding
    /// the session runtime.
    ///
    /// # Errors
    ///
    /// Returns a provider error when the selected backend does not support the tier.
    pub async fn set_service_tier(&self, tier: Option<&str>) -> Result<()> {
        self.selection
            .read()
            .await
            .provider
            .set_service_tier(tier)
            .map_err(|error| MimirError::Provider(error.to_string()))?;
        *self.service_tier.write().await = tier.map(str::to_owned);
        Ok(())
    }

    /// Returns the effective request service tier tracked after provider validation.
    pub async fn service_tier(&self) -> Option<String> {
        self.service_tier.read().await.clone()
    }

    #[must_use]
    pub fn is_compacting(&self) -> bool {
        self.compacting.load(Ordering::Acquire)
    }

    #[must_use]
    pub fn retry_attempt(&self) -> u32 {
        self.retry_attempt.load(Ordering::Acquire)
    }

    #[must_use]
    pub fn is_retrying(&self) -> bool {
        self.retrying.load(Ordering::Acquire)
    }

    /// Counts durable compaction checkpoints in the bound transcript.
    ///
    /// # Errors
    ///
    /// Returns a persistence or decoding error when the session cannot be loaded.
    pub async fn compaction_count(&self) -> Result<usize> {
        Ok(self
            .store
            .load()
            .await?
            .records
            .iter()
            .filter(|record| matches!(record.payload, SessionPayload::Compaction { .. }))
            .count())
    }

    /// Returns the effective active tool names used for the next provider request.
    pub async fn active_tool_names(&self) -> Vec<String> {
        let configured = self
            .tools
            .definitions()
            .into_iter()
            .map(|definition| definition.name)
            .collect::<BTreeSet<_>>();
        self.active_tools
            .read()
            .await
            .clone()
            .unwrap_or(configured)
            .into_iter()
            .collect()
    }

    pub async fn supported_thinking_levels(&self) -> Vec<ThinkingLevel> {
        self.selection
            .read()
            .await
            .supported_thinking_levels
            .clone()
    }

    /// Atomically changes the provider/model used by subsequent provider turns
    /// and durably records the effective thinking level after capability clamping.
    ///
    /// # Errors
    ///
    /// Returns a configuration error for a blank selector or a persistence error
    /// when the session selection cannot be recorded.
    pub async fn select_model(
        &self,
        provider: Arc<dyn Provider>,
        provider_id: &str,
        model: &str,
        supported_thinking_levels: Vec<ThinkingLevel>,
        thinking_level_map: Option<BTreeMap<ThinkingLevel, Option<String>>>,
    ) -> Result<ThinkingLevel> {
        let provider_id = provider_id.trim();
        let model = model.trim();
        if provider_id.is_empty() || model.is_empty() {
            return Err(MimirError::Configuration(
                "provider and model must not be blank".into(),
            ));
        }
        let supported_thinking_levels = normalize_thinking_levels(&supported_thinking_levels);
        let mut selection = self.selection.write().await;
        let thinking_level =
            clamp_thinking_level(selection.thinking_level, &supported_thinking_levels);
        persist_model_selection(self.store.as_ref(), provider_id, model, thinking_level).await?;
        selection.provider = provider;
        selection.provider_id = provider_id.into();
        selection.model = model.into();
        selection.thinking_level = thinking_level;
        selection.supported_thinking_levels = supported_thinking_levels;
        selection.thinking_level_map = thinking_level_map;
        *self.service_tier.write().await = None;
        Ok(thinking_level)
    }

    /// Applies a requested thinking level after clamping it to the active
    /// model's advertised capabilities, then records the selection durably.
    ///
    /// # Errors
    ///
    /// Returns a persistence error when the effective level cannot be recorded.
    pub async fn set_thinking_level(&self, requested: ThinkingLevel) -> Result<ThinkingLevel> {
        let mut selection = self.selection.write().await;
        let effective = clamp_thinking_level(requested, &selection.supported_thinking_levels);
        if effective == selection.thinking_level {
            return Ok(effective);
        }
        persist_model_selection(
            self.store.as_ref(),
            &selection.provider_id,
            &selection.model,
            effective,
        )
        .await?;
        selection.thinking_level = effective;
        Ok(effective)
    }

    /// Advances through the active model's supported levels, or returns `None`
    /// when reasoning is unavailable.
    ///
    /// # Errors
    ///
    /// Returns a persistence error when the next level cannot be recorded.
    pub async fn cycle_thinking_level(&self) -> Result<Option<ThinkingLevel>> {
        let mut selection = self.selection.write().await;
        if selection.supported_thinking_levels.len() <= 1 {
            return Ok(None);
        }
        let current = selection
            .supported_thinking_levels
            .iter()
            .position(|level| *level == selection.thinking_level)
            .unwrap_or(0);
        let next = selection.supported_thinking_levels
            [(current + 1) % selection.supported_thinking_levels.len()];
        persist_model_selection(
            self.store.as_ref(),
            &selection.provider_id,
            &selection.model,
            next,
        )
        .await?;
        selection.thinking_level = next;
        Ok(Some(next))
    }

    pub fn abort_retry(&self) {
        if self.retrying.load(Ordering::Acquire) {
            self.retry_cancellation
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .cancel();
        }
    }

    /// Returns a point-in-time copy of the active conversation.
    pub async fn messages_snapshot(&self) -> Vec<Message> {
        self.messages.lock().await.clone()
    }

    /// Returns the stable system context used by a normal turn before any
    /// invocation-specific skill text is appended.
    ///
    /// Control-plane features use this snapshot to preserve the active agent's
    /// base instructions without inheriting transient tools or skill authority.
    pub async fn system_prompt_snapshot(&self) -> String {
        self.combined_system_prompt(None).await
    }

    /// Returns one currently active tool definition by name.
    #[must_use]
    pub fn tool_definition(&self, name: &str) -> Option<crate::model::ToolDefinition> {
        self.tools
            .definitions()
            .into_iter()
            .find(|definition| definition.name == name)
    }

    /// Replaces the formatted continual-harness context used by subsequent
    /// provider turns. Persistence is owned by the refinement store.
    pub async fn set_harness_context(&self, context: String) {
        *self.harness_context.write().await = context;
    }

    /// Manually summarizes older context and atomically checkpoints the retained
    /// suffix. The provider call is isolated from the conversation: its prompt and
    /// answer are never appended as ordinary messages.
    ///
    /// # Errors
    ///
    /// Returns a protocol error when there is no discardable history, or a typed
    /// provider/persistence error when summary generation or checkpointing fails.
    #[allow(
        clippy::too_many_lines,
        reason = "manual compaction keeps validation, durable checkpointing, transcript replacement, and event publication in one auditable transaction"
    )]
    pub async fn compact(&self, custom_instructions: Option<&str>) -> Result<CompactionResult> {
        if custom_instructions.is_some_and(|instructions| instructions.len() > 16 * 1024) {
            return Err(MimirError::Protocol(
                "customInstructions exceeds the 16 KiB limit".into(),
            ));
        }
        if self.is_running() {
            self.cancel_run();
        }
        let _control_guard = self.control_lock.lock().await;
        let control_cancellation = self.begin_control_operation();
        let _guard = self.run_lock.lock().await;
        let _compacting = RunningFlag::new(&self.compacting);
        self.reset_cancellations();
        if control_cancellation.is_cancelled() {
            return Err(MimirError::Protocol("Compaction cancelled".into()));
        }
        self.ensure_session_integrity("manual compaction boundary")
            .await?;
        let messages = self.messages.lock().await.clone();
        let loaded = self.store.load().await?;
        let newest_compaction = loaded
            .records
            .iter()
            .filter(|record| matches!(record.payload, SessionPayload::Compaction { .. }))
            .map(|record| record.created_at)
            .max();
        let newest_message = loaded
            .records
            .iter()
            .filter(|record| matches!(record.payload, SessionPayload::Message(_)))
            .map(|record| record.created_at)
            .max();
        if newest_compaction
            .is_some_and(|compaction| newest_message.is_none_or(|message| compaction >= message))
        {
            return Err(MimirError::Protocol("Already compacted".into()));
        }
        if messages.len() < 3 {
            return Err(MimirError::Protocol(
                "Session is too short to compact — try again once it grows".into(),
            ));
        }
        let split = manual_compaction_split(&messages, 20_000).ok_or_else(|| {
            MimirError::Protocol("Session is too short to compact — try again once it grows".into())
        })?;
        let split = session_integrity::safe_compaction_split(&messages, split);
        let first_retained = messages.get(split).ok_or_else(|| {
            MimirError::Protocol(
                "First kept entry is unavailable — session may need migration".into(),
            )
        })?;
        let (first_kept_entry_id, retained_message_count) =
            retained_record_span(&loaded.records, first_retained)?;
        let tokens_before = estimate_message_tokens(&messages);
        let prompt = compaction_prompt(&messages[..split], custom_instructions);
        let mut summary = self
            .complete_control_request_unlocked(
                "Summarize the supplied conversation history for a coding agent. Preserve the original request, decisions, progress, failures, file operations, and next steps. Return only the summary.",
                &prompt,
                16_384,
                true,
                &control_cancellation,
            )
            .await?;
        let retained = messages[split..].to_vec();
        let details = extract_file_operations(&messages[..split], &loaded.records);
        append_file_operations(&mut summary, &details);
        self.store
            .append(SessionRecord::new(SessionPayload::Compaction {
                summary: summary.clone(),
                retained_message_count,
                reason: Some("manual".into()),
                first_kept_entry_id: Some(first_kept_entry_id.clone()),
                tokens_before,
                custom_instructions: custom_instructions.map(str::to_owned),
                details: Some(serde_json::to_value(&details)?),
            }))
            .await?;
        let mut active_messages = self.messages.lock().await;
        *active_messages = vec![Message::system(summary.clone())];
        active_messages.extend(retained);
        let result = CompactionResult {
            summary,
            first_kept_entry_id,
            tokens_before,
            details,
        };
        self.publish_session_event(serde_json::json!({
            "type": "compaction_end",
            "result": result
        }))?;
        Ok(result)
    }

    /// Runs a bounded provider completion for control-plane features such as
    /// refinement without mutating the active transcript.
    ///
    /// # Errors
    ///
    /// Returns a protocol error for a blank prompt or invalid provider response,
    /// plus typed provider errors for timeout, retry exhaustion, or cancellation.
    pub async fn complete_control_request(
        &self,
        system_prompt: &str,
        prompt: &str,
        max_output_tokens: u32,
    ) -> Result<String> {
        if prompt.trim().is_empty() {
            return Err(MimirError::Protocol(
                "control request prompt must not be blank".into(),
            ));
        }
        let _control_guard = self.control_lock.lock().await;
        let control_cancellation = self.begin_control_operation();
        let _guard = self.run_lock.lock().await;
        self.reset_cancellations();
        self.complete_control_request_unlocked(
            system_prompt,
            prompt,
            max_output_tokens,
            false,
            &control_cancellation,
        )
        .await
    }

    /// Persists a non-message control-plane result in the active session.
    ///
    /// # Errors
    ///
    /// Returns a protocol error for a blank name or a persistence error when the
    /// event cannot be appended durably.
    pub async fn record_runtime_event(&self, name: &str, detail: &str) -> Result<()> {
        if name.trim().is_empty() {
            return Err(MimirError::Protocol(
                "runtime event name must not be blank".into(),
            ));
        }
        let mut parsed = serde_json::from_str::<serde_json::Value>(detail)
            .unwrap_or_else(|_| serde_json::Value::String(detail.into()));
        if name == "refinement" {
            let sink = VecEventSink::default();
            let session_id = self.extension_session_id.read().await.clone();
            let outcomes = self
                .dispatch_extension_outcomes(
                    LifecycleEvent::RefineComplete {
                        session_id,
                        result: parsed.clone(),
                    },
                    &sink,
                )
                .await?;
            for outcome in outcomes {
                match outcome.interception {
                    LifecycleInterception::Mutate {
                        mutation: LifecycleMutation::RefineComplete { result },
                    }
                    | LifecycleInterception::Replace {
                        replacement: LifecycleReplacement::RefineComplete { result },
                    } => parsed = result,
                    LifecycleInterception::Continue => {}
                    LifecycleInterception::Block { .. } => unreachable!(
                        "refinement interception cannot block after runtime validation"
                    ),
                    LifecycleInterception::Mutate { .. }
                    | LifecycleInterception::Replace { .. } => unreachable!(
                        "extension runtime validates interception kinds before dispatch"
                    ),
                }
            }
        }
        let persisted_detail = if name == "refinement" {
            serde_json::to_string(&parsed)?
        } else {
            detail.into()
        };
        self.record_runtime_event_raw(name, &persisted_detail).await
    }

    async fn record_runtime_event_raw(&self, name: &str, detail: &str) -> Result<()> {
        if name.trim().is_empty() {
            return Err(MimirError::Protocol(
                "runtime event name must not be blank".into(),
            ));
        }
        let parsed = serde_json::from_str::<serde_json::Value>(detail)
            .unwrap_or_else(|_| serde_json::Value::String(detail.into()));
        self.store
            .append(SessionRecord::new(SessionPayload::RuntimeEvent {
                name: name.into(),
                detail: detail.into(),
            }))
            .await?;
        let event = if name == "refinement" {
            serde_json::json!({"type": "refine_complete", "result": parsed})
        } else if parsed.get("type").is_some() {
            parsed
        } else {
            serde_json::json!({"type": name, "detail": parsed})
        };
        self.publish_session_event(event)
    }

    async fn complete_control_request_unlocked(
        &self,
        system_prompt: &str,
        prompt: &str,
        max_output_tokens: u32,
        use_reasoning: bool,
        cancellation: &CancellationToken,
    ) -> Result<String> {
        let (provider, model, thinking_level, thinking_effort) = {
            let selection = self.selection.read().await;
            let thinking_level = if use_reasoning {
                selection.thinking_level
            } else {
                ThinkingLevel::Off
            };
            let thinking_effort = use_reasoning
                .then(|| {
                    selection
                        .thinking_level_map
                        .as_ref()
                        .and_then(|mapping| mapping.get(&thinking_level))
                        .cloned()
                        .flatten()
                        .or_else(|| {
                            (thinking_level != ThinkingLevel::Off)
                                .then(|| thinking_level.as_str().into())
                        })
                })
                .flatten();
            (
                selection.provider.clone(),
                selection.model.clone(),
                thinking_level,
                thinking_effort,
            )
        };
        let request = ModelRequest {
            model,
            thinking_level,
            thinking_effort,
            system_prompt: system_prompt.into(),
            messages: vec![Message::user(prompt)],
            tools: Vec::new(),
            max_output_tokens: max_output_tokens.clamp(1, 32_000),
        };
        let sink = VecEventSink::default();
        let response = self
            .request_provider(provider, request, cancellation, &sink)
            .await?;
        if response.message.role != Role::Assistant {
            return Err(MimirError::Protocol(
                "provider returned a non-assistant control response".into(),
            ));
        }
        match response.message.stop_reason.unwrap_or(StopReason::Error) {
            StopReason::Stop => {}
            StopReason::Length => {
                return Err(MimirError::Protocol(
                    "control response exceeded its output limit".into(),
                ));
            }
            reason => {
                return Err(MimirError::Protocol(format!(
                    "control response stopped unexpectedly: {reason:?}"
                )));
            }
        }
        let text = response.message.text();
        if text.trim().is_empty() {
            return Err(MimirError::Protocol(
                "provider returned an empty control response".into(),
            ));
        }
        Ok(text)
    }

    /// Records a completed user-initiated shell command for the next model turn.
    ///
    /// The output fence is always longer than any backtick run in captured output,
    /// so shell text cannot terminate its own fenced block.
    ///
    /// # Errors
    ///
    /// Returns a persistence error when the session record cannot be appended.
    pub async fn record_bash_execution(
        &self,
        command: &str,
        result: &crate::tools::BashResult,
    ) -> Result<()> {
        let mut text = format!("Ran `{command}`\n");
        if result.output.is_empty() {
            text.push_str("(no output)");
        } else {
            let longest_backtick_run = result
                .output
                .split(|character| character != '`')
                .map(str::len)
                .max()
                .unwrap_or(0);
            let fence = "`".repeat(longest_backtick_run.saturating_add(1).max(3));
            text.push_str(&fence);
            text.push('\n');
            text.push_str(&result.output);
            text.push('\n');
            text.push_str(&fence);
        }
        if result.cancelled {
            text.push_str("\n\n(command cancelled)");
        } else if let Some(exit_code) = result.exit_code
            && exit_code != 0
        {
            use std::fmt::Write as _;
            write!(text, "\n\nCommand exited with code {exit_code}")
                .expect("writing to a String cannot fail");
        }
        if result.truncated {
            if let Some(path) = &result.full_output_path {
                use std::fmt::Write as _;
                write!(
                    text,
                    "\n\n[Output truncated. Full output: {}]",
                    path.display()
                )
                .expect("writing to a String cannot fail");
            } else {
                text.push_str("\n\n[Output truncated.]");
            }
        }
        self.persist_message(Message::user(text)).await
    }

    /// Runs the extension `user_bash` interception chain before a user-owned
    /// shell command reaches the bounded bash runner.
    ///
    /// # Errors
    ///
    /// Returns an error when the command is invalid or an extension hook
    /// rejects or fails to process the request.
    pub async fn intercept_user_bash(
        &self,
        command: &str,
        cwd: Option<&str>,
    ) -> Result<(String, Option<String>)> {
        if command.trim().is_empty() || command.len() > 64 * 1024 || command.contains('\0') {
            return Err(MimirError::Protocol(
                "user bash command exceeds its bound".into(),
            ));
        }
        let session_id = self.extension_session_id.read().await.clone();
        let sink = VecEventSink::default();
        let mut command = command.to_owned();
        let mut cwd = cwd.map(str::to_owned);
        let outcomes = self
            .dispatch_extension_outcomes(
                LifecycleEvent::UserBash {
                    session_id,
                    command: command.clone(),
                    cwd: cwd.clone(),
                },
                &sink,
            )
            .await?;
        for outcome in outcomes {
            match outcome.interception {
                LifecycleInterception::Block { reason } => {
                    return Err(MimirError::Protocol(reason.unwrap_or_else(|| {
                        "user bash command was blocked by an extension".into()
                    })));
                }
                LifecycleInterception::Mutate {
                    mutation:
                        LifecycleMutation::UserBash {
                            command: changed,
                            cwd: changed_cwd,
                        },
                }
                | LifecycleInterception::Replace {
                    replacement:
                        LifecycleReplacement::UserBash {
                            command: changed,
                            cwd: changed_cwd,
                        },
                } => {
                    if changed.trim().is_empty()
                        || changed.len() > 64 * 1024
                        || changed.contains('\0')
                    {
                        return Err(MimirError::Protocol(
                            "extension user bash command exceeds its bound".into(),
                        ));
                    }
                    command = changed;
                    cwd = changed_cwd;
                }
                LifecycleInterception::Continue => {}
                LifecycleInterception::Mutate { .. } | LifecycleInterception::Replace { .. } => {
                    unreachable!("extension runtime validates interception kinds before dispatch")
                }
            }
        }
        for event in sink.events().await {
            if let RuntimeEvent::ExtensionUi { extension, request } = event {
                self.publish_session_event(serde_json::json!({
                    "type": "extension_ui_request",
                    "extension": extension,
                    "request": request,
                }))?;
            }
        }
        Ok((command, cwd))
    }

    /// Returns the text from the newest assistant message, if one exists.
    pub async fn last_assistant_text(&self) -> Option<String> {
        self.messages
            .lock()
            .await
            .iter()
            .rev()
            .find(|message| message.role == Role::Assistant)
            .map(Message::text)
    }

    /// Queues an instruction for the next provider turn in the active run.
    ///
    /// # Errors
    ///
    /// Returns a protocol error for blank input or when the bounded queue is full.
    pub async fn steer(&self, message: &str) -> Result<()> {
        let message = message.trim();
        if message.is_empty() {
            return Err(MimirError::Protocol(
                "steering message must not be blank".into(),
            ));
        }
        self.steer_message(Message::user(message)).await
    }

    /// Queues a validated multimodal user message for the next provider turn.
    ///
    /// # Errors
    ///
    /// Returns a protocol error for empty/non-user input or a full bounded queue.
    pub async fn steer_message(&self, message: Message) -> Result<()> {
        if message.role != Role::User || message.content.is_empty() {
            return Err(MimirError::Protocol(
                "steering input must be a non-empty user message".into(),
            ));
        }
        self.skill_context_for_messages(std::slice::from_ref(&message))
            .map_err(|error| skill_invocation_error(&error))?;
        let mut steering = self.steering.lock().await;
        if steering.len() >= 64 {
            return Err(MimirError::Protocol(
                "steering queue reached its 64-message limit".into(),
            ));
        }
        steering.push_back(message);
        Ok(())
    }

    pub async fn pending_steering_count(&self) -> usize {
        self.steering.lock().await.len()
    }

    /// Returns bounded text previews for the public queue snapshot.
    pub async fn pending_steering_previews(&self) -> Vec<String> {
        self.steering
            .lock()
            .await
            .iter()
            .map(|message| {
                let text = message.text();
                if text.is_empty() {
                    "[image]".into()
                } else {
                    text
                }
            })
            .collect()
    }

    /// Clears ordinary queued steering messages and returns their public previews.
    pub async fn clear_steering(&self) -> Vec<String> {
        let mut steering = self.steering.lock().await;
        steering
            .drain(..)
            .map(|message| {
                let text = message.text();
                if text.is_empty() {
                    "[image]".into()
                } else {
                    text
                }
            })
            .collect()
    }

    /// Queues a validated agent-to-agent prompt for steering delivery.
    ///
    /// # Errors
    ///
    /// Returns a protocol error when the prompt is malformed or the dedicated
    /// agent-message queue has reached its bounded capacity.
    pub async fn queue_agent_message(&self, prompt: &str) -> Result<()> {
        if !is_agent_message_prompt(prompt) {
            return Err(MimirError::Protocol(
                "agent message prompt has an invalid envelope".into(),
            ));
        }
        let mut steering = self.steering.lock().await;
        let pending = steering
            .iter()
            .filter(|message| is_agent_message_prompt(&message.text()))
            .count();
        if pending >= MAX_PENDING_AGENT_MESSAGES {
            return Err(MimirError::Protocol(format!(
                "target session has too many pending agent messages: limit is {MAX_PENDING_AGENT_MESSAGES}"
            )));
        }
        if steering.len() >= 64 {
            return Err(MimirError::Protocol(
                "steering queue reached its 64-message limit".into(),
            ));
        }
        steering.push_back(Message::user(prompt));
        drop(steering);
        self.publish_session_event(serde_json::json!({
            "type": "inbound_agent_message",
            "message": agent_message_metadata(prompt)
        }))?;
        Ok(())
    }

    /// Removes queued agent-to-agent prompts without affecting ordinary user steering.
    pub async fn clear_queued_agent_messages(&self) -> usize {
        let mut steering = self.steering.lock().await;
        let before = steering.len();
        steering.retain(|message| !is_agent_message_prompt(&message.text()));
        before.saturating_sub(steering.len())
    }

    pub async fn set_steering_mode(&self, mode: QueueMode) {
        *self.steering_mode.lock().await = mode;
    }

    pub async fn steering_mode(&self) -> QueueMode {
        *self.steering_mode.lock().await
    }

    pub fn set_auto_compaction(&self, enabled: bool) {
        self.auto_compaction.store(enabled, Ordering::Release);
    }

    pub fn auto_compaction_enabled(&self) -> bool {
        self.auto_compaction.load(Ordering::Acquire)
    }

    pub fn set_auto_retry(&self, enabled: bool) {
        self.auto_retry.store(enabled, Ordering::Release);
    }

    pub fn auto_retry_enabled(&self) -> bool {
        self.auto_retry.load(Ordering::Acquire)
    }

    /// Overrides the per-provider-request output ceiling for this runtime.
    ///
    /// # Errors
    ///
    /// Returns a configuration error when the limit is zero.
    pub fn set_max_output_tokens(&self, max_output_tokens: u32) -> Result<()> {
        if max_output_tokens == 0 {
            return Err(MimirError::Configuration(
                "max output tokens must be greater than zero".into(),
            ));
        }
        self.max_output_tokens
            .store(max_output_tokens, Ordering::Release);
        Ok(())
    }

    pub fn set_retry_policy(&self, policy: RetryPolicy) {
        *self
            .retry_policy
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = policy;
    }

    /// Runs one user prompt through the bounded provider/tool loop.
    ///
    /// # Errors
    ///
    /// Returns a typed provider, persistence, protocol, cancellation, timeout, or budget error.
    pub async fn run(&self, prompt: &str, sink: &dyn EventSink) -> Result<String> {
        self.run_prompts(&[prompt], sink).await
    }

    /// Runs a non-empty batch of user messages as one provider sequence.
    ///
    /// Each prompt remains a distinct user message, matching the reference harness's
    /// `all` follow-up queue semantics.
    ///
    /// # Errors
    ///
    /// Returns a protocol error for an empty batch or blank prompt, plus the same
    /// typed errors as [`Self::run`].
    pub async fn run_batch(&self, prompts: &[String], sink: &dyn EventSink) -> Result<String> {
        let prompts = prompts.iter().map(String::as_str).collect::<Vec<_>>();
        self.run_prompts(&prompts, sink).await
    }

    async fn run_prompts(&self, prompts: &[&str], sink: &dyn EventSink) -> Result<String> {
        if prompts.is_empty() {
            return Err(MimirError::Protocol(
                "prompt batch must not be empty".into(),
            ));
        }
        if prompts.iter().any(|prompt| prompt.trim().is_empty()) {
            return Err(MimirError::Protocol("prompt must not be blank".into()));
        }
        let messages = prompts
            .iter()
            .map(|prompt| Message::user(prompt.trim()))
            .collect::<Vec<_>>();
        self.run_messages(&messages, RuntimeEventSource::new(), sink)
            .await
    }

    /// Runs a non-empty batch of prevalidated user messages, including images.
    ///
    /// # Errors
    ///
    /// Returns a protocol error for empty, non-user, or content-free messages,
    /// plus the same typed execution errors as [`Self::run_batch`].
    pub async fn run_batch_messages(
        &self,
        messages: &[Message],
        sink: &dyn EventSink,
    ) -> Result<String> {
        if messages.is_empty() {
            return Err(MimirError::Protocol(
                "prompt batch must not be empty".into(),
            ));
        }
        if messages
            .iter()
            .any(|message| message.role != Role::User || message.content.is_empty())
        {
            return Err(MimirError::Protocol(
                "prompt batch must contain non-empty user messages".into(),
            ));
        }
        self.run_messages(messages, RuntimeEventSource::new(), sink)
            .await
    }

    /// Runs a message batch under a caller-provided event source identity.
    ///
    /// This is used by protocol adapters that receive prompt events directly
    /// and subscribe to the runtime broadcast stream concurrently.
    ///
    /// # Errors
    ///
    /// Returns a protocol error for an empty or invalid message batch, plus the
    /// same typed execution errors as [`Self::run_batch_messages`].
    pub async fn run_batch_messages_with_source(
        &self,
        messages: &[Message],
        source: RuntimeEventSource,
        sink: &dyn EventSink,
    ) -> Result<String> {
        if messages.is_empty()
            || messages
                .iter()
                .any(|message| message.role != Role::User || message.content.is_empty())
        {
            return Err(MimirError::Protocol(
                "prompt batch must contain non-empty user messages".into(),
            ));
        }
        self.run_messages(messages, source, sink).await
    }

    async fn run_messages(
        &self,
        messages: &[Message],
        source: RuntimeEventSource,
        sink: &dyn EventSink,
    ) -> Result<String> {
        let active_skill_context = self
            .skill_context_for_messages(messages)
            .map_err(|error| skill_invocation_error(&error))?;
        let _guard = self.run_lock.lock().await;
        let _running = RunningFlag::new(&self.running);
        let sink = BroadcastingEventSink {
            downstream: sink,
            bus: &self.events,
            source,
        };
        let result = self.run_inner(messages, active_skill_context, &sink).await;
        self.finish_run(result, &sink).await
    }

    /// Continues the model loop only when a concurrently queued steering message
    /// was not consumed by the run that was active when it arrived.
    ///
    /// # Errors
    ///
    /// Returns the same provider, persistence, and protocol errors as [`Self::run`].
    pub async fn run_pending_steering(&self, sink: &dyn EventSink) -> Result<Option<String>> {
        let _guard = self.run_lock.lock().await;
        let pending = self.persist_next_steering().await?;
        if pending.is_empty() {
            return Ok(None);
        }
        let active_skill_context = self
            .skill_context_for_messages(&pending)
            .map_err(|error| skill_invocation_error(&error))?;
        let _running = RunningFlag::new(&self.running);
        let sink = BroadcastingEventSink {
            downstream: sink,
            bus: &self.events,
            source: RuntimeEventSource::new(),
        };
        let result = self.run_inner(&[], active_skill_context, &sink).await;
        self.finish_run(result, &sink).await.map(Some)
    }

    async fn finish_run(&self, result: Result<String>, sink: &dyn EventSink) -> Result<String> {
        self.reset_cancellations();
        if result.is_err() {
            self.steering.lock().await.clear();
        }
        match result {
            Ok(answer) => Ok(answer),
            Err(error) => {
                let error_message = error.to_string();
                if error_message.to_ascii_lowercase().contains("extension") {
                    sink.emit(RuntimeEvent::ExtensionError {
                        extension_path: extension_identity_from_error(&error_message),
                        event: "runtime".into(),
                        error: error_message.clone(),
                    })
                    .await;
                }
                let session_id = self.extension_session_id.read().await.clone();
                let _ = self
                    .dispatch_extension_event(
                        LifecycleEvent::AgentEnd {
                            session_id,
                            success: false,
                        },
                        sink,
                    )
                    .await;
                if let MimirError::BudgetPaused(pause) = &error {
                    sink.emit(RuntimeEvent::BudgetPaused { pause: *pause })
                        .await;
                } else {
                    sink.emit(RuntimeEvent::Failed {
                        message: error_message,
                    })
                    .await;
                }
                Err(error)
            }
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "keeping the provider/tool state machine linear makes its persistence ordering auditable"
    )]
    async fn run_inner(
        &self,
        prompts: &[Message],
        mut active_skill_context: Option<String>,
        sink: &dyn EventSink,
    ) -> Result<String> {
        let cancellation = self
            .cancellation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        sink.emit(RuntimeEvent::RunStarted).await;
        let session_id = self.extension_session_id.read().await.clone();
        if !session_id.is_empty() && !self.extension_session_started.swap(true, Ordering::AcqRel) {
            self.dispatch_extension_event(
                LifecycleEvent::SessionStart {
                    session_id: session_id.clone(),
                    reason: SessionStartReason::Resume,
                    previous_session_file: None,
                },
                sink,
            )
            .await?;
        }
        let initial_system_prompt = self
            .combined_system_prompt(active_skill_context.as_deref())
            .await;
        let mut extension_system_prompt_override = None;
        let mut extension_snapshot = self.extension_host_snapshot().await;
        extension_snapshot
            .system_prompt
            .clone_from(&initial_system_prompt);
        let before_agent_outcomes = self
            .dispatch_extension_outcomes_with_snapshot(
                LifecycleEvent::BeforeAgentStart {
                    session_id: session_id.clone(),
                    parent_session_id: None,
                },
                sink,
                extension_snapshot,
            )
            .await?;
        for outcome in before_agent_outcomes {
            if outcome.cancel || matches!(outcome.interception, LifecycleInterception::Block { .. })
            {
                return Err(MimirError::Protocol(
                    "agent start was blocked by an extension".into(),
                ));
            }
            if let Some(system_prompt) = outcome
                .output
                .get("systemPrompt")
                .and_then(serde_json::Value::as_str)
            {
                if system_prompt.len() > 256 * 1024 || system_prompt.contains('\0') {
                    return Err(MimirError::Protocol(
                        "before_agent_start system prompt exceeds its bound".into(),
                    ));
                }
                extension_system_prompt_override = Some(system_prompt.to_owned());
            }
            if let Some(message) = outcome.output.get("message") {
                let custom_type = message
                    .get("customType")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| {
                        MimirError::Protocol(
                            "before_agent_start message requires customType".into(),
                        )
                    })?;
                let content = message
                    .get("content")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                self.apply_extension_actions(&[ExtensionHostAction::SendMessage {
                    custom_type: custom_type.to_owned(),
                    content,
                    display: message
                        .get("display")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false),
                    details: message.get("details").cloned(),
                    trigger_turn: false,
                    deliver_as: crate::extensions::ExtensionDelivery::NextTurn,
                }])
                .await?;
            }
        }
        self.dispatch_extension_event(
            LifecycleEvent::AgentStart {
                session_id: session_id.clone(),
            },
            sink,
        )
        .await?;
        for prompt in prompts {
            let mut prompt = prompt.clone();
            let outcomes = self
                .dispatch_extension_outcomes(
                    LifecycleEvent::Input {
                        session_id: session_id.clone(),
                        message: prompt.clone(),
                    },
                    sink,
                )
                .await?;
            for outcome in outcomes {
                match outcome.interception {
                    LifecycleInterception::Block { reason } => {
                        return Err(MimirError::Protocol(reason.unwrap_or_else(|| {
                            "user input was blocked by an extension".into()
                        })));
                    }
                    LifecycleInterception::Mutate {
                        mutation: LifecycleMutation::Input { message },
                    }
                    | LifecycleInterception::Replace {
                        replacement: LifecycleReplacement::Input { message },
                    } => prompt = message,
                    LifecycleInterception::Continue => {}
                    LifecycleInterception::Mutate { .. }
                    | LifecycleInterception::Replace { .. } => unreachable!(
                        "extension runtime validates interception kinds before dispatch"
                    ),
                }
            }
            let message_id = uuid::Uuid::new_v4().to_string();
            self.dispatch_extension_event(
                LifecycleEvent::MessageStart {
                    session_id: session_id.clone(),
                    message_id: message_id.clone(),
                    role: "user".into(),
                },
                sink,
            )
            .await?;
            let prompt = self
                .dispatch_extension_message_end(&session_id, &message_id, prompt, sink)
                .await?;
            self.persist_message(prompt).await?;
        }
        let mut usage = BudgetUsage::default();

        loop {
            if let Err(error) = usage.check(&self.config.budget) {
                return Err(MimirError::BudgetPaused(error.pause(usage.snapshot())));
            }
            self.ensure_session_integrity("provider request boundary")
                .await?;
            let preflight_system_prompt = extension_system_prompt_override.clone().unwrap_or(
                self.combined_system_prompt(active_skill_context.as_deref())
                    .await,
            );
            let preflight_tools = self.active_tool_definitions().await;
            let max_output_tokens = self.max_output_tokens.load(Ordering::Acquire);
            self.compact_if_needed(
                &preflight_system_prompt,
                &preflight_tools,
                max_output_tokens,
            )
            .await?;
            let mut messages = self.messages.lock().await.clone();
            let context_outcomes = self
                .dispatch_extension_outcomes(
                    LifecycleEvent::Context {
                        session_id: session_id.clone(),
                        messages: messages.clone(),
                    },
                    sink,
                )
                .await?;
            for outcome in context_outcomes {
                match outcome.interception {
                    LifecycleInterception::Mutate {
                        mutation: LifecycleMutation::Context { messages: changed },
                    }
                    | LifecycleInterception::Replace {
                        replacement: LifecycleReplacement::Context { messages: changed },
                    } => messages = changed,
                    LifecycleInterception::Continue => {}
                    LifecycleInterception::Block { .. } => {
                        unreachable!("context interception cannot block after runtime validation")
                    }
                    LifecycleInterception::Mutate { .. }
                    | LifecycleInterception::Replace { .. } => unreachable!(
                        "extension runtime validates interception kinds before dispatch"
                    ),
                }
            }
            normalize_request_messages(&mut messages)?;
            let (provider, mut model, mut thinking_level, thinking_level_map) = {
                let selection = self.selection.read().await;
                (
                    selection.provider.clone(),
                    selection.model.clone(),
                    selection.thinking_level,
                    selection.thinking_level_map.clone(),
                )
            };
            let provider_id = self.selection.read().await.provider_id.clone();
            let model_outcomes = self
                .dispatch_extension_outcomes(
                    LifecycleEvent::ModelSelect {
                        session_id: session_id.clone(),
                        provider: provider_id.clone(),
                        model: model.clone(),
                    },
                    sink,
                )
                .await?;
            for outcome in model_outcomes {
                match outcome.interception {
                    LifecycleInterception::Block { reason } => {
                        return Err(MimirError::Protocol(reason.unwrap_or_else(|| {
                            "model selection was blocked by an extension".into()
                        })));
                    }
                    LifecycleInterception::Mutate {
                        mutation:
                            LifecycleMutation::ModelSelect {
                                provider: changed_provider,
                                model: changed_model,
                            },
                    }
                    | LifecycleInterception::Replace {
                        replacement:
                            LifecycleReplacement::ModelSelect {
                                provider: changed_provider,
                                model: changed_model,
                            },
                    } => {
                        if changed_provider != provider_id {
                            return Err(MimirError::Configuration(format!(
                                "extension selected unbound provider '{changed_provider}'"
                            )));
                        }
                        model = changed_model;
                    }
                    LifecycleInterception::Continue => {}
                    LifecycleInterception::Mutate { .. }
                    | LifecycleInterception::Replace { .. } => unreachable!(
                        "extension runtime validates interception kinds before dispatch"
                    ),
                }
            }
            let thinking_outcomes = self
                .dispatch_extension_outcomes(
                    LifecycleEvent::ThinkingLevelSelect {
                        session_id: session_id.clone(),
                        thinking_level,
                    },
                    sink,
                )
                .await?;
            for outcome in thinking_outcomes {
                match outcome.interception {
                    LifecycleInterception::Block { reason } => {
                        return Err(MimirError::Protocol(reason.unwrap_or_else(|| {
                            "thinking level selection was blocked by an extension".into()
                        })));
                    }
                    LifecycleInterception::Mutate {
                        mutation:
                            LifecycleMutation::ThinkingLevelSelect {
                                thinking_level: changed,
                            },
                    }
                    | LifecycleInterception::Replace {
                        replacement:
                            LifecycleReplacement::ThinkingLevelSelect {
                                thinking_level: changed,
                            },
                    } => thinking_level = changed,
                    LifecycleInterception::Continue => {}
                    LifecycleInterception::Mutate { .. }
                    | LifecycleInterception::Replace { .. } => unreachable!(
                        "extension runtime validates interception kinds before dispatch"
                    ),
                }
            }
            if model.trim().is_empty() {
                return Err(MimirError::Protocol(
                    "extension selected a blank model".into(),
                ));
            }
            let supported = self
                .selection
                .read()
                .await
                .supported_thinking_levels
                .clone();
            thinking_level = clamp_thinking_level(thinking_level, &supported);
            let thinking_effort = thinking_level_map
                .as_ref()
                .and_then(|mapping| mapping.get(&thinking_level))
                .cloned()
                .flatten()
                .or_else(|| {
                    (thinking_level != ThinkingLevel::Off).then(|| thinking_level.as_str().into())
                });
            let effective_system_prompt =
                if let Some(system_prompt) = &extension_system_prompt_override {
                    let workspace = self.tools.workspace_context();
                    format!("{system_prompt}\n\n{workspace}\n\n{PROVENANCE_SYSTEM_GUIDANCE}")
                } else {
                    self.combined_system_prompt(active_skill_context.as_deref())
                        .await
                };
            let request = ModelRequest {
                model,
                thinking_level,
                thinking_effort,
                system_prompt: effective_system_prompt,
                messages: prepare_context_messages(&messages),
                tools: self.active_tool_definitions().await,
                max_output_tokens,
            };
            let turn = usage.snapshot().turns.saturating_add(1);
            self.dispatch_extension_event(
                LifecycleEvent::TurnStart {
                    session_id: session_id.clone(),
                    turn_index: u64::from(turn),
                },
                sink,
            )
            .await?;
            let estimated_context_tokens =
                estimate_request_tokens(&request.system_prompt, &request.messages, &request.tools);
            sink.emit(RuntimeEvent::ProviderRequest {
                turn,
                estimated_context_tokens,
            })
            .await;
            sink.emit(RuntimeEvent::MessageStarted {
                message: Message::assistant_pending(),
            })
            .await;
            self.dispatch_extension_event(
                LifecycleEvent::BeforeProviderRequest {
                    session_id: session_id.clone(),
                    provider: self.selection.read().await.provider_id.clone(),
                    model: request.model.clone(),
                },
                sink,
            )
            .await?;
            let response = match self
                .request_provider(provider, request, &cancellation, sink)
                .await
            {
                Ok(response) => response,
                Err(error) => {
                    self.dispatch_extension_event(
                        LifecycleEvent::AfterProviderResponse {
                            session_id: session_id.clone(),
                            provider: self.selection.read().await.provider_id.clone(),
                            model: self.selection.read().await.model.clone(),
                            success: false,
                        },
                        sink,
                    )
                    .await?;
                    return Err(error);
                }
            };
            self.dispatch_extension_event(
                LifecycleEvent::AfterProviderResponse {
                    session_id: session_id.clone(),
                    provider: self.selection.read().await.provider_id.clone(),
                    model: self.selection.read().await.model.clone(),
                    success: true,
                },
                sink,
            )
            .await?;
            if response.message.role != Role::Assistant {
                return Err(MimirError::Protocol(
                    "provider returned a non-assistant message".into(),
                ));
            }
            usage
                .record_turn(response.message.usage)
                .map_err(|error| MimirError::BudgetPaused(error.pause(usage.snapshot())))?;
            let assistant_message_id = uuid::Uuid::new_v4().to_string();
            self.dispatch_extension_event(
                LifecycleEvent::MessageStart {
                    session_id: session_id.clone(),
                    message_id: assistant_message_id.clone(),
                    role: "assistant".into(),
                },
                sink,
            )
            .await?;
            let assistant_message = self
                .dispatch_extension_message_end(
                    &session_id,
                    &assistant_message_id,
                    response.message,
                    sink,
                )
                .await?;
            let tool_calls: Vec<_> = assistant_message
                .content
                .iter()
                .filter_map(|content| match content {
                    Content::ToolCall(call) => Some(call.clone()),
                    _ => None,
                })
                .collect();
            let stop_reason = assistant_message.stop_reason.unwrap_or(StopReason::Error);
            let final_text = assistant_message.text();
            self.persist_message(assistant_message.clone()).await?;
            sink.emit(RuntimeEvent::MessageCompleted {
                message: assistant_message.clone(),
            })
            .await;

            if tool_calls.is_empty() {
                if stop_reason == StopReason::ToolUse {
                    return Err(MimirError::Protocol(
                        "provider stopped for tool use without a tool call".into(),
                    ));
                }
                let steering = self.persist_next_steering().await?;
                if !steering.is_empty() {
                    active_skill_context = self
                        .skill_context_for_messages(&steering)
                        .map_err(|error| skill_invocation_error(&error))?;
                    continue;
                }
                sink.emit(RuntimeEvent::TurnCompleted {
                    message: assistant_message,
                    tool_results: Vec::new(),
                })
                .await;
                sink.emit(RuntimeEvent::Completed {
                    text: final_text.clone(),
                })
                .await;
                self.dispatch_extension_event(
                    LifecycleEvent::TurnEnd {
                        session_id: session_id.clone(),
                        turn_index: u64::from(turn),
                        stop_reason: Some(format!("{stop_reason:?}").to_ascii_lowercase()),
                    },
                    sink,
                )
                .await?;
                self.dispatch_extension_event(
                    LifecycleEvent::AgentEnd {
                        session_id: session_id.clone(),
                        success: true,
                    },
                    sink,
                )
                .await?;
                return Ok(final_text);
            }

            let mut tool_results = Vec::new();
            for (call_index, call) in tool_calls.iter().cloned().enumerate() {
                if let Err(error) = usage.check(&self.config.budget) {
                    let pause = error.pause(usage.snapshot());
                    let blocked = self
                        .persist_budget_blocked_tool_results(&tool_calls[call_index..], pause, sink)
                        .await?;
                    tool_results.extend(blocked);
                    sink.emit(RuntimeEvent::TurnCompleted {
                        message: assistant_message,
                        tool_results,
                    })
                    .await;
                    return Err(MimirError::BudgetPaused(pause));
                }
                let mut call = call;
                let mut blocked_reason = None;
                let call_outcomes = self
                    .dispatch_extension_outcomes(
                        LifecycleEvent::ToolCall {
                            session_id: session_id.clone(),
                            tool_call: call.clone(),
                        },
                        sink,
                    )
                    .await?;
                for outcome in call_outcomes {
                    match outcome.interception {
                        LifecycleInterception::Block { reason } => {
                            blocked_reason = Some(
                                reason.unwrap_or_else(|| "tool call blocked by extension".into()),
                            );
                        }
                        LifecycleInterception::Mutate {
                            mutation: LifecycleMutation::ToolCall { tool_call },
                        }
                        | LifecycleInterception::Replace {
                            replacement: LifecycleReplacement::ToolCall { tool_call },
                        } => call = tool_call,
                        LifecycleInterception::Continue => {}
                        LifecycleInterception::Mutate { .. }
                        | LifecycleInterception::Replace { .. } => unreachable!(
                            "extension runtime validates interception kinds before dispatch"
                        ),
                    }
                }
                let arguments = call.arguments.clone();
                usage.record_tool_call();
                sink.emit(RuntimeEvent::ToolStarted {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    arguments: arguments.clone(),
                })
                .await;
                self.dispatch_extension_event(
                    LifecycleEvent::ToolExecutionStart {
                        session_id: session_id.clone(),
                        tool_call_id: call.id.clone(),
                        tool_name: call.name.clone(),
                    },
                    sink,
                )
                .await?;
                if let Some(check) =
                    session_integrity::validate_provenance(&self.messages.lock().await, &call)
                {
                    let detail = serde_json::to_string(&serde_json::json!({
                        "type": "provenance_check",
                        "toolCallId": &call.id,
                        "toolName": &call.name,
                        "check": &check
                    }))?;
                    self.record_runtime_event_raw("provenance_check", &detail)
                        .await?;
                    if !check.allowed {
                        blocked_reason = check.warning;
                    }
                }
                let execution = if let Some(reason) = blocked_reason {
                    Err(crate::tools::ToolError::Execution {
                        tool: call.name.clone(),
                        message: reason,
                    })
                } else if self.is_tool_active(&call.name).await {
                    self.tools
                        .execute_cancellable(&call.name, call.arguments, &cancellation)
                        .await
                } else {
                    Err(crate::tools::ToolError::Disabled {
                        tool: call.name.clone(),
                    })
                };
                if let Err(crate::tools::ToolError::ApprovalRequired { request }) = &execution {
                    sink.emit(RuntimeEvent::PermissionRequested {
                        request: request.clone(),
                    })
                    .await;
                }
                let observation = match execution {
                    Ok(observation) => observation,
                    Err(error) => ToolObservation {
                        status: ObservationStatus::Error,
                        summary: error.to_string(),
                        next_actions: vec![
                            "Correct the tool name or arguments before retrying".into(),
                        ],
                        artifacts: Vec::new(),
                        content: String::new(),
                    },
                };
                if let Some(manager) = self.extensions.read().await.clone() {
                    let actions = manager.drain_host_actions().await;
                    self.apply_extension_actions(&actions).await?;
                }
                let mut extension_result = crate::model::ToolResult {
                    tool_call_id: call.id.clone(),
                    tool_name: call.name.clone(),
                    content: serde_json::to_string(&observation)?,
                    is_error: observation.status == ObservationStatus::Error,
                };
                let result_outcomes = self
                    .dispatch_extension_outcomes(
                        LifecycleEvent::ToolResult {
                            session_id: session_id.clone(),
                            tool_result: extension_result.clone(),
                        },
                        sink,
                    )
                    .await?;
                for outcome in result_outcomes {
                    match outcome.interception {
                        LifecycleInterception::Mutate {
                            mutation: LifecycleMutation::ToolResult { tool_result },
                        }
                        | LifecycleInterception::Replace {
                            replacement: LifecycleReplacement::ToolResult { tool_result },
                        } => extension_result = tool_result,
                        LifecycleInterception::Continue => {}
                        LifecycleInterception::Block { .. } => unreachable!(
                            "tool result interception cannot block after runtime validation"
                        ),
                        LifecycleInterception::Mutate { .. }
                        | LifecycleInterception::Replace { .. } => unreachable!(
                            "extension runtime validates interception kinds before dispatch"
                        ),
                    }
                }
                let is_error = extension_result.is_error;
                sink.emit(RuntimeEvent::ToolUpdated {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    arguments,
                    observation: observation.clone(),
                })
                .await;
                let tool_result = Message::tool_result(
                    &extension_result.tool_call_id,
                    &extension_result.tool_name,
                    extension_result.content.clone(),
                    is_error,
                );
                self.persist_message(tool_result.clone()).await?;
                tool_results.push(tool_result);
                sink.emit(RuntimeEvent::ToolFinished {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    observation,
                })
                .await;
                self.flush_extension_ui(sink).await;
                self.dispatch_extension_event(
                    LifecycleEvent::ToolExecutionEnd {
                        session_id: session_id.clone(),
                        tool_call_id: extension_result.tool_call_id,
                        tool_name: extension_result.tool_name,
                        is_error,
                    },
                    sink,
                )
                .await?;
            }
            sink.emit(RuntimeEvent::TurnCompleted {
                message: assistant_message,
                tool_results,
            })
            .await;
            self.dispatch_extension_event(
                LifecycleEvent::TurnEnd {
                    session_id: session_id.clone(),
                    turn_index: u64::from(turn),
                    stop_reason: Some(format!("{stop_reason:?}").to_ascii_lowercase()),
                },
                sink,
            )
            .await?;
            let steering = self.persist_next_steering().await?;
            if !steering.is_empty() {
                active_skill_context = self
                    .skill_context_for_messages(&steering)
                    .map_err(|error| skill_invocation_error(&error))?;
            }
        }
    }

    async fn combined_system_prompt(&self, active_skill_context: Option<&str>) -> String {
        let harness = self.harness_context.read().await;
        let workspace = self.tools.workspace_context();
        let mut parts = Vec::with_capacity(5);
        if !self.config.system_prompt.is_empty() {
            parts.push(self.config.system_prompt.as_str());
        }
        parts.push(workspace.as_str());
        if !harness.is_empty() {
            parts.push(harness.as_str());
        }
        if let Some(skill) = active_skill_context {
            parts.push(skill);
        }
        parts.push(PROVENANCE_SYSTEM_GUIDANCE);
        parts.join("\n\n")
    }

    fn skill_context_for_messages(
        &self,
        messages: &[Message],
    ) -> std::result::Result<Option<String>, SkillInvocationError> {
        self.skills
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .context_for_messages(messages)
    }

    /// Dispatches a typed lifecycle event and emits any validated UI requests.
    ///
    /// # Errors
    ///
    /// Returns an extension protocol error when any subscribed extension fails,
    /// becomes stale, exceeds limits, or returns a malformed response.
    pub async fn dispatch_extension_event(
        &self,
        event: LifecycleEvent,
        sink: &dyn EventSink,
    ) -> Result<bool> {
        let outcomes = self.dispatch_extension_outcomes(event, sink).await?;
        Ok(outcomes.iter().any(|outcome| {
            outcome.cancel || matches!(outcome.interception, LifecycleInterception::Block { .. })
        }))
    }

    async fn dispatch_extension_outcomes(
        &self,
        event: LifecycleEvent,
        sink: &dyn EventSink,
    ) -> Result<Vec<crate::extensions::LifecycleOutcome>> {
        self.dispatch_extension_outcomes_with_snapshot(
            event,
            sink,
            self.extension_host_snapshot().await,
        )
        .await
    }

    async fn dispatch_extension_outcomes_with_snapshot(
        &self,
        event: LifecycleEvent,
        sink: &dyn EventSink,
        snapshot: ExtensionHostSnapshot,
    ) -> Result<Vec<crate::extensions::LifecycleOutcome>> {
        let Some(manager) = self.extensions.read().await.clone() else {
            return Ok(Vec::new());
        };
        let outcomes = manager.dispatch_with_snapshot(event, snapshot).await?;
        let mut collected = Vec::with_capacity(outcomes.len());
        for dispatched in outcomes {
            self.apply_extension_actions(&dispatched.outcome.actions)
                .await?;
            for request in &dispatched.outcome.ui_requests {
                sink.emit(RuntimeEvent::ExtensionUi {
                    extension: dispatched.extension.clone(),
                    request: request.clone(),
                })
                .await;
            }
            collected.push(dispatched.outcome);
        }
        Ok(collected)
    }

    async fn dispatch_extension_message_end(
        &self,
        session_id: &str,
        message_id: &str,
        mut message: Message,
        sink: &dyn EventSink,
    ) -> Result<Message> {
        let original_role = message.role;
        let role = runtime_role_name(original_role).to_owned();
        let outcomes = self
            .dispatch_extension_outcomes(
                LifecycleEvent::MessageEnd {
                    session_id: session_id.to_owned(),
                    message_id: message_id.to_owned(),
                    role,
                    message: message.clone(),
                },
                sink,
            )
            .await?;
        for outcome in outcomes {
            match outcome.interception {
                LifecycleInterception::Mutate {
                    mutation: LifecycleMutation::MessageEnd { message: changed },
                }
                | LifecycleInterception::Replace {
                    replacement: LifecycleReplacement::MessageEnd { message: changed },
                } => {
                    if changed.role != original_role {
                        return Err(MimirError::Protocol(
                            "extension message_end replacement must preserve the message role"
                                .into(),
                        ));
                    }
                    message = changed;
                }
                LifecycleInterception::Continue => {}
                LifecycleInterception::Block { .. } => unreachable!(
                    "extension runtime rejects blocking message_end lifecycle outcomes"
                ),
                LifecycleInterception::Mutate { .. } | LifecycleInterception::Replace { .. } => {
                    unreachable!("extension runtime validates interception kinds before dispatch")
                }
            }
        }
        Ok(message)
    }

    async fn flush_extension_ui(&self, sink: &dyn EventSink) {
        if let Some(manager) = self.extensions.read().await.clone() {
            for (extension, request) in manager.drain_ui_requests().await {
                sink.emit(RuntimeEvent::ExtensionUi { extension, request })
                    .await;
            }
        }
    }

    async fn persist_next_steering(&self) -> Result<Vec<Message>> {
        let mode = self.steering_mode().await;
        let next = {
            let mut steering = self.steering.lock().await;
            match mode {
                QueueMode::All => steering.drain(..).collect::<Vec<_>>(),
                QueueMode::OneAtATime => steering.pop_front().into_iter().collect(),
            }
        };
        for message in &next {
            self.persist_message(message.clone()).await?;
        }
        Ok(next)
    }

    async fn persist_message(&self, message: Message) -> Result<()> {
        self.store
            .append(SessionRecord::new(SessionPayload::Message(message.clone())))
            .await?;
        self.messages.lock().await.push(message);
        Ok(())
    }

    async fn ensure_session_integrity(&self, reason: &str) -> Result<()> {
        let mut messages = self.messages.lock().await;
        repair_message_integrity(self.store.as_ref(), &mut messages, reason).await
    }

    async fn persist_budget_blocked_tool_results(
        &self,
        calls: &[crate::model::ToolCall],
        pause: BudgetPause,
        sink: &dyn EventSink,
    ) -> Result<Vec<Message>> {
        for call in calls {
            let observation = ToolObservation {
                status: ObservationStatus::Error,
                summary: format!("tool not executed: budget paused ({pause})"),
                next_actions: vec![
                    "Resume with a larger budget or start a new run after reducing context".into(),
                ],
                artifacts: Vec::new(),
                content: String::new(),
            };
            sink.emit(RuntimeEvent::ToolFinished {
                id: call.id.clone(),
                name: call.name.clone(),
                observation,
            })
            .await;
        }
        let result =
            session_integrity::interrupted_tool_results(calls, &format!("budget paused: {pause}"));
        self.persist_message(result.clone()).await?;
        Ok(vec![result])
    }

    async fn compact_if_needed(
        &self,
        system_prompt: &str,
        tools: &[crate::model::ToolDefinition],
        max_output_tokens: u32,
    ) -> Result<()> {
        if !self.auto_compaction_enabled() {
            return Ok(());
        }
        self.ensure_session_integrity("automatic compaction boundary")
            .await?;
        let message_limit = self.config.budget.max_context_messages.max(2);
        let context_window = self.config.budget.max_context_tokens.max(1);
        let threshold_percent = u64::from(
            self.config
                .budget
                .auto_compaction_threshold_percent
                .clamp(1, 100),
        );
        let token_threshold = context_window
            .saturating_mul(threshold_percent)
            .checked_div(100)
            .unwrap_or(context_window)
            .max(1);
        let messages = self.messages.lock().await;
        let tokens_before = estimate_request_tokens(system_prompt, &messages, tools);
        let projected_tokens = tokens_before.saturating_add(u64::from(max_output_tokens));
        let message_threshold_reached = messages.len() > message_limit;
        let token_threshold_reached = projected_tokens >= token_threshold;
        if !message_threshold_reached && !token_threshold_reached {
            return Ok(());
        }
        let _compacting = RunningFlag::new(&self.compacting);
        let base_tokens = estimate_request_tokens(system_prompt, &[], tools)
            .saturating_add(u64::from(max_output_tokens));
        let retained_token_target = token_threshold
            .saturating_sub(base_tokens)
            .saturating_mul(3)
            .checked_div(4)
            .unwrap_or(0);
        let split = auto_compaction_split(
            &messages,
            message_limit.saturating_sub(1),
            retained_token_target,
        );
        let Some(split) = split else {
            return Ok(());
        };
        let split = session_integrity::safe_compaction_split(&messages, split);
        let summary = summarize(&messages[..split]);
        let retained = messages[split..].to_vec();
        drop(messages);
        let loaded = self.store.load().await?;
        let first_retained = retained.first().ok_or_else(|| {
            MimirError::Protocol("automatic compaction retained an empty suffix".into())
        })?;
        let (_, retained_record_count) = retained_record_span(&loaded.records, first_retained)?;
        self.store
            .append(SessionRecord::new(SessionPayload::Compaction {
                summary: summary.clone(),
                retained_message_count: retained_record_count,
                reason: Some(if token_threshold_reached {
                    "token_threshold".into()
                } else {
                    "message_threshold".into()
                }),
                first_kept_entry_id: None,
                tokens_before,
                custom_instructions: None,
                details: None,
            }))
            .await?;
        let mut messages = self.messages.lock().await;
        *messages = vec![Message::system(summary)];
        messages.extend(retained);
        drop(messages);
        self.publish_session_event(serde_json::json!({
            "type": "compaction_end",
            "result": {
                "tokensBefore": tokens_before,
                "summary": "automatic context compaction"
            }
        }))?;
        Ok(())
    }

    #[allow(
        clippy::too_many_lines,
        reason = "retry lifecycle ordering remains linear so cancellation and event semantics stay auditable"
    )]
    async fn request_provider(
        &self,
        provider: Arc<dyn Provider>,
        request: ModelRequest,
        cancellation: &CancellationToken,
        sink: &dyn EventSink,
    ) -> Result<crate::model::ModelResponse> {
        let _retry_state = RetryStateGuard {
            retrying: &self.retrying,
            attempt: &self.retry_attempt,
        };
        let policy = *self
            .retry_policy
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let retry_cancellation = self
            .retry_cancellation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let mut attempt = 0_u32;
        loop {
            let emitted = AtomicBool::new(false);
            let provider_sink = RuntimeProviderSink {
                sink,
                emitted: &emitted,
            };
            let outcome = tokio::select! {
                biased;
                () = cancellation.cancelled() => Err(ProviderError::Aborted),
                () = retry_cancellation.cancelled(), if attempt > 0 => {
                    self.retrying.store(false, Ordering::Release);
                    sink.emit(RuntimeEvent::AutoRetryFinished {
                        success: false,
                        attempt,
                        final_error: Some("retry cancelled".into()),
                    }).await;
                    return Err(MimirError::Protocol("retry cancelled".into()));
                }
                result = tokio::time::timeout(
                    self.config.provider_timeout,
                    provider.stream(request.clone(), &provider_sink),
                ) => match result {
                    Ok(result) => result,
                    Err(_) => Err(ProviderError::Unavailable {
                        message: "provider request timed out".into(),
                    }),
                }
            };
            match outcome {
                Ok(response) => {
                    if attempt > 0 {
                        self.retrying.store(false, Ordering::Release);
                        sink.emit(RuntimeEvent::AutoRetryFinished {
                            success: true,
                            attempt,
                            final_error: None,
                        })
                        .await;
                    }
                    return Ok(response);
                }
                Err(ProviderError::Aborted) => {
                    if attempt > 0 {
                        self.retrying.store(false, Ordering::Release);
                        sink.emit(RuntimeEvent::AutoRetryFinished {
                            success: false,
                            attempt,
                            final_error: Some("run cancelled".into()),
                        })
                        .await;
                    }
                    return Err(MimirError::Protocol("run cancelled".into()));
                }
                Err(error)
                    if self.auto_retry_enabled()
                        && error.is_retryable()
                        && !emitted.load(Ordering::Acquire)
                        && attempt < policy.max_attempts =>
                {
                    attempt = attempt.saturating_add(1);
                    self.retry_attempt.store(attempt, Ordering::Release);
                    let delay = retry_delay(policy, attempt);
                    self.retrying.store(true, Ordering::Release);
                    sink.emit(RuntimeEvent::AutoRetryStarted {
                        attempt,
                        max_attempts: policy.max_attempts,
                        delay_ms: duration_millis(delay),
                        error_message: error.to_string(),
                    })
                    .await;
                    tokio::select! {
                        biased;
                        () = cancellation.cancelled() => {
                            self.retrying.store(false, Ordering::Release);
                            sink.emit(RuntimeEvent::AutoRetryFinished {
                                success: false,
                                attempt,
                                final_error: Some("run cancelled".into()),
                            }).await;
                            return Err(MimirError::Protocol("run cancelled".into()));
                        }
                        () = retry_cancellation.cancelled() => {
                            self.retrying.store(false, Ordering::Release);
                            let message = "retry cancelled".to_owned();
                            sink.emit(RuntimeEvent::AutoRetryFinished {
                                success: false,
                                attempt,
                                final_error: Some(message.clone()),
                            }).await;
                            return Err(MimirError::Protocol(message));
                        }
                        () = tokio::time::sleep(delay) => {}
                    }
                }
                Err(error) => {
                    if attempt > 0 {
                        self.retrying.store(false, Ordering::Release);
                        sink.emit(RuntimeEvent::AutoRetryFinished {
                            success: false,
                            attempt,
                            final_error: Some(error.to_string()),
                        })
                        .await;
                    }
                    return Err(MimirError::Provider(error.to_string()));
                }
            }
        }
    }

    fn reset_cancellations(&self) {
        self.retrying.store(false, Ordering::Release);
        *self
            .cancellation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = CancellationToken::new();
        *self
            .retry_cancellation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = CancellationToken::new();
    }

    fn begin_control_operation(&self) -> CancellationToken {
        let cancellation = CancellationToken::new();
        *self
            .control_cancellation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = cancellation.clone();
        cancellation
    }
}

fn extension_identity_from_error(message: &str) -> String {
    message
        .split_once("extension '")
        .and_then(|(_, suffix)| suffix.split_once('\''))
        .map_or_else(|| "unknown".into(), |(name, _)| name.to_owned())
}

fn validate_extension_name(kind: &str, value: &str) -> Result<()> {
    let value = value.trim();
    if value.is_empty() || value.len() > 256 {
        return Err(MimirError::Protocol(format!(
            "extension {kind} must contain 1 to 256 bytes"
        )));
    }
    if value.chars().any(char::is_control) {
        return Err(MimirError::Protocol(format!(
            "extension {kind} must not contain control characters"
        )));
    }
    Ok(())
}

fn extension_content_text(content: &serde_json::Value) -> Result<String> {
    if let Some(text) = content.as_str() {
        if text.trim().is_empty() || text.len() > 64 * 1024 {
            return Err(MimirError::Protocol(
                "extension message text must contain 1 to 65536 bytes".into(),
            ));
        }
        return Ok(text.into());
    }
    let blocks: Vec<Content> = serde_json::from_value(content.clone()).map_err(|error| {
        MimirError::Protocol(format!("extension message content is invalid: {error}"))
    })?;
    let text = blocks
        .iter()
        .filter_map(|block| match block {
            Content::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    if text.trim().is_empty() || text.len() > 64 * 1024 {
        return Err(MimirError::Protocol(
            "extension message content must include bounded text".into(),
        ));
    }
    Ok(text)
}

fn extension_user_message(content: &serde_json::Value) -> Result<Message> {
    if content.is_string() {
        return extension_content_text(content).map(Message::user);
    }
    let blocks: Vec<Content> = serde_json::from_value(content.clone()).map_err(|error| {
        MimirError::Protocol(format!(
            "extension user message content is invalid: {error}"
        ))
    })?;
    if blocks.is_empty()
        || blocks.len() > 32
        || blocks
            .iter()
            .any(|block| !matches!(block, Content::Text { .. } | Content::Image { .. }))
    {
        return Err(MimirError::Protocol(
            "extension user message must contain 1 to 32 text/image blocks".into(),
        ));
    }
    if serde_json::to_vec(&blocks)?.len() > 256 * 1024 {
        return Err(MimirError::Protocol(
            "extension user message exceeds 256 KiB".into(),
        ));
    }
    Ok(Message::user_content(blocks))
}

fn normalize_thinking_levels(levels: &[ThinkingLevel]) -> Vec<ThinkingLevel> {
    let mut normalized = ThinkingLevel::ALL
        .into_iter()
        .filter(|level| levels.contains(level))
        .collect::<Vec<_>>();
    if normalized.is_empty() {
        normalized.push(ThinkingLevel::Off);
    }
    normalized
}

fn clamp_thinking_level(requested: ThinkingLevel, available: &[ThinkingLevel]) -> ThinkingLevel {
    if available.contains(&requested) {
        return requested;
    }
    let requested_index = ThinkingLevel::ALL
        .iter()
        .position(|level| *level == requested)
        .unwrap_or(0);
    ThinkingLevel::ALL[requested_index..]
        .iter()
        .chain(ThinkingLevel::ALL[..requested_index].iter().rev())
        .copied()
        .find(|level| available.contains(level))
        .unwrap_or(ThinkingLevel::Off)
}

async fn persist_model_selection(
    store: &dyn SessionStore,
    provider: &str,
    model: &str,
    thinking_level: ThinkingLevel,
) -> Result<()> {
    let detail = serde_json::to_string(&serde_json::json!({
        "provider": provider,
        "model": model,
        "thinkingLevel": thinking_level
    }))?;
    store
        .append(SessionRecord::new(SessionPayload::RuntimeEvent {
            name: "model_selection".into(),
            detail,
        }))
        .await
}

fn retry_delay(policy: RetryPolicy, attempt: u32) -> Duration {
    let exponent = attempt.saturating_sub(1).min(31);
    let multiplier = 1_u32 << exponent;
    policy
        .base_delay
        .saturating_mul(multiplier)
        .min(policy.max_delay)
}

fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn is_agent_message_prompt(prompt: &str) -> bool {
    if !prompt.starts_with(AGENT_MESSAGE_PREFIX) {
        return false;
    }
    let mut lines = prompt.split('\n');
    matches!(
        (
            lines.next(),
            lines.next(),
            lines.next(),
            lines.next(),
            lines.next(),
            lines.next(),
            lines.next(),
        ),
        (
            Some("Agent-to-agent message received."),
            Some("Source: agent_message"),
            Some(from),
            Some(to),
            Some(message_id),
            Some(""),
            Some(body),
        ) if from.starts_with("From: ")
            && from.len() > "From: ".len()
            && to.starts_with("To: ")
            && to.len() > "To: ".len()
            && message_id.starts_with("Message id: agentmsg_")
            && message_id.len() > "Message id: agentmsg_".len()
            && !body.trim().is_empty()
    )
}

fn agent_message_metadata(prompt: &str) -> serde_json::Value {
    let mut lines = prompt.lines();
    let _ = lines.next();
    let _ = lines.next();
    let from = lines
        .next()
        .and_then(|line| line.strip_prefix("From: "))
        .unwrap_or("unknown");
    let to = lines
        .next()
        .and_then(|line| line.strip_prefix("To: "))
        .unwrap_or("unknown");
    let message_id = lines
        .next()
        .and_then(|line| line.strip_prefix("Message id: "))
        .unwrap_or("unknown");
    serde_json::json!({
        "from": from,
        "to": to,
        "messageId": message_id,
        "deliveryStatus": "delivered"
    })
}

fn skill_invocation_error(error: &SkillInvocationError) -> MimirError {
    MimirError::Protocol(error.to_string())
}

async fn repair_message_integrity(
    store: &dyn SessionStore,
    messages: &mut Vec<Message>,
    reason: &str,
) -> Result<()> {
    let findings = session_integrity::inspect(messages);
    if findings.requires_pause() {
        return Err(MimirError::Protocol(format!(
            "session integrity check paused: unexpected results: {:?}; duplicate tool ids (calls: {:?}, results: {:?})",
            findings.unexpected_result_ids,
            findings.duplicate_call_ids,
            findings.duplicate_result_ids
        )));
    }
    if !findings.missing_calls.is_empty() {
        let synthetic =
            session_integrity::interrupted_tool_results(&findings.missing_calls, reason);
        store
            .append(SessionRecord::new(SessionPayload::Message(
                synthetic.clone(),
            )))
            .await?;
        messages.push(synthetic);
    }
    *messages = session_integrity::normalize(messages);
    Ok(())
}

fn retained_record_span(
    records: &[SessionRecord],
    first_retained: &Message,
) -> Result<(String, usize)> {
    let first_index = records
        .iter()
        .position(|record| {
            matches!(&record.payload, SessionPayload::Message(message) if message == first_retained)
        })
        .ok_or_else(|| {
            MimirError::Protocol(
                "First kept entry is unavailable — session may need migration".into(),
            )
        })?;
    let retained_message_count = records[first_index..]
        .iter()
        .filter(|record| matches!(record.payload, SessionPayload::Message(_)))
        .count();
    Ok((
        records[first_index].record_id.to_string(),
        retained_message_count,
    ))
}

fn normalize_request_messages(messages: &mut Vec<Message>) -> Result<()> {
    let findings = session_integrity::inspect(messages);
    if findings.requires_pause() {
        return Err(MimirError::Protocol(format!(
            "provider request paused by session integrity check: unexpected results: {:?}; duplicate tool ids (calls: {:?}, results: {:?})",
            findings.unexpected_result_ids,
            findings.duplicate_call_ids,
            findings.duplicate_result_ids
        )));
    }
    if !findings.missing_calls.is_empty() {
        messages.push(session_integrity::interrupted_tool_results(
            &findings.missing_calls,
            "context transformation omitted a required tool result",
        ));
    }
    *messages = session_integrity::normalize(messages);
    Ok(())
}

fn summarize(messages: &[Message]) -> String {
    let mut lines = vec!["Compacted conversation:".to_owned()];
    for message in messages.iter().rev().take(12).rev() {
        let role = match message.role {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        };
        let text: String = message_summary_text(message).chars().take(240).collect();
        if !text.is_empty() {
            lines.push(format!("- {role}: {text}"));
        }
    }
    lines.join("\n")
}

fn message_summary_text(message: &Message) -> String {
    message
        .content
        .iter()
        .map(|content| match content {
            Content::Text { text } | Content::Thinking { text, .. } => text.clone(),
            Content::Image { mime_type, .. } => format!("[{mime_type} image]"),
            Content::ToolCall(call) => format!("tool call {} {}", call.name, call.arguments),
            Content::ToolResult(result) => {
                if let Ok(observation) = serde_json::from_str::<ToolObservation>(&result.content) {
                    format!(
                        "tool result {}: {}\n{}",
                        result.tool_name, observation.summary, observation.content
                    )
                } else {
                    format!("tool result {}: {}", result.tool_name, result.content)
                }
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn estimate_message_tokens(messages: &[Message]) -> u64 {
    messages.iter().fold(0_u64, |total, message| {
        let characters = message
            .content
            .iter()
            .map(|content| match content {
                Content::Text { text } | Content::Thinking { text, .. } => text.chars().count(),
                Content::Image { data, mime_type } => {
                    data.chars().count() + mime_type.chars().count()
                }
                Content::ToolCall(call) => {
                    call.name.chars().count() + call.arguments.to_string().chars().count()
                }
                Content::ToolResult(result) => result.content.chars().count(),
            })
            .sum::<usize>();
        total.saturating_add(u64::try_from(characters.div_ceil(4) + 4).unwrap_or(u64::MAX))
    })
}

fn estimate_request_tokens(
    system_prompt: &str,
    messages: &[Message],
    tools: &[crate::model::ToolDefinition],
) -> u64 {
    const REQUEST_FRAMING_TOKENS: u64 = 256;
    const TOOL_FRAMING_TOKENS: u64 = 16;
    let system_tokens =
        u64::try_from(system_prompt.chars().count().div_ceil(4)).unwrap_or(u64::MAX);
    let tool_tokens = tools.iter().fold(0_u64, |total, tool| {
        let characters = tool
            .name
            .chars()
            .count()
            .saturating_add(tool.description.chars().count())
            .saturating_add(tool.parameters.to_string().chars().count());
        total.saturating_add(
            u64::try_from(characters.div_ceil(4))
                .unwrap_or(u64::MAX)
                .saturating_add(TOOL_FRAMING_TOKENS),
        )
    });
    REQUEST_FRAMING_TOKENS
        .saturating_add(system_tokens)
        .saturating_add(tool_tokens)
        .saturating_add(estimate_message_tokens(messages))
}

fn auto_compaction_split(
    messages: &[Message],
    retained_message_target: usize,
    retained_token_target: u64,
) -> Option<usize> {
    if messages.len() < 2 {
        return None;
    }
    let count_split = messages
        .len()
        .saturating_sub(retained_message_target.max(1));
    let mut retained_tokens = 0_u64;
    let mut token_split = 0;
    for index in (0..messages.len()).rev() {
        retained_tokens =
            retained_tokens.saturating_add(estimate_message_tokens(&messages[index..=index]));
        if retained_tokens > retained_token_target {
            token_split = index.saturating_add(1);
            break;
        }
    }
    let desired = count_split.max(token_split).clamp(1, messages.len() - 1);
    (desired..messages.len())
        .find(|split| safe_compaction_split(messages, *split))
        .or_else(|| {
            (1..desired)
                .rev()
                .find(|split| safe_compaction_split(messages, *split))
        })
}

fn safe_compaction_split(messages: &[Message], split: usize) -> bool {
    if split == 0 || split >= messages.len() || messages[split].role == Role::Tool {
        return false;
    }
    let calls_before = messages[..split]
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|content| match content {
            Content::ToolCall(call) => Some(call.id.as_str()),
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    !messages[split..]
        .iter()
        .flat_map(|message| &message.content)
        .any(|content| {
            matches!(content, Content::ToolResult(result) if calls_before.contains(result.tool_call_id.as_str()))
        })
}

fn prepare_context_messages(messages: &[Message]) -> Vec<Message> {
    bound_tool_results_for_context(session_integrity::normalize(messages))
}

fn bound_tool_results_for_context(mut messages: Vec<Message>) -> Vec<Message> {
    const MAX_TOOL_RESULT_CONTEXT_BYTES: usize = 48 * 1024;
    const TRUNCATION_NOTE: &str =
        "\n[tool result truncated for active context; full result remains in session history]";
    for message in &mut messages {
        for content in &mut message.content {
            let Content::ToolResult(result) = content else {
                continue;
            };
            if result.content.len() <= MAX_TOOL_RESULT_CONTEXT_BYTES {
                continue;
            }
            if let Ok(mut observation) = serde_json::from_str::<ToolObservation>(&result.content) {
                truncate_string_bytes(
                    &mut observation.content,
                    MAX_TOOL_RESULT_CONTEXT_BYTES.saturating_sub(TRUNCATION_NOTE.len()),
                );
                observation.content.push_str(TRUNCATION_NOTE);
                observation.next_actions.push(
                    "Use an artifact path or a narrower follow-up read for omitted output".into(),
                );
                if let Ok(encoded) = serde_json::to_string(&observation) {
                    result.content = encoded;
                    continue;
                }
            }
            truncate_string_bytes(
                &mut result.content,
                MAX_TOOL_RESULT_CONTEXT_BYTES.saturating_sub(TRUNCATION_NOTE.len()),
            );
            result.content.push_str(TRUNCATION_NOTE);
        }
    }
    messages
}

fn truncate_string_bytes(value: &mut String, limit: usize) {
    if value.len() <= limit {
        return;
    }
    let mut end = limit.min(value.len());
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
}

fn manual_compaction_split(messages: &[Message], keep_recent_tokens: u64) -> Option<usize> {
    let valid_cut_points = messages
        .iter()
        .enumerate()
        .filter(|(index, _)| safe_compaction_split(messages, *index))
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    if valid_cut_points.is_empty() {
        return None;
    }
    let mut accumulated = 0_u64;
    let mut cut_index = valid_cut_points[0];
    let mut reached_budget = false;
    for index in (0..messages.len()).rev() {
        accumulated = accumulated.saturating_add(estimate_message_tokens(&messages[index..=index]));
        if accumulated >= keep_recent_tokens {
            cut_index = valid_cut_points
                .iter()
                .copied()
                .find(|candidate| *candidate >= index)
                .unwrap_or(cut_index);
            reached_budget = true;
            break;
        }
    }
    (reached_budget && cut_index > 0).then_some(cut_index)
}

fn compaction_prompt(messages: &[Message], custom_instructions: Option<&str>) -> String {
    use std::fmt::Write as _;

    let mut output = String::from("<conversation>\n");
    for message in messages {
        let role = match message.role {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        };
        writeln!(output, "<{role}>\n{}\n</{role}>", message.text())
            .expect("writing to a String cannot fail");
        if output.len() >= 200_000 {
            output.truncate(200_000);
            output.push_str("\n[conversation truncated at 200000 bytes]\n");
            break;
        }
    }
    output.push_str("</conversation>");
    if let Some(instructions) = custom_instructions.filter(|value| !value.trim().is_empty()) {
        output.push_str("\n\n<custom_instructions>\n");
        output.push_str(instructions.trim());
        output.push_str("\n</custom_instructions>");
    }
    output
}

fn extract_file_operations(messages: &[Message], records: &[SessionRecord]) -> CompactionDetails {
    let mut read_files = std::collections::BTreeSet::new();
    let mut modified_files = std::collections::BTreeSet::new();
    for record in records {
        let SessionPayload::Compaction {
            details: Some(details),
            ..
        } = &record.payload
        else {
            continue;
        };
        for path in details
            .get("readFiles")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(serde_json::Value::as_str)
        {
            read_files.insert(path.to_owned());
        }
        for path in details
            .get("modifiedFiles")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(serde_json::Value::as_str)
        {
            modified_files.insert(path.to_owned());
        }
    }
    for call in messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|content| {
            if let Content::ToolCall(call) = content {
                Some(call)
            } else {
                None
            }
        })
    {
        let Some(path) = call
            .arguments
            .get("path")
            .and_then(serde_json::Value::as_str)
        else {
            continue;
        };
        match call.name.as_str() {
            "read_file" => {
                read_files.insert(path.to_owned());
            }
            "write_file" | "edit_file" => {
                modified_files.insert(path.to_owned());
            }
            _ => {}
        }
    }
    CompactionDetails {
        read_files: read_files.into_iter().collect(),
        modified_files: modified_files.into_iter().collect(),
    }
}

fn append_file_operations(summary: &mut String, details: &CompactionDetails) {
    use std::fmt::Write as _;

    if !details.read_files.is_empty() {
        summary.push_str("\n\n## Files Read\n");
        for path in &details.read_files {
            writeln!(summary, "- `{path}`").expect("writing to a String cannot fail");
        }
    }
    if !details.modified_files.is_empty() {
        summary.push_str("\n\n## Files Modified\n");
        for path in &details.modified_files {
            writeln!(summary, "- `{path}`").expect("writing to a String cannot fail");
        }
    }
}

const fn runtime_role_name(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}
