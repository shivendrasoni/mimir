use std::{collections::BTreeSet, path::Path, time::Duration};

use mimir::extensions::{
    Capability, ExtensionCallStatus, ExtensionCatalog, ExtensionEntrypoint, ExtensionFlagValue,
    ExtensionHostAction, ExtensionHostSnapshot, ExtensionManager, ExtensionManifest,
    ExtensionRuntime, HostLimits, LifecycleEvent, RuntimeLimits,
};
use serde_json::json;
use tempfile::TempDir;

fn limits(timeout: Duration) -> RuntimeLimits {
    RuntimeLimits {
        host: HostLimits {
            max_request_bytes: 32 * 1024,
            max_response_bytes: 64 * 1024,
            timeout,
        },
        max_concurrency: 2,
        max_registrations: 32,
    }
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "one end-to-end isolate fixture proves registrations, snapshots, safe exec, host actions, rendering, event delivery, and persistent shortcut state together"
)]
async fn embedded_host_actions_queries_renderers_providers_and_safe_exec_are_operational() {
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    let extension = TempDir::new().expect("extension");
    let entry = extension.path().join("bridge.ts");
    std::fs::write(
        &entry,
        r#"
export default function activate(pi) {
  let busValue = "none";
  pi.events.on("bridge_ready", data => { busValue = data.value; });
  pi.registerFlag("mode", { type: "string", default: "safe" });
  pi.registerShortcut("ctrl+x", {
    description: "Run bridge",
    handler() { pi.appendEntry("shortcut", { invoked: true }); return { message: `shortcut:${busValue}` }; },
  });
  pi.registerMessageRenderer("bridge_message", message => [`render:${message.value}`]);
  pi.registerProvider("bridge_provider", {
    api: "openai-responses",
    baseUrl: "https://example.invalid/v1",
    apiKey: "BRIDGE_API_KEY",
    models: [{ id: "bridge-model" }],
  });
  pi.registerCommand("bridge", {
    async handler(_args, ctx) {
      const ran = await pi.exec("pwd", [], { timeout: 1000 });
      pi.setActiveTools([pi.getActiveTools()[0]]);
      pi.setThinkingLevel("off");
      pi.sendMessage({ customType: "bridge_message", content: { value: 1 }, display: true });
      pi.appendEntry("bridge_state", { ready: true });
      pi.events.emit("bridge_ready", { cwd: ctx.cwd });
      return { message: `${pi.getFlag("mode")}:${ran.stdout.trim()}`, output: pi.getAllTools() };
    },
  });
}
"#,
    )
    .expect("entry");
    let mut manifest = embedded_manifest("bridge-api", &entry);
    manifest
        .capabilities
        .extend([Capability::Provider, Capability::Process]);
    let runtime = ExtensionRuntime::load(
        manifest,
        workspace.path(),
        state.path(),
        limits(Duration::from_secs(2)),
        "init-bridge",
    )
    .await
    .expect("load bridge");

    assert_eq!(runtime.registrations().shortcuts.len(), 1);
    assert_eq!(runtime.registrations().flags.len(), 1);
    assert_eq!(runtime.registrations().renderers.len(), 1);
    assert_eq!(
        runtime.registrations().providers[0].models,
        ["bridge-model"]
    );
    let snapshot = ExtensionHostSnapshot {
        cwd: workspace.path().display().to_string(),
        active_tools: vec!["read_file".into(), "search".into()],
        flags: vec![ExtensionFlagValue {
            name: "mode".into(),
            value: json!("configured"),
        }],
        all_tools: vec![mimir::extensions::ExtensionToolInfo {
            name: "read_file".into(),
            description: "read".into(),
            parameters: json!({"type":"object"}),
            source: "runtime".into(),
        }],
        ..ExtensionHostSnapshot::default()
    };
    let result = runtime
        .invoke_command_with_snapshot("bridge-call", "bridge", "", snapshot)
        .await
        .expect("invoke bridge");
    assert!(
        result
            .message
            .as_deref()
            .is_some_and(|message| message.starts_with("configured:"))
    );
    assert_eq!(result.actions.len(), 5);
    assert!(matches!(
        result.actions[0],
        ExtensionHostAction::SetActiveTools { .. }
    ));
    assert!(matches!(
        result.actions[4],
        ExtensionHostAction::PublishEvent { .. }
    ));
    let rendered = runtime
        .render_message(
            "bridge-render",
            "bridge_message",
            json!({"value": 7}),
            false,
        )
        .await
        .expect("render bridge message");
    assert_eq!(rendered.lines, ["render:7"]);
    assert!(
        runtime
            .deliver_bus_event("bridge_ready", json!({"value":"delivered"}))
            .await
            .expect("deliver bus event")
    );
    let shortcut = runtime
        .invoke_shortcut_with_snapshot(
            "bridge-shortcut",
            "ctrl+x",
            ExtensionHostSnapshot::default(),
        )
        .await
        .expect("invoke shortcut");
    assert_eq!(shortcut.message.as_deref(), Some("shortcut:delivered"));
    assert!(matches!(
        shortcut.actions.as_slice(),
        [ExtensionHostAction::AppendEntry { .. }]
    ));
}

fn embedded_manifest(name: &str, module: &Path) -> ExtensionManifest {
    ExtensionManifest {
        schema_version: 1,
        name: name.into(),
        version: "1.0.0".into(),
        entrypoint: ExtensionEntrypoint::EmbeddedJavaScript {
            module: module.display().to_string(),
        },
        capabilities: BTreeSet::from([
            Capability::Tools,
            Capability::Commands,
            Capability::Ui,
            Capability::Lifecycle,
        ]),
    }
}

#[tokio::test]
async fn typescript_default_export_factory_registers_and_keeps_isolate_state() {
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    let extension = TempDir::new().expect("extension");
    let helper = extension.path().join("helper.ts");
    std::fs::write(
        &helper,
        "export const describe = (value: string): string => `value=${value}`;\n",
    )
    .expect("helper");
    let entry = extension.path().join("index.ts");
    std::fs::write(
        &entry,
        r#"
import { describe } from "./helper.js";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

export default async function activate(pi: ExtensionAPI) {
  let calls: number = 0;
  pi.registerTool({
    name: "counter",
    label: "Counter",
    description: "Persistent counter",
    parameters: { type: "object" },
    async execute(_toolCallId: string, input: { value: string }) {
      calls += 1;
      return {
        content: [{ type: "text", text: `${describe(input.value)};calls=${calls}` }],
        details: { calls },
      };
    },
  });
  pi.registerCommand("hello", {
    description: "Say hello",
    async handler(args: string, ctx: unknown) {
      ctx.ui.notify(`hello ${args}`, "info");
      return { message: `hello ${args}`, output: { args } };
    },
  });
  pi.on("turn_start", async event => ({ output: { seen: event.turnIndex } }));
}
"#,
    )
    .expect("entry");

    let runtime = ExtensionRuntime::load(
        embedded_manifest("embedded-fixture", &entry),
        workspace.path(),
        state.path(),
        limits(Duration::from_secs(2)),
        "init-1",
    )
    .await
    .expect("load embedded extension");
    assert_eq!(runtime.registrations().tools[0].name, "counter");
    assert_eq!(runtime.registrations().commands[0].name, "hello");

    let first = runtime
        .invoke_tool("tool-1", "counter", "call-1", json!({"value":"a"}))
        .await
        .expect("first tool call");
    assert_eq!(first.status, ExtensionCallStatus::Ok);
    assert_eq!(first.summary, "value=a;calls=1");
    let second = runtime
        .invoke_tool("tool-2", "counter", "call-2", json!({"value":"b"}))
        .await
        .expect("second tool call");
    assert_eq!(second.summary, "value=b;calls=2");

    let command = runtime
        .invoke_command("command-1", "hello", "world")
        .await
        .expect("command");
    assert_eq!(command.message.as_deref(), Some("hello world"));
    assert_eq!(command.ui_requests.len(), 1);

    let outcome = runtime
        .dispatch(
            "event-1",
            LifecycleEvent::TurnStart {
                session_id: "session-1".into(),
                turn_index: 7,
            },
        )
        .await
        .expect("dispatch")
        .expect("registered lifecycle event");
    assert_eq!(outcome.output, json!({"seen": 7}));
}

#[tokio::test]
async fn embedded_resolver_rejects_node_modules_and_execution_is_deadline_bounded() {
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    let extension = TempDir::new().expect("extension");
    let forbidden = extension.path().join("forbidden.ts");
    std::fs::write(
        &forbidden,
        "import fs from 'node:fs'; export default () => fs.readFileSync('/etc/passwd');",
    )
    .expect("forbidden");
    let error = ExtensionRuntime::load(
        embedded_manifest("forbidden-module", &forbidden),
        workspace.path(),
        state.path(),
        limits(Duration::from_millis(250)),
        "init-forbidden",
    )
    .await
    .expect_err("node module import must fail closed");
    assert!(error.to_string().contains("node:fs"));

    let unsupported = extension.path().join("unsupported.ts");
    std::fs::write(
        &unsupported,
        "export default pi => pi.registerPrompt('name', { handler() {} });",
    )
    .expect("unsupported entry");
    let error = ExtensionRuntime::load(
        embedded_manifest("unsupported-api", &unsupported),
        workspace.path(),
        state.path(),
        limits(Duration::from_millis(250)),
        "init-unsupported",
    )
    .await
    .expect_err("unsupported APIs must fail explicitly");
    assert!(
        error
            .to_string()
            .contains("Unsupported extension API: registerPrompt")
    );

    let loop_entry = extension.path().join("loop.ts");
    std::fs::write(
        &loop_entry,
        "export default function activate() { while (true) {} }",
    )
    .expect("loop entry");
    let error = ExtensionRuntime::load(
        embedded_manifest("deadline-loop", &loop_entry),
        workspace.path(),
        state.path(),
        limits(Duration::from_millis(100)),
        "init-loop",
    )
    .await
    .expect_err("infinite loop must be interrupted");
    assert!(error.to_string().contains("timed out"));
}

#[tokio::test]
async fn migrated_archived_typescript_extension_is_discovered_and_activated() {
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    let archive = state
        .path()
        .join("migration/compatibility/v1/resources/extensions/greeting/index.ts");
    std::fs::create_dir_all(archive.parent().expect("archive parent")).expect("archive dir");
    std::fs::write(
        &archive,
        r#"export default pi => pi.registerCommand("greet", { description: "Greet", handler: async args => ({ message: `hi ${args}`, output: null }) });"#,
    )
    .expect("archive entry");
    std::fs::create_dir_all(state.path().join("config")).expect("config dir");
    std::fs::write(
        state.path().join("config/inventory.json"),
        serde_json::to_vec_pretty(&json!({
            "schema_version": 1,
            "packages": [],
            "extensions": ["extensions/greeting"],
            "skills": [],
            "discovered": {"extensions": ["extensions/greeting/index.ts"], "skills": []}
        }))
        .expect("inventory"),
    )
    .expect("inventory write");

    let mut catalog = ExtensionCatalog::new(workspace.path(), state.path()).expect("catalog");
    let entries = catalog.snapshot().await.expect("snapshot");
    let migrated = entries
        .iter()
        .find(|entry| entry.manifest.name == "migrated-greeting")
        .expect("migrated extension entry");
    assert!(matches!(
        migrated.manifest.entrypoint,
        ExtensionEntrypoint::EmbeddedJavaScript { .. }
    ));

    let manager = ExtensionManager::load(
        entries,
        workspace.path(),
        state.path(),
        limits(Duration::from_secs(2)),
    )
    .await
    .expect("manager");
    assert_eq!(manager.commands()[0].name, "greet");
    let result = manager
        .invoke_command("greet", "Ada")
        .await
        .expect("migrated command");
    assert_eq!(result.message.as_deref(), Some("hi Ada"));
}
