use std::{
    collections::{BTreeSet, VecDeque},
    fmt::Write as _,
    sync::LazyLock,
};

use crate::{
    model::ThinkingLevel,
    orchestration::HeartbeatManagementAction,
    resources::{CustomTheme, PromptTemplate, Skill, expand_prompt_template},
    runtime::QueueMode,
    tools::{AgentMode, ApprovalDecision, ClarifyingQuestion, PermissionRequest},
};
use uuid::Uuid;

use super::{
    Action, InputBinding, KeyCode, KeyEvent, RenderOptions, SlashCommand, TerminalSize, TuiAction,
    commands::builtin_command_usage, parse_slash_command, render::render,
};

static EMPTY_MODELS: LazyLock<BTreeSet<String>> = LazyLock::new(BTreeSet::new);
const BUILTIN_COMPLETIONS: &[&str] = &[
    "/autonomous",
    "/btw",
    "/changelog",
    "/clear",
    "/clone",
    "/compact",
    "/context",
    "/copy",
    "/effort",
    "/export",
    "/fast",
    "/fork",
    "/fullscreen",
    "/goal",
    "/heartbeat",
    "/heartbeats",
    "/help",
    "/hotkeys",
    "/import",
    "/implement",
    "/login",
    "/logout",
    "/logs",
    "/mcp",
    "/model",
    "/mode",
    "/name",
    "/new",
    "/quit",
    "/refine",
    "/reload",
    "/resume",
    "/rlm-max-depth",
    "/scoped-models",
    "/session",
    "/sessions",
    "/settings",
    "/share",
    "/system-prompt",
    "/theme",
    "/traces",
    "/tree",
    "/update",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeName {
    Dark,
    Light,
    System,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TreeFilterMode {
    #[default]
    Default,
    NoTools,
    UserOnly,
    LabeledOnly,
    All,
}

impl TreeFilterMode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::NoTools => "no-tools",
            Self::UserOnly => "user-only",
            Self::LabeledOnly => "labeled-only",
            Self::All => "all",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "default" => Some(Self::Default),
            "no-tools" => Some(Self::NoTools),
            "user-only" => Some(Self::UserOnly),
            "labeled-only" => Some(Self::LabeledOnly),
            "all" => Some(Self::All),
            _ => None,
        }
    }

    const fn next(self) -> Self {
        match self {
            Self::Default => Self::NoTools,
            Self::NoTools => Self::UserOnly,
            Self::UserOnly => Self::LabeledOnly,
            Self::LabeledOnly => Self::All,
            Self::All => Self::Default,
        }
    }
}

impl ThemeName {
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "dark" => Some(Self::Dark),
            "light" => Some(Self::Light),
            "system" => Some(Self::System),
            _ => None,
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Dark => "dark",
            Self::Light => "light",
            Self::System => "system",
        }
    }

    fn selector_options() -> Vec<String> {
        vec!["dark".into(), "light".into(), "system".into()]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayKind {
    Help,
    Hotkeys,
    Login,
    EffortSelector,
    ForkSelector,
    TreeSelector,
    ModelSelector,
    ResumeSelector,
    SessionSelector,
    HeartbeatSelector,
    HeartbeatActionSelector,
    Settings,
    Autocomplete,
    ScopedModelsSelector,
    ThemeSelector,
    PromptTemplateSelector,
    SkillSelector,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectorOverlay {
    pub title: String,
    pub kind: OverlayKind,
    pub options: Vec<String>,
    pub selected: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Overlay {
    None,
    Help,
    Hotkeys,
    Login {
        provider: Option<String>,
        input: String,
    },
    Logout {
        provider: Option<String>,
        input: String,
    },
    McpLogin {
        server: String,
        input: String,
    },
    Confirm {
        title: String,
        message: String,
        action: Box<TuiAction>,
    },
    WorkspacePermission {
        request: PermissionRequest,
        selected: usize,
    },
    Clarification {
        request: ClarifyingQuestion,
        selected: usize,
        input: String,
    },
    Selector(SelectorOverlay),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiRequest {
    Login {
        provider: String,
        secret: Option<String>,
    },
    Logout {
        provider: String,
    },
    McpApiKey {
        server: String,
        api_key: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamEvent {
    RunStarted,
    TextDelta(String),
    Thinking(String),
    RetryStarted {
        attempt: u32,
        max_attempts: u32,
        delay_ms: u64,
    },
    RetryFinished {
        success: bool,
        attempt: u32,
        final_error: Option<String>,
    },
    Completed(String),
    Failed(String),
    BudgetPaused(String),
    UserInputRequested(ClarifyingQuestion),
    ExtensionUi {
        extension: String,
        request: crate::extensions::UiRequest,
    },
    ExtensionRendered {
        custom_type: String,
        lines: Vec<String>,
    },
    SessionMetadata {
        summary: String,
    },
    Images(Vec<String>),
    Activity(String),
    ToolFinished {
        name: String,
        summary: String,
        failed: bool,
    },
    Warning(String),
    BashStarted {
        command: String,
        exclude_from_context: bool,
    },
    BashFinished {
        output: String,
        exit_code: Option<i32>,
        cancelled: bool,
        truncated: bool,
        timed_out: bool,
        full_output_path: Option<String>,
        error: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptRole {
    User,
    Assistant,
    Thinking,
    Tool,
    System,
    Warning,
    Error,
}

impl TranscriptRole {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Thinking => "thinking",
            Self::Tool => "tool",
            Self::System => "system",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageAttachment {
    pub data: String,
    pub mime_type: String,
    pub byte_size: usize,
}

#[derive(Debug, Clone)]
struct QueuedPrompt {
    text: String,
    images: Vec<ImageAttachment>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptEntry {
    pub role: TranscriptRole,
    pub text: String,
}

#[derive(Debug, Clone, Default)]
pub struct AppConfig {
    pub bindings: Vec<InputBinding>,
}

/// Bounded resource catalogs discovered by the runtime factory for TUI-only selection.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TuiResourceSnapshot {
    pub prompt_templates: Vec<PromptTemplate>,
    pub themes: Vec<CustomTheme>,
    pub skills: Vec<Skill>,
}

#[derive(Debug, Clone)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent TUI display toggles mirror persisted operator settings"
)]
pub struct AppPreferenceState {
    pub agent_mode: AgentMode,
    pub fast_mode: bool,
    pub fullscreen: bool,
    pub auto_compaction: bool,
    pub steering_mode: QueueMode,
    pub theme: ThemeName,
    pub custom_theme: Option<String>,
    pub scoped_models: Option<BTreeSet<String>>,
    pub show_images: bool,
    pub auto_resize_images: bool,
    pub block_images: bool,
    pub follow_up_mode: QueueMode,
    pub autocomplete_max_visible: u8,
    pub tree_filter_mode: TreeFilterMode,
    pub show_hardware_cursor: bool,
    pub editor_padding_x: u8,
    pub show_terminal_progress: bool,
    pub show_warnings: bool,
}

impl Default for AppPreferenceState {
    fn default() -> Self {
        Self {
            agent_mode: AgentMode::Default,
            fast_mode: false,
            fullscreen: true,
            auto_compaction: true,
            steering_mode: QueueMode::OneAtATime,
            theme: ThemeName::Dark,
            custom_theme: None,
            scoped_models: None,
            show_images: true,
            auto_resize_images: true,
            block_images: false,
            follow_up_mode: QueueMode::OneAtATime,
            autocomplete_max_visible: 8,
            tree_filter_mode: TreeFilterMode::Default,
            show_hardware_cursor: true,
            editor_padding_x: 0,
            show_terminal_progress: true,
            show_warnings: true,
        }
    }
}

pub struct App {
    prompt: String,
    cursor_chars: usize,
    history: Vec<String>,
    history_index: Option<usize>,
    history_draft: Option<String>,
    pending_submission: Option<String>,
    pending_ui_request: Option<UiRequest>,
    pending_tui_action: Option<TuiAction>,
    overlay: Overlay,
    transcript: Vec<TranscriptEntry>,
    active_assistant: Option<String>,
    current_activity: Option<String>,
    pending_images: Vec<ImageAttachment>,
    submitted_images: Vec<ImageAttachment>,
    run_active: bool,
    bash_active: bool,
    selected_model: Option<String>,
    selected_session: Option<String>,
    selected_effort: ThinkingLevel,
    models: Vec<String>,
    sessions: Vec<String>,
    fork_entries: Vec<Uuid>,
    tree_entries: Vec<Uuid>,
    heartbeat_entries: Vec<(Uuid, String, bool)>,
    heartbeat_target: Option<(Uuid, String, bool)>,
    extension_commands: Vec<String>,
    follow_ups: VecDeque<QueuedPrompt>,
    resource_snapshot: TuiResourceSnapshot,
    preferences: AppPreferenceState,
    bindings: Vec<InputBinding>,
    should_quit: bool,
}

impl App {
    #[must_use]
    pub fn new(config: AppConfig) -> Self {
        Self {
            prompt: String::new(),
            cursor_chars: 0,
            history: Vec::new(),
            history_index: None,
            history_draft: None,
            pending_submission: None,
            pending_ui_request: None,
            pending_tui_action: None,
            overlay: Overlay::None,
            transcript: Vec::new(),
            active_assistant: None,
            current_activity: None,
            pending_images: Vec::new(),
            submitted_images: Vec::new(),
            run_active: false,
            bash_active: false,
            selected_model: None,
            selected_session: None,
            selected_effort: ThinkingLevel::Off,
            models: Vec::new(),
            sessions: Vec::new(),
            fork_entries: Vec::new(),
            tree_entries: Vec::new(),
            heartbeat_entries: Vec::new(),
            heartbeat_target: None,
            extension_commands: Vec::new(),
            follow_ups: VecDeque::new(),
            resource_snapshot: TuiResourceSnapshot::default(),
            preferences: AppPreferenceState::default(),
            bindings: config.bindings,
            should_quit: false,
        }
    }

    #[must_use]
    pub fn prompt(&self) -> &str {
        &self.prompt
    }

    #[must_use]
    pub fn pending_submission(&self) -> Option<String> {
        self.pending_submission.clone()
    }

    pub fn clear_pending_submission(&mut self) {
        self.pending_submission = None;
    }

    pub fn take_submitted_images(&mut self) -> Vec<ImageAttachment> {
        std::mem::take(&mut self.submitted_images)
    }

    pub fn take_ui_request(&mut self) -> Option<UiRequest> {
        self.pending_ui_request.take()
    }

    pub fn take_tui_action(&mut self) -> Option<TuiAction> {
        self.pending_tui_action.take()
    }

    pub fn push_system_message(&mut self, text: impl Into<String>) {
        self.transcript.push(TranscriptEntry {
            role: TranscriptRole::System,
            text: text.into(),
        });
    }

    pub fn push_warning_message(&mut self, text: impl Into<String>) {
        self.transcript.push(TranscriptEntry {
            role: TranscriptRole::Warning,
            text: text.into(),
        });
    }

    pub fn set_prompt(&mut self, text: impl Into<String>) {
        self.prompt = text.into();
        self.cursor_chars = self.prompt.chars().count();
    }

    pub fn insert_text(&mut self, text: &str) {
        let byte = byte_index(&self.prompt, self.cursor_chars);
        self.prompt.insert_str(byte, text);
        self.cursor_chars = self.cursor_chars.saturating_add(text.chars().count());
    }

    /// Adds one bounded image to the current composer.
    ///
    /// # Errors
    ///
    /// Returns an error when the per-prompt count or decoded-byte limit is exceeded.
    pub fn attach_image(&mut self, image: ImageAttachment) -> Result<(), &'static str> {
        const MAX_IMAGES: usize = 4;
        const MAX_TOTAL_BYTES: usize = 20 * 1024 * 1024;
        if self.pending_images.len() >= MAX_IMAGES {
            return Err("A prompt can contain at most 4 images");
        }
        let total = self
            .pending_images
            .iter()
            .map(|image| image.byte_size)
            .sum::<usize>()
            .saturating_add(image.byte_size);
        if total > MAX_TOTAL_BYTES {
            return Err("Prompt images exceed the 20 MiB limit");
        }
        self.pending_images.push(image);
        Ok(())
    }

    #[must_use]
    pub fn pending_images(&self) -> &[ImageAttachment] {
        &self.pending_images
    }

    pub fn open_fork_selector(&mut self, options: Vec<(Uuid, String)>) {
        self.fork_entries = options.iter().map(|(entry_id, _)| *entry_id).collect();
        self.overlay = Overlay::Selector(SelectorOverlay {
            title: "Fork before user message".into(),
            kind: OverlayKind::ForkSelector,
            options: options.into_iter().map(|(_, label)| label).collect(),
            selected: 0,
        });
    }

    pub fn open_tree_selector(&mut self, options: Vec<(Uuid, String)>) {
        self.tree_entries = options.iter().map(|(entry_id, _)| *entry_id).collect();
        self.overlay = Overlay::Selector(SelectorOverlay {
            title: "Continue from session entry".into(),
            kind: OverlayKind::TreeSelector,
            options: options.into_iter().map(|(_, label)| label).collect(),
            selected: self.tree_entries.len().saturating_sub(1),
        });
    }

    pub fn open_heartbeat_selector(&mut self, options: Vec<(Uuid, String, bool, String)>) {
        self.heartbeat_entries = options
            .iter()
            .map(|(id, session, paused, _)| (*id, session.clone(), *paused))
            .collect();
        self.overlay = Overlay::Selector(SelectorOverlay {
            title: "Heartbeats (Enter manages)".into(),
            kind: OverlayKind::HeartbeatSelector,
            options: options.into_iter().map(|(_, _, _, label)| label).collect(),
            selected: 0,
        });
    }

    fn open_autocomplete(&mut self) {
        let prefix = self.prompt.trim().to_ascii_lowercase();
        if !prefix.starts_with('/') || prefix.chars().any(char::is_whitespace) {
            return;
        }
        let mut options = BUILTIN_COMPLETIONS
            .iter()
            .map(|command| (*command).to_owned())
            .chain(
                self.extension_commands
                    .iter()
                    .map(|command| format!("/{command}")),
            )
            .chain(
                self.resource_snapshot
                    .prompt_templates
                    .iter()
                    .map(|template| format!("/{}", template.name)),
            )
            .chain(
                self.resource_snapshot
                    .skills
                    .iter()
                    .map(|skill| format!("/skill:{}", skill.name)),
            )
            .filter(|option| option.to_ascii_lowercase().starts_with(&prefix))
            .collect::<Vec<_>>();
        options.sort();
        options.dedup();
        options.truncate(usize::from(self.preferences.autocomplete_max_visible));
        if !options.is_empty() {
            self.overlay = Overlay::Selector(SelectorOverlay {
                title: "Autocomplete".into(),
                kind: OverlayKind::Autocomplete,
                options,
                selected: 0,
            });
        }
    }

    pub fn open_confirm(
        &mut self,
        title: impl Into<String>,
        message: impl Into<String>,
        action: TuiAction,
    ) {
        self.overlay = Overlay::Confirm {
            title: title.into(),
            message: message.into(),
            action: Box::new(action),
        };
    }

    pub fn open_workspace_permission(&mut self, request: PermissionRequest) {
        self.overlay = Overlay::WorkspacePermission {
            request,
            selected: 0,
        };
    }

    pub fn open_clarification(&mut self, request: ClarifyingQuestion) {
        self.overlay = Overlay::Clarification {
            request,
            selected: 0,
            input: String::new(),
        };
    }

    pub fn open_mcp_login(&mut self, server: impl Into<String>) {
        self.overlay = Overlay::McpLogin {
            server: server.into(),
            input: String::new(),
        };
    }

    #[must_use]
    pub fn overlay(&self) -> &Overlay {
        &self.overlay
    }

    #[must_use]
    pub fn transcript(&self) -> &[TranscriptEntry] {
        &self.transcript
    }

    #[must_use]
    pub fn active_assistant_text(&self) -> Option<&str> {
        self.active_assistant.as_deref()
    }

    #[must_use]
    pub fn current_activity(&self) -> Option<&str> {
        self.current_activity.as_deref()
    }

    pub(crate) fn command_parameter_hint(&self) -> Option<String> {
        let command = self.prompt.trim_start().strip_prefix('/')?;
        let command_end = command.find(char::is_whitespace).unwrap_or(command.len());
        let name = &command[..command_end];
        if name.is_empty() {
            return None;
        }
        if let Some(usage) = builtin_command_usage(name) {
            return Some(usage.into());
        }
        self.resource_snapshot
            .prompt_templates
            .iter()
            .find(|template| template.name.eq_ignore_ascii_case(name))
            .and_then(|template| template.argument_hint.as_deref())
            .map(str::trim)
            .filter(|hint| !hint.is_empty())
            .map(|hint| format!("/{name} {hint}"))
    }

    #[must_use]
    pub const fn run_active(&self) -> bool {
        self.run_active
    }

    pub fn set_run_active(&mut self, active: bool) {
        self.run_active = active;
    }

    #[must_use]
    pub const fn bash_active(&self) -> bool {
        self.bash_active
    }

    pub fn set_bash_active(&mut self, active: bool) {
        self.bash_active = active;
    }

    #[must_use]
    pub fn selected_model(&self) -> Option<&str> {
        self.selected_model.as_deref()
    }

    #[must_use]
    pub fn selected_session(&self) -> Option<&str> {
        self.selected_session.as_deref()
    }

    #[must_use]
    pub const fn selected_effort(&self) -> ThinkingLevel {
        self.selected_effort
    }

    #[must_use]
    pub const fn theme(&self) -> ThemeName {
        self.preferences.theme
    }

    #[must_use]
    pub const fn fast_mode(&self) -> bool {
        self.preferences.fast_mode
    }

    #[must_use]
    pub const fn agent_mode(&self) -> AgentMode {
        self.preferences.agent_mode
    }

    pub fn set_agent_mode(&mut self, mode: AgentMode) {
        self.preferences.agent_mode = mode;
    }

    pub fn set_fast_mode(&mut self, enabled: bool) {
        self.preferences.fast_mode = enabled;
    }

    #[must_use]
    pub const fn fullscreen(&self) -> bool {
        self.preferences.fullscreen
    }

    #[must_use]
    pub const fn auto_compaction(&self) -> bool {
        self.preferences.auto_compaction
    }

    #[must_use]
    pub const fn steering_mode(&self) -> QueueMode {
        self.preferences.steering_mode
    }

    #[must_use]
    pub const fn follow_up_mode(&self) -> QueueMode {
        self.preferences.follow_up_mode
    }

    #[must_use]
    pub const fn tree_filter_mode(&self) -> TreeFilterMode {
        self.preferences.tree_filter_mode
    }

    #[must_use]
    pub const fn show_hardware_cursor(&self) -> bool {
        self.preferences.show_hardware_cursor
    }

    #[must_use]
    pub const fn editor_padding_x(&self) -> u8 {
        self.preferences.editor_padding_x
    }

    #[must_use]
    pub const fn cursor_chars(&self) -> usize {
        self.cursor_chars
    }

    #[must_use]
    pub const fn show_terminal_progress(&self) -> bool {
        self.preferences.show_terminal_progress
    }

    #[must_use]
    pub const fn show_warnings(&self) -> bool {
        self.preferences.show_warnings
    }

    #[must_use]
    pub(crate) fn preferences_snapshot(&self) -> AppPreferenceState {
        self.preferences.clone()
    }

    /// Adds a bounded follow-up that will run after the active provider turn.
    ///
    /// # Errors
    ///
    /// Returns an error for blank input or when the 64-message queue is full.
    pub fn queue_follow_up(&mut self, prompt: impl Into<String>) -> Result<(), &'static str> {
        self.queue_follow_up_with_images(prompt, Vec::new())
    }

    pub(crate) fn queue_follow_up_with_images(
        &mut self,
        prompt: impl Into<String>,
        images: Vec<ImageAttachment>,
    ) -> Result<(), &'static str> {
        let prompt = prompt.into();
        if prompt.trim().is_empty() && images.is_empty() {
            return Err("follow-up must not be blank");
        }
        if self.follow_ups.len() >= 64 {
            return Err("follow-up queue reached its 64-message limit");
        }
        self.follow_ups.push_back(QueuedPrompt {
            text: prompt,
            images,
        });
        Ok(())
    }

    /// Drains one or all queued follow-ups according to the selected mode.
    pub fn take_next_follow_ups(&mut self) -> Vec<String> {
        self.take_next_follow_up_messages()
            .into_iter()
            .map(|(text, _)| text)
            .collect()
    }

    pub(crate) fn take_next_follow_up_messages(&mut self) -> Vec<(String, Vec<ImageAttachment>)> {
        match self.preferences.follow_up_mode {
            QueueMode::All => self
                .follow_ups
                .drain(..)
                .map(|queued| (queued.text, queued.images))
                .collect(),
            QueueMode::OneAtATime => self
                .follow_ups
                .pop_front()
                .map(|queued| (queued.text, queued.images))
                .into_iter()
                .collect(),
        }
    }

    #[must_use]
    pub fn scoped_models(&self) -> &BTreeSet<String> {
        self.preferences
            .scoped_models
            .as_ref()
            .unwrap_or(&EMPTY_MODELS)
    }

    pub fn set_models(&mut self, models: Vec<String>) {
        if self.selected_model.is_none() {
            self.selected_model = models.first().cloned();
        }
        if self.preferences.scoped_models.is_none() {
            self.preferences.scoped_models = Some(models.iter().cloned().collect());
        }
        self.models = models;
    }

    pub fn set_sessions(&mut self, sessions: Vec<String>) {
        if self.selected_session.is_none() {
            self.selected_session = sessions.first().cloned();
        }
        self.sessions = sessions;
    }

    pub fn set_extension_commands(&mut self, commands: Vec<String>) {
        self.extension_commands = commands;
    }

    pub fn set_resource_snapshot(&mut self, mut snapshot: TuiResourceSnapshot) {
        snapshot
            .prompt_templates
            .sort_by(|left, right| left.name.cmp(&right.name));
        snapshot
            .themes
            .sort_by(|left, right| left.name.cmp(&right.name));
        snapshot
            .skills
            .sort_by(|left, right| left.name.cmp(&right.name));
        self.resource_snapshot = snapshot;
        if self
            .preferences
            .custom_theme
            .as_ref()
            .is_some_and(|selected| {
                !self
                    .resource_snapshot
                    .themes
                    .iter()
                    .any(|theme| &theme.name == selected)
            })
        {
            self.preferences.custom_theme = None;
        }
    }

    pub fn set_preferences(&mut self, preferences: AppPreferenceState) {
        self.preferences = AppPreferenceState {
            autocomplete_max_visible: preferences.autocomplete_max_visible.clamp(3, 20),
            editor_padding_x: preferences.editor_padding_x.min(8),
            ..preferences
        };
        if self
            .preferences
            .custom_theme
            .as_ref()
            .is_some_and(|selected| {
                !self
                    .resource_snapshot
                    .themes
                    .iter()
                    .any(|theme| &theme.name == selected)
            })
        {
            self.preferences.custom_theme = None;
        }
    }

    pub(crate) fn selected_custom_theme(&self) -> Option<&CustomTheme> {
        let selected = self.preferences.custom_theme.as_deref()?;
        self.resource_snapshot
            .themes
            .iter()
            .find(|theme| theme.name == selected)
    }

    pub fn add_session(&mut self, session: impl Into<String>) {
        let session = session.into();
        if !self.sessions.contains(&session) {
            self.sessions.push(session);
        }
    }

    pub fn select_model(&mut self, model: impl Into<String>) {
        self.selected_model = Some(model.into());
    }

    fn select_or_filter_model(&mut self, search: &str) {
        let search = search.trim();
        if let Some(model) = self
            .models
            .iter()
            .find(|model| model.eq_ignore_ascii_case(search))
            .cloned()
        {
            self.selected_model = Some(model);
            return;
        }
        let query = search.to_ascii_lowercase();
        let options = self
            .models
            .iter()
            .filter(|model| model.to_ascii_lowercase().contains(&query))
            .cloned()
            .collect::<Vec<_>>();
        if options.is_empty() {
            self.push_system_message(format!("No models match: {search}"));
            return;
        }
        self.overlay = Overlay::Selector(SelectorOverlay {
            title: format!("Models matching {search}"),
            kind: OverlayKind::ModelSelector,
            options,
            selected: 0,
        });
    }

    pub fn select_session(&mut self, session: impl Into<String>) {
        self.selected_session = Some(session.into());
    }

    pub fn push_user_message(&mut self, text: impl Into<String>) {
        self.transcript.push(TranscriptEntry {
            role: TranscriptRole::User,
            text: text.into(),
        });
    }

    pub(crate) fn push_user_submission(&mut self, text: &str, image_count: usize) {
        let image_label = match image_count {
            0 => String::new(),
            1 => "▣ 1 image".into(),
            count => format!("▣ {count} images"),
        };
        let display = match (text.trim().is_empty(), image_label.is_empty()) {
            (false, false) => format!("{}\n{image_label}", text.trim()),
            (false, true) => text.trim().to_owned(),
            (true, false) => image_label,
            (true, true) => String::new(),
        };
        self.push_user_message(display);
    }

    pub fn open_overlay(&mut self, kind: OverlayKind) {
        self.overlay = match kind {
            OverlayKind::Help => Overlay::Help,
            OverlayKind::Hotkeys => Overlay::Hotkeys,
            OverlayKind::Settings => self.settings_overlay(),
            OverlayKind::Login => Overlay::Login {
                provider: None,
                input: String::new(),
            },
            OverlayKind::EffortSelector => Overlay::Selector(SelectorOverlay {
                title: "Reasoning effort".into(),
                kind,
                options: ThinkingLevel::ALL
                    .into_iter()
                    .map(|level| level.as_str().into())
                    .collect(),
                selected: ThinkingLevel::ALL
                    .iter()
                    .position(|level| *level == self.selected_effort)
                    .unwrap_or(0),
            }),
            OverlayKind::ForkSelector => Overlay::Selector(SelectorOverlay {
                title: "Fork before user message".into(),
                kind,
                options: Vec::new(),
                selected: 0,
            }),
            OverlayKind::TreeSelector => Overlay::Selector(SelectorOverlay {
                title: "Continue from session entry".into(),
                kind,
                options: Vec::new(),
                selected: 0,
            }),
            OverlayKind::ModelSelector => self.model_selector_overlay(),
            OverlayKind::SessionSelector => self.session_selector_overlay("Sessions", kind),
            OverlayKind::HeartbeatSelector => Overlay::Selector(SelectorOverlay {
                title: "Heartbeats (Enter manages)".into(),
                kind,
                options: Vec::new(),
                selected: 0,
            }),
            OverlayKind::HeartbeatActionSelector => Overlay::Selector(SelectorOverlay {
                title: "Manage heartbeat".into(),
                kind,
                options: Vec::new(),
                selected: 0,
            }),
            OverlayKind::ScopedModelsSelector => Overlay::Selector(SelectorOverlay {
                title: "Scoped models (Enter toggles, Esc saves)".into(),
                kind,
                options: self.models.clone(),
                selected: 0,
            }),
            OverlayKind::ResumeSelector => self.session_selector_overlay("Resume session", kind),
            OverlayKind::ThemeSelector => self.theme_selector_overlay(),
            OverlayKind::Autocomplete => Overlay::Selector(SelectorOverlay {
                title: "Autocomplete".into(),
                kind,
                options: Vec::new(),
                selected: 0,
            }),
            OverlayKind::PromptTemplateSelector => Overlay::Selector(SelectorOverlay {
                title: "Prompt templates".into(),
                kind,
                options: self
                    .resource_snapshot
                    .prompt_templates
                    .iter()
                    .map(|template| template.name.clone())
                    .collect(),
                selected: 0,
            }),
            OverlayKind::SkillSelector => Overlay::Selector(SelectorOverlay {
                title: "Skills".into(),
                kind,
                options: self
                    .resource_snapshot
                    .skills
                    .iter()
                    .map(|skill| skill.name.clone())
                    .collect(),
                selected: 0,
            }),
        };
    }

    fn settings_overlay(&self) -> Overlay {
        Overlay::Selector(SelectorOverlay {
            title: "Settings".into(),
            kind: OverlayKind::Settings,
            options: vec![
                "Model…".into(),
                "Reasoning effort…".into(),
                "Theme…".into(),
                "Prompt templates…".into(),
                "Skills…".into(),
                "Scoped models…".into(),
                format!("Agent mode: {}", self.preferences.agent_mode.as_str()),
                format!("OpenAI Fast: {}", on_off(self.preferences.fast_mode)),
                format!("Fullscreen: {}", on_off(self.preferences.fullscreen)),
                format!("Auto-compact: {}", on_off(self.preferences.auto_compaction)),
                format!("Steering mode: {}", self.preferences.steering_mode.as_str()),
                format!("Show images: {}", on_off(self.preferences.show_images)),
                format!(
                    "Auto-resize images: {}",
                    on_off(self.preferences.auto_resize_images)
                ),
                format!("Block images: {}", on_off(self.preferences.block_images)),
                format!(
                    "Follow-up mode: {}",
                    self.preferences.follow_up_mode.as_str()
                ),
                format!(
                    "Autocomplete rows: {}",
                    self.preferences.autocomplete_max_visible
                ),
                format!(
                    "Tree filter: {}",
                    self.preferences.tree_filter_mode.as_str()
                ),
                format!(
                    "Hardware cursor: {}",
                    on_off(self.preferences.show_hardware_cursor)
                ),
                format!("Editor padding: {}", self.preferences.editor_padding_x),
                format!(
                    "Terminal progress: {}",
                    on_off(self.preferences.show_terminal_progress)
                ),
                format!("Warnings: {}", on_off(self.preferences.show_warnings)),
            ],
            selected: 0,
        })
    }

    fn model_selector_overlay(&self) -> Overlay {
        Overlay::Selector(SelectorOverlay {
            title: "Models".into(),
            kind: OverlayKind::ModelSelector,
            options: self.models.clone(),
            selected: selected_index(&self.models, self.selected_model.as_deref()),
        })
    }

    fn session_selector_overlay(&self, title: &str, kind: OverlayKind) -> Overlay {
        Overlay::Selector(SelectorOverlay {
            title: title.into(),
            kind,
            options: self.sessions.clone(),
            selected: selected_index(&self.sessions, self.selected_session.as_deref()),
        })
    }

    fn theme_selector_overlay(&self) -> Overlay {
        let mut options = ThemeName::selector_options();
        options.extend(
            self.resource_snapshot
                .themes
                .iter()
                .map(|theme| theme.name.clone()),
        );
        Overlay::Selector(SelectorOverlay {
            title: "Themes".into(),
            kind: OverlayKind::ThemeSelector,
            selected: selected_index(&options, Some(self.theme_label())),
            options,
        })
    }

    #[must_use]
    pub fn theme_label(&self) -> &str {
        self.preferences
            .custom_theme
            .as_deref()
            .unwrap_or_else(|| self.preferences.theme.label())
    }

    #[allow(
        clippy::too_many_lines,
        reason = "the exhaustive event-to-transcript mapping keeps preference gates auditable"
    )]
    pub fn apply_stream_event(&mut self, event: StreamEvent) {
        match event {
            StreamEvent::RunStarted => {
                self.current_activity = Some("Thinking…".into());
            }
            StreamEvent::TextDelta(text) => {
                self.current_activity = Some("Writing…".into());
                self.active_assistant
                    .get_or_insert_with(String::new)
                    .push_str(&text);
            }
            StreamEvent::Thinking(text) => {
                if let Some(summary) = compact_thinking(&text) {
                    self.transcript.push(TranscriptEntry {
                        role: TranscriptRole::Thinking,
                        text: summary,
                    });
                }
            }
            StreamEvent::RetryStarted {
                attempt,
                max_attempts,
                delay_ms,
            } => {
                if self.preferences.show_terminal_progress {
                    self.current_activity = Some(format!(
                        "Retrying request {attempt}/{max_attempts} in {delay_ms} ms…"
                    ));
                }
            }
            StreamEvent::RetryFinished {
                success,
                attempt,
                final_error,
            } => {
                if self.preferences.show_terminal_progress {
                    if success {
                        self.current_activity =
                            Some(format!("Request recovered on attempt {attempt}"));
                    } else {
                        self.transcript.push(TranscriptEntry {
                            role: TranscriptRole::Warning,
                            text: format!(
                                "Request retry {attempt} failed: {}",
                                final_error.unwrap_or_else(|| "unknown error".into())
                            ),
                        });
                    }
                }
            }
            StreamEvent::Completed(text) => {
                self.current_activity = None;
                let final_text = if text.is_empty() {
                    self.active_assistant.take().unwrap_or_default()
                } else {
                    self.active_assistant = None;
                    text
                };
                if !final_text.is_empty() {
                    self.transcript.push(TranscriptEntry {
                        role: TranscriptRole::Assistant,
                        text: final_text,
                    });
                }
            }
            StreamEvent::Failed(message) => {
                self.active_assistant = None;
                self.current_activity = None;
                self.transcript.push(TranscriptEntry {
                    role: TranscriptRole::Error,
                    text: message,
                });
            }
            StreamEvent::BudgetPaused(message) => {
                self.active_assistant = None;
                self.current_activity = None;
                self.transcript.push(TranscriptEntry {
                    role: TranscriptRole::Warning,
                    text: message,
                });
            }
            StreamEvent::UserInputRequested(request) => self.open_clarification(request),
            StreamEvent::ExtensionUi { extension, request } => {
                self.transcript.push(TranscriptEntry {
                    role: TranscriptRole::System,
                    text: format!("extension {extension}: {}", extension_ui_summary(&request)),
                });
            }
            StreamEvent::ExtensionRendered { custom_type, lines } => {
                self.transcript.push(TranscriptEntry {
                    role: TranscriptRole::System,
                    text: format!("{custom_type}: {}", lines.join("\n")),
                });
            }
            StreamEvent::SessionMetadata { summary } => {
                self.transcript.push(TranscriptEntry {
                    role: TranscriptRole::System,
                    text: summary,
                });
            }
            StreamEvent::Images(mime_types) => {
                if !self.preferences.show_images {
                    return;
                }
                let text = if self.preferences.block_images {
                    format!("[{} image(s) blocked by TUI settings]", mime_types.len())
                } else {
                    let display = if self.preferences.auto_resize_images {
                        "fit-to-terminal"
                    } else {
                        "original-size"
                    };
                    format!("[image: {} ({display})]", mime_types.join(", "))
                };
                self.transcript.push(TranscriptEntry {
                    role: TranscriptRole::Assistant,
                    text,
                });
            }
            StreamEvent::Activity(text) => {
                if self.preferences.show_terminal_progress {
                    self.current_activity = Some(text);
                }
            }
            StreamEvent::ToolFinished {
                name,
                summary,
                failed,
            } => {
                self.current_activity = Some("Thinking…".into());
                self.transcript.push(TranscriptEntry {
                    role: if failed {
                        TranscriptRole::Warning
                    } else {
                        TranscriptRole::Tool
                    },
                    text: format!("{} · {summary}", humanize_tool_name(&name)),
                });
            }
            StreamEvent::Warning(text) => {
                if self.preferences.show_warnings {
                    self.transcript.push(TranscriptEntry {
                        role: TranscriptRole::Warning,
                        text,
                    });
                }
            }
            StreamEvent::BashStarted {
                command,
                exclude_from_context,
            } => {
                let suffix = if exclude_from_context {
                    " (excluded from model context)"
                } else {
                    ""
                };
                self.transcript.push(TranscriptEntry {
                    role: TranscriptRole::System,
                    text: format!("$ {command}{suffix}"),
                });
            }
            StreamEvent::BashFinished {
                output,
                exit_code,
                cancelled,
                truncated,
                timed_out,
                full_output_path,
                error,
            } => {
                if !output.is_empty() {
                    self.transcript.push(TranscriptEntry {
                        role: TranscriptRole::System,
                        text: output,
                    });
                }
                let mut status = if let Some(error) = error {
                    format!("bash failed: {error}")
                } else if cancelled {
                    "bash cancelled".into()
                } else if timed_out {
                    "bash timed out".into()
                } else if let Some(exit_code) = exit_code {
                    format!("bash exited with code {exit_code}")
                } else {
                    "bash ended without an exit code".into()
                };
                if truncated {
                    status.push_str(" (output truncated)");
                }
                if let Some(path) = full_output_path {
                    write!(status, "; full output: {path}")
                        .expect("writing to a String cannot fail");
                }
                self.transcript.push(TranscriptEntry {
                    role: TranscriptRole::System,
                    text: status,
                });
            }
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "keyboard dispatch keeps all mutually exclusive overlay interactions explicit"
    )]
    pub fn apply_key(&mut self, event: KeyEvent) {
        if let Some(action) = self
            .bindings
            .iter()
            .find(|binding| binding.key == event)
            .map(|binding| binding.action)
        {
            self.apply_action(action);
            return;
        }

        match (&mut self.overlay, &event.code) {
            (
                Overlay::Login { .. } | Overlay::Logout { .. } | Overlay::McpLogin { .. },
                KeyCode::Enter,
            ) => {
                self.submit_auth_overlay();
                return;
            }
            (
                Overlay::Login { input, .. }
                | Overlay::Logout { input, .. }
                | Overlay::McpLogin { input, .. },
                KeyCode::Backspace,
            ) => {
                input.pop();
                return;
            }
            (
                Overlay::Login { input, .. }
                | Overlay::Logout { input, .. }
                | Overlay::McpLogin { input, .. },
                KeyCode::Char(ch),
            ) if !event.ctrl && !event.alt => {
                input.push(*ch);
                return;
            }
            _ => {}
        }

        match (&mut self.overlay, &event.code) {
            (
                Overlay::Clarification {
                    request,
                    selected,
                    input,
                },
                KeyCode::Backspace,
            ) if *selected == request.options.len() => {
                input.pop();
                return;
            }
            (
                Overlay::Clarification {
                    request,
                    selected,
                    input,
                },
                KeyCode::Char(ch),
            ) if !event.ctrl && !event.alt => {
                *selected = request.options.len();
                input.push(*ch);
                return;
            }
            _ => {}
        }

        match (&self.overlay, &event.code) {
            (
                Overlay::Selector(_)
                | Overlay::WorkspacePermission { .. }
                | Overlay::Clarification { .. },
                KeyCode::Up,
            ) => {
                self.apply_action(Action::SelectPrev);
            }
            (
                Overlay::Selector(_)
                | Overlay::WorkspacePermission { .. }
                | Overlay::Clarification { .. },
                KeyCode::Down,
            ) => {
                self.apply_action(Action::SelectNext);
            }
            (
                Overlay::Selector(_)
                | Overlay::Confirm { .. }
                | Overlay::WorkspacePermission { .. }
                | Overlay::Clarification { .. },
                KeyCode::Enter,
            ) => {
                self.apply_action(Action::Confirm);
            }
            (
                Overlay::Selector(_)
                | Overlay::Help
                | Overlay::Hotkeys
                | Overlay::Login { .. }
                | Overlay::Logout { .. }
                | Overlay::McpLogin { .. }
                | Overlay::Confirm { .. }
                | Overlay::WorkspacePermission { .. }
                | Overlay::Clarification { .. },
                KeyCode::Esc,
            ) => {
                self.apply_action(Action::CloseOverlay);
            }
            (Overlay::None, KeyCode::Enter) => self.apply_action(Action::SubmitPrompt),
            (Overlay::None, KeyCode::Char('p')) if event.ctrl => self.cycle_scoped_model(),
            (Overlay::None, KeyCode::Up) => self.apply_action(Action::HistoryPrev),
            (Overlay::None, KeyCode::Down) => self.apply_action(Action::HistoryNext),
            (Overlay::None, KeyCode::Left) => self.apply_action(Action::CursorLeft),
            (Overlay::None, KeyCode::Right) => self.apply_action(Action::CursorRight),
            (Overlay::None, KeyCode::Backspace) => self.apply_action(Action::Backspace),
            (Overlay::None, KeyCode::Delete) => self.apply_action(Action::DeleteForward),
            (Overlay::None, KeyCode::Tab) => self.open_autocomplete(),
            (Overlay::None, KeyCode::Char(ch)) if !event.ctrl && !event.alt => {
                self.insert_char(*ch);
            }
            _ => {}
        }
    }

    pub fn apply_action(&mut self, action: Action) {
        match action {
            Action::OpenOverlay(kind) => self.open_overlay(kind),
            Action::CloseOverlay => {
                if let Overlay::WorkspacePermission { request, .. } = &self.overlay {
                    self.pending_tui_action = Some(TuiAction::WorkspacePermission {
                        request: request.clone(),
                        decision: ApprovalDecision::Deny,
                    });
                }
                self.overlay = Overlay::None;
            }
            Action::Confirm => self.confirm_overlay(),
            Action::SelectNext => self.move_selection(1),
            Action::SelectPrev => self.move_selection(-1),
            Action::SubmitPrompt => self.submit_prompt(),
            Action::HistoryPrev => self.history_prev(),
            Action::HistoryNext => self.history_next(),
            Action::CursorLeft => self.cursor_chars = self.cursor_chars.saturating_sub(1),
            Action::CursorRight => {
                self.cursor_chars = self
                    .cursor_chars
                    .saturating_add(1)
                    .min(self.prompt.chars().count());
            }
            Action::Backspace => self.remove_prev_char(),
            Action::DeleteForward => self.remove_next_char(),
        }
    }

    #[must_use]
    pub fn render(&self, size: TerminalSize, options: RenderOptions) -> String {
        render(self, size, options)
    }

    #[must_use]
    pub const fn should_quit(&self) -> bool {
        self.should_quit
    }

    fn submit_prompt(&mut self) {
        let prompt = std::mem::take(&mut self.prompt);
        self.cursor_chars = 0;
        self.history_index = None;
        self.history_draft = None;
        let trimmed = prompt.trim().to_owned();
        if trimmed.is_empty() && self.pending_images.is_empty() {
            return;
        }
        if !trimmed.is_empty() {
            self.history.push(trimmed.clone());
        }
        if let Some(rest) = trimmed.strip_prefix('/') {
            let split = rest.find(char::is_whitespace).unwrap_or(rest.len());
            let name = &rest[..split];
            if self.preferences.agent_mode != AgentMode::Plan
                && self
                    .extension_commands
                    .iter()
                    .any(|command| command == name)
            {
                self.pending_tui_action = Some(TuiAction::ExtensionCommand {
                    name: name.to_owned(),
                    args: rest[split..].trim().to_owned(),
                });
                return;
            }
        }
        if let Some(command) = parse_slash_command(&trimmed) {
            self.handle_slash_command(command);
            return;
        }
        if self.preferences.agent_mode == AgentMode::Plan
            && trimmed.eq_ignore_ascii_case("implement")
            && self.pending_images.is_empty()
        {
            self.pending_tui_action = Some(TuiAction::ImplementPlan { instructions: None });
            return;
        }
        self.submitted_images = std::mem::take(&mut self.pending_images);
        self.pending_submission = Some(if trimmed.is_empty() {
            String::new()
        } else {
            expand_prompt_template(&trimmed, &self.resource_snapshot.prompt_templates)
        });
    }

    #[allow(
        clippy::too_many_lines,
        reason = "the exhaustive typed slash-command router keeps every visible command mapping in one place"
    )]
    fn handle_slash_command(&mut self, command: SlashCommand) {
        match command {
            SlashCommand::Login { provider } => {
                self.overlay = Overlay::Login {
                    provider,
                    input: String::new(),
                };
            }
            SlashCommand::Logout { provider } => {
                self.overlay = Overlay::Logout {
                    provider,
                    input: String::new(),
                };
            }
            SlashCommand::Model { model } => {
                if let Some(model) = model {
                    self.select_or_filter_model(&model);
                } else {
                    self.open_overlay(OverlayKind::ModelSelector);
                }
            }
            SlashCommand::Mode { mode } => {
                if let Some(mode) = mode {
                    self.preferences.agent_mode = mode;
                    self.pending_tui_action = Some(TuiAction::SetAgentMode(mode));
                } else {
                    self.push_system_message(format!(
                        "Mode: {}. Plan permits inspection, clarification, and the plan artifact only; default asks before model-issued shell commands; auto runs shell commands and workspace edits immediately.",
                        self.preferences.agent_mode.as_str()
                    ));
                }
            }
            SlashCommand::Implement { instructions } => {
                self.pending_tui_action = Some(TuiAction::ImplementPlan { instructions });
            }
            SlashCommand::Session { .. } => {
                self.pending_tui_action = Some(TuiAction::ShowSessionInfo);
            }
            SlashCommand::Sessions { session } => {
                if let Some(session) = session {
                    if self.sessions.contains(&session) {
                        self.selected_session = Some(session.clone());
                        self.pending_tui_action = Some(TuiAction::Resume { session });
                    } else {
                        self.push_system_message(format!("session does not exist: {session}"));
                    }
                } else {
                    self.open_overlay(OverlayKind::SessionSelector);
                }
            }
            SlashCommand::Effort { level } => {
                if let Some(level) = level {
                    self.selected_effort = level;
                    self.pending_tui_action = Some(TuiAction::SetEffort(level));
                } else {
                    self.open_overlay(OverlayKind::EffortSelector);
                }
            }
            SlashCommand::Fast => {
                self.preferences.fast_mode = !self.preferences.fast_mode;
                self.pending_tui_action = Some(TuiAction::ToggleFast);
            }
            SlashCommand::ScopedModels => self.open_overlay(OverlayKind::ScopedModelsSelector),
            SlashCommand::Resume { session } => {
                if let Some(session) = session {
                    if self.sessions.contains(&session) {
                        self.selected_session = Some(session.clone());
                        self.pending_tui_action = Some(TuiAction::Resume { session });
                    } else {
                        self.push_system_message(format!("session does not exist: {session}"));
                    }
                } else {
                    self.open_overlay(OverlayKind::ResumeSelector);
                }
            }
            SlashCommand::New { name, prompt } => {
                self.pending_tui_action = Some(TuiAction::NewSession { name, prompt });
            }
            SlashCommand::Name { name } => {
                self.pending_tui_action = Some(TuiAction::SetSessionName { name });
            }
            SlashCommand::Tree => self.pending_tui_action = Some(TuiAction::ShowSessionTree),
            SlashCommand::Fork => self.pending_tui_action = Some(TuiAction::Fork),
            SlashCommand::Clone => self.pending_tui_action = Some(TuiAction::Clone),
            SlashCommand::Compact { instructions } => {
                self.pending_tui_action = Some(TuiAction::Compact { instructions });
            }
            SlashCommand::Refine { arguments } => {
                self.pending_tui_action = Some(TuiAction::Refine { arguments });
            }
            SlashCommand::Copy => self.pending_tui_action = Some(TuiAction::CopyLastMessage),
            SlashCommand::SideQuestion { question } => {
                self.pending_tui_action = Some(TuiAction::SideQuestion { question });
            }
            SlashCommand::Export { path } => {
                self.pending_tui_action = Some(TuiAction::ExportSession { path });
            }
            SlashCommand::Import { path } => {
                self.open_confirm(
                    "Import session",
                    format!("Replace the current session with {path}?"),
                    TuiAction::ImportSession { path },
                );
            }
            SlashCommand::Share => self.pending_tui_action = Some(TuiAction::ShareSession),
            SlashCommand::Hotkeys => self.open_overlay(OverlayKind::Hotkeys),
            SlashCommand::Changelog => {
                self.pending_tui_action = Some(TuiAction::ShowChangelog);
            }
            SlashCommand::SystemPrompt => {
                self.pending_tui_action = Some(TuiAction::ShowSystemPrompt);
            }
            SlashCommand::Logs => self.pending_tui_action = Some(TuiAction::ShowLogs),
            SlashCommand::Update { arguments } => {
                self.pending_tui_action = Some(TuiAction::Update { arguments });
            }
            SlashCommand::RlmMaxDepth { arguments } => {
                self.pending_tui_action = Some(TuiAction::RlmMaxDepth { arguments });
            }
            SlashCommand::Fullscreen { enabled } => {
                self.preferences.fullscreen = enabled.unwrap_or(!self.preferences.fullscreen);
                self.pending_tui_action = Some(TuiAction::Fullscreen {
                    enabled: Some(self.preferences.fullscreen),
                });
            }
            SlashCommand::Traces { arguments } => {
                self.pending_tui_action = Some(TuiAction::Traces { arguments });
            }
            SlashCommand::Heartbeat { arguments } => {
                self.pending_tui_action = Some(TuiAction::Heartbeat { arguments });
            }
            SlashCommand::Heartbeats => {
                self.pending_tui_action = Some(TuiAction::ListHeartbeats);
            }
            SlashCommand::Goal { arguments } => {
                self.pending_tui_action = Some(TuiAction::Goal { arguments });
            }
            SlashCommand::Autonomous { arguments } => {
                self.pending_tui_action = Some(TuiAction::Autonomous { arguments });
            }
            SlashCommand::Reload => self.pending_tui_action = Some(TuiAction::Reload),
            SlashCommand::Context => self.pending_tui_action = Some(TuiAction::ShowContext),
            SlashCommand::Mcp { command } => {
                self.pending_tui_action = Some(TuiAction::Mcp(command));
            }
            SlashCommand::Settings => self.open_overlay(OverlayKind::Settings),
            SlashCommand::Theme { theme } => {
                if let Some(theme) = theme {
                    self.preferences.theme = theme;
                    self.preferences.custom_theme = None;
                    self.pending_tui_action = Some(TuiAction::SetTheme);
                } else {
                    self.open_overlay(OverlayKind::ThemeSelector);
                }
            }
            SlashCommand::Help => self.open_overlay(OverlayKind::Help),
            SlashCommand::Quit => self.should_quit = true,
            SlashCommand::Invalid { message } => self.push_system_message(message),
        }
    }

    fn submit_auth_overlay(&mut self) {
        match &mut self.overlay {
            Overlay::Login { provider, input } => {
                if provider.is_none() {
                    let value = std::mem::take(input).trim().to_owned();
                    if !value.is_empty() {
                        *provider = Some(value);
                    }
                    return;
                }
                let provider = provider.take().unwrap_or_default();
                let secret = std::mem::take(input);
                self.pending_ui_request = Some(UiRequest::Login {
                    provider,
                    secret: (!secret.is_empty()).then_some(secret),
                });
                self.overlay = Overlay::None;
            }
            Overlay::Logout { provider, input } => {
                let selected = provider
                    .take()
                    .or_else(|| (!input.trim().is_empty()).then(|| input.trim().to_owned()));
                if let Some(provider) = selected {
                    self.pending_ui_request = Some(UiRequest::Logout { provider });
                    self.overlay = Overlay::None;
                }
            }
            Overlay::McpLogin { server, input } => {
                let api_key = std::mem::take(input);
                if !api_key.trim().is_empty() {
                    self.pending_ui_request = Some(UiRequest::McpApiKey {
                        server: server.clone(),
                        api_key,
                    });
                    self.overlay = Overlay::None;
                }
            }
            Overlay::None
            | Overlay::Help
            | Overlay::Hotkeys
            | Overlay::Selector(_)
            | Overlay::Confirm { .. }
            | Overlay::WorkspacePermission { .. }
            | Overlay::Clarification { .. } => {}
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "overlay confirmation keeps each mutually exclusive UI state transition explicit"
    )]
    fn confirm_overlay(&mut self) {
        if let Overlay::Confirm { action, .. } = &self.overlay {
            self.pending_tui_action = Some((**action).clone());
            self.overlay = Overlay::None;
            return;
        }
        if let Overlay::WorkspacePermission { request, selected } = &self.overlay {
            let decision = match selected {
                0 => ApprovalDecision::AllowOnce,
                1 => ApprovalDecision::AlwaysAllowWorkspace,
                _ => ApprovalDecision::Deny,
            };
            self.pending_tui_action = Some(TuiAction::WorkspacePermission {
                request: request.clone(),
                decision,
            });
            self.overlay = Overlay::None;
            return;
        }
        if let Overlay::Clarification {
            request,
            selected,
            input,
        } = &self.overlay
        {
            let answer = if *selected < request.options.len() {
                request.options[*selected].label.clone()
            } else {
                input.trim().to_owned()
            };
            if !answer.is_empty() {
                self.history.push(answer.clone());
                self.pending_submission = Some(answer);
                self.overlay = Overlay::None;
            }
            return;
        }
        let Overlay::Selector(selector) = &self.overlay else {
            self.overlay = Overlay::None;
            return;
        };
        let selection = selector.options.get(selector.selected).cloned();
        let heartbeat_selection = self.heartbeat_entries.get(selector.selected).cloned();
        match selector.kind {
            OverlayKind::EffortSelector => {
                if let Some(level) = selection.as_deref().and_then(parse_thinking_level) {
                    self.selected_effort = level;
                    self.pending_tui_action = Some(TuiAction::SetEffort(level));
                }
            }
            OverlayKind::ForkSelector => {
                if let Some(entry_id) = self.fork_entries.get(selector.selected).copied() {
                    self.pending_tui_action = Some(TuiAction::ForkAt { entry_id });
                }
            }
            OverlayKind::TreeSelector => {
                if let Some(entry_id) = self.tree_entries.get(selector.selected).copied() {
                    self.pending_tui_action = Some(TuiAction::ContinueAt { entry_id });
                }
            }
            OverlayKind::ModelSelector => self.selected_model = selection,
            OverlayKind::ResumeSelector | OverlayKind::SessionSelector => {
                if let Some(session) = selection {
                    self.selected_session = Some(session.clone());
                    self.pending_tui_action = Some(TuiAction::Resume { session });
                }
            }
            OverlayKind::HeartbeatSelector => {
                if self.confirm_heartbeat_selector(heartbeat_selection) {
                    return;
                }
            }
            OverlayKind::HeartbeatActionSelector => {
                self.confirm_heartbeat_action(selection.as_deref());
            }
            OverlayKind::ScopedModelsSelector => {
                if let Some(model) = selection {
                    let scoped_models = self
                        .preferences
                        .scoped_models
                        .get_or_insert_with(BTreeSet::new);
                    if !scoped_models.remove(&model) {
                        scoped_models.insert(model);
                    }
                    self.pending_tui_action = Some(TuiAction::ConfigureScopedModels);
                    return;
                }
            }
            OverlayKind::ThemeSelector => {
                if let Some(selection) = selection {
                    if let Some(theme) = ThemeName::parse(&selection) {
                        self.preferences.theme = theme;
                        self.preferences.custom_theme = None;
                    } else if self
                        .resource_snapshot
                        .themes
                        .iter()
                        .any(|theme| theme.name == selection)
                    {
                        self.preferences.custom_theme = Some(selection);
                    }
                    self.pending_tui_action = Some(TuiAction::SetTheme);
                }
            }
            OverlayKind::PromptTemplateSelector => {
                if let Some(template) = selection {
                    self.set_prompt(format!("/{template} "));
                }
            }
            OverlayKind::SkillSelector => {
                if let Some(skill) = selection {
                    self.set_prompt(format!("/skill:{skill} "));
                }
            }
            OverlayKind::Autocomplete => {
                if let Some(command) = selection {
                    self.set_prompt(format!("{command} "));
                }
            }
            OverlayKind::Settings => {
                if self.confirm_settings(selector.selected) {
                    return;
                }
            }
            OverlayKind::Help | OverlayKind::Hotkeys | OverlayKind::Login => {}
        }
        self.overlay = Overlay::None;
    }

    fn confirm_heartbeat_selector(&mut self, target: Option<(Uuid, String, bool)>) -> bool {
        let Some(target @ (_, _, paused)) = target else {
            return false;
        };
        self.heartbeat_target = Some(target);
        self.overlay = Overlay::Selector(SelectorOverlay {
            title: "Manage heartbeat".into(),
            kind: OverlayKind::HeartbeatActionSelector,
            options: if paused {
                vec!["Resume".into(), "Stop".into()]
            } else {
                vec!["Pause".into(), "Stop".into()]
            },
            selected: 0,
        });
        true
    }

    fn confirm_heartbeat_action(&mut self, selection: Option<&str>) {
        let (Some((id, session, _)), Some(selection)) = (self.heartbeat_target.take(), selection)
        else {
            return;
        };
        let action = match selection {
            "Pause" => HeartbeatManagementAction::Pause,
            "Resume" => HeartbeatManagementAction::Resume,
            "Stop" => HeartbeatManagementAction::Stop,
            _ => return,
        };
        self.pending_tui_action = Some(TuiAction::ManageHeartbeat {
            session,
            id,
            action,
        });
    }

    fn confirm_settings(&mut self, selected: usize) -> bool {
        let overlay = match selected {
            0 => Some(OverlayKind::ModelSelector),
            1 => Some(OverlayKind::EffortSelector),
            2 => Some(OverlayKind::ThemeSelector),
            3 => Some(OverlayKind::PromptTemplateSelector),
            4 => Some(OverlayKind::SkillSelector),
            5 => Some(OverlayKind::ScopedModelsSelector),
            _ => None,
        };
        if let Some(overlay) = overlay {
            self.open_overlay(overlay);
            return true;
        }
        match selected {
            6 => {
                self.preferences.agent_mode = match self.preferences.agent_mode {
                    AgentMode::Default => AgentMode::Plan,
                    AgentMode::Plan => AgentMode::Auto,
                    AgentMode::Auto => AgentMode::Default,
                };
                self.pending_tui_action =
                    Some(TuiAction::SetAgentMode(self.preferences.agent_mode));
            }
            7 => {
                self.preferences.fast_mode = !self.preferences.fast_mode;
                self.pending_tui_action = Some(TuiAction::ToggleFast);
            }
            8 => {
                self.preferences.fullscreen = !self.preferences.fullscreen;
                self.pending_tui_action = Some(TuiAction::Fullscreen {
                    enabled: Some(self.preferences.fullscreen),
                });
            }
            9 => {
                self.preferences.auto_compaction = !self.preferences.auto_compaction;
                self.pending_tui_action = Some(TuiAction::SetAutoCompaction {
                    enabled: self.preferences.auto_compaction,
                });
            }
            10 => {
                self.preferences.steering_mode = match self.preferences.steering_mode {
                    QueueMode::All => QueueMode::OneAtATime,
                    QueueMode::OneAtATime => QueueMode::All,
                };
                self.pending_tui_action = Some(TuiAction::SetSteeringMode {
                    mode: self.preferences.steering_mode,
                });
            }
            11 => self.preferences.show_images = !self.preferences.show_images,
            12 => self.preferences.auto_resize_images = !self.preferences.auto_resize_images,
            13 => self.preferences.block_images = !self.preferences.block_images,
            14 => {
                self.preferences.follow_up_mode = match self.preferences.follow_up_mode {
                    QueueMode::All => QueueMode::OneAtATime,
                    QueueMode::OneAtATime => QueueMode::All,
                };
            }
            15 => {
                self.preferences.autocomplete_max_visible =
                    if self.preferences.autocomplete_max_visible >= 20 {
                        3
                    } else {
                        self.preferences.autocomplete_max_visible.saturating_add(1)
                    };
            }
            16 => self.preferences.tree_filter_mode = self.preferences.tree_filter_mode.next(),
            17 => {
                self.preferences.show_hardware_cursor = !self.preferences.show_hardware_cursor;
            }
            18 => {
                self.preferences.editor_padding_x =
                    self.preferences.editor_padding_x.saturating_add(1) % 9;
            }
            19 => {
                self.preferences.show_terminal_progress = !self.preferences.show_terminal_progress;
            }
            20 => self.preferences.show_warnings = !self.preferences.show_warnings,
            _ => {}
        }
        if (11..=20).contains(&selected) {
            self.pending_tui_action = Some(TuiAction::PersistSettings);
        }
        false
    }

    fn move_selection(&mut self, delta: isize) {
        if let Overlay::WorkspacePermission { selected, .. } = &mut self.overlay {
            *selected = ((*selected).cast_signed() + delta).rem_euclid(3) as usize;
            return;
        }
        if let Overlay::Clarification {
            request, selected, ..
        } = &mut self.overlay
        {
            let last = request.options.len();
            if delta.is_negative() {
                *selected = selected.saturating_sub(delta.unsigned_abs());
            } else {
                *selected = selected.saturating_add(delta.unsigned_abs()).min(last);
            }
            return;
        }
        let Overlay::Selector(selector) = &mut self.overlay else {
            return;
        };
        if selector.options.is_empty() {
            return;
        }
        if delta.is_negative() {
            selector.selected = selector.selected.saturating_sub(delta.unsigned_abs());
        } else {
            selector.selected = selector
                .selected
                .saturating_add(delta.unsigned_abs())
                .min(selector.options.len().saturating_sub(1));
        }
    }

    fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        match self.history_index {
            None => {
                self.history_draft = Some(self.prompt.clone());
                self.history_index = Some(self.history.len().saturating_sub(1));
            }
            Some(index) if index > 0 => self.history_index = Some(index - 1),
            Some(_) => {}
        }
        self.load_history();
    }

    fn history_next(&mut self) {
        let Some(index) = self.history_index else {
            return;
        };
        if index + 1 < self.history.len() {
            self.history_index = Some(index + 1);
            self.load_history();
        } else {
            self.history_index = None;
            self.prompt = self.history_draft.take().unwrap_or_default();
            self.cursor_chars = self.prompt.chars().count();
        }
    }

    fn load_history(&mut self) {
        if let Some(index) = self.history_index {
            self.prompt = self.history[index].clone();
            self.cursor_chars = self.prompt.chars().count();
        }
    }

    fn cycle_scoped_model(&mut self) {
        let enabled = self
            .models
            .iter()
            .filter(|model| self.scoped_models().contains(*model))
            .collect::<Vec<_>>();
        if enabled.is_empty() {
            self.push_system_message("No scoped models are enabled. Run /scoped-models.");
            return;
        }
        let current = self
            .selected_model
            .as_ref()
            .and_then(|selected| enabled.iter().position(|model| *model == selected));
        let next = current.map_or(0, |index| (index + 1) % enabled.len());
        self.selected_model = Some((*enabled[next]).clone());
    }

    fn insert_char(&mut self, ch: char) {
        let byte = byte_index(&self.prompt, self.cursor_chars);
        self.prompt.insert(byte, ch);
        self.cursor_chars += 1;
    }

    fn remove_prev_char(&mut self) {
        if self.cursor_chars == 0 {
            return;
        }
        let current = byte_index(&self.prompt, self.cursor_chars);
        let previous = byte_index(&self.prompt, self.cursor_chars - 1);
        self.prompt.replace_range(previous..current, "");
        self.cursor_chars -= 1;
    }

    fn remove_next_char(&mut self) {
        if self.cursor_chars >= self.prompt.chars().count() {
            return;
        }
        let start = byte_index(&self.prompt, self.cursor_chars);
        let end = byte_index(&self.prompt, self.cursor_chars + 1);
        self.prompt.replace_range(start..end, "");
    }
}

fn humanize_tool_name(name: &str) -> String {
    name.replace('_', " ")
        .split_whitespace()
        .map(|word| {
            let mut chars = word.chars();
            chars.next().map_or_else(String::new, |first| {
                first.to_uppercase().chain(chars).collect()
            })
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn compact_thinking(text: &str) -> Option<String> {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        return None;
    }
    let mut summary = normalized.chars().take(240).collect::<String>();
    if normalized.chars().count() > 240 {
        summary.push('…');
    }
    Some(summary)
}

const fn on_off(enabled: bool) -> &'static str {
    if enabled { "on" } else { "off" }
}

fn selected_index(options: &[String], selected: Option<&str>) -> usize {
    selected
        .and_then(|value| options.iter().position(|item| item == value))
        .unwrap_or(0)
}

fn extension_ui_summary(request: &crate::extensions::UiRequest) -> String {
    match request {
        crate::extensions::UiRequest::Notify { level, message } => {
            format!("{level}: {message}")
        }
        crate::extensions::UiRequest::Input { prompt, .. } => prompt.clone(),
        crate::extensions::UiRequest::Confirm { title, message, .. } => {
            format!("{title}: {message}")
        }
        crate::extensions::UiRequest::Select { title, options, .. } => {
            format!("{title} [{}]", options.join(", "))
        }
        crate::extensions::UiRequest::SetStatus { key, text } => {
            format!("{key}: {}", text.as_deref().unwrap_or("cleared"))
        }
    }
}

fn byte_index(text: &str, char_index: usize) -> usize {
    text.char_indices()
        .nth(char_index)
        .map_or(text.len(), |(index, _)| index)
}

fn parse_thinking_level(value: &str) -> Option<ThinkingLevel> {
    ThinkingLevel::ALL
        .into_iter()
        .find(|level| level.as_str() == value)
}
