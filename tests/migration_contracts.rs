use std::path::Path;

use mimir::migration::{MigrationAction, StateMigrator};
use serde_json::{Value, json};
use tempfile::TempDir;

#[tokio::test]
async fn dry_run_plan_redacts_secrets_and_describes_inputs() {
    let (legacy, state) = fixture_roots();
    write_json(
        legacy.path(),
        "auth.json",
        &json!({
            "providers": {
                "anthropic": {"type": "api_key", "key": "sk-secret"},
                "openai": {
                    "type": "oauth",
                    "access": "oa-access",
                    "refresh": "oa-refresh",
                    "expires_at_ms": 42
                }
            }
        }),
    )
    .await;
    write_json(
        legacy.path(),
        "settings.json",
        &json!({
            "defaultProvider": "anthropic",
            "extensions": ["./extensions/local.ts"],
            "packages": ["pi-skills"]
        }),
    )
    .await;
    write_json(
        legacy.path(),
        "models.json",
        &json!({"providers": {"ollama": {"baseUrl": "http://localhost:11434/v1"}}}),
    )
    .await;
    write_lines(
        legacy.path(),
        "sessions/demo.jsonl",
        &[
            json!({"type":"session","version":3,"id":"demo","timestamp":"2026-08-07T10:00:00Z","cwd":"/tmp/project"}).to_string(),
            json!({"type":"message","id":"m1","parentId":null,"timestamp":"2026-08-07T10:00:01Z","message":{"role":"user","content":"hello","timestamp":1000}}).to_string(),
            json!({"type":"message","id":"m2","parentId":"m1","timestamp":"2026-08-07T10:00:02Z","message":{"role":"assistant","content":[{"type":"text","text":"world"}],"stopReason":"stop","timestamp":2000}}).to_string(),
        ],
    )
    .await;

    let plan = StateMigrator::new()
        .plan(legacy.path(), state.path())
        .await
        .expect("plan should succeed");

    assert!(plan.actionable_steps() >= 4);
    assert!(plan.report.redacted);
    assert_eq!(plan.report.auth.len(), 2);
    assert!(
        plan.report
            .auth
            .iter()
            .any(|entry| entry.provider == "anthropic")
    );
    let report_json = serde_json::to_string(&plan.report).expect("report should serialize");
    assert!(!report_json.contains("sk-secret"));
    assert!(!report_json.contains("oa-access"));
    assert!(!state.path().join("auth.json").exists());
}

#[tokio::test]
async fn apply_writes_state_and_repeated_apply_is_idempotent() {
    let (legacy, state) = fixture_roots();
    write_json(
        legacy.path(),
        "auth.json",
        &json!({"providers":{"anthropic":{"type":"api_key","key":"sk-live"}}}),
    )
    .await;
    write_json(
        legacy.path(),
        "settings.json",
        &json!({"extensions":["./ext.ts"],"packages":["pkg-a"]}),
    )
    .await;
    write_json(
        legacy.path(),
        "models.json",
        &json!({"providers":{"custom":{"baseUrl":"https://example.invalid"}}}),
    )
    .await;
    write_json(
        legacy.path(),
        "inventory.json",
        &json!({"extensions":["./ext.ts"],"packages":["pkg-a","pkg-b"]}),
    )
    .await;
    write_lines(
        legacy.path(),
        "sessions/demo.jsonl",
        &[
            json!({"type":"session","version":2,"id":"demo","timestamp":"2026-08-07T10:00:00Z","cwd":"/tmp/project"}).to_string(),
            json!({"type":"message","id":"m1","parentId":null,"timestamp":"2026-08-07T10:00:01Z","message":{"role":"user","content":"hello","timestamp":1000}}).to_string(),
            json!({"type":"compaction","id":"m2","parentId":"m1","timestamp":"2026-08-07T10:00:02Z","summary":"summary"}).to_string(),
        ],
    )
    .await;

    let migrator = StateMigrator::new();
    let plan = migrator
        .plan(legacy.path(), state.path())
        .await
        .expect("plan should succeed");
    let first = migrator.apply(&plan).await.expect("apply should succeed");
    let journal_path = first.journal_path.clone().expect("journal path");
    let auth: Value = read_json(state.path().join("auth.json")).await;
    assert_eq!(auth["anthropic"]["type"], "api_key");
    let inventory: Value = read_json(state.path().join("config/inventory.json")).await;
    assert_eq!(inventory["packages"].as_array().expect("packages").len(), 2);
    let session_content = tokio::fs::read_to_string(state.path().join("sessions/demo.jsonl"))
        .await
        .expect("session should exist");
    assert_eq!(session_content.lines().count(), 2);

    let repeated = migrator
        .apply(&plan)
        .await
        .expect("reusing an applied plan should no-op");
    assert!(repeated.journal_path.is_none());
    assert_eq!(repeated.applied_steps, 0);

    let second_plan = migrator
        .plan(legacy.path(), state.path())
        .await
        .expect("second plan should succeed");
    assert!(
        second_plan
            .steps
            .iter()
            .all(|step| step.action == MigrationAction::Unchanged)
    );
    let second = migrator
        .apply(&second_plan)
        .await
        .expect("second apply should no-op");
    assert!(second.journal_path.is_none());

    let journal: Value = read_json(journal_path).await;
    assert_eq!(journal["status"], "applied");
}

#[tokio::test]
async fn rollback_restores_backed_up_state() {
    let (legacy, state) = fixture_roots();
    write_json(
        legacy.path(),
        "auth.json",
        &json!({"providers":{"anthropic":{"type":"api_key","key":"sk-new"}}}),
    )
    .await;
    write_json(
        state.path(),
        "auth.json",
        &json!({"anthropic":{"type":"api_key","key":"sk-old"}}),
    )
    .await;

    let migrator = StateMigrator::new();
    let plan = migrator
        .plan(legacy.path(), state.path())
        .await
        .expect("plan should succeed");
    let applied = migrator.apply(&plan).await.expect("apply should succeed");
    let journal_path = applied.journal_path.expect("journal path");
    let rollback = migrator
        .rollback(&journal_path)
        .await
        .expect("rollback should succeed");

    assert_eq!(rollback.restored_steps, 1);
    let restored: Value = read_json(state.path().join("auth.json")).await;
    assert_eq!(restored["anthropic"]["key"], "sk-old");
    let journal: Value = read_json(journal_path).await;
    assert_eq!(journal["status"], "rolled_back");
}

#[tokio::test]
async fn migration_rejects_symlinked_source_entries() {
    let (legacy, state) = fixture_roots();
    let outside = TempDir::new().expect("outside tempdir");
    write_json(
        outside.path(),
        "outside-auth.json",
        &json!({"providers":{"anthropic":{"type":"api_key","key":"sk-outside"}}}),
    )
    .await;
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(
            outside.path().join("outside-auth.json"),
            legacy.path().join("auth.json"),
        )
        .expect("symlink should be created");
        let error = StateMigrator::new()
            .plan(legacy.path(), state.path())
            .await
            .expect_err("symlink should be rejected");
        assert!(error.to_string().contains("symlink"));
    }
}

#[cfg(unix)]
#[tokio::test]
async fn migrated_credentials_are_owner_readable_only() {
    use std::os::unix::fs::PermissionsExt;

    let (legacy, state) = fixture_roots();
    write_json(
        legacy.path(),
        "auth.json",
        &json!({"providers":{"openai":{"type":"api_key","key":"sk-private"}}}),
    )
    .await;

    let migrator = StateMigrator::new();
    let plan = migrator
        .plan(legacy.path(), state.path())
        .await
        .expect("plan");
    migrator.apply(&plan).await.expect("apply");

    let mode = tokio::fs::metadata(state.path().join("auth.json"))
        .await
        .expect("auth metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);
}

#[tokio::test]
async fn invalid_session_schema_fails_closed() {
    let (legacy, state) = fixture_roots();
    write_lines(
        legacy.path(),
        "sessions/demo.jsonl",
        &[
            json!({"type":"session","version":99,"id":"demo"}).to_string(),
            json!({"type":"message","id":"m1","message":{"role":"user","content":"hello"}})
                .to_string(),
        ],
    )
    .await;

    let error = StateMigrator::new()
        .plan(legacy.path(), state.path())
        .await
        .expect_err("invalid schema should fail");
    assert!(
        error
            .to_string()
            .contains("unsupported legacy session header")
    );
}

#[tokio::test]
async fn apply_rejects_legacy_state_changed_after_planning() {
    let (legacy, state) = fixture_roots();
    write_json(
        legacy.path(),
        "auth.json",
        &json!({"providers":{"openai":{"type":"api_key","key":"sk-first"}}}),
    )
    .await;
    let migrator = StateMigrator::new();
    let plan = migrator
        .plan(legacy.path(), state.path())
        .await
        .expect("plan");
    write_json(
        legacy.path(),
        "auth.json",
        &json!({"providers":{"openai":{"type":"api_key","key":"sk-second"}}}),
    )
    .await;

    let error = migrator
        .apply(&plan)
        .await
        .expect_err("changed source must be rejected");
    assert!(error.to_string().contains("changed after planning"));
    assert!(!state.path().join("auth.json").exists());
}

#[tokio::test]
async fn migrates_versioned_preferences_catalog_and_resource_metadata_losslessly() {
    let (legacy, state) = fixture_roots();
    let settings = concat!(
        "{\n",
        "  \"defaultProvider\": \"anthropic\",\n",
        "  \"defaultModel\": \"claude-sonnet-4-6\",\n",
        "  \"recentModels\": [\"anthropic/claude-sonnet-4-6\"],\n",
        "  \"enabledModels\": [\"anthropic/*\"],\n",
        "  \"defaultThinkingLevel\": \"high\",\n",
        "  \"defaultServiceTier\": \"priority\",\n",
        "  \"transport\": \"sse\",\n",
        "  \"packages\": [{\"source\": \"pkg-a\", \"skills\": [\"skills/**\"]}],\n",
        "  \"extensions\": [\"extensions/local.ts\"],\n",
        "  \"skills\": [\"skills/demo\"],\n",
        "  \"futureSetting\": {\"preserve\": true}\n",
        "}\n",
    );
    write_text(legacy.path(), "settings.json", settings).await;
    let models = concat!(
        "{\n",
        "  // models.json intentionally supports comments\n",
        "  \"providers\": {\n",
        "    \"local\": {\n",
        "      \"name\": \"Local Provider\",\n",
        "      \"api\": \"openai-completions\",\n",
        "      \"apiKey\": \"model-config-secret\",\n",
        "      \"headers\": {\"Authorization\": \"Bearer header-secret\"},\n",
        "      \"models\": [{\"id\": \"local-7b\"}],\n",
        "      \"modelOverrides\": {\"built-in\": {\"maxTokens\": 2048}},\n",
        "    },\n",
        "  },\n",
        "}\n",
    );
    write_text(legacy.path(), "models.json", models).await;
    write_text(
        legacy.path(),
        "extensions/local.ts",
        "export default function local() {}\n",
    )
    .await;
    write_text(
        legacy.path(),
        "skills/demo/SKILL.md",
        "---\nname: demo\n---\nDo the demo.\n",
    )
    .await;

    let migrator = StateMigrator::new();
    let plan = migrator
        .plan(legacy.path(), state.path())
        .await
        .expect("representative legacy state should plan");
    let report = serde_json::to_string(&plan.report).expect("report");
    assert!(!report.contains("model-config-secret"));
    assert!(!report.contains("header-secret"));
    assert!(
        plan.report
            .compatibility
            .unrepresentable
            .contains(&"settings.futureSetting".to_owned())
    );
    assert_eq!(plan.report.inventory.as_ref().expect("inventory").skills, 1);

    migrator.apply(&plan).await.expect("apply");
    let preferences: Value = read_json(state.path().join("config/preferences.json")).await;
    assert_eq!(preferences["schema_version"], 1);
    assert_eq!(preferences["default_provider"], "anthropic");
    assert_eq!(preferences["default_model"], "claude-sonnet-4-6");
    assert_eq!(preferences["default_thinking_level"], "high");

    let catalog: Value = read_json(state.path().join("config/model-catalog.json")).await;
    assert_eq!(catalog["schema_version"], 1);
    assert_eq!(catalog["providers"][0]["id"], "local");
    assert_eq!(catalog["providers"][0]["models"][0], "local-7b");
    let catalog_text = serde_json::to_string(&catalog).expect("catalog text");
    assert!(!catalog_text.contains("model-config-secret"));
    assert!(!catalog_text.contains("header-secret"));

    let inventory: Value = read_json(state.path().join("config/inventory.json")).await;
    assert_eq!(inventory["schema_version"], 1);
    assert_eq!(inventory["packages"].as_array().expect("packages").len(), 1);
    assert_eq!(
        inventory["extensions"]
            .as_array()
            .expect("extensions")
            .len(),
        1
    );
    assert_eq!(inventory["skills"].as_array().expect("skills").len(), 1);
    assert_eq!(
        inventory["discovered"]["extensions"][0],
        "extensions/local.ts"
    );
    assert_eq!(inventory["discovered"]["skills"][0], "skills/demo/SKILL.md");

    assert_eq!(
        tokio::fs::read_to_string(
            state
                .path()
                .join("migration/compatibility/v1/settings.json")
        )
        .await
        .expect("settings archive"),
        settings
    );
    assert_eq!(
        tokio::fs::read_to_string(state.path().join("migration/compatibility/v1/models.json"))
            .await
            .expect("models archive"),
        models
    );
    assert_eq!(
        tokio::fs::read_to_string(
            state
                .path()
                .join("migration/compatibility/v1/resources/extensions/local.ts")
        )
        .await
        .expect("extension archive"),
        "export default function local() {}\n"
    );
    assert_eq!(
        tokio::fs::read_to_string(
            state
                .path()
                .join("migration/compatibility/v1/resources/skills/demo/SKILL.md")
        )
        .await
        .expect("skill archive"),
        "---\nname: demo\n---\nDo the demo.\n"
    );
}

#[tokio::test]
async fn migration_fails_closed_on_oversized_or_symlinked_resource_inputs() {
    let (legacy, state) = fixture_roots();
    write_text(
        legacy.path(),
        "settings.json",
        &format!("{{\"future\":\"{}\"}}", "x".repeat(16 * 1024 * 1024)),
    )
    .await;
    let error = StateMigrator::new()
        .plan(legacy.path(), state.path())
        .await
        .expect_err("oversized settings must fail");
    assert!(error.to_string().contains("size limit"));

    #[cfg(unix)]
    {
        let (legacy, state) = fixture_roots();
        let outside = TempDir::new().expect("outside");
        write_text(outside.path(), "external.ts", "export default 1;\n").await;
        tokio::fs::create_dir_all(legacy.path().join("extensions"))
            .await
            .expect("extensions directory");
        std::os::unix::fs::symlink(
            outside.path().join("external.ts"),
            legacy.path().join("extensions/linked.ts"),
        )
        .expect("resource symlink");
        let error = StateMigrator::new()
            .plan(legacy.path(), state.path())
            .await
            .expect_err("resource symlink must fail");
        assert!(error.to_string().contains("symlink"));
    }
}

#[tokio::test]
async fn rollback_restores_versioned_metadata_and_removes_created_archives_idempotently() {
    let (legacy, state) = fixture_roots();
    write_json(
        legacy.path(),
        "settings.json",
        &json!({
            "defaultProvider": "anthropic",
            "packages": ["pkg-new"],
            "extensions": ["extensions/new.ts"],
            "skills": ["skills/new"]
        }),
    )
    .await;
    write_json(
        state.path(),
        "config/preferences.json",
        &json!({"schema_version":1,"default_provider":"openai"}),
    )
    .await;
    let old_preferences = tokio::fs::read(state.path().join("config/preferences.json"))
        .await
        .expect("old preferences");

    let migrator = StateMigrator::new();
    let plan = migrator
        .plan(legacy.path(), state.path())
        .await
        .expect("plan");
    let applied = migrator.apply(&plan).await.expect("apply");
    let journal_path = applied.journal_path.expect("journal");
    assert!(
        state
            .path()
            .join("migration/compatibility/v1/settings.json")
            .exists()
    );

    let first = migrator.rollback(&journal_path).await.expect("rollback");
    assert!(first.restored_steps > 0);
    assert_eq!(
        tokio::fs::read(state.path().join("config/preferences.json"))
            .await
            .expect("restored preferences"),
        old_preferences
    );
    assert!(
        !state
            .path()
            .join("migration/compatibility/v1/settings.json")
            .exists()
    );
    let second = migrator
        .rollback(&journal_path)
        .await
        .expect("repeated rollback");
    assert_eq!(second.restored_steps, 0);
}

#[tokio::test]
async fn imports_the_reference_flat_auth_shape_and_oauth_expiry_name() {
    let (legacy, state) = fixture_roots();
    write_json(
        legacy.path(),
        "auth.json",
        &json!({
            "anthropic": {"type":"api_key","key":"sk-flat"},
            "openai-codex": {
                "type":"oauth",
                "access":"access-secret",
                "refresh":"refresh-secret",
                "expires":1234
            }
        }),
    )
    .await;

    let migrator = StateMigrator::new();
    let plan = migrator
        .plan(legacy.path(), state.path())
        .await
        .expect("plan");
    assert_eq!(plan.report.auth.len(), 2);
    assert!(
        !serde_json::to_string(&plan.report)
            .expect("report")
            .contains("sk-flat")
    );
    migrator.apply(&plan).await.expect("apply");
    let auth: Value = read_json(state.path().join("auth.json")).await;
    assert_eq!(auth["anthropic"]["type"], "api_key");
    assert_eq!(auth["openai-codex"]["expires_at_ms"], 1234);
}

#[tokio::test]
async fn rollback_rejects_a_journal_outside_its_declared_state_root_before_mutation() {
    let (legacy, state) = fixture_roots();
    let outside = TempDir::new().expect("outside");
    write_json(
        outside.path(),
        "forged.json",
        &json!({
            "journal_id":"forged",
            "legacy_root":legacy.path(),
            "state_root":state.path(),
            "status":"applied",
            "created_at_rfc3339":"2026-08-07T10:00:00Z",
            "steps":[{
                "kind":"settings",
                "target":"config/preferences.json",
                "sha256":"00",
                "backup":null
            }]
        }),
    )
    .await;
    write_text(state.path(), "config/preferences.json", "keep-me\n").await;

    let error = StateMigrator::new()
        .rollback(&outside.path().join("forged.json"))
        .await
        .expect_err("outside journal must fail closed");
    assert!(error.to_string().contains("state root"));
    assert_eq!(
        tokio::fs::read_to_string(state.path().join("config/preferences.json"))
            .await
            .expect("protected state"),
        "keep-me\n"
    );
}

fn fixture_roots() -> (TempDir, TempDir) {
    (
        TempDir::new().expect("legacy root"),
        TempDir::new().expect("state root"),
    )
}

async fn write_json(root: &Path, relative: &str, value: &Value) {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .expect("parent should be created");
    }
    let mut bytes = serde_json::to_vec_pretty(value).expect("json bytes");
    bytes.push(b'\n');
    tokio::fs::write(path, bytes)
        .await
        .expect("json should write");
}

async fn write_lines(root: &Path, relative: &str, lines: &[String]) {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .expect("parent should be created");
    }
    tokio::fs::write(path, format!("{}\n", lines.join("\n")))
        .await
        .expect("lines should write");
}

async fn write_text(root: &Path, relative: &str, value: &str) {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .expect("parent should be created");
    }
    tokio::fs::write(path, value)
        .await
        .expect("text should write");
}

async fn read_json<T: serde::de::DeserializeOwned>(path: impl AsRef<Path>) -> T {
    let bytes = tokio::fs::read(path).await.expect("json should read");
    serde_json::from_slice(&bytes).expect("json should parse")
}
