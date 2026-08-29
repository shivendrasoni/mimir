use std::{
    collections::{BTreeMap, VecDeque},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use mimir::{
    budget::Budget,
    error::MimirError,
    model::{Content, Message, ModelResponse, StopReason, ThinkingLevel, ToolCall, Usage},
    provider::{FakeProvider, Provider, ProviderError, ProviderEvent, ProviderEventSink},
    runtime::{AgentRuntime, QueueMode, RetryPolicy, RuntimeConfig, RuntimeEvent, VecEventSink},
    session::{InMemorySessionStore, LoadedSession, SessionPayload, SessionRecord, SessionStore},
    tools::{ToolPolicy, ToolRegistry},
};
use serde_json::json;
use tempfile::TempDir;
use tokio::sync::{Mutex, Notify};

fn response(content: Vec<Content>, stop_reason: StopReason, tokens: u64) -> ModelResponse {
    let mut message = Message::assistant(content, stop_reason);
    message.usage = Usage {
        input_tokens: tokens / 2,
        output_tokens: tokens - tokens / 2,
        cached_tokens: 0,
    };
    ModelResponse {
        message,
        response_id: Some("fake-response".into()),
    }
}

fn agent_message_prompt(index: usize) -> String {
    format!(
        "Agent-to-agent message received.\nSource: agent_message\nFrom: active source, session source, client mimir-rpc\nTo: active target, session target\nMessage id: agentmsg_{index}\n\nmessage {index}"
    )
}

struct DelayedProvider {
    response: ModelResponse,
    delay: Duration,
}

struct StreamingProvider {
    response: ModelResponse,
}

struct RetryScriptProvider {
    response: ModelResponse,
    failures_remaining: AtomicUsize,
    attempts: AtomicUsize,
    failed: Notify,
}

struct PartialFailureProvider {
    attempts: AtomicUsize,
}

struct GatedScriptProvider {
    responses: Mutex<VecDeque<ModelResponse>>,
    requests: Mutex<Vec<mimir::model::ModelRequest>>,
    first_started: Notify,
    release_first: Notify,
}

struct FailOnceOnCompactionStore {
    inner: Arc<InMemorySessionStore>,
    failed: AtomicBool,
}

#[async_trait]
impl SessionStore for FailOnceOnCompactionStore {
    async fn append(&self, record: SessionRecord) -> mimir::error::Result<()> {
        if matches!(record.payload, SessionPayload::Compaction { .. })
            && !self.failed.swap(true, Ordering::SeqCst)
        {
            return Err(MimirError::Session {
                path: PathBuf::from("compaction.jsonl"),
                message: "injected compaction failure".into(),
            });
        }
        self.inner.append(record).await
    }

    async fn load(&self) -> mimir::error::Result<LoadedSession> {
        self.inner.load().await
    }
}

#[async_trait]
impl Provider for StreamingProvider {
    async fn complete(
        &self,
        _request: mimir::model::ModelRequest,
    ) -> Result<ModelResponse, ProviderError> {
        Ok(self.response.clone())
    }

    async fn stream(
        &self,
        _request: mimir::model::ModelRequest,
        sink: &dyn ProviderEventSink,
    ) -> Result<ModelResponse, ProviderError> {
        sink.emit(ProviderEvent::TextDelta("hel".into())).await;
        sink.emit(ProviderEvent::TextDelta("lo".into())).await;
        Ok(self.response.clone())
    }
}

#[async_trait]
impl Provider for DelayedProvider {
    async fn complete(
        &self,
        _request: mimir::model::ModelRequest,
    ) -> Result<ModelResponse, ProviderError> {
        tokio::time::sleep(self.delay).await;
        Ok(self.response.clone())
    }
}

#[async_trait]
impl Provider for RetryScriptProvider {
    async fn complete(
        &self,
        _request: mimir::model::ModelRequest,
    ) -> Result<ModelResponse, ProviderError> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        let should_fail = self
            .failures_remaining
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok();
        if should_fail {
            self.failed.notify_one();
            Err(ProviderError::RateLimited {
                message: "retry later".into(),
            })
        } else {
            Ok(self.response.clone())
        }
    }
}

#[async_trait]
impl Provider for PartialFailureProvider {
    async fn complete(
        &self,
        _request: mimir::model::ModelRequest,
    ) -> Result<ModelResponse, ProviderError> {
        unreachable!("stream is implemented directly")
    }

    async fn stream(
        &self,
        _request: mimir::model::ModelRequest,
        sink: &dyn ProviderEventSink,
    ) -> Result<ModelResponse, ProviderError> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        sink.emit(ProviderEvent::TextDelta("partial".into())).await;
        Err(ProviderError::Unavailable {
            message: "stream disconnected".into(),
        })
    }
}

#[async_trait]
impl Provider for GatedScriptProvider {
    async fn complete(
        &self,
        request: mimir::model::ModelRequest,
    ) -> Result<ModelResponse, ProviderError> {
        let first = self.requests.lock().await.is_empty();
        self.requests.lock().await.push(request);
        if first {
            self.first_started.notify_one();
            self.release_first.notified().await;
        }
        self.responses
            .lock()
            .await
            .pop_front()
            .ok_or_else(|| ProviderError::Protocol {
                message: "gated response script exhausted".into(),
            })
    }
}

#[tokio::test]
async fn wait_for_idle_blocks_until_the_active_run_finishes() {
    let provider = Arc::new(GatedScriptProvider {
        responses: Mutex::new(VecDeque::from([response(
            vec![Content::Text {
                text: "finished".into(),
            }],
            StopReason::Stop,
            2,
        )])),
        requests: Mutex::new(Vec::new()),
        first_started: Notify::new(),
        release_first: Notify::new(),
    });
    let root = TempDir::new().expect("tempdir");
    let runtime = Arc::new(
        AgentRuntime::resume(
            provider.clone(),
            Arc::new(
                ToolRegistry::with_default_tools(root.path(), ToolPolicy::default())
                    .expect("tools"),
            ),
            Arc::new(InMemorySessionStore::default()),
            RuntimeConfig::default_for_model("fake-model"),
        )
        .await
        .expect("runtime"),
    );
    let run = {
        let runtime = Arc::clone(&runtime);
        tokio::spawn(async move { runtime.run("start", &VecEventSink::default()).await })
    };
    provider.first_started.notified().await;
    let waiter = {
        let runtime = Arc::clone(&runtime);
        tokio::spawn(async move { runtime.wait_for_idle().await })
    };
    tokio::task::yield_now().await;
    assert!(!waiter.is_finished());

    provider.release_first.notify_one();
    assert_eq!(run.await.expect("run task").expect("run"), "finished");
    waiter.await.expect("idle waiter");
}

#[tokio::test]
async fn steering_is_injected_into_the_active_run_before_the_next_provider_turn() {
    let provider = Arc::new(GatedScriptProvider {
        responses: Mutex::new(VecDeque::from([
            response(
                vec![Content::Text {
                    text: "first answer".into(),
                }],
                StopReason::Stop,
                2,
            ),
            response(
                vec![Content::Text {
                    text: "steered answer".into(),
                }],
                StopReason::Stop,
                2,
            ),
        ])),
        requests: Mutex::new(Vec::new()),
        first_started: Notify::new(),
        release_first: Notify::new(),
    });
    let root = TempDir::new().expect("tempdir");
    let runtime = Arc::new(
        AgentRuntime::resume(
            provider.clone(),
            Arc::new(
                ToolRegistry::with_default_tools(root.path(), ToolPolicy::default())
                    .expect("tools"),
            ),
            Arc::new(InMemorySessionStore::default()),
            RuntimeConfig {
                model: "fake-model".into(),
                system_prompt: String::new(),
                budget: Budget::default(),
                provider_timeout: Duration::from_secs(2),
                ..RuntimeConfig::default_for_model("fake-model")
            },
        )
        .await
        .expect("runtime"),
    );
    let running = {
        let runtime = runtime.clone();
        tokio::spawn(async move { runtime.run("initial", &VecEventSink::default()).await })
    };
    provider.first_started.notified().await;
    assert!(runtime.is_running());
    let delivery = {
        let runtime = runtime.clone();
        tokio::spawn(async move {
            runtime
                .steer("changed instruction")
                .await
                .expect("queue steering");
            runtime.run_pending_steering(&VecEventSink::default()).await
        })
    };
    while runtime.pending_steering_count().await == 0 {
        tokio::task::yield_now().await;
    }
    provider.release_first.notify_one();

    assert_eq!(running.await.expect("task").expect("run"), "steered answer");
    assert_eq!(
        delivery.await.expect("delivery task").expect("delivery"),
        None,
        "the active turn consumed the queued steering heartbeat"
    );
    let requests = provider.requests.lock().await;
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[1].messages.last().unwrap().text(),
        "changed instruction"
    );
}

#[tokio::test]
async fn agent_message_queue_validates_envelopes_and_bounds_only_agent_messages() {
    let root = TempDir::new().expect("tempdir");
    let runtime = AgentRuntime::resume(
        Arc::new(FakeProvider::new(Vec::new())),
        Arc::new(
            ToolRegistry::with_default_tools(root.path(), ToolPolicy::default()).expect("tools"),
        ),
        Arc::new(InMemorySessionStore::default()),
        RuntimeConfig::default_for_model("fake-model"),
    )
    .await
    .expect("runtime");

    runtime
        .steer("ordinary steering")
        .await
        .expect("ordinary steer");
    for index in 0..20 {
        runtime
            .queue_agent_message(&agent_message_prompt(index))
            .await
            .expect("agent message inside queue bound");
    }
    let full = runtime
        .queue_agent_message(&agent_message_prompt(20))
        .await
        .expect_err("twenty-first agent message must fail");
    assert!(full.to_string().contains("limit is 20"));
    assert_eq!(runtime.pending_steering_count().await, 21);

    assert_eq!(runtime.clear_queued_agent_messages().await, 20);
    assert_eq!(runtime.pending_steering_count().await, 1);
    assert_eq!(runtime.clear_steering().await, ["ordinary steering"]);
    assert_eq!(runtime.pending_steering_count().await, 0);

    let malformed = runtime
        .queue_agent_message(
            "Agent-to-agent message received.\nSource: agent_message\nsecret-without-routing-metadata",
        )
        .await
        .expect_err("partial envelopes must fail closed");
    assert!(malformed.to_string().contains("invalid envelope"));
    assert!(
        !malformed
            .to_string()
            .contains("secret-without-routing-metadata")
    );
}

#[tokio::test]
async fn model_and_thinking_changes_reach_the_next_turn_of_an_active_run() {
    let first_provider = Arc::new(GatedScriptProvider {
        responses: Mutex::new(VecDeque::from([response(
            vec![Content::ToolCall(ToolCall {
                id: "call-1".into(),
                name: "read_file".into(),
                arguments: json!({"path": "README.md"}),
            })],
            StopReason::ToolUse,
            4,
        )])),
        requests: Mutex::new(Vec::new()),
        first_started: Notify::new(),
        release_first: Notify::new(),
    });
    let second_provider = Arc::new(FakeProvider::new(vec![response(
        vec![Content::Text {
            text: "switched".into(),
        }],
        StopReason::Stop,
        2,
    )]));
    let root = TempDir::new().expect("tempdir");
    std::fs::write(root.path().join("README.md"), "runtime switch").expect("fixture");
    let store = Arc::new(InMemorySessionStore::default());
    let mut config = RuntimeConfig::default_for_model("model-a");
    config.provider = "provider-a".into();
    let runtime = Arc::new(
        AgentRuntime::resume(
            first_provider.clone(),
            Arc::new(
                ToolRegistry::with_default_tools(root.path(), ToolPolicy::default())
                    .expect("tools"),
            ),
            store.clone(),
            config,
        )
        .await
        .expect("runtime"),
    );

    let running = {
        let runtime = runtime.clone();
        tokio::spawn(async move { runtime.run("start", &VecEventSink::default()).await })
    };
    first_provider.first_started.notified().await;
    runtime
        .select_model(
            second_provider.clone(),
            "provider-b",
            "model-b",
            vec![ThinkingLevel::Off, ThinkingLevel::High],
            Some(BTreeMap::from([(ThinkingLevel::High, Some("max".into()))])),
        )
        .await
        .expect("select model");
    assert_eq!(
        runtime
            .set_thinking_level(ThinkingLevel::High)
            .await
            .expect("set thinking"),
        ThinkingLevel::High
    );
    first_provider.release_first.notify_one();

    assert_eq!(running.await.expect("join").expect("run"), "switched");
    let requests = second_provider.requests().await;
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].model, "model-b");
    assert_eq!(requests[0].thinking_level, ThinkingLevel::High);
    assert_eq!(requests[0].thinking_effort.as_deref(), Some("max"));
    let records = store.load().await.expect("persisted selection").records;
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(
                &record.payload,
                SessionPayload::RuntimeEvent { name, .. } if name == "model_selection"
            ))
            .count(),
        2
    );
}

#[tokio::test]
async fn follow_up_waits_for_a_fresh_turn_after_the_busy_run() {
    let provider = Arc::new(GatedScriptProvider {
        responses: Mutex::new(VecDeque::from([
            response(
                vec![Content::Text {
                    text: "first answer".into(),
                }],
                StopReason::Stop,
                2,
            ),
            response(
                vec![Content::Text {
                    text: "follow-up answer".into(),
                }],
                StopReason::Stop,
                2,
            ),
        ])),
        requests: Mutex::new(Vec::new()),
        first_started: Notify::new(),
        release_first: Notify::new(),
    });
    let root = TempDir::new().expect("tempdir");
    let runtime = Arc::new(
        AgentRuntime::resume(
            provider.clone(),
            Arc::new(ToolRegistry::with_default_tools(root.path(), ToolPolicy::default()).unwrap()),
            Arc::new(InMemorySessionStore::default()),
            RuntimeConfig::default_for_model("fake-model"),
        )
        .await
        .unwrap(),
    );
    let running = {
        let runtime = runtime.clone();
        tokio::spawn(async move { runtime.run("initial", &VecEventSink::default()).await })
    };
    provider.first_started.notified().await;
    let follow_up = {
        let runtime = runtime.clone();
        tokio::spawn(async move {
            runtime
                .run("heartbeat follow-up", &VecEventSink::default())
                .await
        })
    };
    tokio::task::yield_now().await;
    assert_eq!(provider.requests.lock().await.len(), 1);
    provider.release_first.notify_one();

    assert_eq!(running.await.unwrap().unwrap(), "first answer");
    assert_eq!(follow_up.await.unwrap().unwrap(), "follow-up answer");
    let requests = provider.requests.lock().await;
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[1].messages.last().unwrap().text(),
        "heartbeat follow-up"
    );
}

#[tokio::test]
async fn all_mode_drains_every_steering_message_into_one_provider_turn() {
    let provider = Arc::new(GatedScriptProvider {
        responses: Mutex::new(VecDeque::from([
            response(
                vec![Content::Text {
                    text: "first".into(),
                }],
                StopReason::Stop,
                2,
            ),
            response(
                vec![Content::Text {
                    text: "combined".into(),
                }],
                StopReason::Stop,
                2,
            ),
        ])),
        requests: Mutex::new(Vec::new()),
        first_started: Notify::new(),
        release_first: Notify::new(),
    });
    let root = TempDir::new().expect("tempdir");
    let runtime = Arc::new(
        AgentRuntime::resume(
            provider.clone(),
            Arc::new(ToolRegistry::with_default_tools(root.path(), ToolPolicy::default()).unwrap()),
            Arc::new(InMemorySessionStore::default()),
            RuntimeConfig::default_for_model("fake-model"),
        )
        .await
        .unwrap(),
    );
    runtime.set_steering_mode(QueueMode::All).await;
    let running = {
        let runtime = runtime.clone();
        tokio::spawn(async move { runtime.run("initial", &VecEventSink::default()).await })
    };
    provider.first_started.notified().await;
    runtime.steer("first steering").await.unwrap();
    runtime.steer("second steering").await.unwrap();
    provider.release_first.notify_one();

    assert_eq!(running.await.unwrap().unwrap(), "combined");
    let requests = provider.requests.lock().await;
    assert_eq!(requests.len(), 2);
    let tail: Vec<_> = requests[1]
        .messages
        .iter()
        .rev()
        .take(2)
        .map(Message::text)
        .collect();
    assert_eq!(tail, ["second steering", "first steering"]);
}

#[tokio::test]
async fn one_at_a_time_mode_delivers_one_steering_message_per_provider_turn() {
    let provider = Arc::new(GatedScriptProvider {
        responses: Mutex::new(VecDeque::from([
            response(
                vec![Content::Text {
                    text: "first".into(),
                }],
                StopReason::Stop,
                2,
            ),
            response(
                vec![Content::Text {
                    text: "second".into(),
                }],
                StopReason::Stop,
                2,
            ),
            response(
                vec![Content::Text {
                    text: "third".into(),
                }],
                StopReason::Stop,
                2,
            ),
        ])),
        requests: Mutex::new(Vec::new()),
        first_started: Notify::new(),
        release_first: Notify::new(),
    });
    let root = TempDir::new().expect("tempdir");
    let runtime = Arc::new(
        AgentRuntime::resume(
            provider.clone(),
            Arc::new(ToolRegistry::with_default_tools(root.path(), ToolPolicy::default()).unwrap()),
            Arc::new(InMemorySessionStore::default()),
            RuntimeConfig::default_for_model("fake-model"),
        )
        .await
        .unwrap(),
    );
    assert_eq!(runtime.steering_mode().await, QueueMode::OneAtATime);
    let running = {
        let runtime = runtime.clone();
        tokio::spawn(async move { runtime.run("initial", &VecEventSink::default()).await })
    };
    provider.first_started.notified().await;
    runtime.steer("first steering").await.unwrap();
    runtime.steer("second steering").await.unwrap();
    provider.release_first.notify_one();

    assert_eq!(running.await.unwrap().unwrap(), "third");
    let requests = provider.requests.lock().await;
    assert_eq!(requests.len(), 3);
    assert_eq!(
        requests[1].messages.last().unwrap().text(),
        "first steering"
    );
    assert_eq!(
        requests[2].messages.last().unwrap().text(),
        "second steering"
    );
}

#[tokio::test]
async fn fake_provider_drives_tool_result_and_final_response_through_persistence() {
    let root = TempDir::new().expect("tempdir");
    let provider = Arc::new(FakeProvider::new(vec![
        response(
            vec![Content::ToolCall(ToolCall {
                id: "call-1".into(),
                name: "write_file".into(),
                arguments: json!({"path": "answer.txt", "content": "forty-two"}),
            })],
            StopReason::ToolUse,
            12,
        ),
        response(
            vec![Content::Text {
                text: "Done.".into(),
            }],
            StopReason::Stop,
            8,
        ),
    ]));
    let store: Arc<dyn SessionStore> = Arc::new(InMemorySessionStore::default());
    let tools = Arc::new(
        ToolRegistry::with_default_tools(
            root.path(),
            ToolPolicy {
                allow_process: false,
                ..ToolPolicy::default()
            },
        )
        .expect("tools"),
    );
    let runtime = AgentRuntime::resume(
        provider.clone(),
        tools,
        store.clone(),
        RuntimeConfig {
            model: "fake-model".into(),
            system_prompt: "Use tools carefully.".into(),
            budget: Budget::default(),
            provider_timeout: Duration::from_secs(1),
            ..RuntimeConfig::default_for_model("fake-model")
        },
    )
    .await
    .expect("runtime");
    let events = VecEventSink::default();

    let answer = runtime.run("write the answer", &events).await.expect("run");

    assert_eq!(answer, "Done.");
    assert_eq!(
        std::fs::read_to_string(root.path().join("answer.txt")).expect("file"),
        "forty-two"
    );
    let requests = provider.requests().await;
    assert_eq!(requests.len(), 2);
    let canonical_workspace = root.path().canonicalize().expect("canonical workspace");
    assert!(requests[0].system_prompt.contains("Use tools carefully."));
    assert!(
        requests[0]
            .system_prompt
            .contains(&canonical_workspace.display().to_string())
    );
    assert!(requests[0].system_prompt.contains("$WORKSPACE"));
    assert!(requests[0].system_prompt.contains("not an OS sandbox"));
    let loaded = store.load().await.expect("session");
    assert_eq!(
        loaded
            .records
            .iter()
            .filter(|record| matches!(record.payload, SessionPayload::Message(_)))
            .count(),
        4
    );
    assert!(
        events
            .events()
            .await
            .iter()
            .any(|event| event.kind() == "tool_finished")
    );
    assert!(
        events
            .events()
            .await
            .iter()
            .any(|event| event.kind() == "completed")
    );
}

#[tokio::test]
async fn budget_interruption_persists_results_for_every_assistant_tool_call() {
    let root = TempDir::new().expect("tempdir");
    std::fs::write(root.path().join("source.txt"), "source").expect("fixture");
    let provider = Arc::new(FakeProvider::new(vec![response(
        vec![
            Content::ToolCall(ToolCall {
                id: "call-first".into(),
                name: "read_file".into(),
                arguments: json!({"path": "source.txt"}),
            }),
            Content::ToolCall(ToolCall {
                id: "call-interrupted".into(),
                name: "read_file".into(),
                arguments: json!({"path": "source.txt"}),
            }),
        ],
        StopReason::ToolUse,
        2,
    )]));
    let store = Arc::new(InMemorySessionStore::default());
    let runtime = AgentRuntime::resume(
        provider,
        Arc::new(
            ToolRegistry::with_default_tools(root.path(), ToolPolicy::default()).expect("tools"),
        ),
        store.clone(),
        RuntimeConfig {
            budget: Budget {
                max_tool_calls: 1,
                ..Budget::default()
            },
            provider_timeout: Duration::from_secs(1),
            ..RuntimeConfig::default_for_model("fake-model")
        },
    )
    .await
    .expect("runtime");

    let error = runtime
        .run("read twice", &VecEventSink::default())
        .await
        .expect_err("second call must exceed budget");
    assert!(error.to_string().contains("tool-call budget exhausted"));

    let messages = store
        .load()
        .await
        .expect("session")
        .records
        .into_iter()
        .filter_map(|record| match record.payload {
            SessionPayload::Message(message) => Some(message),
            _ => None,
        })
        .collect::<Vec<_>>();
    let findings = mimir::session_integrity::inspect(&messages);
    assert!(findings.missing_calls.is_empty());
    let interrupted = messages
        .iter()
        .flat_map(|message| &message.content)
        .find_map(|content| match content {
            Content::ToolResult(result) if result.tool_call_id == "call-interrupted" => {
                Some(result)
            }
            _ => None,
        });
    let interrupted = interrupted.expect("synthetic interrupted result");
    assert!(interrupted.is_error);
    assert!(interrupted.content.contains("session_integrity_repair"));
    assert!(interrupted.content.contains("not performed"));
}

#[tokio::test]
async fn resume_repairs_an_orphaned_tool_call_before_the_next_provider_request() {
    let store = Arc::new(InMemorySessionStore::default());
    store
        .append(SessionRecord::new(SessionPayload::Message(
            Message::assistant(
                vec![Content::ToolCall(ToolCall {
                    id: "crashed-call".into(),
                    name: "read_file".into(),
                    arguments: json!({"path": "source.txt"}),
                })],
                StopReason::ToolUse,
            ),
        )))
        .await
        .expect("seed orphan");
    let provider = Arc::new(FakeProvider::new(vec![response(
        vec![Content::Text {
            text: "recovered".into(),
        }],
        StopReason::Stop,
        2,
    )]));
    let root = TempDir::new().expect("tempdir");
    let runtime = AgentRuntime::resume(
        provider.clone(),
        Arc::new(
            ToolRegistry::with_default_tools(root.path(), ToolPolicy::default()).expect("tools"),
        ),
        store.clone(),
        RuntimeConfig::default_for_model("fake-model"),
    )
    .await
    .expect("resume repairs orphan");

    assert_eq!(
        runtime
            .run("continue", &VecEventSink::default())
            .await
            .expect("run"),
        "recovered"
    );
    let request = provider.requests().await.pop().expect("request");
    assert_eq!(request.messages[0].role, mimir::model::Role::Assistant);
    assert_eq!(request.messages[1].role, mimir::model::Role::Tool);
    assert!(matches!(
        &request.messages[1].content[..],
        [Content::ToolResult(result)] if result.tool_call_id == "crashed-call" && result.is_error
    ));

    let durable = store.load().await.expect("session");
    assert!(durable.records.iter().any(|record| matches!(
        &record.payload,
        SessionPayload::Message(message)
            if message.content.iter().any(|content| matches!(
                content,
                Content::ToolResult(result)
                    if result.tool_call_id == "crashed-call"
                        && result.content.contains("session_integrity_repair")
            ))
    )));
}

#[tokio::test]
async fn resume_pauses_on_an_unexpected_tool_result_instead_of_dropping_it() {
    let store = Arc::new(InMemorySessionStore::default());
    store
        .append(SessionRecord::new(SessionPayload::Message(
            Message::tool_result("unknown-call", "read_file", "unexpected", false),
        )))
        .await
        .expect("seed unexpected result");
    let root = TempDir::new().expect("tempdir");
    let Err(error) = AgentRuntime::resume(
        Arc::new(FakeProvider::new(Vec::new())),
        Arc::new(
            ToolRegistry::with_default_tools(root.path(), ToolPolicy::default()).expect("tools"),
        ),
        store,
        RuntimeConfig::default_for_model("fake-model"),
    )
    .await
    else {
        panic!("ambiguous history must pause");
    };
    assert!(error.to_string().contains("unexpected results"));
    assert!(error.to_string().contains("unknown-call"));
}

#[tokio::test]
async fn automatic_compaction_never_splits_a_tool_exchange() {
    let store = Arc::new(InMemorySessionStore::default());
    for message in [
        Message::user("old"),
        Message::assistant(
            vec![
                Content::ToolCall(ToolCall {
                    id: "kept-call".into(),
                    name: "read_file".into(),
                    arguments: json!({"path": "source.txt"}),
                }),
                Content::ToolCall(ToolCall {
                    id: "kept-call-2".into(),
                    name: "read_file".into(),
                    arguments: json!({"path": "other.txt"}),
                }),
            ],
            StopReason::ToolUse,
        ),
        Message::tool_result("kept-call", "read_file", "source", false),
        Message::tool_result("kept-call-2", "read_file", "other", false),
        Message::user("after tool"),
    ] {
        store
            .append(SessionRecord::new(SessionPayload::Message(message)))
            .await
            .expect("seed session");
    }
    let provider = Arc::new(FakeProvider::new(vec![response(
        vec![Content::Text { text: "ok".into() }],
        StopReason::Stop,
        2,
    )]));
    let root = TempDir::new().expect("tempdir");
    let runtime = AgentRuntime::resume(
        provider.clone(),
        Arc::new(
            ToolRegistry::with_default_tools(root.path(), ToolPolicy::default()).expect("tools"),
        ),
        store.clone(),
        RuntimeConfig {
            budget: Budget {
                max_context_messages: 5,
                ..Budget::default()
            },
            ..RuntimeConfig::default_for_model("fake-model")
        },
    )
    .await
    .expect("runtime");
    runtime
        .run("new", &VecEventSink::default())
        .await
        .expect("run");

    let request = provider.requests().await.pop().expect("request");
    let call_index = request
        .messages
        .iter()
        .position(|message| {
            message
                .content
                .iter()
                .any(|content| matches!(content, Content::ToolCall(call) if call.id == "kept-call"))
        })
        .expect("retained call");
    assert!(matches!(
        request
            .messages
            .get(call_index + 1)
            .map(|message| message.role),
        Some(mimir::model::Role::Tool)
    ));
    assert_eq!(request.messages[call_index + 1].content.len(), 2);

    let resumed = AgentRuntime::resume(
        Arc::new(FakeProvider::new(Vec::new())),
        Arc::new(
            ToolRegistry::with_default_tools(root.path(), ToolPolicy::default()).expect("tools"),
        ),
        store,
        RuntimeConfig::default_for_model("fake-model"),
    )
    .await
    .expect("compacted session remains resumable");
    let resumed_messages = resumed.messages_snapshot().await;
    let findings = mimir::session_integrity::inspect(&resumed_messages);
    assert!(findings.missing_calls.is_empty());
    assert!(!findings.requires_pause());
    assert!(
        resumed_messages.iter().any(|message| {
            message.role == mimir::model::Role::Tool && message.content.len() == 2
        })
    );
}

#[tokio::test]
async fn required_provenance_blocks_a_fabricated_copy_after_a_failed_read() {
    let provider = Arc::new(FakeProvider::new(vec![
        response(
            vec![Content::ToolCall(ToolCall {
                id: "read-source".into(),
                name: "read_file".into(),
                arguments: json!({"path": "missing.css"}),
            })],
            StopReason::ToolUse,
            2,
        ),
        response(
            vec![Content::ToolCall(ToolCall {
                id: "write-copy".into(),
                name: "write_file".into(),
                arguments: json!({
                    "path": "copy.css",
                    "content": "invented replacement",
                    "provenance": {
                        "required": true,
                        "derivedFrom": [{
                            "toolCallId": "read-source",
                            "path": "missing.css"
                        }]
                    }
                }),
            })],
            StopReason::ToolUse,
            2,
        ),
        response(
            vec![Content::Text {
                text: "stopped safely".into(),
            }],
            StopReason::Stop,
            2,
        ),
    ]));
    let store = Arc::new(InMemorySessionStore::default());
    let root = TempDir::new().expect("tempdir");
    let runtime = AgentRuntime::resume(
        provider,
        Arc::new(
            ToolRegistry::with_default_tools(root.path(), ToolPolicy::default()).expect("tools"),
        ),
        store.clone(),
        RuntimeConfig::default_for_model("fake-model"),
    )
    .await
    .expect("runtime");

    assert_eq!(
        runtime
            .run("copy the source", &VecEventSink::default())
            .await
            .expect("agent can recover"),
        "stopped safely"
    );
    assert!(!root.path().join("copy.css").exists());
    let loaded = store.load().await.expect("session");
    assert!(loaded.records.iter().any(|record| matches!(
        &record.payload,
        SessionPayload::RuntimeEvent { name, detail }
            if name == "provenance_check"
                && detail.contains("\"allowed\":false")
                && detail.contains("mutation paused")
    )));
    assert!(loaded.records.iter().any(|record| matches!(
        &record.payload,
        SessionPayload::Message(message)
            if message.content.iter().any(|content| matches!(
                content,
                Content::ToolResult(result)
                    if result.tool_call_id == "write-copy" && result.is_error
            ))
    )));
}

#[tokio::test]
async fn runtime_compacts_context_before_provider_request() {
    let provider = Arc::new(FakeProvider::new(vec![response(
        vec![Content::Text { text: "ok".into() }],
        StopReason::Stop,
        2,
    )]));
    let store = Arc::new(InMemorySessionStore::default());
    for index in 0..10 {
        store
            .append(mimir::session::SessionRecord::new(SessionPayload::Message(
                Message::user(format!("old message {index}")),
            )))
            .await
            .expect("seed session");
    }
    let root = TempDir::new().expect("tempdir");
    let tools = Arc::new(
        ToolRegistry::with_default_tools(root.path(), ToolPolicy::default()).expect("tools"),
    );
    let runtime = AgentRuntime::resume(
        provider.clone(),
        tools,
        store,
        RuntimeConfig {
            model: "fake-model".into(),
            system_prompt: String::new(),
            budget: Budget {
                max_context_messages: 4,
                ..Budget::default()
            },
            provider_timeout: Duration::from_secs(1),
            ..RuntimeConfig::default_for_model("fake-model")
        },
    )
    .await
    .expect("runtime");

    runtime
        .run("new message", &VecEventSink::default())
        .await
        .expect("run");

    let request = provider.requests().await.pop().expect("request");
    assert!(request.messages.len() <= 4);
    assert!(
        request.messages[0]
            .text()
            .contains("Compacted conversation")
    );
}

#[tokio::test]
async fn disabling_auto_compaction_preserves_the_full_context_for_the_provider() {
    let provider = Arc::new(FakeProvider::new(vec![response(
        vec![Content::Text { text: "ok".into() }],
        StopReason::Stop,
        2,
    )]));
    let store = Arc::new(InMemorySessionStore::default());
    for index in 0..6 {
        store
            .append(SessionRecord::new(SessionPayload::Message(Message::user(
                format!("old message {index}"),
            ))))
            .await
            .expect("seed session");
    }
    let root = TempDir::new().expect("tempdir");
    let runtime = AgentRuntime::resume(
        provider.clone(),
        Arc::new(
            ToolRegistry::with_default_tools(root.path(), ToolPolicy::default()).expect("tools"),
        ),
        store.clone(),
        RuntimeConfig {
            model: "fake-model".into(),
            system_prompt: String::new(),
            budget: Budget {
                max_context_messages: 4,
                ..Budget::default()
            },
            provider_timeout: Duration::from_secs(1),
            ..RuntimeConfig::default_for_model("fake-model")
        },
    )
    .await
    .expect("runtime");
    assert!(runtime.auto_compaction_enabled());
    runtime.set_auto_compaction(false);

    runtime
        .run("new message", &VecEventSink::default())
        .await
        .expect("run");

    let request = provider.requests().await.pop().expect("request");
    assert_eq!(request.messages.len(), 7);
    assert!(
        !request.messages[0]
            .text()
            .contains("Compacted conversation")
    );
    assert!(
        store
            .load()
            .await
            .expect("session")
            .records
            .iter()
            .all(|record| !matches!(record.payload, SessionPayload::Compaction { .. }))
    );
}

#[tokio::test]
async fn retryable_provider_failures_use_exponential_retry_and_emit_lifecycle_events() {
    let provider = Arc::new(RetryScriptProvider {
        response: response(
            vec![Content::Text {
                text: "recovered".into(),
            }],
            StopReason::Stop,
            2,
        ),
        failures_remaining: AtomicUsize::new(1),
        attempts: AtomicUsize::new(0),
        failed: Notify::new(),
    });
    let root = TempDir::new().expect("tempdir");
    let runtime = Arc::new(
        AgentRuntime::resume(
            provider.clone(),
            Arc::new(ToolRegistry::with_default_tools(root.path(), ToolPolicy::default()).unwrap()),
            Arc::new(InMemorySessionStore::default()),
            RuntimeConfig::default_for_model("fake-model"),
        )
        .await
        .unwrap(),
    );
    runtime.set_retry_policy(RetryPolicy {
        max_attempts: 3,
        base_delay: Duration::from_millis(50),
        max_delay: Duration::from_millis(50),
    });
    let events = Arc::new(VecEventSink::default());

    let run = {
        let runtime = Arc::clone(&runtime);
        let events = Arc::clone(&events);
        tokio::spawn(async move { runtime.run("hello", events.as_ref()).await })
    };
    provider.failed.notified().await;
    while runtime.retry_attempt() == 0 {
        tokio::task::yield_now().await;
    }
    assert!(runtime.is_retrying());
    assert_eq!(runtime.retry_attempt(), 1);
    assert_eq!(
        run.await.expect("retry task").expect("retry run"),
        "recovered"
    );
    assert!(!runtime.is_retrying());
    assert_eq!(runtime.retry_attempt(), 0);
    assert_eq!(provider.attempts.load(Ordering::SeqCst), 2);
    assert!(events.events().await.iter().any(|event| matches!(
        event,
        RuntimeEvent::AutoRetryStarted {
            attempt: 1,
            max_attempts: 3,
            delay_ms: 50,
            ..
        }
    )));
    assert!(events.events().await.iter().any(|event| matches!(
        event,
        RuntimeEvent::AutoRetryFinished {
            success: true,
            attempt: 1,
            final_error: None,
        }
    )));
}

#[tokio::test]
async fn disabling_auto_retry_returns_the_first_retryable_failure() {
    let provider = Arc::new(RetryScriptProvider {
        response: response(
            vec![Content::Text {
                text: "unused".into(),
            }],
            StopReason::Stop,
            2,
        ),
        failures_remaining: AtomicUsize::new(1),
        attempts: AtomicUsize::new(0),
        failed: Notify::new(),
    });
    let root = TempDir::new().expect("tempdir");
    let runtime = AgentRuntime::resume(
        provider.clone(),
        Arc::new(ToolRegistry::with_default_tools(root.path(), ToolPolicy::default()).unwrap()),
        Arc::new(InMemorySessionStore::default()),
        RuntimeConfig::default_for_model("fake-model"),
    )
    .await
    .unwrap();
    assert!(runtime.auto_retry_enabled());
    runtime.set_auto_retry(false);

    let error = runtime
        .run("hello", &VecEventSink::default())
        .await
        .expect_err("retry must be disabled");
    assert!(error.to_string().contains("retry later"));
    assert_eq!(provider.attempts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn abort_retry_interrupts_the_backoff_without_replaying_the_request() {
    let provider = Arc::new(RetryScriptProvider {
        response: response(
            vec![Content::Text {
                text: "unused".into(),
            }],
            StopReason::Stop,
            2,
        ),
        failures_remaining: AtomicUsize::new(usize::MAX),
        attempts: AtomicUsize::new(0),
        failed: Notify::new(),
    });
    let root = TempDir::new().expect("tempdir");
    let runtime = Arc::new(
        AgentRuntime::resume(
            provider.clone(),
            Arc::new(ToolRegistry::with_default_tools(root.path(), ToolPolicy::default()).unwrap()),
            Arc::new(InMemorySessionStore::default()),
            RuntimeConfig::default_for_model("fake-model"),
        )
        .await
        .unwrap(),
    );
    runtime.set_retry_policy(RetryPolicy {
        max_attempts: 3,
        base_delay: Duration::from_secs(30),
        max_delay: Duration::from_secs(30),
    });
    let events = Arc::new(VecEventSink::default());
    let running = {
        let runtime = runtime.clone();
        let events = events.clone();
        tokio::spawn(async move { runtime.run("hello", events.as_ref()).await })
    };
    provider.failed.notified().await;
    runtime.abort_retry();

    let error = running
        .await
        .unwrap()
        .expect_err("retry should be cancelled");
    assert!(error.to_string().contains("retry cancelled"));
    assert_eq!(provider.attempts.load(Ordering::SeqCst), 1);
    assert!(events.events().await.iter().any(|event| matches!(
        event,
        RuntimeEvent::AutoRetryFinished {
            success: false,
            attempt: 1,
            final_error: Some(message),
        } if message == "retry cancelled"
    )));
}

#[tokio::test]
async fn abort_retry_while_idle_does_not_poison_the_next_run() {
    let provider = Arc::new(RetryScriptProvider {
        response: response(
            vec![Content::Text {
                text: "recovered".into(),
            }],
            StopReason::Stop,
            2,
        ),
        failures_remaining: AtomicUsize::new(1),
        attempts: AtomicUsize::new(0),
        failed: Notify::new(),
    });
    let root = TempDir::new().expect("tempdir");
    let runtime = AgentRuntime::resume(
        provider.clone(),
        Arc::new(ToolRegistry::with_default_tools(root.path(), ToolPolicy::default()).unwrap()),
        Arc::new(InMemorySessionStore::default()),
        RuntimeConfig::default_for_model("fake-model"),
    )
    .await
    .unwrap();
    runtime.set_retry_policy(RetryPolicy {
        max_attempts: 3,
        base_delay: Duration::ZERO,
        max_delay: Duration::ZERO,
    });
    runtime.abort_retry();

    assert_eq!(
        runtime
            .run("hello", &VecEventSink::default())
            .await
            .unwrap(),
        "recovered"
    );
    assert_eq!(provider.attempts.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn cancelling_a_run_during_retry_backoff_closes_the_retry_lifecycle() {
    let provider = Arc::new(RetryScriptProvider {
        response: response(
            vec![Content::Text {
                text: "unused".into(),
            }],
            StopReason::Stop,
            2,
        ),
        failures_remaining: AtomicUsize::new(usize::MAX),
        attempts: AtomicUsize::new(0),
        failed: Notify::new(),
    });
    let root = TempDir::new().expect("tempdir");
    let runtime = Arc::new(
        AgentRuntime::resume(
            provider.clone(),
            Arc::new(ToolRegistry::with_default_tools(root.path(), ToolPolicy::default()).unwrap()),
            Arc::new(InMemorySessionStore::default()),
            RuntimeConfig::default_for_model("fake-model"),
        )
        .await
        .unwrap(),
    );
    runtime.set_retry_policy(RetryPolicy {
        max_attempts: 3,
        base_delay: Duration::from_secs(30),
        max_delay: Duration::from_secs(30),
    });
    let events = Arc::new(VecEventSink::default());
    let running = {
        let runtime = runtime.clone();
        let events = events.clone();
        tokio::spawn(async move { runtime.run("hello", events.as_ref()).await })
    };
    provider.failed.notified().await;
    runtime.cancel();

    let error = running.await.unwrap().expect_err("run should be cancelled");
    assert!(error.to_string().contains("run cancelled"));
    assert!(events.events().await.iter().any(|event| matches!(
        event,
        RuntimeEvent::AutoRetryFinished {
            success: false,
            attempt: 1,
            final_error: Some(message),
        } if message == "run cancelled"
    )));
}

#[tokio::test]
async fn retryable_failure_after_a_stream_delta_is_not_replayed() {
    let provider = Arc::new(PartialFailureProvider {
        attempts: AtomicUsize::new(0),
    });
    let root = TempDir::new().expect("tempdir");
    let runtime = AgentRuntime::resume(
        provider.clone(),
        Arc::new(ToolRegistry::with_default_tools(root.path(), ToolPolicy::default()).unwrap()),
        Arc::new(InMemorySessionStore::default()),
        RuntimeConfig::default_for_model("fake-model"),
    )
    .await
    .unwrap();
    runtime.set_retry_policy(RetryPolicy {
        max_attempts: 3,
        base_delay: Duration::ZERO,
        max_delay: Duration::ZERO,
    });

    let error = runtime
        .run("hello", &VecEventSink::default())
        .await
        .expect_err("partial stream failure must be terminal");
    assert!(error.to_string().contains("stream disconnected"));
    assert_eq!(provider.attempts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cancellation_only_aborts_the_current_run_and_future_runs_recover() {
    let provider = Arc::new(DelayedProvider {
        response: response(
            vec![Content::Text {
                text: "second run works".into(),
            }],
            StopReason::Stop,
            2,
        ),
        delay: Duration::from_millis(100),
    });
    let store: Arc<dyn SessionStore> = Arc::new(InMemorySessionStore::default());
    let root = TempDir::new().expect("tempdir");
    let tools = Arc::new(
        ToolRegistry::with_default_tools(root.path(), ToolPolicy::default()).expect("tools"),
    );
    let runtime = Arc::new(
        AgentRuntime::resume(
            provider,
            tools,
            store,
            RuntimeConfig {
                model: "fake-model".into(),
                system_prompt: String::new(),
                budget: Budget::default(),
                provider_timeout: Duration::from_secs(1),
                ..RuntimeConfig::default_for_model("fake-model")
            },
        )
        .await
        .expect("runtime"),
    );

    let cancelled = runtime.clone();
    let run = tokio::spawn(async move {
        cancelled
            .run("first prompt", &VecEventSink::default())
            .await
    });
    tokio::time::sleep(Duration::from_millis(10)).await;
    runtime
        .steer("must not leak into a later run")
        .await
        .expect("steer current run");
    runtime.cancel();
    let error = run
        .await
        .expect("join")
        .expect_err("current run should cancel");
    assert!(error.to_string().contains("cancelled"));
    assert_eq!(runtime.pending_steering_count().await, 0);

    let answer = runtime
        .run("second prompt", &VecEventSink::default())
        .await
        .expect("future runs should recover");
    assert_eq!(answer, "second run works");
}

#[tokio::test]
async fn provider_native_deltas_are_forwarded_once_before_completion() {
    let provider = Arc::new(StreamingProvider {
        response: response(
            vec![Content::Text {
                text: "hello".into(),
            }],
            StopReason::Stop,
            2,
        ),
    });
    let store: Arc<dyn SessionStore> = Arc::new(InMemorySessionStore::default());
    let root = TempDir::new().expect("tempdir");
    let tools = Arc::new(
        ToolRegistry::with_default_tools(root.path(), ToolPolicy::default()).expect("tools"),
    );
    let runtime = AgentRuntime::resume(
        provider,
        tools,
        store,
        RuntimeConfig::default_for_model("stream-model"),
    )
    .await
    .expect("runtime");
    let events = VecEventSink::default();

    assert_eq!(runtime.run("prompt", &events).await.expect("run"), "hello");
    let emitted = events.events().await;
    let deltas: Vec<_> = emitted
        .iter()
        .filter_map(|event| match event {
            mimir::runtime::RuntimeEvent::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(deltas, ["hel", "lo"]);
}

#[tokio::test]
async fn compaction_failure_does_not_mutate_in_memory_state_before_persistence() {
    let provider = Arc::new(FakeProvider::new(vec![response(
        vec![Content::Text {
            text: "second run works".into(),
        }],
        StopReason::Stop,
        2,
    )]));
    let inner_store = Arc::new(InMemorySessionStore::default());
    for index in 0..10 {
        inner_store
            .append(SessionRecord::new(SessionPayload::Message(Message::user(
                format!("old message {index}"),
            ))))
            .await
            .expect("seed session");
    }
    let store: Arc<dyn SessionStore> = Arc::new(FailOnceOnCompactionStore {
        inner: inner_store,
        failed: AtomicBool::new(false),
    });
    let root = TempDir::new().expect("tempdir");
    let tools = Arc::new(
        ToolRegistry::with_default_tools(root.path(), ToolPolicy::default()).expect("tools"),
    );
    let runtime = AgentRuntime::resume(
        provider.clone(),
        tools,
        store,
        RuntimeConfig {
            model: "fake-model".into(),
            system_prompt: String::new(),
            budget: Budget {
                max_context_messages: 4,
                ..Budget::default()
            },
            provider_timeout: Duration::from_secs(1),
            ..RuntimeConfig::default_for_model("fake-model")
        },
    )
    .await
    .expect("runtime");

    let error = runtime
        .run("first prompt", &VecEventSink::default())
        .await
        .expect_err("first run should fail");
    assert!(error.to_string().contains("injected compaction failure"));

    let answer = runtime
        .run("second prompt", &VecEventSink::default())
        .await
        .expect("second run should recover");
    assert_eq!(answer, "second run works");

    let request = provider.requests().await.pop().expect("request");
    assert!(
        request.messages[0]
            .text()
            .matches("Compacted conversation")
            .count()
            == 1,
        "compaction summary must not be derived from an unpersisted in-memory summary"
    );
}

#[tokio::test]
async fn manual_compaction_failure_keeps_the_active_transcript_unchanged() {
    let provider = Arc::new(FakeProvider::new(vec![
        response(
            vec![Content::Text {
                text: "answer one".into(),
            }],
            StopReason::Stop,
            2,
        ),
        response(
            vec![Content::Text {
                text: "answer two".into(),
            }],
            StopReason::Stop,
            2,
        ),
        response(
            vec![Content::Text {
                text: "summary".into(),
            }],
            StopReason::Stop,
            2,
        ),
    ]));
    let inner_store = Arc::new(InMemorySessionStore::default());
    let store: Arc<dyn SessionStore> = Arc::new(FailOnceOnCompactionStore {
        inner: inner_store.clone(),
        failed: AtomicBool::new(false),
    });
    let root = TempDir::new().expect("tempdir");
    let tools = Arc::new(
        ToolRegistry::with_default_tools(root.path(), ToolPolicy::default()).expect("tools"),
    );
    let runtime = AgentRuntime::resume(
        provider,
        tools,
        store,
        RuntimeConfig {
            model: "fake-model".into(),
            system_prompt: String::new(),
            budget: Budget {
                max_context_messages: 100,
                ..Budget::default()
            },
            provider_timeout: Duration::from_secs(1),
            ..RuntimeConfig::default_for_model("fake-model")
        },
    )
    .await
    .expect("runtime");
    runtime
        .run("prompt one", &VecEventSink::default())
        .await
        .expect("first run");
    let recent_prompt = "R".repeat(100_000);
    runtime
        .run(&recent_prompt, &VecEventSink::default())
        .await
        .expect("second run");
    let before = runtime.messages_snapshot().await;

    let error = runtime
        .compact(Some("keep decisions"))
        .await
        .expect_err("checkpoint failure must surface");
    assert!(error.to_string().contains("injected compaction failure"));
    assert_eq!(runtime.messages_snapshot().await, before);
    assert!(
        inner_store
            .load()
            .await
            .expect("records")
            .records
            .iter()
            .all(|record| !matches!(record.payload, SessionPayload::Compaction { .. }))
    );
}
