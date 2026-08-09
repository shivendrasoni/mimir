use std::time::Duration;

use mimir::{
    model::{Content, Message, StopReason},
    observation::{MAX_POLL_INTERVAL, MIN_POLL_INTERVAL, SessionObserver},
    session::{FileSessionStore, SessionPayload, SessionRecord, SessionStore},
};
use serde_json::json;
use tempfile::TempDir;

async fn append_message(store: &FileSessionStore, message: Message) {
    store
        .append(SessionRecord::new(SessionPayload::Message(message)))
        .await
        .expect("append message");
}

fn assistant_message(text: &str) -> Message {
    Message::assistant(vec![Content::Text { text: text.into() }], StopReason::Stop)
}

#[tokio::test]
async fn initial_messages_are_compaction_aware() {
    let state = TempDir::new().expect("state");
    let store = FileSessionStore::create(state.path(), "target")
        .await
        .expect("session store");
    append_message(&store, Message::user("superseded question")).await;
    append_message(&store, assistant_message("superseded answer")).await;
    append_message(&store, Message::user("retained question")).await;
    append_message(&store, assistant_message("retained answer")).await;
    store
        .append(SessionRecord::new(SessionPayload::Compaction {
            summary: "durable summary".into(),
            retained_message_count: 2,
            reason: Some("manual".into()),
            first_kept_entry_id: None,
            tokens_before: 20_001,
            custom_instructions: None,
            details: None,
        }))
        .await
        .expect("append compaction");

    let observer = SessionObserver::new(state.path());
    let mut observation = observer.start("target").await.expect("start observation");

    assert_eq!(observation.messages().len(), 3);
    assert_eq!(observation.messages()[0].text(), "durable summary");
    assert_eq!(observation.messages()[1].text(), "retained question");
    assert_eq!(observation.messages()[2].text(), "retained answer");
    observation.stop().await.expect("stop observation");
}

#[tokio::test]
async fn watcher_forwards_only_new_persisted_messages_with_reference_shape() {
    let state = TempDir::new().expect("state");
    let store = FileSessionStore::create(state.path(), "target")
        .await
        .expect("session store");
    append_message(&store, Message::user("already present")).await;

    let observer = SessionObserver::new(state.path()).with_poll_interval(Duration::from_millis(10));
    let mut observation = observer.start("target").await.expect("start observation");
    store
        .append(SessionRecord::new(SessionPayload::RuntimeEvent {
            name: "ignored".into(),
            detail: "not a message".into(),
        }))
        .await
        .expect("append runtime event");
    let new_message = Message::user("newly persisted");
    append_message(&store, new_message.clone()).await;

    let event = tokio::time::timeout(Duration::from_secs(1), observation.next_event())
        .await
        .expect("event timeout")
        .expect("event");
    let value = serde_json::to_value(event).expect("serialize event");
    assert_eq!(
        value,
        json!({
            "type": "observed_session_event",
            "activeSessionId": "target",
            "event": {
                "type": "message_end",
                "message": new_message
            }
        })
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(50), observation.next_event())
            .await
            .is_err(),
        "existing messages and runtime events must not be replayed"
    );
    observation.stop().await.expect("stop observation");
}

#[tokio::test]
async fn live_compaction_emits_resync_event_without_replaying_retained_messages() {
    let state = TempDir::new().expect("state");
    let store = FileSessionStore::create(state.path(), "target")
        .await
        .expect("session store");
    append_message(&store, Message::user("old question")).await;
    append_message(&store, assistant_message("old answer")).await;
    append_message(&store, Message::user("retained question")).await;
    append_message(&store, assistant_message("retained answer")).await;
    let observer = SessionObserver::new(state.path()).with_poll_interval(Duration::from_millis(10));
    let mut observation = observer.start("target").await.expect("start observation");

    store
        .append(SessionRecord::new(SessionPayload::Compaction {
            summary: "live summary".into(),
            retained_message_count: 2,
            reason: Some("requested".into()),
            first_kept_entry_id: Some("first-retained".into()),
            tokens_before: 23_000,
            custom_instructions: Some("preserve decisions".into()),
            details: Some(json!({"readFiles": ["README.md"]})),
        }))
        .await
        .expect("append compaction");

    let compacted = tokio::time::timeout(Duration::from_secs(1), observation.next_event())
        .await
        .expect("compaction timeout")
        .expect("compaction event");
    assert_eq!(
        serde_json::to_value(compacted).expect("serialize compaction"),
        json!({
            "type": "observed_session_event",
            "activeSessionId": "target",
            "event": {
                "type": "compaction_end",
                "reason": "requested",
                "result": {
                    "summary": "live summary",
                    "firstKeptEntryId": "first-retained",
                    "tokensBefore": 23_000,
                    "details": {"readFiles": ["README.md"]}
                },
                "aborted": false,
                "willRetry": false,
                "customInstructions": "preserve decisions"
            }
        })
    );

    let after_compaction = Message::user("after compaction");
    append_message(&store, after_compaction.clone()).await;
    let message = tokio::time::timeout(Duration::from_secs(1), observation.next_event())
        .await
        .expect("message timeout")
        .expect("message event");
    assert_eq!(
        serde_json::to_value(message).expect("serialize message"),
        json!({
            "type": "observed_session_event",
            "activeSessionId": "target",
            "event": {"type": "message_end", "message": after_compaction}
        })
    );
    observation.stop().await.expect("stop observation");
}

#[tokio::test]
async fn watcher_reports_session_file_disappearance_and_closes() {
    let state = TempDir::new().expect("state");
    let store = FileSessionStore::create(state.path(), "target")
        .await
        .expect("session store");
    append_message(&store, Message::user("present")).await;

    let observer = SessionObserver::new(state.path()).with_poll_interval(Duration::from_millis(10));
    let mut observation = observer.start("target").await.expect("start observation");
    tokio::fs::remove_file(store.path())
        .await
        .expect("remove target session");

    let event = tokio::time::timeout(Duration::from_secs(1), observation.next_event())
        .await
        .expect("closure timeout")
        .expect("closure event");
    assert_eq!(
        serde_json::to_value(event).expect("serialize event"),
        json!({
            "type": "observed_session_closed",
            "activeSessionId": "target"
        })
    );
    assert!(observation.next_event().await.is_none());
    observation.stop().await.expect("stop observation");
}

#[tokio::test]
async fn stopping_is_idempotent_and_ends_the_event_stream() {
    let state = TempDir::new().expect("state");
    let store = FileSessionStore::create(state.path(), "target")
        .await
        .expect("session store");
    append_message(&store, Message::user("present")).await;

    let observer = SessionObserver::new(state.path()).with_poll_interval(Duration::from_millis(10));
    let mut observation = observer.start("target").await.expect("start observation");
    observation.stop().await.expect("first stop");
    observation.stop().await.expect("second stop");
    append_message(&store, Message::user("after stop")).await;

    assert!(observation.next_event().await.is_none());
}

#[tokio::test]
async fn invalid_missing_and_symlinked_sessions_are_rejected() {
    let state = TempDir::new().expect("state");
    let observer = SessionObserver::new(state.path());
    assert!(observer.start("../escape").await.is_err());
    assert!(observer.start("missing").await.is_err());

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;

        let outside = TempDir::new().expect("outside");
        let outside_store = FileSessionStore::create(outside.path(), "outside")
            .await
            .expect("outside store");
        append_message(&outside_store, Message::user("outside")).await;
        tokio::fs::create_dir_all(state.path().join("sessions"))
            .await
            .expect("sessions directory");
        symlink(
            outside_store.path(),
            state.path().join("sessions/link.jsonl"),
        )
        .expect("session symlink");

        assert!(observer.start("link").await.is_err());
    }
}

#[test]
fn polling_interval_is_bounded() {
    let state = TempDir::new().expect("state");
    assert_eq!(
        SessionObserver::new(state.path())
            .with_poll_interval(Duration::ZERO)
            .poll_interval(),
        MIN_POLL_INTERVAL
    );
    assert_eq!(
        SessionObserver::new(state.path())
            .with_poll_interval(Duration::from_mins(1))
            .poll_interval(),
        MAX_POLL_INTERVAL
    );
}
