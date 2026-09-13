#![allow(
    clippy::missing_errors_doc,
    reason = "RLM integration entrypoints consistently return the crate's typed configuration, provider, and persistence errors"
)]

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::{
    auth::{AuthCredential, AuthStore},
    budget::Budget,
    error::{MimirError, Result},
    extensions::{
        AuthenticatedModelCatalog, DEFAULT_RLM_MODEL_SEARCH_LIMIT, RlmChildExecutor,
        RlmChildStatus, RlmExecutionRequest, RlmExecutionResult, RlmModel, RlmRunRequest,
        RlmRuntime, RlmSubagent,
    },
    model::ThinkingLevel,
    provider::{
        Provider,
        registry::{ModelDefinition, ProviderDefinition, ProviderRegistry, model_catalog},
    },
    runtime::{AgentRuntime, EventSink, RuntimeConfig, RuntimeEvent},
    session::FileSessionStore,
    tools::ToolRegistry,
};

const DEFAULT_AUTHENTICATED_MODEL_LIMIT: usize = 512;
const DEFAULT_CHILD_MAX_TURNS: u32 = 12;
const DEFAULT_CHILD_MAX_TOOL_CALLS: u32 = 48;
const DEFAULT_CHILD_MAX_CONTEXT_MESSAGES: usize = 200;
const DEFAULT_CHILD_PROVIDER_TIMEOUT: Duration = Duration::from_secs(120);

/// Answers whether a provider has credentials that are usable without prompting.
#[async_trait]
pub trait ProviderAuthenticationProbe: Send + Sync {
    async fn is_authenticated(&self, provider: &ProviderDefinition) -> Result<bool>;
}

/// Authentication probe backed by the durable auth store and conservative
/// process-environment checks for ambient cloud credentials.
pub struct AuthStoreProviderAuthentication {
    store: AuthStore,
}

impl AuthStoreProviderAuthentication {
    #[must_use]
    pub fn new(store: AuthStore) -> Self {
        Self { store }
    }
}

#[async_trait]
impl ProviderAuthenticationProbe for AuthStoreProviderAuthentication {
    async fn is_authenticated(&self, provider: &ProviderDefinition) -> Result<bool> {
        if provider
            .env_vars
            .iter()
            .any(|name| nonempty_environment(name))
        {
            return Ok(true);
        }

        if let Some(credential) = self.store.get(provider.id).await? {
            return Ok(match credential {
                AuthCredential::ApiKey { key } => !key.trim().is_empty(),
                AuthCredential::OAuth(credential) => {
                    !credential.access.trim().is_empty() && !credential.is_expired(now_ms())
                }
            });
        }

        Ok(ambient_credentials_available(provider.id))
    }
}

/// Bounded model catalog containing only native-runtime models whose provider
/// currently has a usable credential.
pub struct AuthStoreModelCatalog {
    registry: ProviderRegistry,
    authentication: Arc<dyn ProviderAuthenticationProbe>,
    additional_models: Vec<ModelDefinition>,
    max_models: usize,
}

impl AuthStoreModelCatalog {
    pub fn new(store: AuthStore) -> Self {
        Self {
            registry: ProviderRegistry::builtin(),
            authentication: Arc::new(AuthStoreProviderAuthentication::new(store)),
            additional_models: Vec::new(),
            max_models: DEFAULT_AUTHENTICATED_MODEL_LIMIT,
        }
    }

    pub fn with_probe(
        authentication: Arc<dyn ProviderAuthenticationProbe>,
        max_models: usize,
    ) -> Result<Self> {
        if max_models == 0 {
            return Err(configuration(
                "authenticated RLM model limit must be greater than zero",
            ));
        }
        Ok(Self {
            registry: ProviderRegistry::builtin(),
            authentication,
            additional_models: Vec::new(),
            max_models,
        })
    }

    #[must_use]
    pub fn with_additional_models(mut self, models: Vec<ModelDefinition>) -> Self {
        self.additional_models = models;
        self
    }
}

#[async_trait]
impl AuthenticatedModelCatalog for AuthStoreModelCatalog {
    async fn list_authenticated_models(&self) -> Result<Vec<RlmModel>> {
        let mut authenticated_providers = BTreeSet::new();
        for provider in self.registry.iter() {
            if provider.supports_runtime() && self.authentication.is_authenticated(provider).await?
            {
                authenticated_providers.insert(provider.id);
            }
        }

        let mut models = model_catalog()
            .iter()
            .chain(&self.additional_models)
            .filter_map(|model| {
                let provider = self.registry.get(&model.provider)?;
                (authenticated_providers.contains(provider.id)
                    && provider.runtime_for_model(model).is_some())
                .then(|| RlmModel {
                    provider: model.provider.clone(),
                    id: model.id.clone(),
                    name: model.name.clone(),
                })
            })
            .collect::<Vec<_>>();
        models.sort_by_key(RlmModel::selector);
        models.dedup_by(|left, right| left.selector() == right.selector());
        if models.len() > self.max_models {
            return Err(configuration(format!(
                "authenticated RLM model catalog has {} entries, exceeding its configured limit of {}",
                models.len(),
                self.max_models
            )));
        }
        Ok(models)
    }
}

/// Constructs a native provider for an already-authenticated catalog model.
///
/// The CLI owns the concrete implementation so credential values never enter
/// the RLM registry or persisted child metadata.
#[async_trait]
pub trait RlmProviderFactory: Send + Sync {
    async fn create_provider(&self, model: &RlmModel) -> Result<Arc<dyn Provider>>;
}

/// Builds the capability-scoped tool registry for one admitted child.
/// Implementations may attach a fresh RLM runtime while the child remains
/// below the configured recursion depth.
#[async_trait]
pub trait RlmChildToolRegistryFactory: Send + Sync {
    async fn tools_for_child(
        self: Arc<Self>,
        request: &RlmExecutionRequest,
    ) -> Result<Arc<ToolRegistry>>;
}

#[derive(Debug, Clone)]
pub struct RlmChildRuntimePolicy {
    pub system_prompt: String,
    pub max_turns: u32,
    pub max_tool_calls: u32,
    pub max_context_messages: usize,
    pub max_elapsed: Duration,
    pub provider_timeout: Duration,
}

impl Default for RlmChildRuntimePolicy {
    fn default() -> Self {
        Self {
            system_prompt: "You are a recursive Mimir child. Complete the delegated task, persist useful artifacts in the shared workspace, and keep the result concise.".into(),
            max_turns: DEFAULT_CHILD_MAX_TURNS,
            max_tool_calls: DEFAULT_CHILD_MAX_TOOL_CALLS,
            max_context_messages: DEFAULT_CHILD_MAX_CONTEXT_MESSAGES,
            max_elapsed: Duration::from_secs(30 * 60),
            provider_timeout: DEFAULT_CHILD_PROVIDER_TIMEOUT,
        }
    }
}

/// Executes admitted RLM children through the same `AgentRuntime`, tool
/// registry, and append-only session store as ordinary harness sessions.
pub struct AgentRuntimeChildExecutor {
    providers: Arc<dyn RlmProviderFactory>,
    tools: Arc<ToolRegistry>,
    child_tools: Option<Arc<dyn RlmChildToolRegistryFactory>>,
    policy: RlmChildRuntimePolicy,
}

impl AgentRuntimeChildExecutor {
    #[must_use]
    pub fn new(
        providers: Arc<dyn RlmProviderFactory>,
        tools: Arc<ToolRegistry>,
        policy: RlmChildRuntimePolicy,
    ) -> Self {
        Self {
            providers,
            tools,
            child_tools: None,
            policy,
        }
    }

    #[must_use]
    pub fn with_child_tool_factory(
        mut self,
        child_tools: Arc<dyn RlmChildToolRegistryFactory>,
    ) -> Self {
        self.child_tools = Some(child_tools);
        self
    }
}

#[async_trait]
impl RlmChildExecutor for AgentRuntimeChildExecutor {
    async fn execute(
        &self,
        request: RlmExecutionRequest,
        cancellation: CancellationToken,
    ) -> Result<RlmExecutionResult> {
        let model = catalog_model(&request.model)?;
        let provider = self.providers.create_provider(&request.model).await?;
        let tools = if let Some(factory) = &self.child_tools {
            Arc::clone(factory).tools_for_child(&request).await?
        } else {
            Arc::clone(&self.tools)
        };
        let store =
            Arc::new(FileSessionStore::create(&request.session_dir, &request.session_id).await?);
        let budget = Budget {
            max_turns: self.policy.max_turns,
            max_tool_calls: self.policy.max_tool_calls,
            max_tokens: request.max_output_tokens,
            max_elapsed: self.policy.max_elapsed,
            max_context_messages: self.policy.max_context_messages,
            max_context_tokens: u64::from(model.context_window),
            auto_compaction_threshold_percent: 80,
        };
        let system_prompt = child_system_prompt(&self.policy.system_prompt, &request);
        let runtime = Arc::new(
            AgentRuntime::resume(
                provider,
                tools,
                store,
                RuntimeConfig {
                    provider: request.model.provider.clone(),
                    model: request.model.id.clone(),
                    thinking_level: ThinkingLevel::Off,
                    supported_thinking_levels: model.thinking_levels(),
                    thinking_level_map: model.thinking_level_map.clone(),
                    system_prompt,
                    budget,
                    provider_timeout: self.policy.provider_timeout,
                },
            )
            .await?,
        );
        let max_output_tokens = u32::try_from(request.max_output_tokens).map_err(|_| {
            configuration("RLM output token limit exceeds the provider protocol limit")
        })?;
        runtime.set_max_output_tokens(max_output_tokens)?;
        let sink = RlmUsageSink::default();
        let run = runtime.run(&request.prompt, &sink);
        tokio::pin!(run);
        let answer = tokio::select! {
            result = &mut run => result?,
            () = cancellation.cancelled() => {
                runtime.cancel();
                return Err(MimirError::Protocol("RLM child execution cancelled".into()));
            }
        };
        let reported = sink.output_tokens();
        let output_tokens = if reported == 0 {
            estimate_tokens(&answer)
        } else {
            reported
        };
        Ok(RlmExecutionResult { output_tokens })
    }
}

#[derive(Default)]
struct RlmUsageSink {
    output_tokens: AtomicU64,
}

impl RlmUsageSink {
    fn output_tokens(&self) -> u64 {
        self.output_tokens.load(Ordering::Acquire)
    }
}

#[async_trait]
impl EventSink for RlmUsageSink {
    async fn emit(&self, event: RuntimeEvent) {
        if let RuntimeEvent::MessageCompleted { message } = event {
            self.output_tokens
                .fetch_add(message.usage.output_tokens, Ordering::AcqRel);
        }
    }
}

/// Typed host bridge for the Python RLM operations used by the reference
/// harness. `spawn` remains admission-only; child answers stay in their session
/// transcript and shared artifacts.
pub struct RlmHostOperations {
    runtime: Arc<RlmRuntime>,
    parent_node_id: Option<String>,
}

impl RlmHostOperations {
    #[must_use]
    pub fn new(runtime: Arc<RlmRuntime>, parent_node_id: Option<String>) -> Self {
        Self {
            runtime,
            parent_node_id,
        }
    }

    pub async fn handle(&self, operation: &str, payload: Value) -> Result<Value> {
        match operation {
            "agent.spawn" => self.spawn(payload).await,
            "rlm.find_models" => self.find_models(payload).await,
            "rlm.list_subagents" => self.list_subagents().await,
            "rlm.delete_subagent" => self.delete_subagent(payload).await,
            "rlm.cancel_subagent" => self.cancel_subagent(payload).await,
            _ => Err(configuration(format!(
                "unsupported RLM host operation '{operation}'"
            ))),
        }
    }

    async fn spawn(&self, payload: Value) -> Result<Value> {
        let payload = require_object("agent.spawn", &payload)?;
        let prompt = required_string(payload, "prompt", "agent.spawn")?;
        let kwargs = match payload.get("kwargs") {
            None | Some(Value::Null) => Map::new(),
            Some(Value::Object(kwargs)) => kwargs.clone(),
            Some(_) => return Err(configuration("agent.spawn kwargs must be an object")),
        };
        let allowed = BTreeSet::from(["model", "name"]);
        let unsupported = kwargs
            .keys()
            .filter(|key| !allowed.contains(key.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        if !unsupported.is_empty() {
            return Err(configuration(format!(
                "unsupported agent.spawn kwargs: {}",
                unsupported.join(", ")
            )));
        }
        let name = optional_string(&kwargs, "name", "agent.spawn")?;
        let model = optional_string(&kwargs, "model", "agent.spawn")?;
        let spawn_code = optional_string(payload, "cellSourceCode", "agent.spawn")?;
        let handle = self
            .runtime
            .run(RlmRunRequest {
                prompt,
                name,
                model,
                parent_node_id: self.parent_node_id.clone(),
                spawn_code,
                max_output_tokens: None,
            })
            .await?;
        Ok(json!({
            "rlm_child_id": handle.child_id,
            "name": handle.name,
            "session_dir": path_string(&handle.session_dir)?,
            "model": handle.model,
        }))
    }

    async fn find_models(&self, payload: Value) -> Result<Value> {
        let payload = require_object("rlm.find_models", &payload)?;
        let query = required_string_allow_empty(payload, "query", "rlm.find_models")?;
        let limit = match payload.get("limit") {
            None => DEFAULT_RLM_MODEL_SEARCH_LIMIT,
            Some(value) => value
                .as_u64()
                .and_then(|value| usize::try_from(value).ok())
                .ok_or_else(|| configuration("rlm.find_models limit must be an integer"))?,
        };
        let models = self.runtime.find_models(&query, limit).await?;
        Ok(json!({
            "models": models.into_iter().map(|model| {
                let selector = model.selector();
                json!({
                    "provider": model.provider,
                    "id": model.id,
                    "name": model.name,
                    "selector": selector,
                })
            }).collect::<Vec<_>>()
        }))
    }

    async fn list_subagents(&self) -> Result<Value> {
        let subagents = self.runtime.list_subagents().await?;
        Ok(json!({
            "subagents": subagents.iter().map(subagent_value).collect::<Result<Vec<_>>>()?
        }))
    }

    async fn delete_subagent(&self, payload: Value) -> Result<Value> {
        let payload = require_object("rlm.delete_subagent", &payload)?;
        let target = required_string(payload, "target", "rlm.delete_subagent")?;
        let deleted = self.runtime.delete_subagent(&target).await?;
        Ok(json!({
            "subagent": subagent_value(&deleted.subagent)?,
            "outcome": "deleted",
        }))
    }

    async fn cancel_subagent(&self, payload: Value) -> Result<Value> {
        let payload = require_object("rlm.cancel_subagent", &payload)?;
        let target = required_string(payload, "target", "rlm.cancel_subagent")?;
        let cancelled = self.runtime.cancel_subagent(&target).await?;
        let subagent = self
            .runtime
            .list_subagents()
            .await?
            .into_iter()
            .find(|child| {
                child.child_id == target
                    || child.session_name == target
                    || child.session_id.as_deref() == Some(target.as_str())
            });
        Ok(json!({
            "cancelled": cancelled,
            "subagent": subagent.as_ref().map(subagent_value).transpose()?,
        }))
    }
}

fn catalog_model(model: &RlmModel) -> Result<ModelDefinition> {
    if let Some(definition) = model_catalog()
        .iter()
        .find(|candidate| candidate.provider == model.provider && candidate.id == model.id)
    {
        return Ok(definition.clone());
    }
    let registry = ProviderRegistry::builtin();
    let provider = registry
        .get(&model.provider)
        .ok_or_else(|| configuration("RLM model provider is not registered"))?;
    let definition = ModelDefinition::from_runtime(
        &model.provider,
        &model.id,
        provider.base_url,
        provider.runtime_support,
    );
    if provider.runtime_for_model(&definition).is_none() {
        return Err(configuration(format!(
            "RLM model '{}' does not have a native runtime",
            model.selector()
        )));
    }
    Ok(definition)
}

fn child_system_prompt(base: &str, request: &RlmExecutionRequest) -> String {
    let mut prompt = String::with_capacity(base.len().saturating_add(256));
    prompt.push_str(base.trim());
    prompt.push_str("\n\nRLM child identity:\n- child_id: ");
    prompt.push_str(&request.child_id);
    prompt.push_str("\n- parent_session_id: ");
    prompt.push_str(&request.parent_session_id);
    prompt.push_str("\n- depth: ");
    prompt.push_str(&request.depth.to_string());
    prompt
}

fn subagent_value(child: &RlmSubagent) -> Result<Value> {
    let active = matches!(
        child.status,
        RlmChildStatus::Queued | RlmChildStatus::Running
    );
    let status = match child.status {
        RlmChildStatus::Queued | RlmChildStatus::Running => "running",
        RlmChildStatus::Completed => "completed",
        RlmChildStatus::Error | RlmChildStatus::Cancelled => "error",
    };
    Ok(json!({
        "rlm_child_id": child.child_id,
        "active_session_id": active.then(|| child.session_id.clone()).flatten(),
        "session_id": child.session_id,
        "session_name": child.session_name,
        "session_dir": path_string(&child.session_dir)?,
        "status": status,
    }))
}

fn require_object<'a>(operation: &str, value: &'a Value) -> Result<&'a Map<String, Value>> {
    value
        .as_object()
        .ok_or_else(|| configuration(format!("{operation} payload must be an object")))
}

fn required_string(object: &Map<String, Value>, field: &str, operation: &str) -> Result<String> {
    let value = required_string_allow_empty(object, field, operation)?;
    if value.trim().is_empty() {
        return Err(configuration(format!(
            "{operation} {field} must not be empty"
        )));
    }
    Ok(value.trim().to_owned())
}

fn required_string_allow_empty(
    object: &Map<String, Value>,
    field: &str,
    operation: &str,
) -> Result<String> {
    object
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| configuration(format!("{operation} {field} must be a string")))
}

fn optional_string(
    object: &Map<String, Value>,
    field: &str,
    operation: &str,
) -> Result<Option<String>> {
    match object.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) if !value.trim().is_empty() => Ok(Some(value.trim().to_owned())),
        Some(Value::String(_)) => Err(configuration(format!(
            "{operation} {field} must not be empty"
        ))),
        Some(_) => Err(configuration(format!(
            "{operation} {field} must be a string"
        ))),
    }
}

fn path_string(path: &Path) -> Result<String> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| configuration("RLM session path is not valid UTF-8"))
}

fn nonempty_environment(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|value| !value.is_empty())
}

fn ambient_credentials_available(provider: &str) -> bool {
    match provider {
        "amazon-bedrock" => {
            (nonempty_environment("AWS_ACCESS_KEY_ID")
                && nonempty_environment("AWS_SECRET_ACCESS_KEY"))
                || (nonempty_environment("AWS_ROLE_ARN")
                    && nonempty_environment("AWS_WEB_IDENTITY_TOKEN_FILE"))
                || aws_profile_files_available()
                || nonempty_environment("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI")
                || nonempty_environment("AWS_CONTAINER_CREDENTIALS_FULL_URI")
        }
        "google-vertex" => {
            nonempty_environment("GOOGLE_OAUTH_ACCESS_TOKEN")
                || environment_file_exists("GOOGLE_APPLICATION_CREDENTIALS")
                || home_file_exists(".config/gcloud/application_default_credentials.json")
        }
        _ => false,
    }
}

fn environment_file_exists(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|path| PathBuf::from(path).is_file())
}

fn aws_profile_files_available() -> bool {
    environment_file_exists("AWS_SHARED_CREDENTIALS_FILE")
        || environment_file_exists("AWS_CONFIG_FILE")
        || home_file_exists(".aws/credentials")
        || home_file_exists(".aws/config")
}

fn home_file_exists(relative: &str) -> bool {
    std::env::var_os("HOME")
        .is_some_and(|directory| PathBuf::from(directory).join(relative).is_file())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

fn estimate_tokens(text: &str) -> u64 {
    if text.is_empty() {
        return 0;
    }
    let characters = u64::try_from(text.chars().count()).unwrap_or(u64::MAX);
    characters.saturating_add(3) / 4
}

fn configuration(message: impl Into<String>) -> MimirError {
    MimirError::Configuration(message.into())
}
