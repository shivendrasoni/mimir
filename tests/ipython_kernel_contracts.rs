use std::{sync::Arc, time::Duration};

use mimir::tools::{ObservationStatus, ToolPolicy, ToolRegistry};
use serde_json::json;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

fn policy(timeout: Duration, output: usize) -> ToolPolicy {
    ToolPolicy {
        allow_process: true,
        command_timeout: timeout,
        max_output_bytes: output,
        ..ToolPolicy::default()
    }
}

fn registry(
    workspace: &TempDir,
    state: &TempDir,
    session: &str,
    policy: ToolPolicy,
) -> ToolRegistry {
    let mut registry =
        ToolRegistry::with_default_tools(workspace.path(), policy.clone()).expect("default tools");
    registry
        .register_ipython_kernel(workspace.path(), state.path(), session, policy)
        .expect("IPython kernel");
    registry
}

#[tokio::test]
async fn kernel_schema_is_compatible_and_python_state_persists_per_session() {
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    let first = registry(
        &workspace,
        &state,
        "session-a",
        policy(Duration::from_secs(3), 16 * 1024),
    );
    let definition = first
        .definitions()
        .into_iter()
        .find(|definition| definition.name == "ipython")
        .expect("definition");
    assert_eq!(definition.parameters["required"], json!(["code"]));
    assert_eq!(definition.parameters["additionalProperties"], false);

    first
        .execute("ipython", json!({"code": "x = 40"}))
        .await
        .expect("assign");
    let result = first
        .execute("ipython", json!({"code": "x + 2"}))
        .await
        .expect("evaluate");
    assert_eq!(result.status, ObservationStatus::Success);
    assert_eq!(result.content, "42");

    let second = registry(
        &workspace,
        &state,
        "session-b",
        policy(Duration::from_secs(3), 16 * 1024),
    );
    let isolated = second
        .execute("ipython", json!({"code": "x"}))
        .await
        .expect("kernel error is an observation");
    assert_eq!(isolated.status, ObservationStatus::Error);
    assert!(isolated.content.contains("NameError"));
}

#[tokio::test]
async fn kernel_captures_stdout_stderr_errors_and_rich_outputs() {
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    let registry = registry(
        &workspace,
        &state,
        "rich-session",
        policy(Duration::from_secs(3), 16 * 1024),
    );
    let text = registry
        .execute(
            "ipython",
            json!({"code": "import sys\nprint('out')\nprint('err', file=sys.stderr)\n6 * 7"}),
        )
        .await
        .expect("text outputs");
    assert_eq!(text.status, ObservationStatus::Success);
    assert!(text.content.contains("out"));
    assert!(text.content.contains("err"));
    assert!(text.content.contains("42"));

    let rich = registry
        .execute(
            "ipython",
            json!({"code": "class R:\n    def _repr_html_(self): return '<b>rich</b>'\nR()"}),
        )
        .await
        .expect("rich output");
    assert!(
        rich.content
            .contains("<rich_output mime_type=\"text/html\">")
    );
    assert!(rich.content.contains("<b>rich</b>"));

    let image = registry
        .execute(
            "ipython",
            json!({"code": "class P:\n    def _repr_png_(self): return b'bounded-image'\nP()"}),
        )
        .await
        .expect("image output");
    assert_eq!(image.artifacts.len(), 1);
    assert_eq!(
        std::fs::read(&image.artifacts[0]).expect("image artifact"),
        b"bounded-image"
    );

    let error = registry
        .execute("ipython", json!({"code": "raise ValueError('bad cell')"}))
        .await
        .expect("execution error observation");
    assert_eq!(error.status, ObservationStatus::Error);
    assert!(error.content.contains("ValueError: bad cell"));
}

#[tokio::test]
async fn kernel_output_is_bounded_and_timeout_restarts_namespace() {
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    let registry = registry(
        &workspace,
        &state,
        "bounded-session",
        policy(Duration::from_millis(150), 2 * 1024),
    );
    let bounded = registry
        .execute("ipython", json!({"code": "print('x' * 100000)"}))
        .await
        .expect("bounded output");
    assert_eq!(bounded.status, ObservationStatus::Error);
    assert!(bounded.summary.contains("output truncated"));
    assert!(bounded.content.len() <= 2 * 1024);

    registry
        .execute("ipython", json!({"code": "survivor = 42"}))
        .await
        .expect("state");
    let timeout = registry
        .execute("ipython", json!({"code": "import time; time.sleep(30)"}))
        .await
        .expect_err("timeout");
    assert!(timeout.to_string().contains("timed out"));
    let restarted = registry
        .execute("ipython", json!({"code": "survivor"}))
        .await
        .expect("restarted kernel");
    assert_eq!(restarted.status, ObservationStatus::Error);
    assert!(restarted.content.contains("NameError"));
}

#[tokio::test]
async fn runtime_cancellation_interrupts_a_cell_and_kernel_remains_usable() {
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    let registry = Arc::new(registry(
        &workspace,
        &state,
        "cancel-session",
        policy(Duration::from_secs(30), 16 * 1024),
    ));
    let cancellation = CancellationToken::new();
    let running = {
        let registry = Arc::clone(&registry);
        let cancellation = cancellation.clone();
        tokio::spawn(async move {
            registry
                .execute_cancellable(
                    "ipython",
                    json!({"code": "import time; time.sleep(30)"}),
                    &cancellation,
                )
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    cancellation.cancel();
    let interrupted = tokio::time::timeout(Duration::from_secs(4), running)
        .await
        .expect("bounded cancellation")
        .expect("join")
        .expect("aborted observation");
    assert_eq!(interrupted.status, ObservationStatus::Error);
    assert!(interrupted.summary.contains("aborted"));

    let next = registry
        .execute("ipython", json!({"code": "21 * 2"}))
        .await
        .expect("kernel remains usable");
    assert_eq!(next.content, "42");
}

#[tokio::test]
async fn bash_magic_is_policy_gated_and_bounded() {
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    let disabled_policy = ToolPolicy {
        allow_process: false,
        ..ToolPolicy::default()
    };
    let mut disabled =
        ToolRegistry::with_default_tools(workspace.path(), disabled_policy.clone()).expect("tools");
    let error = disabled
        .register_ipython_kernel(workspace.path(), state.path(), "disabled", disabled_policy)
        .expect_err("process policy");
    assert!(error.to_string().contains("disabled by policy"));

    let enabled = registry(
        &workspace,
        &state,
        "bash-session",
        policy(Duration::from_secs(3), 4096),
    );
    let output = enabled
        .execute("ipython", json!({"code": "%%bash\nprintf 'bash-cell-ok'"}))
        .await
        .expect("bash cell");
    assert_eq!(output.status, ObservationStatus::Success);
    assert_eq!(output.content, "bash-cell-ok");
}
