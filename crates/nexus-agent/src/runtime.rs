//! Read-only runtime environment observation.
//!
//! Runtime discovery never installs, downloads, writes configuration, changes
//! PATH, or starts a Harness. It only probes already existing executables so
//! the UI can show an honest preflight result before a later install flow.

use std::{
    env,
    ffi::{OsStr, OsString},
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, OnceLock},
    time::Duration,
};

use nexus_core::{resolve_runtime_command, NexusPaths, RuntimeConfig};
use nexus_protocol::{RuntimeListResponse, RuntimeOwnership, RuntimeToolStatus};
use tokio::{
    io::AsyncReadExt,
    process::{Child, Command},
    sync::Semaphore,
    time::{timeout_at, Instant},
};

const TOOL_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const CHILD_CLEANUP_TIMEOUT: Duration = Duration::from_secs(1);
const TOOL_ROUND_TIMEOUT: Duration = Duration::from_secs(6);
const MAX_PROBE_OUTPUT_BYTES: usize = 16 * 1024;
const MAX_SHIM_BYTES: usize = 16 * 1024;
const MAX_SYSTEM_CANDIDATES: usize = 32;
const MAX_SYSTEM_PATH_ENTRIES: usize = 256;
const MAX_PORTABLE_RUNTIME_ENTRIES: usize = 64;
const MAX_PORTABLE_CANDIDATES: usize = 64;
const MAX_BLOCKING_FS_OPERATIONS: usize = 3;

const REASON_NOT_FOUND: &str = "not_found";
const REASON_PROBE_CWD_UNAVAILABLE: &str = "probe_cwd_unavailable";
const REASON_SHIM_UNVERIFIED: &str = "shim_unverified";
const REASON_COREPACK_SHIM_UNVERIFIED: &str = "corepack_shim_unverified";
const REASON_UNSAFE_CMD_PATH: &str = "unsafe_cmd_path";
const REASON_UNSUPPORTED_SHIM: &str = "unsupported_shim";
const REASON_PROBE_FAILED: &str = "probe_failed";
const REASON_INVALID_VERSION_OUTPUT: &str = "invalid_version_output";
const REASON_PROBE_BUDGET_EXCEEDED: &str = "probe_budget_exceeded";
const REASON_CONFIGURED_PATH_MISSING: &str = "configured_path_missing";
const REASON_CONFIGURED_PATH_UNSAFE: &str = "configured_path_unsafe";
const REASON_CONFIGURED_COMMAND_INVALID: &str = "configured_command_invalid";

const COREPACK_PROBE_ENV: &[(&str, &str)] = &[
    // Do not let a Corepack-backed command reach a registry during discovery.
    ("COREPACK_ENABLE_NETWORK", "0"),
    // Do not wait for a download prompt or accept an implicit latest manager.
    ("COREPACK_ENABLE_DOWNLOAD_PROMPT", "0"),
    ("COREPACK_DEFAULT_TO_LATEST", "0"),
    ("COREPACK_ENABLE_AUTO_PIN", "0"),
    // Ignore package.json packageManager resolution in the probe cwd.
    ("COREPACK_ENABLE_PROJECT_SPEC", "0"),
];

/// Tools required by the git-upstream Harness workflow.
pub const RUNTIME_TOOLS: &[&str] = &["git", "node", "pnpm"];

#[derive(Debug, Clone, Copy)]
struct ProbeBudget {
    round: Duration,
    child: Duration,
    cleanup: Duration,
}

const PRODUCTION_PROBE_BUDGET: ProbeBudget = ProbeBudget {
    round: TOOL_ROUND_TIMEOUT,
    child: TOOL_PROBE_TIMEOUT,
    cleanup: CHILD_CLEANUP_TIMEOUT,
};

#[derive(Clone)]
struct ProbeConfig {
    search_path: Option<OsString>,
    data_root: PathBuf,
    data_root_is_safe: bool,
    portable_root: PathBuf,
    probe_cwd: Option<PathBuf>,
    blocking_fs: BlockingFs,
    blocking_hooks: BlockingHooks,
    probe_lifecycle: ProbeLifecycle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BlockingStage {
    Diagnostics,
    Maintenance,
    ConfigFile,
    ReleaseRoot,
    Requirements,
    Configure,
    SystemDirectory,
    PortableRoot,
    ProbeCandidate,
}

#[derive(Clone, Default)]
struct BlockingHooks {
    #[cfg(test)]
    callback: Option<Arc<dyn Fn(BlockingStage, &Path) + Send + Sync>>,
}

#[derive(Clone)]
struct BlockingFs {
    permits: Arc<Semaphore>,
}

#[derive(Clone)]
pub(crate) struct RuntimeRequestContext {
    deadline: Instant,
    budget: ProbeBudget,
    blocking_fs: BlockingFs,
    blocking_hooks: BlockingHooks,
}

impl RuntimeRequestContext {
    pub(crate) fn production() -> Self {
        Self::with_budget(
            PRODUCTION_PROBE_BUDGET,
            production_blocking_fs(),
            BlockingHooks::default(),
        )
    }

    pub(crate) fn preflight() -> Self {
        let mut request=Self::production(); request.deadline=Instant::now()+Duration::from_secs(40); request
    }

    fn with_budget(
        budget: ProbeBudget,
        blocking_fs: BlockingFs,
        blocking_hooks: BlockingHooks,
    ) -> Self {
        Self {
            deadline: Instant::now() + budget.round,
            budget,
            blocking_fs,
            blocking_hooks,
        }
    }

    pub(crate) async fn run_blocking_io<T, F>(
        &self,
        stage: BlockingStage,
        path: PathBuf,
        operation: F,
    ) -> io::Result<T>
    where
        T: Send + 'static,
        F: FnOnce() -> io::Result<T> + Send + 'static,
    {
        let hooks = self.blocking_hooks.clone();
        self.blocking_fs
            .run(self.deadline, move || {
                hooks.notify(stage, &path);
                operation()
            })
            .await
            .ok_or_else(runtime_request_timeout)?
    }

    #[cfg(test)]
    fn available_blocking_permits(&self) -> usize {
        self.blocking_fs.permits.available_permits()
    }
}

fn runtime_request_timeout() -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        "File operation exceeded its absolute wait deadline; background work may still finish. Refresh its status before retrying",
    )
}

impl BlockingFs {
    #[cfg(test)]
    fn new(limit: usize) -> Self {
        Self {
            permits: Arc::new(Semaphore::new(limit)),
        }
    }

    async fn run<T, F>(&self, deadline: Instant, operation: F) -> Option<T>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        if Instant::now() >= deadline {
            return None;
        }
        let permit = timeout_at(deadline, Arc::clone(&self.permits).acquire_owned())
            .await
            .ok()?
            .ok()?;
        if Instant::now() >= deadline {
            return None;
        }
        let task = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            operation()
        });
        timeout_at(deadline, task).await.ok()?.ok()
    }
}

fn production_blocking_fs() -> BlockingFs {
    static PERMITS: OnceLock<Arc<Semaphore>> = OnceLock::new();
    BlockingFs {
        permits: Arc::clone(
            PERMITS.get_or_init(|| Arc::new(Semaphore::new(MAX_BLOCKING_FS_OPERATIONS))),
        ),
    }
}

#[derive(Clone, Default)]
struct ProbeLifecycle {
    #[cfg(test)]
    sender: Option<tokio::sync::mpsc::UnboundedSender<ProbeLifecycleEvent>>,
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProbeLifecycleEvent {
    Started,
    Finished { reaped: bool },
}

impl ProbeLifecycle {
    fn started(&self) {
        #[cfg(test)]
        if let Some(sender) = &self.sender {
            let _ = sender.send(ProbeLifecycleEvent::Started);
        }
    }

    fn finished(&self, reaped: bool) {
        #[cfg(test)]
        if let Some(sender) = &self.sender {
            let _ = sender.send(ProbeLifecycleEvent::Finished { reaped });
        }
        #[cfg(not(test))]
        let _ = reaped;
    }
}

impl BlockingHooks {
    fn notify(&self, stage: BlockingStage, path: &Path) {
        #[cfg(test)]
        if let Some(callback) = &self.callback {
            callback(stage, path);
        }
        #[cfg(not(test))]
        let _ = (stage, path);
    }
}

struct ProbeFailure {
    source: &'static str,
    path: String,
    reason: &'static str,
}

#[derive(Debug)]
struct ProbeResult {
    output: Option<Vec<u8>>,
    reaped: bool,
}

impl ProbeConfig {
    fn from_paths(
        paths: &NexusPaths,
        blocking_fs: BlockingFs,
        blocking_hooks: BlockingHooks,
    ) -> Self {
        blocking_hooks.notify(BlockingStage::Configure, &paths.root);
        let root_is_remote = is_remote_path(&paths.root);
        let data_root_is_safe = !root_is_remote
            && fs::symlink_metadata(&paths.root)
                .map(|metadata| !is_reparse_point(&metadata))
                .unwrap_or(false);
        let data_root = if root_is_remote {
            paths.root.clone()
        } else {
            fs::canonicalize(&paths.root).unwrap_or_else(|_| paths.root.clone())
        };
        let data_root_is_safe = data_root_is_safe && !is_remote_path(&data_root);
        Self {
            search_path: env::var_os("PATH"),
            data_root,
            data_root_is_safe,
            portable_root: paths.runtimes_dir.clone(),
            probe_cwd: select_probe_cwd(probe_cwd_candidates(paths)),
            blocking_fs,
            blocking_hooks,
            probe_lifecycle: ProbeLifecycle::default(),
        }
    }
}

fn probe_cwd_candidates(paths: &NexusPaths) -> Vec<PathBuf> {
    let mut candidates = vec![env::temp_dir(), paths.run_dir.clone(), paths.root.clone()];
    if let Ok(current_dir) = env::current_dir() {
        candidates.push(current_dir);
    }
    if let Ok(current_exe) = env::current_exe() {
        if let Some(parent) = current_exe.parent() {
            candidates.push(parent.to_owned());
        }
    }
    candidates
}

/// Observe the exact configured pins first. Missing or unsafe pins remain
/// explicit failures and never fall back silently to a different PATH tool.
pub(crate) async fn observe_runtime_selection_until(
    paths: &NexusPaths,
    runtime: Option<&RuntimeConfig>,
    request: &RuntimeRequestContext,
) -> RuntimeListResponse {
    observe_runtimes_with_selection_budget(
        paths,
        runtime.cloned(),
        request.deadline,
        request.budget,
        request.blocking_fs.clone(),
        request.blocking_hooks.clone(),
    )
    .await
}

#[cfg(test)]
async fn observe_runtimes_with_budget(
    paths: &NexusPaths,
    budget: ProbeBudget,
    blocking_fs: BlockingFs,
    blocking_hooks: BlockingHooks,
) -> RuntimeListResponse {
    let request = RuntimeRequestContext::with_budget(budget, blocking_fs, blocking_hooks);
    observe_runtime_selection_until(paths, None, &request).await
}

async fn observe_runtimes_with_selection_budget(
    paths: &NexusPaths,
    runtime: Option<RuntimeConfig>,
    deadline: Instant,
    budget: ProbeBudget,
    blocking_fs: BlockingFs,
    blocking_hooks: BlockingHooks,
) -> RuntimeListResponse {
    let owned_paths = paths.clone();
    let config_blocking_fs = blocking_fs.clone();
    let config = blocking_fs
        .run(deadline, move || {
            ProbeConfig::from_paths(&owned_paths, config_blocking_fs, blocking_hooks)
        })
        .await;
    let Some(config) = config else {
        return RuntimeListResponse::new(
            RUNTIME_TOOLS
                .iter()
                .map(|name| budget_exceeded_status(name))
                .collect(),
        );
    };
    let selected = config.blocking_fs.run(deadline, move || {
        prefer_bundled_runtime(runtime.unwrap_or_default(), nexus_core::bundled_runtime_dir().as_deref())
    }).await;
    let Some(selected) = selected else {
        return RuntimeListResponse::new(RUNTIME_TOOLS.iter().map(|name| budget_exceeded_status(name)).collect());
    };
    let (git, (node, pnpm)) = tokio::join!(
        observe_selected_tool_until("git", Some(selected.clone()), config.clone(), deadline, budget),
        observe_node_package_managers(selected, config, deadline, budget),
    );
    let tools = vec![git, node, pnpm];
    debug_assert_eq!(tools.len(), RUNTIME_TOOLS.len());
    RuntimeListResponse::new(tools)
}

/// Explicit pins are authoritative. An installed bundle is the default set,
/// including when a damaged bundle must be reported instead of hidden by PATH.
fn prefer_bundled_runtime(mut runtime: RuntimeConfig, root: Option<&Path>) -> RuntimeConfig {
    if let Some(root) = root.filter(|root| root.is_dir()) {
        runtime.node.get_or_insert_with(|| nexus_core::RuntimePin {
            path: root.join("node").join(if cfg!(windows) { "node.exe" } else { "node" }),
            ownership: RuntimeOwnership::Bundled,
        });
        runtime.pnpm.get_or_insert_with(|| nexus_core::RuntimePin {
            path: root.join("pnpm/bin/pnpm.cjs"),
            ownership: RuntimeOwnership::Bundled,
        });
    }
    runtime
}

/// Use the same effective Node and transitive PATH for observation and spawn.
/// Absolute launch programs remain authoritative; only bare Node is resolved.
pub(crate) fn runtime_for_launch(spec: &mut nexus_core::HarnessLaunchSpec, configured: RuntimeConfig, root: Option<&Path>) -> RuntimeConfig {
    if spec.mode != nexus_protocol::HarnessLaunchMode::Node { return configured; }
    let mut runtime = prefer_bundled_runtime(configured, root);
    let bare_node = spec.program.to_str().is_some_and(|program| {
        program == "node" || program == "node.exe"
            || (cfg!(windows) && (program.eq_ignore_ascii_case("node") || program.eq_ignore_ascii_case("node.exe")))
    });
    if bare_node {
        if let Some(node) = &runtime.node { spec.program = node.path.clone(); }
    } else if spec.program.is_absolute() {
        if runtime.node.as_ref().is_none_or(|node| node.path != spec.program) {
            runtime.node = Some(nexus_core::RuntimePin { path: spec.program.clone(), ownership: RuntimeOwnership::System });
        }
    }
    runtime
}

async fn observe_node_package_managers(
    mut runtime: RuntimeConfig,
    config: ProbeConfig,
    deadline: Instant,
    budget: ProbeBudget,
) -> (RuntimeToolStatus, RuntimeToolStatus) {
    let mut node = observe_selected_tool_until("node", Some(runtime.clone()), config.clone(), deadline, budget).await;
    if node.available {
        runtime.node = node.path.as_ref().map(|path| nexus_core::RuntimePin {
            path: PathBuf::from(path),
            ownership: match node.source.as_deref() {
                Some("bundled") => RuntimeOwnership::Bundled,
                Some("nexus") => RuntimeOwnership::Nexus,
                _ => RuntimeOwnership::System,
            },
        });
    }
    let pnpm_was_configured = runtime.pnpm.is_some();
    let mut pnpm = observe_selected_tool_until("pnpm", Some(runtime.clone()), config.clone(), deadline, budget).await;
    if !node.available {
        if pnpm.available {
            pnpm.available = false;
            pnpm.reason = Some("node_unavailable".to_owned());
        }
        return (node, pnpm);
    }
    if pnpm.available {
        runtime.pnpm = pnpm.path.as_ref().map(|path| nexus_core::RuntimePin {
            path: PathBuf::from(path),
            ownership: match pnpm.source.as_deref() {
                Some("bundled") => RuntimeOwnership::Bundled,
                Some("nexus") => RuntimeOwnership::Nexus,
                _ => RuntimeOwnership::System,
            },
        });
        // Automatic discovery may have probed a shim with the ambient Node.
        // Recheck using exactly the selection and PATH that will run Harness.
        if !pnpm_was_configured {
            pnpm = observe_configured_pin_until("pnpm", runtime.clone(), config.clone(), deadline, budget).await;
        }
    }
    let command_runtime = runtime.clone();
    let command_config = config.clone();
    let prepared = config.blocking_fs.run(deadline, move || {
        if Instant::now() >= cleanup_start(deadline, budget.cleanup) {
            return Err(REASON_PROBE_BUDGET_EXCEEDED);
        }
        prepare_npm_probe_command(&command_runtime, &command_config)
    }).await;
    let result = match prepared {
        Some(Ok(command)) => run_prepared_version_probe("npm", command, &config, deadline, budget)
            .await.map_err(|reason| if reason == REASON_PROBE_BUDGET_EXCEEDED
                || Instant::now() >= cleanup_start(deadline, budget.cleanup) {
                REASON_PROBE_BUDGET_EXCEEDED
            } else { "npm_probe_failed" }),
        Some(Err(reason)) => Err(reason),
        None => Err(REASON_PROBE_BUDGET_EXCEEDED),
    };
    if let Err(reason) = result {
        node.available = false;
        node.reason = Some(reason.to_owned());
    }
    (node, pnpm)
}

fn prepare_npm_probe_command(runtime: &RuntimeConfig, config: &ProbeConfig) -> Result<Command, &'static str> {
    let node = runtime.node.as_ref().ok_or("node_unavailable")?;
    let program = canonical_configured_path(&node.path, node.ownership, config)?;
    let parent = program.parent().ok_or("npm_missing")?;
    let npm = parent.join(if cfg!(windows) { "npm.cmd" } else { "npm" });
    if canonical_file(&npm).is_none() {
        return Err("npm_missing");
    }
    let npm_entry = if cfg!(windows) {
        parent.join("node_modules/npm/bin/npm-cli.js")
    } else {
        fs::canonicalize(&npm).map_err(|_| "npm_missing")?
    };
    let npm_entry = canonical_file(&npm_entry).ok_or("npm_missing")?;
    let cwd = config.probe_cwd.as_deref().filter(|path| is_safe_probe_cwd(path))
        .ok_or(REASON_PROBE_CWD_UNAVAILABLE)?;
    let mut command = Command::new(program);
    // Probe one directly owned process; the packaging test separately proves
    // nested npm.cmd execution without developer tools on PATH.
    command.arg(display_path(&npm_entry)).arg("--version")
        .envs(nexus_core::build_runtime_child_env(runtime, config.search_path.as_deref()).map_err(|_| REASON_CONFIGURED_COMMAND_INVALID)?)
        .current_dir(cwd).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).kill_on_drop(true);
    apply_probe_environment(&mut command);
    Ok(command)
}

async fn observe_selected_tool_until(
    name: &'static str,
    runtime: Option<RuntimeConfig>,
    config: ProbeConfig,
    deadline: Instant,
    budget: ProbeBudget,
) -> RuntimeToolStatus {
    if runtime.as_ref().and_then(|runtime| runtime.pin(name)).is_some() {
        observe_configured_pin_until(name, runtime.expect("pin implies config"), config, deadline, budget)
            .await
    } else {
        observe_tool_until(name, config, deadline, budget).await
    }
}

async fn observe_configured_pin_until(
    name: &'static str,
    runtime: RuntimeConfig,
    config: ProbeConfig,
    deadline: Instant,
    budget: ProbeBudget,
) -> RuntimeToolStatus {
    let pin = runtime.pin(name).expect("configured pin exists").clone();
    let reported_path = pin.path.to_string_lossy().into_owned();
    let ownership = pin.ownership;
    let command_runtime = runtime.clone();
    let command_config = config.clone();
    let command = config
        .blocking_fs
        .run(deadline, move || {
            prepare_configured_probe_command(name, &command_runtime, &command_config)
        })
        .await;
    let command = match command {
        None => {
            return RuntimeToolStatus {
                name: name.to_owned(),
                available: false,
                version: None,
                source: Some(runtime_source(ownership).to_owned()),
                path: Some(reported_path),
                reason: Some(REASON_PROBE_BUDGET_EXCEEDED.to_owned()),
            };
        }
        Some(Err(reason)) => {
            return RuntimeToolStatus {
                name: name.to_owned(),
                available: false,
                version: None,
                source: Some(runtime_source(ownership).to_owned()),
                path: Some(reported_path),
                reason: Some(reason.to_owned()),
            };
        }
        Some(Ok(command)) => command,
    };
    match run_prepared_version_probe(name, command, &config, deadline, budget).await {
        Ok(version) => RuntimeToolStatus {
            name: name.to_owned(),
            available: true,
            version: Some(version),
            source: Some(runtime_source(ownership).to_owned()),
            path: Some(reported_path),
            reason: None,
        },
        Err(reason) => RuntimeToolStatus {
            name: name.to_owned(),
            available: false,
            version: None,
            source: Some(runtime_source(ownership).to_owned()),
            path: Some(reported_path),
            reason: Some(reason.to_owned()),
        },
    }
}

fn prepare_configured_probe_command(
    name: &str,
    runtime: &RuntimeConfig,
    config: &ProbeConfig,
) -> Result<Command, &'static str> {
    let spec = resolve_runtime_command(runtime, name)
        .map_err(|_| REASON_CONFIGURED_COMMAND_INVALID)?
        .ok_or(REASON_CONFIGURED_COMMAND_INVALID)?;
    let program_owner = if name == "pnpm" && !spec.prefix_args.is_empty() {
        runtime
            .node
            .as_ref()
            .map(|pin| pin.ownership)
            .ok_or(REASON_CONFIGURED_COMMAND_INVALID)?
    } else {
        runtime
            .pin(name)
            .map(|pin| pin.ownership)
            .ok_or(REASON_CONFIGURED_COMMAND_INVALID)?
    };
    let program = canonical_configured_path(&spec.program, program_owner, config)?;
    if spec.prefix_args.is_empty() {
        let mut command = prepare_probe_command(&program, config.probe_cwd.as_deref())?;
        command.envs(nexus_core::build_runtime_child_env(runtime, config.search_path.as_deref())
            .map_err(|_| REASON_CONFIGURED_COMMAND_INVALID)?);
        return Ok(command);
    }
    if name != "pnpm" || spec.prefix_args.len() != 1 {
        return Err(REASON_CONFIGURED_COMMAND_INVALID);
    }
    let entry = PathBuf::from(&spec.prefix_args[0]);
    let entry_owner = runtime
        .pnpm
        .as_ref()
        .map(|pin| pin.ownership)
        .ok_or(REASON_CONFIGURED_COMMAND_INVALID)?;
    let entry = canonical_configured_path(&entry, entry_owner, config)?;
    let probe_cwd = config
        .probe_cwd
        .as_deref()
        .filter(|path| is_safe_probe_cwd(path))
        .ok_or(REASON_PROBE_CWD_UNAVAILABLE)?;
    if is_corepack_shim(&program) != Some(false) {
        return Err(REASON_CONFIGURED_COMMAND_INVALID);
    }
    #[cfg(windows)]
    if is_cmd_or_bat_path(&program) {
        return Err(REASON_CONFIGURED_COMMAND_INVALID);
    }
    let mut command = Command::new(program);
    command.envs(nexus_core::build_runtime_child_env(runtime, config.search_path.as_deref())
        .map_err(|_| REASON_CONFIGURED_COMMAND_INVALID)?);
    command
        // Node's script loader rejects the Win32 verbatim prefix returned by
        // canonicalize, just as it does during automatic bundled discovery.
        .arg(display_path(&entry))
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .current_dir(probe_cwd)
        .kill_on_drop(true);
    apply_probe_environment(&mut command);
    Ok(command)
}

fn canonical_configured_path(
    path: &Path,
    ownership: RuntimeOwnership,
    config: &ProbeConfig,
) -> Result<PathBuf, &'static str> {
    if !path.is_absolute() || is_remote_path(path) {
        return Err(REASON_CONFIGURED_PATH_UNSAFE);
    }
    if !fs::metadata(path).is_ok_and(|metadata| metadata.is_file()) {
        return Err(REASON_CONFIGURED_PATH_MISSING);
    }
    match ownership {
        RuntimeOwnership::System | RuntimeOwnership::Bundled => {
            let path = fs::canonicalize(path).map_err(|_| REASON_CONFIGURED_PATH_MISSING)?;
            if is_remote_path(&path) {
                Err(REASON_CONFIGURED_PATH_UNSAFE)
            } else {
                Ok(path)
            }
        }
        RuntimeOwnership::Nexus => {
            if !config.data_root_is_safe {
                return Err(REASON_CONFIGURED_PATH_UNSAFE);
            }
            let data_root = canonical_directory(&config.data_root)
                .ok_or(REASON_CONFIGURED_PATH_UNSAFE)?;
            let portable_root = canonical_portable_root(&data_root, &config.portable_root)
                .ok_or(REASON_CONFIGURED_PATH_UNSAFE)?;
            canonical_file_within(&portable_root, path).ok_or(REASON_CONFIGURED_PATH_UNSAFE)
        }
    }
}

fn runtime_source(ownership: RuntimeOwnership) -> &'static str {
    match ownership {
        RuntimeOwnership::System => "system",
        RuntimeOwnership::Nexus => "nexus",
        RuntimeOwnership::Bundled => "bundled",
    }
}

#[cfg(test)]
async fn observe_tool(name: &str, config: ProbeConfig) -> RuntimeToolStatus {
    observe_tool_with_budget(name, config, PRODUCTION_PROBE_BUDGET).await
}

#[cfg(test)]
async fn observe_tool_with_budget(
    name: &str,
    config: ProbeConfig,
    budget: ProbeBudget,
) -> RuntimeToolStatus {
    let deadline = Instant::now() + budget.round;
    observe_tool_until(name, config, deadline, budget).await
}

async fn observe_tool_until(
    name: &str,
    config: ProbeConfig,
    deadline: Instant,
    budget: ProbeBudget,
) -> RuntimeToolStatus {
    let mut first_failure = None;
    let system_context = first_system_candidate_context(name, config.search_path.as_deref());
    let search_path = config.search_path.clone();
    let system_name = name.to_owned();
    let system_hooks = config.blocking_hooks.clone();
    let system = config
        .blocking_fs
        .run(deadline, move || {
            system_candidates(
                &system_name,
                search_path.as_deref(),
                &system_hooks,
                deadline,
            )
        })
        .await;
    let Some(system) = system else {
        record_failure(
            &mut first_failure,
            "system",
            system_context.as_deref().unwrap_or_else(|| Path::new(name)),
            REASON_PROBE_BUDGET_EXCEEDED,
        );
        return unavailable_status(name, first_failure);
    };
    for path in system {
        if Instant::now() >= deadline {
            record_failure(
                &mut first_failure,
                "system",
                &path,
                REASON_PROBE_BUDGET_EXCEEDED,
            );
            break;
        }
        match probe_path(name, &path, &config, deadline, budget).await {
            Ok(version) => return available_status(name, version, "system", &path),
            Err(reason) => record_failure(&mut first_failure, "system", &path, reason),
        }
    }

    // Git is guided toward a user-managed installation. Nexus-owned portable
    // runtimes are intentionally limited to Node and pnpm in this phase.
    if name != "git" {
        // BlockingFs::run returns None once the deadline has passed, so the
        // empty result falls through to the same budget-exceeded failure.
        let portable_config = config.clone();
        let portable_name = name.to_owned();
        let portable = config
            .blocking_fs
            .run(deadline, move || {
                portable_candidates(&portable_name, &portable_config, deadline)
            })
            .await;
        let Some(portable) = portable else {
            record_failure(
                &mut first_failure,
                "nexus",
                &config.portable_root,
                REASON_PROBE_BUDGET_EXCEEDED,
            );
            return unavailable_status(name, first_failure);
        };
        for path in portable {
            if Instant::now() >= deadline {
                record_failure(
                    &mut first_failure,
                    "nexus",
                    &path,
                    REASON_PROBE_BUDGET_EXCEEDED,
                );
                break;
            }
            match probe_path(name, &path, &config, deadline, budget).await {
                Ok(version) => return available_status(name, version, "nexus", &path),
                Err(reason) => record_failure(&mut first_failure, "nexus", &path, reason),
            }
        }
    }

    // Bundled runtimes ship inside the Nexus installation and are the last
    // automatic tier before the plan reports the tool as unresolvable. Git
    // is absent here: its fallback is the embedded libgit2 worker.
    if name != "git" {
        let bundled_name = name.to_owned();
        let bundled_root = nexus_core::bundled_runtime_dir();
        let bundled = config
            .blocking_fs
            .run(deadline, move || {
                bundled_candidates(&bundled_name, bundled_root.as_deref(), deadline)
            })
            .await
            .unwrap_or_default();
        for (path, node_prefix) in bundled {
            if Instant::now() >= deadline {
                record_failure(
                    &mut first_failure,
                    "bundled",
                    &path,
                    REASON_PROBE_BUDGET_EXCEEDED,
                );
                break;
            }
            let probe = match node_prefix.as_deref() {
                Some(node) => {
                    probe_bundled_script(name, &path, node, &config, deadline, budget).await
                }
                None => probe_path(name, &path, &config, deadline, budget).await,
            };
            match probe {
                Ok(version) => return available_status(name, version, "bundled", &path),
                Err(reason) => record_failure(&mut first_failure, "bundled", &path, reason),
            }
        }
    }

    unavailable_status(name, first_failure)
}

/// Bundled layout: `<runtime>/node/node.exe` (official distribution root)
/// and the full pnpm package tree at `<runtime>/pnpm/` whose entry run by
/// the bundled Node is `<runtime>/pnpm/bin/pnpm.cjs` — never via Corepack.
fn bundled_candidates(
    name: &str,
    root: Option<&Path>,
    deadline: Instant,
) -> Vec<(PathBuf, Option<PathBuf>)> {
    // The root comes from bundled_runtime_dir, which guarantees an absolute
    // path; deadline enforcement happens in the BlockingFs::run wrapper.
    let Some(root) = root else {
        return Vec::new();
    };
    if Instant::now() >= deadline {
        return Vec::new();
    }
    let node_root = root.join("node");
    let node_executable = if cfg!(windows) { "node.exe" } else { "node" };
    match name {
        "node" => canonical_file(&node_root.join(node_executable))
            .map(|path| vec![(path, None)])
            .unwrap_or_default(),
        "pnpm" => {
            // The bundled pnpm is a Node script; without the bundled Node it
            // cannot run, so report it missing instead of a confusing
            // direct-execution probe failure.
            let Some(node) = canonical_file(&node_root.join(node_executable)) else {
                return Vec::new();
            };
            let entry = root.join("pnpm").join("bin").join("pnpm.cjs");
            canonical_file(&entry)
                .map(|path| vec![(path, Some(node))])
                .unwrap_or_default()
        }
        _ => Vec::new(),
    }
}

async fn probe_bundled_script(
    name: &str,
    entry: &Path,
    node: &Path,
    config: &ProbeConfig,
    deadline: Instant,
    budget: ProbeBudget,
) -> Result<String, &'static str> {
    let candidate = entry.to_owned();
    let node_path = node.to_owned();
    let probe_cwd = config.probe_cwd.clone();
    let blocking_hooks = config.blocking_hooks.clone();
    let command = config
        .blocking_fs
        .run(deadline, move || {
            if Instant::now() >= cleanup_start(deadline, budget.cleanup) {
                return Err(REASON_PROBE_BUDGET_EXCEEDED);
            }
            blocking_hooks.notify(BlockingStage::ProbeCandidate, &candidate);
            prepare_bundled_script_probe_command(&candidate, &node_path, probe_cwd.as_deref())
        })
        .await
        .ok_or(REASON_PROBE_BUDGET_EXCEEDED)??;
    run_prepared_version_probe(name, command, config, deadline, budget).await
}

fn prepare_bundled_script_probe_command(
    entry: &Path,
    node: &Path,
    probe_cwd: Option<&Path>,
) -> Result<Command, &'static str> {
    let probe_cwd = probe_cwd
        .filter(|path| is_safe_probe_cwd(path))
        .ok_or(REASON_PROBE_CWD_UNAVAILABLE)?;
    // The node path was already canonicalized by bundled_candidates in this
    // probe round; re-validating it here would only duplicate that work.
    let mut command = Command::new(node);
    // Node's script loader cannot handle Win32 verbatim prefixes; pass the
    // ordinary spelling at this process boundary, mirroring the production
    // launcher's node_script_argument behavior.
    command.arg(display_path(entry));
    command.arg("--version");
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .current_dir(probe_cwd)
        .kill_on_drop(true);
    apply_probe_environment(&mut command);
    Ok(command)
}

fn budget_exceeded_status(name: &str) -> RuntimeToolStatus {
    RuntimeToolStatus {
        name: name.to_owned(),
        available: false,
        version: None,
        source: None,
        path: None,
        reason: Some(REASON_PROBE_BUDGET_EXCEEDED.to_owned()),
    }
}

fn record_failure(
    first_failure: &mut Option<ProbeFailure>,
    source: &'static str,
    path: &Path,
    reason: &'static str,
) {
    if first_failure.is_none() {
        *first_failure = Some(ProbeFailure {
            source,
            path: display_path(path),
            reason,
        });
    }
}

fn available_status(name: &str, version: String, source: &str, path: &Path) -> RuntimeToolStatus {
    RuntimeToolStatus {
        name: name.to_owned(),
        available: true,
        version: Some(version),
        source: Some(source.to_owned()),
        path: Some(display_path(path)),
        reason: None,
    }
}

fn unavailable_status(name: &str, failure: Option<ProbeFailure>) -> RuntimeToolStatus {
    let (source, path, reason) = match failure {
        Some(failure) => (
            Some(failure.source.to_owned()),
            Some(failure.path),
            Some(failure.reason.to_owned()),
        ),
        None => (None, None, Some(REASON_NOT_FOUND.to_owned())),
    };
    RuntimeToolStatus {
        name: name.to_owned(),
        available: false,
        version: None,
        source,
        path,
        reason,
    }
}

fn system_candidates(
    name: &str,
    search_path: Option<&OsStr>,
    blocking_hooks: &BlockingHooks,
    deadline: Instant,
) -> Vec<PathBuf> {
    let Some(search_path) = search_path else {
        return Vec::new();
    };

    let mut candidates = Vec::new();
    for directory in env::split_paths(search_path).take(MAX_SYSTEM_PATH_ENTRIES) {
        if Instant::now() >= deadline {
            break;
        }
        if !directory.is_absolute() || is_remote_path(&directory) {
            continue;
        }
        blocking_hooks.notify(BlockingStage::SystemDirectory, &directory);
        for executable_name in executable_names(name) {
            if Instant::now() >= deadline {
                return candidates;
            }
            let Some(path) = canonical_file(&directory.join(executable_name)) else {
                continue;
            };
            if !candidates.iter().any(|existing| existing == &path) {
                candidates.push(path);
                if candidates.len() >= MAX_SYSTEM_CANDIDATES {
                    return candidates;
                }
            }
        }
    }
    candidates
}

fn first_system_candidate_context(name: &str, search_path: Option<&OsStr>) -> Option<PathBuf> {
    let executable = executable_names(name).into_iter().next()?;
    env::split_paths(search_path?).find_map(|directory| {
        (directory.is_absolute() && !is_unc_path(&directory)).then(|| directory.join(executable))
    })
}

fn portable_candidates(name: &str, config: &ProbeConfig, deadline: Instant) -> Vec<PathBuf> {
    if !config.data_root_is_safe {
        return Vec::new();
    }
    if Instant::now() >= deadline
        || is_remote_path(&config.data_root)
        || is_remote_path(&config.portable_root)
    {
        return Vec::new();
    }
    config
        .blocking_hooks
        .notify(BlockingStage::PortableRoot, &config.portable_root);
    let Some(data_root) = canonical_directory(&config.data_root) else {
        return Vec::new();
    };
    let Some(portable_root) = canonical_portable_root(&data_root, &config.portable_root) else {
        return Vec::new();
    };

    let mut runtime_dirs = Vec::new();
    let Ok(entries) = fs::read_dir(&portable_root) else {
        return Vec::new();
    };
    for entry in entries.take(MAX_PORTABLE_RUNTIME_ENTRIES) {
        if Instant::now() >= deadline {
            break;
        }
        let Ok(entry) = entry else {
            continue;
        };
        let path = entry.path();
        if fs::metadata(&path).is_ok_and(|metadata| metadata.is_dir()) {
            runtime_dirs.push(path);
        }
    }
    runtime_dirs.sort_by(|left, right| {
        left.file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_ascii_lowercase()
            .cmp(
                &right
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_ascii_lowercase(),
            )
    });

    let mut candidates = Vec::new();
    let mut roots = Vec::with_capacity(runtime_dirs.len() + 1);
    roots.push(portable_root.clone());
    roots.extend(runtime_dirs);
    for runtime_dir in roots {
        if Instant::now() >= deadline {
            return candidates;
        }
        for subdirectory in ["", "bin", "cmd", "node_modules/.bin"] {
            let directory = if subdirectory.is_empty() {
                runtime_dir.clone()
            } else {
                runtime_dir.join(subdirectory)
            };
            for executable_name in executable_names(name) {
                if Instant::now() >= deadline {
                    return candidates;
                }
                let Some(path) =
                    canonical_file_within(&portable_root, &directory.join(executable_name))
                else {
                    continue;
                };
                if !candidates.iter().any(|existing| existing == &path) {
                    candidates.push(path);
                    if candidates.len() >= MAX_PORTABLE_CANDIDATES {
                        return candidates;
                    }
                }
            }
        }
    }
    candidates
}

fn executable_names(name: &str) -> Vec<&'static str> {
    if cfg!(windows) {
        match name {
            // Keep this list deliberately narrower than the ambient PATHEXT;
            // the order is the supported exe/cmd/bat search order.
            "git" => vec!["git.exe", "git.cmd", "git.bat"],
            "node" => vec!["node.exe", "node.cmd", "node.bat"],
            "pnpm" => vec!["pnpm.exe", "pnpm.cmd", "pnpm.bat"],
            _ => Vec::new(),
        }
    } else {
        match name {
            "git" => vec!["git"],
            "node" => vec!["node", "nodejs"],
            "pnpm" => vec!["pnpm"],
            _ => Vec::new(),
        }
    }
}

async fn probe_path(
    name: &str,
    path: &Path,
    config: &ProbeConfig,
    deadline: Instant,
    budget: ProbeBudget,
) -> Result<String, &'static str> {
    let candidate = path.to_owned();
    let probe_cwd = config.probe_cwd.clone();
    let blocking_hooks = config.blocking_hooks.clone();
    let command = config
        .blocking_fs
        .run(deadline, move || {
            if Instant::now() >= cleanup_start(deadline, budget.cleanup) {
                return Err(REASON_PROBE_BUDGET_EXCEEDED);
            }
            blocking_hooks.notify(BlockingStage::ProbeCandidate, &candidate);
            prepare_probe_command(&candidate, probe_cwd.as_deref())
        })
        .await
        .ok_or(REASON_PROBE_BUDGET_EXCEEDED)??;

    run_prepared_version_probe(name, command, config, deadline, budget).await
}

async fn run_prepared_version_probe(
    name: &str,
    command: Command,
    config: &ProbeConfig,
    deadline: Instant,
    budget: ProbeBudget,
) -> Result<String, &'static str> {
    let lifecycle = config.probe_lifecycle.clone();
    // Keep the child-owning future alive when a caller cancels this probe.
    // The detached task has its own child and cleanup deadlines, so dropping
    // an HTTP request cannot strand an unbounded process wait.
    let result = tokio::spawn(run_version_probe_observed(
        command, deadline, budget, lifecycle,
    ))
    .await
    .unwrap_or(ProbeResult {
        output: None,
        reaped: false,
    });
    if !result.reaped {
        return Err(REASON_PROBE_FAILED);
    }
    let output = result.output.ok_or(REASON_PROBE_FAILED)?;
    parse_version(name, &output).ok_or(REASON_INVALID_VERSION_OUTPUT)
}

fn prepare_probe_command(path: &Path, probe_cwd: Option<&Path>) -> Result<Command, &'static str> {
    // A Corepack-generated .cmd can download a manager and mutate the user's
    // Corepack cache. We cannot prove it is a read-only observation, so skip
    // the shim. A later runtime provisioning phase may resolve it explicitly.
    match is_corepack_shim(path) {
        Some(true) => return Err(REASON_COREPACK_SHIM_UNVERIFIED),
        Some(false) => {}
        None => return Err(REASON_SHIM_UNVERIFIED),
    }

    let probe_cwd = probe_cwd
        .filter(|path| is_safe_probe_cwd(path))
        .ok_or(REASON_PROBE_CWD_UNAVAILABLE)?;

    #[cfg(windows)]
    if is_cmd_or_bat_path(path) && !is_safe_cmd_path(path) {
        return Err(REASON_UNSAFE_CMD_PATH);
    }

    let mut command = version_command(path).ok_or(REASON_UNSUPPORTED_SHIM)?;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .current_dir(probe_cwd)
        .kill_on_drop(true);
    apply_probe_environment(&mut command);
    Ok(command)
}

fn version_command(path: &Path) -> Option<Command> {
    #[cfg(windows)]
    {
        let extension = path
            .extension()
            .and_then(|extension| extension.to_str())
            .map(|extension| extension.to_ascii_lowercase());
        if matches!(extension.as_deref(), Some("cmd" | "bat")) {
            if !is_safe_cmd_path(path) {
                return None;
            }
            let mut command = Command::new(system_command_processor()?);
            // `call` is required for a batch file. The path has already been
            // checked by is_safe_cmd_path: cmd.exe cannot safely carry shell
            // metacharacters through this /c form, so such candidates are
            // reported unavailable instead of being invoked.
            command.args(["/d", "/s", "/c", "call"]);
            // cmd.exe does not understand the Win32 verbatim `\\?\\` prefix
            // returned by canonicalize, while the child executable does.
            command.arg(display_path(path));
            command.arg("--version");
            return Some(command);
        }
        // PowerShell shims need a shell policy decision and are not the
        // common executable form needed by this read-only preflight.
        if extension.as_deref() == Some("ps1") {
            return None;
        }
    }

    let mut command = Command::new(path);
    command.arg("--version");
    Some(command)
}

fn apply_probe_environment(command: &mut Command) {
    for (name, value) in COREPACK_PROBE_ENV {
        command.env(name, value);
    }
    // Keep npm/pnpm from reading or writing the user's userconfig while they
    // answer --version. This points only at the platform null device.
    let null_device = if cfg!(windows) { "NUL" } else { "/dev/null" };
    command
        .env("NPM_CONFIG_USERCONFIG", null_device)
        .env("npm_config_userconfig", null_device)
        .env("npm_config_update_notifier", "false")
        .env("CI", "1");

    #[cfg(windows)]
    {
        // Keep a console-subsystem probe from opening a visible console next
        // to the native launcher.
        command.creation_flags(0x0800_0000);
    }
}

#[cfg(windows)]
fn system_command_processor() -> Option<PathBuf> {
    let system_root = env::var_os("SystemRoot")?;
    canonical_file(&PathBuf::from(system_root).join("System32").join("cmd.exe"))
}

async fn run_version_probe_observed(
    command: Command,
    deadline: Instant,
    budget: ProbeBudget,
    lifecycle: ProbeLifecycle,
) -> ProbeResult {
    let result = run_version_probe_inner(command, deadline, budget, &lifecycle).await;
    lifecycle.finished(result.reaped);
    result
}

async fn run_version_probe_inner(
    mut command: Command,
    deadline: Instant,
    budget: ProbeBudget,
    lifecycle: &ProbeLifecycle,
) -> ProbeResult {
    let child_deadline = std::cmp::min(
        cleanup_start(deadline, budget.cleanup),
        Instant::now() + budget.child,
    );
    if Instant::now() >= child_deadline {
        return ProbeResult {
            output: None,
            reaped: true,
        };
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_) => {
            return ProbeResult {
                output: None,
                reaped: true,
            }
        }
    };
    lifecycle.started();
    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            let reaped = stop_child(&mut child, cleanup_deadline(deadline, budget.cleanup)).await;
            return ProbeResult {
                output: None,
                reaped,
            };
        }
    };

    let result = timeout_at(child_deadline, async {
        let mut bytes = Vec::new();
        let mut limited = stdout.take((MAX_PROBE_OUTPUT_BYTES + 1) as u64);
        let read = limited.read_to_end(&mut bytes).await;
        if read.is_err() || bytes.len() > MAX_PROBE_OUTPUT_BYTES {
            return (None, false);
        }
        match child.wait().await {
            Ok(status) => (status.success().then_some(bytes), true),
            Err(_) => (None, false),
        }
    })
    .await;

    let (output, mut reaped) = result.unwrap_or((None, false));
    if !reaped {
        reaped = stop_child(&mut child, cleanup_deadline(deadline, budget.cleanup)).await;
    }
    ProbeResult { output, reaped }
}

fn cleanup_start(deadline: Instant, cleanup: Duration) -> Instant {
    deadline.checked_sub(cleanup).unwrap_or(deadline)
}

fn cleanup_deadline(deadline: Instant, cleanup: Duration) -> Instant {
    std::cmp::min(deadline, Instant::now() + cleanup)
}

async fn stop_child(child: &mut Child, deadline: Instant) -> bool {
    let _ = child.start_kill();
    matches!(timeout_at(deadline, child.wait()).await, Ok(Ok(_)))
}

fn parse_version(name: &str, output: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(output).ok()?;
    let line = text.lines().map(str::trim).find(|line| !line.is_empty())?;
    match name {
        "git" => {
            let value = line.strip_prefix("git version ")?;
            let value = value.split_whitespace().next()?;
            valid_dot_version(value, true).then(|| line.to_owned())
        }
        "node" => {
            let value = line.strip_prefix('v')?;
            valid_node_version(value).then(|| line.to_owned())
        }
        "pnpm" | "npm" => valid_dot_version(line, false).then(|| line.to_owned()),
        _ => None,
    }
}

fn valid_node_version(value: &str) -> bool {
    let (without_build, build) = value
        .split_once('+')
        .map_or((value, None), |(core, build)| (core, Some(build)));
    let (core, prerelease) = without_build
        .split_once('-')
        .map_or((without_build, None), |(core, prerelease)| {
            (core, Some(prerelease))
        });
    let segments = core.split('.').collect::<Vec<_>>();
    segments.len() == 3
        && segments.iter().all(|segment| {
            !segment.is_empty() && segment.chars().all(|character| character.is_ascii_digit())
        })
        && prerelease.map_or(true, valid_version_suffix)
        && build.map_or(true, valid_version_suffix)
}

fn valid_dot_version(value: &str, allow_labels: bool) -> bool {
    if allow_labels {
        let mut segments = value.split('.');
        let numeric = segments.by_ref().take(3).collect::<Vec<_>>();
        numeric.len() == 3
            && numeric.iter().all(|segment| {
                !segment.is_empty() && segment.chars().all(|character| character.is_ascii_digit())
            })
            && segments.all(|segment| {
                !segment.is_empty()
                    && segment
                        .chars()
                        .all(|character| character.is_ascii_alphanumeric() || character == '-')
            })
    } else {
        let (without_build, build) = value
            .split_once('+')
            .map_or((value, None), |(core, build)| (core, Some(build)));
        let (core, prerelease) = without_build
            .split_once('-')
            .map_or((without_build, None), |(core, prerelease)| {
                (core, Some(prerelease))
            });
        let segments = core.split('.').collect::<Vec<_>>();
        segments.len() == 3
            && segments.iter().all(|segment| {
                !segment.is_empty() && segment.chars().all(|character| character.is_ascii_digit())
            })
            && prerelease.map_or(true, valid_version_suffix)
            && build.map_or(true, valid_version_suffix)
    }
}

fn valid_version_suffix(value: &str) -> bool {
    !value.is_empty()
        && value.split('.').all(|segment| {
            !segment.is_empty()
                && segment
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || character == '-')
        })
}

fn is_corepack_shim(path: &Path) -> Option<bool> {
    let canonical = fs::canonicalize(path).ok()?;
    let prefix = read_prefix(&canonical, 512)?;
    if is_native_binary_prefix(&prefix) {
        return Some(false);
    }

    // Corepack's Unix symlink target is usually a JS file below a component
    // named `corepack`; Windows cmd shims expose the name in their script.
    // Restrict the path check to an exact component so a fixture or user
    // directory merely containing the word does not become unavailable.
    let canonical_has_corepack_component = canonical.components().any(|component| {
        component
            .as_os_str()
            .to_string_lossy()
            .eq_ignore_ascii_case("corepack")
    });

    let text_prefix = std::str::from_utf8(&prefix).ok();
    let looks_like_script = text_prefix.is_some_and(|text| {
        let text = text.trim_start_matches('\u{feff}');
        text.starts_with("#!")
    }) || is_script_extension(&canonical);
    if !looks_like_script {
        // Only a recognized native image may be executed. An unknown file is
        // deliberately fail-closed because it may be a wrapper in a format
        // this discovery code cannot prove to be read-only.
        return canonical_has_corepack_component.then_some(true);
    }

    let contents = read_bounded(&canonical, MAX_SHIM_BYTES)?;
    let contents = std::str::from_utf8(&contents).ok()?;
    if canonical_has_corepack_component || contents.to_ascii_lowercase().contains("corepack") {
        return Some(true);
    }

    let first_line = contents
        .trim_start_matches('\u{feff}')
        .lines()
        .next()
        .map(str::trim)
        .unwrap_or_default();
    if first_line.starts_with("#!") {
        return known_shebang(first_line).then_some(false);
    }

    // Windows batch files are a supported executable form; a readable batch
    // file without a Corepack marker can be probed after cmd-path validation.
    if cfg!(windows) && is_cmd_or_bat_path(canonical.as_path()) {
        return Some(false);
    }
    None
}

fn is_script_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "bat" | "cmd" | "cjs" | "js" | "mjs" | "ps1" | "sh"
            )
        })
}

fn known_shebang(line: &str) -> bool {
    let line = line.trim_start_matches("#!").trim();
    let mut parts = line.split_ascii_whitespace();
    let first = parts.next().unwrap_or_default();
    let interpreter = if first.rsplit('/').next() == Some("env") {
        parts
            .find(|part| !part.starts_with('-'))
            .unwrap_or_default()
    } else {
        first
    };
    matches!(
        interpreter.rsplit('/').next(),
        Some("bash" | "busybox" | "bun" | "dash" | "deno" | "fish" | "ksh" | "node" | "sh" | "zsh")
    )
}

fn is_native_binary_prefix(prefix: &[u8]) -> bool {
    prefix.starts_with(b"MZ")
        || prefix.starts_with(b"\x7fELF")
        || matches!(
            prefix.get(..4),
            Some([0xfe, 0xed, 0xfa, 0xce])
                | Some([0xce, 0xfa, 0xed, 0xfe])
                | Some([0xfe, 0xed, 0xfa, 0xcf])
                | Some([0xcf, 0xfa, 0xed, 0xfe])
                | Some([0xca, 0xfe, 0xba, 0xbe])
                | Some([0xbe, 0xba, 0xfe, 0xca])
        )
}

fn is_cmd_or_bat_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| matches!(extension.to_ascii_lowercase().as_str(), "cmd" | "bat"))
}

fn is_safe_cmd_path(path: &Path) -> bool {
    let text = path.to_string_lossy();
    !text.is_empty()
        && !text.chars().any(|character| {
            matches!(
                character,
                '&' | '|' | '<' | '>' | '^' | '%' | '!' | '(' | ')' | '"' | '\r' | '\n'
            )
        })
}

fn canonical_file(path: &Path) -> Option<PathBuf> {
    if is_remote_path(path) {
        return None;
    }
    let path = fs::canonicalize(path).ok()?;
    if is_remote_path(&path) {
        return None;
    }
    fs::metadata(&path).ok()?.is_file().then_some(path)
}

fn canonical_directory(path: &Path) -> Option<PathBuf> {
    if is_remote_path(path) {
        return None;
    }
    let path = fs::canonicalize(path).ok()?;
    if is_remote_path(&path) {
        return None;
    }
    fs::metadata(&path).ok()?.is_dir().then_some(path)
}

fn is_remote_path(path: &Path) -> bool {
    #[cfg(windows)]
    {
        use std::path::{Component, Prefix};

        match path.components().next() {
            Some(Component::Prefix(prefix)) => match prefix.kind() {
                Prefix::UNC(_, _) | Prefix::VerbatimUNC(_, _) => true,
                Prefix::Disk(drive) | Prefix::VerbatimDisk(drive) => windows_drive_is_remote(drive),
                Prefix::DeviceNS(_) | Prefix::Verbatim(_) => true,
            },
            _ => false,
        }
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        false
    }
}

fn is_unc_path(path: &Path) -> bool {
    #[cfg(windows)]
    {
        use std::path::{Component, Prefix};

        matches!(
            path.components().next(),
            Some(Component::Prefix(prefix))
                if matches!(prefix.kind(), Prefix::UNC(_, _) | Prefix::VerbatimUNC(_, _))
        )
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        false
    }
}

#[cfg(windows)]
fn windows_drive_is_remote(drive: u8) -> bool {
    const DRIVE_REMOTE: u32 = 4;
    #[link(name = "kernel32")]
    extern "system" {
        fn GetDriveTypeW(root_path_name: *const u16) -> u32;
    }

    let root = [
        drive.to_ascii_uppercase() as u16,
        b':' as u16,
        b'\\' as u16,
        0,
    ];
    // SAFETY: root is a local, NUL-terminated `X:\\` UTF-16 buffer that lives
    // for the duration of this read-only Win32 query.
    unsafe { GetDriveTypeW(root.as_ptr()) == DRIVE_REMOTE }
}

fn canonical_portable_root(data_root: &Path, portable_root: &Path) -> Option<PathBuf> {
    let metadata = fs::symlink_metadata(portable_root).ok()?;
    if is_reparse_point(&metadata) {
        return None;
    }
    let portable_root = canonical_directory(portable_root)?;
    if portable_root == data_root || !is_within(data_root, &portable_root) {
        return None;
    }
    Some(portable_root)
}

fn is_reparse_point(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;

        // FILE_ATTRIBUTE_REPARSE_POINT. This also rejects directory junctions
        // whose target happens to remain under the data root.
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

fn canonical_file_within(root: &Path, path: &Path) -> Option<PathBuf> {
    let path = canonical_file(path)?;
    is_within(root, &path).then_some(path)
}

fn is_within(root: &Path, path: &Path) -> bool {
    #[cfg(windows)]
    {
        let root = display_path(root)
            .trim_end_matches(['\\', '/'])
            .to_ascii_lowercase();
        let path = display_path(path).to_ascii_lowercase();
        path == root
            || path
                .strip_prefix(&root)
                .is_some_and(|rest| rest.starts_with(['\\', '/']))
    }
    #[cfg(not(windows))]
    {
        path == root || path.starts_with(root)
    }
}

fn display_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix("\\\\?\\UNC\\") {
        return format!("\\\\{rest}");
    }
    if let Some(rest) = text.strip_prefix("\\\\?\\") {
        return rest.to_owned();
    }
    text.into_owned()
}

fn select_probe_cwd(candidates: impl IntoIterator<Item = PathBuf>) -> Option<PathBuf> {
    candidates.into_iter().find_map(|candidate| {
        if is_remote_path(&candidate) {
            return None;
        }
        let canonical = fs::canonicalize(candidate).ok()?;
        if is_remote_path(&canonical) {
            return None;
        }
        is_safe_probe_cwd(&canonical).then_some(canonical)
    })
}

fn is_safe_probe_cwd(path: &Path) -> bool {
    if is_remote_path(path) {
        return false;
    }
    let Ok(path) = fs::canonicalize(path) else {
        return false;
    };
    if is_remote_path(&path) || !path.is_dir() {
        return false;
    }
    // Corepack walks upward from cwd when looking for packageManager. Check
    // the complete ancestor chain, while touching only three known filenames.
    for ancestor in path.ancestors() {
        for manifest in ["package.json", "pnpm-workspace.yaml", "pnpm-workspace.yml"] {
            if ancestor.join(manifest).is_file() {
                return false;
            }
        }
    }
    true
}

fn read_bounded(path: &Path, limit: usize) -> Option<Vec<u8>> {
    let mut file = fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take((limit + 1) as u64)
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() <= limit).then_some(bytes)
}

fn read_prefix(path: &Path, limit: usize) -> Option<Vec<u8>> {
    let mut file = fs::File::open(path).ok()?;
    let mut bytes = vec![0_u8; limit];
    let length = file.read(&mut bytes).ok()?;
    bytes.truncate(length);
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        sync::{
            atomic::{AtomicU64, AtomicUsize, Ordering},
            Condvar, Mutex,
        },
    };

    static FIXTURE_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn fixture_root(label: &str) -> PathBuf {
        let id = FIXTURE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("nexus-runtime-{label}-{}-{id}", std::process::id()));
        fs::create_dir_all(root.join("data/runtimes")).expect("fixture directories create");
        fs::create_dir_all(root.join("cwd")).expect("fixture cwd creates");
        root
    }

    fn fixture_name(name: &str) -> &'static str {
        if cfg!(windows) {
            match name {
                "pnpm" => "pnpm.cmd",
                "git" => "git.exe",
                _ => "node.exe",
            }
        } else {
            match name {
                "git" => "git",
                "pnpm" => "pnpm",
                _ => "node",
            }
        }
    }

    fn write_version_fixture(directory: &Path, name: &str, output: &str) -> PathBuf {
        fs::create_dir_all(directory).expect("fixture directory creates");
        let path = directory.join(fixture_name(name));
        if cfg!(windows) {
            fs::write(
                &path,
                format!("@echo off\r\necho {output}\r\nexit /b 0\r\n"),
            )
            .expect("windows fixture writes");
        } else {
            fs::write(&path, format!("#!/bin/sh\nprintf '%s\\n' '{output}'\n"))
                .expect("unix fixture writes");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
                    .expect("unix fixture is executable");
            }
        }
        path
    }

    fn write_probe_fixture(directory: &Path, name: &str, body: &str) -> PathBuf {
        fs::create_dir_all(directory).expect("fixture directory creates");
        let path = directory.join(fixture_name(name));
        fs::write(&path, body).expect("probe fixture writes");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
                .expect("probe fixture is executable");
        }
        path
    }

    fn write_empty_fixture(directory: &Path, name: &str) -> PathBuf {
        if cfg!(windows) {
            write_probe_fixture(directory, name, "@echo off\r\nexit /b 0\r\n")
        } else {
            write_probe_fixture(directory, name, "#!/bin/sh\nexit 0\n")
        }
    }

    #[test]
    fn bundled_candidates_follow_the_shipped_layout() {
        let root = fixture_root("bundled-candidates");
        let runtime = root.join("install/runtime");
        let deadline = Instant::now() + Duration::from_secs(5);

        assert!(bundled_candidates("node", Some(&runtime), deadline).is_empty());
        assert!(bundled_candidates("pnpm", Some(&runtime), deadline).is_empty());
        assert!(bundled_candidates("node", None, deadline).is_empty());

        let node = write_version_fixture(&runtime.join("node"), "node", "v24.1.0");
        let node = fs::canonicalize(node).expect("bundled node canonicalizes");
        let pnpm_entry = runtime.join("pnpm").join("bin").join("pnpm.cjs");
        fs::create_dir_all(runtime.join("pnpm").join("bin")).expect("bundled pnpm dir creates");
        fs::write(&pnpm_entry, "// standalone pnpm entry").expect("bundled pnpm entry writes");
        let pnpm_entry = fs::canonicalize(pnpm_entry).expect("bundled pnpm canonicalizes");

        assert_eq!(
            bundled_candidates("node", Some(&runtime), deadline),
            vec![(node.clone(), None)]
        );
        assert_eq!(
            bundled_candidates("pnpm", Some(&runtime), deadline),
            vec![(pnpm_entry, Some(node))]
        );
    }

    #[test]
    fn bundled_selection_is_a_set_and_preserves_explicit_pins() {
        let root = fixture_root("bundled-selection");
        let selected = prefer_bundled_runtime(RuntimeConfig::default(), Some(&root));
        assert_eq!(selected.node.as_ref().unwrap().ownership, RuntimeOwnership::Bundled);
        assert_eq!(selected.pnpm.as_ref().unwrap().ownership, RuntimeOwnership::Bundled);
        let custom = nexus_core::RuntimePin {
            path: root.join("custom/node.exe"),
            ownership: RuntimeOwnership::System,
        };
        let selected = prefer_bundled_runtime(RuntimeConfig {
            node: Some(custom.clone()), ..RuntimeConfig::default()
        }, Some(&root));
        assert_eq!(selected.node, Some(custom));
        assert_eq!(selected.pnpm.unwrap().path, root.join("pnpm/bin/pnpm.cjs"));
        assert_eq!(prefer_bundled_runtime(RuntimeConfig::default(), None), RuntimeConfig::default());
        assert_eq!(prefer_bundled_runtime(RuntimeConfig::default(), Some(&root.join("absent"))), RuntimeConfig::default());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn node_without_paired_npm_is_rejected_before_spawn() {
        let root = fixture_root("missing-npm");
        let node = write_version_fixture(&root.join("custom"), "node", "v24.1.0");
        let runtime = RuntimeConfig {
            node: Some(nexus_core::RuntimePin { path: node, ownership: RuntimeOwnership::System }),
            ..RuntimeConfig::default()
        };
        assert_eq!(prepare_npm_probe_command(&runtime, &config_for(&root, None)).unwrap_err(), "npm_missing");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn launch_defaults_resolve_bare_node_and_align_absolute_node_path() {
        let root = fixture_root("launch-runtime-defaults");
        let mut spec = nexus_core::HarnessLaunchSpec::new("node".into());
        spec.mode = nexus_protocol::HarnessLaunchMode::Node;
        let runtime = runtime_for_launch(&mut spec, RuntimeConfig::default(), Some(&root));
        assert_eq!(spec.program, runtime.node.as_ref().unwrap().path);
        assert_eq!(runtime.node.as_ref().unwrap().ownership, RuntimeOwnership::Bundled);
        #[cfg(windows)]
        {
            let mut upper = spec.clone();
            upper.program = "NODE.EXE".into();
            let selected = runtime_for_launch(&mut upper, RuntimeConfig::default(), Some(&root));
            assert_eq!(upper.program, selected.node.unwrap().path);
        }
        let explicit = root.join("explicit/node.exe");
        spec.program = explicit.clone();
        let runtime = runtime_for_launch(&mut spec, runtime, Some(&root));
        assert_eq!(spec.program, explicit);
        assert_eq!(runtime.node.as_ref().unwrap().path, explicit);
        let environment = nexus_core::build_runtime_child_env(&runtime, None).unwrap();
        assert_eq!(std::env::split_paths(&environment[0].1).next().unwrap(), explicit.parent().unwrap());
        let direct = nexus_core::HarnessLaunchSpec::new("custom-command".into());
        assert_eq!(runtime_for_launch(&mut direct.clone(), RuntimeConfig::default(), Some(&root)), RuntimeConfig::default());
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn shipped_runtime_checks_combination_with_no_developer_path() {
        let bundle = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../apps/nexus-launcher/src-tauri/resources/runtime");
        if !bundle.join("node/node.exe").is_file() {
            // The source-only test suite does not download release resources.
            return;
        }
        let root = fixture_root("complete-bundled-combination");
        let mut config = config_for(&root, None);
        config.search_path = Some(env::join_paths([
            PathBuf::from(env::var_os("SystemRoot").unwrap()).join("System32")
        ]).unwrap());
        let mut launch = nexus_core::HarnessLaunchSpec::new("node".into());
        launch.mode = nexus_protocol::HarnessLaunchMode::Node;
        let runtime = runtime_for_launch(&mut launch, RuntimeConfig::default(), Some(&bundle));
        assert_eq!(launch.program, runtime.node.as_ref().unwrap().path);
        let npm_probe = prepare_npm_probe_command(&runtime, &config).unwrap();
        let npm_args: Vec<_> = npm_probe.as_std().get_args().collect();
        assert_eq!(npm_args.len(), 2);
        assert!(npm_args[0].to_string_lossy().ends_with("npm-cli.js"));
        assert_eq!(npm_args[1], OsStr::new("--version"));
        let (node, pnpm) = observe_node_package_managers(runtime.clone(), config.clone(),
            Instant::now() + PRODUCTION_PROBE_BUDGET.round, PRODUCTION_PROBE_BUDGET).await;
        assert!(node.available, "{node:?}");
        assert!(pnpm.available, "{pnpm:?}");
        let mut broken = runtime;
        let incomplete = root.join("incomplete");
        fs::create_dir_all(&incomplete).unwrap();
        fs::copy(&broken.node.as_ref().unwrap().path, incomplete.join("node.exe")).unwrap();
        broken.node.as_mut().unwrap().path = incomplete.join("node.exe");
        let (node, _) = observe_node_package_managers(broken, config,
            Instant::now() + PRODUCTION_PROBE_BUDGET.round, PRODUCTION_PROBE_BUDGET).await;
        assert!(!node.available);
        assert_eq!(node.reason.as_deref(), Some("npm_missing"));
        let _ = fs::remove_dir_all(root);
    }

    fn write_nonzero_fixture(directory: &Path, name: &str) -> PathBuf {
        if cfg!(windows) {
            write_probe_fixture(directory, name, "@echo off\r\nexit /b 7\r\n")
        } else {
            write_probe_fixture(directory, name, "#!/bin/sh\nexit 7\n")
        }
    }

    fn write_hanging_fixture(directory: &Path, name: &str) -> PathBuf {
        if cfg!(windows) {
            write_probe_fixture(directory, name, "@echo off\r\n:loop\r\ngoto loop\r\n")
        } else {
            write_probe_fixture(directory, name, "#!/bin/sh\nwhile :; do :; done\n")
        }
    }

    fn write_oversized_fixture(directory: &Path, name: &str) -> PathBuf {
        if cfg!(windows) {
            write_probe_fixture(
                directory,
                name,
                "@echo off\r\nfor /l %%A in (1,1,2000) do @echo 1234567890\r\nexit /b 0\r\n",
            )
        } else {
            write_probe_fixture(
                directory,
                name,
                "#!/bin/sh\nprintf '%17000s' ''\nprintf '\\n'\n",
            )
        }
    }

    fn short_probe_budget() -> ProbeBudget {
        ProbeBudget {
            round: Duration::from_millis(250),
            child: Duration::from_millis(50),
            cleanup: Duration::from_millis(25),
        }
    }

    #[tokio::test]
    async fn configured_pin_is_probed_exactly_and_missing_pin_does_not_fallback() {
        let root = fixture_root("configured-pin");
        let system_dir = root.join("system");
        let pnpm = write_version_fixture(&system_dir, "pnpm", "11.7.0");
        let probe_config = config_for(&root, None);
        let runtime = RuntimeConfig {
            pnpm: Some(nexus_core::RuntimePin {
                path: pnpm.clone(),
                ownership: RuntimeOwnership::System,
            }),
            ..RuntimeConfig::default()
        };
        let status = observe_configured_pin_until(
            "pnpm",
            runtime,
            probe_config.clone(),
            Instant::now() + PRODUCTION_PROBE_BUDGET.round,
            PRODUCTION_PROBE_BUDGET,
        )
        .await;
        assert!(status.available);
        assert_eq!(status.version.as_deref(), Some("11.7.0"));
        assert_eq!(status.path.as_deref(), Some(pnpm.to_string_lossy().as_ref()));

        let missing = root.join("missing").join(fixture_name("pnpm"));
        let runtime = RuntimeConfig {
            pnpm: Some(nexus_core::RuntimePin {
                path: missing,
                ownership: RuntimeOwnership::System,
            }),
            ..RuntimeConfig::default()
        };
        let status = observe_configured_pin_until(
            "pnpm",
            runtime,
            probe_config,
            Instant::now() + PRODUCTION_PROBE_BUDGET.round,
            PRODUCTION_PROBE_BUDGET,
        )
        .await;
        assert!(!status.available);
        assert_eq!(status.reason.as_deref(), Some(REASON_CONFIGURED_PATH_MISSING));
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(windows)]
    #[test]
    fn configured_pnpm_script_uses_node_compatible_path() {
        let root = fixture_root("configured-script-path");
        let entry = root.join("runtime with spaces").join("pnpm.cjs");
        fs::create_dir_all(entry.parent().unwrap()).unwrap();
        fs::write(&entry, "console.log('11.7.0')").unwrap();
        let node = env::current_exe().unwrap();
        let runtime = RuntimeConfig {
            node: Some(nexus_core::RuntimePin {
                path: node,
                ownership: RuntimeOwnership::Bundled,
            }),
            pnpm: Some(nexus_core::RuntimePin {
                path: entry.clone(),
                ownership: RuntimeOwnership::Bundled,
            }),
            ..RuntimeConfig::default()
        };
        let command = prepare_configured_probe_command("pnpm", &runtime, &config_for(&root, None))
            .expect("configured script probe prepares");
        let args: Vec<_> = command.as_std().get_args().collect();
        assert_eq!(args, vec![entry.as_os_str(), OsStr::new("--version")]);
        let _ = fs::remove_dir_all(root);
    }

    fn config_for(root: &Path, search_dir: Option<&Path>) -> ProbeConfig {
        let data_root = fs::canonicalize(root.join("data")).expect("data root canonicalizes");
        let probe_cwd = fs::canonicalize(root.join("cwd")).expect("probe cwd canonicalizes");
        let search_path = search_dir.map(|directory| {
            env::join_paths([directory.as_os_str()]).expect("fixture path encodes")
        });
        ProbeConfig {
            search_path,
            data_root,
            data_root_is_safe: true,
            portable_root: root.join("data/runtimes"),
            probe_cwd: Some(probe_cwd),
            blocking_fs: BlockingFs::new(MAX_BLOCKING_FS_OPERATIONS),
            blocking_hooks: BlockingHooks::default(),
            probe_lifecycle: ProbeLifecycle::default(),
        }
    }

    #[derive(Clone)]
    struct BlockingGate(Arc<(Mutex<bool>, Condvar)>);

    impl BlockingGate {
        fn new() -> Self {
            Self(Arc::new((Mutex::new(false), Condvar::new())))
        }

        fn wait(&self) {
            let (lock, ready) = &*self.0;
            let mut released = lock.lock().expect("release lock");
            while !*released {
                released = ready.wait(released).expect("release wait");
            }
        }

        fn release(&self) {
            let (lock, ready) = &*self.0;
            *lock.lock().expect("release lock") = true;
            ready.notify_all();
        }
    }

    fn blocking_stage_hook(
        target: BlockingStage,
        target_index: usize,
    ) -> (
        BlockingHooks,
        tokio::sync::oneshot::Receiver<()>,
        BlockingGate,
    ) {
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let entered_tx = Arc::new(Mutex::new(Some(entered_tx)));
        let index = Arc::new(AtomicUsize::new(0));
        let gate = BlockingGate::new();
        let hooks = BlockingHooks {
            callback: Some(Arc::new({
                let entered_tx = Arc::clone(&entered_tx);
                let index = Arc::clone(&index);
                let gate = gate.clone();
                move |stage, _| {
                    if stage != target || index.fetch_add(1, Ordering::SeqCst) != target_index {
                        return;
                    }
                    if let Some(sender) = entered_tx.lock().expect("sender lock").take() {
                        let _ = sender.send(());
                    }
                    gate.wait();
                }
            })),
        };
        (hooks, entered_rx, gate)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn slow_system_enumeration_cannot_block_the_round_response() {
        let root = fixture_root("slow-enumeration");
        let search_dir = root.join("system");
        fs::create_dir_all(&search_dir).expect("system directory creates");
        let mut config = config_for(&root, Some(&search_dir));
        let (hooks, entered_rx, gate) = blocking_stage_hook(BlockingStage::SystemDirectory, 0);
        config.blocking_hooks = hooks;
        let budget = ProbeBudget {
            round: Duration::from_millis(75),
            child: Duration::from_millis(25),
            cleanup: Duration::from_millis(10),
        };

        let observation = tokio::spawn(observe_tool_with_budget("pnpm", config, budget));
        entered_rx.await.expect("enumeration entered");
        let bounded = tokio::time::timeout(Duration::from_millis(200), observation).await;
        gate.release();
        assert!(bounded.is_ok(), "enumeration exceeded the round budget");
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn slow_probe_configuration_is_inside_the_request_budget() {
        let root = fixture_root("slow-config");
        let paths = NexusPaths::from_root(root.join("data"));
        let (hooks, entered_rx, gate) = blocking_stage_hook(BlockingStage::Configure, 0);
        let budget = ProbeBudget {
            round: Duration::from_millis(75),
            child: Duration::from_millis(25),
            cleanup: Duration::from_millis(10),
        };
        let observation = tokio::spawn(async move {
            observe_runtimes_with_budget(&paths, budget, BlockingFs::new(1), hooks).await
        });
        entered_rx.await.expect("configuration entered");
        let bounded = tokio::time::timeout(Duration::from_millis(200), observation)
            .await
            .expect("configuration obeys request deadline")
            .expect("observation task completes");
        gate.release();
        assert!(bounded
            .tools
            .iter()
            .all(|tool| { tool.reason.as_deref() == Some(REASON_PROBE_BUDGET_EXCEEDED) }));
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn diagnostics_and_preview_budget_keep_health_live_and_hold_worker_permits() {
        for stage in [BlockingStage::Diagnostics, BlockingStage::Maintenance] {
            let state=crate::switch_ownership_tests::switch_test_state(&format!("bounded-{stage:?}"));
            let root=state.paths.root.clone();
            if stage == BlockingStage::Maintenance { fs::write(state.paths.run_dir.join("harness-log-session.json"), b"invalid-budget-fixture").unwrap(); }
            let (hooks,entered_rx,gate)=blocking_stage_hook(stage,0);
            struct ReleaseOnExit(BlockingGate);
            impl Drop for ReleaseOnExit { fn drop(&mut self) { self.0.release(); } }
            let _release_on_exit = ReleaseOnExit(gate.clone());
            let request=RuntimeRequestContext::with_budget(ProbeBudget{round:Duration::from_millis(100),child:Duration::from_millis(25),cleanup:Duration::from_millis(10)},BlockingFs::new(1),hooks);
            let inspector=request.clone();let owned=state.clone();
            let task=tokio::spawn(async move {
                match stage {
                    BlockingStage::Diagnostics=>crate::diagnostics_status_with_budget(owned,request).await,
                    BlockingStage::Maintenance=>crate::maintenance_preview_with_budget(owned,30,request).await,
                    _=>unreachable!(),
                }
            });
            tokio::time::timeout(Duration::from_millis(200),entered_rx).await.unwrap().unwrap();
            let _ = tokio::time::timeout(Duration::from_millis(30),crate::health(axum::extract::State(state.clone()))).await.expect("health remains live on the single async worker");
            let response=tokio::time::timeout(Duration::from_millis(250),task).await.unwrap().unwrap();
            assert_eq!(response.status(),if stage == BlockingStage::Maintenance { axum::http::StatusCode::ACCEPTED } else { axum::http::StatusCode::GATEWAY_TIMEOUT });
            assert_eq!(inspector.available_blocking_permits(),0,"timed-out disk work still owns its permit");
            let scan_id = crate::maintenance_preview_snapshot(&state).operation_id;
            if stage == BlockingStage::Maintenance {
                let duplicate = crate::maintenance_preview_with_budget(state.clone(), 45, inspector.clone()).await;
                assert_eq!(duplicate.status(), axum::http::StatusCode::ACCEPTED);
                let scan = crate::maintenance_preview_snapshot(&state);
                assert_eq!(scan.operation_id, scan_id, "duplicate joins the original scan rather than creating another worker");
                assert_eq!(scan.retention_days, 30, "the active scan keeps its original inputs");
                assert!(scan.wait_message.as_deref().is_some_and(|m| m.contains("absolute wait deadline")));
                let status = tokio::time::timeout(Duration::from_millis(30), crate::maintenance_status(axum::extract::State(state.clone()))).await.expect("scan status does not wait behind the blocked disk worker");
                assert_eq!(status.status(), axum::http::StatusCode::OK);
            }
            gate.release();
            tokio::time::timeout(Duration::from_secs(2),async {while inspector.available_blocking_permits()!=1 {tokio::task::yield_now().await;}}).await.unwrap();
            if stage == BlockingStage::Maintenance {
                let scan = crate::maintenance_preview_snapshot(&state);
                assert!(matches!(scan.state, crate::MaintenancePreviewPhase::Failed));
                assert!(scan.error.as_deref().is_some_and(|m| m.contains("line 1 column 1")), "the original JSON error remains available after the foreground wait timed out");
                let retry = crate::maintenance_preview_with_budget(state.clone(), 30, RuntimeRequestContext::production()).await;
                assert_eq!(retry.status(), axum::http::StatusCode::BAD_REQUEST);
                assert_ne!(crate::maintenance_preview_snapshot(&state).operation_id, scan_id, "failed workers release the reservation for retry");
            }
            let _=fs::remove_dir_all(root);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn slow_runtime_config_load_is_inside_the_get_request_budget() {
        let root = fixture_root("slow-route-config");
        let paths = NexusPaths::from_root(root.join("data"));
        let (hooks, entered_rx, gate) = blocking_stage_hook(BlockingStage::ConfigFile, 0);
        let request = RuntimeRequestContext::with_budget(
            ProbeBudget {
                round: Duration::from_millis(75),
                child: Duration::from_millis(25),
                cleanup: Duration::from_millis(10),
            },
            BlockingFs::new(1),
            hooks,
        );
        let inspector = request.clone();
        let config_store = nexus_core::ConfigStore::new(paths.clone());

        let response = tokio::spawn(crate::runtime_status_for_parts(
            paths,
            config_store,
            request,
        ));
        tokio::time::timeout(Duration::from_millis(200), entered_rx)
            .await
            .expect("config load enters the bounded blocking pool")
            .expect("config hook remains connected");
        let bounded = tokio::time::timeout(Duration::from_millis(200), response)
            .await
            .expect("GET runtime obeys its absolute deadline")
            .expect("GET runtime task completes");
        gate.release();
        assert_eq!(
            bounded.status(),
            axum::http::StatusCode::GATEWAY_TIMEOUT
        );
        tokio::time::timeout(Duration::from_secs(1), async {
            while inspector.available_blocking_permits() != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("config worker releases its permit after the real read exits");
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn slow_release_requirements_are_inside_the_plan_request_budget() {
        let root = fixture_root("slow-plan-requirements");
        let paths = NexusPaths::from_root(root.join("data"));
        let releases = nexus_core::ReleaseStore::new(paths.clone());
        releases
            .register("release-a", "tag-a", None, None)
            .expect("release registers");
        let release_root = releases
            .release_root("release-a")
            .expect("release resolves");
        fs::create_dir_all(release_root.join("apps/cli")).expect("CLI directory creates");
        fs::write(
            release_root.join("package.json"),
            br#"{"engines":{"node":"^22.19.0 || >=24.0.0"},"packageManager":"pnpm@11.7.0"}"#,
        )
        .expect("root package writes");
        fs::write(
            release_root.join("apps/cli/package.json"),
            br#"{"engines":{"node":">=22.19.0"}}"#,
        )
        .expect("CLI package writes");
        let config_store = nexus_core::ConfigStore::new(paths);
        let (hooks, entered_rx, gate) = blocking_stage_hook(BlockingStage::Requirements, 0);
        let runtime_request = RuntimeRequestContext::with_budget(
            ProbeBudget {
                round: Duration::from_millis(150),
                child: Duration::from_millis(50),
                cleanup: Duration::from_millis(25),
            },
            BlockingFs::new(MAX_BLOCKING_FS_OPERATIONS),
            hooks,
        );
        let inspector = runtime_request.clone();

        let plan = tokio::spawn(async move {
            crate::runtime_plan::plan_registered_release(
                &releases,
                &config_store,
                nexus_protocol::RuntimePlanRequest {
                    release_id: "release-a".to_owned(),
                    source: nexus_protocol::RuntimeSource::Official,
                    mode: nexus_protocol::RuntimeInstallMode::Portable,
                },
                &runtime_request,
            )
            .await
        });
        tokio::time::timeout(Duration::from_millis(300), entered_rx)
            .await
            .expect("requirements load enters the bounded blocking pool")
            .expect("requirements hook remains connected");
        let error = tokio::time::timeout(Duration::from_millis(300), plan)
            .await
            .expect("POST runtime plan obeys its absolute deadline")
            .expect("POST runtime plan task completes")
            .expect_err("blocked requirements cannot produce a plan");
        gate.release();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        tokio::time::timeout(Duration::from_secs(1), async {
            while inspector.available_blocking_permits() != MAX_BLOCKING_FS_OPERATIONS {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("requirements worker releases its permit after the real read exits");
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn timed_out_preparation_workers_keep_all_three_shared_permits_until_exit() {
        let root = fixture_root("request-permit-limit");
        let blocking_fs = BlockingFs::new(MAX_BLOCKING_FS_OPERATIONS);
        let gate = BlockingGate::new();
        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
        let (finished_tx, mut finished_rx) = tokio::sync::mpsc::unbounded_channel();
        let hooks = BlockingHooks {
            callback: Some(Arc::new({
                let gate = gate.clone();
                let active = Arc::clone(&active);
                let maximum = Arc::clone(&maximum);
                move |stage, _| {
                    if stage != BlockingStage::ConfigFile {
                        return;
                    }
                    let count = active.fetch_add(1, Ordering::SeqCst) + 1;
                    maximum.fetch_max(count, Ordering::SeqCst);
                    let _ = entered_tx.send(());
                    gate.wait();
                    active.fetch_sub(1, Ordering::SeqCst);
                    let _ = finished_tx.send(());
                }
            })),
        };
        let budget = ProbeBudget {
            round: Duration::from_millis(150),
            child: Duration::from_millis(50),
            cleanup: Duration::from_millis(25),
        };
        let inspector =
            RuntimeRequestContext::with_budget(budget, blocking_fs.clone(), hooks.clone());
        let holders = (0..MAX_BLOCKING_FS_OPERATIONS)
            .map(|index| {
                let request =
                    RuntimeRequestContext::with_budget(budget, blocking_fs.clone(), hooks.clone());
                let path = root.join(format!("config-{index}.json"));
                tokio::spawn(async move {
                    request
                        .run_blocking_io(BlockingStage::ConfigFile, path, || Ok(()))
                        .await
                })
            })
            .collect::<Vec<_>>();
        for _ in 0..MAX_BLOCKING_FS_OPERATIONS {
            tokio::time::timeout(Duration::from_secs(1), entered_rx.recv())
                .await
                .expect("bounded preparation worker enters")
                .expect("entry channel remains open");
        }
        for holder in holders {
            let error = tokio::time::timeout(Duration::from_millis(300), holder)
                .await
                .expect("timed-out holder returns under its request deadline")
                .expect("holder task completes")
                .expect_err("blocked preparation must time out");
            assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        }
        assert_eq!(inspector.available_blocking_permits(), 0);

        let later_request = RuntimeRequestContext::with_budget(budget, blocking_fs, hooks);
        let later_started = Instant::now();
        let error = later_request
            .run_blocking_io(
                BlockingStage::ConfigFile,
                root.join("later.json"),
                || Ok(()),
            )
            .await
            .expect_err("later request cannot bypass saturated preparation workers");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(later_started.elapsed() < Duration::from_millis(300));
        assert_eq!(maximum.load(Ordering::SeqCst), MAX_BLOCKING_FS_OPERATIONS);
        assert!(entered_rx.try_recv().is_err());

        gate.release();
        for _ in 0..MAX_BLOCKING_FS_OPERATIONS {
            tokio::time::timeout(Duration::from_secs(1), finished_rx.recv())
                .await
                .expect("blocked preparation worker exits after release")
                .expect("finish channel remains open");
        }
        assert_eq!(active.load(Ordering::SeqCst), 0);
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn preparation_time_is_not_reset_before_child_probe_and_cleanup() {
        let root = fixture_root("request-child-deadline");
        let paths = NexusPaths::from_root(root.join("data"));
        let system_dir = root.join("system");
        let node = write_hanging_fixture(&system_dir, "node");
        let pnpm = write_hanging_fixture(&system_dir, "pnpm");
        let git = write_hanging_fixture(&system_dir, "git");
        let runtime = RuntimeConfig {
            node: Some(nexus_core::RuntimePin {
                path: node,
                ownership: RuntimeOwnership::System,
            }),
            pnpm: Some(nexus_core::RuntimePin {
                path: pnpm,
                ownership: RuntimeOwnership::System,
            }),
            git: Some(nexus_core::RuntimePin {
                path: git,
                ownership: RuntimeOwnership::System,
            }),
            ..RuntimeConfig::default()
        };
        let request = RuntimeRequestContext::with_budget(
            ProbeBudget {
                round: Duration::from_secs(6),
                child: Duration::from_secs(5),
                cleanup: Duration::from_millis(500),
            },
            BlockingFs::new(MAX_BLOCKING_FS_OPERATIONS),
            BlockingHooks::default(),
        );
        let started = Instant::now();
        request
            .run_blocking_io(BlockingStage::ConfigFile, paths.config_file.clone(), || {
                std::thread::sleep(Duration::from_secs(4));
                Ok(())
            })
            .await
            .expect("preparation completes inside request deadline");
        // Keep the regression discriminatory without relying on sub-second
        // Windows process startup/reaping under the full parallel test suite.
        // Correct code has at most two seconds left; restarting the six-second
        // round would let the hanging child probe run for five more seconds.
        let observed = tokio::time::timeout(
            Duration::from_secs(3),
            observe_runtime_selection_until(&paths, Some(&runtime), &request),
        )
        .await
        .expect("child probes consume only the original request remainder");
        assert!(started.elapsed() < Duration::from_secs(7));
        assert!(observed.tools.iter().all(|tool| !tool.available));
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn unc_search_path_is_skipped_before_filesystem_enumeration() {
        let root = fixture_root("unc-skip");
        let mut config = config_for(&root, None);
        config.search_path = Some(OsString::from(r"\\server\share"));
        let entered = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        config.blocking_hooks.callback = Some(Arc::new({
            let entered = Arc::clone(&entered);
            move |stage, _| {
                if stage == BlockingStage::SystemDirectory {
                    entered.fetch_add(1, Ordering::SeqCst);
                }
            }
        }));
        let status = tokio::time::timeout(
            Duration::from_millis(200),
            observe_tool_with_budget(
                "pnpm",
                config,
                ProbeBudget {
                    round: Duration::from_millis(75),
                    child: Duration::from_millis(25),
                    cleanup: Duration::from_millis(10),
                },
            ),
        )
        .await
        .expect("UNC path does not block the response");
        assert!(!status.available);
        assert_eq!(entered.load(Ordering::SeqCst), 0);
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn late_blocked_candidate_cannot_extend_round_or_cleanup_budget() {
        let root = fixture_root("late-candidate");
        let first = root.join("system-first");
        let second = root.join("system-second");
        write_hanging_fixture(&first, "pnpm");
        write_version_fixture(&second, "pnpm", "10.15.0");
        let mut config = config_for(&root, None);
        config.search_path =
            Some(env::join_paths([&first, &second]).expect("fixture PATH encodes"));
        let (hooks, entered_rx, gate) = blocking_stage_hook(BlockingStage::ProbeCandidate, 1);
        config.blocking_hooks = hooks;
        let budget = ProbeBudget {
            round: Duration::from_millis(140),
            child: Duration::from_millis(50),
            cleanup: Duration::from_millis(20),
        };
        let observation = tokio::spawn(observe_tool_with_budget("pnpm", config, budget));
        tokio::time::timeout(Duration::from_millis(120), entered_rx)
            .await
            .expect("second candidate is reached")
            .expect("second candidate signal remains open");
        let bounded = tokio::time::timeout(Duration::from_millis(240), observation).await;
        gate.release();
        assert!(
            bounded.is_ok(),
            "late candidate extended the round deadline"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn cancelling_observer_keeps_detached_child_cleanup_bounded() {
        let root = fixture_root("cancel-cleanup");
        let system_dir = root.join("system");
        write_hanging_fixture(&system_dir, "pnpm");
        let mut config = config_for(&root, Some(&system_dir));
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
        config.probe_lifecycle.sender = Some(events_tx);
        let observation = tokio::spawn(observe_tool_with_budget(
            "pnpm",
            config,
            ProbeBudget {
                round: Duration::from_millis(180),
                child: Duration::from_millis(80),
                cleanup: Duration::from_millis(30),
            },
        ));
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), events_rx.recv())
                .await
                .expect("probe starts")
                .expect("probe event channel remains open"),
            ProbeLifecycleEvent::Started
        );
        observation.abort();
        let _ = observation.await;
        let finished = tokio::time::timeout(Duration::from_millis(300), async {
            loop {
                if let Some(ProbeLifecycleEvent::Finished { reaped }) = events_rx.recv().await {
                    break reaped;
                }
            }
        })
        .await
        .expect("detached cleanup remains bounded");
        assert!(finished, "cancelled probe must reap its child");
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn blocked_filesystem_workers_have_a_hard_concurrency_limit() {
        let root = fixture_root("blocking-limit");
        let search_dir = root.join("system");
        fs::create_dir_all(&search_dir).expect("system directory creates");
        let blocking_fs = BlockingFs::new(2);
        let gate = BlockingGate::new();
        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
        let (finished_tx, mut finished_rx) = tokio::sync::mpsc::unbounded_channel();
        let hooks = BlockingHooks {
            callback: Some(Arc::new({
                let gate = gate.clone();
                let active = Arc::clone(&active);
                let maximum = Arc::clone(&maximum);
                move |stage, _| {
                    if stage != BlockingStage::SystemDirectory {
                        return;
                    }
                    let count = active.fetch_add(1, Ordering::SeqCst) + 1;
                    maximum.fetch_max(count, Ordering::SeqCst);
                    let _ = entered_tx.send(());
                    gate.wait();
                    active.fetch_sub(1, Ordering::SeqCst);
                    let _ = finished_tx.send(());
                }
            })),
        };
        let mut config = config_for(&root, Some(&search_dir));
        config.blocking_fs = blocking_fs;
        config.blocking_hooks = hooks;
        let budget = ProbeBudget {
            round: Duration::from_millis(75),
            child: Duration::from_millis(25),
            cleanup: Duration::from_millis(10),
        };
        let holders = (0..2)
            .map(|_| tokio::spawn(observe_tool_with_budget("pnpm", config.clone(), budget)))
            .collect::<Vec<_>>();
        for _ in 0..2 {
            tokio::time::timeout(Duration::from_secs(1), entered_rx.recv())
                .await
                .expect("bounded worker enters")
                .expect("entry channel remains open");
        }
        let waiters = (0..6)
            .map(|_| tokio::spawn(observe_tool_with_budget("pnpm", config.clone(), budget)))
            .collect::<Vec<_>>();
        for waiter in waiters {
            tokio::time::timeout(Duration::from_millis(200), waiter)
                .await
                .expect("waiting request obeys deadline")
                .expect("waiting observation completes");
        }
        for holder in holders {
            tokio::time::timeout(Duration::from_millis(200), holder)
                .await
                .expect("holder request obeys deadline")
                .expect("holder observation completes");
        }
        assert_eq!(maximum.load(Ordering::SeqCst), 2);
        assert!(entered_rx.try_recv().is_err());
        gate.release();
        for _ in 0..2 {
            tokio::time::timeout(Duration::from_secs(1), finished_rx.recv())
                .await
                .expect("blocked worker exits after release")
                .expect("finish channel remains open");
        }
        assert_eq!(active.load(Ordering::SeqCst), 0);
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn missing_tool_is_unavailable_without_error() {
        let root = fixture_root("missing");
        let config = config_for(&root, None);
        let status = observe_tool("node", config).await;
        assert!(!status.available);
        assert!(status.version.is_none());
        assert!(status.source.is_none());
        assert_eq!(status.reason.as_deref(), Some(REASON_NOT_FOUND));
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn first_valid_system_source_wins_over_portable_runtime() {
        let root = fixture_root("priority");
        let system_dir = root.join("system");
        let portable_dir = root.join("data/runtimes/pnpm-v1");
        let system = write_version_fixture(&system_dir, "pnpm", "10.1.0");
        write_version_fixture(&portable_dir, "pnpm", "9.2.0");
        let config = config_for(&root, Some(&system_dir));
        let status = observe_tool("pnpm", config).await;
        assert_eq!(status.source.as_deref(), Some("system"));
        assert_eq!(status.version.as_deref(), Some("10.1.0"));
        let expected_path = display_path(&fs::canonicalize(system).unwrap());
        assert_eq!(status.path.as_deref(), Some(expected_path.as_str()));
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn portable_runtime_is_discovered_only_inside_data_root() {
        let root = fixture_root("portable");
        let portable = root.join("data/runtimes/pnpm-v1");
        let path = write_version_fixture(&portable, "pnpm", "10.3.0");
        let config = config_for(&root, None);
        let status = observe_tool("pnpm", config).await;
        assert_eq!(status.source.as_deref(), Some("nexus"));
        assert_eq!(status.version.as_deref(), Some("10.3.0"));
        let expected_path = display_path(&fs::canonicalize(path).unwrap());
        assert_eq!(status.path.as_deref(), Some(expected_path.as_str()));
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn malformed_stdout_is_unavailable() {
        let root = fixture_root("malformed");
        let system_dir = root.join("system");
        write_version_fixture(&system_dir, "pnpm", "pnpm is not a version");
        let config = config_for(&root, Some(&system_dir));
        let status = observe_tool("pnpm", config).await;
        assert!(!status.available);
        assert!(status.version.is_none());
        assert_eq!(status.source.as_deref(), Some("system"));
        assert_eq!(
            status.reason.as_deref(),
            Some(REASON_INVALID_VERSION_OUTPUT)
        );
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn empty_success_stdout_is_unavailable() {
        let root = fixture_root("empty-stdout");
        let system_dir = root.join("system");
        let path = write_empty_fixture(&system_dir, "pnpm");
        let config = config_for(&root, Some(&system_dir));
        let status = observe_tool("pnpm", config).await;
        assert!(!status.available);
        assert_eq!(status.source.as_deref(), Some("system"));
        assert_eq!(
            status.reason.as_deref(),
            Some(REASON_INVALID_VERSION_OUTPUT)
        );
        assert_eq!(
            status.path.as_deref(),
            Some(display_path(&fs::canonicalize(path).unwrap()).as_str())
        );
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn nonzero_probe_is_unavailable() {
        let root = fixture_root("nonzero");
        let system_dir = root.join("system");
        let path = write_nonzero_fixture(&system_dir, "pnpm");
        let config = config_for(&root, Some(&system_dir));
        let status = observe_tool("pnpm", config).await;
        assert!(!status.available);
        assert_eq!(status.source.as_deref(), Some("system"));
        assert_eq!(status.reason.as_deref(), Some(REASON_PROBE_FAILED));
        assert_eq!(
            status.path.as_deref(),
            Some(display_path(&fs::canonicalize(path).unwrap()).as_str())
        );
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn oversized_stdout_is_unavailable() {
        let root = fixture_root("oversized-stdout");
        let system_dir = root.join("system");
        let path = write_oversized_fixture(&system_dir, "pnpm");
        let config = config_for(&root, Some(&system_dir));
        let status = observe_tool("pnpm", config).await;
        assert!(!status.available);
        assert_eq!(status.source.as_deref(), Some("system"));
        assert_eq!(status.reason.as_deref(), Some(REASON_PROBE_FAILED));
        assert_eq!(
            status.path.as_deref(),
            Some(display_path(&fs::canonicalize(path).unwrap()).as_str())
        );
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn timed_out_probe_is_bounded() {
        let root = fixture_root("timeout");
        let system_dir = root.join("system");
        let path = write_hanging_fixture(&system_dir, "pnpm");
        let config = config_for(&root, Some(&system_dir));
        let started = Instant::now();
        let status = observe_tool_with_budget("pnpm", config, short_probe_budget()).await;
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(!status.available);
        assert_eq!(status.source.as_deref(), Some("system"));
        assert_eq!(status.reason.as_deref(), Some(REASON_PROBE_FAILED));
        assert_eq!(
            status.path.as_deref(),
            Some(display_path(&fs::canonicalize(path).unwrap()).as_str())
        );
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn timed_out_probe_reports_a_reaped_child() {
        let root = fixture_root("timeout-reaped");
        let system_dir = root.join("system");
        let path = write_hanging_fixture(&system_dir, "pnpm");
        let probe_cwd = fs::canonicalize(root.join("cwd")).expect("probe cwd canonicalizes");
        let mut command = version_command(&path).expect("fixture command resolves");
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .current_dir(probe_cwd)
            .kill_on_drop(true);
        apply_probe_environment(&mut command);

        // This test proves child reaping, not a 25 ms OS scheduling bound.
        // Keep the probe short but give cleanup its production allowance.
        let budget = ProbeBudget { cleanup: CHILD_CLEANUP_TIMEOUT, ..short_probe_budget() };
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
        let result = run_version_probe_observed(
            command,
            Instant::now() + Duration::from_secs(3),
            budget,
            ProbeLifecycle { sender: Some(events_tx) },
        )
        .await;
        assert_eq!(events_rx.try_recv().unwrap(), ProbeLifecycleEvent::Started);
        assert!(result.output.is_none());
        assert!(result.reaped, "timed-out probe must wait for its child");
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn exhausted_probe_budget_retains_candidate_context() {
        let root = fixture_root("budget-exhausted");
        let system_dir = root.join("system");
        let path = write_version_fixture(&system_dir, "pnpm", "10.15.0");
        let config = config_for(&root, Some(&system_dir));
        let budget = ProbeBudget {
            round: Duration::ZERO,
            child: Duration::from_millis(50),
            cleanup: Duration::from_millis(25),
        };
        let status = observe_tool_with_budget("pnpm", config, budget).await;
        assert!(!status.available);
        assert_eq!(status.reason.as_deref(), Some(REASON_PROBE_BUDGET_EXCEEDED));
        assert_eq!(
            status.path.as_deref(),
            Some(
                display_path(
                    &path
                        .parent()
                        .expect("fixture has parent")
                        .join(executable_names("pnpm")[0]),
                )
                .as_str()
            )
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn corepack_probe_environment_is_explicitly_non_networking() {
        assert_eq!(
            COREPACK_PROBE_ENV,
            [
                ("COREPACK_ENABLE_NETWORK", "0"),
                ("COREPACK_ENABLE_DOWNLOAD_PROMPT", "0"),
                ("COREPACK_DEFAULT_TO_LATEST", "0"),
                ("COREPACK_ENABLE_AUTO_PIN", "0"),
                ("COREPACK_ENABLE_PROJECT_SPEC", "0"),
            ]
        );
    }

    #[test]
    fn probe_cwd_fallback_skips_project_manifest_without_writing() {
        let root = fixture_root("project-cwd");
        let project = root.join("cwd/project");
        let safe = root.join("cwd/safe");
        fs::create_dir_all(&project).expect("project cwd creates");
        fs::create_dir_all(&safe).expect("safe cwd creates");
        fs::write(project.join("package.json"), b"{}\n").expect("project fixture writes");
        assert!(!is_safe_probe_cwd(&project));
        let selected = select_probe_cwd([project, safe.clone()]).expect("safe cwd selected");
        assert_eq!(
            selected,
            fs::canonicalize(safe).expect("safe cwd canonicalizes")
        );
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unix_corepack_script_is_not_executed() {
        let root = fixture_root("unix-corepack-shim");
        let system_dir = root.join("system");
        let path = write_probe_fixture(
            &system_dir,
            "pnpm",
            "#!/usr/bin/env sh\n# corepack shim fixture\nprintf '%s\\n' '10.15.0'\n",
        );
        let config = config_for(&root, Some(&system_dir));
        let status = observe_tool("pnpm", config).await;
        assert!(!status.available);
        assert_eq!(status.source.as_deref(), Some("system"));
        assert_eq!(
            status.reason.as_deref(),
            Some(REASON_COREPACK_SHIM_UNVERIFIED)
        );
        assert_eq!(
            status.path.as_deref(),
            Some(display_path(&fs::canonicalize(path).unwrap()).as_str())
        );
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unix_unknown_script_shim_is_not_executed() {
        let root = fixture_root("unix-unknown-shim");
        let system_dir = root.join("system");
        let path = write_probe_fixture(
            &system_dir,
            "pnpm",
            "#!/opt/unknown-wrapper\nprintf '%s\\n' '10.15.0'\n",
        );
        let config = config_for(&root, Some(&system_dir));
        let status = observe_tool("pnpm", config).await;
        assert!(!status.available);
        assert_eq!(status.source.as_deref(), Some("system"));
        assert_eq!(status.reason.as_deref(), Some(REASON_SHIM_UNVERIFIED));
        assert_eq!(
            status.path.as_deref(),
            Some(display_path(&fs::canonicalize(path).unwrap()).as_str())
        );
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn portable_symlink_escape_is_rejected() {
        use std::os::unix::fs::symlink;

        let root = fixture_root("portable-link-escape");
        let outside = root.join("outside");
        let escaped = root.join("data/runtimes/escape");
        write_version_fixture(&outside, "pnpm", "10.3.0");
        symlink(&outside, &escaped).expect("portable escape symlink creates");
        let config = config_for(&root, None);
        assert!(
            portable_candidates("pnpm", &config, Instant::now() + Duration::from_secs(1))
                .is_empty()
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn version_parser_rejects_empty_and_accepts_tool_formats() {
        assert!(parse_version("node", b"\n  ").is_none());
        assert!(parse_version("node", b"v24.19.0\n").is_some());
        assert!(parse_version("pnpm", b"10.15.0\n").is_some());
        assert!(parse_version("git", b"git version 2.51.0.windows.1\n").is_some());
        assert!(parse_version("node", b"node version unknown\n").is_none());
    }

    #[test]
    fn cmd_paths_with_shell_metacharacters_are_rejected_explicitly() {
        assert!(is_safe_cmd_path(Path::new(
            r"C:\Program Files\nodejs\pnpm.cmd"
        )));
        for path in [
            r"C:\tool&escape\pnpm.cmd",
            r"C:\tool|escape\pnpm.cmd",
            r"C:\tool<escape\pnpm.cmd",
            r"C:\tool>escape\pnpm.cmd",
            r"C:\tool^escape\pnpm.cmd",
            r"C:\tool%escape\pnpm.cmd",
            r"C:\tool!escape\pnpm.cmd",
            r"C:\tool(escape)\pnpm.cmd",
        ] {
            assert!(
                !is_safe_cmd_path(Path::new(path)),
                "unsafe path accepted: {path}"
            );
        }
        #[cfg(windows)]
        assert!(version_command(Path::new(r"C:\tool&escape\pnpm.cmd")).is_none());
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_cmd_pnpm_shim_is_invoked_through_cmd() {
        let root = fixture_root("windows-shim");
        let system_dir = root.join("system");
        let path = write_version_fixture(&system_dir, "pnpm", "10.15.0");
        let config = config_for(&root, Some(&system_dir));
        let status = observe_tool("pnpm", config).await;
        assert!(status.available);
        assert_eq!(status.version.as_deref(), Some("10.15.0"));
        let expected_path = display_path(&fs::canonicalize(path).unwrap());
        assert_eq!(status.path.as_deref(), Some(expected_path.as_str()));
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(windows)]
    #[test]
    fn windows_batch_probe_uses_absolute_system_command_processor() {
        let processor = system_command_processor().expect("Windows command processor resolves");
        assert!(processor.is_absolute());
        assert_eq!(
            processor
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_ascii_lowercase)
                .as_deref(),
            Some("cmd.exe")
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_pathex_order_prefers_exe_before_cmd_and_handles_spaces() {
        let root = fixture_root("windows-pathex-order");
        let system_dir = root.join("system with spaces");
        fs::create_dir_all(&system_dir).expect("fixture directory creates");
        fs::write(system_dir.join("pnpm.exe"), b"not a Windows executable")
            .expect("fake exe writes");
        let cmd = write_version_fixture(&system_dir, "pnpm", "10.15.0");
        let config = config_for(&root, Some(&system_dir));
        let status = observe_tool("pnpm", config).await;
        assert!(status.available);
        assert_eq!(status.version.as_deref(), Some("10.15.0"));
        assert_eq!(
            status.path.as_deref(),
            Some(display_path(&fs::canonicalize(cmd).unwrap()).as_str())
        );
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_extensionless_fake_cmd_is_not_selected() {
        let root = fixture_root("windows-extensionless");
        let system_dir = root.join("system");
        fs::create_dir_all(&system_dir).expect("fixture directory creates");
        fs::write(
            system_dir.join("pnpm"),
            "@echo off\r\necho 10.15.0\r\nexit /b 0\r\n",
        )
        .expect("extensionless fake writes");
        let config = config_for(&root, Some(&system_dir));
        let status = observe_tool("pnpm", config).await;
        assert!(!status.available);
        assert_eq!(status.reason.as_deref(), Some(REASON_NOT_FOUND));
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn corepack_cmd_shim_is_unavailable_without_running_it() {
        let root = fixture_root("corepack-shim");
        let system_dir = root.join("system");
        let path = system_dir.join("pnpm.cmd");
        fs::create_dir_all(&system_dir).expect("fixture directory creates");
        fs::write(
            &path,
            "@SETLOCAL\r\nnode \"%~dp0\\node_modules\\corepack\\dist\\pnpm.js\" %*\r\n",
        )
        .expect("corepack fixture writes");
        let config = config_for(&root, Some(&system_dir));
        let status = observe_tool("pnpm", config).await;
        assert!(!status.available);
        assert_eq!(status.source.as_deref(), Some("system"));
        assert_eq!(
            status.path.as_deref(),
            Some(display_path(&fs::canonicalize(path).unwrap()).as_str())
        );
        assert_eq!(
            status.reason.as_deref(),
            Some(REASON_COREPACK_SHIM_UNVERIFIED)
        );
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_cmd_path_with_shell_metacharacter_is_not_invoked() {
        let root = fixture_root("unsafe-cmd-path");
        let system_dir = root.join("system&escape");
        let path = write_version_fixture(&system_dir, "pnpm", "10.15.0");
        let config = config_for(&root, Some(&system_dir));
        let status = observe_tool("pnpm", config).await;
        assert!(!status.available);
        assert_eq!(status.source.as_deref(), Some("system"));
        assert_eq!(status.reason.as_deref(), Some(REASON_UNSAFE_CMD_PATH));
        assert_eq!(
            status.path.as_deref(),
            Some(display_path(&fs::canonicalize(path).unwrap()).as_str())
        );
        let _ = fs::remove_dir_all(root);
    }
}
