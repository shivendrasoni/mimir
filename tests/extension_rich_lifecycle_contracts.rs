use std::{collections::BTreeSet, path::Path, time::Duration};

use mimir::{
    extensions::{
        Capability, CatalogEntry, ExtensionEntrypoint, ExtensionHostAction, ExtensionHostSnapshot,
        ExtensionManager, ExtensionManifest, ExtensionRuntime, HostLimits, LifecycleEvent,
        LifecycleInterception, LifecycleReplacement, ManifestSource, RuntimeLimits,
    },
    model::Message,
};
use tempfile::TempDir;

fn limits() -> RuntimeLimits {
    RuntimeLimits {
        host: HostLimits {
            max_request_bytes: 32 * 1024,
            max_response_bytes: 64 * 1024,
            timeout: Duration::from_secs(2),
        },
        max_concurrency: 2,
        max_registrations: 16,
    }
}

#[tokio::test]
async fn command_session_controls_emit_bounded_host_actions() {
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    let extension = TempDir::new().expect("extension");
    let module = extension.path().join("sessions.ts");
    std::fs::write(
        &module,
        r#"
export default function activate(pi) {
  pi.registerCommand("sessions", {
    async handler(_args, ctx) {
      await ctx.newSession({ parentSession: "parent.jsonl" });
      await ctx.fork("entry-1", { position: "at" });
      await ctx.navigateTree("entry-2", { summarize: true, label: "checkpoint" });
      await ctx.switchSession("session.jsonl");
      await ctx.reload();
      ctx.compact({ customInstructions: "keep decisions" });
      return { message: "queued" };
    },
  });
}
"#,
    )
    .expect("module");
    let mut session_manifest = manifest("session-actions", &module);
    session_manifest.capabilities.insert(Capability::Commands);
    let runtime = ExtensionRuntime::load(
        session_manifest,
        workspace.path(),
        state.path(),
        limits(),
        "init-session",
    )
    .await
    .expect("runtime");
    let result = runtime
        .invoke_command("session-actions", "sessions", "")
        .await
        .expect("command");
    assert_eq!(result.actions.len(), 6);
    assert!(
        result
            .actions
            .iter()
            .all(|action| matches!(action, ExtensionHostAction::Session { .. }))
    );
}

#[tokio::test]
async fn interactive_command_promises_suspend_and_resume_through_correlated_responses() {
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    let extension = TempDir::new().expect("extension");
    let module = extension.path().join("interactive.ts");
    std::fs::write(
        &module,
        r#"
export default function activate(pi) {
  pi.registerCommand("interactive", {
    async handler(_args, ctx) {
      const name = await ctx.ui.input("Name", "Ada");
      const confirmed = await ctx.ui.confirm("Continue", `Use ${name}?`);
      const target = await ctx.ui.select("Target", ["dev", "prod"]);
      pi.appendEntry("interactive_result", { name, confirmed, target });
      return { message: `${name}:${confirmed}:${target}` };
    },
  });
}
"#,
    )
    .expect("module");
    let mut interactive_manifest = manifest("interactive-ui", &module);
    interactive_manifest
        .capabilities
        .extend([Capability::Commands, Capability::Ui]);
    let manager = ExtensionManager::load(
        vec![CatalogEntry {
            manifest: interactive_manifest,
            root_dir: extension.path().to_owned(),
            source: ManifestSource::Workspace,
            enabled: true,
        }],
        workspace.path(),
        state.path(),
        limits(),
    )
    .await
    .expect("manager");

    let suspended = manager
        .invoke_command("interactive", "")
        .await
        .expect("initial suspend");
    let first_id = match &suspended.ui_requests[0] {
        mimir::extensions::UiRequest::Input { id, .. } => id.clone(),
        other => panic!("expected input, got {other:?}"),
    };
    let _ = manager.drain_ui_requests().await;
    assert!(
        manager
            .respond_ui(&first_id, serde_json::json!("Ada"))
            .await
            .expect("input response")
            .is_none()
    );
    let second = manager.drain_ui_requests().await;
    let second_id = match &second[0].1 {
        mimir::extensions::UiRequest::Confirm { id, .. } => id.clone(),
        other => panic!("expected confirm, got {other:?}"),
    };
    assert!(
        manager
            .respond_ui(&second_id, serde_json::json!(true))
            .await
            .expect("confirm response")
            .is_none()
    );
    let third = manager.drain_ui_requests().await;
    let third_id = match &third[0].1 {
        mimir::extensions::UiRequest::Select { id, .. } => id.clone(),
        other => panic!("expected select, got {other:?}"),
    };
    let completed = manager
        .respond_ui(&third_id, serde_json::json!("prod"))
        .await
        .expect("select response")
        .expect("command continuation");
    assert_eq!(completed.message.as_deref(), Some("Ada:true:prod"));
    assert!(matches!(
        manager.drain_host_actions().await.as_slice(),
        [ExtensionHostAction::AppendEntry { .. }]
    ));
}

fn manifest(name: &str, module: &Path) -> ExtensionManifest {
    ExtensionManifest {
        schema_version: 1,
        name: name.into(),
        version: "1.0.0".into(),
        entrypoint: ExtensionEntrypoint::EmbeddedJavaScript {
            module: module.display().to_string(),
        },
        capabilities: BTreeSet::from([Capability::Lifecycle]),
    }
}

#[tokio::test]
async fn message_end_can_replace_content_but_runtime_receives_the_original_role() {
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    let extension = TempDir::new().expect("extension");
    let module = extension.path().join("message.ts");
    std::fs::write(
        &module,
        r#"
export default function activate(pi) {
  pi.on("message_end", event => ({
    message: { ...event.message, content: [{ type: "text", text: "rewritten" }] },
  }));
}
"#,
    )
    .expect("module");
    let runtime = ExtensionRuntime::load(
        manifest("message-rewriter", &module),
        workspace.path(),
        state.path(),
        limits(),
        "init",
    )
    .await
    .expect("runtime");
    let message = Message::user("original");
    let outcome = runtime
        .dispatch(
            "message-end",
            LifecycleEvent::MessageEnd {
                session_id: "session".into(),
                message_id: "message".into(),
                role: "user".into(),
                message,
            },
        )
        .await
        .expect("dispatch")
        .expect("outcome");
    let LifecycleInterception::Replace {
        replacement: LifecycleReplacement::MessageEnd { message },
    } = outcome.interception
    else {
        panic!("expected message replacement");
    };
    assert_eq!(message.text(), "rewritten");
}

#[tokio::test]
async fn before_agent_start_system_prompt_replacements_chain_in_extension_order() {
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    let first = TempDir::new().expect("first");
    let second = TempDir::new().expect("second");
    let first_module = first.path().join("first.ts");
    let second_module = second.path().join("second.ts");
    std::fs::write(
        &first_module,
        r#"export default pi => pi.on("before_agent_start", (_event, ctx) => ({ systemPrompt: `${ctx.getSystemPrompt()}-first` }));"#,
    )
    .expect("first module");
    std::fs::write(
        &second_module,
        r#"export default pi => pi.on("before_agent_start", (_event, ctx) => ({ systemPrompt: `${ctx.getSystemPrompt()}-second` }));"#,
    )
    .expect("second module");
    let entries = [
        ("a-first", first.path(), first_module),
        ("b-second", second.path(), second_module),
    ]
    .into_iter()
    .map(|(name, root, module)| CatalogEntry {
        manifest: manifest(name, &module),
        root_dir: root.to_owned(),
        source: ManifestSource::Workspace,
        enabled: true,
    })
    .collect();
    let manager = ExtensionManager::load(entries, workspace.path(), state.path(), limits())
        .await
        .expect("manager");
    let outcomes = manager
        .dispatch_with_snapshot(
            LifecycleEvent::BeforeAgentStart {
                session_id: "session".into(),
                parent_session_id: None,
            },
            ExtensionHostSnapshot {
                system_prompt: "base".into(),
                ..ExtensionHostSnapshot::default()
            },
        )
        .await
        .expect("dispatch");
    assert_eq!(outcomes[0].outcome.output["systemPrompt"], "base-first");
    assert_eq!(
        outcomes[1].outcome.output["systemPrompt"],
        "base-first-second"
    );
}
