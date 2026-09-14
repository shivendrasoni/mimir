use std::{path::PathBuf, sync::Arc, time::Duration};

use mimir::{
    extensions::{
        ExtensionCatalog, ExtensionManager, ExtensionPackageManager, HostLimits, LifecycleEvent,
        LifecycleInterception, RuntimeLimits,
    },
    model::{Content, Message, ModelResponse, StopReason},
    provider::FakeProvider,
    runtime::{AgentRuntime, RuntimeConfig, VecEventSink},
    session::InMemorySessionStore,
    tools::{ToolPolicy, ToolRegistry},
};
use tempfile::TempDir;

fn example(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("examples/extensions")
        .join(name)
}

fn limits() -> RuntimeLimits {
    RuntimeLimits {
        host: HostLimits {
            max_request_bytes: 64 * 1024,
            max_response_bytes: 64 * 1024,
            timeout: Duration::from_secs(2),
        },
        max_concurrency: 2,
        max_registrations: 64,
    }
}

#[tokio::test]
async fn lifecycle_examples_install_and_execute_through_the_public_extension_abi() {
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    let packages = ExtensionPackageManager::new(state.path()).expect("package manager");
    packages
        .install_local(&example("lifecycle-observer"))
        .await
        .expect("install observer");
    packages
        .install_local(&example("compaction-guard"))
        .await
        .expect("install compaction guard");

    let mut catalog = ExtensionCatalog::new(workspace.path(), state.path()).expect("catalog");
    let manager = ExtensionManager::load(
        catalog.reload().await.expect("reload catalog"),
        workspace.path(),
        state.path(),
        limits(),
    )
    .await
    .expect("load examples");

    let updates = manager
        .dispatch(LifecycleEvent::MessageUpdate {
            session_id: "session-1".into(),
            message_id: "message-1".into(),
            role: "assistant".into(),
        })
        .await
        .expect("dispatch message update");
    assert_eq!(updates.len(), 1);

    let counts = manager
        .invoke_command("lifecycle-counts", "")
        .await
        .expect("invoke lifecycle counts");
    assert_eq!(counts.output["message_update"], 1);

    let compaction = manager
        .dispatch(LifecycleEvent::SessionBeforeCompact {
            session_id: "session-1".into(),
            context_tokens: 1_000,
        })
        .await
        .expect("dispatch compaction pre-hook");
    assert!(compaction.iter().any(|dispatch| {
        matches!(
            dispatch.outcome.interception,
            LifecycleInterception::Block { .. }
        )
    }));
}

#[tokio::test]
async fn compaction_lifecycle_example_observes_the_real_runtime_path_without_deadlocking() {
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    let packages = ExtensionPackageManager::new(state.path()).expect("package manager");
    packages
        .install_local(&example("lifecycle-observer"))
        .await
        .expect("install observer");

    let mut catalog = ExtensionCatalog::new(workspace.path(), state.path()).expect("catalog");
    let manager = Arc::new(
        ExtensionManager::load(
            catalog.reload().await.expect("reload catalog"),
            workspace.path(),
            state.path(),
            limits(),
        )
        .await
        .expect("load observer"),
    );
    let response = |text: &str| ModelResponse {
        message: Message::assistant(vec![Content::Text { text: text.into() }], StopReason::Stop),
        response_id: None,
    };
    let provider = std::sync::Arc::new(FakeProvider::new(vec![
        response("old answer"),
        response("recent answer"),
        response("manual summary"),
    ]));
    let tools = std::sync::Arc::new(
        ToolRegistry::with_default_tools(workspace.path(), ToolPolicy::default()).expect("tools"),
    );
    let runtime = AgentRuntime::resume(
        provider,
        tools,
        std::sync::Arc::new(InMemorySessionStore::default()),
        RuntimeConfig::default_for_model("fake-model"),
    )
    .await
    .expect("runtime");
    runtime
        .attach_extension_manager(manager.clone(), "session-1")
        .await;
    runtime
        .run("old decision", &VecEventSink::default())
        .await
        .expect("old turn");
    runtime
        .run(&"R".repeat(100_000), &VecEventSink::default())
        .await
        .expect("recent turn");

    tokio::time::timeout(Duration::from_secs(5), runtime.compact(None))
        .await
        .expect("compaction lifecycle dispatch must not deadlock")
        .expect("compact session");

    let counts = manager
        .invoke_command("lifecycle-counts", "")
        .await
        .expect("invoke lifecycle counts");
    assert_eq!(counts.output["session_before_compact"], 1);
    assert_eq!(counts.output["session_compact"], 1);
}

#[test]
fn every_declared_lifecycle_event_has_a_production_dispatch_site() {
    let production = format!(
        "{}\n{}",
        include_str!("../src/runtime.rs"),
        include_str!("../src/extensions/manager.rs")
    );
    for event in [
        "ResourcesDiscover",
        "SessionStart",
        "SessionBeforeSwitch",
        "SessionBeforeFork",
        "SessionBeforeCompact",
        "SessionCompact",
        "SessionShutdown",
        "SessionBeforeTree",
        "SessionTree",
        "BeforeProviderRequest",
        "AfterProviderResponse",
        "AgentStart",
        "AgentEnd",
        "Context",
        "BeforeAgentStart",
        "TurnStart",
        "TurnEnd",
        "MessageStart",
        "MessageUpdate",
        "MessageEnd",
        "ModelSelect",
        "ThinkingLevelSelect",
        "ToolCall",
        "ToolResult",
        "UserBash",
        "Input",
        "RefineComplete",
        "ToolExecutionStart",
        "ToolExecutionUpdate",
        "ToolExecutionEnd",
    ] {
        assert!(
            production.contains(&format!("LifecycleEvent::{event}")),
            "LifecycleEvent::{event} has no production dispatch site"
        );
    }
}
