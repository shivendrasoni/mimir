use std::{sync::Arc, time::Duration};

use mimir::{
    budget::Budget,
    model::{Content, Message, ModelResponse, StopReason, ToolCall},
    provider::FakeProvider,
    resources::{MAX_SKILL_BODY_BYTES, ResourceLoader},
    runtime::{AgentRuntime, RuntimeConfig, VecEventSink},
    session::{InMemorySessionStore, SessionStore},
    skills::SkillRuntime,
    tools::{ToolPolicy, ToolRegistry},
    tui::{Action, App, AppConfig},
    typesafe::{TypeSafeSkillConfig, TypeSafeSkillMode, TypeSafeSkillSelector},
};
use tempfile::TempDir;
use typesafe_client::fake::FakeSystemOne;

fn response(text: &str) -> ModelResponse {
    ModelResponse {
        message: Message::assistant(vec![Content::Text { text: text.into() }], StopReason::Stop),
        response_id: None,
    }
}

fn tool_response(id: &str, name: &str, arguments: serde_json::Value) -> ModelResponse {
    ModelResponse {
        message: Message::assistant(
            vec![Content::ToolCall(ToolCall {
                id: id.into(),
                name: name.into(),
                arguments,
            })],
            StopReason::ToolUse,
        ),
        response_id: None,
    }
}

fn write_skill(root: &std::path::Path, name: &str, description: &str, body: &str) {
    let directory = root.join(".agents/skills").join(name);
    std::fs::create_dir_all(&directory).expect("skill directory");
    std::fs::write(
        directory.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {description}\n---\n{body}\n"),
    )
    .expect("skill file");
}

#[test]
fn tui_forwards_skill_slash_commands_to_the_runtime() {
    let mut app = App::new(AppConfig::default());
    app.set_prompt("/skill:review focus on auth");
    app.apply_action(Action::SubmitPrompt);
    assert_eq!(
        app.pending_submission().as_deref(),
        Some("/skill:review focus on auth")
    );
}

async fn runtime_with_skills(
    workspace: &std::path::Path,
    provider: Arc<FakeProvider>,
    store: Arc<dyn SessionStore>,
) -> AgentRuntime {
    let resources = ResourceLoader::new(workspace, workspace)
        .expect("resource loader")
        .load()
        .expect("resources");
    let skills = SkillRuntime::new(resources.skills);
    let mut tools =
        ToolRegistry::with_default_tools(workspace, ToolPolicy::default()).expect("tools");
    tools
        .register_skill_search(&skills)
        .expect("skill search tool");
    let runtime = AgentRuntime::resume(
        provider,
        Arc::new(tools),
        store,
        RuntimeConfig {
            model: "fake-model".into(),
            system_prompt: "base system".into(),
            budget: Budget::default(),
            provider_timeout: Duration::from_secs(2),
            ..RuntimeConfig::default_for_model("fake-model")
        },
    )
    .await
    .expect("runtime");
    runtime.attach_skill_runtime(skills);
    runtime
}

#[tokio::test]
async fn model_can_discover_then_ephemerally_activate_a_skill() {
    let workspace = TempDir::new().expect("workspace");
    write_skill(
        workspace.path(),
        "brainstorming",
        "Explore product ideas before implementation",
        "Ask focused questions before selecting a design.",
    );
    write_skill(
        workspace.path(),
        "api-design",
        "Design stable service interfaces",
        "Define the interface contract.",
    );
    let provider = Arc::new(FakeProvider::new(vec![
        tool_response(
            "search-1",
            "search_skills",
            serde_json::json!({"query": "explore product ideas"}),
        ),
        tool_response(
            "activate-1",
            "search_skills",
            serde_json::json!({"name": "brainstorming"}),
        ),
        response("finished with the selected skill"),
    ]));
    let store = Arc::new(InMemorySessionStore::default());
    let runtime = runtime_with_skills(workspace.path(), provider.clone(), store.clone()).await;

    runtime
        .run("Help me shape this feature", &VecEventSink::default())
        .await
        .expect("skill-assisted run");

    let requests = provider.requests().await;
    assert_eq!(requests.len(), 3);
    assert!(
        requests[0]
            .tools
            .iter()
            .any(|definition| definition.name == "search_skills")
    );
    assert!(!requests[0].system_prompt.contains("<active_skill"));
    assert!(!requests[1].system_prompt.contains("<active_skill"));
    assert!(
        requests[2]
            .system_prompt
            .contains("<active_skill name=\"brainstorming\"")
    );
    assert!(
        requests[2]
            .system_prompt
            .contains("Ask focused questions before selecting a design.")
    );

    let loaded = store.load().await.expect("session records");
    let serialized = serde_json::to_string(&loaded.records).expect("serialize records");
    assert!(serialized.contains("brainstorming"));
    assert!(!serialized.contains("Ask focused questions before selecting a design."));
}

#[tokio::test]
async fn assist_mode_loads_a_confident_sampled_skill_and_records_redacted_outcomes() {
    let workspace = TempDir::new().expect("workspace");
    write_skill(
        workspace.path(),
        "brainstorming",
        "Explore product ideas before implementation",
        "Ask focused questions before selecting a design.",
    );
    write_skill(
        workspace.path(),
        "api-design",
        "Design stable service interfaces",
        "Define the interface contract.",
    );
    let provider = Arc::new(FakeProvider::new(vec![response("assisted")]));
    let store = Arc::new(InMemorySessionStore::default());
    let runtime = runtime_with_skills(workspace.path(), provider.clone(), store.clone()).await;
    let typesafe = Arc::new(FakeSystemOne::new());
    typesafe.set_noul("skill_applies", 0.96);
    typesafe.set_choice_probabilities(
        "best_skill",
        [("brainstorming", 0.95), ("api-design", 0.05)],
    );
    runtime
        .attach_typesafe_skill_selector(TypeSafeSkillSelector::with_transport(
            TypeSafeSkillConfig {
                mode: TypeSafeSkillMode::Assist,
                assist_rollout_percent: 100,
                ..TypeSafeSkillConfig::default()
            },
            typesafe,
        ))
        .await;

    let prompt = "Help me shape this feature";
    assert_eq!(
        runtime
            .run(prompt, &VecEventSink::default())
            .await
            .expect("assisted run"),
        "assisted"
    );

    let requests = provider.requests().await;
    assert_eq!(requests.len(), 1);
    assert!(
        requests[0]
            .system_prompt
            .contains("<active_skill name=\"brainstorming\"")
    );
    assert!(requests[0].system_prompt.contains("Ask focused questions"));
    let loaded = store.load().await.expect("session");
    let diagnostics = loaded
        .records
        .iter()
        .filter_map(|record| match &record.payload {
            mimir::session::SessionPayload::RuntimeEvent { name, detail }
                if name.starts_with("typesafe_skill_") =>
            {
                Some(detail.as_str())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(diagnostics.len(), 2);
    assert!(
        diagnostics
            .iter()
            .any(|detail| detail.contains("assist_activated"))
    );
    assert!(
        diagnostics
            .iter()
            .any(|detail| detail.contains("task_succeeded"))
    );
    assert!(diagnostics.iter().all(|detail| !detail.contains(prompt)));
}

#[tokio::test]
async fn explicit_skill_is_ephemeral_and_bare_or_slash_invocations_are_supported() {
    let workspace = TempDir::new().expect("workspace");
    write_skill(
        workspace.path(),
        "review",
        "Review changes",
        "Inspect correctness and tests.",
    );
    let provider = Arc::new(FakeProvider::new(vec![
        response("first"),
        response("second"),
        response("third"),
    ]));
    let store = Arc::new(InMemorySessionStore::default());
    let runtime = runtime_with_skills(workspace.path(), provider.clone(), store.clone()).await;

    runtime
        .run("/skill:review focus on auth", &VecEventSink::default())
        .await
        .expect("slash skill run");
    runtime
        .run("an unrelated request", &VecEventSink::default())
        .await
        .expect("unrelated run");
    runtime
        .run("skill:review focus on storage", &VecEventSink::default())
        .await
        .expect("bare skill run");

    let requests = provider.requests().await;
    assert_eq!(requests.len(), 3);
    assert!(
        requests[0]
            .system_prompt
            .contains("<active_skill name=\"review\"")
    );
    assert!(
        requests[0]
            .system_prompt
            .contains("Inspect correctness and tests.")
    );
    assert!(requests[0].messages[0].text().contains("focus on auth"));
    assert!(!requests[1].system_prompt.contains("<active_skill"));
    assert!(
        requests[2]
            .messages
            .last()
            .expect("latest user message")
            .text()
            .contains("focus on storage")
    );

    let loaded = store.load().await.expect("session records");
    let serialized = serde_json::to_string(&loaded.records).expect("serialize records");
    assert!(!serialized.contains("Inspect correctness and tests."));
    assert!(serialized.contains("/skill:review focus on auth"));
}

#[tokio::test]
async fn unknown_and_malformed_invocations_fail_before_provider_or_persistence() {
    let workspace = TempDir::new().expect("workspace");
    write_skill(workspace.path(), "review", "Review changes", "Review it.");
    let provider = Arc::new(FakeProvider::new(vec![response("unused")]));
    let store = Arc::new(InMemorySessionStore::default());
    let runtime = runtime_with_skills(workspace.path(), provider.clone(), store.clone()).await;

    let unknown = runtime
        .run("/skill:missing do work", &VecEventSink::default())
        .await
        .expect_err("unknown skill");
    assert!(unknown.to_string().contains("unknown skill `missing`"));
    let malformed = runtime
        .run("/skill:Bad_Name do work", &VecEventSink::default())
        .await
        .expect_err("malformed skill");
    assert!(malformed.to_string().contains("malformed skill invocation"));
    assert!(provider.requests().await.is_empty());
    assert!(store.load().await.expect("records").records.is_empty());
}

#[test]
fn loader_rejects_oversized_or_malformed_skills() {
    let oversized = TempDir::new().expect("oversized workspace");
    write_skill(
        oversized.path(),
        "review",
        "Review changes",
        &"x".repeat(MAX_SKILL_BODY_BYTES + 1),
    );
    let error = ResourceLoader::new(oversized.path(), oversized.path())
        .expect("loader")
        .load()
        .expect_err("oversized skill");
    assert!(error.to_string().contains("instructions exceed"));

    let malformed = TempDir::new().expect("malformed workspace");
    write_skill(malformed.path(), "Bad_Name", "Review changes", "Review it.");
    let error = ResourceLoader::new(malformed.path(), malformed.path())
        .expect("loader")
        .load()
        .expect_err("invalid skill name");
    assert!(error.to_string().contains("lowercase ASCII"));
}

#[test]
fn active_skill_batch_has_a_separate_combined_memory_bound() {
    let workspace = TempDir::new().expect("workspace");
    write_skill(
        workspace.path(),
        "first",
        "First bounded skill",
        &"a".repeat(55 * 1_024),
    );
    write_skill(
        workspace.path(),
        "second",
        "Second bounded skill",
        &"b".repeat(55 * 1_024),
    );
    let resources = ResourceLoader::new(workspace.path(), workspace.path())
        .expect("loader")
        .load()
        .expect("individually bounded skills");
    let error = SkillRuntime::new(resources.skills)
        .context_for_messages(&[
            Message::user("/skill:first"),
            Message::user("/skill:second"),
        ])
        .expect_err("combined skill limit");
    assert!(
        error
            .to_string()
            .contains("active skill instructions exceed")
    );
}

#[cfg(unix)]
#[test]
fn loader_rejects_skill_symlinks_that_escape_the_resource_boundary() {
    use std::os::unix::fs::symlink;

    let workspace = TempDir::new().expect("workspace");
    let outside = TempDir::new().expect("outside");
    write_skill(
        outside.path(),
        "escape",
        "Escaped skill",
        "Outside instructions.",
    );
    let skills = workspace.path().join(".agents/skills");
    std::fs::create_dir_all(&skills).expect("skills root");
    symlink(
        outside.path().join(".agents/skills/escape"),
        skills.join("escape"),
    )
    .expect("skill symlink");

    let error = ResourceLoader::new(workspace.path(), workspace.path())
        .expect("loader")
        .load()
        .expect_err("escaped symlink");
    assert!(error.to_string().contains("resolves outside"));
}
