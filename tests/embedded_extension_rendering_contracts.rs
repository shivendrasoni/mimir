use std::{collections::BTreeSet, path::Path, time::Duration};

use mimir::extensions::{
    Capability, ExtensionCallStatus, ExtensionEntrypoint, ExtensionManifest, ExtensionRuntime,
    HostLimits, RuntimeLimits,
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

fn embedded_manifest(name: &str, module: &Path) -> ExtensionManifest {
    ExtensionManifest {
        schema_version: 1,
        name: name.into(),
        version: "1.0.0".into(),
        entrypoint: ExtensionEntrypoint::EmbeddedJavaScript {
            module: module.display().to_string(),
        },
        capabilities: BTreeSet::from([Capability::Tools, Capability::Commands]),
    }
}

#[tokio::test]
async fn embedded_tool_rendering_and_command_completions_use_explicit_abi_paths() {
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    let extension = TempDir::new().expect("extension");
    let entry = extension.path().join("advanced.ts");
    std::fs::write(
        &entry,
        r#"
export default function activate(pi) {
  pi.registerTool({
    name: "advanced",
    label: "Advanced",
    description: "Prepare and render tool calls",
    parameters: { type: "object" },
    async prepareArguments(input) {
      return { value: String(input.value || "").trim().toUpperCase() };
    },
    renderCall(input) {
      return [`call:${input.value}`];
    },
    async execute(_toolCallId, input) {
      return {
        content: [{ type: "text", text: `ran:${input.value}` }],
        details: { prepared: input.value },
      };
    },
    renderResult(result, input) {
      return [`result:${input.value}`, `details:${result.details.prepared}`];
    },
  });
  pi.registerCommand("deploy", {
    description: "Deploy the project",
    getArgumentCompletions(args) {
      if (args === "--") {
        return [{ value: "--force", description: "Force the rollout" }];
      }
      return ["preview", "production"];
    },
    async handler(args) {
      return { message: `deploy:${args}`, output: { args } };
    },
  });
}
"#,
    )
    .expect("entry");

    let runtime = ExtensionRuntime::load(
        embedded_manifest("embedded-advanced", &entry),
        workspace.path(),
        state.path(),
        limits(Duration::from_secs(2)),
        "init-advanced",
    )
    .await
    .expect("load embedded extension");

    assert!(runtime.registrations().commands[0].supports_argument_completions);

    let tool = runtime
        .invoke_tool("tool-1", "advanced", "call-1", json!({"value":"  hello "}))
        .await
        .expect("invoke tool");
    assert_eq!(tool.status, ExtensionCallStatus::Ok);
    assert_eq!(tool.summary, "ran:HELLO");
    assert_eq!(
        tool.content,
        json!([{ "type": "text", "text": "ran:HELLO" }])
    );
    assert_eq!(tool.render_call.expect("render call").lines, ["call:HELLO"]);
    assert_eq!(
        tool.render_result.expect("render result").lines,
        ["result:HELLO", "details:HELLO"]
    );

    let completions = runtime
        .get_command_argument_completions("complete-1", "deploy", "--")
        .await
        .expect("command completions");
    assert_eq!(completions.items.len(), 1);
    assert_eq!(completions.items[0].value, "--force");
    assert_eq!(
        completions.items[0].description.as_deref(),
        Some("Force the rollout")
    );

    let more = runtime
        .get_command_argument_completions("complete-2", "deploy", "")
        .await
        .expect("base completions");
    assert_eq!(more.items[0].value, "preview");
    assert_eq!(more.items[1].value, "production");
}
