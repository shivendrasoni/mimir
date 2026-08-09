use std::{collections::BTreeSet, sync::Arc, time::Duration};

use async_trait::async_trait;
use mimir::{
    auth::{AuthStore, OAuthCredential},
    error::Result,
    extensions::{
        AgentRuntimeChildExecutor, AuthStoreModelCatalog, AuthStoreProviderAuthentication,
        AuthenticatedModelCatalog, ProviderAuthenticationProbe, RlmChildExecutor,
        RlmChildRuntimePolicy, RlmExecutionRequest, RlmExecutionResult, RlmHostOperations,
        RlmModel, RlmRuntime, RlmRuntimeLimits,
    },
    model::{Content, Message, ModelResponse, StopReason, Usage},
    provider::{
        FakeProvider, Provider,
        registry::{AuthKind, ProviderDefinition, RuntimeSupport},
    },
    tools::{ToolPolicy, ToolRegistry},
};
use serde_json::json;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

const NO_AUTH_KINDS: &[AuthKind] = &[];

struct AllowedProviders(BTreeSet<&'static str>);

#[async_trait]
impl ProviderAuthenticationProbe for AllowedProviders {
    async fn is_authenticated(&self, provider: &ProviderDefinition) -> Result<bool> {
        Ok(self.0.contains(provider.id))
    }
}

struct StaticProviderFactory {
    provider: Arc<dyn Provider>,
}

#[async_trait]
impl mimir::extensions::RlmProviderFactory for StaticProviderFactory {
    async fn create_provider(&self, _model: &RlmModel) -> Result<Arc<dyn Provider>> {
        Ok(Arc::clone(&self.provider))
    }
}

#[derive(Clone)]
struct StaticCatalog(Vec<RlmModel>);

#[async_trait]
impl AuthenticatedModelCatalog for StaticCatalog {
    async fn list_authenticated_models(&self) -> Result<Vec<RlmModel>> {
        Ok(self.0.clone())
    }
}

struct ImmediateExecutor;

#[async_trait]
impl RlmChildExecutor for ImmediateExecutor {
    async fn execute(
        &self,
        _request: RlmExecutionRequest,
        _cancellation: CancellationToken,
    ) -> Result<RlmExecutionResult> {
        Ok(RlmExecutionResult { output_tokens: 3 })
    }
}

fn openai_model() -> RlmModel {
    RlmModel {
        provider: "openai".into(),
        id: "gpt-5-mini".into(),
        name: "GPT-5 Mini".into(),
    }
}

fn response(text: &str, output_tokens: u64) -> ModelResponse {
    let mut message =
        Message::assistant(vec![Content::Text { text: text.into() }], StopReason::Stop);
    message.usage = Usage {
        input_tokens: 2,
        output_tokens,
        cached_tokens: 0,
    };
    ModelResponse {
        message,
        response_id: Some("rlm-fake-response".into()),
    }
}

fn execution_request(session_dir: &std::path::Path) -> RlmExecutionRequest {
    RlmExecutionRequest {
        child_id: "sub-child".into(),
        session_id: "child-session".into(),
        session_name: "child-reviewer".into(),
        session_dir: session_dir.into(),
        parent_session_id: "parent-session".into(),
        parent_session_path: Some("/sessions/parent.jsonl".into()),
        parent_node_id: Some("parent-node".into()),
        depth: 1,
        prompt: "Inspect the migration".into(),
        spawn_code: Some("await rlm.run(...)".into()),
        model: openai_model(),
        max_output_tokens: 64,
    }
}

fn limits() -> RlmRuntimeLimits {
    RlmRuntimeLimits {
        max_prompt_bytes: 1024,
        max_spawn_code_bytes: 1024,
        max_children: 8,
        max_concurrent_children: 2,
        max_depth: 3,
        max_duration: Duration::from_secs(5),
        max_output_tokens: 128,
        max_catalog_models: 64,
        max_state_bytes: 1024 * 1024,
    }
}

#[tokio::test]
async fn auth_store_probe_accepts_api_keys_and_rejects_expired_oauth() {
    let state = TempDir::new().expect("state");
    let store = AuthStore::new(state.path()).expect("auth store");
    store
        .set_api_key("stored-provider", "private-key")
        .await
        .expect("store key");
    store
        .set_oauth(
            "expired-provider",
            OAuthCredential {
                access: "expired-access".into(),
                refresh: "refresh".into(),
                expires_at_ms: 1,
                account_id: None,
                enterprise_url: None,
            },
        )
        .await
        .expect("store OAuth");
    let probe = AuthStoreProviderAuthentication::new(store);
    let stored = ProviderDefinition {
        id: "stored-provider",
        name: "Stored",
        env_vars: &[],
        auth: NO_AUTH_KINDS,
        base_url: None,
        default_model: None,
        runtime_support: RuntimeSupport::OpenAiCompatible,
    };
    let expired = ProviderDefinition {
        id: "expired-provider",
        name: "Expired",
        env_vars: &[],
        auth: NO_AUTH_KINDS,
        base_url: None,
        default_model: None,
        runtime_support: RuntimeSupport::OpenAiCompatible,
    };

    assert!(probe.is_authenticated(&stored).await.expect("stored"));
    assert!(!probe.is_authenticated(&expired).await.expect("expired"));
}

#[tokio::test]
async fn authenticated_catalog_filters_by_credentials_and_native_transport_and_is_bounded() {
    let catalog = AuthStoreModelCatalog::with_probe(
        Arc::new(AllowedProviders(BTreeSet::from(["openai", "anthropic"]))),
        128,
    )
    .expect("catalog");
    let models = catalog
        .list_authenticated_models()
        .await
        .expect("authenticated models");
    assert!(
        models
            .iter()
            .any(|model| model.selector() == "openai/gpt-5-mini")
    );
    assert!(
        models
            .iter()
            .any(|model| model.selector() == "anthropic/claude-sonnet-4-6")
    );
    assert!(
        models
            .iter()
            .all(|model| matches!(model.provider.as_str(), "openai" | "anthropic"))
    );
    assert!(
        models
            .windows(2)
            .all(|pair| pair[0].selector() < pair[1].selector())
    );

    let bounded = AuthStoreModelCatalog::with_probe(
        Arc::new(AllowedProviders(BTreeSet::from(["openrouter"]))),
        8,
    )
    .expect("bounded catalog");
    let error = bounded
        .list_authenticated_models()
        .await
        .expect_err("catalog must fail closed instead of truncating silently");
    assert!(error.to_string().contains("exceeding its configured limit"));
}

#[tokio::test]
async fn child_executor_uses_agent_runtime_persists_session_and_reports_output_usage() {
    let workspace = TempDir::new().expect("workspace");
    let session = TempDir::new().expect("session");
    let fake = FakeProvider::new(vec![response("done", 7)]);
    let tools = Arc::new(
        ToolRegistry::with_default_tools(workspace.path(), ToolPolicy::default()).expect("tools"),
    );
    let executor = AgentRuntimeChildExecutor::new(
        Arc::new(StaticProviderFactory {
            provider: Arc::new(fake.clone()),
        }),
        tools,
        RlmChildRuntimePolicy::default(),
    );

    let result = executor
        .execute(execution_request(session.path()), CancellationToken::new())
        .await
        .expect("child run");
    assert_eq!(result.output_tokens, 7);
    assert!(
        session
            .path()
            .join("sessions/child-session.jsonl")
            .is_file()
    );
    let requests = fake.requests().await;
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].model, "gpt-5-mini");
    assert_eq!(requests[0].max_output_tokens, 64);
    assert!(requests[0].system_prompt.contains("child_id: sub-child"));
    assert!(requests[0].system_prompt.contains("depth: 1"));
}

#[tokio::test]
async fn child_executor_cancellation_aborts_a_slow_provider_without_waiting_for_timeout() {
    let workspace = TempDir::new().expect("workspace");
    let session = TempDir::new().expect("session");
    let slow = FakeProvider::new(vec![response("late", 1)]).with_delay(Duration::from_secs(30));
    let executor = Arc::new(AgentRuntimeChildExecutor::new(
        Arc::new(StaticProviderFactory {
            provider: Arc::new(slow),
        }),
        Arc::new(
            ToolRegistry::with_default_tools(workspace.path(), ToolPolicy::default())
                .expect("tools"),
        ),
        RlmChildRuntimePolicy::default(),
    ));
    let cancellation = CancellationToken::new();
    let child_cancellation = cancellation.clone();
    let task = tokio::spawn({
        let executor = Arc::clone(&executor);
        let request = execution_request(session.path());
        async move { executor.execute(request, child_cancellation).await }
    });
    tokio::task::yield_now().await;
    cancellation.cancel();
    let error = tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .expect("bounded cancellation")
        .expect("task")
        .expect_err("cancelled");
    assert!(error.to_string().contains("cancelled"));
}

#[tokio::test]
async fn host_operations_match_reference_payloads_and_reject_unsupported_kwargs() {
    let state = TempDir::new().expect("state");
    let workspace = TempDir::new().expect("workspace");
    let runtime = Arc::new(
        RlmRuntime::open(
            state.path(),
            workspace.path(),
            "parent-session",
            Some("/sessions/parent.jsonl"),
            0,
            "openai/gpt-5-mini",
            Arc::new(StaticCatalog(vec![openai_model()])),
            Arc::new(ImmediateExecutor),
            limits(),
        )
        .await
        .expect("runtime"),
    );
    let host = RlmHostOperations::new(runtime, Some("parent-node".into()));

    let models = host
        .handle("rlm.find_models", json!({"query": "mini", "limit": 8}))
        .await
        .expect("find");
    assert_eq!(models["models"][0]["selector"], "openai/gpt-5-mini");

    let handle = host
        .handle(
            "rlm.run",
            json!({
                "prompt": "Review auth",
                "kwargs": {"name": "auth-reviewer"},
                "cellSourceCode": "await rlm.run('Review auth')"
            }),
        )
        .await
        .expect("run admission");
    assert!(handle["rlm_child_id"].as_str().is_some());
    assert_eq!(handle["name"], "auth-reviewer");
    assert_eq!(handle["model"], "openai/gpt-5-mini");

    let listed = host
        .handle("rlm.list_subagents", json!({}))
        .await
        .expect("list");
    let entry = &listed["subagents"][0];
    assert_eq!(entry["rlm_child_id"], handle["rlm_child_id"]);
    assert!(matches!(
        entry["status"].as_str(),
        Some("running" | "completed")
    ));

    let invalid = host
        .handle(
            "rlm.run",
            json!({"prompt": "bad", "kwargs": {"temperature": 1}}),
        )
        .await
        .expect_err("unknown kwargs");
    assert!(invalid.to_string().contains("unsupported rlm.run kwargs"));

    let target = handle["rlm_child_id"].as_str().expect("child id");
    let deleted = host
        .handle("rlm.delete_subagent", json!({"target": target}))
        .await
        .expect("delete");
    assert_eq!(deleted["outcome"], "deleted");
    assert_eq!(deleted["subagent"]["rlm_child_id"], target);
}
