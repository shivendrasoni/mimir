#![allow(
    clippy::missing_errors_doc,
    reason = "extension ABI operations return typed protocol, capability, and persistence errors"
)]

use std::{
    collections::{BTreeSet, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
};

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Semaphore;

use crate::{
    atomic,
    auth::OAuthCredential,
    error::{MimirError, Result},
    model::{Message, ModelRequest, ModelResponse, Role, ThinkingLevel, ToolCall, ToolResult},
};

use super::{
    Capability, EmbeddedJsExtensionHost, ExtensionEntrypoint, ExtensionHostAction,
    ExtensionHostSnapshot, ExtensionManifest, FlagDescriptor, HostLimits, HostRequest,
    HostResponseStatus, JsonLineExtensionHost, ShortcutDescriptor,
};

const ABI_VERSION: u16 = 1;
const RUNTIME_STATE_VERSION: u16 = 1;

#[derive(Debug, Clone, Copy)]
pub struct RuntimeLimits {
    pub host: HostLimits,
    pub max_concurrency: usize,
    pub max_registrations: usize,
}

impl Default for RuntimeLimits {
    fn default() -> Self {
        Self {
            host: HostLimits::default(),
            max_concurrency: 4,
            max_registrations: 128,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolDescriptor {
    pub name: String,
    pub label: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandDescriptor {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub supports_argument_completions: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiRequestKind {
    Notify,
    Input,
    Confirm,
    Select,
    SetStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererDescriptor {
    pub custom_type: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderTransport {
    OpenAiCompatible,
    AnthropicMessages,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ProviderApi {
    #[default]
    OpenAiCompletions,
    OpenAiResponses,
    AnthropicMessages,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderDescriptor {
    pub name: String,
    pub transport: ProviderTransport,
    pub models: Vec<String>,
    #[serde(default)]
    pub api: ProviderApi,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub credential_env: Option<String>,
    #[serde(default)]
    pub oauth_name: Option<String>,
    #[serde(default)]
    pub custom_stream: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExtensionOAuthCredential {
    access: String,
    refresh: String,
    #[serde(rename = "expires")]
    expires_at_ms: u64,
}

impl From<OAuthCredential> for ExtensionOAuthCredential {
    fn from(value: OAuthCredential) -> Self {
        Self {
            access: value.access,
            refresh: value.refresh,
            expires_at_ms: value.expires_at_ms,
        }
    }
}

impl From<ExtensionOAuthCredential> for OAuthCredential {
    fn from(value: ExtensionOAuthCredential) -> Self {
        Self {
            access: value.access,
            refresh: value.refresh,
            expires_at_ms: value.expires_at_ms,
            account_id: None,
            enterprise_url: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExtensionProviderEvent {
    TextDelta { text: String },
    ThinkingDelta { text: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionProviderStreamResult {
    pub response: ModelResponse,
    #[serde(default)]
    pub events: Vec<ExtensionProviderEvent>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ExtensionRegistrations {
    pub tools: Vec<ToolDescriptor>,
    pub commands: Vec<CommandDescriptor>,
    pub shortcuts: Vec<ShortcutDescriptor>,
    pub flags: Vec<FlagDescriptor>,
    pub ui_requests: BTreeSet<UiRequestKind>,
    pub renderers: Vec<RendererDescriptor>,
    pub providers: Vec<ProviderDescriptor>,
    pub lifecycle_events: BTreeSet<LifecycleEventKind>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleEventKind {
    ResourcesDiscover,
    SessionStart,
    SessionBeforeSwitch,
    SessionBeforeFork,
    SessionBeforeCompact,
    SessionCompact,
    SessionShutdown,
    SessionBeforeTree,
    SessionTree,
    BeforeProviderRequest,
    AfterProviderResponse,
    AgentStart,
    AgentEnd,
    Context,
    BeforeAgentStart,
    TurnStart,
    TurnEnd,
    MessageStart,
    MessageUpdate,
    MessageEnd,
    ModelSelect,
    ThinkingLevelSelect,
    ToolCall,
    ToolResult,
    UserBash,
    Input,
    RefineComplete,
    ToolExecutionStart,
    ToolExecutionUpdate,
    ToolExecutionEnd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStartReason {
    Startup,
    Reload,
    New,
    Resume,
    Fork,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionSwitchReason {
    New,
    Resume,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceDiscoveryReason {
    Startup,
    Reload,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionForkPosition {
    Before,
    At,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum LifecycleEvent {
    ResourcesDiscover {
        cwd: String,
        reason: ResourceDiscoveryReason,
    },
    SessionStart {
        session_id: String,
        reason: SessionStartReason,
        #[serde(default)]
        previous_session_file: Option<String>,
    },
    SessionBeforeSwitch {
        session_id: String,
        reason: SessionSwitchReason,
        #[serde(default)]
        target_session_file: Option<String>,
    },
    SessionBeforeFork {
        session_id: String,
        entry_id: String,
        position: SessionForkPosition,
    },
    SessionBeforeCompact {
        session_id: String,
        context_tokens: u64,
    },
    SessionCompact {
        session_id: String,
    },
    SessionShutdown {
        session_id: String,
        reason: String,
    },
    SessionBeforeTree {
        session_id: String,
        target_id: String,
    },
    SessionTree {
        session_id: String,
        target_id: String,
    },
    BeforeProviderRequest {
        session_id: String,
        provider: String,
        model: String,
    },
    AfterProviderResponse {
        session_id: String,
        provider: String,
        model: String,
        success: bool,
    },
    AgentStart {
        session_id: String,
    },
    AgentEnd {
        session_id: String,
        success: bool,
    },
    Context {
        session_id: String,
        messages: Vec<Message>,
    },
    BeforeAgentStart {
        session_id: String,
        #[serde(default)]
        parent_session_id: Option<String>,
    },
    TurnStart {
        session_id: String,
        turn_index: u64,
    },
    TurnEnd {
        session_id: String,
        turn_index: u64,
        #[serde(default)]
        stop_reason: Option<String>,
    },
    MessageStart {
        session_id: String,
        message_id: String,
        role: String,
    },
    MessageUpdate {
        session_id: String,
        message_id: String,
        role: String,
    },
    MessageEnd {
        session_id: String,
        message_id: String,
        role: String,
        message: Message,
    },
    ModelSelect {
        session_id: String,
        provider: String,
        model: String,
    },
    ThinkingLevelSelect {
        session_id: String,
        thinking_level: ThinkingLevel,
    },
    ToolCall {
        session_id: String,
        tool_call: ToolCall,
    },
    ToolResult {
        session_id: String,
        tool_result: ToolResult,
    },
    UserBash {
        session_id: String,
        command: String,
        #[serde(default)]
        cwd: Option<String>,
    },
    Input {
        session_id: String,
        message: Message,
    },
    RefineComplete {
        session_id: String,
        result: Value,
    },
    ToolExecutionStart {
        session_id: String,
        tool_call_id: String,
        tool_name: String,
    },
    ToolExecutionUpdate {
        session_id: String,
        tool_call_id: String,
        tool_name: String,
    },
    ToolExecutionEnd {
        session_id: String,
        tool_call_id: String,
        tool_name: String,
        is_error: bool,
    },
}

impl LifecycleEvent {
    pub fn kind(&self) -> LifecycleEventKind {
        match self {
            Self::ResourcesDiscover { .. } => LifecycleEventKind::ResourcesDiscover,
            Self::SessionStart { .. } => LifecycleEventKind::SessionStart,
            Self::SessionBeforeSwitch { .. } => LifecycleEventKind::SessionBeforeSwitch,
            Self::SessionBeforeFork { .. } => LifecycleEventKind::SessionBeforeFork,
            Self::SessionBeforeCompact { .. } => LifecycleEventKind::SessionBeforeCompact,
            Self::SessionCompact { .. } => LifecycleEventKind::SessionCompact,
            Self::SessionShutdown { .. } => LifecycleEventKind::SessionShutdown,
            Self::SessionBeforeTree { .. } => LifecycleEventKind::SessionBeforeTree,
            Self::SessionTree { .. } => LifecycleEventKind::SessionTree,
            Self::BeforeProviderRequest { .. } => LifecycleEventKind::BeforeProviderRequest,
            Self::AfterProviderResponse { .. } => LifecycleEventKind::AfterProviderResponse,
            Self::AgentStart { .. } => LifecycleEventKind::AgentStart,
            Self::AgentEnd { .. } => LifecycleEventKind::AgentEnd,
            Self::Context { .. } => LifecycleEventKind::Context,
            Self::BeforeAgentStart { .. } => LifecycleEventKind::BeforeAgentStart,
            Self::TurnStart { .. } => LifecycleEventKind::TurnStart,
            Self::TurnEnd { .. } => LifecycleEventKind::TurnEnd,
            Self::MessageStart { .. } => LifecycleEventKind::MessageStart,
            Self::MessageUpdate { .. } => LifecycleEventKind::MessageUpdate,
            Self::MessageEnd { .. } => LifecycleEventKind::MessageEnd,
            Self::ModelSelect { .. } => LifecycleEventKind::ModelSelect,
            Self::ThinkingLevelSelect { .. } => LifecycleEventKind::ThinkingLevelSelect,
            Self::ToolCall { .. } => LifecycleEventKind::ToolCall,
            Self::ToolResult { .. } => LifecycleEventKind::ToolResult,
            Self::UserBash { .. } => LifecycleEventKind::UserBash,
            Self::Input { .. } => LifecycleEventKind::Input,
            Self::RefineComplete { .. } => LifecycleEventKind::RefineComplete,
            Self::ToolExecutionStart { .. } => LifecycleEventKind::ToolExecutionStart,
            Self::ToolExecutionUpdate { .. } => LifecycleEventKind::ToolExecutionUpdate,
            Self::ToolExecutionEnd { .. } => LifecycleEventKind::ToolExecutionEnd,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum UiRequest {
    Notify {
        level: String,
        message: String,
    },
    Input {
        id: String,
        prompt: String,
        #[serde(default)]
        placeholder: Option<String>,
    },
    Confirm {
        id: String,
        title: String,
        message: String,
    },
    Select {
        id: String,
        title: String,
        options: Vec<String>,
    },
    SetStatus {
        key: String,
        #[serde(default)]
        text: Option<String>,
    },
}

impl UiRequest {
    fn kind(&self) -> UiRequestKind {
        match self {
            Self::Notify { .. } => UiRequestKind::Notify,
            Self::Input { .. } => UiRequestKind::Input,
            Self::Confirm { .. } => UiRequestKind::Confirm,
            Self::Select { .. } => UiRequestKind::Select,
            Self::SetStatus { .. } => UiRequestKind::SetStatus,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum LifecycleMutation {
    Context {
        messages: Vec<Message>,
    },
    ModelSelect {
        provider: String,
        model: String,
    },
    ThinkingLevelSelect {
        thinking_level: ThinkingLevel,
    },
    ToolCall {
        tool_call: ToolCall,
    },
    ToolResult {
        tool_result: ToolResult,
    },
    UserBash {
        command: String,
        #[serde(default)]
        cwd: Option<String>,
    },
    Input {
        message: Message,
    },
    RefineComplete {
        result: Value,
    },
    MessageEnd {
        message: Message,
    },
}

impl LifecycleMutation {
    fn kind(&self) -> LifecycleEventKind {
        match self {
            Self::Context { .. } => LifecycleEventKind::Context,
            Self::ModelSelect { .. } => LifecycleEventKind::ModelSelect,
            Self::ThinkingLevelSelect { .. } => LifecycleEventKind::ThinkingLevelSelect,
            Self::ToolCall { .. } => LifecycleEventKind::ToolCall,
            Self::ToolResult { .. } => LifecycleEventKind::ToolResult,
            Self::UserBash { .. } => LifecycleEventKind::UserBash,
            Self::Input { .. } => LifecycleEventKind::Input,
            Self::RefineComplete { .. } => LifecycleEventKind::RefineComplete,
            Self::MessageEnd { .. } => LifecycleEventKind::MessageEnd,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum LifecycleReplacement {
    Context {
        messages: Vec<Message>,
    },
    ModelSelect {
        provider: String,
        model: String,
    },
    ThinkingLevelSelect {
        thinking_level: ThinkingLevel,
    },
    ToolCall {
        tool_call: ToolCall,
    },
    ToolResult {
        tool_result: ToolResult,
    },
    UserBash {
        command: String,
        #[serde(default)]
        cwd: Option<String>,
    },
    Input {
        message: Message,
    },
    RefineComplete {
        result: Value,
    },
    MessageEnd {
        message: Message,
    },
}

impl LifecycleReplacement {
    fn kind(&self) -> LifecycleEventKind {
        match self {
            Self::Context { .. } => LifecycleEventKind::Context,
            Self::ModelSelect { .. } => LifecycleEventKind::ModelSelect,
            Self::ThinkingLevelSelect { .. } => LifecycleEventKind::ThinkingLevelSelect,
            Self::ToolCall { .. } => LifecycleEventKind::ToolCall,
            Self::ToolResult { .. } => LifecycleEventKind::ToolResult,
            Self::UserBash { .. } => LifecycleEventKind::UserBash,
            Self::Input { .. } => LifecycleEventKind::Input,
            Self::RefineComplete { .. } => LifecycleEventKind::RefineComplete,
            Self::MessageEnd { .. } => LifecycleEventKind::MessageEnd,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum LifecycleInterception {
    #[default]
    Continue,
    Block {
        #[serde(default)]
        reason: Option<String>,
    },
    Mutate {
        mutation: LifecycleMutation,
    },
    Replace {
        replacement: LifecycleReplacement,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LifecycleOutcome {
    pub cancel: bool,
    pub output: Value,
    pub ui_requests: Vec<UiRequest>,
    pub interception: LifecycleInterception,
    pub actions: Vec<ExtensionHostAction>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionCallStatus {
    Ok,
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolInvocationResult {
    pub status: ExtensionCallStatus,
    pub summary: String,
    pub content: Value,
    #[serde(default)]
    pub next_actions: Vec<String>,
    #[serde(default)]
    pub ui_requests: Vec<UiRequest>,
    #[serde(default)]
    pub actions: Vec<ExtensionHostAction>,
    #[serde(default)]
    pub render_call: Option<RenderOutput>,
    #[serde(default)]
    pub render_result: Option<RenderOutput>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandInvocationResult {
    #[serde(default)]
    pub message: Option<String>,
    pub output: Value,
    #[serde(default)]
    pub ui_requests: Vec<UiRequest>,
    #[serde(default)]
    pub actions: Vec<ExtensionHostAction>,
}

#[derive(Debug, Clone, Default)]
pub struct UiResponseContinuation {
    pub command_result: Option<CommandInvocationResult>,
    pub next_request: Option<UiRequest>,
    pub oauth_credential: Option<OAuthCredential>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandArgumentCompletion {
    pub value: String,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CommandArgumentCompletions {
    pub items: Vec<CommandArgumentCompletion>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenderOutput {
    pub lines: Vec<String>,
}

impl Default for LifecycleOutcome {
    fn default() -> Self {
        Self {
            cancel: false,
            output: Value::Null,
            ui_requests: Vec::new(),
            interception: LifecycleInterception::Continue,
            actions: Vec::new(),
        }
    }
}

#[derive(Debug, Serialize)]
struct AbiEnvelope<T> {
    abi_version: u16,
    generation: u64,
    request: T,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AbiRequest {
    Initialize {
        extension_name: String,
        extension_version: String,
        capabilities: BTreeSet<Capability>,
    },
    Lifecycle {
        event: LifecycleEvent,
        #[serde(default)]
        host: ExtensionHostSnapshot,
    },
    Tool {
        name: String,
        tool_call_id: String,
        input: Value,
        #[serde(default)]
        host: ExtensionHostSnapshot,
    },
    Command {
        name: String,
        args: String,
        #[serde(default)]
        host: ExtensionHostSnapshot,
    },
    CommandArgumentCompletions {
        name: String,
        args: String,
        #[serde(default)]
        host: ExtensionHostSnapshot,
    },
    Shortcut {
        shortcut: String,
        #[serde(default)]
        host: ExtensionHostSnapshot,
    },
    Render {
        custom_type: String,
        message: Value,
        expanded: bool,
    },
    UiResponse {
        request_id: String,
        response: Value,
    },
    BusEvent {
        topic: String,
        data: Value,
    },
    #[serde(rename = "provider_oauth_login")]
    ProviderOAuthLogin {
        name: String,
    },
    #[serde(rename = "provider_oauth_refresh")]
    ProviderOAuthRefresh {
        name: String,
        credential: ExtensionOAuthCredential,
    },
    #[serde(rename = "provider_oauth_get_api_key")]
    ProviderOAuthGetApiKey {
        name: String,
        credential: ExtensionOAuthCredential,
    },
    ProviderStream {
        name: String,
        model: String,
        credential: String,
        request: ModelRequest,
    },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AbiResponseEnvelope {
    abi_version: u16,
    generation: u64,
    response: AbiResponse,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum AbiResponse {
    Registration {
        registrations: ExtensionRegistrations,
    },
    Lifecycle {
        outcome: LifecycleOutcome,
    },
    Tool {
        result: ToolInvocationResult,
    },
    Command {
        result: CommandInvocationResult,
    },
    CommandArgumentCompletions {
        result: CommandArgumentCompletions,
    },
    Render {
        output: RenderOutput,
    },
    UiResponse {
        accepted: bool,
    },
    BusEvent {
        delivered: bool,
    },
    Suspended {
        request: UiRequest,
    },
    #[serde(rename = "provider_oauth")]
    ProviderOAuth {
        credential: ExtensionOAuthCredential,
    },
    ProviderApiKey {
        api_key: String,
    },
    ProviderStream {
        result: ExtensionProviderStreamResult,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimeState {
    schema_version: u16,
    generation: u64,
    extension_version: String,
}

#[derive(Debug)]
enum ExtensionBackend {
    Native(JsonLineExtensionHost),
    Embedded(EmbeddedJsExtensionHost),
}

#[derive(Debug)]
pub struct ExtensionRuntime {
    manifest: ExtensionManifest,
    host: ExtensionBackend,
    state_root: PathBuf,
    state_path: PathBuf,
    generation: u64,
    registrations: ExtensionRegistrations,
    semaphore: Arc<Semaphore>,
}

impl ExtensionRuntime {
    pub async fn load(
        manifest: ExtensionManifest,
        workspace_root: &Path,
        state_root: &Path,
        limits: RuntimeLimits,
        request_id: &str,
    ) -> Result<Self> {
        validate_limits(limits)?;
        manifest.validate()?;
        let is_native = matches!(
            manifest.entrypoint,
            ExtensionEntrypoint::NativeProcess { .. }
        );
        if is_native
            && (!manifest.capabilities.contains(&Capability::Process)
                || !manifest
                    .capabilities
                    .contains(&Capability::UnrestrictedNative))
        {
            return Err(MimirError::Configuration(format!(
                "extension '{}' must declare process and unrestricted_native capabilities to use the unsandboxed native subprocess ABI",
                manifest.name
            )));
        }
        let state_root = atomic::canonical_state_root(state_root);
        let state_path = state_root
            .join("extensions/runtime")
            .join(format!("{}.json", manifest.name));
        let (previous_generation, generation) = next_generation(&state_root, &state_path).await?;
        let host = match &manifest.entrypoint {
            ExtensionEntrypoint::NativeProcess { .. } => ExtensionBackend::Native(
                JsonLineExtensionHost::new(manifest.clone(), workspace_root, limits.host)?,
            ),
            ExtensionEntrypoint::EmbeddedJavaScript { .. } => ExtensionBackend::Embedded(
                EmbeddedJsExtensionHost::new(&manifest, workspace_root, limits.host)?,
            ),
        };
        let response = invoke_abi(
            &host,
            request_id,
            generation,
            AbiRequest::Initialize {
                extension_name: manifest.name.clone(),
                extension_version: manifest.version.clone(),
                capabilities: manifest.capabilities.clone(),
            },
        )
        .await?;
        let registrations = match response {
            AbiResponse::Registration { registrations } => registrations,
            AbiResponse::Lifecycle { .. } => {
                return Err(MimirError::Protocol(
                    "extension returned a lifecycle response during initialization".into(),
                ));
            }
            AbiResponse::Tool { .. }
            | AbiResponse::Command { .. }
            | AbiResponse::CommandArgumentCompletions { .. }
            | AbiResponse::Render { .. }
            | AbiResponse::UiResponse { .. }
            | AbiResponse::BusEvent { .. }
            | AbiResponse::Suspended { .. }
            | AbiResponse::ProviderOAuth { .. }
            | AbiResponse::ProviderApiKey { .. }
            | AbiResponse::ProviderStream { .. } => {
                return Err(MimirError::Protocol(
                    "extension returned an invocation response during initialization".into(),
                ));
            }
        };
        validate_registrations(&manifest, &registrations, limits.max_registrations)?;
        commit_generation(
            &state_root,
            &state_path,
            previous_generation,
            generation,
            &manifest.version,
        )
        .await?;
        Ok(Self {
            manifest,
            host,
            state_root,
            state_path,
            generation,
            registrations,
            semaphore: Arc::new(Semaphore::new(limits.max_concurrency)),
        })
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn name(&self) -> &str {
        &self.manifest.name
    }

    pub fn registrations(&self) -> &ExtensionRegistrations {
        &self.registrations
    }

    pub async fn dispatch(
        &self,
        request_id: &str,
        event: LifecycleEvent,
    ) -> Result<Option<LifecycleOutcome>> {
        self.dispatch_with_snapshot(request_id, event, ExtensionHostSnapshot::default())
            .await
    }

    pub async fn dispatch_with_snapshot(
        &self,
        request_id: &str,
        event: LifecycleEvent,
        host: ExtensionHostSnapshot,
    ) -> Result<Option<LifecycleOutcome>> {
        let event_kind = event.kind();
        if !self.registrations.lifecycle_events.contains(&event_kind) {
            return Ok(None);
        }
        let _permit = Arc::clone(&self.semaphore)
            .try_acquire_owned()
            .map_err(|_| {
                MimirError::Protocol(format!(
                    "extension '{}' reached its concurrency limit",
                    self.manifest.name
                ))
            })?;
        assert_generation_current(&self.state_path, self.generation).await?;
        let response = invoke_abi(
            &self.host,
            request_id,
            self.generation,
            AbiRequest::Lifecycle { event, host },
        )
        .await?;
        assert_generation_current(&self.state_path, self.generation).await?;
        let outcome = match response {
            AbiResponse::Lifecycle { outcome } => outcome,
            AbiResponse::Registration { .. }
            | AbiResponse::Tool { .. }
            | AbiResponse::Command { .. }
            | AbiResponse::CommandArgumentCompletions { .. }
            | AbiResponse::Render { .. }
            | AbiResponse::UiResponse { .. }
            | AbiResponse::BusEvent { .. }
            | AbiResponse::Suspended { .. }
            | AbiResponse::ProviderOAuth { .. }
            | AbiResponse::ProviderApiKey { .. }
            | AbiResponse::ProviderStream { .. } => {
                return Err(MimirError::Protocol(
                    "extension returned a registration response during lifecycle dispatch".into(),
                ));
            }
        };
        validate_outcome(&self.manifest, &self.registrations, event_kind, &outcome)?;
        Ok(Some(outcome))
    }

    pub async fn invoke_tool(
        &self,
        request_id: &str,
        name: &str,
        tool_call_id: &str,
        input: Value,
    ) -> Result<ToolInvocationResult> {
        self.invoke_tool_with_snapshot(
            request_id,
            name,
            tool_call_id,
            input,
            ExtensionHostSnapshot::default(),
        )
        .await
    }

    pub async fn invoke_tool_with_snapshot(
        &self,
        request_id: &str,
        name: &str,
        tool_call_id: &str,
        input: Value,
        host: ExtensionHostSnapshot,
    ) -> Result<ToolInvocationResult> {
        if !self
            .registrations
            .tools
            .iter()
            .any(|tool| tool.name == name)
        {
            return Err(MimirError::Protocol(format!(
                "extension '{}' did not register tool '{name}'",
                self.manifest.name
            )));
        }
        let response = self
            .invoke_guarded(
                request_id,
                AbiRequest::Tool {
                    name: name.to_owned(),
                    tool_call_id: tool_call_id.to_owned(),
                    input,
                    host,
                },
            )
            .await?;
        let AbiResponse::Tool { result } = response else {
            return Err(MimirError::Protocol(
                "extension returned the wrong response for a tool invocation".into(),
            ));
        };
        validate_ui_requests(&self.manifest, &self.registrations, &result.ui_requests)?;
        validate_actions(&self.manifest, &result.actions)?;
        validate_text("tool summary", &result.summary)?;
        if let Some(output) = &result.render_call {
            validate_render_output(output, "extension tool call renderer")?;
        }
        if let Some(output) = &result.render_result {
            validate_render_output(output, "extension tool result renderer")?;
        }
        Ok(result)
    }

    pub async fn invoke_command(
        &self,
        request_id: &str,
        name: &str,
        args: &str,
    ) -> Result<CommandInvocationResult> {
        self.invoke_command_with_snapshot(request_id, name, args, ExtensionHostSnapshot::default())
            .await
    }

    pub async fn invoke_command_with_snapshot(
        &self,
        request_id: &str,
        name: &str,
        args: &str,
        host: ExtensionHostSnapshot,
    ) -> Result<CommandInvocationResult> {
        if !self
            .registrations
            .commands
            .iter()
            .any(|command| command.name == name)
        {
            return Err(MimirError::Protocol(format!(
                "extension '{}' did not register command '{name}'",
                self.manifest.name
            )));
        }
        validate_text("command arguments", args)
            .or_else(|error| if args.is_empty() { Ok(()) } else { Err(error) })?;
        let response = self
            .invoke_guarded(
                request_id,
                AbiRequest::Command {
                    name: name.to_owned(),
                    args: args.to_owned(),
                    host,
                },
            )
            .await?;
        let result = match response {
            AbiResponse::Command { result } => result,
            AbiResponse::Suspended { request } => CommandInvocationResult {
                message: None,
                output: Value::Null,
                ui_requests: vec![request],
                actions: Vec::new(),
            },
            _ => {
                return Err(MimirError::Protocol(
                    "extension returned the wrong response for a command invocation".into(),
                ));
            }
        };
        validate_ui_requests(&self.manifest, &self.registrations, &result.ui_requests)?;
        validate_actions(&self.manifest, &result.actions)?;
        Ok(result)
    }

    pub async fn invoke_shortcut_with_snapshot(
        &self,
        request_id: &str,
        shortcut: &str,
        host: ExtensionHostSnapshot,
    ) -> Result<CommandInvocationResult> {
        if !self
            .registrations
            .shortcuts
            .iter()
            .any(|registered| registered.shortcut == shortcut)
        {
            return Err(MimirError::Protocol(format!(
                "extension '{}' did not register shortcut '{shortcut}'",
                self.manifest.name
            )));
        }
        let response = self
            .invoke_guarded(
                request_id,
                AbiRequest::Shortcut {
                    shortcut: shortcut.to_owned(),
                    host,
                },
            )
            .await?;
        let result = match response {
            AbiResponse::Command { result } => result,
            AbiResponse::Suspended { request } => CommandInvocationResult {
                message: None,
                output: Value::Null,
                ui_requests: vec![request],
                actions: Vec::new(),
            },
            _ => {
                return Err(MimirError::Protocol(
                    "extension returned the wrong response for a shortcut invocation".into(),
                ));
            }
        };
        validate_ui_requests(&self.manifest, &self.registrations, &result.ui_requests)?;
        validate_actions(&self.manifest, &result.actions)?;
        Ok(result)
    }

    pub async fn get_command_argument_completions(
        &self,
        request_id: &str,
        name: &str,
        args: &str,
    ) -> Result<CommandArgumentCompletions> {
        self.get_command_argument_completions_with_snapshot(
            request_id,
            name,
            args,
            ExtensionHostSnapshot::default(),
        )
        .await
    }

    pub async fn get_command_argument_completions_with_snapshot(
        &self,
        request_id: &str,
        name: &str,
        args: &str,
        host: ExtensionHostSnapshot,
    ) -> Result<CommandArgumentCompletions> {
        let Some(command) = self
            .registrations
            .commands
            .iter()
            .find(|command| command.name == name)
        else {
            return Err(MimirError::Protocol(format!(
                "extension '{}' did not register command '{name}'",
                self.manifest.name
            )));
        };
        if !command.supports_argument_completions {
            return Err(MimirError::Protocol(format!(
                "extension command '{name}' does not support argument completions"
            )));
        }
        validate_text("command arguments", args)
            .or_else(|error| if args.is_empty() { Ok(()) } else { Err(error) })?;
        let response = self
            .invoke_guarded(
                request_id,
                AbiRequest::CommandArgumentCompletions {
                    name: name.to_owned(),
                    args: args.to_owned(),
                    host,
                },
            )
            .await?;
        let AbiResponse::CommandArgumentCompletions { result } = response else {
            return Err(MimirError::Protocol(
                "extension returned the wrong response for command argument completions".into(),
            ));
        };
        validate_command_argument_completions(&result)?;
        Ok(result)
    }

    pub async fn render_message(
        &self,
        request_id: &str,
        custom_type: &str,
        message: Value,
        expanded: bool,
    ) -> Result<RenderOutput> {
        if !self
            .registrations
            .renderers
            .iter()
            .any(|renderer| renderer.custom_type == custom_type)
        {
            return Err(MimirError::Protocol(format!(
                "extension '{}' did not register renderer '{custom_type}'",
                self.manifest.name
            )));
        }
        let response = self
            .invoke_guarded(
                request_id,
                AbiRequest::Render {
                    custom_type: custom_type.to_owned(),
                    message,
                    expanded,
                },
            )
            .await?;
        let AbiResponse::Render { output } = response else {
            return Err(MimirError::Protocol(
                "extension returned the wrong response for a render invocation".into(),
            ));
        };
        validate_render_output(&output, "extension renderer")?;
        Ok(output)
    }

    pub async fn begin_provider_oauth_login(&self, name: &str) -> Result<UiResponseContinuation> {
        self.require_provider_feature(name, true, false)?;
        let response = self
            .invoke_guarded(
                &uuid::Uuid::new_v4().to_string(),
                AbiRequest::ProviderOAuthLogin {
                    name: name.to_owned(),
                },
            )
            .await?;
        self.provider_oauth_continuation(response)
    }

    pub async fn refresh_provider_oauth(
        &self,
        name: &str,
        credential: OAuthCredential,
    ) -> Result<OAuthCredential> {
        self.require_provider_feature(name, true, false)?;
        let response = self
            .invoke_guarded(
                &uuid::Uuid::new_v4().to_string(),
                AbiRequest::ProviderOAuthRefresh {
                    name: name.to_owned(),
                    credential: credential.into(),
                },
            )
            .await?;
        let AbiResponse::ProviderOAuth { credential } = response else {
            return Err(MimirError::Protocol(
                "extension returned the wrong response for an OAuth refresh".into(),
            ));
        };
        validate_oauth_credential(&credential)?;
        Ok(credential.into())
    }

    pub async fn provider_oauth_api_key(
        &self,
        name: &str,
        credential: OAuthCredential,
    ) -> Result<String> {
        self.require_provider_feature(name, true, false)?;
        let response = self
            .invoke_guarded(
                &uuid::Uuid::new_v4().to_string(),
                AbiRequest::ProviderOAuthGetApiKey {
                    name: name.to_owned(),
                    credential: credential.into(),
                },
            )
            .await?;
        let AbiResponse::ProviderApiKey { api_key } = response else {
            return Err(MimirError::Protocol(
                "extension returned the wrong response for OAuth API-key conversion".into(),
            ));
        };
        validate_provider_secret(&api_key)?;
        Ok(api_key)
    }

    pub async fn invoke_provider_stream(
        &self,
        name: &str,
        model: &str,
        credential: &str,
        request: ModelRequest,
    ) -> Result<ExtensionProviderStreamResult> {
        self.require_provider_feature(name, false, true)?;
        validate_model(model)?;
        validate_provider_secret(credential)?;
        let response = self
            .invoke_guarded(
                &uuid::Uuid::new_v4().to_string(),
                AbiRequest::ProviderStream {
                    name: name.to_owned(),
                    model: model.to_owned(),
                    credential: credential.to_owned(),
                    request,
                },
            )
            .await?;
        let AbiResponse::ProviderStream { result } = response else {
            return Err(MimirError::Protocol(
                "extension returned the wrong response for a provider stream".into(),
            ));
        };
        validate_provider_stream_result(&result)?;
        Ok(result)
    }

    fn require_provider_feature(&self, name: &str, oauth: bool, stream: bool) -> Result<()> {
        let descriptor = self
            .registrations
            .providers
            .iter()
            .find(|provider| provider.name == name)
            .ok_or_else(|| {
                MimirError::Protocol(format!(
                    "extension '{}' did not register provider '{name}'",
                    self.manifest.name
                ))
            })?;
        if oauth && descriptor.oauth_name.is_none() {
            return Err(MimirError::Protocol(format!(
                "extension provider '{name}' did not register OAuth callbacks"
            )));
        }
        if stream && !descriptor.custom_stream {
            return Err(MimirError::Protocol(format!(
                "extension provider '{name}' did not register a custom stream"
            )));
        }
        Ok(())
    }

    fn provider_oauth_continuation(&self, response: AbiResponse) -> Result<UiResponseContinuation> {
        match response {
            AbiResponse::ProviderOAuth { credential } => {
                validate_oauth_credential(&credential)?;
                Ok(UiResponseContinuation {
                    oauth_credential: Some(credential.into()),
                    ..UiResponseContinuation::default()
                })
            }
            AbiResponse::Suspended { request } => {
                validate_ui_requests(
                    &self.manifest,
                    &self.registrations,
                    std::slice::from_ref(&request),
                )?;
                Ok(UiResponseContinuation {
                    next_request: Some(request),
                    ..UiResponseContinuation::default()
                })
            }
            _ => Err(MimirError::Protocol(
                "extension returned the wrong response for an OAuth login".into(),
            )),
        }
    }

    pub async fn submit_ui_response(
        &self,
        request_id: &str,
        response: Value,
    ) -> Result<UiResponseContinuation> {
        validate_identifier("UI request", request_id)?;
        if serde_json::to_vec(&response)?.len() > 64 * 1024 {
            return Err(MimirError::Protocol(
                "extension UI response exceeds 64 KiB".into(),
            ));
        }
        let response = self
            .invoke_guarded(
                &uuid::Uuid::new_v4().to_string(),
                AbiRequest::UiResponse {
                    request_id: request_id.to_owned(),
                    response,
                },
            )
            .await?;
        match response {
            AbiResponse::UiResponse { accepted: true } => Ok(UiResponseContinuation::default()),
            AbiResponse::UiResponse { accepted: false } => Err(MimirError::Protocol(format!(
                "extension '{}' rejected UI response '{request_id}'",
                self.manifest.name
            ))),
            AbiResponse::Command { result } => {
                validate_ui_requests(&self.manifest, &self.registrations, &result.ui_requests)?;
                validate_actions(&self.manifest, &result.actions)?;
                Ok(UiResponseContinuation {
                    command_result: Some(result),
                    next_request: None,
                    oauth_credential: None,
                })
            }
            AbiResponse::Suspended { request } => {
                validate_ui_requests(
                    &self.manifest,
                    &self.registrations,
                    std::slice::from_ref(&request),
                )?;
                Ok(UiResponseContinuation {
                    command_result: None,
                    next_request: Some(request),
                    oauth_credential: None,
                })
            }
            AbiResponse::ProviderOAuth { credential } => {
                validate_oauth_credential(&credential)?;
                Ok(UiResponseContinuation {
                    oauth_credential: Some(credential.into()),
                    ..UiResponseContinuation::default()
                })
            }
            _ => Err(MimirError::Protocol(
                "extension returned the wrong response for a UI response".into(),
            )),
        }
    }

    pub async fn deliver_bus_event(&self, topic: &str, data: Value) -> Result<bool> {
        validate_identifier("event topic", topic)?;
        if serde_json::to_vec(&data)?.len() > 64 * 1024 {
            return Err(MimirError::Protocol(
                "extension bus event exceeds 64 KiB".into(),
            ));
        }
        let response = self
            .invoke_guarded(
                &uuid::Uuid::new_v4().to_string(),
                AbiRequest::BusEvent {
                    topic: topic.to_owned(),
                    data,
                },
            )
            .await?;
        match response {
            AbiResponse::BusEvent { delivered } => Ok(delivered),
            _ => Err(MimirError::Protocol(
                "extension returned the wrong response for a bus event".into(),
            )),
        }
    }

    async fn invoke_guarded(&self, request_id: &str, request: AbiRequest) -> Result<AbiResponse> {
        let _permit = Arc::clone(&self.semaphore)
            .try_acquire_owned()
            .map_err(|_| {
                MimirError::Protocol(format!(
                    "extension '{}' reached its concurrency limit",
                    self.manifest.name
                ))
            })?;
        assert_generation_current(&self.state_path, self.generation).await?;
        let response = invoke_abi(&self.host, request_id, self.generation, request).await?;
        assert_generation_current(&self.state_path, self.generation).await?;
        Ok(response)
    }

    pub fn state_root(&self) -> &Path {
        &self.state_root
    }
}

async fn invoke_abi(
    host: &ExtensionBackend,
    request_id: &str,
    generation: u64,
    request: AbiRequest,
) -> Result<AbiResponse> {
    let payload = serde_json::to_value(AbiEnvelope {
        abi_version: ABI_VERSION,
        generation,
        request,
    })?;
    let output = match host {
        ExtensionBackend::Native(host) => {
            let response = host
                .invoke(HostRequest {
                    schema_version: 1,
                    id: request_id.to_owned(),
                    command: "extension.abi".into(),
                    payload,
                })
                .await?;
            if response.status != HostResponseStatus::Ok {
                return Err(MimirError::Protocol(
                    response
                        .message
                        .unwrap_or_else(|| "extension rejected the ABI request".into()),
                ));
            }
            response.output
        }
        ExtensionBackend::Embedded(host) => host.invoke(&payload).await?,
    };
    let envelope: AbiResponseEnvelope = serde_json::from_value(output).map_err(|error| {
        MimirError::Protocol(format!(
            "extension returned an invalid ABI response: {error}"
        ))
    })?;
    if envelope.abi_version != ABI_VERSION {
        return Err(MimirError::Protocol(format!(
            "unsupported extension ABI response version {}",
            envelope.abi_version
        )));
    }
    if envelope.generation != generation {
        return Err(MimirError::Protocol(format!(
            "extension response generation {} does not match active generation {generation}",
            envelope.generation
        )));
    }
    Ok(envelope.response)
}

async fn next_generation(state_root: &Path, state_path: &Path) -> Result<(u64, u64)> {
    let lock = atomic::path_lock(state_path);
    let _guard = lock.lock().await;
    atomic::prepare_state_path(state_root, state_path).await?;
    let previous: Option<RuntimeState> = atomic::read_json(state_path).await?;
    if previous
        .as_ref()
        .is_some_and(|state| state.schema_version != RUNTIME_STATE_VERSION)
    {
        return Err(MimirError::Configuration(
            "unsupported extension runtime state schema version".into(),
        ));
    }
    let previous_generation = previous.map_or(0, |state| state.generation);
    let generation = previous_generation.checked_add(1).ok_or_else(|| {
        MimirError::Configuration("extension runtime generation exhausted".into())
    })?;
    Ok((previous_generation, generation))
}

async fn commit_generation(
    state_root: &Path,
    state_path: &Path,
    expected_previous: u64,
    generation: u64,
    extension_version: &str,
) -> Result<()> {
    let lock = atomic::path_lock(state_path);
    let _guard = lock.lock().await;
    atomic::prepare_state_path(state_root, state_path).await?;
    let current: Option<RuntimeState> = atomic::read_json(state_path).await?;
    let current_generation = current.as_ref().map_or(0, |state| state.generation);
    if current
        .as_ref()
        .is_some_and(|state| state.schema_version != RUNTIME_STATE_VERSION)
    {
        return Err(MimirError::Configuration(
            "unsupported extension runtime state schema version".into(),
        ));
    }
    if current_generation != expected_previous {
        return Err(MimirError::Protocol(format!(
            "extension reload lost its generation lease; expected {expected_previous}, active generation is {current_generation}"
        )));
    }
    atomic::write_json(
        state_path,
        &RuntimeState {
            schema_version: RUNTIME_STATE_VERSION,
            generation,
            extension_version: extension_version.to_owned(),
        },
    )
    .await?;
    Ok(())
}

async fn assert_generation_current(state_path: &Path, expected: u64) -> Result<()> {
    let lock = atomic::path_lock(state_path);
    let _guard = lock.lock().await;
    let state: RuntimeState = atomic::read_json(state_path)
        .await?
        .ok_or_else(|| MimirError::Protocol("extension runtime state is missing".into()))?;
    if state.schema_version != RUNTIME_STATE_VERSION || state.generation != expected {
        return Err(MimirError::Protocol(format!(
            "extension runtime stale generation {expected}; active generation is {}",
            state.generation
        )));
    }
    Ok(())
}

fn validate_limits(limits: RuntimeLimits) -> Result<()> {
    if limits.max_concurrency == 0
        || limits.max_registrations == 0
        || limits.host.max_request_bytes == 0
        || limits.host.max_response_bytes == 0
        || limits.host.timeout.is_zero()
    {
        return Err(MimirError::Configuration(
            "extension runtime limits must be greater than zero".into(),
        ));
    }
    Ok(())
}

fn validate_registrations(
    manifest: &ExtensionManifest,
    registrations: &ExtensionRegistrations,
    max_registrations: usize,
) -> Result<()> {
    let count = registrations.tools.len()
        + registrations.commands.len()
        + registrations.shortcuts.len()
        + registrations.flags.len()
        + registrations.ui_requests.len()
        + registrations.renderers.len()
        + registrations.providers.len()
        + registrations.lifecycle_events.len();
    if count > max_registrations {
        return Err(MimirError::Protocol(format!(
            "extension registrations exceed the configured limit of {max_registrations}"
        )));
    }
    require_capability(
        manifest,
        Capability::Tools,
        !registrations.tools.is_empty(),
        "tools",
    )?;
    require_capability(
        manifest,
        Capability::Commands,
        !registrations.commands.is_empty(),
        "commands",
    )?;
    require_capability(
        manifest,
        Capability::Ui,
        !registrations.ui_requests.is_empty() || !registrations.renderers.is_empty(),
        "ui",
    )?;
    require_capability(
        manifest,
        Capability::Provider,
        !registrations.providers.is_empty(),
        "provider",
    )?;
    require_capability(
        manifest,
        Capability::Lifecycle,
        !registrations.lifecycle_events.is_empty(),
        "lifecycle",
    )?;

    validate_tools(&registrations.tools)?;
    validate_commands(&registrations.commands)?;
    validate_shortcuts(&registrations.shortcuts)?;
    validate_flags(&registrations.flags)?;
    validate_renderers(&registrations.renderers)?;
    validate_providers(&registrations.providers)
}

fn validate_shortcuts(shortcuts: &[ShortcutDescriptor]) -> Result<()> {
    validate_unique(
        shortcuts
            .iter()
            .map(|descriptor| descriptor.shortcut.as_str()),
        "shortcut",
    )?;
    for shortcut in shortcuts {
        validate_text("shortcut", &shortcut.shortcut)?;
        if shortcut.shortcut.len() > 64 || shortcut.shortcut.chars().any(char::is_control) {
            return Err(MimirError::Protocol("extension shortcut is invalid".into()));
        }
        if let Some(description) = &shortcut.description {
            validate_text("shortcut description", description)?;
        }
    }
    Ok(())
}

fn validate_flags(flags: &[FlagDescriptor]) -> Result<()> {
    validate_unique(
        flags.iter().map(|descriptor| descriptor.name.as_str()),
        "flag",
    )?;
    for flag in flags {
        validate_identifier("flag", &flag.name)?;
        if let Some(description) = &flag.description {
            validate_text("flag description", description)?;
        }
        if let Some(default) = &flag.default {
            let valid = match flag.kind {
                super::FlagKind::Boolean => default.is_boolean(),
                super::FlagKind::String => default.is_string(),
            };
            if !valid {
                return Err(MimirError::Protocol(format!(
                    "extension flag '{}' default does not match its type",
                    flag.name
                )));
            }
        }
    }
    Ok(())
}

fn validate_tools(tools: &[ToolDescriptor]) -> Result<()> {
    validate_unique(
        tools.iter().map(|descriptor| descriptor.name.as_str()),
        "tool",
    )?;
    for tool in tools {
        validate_identifier("tool", &tool.name)?;
        validate_text("tool label", &tool.label)?;
        validate_text("tool description", &tool.description)?;
        if !tool.parameters.is_object() {
            return Err(MimirError::Protocol(format!(
                "extension tool '{}' parameters must be a JSON object",
                tool.name
            )));
        }
    }
    Ok(())
}

fn validate_commands(commands: &[CommandDescriptor]) -> Result<()> {
    validate_unique(
        commands.iter().map(|descriptor| descriptor.name.as_str()),
        "command",
    )?;
    for command in commands {
        validate_identifier("command", &command.name)?;
        if let Some(description) = &command.description {
            validate_text("command description", description)?;
        }
    }
    Ok(())
}

fn validate_command_argument_completions(result: &CommandArgumentCompletions) -> Result<()> {
    if result.items.len() > 64 {
        return Err(MimirError::Protocol(
            "extension command completions returned too many items".into(),
        ));
    }
    for item in &result.items {
        validate_text("command completion", &item.value)?;
        if let Some(description) = &item.description {
            validate_text("command completion description", description)?;
        }
    }
    Ok(())
}

fn validate_renderers(renderers: &[RendererDescriptor]) -> Result<()> {
    validate_unique(
        renderers
            .iter()
            .map(|descriptor| descriptor.custom_type.as_str()),
        "renderer",
    )?;
    for renderer in renderers {
        validate_identifier("renderer", &renderer.custom_type)?;
    }
    Ok(())
}

fn validate_providers(providers: &[ProviderDescriptor]) -> Result<()> {
    validate_unique(
        providers.iter().map(|descriptor| descriptor.name.as_str()),
        "provider",
    )?;
    for provider in providers {
        validate_identifier("provider", &provider.name)?;
        if provider.models.is_empty() || provider.models.len() > 64 {
            return Err(MimirError::Protocol(format!(
                "extension provider '{}' must register between 1 and 64 models",
                provider.name
            )));
        }
        validate_unique(provider.models.iter().map(String::as_str), "provider model")?;
        for model in &provider.models {
            validate_model(model)?;
        }
        if let Some(base_url) = &provider.base_url {
            let url = reqwest::Url::parse(base_url).map_err(|error| {
                MimirError::Protocol(format!(
                    "extension provider '{}' base URL is invalid: {error}",
                    provider.name
                ))
            })?;
            if !matches!(url.scheme(), "http" | "https") {
                return Err(MimirError::Protocol(format!(
                    "extension provider '{}' base URL must use HTTP or HTTPS",
                    provider.name
                )));
            }
        }
        if let Some(environment) = &provider.credential_env
            && (environment.len() > 128
                || !environment
                    .bytes()
                    .all(|byte| byte == b'_' || byte.is_ascii_uppercase() || byte.is_ascii_digit()))
        {
            return Err(MimirError::Protocol(format!(
                "extension provider '{}' credential environment name is invalid",
                provider.name
            )));
        }
        if let Some(name) = &provider.oauth_name {
            validate_text("provider OAuth name", name)?;
        }
        let transport_matches = matches!(
            (provider.transport, provider.api),
            (
                ProviderTransport::AnthropicMessages,
                ProviderApi::AnthropicMessages
            ) | (
                ProviderTransport::OpenAiCompatible,
                ProviderApi::OpenAiCompletions | ProviderApi::OpenAiResponses
            )
        );
        if !transport_matches {
            return Err(MimirError::Protocol(format!(
                "extension provider '{}' API and transport do not match",
                provider.name
            )));
        }
    }
    Ok(())
}

fn validate_oauth_credential(credential: &ExtensionOAuthCredential) -> Result<()> {
    validate_provider_secret(&credential.access)?;
    if !credential.refresh.is_empty() {
        validate_provider_secret(&credential.refresh)?;
    }
    if credential.expires_at_ms == 0 {
        return Err(MimirError::Protocol(
            "extension OAuth credential has an invalid expiration".into(),
        ));
    }
    Ok(())
}

fn validate_provider_secret(secret: &str) -> Result<()> {
    if secret.trim().is_empty() || secret.len() > 64 * 1024 || secret.contains('\0') {
        return Err(MimirError::Protocol(
            "extension provider returned an invalid credential".into(),
        ));
    }
    Ok(())
}

fn validate_provider_stream_result(result: &ExtensionProviderStreamResult) -> Result<()> {
    const MAX_STREAM_EVENTS: usize = 4096;
    if result.response.message.role != Role::Assistant {
        return Err(MimirError::Protocol(
            "extension provider stream must return an assistant message".into(),
        ));
    }
    if result.events.len() > MAX_STREAM_EVENTS {
        return Err(MimirError::Protocol(format!(
            "extension provider stream exceeds {MAX_STREAM_EVENTS} delta events"
        )));
    }
    for event in &result.events {
        let text = match event {
            ExtensionProviderEvent::TextDelta { text }
            | ExtensionProviderEvent::ThinkingDelta { text } => text,
        };
        if text.len() > 64 * 1024 || text.contains('\0') {
            return Err(MimirError::Protocol(
                "extension provider stream returned an invalid delta".into(),
            ));
        }
    }
    Ok(())
}

fn validate_outcome(
    manifest: &ExtensionManifest,
    registrations: &ExtensionRegistrations,
    event_kind: LifecycleEventKind,
    outcome: &LifecycleOutcome,
) -> Result<()> {
    let legacy_cancel_allowed = can_block_event(event_kind);
    if outcome.cancel && !legacy_cancel_allowed {
        return Err(MimirError::Protocol(format!(
            "extension cannot cancel the {event_kind:?} lifecycle event"
        )));
    }
    match &outcome.interception {
        LifecycleInterception::Continue => {}
        LifecycleInterception::Block { reason } => {
            if !can_block_event(event_kind) {
                return Err(MimirError::Protocol(format!(
                    "extension cannot block the {event_kind:?} lifecycle event"
                )));
            }
            if let Some(reason) = reason {
                validate_text("interception block reason", reason)?;
            }
        }
        LifecycleInterception::Mutate { mutation } => {
            if outcome.cancel {
                return Err(MimirError::Protocol(
                    "extension lifecycle outcome cannot cancel and mutate the same event".into(),
                ));
            }
            validate_mutation(event_kind, mutation)?;
        }
        LifecycleInterception::Replace { replacement } => {
            if outcome.cancel {
                return Err(MimirError::Protocol(
                    "extension lifecycle outcome cannot cancel and replace the same event".into(),
                ));
            }
            validate_replacement(event_kind, replacement)?;
        }
    }
    validate_ui_requests(manifest, registrations, &outcome.ui_requests)?;
    validate_actions(manifest, &outcome.actions)?;
    Ok(())
}

fn validate_actions(manifest: &ExtensionManifest, actions: &[ExtensionHostAction]) -> Result<()> {
    if actions.len() > 64 || serde_json::to_vec(actions)?.len() > 256 * 1024 {
        return Err(MimirError::Protocol(
            "extension host actions exceed configured bounds".into(),
        ));
    }
    for action in actions {
        let required = match action {
            ExtensionHostAction::SetActiveTools { .. } => Some(Capability::Tools),
            ExtensionHostAction::SetModel { .. } | ExtensionHostAction::SetThinkingLevel { .. } => {
                Some(Capability::Provider)
            }
            ExtensionHostAction::Session { action } => {
                validate_session_action(action)?;
                Some(Capability::Commands)
            }
            ExtensionHostAction::SendMessage { .. }
            | ExtensionHostAction::SendUserMessage { .. }
            | ExtensionHostAction::AppendEntry { .. }
            | ExtensionHostAction::SetSessionName { .. }
            | ExtensionHostAction::SetLabel { .. }
            | ExtensionHostAction::PublishEvent { .. }
            | ExtensionHostAction::Abort
            | ExtensionHostAction::Shutdown => None,
        };
        if required.is_some_and(|capability| !manifest.capabilities.contains(&capability)) {
            return Err(MimirError::Protocol(format!(
                "extension host action requires the {required:?} capability"
            )));
        }
    }
    Ok(())
}

fn validate_session_action(action: &super::ExtensionSessionAction) -> Result<()> {
    const MAX_ID_BYTES: usize = 4 * 1024;
    const MAX_INSTRUCTIONS_BYTES: usize = 64 * 1024;
    let validate_bound = |label: &str, value: &str, maximum: usize| {
        if value.is_empty() || value.len() > maximum || value.contains('\0') {
            Err(MimirError::Protocol(format!(
                "extension session {label} exceeds its bound"
            )))
        } else {
            Ok(())
        }
    };
    match action {
        super::ExtensionSessionAction::New { parent_session } => {
            if let Some(parent) = parent_session {
                validate_bound("parent session", parent, MAX_ID_BYTES)?;
            }
        }
        super::ExtensionSessionAction::Fork { entry_id, .. } => {
            validate_bound("entry id", entry_id, MAX_ID_BYTES)?;
        }
        super::ExtensionSessionAction::NavigateTree {
            target_id,
            custom_instructions,
            label,
            ..
        } => {
            validate_bound("tree target", target_id, MAX_ID_BYTES)?;
            if let Some(instructions) = custom_instructions {
                validate_bound("tree instructions", instructions, MAX_INSTRUCTIONS_BYTES)?;
            }
            if let Some(label) = label {
                validate_bound("tree label", label, MAX_ID_BYTES)?;
            }
        }
        super::ExtensionSessionAction::Switch { session_path } => {
            validate_bound("switch path", session_path, MAX_ID_BYTES)?;
        }
        super::ExtensionSessionAction::Reload => {}
        super::ExtensionSessionAction::Compact {
            custom_instructions,
        } => {
            if let Some(instructions) = custom_instructions {
                validate_bound(
                    "compaction instructions",
                    instructions,
                    MAX_INSTRUCTIONS_BYTES,
                )?;
            }
        }
    }
    Ok(())
}

fn can_block_event(event_kind: LifecycleEventKind) -> bool {
    matches!(
        event_kind,
        LifecycleEventKind::SessionBeforeSwitch
            | LifecycleEventKind::SessionBeforeFork
            | LifecycleEventKind::SessionBeforeCompact
            | LifecycleEventKind::SessionBeforeTree
            | LifecycleEventKind::BeforeAgentStart
            | LifecycleEventKind::ModelSelect
            | LifecycleEventKind::ThinkingLevelSelect
            | LifecycleEventKind::ToolCall
            | LifecycleEventKind::UserBash
            | LifecycleEventKind::Input
    )
}

fn validate_mutation(event_kind: LifecycleEventKind, mutation: &LifecycleMutation) -> Result<()> {
    validate_interception_kind("mutation", event_kind, mutation.kind())
}

fn validate_replacement(
    event_kind: LifecycleEventKind,
    replacement: &LifecycleReplacement,
) -> Result<()> {
    validate_interception_kind("replacement", event_kind, replacement.kind())
}

fn validate_interception_kind(
    label: &str,
    event_kind: LifecycleEventKind,
    interception_kind: LifecycleEventKind,
) -> Result<()> {
    if event_kind != interception_kind {
        return Err(MimirError::Protocol(format!(
            "extension {label} kind {interception_kind:?} does not match lifecycle event {event_kind:?}"
        )));
    }
    if !can_intercept_event(event_kind) {
        return Err(MimirError::Protocol(format!(
            "extension cannot apply a {label} to the {event_kind:?} lifecycle event"
        )));
    }
    Ok(())
}

fn can_intercept_event(event_kind: LifecycleEventKind) -> bool {
    matches!(
        event_kind,
        LifecycleEventKind::Context
            | LifecycleEventKind::ModelSelect
            | LifecycleEventKind::ThinkingLevelSelect
            | LifecycleEventKind::ToolCall
            | LifecycleEventKind::ToolResult
            | LifecycleEventKind::UserBash
            | LifecycleEventKind::Input
            | LifecycleEventKind::RefineComplete
            | LifecycleEventKind::MessageEnd
    )
}

fn validate_ui_requests(
    manifest: &ExtensionManifest,
    registrations: &ExtensionRegistrations,
    requests: &[UiRequest],
) -> Result<()> {
    if !requests.is_empty() && !manifest.capabilities.contains(&Capability::Ui) {
        return Err(MimirError::Protocol(
            "extension lifecycle response requires the ui capability".into(),
        ));
    }
    if requests.len() > 32 {
        return Err(MimirError::Protocol(
            "extension lifecycle response contains too many UI requests".into(),
        ));
    }
    for request in requests {
        if !registrations.ui_requests.contains(&request.kind()) {
            return Err(MimirError::Protocol(format!(
                "extension used unregistered UI request kind {:?}",
                request.kind()
            )));
        }
        validate_ui_request(request)?;
    }
    Ok(())
}

fn validate_ui_request(request: &UiRequest) -> Result<()> {
    match request {
        UiRequest::Notify { level, message } => {
            if !matches!(level.as_str(), "info" | "warning" | "error") {
                return Err(MimirError::Protocol(
                    "extension notification level is invalid".into(),
                ));
            }
            validate_text("notification", message)
        }
        UiRequest::Input {
            id,
            prompt,
            placeholder,
        } => {
            validate_identifier("UI request", id)?;
            validate_text("UI prompt", prompt)?;
            if let Some(placeholder) = placeholder {
                validate_text("UI placeholder", placeholder)?;
            }
            Ok(())
        }
        UiRequest::Confirm { id, title, message } => {
            validate_identifier("UI request", id)?;
            validate_text("UI title", title)?;
            validate_text("UI message", message)
        }
        UiRequest::Select { id, title, options } => {
            validate_identifier("UI request", id)?;
            validate_text("UI title", title)?;
            if options.is_empty() || options.len() > 64 {
                return Err(MimirError::Protocol(
                    "extension UI selection must contain between 1 and 64 options".into(),
                ));
            }
            for option in options {
                validate_text("UI option", option)?;
            }
            Ok(())
        }
        UiRequest::SetStatus { key, text } => {
            validate_identifier("UI status", key)?;
            if let Some(text) = text {
                validate_text("UI status", text)?;
            }
            Ok(())
        }
    }
}

fn validate_render_output(output: &RenderOutput, label: &str) -> Result<()> {
    if output.lines.len() > 256 {
        return Err(MimirError::Protocol(format!(
            "{label} returned too many lines"
        )));
    }
    for line in &output.lines {
        if line.len() > 4096 {
            return Err(MimirError::Protocol(format!(
                "{label} line exceeds the configured limit"
            )));
        }
    }
    Ok(())
}

fn require_capability(
    manifest: &ExtensionManifest,
    capability: Capability,
    required: bool,
    label: &str,
) -> Result<()> {
    if required && !manifest.capabilities.contains(&capability) {
        return Err(MimirError::Protocol(format!(
            "extension '{}' registration requires the {label} capability",
            manifest.name
        )));
    }
    Ok(())
}

fn validate_identifier(label: &str, value: &str) -> Result<()> {
    let pattern = Regex::new(r"^[A-Za-z][A-Za-z0-9._-]{0,63}$")
        .expect("extension ABI identifier regex is valid");
    if pattern.is_match(value) {
        Ok(())
    } else {
        Err(MimirError::Protocol(format!(
            "extension {label} identifier is invalid"
        )))
    }
}

fn validate_model(value: &str) -> Result<()> {
    let pattern = Regex::new(r"^[A-Za-z0-9][A-Za-z0-9._:/-]{0,127}$")
        .expect("extension provider model regex is valid");
    if pattern.is_match(value) {
        Ok(())
    } else {
        Err(MimirError::Protocol(
            "extension provider model identifier is invalid".into(),
        ))
    }
}

fn validate_text(label: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() || value.len() > 4096 {
        Err(MimirError::Protocol(format!(
            "extension {label} must contain between 1 and 4096 bytes"
        )))
    } else {
        Ok(())
    }
}

fn validate_unique<'a>(values: impl Iterator<Item = &'a str>, label: &str) -> Result<()> {
    let mut seen = HashSet::new();
    for value in values {
        if !seen.insert(value) {
            return Err(MimirError::Protocol(format!(
                "extension registered duplicate {label} '{value}'"
            )));
        }
    }
    Ok(())
}
