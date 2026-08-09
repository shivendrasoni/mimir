use std::{
    collections::BTreeSet,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
    time::Duration,
};

use mimir::extensions::{
    Capability, ExtensionEntrypoint, ExtensionManifest, ExtensionRuntime, HostLimits,
    LifecycleEvent, RuntimeLimits, SessionStartReason,
};
use tempfile::TempDir;

fn compile_fixture(root: &Path) -> PathBuf {
    let source = root.join("extension_fixture.rs");
    let binary = root.join("extension-fixture");
    std::fs::write(
        &source,
        r##"
use std::{env, io::{self, Read}, thread, time::Duration};

fn field(input: &str, marker: &str, end: char) -> String {
    let start = input.find(marker).expect("field") + marker.len();
    let tail = &input[start..];
    tail[..tail.find(end).expect("field end")].to_owned()
}

fn main() {
    let mode = env::args().nth(1).unwrap_or_else(|| "full".into());
    let mut input = String::new();
    io::stdin().read_to_string(&mut input).expect("stdin");
    let id = field(&input, "\"id\":\"", '"');
    let generation = field(&input, "\"generation\":", ',');
    let initialize = input.contains("\"type\":\"initialize\"");
    if !initialize && mode == "slow" {
        thread::sleep(Duration::from_millis(500));
    }
    let response = if initialize {
        let registrations = if mode == "tool_only" {
            r#"{"tools":[{"name":"fixture_tool","label":"Fixture","description":"test tool","parameters":{"type":"object"}}]}"#
        } else {
            r#"{"tools":[{"name":"fixture_tool","label":"Fixture","description":"test tool","parameters":{"type":"object"}}],"commands":[{"name":"fixture","description":"test command"}],"ui_requests":["notify"],"renderers":[{"custom_type":"fixture_message"}],"providers":[{"name":"fixture_provider","transport":"open_ai_compatible","models":["fixture-model"]}],"lifecycle_events":["turn_start","session_start"]}"#
        };
        format!(r#"{{"abi_version":1,"generation":{generation},"response":{{"type":"registration","registrations":{registrations}}}}}"#)
    } else {
        let cancel = mode == "cancel";
        format!(r#"{{"abi_version":1,"generation":{generation},"response":{{"type":"lifecycle","outcome":{{"cancel":{cancel},"output":{{"handled":true}},"ui_requests":[{{"kind":"notify","level":"info","message":"fixture handled event"}}]}}}}}}"#)
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

fn manifest(binary: &Path, mode: &str, capabilities: &[Capability]) -> ExtensionManifest {
    ExtensionManifest {
        schema_version: 1,
        name: "runtime-fixture".into(),
        version: "1.0.0".into(),
        entrypoint: ExtensionEntrypoint::NativeProcess {
            program: binary.display().to_string(),
            args: vec![mode.into()],
        },
        capabilities: capabilities.iter().copied().collect::<BTreeSet<_>>(),
    }
}

fn limits(max_concurrency: usize) -> RuntimeLimits {
    RuntimeLimits {
        host: HostLimits {
            max_request_bytes: 16 * 1024,
            max_response_bytes: 64 * 1024,
            timeout: Duration::from_secs(5),
        },
        max_concurrency,
        max_registrations: 32,
    }
}

#[tokio::test]
async fn native_subprocess_runtime_requires_explicit_process_authority() {
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    let error = ExtensionRuntime::load(
        manifest(
            Path::new("/bin/echo"),
            "unused",
            &[Capability::Lifecycle, Capability::Process],
        ),
        workspace.path(),
        state.path(),
        limits(1),
        "initialize-1",
    )
    .await
    .expect_err("native process authority");
    assert!(error.to_string().contains("unrestricted_native"));
}

#[tokio::test]
async fn typed_registration_and_lifecycle_dispatch_are_capability_gated() {
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    let fixture_root = TempDir::new().expect("fixture");
    let binary = compile_fixture(fixture_root.path());
    let runtime = ExtensionRuntime::load(
        manifest(
            &binary,
            "full",
            &[
                Capability::Tools,
                Capability::Commands,
                Capability::Ui,
                Capability::Provider,
                Capability::Lifecycle,
                Capability::Process,
                Capability::UnrestrictedNative,
            ],
        ),
        workspace.path(),
        state.path(),
        limits(2),
        "initialize-1",
    )
    .await
    .expect("load runtime");

    assert_eq!(runtime.generation(), 1);
    assert_eq!(runtime.registrations().tools[0].name, "fixture_tool");
    assert_eq!(runtime.registrations().commands[0].name, "fixture");
    assert_eq!(
        runtime.registrations().renderers[0].custom_type,
        "fixture_message"
    );
    assert_eq!(
        runtime.registrations().providers[0].name,
        "fixture_provider"
    );

    let outcome = runtime
        .dispatch(
            "event-1",
            LifecycleEvent::TurnStart {
                session_id: "session-1".into(),
                turn_index: 3,
            },
        )
        .await
        .expect("dispatch")
        .expect("subscribed event");
    assert_eq!(outcome.output, serde_json::json!({"handled": true}));
    assert_eq!(outcome.ui_requests.len(), 1);

    let ignored = runtime
        .dispatch(
            "event-2",
            LifecycleEvent::AgentStart {
                session_id: "session-1".into(),
            },
        )
        .await
        .expect("unsubscribed event");
    assert!(ignored.is_none());
}

#[tokio::test]
async fn undeclared_registration_capabilities_fail_closed() {
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    let fixture_root = TempDir::new().expect("fixture");
    let binary = compile_fixture(fixture_root.path());
    let error = ExtensionRuntime::load(
        manifest(
            &binary,
            "tool_only",
            &[
                Capability::Commands,
                Capability::Process,
                Capability::UnrestrictedNative,
            ],
        ),
        workspace.path(),
        state.path(),
        limits(1),
        "initialize-1",
    )
    .await
    .expect_err("tools capability must be declared");
    assert!(error.to_string().contains("tools capability"));
}

#[tokio::test]
async fn reload_generation_is_durable_and_invalidates_stale_runtimes() {
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    let fixture_root = TempDir::new().expect("fixture");
    let binary = compile_fixture(fixture_root.path());
    let capabilities = [
        Capability::Tools,
        Capability::Commands,
        Capability::Ui,
        Capability::Provider,
        Capability::Lifecycle,
        Capability::Process,
        Capability::UnrestrictedNative,
    ];
    let first = ExtensionRuntime::load(
        manifest(&binary, "full", &capabilities),
        workspace.path(),
        state.path(),
        limits(1),
        "initialize-1",
    )
    .await
    .expect("first load");
    let second = ExtensionRuntime::load(
        manifest(&binary, "full", &capabilities),
        workspace.path(),
        state.path(),
        limits(1),
        "initialize-2",
    )
    .await
    .expect("second load");

    assert_eq!(first.generation(), 1);
    assert_eq!(second.generation(), 2);
    let error = first
        .dispatch(
            "stale-event",
            LifecycleEvent::SessionStart {
                session_id: "session-1".into(),
                reason: SessionStartReason::Reload,
                previous_session_file: None,
            },
        )
        .await
        .expect_err("old generation is stale");
    assert!(error.to_string().contains("stale generation"));
}

#[tokio::test]
async fn failed_reload_preserves_the_last_active_generation() {
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    let fixture_root = TempDir::new().expect("fixture");
    let binary = compile_fixture(fixture_root.path());
    let capabilities = [
        Capability::Tools,
        Capability::Commands,
        Capability::Ui,
        Capability::Provider,
        Capability::Lifecycle,
        Capability::Process,
        Capability::UnrestrictedNative,
    ];
    let active = ExtensionRuntime::load(
        manifest(&binary, "full", &capabilities),
        workspace.path(),
        state.path(),
        limits(1),
        "initialize-1",
    )
    .await
    .expect("active runtime");
    ExtensionRuntime::load(
        manifest(
            &binary,
            "tool_only",
            &[
                Capability::Commands,
                Capability::Process,
                Capability::UnrestrictedNative,
            ],
        ),
        workspace.path(),
        state.path(),
        limits(1),
        "initialize-invalid",
    )
    .await
    .expect_err("invalid reload");

    assert_eq!(active.generation(), 1);
    active
        .dispatch(
            "event-after-failure",
            LifecycleEvent::TurnStart {
                session_id: "session-1".into(),
                turn_index: 1,
            },
        )
        .await
        .expect("old runtime remains active")
        .expect("subscribed event");
}

#[tokio::test]
async fn runtime_rejects_work_above_its_concurrency_limit() {
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    let fixture_root = TempDir::new().expect("fixture");
    let binary = compile_fixture(fixture_root.path());
    let runtime = Arc::new(
        ExtensionRuntime::load(
            manifest(
                &binary,
                "slow",
                &[
                    Capability::Tools,
                    Capability::Commands,
                    Capability::Ui,
                    Capability::Provider,
                    Capability::Lifecycle,
                    Capability::Process,
                    Capability::UnrestrictedNative,
                ],
            ),
            workspace.path(),
            state.path(),
            limits(1),
            "initialize-1",
        )
        .await
        .expect("load runtime"),
    );
    let first_runtime = Arc::clone(&runtime);
    let first = tokio::spawn(async move {
        first_runtime
            .dispatch(
                "event-1",
                LifecycleEvent::TurnStart {
                    session_id: "session-1".into(),
                    turn_index: 1,
                },
            )
            .await
    });
    tokio::time::sleep(Duration::from_millis(30)).await;
    let error = runtime
        .dispatch(
            "event-2",
            LifecycleEvent::TurnStart {
                session_id: "session-1".into(),
                turn_index: 2,
            },
        )
        .await
        .expect_err("concurrency limit");
    assert!(error.to_string().contains("concurrency limit"));
    first.await.expect("first join").expect("first dispatch");
}

#[tokio::test]
async fn only_cancellable_lifecycle_events_accept_cancellation() {
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    let fixture_root = TempDir::new().expect("fixture");
    let binary = compile_fixture(fixture_root.path());
    let runtime = ExtensionRuntime::load(
        manifest(
            &binary,
            "cancel",
            &[
                Capability::Tools,
                Capability::Commands,
                Capability::Ui,
                Capability::Provider,
                Capability::Lifecycle,
                Capability::Process,
                Capability::UnrestrictedNative,
            ],
        ),
        workspace.path(),
        state.path(),
        limits(1),
        "initialize-1",
    )
    .await
    .expect("load runtime");

    let error = runtime
        .dispatch(
            "event-1",
            LifecycleEvent::TurnStart {
                session_id: "session-1".into(),
                turn_index: 1,
            },
        )
        .await
        .expect_err("turn start cannot be cancelled");
    assert!(error.to_string().contains("cannot cancel"));
}
