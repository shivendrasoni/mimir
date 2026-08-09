use mimir::extensions::{ExtensionCatalog, ExtensionPackageManager};
use serde_json::json;
use tempfile::TempDir;

#[tokio::test]
async fn local_extension_packages_install_list_update_and_remove_recoverably() {
    let source = TempDir::new().expect("source");
    let state = TempDir::new().expect("state");
    let workspace = TempDir::new().expect("workspace");
    std::fs::write(
        source.path().join("package.json"),
        serde_json::to_vec_pretty(&json!({
            "name": "safe-local-extension",
            "version": "1.0.0",
            "pi": {"extensions": ["./index.ts"]},
            "scripts": {"postinstall": "must-not-run"}
        }))
        .expect("package json"),
    )
    .expect("manifest");
    std::fs::write(
        source.path().join("index.ts"),
        "export default pi => pi.registerCommand('local-one', { handler: async () => ({ output: null }) });",
    )
    .expect("entry");

    let packages = ExtensionPackageManager::new(state.path()).expect("package manager");
    let installed = packages
        .install_local(source.path())
        .await
        .expect("install local package");
    assert_eq!(installed.name, "safe-local-extension");
    assert_eq!(installed.version, "1.0.0");
    assert_eq!(packages.list().await.expect("list").len(), 1);

    let mut catalog = ExtensionCatalog::new(workspace.path(), state.path()).expect("catalog");
    let entries = catalog.snapshot().await.expect("catalog snapshot");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].manifest.name, "safe-local-extension");

    std::fs::write(
        source.path().join("package.json"),
        serde_json::to_vec_pretty(&json!({
            "name": "safe-local-extension",
            "version": "1.1.0",
            "pi": {"extensions": ["./index.ts"]}
        }))
        .expect("package json"),
    )
    .expect("manifest update");
    let updated = packages
        .update("safe-local-extension")
        .await
        .expect("update package");
    assert_eq!(updated.version, "1.1.0");

    let removed = packages
        .remove("safe-local-extension")
        .await
        .expect("remove package")
        .expect("installed package");
    assert!(removed.recovery_path.is_dir());
    assert!(packages.list().await.expect("empty list").is_empty());
    assert!(catalog.snapshot().await.expect("empty catalog").is_empty());
}

#[tokio::test]
async fn package_install_rejects_symlinks_and_non_local_sources() {
    let source = TempDir::new().expect("source");
    let state = TempDir::new().expect("state");
    std::fs::write(
        source.path().join("package.json"),
        r#"{"name":"unsafe-extension","version":"1.0.0","pi":{"extensions":["index.ts"]}}"#,
    )
    .expect("manifest");
    std::os::unix::fs::symlink("/etc/passwd", source.path().join("index.ts"))
        .expect("fixture symlink");
    let packages = ExtensionPackageManager::new(state.path()).expect("package manager");
    let error = packages
        .install_local(source.path())
        .await
        .expect_err("symlink must fail closed");
    assert!(error.to_string().contains("symlink"));

    let error = packages
        .install("npm:unsafe-extension")
        .await
        .expect_err("remote install must be explicit");
    assert!(error.to_string().contains("local filesystem"));
}
