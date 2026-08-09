use std::{
    collections::BTreeMap,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::Duration,
};

use mimir::{
    auth::{AuthStore, OAuthCredential},
    mcp::{
        McpAuthCoordinator, McpAuthSource, McpCatalogServer, McpCatalogStdio, McpClient,
        McpServerCatalog, McpServerConfig, McpStdioConfig, McpToolCallOutput, auth_status,
        auth_status_with_lookup, tool_identifier,
    },
};
use serde_json::json;
use tempfile::TempDir;

fn make_script(path: &Path, body: &str) {
    std::fs::write(path, body).expect("script");
    let mut permissions = std::fs::metadata(path).expect("metadata").permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions).expect("permissions");
}

fn stdio_config(program: PathBuf) -> McpStdioConfig {
    McpStdioConfig {
        program,
        args: Vec::new(),
        env: BTreeMap::new(),
        startup_timeout: Duration::from_secs(1),
        io_timeout: Duration::from_secs(2),
        max_frame_bytes: 64 * 1024,
        max_tool_payload_bytes: 8 * 1024,
        max_stderr_bytes: 4 * 1024,
    }
}

#[test]
fn config_validation_and_tool_identifier_follow_the_reference_rules() {
    let relative = McpServerConfig::new(
        "demo",
        "Demo",
        McpStdioConfig {
            program: PathBuf::from("relative.sh"),
            ..McpStdioConfig::default()
        },
    )
    .expect_err("relative programs must be rejected");
    assert!(relative.to_string().contains("absolute path"));

    assert_eq!(tool_identifier("list_issues"), Some("list_issues".into()));
    assert_eq!(tool_identifier("notion-search"), None);
    assert_eq!(tool_identifier("call_tool"), None);
}

#[tokio::test]
async fn auth_status_prefers_bearer_env_and_reports_expired_oauth() {
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    let script = workspace.path().join("noop.sh");
    make_script(&script, "#!/bin/sh\nexit 0\n");
    let mut config = McpServerConfig::new("demo", "Demo", stdio_config(script)).expect("config");
    config.bearer_token_env_var = Some("DEMO_MCP_TOKEN".into());
    let store = AuthStore::new(state.path()).expect("auth");
    store
        .set_oauth(
            "mcp:demo",
            OAuthCredential {
                access: "access".into(),
                refresh: "refresh".into(),
                expires_at_ms: 1,
                account_id: None,
                enterprise_url: None,
            },
        )
        .await
        .expect("stored oauth");

    let expired = auth_status(&config, &store).await.expect("status");
    assert!(expired.enabled);
    assert_eq!(expired.source, Some(McpAuthSource::StoredOAuth));
    assert!(expired.expired);

    let env = BTreeMap::from([(String::from("DEMO_MCP_TOKEN"), String::from("env-token"))]);
    let env_status = auth_status_with_lookup(&config, &store, |key| env.get(key).cloned())
        .await
        .expect("env status");
    assert!(env_status.enabled);
    assert_eq!(env_status.source, Some(McpAuthSource::BearerEnv));
    assert!(!env_status.expired);
}

#[tokio::test]
async fn catalog_persists_safe_references_and_resolves_runtime_config() {
    let state = TempDir::new().expect("state");
    let catalog = McpServerCatalog::new(state.path()).expect("catalog");
    let script = state.path().join("mcp.sh");
    make_script(&script, "#!/bin/sh\nexit 0\n");

    let mut entry = McpCatalogServer::new(
        "linear",
        "Linear",
        McpCatalogStdio {
            program: script.clone(),
            args: vec!["serve".into()],
            env: BTreeMap::from([("LINEAR_TOKEN".into(), "LINEAR_TOKEN_SOURCE".into())]),
            startup_timeout_ms: 500,
            io_timeout_ms: 1_000,
            max_frame_bytes: 64 * 1024,
            max_tool_payload_bytes: 8 * 1024,
            max_stderr_bytes: 4 * 1024,
        },
    )
    .expect("entry");
    entry.bearer_token_env_var = Some("LINEAR_BEARER".into());
    entry
        .header_env
        .insert("Authorization".into(), "LINEAR_HEADER".into());

    catalog.upsert(entry).await.expect("upsert");

    let persisted = std::fs::read_to_string(catalog.path()).expect("catalog file");
    assert!(persisted.contains("LINEAR_TOKEN_SOURCE"));
    assert!(persisted.contains("LINEAR_HEADER"));
    assert!(!persisted.contains("super-secret-token"));

    let runtime = catalog
        .resolve_with_lookup("linear", |key| match key {
            "LINEAR_TOKEN_SOURCE" => Some("super-secret-token".into()),
            "LINEAR_HEADER" => Some("Bearer super-secret-token".into()),
            _ => None,
        })
        .await
        .expect("resolve")
        .expect("runtime");
    assert_eq!(runtime.server, "linear");
    assert_eq!(runtime.label, "Linear");
    assert_eq!(
        runtime.stdio.env.get("LINEAR_TOKEN").map(String::as_str),
        Some("super-secret-token")
    );
    assert_eq!(
        runtime.headers.get("Authorization").map(String::as_str),
        Some("Bearer super-secret-token")
    );
    assert_eq!(
        runtime.bearer_token_env_var.as_deref(),
        Some("LINEAR_BEARER")
    );
}

#[tokio::test]
async fn catalog_enable_disable_remove_are_durable() {
    let state = TempDir::new().expect("state");
    let catalog = McpServerCatalog::new(state.path()).expect("catalog");
    let script = state.path().join("catalog.sh");
    make_script(&script, "#!/bin/sh\nexit 0\n");
    let entry = McpCatalogServer::new(
        "notion",
        "Notion",
        McpCatalogStdio {
            program: script,
            ..McpCatalogStdio::default()
        },
    )
    .expect("entry");

    catalog.upsert(entry).await.expect("upsert");
    assert!(catalog.disable("notion").await.expect("disable"));
    assert!(!catalog.disable("notion").await.expect("idempotent disable"));

    let reopened = McpServerCatalog::new(state.path()).expect("reopen");
    let disabled = reopened.get("notion").await.expect("get").expect("entry");
    assert!(!disabled.enabled);

    assert!(reopened.enable("notion").await.expect("enable"));
    let enabled = reopened
        .get("notion")
        .await
        .expect("get enabled")
        .expect("entry");
    assert!(enabled.enabled);

    assert!(reopened.remove("notion").await.expect("remove"));
    assert!(!reopened.remove("notion").await.expect("remove again"));
    assert!(reopened.get("notion").await.expect("get removed").is_none());
}

#[tokio::test]
async fn auth_coordinator_uses_catalog_provider_ids_and_login_modes() {
    let state = TempDir::new().expect("state");
    let coordinator = McpAuthCoordinator::new(state.path()).expect("coordinator");
    let script = state.path().join("coordinator.sh");
    make_script(&script, "#!/bin/sh\nexit 0\n");

    let mut oauth_entry = McpCatalogServer::new(
        "linear",
        "Linear",
        McpCatalogStdio {
            program: script.clone(),
            ..McpCatalogStdio::default()
        },
    )
    .expect("oauth entry");
    oauth_entry.oauth = true;
    coordinator
        .catalog()
        .upsert(oauth_entry)
        .await
        .expect("upsert oauth");

    let mut api_entry = McpCatalogServer::new(
        "notion",
        "Notion",
        McpCatalogStdio {
            program: script,
            ..McpCatalogStdio::default()
        },
    )
    .expect("api entry");
    api_entry.bearer_token_env_var = Some("NOTION_TOKEN".into());
    coordinator
        .catalog()
        .upsert(api_entry)
        .await
        .expect("upsert api");

    coordinator
        .store_oauth(
            "linear",
            OAuthCredential {
                access: "access".into(),
                refresh: "refresh".into(),
                expires_at_ms: 1,
                account_id: None,
                enterprise_url: None,
            },
        )
        .await
        .expect("store oauth");
    coordinator
        .store_api_key("notion", "notion-secret")
        .await
        .expect("store key");

    let linear = coordinator
        .status("linear")
        .await
        .expect("linear status")
        .expect("linear");
    assert_eq!(linear.auth.provider_id, "mcp:linear");
    assert_eq!(linear.auth.source, Some(McpAuthSource::StoredOAuth));
    assert!(linear.auth.expired);

    let notion = coordinator
        .status("notion")
        .await
        .expect("notion status")
        .expect("notion");
    assert_eq!(notion.auth.provider_id, "mcp:notion");
    assert_eq!(notion.auth.source, Some(McpAuthSource::StoredApiKey));
    assert!(!notion.auth.expired);

    assert!(coordinator.logout("notion").await.expect("logout notion"));
    assert!(
        coordinator
            .store_api_key("linear", "bad")
            .await
            .expect_err("oauth-only server rejects api keys")
            .to_string()
            .contains("requires OAuth")
    );
}

#[tokio::test]
async fn stdio_client_initializes_lists_tools_and_calls_tools() {
    let workspace = TempDir::new().expect("workspace");
    let log_path = workspace.path().join("mcp-log.jsonl");
    let script_path = workspace.path().join("fake-mcp.sh");
    let body = format!(
        r#"#!/bin/sh
set -eu
LOG="{log}"

read_message() {{
  len=""
  while IFS= read -r line; do
    line=$(printf '%s' "$line" | tr -d '\r')
    if [ -z "$line" ]; then
      break
    fi
    case "$line" in
      [Cc]ontent-[Ll]ength:*)
        len=$(printf '%s' "$line" | sed 's/^[^:]*:[ ]*//')
        ;;
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
send_json "{{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{{\"protocolVersion\":\"2025-06-18\",\"capabilities\":{{\"tools\":{{\"listChanged\":false}}}},\"serverInfo\":{{\"name\":\"fake-mcp\",\"version\":\"1.0.0\"}}}}}}"

request=$(read_message)
printf '%s\n' "$request" >> "$LOG"

request=$(read_message)
printf '%s\n' "$request" >> "$LOG"
id=$(printf '%s' "$request" | sed -n 's/.*"id":[ ]*\([0-9][0-9]*\).*/\1/p')
send_json "{{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{{\"tools\":[{{\"name\":\"list_issues\",\"description\":\"List issues\",\"inputSchema\":{{\"type\":\"object\"}}}},{{\"name\":\"notion-search\",\"description\":\"Search Notion\",\"inputSchema\":{{\"type\":\"object\"}}}}]}}}}"

request=$(read_message)
printf '%s\n' "$request" >> "$LOG"
id=$(printf '%s' "$request" | sed -n 's/.*"id":[ ]*\([0-9][0-9]*\).*/\1/p')
send_json "{{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{{\"structuredContent\":{{\"ok\":true,\"tool\":\"notion-search\"}},\"content\":[],\"isError\":false}}}}"
"#,
        log = log_path.display()
    );
    make_script(&script_path, &body);

    let config = McpServerConfig::new("demo", "Demo", stdio_config(script_path)).expect("config");
    let mut client = McpClient::connect(&config).await.expect("client");
    assert_eq!(
        client.server_info(),
        &mimir::mcp::McpServerInfo {
            name: "fake-mcp".into(),
            version: "1.0.0".into(),
        }
    );

    let tools = client.list_tools().await.expect("tools");
    assert_eq!(tools.len(), 2);
    assert_eq!(tools[0].name, "list_issues");
    assert_eq!(tools[0].identifier.as_deref(), Some("list_issues"));
    assert_eq!(tools[1].name, "notion-search");
    assert_eq!(tools[1].identifier, None);

    let output = client
        .call_tool("notion-search", json!({"query":"roadmap"}))
        .await
        .expect("tool call");
    assert_eq!(
        output,
        McpToolCallOutput::Structured(json!({"ok": true, "tool": "notion-search"}))
    );

    let log = std::fs::read_to_string(log_path).expect("log");
    let methods = log
        .lines()
        .map(|line| {
            let value: serde_json::Value = serde_json::from_str(line).expect("request json");
            value["method"].as_str().expect("method").to_owned()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        methods,
        vec![
            "initialize".to_owned(),
            "notifications/initialized".to_owned(),
            "tools/list".to_owned(),
            "tools/call".to_owned(),
        ]
    );
    assert!(log.contains(r#""name":"notion-search""#));
    assert!(log.contains(r#""query":"roadmap""#));
}
