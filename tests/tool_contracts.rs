use std::{sync::Arc, time::Duration};

use mimir::tools::{BashRunner, ObservationStatus, ToolPolicy, ToolRegistry};
use serde_json::json;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

fn registry(root: &TempDir) -> ToolRegistry {
    ToolRegistry::with_default_tools(
        root.path(),
        ToolPolicy {
            command_timeout: Duration::from_millis(250),
            max_output_bytes: 8,
            max_write_bytes: 1024,
            allow_write: true,
            allow_process: true,
            allowed_programs: Some(vec!["sleep".into(), "printf".into()]),
            approvals: None,
        },
    )
    .expect("registry should initialize")
}

#[tokio::test]
async fn file_tools_reject_workspace_traversal_before_io() {
    let root = TempDir::new().expect("tempdir");
    let tools = registry(&root);

    let error = tools
        .execute("read_file", json!({"path": "../outside.txt"}))
        .await
        .expect_err("traversal must be rejected");

    assert!(error.to_string().contains("workspace"));
}

#[tokio::test]
async fn write_edit_and_read_use_the_same_canonical_workspace_policy() {
    let root = TempDir::new().expect("tempdir");
    let tools = registry(&root);

    let written = tools
        .execute(
            "write_file",
            json!({"path": "note.txt", "content": "alpha"}),
        )
        .await
        .expect("write should succeed");
    let edited = tools
        .execute(
            "edit_file",
            json!({"path": "note.txt", "old_text": "alpha", "new_text": "beta"}),
        )
        .await
        .expect("edit should succeed");
    let read = tools
        .execute("read_file", json!({"path": "note.txt"}))
        .await
        .expect("read should succeed");

    assert_eq!(written.status, ObservationStatus::Success);
    assert_eq!(edited.status, ObservationStatus::Success);
    assert_eq!(read.content, "beta");
    assert_eq!(
        read.artifacts,
        vec![
            root.path()
                .canonicalize()
                .expect("canonical root")
                .join("note.txt")
        ]
    );
}

#[tokio::test]
async fn write_file_creates_missing_nested_workspace_directories() {
    let root = TempDir::new().expect("tempdir");
    let tools = registry(&root);

    let written = tools
        .execute(
            "write_file",
            json!({"path": "mockup/react-app/src/main.tsx", "content": "export {};"}),
        )
        .await
        .expect("nested write should create parents");

    assert_eq!(written.status, ObservationStatus::Success);
    assert_eq!(
        std::fs::read_to_string(root.path().join("mockup/react-app/src/main.tsx"))
            .expect("written file"),
        "export {};"
    );
}

#[tokio::test]
async fn process_tool_times_out_and_returns_a_recovery_hint() {
    let root = TempDir::new().expect("tempdir");
    let tools = registry(&root);

    let observation = tools
        .execute("run_process", json!({"program": "sleep", "args": ["1"]}))
        .await
        .expect("timeout is an observation, not a host error");

    assert_eq!(observation.status, ObservationStatus::Error);
    assert!(observation.summary.contains("timed out"));
    assert!(!observation.next_actions.is_empty());
}

#[tokio::test]
async fn process_tool_caps_observation_bytes() {
    let root = TempDir::new().expect("tempdir");
    let tools = ToolRegistry::with_default_tools(
        root.path(),
        ToolPolicy {
            command_timeout: Duration::from_secs(10),
            max_output_bytes: 8,
            max_write_bytes: 1024,
            allow_write: true,
            allow_process: true,
            allowed_programs: Some(vec!["printf".into()]),
            approvals: None,
        },
    )
    .expect("registry should initialize");

    let observation = tools
        .execute(
            "run_process",
            json!({"program": "printf", "args": ["123456789abcdef"]}),
        )
        .await
        .expect("process should execute");

    assert!(observation.content.len() <= 32);
    assert!(
        observation.summary.contains("truncated"),
        "unexpected summary: {}; content: {:?}",
        observation.summary,
        observation.content
    );
}

fn process_registry(
    root: &TempDir,
    timeout: Duration,
    max_output_bytes: usize,
    allowed_programs: &[&str],
) -> ToolRegistry {
    ToolRegistry::with_default_tools(
        root.path(),
        ToolPolicy {
            command_timeout: timeout,
            max_output_bytes,
            max_write_bytes: 1024,
            allow_write: true,
            allow_process: true,
            allowed_programs: Some(
                allowed_programs
                    .iter()
                    .map(|program| (*program).to_owned())
                    .collect(),
            ),
            approvals: None,
        },
    )
    .expect("registry should initialize")
}

#[cfg(unix)]
#[tokio::test]
async fn process_tool_captures_fast_silent_and_search_commands() {
    let root = TempDir::new().expect("tempdir");
    std::fs::write(root.path().join("needle.txt"), "fixture").expect("fixture");
    let tools = process_registry(
        &root,
        Duration::from_secs(10),
        64 * 1024,
        &["mkdir", "find"],
    );

    let mkdir = tools
        .execute(
            "run_process",
            json!({"program": "mkdir", "args": ["-p", "nested/path"]}),
        )
        .await
        .expect("mkdir should execute");
    assert_eq!(mkdir.status, ObservationStatus::Success);
    assert!(!mkdir.summary.contains("timed out"));
    assert!(root.path().join("nested/path").is_dir());

    let find = tools
        .execute(
            "run_process",
            json!({"program": "find", "args": [".", "-name", "needle.txt"]}),
        )
        .await
        .expect("find should execute");
    assert_eq!(find.status, ObservationStatus::Success);
    assert_eq!(find.content.trim(), "./needle.txt");
    assert!(!find.summary.contains("timed out"));
}

#[cfg(unix)]
#[tokio::test]
async fn process_tool_preserves_stderr_and_nonzero_exit() {
    let root = TempDir::new().expect("tempdir");
    let tools = process_registry(&root, Duration::from_secs(10), 1024, &["sh"]);

    let observation = tools
        .execute(
            "run_process",
            json!({"program": "sh", "args": ["-c", "printf failure >&2; exit 7"]}),
        )
        .await
        .expect("process should execute");

    assert_eq!(observation.status, ObservationStatus::Error);
    assert_eq!(observation.content, "failure");
    assert!(observation.summary.contains("exited with 7"));
}

#[cfg(unix)]
#[tokio::test]
async fn process_tool_handles_repeated_fast_processes_without_false_timeouts() {
    let root = TempDir::new().expect("tempdir");
    let tools = process_registry(&root, Duration::from_secs(10), 1024, &["printf"]);

    for index in 0..100 {
        let observation = tools
            .execute(
                "run_process",
                json!({"program": "printf", "args": [index.to_string()]}),
            )
            .await
            .expect("printf should execute");
        assert_eq!(observation.status, ObservationStatus::Success);
        assert_eq!(observation.content, index.to_string());
        assert!(!observation.summary.contains("timed out"));
    }
}

#[cfg(unix)]
#[tokio::test]
#[ignore = "10,000-process reliability soak; run explicitly before releases"]
async fn process_tool_fast_process_release_soak() {
    let root = TempDir::new().expect("tempdir");
    let tools = process_registry(&root, Duration::from_secs(1), 16, &["printf"]);

    for _ in 0..10_000 {
        let observation = tools
            .execute("run_process", json!({"program": "printf", "args": ["ok"]}))
            .await
            .expect("printf should execute");
        assert_eq!(observation.status, ObservationStatus::Success);
        assert_eq!(observation.content, "ok");
        assert!(!observation.summary.contains("timed out"));
    }
}

#[cfg(unix)]
#[tokio::test]
async fn process_tool_handles_concurrent_fast_processes() {
    let root = TempDir::new().expect("tempdir");
    let tools = Arc::new(process_registry(
        &root,
        Duration::from_secs(10),
        1024,
        &["printf"],
    ));
    let mut tasks = Vec::new();
    for index in 0..32 {
        let tools = Arc::clone(&tools);
        tasks.push(tokio::spawn(async move {
            tools
                .execute(
                    "run_process",
                    json!({"program": "printf", "args": [index.to_string()]}),
                )
                .await
                .expect("printf should execute")
        }));
    }

    for task in tasks {
        let observation = task.await.expect("process task");
        assert_eq!(observation.status, ObservationStatus::Success);
        assert!(!observation.summary.contains("timed out"));
    }
}

#[cfg(unix)]
#[tokio::test]
async fn process_tool_distinguishes_inherited_pipe_drain_from_execution_timeout() {
    let root = TempDir::new().expect("tempdir");
    let tools = process_registry(&root, Duration::from_secs(10), 1024, &["sh"]);
    let started = std::time::Instant::now();

    let observation = tools
        .execute(
            "run_process",
            json!({
                "program": "sh",
                "args": ["-c", "(sleep 1; printf leaked > descendant.txt) & printf ready"]
            }),
        )
        .await
        .expect("shell should execute");

    assert_eq!(observation.status, ObservationStatus::Warning);
    assert_eq!(observation.content, "ready");
    assert!(observation.summary.contains("output pipes remained open"));
    assert!(!observation.summary.contains("process execution timed out"));
    assert!(started.elapsed() < Duration::from_secs(5));
    tokio::time::sleep(Duration::from_millis(750)).await;
    assert!(
        !root.path().join("descendant.txt").exists(),
        "the inherited-descriptor process should have been terminated with its group"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn process_tool_cancellation_stops_the_process_group() {
    let root = TempDir::new().expect("tempdir");
    let tools = Arc::new(process_registry(
        &root,
        Duration::from_secs(30),
        1024,
        &["sh"],
    ));
    let cancellation = CancellationToken::new();
    let running = {
        let tools = Arc::clone(&tools);
        let cancellation = cancellation.clone();
        tokio::spawn(async move {
            tools
                .execute_cancellable(
                    "run_process",
                    json!({
                        "program": "sh",
                        "args": ["-c", "(sleep 1; printf leaked > cancelled.txt) & wait"]
                    }),
                    &cancellation,
                )
                .await
                .expect("cancellation should be an observation")
        })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    cancellation.cancel();
    let observation = tokio::time::timeout(Duration::from_secs(2), running)
        .await
        .expect("cancelled process should return promptly")
        .expect("process task");

    assert_eq!(observation.status, ObservationStatus::Error);
    assert!(observation.summary.contains("cancelled"));
    assert!(!observation.summary.contains("timed out"));
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    assert!(
        !root.path().join("cancelled.txt").exists(),
        "cancellation should terminate descendants in the process group"
    );
}

#[tokio::test]
async fn malformed_tool_input_never_reaches_an_adapter() {
    let root = TempDir::new().expect("tempdir");
    let tools = registry(&root);

    let error = tools
        .execute("read_file", json!({"path": "missing", "surprise": true}))
        .await
        .expect_err("unknown fields should fail closed");

    assert!(error.to_string().contains("invalid arguments"));
}

#[tokio::test]
async fn process_allowlist_rejects_shell_interpreters() {
    let root = TempDir::new().expect("tempdir");
    let tools = registry(&root);

    let error = tools
        .execute(
            "run_process",
            json!({"program": "/bin/sh", "args": ["-c", "cat /etc/passwd"]}),
        )
        .await
        .expect_err("shell must not bypass the program allowlist");

    assert!(error.to_string().contains("disabled by policy"));
}

#[tokio::test]
async fn process_allowlist_rejects_matching_basenames_and_missing_lists() {
    let root = TempDir::new().expect("tempdir");
    let tools = registry(&root);
    for program in ["/tmp/sleep", "./printf"] {
        tools
            .execute("run_process", json!({"program": program, "args": []}))
            .await
            .expect_err("the complete program value must match the allowlist");
    }
    let unrestricted = ToolRegistry::with_default_tools(
        root.path(),
        ToolPolicy {
            allow_process: true,
            allowed_programs: None,
            ..ToolPolicy::default()
        },
    )
    .expect("registry");
    assert!(
        unrestricted
            .definitions()
            .iter()
            .all(|definition| definition.name != "run_process"),
        "disabled process execution must not be advertised to the model"
    );
}

#[test]
fn default_registry_does_not_advertise_disabled_process_execution() {
    let root = TempDir::new().expect("tempdir");
    let tools =
        ToolRegistry::with_default_tools(root.path(), ToolPolicy::default()).expect("registry");

    assert!(
        tools
            .definitions()
            .iter()
            .all(|definition| definition.name != "run_process")
    );
}

#[tokio::test]
async fn bash_runner_is_opt_in_and_keeps_bounded_output_with_a_full_log() {
    let root = TempDir::new().expect("tempdir");
    let disabled = BashRunner::new(root.path(), ToolPolicy::default()).expect("runner");
    disabled
        .execute("printf disabled")
        .await
        .expect_err("bash must be disabled by default");

    let runner = BashRunner::new(
        root.path(),
        ToolPolicy {
            allow_process: true,
            max_output_bytes: 8,
            command_timeout: Duration::from_secs(2),
            ..ToolPolicy::default()
        },
    )
    .expect("runner");
    let result = runner
        .execute("printf '123456789abcdef'")
        .await
        .expect("bash command");

    assert_eq!(result.exit_code, Some(0));
    assert!(!result.cancelled);
    assert!(result.truncated);
    assert!(result.output.len() <= 8);
    let full_log = result.full_output_path.expect("full output log");
    assert_eq!(
        std::fs::read_to_string(full_log).expect("full output"),
        "123456789abcdef"
    );
}

#[tokio::test]
async fn bash_runner_requires_an_explicit_allowlist_and_rejects_shell_composition() {
    let root = TempDir::new().expect("tempdir");
    let no_allowlist = BashRunner::new(
        root.path(),
        ToolPolicy {
            allow_process: true,
            allowed_programs: Some(Vec::new()),
            ..ToolPolicy::default()
        },
    )
    .expect("runner");
    no_allowlist
        .execute("printf denied")
        .await
        .expect_err("empty allowlist must fail closed");

    let runner = BashRunner::new(
        root.path(),
        ToolPolicy {
            allow_process: true,
            allowed_programs: Some(vec!["printf".into()]),
            ..ToolPolicy::default()
        },
    )
    .expect("runner");
    assert_eq!(
        runner
            .execute("printf allowed")
            .await
            .expect("allowed")
            .output,
        "allowed"
    );
    for command in ["sleep 1", "printf ok; sleep 1", "printf $(pwd)"] {
        runner
            .execute(command)
            .await
            .expect_err("unlisted or compound command must fail closed");
    }
}

#[tokio::test]
async fn bash_runner_timeout_and_abort_terminate_the_process_group() {
    let root = TempDir::new().expect("tempdir");
    let timeout = BashRunner::new(
        root.path(),
        ToolPolicy {
            allow_process: true,
            command_timeout: Duration::from_millis(50),
            allowed_programs: Some(vec!["sleep".into()]),
            ..ToolPolicy::default()
        },
    )
    .expect("runner");
    let timed_out = timeout.execute("sleep 30").await.expect("timeout result");
    assert!(timed_out.timed_out);
    assert!(!timed_out.cancelled);
    assert_eq!(timed_out.exit_code, None);

    let runner = Arc::new(
        BashRunner::new(
            root.path(),
            ToolPolicy {
                allow_process: true,
                command_timeout: Duration::from_secs(30),
                allowed_programs: Some(vec!["sleep".into()]),
                ..ToolPolicy::default()
            },
        )
        .expect("runner"),
    );
    let running = {
        let runner = runner.clone();
        tokio::spawn(async move { runner.execute("sleep 30").await })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    runner.abort();
    let cancelled = running.await.unwrap().expect("cancelled result");
    assert!(cancelled.cancelled);
    assert!(!cancelled.timed_out);
    assert_eq!(cancelled.exit_code, None);
}

#[cfg(unix)]
#[tokio::test]
async fn writes_reject_symlinks_that_escape_the_workspace() {
    let root = TempDir::new().expect("tempdir");
    let outside = TempDir::new().expect("outside tempdir");
    let outside_file = outside.path().join("outside.txt");
    std::fs::write(&outside_file, "unchanged").expect("outside fixture");
    std::os::unix::fs::symlink(&outside_file, root.path().join("link.txt")).expect("symlink");
    let tools = registry(&root);

    let error = tools
        .execute(
            "write_file",
            json!({"path": "link.txt", "content": "changed"}),
        )
        .await
        .expect_err("escaping symlink must be denied");

    assert!(error.to_string().contains("workspace"));
    assert_eq!(
        std::fs::read_to_string(outside_file).expect("outside remains"),
        "unchanged"
    );
}
