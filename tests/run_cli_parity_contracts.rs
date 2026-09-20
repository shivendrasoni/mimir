use std::{fs, path::Path, process::Command, thread, time::Duration};

use serde_json::Value;
use tempfile::TempDir;

use mimir::learning::project_session_root;

fn binary() -> Command {
    Command::new(assert_cmd::cargo::cargo_bin!("mimir"))
}

fn fake_run(workspace: &Path, state: &Path, session: &str, prompt: &str) {
    let output = binary()
        .args([
            "--provider",
            "fake",
            "--workspace",
            workspace.to_str().expect("workspace"),
            "--state-dir",
            state.to_str().expect("state"),
            "--session",
            session,
            "--fake-response",
            "ok",
            "--print",
            prompt,
        ])
        .output()
        .expect("run binary");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn project_sessions(state: &Path, workspace: &Path) -> std::path::PathBuf {
    project_session_root(state, workspace)
        .expect("project session root")
        .join("sessions")
}

#[test]
fn help_exposes_reference_run_flags_and_rejects_unsafe_combinations() {
    let help = binary().arg("--help").output().expect("help");
    assert!(help.status.success());
    let help = String::from_utf8_lossy(&help.stdout);
    for flag in [
        "--cwd",
        "--api-key",
        "--thinking",
        "--continue",
        "--resume",
        "--fork",
        "--session-dir",
        "--no-session",
        "--tools",
        "--system-prompt",
        "--append-system-prompt",
        "--models",
        "--autonomous-max-turns",
        "--goal-token-budget",
        "--offline",
        "--verbose",
        "--socket",
        "--skill",
        "--prompt-template",
        "--theme",
        "--no-prompt-templates",
        "--no-themes",
    ] {
        assert!(help.contains(flag), "missing {flag}");
    }

    let invalid = binary()
        .args(["--autonomous-max-turns", "0"])
        .output()
        .expect("invalid numeric flag");
    assert!(!invalid.status.success());
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("positive integer"));

    let socket = binary()
        .args(["--socket", "/tmp/mimir.sock"])
        .output()
        .expect("unsupported socket");
    assert!(!socket.status.success());
    assert!(String::from_utf8_lossy(&socket.stderr).contains("requires a prompt"));

    let missing_allowlist = binary()
        .args(["--allow-process", "--provider", "fake", "--print", "hello"])
        .output()
        .expect("missing process allowlist");
    assert!(!missing_allowlist.status.success());
    assert!(
        String::from_utf8_lossy(&missing_allowlist.stderr)
            .contains("requires an explicit non-empty --allowed-programs allowlist")
    );

    let missing_authorization = binary()
        .args([
            "--allowed-programs",
            "printf",
            "--provider",
            "fake",
            "--print",
            "hello",
        ])
        .output()
        .expect("missing process authorization");
    assert!(!missing_authorization.status.success());
    assert!(
        String::from_utf8_lossy(&missing_authorization.stderr)
            .contains("--allowed-programs requires --allow-process")
    );
}

#[test]
fn operational_limit_flags_and_environment_accept_unlimited() {
    let flags = binary()
        .args([
            "--max-turns",
            "unlimited",
            "--max-run-tokens",
            "unlimited",
            "--autonomous-max-continuations",
            "unlimited",
            "--autonomous-max-turns",
            "unlimited",
            "--autonomous-max-tokens",
            "unlimited",
            "--autonomous-timeout-ms",
            "unlimited",
            "--help",
        ])
        .output()
        .expect("unlimited flags");
    assert!(
        flags.status.success(),
        "{}",
        String::from_utf8_lossy(&flags.stderr)
    );

    let environment = binary()
        .env("MIMIR_MAX_TURNS", "unlimited")
        .env("MIMIR_MAX_RUN_TOKENS", "unlimited")
        .arg("--help")
        .output()
        .expect("unlimited environment");
    assert!(
        environment.status.success(),
        "{}",
        String::from_utf8_lossy(&environment.stderr)
    );
}

#[test]
fn process_tool_selection_requires_explicit_authorization_and_an_exact_allowlist() {
    let workspace = TempDir::new().expect("workspace");
    let state = workspace.path().join("state");
    let common = [
        "--provider",
        "fake",
        "--workspace",
        workspace.path().to_str().expect("workspace"),
        "--state-dir",
        state.to_str().expect("state"),
        "--tools",
        "run_process",
        "--fake-response",
        "done",
        "--print",
        "hello",
    ];

    let denied = binary()
        .args(common)
        .output()
        .expect("default process selection");
    assert!(!denied.status.success());
    assert!(
        String::from_utf8_lossy(&denied.stderr).contains("unknown tool selection: run_process")
    );

    let authorized = binary()
        .args(common)
        .args(["--allow-process", "--allowed-programs", "printf"])
        .output()
        .expect("authorized process selection");
    assert!(
        authorized.status.success(),
        "{}",
        String::from_utf8_lossy(&authorized.stderr)
    );
}

#[test]
fn no_session_api_key_and_cwd_alias_do_not_persist_or_echo_secrets() {
    let workspace = TempDir::new().expect("workspace");
    let state = workspace.path().join("state");
    let secret = "run-only-super-secret";
    let output = binary()
        .current_dir(workspace.path())
        .args([
            "--provider",
            "fake",
            "--cwd",
            ".",
            "--state-dir",
            state.to_str().expect("state"),
            "--no-session",
            "--api-key",
            secret,
            "--fake-response",
            "done",
            "--print",
            "hello",
        ])
        .output()
        .expect("run binary");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!combined.contains(secret));
    assert!(!state.join("sessions/default.jsonl").exists());
}

#[test]
fn explicit_session_directory_uses_reference_direct_file_layout() {
    let workspace = TempDir::new().expect("workspace");
    let state = workspace.path().join("state");
    let sessions = workspace.path().join("custom-sessions");
    let output = binary()
        .args([
            "--provider",
            "fake",
            "--workspace",
            workspace.path().to_str().expect("workspace"),
            "--state-dir",
            state.to_str().expect("state"),
            "--session-dir",
            sessions.to_str().expect("sessions"),
            "--session",
            "custom",
            "--fake-response",
            "done",
            "--print",
            "hello",
        ])
        .output()
        .expect("run binary");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(sessions.join("custom.jsonl").is_file());
    assert!(!sessions.join("sessions/custom.jsonl").exists());
}

#[test]
fn continue_resume_and_fork_select_durable_sessions() {
    let workspace = TempDir::new().expect("workspace");
    let state = workspace.path().join("state");
    fake_run(workspace.path(), &state, "alpha", "alpha-one");
    thread::sleep(Duration::from_millis(20));
    fake_run(workspace.path(), &state, "beta", "beta-one");

    let continued = binary()
        .args([
            "--provider",
            "fake",
            "--workspace",
            workspace.path().to_str().expect("workspace"),
            "--state-dir",
            state.to_str().expect("state"),
            "--continue",
            "--fake-response",
            "continued",
            "--print",
            "beta-two",
        ])
        .output()
        .expect("continue");
    assert!(
        continued.status.success(),
        "{}",
        String::from_utf8_lossy(&continued.stderr)
    );
    let sessions = project_sessions(&state, workspace.path());
    let beta = fs::read_to_string(sessions.join("beta.jsonl")).expect("beta transcript");
    assert!(beta.contains("beta-two"));

    let resumed = binary()
        .args([
            "--provider",
            "fake",
            "--workspace",
            workspace.path().to_str().expect("workspace"),
            "--state-dir",
            state.to_str().expect("state"),
            "--resume",
            "alp",
            "--fake-response",
            "resumed",
            "--print",
            "alpha-two",
        ])
        .output()
        .expect("resume");
    assert!(
        resumed.status.success(),
        "{}",
        String::from_utf8_lossy(&resumed.stderr)
    );

    let forked = binary()
        .args([
            "--provider",
            "fake",
            "--workspace",
            workspace.path().to_str().expect("workspace"),
            "--state-dir",
            state.to_str().expect("state"),
            "--fork",
            "alpha",
            "--fake-response",
            "forked",
            "--print",
            "branch",
        ])
        .output()
        .expect("fork");
    assert!(
        forked.status.success(),
        "{}",
        String::from_utf8_lossy(&forked.stderr)
    );
    let fork = fs::read_dir(sessions)
        .expect("sessions")
        .filter_map(Result::ok)
        .find(|entry| entry.file_name().to_string_lossy().starts_with("fork-"))
        .expect("fork transcript");
    let fork = fs::read_to_string(fork.path()).expect("fork body");
    assert!(fork.contains("alpha-one"));
    assert!(fork.contains("branch"));
    assert!(fork.contains("session_forked_from"));
}

#[test]
fn bare_resume_selects_the_latest_durable_session() {
    let workspace = TempDir::new().expect("workspace");
    let state = workspace.path().join("state");
    fake_run(workspace.path(), &state, "alpha", "alpha-one");
    thread::sleep(Duration::from_millis(20));
    fake_run(workspace.path(), &state, "beta", "beta-one");
    let resumed = binary()
        .args([
            "--provider",
            "fake",
            "--workspace",
            workspace.path().to_str().expect("workspace"),
            "--state-dir",
            state.to_str().expect("state"),
            "--resume",
            "--fake-response",
            "resumed",
            "--print",
            "latest-session",
        ])
        .output()
        .expect("bare resume");
    assert!(
        resumed.status.success(),
        "{}",
        String::from_utf8_lossy(&resumed.stderr)
    );
    let beta = fs::read_to_string(project_sessions(&state, workspace.path()).join("beta.jsonl"))
        .expect("beta transcript");
    assert!(beta.contains("latest-session"));
}

#[test]
fn positional_dash_and_file_prompts_are_composed_safely() {
    let workspace = TempDir::new().expect("workspace");
    let state = workspace.path().join("state");
    let positional = binary()
        .args([
            "--provider",
            "fake",
            "--workspace",
            workspace.path().to_str().expect("workspace"),
            "--state-dir",
            state.to_str().expect("state"),
            "--no-session",
            "--fake-response",
            "positional-ok",
            "hello",
            "world",
        ])
        .output()
        .expect("positional prompt");
    assert!(
        positional.status.success(),
        "{}",
        String::from_utf8_lossy(&positional.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&positional.stdout),
        "positional-ok\n"
    );

    let attachment = workspace.path().join("notes.txt");
    fs::write(&attachment, "bounded attachment").expect("attachment");
    let output = binary()
        .args([
            "--provider",
            "fake",
            "--workspace",
            workspace.path().to_str().expect("workspace"),
            "--state-dir",
            state.to_str().expect("state"),
            "--session",
            "composed",
            "--fake-response",
            "done",
            "--print",
            "",
            "@notes.txt",
            "explain",
            "this",
            "--",
            "- carefully",
        ])
        .output()
        .expect("composed prompt");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let transcript =
        fs::read_to_string(project_sessions(&state, workspace.path()).join("composed.jsonl"))
            .expect("transcript");
    assert!(transcript.contains("bounded attachment"));
    assert!(transcript.contains("&lt;") || transcript.contains("<file"));
    assert!(transcript.contains("explain this - carefully"));
}

#[test]
fn explicit_prompt_templates_load_even_when_discovery_is_disabled() {
    let workspace = TempDir::new().expect("workspace");
    let state = workspace.path().join("state");
    let prompt = workspace.path().join("hello.md");
    fs::write(
        &prompt,
        "---\ndescription: Say hello\nargument-hint: NAME\n---\nHello $1 from $@",
    )
    .expect("prompt template");
    let output = binary()
        .args([
            "--provider",
            "fake",
            "--workspace",
            workspace.path().to_str().expect("workspace"),
            "--state-dir",
            state.to_str().expect("state"),
            "--session",
            "template",
            "--no-prompt-templates",
            "--prompt-template",
            prompt.to_str().expect("prompt path"),
            "--fake-response",
            "done",
            "--print",
            "/hello Codex",
        ])
        .output()
        .expect("template prompt");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let transcript =
        fs::read_to_string(project_sessions(&state, workspace.path()).join("template.jsonl"))
            .expect("transcript");
    assert!(transcript.contains("Hello Codex from Codex"));
}

#[test]
fn autonomous_limits_and_tool_allowlists_are_enforced() {
    let workspace = TempDir::new().expect("workspace");
    let state = workspace.path().join("state");
    let output = binary()
        .args([
            "--provider",
            "fake",
            "--workspace",
            workspace.path().to_str().expect("workspace"),
            "--state-dir",
            state.to_str().expect("state"),
            "--no-session",
            "--no-tools",
            "--autonomous",
            "--autonomous-max-continuations",
            "2",
            "--fake-response",
            "one",
            "--fake-response",
            "two",
            "--fake-response",
            "three",
            "--print",
            "start",
        ])
        .output()
        .expect("autonomous run");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).lines().count(), 3);

    let missing = binary()
        .args([
            "--provider",
            "fake",
            "--workspace",
            workspace.path().to_str().expect("workspace"),
            "--state-dir",
            state.to_str().expect("state"),
            "--tools",
            "does_not_exist",
            "--fake-response",
            "unused",
            "--print",
            "hello",
        ])
        .output()
        .expect("unknown tool");
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("unknown tool selection"));

    let selected = binary()
        .args([
            "--provider",
            "fake",
            "--workspace",
            workspace.path().to_str().expect("workspace"),
            "--state-dir",
            state.to_str().expect("state"),
            "--tools",
            "read_file",
            "--fake-response",
            "selected",
            "--print",
            "hello",
        ])
        .output()
        .expect("selected tool");
    assert!(
        selected.status.success(),
        "{}",
        String::from_utf8_lossy(&selected.stderr)
    );
}

#[test]
fn no_skills_disables_discovered_skill_resources() {
    let workspace = TempDir::new().expect("workspace");
    let state = workspace.path().join("state");
    let skill = workspace.path().join(".agents/skills/review");
    fs::create_dir_all(&skill).expect("skill directory");
    fs::write(
        skill.join("SKILL.md"),
        "---\nname: review\ndescription: Review changes\n---\nCheck correctness.\n",
    )
    .expect("skill");
    let enabled = binary()
        .args([
            "--provider",
            "fake",
            "--workspace",
            workspace.path().to_str().expect("workspace"),
            "--state-dir",
            state.to_str().expect("state"),
            "--fake-response",
            "reviewed",
            "--print",
            "/skill:review focus",
        ])
        .output()
        .expect("skill enabled");
    assert!(enabled.status.success());

    let disabled = binary()
        .args([
            "--provider",
            "fake",
            "--workspace",
            workspace.path().to_str().expect("workspace"),
            "--state-dir",
            state.to_str().expect("state"),
            "--no-session",
            "--no-skills",
            "--fake-response",
            "unused",
            "--print",
            "/skill:review focus",
        ])
        .output()
        .expect("skill disabled");
    assert!(!disabled.status.success());
    assert!(String::from_utf8_lossy(&disabled.stderr).contains("unknown skill `review`"));
}

#[test]
fn user_skills_are_discovered_from_the_shared_agents_directory() {
    let home = TempDir::new().expect("home");
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    let shared = home.path().join(".agents/skills/shared");
    fs::create_dir_all(&shared).expect("shared skill directory");
    fs::write(
        shared.join("SKILL.md"),
        "---\nname: shared\ndescription: Shared user skill\n---\nUse the shared skill.\n",
    )
    .expect("shared skill");
    let legacy = home.path().join(".mimir/agent/skills/legacy");
    fs::create_dir_all(&legacy).expect("legacy skill directory");
    fs::write(
        legacy.join("SKILL.md"),
        "---\nname: legacy\ndescription: Legacy user skill\n---\nUse the legacy skill.\n",
    )
    .expect("legacy skill");

    let shared_run = binary()
        .env("HOME", home.path())
        .args([
            "--provider",
            "fake",
            "--workspace",
            workspace.path().to_str().expect("workspace"),
            "--state-dir",
            state.path().to_str().expect("state"),
            "--no-session",
            "--fake-response",
            "shared loaded",
            "--print",
            "/skill:shared focus",
        ])
        .output()
        .expect("shared skill run");
    assert!(
        shared_run.status.success(),
        "{}",
        String::from_utf8_lossy(&shared_run.stderr)
    );

    let legacy_run = binary()
        .env("HOME", home.path())
        .args([
            "--provider",
            "fake",
            "--workspace",
            workspace.path().to_str().expect("workspace"),
            "--state-dir",
            state.path().to_str().expect("state"),
            "--no-session",
            "--fake-response",
            "unused",
            "--print",
            "/skill:legacy focus",
        ])
        .output()
        .expect("legacy skill run");
    assert!(!legacy_run.status.success());
    assert!(String::from_utf8_lossy(&legacy_run.stderr).contains("unknown skill `legacy`"));
}

#[test]
fn startup_goal_creates_a_new_root_with_the_requested_budget() {
    let workspace = TempDir::new().expect("workspace");
    let state = workspace.path().join("state");
    let output = binary()
        .args([
            "--provider",
            "fake",
            "--workspace",
            workspace.path().to_str().expect("workspace"),
            "--state-dir",
            state.to_str().expect("state"),
            "--goal",
            "finish migration",
            "--goal-token-budget",
            "1234",
            "--fake-response",
            "working",
            "--print",
            "begin",
        ])
        .output()
        .expect("goal run");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value =
        serde_json::from_slice(&fs::read(state.join("goals/goal.json")).expect("goal file"))
            .expect("goal JSON");
    assert_eq!(value["objective"], "finish migration");
    assert_eq!(value["token_budget"], 1234);
    assert!(
        fs::read_dir(project_sessions(&state, workspace.path()))
            .expect("sessions")
            .filter_map(Result::ok)
            .any(|entry| entry.file_name().to_string_lossy().starts_with("session-"))
    );
}

#[test]
fn model_search_and_safe_html_export_are_public_cli_contracts() {
    let workspace = TempDir::new().expect("workspace");
    let state = workspace.path().join("state");
    let models = binary()
        .args([
            "--workspace",
            workspace.path().to_str().expect("workspace"),
            "--state-dir",
            state.to_str().expect("state"),
            "model",
            "list",
            "gpt-5-mini",
        ])
        .output()
        .expect("model list");
    assert!(
        models.status.success(),
        "{}",
        String::from_utf8_lossy(&models.stderr)
    );
    let catalog: Value = serde_json::from_slice(&models.stdout).expect("model list JSON");
    assert!(catalog["models"].as_array().is_some_and(|models| {
        !models.is_empty()
            && models.iter().all(|model| {
                model["selector"]
                    .as_str()
                    .is_some_and(|selector| selector.contains("gpt-5-mini"))
            })
    }));

    fake_run(
        workspace.path(),
        &state,
        "html",
        "<script>alert('x')</script>",
    );
    let html_path = workspace.path().join("session.html");
    let exported = binary()
        .args([
            "--workspace",
            workspace.path().to_str().expect("workspace"),
            "--state-dir",
            state.to_str().expect("state"),
            "--session",
            "html",
            "session",
            "export",
            html_path.to_str().expect("HTML output"),
        ])
        .output()
        .expect("HTML export");
    assert!(
        exported.status.success(),
        "{}",
        String::from_utf8_lossy(&exported.stderr)
    );
    let html = fs::read_to_string(html_path).expect("HTML body");
    assert!(html.starts_with("<!doctype html>"));
    assert!(html.contains("&lt;script&gt;alert(&#39;x&#39;)&lt;/script&gt;"));
    assert!(!html.contains("<script>alert"));
}

#[test]
fn package_cli_uses_the_audited_local_package_manager() {
    let workspace = TempDir::new().expect("workspace");
    let state = workspace.path().join("state");
    let package = workspace.path().join("extension-package");
    fs::create_dir(&package).expect("package directory");
    fs::write(
        package.join("package.json"),
        r#"{"name":"cli-local-extension","version":"1.0.0","pi":{"extensions":["index.ts"]}}"#,
    )
    .expect("package manifest");
    fs::write(
        package.join("index.ts"),
        "export default pi => pi.registerCommand('local', { handler: async () => ({ output: null }) });",
    )
    .expect("package entry");

    let installed = binary()
        .args([
            "--state-dir",
            state.to_str().expect("state"),
            "package",
            "install",
            package.to_str().expect("package"),
        ])
        .output()
        .expect("package install");
    assert!(
        installed.status.success(),
        "{}",
        String::from_utf8_lossy(&installed.stderr)
    );
    let listed = binary()
        .args([
            "--state-dir",
            state.to_str().expect("state"),
            "package",
            "list",
        ])
        .output()
        .expect("package list");
    assert!(listed.status.success());
    assert!(String::from_utf8_lossy(&listed.stdout).contains("cli-local-extension"));
    let removed = binary()
        .args([
            "--state-dir",
            state.to_str().expect("state"),
            "package",
            "remove",
            "cli-local-extension",
        ])
        .output()
        .expect("package remove");
    assert!(
        removed.status.success(),
        "{}",
        String::from_utf8_lossy(&removed.stderr)
    );
}
