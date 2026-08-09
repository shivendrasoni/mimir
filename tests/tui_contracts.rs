use std::sync::Arc;

use mimir::model::{Message, ThinkingLevel};
use mimir::tui::{
    Action, App, AppConfig, AppPreferenceState, InputBinding, KeyCode, KeyEvent, McpCommand,
    Overlay, OverlayKind, RenderOptions, SlashCommand, StreamEvent, TerminalCapabilities,
    TerminalSize, ThemeName, TreeFilterMode, TuiAction, UiRequest, dispatch_persistent_action,
    dispatch_runtime_action, export_tui_session, handle_ui_request, parse_slash_command,
};
use mimir::{
    mcp::{McpAuthCoordinator, McpCatalogServer, McpCatalogStdio, McpServerCatalog},
    orchestration::HeartbeatManagementAction,
    provider::FakeProvider,
    resources::{CustomTheme, PromptTemplate, Skill},
    runtime::{AgentRuntime, QueueMode, RuntimeConfig},
    session::{
        FileSessionStore, InMemorySessionStore, SessionPayload, SessionRecord, SessionStore,
    },
    tools::{ToolPolicy, ToolRegistry},
};
use tempfile::TempDir;
use uuid::Uuid;

#[test]
fn parses_supported_slash_commands_and_arguments() {
    assert_eq!(
        parse_slash_command("/login"),
        Some(SlashCommand::Login { provider: None })
    );
    assert_eq!(
        parse_slash_command("/logout"),
        Some(SlashCommand::Logout { provider: None })
    );
    assert_eq!(
        parse_slash_command("/model gpt-5-mini"),
        Some(SlashCommand::Model {
            model: Some("gpt-5-mini".into()),
        })
    );
    assert_eq!(
        parse_slash_command("/session"),
        Some(SlashCommand::Session { session: None })
    );
    assert_eq!(
        parse_slash_command("/sessions review-42"),
        Some(SlashCommand::Sessions {
            session: Some("review-42".into()),
        })
    );
    assert!(matches!(
        parse_slash_command("/session review-42"),
        Some(SlashCommand::Invalid { .. })
    ));
    assert_eq!(
        parse_slash_command("/theme light"),
        Some(SlashCommand::Theme {
            theme: Some(ThemeName::Light),
        })
    );
    assert_eq!(
        parse_slash_command("/settings"),
        Some(SlashCommand::Settings)
    );
    assert_eq!(parse_slash_command("/help"), Some(SlashCommand::Help));
    assert_eq!(parse_slash_command("/quit"), Some(SlashCommand::Quit));
    assert_eq!(parse_slash_command("not a command"), None);
}

#[tokio::test]
async fn mcp_api_key_request_persists_through_the_typed_coordinator() {
    let state = TempDir::new().expect("state");
    let catalog = McpServerCatalog::new(state.path()).expect("catalog");
    catalog
        .upsert(
            McpCatalogServer::new(
                "linear",
                "Linear",
                McpCatalogStdio {
                    program: "/bin/sh".into(),
                    ..McpCatalogStdio::default()
                },
            )
            .expect("server"),
        )
        .await
        .expect("persist server");
    let message = handle_ui_request(
        state.path(),
        UiRequest::McpApiKey {
            server: "linear".into(),
            api_key: "mcp-test-secret".into(),
        },
    )
    .await
    .expect("store API key");
    assert_eq!(message, "Connected MCP server linear");
    let status = McpAuthCoordinator::new(state.path())
        .expect("coordinator")
        .status("linear")
        .await
        .expect("status")
        .expect("configured server");
    assert!(status.auth.enabled);
}

#[test]
fn parses_reference_session_reasoning_and_mcp_commands_into_typed_variants() {
    assert_eq!(
        parse_slash_command("/effort high"),
        Some(SlashCommand::Effort {
            level: Some(ThinkingLevel::High),
        })
    );
    assert_eq!(
        parse_slash_command("/resume session-42"),
        Some(SlashCommand::Resume {
            session: Some("session-42".into()),
        })
    );
    assert_eq!(
        parse_slash_command("/new --name 'Review session' -- inspect the diff"),
        Some(SlashCommand::New {
            name: Some("Review session".into()),
            prompt: Some("inspect the diff".into()),
        })
    );
    assert_eq!(
        parse_slash_command("/name release review"),
        Some(SlashCommand::Name {
            name: Some("release review".into()),
        })
    );
    assert_eq!(parse_slash_command("/tree"), Some(SlashCommand::Tree));
    assert_eq!(parse_slash_command("/fork"), Some(SlashCommand::Fork));
    assert_eq!(
        parse_slash_command("/clear"),
        Some(SlashCommand::New {
            name: None,
            prompt: None,
        })
    );
    assert_eq!(parse_slash_command("/clone"), Some(SlashCommand::Clone));
    assert_eq!(
        parse_slash_command("/compact keep decisions and file operations"),
        Some(SlashCommand::Compact {
            instructions: Some("keep decisions and file operations".into()),
        })
    );
    assert_eq!(parse_slash_command("/reload"), Some(SlashCommand::Reload));
    assert_eq!(parse_slash_command("/usage"), Some(SlashCommand::Context));
    assert_eq!(parse_slash_command("/context"), Some(SlashCommand::Context));
    assert_eq!(
        parse_slash_command("/mcp login linear"),
        Some(SlashCommand::Mcp {
            command: McpCommand::Login {
                server: "linear".into(),
            },
        })
    );
    assert!(matches!(
        parse_slash_command("/effort impossible"),
        Some(SlashCommand::Invalid { .. })
    ));
    assert!(matches!(
        parse_slash_command("/mcp login"),
        Some(SlashCommand::Invalid { .. })
    ));
    assert!(matches!(
        parse_slash_command("/new --name 'review'-- inspect"),
        Some(SlashCommand::Invalid { .. })
    ));
}

#[test]
fn parses_extended_reference_commands_and_side_alias() {
    assert_eq!(
        parse_slash_command("/refine --global retain provider lessons"),
        Some(SlashCommand::Refine {
            arguments: Some("--global retain provider lessons".into()),
        })
    );
    assert_eq!(parse_slash_command("/copy"), Some(SlashCommand::Copy));
    for command in ["/btw is the build green?", "/side is the build green?"] {
        assert_eq!(
            parse_slash_command(command),
            Some(SlashCommand::SideQuestion {
                question: "is the build green?".into(),
            })
        );
    }
    assert_eq!(
        parse_slash_command("/export exports/review.jsonl"),
        Some(SlashCommand::Export {
            path: Some("exports/review.jsonl".into()),
        })
    );
    assert_eq!(parse_slash_command("/share"), Some(SlashCommand::Share));
    assert_eq!(parse_slash_command("/hotkeys"), Some(SlashCommand::Hotkeys));
    assert_eq!(
        parse_slash_command("/changelog"),
        Some(SlashCommand::Changelog)
    );
    assert_eq!(
        parse_slash_command("/traces preview"),
        Some(SlashCommand::Traces {
            arguments: Some("preview".into()),
        })
    );
    assert_eq!(
        parse_slash_command("/heartbeat every 10m inspect the queue"),
        Some(SlashCommand::Heartbeat {
            arguments: Some("every 10m inspect the queue".into()),
        })
    );
    assert_eq!(
        parse_slash_command("/heartbeats"),
        Some(SlashCommand::Heartbeats)
    );
    assert_eq!(
        parse_slash_command("/goal --budget 100 finish migration"),
        Some(SlashCommand::Goal {
            arguments: Some("--budget 100 finish migration".into()),
        })
    );
    assert_eq!(
        parse_slash_command("/autonomous on"),
        Some(SlashCommand::Autonomous {
            arguments: Some("on".into()),
        })
    );
    assert!(matches!(
        parse_slash_command("/btw"),
        Some(SlashCommand::Invalid { .. })
    ));
}

#[test]
fn parses_remaining_reference_runtime_and_local_commands() {
    assert_eq!(parse_slash_command("/fast"), Some(SlashCommand::Fast));
    assert_eq!(
        parse_slash_command("/scoped-models"),
        Some(SlashCommand::ScopedModels)
    );
    assert_eq!(
        parse_slash_command("/import '/tmp/review session.jsonl'"),
        Some(SlashCommand::Import {
            path: "/tmp/review session.jsonl".into()
        })
    );
    assert_eq!(
        parse_slash_command("/system-prompt"),
        Some(SlashCommand::SystemPrompt)
    );
    assert_eq!(parse_slash_command("/logs"), Some(SlashCommand::Logs));
    assert_eq!(
        parse_slash_command("/update check"),
        Some(SlashCommand::Update {
            arguments: Some("check".into())
        })
    );
    assert_eq!(
        parse_slash_command("/rlm-max-depth 4 --global"),
        Some(SlashCommand::RlmMaxDepth {
            arguments: Some("4 --global".into())
        })
    );
    assert_eq!(
        parse_slash_command("/fullscreen off"),
        Some(SlashCommand::Fullscreen {
            enabled: Some(false)
        })
    );
    assert!(matches!(
        parse_slash_command("/fullscreen maybe"),
        Some(SlashCommand::Invalid { .. })
    ));
}

#[test]
fn scoped_models_are_interactive_and_drive_ctrl_p_cycling() {
    let mut app = App::new(AppConfig::default());
    app.set_models(vec!["openai/gpt-5".into(), "anthropic/claude".into()]);
    app.select_model("openai/gpt-5");

    submit_command(&mut app, "/scoped-models");
    assert!(matches!(
        app.overlay(),
        Overlay::Selector(selector)
            if selector.kind == OverlayKind::ScopedModelsSelector
    ));
    app.apply_key(KeyEvent::plain(KeyCode::Enter));
    assert_eq!(
        app.take_tui_action(),
        Some(TuiAction::ConfigureScopedModels)
    );
    app.apply_key(KeyEvent::plain(KeyCode::Esc));
    app.apply_key(KeyEvent {
        code: KeyCode::Char('p'),
        ctrl: true,
        alt: false,
        shift: false,
    });
    assert_eq!(app.selected_model(), Some("anthropic/claude"));
}

#[test]
fn settings_overlay_is_an_interactive_runtime_selector() {
    let mut app = App::new(AppConfig::default());
    app.set_models(vec!["openai/gpt-5".into()]);
    submit_command(&mut app, "/settings");
    assert!(matches!(
        app.overlay(),
        Overlay::Selector(selector)
            if selector.kind == OverlayKind::Settings && selector.options.len() >= 8
    ));
    for _ in 0..6 {
        app.apply_key(KeyEvent::plain(KeyCode::Down));
    }
    app.apply_key(KeyEvent::plain(KeyCode::Enter));
    assert_eq!(app.take_tui_action(), Some(TuiAction::ToggleFast));
    assert!(app.fast_mode());
}

#[test]
fn model_search_session_stats_and_explicit_session_selection_are_distinct() {
    let mut app = App::new(AppConfig::default());
    app.set_models(vec![
        "openai/gpt-5".into(),
        "anthropic/claude-sonnet".into(),
        "anthropic/claude-opus".into(),
    ]);
    app.set_sessions(vec!["alpha".into(), "beta".into()]);

    submit_command(&mut app, "/model sonnet");
    assert!(matches!(
        app.overlay(),
        Overlay::Selector(selector)
            if selector.kind == OverlayKind::ModelSelector
                && selector.options == vec!["anthropic/claude-sonnet"]
    ));
    app.apply_key(KeyEvent::plain(KeyCode::Enter));
    assert_eq!(app.selected_model(), Some("anthropic/claude-sonnet"));

    submit_command(&mut app, "/session");
    assert_eq!(app.take_tui_action(), Some(TuiAction::ShowSessionInfo));
    submit_command(&mut app, "/sessions beta");
    assert_eq!(app.selected_session(), Some("beta"));
    assert_eq!(
        app.take_tui_action(),
        Some(TuiAction::Resume {
            session: "beta".into()
        })
    );
}

#[test]
fn import_requires_confirmation_before_replacing_the_session() {
    let mut app = App::new(AppConfig::default());
    submit_command(&mut app, "/import /tmp/session.jsonl");
    assert!(matches!(app.overlay(), Overlay::Confirm { .. }));
    assert_eq!(app.take_tui_action(), None);
    app.apply_key(KeyEvent::plain(KeyCode::Enter));
    assert_eq!(
        app.take_tui_action(),
        Some(TuiAction::ImportSession {
            path: "/tmp/session.jsonl".into(),
        })
    );
}

#[test]
fn heartbeat_selector_exposes_bounded_lifecycle_actions() {
    let mut app = App::new(AppConfig::default());
    let id = Uuid::new_v4();
    app.open_heartbeat_selector(vec![(
        id,
        "alpha".into(),
        false,
        "alpha [active] every 5m — inspect".into(),
    )]);
    app.apply_key(KeyEvent::plain(KeyCode::Enter));
    assert!(matches!(
        app.overlay(),
        Overlay::Selector(selector)
            if selector.kind == OverlayKind::HeartbeatActionSelector
                && selector.options == vec!["Pause", "Stop"]
    ));
    app.apply_key(KeyEvent::plain(KeyCode::Enter));
    assert_eq!(
        app.take_tui_action(),
        Some(TuiAction::ManageHeartbeat {
            session: "alpha".into(),
            id,
            action: HeartbeatManagementAction::Pause,
        })
    );
}

#[test]
fn extended_reference_commands_emit_actions_and_render_hotkeys() {
    let mut app = App::new(AppConfig::default());
    submit_command(&mut app, "/copy");
    assert_eq!(app.take_tui_action(), Some(TuiAction::CopyLastMessage));
    submit_command(&mut app, "/side why did the test fail?");
    assert_eq!(
        app.take_tui_action(),
        Some(TuiAction::SideQuestion {
            question: "why did the test fail?".into(),
        })
    );
    submit_command(&mut app, "/goal pause");
    assert_eq!(
        app.take_tui_action(),
        Some(TuiAction::Goal {
            arguments: Some("pause".into()),
        })
    );
    submit_command(&mut app, "/heartbeat status");
    assert_eq!(
        app.take_tui_action(),
        Some(TuiAction::Heartbeat {
            arguments: Some("status".into()),
        })
    );
    submit_command(&mut app, "/hotkeys");
    assert_eq!(app.overlay(), &Overlay::Hotkeys);
    let rendered = app.render(
        TerminalSize {
            width: 100,
            height: 24,
        },
        RenderOptions {
            capabilities: TerminalCapabilities::plain(),
        },
    );
    assert!(rendered.contains("Keyboard shortcuts"));
    assert!(rendered.contains("Ctrl+C cancel run"));
    assert!(rendered.contains("Ctrl+D quit"));
}

#[tokio::test]
async fn goal_and_heartbeat_tui_actions_use_durable_stores() {
    let state = TempDir::new().expect("state");

    let created = dispatch_persistent_action(
        state.path(),
        "session-a",
        &TuiAction::Goal {
            arguments: Some("--budget 100 finish migration".into()),
        },
    )
    .await
    .expect("create goal")
    .expect("goal response");
    assert_eq!(created, "Goal active: finish migration");
    let paused = dispatch_persistent_action(
        state.path(),
        "session-a",
        &TuiAction::Goal {
            arguments: Some("pause".into()),
        },
    )
    .await
    .expect("pause goal")
    .expect("goal response");
    assert_eq!(paused, "Goal paused: finish migration");

    let heartbeat = dispatch_persistent_action(
        state.path(),
        "session-a",
        &TuiAction::Heartbeat {
            arguments: Some("every 10m --follow-up inspect the queue".into()),
        },
    )
    .await
    .expect("set heartbeat")
    .expect("heartbeat response");
    assert!(heartbeat.contains("Heartbeat set:"));
    assert!(heartbeat.contains("every 10m"));
    assert!(heartbeat.contains("follow-up"));

    let catalog = dispatch_persistent_action(state.path(), "session-a", &TuiAction::ListHeartbeats)
        .await
        .expect("list heartbeat")
        .expect("heartbeat response");
    assert!(catalog.contains("session-a [active] every 10m: inspect the queue"));

    let paused = dispatch_persistent_action(
        state.path(),
        "session-a",
        &TuiAction::Heartbeat {
            arguments: Some("pause".into()),
        },
    )
    .await
    .expect("pause heartbeat")
    .expect("heartbeat response");
    assert_eq!(paused, "Heartbeat paused: inspect the queue");
}

#[tokio::test]
async fn tui_export_defaults_to_safe_standalone_html_and_supports_jsonl() {
    let state = TempDir::new().expect("state");
    let store = FileSessionStore::create(state.path(), "session-a")
        .await
        .expect("session store");
    store
        .append(SessionRecord::new(SessionPayload::Message(Message::user(
            "inspect <unsafe> & report",
        ))))
        .await
        .expect("append message");

    let html_message = export_tui_session(state.path(), "session-a", None)
        .await
        .expect("HTML export");
    assert!(html_message.contains("exports/session-a.html"));
    let html = tokio::fs::read_to_string(state.path().join("exports/session-a.html"))
        .await
        .expect("read HTML");
    assert!(html.contains("inspect &lt;unsafe&gt; &amp; report"));
    assert!(!html.contains("inspect <unsafe>"));

    export_tui_session(state.path(), "session-a", Some("exports/session-a.jsonl"))
        .await
        .expect("JSONL export");
    let jsonl = tokio::fs::read_to_string(state.path().join("exports/session-a.jsonl"))
        .await
        .expect("read JSONL");
    assert!(
        jsonl
            .lines()
            .next()
            .unwrap_or_default()
            .contains("\"version\":3")
    );
    assert!(jsonl.contains("inspect <unsafe> & report"));

    let traversal = export_tui_session(state.path(), "session-a", Some("../escape.html"))
        .await
        .expect_err("path traversal");
    assert!(traversal.to_string().contains("relative .html or .jsonl"));
}

#[test]
fn reference_commands_update_tui_state_and_emit_typed_actions() {
    let mut app = App::new(AppConfig::default());
    app.set_sessions(vec!["alpha".into(), "beta".into()]);

    submit_command(&mut app, "/effort max");
    assert_eq!(app.selected_effort(), ThinkingLevel::Max);
    assert_eq!(
        app.take_tui_action(),
        Some(TuiAction::SetEffort(ThinkingLevel::Max))
    );

    submit_command(&mut app, "/resume beta");
    assert_eq!(app.selected_session(), Some("beta"));
    assert_eq!(
        app.take_tui_action(),
        Some(TuiAction::Resume {
            session: "beta".into(),
        })
    );

    submit_command(&mut app, "/new --name sprint -- start review");
    assert_eq!(
        app.take_tui_action(),
        Some(TuiAction::NewSession {
            name: Some("sprint".into()),
            prompt: Some("start review".into()),
        })
    );
    submit_command(&mut app, "/name release");
    assert_eq!(
        app.take_tui_action(),
        Some(TuiAction::SetSessionName {
            name: Some("release".into()),
        })
    );
    submit_command(&mut app, "/tree");
    assert_eq!(app.take_tui_action(), Some(TuiAction::ShowSessionTree));
    submit_command(&mut app, "/fork");
    assert_eq!(app.take_tui_action(), Some(TuiAction::Fork));
    submit_command(&mut app, "/clone");
    assert_eq!(app.take_tui_action(), Some(TuiAction::Clone));
    submit_command(&mut app, "/compact preserve filenames");
    assert_eq!(
        app.take_tui_action(),
        Some(TuiAction::Compact {
            instructions: Some("preserve filenames".into()),
        })
    );
    submit_command(&mut app, "/reload");
    assert_eq!(app.take_tui_action(), Some(TuiAction::Reload));
    submit_command(&mut app, "/context");
    assert_eq!(app.take_tui_action(), Some(TuiAction::ShowContext));
    submit_command(&mut app, "/mcp logout notion");
    assert_eq!(
        app.take_tui_action(),
        Some(TuiAction::Mcp(McpCommand::Logout {
            server: "notion".into(),
        }))
    );
}

#[test]
fn registered_extension_commands_emit_typed_coordinator_actions() {
    let mut app = App::new(AppConfig::default());
    app.set_extension_commands(vec!["fixture".into()]);

    submit_command(&mut app, "/fixture inspect --deep");

    assert_eq!(
        app.take_tui_action(),
        Some(TuiAction::ExtensionCommand {
            name: "fixture".into(),
            args: "inspect --deep".into(),
        })
    );
}

#[test]
fn effort_and_resume_without_arguments_use_typed_selectors() {
    let mut app = App::new(AppConfig::default());
    app.set_sessions(vec!["alpha".into(), "beta".into()]);

    submit_command(&mut app, "/effort");
    assert!(matches!(
        app.overlay(),
        Overlay::Selector(selector) if selector.kind == OverlayKind::EffortSelector
    ));
    app.apply_key(KeyEvent::plain(KeyCode::Down));
    app.apply_key(KeyEvent::plain(KeyCode::Enter));
    assert_eq!(app.selected_effort(), ThinkingLevel::Minimal);
    assert_eq!(
        app.take_tui_action(),
        Some(TuiAction::SetEffort(ThinkingLevel::Minimal))
    );

    submit_command(&mut app, "/resume");
    assert!(matches!(
        app.overlay(),
        Overlay::Selector(selector) if selector.kind == OverlayKind::ResumeSelector
    ));
    app.apply_key(KeyEvent::plain(KeyCode::Down));
    app.apply_key(KeyEvent::plain(KeyCode::Enter));
    assert_eq!(
        app.take_tui_action(),
        Some(TuiAction::Resume {
            session: "beta".into(),
        })
    );
}

#[test]
fn fork_selector_emits_the_selected_session_tree_entry() {
    let mut app = App::new(AppConfig::default());
    let first = Uuid::new_v4();
    let second = Uuid::new_v4();
    app.open_fork_selector(vec![
        (first, "first prompt".into()),
        (second, "second prompt".into()),
    ]);
    app.apply_key(KeyEvent::plain(KeyCode::Down));
    app.apply_key(KeyEvent::plain(KeyCode::Enter));
    assert_eq!(
        app.take_tui_action(),
        Some(TuiAction::ForkAt { entry_id: second })
    );
}

#[test]
fn tree_selector_emits_continuation_and_mcp_keys_remain_masked() {
    let mut app = App::new(AppConfig::default());
    let first = Uuid::new_v4();
    let second = Uuid::new_v4();
    app.open_tree_selector(vec![
        (first, "first entry".into()),
        (second, "second entry".into()),
    ]);
    app.apply_key(KeyEvent::plain(KeyCode::Enter));
    assert_eq!(
        app.take_tui_action(),
        Some(TuiAction::ContinueAt { entry_id: second })
    );

    app.open_mcp_login("linear");
    for character in "mcp-secret".chars() {
        app.apply_key(KeyEvent::plain(KeyCode::Char(character)));
    }
    let rendered = app.render(
        TerminalSize {
            width: 80,
            height: 24,
        },
        RenderOptions {
            capabilities: TerminalCapabilities::plain(),
        },
    );
    assert!(!rendered.contains("mcp-secret"));
    assert!(rendered.contains("••••••••••"));
    app.apply_key(KeyEvent::plain(KeyCode::Enter));
    assert_eq!(
        app.take_ui_request(),
        Some(UiRequest::McpApiKey {
            server: "linear".into(),
            api_key: "mcp-secret".into(),
        })
    );
}

#[tokio::test]
async fn runtime_owned_tui_actions_are_dispatched_instead_of_dropped() {
    let workspace = TempDir::new().expect("workspace");
    let tools =
        ToolRegistry::with_default_tools(workspace.path(), ToolPolicy::default()).expect("tools");
    let mut config = RuntimeConfig::default_for_model("test-model");
    config.supported_thinking_levels = ThinkingLevel::ALL.to_vec();
    let runtime = AgentRuntime::resume(
        Arc::new(FakeProvider::default()),
        Arc::new(tools),
        Arc::new(InMemorySessionStore::default()),
        config,
    )
    .await
    .expect("runtime");

    let message = dispatch_runtime_action(&runtime, &TuiAction::SetEffort(ThinkingLevel::High))
        .await
        .expect("dispatch")
        .expect("runtime-owned action");
    assert_eq!(message, "Reasoning effort set to high");
    assert_eq!(runtime.model_selection().await.2, ThinkingLevel::High);

    dispatch_runtime_action(&runtime, &TuiAction::SetAutoCompaction { enabled: false })
        .await
        .expect("auto-compaction")
        .expect("runtime-owned action");
    assert!(!runtime.auto_compaction_enabled());
    dispatch_runtime_action(
        &runtime,
        &TuiAction::SetSteeringMode {
            mode: QueueMode::All,
        },
    )
    .await
    .expect("steering mode")
    .expect("runtime-owned action");
    assert_eq!(runtime.steering_mode().await, QueueMode::All);

    let context = dispatch_runtime_action(&runtime, &TuiAction::ShowContext)
        .await
        .expect("context tree")
        .expect("runtime-owned action");
    assert!(context.contains("Context Tree"));
    assert!(context.contains("Own/total usage"));
    assert!(context.contains("Children: none"));
}

#[test]
fn editable_prompt_history_and_custom_bindings_are_pure_state_transitions() {
    let mut app = App::new(AppConfig {
        bindings: vec![InputBinding::new(
            KeyEvent::plain(KeyCode::F(2)),
            Action::OpenOverlay(OverlayKind::Help),
        )],
    });

    app.apply_key(KeyEvent::plain(KeyCode::Char('h')));
    app.apply_key(KeyEvent::plain(KeyCode::Char('i')));
    app.apply_key(KeyEvent::plain(KeyCode::Enter));
    assert_eq!(app.pending_submission(), Some("hi".into()));
    assert_eq!(app.prompt(), "");

    app.clear_pending_submission();
    app.apply_key(KeyEvent::plain(KeyCode::Char('b')));
    app.apply_key(KeyEvent::plain(KeyCode::Char('y')));
    app.apply_key(KeyEvent::plain(KeyCode::Char('e')));
    app.apply_key(KeyEvent::plain(KeyCode::Up));
    assert_eq!(app.prompt(), "hi");
    app.apply_key(KeyEvent::plain(KeyCode::Down));
    assert_eq!(app.prompt(), "bye");

    app.apply_key(KeyEvent::plain(KeyCode::F(2)));
    assert_eq!(app.overlay(), &Overlay::Help);
}

#[test]
fn selectors_and_overlays_switch_and_confirm_choices() {
    let mut app = App::new(AppConfig::default());
    app.set_models(vec!["gpt-5-mini".into(), "gpt-5".into()]);
    app.set_sessions(vec!["alpha".into(), "beta".into()]);

    app.open_overlay(OverlayKind::ModelSelector);
    assert!(matches!(app.overlay(), Overlay::Selector(selector) if selector.title == "Models"));
    app.apply_key(KeyEvent::plain(KeyCode::Down));
    app.apply_key(KeyEvent::plain(KeyCode::Enter));
    assert_eq!(app.selected_model(), Some("gpt-5"));

    app.open_overlay(OverlayKind::SessionSelector);
    app.apply_key(KeyEvent::plain(KeyCode::Down));
    app.apply_key(KeyEvent::plain(KeyCode::Enter));
    assert_eq!(app.selected_session(), Some("beta"));
    assert_eq!(
        app.take_tui_action(),
        Some(TuiAction::Resume {
            session: "beta".into()
        })
    );

    app.open_overlay(OverlayKind::ThemeSelector);
    app.apply_key(KeyEvent::plain(KeyCode::Down));
    app.apply_key(KeyEvent::plain(KeyCode::Enter));
    assert_eq!(app.theme(), ThemeName::Light);
}

#[test]
fn resource_snapshot_drives_prompt_skill_and_custom_theme_selectors() {
    let root = TempDir::new().expect("resources");
    let mut app = App::new(AppConfig::default());
    app.set_resource_snapshot(mimir::tui::TuiResourceSnapshot {
        prompt_templates: vec![PromptTemplate {
            name: "audit".into(),
            description: "Audit a target".into(),
            argument_hint: Some("<target>".into()),
            content: "Audit $1 carefully".into(),
            path: root.path().join("audit.md"),
        }],
        themes: vec![CustomTheme {
            name: "ocean".into(),
            definition: serde_json::json!({"vars": {"accent": "#0088ff"}}),
            path: root.path().join("ocean.json"),
        }],
        skills: vec![Skill {
            name: "review".into(),
            description: "Review code".into(),
            body: "Review carefully".into(),
            path: root.path().join("review/SKILL.md"),
        }],
    });

    app.set_prompt("/audit src/runtime.rs");
    app.apply_key(KeyEvent::plain(KeyCode::Enter));
    assert_eq!(
        app.pending_submission().as_deref(),
        Some("Audit src/runtime.rs carefully")
    );
    app.clear_pending_submission();

    app.open_overlay(OverlayKind::PromptTemplateSelector);
    app.apply_key(KeyEvent::plain(KeyCode::Enter));
    assert_eq!(app.prompt(), "/audit ");

    app.open_overlay(OverlayKind::SkillSelector);
    app.apply_key(KeyEvent::plain(KeyCode::Enter));
    assert_eq!(app.prompt(), "/skill:review ");

    app.open_overlay(OverlayKind::ThemeSelector);
    app.apply_key(KeyEvent::plain(KeyCode::Down));
    app.apply_key(KeyEvent::plain(KeyCode::Down));
    app.apply_key(KeyEvent::plain(KeyCode::Down));
    app.apply_key(KeyEvent::plain(KeyCode::Enter));
    assert_eq!(app.theme_label(), "ocean");

    let rendered = app.render(
        TerminalSize {
            width: 100,
            height: 24,
        },
        RenderOptions {
            capabilities: TerminalCapabilities::rich_ansi(),
        },
    );
    assert!(rendered.contains("\u{1b}[38;2;0;136;255m"));
}

#[test]
fn invalid_custom_theme_colors_fall_back_without_terminal_escape_injection() {
    let root = TempDir::new().expect("resources");
    let mut app = App::new(AppConfig::default());
    app.set_resource_snapshot(mimir::tui::TuiResourceSnapshot {
        themes: vec![CustomTheme {
            name: "unsafe".into(),
            definition: serde_json::json!({
                "colors": {"accent": "\u{001b}[2Jowned", "error": "not-a-color"}
            }),
            path: root.path().join("unsafe.json"),
        }],
        ..mimir::tui::TuiResourceSnapshot::default()
    });
    app.open_overlay(OverlayKind::ThemeSelector);
    for _ in 0..3 {
        app.apply_key(KeyEvent::plain(KeyCode::Down));
    }
    app.apply_key(KeyEvent::plain(KeyCode::Enter));
    app.push_system_message("untrusted \u{001b}[2Jprovider output");

    let rendered = app.render(
        TerminalSize {
            width: 100,
            height: 24,
        },
        RenderOptions {
            capabilities: TerminalCapabilities::rich_ansi(),
        },
    );
    assert!(!rendered.contains("owned"));
    assert!(!rendered.contains("\u{1b}[2Jowned"));
    assert!(!rendered.contains("\u{1b}[2Jprovider output"));
    assert!(rendered.contains("\u{1b}[38;2;96;165;250m"));
}

#[test]
fn fullscreen_renderer_anchors_prompt_to_bottom_and_uses_raw_mode_safe_rows() {
    let app = App::new(AppConfig::default());
    let size = TerminalSize {
        width: 100,
        height: 12,
    };
    let plain = app.render(
        size,
        RenderOptions {
            capabilities: TerminalCapabilities::plain(),
        },
    );
    let plain_lines = plain.lines().collect::<Vec<_>>();
    assert_eq!(plain_lines.len(), size.height);
    assert!(plain_lines.last().is_some_and(|line| line == &"Prompt: "));

    let ansi = app.render(
        size,
        RenderOptions {
            capabilities: TerminalCapabilities::rich_ansi(),
        },
    );
    let body = ansi
        .strip_prefix(TerminalCapabilities::rich_ansi().screen_prefix())
        .expect("alternate-screen prefix");
    assert!(body.contains("\r\n"));
    assert!(!body.replace("\r\n", "").contains('\n'));
}

#[test]
fn display_preferences_drive_autocomplete_followups_images_progress_and_layout() {
    let mut app = App::new(AppConfig::default());
    app.set_extension_commands(vec!["status".into(), "sync".into(), "summarize".into()]);
    app.set_preferences(AppPreferenceState {
        show_images: true,
        auto_resize_images: true,
        block_images: false,
        follow_up_mode: QueueMode::All,
        autocomplete_max_visible: 3,
        tree_filter_mode: TreeFilterMode::UserOnly,
        show_hardware_cursor: true,
        editor_padding_x: 3,
        show_terminal_progress: true,
        show_warnings: true,
        ..AppPreferenceState::default()
    });

    app.set_prompt("/s");
    app.apply_key(KeyEvent::plain(KeyCode::Tab));
    assert!(matches!(
        app.overlay(),
        Overlay::Selector(selector)
            if selector.kind == OverlayKind::Autocomplete
                && selector.options.len() == 3
    ));
    app.apply_key(KeyEvent::plain(KeyCode::Enter));
    assert!(app.prompt().starts_with('/'));

    app.queue_follow_up("first").expect("first follow-up");
    app.queue_follow_up("second").expect("second follow-up");
    assert_eq!(app.take_next_follow_ups(), vec!["first", "second"]);

    app.apply_stream_event(StreamEvent::Images(vec!["image/png".into()]));
    app.apply_stream_event(StreamEvent::Progress("tool read started".into()));
    app.apply_stream_event(StreamEvent::Warning("extension warning".into()));
    app.set_bash_active(true);
    app.apply_stream_event(StreamEvent::BashStarted {
        command: "pwd".into(),
        exclude_from_context: true,
    });
    app.apply_stream_event(StreamEvent::BashFinished {
        output: "/workspace".into(),
        exit_code: Some(0),
        cancelled: false,
        truncated: false,
        timed_out: false,
        full_output_path: None,
        error: None,
    });
    app.set_bash_active(false);
    assert!(
        app.transcript()
            .iter()
            .any(|entry| entry.text.contains("image/png"))
    );
    assert!(
        app.transcript()
            .iter()
            .any(|entry| entry.text == "tool read started")
    );
    assert!(
        app.transcript()
            .iter()
            .any(|entry| entry.text == "extension warning")
    );
    assert_eq!(app.tree_filter_mode(), TreeFilterMode::UserOnly);
    assert!(app.show_hardware_cursor());
    assert!(!app.bash_active());
    assert!(
        app.transcript()
            .iter()
            .any(|entry| { entry.text.contains("$ pwd (excluded from model context)") })
    );
    assert!(
        app.transcript()
            .iter()
            .any(|entry| entry.text == "/workspace")
    );

    let rendered = app.render(
        TerminalSize {
            width: 100,
            height: 24,
        },
        RenderOptions {
            capabilities: TerminalCapabilities::plain(),
        },
    );
    assert!(rendered.contains("   Prompt: /"));
}

#[test]
fn blocked_or_disabled_runtime_visuals_are_not_rendered_as_if_enabled() {
    let mut app = App::new(AppConfig::default());
    app.set_preferences(AppPreferenceState {
        show_images: false,
        show_terminal_progress: false,
        show_warnings: false,
        ..AppPreferenceState::default()
    });
    app.apply_stream_event(StreamEvent::Images(vec!["image/png".into()]));
    app.apply_stream_event(StreamEvent::Progress("hidden progress".into()));
    app.apply_stream_event(StreamEvent::Warning("hidden warning".into()));
    assert!(app.transcript().is_empty());

    app.set_preferences(AppPreferenceState {
        show_images: true,
        block_images: true,
        ..AppPreferenceState::default()
    });
    app.apply_stream_event(StreamEvent::Images(vec!["image/jpeg".into()]));
    assert_eq!(app.transcript().len(), 1);
    assert!(app.transcript()[0].text.contains("blocked"));
    assert!(!app.transcript()[0].text.contains("image/jpeg"));
}

#[test]
fn stream_events_accumulate_transcript_and_pending_assistant_text() {
    let mut app = App::new(AppConfig::default());
    app.push_user_message("inspect repo");
    app.apply_stream_event(StreamEvent::RunStarted);
    app.apply_stream_event(StreamEvent::TextDelta("Hello".into()));
    app.apply_stream_event(StreamEvent::TextDelta(", world".into()));
    assert_eq!(app.active_assistant_text(), Some("Hello, world"));

    app.apply_stream_event(StreamEvent::Completed("Hello, world".into()));
    assert_eq!(app.active_assistant_text(), None);
    let transcript = app.transcript();
    assert_eq!(transcript.len(), 2);
    assert_eq!(transcript[0].text, "inspect repo");
    assert_eq!(transcript[1].text, "Hello, world");
}

#[test]
fn retry_lifecycle_is_visible_in_the_tui_transcript() {
    let mut app = App::new(AppConfig::default());
    app.apply_stream_event(StreamEvent::RetryStarted {
        attempt: 1,
        max_attempts: 3,
        delay_ms: 2000,
    });
    app.apply_stream_event(StreamEvent::RetryFinished {
        success: true,
        attempt: 1,
        final_error: None,
    });

    assert_eq!(app.transcript().len(), 2);
    assert_eq!(
        app.transcript()[0].text,
        "retrying provider request 1/3 in 2000 ms"
    );
    assert_eq!(app.transcript()[1].text, "provider retry 1 succeeded");
}

#[test]
fn renderer_is_resize_safe_and_has_plaintext_fallback() {
    let mut app = App::new(AppConfig::default());
    app.push_user_message("first line");
    app.apply_stream_event(StreamEvent::Completed("second line".into()));
    app.open_overlay(OverlayKind::Help);

    let ansi = app.render(
        TerminalSize {
            width: 28,
            height: 8,
        },
        RenderOptions {
            capabilities: TerminalCapabilities::rich_ansi(),
        },
    );
    assert!(ansi.contains("\u{1b}[?1049h"));
    assert!(ansi.contains("Help"));
    assert!(ansi.lines().count() <= 12);

    let plain = app.render(
        TerminalSize {
            width: 18,
            height: 5,
        },
        RenderOptions {
            capabilities: TerminalCapabilities::plain(),
        },
    );
    assert!(!plain.contains("\u{1b}["));
    assert!(plain.contains("Prompt"));
    assert!(plain.lines().count() <= 5);
}

#[test]
fn login_overlay_masks_api_keys_and_emits_a_typed_request() {
    let mut app = App::new(AppConfig::default());
    for character in "/login openai".chars() {
        app.apply_key(KeyEvent::plain(KeyCode::Char(character)));
    }
    app.apply_key(KeyEvent::plain(KeyCode::Enter));
    for character in "secret-value".chars() {
        app.apply_key(KeyEvent::plain(KeyCode::Char(character)));
    }
    let rendered = app.render(
        TerminalSize {
            width: 80,
            height: 24,
        },
        RenderOptions {
            capabilities: TerminalCapabilities::plain(),
        },
    );
    assert!(!rendered.contains("secret-value"));
    assert!(rendered.contains("••••••••••••"));

    app.apply_key(KeyEvent::plain(KeyCode::Enter));
    assert_eq!(
        app.take_ui_request(),
        Some(UiRequest::Login {
            provider: "openai".into(),
            secret: Some("secret-value".into()),
        })
    );
}

fn submit_command(app: &mut App, command: &str) {
    for character in command.chars() {
        app.apply_key(KeyEvent::plain(KeyCode::Char(character)));
    }
    app.apply_key(KeyEvent::plain(KeyCode::Enter));
}
