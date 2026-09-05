//! Read-only runtime environment observation.
//!
//! Runtime discovery never installs, downloads, writes configuration, changes
//! PATH, or starts a Harness. It only probes already existing executables so
//! the UI can show an honest preflight result before a later install flow.

use std::{
    env,
    ffi::{OsStr, OsString},
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, OnceLock},
    time::Duration,
};

use nexus_core::NexusPaths;
use nexus_protocol::{RuntimeListResponse, RuntimeToolStatus};
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
enum BlockingStage {
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
            portable_root: paths.root.join("runtimes"),
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

/// Observe every known runtime tool. Results are ordered as `RUNTIME_TOOLS`.
pub async fn observe_runtimes(paths: &NexusPaths) -> RuntimeListResponse {
    observe_runtimes_with_budget(
        paths,
        PRODUCTION_PROBE_BUDGET,
        production_blocking_fs(),
        BlockingHooks::default(),
    )
    .await
}

async fn observe_runtimes_with_budget(
    paths: &NexusPaths,
    budget: ProbeBudget,
    blocking_fs: BlockingFs,
    blocking_hooks: BlockingHooks,
) -> RuntimeListResponse {
    let deadline = Instant::now() + budget.round;
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
    let (git, node, pnpm) = tokio::join!(
        observe_tool_until("git", config.clone(), deadline, budget),
        observe_tool_until("node", config.clone(), deadline, budget),
        observe_tool_until("pnpm", config, deadline, budget),
    );
    let tools = vec![git, node, pnpm];
    debug_assert_eq!(tools.len(), RUNTIME_TOOLS.len());
    RuntimeListResponse::new(tools)
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
        if Instant::now() >= deadline {
            record_failure(
                &mut first_failure,
                "nexus",
                &config.portable_root,
                REASON_PROBE_BUDGET_EXCEEDED,
            );
        }
        let portable = if Instant::now() < deadline {
            let portable_config = config.clone();
            let portable_name = name.to_owned();
            config
                .blocking_fs
                .run(deadline, move || {
                    portable_candidates(&portable_name, &portable_config, deadline)
                })
                .await
        } else {
            None
        };
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

    unavailable_status(name, first_failure)
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

#[cfg(test)]
async fn run_version_probe(
    command: Command,
    deadline: Instant,
    budget: ProbeBudget,
) -> ProbeResult {
    run_version_probe_observed(command, deadline, budget, ProbeLifecycle::default()).await
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
        "pnpm" => valid_dot_version(line, false).then(|| line.to_owned()),
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

        let result = run_version_probe(
            command,
            Instant::now() + Duration::from_secs(1),
            short_probe_budget(),
        )
        .await;
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
