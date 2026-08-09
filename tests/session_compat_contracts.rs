use std::{path::Path, time::Duration};

use chrono::{TimeZone, Utc};
use mimir::{
    model::{Content, Message, StopReason, Usage},
    session::{SESSION_SCHEMA_VERSION, SessionPayload, SessionRecord},
    session_compat::{
        MAX_SHARE_PAYLOAD_BYTES, ReferenceSessionMetadata, SessionFormat, export_jsonl,
        import_jsonl, prepare_share_payload, prepare_switch_session,
    },
};
use serde_json::{Value, json};
use uuid::Uuid;

fn reference_header(id: &str) -> Value {
    json!({
        "type": "session",
        "version": 3,
        "id": id,
        "timestamp": "2026-08-07T12:00:00Z",
        "cwd": "/workspace"
    })
}

fn jsonl(values: &[Value]) -> String {
    let mut output = values
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    output.push('\n');
    output
}

fn rich_reference_session() -> String {
    jsonl(&[
        reference_header("legacy-session"),
        json!({
            "type":"message", "id":"aaaaaaaa", "parentId":null,
            "timestamp":"2026-08-07T12:00:01Z",
            "message":{"role":"user","content":[
                {"type":"text","text":"root question"},
                {"type":"image","data":"aGVsbG8=","mimeType":"image/png"}
            ],"timestamp":1_786_104_001_000_i64}
        }),
        json!({
            "type":"message", "id":"bbbbbbbb", "parentId":"aaaaaaaa",
            "timestamp":"2026-08-07T12:00:02Z",
            "message":{"role":"assistant","content":[{"type":"text","text":"abandoned"}],"stopReason":"stop"}
        }),
        json!({
            "type":"message", "id":"cccccccc", "parentId":"aaaaaaaa",
            "timestamp":"2026-08-07T12:00:03Z",
            "message":{"role":"assistant","content":[
                {"type":"thinking","thinking":"consider","signature":"signed-thought"},
                {"type":"redacted_thinking","data":"opaque-redacted-data"},
                {"type":"toolCall","id":"call-1","name":"read","arguments":{"path":"README.md"}}
            ],"stopReason":"toolUse"}
        }),
        json!({
            "type":"message", "id":"dddddddd", "parentId":"cccccccc",
            "timestamp":"2026-08-07T12:00:04Z",
            "message":{"role":"toolResult","toolCallId":"call-1","toolName":"read",
                "content":[{"type":"text","text":"contents"}],"isError":false,"timestamp":1_786_104_004_000_i64}
        }),
    ])
}

#[test]
fn imports_only_the_reference_active_branch_and_preserves_rich_messages() {
    let input = rich_reference_session();

    let imported = import_jsonl(Path::new("foreign.jsonl"), input.as_bytes()).expect("import");

    assert_eq!(imported.format, SessionFormat::Reference(3));
    assert_eq!(
        imported.metadata.as_ref().unwrap().session_id,
        "legacy-session"
    );
    assert_eq!(imported.records.len(), 3, "abandoned branch is excluded");
    assert_eq!(
        imported.records[0].payload,
        SessionPayload::Message(Message {
            role: mimir::model::Role::User,
            content: vec![
                Content::Text {
                    text: "root question".into()
                },
                Content::Image {
                    data: "aGVsbG8=".into(),
                    mime_type: "image/png".into(),
                },
            ],
            stop_reason: None,
            usage: Usage::default(),
            timestamp_ms: 1_786_104_001_000,
        })
    );
    let SessionPayload::Message(assistant) = &imported.records[1].payload else {
        panic!("assistant message");
    };
    assert_eq!(assistant.stop_reason, Some(StopReason::ToolUse));
    assert!(matches!(
        &assistant.content[0],
        Content::Thinking {
            text,
            signature: Some(signature),
            redacted: false,
        } if text == "consider" && signature == "signed-thought"
    ));
    assert!(matches!(
        &assistant.content[1],
        Content::Thinking {
            text,
            signature: Some(signature),
            redacted: true,
        } if text == "[redacted thinking]" && signature == "opaque-redacted-data"
    ));
    assert!(matches!(assistant.content[2], Content::ToolCall(_)));
    assert_eq!(
        imported.records[2].parent_id,
        Some(imported.records[1].record_id)
    );

    let reexported = export_jsonl(&imported.records, imported.metadata.as_ref().unwrap())
        .expect("re-export rich messages");
    let reexported = reexported
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(reexported[1]["message"]["content"][1]["type"], "image");
    assert_eq!(
        reexported[1]["message"]["content"][1]["mimeType"],
        "image/png"
    );
    assert_eq!(
        reexported[2]["message"]["content"][0]["signature"],
        "signed-thought"
    );
    assert_eq!(
        reexported[2]["message"]["content"][1]["type"],
        "redacted_thinking"
    );
    assert_eq!(
        reexported[2]["message"]["content"][1]["data"],
        "opaque-redacted-data"
    );
}

#[test]
fn import_computes_reference_compaction_retention_and_rejects_bad_graphs() {
    let input = jsonl(&[
        reference_header("compact-session"),
        json!({"type":"message","id":"aaaaaaaa","parentId":null,"message":{"role":"user","content":"old"}}),
        json!({"type":"message","id":"bbbbbbbb","parentId":"aaaaaaaa","message":{"role":"assistant","content":"keep one"}}),
        json!({"type":"message","id":"cccccccc","parentId":"bbbbbbbb","message":{"role":"user","content":"keep two"}}),
        json!({"type":"compaction","id":"dddddddd","parentId":"cccccccc","summary":"summary","firstKeptEntryId":"bbbbbbbb","tokensBefore":50000,"details":{"readFiles":["README.md"]}}),
    ]);
    let imported = import_jsonl(Path::new("compact.jsonl"), input.as_bytes()).expect("import");
    let SessionPayload::Compaction {
        retained_message_count,
        first_kept_entry_id,
        tokens_before,
        ..
    } = &imported.records[3].payload
    else {
        panic!("compaction payload");
    };
    assert_eq!(*retained_message_count, 2);
    let expected_first_kept_id = imported.records[1].record_id.to_string();
    assert_eq!(
        first_kept_entry_id.as_deref(),
        Some(expected_first_kept_id.as_str())
    );
    assert_eq!(*tokens_before, 50_000);

    let cyclic = jsonl(&[
        reference_header("cycle"),
        json!({"type":"message","id":"aaaaaaaa","parentId":"bbbbbbbb","message":{"role":"user","content":"a"}}),
        json!({"type":"message","id":"bbbbbbbb","parentId":"aaaaaaaa","message":{"role":"user","content":"b"}}),
    ]);
    assert!(import_jsonl(Path::new("cycle.jsonl"), cyclic.as_bytes()).is_err());
}

#[test]
fn exports_reference_v3_jsonl_with_linear_parents_and_round_trips() {
    let first_id = Uuid::parse_str("11111111-1111-4111-8111-111111111111").unwrap();
    let second_id = Uuid::parse_str("22222222-2222-4222-8222-222222222222").unwrap();
    let compact_id = Uuid::parse_str("33333333-3333-4333-8333-333333333333").unwrap();
    let timestamp = Utc.with_ymd_and_hms(2026, 8, 7, 12, 0, 0).unwrap();
    let records = vec![
        SessionRecord {
            schema_version: SESSION_SCHEMA_VERSION,
            record_id: first_id,
            parent_id: None,
            created_at: timestamp + chrono::Duration::seconds(1),
            payload: SessionPayload::Message(Message::user("hello")),
        },
        SessionRecord {
            schema_version: SESSION_SCHEMA_VERSION,
            record_id: second_id,
            parent_id: Some(first_id),
            created_at: timestamp + chrono::Duration::seconds(2),
            payload: SessionPayload::Message(Message::assistant(
                vec![Content::Text {
                    text: "answer".into(),
                }],
                StopReason::Stop,
            )),
        },
        SessionRecord {
            schema_version: SESSION_SCHEMA_VERSION,
            record_id: compact_id,
            parent_id: Some(second_id),
            created_at: timestamp + chrono::Duration::seconds(3),
            payload: SessionPayload::Compaction {
                summary: "summary".into(),
                retained_message_count: 1,
                reason: Some("manual".into()),
                first_kept_entry_id: Some(second_id.to_string()),
                tokens_before: 42_000,
                custom_instructions: Some("keep decisions".into()),
                details: Some(json!({"modifiedFiles":["src/lib.rs"]})),
            },
        },
    ];
    let metadata = ReferenceSessionMetadata {
        session_id: "rust-session".into(),
        timestamp,
        cwd: Path::new("/workspace").to_path_buf(),
        parent_session: Some(Path::new("/sessions/parent.jsonl").to_path_buf()),
    };

    let output = export_jsonl(&records, &metadata).expect("export");
    assert!(output.ends_with('\n'));
    let values = output
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(values[0]["type"], "session");
    assert_eq!(values[0]["version"], 3);
    assert_eq!(values[0]["parentSession"], "/sessions/parent.jsonl");
    assert_eq!(values[1]["id"], "11111111");
    assert_eq!(values[2]["parentId"], "11111111");
    assert_eq!(values[2]["message"]["role"], "assistant");
    assert_eq!(values[2]["message"]["content"][0]["type"], "text");
    assert_eq!(values[3]["firstKeptEntryId"], "22222222");
    assert_eq!(values[3]["reason"], "manual");

    let imported =
        import_jsonl(Path::new("roundtrip.jsonl"), output.as_bytes()).expect("roundtrip");
    assert_eq!(imported.records.len(), records.len());
    assert_eq!(imported.records[0].payload, records[0].payload);
    assert_eq!(imported.records[1].payload, records[1].payload);
}

#[test]
fn switch_plan_detects_native_rust_and_uses_safe_target_and_cwd_override() {
    let record = SessionRecord::new(SessionPayload::Message(Message::user("native")));
    let native = format!("{}\n", serde_json::to_string(&record).unwrap());
    let cwd = Path::new("/override");
    let plan = prepare_switch_session(
        Path::new("foreign.session.jsonl"),
        native.as_bytes(),
        Some(cwd),
    )
    .expect("switch plan");

    assert_eq!(plan.format, SessionFormat::Rust(SESSION_SCHEMA_VERSION));
    assert_eq!(plan.target_session_id, "foreign-session");
    assert_eq!(plan.cwd, cwd);
    assert_eq!(plan.records, vec![record]);

    assert!(
        prepare_switch_session(Path::new("native.jsonl"), native.as_bytes(), None).is_err(),
        "native sessions require an explicit cwd"
    );
}

#[test]
fn switch_plan_uses_reference_cwd_and_bounds_hostile_input() {
    let input = jsonl(&[
        reference_header("reference-id"),
        json!({"type":"message","id":"aaaaaaaa","parentId":null,"message":{"role":"user","content":"hello"}}),
    ]);
    let plan = prepare_switch_session(Path::new("../bad name.jsonl"), input.as_bytes(), None)
        .expect("switch plan");
    assert_eq!(plan.target_session_id, "bad-name");
    assert_eq!(plan.cwd, Path::new("/workspace"));

    let unsupported = jsonl(&[json!({
        "type":"session", "version":99, "id":"future", "timestamp":"2026-08-07T12:00:00Z", "cwd":"/workspace"
    })]);
    assert!(import_jsonl(Path::new("future.jsonl"), unsupported.as_bytes()).is_err());

    let huge_line = vec![b'x'; 4 * 1024 * 1024 + 1];
    assert!(import_jsonl(Path::new("huge.jsonl"), &huge_line).is_err());
}

#[test]
fn share_payload_is_fixed_name_bounded_and_builds_safe_fragment_urls() {
    let payload = prepare_share_payload(b"<!doctype html><p>safe</p>", None).expect("payload");
    assert_eq!(payload.filename, "session.html");
    assert_eq!(payload.content_type, "text/html; charset=utf-8");
    assert_eq!(payload.bytes, b"<!doctype html><p>safe</p>");
    assert_eq!(
        payload.viewer_url("0123456789abcdef").expect("viewer URL"),
        "https://pi.dev/session/#0123456789abcdef"
    );

    let custom = prepare_share_payload(b"<p>safe</p>", Some("https://viewer.example/sessions"))
        .expect("custom payload");
    assert_eq!(
        custom.viewer_url("abcdef123456").expect("custom URL"),
        "https://viewer.example/sessions/#abcdef123456"
    );
    assert!(custom.viewer_url("../../token").is_err());
    assert!(prepare_share_payload(b"safe", Some("http://insecure.example/")).is_err());
    assert!(prepare_share_payload(&[0xff], None).is_err());
    assert!(prepare_share_payload(&vec![b'x'; MAX_SHARE_PAYLOAD_BYTES + 1], None).is_err());
}

#[test]
fn import_and_export_are_deterministic_for_identical_inputs() {
    let input = jsonl(&[
        reference_header("stable"),
        json!({"type":"message","id":"aaaaaaaa","parentId":null,"message":{"role":"user","content":"hello"}}),
    ]);
    let first = import_jsonl(Path::new("stable.jsonl"), input.as_bytes()).expect("first");
    std::thread::sleep(Duration::from_millis(2));
    let second = import_jsonl(Path::new("stable.jsonl"), input.as_bytes()).expect("second");
    assert_eq!(first.records, second.records);
}

#[test]
fn import_preserves_reference_branch_settings_and_context_entries() {
    let input = jsonl(&[
        reference_header("stateful"),
        json!({"type":"thinking_level_change","id":"aaaaaaaa","parentId":null,"thinkingLevel":"high"}),
        json!({"type":"service_tier_change","id":"bbbbbbbb","parentId":"aaaaaaaa","serviceTier":"priority"}),
        json!({"type":"model_change","id":"cccccccc","parentId":"bbbbbbbb","provider":"anthropic","modelId":"claude-sonnet-4-5"}),
        json!({"type":"custom_message","id":"dddddddd","parentId":"cccccccc","customType":"extension-note","content":"Injected context","display":false}),
        json!({"type":"branch_summary","id":"eeeeeeee","parentId":"dddddddd","fromId":"old-leaf","summary":"A discarded branch found the answer."}),
    ]);

    let imported = import_jsonl(Path::new("stateful.jsonl"), input.as_bytes()).expect("import");

    assert_eq!(imported.state.thinking_level.as_deref(), Some("high"));
    assert_eq!(imported.state.service_tier.as_deref(), Some("priority"));
    let model = imported.state.model.as_ref().expect("model selection");
    assert_eq!(model.provider, "anthropic");
    assert_eq!(model.model, "claude-sonnet-4-5");
    let context_messages = imported
        .records
        .iter()
        .filter_map(|record| match &record.payload {
            SessionPayload::Message(message) => Some(message),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(context_messages.len(), 2);
    assert_eq!(context_messages[0].role, mimir::model::Role::User);
    assert_eq!(
        context_messages[0].content,
        vec![Content::Text {
            text: "Injected context".into()
        }]
    );
    assert!(matches!(
        &context_messages[1].content[0],
        Content::Text { text } if text.contains("A discarded branch found the answer.")
    ));
}

#[test]
fn import_translates_reference_bash_and_custom_messages_without_fake_tools() {
    let input = jsonl(&[
        reference_header("message-roles"),
        json!({"type":"message","id":"aaaaaaaa","parentId":null,"message":{
            "role":"bashExecution","command":"printf hi","output":"hi","exitCode":0,
            "cancelled":false,"truncated":false,"timestamp":1_786_104_001_000_i64
        }}),
        json!({"type":"message","id":"bbbbbbbb","parentId":"aaaaaaaa","message":{
            "role":"custom","customType":"extension-note","content":[
                {"type":"text","text":"remember this"},
                {"type":"image","data":"aW1hZ2U=","mimeType":"image/png"}
            ],"display":true,"timestamp":1_786_104_002_000_i64
        }}),
    ]);

    let imported = import_jsonl(Path::new("roles.jsonl"), input.as_bytes()).expect("import");
    let messages = imported
        .records
        .iter()
        .map(|record| match &record.payload {
            SessionPayload::Message(message) => message,
            other => panic!("expected translated message, got {other:?}"),
        })
        .collect::<Vec<_>>();
    assert!(
        messages
            .iter()
            .all(|message| message.role == mimir::model::Role::User)
    );
    assert_eq!(
        messages[0].content,
        vec![Content::Text {
            text: "Ran `printf hi`\n```\nhi\n```".into()
        }]
    );
    assert_eq!(messages[1].content.len(), 2);
    assert!(matches!(messages[1].content[1], Content::Image { .. }));
}

#[test]
fn export_resolves_reference_id_prefix_collisions_without_losing_records() {
    let timestamp = Utc.with_ymd_and_hms(2026, 8, 7, 12, 0, 0).unwrap();
    let records = vec![
        SessionRecord {
            schema_version: SESSION_SCHEMA_VERSION,
            record_id: Uuid::parse_str("aaaaaaaa-1111-4111-8111-111111111111").unwrap(),
            parent_id: None,
            created_at: timestamp,
            payload: SessionPayload::Message(Message::user("one")),
        },
        SessionRecord {
            schema_version: SESSION_SCHEMA_VERSION,
            record_id: Uuid::parse_str("aaaaaaaa-2222-4222-8222-222222222222").unwrap(),
            parent_id: None,
            created_at: timestamp,
            payload: SessionPayload::Message(Message::user("two")),
        },
    ];
    let output = export_jsonl(
        &records,
        &ReferenceSessionMetadata {
            session_id: "collision".into(),
            timestamp,
            cwd: Path::new("/workspace").to_path_buf(),
            parent_session: None,
        },
    )
    .expect("collision-safe export");
    let values = output
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_ne!(values[1]["id"], values[2]["id"]);
    assert_eq!(values[1]["id"].as_str().unwrap().len(), 8);
    assert_eq!(values[2]["id"].as_str().unwrap().len(), 8);
}
