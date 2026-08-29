#[path = "support/reliability.rs"]
#[allow(dead_code)]
mod reliability;

use std::{sync::Arc, time::Duration};

use mimir::{
    model::{Content, Message, StopReason, ToolCall},
    session::{FileSessionStore, SessionPayload, SessionRecord, SessionStore},
    tools::{ToolPolicy, ToolRegistry},
};
use reliability::{
    append_torn_json_tail, orphan_tool_call_ids, repeat_process, scan_for_literal_leaks,
};
use serde_json::json;
use tempfile::TempDir;

fn process_registry(root: &TempDir) -> Arc<ToolRegistry> {
    Arc::new(
        ToolRegistry::with_default_tools(
            root.path(),
            ToolPolicy {
                allow_process: true,
                allowed_programs: Some(vec!["true".into()]),
                command_timeout: Duration::from_secs(1),
                max_output_bytes: 1024,
                ..ToolPolicy::default()
            },
        )
        .expect("process fixture registry"),
    )
}

#[tokio::test]
async fn repeated_fast_processes_preserve_exit_detection() {
    let root = TempDir::new().expect("workspace");
    repeat_process(process_registry(&root), "true", 64)
        .await
        .expect("64 fast processes must not report false timeouts");
}

#[tokio::test]
#[ignore = "10,000-process release soak; run explicitly as documented in docs/RELIABILITY.md"]
async fn process_exit_and_pipe_close_soak_10000() {
    let root = TempDir::new().expect("workspace");
    repeat_process(process_registry(&root), "true", 10_000)
        .await
        .expect("10,000 fast processes must not report false timeouts");
}

#[tokio::test]
async fn torn_session_tail_is_recovered_without_losing_complete_records() {
    let root = TempDir::new().expect("state");
    let store = FileSessionStore::create(root.path(), "torn-tail")
        .await
        .expect("store");
    store
        .append(SessionRecord::new(SessionPayload::Message(Message::user(
            "durable",
        ))))
        .await
        .expect("append complete record");
    append_torn_json_tail(store.path());

    let loaded = store.load().await.expect("recover torn final record");
    assert!(loaded.recovered_incomplete_tail);
    assert_eq!(loaded.records.len(), 1);
    assert!(matches!(
        &loaded.records[0].payload,
        SessionPayload::Message(message) if message.text() == "durable"
    ));
}

#[test]
fn orphan_fixture_identifies_only_unmatched_tool_calls() {
    let records = vec![
        SessionRecord::new(SessionPayload::Message(Message::assistant(
            vec![
                Content::ToolCall(ToolCall {
                    id: "matched".into(),
                    name: "read_file".into(),
                    arguments: json!({"path": "README.md"}),
                }),
                Content::ToolCall(ToolCall {
                    id: "orphan".into(),
                    name: "read_file".into(),
                    arguments: json!({"path": "ARCHITECTURE.md"}),
                }),
            ],
            StopReason::ToolUse,
        ))),
        SessionRecord::new(SessionPayload::Message(Message::tool_result(
            "matched",
            "read_file",
            "ok",
            false,
        ))),
    ];

    assert_eq!(orphan_tool_call_ids(&records), ["orphan"]);
}

#[test]
fn diagnostic_export_fixture_has_no_secret_or_absolute_path_leaks() {
    let root = TempDir::new().expect("diagnostic bundle");
    let sensitive_marker = "RELIABILITY_SENTINEL_DO_NOT_EXPORT";
    let workspace_path = root.path().join("private-workspace");
    std::fs::write(
        root.path().join("summary.json"),
        r#"{"workspace":"$WORKSPACE","authorization":"[REDACTED]"}"#,
    )
    .expect("redacted fixture");
    let markers = [
        ("api_key", sensitive_marker),
        (
            "workspace_path",
            workspace_path.to_str().expect("UTF-8 path"),
        ),
    ];

    assert!(
        scan_for_literal_leaks(root.path(), &markers)
            .expect("scan redacted bundle")
            .is_empty()
    );

    std::fs::write(
        root.path().join("unsafe.json"),
        format!(
            "{{\"token\":\"{sensitive_marker}\",\"cwd\":\"{}\"}}",
            workspace_path.display()
        ),
    )
    .expect("unsafe control fixture");
    let findings = scan_for_literal_leaks(root.path(), &markers).expect("scan unsafe bundle");
    assert_eq!(findings.len(), 2);
    assert!(
        findings
            .iter()
            .all(|finding| finding.relative_path == std::path::Path::new("unsafe.json"))
    );
    assert_eq!(
        findings
            .iter()
            .map(|finding| finding.marker.as_str())
            .collect::<Vec<_>>(),
        ["api_key", "workspace_path"]
    );
}
