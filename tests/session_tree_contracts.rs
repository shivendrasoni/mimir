use chrono::{TimeZone as _, Utc};
use mimir::{
    model::{Content, Message, Role, StopReason},
    session::{SESSION_SCHEMA_VERSION, SessionPayload, SessionRecord},
    session_tree::{
        MAX_BRANCH_SELECTOR_CHARS, MAX_SESSION_NAME_CHARS, SessionBranchCatalog, SessionNodeKind,
    },
};
use uuid::Uuid;

fn record(id: u128, parent: Option<u128>, second: i64, payload: SessionPayload) -> SessionRecord {
    SessionRecord {
        schema_version: SESSION_SCHEMA_VERSION,
        record_id: Uuid::from_u128(id),
        parent_id: parent.map(Uuid::from_u128),
        created_at: Utc.timestamp_opt(second, 0).single().expect("timestamp"),
        payload,
    }
}

fn user(id: u128, parent: Option<u128>, second: i64, text: &str) -> SessionRecord {
    record(
        id,
        parent,
        second,
        SessionPayload::Message(Message::user(text)),
    )
}

fn assistant(id: u128, parent: Option<u128>, second: i64, text: &str) -> SessionRecord {
    record(
        id,
        parent,
        second,
        SessionPayload::Message(Message::assistant(
            vec![Content::Text { text: text.into() }],
            StopReason::Stop,
        )),
    )
}

#[test]
fn catalog_builds_a_stable_parent_aware_tree_and_user_selectors() {
    let long_prompt = "x".repeat(MAX_BRANCH_SELECTOR_CHARS + 7);
    let records = vec![
        user(1, None, 1, "root"),
        assistant(2, Some(1), 2, "answer"),
        user(4, Some(2), 4, "later branch"),
        user(3, Some(2), 3, &long_prompt),
    ];

    let catalog = SessionBranchCatalog::from_records(records).expect("catalog");
    assert_eq!(catalog.roots(), &[Uuid::from_u128(1)]);
    assert_eq!(catalog.leaf_id(), Some(Uuid::from_u128(3)));
    assert_eq!(catalog.nodes()[0].record_id, Uuid::from_u128(1));
    assert_eq!(catalog.nodes()[0].children, vec![Uuid::from_u128(2)]);
    assert_eq!(
        catalog
            .node(Uuid::from_u128(2))
            .expect("assistant")
            .children,
        vec![Uuid::from_u128(3), Uuid::from_u128(4)]
    );
    assert_eq!(
        catalog.node(Uuid::from_u128(2)).expect("assistant").kind,
        SessionNodeKind::Message {
            role: Role::Assistant
        }
    );

    let selectors = catalog.user_message_selectors();
    assert_eq!(selectors.len(), 3);
    assert_eq!(selectors[0].text, "root");
    assert_eq!(selectors[1].entry_id, Uuid::from_u128(4));
    assert!(!selectors[1].truncated);
    assert_eq!(selectors[2].text.chars().count(), MAX_BRANCH_SELECTOR_CHARS);
    assert!(selectors[2].truncated);
}

#[test]
fn native_jsonl_without_parent_links_is_interpreted_as_one_append_only_branch() {
    let records = [
        user(11, None, 1, "one"),
        assistant(12, None, 2, "answer"),
        user(13, None, 3, "two"),
    ];
    let jsonl = records
        .iter()
        .map(|record| serde_json::to_string(record).expect("record JSON"))
        .collect::<Vec<_>>()
        .join("\n");

    let catalog = SessionBranchCatalog::from_jsonl(&jsonl).expect("native JSONL");
    assert_eq!(
        catalog.node(Uuid::from_u128(12)).expect("second").parent_id,
        Some(Uuid::from_u128(11))
    );
    assert_eq!(
        catalog.node(Uuid::from_u128(13)).expect("third").parent_id,
        Some(Uuid::from_u128(12))
    );
    assert_eq!(catalog.leaf_id(), Some(Uuid::from_u128(13)));
}

#[test]
fn fork_before_user_message_copies_only_its_ancestor_path_and_returns_full_text() {
    let records = vec![
        user(21, None, 1, "first"),
        assistant(22, Some(21), 2, "answer"),
        user(23, Some(22), 3, "selected prompt"),
        assistant(24, Some(23), 4, "abandoned"),
        user(25, Some(22), 5, "other branch"),
    ];
    let catalog = SessionBranchCatalog::from_records(records).expect("catalog");

    let fork = catalog
        .fork_before_user_message(Uuid::from_u128(23), "source-session")
        .expect("fork plan");

    assert_eq!(fork.selected_text.as_deref(), Some("selected prompt"));
    assert_eq!(fork.records.len(), 3);
    assert_eq!(fork.records[0].record_id, Uuid::from_u128(21));
    assert_eq!(fork.records[1].record_id, Uuid::from_u128(22));
    let lineage = fork.records.last().expect("lineage event");
    assert_eq!(lineage.parent_id, Some(Uuid::from_u128(22)));
    assert!(matches!(
        &lineage.payload,
        SessionPayload::RuntimeEvent { name, detail }
            if name == "session_forked_from"
                && detail == &format!("source-session:{}", Uuid::from_u128(23))
    ));

    assert!(
        catalog
            .fork_before_user_message(Uuid::from_u128(22), "source-session")
            .is_err()
    );
    assert!(
        catalog
            .fork_before_user_message(Uuid::from_u128(99), "source-session")
            .is_err()
    );
}

#[test]
fn clone_at_leaf_copies_only_the_active_branch_and_adds_lineage() {
    let records = vec![
        user(31, None, 1, "root"),
        assistant(32, Some(31), 2, "answer"),
        user(33, Some(32), 3, "abandoned"),
        user(34, Some(32), 4, "active"),
    ];
    let catalog = SessionBranchCatalog::from_records(records).expect("catalog");

    let cloned = catalog
        .clone_at(Uuid::from_u128(34), "source-session")
        .expect("clone plan");
    assert_eq!(
        cloned
            .records
            .iter()
            .take(3)
            .map(|record| record.record_id)
            .collect::<Vec<_>>(),
        vec![
            Uuid::from_u128(31),
            Uuid::from_u128(32),
            Uuid::from_u128(34)
        ]
    );
    assert_eq!(cloned.records.len(), 4);
    assert_eq!(
        cloned.records.last().expect("lineage").parent_id,
        Some(Uuid::from_u128(34))
    );
    assert!(matches!(
        &cloned.records.last().expect("lineage").payload,
        SessionPayload::RuntimeEvent { name, detail }
            if name == "session_cloned_from" && detail == "source-session"
    ));

    let lineage_id = cloned.records.last().expect("lineage").record_id;
    let mut legacy_parentless_records = cloned.records.clone();
    legacy_parentless_records
        .last_mut()
        .expect("legacy lineage")
        .parent_id = None;
    legacy_parentless_records.push(user(36, None, 5, "continued after legacy clone"));
    let legacy_resumed =
        SessionBranchCatalog::from_records(legacy_parentless_records).expect("legacy clone");
    assert_eq!(
        legacy_resumed
            .node(lineage_id)
            .expect("normalized legacy lineage")
            .parent_id,
        Some(Uuid::from_u128(34))
    );
    assert_eq!(
        legacy_resumed
            .node(Uuid::from_u128(36))
            .expect("legacy continuation")
            .parent_id,
        Some(lineage_id)
    );

    let mut resumed_records = cloned.records;
    resumed_records.push(user(35, None, 5, "continued after clone"));
    let resumed = SessionBranchCatalog::from_records(resumed_records).expect("resumed clone");
    assert_eq!(
        resumed
            .branch_records(Uuid::from_u128(35))
            .expect("continued branch")
            .iter()
            .map(|record| record.record_id)
            .collect::<Vec<_>>(),
        vec![
            Uuid::from_u128(31),
            Uuid::from_u128(32),
            Uuid::from_u128(34),
            lineage_id,
            Uuid::from_u128(35),
        ]
    );
}

#[test]
fn abandoned_branch_messages_match_reference_common_ancestor_walk() {
    let ignored_event = record(
        76,
        Some(74),
        6,
        SessionPayload::RuntimeEvent {
            name: "ignored".into(),
            detail: "not a message".into(),
        },
    );
    let records = vec![
        user(71, None, 1, "shared root"),
        assistant(72, Some(71), 2, "shared answer"),
        user(75, Some(72), 5, "target sibling"),
        user(73, Some(72), 3, "abandoned request"),
        assistant(74, Some(73), 4, "abandoned answer"),
        ignored_event,
    ];
    let catalog = SessionBranchCatalog::from_records(records).expect("catalog");

    let sibling = catalog
        .abandoned_branch_messages(Uuid::from_u128(75))
        .expect("sibling summary slice");
    assert_eq!(
        sibling.iter().map(Message::text).collect::<Vec<_>>(),
        vec!["abandoned request", "abandoned answer"]
    );

    let ancestor = catalog
        .abandoned_branch_messages(Uuid::from_u128(72))
        .expect("ancestor summary slice");
    assert_eq!(
        ancestor.iter().map(Message::text).collect::<Vec<_>>(),
        vec!["abandoned request", "abandoned answer"]
    );

    let same_leaf = catalog
        .abandoned_branch_messages(Uuid::from_u128(76))
        .expect("same leaf");
    assert!(same_leaf.is_empty());
}

#[test]
fn abandoned_branch_messages_preserve_root_and_disconnected_tree_behavior() {
    let connected = SessionBranchCatalog::from_records(vec![
        user(81, None, 1, "root target"),
        user(82, Some(81), 2, "first abandoned"),
        assistant(83, Some(82), 3, "second abandoned"),
    ])
    .expect("connected catalog");
    let from_root = connected
        .abandoned_branch_messages(Uuid::from_u128(81))
        .expect("root summary slice");
    assert_eq!(
        from_root.iter().map(Message::text).collect::<Vec<_>>(),
        vec!["first abandoned", "second abandoned"]
    );

    let disconnected = SessionBranchCatalog::from_records(vec![
        user(91, None, 1, "target root"),
        user(92, None, 2, "old root"),
        assistant(93, Some(92), 3, "old leaf"),
    ])
    .expect("disconnected catalog");
    let without_common_ancestor = disconnected
        .abandoned_branch_messages(Uuid::from_u128(91))
        .expect("disconnected summary slice");
    assert_eq!(
        without_common_ancestor
            .iter()
            .map(Message::text)
            .collect::<Vec<_>>(),
        vec!["old root", "old leaf"]
    );
    assert!(
        disconnected
            .abandoned_branch_messages(Uuid::from_u128(999))
            .is_err()
    );
}

#[test]
fn rename_is_append_only_normalized_and_bounded() {
    let catalog =
        SessionBranchCatalog::from_records(vec![user(41, None, 1, "root")]).expect("catalog");
    let renamed = catalog.rename_record("  Important work  ").expect("rename");
    assert_eq!(renamed.parent_id, Some(Uuid::from_u128(41)));
    assert!(matches!(
        renamed.payload,
        SessionPayload::RuntimeEvent { name, detail }
            if name == "session_name" && detail == "Important work"
    ));
    assert!(catalog.rename_record(" \n ").is_err());
    assert!(catalog.rename_record("unsafe\nname").is_err());
    assert!(
        catalog
            .rename_record(&"n".repeat(MAX_SESSION_NAME_CHARS + 1))
            .is_err()
    );
}

#[test]
fn malformed_duplicate_cyclic_and_unsupported_records_fail_closed() {
    assert!(SessionBranchCatalog::from_jsonl("{not-json}\n").is_err());

    let duplicate = user(51, None, 1, "one");
    assert!(SessionBranchCatalog::from_records(vec![duplicate.clone(), duplicate]).is_err());

    let cyclic = vec![user(52, Some(53), 1, "one"), user(53, Some(52), 2, "two")];
    assert!(SessionBranchCatalog::from_records(cyclic).is_err());

    let mut unsupported = user(54, None, 1, "one");
    unsupported.schema_version = SESSION_SCHEMA_VERSION + 1;
    assert!(SessionBranchCatalog::from_records(vec![unsupported]).is_err());
}

#[test]
fn orphan_and_self_parent_records_are_stable_roots_and_names_match_reference_trimming() {
    let orphan_parent = Uuid::from_u128(999);
    let name = record(
        63,
        Some(62),
        3,
        SessionPayload::RuntimeEvent {
            name: "session_name".into(),
            detail: "  trimmed name  ".into(),
        },
    );
    let catalog = SessionBranchCatalog::from_records(vec![
        user(61, Some(61), 1, "self root"),
        SessionRecord {
            parent_id: Some(orphan_parent),
            ..user(62, None, 2, "orphan root")
        },
        name,
    ])
    .expect("catalog");

    assert_eq!(catalog.roots(), &[Uuid::from_u128(61), Uuid::from_u128(62)]);
    assert_eq!(catalog.session_name(), Some("trimmed name"));
    assert!(catalog.clone_active(" source-session").is_err());

    let cleared = SessionBranchCatalog::from_records(vec![
        record(
            64,
            None,
            1,
            SessionPayload::RuntimeEvent {
                name: "session_name".into(),
                detail: "old name".into(),
            },
        ),
        record(
            65,
            None,
            2,
            SessionPayload::RuntimeEvent {
                name: "session_name".into(),
                detail: "   ".into(),
            },
        ),
    ])
    .expect("cleared name catalog");
    assert_eq!(cleared.session_name(), None);
}
