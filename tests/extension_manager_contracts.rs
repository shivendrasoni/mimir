use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
};

use mimir::{
    auth::AuthStore,
    budget::Budget,
    extensions::{ExtensionCatalog, ExtensionManager, LifecycleEvent, RuntimeLimits},
    model::{Content, Message, ModelResponse, StopReason, ThinkingLevel, ToolCall},
    provider::FakeProvider,
    runtime::{AgentRuntime, RuntimeConfig, RuntimeEvent, VecEventSink},
    session::InMemorySessionStore,
    tools::{ToolPolicy, ToolRegistry},
};
use serde_json::json;
use tempfile::TempDir;

fn compile_fixture(root: &Path) -> PathBuf {
    let source = root.join("manager_fixture.rs");
    let binary = root.join("manager-fixture");
    std::fs::write(&source, r##"
use std::io::{self, Read};
fn field(input: &str, marker: &str, end: char) -> String {
    let start = input.find(marker).expect("field") + marker.len();
    let tail = &input[start..];
    tail[..tail.find(end).expect("end")].to_owned()
}

fn main() {
    let mut input = String::new();
    io::stdin().read_to_string(&mut input).expect("stdin");
    let id = field(&input, "\"id\":\"", '"');
    let generation = field(&input, "\"generation\":", ',');
    let response = if input.contains("\"type\":\"initialize\"") {
        r#"{"type":"registration","registrations":{"tools":[{"name":"fixture_tool","label":"Fixture","description":"fixture tool","parameters":{"type":"object"}}],"commands":[{"name":"fixture_command","description":"fixture command"}],"ui_requests":["notify","confirm"],"renderers":[{"custom_type":"fixture_message"}],"providers":[{"name":"fixture_provider","transport":"open_ai_compatible","models":["fixture-model"],"base_url":"https://example.invalid/v1"}],"lifecycle_events":["turn_start"]}}"#.to_owned()
    } else if input.contains("\"type\":\"tool\"") {
        r#"{"type":"tool","result":{"status":"ok","summary":"tool complete","content":{"value":42},"next_actions":[],"ui_requests":[{"kind":"notify","level":"info","message":"tool ui"}]}}"#.to_owned()
    } else if input.contains("\"type\":\"command\"") {
        r#"{"type":"command","result":{"message":"command complete","output":{"ran":true},"ui_requests":[{"kind":"confirm","id":"confirm-1","title":"Confirm","message":"Continue?"}]}}"#.to_owned()
    } else if input.contains("\"type\":\"render\"") {
        r#"{"type":"render","output":{"lines":["rendered fixture message"]}}"#.to_owned()
    } else if input.contains("\"type\":\"ui_response\"") {
        r#"{"type":"ui_response","accepted":true}"#.to_owned()
    } else {
        r#"{"type":"lifecycle","outcome":{"cancel":false,"output":{"event":true},"ui_requests":[{"kind":"notify","level":"info","message":"event ui"}]}}"#.to_owned()
    };
    println!(r#"{{"schema_version":1,"id":"{id}","status":"ok","output":{{"abi_version":1,"generation":{generation},"response":{response}}}}}"#);
}
"##).expect("source");
    let status = Command::new("rustc")
        .args(["--edition=2024", "-o"])
        .arg(&binary)
        .arg(&source)
        .status()
        .expect("rustc");
    assert!(status.success());
    let mut permissions = std::fs::metadata(&binary).expect("metadata").permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&binary, permissions).expect("permissions");
    binary
}

fn write_manifest(workspace: &Path, binary: &Path) {
    let path = workspace.join(".mimir/extensions/fixture/manifest.json");
    std::fs::create_dir_all(path.parent().expect("parent")).expect("directory");
    std::fs::write(
        path,
        serde_json::to_vec(&json!({
            "schema_version": 1,
            "name": "fixture",
            "version": "1.0.0",
            "entrypoint": {"program": binary, "args": []},
            "capabilities": [
                "tools", "commands", "ui", "provider", "lifecycle", "process",
                "unrestricted_native"
            ]
        }))
        .expect("manifest"),
    )
    .expect("write");
}

#[tokio::test]
async fn manager_materializes_and_executes_registered_extension_surfaces() {
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    let fixture = TempDir::new().expect("fixture");
    let binary = compile_fixture(fixture.path());
    write_manifest(workspace.path(), &binary);
    let mut catalog = ExtensionCatalog::new(workspace.path(), state.path()).expect("catalog");
    let auth = AuthStore::new(state.path()).expect("auth store");
    let manager = Arc::new(
        ExtensionManager::load_with_auth_store(
            catalog.reload().await.expect("catalog entries"),
            workspace.path(),
            state.path(),
            RuntimeLimits::default(),
            auth.clone(),
        )
        .await
        .expect("manager"),
    );

    assert_eq!(manager.tools()[0].name, "fixture_tool");
    assert_eq!(manager.commands()[0].name, "fixture_command");
    assert_eq!(manager.providers()[0].name, "fixture_provider");
    auth.set_api_key("fixture_provider", "fixture-secret")
        .await
        .expect("provider credential");
    manager
        .activate_provider("fixture_provider", "fixture-model")
        .await
        .expect("activate extension provider without network I/O");

    let mut tools =
        ToolRegistry::with_default_tools(workspace.path(), ToolPolicy::default()).expect("tools");
    tools
        .register_extension_manager(&manager)
        .expect("materialize tools");
    let observation = tools
        .execute("fixture_tool", json!({"question":"answer"}))
        .await
        .expect("tool invocation");
    assert_eq!(observation.content, "{\"value\":42}");
    assert_eq!(manager.drain_ui_requests().await.len(), 1);

    let command = manager
        .invoke_command("fixture_command", "hello")
        .await
        .expect("command");
    assert_eq!(command.output, json!({"ran":true}));
    let wrong = manager
        .respond_ui("confirm-1", json!("yes"))
        .await
        .expect_err("confirm response must be boolean");
    assert!(wrong.to_string().contains("does not match"));
    manager
        .respond_ui("confirm-1", json!(true))
        .await
        .expect("correlated UI response");
    let replay = manager
        .respond_ui("confirm-1", json!(true))
        .await
        .expect_err("response id is single-use");
    assert!(replay.to_string().contains("unknown or expired"));
    let render = manager
        .render_message("fixture_message", json!({"content":"hello"}), false)
        .await
        .expect("renderer");
    assert_eq!(render.lines, vec!["rendered fixture message"]);

    let dispatched = manager
        .dispatch(LifecycleEvent::TurnStart {
            session_id: "session-1".into(),
            turn_index: 1,
        })
        .await
        .expect("lifecycle");
    assert_eq!(dispatched.len(), 1);
    assert_eq!(dispatched[0].outcome.ui_requests.len(), 1);
}

#[tokio::test]
async fn agent_runtime_dispatches_lifecycle_and_surfaces_extension_ui() {
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    let fixture = TempDir::new().expect("fixture");
    let binary = compile_fixture(fixture.path());
    write_manifest(workspace.path(), &binary);
    let mut catalog = ExtensionCatalog::new(workspace.path(), state.path()).expect("catalog");
    let manager = Arc::new(
        ExtensionManager::load(
            catalog.reload().await.expect("entries"),
            workspace.path(),
            state.path(),
            RuntimeLimits::default(),
        )
        .await
        .expect("manager"),
    );
    let mut tools =
        ToolRegistry::with_default_tools(workspace.path(), ToolPolicy::default()).expect("tools");
    tools
        .register_extension_manager(&manager)
        .expect("extension tools");
    let mut tool_message = Message::assistant(
        vec![Content::ToolCall(ToolCall {
            id: "call-1".into(),
            name: "fixture_tool".into(),
            arguments: json!({}),
        })],
        StopReason::ToolUse,
    );
    tool_message.usage.input_tokens = 1;
    let final_message = Message::assistant(
        vec![Content::Text {
            text: "done".into(),
        }],
        StopReason::Stop,
    );
    let provider = Arc::new(FakeProvider::new(vec![
        ModelResponse {
            message: tool_message,
            response_id: None,
        },
        ModelResponse {
            message: final_message,
            response_id: None,
        },
    ]));
    let config = RuntimeConfig {
        provider: "fake".into(),
        model: "fake-model".into(),
        thinking_level: ThinkingLevel::Off,
        supported_thinking_levels: vec![ThinkingLevel::Off],
        thinking_level_map: None,
        system_prompt: String::new(),
        budget: Budget::default(),
        provider_aware_token_budget: false,
        provider_timeout: std::time::Duration::from_secs(2),
        typesafe: mimir::typesafe::TypeSafeConfig::default(),
    };
    let runtime = AgentRuntime::resume(
        provider,
        Arc::new(tools),
        Arc::new(InMemorySessionStore::default()),
        config,
    )
    .await
    .expect("runtime");
    runtime.attach_extension_manager(manager, "session-1").await;
    let sink = VecEventSink::default();
    assert_eq!(runtime.run("hello", &sink).await.expect("run"), "done");
    assert!(sink.events().await.iter().any(|event| matches!(
        event,
        RuntimeEvent::ExtensionUi { extension, .. } if extension == "fixture"
    )));
}
