mod actions;
mod app;
mod autonomous;
mod clipboard;
mod commands;
mod driver;
mod input;
mod render;
mod side_question;
mod term;

pub use actions::TuiAction;
pub use app::{
    App, AppConfig, AppPreferenceState, Overlay, OverlayKind, SelectorOverlay, StreamEvent,
    ThemeName, TranscriptEntry, TreeFilterMode, TuiResourceSnapshot, UiRequest,
};
pub use autonomous::{
    AutonomousGateCommand, AutonomousLimitReason, AutonomousLimits, AutonomousState,
    DEFAULT_AUTONOMOUS_CONTINUATION_PROMPT, ProcessExecutionPolicy,
};
pub use commands::{McpCommand, SlashCommand, parse_slash_command};
pub use driver::{
    RlmMaxDepthStatus, TuiRuntimeFactory, dispatch_persistent_action, dispatch_runtime_action,
    export_tui_session, handle_ui_request, load_tui_fast_mode, load_tui_rlm_max_depth,
    load_tui_rlm_max_depth_status, preview_tui_share, preview_tui_traces, run_tui,
    run_tui_with_autonomous, set_tui_rlm_max_depth,
};
pub use input::{Action, InputBinding, KeyCode, KeyEvent};
pub use render::{RenderOptions, TerminalSize};
pub use side_question::{
    SideQuestionSession, SideQuestionTurn, ask_side_question, ask_side_question_cancellable,
};
pub use term::TerminalCapabilities;
