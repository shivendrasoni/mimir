use std::{
    collections::{BTreeMap, BTreeSet},
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
};

use serde::Deserialize;
use serde_json::Value;
use thiserror::Error;
use walkdir::WalkDir;

#[derive(Debug, Error)]
pub enum ResourceError {
    #[error("resource path error: {0}")]
    Path(String),
    #[error("invalid skill at {path}: {message}")]
    Skill { path: PathBuf, message: String },
    #[error("invalid {kind} resource at {path}: {message}")]
    Resource {
        kind: &'static str,
        path: PathBuf,
        message: String,
    },
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub const MAX_SKILL_NAME_BYTES: usize = 64;
pub const MAX_SKILL_DESCRIPTION_BYTES: usize = 1_024;
const MAX_SKILL_FRONTMATTER_BYTES: usize = 16 * 1_024;
const MAX_MIGRATED_SKILLS: usize = 512;
const MAX_RESOURCE_FILES: usize = 512;
const MAX_RESOURCE_ROOTS: usize = 64;
const MAX_CONTEXT_FILE_BYTES: u64 = 256 * 1024;
const MAX_SYSTEM_PROMPT_BYTES: u64 = 256 * 1024;
const MAX_PROMPT_TEMPLATE_BYTES: u64 = 64 * 1024;
const MAX_THEME_BYTES: u64 = 128 * 1024;
const MAX_PACKAGE_MANIFEST_BYTES: u64 = 64 * 1024;
const MAX_RESOURCE_PATH_BYTES: usize = 4 * 1024;
const MAX_PROJECT_RESOURCE_DEPTH: usize = 128;
const MAX_TOTAL_CONTEXT_BYTES: usize = 2 * 1024 * 1024;
const MAX_TOTAL_CATALOG_BYTES: usize = 8 * 1024 * 1024;
const MIMIR_CONFIG_DIR: &str = ".mimir/agent";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
    instructions: SkillInstructions,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SkillInstructions {
    File { allowed_root: PathBuf },
    Inline(String),
}

impl Skill {
    /// Creates an in-memory skill for embedders that do not load from a `SKILL.md` file.
    #[must_use]
    pub fn in_memory(
        name: impl Into<String>,
        description: impl Into<String>,
        body: impl Into<String>,
        path: PathBuf,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            path,
            instructions: SkillInstructions::Inline(body.into()),
        }
    }

    fn from_file(name: String, description: String, path: PathBuf, allowed_root: PathBuf) -> Self {
        Self {
            name,
            description,
            path,
            instructions: SkillInstructions::File { allowed_root },
        }
    }

    /// Loads and revalidates instructions only when the runtime activates this skill.
    pub(crate) fn load_instructions(
        &self,
        max_instruction_bytes: usize,
    ) -> Result<String, ResourceError> {
        let SkillInstructions::File { allowed_root } = &self.instructions else {
            let SkillInstructions::Inline(body) = &self.instructions else {
                unreachable!("skill instruction source is exhaustive");
            };
            return Ok(body.clone());
        };
        let canonical = self.path.canonicalize()?;
        let allowed_root = allowed_root.canonicalize()?;
        if canonical != self.path || !canonical.starts_with(&allowed_root) {
            return Err(skill_error(
                &self.path,
                "skill source changed or resolves outside its validated resource root",
            ));
        }
        let metadata = canonical.metadata()?;
        if !metadata.is_file() {
            return Err(skill_error(
                &self.path,
                "skill source is not a regular file",
            ));
        }
        let max_file_bytes = max_instruction_bytes
            .saturating_add(MAX_SKILL_FRONTMATTER_BYTES)
            .saturating_add(16);
        let max_file_bytes_u64 = u64::try_from(max_file_bytes).unwrap_or(u64::MAX);
        if metadata.len() > max_file_bytes_u64 {
            return Err(skill_error(
                &self.path,
                &format!(
                    "skill source is {} bytes and exceeds the bounded activation read of {max_file_bytes} bytes; keep SKILL.md concise and move detailed material into referenced files",
                    metadata.len()
                ),
            ));
        }
        let mut bytes = Vec::with_capacity(max_file_bytes.min(64 * 1024));
        std::fs::File::open(&canonical)?
            .take(max_file_bytes_u64.saturating_add(1))
            .read_to_end(&mut bytes)?;
        if bytes.len() > max_file_bytes {
            return Err(skill_error(
                &self.path,
                &format!(
                    "skill source exceeds the bounded activation read of {max_file_bytes} bytes; keep SKILL.md concise and move detailed material into referenced files"
                ),
            ));
        }
        let raw_content = String::from_utf8(bytes)
            .map_err(|_| skill_error(&self.path, "skill source must be UTF-8 text"))?;
        let normalized = raw_content
            .contains("\r\n")
            .then(|| raw_content.replace("\r\n", "\n"));
        let content = normalized.as_deref().unwrap_or(&raw_content);
        let (metadata, body) = parse_skill_document(&self.path, content)?;
        let description = metadata
            .description
            .unwrap_or_else(|| format!("Migrated legacy skill {}", metadata.name));
        if metadata.name != self.name || description != self.description {
            return Err(skill_error(
                &self.path,
                "skill metadata changed after discovery; restart Mimir to reload the catalog",
            ));
        }
        let body = body.trim();
        if body.is_empty() {
            return Err(skill_error(&self.path, "instructions must not be blank"));
        }
        Ok(body.to_owned())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptTemplate {
    pub name: String,
    pub description: String,
    pub argument_hint: Option<String>,
    pub content: String,
    pub path: PathBuf,
}

impl PromptTemplate {
    /// Expands positional and aggregate argument placeholders without recursively
    /// interpreting placeholder-looking text inside argument values.
    #[must_use]
    pub fn expand(&self, arguments: &[String]) -> String {
        substitute_template_arguments(&self.content, arguments)
    }
}

/// Expands a slash-command prompt template, or returns the original input when
/// no loaded template has that name.
#[must_use]
pub fn expand_prompt_template(input: &str, templates: &[PromptTemplate]) -> String {
    let Some(command) = input.strip_prefix('/') else {
        return input.into();
    };
    let split = command.find(char::is_whitespace).unwrap_or(command.len());
    let name = &command[..split];
    let Some(template) = templates.iter().find(|template| template.name == name) else {
        return input.into();
    };
    let arguments = parse_prompt_arguments(command[split..].trim_start());
    template.expand(&arguments)
}

/// Parses prompt-template arguments with bounded shell-like single/double quote
/// grouping. Backslashes and command substitution have no special authority.
#[must_use]
pub fn parse_prompt_arguments(input: &str) -> Vec<String> {
    let mut arguments = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    for character in input.chars() {
        if let Some(expected) = quote {
            if character == expected {
                quote = None;
            } else {
                current.push(character);
            }
            continue;
        }
        match character {
            '\'' | '"' => quote = Some(character),
            value if value.is_whitespace() => {
                if !current.is_empty() {
                    arguments.push(std::mem::take(&mut current));
                }
            }
            value => current.push(value),
        }
    }
    if !current.is_empty() {
        arguments.push(current);
    }
    arguments
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomTheme {
    pub name: String,
    pub definition: Value,
    pub path: PathBuf,
}

#[derive(Debug, Clone)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent discovery switches mirror the CLI's fail-closed --no-* resource flags"
)]
pub struct ResourceLoaderOptions {
    /// Lowest-precedence bundled/global resource directory.
    pub global_dir: Option<PathBuf>,
    /// User resource directory, normally `~/.agents`.
    pub user_dir: Option<PathBuf>,
    /// Local package roots containing `package.json` Mimir/Pi manifests.
    pub package_paths: Vec<PathBuf>,
    /// Highest-precedence skill files or directories, matching repeated `--skill`.
    pub explicit_skill_paths: Vec<PathBuf>,
    /// Highest-precedence prompt files or directories, matching `--prompt-template`.
    pub explicit_prompt_template_paths: Vec<PathBuf>,
    /// Highest-precedence theme files or directories, matching repeated `--theme`.
    pub explicit_theme_paths: Vec<PathBuf>,
    pub discover_context_files: bool,
    pub discover_skills: bool,
    pub discover_prompt_templates: bool,
    pub discover_themes: bool,
}

impl Default for ResourceLoaderOptions {
    fn default() -> Self {
        Self {
            global_dir: None,
            user_dir: None,
            package_paths: Vec::new(),
            explicit_skill_paths: Vec::new(),
            explicit_prompt_template_paths: Vec::new(),
            explicit_theme_paths: Vec::new(),
            discover_context_files: true,
            discover_skills: true,
            discover_prompt_templates: true,
            discover_themes: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resources {
    pub system_context: String,
    pub context_files: Vec<PathBuf>,
    pub skills: Vec<Skill>,
    pub system_prompt: Option<String>,
    pub system_prompt_file: Option<PathBuf>,
    pub append_system_prompt: Vec<String>,
    pub append_system_prompt_files: Vec<PathBuf>,
    pub prompt_templates: Vec<PromptTemplate>,
    pub themes: Vec<CustomTheme>,
    pub package_manifests: Vec<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SkillSourcePolicy {
    Strict,
    SharedUser,
}

pub struct ResourceLoader {
    boundary: PathBuf,
    cwd: PathBuf,
    options: ResourceLoaderOptions,
}

impl ResourceLoader {
    /// Creates a loader constrained to a canonical directory boundary.
    ///
    /// # Errors
    ///
    /// Returns an error when the working directory is outside the boundary.
    pub fn new(boundary: &std::path::Path, cwd: &std::path::Path) -> Result<Self, ResourceError> {
        let boundary = boundary.canonicalize()?;
        let cwd = cwd.canonicalize()?;
        if !cwd.starts_with(&boundary) {
            return Err(ResourceError::Path(format!(
                "{} is outside {}",
                cwd.display(),
                boundary.display()
            )));
        }
        Ok(Self {
            boundary,
            cwd,
            options: ResourceLoaderOptions::default(),
        })
    }

    /// Creates a loader with global, user, package, and explicit resource roots.
    ///
    /// # Errors
    ///
    /// Returns an error when the project working directory is outside its boundary
    /// or the configured root count exceeds the fixed discovery bound.
    pub fn with_options(
        boundary: &Path,
        cwd: &Path,
        options: ResourceLoaderOptions,
    ) -> Result<Self, ResourceError> {
        let root_count = usize::from(options.global_dir.is_some())
            + usize::from(options.user_dir.is_some())
            + options.package_paths.len()
            + options.explicit_skill_paths.len()
            + options.explicit_prompt_template_paths.len()
            + options.explicit_theme_paths.len();
        if root_count > MAX_RESOURCE_ROOTS {
            return Err(ResourceError::Path(format!(
                "resource root count exceeds {MAX_RESOURCE_ROOTS}"
            )));
        }
        let mut loader = Self::new(boundary, cwd)?;
        loader.options = options;
        Ok(loader)
    }

    /// Loads ordered context files and nearest-wins skills.
    ///
    /// # Errors
    ///
    /// Returns an I/O or skill-frontmatter validation error.
    #[allow(
        clippy::too_many_lines,
        reason = "resource precedence stays linear and auditable: global, packages, user, project ancestors, then explicit paths"
    )]
    pub fn load(&self) -> Result<Resources, ResourceError> {
        let directories = self.directories()?;
        let mut context_files = Vec::new();
        let mut context_parts = Vec::new();
        let mut skills = BTreeMap::new();
        let mut prompts = BTreeMap::new();
        let mut themes = BTreeMap::new();
        let mut system_prompt = None;
        let mut system_prompt_file = None;
        let mut append_system_prompt = Vec::new();
        let mut append_system_prompt_files = Vec::new();
        let mut package_manifests = Vec::new();
        let mut seen_context = BTreeSet::new();

        if let Some(global) = self.options.global_dir.as_deref() {
            self.load_config_root(
                global,
                SkillSourcePolicy::Strict,
                &mut skills,
                &mut prompts,
                &mut themes,
                &mut system_prompt,
                &mut system_prompt_file,
                &mut append_system_prompt,
                &mut append_system_prompt_files,
            )?;
            if self.options.discover_context_files {
                self.load_context(
                    global,
                    &mut seen_context,
                    &mut context_files,
                    &mut context_parts,
                )?;
            }
        }

        for package in &self.options.package_paths {
            self.load_package(
                package,
                &mut skills,
                &mut prompts,
                &mut themes,
                &mut package_manifests,
            )?;
        }

        if let Some(user) = self.options.user_dir.as_deref() {
            self.load_config_root(
                user,
                SkillSourcePolicy::SharedUser,
                &mut skills,
                &mut prompts,
                &mut themes,
                &mut system_prompt,
                &mut system_prompt_file,
                &mut append_system_prompt,
                &mut append_system_prompt_files,
            )?;
            if self.options.discover_context_files {
                self.load_context(
                    user,
                    &mut seen_context,
                    &mut context_files,
                    &mut context_parts,
                )?;
            }
        }

        for directory in directories {
            if self.options.discover_context_files {
                self.load_context(
                    &directory,
                    &mut seen_context,
                    &mut context_files,
                    &mut context_parts,
                )?;
            }
            if self.options.discover_skills {
                Self::load_skill_path(
                    &directory.join(".agents/skills"),
                    &self.boundary,
                    &mut skills,
                    SkillSourcePolicy::Strict,
                )?;
            }
            self.load_config_root(
                &directory.join(MIMIR_CONFIG_DIR),
                SkillSourcePolicy::Strict,
                &mut skills,
                &mut prompts,
                &mut themes,
                &mut system_prompt,
                &mut system_prompt_file,
                &mut append_system_prompt,
                &mut append_system_prompt_files,
            )?;
        }

        for path in &self.options.explicit_skill_paths {
            self.load_explicit_skill(path, &mut skills)?;
        }
        for path in &self.options.explicit_prompt_template_paths {
            self.load_explicit_prompt(path, &mut prompts)?;
        }
        for path in &self.options.explicit_theme_paths {
            self.load_explicit_theme(path, &mut themes)?;
        }

        Ok(Resources {
            system_context: context_parts.join("\n\n"),
            context_files,
            skills: skills.into_values().collect(),
            system_prompt,
            system_prompt_file,
            append_system_prompt,
            append_system_prompt_files,
            prompt_templates: prompts.into_values().collect(),
            themes: themes.into_values().collect(),
            package_manifests,
        })
    }

    fn directories(&self) -> Result<Vec<PathBuf>, ResourceError> {
        let mut directories = vec![self.boundary.clone()];
        let relative = self
            .cwd
            .strip_prefix(&self.boundary)
            .unwrap_or_else(|_| std::path::Path::new(""));
        let mut current = self.boundary.clone();
        for component in relative.components() {
            current.push(component);
            directories.push(current.clone());
            if directories.len() > MAX_PROJECT_RESOURCE_DEPTH {
                return Err(ResourceError::Path(format!(
                    "project resource depth exceeds {MAX_PROJECT_RESOURCE_DEPTH} directories"
                )));
            }
        }
        Ok(directories)
    }

    fn load_context(
        &self,
        directory: &Path,
        seen: &mut BTreeSet<PathBuf>,
        paths: &mut Vec<PathBuf>,
        parts: &mut Vec<String>,
    ) -> Result<(), ResourceError> {
        let Some(candidate) = case_insensitive_context_file(directory)? else {
            return Ok(());
        };
        let allowed_root = if directory.starts_with(&self.boundary) {
            &self.boundary
        } else {
            directory
        };
        let path =
            checked_resource_file(&candidate, allowed_root, MAX_CONTEXT_FILE_BYTES, "context")?;
        if !seen.insert(path.clone()) {
            return Ok(());
        }
        let body = std::fs::read_to_string(&path)?;
        let total_bytes = parts
            .iter()
            .map(String::len)
            .sum::<usize>()
            .saturating_add(body.len());
        if total_bytes > MAX_TOTAL_CONTEXT_BYTES {
            return Err(ResourceError::Path(format!(
                "combined context files exceed {MAX_TOTAL_CONTEXT_BYTES} bytes"
            )));
        }
        parts.push(format!("# Source: {}\n{body}", path.display()));
        paths.push(path);
        Ok(())
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "the loader keeps each independently bounded catalog explicit at the discovery boundary"
    )]
    fn load_config_root(
        &self,
        root: &Path,
        skill_policy: SkillSourcePolicy,
        skills: &mut BTreeMap<String, Skill>,
        prompts: &mut BTreeMap<String, PromptTemplate>,
        themes: &mut BTreeMap<String, CustomTheme>,
        system_prompt: &mut Option<String>,
        system_prompt_file: &mut Option<PathBuf>,
        append_system_prompt: &mut Vec<String>,
        append_system_prompt_files: &mut Vec<PathBuf>,
    ) -> Result<(), ResourceError> {
        if !root.exists() {
            return Ok(());
        }
        let root = canonical_directory(root, "resource root")?;
        if self.options.discover_skills {
            Self::load_skill_path(&root.join("skills"), &root, skills, skill_policy)?;
        }
        if self.options.discover_prompt_templates {
            Self::load_prompt_path(&root.join("prompts"), &root, prompts)?;
        }
        if self.options.discover_themes {
            Self::load_theme_path(&root.join("themes"), &root, themes)?;
        }
        let system = root.join("SYSTEM.md");
        if system.is_file() {
            let path =
                checked_resource_file(&system, &root, MAX_SYSTEM_PROMPT_BYTES, "system prompt")?;
            *system_prompt = Some(std::fs::read_to_string(&path)?);
            *system_prompt_file = Some(path);
        }
        let append = root.join("APPEND_SYSTEM.md");
        if append.is_file() {
            let path = checked_resource_file(
                &append,
                &root,
                MAX_SYSTEM_PROMPT_BYTES,
                "append system prompt",
            )?;
            *append_system_prompt = vec![std::fs::read_to_string(&path)?];
            *append_system_prompt_files = vec![path];
        }
        Ok(())
    }

    fn load_explicit_skill(
        &self,
        path: &Path,
        skills: &mut BTreeMap<String, Skill>,
    ) -> Result<(), ResourceError> {
        let path = self.resolve_explicit_path(path)?;
        let root = explicit_allowed_root(&path)?;
        Self::load_skill_path(&path, &root, skills, SkillSourcePolicy::Strict)
    }

    fn load_explicit_prompt(
        &self,
        path: &Path,
        prompts: &mut BTreeMap<String, PromptTemplate>,
    ) -> Result<(), ResourceError> {
        let path = self.resolve_explicit_path(path)?;
        let root = explicit_allowed_root(&path)?;
        Self::load_prompt_path(&path, &root, prompts)
    }

    fn load_explicit_theme(
        &self,
        path: &Path,
        themes: &mut BTreeMap<String, CustomTheme>,
    ) -> Result<(), ResourceError> {
        let path = self.resolve_explicit_path(path)?;
        let root = explicit_allowed_root(&path)?;
        Self::load_theme_path(&path, &root, themes)
    }

    fn resolve_explicit_path(&self, path: &Path) -> Result<PathBuf, ResourceError> {
        if path.as_os_str().is_empty() {
            return Err(ResourceError::Path(
                "explicit resource path is blank".into(),
            ));
        }
        let expanded = expand_home_path(path);
        let resolved = if expanded.is_absolute() {
            expanded
        } else {
            self.cwd.join(expanded)
        };
        if !resolved.exists() {
            return Err(ResourceError::Path(format!(
                "explicit resource path does not exist: {}",
                resolved.display()
            )));
        }
        if std::fs::symlink_metadata(&resolved)?
            .file_type()
            .is_symlink()
        {
            return Err(ResourceError::Path(format!(
                "explicit resource path must not be a symlink: {}",
                resolved.display()
            )));
        }
        Ok(resolved)
    }

    fn load_skill_path(
        path: &Path,
        root: &Path,
        skills: &mut BTreeMap<String, Skill>,
        policy: SkillSourcePolicy,
    ) -> Result<(), ResourceError> {
        if !path.exists() {
            return Ok(());
        }
        let mut files = if path.is_file() {
            vec![(path.to_path_buf(), root.to_path_buf())]
        } else if path.join("SKILL.md").is_file() {
            vec![(path.join("SKILL.md"), root.to_path_buf())]
        } else {
            let mut files = Vec::new();
            for entry in sorted_entries(path)? {
                let file = entry.join("SKILL.md");
                if !file.is_file() {
                    continue;
                }
                let allowed_root = if matches!(policy, SkillSourcePolicy::SharedUser)
                    && std::fs::symlink_metadata(&entry)?.file_type().is_symlink()
                {
                    canonical_directory(&entry, "linked user skill")?
                } else {
                    root.to_path_buf()
                };
                files.push((file, allowed_root));
            }
            files
        };
        files.sort_by(|left, right| left.0.cmp(&right.0));
        for (file, allowed_root) in files {
            let (file, allowed_root) = checked_skill_source(&file, &allowed_root)?;
            let skill = discover_skill(&file, &allowed_root, policy)?;
            skills.insert(skill.name.clone(), skill);
            ensure_catalog_bound(skills.len(), "skill")?;
            ensure_catalog_bytes(
                skills
                    .values()
                    .map(|skill| skill.name.len().saturating_add(skill.description.len())),
                "skill",
            )?;
        }
        Ok(())
    }

    fn load_prompt_path(
        path: &Path,
        root: &Path,
        prompts: &mut BTreeMap<String, PromptTemplate>,
    ) -> Result<(), ResourceError> {
        if !path.exists() {
            return Ok(());
        }
        let mut files = resource_files(path, "md")?;
        files.sort();
        for file in files {
            let file =
                checked_resource_file(&file, root, MAX_PROMPT_TEMPLATE_BYTES, "prompt template")?;
            let prompt = parse_prompt_template(&file)?;
            prompts.insert(prompt.name.clone(), prompt);
            ensure_catalog_bound(prompts.len(), "prompt template")?;
            ensure_catalog_bytes(
                prompts.values().map(|prompt| prompt.content.len()),
                "prompt template",
            )?;
        }
        Ok(())
    }

    fn load_theme_path(
        path: &Path,
        root: &Path,
        themes: &mut BTreeMap<String, CustomTheme>,
    ) -> Result<(), ResourceError> {
        if !path.exists() {
            return Ok(());
        }
        let mut files = resource_files(path, "json")?;
        files.sort();
        for file in files {
            let file = checked_resource_file(&file, root, MAX_THEME_BYTES, "theme")?;
            let theme = parse_theme(&file)?;
            themes.insert(theme.name.clone(), theme);
            ensure_catalog_bound(themes.len(), "theme")?;
            ensure_catalog_bytes(
                themes.values().map(|theme| {
                    serde_json::to_string(&theme.definition).map_or(0, |json| json.len())
                }),
                "theme",
            )?;
        }
        Ok(())
    }

    fn load_package(
        &self,
        package: &Path,
        skills: &mut BTreeMap<String, Skill>,
        prompts: &mut BTreeMap<String, PromptTemplate>,
        themes: &mut BTreeMap<String, CustomTheme>,
        manifests: &mut Vec<PathBuf>,
    ) -> Result<(), ResourceError> {
        let package = self.resolve_explicit_path(package)?;
        let package = canonical_directory(&package, "package")?;
        let manifest_path = package.join("package.json");
        let manifest = if manifest_path.is_file() {
            let manifest_path = checked_resource_file(
                &manifest_path,
                &package,
                MAX_PACKAGE_MANIFEST_BYTES,
                "package manifest",
            )?;
            let manifest: PackageJson =
                serde_json::from_str(&std::fs::read_to_string(&manifest_path)?).map_err(
                    |error| resource_error("package manifest", &manifest_path, &error.to_string()),
                )?;
            manifests.push(manifest_path);
            manifest.pi.or(manifest.mimir)
        } else {
            None
        };
        let manifest = manifest.unwrap_or_default();
        let skill_paths = manifest.skills.unwrap_or_else(|| vec!["skills".into()]);
        let prompt_paths = manifest.prompts.unwrap_or_else(|| vec!["prompts".into()]);
        let theme_paths = manifest.themes.unwrap_or_else(|| vec!["themes".into()]);
        ensure_manifest_entry_bound(&skill_paths, "skills")?;
        ensure_manifest_entry_bound(&prompt_paths, "prompts")?;
        ensure_manifest_entry_bound(&theme_paths, "themes")?;
        if self.options.discover_skills {
            for entry in skill_paths {
                Self::load_skill_path(
                    &checked_package_entry(&package, &entry)?,
                    &package,
                    skills,
                    SkillSourcePolicy::Strict,
                )?;
            }
        }
        if self.options.discover_prompt_templates {
            for entry in prompt_paths {
                Self::load_prompt_path(
                    &checked_package_entry(&package, &entry)?,
                    &package,
                    prompts,
                )?;
            }
        }
        if self.options.discover_themes {
            for entry in theme_paths {
                Self::load_theme_path(&checked_package_entry(&package, &entry)?, &package, themes)?;
            }
        }
        Ok(())
    }
}

#[derive(Debug, Default, Deserialize)]
struct PackageResourceManifest {
    skills: Option<Vec<String>>,
    prompts: Option<Vec<String>>,
    themes: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
struct PackageJson {
    pi: Option<PackageResourceManifest>,
    #[serde(rename = "mimir")]
    mimir: Option<PackageResourceManifest>,
}

#[derive(Debug, Default, Deserialize)]
struct PromptMetadata {
    #[serde(default)]
    description: Option<String>,
    #[serde(default, rename = "argument-hint")]
    argument_hint: Option<String>,
}

fn canonical_directory(path: &Path, kind: &str) -> Result<PathBuf, ResourceError> {
    let canonical = path.canonicalize()?;
    if !canonical.is_dir() {
        return Err(ResourceError::Path(format!(
            "{kind} path is not a directory: {}",
            path.display()
        )));
    }
    Ok(canonical)
}

fn explicit_allowed_root(path: &Path) -> Result<PathBuf, ResourceError> {
    if path.is_dir() {
        path.canonicalize().map_err(ResourceError::from)
    } else {
        path.parent()
            .ok_or_else(|| ResourceError::Path("explicit resource file has no parent".into()))?
            .canonicalize()
            .map_err(ResourceError::from)
    }
}

fn checked_resource_file(
    path: &Path,
    root: &Path,
    max_bytes: u64,
    kind: &'static str,
) -> Result<PathBuf, ResourceError> {
    if path.as_os_str().as_encoded_bytes().len() > MAX_RESOURCE_PATH_BYTES {
        return Err(ResourceError::Path(format!(
            "resource path exceeds {MAX_RESOURCE_PATH_BYTES} bytes"
        )));
    }
    let root = root.canonicalize()?;
    let canonical = path.canonicalize()?;
    if !canonical.starts_with(&root) {
        return Err(ResourceError::Path(format!(
            "{} resolves outside {}",
            path.display(),
            root.display()
        )));
    }
    let metadata = canonical.metadata()?;
    if !metadata.is_file() {
        return Err(resource_error(kind, path, "resource is not a regular file"));
    }
    if metadata.len() > max_bytes {
        return Err(resource_error(
            kind,
            path,
            &format!("resource exceeds {max_bytes} bytes"),
        ));
    }
    Ok(canonical)
}

fn checked_skill_source(path: &Path, root: &Path) -> Result<(PathBuf, PathBuf), ResourceError> {
    if path.as_os_str().as_encoded_bytes().len() > MAX_RESOURCE_PATH_BYTES {
        return Err(ResourceError::Path(format!(
            "resource path exceeds {MAX_RESOURCE_PATH_BYTES} bytes"
        )));
    }
    let root = root.canonicalize()?;
    let canonical = path.canonicalize()?;
    if !canonical.starts_with(&root) {
        return Err(ResourceError::Path(format!(
            "{} resolves outside {}",
            path.display(),
            root.display()
        )));
    }
    if !canonical.metadata()?.is_file() {
        return Err(resource_error(
            "skill",
            path,
            "resource is not a regular file",
        ));
    }
    Ok((canonical, root))
}

fn sorted_entries(directory: &Path) -> Result<Vec<PathBuf>, ResourceError> {
    if !directory.is_dir() {
        return Ok(Vec::new());
    }
    let mut entries = std::fs::read_dir(directory)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    entries.sort_by(|left, right| left.file_name().cmp(&right.file_name()));
    Ok(entries)
}

fn resource_files(path: &Path, extension: &str) -> Result<Vec<PathBuf>, ResourceError> {
    if path.is_file() {
        return Ok(
            (path.extension().and_then(std::ffi::OsStr::to_str) == Some(extension))
                .then(|| path.to_path_buf())
                .into_iter()
                .collect(),
        );
    }
    Ok(sorted_entries(path)?
        .into_iter()
        .filter(|file| {
            file.is_file() && file.extension().and_then(std::ffi::OsStr::to_str) == Some(extension)
        })
        .collect())
}

fn case_insensitive_context_file(directory: &Path) -> Result<Option<PathBuf>, ResourceError> {
    let entries = sorted_entries(directory)?;
    for target in ["agents.md", "claude.md"] {
        if let Some(path) = entries.iter().find(|path| {
            path.file_name()
                .and_then(std::ffi::OsStr::to_str)
                .is_some_and(|name| name.eq_ignore_ascii_case(target))
        }) {
            return Ok(Some(path.clone()));
        }
    }
    Ok(None)
}

fn ensure_catalog_bound(count: usize, kind: &str) -> Result<(), ResourceError> {
    if count > MAX_RESOURCE_FILES {
        return Err(ResourceError::Path(format!(
            "{kind} catalog exceeds {MAX_RESOURCE_FILES} entries"
        )));
    }
    Ok(())
}

fn ensure_catalog_bytes(
    sizes: impl Iterator<Item = usize>,
    kind: &str,
) -> Result<(), ResourceError> {
    let total = sizes.fold(0_usize, usize::saturating_add);
    if total > MAX_TOTAL_CATALOG_BYTES {
        return Err(ResourceError::Path(format!(
            "combined {kind} resources exceed {MAX_TOTAL_CATALOG_BYTES} bytes"
        )));
    }
    Ok(())
}

fn ensure_manifest_entry_bound(entries: &[String], kind: &str) -> Result<(), ResourceError> {
    if entries.len() > MAX_RESOURCE_FILES {
        return Err(ResourceError::Path(format!(
            "package {kind} manifest exceeds {MAX_RESOURCE_FILES} entries"
        )));
    }
    Ok(())
}

fn checked_package_entry(root: &Path, entry: &str) -> Result<PathBuf, ResourceError> {
    let entry_path = Path::new(entry);
    if entry.trim().is_empty()
        || entry.len() > MAX_RESOURCE_PATH_BYTES
        || entry_path.is_absolute()
        || entry_path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(ResourceError::Path(
            "package resource entries must be non-empty relative paths".into(),
        ));
    }
    let joined = root.join(entry);
    if !joined.exists() {
        return Ok(joined);
    }
    let canonical = joined.canonicalize()?;
    if !canonical.starts_with(root) {
        return Err(ResourceError::Path(format!(
            "package resource entry escapes package root: {entry}"
        )));
    }
    Ok(canonical)
}

fn expand_home_path(path: &Path) -> PathBuf {
    let Some(value) = path.to_str() else {
        return path.to_owned();
    };
    let Some(home) = std::env::var_os("HOME") else {
        return path.to_owned();
    };
    if value == "~" {
        return PathBuf::from(home);
    }
    value.strip_prefix("~/").map_or_else(
        || path.to_owned(),
        |suffix| PathBuf::from(home).join(suffix),
    )
}

fn parse_prompt_template(path: &Path) -> Result<PromptTemplate, ResourceError> {
    let raw = std::fs::read_to_string(path)?;
    let normalized = raw.replace("\r\n", "\n");
    let (metadata, body) = if let Some(rest) = normalized.strip_prefix("---\n") {
        let marker = rest.find("\n---\n").ok_or_else(|| {
            resource_error("prompt template", path, "unterminated YAML frontmatter")
        })?;
        let metadata = serde_yaml::from_str::<PromptMetadata>(&rest[..marker])
            .map_err(|error| resource_error("prompt template", path, &error.to_string()))?;
        (metadata, &rest[marker + 5..])
    } else {
        (PromptMetadata::default(), normalized.as_str())
    };
    let name = path
        .file_stem()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or_default();
    validate_resource_name("prompt template", path, name)?;
    if body.trim().is_empty() {
        return Err(resource_error(
            "prompt template",
            path,
            "content must not be blank",
        ));
    }
    let description = metadata.description.unwrap_or_else(|| {
        let first = body
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("");
        truncate_chars(first, 60)
    });
    if description.len() > MAX_SKILL_DESCRIPTION_BYTES {
        return Err(resource_error(
            "prompt template",
            path,
            "description is too large",
        ));
    }
    if metadata
        .argument_hint
        .as_ref()
        .is_some_and(|hint| hint.len() > 1_024)
    {
        return Err(resource_error(
            "prompt template",
            path,
            "argument hint is too large",
        ));
    }
    Ok(PromptTemplate {
        name: name.into(),
        description,
        argument_hint: metadata.argument_hint,
        content: body.to_owned(),
        path: path.to_owned(),
    })
}

fn parse_theme(path: &Path) -> Result<CustomTheme, ResourceError> {
    let definition: Value = serde_json::from_str(&std::fs::read_to_string(path)?)
        .map_err(|error| resource_error("theme", path, &error.to_string()))?;
    let name = definition
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    validate_resource_name("theme", path, name)?;
    let colors = definition
        .get("colors")
        .and_then(Value::as_object)
        .filter(|colors| !colors.is_empty())
        .ok_or_else(|| resource_error("theme", path, "colors must be a non-empty object"))?;
    if colors.values().any(|color| {
        !(color.is_string()
            || color
                .as_u64()
                .is_some_and(|palette_index| palette_index <= 255))
    }) {
        return Err(resource_error(
            "theme",
            path,
            "color values must be strings or palette indexes from 0 to 255",
        ));
    }
    Ok(CustomTheme {
        name: name.into(),
        definition,
        path: path.to_owned(),
    })
}

fn validate_resource_name(
    kind: &'static str,
    path: &Path,
    name: &str,
) -> Result<(), ResourceError> {
    if name.is_empty()
        || name.len() > MAX_SKILL_NAME_BYTES
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(resource_error(
            kind,
            path,
            "name must be 1-64 ASCII letters, digits, hyphens, or underscores",
        ));
    }
    Ok(())
}

fn truncate_chars(value: &str, limit: usize) -> String {
    let mut chars = value.chars();
    let result = chars.by_ref().take(limit).collect::<String>();
    if chars.next().is_some() {
        format!("{result}...")
    } else {
        result
    }
}

fn substitute_template_arguments(content: &str, arguments: &[String]) -> String {
    let mut result = String::with_capacity(content.len());
    let all = arguments.join(" ");
    let mut offset = 0;
    while offset < content.len() {
        let remaining = &content[offset..];
        if remaining.starts_with("$ARGUMENTS") {
            result.push_str(&all);
            offset += "$ARGUMENTS".len();
            continue;
        }
        if remaining.starts_with("$@") {
            result.push_str(&all);
            offset += 2;
            continue;
        }
        if let Some(slice) = remaining.strip_prefix("${@:")
            && let Some(end) = slice.find('}')
        {
            let parts = slice[..end].split(':').collect::<Vec<_>>();
            if let Ok(start) = parts[0].parse::<usize>() {
                let start = start.max(1).saturating_sub(1).min(arguments.len());
                let finish = parts
                    .get(1)
                    .and_then(|length| length.parse::<usize>().ok())
                    .map_or(arguments.len(), |length| {
                        start.saturating_add(length).min(arguments.len())
                    });
                result.push_str(&arguments[start..finish].join(" "));
                offset += "${@:".len() + end + 1;
                continue;
            }
        }
        if let Some(digits) = remaining.strip_prefix('$') {
            let count = digits.bytes().take_while(u8::is_ascii_digit).count();
            if count > 0 {
                let index = digits[..count].parse::<usize>().unwrap_or(0);
                result.push_str(
                    index
                        .checked_sub(1)
                        .and_then(|index| arguments.get(index))
                        .map_or("", String::as_str),
                );
                offset += 1 + count;
                continue;
            }
        }
        let character = remaining
            .chars()
            .next()
            .expect("offset remains within a valid string");
        result.push(character);
        offset += character.len_utf8();
    }
    result
}

fn resource_error(kind: &'static str, path: &Path, message: &str) -> ResourceError {
    ResourceError::Resource {
        kind,
        path: path.to_owned(),
        message: message.into(),
    }
}

/// Loads archived legacy skills as bounded runtime skills. Workspace-native
/// skills can be appended afterwards and therefore win on duplicate names.
///
/// Legacy Mimir accepted frontmatter without a description, so a stable
/// non-secret description is derived from the validated skill name when absent.
///
/// # Errors
///
/// Returns an error for symlinks, paths escaping state, malformed frontmatter,
/// duplicate/oversized catalogs, or invalid skill metadata.
pub fn load_migrated_skills(state_root: &std::path::Path) -> Result<Vec<Skill>, ResourceError> {
    let state_root = state_root.canonicalize()?;
    let root = state_root.join("migration/compatibility/v1/resources/skills");
    if !root.exists() {
        return Ok(Vec::new());
    }
    let root = root.canonicalize()?;
    if !root.starts_with(&state_root) {
        return Err(ResourceError::Path(
            "migrated skills escaped the state root".into(),
        ));
    }
    let mut paths = WalkDir::new(&root)
        .follow_links(false)
        .into_iter()
        .map(|entry| entry.map_err(|error| ResourceError::Io(error.into())))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|entry| entry.file_type().is_file() && entry.file_name() == "SKILL.md")
        .map(walkdir::DirEntry::into_path)
        .collect::<Vec<_>>();
    paths.sort();
    if paths.len() > MAX_MIGRATED_SKILLS {
        return Err(ResourceError::Path(format!(
            "migrated skill catalog exceeds {MAX_MIGRATED_SKILLS} entries"
        )));
    }
    let mut skills = BTreeMap::new();
    for path in paths {
        let (canonical, allowed_root) = checked_skill_source(&path, &root)?;
        let skill = discover_migrated_skill(&canonical, &allowed_root)?;
        if skills.insert(skill.name.clone(), skill).is_some() {
            return Err(ResourceError::Path(
                "migrated skill catalog contains duplicate names".into(),
            ));
        }
    }
    Ok(skills.into_values().collect())
}

#[derive(Deserialize)]
struct SkillDocumentMetadata {
    name: String,
    #[serde(default)]
    description: Option<String>,
}

fn discover_skill(
    path: &Path,
    allowed_root: &Path,
    policy: SkillSourcePolicy,
) -> Result<Skill, ResourceError> {
    let frontmatter = read_skill_frontmatter(path)?;
    let metadata: SkillDocumentMetadata = serde_yaml::from_str(&frontmatter)
        .map_err(|error| skill_error(path, &error.to_string()))?;
    let expected = path
        .parent()
        .and_then(std::path::Path::file_name)
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or_default();
    if matches!(policy, SkillSourcePolicy::Strict) && metadata.name != expected {
        return Err(skill_error(path, "name must match the skill directory"));
    }
    validate_skill_name(
        path,
        &metadata.name,
        matches!(policy, SkillSourcePolicy::SharedUser),
    )?;
    let description = metadata
        .description
        .ok_or_else(|| skill_error(path, "description is required"))?;
    if description.trim().is_empty() {
        return Err(skill_error(path, "description must not be blank"));
    }
    if description.len() > MAX_SKILL_DESCRIPTION_BYTES {
        return Err(skill_error(
            path,
            &format!("description exceeds {MAX_SKILL_DESCRIPTION_BYTES} bytes"),
        ));
    }
    Ok(Skill::from_file(
        metadata.name,
        description,
        path.to_owned(),
        allowed_root.to_owned(),
    ))
}

fn discover_migrated_skill(path: &Path, allowed_root: &Path) -> Result<Skill, ResourceError> {
    let frontmatter = read_skill_frontmatter(path)?;
    let metadata: SkillDocumentMetadata = serde_yaml::from_str(&frontmatter)
        .map_err(|error| skill_error(path, &error.to_string()))?;
    let expected = path
        .parent()
        .and_then(std::path::Path::file_name)
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or_default();
    if metadata.name != expected {
        return Err(skill_error(path, "name must match the skill directory"));
    }
    validate_skill_name(path, &metadata.name, false)?;
    let description = metadata
        .description
        .unwrap_or_else(|| format!("Migrated legacy skill {}", metadata.name));
    if description.trim().is_empty() || description.len() > MAX_SKILL_DESCRIPTION_BYTES {
        return Err(skill_error(path, "description is invalid"));
    }
    Ok(Skill::from_file(
        metadata.name,
        description,
        path.to_owned(),
        allowed_root.to_owned(),
    ))
}

fn read_skill_frontmatter(path: &Path) -> Result<String, ResourceError> {
    let mut reader = BufReader::new(std::fs::File::open(path)?);
    let mut line = String::new();
    let mut total_bytes = reader.read_line(&mut line)?;
    if line_without_ending(&line) != "---" {
        return Err(skill_error(path, "missing YAML frontmatter"));
    }
    let mut frontmatter = String::new();
    loop {
        line.clear();
        let read = reader.read_line(&mut line)?;
        if read == 0 {
            return Err(skill_error(path, "unterminated YAML frontmatter"));
        }
        total_bytes = total_bytes.saturating_add(read);
        if total_bytes > MAX_SKILL_FRONTMATTER_BYTES {
            return Err(skill_error(
                path,
                &format!("frontmatter exceeds {MAX_SKILL_FRONTMATTER_BYTES} bytes"),
            ));
        }
        let content = line_without_ending(&line);
        if content == "---" {
            return Ok(frontmatter);
        }
        frontmatter.push_str(content);
        frontmatter.push('\n');
    }
}

fn line_without_ending(line: &str) -> &str {
    let without_newline = line.strip_suffix('\n').unwrap_or(line);
    without_newline
        .strip_suffix('\r')
        .unwrap_or(without_newline)
}

fn parse_skill_document<'a>(
    path: &Path,
    content: &'a str,
) -> Result<(SkillDocumentMetadata, &'a str), ResourceError> {
    let rest = content
        .strip_prefix("---\n")
        .ok_or_else(|| skill_error(path, "missing YAML frontmatter"))?;
    let marker = rest
        .find("\n---\n")
        .ok_or_else(|| skill_error(path, "unterminated YAML frontmatter"))?;
    let frontmatter_bytes = "---\n"
        .len()
        .saturating_add(marker)
        .saturating_add("\n---\n".len());
    if frontmatter_bytes > MAX_SKILL_FRONTMATTER_BYTES {
        return Err(skill_error(
            path,
            &format!("frontmatter exceeds {MAX_SKILL_FRONTMATTER_BYTES} bytes"),
        ));
    }
    let metadata = serde_yaml::from_str(&rest[..marker])
        .map_err(|error| skill_error(path, &error.to_string()))?;
    Ok((metadata, &rest[marker + "\n---\n".len()..]))
}

fn validate_skill_name(
    path: &std::path::Path,
    name: &str,
    allow_namespace: bool,
) -> Result<(), ResourceError> {
    if name.is_empty() || name.len() > MAX_SKILL_NAME_BYTES {
        return Err(skill_error(
            path,
            &format!("name must contain 1 to {MAX_SKILL_NAME_BYTES} bytes"),
        ));
    }
    if !allow_namespace && name.contains(':') {
        return Err(skill_error(
            path,
            "name may contain only lowercase ASCII letters, digits, and hyphens",
        ));
    }
    if name.split(':').any(|segment| {
        segment.is_empty()
            || segment.starts_with('-')
            || segment.ends_with('-')
            || segment.contains("--")
    }) {
        return Err(skill_error(
            path,
            "name segments must not be empty, start or end with a hyphen, or contain consecutive hyphens",
        ));
    }
    if !name.bytes().all(|byte| {
        byte.is_ascii_lowercase()
            || byte.is_ascii_digit()
            || byte == b'-'
            || (allow_namespace && byte == b':')
    }) {
        return Err(skill_error(
            path,
            "name may contain only lowercase ASCII letters, digits, hyphens, and namespace separators",
        ));
    }
    Ok(())
}

fn skill_error(path: &std::path::Path, message: &str) -> ResourceError {
    ResourceError::Skill {
        path: path.to_owned(),
        message: message.into(),
    }
}
