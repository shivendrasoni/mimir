use std::{fs, path::Path, process::Command};

use serde_json::Value;
use tempfile::TempDir;

fn binary() -> Command {
    Command::new(assert_cmd::cargo::cargo_bin!("mimir"))
}

fn run_id_from_list(state: &Path) -> String {
    let output = binary()
        .args([
            "--state-dir",
            state.to_str().expect("state"),
            "diagnose",
            "list",
        ])
        .output()
        .expect("diagnose list");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let list: Value = serde_json::from_slice(&output.stdout).expect("list json");
    list["runs"][0]["run_id"]
        .as_str()
        .expect("run id")
        .to_owned()
}

#[test]
fn a_run_creates_a_queryable_privacy_safe_bundle() {
    let workspace = TempDir::new().expect("workspace");
    let state = workspace.path().join("state");
    let prompt = "prompt-private-diagnostic-marker";
    let response = "response-private-diagnostic-marker";
    let sensitive_marker = "diagnostic-sensitive-marker-123456789";
    let output = binary()
        .args([
            "--provider",
            "fake",
            "--workspace",
            workspace.path().to_str().expect("workspace"),
            "--state-dir",
            state.to_str().expect("state"),
            "--api-key",
            sensitive_marker,
            "--fake-response",
            response,
            "--print",
            prompt,
        ])
        .output()
        .expect("run");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let run_id = run_id_from_list(&state);
    let directory = state.join("diagnostics/runs").join(&run_id);
    for relative in [
        "manifest.json",
        "events.jsonl",
        "summary.json",
        "analysis.jsonl",
    ] {
        assert!(directory.join(relative).is_file(), "missing {relative}");
    }
    assert!(directory.join("artifacts").is_dir());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(state.join("diagnostics"))
                .expect("diagnostics metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(directory.join("manifest.json"))
                .expect("manifest metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    let show = binary()
        .args([
            "--state-dir",
            state.to_str().expect("state"),
            "diagnose",
            "show",
            &run_id,
        ])
        .output()
        .expect("diagnose show");
    assert!(
        show.status.success(),
        "{}",
        String::from_utf8_lossy(&show.stderr)
    );
    let bundle: Value = serde_json::from_slice(&show.stdout).expect("bundle json");
    assert_eq!(bundle["manifest"]["workspace"], "$WORKSPACE");
    assert_eq!(bundle["summary"]["outcome"], "completed");
    assert_eq!(bundle["redacted"], true);
    let serialized = String::from_utf8(show.stdout).expect("utf8 bundle");
    assert!(!serialized.contains(prompt));
    assert!(!serialized.contains(response));
    assert!(!serialized.contains(sensitive_marker));
    assert!(!serialized.contains(workspace.path().to_str().expect("workspace")));

    let query = binary()
        .args([
            "--state-dir",
            state.to_str().expect("state"),
            "diagnose",
            "query",
            &run_id,
            "--kind",
            "provider_request",
            "--json",
        ])
        .output()
        .expect("diagnose query");
    assert!(query.status.success());
    let query: Value = serde_json::from_slice(&query.stdout).expect("query json");
    assert_eq!(query["events"].as_array().expect("events").len(), 1);

    let export_path = workspace.path().join("portable-diagnostic.json");
    let export = binary()
        .args([
            "--state-dir",
            state.to_str().expect("state"),
            "diagnose",
            "export",
            &run_id,
            "--redacted",
            "--output",
            export_path.to_str().expect("export path"),
        ])
        .output()
        .expect("diagnose export");
    assert!(
        export.status.success(),
        "{}",
        String::from_utf8_lossy(&export.stderr)
    );
    let exported = fs::read_to_string(export_path).expect("exported bundle");
    assert!(!exported.contains(prompt));
    assert!(!exported.contains(response));
    assert!(!exported.contains(workspace.path().to_str().expect("workspace")));

    let replay = binary()
        .args([
            "--state-dir",
            state.to_str().expect("state"),
            "diagnose",
            "replay",
            &run_id,
        ])
        .output()
        .expect("diagnose replay");
    let replay: Value = serde_json::from_slice(&replay.stdout).expect("replay json");
    assert_eq!(replay["mode"], "verification_only");
    assert_eq!(replay["executed_provider_requests"], false);
    assert_eq!(replay["executed_tools"], false);
}

#[test]
fn annotation_is_appended_and_sensitive_literals_are_redacted() {
    let workspace = TempDir::new().expect("workspace");
    let state = workspace.path().join("state");
    let output = binary()
        .args([
            "--provider",
            "fake",
            "--workspace",
            workspace.path().to_str().expect("workspace"),
            "--state-dir",
            state.to_str().expect("state"),
            "--fake-response",
            "ok",
            "--print",
            "hello",
        ])
        .output()
        .expect("run");
    assert!(output.status.success());
    let run_id = run_id_from_list(&state);
    let annotation = workspace.path().join("analysis.json");
    fs::write(
        &annotation,
        r#"{
            "author":"claude",
            "finding":"Inspect /Users/example/private/log.txt with token=secret-value",
            "confidence":0.7,
            "evidence_event_ids":[],
            "proposed_fix":"Never copy credential-like values",
            "verification":"offline"
        }"#,
    )
    .expect("write annotation");
    let output = binary()
        .args([
            "--state-dir",
            state.to_str().expect("state"),
            "diagnose",
            "annotate",
            &run_id,
            "--file",
            annotation.to_str().expect("annotation"),
        ])
        .output()
        .expect("annotate");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stored = fs::read_to_string(
        state
            .join("diagnostics/runs")
            .join(run_id)
            .join("analysis.jsonl"),
    )
    .expect("stored analysis");
    assert!(stored.contains("$ABSOLUTE_PATH"));
    assert!(stored.contains("$REDACTED"));
    assert!(!stored.contains("/Users/example"));
    assert!(!stored.contains("secret-value"));
}
