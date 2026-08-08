#![allow(
    clippy::missing_errors_doc,
    reason = "embedded extension operations return bounded configuration and protocol errors"
)]

use std::{
    collections::{BTreeMap, BTreeSet},
    io::{Read, Seek, SeekFrom},
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, SyncSender, TrySendError},
    },
    time::{Duration, Instant},
};

use oxc_allocator::Allocator;
use oxc_codegen::Codegen;
use oxc_parser::Parser;
use oxc_semantic::SemanticBuilder;
use oxc_span::SourceType;
use oxc_transformer::{Module as TransformModule, TransformOptions, Transformer};
use rquickjs::{
    CatchResultExt, Context, Ctx, Error as JsError, Function, Module as JsModule, Object, Promise,
    Runtime,
    loader::{ImportAttributes, Loader, Resolver},
};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::sync::oneshot;
use walkdir::WalkDir;

use crate::error::{MimirError, Result};

use super::{ExtensionManifest, HostLimits};

const MAX_MODULES: usize = 128;
const MAX_MODULE_SOURCE_BYTES: usize = 2 * 1024 * 1024;
const MAX_CACHE_ENTRIES: usize = 32;
const MAX_PENDING_REQUESTS: usize = 64;
const ISOLATE_MEMORY_LIMIT: usize = 32 * 1024 * 1024;
const ISOLATE_STACK_LIMIT: usize = 512 * 1024;
const MAX_EXEC_OUTPUT_BYTES: u64 = 256 * 1024;

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct SafeExecOptions {
    timeout: Option<u64>,
    cwd: Option<String>,
    env: BTreeMap<String, Option<String>>,
}

#[derive(Debug, Clone)]
struct ModuleBundle {
    entry_id: String,
    modules: BTreeMap<String, String>,
}

#[derive(Debug)]
struct SafeModuleResolver {
    modules: BTreeSet<String>,
}

impl Resolver for SafeModuleResolver {
    fn resolve<'js>(
        &mut self,
        _ctx: &Ctx<'js>,
        base: &str,
        name: &str,
        _attributes: Option<ImportAttributes<'js>>,
    ) -> rquickjs::Result<String> {
        if is_virtual_module(name) || self.modules.contains(name) {
            return Ok(name.to_owned());
        }
        resolve_module_id(base, name, &self.modules)
            .map_err(|error| JsError::new_resolving_message(base, name, error.to_string()))
    }
}

#[derive(Debug)]
struct SafeModuleLoader {
    modules: BTreeMap<String, String>,
}

impl Loader for SafeModuleLoader {
    fn load<'js>(
        &mut self,
        ctx: &Ctx<'js>,
        name: &str,
        _attributes: Option<ImportAttributes<'js>>,
    ) -> rquickjs::Result<JsModule<'js>> {
        if let Some(source) = virtual_module_source(name) {
            return JsModule::declare(ctx.clone(), name, source);
        }
        let source = self.modules.get(name).ok_or_else(|| {
            JsError::new_loading_message(name, "module is outside the extension bundle")
        })?;
        JsModule::declare(ctx.clone(), name, source.as_bytes())
    }
}

#[derive(Debug)]
struct ActorRequest {
    payload: String,
    cancelled: Arc<AtomicBool>,
    response: oneshot::Sender<Result<Value>>,
}

#[derive(Debug)]
struct CancellationGuard(Arc<AtomicBool>);

impl Drop for CancellationGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

#[derive(Debug)]
pub struct EmbeddedJsExtensionHost {
    extension_name: String,
    sender: SyncSender<ActorRequest>,
    limits: HostLimits,
}

impl EmbeddedJsExtensionHost {
    pub fn new(
        manifest: &ExtensionManifest,
        workspace_root: &Path,
        limits: HostLimits,
    ) -> Result<Self> {
        let module = manifest.entrypoint.embedded_module().ok_or_else(|| {
            MimirError::Configuration(
                "the embedded JavaScript host requires an embedded module entrypoint".into(),
            )
        })?;
        let bundle = cached_bundle(Path::new(module))?;
        let (sender, receiver) = mpsc::sync_channel(MAX_PENDING_REQUESTS);
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let extension_name = manifest.name.clone();
        let workspace_root = std::fs::canonicalize(workspace_root).map_err(|error| {
            MimirError::Configuration(format!("extension workspace is inaccessible: {error}"))
        })?;
        let process_allowed = manifest.capabilities.contains(&super::Capability::Process);
        let actor_name = extension_name.clone();
        std::thread::Builder::new()
            .name(format!("mimir-ext-{actor_name}"))
            .spawn(move || {
                run_actor(
                    &actor_name,
                    &bundle,
                    limits,
                    &workspace_root,
                    process_allowed,
                    receiver,
                    &ready_sender,
                );
            })
            .map_err(|error| {
                MimirError::Protocol(format!(
                    "failed to start embedded extension '{}': {error}",
                    manifest.name
                ))
            })?;
        match ready_receiver.recv_timeout(limits.timeout) {
            Ok(result) => result?,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                return Err(MimirError::Protocol(format!(
                    "embedded extension '{}' timed out while creating its isolate",
                    manifest.name
                )));
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(MimirError::Protocol(format!(
                    "embedded extension '{}' isolate exited during startup",
                    manifest.name
                )));
            }
        }
        Ok(Self {
            extension_name,
            sender,
            limits,
        })
    }

    pub async fn invoke(&self, payload: &Value) -> Result<Value> {
        let encoded = serde_json::to_vec(payload)?;
        if encoded.len() > self.limits.max_request_bytes {
            return Err(MimirError::Protocol(format!(
                "embedded extension request exceeds the configured request limit of {} bytes",
                self.limits.max_request_bytes
            )));
        }
        let payload = String::from_utf8(encoded).map_err(|error| {
            MimirError::Protocol(format!("extension request was not UTF-8: {error}"))
        })?;
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancellation_guard = CancellationGuard(Arc::clone(&cancelled));
        let (response, receiver) = oneshot::channel();
        self.sender
            .try_send(ActorRequest {
                payload,
                cancelled,
                response,
            })
            .map_err(|error| match error {
                TrySendError::Full(_) => MimirError::Protocol(format!(
                    "embedded extension '{}' request queue is full",
                    self.extension_name
                )),
                TrySendError::Disconnected(_) => MimirError::Protocol(format!(
                    "embedded extension '{}' isolate is unavailable",
                    self.extension_name
                )),
            })?;
        let result = match tokio::time::timeout(self.limits.timeout, receiver).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(MimirError::Protocol(format!(
                "embedded extension '{}' isolate exited before responding",
                self.extension_name
            ))),
            Err(_) => Err(MimirError::Protocol(format!(
                "embedded extension '{}' timed out after {} ms",
                self.extension_name,
                self.limits.timeout.as_millis()
            ))),
        };
        drop(cancellation_guard);
        let value = result?;
        let response_bytes = serde_json::to_vec(&value)?;
        if response_bytes.len() > self.limits.max_response_bytes {
            return Err(MimirError::Protocol(format!(
                "embedded extension response exceeded the configured response limit of {} bytes",
                self.limits.max_response_bytes
            )));
        }
        Ok(value)
    }
}

fn cached_bundle(entry: &Path) -> Result<Arc<ModuleBundle>> {
    static CACHE: OnceLock<Mutex<BTreeMap<String, Arc<ModuleBundle>>>> = OnceLock::new();
    let (cache_key, sources, entry_id) = read_module_sources(entry)?;
    let cache = CACHE.get_or_init(|| Mutex::new(BTreeMap::new()));
    {
        let guard = cache
            .lock()
            .map_err(|_| MimirError::Protocol("extension module cache is poisoned".into()))?;
        if let Some(bundle) = guard.get(&cache_key) {
            return Ok(Arc::clone(bundle));
        }
    }
    let modules = transpile_module_graph(&entry_id, sources)?;
    let bundle = Arc::new(ModuleBundle { entry_id, modules });
    let mut guard = cache
        .lock()
        .map_err(|_| MimirError::Protocol("extension module cache is poisoned".into()))?;
    while guard.len() >= MAX_CACHE_ENTRIES {
        let Some(key) = guard.keys().next().cloned() else {
            break;
        };
        guard.remove(&key);
    }
    guard.insert(cache_key, Arc::clone(&bundle));
    Ok(bundle)
}

type ModuleSource = (String, PathBuf, String);

fn read_module_sources(entry: &Path) -> Result<(String, Vec<ModuleSource>, String)> {
    let entry = std::fs::canonicalize(entry).map_err(|error| {
        MimirError::Configuration(format!(
            "embedded extension module '{}' is unavailable: {error}",
            entry.display()
        ))
    })?;
    let metadata = std::fs::symlink_metadata(&entry)?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(MimirError::Configuration(
            "embedded extension entrypoint must be a regular file".into(),
        ));
    }
    let root = entry.parent().ok_or_else(|| {
        MimirError::Configuration("embedded extension entrypoint has no parent".into())
    })?;
    let mut hasher = Sha256::new();
    let mut sources = Vec::new();
    let mut total_bytes = 0usize;
    for item in WalkDir::new(root).follow_links(false).sort_by_file_name() {
        let item = item.map_err(|error| {
            MimirError::Configuration(format!("failed to inspect extension modules: {error}"))
        })?;
        if item.file_type().is_symlink() {
            return Err(MimirError::Configuration(format!(
                "embedded extension modules cannot contain symlinks: {}",
                item.path().display()
            )));
        }
        if !item.file_type().is_file() || !is_supported_module(item.path()) {
            continue;
        }
        if sources.len() >= MAX_MODULES {
            return Err(MimirError::Configuration(format!(
                "embedded extension exceeds the module limit of {MAX_MODULES}"
            )));
        }
        let bytes = std::fs::read(item.path())?;
        total_bytes = total_bytes.checked_add(bytes.len()).ok_or_else(|| {
            MimirError::Configuration("embedded extension source size overflow".into())
        })?;
        if total_bytes > MAX_MODULE_SOURCE_BYTES {
            return Err(MimirError::Configuration(format!(
                "embedded extension source exceeds {MAX_MODULE_SOURCE_BYTES} bytes"
            )));
        }
        let source = String::from_utf8(bytes).map_err(|error| {
            MimirError::Configuration(format!(
                "extension module '{}' is not UTF-8: {error}",
                item.path().display()
            ))
        })?;
        let relative = item
            .path()
            .strip_prefix(root)
            .map_err(|_| MimirError::Configuration("extension module escaped its root".into()))?;
        let id = safe_module_id(relative)?;
        hasher.update(id.as_bytes());
        hasher.update([0]);
        hasher.update(source.as_bytes());
        hasher.update([0]);
        sources.push((id, item.path().to_path_buf(), source));
    }
    let entry_id = safe_module_id(entry.strip_prefix(root).map_err(|_| {
        MimirError::Configuration("extension entrypoint escaped its module root".into())
    })?)?;
    if !sources.iter().any(|(id, _, _)| id == &entry_id) {
        return Err(MimirError::Configuration(
            "embedded extension entrypoint has an unsupported module type".into(),
        ));
    }
    Ok((format!("{:x}", hasher.finalize()), sources, entry_id))
}

fn safe_module_id(path: &Path) -> Result<String> {
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(value) => {
                let value = value.to_str().ok_or_else(|| {
                    MimirError::Configuration(
                        "extension module path must contain valid UTF-8".into(),
                    )
                })?;
                parts.push(value);
            }
            _ => {
                return Err(MimirError::Configuration(
                    "extension module path contains unsafe traversal".into(),
                ));
            }
        }
    }
    Ok(parts.join("/"))
}

fn is_supported_module(path: &Path) -> bool {
    path.extension()
        .and_then(std::ffi::OsStr::to_str)
        .is_some_and(|extension| matches!(extension, "js" | "mjs" | "ts"))
}

fn transpile(path: &Path, source: &str) -> Result<String> {
    let allocator = Allocator::default();
    let source_type = SourceType::from_path(path).map_err(|error| {
        MimirError::Configuration(format!(
            "unsupported extension module '{}': {error}",
            path.display()
        ))
    })?;
    let parsed = Parser::new(&allocator, source, source_type).parse();
    if !parsed.errors.is_empty() {
        return Err(MimirError::Configuration(format!(
            "failed to parse extension module '{}': {}",
            path.display(),
            diagnostics(&parsed.errors)
        )));
    }
    let mut program = parsed.program;
    let semantic = SemanticBuilder::new()
        .with_excess_capacity(2.0)
        .with_enum_eval(true)
        .build(&program);
    if !semantic.errors.is_empty() {
        return Err(MimirError::Configuration(format!(
            "failed to analyze extension module '{}': {}",
            path.display(),
            diagnostics(&semantic.errors)
        )));
    }
    let mut options = TransformOptions::default();
    options.env.module = TransformModule::CommonJS;
    let transformed = Transformer::new(&allocator, path, &options)
        .build_with_scoping(semantic.semantic.into_scoping(), &mut program);
    if !transformed.errors.is_empty() {
        return Err(MimirError::Configuration(format!(
            "failed to transform extension module '{}': {}",
            path.display(),
            diagnostics(&transformed.errors)
        )));
    }
    Ok(Codegen::new().build(&program).code)
}

fn transpile_module_graph(
    entry_id: &str,
    sources: Vec<ModuleSource>,
) -> Result<BTreeMap<String, String>> {
    let source_map = sources
        .into_iter()
        .map(|(id, path, source)| (id, (path, source)))
        .collect::<BTreeMap<_, _>>();
    let import_pattern =
        regex::Regex::new(r#"(?m)(?:import|export)\s+(?:[^;\n]*?\s+from\s+)?["']([^"']+)["']"#)
            .map_err(|error| {
                MimirError::Configuration(format!(
                    "failed to create the extension module resolver: {error}"
                ))
            })?;
    let module_ids = source_map.keys().cloned().collect::<BTreeSet<_>>();
    let mut pending = vec![entry_id.to_owned()];
    let mut modules = BTreeMap::new();
    while let Some(id) = pending.pop() {
        if modules.contains_key(&id) {
            continue;
        }
        let (path, source) = source_map.get(&id).ok_or_else(|| {
            MimirError::Configuration(format!("extension module not found: {id}"))
        })?;
        for capture in import_pattern.captures_iter(source) {
            let request = capture
                .get(1)
                .map(|value| value.as_str())
                .unwrap_or_default();
            if is_virtual_module(request) {
                continue;
            }
            pending.push(resolve_module_id(&id, request, &module_ids)?);
        }
        let compiled = transpile(path, source)?;
        modules.insert(id, compiled);
    }
    Ok(modules)
}

fn resolve_module_id(parent: &str, request: &str, module_ids: &BTreeSet<String>) -> Result<String> {
    if request.starts_with("node:") || !request.starts_with('.') {
        return Err(MimirError::Configuration(format!(
            "unsupported extension module: {request}"
        )));
    }
    let mut parts = parent.split('/').collect::<Vec<_>>();
    parts.pop();
    for part in request.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if parts.pop().is_none() {
                    return Err(MimirError::Configuration(format!(
                        "extension module escaped its root: {request}"
                    )));
                }
            }
            value => parts.push(value),
        }
    }
    let raw = parts.join("/");
    let without_extension = [".js", ".mjs", ".ts"]
        .iter()
        .find_map(|suffix| raw.strip_suffix(suffix))
        .unwrap_or(&raw);
    for candidate in [
        raw.clone(),
        format!("{without_extension}.ts"),
        format!("{without_extension}.js"),
        format!("{without_extension}.mjs"),
        format!("{raw}/index.ts"),
        format!("{raw}/index.js"),
    ] {
        if module_ids.contains(&candidate) {
            return Ok(candidate);
        }
    }
    Err(MimirError::Configuration(format!(
        "extension module not found: {request} from {parent}"
    )))
}

fn is_virtual_module(request: &str) -> bool {
    matches!(
        request,
        "@earendil-works/pi-coding-agent"
            | "@mariozechner/pi-coding-agent"
            | "@earendil-works/pi-ai"
            | "@mariozechner/pi-ai"
            | "@sinclair/typebox"
            | "typebox"
    )
}

fn virtual_module_source(request: &str) -> Option<&'static str> {
    match request {
        "@earendil-works/pi-coding-agent" | "@mariozechner/pi-coding-agent" => {
            Some("export const defineTool = value => value;")
        }
        "@earendil-works/pi-ai" | "@mariozechner/pi-ai" | "@sinclair/typebox" | "typebox" => {
            Some(VIRTUAL_TYPEBOX_MODULE)
        }
        _ => None,
    }
}

fn diagnostics<T: std::fmt::Display>(errors: &[T]) -> String {
    errors
        .iter()
        .take(3)
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

fn run_actor(
    extension_name: &str,
    bundle: &ModuleBundle,
    limits: HostLimits,
    workspace_root: &Path,
    process_allowed: bool,
    receiver: mpsc::Receiver<ActorRequest>,
    ready: &SyncSender<Result<()>>,
) {
    let origin = Instant::now();
    let deadline_millis = Arc::new(AtomicU64::new(0));
    let interrupted = Arc::new(AtomicBool::new(false));
    let current_cancel = Arc::new(Mutex::new(None::<Arc<AtomicBool>>));
    let runtime = match Runtime::new() {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = ready.send(Err(MimirError::Protocol(format!(
                "failed to create embedded extension runtime: {error}"
            ))));
            return;
        }
    };
    runtime.set_loader(
        SafeModuleResolver {
            modules: bundle.modules.keys().cloned().collect(),
        },
        SafeModuleLoader {
            modules: bundle.modules.clone(),
        },
    );
    runtime.set_memory_limit(ISOLATE_MEMORY_LIMIT);
    runtime.set_max_stack_size(ISOLATE_STACK_LIMIT);
    let deadline_for_interrupt = Arc::clone(&deadline_millis);
    let interrupted_for_handler = Arc::clone(&interrupted);
    let cancel_for_handler = Arc::clone(&current_cancel);
    runtime.set_interrupt_handler(Some(Box::new(move || {
        let cancelled = cancel_for_handler
            .lock()
            .ok()
            .and_then(|guard| guard.as_ref().cloned())
            .is_some_and(|token| token.load(Ordering::Acquire));
        let deadline = deadline_for_interrupt.load(Ordering::Acquire);
        let expired = deadline != 0 && elapsed_millis(origin) >= deadline;
        if expired || cancelled {
            interrupted_for_handler.store(true, Ordering::Release);
        }
        expired || cancelled
    })));
    let context = match Context::full(&runtime) {
        Ok(context) => context,
        Err(error) => {
            let _ = ready.send(Err(MimirError::Protocol(format!(
                "failed to create embedded extension context: {error}"
            ))));
            return;
        }
    };
    deadline_millis.store(deadline_after(origin, limits.timeout), Ordering::Release);
    let initialized = initialize_context(
        &context,
        bundle,
        extension_name,
        workspace_root,
        process_allowed,
        limits,
    );
    deadline_millis.store(0, Ordering::Release);
    if let Err(error) = initialized {
        let _ = ready.send(Err(if interrupted.load(Ordering::Acquire) {
            MimirError::Protocol(format!(
                "embedded extension '{extension_name}' timed out while loading modules"
            ))
        } else {
            error
        }));
        return;
    }
    if ready.send(Ok(())).is_err() {
        return;
    }
    for request in receiver {
        interrupted.store(false, Ordering::Release);
        if let Ok(mut active) = current_cancel.lock() {
            *active = Some(Arc::clone(&request.cancelled));
        }
        deadline_millis.store(deadline_after(origin, limits.timeout), Ordering::Release);
        let result = invoke_context(&context, &request.payload).map_err(|error| {
            if interrupted.load(Ordering::Acquire) {
                MimirError::Protocol(format!(
                    "embedded extension '{extension_name}' timed out or was cancelled"
                ))
            } else {
                error
            }
        });
        deadline_millis.store(0, Ordering::Release);
        if let Ok(mut active) = current_cancel.lock() {
            *active = None;
        }
        let _ = request.response.send(result);
    }
}

fn initialize_context(
    context: &Context,
    bundle: &ModuleBundle,
    extension_name: &str,
    workspace_root: &Path,
    process_allowed: bool,
    limits: HostLimits,
) -> Result<()> {
    let extension_name = serde_json::to_string(extension_name)?;
    context.with(|ctx| {
        if process_allowed {
            let workspace_root = workspace_root.to_path_buf();
            let exec = Function::new(
                ctx.clone(),
                move |command: String, args: Vec<String>, options: String| {
                    safe_exec(&workspace_root, &command, &args, &options, limits).map_err(
                        |message| JsError::new_from_js_message("exec", "ExecResult", message),
                    )
                },
            )
            .map_err(|error| js_protocol_error("create safe process bridge", error))?;
            ctx.globals()
                .set("__mimirExec", exec)
                .map_err(|error| js_protocol_error("install safe process bridge", error))?;
        }
        let prelude = format!("globalThis.__mimirExtensionName={extension_name};");
        ctx.eval::<(), _>(prelude)
            .catch(&ctx)
            .map_err(|error| js_protocol_error("install extension identity", error))?;
        ctx.eval::<(), _>(BOOTSTRAP)
            .catch(&ctx)
            .map_err(|error| js_protocol_error("install extension bridge", error))?;
        let namespace = JsModule::import(&ctx, bundle.entry_id.as_bytes())
            .catch(&ctx)
            .map_err(|error| js_protocol_error("import extension module", error))?
            .finish::<Object<'_>>()
            .catch(&ctx)
            .map_err(|error| js_protocol_error("evaluate extension module", error))?;
        let factory = namespace
            .get::<_, rquickjs::Value<'_>>("default")
            .catch(&ctx)
            .map_err(|error| js_protocol_error("read extension default export", error))?;
        ctx.globals()
            .set("__mimirFactory", factory)
            .catch(&ctx)
            .map_err(|error| js_protocol_error("retain extension factory", error))
    })
}

fn invoke_context(context: &Context, payload: &str) -> Result<Value> {
    let encoded_payload = serde_json::to_string(payload)?;
    let expression = format!("globalThis.__mimirAbi({encoded_payload})");
    let response = context.with(|ctx| {
        let promise = ctx
            .eval::<Promise<'_>, _>(expression)
            .catch(&ctx)
            .map_err(|error| js_protocol_error("invoke extension", error))?;
        promise
            .finish::<String>()
            .catch(&ctx)
            .map_err(|error| js_protocol_error("await extension", error))
    })?;
    serde_json::from_str(&response).map_err(|error| {
        MimirError::Protocol(format!(
            "embedded extension returned invalid JSON output: {error}"
        ))
    })
}

fn js_protocol_error(stage: &str, error: impl std::fmt::Display) -> MimirError {
    MimirError::Protocol(format!("failed to {stage}: {error}"))
}

#[allow(
    clippy::too_many_lines,
    reason = "safe extension process execution keeps validation, sandboxed environment assembly, timeout, and bounded capture in one auditable gate"
)]
fn safe_exec(
    workspace_root: &Path,
    command: &str,
    args: &[String],
    options_json: &str,
    limits: HostLimits,
) -> std::result::Result<String, String> {
    const ALLOWED_PROGRAMS: &[&str] = &[
        "cargo", "git", "rg", "find", "ls", "pwd", "wc", "sed", "head", "tail", "printf", "echo",
        "rustc", "rustfmt",
    ];
    if !ALLOWED_PROGRAMS.contains(&command) {
        return Err(format!(
            "process '{command}' is not in the embedded extension allowlist"
        ));
    }
    if args.len() > 128
        || args
            .iter()
            .any(|arg| arg.len() > 16 * 1024 || arg.contains('\0'))
        || args.iter().map(String::len).sum::<usize>() > 64 * 1024
    {
        return Err("process arguments exceed configured bounds".into());
    }
    let options: SafeExecOptions = serde_json::from_str(options_json)
        .map_err(|error| format!("invalid exec options: {error}"))?;
    let cwd = if let Some(requested) = options.cwd.as_deref() {
        let requested = Path::new(requested);
        let candidate = if requested.is_absolute() {
            requested.to_path_buf()
        } else {
            workspace_root.join(requested)
        };
        let canonical = std::fs::canonicalize(&candidate)
            .map_err(|error| format!("exec cwd is inaccessible: {error}"))?;
        if !canonical.starts_with(workspace_root) {
            return Err("exec cwd escapes the extension workspace".into());
        }
        canonical
    } else {
        workspace_root.to_path_buf()
    };
    let host_timeout = u64::try_from(limits.timeout.as_millis()).unwrap_or(u64::MAX);
    let timeout_ms = options
        .timeout
        .unwrap_or(host_timeout)
        .clamp(1, host_timeout);
    if options.env.len() > 64 {
        return Err("exec environment exceeds 64 entries".into());
    }
    let mut stdout =
        tempfile::tempfile().map_err(|error| format!("create stdout capture: {error}"))?;
    let mut stderr =
        tempfile::tempfile().map_err(|error| format!("create stderr capture: {error}"))?;
    let stdout_child = stdout
        .try_clone()
        .map_err(|error| format!("clone stdout capture: {error}"))?;
    let stderr_child = stderr
        .try_clone()
        .map_err(|error| format!("clone stderr capture: {error}"))?;
    let mut process = Command::new(command);
    process
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout_child))
        .stderr(Stdio::from(stderr_child))
        .env_clear();
    if let Some(path) = std::env::var_os("PATH") {
        process.env("PATH", path);
    }
    process.env("TERM", "dumb").env("NO_COLOR", "1");
    for (key, value) in options.env {
        if key.is_empty()
            || key.len() > 128
            || !key
                .bytes()
                .all(|byte| byte == b'_' || byte.is_ascii_alphanumeric())
            || value.as_ref().is_some_and(|value| value.len() > 16 * 1024)
        {
            return Err("exec environment contains an invalid entry".into());
        }
        if let Some(value) = value {
            process.env(key, value);
        }
    }
    let mut child = process
        .spawn()
        .map_err(|error| format!("spawn process: {error}"))?;
    let started = Instant::now();
    let (status, killed) = loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|error| format!("wait for process: {error}"))?
        {
            break (status, false);
        }
        let output_too_large = stdout
            .metadata()
            .map_or(true, |metadata| metadata.len() > MAX_EXEC_OUTPUT_BYTES)
            || stderr
                .metadata()
                .map_or(true, |metadata| metadata.len() > MAX_EXEC_OUTPUT_BYTES);
        let timed_out = started.elapsed() >= Duration::from_millis(timeout_ms);
        if output_too_large || timed_out {
            let _ = child.kill();
            let status = child
                .wait()
                .map_err(|error| format!("reap process: {error}"))?;
            break (status, true);
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let read_capture = |capture: &mut std::fs::File| -> std::result::Result<String, String> {
        capture
            .seek(SeekFrom::Start(0))
            .map_err(|error| format!("seek process output: {error}"))?;
        let mut limited = capture.take(MAX_EXEC_OUTPUT_BYTES + 1);
        let mut bytes = Vec::new();
        limited
            .read_to_end(&mut bytes)
            .map_err(|error| format!("read process output: {error}"))?;
        if bytes.len() > usize::try_from(MAX_EXEC_OUTPUT_BYTES).unwrap_or(usize::MAX) {
            return Err("process output exceeded 256 KiB".into());
        }
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    };
    serde_json::to_string(&serde_json::json!({
        "stdout": read_capture(&mut stdout)?,
        "stderr": read_capture(&mut stderr)?,
        "code": status.code().unwrap_or(-1),
        "killed": killed,
    }))
    .map_err(|error| format!("encode exec result: {error}"))
}

fn elapsed_millis(origin: Instant) -> u64 {
    u64::try_from(origin.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn deadline_after(origin: Instant, timeout: Duration) -> u64 {
    elapsed_millis(origin)
        .saturating_add(u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX))
        .max(1)
}

const VIRTUAL_TYPEBOX_MODULE: &str = r#"
const schema = (type, extra = {}) => ({ type, ...extra });
export const Type = new Proxy(Object.create(null), {
  get(_target, name) {
    const builders = {
      String: options => schema("string", options),
      Number: options => schema("number", options),
      Integer: options => schema("integer", options),
      Boolean: options => schema("boolean", options),
      Null: () => schema("null"),
      Literal: value => ({ const: value }),
      Array: (items, options = {}) => schema("array", { items, ...options }),
      Object: (properties, options = {}) => schema("object", { properties, ...options }),
      Union: variants => ({ anyOf: variants }),
      Optional: value => ({ ...value, optional: true }),
    };
    if (Object.hasOwn(builders, name)) return builders[name];
    throw new Error(`Unsupported TypeBox builder: ${String(name)}`);
  },
});
export const StringEnum = values => schema("string", { enum: [...values] });
export class AssistantMessageEventStream {
  constructor() {
    this.queue = [];
    this.waiting = [];
    this.done = false;
    this.final = new Promise(resolve => { this.resolveFinal = resolve; });
  }
  push(event) {
    if (this.done) return;
    if (event && (event.type === "done" || event.type === "error")) {
      this.done = true;
      this.resolveFinal(event.type === "done" ? event.message : event.error);
    }
    const waiter = this.waiting.shift();
    if (waiter) waiter({ value: event, done: false });
    else this.queue.push(event);
  }
  end(result) {
    this.done = true;
    if (result !== undefined) this.resolveFinal(result);
    for (const waiter of this.waiting.splice(0)) waiter({ value: undefined, done: true });
  }
  result() { return this.final; }
  async *[Symbol.asyncIterator]() {
    while (true) {
      if (this.queue.length) yield this.queue.shift();
      else if (this.done) return;
      else {
        const item = await new Promise(resolve => this.waiting.push(resolve));
        if (item.done) return;
        yield item.value;
      }
    }
  }
}
export const createAssistantMessageEventStream = () => new AssistantMessageEventStream();
export const calculateCost = (_model, usage) => {
  usage.cost ||= { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 };
  return usage.cost;
};
export const streamSimpleAnthropic = () => { throw new Error("Nested built-in provider streams are unavailable in isolated extensions"); };
export const streamSimpleOpenAIResponses = () => { throw new Error("Nested built-in provider streams are unavailable in isolated extensions"); };
"#;

const BOOTSTRAP: &str = r#"
(() => {
  "use strict";
  const extensionName = globalThis.__mimirExtensionName;
  delete globalThis.__mimirExtensionName;
  globalThis.process = undefined;
  globalThis.fetch = undefined;
  globalThis.XMLHttpRequest = undefined;
  globalThis.WebSocket = undefined;

  const registry = {
    tools: new Map(),
    commands: new Map(),
    shortcuts: new Map(),
    flags: new Map(),
    renderers: new Map(),
    providers: new Map(),
    providerConfigs: new Map(),
    bus: new Map(),
    lifecycle: new Map(),
  };
  let initialized = false;
  let capabilities = new Set();
  let invocationHost = Object.freeze({});
  let invocationActions = [];
  let pendingCommand = null;
  let activeSuspend = null;
  let uiSequence = 0;
  const pendingUiResolvers = new Map();
  const supportedEvents = new Set([
    "resources_discover", "session_start", "session_before_switch", "session_before_fork",
    "session_before_compact", "session_compact", "session_shutdown", "session_before_tree",
    "session_tree", "before_provider_request", "after_provider_response", "agent_start",
    "agent_end", "turn_start", "turn_end", "message_start", "message_update", "message_end",
    "tool_execution_start", "tool_execution_update", "tool_execution_end", "context",
    "before_agent_start", "model_select", "thinking_level_select", "tool_call", "tool_result",
    "user_bash", "input", "refine_complete",
  ]);

  function requireCapability(capability) {
    if (!capabilities.has(capability)) {
      throw new Error(`Extension capability '${capability}' was not declared`);
    }
  }
  function validateName(value, kind) {
    if (typeof value !== "string" || !/^[A-Za-z][A-Za-z0-9_.:-]{0,63}$/.test(value)) {
      throw new Error(`Invalid extension ${kind} name`);
    }
  }
  function makeApi() {
    const api = {
      registerTool(tool) {
        requireCapability("tools");
        if (!tool || typeof tool.execute !== "function") throw new Error("registerTool requires an execute function");
        for (const option of [
          "promptSnippet", "promptGuidelines", "renderShell", "replayBuiltInToolName",
          "executionMode",
        ]) {
          if (Object.hasOwn(tool, option)) throw new Error(`Unsupported extension tool option: ${option}`);
        }
        validateName(tool.name, "tool");
        if (registry.tools.has(tool.name)) throw new Error(`Duplicate extension tool: ${tool.name}`);
        registry.tools.set(tool.name, tool);
      },
      registerCommand(name, options) {
        requireCapability("commands");
        validateName(name, "command");
        if (!options || typeof options.handler !== "function") throw new Error("registerCommand requires a handler function");
        if (registry.commands.has(name)) throw new Error(`Duplicate extension command: ${name}`);
        registry.commands.set(name, options);
      },
      registerShortcut(shortcut, options) {
        requireCapability("commands");
        if (typeof shortcut !== "string" || !shortcut || shortcut.length > 64) throw new Error("Invalid extension shortcut");
        if (!options || typeof options.handler !== "function") throw new Error("registerShortcut requires a handler");
        if (registry.shortcuts.has(shortcut)) throw new Error(`Duplicate extension shortcut: ${shortcut}`);
        registry.shortcuts.set(shortcut, options);
      },
      registerFlag(name, options) {
        requireCapability("commands");
        validateName(name, "flag");
        if (!options || !["boolean", "string"].includes(options.type)) throw new Error("registerFlag type must be boolean or string");
        if (options.default !== undefined && typeof options.default !== options.type) throw new Error("registerFlag default type mismatch");
        if (registry.flags.has(name)) throw new Error(`Duplicate extension flag: ${name}`);
        registry.flags.set(name, options);
      },
      getFlag(name) {
        const configured = (invocationHost.flags || []).find(flag => flag.name === name);
        return configured ? configured.value : registry.flags.get(name)?.default;
      },
      registerMessageRenderer(customType, renderer) {
        requireCapability("ui");
        validateName(customType, "renderer");
        if (typeof renderer !== "function") throw new Error("registerMessageRenderer requires a function");
        if (registry.renderers.has(customType)) throw new Error(`Duplicate extension renderer: ${customType}`);
        registry.renderers.set(customType, renderer);
      },
      sendMessage(message, options = {}) {
        if (!message || typeof message.customType !== "string") throw new Error("sendMessage requires customType");
        invocationActions.push({
          type: "send_message", custom_type: message.customType, content: message.content ?? null,
          display: message.display === true, details: message.details ?? null,
          trigger_turn: options.triggerTurn === true, deliver_as: delivery(options.deliverAs),
        });
      },
      sendUserMessage(content, options = {}) {
        invocationActions.push({ type: "send_user_message", content, deliver_as: delivery(options.deliverAs) });
      },
      appendEntry(customType, data = null) {
        invocationActions.push({ type: "append_entry", custom_type: String(customType), data });
      },
      setSessionName(name) { invocationActions.push({ type: "set_session_name", name: String(name) }); },
      getSessionName() { return invocationHost.sessionName; },
      setLabel(entryId, label) {
        invocationActions.push({ type: "set_label", entry_id: String(entryId), label: label == null ? null : String(label) });
      },
      async exec(command, args = [], options = {}) {
        requireCapability("process");
        if (typeof globalThis.__mimirExec !== "function") throw new Error("Safe process bridge is unavailable");
        return JSON.parse(globalThis.__mimirExec(String(command), args.map(String), JSON.stringify(options || {})));
      },
      getActiveTools() { return [...(invocationHost.activeTools || [])]; },
      getAllTools() { return JSON.parse(JSON.stringify(invocationHost.allTools || [])); },
      setActiveTools(names) {
        requireCapability("tools");
        if (!Array.isArray(names)) throw new Error("setActiveTools requires an array");
        invocationActions.push({ type: "set_active_tools", names: names.map(String) });
      },
      getCommands() { return JSON.parse(JSON.stringify(invocationHost.commands || [])); },
      async setModel(model) {
        requireCapability("provider");
        const provider = String(model?.provider || invocationHost.provider || "");
        const id = String(model?.id || model?.model || "");
        invocationActions.push({ type: "set_model", provider, model: id });
        return true;
      },
      getThinkingLevel() { return invocationHost.thinkingLevel || "off"; },
      setThinkingLevel(level) {
        requireCapability("provider");
        invocationActions.push({ type: "set_thinking_level", level: String(level) });
      },
      registerProvider(name, config) {
        requireCapability("provider");
        validateName(name, "provider");
        if (!config || typeof config !== "object") throw new Error("registerProvider requires a config object");
        const api = String(config.api || "openai-responses");
        const transport = api === "anthropic-messages" ? "anthropic_messages" : "open_ai_compatible";
        const apiKind = api === "anthropic-messages"
          ? "anthropic_messages"
          : api === "openai-responses" ? "open_ai_responses" : "open_ai_completions";
        const models = Array.isArray(config.models) ? config.models.map(model => String(model.id)) : [];
        if (!models.length) throw new Error("registerProvider requires at least one model");
        if (typeof config.baseUrl !== "string" || !config.baseUrl) throw new Error("registerProvider requires baseUrl");
        if (config.headers) throw new Error("Custom provider headers are unsupported by the isolated runtime");
        if (config.oauth) {
          if (typeof config.oauth.name !== "string" || !config.oauth.name) throw new Error("Custom provider OAuth requires a display name");
          for (const callback of ["login", "refreshToken", "getApiKey"]) {
            if (typeof config.oauth[callback] !== "function") throw new Error(`Custom provider OAuth requires ${callback}()`);
          }
          if (config.oauth.modifyModels !== undefined && typeof config.oauth.modifyModels !== "function") {
            throw new Error("Custom provider OAuth modifyModels must be a function");
          }
        }
        let credentialEnv = null;
        if (config.apiKey !== undefined) {
          if (typeof config.apiKey !== "string" || !/^[A-Z][A-Z0-9_]{0,127}$/.test(config.apiKey)) {
            throw new Error("Custom provider apiKey must name an environment variable; inline secrets are forbidden");
          }
          credentialEnv = config.apiKey;
        }
        if (config.streamSimple !== undefined && typeof config.streamSimple !== "function") {
          throw new Error("Custom provider streamSimple must be a function");
        }
        registry.providerConfigs.set(name, config);
        registry.providers.set(name, {
          name, transport, models, api: apiKind, base_url: config.baseUrl,
          credential_env: credentialEnv, oauth_name: config.oauth ? String(config.oauth.name) : null,
          custom_stream: typeof config.streamSimple === "function",
        });
      },
      unregisterProvider(name) {
        registry.providers.delete(String(name));
        registry.providerConfigs.delete(String(name));
      },
      on(event, handler) {
        requireCapability("lifecycle");
        if (!supportedEvents.has(event)) throw new Error(`Unsupported extension lifecycle event: ${event}`);
        if (typeof handler !== "function") throw new Error("pi.on requires a handler function");
        const handlers = registry.lifecycle.get(event) || [];
        handlers.push(handler);
        registry.lifecycle.set(event, handlers);
      },
    };
    api.events = Object.freeze({
      on(topic, handler) {
        validateName(topic, "event");
        if (typeof handler !== "function") throw new Error("event handler must be a function");
        const handlers = registry.bus.get(topic) || [];
        handlers.push(handler);
        registry.bus.set(topic, handlers);
      },
      off(topic, handler) {
        const handlers = registry.bus.get(topic) || [];
        registry.bus.set(topic, handlers.filter(candidate => candidate !== handler));
      },
      emit(topic, data) {
        validateName(topic, "event");
        invocationActions.push({ type: "publish_event", topic, data: data ?? null });
      },
    });
    return new Proxy(Object.freeze(api), {
      get(target, name) {
        if (Reflect.has(target, name)) return Reflect.get(target, name);
        throw new Error(`Unsupported extension API: ${String(name)}`);
      },
    });
  }

  function delivery(value) {
    return value === "steer" ? "steer" : value === "followUp" ? "follow_up" : "next_turn";
  }

  function uiContext(requests, commandCapable = false) {
    const requireUi = () => requireCapability("ui");
    const ui = {
      notify(message, level = "info") {
        requireUi();
        requests.push({ kind: "notify", level: String(level), message: String(message) });
      },
      setStatus(key, text) {
        requireUi();
        requests.push({ kind: "set_status", key: String(key), text: text == null ? null : String(text) });
      },
      setWorkingMessage(text) { this.setStatus("working_message", text); },
      setWorkingVisible(visible) { this.setStatus("working_visible", String(Boolean(visible))); },
      setWorkingIndicator(text) { this.setStatus("working_indicator", text); },
      setHiddenThinkingLabel(text) { this.setStatus("hidden_thinking_label", text); },
      setTitle(text) { this.setStatus("title", text); },
      setToolsExpanded(expanded) { this.setStatus("tools_expanded", String(Boolean(expanded))); },
      getToolsExpanded() { return false; },
      input(title, placeholder = null) {
        requireUi();
        commandOnly("ui.input");
        return interactiveUi({ kind: "input", prompt: String(title), placeholder: placeholder == null ? null : String(placeholder) });
      },
      confirm(title, message) {
        requireUi();
        commandOnly("ui.confirm");
        return interactiveUi({ kind: "confirm", title: String(title), message: String(message) });
      },
      select(title, options) {
        requireUi();
        commandOnly("ui.select");
        if (!Array.isArray(options) || options.length === 0) throw new Error("ui.select requires options");
        return interactiveUi({ kind: "select", title: String(title), options: options.map(String) });
      },
    };
    const uiProxy = new Proxy(Object.freeze(ui), {
      get(target, name) {
        if (Reflect.has(target, name)) return Reflect.get(target, name);
        throw new Error(`Unsupported extension UI operation: ${String(name)}`);
      },
    });
    const commandOnly = name => {
      if (!commandCapable) throw new Error(`${name} is only available to extension commands and shortcuts`);
    };
    const rejectCallbacks = options => {
      if (options && (options.setup || options.withSession || options.onComplete || options.onError)) {
        throw new Error("Session callback closures cannot cross the isolated host-action boundary");
      }
    };
    const sessionManager = Object.freeze({
      getSessionId() { return String(invocationHost.sessionId || ""); },
      getSessionFile() { return null; },
      getSessionName() { return invocationHost.sessionName == null ? undefined : String(invocationHost.sessionName); },
      getCwd() { return String(invocationHost.cwd || ""); },
      getEntries() { return []; },
      getBranch() { return []; },
      getLeafId() { return null; },
    });
    const context = {
      ui: uiProxy,
      hasUI: capabilities.has("ui"),
      cwd: invocationHost.cwd || "",
      sessionManager,
      model: { provider: invocationHost.provider || "", id: invocationHost.model || "" },
      isIdle: invocationHost.isIdle !== false,
      signal: Object.freeze({ aborted: false }),
      abort() { invocationActions.push({ type: "abort" }); },
      hasPendingMessages() { return invocationHost.hasPendingMessages === true; },
      shutdown() { invocationActions.push({ type: "shutdown" }); },
      getContextUsage() { return { ...(invocationHost.contextUsage || {}) }; },
      getSystemPrompt() { return String(invocationHost.systemPrompt || ""); },
      compact(options = {}) {
        rejectCallbacks(options);
        invocationActions.push({
          type: "session",
          action: { kind: "compact", custom_instructions: options.customInstructions == null ? null : String(options.customInstructions) },
        });
      },
      async waitForIdle() {
        commandOnly("waitForIdle");
        if (invocationHost.isIdle === false) throw new Error("Agent is not idle; deferred host waits are unsupported");
      },
      async newSession(options = {}) {
        commandOnly("newSession");
        rejectCallbacks(options);
        invocationActions.push({
          type: "session",
          action: { kind: "new", parent_session: options.parentSession == null ? null : String(options.parentSession) },
        });
        return { cancelled: false, queued: true };
      },
      async fork(entryId, options = {}) {
        commandOnly("fork");
        rejectCallbacks(options);
        invocationActions.push({
          type: "session",
          action: { kind: "fork", entry_id: String(entryId), position: options.position === "at" ? "at" : "before" },
        });
        return { cancelled: false, queued: true };
      },
      async navigateTree(targetId, options = {}) {
        commandOnly("navigateTree");
        rejectCallbacks(options);
        invocationActions.push({
          type: "session",
          action: {
            kind: "navigate_tree",
            target_id: String(targetId),
            summarize: Boolean(options.summarize),
            custom_instructions: options.customInstructions == null ? null : String(options.customInstructions),
            replace_instructions: Boolean(options.replaceInstructions),
            label: options.label == null ? null : String(options.label),
          },
        });
        return { cancelled: false, queued: true };
      },
      async switchSession(sessionPath, options = {}) {
        commandOnly("switchSession");
        rejectCallbacks(options);
        invocationActions.push({
          type: "session",
          action: { kind: "switch", session_path: String(sessionPath) },
        });
        return { cancelled: false, queued: true };
      },
      async reload() {
        commandOnly("reload");
        invocationActions.push({ type: "session", action: { kind: "reload" } });
      },
    };
    return new Proxy(Object.freeze(context), {
      get(target, name) {
        if (Reflect.has(target, name)) return Reflect.get(target, name);
        throw new Error(`Unsupported extension context API: ${String(name)}`);
      },
    });
  }

  function interactiveUi(request) {
    if (pendingUiResolvers.size >= 1) throw new Error("Only one interactive UI request may be pending per extension");
    if (typeof activeSuspend !== "function") throw new Error("Interactive UI is unavailable outside a resumable command");
    const id = `ui-${++uiSequence}`;
    const correlated = { ...request, id };
    return new Promise(resolve => {
      pendingUiResolvers.set(id, resolve);
      activeSuspend({ done: false, request: correlated });
    });
  }

  async function awaitCommand(command) {
    const suspended = new Promise(resolve => { activeSuspend = resolve; });
    if (!command.promise) command.promise = Promise.resolve(command.start());
    const outcome = await Promise.race([command.promise.then(raw => ({ done: true, raw })), suspended]);
    activeSuspend = null;
    if (!outcome.done) {
      pendingCommand = command;
      return { type: "suspended", request: outcome.request };
    }
    pendingCommand = null;
    return command.build(outcome.raw);
  }

  function registrations() {
    return {
      tools: [...registry.tools.values()].map(tool => ({
        name: tool.name,
        label: String(tool.label || tool.name),
        description: String(tool.description || ""),
        parameters: tool.parameters || { type: "object" },
      })),
      commands: [...registry.commands].map(([name, options]) => ({
        name,
        description: options.description == null ? null : String(options.description),
        supports_argument_completions: typeof options.getArgumentCompletions === "function",
      })),
      shortcuts: [...registry.shortcuts].map(([shortcut, options]) => ({
        shortcut,
        description: options.description == null ? null : String(options.description),
      })),
      flags: [...registry.flags].map(([name, options]) => ({
        name,
        description: options.description == null ? null : String(options.description),
        kind: options.type,
        default: options.default === undefined ? null : options.default,
      })),
      ui_requests: capabilities.has("ui") ? ["notify", "input", "confirm", "select", "set_status"] : [],
      renderers: [...registry.renderers.keys()].map(custom_type => ({ custom_type })),
      providers: [...registry.providers.values()],
      lifecycle_events: [...registry.lifecycle.keys()],
    };
  }

  function textSummary(content) {
    if (!Array.isArray(content)) return "Extension tool completed";
    const text = content.find(item => item && item.type === "text" && typeof item.text === "string");
    return text ? text.text : "Extension tool completed";
  }

  function renderLines(raw) {
    if (Array.isArray(raw)) return raw.map(String);
    return String(raw ?? "").split("\n");
  }

  function normalizeCommandCompletions(raw) {
    if (!Array.isArray(raw)) return { items: [] };
    return {
      items: raw.map(item => {
        if (typeof item === "string") {
          return { value: item, description: null };
        }
        if (item && typeof item === "object") {
          const value = item.value === undefined ? item.label : item.value;
          if (value === undefined) {
            throw new Error("Command completion items must provide a value");
          }
          return {
            value: String(value),
            description: item.description == null ? null : String(item.description),
          };
        }
        throw new Error("Command completion items must be strings or objects");
      }),
    };
  }

  function camelize(value) {
    if (Array.isArray(value)) return value.map(camelize);
    if (!value || typeof value !== "object") return value;
    return Object.fromEntries(Object.entries(value).map(([key, item]) => [
      key.replace(/_([a-z])/g, (_match, letter) => letter.toUpperCase()),
      camelize(item),
    ]));
  }

  function normalizeMessage(message) {
    if (!message || typeof message !== "object") throw new Error("Lifecycle message replacement must be an object");
    const content = Array.isArray(message.content) ? message.content.map(item => {
      if (!item || typeof item !== "object") return item;
      if (item.type === "image") {
        return { ...item, mime_type: item.mime_type === undefined ? item.mimeType : item.mime_type };
      }
      if (item.type === "tool_result") {
        return {
          ...item,
          tool_call_id: item.tool_call_id === undefined ? item.toolCallId : item.tool_call_id,
          tool_name: item.tool_name === undefined ? item.toolName : item.tool_name,
          is_error: item.is_error === undefined ? item.isError : item.is_error,
        };
      }
      return item;
    }) : [];
    const usage = message.usage || {};
    return {
      role: message.role,
      content,
      stop_reason: message.stop_reason === undefined ? message.stopReason : message.stop_reason,
      usage: {
        input_tokens: usage.input_tokens === undefined ? (usage.inputTokens || 0) : usage.input_tokens,
        output_tokens: usage.output_tokens === undefined ? (usage.outputTokens || 0) : usage.output_tokens,
        cached_tokens: usage.cached_tokens === undefined ? (usage.cachedTokens || 0) : usage.cached_tokens,
      },
      timestamp_ms: message.timestamp_ms === undefined ? (message.timestampMs || 0) : message.timestamp_ms,
    };
  }

  function interceptionFor(event, raw) {
    if (!raw || typeof raw !== "object") return { kind: "continue" };
    if (raw.interception && typeof raw.interception === "object") return raw.interception;
    if (raw.block === true || raw.cancel === true) {
      return { kind: "block", reason: raw.reason == null ? null : String(raw.reason) };
    }
    if (event.type === "context" && Array.isArray(raw.messages)) {
      return { kind: "mutate", mutation: { kind: "context", messages: raw.messages.map(normalizeMessage) } };
    }
    if (event.type === "tool_call" && (raw.toolName !== undefined || raw.input !== undefined)) {
      return {
        kind: "mutate",
        mutation: {
          kind: "tool_call",
          tool_call: {
            ...event.toolCall,
            name: raw.toolName === undefined ? event.toolCall.name : String(raw.toolName),
            arguments: raw.input === undefined ? event.toolCall.arguments : raw.input,
          },
        },
      };
    }
    if (event.type === "tool_result" && (raw.content !== undefined || raw.isError !== undefined)) {
      return {
        kind: "mutate",
        mutation: {
          kind: "tool_result",
          tool_result: {
            tool_call_id: event.toolResult.toolCallId,
            tool_name: event.toolResult.toolName,
            content: raw.content === undefined ? event.toolResult.content : String(raw.content),
            is_error: raw.isError === undefined ? event.toolResult.isError : Boolean(raw.isError),
          },
        },
      };
    }
    if (event.type === "message_end" && raw.message !== undefined) {
      return {
        kind: "replace",
        replacement: { kind: "message_end", message: normalizeMessage(raw.message) },
      };
    }
    if (event.type === "input" && raw.message !== undefined) {
      return { kind: "mutate", mutation: { kind: "input", message: normalizeMessage(raw.message) } };
    }
    if (event.type === "user_bash" && (raw.command !== undefined || raw.cwd !== undefined)) {
      return {
        kind: "mutate",
        mutation: {
          kind: "user_bash",
          command: raw.command === undefined ? event.command : String(raw.command),
          cwd: raw.cwd === undefined ? (event.cwd || null) : (raw.cwd == null ? null : String(raw.cwd)),
        },
      };
    }
    return { kind: "continue" };
  }

  function providerConfig(name) {
    const config = registry.providerConfigs.get(name);
    if (!config) throw new Error(`Extension provider is not registered: ${name}`);
    return config;
  }

  function normalizeOAuthCredential(raw) {
    if (!raw || typeof raw !== "object") throw new Error("OAuth callback must return credentials");
    const credential = {
      access: String(raw.access || ""),
      refresh: String(raw.refresh || ""),
      expires: Number(raw.expires),
    };
    if (!credential.access || !Number.isSafeInteger(credential.expires) || credential.expires <= 0) {
      throw new Error("OAuth callback returned invalid credentials");
    }
    return credential;
  }

  function toPiContent(item) {
    if (!item || typeof item !== "object") return item;
    if (item.type === "tool_call") return { type: "toolCall", id: item.id, name: item.name, arguments: item.arguments };
    if (item.type === "tool_result") {
      return {
        type: "toolResult", toolCallId: item.tool_call_id, toolName: item.tool_name,
        content: [{ type: "text", text: String(item.content || "") }], isError: Boolean(item.is_error),
      };
    }
    if (item.type === "thinking") {
      return { type: "thinking", thinking: String(item.text || ""), thinkingSignature: item.signature, redacted: Boolean(item.redacted) };
    }
    if (item.type === "image") return { type: "image", data: item.data, mimeType: item.mimeType || item.mime_type };
    return { ...item };
  }

  function toPiMessage(message) {
    const content = Array.isArray(message.content) ? message.content.map(toPiContent) : [];
    if (message.role === "user") {
      return { role: "user", content, timestamp: Number(message.timestamp_ms || 0) };
    }
    if (message.role === "tool") {
      const result = content.find(item => item && item.type === "toolResult");
      return result ? { role: "toolResult", ...result, timestamp: Number(message.timestamp_ms || 0) } : null;
    }
    if (message.role === "assistant") {
      return {
        role: "assistant", content, usage: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, totalTokens: 0, cost: {} },
        stopReason: message.stop_reason || "stop", timestamp: Number(message.timestamp_ms || 0),
      };
    }
    return null;
  }

  function fromPiAssistantMessage(message) {
    if (!message || message.role !== "assistant" || !Array.isArray(message.content)) {
      throw new Error("Custom provider stream did not return an assistant message");
    }
    const content = message.content.map(item => {
      if (!item || typeof item !== "object") throw new Error("Custom provider returned invalid content");
      if (item.type === "text") return { type: "text", text: String(item.text || "") };
      if (item.type === "thinking") {
        return {
          type: "thinking", text: String(item.thinking || ""), signature: item.thinkingSignature == null ? null : String(item.thinkingSignature),
          redacted: Boolean(item.redacted),
        };
      }
      if (item.type === "toolCall") {
        return { type: "tool_call", id: String(item.id), name: String(item.name), arguments: item.arguments || {} };
      }
      throw new Error(`Custom provider returned unsupported content type: ${String(item.type)}`);
    });
    const usage = message.usage || {};
    const stopReason = { toolUse: "tool_use", stop: "stop", length: "length", error: "error", aborted: "aborted" }[message.stopReason] || "error";
    return {
      message: {
        role: "assistant", content, stop_reason: stopReason,
        usage: {
          input_tokens: Number(usage.input || 0), output_tokens: Number(usage.output || 0),
          cached_tokens: Number(usage.cacheRead || 0) + Number(usage.cacheWrite || 0),
        },
        timestamp_ms: Number(message.timestamp || Date.now()),
      },
      response_id: message.responseId == null ? null : String(message.responseId),
    };
  }

  async function consumeProviderStream(name, modelId, credential, request) {
    const config = providerConfig(name);
    if (typeof config.streamSimple !== "function") throw new Error(`Provider ${name} has no streamSimple callback`);
    const configuredModel = config.models.find(model => String(model.id) === modelId);
    if (!configuredModel) throw new Error(`Provider model is not registered: ${modelId}`);
    const model = {
      ...configuredModel, id: modelId, provider: name,
      api: configuredModel.api || config.api || "openai-completions",
      baseUrl: configuredModel.baseUrl || config.baseUrl,
    };
    const context = {
      systemPrompt: String(request.system_prompt || ""),
      messages: (request.messages || []).map(toPiMessage).filter(Boolean),
      tools: (request.tools || []).map(tool => ({ ...tool })),
    };
    const options = {
      apiKey: credential,
      maxTokens: Number(request.max_output_tokens || configuredModel.maxTokens || 0),
      reasoning: request.thinking_level === "off" ? undefined : request.thinking_level,
      signal: Object.freeze({ aborted: false }),
    };
    const stream = config.streamSimple(model, context, options);
    if (!stream || typeof stream[Symbol.asyncIterator] !== "function") {
      throw new Error("Custom provider streamSimple must return an async iterable");
    }
    const events = [];
    let finalMessage = null;
    for await (const event of stream) {
      if (!event || typeof event !== "object") throw new Error("Custom provider emitted an invalid stream event");
      if (event.type === "text_delta") events.push({ type: "text_delta", text: String(event.delta || "") });
      if (event.type === "thinking_delta") events.push({ type: "thinking_delta", text: String(event.delta || "") });
      if (event.type === "done") finalMessage = event.message;
      if (event.type === "error") {
        const reason = event.error && event.error.errorMessage ? String(event.error.errorMessage) : "custom provider stream failed";
        throw new Error(reason);
      }
      if (events.length > 4096) throw new Error("Custom provider emitted too many stream events");
    }
    if (finalMessage === null && typeof stream.result === "function") finalMessage = await stream.result();
    return { response: fromPiAssistantMessage(finalMessage), events };
  }

  globalThis.__mimirAbi = async envelopeJson => {
    const envelope = JSON.parse(envelopeJson);
    const request = envelope.request;
    let response;
    if (request.type === "initialize") {
      if (initialized) throw new Error("Extension factory has already been initialized");
      if (typeof globalThis.__mimirFactory !== "function") {
        throw new Error("Extension module must default-export a factory function");
      }
      capabilities = new Set(request.capabilities || []);
      await globalThis.__mimirFactory(makeApi());
      initialized = true;
      response = { type: "registration", registrations: registrations() };
    } else {
      if (!initialized) throw new Error("Extension has not been initialized");
      if (request.type === "ui_response" && pendingCommand !== null) {
        const resolve = pendingUiResolvers.get(request.request_id);
        if (typeof resolve !== "function") throw new Error(`Unknown pending UI request: ${request.request_id}`);
        pendingUiResolvers.delete(request.request_id);
        const command = pendingCommand;
        resolve(request.response);
        response = await awaitCommand(command);
      } else {
        if (pendingCommand !== null) throw new Error("Extension has a suspended interactive command");
        invocationHost = Object.freeze(camelize(request.host || {}));
        invocationActions = [];
        if (request.type === "tool") {
        const tool = registry.tools.get(request.name);
        if (!tool) throw new Error(`Extension tool is not registered: ${request.name}`);
        const uiRequests = [];
        const context = uiContext(uiRequests);
        let effectiveInput = request.input;
        if (typeof tool.prepareArguments === "function") {
          const prepared = await tool.prepareArguments(request.input, context);
          if (prepared !== undefined) effectiveInput = prepared;
        }
        const renderCall = typeof tool.renderCall === "function"
          ? { lines: renderLines(await tool.renderCall(effectiveInput, context)) }
          : null;
        const raw = await tool.execute(
          request.tool_call_id,
          effectiveInput,
          Object.freeze({ aborted: false }),
          () => {},
          context,
        );
        const content = raw && raw.content !== undefined ? raw.content : raw;
        const renderResult = typeof tool.renderResult === "function"
          ? { lines: renderLines(await tool.renderResult(raw, effectiveInput, context)) }
          : null;
        response = {
          type: "tool",
          result: {
            status: "ok",
            summary: textSummary(content),
            content: content == null ? null : content,
            next_actions: [],
            ui_requests: uiRequests,
            actions: invocationActions,
            render_call: renderCall,
            render_result: renderResult,
          },
        };
      } else if (request.type === "command") {
        const command = registry.commands.get(request.name);
        if (!command) throw new Error(`Extension command is not registered: ${request.name}`);
        const uiRequests = [];
        response = await awaitCommand({
          start: () => command.handler(request.args, uiContext(uiRequests, true)),
          build: raw => ({
            type: "command",
            result: {
              message: raw && raw.message != null ? String(raw.message) : null,
              output: raw && Object.hasOwn(raw, "output") ? raw.output : (raw == null ? null : raw),
              ui_requests: uiRequests,
              actions: invocationActions,
            },
          }),
        });
      } else if (request.type === "command_argument_completions") {
        const command = registry.commands.get(request.name);
        if (!command) throw new Error(`Extension command is not registered: ${request.name}`);
        if (typeof command.getArgumentCompletions !== "function") {
          throw new Error(`Extension command does not support argument completions: ${request.name}`);
        }
        const uiRequests = [];
        const raw = await command.getArgumentCompletions(request.args, uiContext(uiRequests));
        if (uiRequests.length) {
          throw new Error("Extension command argument completions cannot emit UI requests");
        }
        if (invocationActions.length) {
          throw new Error("Extension command argument completions cannot emit host actions");
        }
        response = {
          type: "command_argument_completions",
          result: normalizeCommandCompletions(raw),
        };
      } else if (request.type === "shortcut") {
        const shortcut = registry.shortcuts.get(request.shortcut);
        if (!shortcut) throw new Error(`Extension shortcut is not registered: ${request.shortcut}`);
        const uiRequests = [];
        response = await awaitCommand({
          start: () => shortcut.handler(uiContext(uiRequests, true)),
          build: raw => ({
            type: "command",
            result: {
              message: raw && raw.message != null ? String(raw.message) : null,
              output: raw && Object.hasOwn(raw, "output") ? raw.output : (raw == null ? null : raw),
              ui_requests: uiRequests,
              actions: invocationActions,
            },
          }),
        });
      } else if (request.type === "provider_oauth_login") {
        const config = providerConfig(request.name);
        if (!config.oauth) throw new Error(`Provider ${request.name} has no OAuth callbacks`);
        const notices = [];
        const callbacks = Object.freeze({
          onAuth(params) {
            const url = String(params && params.url || "");
            if (!url) throw new Error("OAuth onAuth requires a URL");
            notices.push(`Open: ${url}`);
          },
          onDeviceCode(params) {
            const code = String(params && params.userCode || "");
            const url = String(params && params.verificationUri || "");
            if (!code || !url) throw new Error("OAuth onDeviceCode requires a code and verification URL");
            notices.push(`Open: ${url}\nCode: ${code}`);
          },
          onPrompt(params) {
            const message = String(params && params.message || "OAuth input:");
            const prefix = notices.length ? `${notices.join("\n")}\n` : "";
            return interactiveUi({ kind: "input", prompt: `${prefix}${message}`.slice(0, 65536), placeholder: null });
          },
        });
        response = await awaitCommand({
          start: () => config.oauth.login(callbacks),
          build: raw => ({ type: "provider_oauth", credential: normalizeOAuthCredential(raw) }),
        });
      } else if (request.type === "provider_oauth_refresh") {
        const config = providerConfig(request.name);
        if (!config.oauth) throw new Error(`Provider ${request.name} has no OAuth callbacks`);
        const raw = await config.oauth.refreshToken(request.credential);
        response = { type: "provider_oauth", credential: normalizeOAuthCredential(raw) };
      } else if (request.type === "provider_oauth_get_api_key") {
        const config = providerConfig(request.name);
        if (!config.oauth) throw new Error(`Provider ${request.name} has no OAuth callbacks`);
        const apiKey = await config.oauth.getApiKey(request.credential);
        if (typeof apiKey !== "string" || !apiKey) throw new Error("OAuth getApiKey returned an invalid credential");
        response = { type: "provider_api_key", api_key: apiKey };
      } else if (request.type === "provider_stream") {
        response = {
          type: "provider_stream",
          result: await consumeProviderStream(request.name, request.model, request.credential, request.request),
        };
      } else if (request.type === "lifecycle") {
        const event = camelize(request.event);
        const handlers = registry.lifecycle.get(event.type) || [];
        const uiRequests = [];
        let cancel = false;
        let output = null;
        for (const handler of handlers) {
          const raw = await handler(event, uiContext(uiRequests));
          if (raw && raw.cancel === true) cancel = true;
          if (raw && Object.hasOwn(raw, "output")) {
            output = raw.output;
          } else if (raw !== undefined) {
            output = raw;
          }
        }
        const interception = interceptionFor(event, output);
        response = {
          type: "lifecycle",
          outcome: { cancel, output, ui_requests: uiRequests, interception, actions: invocationActions },
        };
      } else if (request.type === "render") {
        const renderer = registry.renderers.get(request.custom_type);
        if (!renderer) throw new Error(`Extension renderer is not registered: ${request.custom_type}`);
        const raw = await renderer(request.message, { expanded: request.expanded }, uiContext([]));
        const lines = Array.isArray(raw) ? raw.map(String) : String(raw ?? "").split("\n");
        response = { type: "render", output: { lines } };
      } else if (request.type === "ui_response") {
        const topic = `ui_response:${request.request_id}`;
        const handlers = registry.bus.get(topic) || [];
        for (const handler of handlers) await handler(request.response);
        response = { type: "ui_response", accepted: handlers.length > 0 };
      } else if (request.type === "bus_event") {
        const handlers = registry.bus.get(request.topic) || [];
        for (const handler of handlers) await handler(request.data);
        if (invocationActions.length) {
          throw new Error("Extension bus handlers cannot emit nested host actions");
        }
        response = { type: "bus_event", delivered: handlers.length > 0 };
        } else {
          throw new Error(`Unsupported embedded extension ABI request: ${request.type}`);
        }
      }
    }
    return JSON.stringify({
      abi_version: envelope.abi_version,
      generation: envelope.generation,
      response,
    });
  };
})();
"#;
