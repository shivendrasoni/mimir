use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use mimir::{
    error::{MimirError, Result},
    extensions::{
        AuthenticatedModelCatalog, RlmChildExecutor, RlmChildStatus, RlmExecutionRequest,
        RlmExecutionResult, RlmModel, RlmRunRequest, RlmRuntime, RlmRuntimeLimits,
    },
};
use tempfile::TempDir;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
struct FakeCatalog(Vec<RlmModel>);

#[async_trait]
impl AuthenticatedModelCatalog for FakeCatalog {
    async fn list_authenticated_models(&self) -> Result<Vec<RlmModel>> {
        Ok(self.0.clone())
    }
}

struct ImmediateExecutor {
    output_tokens: u64,
}

#[async_trait]
impl RlmChildExecutor for ImmediateExecutor {
    async fn execute(
        &self,
        _request: RlmExecutionRequest,
        _cancellation: CancellationToken,
    ) -> Result<RlmExecutionResult> {
        Ok(RlmExecutionResult {
            output_tokens: self.output_tokens,
        })
    }
}

struct BlockingExecutor {
    active: AtomicUsize,
    peak: AtomicUsize,
    started: Notify,
    release: Notify,
}

impl BlockingExecutor {
    fn new() -> Self {
        Self {
            active: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            started: Notify::new(),
            release: Notify::new(),
        }
    }

    fn peak(&self) -> usize {
        self.peak.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl RlmChildExecutor for BlockingExecutor {
    async fn execute(
        &self,
        _request: RlmExecutionRequest,
        cancellation: CancellationToken,
    ) -> Result<RlmExecutionResult> {
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(active, Ordering::SeqCst);
        self.started.notify_waiters();
        tokio::select! {
            () = self.release.notified() => {}
            () = cancellation.cancelled() => {
                self.active.fetch_sub(1, Ordering::SeqCst);
                return Err(MimirError::Protocol("cancelled".into()));
            }
        }
        self.active.fetch_sub(1, Ordering::SeqCst);
        Ok(RlmExecutionResult { output_tokens: 3 })
    }
}

fn models() -> Vec<RlmModel> {
    vec![
        RlmModel {
            provider: "anthropic".into(),
            id: "claude-sonnet-4-6".into(),
            name: "Claude Sonnet 4.6".into(),
        },
        RlmModel {
            provider: "openai".into(),
            id: "gpt-5-mini".into(),
            name: "GPT-5 mini".into(),
        },
    ]
}

fn limits() -> RlmRuntimeLimits {
    RlmRuntimeLimits {
        max_prompt_bytes: 1024,
        max_spawn_code_bytes: 1024,
        max_children: 8,
        max_concurrent_children: 1,
        max_depth: 3,
        max_duration: Duration::from_secs(5),
        max_output_tokens: 128,
        max_catalog_models: 16,
        max_state_bytes: 1024 * 1024,
    }
}

async fn open_runtime(
    state: &TempDir,
    workspace: &TempDir,
    executor: Arc<dyn RlmChildExecutor>,
    limits: RlmRuntimeLimits,
) -> RlmRuntime {
    RlmRuntime::open(
        state.path(),
        workspace.path(),
        "parent-session",
        Some("/sessions/parent.jsonl"),
        0,
        "anthropic/claude-sonnet-4-6",
        Arc::new(FakeCatalog(models())),
        executor,
        limits,
    )
    .await
    .expect("RLM runtime")
}

async fn wait_for_status(runtime: &RlmRuntime, child_id: &str, status: RlmChildStatus) {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if runtime
                .list_subagents()
                .await
                .expect("list")
                .iter()
                .any(|child| child.child_id == child_id && child.status == status)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("status transition");
}

#[tokio::test]
async fn find_models_is_bounded_ranked_and_contains_only_authenticated_catalog_entries() {
    let state = TempDir::new().expect("state");
    let workspace = TempDir::new().expect("workspace");
    let runtime = open_runtime(
        &state,
        &workspace,
        Arc::new(ImmediateExecutor { output_tokens: 1 }),
        limits(),
    )
    .await;

    let matches = runtime.find_models("sonnet", 8).await.expect("models");
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].selector(), "anthropic/claude-sonnet-4-6");
    assert!(runtime.find_models("", 0).await.is_err());
    assert!(runtime.find_models("", 21).await.is_err());
}

#[tokio::test]
async fn run_returns_immediately_then_persists_completion_and_session_linkage() {
    let state = TempDir::new().expect("state");
    let workspace = TempDir::new().expect("workspace");
    let runtime = open_runtime(
        &state,
        &workspace,
        Arc::new(ImmediateExecutor { output_tokens: 7 }),
        limits(),
    )
    .await;

    let handle = runtime
        .run(RlmRunRequest {
            prompt: "Inspect the migration".into(),
            name: Some("review-worker".into()),
            model: None,
            parent_node_id: Some("node-1".into()),
            spawn_code: Some("rlm.run(...)".into()),
            max_output_tokens: Some(32),
        })
        .await
        .expect("admit child");
    assert_eq!(handle.name, "review-worker");
    assert_eq!(handle.model, "anthropic/claude-sonnet-4-6");
    wait_for_status(&runtime, &handle.child_id, RlmChildStatus::Completed).await;

    let child = runtime
        .list_subagents()
        .await
        .expect("list")
        .into_iter()
        .find(|child| child.child_id == handle.child_id)
        .expect("child");
    assert_eq!(child.parent_session_id, "parent-session");
    assert_eq!(
        child.parent_session_path.as_deref(),
        Some("/sessions/parent.jsonl")
    );
    assert_eq!(child.parent_node_id.as_deref(), Some("node-1"));
    assert!(child.session_id.is_some());
    assert_eq!(child.output_tokens, 7);

    drop(runtime);
    let reopened = open_runtime(
        &state,
        &workspace,
        Arc::new(ImmediateExecutor { output_tokens: 1 }),
        limits(),
    )
    .await;
    assert_eq!(reopened.list_subagents().await.expect("reopen")[0], child);
}

#[tokio::test]
async fn concurrency_is_bounded_and_queued_children_can_be_cancelled_and_deleted() {
    let state = TempDir::new().expect("state");
    let workspace = TempDir::new().expect("workspace");
    let executor = Arc::new(BlockingExecutor::new());
    let runtime = open_runtime(&state, &workspace, executor.clone(), limits()).await;
    let first = runtime
        .run(RlmRunRequest::new("first"))
        .await
        .expect("first");
    wait_for_status(&runtime, &first.child_id, RlmChildStatus::Running).await;
    let second = runtime
        .run(RlmRunRequest::new("second"))
        .await
        .expect("second");
    wait_for_status(&runtime, &second.child_id, RlmChildStatus::Queued).await;
    assert_eq!(executor.peak(), 1);

    assert!(
        runtime
            .cancel_subagent(&second.child_id)
            .await
            .expect("cancel")
    );
    wait_for_status(&runtime, &second.child_id, RlmChildStatus::Cancelled).await;
    let deleted = runtime.delete_subagent(&second.name).await.expect("delete");
    assert_eq!(deleted.subagent.child_id, second.child_id);
    assert!(
        runtime
            .list_subagents()
            .await
            .expect("list")
            .iter()
            .all(|child| child.child_id != second.child_id)
    );

    executor.release.notify_waiters();
    wait_for_status(&runtime, &first.child_id, RlmChildStatus::Completed).await;
}

#[tokio::test]
async fn prompt_depth_model_time_and_token_limits_fail_closed() {
    let state = TempDir::new().expect("state");
    let workspace = TempDir::new().expect("workspace");
    let mut tight = limits();
    tight.max_prompt_bytes = 4;
    tight.max_duration = Duration::from_millis(30);
    tight.max_output_tokens = 5;
    let executor = Arc::new(BlockingExecutor::new());
    let runtime = open_runtime(&state, &workspace, executor, tight.clone()).await;
    assert!(runtime.run(RlmRunRequest::new("too long")).await.is_err());
    let mut unavailable = RlmRunRequest::new("ok");
    unavailable.model = Some("google/not-authenticated".into());
    assert!(runtime.run(unavailable).await.is_err());

    let timed = runtime
        .run(RlmRunRequest::new("slow"))
        .await
        .expect("admit slow child");
    wait_for_status(&runtime, &timed.child_id, RlmChildStatus::Error).await;
    let timed_child = runtime.list_subagents().await.expect("list").remove(0);
    assert!(
        timed_child
            .error
            .as_deref()
            .is_some_and(|error| error.contains("time limit"))
    );

    let token_state = TempDir::new().expect("token state");
    let token_runtime = open_runtime(
        &token_state,
        &workspace,
        Arc::new(ImmediateExecutor { output_tokens: 6 }),
        tight,
    )
    .await;
    let token_child = token_runtime
        .run(RlmRunRequest::new("okay"))
        .await
        .expect("admit token child");
    wait_for_status(&token_runtime, &token_child.child_id, RlmChildStatus::Error).await;

    let depth_state = TempDir::new().expect("depth state");
    let depth_error = RlmRuntime::open(
        depth_state.path(),
        workspace.path(),
        "nested-parent",
        None,
        3,
        "anthropic/claude-sonnet-4-6",
        Arc::new(FakeCatalog(models())),
        Arc::new(ImmediateExecutor { output_tokens: 1 }),
        limits(),
    )
    .await
    .expect_err("depth rejected");
    assert!(depth_error.to_string().contains("depth"));
}

#[tokio::test]
async fn reopening_marks_orphaned_running_children_as_interrupted() {
    let state = TempDir::new().expect("state");
    let workspace = TempDir::new().expect("workspace");
    let executor = Arc::new(BlockingExecutor::new());
    let runtime = open_runtime(&state, &workspace, executor.clone(), limits()).await;
    let handle = runtime
        .run(RlmRunRequest::new("never finishes"))
        .await
        .expect("child");
    wait_for_status(&runtime, &handle.child_id, RlmChildStatus::Running).await;
    std::mem::forget(runtime);

    let reopened = open_runtime(
        &state,
        &workspace,
        Arc::new(ImmediateExecutor { output_tokens: 1 }),
        limits(),
    )
    .await;
    let child = reopened.list_subagents().await.expect("list").remove(0);
    assert_eq!(child.status, RlmChildStatus::Error);
    assert!(
        child
            .error
            .as_deref()
            .is_some_and(|error| error.contains("restart"))
    );

    executor.release.notify_waiters();
    tokio::time::sleep(Duration::from_millis(30)).await;
    drop(reopened);
    let verified = open_runtime(
        &state,
        &workspace,
        Arc::new(ImmediateExecutor { output_tokens: 1 }),
        limits(),
    )
    .await;
    assert_eq!(
        verified.list_subagents().await.expect("verify")[0].status,
        RlmChildStatus::Error,
        "a superseded pre-restart task must not overwrite durable state"
    );
}
