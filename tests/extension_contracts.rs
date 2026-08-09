use std::{collections::BTreeSet, os::unix::fs::PermissionsExt, time::Duration};

use serde_json::json;
use tempfile::TempDir;

use mimir::extensions::{
    Capability, ExtensionCatalog, ExtensionManifest, HostLimits, HostRequest, HostResponseStatus,
    JsonLineExtensionHost, RlmLimits, RlmStore,
};

fn manifest_json(
    name: &str,
    version: &str,
    capabilities: &[&str],
    program: &str,
    args: &[&str],
) -> serde_json::Value {
    json!({
        "schema_version": 1,
        "name": name,
        "version": version,
        "entrypoint": {
            "program": program,
            "args": args,
        },
        "capabilities": capabilities,
    })
}

fn write_manifest(root: &std::path::Path, relative: &str, manifest: &serde_json::Value) {
    let path = root.join(relative).join("manifest.json");
    std::fs::create_dir_all(path.parent().expect("manifest parent")).expect("manifest dir");
    std::fs::write(
        path,
        serde_json::to_vec_pretty(&manifest).expect("manifest bytes"),
    )
    .expect("manifest write");
}

fn write_script(root: &TempDir, name: &str, body: &str) -> std::path::PathBuf {
    let path = root.path().join(name);
    std::fs::write(&path, body).expect("script write");
    let mut permissions = std::fs::metadata(&path)
        .expect("script metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&path, permissions).expect("script permissions");
    path
}

#[tokio::test]
async fn catalog_discovers_workspace_and_state_manifests_and_persists_enablement() {
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    write_manifest(
        workspace.path(),
        ".mimir/extensions/echo",
        &manifest_json(
            "echo",
            "2.0.0",
            &["tools", "workspace_read"],
            "/bin/sh",
            &[],
        ),
    );
    write_manifest(
        state.path(),
        "extensions/summarize",
        &manifest_json("summarize", "1.0.0", &["commands", "ui"], "/bin/sh", &[]),
    );
    write_manifest(
        state.path(),
        "extensions/echo",
        &manifest_json("echo", "1.0.0", &["provider"], "/bin/sh", &[]),
    );

    let mut catalog = ExtensionCatalog::new(workspace.path(), state.path()).expect("catalog");
    let entries = catalog.reload().await.expect("reload");
    assert_eq!(entries.len(), 2);
    let echo = entries
        .iter()
        .find(|entry| entry.manifest.name == "echo")
        .expect("echo");
    assert_eq!(echo.manifest.version, "2.0.0");
    assert!(echo.enabled);
    assert!(
        echo.manifest
            .capabilities
            .contains(&Capability::WorkspaceRead)
    );

    catalog.disable("echo").await.expect("disable");
    let mut reopened = ExtensionCatalog::new(workspace.path(), state.path()).expect("catalog");
    let disabled = reopened
        .snapshot()
        .await
        .expect("snapshot")
        .into_iter()
        .find(|entry| entry.manifest.name == "echo")
        .expect("echo entry");
    assert!(!disabled.enabled);

    let mut reopened = ExtensionCatalog::new(workspace.path(), state.path()).expect("catalog");
    reopened.enable("echo").await.expect("enable");
    let enabled = reopened
        .snapshot()
        .await
        .expect("snapshot")
        .into_iter()
        .find(|entry| entry.manifest.name == "echo")
        .expect("echo entry");
    assert!(enabled.enabled);
}

#[tokio::test]
async fn catalog_rejects_unknown_manifest_fields_and_unknown_capabilities() {
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    write_manifest(
        workspace.path(),
        ".mimir/extensions/bad",
        &json!({
            "schema_version": 1,
            "name": "bad",
            "version": "1.0.0",
            "entrypoint": {"program": "/bin/sh", "args": []},
            "capabilities": ["tools", "teleport"],
            "extra": true,
        }),
    );

    let mut catalog = ExtensionCatalog::new(workspace.path(), state.path()).expect("catalog");
    let error = catalog.reload().await.expect_err("invalid manifest");
    assert!(error.to_string().contains("unknown field") || error.to_string().contains("teleport"));
}

#[tokio::test]
async fn host_rejects_invalid_requests_before_spawning() {
    let workspace = TempDir::new().expect("workspace");
    let script_root = TempDir::new().expect("scripts");
    let script = write_script(
        &script_root,
        "echo-extension.sh",
        "#!/bin/sh\nprintf '{\"schema_version\":1,\"id\":\"req-1\",\"status\":\"ok\",\"output\":{\"ok\":true}}\\n'\n",
    );
    let manifest = ExtensionManifest {
        schema_version: 1,
        name: "echo".into(),
        version: "1.0.0".into(),
        entrypoint: mimir::extensions::ExtensionEntrypoint::NativeProcess {
            program: script.display().to_string(),
            args: Vec::new(),
        },
        capabilities: BTreeSet::from([Capability::Tools]),
    };
    let host = JsonLineExtensionHost::new(
        manifest,
        workspace.path(),
        HostLimits {
            max_request_bytes: 256,
            max_response_bytes: 256,
            timeout: Duration::from_secs(1),
        },
    )
    .expect("host");

    let error = host
        .invoke(HostRequest {
            schema_version: 1,
            id: String::new(),
            command: "run".into(),
            payload: json!({"ok": true}),
        })
        .await
        .expect_err("blank id");
    assert!(error.to_string().contains("id"));
}

#[tokio::test]
async fn host_clears_environment_and_parses_json_line_responses() {
    let workspace = TempDir::new().expect("workspace");
    let script_root = TempDir::new().expect("scripts");
    let script = write_script(
        &script_root,
        "env-extension.sh",
        "#!/bin/sh\nread line\nif [ -n \"$HOME\" ]; then\n  printf '{\"schema_version\":1,\"id\":\"req-1\",\"status\":\"error\",\"message\":\"env leak\",\"output\":{}}\\n'\nelse\n  printf '{\"schema_version\":1,\"id\":\"req-1\",\"status\":\"ok\",\"output\":{\"env\":\"cleared\"}}\\n'\nfi\n",
    );
    let manifest = ExtensionManifest {
        schema_version: 1,
        name: "env-check".into(),
        version: "1.0.0".into(),
        entrypoint: mimir::extensions::ExtensionEntrypoint::NativeProcess {
            program: script.display().to_string(),
            args: Vec::new(),
        },
        capabilities: BTreeSet::from([Capability::Commands, Capability::Process]),
    };
    let host = JsonLineExtensionHost::new(manifest, workspace.path(), HostLimits::default())
        .expect("host");

    let response = host
        .invoke(HostRequest {
            schema_version: 1,
            id: "req-1".into(),
            command: "inspect".into(),
            payload: json!({"subject": "workspace"}),
        })
        .await
        .expect("response");

    assert_eq!(response.status, HostResponseStatus::Ok);
    assert_eq!(response.output, json!({"env":"cleared"}));
}

#[tokio::test]
async fn host_enforces_response_size_limits() {
    let workspace = TempDir::new().expect("workspace");
    let script_root = TempDir::new().expect("scripts");
    let script = write_script(
        &script_root,
        "large-extension.sh",
        "#!/bin/sh\nprintf 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\\n'\n",
    );
    let manifest = ExtensionManifest {
        schema_version: 1,
        name: "large".into(),
        version: "1.0.0".into(),
        entrypoint: mimir::extensions::ExtensionEntrypoint::NativeProcess {
            program: script.display().to_string(),
            args: Vec::new(),
        },
        capabilities: BTreeSet::from([Capability::Ui]),
    };
    let host = JsonLineExtensionHost::new(
        manifest,
        workspace.path(),
        HostLimits {
            max_request_bytes: 256,
            max_response_bytes: 24,
            // Keep this contract about the response-size boundary. A one-second
            // process deadline can expire first when the complete workspace suite
            // is running concurrently on a loaded CI host.
            timeout: Duration::from_secs(10),
        },
    )
    .expect("host");

    let error = host
        .invoke(HostRequest {
            schema_version: 1,
            id: "req-1".into(),
            command: "run".into(),
            payload: json!({}),
        })
        .await
        .expect_err("oversized response");
    assert!(error.to_string().contains("response"));
}

#[tokio::test]
async fn rlm_store_persists_namespaced_workspace_state_and_rejects_oversized_values() {
    let state = TempDir::new().expect("state");
    let workspace_a = TempDir::new().expect("workspace a");
    let workspace_b = TempDir::new().expect("workspace b");
    let limits = RlmLimits {
        max_value_bytes: 128,
        max_namespace_bytes: 512,
        max_keys: 8,
    };
    let store_a = RlmStore::new(state.path(), workspace_a.path(), "echo", limits).expect("store");
    let store_b = RlmStore::new(state.path(), workspace_b.path(), "echo", limits).expect("store");

    store_a
        .put("memory", "answer", json!({"value": 42}))
        .await
        .expect("put");
    assert_eq!(
        store_a.get("memory", "answer").await.expect("get"),
        Some(json!({"value": 42}))
    );
    assert_eq!(
        store_a.list("memory").await.expect("list"),
        json!({"answer":{"value":42}})
    );
    assert_eq!(store_b.get("memory", "answer").await.expect("get"), None);

    let reopened = RlmStore::new(state.path(), workspace_a.path(), "echo", limits).expect("store");
    assert_eq!(
        reopened.get("memory", "answer").await.expect("get"),
        Some(json!({"value": 42}))
    );

    let error = reopened
        .put("memory", "too-large", json!("x".repeat(256)))
        .await
        .expect_err("value limit");
    assert!(error.to_string().contains("value"));

    reopened.delete("memory", "answer").await.expect("delete");
    assert_eq!(reopened.get("memory", "answer").await.expect("get"), None);
}
