use std::path::Path;

use mimir::resources::{ResourceLoader, ResourceLoaderOptions, expand_prompt_template};
use serde_json::json;
use tempfile::TempDir;

fn write_skill(root: &Path, relative: &str, name: &str, description: &str) {
    let directory = root.join(relative);
    std::fs::create_dir_all(&directory).expect("skill directory");
    std::fs::write(
        directory.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {description}\n---\n{description} body"),
    )
    .expect("skill");
}

fn write_prompt(root: &Path, relative: &str, description: &str, body: &str) {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().expect("prompt parent")).expect("prompt directory");
    std::fs::write(
        path,
        format!("---\ndescription: {description}\nargument-hint: <target>\n---\n{body}"),
    )
    .expect("prompt");
}

fn write_theme(root: &Path, relative: &str, name: &str, accent: &str) {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().expect("theme parent")).expect("theme directory");
    std::fs::write(
        path,
        serde_json::to_vec(&json!({
            "name": name,
            "vars": {"accent": accent},
            "colors": {"accent": "accent", "text": ""}
        }))
        .expect("theme json"),
    )
    .expect("theme");
}

fn populate_precedence_fixture(
    workspace: &Path,
    child: &Path,
    global: &Path,
    user: &Path,
    package: &Path,
    explicit: &Path,
) {
    std::fs::write(global.join("claude.MD"), "global context").expect("global context");
    std::fs::write(user.join("AGENTS.Md"), "user context").expect("user context");
    std::fs::write(workspace.join("agents.md"), "project context").expect("project context");
    std::fs::write(child.join("CLAUDE.MD"), "nearest context").expect("nearest context");

    write_skill(global, "skills/review", "review", "global");
    write_skill(package, "capabilities/review", "review", "package");
    write_skill(
        package,
        "capabilities/package-only",
        "package-only",
        "package only",
    );
    write_skill(user, "skills/review", "review", "user");
    write_skill(workspace, ".mimir/agent/skills/review", "review", "project");
    write_skill(child, ".agents/skills/review", "review", "nearest");
    write_skill(explicit, "review", "review", "explicit");

    write_prompt(global, "prompts/audit.md", "global", "global $1");
    write_prompt(user, "prompts/audit.md", "user", "user $1");
    write_prompt(
        workspace,
        ".mimir/agent/prompts/audit.md",
        "project",
        "project $1",
    );
    write_prompt(explicit, "audit.md", "explicit", "explicit $1 $ARGUMENTS");

    write_theme(global, "themes/custom.json", "custom", "#111111");
    write_theme(user, "themes/custom.json", "custom", "#222222");
    write_theme(
        workspace,
        ".mimir/agent/themes/custom.json",
        "custom",
        "#333333",
    );
    write_theme(explicit, "custom.json", "custom", "#444444");

    std::fs::write(global.join("SYSTEM.md"), "global system").expect("global system");
    let project_config = workspace.join(".mimir/agent");
    std::fs::create_dir_all(&project_config).expect("project config");
    std::fs::write(project_config.join("SYSTEM.md"), "project system").expect("project system");
    std::fs::write(project_config.join("APPEND_SYSTEM.md"), "project append").expect("append");
    let nearest_config = child.join(".mimir/agent");
    std::fs::create_dir_all(&nearest_config).expect("nearest config");
    std::fs::write(nearest_config.join("SYSTEM.md"), "nearest system").expect("nearest system");

    std::fs::write(
        package.join("package.json"),
        json!({
            "name": "local-capabilities",
            "pi": {"skills": ["capabilities/review", "capabilities/package-only"], "prompts": [], "themes": []}
        })
        .to_string(),
    )
    .expect("manifest");
}

#[test]
fn loader_discovers_every_scope_deterministically_with_nearest_and_explicit_wins() {
    let workspace = TempDir::new().expect("workspace");
    let child = workspace.path().join("src/nested");
    let global = TempDir::new().expect("global");
    let user = TempDir::new().expect("user");
    let package = TempDir::new().expect("package");
    let explicit = TempDir::new().expect("explicit");
    std::fs::create_dir_all(&child).expect("child");
    populate_precedence_fixture(
        workspace.path(),
        &child,
        global.path(),
        user.path(),
        package.path(),
        explicit.path(),
    );

    let resources = ResourceLoader::with_options(
        workspace.path(),
        &child,
        ResourceLoaderOptions {
            global_dir: Some(global.path().into()),
            user_dir: Some(user.path().into()),
            package_paths: vec![package.path().into()],
            explicit_skill_paths: vec![explicit.path().join("review")],
            explicit_prompt_template_paths: vec![explicit.path().join("audit.md")],
            explicit_theme_paths: vec![explicit.path().join("custom.json")],
            ..ResourceLoaderOptions::default()
        },
    )
    .expect("loader")
    .load()
    .expect("resources");

    let context = &resources.system_context;
    let positions = [
        "global context",
        "user context",
        "project context",
        "nearest context",
    ]
    .map(|value| context.find(value).expect("context entry"));
    assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
    assert_eq!(resources.skills.len(), 2);
    assert_eq!(
        resources
            .skills
            .iter()
            .find(|skill| skill.name == "review")
            .expect("review skill")
            .description,
        "explicit"
    );
    assert!(
        resources
            .skills
            .iter()
            .any(|skill| skill.name == "package-only")
    );
    assert_eq!(resources.system_prompt.as_deref(), Some("nearest system"));
    assert_eq!(resources.append_system_prompt, ["project append"]);
    assert_eq!(resources.package_manifests.len(), 1);
    assert_eq!(resources.prompt_templates[0].description, "explicit");
    assert_eq!(resources.themes[0].definition["vars"]["accent"], "#444444");
    assert_eq!(
        expand_prompt_template("/audit 'one two' $@", &resources.prompt_templates),
        "explicit one two one two $@"
    );
}

#[cfg(unix)]
#[test]
fn explicit_symlinks_and_package_manifest_escapes_fail_closed_without_file_contents() {
    use std::os::unix::fs::symlink;

    let workspace = TempDir::new().expect("workspace");
    let outside = workspace.path().join("outside");
    let package = workspace.path().join("package");
    std::fs::create_dir_all(&package).expect("package");
    write_prompt(&outside, "secret.md", "private", "DO_NOT_LEAK_SECRET_VALUE");
    symlink(
        outside.join("secret.md"),
        workspace.path().join("linked.md"),
    )
    .expect("symlink");

    let symlink_error = ResourceLoader::with_options(
        workspace.path(),
        workspace.path(),
        ResourceLoaderOptions {
            explicit_prompt_template_paths: vec![workspace.path().join("linked.md")],
            ..ResourceLoaderOptions::default()
        },
    )
    .expect("loader")
    .load()
    .expect_err("explicit symlink rejected");
    assert!(symlink_error.to_string().contains("must not be a symlink"));
    assert!(
        !symlink_error
            .to_string()
            .contains("DO_NOT_LEAK_SECRET_VALUE")
    );

    std::fs::write(
        package.join("package.json"),
        json!({"pi": {"prompts": ["../outside/secret.md"]}}).to_string(),
    )
    .expect("manifest");
    let escape = ResourceLoader::with_options(
        workspace.path(),
        workspace.path(),
        ResourceLoaderOptions {
            package_paths: vec![package],
            ..ResourceLoaderOptions::default()
        },
    )
    .expect("loader")
    .load()
    .expect_err("package escape rejected");
    assert!(escape.to_string().contains("relative paths"));
    assert!(!escape.to_string().contains("DO_NOT_LEAK_SECRET_VALUE"));
}
