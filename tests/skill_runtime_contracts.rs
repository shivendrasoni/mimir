use std::{sync::Arc, time::Duration};

use mimir::{
    budget::Budget,
    extensions::ExtensionHostAction,
    model::{Content, Message, ModelResponse, StopReason, ToolCall},
    provider::FakeProvider,
    resources::ResourceLoader,
    runtime::{AgentRuntime, RuntimeConfig, VecEventSink},
    session::{InMemorySessionStore, SessionStore},
    skills::SkillRuntime,
    tools::{ToolPolicy, ToolRegistry},
    tui::{Action, App, AppConfig},
    typesafe::{TypeSafeConfig, TypeSafeMode, TypeSafeSkillSelector},
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

fn two_test_skills(root: &std::path::Path) -> SkillRuntime {
    write_skill(
        root,
        "brainstorming",
        "Explore product ideas before implementation",
        "Ask focused questions before selecting a design.",
    );
    write_skill(
        root,
        "api-design",
        "Design stable service interfaces",
        "Define the interface contract.",
    );
    let resources = ResourceLoader::new(root, root)
        .expect("resource loader")
        .load()
        .expect("resources");
    SkillRuntime::new(resources.skills)
}

fn typesafe_tool_diagnostics(records: &[mimir::session::SessionRecord]) -> String {
    records
        .iter()
        .filter_map(|record| match &record.payload {
            mimir::session::SessionPayload::RuntimeEvent { name, detail }
                if name.starts_with("typesafe_tool_") =>
            {
                Some(detail.as_str())
            }
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
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
async fn typesafe_on_loads_a_confident_skill_and_records_redacted_outcomes() {
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
            TypeSafeConfig {
                mode: TypeSafeMode::On,
                ..TypeSafeConfig::default()
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
            .any(|detail| detail.contains("\"decision\":\"activated\""))
    );
    assert!(
        diagnostics
            .iter()
            .any(|detail| detail.contains("task_succeeded"))
    );
    assert!(diagnostics.iter().all(|detail| !detail.contains(prompt)));
}

#[tokio::test]
async fn typesafe_shortlists_tools_and_search_tools_recovers_an_omission() {
    let workspace = TempDir::new().expect("workspace");
    let skills = two_test_skills(workspace.path());
    let mut tools =
        ToolRegistry::with_default_tools(workspace.path(), ToolPolicy::default()).expect("tools");
    tools
        .register_skill_search(&skills)
        .expect("skill search tool");
    tools.register_tool_search().expect("tool recovery search");

    let provider = Arc::new(FakeProvider::new(vec![
        tool_response(
            "hidden-tool",
            "write_file",
            serde_json::json!({"path": "should-not-exist.txt", "content": "blocked"}),
        ),
        tool_response(
            "recover-tool",
            "search_tools",
            serde_json::json!({"query": "write_file", "limit": 1}),
        ),
        response("recovered"),
    ]));
    let store = Arc::new(InMemorySessionStore::default());
    let runtime = AgentRuntime::resume(
        provider.clone(),
        Arc::new(tools),
        store.clone(),
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

    let typesafe = Arc::new(FakeSystemOne::new());
    typesafe.set_noul("skill_applies", 0.96);
    typesafe.set_choice_probabilities(
        "best_skill",
        [("brainstorming", 0.95), ("api-design", 0.05)],
    );
    // BTreeMap definition order: edit_file, list_files, read_file, search, write_file.
    for (id, probability) in [
        ("tool_needed_0", 0.02),
        ("tool_needed_1", 0.02),
        ("tool_needed_2", 0.94),
        ("tool_needed_3", 0.91),
        ("tool_needed_4", 0.02),
    ] {
        typesafe.set_noul(id, probability);
    }
    runtime
        .attach_typesafe_skill_selector(TypeSafeSkillSelector::with_transport(
            TypeSafeConfig {
                mode: TypeSafeMode::On,
                ..TypeSafeConfig::default()
            },
            typesafe,
        ))
        .await;

    let prompt = "Inspect the workspace and prepare the requested change";
    assert_eq!(
        runtime
            .run(prompt, &VecEventSink::default())
            .await
            .expect("shortlisted run"),
        "recovered"
    );

    let requests = provider.requests().await;
    assert_eq!(requests.len(), 3);
    let first = requests[0]
        .tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect::<Vec<_>>();
    assert!(first.contains(&"read_file"));
    assert!(first.contains(&"search"));
    assert!(first.contains(&"search_tools"));
    assert!(!first.contains(&"write_file"));
    assert!(!workspace.path().join("should-not-exist.txt").exists());
    assert!(
        requests[2]
            .tools
            .iter()
            .any(|tool| tool.name == "write_file")
    );

    let records = store.load().await.expect("session").records;
    let serialized = typesafe_tool_diagnostics(&records);
    assert!(serialized.contains("typesafe_tool_selection"));
    assert!(serialized.contains("typesafe_tool_recovery"));
    assert!(!serialized.contains(prompt));
}

#[tokio::test]
async fn typesafe_tool_uncertainty_keeps_the_complete_pool() {
    let workspace = TempDir::new().expect("workspace");
    let mut tools =
        ToolRegistry::with_default_tools(workspace.path(), ToolPolicy::default()).expect("tools");
    tools.register_tool_search().expect("tool recovery search");
    let provider = Arc::new(FakeProvider::new(vec![response("full pool")]));
    let store = Arc::new(InMemorySessionStore::default());
    let runtime = AgentRuntime::resume(
        provider.clone(),
        Arc::new(tools),
        store.clone(),
        RuntimeConfig::default_for_model("fake-model"),
    )
    .await
    .expect("runtime");
    let typesafe = Arc::new(FakeSystemOne::new());
    // BTreeMap definition order: edit_file, list_files, read_file, search, write_file.
    for (id, probability) in [
        ("tool_needed_0", 0.57),
        ("tool_needed_1", 0.02),
        ("tool_needed_2", 0.94),
        ("tool_needed_3", 0.02),
        ("tool_needed_4", 0.02),
    ] {
        typesafe.set_noul(id, probability);
    }
    runtime
        .attach_typesafe_skill_selector(TypeSafeSkillSelector::with_transport(
            TypeSafeConfig {
                mode: TypeSafeMode::On,
                ..TypeSafeConfig::default()
            },
            typesafe,
        ))
        .await;

    runtime
        .run("Read the configuration", &VecEventSink::default())
        .await
        .expect("fallback run");

    let requests = provider.requests().await;
    let names = requests[0]
        .tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect::<Vec<_>>();
    for expected in [
        "edit_file",
        "list_files",
        "read_file",
        "search",
        "write_file",
        "search_tools",
    ] {
        assert!(names.contains(&expected));
    }
    let diagnostics = store
        .load()
        .await
        .expect("session")
        .records
        .into_iter()
        .filter_map(|record| match record.payload {
            mimir::session::SessionPayload::RuntimeEvent { name, detail }
                if name == "typesafe_tool_selection" =>
            {
                Some(detail)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(
        diagnostics
            .iter()
            .any(|detail| detail.contains("fallback_uncertain"))
    );
}

#[tokio::test]
async fn typesafe_tool_shortlist_is_disabled_when_recovery_is_inactive() {
    let workspace = TempDir::new().expect("workspace");
    let mut tools =
        ToolRegistry::with_default_tools(workspace.path(), ToolPolicy::default()).expect("tools");
    tools.register_tool_search().expect("tool recovery search");
    let provider = Arc::new(FakeProvider::new(vec![response("extension pool")]));
    let runtime = AgentRuntime::resume(
        provider.clone(),
        Arc::new(tools),
        Arc::new(InMemorySessionStore::default()),
        RuntimeConfig::default_for_model("fake-model"),
    )
    .await
    .expect("runtime");
    runtime
        .apply_extension_actions(&[ExtensionHostAction::SetActiveTools {
            names: vec!["read_file".into(), "write_file".into()],
        }])
        .await
        .expect("active tool override");
    let typesafe = Arc::new(FakeSystemOne::new());
    runtime
        .attach_typesafe_skill_selector(TypeSafeSkillSelector::with_transport(
            TypeSafeConfig {
                mode: TypeSafeMode::On,
                ..TypeSafeConfig::default()
            },
            typesafe.clone(),
        ))
        .await;

    runtime
        .run("Update the file", &VecEventSink::default())
        .await
        .expect("run");

    assert_eq!(typesafe.request_count(), 0);
    let requests = provider.requests().await;
    let names = requests[0]
        .tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(names, ["read_file", "write_file"]);
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
fn loader_defers_large_bodies_to_activation_but_rejects_malformed_metadata() {
    let large = TempDir::new().expect("large workspace");
    write_skill(
        large.path(),
        "review",
        "Review changes",
        &"x".repeat(80 * 1_024),
    );
    let resources = ResourceLoader::new(large.path(), large.path())
        .expect("loader")
        .load()
        .expect("large skill is discovered from metadata");
    let context = SkillRuntime::new(resources.skills)
        .context_for_messages(&[Message::user("/skill:review")])
        .expect("large skill fits activation context")
        .expect("active context");
    assert!(context.contains(&"x".repeat(80 * 1_024)));

    let malformed = TempDir::new().expect("malformed workspace");
    write_skill(malformed.path(), "Bad_Name", "Review changes", "Review it.");
    let error = ResourceLoader::new(malformed.path(), malformed.path())
        .expect("loader")
        .load()
        .expect_err("invalid skill name");
    assert!(error.to_string().contains("lowercase ASCII"));
}

#[test]
fn skill_larger_than_active_context_is_listed_but_fails_on_activation() {
    let workspace = TempDir::new().expect("workspace");
    write_skill(
        workspace.path(),
        "review",
        "Review changes",
        &"x".repeat(100 * 1_024),
    );
    let resources = ResourceLoader::new(workspace.path(), workspace.path())
        .expect("loader")
        .load()
        .expect("oversized instructions do not block discovery");
    assert_eq!(resources.skills[0].name, "review");
    let error = SkillRuntime::new(resources.skills)
        .context_for_messages(&[Message::user("/skill:review")])
        .expect_err("activation context remains bounded");
    let message = error.to_string();
    assert!(message.contains("would use"));
    assert!(message.contains("98304-byte limit"));
    assert!(message.contains("referenced files"));
}

#[test]
fn deferred_skill_body_failures_do_not_block_catalog_discovery() {
    let workspace = TempDir::new().expect("workspace");
    let directory = workspace.path().join(".agents/skills/review");
    std::fs::create_dir_all(&directory).expect("skill directory");
    let path = directory.join("SKILL.md");
    let mut invalid_utf8 = b"---\nname: review\ndescription: Review changes\n---\n".to_vec();
    invalid_utf8.extend_from_slice(&[0xff, 0xfe]);
    std::fs::write(&path, invalid_utf8).expect("skill with invalid body encoding");

    let resources = ResourceLoader::new(workspace.path(), workspace.path())
        .expect("loader")
        .load()
        .expect("frontmatter-only discovery");
    assert_eq!(resources.skills[0].name, "review");
    let error = SkillRuntime::new(resources.skills)
        .context_for_messages(&[Message::user("/skill:review")])
        .expect_err("body encoding is validated at activation");
    assert!(error.to_string().contains("must be UTF-8 text"));
}

#[test]
fn activation_rejects_deleted_or_metadata_replaced_skill_sources() {
    let deleted_workspace = TempDir::new().expect("deleted workspace");
    write_skill(
        deleted_workspace.path(),
        "review",
        "Review changes",
        "Review it.",
    );
    let deleted_resources = ResourceLoader::new(deleted_workspace.path(), deleted_workspace.path())
        .expect("loader")
        .load()
        .expect("resources");
    std::fs::remove_file(
        deleted_workspace
            .path()
            .join(".agents/skills/review/SKILL.md"),
    )
    .expect("remove discovered source");
    let error = SkillRuntime::new(deleted_resources.skills)
        .context_for_messages(&[Message::user("/skill:review")])
        .expect_err("missing source is rejected");
    assert!(error.to_string().contains("cannot activate skill `review`"));

    let replaced_workspace = TempDir::new().expect("replaced workspace");
    write_skill(
        replaced_workspace.path(),
        "review",
        "Review changes",
        "Review it.",
    );
    let replaced_resources =
        ResourceLoader::new(replaced_workspace.path(), replaced_workspace.path())
            .expect("loader")
            .load()
            .expect("resources");
    write_skill(
        replaced_workspace.path(),
        "review",
        "Different catalog description",
        "Changed instructions.",
    );
    let error = SkillRuntime::new(replaced_resources.skills)
        .context_for_messages(&[Message::user("/skill:review")])
        .expect_err("stale catalog identity is rejected");
    assert!(
        error
            .to_string()
            .contains("metadata changed after discovery")
    );
}

#[test]
fn oversized_frontmatter_remains_a_discovery_error() {
    let workspace = TempDir::new().expect("workspace");
    let directory = workspace.path().join(".agents/skills/review");
    std::fs::create_dir_all(&directory).expect("skill directory");
    std::fs::write(
        directory.join("SKILL.md"),
        format!(
            "---\nname: review\ndescription: Review changes\nmetadata:\n  padding: {}\n---\nReview it.\n",
            "x".repeat(17 * 1_024)
        ),
    )
    .expect("oversized frontmatter");
    let error = ResourceLoader::new(workspace.path(), workspace.path())
        .expect("loader")
        .load()
        .expect_err("oversized frontmatter");
    assert!(
        error
            .to_string()
            .contains("frontmatter exceeds 16384 bytes")
    );
}

#[tokio::test]
async fn search_activation_reports_an_oversized_skill_without_loading_it() {
    let workspace = TempDir::new().expect("workspace");
    write_skill(
        workspace.path(),
        "review",
        "Review changes",
        &"x".repeat(100 * 1_024),
    );
    let provider = Arc::new(FakeProvider::new(vec![
        tool_response(
            "activate-review",
            "search_skills",
            serde_json::json!({"name": "review"}),
        ),
        response("continued without the oversized skill"),
    ]));
    let store = Arc::new(InMemorySessionStore::default());
    let runtime = runtime_with_skills(workspace.path(), provider.clone(), store).await;

    assert_eq!(
        runtime
            .run("Review this change", &VecEventSink::default())
            .await
            .expect("tool error is recoverable"),
        "continued without the oversized skill"
    );
    let requests = provider.requests().await;
    assert_eq!(requests.len(), 2);
    assert!(!requests[1].system_prompt.contains("<active_skill"));
    assert!(
        requests[1]
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .any(|content| matches!(
                content,
                Content::ToolResult(result) if result.content.contains("98304-byte limit")
            ))
    );
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
    assert!(error.to_string().contains("active skill context"));
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
