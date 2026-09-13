mod approval;
mod bash;
mod extension;
mod file;
mod ipython;
mod mcp;
mod memory;
mod path_policy;
mod plan;
mod process;
mod rlm;

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::model::ToolDefinition;
use tokio_util::sync::CancellationToken;

pub use approval::{
    ApprovalDecision, DestructiveAction, PermissionRequest, WorkspaceApprovalStore,
};
pub use bash::{BashResult, BashRunner};
pub use mcp::{McpRegistrationReport, McpUnavailableServer};
pub use path_policy::WorkspacePathPolicy;
pub use plan::{ClarifyingOption, ClarifyingQuestion, PlanContextStore};

#[derive(Debug, Clone)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent tool capabilities and shell policy remain explicit for auditability"
)]
pub struct ToolPolicy {
    pub command_timeout: Duration,
    pub max_output_bytes: usize,
    pub max_write_bytes: usize,
    pub allow_write: bool,
    pub allow_process: bool,
    pub allow_shell: bool,
    pub allow_any_program: bool,
    pub agent_mode: AgentMode,
    pub allowed_programs: Option<Vec<String>>,
    pub approvals: Option<Arc<WorkspaceApprovalStore>>,
    pub plan_context: Option<Arc<PlanContextStore>>,
}

impl Default for ToolPolicy {
    fn default() -> Self {
        Self {
            command_timeout: Duration::from_secs(120),
            max_output_bytes: 64 * 1024,
            max_write_bytes: 2 * 1024 * 1024,
            allow_write: true,
            allow_process: false,
            allow_shell: false,
            allow_any_program: false,
            agent_mode: AgentMode::Default,
            allowed_programs: None,
            approvals: None,
            plan_context: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentMode {
    #[default]
    Default,
    Plan,
    Auto,
}

impl AgentMode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Plan => "plan",
            Self::Auto => "auto",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "default" => Some(Self::Default),
            "plan" => Some(Self::Plan),
            "auto" => Some(Self::Auto),
            _ => None,
        }
    }

    #[must_use]
    pub const fn automatically_approves(self) -> bool {
        matches!(self, Self::Auto)
    }

    #[must_use]
    pub const fn is_plan(self) -> bool {
        matches!(self, Self::Plan)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationStatus {
    Success,
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolObservation {
    pub status: ObservationStatus,
    pub summary: String,
    pub next_actions: Vec<String>,
    pub artifacts: Vec<std::path::PathBuf>,
    pub content: String,
}

impl ToolObservation {
    fn success(summary: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            status: ObservationStatus::Success,
            summary: summary.into(),
            next_actions: Vec::new(),
            artifacts: Vec::new(),
            content: content.into(),
        }
    }
}

#[derive(Debug, Error)]
pub enum ToolError {
    #[error("invalid arguments for {tool}: {message}")]
    InvalidArguments { tool: String, message: String },
    #[error("workspace policy denied path {path}: {reason}")]
    WorkspaceDenied { path: String, reason: String },
    #[error("tool {tool} is disabled by policy")]
    Disabled { tool: String },
    #[error("workspace approval required to {}: {command}", request.action.label(), command = request.command)]
    ApprovalRequired { request: PermissionRequest },
    #[error("user input is required")]
    UserInputRequired { request: ClarifyingQuestion },
    #[error("tool {tool} failed: {message}")]
    Execution { tool: String, message: String },
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[async_trait]
trait Tool: Send + Sync {
    fn definition(&self) -> ToolDefinition;
    async fn execute(&self, input: Value) -> Result<ToolObservation, ToolError>;

    async fn execute_cancellable(
        &self,
        input: Value,
        _cancellation: &CancellationToken,
    ) -> Result<ToolObservation, ToolError> {
        self.execute(input).await
    }
}

pub struct ToolRegistry {
    tools: BTreeMap<String, Arc<dyn Tool>>,
    rlm_runtime: Option<Arc<crate::extensions::RlmRuntime>>,
    workspace_root: Arc<PathBuf>,
    agent_mode: AgentMode,
    plan_context: Option<Arc<PlanContextStore>>,
}

impl ToolRegistry {
    /// Creates the standard workspace-scoped tool set.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if the workspace root cannot be canonicalized.
    pub fn with_default_tools(root: &Path, policy: ToolPolicy) -> Result<Self, ToolError> {
        let paths = WorkspacePathPolicy::new(root)?;
        let mut registry = Self {
            tools: BTreeMap::new(),
            rlm_runtime: None,
            workspace_root: Arc::new(paths.root().to_owned()),
            agent_mode: policy.agent_mode,
            plan_context: policy.plan_context.clone(),
        };
        registry.register(file::ReadFileTool::new(paths.clone(), policy.clone()));
        registry.register(file::ListFilesTool::new(paths.clone(), policy.clone()));
        registry.register(file::SearchTool::new(paths.clone(), policy.clone()));
        if policy.agent_mode == AgentMode::Plan {
            let context = policy
                .plan_context
                .clone()
                .ok_or_else(|| ToolError::Execution {
                    tool: "plan_mode".into(),
                    message: "plan mode requires a durable plan context".into(),
                })?;
            registry.register(plan::AskUserTool::new(Arc::clone(&context)));
            registry.register(plan::WritePlanTool::new(
                paths,
                context,
                policy.max_write_bytes,
            ));
            return Ok(registry);
        }
        registry.register(file::WriteFileTool::new(paths.clone(), policy.clone()));
        registry.register(file::EditFileTool::new(paths.clone(), policy.clone()));
        #[cfg(unix)]
        if policy.allow_shell {
            registry.register(bash::BashTool::new(paths.root(), policy.clone())?);
        }
        if policy.allow_process
            && policy
                .allowed_programs
                .as_ref()
                .is_some_and(|programs| !programs.is_empty())
        {
            registry.register(process::ProcessTool::new(paths, policy));
        }
        Ok(registry)
    }

    /// Returns a single system-prompt section describing the effective workspace contract.
    #[must_use]
    pub fn workspace_context(&self) -> String {
        let permission_guidance = match self.agent_mode {
            AgentMode::Default => {
                "Workspace writes and model-issued Bash commands require explicit approval."
            }
            AgentMode::Plan => {
                "Plan mode is active. Explore the workspace before deciding. Ask one focused clarifying question with ask_user whenever a material product or implementation choice cannot be discovered. Recommend the best option first and resolve low-risk details yourself. Do not implement, run commands, delegate, or mutate workspace files. When the plan is decision-complete, write it with write_plan; that bound plan artifact is the only workspace file plan mode may create or update."
            }
            AgentMode::Auto => {
                "Auto mode is active: workspace writes and model-issued Bash commands run without approval prompts."
            }
        };
        let remember_guidance = if self.tools.contains_key("remember") {
            " When the user explicitly asks you to remember or always remember guidance, call the remember tool with a concise standalone memory; do not merely acknowledge the request."
        } else {
            ""
        };
        format!(
            "Workspace root: {}\nFor filesystem tools and path-like process arguments, $WORKSPACE refers to this directory. Pass workspace-relative paths without '..'. To access a target outside it, do not retry with absolute paths or traversal; restart Mimir with a broader --workspace or copy the target into this workspace. {permission_guidance}{remember_guidance} Recursive search and listing skip .mimir, version-control metadata, dependencies, and generated outputs so internal state cannot amplify model context; read_file remains available for a deliberately targeted file. Bash and run_process execution are not an OS sandbox.",
            self.workspace_root.display(),
        )
    }

    /// Returns the canonical workspace root used by filesystem tools.
    #[must_use]
    pub fn workspace_root(&self) -> &Path {
        self.workspace_root.as_ref()
    }

    fn register(&mut self, tool: impl Tool + 'static) {
        let name = tool.definition().name;
        self.tools.insert(name, Arc::new(tool));
    }

    /// Materializes typed extension tool descriptors without replacing built-ins.
    ///
    /// # Errors
    ///
    /// Returns an execution error when a descriptor conflicts with a registered tool.
    pub fn register_extension_manager(
        &mut self,
        manager: &Arc<crate::extensions::ExtensionManager>,
    ) -> Result<(), ToolError> {
        self.deny_plan_registration("extension tools")?;
        for descriptor in manager.tools() {
            if self.tools.contains_key(&descriptor.name) {
                return Err(ToolError::Execution {
                    tool: descriptor.name,
                    message: "extension tool conflicts with an existing tool".into(),
                });
            }
            self.register(extension::RegisteredExtensionTool::new(
                Arc::clone(manager),
                descriptor,
            ));
        }
        Ok(())
    }

    /// Registers the bounded extension bridge when enabled tool extensions exist.
    ///
    /// # Errors
    ///
    /// Returns a validation error when an enabled extension cannot be safely exposed as a tool.
    pub fn register_extension_tools(
        &mut self,
        entries: Vec<crate::extensions::CatalogEntry>,
        workspace: &Path,
    ) -> Result<(), ToolError> {
        self.deny_plan_registration("extension tools")?;
        let tool = extension::ExtensionInvokeTool::from_catalog(entries, workspace)?;
        if !tool.is_empty() {
            self.register(tool);
        }
        Ok(())
    }

    /// Discovers tools from every enabled MCP server and registers the servers
    /// that are currently reachable and authenticated.
    ///
    /// One unavailable MCP integration never prevents the built-in tools or
    /// other MCP servers from starting. Callers receive a structured report so
    /// they can surface skipped integrations without exposing credentials.
    ///
    /// # Errors
    ///
    /// Returns an error only when the MCP catalog itself cannot be opened. A
    /// server-specific configuration, authentication, transport, or schema
    /// failure is captured in the returned report.
    pub async fn register_mcp_servers(
        &mut self,
        state_root: &Path,
    ) -> Result<McpRegistrationReport, ToolError> {
        self.deny_plan_registration("MCP tools")?;
        let discovery = mcp::discover(state_root).await?;
        let mut report = discovery.report;
        for tool in discovery.tools {
            let name = tool.definition().name;
            if self.tools.contains_key(&name) {
                report.unavailable.push(McpUnavailableServer {
                    server: tool.server_name().to_owned(),
                    reason: format!("generated tool name '{name}' conflicts with an existing tool"),
                });
                continue;
            }
            report.registered_tools.push(name);
            self.register(tool);
        }
        report.registered_tools.sort();
        report.unavailable.sort();
        Ok(report)
    }

    /// Captures the tools currently registered for use by a child runtime.
    ///
    /// Session-scoped RLM orchestration tools are always filtered out. A child
    /// factory may then attach a fresh runtime with the child's identity and
    /// depth, avoiding graph cycles while retaining core, MCP, and extension
    /// capabilities regardless of assembly order.
    #[must_use]
    pub fn snapshot_for_child_runtime(&self) -> Arc<Self> {
        Arc::new(self.fork_for_child_runtime())
    }

    /// Creates a mutable child registry without parent-only RLM aliases or the
    /// parent's session-scoped Python kernel. Child factories attach a fresh
    /// kernel under the child's own session identity.
    #[must_use]
    pub fn fork_for_child_runtime(&self) -> Self {
        Self {
            tools: self
                .tools
                .iter()
                .filter(|(name, _)| {
                    !rlm::is_reserved(name) && !matches!(name.as_str(), "ipython" | "remember")
                })
                .map(|(name, tool)| (name.clone(), Arc::clone(tool)))
                .collect(),
            rlm_runtime: None,
            workspace_root: Arc::clone(&self.workspace_root),
            agent_mode: self.agent_mode,
            plan_context: self.plan_context.clone(),
        }
    }

    /// Exposes the bounded RLM host operations as five typed model tools.
    ///
    /// # Errors
    ///
    /// Returns an execution error if an existing tool uses a reserved RLM name.
    pub fn register_rlm_runtime(
        &mut self,
        runtime: Arc<crate::extensions::RlmRuntime>,
    ) -> Result<(), ToolError> {
        self.deny_plan_registration("RLM child tools")?;
        rlm::register(self, Arc::clone(&runtime))?;
        self.rlm_runtime = Some(runtime);
        Ok(())
    }

    /// Returns at most `limit` children from the registered session-scoped RLM runtime.
    ///
    /// # Errors
    ///
    /// Returns a persistence error when the bounded RLM state cannot be read.
    pub async fn rlm_subagents(
        &self,
        limit: usize,
    ) -> crate::error::Result<Vec<crate::extensions::RlmSubagent>> {
        let Some(runtime) = &self.rlm_runtime else {
            return Ok(Vec::new());
        };
        let mut children = runtime.list_subagents().await?;
        children.truncate(limit);
        Ok(children)
    }

    /// Cancels one active child in this session's registered RLM runtime.
    ///
    /// # Errors
    ///
    /// Returns a validation or persistence error when the target is ambiguous,
    /// missing, or the generation-safe state update fails.
    pub async fn cancel_rlm_subagent(&self, target: &str) -> crate::error::Result<bool> {
        let Some(runtime) = &self.rlm_runtime else {
            return Ok(false);
        };
        runtime.cancel_subagent(target).await
    }

    /// Deletes one inactive child from this session's registered RLM runtime.
    ///
    /// # Errors
    ///
    /// Returns a validation or persistence error when the target is ambiguous,
    /// missing, active, or the generation-safe state update fails.
    pub async fn delete_rlm_subagent(
        &self,
        target: &str,
    ) -> crate::error::Result<Option<crate::extensions::RlmDeleteResult>> {
        let Some(runtime) = &self.rlm_runtime else {
            return Ok(None);
        };
        let active = runtime.list_subagents().await?.into_iter().any(|child| {
            (child.child_id == target
                || child.session_name == target
                || child.session_id.as_deref() == Some(target))
                && matches!(
                    child.status,
                    crate::extensions::RlmChildStatus::Queued
                        | crate::extensions::RlmChildStatus::Running
                )
        });
        if active {
            return Err(crate::error::MimirError::Protocol(
                "running RLM subagents must be cancelled before deletion".into(),
            ));
        }
        runtime.delete_subagent(target).await.map(Some)
    }

    /// Registers one lazy persistent Python kernel for this runtime session.
    /// Process execution must be explicitly enabled by policy.
    ///
    /// # Errors
    ///
    /// Returns a validation or policy error for an unsafe session identifier,
    /// unavailable Python executable, or missing process authority.
    pub fn register_ipython_kernel(
        &mut self,
        workspace: &Path,
        state_root: &Path,
        session: &str,
        policy: ToolPolicy,
    ) -> Result<(), ToolError> {
        self.deny_plan_registration("IPython")?;
        let tool = ipython::IpythonTool::new(workspace, state_root, session, policy)?;
        if self.tools.contains_key("ipython") {
            return Err(ToolError::Execution {
                tool: "ipython".into(),
                message: "tool name conflicts with an existing tool".into(),
            });
        }
        self.register(tool);
        Ok(())
    }

    /// Registers explicit natural-language memory for the parent runtime.
    ///
    /// # Errors
    ///
    /// Returns an error in plan mode or when the tool name is already in use.
    pub fn register_remember_tool(
        &mut self,
        state_root: &Path,
        session: &str,
    ) -> Result<(), ToolError> {
        self.deny_plan_registration("durable memory")?;
        if self.tools.contains_key("remember") {
            return Err(ToolError::Execution {
                tool: "remember".into(),
                message: "tool name conflicts with an existing tool".into(),
            });
        }
        self.register(memory::RememberTool::new(
            state_root,
            self.workspace_root.as_ref(),
            session,
        ));
        Ok(())
    }

    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools.values().map(|tool| tool.definition()).collect()
    }

    /// Retains only explicitly selected tool names and returns requested names
    /// that were not registered. This keeps CLI allowlists fail-closed after
    /// extensions and remote MCP tools have been discovered.
    pub fn retain_named(&mut self, allowed: &BTreeSet<String>) -> Vec<String> {
        self.tools.retain(|name, _| allowed.contains(name));
        allowed
            .iter()
            .filter(|name| !self.tools.contains_key(*name))
            .cloned()
            .collect()
    }

    /// Executes a registered tool after its adapter validates the input.
    ///
    /// # Errors
    ///
    /// Returns a typed error for unknown tools, invalid arguments, denied policy, or execution failure.
    pub async fn execute(&self, name: &str, input: Value) -> Result<ToolObservation, ToolError> {
        self.enforce_mode(name)?;
        let tool = self.tools.get(name).ok_or_else(|| ToolError::Execution {
            tool: name.into(),
            message: "tool is not registered".into(),
        })?;
        tool.execute(input).await
    }

    /// Executes a registered tool while propagating runtime cancellation to
    /// tools that own long-lived subprocesses.
    ///
    /// # Errors
    ///
    /// Returns a typed error for unknown tools, invalid input, policy denial,
    /// cancellation, or tool execution failure.
    pub async fn execute_cancellable(
        &self,
        name: &str,
        input: Value,
        cancellation: &CancellationToken,
    ) -> Result<ToolObservation, ToolError> {
        self.enforce_mode(name)?;
        let tool = self.tools.get(name).ok_or_else(|| ToolError::Execution {
            tool: name.into(),
            message: "tool is not registered".into(),
        })?;
        tool.execute_cancellable(input, cancellation).await
    }

    fn enforce_mode(&self, name: &str) -> Result<(), ToolError> {
        if self.agent_mode == AgentMode::Plan
            && !matches!(
                name,
                "read_file" | "list_files" | "search" | "ask_user" | "write_plan"
            )
        {
            return Err(ToolError::Disabled { tool: name.into() });
        }
        Ok(())
    }

    fn deny_plan_registration(&self, capability: &str) -> Result<(), ToolError> {
        if self.agent_mode.is_plan() {
            return Err(ToolError::Execution {
                tool: "plan_mode".into(),
                message: format!("{capability} are disabled in plan mode"),
            });
        }
        Ok(())
    }

    /// Returns the unresolved structured clarification for the active plan session.
    ///
    /// # Errors
    ///
    /// Returns a persistence error when private plan state cannot be read.
    pub async fn pending_plan_question(&self) -> Result<Option<ClarifyingQuestion>, ToolError> {
        let Some(context) = self.plan_context() else {
            return Ok(None);
        };
        context.pending_question().await
    }

    /// Clears the active plan session's pending clarification after a user answer.
    ///
    /// # Errors
    ///
    /// Returns a persistence error when private plan state cannot be written.
    pub async fn clear_pending_plan_question(&self) -> Result<(), ToolError> {
        if let Some(context) = self.plan_context() {
            context.clear_pending_question().await?;
        }
        Ok(())
    }

    /// Returns the validated regular plan artifact bound to the active plan session.
    ///
    /// # Errors
    ///
    /// Returns a policy or I/O error when the bound artifact is unsafe or unreadable.
    pub async fn plan_artifact(&self) -> Result<Option<PathBuf>, ToolError> {
        let Some(context) = self.plan_context() else {
            return Ok(None);
        };
        context.validated_artifact().await
    }

    /// Records that the bound plan was handed to a rebuilt implementation runtime.
    ///
    /// # Errors
    ///
    /// Returns a persistence error when no plan is bound or state cannot be written.
    pub async fn mark_plan_handed_off(&self) -> Result<(), ToolError> {
        let Some(context) = self.plan_context() else {
            return Err(ToolError::Disabled {
                tool: "write_plan".into(),
            });
        };
        context.mark_handed_off().await
    }

    fn plan_context(&self) -> Option<Arc<PlanContextStore>> {
        self.plan_context.clone()
    }
}

fn parse_input<T: for<'de> Deserialize<'de>>(tool: &str, input: Value) -> Result<T, ToolError> {
    serde_json::from_value(input).map_err(|error| ToolError::InvalidArguments {
        tool: tool.into(),
        message: error.to_string(),
    })
}

fn object_schema(properties: &Value, required: &[&str]) -> Value {
    serde_json::json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false
    })
}

fn truncate_utf8(value: &str, limit: usize) -> (String, bool) {
    if value.len() <= limit {
        return (value.to_owned(), false);
    }
    let mut end = limit.min(value.len());
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    (value[..end].to_owned(), true)
}
