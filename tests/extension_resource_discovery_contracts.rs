use std::{collections::BTreeSet, time::Duration};

use mimir::extensions::{
    Capability, CatalogEntry, ExtensionEntrypoint, ExtensionManager, ExtensionManifest, HostLimits,
    ManifestSource, ResourceDiscoveryReason, RuntimeLimits,
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
async fn resource_discovery_resolves_extension_relative_paths() {
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    let extension = TempDir::new().expect("extension");
    let skill = extension.path().join("skills/example");
    std::fs::create_dir_all(&skill).expect("skill root");
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: example\ndescription: example\n---\nbody",
    )
    .expect("skill");
    let module = extension.path().join("resources.ts");
    std::fs::write(
        &module,
        r#"
export default function activate(pi) {
  pi.on("resources_discover", () => ({ skillPaths: ["skills"] }));
}
"#,
    )
    .expect("module");
    let manifest = ExtensionManifest {
        schema_version: 1,
        name: "resource-provider".into(),
        version: "1.0.0".into(),
        entrypoint: ExtensionEntrypoint::EmbeddedJavaScript {
            module: module.display().to_string(),
        },
        capabilities: BTreeSet::from([Capability::Lifecycle]),
    };
    let manager = ExtensionManager::load(
        vec![CatalogEntry {
            manifest,
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
    let discovered = manager
        .discover_resources(ResourceDiscoveryReason::Startup)
        .await
        .expect("resources");
    assert_eq!(
        discovered.skill_paths,
        [std::fs::canonicalize(extension.path().join("skills")).expect("canonical skill")]
    );
    assert!(discovered.prompt_paths.is_empty());
    assert!(discovered.theme_paths.is_empty());
}
