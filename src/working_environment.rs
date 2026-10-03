//! Versioned Betterloop environment semantics shared with the native adapters.
use crate::{
    error::{MimirError, Result},
    resources::Skill,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::io::Read as _;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkingEnvironment {
    pub schema: u8,
    pub minimum_adapter_version: String,
    pub minimum_runtime_versions: BTreeMap<String, String>,
    pub context: Vec<Value>,
    pub skills: Vec<EnvironmentSkill>,
    pub mcp_servers: Vec<Value>,
    pub workflows: Vec<Value>,
    pub disabled_inherited: Vec<String>,
    pub visibility: Value,
    pub delivery: Value,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentSkill {
    pub key: String,
    pub template_id: String,
    pub version: String,
    pub enforcement: String,
    pub parameters: BTreeMap<String, String>,
}
fn fixture() -> Value {
    serde_json::from_str(include_str!("shared-working-environment.json"))
        .expect("embedded shared environment fixture must be valid")
}
/// Returns the embedded platform defaults.
///
/// # Panics
/// Panics if the compiled shared fixture is invalid, which is checked in tests.
pub fn defaults() -> WorkingEnvironment {
    serde_json::from_value(fixture()["environment"].clone())
        .expect("embedded environment must match contract")
}
///
/// # Errors
/// Returns an error for invalid verified configuration or unavailable local resources.
pub fn validate(environment: &WorkingEnvironment) -> Result<()> {
    if environment.schema != 1
        || environment.context.len() > 32
        || environment.skills.len() > 128
        || environment.mcp_servers.len() > 32
        || environment.workflows.len() > 128
        || !environment.disabled_inherited.is_empty()
    {
        return Err(MimirError::Configuration(
            "Invalid resolved working environment bounds".into(),
        ));
    }
    if let Some(minimum) = environment.minimum_runtime_versions.get("mimir")
        && crate::enterprise::compare_versions(env!("CARGO_PKG_VERSION"), minimum)? < 0
    {
        return Err(MimirError::Configuration(
            "Mimir upgrade required by working environment".into(),
        ));
    }
    validate_value_contract(environment)?;
    let templates = fixture()["templates"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let mut keys = BTreeSet::new();
    for item in environment
        .context
        .iter()
        .chain(environment.mcp_servers.iter())
        .chain(environment.workflows.iter())
    {
        let key = item["key"]
            .as_str()
            .ok_or_else(|| MimirError::Configuration("Environment item key is missing".into()))?;
        if key.len() > 128 || !keys.insert(key.to_owned()) {
            return Err(MimirError::Configuration(
                "Duplicate or oversized environment key".into(),
            ));
        }
        if !matches!(item["enforcement"].as_str(), Some("default" | "mandatory")) {
            return Err(MimirError::Configuration(
                "Invalid environment enforcement".into(),
            ));
        }
    }
    for item in &environment.skills {
        if !keys.insert(item.key.clone())
            || !matches!(item.enforcement.as_str(), "default" | "mandatory")
            || item.parameters.len() > 16
        {
            return Err(MimirError::Configuration(
                "Invalid environment skill".into(),
            ));
        }
        let template = templates
            .iter()
            .find(|template| {
                template["id"] == item.template_id && template["version"] == item.version
            })
            .ok_or_else(|| MimirError::Configuration("Unavailable platform template".into()))?;
        let names = template["parameters"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        if item.parameters.iter().any(|(key, value)| {
            !names.iter().any(|name| name.as_str() == Some(key)) || value.len() > 1024
        }) {
            return Err(MimirError::Configuration(
                "Invalid template parameters".into(),
            ));
        }
    }
    if environment.delivery["mode"] != "developer_authorized_pr"
        || environment.delivery["merge"] != "explicit_authorization"
        || environment.delivery["deploy"] != "explicit_authorization"
    {
        return Err(MimirError::Configuration(
            "Invalid delivery authorization contract".into(),
        ));
    }
    let budget = environment.visibility["startup_characters"]
        .as_u64()
        .unwrap_or(0);
    if !(1000..=8000).contains(&budget) {
        return Err(MimirError::Configuration(
            "Invalid startup context budget".into(),
        ));
    }
    Ok(())
}
fn strict_fields(value: &Value, fields: &[&str]) -> Result<()> {
    if value
        .as_object()
        .is_none_or(|object| object.keys().any(|key| !fields.contains(&key.as_str())))
    {
        return Err(MimirError::Configuration(
            "Unknown environment fields".into(),
        ));
    }
    Ok(())
}
fn valid_key(key: &str) -> bool {
    regex::Regex::new(r"^[a-z][a-z0-9_.-]{0,127}$").is_ok_and(|pattern| pattern.is_match(key))
}
fn validate_value_contract(environment: &WorkingEnvironment) -> Result<()> {
    let invalid = || MimirError::Configuration("Invalid shared environment contract".into());
    crate::enterprise::compare_versions(&environment.minimum_adapter_version, "0.2.0")?;
    for (harness, minimum) in &environment.minimum_runtime_versions {
        if !matches!(harness.as_str(), "mimir" | "claude-code" | "codex") {
            return Err(invalid());
        }
        crate::enterprise::compare_versions(minimum, "0.0.0")?;
    }
    for item in &environment.context {
        strict_fields(item, &["key", "kind", "enforcement", "visibility"])?;
        if !matches!(
            item["kind"].as_str(),
            Some("repository" | "guidance" | "learning")
        ) || !matches!(item["visibility"].as_str(), Some("startup" | "on_demand"))
        {
            return Err(invalid());
        }
    }
    for item in &environment.workflows {
        strict_fields(item, &["key", "level", "operation", "enforcement", "order"])?;
        if !matches!(
            item["level"].as_str(),
            Some(
                "setup"
                    | "session"
                    | "task"
                    | "before_action"
                    | "after_action"
                    | "completion"
                    | "continuity"
            )
        ) || !matches!(
            item["operation"].as_str(),
            Some(
                "prepare_context"
                    | "select_skills"
                    | "evaluate_policy"
                    | "observe_result"
                    | "verify_work"
                    | "prepare_delivery"
                    | "preserve_state"
            )
        ) || item["order"].as_u64().is_none_or(|order| order > 10_000)
        {
            return Err(invalid());
        }
    }
    validate_mcp_contract(environment)?;
    for item in environment
        .context
        .iter()
        .chain(&environment.workflows)
        .chain(&environment.mcp_servers)
    {
        if item["key"].as_str().is_none_or(|key| !valid_key(key)) {
            return Err(invalid());
        }
    }
    let parameter_pattern = regex::Regex::new(r"^[a-z][a-z0-9_]{0,63}$").map_err(|_| invalid())?;
    for item in &environment.skills {
        if !valid_key(&item.key)
            || !valid_key(&item.template_id)
            || item
                .parameters
                .iter()
                .any(|(key, _)| !parameter_pattern.is_match(key))
        {
            return Err(invalid());
        }
        crate::enterprise::compare_versions(&item.version, "0.0.0")?;
    }
    strict_fields(
        &environment.visibility,
        &["startup_characters", "developer_activity"],
    )?;
    if !environment.visibility["developer_activity"].is_boolean() {
        return Err(invalid());
    }
    strict_fields(&environment.delivery, &["mode", "merge", "deploy"])?;
    Ok(())
}
fn validate_mcp_contract(environment: &WorkingEnvironment) -> Result<()> {
    let invalid = || MimirError::Configuration("Invalid MCP environment contract".into());
    let credential_pattern =
        regex::Regex::new(r"^[A-Z][A-Z0-9_]{0,127}$").map_err(|_| invalid())?;
    for item in &environment.mcp_servers {
        strict_fields(
            item,
            &[
                "key",
                "label",
                "transport",
                "command",
                "args",
                "url",
                "credential_ref",
                "expected_tools",
                "enforcement",
            ],
        )?;
        if item["key"] == "betterloop"
            || item["label"]
                .as_str()
                .is_none_or(|label| label.is_empty() || label.len() > 160)
        {
            return Err(invalid());
        }
        if item.get("credential_ref").is_some_and(|value| {
            value
                .as_str()
                .is_none_or(|value| !credential_pattern.is_match(value))
        }) {
            return Err(invalid());
        }
        if item["args"].as_array().is_none_or(|args| {
            args.len() > 64
                || args
                    .iter()
                    .any(|arg| arg.as_str().is_none_or(|arg| arg.len() > 1024))
        }) || item["expected_tools"].as_array().is_none_or(|tools| {
            tools.len() > 128
                || tools
                    .iter()
                    .any(|tool| tool.as_str().is_none_or(|tool| !valid_key(tool)))
        }) {
            return Err(invalid());
        }
        match item["transport"].as_str() {
            Some("stdio")
                if item.get("url").is_none()
                    && item["command"]
                        .as_str()
                        .is_some_and(|command| !command.is_empty() && command.len() <= 512) => {}
            Some("http")
                if item.get("command").is_none()
                    && item["args"].as_array().is_some_and(Vec::is_empty) =>
            {
                let endpoint = reqwest::Url::parse(item["url"].as_str().ok_or_else(invalid)?)
                    .map_err(|_| invalid())?;
                if endpoint.as_str().len() > 2048
                    || endpoint.scheme() != "https"
                    || !endpoint.username().is_empty()
                    || endpoint.password().is_some()
                    || endpoint.query().is_some()
                    || endpoint.fragment().is_some()
                {
                    return Err(invalid());
                }
            }
            _ => return Err(invalid()),
        }
    }
    Ok(())
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepositoryPackage {
    pub path: String,
    pub stack: Vec<String>,
    pub languages: Vec<String>,
    pub roles: Vec<String>,
    pub commands: Vec<Value>,
    pub evidence: Vec<Value>,
    pub manifests: Vec<String>,
    pub guidance: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepositoryFacts {
    pub guidance: Vec<String>,
    pub documents: Vec<String>,
    pub packages: Vec<RepositoryPackage>,
    pub fingerprint: String,
    pub truncated: bool,
}
#[derive(Serialize)]
struct Fingerprint<'a> {
    files: Vec<(&'a str, String)>,
    documents: &'a Vec<String>,
}
/// Bounded local discovery; symlinks and generated directories are excluded.
///
/// # Errors
/// Returns an error for invalid verified configuration or unavailable local resources.
#[allow(
    clippy::too_many_lines,
    reason = "bounded discovery keeps all manifest cases together without executing repository code"
)]
pub fn repository_facts(workspace: &Path) -> Result<RepositoryFacts> {
    let root = std::fs::canonicalize(workspace)?;
    let manifests = [
        "package.json",
        "Cargo.toml",
        "pyproject.toml",
        "requirements.txt",
        "go.mod",
        "pom.xml",
        "build.gradle",
        "Gemfile",
        "composer.json",
        "pnpm-workspace.yaml",
    ];
    let guides = ["AGENTS.md", "CLAUDE.md", "README.md", "CONTRIBUTING.md"];
    let ignored = [
        "node_modules",
        "target",
        "dist",
        "build",
        "venv",
        "vendor",
        "coverage",
    ];
    let mut files = Vec::<(String, String)>::new();
    let mut documents = Vec::new();
    let mut directories = 0;
    let truncated = std::cell::Cell::new(false);
    for entry in walkdir::WalkDir::new(&root)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|entry| {
            if entry.depth() == 0 {
                return true;
            }
            let name = entry.file_name().to_string_lossy();
            if entry.file_type().is_dir() {
                if name.starts_with('.') || ignored.contains(&name.as_ref()) {
                    return false;
                }
                directories += 1;
                if entry.depth() > 6 || directories >= 1000 {
                    truncated.set(true);
                    return false;
                }
            }
            !entry.file_type().is_symlink()
        })
    {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy();
        let relative = entry
            .path()
            .strip_prefix(&root)
            .map_err(|_| MimirError::Configuration("Repository path escaped".into()))?
            .to_string_lossy()
            .replace('\\', "/");
        if manifests.contains(&name.as_ref()) || guides.contains(&name.as_ref()) {
            if files.len() >= 512
                || entry
                    .metadata()
                    .map_or(true, |info| info.len() > 128 * 1024)
            {
                truncated.set(true);
                continue;
            }
            let mut content = String::new();
            std::fs::File::open(entry.path())?
                .take(128 * 1024 + 1)
                .read_to_string(&mut content)?;
            if content.len() > 128 * 1024 {
                truncated.set(true);
                continue;
            }
            files.push((relative, content));
        } else if relative.starts_with("docs/")
            && (name.ends_with(".md") || name.ends_with(".mdx"))
            && documents.len() < 128
        {
            documents.push(relative);
        }
    }
    let guidance: Vec<String> = files
        .iter()
        .filter(|(name, _)| {
            guides.contains(
                &Path::new(name)
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .as_ref(),
            )
        })
        .take(128)
        .map(|(name, _)| name.clone())
        .collect();
    let manifest_entries: Vec<_> = files
        .iter()
        .filter(|(name, _)| {
            manifests.contains(
                &Path::new(name)
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .as_ref(),
            )
        })
        .collect();
    let package_paths: BTreeSet<String> = std::iter::once(".".to_owned())
        .chain(manifest_entries.iter().map(|(name, _)| parent_path(name)))
        .collect();
    if package_paths.len() > 128 {
        truncated.set(true);
    }
    let mut packages = Vec::new();
    for path in package_paths.into_iter().take(128) {
        let mut package = RepositoryPackage {
            path: path.clone(),
            stack: vec![],
            languages: vec![],
            roles: vec![],
            commands: vec![],
            evidence: vec![],
            manifests: vec![],
            guidance: guidance
                .iter()
                .filter(|name| {
                    let parent = parent_path(name);
                    parent == "." || path == parent || path.starts_with(&format!("{parent}/"))
                })
                .cloned()
                .collect(),
        };
        for (name, content) in manifest_entries
            .iter()
            .filter(|(name, _)| parent_path(name) == path)
        {
            package.manifests.push(name.clone());
            package.evidence.push(
                json!({"path":name,"sha256":format!("{:x}",Sha256::digest(content.as_bytes()))}),
            );
            populate_manifest(
                &mut package,
                name,
                content,
                files.iter().any(|(name, _)| name == "pnpm-workspace.yaml"),
            );
        }
        packages.push(package);
    }
    let fingerprint = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&Fingerprint {
            files: files
                .iter()
                .map(|(name, content)| (
                    name.as_str(),
                    format!("{:x}", Sha256::digest(content.as_bytes()))
                ))
                .collect(),
            documents: &documents
        })?)
    );
    Ok(RepositoryFacts {
        packages,
        guidance,
        documents,
        fingerprint,
        truncated: truncated.get(),
    })
}
fn parent_path(name: &str) -> String {
    Path::new(name)
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map_or_else(
            || ".".to_owned(),
            |parent| parent.to_string_lossy().into_owned(),
        )
}
fn insert_fact(list: &mut Vec<String>, value: &str) {
    if !list.iter().any(|item| item == value) {
        list.push(value.to_owned());
    }
}
fn candidate(package: &mut RepositoryPackage, name: &str, kind: &str, argv: &[&str]) {
    package
        .commands
        .push(json!({"kind":kind,"argv":argv,"source":name,"review_required":true}));
}
fn populate_manifest(package: &mut RepositoryPackage, name: &str, content: &str, workspace: bool) {
    let base = Path::new(name)
        .file_name()
        .unwrap_or_default()
        .to_string_lossy();
    match base.as_ref() {
        "package.json" => {
            insert_fact(&mut package.languages, "JavaScript/TypeScript");
            let Ok(manifest) = serde_json::from_str::<Value>(content) else {
                return;
            };
            for (dependency, stack, role) in [
                ("react", "React", "frontend"),
                ("next", "Next.js", "frontend"),
                ("astro", "Astro", "frontend"),
                ("vue", "Vue", "frontend"),
                ("svelte", "Svelte", "frontend"),
                ("react-native", "React Native", "frontend"),
                ("expo", "Expo", "frontend"),
                ("fastify", "Fastify", "backend"),
                ("express", "Express", "backend"),
                ("hono", "Hono", "backend"),
                ("pg", "PostgreSQL", "database"),
                ("prisma", "Prisma", "database"),
                ("@supabase/supabase-js", "Supabase", "database"),
            ] {
                if manifest["dependencies"].get(dependency).is_some()
                    || manifest["devDependencies"].get(dependency).is_some()
                {
                    insert_fact(&mut package.stack, stack);
                    insert_fact(&mut package.roles, role);
                }
            }
            let manager = manifest["packageManager"].as_str().unwrap_or_default();
            let runner = if manager.starts_with("pnpm@") || workspace {
                "pnpm"
            } else if manager.starts_with("yarn@") {
                "yarn"
            } else {
                "npm"
            };
            for kind in ["test", "build", "typecheck", "lint"] {
                if manifest["scripts"][kind].is_string() {
                    candidate(package, name, kind, &[runner, "run", kind]);
                }
            }
        }
        "Cargo.toml" => {
            insert_fact(&mut package.languages, "Rust");
            insert_fact(&mut package.stack, "Cargo");
            candidate(package, name, "test", &["cargo", "test"]);
            candidate(package, name, "build", &["cargo", "build"]);
        }
        "pyproject.toml" | "requirements.txt" => {
            insert_fact(&mut package.languages, "Python");
            let lower = content.to_lowercase();
            for (dependency, stack) in [("django", "Django"), ("fastapi", "FastAPI")] {
                if lower.contains(dependency) {
                    insert_fact(&mut package.stack, stack);
                    insert_fact(&mut package.roles, "backend");
                }
            }
            if lower.contains("pytest") {
                candidate(package, name, "test", &["python", "-m", "pytest"]);
            }
        }
        "go.mod" => {
            insert_fact(&mut package.languages, "Go");
            candidate(package, name, "test", &["go", "test", "./..."]);
        }
        "pom.xml" | "build.gradle" => insert_fact(&mut package.languages, "Java"),
        _ => {}
    }
}

///
/// # Errors
/// Returns an error for invalid verified configuration or unavailable local resources.
pub fn template_skills(
    environment: &WorkingEnvironment,
    facts: &RepositoryFacts,
) -> Result<Vec<Skill>> {
    validate(environment)?;
    let templates = fixture()["templates"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let root_stack = facts
        .packages
        .iter()
        .find(|item| item.path == ".")
        .map(|item| item.stack.clone())
        .unwrap_or_default();
    let selected_stack = if root_stack.is_empty() {
        facts
            .packages
            .iter()
            .flat_map(|item| item.stack.clone())
            .collect()
    } else {
        root_stack
    };
    let stack = selected_stack.join(", ");
    environment
        .skills
        .iter()
        .map(|selection| {
            let template = templates
                .iter()
                .find(|item| {
                    item["id"] == selection.template_id && item["version"] == selection.version
                })
                .ok_or_else(|| MimirError::Configuration("Unavailable template".into()))?;
            let mut instructions = template["instructions"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            for name in template["parameters"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                let value = selection
                    .parameters
                    .get(name)
                    .cloned()
                    .or_else(|| match name {
                        "stack" if !stack.is_empty() => Some(stack.clone()),
                        "package" => Some(".".into()),
                        _ => None,
                    });
                let replacement = value.map_or_else(
                    || format!("[inspect repository {name}]"),
                    |value| serde_json::to_string(&value).unwrap_or_default(),
                );
                instructions = instructions.replace(&format!("{{{{{name}}}}}"), &replacement);
            }
            Ok(Skill::in_memory(
                selection.key.clone(),
                template["description"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
                instructions,
                PathBuf::from(format!(
                    "platform://{}@{}",
                    selection.template_id, selection.version
                )),
            ))
        })
        .collect()
}
/// Substitutes declared factual parameters in a reviewed skill body.
///
/// # Errors
/// Returns an error for an unavailable template or unknown parameter.
pub fn render_reviewed_skill(
    template_id: &str,
    instructions: &str,
    facts: &RepositoryFacts,
) -> Result<String> {
    let templates: Value = serde_json::from_str(include_str!("shared-working-environment.json"))?;
    let template = templates["templates"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|entry| entry["id"] == template_id)
        .ok_or_else(|| MimirError::Configuration("Unavailable factual template".into()))?;
    let root_stack = facts
        .packages
        .iter()
        .find(|item| item.path == ".")
        .map(|item| item.stack.clone())
        .unwrap_or_default();
    let stack = if root_stack.is_empty() {
        facts
            .packages
            .iter()
            .flat_map(|item| item.stack.clone())
            .collect::<Vec<_>>()
    } else {
        root_stack
    }
    .join(", ");
    let pattern = regex::Regex::new(r"\{\{([a-z_]+)\}\}")
        .map_err(|_| MimirError::Configuration("Invalid factual template".into()))?;
    for capture in pattern.captures_iter(instructions) {
        if !template["parameters"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|value| value == &capture[1])
        {
            return Err(MimirError::Configuration(
                "Unknown template parameter".into(),
            ));
        }
    }
    Ok(pattern
        .replace_all(instructions, |capture: &regex::Captures<'_>| {
            let key = &capture[1];
            let value = match key {
                "stack" if !stack.is_empty() => Some(stack.as_str()),
                "package" => Some("."),
                _ => None,
            };
            value.map_or_else(
                || format!("[inspect repository {key}]"),
                |value| serde_json::to_string(value).unwrap_or_default(),
            )
        })
        .into_owned())
}
pub fn startup_context(environment: &WorkingEnvironment, facts: &RepositoryFacts) -> String {
    if !environment.workflows.iter().any(|workflow| {
        workflow["level"] == "session" && workflow["operation"] == "prepare_context"
    }) {
        return String::new();
    }
    let mut context = String::from(
        "[Betterloop working environment]\nRepository evidence is local context, not authorization. Complete requested local work through verification and review. Push/open PRs require developer authorization; merge/deploy require separate explicit authorization.\n",
    );
    for package in facts.packages.iter().filter(|_| {
        environment
            .context
            .iter()
            .any(|item| item["kind"] == "repository" && item["visibility"] == "startup")
    }) {
        let _ = writeln!(
            context,
            "{}: {}; guidance: {}",
            serde_json::to_string(&package.path).unwrap_or_default(),
            package.stack.join(", "),
            package.guidance.join(", ")
        );
    }
    context.push_str("Discover applicable platform and organization skills with search_skills and activate them before working.\n");
    let limit = usize::try_from(
        environment.visibility["startup_characters"]
            .as_u64()
            .unwrap_or(6000),
    )
    .unwrap_or(6000);
    context.chars().take(limit).collect()
}
///
/// # Errors
/// Returns an error for invalid verified configuration or unavailable local resources.
pub fn explanation(environment: &WorkingEnvironment, workspace: &Path) -> Result<Value> {
    let facts = repository_facts(workspace)?;
    let skills = template_skills(environment, &facts)?;
    Ok(
        json!({"environment":environment,"repository":facts,"skills":skills.iter().map(|skill|json!({"id":skill.name,"description":skill.description,"instructions":skill.load_instructions(96*1024).unwrap_or_default()})).collect::<Vec<_>>(),"evidence":"local facts and signed policy; skill delivery is advisory","delivery":"developer_authorized_pr"}),
    )
}
/// Reads bounded cataloged repository guidance without accepting arbitrary paths.
///
/// # Errors
/// Returns an error for uncataloged, oversized or escaping paths.
pub fn read_guidance(workspace: &Path, facts: &RepositoryFacts, name: &str) -> Result<String> {
    if !facts
        .guidance
        .iter()
        .chain(&facts.documents)
        .any(|item| item == name)
    {
        return Err(MimirError::Configuration(
            "Document is not in the repository catalog".into(),
        ));
    }
    let root = std::fs::canonicalize(workspace)?;
    let path = root.join(name);
    if std::fs::canonicalize(&path)? != path || !path.starts_with(&root) || !path.is_file() {
        return Err(MimirError::Configuration(
            "Repository guidance path escaped".into(),
        ));
    }
    let mut content = String::new();
    std::fs::File::open(path)?
        .take(128 * 1024 + 1)
        .read_to_string(&mut content)?;
    if content.len() > 128 * 1024 {
        return Err(MimirError::Configuration(
            "Repository guidance exceeds limit".into(),
        ));
    }
    Ok(content)
}
fn invalid_rpc_request(request: &Value) -> bool {
    request["jsonrpc"] != "2.0"
        || !request["method"].is_string()
        || request
            .get("id")
            .is_some_and(|id| !id.is_null() && !id.is_string() && !id.is_number())
}
/// Handles local MCP requests after loading verified native policy for every request.
///
/// # Errors
/// Returns an error for invalid verified configuration or unavailable local resources.
pub fn bridge_request(request: &Value, workspace: &Path) -> Result<Option<Value>> {
    if invalid_rpc_request(request) {
        return Ok(Some(
            json!({"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"Invalid request"}}),
        ));
    }
    let Some(id) = request.get("id") else {
        return Ok(None);
    };
    let environment = crate::enterprise::working_environment()?
        .ok_or_else(|| MimirError::Configuration("Mimir is not enrolled".into()))?;
    let result = match request["method"].as_str() {
        Some("initialize") => {
            json!({"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"betterloop","version":"0.2.0"}})
        }
        Some("ping") => json!({}),
        Some("tools/list") => json!({"tools":[
         {"name":"environment_status","description":"Inspect signed environment and local repository facts.","inputSchema":{"type":"object","properties":{},"additionalProperties":false}},
         {"name":"repository_context","description":"Read local repository package evidence.","inputSchema":{"type":"object","properties":{},"additionalProperties":false}},
         {"name":"read_repository_guidance","description":"Read a cataloged local guidance document.","inputSchema":{"type":"object","properties":{"path":{"type":"string","maxLength":1024}},"required":["path"],"additionalProperties":false}},
         {"name":"list_agents","description":"List reviewed registry agent roles.","inputSchema":{"type":"object","properties":{},"additionalProperties":false}},
         {"name":"activate_agent","description":"Read a reviewed role; native approvals remain authoritative and delegation is not authorized by activation.","inputSchema":{"type":"object","properties":{"id":{"type":"string","maxLength":128}},"required":["id"],"additionalProperties":false}},
         {"name":"list_skills","description":"List platform and verified organization skills.","inputSchema":{"type":"object","properties":{},"additionalProperties":false}},
         {"name":"activate_skill","description":"Read an applicable approved skill.","inputSchema":{"type":"object","properties":{"id":{"type":"string","maxLength":128}},"required":["id"],"additionalProperties":false}}
        ]}),
        Some("tools/call") => {
            let name = request["params"]["name"].as_str().unwrap_or_default();
            let arguments = request["params"]
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            strict_fields(
                &arguments,
                match name {
                    "activate_skill" | "activate_agent" => &["id"],
                    "read_repository_guidance" => &["path"],
                    _ => &[],
                },
            )?;
            let facts = repository_facts(workspace)?;
            let mut skills = template_skills(&environment, &facts)?;
            skills.extend(crate::enterprise::managed_skills_for(workspace)?);
            let content = match name {
                "environment_status" => explanation(&environment, workspace)?,
                "repository_context" => json!({"repository":facts,"evidence_only":true}),
                "read_repository_guidance" => {
                    let path = arguments["path"]
                        .as_str()
                        .filter(|path| path.len() <= 1024)
                        .ok_or_else(|| {
                            MimirError::Configuration("Guidance path required".into())
                        })?;
                    json!({"path":path,"content":read_guidance(workspace,&facts,path)?,"evidence_only":true})
                }
                "list_agents" => crate::registry::agent_catalog(&crate::registry::active()?),
                "activate_agent" => {
                    let key = arguments["id"]
                        .as_str()
                        .ok_or_else(|| MimirError::Configuration("Agent id required".into()))?;
                    let artifacts = crate::registry::active()?;
                    let artifact = artifacts
                        .iter()
                        .find(|item| item.kind == "agent" && item.id == key)
                        .ok_or_else(|| {
                            MimirError::Configuration("Registry agent is unavailable".into())
                        })?;
                    json!({"id":key,"agent":serde_json::from_str::<crate::registry::AgentRole>(&artifact.content)?,"mode":"role_instructions","permissions":"Native and organization approvals remain authoritative; activation does not authorize delegation"})
                }
                "list_skills" => json!(
                    skills
                        .iter()
                        .map(|skill| json!({"id":skill.name,"description":skill.description}))
                        .collect::<Vec<_>>()
                ),
                "activate_skill" => {
                    let key = request["params"]["arguments"]["id"]
                        .as_str()
                        .ok_or_else(|| MimirError::Configuration("Skill id is required".into()))?;
                    let skill = skills
                        .iter()
                        .find(|skill| skill.name == key)
                        .ok_or_else(|| MimirError::Configuration("Skill is not approved".into()))?;
                    json!({"id":key,"instructions":skill.load_instructions(96*1024).map_err(|error|MimirError::Configuration(error.to_string()))?})
                }
                _ => {
                    return Ok(Some(
                        json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"Unknown Betterloop tool"}}),
                    ));
                }
            };
            json!({"content":[{"type":"text","text":serde_json::to_string(&content)?}]})
        }
        _ => {
            return Ok(Some(
                json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"Method not found"}}),
            ));
        }
    };
    Ok(Some(json!({"jsonrpc":"2.0","id":id,"result":result})))
}
/// Runs the local, credential-free MCP bridge. Signed policy remains in Mimir's private store.
///
/// # Errors
/// Returns an error for invalid verified configuration or unavailable local resources.
pub async fn serve_bridge(workspace: &Path) -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
    let mut input = tokio::io::BufReader::new(tokio::io::stdin());
    let mut output = tokio::io::stdout();
    loop {
        let mut frame = Vec::new();
        let read = (&mut input)
            .take(131_073)
            .read_until(b'\n', &mut frame)
            .await?;
        if read == 0 {
            break;
        }
        if frame.len() > 131_072 {
            return Err(MimirError::Configuration(
                "MCP request exceeds limit".into(),
            ));
        }
        let request: Value = serde_json::from_slice(&frame)?;
        let response = match bridge_request(&request, workspace) {
            Ok(response) => response,
            Err(_) => Some(
                json!({"jsonrpc":"2.0","id":request.get("id").cloned().unwrap_or(Value::Null),"error":{"code":-32000,"message":"Verified environment unavailable; run mimir enterprise explain"}}),
            ),
        };
        if let Some(response) = response {
            output
                .write_all(serde_json::to_string(&response)?.as_bytes())
                .await?;
            output.write_all(b"\n").await?;
            output.flush().await?;
        }
    }
    Ok(())
}
/// Project-owned MCP state is independent from the developer's shared user catalog.
/// # Errors
/// Returns an error if the workspace cannot be canonicalized.
pub(crate) fn native_mcp_state(state: &Path, workspace: &Path) -> Result<PathBuf> {
    let root = std::fs::canonicalize(workspace)?;
    let key = format!("{:x}", Sha256::digest(root.to_string_lossy().as_bytes()));
    Ok(state.join("projects").join(key).join("working-environment"))
}

/// Provisions only owned catalog entries. Existing unrelated servers are preserved.
///
/// # Errors
/// Returns an error for invalid verified configuration or unavailable local resources.
pub async fn provision_native_mcp(state: &Path, workspace: &Path) -> Result<()> {
    let Some(environment) = crate::enterprise::working_environment()? else {
        return Ok(());
    };
    provision_environment_mcp(state, workspace, &environment).await
}

async fn provision_environment_mcp(
    state: &Path,
    workspace: &Path,
    environment: &WorkingEnvironment,
) -> Result<()> {
    use crate::mcp::{McpCatalogHttp, McpCatalogServer, McpCatalogStdio, McpServerCatalog};
    let project_state = native_mcp_state(state, workspace)?;
    let state = project_state.as_path();
    let catalog = McpServerCatalog::new(state)?;
    let marker = state.join("enterprise-owned-mcp.json");
    let previous: BTreeMap<String, Value> = if marker.exists() {
        serde_json::from_slice(&std::fs::read(&marker)?)?
    } else {
        BTreeMap::new()
    };
    let mut desired = Vec::new();
    let mut fingerprints =
        BTreeMap::from([("betterloop".to_owned(), "local-betterloop-0.2.0".to_owned())]);
    let bridge_stdio = McpCatalogStdio {
        program: std::env::current_exe()?,
        args: vec![
            "--workspace".into(),
            workspace.to_string_lossy().into_owned(),
            "enterprise".into(),
            "bridge".into(),
        ],
        env: ["HOME", "PATH", "TMPDIR", "MIMIR_ENTERPRISE_STATE_DIR"]
            .into_iter()
            .filter(|key| std::env::var_os(key).is_some())
            .map(|key| (key.to_owned(), key.to_owned()))
            .collect(),
        ..McpCatalogStdio::default()
    };
    desired.push(McpCatalogServer::new(
        "betterloop",
        "Betterloop local bridge",
        bridge_stdio,
    )?);
    for server in &environment.mcp_servers {
        let key = server["key"]
            .as_str()
            .ok_or_else(|| MimirError::Configuration("MCP key missing".into()))?;
        // Namespace translation is reversible; dots and hyphens cannot collide.
        let native_key = format!(
            "bl_{}",
            key.replace('_', "_u").replace('.', "_d").replace('-', "_h")
        );
        fingerprints.insert(
            native_key.clone(),
            format!("{:x}", Sha256::digest(serde_json::to_vec(server)?)),
        );
        let label = server["label"].as_str().unwrap_or(key);
        let mut entry = if server["transport"] == "http" {
            let remote = McpCatalogHttp::new(server["url"].as_str().unwrap_or_default())?;
            McpCatalogServer::remote(native_key, label, remote)?
        } else {
            let mut stdio = McpCatalogStdio {
                program: PathBuf::from(server["command"].as_str().unwrap_or_default()),
                args: server["args"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect(),
                env: server["credential_ref"]
                    .as_str()
                    .map(|key| BTreeMap::from([(key.to_owned(), key.to_owned())]))
                    .unwrap_or_default(),
                ..McpCatalogStdio::default()
            };
            for key in ["HOME", "PATH", "TMPDIR"] {
                if std::env::var_os(key).is_some() {
                    stdio
                        .env
                        .entry(key.to_owned())
                        .or_insert_with(|| key.to_owned());
                }
            }
            McpCatalogServer::new(native_key, label, stdio)?
        };
        if server["transport"] == "http" {
            entry.bearer_token_env_var = server["credential_ref"].as_str().map(str::to_owned);
        }
        desired.push(entry);
    }
    validate_owned_collisions(&catalog, &desired, &previous).await?;
    reconcile_owned_mcp(&catalog, state, desired, previous, fingerprints).await
}

async fn reconcile_owned_mcp(
    catalog: &crate::mcp::McpServerCatalog,
    state: &Path,
    desired: Vec<crate::mcp::McpCatalogServer>,
    previous: BTreeMap<String, Value>,
    fingerprints: BTreeMap<String, String>,
) -> Result<()> {
    let mut owned = BTreeMap::new();
    let mut receipts = BTreeMap::new();
    for entry in desired {
        receipts.insert(entry.server.clone(), json!({"catalog":serde_json::to_value(&entry)?,"fingerprint":fingerprints.get(&entry.server)}));
        owned.insert(entry.server.clone(), serde_json::to_value(&entry)?);
        catalog.upsert(entry).await?;
    }
    for (key, value) in previous {
        if !owned.contains_key(&key)
            && catalog
                .get(&key)
                .await?
                .as_ref()
                .map(serde_json::to_value)
                .transpose()?
                .as_ref()
                == Some(&value)
        {
            catalog.remove(&key).await?;
        }
    }
    crate::atomic::write_json(&state.join("enterprise-owned-mcp.json"), &owned).await?;
    crate::atomic::write_json(&state.join("enterprise-owned-mcp-grants.json"), &receipts).await?;
    Ok(())
}

async fn validate_owned_collisions(
    catalog: &crate::mcp::McpServerCatalog,
    desired: &[crate::mcp::McpCatalogServer],
    previous: &BTreeMap<String, Value>,
) -> Result<()> {
    // Validate every collision before changing any catalog entry.
    for entry in desired {
        if let Some(existing) = catalog.get(&entry.server).await?
            && previous.get(&entry.server) != Some(&serde_json::to_value(existing)?)
        {
            return Err(MimirError::Configuration(format!(
                "MCP entry {} is unowned or changed; preserved",
                entry.server
            )));
        }
    }
    Ok(())
}
/// Removes unchanged Betterloop-owned servers while preserving local edits.
///
/// # Errors
/// Returns an error if the local owned catalog cannot be read or updated.
pub async fn remove_native_mcp(state: &Path, workspace: &Path) -> Result<()> {
    let project_state = native_mcp_state(state, workspace)?;
    let state = project_state.as_path();
    let marker = state.join("enterprise-owned-mcp.json");
    if !marker.exists() {
        return Ok(());
    }
    let owned: BTreeMap<String, Value> = serde_json::from_slice(&std::fs::read(&marker)?)?;
    let catalog = crate::mcp::McpServerCatalog::new(state)?;
    for (key, value) in owned {
        if catalog
            .get(&key)
            .await?
            .as_ref()
            .map(serde_json::to_value)
            .transpose()?
            .as_ref()
            == Some(&value)
        {
            catalog.remove(&key).await?;
        }
    }
    tokio::fs::remove_file(marker).await?;
    match tokio::fs::remove_file(state.join("enterprise-owned-mcp-grants.json")).await {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}
/// Provisions and initializes the approved bridge and selected organization servers.
///
/// # Errors
/// Returns an error for unavailable signed policy or conflicting owned configuration.
pub async fn prepare_native(state: &Path, workspace: &Path) -> Result<Value> {
    let manager = crate::enterprise::EnterpriseManager::global()?;
    let release_id = manager.load_profile()?.release_id;
    let environment = crate::enterprise::working_environment()?
        .ok_or_else(|| MimirError::Configuration("Mimir is not enrolled".into()))?;
    ensure_preparation_release(release_id, manager.load_profile()?.release_id)?;
    provision_environment_mcp(state, workspace, &environment).await?;
    let project_state = native_mcp_state(state, workspace)?;
    let catalog = crate::mcp::McpServerCatalog::new(&project_state)?;
    let mut issues = crate::registry::readiness_issues(workspace)?;
    for server in std::iter::once(json!({"key":"betterloop","enforcement":"mandatory","expected_tools":["environment_status","repository_context","list_skills","activate_skill"]})).chain(environment.mcp_servers.iter().cloned()) {
        let key = server["key"].as_str().unwrap_or_default();
        let native_key = if key == "betterloop" { key.to_owned() } else {format!("bl_{}",key.replace('_',"_u").replace('.',"_d").replace('-',"_h"))};
        let probe = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let entry = catalog.get(&native_key).await?.ok_or_else(|| MimirError::Configuration("Owned MCP entry missing".into()))?;
            let mut client = crate::mcp::connect_catalog_entry(&entry,&project_state,true).await?;
            let tools = client.list_tools().await?;
            if server["expected_tools"].as_array().into_iter().flatten().any(|name|!tools.iter().any(|tool|Some(tool.name.as_str())==name.as_str())) { return Err(MimirError::Configuration("Required MCP tools are missing".into())); }
            Ok(())
        }).await;
        if !matches!(probe,Ok(Ok(()))) { issues.push(json!({"key":key,"required":server["enforcement"]=="mandatory","reason_code":"mcp_unverified"})); }
    }
    let facts = repository_facts(workspace)?;
    if facts.truncated {
        issues.push(json!({"key":"repository.discovery","required":false,"reason_code":"repository_bounded"}));
    }
    for workflow in &environment.workflows {
        if workflow["enforcement"] == "mandatory"
            && matches!(
                workflow["level"].as_str(),
                Some("task" | "completion" | "continuity")
            )
        {
            issues.push(
                json!({"key":workflow["key"],"required":true,"reason_code":"workflow_advisory"}),
            );
        }
    }
    let skills = template_skills(&environment, &facts)?;
    let skill_count = skills.len() + crate::enterprise::managed_skills_for(workspace)?.len();
    let digest = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(
            &json!({"environment":environment,"repository":facts.fingerprint})
        )?)
    );
    let state_name = if issues.iter().any(|issue| issue["required"] == true) {
        "not_ready"
    } else if issues.is_empty() {
        "ready"
    } else {
        "degraded"
    };
    ensure_preparation_release(release_id, manager.load_profile()?.release_id)?;
    let report = json!({"schema":1,"release_id":release_id,"runtime":{"harness":"mimir","harness_version":env!("CARGO_PKG_VERSION"),"adapter_version":"0.2.0"},"bundle_digest":digest,"state":state_name,"activation":"active","skill_count":skill_count,"issues":issues});
    crate::atomic::write_json(
        &project_state.join("enterprise-environment-readiness.json"),
        &report,
    )
    .await?;
    let reported = crate::enterprise::EnterpriseManager::global()?
        .report_environment(report.clone())
        .await
        .is_ok();
    Ok(
        json!({"readiness":report,"reported":reported,"evidence":"client_reported: native bridge initialization and local signed environment checks"}),
    )
}

fn ensure_preparation_release(expected: uuid::Uuid, current: uuid::Uuid) -> Result<()> {
    if expected != current {
        return Err(MimirError::Configuration(
            "Signed profile changed during preparation; retry prepare".into(),
        ));
    }
    Ok(())
}

/// Captures signed MCP identity so live connections cannot outlast revocation or replacement.
///
/// # Errors
/// Returns an error for invalid verified configuration or unavailable local resources.
pub fn managed_mcp_fingerprint(server: &str) -> Result<Option<String>> {
    let Some(environment) = crate::enterprise::working_environment()? else {
        return Ok(None);
    };
    if server == "betterloop" {
        return Ok(Some("local-betterloop-0.2.0".into()));
    }
    for item in environment.mcp_servers {
        let key = item["key"].as_str().unwrap_or_default();
        let native = format!(
            "bl_{}",
            key.replace('_', "_u").replace('.', "_d").replace('-', "_h")
        );
        if native == server {
            return Ok(Some(format!(
                "{:x}",
                Sha256::digest(serde_json::to_vec(&item)?)
            )));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preparation_evidence_cannot_be_relabelled_after_sync() {
        let release = uuid::Uuid::new_v4();
        assert!(ensure_preparation_release(release, release).is_ok());
        assert!(ensure_preparation_release(release, uuid::Uuid::new_v4()).is_err());
    }
    #[test]
    fn shared_defaults_render_five_starter_skills() {
        let env = defaults();
        validate(&env).unwrap();
        let facts = RepositoryFacts {
            guidance: vec![],
            documents: vec![],
            packages: vec![],
            fingerprint: String::new(),
            truncated: false,
        };
        assert_eq!(template_skills(&env, &facts).unwrap().len(), 5);
        assert!(startup_context(&env, &facts).contains("developer authorization"));
    }
    #[test]
    fn invalid_delivery_and_templates_are_rejected() {
        let mut env = defaults();
        env.delivery["merge"] = json!("automatic");
        assert!(validate(&env).is_err());
        let mut env = defaults();
        env.skills[0].template_id = "org.unknown".into();
        assert!(validate(&env).is_err());
    }
    #[test]
    fn repository_discovery_does_not_execute_scripts() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"dependencies":{"react":"1"},"scripts":{"build":"touch SHOULD_NOT_RUN"}}"#,
        )
        .unwrap();
        let facts = repository_facts(dir.path()).unwrap();
        assert_eq!(facts.packages[0].stack, vec!["React"]);
        assert!(!dir.path().join("SHOULD_NOT_RUN").exists());
    }
}
