#![allow(
    clippy::missing_errors_doc,
    reason = "extension manager operations return typed protocol and activation errors"
)]

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock, Weak},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::{
    auth::{AuthCredential, AuthStore},
    config::ProviderConfig,
    error::{MimirError, Result},
    model::{ModelRequest, ModelResponse, ThinkingLevel},
    provider::{
        AnthropicProvider, OpenAiProvider, Provider, ProviderError, ProviderEvent,
        ProviderEventSink, ResponsesProvider,
    },
};

use super::{
    CatalogEntry, CommandDescriptor, CommandInvocationResult, ExtensionHostAction,
    ExtensionHostSnapshot, ExtensionRuntime, FlagDescriptor, LifecycleEvent, LifecycleOutcome,
    ProviderApi, ProviderDescriptor, RenderOutput, RendererDescriptor, RuntimeLimits,
    ShortcutDescriptor, ToolDescriptor, ToolInvocationResult,
};

const MAX_PENDING_UI_REQUESTS: usize = 128;
const MAX_PENDING_HOST_ACTIONS: usize = 256;
const UI_RESPONSE_TIMEOUT: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Clone)]
struct PendingUiInteraction {
    extension: String,
    generation: u64,
    request: super::UiRequest,
    expires_at: Instant,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExtensionDispatch {
    pub extension: String,
    pub outcome: LifecycleOutcome,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiscoveredResourcePaths {
    pub skill_paths: Vec<PathBuf>,
    pub prompt_paths: Vec<PathBuf>,
    pub theme_paths: Vec<PathBuf>,
}

#[derive(Debug, Deserialize, Default)]
#[allow(
    clippy::struct_field_names,
    reason = "the three protocol fields intentionally mirror skillPaths, promptPaths, and themePaths"
)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
struct ResourceDiscoveryOutput {
    skill_paths: Vec<String>,
    prompt_paths: Vec<String>,
    theme_paths: Vec<String>,
}

#[derive(Debug)]
pub struct ExtensionManager {
    workspace_root: PathBuf,
    auth_store: AuthStore,
    runtimes: BTreeMap<String, Arc<ExtensionRuntime>>,
    tools: BTreeMap<String, (String, ToolDescriptor)>,
    commands: BTreeMap<String, (String, CommandDescriptor)>,
    shortcuts: BTreeMap<String, (String, ShortcutDescriptor)>,
    flags: BTreeMap<String, (String, FlagDescriptor)>,
    renderers: BTreeMap<String, (String, RendererDescriptor)>,
    providers: BTreeMap<String, (String, ProviderDescriptor)>,
    extension_roots: BTreeMap<String, PathBuf>,
    pending_ui: Mutex<Vec<(String, super::UiRequest)>>,
    pending_actions: Mutex<Vec<ExtensionHostAction>>,
    pending_interactions: Mutex<BTreeMap<String, PendingUiInteraction>>,
    pending_oauth: Mutex<BTreeMap<String, String>>,
}

#[derive(Debug)]
struct ExtensionStreamProvider {
    runtime: Arc<ExtensionRuntime>,
    provider: String,
    model: String,
    credential: String,
}

#[async_trait]
impl Provider for ExtensionStreamProvider {
    async fn complete(
        &self,
        mut request: ModelRequest,
    ) -> std::result::Result<ModelResponse, ProviderError> {
        request.model.clone_from(&self.model);
        self.runtime
            .invoke_provider_stream(&self.provider, &self.model, &self.credential, request)
            .await
            .map(|result| result.response)
            .map_err(|error| extension_provider_error(&error, &self.credential))
    }

    async fn stream(
        &self,
        mut request: ModelRequest,
        sink: &dyn ProviderEventSink,
    ) -> std::result::Result<ModelResponse, ProviderError> {
        request.model.clone_from(&self.model);
        let result = self
            .runtime
            .invoke_provider_stream(&self.provider, &self.model, &self.credential, request)
            .await
            .map_err(|error| extension_provider_error(&error, &self.credential))?;
        for event in result.events {
            let event = match event {
                super::ExtensionProviderEvent::TextDelta { text } => ProviderEvent::TextDelta(text),
                super::ExtensionProviderEvent::ThinkingDelta { text } => {
                    ProviderEvent::ThinkingDelta(text)
                }
            };
            sink.emit(event).await;
        }
        Ok(result.response)
    }
}

fn extension_provider_error(error: &MimirError, credential: &str) -> ProviderError {
    let message = error.to_string().replace(credential, "[REDACTED]");
    ProviderError::Protocol { message }
}

fn redacted_extension_error(error: &MimirError, secrets: &[&str]) -> MimirError {
    let mut message = error.to_string();
    for secret in secrets.iter().filter(|secret| !secret.is_empty()) {
        message = message.replace(secret, "[REDACTED]");
    }
    MimirError::Protocol(message)
}

impl ExtensionManager {
    pub async fn load_shared(
        entries: Vec<CatalogEntry>,
        workspace_root: &Path,
        state_root: &Path,
        limits: RuntimeLimits,
        auth_store: AuthStore,
    ) -> Result<Arc<Self>> {
        type Cache = tokio::sync::Mutex<BTreeMap<String, Weak<ExtensionManager>>>;
        static CACHE: OnceLock<Cache> = OnceLock::new();
        let workspace =
            std::fs::canonicalize(workspace_root).unwrap_or_else(|_| workspace_root.to_path_buf());
        let state = std::fs::canonicalize(state_root).unwrap_or_else(|_| state_root.to_path_buf());
        let key = format!(
            "{}\0{}\0{}",
            workspace.display(),
            state.display(),
            auth_store.path().display()
        );
        let mut cache = CACHE
            .get_or_init(|| tokio::sync::Mutex::new(BTreeMap::new()))
            .lock()
            .await;
        if let Some(manager) = cache.get(&key).and_then(Weak::upgrade) {
            return Ok(manager);
        }
        let manager = Arc::new(
            Self::load_with_auth_store(entries, &workspace, &state, limits, auth_store).await?,
        );
        cache.insert(key, Arc::downgrade(&manager));
        Ok(manager)
    }

    pub async fn load(
        entries: Vec<CatalogEntry>,
        workspace_root: &Path,
        state_root: &Path,
        limits: RuntimeLimits,
    ) -> Result<Self> {
        Self::load_with_auth_store(
            entries,
            workspace_root,
            state_root,
            limits,
            AuthStore::global()?,
        )
        .await
    }

    /// Loads extensions with an explicit credential store for isolated embedding and tests.
    pub async fn load_with_auth_store(
        entries: Vec<CatalogEntry>,
        workspace_root: &Path,
        state_root: &Path,
        limits: RuntimeLimits,
        auth_store: AuthStore,
    ) -> Result<Self> {
        let mut manager = Self {
            workspace_root: workspace_root.to_path_buf(),
            auth_store,
            runtimes: BTreeMap::new(),
            tools: BTreeMap::new(),
            commands: BTreeMap::new(),
            shortcuts: BTreeMap::new(),
            flags: BTreeMap::new(),
            renderers: BTreeMap::new(),
            providers: BTreeMap::new(),
            extension_roots: BTreeMap::new(),
            pending_ui: Mutex::new(Vec::new()),
            pending_actions: Mutex::new(Vec::new()),
            pending_interactions: Mutex::new(BTreeMap::new()),
            pending_oauth: Mutex::new(BTreeMap::new()),
        };
        for entry in entries.into_iter().filter(|entry| entry.enabled) {
            let extension_name = entry.manifest.name.clone();
            let extension_root = entry.root_dir.clone();
            let runtime = Arc::new(
                ExtensionRuntime::load(
                    entry.manifest,
                    workspace_root,
                    state_root,
                    limits,
                    &Uuid::new_v4().to_string(),
                )
                .await?,
            );
            manager.insert_runtime(runtime)?;
            manager
                .extension_roots
                .insert(extension_name, extension_root);
        }
        Ok(manager)
    }

    pub fn is_empty(&self) -> bool {
        self.runtimes.is_empty()
    }

    #[must_use]
    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    pub fn tools(&self) -> Vec<ToolDescriptor> {
        self.tools
            .values()
            .map(|(_, value)| value.clone())
            .collect()
    }

    pub fn commands(&self) -> Vec<CommandDescriptor> {
        self.commands
            .values()
            .map(|(_, value)| value.clone())
            .collect()
    }

    pub fn shortcuts(&self) -> Vec<ShortcutDescriptor> {
        self.shortcuts
            .values()
            .map(|(_, value)| value.clone())
            .collect()
    }

    pub fn flags(&self) -> Vec<FlagDescriptor> {
        self.flags
            .values()
            .map(|(_, value)| value.clone())
            .collect()
    }

    pub fn providers(&self) -> Vec<ProviderDescriptor> {
        self.providers
            .values()
            .map(|(_, value)| value.clone())
            .collect()
    }

    pub async fn begin_provider_oauth_login(&self, name: &str) -> Result<Option<super::UiRequest>> {
        let (extension, descriptor) = self.providers.get(name).ok_or_else(|| {
            MimirError::Configuration(format!("extension provider '{name}' is not registered"))
        })?;
        if descriptor.oauth_name.is_none() {
            return Err(MimirError::Configuration(format!(
                "extension provider '{name}' does not support OAuth login"
            )));
        }
        let continuation = self
            .runtime(extension)?
            .begin_provider_oauth_login(name)
            .await?;
        if let Some(credential) = continuation.oauth_credential {
            self.auth_store.set_oauth(name, credential).await?;
            return Ok(None);
        }
        let request = continuation.next_request.ok_or_else(|| {
            MimirError::Protocol("extension OAuth login returned no credential or prompt".into())
        })?;
        self.queue_ui(extension, std::slice::from_ref(&request))
            .await?;
        let request_id = ui_request_id(&request).ok_or_else(|| {
            MimirError::Protocol("extension OAuth login returned a non-interactive prompt".into())
        })?;
        self.pending_oauth
            .lock()
            .await
            .insert(request_id.to_owned(), name.to_owned());
        Ok(Some(request))
    }

    /// Runs `resources_discover` and resolves every returned path beneath the
    /// declaring extension or workspace root. Symlink escapes and oversized
    /// catalogs fail closed before the resource loader sees them.
    pub async fn discover_resources(
        &self,
        reason: super::ResourceDiscoveryReason,
    ) -> Result<DiscoveredResourcePaths> {
        let cwd = self.workspace_root.display().to_string();
        let dispatched = self
            .dispatch_with_snapshot(
                LifecycleEvent::ResourcesDiscover {
                    cwd: cwd.clone(),
                    reason,
                },
                ExtensionHostSnapshot {
                    cwd,
                    ..ExtensionHostSnapshot::default()
                },
            )
            .await?;
        let mut discovered = DiscoveredResourcePaths::default();
        for dispatch in dispatched {
            if dispatch.outcome.cancel
                || !matches!(
                    dispatch.outcome.interception,
                    super::LifecycleInterception::Continue
                )
                || !dispatch.outcome.ui_requests.is_empty()
                || !dispatch.outcome.actions.is_empty()
            {
                return Err(MimirError::Protocol(
                    "resources_discover may only return resource paths".into(),
                ));
            }
            let output: ResourceDiscoveryOutput = if dispatch.outcome.output.is_null() {
                ResourceDiscoveryOutput::default()
            } else {
                serde_json::from_value(dispatch.outcome.output).map_err(|error| {
                    MimirError::Protocol(format!(
                        "extension '{}' returned invalid resources_discover output: {error}",
                        dispatch.extension
                    ))
                })?
            };
            let root = self
                .extension_roots
                .get(&dispatch.extension)
                .ok_or_else(|| {
                    MimirError::Protocol(format!(
                        "extension '{}' has no resource root",
                        dispatch.extension
                    ))
                })?;
            append_discovered_paths(
                &self.workspace_root,
                root,
                output.skill_paths,
                &mut discovered.skill_paths,
                "skill",
            )?;
            append_discovered_paths(
                &self.workspace_root,
                root,
                output.prompt_paths,
                &mut discovered.prompt_paths,
                "prompt",
            )?;
            append_discovered_paths(
                &self.workspace_root,
                root,
                output.theme_paths,
                &mut discovered.theme_paths,
                "theme",
            )?;
        }
        Ok(discovered)
    }

    #[allow(
        clippy::too_many_lines,
        reason = "provider activation keeps credential source, OAuth refresh, and transport construction in one audited path"
    )]
    pub async fn activate_provider(
        &self,
        name: &str,
        model: &str,
    ) -> Result<(Arc<dyn Provider>, Vec<ThinkingLevel>)> {
        let (extension, descriptor) = self.providers.get(name).ok_or_else(|| {
            MimirError::Configuration(format!("extension provider '{name}' is not registered"))
        })?;
        if !descriptor.models.iter().any(|candidate| candidate == model) {
            return Err(MimirError::Configuration(format!(
                "model '{model}' is not registered by extension provider '{name}'"
            )));
        }
        let environment_credential = descriptor
            .credential_env
            .as_ref()
            .and_then(|environment| std::env::var(environment).ok())
            .filter(|value| !value.trim().is_empty());
        let credential = if let Some(credential) = environment_credential {
            credential
        } else {
            match self.auth_store.get(name).await? {
                Some(AuthCredential::ApiKey { key }) => key,
                Some(AuthCredential::OAuth(mut oauth)) => {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_or(0, |duration| {
                            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
                        });
                    if descriptor.oauth_name.is_some() {
                        let runtime = self.runtime(extension)?;
                        if oauth.is_expired(now) {
                            let access = oauth.access.clone();
                            let refresh = oauth.refresh.clone();
                            oauth = runtime.refresh_provider_oauth(name, oauth).await.map_err(
                                |error| redacted_extension_error(&error, &[&access, &refresh]),
                            )?;
                            self.auth_store.set_oauth(name, oauth.clone()).await?;
                        }
                        let access = oauth.access.clone();
                        let refresh = oauth.refresh.clone();
                        runtime
                            .provider_oauth_api_key(name, oauth)
                            .await
                            .map_err(|error| {
                                redacted_extension_error(&error, &[&access, &refresh])
                            })?
                    } else {
                        if oauth.is_expired(now) {
                            return Err(MimirError::Configuration(format!(
                                "extension provider '{name}' OAuth credential is expired"
                            )));
                        }
                        oauth.access
                    }
                }
                None => {
                    let hint = descriptor
                        .credential_env
                        .as_deref()
                        .unwrap_or("a stored provider credential");
                    return Err(MimirError::Configuration(format!(
                        "extension provider '{name}' credential is missing; set {hint} or add a stored provider credential"
                    )));
                }
            }
        };
        if descriptor.custom_stream {
            return Ok((
                Arc::new(ExtensionStreamProvider {
                    runtime: Arc::clone(self.runtimes.get(extension).ok_or_else(|| {
                        MimirError::Protocol(format!(
                            "extension runtime '{extension}' is unavailable"
                        ))
                    })?),
                    provider: name.to_owned(),
                    model: model.to_owned(),
                    credential,
                }),
                vec![ThinkingLevel::Off],
            ));
        }
        let base_url = descriptor.base_url.as_deref().ok_or_else(|| {
            MimirError::Configuration(format!("extension provider '{name}' has no base URL"))
        })?;
        let provider: Arc<dyn Provider> = match descriptor.api {
            ProviderApi::OpenAiResponses => Arc::new(
                ResponsesProvider::new(Some(base_url), credential)
                    .map_err(|error| MimirError::Provider(error.to_string()))?,
            ),
            ProviderApi::AnthropicMessages => Arc::new(
                AnthropicProvider::new(Some(base_url), credential)
                    .map_err(|error| MimirError::Provider(error.to_string()))?,
            ),
            ProviderApi::OpenAiCompletions => {
                let mut config = ProviderConfig::openai(base_url, model, credential)?;
                config.name = name.into();
                Arc::new(
                    OpenAiProvider::new(config)
                        .map_err(|error| MimirError::Provider(error.to_string()))?,
                )
            }
        };
        Ok((provider, vec![ThinkingLevel::Off]))
    }

    pub fn renderers(&self) -> Vec<RendererDescriptor> {
        self.renderers
            .values()
            .map(|(_, value)| value.clone())
            .collect()
    }

    pub async fn dispatch(&self, event: LifecycleEvent) -> Result<Vec<ExtensionDispatch>> {
        self.dispatch_with_snapshot(event, ExtensionHostSnapshot::default())
            .await
    }

    pub async fn dispatch_with_snapshot(
        &self,
        event: LifecycleEvent,
        mut host: ExtensionHostSnapshot,
    ) -> Result<Vec<ExtensionDispatch>> {
        let event_kind = event.kind();
        let mut dispatched = Vec::new();
        for (name, runtime) in &self.runtimes {
            if let Some(outcome) = runtime
                .dispatch_with_snapshot(&Uuid::new_v4().to_string(), event.clone(), host.clone())
                .await?
            {
                self.queue_interactions(name, &outcome.ui_requests).await?;
                if event_kind == super::LifecycleEventKind::BeforeAgentStart
                    && let Some(system_prompt) =
                        outcome.output.get("systemPrompt").and_then(Value::as_str)
                {
                    if system_prompt.len() > 256 * 1024 || system_prompt.contains('\0') {
                        return Err(MimirError::Protocol(
                            "before_agent_start system prompt exceeds its bound".into(),
                        ));
                    }
                    host.system_prompt = system_prompt.to_owned();
                }
                dispatched.push(ExtensionDispatch {
                    extension: name.clone(),
                    outcome,
                });
            }
        }
        Ok(dispatched)
    }

    pub async fn invoke_tool(
        &self,
        name: &str,
        tool_call_id: &str,
        input: Value,
    ) -> Result<ToolInvocationResult> {
        self.invoke_tool_with_snapshot(name, tool_call_id, input, ExtensionHostSnapshot::default())
            .await
    }

    pub async fn invoke_tool_with_snapshot(
        &self,
        name: &str,
        tool_call_id: &str,
        input: Value,
        host: ExtensionHostSnapshot,
    ) -> Result<ToolInvocationResult> {
        let (extension, _) = self.tools.get(name).ok_or_else(|| {
            MimirError::Protocol(format!("extension tool '{name}' is not registered"))
        })?;
        let result = self
            .runtime(extension)?
            .invoke_tool_with_snapshot(&Uuid::new_v4().to_string(), name, tool_call_id, input, host)
            .await?;
        self.queue_ui(extension, &result.ui_requests).await?;
        self.queue_actions(&result.actions).await?;
        Ok(result)
    }

    pub async fn invoke_command(&self, name: &str, args: &str) -> Result<CommandInvocationResult> {
        self.invoke_command_with_snapshot(name, args, ExtensionHostSnapshot::default())
            .await
    }

    pub async fn invoke_command_with_snapshot(
        &self,
        name: &str,
        args: &str,
        host: ExtensionHostSnapshot,
    ) -> Result<CommandInvocationResult> {
        let (extension, _) = self.commands.get(name).ok_or_else(|| {
            MimirError::Protocol(format!("extension command '{name}' is not registered"))
        })?;
        let result = self
            .runtime(extension)?
            .invoke_command_with_snapshot(&Uuid::new_v4().to_string(), name, args, host)
            .await?;
        self.queue_ui(extension, &result.ui_requests).await?;
        self.queue_actions(&result.actions).await?;
        Ok(result)
    }

    pub async fn invoke_shortcut_with_snapshot(
        &self,
        shortcut: &str,
        host: ExtensionHostSnapshot,
    ) -> Result<CommandInvocationResult> {
        let (extension, _) = self.shortcuts.get(shortcut).ok_or_else(|| {
            MimirError::Protocol(format!("extension shortcut '{shortcut}' is not registered"))
        })?;
        let result = self
            .runtime(extension)?
            .invoke_shortcut_with_snapshot(&Uuid::new_v4().to_string(), shortcut, host)
            .await?;
        self.queue_ui(extension, &result.ui_requests).await?;
        self.queue_actions(&result.actions).await?;
        Ok(result)
    }

    pub async fn render_message(
        &self,
        custom_type: &str,
        message: Value,
        expanded: bool,
    ) -> Result<RenderOutput> {
        let (extension, _) = self.renderers.get(custom_type).ok_or_else(|| {
            MimirError::Protocol(format!(
                "extension renderer '{custom_type}' is not registered"
            ))
        })?;
        self.runtime(extension)?
            .render_message(&Uuid::new_v4().to_string(), custom_type, message, expanded)
            .await
    }

    pub async fn drain_host_actions(&self) -> Vec<ExtensionHostAction> {
        std::mem::take(&mut *self.pending_actions.lock().await)
    }

    pub async fn respond_ui(
        &self,
        request_id: &str,
        response: Value,
    ) -> Result<Option<CommandInvocationResult>> {
        if request_id.is_empty() || request_id.len() > 64 {
            return Err(MimirError::Protocol(
                "extension UI request id must contain 1 to 64 bytes".into(),
            ));
        }
        let pending = {
            let mut interactions = self.pending_interactions.lock().await;
            let now = Instant::now();
            interactions.retain(|_, pending| pending.expires_at > now);
            interactions.get(request_id).cloned().ok_or_else(|| {
                MimirError::Protocol(format!(
                    "extension UI request '{request_id}' is unknown or expired"
                ))
            })?
        };
        validate_ui_response(&pending.request, &response)?;
        self.pending_ui
            .lock()
            .await
            .retain(|(_, request)| ui_request_id(request) != Some(request_id));
        let runtime = self.runtime(&pending.extension)?;
        if runtime.generation() != pending.generation {
            self.pending_interactions.lock().await.remove(request_id);
            return Err(MimirError::Protocol(format!(
                "extension UI request '{request_id}' belongs to a stale generation"
            )));
        }
        let response_secret = response.as_str().map(str::to_owned);
        let continuation = runtime
            .submit_ui_response(request_id, response)
            .await
            .map_err(|error| {
                redacted_extension_error(&error, &[response_secret.as_deref().unwrap_or("")])
            })?;
        let oauth_provider = self.pending_oauth.lock().await.remove(request_id);
        let mut interactions = self.pending_interactions.lock().await;
        if interactions
            .get(request_id)
            .is_some_and(|active| active.generation == pending.generation)
        {
            interactions.remove(request_id);
        }
        drop(interactions);
        if let Some(next) = continuation.next_request {
            self.queue_ui(&pending.extension, std::slice::from_ref(&next))
                .await?;
            if let Some(provider) = oauth_provider.as_ref() {
                let next_id = ui_request_id(&next).ok_or_else(|| {
                    MimirError::Protocol(
                        "extension OAuth continuation returned a non-interactive prompt".into(),
                    )
                })?;
                self.pending_oauth
                    .lock()
                    .await
                    .insert(next_id.to_owned(), provider.clone());
            }
        }
        if let Some(result) = &continuation.command_result {
            self.queue_ui(&pending.extension, &result.ui_requests)
                .await?;
            self.queue_actions(&result.actions).await?;
        }
        if let Some(credential) = continuation.oauth_credential {
            let provider = oauth_provider.ok_or_else(|| {
                MimirError::Protocol(
                    "extension returned OAuth credentials for a non-OAuth interaction".into(),
                )
            })?;
            self.auth_store.set_oauth(&provider, credential).await?;
        }
        Ok(continuation.command_result)
    }

    pub async fn publish_event(&self, topic: &str, data: Value) -> Result<usize> {
        let mut delivered = 0_usize;
        for runtime in self.runtimes.values() {
            if runtime.deliver_bus_event(topic, data.clone()).await? {
                delivered = delivered.saturating_add(1);
            }
        }
        Ok(delivered)
    }

    async fn queue_actions(&self, actions: &[ExtensionHostAction]) -> Result<()> {
        let mut pending = self.pending_actions.lock().await;
        if pending.len().saturating_add(actions.len()) > MAX_PENDING_HOST_ACTIONS {
            return Err(MimirError::Protocol(format!(
                "extension host action queue exceeds {MAX_PENDING_HOST_ACTIONS} entries"
            )));
        }
        pending.extend_from_slice(actions);
        Ok(())
    }

    fn insert_runtime(&mut self, runtime: Arc<ExtensionRuntime>) -> Result<()> {
        let extension = runtime.name().to_owned();
        if self.runtimes.contains_key(&extension) {
            return Err(MimirError::Configuration(format!(
                "duplicate extension runtime '{extension}'"
            )));
        }
        for tool in &runtime.registrations().tools {
            insert_unique(
                &mut self.tools,
                &tool.name,
                &extension,
                tool.clone(),
                "tool",
            )?;
        }
        for command in &runtime.registrations().commands {
            insert_unique(
                &mut self.commands,
                &command.name,
                &extension,
                command.clone(),
                "command",
            )?;
        }
        for shortcut in &runtime.registrations().shortcuts {
            insert_unique(
                &mut self.shortcuts,
                &shortcut.shortcut,
                &extension,
                shortcut.clone(),
                "shortcut",
            )?;
        }
        for flag in &runtime.registrations().flags {
            insert_unique(
                &mut self.flags,
                &flag.name,
                &extension,
                flag.clone(),
                "flag",
            )?;
        }
        for renderer in &runtime.registrations().renderers {
            insert_unique(
                &mut self.renderers,
                &renderer.custom_type,
                &extension,
                renderer.clone(),
                "renderer",
            )?;
        }
        for provider in &runtime.registrations().providers {
            insert_unique(
                &mut self.providers,
                &provider.name,
                &extension,
                provider.clone(),
                "provider",
            )?;
        }
        self.runtimes.insert(extension, runtime);
        Ok(())
    }

    fn runtime(&self, extension: &str) -> Result<&ExtensionRuntime> {
        self.runtimes
            .get(extension)
            .map(AsRef::as_ref)
            .ok_or_else(|| {
                MimirError::Protocol(format!("extension runtime '{extension}' is unavailable"))
            })
    }

    pub async fn drain_ui_requests(&self) -> Vec<(String, super::UiRequest)> {
        std::mem::take(&mut *self.pending_ui.lock().await)
    }

    async fn queue_ui(&self, extension: &str, requests: &[super::UiRequest]) -> Result<()> {
        let mut pending = self.pending_ui.lock().await;
        if pending.len().saturating_add(requests.len()) > MAX_PENDING_UI_REQUESTS {
            return Err(MimirError::Protocol(
                "extension UI request queue reached its configured limit".into(),
            ));
        }
        pending.extend(
            requests
                .iter()
                .cloned()
                .map(|request| (extension.to_owned(), request)),
        );
        drop(pending);
        self.queue_interactions(extension, requests).await
    }

    async fn queue_interactions(
        &self,
        extension: &str,
        requests: &[super::UiRequest],
    ) -> Result<()> {
        let runtime = self.runtime(extension)?;
        let interactive = requests
            .iter()
            .filter_map(|request| ui_request_id(request).map(|id| (id, request)))
            .collect::<Vec<_>>();
        if interactive.is_empty() {
            return Ok(());
        }
        let mut interactions = self.pending_interactions.lock().await;
        let now = Instant::now();
        interactions.retain(|_, pending| pending.expires_at > now);
        if interactions.len().saturating_add(interactive.len()) > MAX_PENDING_UI_REQUESTS {
            return Err(MimirError::Protocol(
                "extension UI interaction queue reached its configured limit".into(),
            ));
        }
        for (id, _) in &interactive {
            if interactions.contains_key(*id) {
                return Err(MimirError::Protocol(format!(
                    "duplicate extension UI request id '{id}'"
                )));
            }
        }
        for (id, request) in interactive {
            interactions.insert(
                id.to_owned(),
                PendingUiInteraction {
                    extension: extension.to_owned(),
                    generation: runtime.generation(),
                    request: request.clone(),
                    expires_at: now + UI_RESPONSE_TIMEOUT,
                },
            );
        }
        Ok(())
    }
}

fn append_discovered_paths(
    workspace_root: &Path,
    extension_root: &Path,
    candidates: Vec<String>,
    output: &mut Vec<PathBuf>,
    kind: &str,
) -> Result<()> {
    const MAX_PATHS_PER_KIND: usize = 64;
    const MAX_PATH_BYTES: usize = 4 * 1024;
    if output.len().saturating_add(candidates.len()) > MAX_PATHS_PER_KIND {
        return Err(MimirError::Protocol(format!(
            "resources_discover returned more than {MAX_PATHS_PER_KIND} {kind} paths"
        )));
    }
    let workspace = std::fs::canonicalize(workspace_root).map_err(|error| {
        MimirError::Configuration(format!("workspace is inaccessible: {error}"))
    })?;
    let extension = std::fs::canonicalize(extension_root).map_err(|error| {
        MimirError::Configuration(format!("extension root is inaccessible: {error}"))
    })?;
    for candidate in candidates {
        if candidate.is_empty() || candidate.len() > MAX_PATH_BYTES || candidate.contains('\0') {
            return Err(MimirError::Protocol(format!(
                "resources_discover returned an invalid {kind} path"
            )));
        }
        let requested = PathBuf::from(&candidate);
        let requested = if requested.is_absolute() {
            requested
        } else {
            extension.join(requested)
        };
        let canonical = std::fs::canonicalize(&requested).map_err(|error| {
            MimirError::Configuration(format!(
                "extension {kind} resource '{}' is inaccessible: {error}",
                requested.display()
            ))
        })?;
        if !canonical.starts_with(&workspace) && !canonical.starts_with(&extension) {
            return Err(MimirError::Protocol(format!(
                "extension {kind} resource escapes the workspace and extension roots: {}",
                canonical.display()
            )));
        }
        output.push(canonical);
    }
    Ok(())
}

fn ui_request_id(request: &super::UiRequest) -> Option<&str> {
    match request {
        super::UiRequest::Input { id, .. }
        | super::UiRequest::Confirm { id, .. }
        | super::UiRequest::Select { id, .. } => Some(id),
        super::UiRequest::Notify { .. } | super::UiRequest::SetStatus { .. } => None,
    }
}

fn validate_ui_response(request: &super::UiRequest, response: &Value) -> Result<()> {
    if serde_json::to_vec(response)?.len() > 64 * 1024 {
        return Err(MimirError::Protocol(
            "extension UI response exceeds 64 KiB".into(),
        ));
    }
    let valid = match request {
        super::UiRequest::Input { .. } => response.is_null() || response.is_string(),
        super::UiRequest::Confirm { .. } => response.is_boolean(),
        super::UiRequest::Select { options, .. } => response
            .as_str()
            .is_some_and(|selected| options.iter().any(|option| option == selected)),
        super::UiRequest::Notify { .. } | super::UiRequest::SetStatus { .. } => false,
    };
    if !valid {
        return Err(MimirError::Protocol(
            "extension UI response does not match the pending request type".into(),
        ));
    }
    Ok(())
}

fn insert_unique<T>(
    registry: &mut BTreeMap<String, (String, T)>,
    name: &str,
    extension: &str,
    value: T,
    kind: &str,
) -> Result<()> {
    if let Some((owner, _)) = registry.get(name) {
        return Err(MimirError::Configuration(format!(
            "extension {kind} '{name}' conflicts between '{owner}' and '{extension}'"
        )));
    }
    registry.insert(name.to_owned(), (extension.to_owned(), value));
    Ok(())
}
