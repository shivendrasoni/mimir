use std::{
    collections::BTreeSet,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use mimir::{
    extensions::{
        Capability, ExtensionEntrypoint, ExtensionManifest, ExtensionRuntime, HostLimits,
        LifecycleEvent, LifecycleInterception, LifecycleReplacement, RuntimeLimits,
    },
    model::{Content, Message, ThinkingLevel},
};
use tempfile::TempDir;

fn compile_fixture(root: &Path) -> PathBuf {
    let source = root.join("interception_fixture.rs");
    let binary = root.join("interception-fixture");
    std::fs::write(
        &source,
        r##"
use std::{env, io::{self, Read}};

fn field(input: &str, marker: &str, end: char) -> String {
    let start = input.find(marker).expect("field") + marker.len();
    let tail = &input[start..];
    tail[..tail.find(end).expect("field end")].to_owned()
}

fn main() {
    let mode = env::args().nth(1).unwrap_or_else(|| "replace_model_select".into());
    let mut input = String::new();
    io::stdin().read_to_string(&mut input).expect("stdin");
    let id = field(&input, "\"id\":\"", '"');
    let generation = field(&input, "\"generation\":", ',');
    let initialize = input.contains("\"type\":\"initialize\"");
    let with_generation = |template: &str| template.replace("__GEN__", &generation);
    let response = if initialize {
        let lifecycle_events = match mode.as_str() {
            "turn_start_mutation" => r#"["turn_start"]"#,
            "refine_complete_replace" => r#"["refine_complete"]"#,
            _ => r#"["model_select"]"#,
        };
        with_generation(
            r#"{"abi_version":1,"generation":__GEN__,"response":{"type":"registration","registrations":{"lifecycle_events":__LIFECYCLE_EVENTS__}}}"#,
        )
        .replace("__LIFECYCLE_EVENTS__", lifecycle_events)
    } else {
        match mode.as_str() {
            "replace_model_select" => with_generation(
                r#"{"abi_version":1,"generation":__GEN__,"response":{"type":"lifecycle","outcome":{"cancel":false,"output":null,"ui_requests":[],"interception":{"kind":"replace","replacement":{"kind":"model_select","provider":"fixture-provider","model":"fixture-model"}}}}}"#,
            ),
            "block_model_select" => with_generation(
                r#"{"abi_version":1,"generation":__GEN__,"response":{"type":"lifecycle","outcome":{"cancel":true,"output":null,"ui_requests":[],"interception":{"kind":"block","reason":"blocked by fixture"}}}}"#,
            ),
            "mismatched_replacement" => with_generation(
                r#"{"abi_version":1,"generation":__GEN__,"response":{"type":"lifecycle","outcome":{"cancel":false,"output":null,"ui_requests":[],"interception":{"kind":"replace","replacement":{"kind":"input","message":{"role":"user","content":[{"type":"text","text":"replacement"}],"stop_reason":null,"usage":{"input_tokens":0,"output_tokens":0,"cached_tokens":0},"timestamp_ms":0}}}}}}"#,
            ),
            "turn_start_mutation" => with_generation(
                r#"{"abi_version":1,"generation":__GEN__,"response":{"type":"lifecycle","outcome":{"cancel":false,"output":null,"ui_requests":[],"interception":{"kind":"mutate","mutation":{"kind":"model_select","provider":"fixture-provider","model":"fixture-model"}}}}}"#,
            ),
            "cancel_and_mutate" => with_generation(
                r#"{"abi_version":1,"generation":__GEN__,"response":{"type":"lifecycle","outcome":{"cancel":true,"output":null,"ui_requests":[],"interception":{"kind":"mutate","mutation":{"kind":"model_select","provider":"fixture-provider","model":"fixture-model"}}}}}"#,
            ),
            "refine_complete_replace" => with_generation(
                r#"{"abi_version":1,"generation":__GEN__,"response":{"type":"lifecycle","outcome":{"cancel":false,"output":{"handled":true},"ui_requests":[],"interception":{"kind":"replace","replacement":{"kind":"refine_complete","result":{"summary":"refined"}}}}}}"#,
            ),
            other => panic!("unsupported mode: {other}"),
        }
    };
    println!(r#"{{"schema_version":1,"id":"{id}","status":"ok","output":{response}}}"#);
}
"##,
    )
    .expect("fixture source");
    let status = Command::new("rustc")
        .args(["--edition=2024", "-o"])
        .arg(&binary)
        .arg(&source)
        .status()
        .expect("run rustc");
    assert!(status.success(), "fixture compilation failed");
    let mut permissions = std::fs::metadata(&binary)
        .expect("fixture metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&binary, permissions).expect("fixture permissions");
    binary
}

fn manifest(binary: &Path, mode: &str) -> ExtensionManifest {
    ExtensionManifest {
        schema_version: 1,
        name: "interception-fixture".into(),
        version: "1.0.0".into(),
        entrypoint: ExtensionEntrypoint::NativeProcess {
            program: binary.display().to_string(),
            args: vec![mode.into()],
        },
        capabilities: [
            Capability::Lifecycle,
            Capability::Process,
            Capability::UnrestrictedNative,
        ]
        .into_iter()
        .collect::<BTreeSet<_>>(),
    }
}

fn limits() -> RuntimeLimits {
    RuntimeLimits {
        host: HostLimits {
            max_request_bytes: 16 * 1024,
            max_response_bytes: 64 * 1024,
            timeout: Duration::from_secs(5),
        },
        max_concurrency: 1,
        max_registrations: 32,
    }
}

async fn load_runtime(mode: &str) -> (TempDir, TempDir, TempDir, ExtensionRuntime) {
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    let fixture_root = TempDir::new().expect("fixture");
    let binary = compile_fixture(fixture_root.path());
    let runtime = ExtensionRuntime::load(
        manifest(&binary, mode),
        workspace.path(),
        state.path(),
        limits(),
        "initialize-1",
    )
    .await
    .expect("load runtime");
    (workspace, state, fixture_root, runtime)
}

#[test]
fn lifecycle_context_and_input_payloads_serialize_with_model_types() {
    let event = LifecycleEvent::Context {
        session_id: "session-1".into(),
        messages: vec![Message::user_content(vec![Content::Text {
            text: "hello".into(),
        }])],
    };
    let encoded = serde_json::to_value(&event).expect("serialize event");
    assert_eq!(encoded["type"], "context");
    assert_eq!(encoded["messages"][0]["content"][0]["type"], "text");

    let decoded: LifecycleEvent = serde_json::from_value(encoded).expect("deserialize event");
    assert_eq!(decoded, event);
}

#[tokio::test]
async fn matching_model_select_replacement_dispatches() {
    let (_workspace, _state, _fixture_root, runtime) = load_runtime("replace_model_select").await;
    let outcome = runtime
        .dispatch(
            "event-1",
            LifecycleEvent::ModelSelect {
                session_id: "session-1".into(),
                provider: "openai".into(),
                model: "gpt-5.4".into(),
            },
        )
        .await
        .expect("dispatch")
        .expect("subscribed event");
    assert_eq!(
        outcome.interception,
        LifecycleInterception::Replace {
            replacement: LifecycleReplacement::ModelSelect {
                provider: "fixture-provider".into(),
                model: "fixture-model".into(),
            },
        }
    );
}

#[tokio::test]
async fn matching_block_outcome_is_accepted_for_model_select() {
    let (_workspace, _state, _fixture_root, runtime) = load_runtime("block_model_select").await;
    let outcome = runtime
        .dispatch(
            "event-1",
            LifecycleEvent::ModelSelect {
                session_id: "session-1".into(),
                provider: "openai".into(),
                model: "gpt-5.4".into(),
            },
        )
        .await
        .expect("dispatch")
        .expect("subscribed event");
    assert!(outcome.cancel);
    assert_eq!(
        outcome.interception,
        LifecycleInterception::Block {
            reason: Some("blocked by fixture".into()),
        }
    );
}

#[tokio::test]
async fn mismatched_replacement_kind_fails_closed() {
    let (_workspace, _state, _fixture_root, runtime) = load_runtime("mismatched_replacement").await;
    let error = runtime
        .dispatch(
            "event-1",
            LifecycleEvent::ModelSelect {
                session_id: "session-1".into(),
                provider: "openai".into(),
                model: "gpt-5.4".into(),
            },
        )
        .await
        .expect_err("mismatched replacement must fail");
    assert!(
        error
            .to_string()
            .contains("replacement kind Input does not match lifecycle event ModelSelect")
    );
}

#[tokio::test]
async fn mutation_is_rejected_for_non_interception_events() {
    let (_workspace, _state, _fixture_root, runtime) = load_runtime("turn_start_mutation").await;
    let error = runtime
        .dispatch(
            "event-1",
            LifecycleEvent::TurnStart {
                session_id: "session-1".into(),
                turn_index: 2,
            },
        )
        .await
        .expect_err("turn_start mutation must fail");
    assert!(
        error
            .to_string()
            .contains("mutation kind ModelSelect does not match lifecycle event TurnStart")
    );
}

#[tokio::test]
async fn cancel_cannot_be_combined_with_mutation() {
    let (_workspace, _state, _fixture_root, runtime) = load_runtime("cancel_and_mutate").await;
    let error = runtime
        .dispatch(
            "event-1",
            LifecycleEvent::ModelSelect {
                session_id: "session-1".into(),
                provider: "openai".into(),
                model: "gpt-5.4".into(),
            },
        )
        .await
        .expect_err("cancel plus mutation must fail");
    assert!(
        error
            .to_string()
            .contains("cannot cancel and mutate the same event")
    );
}

#[tokio::test]
async fn refine_complete_replacement_round_trips() {
    let (_workspace, _state, _fixture_root, runtime) =
        load_runtime("refine_complete_replace").await;
    let outcome = runtime
        .dispatch(
            "event-1",
            LifecycleEvent::RefineComplete {
                session_id: "session-1".into(),
                result: serde_json::json!({"summary": "original"}),
            },
        )
        .await
        .expect("dispatch")
        .expect("subscribed event");
    assert_eq!(outcome.output, serde_json::json!({"handled": true}));
    match outcome.interception {
        LifecycleInterception::Replace {
            replacement: LifecycleReplacement::RefineComplete { result },
        } => assert_eq!(result, serde_json::json!({"summary": "refined"})),
        other => panic!("unexpected interception: {other:?}"),
    }
}

#[test]
fn thinking_level_event_uses_shared_model_enum() {
    let event = LifecycleEvent::ThinkingLevelSelect {
        session_id: "session-1".into(),
        thinking_level: ThinkingLevel::High,
    };
    let encoded = serde_json::to_value(&event).expect("serialize event");
    assert_eq!(encoded["type"], "thinking_level_select");
    assert_eq!(encoded["thinking_level"], "high");
}
