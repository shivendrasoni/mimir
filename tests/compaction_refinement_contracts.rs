use std::{
    io::{BufRead, BufReader, Write},
    process::{Command as StdCommand, Stdio},
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};

use assert_cmd::Command;
use mimir::{
    learning,
    model::{Content, Message, ModelResponse, StopReason},
    provider::FakeProvider,
    refinement::{self, RefineOptions},
    runtime::{AgentRuntime, RuntimeConfig, VecEventSink},
    session::InMemorySessionStore,
    tools::{ToolPolicy, ToolRegistry},
};
use serde_json::{Value, json};
use tempfile::TempDir;

fn common_args<'a>(workspace: &'a TempDir, state: &'a TempDir) -> [&'a str; 8] {
    common_args_for_session(workspace, state, "main")
}

fn common_args_for_session<'a>(
    workspace: &'a TempDir,
    state: &'a TempDir,
    session: &'a str,
) -> [&'a str; 8] {
    [
        "--provider",
        "fake",
        "--workspace",
        workspace.path().to_str().expect("workspace path"),
        "--state-dir",
        state.path().to_str().expect("state path"),
        "--session",
        session,
    ]
}

fn seed_turn(workspace: &TempDir, state: &TempDir, prompt: &str, answer: &str) {
    Command::cargo_bin("mimir")
        .expect("binary")
        .args(common_args(workspace, state))
        .args(["--fake-response", answer, "--print", prompt])
        .assert()
        .success();
}

fn project_sessions(state: &TempDir, workspace: &TempDir) -> std::path::PathBuf {
    learning::project_session_root(state.path(), workspace.path())
        .expect("project session root")
        .join("sessions")
}

fn session_harness(workspace: &TempDir, session: &str) -> std::path::PathBuf {
    std::fs::canonicalize(workspace.path())
        .expect("canonical workspace")
        .join(".mimir/learning/harness/sessions")
        .join(session)
        .join("harness_state.json")
}

fn migration_refinement_proposal() -> String {
    json!({
        "summary": "Remember the Rust migration",
        "rationale": "The active task needs durable project context",
        "expectedOutcome": "Future turns retain the migration constraint",
        "edits": [{
            "action": "create",
            "kind": "memory",
            "id": "rust_migration",
            "title": "Rust migration",
            "content": "Migrate mimir completely to Rust.",
            "path": "projects/mimir",
            "metadata": {"project": "mimir"},
            "reason": "Preserve the active objective"
        }]
    })
    .to_string()
}

fn response_by_id(output: &[u8], id: &str) -> Value {
    String::from_utf8(output.to_vec())
        .expect("UTF-8 RPC output")
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|value| value["type"] == "response" && value["id"] == id)
        .expect("matching RPC response")
}

#[test]
fn manual_compact_returns_reference_shape_and_durably_rewrites_context() {
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    seed_turn(&workspace, &state, "old decision", "old answer");
    let recent_request = "R".repeat(100_000);
    seed_turn(&workspace, &state, &recent_request, "recent answer");

    let input = format!(
        "{}\n",
        json!({"id":"compact","type":"compact","customInstructions":"Keep decisions"})
    );
    let output = Command::cargo_bin("mimir")
        .expect("binary")
        .args(common_args(&workspace, &state))
        .args(["--fake-response", "manual summary", "--output", "rpc"])
        .write_stdin(input)
        .output()
        .expect("RPC output");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let compact = response_by_id(&output.stdout, "compact");
    assert_eq!(compact["success"], true);
    assert_eq!(compact["data"]["summary"], "manual summary");
    assert!(
        compact["data"]["firstKeptEntryId"]
            .as_str()
            .is_some_and(|id| !id.is_empty())
    );
    assert!(compact["data"]["tokensBefore"].as_u64().unwrap() > 0);
    assert_eq!(compact["data"]["details"]["readFiles"], json!([]));
    assert_eq!(compact["data"]["details"]["modifiedFiles"], json!([]));

    let compact_again_output = Command::cargo_bin("mimir")
        .expect("binary")
        .args(common_args(&workspace, &state))
        .args(["--fake-response", "unused", "--output", "rpc"])
        .write_stdin("{\"id\":\"compact-again\",\"type\":\"compact\"}\n")
        .output()
        .expect("second compact output");
    assert!(compact_again_output.status.success());
    let compact_again = response_by_id(&compact_again_output.stdout, "compact-again");
    assert_eq!(compact_again["success"], false);
    assert!(
        compact_again["error"]
            .as_str()
            .expect("second compact error")
            .contains("Already compacted")
    );
    let second_end = String::from_utf8(compact_again_output.stdout)
        .expect("UTF-8 output")
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|value| value["type"] == "compaction_end")
        .expect("second compaction end event");
    assert_eq!(second_end["errorSeverity"], "warning");

    let messages_output = Command::cargo_bin("mimir")
        .expect("binary")
        .args(common_args(&workspace, &state))
        .args(["--fake-response", "unused", "--output", "rpc"])
        .write_stdin("{\"id\":\"messages\",\"type\":\"get_messages\"}\n")
        .output()
        .expect("messages output");
    assert!(messages_output.status.success());
    let messages = response_by_id(&messages_output.stdout, "messages");
    let messages = messages["data"]["messages"]
        .as_array()
        .expect("message array");
    assert_eq!(messages[0]["role"], "system");
    assert_eq!(messages[0]["content"][0]["text"], "manual summary");
    assert!(messages.iter().any(|message| {
        message["content"][0]["text"]
            .as_str()
            .is_some_and(|text| text.len() == recent_request.len() && text.starts_with("RRRR"))
    }));
    assert!(
        !messages
            .iter()
            .any(|message| { message["content"][0]["text"] == "old decision" })
    );

    let session = std::fs::read_to_string(project_sessions(&state, &workspace).join("main.jsonl"))
        .expect("session JSONL");
    let first: Value = serde_json::from_str(session.lines().next().expect("compaction line"))
        .expect("compaction JSON");
    assert_eq!(first["payload"]["type"], "compaction");
    assert_eq!(first["payload"]["data"]["summary"], "manual summary");
    assert_eq!(
        first["payload"]["data"]["custom_instructions"],
        "Keep decisions"
    );
}

fn write_rpc(stdin: &mut impl Write, value: &Value) {
    writeln!(stdin, "{value}").expect("write RPC command");
    stdin.flush().expect("flush RPC command");
}

fn receive_rpc_until(
    lines: &mpsc::Receiver<String>,
    timeout: Duration,
    predicate: impl Fn(&Value) -> bool,
) -> Value {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let line = lines.recv_timeout(remaining).expect("matching RPC output");
        let value: Value = serde_json::from_str(&line).expect("RPC JSON line");
        if predicate(&value) {
            return value;
        }
    }
}

#[test]
fn legacy_rpc_abort_cancels_an_in_flight_manual_compaction() {
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    seed_turn(&workspace, &state, "old decision", "old answer");
    seed_turn(&workspace, &state, &"R".repeat(100_000), "recent answer");

    let mut child = StdCommand::new(assert_cmd::cargo::cargo_bin!("mimir"))
        .args(common_args(&workspace, &state))
        .args([
            "--fake-response",
            "summary that should be cancelled",
            "--fake-delay-ms",
            "5000",
            "--output",
            "rpc",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn RPC process");
    let mut stdin = child.stdin.take().expect("RPC stdin");
    let stdout = child.stdout.take().expect("RPC stdout");
    let (lines_tx, lines_rx) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            lines_tx.send(line.expect("RPC line")).expect("send line");
        }
    });

    write_rpc(&mut stdin, &json!({"id":"compact","type":"compact"}));
    receive_rpc_until(&lines_rx, Duration::from_secs(2), |value| {
        value["type"] == "compaction_start"
    });
    let abort_started = Instant::now();
    write_rpc(&mut stdin, &json!({"id":"abort","type":"abort"}));
    let abort = receive_rpc_until(&lines_rx, Duration::from_secs(2), |value| {
        value["id"] == "abort"
    });
    assert_eq!(abort["success"], true);
    let compact = receive_rpc_until(&lines_rx, Duration::from_secs(2), |value| {
        value["id"] == "compact"
    });
    assert_eq!(compact["success"], false);
    assert!(abort_started.elapsed() < Duration::from_secs(2));

    drop(stdin);
    let status = child.wait().expect("wait for RPC process");
    reader.join().expect("reader thread");
    assert!(status.success(), "RPC process exited with {status}");
}

#[test]
fn refine_rollback_uses_the_recorded_path_after_the_session_is_cloned() {
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    seed_turn(&workspace, &state, "Migrate the harness", "Working on it");
    let proposal = migration_refinement_proposal();

    let mut child = StdCommand::new(assert_cmd::cargo::cargo_bin!("mimir"))
        .args(common_args(&workspace, &state))
        .args(["--fake-response", &proposal, "--output", "rpc"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn RPC process");
    let mut stdin = child.stdin.take().expect("RPC stdin");
    let stdout = child.stdout.take().expect("RPC stdout");
    let (lines_tx, lines_rx) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            lines_tx.send(line.expect("RPC line")).expect("send line");
        }
    });

    write_rpc(
        &mut stdin,
        &json!({"id":"refine","type":"refine","instructions":"Persist project context","global":false}),
    );
    let refined = receive_rpc_until(&lines_rx, Duration::from_secs(2), |value| {
        value["id"] == "refine"
    });
    assert_eq!(refined["success"], true);
    assert_eq!(refined["data"]["scope"], "session");
    assert_eq!(refined["data"]["appliedEdits"][0]["applied"], true);
    let refinement_id = refined["data"]["id"]
        .as_str()
        .expect("refinement id")
        .to_owned();
    let harness_path = refined["data"]["harnessStatePath"]
        .as_str()
        .expect("harness path");
    assert_eq!(
        std::path::Path::new(harness_path),
        session_harness(&workspace, "main")
    );
    let harness: Value =
        serde_json::from_slice(&std::fs::read(harness_path).expect("harness state"))
            .expect("harness JSON");
    assert_eq!(
        harness["entries"]["memory"]["rust_migration"]["scope"],
        "session"
    );
    assert_eq!(harness["entries"]["memory"]["rust_migration"]["version"], 1);

    drop(stdin);
    let status = child.wait().expect("wait for refine RPC process");
    reader.join().expect("reader thread");
    assert!(status.success(), "RPC process exited with {status}");

    std::fs::copy(
        project_sessions(&state, &workspace).join("main.jsonl"),
        project_sessions(&state, &workspace).join("clone.jsonl"),
    )
    .expect("clone persisted session");
    let rollback_output = Command::cargo_bin("mimir")
        .expect("binary")
        .args(common_args_for_session(&workspace, &state, "clone"))
        .args(["--fake-response", "unused", "--output", "rpc"])
        .write_stdin(format!(
            "{}\n",
            json!({"id":"rollback","type":"refine","rollbackId":refinement_id})
        ))
        .output()
        .expect("rollback RPC output");
    assert!(
        rollback_output.status.success(),
        "{}",
        String::from_utf8_lossy(&rollback_output.stderr)
    );
    let rollback = response_by_id(&rollback_output.stdout, "rollback");
    assert_eq!(rollback["success"], true, "rollback response: {rollback}");
    assert_eq!(rollback["data"]["rollbackOf"], refinement_id);
    assert_eq!(rollback["data"]["appliedEdits"][0]["action"], "delete");
    assert_eq!(rollback["data"]["appliedEdits"][0]["applied"], true);
    let harness: Value =
        serde_json::from_slice(&std::fs::read(harness_path).expect("rolled back harness"))
            .expect("harness JSON");
    assert!(harness["entries"]["memory"]["rust_migration"].is_null());
    assert!(
        !state
            .path()
            .join(".mimir/learning/harness/sessions/clone/harness_state.json")
            .exists()
    );

    let history = std::fs::read_to_string(
        workspace
            .path()
            .join(".mimir/learning/harness/sessions/main/refinements.jsonl"),
    )
    .expect("durable refinement history");
    assert_eq!(history.lines().count(), 2);
    assert!(history.contains(&refinement_id));
}

#[test]
fn refine_rejects_malformed_provider_json_without_writing_harness_state() {
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    seed_turn(&workspace, &state, "Remember this", "Noted");
    let output = Command::cargo_bin("mimir")
        .expect("binary")
        .args(common_args(&workspace, &state))
        .args(["--fake-response", "{\"summary\":", "--output", "rpc"])
        .write_stdin("{\"id\":\"bad\",\"type\":\"refine\"}\n")
        .output()
        .expect("refine RPC output");
    assert!(
        output.status.success(),
        "status={} stdout={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let response = response_by_id(&output.stdout, "bad");
    assert_eq!(response["success"], false);
    assert!(
        response["error"]
            .as_str()
            .expect("refine error")
            .contains("JSON")
    );
    assert!(
        !state
            .path()
            .join("harness/sessions/main/harness_state.json")
            .exists()
    );
}

#[cfg(unix)]
#[test]
fn refine_rejects_symlinked_harness_directories_before_provider_execution() {
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    let outside = TempDir::new().expect("outside");
    std::os::unix::fs::symlink(outside.path(), state.path().join("harness"))
        .expect("harness symlink");
    let output = Command::cargo_bin("mimir")
        .expect("binary")
        .args(common_args(&workspace, &state))
        .args(["--fake-response", "unused", "--output", "rpc"])
        .write_stdin("{\"id\":\"unsafe\",\"type\":\"refine\"}\n")
        .output()
        .expect("refine RPC output");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("symlink"));
    assert!(output.stdout.is_empty());
    assert!(std::fs::read_dir(outside.path()).unwrap().next().is_none());
}

fn model_response(text: impl Into<String>) -> ModelResponse {
    ModelResponse {
        message: Message::assistant(vec![Content::Text { text: text.into() }], StopReason::Stop),
        response_id: Some("fake-response".into()),
    }
}

#[tokio::test]
async fn refined_harness_context_reaches_the_next_provider_turn() {
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    let proposal = json!({
        "summary": "Remember the Rust migration",
        "rationale": "The objective is durable",
        "expectedOutcome": "The next turn retains the constraint",
        "edits": [{
            "action": "create",
            "kind": "memory",
            "id": "rust_migration",
            "title": "Rust migration",
            "content": "Migrate mimir completely to Rust."
        }]
    })
    .to_string();
    let provider = Arc::new(FakeProvider::new(vec![
        model_response(proposal),
        model_response("continued"),
    ]));
    let tools = Arc::new(
        ToolRegistry::with_default_tools(workspace.path(), ToolPolicy::default()).expect("tools"),
    );
    let runtime = AgentRuntime::resume(
        provider.clone(),
        tools,
        Arc::new(InMemorySessionStore::default()),
        RuntimeConfig::default_for_model("fake-model"),
    )
    .await
    .expect("runtime");
    refinement::refine(&runtime, state.path(), "main", RefineOptions::default())
        .await
        .expect("refine");
    runtime
        .set_harness_context(
            refinement::load_harness_context(state.path(), "main")
                .await
                .expect("harness context"),
        )
        .await;

    runtime
        .run("continue", &VecEventSink::default())
        .await
        .expect("next turn");
    let requests = provider.requests().await;
    assert_eq!(requests.len(), 2);
    assert!(requests[1].system_prompt.contains("Continual Harness"));
    assert!(requests[1].system_prompt.contains("Rust migration"));
    assert!(
        requests[1]
            .system_prompt
            .contains("Migrate mimir completely to Rust.")
    );
}

#[tokio::test]
async fn refinement_planner_receives_prior_refinement_history() {
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    let first_proposal = json!({
        "summary": "Remember the Rust migration",
        "rationale": "The objective is durable",
        "expectedOutcome": "The next refinement sees this decision",
        "edits": [{
            "action": "create",
            "kind": "memory",
            "id": "rust_migration",
            "title": "Rust migration",
            "content": "Migrate mimir completely to Rust."
        }]
    })
    .to_string();
    let second_proposal = json!({
        "summary": "No further changes",
        "rationale": "The prior refinement remains valid",
        "expectedOutcome": "The harness stays stable",
        "edits": []
    })
    .to_string();
    let provider = Arc::new(FakeProvider::new(vec![
        model_response(first_proposal),
        model_response(second_proposal),
    ]));
    let tools = Arc::new(
        ToolRegistry::with_default_tools(workspace.path(), ToolPolicy::default()).expect("tools"),
    );
    let runtime = AgentRuntime::resume(
        provider.clone(),
        tools,
        Arc::new(InMemorySessionStore::default()),
        RuntimeConfig::default_for_model("fake-model"),
    )
    .await
    .expect("runtime");
    let first = refinement::refine(&runtime, state.path(), "main", RefineOptions::default())
        .await
        .expect("first refinement");
    refinement::refine(&runtime, state.path(), "main", RefineOptions::default())
        .await
        .expect("second refinement");

    let requests = provider.requests().await;
    let second_prompt = requests[1].messages[0].text();
    assert!(second_prompt.contains("<refinement_history>"));
    assert!(second_prompt.contains(&first.id));
    assert!(second_prompt.contains("Remember the Rust migration"));
}

#[tokio::test]
async fn abort_cancels_refinement_without_writing_harness_state() {
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    let provider = Arc::new(
        FakeProvider::new(vec![model_response("unused")]).with_delay(Duration::from_secs(5)),
    );
    let tools = Arc::new(
        ToolRegistry::with_default_tools(workspace.path(), ToolPolicy::default()).expect("tools"),
    );
    let runtime = Arc::new(
        AgentRuntime::resume(
            provider.clone(),
            tools,
            Arc::new(InMemorySessionStore::default()),
            RuntimeConfig::default_for_model("fake-model"),
        )
        .await
        .expect("runtime"),
    );
    let task_runtime = runtime.clone();
    let state_path = state.path().to_owned();
    let task = tokio::spawn(async move {
        refinement::refine(
            task_runtime.as_ref(),
            &state_path,
            "main",
            RefineOptions::default(),
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        while provider.requests().await.is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("provider request started");
    runtime.cancel();
    let error = tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .expect("refinement cancellation timeout")
        .expect("refinement task")
        .expect_err("refinement should be cancelled");
    let message = error.to_string().to_ascii_lowercase();
    assert!(
        message.contains("abort") || message.contains("cancel"),
        "unexpected cancellation error: {error}"
    );
    assert!(
        !state
            .path()
            .join("harness/sessions/main/harness_state.json")
            .exists()
    );
}
