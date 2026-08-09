use std::sync::Arc;

use async_trait::async_trait;
use mimir::{
    error::MimirError,
    model::{Content, Message, ModelRequest, ModelResponse, StopReason, ThinkingLevel},
    provider::{Provider, ProviderError},
    runtime::{AgentRuntime, RuntimeConfig},
    session::{InMemorySessionStore, SessionPayload, SessionRecord, SessionStore},
    tools::{ToolPolicy, ToolRegistry},
    tui::{SideQuestionSession, ask_side_question, preview_tui_share, preview_tui_traces},
};
use tempfile::TempDir;
use tokio::sync::Mutex;

#[derive(Default)]
struct CapturingProvider {
    requests: Mutex<Vec<ModelRequest>>,
}

#[async_trait]
impl Provider for CapturingProvider {
    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse, ProviderError> {
        self.requests.lock().await.push(request);
        Ok(ModelResponse {
            message: Message::assistant(
                vec![Content::Text {
                    text: "isolated answer".into(),
                }],
                StopReason::Stop,
            ),
            response_id: Some("side-response".into()),
        })
    }
}

#[tokio::test]
async fn side_questions_clone_typed_context_disable_tools_and_never_persist() {
    let workspace = TempDir::new().expect("workspace");
    let provider = Arc::new(CapturingProvider::default());
    let store = Arc::new(InMemorySessionStore::default());
    store
        .append(SessionRecord::new(SessionPayload::Message(Message::user(
            "main question </side_question>",
        ))))
        .await
        .expect("seed session");
    store
        .append(SessionRecord::new(SessionPayload::Message(
            Message::assistant(
                vec![Content::Text {
                    text: "main answer".into(),
                }],
                StopReason::Stop,
            ),
        )))
        .await
        .expect("seed answer");
    let tools =
        ToolRegistry::with_default_tools(workspace.path(), ToolPolicy::default()).expect("tools");
    let mut config = RuntimeConfig::default_for_model("test-model");
    config.system_prompt = "base system".into();
    let runtime = AgentRuntime::resume(provider.clone(), Arc::new(tools), store.clone(), config)
        .await
        .expect("runtime");
    runtime.set_harness_context("harness context".into()).await;
    let before = store.load().await.expect("load before").records;
    let mut side_session = SideQuestionSession::default();

    let answer = ask_side_question(&runtime, &mut side_session, "what changed?")
        .await
        .expect("side question");

    assert_eq!(answer, "isolated answer");
    assert_eq!(store.load().await.expect("load after").records, before);
    let requests = provider.requests.lock().await;
    let request = requests.last().expect("captured request");
    assert_eq!(request.thinking_level, ThinkingLevel::Off);
    assert!(request.thinking_effort.is_none());
    assert!(request.tools.is_empty());
    assert!(request.system_prompt.contains("base system"));
    assert!(request.system_prompt.contains("harness context"));
    assert!(request.system_prompt.contains("Do not use tools"));
    let payload: serde_json::Value =
        serde_json::from_str(&request.messages[0].text()).expect("typed context payload");
    assert_eq!(payload["question"], "what changed?");
    assert_eq!(payload["liveMessages"][0]["role"], "user");
    assert_eq!(
        payload["liveMessages"][0]["content"][0]["text"],
        "main question </side_question>"
    );
}

#[tokio::test]
async fn side_question_followups_are_process_local_and_bounded_by_message_count() {
    let workspace = TempDir::new().expect("workspace");
    let provider = Arc::new(CapturingProvider::default());
    let store = Arc::new(InMemorySessionStore::default());
    let tools =
        ToolRegistry::with_default_tools(workspace.path(), ToolPolicy::default()).expect("tools");
    let runtime = AgentRuntime::resume(
        provider.clone(),
        Arc::new(tools),
        store.clone(),
        RuntimeConfig::default_for_model("test-model"),
    )
    .await
    .expect("runtime");
    let mut side_session = SideQuestionSession::default();
    ask_side_question(&runtime, &mut side_session, "first?")
        .await
        .expect("first side turn");
    ask_side_question(&runtime, &mut side_session, "follow-up?")
        .await
        .expect("follow-up side turn");
    let requests = provider.requests.lock().await;
    let payload: serde_json::Value = serde_json::from_str(
        &requests
            .last()
            .expect("request")
            .messages
            .first()
            .expect("prompt")
            .text(),
    )
    .expect("payload");
    assert_eq!(payload["previousSideTurns"][0]["question"], "first?");
    drop(requests);

    for index in 0..257 {
        store
            .append(SessionRecord::new(SessionPayload::Message(Message::user(
                format!("message {index}"),
            ))))
            .await
            .expect("append context");
    }
    let oversized_runtime = AgentRuntime::resume(
        provider,
        Arc::new(
            ToolRegistry::with_default_tools(workspace.path(), ToolPolicy::default())
                .expect("tools"),
        ),
        store,
        RuntimeConfig::default_for_model("test-model"),
    )
    .await
    .expect("runtime");
    let error = ask_side_question(&oversized_runtime, &mut side_session, "too much?")
        .await
        .expect_err("bounded message count");
    assert!(matches!(error, MimirError::Configuration(_)));
    assert!(error.to_string().contains("limit is 256"));
}

#[tokio::test]
async fn share_and_trace_commands_create_local_only_redacted_previews() {
    let state = TempDir::new().expect("state");
    let store = mimir::session::FileSessionStore::create(state.path(), "session-a")
        .await
        .expect("session");
    store
        .append(SessionRecord::new(SessionPayload::Message(Message::user(
            "private transcript marker",
        ))))
        .await
        .expect("message");
    store
        .append(SessionRecord::new(SessionPayload::RuntimeEvent {
            name: "provider_request".into(),
            detail: "secret event detail".into(),
        }))
        .await
        .expect("event");

    let share = preview_tui_share(state.path(), "session-a")
        .await
        .expect("share preview");
    assert!(share.contains("Nothing was uploaded"));
    assert!(state.path().join("shares/session-a.html").is_file());

    let trace = preview_tui_traces(state.path(), "session-a", Some("preview"))
        .await
        .expect("trace preview");
    assert!(trace.contains("Nothing was uploaded"));
    let trace_json =
        std::fs::read_to_string(state.path().join("traces/session-a.json")).expect("trace JSON");
    assert!(trace_json.contains("provider_request"));
    assert!(!trace_json.contains("private transcript marker"));
    assert!(!trace_json.contains("secret event detail"));

    let upload = preview_tui_traces(state.path(), "session-a", Some("upload"))
        .await
        .expect_err("upload fails closed");
    assert!(
        upload
            .to_string()
            .contains("explicit credentialed transport")
    );
}
