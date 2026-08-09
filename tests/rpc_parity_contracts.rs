use std::{io::Write as _, process::Command as StdCommand};

use assert_cmd::cargo::cargo_bin;
use serde_json::{Value, json};
use tempfile::TempDir;

fn rpc_lines(output: &[u8]) -> Vec<Value> {
    String::from_utf8(output.to_vec())
        .expect("UTF-8 RPC output")
        .lines()
        .map(|line| serde_json::from_str(line).expect("RPC JSON line"))
        .collect()
}

#[test]
fn rpc_prompt_accepts_images_and_emits_complete_turn_lifecycle() {
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    let child = StdCommand::new(cargo_bin("mimir"))
        .args([
            "--provider",
            "fake",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "--state-dir",
            state.path().to_str().unwrap(),
            "--session",
            "image-rpc",
            "--fake-response",
            "image accepted",
            "--output",
            "rpc",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("spawn RPC process");
    let mut child = child;
    writeln!(
        child.stdin.as_mut().unwrap(),
        "{}",
        json!({
            "id": "image-prompt",
            "type": "prompt",
            "message": "inspect",
            "images": [{"type": "image", "data": "aGVsbG8=", "mimeType": "image/png"}]
        })
    )
    .unwrap();
    drop(child.stdin.take());
    let output = child.wait_with_output().expect("RPC output");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let lines = rpc_lines(&output.stdout);

    for event_type in [
        "agent_start",
        "turn_start",
        "message_start",
        "message_update",
        "message_end",
        "turn_end",
        "agent_end",
    ] {
        assert!(
            lines.iter().any(|line| line["type"] == event_type),
            "missing {event_type}: {lines:?}"
        );
    }
    let end = lines
        .iter()
        .find(|line| line["type"] == "agent_end")
        .expect("agent_end");
    assert_eq!(end["messages"][0]["content"][0]["text"], "inspect");
    assert_eq!(end["messages"][0]["content"][1]["type"], "image");
    assert_eq!(end["messages"][0]["content"][1]["mimeType"], "image/png");
}

#[test]
fn rpc_rejects_malformed_or_oversized_image_inputs_before_provider_execution() {
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    let output = StdCommand::new(cargo_bin("mimir"))
        .args([
            "--provider",
            "fake",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "--state-dir",
            state.path().to_str().unwrap(),
            "--fake-response",
            "must remain unused",
            "--output",
            "rpc",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            writeln!(
                child.stdin.as_mut().unwrap(),
                "{}",
                json!({
                    "id": "bad-image",
                    "type": "prompt",
                    "images": [{"type": "image", "data": "not base64", "mimeType": "image/png"}]
                })
            )?;
            drop(child.stdin.take());
            child.wait_with_output()
        })
        .expect("RPC output");
    assert!(output.status.success());
    let lines = rpc_lines(&output.stdout);
    let error = lines
        .iter()
        .find(|line| line["id"] == "bad-image")
        .expect("error response");
    assert_eq!(error["success"], false);
    assert!(error["error"].as_str().unwrap().contains("valid base64"));
    assert!(!lines.iter().any(|line| line["type"] == "agent_start"));
}

#[test]
fn session_action_updates_use_the_reference_actions_snapshot_shape() {
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    let mut child = StdCommand::new(cargo_bin("mimir"))
        .args([
            "--provider",
            "fake",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "--state-dir",
            state.path().to_str().unwrap(),
            "--fake-delay-ms",
            "300",
            "--fake-response",
            "first",
            "--fake-response",
            "second",
            "--output",
            "rpc",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("spawn RPC process");
    {
        let stdin = child.stdin.as_mut().unwrap();
        writeln!(
            stdin,
            "{}",
            json!({"id": "initial", "type": "prompt", "message": "start"})
        )
        .unwrap();
        writeln!(
            stdin,
            "{}",
            json!({"id": "queued", "type": "follow_up", "message": "after that"})
        )
        .unwrap();
    }
    drop(child.stdin.take());
    let output = child.wait_with_output().expect("RPC output");
    assert!(output.status.success());
    let lines = rpc_lines(&output.stdout);
    let update = lines
        .iter()
        .find(|line| {
            line["type"] == "session_action_update"
                && line["actions"]["followUps"] == json!(["after that"])
        })
        .expect("queued action update");
    assert_eq!(update["actions"]["queuedCount"], 1);
    assert_eq!(update["actions"]["steering"], json!([]));
    assert!(update.get("sessionActions").is_none());
}
