use std::{sync::Arc, time::Duration};

use mimir::{
    budget::Budget,
    model::{Content, Message, ModelResponse, StopReason},
    orchestration::{AgentMessageBus, SubagentManager, SubagentStatus},
    provider::FakeProvider,
    runtime::{AgentRuntime, RuntimeConfig, VecEventSink},
    session::InMemorySessionStore,
    tools::{ToolPolicy, ToolRegistry},
};
use tempfile::TempDir;

#[tokio::test]
async fn subagents_run_with_bounded_admission_and_report_terminal_state() {
    let root = TempDir::new().expect("tempdir");
    let provider = Arc::new(FakeProvider::new(vec![ModelResponse {
        message: Message::assistant(
            vec![Content::Text {
                text: "child result".into(),
            }],
            StopReason::Stop,
        ),
        response_id: None,
    }]));
    let runtime = Arc::new(
        AgentRuntime::resume(
            provider,
            Arc::new(
                ToolRegistry::with_default_tools(root.path(), ToolPolicy::default())
                    .expect("tools"),
            ),
            Arc::new(InMemorySessionStore::default()),
            RuntimeConfig {
                model: "fake".into(),
                system_prompt: String::new(),
                budget: Budget::default(),
                provider_timeout: Duration::from_secs(1),
                ..RuntimeConfig::default_for_model("fake")
            },
        )
        .await
        .expect("runtime"),
    );
    let manager = Arc::new(SubagentManager::new(1));
    let child = manager
        .spawn(
            "reviewer",
            "inspect the change",
            runtime,
            Arc::new(VecEventSink::default()),
        )
        .await
        .expect("child admitted");
    assert!(
        manager
            .spawn(
                "second",
                "must wait",
                Arc::new(
                    AgentRuntime::resume(
                        Arc::new(FakeProvider::new(vec![])),
                        Arc::new(
                            ToolRegistry::with_default_tools(root.path(), ToolPolicy::default())
                                .expect("tools")
                        ),
                        Arc::new(InMemorySessionStore::default()),
                        RuntimeConfig::default_for_model("fake"),
                    )
                    .await
                    .expect("second runtime")
                ),
                Arc::new(VecEventSink::default()),
            )
            .await
            .is_err()
    );

    let terminal = manager.wait(child.id).await.expect("wait child");
    assert_eq!(terminal.status, SubagentStatus::Complete);
    assert_eq!(terminal.result.as_deref(), Some("child result"));
}

#[tokio::test]
async fn message_bus_routes_only_to_the_named_agent() {
    let bus = AgentMessageBus::default();
    bus.send("parent", "child-a", "check auth")
        .await
        .expect("send");
    bus.send("parent", "child-b", "check tests")
        .await
        .expect("send");

    let messages = bus.drain("child-a").await;
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].body, "check auth");
    assert!(bus.drain("child-a").await.is_empty());
    assert_eq!(bus.drain("child-b").await.len(), 1);
}
