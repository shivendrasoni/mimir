use std::{
    io::{BufRead, BufReader, Write},
    process::{Command as StdCommand, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::json;
use tempfile::TempDir;

#[test]
fn help_exposes_the_operational_surface() {
    Command::cargo_bin("mimir")
        .expect("binary")
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("--provider"))
        .stdout(predicate::str::contains("doctor"))
        .stdout(predicate::str::contains("login"))
        .stdout(predicate::str::contains("logout"))
        .stdout(predicate::str::contains("providers"));
}

#[test]
fn providers_output_uses_stable_snake_case_auth_names() {
    Command::cargo_bin("mimir")
        .expect("binary")
        .args(["providers"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"oauth_pkce\""))
        .stdout(predicate::str::contains("\"oauth_device\""))
        .stdout(predicate::str::contains("\"api_key\""))
        .stdout(predicate::str::contains(
            "\"runtime_support\": \"unsupported\"",
        ))
        .stdout(predicate::str::contains(
            "\"runtime_support\": \"openai_compatible\"",
        ))
        .stdout(predicate::str::contains(
            "\"runtime_support\": \"anthropic_messages\"",
        ))
        .stdout(predicate::str::contains("\"id\": \"anthropic\""));
}

#[test]
fn provider_defaults_and_base_url_environment_are_provider_scoped() {
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    let common = [
        "--workspace",
        workspace.path().to_str().expect("workspace path"),
        "--state-dir",
        state.path().to_str().expect("state path"),
        "--provider",
        "anthropic",
    ];
    Command::cargo_bin("mimir")
        .expect("binary")
        .env("OPENAI_BASE_URL", "https://wrong-for-anthropic.invalid")
        .env_remove("ANTHROPIC_BASE_URL")
        .args(common)
        .arg("doctor")
        .assert()
        .success()
        .stdout(predicate::str::contains("model: claude-sonnet-5"))
        .stdout(predicate::str::contains(
            "base_url_source: provider_default",
        ));
    Command::cargo_bin("mimir")
        .expect("binary")
        .env("MIMIR_MODEL", "claude-explicit-env")
        .env("ANTHROPIC_BASE_URL", "https://anthropic.example.test")
        .args(common)
        .arg("doctor")
        .assert()
        .success()
        .stdout(predicate::str::contains("model: claude-explicit-env"))
        .stdout(predicate::str::contains(
            "base_url_source: anthropic_environment",
        ));
    Command::cargo_bin("mimir")
        .expect("binary")
        .env("MIMIR_MODEL", "ignored-env-model")
        .env("ANTHROPIC_BASE_URL", "https://ignored-env.invalid")
        .args(common)
        .args([
            "--model",
            "claude-command-line",
            "--base-url",
            "https://explicit.example.test",
            "doctor",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("model: claude-command-line"))
        .stdout(predicate::str::contains("base_url_source: command_line"));
}

#[test]
fn google_base_url_uses_gemini_environment_with_explicit_precedence() {
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    let common = [
        "--workspace",
        workspace.path().to_str().expect("workspace path"),
        "--state-dir",
        state.path().to_str().expect("state path"),
        "--provider",
        "google",
    ];
    Command::cargo_bin("mimir")
        .expect("binary")
        .env("OPENAI_BASE_URL", "https://wrong.invalid")
        .env("GEMINI_BASE_URL", "https://gemini.example.test")
        .args(common)
        .arg("doctor")
        .assert()
        .success()
        .stdout(predicate::str::contains("model: gemini-2.5-pro"))
        .stdout(predicate::str::contains(
            "base_url_source: google_environment",
        ));
    Command::cargo_bin("mimir")
        .expect("binary")
        .env("GEMINI_BASE_URL", "https://ignored.invalid")
        .args(common)
        .args([
            "--base-url",
            "https://explicit-gemini.example.test",
            "doctor",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("base_url_source: command_line"));
}

#[test]
fn anthropic_api_key_login_is_exposed_with_the_native_runtime() {
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    let home = TempDir::new().expect("home");
    Command::cargo_bin("mimir")
        .expect("binary")
        .env("HOME", home.path())
        .args([
            "--workspace",
            workspace.path().to_str().expect("workspace path"),
            "--state-dir",
            state.path().to_str().expect("state path"),
            "login",
            "anthropic",
            "--api-key-stdin",
        ])
        .write_stdin("anthropic-test-secret\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("anthropic-test-secret").not())
        .stdout(predicate::str::contains("\"provider\": \"anthropic\""));
}

#[test]
fn api_key_login_status_and_logout_work_without_echoing_the_secret() {
    let workspace = TempDir::new().expect("workspace");
    let first_state = TempDir::new().expect("first state");
    let second_state = TempDir::new().expect("second state");
    let home = TempDir::new().expect("home");
    let first = [
        "--workspace",
        workspace.path().to_str().expect("workspace path"),
        "--state-dir",
        first_state.path().to_str().expect("state path"),
    ];
    let second = [
        "--workspace",
        workspace.path().to_str().expect("workspace path"),
        "--state-dir",
        second_state.path().to_str().expect("state path"),
    ];
    Command::cargo_bin("mimir")
        .expect("binary")
        .env("HOME", home.path())
        .args(first)
        .args(["login", "openai", "--api-key-stdin"])
        .write_stdin("cli-super-secret\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("cli-super-secret").not());
    Command::cargo_bin("mimir")
        .expect("binary")
        .env("HOME", home.path())
        .args(second)
        .args(["auth", "status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("openai"))
        .stdout(predicate::str::contains("cli-super-secret").not());
    Command::cargo_bin("mimir")
        .expect("binary")
        .env("HOME", home.path())
        .args(first)
        .args(["logout", "openai"])
        .assert()
        .success()
        .stdout(predicate::str::contains("logged_out"));
    assert!(!first_state.path().join("auth.json").exists());
    assert!(!second_state.path().join("auth.json").exists());
    assert!(home.path().join(".mimir/auth.json").exists());
}

#[test]
fn implicit_provider_uses_global_login_order_across_state_directories() {
    let workspace = TempDir::new().expect("workspace");
    let login_state = TempDir::new().expect("login state");
    let runtime_state = TempDir::new().expect("runtime state");
    let home = TempDir::new().expect("home");
    for (provider, key) in [("openai", "first-key"), ("anthropic", "second-key")] {
        Command::cargo_bin("mimir")
            .expect("binary")
            .env("HOME", home.path())
            .args([
                "--workspace",
                workspace.path().to_str().expect("workspace path"),
                "--state-dir",
                login_state.path().to_str().expect("login state path"),
                "login",
                provider,
                "--api-key-stdin",
            ])
            .write_stdin(format!("{key}\n"))
            .assert()
            .success();
    }

    Command::cargo_bin("mimir")
        .expect("binary")
        .env("HOME", home.path())
        .env_remove("OPENAI_API_KEY")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("ANTHROPIC_OAUTH_TOKEN")
        .args([
            "--workspace",
            workspace.path().to_str().expect("workspace path"),
            "--state-dir",
            runtime_state.path().to_str().expect("runtime state path"),
            "doctor",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("provider: openai"))
        .stdout(predicate::str::contains("model: gpt-5-mini"));

    assert!(!login_state.path().join("auth.json").exists());
    assert!(!runtime_state.path().join("auth.json").exists());
}

#[test]
fn migration_cli_plans_applies_and_rolls_back_without_printing_secrets() {
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    let legacy = TempDir::new().expect("legacy");
    std::fs::write(
        legacy.path().join("auth.json"),
        r#"{"providers":{"openai":{"type":"api_key","key":"migration-secret"}}}"#,
    )
    .expect("legacy auth");
    let common = [
        "--workspace",
        workspace.path().to_str().expect("workspace path"),
        "--state-dir",
        state.path().to_str().expect("state path"),
    ];

    Command::cargo_bin("mimir")
        .expect("binary")
        .args(common)
        .args([
            "migrate",
            "plan",
            "--legacy-root",
            legacy.path().to_str().expect("legacy path"),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("migration-secret").not())
        .stdout(predicate::str::contains("\"redacted\": true"));

    let apply = Command::cargo_bin("mimir")
        .expect("binary")
        .args(common)
        .args([
            "migrate",
            "apply",
            "--legacy-root",
            legacy.path().to_str().expect("legacy path"),
        ])
        .output()
        .expect("apply command");
    assert!(apply.status.success());
    let result: serde_json::Value =
        serde_json::from_slice(&apply.stdout).expect("apply response JSON");
    let journal = result["journal_path"]
        .as_str()
        .expect("journal path")
        .to_owned();
    assert!(state.path().join("auth.json").exists());

    Command::cargo_bin("mimir")
        .expect("binary")
        .args(common)
        .args(["migrate", "rollback", "--journal", &journal])
        .assert()
        .success();
    assert!(!state.path().join("auth.json").exists());
}

#[cfg(unix)]
#[test]
fn symlinked_state_root_is_rejected_before_management_commands_run() {
    let workspace = TempDir::new().expect("workspace");
    let real_state = TempDir::new().expect("real state");
    let parent = TempDir::new().expect("parent");
    let linked_state = parent.path().join("state-link");
    std::os::unix::fs::symlink(real_state.path(), &linked_state).expect("symlink");

    Command::cargo_bin("mimir")
        .expect("binary")
        .args([
            "--workspace",
            workspace.path().to_str().expect("workspace path"),
            "--state-dir",
            linked_state.to_str().expect("state link"),
            "doctor",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "state directory must not be a symlink",
        ));
}

#[test]
fn offline_print_mode_needs_no_api_key_and_persists_a_session() {
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    Command::cargo_bin("mimir")
        .expect("binary")
        .env_remove("OPENAI_API_KEY")
        .args([
            "--provider",
            "fake",
            "--workspace",
            workspace.path().to_str().expect("workspace path"),
            "--state-dir",
            state.path().to_str().expect("state path"),
            "--session",
            "cli-test",
            "--fake-response",
            "offline answer",
            "--print",
            "hello",
        ])
        .assert()
        .success()
        .stdout("offline answer\n");
    assert!(state.path().join("sessions/cli-test.jsonl").exists());
}

#[test]
fn json_mode_emits_versioned_machine_readable_events() {
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    Command::cargo_bin("mimir")
        .expect("binary")
        .args([
            "--provider",
            "fake",
            "--workspace",
            workspace.path().to_str().expect("workspace path"),
            "--state-dir",
            state.path().to_str().expect("state path"),
            "--fake-response",
            "json answer",
            "--output",
            "json",
            "--print",
            "hello",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"schema_version\":1"))
        .stdout(predicate::str::contains("\"type\":\"completed\""));
}

#[test]
fn doctor_never_requires_or_prints_the_api_key() {
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    let home = TempDir::new().expect("home");
    Command::cargo_bin("mimir")
        .expect("binary")
        .env("HOME", home.path())
        .env("OPENAI_API_KEY", "super-secret-test-value")
        .args([
            "--provider",
            "openai",
            "--workspace",
            workspace.path().to_str().expect("workspace path"),
            "--state-dir",
            state.path().to_str().expect("state path"),
            "doctor",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("provider: openai"))
        .stdout(predicate::str::contains(
            "runtime_support: openai_compatible",
        ))
        .stdout(predicate::str::contains("credential: present"))
        .stdout(predicate::str::contains("credential_source: environment"))
        .stdout(predicate::str::contains("super-secret-test-value").not());
}

#[test]
fn doctor_reports_stored_credentials_and_fake_mode_correctly() {
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    let home = TempDir::new().expect("home");
    let common = [
        "--workspace",
        workspace.path().to_str().expect("workspace path"),
        "--state-dir",
        state.path().to_str().expect("state path"),
    ];
    Command::cargo_bin("mimir")
        .expect("binary")
        .env("HOME", home.path())
        .args(common)
        .args(["login", "openai", "--api-key-stdin"])
        .write_stdin("stored-secret\n")
        .assert()
        .success();
    Command::cargo_bin("mimir")
        .expect("binary")
        .env("HOME", home.path())
        .env_remove("OPENAI_API_KEY")
        .args(common)
        .args(["--provider", "openai", "doctor"])
        .assert()
        .success()
        .stdout(predicate::str::contains("credential: present"))
        .stdout(predicate::str::contains(
            "credential_source: stored_api_key",
        ));
    Command::cargo_bin("mimir")
        .expect("binary")
        .env("HOME", home.path())
        .env_remove("OPENAI_API_KEY")
        .args(common)
        .args(["--provider", "fake", "doctor"])
        .assert()
        .success()
        .stdout(predicate::str::contains("runtime_support: offline_fake"))
        .stdout(predicate::str::contains("credential: not_required"));
}

#[test]
fn anthropic_provider_requires_an_api_key_before_runtime_execution() {
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    let home = TempDir::new().expect("home");
    Command::cargo_bin("mimir")
        .expect("binary")
        .env("HOME", home.path())
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("ANTHROPIC_OAUTH_TOKEN")
        .args([
            "--provider",
            "anthropic",
            "--workspace",
            workspace.path().to_str().expect("workspace path"),
            "--state-dir",
            state.path().to_str().expect("state path"),
            "--print",
            "hello",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("credential is missing"));
}

#[test]
fn rpc_mode_uses_json_rpc_envelopes_and_stable_errors() {
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    Command::cargo_bin("mimir")
        .expect("binary")
        .args([
            "--provider", "fake", "--workspace",
            workspace.path().to_str().expect("workspace path"), "--state-dir",
            state.path().to_str().expect("state path"), "--fake-response", "rpc answer",
            "--output", "rpc",
        ])
        .write_stdin(
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"health\"}\n{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"prompt\",\"params\":{\"prompt\":\"hello\"}}\n",
        )
        .assert()
        .success()
        .stdout(predicate::str::contains("\"status\":\"ready\""))
        .stdout(predicate::str::contains("\"text\":\"rpc answer\""));
}

#[test]
fn rpc_mode_accepts_the_stateful_legacy_jsonl_core() {
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    let output = Command::cargo_bin("mimir")
        .expect("binary")
        .args([
            "--provider",
            "fake",
            "--workspace",
            workspace.path().to_str().expect("workspace path"),
            "--state-dir",
            state.path().to_str().expect("state path"),
            "--session",
            "legacy-rpc",
            "--model",
            "fake/test-model",
            "--fake-response",
            "legacy answer",
            "--output",
            "rpc",
        ])
        .write_stdin(
            "{\"jsonrpc\":\"2.0\",\"id\":\"prompt-1\",\"method\":\"prompt\",\"params\":{\"prompt\":\"hello\"}}\n\
             {\"id\":\"state-1\",\"type\":\"get_state\"}\n\
             {\"id\":\"messages-1\",\"type\":\"get_messages\"}\n\
             {\"id\":\"last-1\",\"type\":\"get_last_assistant_text\"}\n\
             {\"id\":\"abort-1\",\"type\":\"abort\"}\n",
        )
        .output()
        .expect("legacy RPC command");
    assert!(
        output.status.success(),
        "legacy RPC process failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let responses: Vec<serde_json::Value> = String::from_utf8(output.stdout)
        .expect("UTF-8 stdout")
        .lines()
        .map(|line| serde_json::from_str(line).expect("JSON response line"))
        .collect();
    assert_eq!(responses.len(), 5);
    assert_eq!(responses[0]["id"], "prompt-1");
    assert_eq!(responses[0]["result"]["text"], "legacy answer");
    assert_eq!(responses[1]["data"]["sessionId"], "legacy-rpc");
    assert_eq!(responses[1]["data"]["model"]["id"], "fake/test-model");
    assert_eq!(responses[1]["data"]["messageCount"], 2);
    assert_eq!(
        responses[2]["data"]["messages"].as_array().unwrap().len(),
        2
    );
    assert_eq!(responses[3]["data"]["text"], "legacy answer");
    assert_eq!(responses[4]["command"], "abort");
    assert_eq!(responses[4]["success"], true);
}

#[test]
fn legacy_rpc_rejects_invalid_and_unknown_commands_without_panicking() {
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    Command::cargo_bin("mimir")
        .expect("binary")
        .args([
            "--provider",
            "fake",
            "--workspace",
            workspace.path().to_str().expect("workspace path"),
            "--state-dir",
            state.path().to_str().expect("state path"),
            "--fake-response",
            "unused",
            "--output",
            "rpc",
        ])
        .write_stdin(
            "{\"id\":\"blank\",\"type\":\"prompt\",\"message\":\"   \"}\n\
             {\"id\":\"unknown\",\"type\":\"not_a_command\"}\n\
             {\"id\":\"missing\",\"type\":\"switch_session\",\"sessionPath\":\"missing.jsonl\"}\n",
        )
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "{\"command\":\"prompt\",\"error\":\"message must be a non-empty string\",\"id\":\"blank\",\"success\":false,\"type\":\"response\"}",
        ))
        .stdout(predicate::str::contains(
            "{\"command\":\"not_a_command\",\"error\":\"unsupported command\",\"id\":\"unknown\",\"success\":false,\"type\":\"response\"}",
        ))
        .stdout(predicate::str::contains("session does not exist: missing"));
}

#[test]
fn legacy_rpc_manages_durable_session_lifecycle() {
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    let output = Command::cargo_bin("mimir")
        .expect("binary")
        .args([
            "--provider",
            "fake",
            "--workspace",
            workspace.path().to_str().expect("workspace path"),
            "--state-dir",
            state.path().to_str().expect("state path"),
            "--session",
            "original",
            "--fake-response",
            "session answer",
            "--output",
            "rpc",
        ])
        .write_stdin(
            "{\"jsonrpc\":\"2.0\",\"id\":\"p\",\"method\":\"prompt\",\"params\":{\"prompt\":\"hello\"}}\n\
             {\"id\":\"name\",\"type\":\"set_session_name\",\"name\":\"Important work\"}\n\
             {\"id\":\"state-original\",\"type\":\"get_state\"}\n\
             {\"id\":\"stats\",\"type\":\"get_session_stats\"}\n\
             {\"id\":\"forks\",\"type\":\"get_fork_messages\"}\n\
             {\"id\":\"clone\",\"type\":\"clone\"}\n\
             {\"id\":\"state-clone\",\"type\":\"get_state\"}\n\
             {\"id\":\"clone-messages\",\"type\":\"get_messages\"}\n\
             {\"id\":\"new\",\"type\":\"new_session\"}\n\
             {\"id\":\"state-new\",\"type\":\"get_state\"}\n\
             {\"id\":\"switch\",\"type\":\"switch_session\",\"sessionPath\":\"original.jsonl\"}\n\
             {\"id\":\"last\",\"type\":\"get_last_assistant_text\"}\n",
        )
        .output()
        .expect("legacy session RPC command");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let responses: Vec<serde_json::Value> = String::from_utf8(output.stdout)
        .expect("UTF-8 stdout")
        .lines()
        .map(|line| serde_json::from_str(line).expect("JSON response line"))
        .collect();
    assert_eq!(responses.len(), 12);
    assert_eq!(responses[1]["success"], true);
    assert_eq!(responses[2]["data"]["sessionName"], "Important work");
    assert_eq!(responses[3]["data"]["userMessages"], 1);
    assert_eq!(responses[3]["data"]["assistantMessages"], 1);
    assert_eq!(responses[4]["data"]["messages"][0]["text"], "hello");
    assert_eq!(responses[5]["success"], true);
    assert_ne!(responses[6]["data"]["sessionId"], "original");
    assert_eq!(
        responses[7]["data"]["messages"].as_array().unwrap().len(),
        2
    );
    assert_eq!(responses[8]["success"], true);
    assert_eq!(responses[9]["data"]["messageCount"], 0);
    assert_eq!(responses[10]["success"], true);
    assert_eq!(responses[11]["data"]["text"], "session answer");
}

#[test]
fn legacy_rpc_forks_before_the_selected_user_message() {
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    let common = [
        "--provider",
        "fake",
        "--workspace",
        workspace.path().to_str().expect("workspace path"),
        "--state-dir",
        state.path().to_str().expect("state path"),
        "--session",
        "fork-source",
        "--fake-response",
        "answer",
    ];
    Command::cargo_bin("mimir")
        .expect("binary")
        .args(common)
        .args(["--print", "fork this prompt"])
        .assert()
        .success();
    let transcript = std::fs::read_to_string(state.path().join("sessions/fork-source.jsonl"))
        .expect("source transcript");
    let first: serde_json::Value =
        serde_json::from_str(transcript.lines().next().expect("user record")).expect("record JSON");
    let entry_id = first["record_id"].as_str().expect("record id");

    let output = Command::cargo_bin("mimir")
        .expect("binary")
        .args(common)
        .args(["--output", "rpc"])
        .write_stdin(format!(
            "{{\"id\":\"fork\",\"type\":\"fork\",\"entryId\":\"{entry_id}\"}}\n\
             {{\"id\":\"state\",\"type\":\"get_state\"}}\n"
        ))
        .output()
        .expect("fork RPC command");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let responses: Vec<serde_json::Value> = String::from_utf8(output.stdout)
        .expect("UTF-8 stdout")
        .lines()
        .map(|line| serde_json::from_str(line).expect("JSON response line"))
        .collect();
    assert_eq!(responses[0]["data"]["text"], "fork this prompt");
    assert_eq!(responses[0]["data"]["cancelled"], false);
    assert_eq!(responses[1]["data"]["messageCount"], 0);
    assert_ne!(responses[1]["data"]["sessionId"], "fork-source");
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one linear subprocess scenario makes response and event ordering auditable"
)]
fn legacy_rpc_streams_events_and_accepts_control_while_running() {
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    let mut child = StdCommand::new(assert_cmd::cargo::cargo_bin("mimir"))
        .args([
            "--provider",
            "fake",
            "--workspace",
            workspace.path().to_str().expect("workspace path"),
            "--state-dir",
            state.path().to_str().expect("state path"),
            "--session",
            "concurrent-rpc",
            "--fake-delay-ms",
            "1000",
            "--fake-response",
            "first answer",
            "--fake-response",
            "steered answer",
            "--fake-response",
            "follow-up answer",
            "--fake-response",
            "must be cancelled",
            "--fake-response",
            "EOF answer",
            "--output",
            "rpc",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn RPC process");
    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let (lines_tx, lines_rx) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            if lines_tx.send(line.expect("stdout line")).is_err() {
                break;
            }
        }
    });

    write_rpc(
        &mut stdin,
        &json!({"jsonrpc":"2.0","id":"ready","method":"health"}),
    );
    receive_rpc_until(&lines_rx, Duration::from_secs(3), |value| {
        value["id"] == "ready"
    });

    write_rpc(
        &mut stdin,
        &json!({"id":"p1","type":"prompt","message":"initial"}),
    );
    let accepted: serde_json::Value = serde_json::from_str(
        &lines_rx
            .recv_timeout(Duration::from_millis(200))
            .expect("prompt must be acknowledged before provider completion"),
    )
    .expect("prompt response JSON");
    assert_eq!(accepted["id"], "p1");
    assert_eq!(accepted["success"], true);

    write_rpc(
        &mut stdin,
        &json!({"id":"s1","type":"steer","message":"changed instruction"}),
    );
    write_rpc(
        &mut stdin,
        &json!({"id":"f1","type":"follow_up","message":"after that"}),
    );
    receive_rpc_until(&lines_rx, Duration::from_secs(1), |value| {
        value["id"] == "f1"
    });
    let first_end = receive_rpc_until(&lines_rx, Duration::from_secs(5), |value| {
        value["type"] == "agent_end"
    });
    assert_eq!(first_end["error"], serde_json::Value::Null);
    assert_eq!(first_end["messages"].as_array().unwrap().len(), 4);
    assert_eq!(
        first_end["messages"][3]["content"][0]["text"],
        "steered answer"
    );
    let follow_end = receive_rpc_until(&lines_rx, Duration::from_secs(3), |value| {
        value["type"] == "agent_end"
    });
    assert_eq!(
        follow_end["messages"][1]["content"][0]["text"],
        "follow-up answer"
    );

    write_rpc(
        &mut stdin,
        &json!({"id":"p2","type":"prompt","message":"cancel me"}),
    );
    receive_rpc_until(&lines_rx, Duration::from_secs(1), |value| {
        value["id"] == "p2"
    });
    receive_rpc_until(&lines_rx, Duration::from_secs(1), |value| {
        value["type"] == "turn_start"
    });
    write_rpc(
        &mut stdin,
        &json!({"id":"f2","type":"follow_up","message":"must be cleared"}),
    );
    receive_rpc_until(&lines_rx, Duration::from_secs(1), |value| {
        value["id"] == "f2"
    });
    std::thread::sleep(Duration::from_millis(50));
    write_rpc(&mut stdin, &json!({"id":"a1","type":"abort"}));
    receive_rpc_until(&lines_rx, Duration::from_secs(1), |value| {
        value["id"] == "a1"
    });
    let aborted = receive_rpc_until(&lines_rx, Duration::from_secs(2), |value| {
        value["type"] == "agent_end"
    });
    assert!(aborted["error"].as_str().unwrap().contains("cancelled"));
    write_rpc(&mut stdin, &json!({"id":"after-abort","type":"get_state"}));
    let after_abort = receive_rpc_until(&lines_rx, Duration::from_secs(1), |value| {
        value["id"] == "after-abort"
    });
    assert_eq!(after_abort["data"]["isStreaming"], false);
    assert_eq!(
        after_abort["data"]["sessionActions"]["followUps"]
            .as_array()
            .unwrap()
            .len(),
        0
    );

    write_rpc(
        &mut stdin,
        &json!({"id":"p3","type":"prompt","message":"finish after EOF"}),
    );
    receive_rpc_until(&lines_rx, Duration::from_secs(1), |value| {
        value["id"] == "p3"
    });
    drop(stdin);
    let eof_end = receive_rpc_until(&lines_rx, Duration::from_secs(3), |value| {
        value["type"] == "agent_end"
    });
    assert_eq!(eof_end["error"], serde_json::Value::Null);
    assert_eq!(eof_end["messages"][1]["content"][0]["text"], "EOF answer");
    let status = child.wait().expect("wait for RPC process");
    reader.join().expect("reader thread");
    assert!(status.success(), "RPC process exited with {status}");
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one linear subprocess scenario keeps queue command ordering and batching auditable"
)]
fn legacy_rpc_queue_modes_are_configurable_and_all_mode_batches_follow_ups() {
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    let mut child = StdCommand::new(assert_cmd::cargo::cargo_bin("mimir"))
        .args([
            "--provider",
            "fake",
            "--workspace",
            workspace.path().to_str().expect("workspace path"),
            "--state-dir",
            state.path().to_str().expect("state path"),
            "--session",
            "queue-modes",
            "--fake-delay-ms",
            "500",
            "--fake-response",
            "initial answer",
            "--fake-response",
            "combined follow-up answer",
            "--output",
            "rpc",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn RPC process");
    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let (lines_tx, lines_rx) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            if lines_tx.send(line.expect("stdout line")).is_err() {
                break;
            }
        }
    });

    write_rpc(&mut stdin, &json!({"id":"before","type":"get_state"}));
    let before = receive_rpc_until(&lines_rx, Duration::from_secs(3), |value| {
        value["id"] == "before"
    });
    assert_eq!(before["data"]["steeringMode"], "one-at-a-time");
    assert_eq!(before["data"]["followUpMode"], "one-at-a-time");

    write_rpc(
        &mut stdin,
        &json!({"id":"steering-mode","type":"set_steering_mode","mode":"all"}),
    );
    let steering_mode = receive_rpc_until(&lines_rx, Duration::from_secs(1), |value| {
        value["id"] == "steering-mode"
    });
    assert_eq!(steering_mode["success"], true);
    write_rpc(
        &mut stdin,
        &json!({"id":"follow-mode","type":"set_follow_up_mode","mode":"all"}),
    );
    let follow_mode = receive_rpc_until(&lines_rx, Duration::from_secs(1), |value| {
        value["id"] == "follow-mode"
    });
    assert_eq!(follow_mode["success"], true);
    write_rpc(&mut stdin, &json!({"id":"after","type":"get_state"}));
    let after = receive_rpc_until(&lines_rx, Duration::from_secs(1), |value| {
        value["id"] == "after"
    });
    assert_eq!(after["data"]["steeringMode"], "all");
    assert_eq!(after["data"]["followUpMode"], "all");

    write_rpc(
        &mut stdin,
        &json!({"id":"prompt","type":"prompt","message":"initial"}),
    );
    receive_rpc_until(&lines_rx, Duration::from_secs(1), |value| {
        value["id"] == "prompt"
    });
    write_rpc(
        &mut stdin,
        &json!({"id":"first-follow","type":"follow_up","message":"first follow-up"}),
    );
    write_rpc(
        &mut stdin,
        &json!({"id":"second-follow","type":"follow_up","message":"second follow-up"}),
    );
    receive_rpc_until(&lines_rx, Duration::from_secs(1), |value| {
        value["id"] == "second-follow"
    });
    receive_rpc_until(&lines_rx, Duration::from_secs(3), |value| {
        value["type"] == "agent_end"
    });
    let batched = receive_rpc_until(&lines_rx, Duration::from_secs(3), |value| {
        value["type"] == "agent_end"
    });
    assert_eq!(batched["error"], serde_json::Value::Null);
    assert_eq!(batched["messages"].as_array().unwrap().len(), 3);
    assert_eq!(
        batched["messages"][0]["content"][0]["text"],
        "first follow-up"
    );
    assert_eq!(
        batched["messages"][1]["content"][0]["text"],
        "second follow-up"
    );
    assert_eq!(
        batched["messages"][2]["content"][0]["text"],
        "combined follow-up answer"
    );

    write_rpc(
        &mut stdin,
        &json!({"id":"invalid","type":"set_follow_up_mode","mode":"invalid"}),
    );
    let invalid = receive_rpc_until(&lines_rx, Duration::from_secs(1), |value| {
        value["id"] == "invalid"
    });
    assert_eq!(invalid["success"], false);
    assert!(invalid["error"].as_str().unwrap().contains("mode"));

    drop(stdin);
    let status = child.wait().expect("wait for RPC process");
    reader.join().expect("reader thread");
    assert!(status.success(), "RPC process exited with {status}");
}

#[test]
fn legacy_rpc_controls_compaction_lists_skill_commands_and_exports_safe_html() {
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    let skill = workspace.path().join(".agents/skills/review");
    std::fs::create_dir_all(&skill).expect("skill directory");
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: review\ndescription: Review the current changes\n---\nCheck correctness.\n",
    )
    .expect("skill");
    let exported = workspace.path().join("session-export.html");
    Command::cargo_bin("mimir")
        .expect("binary")
        .args([
            "--provider",
            "fake",
            "--workspace",
            workspace.path().to_str().expect("workspace path"),
            "--state-dir",
            state.path().to_str().expect("state path"),
            "--fake-response",
            "<script>alert('unsafe')</script>",
            "--print",
            "show <unsafe> & content",
        ])
        .assert()
        .success();
    let input = [
        json!({"id":"compact","type":"set_auto_compaction","enabled":false}),
        json!({"id":"state","type":"get_state"}),
        json!({"id":"commands","type":"get_commands"}),
        json!({"id":"export","type":"export_html","outputPath":exported}),
    ]
    .into_iter()
    .fold(String::new(), |mut input, value| {
        use std::fmt::Write as _;
        writeln!(input, "{value}").expect("serialize RPC input");
        input
    });
    let output = Command::cargo_bin("mimir")
        .expect("binary")
        .args([
            "--provider",
            "fake",
            "--workspace",
            workspace.path().to_str().expect("workspace path"),
            "--state-dir",
            state.path().to_str().expect("state path"),
            "--fake-response",
            "unused",
            "--output",
            "rpc",
        ])
        .write_stdin(input)
        .output()
        .expect("RPC command");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let responses: Vec<serde_json::Value> = String::from_utf8(output.stdout)
        .expect("UTF-8 stdout")
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .filter(|value: &serde_json::Value| value["type"] == "response")
        .collect();
    let response = |id: &str| responses.iter().find(|value| value["id"] == id).unwrap();
    assert_eq!(response("compact")["success"], true);
    assert_eq!(response("state")["data"]["autoCompactionEnabled"], false);
    assert_eq!(
        response("commands")["data"]["commands"][0]["name"],
        "skill:review"
    );
    assert_eq!(
        response("export")["data"]["path"],
        std::fs::canonicalize(&exported)
            .expect("canonical export")
            .to_str()
            .unwrap()
    );
    let html = std::fs::read_to_string(exported).expect("exported HTML");
    assert!(html.contains("&lt;unsafe&gt; &amp; content"));
    assert!(html.contains("&lt;script&gt;alert(&#39;unsafe&#39;)&lt;/script&gt;"));
    assert!(!html.contains("<script>"));
}

#[test]
fn legacy_rpc_configures_auto_retry_and_streams_retry_lifecycle_events() {
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    let output = Command::cargo_bin("mimir")
        .expect("binary")
        .args([
            "--provider",
            "fake",
            "--workspace",
            workspace.path().to_str().expect("workspace path"),
            "--state-dir",
            state.path().to_str().expect("state path"),
            "--fake-retryable-failures",
            "1",
            "--fake-response",
            "recovered",
            "--output",
            "rpc",
        ])
        .write_stdin(
            "{\"id\":\"retry-on\",\"type\":\"set_auto_retry\",\"enabled\":true}\n\
             {\"id\":\"prompt\",\"type\":\"prompt\",\"message\":\"hello\"}\n",
        )
        .output()
        .expect("retry RPC command");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let lines: Vec<serde_json::Value> = String::from_utf8(output.stdout)
        .expect("UTF-8 stdout")
        .lines()
        .map(|line| serde_json::from_str(line).expect("RPC JSON"))
        .collect();
    assert!(
        lines
            .iter()
            .any(|value| { value["id"] == "retry-on" && value["success"] == true })
    );
    assert!(lines.iter().any(|value| {
        value["type"] == "auto_retry_start"
            && value["attempt"] == 1
            && value["maxAttempts"] == 3
            && value["delayMs"] == 2000
    }));
    assert!(
        lines
            .iter()
            .any(|value| { value["type"] == "auto_retry_end" && value["success"] == true })
    );
    assert!(lines.iter().any(|value| {
        value["type"] == "agent_end"
            && value["error"].is_null()
            && value["messages"][1]["content"][0]["text"] == "recovered"
    }));

    Command::cargo_bin("mimir")
        .expect("binary")
        .args([
            "--provider",
            "fake",
            "--workspace",
            workspace.path().to_str().expect("workspace path"),
            "--state-dir",
            state.path().to_str().expect("state path"),
            "--fake-response",
            "unused",
            "--output",
            "rpc",
        ])
        .write_stdin("{\"id\":\"abort\",\"type\":\"abort_retry\"}\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("\"command\":\"abort_retry\""))
        .stdout(predicate::str::contains("\"success\":true"));
}

#[test]
fn legacy_rpc_bash_is_disabled_by_default_and_requires_an_allowlist() {
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    let common = [
        "--provider",
        "fake",
        "--workspace",
        workspace.path().to_str().expect("workspace path"),
        "--state-dir",
        state.path().to_str().expect("state path"),
        "--fake-response",
        "unused",
        "--output",
        "rpc",
    ];
    Command::cargo_bin("mimir")
        .expect("binary")
        .args(common)
        .write_stdin("{\"id\":\"bash\",\"type\":\"bash\",\"command\":\"printf denied\"}\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("\"command\":\"bash\""))
        .stdout(predicate::str::contains("\"success\":false"))
        .stdout(predicate::str::contains("pass --allow-process"));

    Command::cargo_bin("mimir")
        .expect("binary")
        .args(common)
        .args(["--allow-process", "--allowed-programs", "printf"])
        .write_stdin("{\"id\":\"bash\",\"type\":\"bash\",\"command\":\"printf permitted\"}\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("\"command\":\"bash\""))
        .stdout(predicate::str::contains("\"success\":true"))
        .stdout(predicate::str::contains("permitted"));
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one subprocess scenario verifies bash execution, persistence, cancellation, and EOF cleanup"
)]
fn legacy_rpc_bash_executes_persists_and_aborts_concurrently() {
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    let mut child = StdCommand::new(assert_cmd::cargo::cargo_bin("mimir"))
        .args([
            "--provider",
            "fake",
            "--workspace",
            workspace.path().to_str().expect("workspace path"),
            "--state-dir",
            state.path().to_str().expect("state path"),
            "--session",
            "bash-rpc",
            "--allow-process",
            "--allowed-programs",
            "printf,sleep",
            "--fake-response",
            "unused",
            "--output",
            "rpc",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn RPC process");
    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let (lines_tx, lines_rx) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            if lines_tx.send(line.expect("stdout line")).is_err() {
                break;
            }
        }
    });

    write_rpc(
        &mut stdin,
        &json!({
            "id":"bash-ok",
            "type":"bash",
            "command":"printf captured"
        }),
    );
    let completed = receive_rpc_until(&lines_rx, Duration::from_secs(10), |value| {
        value["id"] == "bash-ok"
    });
    assert_eq!(completed["success"], true);
    assert_eq!(completed["data"]["output"], "captured");
    assert_eq!(completed["data"]["exitCode"], 0);
    assert_eq!(completed["data"]["cancelled"], false);

    write_rpc(&mut stdin, &json!({"id":"messages","type":"get_messages"}));
    let messages = receive_rpc_until(&lines_rx, Duration::from_secs(1), |value| {
        value["id"] == "messages"
    });
    let context = messages["data"]["messages"][0]["content"][0]["text"]
        .as_str()
        .expect("bash context");
    assert!(context.contains("Ran `printf captured`"));
    assert!(context.contains("captured"));

    write_rpc(
        &mut stdin,
        &json!({"id":"bash-slow","type":"bash","command":"sleep 30"}),
    );
    receive_rpc_until(&lines_rx, Duration::from_secs(1), |value| {
        value["type"] == "bash_start"
    });
    write_rpc(&mut stdin, &json!({"id":"abort-bash","type":"abort_bash"}));
    let abort = receive_rpc_until(&lines_rx, Duration::from_secs(1), |value| {
        value["id"] == "abort-bash"
    });
    assert_eq!(abort["success"], true);
    let cancelled = receive_rpc_until(&lines_rx, Duration::from_secs(2), |value| {
        value["id"] == "bash-slow"
    });
    assert_eq!(cancelled["success"], true);
    assert_eq!(cancelled["data"]["cancelled"], true);
    assert_eq!(cancelled["data"]["exitCode"], serde_json::Value::Null);

    drop(stdin);
    let status = child.wait().expect("wait for RPC process");
    reader.join().expect("reader thread");
    assert!(status.success(), "RPC process exited with {status}");
}

#[test]
fn legacy_rpc_agent_message_commands_deliver_and_enforce_pause() {
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    let common = [
        "--provider",
        "fake",
        "--workspace",
        workspace.path().to_str().expect("workspace path"),
        "--state-dir",
        state.path().to_str().expect("state path"),
    ];
    Command::cargo_bin("mimir")
        .expect("binary")
        .args(common)
        .args([
            "--session",
            "target",
            "--fake-response",
            "target ready",
            "--print",
            "initialize target",
        ])
        .assert()
        .success();

    let input = concat!(
        "{\"id\":\"status\",\"type\":\"agent_messages_status\"}\n",
        "{\"id\":\"bad-target\",\"type\":\"send_message\",\"targetActiveSessionId\":\"../target\",\"message\":\"blocked\"}\n",
        "{\"id\":\"blank-message\",\"type\":\"send_message\",\"targetActiveSessionId\":\"target\",\"message\":\"   \"}\n",
        "{\"id\":\"pause\",\"type\":\"agent_messages_pause\"}\n",
        "{\"id\":\"blocked\",\"type\":\"send_message\",\"targetActiveSessionId\":\"target\",\"message\":\"blocked\"}\n",
        "{\"id\":\"resume\",\"type\":\"agent_messages_resume\"}\n",
        "{\"id\":\"send\",\"type\":\"send_message\",\"targetActiveSessionId\":\"target\",\"message\":\"review complete\"}\n",
        "{\"id\":\"clear\",\"type\":\"agent_messages_clear\"}\n"
    );
    let output = Command::cargo_bin("mimir")
        .expect("binary")
        .args(common)
        .args([
            "--session",
            "source",
            "--fake-response",
            "message handled",
            "--output",
            "rpc",
        ])
        .write_stdin(input)
        .output()
        .expect("agent message RPC output");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let responses = String::from_utf8(output.stdout)
        .expect("UTF-8 responses")
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("response JSON"))
        .collect::<Vec<_>>();
    let response = |id: &str| {
        responses
            .iter()
            .find(|value| value["id"] == id)
            .expect("response by id")
    };
    assert_eq!(response("status")["data"]["paused"], false);
    assert_eq!(response("status")["data"]["maxMessageChars"], 16_384);
    assert_eq!(response("bad-target")["success"], false);
    assert_eq!(response("blank-message")["success"], false);
    assert_eq!(response("pause")["data"]["paused"], true);
    assert_eq!(response("blocked")["success"], false);
    assert_eq!(response("resume")["data"]["paused"], false);
    assert_eq!(response("send")["success"], true);
    assert_eq!(response("send")["data"]["source"], "agent_message");
    assert_eq!(
        response("send")["data"]["target"]["activeSessionId"],
        "target"
    );
    assert_eq!(response("send")["data"]["message"], "review complete");
    assert!(matches!(
        response("send")["data"]["deliveryStatus"].as_str(),
        Some("delivered" | "queued")
    ));
    assert_eq!(response("clear")["data"]["cleared"], 0);

    let target_session = std::fs::read_to_string(state.path().join("sessions/target.jsonl"))
        .expect("target session");
    assert!(target_session.contains("review complete"));

    Command::cargo_bin("mimir")
        .expect("binary")
        .args(common)
        .args(["daemon", "status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"active_leases\": 0"));

    Command::cargo_bin("mimir")
        .expect("binary")
        .args(common)
        .args(["daemon", "stop"])
        .assert()
        .success();
}

#[test]
fn legacy_rpc_observe_streams_new_target_messages_and_unobserves() {
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    let common = [
        "--provider",
        "fake",
        "--workspace",
        workspace.path().to_str().expect("workspace path"),
        "--state-dir",
        state.path().to_str().expect("state path"),
    ];
    let run_target = |prompt: &str, response: &str| {
        Command::cargo_bin("mimir")
            .expect("binary")
            .args(common)
            .args([
                "--session",
                "observed",
                "--fake-response",
                response,
                "--print",
                prompt,
            ])
            .assert()
            .success();
    };
    run_target("initial prompt", "initial answer");

    let mut child = StdCommand::new(assert_cmd::cargo::cargo_bin("mimir"))
        .args(common)
        .args([
            "--session",
            "observer",
            "--fake-response",
            "unused",
            "--output",
            "rpc",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn observer");
    let mut stdin = child.stdin.take().expect("observer stdin");
    let stdout = child.stdout.take().expect("observer stdout");
    let (lines_tx, lines_rx) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            if lines_tx.send(line.expect("observer line")).is_err() {
                break;
            }
        }
    });
    write_rpc(
        &mut stdin,
        &json!({"id":"observe","type":"observe","activeSessionId":"observed"}),
    );
    let observed = receive_rpc_response(&lines_rx, "observe");
    assert_eq!(observed["success"], true);
    assert_eq!(observed["data"]["messages"].as_array().unwrap().len(), 2);
    write_rpc(
        &mut stdin,
        &json!({"id":"observe-again","type":"observe","activeSessionId":"observed"}),
    );
    let observed_again = receive_rpc_response(&lines_rx, "observe-again");
    assert_eq!(observed_again["success"], true);
    assert_eq!(
        observed_again["data"]["messages"].as_array().unwrap().len(),
        2
    );

    run_target("new target prompt", "new target answer");
    let event = receive_rpc_until(&lines_rx, Duration::from_secs(2), |value| {
        value["type"] == "observed_session_event"
            && value["event"]["message"]["content"][0]["text"] == "new target prompt"
    });
    assert_eq!(event["activeSessionId"], "observed");

    write_rpc(
        &mut stdin,
        &json!({"id":"unobserve","type":"unobserve","activeSessionId":"observed"}),
    );
    let stopped = receive_rpc_response(&lines_rx, "unobserve");
    assert_eq!(stopped["success"], true);
    write_rpc(
        &mut stdin,
        &json!({"id":"unobserve-again","type":"unobserve","activeSessionId":"observed"}),
    );
    let stopped_again = receive_rpc_response(&lines_rx, "unobserve-again");
    assert_eq!(stopped_again["success"], true);
    write_rpc(
        &mut stdin,
        &json!({"id":"bad-observe","type":"observe","activeSessionId":"../observed"}),
    );
    let rejected = receive_rpc_response(&lines_rx, "bad-observe");
    assert_eq!(rejected["success"], false);
    run_target("after unobserve", "not streamed");
    assert!(lines_rx.recv_timeout(Duration::from_millis(250)).is_err());

    drop(stdin);
    let status = child.wait().expect("observer status");
    reader.join().expect("observer reader");
    assert!(status.success(), "observer exited with {status}");
}

#[test]
fn legacy_rpc_schedules_are_durable_filterable_and_reference_shaped() {
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    let common = [
        "--provider",
        "fake",
        "--workspace",
        workspace.path().to_str().expect("workspace path"),
        "--state-dir",
        state.path().to_str().expect("state path"),
        "--session",
        "schedule-rpc",
        "--fake-response",
        "unused",
        "--output",
        "rpc",
    ];
    let added = Command::cargo_bin("mimir")
        .expect("binary")
        .args(common)
        .write_stdin(
            "{\"id\":\"add\",\"type\":\"add_schedule\",\"schedule\":\"every 10s\",\"prompt\":\"check the queue\"}\n",
        )
        .output()
        .expect("add schedule RPC");
    assert!(added.status.success());
    let added: serde_json::Value = serde_json::from_slice(&added.stdout).expect("add response");
    assert_eq!(added["success"], true);
    assert_eq!(added["data"]["job"]["status"], "active");
    assert_eq!(added["data"]["job"]["source"], "cron");
    assert_eq!(added["data"]["job"]["activeSessionId"], "schedule-rpc");
    assert_eq!(added["data"]["job"]["schedule"]["kind"], "interval");
    assert_eq!(added["data"]["job"]["schedule"]["intervalMs"], 10_000);
    assert_eq!(added["data"]["job"]["runCount"], 0);
    let job_id = added["data"]["job"]["id"].as_str().expect("job id");

    let follow_up = format!(
        "{{\"id\":\"cancel\",\"type\":\"cancel_schedule\",\"jobId\":\"{job_id}\"}}\n\
         {{\"id\":\"active\",\"type\":\"list_schedules\"}}\n\
         {{\"id\":\"all\",\"type\":\"list_schedules\",\"includeInactive\":true}}\n"
    );
    let listed = Command::cargo_bin("mimir")
        .expect("binary")
        .args(common)
        .write_stdin(follow_up)
        .output()
        .expect("list/cancel RPC");
    assert!(listed.status.success());
    let responses: Vec<serde_json::Value> = String::from_utf8(listed.stdout)
        .expect("UTF-8 RPC")
        .lines()
        .map(|line| serde_json::from_str(line).expect("RPC response"))
        .collect();
    assert_eq!(responses[0]["data"]["job"]["status"], "cancelled");
    assert!(responses[1]["data"]["jobs"].as_array().unwrap().is_empty());
    assert_eq!(responses[2]["data"]["jobs"].as_array().unwrap().len(), 1);
    assert_eq!(responses[2]["data"]["jobs"][0]["status"], "cancelled");

    Command::cargo_bin("mimir")
        .expect("binary")
        .args(common)
        .write_stdin(
            "{\"id\":\"invalid\",\"type\":\"add_schedule\",\"schedule\":\"every 1s\",\"prompt\":\"too fast\"}\n",
        )
        .assert()
        .success()
        .stdout(predicate::str::contains("\"success\":false"))
        .stdout(predicate::str::contains("at least 10 seconds"));

    Command::cargo_bin("mimir")
        .expect("binary")
        .args(common)
        .write_stdin(
            "{\"id\":\"due\",\"type\":\"add_schedule\",\"schedule\":\"in 0m\",\"prompt\":\"scheduled after EOF\"}\n",
        )
        .assert()
        .success()
        .stdout(predicate::str::contains("\"success\":true"));
    let transcript = state.path().join("sessions/schedule-rpc.jsonl");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let contents = std::fs::read_to_string(&transcript).unwrap_or_default();
        if contents.contains("scheduled after EOF") && contents.contains("unused") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "RPC-added due schedule did not execute after stdin EOF"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    Command::cargo_bin("mimir")
        .expect("binary")
        .args(common)
        .args(["daemon", "stop"])
        .assert()
        .success();
}

#[test]
fn legacy_rpc_heartbeats_match_reference_lifecycle_and_catalog_shapes() {
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    let common = [
        "--provider",
        "fake",
        "--workspace",
        workspace.path().to_str().expect("workspace path"),
        "--state-dir",
        state.path().to_str().expect("state path"),
        "--session",
        "heartbeat-rpc",
        "--fake-response",
        "unused",
        "--output",
        "rpc",
    ];
    let set = Command::cargo_bin("mimir")
        .expect("binary")
        .args(common)
        .write_stdin(
            "{\"id\":\"set\",\"type\":\"set_heartbeat\",\"schedule\":\"10m\",\"prompt\":\"inspect the queue\",\"deliveryMode\":\"follow_up\"}\n",
        )
        .output()
        .expect("set heartbeat RPC");
    assert!(set.status.success());
    let set: serde_json::Value = serde_json::from_slice(&set.stdout).expect("set response");
    assert_eq!(set["success"], true);
    let heartbeat = &set["data"]["heartbeat"];
    assert_eq!(heartbeat["status"], "active");
    assert_eq!(heartbeat["source"], "heartbeat");
    assert_eq!(heartbeat["deliveryMode"], "follow_up");
    assert_eq!(heartbeat["activeSessionId"], "heartbeat-rpc");
    assert_eq!(heartbeat["schedule"]["kind"], "interval");
    assert_eq!(heartbeat["schedule"]["expression"], "every 10m");
    assert_eq!(heartbeat["schedule"]["intervalMs"], 600_000);
    let job_id = heartbeat["id"].as_str().expect("job id");

    let lifecycle = format!(
        "{{\"id\":\"get\",\"type\":\"get_heartbeat\"}}\n\
         {{\"id\":\"list\",\"type\":\"list_heartbeats\"}}\n\
         {{\"id\":\"pause\",\"type\":\"update_heartbeat\",\"action\":\"pause\"}}\n\
         {{\"id\":\"schedules\",\"type\":\"list_schedules\"}}\n\
         {{\"id\":\"resume\",\"type\":\"update_heartbeat\",\"action\":\"resume\"}}\n\
         {{\"id\":\"manage-pause\",\"type\":\"manage_heartbeat\",\"activeSessionId\":\"heartbeat-rpc\",\"jobId\":\"{job_id}\",\"action\":\"pause\"}}\n\
         {{\"id\":\"manage-resume\",\"type\":\"manage_heartbeat\",\"activeSessionId\":\"heartbeat-rpc\",\"jobId\":\"{job_id}\",\"action\":\"resume\"}}\n\
         {{\"id\":\"stop\",\"type\":\"manage_heartbeat\",\"activeSessionId\":\"heartbeat-rpc\",\"jobId\":\"{job_id}\",\"action\":\"stop\"}}\n\
         {{\"id\":\"after\",\"type\":\"get_heartbeat\"}}\n\
         {{\"id\":\"set-clear\",\"type\":\"set_heartbeat\",\"schedule\":\"every 15m\",\"prompt\":\"clear me\"}}\n\
         {{\"id\":\"clear\",\"type\":\"update_heartbeat\",\"action\":\"clear\"}}\n\
         {{\"id\":\"after-clear\",\"type\":\"get_heartbeat\"}}\n\
         {{\"id\":\"invalid\",\"type\":\"set_heartbeat\",\"schedule\":\"in 5m\",\"prompt\":\"not recurring\"}}\n"
    );
    let output = Command::cargo_bin("mimir")
        .expect("binary")
        .args(common)
        .write_stdin(lifecycle)
        .output()
        .expect("heartbeat lifecycle RPC");
    assert!(output.status.success());
    let responses: Vec<serde_json::Value> = String::from_utf8(output.stdout)
        .expect("UTF-8 RPC")
        .lines()
        .map(|line| serde_json::from_str(line).expect("RPC response"))
        .collect();
    assert_eq!(responses.len(), 13);
    assert_eq!(responses[0]["data"]["heartbeat"]["id"], job_id);
    assert_eq!(
        responses[1]["data"]["heartbeats"].as_array().unwrap().len(),
        1
    );
    assert_eq!(responses[1]["data"]["heartbeats"][0]["job"]["id"], job_id);
    assert_eq!(responses[2]["data"]["heartbeat"]["status"], "paused");
    assert!(responses[2]["data"]["heartbeat"].get("nextRunAt").is_none());
    assert_eq!(responses[3]["data"]["jobs"][0]["status"], "paused");
    assert_eq!(responses[4]["data"]["heartbeat"]["status"], "active");
    assert!(responses[4]["data"]["heartbeat"]["nextRunAt"].is_string());
    assert_eq!(responses[5]["data"]["heartbeat"]["status"], "paused");
    assert_eq!(responses[6]["data"]["heartbeat"]["status"], "active");
    assert_eq!(responses[7]["data"]["heartbeat"]["status"], "cancelled");
    assert_eq!(responses[8]["data"]["heartbeat"], serde_json::Value::Null);
    assert_eq!(responses[9]["data"]["heartbeat"]["deliveryMode"], "steer");
    assert_eq!(responses[10]["data"]["heartbeat"]["status"], "cancelled");
    assert_eq!(responses[11]["data"]["heartbeat"], serde_json::Value::Null);
    assert_eq!(responses[12]["success"], false);
    assert!(
        responses[12]["error"]
            .as_str()
            .unwrap()
            .contains("must be recurring")
    );

    Command::cargo_bin("mimir")
        .expect("binary")
        .args(common)
        .args(["daemon", "stop"])
        .assert()
        .success();
}

#[test]
fn legacy_rpc_heartbeat_catalog_is_scoped_to_the_owned_session() {
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    let run = |session: &str, input: &str| {
        Command::cargo_bin("mimir")
            .expect("binary")
            .args([
                "--provider",
                "fake",
                "--workspace",
                workspace.path().to_str().expect("workspace path"),
                "--state-dir",
                state.path().to_str().expect("state path"),
                "--session",
                session,
                "--fake-response",
                "unused",
                "--output",
                "rpc",
            ])
            .write_stdin(input)
            .output()
            .expect("heartbeat RPC")
    };
    assert!(
        run(
            "owned",
            "{\"type\":\"set_session_name\",\"name\":\"Owned session\"}\n\
             {\"type\":\"set_heartbeat\",\"schedule\":\"every 10m\",\"prompt\":\"owned\"}\n"
        )
        .status
        .success()
    );
    assert!(
        run(
            "foreign",
            "{\"type\":\"set_heartbeat\",\"schedule\":\"every 20m\",\"prompt\":\"foreign\"}\n"
        )
        .status
        .success()
    );

    let listed = run("owned", "{\"id\":\"list\",\"type\":\"list_heartbeats\"}\n");
    assert!(listed.status.success());
    let listed: serde_json::Value = serde_json::from_slice(&listed.stdout).expect("list response");
    let heartbeats = listed["data"]["heartbeats"].as_array().expect("heartbeats");
    assert_eq!(heartbeats.len(), 1);
    assert_eq!(heartbeats[0]["job"]["activeSessionId"], "owned");
    assert_eq!(heartbeats[0]["sessionName"], "Owned session");

    Command::cargo_bin("mimir")
        .expect("binary")
        .args([
            "--workspace",
            workspace.path().to_str().expect("workspace path"),
            "--state-dir",
            state.path().to_str().expect("state path"),
            "daemon",
            "stop",
        ])
        .assert()
        .success();
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one subprocess transcript proves the complete five-command state transition"
)]
fn legacy_rpc_model_and_thinking_controls_are_runtime_backed_and_durable() {
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    let home = TempDir::new().expect("home");
    let workspace_path = workspace.path().to_str().expect("workspace path");
    let state_path = state.path().to_str().expect("state path");

    Command::cargo_bin("mimir")
        .expect("binary")
        .env("HOME", home.path())
        .current_dir(workspace.path())
        .args([
            "--workspace",
            workspace_path,
            "--state-dir",
            state_path,
            "login",
            "openai",
            "--api-key-stdin",
        ])
        .write_stdin("catalog-test-key\n")
        .assert()
        .success();

    let common = [
        "--provider",
        "fake",
        "--model",
        "sandbox-model",
        "--workspace",
        workspace_path,
        "--state-dir",
        state_path,
        "--session",
        "model-rpc",
        "--fake-response",
        "unused",
        "--output",
        "rpc",
    ];
    let output = Command::cargo_bin("mimir")
        .expect("binary")
        .env("HOME", home.path())
        .current_dir(workspace.path())
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("ANTHROPIC_OAUTH_TOKEN")
        .args(common)
        .write_stdin(
            "{\"id\":\"models\",\"type\":\"get_available_models\"}\n\
             {\"id\":\"set\",\"type\":\"set_model\",\"provider\":\"openai\",\"modelId\":\"gpt-5-mini\"}\n\
             {\"id\":\"state-model\",\"type\":\"get_state\"}\n\
             {\"id\":\"think\",\"type\":\"set_thinking_level\",\"level\":\"high\"}\n\
             {\"id\":\"state-thinking\",\"type\":\"get_state\"}\n\
             {\"id\":\"cycle-thinking\",\"type\":\"cycle_thinking_level\"}\n\
             {\"id\":\"state-cycled-thinking\",\"type\":\"get_state\"}\n\
             {\"id\":\"invalid-model\",\"type\":\"set_model\",\"provider\":\"anthropic\",\"modelId\":\"claude\"}\n\
             {\"id\":\"invalid-thinking\",\"type\":\"set_thinking_level\",\"level\":\"turbo\"}\n",
        )
        .output()
        .expect("model RPC output");
    assert!(output.status.success());
    let output_values: Vec<serde_json::Value> = String::from_utf8(output.stdout)
        .expect("UTF-8 RPC")
        .lines()
        .map(|line| serde_json::from_str(line).expect("RPC response"))
        .collect();
    let responses: Vec<_> = output_values
        .iter()
        .filter(|value| value["type"] == "response")
        .cloned()
        .collect();
    assert_eq!(responses.len(), 9);
    let thinking_events: Vec<_> = output_values
        .iter()
        .filter(|value| value["type"] == "thinking_level_changed")
        .collect();
    assert_eq!(thinking_events.len(), 3);
    assert_eq!(thinking_events[0]["level"], "minimal");
    assert_eq!(thinking_events[1]["level"], "high");
    assert_eq!(thinking_events[2]["level"], "minimal");

    let models = responses[0]["data"]["models"]
        .as_array()
        .expect("available models");
    assert!(
        models
            .iter()
            .any(|model| { model["provider"] == "fake" && model["id"] == "sandbox-model" })
    );
    let openai = models
        .iter()
        .find(|model| model["provider"] == "openai" && model["id"] == "gpt-5-mini")
        .expect("configured OpenAI model");
    assert_eq!(openai["reasoning"], true);
    assert_eq!(openai["input"], json!(["text", "image"]));
    assert_eq!(openai["contextWindow"], 400_000);
    assert_eq!(openai["maxTokens"], 128_000);
    assert_eq!(openai["thinkingLevelMap"]["off"], serde_json::Value::Null);
    assert!(!models.iter().any(|model| model["provider"] == "anthropic"));
    assert!(
        !models
            .iter()
            .any(|model| model["provider"] == "github-copilot")
    );

    assert_eq!(responses[1]["success"], true);
    assert_eq!(responses[1]["data"]["provider"], "openai");
    assert_eq!(responses[1]["data"]["id"], "gpt-5-mini");
    assert_eq!(responses[2]["data"]["model"]["provider"], "openai");
    assert_eq!(responses[2]["data"]["model"]["id"], "gpt-5-mini");
    assert_eq!(responses[3]["success"], true);
    assert_eq!(responses[4]["data"]["thinkingLevel"], "high");
    assert_eq!(responses[5]["data"]["level"], "minimal");
    assert_eq!(responses[6]["data"]["thinkingLevel"], "minimal");
    assert_eq!(responses[7]["success"], false);
    assert!(
        responses[7]["error"]
            .as_str()
            .expect("model error")
            .contains("Model not found")
    );
    assert_eq!(responses[8]["success"], false);
    assert!(
        responses[8]["error"]
            .as_str()
            .expect("thinking error")
            .contains("thinking level")
    );

    let restored = Command::cargo_bin("mimir")
        .expect("binary")
        .current_dir(workspace.path())
        .args(common)
        .write_stdin("{\"id\":\"state\",\"type\":\"get_state\"}\n")
        .output()
        .expect("restored model RPC output");
    assert!(restored.status.success());
    let restored: serde_json::Value =
        serde_json::from_slice(&restored.stdout).expect("restored state response");
    assert_eq!(restored["data"]["model"]["provider"], "openai");
    assert_eq!(restored["data"]["model"]["id"], "gpt-5-mini");
    assert_eq!(restored["data"]["thinkingLevel"], "minimal");
}

fn write_rpc(stdin: &mut impl Write, value: &serde_json::Value) {
    writeln!(stdin, "{value}").expect("write RPC command");
    stdin.flush().expect("flush RPC command");
}

fn receive_rpc_until(
    lines: &mpsc::Receiver<String>,
    timeout: Duration,
    predicate: impl Fn(&serde_json::Value) -> bool,
) -> serde_json::Value {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let line = lines.recv_timeout(remaining).expect("matching RPC output");
        let value: serde_json::Value = serde_json::from_str(&line).expect("RPC JSON line");
        if predicate(&value) {
            return value;
        }
    }
}

fn receive_rpc_response(receiver: &mpsc::Receiver<String>, id: &str) -> serde_json::Value {
    receive_rpc_until(receiver, Duration::from_secs(2), |value| value["id"] == id)
}

#[test]
fn management_commands_round_trip_without_a_provider() {
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    let common = [
        "--workspace",
        workspace.path().to_str().expect("workspace path"),
        "--state-dir",
        state.path().to_str().expect("state path"),
    ];
    Command::cargo_bin("mimir")
        .expect("binary")
        .args(common)
        .args(["goal", "set", "finish migration", "--token-budget", "100"])
        .assert()
        .success()
        .stdout(predicate::str::contains("finish migration"));
    Command::cargo_bin("mimir")
        .expect("binary")
        .env_remove("OPENAI_API_KEY")
        .args(common)
        .args(["goal", "show"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"token_budget\": 100"));
    Command::cargo_bin("mimir")
        .expect("binary")
        .args(common)
        .args(["schedule", "add", "heartbeat", "continue"])
        .assert()
        .success()
        .stdout(predicate::str::contains("heartbeat"));
    Command::cargo_bin("mimir")
        .expect("binary")
        .env_remove("OPENAI_API_KEY")
        .args(common)
        .args([
            "--provider",
            "fake",
            "--session",
            "persisted",
            "--fake-response",
            "saved",
            "--print",
            "hello",
        ])
        .assert()
        .success()
        .stdout("saved\n");
    Command::cargo_bin("mimir")
        .expect("binary")
        .args(common)
        .args(["session", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("persisted"));
    Command::cargo_bin("mimir")
        .expect("binary")
        .env_remove("OPENAI_API_KEY")
        .args(common)
        .args([
            "--provider",
            "fake",
            "--fake-response",
            "bench",
            "benchmark",
            "prompt",
            "hello",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"elapsed_ms\""))
        .stdout(predicate::str::contains("\"answer_chars\""));
}

#[test]
fn session_compat_commands_export_import_and_plan_switches() {
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    let exported = workspace.path().join("reference.jsonl");
    let common = [
        "--workspace",
        workspace.path().to_str().expect("workspace path"),
        "--state-dir",
        state.path().to_str().expect("state path"),
    ];
    Command::cargo_bin("mimir")
        .expect("binary")
        .env_remove("OPENAI_API_KEY")
        .args(common)
        .args([
            "--provider",
            "fake",
            "--session",
            "portable",
            "--fake-response",
            "portable answer",
            "--print",
            "hello",
        ])
        .assert()
        .success();
    Command::cargo_bin("mimir")
        .expect("binary")
        .args(common)
        .args([
            "--session",
            "portable",
            "session",
            "export",
            exported.to_str().expect("export path"),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"format\": \"reference_v3\""));
    let exported_text = std::fs::read_to_string(&exported).expect("reference export");
    assert!(exported_text.contains("\"type\":\"session\""));

    Command::cargo_bin("mimir")
        .expect("binary")
        .args(common)
        .args(["session", "switch", exported.to_str().expect("export path")])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"planned\": true"));
    Command::cargo_bin("mimir")
        .expect("binary")
        .args(common)
        .args(["session", "import", exported.to_str().expect("export path")])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"imported\": true"));
}

#[test]
fn session_share_prints_only_safe_payload_metadata() {
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    let html = workspace.path().join("session.html");
    std::fs::write(&html, "<html>private transcript marker</html>").expect("share source");
    Command::cargo_bin("mimir")
        .expect("binary")
        .args([
            "--workspace",
            workspace.path().to_str().expect("workspace path"),
            "--state-dir",
            state.path().to_str().expect("state path"),
            "session",
            "share",
            html.to_str().expect("HTML path"),
            "--gist-id",
            "0123456789abcdef",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("private transcript marker").not())
        .stdout(predicate::str::contains("\"filename\": \"session.html\""))
        .stdout(predicate::str::contains(
            "https://pi.dev/session/#0123456789abcdef",
        ));
}

#[test]
fn mcp_catalog_commands_persist_redacted_server_configuration() {
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    let common = [
        "--workspace",
        workspace.path().to_str().expect("workspace path"),
        "--state-dir",
        state.path().to_str().expect("state path"),
    ];
    Command::cargo_bin("mimir")
        .expect("binary")
        .args(common)
        .args([
            "mcp",
            "add",
            "local",
            "--label",
            "Local mock",
            "--program",
            "/bin/sh",
            "--bearer-token-env",
            "PRIVATE_TOKEN",
        ])
        .assert()
        .success();
    Command::cargo_bin("mimir")
        .expect("binary")
        .args(common)
        .args(["mcp", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Local mock"))
        .stdout(predicate::str::contains("PRIVATE_TOKEN"))
        .stdout(predicate::str::contains("do-not-print").not());
    Command::cargo_bin("mimir")
        .expect("binary")
        .args(common)
        .args(["mcp", "status", "local"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"configured\": true"));
    Command::cargo_bin("mimir")
        .expect("binary")
        .args(common)
        .args(["mcp", "remove", "local"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"removed\": true"));
}

fn write_mock_mcp_server(directory: &std::path::Path) -> (String, std::path::PathBuf) {
    let python = StdCommand::new("sh")
        .args(["-c", "command -v python3"])
        .output()
        .expect("locate python3");
    assert!(python.status.success());
    let python = String::from_utf8(python.stdout)
        .expect("python path")
        .trim()
        .to_owned();
    let script = directory.join("mock_mcp.py");
    std::fs::write(
        &script,
        r"import json, sys
def read_message():
    size = None
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            return None
        if line in (b'\r\n', b'\n'):
            break
        if line.lower().startswith(b'content-length:'):
            size = int(line.split(b':', 1)[1].strip())
    return json.loads(sys.stdin.buffer.read(size))
def send(value):
    payload = json.dumps(value, separators=(',', ':')).encode()
    sys.stdout.buffer.write(b'Content-Length: ' + str(len(payload)).encode() + b'\r\n\r\n' + payload)
    sys.stdout.buffer.flush()
while True:
    request = read_message()
    if request is None:
        break
    if 'id' not in request:
        continue
    method = request.get('method')
    if method == 'initialize':
        result = {'protocolVersion':'2025-06-18','capabilities':{},'serverInfo':{'name':'mock','version':'1'}}
    elif method == 'tools/list':
        result = {'tools':[{'name':'echo','description':'Echo value','inputSchema':{'type':'object'}}]}
    else:
        result = {'content':[{'type':'text','text':request['params']['arguments'].get('value', '')}]}
    send({'jsonrpc':'2.0','id':request['id'],'result':result})
",
    )
    .expect("mock MCP server");
    (python, script)
}

#[test]
fn mcp_tools_and_call_use_the_local_stdio_client() {
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    let (python, script) = write_mock_mcp_server(workspace.path());
    let common = [
        "--workspace",
        workspace.path().to_str().expect("workspace path"),
        "--state-dir",
        state.path().to_str().expect("state path"),
    ];
    Command::cargo_bin("mimir")
        .expect("binary")
        .args(common)
        .args([
            "mcp",
            "add",
            "mock",
            "--label",
            "Mock",
            "--program",
            &python,
            "--arg",
            script.to_str().expect("script path"),
        ])
        .assert()
        .success();
    Command::cargo_bin("mimir")
        .expect("binary")
        .args(common)
        .args(["mcp", "tools", "mock"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"name\": \"echo\""));
    Command::cargo_bin("mimir")
        .expect("binary")
        .args(common)
        .args([
            "mcp",
            "call",
            "mock",
            "echo",
            "--arguments",
            r#"{"value":"local result"}"#,
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("local result"));
}

#[test]
fn mcp_builtin_remote_entries_surface_transport_and_oauth_metadata() {
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    let common = [
        "--workspace",
        workspace.path().to_str().expect("workspace path"),
        "--state-dir",
        state.path().to_str().expect("state path"),
    ];
    Command::cargo_bin("mimir")
        .expect("binary")
        .args(common)
        .args(["mcp", "add", "linear", "--builtin"])
        .assert()
        .success();
    Command::cargo_bin("mimir")
        .expect("binary")
        .args(common)
        .args(["mcp", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"server\": \"linear\""))
        .stdout(predicate::str::contains("\"transport\": \"http\""))
        .stdout(predicate::str::contains("https://mcp.linear.app/mcp"))
        .stdout(predicate::str::contains("\"oauth\": true"));
}

#[test]
fn mcp_login_and_logout_manage_remote_api_key_credentials_without_echoing_secrets() {
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    let home = TempDir::new().expect("home");
    let common = [
        "--workspace",
        workspace.path().to_str().expect("workspace path"),
        "--state-dir",
        state.path().to_str().expect("state path"),
    ];
    Command::cargo_bin("mimir")
        .expect("binary")
        .env("HOME", home.path())
        .args(common)
        .args([
            "mcp",
            "add",
            "acme",
            "--url",
            "https://mcp.example.com/mcp",
            "--label",
            "Acme",
        ])
        .assert()
        .success();

    let mut child = StdCommand::new(assert_cmd::cargo::cargo_bin("mimir"))
        .env("HOME", home.path())
        .args(common)
        .args(["mcp", "login", "acme", "--api-key-stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mcp login");
    let mut stdin = child.stdin.take().expect("stdin");
    stdin
        .write_all(b"super-secret-api-key\n")
        .expect("write api key");
    drop(stdin);
    let output = child.wait_with_output().expect("wait for login");
    assert!(
        output.status.success(),
        "login failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("stdout utf8");
    assert!(stdout.contains("\"auth_type\": \"api_key\""));
    assert!(!stdout.contains("super-secret-api-key"));

    Command::cargo_bin("mimir")
        .expect("binary")
        .env("HOME", home.path())
        .args(common)
        .args(["mcp", "status", "acme"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"authenticated\": true"))
        .stdout(predicate::str::contains("\"source\": \"stored_api_key\""));

    Command::cargo_bin("mimir")
        .expect("binary")
        .env("HOME", home.path())
        .args(common)
        .args(["mcp", "logout", "acme"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"logged_out\": true"));

    Command::cargo_bin("mimir")
        .expect("binary")
        .env("HOME", home.path())
        .args(common)
        .args(["mcp", "status", "acme"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"authenticated\": false"));
}

#[test]
fn mcp_oauth_builtin_rejects_api_keys_without_echoing_the_secret() {
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    let home = TempDir::new().expect("home");
    let common = [
        "--workspace",
        workspace.path().to_str().expect("workspace path"),
        "--state-dir",
        state.path().to_str().expect("state path"),
    ];
    Command::cargo_bin("mimir")
        .expect("binary")
        .env("HOME", home.path())
        .args(common)
        .args(["mcp", "add", "notion", "--builtin"])
        .assert()
        .success();

    Command::cargo_bin("mimir")
        .expect("binary")
        .env("HOME", home.path())
        .args(common)
        .args(["mcp", "login", "notion", "--api-key-stdin"])
        .write_stdin("must-never-be-printed\n")
        .assert()
        .failure()
        .stderr(predicate::str::contains("requires OAuth"))
        .stdout(predicate::str::contains("must-never-be-printed").not())
        .stderr(predicate::str::contains("must-never-be-printed").not());
}

#[cfg(unix)]
#[test]
fn session_export_rejects_symlinked_output_ancestors() {
    use std::os::unix::fs::symlink;

    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    let real_output = workspace.path().join("real-output");
    std::fs::create_dir(&real_output).expect("real output directory");
    let linked_output = workspace.path().join("linked-output");
    symlink(&real_output, &linked_output).expect("symlink output directory");
    Command::cargo_bin("mimir")
        .expect("binary")
        .args([
            "--workspace",
            workspace.path().to_str().expect("workspace path"),
            "--state-dir",
            state.path().to_str().expect("state path"),
            "--session",
            "safe-export",
            "session",
            "export",
            linked_output
                .join("session.jsonl")
                .to_str()
                .expect("export path"),
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "export path ancestor must not be a symlink",
        ));
    assert!(!real_output.join("session.jsonl").exists());
}
