use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    io::{self, BufRead, IsTerminal, Write},
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use clap::{Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
#[cfg(unix)]
use tokio::io::AsyncWriteExt;
use tokio::io::{AsyncBufReadExt, AsyncReadExt};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{
    acp::serve_acp,
    atomic::canonical_state_root,
    auth::{
        AuthCredential, AuthStore, CredentialType, DeviceAuthorization, OAuthProvider,
        PendingOAuth, RefreshingOAuthProvider, refresh_stored_oauth_if_expired,
        resolve_credential_typed,
    },
    config::ProviderConfig,
    daemon::{
        AgentMessageDelivery, AgentMessageRequest, ClientRequest, DaemonClient, DaemonConfig,
        DaemonError, DaemonServer, PromptHandler, PromptRequest, PublicDaemonCommand,
        PublicImageContent, ScheduledPromptDelivery, ServerResponse,
    },
    diagnostics::{
        DiagnosticAnalysisInput, DiagnosticConfiguration, DiagnosticManifest, DiagnosticPrivacy,
        RuntimeDiagnosticRunCollector, append_analysis, diagnostics_root, list_runs, load_bundle,
        query_events, replay_bundle,
    },
    error::{MimirError, Result},
    extensions::{
        AgentRuntimeChildExecutor, AuthStoreModelCatalog, AuthenticatedModelCatalog, Capability,
        CatalogEntry, DiscoveredResourcePaths, ExtensionCatalog, ExtensionManager,
        ExtensionManifest, ExtensionPackageManager, FlagKind, HostLimits, HostRequest,
        JsonLineExtensionHost, ManifestSource, ResourceDiscoveryReason, RlmChildRuntimePolicy,
        RlmChildToolRegistryFactory, RlmExecutionRequest, RlmLimits, RlmModel, RlmProviderFactory,
        RlmRuntime, RlmRuntimeLimits, RlmStore, RuntimeLimits,
    },
    mcp::{
        McpAuthCoordinator, McpCatalogHttp, McpCatalogServer, McpCatalogStdio,
        McpOAuthAuthorization, McpOAuthClient, McpOAuthCodeReceiver, McpServerCatalog,
        McpToolCallOutput, builtin_mcp_catalog, connect_catalog_client,
    },
    migration::{MigratedRuntimeState, StateMigrator},
    model::{Content, Message, ModelResponse, Role, StopReason, ThinkingLevel},
    observation::{ObservedSessionOutput, SessionObservation, SessionObserver},
    orchestration::{
        GoalStore, HeartbeatDeliveryMode, HeartbeatManagementAction, Schedule, ScheduleKind,
        ScheduleSource, ScheduleStore,
    },
    provider::{
        AnthropicCredentialKind, AnthropicProvider, BedrockProvider, CodexProvider, FakeProvider,
        GoogleAdcResolver, GoogleProvider, MistralProvider, OpenAiProvider, Provider,
        ProviderError, ResponsesProvider, VertexProvider,
        cloudflare::{CloudflareConfig, CloudflareProvider},
        registry::{ModelDefinition, ProviderRegistry, RuntimeSupport, model_catalog},
        resolve_bedrock_region,
    },
    refinement::{self, RefineOptions},
    resources::{
        ResourceLoader, ResourceLoaderOptions, Resources, expand_prompt_template,
        load_migrated_skills,
    },
    rpc::{RpcImageContent, RpcUserInput},
    runtime::{AgentRuntime, EventSink, QueueMode, RuntimeConfig, RuntimeEvent},
    session::{
        FileSessionStore, InMemorySessionStore, SessionPayload, SessionRecord, SessionStore,
    },
    session_compat::{
        ReferenceSessionMetadata, export_jsonl, prepare_share_payload, prepare_switch_session,
    },
    skills::SkillRuntime,
    tools::{
        AgentMode, BashResult, BashRunner, ObservationStatus, PlanContextStore, ToolPolicy,
        ToolRegistry, WorkspaceApprovalStore,
    },
    tui::{
        AutonomousLimits, AutonomousState, TuiResourceSnapshot, TuiRuntimeFactory,
        load_tui_agent_mode, load_tui_fast_mode, load_tui_rlm_max_depth, run_tui_with_autonomous,
    },
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum OutputMode {
    Text,
    Json,
    Rpc,
    Acp,
    Daemon,
}

impl OutputMode {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Json => "json",
            Self::Rpc => "rpc",
            Self::Acp => "acp",
            Self::Daemon => "daemon",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ThinkingArg {
    Off,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum AgentModeArg {
    Default,
    Plan,
    Auto,
}

impl From<AgentModeArg> for AgentMode {
    fn from(value: AgentModeArg) -> Self {
        match value {
            AgentModeArg::Default => Self::Default,
            AgentModeArg::Plan => Self::Plan,
            AgentModeArg::Auto => Self::Auto,
        }
    }
}

impl From<AgentMode> for AgentModeArg {
    fn from(value: AgentMode) -> Self {
        match value {
            AgentMode::Default => Self::Default,
            AgentMode::Plan => Self::Plan,
            AgentMode::Auto => Self::Auto,
        }
    }
}

impl From<ThinkingArg> for ThinkingLevel {
    fn from(value: ThinkingArg) -> Self {
        match value {
            ThinkingArg::Off => Self::Off,
            ThinkingArg::Minimal => Self::Minimal,
            ThinkingArg::Low => Self::Low,
            ThinkingArg::Medium => Self::Medium,
            ThinkingArg::High => Self::High,
            ThinkingArg::Xhigh => Self::Xhigh,
            ThinkingArg::Max => Self::Max,
        }
    }
}

fn default_state_dir() -> PathBuf {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .filter(|home| home.is_absolute())
        .map_or_else(|| PathBuf::from(".mimir"), |home| home.join(".mimir"))
}

#[derive(Debug, Parser)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "flat compatibility flags are intentionally represented by clap as booleans"
)]
#[command(name = "mimir", version, about = "Fast, bounded Rust agent harness")]
pub struct Cli {
    #[arg(long)]
    provider: Option<String>,
    #[arg(long, env = "MIMIR_MODEL")]
    model: Option<String>,
    #[arg(long)]
    base_url: Option<String>,
    #[arg(long = "workspace", visible_alias = "cwd", default_value = ".")]
    workspace: PathBuf,
    #[arg(long, env = "MIMIR_STATE_DIR", default_value_os_t = default_state_dir())]
    state_dir: PathBuf,
    #[arg(long, default_value = "default")]
    session: String,
    #[arg(short = 'c', long = "continue", conflicts_with_all = ["resume", "fork", "no_session"])]
    continue_session: bool,
    #[arg(short = 'r', long, value_name = "SESSION", num_args = 0..=1, default_missing_value = "", conflicts_with_all = ["continue_session", "fork", "no_session"])]
    resume: Option<String>,
    #[arg(long, value_name = "SESSION", conflicts_with_all = ["continue_session", "resume", "no_session"])]
    fork: Option<String>,
    #[arg(long, value_name = "DIR")]
    session_dir: Option<PathBuf>,
    #[arg(long, conflicts_with_all = ["continue_session", "resume", "fork", "session_dir"])]
    no_session: bool,
    #[arg(long, visible_alias = "mode", value_enum, default_value = "text")]
    output: OutputMode,
    #[arg(long)]
    allow_process: bool,
    #[arg(
        long = "agent-mode",
        value_enum,
        help = "Agent mode: default asks before writes, plan permits only inspection and one plan artifact, auto runs immediately"
    )]
    agent_mode: Option<AgentModeArg>,
    #[arg(long, value_delimiter = ',')]
    allowed_programs: Vec<String>,
    #[arg(long, value_delimiter = ',', conflicts_with = "no_tools")]
    tools: Vec<String>,
    #[arg(long, conflicts_with = "tools")]
    no_tools: bool,
    #[arg(long)]
    no_builtin_tools: bool,
    #[arg(short = 'e', long = "extension", value_name = "PATH")]
    extension: Vec<PathBuf>,
    #[arg(long)]
    no_extensions: bool,
    #[arg(long)]
    no_context_files: bool,
    #[arg(long)]
    no_skills: bool,
    #[arg(long)]
    no_prompt_templates: bool,
    #[arg(long)]
    no_themes: bool,
    #[arg(long, value_name = "PATH")]
    skill: Vec<PathBuf>,
    #[arg(long, value_name = "PATH")]
    prompt_template: Vec<PathBuf>,
    #[arg(long, value_name = "PATH")]
    theme: Vec<PathBuf>,
    #[arg(long, value_enum)]
    thinking: Option<ThinkingArg>,
    #[arg(long, value_name = "KEY")]
    api_key: Option<String>,
    #[arg(long, value_name = "TEXT")]
    system_prompt: Option<String>,
    #[arg(long, value_name = "TEXT")]
    append_system_prompt: Vec<String>,
    #[arg(
        long = "extension-flag",
        value_name = "NAME=VALUE",
        help = "Set a registered extension flag (repeatable)"
    )]
    extension_flags: Vec<String>,
    #[arg(long, value_delimiter = ',')]
    models: Vec<String>,
    #[arg(long)]
    offline: bool,
    #[arg(long)]
    verbose: bool,
    #[arg(
        long,
        env = "MIMIR_PROVIDER_TIMEOUT_SECONDS",
        default_value_t = 900,
        value_parser = parse_positive_u64,
        help = "Maximum time to wait for one provider request before treating it as unavailable"
    )]
    provider_timeout_seconds: u64,
    #[arg(
        long,
        env = "MIMIR_MAX_TURNS",
        default_value_t = 64,
        value_parser = parse_positive_u32,
        help = "Maximum provider turns in one prompt before pausing; send another prompt to continue"
    )]
    max_turns: u32,
    #[arg(
        long,
        env = "MIMIR_MAX_RUN_TOKENS",
        default_value_t = 1_000_000,
        value_parser = parse_positive_u64,
        help = "Maximum fresh-input plus output tokens in one prompt before pausing"
    )]
    max_run_tokens: u64,
    #[arg(long = "socket", visible_alias = "daemon-socket", value_name = "PATH")]
    socket: Option<PathBuf>,
    #[arg(long)]
    autonomous: bool,
    #[arg(long, value_parser = parse_positive_u32)]
    autonomous_max_continuations: Option<u32>,
    #[arg(long, value_parser = parse_positive_u32)]
    autonomous_max_turns: Option<u32>,
    #[arg(long, value_parser = parse_positive_u64)]
    autonomous_max_tokens: Option<u64>,
    #[arg(long, value_parser = parse_positive_u64)]
    autonomous_timeout_ms: Option<u64>,
    #[arg(long, value_name = "COMMAND")]
    autonomous_gate: Vec<String>,
    #[arg(long, value_parser = parse_positive_u32)]
    autonomous_gate_retries: Option<u32>,
    #[arg(long, value_parser = parse_positive_u64)]
    autonomous_gate_timeout_ms: Option<u64>,
    #[arg(long, value_name = "OBJECTIVE")]
    goal: Option<String>,
    #[arg(long, value_parser = parse_positive_u64, requires = "goal")]
    goal_token_budget: Option<u64>,
    #[arg(long = "fake-response")]
    fake_responses: Vec<String>,
    #[arg(long, default_value_t = 0, hide = true)]
    fake_delay_ms: u64,
    #[arg(long, default_value_t = 0, hide = true)]
    fake_retryable_failures: u32,
    #[arg(short = 'p', long = "print", value_name = "PROMPT", num_args = 0..=1, default_missing_value = "")]
    prompt: Option<String>,
    #[arg(
        long,
        help = "Use the line-oriented fallback instead of the full-screen TUI"
    )]
    no_tui: bool,
    #[command(subcommand)]
    command: Option<Command>,
    #[arg(
        value_name = "MESSAGE",
        trailing_var_arg = true,
        allow_hyphen_values = true
    )]
    prompt_segments: Vec<String>,
}

#[derive(Debug, Clone)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "runtime launch policy mirrors the explicit CLI compatibility switches"
)]
struct RuntimeBuildConfig {
    provider: String,
    model: String,
    base_url: Option<String>,
    workspace: PathBuf,
    state_dir: PathBuf,
    session_dir: Option<PathBuf>,
    no_session: bool,
    allow_process: bool,
    agent_mode: AgentMode,
    allowed_programs: Vec<String>,
    tool_allowlist: Option<BTreeSet<String>>,
    no_builtin_tools: bool,
    extension_paths: Vec<PathBuf>,
    no_extensions: bool,
    no_context_files: bool,
    no_skills: bool,
    no_prompt_templates: bool,
    no_themes: bool,
    skill_paths: Vec<PathBuf>,
    prompt_template_paths: Vec<PathBuf>,
    theme_paths: Vec<PathBuf>,
    thinking: Option<ThinkingLevel>,
    api_key: Option<String>,
    system_prompt: Option<String>,
    append_system_prompt: Vec<String>,
    extension_flags: Vec<String>,
    offline: bool,
    verbose: bool,
    provider_timeout_seconds: u64,
    max_turns: u32,
    max_run_tokens: u64,
    autonomous_limits: Option<AutonomousLimits>,
    fake_responses: Vec<String>,
    fake_delay_ms: u64,
    fake_retryable_failures: u32,
    provider_explicit: bool,
    model_explicit: bool,
    default_thinking_level: ThinkingLevel,
}

impl RuntimeBuildConfig {
    fn from_cli(cli: &Cli) -> Self {
        let provider = resolved_cli_provider(cli);
        Self {
            provider: provider.clone(),
            model: resolved_cli_model(cli, &provider),
            base_url: resolved_cli_base_url(cli, &provider),
            workspace: cli.workspace.clone(),
            state_dir: cli.state_dir.clone(),
            session_dir: cli.session_dir.clone(),
            no_session: cli.no_session,
            allow_process: cli.allow_process,
            agent_mode: cli.agent_mode.map(Into::into).unwrap_or_default(),
            allowed_programs: cli.allowed_programs.clone(),
            tool_allowlist: if cli.no_tools {
                Some(BTreeSet::new())
            } else if cli.tools.is_empty() {
                None
            } else {
                Some(cli.tools.iter().cloned().collect())
            },
            no_builtin_tools: cli.no_builtin_tools,
            extension_paths: cli.extension.clone(),
            no_extensions: cli.no_extensions,
            no_context_files: cli.no_context_files,
            no_skills: cli.no_skills,
            no_prompt_templates: cli.no_prompt_templates,
            no_themes: cli.no_themes,
            skill_paths: cli.skill.clone(),
            prompt_template_paths: cli.prompt_template.clone(),
            theme_paths: cli.theme.clone(),
            thinking: cli.thinking.map(Into::into),
            api_key: cli.api_key.clone(),
            system_prompt: cli.system_prompt.clone(),
            append_system_prompt: cli.append_system_prompt.clone(),
            extension_flags: cli.extension_flags.clone(),
            offline: cli.offline,
            verbose: cli.verbose,
            provider_timeout_seconds: cli.provider_timeout_seconds,
            max_turns: cli.max_turns,
            max_run_tokens: cli.max_run_tokens,
            autonomous_limits: cli_autonomous_limits(cli),
            fake_responses: cli.fake_responses.clone(),
            fake_delay_ms: cli.fake_delay_ms,
            fake_retryable_failures: cli.fake_retryable_failures,
            provider_explicit: cli.provider.is_some(),
            model_explicit: cli.model.is_some(),
            default_thinking_level: ThinkingLevel::Off,
        }
    }
}

fn parse_positive_u32(value: &str) -> std::result::Result<u32, String> {
    let parsed = value
        .parse::<u32>()
        .map_err(|_| "must be a positive integer".to_owned())?;
    (parsed > 0)
        .then_some(parsed)
        .ok_or_else(|| "must be a positive integer".to_owned())
}

fn parse_positive_u64(value: &str) -> std::result::Result<u64, String> {
    let parsed = value
        .parse::<u64>()
        .map_err(|_| "must be a positive integer".to_owned())?;
    (parsed > 0)
        .then_some(parsed)
        .ok_or_else(|| "must be a positive integer".to_owned())
}

fn resolve_extension_flags(
    assignments: &[String],
    descriptors: &[crate::extensions::FlagDescriptor],
) -> Result<Vec<(String, Value)>> {
    const MAX_FLAG_NAME_BYTES: usize = 128;
    const MAX_FLAG_VALUE_BYTES: usize = 16 * 1024;
    let mut seen = BTreeSet::new();
    let mut resolved = Vec::with_capacity(assignments.len());
    for assignment in assignments {
        let (name, raw_value) = assignment.split_once('=').ok_or_else(|| {
            MimirError::Configuration(
                "--extension-flag must use registered-name=value syntax".into(),
            )
        })?;
        if name.is_empty()
            || name.len() > MAX_FLAG_NAME_BYTES
            || raw_value.len() > MAX_FLAG_VALUE_BYTES
            || name.contains('\0')
            || raw_value.contains('\0')
        {
            return Err(MimirError::Configuration(
                "--extension-flag name or value exceeds its bounded format".into(),
            ));
        }
        if !seen.insert(name) {
            return Err(MimirError::Configuration(format!(
                "duplicate extension flag: {name}"
            )));
        }
        let descriptor = descriptors
            .iter()
            .find(|descriptor| descriptor.name == name)
            .ok_or_else(|| MimirError::Configuration(format!("unknown extension flag: {name}")))?;
        let value = match descriptor.kind {
            FlagKind::Boolean => match raw_value {
                "true" => Value::Bool(true),
                "false" => Value::Bool(false),
                _ => {
                    return Err(MimirError::Configuration(format!(
                        "extension flag '{name}' requires true or false"
                    )));
                }
            },
            FlagKind::String => Value::String(raw_value.to_owned()),
        };
        resolved.push((name.to_owned(), value));
    }
    Ok(resolved)
}

fn cli_autonomous_limits(cli: &Cli) -> Option<AutonomousLimits> {
    let enabled = cli.autonomous
        || cli.autonomous_max_continuations.is_some()
        || cli.autonomous_max_turns.is_some()
        || cli.autonomous_max_tokens.is_some()
        || cli.autonomous_timeout_ms.is_some()
        || !cli.autonomous_gate.is_empty()
        || cli.autonomous_gate_retries.is_some()
        || cli.autonomous_gate_timeout_ms.is_some();
    enabled.then(|| {
        let defaults = AutonomousLimits::default();
        AutonomousLimits {
            max_continuations: cli
                .autonomous_max_continuations
                .unwrap_or(defaults.max_continuations),
            max_turns: cli.autonomous_max_turns.unwrap_or(defaults.max_turns),
            max_tokens: cli.autonomous_max_tokens.unwrap_or(defaults.max_tokens),
            timeout: std::time::Duration::from_millis(cli.autonomous_timeout_ms.unwrap_or_else(
                || u64::try_from(defaults.timeout.as_millis()).unwrap_or(u64::MAX),
            )),
        }
    })
}

fn resolved_cli_provider(cli: &Cli) -> String {
    cli.provider.clone().unwrap_or_else(|| "anthropic".into())
}

fn resolved_cli_model(cli: &Cli, provider: &str) -> String {
    cli.model.clone().unwrap_or_else(|| {
        ProviderRegistry::builtin()
            .get(provider)
            .and_then(|provider| provider.default_model)
            .unwrap_or("gpt-5-mini")
            .to_owned()
    })
}

fn resolved_cli_base_url(cli: &Cli, provider: &str) -> Option<String> {
    cli.base_url.clone().or_else(|| {
        let env_var = match provider {
            "openai" | "openai-codex" => "OPENAI_BASE_URL",
            "anthropic" => "ANTHROPIC_BASE_URL",
            "google" => "GEMINI_BASE_URL",
            "azure-openai-responses" => "AZURE_OPENAI_BASE_URL",
            _ => return None,
        };
        std::env::var(env_var)
            .ok()
            .filter(|value| !value.trim().is_empty())
    })
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Validate paths and report credential presence without revealing it.
    Doctor,
    /// Store an API key or start a supported OAuth login flow.
    Login {
        #[arg(default_value = "anthropic")]
        provider: String,
        #[arg(long, hide_env_values = true, conflicts_with = "api_key_stdin")]
        api_key: Option<String>,
        #[arg(
            long,
            help = "Read the API key from stdin to avoid exposing it in argv"
        )]
        api_key_stdin: bool,
        #[arg(long)]
        enterprise_domain: Option<String>,
    },
    /// Remove credentials previously saved by login.
    Logout { provider: String },
    /// Inspect stored authentication without exposing secrets.
    Auth {
        #[command(subcommand)]
        action: AuthCommand,
    },
    /// List built-in model providers and authentication methods.
    Providers,
    /// Inspect the native model catalog.
    Model {
        #[command(subcommand)]
        action: ModelCommand,
    },
    /// Search active and saved agents.
    Agents,
    /// List active agents, optionally including saved sessions.
    List {
        #[arg(short = 'a', long)]
        all: bool,
    },
    /// Attach the interactive runtime to a saved agent.
    Attach { agent: String },
    /// Stop an active daemon agent.
    Stop { agent: String },
    /// Rename an active daemon agent.
    Rename {
        agent: String,
        #[arg(required = true, trailing_var_arg = true)]
        name: Vec<String>,
    },
    /// Send a message between active daemon agents.
    Send {
        #[arg(long)]
        from: Option<String>,
        #[arg(long, conflicts_with = "follow_up")]
        steer: bool,
        #[arg(long = "follow-up", conflicts_with = "steer")]
        follow_up: bool,
        #[arg(long)]
        json: bool,
        agent: String,
        #[arg(long = "message", conflicts_with = "message_parts")]
        explicit_message: Option<String>,
        #[arg(value_name = "MESSAGE")]
        message_parts: Vec<String>,
    },
    /// Show native daemon status.
    Status,
    /// Stop the native daemon and its active agents.
    Shutdown {
        #[arg(short = 'f', long)]
        force: bool,
    },
    /// Inspect the trusted self-update configuration.
    Update {
        #[arg(value_enum, default_value = "status")]
        action: UpdateAction,
        #[arg(long)]
        force: bool,
    },
    /// Configure resources interactively, or print machine-readable state.
    Config {
        #[command(subcommand)]
        action: Option<ConfigCommand>,
    },
    /// Manage audited local extension packages.
    Package {
        #[command(subcommand)]
        action: PackageCommand,
    },
    /// Manage the durable local agent daemon.
    Daemon {
        #[command(subcommand)]
        action: DaemonCommand,
    },
    /// Discover, enable, disable, and invoke Rust-native extensions.
    Extension {
        #[command(subcommand)]
        action: ExtensionCommand,
    },
    /// Inspect and mutate persistent extension/RLM state.
    Rlm {
        #[command(subcommand)]
        action: RlmCommand,
    },
    /// Plan, apply, or roll back migration from the legacy harness state.
    Migrate {
        #[command(subcommand)]
        action: MigrateCommand,
    },
    /// Inspect the selected durable session.
    Session {
        #[command(subcommand)]
        action: SessionCommand,
    },
    /// Inspect, export, annotate, or safely replay local diagnostic evidence.
    Diagnose {
        #[command(subcommand)]
        action: DiagnoseCommand,
    },
    /// Configure, authenticate, and invoke local or remote MCP servers.
    Mcp {
        #[command(subcommand)]
        action: McpCommand,
    },
    /// Manage the one active durable goal.
    Goal {
        #[command(subcommand)]
        action: GoalCommand,
    },
    /// Manage durable scheduled prompts.
    Schedule {
        #[command(subcommand)]
        action: ScheduleCommand,
    },
    /// Measure a bounded local operation and print machine-readable timings.
    Benchmark {
        #[command(subcommand)]
        action: BenchmarkCommand,
    },
}

#[derive(Debug, Subcommand)]
enum AuthCommand {
    Status,
}

#[derive(Debug, Subcommand)]
enum ModelCommand {
    /// List available models, optionally filtered by provider, id, or API.
    List { search: Option<String> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum UpdateAction {
    Status,
    Check,
    Install,
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    Show,
    List,
}

#[derive(Debug, Subcommand)]
enum PackageCommand {
    List,
    Install {
        source: String,
        #[arg(long)]
        local: bool,
    },
    Remove {
        name: String,
        #[arg(long)]
        local: bool,
    },
    Update {
        name: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum DaemonCommand {
    Start,
    #[command(hide = true)]
    Serve,
    Status,
    Prompt {
        prompt: String,
    },
    Stop,
}

#[derive(Debug, Subcommand)]
enum ExtensionCommand {
    List,
    Enable {
        name: String,
    },
    Disable {
        name: String,
    },
    Invoke {
        name: String,
        command: String,
        #[arg(long, default_value = "{}")]
        payload: String,
    },
    Registrations,
    Run {
        command: String,
        #[arg(default_value = "")]
        args: String,
    },
}

#[derive(Debug, Subcommand)]
enum RlmCommand {
    Get {
        extension: String,
        namespace: String,
        key: String,
    },
    Put {
        extension: String,
        namespace: String,
        key: String,
        value: String,
    },
    List {
        extension: String,
        namespace: String,
    },
    Delete {
        extension: String,
        namespace: String,
        key: String,
    },
}

#[derive(Debug, Subcommand)]
enum MigrateCommand {
    /// Inspect legacy state and emit a secret-redacted migration plan.
    Plan {
        #[arg(long)]
        legacy_root: PathBuf,
    },
    /// Apply a fresh migration plan using backups and a rollback journal.
    Apply {
        #[arg(long)]
        legacy_root: PathBuf,
    },
    /// Restore state from a journal produced by `migrate apply`.
    Rollback {
        #[arg(long)]
        journal: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
enum SessionCommand {
    List,
    Show,
    /// Export the selected transcript as safe HTML or reference-compatible v3 JSONL.
    Export {
        output: PathBuf,
    },
    /// Import a reference or native transcript into the durable session store.
    Import {
        input: PathBuf,
        #[arg(long)]
        cwd: Option<PathBuf>,
    },
    /// Validate and plan a session switch without mutating durable state.
    Switch {
        input: PathBuf,
        #[arg(long)]
        cwd: Option<PathBuf>,
    },
    /// Prepare redacted metadata for the reference secret-gist share flow.
    Share {
        input: PathBuf,
        #[arg(long)]
        viewer_base: Option<String>,
        #[arg(long)]
        gist_id: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum DiagnoseCommand {
    /// List diagnostic runs, newest first.
    List,
    /// Show one complete redacted diagnostic bundle.
    Show { run_id: String },
    /// Filter one run's typed events.
    Query {
        run_id: String,
        #[arg(long)]
        kind: Option<String>,
        #[arg(long)]
        status: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Export one portable redacted JSON bundle.
    Export {
        run_id: String,
        #[arg(long)]
        output: Option<PathBuf>,
        #[arg(long)]
        redacted: bool,
    },
    /// Append an external harness assessment from a JSON file.
    Annotate {
        run_id: String,
        #[arg(long)]
        file: PathBuf,
    },
    /// Deterministically validate recorded evidence without executing tools or providers.
    Replay { run_id: String },
}

#[derive(Debug, Subcommand)]
enum McpCommand {
    List,
    Add {
        server: String,
        #[arg(long)]
        label: Option<String>,
        #[arg(long, conflicts_with_all = ["url", "builtin"])]
        program: Option<PathBuf>,
        #[arg(long, conflicts_with_all = ["program", "builtin"])]
        url: Option<String>,
        #[arg(
            long,
            conflicts_with_all = ["program", "url"],
            help = "Use a built-in definition (linear or notion)"
        )]
        builtin: bool,
        #[arg(long = "arg")]
        args: Vec<String>,
        #[arg(long = "env", value_name = "TARGET=SOURCE_ENV")]
        env: Vec<String>,
        #[arg(long)]
        oauth: bool,
        #[arg(long)]
        bearer_token_env: Option<String>,
        #[arg(long)]
        client_id: Option<String>,
        #[arg(long = "scope")]
        scopes: Vec<String>,
        #[arg(long)]
        disabled: bool,
    },
    Remove {
        server: String,
    },
    Status {
        server: Option<String>,
    },
    AuthStatus {
        server: Option<String>,
    },
    Login {
        server: String,
        #[arg(long, hide_env_values = true, conflicts_with = "api_key_stdin")]
        api_key: Option<String>,
        #[arg(
            long,
            help = "Read the API key from stdin to avoid exposing it in argv"
        )]
        api_key_stdin: bool,
        #[arg(long, default_value = "http://127.0.0.1:53700/callback")]
        redirect_uri: String,
    },
    Logout {
        server: String,
    },
    Tools {
        server: String,
    },
    Call {
        server: String,
        tool: String,
        #[arg(long, default_value = "{}")]
        arguments: String,
    },
}

#[derive(Debug, Subcommand)]
enum GoalCommand {
    Set {
        objective: String,
        #[arg(long)]
        token_budget: Option<u64>,
    },
    Show,
    Clear,
}

#[derive(Debug, Subcommand)]
enum ScheduleCommand {
    Add {
        name: String,
        prompt: String,
        #[arg(last = true, value_name = "MESSAGE", num_args = 1..)]
        message: Vec<String>,
        #[arg(long, help = "RFC 3339 timestamp; defaults to now")]
        at: Option<String>,
        #[arg(long)]
        every_seconds: Option<u64>,
        #[arg(long)]
        json: bool,
    },
    List {
        #[arg(short = 'a', long)]
        all: bool,
        agent: Option<String>,
        #[arg(long)]
        json: bool,
    },
    Cancel {
        id: Uuid,
    },
}

#[derive(Debug, Subcommand)]
enum BenchmarkCommand {
    Prompt { prompt: String },
}

#[derive(Debug, Serialize)]
struct EventEnvelope<'a> {
    schema_version: u16,
    #[serde(flatten)]
    event: &'a RuntimeEvent,
}

struct StdoutEventSink {
    enabled: bool,
}

#[async_trait]
impl EventSink for StdoutEventSink {
    async fn emit(&self, event: RuntimeEvent) {
        if self.enabled {
            let envelope = EventEnvelope {
                schema_version: 1,
                event: &event,
            };
            if let Ok(encoded) = serde_json::to_string(&envelope) {
                println!("{encoded}");
            }
        }
    }
}

#[derive(Debug, Deserialize)]
struct RpcRequest {
    jsonrpc: String,
    id: Value,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Debug, Deserialize)]
struct LegacyRpcRequest {
    #[serde(default)]
    id: Option<Value>,
    #[serde(rename = "type")]
    command: String,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    images: Vec<RpcImageContent>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default, rename = "sessionPath")]
    session_path: Option<String>,
    #[serde(default, rename = "parentSession")]
    parent_session: Option<String>,
    #[serde(default, rename = "entryId")]
    entry_id: Option<String>,
    #[serde(default, rename = "streamingBehavior")]
    streaming_behavior: Option<String>,
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default, rename = "outputPath")]
    output_path: Option<String>,
    #[serde(default, rename = "command")]
    shell_command: Option<String>,
    #[serde(default)]
    schedule: Option<String>,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default, rename = "jobId")]
    job_id: Option<String>,
    #[serde(default, rename = "includeInactive")]
    include_inactive: Option<bool>,
    #[serde(default, rename = "deliveryMode")]
    delivery_mode: Option<String>,
    #[serde(default)]
    action: Option<String>,
    #[serde(default, rename = "activeSessionId")]
    active_session_id: Option<String>,
    #[serde(default, rename = "targetActiveSessionId")]
    target_active_session_id: Option<String>,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default, rename = "modelId")]
    model_id: Option<String>,
    #[serde(default)]
    level: Option<String>,
    #[serde(default)]
    direction: Option<String>,
    #[serde(default, rename = "customInstructions")]
    custom_instructions: Option<String>,
    #[serde(default)]
    instructions: Option<String>,
    #[serde(default, rename = "rollbackId")]
    rollback_id: Option<String>,
    #[serde(default)]
    global: Option<bool>,
}

struct RpcSessionContext {
    runtime: Arc<AgentRuntime>,
    build: RuntimeBuildConfig,
    session_id: String,
    activity: Arc<tokio::sync::Mutex<RpcActivity>>,
    worker: Option<tokio::task::JoinHandle<()>>,
    control_worker: Option<tokio::task::JoinHandle<()>>,
    bash_runner: Arc<BashRunner>,
    bash_worker: Option<tokio::task::JoinHandle<()>>,
    observations: HashMap<String, RpcObservationWorker>,
}

struct RpcObservationWorker {
    cancellation: CancellationToken,
    worker: tokio::task::JoinHandle<()>,
}

#[derive(Default)]
struct RpcActivity {
    running: bool,
    bash_running: bool,
    control_running: Option<String>,
    follow_ups: VecDeque<Message>,
    follow_up_mode: QueueMode,
}

#[derive(Default)]
struct LegacyRpcEventSink {
    partial_text: tokio::sync::Mutex<String>,
}

#[allow(
    clippy::too_many_lines,
    reason = "the exhaustive RPC event mapping stays adjacent so wire lifecycle ordering remains auditable"
)]
#[async_trait]
impl EventSink for LegacyRpcEventSink {
    async fn emit(&self, event: RuntimeEvent) {
        let values = match event {
            RuntimeEvent::RunStarted => vec![json!({"type": "agent_start"})],
            RuntimeEvent::ProviderRequest {
                turn,
                estimated_context_tokens,
            } => {
                self.partial_text.lock().await.clear();
                vec![json!({
                    "type": "turn_start",
                    "turn": turn,
                    "estimated_context_tokens": estimated_context_tokens
                })]
            }
            RuntimeEvent::MessageStarted { message } => {
                vec![json!({"type": "message_start", "message": message})]
            }
            RuntimeEvent::MessageCompleted { message } => {
                vec![json!({"type": "message_end", "message": message})]
            }
            RuntimeEvent::TurnCompleted {
                message,
                tool_results,
            } => vec![json!({
                "type": "turn_end",
                "message": message,
                "toolResults": tool_results
            })],
            RuntimeEvent::PermissionRequested { request } => vec![json!({
                "type": "workspace_permission_required", "request": request
            })],
            RuntimeEvent::UserInputRequested { request } => vec![json!({
                "type": "user_input_requested", "request": request
            })],
            RuntimeEvent::TextDelta { text } => {
                let partial = {
                    let mut partial = self.partial_text.lock().await;
                    partial.push_str(&text);
                    partial.clone()
                };
                vec![json!({
                    "type": "message_update",
                    "message": {"role": "assistant", "content": [{"type": "text", "text": partial}]},
                    "assistantMessageEvent": {
                        "type": "text_delta",
                        "contentIndex": 0,
                        "delta": text,
                        "partial": {"role": "assistant", "content": [{"type": "text", "text": partial}]}
                    }
                })]
            }
            RuntimeEvent::AutoRetryStarted {
                attempt,
                max_attempts,
                delay_ms,
                error_message,
            } => vec![json!({
                "type": "auto_retry_start",
                "attempt": attempt,
                "maxAttempts": max_attempts,
                "delayMs": delay_ms,
                "errorMessage": error_message
            })],
            RuntimeEvent::AutoRetryFinished {
                success,
                attempt,
                final_error,
            } => vec![json!({
                "type": "auto_retry_end",
                "success": success,
                "attempt": attempt,
                "finalError": final_error
            })],
            RuntimeEvent::ToolStarted {
                id,
                name,
                arguments,
            } => vec![json!({
                "type": "tool_execution_start",
                "toolCallId": id,
                "toolName": name,
                "args": arguments
            })],
            RuntimeEvent::ToolUpdated {
                id,
                name,
                arguments,
                observation,
            } => vec![json!({
                "type": "tool_execution_update",
                "toolCallId": id,
                "toolName": name,
                "args": arguments,
                "partialResult": observation
            })],
            RuntimeEvent::ToolFinished {
                id,
                name,
                observation,
            } => vec![json!({
                "type": "tool_execution_end",
                "toolCallId": id,
                "toolName": name,
                "isError": observation.status == crate::tools::ObservationStatus::Error,
                "result": observation
            })],
            RuntimeEvent::Completed { .. } => Vec::new(),
            RuntimeEvent::Failed { message } => vec![json!({
                "type": "message_update",
                "assistantMessageEvent": {"type": "error", "reason": "error", "message": message}
            })],
            RuntimeEvent::BudgetPaused { pause } => vec![json!({
                "type": "budget_paused",
                "reason": pause.kind,
                "limit": pause.limit,
                "usage": pause.usage
            })],
            RuntimeEvent::ExtensionUi { extension, request } => vec![json!({
                "type": "extension_ui_request",
                "extension": extension,
                "request": request
            })],
            RuntimeEvent::ExtensionRendered { custom_type, lines } => vec![json!({
                "type": "extension_rendered",
                "customType": custom_type,
                "lines": lines
            })],
            RuntimeEvent::ExtensionError {
                extension_path,
                event,
                error,
            } => vec![json!({
                "type": "extension_error",
                "extensionPath": extension_path,
                "event": event,
                "error": error
            })],
            RuntimeEvent::SessionEvent { event } => vec![event],
        };
        for value in values {
            emit_rpc_value(&value);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedGateCommand {
    program: String,
    arguments: Vec<String>,
}

fn parse_gate_command(command: &str) -> Result<ParsedGateCommand> {
    if command.len() > 32 * 1024 {
        return Err(MimirError::Configuration(
            "autonomous gate command exceeds 32 KiB".into(),
        ));
    }
    let mut values = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut escaped = false;
    for character in command.chars() {
        if escaped {
            current.push(character);
            escaped = false;
            continue;
        }
        match (quote, character) {
            (Some('\''), '\'') | (Some('"'), '"') => quote = None,
            (Some('"') | None, '\\') => escaped = true,
            (None, '\'' | '"') => quote = Some(character),
            (None, value) if value.is_whitespace() => {
                if !current.is_empty() {
                    values.push(std::mem::take(&mut current));
                }
            }
            (Some(_) | None, value) => current.push(value),
        }
    }
    if escaped || quote.is_some() {
        return Err(MimirError::Configuration(
            "autonomous gate command has an unterminated quote or escape".into(),
        ));
    }
    if !current.is_empty() {
        values.push(current);
    }
    if values.is_empty() || values.len() > 65 || values.iter().any(|value| value.len() > 4096) {
        return Err(MimirError::Configuration(
            "autonomous gate requires one program and at most 64 bounded arguments".into(),
        ));
    }
    Ok(ParsedGateCommand {
        program: values.remove(0),
        arguments: values,
    })
}

fn validate_process_options(cli: &Cli) -> Result<()> {
    if cli.allow_process && cli.allowed_programs.is_empty() {
        return Err(MimirError::Configuration(
            "--allow-process requires an explicit non-empty --allowed-programs allowlist".into(),
        ));
    }
    if !cli.allow_process && !cli.allowed_programs.is_empty() {
        return Err(MimirError::Configuration(
            "--allowed-programs requires --allow-process".into(),
        ));
    }
    Ok(())
}

#[allow(
    clippy::too_many_lines,
    reason = "run option validation keeps cross-flag policy conflicts in one auditable boundary"
)]
fn validate_run_options(cli: &Cli) -> Result<()> {
    validate_process_options(cli)?;
    if cli.agent_mode == Some(AgentModeArg::Plan)
        && (cli_autonomous_limits(cli).is_some() || cli.goal.is_some())
    {
        return Err(MimirError::Configuration(
            "plan mode cannot run autonomous continuations or create a goal".into(),
        ));
    }
    if cli.socket.is_some() && matches!(cli.output, OutputMode::Rpc | OutputMode::Acp) {
        return Err(MimirError::Configuration(
            "--socket/--daemon-socket is only supported by text/json prompt mode".into(),
        ));
    }
    if cli.socket.is_some() && cli_autonomous_limits(cli).is_some() {
        return Err(MimirError::Configuration(
            "--socket cannot configure autonomous policy on an existing daemon runtime".into(),
        ));
    }
    if cli.socket.is_some()
        && (cli.continue_session
            || cli.resume.is_some()
            || cli.fork.is_some()
            || cli.session_dir.is_some()
            || cli.no_session)
    {
        return Err(MimirError::Configuration(
            "--socket supports an explicit --session id, not local session selection flags".into(),
        ));
    }
    if matches!(cli.output, OutputMode::Rpc | OutputMode::Acp)
        && (cli.prompt.is_some() || !cli.prompt_segments.is_empty())
    {
        return Err(MimirError::Configuration(
            "prompt arguments are not supported in RPC or ACP mode".into(),
        ));
    }
    if matches!(cli.output, OutputMode::Rpc | OutputMode::Acp) && has_prompt_file_arguments(cli) {
        return Err(MimirError::Configuration(
            "@file arguments are not supported in RPC or ACP mode".into(),
        ));
    }
    if !cli.autonomous_gate.is_empty() {
        if cli.autonomous_gate.len() > 32 {
            return Err(MimirError::Configuration(
                "at most 32 autonomous quality gates may be configured".into(),
            ));
        }
        if !cli.allow_process || cli.allowed_programs.is_empty() {
            return Err(MimirError::Configuration(
                "autonomous quality gates require --allow-process and an explicit --allowed-programs allowlist"
                    .into(),
            ));
        }
        for gate in &cli.autonomous_gate {
            let parsed = parse_gate_command(gate)?;
            if !cli
                .allowed_programs
                .iter()
                .any(|allowed| allowed == &parsed.program)
            {
                return Err(MimirError::Configuration(format!(
                    "autonomous gate program '{}' is not in --allowed-programs",
                    parsed.program
                )));
            }
        }
    } else if cli.autonomous_gate_retries.is_some() || cli.autonomous_gate_timeout_ms.is_some() {
        return Err(MimirError::Configuration(
            "autonomous gate retry/timeout options require --autonomous-gate".into(),
        ));
    }
    if cli_autonomous_limits(cli).is_some()
        && matches!(cli.output, OutputMode::Rpc | OutputMode::Acp)
    {
        return Err(MimirError::Configuration(
            "--autonomous is only supported by text/json run modes".into(),
        ));
    }
    if cli.no_session && cli.session != "default" {
        return Err(MimirError::Configuration(
            "--no-session cannot be combined with --session".into(),
        ));
    }
    if (cli.no_session && cli.output == OutputMode::Rpc)
        || (cli.session_dir.is_some() && matches!(cli.output, OutputMode::Rpc | OutputMode::Acp))
    {
        return Err(MimirError::Configuration(
            "--no-session and --session-dir are not supported by RPC/ACP modes".into(),
        ));
    }
    if cli.goal.as_ref().is_some_and(|goal| goal.trim().is_empty()) {
        return Err(MimirError::Configuration(
            "--goal requires a non-empty objective".into(),
        ));
    }
    if cli.goal.is_some()
        && (cli.continue_session || cli.resume.is_some() || cli.fork.is_some() || cli.no_session)
    {
        return Err(MimirError::Configuration(
            "--goal can only create a new durable root session".into(),
        ));
    }
    Ok(())
}

fn has_prompt_file_arguments(cli: &Cli) -> bool {
    cli.prompt
        .as_deref()
        .is_some_and(|value| value.starts_with('@') && value.len() > 1)
        || cli
            .prompt_segments
            .iter()
            .any(|value| value.starts_with('@') && value.len() > 1)
}

const MAX_PROMPT_FILE_BYTES: u64 = 8 * 1024 * 1024;

async fn expand_prompt_segment(workspace: &Path, segment: &str) -> Result<String> {
    let Some(file) = segment.strip_prefix('@').filter(|path| !path.is_empty()) else {
        return Ok(segment.to_owned());
    };
    let requested = PathBuf::from(file);
    let path = if requested.is_absolute() {
        requested
    } else {
        workspace.join(requested)
    };
    let bytes = read_bounded_regular_file(&path, MAX_PROMPT_FILE_BYTES).await?;
    let content = String::from_utf8(bytes).map_err(|_| {
        MimirError::Configuration(format!(
            "@file must be UTF-8 text in native Rust mode: {}",
            path.display()
        ))
    })?;
    let name = path.display().to_string();
    let name = name
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;");
    Ok(format!("<file name=\"{name}\">\n{content}\n</file>\n"))
}

async fn load_runtime_resources(
    build: &RuntimeBuildConfig,
    state: &Path,
    workspace: &Path,
) -> Result<Resources> {
    let mut package_paths = ExtensionPackageManager::new(state)?
        .list()
        .await?
        .into_iter()
        .map(|package| package.installed_path)
        .collect::<Vec<_>>();
    if let Some(local_state) = distinct_project_local_state(workspace, state) {
        package_paths.extend(
            ExtensionPackageManager::new(&local_state)?
                .list()
                .await?
                .into_iter()
                .map(|package| package.installed_path),
        );
    }
    package_paths.sort();
    package_paths.dedup();
    let user_dir = std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|path| path.join(".mimir/agent"));
    let options = ResourceLoaderOptions {
        user_dir,
        package_paths,
        explicit_skill_paths: build.skill_paths.clone(),
        explicit_prompt_template_paths: build.prompt_template_paths.clone(),
        explicit_theme_paths: build.theme_paths.clone(),
        discover_context_files: !build.no_context_files,
        discover_skills: !build.no_skills,
        discover_prompt_templates: !build.no_prompt_templates,
        discover_themes: !build.no_themes,
        ..ResourceLoaderOptions::default()
    };
    ResourceLoader::with_options(workspace, workspace, options)
        .and_then(|loader| loader.load())
        .map_err(|error| MimirError::Configuration(error.to_string()))
}

fn distinct_project_local_state(workspace: &Path, state: &Path) -> Option<PathBuf> {
    let local = workspace.join(".mimir");
    if !local.exists() {
        return None;
    }
    let local = std::fs::canonicalize(local).ok()?;
    let state = std::fs::canonicalize(state).unwrap_or_else(|_| state.to_path_buf());
    (local != state).then_some(local)
}

async fn prepare_initial_prompt(cli: &Cli) -> Result<Option<String>> {
    let workspace = std::fs::canonicalize(&cli.workspace).map_err(|error| {
        MimirError::Configuration(format!("workspace is inaccessible: {error}"))
    })?;
    let mut arguments = Vec::new();
    if let Some(prompt) = cli.prompt.as_deref().filter(|prompt| !prompt.is_empty()) {
        arguments.push(prompt.to_owned());
    }
    arguments.extend(cli.prompt_segments.iter().cloned());

    let mut files = Vec::new();
    let mut messages = Vec::new();
    for argument in arguments {
        if argument == "--" {
            continue;
        }
        if argument.starts_with('@') && argument.len() > 1 {
            files.push(expand_prompt_segment(&workspace, &argument).await?);
        } else {
            messages.push(argument);
        }
    }

    let wants_one_shot = cli.prompt.is_some() || !messages.is_empty() || !files.is_empty();
    let stdin = if wants_one_shot && !io::stdin().is_terminal() {
        let mut input = String::new();
        tokio::io::stdin().read_to_string(&mut input).await?;
        (!input.is_empty()).then_some(input)
    } else {
        None
    };
    let mut parts = Vec::new();
    parts.extend(stdin);
    parts.extend(files);
    if !messages.is_empty() {
        parts.push(messages.join(" "));
    }
    let Some(prompt) = (!parts.is_empty()).then(|| parts.join("")) else {
        return Ok(None);
    };
    let state = resolve_state_dir(&cli.state_dir)?;
    let resources =
        load_runtime_resources(&RuntimeBuildConfig::from_cli(cli), &state, &workspace).await?;
    Ok(Some(expand_prompt_template(
        &prompt,
        &resources.prompt_templates,
    )))
}

async fn run_socket_prompt(cli: &Cli, prompt: &str) -> Result<()> {
    let socket = cli
        .socket
        .as_deref()
        .ok_or_else(|| MimirError::Configuration("daemon socket path is missing".into()))?;
    let mut client = DaemonClient::connect(socket).await.map_err(daemon_error)?;
    let attached = client
        .request(ClientRequest::attach(&cli.session, "mimir-direct-cli"))
        .await
        .map_err(daemon_error)?;
    let ServerResponse::SessionAttached(attached) = attached else {
        return Err(MimirError::Protocol(
            "daemon returned an invalid attach response".into(),
        ));
    };
    let lease_id = attached.lease.lease_id.to_string();
    let response = client
        .request(ClientRequest::prompt(&lease_id, &cli.session, prompt))
        .await
        .map_err(daemon_error);
    let _ = client.request(ClientRequest::detach(&lease_id)).await;
    let response = response?;
    let ServerResponse::PromptCompleted(completed) = response else {
        return Err(MimirError::Protocol(
            "daemon returned an invalid prompt response".into(),
        ));
    };
    match cli.output {
        OutputMode::Text => println!("{}", completed.output),
        OutputMode::Json => print_json(&serde_json::to_value(completed)?)?,
        OutputMode::Rpc | OutputMode::Acp | OutputMode::Daemon => unreachable!("validated above"),
    }
    Ok(())
}

async fn create_run_session_store(
    build: &RuntimeBuildConfig,
    session_root: &Path,
    session_id: &str,
) -> Result<FileSessionStore> {
    if build.session_dir.is_some() {
        FileSessionStore::create_in_directory(session_root, session_id).await
    } else {
        FileSessionStore::create(session_root, session_id).await
    }
}

async fn list_run_session_ids(
    build: &RuntimeBuildConfig,
    session_root: &Path,
) -> Result<Vec<String>> {
    if build.session_dir.is_some() {
        FileSessionStore::list_ids_in_directory(session_root).await
    } else {
        FileSessionStore::list_ids(session_root).await
    }
}

async fn resolve_session_selector(
    build: &RuntimeBuildConfig,
    session_root: &Path,
    selector: &str,
) -> Result<String> {
    let ids = list_run_session_ids(build, session_root).await?;
    if ids.iter().any(|id| id == selector) {
        return Ok(selector.to_owned());
    }
    let mut prefixes = ids.iter().filter(|id| id.starts_with(selector));
    if let Some(first) = prefixes.next()
        && prefixes.next().is_none()
    {
        return Ok(first.clone());
    }
    if std::path::Path::new(selector).components().count() > 1 {
        return Err(MimirError::Configuration(
            "session path selectors are not supported; use an exact or unique session id".into(),
        ));
    }
    Err(MimirError::Configuration(format!(
        "session selector is missing or ambiguous: {selector}"
    )))
}

async fn latest_session_id(build: &RuntimeBuildConfig, session_root: &Path) -> Result<String> {
    let ids = list_run_session_ids(build, session_root).await?;
    let mut latest = None;
    for id in ids {
        let store = create_run_session_store(build, session_root, &id).await?;
        let modified = tokio::fs::metadata(store.path())
            .await?
            .modified()
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        if latest
            .as_ref()
            .is_none_or(|(_, latest_modified)| modified > *latest_modified)
        {
            latest = Some((id, modified));
        }
    }
    latest
        .map(|(id, _)| id)
        .ok_or_else(|| MimirError::Configuration("--continue requires an existing session".into()))
}

async fn prepare_run_session(cli: &mut Cli) -> Result<()> {
    let build = RuntimeBuildConfig::from_cli(cli);
    let state = resolve_state_dir(&build.state_dir)?;
    let session_root = resolve_session_root(&build, &state)?;
    if cli.continue_session {
        cli.session = latest_session_id(&build, &session_root).await?;
    } else if let Some(selector) = cli.resume.as_deref() {
        cli.session = if selector.trim().is_empty() {
            latest_session_id(&build, &session_root).await?
        } else {
            resolve_session_selector(&build, &session_root, selector).await?
        };
    } else if let Some(selector) = cli.fork.as_deref() {
        let source_id = resolve_session_selector(&build, &session_root, selector).await?;
        let source = create_run_session_store(&build, &session_root, &source_id).await?;
        let destination_id = format!("fork-{}", Uuid::new_v4().simple());
        let destination = create_run_session_store(&build, &session_root, &destination_id).await?;
        for record in source.load().await?.records {
            destination.append(record).await?;
        }
        destination
            .append(SessionRecord::new(SessionPayload::RuntimeEvent {
                name: "session_forked_from".into(),
                detail: source_id,
            }))
            .await?;
        cli.session = destination_id;
    }
    if let Some(goal) = cli.goal.as_deref() {
        if cli.session == "default" {
            cli.session = format!("goal-{}", Uuid::new_v4().simple());
        }
        GoalStore::new(&state)
            .create(goal, cli.goal_token_budget)
            .await?;
    }
    Ok(())
}

/// Parses the command line and runs the selected harness mode.
///
/// # Errors
///
/// Returns configuration, persistence, provider, tool, or protocol errors after sanitization.
pub async fn entrypoint() -> Result<()> {
    let _ = dotenvy::dotenv();
    let mut cli = Cli::parse();
    if let Some(Command::Attach { agent }) = cli.command.as_ref() {
        let agent = agent.clone();
        if cli.continue_session || cli.resume.is_some() || cli.fork.is_some() || cli.no_session {
            return Err(MimirError::Configuration(
                "attach cannot be combined with --resume, --continue, --fork, or --no-session"
                    .into(),
            ));
        }
        cli.command = None;
        cli.resume = Some(agent);
    }
    if let Some(command) = &cli.command {
        return run_management(&cli, command).await;
    }
    if cli.output == OutputMode::Daemon {
        if cli.prompt.is_some() || !cli.prompt_segments.is_empty() {
            return Err(MimirError::Configuration(
                "--mode daemon does not accept prompt arguments".into(),
            ));
        }
        let state = resolve_state_dir(&cli.state_dir)?;
        return run_daemon_management(&cli, &DaemonCommand::Serve, &state).await;
    }
    validate_run_options(&cli)?;
    let initial_prompt = if matches!(cli.output, OutputMode::Rpc | OutputMode::Acp) {
        None
    } else {
        prepare_initial_prompt(&cli).await?
    };
    if cli.prompt.is_some() && initial_prompt.is_none() {
        return Err(MimirError::Configuration(
            "--print requires a prompt from arguments, @file, or piped stdin".into(),
        ));
    }
    if !cli.autonomous_gate.is_empty() && initial_prompt.is_none() {
        return Err(MimirError::Configuration(
            "autonomous quality gates require a noninteractive prompt".into(),
        ));
    }
    if cli.socket.is_some() {
        let prompt = initial_prompt
            .as_deref()
            .ok_or_else(|| MimirError::Configuration("--socket requires a prompt".into()))?;
        return run_socket_prompt(&cli, prompt).await;
    }
    prepare_run_session(&mut cli).await?;
    if cli.agent_mode.is_none()
        && cli.output == OutputMode::Text
        && initial_prompt.is_none()
        && !cli.no_tui
        && io::stdin().is_terminal()
        && io::stdout().is_terminal()
    {
        let state = resolve_state_dir(&cli.state_dir)?;
        cli.agent_mode = Some(load_tui_agent_mode(&state).await?.into());
    }
    let runtime = build_runtime(&cli).await?;
    dispatch_run_with_diagnostics(&cli, runtime, initial_prompt.as_deref()).await
}

async fn dispatch_run_with_diagnostics(
    cli: &Cli,
    runtime: Arc<AgentRuntime>,
    initial_prompt: Option<&str>,
) -> Result<()> {
    let state = resolve_state_dir(&cli.state_dir)?;
    let (provider, model, _) = runtime.model_selection().await;
    let mut recorder = RuntimeDiagnosticRunCollector::new(
        diagnostics_root(&state),
        DiagnosticManifest {
            schema_version: crate::diagnostics::DIAGNOSTIC_SCHEMA_VERSION,
            run_id: Uuid::nil(),
            session_id: cli.session.clone(),
            started_at: chrono::Utc::now(),
            mimir_version: env!("CARGO_PKG_VERSION").into(),
            provider,
            model,
            workspace: "$WORKSPACE".into(),
            configuration: DiagnosticConfiguration {
                output_mode: cli.output.as_str().into(),
                offline: cli.offline,
                autonomous: RuntimeBuildConfig::from_cli(cli)
                    .autonomous_limits
                    .is_some(),
            },
            privacy: DiagnosticPrivacy::default(),
        },
    );
    let mut receiver = runtime.subscribe_events();
    let (stop_sender, mut stop_receiver) = tokio::sync::oneshot::channel();
    let collector = tokio::spawn(async move {
        loop {
            tokio::select! {
                event_result = receiver.recv() => match event_result {
                    Ok(envelope) => recorder.record(&envelope.event),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                        recorder.note_dropped(skipped);
                        recorder.record(&RuntimeEvent::SessionEvent {
                            event: json!({"type": "diagnostic_events_lagged", "count": skipped}),
                        });
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                },
                _ = &mut stop_receiver => {
                    while let Ok(envelope) = receiver.try_recv() {
                        recorder.record(&envelope.event);
                    }
                    break;
                }
            }
        }
        recorder.finish_open();
    });
    let result = dispatch_run(cli, runtime, initial_prompt).await;
    let _ = stop_sender.send(());
    let _ = collector.await;
    result
}

fn daemon_runtime_error(error: MimirError) -> DaemonError {
    match error {
        MimirError::BudgetPaused(pause) => DaemonError::BudgetPaused(pause.to_string()),
        other => DaemonError::Protocol(other.to_string()),
    }
}

async fn dispatch_run(
    cli: &Cli,
    runtime: Arc<AgentRuntime>,
    initial_prompt: Option<&str>,
) -> Result<()> {
    match (cli.output, initial_prompt) {
        (OutputMode::Acp, _) => {
            serve_acp(
                runtime,
                cli.workspace.clone(),
                tokio::io::stdin(),
                tokio::io::stdout(),
            )
            .await
        }
        (OutputMode::Rpc, _) => {
            let mut build = RuntimeBuildConfig::from_cli(cli);
            let (provider, model, _) = runtime.model_selection().await;
            if build.provider != provider {
                build.base_url = None;
            }
            build.provider = provider;
            build.model = model;
            run_rpc(RpcSessionContext {
                runtime,
                bash_runner: build_bash_runner(&build)?,
                build,
                session_id: cli.session.clone(),
                activity: Arc::new(tokio::sync::Mutex::new(RpcActivity::default())),
                worker: None,
                control_worker: None,
                bash_worker: None,
                observations: HashMap::new(),
            })
            .await
        }
        (OutputMode::Daemon, _) => unreachable!("daemon mode is routed before runtime dispatch"),
        (mode, Some(prompt)) => {
            if let Some(limits) = RuntimeBuildConfig::from_cli(cli).autonomous_limits {
                run_once_autonomous(&runtime, prompt, mode, limits, cli).await
            } else {
                run_once(&runtime, prompt, mode).await
            }
        }
        (OutputMode::Text, None)
            if !cli.no_tui && io::stdin().is_terminal() && io::stdout().is_terminal() =>
        {
            let (initial_provider, initial_model, _) = runtime.model_selection().await;
            let initial_selection = tui_model_key(&initial_provider, &initial_model);
            let mut models = tui_model_options(&initial_provider, &initial_model);
            let mut sessions = Vec::new();
            if let Ok(state) = resolve_state_dir(&cli.state_dir) {
                let build = RuntimeBuildConfig::from_cli(cli);
                let session_root = resolve_session_root(&build, &state)?;
                sessions = list_run_session_ids(&build, &session_root)
                    .await
                    .unwrap_or_default();
            }
            let state = resolve_state_dir(&cli.state_dir)?;
            let migrated = MigratedRuntimeState::load(&state)?;
            models.extend(
                migrated
                    .models()
                    .iter()
                    .map(|model| tui_model_key(&model.provider, &model.id)),
            );
            models.retain(|selector| {
                selector == &initial_selection
                    || selector
                        .split_once('/')
                        .is_none_or(|(provider, model)| migrated.model_is_enabled(provider, model))
            });
            models.sort();
            models.dedup();
            models = filter_tui_models(models, &cli.models, &initial_selection)?;
            if !sessions.iter().any(|session| session == &cli.session) {
                sessions.insert(0, cli.session.clone());
            }
            let tui_build = RuntimeBuildConfig::from_cli(cli);
            let tui_bash_runner = build_bash_runner(&tui_build)?;
            run_tui_with_autonomous(
                runtime,
                Arc::new(CliTuiRuntimeFactory {
                    build: tui_build.clone(),
                    explicit_base_url: cli.base_url.clone(),
                    bash_runner: std::sync::RwLock::new(tui_bash_runner),
                    agent_mode: std::sync::RwLock::new(tui_build.agent_mode),
                }),
                models,
                sessions,
                state,
                initial_selection,
                cli.session.clone(),
                tui_build.autonomous_limits,
            )
            .await
        }
        (mode, None) => run_repl(runtime, mode).await,
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "centralizing the management surface keeps CLI routing explicit and auditable"
)]
async fn run_management(cli: &Cli, command: &Command) -> Result<()> {
    if matches!(command, Command::Doctor) {
        return doctor(cli).await;
    }
    let state = resolve_state_dir(&cli.state_dir)?;
    match command {
        Command::Doctor => doctor(cli).await,
        Command::Login {
            provider,
            api_key,
            api_key_stdin,
            enterprise_domain,
        } => {
            let store = AuthStore::new(&state)?;
            let api_key = match (api_key.as_deref(), *api_key_stdin) {
                (Some(_), true) => {
                    return Err(MimirError::Configuration(
                        "choose either --api-key or --api-key-stdin".into(),
                    ));
                }
                (Some(value), false) => Some(value.to_owned()),
                (None, true) => Some(read_api_key_from_stdin()?),
                (None, false) => None,
            };
            let registry = ProviderRegistry::builtin();
            let Some(definition) = registry.get(provider) else {
                let workspace = std::fs::canonicalize(&cli.workspace).map_err(|error| {
                    MimirError::Configuration(format!("workspace is inaccessible: {error}"))
                })?;
                let manager = load_extension_manager(&workspace, &state).await?;
                let descriptor = manager
                    .providers()
                    .into_iter()
                    .find(|descriptor| descriptor.name == *provider)
                    .ok_or_else(|| {
                        MimirError::Configuration(format!("unknown provider: {provider}"))
                    })?;
                if enterprise_domain.is_some() {
                    return Err(MimirError::Configuration(
                        "extension providers do not accept --enterprise-domain".into(),
                    ));
                }
                if let Some(api_key) = api_key {
                    store.set_api_key(provider, &api_key).await?;
                    return print_json(&json!({
                        "provider": provider,
                        "auth_type": "api_key",
                        "status": "stored"
                    }));
                }
                if descriptor.oauth_name.is_none() {
                    return Err(MimirError::Configuration(format!(
                        "extension provider {provider} requires --api-key"
                    )));
                }
                let mut request = manager.begin_provider_oauth_login(provider).await?;
                while let Some(pending) = request {
                    let crate::extensions::UiRequest::Input { id, prompt, .. } = pending else {
                        return Err(MimirError::Protocol(
                            "extension OAuth login requested unsupported interactive UI".into(),
                        ));
                    };
                    println!("{prompt}");
                    print!("> ");
                    io::stdout().flush()?;
                    let mut input = String::new();
                    io::stdin().read_line(&mut input)?;
                    manager
                        .respond_ui(&id, json!(input.trim_end_matches(['\r', '\n'])))
                        .await?;
                    if matches!(store.get(provider).await?, Some(AuthCredential::OAuth(_))) {
                        request = None;
                    } else {
                        request = manager
                            .drain_ui_requests()
                            .await
                            .into_iter()
                            .map(|(_, request)| request)
                            .find(|request| {
                                matches!(request, crate::extensions::UiRequest::Input { .. })
                            });
                        if request.is_none() {
                            return Err(MimirError::Protocol(
                                "extension OAuth login ended without credentials".into(),
                            ));
                        }
                    }
                }
                return print_json(&json!({
                    "provider": provider,
                    "auth_type": "oauth",
                    "status": "stored"
                }));
            };
            if let Some(api_key) = api_key {
                if !definition
                    .auth
                    .contains(&crate::provider::registry::AuthKind::ApiKey)
                {
                    return Err(MimirError::Configuration(format!(
                        "{provider} does not accept API-key login"
                    )));
                }
                store.set_api_key(provider, &api_key).await?;
                print_json(&json!({
                    "provider": provider,
                    "auth_type": "api_key",
                    "status": "stored"
                }))
            } else if let Some(oauth_provider) = OAuthProvider::from_id(provider) {
                if oauth_provider == OAuthProvider::GitHubCopilot {
                    let domain = enterprise_domain.as_deref().unwrap_or("github.com");
                    let device = DeviceAuthorization::begin_github(domain).await?;
                    println!("Open: {}", device.verification_uri);
                    println!("Enter code: {}", device.user_code);
                    io::stdout().flush()?;
                    let credential = device.poll_github(domain).await?;
                    store.set_oauth(provider, credential).await?;
                    return print_json(&json!({
                        "provider": provider,
                        "auth_type": "oauth_device",
                        "status": "stored"
                    }));
                }
                let pending = PendingOAuth::begin(oauth_provider)?;
                println!("Open: {}", pending.authorize_url);
                println!(
                    "Waiting for the browser to return to {} ...",
                    pending.redirect_uri
                );
                let input = match pending
                    .receive_browser_callback(Duration::from_secs(5 * 60))
                    .await
                {
                    Ok(callback) => callback,
                    Err(error) => {
                        eprintln!("Automatic browser callback unavailable: {error}");
                        print!("Paste the authorization code or full redirect URL: ");
                        io::stdout().flush()?;
                        let mut input = String::new();
                        io::stdin().read_line(&mut input)?;
                        input
                    }
                };
                let credential = pending.exchange(&input).await?;
                store.set_oauth(provider, credential).await?;
                print_json(&json!({
                    "provider": provider,
                    "auth_type": "oauth",
                    "status": "stored"
                }))
            } else if definition
                .auth
                .contains(&crate::provider::registry::AuthKind::Ambient)
            {
                let configured = ambient_credentials_configured(provider, definition);
                if !configured {
                    return Err(MimirError::Configuration(format!(
                        "{provider} ambient credentials are not configured; use --api-key or configure the provider's documented environment credentials"
                    )));
                }
                print_json(&json!({
                    "provider": provider,
                    "auth_type": "ambient",
                    "status": "available",
                    "stored": false
                }))
            } else {
                Err(MimirError::Configuration(format!(
                    "{provider} requires --api-key"
                )))
            }
        }
        Command::Logout { provider } => {
            let store = AuthStore::new(&state)?;
            let logged_out = store.logout(provider).await?;
            print_json(&json!({"provider": provider, "logged_out": logged_out}))
        }
        Command::Auth {
            action: AuthCommand::Status,
        } => {
            let store = AuthStore::new(&state)?;
            print_json(&json!({
                "schema_version": 1,
                "credentials": store.statuses().await?
            }))
        }
        Command::Providers => {
            let registry = ProviderRegistry::builtin();
            let providers: Vec<_> = registry
                .iter()
                .map(|provider| {
                    json!({
                        "id": provider.id,
                        "name": provider.name,
                        "auth": provider.auth.iter().map(|kind| kind.as_str()).collect::<Vec<_>>(),
                        "configured_from_environment": provider.environment_key().is_some(),
                        "default_model": provider.default_model,
                        "runtime_support": provider.runtime_support.as_str()
                    })
                })
                .collect();
            print_json(&json!({"schema_version": 1, "providers": providers}))
        }
        Command::Model {
            action: ModelCommand::List { search },
        } => {
            let migrated = MigratedRuntimeState::load(&state)?;
            let query = search.as_deref().map(str::to_ascii_lowercase);
            let mut models = model_catalog()
                .iter()
                .filter(|model| migrated.model_is_enabled(&model.provider, &model.id))
                .cloned()
                .collect::<Vec<_>>();
            models.extend(migrated.models().iter().cloned());
            models.sort_by(|left, right| {
                (&left.provider, &left.id).cmp(&(&right.provider, &right.id))
            });
            models.dedup_by(|left, right| left.provider == right.provider && left.id == right.id);
            let models = models
                .into_iter()
                .filter(|model| {
                    query.as_ref().is_none_or(|query| {
                        format!("{}/{} {}", model.provider, model.id, model.api)
                            .to_ascii_lowercase()
                            .contains(query)
                    })
                })
                .map(|model| {
                    json!({
                        "provider": model.provider,
                        "id": model.id,
                        "selector": format!("{}/{}", model.provider, model.id),
                        "api": model.api,
                        "reasoning": model.reasoning,
                        "context_window": model.context_window,
                        "max_tokens": model.max_tokens,
                    })
                })
                .collect::<Vec<_>>();
            print_json(&json!({
                "schema_version": 1,
                "search": search,
                "models": models,
            }))
        }
        Command::Agents => run_top_level_list(cli, &state, true).await,
        Command::List { all } => run_top_level_list(cli, &state, *all).await,
        Command::Attach { .. } => unreachable!("attach is rewritten before management routing"),
        Command::Stop { agent } => {
            run_public_daemon_command(
                cli,
                &state,
                json!({"type": "kill", "activeSessionId": agent}),
            )
            .await
        }
        Command::Rename { agent, name } => {
            let name = name.join(" ");
            run_public_daemon_command(
                cli,
                &state,
                json!({"type": "rename", "activeSessionId": agent, "name": name}),
            )
            .await
        }
        Command::Send {
            from,
            steer,
            follow_up,
            json: _,
            agent,
            explicit_message,
            message_parts,
        } => {
            let message = explicit_message
                .clone()
                .unwrap_or_else(|| message_parts.join(" "));
            if message.trim().is_empty() {
                return Err(MimirError::Configuration(
                    "send requires a non-empty message".into(),
                ));
            }
            let command =
                send_public_command(agent, &message, from.as_deref(), *steer, *follow_up)?;
            run_public_daemon_command(cli, &state, command).await
        }
        Command::Status => run_top_level_status(cli, &state).await,
        Command::Shutdown { force } => run_top_level_shutdown(cli, &state, *force).await,
        Command::Update { action, force } => run_self_update(*action, *force),
        Command::Config { action } => run_config_management(cli, action.as_ref(), &state).await,
        Command::Package { action } => run_package_management(action, &state, &cli.workspace).await,
        Command::Daemon { action } => run_daemon_management(cli, action, &state).await,
        Command::Extension { action } => run_extension_management(cli, action, &state).await,
        Command::Rlm { action } => run_rlm_management(cli, action, &state).await,
        Command::Migrate { action } => run_migration_management(action, &state).await,
        Command::Session {
            action: SessionCommand::List,
        } => print_json(&json!({
            "schema_version": 1,
            "sessions": FileSessionStore::list_ids(&state).await?,
        })),
        Command::Session { action } => run_session_management(cli, action, &state).await,
        Command::Diagnose { action } => run_diagnose_management(action, &state).await,
        Command::Mcp { action } => run_mcp_management(action, &state).await,
        Command::Goal { action } => {
            let store = GoalStore::new(&state);
            match action {
                GoalCommand::Set {
                    objective,
                    token_budget,
                } => print_json(&serde_json::to_value(
                    store.create(objective, *token_budget).await?,
                )?),
                GoalCommand::Show => print_json(&serde_json::to_value(store.load().await?)?),
                GoalCommand::Clear => {
                    store.clear().await?;
                    print_json(&json!({"cleared": true}))
                }
            }
        }
        Command::Schedule { action } => run_schedule_management(cli, action, &state).await,
        Command::Benchmark { action } => {
            let runtime = build_runtime(cli).await?;
            match action {
                BenchmarkCommand::Prompt { prompt } => {
                    let started = std::time::Instant::now();
                    let answer = runtime
                        .run(prompt, &StdoutEventSink { enabled: false })
                        .await?;
                    let elapsed = started.elapsed();
                    print_json(&json!({
                        "schema_version": 1,
                        "mode": "prompt",
                        "elapsed_ms": elapsed.as_secs_f64() * 1000.0,
                        "answer_chars": answer.chars().count(),
                        "session": cli.session,
                    }))
                }
            }
        }
    }
}

fn send_public_command(
    agent: &str,
    message: &str,
    from: Option<&str>,
    steer: bool,
    follow_up: bool,
) -> Result<Value> {
    if (steer || follow_up) && from.is_some() {
        return Err(MimirError::Configuration(
            "send --from cannot be combined with --steer or --follow-up".into(),
        ));
    }
    if steer {
        return Ok(json!({"type": "steer", "activeSessionId": agent, "message": message}));
    }
    if follow_up {
        return Ok(json!({"type": "follow_up", "activeSessionId": agent, "message": message}));
    }
    let mut command = json!({
        "type": "send_message",
        "targetActiveSessionId": agent,
        "message": message,
    });
    if let Some(source) = from {
        command["fromActiveSessionId"] = Value::String(source.into());
    }
    Ok(command)
}

async fn run_schedule_management(cli: &Cli, action: &ScheduleCommand, state: &Path) -> Result<()> {
    let store = ScheduleStore::new(state);
    match action {
        ScheduleCommand::Add {
            name,
            prompt,
            message,
            at,
            every_seconds,
            json: _,
        } if !message.is_empty() => {
            if at.is_some() || every_seconds.is_some() {
                return Err(MimirError::Configuration(
                    "reference schedule form cannot be combined with --at or --every-seconds"
                        .into(),
                ));
            }
            let message = message.join(" ");
            if prompt.trim().is_empty() || message.trim().is_empty() {
                return Err(MimirError::Configuration(
                    "usage: schedule add <agent> <schedule> -- <message>".into(),
                ));
            }
            run_public_daemon_command(
                cli,
                state,
                json!({
                    "type": "cron_add",
                    "activeSessionId": name,
                    "schedule": prompt,
                    "prompt": message,
                }),
            )
            .await
        }
        ScheduleCommand::Add {
            name,
            prompt,
            at,
            every_seconds,
            ..
        } => {
            let next_run = at.as_deref().map_or_else(
                || Ok(chrono::Utc::now()),
                |value| {
                    chrono::DateTime::parse_from_rfc3339(value)
                        .map(|time| time.with_timezone(&chrono::Utc))
                        .map_err(|error| {
                            MimirError::Configuration(format!("invalid --at timestamp: {error}"))
                        })
                },
            )?;
            let every = every_seconds.map(std::time::Duration::from_secs);
            print_json(&serde_json::to_value(
                store
                    .add(name, &cli.session, prompt, next_run, every)
                    .await?,
            )?)
        }
        ScheduleCommand::List { all, agent, json } if *all || agent.is_some() || *json => {
            let mut command = json!({"type": "cron_list", "includeInactive": all});
            if let Some(agent) = agent {
                command["activeSessionId"] = Value::String(agent.clone());
            }
            run_public_daemon_command(cli, state, command).await
        }
        ScheduleCommand::List { .. } => print_json(&serde_json::to_value(store.list().await?)?),
        ScheduleCommand::Cancel { id } => {
            print_json(&serde_json::to_value(store.cancel(*id).await?)?)
        }
    }
}

fn run_self_update(action: UpdateAction, force: bool) -> Result<()> {
    let status = json!({
        "schema_version": 1,
        "current_version": env!("CARGO_PKG_VERSION"),
        "status": "not_configured",
        "signed_release_transport": false,
        "update_available": null,
        "force_requested": force,
    });
    match action {
        UpdateAction::Status | UpdateAction::Check => print_json(&status),
        UpdateAction::Install => Err(MimirError::Configuration(
            "self-update is not configured: this build has no signed release transport; install a newer trusted binary using the same method used for this installation"
                .into(),
        )),
    }
}

async fn resolved_config(cli: &Cli, state: &Path) -> Result<Value> {
    let workspace = std::fs::canonicalize(&cli.workspace)?;
    let build = RuntimeBuildConfig::from_cli(cli);
    let packages = ExtensionPackageManager::new(state)?.list().await?;
    Ok(json!({
        "schema_version": 1,
        "workspace": workspace,
        "state_dir": state,
        "session": cli.session,
        "provider": build.provider,
        "model": build.model,
        "offline": build.offline,
        "tools": {
            "all_disabled": build.tool_allowlist.as_ref().is_some_and(BTreeSet::is_empty),
            "builtin_tools": !build.no_builtin_tools,
        },
        "extensions": {
            "discovery": !build.no_extensions,
            "explicit": build.extension_paths,
        },
        "resource_discovery": {
            "context_files": !build.no_context_files,
            "skills": !build.no_skills,
            "prompt_templates": !build.no_prompt_templates,
            "themes": !build.no_themes,
            "explicit_skills": build.skill_paths,
            "explicit_prompt_templates": build.prompt_template_paths,
            "explicit_themes": build.theme_paths,
        },
        "installed_packages": packages,
        "credential_values_exposed": false,
    }))
}

async fn run_config_management(
    cli: &Cli,
    action: Option<&ConfigCommand>,
    state: &Path,
) -> Result<()> {
    if action.is_none() && io::stdin().is_terminal() && io::stdout().is_terminal() {
        return run_interactive_config(cli, state).await;
    }
    match action {
        Some(ConfigCommand::List) => {
            let packages = ExtensionPackageManager::new(state)?.list().await?;
            print_json(&json!({"schema_version": 1, "packages": packages}))
        }
        Some(ConfigCommand::Show) | None => print_json(&resolved_config(cli, state).await?),
    }
}

async fn run_interactive_config(cli: &Cli, state: &Path) -> Result<()> {
    loop {
        println!("Mimir configuration");
        println!("  1. Show resolved configuration");
        println!("  2. List installed packages");
        println!("  q. Quit");
        print!("Select: ");
        io::stdout().flush()?;
        let mut selection = String::new();
        io::stdin().read_line(&mut selection)?;
        match selection.trim() {
            "1" => {
                let config = resolved_config(cli, state).await?;
                println!("{}", serde_json::to_string_pretty(&config)?);
            }
            "2" => {
                let packages = ExtensionPackageManager::new(state)?.list().await?;
                if packages.is_empty() {
                    println!("No packages installed.");
                } else {
                    for package in packages {
                        println!("{} {}", package.name, package.version);
                    }
                }
            }
            "q" | "quit" | "exit" | "" => return Ok(()),
            _ => println!("Choose 1, 2, or q."),
        }
        println!();
    }
}

async fn run_top_level_shutdown(cli: &Cli, state: &Path, force: bool) -> Result<()> {
    if !force && io::stdin().is_terminal() && io::stdout().is_terminal() {
        print!("Stop the Mimir daemon and all active agents? [y/N] ");
        io::stdout().flush()?;
        let mut answer = String::new();
        io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            return print_json(&json!({"shutdown": false, "cancelled": true}));
        }
    }
    let config = daemon_config(state);
    let socket = cli.socket.as_deref().unwrap_or(&config.socket_path);
    let Ok(mut client) = DaemonClient::connect(socket).await else {
        return print_json(&json!({
            "schema_version": 1,
            "status": "stopped",
            "shutdown": false,
            "socket": socket,
        }));
    };
    let response = client
        .request(ClientRequest::Shutdown)
        .await
        .map_err(daemon_error)?;
    print_json(&serde_json::to_value(response)?)
}

async fn run_top_level_list(cli: &Cli, state: &Path, all: bool) -> Result<()> {
    let config = daemon_config(state);
    let socket = cli.socket.as_deref().unwrap_or(&config.socket_path);
    let mut data = if DaemonClient::connect(socket).await.is_ok() {
        public_daemon_request(cli, state, json!({"type": "list"})).await?
    } else if all {
        json!({"schemaVersion": 1, "sessions": [], "daemonStatus": "stopped"})
    } else {
        return Err(MimirError::Protocol(format!(
            "cannot connect to daemon socket '{}'",
            socket.display()
        )));
    };
    if all {
        data["savedSessions"] = serde_json::to_value(FileSessionStore::list_ids(state).await?)?;
    }
    print_json(&data)
}

async fn run_top_level_status(cli: &Cli, state: &Path) -> Result<()> {
    let config = daemon_config(state);
    let socket = cli.socket.as_deref().unwrap_or(&config.socket_path);
    let Ok(mut client) = DaemonClient::connect(socket).await else {
        return print_json(&json!({
            "schema_version": 1,
            "status": "stopped",
            "socket": socket,
        }));
    };
    let response = client
        .request(ClientRequest::Health)
        .await
        .map_err(daemon_error)?;
    let mut health = serde_json::to_value(response)?;
    health["status"] = Value::String("running".into());
    print_json(&health)
}

async fn run_public_daemon_command(cli: &Cli, state: &Path, command: Value) -> Result<()> {
    print_json(&public_daemon_request(cli, state, command).await?)
}

#[cfg(unix)]
async fn public_daemon_request(cli: &Cli, state: &Path, command: Value) -> Result<Value> {
    const MAX_PUBLIC_FRAME_BYTES: usize = 1024 * 1024;
    let socket = cli
        .socket
        .clone()
        .unwrap_or_else(|| daemon_config(state).socket_path);
    let stream = tokio::net::UnixStream::connect(&socket)
        .await
        .map_err(|error| {
            MimirError::Protocol(format!(
                "cannot connect to daemon socket '{}': {error}",
                socket.display()
            ))
        })?;
    let (reader, mut writer) = stream.into_split();
    let mut reader = tokio::io::BufReader::new(reader);
    let hello = read_public_json_line(&mut reader, MAX_PUBLIC_FRAME_BYTES).await?;
    if hello.get("type").and_then(Value::as_str) != Some("daemon_hello") {
        return Err(MimirError::Protocol(
            "daemon did not send a public protocol greeting".into(),
        ));
    }
    let request_id = Uuid::new_v4().to_string();
    let envelope = json!({
        "type": "command",
        "id": request_id,
        "protocol": {"name": crate::daemon::PUBLIC_DAEMON_PROTOCOL_NAME, "version": 7},
        "clientId": format!("mimir-cli-{}", std::process::id()),
        "command": command,
    });
    let encoded = serde_json::to_vec(&envelope)?;
    if encoded.len() > MAX_PUBLIC_FRAME_BYTES {
        return Err(MimirError::Protocol(
            "daemon command exceeds the 1 MiB public frame limit".into(),
        ));
    }
    writer.write_all(&encoded).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    for _ in 0..64 {
        let response = read_public_json_line(&mut reader, MAX_PUBLIC_FRAME_BYTES).await?;
        if response.get("type").and_then(Value::as_str) != Some("response")
            || response.get("id").and_then(Value::as_str) != Some(request_id.as_str())
        {
            continue;
        }
        if response.get("success").and_then(Value::as_bool) == Some(false) {
            let message = response
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("daemon command failed");
            return Err(MimirError::Protocol(message.into()));
        }
        return Ok(response.get("data").cloned().unwrap_or(Value::Null));
    }
    Err(MimirError::Protocol(
        "daemon did not return a matching command response".into(),
    ))
}

#[cfg(not(unix))]
fn public_daemon_request(
    _cli: &Cli,
    _state: &Path,
    _command: Value,
) -> std::future::Ready<Result<Value>> {
    std::future::ready(Err(MimirError::Configuration(
        "public daemon commands require Unix-domain sockets".into(),
    )))
}

#[cfg(unix)]
async fn read_public_json_line<R>(
    reader: &mut tokio::io::BufReader<R>,
    limit: usize,
) -> Result<Value>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut bytes = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Err(MimirError::Protocol(
                "daemon closed before returning a complete public frame".into(),
            ));
        }
        let count = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |index| index + 1);
        if bytes.len().saturating_add(count) > limit.saturating_add(1) {
            return Err(MimirError::Protocol(
                "daemon public frame exceeds the 1 MiB limit".into(),
            ));
        }
        bytes.extend_from_slice(&available[..count]);
        reader.consume(count);
        if bytes.last() == Some(&b'\n') {
            bytes.pop();
            return serde_json::from_slice(&bytes).map_err(Into::into);
        }
    }
}

async fn run_package_management(
    action: &PackageCommand,
    state: &Path,
    workspace: &Path,
) -> Result<()> {
    let workspace = std::fs::canonicalize(workspace)?;
    let local_state = workspace.join(".mimir");
    let manager_root = match action {
        PackageCommand::Install { local: true, .. }
        | PackageCommand::Remove { local: true, .. } => resolve_state_dir(&local_state)?,
        _ => state.to_path_buf(),
    };
    let manager = ExtensionPackageManager::new(&manager_root)?;
    match action {
        PackageCommand::List => {
            let mut packages = manager
                .list()
                .await?
                .into_iter()
                .map(|package| json!({"scope": "configured", "package": package}))
                .collect::<Vec<_>>();
            if local_state != state && local_state.exists() {
                packages.extend(
                    ExtensionPackageManager::new(&local_state)?
                        .list()
                        .await?
                        .into_iter()
                        .map(|package| json!({"scope": "local", "package": package})),
                );
            }
            print_json(&json!({"schema_version": 1, "packages": packages}))
        }
        PackageCommand::Install { source, local } => print_json(&json!({
            "schema_version": 1,
            "installed": manager.install(source).await?,
            "scope": if *local { "local" } else { "configured" },
        })),
        PackageCommand::Remove { name, local } => {
            let removed = manager.remove(name).await?;
            print_json(&json!({
                "schema_version": 1,
                "scope": if *local { "local" } else { "configured" },
                "removed": removed.as_ref().map(|package| json!({
                    "name": package.name,
                    "recovery_path": package.recovery_path,
                })),
            }))
        }
        PackageCommand::Update { name: Some(name) } => print_json(&json!({
            "schema_version": 1,
            "updated": [manager.update(name).await?],
        })),
        PackageCommand::Update { name: None } => {
            let installed = manager.list().await?;
            let mut updated = Vec::with_capacity(installed.len());
            for package in installed {
                updated.push(manager.update(&package.name).await?);
            }
            print_json(&json!({"schema_version": 1, "updated": updated}))
        }
    }
}

const MAX_SESSION_IMPORT_BYTES: u64 = 64 * 1024 * 1024;

async fn run_session_management(cli: &Cli, action: &SessionCommand, state: &Path) -> Result<()> {
    match action {
        SessionCommand::List => unreachable!("session list is handled by the caller"),
        SessionCommand::Show => {
            let store = FileSessionStore::create(state, &cli.session).await?;
            let loaded = store.load().await?;
            print_json(&json!({
                "schema_version": 1,
                "session": cli.session,
                "recovered_incomplete_tail": loaded.recovered_incomplete_tail,
                "records": loaded.records,
            }))
        }
        SessionCommand::Export { output } => export_session(cli, state, output).await,
        SessionCommand::Import { input, cwd } => {
            let bytes = read_bounded_regular_file(input, MAX_SESSION_IMPORT_BYTES).await?;
            let plan = prepare_switch_session(input, &bytes, cwd.as_deref())?;
            let store = FileSessionStore::create(state, &plan.target_session_id).await?;
            if tokio::fs::try_exists(store.path()).await? {
                return Err(MimirError::Configuration(format!(
                    "session already exists: {}",
                    plan.target_session_id
                )));
            }
            let record_count = plan.records.len();
            for record in plan.records {
                store.append(record).await?;
            }
            print_json(&json!({
                "schema_version": 1,
                "imported": true,
                "session": plan.target_session_id,
                "cwd": plan.cwd,
                "records": record_count,
            }))
        }
        SessionCommand::Switch { input, cwd } => {
            let bytes = read_bounded_regular_file(input, MAX_SESSION_IMPORT_BYTES).await?;
            let plan = prepare_switch_session(input, &bytes, cwd.as_deref())?;
            print_json(&json!({
                "schema_version": 1,
                "planned": true,
                "session": plan.target_session_id,
                "cwd": plan.cwd,
                "records": plan.records.len(),
            }))
        }
        SessionCommand::Share {
            input,
            viewer_base,
            gist_id,
        } => {
            let bytes = read_bounded_regular_file(
                input,
                u64::try_from(crate::session_compat::MAX_SHARE_PAYLOAD_BYTES).unwrap_or(u64::MAX),
            )
            .await?;
            let payload = prepare_share_payload(&bytes, viewer_base.as_deref())?;
            let viewer_url = gist_id
                .as_deref()
                .map(|id| payload.viewer_url(id))
                .transpose()?;
            print_json(&json!({
                "schema_version": 1,
                "filename": payload.filename,
                "content_type": payload.content_type,
                "byte_length": payload.bytes.len(),
                "viewer_url": viewer_url,
            }))
        }
    }
}

async fn run_diagnose_management(action: &DiagnoseCommand, state: &Path) -> Result<()> {
    const MAX_ANALYSIS_BYTES: u64 = 1024 * 1024;
    let root = diagnostics_root(state);
    match action {
        DiagnoseCommand::List => print_json(&json!({
            "schema_version": crate::diagnostics::DIAGNOSTIC_SCHEMA_VERSION,
            "runs": list_runs(&root)?,
        })),
        DiagnoseCommand::Show { run_id } => {
            print_json(&serde_json::to_value(load_bundle(&root, run_id)?)?)
        }
        DiagnoseCommand::Query {
            run_id,
            kind,
            status,
            json,
        } => {
            let events = query_events(&root, run_id, kind.as_deref(), status.as_deref())?;
            if *json {
                print_json(&json!({
                    "schema_version": crate::diagnostics::DIAGNOSTIC_SCHEMA_VERSION,
                    "run_id": run_id,
                    "events": events,
                }))
            } else {
                if events.is_empty() {
                    println!("No matching diagnostic events.");
                } else {
                    for event in events {
                        println!(
                            "{:06} +{:>8}ms {:<24} turn={} tool={}",
                            event.sequence,
                            event.elapsed_ms,
                            event.kind.name(),
                            event.turn_id.as_deref().unwrap_or("-"),
                            event.tool_call_id.as_deref().unwrap_or("-")
                        );
                    }
                }
                Ok(())
            }
        }
        DiagnoseCommand::Export {
            run_id,
            output,
            redacted,
        } => {
            let bundle = load_bundle(&root, run_id)?;
            if let Some(output) = output {
                let bytes = serde_json::to_vec_pretty(&bundle)?;
                write_output_atomic(output, &bytes).await?;
                print_json(&json!({
                    "schema_version": crate::diagnostics::DIAGNOSTIC_SCHEMA_VERSION,
                    "run_id": run_id,
                    "redacted": true,
                    "redaction_requested": redacted,
                    "output": output,
                }))
            } else {
                print_json(&serde_json::to_value(bundle)?)
            }
        }
        DiagnoseCommand::Annotate { run_id, file } => {
            let bytes = read_bounded_regular_file(file, MAX_ANALYSIS_BYTES).await?;
            let input: DiagnosticAnalysisInput = serde_json::from_slice(&bytes)?;
            print_json(&serde_json::to_value(append_analysis(
                &root, run_id, input,
            )?)?)
        }
        DiagnoseCommand::Replay { run_id } => {
            print_json(&serde_json::to_value(replay_bundle(&root, run_id)?)?)
        }
    }
}

async fn export_session(cli: &Cli, state: &Path, output: &Path) -> Result<()> {
    let store = FileSessionStore::create(state, &cli.session).await?;
    let loaded = store.load().await?;
    if output.extension().and_then(std::ffi::OsStr::to_str) == Some("html") {
        let html = render_session_messages_html(loaded.records.iter().filter_map(|record| {
            if let SessionPayload::Message(message) = &record.payload {
                Some(message)
            } else {
                None
            }
        }));
        write_output_atomic(output, html.as_bytes()).await?;
        return print_json(&json!({
            "schema_version": 1,
            "format": "html",
            "session": cli.session,
            "records": loaded.records.len(),
            "output": output,
        }));
    }
    if output.extension().and_then(std::ffi::OsStr::to_str) != Some("jsonl") {
        return Err(MimirError::Configuration(
            "session export output must end in .html or .jsonl".into(),
        ));
    }
    let cwd = std::fs::canonicalize(&cli.workspace)?;
    let timestamp = loaded
        .records
        .first()
        .map_or_else(chrono::Utc::now, |record| record.created_at);
    let text = export_jsonl(
        &loaded.records,
        &ReferenceSessionMetadata {
            session_id: cli.session.clone(),
            timestamp,
            cwd,
            parent_session: None,
        },
    )?;
    write_output_atomic(output, text.as_bytes()).await?;
    print_json(&json!({
        "schema_version": 1,
        "format": "reference_v3",
        "session": cli.session,
        "records": loaded.records.len(),
        "output": output,
    }))
}

async fn read_bounded_regular_file(path: &Path, max_bytes: u64) -> Result<Vec<u8>> {
    let metadata = tokio::fs::symlink_metadata(path).await?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(MimirError::Configuration(
            "input must be a regular file and not a symlink".into(),
        ));
    }
    if metadata.len() > max_bytes {
        return Err(MimirError::Configuration(format!(
            "input exceeds the {max_bytes}-byte limit"
        )));
    }
    Ok(tokio::fs::read(path).await?)
}

async fn write_output_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let path = safe_output_path(path).await?;
    if let Ok(metadata) = tokio::fs::symlink_metadata(&path).await
        && (metadata.file_type().is_symlink() || !metadata.is_file())
    {
        return Err(MimirError::Configuration(
            "output must be a regular file and not a symlink".into(),
        ));
    }
    let parent = path.parent().ok_or_else(|| {
        MimirError::Configuration("output path must have a parent directory".into())
    })?;
    let temporary = parent.join(format!(".mimir-export-{}.tmp", Uuid::new_v4()));
    let mut file = tokio::fs::File::create(&temporary).await?;
    tokio::io::AsyncWriteExt::write_all(&mut file, bytes).await?;
    file.sync_all().await?;
    drop(file);
    if let Err(error) = tokio::fs::rename(&temporary, &path).await {
        let _ = tokio::fs::remove_file(&temporary).await;
        return Err(error.into());
    }
    Ok(())
}

async fn safe_output_path(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    #[cfg(target_os = "macos")]
    let absolute = {
        let mut absolute = absolute;
        for (alias, canonical) in [("/var", "/private/var"), ("/tmp", "/private/tmp")] {
            if let Ok(relative) = absolute.strip_prefix(alias) {
                absolute = Path::new(canonical).join(relative);
                break;
            }
        }
        absolute
    };
    if absolute.components().any(|component| {
        matches!(
            component,
            std::path::Component::ParentDir | std::path::Component::Prefix(_)
        )
    }) {
        return Err(MimirError::Configuration(
            "output path must not contain parent-directory components".into(),
        ));
    }
    let parent = absolute.parent().ok_or_else(|| {
        MimirError::Configuration("output path must have a parent directory".into())
    })?;
    let mut current = PathBuf::from(std::path::MAIN_SEPARATOR_STR);
    for component in parent.components() {
        let std::path::Component::Normal(segment) = component else {
            continue;
        };
        current.push(segment);
        match tokio::fs::symlink_metadata(&current).await {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(MimirError::Configuration(format!(
                    "export path ancestor must not be a symlink: {}",
                    current.display()
                )));
            }
            Ok(metadata) if !metadata.is_dir() => {
                return Err(MimirError::Configuration(format!(
                    "export path ancestor is not a directory: {}",
                    current.display()
                )));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                tokio::fs::create_dir(&current).await?;
                if tokio::fs::symlink_metadata(&current)
                    .await?
                    .file_type()
                    .is_symlink()
                {
                    return Err(MimirError::Configuration(
                        "export path ancestor became a symlink".into(),
                    ));
                }
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(absolute)
}

fn parse_mcp_env(values: &[String]) -> Result<BTreeMap<String, String>> {
    let mut env = BTreeMap::new();
    for value in values {
        let (key, item) = value.split_once('=').ok_or_else(|| {
            MimirError::Configuration("--env must use TARGET=SOURCE_ENV syntax".into())
        })?;
        if env.insert(key.to_owned(), item.to_owned()).is_some() {
            return Err(MimirError::Configuration(format!(
                "duplicate MCP environment key: {key}"
            )));
        }
    }
    Ok(env)
}

async fn run_mcp_management(action: &McpCommand, state: &Path) -> Result<()> {
    let catalog = McpServerCatalog::new(state)?;
    match action {
        McpCommand::List => print_mcp_list(&catalog).await,
        McpCommand::Add { .. } => add_mcp_server(&catalog, action).await,
        McpCommand::Remove { server } => {
            if catalog.get(server).await?.is_some() {
                McpAuthCoordinator::new(state)?.logout(server).await?;
            }
            let removed = catalog.remove(server).await?;
            print_json(&json!({"schema_version": 1, "server": server, "removed": removed}))
        }
        McpCommand::Status { server } | McpCommand::AuthStatus { server } => {
            print_mcp_status(state, server.as_deref()).await
        }
        McpCommand::Login {
            server,
            api_key,
            api_key_stdin,
            redirect_uri,
        } => {
            login_mcp(
                state,
                server,
                api_key.as_deref(),
                *api_key_stdin,
                redirect_uri,
            )
            .await
        }
        McpCommand::Logout { server } => {
            let logged_out = McpAuthCoordinator::new(state)?.logout(server).await?;
            print_json(&json!({"schema_version":1,"server":server,"logged_out":logged_out}))
        }
        McpCommand::Tools { server } => list_mcp_tools(&catalog, state, server).await,
        McpCommand::Call {
            server,
            tool,
            arguments,
        } => call_mcp_tool(&catalog, state, server, tool, arguments).await,
    }
}

async fn add_mcp_server(catalog: &McpServerCatalog, action: &McpCommand) -> Result<()> {
    let McpCommand::Add {
        server,
        label,
        program,
        url,
        builtin,
        args,
        env,
        oauth,
        bearer_token_env,
        client_id,
        scopes,
        disabled,
    } = action
    else {
        return Err(MimirError::Configuration(
            "internal MCP add dispatch mismatch".into(),
        ));
    };
    if catalog.get(server).await?.is_some() {
        return Err(MimirError::Configuration(format!(
            "MCP server already exists: {server}"
        )));
    }
    let mut config = if *builtin {
        if label.is_some()
            || !args.is_empty()
            || !env.is_empty()
            || client_id.is_some()
            || !scopes.is_empty()
            || bearer_token_env.is_some()
            || *oauth
        {
            return Err(MimirError::Configuration(
                "built-in MCP entries do not accept custom transport or auth metadata".into(),
            ));
        }
        builtin_mcp_catalog()
            .into_iter()
            .find(|entry| entry.server == *server)
            .ok_or_else(|| {
                MimirError::Configuration(format!("unknown built-in MCP server: {server}"))
            })?
    } else if let Some(url) = url {
        if !args.is_empty() || !env.is_empty() {
            return Err(MimirError::Configuration(
                "remote MCP entries do not accept --arg or --env".into(),
            ));
        }
        let mut remote = McpCatalogHttp::new(url)?;
        remote.client_id.clone_from(client_id);
        remote.scopes.clone_from(scopes);
        let mut entry =
            McpCatalogServer::remote(server, label.as_deref().unwrap_or(server), remote)?;
        entry.oauth = *oauth;
        entry
    } else {
        let program = program.as_ref().ok_or_else(|| {
            MimirError::Configuration(
                "MCP add requires exactly one of --program, --url, or --builtin".into(),
            )
        })?;
        if client_id.is_some() || !scopes.is_empty() {
            return Err(MimirError::Configuration(
                "stdio MCP entries do not accept OAuth client metadata".into(),
            ));
        }
        let mut entry = McpCatalogServer::new(
            server,
            label.as_deref().unwrap_or(server),
            McpCatalogStdio {
                program: program.clone(),
                args: args.clone(),
                env: parse_mcp_env(env)?,
                ..McpCatalogStdio::default()
            },
        )?;
        entry.oauth = *oauth;
        entry
    };
    config.bearer_token_env_var.clone_from(bearer_token_env);
    config.enabled = !disabled;
    config.validate()?;
    catalog.upsert(config).await?;
    print_json(&json!({"schema_version": 1, "server": server, "added": true}))
}

struct CliMcpCodeReceiver;

#[async_trait]
impl McpOAuthCodeReceiver for CliMcpCodeReceiver {
    async fn receive_code(&self, authorization: &McpOAuthAuthorization) -> Result<String> {
        eprintln!(
            "Open this URL in a browser and authorize the MCP server:\n{}",
            authorization.authorization_url()
        );
        eprint!("Paste the full callback URL, or code=...&state=...: ");
        io::stderr().flush()?;
        let mut callback = String::new();
        io::stdin().read_line(&mut callback)?;
        let callback = callback.trim().to_owned();
        if callback.is_empty() {
            return Err(MimirError::Configuration(
                "MCP OAuth callback must not be blank".into(),
            ));
        }
        Ok(callback)
    }
}

async fn login_mcp(
    state: &Path,
    server: &str,
    api_key: Option<&str>,
    api_key_stdin: bool,
    redirect_uri: &str,
) -> Result<()> {
    let coordinator = McpAuthCoordinator::new(state)?;
    let entry = coordinator
        .catalog()
        .get(server)
        .await?
        .ok_or_else(|| MimirError::Configuration(format!("unknown MCP server: {server}")))?;
    let supplied_api_key = match (api_key, api_key_stdin) {
        (Some(value), false) => Some(value.to_owned()),
        (None, true) => Some(read_api_key_from_stdin()?),
        (None, false) => None,
        (Some(_), true) => {
            return Err(MimirError::Configuration(
                "choose either --api-key or --api-key-stdin".into(),
            ));
        }
    };
    if let Some(api_key) = supplied_api_key {
        coordinator.store_api_key(server, &api_key).await?;
        return print_json(&json!({
            "schema_version": 1,
            "server": server,
            "authenticated": true,
            "auth_type": "api_key",
        }));
    }
    if !entry.oauth {
        return Err(MimirError::Configuration(format!(
            "MCP server {server} uses API-key authentication; provide --api-key-stdin"
        )));
    }
    let config = entry.to_runtime_config()?;
    let remote = config.remote.as_ref().ok_or_else(|| {
        MimirError::Configuration(format!(
            "MCP server {server} must use a remote transport for OAuth login"
        ))
    })?;
    let oauth = McpOAuthClient::new(remote.io_timeout, remote.max_response_bytes)?;
    let bundle = oauth
        .authorize_with_headers(
            remote,
            &config.headers,
            None,
            redirect_uri,
            &CliMcpCodeReceiver,
        )
        .await?;
    coordinator.store_oauth_bundle(server, bundle).await?;
    print_json(&json!({
        "schema_version": 1,
        "server": server,
        "authenticated": true,
        "auth_type": "oauth",
    }))
}

async fn print_mcp_list(catalog: &McpServerCatalog) -> Result<()> {
    let servers = catalog
        .list()
        .await?
        .into_iter()
        .map(|config| {
            let remote = config.remote.as_ref();
            json!({
                "server": config.server,
                "label": config.label,
                "transport": if remote.is_some() { "http" } else { "stdio" },
                "program": config.stdio.program,
                "args": config.stdio.args,
                "env_keys": config.stdio.env.keys().collect::<Vec<_>>(),
                "url": remote.map(|remote| remote.url.as_str()),
                "client_id": remote.and_then(|remote| remote.client_id.as_deref()),
                "scopes": remote.map(|remote| remote.scopes.as_slice()).unwrap_or_default(),
                "oauth": config.oauth,
                "bearer_token_env_var": config.bearer_token_env_var,
                "enabled": config.enabled,
            })
        })
        .collect::<Vec<_>>();
    print_json(&json!({"schema_version": 1, "servers": servers}))
}

async fn list_mcp_tools(catalog: &McpServerCatalog, state: &Path, server: &str) -> Result<()> {
    let mut client = connect_catalog_client(catalog, state, server).await?;
    let tools = client
        .list_tools()
        .await?
        .into_iter()
        .map(|tool| {
            json!({
                "name": tool.name,
                "identifier": tool.identifier,
                "description": tool.description,
                "input_schema": tool.input_schema,
            })
        })
        .collect::<Vec<_>>();
    print_json(&json!({
        "schema_version": 1,
        "server": server,
        "server_info": {
            "name": client.server_info().name,
            "version": client.server_info().version,
        },
        "tools": tools,
    }))
}

async fn call_mcp_tool(
    catalog: &McpServerCatalog,
    state: &Path,
    server: &str,
    tool: &str,
    arguments: &str,
) -> Result<()> {
    let arguments: Value = serde_json::from_str(arguments)
        .map_err(|error| MimirError::Configuration(format!("invalid --arguments JSON: {error}")))?;
    if !arguments.is_object() {
        return Err(MimirError::Configuration(
            "--arguments must be a JSON object".into(),
        ));
    }
    let mut client = connect_catalog_client(catalog, state, server).await?;
    let output = match client.call_tool(tool, arguments).await? {
        McpToolCallOutput::Structured(value) => json!({"kind": "structured", "value": value}),
        McpToolCallOutput::Text(text) => json!({"kind": "text", "text": text}),
        McpToolCallOutput::Blocks(blocks) => json!({"kind": "blocks", "blocks": blocks}),
    };
    print_json(&json!({
        "schema_version": 1,
        "server": server,
        "tool": tool,
        "output": output,
    }))
}

async fn print_mcp_status(state: &Path, server: Option<&str>) -> Result<()> {
    let coordinator = McpAuthCoordinator::new(state)?;
    let statuses =
        match server {
            Some(server) => vec![coordinator.status(server).await?.ok_or_else(|| {
                MimirError::Configuration(format!("unknown MCP server: {server}"))
            })?],
            None => coordinator.list_statuses().await?,
        };
    let statuses = statuses
        .into_iter()
        .map(|status| {
            let source = status.auth.source.map(|source| match source {
                crate::mcp::McpAuthSource::BearerEnv => "bearer_env",
                crate::mcp::McpAuthSource::StoredApiKey => "stored_api_key",
                crate::mcp::McpAuthSource::StoredOAuth => "stored_oauth",
            });
            json!({
                "server": status.server,
                "label": status.label,
                "configured": true,
            "enabled": status.enabled,
            "authenticated": status.auth.enabled,
                "source": source,
                "expired": status.auth.expired,
                "uses_oauth": status.auth.uses_oauth,
                "bearer_token_env_var": status.auth.bearer_token_env_var,
            })
        })
        .collect::<Vec<_>>();
    if server.is_some() {
        print_json(statuses.first().unwrap_or(&Value::Null))
    } else {
        print_json(&json!({"schema_version": 1, "servers": statuses}))
    }
}

async fn run_migration_management(action: &MigrateCommand, state: &std::path::Path) -> Result<()> {
    let migrator = StateMigrator::new();
    match action {
        MigrateCommand::Plan { legacy_root } => {
            let plan = migrator.plan(legacy_root, state).await?;
            print_json(&serde_json::to_value(plan)?)
        }
        MigrateCommand::Apply { legacy_root } => {
            let plan = migrator.plan(legacy_root, state).await?;
            let result = migrator.apply(&plan).await?;
            print_json(&serde_json::to_value(result)?)
        }
        MigrateCommand::Rollback { journal } => {
            let result = migrator.rollback(journal).await?;
            print_json(&serde_json::to_value(result)?)
        }
    }
}

async fn run_extension_management(
    cli: &Cli,
    action: &ExtensionCommand,
    state: &std::path::Path,
) -> Result<()> {
    let workspace = std::fs::canonicalize(&cli.workspace)?;
    let mut catalog = ExtensionCatalog::new(&workspace, state)?;
    match action {
        ExtensionCommand::List => {
            let entries: Vec<_> = catalog
                .reload()
                .await?
                .into_iter()
                .map(|entry| {
                    json!({
                        "name": entry.manifest.name,
                        "version": entry.manifest.version,
                        "enabled": entry.enabled,
                        "capabilities": entry.manifest.capabilities,
                        "source": format!("{:?}", entry.source).to_ascii_lowercase()
                    })
                })
                .collect();
            print_json(&json!({"schema_version": 1, "extensions": entries}))
        }
        ExtensionCommand::Enable { name } => {
            catalog.enable(name).await?;
            print_json(&json!({"name": name, "enabled": true}))
        }
        ExtensionCommand::Disable { name } => {
            catalog.disable(name).await?;
            print_json(&json!({"name": name, "enabled": false}))
        }
        ExtensionCommand::Invoke {
            name,
            command,
            payload,
        } => {
            let entry = catalog
                .reload()
                .await?
                .into_iter()
                .find(|entry| entry.manifest.name == *name)
                .ok_or_else(|| MimirError::Configuration(format!("unknown extension: {name}")))?;
            if !entry.enabled {
                return Err(MimirError::Configuration(format!(
                    "extension {name} is disabled"
                )));
            }
            if !entry.manifest.capabilities.contains(&Capability::Commands) {
                return Err(MimirError::Configuration(format!(
                    "extension {name} lacks the commands capability"
                )));
            }
            let payload = serde_json::from_str(payload).map_err(|error| {
                MimirError::Configuration(format!("invalid --payload JSON: {error}"))
            })?;
            let host =
                JsonLineExtensionHost::new(entry.manifest, &workspace, HostLimits::default())?;
            let response = host
                .invoke(HostRequest {
                    schema_version: 1,
                    id: Uuid::new_v4().to_string(),
                    command: command.clone(),
                    payload,
                })
                .await?;
            print_json(&serde_json::to_value(response)?)
        }
        ExtensionCommand::Registrations => {
            let manager = load_extension_manager(&workspace, state).await?;
            print_json(&json!({
                "schema_version": 1,
                "tools": manager.tools(),
                "commands": manager.commands(),
                "renderers": manager.renderers(),
                "providers": manager.providers()
            }))
        }
        ExtensionCommand::Run { command, args } => {
            let manager = load_extension_manager(&workspace, state).await?;
            let result = manager.invoke_command(command, args).await?;
            print_json(&serde_json::to_value(result)?)
        }
    }
}

async fn load_extension_manager(workspace: &Path, state: &Path) -> Result<Arc<ExtensionManager>> {
    let mut catalog = ExtensionCatalog::new(workspace, state)?;
    Ok(Arc::new(
        ExtensionManager::load(
            catalog.reload().await?,
            workspace,
            state,
            RuntimeLimits::default(),
        )
        .await?,
    ))
}

async fn run_rlm_management(cli: &Cli, action: &RlmCommand, state: &std::path::Path) -> Result<()> {
    let workspace = std::fs::canonicalize(&cli.workspace)?;
    let (extension, namespace) = match action {
        RlmCommand::Get {
            extension,
            namespace,
            ..
        }
        | RlmCommand::Put {
            extension,
            namespace,
            ..
        }
        | RlmCommand::List {
            extension,
            namespace,
        }
        | RlmCommand::Delete {
            extension,
            namespace,
            ..
        } => (extension, namespace),
    };
    let store = RlmStore::new(
        state,
        &workspace,
        extension,
        RlmLimits {
            max_value_bytes: 256 * 1024,
            max_namespace_bytes: 4 * 1024 * 1024,
            max_keys: 4096,
        },
    )?;
    match action {
        RlmCommand::Get { key, .. } => print_json(&json!({
            "value": store.get(namespace, key).await?
        })),
        RlmCommand::Put { key, value, .. } => {
            let value = serde_json::from_str(value).map_err(|error| {
                MimirError::Configuration(format!("invalid value JSON: {error}"))
            })?;
            store.put(namespace, key, value).await?;
            print_json(&json!({"stored": true}))
        }
        RlmCommand::List { .. } => print_json(&store.list(namespace).await?),
        RlmCommand::Delete { key, .. } => {
            store.delete(namespace, key).await?;
            print_json(&json!({"deleted": true}))
        }
    }
}

struct RuntimePromptHandler {
    build: RuntimeBuildConfig,
    state_root: PathBuf,
    runtime_operations: Arc<crate::daemon::runtime_ops::RuntimeOperations>,
    runtimes: tokio::sync::Mutex<HashMap<String, Arc<AgentRuntime>>>,
    durable_bindings: tokio::sync::Mutex<HashMap<String, String>>,
    follow_ups: tokio::sync::Mutex<HashMap<String, VecDeque<QueuedFollowUp>>>,
    follow_up_modes: tokio::sync::Mutex<HashMap<String, QueueMode>>,
    bash_runners: tokio::sync::Mutex<HashMap<String, Arc<BashRunner>>>,
    scoped_models: tokio::sync::Mutex<HashMap<String, Vec<ScopedModelSelection>>>,
    transports: tokio::sync::Mutex<HashMap<String, String>>,
    recovered_commands:
        tokio::sync::Mutex<HashMap<String, VecDeque<crate::daemon::turn_ops::RecoveredAction>>>,
    autonomous_states: tokio::sync::Mutex<HashMap<String, AutonomousState>>,
    queued_message_ids: tokio::sync::Mutex<HashMap<String, MessageDedupe>>,
    follow_up_queue_keys: tokio::sync::Mutex<HashMap<String, BTreeSet<String>>>,
    headless_gates: tokio::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

struct ActiveDiagnosticCollector {
    stop: tokio::sync::oneshot::Sender<()>,
    task: tokio::task::JoinHandle<()>,
}

impl ActiveDiagnosticCollector {
    async fn finish(self) {
        let _ = self.stop.send(());
        let _ = self.task.await;
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ScopedModelSelection {
    provider: String,
    model_id: String,
    #[serde(default)]
    thinking_level: Option<ThinkingLevel>,
}

#[derive(Default)]
struct MessageDedupe {
    order: VecDeque<String>,
    ids: BTreeSet<String>,
}

struct QueuedFollowUp {
    message: Message,
    queue_key: Option<String>,
}

impl MessageDedupe {
    fn admit(&mut self, id: &str) -> bool {
        if self.ids.contains(id) {
            return false;
        }
        if self.order.len() >= 1_024
            && let Some(expired) = self.order.pop_front()
        {
            self.ids.remove(&expired);
        }
        self.order.push_back(id.into());
        self.ids.insert(id.into());
        true
    }
}

impl RuntimePromptHandler {
    fn new(build: RuntimeBuildConfig, state: PathBuf) -> Self {
        Self {
            build,
            state_root: state.clone(),
            runtime_operations: Arc::new(crate::daemon::runtime_ops::RuntimeOperations::new(state)),
            runtimes: tokio::sync::Mutex::new(HashMap::new()),
            durable_bindings: tokio::sync::Mutex::new(HashMap::new()),
            follow_ups: tokio::sync::Mutex::new(HashMap::new()),
            follow_up_modes: tokio::sync::Mutex::new(HashMap::new()),
            bash_runners: tokio::sync::Mutex::new(HashMap::new()),
            scoped_models: tokio::sync::Mutex::new(HashMap::new()),
            transports: tokio::sync::Mutex::new(HashMap::new()),
            recovered_commands: tokio::sync::Mutex::new(HashMap::new()),
            autonomous_states: tokio::sync::Mutex::new(HashMap::new()),
            queued_message_ids: tokio::sync::Mutex::new(HashMap::new()),
            follow_up_queue_keys: tokio::sync::Mutex::new(HashMap::new()),
            headless_gates: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    async fn runtime(&self, session_id: &str) -> Result<Arc<AgentRuntime>> {
        if let Some(runtime) = self.runtimes.lock().await.get(session_id).cloned() {
            return Ok(runtime);
        }
        let durable_session_id = self
            .durable_bindings
            .lock()
            .await
            .get(session_id)
            .cloned()
            .unwrap_or_else(|| session_id.into());
        let runtime = build_runtime_for_session(&self.build, &durable_session_id).await?;
        self.runtimes
            .lock()
            .await
            .insert(session_id.into(), Arc::clone(&runtime));
        Ok(runtime)
    }

    async fn bash_runner(&self, session_id: &str) -> Result<Arc<BashRunner>> {
        let mut runners = self.bash_runners.lock().await;
        if let Some(runner) = runners.get(session_id) {
            return Ok(Arc::clone(runner));
        }
        let runner = build_bash_runner(&self.build)?;
        runners.insert(session_id.into(), Arc::clone(&runner));
        Ok(runner)
    }

    async fn headless_gate(&self, session_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.headless_gates
            .lock()
            .await
            .entry(session_id.into())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    async fn start_diagnostic_collector(
        &self,
        session_id: &str,
        runtime: &AgentRuntime,
    ) -> ActiveDiagnosticCollector {
        let (provider, model, _) = runtime.model_selection().await;
        let mut recorder = RuntimeDiagnosticRunCollector::new(
            diagnostics_root(&self.state_root),
            DiagnosticManifest {
                schema_version: crate::diagnostics::DIAGNOSTIC_SCHEMA_VERSION,
                run_id: Uuid::nil(),
                session_id: session_id.into(),
                started_at: chrono::Utc::now(),
                mimir_version: env!("CARGO_PKG_VERSION").into(),
                provider,
                model,
                workspace: "$WORKSPACE".into(),
                configuration: DiagnosticConfiguration {
                    output_mode: "daemon".into(),
                    offline: self.build.offline,
                    autonomous: self.build.autonomous_limits.is_some(),
                },
                privacy: DiagnosticPrivacy::default(),
            },
        );
        let mut receiver = runtime.subscribe_events();
        let (stop, mut stop_receiver) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    event_result = receiver.recv() => match event_result {
                        Ok(envelope) => recorder.record(&envelope.event),
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                            recorder.note_dropped(skipped);
                            recorder.record(&RuntimeEvent::SessionEvent {
                                event: json!({"type": "diagnostic_events_lagged", "count": skipped}),
                            });
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    },
                    _ = &mut stop_receiver => {
                        while let Ok(envelope) = receiver.try_recv() {
                            recorder.record(&envelope.event);
                        }
                        break;
                    }
                }
            }
            recorder.finish_open();
        });
        ActiveDiagnosticCollector { stop, task }
    }

    async fn run_autonomous_continuations(
        &self,
        session_id: &str,
        runtime: &AgentRuntime,
        mut answer: String,
    ) -> std::result::Result<String, DaemonError> {
        let generation = {
            let mut states = self.autonomous_states.lock().await;
            let Some(state) = states.get_mut(session_id) else {
                return Ok(answer);
            };
            let Some(generation) = state.begin_run(std::time::Instant::now()) else {
                return Ok(answer);
            };
            generation
        };
        loop {
            let usage = runtime
                .messages_snapshot()
                .await
                .iter()
                .rev()
                .find(|message| message.role == Role::Assistant)
                .map_or_else(Default::default, |message| message.usage);
            let continuation = {
                let mut states = self.autonomous_states.lock().await;
                let Some(state) = states.get_mut(session_id) else {
                    return Ok(answer);
                };
                state.record_turn(generation, usage);
                state.next_continuation(generation, std::time::Instant::now())
            };
            let Some(continuation) = continuation else {
                return Ok(answer);
            };
            answer = runtime
                .run(&continuation, &StdoutEventSink { enabled: false })
                .await
                .map_err(daemon_runtime_error)?;
        }
    }

    async fn select_runtime_model(
        &self,
        runtime: &AgentRuntime,
        provider: &str,
        model: &str,
    ) -> Result<(ModelDefinition, ThinkingLevel)> {
        let available = runtime_available_models(&self.build, runtime).await?;
        let selected = available
            .into_iter()
            .find(|entry| entry.provider == provider && entry.id == model)
            .ok_or_else(|| {
                MimirError::Configuration(format!("Model not found: {provider}/{model}"))
            })?;
        let state = resolve_state_dir(&self.build.state_dir)?;
        let mut build = self.build.clone();
        if build.provider != provider {
            build.base_url = None;
        }
        build.provider = provider.into();
        build.model = model.into();
        let runtime_provider = build_provider_for_runtime(&build, &state).await?;
        let thinking_level = runtime
            .select_model(
                runtime_provider,
                provider,
                model,
                selected.thinking_levels(),
                selected.thinking_level_map.clone(),
            )
            .await?;
        Ok((selected, thinking_level))
    }

    async fn admit_queued_message(
        &self,
        session_id: &str,
        command: &PublicDaemonCommand,
        follow_up: bool,
    ) -> bool {
        if let Some(id) = command.field("agentMessageId").and_then(Value::as_str) {
            let mut sessions = self.queued_message_ids.lock().await;
            if !sessions.entry(session_id.into()).or_default().admit(id) {
                return false;
            }
        }
        if follow_up && let Some(key) = command.field("queueKey").and_then(Value::as_str) {
            let mut sessions = self.follow_up_queue_keys.lock().await;
            if !sessions
                .entry(session_id.into())
                .or_default()
                .insert(key.into())
            {
                return false;
            }
        }
        true
    }

    async fn prepare_queued_message(
        &self,
        runtime: &AgentRuntime,
        mut message: Message,
        command: &PublicDaemonCommand,
    ) -> std::result::Result<Message, DaemonError> {
        let expand = command
            .field("expandPromptTemplates")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        if expand {
            let workspace = std::fs::canonicalize(&self.build.workspace)
                .map_err(|error| DaemonError::Protocol(error.to_string()))?;
            let resources = load_runtime_resources(&self.build, &self.state_root, &workspace)
                .await
                .map_err(|error| DaemonError::Protocol(error.to_string()))?;
            for block in &mut message.content {
                if let Content::Text { text } = block {
                    *text = expand_prompt_template(text, &resources.prompt_templates);
                }
            }
            return Ok(message);
        }

        let mut prefix = Vec::new();
        if let Some(values) = command.field("prefixMessages").and_then(Value::as_array) {
            for value in values {
                append_custom_content(&mut prefix, value)?;
                runtime
                    .record_runtime_event("queued_prefix_message", &value.to_string())
                    .await
                    .map_err(|error| DaemonError::Protocol(error.to_string()))?;
            }
        }
        if let Some(value) = command.field("customMessage") {
            runtime
                .record_runtime_event("queued_custom_message", &value.to_string())
                .await
                .map_err(|error| DaemonError::Protocol(error.to_string()))?;
        }
        if !prefix.is_empty() {
            prefix.append(&mut message.content);
            message.content = prefix;
        }
        Ok(message)
    }

    async fn run_pending_follow_ups(
        &self,
        session_id: &str,
        runtime: &AgentRuntime,
    ) -> Result<Option<String>> {
        let mut last_answer = None;
        loop {
            let mode = self
                .follow_up_modes
                .lock()
                .await
                .get(session_id)
                .copied()
                .unwrap_or_default();
            let items = {
                let mut queues = self.follow_ups.lock().await;
                let Some(queue) = queues.get_mut(session_id) else {
                    break;
                };
                let items = match mode {
                    QueueMode::All => queue.drain(..).collect::<Vec<_>>(),
                    QueueMode::OneAtATime => queue.pop_front().into_iter().collect(),
                };
                if queue.is_empty() {
                    queues.remove(session_id);
                    self.follow_up_queue_keys.lock().await.remove(session_id);
                }
                items
            };
            if items.is_empty() {
                break;
            }
            {
                let mut keys = self.follow_up_queue_keys.lock().await;
                if let Some(active) = keys.get_mut(session_id) {
                    for key in items.iter().filter_map(|item| item.queue_key.as_ref()) {
                        active.remove(key);
                    }
                    if active.is_empty() {
                        keys.remove(session_id);
                    }
                }
            }
            let messages = items
                .into_iter()
                .map(|item| item.message)
                .collect::<Vec<_>>();
            last_answer = Some(
                runtime
                    .run_batch_messages(&messages, &StdoutEventSink { enabled: false })
                    .await?,
            );
        }
        Ok(last_answer)
    }

    async fn handle_turn_recovery(
        &self,
        session_id: &str,
        recovery: crate::daemon::turn_ops::TurnRecovery,
    ) -> std::result::Result<Value, DaemonError> {
        use crate::daemon::turn_ops::TurnRecovery;

        let runtime = self
            .runtime(session_id)
            .await
            .map_err(|error| DaemonError::Protocol(error.to_string()))?;
        match recovery {
            TurnRecovery::RestoreNextTurn { messages } => {
                Self::restore_next_turn(runtime.as_ref(), messages).await
            }
            TurnRecovery::RestoreActions { actions } => {
                self.restore_actions(session_id, runtime.as_ref(), actions)
                    .await
            }
            TurnRecovery::AppendCustomMessage { message } => {
                Self::append_custom_message(runtime.as_ref(), &message).await
            }
            TurnRecovery::ResumeQueue => {
                self.resume_recovered_queue(session_id, runtime.as_ref())
                    .await
            }
        }
    }

    async fn restore_next_turn(
        runtime: &AgentRuntime,
        messages: Vec<crate::daemon::turn_ops::RecoveredCustomMessage>,
    ) -> std::result::Result<Value, DaemonError> {
        if runtime
            .pending_steering_count()
            .await
            .saturating_add(messages.len())
            > 64
        {
            return Err(DaemonError::Protocol(
                "restored next-turn messages exceed the 64-message steering limit".into(),
            ));
        }
        for message in messages {
            let detail = serde_json::to_string(&message.raw)?;
            runtime
                .steer_message(message.into_user_message())
                .await
                .map_err(|error| DaemonError::Protocol(error.to_string()))?;
            runtime
                .record_runtime_event("restored_next_turn", &detail)
                .await
                .map_err(|error| DaemonError::Protocol(error.to_string()))?;
        }
        Ok(Value::Null)
    }

    async fn restore_actions(
        &self,
        session_id: &str,
        runtime: &AgentRuntime,
        actions: Vec<crate::daemon::turn_ops::RecoveredAction>,
    ) -> std::result::Result<Value, DaemonError> {
        use crate::daemon::turn_ops::{RecoveredActionPayload, RecoveredDelivery};

        let steering_count = actions
            .iter()
            .filter(|action| {
                action.delivery == RecoveredDelivery::NextTurn
                    && matches!(action.payload, RecoveredActionPayload::Turn(_))
            })
            .count();
        let follow_up_count = actions
            .iter()
            .filter(|action| {
                action.delivery == RecoveredDelivery::WhenIdle
                    && matches!(action.payload, RecoveredActionPayload::Turn(_))
            })
            .count();
        let command_count = actions
            .len()
            .saturating_sub(steering_count)
            .saturating_sub(follow_up_count);
        if runtime
            .pending_steering_count()
            .await
            .saturating_add(steering_count)
            > 64
        {
            return Err(DaemonError::Protocol(
                "restored actions exceed the 64-message steering limit".into(),
            ));
        }
        let existing = self
            .follow_ups
            .lock()
            .await
            .get(session_id)
            .map_or(0, VecDeque::len);
        if existing.saturating_add(follow_up_count) > 64 {
            return Err(DaemonError::Protocol(
                "restored actions exceed the 64-message follow-up limit".into(),
            ));
        }
        let existing_commands = self
            .recovered_commands
            .lock()
            .await
            .get(session_id)
            .map_or(0, VecDeque::len);
        if existing_commands.saturating_add(command_count) > 64 {
            return Err(DaemonError::Protocol(
                "recovered session-command store exceeds its 64-action limit".into(),
            ));
        }
        let restored = actions.len();
        for action in actions {
            self.restore_action(session_id, runtime, action).await?;
        }
        Ok(json!({"restored": restored}))
    }

    async fn restore_action(
        &self,
        session_id: &str,
        runtime: &AgentRuntime,
        action: crate::daemon::turn_ops::RecoveredAction,
    ) -> std::result::Result<(), DaemonError> {
        use crate::daemon::turn_ops::{RecoveredActionPayload, RecoveredDelivery};

        let detail = serde_json::to_string(&action.raw)?;
        match &action.payload {
            RecoveredActionPayload::Turn(message) => match action.delivery {
                RecoveredDelivery::NextTurn => runtime
                    .steer_message(message.clone())
                    .await
                    .map_err(|error| DaemonError::Protocol(error.to_string()))?,
                RecoveredDelivery::WhenIdle => self
                    .follow_ups
                    .lock()
                    .await
                    .entry(session_id.into())
                    .or_default()
                    .push_back(QueuedFollowUp {
                        message: message.clone(),
                        queue_key: None,
                    }),
            },
            RecoveredActionPayload::SessionCommand(_) => self
                .recovered_commands
                .lock()
                .await
                .entry(session_id.into())
                .or_default()
                .push_back(action),
        }
        runtime
            .record_runtime_event("restored_session_action", &detail)
            .await
            .map_err(|error| DaemonError::Protocol(error.to_string()))
    }

    async fn append_custom_message(
        runtime: &AgentRuntime,
        message: &crate::daemon::turn_ops::RecoveredCustomMessage,
    ) -> std::result::Result<Value, DaemonError> {
        let detail = serde_json::to_string(&message.raw)?;
        runtime
            .record_runtime_event(&format!("custom_message:{}", message.custom_type), &detail)
            .await
            .map_err(|error| DaemonError::Protocol(error.to_string()))?;
        Ok(Value::Null)
    }

    async fn resume_recovered_queue(
        &self,
        session_id: &str,
        runtime: &AgentRuntime,
    ) -> std::result::Result<Value, DaemonError> {
        let steering = runtime.pending_steering_count().await;
        let follow_ups = self
            .follow_ups
            .lock()
            .await
            .get(session_id)
            .map_or(0, VecDeque::len);
        let commands = self
            .recovered_commands
            .lock()
            .await
            .get(session_id)
            .map_or(0, VecDeque::len);
        if steering == 0 && follow_ups == 0 && commands == 0 {
            return Err(DaemonError::Protocol("no queued work to resume".into()));
        }
        self.dispatch_recovered_commands(session_id, runtime, None)
            .await?;
        runtime
            .run_pending_steering(&StdoutEventSink { enabled: false })
            .await
            .map_err(daemon_runtime_error)?;
        self.run_pending_follow_ups(session_id, runtime)
            .await
            .map_err(daemon_runtime_error)?;
        self.dispatch_recovered_commands(session_id, runtime, None)
            .await?;
        Ok(Value::Null)
    }

    async fn dispatch_recovered_commands(
        &self,
        session_id: &str,
        runtime: &AgentRuntime,
        boundary: Option<crate::daemon::turn_ops::RecoveredDelivery>,
    ) -> std::result::Result<usize, DaemonError> {
        let mut dispatched = 0_usize;
        loop {
            let action = {
                let mut stores = self.recovered_commands.lock().await;
                let Some(queue) = stores.get_mut(session_id) else {
                    break;
                };
                let position = queue
                    .iter()
                    .position(|action| boundary.is_none_or(|value| action.delivery == value));
                let action = position.and_then(|position| queue.remove(position));
                if queue.is_empty() {
                    stores.remove(session_id);
                }
                action
            };
            let Some(action) = action else {
                break;
            };
            self.dispatch_recovered_command(session_id, runtime, action)
                .await?;
            dispatched = dispatched.saturating_add(1);
        }
        Ok(dispatched)
    }

    async fn dispatch_recovered_command(
        &self,
        session_id: &str,
        runtime: &AgentRuntime,
        action: crate::daemon::turn_ops::RecoveredAction,
    ) -> std::result::Result<(), DaemonError> {
        use crate::daemon::turn_ops::RecoveredActionPayload;

        let RecoveredActionPayload::SessionCommand(command) = &action.payload else {
            return Err(DaemonError::Protocol(
                "recovered command store contained a non-command action".into(),
            ));
        };
        runtime
            .record_runtime_event(
                "recovered_session_command_started",
                &serde_json::to_string(&action.raw)?,
            )
            .await
            .map_err(|error| DaemonError::Protocol(error.to_string()))?;
        let result = self
            .execute_recovered_command(session_id, runtime, command)
            .await;
        let audit = match &result {
            Ok(value) => json!({"actionId": action.id, "status": "completed", "result": value}),
            Err(error) => {
                json!({"actionId": action.id, "status": "failed", "error": error.to_string()})
            }
        };
        runtime
            .record_runtime_event("recovered_session_command_finished", &audit.to_string())
            .await
            .map_err(|error| DaemonError::Protocol(error.to_string()))?;
        result.map(|_| ())
    }

    async fn execute_recovered_command(
        &self,
        session_id: &str,
        runtime: &AgentRuntime,
        command: &crate::daemon::turn_ops::RecoveredSessionCommand,
    ) -> std::result::Result<Value, DaemonError> {
        use crate::daemon::turn_ops::RecoveredSessionCommandName;

        match command.name {
            RecoveredSessionCommandName::Compact => serde_json::to_value(
                runtime
                    .compact((!command.args.trim().is_empty()).then_some(command.args.as_str()))
                    .await
                    .map_err(|error| DaemonError::Protocol(error.to_string()))?,
            )
            .map_err(DaemonError::from),
            RecoveredSessionCommandName::Refine => {
                self.execute_recovered_refine(session_id, runtime, &command.args)
                    .await
            }
            RecoveredSessionCommandName::Goal => self.execute_recovered_goal(&command.args).await,
            RecoveredSessionCommandName::Autonomous => {
                self.execute_recovered_autonomous(session_id, runtime, &command.args)
                    .await
            }
        }
    }

    async fn execute_recovered_refine(
        &self,
        session_id: &str,
        runtime: &AgentRuntime,
        arguments: &str,
    ) -> std::result::Result<Value, DaemonError> {
        let (instructions, rollback_id, global) = parse_recovered_refine_args(arguments)?;
        let result = refinement::refine(
            runtime,
            &self.state_root,
            session_id,
            RefineOptions {
                instructions: instructions.as_deref(),
                rollback_id: rollback_id.as_deref(),
                global,
            },
        )
        .await
        .map_err(|error| DaemonError::Protocol(error.to_string()))?;
        runtime
            .set_harness_context(
                refinement::load_harness_context(&self.state_root, session_id)
                    .await
                    .map_err(|error| DaemonError::Protocol(error.to_string()))?,
            )
            .await;
        serde_json::to_value(result).map_err(DaemonError::from)
    }

    async fn execute_recovered_goal(
        &self,
        arguments: &str,
    ) -> std::result::Result<Value, DaemonError> {
        use crate::orchestration::GoalStatus;

        let store = GoalStore::new(&self.state_root);
        let arguments = arguments.trim();
        let goal = match arguments.to_ascii_lowercase().as_str() {
            "" | "status" => store.load().await,
            "clear" | "stop" => store.clear().await.map(|()| None),
            "pause" => store.set_status(GoalStatus::Paused).await.map(Some),
            "resume" => store.set_status(GoalStatus::Active).await.map(Some),
            _ => {
                let (budget, objective) = parse_recovered_goal_create(arguments)
                    .map_err(|error| DaemonError::Protocol(error.to_string()))?;
                store.create(objective, budget).await.map(Some)
            }
        }
        .map_err(|error| DaemonError::Protocol(error.to_string()))?;
        serde_json::to_value(json!({"goal": goal})).map_err(DaemonError::from)
    }

    async fn execute_recovered_autonomous(
        &self,
        session_id: &str,
        runtime: &AgentRuntime,
        arguments: &str,
    ) -> std::result::Result<Value, DaemonError> {
        use std::time::Instant;

        let mut states = self.autonomous_states.lock().await;
        if !states.contains_key(session_id) && states.len() >= 1024 {
            return Err(DaemonError::Protocol(
                "autonomous session-state limit reached".into(),
            ));
        }
        let state = states.entry(session_id.into()).or_default();
        let now = Instant::now();
        let cancelled = match arguments.trim().to_ascii_lowercase().as_str() {
            "" | "status" => false,
            "on" => {
                state.enable(now);
                false
            }
            "off" => {
                state.disable();
                true
            }
            "cancel" => {
                state.cancel_current();
                true
            }
            _ => {
                return Err(DaemonError::Protocol(
                    "autonomous args must be on, off, status, or cancel".into(),
                ));
            }
        };
        let autonomous_status = state.status(now);
        drop(states);
        if cancelled {
            runtime.cancel();
        }
        Ok(json!({"status": autonomous_status, "cancelled": cancelled}))
    }
}

fn parse_recovered_refine_args(
    arguments: &str,
) -> std::result::Result<(Option<String>, Option<String>, bool), DaemonError> {
    let mut rest = arguments.trim();
    let mut global = false;
    if let Some(value) = rest.strip_prefix("--global") {
        if !value.is_empty() && !value.starts_with(char::is_whitespace) {
            return Err(DaemonError::Protocol(
                "invalid refine --global option".into(),
            ));
        }
        global = true;
        rest = value.trim_start();
    }
    if rest == "rollback" {
        return Err(DaemonError::Protocol(
            "refine rollback requires a refinement id".into(),
        ));
    }
    if let Some(value) = rest.strip_prefix("rollback ") {
        let mut rollback_id = value.trim();
        if let Some(value) = rollback_id.strip_suffix(" --global") {
            global = true;
            rollback_id = value.trim_end();
        }
        if rollback_id.is_empty() {
            return Err(DaemonError::Protocol(
                "refine rollback requires a refinement id".into(),
            ));
        }
        return Ok((None, Some(rollback_id.into()), global));
    }
    Ok(((!rest.is_empty()).then(|| rest.into()), None, global))
}

fn autonomous_status_value(state: Option<&AutonomousState>) -> Value {
    let defaults = AutonomousState::default();
    let state = state.unwrap_or(&defaults);
    let limits = state.limits();
    json!({
        "enabled": state.enabled(),
        "status": state.status(std::time::Instant::now()),
        "continuationsUsed": state.continuations_used(),
        "turnsUsed": state.turns_used(),
        "tokensUsed": state.tokens_used(),
        "limits": {
            "maxContinuations": limits.max_continuations,
            "maxTurns": limits.max_turns,
            "maxTokens": limits.max_tokens,
            "timeoutMs": u64::try_from(limits.timeout.as_millis()).unwrap_or(u64::MAX)
        },
        "gates": {
            "configuredCount": state.quality_gate_count()
        }
    })
}

fn append_custom_content(
    target: &mut Vec<Content>,
    custom: &Value,
) -> std::result::Result<(), DaemonError> {
    let custom_type = custom
        .get("customType")
        .and_then(Value::as_str)
        .unwrap_or("custom");
    match custom.get("content") {
        Some(Value::String(text)) => target.push(Content::Text {
            text: format!("[custom:{custom_type}]\n{text}\n[/custom]"),
        }),
        Some(Value::Array(blocks)) => {
            target.push(Content::Text {
                text: format!("[custom:{custom_type}]"),
            });
            for block in blocks {
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => target.push(Content::Text {
                        text: block
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .into(),
                    }),
                    Some("image") => {
                        let image: PublicImageContent = serde_json::from_value(block.clone())?;
                        image.validate().map_err(DaemonError::Protocol)?;
                        target.push(Content::Image {
                            data: image.data,
                            mime_type: image.mime_type,
                        });
                    }
                    _ => {
                        return Err(DaemonError::Protocol(
                            "custom content contains an invalid block".into(),
                        ));
                    }
                }
            }
            target.push(Content::Text {
                text: "[/custom]".into(),
            });
        }
        _ => {
            return Err(DaemonError::Protocol(
                "custom message content is unavailable".into(),
            ));
        }
    }
    Ok(())
}

fn parse_recovered_goal_create(
    arguments: &str,
) -> std::result::Result<(Option<u64>, &str), DaemonError> {
    let Some(flag) = arguments.split_whitespace().next() else {
        return Err(DaemonError::Protocol(
            "goal requires an objective or status action".into(),
        ));
    };
    if !matches!(flag, "--budget" | "--token-budget")
        && !flag.starts_with("--budget=")
        && !flag.starts_with("--token-budget=")
    {
        return Ok((None, arguments));
    }
    let (value, objective) = if let Some((_, value)) = flag.split_once('=') {
        (value, arguments[flag.len()..].trim())
    } else {
        let rest = arguments[flag.len()..].trim_start();
        let split = rest
            .find(char::is_whitespace)
            .ok_or_else(|| DaemonError::Protocol("goal budget requires an objective".into()))?;
        (&rest[..split], rest[split..].trim())
    };
    let budget = value
        .parse::<u64>()
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| DaemonError::Protocol("goal budget must be positive".into()))?;
    if objective.is_empty() {
        return Err(DaemonError::Protocol(
            "goal budget requires an objective".into(),
        ));
    }
    Ok((Some(budget), objective))
}

struct CliTuiRuntimeFactory {
    build: RuntimeBuildConfig,
    explicit_base_url: Option<String>,
    bash_runner: std::sync::RwLock<Arc<BashRunner>>,
    agent_mode: std::sync::RwLock<AgentMode>,
}

#[async_trait]
impl TuiRuntimeFactory for CliTuiRuntimeFactory {
    async fn build(&self, model: &str, session: &str) -> Result<Arc<AgentRuntime>> {
        let mut build = self.build.clone();
        build.agent_mode = self.agent_mode();
        let (provider, model) = resolve_tui_model_selection(model, &build.provider)?;
        build.provider = provider.clone();
        build.model = model;
        build.base_url = self
            .explicit_base_url
            .clone()
            .or_else(|| provider_base_url_from_env(&provider));
        let runtime = build_runtime_for_session(&build, session).await?;
        let (active_provider, active_model, _) = runtime.model_selection().await;
        if active_provider != build.provider || active_model != build.model {
            let state = resolve_state_dir(&build.state_dir)?;
            let provider = build_provider_for_runtime(&build, &state).await?;
            let definition = runtime_model_definition(&build)?;
            runtime
                .select_model(
                    provider,
                    &build.provider,
                    &build.model,
                    definition.thinking_levels(),
                    definition.thinking_level_map.clone(),
                )
                .await?;
        }
        Ok(runtime)
    }

    fn agent_mode(&self) -> AgentMode {
        *self
            .agent_mode
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn set_agent_mode(&self, mode: AgentMode) -> Result<()> {
        let mut build = self.build.clone();
        build.agent_mode = mode;
        let bash_runner = build_bash_runner(&build)?;
        *self
            .bash_runner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = bash_runner;
        *self
            .agent_mode
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = mode;
        Ok(())
    }

    async fn execute_user_bash(
        &self,
        runtime: &AgentRuntime,
        command: &str,
        exclude_from_context: bool,
    ) -> Result<BashResult> {
        let workspace = std::fs::canonicalize(&self.build.workspace).map_err(|error| {
            MimirError::Configuration(format!("workspace is inaccessible: {error}"))
        })?;
        let workspace_text = workspace.display().to_string();
        let (command, requested_cwd) = runtime
            .intercept_user_bash(command, Some(&workspace_text))
            .await?;
        if requested_cwd
            .as_deref()
            .is_some_and(|cwd| cwd != workspace_text)
        {
            return Err(MimirError::Configuration(
                "extension user_bash cwd overrides are unavailable in the bounded runner".into(),
            ));
        }
        let bash_runner = self
            .bash_runner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let result = bash_runner
            .execute(&command)
            .await
            .map_err(|error| MimirError::Tool(error.to_string()))?;
        if !exclude_from_context {
            runtime.record_bash_execution(&command, &result).await?;
        }
        Ok(result)
    }

    fn abort_user_bash(&self) {
        self.bash_runner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .abort();
    }

    async fn record_workspace_permission(
        &self,
        request: crate::tools::PermissionRequest,
        decision: crate::tools::ApprovalDecision,
    ) -> Result<String> {
        let store = WorkspaceApprovalStore::new(&self.build.workspace)
            .map_err(|error| MimirError::Tool(error.to_string()))?;
        store
            .record(&request, decision)
            .map_err(|error| MimirError::Tool(error.to_string()))?;
        Ok(match decision {
            crate::tools::ApprovalDecision::AllowOnce => {
                "Allowed once. Retry the command to run it.".into()
            }
            crate::tools::ApprovalDecision::AlwaysAllowWorkspace => {
                "Allowed for this workspace and recorded in .mimir/workspace-permissions.json."
                    .into()
            }
            crate::tools::ApprovalDecision::Deny => {
                "Denied. The decision was recorded in the workspace audit log.".into()
            }
        })
    }

    async fn resource_snapshot(&self) -> Result<TuiResourceSnapshot> {
        let workspace = std::fs::canonicalize(&self.build.workspace).map_err(|error| {
            MimirError::Configuration(format!("workspace is inaccessible: {error}"))
        })?;
        let state = resolve_state_dir(&self.build.state_dir)?;
        let resources = load_runtime_resources(&self.build, &state, &workspace).await?;
        let mut skills = if self.build.no_skills {
            Vec::new()
        } else {
            load_migrated_skills(&state)
                .map_err(|error| MimirError::Configuration(error.to_string()))?
        };
        skills.extend(resources.skills);
        let skills = skills
            .into_iter()
            .map(|skill| (skill.name.clone(), skill))
            .collect::<BTreeMap<_, _>>()
            .into_values()
            .collect();
        Ok(TuiResourceSnapshot {
            prompt_templates: resources.prompt_templates,
            themes: resources.themes,
            skills,
        })
    }
}

fn tui_model_key(provider: &str, model: &str) -> String {
    format!("{provider}/{model}")
}

fn tui_model_options(initial_provider: &str, initial_model: &str) -> Vec<String> {
    let registry = ProviderRegistry::builtin();
    let mut options = model_catalog()
        .iter()
        .filter(|model| {
            registry
                .get(&model.provider)
                .and_then(|provider| provider.runtime_for_model(model))
                .is_some()
        })
        .map(|model| tui_model_key(&model.provider, &model.id))
        .collect::<BTreeSet<_>>();
    options.insert(tui_model_key(initial_provider, initial_model));
    options.into_iter().collect()
}

fn wildcard_matches(pattern: &str, value: &str) -> bool {
    let (mut pattern_index, mut value_index, mut star, mut retry) = (0, 0, None, 0);
    let pattern = pattern.as_bytes();
    let value = value.as_bytes();
    while value_index < value.len() {
        if pattern_index < pattern.len()
            && (pattern[pattern_index] == b'?' || pattern[pattern_index] == value[value_index])
        {
            pattern_index += 1;
            value_index += 1;
        } else if pattern_index < pattern.len() && pattern[pattern_index] == b'*' {
            star = Some(pattern_index);
            pattern_index += 1;
            retry = value_index;
        } else if let Some(star_index) = star {
            pattern_index = star_index + 1;
            retry += 1;
            value_index = retry;
        } else {
            return false;
        }
    }
    pattern[pattern_index..].iter().all(|byte| *byte == b'*')
}

fn filter_tui_models(
    models: Vec<String>,
    patterns: &[String],
    initial_selection: &str,
) -> Result<Vec<String>> {
    if patterns.is_empty() {
        return Ok(models);
    }
    let mut selected = models
        .into_iter()
        .filter(|model| {
            patterns
                .iter()
                .any(|pattern| wildcard_matches(pattern, model))
        })
        .collect::<Vec<_>>();
    if selected.is_empty() {
        return Err(MimirError::Configuration(format!(
            "--models matched no available model: {}",
            patterns.join(", ")
        )));
    }
    if !selected.iter().any(|model| model == initial_selection) {
        selected.insert(0, initial_selection.to_owned());
    }
    Ok(selected)
}

fn resolve_tui_model_selection(
    selection: &str,
    fallback_provider: &str,
) -> Result<(String, String)> {
    let registry = ProviderRegistry::builtin();
    if let Some((provider, model)) = selection.split_once('/') {
        if provider == "fake" && fallback_provider == "fake" && !model.trim().is_empty() {
            return Ok((provider.into(), model.into()));
        }
        let definition = registry.get(provider).ok_or_else(|| {
            MimirError::Configuration(format!("unknown model provider: {provider}"))
        })?;
        let selected = ModelDefinition::from_runtime(
            provider,
            model,
            definition.base_url,
            definition.runtime_support,
        );
        if model.trim().is_empty() || definition.runtime_for_model(&selected).is_none() {
            return Err(MimirError::Configuration(format!(
                "model selection is unavailable: {selection}"
            )));
        }
        return Ok((provider.into(), model.into()));
    }
    let mut matches = model_catalog()
        .iter()
        .filter(|model| model.id == selection)
        .filter(|model| {
            registry
                .get(&model.provider)
                .and_then(|provider| provider.runtime_for_model(model))
                .is_some()
        });
    let first = matches.next();
    if let Some(model) = first
        && matches.next().is_none()
    {
        return Ok((model.provider.clone(), model.id.clone()));
    }
    Ok((fallback_provider.into(), selection.into()))
}

fn provider_base_url_from_env(provider: &str) -> Option<String> {
    let env_var = match provider {
        "openai" | "openai-codex" => "OPENAI_BASE_URL",
        "anthropic" => "ANTHROPIC_BASE_URL",
        "google" => "GEMINI_BASE_URL",
        "google-vertex" => "GOOGLE_VERTEX_BASE_URL",
        "amazon-bedrock" => "AWS_BEDROCK_BASE_URL",
        "mistral" => "MISTRAL_BASE_URL",
        "azure-openai-responses" => "AZURE_OPENAI_BASE_URL",
        _ => return None,
    };
    std::env::var(env_var)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

fn ambient_credentials_configured(
    provider: &str,
    definition: &crate::provider::registry::ProviderDefinition,
) -> bool {
    match provider {
        "amazon-bedrock" => {
            std::env::var("AWS_BEARER_TOKEN_BEDROCK").is_ok_and(|value| !value.trim().is_empty())
                || (std::env::var("AWS_ACCESS_KEY_ID").is_ok_and(|value| !value.trim().is_empty())
                    && std::env::var("AWS_SECRET_ACCESS_KEY")
                        .is_ok_and(|value| !value.trim().is_empty()))
                || std::env::var("AWS_PROFILE").is_ok_and(|value| !value.trim().is_empty())
        }
        "google-vertex" => {
            definition.environment_variable().is_some()
                || std::env::var("GOOGLE_OAUTH_ACCESS_TOKEN")
                    .is_ok_and(|value| !value.trim().is_empty())
                || std::env::var_os("GOOGLE_APPLICATION_CREDENTIALS")
                    .is_some_and(|path| PathBuf::from(path).is_file())
                || std::env::var_os("HOME").is_some_and(|home| {
                    PathBuf::from(home)
                        .join(".config/gcloud/application_default_credentials.json")
                        .is_file()
                })
                || std::env::var("GCE_METADATA_HOST").is_ok_and(|value| !value.trim().is_empty())
        }
        _ => false,
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "the daemon adapter keeps each PromptHandler capability explicit and independently auditable"
)]
#[async_trait]
impl PromptHandler for RuntimePromptHandler {
    async fn handle_prompt(
        &self,
        request: PromptRequest,
    ) -> std::result::Result<String, DaemonError> {
        let gate = self.headless_gate(&request.session_id).await;
        let _headless_guard = gate.lock().await;
        let runtime = self
            .runtime(&request.session_id)
            .await
            .map_err(|error| DaemonError::Protocol(error.to_string()))?;
        let diagnostics = self
            .start_diagnostic_collector(&request.session_id, runtime.as_ref())
            .await;
        let result = async {
            self.dispatch_recovered_commands(
                &request.session_id,
                runtime.as_ref(),
                Some(crate::daemon::turn_ops::RecoveredDelivery::NextTurn),
            )
            .await?;
            let mut answer = runtime
                .run(&request.prompt, &StdoutEventSink { enabled: false })
                .await
                .map_err(daemon_runtime_error)?;
            if let Some(follow_up_answer) = self
                .run_pending_follow_ups(&request.session_id, runtime.as_ref())
                .await
                .map_err(daemon_runtime_error)?
            {
                answer = follow_up_answer;
            }
            self.dispatch_recovered_commands(
                &request.session_id,
                runtime.as_ref(),
                Some(crate::daemon::turn_ops::RecoveredDelivery::WhenIdle),
            )
            .await?;
            self.run_autonomous_continuations(&request.session_id, runtime.as_ref(), answer)
                .await
        }
        .await;
        diagnostics.finish().await;
        result
    }

    async fn handle_prompt_message(
        &self,
        request: PromptRequest,
        message: Message,
    ) -> std::result::Result<String, DaemonError> {
        let gate = self.headless_gate(&request.session_id).await;
        let _headless_guard = gate.lock().await;
        let runtime = self
            .runtime(&request.session_id)
            .await
            .map_err(|error| DaemonError::Protocol(error.to_string()))?;
        let diagnostics = self
            .start_diagnostic_collector(&request.session_id, runtime.as_ref())
            .await;
        let result = async {
            self.dispatch_recovered_commands(
                &request.session_id,
                runtime.as_ref(),
                Some(crate::daemon::turn_ops::RecoveredDelivery::NextTurn),
            )
            .await?;
            let mut answer = runtime
                .run_batch_messages(&[message], &StdoutEventSink { enabled: false })
                .await
                .map_err(daemon_runtime_error)?;
            if let Some(follow_up_answer) = self
                .run_pending_follow_ups(&request.session_id, runtime.as_ref())
                .await
                .map_err(daemon_runtime_error)?
            {
                answer = follow_up_answer;
            }
            self.dispatch_recovered_commands(
                &request.session_id,
                runtime.as_ref(),
                Some(crate::daemon::turn_ops::RecoveredDelivery::WhenIdle),
            )
            .await?;
            self.run_autonomous_continuations(&request.session_id, runtime.as_ref(), answer)
                .await
        }
        .await;
        diagnostics.finish().await;
        result
    }

    async fn session_messages(
        &self,
        session_id: &str,
    ) -> std::result::Result<Option<Vec<Message>>, DaemonError> {
        Ok(Some(
            self.runtime(session_id)
                .await
                .map_err(|error| DaemonError::Protocol(error.to_string()))?
                .messages_snapshot()
                .await,
        ))
    }

    async fn session_system_prompt(
        &self,
        session_id: &str,
    ) -> std::result::Result<Option<String>, DaemonError> {
        Ok(Some(
            self.runtime(session_id)
                .await
                .map_err(|error| DaemonError::Protocol(error.to_string()))?
                .system_prompt_snapshot()
                .await,
        ))
    }

    async fn session_queue(
        &self,
        session_id: &str,
    ) -> std::result::Result<Option<Value>, DaemonError> {
        let steering = self
            .runtime(session_id)
            .await
            .map_err(|error| DaemonError::Protocol(error.to_string()))?
            .pending_steering_previews()
            .await;
        let steering_mode = self
            .runtime(session_id)
            .await
            .map_err(|error| DaemonError::Protocol(error.to_string()))?
            .steering_mode()
            .await;
        let follow_up = self
            .follow_ups
            .lock()
            .await
            .get(session_id)
            .map(|queue| {
                queue
                    .iter()
                    .map(|item| item.message.text())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let follow_up_mode = self
            .follow_up_modes
            .lock()
            .await
            .get(session_id)
            .copied()
            .unwrap_or_default();
        Ok(Some(json!({
            "steering": steering,
            "followUp": follow_up,
            "queuedCount": steering.len().saturating_add(follow_up.len()),
            "steeringMode": steering_mode.as_str(),
            "followUpMode": follow_up_mode.as_str()
        })))
    }

    async fn session_runtime_state(
        &self,
        session_id: &str,
    ) -> std::result::Result<Option<Value>, DaemonError> {
        let runtime = self
            .runtime(session_id)
            .await
            .map_err(|error| DaemonError::Protocol(error.to_string()))?;
        let (provider, model, thinking_level) = runtime.model_selection().await;
        let available_thinking_levels = runtime.supported_thinking_levels().await;
        let steering_mode = runtime.steering_mode().await;
        let steering = runtime.pending_steering_previews().await;
        let message_count = runtime.messages_snapshot().await.len();
        let bash_running = self
            .bash_runners
            .lock()
            .await
            .get(session_id)
            .is_some_and(|runner| runner.is_running());
        let scoped_models = self
            .scoped_models
            .lock()
            .await
            .get(session_id)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(|selection| {
                json!({
                    "provider": selection.provider,
                    "modelId": selection.model_id,
                    "thinkingLevel": selection.thinking_level
                })
            })
            .collect::<Vec<_>>();
        let transport = self
            .transports
            .lock()
            .await
            .get(session_id)
            .cloned()
            .unwrap_or_else(|| "sse".into());
        let follow_up_mode = self
            .follow_up_modes
            .lock()
            .await
            .get(session_id)
            .copied()
            .unwrap_or_default();
        let follow_ups = self
            .follow_ups
            .lock()
            .await
            .get(session_id)
            .map(|queue| {
                queue
                    .iter()
                    .map(|item| item.message.text())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let service_tier = runtime
            .service_tier()
            .await
            .unwrap_or_else(|| "auto".into());
        let compaction_count = runtime
            .compaction_count()
            .await
            .map_err(|error| DaemonError::Protocol(error.to_string()))?;
        let active_tool_names = runtime.active_tool_names().await;
        let goal = GoalStore::new(&self.state_root)
            .load()
            .await
            .map_err(|error| DaemonError::Protocol(error.to_string()))?;
        let autonomous = {
            let states = self.autonomous_states.lock().await;
            autonomous_status_value(states.get(session_id))
        };
        let queued_count = steering.len().saturating_add(follow_ups.len());
        Ok(Some(json!({
            "cwd": std::env::current_dir()
                .map(|path| path.display().to_string())
                .unwrap_or_default(),
            "model": {
                "provider": provider,
                "id": model.clone(),
                "name": model
            },
            "thinkingLevel": thinking_level,
            "serviceTier": service_tier,
            "transport": transport,
            "availableThinkingLevels": available_thinking_levels,
            "isStreaming": runtime.is_running(),
            "isCompacting": runtime.is_compacting(),
            "isBashRunning": bash_running,
            "isRetrying": runtime.is_retrying(),
            "retryAttempt": runtime.retry_attempt(),
            "steeringMode": steering_mode.as_str(),
            "followUpMode": follow_up_mode.as_str(),
            "autoCompactionEnabled": runtime.auto_compaction_enabled(),
            "autoRetryEnabled": runtime.auto_retry_enabled(),
            "messageCount": message_count,
            "compactionCount": compaction_count,
            "goal": goal,
            "autonomous": autonomous,
            "scopedModels": scoped_models,
            "activeToolNames": active_tool_names,
            "sessionActions": {
                "queuedCount": queued_count,
                "steering": steering,
                "followUps": follow_ups
            }
        })))
    }

    async fn subscribe_session_events(
        &self,
        session_id: &str,
    ) -> std::result::Result<
        Option<tokio::sync::broadcast::Receiver<crate::runtime_events::RuntimeEventEnvelope>>,
        DaemonError,
    > {
        Ok(Some(
            self.runtime(session_id)
                .await
                .map_err(|error| DaemonError::Protocol(error.to_string()))?
                .subscribe_events(),
        ))
    }

    async fn steer_session(
        &self,
        session_id: &str,
        message: Message,
        command: &PublicDaemonCommand,
    ) -> std::result::Result<bool, DaemonError> {
        let runtime = self
            .runtime(session_id)
            .await
            .map_err(|error| DaemonError::Protocol(error.to_string()))?;
        if runtime.pending_steering_count().await >= 64 {
            return Err(DaemonError::Protocol(
                "steering queue reached its 64-message limit".into(),
            ));
        }
        if !self.admit_queued_message(session_id, command, false).await {
            return Ok(true);
        }
        let message = self
            .prepare_queued_message(runtime.as_ref(), message, command)
            .await?;
        let was_running = runtime.is_running();
        runtime
            .steer_message(message)
            .await
            .map_err(|error| DaemonError::Protocol(error.to_string()))?;
        if !was_running {
            self.dispatch_recovered_commands(
                session_id,
                runtime.as_ref(),
                Some(crate::daemon::turn_ops::RecoveredDelivery::NextTurn),
            )
            .await?;
            runtime
                .run_pending_steering(&StdoutEventSink { enabled: false })
                .await
                .map_err(daemon_runtime_error)?;
            self.dispatch_recovered_commands(
                session_id,
                runtime.as_ref(),
                Some(crate::daemon::turn_ops::RecoveredDelivery::WhenIdle),
            )
            .await?;
        }
        Ok(true)
    }

    async fn follow_up_session(
        &self,
        session_id: &str,
        message: Message,
        command: &PublicDaemonCommand,
    ) -> std::result::Result<Option<bool>, DaemonError> {
        let runtime = self
            .runtime(session_id)
            .await
            .map_err(|error| DaemonError::Protocol(error.to_string()))?;
        {
            let mut queues = self.follow_ups.lock().await;
            let queue = queues.entry(session_id.into()).or_default();
            if queue.len() >= 64 {
                return Err(DaemonError::Protocol(
                    "follow-up queue reached its 64-message limit".into(),
                ));
            }
        }
        if !self.admit_queued_message(session_id, command, true).await {
            return Ok(Some(false));
        }
        let message = self
            .prepare_queued_message(runtime.as_ref(), message, command)
            .await?;
        self.follow_ups
            .lock()
            .await
            .entry(session_id.into())
            .or_default()
            .push_back(QueuedFollowUp {
                message,
                queue_key: command
                    .field("queueKey")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            });
        if !runtime.is_running() {
            self.dispatch_recovered_commands(
                session_id,
                runtime.as_ref(),
                Some(crate::daemon::turn_ops::RecoveredDelivery::NextTurn),
            )
            .await?;
            self.run_pending_follow_ups(session_id, runtime.as_ref())
                .await
                .map_err(daemon_runtime_error)?;
            self.dispatch_recovered_commands(
                session_id,
                runtime.as_ref(),
                Some(crate::daemon::turn_ops::RecoveredDelivery::WhenIdle),
            )
            .await?;
        }
        Ok(Some(true))
    }

    async fn rebind_session(
        &self,
        active_session_id: &str,
        durable_session_id: &str,
    ) -> std::result::Result<bool, DaemonError> {
        if let Some(runtime) = self.runtimes.lock().await.get(active_session_id).cloned() {
            runtime.wait_for_idle().await;
        }
        let runtime = build_runtime_for_session(&self.build, durable_session_id)
            .await
            .map_err(|error| DaemonError::Protocol(error.to_string()))?;
        self.runtimes
            .lock()
            .await
            .insert(active_session_id.into(), runtime);
        self.durable_bindings
            .lock()
            .await
            .insert(active_session_id.into(), durable_session_id.into());
        self.follow_ups.lock().await.remove(active_session_id);
        self.follow_up_modes.lock().await.remove(active_session_id);
        self.recovered_commands
            .lock()
            .await
            .remove(active_session_id);
        self.autonomous_states
            .lock()
            .await
            .remove(active_session_id);
        self.queued_message_ids
            .lock()
            .await
            .remove(active_session_id);
        self.follow_up_queue_keys
            .lock()
            .await
            .remove(active_session_id);
        self.headless_gates.lock().await.remove(active_session_id);
        if let Some(runner) = self.bash_runners.lock().await.remove(active_session_id) {
            runner.abort();
        }
        Ok(true)
    }

    async fn close_session(&self, session_id: &str) -> std::result::Result<bool, DaemonError> {
        let runtime = self.runtimes.lock().await.remove(session_id);
        if let Some(runtime) = runtime {
            runtime.cancel();
            runtime.clear_steering().await;
        }
        self.durable_bindings.lock().await.remove(session_id);
        self.follow_ups.lock().await.remove(session_id);
        self.follow_up_modes.lock().await.remove(session_id);
        self.scoped_models.lock().await.remove(session_id);
        self.transports.lock().await.remove(session_id);
        self.recovered_commands.lock().await.remove(session_id);
        self.autonomous_states.lock().await.remove(session_id);
        self.queued_message_ids.lock().await.remove(session_id);
        self.follow_up_queue_keys.lock().await.remove(session_id);
        self.headless_gates.lock().await.remove(session_id);
        if let Some(runner) = self.bash_runners.lock().await.remove(session_id) {
            runner.abort();
        }
        Ok(true)
    }

    async fn handle_session_control(
        &self,
        session_id: &str,
        command: &PublicDaemonCommand,
    ) -> std::result::Result<Option<Value>, DaemonError> {
        let runtime = self
            .runtime(session_id)
            .await
            .map_err(|error| DaemonError::Protocol(error.to_string()))?;
        let result = match command.command_type() {
            "restore_next_turn" | "restore_actions" | "append_custom_message" | "resume_queue" => {
                let recovery =
                    crate::daemon::turn_ops::parse_turn_recovery(command)?.ok_or_else(|| {
                        DaemonError::Protocol("turn recovery command was not recognized".into())
                    })?;
                return self
                    .handle_turn_recovery(session_id, recovery)
                    .await
                    .map(Some);
            }
            "abort" => {
                runtime.cancel();
                Value::Null
            }
            "clear_queue" | "abort_and_clear_queue" => {
                let steering = runtime.clear_steering().await;
                let follow_up = self
                    .follow_ups
                    .lock()
                    .await
                    .remove(session_id)
                    .map(|queue| {
                        queue
                            .into_iter()
                            .map(|item| item.message.text())
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                let recovered_commands = self
                    .recovered_commands
                    .lock()
                    .await
                    .remove(session_id)
                    .map(|queue| {
                        queue
                            .into_iter()
                            .filter_map(|action| match action.payload {
                                crate::daemon::turn_ops::RecoveredActionPayload::SessionCommand(
                                    command,
                                ) => Some(format!("{:?} {}", command.name, command.args)),
                                crate::daemon::turn_ops::RecoveredActionPayload::Turn(_) => None,
                            })
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                self.follow_up_queue_keys.lock().await.remove(session_id);
                if command.command_type() == "abort_and_clear_queue" {
                    runtime.cancel();
                }
                json!({"steering": steering, "followUp": follow_up, "commands": recovered_commands})
            }
            "set_model" => {
                let provider = command
                    .field("provider")
                    .and_then(Value::as_str)
                    .ok_or_else(|| DaemonError::Protocol("provider must be a string".into()))?;
                let model = command
                    .field("modelId")
                    .and_then(Value::as_str)
                    .ok_or_else(|| DaemonError::Protocol("modelId must be a string".into()))?;
                let (selected, _) = self
                    .select_runtime_model(runtime.as_ref(), provider, model)
                    .await
                    .map_err(|error| DaemonError::Protocol(error.to_string()))?;
                serde_json::to_value(selected)?
            }
            "cycle_model" => {
                let direction = match command.field("direction").and_then(Value::as_str) {
                    None | Some("forward") => 1_isize,
                    Some("backward") => -1,
                    Some(_) => {
                        return Err(DaemonError::Protocol(
                            "direction must be forward or backward".into(),
                        ));
                    }
                };
                let scoped = self
                    .scoped_models
                    .lock()
                    .await
                    .get(session_id)
                    .cloned()
                    .unwrap_or_default();
                let is_scoped = !scoped.is_empty();
                let choices = if is_scoped {
                    scoped
                } else {
                    runtime_available_models(&self.build, runtime.as_ref())
                        .await
                        .map_err(|error| DaemonError::Protocol(error.to_string()))?
                        .into_iter()
                        .map(|model| ScopedModelSelection {
                            provider: model.provider,
                            model_id: model.id,
                            thinking_level: None,
                        })
                        .collect()
                };
                if choices.len() <= 1 {
                    Value::Null
                } else {
                    let (provider, model, _) = runtime.model_selection().await;
                    let current = choices
                        .iter()
                        .position(|entry| entry.provider == provider && entry.model_id == model)
                        .unwrap_or(0);
                    let next = if direction > 0 {
                        (current + 1) % choices.len()
                    } else {
                        (current + choices.len() - 1) % choices.len()
                    };
                    let target = &choices[next];
                    let (selected, mut thinking_level) = self
                        .select_runtime_model(runtime.as_ref(), &target.provider, &target.model_id)
                        .await
                        .map_err(|error| DaemonError::Protocol(error.to_string()))?;
                    if let Some(requested) = target.thinking_level {
                        thinking_level = runtime
                            .set_thinking_level(requested)
                            .await
                            .map_err(|error| DaemonError::Protocol(error.to_string()))?;
                    }
                    json!({
                        "model": selected,
                        "thinkingLevel": thinking_level,
                        "isScoped": is_scoped
                    })
                }
            }
            "set_scoped_models" => {
                let value = command
                    .field("scopedModels")
                    .cloned()
                    .ok_or_else(|| DaemonError::Protocol("scopedModels must be an array".into()))?;
                let selections: Vec<ScopedModelSelection> = serde_json::from_value(value)?;
                if selections.len() > 64
                    || selections.iter().any(|selection| {
                        selection.provider.trim().is_empty() || selection.model_id.trim().is_empty()
                    })
                {
                    return Err(DaemonError::Protocol(
                        "scopedModels must contain at most 64 non-empty model selections".into(),
                    ));
                }
                let available = runtime_available_models(&self.build, runtime.as_ref())
                    .await
                    .map_err(|error| DaemonError::Protocol(error.to_string()))?;
                if let Some(selection) = selections.iter().find(|selection| {
                    !available.iter().any(|model| {
                        model.provider == selection.provider && model.id == selection.model_id
                    })
                }) {
                    return Err(DaemonError::Protocol(format!(
                        "Model not found: {}/{}",
                        selection.provider, selection.model_id
                    )));
                }
                self.scoped_models
                    .lock()
                    .await
                    .insert(session_id.into(), selections);
                Value::Null
            }
            "set_transport" => {
                let transport = command
                    .field("transport")
                    .and_then(Value::as_str)
                    .ok_or_else(|| DaemonError::Protocol("transport must be a string".into()))?;
                match transport {
                    "auto" | "sse" => {
                        // Native providers already execute through their streaming SSE
                        // path. `auto` therefore resolves to the same effective mode.
                        let effective = "sse";
                        let mut transports = self.transports.lock().await;
                        let previous = transports.get(session_id).map_or("sse", String::as_str);
                        let changed = previous != effective;
                        if changed {
                            transports.insert(session_id.into(), effective.into());
                        }
                        json!({
                            "requestedTransport": transport,
                            "transport": effective,
                            "changed": changed
                        })
                    }
                    "websocket" | "websocket-cached" => {
                        return Err(DaemonError::Protocol(format!(
                            "transport {transport} is not supported by the native Rust HTTP/SSE provider"
                        )));
                    }
                    _ => {
                        return Err(DaemonError::Protocol(
                            "transport must be auto, sse, websocket, or websocket-cached".into(),
                        ));
                    }
                }
            }
            "extension_ui_response" => {
                let request_id = command
                    .field("requestId")
                    .and_then(Value::as_str)
                    .ok_or_else(|| DaemonError::Protocol("requestId must be a string".into()))?;
                let response = command
                    .field("response")
                    .cloned()
                    .ok_or_else(|| DaemonError::Protocol("response must be present".into()))?;
                runtime
                    .respond_extension_ui(request_id, response)
                    .await
                    .map_err(|error| DaemonError::Protocol(error.to_string()))?;
                Value::Null
            }
            "set_thinking_level" => {
                let level = command.field("level").and_then(Value::as_str);
                let requested = parse_thinking_level(level).map_err(|error| {
                    DaemonError::Protocol(format!("invalid thinking level: {error}"))
                })?;
                runtime
                    .set_thinking_level(requested)
                    .await
                    .map_err(|error| DaemonError::Protocol(error.to_string()))?;
                Value::Null
            }
            "set_service_tier" => {
                let tier = match command.field("serviceTier") {
                    Some(Value::String(value)) => Some(value.as_str()),
                    Some(Value::Null) => None,
                    _ => {
                        return Err(DaemonError::Protocol(
                            "serviceTier must be a string or null".into(),
                        ));
                    }
                };
                runtime
                    .set_service_tier(tier)
                    .await
                    .map_err(|error| DaemonError::Protocol(error.to_string()))?;
                Value::Null
            }
            "cycle_thinking_level" => runtime
                .cycle_thinking_level()
                .await
                .map_err(|error| DaemonError::Protocol(error.to_string()))?
                .map_or(Value::Null, |level| json!({"level": level})),
            "set_steering_mode" => {
                let mode = parse_queue_mode(command.field("mode").and_then(Value::as_str))
                    .map_err(|error| DaemonError::Protocol(error.into()))?;
                runtime.set_steering_mode(mode).await;
                Value::Null
            }
            "set_follow_up_mode" => {
                let mode = parse_queue_mode(command.field("mode").and_then(Value::as_str))
                    .map_err(|error| DaemonError::Protocol(error.into()))?;
                self.follow_up_modes
                    .lock()
                    .await
                    .insert(session_id.into(), mode);
                Value::Null
            }
            "set_auto_compaction" => {
                let enabled = command
                    .field("enabled")
                    .and_then(Value::as_bool)
                    .ok_or_else(|| DaemonError::Protocol("enabled must be a boolean".into()))?;
                runtime.set_auto_compaction(enabled);
                Value::Null
            }
            "set_auto_retry" => {
                let enabled = command
                    .field("enabled")
                    .and_then(Value::as_bool)
                    .ok_or_else(|| DaemonError::Protocol("enabled must be a boolean".into()))?;
                runtime.set_auto_retry(enabled);
                Value::Null
            }
            "compact" => {
                let custom_instructions =
                    command.field("customInstructions").and_then(Value::as_str);
                serde_json::to_value(
                    runtime
                        .compact(custom_instructions)
                        .await
                        .map_err(|error| DaemonError::Protocol(error.to_string()))?,
                )?
            }
            "abort_retry" => {
                runtime.abort_retry();
                Value::Null
            }
            "wait_for_idle" => {
                runtime.wait_for_idle().await;
                Value::Null
            }
            "wait_for_headless_completion" => {
                let gate = self.headless_gate(session_id).await;
                let _headless_guard = gate.lock().await;
                runtime.wait_for_idle().await;
                let states = self.autonomous_states.lock().await;
                autonomous_status_value(states.get(session_id))
            }
            "execute_bash_and_wait" => {
                let shell_command = command
                    .field("command")
                    .and_then(Value::as_str)
                    .ok_or_else(|| DaemonError::Protocol("command must be a string".into()))?;
                let workspace = std::fs::canonicalize(&self.build.workspace)
                    .map_err(|error| DaemonError::Protocol(error.to_string()))?;
                let workspace_text = workspace.display().to_string();
                let (shell_command, requested_cwd) = runtime
                    .intercept_user_bash(shell_command, Some(&workspace_text))
                    .await
                    .map_err(|error| DaemonError::Protocol(error.to_string()))?;
                if requested_cwd
                    .as_deref()
                    .is_some_and(|cwd| cwd != workspace_text)
                {
                    return Err(DaemonError::Protocol(
                        "extension user_bash cwd overrides are unavailable in the bounded runner"
                            .into(),
                    ));
                }
                let result = self
                    .bash_runner(session_id)
                    .await
                    .map_err(|error| DaemonError::Protocol(error.to_string()))?
                    .execute(&shell_command)
                    .await
                    .map_err(|error| DaemonError::Protocol(error.to_string()))?;
                runtime
                    .record_bash_execution(&shell_command, &result)
                    .await
                    .map_err(|error| DaemonError::Protocol(error.to_string()))?;
                serde_json::to_value(result)?
            }
            "abort_bash" => {
                if let Some(runner) = self.bash_runners.lock().await.get(session_id) {
                    runner.abort();
                }
                Value::Null
            }
            "reload" => {
                runtime.wait_for_idle().await;
                let durable_session_id = self
                    .durable_bindings
                    .lock()
                    .await
                    .get(session_id)
                    .cloned()
                    .unwrap_or_else(|| session_id.into());
                let replacement = build_runtime_for_session(&self.build, &durable_session_id)
                    .await
                    .map_err(|error| DaemonError::Protocol(error.to_string()))?;
                self.runtimes
                    .lock()
                    .await
                    .insert(session_id.into(), replacement);
                runtime.cancel();
                runtime.clear_steering().await;
                self.follow_ups.lock().await.remove(session_id);
                if let Some(runner) = self.bash_runners.lock().await.remove(session_id) {
                    runner.abort();
                }
                Value::Null
            }
            "get_commands" => json!({
                "commands": runtime_resource_commands(&self.build)
                    .map_err(|error| DaemonError::Protocol(error.to_string()))?
            }),
            "get_resource_snapshot" => runtime_resource_snapshot(&self.build)
                .await
                .map_err(|error| DaemonError::Protocol(error.to_string()))?,
            "get_model_catalog" => serde_json::to_value(model_catalog())?,
            "get_available_models" => json!({
                "models": runtime_available_models(&self.build, runtime.as_ref())
                    .await
                    .map_err(|error| DaemonError::Protocol(error.to_string()))?
            }),
            "get_tool_definition" => {
                let name = command
                    .field("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| DaemonError::Protocol("name must be a string".into()))?;
                json!({"toolDefinition": runtime.tool_definition(name)})
            }
            _ => return Ok(None),
        };
        Ok(Some(result))
    }

    async fn handle_runtime_operation(
        &self,
        client_id: &str,
        session_id: &str,
        command: &PublicDaemonCommand,
    ) -> std::result::Result<Option<Value>, DaemonError> {
        let runtime = self
            .runtime(session_id)
            .await
            .map_err(|error| DaemonError::Protocol(error.to_string()))?;
        let bash_runner = if command.command_type() == "execute_bash" {
            Some(
                self.bash_runner(session_id)
                    .await
                    .map_err(|error| DaemonError::Protocol(error.to_string()))?,
            )
        } else {
            None
        };
        self.runtime_operations
            .handle_for_client(client_id, session_id, command, runtime, bash_runner)
            .await
    }

    async fn summarize_navigation(
        &self,
        session_id: &str,
        command: &PublicDaemonCommand,
        messages: Vec<Message>,
    ) -> std::result::Result<Option<String>, DaemonError> {
        let runtime = self
            .runtime(session_id)
            .await
            .map_err(|error| DaemonError::Protocol(error.to_string()))?;
        self.runtime_operations
            .summarize_branch(session_id, command, runtime, &messages)
            .await
            .map(Some)
    }

    async fn handle_scheduled_prompt(
        &self,
        request: PromptRequest,
        delivery: ScheduledPromptDelivery,
    ) -> std::result::Result<String, DaemonError> {
        let runtime = self
            .runtime(&request.session_id)
            .await
            .map_err(|error| DaemonError::Protocol(error.to_string()))?;
        if delivery == ScheduledPromptDelivery::Steer && runtime.is_running() {
            runtime
                .steer(&request.prompt)
                .await
                .map_err(|error| DaemonError::Protocol(error.to_string()))?;
            return runtime
                .run_pending_steering(&StdoutEventSink { enabled: false })
                .await
                .map(|answer| answer.unwrap_or_else(|| "heartbeat steered active turn".into()))
                .map_err(daemon_runtime_error);
        }
        runtime
            .run(&request.prompt, &StdoutEventSink { enabled: false })
            .await
            .map_err(daemon_runtime_error)
    }

    async fn handle_agent_message(
        &self,
        request: AgentMessageRequest,
    ) -> std::result::Result<AgentMessageDelivery, DaemonError> {
        let state = resolve_state_dir(&self.build.state_dir)
            .map_err(|error| DaemonError::Protocol(error.to_string()))?;
        let store = FileSessionStore::create(&state, &request.target_session_id)
            .await
            .map_err(|error| DaemonError::Protocol(error.to_string()))?;
        if !tokio::fs::try_exists(store.path())
            .await
            .map_err(DaemonError::Io)?
        {
            return Err(DaemonError::Protocol(format!(
                "unknown active session: {}",
                request.target_session_id
            )));
        }
        let target_session_name = loaded_session_name(&store)
            .await
            .map_err(|error| DaemonError::Protocol(error.to_string()))?;
        let runtime = self
            .runtime(&request.target_session_id)
            .await
            .map_err(|error| DaemonError::Protocol(error.to_string()))?;
        let queued = runtime.is_running();
        if queued {
            runtime
                .queue_agent_message(&request.prompt)
                .await
                .map_err(|error| DaemonError::Protocol(error.to_string()))?;
        } else {
            runtime
                .run(&request.prompt, &StdoutEventSink { enabled: false })
                .await
                .map_err(daemon_runtime_error)?;
        }
        Ok(AgentMessageDelivery {
            queued,
            target_session_name,
        })
    }

    async fn clear_agent_messages(
        &self,
        session_id: &str,
    ) -> std::result::Result<usize, DaemonError> {
        let runtime = self.runtimes.lock().await.get(session_id).cloned();
        Ok(match runtime {
            Some(runtime) => runtime.clear_queued_agent_messages().await,
            None => 0,
        })
    }

    async fn clear_all_agent_messages(&self) -> std::result::Result<usize, DaemonError> {
        let runtimes = self
            .runtimes
            .lock()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut cleared = 0_usize;
        for runtime in runtimes {
            cleared = cleared.saturating_add(runtime.clear_queued_agent_messages().await);
        }
        Ok(cleared)
    }
}

async fn loaded_session_name(store: &FileSessionStore) -> Result<Option<String>> {
    Ok(store.load().await?.records.iter().rev().find_map(|record| {
        if let SessionPayload::RuntimeEvent { name, detail } = &record.payload
            && name == "session_name"
        {
            return Some(detail.clone());
        }
        None
    }))
}

#[allow(
    clippy::too_many_lines,
    reason = "daemon lifecycle commands share one explicit routing boundary"
)]
async fn run_daemon_management(
    cli: &Cli,
    action: &DaemonCommand,
    state: &std::path::Path,
) -> Result<()> {
    let mut config = daemon_config(state);
    if let Some(socket) = &cli.socket {
        config.socket_path.clone_from(socket);
    }
    match action {
        DaemonCommand::Serve => {
            let build = RuntimeBuildConfig::from_cli(cli);
            build_runtime_for_session(&build, &cli.session).await?;
            let handle = DaemonServer::spawn(
                config,
                Arc::new(RuntimePromptHandler::new(build, state.to_path_buf())),
            )
            .await
            .map_err(daemon_error)?;
            handle.wait().await.map_err(daemon_error)
        }
        DaemonCommand::Start => {
            if DaemonClient::connect(&config.socket_path).await.is_ok() {
                return Err(MimirError::Configuration(
                    "daemon is already running".into(),
                ));
            }
            start_daemon_process(&RuntimeBuildConfig::from_cli(cli), &cli.session, &config).await?;
            print_json(&json!({
                "status": "started",
                "socket": config.socket_path
            }))
        }
        DaemonCommand::Status => {
            let mut client = DaemonClient::connect(&config.socket_path)
                .await
                .map_err(daemon_error)?;
            let response = client
                .request(ClientRequest::Health)
                .await
                .map_err(daemon_error)?;
            print_json(&serde_json::to_value(response)?)
        }
        DaemonCommand::Prompt { prompt } => {
            let mut client = DaemonClient::connect(&config.socket_path)
                .await
                .map_err(daemon_error)?;
            let attached = client
                .request(ClientRequest::attach(&cli.session, "mimir-cli"))
                .await
                .map_err(daemon_error)?;
            let ServerResponse::SessionAttached(attached) = attached else {
                return Err(MimirError::Protocol(
                    "daemon returned an invalid attach response".into(),
                ));
            };
            let response = client
                .request(ClientRequest::prompt(
                    &attached.lease.lease_id.to_string(),
                    &cli.session,
                    prompt,
                ))
                .await
                .map_err(daemon_error)?;
            print_json(&serde_json::to_value(response)?)
        }
        DaemonCommand::Stop => {
            let mut client = DaemonClient::connect(&config.socket_path)
                .await
                .map_err(daemon_error)?;
            let response = client
                .request(ClientRequest::Shutdown)
                .await
                .map_err(daemon_error)?;
            print_json(&serde_json::to_value(response)?)
        }
    }
}

async fn start_daemon_process(
    build: &RuntimeBuildConfig,
    session: &str,
    config: &DaemonConfig,
) -> Result<()> {
    let executable = std::env::current_exe()?;
    let mut command = std::process::Command::new(executable);
    command
        .arg("--provider")
        .arg(&build.provider)
        .arg("--model")
        .arg(&build.model)
        .arg("--workspace")
        .arg(&build.workspace)
        .arg("--state-dir")
        .arg(&build.state_dir)
        .arg("--session")
        .arg(session);
    if let Some(base_url) = &build.base_url {
        command.arg("--base-url").arg(base_url);
    }
    if build.allow_process {
        command.arg("--allow-process");
    }
    if build.no_builtin_tools {
        command.arg("--no-builtin-tools");
    }
    if build.no_extensions {
        command.arg("--no-extensions");
    }
    for extension in &build.extension_paths {
        command.arg("--extension").arg(extension);
    }
    for flag in &build.extension_flags {
        command.arg("--extension-flag").arg(flag);
    }
    if config.socket_path != daemon_config(&config.state_root).socket_path {
        command.arg("--socket").arg(&config.socket_path);
    }
    if !build.allowed_programs.is_empty() {
        command
            .arg("--allowed-programs")
            .arg(build.allowed_programs.join(","));
    }
    for response in &build.fake_responses {
        command.arg("--fake-response").arg(response);
    }
    if build.fake_delay_ms != 0 {
        command
            .arg("--fake-delay-ms")
            .arg(build.fake_delay_ms.to_string());
    }
    if build.fake_retryable_failures != 0 {
        command
            .arg("--fake-retryable-failures")
            .arg(build.fake_retryable_failures.to_string());
    }
    let mut child = command
        .args(["daemon", "serve"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    for _ in 0..200 {
        if DaemonClient::connect(&config.socket_path).await.is_ok() {
            return Ok(());
        }
        if let Some(status) = child.try_wait()? {
            return Err(MimirError::Protocol(format!(
                "daemon exited before becoming ready: {status}"
            )));
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    Err(MimirError::Protocol(
        "daemon did not become ready within 10 seconds".into(),
    ))
}

async fn ensure_rpc_daemon(context: &RpcSessionContext, state: &std::path::Path) -> Result<()> {
    let config = daemon_config(state);
    if DaemonClient::connect(&config.socket_path).await.is_err() {
        start_daemon_process(&context.build, &context.session_id, &config).await?;
    }
    Ok(())
}

fn daemon_config(state: &std::path::Path) -> DaemonConfig {
    DaemonConfig {
        state_root: state.to_owned(),
        socket_path: state.join("daemon/mimir.sock"),
        server_name: "mimir".into(),
        lease_ttl: std::time::Duration::from_secs(30),
        supported_capabilities: BTreeSet::from([
            "health".into(),
            "prompt".into(),
            "session_catalog".into(),
            "shutdown".into(),
        ]),
    }
}

fn daemon_error(error: DaemonError) -> MimirError {
    let message = error.to_string();
    drop(error);
    MimirError::Protocol(message)
}

fn print_json(value: &Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn resolve_state_dir(path: &std::path::Path) -> Result<std::path::PathBuf> {
    std::fs::create_dir_all(path)?;
    let state = canonical_state_root(path);
    match std::fs::symlink_metadata(&state) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(MimirError::Configuration(
            format!("state directory must not be a symlink: {}", path.display()),
        )),
        Ok(metadata) if metadata.is_dir() => Ok(state),
        Ok(_) => Err(MimirError::Configuration(format!(
            "state path is not a directory: {}",
            path.display()
        ))),
        Err(error) => Err(error.into()),
    }
}

fn read_api_key_from_stdin() -> Result<String> {
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let secret = input.trim_end_matches(['\r', '\n']).to_owned();
    if secret.trim().is_empty() {
        return Err(MimirError::Configuration(
            "credential must not be blank".into(),
        ));
    }
    Ok(secret)
}

async fn doctor(cli: &Cli) -> Result<()> {
    let workspace = std::fs::canonicalize(&cli.workspace).map_err(|error| {
        MimirError::Configuration(format!("workspace is inaccessible: {error}"))
    })?;
    let state = resolve_state_dir(&cli.state_dir)?;
    let registry = ProviderRegistry::builtin();
    let provider_id = resolved_cli_provider(cli);
    let (credential_status, credential_source, runtime_support) = if provider_id == "fake" {
        (
            "not_required".to_owned(),
            "none".to_owned(),
            "offline_fake".to_owned(),
        )
    } else {
        let provider = registry
            .get(&provider_id)
            .ok_or_else(|| MimirError::Configuration(format!("unknown provider: {provider_id}")))?;
        let store = AuthStore::new(&state)?;
        let (credential_status, credential_source) =
            if let Some(stored) = store.get(&provider_id).await? {
                let source = match stored {
                    AuthCredential::ApiKey { .. } => "stored_api_key",
                    AuthCredential::OAuth(_) => "stored_oauth",
                };
                ("present".to_owned(), source.to_owned())
            } else if provider.environment_key().is_some() {
                ("present".to_owned(), "environment".to_owned())
            } else {
                ("missing".to_owned(), "none".to_owned())
            };
        (
            credential_status,
            credential_source,
            provider.runtime_support.as_str().to_owned(),
        )
    };
    println!("workspace: {}", workspace.display());
    println!("state: {}", state.display());
    println!("provider: {provider_id}");
    println!("model: {}", resolved_cli_model(cli, &provider_id));
    let base_url_source = if cli.base_url.is_some() {
        "command_line"
    } else if matches!(provider_id.as_str(), "openai" | "openai-codex")
        && std::env::var("OPENAI_BASE_URL").is_ok_and(|value| !value.trim().is_empty())
    {
        "openai_environment"
    } else if provider_id == "anthropic"
        && std::env::var("ANTHROPIC_BASE_URL").is_ok_and(|value| !value.trim().is_empty())
    {
        "anthropic_environment"
    } else if provider_id == "google"
        && std::env::var("GEMINI_BASE_URL").is_ok_and(|value| !value.trim().is_empty())
    {
        "google_environment"
    } else if provider_id == "azure-openai-responses"
        && std::env::var("AZURE_OPENAI_BASE_URL").is_ok_and(|value| !value.trim().is_empty())
    {
        "azure_environment"
    } else {
        "provider_default"
    };
    println!("base_url_source: {base_url_source}");
    println!("runtime_support: {runtime_support}");
    println!("credential: {credential_status}");
    println!("credential_source: {credential_source}");
    println!("status: ready");
    Ok(())
}

#[allow(
    clippy::too_many_lines,
    reason = "runtime construction keeps provider, extension, policy, and session wiring auditable"
)]
async fn build_runtime(cli: &Cli) -> Result<Arc<AgentRuntime>> {
    build_runtime_for_session(&RuntimeBuildConfig::from_cli(cli), &cli.session).await
}

async fn restored_runtime_build(
    build: &RuntimeBuildConfig,
    session_root: &std::path::Path,
    session: &str,
) -> Result<(RuntimeBuildConfig, ThinkingLevel)> {
    if build.no_session {
        return Ok((build.clone(), build.default_thinking_level));
    }
    let store = create_run_session_store(build, session_root, session).await?;
    let loaded = store.load().await?;
    let mut restored = build.clone();
    let mut thinking_level = build.default_thinking_level;
    for record in loaded.records.iter().rev() {
        let SessionPayload::RuntimeEvent { name, detail } = &record.payload else {
            continue;
        };
        if name != "model_selection" {
            continue;
        }
        let value: Value = serde_json::from_str(detail).map_err(|error| MimirError::Session {
            path: store.path().to_owned(),
            message: format!("invalid persisted model selection: {error}"),
        })?;
        let provider = value
            .get("provider")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| MimirError::Session {
                path: store.path().to_owned(),
                message: "persisted model selection is missing provider".into(),
            })?;
        let model = value
            .get("model")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| MimirError::Session {
                path: store.path().to_owned(),
                message: "persisted model selection is missing model".into(),
            })?;
        restored.provider = provider.into();
        restored.model = model.into();
        thinking_level = value
            .get("thinkingLevel")
            .cloned()
            .map(serde_json::from_value)
            .transpose()?
            .unwrap_or(ThinkingLevel::Off);
        break;
    }
    Ok((restored, thinking_level))
}

fn resolve_session_root(build: &RuntimeBuildConfig, state: &Path) -> Result<PathBuf> {
    build
        .session_dir
        .as_deref()
        .map_or_else(|| Ok(state.to_owned()), resolve_state_dir)
}

fn activate_migrated_preferences(
    build: &RuntimeBuildConfig,
    activation: &MigratedRuntimeState,
) -> RuntimeBuildConfig {
    let Some(preferences) = &activation.preferences else {
        return build.clone();
    };
    let mut activated = build.clone();
    let previous_provider = activated.provider.clone();
    if !activated.provider_explicit
        && let Some(provider) = preferences.default_provider.as_deref()
    {
        provider.clone_into(&mut activated.provider);
    }
    if activated.provider != previous_provider {
        activated.base_url = None;
    }
    if !activated.model_explicit {
        let preference_matches_provider = preferences
            .default_provider
            .as_deref()
            .is_none_or(|provider| provider == activated.provider);
        activated.model = preferences
            .default_model
            .as_deref()
            .filter(|_| preference_matches_provider)
            .map_or_else(
                || {
                    ProviderRegistry::builtin()
                        .get(&activated.provider)
                        .and_then(|provider| provider.default_model)
                        .unwrap_or("gpt-5-mini")
                        .to_owned()
                },
                str::to_owned,
            );
    }
    activated.default_thinking_level = preferences.default_thinking_level.unwrap_or_default();
    activated
}

/// Uses an unambiguous saved login when the caller did not choose a provider.
///
/// The CLI historically defaulted to `OpenAI` before consulting the auth store,
/// which made `mimir login anthropic` insufficient for a bare `mimir` launch.
/// A command-line provider and migrated preferences retain precedence; this
/// fallback applies only when one stored credential can run natively.
async fn activate_single_stored_provider(
    build: &RuntimeBuildConfig,
    state: &std::path::Path,
) -> Result<RuntimeBuildConfig> {
    if build.provider_explicit {
        return Ok(build.clone());
    }
    let registry = ProviderRegistry::builtin();
    let mut providers = AuthStore::new(state)?
        .statuses()
        .await?
        .into_iter()
        .filter_map(|status| {
            registry
                .get(&status.provider)
                .filter(|provider| provider.supports_runtime())
                .map(|_| status.provider)
        })
        .collect::<Vec<_>>();
    providers.sort();
    providers.dedup();
    let [provider] = providers.as_slice() else {
        return Ok(build.clone());
    };

    let mut activated = build.clone();
    activated.provider.clone_from(provider);
    activated.base_url = None;
    if !activated.model_explicit {
        registry
            .get(provider)
            .and_then(|definition| definition.default_model)
            .unwrap_or("gpt-5-mini")
            .clone_into(&mut activated.model);
    }
    Ok(activated)
}

fn runtime_model_definition(build: &RuntimeBuildConfig) -> Result<ModelDefinition> {
    if build.provider == "fake" {
        return Ok(ModelDefinition::fake(&build.model));
    }
    let state = resolve_state_dir(&build.state_dir)?;
    if let Some(mut migrated) =
        MigratedRuntimeState::load(&state)?.model(&build.provider, &build.model)
    {
        if let Some(base_url) = build.base_url.as_deref() {
            base_url.clone_into(&mut migrated.base_url);
        }
        return Ok(migrated);
    }
    let registry = ProviderRegistry::builtin();
    let definition = registry.get(&build.provider).ok_or_else(|| {
        MimirError::Configuration(format!("unknown provider: {}", build.provider))
    })?;
    if build.api_key.is_some()
        && !definition
            .auth
            .contains(&crate::provider::registry::AuthKind::ApiKey)
    {
        return Err(MimirError::Configuration(format!(
            "{} does not accept --api-key",
            build.provider
        )));
    }
    if !definition.supports_runtime() {
        return Err(MimirError::Configuration(format!(
            "{} does not have a native Rust runtime",
            build.provider
        )));
    }
    let model = ModelDefinition::from_runtime(
        &build.provider,
        &build.model,
        build.base_url.as_deref().or(definition.base_url),
        definition.runtime_support,
    );
    definition.runtime_for_model(&model).ok_or_else(|| {
        MimirError::Configuration(format!(
            "model {}/{} uses unsupported API {}",
            build.provider, build.model, model.api
        ))
    })?;
    Ok(model)
}

fn resolve_runtime_thinking_level(
    explicit: Option<ThinkingLevel>,
    restored: ThinkingLevel,
    supported: &[ThinkingLevel],
    provider: &str,
    model: &str,
) -> Result<ThinkingLevel> {
    let requested = explicit.unwrap_or(restored);
    if explicit.is_none() && !supported.contains(&requested) {
        return supported.first().copied().ok_or_else(|| {
            MimirError::Configuration(format!(
                "provider {provider}/{model} exposes no supported thinking levels"
            ))
        });
    }
    if !supported.contains(&requested) {
        return Err(MimirError::Configuration(format!(
            "thinking level '{}' is unavailable for {provider}/{model}",
            requested.as_str()
        )));
    }
    Ok(requested)
}

#[allow(
    clippy::too_many_lines,
    reason = "provider construction keeps credential resolution and transport selection auditable"
)]
async fn build_provider_for_runtime(
    build: &RuntimeBuildConfig,
    state: &std::path::Path,
) -> Result<Arc<dyn Provider>> {
    if build.provider == "fake" {
        if build.fake_responses.is_empty() {
            return Err(MimirError::Configuration(
                "fake provider requires at least one --fake-response".into(),
            ));
        }
        let mut results =
            Vec::with_capacity(build.fake_responses.len() + build.fake_retryable_failures as usize);
        results.extend((0..build.fake_retryable_failures).map(|_| {
            Err(ProviderError::RateLimited {
                message: "scripted fake rate limit".into(),
            })
        }));
        results.extend(build.fake_responses.iter().map(|text| {
            Ok(ModelResponse {
                message: Message::assistant(
                    vec![Content::Text { text: text.clone() }],
                    StopReason::Stop,
                ),
                response_id: Some("fake-cli".into()),
            })
        }));
        return Ok(Arc::new(FakeProvider::with_results(results).with_delay(
            std::time::Duration::from_millis(build.fake_delay_ms),
        )));
    }

    let registry = ProviderRegistry::builtin();
    let activation = MigratedRuntimeState::load(state)?;
    let definition = registry.get(&build.provider);
    let custom_openai = activation.custom_openai_provider(&build.provider);
    if definition.is_none() && custom_openai.is_none() {
        return Err(MimirError::Configuration(format!(
            "unknown provider: {}",
            build.provider
        )));
    }
    if custom_openai.is_some() && build.api_key.is_some() {
        return Err(MimirError::Configuration(format!(
            "migrated custom provider {} accepts credentials only through its audited environment reference",
            build.provider
        )));
    }
    if custom_openai.is_some() && build.base_url.is_some() {
        return Err(MimirError::Configuration(format!(
            "migrated custom provider {} owns its audited endpoint and does not accept --base-url",
            build.provider
        )));
    }
    if build.api_key.is_some()
        && definition.is_some_and(|definition| {
            !definition
                .auth
                .contains(&crate::provider::registry::AuthKind::ApiKey)
        })
    {
        return Err(MimirError::Configuration(format!(
            "{} does not accept --api-key",
            build.provider
        )));
    }
    if definition.is_some_and(|definition| !definition.supports_runtime()) {
        return Err(MimirError::Configuration(format!(
            "{} is cataloged for auth/discovery only and does not have a native Rust runtime yet",
            build.provider
        )));
    }
    let auth = AuthStore::new(state)?;
    #[allow(
        clippy::redundant_closure_for_method_calls,
        reason = "the migrated provider type is intentionally private outside its module"
    )]
    let environment_name = custom_openai
        .map(|provider| provider.credential_env())
        .or_else(|| {
            definition.and_then(crate::provider::registry::ProviderDefinition::environment_variable)
        });
    let environment = build
        .api_key
        .clone()
        .or_else(|| environment_name.and_then(|name| std::env::var(name).ok()));
    if custom_openai.is_some() && environment.is_none() {
        return Err(MimirError::Configuration(format!(
            "{} credential is missing; set {}",
            build.provider,
            environment_name.unwrap_or("the configured credential environment variable")
        )));
    }
    let mut stored_oauth = None;
    if environment.is_none()
        && definition.is_some()
        && let Some(AuthCredential::OAuth(credential)) = auth.get(&build.provider).await?
    {
        let credential = if let Some(oauth_provider) = OAuthProvider::from_id(&build.provider) {
            refresh_stored_oauth_if_expired(&auth, &build.provider, oauth_provider, credential)
                .await?
        } else {
            credential
        };
        stored_oauth = Some(credential);
    }
    let model_definition = runtime_model_definition(build)?;
    let runtime_support = if let Some(definition) = definition {
        definition
            .runtime_for_model(&model_definition)
            .ok_or_else(|| {
                MimirError::Configuration(format!(
                    "model {}/{} uses unsupported API {}",
                    build.provider, build.model, model_definition.api
                ))
            })?
    } else if model_definition.api == "openai-completions" {
        RuntimeSupport::OpenAiCompatible
    } else {
        return Err(MimirError::Configuration(format!(
            "model {}/{} uses unsupported custom-provider API {}",
            build.provider, build.model, model_definition.api
        )));
    };
    let environment_type = if build.api_key.is_some() {
        CredentialType::ApiKey
    } else if environment_name == Some("ANTHROPIC_OAUTH_TOKEN") {
        CredentialType::OAuthToken
    } else {
        CredentialType::ApiKey
    };
    let credential = resolve_credential_typed(
        &auth,
        &build.provider,
        environment
            .as_deref()
            .map(|value| (value, environment_type)),
    )
    .await?;

    if runtime_support == RuntimeSupport::BedrockConverseStream {
        let profile = std::env::var("AWS_PROFILE")
            .ok()
            .filter(|value| !value.trim().is_empty());
        let base_url = build
            .base_url
            .as_deref()
            .or((!model_definition.base_url.is_empty())
                .then_some(model_definition.base_url.as_str()));
        let region = resolve_bedrock_region(None, base_url, profile.as_deref())
            .await
            .map_err(|error| MimirError::Provider(error.to_string()))?;
        let provider = if let Some(credential) = credential {
            BedrockProvider::new_bearer(&region, base_url, credential.expose_for_provider())
        } else {
            BedrockProvider::new_ambient(&region, base_url, profile.as_deref())
        };
        return provider
            .map(|provider| Arc::new(provider) as Arc<dyn Provider>)
            .map_err(|error| MimirError::Provider(error.to_string()));
    }

    if runtime_support == RuntimeSupport::GoogleVertex {
        let base_url = build
            .base_url
            .as_deref()
            .or((!model_definition.base_url.is_empty())
                .then_some(model_definition.base_url.as_str()));
        let provider = if let Some(credential) = credential {
            VertexProvider::with_api_key(base_url, credential.expose_for_provider())
        } else {
            let adc = GoogleAdcResolver::from_process_env()
                .map_err(|error| MimirError::Provider(error.to_string()))?
                .resolve()
                .await
                .map_err(|error| MimirError::Provider(error.to_string()))?;
            VertexProvider::with_bearer_token_and_quota_project(
                base_url,
                adc.project_id.clone(),
                adc.location.clone(),
                adc.exposed_access_token(),
                adc.quota_project_id.clone(),
            )
        };
        return provider
            .map(|provider| Arc::new(provider) as Arc<dyn Provider>)
            .map_err(|error| MimirError::Provider(error.to_string()));
    }

    let credential = credential.ok_or_else(|| {
        MimirError::Configuration(format!(
            "{} credential is missing; run `mimir login {}` or set {}",
            build.provider,
            build.provider,
            definition
                .and_then(|definition| definition.env_vars.first().copied())
                .or(environment_name)
                .unwrap_or("the provider credential")
        ))
    })?;
    if build.provider == "openai-codex" {
        let account_id = match auth.get(&build.provider).await? {
            Some(AuthCredential::OAuth(value)) => value.account_id,
            Some(AuthCredential::ApiKey { .. }) | None => None,
        };
        return CodexProvider::new(
            build.base_url.as_deref(),
            credential.expose_for_provider(),
            account_id.as_deref(),
        )
        .map(|provider| Arc::new(provider) as Arc<dyn Provider>)
        .map_err(|error| MimirError::Provider(error.to_string()));
    }
    let raw_base_url = build
        .base_url
        .as_deref()
        .or((!model_definition.base_url.is_empty()).then_some(model_definition.base_url.as_str()))
        .or(definition.and_then(|definition| definition.base_url))
        .ok_or_else(|| {
            MimirError::Configuration(format!(
                "{} requires --base-url for its OpenAI-compatible endpoint",
                build.provider
            ))
        })?;
    let cloudflare_provider = match build.provider.as_str() {
        "cloudflare-workers-ai" => Some(CloudflareProvider::WorkersAi),
        "cloudflare-ai-gateway" => Some(CloudflareProvider::AiGateway),
        _ => None,
    };
    let resolved_cloudflare_url = cloudflare_provider
        .map(|provider| {
            CloudflareConfig::from_environment(provider)
                .and_then(|config| config.resolve_base_url(raw_base_url))
                .map_err(|error| MimirError::Configuration(error.to_string()))
        })
        .transpose()?;
    let base_url = resolved_cloudflare_url.as_deref().unwrap_or(raw_base_url);
    if model_definition.api == "anthropic-messages" {
        let credential_kind = if build.provider == "cloudflare-ai-gateway" {
            AnthropicCredentialKind::CloudflareGateway
        } else if credential.auth_type == "oauth" {
            AnthropicCredentialKind::OAuthToken
        } else {
            AnthropicCredentialKind::ApiKey
        };
        let empty_headers = BTreeMap::new();
        if build.provider == "anthropic"
            && credential_kind == AnthropicCredentialKind::OAuthToken
            && let Some(oauth) = stored_oauth
        {
            return RefreshingOAuthProvider::anthropic(
                auth,
                &build.provider,
                oauth,
                base_url.into(),
                model_definition
                    .headers
                    .as_ref()
                    .unwrap_or(&empty_headers)
                    .clone(),
            )
            .map(|provider| Arc::new(provider) as Arc<dyn Provider>)
            .map_err(|error| MimirError::Provider(error.to_string()));
        }
        return AnthropicProvider::with_credential_kind_and_headers(
            Some(base_url),
            credential.expose_for_provider(),
            credential_kind,
            model_definition.headers.as_ref().unwrap_or(&empty_headers),
        )
        .map(|provider| Arc::new(provider) as Arc<dyn Provider>)
        .map_err(|error| MimirError::Provider(error.to_string()));
    }
    if model_definition.api == "google-generative-ai" {
        return GoogleProvider::new(Some(base_url), credential.expose_for_provider())
            .map(|provider| Arc::new(provider) as Arc<dyn Provider>)
            .map_err(|error| MimirError::Provider(error.to_string()));
    }
    if model_definition.api == "mistral-conversations" {
        let empty_headers = BTreeMap::new();
        return MistralProvider::with_headers(
            Some(base_url),
            credential.expose_for_provider(),
            model_definition.headers.as_ref().unwrap_or(&empty_headers),
        )
        .map(|provider| Arc::new(provider) as Arc<dyn Provider>)
        .map_err(|error| MimirError::Provider(error.to_string()));
    }
    match model_definition.api.as_str() {
        "openai-responses" => (if build.provider == "cloudflare-ai-gateway" {
            ResponsesProvider::new_cloudflare_gateway(
                Some(base_url),
                credential.expose_for_provider(),
            )
        } else {
            ResponsesProvider::new(Some(base_url), credential.expose_for_provider())
        })
        .map(|provider| Arc::new(provider) as Arc<dyn Provider>)
        .map_err(|error| MimirError::Provider(error.to_string())),
        "azure-openai-responses" => ResponsesProvider::new_azure(
            Some(base_url),
            credential.expose_for_provider(),
            std::env::var("AZURE_OPENAI_API_VERSION").ok().as_deref(),
        )
        .map(|provider| Arc::new(provider) as Arc<dyn Provider>)
        .map_err(|error| MimirError::Provider(error.to_string())),
        "openai-completions" => (if build.provider == "cloudflare-ai-gateway" {
            let mut config =
                ProviderConfig::openai(base_url, &build.model, credential.expose_for_provider())?;
            config.name.clone_from(&build.provider);
            OpenAiProvider::new_cloudflare_gateway_with_compat(
                config,
                model_definition.compat.as_ref(),
            )
        } else {
            let mut config =
                ProviderConfig::openai(base_url, &build.model, credential.expose_for_provider())?;
            config.name.clone_from(&build.provider);
            OpenAiProvider::new_with_compat(config, model_definition.compat.as_ref())
        })
        .map(|provider| Arc::new(provider) as Arc<dyn Provider>)
        .map_err(|error| MimirError::Provider(error.to_string())),
        api => Err(MimirError::Configuration(format!(
            "model {}/{} requires unsupported API {api}",
            build.provider, build.model
        ))),
    }
}

/// Reuses the audited CLI credential/provider construction without allowing
/// credential material into RLM state or tool payloads.
struct CliRlmProviderFactory {
    template: RuntimeBuildConfig,
    state: PathBuf,
}

#[async_trait]
impl RlmProviderFactory for CliRlmProviderFactory {
    async fn create_provider(&self, model: &RlmModel) -> Result<Arc<dyn Provider>> {
        let mut build = self.template.clone();
        let inherits_parent_endpoint = build.provider == model.provider;
        build.provider.clone_from(&model.provider);
        build.model.clone_from(&model.id);
        // A parent-specific override must never be applied to a child using a
        // different provider. A sibling model on the same provider deliberately
        // retains it, including private/local compatible endpoints.
        if !inherits_parent_endpoint {
            build.base_url = None;
        }
        build_provider_for_runtime(&build, &self.state).await
    }
}

struct CliRlmChildToolFactory {
    base_tools: Arc<ToolRegistry>,
    providers: Arc<dyn RlmProviderFactory>,
    catalog: Arc<dyn AuthenticatedModelCatalog>,
    state: PathBuf,
    workspace: PathBuf,
    policy: RlmChildRuntimePolicy,
    limits: RlmRuntimeLimits,
    kernel_policy: Option<ToolPolicy>,
}

#[async_trait]
impl RlmChildToolRegistryFactory for CliRlmChildToolFactory {
    async fn tools_for_child(
        self: Arc<Self>,
        request: &RlmExecutionRequest,
    ) -> Result<Arc<ToolRegistry>> {
        let mut tools = self.base_tools.fork_for_child_runtime();
        if let Some(policy) = self.kernel_policy.clone() {
            tools
                .register_ipython_kernel(&self.workspace, &self.state, &request.session_id, policy)
                .map_err(|error| MimirError::Tool(error.to_string()))?;
        }
        // A depth-N agent receives spawning tools only when a depth-(N+1)
        // child can still be admitted. Max-depth agents retain all non-RLM
        // tools and execute normally.
        if request.depth < self.limits.max_depth {
            let child_executor = Arc::new(
                AgentRuntimeChildExecutor::new(
                    Arc::clone(&self.providers),
                    Arc::clone(&self.base_tools),
                    self.policy.clone(),
                )
                .with_child_tool_factory(Arc::clone(&self) as Arc<dyn RlmChildToolRegistryFactory>),
            );
            let parent_session_path = request
                .session_dir
                .join("sessions")
                .join(format!("{}.jsonl", request.session_id));
            let runtime = Arc::new(
                RlmRuntime::open(
                    &self.state,
                    &self.workspace,
                    &request.session_id,
                    parent_session_path.to_str(),
                    request.depth,
                    &request.model.selector(),
                    Arc::clone(&self.catalog),
                    child_executor,
                    self.limits.clone(),
                )
                .await?,
            );
            tools
                .register_rlm_runtime(runtime)
                .map_err(|error| MimirError::Tool(error.to_string()))?;
        }
        Ok(Arc::new(tools))
    }
}

fn build_bash_runner(build: &RuntimeBuildConfig) -> Result<Arc<BashRunner>> {
    let workspace = std::fs::canonicalize(&build.workspace).map_err(|error| {
        MimirError::Configuration(format!("workspace is inaccessible: {error}"))
    })?;
    let policy = ToolPolicy {
        allow_process: !build.agent_mode.is_plan()
            && (build.allow_process || build.agent_mode == AgentMode::Auto),
        allow_any_program: build.agent_mode == AgentMode::Auto,
        agent_mode: build.agent_mode,
        allowed_programs: Some(build.allowed_programs.clone()),
        approvals: Some(Arc::new(
            WorkspaceApprovalStore::new(&workspace)
                .map_err(|error| MimirError::Tool(error.to_string()))?,
        )),
        ..ToolPolicy::default()
    };
    BashRunner::new(&workspace, policy)
        .map(Arc::new)
        .map_err(|error| MimirError::Tool(error.to_string()))
}

async fn load_runtime_extensions(
    build: &RuntimeBuildConfig,
    workspace: &Path,
    state: &Path,
) -> Result<Vec<CatalogEntry>> {
    let mut entries = if build.no_extensions {
        Vec::new()
    } else {
        let mut catalog = ExtensionCatalog::new(workspace, state)?;
        let mut entries = catalog.reload().await?;
        if let Some(local_state) = distinct_project_local_state(workspace, state) {
            let mut local_catalog = ExtensionCatalog::new(workspace, &local_state)?;
            let local_entries = local_catalog.reload().await?;
            let mut indexes = entries
                .iter()
                .enumerate()
                .map(|(index, entry)| (entry.manifest.name.clone(), index))
                .collect::<BTreeMap<_, _>>();
            for entry in local_entries {
                if let Some(index) = indexes.get(&entry.manifest.name).copied() {
                    entries[index] = entry;
                } else {
                    indexes.insert(entry.manifest.name.clone(), entries.len());
                    entries.push(entry);
                }
            }
        }
        entries
    };
    let mut indexes = entries
        .iter()
        .enumerate()
        .map(|(index, entry)| (entry.manifest.name.clone(), index))
        .collect::<BTreeMap<_, _>>();
    for requested in &build.extension_paths {
        let requested = if requested.is_absolute() {
            requested.clone()
        } else {
            workspace.join(requested)
        };
        let metadata = std::fs::symlink_metadata(&requested).map_err(|error| {
            MimirError::Configuration(format!(
                "explicit extension '{}' is unavailable: {error}",
                requested.display()
            ))
        })?;
        if metadata.file_type().is_symlink() {
            return Err(MimirError::Configuration(format!(
                "explicit extension cannot be a symlink: {}",
                requested.display()
            )));
        }
        let manifest_path = if metadata.is_dir() {
            requested.join("manifest.json")
        } else {
            requested
        };
        let manifest_metadata = std::fs::symlink_metadata(&manifest_path).map_err(|error| {
            MimirError::Configuration(format!(
                "explicit extension manifest '{}' is unavailable: {error}",
                manifest_path.display()
            ))
        })?;
        if manifest_metadata.file_type().is_symlink()
            || !manifest_metadata.is_file()
            || manifest_metadata.len() > 1024 * 1024
        {
            return Err(MimirError::Configuration(format!(
                "explicit extension manifest must be a regular file at most 1 MiB: {}",
                manifest_path.display()
            )));
        }
        let manifest_path = std::fs::canonicalize(manifest_path)?;
        let manifest: ExtensionManifest = serde_json::from_slice(&std::fs::read(&manifest_path)?)?;
        manifest.validate()?;
        let root_dir = manifest_path
            .parent()
            .ok_or_else(|| {
                MimirError::Configuration(
                    "explicit extension manifest path has no parent directory".into(),
                )
            })?
            .to_path_buf();
        let entry = CatalogEntry {
            enabled: true,
            root_dir,
            source: ManifestSource::Workspace,
            manifest,
        };
        if let Some(index) = indexes.get(&entry.manifest.name).copied() {
            entries[index] = entry;
        } else {
            indexes.insert(entry.manifest.name.clone(), entries.len());
            entries.push(entry);
        }
    }
    entries.sort_by(|left, right| left.manifest.name.cmp(&right.manifest.name));
    Ok(entries)
}

#[allow(
    clippy::too_many_lines,
    reason = "provider, auth refresh, extension, and session assembly are intentionally audited in one place"
)]
async fn build_runtime_for_session(
    build: &RuntimeBuildConfig,
    session: &str,
) -> Result<Arc<AgentRuntime>> {
    let workspace = std::fs::canonicalize(&build.workspace).map_err(|error| {
        MimirError::Configuration(format!("workspace is inaccessible: {error}"))
    })?;
    let state = resolve_state_dir(&build.state_dir)?;
    let session_root = resolve_session_root(build, &state)?;
    let activation = MigratedRuntimeState::load(&state)?;
    let stored_provider_build = activate_single_stored_provider(build, &state).await?;
    let activated_build = activate_migrated_preferences(&stored_provider_build, &activation);
    let (effective_build, restored_thinking_level) =
        restored_runtime_build(&activated_build, &session_root, session).await?;
    let build = &effective_build;
    let extensions = if build.agent_mode.is_plan() {
        Vec::new()
    } else {
        load_runtime_extensions(build, &workspace, &state).await?
    };
    let extension_manager = ExtensionManager::load_shared(
        extensions.clone(),
        &workspace,
        &state,
        RuntimeLimits::default(),
    )
    .await?;
    let discovered_resources = if build.agent_mode.is_plan() {
        DiscoveredResourcePaths::default()
    } else {
        extension_manager
            .discover_resources(ResourceDiscoveryReason::Startup)
            .await?
    };
    let mut resource_build = build.clone();
    resource_build
        .skill_paths
        .extend(discovered_resources.skill_paths);
    resource_build
        .prompt_template_paths
        .extend(discovered_resources.prompt_paths);
    resource_build
        .theme_paths
        .extend(discovered_resources.theme_paths);
    let extension_provider_selected = extension_manager
        .providers()
        .iter()
        .any(|provider| provider.name == build.provider);
    let (
        provider,
        supported_thinking_levels,
        thinking_level_map,
        supports_priority_tier,
        model_max_output_tokens,
        model_context_window_tokens,
    ) = if extension_provider_selected {
        if build.base_url.is_some() {
            return Err(MimirError::Configuration(format!(
                "extension provider '{}' owns its audited base URL and does not accept --base-url",
                build.provider
            )));
        }
        let (provider, supported) = extension_manager
            .activate_provider(&build.provider, &build.model)
            .await?;
        (provider, supported, None, false, 16_384, 128_000)
    } else {
        let model_definition = runtime_model_definition(build)?;
        let supports_priority_tier = matches!(
            model_definition.api.as_str(),
            "openai-responses" | "azure-openai-responses" | "openai-completions"
        );
        let provider = build_provider_for_runtime(build, &state).await?;
        (
            provider,
            model_definition.thinking_levels(),
            model_definition.thinking_level_map,
            supports_priority_tier,
            model_definition.max_tokens,
            model_definition.context_window,
        )
    };
    let thinking_level = resolve_runtime_thinking_level(
        build.thinking,
        restored_thinking_level,
        &supported_thinking_levels,
        &build.provider,
        &build.model,
    )?;
    if build.agent_mode.is_plan() && extension_provider_selected {
        return Err(MimirError::Configuration(
            "plan mode supports native providers only".into(),
        ));
    }
    let process_tools_authorized = build.allow_process && !build.agent_mode.is_plan();
    let plan_context = if build.agent_mode.is_plan() {
        let context = Arc::new(
            PlanContextStore::new(&workspace, &state, session)
                .map_err(|error| MimirError::Tool(error.to_string()))?,
        );
        context
            .prepare()
            .await
            .map_err(|error| MimirError::Tool(error.to_string()))?;
        Some(context)
    } else {
        None
    };
    let policy = ToolPolicy {
        allow_write: !build.agent_mode.is_plan(),
        allow_process: process_tools_authorized,
        allow_shell: !build.agent_mode.is_plan(),
        agent_mode: build.agent_mode,
        allowed_programs: Some(build.allowed_programs.clone()),
        approvals: Some(Arc::new(
            WorkspaceApprovalStore::new(&workspace)
                .map_err(|error| MimirError::Tool(error.to_string()))?,
        )),
        plan_context,
        ..ToolPolicy::default()
    };
    let mut tool_registry = ToolRegistry::with_default_tools(&workspace, policy.clone())
        .map_err(|error| MimirError::Tool(error.to_string()))?;
    if build.no_builtin_tools && !build.agent_mode.is_plan() {
        let _ = tool_registry.retain_named(&BTreeSet::new());
    }
    if process_tools_authorized && !build.no_builtin_tools {
        tool_registry
            .register_ipython_kernel(&workspace, &state, session, policy.clone())
            .map_err(|error| MimirError::Tool(error.to_string()))?;
    }
    if !build.agent_mode.is_plan() {
        tool_registry
            .register_extension_manager(&extension_manager)
            .map_err(|error| MimirError::Tool(error.to_string()))?;
        tool_registry
            .register_extension_tools(extensions, &workspace)
            .map_err(|error| MimirError::Tool(error.to_string()))?;
    }
    if !build.offline && !build.agent_mode.is_plan() {
        let mcp_report = tool_registry
            .register_mcp_servers(&state)
            .await
            .map_err(|error| MimirError::Tool(error.to_string()))?;
        for unavailable in &mcp_report.unavailable {
            eprintln!(
                "warning: MCP server {} is unavailable: {}",
                unavailable.server, unavailable.reason
            );
        }
    }
    if !activation.blocked_extensions().is_empty() {
        eprintln!(
            "warning: {} migrated JavaScript extensions remain disabled; install capability-scoped native Rust extension manifests to activate replacements",
            activation.blocked_extensions().len()
        );
    }
    if !activation.blocked_model_providers().is_empty() {
        eprintln!(
            "warning: {} migrated custom model providers remain disabled because no audited native Rust provider is registered",
            activation.blocked_model_providers().len()
        );
    }
    if let Some(allowed) = &build.tool_allowlist
        && !build.agent_mode.is_plan()
    {
        let _ = tool_registry.retain_named(allowed);
    }
    let file_store = if build.no_session {
        None
    } else {
        Some(Arc::new(
            create_run_session_store(build, &session_root, session).await?,
        ))
    };
    let rlm_limits = RlmRuntimeLimits {
        max_depth: load_tui_rlm_max_depth(&state, session).await?,
        ..RlmRuntimeLimits::default()
    };
    if rlm_limits.max_depth > 0 && file_store.is_some() && !build.agent_mode.is_plan() {
        // Every child starts from the fully assembled core/extension/MCP registry.
        // The child factory adds a session-scoped RLM runtime only while another
        // level remains below the configured recursion bound.
        let child_tools = tool_registry.snapshot_for_child_runtime();
        let catalog: Arc<dyn AuthenticatedModelCatalog> = Arc::new(
            AuthStoreModelCatalog::new(AuthStore::new(&state)?)
                .with_additional_models(activation.models().to_vec()),
        );
        let provider_factory: Arc<dyn RlmProviderFactory> = Arc::new(CliRlmProviderFactory {
            template: build.clone(),
            state: state.clone(),
        });
        let child_policy = RlmChildRuntimePolicy::default();
        let child_tool_factory: Arc<dyn RlmChildToolRegistryFactory> =
            Arc::new(CliRlmChildToolFactory {
                base_tools: Arc::clone(&child_tools),
                providers: Arc::clone(&provider_factory),
                catalog: Arc::clone(&catalog),
                state: state.clone(),
                workspace: workspace.clone(),
                policy: child_policy.clone(),
                limits: rlm_limits.clone(),
                kernel_policy: process_tools_authorized.then_some(policy.clone()),
            });
        let child_executor = Arc::new(
            AgentRuntimeChildExecutor::new(provider_factory, child_tools, child_policy)
                .with_child_tool_factory(child_tool_factory),
        );
        let parent_session_path = file_store.as_ref().and_then(|store| store.path().to_str());
        let default_rlm_model = format!("{}/{}", build.provider, build.model);
        let rlm_runtime = Arc::new(
            RlmRuntime::open(
                &state,
                &workspace,
                session,
                parent_session_path,
                0,
                &default_rlm_model,
                catalog,
                child_executor,
                rlm_limits,
            )
            .await?,
        );
        tool_registry
            .register_rlm_runtime(rlm_runtime)
            .map_err(|error| MimirError::Tool(error.to_string()))?;
    }
    if let Some(allowed) = &build.tool_allowlist
        && !build.agent_mode.is_plan()
    {
        let missing = tool_registry.retain_named(allowed);
        if !missing.is_empty() {
            return Err(MimirError::Configuration(format!(
                "unknown tool selection: {}",
                missing.join(", ")
            )));
        }
    }
    let tools = Arc::new(tool_registry);
    let resources = load_runtime_resources(&resource_build, &state, &workspace).await?;
    let mut skills = if build.no_skills {
        Vec::new()
    } else {
        load_migrated_skills(&state)
            .map_err(|error| MimirError::Configuration(error.to_string()))?
    };
    skills.extend(resources.skills);
    skills.sort_by(|left, right| left.name.cmp(&right.name));
    let mut config = RuntimeConfig::default_for_model(&build.model);
    config.provider.clone_from(&build.provider);
    config.thinking_level = thinking_level;
    config.supported_thinking_levels = supported_thinking_levels;
    config.thinking_level_map = thinking_level_map;
    config.provider_timeout = std::time::Duration::from_secs(build.provider_timeout_seconds);
    config.budget.max_turns = build.max_turns;
    config.budget.max_tokens = build.max_run_tokens;
    config.budget.max_context_tokens = u64::from(model_context_window_tokens);
    let mut system_parts = Vec::new();
    if let Some(prompt) = build
        .system_prompt
        .as_deref()
        .or(resources.system_prompt.as_deref())
        .map(str::trim)
        .filter(|prompt| !prompt.is_empty())
    {
        system_parts.push(prompt.to_owned());
    }
    if !resources.system_context.trim().is_empty() {
        system_parts.push(resources.system_context);
    }
    let append_prompts = if build.append_system_prompt.is_empty() {
        &resources.append_system_prompt
    } else {
        &build.append_system_prompt
    };
    system_parts.extend(
        append_prompts
            .iter()
            .map(|prompt| prompt.trim())
            .filter(|prompt| !prompt.is_empty())
            .map(str::to_owned),
    );
    config.system_prompt = system_parts.join("\n\n");
    if let Some(limits) = build.autonomous_limits {
        config.budget.max_turns = limits.max_turns;
        config.budget.max_tokens = limits.max_tokens;
        config.budget.max_elapsed = limits.timeout;
    }
    let store: Arc<dyn SessionStore> = file_store.map_or_else(
        || Arc::new(InMemorySessionStore::default()) as Arc<dyn SessionStore>,
        |store| store as Arc<dyn SessionStore>,
    );
    if build.verbose {
        eprintln!(
            "runtime: provider={}; model={}; session={}; offline={}; context_files={}; skills={}; tools={}",
            build.provider,
            build.model,
            if build.no_session { "memory" } else { session },
            build.offline,
            resources.context_files.len(),
            skills.len(),
            tools.definitions().len()
        );
    }
    let runtime = Arc::new(AgentRuntime::resume(provider, tools, store, config).await?);
    runtime.set_max_output_tokens(model_max_output_tokens)?;
    let migrated_service_tier = activation
        .preferences
        .as_ref()
        .and_then(|preferences| preferences.default_service_tier.as_deref());
    let service_tier = if supports_priority_tier && load_tui_fast_mode(&state).await? {
        Some("priority")
    } else {
        migrated_service_tier
    };
    if service_tier.is_some() {
        runtime.set_service_tier(service_tier).await?;
    }
    runtime.attach_skill_runtime(SkillRuntime::new(skills));
    runtime
        .attach_extension_manager(extension_manager, session)
        .await;
    let extension_flags = resolve_extension_flags(
        &build.extension_flags,
        &runtime
            .extension_manager()
            .await
            .map_or_else(Vec::new, |manager| manager.flags()),
    )?;
    for (name, value) in extension_flags {
        runtime.set_extension_flag(&name, value).await?;
    }
    runtime
        .set_harness_context(refinement::load_harness_context(&state, session).await?)
        .await;
    Ok(runtime)
}

async fn run_once(runtime: &AgentRuntime, prompt: &str, mode: OutputMode) -> Result<()> {
    let sink = StdoutEventSink {
        enabled: mode == OutputMode::Json,
    };
    let answer = runtime.run(prompt, &sink).await?;
    if mode == OutputMode::Text {
        println!("{answer}");
    }
    Ok(())
}

struct AutonomousGateRunner {
    registry: ToolRegistry,
    commands: Vec<ParsedGateCommand>,
    retry_limit: u32,
}

impl AutonomousGateRunner {
    fn from_cli(cli: &Cli) -> Result<Option<Self>> {
        if cli.autonomous_gate.is_empty() {
            return Ok(None);
        }
        let workspace = std::fs::canonicalize(&cli.workspace).map_err(|error| {
            MimirError::Configuration(format!("workspace is inaccessible: {error}"))
        })?;
        let policy = ToolPolicy {
            allow_process: true,
            allowed_programs: Some(cli.allowed_programs.clone()),
            command_timeout: std::time::Duration::from_millis(
                cli.autonomous_gate_timeout_ms.unwrap_or(300_000),
            ),
            ..ToolPolicy::default()
        };
        let registry = ToolRegistry::with_default_tools(&workspace, policy)
            .map_err(|error| MimirError::Tool(error.to_string()))?;
        let commands = cli
            .autonomous_gate
            .iter()
            .map(|command| parse_gate_command(command))
            .collect::<Result<Vec<_>>>()?;
        Ok(Some(Self {
            registry,
            commands,
            retry_limit: cli.autonomous_gate_retries.unwrap_or(3),
        }))
    }

    async fn run(&self) -> std::result::Result<(), String> {
        let mut failures = Vec::new();
        for command in &self.commands {
            let observation = self
                .registry
                .execute(
                    "run_process",
                    json!({"program": command.program, "args": command.arguments}),
                )
                .await;
            match observation {
                Ok(observation) if observation.status == ObservationStatus::Success => {}
                Ok(observation) => failures.push(format!(
                    "{} {}: {}\n{}",
                    command.program,
                    command.arguments.join(" "),
                    observation.summary,
                    observation.content
                )),
                Err(error) => failures.push(format!(
                    "{} {}: {error}",
                    command.program,
                    command.arguments.join(" ")
                )),
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            let mut report = failures.join("\n\n");
            if report.len() > 64 * 1024 {
                let mut boundary = 64 * 1024;
                while !report.is_char_boundary(boundary) {
                    boundary -= 1;
                }
                report.truncate(boundary);
                report.push_str("\n[quality-gate report truncated]");
            }
            Err(report)
        }
    }
}

async fn run_once_autonomous(
    runtime: &AgentRuntime,
    prompt: &str,
    mode: OutputMode,
    limits: AutonomousLimits,
    cli: &Cli,
) -> Result<()> {
    let gates = AutonomousGateRunner::from_cli(cli)?;
    let mut autonomous = AutonomousState::default();
    autonomous.set_limits(limits);
    autonomous.enable(std::time::Instant::now());
    let Some(generation) = autonomous.begin_run(std::time::Instant::now()) else {
        return run_once(runtime, prompt, mode).await;
    };
    let mut next_prompt = prompt.to_owned();
    let mut gate_failures = 0_u32;
    loop {
        let before = runtime.messages_snapshot().await.len();
        run_once(runtime, &next_prompt, mode).await?;
        for message in runtime.messages_snapshot().await.iter().skip(before) {
            if message.role == Role::Assistant {
                autonomous.record_turn(generation, message.usage);
            }
        }
        if let Some(gates) = &gates {
            match gates.run().await {
                Ok(()) => return Ok(()),
                Err(report) if gate_failures >= gates.retry_limit => {
                    return Err(MimirError::Tool(format!(
                        "autonomous quality gates failed after {} attempt(s):\n{report}",
                        gate_failures.saturating_add(1)
                    )));
                }
                Err(report) => {
                    gate_failures = gate_failures.saturating_add(1);
                    let Some(continuation) =
                        autonomous.next_continuation(generation, std::time::Instant::now())
                    else {
                        return Err(MimirError::Tool(format!(
                            "autonomous quality gates failed and the autonomous budget is exhausted:\n{report}"
                        )));
                    };
                    next_prompt = format!(
                        "{continuation}\n\nThe following explicitly configured quality gates failed. Fix the failures, then continue:\n\n{report}"
                    );
                    continue;
                }
            }
        }
        let Some(continuation) =
            autonomous.next_continuation(generation, std::time::Instant::now())
        else {
            return Ok(());
        };
        next_prompt = continuation;
    }
}

async fn run_repl(runtime: Arc<AgentRuntime>, mode: OutputMode) -> Result<()> {
    let stdin = io::stdin();
    let mut stdout = io::stdout();
    for line in stdin.lock().lines() {
        let line = line?;
        if matches!(line.trim(), "/quit" | "/exit") {
            break;
        }
        if line.trim().is_empty() {
            continue;
        }
        run_once(&runtime, &line, mode).await?;
        stdout.flush()?;
    }
    Ok(())
}

async fn run_rpc(mut context: RpcSessionContext) -> Result<()> {
    let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = lines.next_line().await? {
        let response = match serde_json::from_str::<Value>(&line) {
            Ok(value) if value.get("jsonrpc").is_some() => match serde_json::from_value(value) {
                Ok(request) => run_json_rpc_request(&context.runtime, request).await,
                Err(error) => {
                    rpc_error(&Value::Null, -32_600, &format!("invalid request: {error}"))
                }
            },
            Ok(value) if value.get("type").is_some() => match serde_json::from_value(value) {
                Ok(request) => run_legacy_rpc_request(&mut context, request).await,
                Err(error) => legacy_error(None, "unknown", &format!("invalid request: {error}")),
            },
            Ok(_) => rpc_error(&Value::Null, -32_600, "invalid request"),
            Err(error) => rpc_error(&Value::Null, -32_700, &format!("parse error: {error}")),
        };
        if !response.is_null() {
            emit_rpc_value(&response);
        }
    }
    stop_all_legacy_observations(&mut context).await?;
    if let Some(worker) = context.worker.take() {
        worker
            .await
            .map_err(|error| MimirError::Protocol(format!("RPC worker failed: {error}")))?;
    }
    if let Some(worker) = context.control_worker.take() {
        worker
            .await
            .map_err(|error| MimirError::Protocol(format!("RPC control worker failed: {error}")))?;
    }
    if let Some(worker) = context.bash_worker.take() {
        worker
            .await
            .map_err(|error| MimirError::Protocol(format!("RPC bash worker failed: {error}")))?;
    }
    Ok(())
}

fn emit_rpc_value(value: &Value) {
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    let _ = writeln!(stdout, "{value}");
    let _ = stdout.flush();
}

async fn run_json_rpc_request(runtime: &AgentRuntime, request: RpcRequest) -> Value {
    if request.jsonrpc != "2.0" {
        return rpc_error(&request.id, -32_600, "jsonrpc must be 2.0");
    }
    match request.method.as_str() {
        "prompt" => {
            let prompt = request.params.get("prompt").and_then(Value::as_str);
            match prompt {
                Some(prompt) => match runtime
                    .run(prompt, &StdoutEventSink { enabled: false })
                    .await
                {
                    Ok(answer) => {
                        json!({"jsonrpc":"2.0","id":request.id,"result":{"text":answer}})
                    }
                    Err(error) => rpc_error(&request.id, -32_000, &error.to_string()),
                },
                None => rpc_error(&request.id, -32_602, "params.prompt must be a string"),
            }
        }
        "health" => {
            json!({"jsonrpc":"2.0","id":request.id,"result":{"status":"ready","schema_version":1}})
        }
        _ => rpc_error(&request.id, -32_601, "method not found"),
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "the protocol dispatcher is kept linear so every public legacy command remains auditable"
)]
async fn run_legacy_rpc_request(
    context: &mut RpcSessionContext,
    request: LegacyRpcRequest,
) -> Value {
    if matches!(
        request.command.as_str(),
        "fork" | "clone" | "new_session" | "switch_session"
    ) && {
        let activity = context.activity.lock().await;
        activity.running || activity.bash_running || activity.control_running.is_some()
    } {
        return legacy_error(
            request.id,
            &request.command,
            "session cannot change while the agent or bash is running",
        );
    }
    match request.command.as_str() {
        "prompt" => {
            handle_legacy_prompt(
                context,
                request.id,
                request.message.as_deref(),
                request.images,
                request.streaming_behavior.as_deref(),
            )
            .await
        }
        "steer" => {
            handle_legacy_steer(
                context,
                request.id,
                request.message.as_deref(),
                request.images,
            )
            .await
        }
        "follow_up" => {
            handle_legacy_follow_up(
                context,
                request.id,
                request.message.as_deref(),
                request.images,
            )
            .await
        }
        "set_steering_mode" | "set_follow_up_mode" => {
            handle_legacy_queue_mode(
                context,
                request.id,
                &request.command,
                request.mode.as_deref(),
            )
            .await
        }
        "set_model"
        | "cycle_model"
        | "get_available_models"
        | "set_thinking_level"
        | "cycle_thinking_level" => run_legacy_model_request(context, request).await,
        "compact" | "refine" => start_legacy_compaction_refinement(context, request).await,
        "send_message"
        | "agent_messages_status"
        | "agent_messages_pause"
        | "agent_messages_resume"
        | "agent_messages_clear" => run_legacy_agent_message_request(context, request).await,
        "observe" | "unobserve" => run_legacy_observation_request(context, request).await,
        "set_auto_compaction"
        | "set_auto_retry"
        | "abort_retry"
        | "abort_bash"
        | "export_html"
        | "get_commands" => run_legacy_utility_request(context, request).await,
        "bash" => handle_legacy_bash(context, request.id, request.shell_command.as_deref()).await,
        "list_schedules" | "add_schedule" | "cancel_schedule" | "list_heartbeats"
        | "get_heartbeat" | "set_heartbeat" | "update_heartbeat" | "manage_heartbeat" => {
            run_legacy_schedule_request(context, request).await
        }
        "get_state" => handle_legacy_get_state(context, request.id).await,
        "get_messages" => legacy_success(
            request.id,
            "get_messages",
            Some(json!({"messages": context.runtime.messages_snapshot().await})),
        ),
        "get_last_assistant_text" => legacy_success(
            request.id,
            "get_last_assistant_text",
            Some(json!({"text": context.runtime.last_assistant_text().await})),
        ),
        "set_session_name" => match set_legacy_session_name(context, request.name.as_deref()).await
        {
            Ok(()) => legacy_success(request.id, "set_session_name", None),
            Err(error) => legacy_error(request.id, "set_session_name", &error.to_string()),
        },
        "get_session_stats" => match legacy_session_stats(context).await {
            Ok(data) => legacy_success(request.id, "get_session_stats", Some(data)),
            Err(error) => legacy_error(request.id, "get_session_stats", &error.to_string()),
        },
        "get_fork_messages" => match legacy_fork_messages(context).await {
            Ok(data) => legacy_success(request.id, "get_fork_messages", Some(data)),
            Err(error) => legacy_error(request.id, "get_fork_messages", &error.to_string()),
        },
        "fork" => match request.entry_id.as_deref() {
            Some(entry_id) => match fork_legacy_session(context, entry_id).await {
                Ok(text) => legacy_success(
                    request.id,
                    "fork",
                    Some(json!({"text": text, "cancelled": false})),
                ),
                Err(error) => legacy_error(request.id, "fork", &error.to_string()),
            },
            None => legacy_error(request.id, "fork", "entryId must be a string"),
        },
        "clone" => match clone_legacy_session(context).await {
            Ok(()) => legacy_success(request.id, "clone", Some(json!({"cancelled": false}))),
            Err(error) => legacy_error(request.id, "clone", &error.to_string()),
        },
        "new_session" => match new_legacy_session(context, request.parent_session.as_deref()).await
        {
            Ok(()) => legacy_success(request.id, "new_session", Some(json!({"cancelled": false}))),
            Err(error) => legacy_error(request.id, "new_session", &error.to_string()),
        },
        "switch_session" => match request.session_path.as_deref() {
            Some(path) => match switch_legacy_session(context, path).await {
                Ok(()) => legacy_success(
                    request.id,
                    "switch_session",
                    Some(json!({"cancelled": false})),
                ),
                Err(error) => legacy_error(request.id, "switch_session", &error.to_string()),
            },
            None => legacy_error(request.id, "switch_session", "sessionPath must be a string"),
        },
        "abort" => handle_legacy_abort(context, request.id),
        command => legacy_error(request.id, command, "unsupported command"),
    }
}

async fn start_legacy_compaction_refinement(
    context: &mut RpcSessionContext,
    request: LegacyRpcRequest,
) -> Value {
    {
        let mut activity = context.activity.lock().await;
        if activity.bash_running {
            return legacy_error(
                request.id,
                &request.command,
                "control operation cannot start while bash is running",
            );
        }
        if let Some(running) = &activity.control_running {
            return legacy_error(
                request.id,
                &request.command,
                &format!("{running} is already running"),
            );
        }
        activity.control_running = Some(request.command.clone());
    }
    if let Some(previous) = context.control_worker.take()
        && let Err(error) = previous.await
    {
        context.activity.lock().await.control_running = None;
        return legacy_error(
            request.id,
            &request.command,
            &format!("previous control worker failed: {error}"),
        );
    }
    let runtime = context.runtime.clone();
    let state_dir = context.build.state_dir.clone();
    let session_id = context.session_id.clone();
    let activity = context.activity.clone();
    context.control_worker = Some(tokio::spawn(async move {
        let response =
            execute_legacy_compaction_refinement(runtime, state_dir, session_id, request).await;
        activity.lock().await.control_running = None;
        emit_rpc_value(&response);
    }));
    Value::Null
}

async fn execute_legacy_compaction_refinement(
    runtime: Arc<AgentRuntime>,
    state_dir: PathBuf,
    session_id: String,
    request: LegacyRpcRequest,
) -> Value {
    match request.command.as_str() {
        "compact" => execute_legacy_compact(runtime, request).await,
        "refine" => execute_legacy_refine(runtime, &state_dir, &session_id, request).await,
        command => legacy_error(request.id, command, "unsupported compaction command"),
    }
}

async fn execute_legacy_compact(runtime: Arc<AgentRuntime>, request: LegacyRpcRequest) -> Value {
    emit_rpc_value(&json!({
        "type": "compaction_start",
        "reason": "manual",
        "customInstructions": request.custom_instructions
    }));
    match runtime
        .compact(request.custom_instructions.as_deref())
        .await
    {
        Ok(result) => {
            let data = match serde_json::to_value(&result) {
                Ok(data) => data,
                Err(error) => return legacy_error(request.id, "compact", &error.to_string()),
            };
            emit_rpc_value(&json!({
                "type": "compaction_end",
                "reason": "manual",
                "result": data,
                "aborted": false,
                "willRetry": false,
                "customInstructions": request.custom_instructions
            }));
            legacy_success(request.id, "compact", Some(data))
        }
        Err(error) => emit_legacy_compact_error(request, &error.to_string()),
    }
}

fn emit_legacy_compact_error(request: LegacyRpcRequest, message: &str) -> Value {
    let normalized = message.to_ascii_lowercase();
    let aborted = normalized.contains("abort") || normalized.contains("cancel");
    let severity = if message == "protocol error: Already compacted"
        || message.contains("too short to compact")
    {
        "warning"
    } else {
        "error"
    };
    emit_rpc_value(&json!({
        "type": "compaction_end",
        "reason": "manual",
        "aborted": aborted,
        "willRetry": false,
        "errorMessage": (!aborted).then_some(message),
        "errorSeverity": (!aborted).then_some(severity),
        "customInstructions": request.custom_instructions
    }));
    legacy_error(request.id, "compact", message)
}

async fn execute_legacy_refine(
    runtime: Arc<AgentRuntime>,
    state_dir: &Path,
    session_id: &str,
    request: LegacyRpcRequest,
) -> Value {
    let state = match resolve_state_dir(state_dir) {
        Ok(state) => state,
        Err(error) => return legacy_error(request.id, "refine", &error.to_string()),
    };
    let options = RefineOptions {
        instructions: request.instructions.as_deref(),
        rollback_id: request.rollback_id.as_deref(),
        global: request.global.unwrap_or(false),
    };
    match refinement::refine(runtime.as_ref(), &state, session_id, options).await {
        Ok(result) => {
            finish_legacy_refine(runtime.as_ref(), &state, session_id, request.id, result).await
        }
        Err(error) => {
            emit_rpc_value(&json!({"type": "refine_failed", "error": error.to_string()}));
            legacy_error(request.id, "refine", &error.to_string())
        }
    }
}

async fn finish_legacy_refine(
    runtime: &AgentRuntime,
    state: &Path,
    session_id: &str,
    id: Option<Value>,
    result: refinement::RefinementResult,
) -> Value {
    let data = match serde_json::to_value(&result) {
        Ok(data) => data,
        Err(error) => return legacy_error(id, "refine", &error.to_string()),
    };
    if let Err(error) = runtime
        .record_runtime_event("refinement", &data.to_string())
        .await
    {
        return legacy_error(id, "refine", &error.to_string());
    }
    let harness_context = match refinement::load_harness_context(state, session_id).await {
        Ok(context) => context,
        Err(error) => return legacy_error(id, "refine", &error.to_string()),
    };
    runtime.set_harness_context(harness_context).await;
    emit_rpc_value(&json!({"type": "refine_complete", "result": data}));
    legacy_success(id, "refine", Some(data))
}

fn handle_legacy_abort(context: &RpcSessionContext, id: Option<Value>) -> Value {
    context.runtime.cancel();
    legacy_success(id, "abort", None)
}

async fn handle_legacy_get_state(context: &RpcSessionContext, id: Option<Value>) -> Value {
    match legacy_session_state(context).await {
        Ok(data) => legacy_success(id, "get_state", Some(data)),
        Err(error) => legacy_error(id, "get_state", &error.to_string()),
    }
}

fn model_definition_for_selection(
    provider: &str,
    model: &str,
    base_url: Option<&str>,
) -> Result<ModelDefinition> {
    if provider == "fake" {
        return Ok(ModelDefinition::fake(model));
    }
    let registry = ProviderRegistry::builtin();
    let definition = registry
        .get(provider)
        .ok_or_else(|| MimirError::Configuration(format!("unknown provider: {provider}")))?;
    if !definition.supports_runtime() {
        return Err(MimirError::Configuration(format!(
            "{provider} does not have a native Rust runtime"
        )));
    }
    let model = ModelDefinition::from_runtime(
        provider,
        model,
        base_url.or(definition.base_url),
        definition.runtime_support,
    );
    definition.runtime_for_model(&model).ok_or_else(|| {
        MimirError::Configuration(format!(
            "model {provider}/{} uses unsupported API {}",
            model.id, model.api
        ))
    })?;
    Ok(model)
}

async fn runtime_available_models(
    build: &RuntimeBuildConfig,
    runtime: &AgentRuntime,
) -> Result<Vec<ModelDefinition>> {
    let state = resolve_state_dir(&build.state_dir)?;
    let auth = AuthStore::new(&state)?;
    let (current_provider, current_model, _) = runtime.model_selection().await;
    let registry = ProviderRegistry::builtin();
    let mut configured = BTreeSet::new();
    for definition in registry.iter().filter(|entry| entry.supports_runtime()) {
        if definition.environment_key().is_some()
            || auth.get(definition.id).await?.is_some()
            || ambient_credentials_configured(definition.id, definition)
        {
            configured.insert(definition.id);
        }
    }
    if current_provider != "fake" {
        configured.insert(&current_provider);
    }
    let mut models = model_catalog()
        .iter()
        .filter(|model| {
            configured.contains(model.provider.as_str())
                && registry
                    .get(&model.provider)
                    .and_then(|provider| provider.runtime_for_model(model))
                    .is_some()
        })
        .cloned()
        .collect::<Vec<_>>();
    let migrated = MigratedRuntimeState::load(&state)?;
    let mut merged = models
        .into_iter()
        .filter(|model| {
            (model.provider == current_provider && model.id == current_model)
                || migrated.model_is_enabled(&model.provider, &model.id)
        })
        .map(|model| ((model.provider.clone(), model.id.clone()), model))
        .collect::<BTreeMap<_, _>>();
    for model in migrated.models().iter().filter(|model| {
        configured.contains(model.provider.as_str())
            && migrated.model_is_enabled(&model.provider, &model.id)
    }) {
        merged.insert((model.provider.clone(), model.id.clone()), model.clone());
    }
    models = merged.into_values().collect();
    if !models
        .iter()
        .any(|entry| entry.provider == current_provider && entry.id == current_model)
    {
        models.insert(
            0,
            model_definition_for_selection(
                &current_provider,
                &current_model,
                build.base_url.as_deref(),
            )?,
        );
    }
    if configured.contains("openai-codex") {
        let mut discovery_build = build.clone();
        if discovery_build.provider != "openai-codex" {
            discovery_build.base_url = None;
        }
        discovery_build.provider = "openai-codex".into();
        discovery_build.model = "gpt-5.1".into();
        let discovered = match build_provider_for_runtime(&discovery_build, &state).await {
            Ok(provider) => provider.available_model_ids().await.ok().flatten(),
            Err(_) => None,
        };
        if let Some(discovered) = discovered {
            let discovered = discovered.into_iter().collect::<BTreeSet<_>>();
            models
                .retain(|model| model.provider != "openai-codex" || discovered.contains(&model.id));
        } else {
            models.retain(|model| model.provider != "openai-codex");
        }
    }
    Ok(models)
}

async fn legacy_available_models(context: &RpcSessionContext) -> Result<Vec<ModelDefinition>> {
    runtime_available_models(&context.build, context.runtime.as_ref()).await
}

fn parse_thinking_level(level: Option<&str>) -> std::result::Result<ThinkingLevel, &'static str> {
    match level {
        Some("off") => Ok(ThinkingLevel::Off),
        Some("minimal") => Ok(ThinkingLevel::Minimal),
        Some("low") => Ok(ThinkingLevel::Low),
        Some("medium") => Ok(ThinkingLevel::Medium),
        Some("high") => Ok(ThinkingLevel::High),
        Some("xhigh") => Ok(ThinkingLevel::Xhigh),
        Some("max") => Ok(ThinkingLevel::Max),
        _ => Err("thinking level must be off, minimal, low, medium, high, xhigh, or max"),
    }
}

async fn select_legacy_model(
    context: &mut RpcSessionContext,
    provider: &str,
    model: &str,
) -> Result<(ModelDefinition, ThinkingLevel)> {
    let available = legacy_available_models(context).await?;
    let selected = available
        .into_iter()
        .find(|entry| entry.provider == provider && entry.id == model)
        .ok_or_else(|| MimirError::Configuration(format!("Model not found: {provider}/{model}")))?;
    let state = resolve_state_dir(&context.build.state_dir)?;
    let mut build = context.build.clone();
    if build.provider != provider {
        build.base_url = None;
    }
    build.provider = provider.into();
    build.model = model.into();
    let runtime_provider = build_provider_for_runtime(&build, &state).await?;
    let thinking_level = context
        .runtime
        .select_model(
            runtime_provider,
            provider,
            model,
            selected.thinking_levels(),
            selected.thinking_level_map.clone(),
        )
        .await?;
    context.build = build;
    Ok((selected, thinking_level))
}

#[allow(
    clippy::too_many_lines,
    reason = "the five reference model commands share catalog and selection invariants"
)]
async fn run_legacy_model_request(
    context: &mut RpcSessionContext,
    request: LegacyRpcRequest,
) -> Value {
    match request.command.as_str() {
        "get_available_models" => match legacy_available_models(context).await {
            Ok(models) => legacy_success(
                request.id,
                "get_available_models",
                Some(json!({"models": models})),
            ),
            Err(error) => legacy_error(request.id, "get_available_models", &error.to_string()),
        },
        "set_model" => {
            let Some(provider) = request
                .provider
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            else {
                return legacy_error(request.id, "set_model", "provider must be a string");
            };
            let Some(model) = request
                .model_id
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            else {
                return legacy_error(request.id, "set_model", "modelId must be a string");
            };
            let before = context.runtime.model_selection().await.2;
            match select_legacy_model(context, provider, model).await {
                Ok((selected, thinking_level)) => {
                    if thinking_level != before {
                        emit_rpc_value(&json!({
                            "type": "thinking_level_changed",
                            "level": thinking_level.as_str()
                        }));
                    }
                    legacy_success(request.id, "set_model", Some(json!(selected)))
                }
                Err(error) => legacy_error(request.id, "set_model", &error.to_string()),
            }
        }
        "cycle_model" => {
            let direction = match request.direction.as_deref() {
                None | Some("forward") => 1_isize,
                Some("backward") => -1,
                Some(_) => {
                    return legacy_error(
                        request.id,
                        "cycle_model",
                        "direction must be forward or backward",
                    );
                }
            };
            let available = match legacy_available_models(context).await {
                Ok(models) => models,
                Err(error) => {
                    return legacy_error(request.id, "cycle_model", &error.to_string());
                }
            };
            if available.len() <= 1 {
                return legacy_success(request.id, "cycle_model", Some(Value::Null));
            }
            let (provider, model, before) = context.runtime.model_selection().await;
            let current = available
                .iter()
                .position(|entry| entry.provider == provider && entry.id == model)
                .unwrap_or(0);
            let next = if direction > 0 {
                (current + 1) % available.len()
            } else {
                (current + available.len() - 1) % available.len()
            };
            let target = &available[next];
            match select_legacy_model(context, &target.provider, &target.id).await {
                Ok((selected, thinking_level)) => {
                    if thinking_level != before {
                        emit_rpc_value(&json!({
                            "type": "thinking_level_changed",
                            "level": thinking_level.as_str()
                        }));
                    }
                    legacy_success(
                        request.id,
                        "cycle_model",
                        Some(json!({
                            "model": selected,
                            "thinkingLevel": thinking_level.as_str(),
                            "isScoped": false
                        })),
                    )
                }
                Err(error) => legacy_error(request.id, "cycle_model", &error.to_string()),
            }
        }
        "set_thinking_level" => {
            let requested = match parse_thinking_level(request.level.as_deref()) {
                Ok(level) => level,
                Err(error) => return legacy_error(request.id, "set_thinking_level", error),
            };
            let before = context.runtime.model_selection().await.2;
            match context.runtime.set_thinking_level(requested).await {
                Ok(level) => {
                    if level != before {
                        emit_rpc_value(&json!({
                            "type": "thinking_level_changed",
                            "level": level.as_str()
                        }));
                    }
                    legacy_success(request.id, "set_thinking_level", None)
                }
                Err(error) => legacy_error(request.id, "set_thinking_level", &error.to_string()),
            }
        }
        "cycle_thinking_level" => match context.runtime.cycle_thinking_level().await {
            Ok(Some(level)) => {
                emit_rpc_value(&json!({
                    "type": "thinking_level_changed",
                    "level": level.as_str()
                }));
                legacy_success(
                    request.id,
                    "cycle_thinking_level",
                    Some(json!({"level": level.as_str()})),
                )
            }
            Ok(None) => legacy_success(request.id, "cycle_thinking_level", Some(Value::Null)),
            Err(error) => legacy_error(request.id, "cycle_thinking_level", &error.to_string()),
        },
        command => legacy_error(request.id, command, "unsupported model command"),
    }
}

async fn run_legacy_utility_request(
    context: &RpcSessionContext,
    request: LegacyRpcRequest,
) -> Value {
    match request.command.as_str() {
        "set_auto_compaction" => match request.enabled {
            Some(enabled) => {
                context.runtime.set_auto_compaction(enabled);
                legacy_success(request.id, "set_auto_compaction", None)
            }
            None => legacy_error(
                request.id,
                "set_auto_compaction",
                "enabled must be a boolean",
            ),
        },
        "set_auto_retry" => match request.enabled {
            Some(enabled) => {
                context.runtime.set_auto_retry(enabled);
                legacy_success(request.id, "set_auto_retry", None)
            }
            None => legacy_error(request.id, "set_auto_retry", "enabled must be a boolean"),
        },
        "abort_retry" => {
            context.runtime.abort_retry();
            legacy_success(request.id, "abort_retry", None)
        }
        "abort_bash" => {
            context.bash_runner.abort();
            legacy_success(request.id, "abort_bash", None)
        }
        "export_html" => {
            match export_legacy_session_html(context, request.output_path.as_deref()).await {
                Ok(path) => legacy_success(request.id, "export_html", Some(json!({"path": path}))),
                Err(error) => legacy_error(request.id, "export_html", &error.to_string()),
            }
        }
        "get_commands" => match legacy_resource_commands(context) {
            Ok(commands) => legacy_success(
                request.id,
                "get_commands",
                Some(json!({"commands": commands})),
            ),
            Err(error) => legacy_error(request.id, "get_commands", &error.to_string()),
        },
        command => legacy_error(request.id, command, "unsupported utility command"),
    }
}

async fn run_legacy_agent_message_request(
    context: &RpcSessionContext,
    request: LegacyRpcRequest,
) -> Value {
    let payload = match request.command.as_str() {
        "send_message" => {
            let Some(target_session_id) = request
                .target_active_session_id
                .as_deref()
                .and_then(non_empty_legacy_field)
            else {
                return legacy_error(
                    request.id,
                    "send_message",
                    "targetActiveSessionId must be a non-empty string",
                );
            };
            if let Err(error) = validate_legacy_session_id(target_session_id) {
                return legacy_error(request.id, "send_message", error);
            }
            let Some(message) = request.message.as_deref().and_then(non_empty_legacy_field) else {
                return legacy_error(
                    request.id,
                    "send_message",
                    "message must be a non-empty string",
                );
            };
            ClientRequest::SendMessage {
                from_session_id: context.session_id.clone(),
                target_session_id: target_session_id.into(),
                message: message.into(),
            }
        }
        "agent_messages_status" => ClientRequest::AgentMessagesStatus,
        "agent_messages_pause" => ClientRequest::AgentMessagesPause,
        "agent_messages_resume" => ClientRequest::AgentMessagesResume,
        "agent_messages_clear" => ClientRequest::AgentMessagesClear {
            session_id: context.session_id.clone(),
        },
        command => return legacy_error(request.id, command, "unsupported agent message command"),
    };
    let response = match request_rpc_daemon(context, payload).await {
        Ok(response) => response,
        Err(error) => {
            return legacy_error(request.id, &request.command, &error.to_string());
        }
    };
    let data = match (&request.command[..], response) {
        ("send_message", ServerResponse::AgentMessageSent(receipt)) => json!(receipt),
        (
            "agent_messages_status" | "agent_messages_pause" | "agent_messages_resume",
            ServerResponse::AgentMessagesStatus(status),
        ) => json!(status),
        ("agent_messages_clear", ServerResponse::AgentMessagesCleared(cleared)) => json!(cleared),
        _ => {
            return legacy_error(
                request.id,
                &request.command,
                "daemon returned an invalid agent message response",
            );
        }
    };
    legacy_success(request.id, &request.command, Some(data))
}

async fn request_rpc_daemon(
    context: &RpcSessionContext,
    request: ClientRequest,
) -> Result<ServerResponse> {
    let state = resolve_state_dir(&context.build.state_dir)?;
    ensure_rpc_daemon(context, &state).await?;
    let mut client = DaemonClient::connect(&daemon_config(&state).socket_path)
        .await
        .map_err(daemon_error)?;
    if let ClientRequest::SendMessage {
        target_session_id, ..
    } = &request
    {
        let target = FileSessionStore::create(&state, target_session_id).await?;
        if !tokio::fs::try_exists(target.path()).await? {
            return Err(MimirError::Protocol(format!(
                "unknown target session: {target_session_id}"
            )));
        }
        register_rpc_daemon_session(&mut client, &context.session_id).await?;
        register_rpc_daemon_session(&mut client, target_session_id).await?;
    }
    client.request(request).await.map_err(daemon_error)
}

async fn register_rpc_daemon_session(client: &mut DaemonClient, session_id: &str) -> Result<()> {
    let attached = client
        .request(ClientRequest::attach(session_id, "mimir-rpc-message"))
        .await
        .map_err(daemon_error)?;
    let ServerResponse::SessionAttached(attached) = attached else {
        return Err(MimirError::Protocol(
            "daemon returned an invalid attach response".into(),
        ));
    };
    let detached = client
        .request(ClientRequest::detach(&attached.lease.lease_id.to_string()))
        .await
        .map_err(daemon_error)?;
    if !matches!(detached, ServerResponse::SessionDetached(_)) {
        return Err(MimirError::Protocol(
            "daemon returned an invalid detach response".into(),
        ));
    }
    Ok(())
}

async fn run_legacy_observation_request(
    context: &mut RpcSessionContext,
    request: LegacyRpcRequest,
) -> Value {
    let Some(target_session_id) = request
        .active_session_id
        .as_deref()
        .and_then(non_empty_legacy_field)
        .map(str::to_owned)
    else {
        return legacy_error(
            request.id,
            &request.command,
            "activeSessionId must be a non-empty string",
        );
    };
    if let Err(error) = validate_legacy_session_id(&target_session_id) {
        return legacy_error(request.id, &request.command, error);
    }
    match request.command.as_str() {
        "observe" => start_legacy_observation(context, request.id, target_session_id).await,
        "unobserve" => stop_legacy_observation(context, request.id, &target_session_id).await,
        command => legacy_error(request.id, command, "unsupported observation command"),
    }
}

async fn start_legacy_observation(
    context: &mut RpcSessionContext,
    id: Option<Value>,
    target_session_id: String,
) -> Value {
    if context
        .observations
        .get(&target_session_id)
        .is_some_and(|observation| observation.worker.is_finished())
        && let Some(observation) = context.observations.remove(&target_session_id)
    {
        let _ = observation.worker.await;
    }
    if context.observations.contains_key(&target_session_id) {
        return match observed_session_messages(context, &target_session_id).await {
            Ok(messages) => legacy_success(id, "observe", Some(json!({"messages": messages}))),
            Err(error) => legacy_error(id, "observe", &error.to_string()),
        };
    }

    let state = match resolve_state_dir(&context.build.state_dir) {
        Ok(state) => state,
        Err(error) => return legacy_error(id, "observe", &error.to_string()),
    };
    let observer = SessionObserver::new(&state);
    let observation = match observer.start(&target_session_id).await {
        Ok(observation) => observation,
        Err(error) => return legacy_error(id, "observe", &error.to_string()),
    };
    let messages = observation.messages().to_vec();
    let response = legacy_success(id, "observe", Some(json!({"messages": messages})));
    emit_rpc_value(&response);

    let cancellation = CancellationToken::new();
    let worker_cancellation = cancellation.clone();
    let worker = tokio::spawn(run_legacy_observation_worker(
        observation,
        worker_cancellation,
        target_session_id.clone(),
    ));
    context.observations.insert(
        target_session_id,
        RpcObservationWorker {
            cancellation,
            worker,
        },
    );
    Value::Null
}

async fn run_legacy_observation_worker(
    mut observation: SessionObservation,
    cancellation: CancellationToken,
    target_session_id: String,
) {
    loop {
        tokio::select! {
            () = cancellation.cancelled() => {
                if let Err(error) = observation.stop().await {
                    emit_rpc_value(&json!({
                        "type": "observed_session_closed",
                        "activeSessionId": target_session_id,
                        "error": format!("observation shutdown failed: {error}")
                    }));
                }
                return;
            }
            output = observation.next_event() => match output {
                Some(output) => emit_observed_session_output(output, &target_session_id),
                None => return,
            }
        }
    }
}

fn emit_observed_session_output(output: ObservedSessionOutput, target_session_id: &str) {
    match serde_json::to_value(output) {
        Ok(value) => emit_rpc_value(&value),
        Err(error) => emit_rpc_value(&json!({
            "type": "observed_session_closed",
            "activeSessionId": target_session_id,
            "error": format!("failed to encode observation event: {error}")
        })),
    }
}

async fn stop_legacy_observation(
    context: &mut RpcSessionContext,
    id: Option<Value>,
    target_session_id: &str,
) -> Value {
    if let Some(observation) = context.observations.remove(target_session_id) {
        observation.cancellation.cancel();
        if let Err(error) = observation.worker.await {
            return legacy_error(
                id,
                "unobserve",
                &format!("observation worker failed: {error}"),
            );
        }
    }
    legacy_success(id, "unobserve", None)
}

async fn stop_all_legacy_observations(context: &mut RpcSessionContext) -> Result<()> {
    let observations = std::mem::take(&mut context.observations);
    for (_, observation) in observations {
        observation.cancellation.cancel();
        observation.worker.await.map_err(|error| {
            MimirError::Protocol(format!("RPC observation worker failed: {error}"))
        })?;
    }
    Ok(())
}

async fn observed_session_messages(
    context: &RpcSessionContext,
    session_id: &str,
) -> Result<Vec<Message>> {
    let state = resolve_state_dir(&context.build.state_dir)?;
    let mut snapshot = SessionObserver::new(state).start(session_id).await?;
    let messages = snapshot.messages().to_vec();
    snapshot.stop().await?;
    Ok(messages)
}

fn non_empty_legacy_field(value: &str) -> Option<&str> {
    let value = value.trim();
    (!value.is_empty()).then_some(value)
}

fn validate_legacy_session_id(session_id: &str) -> std::result::Result<(), &'static str> {
    if session_id
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        Ok(())
    } else {
        Err("active session id must contain only letters, digits, '-' or '_'")
    }
}

async fn run_legacy_schedule_request(
    context: &RpcSessionContext,
    request: LegacyRpcRequest,
) -> Value {
    let state = match resolve_state_dir(&context.build.state_dir) {
        Ok(state) => state,
        Err(error) => return legacy_error(request.id, &request.command, &error.to_string()),
    };
    let store = ScheduleStore::new(&state);
    if matches!(
        request.command.as_str(),
        "list_heartbeats"
            | "get_heartbeat"
            | "set_heartbeat"
            | "update_heartbeat"
            | "manage_heartbeat"
    ) {
        return run_legacy_heartbeat_request(context, &state, &store, request).await;
    }
    run_legacy_cron_request(context, &state, &store, request).await
}

async fn run_legacy_cron_request(
    context: &RpcSessionContext,
    state: &std::path::Path,
    store: &ScheduleStore,
    request: LegacyRpcRequest,
) -> Value {
    match request.command.as_str() {
        "list_schedules" => match store.list().await {
            Ok(schedules) => {
                let mut jobs = Vec::new();
                for schedule in schedules.into_iter().filter(|schedule| {
                    schedule.session_id == context.session_id
                        && (request.include_inactive.unwrap_or(false)
                            || schedule.enabled
                            || schedule.paused)
                }) {
                    match legacy_schedule_job(context, state, &schedule) {
                        Ok(job) => jobs.push(job),
                        Err(error) => {
                            return legacy_error(request.id, "list_schedules", &error.to_string());
                        }
                    }
                }
                legacy_success(request.id, "list_schedules", Some(json!({"jobs": jobs})))
            }
            Err(error) => legacy_error(request.id, "list_schedules", &error.to_string()),
        },
        "add_schedule" => {
            add_legacy_schedule(
                context,
                state,
                store,
                request.id,
                request.schedule.as_deref(),
                request.prompt.as_deref(),
            )
            .await
        }
        "cancel_schedule" => {
            let Some(job_id) = request.job_id.as_deref() else {
                return legacy_error(request.id, "cancel_schedule", "jobId must be a UUID");
            };
            let Ok(job_id) = Uuid::parse_str(job_id) else {
                return legacy_error(request.id, "cancel_schedule", "jobId must be a UUID");
            };
            match store.list().await {
                Ok(schedules)
                    if schedules.iter().any(|schedule| {
                        schedule.id == job_id && schedule.session_id == context.session_id
                    }) => {}
                Ok(_) => {
                    return legacy_error(
                        request.id,
                        "cancel_schedule",
                        "schedule does not belong to the active session",
                    );
                }
                Err(error) => {
                    return legacy_error(request.id, "cancel_schedule", &error.to_string());
                }
            }
            match store.cancel(job_id).await {
                Ok(schedule) => match legacy_schedule_job(context, state, &schedule) {
                    Ok(job) => {
                        legacy_success(request.id, "cancel_schedule", Some(json!({"job": job})))
                    }
                    Err(error) => legacy_error(request.id, "cancel_schedule", &error.to_string()),
                },
                Err(error) => legacy_error(request.id, "cancel_schedule", &error.to_string()),
            }
        }
        command => legacy_error(request.id, command, "unsupported schedule command"),
    }
}

async fn run_legacy_heartbeat_request(
    context: &RpcSessionContext,
    state: &std::path::Path,
    store: &ScheduleStore,
    request: LegacyRpcRequest,
) -> Value {
    match request.command.as_str() {
        "list_heartbeats" => match store.list_heartbeats().await {
            Ok(schedules) => {
                let session_name = match legacy_session_name(context).await {
                    Ok(name) => name,
                    Err(error) => {
                        return legacy_error(request.id, "list_heartbeats", &error.to_string());
                    }
                };
                let first_message = context
                    .runtime
                    .messages_snapshot()
                    .await
                    .into_iter()
                    .find(|message| message.role == Role::User)
                    .map(|message| message.text());
                let mut heartbeats = Vec::new();
                for schedule in schedules
                    .into_iter()
                    .filter(|schedule| schedule.session_id == context.session_id)
                {
                    match legacy_schedule_job(context, state, &schedule) {
                        Ok(job) => {
                            let mut heartbeat = json!({"job": job});
                            if let Some(name) = &session_name {
                                heartbeat["sessionName"] = json!(name);
                            }
                            if let Some(message) = &first_message {
                                heartbeat["firstMessage"] = json!(message);
                            }
                            heartbeats.push(heartbeat);
                        }
                        Err(error) => {
                            return legacy_error(request.id, "list_heartbeats", &error.to_string());
                        }
                    }
                }
                legacy_success(
                    request.id,
                    "list_heartbeats",
                    Some(json!({"heartbeats": heartbeats})),
                )
            }
            Err(error) => legacy_error(request.id, "list_heartbeats", &error.to_string()),
        },
        "get_heartbeat" => match store.get_heartbeat(&context.session_id).await {
            Ok(Some(schedule)) => match legacy_schedule_job(context, state, &schedule) {
                Ok(heartbeat) => legacy_success(
                    request.id,
                    "get_heartbeat",
                    Some(json!({"heartbeat": heartbeat})),
                ),
                Err(error) => legacy_error(request.id, "get_heartbeat", &error.to_string()),
            },
            Ok(None) => legacy_success(
                request.id,
                "get_heartbeat",
                Some(json!({"heartbeat": Value::Null})),
            ),
            Err(error) => legacy_error(request.id, "get_heartbeat", &error.to_string()),
        },
        "set_heartbeat" => {
            set_legacy_heartbeat(
                context,
                state,
                store,
                request.id,
                request.schedule.as_deref(),
                request.prompt.as_deref(),
                request.delivery_mode.as_deref(),
            )
            .await
        }
        "update_heartbeat" => {
            update_legacy_heartbeat(context, state, store, request.id, request.action.as_deref())
                .await
        }
        "manage_heartbeat" => {
            manage_legacy_heartbeat(
                context,
                state,
                store,
                request.id,
                request.active_session_id.as_deref(),
                request.job_id.as_deref(),
                request.action.as_deref(),
            )
            .await
        }
        command => legacy_error(request.id, command, "unsupported schedule command"),
    }
}

async fn set_legacy_heartbeat(
    context: &RpcSessionContext,
    state: &std::path::Path,
    store: &ScheduleStore,
    id: Option<Value>,
    schedule_text: Option<&str>,
    prompt: Option<&str>,
    delivery_mode: Option<&str>,
) -> Value {
    let Some(schedule_text) = schedule_text else {
        return legacy_error(id, "set_heartbeat", "schedule must be a string");
    };
    let Some(prompt) = prompt.map(str::trim).filter(|value| !value.is_empty()) else {
        return legacy_error(id, "set_heartbeat", "heartbeat instruction cannot be empty");
    };
    let delivery_mode = match delivery_mode {
        Some("steer") => Some(HeartbeatDeliveryMode::Steer),
        Some("follow_up") => Some(HeartbeatDeliveryMode::FollowUp),
        Some(_) => {
            return legacy_error(
                id,
                "set_heartbeat",
                "heartbeat delivery mode must be \"steer\" or \"follow_up\"",
            );
        }
        None => None,
    };
    let now = chrono::Utc::now();
    if let Err(error) = ScheduleStore::validate_heartbeat_text(schedule_text, now) {
        return legacy_error(id, "set_heartbeat", &error.to_string());
    }
    if let Err(error) = ensure_rpc_daemon(context, state).await {
        return legacy_error(id, "set_heartbeat", &error.to_string());
    }
    let label = prompt.chars().take(80).collect::<String>();
    match store
        .set_heartbeat(
            &label,
            &context.session_id,
            prompt,
            schedule_text,
            delivery_mode,
            now,
        )
        .await
    {
        Ok(schedule) => match legacy_schedule_job(context, state, &schedule) {
            Ok(heartbeat) => {
                legacy_success(id, "set_heartbeat", Some(json!({"heartbeat": heartbeat})))
            }
            Err(error) => legacy_error(id, "set_heartbeat", &error.to_string()),
        },
        Err(error) => legacy_error(id, "set_heartbeat", &error.to_string()),
    }
}

async fn update_legacy_heartbeat(
    context: &RpcSessionContext,
    state: &std::path::Path,
    store: &ScheduleStore,
    id: Option<Value>,
    action: Option<&str>,
) -> Value {
    let now = chrono::Utc::now();
    let updated = match action {
        Some("pause") => store.pause_heartbeat(&context.session_id, now).await,
        Some("resume") => store.resume_heartbeat(&context.session_id, now).await,
        Some("clear") => store.clear_heartbeat(&context.session_id, now).await,
        _ => {
            return legacy_error(
                id,
                "update_heartbeat",
                "action must be pause, resume, or clear",
            );
        }
    };
    match updated {
        Ok(Some(schedule)) => match legacy_schedule_job(context, state, &schedule) {
            Ok(heartbeat) => legacy_success(
                id,
                "update_heartbeat",
                Some(json!({"heartbeat": heartbeat})),
            ),
            Err(error) => legacy_error(id, "update_heartbeat", &error.to_string()),
        },
        Ok(None) => legacy_success(
            id,
            "update_heartbeat",
            Some(json!({"heartbeat": Value::Null})),
        ),
        Err(error) => legacy_error(id, "update_heartbeat", &error.to_string()),
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "the legacy RPC boundary names each independently validated protocol field"
)]
async fn manage_legacy_heartbeat(
    context: &RpcSessionContext,
    state: &std::path::Path,
    store: &ScheduleStore,
    id: Option<Value>,
    active_session_id: Option<&str>,
    job_id: Option<&str>,
    action: Option<&str>,
) -> Value {
    let Some(active_session_id) = active_session_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return legacy_error(id, "manage_heartbeat", "activeSessionId must be a string");
    };
    let Some(job_id) = job_id.and_then(|value| Uuid::parse_str(value).ok()) else {
        return legacy_error(id, "manage_heartbeat", "jobId must be a UUID");
    };
    let action = match action {
        Some("pause") => HeartbeatManagementAction::Pause,
        Some("resume") => HeartbeatManagementAction::Resume,
        Some("stop") => HeartbeatManagementAction::Stop,
        _ => {
            return legacy_error(
                id,
                "manage_heartbeat",
                "action must be pause, resume, or stop",
            );
        }
    };
    match store
        .manage_heartbeat(active_session_id, job_id, action, chrono::Utc::now())
        .await
    {
        Ok(Some(schedule)) => match legacy_schedule_job(context, state, &schedule) {
            Ok(heartbeat) => legacy_success(
                id,
                "manage_heartbeat",
                Some(json!({"heartbeat": heartbeat})),
            ),
            Err(error) => legacy_error(id, "manage_heartbeat", &error.to_string()),
        },
        Ok(None) => legacy_error(
            id,
            "manage_heartbeat",
            &format!("no active heartbeat found: {job_id}"),
        ),
        Err(error) => legacy_error(id, "manage_heartbeat", &error.to_string()),
    }
}

async fn add_legacy_schedule(
    context: &RpcSessionContext,
    state: &std::path::Path,
    store: &ScheduleStore,
    id: Option<Value>,
    schedule_text: Option<&str>,
    prompt: Option<&str>,
) -> Value {
    let Some(schedule_text) = schedule_text
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return legacy_error(id, "add_schedule", "schedule must be a string");
    };
    let Some(prompt) = prompt.map(str::trim).filter(|value| !value.is_empty()) else {
        return legacy_error(id, "add_schedule", "prompt must be a string");
    };
    if let Err(error) = ScheduleStore::validate_text(schedule_text, chrono::Utc::now()) {
        return legacy_error(id, "add_schedule", &error.to_string());
    }
    if let Err(error) = ensure_rpc_daemon(context, state).await {
        return legacy_error(id, "add_schedule", &error.to_string());
    }
    let label = prompt.chars().take(80).collect::<String>();
    match store
        .add_text(
            &label,
            &context.session_id,
            prompt,
            schedule_text,
            chrono::Utc::now(),
        )
        .await
    {
        Ok(schedule) => match legacy_schedule_job(context, state, &schedule) {
            Ok(job) => legacy_success(id, "add_schedule", Some(json!({"job": job}))),
            Err(error) => legacy_error(id, "add_schedule", &error.to_string()),
        },
        Err(error) => legacy_error(id, "add_schedule", &error.to_string()),
    }
}

fn legacy_schedule_job(
    context: &RpcSessionContext,
    state: &std::path::Path,
    schedule: &Schedule,
) -> Result<Value> {
    let workspace = std::fs::canonicalize(&context.build.workspace)?;
    let session_file = state
        .join("sessions")
        .join(format!("{}.jsonl", schedule.session_id));
    let schedule_kind = if schedule.every_seconds.is_some() {
        ScheduleKind::Interval
    } else {
        schedule.schedule_kind
    };
    let expression = if schedule.schedule_expression.is_empty() {
        schedule.every_seconds.map_or_else(
            || format!("at {}", schedule.next_run.to_rfc3339()),
            |seconds| format!("every {seconds}s"),
        )
    } else {
        schedule.schedule_expression.clone()
    };
    let mut schedule_data = json!({
        "kind": match schedule_kind {
            ScheduleKind::Once => "once",
            ScheduleKind::Cron => "cron",
            ScheduleKind::Interval => "interval",
        },
        "expression": expression
    });
    if let Some(seconds) = schedule.every_seconds {
        let interval_ms = seconds
            .checked_mul(1_000)
            .ok_or_else(|| MimirError::Configuration("schedule interval is too large".into()))?;
        schedule_data["intervalMs"] = json!(interval_ms);
    }
    let status = if schedule.enabled {
        "active"
    } else if schedule.paused {
        "paused"
    } else if schedule.cancelled {
        "cancelled"
    } else {
        "completed"
    };
    let created_at = schedule
        .created_at
        .unwrap_or(schedule.next_run)
        .to_rfc3339();
    let updated_at = schedule
        .updated_at
        .or(schedule.last_run)
        .unwrap_or(schedule.next_run)
        .to_rfc3339();
    let mut job = json!({
        "id": schedule.id,
        "status": status,
        "source": match schedule.source {
            ScheduleSource::Cron => "cron",
            ScheduleSource::Heartbeat => "heartbeat",
        },
        "runtimeKind": "top-level",
        "activeSessionId": schedule.session_id,
        "sessionId": schedule.session_id,
        "sessionFile": session_file,
        "cwd": workspace,
        "label": schedule.name,
        "prompt": schedule.prompt,
        "schedule": schedule_data,
        "createdAt": created_at,
        "updatedAt": updated_at,
        "runCount": schedule.run_count
    });
    if schedule.enabled {
        job["nextRunAt"] = json!(schedule.next_run.to_rfc3339());
    }
    if let Some(delivery_mode) = schedule.delivery_mode {
        job["deliveryMode"] = json!(match delivery_mode {
            HeartbeatDeliveryMode::Steer => "steer",
            HeartbeatDeliveryMode::FollowUp => "follow_up",
        });
    }
    if let Some(last_run) = schedule.last_run {
        job["lastRunAt"] = json!(last_run.to_rfc3339());
    }
    Ok(job)
}

async fn handle_legacy_bash(
    context: &mut RpcSessionContext,
    id: Option<Value>,
    command: Option<&str>,
) -> Value {
    if !context.build.allow_process {
        return legacy_error(
            id,
            "bash",
            "bash is disabled; pass --allow-process to enable it",
        );
    }
    let Some(command) = command.map(str::trim).filter(|command| !command.is_empty()) else {
        return legacy_error(id, "bash", "command must be a non-empty string");
    };
    {
        let mut activity = context.activity.lock().await;
        if activity.running {
            return legacy_error(id, "bash", "bash cannot start while the agent is running");
        }
        if activity.bash_running {
            return legacy_error(id, "bash", "a bash command is already running");
        }
        activity.bash_running = true;
    }
    if let Some(previous) = context.bash_worker.take() {
        let _ = previous.await;
    }

    let command = command.to_owned();
    emit_rpc_value(&json!({
        "type": "bash_start",
        "command": command,
        "excludeFromContext": false
    }));
    let runner = context.bash_runner.clone();
    let runtime = context.runtime.clone();
    let activity = context.activity.clone();
    context.bash_worker = Some(tokio::spawn(async move {
        match runner.execute(&command).await {
            Ok(result) => {
                let persistence = runtime.record_bash_execution(&command, &result).await;
                activity.lock().await.bash_running = false;
                emit_rpc_value(&json!({
                    "type": "bash_end",
                    "exitCode": result.exit_code,
                    "cancelled": result.cancelled,
                    "truncated": result.truncated,
                    "fullOutputPath": result.full_output_path
                }));
                match persistence {
                    Ok(()) => match serde_json::to_value(result) {
                        Ok(data) => emit_rpc_value(&legacy_success(id, "bash", Some(data))),
                        Err(error) => emit_rpc_value(&legacy_error(
                            id,
                            "bash",
                            &format!("could not encode bash result: {error}"),
                        )),
                    },
                    Err(error) => {
                        emit_rpc_value(&legacy_error(id, "bash", &error.to_string()));
                    }
                }
            }
            Err(error) => {
                activity.lock().await.bash_running = false;
                emit_rpc_value(&json!({
                    "type": "bash_end",
                    "cancelled": false,
                    "truncated": false,
                    "errorMessage": error.to_string()
                }));
                emit_rpc_value(&legacy_error(id, "bash", &error.to_string()));
            }
        }
    }));
    Value::Null
}

fn rpc_user_input(
    message: Option<&str>,
    images: Vec<RpcImageContent>,
) -> std::result::Result<RpcUserInput, String> {
    if message.is_none_or(|value| value.trim().is_empty()) && images.is_empty() {
        return Err("message must be a non-empty string".into());
    }
    RpcUserInput::new(message, images)
}

fn rpc_message_preview(message: &Message) -> String {
    let text = message.text();
    if text.is_empty() {
        "[image]".into()
    } else {
        text
    }
}

async fn handle_legacy_prompt(
    context: &mut RpcSessionContext,
    id: Option<Value>,
    message: Option<&str>,
    images: Vec<RpcImageContent>,
    streaming_behavior: Option<&str>,
) -> Value {
    let input = match rpc_user_input(message, images) {
        Ok(input) => input.into_message(),
        Err(error) => return legacy_error(id, "prompt", &error),
    };
    let (running, bash_running) = {
        let activity = context.activity.lock().await;
        (activity.running, activity.bash_running)
    };
    if bash_running {
        return legacy_error(id, "prompt", "prompt cannot start while bash is running");
    }
    if running {
        return match streaming_behavior {
            Some("steer") => match context.runtime.steer_message(input).await {
                Ok(()) => emit_control_response(context, id, "prompt").await,
                Err(error) => legacy_error(id, "prompt", &error.to_string()),
            },
            Some("followUp") => queue_legacy_follow_up(context, id, "prompt", input).await,
            Some(_) => legacy_error(id, "prompt", "streamingBehavior must be steer or followUp"),
            None => legacy_error(
                id,
                "prompt",
                "streamingBehavior is required while the agent is running",
            ),
        };
    }
    {
        let mut activity = context.activity.lock().await;
        activity.running = true;
    }
    emit_rpc_value(&legacy_success(id, "prompt", None));
    start_legacy_rpc_worker(context, input).await;
    Value::Null
}

async fn handle_legacy_steer(
    context: &RpcSessionContext,
    id: Option<Value>,
    message: Option<&str>,
    images: Vec<RpcImageContent>,
) -> Value {
    if !context.activity.lock().await.running {
        return legacy_error(id, "steer", "agent is not running");
    }
    match rpc_user_input(message, images) {
        Ok(input) => match context.runtime.steer_message(input.into_message()).await {
            Ok(()) => emit_control_response(context, id, "steer").await,
            Err(error) => legacy_error(id, "steer", &error.to_string()),
        },
        Err(error) => legacy_error(id, "steer", &error),
    }
}

async fn handle_legacy_follow_up(
    context: &RpcSessionContext,
    id: Option<Value>,
    message: Option<&str>,
    images: Vec<RpcImageContent>,
) -> Value {
    let input = match rpc_user_input(message, images) {
        Ok(input) => input.into_message(),
        Err(error) => return legacy_error(id, "follow_up", &error),
    };
    if !context.activity.lock().await.running {
        return legacy_error(id, "follow_up", "agent is not running");
    }
    queue_legacy_follow_up(context, id, "follow_up", input).await
}

async fn queue_legacy_follow_up(
    context: &RpcSessionContext,
    id: Option<Value>,
    command: &str,
    message: Message,
) -> Value {
    let mut activity = context.activity.lock().await;
    if activity.follow_ups.len() >= 64 {
        return legacy_error(id, command, "follow-up queue reached its 64-message limit");
    }
    activity.follow_ups.push_back(message);
    drop(activity);
    emit_control_response(context, id, command).await
}

async fn emit_control_response(
    context: &RpcSessionContext,
    id: Option<Value>,
    command: &str,
) -> Value {
    emit_rpc_value(&legacy_success(id, command, None));
    emit_rpc_value(&legacy_session_action_update(&context.runtime, &context.activity).await);
    Value::Null
}

async fn legacy_session_action_update(
    runtime: &AgentRuntime,
    activity: &tokio::sync::Mutex<RpcActivity>,
) -> Value {
    let steering = runtime.pending_steering_previews().await;
    let follow_ups = activity
        .lock()
        .await
        .follow_ups
        .iter()
        .map(rpc_message_preview)
        .collect::<Vec<_>>();
    json!({
        "type": "session_action_update",
        "actions": {
            "queuedCount": steering.len().saturating_add(follow_ups.len()),
            "steering": steering,
            "followUps": follow_ups
        }
    })
}

async fn start_legacy_rpc_worker(context: &mut RpcSessionContext, prompt: Message) {
    if let Some(previous) = context.worker.take() {
        let _ = previous.await;
    }
    let runtime = context.runtime.clone();
    let activity = context.activity.clone();
    context.worker = Some(tokio::spawn(async move {
        let mut next_prompts = Some(vec![prompt]);
        while let Some(prompts) = next_prompts.take() {
            let before = runtime.messages_snapshot().await.len();
            let result = runtime
                .run_batch_messages(&prompts, &LegacyRpcEventSink::default())
                .await;
            let generated: Vec<_> = runtime
                .messages_snapshot()
                .await
                .into_iter()
                .skip(before)
                .collect();
            let error = result.as_ref().err().map(ToString::to_string);
            {
                let mut state = activity.lock().await;
                if result.is_err() {
                    state.follow_ups.clear();
                    state.running = false;
                } else {
                    let drained = match state.follow_up_mode {
                        QueueMode::All => state.follow_ups.drain(..).collect::<Vec<_>>(),
                        QueueMode::OneAtATime => state.follow_ups.pop_front().into_iter().collect(),
                    };
                    if drained.is_empty() {
                        state.running = false;
                    } else {
                        next_prompts = Some(drained);
                    }
                }
            }
            emit_rpc_value(&legacy_session_action_update(&runtime, &activity).await);
            emit_rpc_value(&json!({
                "type": "agent_end",
                "messages": generated,
                "error": error
            }));
        }
    }));
}

async fn legacy_session_state(context: &RpcSessionContext) -> Result<Value> {
    let message_count = context.runtime.messages_snapshot().await.len();
    let steering = context.runtime.pending_steering_previews().await;
    let steering_mode = context.runtime.steering_mode().await;
    let (is_streaming, follow_ups, follow_up_mode) = {
        let activity = context.activity.lock().await;
        (
            activity.running,
            activity
                .follow_ups
                .iter()
                .map(rpc_message_preview)
                .collect::<Vec<_>>(),
            activity.follow_up_mode,
        )
    };
    let (provider, model, thinking_level) = context.runtime.model_selection().await;
    let session_name = legacy_session_name(context).await?;
    Ok(json!({
        "model": {"id": model, "provider": provider},
        "thinkingLevel": thinking_level.as_str(),
        "isStreaming": is_streaming,
        "isCompacting": false,
        "steeringMode": steering_mode.as_str(),
        "followUpMode": follow_up_mode.as_str(),
        "sessionId": context.session_id,
        "sessionName": session_name,
        "autoCompactionEnabled": context.runtime.auto_compaction_enabled(),
        "messageCount": message_count,
        "sessionActions": {
            "queuedCount": steering.len().saturating_add(follow_ups.len()),
            "steering": steering,
            "followUps": follow_ups
        },
        "goal": null
    }))
}

fn parse_queue_mode(mode: Option<&str>) -> std::result::Result<QueueMode, &'static str> {
    match mode {
        Some("all") => Ok(QueueMode::All),
        Some("one-at-a-time") => Ok(QueueMode::OneAtATime),
        _ => Err("mode must be all or one-at-a-time"),
    }
}

async fn handle_legacy_queue_mode(
    context: &RpcSessionContext,
    id: Option<Value>,
    command: &str,
    mode: Option<&str>,
) -> Value {
    let mode = match parse_queue_mode(mode) {
        Ok(mode) => mode,
        Err(message) => return legacy_error(id, command, message),
    };
    match command {
        "set_steering_mode" => context.runtime.set_steering_mode(mode).await,
        "set_follow_up_mode" => context.activity.lock().await.follow_up_mode = mode,
        _ => return legacy_error(id, command, "unsupported queue mode command"),
    }
    legacy_success(id, command, None)
}

fn runtime_resource_commands(build: &RuntimeBuildConfig) -> Result<Vec<Value>> {
    let workspace = std::fs::canonicalize(&build.workspace)?;
    let resources = ResourceLoader::new(&workspace, &workspace)
        .and_then(|loader| loader.load())
        .map_err(|error| MimirError::Configuration(error.to_string()))?;
    Ok(resources
        .skills
        .into_iter()
        .map(|skill| {
            json!({
                "name": format!("skill:{}", skill.name),
                "description": skill.description,
                "source": "skill",
                "sourceInfo": {
                    "path": skill.path,
                    "source": "project",
                    "scope": "project",
                    "origin": "top-level",
                    "baseDir": workspace
                }
            })
        })
        .collect())
}

fn legacy_resource_commands(context: &RpcSessionContext) -> Result<Vec<Value>> {
    runtime_resource_commands(&context.build)
}

async fn runtime_resource_snapshot(build: &RuntimeBuildConfig) -> Result<Value> {
    let workspace = std::fs::canonicalize(&build.workspace)?;
    let state = resolve_state_dir(&build.state_dir)?;
    let resources = load_runtime_resources(build, &state, &workspace).await?;
    let source_info = |path: &Path| {
        json!({
            "path": path,
            "source": "project",
            "scope": "project",
            "origin": "top-level",
            "baseDir": workspace
        })
    };
    Ok(json!({
        "contextFiles": resources.context_files.into_iter().map(|path| {
            json!({"path": path})
        }).collect::<Vec<_>>(),
        "skills": resources.skills.into_iter().map(|skill| {
            json!({
                "name": skill.name,
                "description": skill.description,
                "filePath": skill.path,
                "sourceInfo": source_info(&skill.path)
            })
        }).collect::<Vec<_>>(),
        "prompts": resources.prompt_templates.into_iter().map(|prompt| {
            json!({
                "name": prompt.name,
                "description": prompt.description,
                "argumentHint": prompt.argument_hint,
                "filePath": prompt.path,
                "sourceInfo": source_info(&prompt.path)
            })
        }).collect::<Vec<_>>(),
        "extensions": resources.package_manifests.into_iter().map(|path| {
            json!({"path": path, "sourceInfo": source_info(&path)})
        }).collect::<Vec<_>>(),
        "themes": resources.themes.into_iter().map(|theme| {
            json!({
                "name": theme.name,
                "sourcePath": theme.path,
                "sourceInfo": source_info(&theme.path)
            })
        }).collect::<Vec<_>>(),
        "diagnostics": {
            "skills": [],
            "prompts": [],
            "extensions": [],
            "themes": []
        }
    }))
}

async fn export_legacy_session_html(
    context: &RpcSessionContext,
    requested_path: Option<&str>,
) -> Result<String> {
    use tokio::io::AsyncWriteExt;

    let workspace = std::fs::canonicalize(&context.build.workspace)?;
    let state = resolve_state_dir(&context.build.state_dir)?;
    let requested = requested_path.map_or_else(
        || {
            state
                .join("exports")
                .join(format!("{}.html", context.session_id))
        },
        |path| {
            let path = PathBuf::from(path);
            if path.is_absolute() {
                path
            } else {
                workspace.join(path)
            }
        },
    );
    let file_name = requested
        .file_name()
        .ok_or_else(|| MimirError::Configuration("export path has no file name".into()))?
        .to_owned();
    let parent = requested
        .parent()
        .ok_or_else(|| MimirError::Configuration("export path has no parent".into()))?;
    tokio::fs::create_dir_all(parent).await?;
    let parent = std::fs::canonicalize(parent)?;
    let target = parent.join(file_name);
    if let Ok(metadata) = tokio::fs::symlink_metadata(&target).await {
        if metadata.file_type().is_symlink() {
            return Err(MimirError::Configuration(
                "export target must not be a symlink".into(),
            ));
        }
        if !metadata.is_file() {
            return Err(MimirError::Configuration(
                "export target must be a regular file".into(),
            ));
        }
    }

    let messages = context.runtime.messages_snapshot().await;
    let html = render_session_messages_html(messages.iter());

    let temporary = parent.join(format!(".{}.tmp", Uuid::new_v4()));
    let mut file = tokio::fs::File::create(&temporary).await?;
    if let Err(error) = async {
        file.write_all(html.as_bytes()).await?;
        file.sync_all().await?;
        drop(file);
        tokio::fs::rename(&temporary, &target).await
    }
    .await
    {
        let _ = tokio::fs::remove_file(&temporary).await;
        return Err(error.into());
    }
    Ok(target.to_string_lossy().into_owned())
}

fn render_session_messages_html<'a>(messages: impl IntoIterator<Item = &'a Message>) -> String {
    let mut html = String::from(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>Mimir Session</title><style>body{font:15px system-ui;max-width:900px;margin:2rem auto;padding:0 1rem;color:#18181b}article{border:1px solid #d4d4d8;border-radius:8px;padding:1rem;margin:1rem 0}h2{font-size:.8rem;text-transform:uppercase;color:#52525b}pre{white-space:pre-wrap;overflow-wrap:anywhere}</style></head><body><h1>Mimir Session</h1>",
    );
    for message in messages {
        let role = match message.role {
            crate::model::Role::System => "system",
            crate::model::Role::User => "user",
            crate::model::Role::Assistant => "assistant",
            crate::model::Role::Tool => "tool",
        };
        html.push_str("<article><h2>");
        push_html_escaped(&mut html, role);
        html.push_str("</h2><pre>");
        push_html_escaped(&mut html, &message.text());
        html.push_str("</pre></article>");
    }
    html.push_str("</body></html>\n");
    html
}

fn push_html_escaped(output: &mut String, input: &str) {
    for character in input.chars() {
        output.push_str(match character {
            '&' => "&amp;",
            '<' => "&lt;",
            '>' => "&gt;",
            '\"' => "&quot;",
            '\'' => "&#39;",
            _ => {
                output.push(character);
                continue;
            }
        });
    }
}

async fn current_session_store(context: &RpcSessionContext) -> Result<FileSessionStore> {
    let state = resolve_state_dir(&context.build.state_dir)?;
    FileSessionStore::create(&state, &context.session_id).await
}

async fn legacy_session_name(context: &RpcSessionContext) -> Result<Option<String>> {
    let loaded = current_session_store(context).await?.load().await?;
    Ok(loaded.records.iter().rev().find_map(|record| {
        if let SessionPayload::RuntimeEvent { name, detail } = &record.payload
            && name == "session_name"
        {
            return Some(detail.clone());
        }
        None
    }))
}

async fn set_legacy_session_name(context: &RpcSessionContext, name: Option<&str>) -> Result<()> {
    let name = name
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| MimirError::Protocol("session name must be a non-empty string".into()))?;
    current_session_store(context)
        .await?
        .append(SessionRecord::new(SessionPayload::RuntimeEvent {
            name: "session_name".into(),
            detail: name.into(),
        }))
        .await
}

async fn legacy_session_stats(context: &RpcSessionContext) -> Result<Value> {
    let messages = context.runtime.messages_snapshot().await;
    let user_messages = messages
        .iter()
        .filter(|message| message.role == crate::model::Role::User)
        .count();
    let assistant: Vec<_> = messages
        .iter()
        .filter(|message| message.role == crate::model::Role::Assistant)
        .collect();
    let assistant_messages = assistant.len();
    let tool_results = messages
        .iter()
        .filter(|message| message.role == crate::model::Role::Tool)
        .count();
    let tool_calls = messages
        .iter()
        .flat_map(|message| &message.content)
        .filter(|content| matches!(content, Content::ToolCall(_)))
        .count();
    let input: u64 = assistant
        .iter()
        .map(|message| message.usage.input_tokens)
        .sum();
    let output: u64 = assistant
        .iter()
        .map(|message| message.usage.output_tokens)
        .sum();
    let cache_read: u64 = assistant
        .iter()
        .map(|message| message.usage.cached_tokens)
        .sum();
    let store = current_session_store(context).await?;
    Ok(json!({
        "sessionFile": store.path(),
        "sessionId": context.session_id,
        "userMessages": user_messages,
        "assistantMessages": assistant_messages,
        "toolCalls": tool_calls,
        "toolResults": tool_results,
        "totalMessages": messages.len(),
        "tokens": {"input": input, "output": output, "cacheRead": cache_read, "cacheWrite": 0, "total": input.saturating_add(output)},
        "cost": 0.0
    }))
}

async fn legacy_fork_messages(context: &RpcSessionContext) -> Result<Value> {
    let loaded = current_session_store(context).await?.load().await?;
    let messages: Vec<_> = loaded
        .records
        .iter()
        .filter_map(|record| {
            if let SessionPayload::Message(message) = &record.payload
                && message.role == crate::model::Role::User
            {
                return Some(json!({"entryId": record.record_id, "text": message.text()}));
            }
            None
        })
        .collect();
    Ok(json!({"messages": messages}))
}

async fn clone_legacy_session(context: &mut RpcSessionContext) -> Result<()> {
    let source = current_session_store(context).await?;
    let records = source.load().await?.records;
    let session_id = format!("session-{}", Uuid::new_v4().simple());
    let state = resolve_state_dir(&context.build.state_dir)?;
    let destination = FileSessionStore::create(&state, &session_id).await?;
    for record in records {
        destination.append(record).await?;
    }
    destination
        .append(SessionRecord::new(SessionPayload::RuntimeEvent {
            name: "session_cloned_from".into(),
            detail: context.session_id.clone(),
        }))
        .await?;
    context.runtime = build_runtime_for_session(&context.build, &session_id).await?;
    context.session_id = session_id;
    Ok(())
}

async fn fork_legacy_session(context: &mut RpcSessionContext, entry_id: &str) -> Result<String> {
    let entry_id = Uuid::parse_str(entry_id)
        .map_err(|_| MimirError::Protocol("entryId must be a UUID".into()))?;
    let source = current_session_store(context).await?;
    let records = source.load().await?.records;
    let index = records
        .iter()
        .position(|record| record.record_id == entry_id)
        .ok_or_else(|| MimirError::Protocol("entryId was not found".into()))?;
    let selected_text = match &records[index].payload {
        SessionPayload::Message(message) if message.role == crate::model::Role::User => {
            message.text()
        }
        _ => {
            return Err(MimirError::Protocol(
                "entryId must identify a user message".into(),
            ));
        }
    };
    let session_id = format!("session-{}", Uuid::new_v4().simple());
    let state = resolve_state_dir(&context.build.state_dir)?;
    let destination = FileSessionStore::create(&state, &session_id).await?;
    for record in records.into_iter().take(index) {
        destination.append(record).await?;
    }
    destination
        .append(SessionRecord::new(SessionPayload::RuntimeEvent {
            name: "session_forked_from".into(),
            detail: format!("{}:{entry_id}", context.session_id),
        }))
        .await?;
    context.runtime = build_runtime_for_session(&context.build, &session_id).await?;
    context.session_id = session_id;
    Ok(selected_text)
}

async fn new_legacy_session(context: &mut RpcSessionContext, parent: Option<&str>) -> Result<()> {
    let session_id = format!("session-{}", Uuid::new_v4().simple());
    context.runtime = build_runtime_for_session(&context.build, &session_id).await?;
    context.session_id = session_id;
    current_session_store(context)
        .await?
        .append(SessionRecord::new(SessionPayload::RuntimeEvent {
            name: "session_created".into(),
            detail: parent.unwrap_or("").into(),
        }))
        .await?;
    Ok(())
}

async fn switch_legacy_session(context: &mut RpcSessionContext, path: &str) -> Result<()> {
    let session_id = std::path::Path::new(path)
        .file_stem()
        .and_then(std::ffi::OsStr::to_str)
        .ok_or_else(|| MimirError::Protocol("sessionPath has no valid session id".into()))?;
    let state = resolve_state_dir(&context.build.state_dir)?;
    let store = FileSessionStore::create(&state, session_id).await?;
    if !tokio::fs::try_exists(store.path()).await? {
        return Err(MimirError::Protocol(format!(
            "session does not exist: {session_id}"
        )));
    }
    context.runtime = build_runtime_for_session(&context.build, session_id).await?;
    context.session_id = session_id.into();
    Ok(())
}

fn legacy_success(id: Option<Value>, command: &str, data: Option<Value>) -> Value {
    let mut response = serde_json::Map::from_iter([
        ("type".into(), json!("response")),
        ("command".into(), json!(command)),
        ("success".into(), json!(true)),
    ]);
    if let Some(id) = id {
        response.insert("id".into(), id);
    }
    if let Some(data) = data {
        response.insert("data".into(), data);
    }
    Value::Object(response)
}

fn legacy_error(id: Option<Value>, command: &str, message: &str) -> Value {
    let mut response = serde_json::Map::from_iter([
        ("type".into(), json!("response")),
        ("command".into(), json!(command)),
        ("success".into(), json!(false)),
        ("error".into(), json!(message)),
    ]);
    if let Some(id) = id {
        response.insert("id".into(), id);
    }
    Value::Object(response)
}

fn rpc_error(id: &Value, code: i32, message: &str) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
}

#[cfg(test)]
mod tui_model_selection_tests {
    use std::path::PathBuf;

    use crate::{
        auth::{AuthStore, OAuthCredential},
        budget::{BudgetKind, BudgetPause, BudgetSnapshot},
        daemon::{DaemonError, PromptHandler, PublicDaemonCommand},
        diagnostics::{DiagnosticOutcome, diagnostics_root, list_runs, load_bundle},
        error::MimirError,
        model::{Message, ThinkingLevel},
        runtime::QueueMode,
        session::{FileSessionStore, SessionPayload, SessionRecord, SessionStore},
        tools::AgentMode,
    };
    use clap::Parser;
    use serde_json::json;

    use super::{
        Cli, Command, ConfigCommand, OutputMode, PackageCommand, RuntimeBuildConfig,
        RuntimePromptHandler, ScheduleCommand, UpdateAction, activate_single_stored_provider,
        build_runtime_for_session, daemon_runtime_error, parse_recovered_goal_create,
        parse_recovered_refine_args, resolve_extension_flags, resolve_runtime_thinking_level,
        resolve_tui_model_selection, run_self_update, runtime_model_definition,
        send_public_command, tui_model_options, validate_run_options,
    };

    #[test]
    fn compatibility_runtime_flags_parse_without_consuming_the_prompt() {
        let cli = Cli::try_parse_from([
            "mimir",
            "--no-builtin-tools",
            "--no-extensions",
            "-e",
            "./explicit-extension",
            "hello",
        ])
        .expect("compatibility flags");
        assert!(cli.no_builtin_tools);
        assert!(cli.no_extensions);
        assert_eq!(cli.extension, [PathBuf::from("./explicit-extension")]);
        assert_eq!(cli.prompt_segments, ["hello"]);
    }

    #[test]
    fn provider_timeout_defaults_to_fifteen_minutes_and_accepts_an_override() {
        let defaults = Cli::try_parse_from(["mimir"]).expect("defaults");
        assert_eq!(defaults.provider_timeout_seconds, 900);

        let overridden = Cli::try_parse_from(["mimir", "--provider-timeout-seconds", "1800"])
            .expect("timeout override");
        assert_eq!(overridden.provider_timeout_seconds, 1_800);
    }

    #[test]
    fn bare_cli_defaults_to_global_anthropic_sonnet() {
        let defaults = Cli::try_parse_from(["mimir"]).expect("defaults");
        let expected_state = std::env::var_os("HOME")
            .filter(|home| !home.is_empty())
            .map(PathBuf::from)
            .filter(|home| home.is_absolute())
            .map_or_else(|| PathBuf::from(".mimir"), |home| home.join(".mimir"));
        assert_eq!(defaults.state_dir, expected_state);

        let build = RuntimeBuildConfig::from_cli(&defaults);
        assert_eq!(build.provider, "anthropic");
        assert_eq!(build.model, "claude-sonnet-5");
    }

    #[test]
    fn bare_login_targets_anthropic() {
        let cli = Cli::try_parse_from(["mimir", "login"]).expect("default login");
        assert!(matches!(
            cli.command,
            Some(Command::Login { ref provider, .. }) if provider == "anthropic"
        ));
    }

    #[test]
    fn agent_modes_parse_without_reusing_the_output_mode_flag() {
        let defaults = Cli::try_parse_from(["mimir"]).expect("defaults");
        assert_eq!(
            RuntimeBuildConfig::from_cli(&defaults).agent_mode,
            AgentMode::Default
        );

        let auto = Cli::try_parse_from(["mimir", "--agent-mode", "auto"]).expect("auto agent mode");
        assert_eq!(
            RuntimeBuildConfig::from_cli(&auto).agent_mode,
            AgentMode::Auto
        );

        let plan = Cli::try_parse_from(["mimir", "--agent-mode", "plan"]).expect("plan agent mode");
        assert_eq!(
            RuntimeBuildConfig::from_cli(&plan).agent_mode,
            AgentMode::Plan
        );

        let plan_autonomous = Cli::try_parse_from([
            "mimir",
            "--agent-mode",
            "plan",
            "--autonomous",
            "--print",
            "inspect",
        ])
        .expect("parse plan autonomous conflict");
        assert!(validate_run_options(&plan_autonomous).is_err());

        let output = Cli::try_parse_from(["mimir", "--mode", "json"])
            .expect("output mode compatibility alias");
        assert_eq!(output.output, OutputMode::Json);
    }

    #[test]
    fn normal_turn_budget_is_practical_configurable_and_positive() {
        let defaults = Cli::try_parse_from(["mimir"]).expect("defaults");
        assert_eq!(defaults.max_turns, 64);
        assert_eq!(defaults.max_run_tokens, 1_000_000);
        assert_eq!(RuntimeBuildConfig::from_cli(&defaults).max_turns, 64);
        assert_eq!(
            RuntimeBuildConfig::from_cli(&defaults).max_run_tokens,
            1_000_000
        );

        let overridden =
            Cli::try_parse_from(["mimir", "--max-turns", "128", "--max-run-tokens", "2000000"])
                .expect("budget override");
        assert_eq!(overridden.max_turns, 128);
        assert_eq!(overridden.max_run_tokens, 2_000_000);
        assert_eq!(RuntimeBuildConfig::from_cli(&overridden).max_turns, 128);
        assert_eq!(
            RuntimeBuildConfig::from_cli(&overridden).max_run_tokens,
            2_000_000
        );
        assert!(Cli::try_parse_from(["mimir", "--max-turns", "0"]).is_err());
        assert!(Cli::try_parse_from(["mimir", "--max-run-tokens", "0"]).is_err());
    }

    #[test]
    fn daemon_preserves_budget_pause_as_a_non_protocol_error() {
        let error = daemon_runtime_error(MimirError::BudgetPaused(BudgetPause {
            kind: BudgetKind::Turns,
            limit: 64,
            usage: BudgetSnapshot {
                turns: 64,
                ..BudgetSnapshot::default()
            },
        }));
        assert!(matches!(error, DaemonError::BudgetPaused(_)));
        assert_eq!(
            error.to_string(),
            "budget paused: turn budget exhausted at 64 (turns=64, tool_calls=0, budget_tokens=0, input_tokens=0, cached_tokens=0, fresh_input_tokens=0, output_tokens=0, current_context_tokens=0, elapsed_ms=0)"
        );
        assert!(!error.to_string().contains("protocol"));
    }

    #[test]
    fn top_level_compatibility_commands_and_local_package_scope_parse() {
        let cli = Cli::try_parse_from(["mimir", "providers"]).expect("providers");
        assert!(matches!(cli.command, Some(Command::Providers)));

        let cli = Cli::try_parse_from(["mimir", "list", "--all"]).expect("list");
        assert!(matches!(cli.command, Some(Command::List { all: true })));

        let cli = Cli::try_parse_from(["mimir", "package", "install", "./extension", "--local"])
            .expect("local install");
        assert!(matches!(
            cli.command,
            Some(Command::Package {
                action: PackageCommand::Install { local: true, .. }
            })
        ));

        let cli = Cli::try_parse_from(["mimir", "config", "show"]).expect("config show");
        assert!(matches!(
            cli.command,
            Some(Command::Config {
                action: Some(ConfigCommand::Show)
            })
        ));

        let cli =
            Cli::try_parse_from(["mimir", "update", "install", "--force"]).expect("update install");
        assert!(matches!(
            cli.command,
            Some(Command::Update {
                action: UpdateAction::Install,
                force: true
            })
        ));
    }

    #[test]
    fn send_compatibility_forms_parse() {
        let cli = Cli::try_parse_from([
            "mimir",
            "send",
            "worker",
            "interrupt now",
            "--steer",
            "--json",
        ])
        .expect("steering send");
        assert!(matches!(
            cli.command,
            Some(Command::Send {
                steer: true,
                follow_up: false,
                json: true,
                ..
            })
        ));
        assert!(
            Cli::try_parse_from([
                "mimir",
                "send",
                "--steer",
                "--follow-up",
                "worker",
                "invalid",
            ])
            .is_err()
        );
    }

    #[test]
    fn reference_and_native_schedule_forms_parse_without_conflict() {
        let cli = Cli::try_parse_from([
            "mimir",
            "schedule",
            "add",
            "worker",
            "0 9 * * 1-5",
            "--",
            "Check",
            "open work",
        ])
        .expect("reference schedule add");
        assert!(matches!(
            cli.command,
            Some(Command::Schedule {
                action: ScheduleCommand::Add { ref message, .. }
            }) if message == &["Check", "open work"]
        ));

        let cli = Cli::try_parse_from([
            "mimir",
            "schedule",
            "add",
            "native-job",
            "native prompt",
            "--every-seconds",
            "60",
        ])
        .expect("native schedule add");
        assert!(matches!(
            cli.command,
            Some(Command::Schedule {
                action: ScheduleCommand::Add {
                    ref message,
                    every_seconds: Some(60),
                    ..
                }
            }) if message.is_empty()
        ));

        let cli = Cli::try_parse_from(["mimir", "schedule", "list", "--all", "worker", "--json"])
            .expect("reference schedule list");
        assert!(matches!(
            cli.command,
            Some(Command::Schedule {
                action: ScheduleCommand::List {
                    all: true,
                    agent: Some(ref agent),
                    json: true,
                }
            }) if agent == "worker"
        ));
    }

    #[test]
    fn daemon_mode_compatibility_alias_parses() {
        let cli = Cli::try_parse_from(["mimir", "--mode", "daemon"]).expect("daemon mode alias");
        assert_eq!(cli.output, OutputMode::Daemon);
    }

    #[test]
    fn send_delivery_flags_route_to_real_public_commands() {
        assert_eq!(
            send_public_command("worker", "now", None, true, false).expect("steer"),
            json!({"type": "steer", "activeSessionId": "worker", "message": "now"})
        );
        assert_eq!(
            send_public_command("worker", "later", None, false, true).expect("follow-up"),
            json!({"type": "follow_up", "activeSessionId": "worker", "message": "later"})
        );
        assert!(send_public_command("worker", "invalid", Some("sender"), true, false).is_err());
    }

    #[test]
    fn self_update_install_fails_closed_without_a_signed_transport() {
        let error = run_self_update(UpdateAction::Install, false)
            .expect_err("unsigned updater must remain unavailable");
        assert!(error.to_string().contains("no signed release transport"));
    }

    #[test]
    fn recovered_goal_commands_parse_bounded_budget_forms() {
        assert_eq!(
            parse_recovered_goal_create("ship the migration").expect("plain goal"),
            (None, "ship the migration")
        );
        assert_eq!(
            parse_recovered_goal_create("--budget 4096 ship the migration")
                .expect("separate budget"),
            (Some(4096), "ship the migration")
        );
        assert_eq!(
            parse_recovered_goal_create("--token-budget=2048 verify parity")
                .expect("inline budget"),
            (Some(2048), "verify parity")
        );
        assert!(parse_recovered_goal_create("--budget 0 invalid").is_err());
        assert!(parse_recovered_goal_create("--budget 1024").is_err());
    }

    #[test]
    fn recovered_refine_commands_parse_native_options() {
        assert_eq!(
            parse_recovered_refine_args("tighten the prompt").expect("instructions"),
            (Some("tighten the prompt".into()), None, false)
        );
        assert_eq!(
            parse_recovered_refine_args("--global rollback ref-42").expect("global rollback"),
            (None, Some("ref-42".into()), true)
        );
        assert!(parse_recovered_refine_args("rollback").is_err());
        assert!(parse_recovered_refine_args("--globalized invalid").is_err());
    }

    #[test]
    fn provider_qualified_tui_models_switch_provider_and_preserve_model() {
        assert_eq!(
            resolve_tui_model_selection("anthropic/claude-sonnet-4-6", "openai")
                .expect("selection"),
            ("anthropic".into(), "claude-sonnet-4-6".into())
        );
        let options = tui_model_options("openai", "gpt-5-mini");
        assert!(options.iter().any(|value| value.starts_with("anthropic/")));
        assert!(options.iter().any(|value| value.starts_with("google/")));
    }

    #[test]
    fn fake_tui_model_can_rebuild_for_runtime_mode_changes() {
        assert_eq!(
            resolve_tui_model_selection("fake/gpt-5-mini", "fake").expect("fake selection"),
            ("fake".into(), "gpt-5-mini".into())
        );
        assert!(resolve_tui_model_selection("fake/gpt-5-mini", "openai").is_err());
    }

    #[tokio::test]
    async fn sole_stored_anthropic_oauth_login_becomes_the_implicit_provider() {
        let state = tempfile::TempDir::new().expect("state");
        AuthStore::new(state.path())
            .expect("auth store")
            .set_oauth(
                "anthropic",
                OAuthCredential {
                    access: "test-access".into(),
                    refresh: "test-refresh".into(),
                    expires_at_ms: u64::MAX,
                    account_id: None,
                    enterprise_url: None,
                },
            )
            .await
            .expect("stored OAuth");
        let mut config = build("openai", "gpt-5-mini");
        config.provider_explicit = false;
        config.model_explicit = false;

        let selected = activate_single_stored_provider(&config, state.path())
            .await
            .expect("implicit provider");

        assert_eq!(selected.provider, "anthropic");
        assert_eq!(selected.model, "claude-sonnet-5");
    }

    #[test]
    fn extension_flag_assignments_are_typed_bounded_and_unique() {
        let descriptors = vec![
            crate::extensions::FlagDescriptor {
                name: "trace".into(),
                description: None,
                kind: crate::extensions::FlagKind::Boolean,
                default: Some(json!(false)),
            },
            crate::extensions::FlagDescriptor {
                name: "profile".into(),
                description: None,
                kind: crate::extensions::FlagKind::String,
                default: None,
            },
        ];
        assert_eq!(
            resolve_extension_flags(&["trace=true".into(), "profile=ci".into()], &descriptors)
                .expect("typed flags"),
            [
                ("trace".into(), json!(true)),
                ("profile".into(), json!("ci"))
            ]
        );
        assert!(resolve_extension_flags(&["trace=yes".into()], &descriptors).is_err());
        assert!(resolve_extension_flags(&["missing=x".into()], &descriptors).is_err());
        assert!(
            resolve_extension_flags(&["trace=true".into(), "trace=false".into()], &descriptors)
                .is_err()
        );
    }

    #[test]
    fn implicit_thinking_clamps_but_explicit_unsupported_level_fails() {
        let supported = [ThinkingLevel::Minimal, ThinkingLevel::Low];
        assert_eq!(
            resolve_runtime_thinking_level(
                None,
                ThinkingLevel::Off,
                &supported,
                "openai",
                "gpt-5-mini"
            )
            .expect("implicit default clamps"),
            ThinkingLevel::Minimal
        );
        assert!(
            resolve_runtime_thinking_level(
                Some(ThinkingLevel::Off),
                ThinkingLevel::Off,
                &supported,
                "openai",
                "gpt-5-mini"
            )
            .is_err()
        );
    }

    fn build(provider: &str, model: &str) -> RuntimeBuildConfig {
        RuntimeBuildConfig {
            provider: provider.into(),
            model: model.into(),
            base_url: None,
            workspace: PathBuf::from("."),
            state_dir: PathBuf::from(".mimir"),
            session_dir: None,
            no_session: false,
            allow_process: false,
            agent_mode: AgentMode::Default,
            allowed_programs: Vec::new(),
            tool_allowlist: None,
            no_builtin_tools: false,
            extension_paths: Vec::new(),
            no_extensions: false,
            no_context_files: false,
            no_skills: false,
            no_prompt_templates: false,
            no_themes: false,
            skill_paths: Vec::new(),
            prompt_template_paths: Vec::new(),
            theme_paths: Vec::new(),
            thinking: None,
            api_key: None,
            system_prompt: None,
            append_system_prompt: Vec::new(),
            extension_flags: Vec::new(),
            offline: false,
            verbose: false,
            provider_timeout_seconds: 900,
            max_turns: 64,
            max_run_tokens: 1_000_000,
            autonomous_limits: None,
            fake_responses: Vec::new(),
            fake_delay_ms: 0,
            fake_retryable_failures: 0,
            provider_explicit: true,
            model_explicit: true,
            default_thinking_level: ThinkingLevel::Off,
        }
    }

    #[tokio::test]
    async fn daemon_queue_features_expand_replay_and_dedupe_without_network() {
        let workspace = tempfile::TempDir::new().expect("workspace");
        let state = tempfile::TempDir::new().expect("state");
        let template = workspace.path().join("hello.md");
        std::fs::write(
            &template,
            "---\ndescription: greeting\nargument-hint: NAME\n---\nHello $1",
        )
        .expect("template");
        let mut config = build("fake", "fake-model");
        config.workspace = workspace.path().into();
        config.state_dir = state.path().into();
        config.fake_responses = vec!["unused".into()];
        config.prompt_template_paths = vec![template];
        let handler = RuntimePromptHandler::new(config, state.path().into());
        let runtime = handler.runtime("queue-features").await.expect("runtime");

        let expanded = PublicDaemonCommand::new(
            "steer",
            [
                ("expandPromptTemplates".into(), json!(true)),
                ("agentMessageId".into(), json!("message-1")),
            ],
        )
        .expect("expanded command");
        assert!(
            handler
                .admit_queued_message("queue-features", &expanded, false)
                .await
        );
        assert!(
            !handler
                .admit_queued_message("queue-features", &expanded, false)
                .await
        );
        let message = handler
            .prepare_queued_message(runtime.as_ref(), Message::user("/hello Ada"), &expanded)
            .await
            .expect("expanded message");
        assert_eq!(message.text(), "Hello Ada");

        let replay = PublicDaemonCommand::new(
            "follow_up",
            [
                ("expandPromptTemplates".into(), json!(false)),
                ("queueKey".into(), json!("coalesced")),
                (
                    "customMessage".into(),
                    json!({"role":"custom","customType":"agent","content":"primary metadata","display":true,"timestamp":1}),
                ),
                (
                    "prefixMessages".into(),
                    json!([{"role":"custom","customType":"context","content":"prefix context","display":false,"timestamp":1}]),
                ),
            ],
        )
        .expect("replay command");
        assert!(
            handler
                .admit_queued_message("queue-features", &replay, true)
                .await
        );
        assert!(
            !handler
                .admit_queued_message("queue-features", &replay, true)
                .await
        );
        let replayed = handler
            .prepare_queued_message(runtime.as_ref(), Message::user("continue"), &replay)
            .await
            .expect("replayed message");
        assert!(replayed.text().contains("prefix context"));
        assert!(replayed.text().ends_with("continue"));
    }

    #[tokio::test]
    #[allow(
        clippy::too_many_lines,
        reason = "the live daemon snapshot contract is asserted as one coherent fixture"
    )]
    async fn daemon_state_and_headless_status_are_live_snapshots() {
        let workspace = tempfile::TempDir::new().expect("workspace");
        let state = tempfile::TempDir::new().expect("state");
        let mut config = build("fake", "fake-model");
        config.workspace = workspace.path().into();
        config.state_dir = state.path().into();
        config.fake_responses = vec!["unused".into()];
        let handler = RuntimePromptHandler::new(config, state.path().into());
        let runtime = handler.runtime("live-state").await.expect("runtime");
        runtime.set_steering_mode(QueueMode::All).await;
        runtime
            .steer_message(Message::user("queued steering"))
            .await
            .expect("steering");
        handler
            .follow_up_modes
            .lock()
            .await
            .insert("live-state".into(), QueueMode::All);
        handler
            .follow_ups
            .lock()
            .await
            .entry("live-state".into())
            .or_default()
            .push_back(super::QueuedFollowUp {
                message: Message::user("queued follow-up"),
                queue_key: Some("live".into()),
            });
        crate::orchestration::GoalStore::new(state.path())
            .create("finish parity", Some(1_000))
            .await
            .expect("goal");
        handler
            .execute_recovered_autonomous("live-state", runtime.as_ref(), "on")
            .await
            .expect("autonomous on");
        let tier = PublicDaemonCommand::new(
            "set_service_tier",
            [("serviceTier".into(), json!("default"))],
        )
        .expect("tier command");
        handler
            .handle_session_control("live-state", &tier)
            .await
            .expect("tier")
            .expect("handled");
        FileSessionStore::create(state.path(), "live-state")
            .await
            .expect("store")
            .append(SessionRecord::new(SessionPayload::Compaction {
                summary: "checkpoint".into(),
                retained_message_count: 0,
                reason: Some("test".into()),
                first_kept_entry_id: None,
                tokens_before: 0,
                custom_instructions: None,
                details: None,
            }))
            .await
            .expect("compaction");

        let snapshot = handler
            .session_runtime_state("live-state")
            .await
            .expect("state")
            .expect("snapshot");
        assert_eq!(snapshot["model"]["provider"], "fake");
        assert_eq!(snapshot["steeringMode"], "all");
        assert_eq!(snapshot["followUpMode"], "all");
        assert_eq!(snapshot["goal"]["objective"], "finish parity");
        assert_eq!(snapshot["serviceTier"], "default");
        assert_eq!(snapshot["compactionCount"], 1);
        assert_eq!(snapshot["sessionActions"]["queuedCount"], 2);
        assert_eq!(snapshot["autonomous"]["enabled"], true);
        assert!(
            snapshot["activeToolNames"]
                .as_array()
                .is_some_and(|v| !v.is_empty())
        );
        assert_eq!(snapshot["transport"], "sse");

        for requested in ["sse", "auto"] {
            let command =
                PublicDaemonCommand::new("set_transport", [("transport".into(), json!(requested))])
                    .expect("transport command");
            let result = handler
                .handle_session_control("live-state", &command)
                .await
                .expect("transport")
                .expect("handled");
            assert_eq!(result["transport"], "sse");
            assert_eq!(result["changed"], false);
        }
        let unsupported =
            PublicDaemonCommand::new("set_transport", [("transport".into(), json!("websocket"))])
                .expect("unsupported transport");
        assert!(
            handler
                .handle_session_control("live-state", &unsupported)
                .await
                .is_err()
        );

        let wait = PublicDaemonCommand::new("wait_for_headless_completion", []).expect("wait");
        let status = handler
            .handle_session_control("live-state", &wait)
            .await
            .expect("wait status")
            .expect("handled");
        assert_eq!(status["enabled"], true);
        assert!(
            status["status"]
                .as_str()
                .is_some_and(|v| v.contains("Autonomous on"))
        );
    }

    #[tokio::test]
    async fn daemon_headless_completion_runs_real_bounded_continuations() {
        let workspace = tempfile::TempDir::new().expect("workspace");
        let state = tempfile::TempDir::new().expect("state");
        let mut config = build("fake", "fake-model");
        config.workspace = workspace.path().into();
        config.state_dir = state.path().into();
        config.fake_responses = vec!["initial".into(), "one".into(), "two".into(), "final".into()];
        let handler = RuntimePromptHandler::new(config, state.path().into());
        let runtime = handler.runtime("headless").await.expect("runtime");
        handler
            .execute_recovered_autonomous("headless", runtime.as_ref(), "on")
            .await
            .expect("autonomous on");

        let answer = handler
            .handle_prompt(crate::daemon::PromptRequest {
                lease_id: uuid::Uuid::new_v4(),
                session_id: "headless".into(),
                prompt: "start".into(),
            })
            .await
            .expect("headless prompt");
        assert_eq!(answer, "final");
        let wait = PublicDaemonCommand::new("wait_for_headless_completion", []).expect("wait");
        let status = handler
            .handle_session_control("headless", &wait)
            .await
            .expect("wait")
            .expect("handled");
        assert_eq!(status["continuationsUsed"], 3);
        assert_eq!(status["turnsUsed"], 4);
        let runs = list_runs(&diagnostics_root(state.path())).expect("daemon diagnostic runs");
        assert_eq!(runs.len(), 4);
        for run in runs {
            let bundle = load_bundle(&diagnostics_root(state.path()), &run.run_id.to_string())
                .expect("daemon diagnostic bundle");
            let summary = bundle.summary.expect("daemon diagnostic summary");
            assert_eq!(summary.outcome, DiagnosticOutcome::Completed);
            assert_eq!(summary.provider_requests, 1);
        }
    }

    #[test]
    fn catalog_routed_models_resolve_per_model_and_unknown_models_fail_closed() {
        let selected = runtime_model_definition(&build("opencode", "big-pickle"))
            .expect("cataloged OpenCode model");
        assert_eq!(selected.api, "openai-completions");

        let error = runtime_model_definition(&build("opencode", "not-cataloged"))
            .expect_err("uncataloged mixed-protocol model must fail closed");
        assert!(error.to_string().contains("unsupported API"));
    }

    #[tokio::test]
    async fn registered_extension_provider_and_flags_are_selected_during_cli_assembly() {
        let workspace = tempfile::TempDir::new().expect("workspace");
        let state = tempfile::TempDir::new().expect("state");
        let extension_root = workspace.path().join(".mimir/extensions/bridge");
        std::fs::create_dir_all(&extension_root).expect("extension root");
        let module = extension_root.join("bridge.ts");
        std::fs::write(
            &module,
            r#"
export default function activate(pi) {
  pi.registerFlag("trace", { type: "boolean", default: false });
  pi.registerProvider("bridge", {
    api: "openai-responses",
    baseUrl: "https://example.invalid/v1",
    models: [{ id: "bridge-model" }],
  });
}
"#,
        )
        .expect("module");
        std::fs::write(
            extension_root.join("manifest.json"),
            serde_json::to_vec_pretty(&json!({
                "schema_version": 1,
                "name": "bridge",
                "version": "1.0.0",
                "entrypoint": { "module": module },
                "capabilities": ["provider", "commands"]
            }))
            .expect("manifest json"),
        )
        .expect("manifest");
        crate::auth::AuthStore::new(state.path())
            .expect("auth store")
            .set_api_key("bridge", "test-only-key")
            .await
            .expect("credential");

        let mut config = build("bridge", "bridge-model");
        config.workspace = workspace.path().to_owned();
        config.state_dir = state.path().to_owned();
        config.no_session = true;
        config.offline = true;
        config.no_extensions = true;
        config.extension_paths = vec![extension_root];
        config.extension_flags = vec!["trace=true".into()];
        let runtime = build_runtime_for_session(&config, "custom-provider")
            .await
            .expect("custom extension provider runtime");
        let selection = runtime.model_selection().await;
        assert_eq!(selection.0, "bridge");
        assert_eq!(selection.1, "bridge-model");
        assert_eq!(
            runtime.extension_host_snapshot().await.flags[0].value,
            json!(true)
        );
    }

    #[tokio::test]
    async fn system_prompt_replacement_append_and_context_controls_are_wired() {
        let workspace = tempfile::TempDir::new().expect("workspace");
        let state = tempfile::TempDir::new().expect("state");
        std::fs::write(workspace.path().join("AGENTS.md"), "workspace context").expect("context");
        let mut config = build("fake", "test");
        config.workspace = workspace.path().to_owned();
        config.state_dir = state.path().to_owned();
        config.fake_responses = vec!["unused".into()];
        config.system_prompt = Some("replacement prompt".into());
        config.append_system_prompt = vec!["appended prompt".into()];
        let runtime = build_runtime_for_session(&config, "with-context")
            .await
            .expect("runtime");
        let prompt = runtime.system_prompt_snapshot().await;
        assert!(prompt.contains("replacement prompt"));
        assert!(prompt.contains("workspace context"));
        assert!(prompt.contains("appended prompt"));

        config.no_context_files = true;
        let runtime = build_runtime_for_session(&config, "without-context")
            .await
            .expect("runtime without context");
        let prompt = runtime.system_prompt_snapshot().await;
        assert!(prompt.contains("replacement prompt"));
        assert!(!prompt.contains("workspace context"));
        assert!(prompt.contains("appended prompt"));
    }

    #[tokio::test]
    async fn no_builtin_tools_removes_the_core_registry_without_disabling_runtime() {
        let workspace = tempfile::TempDir::new().expect("workspace");
        let state = tempfile::TempDir::new().expect("state");
        let mut config = build("fake", "test");
        config.workspace = workspace.path().to_owned();
        config.state_dir = state.path().to_owned();
        config.fake_responses = vec!["unused".into()];
        config.no_session = true;
        config.no_builtin_tools = true;
        config.no_extensions = true;
        config.offline = true;
        let runtime = build_runtime_for_session(&config, "no-builtins")
            .await
            .expect("runtime without builtin tools");
        assert!(runtime.active_tool_names().await.is_empty());
    }
}

#[cfg(test)]
mod rlm_recursive_tool_tests {
    use std::{path::PathBuf, sync::Arc, time::Duration};

    use async_trait::async_trait;
    use tempfile::TempDir;

    use super::CliRlmChildToolFactory;
    use crate::{
        error::Result,
        extensions::{
            AuthenticatedModelCatalog, RlmChildRuntimePolicy, RlmChildToolRegistryFactory,
            RlmExecutionRequest, RlmModel, RlmProviderFactory, RlmRuntimeLimits,
        },
        model::{Content, Message, ModelResponse, StopReason},
        provider::{FakeProvider, Provider},
        tools::{ToolPolicy, ToolRegistry},
    };

    struct StaticCatalog;

    #[async_trait]
    impl AuthenticatedModelCatalog for StaticCatalog {
        async fn list_authenticated_models(&self) -> Result<Vec<RlmModel>> {
            Ok(vec![model()])
        }
    }

    struct StaticProviderFactory;

    #[async_trait]
    impl RlmProviderFactory for StaticProviderFactory {
        async fn create_provider(&self, _model: &RlmModel) -> Result<Arc<dyn Provider>> {
            Ok(Arc::new(FakeProvider::new(vec![ModelResponse {
                message: Message::assistant(
                    vec![Content::Text {
                        text: "nested task complete".into(),
                    }],
                    StopReason::Stop,
                ),
                response_id: Some("nested-fake".into()),
            }])))
        }
    }

    fn model() -> RlmModel {
        RlmModel {
            provider: "openai".into(),
            id: "gpt-5-mini".into(),
            name: "GPT-5 Mini".into(),
        }
    }

    fn request(state: &TempDir, depth: u32) -> RlmExecutionRequest {
        RlmExecutionRequest {
            child_id: format!("child-{depth}"),
            session_id: format!("session-{depth}"),
            session_name: format!("agent-{depth}"),
            session_dir: state.path().join(format!("child-{depth}")),
            parent_session_id: "root".into(),
            parent_session_path: None,
            parent_node_id: None,
            depth,
            prompt: "nested task".into(),
            spawn_code: None,
            model: model(),
            max_output_tokens: 128,
        }
    }

    fn limits() -> RlmRuntimeLimits {
        RlmRuntimeLimits {
            max_prompt_bytes: 4096,
            max_spawn_code_bytes: 4096,
            max_children: 8,
            max_concurrent_children: 2,
            max_depth: 3,
            max_duration: Duration::from_secs(5),
            max_output_tokens: 128,
            max_catalog_models: 8,
            max_state_bytes: 1024 * 1024,
        }
    }

    #[tokio::test]
    async fn depth_one_child_can_admit_depth_two_but_max_depth_has_no_rlm_aliases() {
        let state = TempDir::new().expect("state");
        let workspace = TempDir::new().expect("workspace");
        let base_tools = Arc::new(
            ToolRegistry::with_default_tools(workspace.path(), ToolPolicy::default())
                .expect("base tools"),
        );
        let factory = Arc::new(CliRlmChildToolFactory {
            base_tools,
            providers: Arc::new(StaticProviderFactory),
            catalog: Arc::new(StaticCatalog),
            state: PathBuf::from(state.path()),
            workspace: PathBuf::from(workspace.path()),
            policy: RlmChildRuntimePolicy::default(),
            limits: limits(),
            kernel_policy: None,
        });

        let depth_one_tools = Arc::clone(&factory)
            .tools_for_child(&request(&state, 1))
            .await
            .expect("depth-one tools");
        assert!(
            depth_one_tools
                .definitions()
                .iter()
                .any(|definition| definition.name == "rlm_run")
        );
        let admission = depth_one_tools
            .execute("rlm_run", serde_json::json!({"prompt": "grandchild"}))
            .await
            .expect("depth-two admission");
        let admission: serde_json::Value =
            serde_json::from_str(&admission.content).expect("admission JSON");
        assert!(admission["rlm_child_id"].as_str().is_some());

        let max_depth_tools = factory
            .tools_for_child(&request(&state, 3))
            .await
            .expect("max-depth tools");
        assert!(
            max_depth_tools
                .definitions()
                .iter()
                .all(|definition| !definition.name.starts_with("rlm_"))
        );
    }
}
