use std::os::unix::fs::PermissionsExt;

use mimir::{
    extensions::ExtensionCatalog,
    tools::{ToolPolicy, ToolRegistry},
};
use serde_json::json;
use tempfile::TempDir;

#[tokio::test]
async fn enabled_tool_extensions_are_exposed_through_the_agent_registry() {
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    let script = workspace.path().join("extension.sh");
    std::fs::write(
        &script,
        r#"#!/bin/sh
read line
id=$(printf '%s' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
printf '{"schema_version":1,"id":"%s","status":"ok","output":{"value":42}}\n' "$id"
"#,
    )
    .expect("script");
    let mut permissions = std::fs::metadata(&script).expect("metadata").permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&script, permissions).expect("permissions");
    let manifest_path = workspace
        .path()
        .join(".mimir/extensions/example/manifest.json");
    std::fs::create_dir_all(manifest_path.parent().expect("parent")).expect("directory");
    std::fs::write(
        manifest_path,
        serde_json::to_vec(&json!({
            "schema_version": 1,
            "name": "example",
            "version": "1.0.0",
            "entrypoint": {"program": script, "args": []},
            "capabilities": ["tools"]
        }))
        .expect("manifest"),
    )
    .expect("write manifest");
    let mut catalog = ExtensionCatalog::new(workspace.path(), state.path()).expect("catalog");
    let entries = catalog.reload().await.expect("reload");
    let mut tools = ToolRegistry::with_default_tools(workspace.path(), ToolPolicy::default())
        .expect("registry");
    tools
        .register_extension_tools(entries, workspace.path())
        .expect("register");

    let observation = tools
        .execute(
            "extension_invoke",
            json!({"extension":"example","command":"answer","payload":{}}),
        )
        .await
        .expect("invoke");
    assert_eq!(observation.content, "{\"value\":42}");
}
