#![allow(
    clippy::missing_errors_doc,
    reason = "RLM APIs consistently return the crate's typed configuration, protocol, and persistence errors"
)]

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fmt::{self, Write as _},
    path::{Path, PathBuf},
    sync::{Arc, Mutex as StdMutex},
    time::Duration,
};

use async_trait::async_trait;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, Semaphore};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{
    atomic,
    error::{MimirError, Result},
};

pub const DEFAULT_RLM_MODEL_SEARCH_LIMIT: usize = 8;
pub const MAX_RLM_MODEL_SEARCH_LIMIT: usize = 20;
const STATE_SCHEMA_VERSION: u16 = 1;

#[derive(Debug, Clone)]
pub struct RlmRuntimeLimits {
    pub max_prompt_bytes: usize,
    pub max_spawn_code_bytes: usize,
    pub max_children: usize,
    pub max_concurrent_children: usize,
    pub max_depth: u32,
    pub max_duration: Duration,
    pub max_output_tokens: u64,
    pub max_catalog_models: usize,
    pub max_state_bytes: usize,
}

impl Default for RlmRuntimeLimits {
    fn default() -> Self {
        Self {
            max_prompt_bytes: 256 * 1024,
            max_spawn_code_bytes: 256 * 1024,
            max_children: 64,
            max_concurrent_children: 4,
            max_depth: 3,
            max_duration: Duration::from_secs(30 * 60),
            max_output_tokens: 65_536,
            max_catalog_models: 512,
            max_state_bytes: 4 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RlmModel {
    pub provider: String,
    pub id: String,
    pub name: String,
}

impl RlmModel {
    #[must_use]
    pub fn selector(&self) -> String {
        format!("{}/{}", self.provider, self.id)
    }
}

/// Supplies only models whose credentials are currently usable by the parent runtime.
#[async_trait]
pub trait AuthenticatedModelCatalog: Send + Sync {
    async fn list_authenticated_models(&self) -> Result<Vec<RlmModel>>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RlmExecutionRequest {
    pub child_id: String,
    pub session_id: String,
    pub session_name: String,
    pub session_dir: PathBuf,
    pub parent_session_id: String,
    pub parent_session_path: Option<String>,
    pub parent_node_id: Option<String>,
    pub depth: u32,
    pub prompt: String,
    pub spawn_code: Option<String>,
    pub model: RlmModel,
    pub max_output_tokens: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RlmExecutionResult {
    pub output_tokens: u64,
}

/// Executes one child using the host's already-authenticated model runtime.
#[async_trait]
pub trait RlmChildExecutor: Send + Sync {
    async fn execute(
        &self,
        request: RlmExecutionRequest,
        cancellation: CancellationToken,
    ) -> Result<RlmExecutionResult>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RlmRunRequest {
    pub prompt: String,
    pub name: Option<String>,
    pub model: Option<String>,
    pub parent_node_id: Option<String>,
    pub spawn_code: Option<String>,
    pub max_output_tokens: Option<u64>,
}

impl RlmRunRequest {
    #[must_use]
    pub fn new(prompt: impl Into<String>) -> Self {
        Self {
            prompt: prompt.into(),
            name: None,
            model: None,
            parent_node_id: None,
            spawn_code: None,
            max_output_tokens: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RlmSpawnHandle {
    pub child_id: String,
    pub name: String,
    pub session_dir: PathBuf,
    pub model: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RlmChildStatus {
    Queued,
    Running,
    Completed,
    Error,
    Cancelled,
}

impl RlmChildStatus {
    const fn is_active(self) -> bool {
        matches!(self, Self::Queued | Self::Running)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RlmSubagent {
    pub child_id: String,
    pub parent_session_id: String,
    pub parent_session_path: Option<String>,
    pub parent_node_id: Option<String>,
    pub session_id: Option<String>,
    pub session_name: String,
    pub session_dir: PathBuf,
    pub model: String,
    pub depth: u32,
    pub status: RlmChildStatus,
    pub output_tokens: u64,
    pub error: Option<String>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RlmDeleteResult {
    pub subagent: RlmSubagent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimeState {
    schema_version: u16,
    generation: String,
    workspace_scope: String,
    parent_session_id: String,
    parent_session_path: Option<String>,
    #[serde(default)]
    children: BTreeMap<String, RlmSubagent>,
}

struct RlmRuntimeInner {
    state_root: PathBuf,
    state_path: PathBuf,
    session_root: PathBuf,
    generation: String,
    parent_session_id: String,
    parent_session_path: Option<String>,
    child_depth: u32,
    default_model: String,
    catalog: Arc<dyn AuthenticatedModelCatalog>,
    executor: Arc<dyn RlmChildExecutor>,
    limits: RlmRuntimeLimits,
    semaphore: Arc<Semaphore>,
    state: Mutex<RuntimeState>,
    cancellations: StdMutex<HashMap<String, CancellationToken>>,
}

pub struct RlmRuntime {
    inner: Arc<RlmRuntimeInner>,
}

impl fmt::Debug for RlmRuntime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RlmRuntime")
            .field("state_path", &self.inner.state_path)
            .field("parent_session_id", &self.inner.parent_session_id)
            .field("child_depth", &self.inner.child_depth)
            .field("default_model", &self.inner.default_model)
            .finish_non_exhaustive()
    }
}

impl Drop for RlmRuntime {
    fn drop(&mut self) {
        let cancellations = self
            .inner
            .cancellations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for cancellation in cancellations.values() {
            cancellation.cancel();
        }
    }
}

impl RlmRuntime {
    #[allow(
        clippy::too_many_arguments,
        reason = "runtime construction makes parent identity, authenticated execution, and every safety boundary explicit"
    )]
    pub async fn open(
        state_root: &Path,
        workspace_root: &Path,
        parent_session_id: &str,
        parent_session_path: Option<&str>,
        parent_depth: u32,
        default_model: &str,
        catalog: Arc<dyn AuthenticatedModelCatalog>,
        executor: Arc<dyn RlmChildExecutor>,
        limits: RlmRuntimeLimits,
    ) -> Result<Self> {
        validate_limits(&limits)?;
        validate_bounded_text("parent session id", parent_session_id, 256, false)?;
        validate_bounded_text("default model", default_model, 512, false)?;
        if parent_session_path.is_some_and(|path| path.len() > 4096) {
            return Err(configuration("RLM parent session path is too long"));
        }
        let child_depth = parent_depth.saturating_add(1);
        if child_depth > limits.max_depth {
            return Err(configuration(format!(
                "RLM child depth {child_depth} exceeds configured max depth {}",
                limits.max_depth
            )));
        }

        let state_root = atomic::canonical_state_root(state_root);
        let workspace_scope = scope_hash(workspace_root);
        let parent_scope = hash_text(parent_session_id);
        let state_path = state_root
            .join("extensions/rlm-runtime")
            .join(&workspace_scope)
            .join(format!("{parent_scope}.json"));
        let session_root = state_root
            .join("rlm-sessions")
            .join(&workspace_scope)
            .join(&parent_scope);
        atomic::prepare_state_path(&state_root, &state_path).await?;
        let path_lock = atomic::path_lock(&state_path);
        let path_guard = path_lock.lock().await;
        let mut state = read_state_bounded(&state_path, limits.max_state_bytes)
            .await?
            .unwrap_or_else(|| RuntimeState {
                schema_version: STATE_SCHEMA_VERSION,
                generation: String::new(),
                workspace_scope: workspace_scope.clone(),
                parent_session_id: parent_session_id.into(),
                parent_session_path: parent_session_path.map(str::to_owned),
                children: BTreeMap::new(),
            });
        validate_loaded_state(
            &state,
            &workspace_scope,
            parent_session_id,
            limits.max_children,
        )?;
        let now = Utc::now().timestamp_millis();
        for child in state.children.values_mut() {
            if child.status.is_active() {
                child.status = RlmChildStatus::Error;
                child.error = Some("RLM child was interrupted by runtime restart".into());
                child.updated_at_ms = now;
            }
        }
        let generation = Uuid::new_v4().to_string();
        state.generation.clone_from(&generation);
        state.parent_session_path = parent_session_path.map(str::to_owned);
        write_state_bounded(&state_root, &state_path, &state, limits.max_state_bytes).await?;
        drop(path_guard);

        Ok(Self {
            inner: Arc::new(RlmRuntimeInner {
                state_root,
                state_path,
                session_root,
                generation,
                parent_session_id: parent_session_id.into(),
                parent_session_path: parent_session_path.map(str::to_owned),
                child_depth,
                default_model: default_model.into(),
                catalog,
                executor,
                semaphore: Arc::new(Semaphore::new(limits.max_concurrent_children)),
                limits,
                state: Mutex::new(state),
                cancellations: StdMutex::new(HashMap::new()),
            }),
        })
    }

    pub async fn find_models(&self, query: &str, limit: usize) -> Result<Vec<RlmModel>> {
        if !(1..=MAX_RLM_MODEL_SEARCH_LIMIT).contains(&limit) {
            return Err(configuration(format!(
                "RLM model search limit must be from 1 to {MAX_RLM_MODEL_SEARCH_LIMIT}"
            )));
        }
        let models = self.authenticated_models().await?;
        let query = normalize_search_text(query.trim());
        let mut ranked = models
            .into_iter()
            .filter_map(|model| {
                let selector = model.selector();
                let fields = [selector.as_str(), model.id.as_str(), model.name.as_str()];
                let normalized = fields.map(normalize_search_text);
                let score = if query.is_empty() {
                    Some(0_usize)
                } else if let Some(index) = normalized.iter().position(|field| field == &query) {
                    Some(index)
                } else if let Some(index) = normalized
                    .iter()
                    .position(|field| field.starts_with(&query))
                {
                    Some(3 + index)
                } else {
                    normalized
                        .iter()
                        .position(|field| field.contains(&query))
                        .map(|index| 6 + index)
                }?;
                Some((score, selector, model))
            })
            .collect::<Vec<_>>();
        ranked.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
        Ok(ranked
            .into_iter()
            .take(limit)
            .map(|(_, _, model)| model)
            .collect())
    }

    pub async fn list_subagents(&self) -> Result<Vec<RlmSubagent>> {
        let state = self.inner.state.lock().await;
        let mut children = state.children.values().cloned().collect::<Vec<_>>();
        children.sort_by(|left, right| {
            left.created_at_ms
                .cmp(&right.created_at_ms)
                .then_with(|| left.child_id.cmp(&right.child_id))
        });
        Ok(children)
    }

    pub async fn run(&self, request: RlmRunRequest) -> Result<RlmSpawnHandle> {
        let requested_tokens = self.validate_run_request(&request)?;
        let selector = request
            .model
            .as_deref()
            .unwrap_or(&self.inner.default_model);
        let model = self.resolve_authenticated_model(selector).await?;

        let child_id = format!("sub-{}", Uuid::new_v4().simple());
        let name = request
            .name
            .as_deref()
            .map(normalize_requested_name)
            .transpose()?
            .unwrap_or_else(|| default_child_name(&request.prompt, &child_id));
        let session_id = Uuid::new_v4().to_string();
        let session_dir = self.inner.session_root.join(&child_id);
        atomic::prepare_state_path(&self.inner.state_root, &session_dir.join("session.jsonl"))
            .await?;
        let now = Utc::now().timestamp_millis();
        let child = RlmSubagent {
            child_id: child_id.clone(),
            parent_session_id: self.inner.parent_session_id.clone(),
            parent_session_path: self.inner.parent_session_path.clone(),
            parent_node_id: request.parent_node_id.clone(),
            session_id: Some(session_id.clone()),
            session_name: name.clone(),
            session_dir: session_dir.clone(),
            model: model.selector(),
            depth: self.inner.child_depth,
            status: RlmChildStatus::Queued,
            output_tokens: 0,
            error: None,
            created_at_ms: now,
            updated_at_ms: now,
        };
        {
            let mut state = self.inner.state.lock().await;
            if state.children.len() >= self.inner.limits.max_children {
                return Err(configuration(
                    "RLM child registry is at its configured limit",
                ));
            }
            if state
                .children
                .values()
                .any(|existing| existing.session_name == name)
            {
                return Err(configuration(format!(
                    "RLM subagent session name '{name}' is already in use"
                )));
            }
            state.children.insert(child_id.clone(), child);
            if !self.inner.persist_if_current(&state).await? {
                state.children.remove(&child_id);
                return Err(configuration("RLM runtime was superseded by a restart"));
            }
        }

        let cancellation = CancellationToken::new();
        self.inner
            .cancellations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(child_id.clone(), cancellation.clone());
        let execution = RlmExecutionRequest {
            child_id: child_id.clone(),
            session_id,
            session_name: name.clone(),
            session_dir: session_dir.clone(),
            parent_session_id: self.inner.parent_session_id.clone(),
            parent_session_path: self.inner.parent_session_path.clone(),
            parent_node_id: request.parent_node_id,
            depth: self.inner.child_depth,
            prompt: request.prompt,
            spawn_code: request.spawn_code,
            model,
            max_output_tokens: requested_tokens,
        };
        let inner = Arc::clone(&self.inner);
        tokio::spawn(async move {
            inner.run_child(execution, cancellation).await;
        });
        Ok(RlmSpawnHandle {
            child_id,
            name,
            session_dir,
            model: selector.into(),
        })
    }

    pub async fn cancel_subagent(&self, target: &str) -> Result<bool> {
        let mut state = self.inner.state.lock().await;
        let child_id = resolve_child(&state, target)?.child_id.clone();
        let Some(child) = state.children.get_mut(&child_id) else {
            return Ok(false);
        };
        if !child.status.is_active() {
            return Ok(false);
        }
        if let Some(cancellation) = self
            .inner
            .cancellations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&child_id)
        {
            cancellation.cancel();
        }
        child.status = RlmChildStatus::Cancelled;
        child.error = Some("RLM child was cancelled by parent".into());
        child.updated_at_ms = Utc::now().timestamp_millis();
        if !self.inner.persist_if_current(&state).await? {
            return Err(configuration("RLM runtime was superseded by a restart"));
        }
        Ok(true)
    }

    pub async fn delete_subagent(&self, target: &str) -> Result<RlmDeleteResult> {
        let mut state = self.inner.state.lock().await;
        let child_id = resolve_child(&state, target)?.child_id.clone();
        if let Some(cancellation) = self
            .inner
            .cancellations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&child_id)
        {
            cancellation.cancel();
        }
        let child = state
            .children
            .remove(&child_id)
            .ok_or_else(|| configuration("RLM child disappeared during deletion"))?;
        if !self.inner.persist_if_current(&state).await? {
            state.children.insert(child_id, child);
            return Err(configuration("RLM runtime was superseded by a restart"));
        }
        Ok(RlmDeleteResult { subagent: child })
    }

    async fn authenticated_models(&self) -> Result<Vec<RlmModel>> {
        let models = self.inner.catalog.list_authenticated_models().await?;
        if models.len() > self.inner.limits.max_catalog_models {
            return Err(configuration(
                "authenticated RLM model catalog exceeds its limit",
            ));
        }
        let mut selectors = HashSet::new();
        for model in &models {
            validate_model(model)?;
            if !selectors.insert(model.selector()) {
                return Err(configuration(
                    "authenticated RLM model catalog has duplicates",
                ));
            }
        }
        Ok(models)
    }

    fn validate_run_request(&self, request: &RlmRunRequest) -> Result<u64> {
        validate_bounded_text(
            "run prompt",
            &request.prompt,
            self.inner.limits.max_prompt_bytes,
            false,
        )?;
        if let Some(code) = request.spawn_code.as_deref() {
            validate_bounded_text(
                "spawn code",
                code,
                self.inner.limits.max_spawn_code_bytes,
                true,
            )?;
        }
        if request
            .parent_node_id
            .as_ref()
            .is_some_and(|value| value.len() > 256)
        {
            return Err(configuration("RLM parent node id is too long"));
        }
        let requested_tokens = request
            .max_output_tokens
            .unwrap_or(self.inner.limits.max_output_tokens);
        if requested_tokens == 0 || requested_tokens > self.inner.limits.max_output_tokens {
            return Err(configuration(format!(
                "RLM output token limit must be from 1 to {}",
                self.inner.limits.max_output_tokens
            )));
        }
        Ok(requested_tokens)
    }

    async fn resolve_authenticated_model(&self, selector: &str) -> Result<RlmModel> {
        self.authenticated_models()
            .await?
            .into_iter()
            .find(|model| model.selector() == selector)
            .ok_or_else(|| {
                configuration(format!(
                    "RLM model '{selector}' is not in the authenticated model catalog"
                ))
            })
    }
}

impl RlmRuntimeInner {
    async fn run_child(
        self: Arc<Self>,
        request: RlmExecutionRequest,
        cancellation: CancellationToken,
    ) {
        let permit = tokio::select! {
            () = cancellation.cancelled() => {
                self.finish_child(&request.child_id, ChildOutcome::Cancelled).await;
                self.remove_cancellation(&request.child_id);
                return;
            }
            permit = Arc::clone(&self.semaphore).acquire_owned() => permit,
        };
        let Ok(_permit) = permit else {
            self.finish_child(
                &request.child_id,
                ChildOutcome::Error("RLM concurrency gate closed".into()),
            )
            .await;
            self.remove_cancellation(&request.child_id);
            return;
        };
        if cancellation.is_cancelled() {
            self.finish_child(&request.child_id, ChildOutcome::Cancelled)
                .await;
            self.remove_cancellation(&request.child_id);
            return;
        }
        if !self.mark_running(&request.child_id).await {
            self.remove_cancellation(&request.child_id);
            return;
        }

        let token_limit = request.max_output_tokens;
        let execution = self.executor.execute(request.clone(), cancellation.clone());
        tokio::pin!(execution);
        let timeout = tokio::time::sleep(self.limits.max_duration);
        tokio::pin!(timeout);
        let outcome = tokio::select! {
            () = cancellation.cancelled() => ChildOutcome::Cancelled,
            () = &mut timeout => {
                cancellation.cancel();
                ChildOutcome::Error("RLM child exceeded its time limit".into())
            }
            result = &mut execution => match result {
                Ok(result) if result.output_tokens <= token_limit => ChildOutcome::Completed(result.output_tokens),
                Ok(result) => ChildOutcome::Error(format!(
                    "RLM child returned {} output tokens, exceeding its limit of {token_limit}",
                    result.output_tokens
                )),
                Err(error) => ChildOutcome::Error(error.to_string()),
            },
        };
        self.finish_child(&request.child_id, outcome).await;
        self.remove_cancellation(&request.child_id);
    }

    async fn mark_running(&self, child_id: &str) -> bool {
        let mut state = self.state.lock().await;
        let Some(child) = state.children.get_mut(child_id) else {
            return false;
        };
        if child.status != RlmChildStatus::Queued {
            return false;
        }
        child.status = RlmChildStatus::Running;
        child.updated_at_ms = Utc::now().timestamp_millis();
        self.persist_if_current(&state).await.unwrap_or(false)
    }

    async fn finish_child(&self, child_id: &str, outcome: ChildOutcome) {
        let mut state = self.state.lock().await;
        let Some(child) = state.children.get_mut(child_id) else {
            return;
        };
        if child.status == RlmChildStatus::Cancelled && !matches!(outcome, ChildOutcome::Cancelled)
        {
            return;
        }
        match outcome {
            ChildOutcome::Completed(tokens) => {
                child.status = RlmChildStatus::Completed;
                child.output_tokens = tokens;
                child.error = None;
            }
            ChildOutcome::Error(error) => {
                child.status = RlmChildStatus::Error;
                child.error = Some(cap_error(&error));
            }
            ChildOutcome::Cancelled => {
                child.status = RlmChildStatus::Cancelled;
                child.error = Some("RLM child was cancelled by parent".into());
            }
        }
        child.updated_at_ms = Utc::now().timestamp_millis();
        let _ = self.persist_if_current(&state).await;
    }

    async fn persist_if_current(&self, state: &RuntimeState) -> Result<bool> {
        let path_lock = atomic::path_lock(&self.state_path);
        let _guard = path_lock.lock().await;
        let Some(current) =
            read_state_bounded(&self.state_path, self.limits.max_state_bytes).await?
        else {
            return Ok(false);
        };
        if current.generation != self.generation {
            return Ok(false);
        }
        write_state_bounded(
            &self.state_root,
            &self.state_path,
            state,
            self.limits.max_state_bytes,
        )
        .await?;
        Ok(true)
    }

    fn remove_cancellation(&self, child_id: &str) {
        self.cancellations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(child_id);
    }
}

enum ChildOutcome {
    Completed(u64),
    Error(String),
    Cancelled,
}

fn resolve_child<'a>(state: &'a RuntimeState, target: &str) -> Result<&'a RlmSubagent> {
    let target = target.trim();
    if target.is_empty() {
        return Err(configuration("RLM subagent target must not be empty"));
    }
    let matches = state
        .children
        .values()
        .filter(|child| {
            child.child_id == target
                || child.session_name == target
                || child.session_id.as_deref() == Some(target)
        })
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [child] => Ok(child),
        [] => Err(configuration(format!(
            "no direct RLM subagent matches '{target}'"
        ))),
        _ => Err(configuration(format!(
            "RLM subagent selector '{target}' is ambiguous"
        ))),
    }
}

fn validate_limits(limits: &RlmRuntimeLimits) -> Result<()> {
    if limits.max_prompt_bytes == 0
        || limits.max_spawn_code_bytes == 0
        || limits.max_children == 0
        || limits.max_concurrent_children == 0
        || limits.max_depth == 0
        || limits.max_duration.is_zero()
        || limits.max_output_tokens == 0
        || limits.max_catalog_models == 0
        || limits.max_state_bytes < 1024
    {
        return Err(configuration("RLM runtime limits must all be positive"));
    }
    Ok(())
}

fn validate_loaded_state(
    state: &RuntimeState,
    workspace_scope: &str,
    parent_session_id: &str,
    max_children: usize,
) -> Result<()> {
    if state.schema_version != STATE_SCHEMA_VERSION {
        return Err(configuration(format!(
            "unsupported RLM runtime schema version {}",
            state.schema_version
        )));
    }
    if state.workspace_scope != workspace_scope || state.parent_session_id != parent_session_id {
        return Err(configuration(
            "RLM runtime state identity does not match its path",
        ));
    }
    if state.children.len() > max_children {
        return Err(configuration(
            "persisted RLM child registry exceeds its limit",
        ));
    }
    Ok(())
}

fn validate_model(model: &RlmModel) -> Result<()> {
    validate_bounded_text("model provider", &model.provider, 128, false)?;
    validate_bounded_text("model id", &model.id, 384, false)?;
    validate_bounded_text("model name", &model.name, 384, false)?;
    if model.provider.contains('/') {
        return Err(configuration("RLM model provider must not contain '/'"));
    }
    Ok(())
}

fn validate_bounded_text(
    label: &str,
    value: &str,
    max_bytes: usize,
    allow_empty: bool,
) -> Result<()> {
    if (!allow_empty && value.trim().is_empty()) || value.len() > max_bytes {
        return Err(configuration(format!(
            "RLM {label} must contain from {} to {max_bytes} bytes",
            usize::from(!allow_empty)
        )));
    }
    if value.chars().any(char::is_control) {
        return Err(configuration(format!(
            "RLM {label} must not contain control characters"
        )));
    }
    Ok(())
}

fn normalize_requested_name(value: &str) -> Result<String> {
    let name = value.trim();
    validate_bounded_text("run name", name, 64, false)?;
    Ok(name.into())
}

fn default_child_name(prompt: &str, child_id: &str) -> String {
    let mut slug = String::with_capacity(40);
    let mut separated = false;
    for character in prompt.chars().flat_map(char::to_lowercase) {
        if character.is_ascii_alphanumeric() {
            if separated && !slug.is_empty() {
                slug.push('-');
            }
            slug.push(character);
            separated = false;
        } else {
            separated = true;
        }
        if slug.len() >= 40 {
            break;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        slug.push_str("worker");
    }
    let suffix = child_id
        .strip_prefix("sub-")
        .unwrap_or(child_id)
        .chars()
        .rev()
        .take(8)
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();
    format!("subagent-{slug}-{suffix}")
}

fn normalize_search_text(value: &str) -> String {
    value
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .flat_map(char::to_lowercase)
        .collect()
}

fn scope_hash(workspace_root: &Path) -> String {
    let workspace = std::fs::canonicalize(workspace_root)
        .unwrap_or_else(|_| workspace_root.to_path_buf())
        .to_string_lossy()
        .into_owned();
    hash_text(&workspace)
}

fn hash_text(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    let mut key = String::with_capacity(16);
    for byte in &digest[..8] {
        write!(key, "{byte:02x}").expect("writing to a string cannot fail");
    }
    key
}

async fn read_state_bounded(path: &Path, max_bytes: usize) -> Result<Option<RuntimeState>> {
    match tokio::fs::metadata(path).await {
        Ok(metadata) if metadata.len() > max_bytes as u64 => {
            return Err(configuration("RLM runtime state exceeds its size limit"));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    let bytes = tokio::fs::read(path).await?;
    if bytes.len() > max_bytes {
        return Err(configuration("RLM runtime state exceeds its size limit"));
    }
    Ok(Some(serde_json::from_slice(&bytes)?))
}

async fn write_state_bounded(
    state_root: &Path,
    path: &Path,
    state: &RuntimeState,
    max_bytes: usize,
) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(state)?;
    if bytes.len() > max_bytes {
        return Err(configuration("RLM runtime state exceeds its size limit"));
    }
    atomic::prepare_state_path(state_root, path).await?;
    atomic::write_json(path, state).await
}

fn cap_error(error: &str) -> String {
    error.chars().take(500).collect()
}

fn configuration(message: impl Into<String>) -> MimirError {
    MimirError::Configuration(message.into())
}
