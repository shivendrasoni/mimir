use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use mimir::{
    error::Result,
    extensions::{
        AuthenticatedModelCatalog, RlmChildExecutor, RlmExecutionRequest, RlmExecutionResult,
        RlmModel, RlmRuntime, RlmRuntimeLimits,
    },
    provider::FakeProvider,
    runtime::{AgentRuntime, RuntimeConfig},
    session::InMemorySessionStore,
    tools::{ToolPolicy, ToolRegistry},
    tui::{TuiAction, dispatch_runtime_action},
};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

struct StaticCatalog;

#[async_trait]
impl AuthenticatedModelCatalog for StaticCatalog {
    async fn list_authenticated_models(&self) -> Result<Vec<RlmModel>> {
        Ok(vec![RlmModel {
            provider: "openai".into(),
            id: "gpt-5-mini".into(),
            name: "GPT-5 Mini".into(),
        }])
    }
}

struct BlockingExecutor;

#[async_trait]
impl RlmChildExecutor for BlockingExecutor {
    async fn execute(
        &self,
        _request: RlmExecutionRequest,
        cancellation: CancellationToken,
    ) -> Result<RlmExecutionResult> {
        cancellation.cancelled().await;
        Ok(RlmExecutionResult { output_tokens: 0 })
    }
}

struct ObservedCancellationExecutor {
    started: Arc<Notify>,
    stopped: Arc<Notify>,
}

struct ExecutionDropSignal(Arc<Notify>);

impl Drop for ExecutionDropSignal {
    fn drop(&mut self) {
        self.0.notify_one();
    }
}

#[async_trait]
impl RlmChildExecutor for ObservedCancellationExecutor {
    async fn execute(
        &self,
        _request: RlmExecutionRequest,
        cancellation: CancellationToken,
    ) -> Result<RlmExecutionResult> {
        let _drop_signal = ExecutionDropSignal(Arc::clone(&self.stopped));
        self.started.notify_one();
        cancellation.cancelled().await;
        Ok(RlmExecutionResult { output_tokens: 0 })
    }
}

fn limits() -> RlmRuntimeLimits {
    RlmRuntimeLimits {
        max_prompt_bytes: 4 * 1024,
        max_spawn_code_bytes: 4 * 1024,
        max_children: 8,
        max_concurrent_children: 1,
        max_depth: 1,
        max_duration: Duration::from_secs(5),
        max_output_tokens: 128,
        max_catalog_models: 8,
        max_state_bytes: 1024 * 1024,
    }
}

async fn registry(state: &TempDir, workspace: &TempDir) -> (ToolRegistry, Arc<RlmRuntime>) {
    let runtime = Arc::new(
        RlmRuntime::open(
            state.path(),
            workspace.path(),
            "parent-session",
            Some("/sessions/parent-session.jsonl"),
            0,
            "openai/gpt-5-mini",
            Arc::new(StaticCatalog),
            Arc::new(BlockingExecutor),
            limits(),
        )
        .await
        .expect("RLM runtime"),
    );
    let mut registry = ToolRegistry::with_default_tools(workspace.path(), ToolPolicy::default())
        .expect("tool registry");
    registry
        .register_rlm_runtime(Arc::clone(&runtime))
        .expect("register RLM tools");
    (registry, runtime)
}

fn content(observation: &mimir::tools::ToolObservation) -> Value {
    serde_json::from_str(&observation.content).expect("JSON observation")
}

#[tokio::test]
async fn provider_safe_aliases_are_registered_and_child_snapshots_are_non_recursive() {
    let state = TempDir::new().expect("state");
    let workspace = TempDir::new().expect("workspace");
    let mut registry =
        ToolRegistry::with_default_tools(workspace.path(), ToolPolicy::default()).expect("tools");
    let runtime = Arc::new(
        RlmRuntime::open(
            state.path(),
            workspace.path(),
            "parent-session",
            None,
            0,
            "openai/gpt-5-mini",
            Arc::new(StaticCatalog),
            Arc::new(BlockingExecutor),
            limits(),
        )
        .await
        .expect("RLM runtime"),
    );
    registry
        .register_rlm_runtime(runtime)
        .expect("register RLM tools");
    // Snapshot after registration to prove the child-specific operation
    // excludes RLM aliases independent of assembly order.
    let child_snapshot = registry.snapshot_for_child_runtime();

    let names = registry
        .definitions()
        .into_iter()
        .map(|definition| definition.name)
        .collect::<Vec<_>>();
    // Each provider-safe alias dispatches the corresponding dotted reference
    // host operation: rlm.run, rlm.find_models, rlm.list_subagents,
    // rlm.delete_subagent, and rlm.cancel_subagent.
    for (name, reference_operation) in [
        ("rlm_run", "rlm.run"),
        ("rlm_find_models", "rlm.find_models"),
        ("rlm_list_subagents", "rlm.list_subagents"),
        ("rlm_delete_subagent", "rlm.delete_subagent"),
        ("rlm_cancel_subagent", "rlm.cancel_subagent"),
    ] {
        assert!(names.iter().any(|candidate| candidate == name));
        assert_eq!(name, reference_operation.replacen('.', "_", 1));
        assert!(
            child_snapshot
                .definitions()
                .iter()
                .all(|definition| definition.name != name)
        );
    }
}

#[tokio::test]
async fn tool_payloads_match_reference_admission_and_management_shapes() {
    let state = TempDir::new().expect("state");
    let workspace = TempDir::new().expect("workspace");
    let (registry, _runtime) = registry(&state, &workspace).await;

    let models = registry
        .execute("rlm_find_models", json!({"query": "mini", "limit": 8}))
        .await
        .expect("find models");
    assert_eq!(
        content(&models)["models"][0]["selector"],
        "openai/gpt-5-mini"
    );

    let admitted = registry
        .execute(
            "rlm_run",
            json!({
                "prompt": "Review provider parity",
                "kwargs": {"name": "provider-review", "model": "openai/gpt-5-mini"},
                "cellSourceCode": "await rlm.run('Review provider parity')"
            }),
        )
        .await
        .expect("admit child");
    let admitted = content(&admitted);
    assert_eq!(admitted["name"], "provider-review");
    assert_eq!(admitted["model"], "openai/gpt-5-mini");
    assert!(admitted["rlm_child_id"].as_str().is_some());
    assert!(admitted["session_dir"].as_str().is_some());
    let target = admitted["rlm_child_id"].as_str().expect("child id");

    let listed = registry
        .execute("rlm_list_subagents", json!({}))
        .await
        .expect("list children");
    assert_eq!(content(&listed)["subagents"][0]["rlm_child_id"], target);

    let cancelled = registry
        .execute("rlm_cancel_subagent", json!({"target": target}))
        .await
        .expect("cancel child");
    let cancelled = content(&cancelled);
    assert_eq!(cancelled["cancelled"], true);
    assert_eq!(cancelled["subagent"]["rlm_child_id"], target);

    let deleted = registry
        .execute("rlm_delete_subagent", json!({"target": target}))
        .await
        .expect("delete child");
    let deleted = content(&deleted);
    assert_eq!(deleted["outcome"], "deleted");
    assert_eq!(deleted["subagent"]["rlm_child_id"], target);
}

#[tokio::test]
async fn agent_runtime_exposes_a_bounded_view_of_registered_rlm_children() {
    let state = TempDir::new().expect("state");
    let workspace = TempDir::new().expect("workspace");
    let (registry, _runtime) = registry(&state, &workspace).await;
    registry
        .execute(
            "rlm_run",
            json!({"prompt": "Review context reporting", "kwargs": {"name": "context-child"}}),
        )
        .await
        .expect("admit child");
    let runtime = AgentRuntime::resume(
        Arc::new(FakeProvider::with_results(Vec::new())),
        Arc::new(registry),
        Arc::new(InMemorySessionStore::default()),
        RuntimeConfig::default_for_model("gpt-5-mini"),
    )
    .await
    .expect("agent runtime");

    let children = runtime.context_children(1).await.expect("context children");
    assert_eq!(children.len(), 1);
    assert_eq!(children[0].session_name, "context-child");
    assert!(runtime.context_children(65).await.is_err());
    let rendered = dispatch_runtime_action(&runtime, &TuiAction::ShowContext)
        .await
        .expect("render context")
        .expect("runtime action");
    assert!(rendered.contains("context-child"));
    assert!(rendered.contains("Children: 1"));
}

#[tokio::test]
async fn tool_inputs_reject_unknown_fields_before_admission() {
    let state = TempDir::new().expect("state");
    let workspace = TempDir::new().expect("workspace");
    let (registry, runtime) = registry(&state, &workspace).await;

    let error = registry
        .execute("rlm_run", json!({"prompt": "bad", "temperature": 0.8}))
        .await
        .expect_err("unknown input must fail");
    assert!(error.to_string().contains("unknown field"));
    assert!(runtime.list_subagents().await.expect("children").is_empty());
}

#[tokio::test]
async fn run_accepts_multiline_text_but_rejects_unsafe_controls() {
    let state = TempDir::new().expect("state");
    let workspace = TempDir::new().expect("workspace");
    let (registry, runtime) = registry(&state, &workspace).await;

    registry
        .execute(
            "rlm_run",
            json!({
                "prompt": "Review:\n\t- provider parity\r\n\t- cancellation",
                "cellSourceCode": "await rlm.run(\n\t'Review provider parity'\n)"
            }),
        )
        .await
        .expect("multiline prompt and spawn code should be admitted");

    let error = registry
        .execute("rlm_run", json!({"prompt": "unsafe\u{0000}prompt"}))
        .await
        .expect_err("unsafe controls must still fail");
    assert!(
        error
            .to_string()
            .contains("control characters other than newlines and tabs")
    );
    assert_eq!(runtime.list_subagents().await.expect("children").len(), 1);
}

#[tokio::test]
async fn tool_registry_owns_runtime_and_cancels_children_when_parent_is_dropped() {
    let state = TempDir::new().expect("state");
    let workspace = TempDir::new().expect("workspace");
    let started = Arc::new(Notify::new());
    let stopped = Arc::new(Notify::new());
    let runtime = Arc::new(
        RlmRuntime::open(
            state.path(),
            workspace.path(),
            "parent-session",
            None,
            0,
            "openai/gpt-5-mini",
            Arc::new(StaticCatalog),
            Arc::new(ObservedCancellationExecutor {
                started: Arc::clone(&started),
                stopped: Arc::clone(&stopped),
            }),
            limits(),
        )
        .await
        .expect("RLM runtime"),
    );
    let mut registry =
        ToolRegistry::with_default_tools(workspace.path(), ToolPolicy::default()).expect("tools");
    registry
        .register_rlm_runtime(Arc::clone(&runtime))
        .expect("register RLM tools");
    drop(runtime);

    registry
        .execute("rlm_run", json!({"prompt": "Wait for parent cancellation"}))
        .await
        .expect("admit child");
    tokio::time::timeout(Duration::from_secs(1), started.notified())
        .await
        .expect("child started");

    drop(registry);
    tokio::time::timeout(Duration::from_secs(1), stopped.notified())
        .await
        .expect("child cancellation propagated");
}
