use mimir::daemon::{
    PUBLIC_DAEMON_PROTOCOL_NAME, PUBLIC_DAEMON_SNAPSHOT_CHUNK_BYTES, PublicDaemonCommand,
    PublicDaemonCommandEnvelope, PublicDaemonEventCursor, PublicDaemonSessionSnapshot,
    PublicDaemonSnapshotRecord, PublicImageContent,
};
use serde_json::json;

#[test]
fn public_v4_command_envelopes_round_trip_without_losing_compatible_fields() {
    let wire = json!({
        "type": "command",
        "id": "command-7",
        "protocol": {"name": "mimir.daemon", "version": 4},
        "clientId": "client-a",
        "command": {
            "type": "attach",
            "activeSessionId": "active-a",
            "supportsExtensionUi": true,
            "capabilities": ["attach_snapshot", "event_sequence"],
            "resumeCursor": {"generation": "generation-a", "sequence": 41},
            "futureCompatibleField": {"preserved": true}
        }
    });

    let envelope: PublicDaemonCommandEnvelope =
        serde_json::from_value(wire.clone()).expect("public v4 envelope");
    envelope.validate().expect("valid v4 envelope");
    assert_eq!(envelope.command.command_type(), "attach");
    assert_eq!(
        envelope.command.field("futureCompatibleField"),
        Some(&json!({"preserved": true}))
    );
    assert_eq!(serde_json::to_value(envelope).unwrap(), wire);
}

#[test]
fn prompt_commands_preserve_reference_image_content() {
    let image = PublicImageContent::new("aGVsbG8=", "image/png").expect("image block");
    let command =
        PublicDaemonCommand::prompt("active-a", "inspect", vec![image]).expect("prompt command");
    let envelope =
        PublicDaemonCommandEnvelope::new("prompt-1", None, 7, command).expect("prompt envelope");

    assert_eq!(
        serde_json::to_value(envelope).unwrap(),
        json!({
            "type": "command",
            "id": "prompt-1",
            "protocol": {"name": PUBLIC_DAEMON_PROTOCOL_NAME, "version": 7},
            "command": {
                "type": "prompt",
                "activeSessionId": "active-a",
                "message": "inspect",
                "images": [{"type": "image", "data": "aGVsbG8=", "mimeType": "image/png"}]
            }
        })
    );
}

#[test]
fn public_commands_fail_closed_for_unknown_private_discriminators() {
    let error = serde_json::from_value::<PublicDaemonCommand>(json!({
        "type": "private_supervisor_escape",
        "path": "/tmp/private"
    }))
    .expect_err("unknown commands must be rejected");
    assert!(error.to_string().contains("unknown public daemon command"));

    let wrong_name = serde_json::from_value::<PublicDaemonCommandEnvelope>(json!({
        "type": "command",
        "id": "x",
        "protocol": {"name": "other.daemon", "version": 4},
        "command": {"type": "list"}
    }))
    .expect_err("invalid protocol names fail during decoding");
    assert!(
        wrong_name
            .to_string()
            .contains("unsupported daemon protocol name")
    );
}

#[test]
fn coherent_snapshots_and_chunk_records_match_the_public_camel_case_wire() {
    let cursor = PublicDaemonEventCursor {
        generation: "generation-a".into(),
        sequence: 12,
    };
    let snapshot = PublicDaemonSessionSnapshot {
        active_session_id: "active-a".into(),
        summary: json!({"sessionId": "session-a"}),
        state: json!({"isStreaming": false}),
        messages: vec![json!({"role": "user", "content": "hello"})],
        session_context: None,
        session_tree: Some(json!({"tree": [], "leafId": null})),
        last_event_sequence: 12,
        last_event_cursor: Some(cursor.clone()),
        parent: None,
        children: Vec::new(),
    };
    let encoded = serde_json::to_value(&snapshot).unwrap();
    assert_eq!(encoded["activeSessionId"], "active-a");
    assert_eq!(encoded["lastEventCursor"]["generation"], "generation-a");
    assert!(encoded.get("sessionContext").is_none());

    let begin = PublicDaemonSnapshotRecord::Begin {
        active_session_id: "active-a".into(),
        snapshot_id: "snapshot-a".into(),
        snapshot: encoded,
        message_count: 1,
        target_chunk_bytes: PUBLIC_DAEMON_SNAPSHOT_CHUNK_BYTES,
        purpose: Some("attach".into()),
    };
    let end = PublicDaemonSnapshotRecord::End {
        active_session_id: "active-a".into(),
        snapshot_id: "snapshot-a".into(),
        chunk_count: 1,
        last_event_sequence: 12,
        last_event_cursor: Some(cursor),
    };
    assert_eq!(
        serde_json::to_value(begin).unwrap()["type"],
        "session_snapshot_begin"
    );
    assert_eq!(
        serde_json::to_value(end).unwrap()["type"],
        "session_snapshot_end"
    );
}
