use assert_cmd::Command;
use mimir::{
    extensions::ExtensionCatalog,
    migration::{MigratedRuntimeState, StateMigrator},
    resources::load_migrated_skills,
};
use predicates::prelude::*;
use serde_json::json;
use tempfile::TempDir;

async fn write(root: &TempDir, relative: &str, contents: &str) {
    let path = root.path().join(relative);
    tokio::fs::create_dir_all(path.parent().expect("parent"))
        .await
        .expect("directory");
    tokio::fs::write(path, contents).await.expect("write");
}

fn roots() -> (TempDir, TempDir, TempDir) {
    (
        TempDir::new().expect("legacy"),
        TempDir::new().expect("state"),
        TempDir::new().expect("workspace"),
    )
}

#[tokio::test]
async fn applied_migration_activates_preferences_known_provider_models_and_legacy_skills() {
    let (legacy, state, workspace) = roots();
    write(
        &legacy,
        "settings.json",
        &serde_json::to_string_pretty(&json!({
            "defaultProvider": "openrouter",
            "defaultModel": "vendor/custom-7b",
            "defaultThinkingLevel": "high",
            "extensions": ["extensions/unsafe.ts"],
            "skills": ["skills/reviewer"]
        }))
        .expect("settings"),
    )
    .await;
    write(
        &legacy,
        "models.json",
        &serde_json::to_string_pretty(&json!({
            "providers": {
                "openrouter": {
                    "api": "openai-completions",
                    "baseUrl": "https://router.example.test/v1",
                    "apiKey": "migration-model-secret",
                    "models": [{
                        "id": "vendor/custom-7b",
                        "name": "Custom 7B",
                        "contextWindow": 32768,
                        "maxTokens": 4096,
                        "input": ["text"]
                    }]
                },
                "unsafe-custom-provider": {
                    "api": "openai-completions",
                    "baseUrl": "https://unsafe.example.test/v1",
                    "apiKey": "another-secret",
                    "models": [{"id": "ignored"}]
                }
            }
        }))
        .expect("models"),
    )
    .await;
    write(
        &legacy,
        "skills/reviewer/SKILL.md",
        "---\nname: reviewer\n---\nReview the migration carefully.\n",
    )
    .await;
    write(
        &legacy,
        "extensions/unsafe.ts",
        "export default () => process.env.SECRET;\n",
    )
    .await;

    let migrator = StateMigrator::new();
    let plan = migrator
        .plan(legacy.path(), state.path())
        .await
        .expect("plan");
    migrator.apply(&plan).await.expect("apply");

    let activation = MigratedRuntimeState::load(state.path()).expect("activation");
    let preferences = activation.preferences.as_ref().expect("preferences");
    assert_eq!(preferences.default_provider.as_deref(), Some("openrouter"));
    assert_eq!(
        preferences.default_model.as_deref(),
        Some("vendor/custom-7b")
    );
    let model = activation
        .model("openrouter", "vendor/custom-7b")
        .expect("activated custom model");
    assert_eq!(model.api, "openai-completions");
    assert_eq!(model.base_url, "https://router.example.test/v1");
    assert_eq!(model.context_window, 32_768);
    assert_eq!(model.max_tokens, 4_096);
    assert!(
        activation
            .model("unsafe-custom-provider", "ignored")
            .is_none()
    );
    assert_eq!(
        activation.blocked_model_providers(),
        &["unsafe-custom-provider"]
    );
    let debug = format!("{activation:?}");
    assert!(!debug.contains("migration-model-secret"));
    assert!(!debug.contains("another-secret"));

    let skills = load_migrated_skills(state.path()).expect("migrated skills");
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].name, "reviewer");
    assert_eq!(skills[0].description, "Migrated legacy skill reviewer");
    assert!(skills[0].path.ends_with("skills/reviewer/SKILL.md"));

    assert!(activation.blocked_extensions().is_empty());
    let mut extensions = ExtensionCatalog::new(workspace.path(), state.path()).expect("catalog");
    let discovered = extensions.snapshot().await.expect("extensions");
    assert_eq!(discovered.len(), 1);
    assert_eq!(discovered[0].manifest.name, "migrated-unsafe");
}

#[tokio::test]
async fn activation_fails_closed_on_insecure_migrated_model_endpoint() {
    let (_legacy, state, _workspace) = roots();
    write(
        &state,
        "config/models.json",
        &serde_json::to_string_pretty(&json!({
            "providers": {
                "openrouter": {
                    "api": "openai-completions",
                    "baseUrl": "http://remote.example.test/v1",
                    "models": [{"id": "unsafe"}]
                }
            }
        }))
        .expect("models"),
    )
    .await;
    let error = MigratedRuntimeState::load(state.path()).expect_err("insecure endpoint");
    assert!(error.to_string().contains("HTTPS or loopback HTTP"));
}

#[tokio::test]
async fn safe_custom_openai_provider_activates_with_env_reference_only() {
    let (_legacy, state, _workspace) = roots();
    write(
        &state,
        "config/models.json",
        &serde_json::to_string_pretty(&json!({
            "providers": {
                "local-openai": {
                    "api": "openai-completions",
                    "baseUrl": "http://127.0.0.1:11434/v1",
                    "apiKey": "LOCAL_OPENAI_API_KEY",
                    "compat": {
                        "maxTokensField": "max_tokens",
                        "supportsReasoningEffort": false,
                        "supportsStrictMode": false
                    },
                    "models": [{
                        "id": "qwen-test",
                        "maxTokens": 4096
                    }]
                }
            }
        }))
        .expect("models"),
    )
    .await;

    let activation = MigratedRuntimeState::load(state.path()).expect("activation");
    let provider = activation
        .custom_openai_provider("local-openai")
        .expect("custom provider");
    assert_eq!(provider.credential_env(), "LOCAL_OPENAI_API_KEY");
    let model = activation
        .model("local-openai", "qwen-test")
        .expect("custom model");
    assert_eq!(model.api, "openai-completions");
    assert_eq!(model.max_tokens, 4_096);
    assert_eq!(
        model.compat,
        Some(json!({
            "maxTokensField": "max_tokens",
            "supportsReasoningEffort": false,
            "supportsStrictMode": false
        }))
    );
    assert!(activation.blocked_model_providers().is_empty());
}

#[test]
fn migrated_provider_and_model_defaults_drive_a_new_cli_session() {
    let state = TempDir::new().expect("state");
    let workspace = TempDir::new().expect("workspace");
    std::fs::create_dir_all(state.path().join("config")).expect("config");
    std::fs::write(
        state.path().join("config/preferences.json"),
        serde_json::to_vec_pretty(&json!({
            "schema_version": 1,
            "default_provider": "fake",
            "default_model": "migrated-fake",
            "default_thinking_level": "off"
        }))
        .expect("preferences"),
    )
    .expect("write preferences");

    Command::cargo_bin("mimir")
        .expect("binary")
        .args([
            "--workspace",
            workspace.path().to_str().expect("workspace path"),
            "--state-dir",
            state.path().to_str().expect("state path"),
            "--session",
            "migration-defaults",
            "--fake-response",
            "activated",
            "--print",
            "verify migrated defaults",
            "--no-tui",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("activated"));
}
