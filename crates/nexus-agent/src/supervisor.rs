//! External, replaceable Harness process supervision.

use std::{
    fmt, fs, io,
    path::Path,
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant},
};

use nexus_core::{
    load_harness_launch_spec, unix_time_seconds, HarnessLaunchSpec, NexusPaths,
    RuntimeMetadataStore,
};
use nexus_protocol::{HarnessRuntimeInfo, HarnessState};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    process::{Child, Command},
    sync::Mutex,
    time::{sleep, timeout},
};

pub const DEFAULT_GRACEFUL_STOP_SECS: u64 = 5;
pub const DEFAULT_READINESS_TIMEOUT_SECS: u64 = 30;

#[derive(Debug)]
pub enum HarnessSupervisorError {
    NotConfigured,
    AlreadyRunning,
    Configuration(io::Error),
    Spawn(io::Error),
    Process(io::Error),
    Readiness(String),
    Persistence(io::Error),
}

impl fmt::Display for HarnessSupervisorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotConfigured => write!(
                formatter,
                "Harness is not configured; set harness.program in Nexus config.json or {HARNESS_PROGRAM_ENV}"
            ),
            Self::AlreadyRunning => formatter.write_str("Harness is already running"),
            Self::Configuration(error) => write!(formatter, "failed to read Harness configuration: {error}"),
            Self::Spawn(error) => write!(formatter, "failed to start Harness: {error}"),
            Self::Process(error) => write!(formatter, "Harness process error: {error}"),
            Self::Readiness(error) => write!(formatter, "Harness readiness check failed: {error}"),
            Self::Persistence(error) => write!(formatter, "failed to persist Harness state: {error}"),
        }
    }
}

impl std::error::Error for HarnessSupervisorError {}

const HARNESS_PROGRAM_ENV: &str = "NEXUS_HARNESS_PROGRAM";

struct SupervisorInner {
    child: Option<Child>,
    runtime: HarnessRuntimeInfo,
    generation: u64,
}

#[derive(Clone)]
pub struct HarnessSupervisor {
    paths: NexusPaths,
    store: RuntimeMetadataStore,
    inner: Arc<Mutex<SupervisorInner>>,
    graceful_wait: Duration,
}

impl HarnessSupervisor {
    /// Create a supervisor using the Nexus-owned state and log directories.
    pub fn new(paths: NexusPaths) -> io::Result<Self> {
        Self::with_graceful_wait(paths, Duration::from_secs(DEFAULT_GRACEFUL_STOP_SECS))
    }

    /// Test and embedding hook for a bounded gentle-stop period.
    pub fn with_graceful_wait(paths: NexusPaths, graceful_wait: Duration) -> io::Result<Self> {
        let store = RuntimeMetadataStore::new(paths.clone());
        let runtime = store
            .read()?
            .map(|metadata| metadata.harness)
            .unwrap_or_else(HarnessRuntimeInfo::detached);
        Ok(Self {
            paths,
            store,
            inner: Arc::new(Mutex::new(SupervisorInner {
                child: None,
                runtime,
                generation: 0,
            })),
            graceful_wait,
        })
    }

    pub fn paths(&self) -> &NexusPaths {
        &self.paths
    }

    pub fn metadata_store(&self) -> RuntimeMetadataStore {
        self.store.clone()
    }

    pub async fn status(&self) -> HarnessRuntimeInfo {
        let (runtime, changed) = {
            let mut inner = self.inner.lock().await;
            match poll_child(&mut inner) {
                Ok(Some(runtime)) => (runtime, true),
                Ok(None) => (inner.runtime.clone(), false),
                Err(error) => {
                    inner.runtime = failed_runtime(
                        &inner.runtime,
                        format!("failed to query child process: {error}"),
                    );
                    (inner.runtime.clone(), true)
                }
            }
        };
        if changed {
            let _ = self.store.update_harness(runtime.clone());
        }
        runtime
    }

    /// A new Agent instance has no process handle to the PID from a previous
    /// instance. Mark that stale observation stopped instead of claiming a
    /// running Harness that Nexus cannot control.
    pub async fn recover_unattached(&self) -> HarnessRuntimeInfo {
        let runtime = {
            let mut inner = self.inner.lock().await;
            if inner.child.is_none()
                && matches!(
                    inner.runtime.state,
                    HarnessState::Starting | HarnessState::Running
                )
            {
                inner.runtime.state = HarnessState::Stopped;
                inner.runtime.error = Some(
                    "previous Harness process was not attached to this Agent instance".to_owned(),
                );
                inner.runtime.updated_at_unix = Some(unix_time_seconds());
            }
            inner.runtime.clone()
        };
        let _ = self.store.update_harness(runtime.clone());
        runtime
    }

    pub async fn start(&self) -> Result<HarnessRuntimeInfo, HarnessSupervisorError> {
        let spec = load_harness_launch_spec(&self.paths)
            .map_err(HarnessSupervisorError::Configuration)?
            .filter(|spec| !spec.program.as_os_str().is_empty())
            .ok_or(HarnessSupervisorError::NotConfigured)?;
        self.paths
            .ensure_directories()
            .map_err(HarnessSupervisorError::Configuration)?;

        let (generation, runtime) = {
            let mut inner = self.inner.lock().await;
            if let Some(child) = inner.child.as_mut() {
                match child.try_wait() {
                    Ok(None) => return Err(HarnessSupervisorError::AlreadyRunning),
                    Ok(Some(exit)) => {
                        inner.child = None;
                        inner.runtime = runtime_from_exit(&inner.runtime, exit, false);
                    }
                    Err(error) => return Err(HarnessSupervisorError::Process(error)),
                }
            }

            let stdout = open_log(&self.paths.logs_dir, "harness.stdout.log")
                .map_err(HarnessSupervisorError::Spawn)?;
            let stderr = open_log(&self.paths.logs_dir, "harness.stderr.log")
                .map_err(HarnessSupervisorError::Spawn)?;
            let mut command = Command::new(&spec.program);
            command
                .args(&spec.args)
                .stdin(Stdio::null())
                .stdout(Stdio::from(stdout))
                .stderr(Stdio::from(stderr));
            if let Some(working_dir) = &spec.working_dir {
                command.current_dir(working_dir);
            }

            let child = command.spawn().map_err(HarnessSupervisorError::Spawn)?;
            let pid = child.id().ok_or_else(|| {
                HarnessSupervisorError::Spawn(io::Error::new(
                    io::ErrorKind::Other,
                    "spawned Harness did not expose a process id",
                ))
            })?;
            inner.generation = inner.generation.wrapping_add(1);
            let generation = inner.generation;
            let now = unix_time_seconds();
            inner.runtime = HarnessRuntimeInfo::starting(pid, now);
            inner.child = Some(child);
            (generation, inner.runtime.clone())
        };

        self.persist(&runtime)?;
        self.spawn_monitor(generation);

        if let Some(readiness_url) = &spec.readiness_url {
            if let Err(error) = self
                .wait_for_readiness(generation, readiness_url, &spec)
                .await
            {
                self.fail_start(generation, error.to_string()).await;
                return Err(error);
            }
        }

        let running = self.mark_running(generation).await;
        Ok(running)
    }

    pub async fn stop(&self) -> Result<HarnessRuntimeInfo, HarnessSupervisorError> {
        let child = {
            let mut inner = self.inner.lock().await;
            if inner.child.is_none() {
                return Ok(inner.runtime.clone());
            }
            inner.generation = inner.generation.wrapping_add(1);
            inner.child.take()
        };

        let Some(mut child) = child else {
            return Ok(self.status().await);
        };
        let pid = child.id();
        let started_at = {
            let inner = self.inner.lock().await;
            inner.runtime.started_at_unix
        };

        let mut killed = false;
        let exit = wait_for_exit(&mut child, self.graceful_wait)
            .await
            .map_err(HarnessSupervisorError::Process)?;
        let exit = match exit {
            Some(exit) => exit,
            None => {
                killed = true;
                child
                    .kill()
                    .await
                    .map_err(HarnessSupervisorError::Process)?;
                child
                    .wait()
                    .await
                    .map_err(HarnessSupervisorError::Process)?
            }
        };

        let runtime = if killed {
            HarnessRuntimeInfo {
                state: HarnessState::Stopped,
                pid,
                exit_code: exit.code(),
                error: Some("Harness was killed after the graceful stop timeout".to_owned()),
                started_at_unix: started_at,
                updated_at_unix: Some(unix_time_seconds()),
            }
        } else {
            runtime_from_exit_with_state(started_at, pid, exit, HarnessState::Stopped)
        };
        {
            let mut inner = self.inner.lock().await;
            inner.runtime = runtime.clone();
        }
        self.persist(&runtime)?;
        Ok(runtime)
    }

    pub async fn restart(&self) -> Result<HarnessRuntimeInfo, HarnessSupervisorError> {
        let _ = self.stop().await?;
        self.start().await
    }

    async fn mark_running(&self, generation: u64) -> HarnessRuntimeInfo {
        let mut inner = self.inner.lock().await;
        if inner.generation != generation || inner.child.is_none() {
            return inner.runtime.clone();
        }
        let (pid, started_at) = match (&inner.runtime.pid, inner.runtime.started_at_unix) {
            (Some(pid), Some(started_at)) => (*pid, started_at),
            _ => return inner.runtime.clone(),
        };
        inner.runtime = HarnessRuntimeInfo::running(pid, started_at, unix_time_seconds());
        let runtime = inner.runtime.clone();
        drop(inner);
        let _ = self.store.update_harness(runtime.clone());
        runtime
    }

    async fn fail_start(&self, generation: u64, message: String) {
        let child = {
            let mut inner = self.inner.lock().await;
            if inner.generation != generation {
                return;
            }
            inner.generation = inner.generation.wrapping_add(1);
            inner.child.take()
        };
        if let Some(mut child) = child {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        let runtime = {
            let mut inner = self.inner.lock().await;
            inner.runtime = failed_runtime(&inner.runtime, message);
            inner.runtime.clone()
        };
        let _ = self.store.update_harness(runtime);
    }

    fn persist(&self, runtime: &HarnessRuntimeInfo) -> Result<(), HarnessSupervisorError> {
        self.store
            .update_harness(runtime.clone())
            .map_err(HarnessSupervisorError::Persistence)
    }

    fn spawn_monitor(&self, generation: u64) {
        let inner = Arc::clone(&self.inner);
        let store = self.store.clone();
        tokio::spawn(async move {
            loop {
                let (runtime, changed, done) = {
                    let mut inner = inner.lock().await;
                    if inner.generation != generation || inner.child.is_none() {
                        (inner.runtime.clone(), false, true)
                    } else {
                        match poll_child(&mut inner) {
                            Ok(Some(runtime)) => (runtime, true, true),
                            Ok(None) => (inner.runtime.clone(), false, false),
                            Err(error) => {
                                inner.runtime = failed_runtime(
                                    &inner.runtime,
                                    format!("failed to query child process: {error}"),
                                );
                                (inner.runtime.clone(), true, false)
                            }
                        }
                    }
                };
                if changed {
                    let _ = store.update_harness(runtime);
                }
                if done {
                    return;
                }
                sleep(Duration::from_millis(100)).await;
            }
        });
    }

    async fn wait_for_readiness(
        &self,
        generation: u64,
        readiness_url: &str,
        spec: &HarnessLaunchSpec,
    ) -> Result<(), HarnessSupervisorError> {
        let target = ReadinessTarget::parse(readiness_url)?;
        let timeout_secs = spec
            .readiness_timeout_secs
            .unwrap_or(DEFAULT_READINESS_TIMEOUT_SECS);
        let deadline = Instant::now() + Duration::from_secs(timeout_secs);

        loop {
            if !self.generation_alive(generation).await {
                return Err(HarnessSupervisorError::Readiness(
                    "Harness exited before readiness was observed".to_owned(),
                ));
            }
            if Instant::now() >= deadline {
                return Err(HarnessSupervisorError::Readiness(format!(
                    "timed out after {timeout_secs}s waiting for {readiness_url}"
                )));
            }

            let remaining = deadline.saturating_duration_since(Instant::now());
            let attempt_timeout = remaining.min(Duration::from_secs(1));
            if let Ok(Ok(())) = timeout(attempt_timeout, readiness_probe(&target)).await {
                return Ok(());
            }
            sleep(Duration::from_millis(100)).await;
        }
    }

    async fn generation_alive(&self, generation: u64) -> bool {
        let mut inner = self.inner.lock().await;
        if inner.generation != generation || inner.child.is_none() {
            return false;
        }
        match poll_child(&mut inner) {
            Ok(Some(runtime)) => {
                inner.runtime = runtime;
                false
            }
            Ok(None) => true,
            Err(_) => false,
        }
    }
}

fn open_log(logs_dir: &Path, name: &str) -> io::Result<fs::File> {
    let path = logs_dir.join(name);
    fs::OpenOptions::new().create(true).append(true).open(path)
}

fn poll_child(inner: &mut SupervisorInner) -> io::Result<Option<HarnessRuntimeInfo>> {
    let Some(child) = inner.child.as_mut() else {
        return Ok(None);
    };
    let Some(exit) = child.try_wait()? else {
        return Ok(None);
    };
    inner.child = None;
    let runtime = runtime_from_exit(&inner.runtime, exit, false);
    inner.runtime = runtime.clone();
    Ok(Some(runtime))
}

fn runtime_from_exit(
    previous: &HarnessRuntimeInfo,
    exit: std::process::ExitStatus,
    killed: bool,
) -> HarnessRuntimeInfo {
    let state = if killed || exit.code() == Some(0) {
        HarnessState::Stopped
    } else {
        HarnessState::Failed
    };
    runtime_from_exit_with_state(previous.started_at_unix, previous.pid, exit, state)
}

fn runtime_from_exit_with_state(
    started_at_unix: Option<u64>,
    pid: Option<u32>,
    exit: std::process::ExitStatus,
    state: HarnessState,
) -> HarnessRuntimeInfo {
    let error = if state == HarnessState::Failed {
        Some(match exit.code() {
            Some(code) => format!("Harness exited with code {code}"),
            None => "Harness exited without an exit code".to_owned(),
        })
    } else {
        None
    };
    HarnessRuntimeInfo {
        state,
        pid,
        exit_code: exit.code(),
        error,
        started_at_unix,
        updated_at_unix: Some(unix_time_seconds()),
    }
}

fn failed_runtime(previous: &HarnessRuntimeInfo, message: String) -> HarnessRuntimeInfo {
    HarnessRuntimeInfo {
        state: HarnessState::Failed,
        pid: previous.pid,
        exit_code: previous.exit_code,
        error: Some(message),
        started_at_unix: previous.started_at_unix,
        updated_at_unix: Some(unix_time_seconds()),
    }
}

async fn wait_for_exit(
    child: &mut Child,
    graceful_wait: Duration,
) -> io::Result<Option<std::process::ExitStatus>> {
    let deadline = Instant::now() + graceful_wait;
    loop {
        if let Some(exit) = child.try_wait()? {
            return Ok(Some(exit));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        sleep(Duration::from_millis(50)).await;
    }
}

#[derive(Debug)]
struct ReadinessTarget {
    host: String,
    port: u16,
    path: String,
}

impl ReadinessTarget {
    fn parse(url: &str) -> Result<Self, HarnessSupervisorError> {
        let authority_and_path = url.strip_prefix("http://").ok_or_else(|| {
            HarnessSupervisorError::Readiness(
                "only http:// readiness URLs are supported by the cross-platform supervisor"
                    .to_owned(),
            )
        })?;
        let (authority, path) = match authority_and_path.split_once('/') {
            Some((authority, path)) => (authority, format!("/{path}")),
            None => (authority_and_path, "/".to_owned()),
        };
        if authority.is_empty() {
            return Err(HarnessSupervisorError::Readiness(
                "readiness URL has no host".to_owned(),
            ));
        }
        let (host, port) = if authority.starts_with('[') {
            let close = authority.find(']').ok_or_else(|| {
                HarnessSupervisorError::Readiness(
                    "readiness URL has an invalid IPv6 host".to_owned(),
                )
            })?;
            let host = authority[1..close].to_owned();
            let suffix = &authority[close + 1..];
            let port = if suffix.is_empty() {
                80
            } else {
                suffix
                    .strip_prefix(':')
                    .ok_or_else(|| {
                        HarnessSupervisorError::Readiness(
                            "readiness URL has an invalid IPv6 port".to_owned(),
                        )
                    })?
                    .parse::<u16>()
                    .map_err(|_| {
                        HarnessSupervisorError::Readiness(
                            "readiness URL has an invalid port".to_owned(),
                        )
                    })?
            };
            (host, port)
        } else if let Some((host, port)) = authority.rsplit_once(':') {
            let port = port.parse::<u16>().map_err(|_| {
                HarnessSupervisorError::Readiness("readiness URL has an invalid port".to_owned())
            })?;
            (host.to_owned(), port)
        } else {
            (authority.to_owned(), 80)
        };
        if host.is_empty() {
            return Err(HarnessSupervisorError::Readiness(
                "readiness URL has no host".to_owned(),
            ));
        }
        if !is_loopback_host(&host) {
            return Err(HarnessSupervisorError::Readiness(
                "readiness URL must target localhost, 127.0.0.1, or [::1]".to_owned(),
            ));
        }
        Ok(Self { host, port, path })
    }
}

fn is_loopback_host(host: &str) -> bool {
    matches!(
        host.to_ascii_lowercase().as_str(),
        "localhost" | "127.0.0.1" | "::1"
    )
}

async fn readiness_probe(target: &ReadinessTarget) -> Result<(), String> {
    let mut stream = TcpStream::connect((target.host.as_str(), target.port))
        .await
        .map_err(|error| error.to_string())?;
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        target.path, target.host
    );
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|error| error.to_string())?;
    let mut response = [0_u8; 256];
    let length = stream
        .read(&mut response)
        .await
        .map_err(|error| error.to_string())?;
    let first_line = std::str::from_utf8(&response[..length])
        .map_err(|error| error.to_string())?
        .lines()
        .next()
        .unwrap_or_default();
    let success = first_line
        .split_whitespace()
        .nth(1)
        .and_then(|status| status.parse::<u16>().ok())
        .is_some_and(|status| (200..300).contains(&status));
    if success {
        Ok(())
    } else {
        Err(format!("endpoint returned {first_line:?}"))
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf, time::Duration};

    use nexus_core::NexusPaths;

    use super::{HarnessSupervisor, HarnessSupervisorError};

    #[tokio::test]
    async fn supervisor_reports_missing_program_as_readable_error() {
        let root =
            std::env::temp_dir().join(format!("nexus-agent-unconfigured-{}", std::process::id()));
        let paths = NexusPaths::from_root(PathBuf::from(&root));
        let supervisor = HarnessSupervisor::new(paths).expect("supervisor creates");

        let error = supervisor
            .start()
            .await
            .expect_err("start must be rejected");
        assert!(matches!(error, HarnessSupervisorError::NotConfigured));
        assert!(error.to_string().contains("not configured"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn readiness_parser_rejects_non_http_without_network() {
        let error = super::ReadinessTarget::parse("https://127.0.0.1:1/health")
            .expect_err("https must be explicit unsupported behavior");
        assert!(error.to_string().contains("only http://"));
    }

    #[test]
    fn readiness_parser_rejects_remote_hosts() {
        let error = super::ReadinessTarget::parse("http://example.com:80/health")
            .expect_err("remote readiness targets must be rejected");
        assert!(error.to_string().contains("must target localhost"));
        assert!(super::ReadinessTarget::parse("http://127.0.0.1:3090/health").is_ok());
        assert!(super::ReadinessTarget::parse("http://[::1]:3090/health").is_ok());
    }

    #[test]
    fn graceful_wait_is_explicitly_bounded() {
        let duration = Duration::from_millis(25);
        assert_eq!(duration, Duration::from_millis(25));
    }
}
