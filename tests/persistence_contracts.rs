use std::{fs::OpenOptions, io::Write, time::Duration};

use chrono::{Duration as ChronoDuration, Utc};
use mimir::{
    model::Message,
    orchestration::{
        GoalStatus, GoalStore, HeartbeatDeliveryMode, HeartbeatManagementAction, ScheduleKind,
        ScheduleSource, ScheduleStore,
    },
    resources::ResourceLoader,
    session::{FileSessionStore, SessionPayload, SessionRecord, SessionStore},
};
use tempfile::TempDir;

#[tokio::test]
async fn session_jsonl_round_trips_and_recovers_only_a_corrupt_tail() {
    let root = TempDir::new().expect("tempdir");
    let store = FileSessionStore::create(root.path(), "session-a")
        .await
        .expect("store");
    store
        .append(SessionRecord::new(SessionPayload::Message(Message::user(
            "one",
        ))))
        .await
        .expect("append one");
    store
        .append(SessionRecord::new(SessionPayload::Message(Message::user(
            "two",
        ))))
        .await
        .expect("append two");

    OpenOptions::new()
        .append(true)
        .open(store.path())
        .expect("open transcript")
        .write_all(b"{\"incomplete\":")
        .expect("write corrupt tail");

    let loaded = store.load().await.expect("tail should recover");
    assert_eq!(loaded.records.len(), 2);
    assert!(loaded.recovered_incomplete_tail);
}

#[tokio::test]
async fn session_jsonl_rejects_interior_corruption() {
    let root = TempDir::new().expect("tempdir");
    let store = FileSessionStore::create(root.path(), "session-b")
        .await
        .expect("store");
    let first = serde_json::to_string(&SessionRecord::new(SessionPayload::Message(Message::user(
        "valid one",
    ))))
    .expect("first record");
    let third = serde_json::to_string(&SessionRecord::new(SessionPayload::Message(Message::user(
        "valid three",
    ))))
    .expect("third record");
    std::fs::write(store.path(), format!("{first}\nnot-json\n{third}\n")).expect("fixture");

    let error = store
        .load()
        .await
        .expect_err("interior corruption must block");
    assert!(error.to_string().contains("line 2"));
}

#[tokio::test]
async fn compaction_checkpoints_prune_superseded_session_history() {
    let root = TempDir::new().expect("tempdir");
    let store = FileSessionStore::create(root.path(), "bounded")
        .await
        .expect("store");
    for index in 0..20 {
        store
            .append(SessionRecord::new(SessionPayload::Message(Message::user(
                format!("message-{index}"),
            ))))
            .await
            .expect("append message");
    }
    store
        .append(SessionRecord::new(SessionPayload::Compaction {
            summary: "checkpoint".into(),
            retained_message_count: 3,
            reason: Some("threshold".into()),
            first_kept_entry_id: None,
            tokens_before: 0,
            custom_instructions: None,
            details: None,
        }))
        .await
        .expect("append compaction");

    let on_disk = tokio::fs::read_to_string(store.path())
        .await
        .expect("transcript");
    assert_eq!(on_disk.lines().count(), 4);
    let loaded = store.load().await.expect("load checkpoint");
    assert_eq!(loaded.records.len(), 4);
    assert!(matches!(
        loaded.records[0].payload,
        SessionPayload::Compaction { .. }
    ));
    assert!(matches!(
        &loaded.records[3].payload,
        SessionPayload::Message(message) if message.text() == "message-19"
    ));
}

#[test]
fn resource_loader_applies_parent_to_child_context_and_nearest_skill_wins() {
    let root = TempDir::new().expect("tempdir");
    let project = root.path().join("project");
    let child = project.join("src");
    std::fs::create_dir_all(child.join(".agents/skills/review")).expect("child skill dir");
    std::fs::create_dir_all(project.join(".agents/skills/review")).expect("parent skill dir");
    std::fs::write(project.join("AGENTS.md"), "parent rule").expect("parent context");
    std::fs::write(child.join("AGENTS.md"), "child rule").expect("child context");
    std::fs::write(
        project.join(".agents/skills/review/SKILL.md"),
        "---\nname: review\ndescription: parent\n---\nparent body",
    )
    .expect("parent skill");
    std::fs::write(
        child.join(".agents/skills/review/SKILL.md"),
        "---\nname: review\ndescription: child\n---\nchild body",
    )
    .expect("child skill");

    let resources = ResourceLoader::new(&project, &child)
        .expect("loader")
        .load()
        .expect("resources");

    assert!(
        resources.system_context.find("parent rule") < resources.system_context.find("child rule")
    );
    assert_eq!(resources.skills.len(), 1);
    assert_eq!(resources.skills[0].description, "child");
    assert_eq!(
        resources.skills[0].path,
        std::fs::canonicalize(child.join(".agents/skills/review/SKILL.md"))
            .expect("canonical child skill")
    );
}

#[tokio::test]
async fn goals_and_schedules_persist_independently() {
    let root = TempDir::new().expect("tempdir");
    let goals = GoalStore::new(root.path());
    let schedules = ScheduleStore::new(root.path());

    let goal = goals
        .create("migrate the harness", Some(10_000))
        .await
        .expect("create goal");
    goals.record_tokens(500).await.expect("record token usage");
    let loaded_goal = goals.load().await.expect("load goal").expect("goal exists");
    assert_eq!(loaded_goal.id, goal.id);
    assert_eq!(loaded_goal.status, GoalStatus::Active);
    assert_eq!(loaded_goal.used_tokens, 500);

    let due_at = Utc::now() - ChronoDuration::seconds(1);
    let schedule = schedules
        .add(
            "heartbeat",
            "default",
            "continue the goal",
            due_at,
            Some(Duration::from_mins(1)),
        )
        .await
        .expect("add schedule");
    let due = schedules.due(Utc::now()).await.expect("load due schedules");
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].id, schedule.id);

    schedules.mark_run(schedule.id).await.expect("mark run");
    assert!(
        schedules
            .due(Utc::now())
            .await
            .expect("due after run")
            .is_empty()
    );
    assert!(goals.load().await.expect("goal remains").is_some());
}

#[tokio::test]
async fn textual_schedules_cover_once_interval_and_five_field_cron_contracts() {
    let root = TempDir::new().expect("tempdir");
    let schedules = ScheduleStore::new(root.path());
    let now = chrono::DateTime::parse_from_rfc3339("2026-08-07T12:00:00Z")
        .expect("fixed time")
        .with_timezone(&Utc);

    let interval = schedules
        .add_text("interval", "default", "repeat safely", "every 15m", now)
        .await
        .expect("interval schedule");
    assert_eq!(interval.schedule_kind, ScheduleKind::Interval);
    assert_eq!(interval.schedule_expression, "every 15m");
    assert_eq!(interval.every_seconds, Some(900));
    assert_eq!(interval.next_run, now + ChronoDuration::minutes(15));

    let once = schedules
        .add_text("once", "default", "run later", "in 2h", now)
        .await
        .expect("one-shot schedule");
    assert_eq!(once.schedule_kind, ScheduleKind::Once);
    assert_eq!(once.next_run, now + ChronoDuration::hours(2));

    let cron = schedules
        .add_text("cron", "default", "quarter-hour", "*/15 * * * *", now)
        .await
        .expect("cron schedule");
    assert_eq!(cron.schedule_kind, ScheduleKind::Cron);
    assert_eq!(cron.next_run, now + ChronoDuration::minutes(15));

    let too_fast = schedules
        .add_text("fast", "default", "reject", "every 9s", now)
        .await
        .expect_err("recurrence floor");
    assert!(too_fast.to_string().contains("at least 10 seconds"));
    let invalid = schedules
        .add_text("invalid", "default", "reject", "60 * * * *", now)
        .await
        .expect_err("invalid cron");
    assert!(invalid.to_string().contains("out of range"));
}

#[tokio::test]
async fn schedules_are_listed_and_dispatched_earliest_first() {
    let root = TempDir::new().expect("tempdir");
    let schedules = ScheduleStore::new(root.path());
    let now = Utc::now();
    let later = schedules
        .add(
            "later",
            "default",
            "second",
            now - ChronoDuration::seconds(1),
            None,
        )
        .await
        .expect("later insertion");
    let earlier = schedules
        .add(
            "earlier",
            "default",
            "first",
            now - ChronoDuration::seconds(2),
            None,
        )
        .await
        .expect("earlier insertion");

    let listed = schedules.list().await.expect("ordered list");
    assert_eq!(listed[0].id, earlier.id);
    assert_eq!(listed[1].id, later.id);
    let due = schedules.due(now).await.expect("ordered due list");
    assert_eq!(due[0].id, earlier.id);
    assert_eq!(due[1].id, later.id);
}

#[tokio::test]
async fn heartbeats_are_recurring_singletons_and_preserve_delivery_mode() {
    let root = TempDir::new().expect("tempdir");
    let schedules = ScheduleStore::new(root.path());
    let now = chrono::DateTime::parse_from_rfc3339("2026-08-07T12:00:00Z")
        .expect("fixed time")
        .with_timezone(&Utc);

    let once = schedules
        .set_heartbeat(
            "heartbeat",
            "session-a",
            "must recur",
            "in 5m",
            Some(HeartbeatDeliveryMode::Steer),
            now,
        )
        .await
        .expect_err("one-shot heartbeat");
    assert!(once.to_string().contains("must be recurring"));

    let first = schedules
        .set_heartbeat(
            "heartbeat",
            "session-a",
            "inspect the queue",
            "5m",
            Some(HeartbeatDeliveryMode::FollowUp),
            now,
        )
        .await
        .expect("first heartbeat");
    assert_eq!(first.source, ScheduleSource::Heartbeat);
    assert_eq!(first.schedule_kind, ScheduleKind::Interval);
    assert_eq!(first.every_seconds, Some(300));
    assert_eq!(first.delivery_mode, Some(HeartbeatDeliveryMode::FollowUp));

    let replacement = schedules
        .set_heartbeat(
            "heartbeat",
            "session-a",
            "inspect again",
            "every 10m",
            None,
            now + ChronoDuration::seconds(1),
        )
        .await
        .expect("replacement heartbeat");
    assert_ne!(replacement.id, first.id);
    assert_eq!(
        replacement.delivery_mode,
        Some(HeartbeatDeliveryMode::FollowUp),
        "an omitted mode inherits the previous heartbeat mode"
    );
    let all = schedules.list().await.expect("all schedules");
    let cancelled = all
        .iter()
        .find(|schedule| schedule.id == first.id)
        .expect("cancelled predecessor");
    assert!(cancelled.cancelled);
    assert!(!cancelled.enabled);

    let current = schedules
        .get_heartbeat("session-a")
        .await
        .expect("get heartbeat")
        .expect("current heartbeat");
    assert_eq!(current.id, replacement.id);
    assert_eq!(schedules.list_heartbeats().await.unwrap().len(), 1);
}

#[tokio::test]
async fn heartbeat_lifecycle_controls_are_session_scoped_and_durable() {
    let root = TempDir::new().expect("tempdir");
    let schedules = ScheduleStore::new(root.path());
    let now = chrono::DateTime::parse_from_rfc3339("2026-08-07T12:00:00Z")
        .expect("fixed time")
        .with_timezone(&Utc);
    let heartbeat = schedules
        .set_heartbeat(
            "heartbeat",
            "session-a",
            "inspect again",
            "every 10m",
            None,
            now,
        )
        .await
        .expect("heartbeat");

    let paused = schedules
        .pause_heartbeat("session-a", now + ChronoDuration::seconds(2))
        .await
        .expect("pause heartbeat")
        .expect("paused heartbeat");
    assert!(paused.paused);
    assert!(!paused.enabled);
    assert!(!paused.cancelled);
    assert!(schedules.due(paused.next_run).await.unwrap().is_empty());

    let wrong_session = schedules
        .manage_heartbeat(
            "session-b",
            heartbeat.id,
            HeartbeatManagementAction::Resume,
            now + ChronoDuration::seconds(3),
        )
        .await
        .expect("wrong-session management");
    assert!(wrong_session.is_none());

    let resumed = schedules
        .manage_heartbeat(
            "session-a",
            heartbeat.id,
            HeartbeatManagementAction::Resume,
            now + ChronoDuration::seconds(3),
        )
        .await
        .expect("resume heartbeat")
        .expect("resumed heartbeat");
    assert!(resumed.enabled);
    assert!(!resumed.paused);
    assert_eq!(
        resumed.next_run,
        now + ChronoDuration::minutes(10) + ChronoDuration::seconds(3)
    );

    let stopped = schedules
        .manage_heartbeat(
            "session-a",
            heartbeat.id,
            HeartbeatManagementAction::Stop,
            now + ChronoDuration::seconds(4),
        )
        .await
        .expect("stop heartbeat")
        .expect("stopped heartbeat");
    assert!(stopped.cancelled);
    assert!(!stopped.enabled);
    assert!(!stopped.paused);
    assert!(
        schedules
            .get_heartbeat("session-a")
            .await
            .unwrap()
            .is_none()
    );
    assert!(schedules.list_heartbeats().await.unwrap().is_empty());
}

#[tokio::test]
async fn concurrent_state_mutations_do_not_lose_updates() {
    let root = TempDir::new().expect("tempdir");
    let first = std::sync::Arc::new(GoalStore::new(root.path()));
    let second = std::sync::Arc::new(GoalStore::new(root.path()));
    first.create("count safely", None).await.expect("goal");
    let mut updates = Vec::new();
    for index in 0..20 {
        let store = if index % 2 == 0 {
            first.clone()
        } else {
            second.clone()
        };
        updates.push(tokio::spawn(async move { store.record_tokens(1).await }));
    }
    for update in updates {
        update.await.expect("task").expect("update");
    }
    assert_eq!(
        first.load().await.expect("load").expect("goal").used_tokens,
        20
    );

    let schedules_a = std::sync::Arc::new(ScheduleStore::new(root.path()));
    let schedules_b = std::sync::Arc::new(ScheduleStore::new(root.path()));
    let mut additions = Vec::new();
    for index in 0..20 {
        let store = if index % 2 == 0 {
            schedules_a.clone()
        } else {
            schedules_b.clone()
        };
        additions.push(tokio::spawn(async move {
            store
                .add(&format!("job-{index}"), "default", "run", Utc::now(), None)
                .await
        }));
    }
    for addition in additions {
        addition.await.expect("task").expect("add");
    }
    assert_eq!(schedules_a.list().await.expect("list").len(), 20);
}

#[cfg(unix)]
#[tokio::test]
async fn persistence_rejects_symlinked_state_components() {
    let root = TempDir::new().expect("tempdir");
    let outside = TempDir::new().expect("outside");
    std::os::unix::fs::symlink(outside.path(), root.path().join("goals")).expect("symlink");
    let goals = GoalStore::new(root.path());
    let error = goals
        .create("must stay contained", None)
        .await
        .expect_err("deny symlink");
    assert!(error.to_string().contains("symlink"));
    assert!(!outside.path().join("goal.json").exists());
}
