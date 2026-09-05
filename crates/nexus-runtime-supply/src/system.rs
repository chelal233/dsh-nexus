use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use async_trait::async_trait;
use nexus_core::{build_runtime_child_env, resolve_runtime_command, RuntimeConfig, RuntimePin};
use nexus_protocol::{RuntimeOwnership, RuntimeSource};
use tokio::{io::AsyncReadExt, process::Command, time::Instant};

use crate::{CancellationToken, HostArch, Result, SupplyError};

const PROCESS_OUTPUT_LIMIT: usize = 64 * 1024;
const PROBE_TIMEOUT: Duration = Duration::from_secs(20);
const INSTALL_TIMEOUT: Duration = Duration::from_secs(15 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemInstallKind {
    NodeMsi,
    PnpmUserScript,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemInstallSpec {
    kind: SystemInstallKind,
    payload_path: PathBuf,
    expected_version: String,
    expected_path: PathBuf,
    architecture: HostArch,
    source: RuntimeSource,
}

impl SystemInstallSpec {
    pub(crate) fn node_msi(
        payload_path: PathBuf,
        expected_version: String,
        expected_path: PathBuf,
        architecture: HostArch,
        source: RuntimeSource,
    ) -> Self {
        Self {
            kind: SystemInstallKind::NodeMsi,
            payload_path,
            expected_version,
            expected_path,
            architecture,
            source,
        }
    }

    pub(crate) fn pnpm_user_script(
        payload_path: PathBuf,
        expected_version: String,
        expected_path: PathBuf,
        architecture: HostArch,
        source: RuntimeSource,
    ) -> Self {
        Self {
            kind: SystemInstallKind::PnpmUserScript,
            payload_path,
            expected_version,
            expected_path,
            architecture,
            source,
        }
    }

    pub fn kind(&self) -> SystemInstallKind {
        self.kind
    }

    pub fn payload_path(&self) -> &Path {
        &self.payload_path
    }

    pub fn expected_version(&self) -> &str {
        &self.expected_version
    }

    pub fn expected_path(&self) -> &Path {
        &self.expected_path
    }

    pub fn architecture(&self) -> HostArch {
        self.architecture
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypedProcessRequest {
    Probe {
        runtime: RuntimeConfig,
        tool: String,
        expected_version: String,
    },
    SystemInstall(SystemInstallSpec),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessOutcome {
    pub exit_code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub timed_out: bool,
    pub cancelled: bool,
    pub os_error: Option<i32>,
}

#[async_trait]
pub trait ProcessRunner: Send + Sync {
    async fn run(
        &self,
        request: &TypedProcessRequest,
        cancellation: &CancellationToken,
    ) -> Result<ProcessOutcome>;
}

#[derive(Debug, Default)]
pub struct CommandProcessRunner;

#[async_trait]
impl ProcessRunner for CommandProcessRunner {
    async fn run(
        &self,
        request: &TypedProcessRequest,
        cancellation: &CancellationToken,
    ) -> Result<ProcessOutcome> {
        cancellation.check()?;
        match request {
            TypedProcessRequest::Probe {
                runtime,
                tool,
                expected_version: _,
            } => run_probe(runtime, tool, cancellation).await,
            TypedProcessRequest::SystemInstall(spec) => {
                run_system_install(spec, cancellation).await
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SystemInstallResult {
    Success,
    RebootRequired,
    UserCancelled,
    UacDenied,
    SpawnFailed(i32),
    Failed(i32),
    NeedsVerification(String),
}

pub fn classify_system_install(outcome: &ProcessOutcome) -> SystemInstallResult {
    if outcome.timed_out {
        return SystemInstallResult::NeedsVerification(
            "installer exceeded its wait budget and may still be running".to_owned(),
        );
    }
    if outcome.cancelled {
        return SystemInstallResult::NeedsVerification(
            "cancellation was observed after system installation began".to_owned(),
        );
    }
    if outcome.os_error == Some(1223) {
        return SystemInstallResult::UacDenied;
    }
    if let Some(code) = outcome.os_error {
        return SystemInstallResult::SpawnFailed(code);
    }
    match outcome.exit_code {
        Some(0) => SystemInstallResult::Success,
        Some(1602) => SystemInstallResult::UserCancelled,
        Some(3010) => SystemInstallResult::RebootRequired,
        Some(code) => SystemInstallResult::Failed(code),
        None => SystemInstallResult::NeedsVerification(
            "installer ended without an exit status".to_owned(),
        ),
    }
}

pub(crate) fn probe_request(
    node: PathBuf,
    pnpm: Option<PathBuf>,
    tool: &str,
    expected_version: &str,
    node_ownership: RuntimeOwnership,
    pnpm_ownership: RuntimeOwnership,
) -> TypedProcessRequest {
    TypedProcessRequest::Probe {
        runtime: RuntimeConfig {
            node: Some(RuntimePin {
                path: node,
                ownership: node_ownership,
            }),
            pnpm: pnpm.map(|path| RuntimePin {
                path,
                ownership: pnpm_ownership,
            }),
            git: None,
            source: RuntimeSource::Official,
            mode: nexus_protocol::RuntimeInstallMode::Portable,
        },
        tool: tool.to_owned(),
        expected_version: expected_version.to_owned(),
    }
}

pub(crate) fn verify_probe(outcome: &ProcessOutcome, expected_version: &str) -> Result<()> {
    if outcome.timed_out
        || outcome.cancelled
        || outcome.os_error.is_some()
        || outcome.exit_code != Some(0)
    {
        return Err(SupplyError::Process(
            "runtime version probe did not exit successfully".to_owned(),
        ));
    }
    let output = std::str::from_utf8(&outcome.stdout)
        .map_err(|_| SupplyError::Process("runtime version output is not UTF-8".to_owned()))?
        .trim();
    if output.strip_prefix('v').unwrap_or(output) != expected_version {
        return Err(SupplyError::Process(format!(
            "runtime reported {output:?}, expected {expected_version:?}"
        )));
    }
    Ok(())
}

async fn run_probe(
    runtime: &RuntimeConfig,
    tool: &str,
    cancellation: &CancellationToken,
) -> Result<ProcessOutcome> {
    let spec = resolve_runtime_command(runtime, tool)?
        .ok_or_else(|| SupplyError::Process(format!("{tool} is not pinned")))?;
    let mut command = Command::new(&spec.program);
    command.args(spec.prefix_args).arg("--version");
    for (name, value) in build_runtime_child_env(runtime, std::env::var_os("PATH").as_deref())? {
        command.env(name, value);
    }
    run_owned_process(command, PROBE_TIMEOUT, cancellation).await
}

async fn run_system_install(
    spec: &SystemInstallSpec,
    cancellation: &CancellationToken,
) -> Result<ProcessOutcome> {
    if !spec.payload_path.is_absolute() || !spec.expected_path.is_absolute() {
        return Err(SupplyError::InvalidPlan(
            "system installer paths must be absolute".to_owned(),
        ));
    }
    let mut command = match spec.kind {
        SystemInstallKind::NodeMsi => {
            let mut command = Command::new(system32_program("msiexec.exe"));
            command
                .arg("/i")
                .arg(&spec.payload_path)
                .arg("/passive")
                .arg("/norestart")
                .arg("ADDLOCAL=NodeRuntime");
            command
        }
        SystemInstallKind::PnpmUserScript => {
            let mut command = Command::new(system32_powershell());
            command
                .arg("-NoLogo")
                .arg("-NoProfile")
                .arg("-NonInteractive")
                .arg("-ExecutionPolicy")
                .arg("Bypass")
                .arg("-File")
                .arg(&spec.payload_path)
                .env("PNPM_VERSION", &spec.expected_version)
                .env(
                    "PNPM_HOME",
                    spec.expected_path.parent().ok_or_else(|| {
                        SupplyError::InvalidPlan("pnpm system path has no parent".to_owned())
                    })?,
                )
                .env(
                    "npm_config_registry",
                    match spec.source {
                        RuntimeSource::Official => "https://registry.npmjs.org",
                        RuntimeSource::Npmmirror => "https://registry.npmmirror.com",
                    },
                );
            command
        }
    };
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    run_detached_system_process(command, INSTALL_TIMEOUT, cancellation).await
}

async fn run_owned_process(
    mut command: Command,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<ProcessOutcome> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    configure_owned_process(&mut command);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return Ok(spawn_error(error)),
    };
    let tree_guard = match OwnedProcessTree::attach(child.id()) {
        Ok(guard) => guard,
        Err(error) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return Err(error);
        }
    };
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let stdout_task = tokio::spawn(read_output(stdout));
    let stderr_task = tokio::spawn(read_output(stderr));
    let deadline = Instant::now() + timeout;
    let mut interval = tokio::time::interval(Duration::from_millis(50));
    let (status, timed_out, cancelled) = loop {
        tokio::select! {
            status = child.wait() => break (Some(status?), false, false),
            _ = tokio::time::sleep_until(deadline) => {
                let _ = child.kill().await;
                let status = child.wait().await.ok();
                break (status, true, false);
            }
            _ = interval.tick() => {
                if cancellation.is_cancelled() {
                    let _ = child.kill().await;
                    let status = child.wait().await.ok();
                    break (status, false, true);
                }
            }
        }
    };
    // Closing the kill-on-close job terminates any descendants that inherited the
    // captured pipes. Do this before awaiting the readers so a child left behind
    // by the direct process cannot keep stdout/stderr open indefinitely.
    drop(tree_guard);
    let stdout = stdout_task
        .await
        .map_err(|error| SupplyError::Process(error.to_string()))??;
    let stderr = stderr_task
        .await
        .map_err(|error| SupplyError::Process(error.to_string()))??;
    Ok(ProcessOutcome {
        exit_code: status.and_then(|status| status.code()),
        stdout,
        stderr,
        timed_out,
        cancelled,
        os_error: None,
    })
}

async fn run_detached_system_process(
    mut command: Command,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<ProcessOutcome> {
    command.kill_on_drop(false);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return Ok(spawn_error(error)),
    };
    let deadline = Instant::now() + timeout;
    let mut interval = tokio::time::interval(Duration::from_millis(100));
    loop {
        tokio::select! {
            status = child.wait() => {
                let status = status?;
                return Ok(ProcessOutcome {
                    exit_code: status.code(), stdout: Vec::new(), stderr: Vec::new(),
                    timed_out: false, cancelled: false, os_error: None,
                });
            }
            _ = tokio::time::sleep_until(deadline) => {
                return Ok(ProcessOutcome {
                    exit_code: None, stdout: Vec::new(), stderr: Vec::new(),
                    timed_out: true, cancelled: false, os_error: None,
                });
            }
            _ = interval.tick() => {
                if cancellation.is_cancelled() {
                    return Ok(ProcessOutcome {
                        exit_code: None, stdout: Vec::new(), stderr: Vec::new(),
                        timed_out: false, cancelled: true, os_error: None,
                    });
                }
            }
        }
    }
}

async fn read_output<R: tokio::io::AsyncRead + Unpin>(stream: Option<R>) -> Result<Vec<u8>> {
    let Some(stream) = stream else {
        return Ok(Vec::new());
    };
    let mut output = Vec::new();
    stream
        .take((PROCESS_OUTPUT_LIMIT + 1) as u64)
        .read_to_end(&mut output)
        .await?;
    if output.len() > PROCESS_OUTPUT_LIMIT {
        return Err(SupplyError::Process(
            "runtime process output exceeded 64 KiB".to_owned(),
        ));
    }
    Ok(output)
}

fn spawn_error(error: std::io::Error) -> ProcessOutcome {
    ProcessOutcome {
        exit_code: None,
        stdout: Vec::new(),
        stderr: error.to_string().into_bytes(),
        timed_out: false,
        cancelled: false,
        os_error: error.raw_os_error(),
    }
}

fn windows_root() -> PathBuf {
    std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
}

fn system32_program(name: &str) -> PathBuf {
    windows_root().join("System32").join(name)
}

fn system32_powershell() -> PathBuf {
    system32_program("WindowsPowerShell")
        .join("v1.0")
        .join("powershell.exe")
}

pub(crate) fn default_system_node_path() -> Result<PathBuf> {
    let root = std::env::var_os("ProgramFiles")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or_else(|| SupplyError::InvalidPlan("ProgramFiles is unavailable".to_owned()))?;
    Ok(root.join("nodejs").join("node.exe"))
}

pub(crate) fn default_system_pnpm_path() -> Result<PathBuf> {
    let root = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or_else(|| SupplyError::InvalidPlan("LOCALAPPDATA is unavailable".to_owned()))?;
    Ok(root.join("pnpm").join("pnpm.exe"))
}

#[cfg(windows)]
fn configure_owned_process(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x01000000;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;
    const CREATE_NO_WINDOW: u32 = 0x08000000;
    command
        .as_std_mut()
        .creation_flags(CREATE_BREAKAWAY_FROM_JOB | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn configure_owned_process(_command: &mut Command) {}

struct OwnedProcessTree {
    #[cfg(windows)]
    job: windows_sys::Win32::Foundation::HANDLE,
}

// The handle is owned exclusively by this guard and is only closed on drop.
// Windows kernel handles may be transferred between threads.
unsafe impl Send for OwnedProcessTree {}

impl OwnedProcessTree {
    fn attach(process_id: Option<u32>) -> Result<Self> {
        #[cfg(windows)]
        unsafe {
            use windows_sys::Win32::{
                Foundation::CloseHandle,
                System::{
                    JobObjects::{
                        AssignProcessToJobObject, CreateJobObjectW,
                        JobObjectExtendedLimitInformation, SetInformationJobObject,
                        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                    },
                    Threading::{
                        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_QUOTA,
                        PROCESS_TERMINATE,
                    },
                },
            };
            let process_id = process_id.ok_or_else(|| {
                SupplyError::Process("spawned runtime process has no PID".to_owned())
            })?;
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return Err(SupplyError::Process(format!(
                    "cannot create runtime process job: {}",
                    std::io::Error::last_os_error()
                )));
            }
            let mut information: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            information.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &information as *const _ as *const _,
                std::mem::size_of_val(&information) as u32,
            ) == 0
            {
                CloseHandle(job);
                return Err(SupplyError::Process(format!(
                    "cannot configure runtime process job: {}",
                    std::io::Error::last_os_error()
                )));
            }
            let process = OpenProcess(
                PROCESS_SET_QUOTA | PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION,
                0,
                process_id,
            );
            if process.is_null() || AssignProcessToJobObject(job, process) == 0 {
                if !process.is_null() {
                    CloseHandle(process);
                }
                CloseHandle(job);
                return Err(SupplyError::Process(format!(
                    "cannot attach runtime process tree: {}",
                    std::io::Error::last_os_error()
                )));
            }
            CloseHandle(process);
            Ok(Self { job })
        }
        #[cfg(not(windows))]
        {
            let _ = process_id;
            Ok(Self {})
        }
    }
}

impl Drop for OwnedProcessTree {
    fn drop(&mut self) {
        #[cfg(windows)]
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.job);
        }
    }
}
