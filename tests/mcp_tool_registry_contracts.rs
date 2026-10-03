use std::{collections::BTreeMap, os::unix::fs::PermissionsExt, path::Path, time::Duration};

use mimir::{
    mcp::{McpCatalogServer, McpCatalogStdio, McpServerCatalog},
    tools::{ObservationStatus, ToolPolicy, ToolRegistry},
};
use serde_json::json;
use tempfile::TempDir;

fn make_script(path: &Path, body: &str) {
    std::fs::write(path, body).expect("script");
    let mut permissions = std::fs::metadata(path).expect("metadata").permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions).expect("permissions");
}

fn fake_mcp_script(log_path: &Path) -> String {
    format!(
        r#"#!/bin/sh
set -eu
LOG="{log}"

read_message() {{
  len=""
  while IFS= read -r line; do
    line=$(printf '%s' "$line" | tr -d '\r')
    if [ -z "$line" ]; then break; fi
    case "$line" in
      [Cc]ontent-[Ll]ength:*) len=$(printf '%s' "$line" | sed 's/^[^:]*:[ ]*//') ;;
    esac
  done
  tmp=$(mktemp)
  dd bs=1 count="$len" of="$tmp" 2>/dev/null
  cat "$tmp"
  rm -f "$tmp"
}}

send_json() {{
  payload="$1"
  length=$(printf '%s' "$payload" | wc -c | tr -d ' ')
  printf 'Content-Length: %s\r\n\r\n%s' "$length" "$payload"
}}

request=$(read_message)
printf '%s\n' "$request" >> "$LOG"
id=$(printf '%s' "$request" | sed -n 's/.*"id":[ ]*\([0-9][0-9]*\).*/\1/p')
send_json "{{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{{\"protocolVersion\":\"2025-06-18\",\"capabilities\":{{\"tools\":{{}}}},\"serverInfo\":{{\"name\":\"registry-test\",\"version\":\"1.0.0\"}}}}}}"

request=$(read_message)
printf '%s\n' "$request" >> "$LOG"

request=$(read_message)
printf '%s\n' "$request" >> "$LOG"
id=$(printf '%s' "$request" | sed -n 's/.*"id":[ ]*\([0-9][0-9]*\).*/\1/p')
case "$request" in
  *'"method":"tools/list"'*)
    send_json "{{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{{\"tools\":[{{\"name\":\"notion-search\",\"description\":\"Search the workspace\",\"inputSchema\":{{\"type\":\"object\",\"properties\":{{\"query\":{{\"type\":\"string\"}}}},\"required\":[\"query\"]}}}}]}}}}"
    request=$(read_message)
    printf '%s\n' "$request" >> "$LOG"
    id=$(printf '%s' "$request" | sed -n 's/.*"id":[ ]*\([0-9][0-9]*\).*/\1/p')
    ;;
esac
send_json "{{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{{\"structuredContent\":{{\"matches\":2}},\"content\":[],\"isError\":false}}}}"
"#,
        log = log_path.display()
    )
}

fn stdio_entry(server: &str, label: &str, program: &Path) -> McpCatalogServer {
    McpCatalogServer::new(
        server,
        label,
        McpCatalogStdio {
            program: program.to_owned(),
            args: Vec::new(),
            env: BTreeMap::new(),
            startup_timeout_ms: 5_000,
            io_timeout_ms: 2_000,
            max_frame_bytes: 64 * 1024,
            max_tool_payload_bytes: 8 * 1024,
            max_stderr_bytes: 4 * 1024,
        },
    )
    .expect("catalog entry")
}

#[tokio::test]
async fn enabled_mcp_tools_are_discovered_registered_and_invoked() {
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    let script = workspace.path().join("fake-mcp.sh");
    let log = workspace.path().join("requests.jsonl");
    make_script(&script, &fake_mcp_script(&log));

    let catalog = McpServerCatalog::new(state.path()).expect("catalog");
    catalog
        .upsert(stdio_entry("knowledge", "Knowledge", &script))
        .await
        .expect("upsert");

    let mut registry = ToolRegistry::with_default_tools(
        workspace.path(),
        ToolPolicy {
            command_timeout: Duration::from_secs(1),
            ..ToolPolicy::default()
        },
    )
    .expect("registry");
    let report = registry
        .register_mcp_servers(state.path())
        .await
        .expect("register MCP");
    assert!(
        report.unavailable.is_empty(),
        "unexpected MCP failures: {:?}",
        report.unavailable
    );
    assert_eq!(report.registered_tools.len(), 1);

    let definition = registry
        .definitions()
        .into_iter()
        .find(|definition| definition.name == report.registered_tools[0])
        .expect("MCP definition");
    assert!(definition.name.starts_with("mcp_knowledge_notion_search_"));
    assert_eq!(definition.parameters["required"], json!(["query"]));

    let observation = registry
        .execute(&definition.name, json!({"query":"roadmap"}))
        .await
        .expect("MCP invocation");
    assert_eq!(observation.status, ObservationStatus::Success);
    assert_eq!(observation.content, json!({"matches":2}).to_string());

    // The fake server exits after one call. A failed follow-up is not retried
    // automatically (the tool may have side effects), but it discards the
    // stale transport so the next explicit call reconnects safely.
    assert!(
        registry
            .execute(&definition.name, json!({"query":"second"}))
            .await
            .is_err()
    );
    let reconnected = registry
        .execute(&definition.name, json!({"query":"third"}))
        .await
        .expect("explicit retry reconnects");
    assert_eq!(reconnected.content, json!({"matches":2}).to_string());

    let requests = std::fs::read_to_string(log).expect("request log");
    assert!(requests.contains(r#""method":"tools/list""#));
    assert!(requests.contains(r#""name":"notion-search""#));
    assert!(requests.contains(r#""query":"roadmap""#));
}

#[tokio::test]
async fn an_unavailable_server_does_not_block_other_runtime_tools() {
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    let catalog = McpServerCatalog::new(state.path()).expect("catalog");
    catalog
        .upsert(stdio_entry(
            "missing",
            "Missing",
            &workspace.path().join("does-not-exist"),
        ))
        .await
        .expect("upsert");

    let mut registry = ToolRegistry::with_default_tools(workspace.path(), ToolPolicy::default())
        .expect("registry");
    let before = registry.definitions().len();
    let report = registry
        .register_mcp_servers(state.path())
        .await
        .expect("non-fatal discovery");

    assert!(report.registered_tools.is_empty());
    assert_eq!(report.unavailable.len(), 1);
    assert_eq!(report.unavailable[0].server, "missing");
    assert_eq!(registry.definitions().len(), before);
    assert!(
        registry
            .definitions()
            .iter()
            .any(|tool| tool.name == "read_file")
    );
}

#[tokio::test]
async fn changed_catalog_cannot_rebind_an_existing_tool_to_another_workspace() {
    let workspace = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let first = workspace.path().join("a.sh");
    let second = workspace.path().join("b.sh");
    let first_log = workspace.path().join("a.jsonl");
    let second_log = workspace.path().join("b.jsonl");
    make_script(&first, &fake_mcp_script(&first_log));
    make_script(&second, &fake_mcp_script(&second_log));
    let catalog = McpServerCatalog::new(state.path()).unwrap();
    catalog
        .upsert(stdio_entry("knowledge", "Knowledge", &first))
        .await
        .unwrap();
    let mut registry =
        ToolRegistry::with_default_tools(workspace.path(), ToolPolicy::default()).unwrap();
    let report = registry.register_mcp_servers(state.path()).await.unwrap();
    let name = &report.registered_tools[0];
    registry
        .execute(name, json!({"query":"first"}))
        .await
        .unwrap();
    assert!(
        registry
            .execute(name, json!({"query":"closed"}))
            .await
            .is_err()
    );
    catalog
        .upsert(stdio_entry("knowledge", "Knowledge", &second))
        .await
        .unwrap();
    assert!(
        registry
            .execute(name, json!({"query":"must not reconnect B"}))
            .await
            .is_err()
    );
    assert!(!second_log.exists());
    catalog
        .upsert(stdio_entry("knowledge", "Knowledge", &first))
        .await
        .unwrap();
    registry
        .execute(name, json!({"query":"restored A"}))
        .await
        .unwrap();
    assert!(!second_log.exists());
}

#[tokio::test]
async fn owned_server_without_signed_receipt_is_never_discovered_as_unmanaged() {
    let workspace = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let script = workspace.path().join("must-not-run.sh");
    let log = workspace.path().join("must-not-read.jsonl");
    make_script(&script, &fake_mcp_script(&log));
    let catalog = McpServerCatalog::new(state.path()).unwrap();
    catalog
        .upsert(stdio_entry("bl_private", "Private", &script))
        .await
        .unwrap();
    let mut registry =
        ToolRegistry::with_default_tools(workspace.path(), ToolPolicy::default()).unwrap();
    let report = registry.register_mcp_servers(state.path()).await.unwrap();
    assert!(report.registered_tools.is_empty());
    assert_eq!(report.unavailable.len(), 1);
    assert!(!log.exists());
}
