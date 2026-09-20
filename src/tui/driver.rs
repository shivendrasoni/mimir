use std::{
    collections::BTreeMap,
    fmt::Write as _,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::{
        self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode as CrosstermKeyCode,
        KeyEventKind, KeyModifiers, ModifierKeyCode,
    },
    execute, queue,
    terminal::{
        Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
        enable_raw_mode, size,
    },
};

use crate::{
    auth::{AuthStore, DeviceAuthorization, OAuthProvider, PendingOAuth},
    error::Result,
    extensions::{SessionForkPosition, SessionStartReason, SessionSwitchReason},
    learning::{self, LearningMode},
    mcp::{McpAuthCoordinator, McpOAuthAuthorization, McpOAuthClient, McpOAuthCodeReceiver},
    model::{Content, Message, Role, Usage},
    orchestration::{
        GoalStatus, GoalStore, HeartbeatDeliveryMode, HeartbeatManagementAction, Schedule,
        ScheduleStore,
    },
    provider::registry::{AuthKind, ProviderRegistry},
    refinement::{self, RefineOptions},
    runtime::{AgentRuntime, EventSink, QueueMode, RuntimeEvent},
    session::{FileSessionStore, SessionPayload, SessionRecord, SessionStore},
    session_compat::{ReferenceSessionMetadata, export_jsonl, import_jsonl},
    session_tree::{SessionBranchCatalog, SessionNodeKind},
    tools::{AgentMode, ApprovalDecision, BashResult, ObservationStatus, PermissionRequest},
};
use uuid::Uuid;

use super::{
    App, AppConfig, AppPreferenceState, AutonomousLimits, AutonomousState, KeyCode, KeyEvent,
    RenderOptions, SideQuestionSession, StreamEvent, TerminalCapabilities, TerminalSize,
    TreeFilterMode, TuiAction, TuiResourceSnapshot, UiRequest,
    autonomous::apply_autonomous_command,
    clipboard::{copy_last_assistant_message, read_image as read_clipboard_image},
    path_picker::{discover_workspace_paths, workspace_reference_context},
    side_question::ask_side_question,
};

struct TerminalGuard {
    fullscreen: bool,
}

#[derive(Default)]
struct TerminalFrameCache {
    width: u16,
    height: u16,
    body: String,
    cursor: Option<(u16, u16)>,
    initialized: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TerminalFrameDamage {
    Unchanged,
    Full,
    Rows(Vec<u16>),
}

const DOUBLE_CTRL_C_WINDOW: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CtrlCAction {
    FirstPress,
    Exit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CtrlCPrimaryAction {
    ClearComposer,
    CancelBash,
    CancelRun,
    None,
}

fn ctrl_c_action(last_press: &mut Option<Instant>, now: Instant) -> CtrlCAction {
    if last_press.is_some_and(|last| {
        now.checked_duration_since(last)
            .is_some_and(|elapsed| elapsed <= DOUBLE_CTRL_C_WINDOW)
    }) {
        *last_press = None;
        CtrlCAction::Exit
    } else {
        *last_press = Some(now);
        CtrlCAction::FirstPress
    }
}

fn ctrl_c_primary_action(app: &mut App, runtime_running: bool) -> CtrlCPrimaryAction {
    if app.clear_composer() {
        CtrlCPrimaryAction::ClearComposer
    } else if app.bash_active() {
        CtrlCPrimaryAction::CancelBash
    } else if runtime_running || app.run_active() {
        CtrlCPrimaryAction::CancelRun
    } else {
        CtrlCPrimaryAction::None
    }
}

impl TerminalFrameCache {
    fn frame_damage(
        &mut self,
        width: u16,
        height: u16,
        body: &str,
        cursor: Option<(u16, u16)>,
    ) -> TerminalFrameDamage {
        let dimensions_changed = !self.initialized || self.width != width || self.height != height;
        let cursor_changed = self.cursor != cursor;
        let damage = if dimensions_changed {
            TerminalFrameDamage::Full
        } else {
            let previous = self.body.split("\r\n").collect::<Vec<_>>();
            let current = body.split("\r\n").collect::<Vec<_>>();
            let changed_rows = (0..previous.len().max(current.len()))
                .filter(|&index| previous.get(index) != current.get(index))
                .filter_map(|index| u16::try_from(index).ok())
                .collect::<Vec<_>>();
            if changed_rows.is_empty() && !cursor_changed {
                TerminalFrameDamage::Unchanged
            } else {
                TerminalFrameDamage::Rows(changed_rows)
            }
        };
        self.width = width;
        self.height = height;
        body.clone_into(&mut self.body);
        self.cursor = cursor;
        self.initialized = true;
        damage
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent persisted TUI display toggles intentionally remain explicit"
)]
struct TuiPreferences {
    #[serde(flatten)]
    toggles: TuiTogglePreferences,
    scoped_models: Vec<String>,
    scoped_models_configured: bool,
    theme: String,
    agent_mode: String,
    rlm_max_depth: u32,
    rlm_session_depths: BTreeMap<String, u32>,
    steering_mode: String,
    follow_up_mode: String,
    show_images: bool,
    auto_resize_images: bool,
    block_images: bool,
    autocomplete_max_visible: u8,
    tree_filter_mode: String,
    show_hardware_cursor: bool,
    editor_padding_x: u8,
    show_terminal_progress: bool,
    show_warnings: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
struct TuiTogglePreferences {
    fast_mode: bool,
    fullscreen: bool,
    auto_compaction: bool,
}

impl Default for TuiTogglePreferences {
    fn default() -> Self {
        Self {
            fast_mode: false,
            fullscreen: true,
            auto_compaction: true,
        }
    }
}

impl Default for TuiPreferences {
    fn default() -> Self {
        Self {
            toggles: TuiTogglePreferences::default(),
            scoped_models: Vec::new(),
            scoped_models_configured: false,
            theme: "dark".into(),
            agent_mode: AgentMode::Default.as_str().into(),
            rlm_max_depth: 3,
            rlm_session_depths: BTreeMap::new(),
            steering_mode: QueueMode::OneAtATime.as_str().into(),
            follow_up_mode: QueueMode::OneAtATime.as_str().into(),
            show_images: true,
            auto_resize_images: true,
            block_images: false,
            autocomplete_max_visible: 8,
            tree_filter_mode: TreeFilterMode::Default.as_str().into(),
            show_hardware_cursor: true,
            editor_padding_x: 0,
            show_terminal_progress: true,
            show_warnings: true,
        }
    }
}

struct TerminalMcpOAuthReceiver;

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct LocalTracePreview<'a> {
    schema_version: u8,
    session: &'a str,
    record_count: usize,
    message_count: usize,
    event_counts: BTreeMap<String, usize>,
    diagnostic_run_ids: Vec<uuid::Uuid>,
    diagnostic_bundle_root: String,
    contains_message_text: bool,
    uploaded: bool,
}

#[async_trait]
impl McpOAuthCodeReceiver for TerminalMcpOAuthReceiver {
    async fn receive_code(&self, authorization: &McpOAuthAuthorization) -> Result<String> {
        TerminalGuard::suspend()?;
        let input_result = (|| -> io::Result<String> {
            println!("Open: {}", authorization.authorization_url());
            print!("Paste the full callback URL (including code and state): ");
            io::stdout().flush()?;
            let mut input = String::new();
            io::stdin().read_line(&mut input)?;
            Ok(input.trim().into())
        })();
        let resume_result = TerminalGuard::resume();
        let input = input_result?;
        resume_result?;
        Ok(input)
    }
}

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        execute!(
            io::stdout(),
            EnterAlternateScreen,
            EnableBracketedPaste,
            Hide
        )?;
        Ok(Self { fullscreen: true })
    }

    fn suspend() -> io::Result<()> {
        disable_raw_mode()?;
        execute!(
            io::stdout(),
            DisableBracketedPaste,
            Show,
            LeaveAlternateScreen
        )
    }

    fn resume() -> io::Result<()> {
        enable_raw_mode()?;
        execute!(
            io::stdout(),
            EnterAlternateScreen,
            EnableBracketedPaste,
            Hide
        )
    }

    fn set_fullscreen(&mut self, enabled: bool) -> io::Result<()> {
        if enabled == self.fullscreen {
            return Ok(());
        }
        if enabled {
            execute!(io::stdout(), EnterAlternateScreen, Hide)?;
        } else {
            execute!(io::stdout(), Show, LeaveAlternateScreen, Hide)?;
        }
        self.fullscreen = enabled;
        Ok(())
    }

    fn restore_layout(&self) -> io::Result<()> {
        if self.fullscreen {
            execute!(io::stdout(), EnterAlternateScreen, Hide)
        } else {
            execute!(io::stdout(), Show, LeaveAlternateScreen, Hide)
        }
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            DisableBracketedPaste,
            Show,
            LeaveAlternateScreen
        );
    }
}

struct TuiSink {
    app: Arc<Mutex<App>>,
}

#[async_trait]
pub trait TuiRuntimeFactory: Send + Sync {
    async fn build(&self, model: &str, session: &str) -> Result<Arc<AgentRuntime>>;

    /// Returns the canonical workspace whose marker owns project learning.
    fn workspace_root(&self) -> Option<PathBuf> {
        None
    }

    /// Returns the command permission mode used when building runtimes.
    fn agent_mode(&self) -> AgentMode {
        AgentMode::Default
    }

    /// Changes the command permission mode for subsequently built runtimes.
    ///
    /// # Errors
    ///
    /// Returns a configuration error when the active coordinator cannot rebuild its tool policy.
    fn set_agent_mode(&self, _mode: AgentMode) -> Result<()> {
        Err(crate::error::MimirError::Configuration(
            "agent mode changes are unavailable in this TUI runtime".into(),
        ))
    }

    /// Executes one user-owned shell command through the configured bounded runner.
    ///
    /// Implementations must fail closed when process execution or the explicit
    /// program allowlist is unavailable. The default never executes a process.
    async fn execute_user_bash(
        &self,
        _runtime: &AgentRuntime,
        _command: &str,
        _exclude_from_context: bool,
    ) -> Result<BashResult> {
        Err(crate::error::MimirError::Configuration(
            "user bash requires an allowlisted runtime shell policy".into(),
        ))
    }

    /// Cancels the currently running user shell command, when one exists.
    fn abort_user_bash(&self) {}

    /// Resolves provider-aware limits for the next autonomous task.
    fn autonomous_limits_for_provider(&self, _provider: &str) -> AutonomousLimits {
        AutonomousLimits::default()
    }

    /// Persists an explicit workspace-owner choice for a displayed request.
    async fn record_workspace_permission(
        &self,
        _request: PermissionRequest,
        _decision: ApprovalDecision,
    ) -> Result<String> {
        Err(crate::error::MimirError::Configuration(
            "workspace permission decisions are unavailable in this TUI runtime".into(),
        ))
    }

    /// Returns the bounded resource catalogs available to interactive selectors.
    async fn resource_snapshot(&self) -> Result<TuiResourceSnapshot> {
        Ok(TuiResourceSnapshot::default())
    }

    /// Handles a typed TUI action that is owned by an outer session or service coordinator.
    ///
    /// # Errors
    ///
    /// Returns a configuration error by default so unsupported actions remain visible rather
    /// than being silently discarded. Coordinators can override this hook for session trees,
    /// forks, clones, naming, new sessions, and MCP management.
    async fn handle_tui_action(&self, action: TuiAction) -> Result<String> {
        let missing = match action {
            TuiAction::ShowSessionTree => {
                "session-tree selection requires a branch catalog API that the Rust runtime does not expose"
            }
            TuiAction::ContinueAt { .. } => {
                "session-tree continuation requires an active session coordinator"
            }
            TuiAction::Fork | TuiAction::ForkAt { .. } => {
                "fork selection requires a user-message branch selector API that the Rust runtime does not expose"
            }
            TuiAction::Mcp(_) => {
                "MCP management requires a configured server catalog and OAuth coordinator"
            }
            TuiAction::SideQuestion { .. } => {
                "side-question isolation is not implemented by the Rust runtime"
            }
            TuiAction::ShareSession => {
                "session sharing requires an explicitly configured secret-gist transport"
            }
            TuiAction::Traces { .. } => {
                "Mimir trace capture and upload are not implemented by the Rust runtime"
            }
            TuiAction::Autonomous { .. } => {
                "autonomous continuation limits are not implemented by the Rust runtime"
            }
            TuiAction::ToggleFast => {
                "OpenAI Fast mode requires provider service-tier wiring in the active runtime factory"
            }
            TuiAction::ConfigureScopedModels => {
                "scoped model preferences require an active TUI coordinator"
            }
            TuiAction::SetAgentMode(_) => "agent mode changes require an active TUI coordinator",
            TuiAction::ImportSession { .. }
            | TuiAction::ShowSystemPrompt
            | TuiAction::ShowLogs
            | TuiAction::Update { .. }
            | TuiAction::RlmMaxDepth { .. }
            | TuiAction::Fullscreen { .. }
            | TuiAction::SetTheme
            | TuiAction::PersistSettings => {
                "the active TUI coordinator does not implement this built-in action"
            }
            _ => "the active TUI coordinator does not implement this action",
        };
        Err(crate::error::MimirError::Configuration(missing.into()))
    }
}

#[async_trait]
impl EventSink for TuiSink {
    #[allow(
        clippy::too_many_lines,
        reason = "runtime event translation exhaustively maps the public event surface"
    )]
    async fn emit(&self, event: RuntimeEvent) {
        if let RuntimeEvent::MessageCompleted { message } = &event {
            let mut state = self
                .app
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for content in &message.content {
                match content {
                    Content::Thinking {
                        text,
                        redacted: false,
                        ..
                    } => state.apply_stream_event(StreamEvent::Thinking(text.clone())),
                    Content::Image { mime_type, .. } => {
                        state.apply_stream_event(StreamEvent::Images(vec![mime_type.clone()]));
                    }
                    Content::Text { .. }
                    | Content::Thinking { .. }
                    | Content::ToolCall(_)
                    | Content::ToolResult(_) => {}
                }
            }
            return;
        }
        let event = match event {
            RuntimeEvent::RunStarted => Some(StreamEvent::RunStarted),
            RuntimeEvent::TextDelta { text } => Some(StreamEvent::TextDelta(text)),
            RuntimeEvent::AutoRetryStarted {
                attempt,
                max_attempts,
                delay_ms,
                ..
            } => Some(StreamEvent::RetryStarted {
                attempt,
                max_attempts,
                delay_ms,
            }),
            RuntimeEvent::AutoRetryFinished {
                success,
                attempt,
                final_error,
            } => Some(StreamEvent::RetryFinished {
                success,
                attempt,
                final_error,
            }),
            RuntimeEvent::Completed { text } => Some(StreamEvent::Completed(text)),
            RuntimeEvent::Failed { message } => Some(StreamEvent::Failed(message)),
            RuntimeEvent::BudgetPaused { pause } => {
                let override_hint = match pause.kind {
                    crate::budget::BudgetKind::Turns => "--max-turns unlimited",
                    crate::budget::BudgetKind::Tokens => "--max-run-tokens unlimited",
                    crate::budget::BudgetKind::ToolCalls => {
                        "an unrestricted provider or runtime tool-call policy"
                    }
                    crate::budget::BudgetKind::Elapsed => {
                        "--autonomous-timeout-ms unlimited in autonomous mode"
                    }
                };
                Some(StreamEvent::BudgetPaused(format!(
                    "Configured budget paused the task: {pause}. Increase the finite limit or use {override_hint}."
                )))
            }
            RuntimeEvent::ProviderRequest { .. } => Some(StreamEvent::Activity("Thinking…".into())),
            RuntimeEvent::ToolStarted { name, .. } => {
                Some(StreamEvent::Activity(tool_activity(&name)))
            }
            RuntimeEvent::PermissionRequested { request } => {
                self.app
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .open_workspace_permission(request);
                None
            }
            RuntimeEvent::UserInputRequested { request } => {
                Some(StreamEvent::UserInputRequested(request))
            }
            RuntimeEvent::ToolFinished {
                name, observation, ..
            } => {
                let summary = if observation.artifacts.is_empty() {
                    observation.summary
                } else {
                    format!(
                        "{} · {}",
                        observation.summary,
                        observation
                            .artifacts
                            .iter()
                            .map(|path| path.display().to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                };
                Some(StreamEvent::ToolFinished {
                    name,
                    summary,
                    failed: observation.status == ObservationStatus::Error,
                })
            }
            RuntimeEvent::ExtensionError {
                extension_path,
                event,
                error,
            } => Some(StreamEvent::Warning(format!(
                "extension warning ({extension_path}, {event}): {error}"
            ))),
            RuntimeEvent::ExtensionUi { extension, request } => {
                Some(StreamEvent::ExtensionUi { extension, request })
            }
            RuntimeEvent::ExtensionRendered { custom_type, lines } => {
                Some(StreamEvent::ExtensionRendered { custom_type, lines })
            }
            RuntimeEvent::MessageCompleted { .. }
            | RuntimeEvent::SessionEvent { .. }
            | RuntimeEvent::MessageStarted { .. }
            | RuntimeEvent::TurnCompleted { .. }
            | RuntimeEvent::ToolUpdated { .. } => None,
        };
        if let Some(event) = event {
            self.app
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .apply_stream_event(event);
        }
    }
}

fn tool_activity(name: &str) -> String {
    match name {
        "read_file" | "read" => "Reading files…".into(),
        "list_files" | "glob" => "Exploring files…".into(),
        "search" | "grep" => "Searching the codebase…".into(),
        "write_file" | "edit_file" | "apply_patch" => "Editing files…".into(),
        "bash" | "process" | "exec" => "Running a command…".into(),
        other => format!("Using {}…", other.replace('_', " ")),
    }
}

#[cfg(test)]
const MAX_SESSION_EVENT_FIELD_CHARS: usize = 128;

#[cfg(test)]
fn summarize_session_event(event: &serde_json::Value) -> String {
    let Some(object) = event.as_object() else {
        return "session event: non-object metadata".into();
    };
    let mut details = Vec::with_capacity(5);
    if let Some(kind) = bounded_session_event_string(object.get("type")) {
        details.push(format!("type={kind}"));
    }
    if let Some(session) = bounded_session_event_string(object.get("activeSessionId")) {
        details.push(format!("session={session}"));
    }
    if let Some(nested) = object.get("event").and_then(serde_json::Value::as_object) {
        if let Some(kind) = bounded_session_event_string(nested.get("type")) {
            details.push(format!("event={kind}"));
        }
        if let Some(tokens) = nested
            .get("result")
            .and_then(|result| result.get("tokensBefore"))
            .and_then(serde_json::Value::as_u64)
        {
            details.push(format!("tokensBefore={tokens}"));
        }
        if let Some(aborted) = nested.get("aborted").and_then(serde_json::Value::as_bool) {
            details.push(format!("aborted={aborted}"));
        }
    }
    if details.is_empty() {
        let mut keys = object.keys().take(8).cloned().collect::<Vec<_>>();
        keys.sort();
        details.push(format!("keys={}", keys.join(",")));
    }
    format!("session event: {}", details.join(" "))
}

#[cfg(test)]
fn bounded_session_event_string(value: Option<&serde_json::Value>) -> Option<String> {
    value
        .and_then(serde_json::Value::as_str)
        .map(|value| {
            value
                .chars()
                .filter(|character| !character.is_control())
                .take(MAX_SESSION_EVENT_FIELD_CHARS)
                .collect::<String>()
        })
        .filter(|value| !value.is_empty())
}

/// Runs the full-screen terminal client until `/quit`, Ctrl-D, a double Ctrl-C, or closure.
///
/// # Errors
///
/// Returns terminal setup, rendering, or input errors after restoring terminal state.
#[allow(
    clippy::too_many_lines,
    reason = "the terminal event loop keeps rendering, input, runtime switching, and auth lifecycle explicit"
)]
pub async fn run_tui(
    runtime: Arc<AgentRuntime>,
    runtime_factory: Arc<dyn TuiRuntimeFactory>,
    models: Vec<String>,
    sessions: Vec<String>,
    state_root: PathBuf,
    initial_model: String,
    initial_session: String,
) -> Result<()> {
    let session_root = state_root.clone();
    run_tui_with_autonomous(
        runtime,
        runtime_factory,
        models,
        sessions,
        state_root,
        session_root,
        initial_model,
        initial_session,
        None,
    )
    .await
}

/// Runs the full-screen client with optional host-autonomous startup limits.
///
/// # Errors
///
/// Returns terminal setup, rendering, input, or runtime errors.
#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "the terminal coordinator accepts explicit launch policy without global state"
)]
pub async fn run_tui_with_autonomous(
    mut runtime: Arc<AgentRuntime>,
    runtime_factory: Arc<dyn TuiRuntimeFactory>,
    models: Vec<String>,
    sessions: Vec<String>,
    state_root: PathBuf,
    session_root: PathBuf,
    initial_model: String,
    initial_session: String,
    autonomous_limits: Option<AutonomousLimits>,
) -> Result<()> {
    let mut terminal_guard = TerminalGuard::enter()?;
    let mut preferences = load_tui_preferences(&state_root).await?;
    preferences.agent_mode = runtime_factory.agent_mode().as_str().into();
    let resource_snapshot = runtime_factory.resource_snapshot().await?;
    apply_runtime_preferences(&runtime, &preferences).await;
    terminal_guard.set_fullscreen(preferences.toggles.fullscreen)?;
    let app = Arc::new(Mutex::new(App::new(AppConfig::default())));
    {
        let mut state = app
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.set_models(models);
        state.set_sessions(sessions);
        state.set_resource_snapshot(resource_snapshot);
        state.set_preferences(AppPreferenceState {
            agent_mode: AgentMode::parse(&preferences.agent_mode).unwrap_or_default(),
            fast_mode: preferences.toggles.fast_mode,
            fullscreen: preferences.toggles.fullscreen,
            auto_compaction: preferences.toggles.auto_compaction,
            steering_mode: parse_queue_mode(&preferences.steering_mode).unwrap_or_default(),
            theme: super::ThemeName::parse(&preferences.theme).unwrap_or(super::ThemeName::Dark),
            custom_theme: super::ThemeName::parse(&preferences.theme)
                .is_none()
                .then_some(preferences.theme.clone()),
            scoped_models: preferences
                .scoped_models_configured
                .then(|| preferences.scoped_models.into_iter().collect()),
            show_images: preferences.show_images,
            auto_resize_images: preferences.auto_resize_images,
            block_images: preferences.block_images,
            follow_up_mode: parse_queue_mode(&preferences.follow_up_mode).unwrap_or_default(),
            autocomplete_max_visible: preferences.autocomplete_max_visible,
            tree_filter_mode: TreeFilterMode::parse(&preferences.tree_filter_mode)
                .unwrap_or_default(),
            show_hardware_cursor: preferences.show_hardware_cursor,
            editor_padding_x: preferences.editor_padding_x,
            show_terminal_progress: preferences.show_terminal_progress,
            show_warnings: preferences.show_warnings,
        });
        state.select_model(&initial_model);
        state.select_session(&initial_session);
    }
    refresh_extension_commands(&runtime, &app).await;
    if let Some(question) = runtime.pending_plan_question().await? {
        app.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .open_clarification(question);
    }
    let mut runtime_key = Some((initial_model, initial_session));
    let mut runtime_needs_refresh = false;
    let mut autonomous_state = AutonomousState::default();
    if let Some(limits) = autonomous_limits {
        autonomous_state.set_limits(limits);
        autonomous_state.enable(Instant::now());
    }
    let autonomous = Arc::new(Mutex::new(autonomous_state));
    let mut side_questions = SideQuestionSession::default();
    let mut frame_cache = TerminalFrameCache::default();
    let mut last_ctrl_c_press = None;
    let mut shift_held = false;

    loop {
        render_frame(&app, &mut frame_cache)?;
        if app
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .should_quit()
        {
            return Ok(());
        }
        let terminal_event = tokio::task::spawn_blocking(|| {
            if event::poll(Duration::from_millis(50))? {
                event::read().map(Some)
            } else {
                Ok(None)
            }
        })
        .await
        .map_err(|error| io::Error::other(error.to_string()))??;

        if let Some(Event::Paste(text)) = terminal_event.as_ref() {
            app.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert_text(text);
            continue;
        }

        if let Some(Event::Key(mut key)) = terminal_event {
            if is_shift_modifier(&key) {
                shift_held = key.kind != KeyEventKind::Release;
                continue;
            }
            if key.kind == KeyEventKind::Release {
                continue;
            }
            if shift_held && matches!(key.code, CrosstermKeyCode::Enter) {
                key.modifiers.insert(KeyModifiers::SHIFT);
            }
            if key.modifiers.contains(KeyModifiers::CONTROL)
                && matches!(key.code, CrosstermKeyCode::Char('v' | 'V'))
            {
                match read_clipboard_image().await {
                    Ok(Some(image)) => {
                        let mut state = app
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        if let Err(error) = state.attach_image(image) {
                            state.push_warning_message(error);
                        }
                    }
                    Ok(None) => {}
                    Err(error) => app
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push_warning_message(format!("Could not attach clipboard image: {error}")),
                }
                continue;
            }
            if key.modifiers.contains(KeyModifiers::CONTROL)
                && matches!(key.code, CrosstermKeyCode::Char('c' | 'C'))
            {
                if key.kind == KeyEventKind::Press {
                    if ctrl_c_action(&mut last_ctrl_c_press, Instant::now()) == CtrlCAction::Exit {
                        return Ok(());
                    }
                    let primary_action = {
                        let mut state = app
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        ctrl_c_primary_action(&mut state, runtime.is_running())
                    };
                    match primary_action {
                        CtrlCPrimaryAction::ClearComposer | CtrlCPrimaryAction::None => {}
                        CtrlCPrimaryAction::CancelBash => runtime_factory.abort_user_bash(),
                        CtrlCPrimaryAction::CancelRun => {
                            runtime.cancel();
                            autonomous
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .cancel_current();
                        }
                    }
                }
                continue;
            }
            if key.kind == KeyEventKind::Press {
                last_ctrl_c_press = None;
            }
            if key.modifiers.contains(KeyModifiers::CONTROL)
                && matches!(key.code, CrosstermKeyCode::Char('d'))
            {
                return Ok(());
            }
            if let Some(key) = convert_key(key) {
                let refreshed_workspace_paths = matches!(key.code, KeyCode::Char('@'))
                    .then(|| runtime_factory.workspace_root())
                    .flatten()
                    .as_deref()
                    .map(discover_workspace_paths);
                let (prompt, images, ui_request, tui_action, selected_model, selected_session) = {
                    let mut state = app
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if let Some(paths) = refreshed_workspace_paths {
                        state.set_workspace_paths(paths);
                    }
                    state.apply_key(key);
                    let prompt = state.pending_submission();
                    if prompt.is_some() {
                        state.clear_pending_submission();
                    }
                    let images = if prompt.is_some() {
                        state.take_submitted_images()
                    } else {
                        Vec::new()
                    };
                    (
                        prompt,
                        images,
                        state.take_ui_request(),
                        state.take_tui_action(),
                        state.selected_model().map(str::to_owned),
                        state.selected_session().map(str::to_owned),
                    )
                };
                if let Some(prompt) = prompt {
                    if let Some(user_bash) = parse_user_bash_submission(&prompt) {
                        if app
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .agent_mode()
                            == AgentMode::Plan
                        {
                            app.lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .push_system_message("Direct commands are disabled in plan mode");
                            continue;
                        }
                        if user_bash.command.is_empty() {
                            app.lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .push_system_message("Enter a command after ! or !!");
                            continue;
                        }
                        let busy = runtime.is_running() || {
                            let state = app
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            state.run_active() || state.bash_active()
                        };
                        if busy {
                            app.lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .push_system_message(
                                    "Wait for the active agent or bash command to finish",
                                );
                            continue;
                        }
                        {
                            let mut state = app
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            state.set_bash_active(true);
                            state.apply_stream_event(StreamEvent::BashStarted {
                                command: user_bash.command.clone(),
                                exclude_from_context: user_bash.exclude_from_context,
                            });
                        }
                        let runtime = Arc::clone(&runtime);
                        let runtime_factory = Arc::clone(&runtime_factory);
                        let app_for_bash = Arc::clone(&app);
                        tokio::spawn(async move {
                            run_tui_user_bash(runtime, runtime_factory, app_for_bash, user_bash)
                                .await;
                        });
                        continue;
                    }
                    if app
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .bash_active()
                    {
                        app.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push_system_message("Wait for the active bash command to finish");
                        continue;
                    }
                    let tui_busy = runtime.is_running()
                        || app
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .run_active();
                    if tui_busy {
                        let mut state = app
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        match state.queue_follow_up_with_images(prompt.clone(), images.clone()) {
                            Ok(()) => {
                                let follow_up_mode = state.follow_up_mode().as_str().to_owned();
                                state.push_user_submission(&prompt, images.len());
                                state.push_system_message(format!(
                                    "Follow-up queued ({follow_up_mode})"
                                ));
                            }
                            Err(error) => state.push_system_message(error),
                        }
                        continue;
                    }
                    let model = selected_model.unwrap_or_else(|| {
                        runtime_key
                            .as_ref()
                            .map_or_else(String::new, |key| key.0.clone())
                    });
                    let session = selected_session.unwrap_or_else(|| {
                        runtime_key.as_ref().map_or_else(
                            || format!("session-{}", Uuid::new_v4().simple()),
                            |key| key.1.clone(),
                        )
                    });
                    let selected_key = (model.clone(), session.clone());
                    if runtime_needs_refresh || runtime_key.as_ref() != Some(&selected_key) {
                        match build_runtime_with_preferences(
                            runtime_factory.as_ref(),
                            &model,
                            &session,
                            &state_root,
                        )
                        .await
                        {
                            Ok(selected_runtime) => {
                                let session_changed = runtime_key
                                    .as_ref()
                                    .is_some_and(|(_, current)| current != &session);
                                if session_changed
                                    && let Err(error) = runtime
                                        .before_session_switch(
                                            SessionSwitchReason::Resume,
                                            Some(
                                                session_root
                                                    .join("sessions")
                                                    .join(format!("{session}.jsonl"))
                                                    .display()
                                                    .to_string(),
                                            ),
                                        )
                                        .await
                                {
                                    app.lock()
                                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                                        .push_system_message(format!(
                                            "runtime switch failed: {error}"
                                        ));
                                    continue;
                                }
                                let previous = runtime_key.as_ref().map(|(_, current)| {
                                    session_root
                                        .join("sessions")
                                        .join(format!("{current}.jsonl"))
                                        .display()
                                        .to_string()
                                });
                                if let Err(error) = runtime
                                    .shutdown_extension_session(if session_changed {
                                        "session_switch"
                                    } else {
                                        "reload"
                                    })
                                    .await
                                {
                                    app.lock()
                                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                                        .push_system_message(format!(
                                            "runtime switch failed: {error}"
                                        ));
                                    continue;
                                }
                                if let Err(error) = selected_runtime
                                    .start_extension_session(
                                        if session_changed {
                                            SessionStartReason::Resume
                                        } else {
                                            SessionStartReason::Reload
                                        },
                                        previous,
                                    )
                                    .await
                                {
                                    app.lock()
                                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                                        .push_system_message(format!(
                                            "runtime switch failed: {error}"
                                        ));
                                    continue;
                                }
                                runtime = selected_runtime;
                                runtime_key = Some(selected_key);
                                runtime_needs_refresh = false;
                                side_questions.clear();
                                refresh_extension_commands(&runtime, &app).await;
                            }
                            Err(error) => {
                                app.lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                                    .push_system_message(format!("runtime switch failed: {error}"));
                                continue;
                            }
                        }
                    }
                    let (provider, _, _) = runtime.model_selection().await;
                    let limits = runtime_factory.autonomous_limits_for_provider(&provider);
                    autonomous
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .set_limits(limits);
                    app.lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .set_run_active(true);
                    app.lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push_user_submission(&prompt, images.len());
                    let learning_workspace = runtime_factory.workspace_root();
                    let message = user_message(&prompt, images, learning_workspace.as_deref());
                    let runtime = runtime.clone();
                    let runtime_factory = Arc::clone(&runtime_factory);
                    let app_for_run = app.clone();
                    let autonomous = autonomous.clone();
                    let learning_session = session.clone();
                    tokio::spawn(async move {
                        run_tui_prompt_loop(
                            runtime,
                            runtime_factory,
                            app_for_run,
                            autonomous,
                            message,
                            learning_workspace,
                            learning_session,
                        )
                        .await;
                    });
                }
                if let Some(request) = ui_request {
                    let result = handle_ui_request(&state_root, request).await;
                    terminal_guard.restore_layout()?;
                    mark_runtime_credentials_stale(&result, &mut runtime_needs_refresh);
                    let message = match result {
                        Ok(message) => message,
                        Err(error) => format!("authentication failed: {error}"),
                    };
                    app.lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push_system_message(message);
                }
                if let Some(action) = tui_action {
                    let compacting = matches!(action, TuiAction::Compact { .. });
                    if compacting {
                        begin_compaction_feedback(
                            &mut app
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner),
                        );
                        render_frame(&app, &mut frame_cache)?;
                    }
                    let result = match dispatch_coordinator_action(
                        &action,
                        &mut runtime,
                        &runtime_factory,
                        &mut runtime_key,
                        &app,
                        &state_root,
                        &session_root,
                        &autonomous,
                        &mut side_questions,
                        &mut terminal_guard,
                    )
                    .await
                    {
                        Ok(Some(message)) => Ok(message),
                        Ok(None) => runtime_factory.handle_tui_action(action).await,
                        Err(error) => Err(error),
                    };
                    let message = match result {
                        Ok(message) => message,
                        Err(error) => format!("command failed: {error}"),
                    };
                    let mut state = app
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if compacting {
                        finish_compaction_feedback(&mut state);
                    }
                    state.push_system_message(message);
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct UserBashSubmission {
    command: String,
    exclude_from_context: bool,
}

fn parse_user_bash_submission(prompt: &str) -> Option<UserBashSubmission> {
    if let Some(command) = prompt.strip_prefix("!!") {
        return Some(UserBashSubmission {
            command: command.trim().into(),
            exclude_from_context: true,
        });
    }
    prompt.strip_prefix('!').map(|command| UserBashSubmission {
        command: command.trim().into(),
        exclude_from_context: false,
    })
}

fn begin_compaction_feedback(app: &mut App) {
    app.push_system_message("Compaction started");
    app.apply_stream_event(StreamEvent::Activity("Compacting context…".into()));
}

fn finish_compaction_feedback(app: &mut App) {
    app.clear_current_activity();
}

async fn run_tui_user_bash(
    runtime: Arc<AgentRuntime>,
    runtime_factory: Arc<dyn TuiRuntimeFactory>,
    app: Arc<Mutex<App>>,
    submission: UserBashSubmission,
) {
    let result = runtime_factory
        .execute_user_bash(
            &runtime,
            &submission.command,
            submission.exclude_from_context,
        )
        .await;
    let mut state = app
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    state.set_bash_active(false);
    match result {
        Ok(result) => state.apply_stream_event(StreamEvent::BashFinished {
            output: result.output,
            exit_code: result.exit_code,
            cancelled: result.cancelled,
            truncated: result.truncated,
            timed_out: result.timed_out,
            full_output_path: result
                .full_output_path
                .map(|path| path.display().to_string()),
            error: None,
        }),
        Err(error) => state.apply_stream_event(StreamEvent::BashFinished {
            output: String::new(),
            exit_code: None,
            cancelled: false,
            truncated: false,
            timed_out: false,
            full_output_path: None,
            error: Some(error.to_string()),
        }),
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "the prompt loop keeps completion, continuation, follow-up, and learning state transitions ordered"
)]
async fn run_tui_prompt_loop(
    runtime: Arc<AgentRuntime>,
    runtime_factory: Arc<dyn TuiRuntimeFactory>,
    app: Arc<Mutex<App>>,
    autonomous: Arc<Mutex<AutonomousState>>,
    message: Message,
    learning_workspace: Option<PathBuf>,
    learning_session: String,
) {
    let generation = autonomous
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .begin_run(Instant::now());
    runtime
        .set_autonomous_completion_enabled(generation.is_some())
        .await;
    if generation.is_some() {
        runtime.clear_task_completion().await;
    }
    let mut next_messages = vec![message];
    loop {
        let before = runtime.messages_snapshot().await.len();
        let sink = TuiSink { app: app.clone() };
        if runtime
            .run_batch_messages(&next_messages, &sink)
            .await
            .is_err()
        {
            if let Some(workspace) = &learning_workspace
                && let Ok(state) =
                    learning::record_runtime_failure(workspace, &learning_session).await
                && state.mode == LearningMode::Auto
            {
                match learning::propose_project_candidate(&runtime, workspace).await {
                    Ok(candidate) => app
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push_system_message(format!(
                            "Automatic learning candidate {} is {:?}: {}",
                            candidate.id, candidate.status, candidate.summary
                        )),
                    Err(error) => app
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push_system_message(format!(
                            "Automatic learning proposal failed safely: {error}"
                        )),
                }
            }
            app.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .set_run_active(false);
            return;
        }
        let completion = runtime.take_task_completion().await;
        let (provider, _, _) = runtime.model_selection().await;
        autonomous
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .set_limits(runtime_factory.autonomous_limits_for_provider(&provider));
        let (continuation, status) = if let Some(generation) = generation {
            let messages = runtime.messages_snapshot().await;
            let mut state = autonomous
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for message in messages.iter().skip(before) {
                if message.role == crate::model::Role::Assistant {
                    state.record_turn(generation, message.usage);
                }
            }
            let continuation = completion
                .is_none()
                .then(|| state.next_continuation(generation, Instant::now()))
                .flatten();
            let status = (completion.is_none() && continuation.is_none())
                .then(|| state.status(Instant::now()));
            (continuation, status)
        } else {
            (None, None)
        };
        if let Some(completion) = completion {
            let artifact_suffix = if completion.artifacts.is_empty() {
                String::new()
            } else {
                format!(
                    " · {}",
                    completion
                        .artifacts
                        .iter()
                        .map(|path| path.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };
            app.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push_system_message(format!(
                    "Autonomous task complete: {}{artifact_suffix}",
                    completion.summary
                ));
            app.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .set_run_active(false);
            return;
        }
        if let Some(continuation) = continuation {
            let mut state = app
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.push_system_message("Autonomous continuation started");
            state.push_user_message(&continuation);
            next_messages = vec![Message::user(continuation)];
            continue;
        }
        let follow_ups = app
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take_next_follow_up_messages();
        if !follow_ups.is_empty() {
            next_messages = follow_ups
                .into_iter()
                .map(|(prompt, images)| {
                    user_message(&prompt, images, learning_workspace.as_deref())
                })
                .collect();
            continue;
        }
        if let Some(status) = status {
            app.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push_system_message(status);
        }
        if let Some(workspace) = learning_workspace
            && learning::request_feedback_if_informative(&workspace)
                .await
                .unwrap_or(false)
        {
            app.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push_system_message(
                    "Did this achieve the goal? Use /learn feedback yes or /learn feedback no.",
                );
        }
        app.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .set_run_active(false);
        return;
    }
}

fn user_message(
    prompt: &str,
    images: Vec<super::ImageAttachment>,
    workspace: Option<&Path>,
) -> Message {
    let mut content = Vec::with_capacity(images.len().saturating_add(2));
    if !prompt.trim().is_empty() {
        content.push(Content::Text {
            text: prompt.trim().to_owned(),
        });
    }
    if let Some(context) = workspace.and_then(|root| workspace_reference_context(prompt, root)) {
        content.push(Content::Text { text: context });
    }
    content.extend(images.into_iter().map(|image| Content::Image {
        data: image.data,
        mime_type: image.mime_type,
    }));
    Message::user_content(content)
}

struct PreparedPlanImplementation {
    runtime: Arc<AgentRuntime>,
    model: String,
    session: String,
    relative_plan: String,
}

async fn rebuild_plan_as_auto(
    runtime: &AgentRuntime,
    runtime_factory: &dyn TuiRuntimeFactory,
    runtime_key: Option<&(String, String)>,
    state_root: &Path,
) -> Result<PreparedPlanImplementation> {
    if runtime_factory.agent_mode() != AgentMode::Plan {
        return Err(crate::error::MimirError::Configuration(
            "implement is available only in plan mode".into(),
        ));
    }
    let artifact = runtime.plan_artifact().await?.ok_or_else(|| {
        crate::error::MimirError::Configuration(
            "no non-empty plan artifact is ready; finish the plan first".into(),
        )
    })?;
    let relative_plan = artifact
        .strip_prefix(runtime.workspace_root())
        .map_err(|_| {
            crate::error::MimirError::Configuration(
                "plan artifact escaped the active workspace".into(),
            )
        })?
        .display()
        .to_string();
    let previous = runtime_factory.agent_mode();
    runtime_factory.set_agent_mode(AgentMode::Auto)?;
    let session = active_session(runtime_key)?.to_owned();
    let model = current_model(runtime, runtime_key).await;
    let updated =
        match build_runtime_with_preferences(runtime_factory, &model, &session, state_root).await {
            Ok(updated) => updated,
            Err(error) => {
                let _ = runtime_factory.set_agent_mode(previous);
                return Err(error);
            }
        };
    if let Err(error) = runtime.mark_plan_handed_off().await {
        let _ = runtime_factory.set_agent_mode(previous);
        return Err(error);
    }
    Ok(PreparedPlanImplementation {
        runtime: updated,
        model,
        session,
        relative_plan,
    })
}

fn implementation_prompt(relative_plan: &str, instructions: Option<&str>) -> String {
    let mut prompt = format!(
        "Implement the accepted plan at $WORKSPACE/{relative_plan}. Follow it exactly and verify the result."
    );
    if let Some(instructions) = instructions
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        prompt.push_str(" Additional instructions: ");
        prompt.push_str(instructions);
    }
    prompt
}

#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "the TUI coordinator keeps all typed action ownership and its process-local state explicit"
)]
async fn dispatch_coordinator_action(
    action: &TuiAction,
    runtime: &mut Arc<AgentRuntime>,
    runtime_factory: &Arc<dyn TuiRuntimeFactory>,
    runtime_key: &mut Option<(String, String)>,
    app: &Arc<Mutex<App>>,
    state_root: &Path,
    session_root: &Path,
    autonomous: &Arc<Mutex<AutonomousState>>,
    side_questions: &mut SideQuestionSession,
    terminal_guard: &mut TerminalGuard,
) -> Result<Option<String>> {
    match action {
        TuiAction::SetAgentMode(mode) => {
            let previous = runtime_factory.agent_mode();
            let bash_active = app
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .bash_active();
            if runtime.is_running() || bash_active {
                app.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .set_agent_mode(previous);
                return Err(crate::error::MimirError::Configuration(
                    "wait for the active agent or Bash command before changing modes".into(),
                ));
            }
            runtime_factory.set_agent_mode(*mode)?;
            let session = active_session(runtime_key.as_ref())?.to_owned();
            let model = current_model(runtime, runtime_key.as_ref()).await;
            match build_runtime_with_preferences(
                runtime_factory.as_ref(),
                &model,
                &session,
                state_root,
            )
            .await
            {
                Ok(updated) => {
                    *runtime = updated;
                    *runtime_key = Some((model, session));
                    refresh_extension_commands(runtime, app).await;
                    persist_app_preferences(state_root, app).await?;
                    if let Some(question) = runtime.pending_plan_question().await? {
                        app.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .open_clarification(question);
                    }
                    Ok(Some(format!(
                        "Mode set to {}. {}",
                        mode.as_str(),
                        match mode {
                            AgentMode::Auto =>
                                "Shell commands and workspace edits will run without confirmation.",
                            AgentMode::Plan =>
                                "Only inspection, clarification, and the bound plan artifact are available.",
                            AgentMode::Default =>
                                "Model-issued shell commands and workspace edits require confirmation.",
                        }
                    )))
                }
                Err(error) => {
                    let _ = runtime_factory.set_agent_mode(previous);
                    app.lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .set_agent_mode(previous);
                    Err(error)
                }
            }
        }
        TuiAction::ImplementPlan { instructions } => {
            let bash_active = app
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .bash_active();
            if runtime.is_running() || bash_active {
                return Err(crate::error::MimirError::Configuration(
                    "wait for the active agent or command before implementing".into(),
                ));
            }
            let prepared = rebuild_plan_as_auto(
                runtime,
                runtime_factory.as_ref(),
                runtime_key.as_ref(),
                state_root,
            )
            .await?;
            app.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .set_agent_mode(AgentMode::Auto);
            let relative = prepared.relative_plan;
            let learning_workspace = runtime_factory.workspace_root();
            let learning_session = prepared.session.clone();
            *runtime = prepared.runtime;
            *runtime_key = Some((prepared.model, prepared.session));
            refresh_extension_commands(runtime, app).await;
            persist_app_preferences(state_root, app).await?;
            let prompt = implementation_prompt(&relative, instructions.as_deref());
            {
                let mut state = app
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                state.set_run_active(true);
                state.push_user_submission(&prompt, 0);
            }
            let runtime_for_run = Arc::clone(runtime);
            let runtime_factory_for_run = Arc::clone(runtime_factory);
            let app_for_run = Arc::clone(app);
            let autonomous_for_run = Arc::clone(autonomous);
            tokio::spawn(async move {
                run_tui_prompt_loop(
                    runtime_for_run,
                    runtime_factory_for_run,
                    app_for_run,
                    autonomous_for_run,
                    Message::user(prompt),
                    learning_workspace,
                    learning_session,
                )
                .await;
            });
            Ok(Some(format!(
                "Mode set to auto. Implementing $WORKSPACE/{relative}"
            )))
        }
        TuiAction::WorkspacePermission { request, decision } => runtime_factory
            .record_workspace_permission(request.clone(), *decision)
            .await
            .map(Some),
        TuiAction::Resume { session } => {
            resume_tui_session(
                runtime,
                runtime_factory.as_ref(),
                runtime_key,
                app,
                state_root,
                session_root,
                session,
            )
            .await
        }
        TuiAction::NewSession { name, prompt } => {
            start_tui_session(
                runtime,
                runtime_factory.as_ref(),
                runtime_key,
                app,
                state_root,
                session_root,
                (name.as_deref(), prompt.as_deref()),
            )
            .await
        }
        TuiAction::SetSessionName { name } => {
            update_tui_session_name(runtime_key.as_ref(), session_root, name.as_deref()).await
        }
        TuiAction::Clone => {
            clone_tui_session(
                runtime,
                runtime_factory.as_ref(),
                runtime_key,
                app,
                state_root,
                session_root,
            )
            .await
        }
        TuiAction::Reload => {
            reload_tui_runtime(
                runtime,
                runtime_factory.as_ref(),
                runtime_key.as_ref(),
                app,
                state_root,
                session_root,
            )
            .await
        }
        TuiAction::SetEffort(_) | TuiAction::Compact { .. } | TuiAction::ShowContext => {
            dispatch_runtime_action(runtime.as_ref(), action).await
        }
        TuiAction::SetAutoCompaction { .. } | TuiAction::SetSteeringMode { .. } => {
            let message = dispatch_runtime_action(runtime.as_ref(), action).await?;
            persist_app_preferences(state_root, app).await?;
            Ok(message)
        }
        TuiAction::ShowSessionInfo => {
            show_tui_session_info(runtime, runtime_key.as_ref(), session_root)
                .await
                .map(Some)
        }
        TuiAction::CopyLastMessage => copy_last_assistant_message(runtime).await.map(Some),
        TuiAction::ShowSessionTree => {
            open_tui_tree_selector(runtime_key.as_ref(), app, session_root).await
        }
        TuiAction::ContinueAt { entry_id } => {
            continue_tui_session(
                runtime,
                runtime_factory.as_ref(),
                runtime_key,
                app,
                state_root,
                session_root,
                *entry_id,
            )
            .await
        }
        TuiAction::Fork => open_tui_fork_selector(runtime_key.as_ref(), app, session_root).await,
        TuiAction::ForkAt { entry_id } => {
            fork_tui_session(
                runtime,
                runtime_factory.as_ref(),
                runtime_key,
                app,
                state_root,
                session_root,
                *entry_id,
            )
            .await
        }
        TuiAction::Mcp(command) => handle_tui_mcp(state_root, app, command).await,
        TuiAction::ExportSession { path } => {
            let session = active_session(runtime_key.as_ref())?;
            export_tui_session(session_root, session, path.as_deref())
                .await
                .map(Some)
        }
        TuiAction::ImportSession { path } => {
            import_tui_session(
                runtime,
                runtime_factory.as_ref(),
                runtime_key,
                app,
                state_root,
                session_root,
                path,
            )
            .await
        }
        TuiAction::Goal { arguments } => {
            let session = active_session(runtime_key.as_ref())?;
            let message = dispatch_persistent_action(state_root, session, action).await?;
            if command_mutates(arguments.as_deref()) {
                let goal = GoalStore::new(state_root).load().await?;
                runtime.publish_session_event(serde_json::json!({
                    "type": "goal_update",
                    "goal": goal,
                }))?;
            }
            Ok(message)
        }
        TuiAction::Heartbeat { arguments } => {
            let session = active_session(runtime_key.as_ref())?;
            let message = dispatch_persistent_action(state_root, session, action).await?;
            if command_mutates(arguments.as_deref()) {
                runtime.publish_session_event(serde_json::json!({
                    "type": "heartbeats_changed",
                }))?;
            }
            Ok(message)
        }
        TuiAction::ListHeartbeats => open_heartbeat_manager(state_root, app).await,
        TuiAction::ManageHeartbeat {
            session,
            id,
            action,
        } => manage_tui_heartbeat(runtime, state_root, session, *id, *action).await,
        TuiAction::Refine { arguments } => {
            let session = active_session(runtime_key.as_ref())?;
            run_tui_refinement(
                runtime,
                state_root,
                runtime_factory.workspace_root().as_deref(),
                session,
                arguments.as_deref(),
            )
            .await
        }
        TuiAction::Learn { arguments } => {
            let session = active_session(runtime_key.as_ref())?;
            run_tui_learning(
                runtime,
                state_root,
                runtime_factory.workspace_root().as_deref(),
                session,
                arguments.as_deref(),
            )
            .await
        }
        TuiAction::ShowChangelog => show_tui_changelog(state_root).await.map(Some),
        TuiAction::ShowSystemPrompt => {
            let prompt = runtime.system_prompt_snapshot().await;
            Ok(Some(format!(
                "System Prompt ({} chars)\n\n{prompt}",
                prompt.chars().count()
            )))
        }
        TuiAction::ShowLogs => show_tui_logs(state_root).await.map(Some),
        TuiAction::Update { arguments } => show_tui_update_status(arguments.as_deref()).map(Some),
        TuiAction::RlmMaxDepth { arguments } => {
            let session = active_session(runtime_key.as_ref())?.to_owned();
            let message =
                handle_tui_rlm_max_depth(state_root, &session, arguments.as_deref()).await?;
            if arguments
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty())
            {
                let model = current_model(runtime, runtime_key.as_ref()).await;
                *runtime = build_runtime_with_preferences(
                    runtime_factory.as_ref(),
                    &model,
                    &session,
                    state_root,
                )
                .await?;
                *runtime_key = Some((model, session));
                refresh_extension_commands(runtime, app).await;
            }
            Ok(Some(message))
        }
        TuiAction::ToggleFast => {
            let enabled = app
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .fast_mode();
            if let Err(error) = runtime
                .set_service_tier(enabled.then_some("priority"))
                .await
            {
                app.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .set_fast_mode(!enabled);
                return Err(error);
            }
            persist_app_preferences(state_root, app).await.map(Some)
        }
        TuiAction::ConfigureScopedModels | TuiAction::SetTheme | TuiAction::PersistSettings => {
            persist_app_preferences(state_root, app).await.map(Some)
        }
        TuiAction::Fullscreen { enabled } => {
            let enabled = enabled.unwrap_or(!terminal_guard.fullscreen);
            terminal_guard.set_fullscreen(enabled)?;
            persist_app_preferences(state_root, app).await?;
            Ok(Some(format!(
                "Fullscreen {}",
                if enabled { "enabled" } else { "disabled" }
            )))
        }
        TuiAction::SideQuestion { question } => {
            ask_side_question(runtime, side_questions, question)
                .await
                .map(|answer| Some(format!("Side answer (not saved to the session): {answer}")))
        }
        TuiAction::ShareSession => {
            let session = active_session(runtime_key.as_ref())?;
            preview_tui_share(session_root, session).await.map(Some)
        }
        TuiAction::Traces { arguments } => {
            let session = active_session(runtime_key.as_ref())?;
            preview_tui_traces(session_root, session, arguments.as_deref())
                .await
                .map(Some)
        }
        TuiAction::Autonomous { arguments } => {
            if runtime_factory.agent_mode() == AgentMode::Plan {
                return Err(crate::error::MimirError::Configuration(
                    "autonomous execution is disabled in plan mode".into(),
                ));
            }
            let (message, cancel_run) = apply_autonomous_command(
                &mut autonomous
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
                arguments.as_deref(),
                Instant::now(),
            )?;
            if cancel_run {
                runtime.cancel();
            }
            let enabled = autonomous
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .enabled();
            runtime.set_autonomous_completion_enabled(enabled).await;
            if command_mutates(arguments.as_deref()) {
                runtime.publish_session_event(serde_json::json!({
                    "type": "autonomous_status",
                    "status": {
                        "enabled": enabled,
                        "summary": message,
                    },
                }))?;
            }
            Ok(Some(message))
        }
        TuiAction::ExtensionCommand { name, args } => {
            invoke_tui_extension(runtime, app, name, args)
                .await
                .map(Some)
        }
    }
}

fn command_mutates(arguments: Option<&str>) -> bool {
    arguments
        .map(str::trim)
        .is_some_and(|value| !value.is_empty() && !value.eq_ignore_ascii_case("status"))
}

async fn invoke_tui_extension(
    runtime: &AgentRuntime,
    app: &Arc<Mutex<App>>,
    name: &str,
    args: &str,
) -> Result<String> {
    let manager = runtime.extension_manager().await.ok_or_else(|| {
        crate::error::MimirError::Configuration("extension command runtime is unavailable".into())
    })?;
    let result = runtime.invoke_extension_command(name, args).await?;
    for (_, request) in manager.drain_ui_requests().await {
        app.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .apply_stream_event(StreamEvent::ExtensionUi {
                extension: name.into(),
                request,
            });
    }
    Ok(result.message.unwrap_or_else(|| result.output.to_string()))
}

async fn run_tui_refinement(
    runtime: &AgentRuntime,
    state_root: &Path,
    workspace: Option<&Path>,
    session: &str,
    arguments: Option<&str>,
) -> Result<Option<String>> {
    let mut rest = arguments.unwrap_or_default().trim();
    let mut global = false;
    let mut scope = None;
    if let Some(value) = strip_command_option(rest, "--global") {
        global = true;
        rest = value;
    }
    if let Some(value) = strip_command_option(rest, "--scope") {
        let split = value.find(char::is_whitespace).unwrap_or(value.len());
        let requested = &value[..split];
        let remaining = value[split..].trim_start();
        if requested.is_empty() {
            return Err(crate::error::MimirError::Configuration(
                "Usage: /refine --scope <session|project|user> [instructions]".into(),
            ));
        }
        scope = Some(refinement::HarnessScope::parse(requested).ok_or_else(|| {
            crate::error::MimirError::Configuration(
                "refine scope must be session, project, or user; fleet is read-only".into(),
            )
        })?);
        if scope == Some(refinement::HarnessScope::Fleet) {
            return Err(crate::error::MimirError::Configuration(
                "fleet learning packs are read-only".into(),
            ));
        }
        rest = remaining;
    }
    let mut rollback_id = None;
    let mut instructions = (!rest.is_empty()).then_some(rest);
    if rest == "rollback" {
        return Err(crate::error::MimirError::Configuration(
            "Usage: /refine rollback <refinement-id>".into(),
        ));
    }
    if let Some(value) = strip_command_option(rest, "rollback") {
        let mut id = value;
        if let Some(value) = id.strip_suffix(" --global") {
            global = true;
            id = value.trim_end();
        }
        if id.is_empty() {
            return Err(crate::error::MimirError::Configuration(
                "Usage: /refine rollback <refinement-id>".into(),
            ));
        }
        rollback_id = Some(id);
        instructions = None;
    }
    let result = refinement::refine(
        runtime,
        state_root,
        session,
        RefineOptions {
            instructions,
            rollback_id,
            global,
            scope,
            workspace,
        },
    )
    .await?;
    runtime
        .record_runtime_event("refinement", &serde_json::to_string(&result)?)
        .await?;
    runtime
        .set_harness_context(
            refinement::load_harness_context_for_workspace(
                state_root,
                workspace.unwrap_or(state_root),
                session,
                None,
            )
            .await?,
        )
        .await;
    let applied = result
        .applied_edits
        .iter()
        .filter(|edit| edit.applied)
        .count();
    Ok(Some(format!(
        "Refinement {} complete: {} ({applied} edits applied)",
        result.id, result.summary
    )))
}

#[allow(
    clippy::too_many_lines,
    reason = "the learning command dispatcher keeps one exhaustive command-to-coordinator mapping"
)]
async fn run_tui_learning(
    runtime: &AgentRuntime,
    state_root: &Path,
    workspace: Option<&Path>,
    session: &str,
    arguments: Option<&str>,
) -> Result<Option<String>> {
    let workspace = workspace.unwrap_or(state_root);
    let arguments = arguments.unwrap_or("status").trim();
    let (command, rest) = arguments
        .split_once(char::is_whitespace)
        .map_or((arguments, ""), |(command, rest)| (command, rest.trim()));
    let output = match command {
        "" | "status" => {
            serde_json::to_string_pretty(&learning::learning_status(workspace, state_root).await?)?
        }
        "candidates" => {
            let root = learning::discover_project_root(workspace)?;
            let state = learning::load_learning_state(&root).await?;
            serde_json::to_string_pretty(&state.candidates)?
        }
        "propose" => {
            let candidate = learning::propose_project_candidate(runtime, workspace).await?;
            runtime
                .set_harness_context(
                    refinement::load_harness_context_for_workspace(
                        state_root,
                        workspace,
                        session,
                        Some(&candidate.summary),
                    )
                    .await?,
                )
                .await;
            format!(
                "Learning candidate {} is {:?}: {}. Did this achieve the goal? Use /learn feedback yes or /learn feedback no.",
                candidate.id, candidate.status, candidate.summary
            )
        }
        "feedback" => {
            let achieved = match rest {
                "yes" => true,
                "no" => false,
                _ => {
                    return Err(crate::error::MimirError::Configuration(
                        "Usage: /learn feedback <yes|no>".into(),
                    ));
                }
            };
            learning::record_feedback(workspace, session, achieved, None).await?;
            runtime
                .set_harness_context(
                    refinement::load_harness_context_for_workspace(
                        state_root, workspace, session, None,
                    )
                    .await?,
                )
                .await;
            if achieved {
                "Feedback recorded as verified success".into()
            } else {
                "Feedback recorded as verified failure; the candidate was quarantined".into()
            }
        }
        "rollback" => {
            let id = Uuid::parse_str(rest).map_err(|_| {
                crate::error::MimirError::Configuration(
                    "Usage: /learn rollback <candidate-id>".into(),
                )
            })?;
            learning::rollback_candidate(workspace, id).await?;
            runtime
                .set_harness_context(
                    refinement::load_harness_context_for_workspace(
                        state_root, workspace, session, None,
                    )
                    .await?,
                )
                .await;
            format!("Learning candidate {id} rolled back")
        }
        "mode" => {
            let mode = match rest {
                "off" => LearningMode::Off,
                "observe" => LearningMode::Observe,
                "auto" => LearningMode::Auto,
                _ => {
                    return Err(crate::error::MimirError::Configuration(
                        "Usage: /learn mode <off|observe|auto>".into(),
                    ));
                }
            };
            learning::set_mode(workspace, mode).await?;
            format!("Project learning mode set to {mode:?}").to_ascii_lowercase()
        }
        "contribution" => {
            let enabled = match rest {
                "enable" => true,
                "disable" => false,
                _ => {
                    return Err(crate::error::MimirError::Configuration(
                        "Usage: /learn contribution <enable|disable>".into(),
                    ));
                }
            };
            learning::set_contribution(workspace, enabled).await?;
            format!(
                "Redacted fleet contribution {}",
                if enabled { "enabled" } else { "disabled" }
            )
        }
        "check" => format!(
            "Fleet learning transport: endpoint={}, public_key={}, active_pack={}",
            std::env::var_os("MIMIR_LEARNING_PACK_URL").is_some(),
            std::env::var_os("MIMIR_LEARNING_PUBLIC_KEY").is_some(),
            learning::load_active_fleet_pack(state_root)
                .await?
                .map_or_else(|| "none".into(), |item| item.pack.version)
        ),
        "update" => {
            let endpoint = std::env::var("MIMIR_LEARNING_PACK_URL").map_err(|_| {
                crate::error::MimirError::Configuration(
                    "MIMIR_LEARNING_PACK_URL is not configured".into(),
                )
            })?;
            let key = std::env::var("MIMIR_LEARNING_PUBLIC_KEY").map_err(|_| {
                crate::error::MimirError::Configuration(
                    "MIMIR_LEARNING_PUBLIC_KEY is not configured".into(),
                )
            })?;
            let path = learning::fetch_and_install_signed_pack(state_root, &endpoint, &key).await?;
            format!("Fleet learning pack activated at {}", path.display())
        }
        "submit" => {
            let id = Uuid::parse_str(rest).map_err(|_| {
                crate::error::MimirError::Configuration(
                    "Usage: /learn submit <candidate-id>".into(),
                )
            })?;
            let endpoint = std::env::var("MIMIR_LEARNING_CONTRIBUTION_URL").map_err(|_| {
                crate::error::MimirError::Configuration(
                    "MIMIR_LEARNING_CONTRIBUTION_URL is not configured".into(),
                )
            })?;
            learning::submit_fleet_contribution(workspace, &endpoint, id).await?;
            format!("Redacted learning candidate {id} submitted")
        }
        "pin" => {
            learning::pin_fleet_version(workspace, (!rest.is_empty()).then_some(rest)).await?;
            if rest.is_empty() {
                "Fleet learning pack unpinned".into()
            } else {
                format!("Fleet learning pack pinned to {rest}")
            }
        }
        _ => {
            return Err(crate::error::MimirError::Configuration(
                "Usage: /learn [status|candidates|propose|feedback yes|no|rollback <id>|mode off|observe|auto|contribution enable|disable|check|update|submit <id>|pin [version]]".into(),
            ));
        }
    };
    Ok(Some(output))
}

async fn refresh_extension_commands(runtime: &AgentRuntime, app: &Arc<Mutex<App>>) {
    let commands = if let Some(manager) = runtime.extension_manager().await {
        manager
            .commands()
            .into_iter()
            .map(|command| (command.name, command.description))
            .collect()
    } else {
        Vec::new()
    };
    app.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .set_extension_command_descriptors(commands);
}

fn active_session(runtime_key: Option<&(String, String)>) -> Result<&str> {
    runtime_key
        .map(|key| key.1.as_str())
        .ok_or_else(|| crate::error::MimirError::Configuration("no active session".into()))
}

fn mark_runtime_credentials_stale(result: &Result<String>, runtime_needs_refresh: &mut bool) {
    if result.is_ok() {
        *runtime_needs_refresh = true;
    }
}

const MAX_TUI_SETTINGS_BYTES: u64 = 256 * 1024;
const MAX_TUI_IMPORT_BYTES: u64 = 64 * 1024 * 1024;

fn tui_preferences_path(state_root: &Path) -> PathBuf {
    crate::atomic::canonical_state_root(state_root).join("config/tui.json")
}

async fn load_tui_preferences(state_root: &Path) -> Result<TuiPreferences> {
    let path = tui_preferences_path(state_root);
    let metadata = match tokio::fs::symlink_metadata(&path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(TuiPreferences::default());
        }
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() > MAX_TUI_SETTINGS_BYTES
    {
        return Err(crate::error::MimirError::Configuration(
            "TUI settings must be a bounded regular file and not a symlink".into(),
        ));
    }
    let bytes = tokio::fs::read(&path).await?;
    let preferences: TuiPreferences = serde_json::from_slice(&bytes)?;
    validate_tui_preferences(&preferences)?;
    Ok(preferences)
}

fn validate_tui_preferences(preferences: &TuiPreferences) -> Result<()> {
    let valid_model = |model: &str| {
        !model.is_empty() && model.len() <= 512 && !model.chars().any(char::is_control)
    };
    let valid_theme = !preferences.theme.is_empty()
        && preferences.theme.len() <= 128
        && !preferences.theme.chars().any(char::is_control);
    if !valid_theme
        || AgentMode::parse(&preferences.agent_mode).is_none()
        || parse_queue_mode(&preferences.steering_mode).is_none()
        || parse_queue_mode(&preferences.follow_up_mode).is_none()
        || TreeFilterMode::parse(&preferences.tree_filter_mode).is_none()
        || !(3..=20).contains(&preferences.autocomplete_max_visible)
        || preferences.editor_padding_x > 8
        || preferences.rlm_max_depth > 32
        || preferences.scoped_models.len() > 4096
        || preferences
            .scoped_models
            .iter()
            .any(|model| !valid_model(model))
        || preferences.rlm_session_depths.len() > 4096
        || preferences
            .rlm_session_depths
            .iter()
            .any(|(session, depth)| safe_preview_file_name(session).is_err() || *depth > 32)
    {
        return Err(crate::error::MimirError::Configuration(
            "TUI settings contain invalid or excessive values".into(),
        ));
    }
    Ok(())
}

async fn write_tui_preferences(state_root: &Path, preferences: &TuiPreferences) -> Result<()> {
    let root = crate::atomic::canonical_state_root(state_root);
    let path = tui_preferences_path(&root);
    crate::atomic::prepare_state_path(&root, &path).await?;
    write_tui_export(&path, &serde_json::to_vec_pretty(preferences)?).await
}

async fn persist_app_preferences(state_root: &Path, app: &Arc<Mutex<App>>) -> Result<String> {
    let (
        agent_mode,
        fast_mode,
        fullscreen,
        scoped_models,
        theme,
        auto_compaction,
        steering_mode,
        follow_up_mode,
        show_images,
        auto_resize_images,
        block_images,
        autocomplete_max_visible,
        tree_filter_mode,
        show_hardware_cursor,
        editor_padding_x,
        show_terminal_progress,
        show_warnings,
    ) = {
        let app = app
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let display = app.preferences_snapshot();
        (
            app.agent_mode(),
            app.fast_mode(),
            app.fullscreen(),
            app.scoped_models().iter().cloned().collect::<Vec<_>>(),
            app.theme_label().to_owned(),
            app.auto_compaction(),
            app.steering_mode(),
            app.follow_up_mode(),
            display.show_images,
            display.auto_resize_images,
            display.block_images,
            display.autocomplete_max_visible,
            app.tree_filter_mode(),
            app.show_hardware_cursor(),
            app.editor_padding_x(),
            app.show_terminal_progress(),
            app.show_warnings(),
        )
    };
    let mut preferences = load_tui_preferences(state_root).await?;
    preferences.agent_mode = agent_mode.as_str().into();
    preferences.toggles.fast_mode = fast_mode;
    preferences.toggles.fullscreen = fullscreen;
    preferences.scoped_models = scoped_models;
    preferences.scoped_models_configured = true;
    preferences.theme = theme;
    preferences.toggles.auto_compaction = auto_compaction;
    preferences.steering_mode = steering_mode.as_str().into();
    preferences.follow_up_mode = follow_up_mode.as_str().into();
    preferences.show_images = show_images;
    preferences.auto_resize_images = auto_resize_images;
    preferences.block_images = block_images;
    preferences.autocomplete_max_visible = autocomplete_max_visible;
    preferences.tree_filter_mode = tree_filter_mode.as_str().into();
    preferences.show_hardware_cursor = show_hardware_cursor;
    preferences.editor_padding_x = editor_padding_x;
    preferences.show_terminal_progress = show_terminal_progress;
    preferences.show_warnings = show_warnings;
    write_tui_preferences(state_root, &preferences).await?;
    Ok(format!(
        "TUI settings saved (Fast: {}, {} scoped models)",
        if fast_mode { "on" } else { "off" },
        preferences.scoped_models.len()
    ))
}

fn parse_queue_mode(value: &str) -> Option<QueueMode> {
    match value {
        "all" => Some(QueueMode::All),
        "one-at-a-time" => Some(QueueMode::OneAtATime),
        _ => None,
    }
}

async fn apply_runtime_preferences(runtime: &AgentRuntime, preferences: &TuiPreferences) {
    runtime.set_auto_compaction(preferences.toggles.auto_compaction);
    runtime
        .set_steering_mode(parse_queue_mode(&preferences.steering_mode).unwrap_or_default())
        .await;
}

async fn import_tui_session(
    runtime: &mut Arc<AgentRuntime>,
    runtime_factory: &dyn TuiRuntimeFactory,
    runtime_key: &mut Option<(String, String)>,
    app: &Arc<Mutex<App>>,
    state_root: &Path,
    session_root: &Path,
    requested_path: &str,
) -> Result<Option<String>> {
    if runtime.is_running() {
        return Err(crate::error::MimirError::Configuration(
            "cancel the active run before importing a replacement session".into(),
        ));
    }
    let session = active_session(runtime_key.as_ref())?.to_owned();
    let path = PathBuf::from(requested_path);
    if path.extension().and_then(std::ffi::OsStr::to_str) != Some("jsonl") {
        return Err(crate::error::MimirError::Configuration(
            "import source must use the .jsonl extension".into(),
        ));
    }
    let metadata = tokio::fs::symlink_metadata(&path).await.map_err(|error| {
        crate::error::MimirError::Configuration(format!(
            "cannot import {}: {error}",
            path.display()
        ))
    })?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() > MAX_TUI_IMPORT_BYTES
    {
        return Err(crate::error::MimirError::Configuration(
            "import source must be a regular JSONL file no larger than 64 MiB and not a symlink"
                .into(),
        ));
    }
    let bytes = tokio::fs::read(&path).await?;
    let imported = import_jsonl(&path, &bytes)?;
    let imported_state = imported.state;
    let imported_model = imported_state.model;
    let imported_thinking = imported_state.thinking_level;
    let store = FileSessionStore::create(session_root, &session).await?;
    runtime
        .before_session_switch(
            SessionSwitchReason::Resume,
            Some(path.display().to_string()),
        )
        .await?;
    store.replace_records(imported.records).await?;
    let model = if let Some(selection) = imported_model {
        format!("{}/{}", selection.provider, selection.model)
    } else {
        current_model(runtime, runtime_key.as_ref()).await
    };
    let next_runtime =
        build_runtime_with_preferences(runtime_factory, &model, &session, state_root).await?;
    runtime.shutdown_extension_session("session_import").await?;
    next_runtime
        .start_extension_session(SessionStartReason::Reload, Some(path.display().to_string()))
        .await?;
    *runtime = next_runtime;
    if let Some(level) = imported_thinking
        .as_deref()
        .and_then(parse_tui_thinking_level)
    {
        runtime.set_thinking_level(level).await?;
    }
    *runtime_key = Some((model.clone(), session.clone()));
    let mut app = app
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    app.select_session(&session);
    app.select_model(model);
    Ok(Some(format!(
        "Imported {} and replaced session {session}",
        path.display()
    )))
}

fn parse_tui_thinking_level(value: &str) -> Option<crate::model::ThinkingLevel> {
    crate::model::ThinkingLevel::ALL
        .into_iter()
        .find(|level| level.as_str() == value)
}

async fn show_tui_logs(state_root: &Path) -> Result<String> {
    let logs = crate::atomic::canonical_state_root(state_root).join("logs");
    let mut entries = match tokio::fs::read_dir(&logs).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(format!(
                "Logs\nDirectory: {}\nNo logs written yet.",
                logs.display()
            ));
        }
        Err(error) => return Err(error.into()),
    };
    let mut files = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        if files.len() >= 100 {
            break;
        }
        let metadata = entry.metadata().await?;
        if metadata.is_file() {
            let kibibytes = metadata.len().saturating_add(1023) / 1024;
            files.push(format!(
                "{} ({kibibytes} KiB)",
                entry.file_name().to_string_lossy(),
            ));
        }
    }
    files.sort();
    Ok(format!(
        "Logs\nDirectory: {}\n{}",
        logs.display(),
        if files.is_empty() {
            "No logs written yet.".into()
        } else {
            files.join("\n")
        }
    ))
}

async fn show_tui_changelog(state_root: &Path) -> Result<String> {
    let candidates = [
        crate::atomic::canonical_state_root(state_root).join("CHANGELOG.md"),
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("CHANGELOG.md"),
    ];
    for path in candidates {
        if let Ok(metadata) = tokio::fs::symlink_metadata(&path).await
            && metadata.is_file()
            && !metadata.file_type().is_symlink()
            && metadata.len() <= 1024 * 1024
        {
            let text = tokio::fs::read_to_string(&path).await?;
            let truncated = text.chars().count() > 128 * 1024;
            let text = text.chars().take(128 * 1024).collect::<String>();
            return Ok(format!(
                "What's New — Mimir Rust {}\nSource: {}\n\n{}{}",
                env!("CARGO_PKG_VERSION"),
                path.display(),
                text.trim(),
                if truncated { "\n\n[truncated]" } else { "" }
            ));
        }
    }
    Ok(format!(
        "What's New — Mimir Rust {}\nNo packaged CHANGELOG.md is present in the local installation.",
        env!("CARGO_PKG_VERSION")
    ))
}

fn show_tui_update_status(arguments: Option<&str>) -> Result<String> {
    match arguments.map(str::trim).filter(|value| !value.is_empty()) {
        None | Some("status" | "check") => Ok(format!(
            "Mimir Rust {}. This build has no configured signed update transport; install a newer trusted binary with the same method used for this installation.",
            env!("CARGO_PKG_VERSION")
        )),
        Some(_) => Err(crate::error::MimirError::Configuration(
            "Usage: /update [status|check]".into(),
        )),
    }
}

async fn handle_tui_rlm_max_depth(
    state_root: &Path,
    session: &str,
    arguments: Option<&str>,
) -> Result<String> {
    let arguments = arguments.unwrap_or_default().trim();
    if arguments.is_empty() {
        let preferences = load_tui_preferences(state_root).await?;
        let (depth, source) = preferences
            .rlm_session_depths
            .get(session)
            .map_or((preferences.rlm_max_depth, "global default"), |depth| {
                (*depth, "session")
            });
        return Ok(format!("RLM max depth: {depth} ({source})"));
    }
    let mut tokens = arguments.split_whitespace();
    let value = tokens.next().unwrap_or_default();
    let global = matches!(tokens.next(), Some("--global"));
    if tokens.next().is_some() || (!global && arguments.split_whitespace().count() > 1) {
        return Err(crate::error::MimirError::Configuration(
            "Usage: /rlm-max-depth [<non-negative integer> [--global]]".into(),
        ));
    }
    let depth = value.parse::<u32>().map_err(|_| {
        crate::error::MimirError::Configuration(
            "RLM max depth must be a non-negative integer".into(),
        )
    })?;
    set_tui_rlm_max_depth(state_root, session, depth, global).await?;
    Ok(format!(
        "RLM max depth set: {depth}{}",
        if global {
            " and saved as global default"
        } else {
            ""
        }
    ))
}

/// Persists one bounded session-specific RLM recursion depth and optionally
/// updates the global default used by sessions without an override.
///
/// # Errors
///
/// Returns a typed settings error for an invalid depth, unsafe session id,
/// exhausted preference bound, or failed atomic write.
pub async fn set_tui_rlm_max_depth(
    state_root: &Path,
    session: &str,
    depth: u32,
    global: bool,
) -> Result<()> {
    if session.trim().is_empty() || session.len() > 128 {
        return Err(crate::error::MimirError::Configuration(
            "RLM session id must be non-empty and at most 128 bytes".into(),
        ));
    }
    if depth > 32 {
        return Err(crate::error::MimirError::Configuration(
            "RLM max depth must not exceed 32".into(),
        ));
    }
    let mut preferences = load_tui_preferences(state_root).await?;
    if !preferences.rlm_session_depths.contains_key(session)
        && preferences.rlm_session_depths.len() >= 4096
    {
        return Err(crate::error::MimirError::Configuration(
            "RLM session-depth preference limit reached".into(),
        ));
    }
    preferences.rlm_session_depths.insert(session.into(), depth);
    if global {
        preferences.rlm_max_depth = depth;
    }
    write_tui_preferences(state_root, &preferences).await
}

/// Loads the effective RLM depth written by `/rlm-max-depth` for runtime assembly.
///
/// # Errors
///
/// Returns a typed settings error if the persisted file is malformed or unsafe.
pub async fn load_tui_rlm_max_depth(state_root: &Path, session: &str) -> Result<u32> {
    Ok(load_tui_rlm_max_depth_status(state_root, session)
        .await?
        .effective_max_depth)
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RlmMaxDepthStatus {
    pub effective_max_depth: u32,
    pub global_max_depth: u32,
    pub session_max_depth: Option<u32>,
    pub source: &'static str,
}

/// Loads the effective, session-specific, and global RLM recursion settings.
///
/// # Errors
///
/// Returns a typed settings error if the persisted file is malformed or unsafe.
pub async fn load_tui_rlm_max_depth_status(
    state_root: &Path,
    session: &str,
) -> Result<RlmMaxDepthStatus> {
    let preferences = load_tui_preferences(state_root).await?;
    let session_max_depth = preferences.rlm_session_depths.get(session).copied();
    Ok(RlmMaxDepthStatus {
        effective_max_depth: session_max_depth.unwrap_or(preferences.rlm_max_depth),
        global_max_depth: preferences.rlm_max_depth,
        session_max_depth,
        source: if session_max_depth.is_some() {
            "session"
        } else {
            "global"
        },
    })
}

/// Loads the persisted `OpenAI Fast` preference for provider runtime assembly.
///
/// # Errors
///
/// Returns a typed settings error if the persisted file is malformed or unsafe.
pub async fn load_tui_fast_mode(state_root: &Path) -> Result<bool> {
    Ok(load_tui_preferences(state_root).await?.toggles.fast_mode)
}

/// Loads the persisted command permission mode for interactive startup.
///
/// # Errors
///
/// Returns a typed settings error if the persisted file is malformed or unsafe.
pub async fn load_tui_agent_mode(state_root: &Path) -> Result<AgentMode> {
    let preferences = load_tui_preferences(state_root).await?;
    AgentMode::parse(&preferences.agent_mode).ok_or_else(|| {
        crate::error::MimirError::Configuration("invalid persisted agent mode".into())
    })
}

/// Applies TUI commands backed by durable state stores.
///
/// # Errors
///
/// Returns a typed validation or persistence error for malformed goal/heartbeat commands.
pub async fn dispatch_persistent_action(
    state_root: &Path,
    session: &str,
    action: &TuiAction,
) -> Result<Option<String>> {
    match action {
        TuiAction::Goal { arguments } => handle_goal_command(state_root, arguments.as_deref())
            .await
            .map(Some),
        TuiAction::Heartbeat { arguments } => {
            handle_heartbeat_command(state_root, session, arguments.as_deref())
                .await
                .map(Some)
        }
        TuiAction::ListHeartbeats => {
            let mut heartbeats = ScheduleStore::new(state_root).list_heartbeats().await?;
            heartbeats.sort_by_key(|heartbeat| heartbeat.next_run);
            if heartbeats.is_empty() {
                return Ok(Some("No active heartbeats.".into()));
            }
            let lines = heartbeats
                .iter()
                .map(|heartbeat| {
                    format!(
                        "{} [{}] {}: {}",
                        heartbeat.session_id,
                        heartbeat_status(heartbeat),
                        heartbeat.schedule_expression,
                        heartbeat.prompt
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            Ok(Some(lines))
        }
        _ => Ok(None),
    }
}

async fn handle_goal_command(state_root: &Path, arguments: Option<&str>) -> Result<String> {
    let store = GoalStore::new(state_root);
    let arguments = arguments.unwrap_or("status").trim();
    match arguments.to_ascii_lowercase().as_str() {
        "" | "status" => match store.load().await? {
            Some(goal) => Ok(format!(
                "Goal {}: {} ({} tokens{})",
                goal_status(goal.status),
                goal.objective,
                goal.used_tokens,
                goal.token_budget
                    .map(|budget| format!("/{budget}"))
                    .unwrap_or_default()
            )),
            None => Ok("No active goal.".into()),
        },
        "clear" | "stop" => {
            store.clear().await?;
            Ok("Goal cleared.".into())
        }
        "pause" => {
            let goal = store.set_status(GoalStatus::Paused).await?;
            Ok(format!("Goal paused: {}", goal.objective))
        }
        "resume" => {
            let goal = store.set_status(GoalStatus::Active).await?;
            Ok(format!("Goal resumed: {}", goal.objective))
        }
        _ => {
            let (budget, objective) = parse_goal_create(arguments)?;
            let goal = store.create(objective, budget).await?;
            Ok(format!("Goal active: {}", goal.objective))
        }
    }
}

fn parse_goal_create(arguments: &str) -> Result<(Option<u64>, &str)> {
    let Some(flag) = arguments.split_whitespace().next() else {
        return Err(crate::error::MimirError::Configuration(
            "Usage: /goal [--budget <tokens>] <objective>".into(),
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
        let split = rest.find(char::is_whitespace).ok_or_else(|| {
            crate::error::MimirError::Configuration(
                "Usage: /goal [--budget <tokens>] <objective>".into(),
            )
        })?;
        (&rest[..split], rest[split..].trim())
    };
    let budget = value
        .parse::<u64>()
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| {
            crate::error::MimirError::Configuration(
                "goal token budget must be a positive integer".into(),
            )
        })?;
    if objective.is_empty() {
        return Err(crate::error::MimirError::Configuration(
            "Usage: /goal [--budget <tokens>] <objective>".into(),
        ));
    }
    Ok((Some(budget), objective))
}

async fn handle_heartbeat_command(
    state_root: &Path,
    session: &str,
    arguments: Option<&str>,
) -> Result<String> {
    let store = ScheduleStore::new(state_root);
    let arguments = arguments.unwrap_or("status").trim();
    match arguments.to_ascii_lowercase().as_str() {
        "" | "status" => match store.get_heartbeat(session).await? {
            Some(heartbeat) => Ok(format_heartbeat(&heartbeat)),
            None => Ok("No heartbeat for this session.".into()),
        },
        "pause" => lifecycle_heartbeat(
            store.pause_heartbeat(session, chrono::Utc::now()).await?,
            "paused",
        ),
        "resume" => lifecycle_heartbeat(
            store.resume_heartbeat(session, chrono::Utc::now()).await?,
            "resumed",
        ),
        "clear" | "stop" => lifecycle_heartbeat(
            store.clear_heartbeat(session, chrono::Utc::now()).await?,
            "stopped",
        ),
        _ => {
            let (schedule, instruction, delivery_mode) = parse_heartbeat_create(arguments)?;
            let heartbeat = store
                .set_heartbeat(
                    "heartbeat",
                    session,
                    instruction,
                    &schedule,
                    delivery_mode,
                    chrono::Utc::now(),
                )
                .await?;
            Ok(format!("Heartbeat set: {}", format_heartbeat(&heartbeat)))
        }
    }
}

fn parse_heartbeat_create(
    arguments: &str,
) -> Result<(String, &str, Option<HeartbeatDeliveryMode>)> {
    let mut rest = arguments.trim();
    let mut delivery_mode = None;
    if let Some(value) = strip_command_option(rest, "--steer") {
        delivery_mode = Some(HeartbeatDeliveryMode::Steer);
        rest = value;
    } else if let Some(value) = strip_command_option(rest, "--follow-up") {
        delivery_mode = Some(HeartbeatDeliveryMode::FollowUp);
        rest = value;
    }
    let (schedule, mut instruction) = if let Some(value) = rest.strip_prefix("--every")
        && value.starts_with(char::is_whitespace)
    {
        heartbeat_schedule_and_instruction(value.trim_start())?
    } else if let Some(value) = rest.strip_prefix("every")
        && value.starts_with(char::is_whitespace)
    {
        heartbeat_schedule_and_instruction(value.trim_start())?
    } else {
        ("every 5m".into(), rest)
    };
    if let Some(value) = strip_command_option(instruction, "--steer") {
        delivery_mode = Some(HeartbeatDeliveryMode::Steer);
        instruction = value;
    } else if let Some(value) = strip_command_option(instruction, "--follow-up") {
        delivery_mode = Some(HeartbeatDeliveryMode::FollowUp);
        instruction = value;
    }
    if instruction.is_empty() {
        return Err(crate::error::MimirError::Configuration(
            "Usage: /heartbeat [--every <interval>] [--steer|--follow-up] <instruction>".into(),
        ));
    }
    Ok((schedule, instruction, delivery_mode))
}

fn strip_command_option<'a>(value: &'a str, option: &str) -> Option<&'a str> {
    let rest = value.strip_prefix(option)?;
    (rest.is_empty() || rest.starts_with(char::is_whitespace)).then(|| rest.trim_start())
}

fn heartbeat_schedule_and_instruction(value: &str) -> Result<(String, &str)> {
    let split = value.find(char::is_whitespace).ok_or_else(|| {
        crate::error::MimirError::Configuration(
            "Usage: /heartbeat [--every <interval>] [--steer|--follow-up] <instruction>".into(),
        )
    })?;
    let interval = &value[..split];
    Ok((format!("every {interval}"), value[split..].trim_start()))
}

fn lifecycle_heartbeat(heartbeat: Option<Schedule>, action: &str) -> Result<String> {
    heartbeat.map_or_else(
        || {
            Err(crate::error::MimirError::Configuration(
                "no active heartbeat for this session".into(),
            ))
        },
        |heartbeat| Ok(format!("Heartbeat {action}: {}", heartbeat.prompt)),
    )
}

fn format_heartbeat(heartbeat: &Schedule) -> String {
    format!(
        "{} [{}] {} ({})",
        heartbeat.prompt,
        heartbeat_status(heartbeat),
        heartbeat.schedule_expression,
        match heartbeat.delivery_mode.unwrap_or_default() {
            HeartbeatDeliveryMode::Steer => "steer",
            HeartbeatDeliveryMode::FollowUp => "follow-up",
        }
    )
}

fn heartbeat_status(heartbeat: &Schedule) -> &'static str {
    if heartbeat.paused {
        "paused"
    } else if heartbeat.cancelled || !heartbeat.enabled {
        "stopped"
    } else {
        "active"
    }
}

async fn open_heartbeat_manager(
    state_root: &Path,
    app: &Arc<Mutex<App>>,
) -> Result<Option<String>> {
    let mut heartbeats = ScheduleStore::new(state_root).list_heartbeats().await?;
    heartbeats.sort_by_key(|heartbeat| heartbeat.next_run);
    if heartbeats.is_empty() {
        return Ok(Some("No active heartbeats.".into()));
    }
    let options = heartbeats
        .iter()
        .map(|heartbeat| {
            let prompt = heartbeat
                .prompt
                .replace(['\r', '\n'], " ")
                .chars()
                .take(96)
                .collect::<String>();
            (
                heartbeat.id,
                heartbeat.session_id.clone(),
                heartbeat.paused,
                format!(
                    "{} [{}] {} — {prompt}",
                    heartbeat.session_id,
                    heartbeat_status(heartbeat),
                    heartbeat.schedule_expression
                ),
            )
        })
        .collect::<Vec<_>>();
    let count = options.len();
    app.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .open_heartbeat_selector(options);
    Ok(Some(format!("Manage {count} heartbeat(s)")))
}

async fn manage_tui_heartbeat(
    runtime: &AgentRuntime,
    state_root: &Path,
    session: &str,
    id: Uuid,
    action: HeartbeatManagementAction,
) -> Result<Option<String>> {
    let action_label = match action {
        HeartbeatManagementAction::Pause => "paused",
        HeartbeatManagementAction::Resume => "resumed",
        HeartbeatManagementAction::Stop => "stopped",
    };
    let updated = ScheduleStore::new(state_root)
        .manage_heartbeat(session, id, action, chrono::Utc::now())
        .await?
        .ok_or_else(|| {
            crate::error::MimirError::Configuration(
                "heartbeat changed before the selected action could be applied".into(),
            )
        })?;
    runtime.publish_session_event(serde_json::json!({"type": "heartbeats_changed"}))?;
    Ok(Some(format!(
        "Heartbeat {action_label}: {} ({})",
        updated.prompt, updated.session_id
    )))
}

async fn show_tui_session_info(
    runtime: &AgentRuntime,
    runtime_key: Option<&(String, String)>,
    state_root: &Path,
) -> Result<String> {
    let session = active_session(runtime_key)?;
    let messages = runtime.messages_snapshot().await;
    let mut user = 0_usize;
    let mut assistant = 0_usize;
    let mut tool_results = 0_usize;
    let mut tool_calls = 0_usize;
    for message in &messages {
        match message.role {
            Role::User => user = user.saturating_add(1),
            Role::Assistant => assistant = assistant.saturating_add(1),
            Role::Tool => tool_results = tool_results.saturating_add(1),
            Role::System => {}
        }
        tool_calls = tool_calls.saturating_add(
            message
                .content
                .iter()
                .filter(|content| matches!(content, Content::ToolCall(_)))
                .count(),
        );
    }
    let store = FileSessionStore::create(state_root, session).await?;
    Ok(format!(
        "Session Info\n\nFile: {}\nID: {session}\n\nMessages\nUser: {user}\nAssistant: {assistant}\nTool Calls: {tool_calls}\nTool Results: {tool_results}\nTotal: {}\n\nUse /context for token usage.",
        store.path().display(),
        messages.len()
    ))
}

const fn goal_status(status: GoalStatus) -> &'static str {
    match status {
        GoalStatus::Active => "active",
        GoalStatus::Paused => "paused",
        GoalStatus::Complete => "complete",
        GoalStatus::Blocked => "blocked",
    }
}

/// Exports the active TUI session as safe standalone HTML or reference-compatible JSONL.
///
/// # Errors
///
/// Returns a typed validation, persistence, or serialization error.
pub async fn export_tui_session(
    state_root: &Path,
    session: &str,
    requested_path: Option<&str>,
) -> Result<String> {
    let relative = requested_path.map_or_else(
        || PathBuf::from(format!("exports/{session}.html")),
        PathBuf::from,
    );
    let extension = relative.extension().and_then(std::ffi::OsStr::to_str);
    if relative.is_absolute()
        || relative
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
        || !matches!(extension, Some("html" | "jsonl"))
    {
        return Err(crate::error::MimirError::Configuration(
            "TUI export path must be a relative .html or .jsonl path inside the state directory"
                .into(),
        ));
    }
    let root = crate::atomic::canonical_state_root(state_root);
    let output = root.join(&relative);
    crate::atomic::prepare_state_path(&root, &output).await?;
    let store = FileSessionStore::create(&root, session).await?;
    let loaded = store.load().await?;
    let text = if extension == Some("html") {
        render_tui_session_html(&loaded.records)
    } else {
        let timestamp = loaded
            .records
            .first()
            .map_or_else(chrono::Utc::now, |record| record.created_at);
        export_jsonl(
            &loaded.records,
            &ReferenceSessionMetadata {
                session_id: session.into(),
                timestamp,
                cwd: root.clone(),
                parent_session: None,
            },
        )?
    };
    write_tui_export(&output, text.as_bytes()).await?;
    Ok(format!("Exported session to {}", output.display()))
}

/// Creates a local-only HTML share preview. This function never performs a
/// network request and makes the missing upload authority explicit.
///
/// # Errors
///
/// Returns validation, persistence, or serialization errors from the bounded
/// export path.
pub async fn preview_tui_share(state_root: &Path, session: &str) -> Result<String> {
    let file_name = safe_preview_file_name(session)?;
    let relative = format!("shares/{file_name}.html");
    export_tui_session(state_root, session, Some(&relative)).await?;
    let path = crate::atomic::canonical_state_root(state_root).join(relative);
    Ok(format!(
        "Local share preview: {}. Nothing was uploaded; remote sharing requires an explicitly configured credential and upload action.",
        path.display()
    ))
}

/// Writes a metadata-only local trace preview. Message text, credentials, tool
/// arguments, and event details are intentionally excluded.
///
/// # Errors
///
/// Returns typed validation or persistence errors. `upload` always fails closed
/// until an explicit credentialed transport is configured by the coordinator.
pub async fn preview_tui_traces(
    state_root: &Path,
    session: &str,
    arguments: Option<&str>,
) -> Result<String> {
    match arguments.map(str::trim).filter(|value| !value.is_empty()) {
        None | Some("preview" | "status") => {}
        Some("upload") => {
            return Err(crate::error::MimirError::Configuration(
                "trace upload requires an explicit credentialed transport; none is configured"
                    .into(),
            ));
        }
        Some(_) => {
            return Err(crate::error::MimirError::Configuration(
                "Usage: /traces [preview|status|upload]".into(),
            ));
        }
    }

    let records = FileSessionStore::create(state_root, session)
        .await?
        .load()
        .await?
        .records;
    let mut event_counts = BTreeMap::<String, usize>::new();
    let mut message_count = 0_usize;
    for record in &records {
        match &record.payload {
            SessionPayload::Message(_) => message_count = message_count.saturating_add(1),
            SessionPayload::RuntimeEvent { name, .. } => {
                let count = event_counts.entry(name.clone()).or_default();
                *count = count.saturating_add(1);
            }
            SessionPayload::Compaction { .. } => {}
        }
    }
    let preview = LocalTracePreview {
        schema_version: 1,
        session,
        record_count: records.len(),
        message_count,
        event_counts,
        diagnostic_run_ids: crate::diagnostics::list_runs(&crate::diagnostics::diagnostics_root(
            state_root,
        ))
        .unwrap_or_default()
        .into_iter()
        .filter(|run| run.session_id == session)
        .map(|run| run.run_id)
        .collect(),
        diagnostic_bundle_root: "$STATE/diagnostics".into(),
        contains_message_text: false,
        uploaded: false,
    };
    let bytes = serde_json::to_vec_pretty(&preview)?;
    let file_name = safe_preview_file_name(session)?;
    let root = crate::atomic::canonical_state_root(state_root);
    let output = root.join("traces").join(format!("{file_name}.json"));
    crate::atomic::prepare_state_path(&root, &output).await?;
    write_tui_export(&output, &bytes).await?;
    Ok(format!(
        "Local trace preview: {} ({} records, {message_count} messages; transcript text excluded). Nothing was uploaded.",
        output.display(),
        records.len()
    ))
}

fn safe_preview_file_name(session: &str) -> Result<&str> {
    let valid = !session.is_empty()
        && session.len() <= 128
        && session
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        && session != "."
        && session != "..";
    if valid {
        Ok(session)
    } else {
        Err(crate::error::MimirError::Configuration(
            "session identifier is unsafe for a local preview filename".into(),
        ))
    }
}

fn render_tui_session_html(records: &[SessionRecord]) -> String {
    let mut html = String::from(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>Mimir Session</title><style>body{font:15px system-ui;max-width:900px;margin:2rem auto;padding:0 1rem;color:#18181b}article{border:1px solid #d4d4d8;border-radius:8px;padding:1rem;margin:1rem 0}h2{font-size:.8rem;text-transform:uppercase;color:#52525b}pre{white-space:pre-wrap;overflow-wrap:anywhere}</style></head><body><h1>Mimir Session</h1>",
    );
    for message in records.iter().filter_map(|record| match &record.payload {
        SessionPayload::Message(message) => Some(message),
        _ => None,
    }) {
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

async fn write_tui_export(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Ok(metadata) = tokio::fs::symlink_metadata(path).await
        && (metadata.file_type().is_symlink() || !metadata.is_file())
    {
        return Err(crate::error::MimirError::Configuration(
            "export target must be a regular file and not a symlink".into(),
        ));
    }
    let parent = path.parent().ok_or_else(|| {
        crate::error::MimirError::Configuration("export path has no parent".into())
    })?;
    let temporary = parent.join(format!(".mimir-tui-{}.tmp", Uuid::new_v4()));
    let mut file = tokio::fs::File::create(&temporary).await?;
    tokio::io::AsyncWriteExt::write_all(&mut file, bytes).await?;
    file.sync_all().await?;
    drop(file);
    if let Err(error) = tokio::fs::rename(&temporary, path).await {
        let _ = tokio::fs::remove_file(&temporary).await;
        return Err(error.into());
    }
    Ok(())
}

async fn load_session_catalog(
    runtime_key: Option<&(String, String)>,
    state_root: &Path,
) -> Result<(String, SessionBranchCatalog)> {
    let session = active_session(runtime_key)?.to_owned();
    let records = FileSessionStore::create(state_root, &session)
        .await?
        .load()
        .await?
        .records;
    Ok((session, SessionBranchCatalog::from_records(records)?))
}

async fn open_tui_tree_selector(
    runtime_key: Option<&(String, String)>,
    app: &Arc<Mutex<App>>,
    state_root: &Path,
) -> Result<Option<String>> {
    let (session, catalog) = load_session_catalog(runtime_key, state_root).await?;
    if catalog.nodes().is_empty() {
        return Err(crate::error::MimirError::Configuration(format!(
            "session tree for {session} is empty"
        )));
    }
    let filter = app
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .tree_filter_mode();
    let options = catalog
        .nodes()
        .iter()
        .filter(|node| tree_node_visible(&node.kind, filter))
        .map(|node| {
            let preview = node
                .preview
                .as_deref()
                .unwrap_or("")
                .replace(['\r', '\n'], " ");
            (
                node.record_id,
                format!("{:?} {preview} [{}]", node.kind, node.record_id),
            )
        })
        .collect::<Vec<_>>();
    if options.is_empty() {
        return Err(crate::error::MimirError::Configuration(format!(
            "session tree for {session} has no entries matching the {} filter",
            filter.as_str()
        )));
    }
    let count = options.len();
    app.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .open_tree_selector(options);
    Ok(Some(format!(
        "Choose one of {count} entries to continue from in {session}"
    )))
}

fn tree_node_visible(kind: &SessionNodeKind, filter: TreeFilterMode) -> bool {
    match filter {
        TreeFilterMode::All => true,
        TreeFilterMode::Default => matches!(
            kind,
            SessionNodeKind::Message {
                role: Role::User | Role::Assistant
            } | SessionNodeKind::Compaction
        ),
        TreeFilterMode::NoTools => match kind {
            SessionNodeKind::Message { role: Role::Tool } => false,
            SessionNodeKind::RuntimeEvent { name } if name.starts_with("tool_") => false,
            _ => true,
        },
        TreeFilterMode::UserOnly => {
            matches!(kind, SessionNodeKind::Message { role: Role::User })
        }
        TreeFilterMode::LabeledOnly => matches!(
            kind,
            SessionNodeKind::RuntimeEvent { name } if name == "session_entry_label"
        ),
    }
}

async fn continue_tui_session(
    runtime: &mut Arc<AgentRuntime>,
    runtime_factory: &dyn TuiRuntimeFactory,
    runtime_key: &mut Option<(String, String)>,
    app: &Arc<Mutex<App>>,
    state_root: &Path,
    session_root: &Path,
    entry_id: Uuid,
) -> Result<Option<String>> {
    let (source_session, catalog) =
        load_session_catalog(runtime_key.as_ref(), session_root).await?;
    runtime.before_session_tree(&entry_id.to_string()).await?;
    let derivation = catalog.clone_at(entry_id, &source_session)?;
    let session = format!("session-{}", Uuid::new_v4().simple());
    let destination = FileSessionStore::create(session_root, &session).await?;
    for record in derivation.records {
        destination.append(record).await?;
    }
    let model = current_model(runtime, runtime_key.as_ref()).await;
    let next_runtime =
        build_runtime_with_preferences(runtime_factory, &model, &session, state_root).await?;
    runtime.complete_session_tree(&entry_id.to_string()).await?;
    runtime.shutdown_extension_session("session_tree").await?;
    next_runtime
        .start_extension_session(
            SessionStartReason::Fork,
            Some(
                FileSessionStore::create(session_root, &source_session)
                    .await?
                    .path()
                    .display()
                    .to_string(),
            ),
        )
        .await?;
    *runtime = next_runtime;
    *runtime_key = Some((model, session.clone()));
    let mut state = app
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    state.add_session(&session);
    state.select_session(&session);
    Ok(Some(format!(
        "Continuing {source_session} from {entry_id} as {session}"
    )))
}

async fn open_tui_fork_selector(
    runtime_key: Option<&(String, String)>,
    app: &Arc<Mutex<App>>,
    state_root: &Path,
) -> Result<Option<String>> {
    let (_, catalog) = load_session_catalog(runtime_key, state_root).await?;
    let options = catalog
        .user_message_selectors()
        .into_iter()
        .map(|entry| (entry.entry_id, entry.text))
        .collect::<Vec<_>>();
    if options.is_empty() {
        return Err(crate::error::MimirError::Configuration(
            "session has no user message to fork from".into(),
        ));
    }
    let count = options.len();
    app.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .open_fork_selector(options);
    Ok(Some(format!("Choose one of {count} user messages to fork")))
}

async fn fork_tui_session(
    runtime: &mut Arc<AgentRuntime>,
    runtime_factory: &dyn TuiRuntimeFactory,
    runtime_key: &mut Option<(String, String)>,
    app: &Arc<Mutex<App>>,
    state_root: &Path,
    session_root: &Path,
    entry_id: Uuid,
) -> Result<Option<String>> {
    let (source_session, catalog) =
        load_session_catalog(runtime_key.as_ref(), session_root).await?;
    runtime
        .before_session_fork(&entry_id.to_string(), SessionForkPosition::Before)
        .await?;
    let derivation = catalog.fork_before_user_message(entry_id, &source_session)?;
    let session = format!("session-{}", Uuid::new_v4().simple());
    let destination = FileSessionStore::create(session_root, &session).await?;
    for record in derivation.records {
        destination.append(record).await?;
    }
    let model = current_model(runtime, runtime_key.as_ref()).await;
    let next_runtime =
        build_runtime_with_preferences(runtime_factory, &model, &session, state_root).await?;
    runtime.shutdown_extension_session("session_fork").await?;
    next_runtime
        .start_extension_session(
            SessionStartReason::Fork,
            Some(
                FileSessionStore::create(session_root, &source_session)
                    .await?
                    .path()
                    .display()
                    .to_string(),
            ),
        )
        .await?;
    *runtime = next_runtime;
    *runtime_key = Some((model, session.clone()));
    let mut state = app
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    state.add_session(&session);
    state.select_session(&session);
    if let Some(selected_text) = derivation.selected_text {
        state.set_prompt(selected_text);
    }
    Ok(Some(format!("Forked {source_session} as {session}")))
}

async fn handle_tui_mcp(
    state_root: &Path,
    app: &Arc<Mutex<App>>,
    command: &super::McpCommand,
) -> Result<Option<String>> {
    let coordinator = McpAuthCoordinator::global(state_root)?;
    match command {
        super::McpCommand::List => {
            let statuses = coordinator.list_statuses().await?;
            if statuses.is_empty() {
                return Ok(Some("No MCP servers configured".into()));
            }
            let lines = statuses
                .into_iter()
                .map(|status| {
                    let connection = if status.auth.enabled {
                        "connected"
                    } else {
                        "not connected"
                    };
                    format!("{} ({}) - {connection}", status.label, status.server)
                })
                .collect::<Vec<_>>();
            Ok(Some(format!("MCP integrations:\n{}", lines.join("\n"))))
        }
        super::McpCommand::Logout { server } => {
            let removed = coordinator.logout(server).await?;
            Ok(Some(if removed {
                format!("Disconnected MCP server {server}")
            } else {
                format!("MCP server {server} was not connected")
            }))
        }
        super::McpCommand::Login { server } => {
            let status = coordinator.status(server).await?.ok_or_else(|| {
                crate::error::MimirError::Configuration(format!("unknown MCP server: {server}"))
            })?;
            if status.auth.enabled {
                return Ok(Some(format!("MCP server {server} is already connected")));
            }
            if status.oauth {
                let entry = coordinator.catalog().get(server).await?.ok_or_else(|| {
                    crate::error::MimirError::Configuration(format!("unknown MCP server: {server}"))
                })?;
                let config = entry.to_runtime_config()?;
                let remote = config.remote.as_ref().ok_or_else(|| {
                    crate::error::MimirError::Configuration(format!(
                        "MCP server {server} advertises OAuth without a remote HTTP transport"
                    ))
                })?;
                let client = McpOAuthClient::new(remote.io_timeout, remote.max_response_bytes)?;
                let bundle = client
                    .authorize_with_headers(
                        remote,
                        &config.headers,
                        None,
                        "http://127.0.0.1:8765/callback",
                        &TerminalMcpOAuthReceiver,
                    )
                    .await?;
                coordinator.store_oauth_bundle(server, bundle).await?;
                return Ok(Some(format!("Connected MCP server {server} with OAuth")));
            }
            app.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .open_mcp_login(server);
            Ok(Some(format!("Enter the API key for MCP server {server}")))
        }
    }
}

async fn resume_tui_session(
    runtime: &mut Arc<AgentRuntime>,
    runtime_factory: &dyn TuiRuntimeFactory,
    runtime_key: &mut Option<(String, String)>,
    app: &Arc<Mutex<App>>,
    state_root: &Path,
    session_root: &Path,
    session: &str,
) -> Result<Option<String>> {
    if !FileSessionStore::list_ids(session_root)
        .await?
        .iter()
        .any(|candidate| candidate == session)
    {
        return Err(crate::error::MimirError::Configuration(format!(
            "session does not exist: {session}"
        )));
    }
    let model = current_model(runtime, runtime_key.as_ref()).await;
    let target = FileSessionStore::create(session_root, session)
        .await?
        .path()
        .display()
        .to_string();
    let previous = runtime_key
        .as_ref()
        .map(|(_, current)| current.clone())
        .unwrap_or_default();
    let next_runtime =
        build_runtime_with_preferences(runtime_factory, &model, session, state_root).await?;
    runtime
        .before_session_switch(SessionSwitchReason::Resume, Some(target))
        .await?;
    runtime.shutdown_extension_session("session_switch").await?;
    next_runtime
        .start_extension_session(
            SessionStartReason::Resume,
            (!previous.is_empty()).then(|| {
                session_root
                    .join("sessions")
                    .join(format!("{previous}.jsonl"))
                    .display()
                    .to_string()
            }),
        )
        .await?;
    *runtime = next_runtime;
    *runtime_key = Some((model, session.into()));
    let mut state = app
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    state.add_session(session);
    state.select_session(session);
    Ok(Some(format!("Resumed session {session}")))
}

async fn start_tui_session(
    runtime: &mut Arc<AgentRuntime>,
    runtime_factory: &dyn TuiRuntimeFactory,
    runtime_key: &mut Option<(String, String)>,
    app: &Arc<Mutex<App>>,
    state_root: &Path,
    session_root: &Path,
    details: (Option<&str>, Option<&str>),
) -> Result<Option<String>> {
    let (name, prompt) = details;
    let previous = runtime_key.as_ref().map(|key| key.1.clone());
    let session = format!("session-{}", Uuid::new_v4().simple());
    let model = current_model(runtime, runtime_key.as_ref()).await;
    runtime
        .before_session_switch(
            SessionSwitchReason::New,
            Some(
                session_root
                    .join("sessions")
                    .join(format!("{session}.jsonl"))
                    .display()
                    .to_string(),
            ),
        )
        .await?;
    let next_runtime =
        build_runtime_with_preferences(runtime_factory, &model, &session, state_root).await?;
    let store = FileSessionStore::create(session_root, &session).await?;
    store
        .append(SessionRecord::new(SessionPayload::RuntimeEvent {
            name: "session_created".into(),
            detail: previous.clone().unwrap_or_default(),
        }))
        .await?;
    if let Some(name) = name {
        persist_session_name(&store, name).await?;
    }
    runtime.shutdown_extension_session("new_session").await?;
    next_runtime
        .start_extension_session(
            SessionStartReason::New,
            previous.map(|previous| {
                session_root
                    .join("sessions")
                    .join(format!("{previous}.jsonl"))
                    .display()
                    .to_string()
            }),
        )
        .await?;
    *runtime = next_runtime;
    *runtime_key = Some((model, session.clone()));
    {
        let mut state = app
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.add_session(&session);
        state.select_session(&session);
        if let Some(prompt) = prompt {
            state.push_user_message(prompt);
        }
    }
    if let Some(prompt) = prompt {
        let runtime = runtime.clone();
        let sink = TuiSink { app: app.clone() };
        let prompt = prompt.to_owned();
        tokio::spawn(async move {
            let _ = runtime.run(&prompt, &sink).await;
        });
    }
    Ok(Some(format!("Started new session {session}")))
}

async fn update_tui_session_name(
    runtime_key: Option<&(String, String)>,
    state_root: &Path,
    name: Option<&str>,
) -> Result<Option<String>> {
    let Some((_, session)) = runtime_key else {
        return Err(crate::error::MimirError::Configuration(
            "no active session".into(),
        ));
    };
    let store = FileSessionStore::create(state_root, session).await?;
    if let Some(name) = name {
        persist_session_name(&store, name).await?;
        Ok(Some(format!("Session named {name}")))
    } else {
        let name = load_session_name(&store)
            .await?
            .unwrap_or_else(|| "unnamed".into());
        Ok(Some(format!("Session name: {name}")))
    }
}

async fn clone_tui_session(
    runtime: &mut Arc<AgentRuntime>,
    runtime_factory: &dyn TuiRuntimeFactory,
    runtime_key: &mut Option<(String, String)>,
    app: &Arc<Mutex<App>>,
    state_root: &Path,
    session_root: &Path,
) -> Result<Option<String>> {
    let Some((model, source_session)) = runtime_key.as_ref() else {
        return Err(crate::error::MimirError::Configuration(
            "no active session".into(),
        ));
    };
    let model = model.clone();
    let source_session = source_session.clone();
    let source = FileSessionStore::create(session_root, &source_session).await?;
    let catalog = SessionBranchCatalog::from_records(source.load().await?.records)?;
    let derivation = catalog.clone_active(&source_session)?;
    let session = format!("session-{}", Uuid::new_v4().simple());
    runtime
        .before_session_switch(
            SessionSwitchReason::New,
            Some(
                session_root
                    .join("sessions")
                    .join(format!("{session}.jsonl"))
                    .display()
                    .to_string(),
            ),
        )
        .await?;
    let destination = FileSessionStore::create(session_root, &session).await?;
    for record in derivation.records {
        destination.append(record).await?;
    }
    let next_runtime =
        build_runtime_with_preferences(runtime_factory, &model, &session, state_root).await?;
    runtime.shutdown_extension_session("session_clone").await?;
    next_runtime
        .start_extension_session(
            SessionStartReason::Fork,
            Some(source.path().display().to_string()),
        )
        .await?;
    *runtime = next_runtime;
    *runtime_key = Some((model, session.clone()));
    let mut state = app
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    state.add_session(&session);
    state.select_session(&session);
    Ok(Some(format!("Cloned session as {session}")))
}

async fn reload_tui_runtime(
    runtime: &mut Arc<AgentRuntime>,
    runtime_factory: &dyn TuiRuntimeFactory,
    runtime_key: Option<&(String, String)>,
    app: &Arc<Mutex<App>>,
    state_root: &Path,
    session_root: &Path,
) -> Result<Option<String>> {
    let Some((model, session)) = runtime_key else {
        return Err(crate::error::MimirError::Configuration(
            "no active session".into(),
        ));
    };
    let next_runtime =
        build_runtime_with_preferences(runtime_factory, model, session, state_root).await?;
    runtime.shutdown_extension_session("reload").await?;
    next_runtime
        .start_extension_session(
            SessionStartReason::Reload,
            Some(
                session_root
                    .join("sessions")
                    .join(format!("{session}.jsonl"))
                    .display()
                    .to_string(),
            ),
        )
        .await?;
    *runtime = next_runtime;
    let resources = runtime_factory.resource_snapshot().await?;
    app.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .set_resource_snapshot(resources);
    refresh_extension_commands(runtime, app).await;
    Ok(Some("Reloaded runtime resources".into()))
}

async fn build_runtime_with_preferences(
    runtime_factory: &dyn TuiRuntimeFactory,
    model: &str,
    session: &str,
    state_root: &Path,
) -> Result<Arc<AgentRuntime>> {
    let runtime = runtime_factory.build(model, session).await?;
    let preferences = load_tui_preferences(state_root).await?;
    apply_runtime_preferences(&runtime, &preferences).await;
    Ok(runtime)
}

async fn current_model(runtime: &AgentRuntime, runtime_key: Option<&(String, String)>) -> String {
    if let Some((model, _)) = runtime_key {
        model.clone()
    } else {
        let (provider, model, _) = runtime.model_selection().await;
        format!("{provider}/{model}")
    }
}

async fn persist_session_name(store: &FileSessionStore, name: &str) -> Result<()> {
    let name = name.trim();
    if name.is_empty() {
        return Err(crate::error::MimirError::Configuration(
            "session name must not be blank".into(),
        ));
    }
    store
        .append(SessionRecord::new(SessionPayload::RuntimeEvent {
            name: "session_name".into(),
            detail: name.into(),
        }))
        .await
}

async fn load_session_name(store: &FileSessionStore) -> Result<Option<String>> {
    Ok(store.load().await?.records.iter().rev().find_map(|record| {
        if let SessionPayload::RuntimeEvent { name, detail } = &record.payload
            && name == "session_name"
        {
            return Some(detail.clone());
        }
        None
    }))
}

/// Applies TUI actions that can be fulfilled by the active runtime itself.
///
/// Returns `Ok(None)` for coordinator-owned actions so the caller can route them through
/// [`TuiRuntimeFactory::handle_tui_action`].
///
/// # Errors
///
/// Returns a typed runtime/provider/persistence error when effort selection or compaction fails.
pub async fn dispatch_runtime_action(
    runtime: &AgentRuntime,
    action: &TuiAction,
) -> Result<Option<String>> {
    match action {
        TuiAction::SetEffort(level) => {
            let selected = runtime.set_thinking_level(*level).await?;
            Ok(Some(format!(
                "Reasoning effort set to {}",
                selected.as_str()
            )))
        }
        TuiAction::SetAutoCompaction { enabled } => {
            runtime.set_auto_compaction(*enabled);
            Ok(Some(format!(
                "Auto-compaction {}",
                if *enabled { "enabled" } else { "disabled" }
            )))
        }
        TuiAction::SetSteeringMode { mode } => {
            runtime.set_steering_mode(*mode).await;
            Ok(Some(format!("Steering mode set to {}", mode.as_str())))
        }
        TuiAction::Compact { instructions } => {
            let result = runtime.compact(instructions.as_deref()).await?;
            Ok(Some(format!(
                "Context compacted: {} tokens summarized",
                result.tokens_before
            )))
        }
        TuiAction::ShowContext => render_context_tree(runtime).await.map(Some),
        TuiAction::CopyLastMessage => copy_last_assistant_message(runtime).await.map(Some),
        TuiAction::SetAgentMode(_)
        | TuiAction::ImplementPlan { .. }
        | TuiAction::WorkspacePermission { .. }
        | TuiAction::Resume { .. }
        | TuiAction::NewSession { .. }
        | TuiAction::SetSessionName { .. }
        | TuiAction::ShowSessionInfo
        | TuiAction::ShowSessionTree
        | TuiAction::ContinueAt { .. }
        | TuiAction::Fork
        | TuiAction::ForkAt { .. }
        | TuiAction::Clone
        | TuiAction::Reload
        | TuiAction::Mcp(_)
        | TuiAction::Refine { .. }
        | TuiAction::Learn { .. }
        | TuiAction::SideQuestion { .. }
        | TuiAction::ExportSession { .. }
        | TuiAction::ImportSession { .. }
        | TuiAction::ShareSession
        | TuiAction::ShowChangelog
        | TuiAction::ShowSystemPrompt
        | TuiAction::ShowLogs
        | TuiAction::Update { .. }
        | TuiAction::RlmMaxDepth { .. }
        | TuiAction::ToggleFast
        | TuiAction::ConfigureScopedModels
        | TuiAction::Fullscreen { .. }
        | TuiAction::SetTheme
        | TuiAction::PersistSettings
        | TuiAction::Traces { .. }
        | TuiAction::Heartbeat { .. }
        | TuiAction::ListHeartbeats
        | TuiAction::ManageHeartbeat { .. }
        | TuiAction::Goal { .. }
        | TuiAction::Autonomous { .. }
        | TuiAction::ExtensionCommand { .. } => Ok(None),
    }
}

async fn render_context_tree(runtime: &AgentRuntime) -> Result<String> {
    let messages = runtime.messages_snapshot().await;
    let children = runtime.context_children(64).await?;
    let (provider, model, _) = runtime.model_selection().await;
    let usage = messages
        .iter()
        .fold(Usage::default(), |usage, message| Usage {
            input_tokens: usage
                .input_tokens
                .saturating_add(message.usage.input_tokens),
            output_tokens: usage
                .output_tokens
                .saturating_add(message.usage.output_tokens),
            cached_tokens: usage
                .cached_tokens
                .saturating_add(message.usage.cached_tokens),
            cache_write_tokens: usage
                .cache_write_tokens
                .saturating_add(message.usage.cache_write_tokens),
        });
    let input = usage.input_tokens;
    let output = usage.output_tokens;
    let cached = usage.cached_tokens;
    let cache_write = usage.cache_write_tokens;
    let fresh_input = usage.uncached_input_tokens();
    let raw_total = usage.total();
    let operational_total = usage.budget_tokens();
    let child_output = children
        .iter()
        .map(|child| child.output_tokens)
        .fold(0_u64, u64::saturating_add);
    let mut tree = format!(
        "Context Tree\n└─ main agent [active] {provider}/{model}\n   Messages: {}\n   Own/total usage: input {input}, cache read {cached}, cache write {cache_write}, fresh input {fresh_input}, output {output}\n   Raw total: {raw_total}; operational budget: {operational_total}\n   Tree output usage: {}\n   Cost: unavailable (provider pricing is not exposed by the native runtime)",
        messages.len(),
        output.saturating_add(child_output),
    );
    if children.is_empty() {
        tree.push_str("\n   Children: none");
        return Ok(tree);
    }
    write!(tree, "\n   Children: {}", children.len()).expect("writing to String cannot fail");
    for (index, child) in children.iter().take(32).enumerate() {
        let branch = if index + 1 == children.len().min(32) {
            "└─"
        } else {
            "├─"
        };
        write!(
            tree,
            "\n   {branch} {} [{:?}] {} depth={} output={}",
            bounded_context_label(&child.session_name),
            child.status,
            bounded_context_label(&child.model),
            child.depth,
            child.output_tokens,
        )
        .expect("writing to String cannot fail");
    }
    if children.len() > 32 {
        write!(tree, "\n   … {} more children", children.len() - 32)
            .expect("writing to String cannot fail");
    }
    Ok(tree)
}

fn bounded_context_label(value: &str) -> String {
    const MAX_CHARS: usize = 80;
    let mut label = value
        .chars()
        .filter(|character| !character.is_control())
        .take(MAX_CHARS + 1)
        .collect::<String>();
    if label.chars().count() > MAX_CHARS {
        label = label.chars().take(MAX_CHARS).collect();
        label.push('…');
    }
    label
}

/// Applies a typed credential request emitted by a masked TUI overlay.
///
/// # Errors
///
/// Returns configuration, authentication, persistence, or OAuth transport errors.
pub async fn handle_ui_request(state_root: &Path, request: UiRequest) -> Result<String> {
    handle_ui_request_with_auth_store(state_root, request, AuthStore::global()?).await
}

/// Applies a typed credential request with an explicit auth store for isolated embedding and tests.
///
/// # Errors
///
/// Returns configuration, authentication, persistence, or OAuth transport errors.
pub async fn handle_ui_request_with_auth_store(
    state_root: &Path,
    request: UiRequest,
    store: AuthStore,
) -> Result<String> {
    match request {
        UiRequest::Logout { provider } => {
            let removed = store.logout(&provider).await?;
            Ok(if removed {
                format!("Logged out of {provider}")
            } else {
                format!("No stored credential for {provider}")
            })
        }
        UiRequest::McpApiKey { server, api_key } => {
            McpAuthCoordinator::with_auth_store(state_root, store.clone())?
                .store_api_key(&server, &api_key)
                .await?;
            Ok(format!("Connected MCP server {server}"))
        }
        UiRequest::Login { provider, secret } => {
            let registry = ProviderRegistry::builtin();
            let definition = registry.get(&provider).ok_or_else(|| {
                crate::error::MimirError::Configuration(format!("unknown provider: {provider}"))
            })?;
            if let Some(secret) = secret {
                if !definition.auth.contains(&AuthKind::ApiKey) {
                    return Err(crate::error::MimirError::Configuration(format!(
                        "{provider} does not accept API-key login"
                    )));
                }
                store.set_api_key(&provider, &secret).await?;
                return Ok(format!("Stored API credential for {provider}"));
            }
            let oauth_provider = OAuthProvider::from_id(&provider).ok_or_else(|| {
                crate::error::MimirError::Configuration(format!("{provider} requires an API key"))
            })?;
            TerminalGuard::suspend()?;
            let login_result = oauth_login(oauth_provider).await;
            let resume_result = TerminalGuard::resume();
            let credential = login_result?;
            resume_result?;
            store.set_oauth(&provider, credential).await?;
            Ok(format!("OAuth login completed for {provider}"))
        }
    }
}

async fn oauth_login(provider: OAuthProvider) -> Result<crate::auth::OAuthCredential> {
    if provider == OAuthProvider::GitHubCopilot {
        let device = DeviceAuthorization::begin_github("github.com").await?;
        println!("Open: {}", device.verification_uri);
        println!("Enter code: {}", device.user_code);
        io::stdout().flush()?;
        return device.poll_github("github.com").await;
    }
    let pending = PendingOAuth::begin(provider)?;
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
    pending.exchange(&input).await
}

fn render_frame(app: &Arc<Mutex<App>>, frame_cache: &mut TerminalFrameCache) -> io::Result<()> {
    let (width, height) = size().unwrap_or((80, 24));
    let (body, show_cursor, cursor_x) = {
        let state = app
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let body = state.render(
            TerminalSize {
                width: usize::from(width),
                height: usize::from(height),
            },
            RenderOptions {
                capabilities: TerminalCapabilities {
                    ansi: true,
                    alternate_screen: false,
                    cursor_addressing: false,
                    color: true,
                    unicode: true,
                },
            },
        );
        let cursor_x = prompt_cursor_x(
            usize::from(width),
            usize::from(state.editor_padding_x()),
            state.prompt(),
            state.cursor_chars(),
        );
        (body, state.show_hardware_cursor(), cursor_x)
    };
    let cursor_y = body
        .lines()
        .count()
        .saturating_sub(1)
        .min(usize::from(height.saturating_sub(1)));
    let cursor = show_cursor.then(|| {
        (
            u16::try_from(cursor_x).unwrap_or(width.saturating_sub(1)),
            u16::try_from(cursor_y).unwrap_or(height.saturating_sub(1)),
        )
    });
    let damage = frame_cache.frame_damage(width, height, &body, cursor);
    let mut stdout = io::stdout();
    match damage {
        TerminalFrameDamage::Unchanged => return Ok(()),
        TerminalFrameDamage::Full => {
            queue!(stdout, MoveTo(0, 0), Clear(ClearType::All))?;
            stdout.write_all(body.as_bytes())?;
        }
        TerminalFrameDamage::Rows(rows) => {
            let body_rows = body.split("\r\n").collect::<Vec<_>>();
            for row in rows {
                queue!(stdout, MoveTo(0, row), Clear(ClearType::CurrentLine))?;
                if let Some(line) = body_rows.get(usize::from(row)) {
                    stdout.write_all(line.as_bytes())?;
                }
            }
        }
    }
    if let Some((cursor_x, cursor_y)) = cursor {
        queue!(stdout, MoveTo(cursor_x, cursor_y), Show)?;
    } else {
        queue!(stdout, Hide)?;
    }
    stdout.flush()
}

fn prompt_cursor_x(width: usize, padding: usize, prompt: &str, cursor_chars: usize) -> usize {
    let cursor_chars = cursor_chars.min(prompt.chars().count());
    let before_cursor = prompt.chars().take(cursor_chars).collect::<String>();
    let column = before_cursor
        .rsplit('\n')
        .next()
        .map_or(0, |line| line.chars().count());
    padding
        .saturating_add("❯ ".chars().count())
        .saturating_add(column)
        % width.max(1)
}

fn is_shift_modifier(key: &crossterm::event::KeyEvent) -> bool {
    matches!(
        key.code,
        CrosstermKeyCode::Modifier(ModifierKeyCode::LeftShift | ModifierKeyCode::RightShift)
    )
}

fn convert_key(key: crossterm::event::KeyEvent) -> Option<KeyEvent> {
    let code = match key.code {
        CrosstermKeyCode::Char(value) => KeyCode::Char(value),
        CrosstermKeyCode::Enter => KeyCode::Enter,
        CrosstermKeyCode::Backspace => KeyCode::Backspace,
        CrosstermKeyCode::Delete => KeyCode::Delete,
        CrosstermKeyCode::Left => KeyCode::Left,
        CrosstermKeyCode::Right => KeyCode::Right,
        CrosstermKeyCode::Up => KeyCode::Up,
        CrosstermKeyCode::Down => KeyCode::Down,
        CrosstermKeyCode::Esc => KeyCode::Esc,
        CrosstermKeyCode::Tab | CrosstermKeyCode::BackTab => KeyCode::Tab,
        CrosstermKeyCode::F(value) => KeyCode::F(value),
        _ => return None,
    };
    Some(KeyEvent {
        code,
        ctrl: key.modifiers.contains(KeyModifiers::CONTROL),
        alt: key.modifiers.contains(KeyModifiers::ALT),
        shift: key.modifiers.contains(KeyModifiers::SHIFT),
    })
}

#[cfg(test)]
mod local_command_tests {
    use std::sync::{
        Arc, RwLock,
        atomic::{AtomicBool, Ordering},
    };

    use serde_json::json;
    use tempfile::TempDir;

    use super::*;
    use crate::{
        model::{Content, Message, ModelResponse, StopReason, ToolCall},
        provider::FakeProvider,
        runtime::RuntimeConfig,
        session::InMemorySessionStore,
        tools::{AgentMode, PlanContextStore, ToolPolicy, ToolRegistry},
    };

    struct TestFactory {
        state: PathBuf,
    }

    struct PlanTransitionFactory {
        workspace: PathBuf,
        state: PathBuf,
        mode: RwLock<AgentMode>,
        fail_build: AtomicBool,
    }

    #[async_trait]
    impl TuiRuntimeFactory for PlanTransitionFactory {
        async fn build(&self, model: &str, session: &str) -> Result<Arc<AgentRuntime>> {
            if self.fail_build.load(Ordering::SeqCst) {
                return Err(crate::error::MimirError::Configuration(
                    "injected rebuild failure".into(),
                ));
            }
            let mode = self.agent_mode();
            let plan_context = if mode.is_plan() {
                let context = Arc::new(
                    PlanContextStore::new(&self.workspace, &self.state, session)
                        .map_err(|error| crate::error::MimirError::Tool(error.to_string()))?,
                );
                context
                    .prepare()
                    .await
                    .map_err(|error| crate::error::MimirError::Tool(error.to_string()))?;
                Some(context)
            } else {
                None
            };
            let tools = ToolRegistry::with_default_tools(
                &self.workspace,
                ToolPolicy {
                    agent_mode: mode,
                    plan_context,
                    ..ToolPolicy::default()
                },
            )
            .map_err(|error| crate::error::MimirError::Configuration(error.to_string()))?;
            AgentRuntime::resume(
                Arc::new(FakeProvider::default()),
                Arc::new(tools),
                Arc::new(InMemorySessionStore::default()),
                RuntimeConfig::default_for_model(model),
            )
            .await
            .map(Arc::new)
        }

        fn agent_mode(&self) -> AgentMode {
            *self
                .mode
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        }

        fn set_agent_mode(&self, mode: AgentMode) -> Result<()> {
            *self
                .mode
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = mode;
            Ok(())
        }
    }

    #[async_trait]
    impl TuiRuntimeFactory for TestFactory {
        async fn build(&self, model: &str, session: &str) -> Result<Arc<AgentRuntime>> {
            let tools = ToolRegistry::with_default_tools(&self.state, ToolPolicy::default())
                .map_err(|error| crate::error::MimirError::Configuration(error.to_string()))?;
            let store = FileSessionStore::create(&self.state, session).await?;
            AgentRuntime::resume(
                Arc::new(FakeProvider::default()),
                Arc::new(tools),
                Arc::new(store),
                RuntimeConfig::default_for_model(model),
            )
            .await
            .map(Arc::new)
        }
    }

    #[tokio::test]
    async fn plan_handoff_requires_an_artifact_rolls_back_and_then_rebuilds_auto() {
        let workspace = TempDir::new().expect("workspace");
        let state = TempDir::new().expect("state");
        let session = "plan-session";
        let context = Arc::new(
            PlanContextStore::new(workspace.path(), state.path(), session).expect("context"),
        );
        context.prepare().await.expect("prepare");
        let tools = Arc::new(
            ToolRegistry::with_default_tools(
                workspace.path(),
                ToolPolicy {
                    agent_mode: AgentMode::Plan,
                    plan_context: Some(Arc::clone(&context)),
                    ..ToolPolicy::default()
                },
            )
            .expect("plan tools"),
        );
        let runtime = AgentRuntime::resume(
            Arc::new(FakeProvider::default()),
            Arc::clone(&tools),
            Arc::new(InMemorySessionStore::default()),
            RuntimeConfig::default_for_model("fake/model"),
        )
        .await
        .expect("plan runtime");
        let factory = PlanTransitionFactory {
            workspace: workspace.path().to_path_buf(),
            state: state.path().to_path_buf(),
            mode: RwLock::new(AgentMode::Plan),
            fail_build: AtomicBool::new(false),
        };
        let key = ("fake/model".into(), session.into());

        let Err(missing) = rebuild_plan_as_auto(&runtime, &factory, Some(&key), state.path()).await
        else {
            panic!("missing plan was accepted");
        };
        assert!(missing.to_string().contains("no non-empty plan"));
        assert_eq!(factory.agent_mode(), AgentMode::Plan);

        tools
            .execute(
                "write_plan",
                json!({
                    "title":"Transition",
                    "markdown":"# Transition\n\n## Summary\nReady.\n\n## Implementation Changes\nChange it.\n\n## Public Interfaces\nNone.\n\n## Tests\nRun them.\n\n## Assumptions\nNone.\n"
                }),
            )
            .await
            .expect("plan artifact");
        factory.fail_build.store(true, Ordering::SeqCst);
        let Err(failed) = rebuild_plan_as_auto(&runtime, &factory, Some(&key), state.path()).await
        else {
            panic!("injected rebuild failure was ignored");
        };
        assert!(failed.to_string().contains("injected rebuild failure"));
        assert_eq!(factory.agent_mode(), AgentMode::Plan);
        assert!(
            runtime
                .plan_artifact()
                .await
                .expect("artifact retained")
                .is_some()
        );

        factory.fail_build.store(false, Ordering::SeqCst);
        let prepared = rebuild_plan_as_auto(&runtime, &factory, Some(&key), state.path())
            .await
            .expect("successful handoff");
        assert_eq!(factory.agent_mode(), AgentMode::Auto);
        assert_eq!(prepared.relative_plan, "plans/transition.md");
        assert!(
            prepared
                .runtime
                .plan_artifact()
                .await
                .expect("auto context")
                .is_none()
        );
        assert_eq!(
            implementation_prompt(&prepared.relative_plan, Some("keep the API stable")),
            "Implement the accepted plan at $WORKSPACE/plans/transition.md. Follow it exactly and verify the result. Additional instructions: keep the API stable"
        );

        let reset =
            PlanContextStore::new(workspace.path(), state.path(), session).expect("reset context");
        reset.prepare().await.expect("prepare after handoff");
        assert!(
            reset
                .validated_artifact()
                .await
                .expect("reset state")
                .is_none()
        );
    }

    #[test]
    fn successful_auth_refreshes_runtime_without_forgetting_active_session() {
        let runtime_key = Some(("fake/model".into(), "active".into()));
        let mut runtime_needs_refresh = false;

        mark_runtime_credentials_stale(&Ok("stored credential".into()), &mut runtime_needs_refresh);

        assert!(runtime_needs_refresh);
        assert_eq!(
            active_session(runtime_key.as_ref()).expect("active session"),
            "active"
        );
    }

    #[test]
    fn hardware_cursor_column_tracks_wrapped_prompt_text() {
        assert_eq!(prompt_cursor_x(10, 3, "", 0), 5);
        assert_eq!(prompt_cursor_x(10, 3, "012345678901234", 15), 0);
        assert_eq!(prompt_cursor_x(10, 3, "first\n", 6), 5);
        assert_eq!(prompt_cursor_x(0, 3, "012345678901234", 15), 0);
    }

    #[test]
    fn recognizes_both_physical_shift_keys() {
        let left = crossterm::event::KeyEvent::new(
            CrosstermKeyCode::Modifier(ModifierKeyCode::LeftShift),
            KeyModifiers::NONE,
        );
        let right = crossterm::event::KeyEvent::new(
            CrosstermKeyCode::Modifier(ModifierKeyCode::RightShift),
            KeyModifiers::NONE,
        );

        assert!(is_shift_modifier(&left));
        assert!(is_shift_modifier(&right));
    }

    #[test]
    fn fresh_tui_preferences_enable_the_hardware_cursor() {
        assert!(TuiPreferences::default().show_hardware_cursor);
    }

    #[test]
    fn ctrl_c_cancels_once_and_exits_on_a_quick_second_press() {
        let started = Instant::now();
        let mut last_press = None;

        assert_eq!(
            ctrl_c_action(&mut last_press, started),
            CtrlCAction::FirstPress
        );
        assert_eq!(
            ctrl_c_action(&mut last_press, started + Duration::from_millis(900)),
            CtrlCAction::Exit
        );
    }

    #[test]
    fn ctrl_c_rearms_after_the_double_press_window_expires() {
        let started = Instant::now();
        let mut last_press = None;

        assert_eq!(
            ctrl_c_action(&mut last_press, started),
            CtrlCAction::FirstPress
        );
        assert_eq!(
            ctrl_c_action(&mut last_press, started + Duration::from_millis(1_001)),
            CtrlCAction::FirstPress
        );
    }

    #[test]
    fn ctrl_c_clears_the_composer_before_cancelling_active_work() {
        let mut app = App::new(AppConfig::default());
        app.set_prompt("keep the active run alive");
        app.set_run_active(true);

        assert_eq!(
            ctrl_c_primary_action(&mut app, true),
            CtrlCPrimaryAction::ClearComposer
        );
        assert!(app.prompt().is_empty());
        assert!(app.run_active());
        assert_eq!(
            ctrl_c_primary_action(&mut app, true),
            CtrlCPrimaryAction::CancelRun
        );
    }

    #[test]
    fn compaction_feedback_is_visible_until_the_command_finishes() {
        let mut app = App::new(AppConfig::default());

        begin_compaction_feedback(&mut app);

        assert_eq!(
            app.transcript().last().map(|entry| entry.text.as_str()),
            Some("Compaction started")
        );
        assert_eq!(app.current_activity(), Some("Compacting context…"));
        let rendered = app.render(
            TerminalSize {
                width: 80,
                height: 24,
            },
            RenderOptions {
                capabilities: TerminalCapabilities::plain(),
            },
        );
        assert!(rendered.contains("Compaction started"));
        assert!(rendered.contains("✦ Compacting context…"));

        finish_compaction_feedback(&mut app);

        assert_eq!(app.current_activity(), None);
    }

    #[test]
    fn unchanged_terminal_frames_are_not_emitted_again() {
        let mut cache = TerminalFrameCache::default();
        assert_eq!(
            cache.frame_damage(80, 24, "frame-a", None),
            TerminalFrameDamage::Full
        );
        assert_eq!(
            cache.frame_damage(80, 24, "frame-a", None),
            TerminalFrameDamage::Unchanged
        );
        assert_eq!(
            cache.frame_damage(81, 24, "frame-a", None),
            TerminalFrameDamage::Full
        );
        assert_eq!(
            cache.frame_damage(81, 24, "frame-b", None),
            TerminalFrameDamage::Rows(vec![0])
        );
        assert_eq!(
            cache.frame_damage(81, 24, "frame-b", Some((3, 23))),
            TerminalFrameDamage::Rows(Vec::new())
        );
    }

    #[test]
    fn changed_terminal_frames_only_damage_changed_rows() {
        let mut cache = TerminalFrameCache::default();
        let stable_transcript = "\u{1b}[32m● stable transcript\u{1b}[0m";
        let first = format!("{stable_transcript}\r\n\u{1b}[35m✦ Working…\u{1b}[0m");
        let second = format!("{stable_transcript}\r\n\u{1b}[35m✦ Writing…\u{1b}[0m");

        assert_eq!(
            cache.frame_damage(80, 24, &first, None),
            TerminalFrameDamage::Full
        );
        assert_eq!(
            cache.frame_damage(80, 24, &second, None),
            TerminalFrameDamage::Rows(vec![1])
        );
    }

    #[test]
    fn user_bash_prefixes_are_consumed_before_model_submission() {
        assert_eq!(
            parse_user_bash_submission("! git status"),
            Some(UserBashSubmission {
                command: "git status".into(),
                exclude_from_context: false,
            })
        );
        assert_eq!(
            parse_user_bash_submission("!! pwd"),
            Some(UserBashSubmission {
                command: "pwd".into(),
                exclude_from_context: true,
            })
        );
        assert_eq!(
            parse_user_bash_submission("!!"),
            Some(UserBashSubmission {
                command: String::new(),
                exclude_from_context: true,
            })
        );
        assert_eq!(parse_user_bash_submission("explain !"), None);
    }

    #[test]
    fn user_messages_carry_validated_workspace_references_to_the_model() {
        let workspace = TempDir::new().expect("workspace");
        std::fs::create_dir(workspace.path().join("src")).expect("source directory");
        std::fs::write(workspace.path().join("src/lib.rs"), "pub fn run() {}").expect("source");

        let message = user_message(
            "Review @src/lib.rs and @src/ but ignore @../outside",
            Vec::new(),
            Some(workspace.path()),
        );
        let text = message.text();
        let context = match message.content.get(1) {
            Some(Content::Text { text }) => text,
            other => panic!("expected workspace reference context, got {other:?}"),
        };

        assert!(text.starts_with("Review @src/lib.rs and @src/"));
        assert!(context.contains(r#""path":"src/lib.rs","kind":"file""#));
        assert!(context.contains(r#""path":"src","kind":"folder""#));
        assert!(!context.contains("outside"));
    }

    #[tokio::test]
    async fn rlm_depth_is_session_scoped_and_global_is_an_explicit_second_write() {
        let state = TempDir::new().expect("state");
        assert_eq!(
            load_tui_rlm_max_depth(state.path(), "alpha")
                .await
                .expect("default"),
            3
        );
        handle_tui_rlm_max_depth(state.path(), "alpha", Some("5"))
            .await
            .expect("session depth");
        assert_eq!(
            load_tui_rlm_max_depth(state.path(), "alpha")
                .await
                .expect("alpha"),
            5
        );
        assert_eq!(
            load_tui_rlm_max_depth(state.path(), "beta")
                .await
                .expect("beta"),
            3
        );
        handle_tui_rlm_max_depth(state.path(), "alpha", Some("7 --global"))
            .await
            .expect("global depth");
        assert_eq!(
            load_tui_rlm_max_depth(state.path(), "alpha")
                .await
                .expect("alpha global"),
            7
        );
        assert_eq!(
            load_tui_rlm_max_depth(state.path(), "beta")
                .await
                .expect("beta global"),
            7
        );
    }

    #[tokio::test]
    async fn local_logs_and_update_commands_do_not_contact_a_remote_service() {
        let state = TempDir::new().expect("state");
        let logs = show_tui_logs(state.path()).await.expect("logs");
        assert!(logs.contains("No logs written yet"));
        let update = show_tui_update_status(Some("check")).expect("update status");
        assert!(update.contains(env!("CARGO_PKG_VERSION")));
        assert!(update.contains("no configured signed update transport"));
        assert!(show_tui_update_status(Some("install")).is_err());
    }

    #[test]
    fn tree_filters_apply_to_typed_session_nodes() {
        let user = SessionNodeKind::Message { role: Role::User };
        let assistant = SessionNodeKind::Message {
            role: Role::Assistant,
        };
        let tool = SessionNodeKind::Message { role: Role::Tool };
        let label = SessionNodeKind::RuntimeEvent {
            name: "session_entry_label".into(),
        };
        let tool_event = SessionNodeKind::RuntimeEvent {
            name: "tool_finished".into(),
        };

        assert!(tree_node_visible(&user, TreeFilterMode::UserOnly));
        assert!(!tree_node_visible(&assistant, TreeFilterMode::UserOnly));
        assert!(tree_node_visible(&assistant, TreeFilterMode::Default));
        assert!(!tree_node_visible(&tool, TreeFilterMode::Default));
        assert!(!tree_node_visible(&tool_event, TreeFilterMode::NoTools));
        assert!(tree_node_visible(&label, TreeFilterMode::LabeledOnly));
        assert!(tree_node_visible(&tool, TreeFilterMode::All));
    }

    #[tokio::test]
    async fn expanded_tui_preferences_round_trip_with_bounds() {
        let state = TempDir::new().expect("state");
        let app = Arc::new(Mutex::new(App::new(AppConfig::default())));
        app.lock()
            .expect("app")
            .set_preferences(AppPreferenceState {
                agent_mode: AgentMode::Plan,
                show_images: false,
                auto_resize_images: false,
                block_images: true,
                follow_up_mode: QueueMode::All,
                autocomplete_max_visible: 20,
                tree_filter_mode: TreeFilterMode::NoTools,
                show_hardware_cursor: true,
                editor_padding_x: 8,
                show_terminal_progress: false,
                show_warnings: false,
                ..AppPreferenceState::default()
            });
        persist_app_preferences(state.path(), &app)
            .await
            .expect("persist preferences");
        let loaded = load_tui_preferences(state.path())
            .await
            .expect("load preferences");
        assert_eq!(loaded.agent_mode, "plan");
        assert!(!loaded.show_images);
        assert!(!loaded.auto_resize_images);
        assert!(loaded.block_images);
        assert_eq!(loaded.follow_up_mode, "all");
        assert_eq!(loaded.autocomplete_max_visible, 20);
        assert_eq!(loaded.tree_filter_mode, "no-tools");
        assert!(loaded.show_hardware_cursor);
        assert_eq!(loaded.editor_padding_x, 8);
        assert!(!loaded.show_terminal_progress);
        assert!(!loaded.show_warnings);
    }

    #[tokio::test]
    async fn older_tui_preferences_default_to_default_agent_mode() {
        let state = TempDir::new().expect("state");
        let path = tui_preferences_path(state.path());
        std::fs::create_dir_all(path.parent().expect("settings parent"))
            .expect("settings directory");
        std::fs::write(&path, "{}").expect("legacy settings");
        assert_eq!(
            load_tui_agent_mode(state.path())
                .await
                .expect("legacy mode"),
            AgentMode::Default
        );
    }

    #[tokio::test]
    async fn prompt_loop_drains_all_mode_followups_as_one_runtime_batch() {
        let state = TempDir::new().expect("state");
        let response = |text: &str| ModelResponse {
            message: Message::assistant(
                vec![Content::Text { text: text.into() }],
                StopReason::Stop,
            ),
            response_id: None,
        };
        let provider = Arc::new(FakeProvider::new(vec![
            response("initial answer"),
            response("follow-up answer"),
        ]));
        let tools = Arc::new(
            ToolRegistry::with_default_tools(state.path(), ToolPolicy::default()).expect("tools"),
        );
        let runtime = Arc::new(
            AgentRuntime::resume(
                provider.clone(),
                tools,
                Arc::new(InMemorySessionStore::default()),
                RuntimeConfig::default_for_model("fake/model"),
            )
            .await
            .expect("runtime"),
        );
        let app = Arc::new(Mutex::new(App::new(AppConfig::default())));
        {
            let mut state = app.lock().expect("app");
            state.set_preferences(AppPreferenceState {
                follow_up_mode: QueueMode::All,
                ..AppPreferenceState::default()
            });
            state.queue_follow_up("first").expect("first");
            state.queue_follow_up("second").expect("second");
        }
        run_tui_prompt_loop(
            runtime,
            Arc::new(TestFactory {
                state: state.path().to_path_buf(),
            }),
            app,
            Arc::new(Mutex::new(AutonomousState::default())),
            user_message(
                "initial",
                vec![crate::tui::ImageAttachment {
                    data: "aW1hZ2U=".into(),
                    mime_type: "image/png".into(),
                    byte_size: 5,
                }],
                None,
            ),
            None,
            "default".into(),
        )
        .await;

        let requests = provider.requests().await;
        assert_eq!(requests.len(), 2);
        assert!(requests[0].messages.iter().any(|message| {
            message.content.iter().any(|content| {
                matches!(
                    content,
                    Content::Image { mime_type, .. } if mime_type == "image/png"
                )
            })
        }));
        let user_texts = requests[1]
            .messages
            .iter()
            .filter(|message| message.role == Role::User)
            .map(Message::text)
            .collect::<Vec<_>>();
        assert!(user_texts.ends_with(&["first".into(), "second".into()]));
    }

    #[tokio::test]
    async fn autonomous_prompt_loop_stops_on_finish_task_without_an_extra_request() {
        let state = TempDir::new().expect("state");
        let provider = Arc::new(FakeProvider::new(vec![ModelResponse {
            message: Message::assistant(
                vec![Content::ToolCall(ToolCall {
                    id: "tui-finish".into(),
                    name: "finish_task".into(),
                    arguments: json!({"summary": "TUI complete"}),
                })],
                StopReason::ToolUse,
            ),
            response_id: Some("tui-finish".into()),
        }]));
        let runtime = Arc::new(
            AgentRuntime::resume(
                provider.clone(),
                Arc::new(
                    ToolRegistry::with_default_tools(state.path(), ToolPolicy::default())
                        .expect("tools"),
                ),
                Arc::new(InMemorySessionStore::default()),
                RuntimeConfig::default_for_model("fake/model"),
            )
            .await
            .expect("runtime"),
        );
        let app = Arc::new(Mutex::new(App::new(AppConfig::default())));
        app.lock().expect("app").set_run_active(true);
        let mut autonomous = AutonomousState::default();
        autonomous.enable(Instant::now());
        let autonomous = Arc::new(Mutex::new(autonomous));

        run_tui_prompt_loop(
            runtime,
            Arc::new(TestFactory {
                state: state.path().to_path_buf(),
            }),
            app.clone(),
            autonomous.clone(),
            Message::user("complete"),
            None,
            "default".into(),
        )
        .await;

        assert_eq!(provider.requests().await.len(), 1);
        assert!(!app.lock().expect("app").run_active());
        assert_eq!(autonomous.lock().expect("state").turns_used(), 1);
        assert!(app.lock().expect("app").transcript().iter().any(|entry| {
            entry
                .text
                .contains("Autonomous task complete: TUI complete")
        }));
    }

    #[tokio::test]
    async fn session_info_reports_the_active_session_and_message_counts() {
        let state = TempDir::new().expect("state");
        let store = FileSessionStore::create(state.path(), "active")
            .await
            .expect("store");
        store
            .append(SessionRecord::new(SessionPayload::Message(Message::user(
                "inspect",
            ))))
            .await
            .expect("message");
        let factory = TestFactory {
            state: state.path().to_path_buf(),
        };
        let runtime = factory
            .build("fake/model", "active")
            .await
            .expect("runtime");
        let runtime_key = ("fake/model".into(), "active".into());
        let info = show_tui_session_info(&runtime, Some(&runtime_key), state.path())
            .await
            .expect("session info");
        assert!(info.contains("ID: active"));
        assert!(info.contains("User: 1"));
        assert!(info.contains("Total: 1"));
    }

    #[tokio::test]
    async fn import_command_replaces_the_confirmed_active_session_atomically() {
        let state = TempDir::new().expect("state");
        let source_store = FileSessionStore::create(state.path(), "source")
            .await
            .expect("source store");
        source_store
            .append(SessionRecord::new(SessionPayload::Message(Message::user(
                "import me",
            ))))
            .await
            .expect("source record");
        export_tui_session(state.path(), "source", Some("exports/source.jsonl"))
            .await
            .expect("source export");

        let factory = TestFactory {
            state: state.path().to_path_buf(),
        };
        let mut runtime = factory
            .build("fake/model", "active")
            .await
            .expect("active runtime");
        let mut runtime_key = Some(("fake/model".into(), "active".into()));
        let app = Arc::new(Mutex::new(App::new(AppConfig::default())));
        app.lock().expect("app").select_session("active");
        let source = state.path().join("exports/source.jsonl");
        let message = import_tui_session(
            &mut runtime,
            &factory,
            &mut runtime_key,
            &app,
            state.path(),
            state.path(),
            source.to_str().expect("UTF-8 path"),
        )
        .await
        .expect("import")
        .expect("message");
        assert!(message.contains("Imported"));
        let (_, imported_session) = runtime_key.expect("runtime key");
        assert_eq!(imported_session, "active");
        let records = FileSessionStore::create(state.path(), &imported_session)
            .await
            .expect("import store")
            .load()
            .await
            .expect("import records")
            .records;
        assert!(records.iter().any(|record| {
            matches!(
                &record.payload,
                SessionPayload::Message(message) if message.text() == "import me"
            )
        }));
    }

    #[test]
    fn session_event_summary_is_useful_bounded_and_content_free() {
        let event = serde_json::json!({
            "type": "observed_session_event",
            "activeSessionId": "session-a",
            "event": {
                "type": "compaction_end",
                "aborted": false,
                "result": {"tokensBefore": 1234, "summary": "private transcript text"},
                "message": {"content": "another private value"}
            }
        });
        let summary = summarize_session_event(&event);
        assert_eq!(
            summary,
            "session event: type=observed_session_event session=session-a event=compaction_end tokensBefore=1234 aborted=false"
        );
        assert!(!summary.contains("private"));
        assert!(summary.len() < 512);
    }
}
