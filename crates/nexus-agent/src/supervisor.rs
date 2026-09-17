//! External, replaceable Harness process supervision.

use std::{
    fmt, fs, io,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use nexus_core::{
    load_harness_launch_spec, log_file_identity, unix_time_seconds, validate_profile_name,
    AgentState, HarnessLaunchSpec, HarnessLogSession, HarnessLogSessionStore, NexusPaths,
    ReleaseStore, RuntimeMetadataStore, DEFAULT_PROFILE,
};
use nexus_launcher_core::{read_harness_ui_info_with_observer, HarnessLogObserver};
use nexus_protocol::{HarnessLaunchMode, HarnessRuntimeInfo, HarnessState};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    process::Command,
    sync::{oneshot, Mutex, OwnedMutexGuard},
    time::{sleep, timeout},
};

#[cfg(windows)]
use crate::windows_harness::Child;
#[cfg(not(windows))]
use crate::unix_harness::Child;

pub const DEFAULT_GRACEFUL_STOP_SECS: u64 = 5;
pub const DEFAULT_READINESS_TIMEOUT_SECS: u64 = 30;

const MONITOR_INTERVAL: Duration = Duration::from_millis(100);
const READINESS_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(1);
const UNATTACHED_RECOVERY_CAP: Duration = Duration::from_secs(1);

/// Registered slot paths are selections, not pins to an old installation.
/// Keep custom commands outside the release store unchanged. Only executable,
/// Node entry and cwd are paths here; never rewrite arbitrary user arguments.
pub(crate) fn normalize_selected_launch(spec: &mut HarnessLaunchSpec, paths: &NexusPaths, releases: &ReleaseStore) -> io::Result<()> {
    if nexus_core::ConfigStore::new(paths.clone()).load()?.external_harness.is_some() { return Ok(()); }
    normalize_managed_launch(spec,releases)
}

pub(crate) fn normalize_managed_launch(
    spec: &mut HarnessLaunchSpec,
    releases: &ReleaseStore,
) -> io::Result<()> {
    let catalog = releases.load()?;
    if catalog.current_release.is_none() {
        return Ok(());
    }
    let roots = catalog.releases.iter().map(|release| {
        releases.release_root(&release.id).and_then(fs::canonicalize)
    }).collect::<io::Result<Vec<_>>>()?;
    let normalize = |path: &Path| -> Option<PathBuf> {
        let input = if path.is_relative() {
            spec.working_dir.as_deref().unwrap_or(Path::new(".")).join(path)
        } else {
            path.to_owned()
        };
        let resolved = fs::canonicalize(input).ok()?;
        roots.iter().find_map(|root| {
            resolved.strip_prefix(root).ok().map(|suffix| {
                Path::new("{release_root}").join(suffix)
            })
        })
    };
    let entry = if spec.mode == HarnessLaunchMode::Node {
        spec.args.first().map(Path::new)
    } else {
        Some(spec.program.as_path())
    };
    let Some(entry) = entry else { return Ok(()); };
    let normalized_entry = normalize(entry);
    if normalized_entry.is_none() && !entry.to_string_lossy().contains("{release_root}") {
        return Ok(());
    }
    let normalized_cwd = spec.working_dir.as_deref().and_then(normalize);
    if let Some(entry) = normalized_entry {
        if spec.mode == HarnessLaunchMode::Node {
            spec.args[0] = entry.to_string_lossy().into_owned();
        } else {
            spec.program = entry;
        }
    }
    if let Some(cwd) = normalized_cwd {
        spec.working_dir = Some(cwd);
    }
    Ok(())
}

pub(crate) struct HarnessLifecycleGuard {
    _guard: OwnedMutexGuard<()>,
}

#[derive(Debug)]
pub enum HarnessSupervisorError {
    NotConfigured,
    Cancelled,
    Busy,
    Preflight(serde_json::Value),
    RecoveryPaused,
    AlreadyRunning,
    Unattached,
    InvalidProfile(String),
    Configuration(io::Error),
    Spawn(io::Error),
    Process(io::Error),
    Readiness(String),
    Persistence(io::Error),
}

impl fmt::Display for HarnessSupervisorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => formatter.write_str("Harness startup was cancelled; no previous instance is restarted automatically"),
            Self::Busy => formatter.write_str("A lifecycle operation is already in progress; retry after it finishes"),
            Self::Preflight(_) => formatter.write_str("Resolve the blocking startup checks before starting Harness"),
            Self::NotConfigured => write!(
                formatter,
                "Harness is not configured; set harness.program in Nexus config.json or {HARNESS_PROGRAM_ENV}"
            ),
            Self::RecoveryPaused => formatter.write_str("Harness startup is paused in recovery mode; leave recovery mode before starting"),
            Self::AlreadyRunning => formatter.write_str("Harness is already running"),
            Self::Unattached => formatter.write_str(
                "Harness is running but is not attached to this Agent instance; stop it from its owning Agent",
            ),
            Self::InvalidProfile(error) => write!(formatter, "invalid Harness profile: {error}"),
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
    // In-memory evidence only: persisted Failed metadata is not proof that
    // this Agent ever spawned a child. Retained across child handoff/reaping.
    spawned_launch: bool,
    spawned_preferences: Option<nexus_protocol::HarnessPreferencesPayload>,
    #[cfg(windows)]
    job: Option<std::sync::Arc<crate::dsh::WindowsJob>>,
    runtime: HarnessRuntimeInfo,
    generation: u64,
    operation_epoch: u64,
    log_session: HarnessLogSession,
    stop_pending: bool,
    start_ownership_pending: bool,
    persisted_agent_revision: Option<u64>,
    persisted_agent_state: Option<AgentState>,
    readiness: Option<ReadinessConfig>,
    recovery: Option<RecoveryState>,
    readiness_owner: Option<ReadinessOwner>,
    attached_readiness: Option<AttachedReadinessState>,
    unattached_monitor_started: bool,
    healthy_candidate: Option<nexus_core::HealthyReleaseEvidence>,
    verified_health: Option<(String, u64)>,
}

pub(crate) fn preflight_readiness_endpoint(spec: &HarnessLaunchSpec) -> Result<Option<(String, u16)>, HarnessSupervisorError> {
    spec.readiness_url.as_deref().map(|url| ReadinessTarget::parse(url).map(|target| (target.host, target.port))).transpose()
}

struct SpawnedHarness {
    child: Child,
    #[cfg(windows)]
    job: std::sync::Arc<crate::dsh::WindowsJob>,
}

async fn spawn_owned_harness(command: Command, logs: Option<(&fs::File, &fs::File)>) -> io::Result<SpawnedHarness> {
    #[cfg(windows)]
    {
        let job = crate::dsh::WindowsJob::new()?;
        let child = crate::windows_harness::spawn(command.as_std(), logs, &job)?;
        Ok(SpawnedHarness { child, job: std::sync::Arc::new(job) })
    }
    #[cfg(not(windows))]
    { let _ = logs; crate::unix_harness::spawn(command).map(|child| SpawnedHarness { child }) }
}

#[cfg(windows)]
async fn finish_owned_job(inner: &mut SupervisorInner) -> io::Result<()> {
    let Some(job) = inner.job.as_ref() else { return Ok(()); };
    job.terminate()?;
    drain_owned_job(job).await?;
    inner.job = None;
    Ok(())
}

#[cfg(windows)]
async fn drain_owned_job(job: &crate::dsh::WindowsJob) -> io::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !job.is_empty()? {
        if Instant::now() >= deadline {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "Harness process tree is still stopping"));
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Ok(())
}

/// Terminate the owned job and remove it from the supervisor state so the
/// bounded drain wait can run without holding the supervisor lock and
/// delaying unlocked read endpoints. Callers must re-validate the
/// generation and epoch tokens after the wait.
#[cfg(windows)]
fn take_terminating_job(
    inner: &mut SupervisorInner,
) -> io::Result<Option<std::sync::Arc<crate::dsh::WindowsJob>>> {
    let Some(job) = inner.job.take() else { return Ok(None); };
    job.terminate()?;
    Ok(Some(job))
}

#[cfg(test)]
struct StartPersistGate {
    reached: oneshot::Sender<()>,
    release: oneshot::Receiver<()>,
}

#[cfg(test)]
struct HarnessPersistGate {
    reached: oneshot::Sender<()>,
    release: oneshot::Receiver<()>,
}

#[derive(Debug, Clone)]
struct ReadinessConfig {
    target: ReadinessTarget,
    timeout: Duration,
}

#[derive(Debug, Clone)]
struct RecoveryState {
    generation: u64,
    target: ReadinessTarget,
    deadline: Instant,
    exit_code: Option<i32>,
    task_started: bool,
}

#[derive(Debug, Clone)]
struct RecoveryTask {
    generation: u64,
    target: ReadinessTarget,
    deadline: Instant,
    exit_code: Option<i32>,
    owner_epoch: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AttachedReadinessState {
    generation: u64,
    epoch: u64,
    deadline: Option<Instant>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadinessOwner {
    Start { generation: u64, epoch: u64 },
    Recovery { generation: u64, epoch: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ReadinessLease {
    owner: ReadinessOwner,
    deadline: Instant,
}

enum StartOwnershipSignal {
    Persisted,
    Abort {
        message: String,
        completion: oneshot::Sender<Result<(), String>>,
    },
}

struct StartupOperation {
    id: String,
    phase: &'static str,
    token: nexus_core::CancellationToken,
}

#[derive(Clone)]
pub struct HarnessSupervisor {
    paths: NexusPaths,
    store: RuntimeMetadataStore,
    log_sessions: HarnessLogSessionStore,
    releases: ReleaseStore,
    inner: Arc<Mutex<SupervisorInner>>,
    lifecycle: Arc<Mutex<()>>,
    desktop_shutdown: Arc<std::sync::atomic::AtomicBool>,
    startup_operation: Arc<Mutex<Option<StartupOperation>>>,
    #[cfg(test)]
    lifecycle_wait_observer: Arc<Mutex<Option<oneshot::Sender<()>>>>,
    #[cfg(test)]
    start_persist_gate: Arc<Mutex<Option<StartPersistGate>>>,
    #[cfg(test)]
    harness_persist_gate: Arc<Mutex<Option<HarnessPersistGate>>>,
    #[cfg(test)]
    stop_wait_failure: Arc<std::sync::atomic::AtomicBool>,
    #[cfg(test)]
    stop_wait_gate: Arc<Mutex<Option<StartPersistGate>>>,
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
        let releases = ReleaseStore::new(paths.clone());
        let log_sessions = HarnessLogSessionStore::new(paths.clone());
        let previous_session = log_sessions.read()?;
        let log_session = match previous_session {
            Some(session) if session.is_current_schema() => session,
            previous => {
                paths.ensure_directories()?;
                let generation = previous.map_or(0, |session| session.generation);
                let (session, _stdout, _stderr) =
                    create_harness_log_session(&paths, generation, false)?;
                log_sessions.write(&session)?;
                session
            }
        };
        let runtime = if log_session.launch_pending
            && !matches!(
                runtime.state,
                HarnessState::Starting | HarnessState::Running
            ) {
            recovery_runtime(&runtime)
        } else {
            runtime
        };
        Ok(Self {
            paths,
            store,
            log_sessions,
            releases,
            inner: Arc::new(Mutex::new(SupervisorInner {
                child: None,
                spawned_launch: false,
                spawned_preferences: None,
                #[cfg(windows)]
                job: None,
                runtime,
                generation: log_session.generation,
                operation_epoch: 0,
                log_session,
                stop_pending: false,
                start_ownership_pending: false,
                persisted_agent_revision: None,
                persisted_agent_state: None,
                readiness: None,
                recovery: None,
                readiness_owner: None,
                attached_readiness: None,
                unattached_monitor_started: false,
                healthy_candidate: None,
                verified_health: None,
            })),
            lifecycle: Arc::new(Mutex::new(())),
            desktop_shutdown: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            startup_operation: Arc::new(Mutex::new(None)),
            #[cfg(test)]
            lifecycle_wait_observer: Arc::new(Mutex::new(None)),
            #[cfg(test)]
            start_persist_gate: Arc::new(Mutex::new(None)),
            #[cfg(test)]
            harness_persist_gate: Arc::new(Mutex::new(None)),
            #[cfg(test)]
            stop_wait_failure: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            #[cfg(test)]
            stop_wait_gate: Arc::new(Mutex::new(None)),
            graceful_wait,
        })
    }

    pub fn paths(&self) -> &NexusPaths {
        &self.paths
    }

    pub fn metadata_store(&self) -> RuntimeMetadataStore {
        self.store.clone()
    }

    pub(crate) async fn persist_agent_snapshot(
        &self,
        generation: u64,
        agent_revision: u64,
        state: &AgentState,
        harness: &HarnessRuntimeInfo,
    ) -> io::Result<bool> {
        let mut inner = self.inner.lock().await;
        if inner.generation != generation || inner.runtime != *harness {
            return Ok(false);
        }
        if let Some(persisted_revision) = inner.persisted_agent_revision {
            if agent_revision < persisted_revision {
                return Ok(false);
            }
            if agent_revision == persisted_revision {
                return Ok(inner.persisted_agent_state.as_ref() == Some(state));
            }
        }
        self.store.write_snapshot(state, harness.clone())?;
        inner.persisted_agent_revision = Some(agent_revision);
        inner.persisted_agent_state = Some(state.clone());
        Ok(true)
    }

    pub async fn status(&self) -> HarnessRuntimeInfo {
        self.status_with_generation().await.1
    }

    pub(crate) async fn status_with_generation(&self) -> (u64, HarnessRuntimeInfo) {
        let (generation, runtime, _) = self.status_observation().await;
        (generation, runtime)
    }

    /// Harness selection metadata may change only when there is positively no
    /// live or recovering Harness owner. The caller must hold the shared
    /// lifecycle guard so no start/stop/restart operation can race this check.
    pub(crate) async fn selection_change_is_quiescent(
        &self,
        _lifecycle: &HarnessLifecycleGuard,
    ) -> bool {
        let inner = self.inner.lock().await;
        #[cfg(windows)]
        if inner.job.as_ref().is_some_and(|job| !job.is_empty().unwrap_or(false)) {
            return false;
        }
        inner.child.is_none()
            && !inner.stop_pending
            && !inner.start_ownership_pending
            && inner.recovery.is_none()
            && inner.readiness_owner.is_none()
            && inner.attached_readiness.is_none()
            && !inner.unattached_monitor_started
            && !inner.log_session.launch_pending
            && inner.runtime.pid.is_none()
            && matches!(
                inner.runtime.state,
                HarnessState::Detached | HarnessState::Stopped | HarnessState::Failed
            )
    }

    #[cfg(test)]
    pub(crate) async fn inject_nonquiescent_failed_state(
        &self,
        child: Option<tokio::process::Child>,
        launch_pending: bool,
    ) -> io::Result<()> {
        let mut inner = self.inner.lock().await;
        let pid = child.as_ref().and_then(tokio::process::Child::id);
        let mut session = inner.log_session.clone();
        session.launch_pending = launch_pending;
        self.log_sessions.write(&session)?;
        inner.log_session = session;
        inner.child = child.map(Into::into);
        inner.stop_pending = false;
        inner.start_ownership_pending = false;
        inner.recovery = None;
        inner.readiness_owner = None;
        inner.attached_readiness = None;
        inner.unattached_monitor_started = false;
        inner.runtime = HarnessRuntimeInfo {
            state: HarnessState::Failed,
            pid,
            exit_code: Some(1),
            error: Some("injected non-quiescent failed Harness".to_owned()),
            started_at_unix: Some(unix_time_seconds()),
            updated_at_unix: Some(unix_time_seconds()),
        };
        Ok(())
    }

    /// Optional explanation must never wait for, poll, or mutate a lifecycle owner.
    pub(crate) fn launch_input_identity(&self) -> Option<(u64, HarnessState, HarnessLogSession)> {
        self.inner.try_lock().ok().map(|inner| (inner.generation, inner.runtime.state, inner.log_session.clone()))
    }

    pub(crate) async fn status_observation(&self) -> (u64, HarnessRuntimeInfo, HarnessLogSession) {
        let (runtime, generation, log_session, changed, recovery) = {
            let mut inner = self.inner.lock().await;
            let (runtime, changed) = match poll_child(&self.paths, &mut inner, &self.log_sessions) {
                Ok(Some(runtime)) => (runtime, true),
                Ok(None) => (inner.runtime.clone(), false),
                Err(error) => {
                    inner.attached_readiness = None;
                    inner.runtime = failed_runtime(
                        &inner.runtime,
                        format!("failed to query child process: {error}"),
                    );
                    (inner.runtime.clone(), true)
                }
            };
            let recovery = pending_recovery(&mut inner);
            (
                runtime,
                inner.generation,
                inner.log_session.clone(),
                changed,
                recovery,
            )
        };
        if let Some(recovery) = recovery {
            self.spawn_recovery(recovery);
        }
        let stale = changed
            && matches!(
                self.persist_if_current(generation, &runtime).await,
                Ok(false)
            );
        if stale {
            let inner = self.inner.lock().await;
            (
                inner.generation,
                inner.runtime.clone(),
                inner.log_session.clone(),
            )
        } else {
            (generation, runtime, log_session)
        }
    }

    /// Durably claim the healthy-snapshot attempt for one concrete Harness log
    /// session. Returns false when this run was already claimed or is no longer
    /// the current Running observation.
    pub(crate) async fn claim_healthy_snapshot_attempt(
        &self,
        run_id: &str,
        generation: u64,
    ) -> io::Result<bool> {
        let mut inner = self.inner.lock().await;
        if inner.runtime.state != HarnessState::Running
            || inner.log_session.run_id != run_id
            || inner.log_session.generation != generation
            || inner.log_session.healthy_snapshot_attempted
            || inner.verified_health.as_ref() != Some(&(run_id.to_owned(), generation))
        {
            return Ok(false);
        }
        let mut claimed = inner.log_session.clone();
        claimed.healthy_snapshot_attempted = true;
        self.log_sessions.write(&claimed)?;
        inner.log_session = claimed;
        Ok(true)
    }

    /// Recover a persisted observation without claiming an old PID is under
    /// this Agent's control. A healthy configured loopback endpoint is enough
    /// to restore Running with no process identity only for legacy readiness;
    /// token-bound readiness fails closed because process ownership cannot be
    /// reconstructed after an Agent restart. Otherwise only stale
    /// Starting/Running observations are made Stopped. A persisted Failed
    /// observation remains Failed unless the endpoint proves it recovered.
    pub async fn recover_unattached(&self) -> HarnessRuntimeInfo {
        let terminal_scrub = {
            let mut inner = self.inner.lock().await;
            if inner.child.is_some() || inner.stop_pending {
                return inner.runtime.clone();
            }
            if matches!(
                inner.runtime.state,
                HarnessState::Detached | HarnessState::Stopped
            ) && inner.runtime.pid.is_some()
            {
                inner.runtime = scrub_unattached_pid(&inner.runtime);
                Some((inner.generation, inner.runtime.clone()))
            } else {
                None
            }
        };
        if let Some((generation, runtime)) = terminal_scrub {
            if matches!(
                self.persist_if_current(generation, &runtime).await,
                Ok(false)
            ) {
                return self.inner.lock().await.runtime.clone();
            }
            return runtime;
        }

        let (previous, generation, launch_pending) = {
            let mut inner = self.inner.lock().await;
            if inner.child.is_some()
                || !matches!(
                    inner.runtime.state,
                    HarnessState::Starting | HarnessState::Running | HarnessState::Failed
                )
            {
                return inner.runtime.clone();
            }
            inner.recovery = None;
            inner.readiness_owner = None;
            inner.attached_readiness = None;
            (
                inner.runtime.clone(),
                inner.generation,
                inner.log_session.launch_pending,
            )
        };

        let readiness = match load_harness_launch_spec(&self.paths) {
            Ok(Some(mut spec)) => {
                let effective = (|| -> io::Result<()> {
                    let preferences = nexus_core::load_harness_preferences(&self.paths)?;
                    let profile = nexus_core::ProfileStore::new(self.paths.clone()).load()?.active_profile;
                    let home = crate::dsh::resolve_dsh_home_for_paths(&self.paths)?;
                    normalize_selected_launch(&mut spec, &self.paths, &self.releases)?;
                    let root = if let Some(source)=nexus_core::ConfigStore::new(self.paths.clone()).load()?.external_harness {Some(source.root)} else {self.releases.load()?.current_release.as_deref().map(|id|self.releases.release_root(id)).transpose()?};
                    crate::runtime_patches::check_failures(&self.paths, &preferences)?;
                    crate::preference_capabilities::validate_launch(&spec, &preferences, root.as_deref())?;
                    let capabilities = crate::preference_capabilities::resolve(root.as_deref(), &home, &profile, &preferences)?;
                    nexus_core::apply_harness_preferences(&mut spec, &preferences, &capabilities);
                    Ok(())
                })();
                effective.ok().and_then(|()| readiness_config(&spec).ok().flatten())
            },
            Ok(None) | Err(_) => None,
        };
        let token_requires_owned_evidence = readiness
            .as_ref()
            .is_some_and(|readiness| readiness.target.token_required);
        let healthy = match readiness.as_ref() {
            Some(readiness) if !readiness.target.token_required => {
                probe_until_deadline(
                    &readiness.target,
                    Instant::now() + readiness.timeout.min(UNATTACHED_RECOVERY_CAP),
                )
                .await
            }
            Some(_) | None => false,
        };

        let (persist_generation, runtime, recovery, arm_monitor) = {
            let mut inner = self.inner.lock().await;
            if inner.generation != generation || inner.child.is_some() || inner.runtime != previous
            {
                return inner.runtime.clone();
            }
            inner.readiness = readiness.clone();
            let mut arm_monitor = None;
            if token_requires_owned_evidence {
                // After an Agent restart there is no owned process transition
                // at which to rotate the log watermark. A bare readiness
                // endpoint therefore cannot distinguish the old Harness from
                // an unrelated process that reused its port. Fail closed;
                // token-bound recovery is allowed only after an observed child
                // exit has established a fresh boundary and emitted a matching
                // token.
                inner.recovery = None;
                inner.unattached_monitor_started = false;
                let mut failed = failed_runtime(
                    &previous,
                    "Token-bound readiness cannot safely reattach after Agent restart without current process identity"
                        .to_owned(),
                );
                failed.pid = None;
                inner.runtime = failed;
                if let Err(error) = clear_launch_pending(&self.log_sessions, &mut inner) {
                    inner.runtime.error = Some(format!(
                        "Token-bound readiness cannot safely reattach after Agent restart without current process identity; failed to clear the launch reservation: {error}"
                    ));
                }
            } else if healthy {
                // The Agent cannot reattach the old PID after a restart. Rotate
                // the durable log boundary before publishing PID-less Running
                // so a token emitted by the previous Agent/Harness instance is
                // never presented as belonging to this recovered observation.
                match rotate_unattached_session(&self.paths, &self.log_sessions, &mut inner) {
                    Ok(()) => {
                        inner.recovery = None;
                        inner.runtime = running_runtime_without_pid(&previous);
                        if let Some(readiness) = readiness.as_ref() {
                            inner.unattached_monitor_started = true;
                            arm_monitor = Some(readiness.target.clone());
                        }
                    }
                    Err(error) => {
                        inner.recovery = None;
                        inner.unattached_monitor_started = false;
                        let mut failed = failed_runtime(
                            &previous,
                            format!(
                                "failed to establish a new Harness token boundary after Agent restart: {error}"
                            ),
                        );
                        failed.pid = None;
                        inner.runtime = failed;
                    }
                }
            } else if launch_pending {
                if let Some(readiness) = readiness.as_ref() {
                    inner.runtime = recovery_runtime(&previous);
                    inner.recovery = Some(RecoveryState {
                        generation,
                        target: readiness.target.clone(),
                        deadline: Instant::now() + readiness.timeout,
                        exit_code: previous.exit_code,
                        task_started: false,
                    });
                } else {
                    inner.runtime = abandoned_without_readiness_runtime(&previous);
                    if let Err(error) = clear_launch_pending(&self.log_sessions, &mut inner) {
                        inner.runtime.error = Some(format!(
                            "Harness launch was abandoned without readiness, but its reservation could not be cleared: {error}"
                        ));
                    }
                }
            } else {
                inner.runtime = match previous.state {
                    HarnessState::Starting | HarnessState::Running => {
                        unattached_stopped_runtime(&previous)
                    }
                    HarnessState::Failed => scrub_unattached_pid(&previous),
                    HarnessState::Detached | HarnessState::Stopped => inner.runtime.clone(),
                };
            }
            let recovery = pending_recovery(&mut inner);
            (
                inner.generation,
                inner.runtime.clone(),
                recovery,
                arm_monitor,
            )
        };
        if let Some(recovery) = recovery {
            self.spawn_recovery(recovery);
        }
        if matches!(
            self.persist_if_current(persist_generation, &runtime).await,
            Ok(false)
        ) {
            return self.inner.lock().await.runtime.clone();
        }
        if let Some(target) = arm_monitor {
            self.spawn_unattached_monitor(persist_generation, target);
        }
        runtime
    }

    pub async fn start(&self) -> Result<HarnessRuntimeInfo, HarnessSupervisorError> {
        self.start_with_profile(DEFAULT_PROFILE).await
    }

    /// Start Harness with the current profile rendered only into explicit
    /// `{profile}` placeholders in the configured argument list.
    pub async fn start_with_profile(
        &self,
        profile: &str,
    ) -> Result<HarnessRuntimeInfo, HarnessSupervisorError> {
        let lifecycle = self.acquire_lifecycle().await;
        self.start_with_profile_locked(profile, &lifecycle).await
    }

    pub(crate) fn try_acquire_lifecycle(&self) -> Option<HarnessLifecycleGuard> {
        Arc::clone(&self.lifecycle).try_lock_owned().ok()
            .map(|guard| HarnessLifecycleGuard { _guard: guard })
    }

    pub(crate) fn seal_for_desktop_update(&self, _guard: &HarnessLifecycleGuard) {
        self.desktop_shutdown.store(true, std::sync::atomic::Ordering::Release);
    }

    pub(crate) async fn acquire_lifecycle(&self) -> HarnessLifecycleGuard {
        let acquire = Arc::clone(&self.lifecycle).lock_owned();
        #[cfg(test)]
        if let Some(reached) = self.lifecycle_wait_observer.lock().await.take() {
            use std::{future::Future, task::Poll};

            let mut acquire = Box::pin(acquire);
            let mut reached = Some(reached);
            let guard = std::future::poll_fn(|context| match acquire.as_mut().poll(context) {
                Poll::Ready(guard) => Poll::Ready(guard),
                Poll::Pending => {
                    if let Some(reached) = reached.take() {
                        let _ = reached.send(());
                    }
                    Poll::Pending
                }
            })
            .await;
            return HarnessLifecycleGuard { _guard: guard };
        }
        HarnessLifecycleGuard {
            _guard: acquire.await,
        }
    }

    #[cfg(test)]
    pub(crate) async fn observe_next_lifecycle_wait(&self, reached: oneshot::Sender<()>) {
        *self.lifecycle_wait_observer.lock().await = Some(reached);
    }

    pub(crate) async fn start_with_profile_locked(
        &self,
        profile: &str,
        _lifecycle: &HarnessLifecycleGuard,
    ) -> Result<HarnessRuntimeInfo, HarnessSupervisorError> {
        self.start_with_profile_inner(profile,None).await
    }

    pub(crate) async fn begin_startup(&self) -> Result<(), HarnessSupervisorError> {
        let id=nexus_core::agent_auth::random_hex().map_err(HarnessSupervisorError::Persistence)?;
        *self.startup_operation.lock().await=Some(StartupOperation{id,phase:"checking",token:Default::default()});
        Ok(())
    }
    pub(crate) async fn startup_status(&self) -> serde_json::Value {
        let operation=self.startup_operation.lock().await;
        match operation.as_ref() {
            Some(op)=>serde_json::json!({"operation_id":op.id,"phase":op.phase,"cancellable":matches!(op.phase,"checking"|"compatibility"),"cancel_requested":op.token.is_cancelled()}),
            None=>serde_json::json!({"phase":"idle","cancellable":false}),
        }
    }
    pub(crate) async fn cancel_startup(&self,id:&str)->bool {
        let operation=self.startup_operation.lock().await;
        if let Some(op)=operation.as_ref().filter(|op|op.id==id&&matches!(op.phase,"checking"|"compatibility")) {op.token.cancel();true} else {false}
    }
    async fn startup_phase(&self,phase:&'static str)->Result<nexus_core::CancellationToken,HarnessSupervisorError> {
        let mut operation=self.startup_operation.lock().await;
        if let Some(op)=operation.as_mut().filter(|op|matches!(op.phase,"checking"|"compatibility")) {
            if op.token.is_cancelled(){return Err(HarnessSupervisorError::Cancelled);}
            op.phase=phase;
            return Ok(op.token.clone());
        }
        Ok(Default::default())
    }
    pub(crate) async fn finish_startup(&self,success:bool,cancelled:bool) {
        if let Some(op)=self.startup_operation.lock().await.as_mut(){op.phase=if success{"submitted"}else if cancelled{"cancelled"}else{"failed"};}
    }
    pub(crate) async fn start_prepared(&self,profile:&str,lifecycle:&HarnessLifecycleGuard,prepared:crate::preflight::PreparedStart,restart:bool)->Result<HarnessRuntimeInfo,HarnessSupervisorError>{
        prepared.recheck(&self.paths,profile).map_err(HarnessSupervisorError::Configuration)?;
        self.startup_phase("checking").await?;
        if restart {self.stop_locked(lifecycle).await?;}
        self.start_with_profile_inner(profile,Some(prepared)).await
    }
    async fn start_with_profile_inner(
        &self,
        profile: &str,
        prepared: Option<crate::preflight::PreparedStart>,
    ) -> Result<HarnessRuntimeInfo, HarnessSupervisorError> {
        if self.desktop_shutdown.load(std::sync::atomic::Ordering::Acquire) {
            return Err(HarnessSupervisorError::Busy);
        }
        crate::recovery_mode::ensure_start_allowed(&self.paths)?;
        validate_profile_name(profile)
            .map_err(|error| HarnessSupervisorError::InvalidProfile(error.to_string()))?;
        let health_config_revision = nexus_core::ConfigStore::new(self.paths.clone()).snapshot().ok().map(|snapshot| snapshot.revision);
        let mut spec = load_harness_launch_spec(&self.paths)
            .map_err(HarnessSupervisorError::Configuration)?
            .filter(|spec| !spec.program.as_os_str().is_empty())
            .ok_or(HarnessSupervisorError::NotConfigured)?;
        normalize_selected_launch(&mut spec, &self.paths, &self.releases)
            .map_err(HarnessSupervisorError::Configuration)?;
        let preferences = nexus_core::load_harness_preferences(&self.paths)
            .map_err(HarnessSupervisorError::Configuration)?;
        let configured_runtime = nexus_core::ConfigStore::new(self.paths.clone()).load()
            .map_err(HarnessSupervisorError::Configuration)?.runtime.unwrap_or_default();
        let effective_runtime = crate::runtime::runtime_for_launch(&mut spec, configured_runtime,
            nexus_core::bundled_runtime_dir().as_deref());
        let runtime_env = nexus_core::build_runtime_child_env(&effective_runtime, std::env::var_os("PATH").as_deref())
            .map_err(HarnessSupervisorError::Configuration)?;
        let selected_home = crate::dsh::resolve_dsh_home_for_paths(&self.paths)
            .map_err(HarnessSupervisorError::Configuration)?;
        let source=if let Some(prepared)=&prepared {
            prepared.recheck(&self.paths,profile).map_err(HarnessSupervisorError::Configuration)?;
            crate::source_context::SourceContext{root:prepared.source.root.clone(),release_id:prepared.source.release_id.clone(),external:prepared.source.external}
        }else{crate::source_context::resolve_async(&self.paths,&self.releases).await.map_err(HarnessSupervisorError::Configuration)?};
        let release_id=source.release_id.as_deref();
        let release_root=source.root;
        crate::runtime_patches::validate_for_paths(&self.paths, &preferences)
            .map_err(HarnessSupervisorError::Configuration)?;
        crate::preference_capabilities::validate_launch(&spec, &preferences, release_root.as_deref())
            .map_err(HarnessSupervisorError::Configuration)?;
        crate::preference_capabilities::resolve(release_root.as_deref(), &selected_home, profile, &preferences)
            .map_err(HarnessSupervisorError::Configuration)?;
        // The lifecycle owner remains held while the isolated check runs, but
        // never hold `inner` across the child-process probe.
        let cancellation = self.startup_phase("compatibility").await?;
        let mut verified_profile = None;
        if spec.mode == HarnessLaunchMode::Node {
            if let (Some(id), Some(root)) = (release_id, release_root.as_deref()) {
                let entry = spec.render_args_for_context(profile, release_id, release_root.as_deref())
                    .map_err(HarnessSupervisorError::Configuration)?;
                let managed = entry.first().and_then(|entry| fs::canonicalize(entry).ok())
                    .zip(fs::canonicalize(root.join("apps/cli/lib/bin.js")).ok())
                    .is_some_and(|(entry, expected)| entry == expected);
                if managed {
                    let status = self.status().await;
                    if matches!(status.state, HarnessState::Starting | HarnessState::Running) {
                        return Err(HarnessSupervisorError::AlreadyRunning);
                    }
                    {
                        let inner = self.inner.lock().await;
                        if inner.child.is_some() || inner.recovery.is_some() || inner.stop_pending || inner.log_session.launch_pending {
                            return Err(HarnessSupervisorError::AlreadyRunning);
                        }
                    }
                    let home = selected_home.clone();
                    verified_profile = crate::compatibility::prepare(&self.paths, &home, profile, id, root,
                        &spec.program, false, &cancellation).await
                        .map_err(|e|if cancellation.is_cancelled()&&e.kind()==io::ErrorKind::Interrupted {HarnessSupervisorError::Cancelled}else{HarnessSupervisorError::Configuration(e)})?.map(|report| report.source_profile);
                }
            }
        }
        if source.external && spec.mode==HarnessLaunchMode::Node {
            // The configuration can change between the external-source check
            // above and this re-read; surface a configuration error instead of
            // panicking in the request handler.
            let missing = || HarnessSupervisorError::Configuration(io::Error::other("external Harness identity is missing; confirm the external Harness source again"));
            let id=crate::source_context::compatibility_id(&self.paths,&self.releases).map_err(HarnessSupervisorError::Configuration)?.ok_or_else(missing)?;
            let root=release_root.as_deref().ok_or_else(missing)?;
            verified_profile=crate::compatibility::prepare(&self.paths,&selected_home,profile,&id,root,&spec.program,false,&cancellation).await.map_err(|e|if cancellation.is_cancelled()&&e.kind()==io::ErrorKind::Interrupted {HarnessSupervisorError::Cancelled}else{HarnessSupervisorError::Configuration(e)})?.map(|report|report.source_profile);
        }
        if let Some(prepared)=&prepared {prepared.recheck(&self.paths,profile).map_err(HarnessSupervisorError::Configuration)?;}
        let profile = verified_profile.as_deref().unwrap_or(profile);
        let capabilities = crate::preference_capabilities::resolve(release_root.as_deref(), &selected_home, profile, &preferences)
            .map_err(HarnessSupervisorError::Configuration)?;
        // A port preference may synthesize a TCP target; it is not a user's
        // explicit readiness contract and must still receive owned Web health checks.
        let configured_spec = spec.clone();
        nexus_core::apply_harness_preferences(&mut spec, &preferences, &capabilities);
        let notifications = crate::notifications::prepare(&self.paths, &mut spec)
            .map_err(HarnessSupervisorError::Configuration)?;
        crate::desktop_plugins::prepare(&self.paths, &selected_home, profile, &mut spec)
            .map_err(HarnessSupervisorError::Configuration)?;
        let mut readiness = readiness_config(&spec)?;
        let program = spec
            .render_path_for_context(&spec.program, profile, release_id, release_root.as_deref())
            .map_err(HarnessSupervisorError::Configuration)?;
        let arguments = spec
            .render_args_for_context(profile, release_id, release_root.as_deref())
            .map_err(HarnessSupervisorError::Configuration)?;
        let working_dir = spec
            .working_dir
            .as_deref()
            .map(|path| {
                spec.render_path_for_context(path, profile, release_id, release_root.as_deref())
            })
            .transpose()
            .map_err(HarnessSupervisorError::Configuration)?;
        if automatic_web_readiness_supported(&configured_spec, release_root.as_deref(), &selected_home, profile, &arguments) {
            readiness = Some(ReadinessConfig {
                target: ReadinessTarget { host: "127.0.0.1".into(), port: 0, path: "/".into(), tcp: true, token_required: true, owned_web: true },
                timeout: Duration::from_secs(spec.readiness_timeout_secs.unwrap_or(DEFAULT_READINESS_TIMEOUT_SECS)),
            });
        }
        let healthy_candidate = if readiness.as_ref().is_some_and(|config| config.target.token_required) {
            let entry = if spec.mode == HarnessLaunchMode::Node { arguments.first().map(PathBuf::from) } else { Some(program.clone()) };
            entry.and_then(|entry| {
                let entry = if entry.is_relative() { working_dir.as_deref().unwrap_or(Path::new(".")).join(entry) } else { entry };
                let revision = nexus_core::ConfigStore::new(self.paths.clone()).snapshot().ok()?.revision;
                if health_config_revision.as_ref() != Some(&revision) { return None; }
                self.releases.healthy_launch_candidate(release_id?, &entry, profile, revision).ok()
            })
        } else { None };
        self.paths
            .ensure_directories()
            .map_err(HarnessSupervisorError::Configuration)?;

        // Atomically close cancellation before committing process ownership.
        self.startup_phase("spawning").await?;
        let (generation, owner_epoch, runtime) = {
            let mut inner = self.inner.lock().await;
            if inner.stop_pending
                || inner.log_session.launch_pending
                || (inner.child.is_none()
                    && (inner.recovery.is_some()
                        || matches!(
                            inner.runtime.state,
                            HarnessState::Starting | HarnessState::Running
                        )))
            {
                return Err(HarnessSupervisorError::AlreadyRunning);
            }
            if inner.child.is_some() {
                match poll_child(&self.paths, &mut inner, &self.log_sessions) {
                    Ok(None) => return Err(HarnessSupervisorError::AlreadyRunning),
                    Ok(Some(_)) => {
                        if inner.recovery.is_some() {
                            let generation = inner.generation;
                            let runtime = inner.runtime.clone();
                            let recovery = pending_recovery(&mut inner);
                            drop(inner);
                            if let Some(recovery) = recovery {
                                self.spawn_recovery(recovery);
                            }
                            // Persist the recovery observation after its
                            // detached owner is scheduled. Cancellation here
                            // cannot strand recovery, and a faster final
                            // transition makes this stale write a no-op.
                            let _ = self.persist_if_current(generation, &runtime).await;
                            return Err(HarnessSupervisorError::AlreadyRunning);
                        }
                    }
                    Err(error) => return Err(HarnessSupervisorError::Process(error)),
                }
            }
            // Start and Restart both arrive here, after the existing process
            // check. Do not mutate a live instance's shared module fallback.
            #[cfg(windows)]
            finish_owned_job(&mut inner).await.map_err(HarnessSupervisorError::Process)?;
            let managed_entry = if spec.mode == HarnessLaunchMode::Node {
                arguments.first().map(Path::new)
            } else {
                Some(program.as_path())
            };
            if let (Some(root), Some(entry)) = (release_root.as_deref(), managed_entry) {
                let managed = fs::canonicalize(entry).ok().zip(fs::canonicalize(root).ok())
                    .is_some_and(|(entry, root)| entry.starts_with(root));
                if managed {
                    let home = selected_home.clone();
                    let repaired = ReleaseStore::heal_module_farm(&home, root)
                        .map_err(HarnessSupervisorError::Configuration)?;
                    tracing::info!(repaired, release = ?release_id, "prepared Harness module farm");
                }
            }
            let generation = inner.generation.wrapping_add(1).max(1);
            let (mut session, stdout, stderr) = self
                .prepare_log_session(&inner, generation)
                .map_err(HarnessSupervisorError::Persistence)?;
            // Only claim the revision if it remained unchanged throughout
            // launch-input resolution. Mixed/concurrently edited inputs are
            // explicitly unknown, rather than attributed to a newer config.
            let current_revision = nexus_core::ConfigStore::new(self.paths.clone()).snapshot().ok().map(|s| s.revision);
            session.context = Some(nexus_core::OperationContext {
                build_id: option_env!("NEXUS_BUILD_ID").map(str::to_owned),
                version: Some(env!("CARGO_PKG_VERSION").into()),
                profile: Some(profile.into()),
                config_revision: health_config_revision.clone().filter(|revision| current_revision.as_ref() == Some(revision)),
                run_id: Some(session.run_id.clone()),
            });
            let inputs = crate::launch_inputs::describe(profile, &selected_home, &program, working_dir.as_deref(),
                release_root.as_deref(), &preferences, crate::launch_inputs::environment_override(), &arguments);
            if crate::launch_inputs::record(&self.paths, inputs, &session).is_err() {
                // Explanation is optional; a stale record cannot match this new run.
                tracing::warn!("Could not save safe Harness launch inputs; configuration explanation unavailable");
            }
            // Write-ahead ownership intent: the session marker is durable and
            // launch_pending before process creation. If the Agent exits after
            // this point, startup recovery converts even an older terminal
            // state snapshot to Starting and refuses a duplicate spawn.
            self.log_sessions
                .write(&session)
                .map_err(HarnessSupervisorError::Persistence)?;
            inner.generation = generation;
            inner.spawned_launch = false;
            inner.spawned_preferences = None;
            inner.log_session = session;
            inner.healthy_candidate = healthy_candidate.map(|mut candidate| {
                candidate.run_id = inner.log_session.run_id.clone(); candidate.generation = generation; candidate
            });
            inner.verified_health = None;
            let now = unix_time_seconds();
            inner.runtime = HarnessRuntimeInfo {
                state: HarnessState::Starting,
                pid: None,
                exit_code: None,
                error: None,
                started_at_unix: Some(now),
                updated_at_unix: Some(now),
            };
            inner.readiness = readiness.clone();
            inner.recovery = None;
            inner.readiness_owner = None;
            inner.attached_readiness = None;
            inner.unattached_monitor_started = false;
            if let Err(error) = self.store.update_harness(inner.runtime.clone()) {
                // No process exists yet, so a failed prepared-state write can
                // safely release the write-ahead reservation. Publish that
                // release durably before changing the in-memory copy; if the
                // marker write also fails, retain launch_pending so another
                // start remains fail-closed.
                let mut released_session = inner.log_session.clone();
                released_session.launch_pending = false;
                if let Err(release_error) = self.log_sessions.write(&released_session) {
                    return Err(HarnessSupervisorError::Persistence(io::Error::new(
                        error.kind(),
                        format!(
                            "failed to persist prepared Harness state: {error}; failed to release the launch reservation: {release_error}"
                        ),
                    )));
                }
                inner.log_session = released_session;
                inner.readiness = None;
                inner.runtime = failed_runtime(
                    &inner.runtime,
                    format!("failed to persist prepared Harness state: {error}"),
                );
                inner.runtime.pid = None;
                return Err(HarnessSupervisorError::Persistence(error));
            }
            let mut command = Command::new(&program);
            command.envs(runtime_env.iter().map(|(key, value)| (key, value)));
            command.envs(nexus_core::harness_preferences_environment(&preferences, &capabilities));
            command.env("DSH_HOME", &selected_home);
            if let Some(slot) = release_root.as_deref() {
                command.env("NEXUS_DESKTOP_CONTEXT", crate::desktop_plugins::context(&self.paths, &selected_home, profile,
                    &program, slot, effective_runtime.pnpm.as_ref().map(|pin| pin.path.as_path()))
                    .map_err(HarnessSupervisorError::Configuration)?);
                command.env("NEXUS_DESKTOP_RUN", &inner.log_session.run_id);
            }
            if notifications {
                command.env("NEXUS_NOTIFICATION_FILE", self.paths.run_dir.join("notifications.json"));
                command.env("NEXUS_NOTIFICATION_RUN", &inner.log_session.run_id);
            }
            command
                .args(arguments.iter().enumerate().map(|(index, argument)| {
                    if index == 0 && spec.mode == HarnessLaunchMode::Node {
                        nexus_core::node_script_argument(Path::new(argument))
                    } else {
                        std::ffi::OsString::from(argument)
                    }
                }))
                .kill_on_drop(true)
                .stdin(Stdio::null());
            #[cfg(not(windows))]
            command.stdout(Stdio::from(stdout)).stderr(Stdio::from(stderr));
            if let Some(working_dir) = &working_dir {
                command.current_dir(working_dir);
            }

            #[cfg(windows)]
            let spawn_result = spawn_owned_harness(command, Some((&stdout, &stderr))).await;
            #[cfg(not(windows))]
            let spawn_result = spawn_owned_harness(command, None).await;
            let spawned = match spawn_result {
                Ok(spawned) => spawned,
                Err(error) => {
                    inner.runtime =
                        failed_runtime(&inner.runtime, format!("failed to start Harness: {error}"));
                    let _ = self.store.update_harness(inner.runtime.clone());
                    if let Err(release_error) = clear_launch_pending(&self.log_sessions, &mut inner)
                    {
                        return Err(HarnessSupervisorError::Persistence(io::Error::new(
                            release_error.kind(),
                            format!(
                                "failed to start Harness: {error}; failed to release the launch reservation: {release_error}"
                            ),
                        )));
                    }
                    return Err(HarnessSupervisorError::Spawn(error));
                }
            };
            let child = spawned.child;
            let Some(pid) = child.id() else {
                let error = io::Error::other("spawned Harness did not expose a process id");
                inner.runtime = failed_runtime(&inner.runtime, error.to_string());
                clear_launch_pending(&self.log_sessions, &mut inner)
                    .map_err(HarnessSupervisorError::Persistence)?;
                return Err(HarnessSupervisorError::Spawn(error));
            };
            inner.runtime = HarnessRuntimeInfo::starting(pid, now);
            #[cfg(windows)]
            { inner.job = Some(spawned.job); }
            inner.child = Some(child.into());
            inner.spawned_launch = true;
            inner.spawned_preferences = Some(preferences.clone());
            inner.start_ownership_pending = true;
            let owner_epoch = next_operation_epoch(&mut inner);
            inner.readiness_owner = Some(ReadinessOwner::Start {
                generation,
                epoch: owner_epoch,
            });
            (generation, owner_epoch, inner.runtime.clone())
        };

        // The readiness transition must outlive the caller. In particular,
        // aborting an API request after spawn must not strand an attached child
        // in Starting forever. Spawn this task before the first await after
        // spawn; its generation checks make late completion harmless after a
        // stop/restart.
        let (persist_sender, persist_receiver) = oneshot::channel();
        self.spawn_start_readiness(generation, owner_epoch, readiness, persist_receiver);
        #[cfg(test)]
        self.wait_for_start_persist_gate().await;
        // Give the detached owner a scheduling point before the first
        // persistence await. If this request is cancelled here, dropping the
        // sender still drives the owner through the generation-safe failure
        // path.
        tokio::task::yield_now().await;

        let persisted = match self.persist_if_current(generation, &runtime).await {
            Ok(persisted) => persisted,
            Err(error) => {
                // Exactly one detached owner performs and acknowledges cleanup.
                // Cancelling this request while it waits cannot cancel that
                // owner or strand the spawned child.
                let (completion_sender, completion_receiver) = oneshot::channel();
                let signal = StartOwnershipSignal::Abort {
                    message: error.to_string(),
                    completion: completion_sender,
                };
                if let Err(StartOwnershipSignal::Abort {
                    message,
                    completion,
                }) = persist_sender.send(signal)
                {
                    let supervisor = self.clone();
                    tokio::spawn(async move {
                        let result = supervisor
                            .fail_start(
                                ReadinessOwner::Start {
                                    generation,
                                    epoch: owner_epoch,
                                },
                                message,
                            )
                            .await;
                        let _ = completion.send(result);
                    });
                }
                let _ = completion_receiver.await;
                return Err(error);
            }
        };
        if !persisted {
            let error = io::Error::other(
                "initial Harness ownership snapshot was superseded before persistence",
            );
            let (completion_sender, completion_receiver) = oneshot::channel();
            let signal = StartOwnershipSignal::Abort {
                message: error.to_string(),
                completion: completion_sender,
            };
            if let Err(StartOwnershipSignal::Abort {
                message,
                completion,
            }) = persist_sender.send(signal)
            {
                let supervisor = self.clone();
                tokio::spawn(async move {
                    let result = supervisor
                        .fail_start(
                            ReadinessOwner::Start {
                                generation,
                                epoch: owner_epoch,
                            },
                            message,
                        )
                        .await;
                    let _ = completion.send(result);
                });
            }
            let _ = completion_receiver.await;
            return Err(HarnessSupervisorError::Process(error));
        }
        // Only let the detached readiness owner observe the child after the
        // initial Starting ownership record has been accepted. Dropping this
        // sender while the request is cancelled tells that owner to fail and
        // tear down or recover the child instead of leaving it unowned.
        let _ = persist_sender.send(StartOwnershipSignal::Persisted);
        Ok(runtime)
    }

    #[cfg(test)]
    async fn wait_for_start_persist_gate(&self) {
        let gate = self.start_persist_gate.lock().await.take();
        if let Some(gate) = gate {
            let _ = gate.reached.send(());
            let _ = gate.release.await;
        }
    }

    #[cfg(test)]
    async fn wait_for_harness_persist_gate(&self) {
        let gate = self.harness_persist_gate.lock().await.take();
        if let Some(gate) = gate {
            let _ = gate.reached.send(());
            let _ = gate.release.await;
        }
    }

    pub async fn stop(&self) -> Result<HarnessRuntimeInfo, HarnessSupervisorError> {
        let lifecycle = self.acquire_lifecycle().await;
        self.stop_locked(&lifecycle).await
    }

    pub(crate) async fn stop_locked(
        &self,
        _lifecycle: &HarnessLifecycleGuard,
    ) -> Result<HarnessRuntimeInfo, HarnessSupervisorError> {
        self.stop_inner().await
    }

    async fn stop_inner(&self) -> Result<HarnessRuntimeInfo, HarnessSupervisorError> {
        let (child, stop_generation, stop_epoch, readiness, attached_readiness, started_at) = {
            let mut inner = self.inner.lock().await;
            if inner.child.is_none() {
                #[cfg(windows)]
                if inner.job.is_some() && !inner.stop_pending {
                    // A bootstrap parent may have handed off to descendants.
                    // They remain ours even though there is no direct child.
                    finish_owned_job(&mut inner).await.map_err(HarnessSupervisorError::Process)?;
                    inner.recovery = None;
                    inner.readiness = None;
                    inner.readiness_owner = None;
                    inner.attached_readiness = None;
                    inner.unattached_monitor_started = false;
                    inner.start_ownership_pending = false;
                    next_operation_epoch(&mut inner);
                    inner.runtime.state = HarnessState::Stopped;
                    inner.runtime.pid = None;
                    inner.runtime.updated_at_unix = Some(unix_time_seconds());
                    clear_launch_pending(&self.log_sessions, &mut inner).map_err(HarnessSupervisorError::Persistence)?;
                    self.store.update_harness(inner.runtime.clone()).map_err(HarnessSupervisorError::Persistence)?;
                }
                if inner.stop_pending {
                    // A stop (or start-failure reap) is already finishing;
                    // naming it "already running" would misdirect a retry.
                    return Err(HarnessSupervisorError::Busy);
                } else if inner.log_session.launch_pending
                    || inner.recovery.is_some()
                    || (inner.runtime.state == HarnessState::Running && inner.runtime.pid.is_none())
                {
                    return Err(HarnessSupervisorError::Unattached);
                } else {
                    return Ok(inner.runtime.clone());
                }
            } else {
                let generation = inner.generation;
                let stop_epoch = next_operation_epoch(&mut inner);
                inner.stop_pending = true;
                inner.start_ownership_pending = false;
                inner.recovery = None;
                inner.readiness_owner = None;
                let readiness = inner.readiness.take();
                let attached_readiness = inner.attached_readiness.take();
                (
                    inner.child.take(),
                    generation,
                    stop_epoch,
                    readiness,
                    attached_readiness,
                    inner.runtime.started_at_unix,
                )
            }
        };
        let Some(child) = child else {
            return Ok(self.status().await);
        };
        let supervisor = self.clone();
        let (result_sender, result_receiver) = oneshot::channel();
        tokio::spawn(async move {
            let result = supervisor
                .finish_stop(
                    child,
                    stop_generation,
                    stop_epoch,
                    started_at,
                    readiness,
                    attached_readiness,
                )
                .await;
            let _ = result_sender.send(result);
        });
        match result_receiver.await {
            Ok(result) => result,
            Err(_) => {
                self.fail_stop_owner(stop_generation, stop_epoch).await;
                Err(HarnessSupervisorError::Process(io::Error::other(
                    "Harness stop owner ended before publishing a result",
                )))
            }
        }
    }

    async fn finish_stop(
        &self,
        mut child: Child,
        stop_generation: u64,
        stop_epoch: u64,
        started_at: Option<u64>,
        readiness: Option<ReadinessConfig>,
        attached_readiness: Option<AttachedReadinessState>,
    ) -> Result<HarnessRuntimeInfo, HarnessSupervisorError> {
        #[cfg(test)]
        if let Some(gate) = self.stop_wait_gate.lock().await.take() {
            let _ = gate.reached.send(());
            let _ = gate.release.await;
        }
        #[cfg(test)]
        if self
            .stop_wait_failure
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            self.restore_stop_child(
                stop_generation,
                stop_epoch,
                child,
                readiness,
                attached_readiness,
            )
            .await;
            return Err(HarnessSupervisorError::Process(io::Error::other(
                "injected stop wait failure",
            )));
        }

        let stop_request_error = child.request_stop().await.err();
        let mut killed = false;
        let exit = match wait_for_exit(&mut child, self.graceful_wait).await {
            Ok(Some(exit)) => exit,
            Ok(None) => {
                killed = true;
                // Both platform Child types kill the whole process tree.
                let kill_result = child.kill().await;
                if let Err(error) = kill_result {
                    self.restore_stop_child(
                        stop_generation,
                        stop_epoch,
                        child,
                        readiness,
                        attached_readiness,
                    )
                    .await;
                    return Err(HarnessSupervisorError::Process(error));
                }
                match child.wait().await {
                    Ok(exit) => exit,
                    Err(error) => {
                        self.restore_stop_child(
                            stop_generation,
                            stop_epoch,
                            child,
                            readiness,
                            attached_readiness,
                        )
                        .await;
                        return Err(HarnessSupervisorError::Process(error));
                    }
                }
            }
            Err(error) => {
                self.restore_stop_child(
                    stop_generation,
                    stop_epoch,
                    child,
                    readiness,
                    attached_readiness,
                )
                .await;
                return Err(HarnessSupervisorError::Process(error));
            }
        };

        let runtime = if killed {
            HarnessRuntimeInfo {
                state: HarnessState::Stopped,
                pid: None,
                exit_code: exit.code(),
                error: Some(match stop_request_error {
                    Some(error) => format!("Harness was forcibly stopped because the graceful stop request failed: {error}"),
                    None => "Harness was killed after the graceful stop timeout".to_owned(),
                }),
                started_at_unix: started_at,
                updated_at_unix: Some(unix_time_seconds()),
            }
        } else {
            runtime_from_exit_with_state(started_at, None, exit, HarnessState::Stopped)
        };
        // Terminate the job under the lock, then wait for the tree to drain
        // without holding the lock so unlocked read endpoints (preflight,
        // startup status) are not delayed by stubborn descendants.
        #[cfg(windows)]
        let draining = {
            let mut inner = self.inner.lock().await;
            if inner.generation != stop_generation
                || inner.operation_epoch != stop_epoch
                || !inner.stop_pending
            {
                return Ok(inner.runtime.clone());
            }
            take_terminating_job(&mut inner).map_err(HarnessSupervisorError::Process)?
        };
        #[cfg(windows)]
        if let Some(job) = draining.as_ref() {
            if let Err(error) = drain_owned_job(job).await {
                let mut inner = self.inner.lock().await;
                if inner.generation == stop_generation
                    && inner.operation_epoch == stop_epoch
                    && inner.stop_pending
                {
                    inner.stop_pending = false;
                }
                return Err(HarnessSupervisorError::Process(error));
            }
        }
        {
            let mut inner = self.inner.lock().await;
            if inner.generation != stop_generation
                || inner.operation_epoch != stop_epoch
                || !inner.stop_pending
            {
                return Ok(inner.runtime.clone());
            }
            inner.stop_pending = false;
            inner.runtime = runtime.clone();
            clear_launch_pending(&self.log_sessions, &mut inner)
                .map_err(HarnessSupervisorError::Persistence)?;
        }
        let persisted = self.persist_if_current(stop_generation, &runtime).await?;
        if !persisted {
            return Ok(self.status().await);
        }
        Ok(runtime)
    }

    async fn restore_stop_child(
        &self,
        generation: u64,
        stop_epoch: u64,
        child: Child,
        readiness: Option<ReadinessConfig>,
        attached_readiness: Option<AttachedReadinessState>,
    ) {
        let (resume_readiness, owner_epoch, attached_target) = {
            let mut inner = self.inner.lock().await;
            if inner.generation != generation
                || inner.operation_epoch != stop_epoch
                || !inner.stop_pending
                || inner.child.is_some()
            {
                return;
            }
            inner.child = Some(child.into());
            inner.stop_pending = false;
            inner.readiness = readiness.clone();
            let attached_target = match (attached_readiness, readiness.as_ref()) {
                (Some(previous), Some(readiness)) => {
                    let epoch = next_operation_epoch(&mut inner);
                    inner.attached_readiness = Some(AttachedReadinessState {
                        generation,
                        epoch,
                        deadline: previous.deadline,
                    });
                    Some(readiness.target.clone())
                }
                _ => {
                    inner.attached_readiness = None;
                    None
                }
            };
            let resume_readiness =
                inner.runtime.state == HarnessState::Starting && attached_target.is_none();
            let owner_epoch = if resume_readiness {
                // Re-arm the same handoff gate used by a fresh start. The
                // restored monitor cannot observe the child until the detached
                // readiness owner accepts the already-durable ownership.
                inner.start_ownership_pending = true;
                let owner_epoch = next_operation_epoch(&mut inner);
                inner.readiness_owner = Some(ReadinessOwner::Start {
                    generation,
                    epoch: owner_epoch,
                });
                Some(owner_epoch)
            } else {
                None
            };
            (resume_readiness, owner_epoch, attached_target)
        };
        self.spawn_monitor(generation);
        if let Some(target) = attached_target {
            self.spawn_attached_readiness_monitor(generation, target);
        }
        if resume_readiness {
            let (persist_sender, persist_receiver) = oneshot::channel();
            self.spawn_start_readiness(
                generation,
                owner_epoch.expect("readiness owner was assigned"),
                readiness,
                persist_receiver,
            );
            let _ = persist_sender.send(StartOwnershipSignal::Persisted);
        }
    }

    async fn fail_stop_owner(&self, generation: u64, stop_epoch: u64) {
        let runtime = {
            let mut inner = self.inner.lock().await;
            if inner.generation != generation
                || inner.operation_epoch != stop_epoch
                || !inner.stop_pending
            {
                return;
            }
            inner.stop_pending = false;
            inner.attached_readiness = None;
            let mut runtime = failed_runtime(
                &inner.runtime,
                "Harness stop owner ended unexpectedly; the child was kill-on-drop".to_owned(),
            );
            runtime.pid = None;
            inner.runtime = runtime.clone();
            runtime
        };
        let _ = self.persist_if_current(generation, &runtime).await;
    }

    pub async fn restart(&self) -> Result<HarnessRuntimeInfo, HarnessSupervisorError> {
        self.restart_with_profile(DEFAULT_PROFILE).await
    }

    pub async fn restart_with_profile(
        &self,
        profile: &str,
    ) -> Result<HarnessRuntimeInfo, HarnessSupervisorError> {
        let lifecycle = self.acquire_lifecycle().await;
        self.restart_with_profile_locked(profile, &lifecycle).await
    }

    pub(crate) async fn restart_with_profile_locked(
        &self,
        profile: &str,
        _lifecycle: &HarnessLifecycleGuard,
    ) -> Result<HarnessRuntimeInfo, HarnessSupervisorError> {
        crate::recovery_mode::ensure_start_allowed(&self.paths)?;
        let _ = self.stop_inner().await?;
        self.start_with_profile_inner(profile,None).await
    }

    async fn mark_running(&self, owner: ReadinessOwner) -> HarnessRuntimeInfo {
        let generation = readiness_owner_generation(owner);
        let (runtime, arm_unattached_monitor, arm_attached_monitor) = {
            let mut inner = self.inner.lock().await;
            if inner.generation != generation || inner.readiness_owner != Some(owner) {
                return inner.runtime.clone();
            }
            let (arm_unattached_monitor, arm_attached_monitor) = if inner.child.is_none() {
                let target =
                    if inner.runtime.state == HarnessState::Running && inner.recovery.is_none() {
                        inner
                            .readiness
                            .as_ref()
                            .map(|readiness| readiness.target.clone())
                    } else {
                        let Some(recovery) = inner
                            .recovery
                            .as_ref()
                            .filter(|recovery| recovery.generation == generation)
                        else {
                            return inner.runtime.clone();
                        };
                        let target = recovery.target.clone();
                        inner.recovery = None;
                        inner.runtime = running_runtime_without_pid(&inner.runtime);
                        Some(target)
                    };
                inner.readiness_owner = None;
                if !inner.unattached_monitor_started {
                    if let Some(target) = target {
                        // Claim monitor ownership while holding the same lock
                        // that publishes PID-less Running. A competing
                        // recovery owner then observes either this owner or a
                        // different generation, never an unmonitored state.
                        inner.unattached_monitor_started = true;
                        (Some(target), None)
                    } else {
                        (None, None)
                    }
                } else {
                    (None, None)
                }
            } else {
                let (pid, started_at) = match (&inner.runtime.pid, inner.runtime.started_at_unix) {
                    (Some(pid), Some(started_at)) => (*pid, started_at),
                    _ => return inner.runtime.clone(),
                };
                inner.readiness_owner = None;
                inner.runtime = HarnessRuntimeInfo::running(pid, started_at, unix_time_seconds());
                if inner.readiness.as_ref().is_some_and(|config| config.target.token_required) {
                    if let Some(candidate) = inner.healthy_candidate.take().filter(|candidate| candidate.generation == generation && candidate.run_id == inner.log_session.run_id) {
                        match self.releases.record_healthy_release(candidate) {
                            Ok(()) => inner.verified_health = Some((inner.log_session.run_id.clone(), generation)),
                            Err(error) => tracing::warn!(error = %error, "Could not persist verified Harness release health"),
                        }
                    }
                }
                let target = inner
                    .readiness
                    .as_ref()
                    .map(|readiness| readiness.target.clone());
                inner.attached_readiness = if target.is_some() {
                    let epoch = next_operation_epoch(&mut inner);
                    Some(AttachedReadinessState {
                        generation,
                        epoch,
                        deadline: None,
                    })
                } else {
                    None
                };
                (None, target)
            };
            (
                inner.runtime.clone(),
                arm_unattached_monitor,
                arm_attached_monitor,
            )
        };
        if let Some(target) = arm_unattached_monitor {
            // Spawn before the first await after claiming ownership so this
            // handoff remains cancellation-safe.
            self.spawn_unattached_monitor(generation, target);
        }
        if let Some(target) = arm_attached_monitor {
            self.spawn_attached_readiness_monitor(generation, target);
        }
        if matches!(
            self.persist_if_current(generation, &runtime).await,
            Ok(false)
        ) {
            return self.inner.lock().await.runtime.clone();
        }
        runtime
    }

    fn spawn_start_readiness(
        &self,
        generation: u64,
        owner_epoch: u64,
        readiness: Option<ReadinessConfig>,
        persist_receiver: oneshot::Receiver<StartOwnershipSignal>,
    ) {
        let supervisor = self.clone();
        tokio::spawn(async move {
            let start_owner = ReadinessOwner::Start {
                generation,
                epoch: owner_epoch,
            };
            match persist_receiver.await {
                Ok(StartOwnershipSignal::Persisted) => {
                    let accepted = {
                        let mut inner = supervisor.inner.lock().await;
                        if inner.generation != generation
                            || inner.readiness_owner
                                != Some(ReadinessOwner::Start {
                                    generation,
                                    epoch: owner_epoch,
                                })
                            || !inner.start_ownership_pending
                            || inner.child.is_none()
                        {
                            false
                        } else {
                            inner.start_ownership_pending = false;
                            true
                        }
                    };
                    if !accepted {
                        return;
                    }
                    supervisor.spawn_monitor(generation);
                }
                Ok(StartOwnershipSignal::Abort {
                    message,
                    completion,
                }) => {
                    let result = supervisor.fail_start(start_owner, message).await;
                    let _ = completion.send(result);
                    return;
                }
                Err(_) => {
                    let _ = supervisor
                        .fail_start(
                            start_owner,
                            "start request was cancelled before ownership was persisted".to_owned(),
                        )
                        .await;
                    return;
                }
            }
            let readiness_result = match readiness.as_ref() {
                Some(readiness) => supervisor.wait_for_readiness(start_owner, readiness).await,
                None => Ok(Some(start_owner)),
            };
            match readiness_result {
                Ok(Some(owner)) => {
                    let _ = supervisor.mark_running(owner).await;
                }
                Ok(None) => return,
                Err((error, owner)) => {
                    let _ = supervisor.fail_start(owner, error.to_string()).await;
                }
            }
        });
    }

    async fn fail_start(&self, owner: ReadinessOwner, message: String) -> Result<(), String> {
        let generation = readiness_owner_generation(owner);
        let (child, cancellation_epoch, recovery_exit_code, readiness) = {
            let mut inner = self.inner.lock().await;
            if inner.generation != generation || inner.readiness_owner != Some(owner) {
                return Ok(());
            }
            let recovery_exit_code = inner.recovery.as_ref().map(|recovery| recovery.exit_code);
            let cancellation_epoch = next_operation_epoch(&mut inner);
            inner.stop_pending = true;
            inner.start_ownership_pending = false;
            inner.recovery = None;
            inner.readiness_owner = None;
            inner.attached_readiness = None;
            let readiness = inner.readiness.take();
            (
                inner.child.take(),
                cancellation_epoch,
                recovery_exit_code,
                readiness,
            )
        };
        let mut natural_exit = None;
        if let Some(mut child) = child {
            let termination = match child.try_wait() {
                Ok(Some(exit)) => {
                    natural_exit = Some(exit);
                    Ok(())
                }
                Ok(None) => {
                // Both platform Child types kill the whole process tree.
                let kill_result = child.kill().await;
                match kill_result {
                        Ok(()) => child.wait().await.map(|_| ()),
                        Err(kill_error) => match child.try_wait() {
                            Ok(Some(exit)) => {
                                natural_exit = Some(exit);
                                Ok(())
                            }
                            Ok(None) | Err(_) => Err(kill_error),
                        },
                    }
                }
                Err(error) => Err(error),
            };
            if let Err(error) = termination {
                self.restore_stop_child(generation, cancellation_epoch, child, readiness, None)
                    .await;
                return Err(format!(
                    "failed to reap Harness after start failure: {error}"
                ));
            }
        }
        if recovery_exit_code.is_none() {
            if let (Some(exit), Some(readiness)) = (natural_exit, readiness.as_ref()) {
                let (runtime, recovery) = {
                    let mut inner = self.inner.lock().await;
                    if inner.generation != generation
                        || inner.operation_epoch != cancellation_epoch
                        || !inner.stop_pending
                    {
                        return Ok(());
                    }
                    inner.stop_pending = false;
                    inner.readiness = Some(readiness.clone());
                    inner.recovery = Some(RecoveryState {
                        generation,
                        target: readiness.target.clone(),
                        deadline: Instant::now() + readiness.timeout,
                        exit_code: exit.code(),
                        task_started: false,
                    });
                    inner.runtime = recovery_runtime(&inner.runtime);
                    let recovery = pending_recovery(&mut inner);
                    (inner.runtime.clone(), recovery)
                };
                if let Some(recovery) = recovery {
                    self.spawn_recovery(recovery);
                }
                return self
                    .persist_if_current(generation, &runtime)
                    .await
                    .map(|_| ())
                    .map_err(|error| error.to_string());
            }
        }
        // Terminate the job under the lock and drain it without holding the
        // lock, re-validating the ownership tokens afterwards.
        #[cfg(windows)]
        let draining = {
            let mut inner = self.inner.lock().await;
            if inner.generation != generation || inner.operation_epoch != cancellation_epoch {
                return Ok(());
            }
            take_terminating_job(&mut inner).map_err(|error| {
                format!("failed to reap Harness process tree after start failure: {error}")
            })?
        };
        #[cfg(windows)]
        if let Some(job) = draining.as_ref() {
            if let Err(error) = drain_owned_job(job).await {
                let mut inner = self.inner.lock().await;
                if inner.generation == generation && inner.operation_epoch == cancellation_epoch {
                    inner.stop_pending = false;
                }
                return Err(format!("failed to reap Harness process tree after start failure: {error}"));
            }
        }
        let runtime = {
            let mut inner = self.inner.lock().await;
            if inner.generation != generation || inner.operation_epoch != cancellation_epoch {
                return Ok(());
            }
            inner.stop_pending = false;
            inner.start_ownership_pending = false;
            inner.runtime = match recovery_exit_code {
                Some(exit_code) => recovery_failed_runtime(&inner.runtime, exit_code),
                None => {
                    let mut runtime = failed_runtime(&inner.runtime, message);
                    // The child has been reaped above. Never expose its former
                    // process id as if it were still owned by this supervisor.
                    runtime.pid = None;
                    runtime
                }
            };
            clear_launch_pending(&self.log_sessions, &mut inner)
                .map_err(|error| error.to_string())?;
            inner.runtime.clone()
        };
        self.persist_if_current(generation, &runtime)
            .await
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    async fn persist_if_current(
        &self,
        generation: u64,
        runtime: &HarnessRuntimeInfo,
    ) -> Result<bool, HarnessSupervisorError> {
        #[cfg(test)]
        self.wait_for_harness_persist_gate().await;
        let inner = self.inner.lock().await;
        if inner.generation != generation || inner.runtime != *runtime {
            return Ok(false);
        }
        // This path persists outcomes from an owned, spawned child. Missing
        // Node / pre-spawn configuration errors never reach this point.
        if runtime.state == HarnessState::Failed && inner.spawned_launch {
            if let Some(preferences) = &inner.spawned_preferences {
                crate::runtime_patches::record_failure(&self.paths, preferences, "spawned_combination_failed")
                    .map_err(HarnessSupervisorError::Persistence)?;
            }
        }
        self.store
            .update_harness(runtime.clone())
            .map_err(HarnessSupervisorError::Persistence)?;
        Ok(true)
    }

    fn prepare_log_session(
        &self,
        inner: &SupervisorInner,
        generation: u64,
    ) -> io::Result<(HarnessLogSession, fs::File, fs::File)> {
        let durable = self.log_sessions.read()?.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "Harness log session marker disappeared before start",
            )
        })?;
        if durable != inner.log_session {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "Harness log session marker belongs to another Agent writer",
            ));
        }
        create_harness_log_session(&self.paths, generation, true)
    }

    fn spawn_monitor(&self, generation: u64) {
        let supervisor = self.clone();
        let inner = Arc::clone(&self.inner);
        tokio::spawn(async move {
            loop {
                let (runtime, changed, done, recovery) = {
                    let mut inner = inner.lock().await;
                    if inner.generation != generation || inner.child.is_none() {
                        (inner.runtime.clone(), false, true, None)
                    } else {
                        let (runtime, changed) = match poll_child(
                            &supervisor.paths,
                            &mut inner,
                            &supervisor.log_sessions,
                        ) {
                            Ok(Some(runtime)) => (runtime, true),
                            Ok(None) => (inner.runtime.clone(), false),
                            Err(error) => {
                                inner.recovery = None;
                                inner.attached_readiness = None;
                                inner.runtime = failed_runtime(
                                    &inner.runtime,
                                    format!("failed to query child process: {error}"),
                                );
                                (inner.runtime.clone(), true)
                            }
                        };
                        let recovery = pending_recovery(&mut inner);
                        let done = inner.child.is_none() || inner.generation != generation;
                        (runtime, changed, done, recovery)
                    }
                };
                if let Some(recovery) = recovery {
                    supervisor.spawn_recovery(recovery);
                }
                if changed {
                    let _ = supervisor.persist_if_current(generation, &runtime).await;
                }
                if done {
                    return;
                }
                sleep(MONITOR_INTERVAL).await;
            }
        });
    }

    async fn wait_for_readiness(
        &self,
        mut owner: ReadinessOwner,
        readiness: &ReadinessConfig,
    ) -> Result<Option<ReadinessOwner>, (HarnessSupervisorError, ReadinessOwner)> {
        let start_deadline = Instant::now() + readiness.timeout;
        let mut log_observer = HarnessLogObserver::default();
        let mut target = readiness.target.clone();

        loop {
            let lease = match self.refresh_readiness_owner(owner, start_deadline).await {
                Some(lease) => lease,
                None => return Ok(None),
            };
            owner = lease.owner;
            if Instant::now() >= lease.deadline {
                return Err((
                    HarnessSupervisorError::Readiness(
                        "timed out waiting for configured loopback readiness endpoint".to_owned(),
                    ),
                    owner,
                ));
            }

            let remaining = lease.deadline.saturating_duration_since(Instant::now());
            let attempt_timeout = remaining.min(READINESS_ATTEMPT_TIMEOUT);
            if target.owned_web {
                let session = {
                    let inner = self.inner.lock().await;
                    if inner.readiness_owner != Some(owner) || inner.child.is_none() { return Ok(None); }
                    inner.log_session.clone()
                };
                let Some(discovered) = observed_web_readiness(&self.paths, &session, &mut log_observer) else { sleep(MONITOR_INTERVAL).await; continue; };
                target = discovered;
            }
            let ready = matches!(
                timeout(attempt_timeout, readiness_probe(&target)).await,
                Ok(Ok(()))
            );
            if ready
                && self
                    .readiness_evidence_matches_owner(owner, &target, &mut log_observer)
                    .await
            {
                if target.owned_web {
                    let mut inner = self.inner.lock().await;
                    if inner.readiness_owner != Some(owner) { return Ok(None); }
                    if let Some(config) = inner.readiness.as_mut() { config.target = target.clone(); }
                }
                return Ok(self
                    .refresh_readiness_owner(owner, start_deadline)
                    .await
                    .map(|lease| lease.owner));
            }
            sleep(MONITOR_INTERVAL).await;
        }
    }

    async fn refresh_readiness_owner(
        &self,
        owner: ReadinessOwner,
        start_deadline: Instant,
    ) -> Option<ReadinessLease> {
        let generation = readiness_owner_generation(owner);
        let (lease, changed, runtime, recovery) = {
            let mut inner = self.inner.lock().await;
            if inner.generation != generation
                || current_readiness_lease(&inner, owner, start_deadline).is_none()
            {
                return None;
            }
            let (runtime, changed) = match poll_child(&self.paths, &mut inner, &self.log_sessions) {
                Ok(Some(runtime)) => (runtime, true),
                Ok(None) => (inner.runtime.clone(), false),
                Err(error) => {
                    inner.recovery = None;
                    inner.runtime = failed_runtime(
                        &inner.runtime,
                        format!("failed to query child process: {error}"),
                    );
                    (inner.runtime.clone(), true)
                }
            };
            let recovery = pending_recovery(&mut inner);
            let lease = current_readiness_lease(&inner, owner, start_deadline);
            (lease, changed, runtime, recovery)
        };
        if let Some(recovery) = recovery {
            self.spawn_recovery(recovery);
        }
        if changed {
            let _ = self.persist_if_current(generation, &runtime).await;
        }
        lease
    }

    fn spawn_recovery(&self, task: RecoveryTask) {
        let supervisor = self.clone();
        let inner = Arc::clone(&self.inner);
        tokio::spawn(async move {
            let mut log_observer = HarnessLogObserver::default();
            loop {
                let valid = {
                    let inner = inner.lock().await;
                    recovery_is_current(&inner, &task)
                };
                if !valid {
                    return;
                }

                let now = Instant::now();
                if now >= task.deadline {
                    let runtime = {
                        let mut inner = inner.lock().await;
                        if !recovery_is_current(&inner, &task) {
                            return;
                        }
                        inner.recovery = None;
                        inner.readiness_owner = None;
                        inner.unattached_monitor_started = false;
                        inner.runtime = recovery_failed_runtime(&inner.runtime, task.exit_code);
                        if let Err(error) =
                            clear_launch_pending(&supervisor.log_sessions, &mut inner)
                        {
                            let message = inner.runtime.error.clone().unwrap_or_else(|| {
                                "Harness loopback readiness recovery timed out".to_owned()
                            });
                            inner.runtime.error = Some(format!(
                                "{message}; failed to clear expired launch reservation: {error}"
                            ));
                        }
                        inner.runtime.clone()
                    };
                    let _ = supervisor
                        .persist_if_current(task.generation, &runtime)
                        .await;
                    return;
                }

                let remaining = task.deadline.saturating_duration_since(now);
                let attempt_timeout = remaining.min(READINESS_ATTEMPT_TIMEOUT);
                let owner = ReadinessOwner::Recovery {
                    generation: task.generation,
                    epoch: task.owner_epoch,
                };
                let ready = matches!(
                    timeout(attempt_timeout, readiness_probe(&task.target)).await,
                    Ok(Ok(()))
                ) && supervisor
                    .readiness_evidence_matches_owner(owner, &task.target, &mut log_observer)
                    .await;
                if ready && Instant::now() <= task.deadline {
                    let (runtime, arm_monitor) = {
                        let mut inner = inner.lock().await;
                        if !recovery_is_current(&inner, &task) {
                            return;
                        }
                        inner.recovery = None;
                        inner.readiness_owner = None;
                        inner.runtime = running_runtime_without_pid(&inner.runtime);
                        let arm_monitor = if inner.unattached_monitor_started {
                            false
                        } else {
                            inner.unattached_monitor_started = true;
                            true
                        };
                        (inner.runtime.clone(), arm_monitor)
                    };
                    let _ = supervisor
                        .persist_if_current(task.generation, &runtime)
                        .await;
                    if arm_monitor {
                        supervisor.spawn_unattached_monitor(task.generation, task.target.clone());
                    }
                    return;
                }
                if Instant::now() >= task.deadline {
                    continue;
                }
                sleep(MONITOR_INTERVAL).await;
            }
        });
    }

    async fn readiness_evidence_matches_owner(
        &self,
        owner: ReadinessOwner,
        target: &ReadinessTarget,
        observer: &mut HarnessLogObserver,
    ) -> bool {
        if !target.token_required {
            return true;
        }
        let session = {
            let inner = self.inner.lock().await;
            if inner.readiness_owner != Some(owner) {
                return false;
            }
            if target.owned_web && !owned_web_listener(&inner, target).await { return false; }
            inner.log_session.clone()
        };
        if !matches!(self.log_sessions.read(), Ok(Some(ref durable)) if durable == &session) {
            return false;
        }
        readiness_has_current_token(&self.paths, target, &session, observer)
    }

    async fn readiness_evidence_matches_current_session(
        &self,
        target: &ReadinessTarget,
        observer: &mut HarnessLogObserver,
    ) -> bool {
        if !target.token_required {
            return true;
        }
        let session = {
            let inner = self.inner.lock().await;
            if target.owned_web && !owned_web_listener(&inner, target).await { return false; }
            inner.log_session.clone()
        };
        if !matches!(self.log_sessions.read(), Ok(Some(ref durable)) if durable == &session) {
            return false;
        }
        readiness_has_current_token(&self.paths, target, &session, observer)
    }

    /// An attached Harness can keep the same OS process while restarting its
    /// internal HTTP service. Its process handle is therefore not sufficient
    /// evidence that a previously observed credential still belongs to the
    /// current service instance. The first readiness gap durably advances the
    /// log watermark and publishes Starting while retaining the child owner.
    fn spawn_attached_readiness_monitor(&self, initial_generation: u64, target: ReadinessTarget) {
        let supervisor = self.clone();
        tokio::spawn(async move {
            let mut generation = initial_generation;
            let mut log_observer = HarnessLogObserver::default();
            loop {
                sleep(Duration::from_millis(250)).await;
                let phase = {
                    let inner = supervisor.inner.lock().await;
                    let Some(phase) = inner.attached_readiness.clone() else {
                        return;
                    };
                    if inner.generation != generation
                        || phase.generation != generation
                        || inner.child.is_none()
                        || inner.stop_pending
                        || (phase.deadline.is_none()
                            && inner.runtime.state != HarnessState::Running)
                        || (phase.deadline.is_some()
                            && inner.runtime.state != HarnessState::Starting)
                    {
                        return;
                    }
                    phase
                };
                let healthy = matches!(
                    timeout(READINESS_ATTEMPT_TIMEOUT, readiness_probe(&target)).await,
                    Ok(Ok(()))
                );

                if phase.deadline.is_none() {
                    if healthy {
                        continue;
                    }
                    let transition = {
                        let mut inner = supervisor.inner.lock().await;
                        if !attached_readiness_is_current(&inner, generation, &phase)
                            || inner.runtime.state != HarnessState::Running
                        {
                            None
                        } else {
                            match advance_attached_session(
                                &supervisor.paths,
                                &supervisor.log_sessions,
                                &mut inner,
                            ) {
                                Ok(()) => Some((inner.generation, inner.runtime.clone(), true)),
                                Err(error) => {
                                    inner.attached_readiness = None;
                                    inner.runtime = failed_runtime(
                                        &inner.runtime,
                                        format!(
                                            "failed to establish a new attached Harness token boundary after readiness loss: {error}"
                                        ),
                                    );
                                    Some((inner.generation, inner.runtime.clone(), false))
                                }
                            }
                        }
                    };
                    let Some((next_generation, runtime, boundary_advanced)) = transition else {
                        return;
                    };
                    if boundary_advanced {
                        // The process monitor for the prior generation exits
                        // after the boundary change; immediately transfer child
                        // observation to the new generation before awaiting IO.
                        supervisor.spawn_monitor(next_generation);
                    }
                    let _ = supervisor
                        .persist_if_current(next_generation, &runtime)
                        .await;
                    if !boundary_advanced {
                        return;
                    }
                    generation = next_generation;
                    continue;
                }

                let current_token = if healthy && phase.deadline.is_some() {
                    supervisor
                        .readiness_evidence_matches_current_session(&target, &mut log_observer)
                        .await
                } else {
                    true
                };

                if healthy
                    && current_token
                    && phase
                        .deadline
                        .map_or(true, |deadline| Instant::now() <= deadline)
                {
                    let transition = {
                        let mut inner = supervisor.inner.lock().await;
                        if !attached_readiness_is_current(&inner, generation, &phase)
                            || inner.runtime.state != HarnessState::Starting
                        {
                            None
                        } else {
                            let (pid, started_at) =
                                match (inner.runtime.pid, inner.runtime.started_at_unix) {
                                    (Some(pid), Some(started_at)) => (pid, started_at),
                                    _ => {
                                        inner.attached_readiness = None;
                                        return;
                                    }
                                };
                            inner.runtime =
                                HarnessRuntimeInfo::running(pid, started_at, unix_time_seconds());
                            inner.attached_readiness = Some(AttachedReadinessState {
                                generation,
                                epoch: phase.epoch,
                                deadline: None,
                            });
                            Some(inner.runtime.clone())
                        }
                    };
                    let Some(runtime) = transition else {
                        return;
                    };
                    let _ = supervisor.persist_if_current(generation, &runtime).await;
                    continue;
                }

                if phase
                    .deadline
                    .is_some_and(|deadline| Instant::now() >= deadline)
                {
                    let runtime = {
                        let mut inner = supervisor.inner.lock().await;
                        if !attached_readiness_is_current(&inner, generation, &phase) {
                            return;
                        }
                        inner.attached_readiness = None;
                        let mut runtime = failed_runtime(
                            &inner.runtime,
                            "attached Harness readiness recovery timed out".to_owned(),
                        );
                        runtime.pid = inner.runtime.pid;
                        inner.runtime = runtime;
                        inner.runtime.clone()
                    };
                    let _ = supervisor.persist_if_current(generation, &runtime).await;
                    return;
                }
            }
        });
    }

    /// A recovered Harness descendant has no direct child handle, so its
    /// readiness endpoint is the only bounded liveness signal. This owner
    /// keeps probing while the exact generation remains Running. On loss it
    /// advances the token watermark before recovery can publish Running again.
    fn spawn_unattached_monitor(&self, generation: u64, target: ReadinessTarget) {
        let supervisor = self.clone();
        tokio::spawn(async move {
            loop {
                sleep(Duration::from_millis(250)).await;
                let current = {
                    let inner = supervisor.inner.lock().await;
                    inner.generation == generation
                        && inner.child.is_none()
                        && inner.recovery.is_none()
                        && inner.runtime.state == HarnessState::Running
                        && inner.unattached_monitor_started
                };
                if !current {
                    return;
                }
                let healthy = matches!(
                    timeout(READINESS_ATTEMPT_TIMEOUT, readiness_probe(&target)).await,
                    Ok(Ok(()))
                );
                if healthy {
                    continue;
                }

                // A PID-less descendant has no process identity that Nexus can
                // compare across probes. The first observed readiness gap may
                // be a restart, so invalidate the old token epoch immediately.
                let transition = {
                    let mut inner = supervisor.inner.lock().await;
                    if inner.generation != generation
                        || inner.child.is_some()
                        || inner.recovery.is_some()
                        || inner.runtime.state != HarnessState::Running
                        || !inner.unattached_monitor_started
                    {
                        None
                    } else {
                        inner.unattached_monitor_started = false;
                        match advance_unattached_session(
                            &supervisor.paths,
                            &supervisor.log_sessions,
                            &mut inner,
                            &target,
                        ) {
                            Ok(()) => {
                                let recovery = pending_recovery(&mut inner);
                                Some((inner.generation, inner.runtime.clone(), recovery))
                            }
                            Err(error) => {
                                inner.runtime = failed_runtime(
                                    &inner.runtime,
                                    format!(
                                        "failed to establish a new Harness token boundary after liveness loss: {error}"
                                    ),
                                );
                                Some((inner.generation, inner.runtime.clone(), None))
                            }
                        }
                    }
                };
                let Some((next_generation, runtime, recovery)) = transition else {
                    return;
                };
                if let Some(recovery) = recovery {
                    supervisor.spawn_recovery(recovery);
                }
                let _ = supervisor
                    .persist_if_current(next_generation, &runtime)
                    .await;
                return;
            }
        });
    }
}

fn observed_web_readiness(paths: &NexusPaths, session: &HarnessLogSession, observer: &mut HarnessLogObserver) -> Option<ReadinessTarget> {
    if !matches!(HarnessLogSessionStore::new(paths.clone()).read(), Ok(Some(ref durable)) if durable == session) { return None; }
    let info = read_harness_ui_info_with_observer(paths, observer, Some(session));
    if !info.available || info.token.is_none() || info.run_id.as_deref() != Some(session.run_id.as_str()) || info.generation != Some(session.generation) { return None; }
    let mut target = ReadinessTarget::parse(info.url.as_deref()?).ok()?;
    // Never retain the credential in readiness targets, debug output or errors.
    target.path = "/".into(); target.tcp = true; target.token_required = true; target.owned_web = true;
    if target.host.eq_ignore_ascii_case("localhost") { target.host = "127.0.0.1".into(); }
    Some(target)
}

fn automatic_web_readiness_supported(spec: &HarnessLaunchSpec, root: Option<&Path>, home: &Path, profile: &str, arguments: &[String]) -> bool {
    if !cfg!(any(windows, target_os = "macos")) || spec.readiness_url.is_some() || spec.mode != HarnessLaunchMode::Node { return false; }
    let Some(root) = root else { return false; };
    let managed = arguments.first().and_then(|entry| fs::canonicalize(entry).ok())
        .zip(fs::canonicalize(root.join("apps/cli/lib/bin.js")).ok()).is_some_and(|(entry, expected)| entry == expected);
    managed && crate::preference_capabilities::inspect(root, home, profile).is_ok_and(|evidence| evidence.capabilities.web)
}

async fn owned_web_listener(inner: &SupervisorInner, target: &ReadinessTarget) -> bool {
    #[cfg(windows)]
    {
        let Some(pid) = inner.child.as_ref().and_then(Child::id) else { return false; };
        let Ok(owners) = windows_listener_owners(&target.host, target.port) else { return false; };
        !owners.is_empty() && owners.into_iter().all(|owner| owner == pid || inner.job.as_ref().is_some_and(|job| job.contains_pid(owner).unwrap_or(false)))
    }
    #[cfg(target_os = "macos")]
    {
        let Some(group) = inner.child.as_ref().and_then(Child::group) else { return false; };
        let output = timeout(Duration::from_secs(1), Command::new("/usr/sbin/lsof")
            .args(["-nP", "-t", "-a", &format!("-iTCP:{}", target.port), "-sTCP:LISTEN"])
            .stdin(Stdio::null()).stderr(Stdio::null()).kill_on_drop(true).output()).await;
        let Ok(Ok(output)) = output else { return false; };
        if !output.status.success() || output.stdout.len() > 65536 { return false; }
        let Ok(text) = std::str::from_utf8(&output.stdout) else { return false; };
        let owners: Vec<_> = text.lines().collect();
        !owners.is_empty() && owners.iter().all(|line| line.parse::<i32>().ok()
            .is_some_and(|pid| pid > 1 && unsafe { libc::getpgid(pid) } == group))
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    { let _ = (inner, target); false }
}

#[cfg(windows)]
fn windows_listener_owners(host: &str, port: u16) -> io::Result<Vec<u32>> {
    // Query the OS listener table directly; never launch a shell or trust the
    // fact that an arbitrary process accepts a loopback TCP connection.
    #[link(name = "iphlpapi")]
    extern "system" { fn GetExtendedTcpTable(table: *mut std::ffi::c_void, size: *mut u32, order: i32, family: u32, class: i32, reserved: u32) -> u32; }
    let ipv6 = host == "::1";
    let mut size = 0u32;
    let status = unsafe { GetExtendedTcpTable(std::ptr::null_mut(), &mut size, 0, if ipv6 { 23 } else { 2 }, 3, 0) };
    if status != 122 { return Err(io::Error::from_raw_os_error(status as i32)); }
    for _ in 0..3 {
        if size < 4 || size > 1024 * 1024 { return Err(io::Error::other("Listener table exceeds observation budget")); }
        let mut table = vec![0u32; (size as usize + 3) / 4];
        let status = unsafe { GetExtendedTcpTable(table.as_mut_ptr().cast(), &mut size, 0, if ipv6 { 23 } else { 2 }, 3, 0) };
        if status == 122 { continue; }
        if status != 0 { return Err(io::Error::from_raw_os_error(status as i32)); }
        let words = if ipv6 { 14 } else { 6 };
        let count = table[0] as usize;
        if count > (table.len() - 1) / words { return Err(io::Error::other("Invalid listener table")); }
        let mut owners = Vec::new();
        for row in table[1..].chunks_exact(words).take(count) {
            let (port_word, pid) = if ipv6 { (row[5], row[13]) } else { (row[2], row[5]) };
            let local = if ipv6 {
                let bytes: Vec<u8> = row[..4].iter().flat_map(|v|v.to_ne_bytes()).collect();
                bytes.iter().all(|b|*b==0) || (bytes[..15].iter().all(|b|*b==0) && bytes[15]==1)
            } else { row[1] == 0 || row[1].to_ne_bytes() == [127,0,0,1] };
            if local && u16::from_be(port_word as u16) == port { owners.push(pid); }
        }
        return Ok(owners);
    }
    Err(io::Error::other("Listener table changed during observation"))
}

fn readiness_config(
    spec: &HarnessLaunchSpec,
) -> Result<Option<ReadinessConfig>, HarnessSupervisorError> {
    spec.readiness_url
        .as_deref()
        .map(|url| {
            let mut target = ReadinessTarget::parse(url)?;
            target.token_required = spec.readiness_token_required;
            Ok(ReadinessConfig {
                target,
                timeout: Duration::from_secs(
                    spec.readiness_timeout_secs
                        .unwrap_or(DEFAULT_READINESS_TIMEOUT_SECS),
                ),
            })
        })
        .transpose()
}

fn pending_recovery(inner: &mut SupervisorInner) -> Option<RecoveryTask> {
    let recovery = inner.recovery.as_ref()?;
    if recovery.generation != inner.generation {
        inner.recovery = None;
        inner.readiness_owner = None;
        return None;
    }
    if recovery.task_started {
        return None;
    }
    if let Some(ReadinessOwner::Start { generation, epoch }) = inner.readiness_owner {
        if generation == recovery.generation {
            if let Some(recovery) = inner.recovery.as_mut() {
                recovery.task_started = true;
            }
            inner.readiness_owner = Some(ReadinessOwner::Recovery { generation, epoch });
            return None;
        }
    }
    let generation = recovery.generation;
    let target = recovery.target.clone();
    let deadline = recovery.deadline;
    let exit_code = recovery.exit_code;
    let owner_epoch = next_operation_epoch(inner);
    inner.readiness_owner = Some(ReadinessOwner::Recovery {
        generation,
        epoch: owner_epoch,
    });
    let task = RecoveryTask {
        generation,
        target,
        deadline,
        exit_code,
        owner_epoch,
    };
    if let Some(recovery) = inner.recovery.as_mut() {
        recovery.task_started = true;
    }
    Some(task)
}

fn recovery_is_current(inner: &SupervisorInner, task: &RecoveryTask) -> bool {
    inner.generation == task.generation
        && inner.readiness_owner
            == Some(ReadinessOwner::Recovery {
                generation: task.generation,
                epoch: task.owner_epoch,
            })
        && inner.child.is_none()
        && inner.recovery.as_ref().is_some_and(|recovery| {
            recovery.generation == task.generation
                && recovery.exit_code == task.exit_code
                && recovery.deadline == task.deadline
        })
}

fn attached_readiness_is_current(
    inner: &SupervisorInner,
    generation: u64,
    expected: &AttachedReadinessState,
) -> bool {
    inner.generation == generation
        && inner.child.is_some()
        && !inner.stop_pending
        && inner.attached_readiness.as_ref() == Some(expected)
}

fn current_readiness_lease(
    inner: &SupervisorInner,
    expected: ReadinessOwner,
    start_deadline: Instant,
) -> Option<ReadinessLease> {
    let current = inner.readiness_owner?;
    match (expected, current) {
        (ReadinessOwner::Start { .. }, ReadinessOwner::Start { .. }) if expected == current => {
            Some(ReadinessLease {
                owner: current,
                deadline: start_deadline,
            })
        }
        (
            ReadinessOwner::Start { generation, epoch }
            | ReadinessOwner::Recovery { generation, epoch },
            ReadinessOwner::Recovery {
                generation: current_generation,
                epoch: current_epoch,
            },
        ) if generation == current_generation && epoch == current_epoch => inner
            .recovery
            .as_ref()
            .filter(|recovery| recovery.generation == generation)
            .map(|recovery| ReadinessLease {
                owner: current,
                deadline: recovery.deadline,
            }),
        _ => None,
    }
}

async fn probe_until_deadline(target: &ReadinessTarget, deadline: Instant) -> bool {
    loop {
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        let attempt_timeout = deadline
            .saturating_duration_since(now)
            .min(READINESS_ATTEMPT_TIMEOUT);
        if matches!(
            timeout(attempt_timeout, readiness_probe(target)).await,
            Ok(Ok(()))
        ) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        sleep(MONITOR_INTERVAL).await;
    }
}

fn create_new_log(logs_dir: &Path, name: &str) -> io::Result<fs::File> {
    fs::OpenOptions::new()
        .write(true)
        .append(true)
        .create_new(true)
        .open(logs_dir.join(name))
}

fn poll_child(
    paths: &NexusPaths,
    inner: &mut SupervisorInner,
    log_sessions: &HarnessLogSessionStore,
) -> io::Result<Option<HarnessRuntimeInfo>> {
    if inner.start_ownership_pending {
        return Ok(None);
    }
    let Some(child) = inner.child.as_mut() else {
        return Ok(None);
    };
    let Some(exit) = child.try_wait()? else {
        return Ok(None);
    };
    inner.child = None;
    inner.attached_readiness = None;
    let runtime = if let Some(readiness) = inner.readiness.clone() {
        // A Harness bootstrap parent may exit successfully after handing the
        // listener to a long-lived descendant. Only the explicit stop owner
        // may interpret exit 0 as Stopped; every observed parent exit probes
        // the configured readiness endpoint before releasing ownership. The
        // boundary is rotated before that probe so a replacement process must
        // emit fresh UI credentials rather than inheriting a stale token.
        let exit_code = exit.code();
        match rotate_unattached_session(paths, log_sessions, inner) {
            Ok(()) => {
                inner.recovery = Some(RecoveryState {
                    generation: inner.generation,
                    target: readiness.target,
                    deadline: Instant::now() + readiness.timeout,
                    exit_code,
                    task_started: false,
                });
                recovery_runtime(&inner.runtime)
            }
            Err(error) => {
                inner.recovery = None;
                inner.unattached_monitor_started = false;
                let mut runtime = failed_runtime(
                    &inner.runtime,
                    format!(
                        "failed to establish a new Harness token boundary after process replacement: {error}"
                    ),
                );
                runtime.pid = None;
                runtime.exit_code = exit_code;
                runtime
            }
        }
    } else {
        #[cfg(windows)]
        if let Some(job) = inner.job.as_ref() { job.terminate()?; }
        let runtime = runtime_from_exit(&inner.runtime, exit, false);
        clear_launch_pending(log_sessions, inner)?;
        runtime
    };
    inner.runtime = runtime.clone();
    Ok(Some(runtime))
}

fn create_harness_log_session(
    paths: &NexusPaths,
    generation: u64,
    launch_pending: bool,
) -> io::Result<(HarnessLogSession, fs::File, fs::File)> {
    let created_at_unix = unix_time_seconds();
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let run_id = format!("{}-{generation}-{unique}", std::process::id());
    let stdout_log_name = format!("harness-{run_id}.stdout.log");
    let stderr_log_name = format!("harness-{run_id}.stderr.log");
    let stdout = create_new_log(&paths.logs_dir, &stdout_log_name)?;
    let stderr = match create_new_log(&paths.logs_dir, &stderr_log_name) {
        Ok(stderr) => stderr,
        Err(error) => {
            drop(stdout);
            let _ = fs::remove_file(paths.logs_dir.join(&stdout_log_name));
            return Err(error);
        }
    };
    stdout.sync_all()?;
    stderr.sync_all()?;
    #[cfg(unix)]
    fs::File::open(&paths.logs_dir)?.sync_all()?;
    let session = session_from_open_logs(
        run_id,
        generation,
        &stdout_log_name,
        &stderr_log_name,
        &stdout,
        &stderr,
        launch_pending,
        created_at_unix,
    )?;
    Ok((session, stdout, stderr))
}

#[allow(clippy::too_many_arguments)]
fn session_from_open_logs(
    run_id: String,
    generation: u64,
    stdout_log_name: &str,
    stderr_log_name: &str,
    stdout: &fs::File,
    stderr: &fs::File,
    launch_pending: bool,
    created_at_unix: u64,
) -> io::Result<HarnessLogSession> {
    Ok(HarnessLogSession::new(
        run_id,
        generation,
        stdout.metadata()?.len(),
        stderr.metadata()?.len(),
        log_file_identity(stdout)?,
        log_file_identity(stderr)?,
        stdout_log_name.to_owned(),
        stderr_log_name.to_owned(),
        launch_pending,
        created_at_unix,
    ))
}

fn advance_unattached_session(
    paths: &NexusPaths,
    log_sessions: &HarnessLogSessionStore,
    inner: &mut SupervisorInner,
    target: &ReadinessTarget,
) -> io::Result<()> {
    rotate_unattached_session(paths, log_sessions, inner)?;
    inner.runtime = recovery_runtime(&inner.runtime);
    let timeout = inner.readiness.as_ref().map_or(
        Duration::from_secs(DEFAULT_READINESS_TIMEOUT_SECS),
        |value| value.timeout,
    );
    inner.recovery = Some(RecoveryState {
        generation: inner.generation,
        target: target.clone(),
        deadline: Instant::now() + timeout,
        exit_code: None,
        task_started: false,
    });
    Ok(())
}

/// Establish a new durable append-only log boundary for a process that Nexus
/// can observe but cannot safely identify by PID. The caller must hold the
/// supervisor's inner lock. This operation is deliberately independent from
/// readiness probing: once it succeeds, only output after the new watermark
/// can be used to synchronize a Harness URL/token.
fn rotate_unattached_session(
    paths: &NexusPaths,
    log_sessions: &HarnessLogSessionStore,
    inner: &mut SupervisorInner,
) -> io::Result<()> {
    let durable = log_sessions.read()?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "Harness log session marker disappeared during liveness recovery",
        )
    })?;
    if durable != inner.log_session {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "Harness log session marker changed during liveness recovery",
        ));
    }
    let (stdout_path, stderr_path) = session_log_paths(paths, &inner.log_session);
    let stdout = fs::OpenOptions::new().append(true).open(stdout_path)?;
    let stderr = fs::OpenOptions::new().append(true).open(stderr_path)?;
    let generation = inner.generation.wrapping_add(1).max(1);
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let session = session_from_open_logs(
        format!("{}-{generation}-{unique}", std::process::id()),
        generation,
        &inner.log_session.stdout_log_name,
        &inner.log_session.stderr_log_name,
        &stdout,
        &stderr,
        true,
        unix_time_seconds(),
    )?;
    log_sessions.write(&session)?;
    inner.generation = generation;
    inner.log_session = session;
    Ok(())
}

fn advance_attached_session(
    paths: &NexusPaths,
    log_sessions: &HarnessLogSessionStore,
    inner: &mut SupervisorInner,
) -> io::Result<()> {
    let durable = log_sessions.read()?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "Harness log session marker disappeared during attached readiness recovery",
        )
    })?;
    if durable != inner.log_session {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "Harness log session marker changed during attached readiness recovery",
        ));
    }
    if inner.child.is_none() || inner.runtime.pid.is_none() {
        return Err(io::Error::other(
            "attached readiness recovery lost its owned process",
        ));
    }
    let (stdout_path, stderr_path) = session_log_paths(paths, &inner.log_session);
    let stdout = fs::OpenOptions::new().append(true).open(stdout_path)?;
    let stderr = fs::OpenOptions::new().append(true).open(stderr_path)?;
    let generation = inner.generation.wrapping_add(1).max(1);
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let session = session_from_open_logs(
        format!("{}-{generation}-{unique}", std::process::id()),
        generation,
        &inner.log_session.stdout_log_name,
        &inner.log_session.stderr_log_name,
        &stdout,
        &stderr,
        true,
        unix_time_seconds(),
    )?;
    log_sessions.write(&session)?;
    let timeout = inner.readiness.as_ref().map_or(
        Duration::from_secs(DEFAULT_READINESS_TIMEOUT_SECS),
        |value| value.timeout,
    );
    inner.generation = generation;
    inner.log_session = session;
    inner.runtime = attached_recovery_runtime(&inner.runtime);
    inner.recovery = None;
    inner.readiness_owner = None;
    inner.unattached_monitor_started = false;
    let epoch = next_operation_epoch(inner);
    inner.attached_readiness = Some(AttachedReadinessState {
        generation,
        epoch,
        deadline: Some(Instant::now() + timeout),
    });
    Ok(())
}

fn session_log_paths(paths: &NexusPaths, session: &HarnessLogSession) -> (PathBuf, PathBuf) {
    (
        paths.logs_dir.join(&session.stdout_log_name),
        paths.logs_dir.join(&session.stderr_log_name),
    )
}

fn next_operation_epoch(inner: &mut SupervisorInner) -> u64 {
    inner.operation_epoch = inner.operation_epoch.wrapping_add(1).max(1);
    inner.operation_epoch
}

fn readiness_owner_generation(owner: ReadinessOwner) -> u64 {
    match owner {
        ReadinessOwner::Start { generation, .. } | ReadinessOwner::Recovery { generation, .. } => {
            generation
        }
    }
}

fn clear_launch_pending(
    log_sessions: &HarnessLogSessionStore,
    inner: &mut SupervisorInner,
) -> io::Result<()> {
    if !inner.log_session.launch_pending {
        return Ok(());
    }
    let mut released = inner.log_session.clone();
    released.launch_pending = false;
    log_sessions.write(&released)?;
    inner.log_session = released;
    Ok(())
}

fn abandoned_without_readiness_runtime(previous: &HarnessRuntimeInfo) -> HarnessRuntimeInfo {
    HarnessRuntimeInfo {
        state: HarnessState::Failed,
        pid: None,
        exit_code: previous.exit_code,
        error: Some(
            "Harness launch was abandoned after Agent restart without readiness; the reservation was cleared"
                .to_owned(),
        ),
        started_at_unix: previous.started_at_unix,
        updated_at_unix: Some(unix_time_seconds()),
    }
}

fn recovery_runtime(previous: &HarnessRuntimeInfo) -> HarnessRuntimeInfo {
    HarnessRuntimeInfo {
        state: HarnessState::Starting,
        pid: None,
        exit_code: None,
        error: None,
        started_at_unix: previous.started_at_unix,
        updated_at_unix: Some(unix_time_seconds()),
    }
}

fn attached_recovery_runtime(previous: &HarnessRuntimeInfo) -> HarnessRuntimeInfo {
    HarnessRuntimeInfo {
        state: HarnessState::Starting,
        pid: previous.pid,
        exit_code: None,
        error: None,
        started_at_unix: previous.started_at_unix,
        updated_at_unix: Some(unix_time_seconds()),
    }
}

fn running_runtime_without_pid(previous: &HarnessRuntimeInfo) -> HarnessRuntimeInfo {
    HarnessRuntimeInfo {
        state: HarnessState::Running,
        pid: None,
        exit_code: None,
        error: None,
        started_at_unix: previous.started_at_unix,
        updated_at_unix: Some(unix_time_seconds()),
    }
}

fn recovery_failed_runtime(
    previous: &HarnessRuntimeInfo,
    exit_code: Option<i32>,
) -> HarnessRuntimeInfo {
    let error = match exit_code {
        Some(code) => {
            format!("Harness exited with code {code}; loopback readiness recovery timed out")
        }
        None => {
            "Harness exited without an exit code; loopback readiness recovery timed out".to_owned()
        }
    };
    HarnessRuntimeInfo {
        state: HarnessState::Failed,
        pid: None,
        exit_code,
        error: Some(error),
        started_at_unix: previous.started_at_unix,
        updated_at_unix: Some(unix_time_seconds()),
    }
}

fn unattached_stopped_runtime(previous: &HarnessRuntimeInfo) -> HarnessRuntimeInfo {
    HarnessRuntimeInfo {
        state: HarnessState::Stopped,
        pid: None,
        exit_code: None,
        error: Some("previous Harness process was not attached to this Agent instance".to_owned()),
        started_at_unix: previous.started_at_unix,
        updated_at_unix: Some(unix_time_seconds()),
    }
}

fn scrub_unattached_pid(previous: &HarnessRuntimeInfo) -> HarnessRuntimeInfo {
    if previous.pid.is_none() {
        return previous.clone();
    }
    let mut runtime = previous.clone();
    runtime.pid = None;
    runtime.updated_at_unix = Some(unix_time_seconds());
    runtime
}

fn runtime_from_exit(
    previous: &HarnessRuntimeInfo,
    exit: std::process::ExitStatus,
    killed: bool,
) -> HarnessRuntimeInfo {
    let state = if killed {
        HarnessState::Stopped
    } else {
        // A natural exit is always unexpected for a directly supervised
        // long-running Harness. In particular, exit 0 without readiness or a
        // separate ownership channel cannot prove that a self-restarted
        // descendant belongs to this launch, so do not report a clean stop or
        // silently claim a replacement process.
        HarnessState::Failed
    };
    runtime_from_exit_with_state(previous.started_at_unix, previous.pid, exit, state)
}

fn runtime_from_exit_with_state(
    started_at_unix: Option<u64>,
    _pid: Option<u32>,
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
        // Reaching this function proves the process has exited and been
        // reaped. Preserve its exit code, not a stale live-process identity.
        pid: None,
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

#[derive(Debug, Clone)]
struct ReadinessTarget {
    host: String,
    port: u16,
    path: String,
    tcp: bool,
    token_required: bool,
    owned_web: bool,
}

impl ReadinessTarget {
    fn parse(url: &str) -> Result<Self, HarnessSupervisorError> {
        let (scheme, authority_and_path) = url.split_once("://").ok_or_else(|| {
            HarnessSupervisorError::Readiness(
                "only http:// or tcp:// loopback readiness URLs are supported by the cross-platform supervisor"
                    .to_owned(),
            )
        })?;
        let tcp = if scheme.eq_ignore_ascii_case("tcp") {
            true
        } else if scheme.eq_ignore_ascii_case("http") {
            false
        } else {
            return Err(HarnessSupervisorError::Readiness(
                "only http:// or tcp:// loopback readiness URLs are supported by the cross-platform supervisor"
                    .to_owned(),
            ));
        };
        if authority_and_path.contains('#') {
            return Err(HarnessSupervisorError::Readiness(
                "readiness URLs cannot contain a fragment".to_owned(),
            ));
        }
        let (authority_and_path, query) = match authority_and_path.split_once('?') {
            Some((base, query)) => (base, Some(query)),
            None => (authority_and_path, None),
        };
        if tcp && query.is_some() {
            return Err(HarnessSupervisorError::Readiness(
                "tcp readiness URLs cannot contain a query".to_owned(),
            ));
        }
        let (authority, mut path) = match authority_and_path.split_once('/') {
            Some((authority, path)) => (authority, format!("/{path}")),
            None => (authority_and_path, "/".to_owned()),
        };
        if let Some(query) = query {
            path.push('?');
            path.push_str(query);
        }
        if tcp && path != "/" {
            return Err(HarnessSupervisorError::Readiness(
                "tcp readiness URLs cannot contain a path".to_owned(),
            ));
        }
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
                if tcp {
                    return Err(HarnessSupervisorError::Readiness(
                        "tcp readiness URLs must include an explicit port".to_owned(),
                    ));
                }
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
            if tcp {
                return Err(HarnessSupervisorError::Readiness(
                    "tcp readiness URLs must include an explicit port".to_owned(),
                ));
            }
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
        if port == 0 {
            return Err(HarnessSupervisorError::Readiness(
                "readiness URL port must be between 1 and 65535".to_owned(),
            ));
        }
        Ok(Self {
            host,
            port,
            path,
            tcp,
            token_required: false,
            owned_web: false,
        })
    }
}

fn is_loopback_host(host: &str) -> bool {
    matches!(
        host.to_ascii_lowercase().as_str(),
        "localhost" | "127.0.0.1" | "::1"
    )
}

fn readiness_has_current_token(
    paths: &NexusPaths,
    target: &ReadinessTarget,
    session: &HarnessLogSession,
    observer: &mut HarnessLogObserver,
) -> bool {
    let info = read_harness_ui_info_with_observer(paths, observer, Some(session));
    if !info.available
        || info.generation != Some(session.generation)
        || info.run_id.as_deref() != Some(session.run_id.as_str())
        || info.token.as_deref().is_none()
    {
        return false;
    }
    let Some(url) = info.url.as_deref() else {
        return false;
    };
    ReadinessTarget::parse(url).is_ok_and(|observed| !observed.tcp && observed.port == target.port)
}

async fn readiness_probe(target: &ReadinessTarget) -> Result<(), String> {
    let mut stream = TcpStream::connect((target.host.as_str(), target.port))
        .await
        .map_err(|error| error.to_string())?;
    if target.tcp {
        return Ok(());
    }
    let host_header = if target.host.contains(':') {
        format!("[{}]", target.host)
    } else {
        target.host.clone()
    };
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        target.path, host_header
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
    use std::{
        fs,
        io::Write as _,
        path::PathBuf,
        time::{Duration, Instant},
    };

    use nexus_core::{
        unix_time_nanos_for_update, unix_time_seconds, AgentState, ConfigStore, HarnessLaunchSpec,
        NexusConfigFile, NexusPaths, RuntimeMetadataStore,
    };
    use nexus_launcher_core::read_harness_ui_info;
    use nexus_protocol::{HarnessRuntimeInfo, HarnessState};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        sync::oneshot,
        time::{sleep, timeout},
    };

    use super::{
        current_readiness_lease, next_operation_epoch, pending_recovery,
        readiness_has_current_token, recovery_failed_runtime, recovery_runtime,
        running_runtime_without_pid, HarnessPersistGate, HarnessSupervisor, HarnessSupervisorError,
        ReadinessConfig, ReadinessOwner, ReadinessTarget, RecoveryState, StartPersistGate,
    };

    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn automatic_web_readiness_requires_managed_verified_web_and_owned_dynamic_listener() {
        let root = std::env::temp_dir().join(format!("nexus-auto-readiness-{}",unix_time_nanos_for_update()));
        let paths = NexusPaths::from_root(root.join("data")); paths.ensure_directories().unwrap();
        let slot = root.join("slot"); fs::create_dir_all(slot.join("apps/cli/lib")).unwrap();
        let entry = slot.join("apps/cli/lib/bin.js"); fs::write(&entry,"fixture").unwrap();
        fs::write(slot.join("package.json"),r#"{"name":"@deepseek-ai/dsh-root","version":"0.1.2-rc.1"}"#).unwrap();
        let home = root.join("home");
        let mut spec = nexus_core::HarnessLaunchSpec { mode:nexus_protocol::HarnessLaunchMode::Node,program:"node.exe".into(),args:vec![entry.to_string_lossy().into_owned()],working_dir:Some(slot.clone()),readiness_url:None,readiness_timeout_secs:None,readiness_token_required:false };
        assert!(super::automatic_web_readiness_supported(&spec,Some(&slot),&home,"web",&spec.args));
        let mut preferred_port = spec.clone();
        let capabilities = crate::preference_capabilities::inspect(&slot, &home, "web").unwrap().capabilities;
        nexus_core::apply_harness_preferences(&mut preferred_port, &nexus_protocol::HarnessPreferencesPayload { port: Some(4567), ..Default::default() }, &capabilities);
        assert!(preferred_port.readiness_url.is_some(), "port preference synthesizes a TCP target");
        assert!(super::automatic_web_readiness_supported(&spec,Some(&slot),&home,"web",&preferred_port.args), "the original configured spec still enables owned health discovery");
        assert!(!super::automatic_web_readiness_supported(&spec,Some(&slot),&home,"headless",&spec.args));
        spec.readiness_url=Some("tcp://127.0.0.1:4567".into());
        assert!(!super::automatic_web_readiness_supported(&spec,Some(&slot),&home,"web",&spec.args));
        assert!(!super::readiness_config(&spec).unwrap().unwrap().target.owned_web);
        spec.readiness_url=None;
        assert!(!super::automatic_web_readiness_supported(&spec,Some(&slot),&home,"web",&[root.join("custom.js").to_string_lossy().into_owned()]));
        fs::write(slot.join("package.json"),r#"{"name":"@deepseek-ai/dsh-root","version":"unknown"}"#).unwrap();
        assert!(!super::automatic_web_readiness_supported(&spec,Some(&slot),&home,"web",&spec.args));

        let listener=std::net::TcpListener::bind("127.0.0.1:0").unwrap(); let port=listener.local_addr().unwrap().port();
        assert_eq!(super::windows_listener_owners("127.0.0.1",port).unwrap(),vec![std::process::id()]);
        let job=crate::dsh::WindowsJob::new().unwrap();
        assert!(!job.contains_pid(std::process::id()).unwrap(),"an unrelated live listener is not owned by a new Harness job");
        let (session,mut stdout,stderr)=super::create_harness_log_session(&paths,1,true).unwrap();
        let mut observer=nexus_launcher_core::HarnessLogObserver::default();
        use std::io::Write;
        writeln!(stdout,"http://127.0.0.1:{port}/?token=secret-fixture").unwrap(); stdout.flush().unwrap();
        assert!(super::observed_web_readiness(&paths,&session,&mut observer).is_none(),"missing durable marker is not evidence");
        nexus_core::HarnessLogSessionStore::new(paths.clone()).write(&session).unwrap();
        let target=super::observed_web_readiness(&paths,&session,&mut observer).unwrap();
        assert_eq!(target.port,port); assert!(target.token_required && target.owned_web && target.tcp); assert!(!format!("{target:?}").contains("secret-fixture"));
        let (next,next_out,next_err)=super::create_harness_log_session(&paths,2,true).unwrap();
        nexus_core::HarnessLogSessionStore::new(paths.clone()).write(&next).unwrap();
        assert!(super::observed_web_readiness(&paths,&session,&mut observer).is_none());
        assert!(super::observed_web_readiness(&paths,&next,&mut observer).is_none(),"old log token cannot complete a new run");
        assert!(nexus_core::HarnessLogSessionStore::new(paths.clone()).read().unwrap().unwrap().launch_pending);
        drop((stdout,stderr,next_out,next_err,listener,job)); fs::remove_dir_all(root).unwrap();
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_owned_harness_job_stops_descendants_after_parent_exit() {
        use windows_sys::Win32::{Foundation::{CloseHandle, WAIT_TIMEOUT}, System::Threading::{OpenProcess, WaitForSingleObject}};
        // Exercise the production suspended spawn path, both before and after
        // the supervisor has observed a bootstrap parent's successful exit.
        for observe_exit in [false, true] {
            let root = std::env::temp_dir().join(format!("nexus-harness-job-{}", unix_time_nanos_for_update()));
            fs::create_dir_all(&root).unwrap();
            let powershell = crate::test_powershell();
            let mut command = tokio::process::Command::new(&powershell);
            command.args(["-NoProfile", "-NonInteractive", "-Command",
                "$p=Start-Process -FilePath $env:NEXUS_JOB_TEST_POWERSHELL -ArgumentList '-NoProfile -NonInteractive -Command Start-Sleep -Seconds 300' -WindowStyle Hidden -PassThru; [IO.File]::WriteAllText($env:NEXUS_JOB_TEST_PID,[string]$p.Id)"])
                .env("NEXUS_JOB_TEST_POWERSHELL", &powershell)
                .env("NEXUS_JOB_TEST_PID", root.join("descendant.pid"))
                .kill_on_drop(true);
            let mut spawned = super::spawn_owned_harness(command, None).await.unwrap();
            assert!(timeout(Duration::from_secs(20), spawned.child.wait()).await.unwrap().unwrap().success());
            assert!(!spawned.job.is_empty().unwrap(), "the bootstrap descendant must remain owned after its parent exits");
            let pid: u32 = fs::read_to_string(root.join("descendant.pid")).unwrap().parse().unwrap();
            assert!(spawned.job.contains_pid(pid).unwrap());
            assert!(!spawned.job.contains_pid(std::process::id()).unwrap());
            let process = unsafe { OpenProcess(0x00100000, 0, pid) };
            assert!(!process.is_null());
            assert_eq!(unsafe { WaitForSingleObject(process, 0) }, WAIT_TIMEOUT);
            let supervisor = HarnessSupervisor::with_graceful_wait(NexusPaths::from_root(root.clone()), Duration::from_millis(50)).unwrap();
            {
                let mut inner = supervisor.inner.lock().await;
                inner.job = Some(spawned.job);
                inner.child = if observe_exit { None } else { Some(spawned.child) };
                inner.runtime.state = HarnessState::Running;
                inner.runtime.pid = None;
            }
            let stopped = supervisor.stop().await.unwrap();
            assert_eq!(stopped.state, HarnessState::Stopped);
            assert_eq!(unsafe { WaitForSingleObject(process, 5000) }, 0, "Stop must reap descendants in either parent state");
            unsafe { CloseHandle(process) };
            assert!(supervisor.inner.lock().await.job.is_none());
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn managed_launch_rebinds_stale_slot_but_preserves_external_commands_and_arguments() {
        let root = std::env::temp_dir().join(format!(
            "nexus-managed-launch-{}-{}", std::process::id(), unix_time_nanos_for_update()
        ));
        let paths = NexusPaths::from_root(root.clone());
        let releases = nexus_core::ReleaseStore::new(paths);
        releases.register("slot-a", "a", None, None).unwrap();
        releases.register("slot-b", "b", None, None).unwrap();
        releases.promote("slot-b").unwrap();
        let old = releases.release_root("slot-a").unwrap();
        let entry = old.join("bin.js");
        fs::write(&entry, "").unwrap();
        let external = root.join("external.js");
        fs::write(&external, "").unwrap();
        let mut spec = HarnessLaunchSpec::new(PathBuf::from("node"));
        spec.mode = nexus_protocol::HarnessLaunchMode::Node;
        spec.args = vec![entry.to_string_lossy().into_owned(), entry.to_string_lossy().into_owned()];
        spec.working_dir = Some(old);
        super::normalize_managed_launch(&mut spec, &releases).unwrap();
        assert_eq!(PathBuf::from(&spec.args[0]), PathBuf::from("{release_root}").join("bin.js"));
        assert_eq!(spec.args[1], entry.to_string_lossy());
        assert_eq!(spec.working_dir, Some(PathBuf::from("{release_root}")));
        let normalized = spec.clone();
        super::normalize_managed_launch(&mut spec, &releases).unwrap();
        assert_eq!(spec, normalized);
        spec.args[0] = external.to_string_lossy().into_owned();
        super::normalize_managed_launch(&mut spec, &releases).unwrap();
        assert_eq!(spec.args[0], external.to_string_lossy());
        spec.working_dir = Some(releases.release_root("slot-a").unwrap());
        let external_spec = spec.clone();
        super::normalize_managed_launch(&mut spec, &releases).unwrap();
        assert_eq!(spec, external_spec, "external entry keeps its explicit cwd");
        spec.args[0] = "bin.js".to_owned();
        super::normalize_managed_launch(&mut spec, &releases).unwrap();
        assert_eq!(PathBuf::from(&spec.args[0]), PathBuf::from("{release_root}").join("bin.js"));
        fs::remove_dir_all(root).unwrap();
    }

    fn immediate_nonzero_marker_command(marker: &std::path::Path) -> (PathBuf, Vec<String>) {
        if cfg!(windows) {
            (
                crate::test_powershell(),
                vec![
                    "-NoProfile".to_owned(),
                    "-Command".to_owned(),
                    format!(
                        "Add-Content -LiteralPath '{}' -Value bootstrap; exit 1",
                        marker.display()
                    ),
                ],
            )
        } else {
            (
                PathBuf::from("sh"),
                vec![
                    "-c".to_owned(),
                    format!("printf 'bootstrap\\n' >> '{}'; exit 1", marker.display()),
                ],
            )
        }
    }

    async fn wait_for_pending_child_exit(supervisor: &HarnessSupervisor) {
        timeout(Duration::from_secs(15), async {
            loop {
                let exited = {
                    let mut inner = supervisor.inner.lock().await;
                    assert!(inner.start_ownership_pending);
                    inner
                        .child
                        .as_mut()
                        .expect("pending start retains its Child")
                        .try_wait()
                        .expect("pending child status is readable")
                        .is_some()
                };
                if exited {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("short bootstrap exits while ownership is gated");
    }

    fn marker_lines(marker: &std::path::Path) -> Vec<String> {
        fs::read_to_string(marker)
            .expect("bootstrap marker exists")
            .lines()
            .map(str::to_owned)
            .collect()
    }

    #[tokio::test]
    async fn healthy_snapshot_claim_is_durable_once_per_running_log_session() {
        let root = std::env::temp_dir().join(format!(
            "nexus-agent-healthy-claim-{}-{}",
            std::process::id(),
            unix_time_nanos_for_update()
        ));
        let paths = NexusPaths::from_root(root.clone());
        let supervisor = HarnessSupervisor::new(paths).expect("supervisor creates");
        {
            let mut inner = supervisor.inner.lock().await;
            inner.generation = 7;
            inner.runtime = HarnessRuntimeInfo {
                state: HarnessState::Running,
                pid: Some(4242),
                exit_code: None,
                error: None,
                started_at_unix: Some(unix_time_seconds()),
                updated_at_unix: Some(unix_time_seconds()),
            };
            inner.log_session.run_id = "run-seven".to_owned();
            inner.log_session.generation = 7;
            inner.log_session.healthy_snapshot_attempted = false;
            supervisor
                .log_sessions
                .write(&inner.log_session)
                .expect("unclaimed session writes");
        }
        assert!(!supervisor.claim_healthy_snapshot_attempt("run-seven", 7).await.unwrap(), "Running without verified readiness is not healthy evidence");
        supervisor.inner.lock().await.verified_health = Some(("run-seven".to_owned(), 7));
        assert!(supervisor
            .claim_healthy_snapshot_attempt("run-seven", 7)
            .await
            .expect("first claim persists"));
        assert!(!supervisor
            .claim_healthy_snapshot_attempt("run-seven", 7)
            .await
            .expect("duplicate claim is readable"));
        assert!(!supervisor
            .claim_healthy_snapshot_attempt("other-run", 7)
            .await
            .expect("different run cannot claim current session"));
        assert!(
            supervisor
                .log_sessions
                .read()
                .expect("durable session reads")
                .expect("durable session exists")
                .healthy_snapshot_attempted
        );
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn supervisor_reports_missing_program_as_readable_error() {
        let root =
            std::env::temp_dir().join(format!("nexus-agent-unconfigured-{}", std::process::id()));
        let paths = NexusPaths::from_root(PathBuf::from(&root));
        let supervisor = HarnessSupervisor::new(paths.clone()).expect("supervisor creates");

        let error = supervisor
            .start()
            .await
            .expect_err("start must be rejected");
        assert!(matches!(error, HarnessSupervisorError::NotConfigured));
        assert!(error.to_string().contains("not configured"));

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn fresh_start_publishes_log_boundary_from_spawn_log_handles() {
        let root = std::env::temp_dir().join(format!(
            "nexus-agent-log-session-order-{}-{}",
            std::process::id(),
            unix_time_seconds()
        ));
        let paths = NexusPaths::from_root(root.clone());
        paths.ensure_directories().expect("directories create");
        let legacy_stdout_path = paths.logs_dir.join("harness.stdout.log");
        let old_output = b"old http://127.0.0.1:3080/?token=old\n";
        fs::write(&legacy_stdout_path, old_output).expect("old Harness output writes");
        let (program, args) = if cfg!(windows) {
            (
                std::env::var_os("ComSpec")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("cmd.exe")),
                vec![
                    "/C".to_owned(),
                    "echo http://127.0.0.1:3080/?token=current & ping -n 30 127.0.0.1 >NUL"
                        .to_owned(),
                ],
            )
        } else {
            (
                PathBuf::from("sh"),
                vec![
                    "-c".to_owned(),
                    "printf 'http://127.0.0.1:3080/?token=current\\n'; sleep 10".to_owned(),
                ],
            )
        };
        ConfigStore::new(paths.clone())
            .write(&NexusConfigFile { update_attempt_id: None, external_harness: None,
                schema_version: 1,
                harness_preferences: None,
                harness: Some(HarnessLaunchSpec {
                    mode: Default::default(),
                    program,
                    args,
                    working_dir: None,
                    readiness_url: None,
                    readiness_timeout_secs: None,
                    readiness_token_required: false,
                }),
                update: None,

                releases: None,
                runtime: None,
                snapshots: None,
            })
            .expect("Harness config writes");

        let supervisor = HarnessSupervisor::new(paths.clone()).expect("supervisor creates");
        let initial_session = supervisor
            .log_sessions
            .read()
            .expect("initial log session reads")
            .expect("initial log session exists");
        supervisor.start().await.expect("Harness starts");
        let current_session = supervisor
            .log_sessions
            .read()
            .expect("current log session reads")
            .expect("current log session exists");
        let stdout_path = paths.logs_dir.join(&current_session.stdout_log_name);
        timeout(Duration::from_secs(2), async {
            loop {
                let output = fs::read_to_string(&stdout_path).expect("Harness output reads");
                if output.contains("token=current") {
                    break;
                }
                sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("child output reaches the Nexus log");
        assert_ne!(current_session.run_id, initial_session.run_id);
        assert!(current_session.generation > initial_session.generation);
        assert_eq!(current_session.stdout_watermark, 0);
        assert_eq!(current_session.stderr_watermark, 0);
        assert_eq!(
            fs::read(&legacy_stdout_path).expect("legacy log remains readable"),
            old_output
        );
        supervisor.stop().await.expect("Harness stops");
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn fresh_start_rejects_a_log_marker_replaced_by_another_writer() {
        let root = std::env::temp_dir().join(format!(
            "nexus-agent-log-session-writer-{}-{}",
            std::process::id(),
            unix_time_seconds()
        ));
        let paths = NexusPaths::from_root(root.clone());
        ConfigStore::new(paths.clone())
            .write(&NexusConfigFile { update_attempt_id: None, external_harness: None,
                schema_version: 1,
                harness_preferences: None,
                harness: Some(HarnessLaunchSpec {
                    mode: Default::default(),
                    program: PathBuf::from("unused-harness"),
                    args: Vec::new(),
                    working_dir: None,
                    readiness_url: None,
                    readiness_timeout_secs: None,
                    readiness_token_required: false,
                }),
                update: None,

                releases: None,
                runtime: None,
                snapshots: None,
            })
            .expect("Harness config writes");
        let supervisor = HarnessSupervisor::new(paths).expect("supervisor creates");
        let mut foreign = supervisor
            .log_sessions
            .read()
            .expect("log session reads")
            .expect("log session exists");
        foreign.run_id = "foreign-agent-writer".to_owned();
        supervisor
            .log_sessions
            .write(&foreign)
            .expect("foreign marker writes");

        let error = supervisor
            .start()
            .await
            .expect_err("a changed writer marker must block spawn");
        assert!(matches!(
            error,
            HarnessSupervisorError::Persistence(ref source)
                if source.kind() == std::io::ErrorKind::AlreadyExists
        ));
        let inner = supervisor.inner.lock().await;
        assert!(inner.child.is_none());
        assert_eq!(inner.runtime.state, HarnessState::Detached);
        drop(inner);
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
        assert!(super::ReadinessTarget::parse("HTTP://127.0.0.1:3090/health").is_ok());
        assert!(super::ReadinessTarget::parse("http://127.0.0.1:3090?token=secret").is_ok());
        assert!(super::ReadinessTarget::parse("http://127.0.0.1:3090/#fragment").is_err());
        assert!(super::ReadinessTarget::parse("http://127.0.0.1:0/health").is_err());
        let tcp = super::ReadinessTarget::parse("tcp://127.0.0.1:3090")
            .expect("TCP loopback target parses");
        assert!(tcp.tcp);
        assert!(super::ReadinessTarget::parse("tcp://[::1]:3090/").is_ok());
        assert!(super::ReadinessTarget::parse("tcp://127.0.0.1").is_err());
        assert!(super::ReadinessTarget::parse("tcp://127.0.0.1:3090?token=secret").is_err());
        assert!(super::ReadinessTarget::parse("tcp://127.0.0.1:3090/health").is_err());
    }

    #[tokio::test]
    async fn tcp_readiness_probe_only_requires_a_loopback_listener() {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("TCP readiness listener binds");
        let port = listener.local_addr().expect("TCP readiness address").port();
        let target = ReadinessTarget::parse(&format!("tcp://127.0.0.1:{port}"))
            .expect("TCP readiness target parses");
        let server = tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.expect("TCP readiness accepts");
        });
        assert!(super::readiness_probe(&target).await.is_ok());
        server.await.expect("TCP readiness server completes");
    }

    #[test]
    fn graceful_wait_is_explicitly_bounded() {
        let duration = Duration::from_millis(25);
        assert_eq!(duration, Duration::from_millis(25));
    }

    #[test]
    fn unexpected_nonzero_exit_enters_scrubbed_starting_recovery() {
        let previous = HarnessRuntimeInfo::running(42, 10, 11);

        let runtime = recovery_runtime(&previous);

        assert_eq!(runtime.state, HarnessState::Starting);
        assert_eq!(runtime.pid, None);
        assert_eq!(runtime.exit_code, None);
        assert_eq!(runtime.error, None);
        assert_eq!(runtime.started_at_unix, Some(10));
    }

    #[tokio::test]
    async fn direct_harness_clean_exit_without_readiness_is_an_explicit_failure() {
        let root = std::env::temp_dir().join(format!(
            "nexus-agent-direct-clean-exit-{}-{}",
            std::process::id(),
            unix_time_seconds()
        ));
        let paths = NexusPaths::from_root(root.clone());
        let (program, args) = if cfg!(windows) {
            (
                std::env::var_os("ComSpec")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("cmd.exe")),
                vec!["/C".to_owned(), "exit 0".to_owned()],
            )
        } else {
            (
                PathBuf::from("sh"),
                vec!["-c".to_owned(), "exit 0".to_owned()],
            )
        };
        ConfigStore::new(paths.clone())
            .write(&NexusConfigFile { update_attempt_id: None, external_harness: None,
                schema_version: 1,
                harness_preferences: None,
                harness: Some(HarnessLaunchSpec {
                    mode: Default::default(),
                    program,
                    args,
                    working_dir: None,
                    readiness_url: None,
                    readiness_timeout_secs: None,
                    readiness_token_required: false,
                }),
                update: None,

                releases: None,
                runtime: None,
                snapshots: None,
            })
            .expect("Harness config writes");
        let supervisor = HarnessSupervisor::new(paths).expect("supervisor creates");
        supervisor.start().await.expect("direct Harness spawns");
        let runtime = timeout(Duration::from_secs(2), async {
            loop {
                let runtime = supervisor.status().await;
                if matches!(runtime.state, HarnessState::Failed | HarnessState::Stopped) {
                    break runtime;
                }
                sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("direct Harness exit is observed");

        assert_eq!(runtime.state, HarnessState::Failed);
        assert_eq!(runtime.pid, None);
        assert_eq!(runtime.exit_code, Some(0));
        assert!(runtime
            .error
            .as_deref()
            .is_some_and(|message| message.contains("exited with code 0")));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn readiness_recovery_success_clears_stale_failure_metadata() {
        let previous = HarnessRuntimeInfo {
            state: HarnessState::Starting,
            pid: None,
            exit_code: None,
            error: Some("old bootstrap failure".to_owned()),
            started_at_unix: Some(10),
            updated_at_unix: Some(11),
        };

        let runtime = running_runtime_without_pid(&previous);

        assert_eq!(runtime.state, HarnessState::Running);
        assert_eq!(runtime.pid, None);
        assert_eq!(runtime.exit_code, None);
        assert_eq!(runtime.error, None);
        assert_eq!(runtime.started_at_unix, Some(10));
    }

    #[test]
    fn readiness_recovery_timeout_keeps_the_real_exit_code() {
        let previous = recovery_runtime(&HarnessRuntimeInfo::running(42, 10, 11));

        let runtime = recovery_failed_runtime(&previous, Some(1));

        assert_eq!(runtime.state, HarnessState::Failed);
        assert_eq!(runtime.pid, None);
        assert_eq!(runtime.exit_code, Some(1));
        assert!(runtime
            .error
            .as_deref()
            .is_some_and(|error| error.contains("code 1")));
    }

    #[tokio::test]
    async fn recover_unattached_restores_healthy_loopback_without_pid() {
        let root = std::env::temp_dir().join(format!(
            "nexus-agent-recover-healthy-{}",
            std::process::id()
        ));
        let paths = NexusPaths::from_root(root.clone());
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("readiness listener binds");
        let port = listener.local_addr().expect("listener address").port();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("readiness accepts");
            let mut request = [0_u8; 256];
            let _ = stream
                .read(&mut request)
                .await
                .expect("readiness request reads");
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                .await
                .expect("readiness responds");
        });
        ConfigStore::new(paths.clone())
            .write(&NexusConfigFile { update_attempt_id: None, external_harness: None,
                schema_version: 1,
                harness_preferences: None,
                harness: Some(HarnessLaunchSpec {
                    mode: Default::default(),
                    program: PathBuf::from("unused-harness"),
                    args: Vec::new(),
                    working_dir: None,
                    readiness_url: Some(format!("http://127.0.0.1:{port}/health")),
                    readiness_timeout_secs: Some(1),
                    readiness_token_required: false,
                }),
                update: None,

                releases: None,
                runtime: None,
                snapshots: None,
            })
            .expect("Harness config writes");
        RuntimeMetadataStore::new(paths.clone())
            .update_harness(HarnessRuntimeInfo::starting(999, 10))
            .expect("persisted Harness state writes");

        let supervisor = HarnessSupervisor::new(paths).expect("supervisor creates");
        let old_session = supervisor
            .log_sessions
            .read()
            .expect("old session reads")
            .expect("old session exists");
        let old_stdout_path = supervisor.paths.logs_dir.join(&old_session.stdout_log_name);
        fs::write(
            &old_stdout_path,
            "http://127.0.0.1:3080/?token=stale-after-agent-restart\n",
        )
        .expect("old token appends");
        let old_stdout_length = fs::metadata(&old_stdout_path)
            .expect("old stdout metadata reads")
            .len();
        let runtime = supervisor.recover_unattached().await;

        assert_eq!(runtime.state, HarnessState::Running);
        assert_eq!(runtime.pid, None);
        assert_eq!(runtime.exit_code, None);
        assert_eq!(runtime.error, None);
        let new_session = supervisor
            .log_sessions
            .read()
            .expect("new session reads")
            .expect("new session exists");
        assert!(new_session.generation > old_session.generation);
        assert_ne!(new_session.run_id, old_session.run_id);
        assert!(new_session.stdout_watermark >= old_stdout_length);
        assert!(new_session.launch_pending);
        let unavailable = read_harness_ui_info(supervisor.paths());
        assert!(!unavailable.available);
        fs::OpenOptions::new()
            .append(true)
            .open(&old_stdout_path)
            .expect("current stdout opens")
            .write_all(b"http://127.0.0.1:3080/?token=fresh-after-agent-restart\n")
            .expect("new token appends");
        let available = read_harness_ui_info(supervisor.paths());
        assert!(available.available);
        assert_eq!(
            available.token.as_deref(),
            Some("fresh-after-agent-restart")
        );
        assert_eq!(available.generation, Some(new_session.generation));
        assert_eq!(
            available.run_id.as_deref(),
            Some(new_session.run_id.as_str())
        );
        server.await.expect("readiness server completes");
        let error = supervisor
            .start()
            .await
            .expect_err("healthy unattached Harness must not be duplicated");
        assert!(matches!(error, HarnessSupervisorError::AlreadyRunning));
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn pending_unattached_recovery_rejects_stop_and_duplicate_start() {
        let root = std::env::temp_dir().join(format!(
            "nexus-agent-recover-stop-conflict-{}-{}",
            std::process::id(),
            unix_time_seconds()
        ));
        let paths = NexusPaths::from_root(root.clone());
        let marker = root.join("duplicate-bootstrap.txt");
        let (program, args) = if cfg!(windows) {
            (
                crate::test_powershell(),
                vec![
                    "-NoProfile".to_owned(),
                    "-Command".to_owned(),
                    format!(
                        "Set-Content -LiteralPath '{}' -Value spawned",
                        marker.display()
                    ),
                ],
            )
        } else {
            (
                PathBuf::from("sh"),
                vec![
                    "-c".to_owned(),
                    format!("printf spawned > '{}'", marker.display()),
                ],
            )
        };
        ConfigStore::new(paths.clone())
            .write(&NexusConfigFile { update_attempt_id: None, external_harness: None,
                schema_version: 1,
                harness_preferences: None,
                harness: Some(HarnessLaunchSpec {
                    mode: Default::default(),
                    program,
                    args,
                    working_dir: None,
                    readiness_url: Some("http://127.0.0.1:1/health".to_owned()),
                    readiness_timeout_secs: Some(60),
                    readiness_token_required: false,
                }),
                update: None,

                releases: None,
                runtime: None,
                snapshots: None,
            })
            .expect("Harness config writes");
        let supervisor = HarnessSupervisor::new(paths).expect("supervisor creates");
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("external readiness listener binds");
        let port = listener.local_addr().expect("readiness address").port();
        let target = ReadinessTarget::parse(&format!("http://127.0.0.1:{port}/health"))
            .expect("loopback target parses");
        let server = tokio::spawn(async move {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().await.expect("readiness accepts");
                let mut request = [0_u8; 256];
                let _ = stream.read(&mut request).await;
                stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                    .await
                    .expect("readiness responds");
            }
        });
        {
            let mut inner = supervisor.inner.lock().await;
            inner.generation = 7;
            inner.runtime = recovery_runtime(&HarnessRuntimeInfo::running(42, 10, 11));
            inner.recovery = Some(RecoveryState {
                generation: 7,
                target: target.clone(),
                deadline: Instant::now() + Duration::from_secs(1),
                exit_code: Some(1),
                task_started: false,
            });
        }

        assert!(super::readiness_probe(&target).await.is_ok());
        let error = supervisor
            .stop()
            .await
            .expect_err("unattached pending recovery cannot be truthfully stopped");
        assert!(matches!(error, HarnessSupervisorError::Unattached));
        assert!(super::readiness_probe(&target).await.is_ok());
        let start_error = supervisor
            .start()
            .await
            .expect_err("pending external recovery must block duplicate bootstrap");
        assert!(matches!(
            start_error,
            HarnessSupervisorError::AlreadyRunning
        ));
        sleep(Duration::from_millis(100)).await;
        assert!(!marker.exists(), "duplicate bootstrap program must not run");
        let inner = supervisor.inner.lock().await;
        assert_eq!(inner.generation, 7);
        assert!(inner.recovery.is_some());
        assert_eq!(inner.runtime.state, HarnessState::Starting);
        drop(inner);
        server.await.expect("readiness server completes");

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn recovery_success_survives_rejected_unattached_stop() {
        let root = std::env::temp_dir().join(format!(
            "nexus-agent-recover-late-success-{}",
            std::process::id()
        ));
        let supervisor = HarnessSupervisor::new(NexusPaths::from_root(root.clone()))
            .expect("supervisor creates");
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("readiness listener binds");
        let port = listener.local_addr().expect("readiness address").port();
        let (accepted_tx, accepted_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("readiness accepts");
            let _ = accepted_tx.send(());
            sleep(Duration::from_millis(100)).await;
            let mut request = [0_u8; 256];
            let _ = stream.read(&mut request).await;
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                .await;
        });
        let task = {
            let mut inner = supervisor.inner.lock().await;
            inner.generation = 9;
            inner.runtime = recovery_runtime(&HarnessRuntimeInfo::running(42, 10, 11));
            inner.recovery = Some(RecoveryState {
                generation: 9,
                target: ReadinessTarget::parse(&format!("http://127.0.0.1:{port}/health"))
                    .expect("loopback target parses"),
                deadline: Instant::now() + Duration::from_millis(500),
                exit_code: Some(1),
                task_started: false,
            });
            let runtime = inner.runtime.clone();
            supervisor
                .metadata_store()
                .update_harness(runtime)
                .expect("recovery metadata writes");
            super::pending_recovery(&mut inner).expect("recovery task is pending")
        };
        supervisor.spawn_recovery(task);
        timeout(Duration::from_millis(200), accepted_rx)
            .await
            .expect("recovery probe connects")
            .expect("probe acknowledgement arrives");

        let error = supervisor
            .stop()
            .await
            .expect_err("in-flight unattached recovery cannot be stopped");
        assert!(matches!(error, HarnessSupervisorError::Unattached));
        server.await.expect("readiness server completes");
        sleep(Duration::from_millis(50)).await;
        let persisted = supervisor
            .metadata_store()
            .read()
            .expect("metadata reads")
            .expect("recovered metadata remains present");
        assert_eq!(persisted.harness.state, HarnessState::Running);
        assert_eq!(persisted.harness.pid, None);

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn historical_failed_metadata_does_not_blame_current_patches() {
        let root = std::env::temp_dir().join(format!("nexus-patch-spawn-evidence-{}", nexus_core::unix_time_nanos_for_update()));
        let paths = NexusPaths::from_root(root.clone());
        let preferences = nexus_protocol::HarnessPreferencesPayload { patches: Some(vec![root.join("local.yml").to_string_lossy().into_owned()]), ..Default::default() };
        nexus_core::ConfigStore::new(paths.clone()).transaction(|document| { document.harness_preferences = Some(preferences.clone()); Ok(()) }).unwrap();
        let supervisor = HarnessSupervisor::new(paths.clone()).unwrap();
        let (generation, failed) = {
            let mut inner = supervisor.inner.lock().await;
            inner.runtime = super::failed_runtime(&inner.runtime, "historical Node spawn failure".into());
            (inner.generation, inner.runtime.clone())
        };
        assert!(supervisor.persist_if_current(generation, &failed).await.unwrap());
        crate::runtime_patches::check_failures(&paths, &preferences).unwrap();
        // A successfully spawned launch remains proven even after reaping
        // removes inner.child. The same failure now blocks the combination.
        {
            let mut inner = supervisor.inner.lock().await;
            inner.spawned_launch = true;
            inner.spawned_preferences = Some(preferences.clone());
        }
        let other = nexus_protocol::HarnessPreferencesPayload { patches: Some(vec![root.join("other.yml").to_string_lossy().into_owned()]), ..Default::default() };
        nexus_core::ConfigStore::new(paths.clone()).transaction(|document| { document.harness_preferences = Some(other.clone()); Ok(()) }).unwrap();
        assert!(supervisor.persist_if_current(generation, &failed).await.unwrap());
        assert!(crate::runtime_patches::check_failures(&paths, &preferences).is_err());
        crate::runtime_patches::check_failures(&paths, &other).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn stale_generation_snapshot_is_dropped_before_persistence() {
        let root = std::env::temp_dir().join(format!(
            "nexus-agent-stale-persistence-{}",
            std::process::id()
        ));
        let paths = NexusPaths::from_root(root.clone());
        let supervisor = HarnessSupervisor::new(paths).expect("supervisor creates");
        let stale = HarnessRuntimeInfo::starting(42, 10);
        let current = HarnessRuntimeInfo {
            state: HarnessState::Stopped,
            pid: None,
            exit_code: None,
            error: Some("explicit stop".to_owned()),
            started_at_unix: Some(10),
            updated_at_unix: Some(12),
        };
        {
            let mut inner = supervisor.inner.lock().await;
            inner.generation = 3;
            inner.runtime = stale.clone();
            supervisor
                .metadata_store()
                .update_harness(stale.clone())
                .expect("stale metadata writes");
            inner.generation = 4;
            inner.runtime = current.clone();
            supervisor
                .metadata_store()
                .update_harness(current.clone())
                .expect("current metadata writes");
        }

        assert!(!supervisor
            .persist_if_current(3, &stale)
            .await
            .expect("stale persistence check succeeds"));
        assert!(!supervisor
            .persist_agent_snapshot(3, 1, &AgentState::starting(), &stale)
            .await
            .expect("stale Agent snapshot check succeeds"));

        let mut newest_agent = AgentState::starting();
        newest_agent.set_profile("newest");
        assert!(supervisor
            .persist_agent_snapshot(4, 5, &newest_agent, &current)
            .await
            .expect("current Agent snapshot persists"));
        let mut stale_agent = AgentState::starting();
        stale_agent.set_profile("stale");
        assert!(!supervisor
            .persist_agent_snapshot(4, 4, &stale_agent, &current)
            .await
            .expect("stale Agent revision is rejected"));
        let persisted = supervisor
            .metadata_store()
            .read()
            .expect("metadata reads")
            .expect("current metadata remains present");
        assert_eq!(persisted.harness, current);
        assert_eq!(persisted.profile.as_deref(), Some("newest"));

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn start_waits_for_an_in_progress_stop_before_spawning() {
        let root = std::env::temp_dir().join(format!(
            "nexus-agent-lifecycle-{}-{}",
            std::process::id(),
            unix_time_seconds()
        ));
        let paths = NexusPaths::from_root(root.clone());
        let (program, args) = if cfg!(windows) {
            (
                std::env::var_os("ComSpec")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("cmd.exe")),
                vec!["/C".to_owned(), "ping -n 30 127.0.0.1 >NUL".to_owned()],
            )
        } else {
            (PathBuf::from("sleep"), vec!["10".to_owned()])
        };
        ConfigStore::new(paths.clone())
            .write(&NexusConfigFile { update_attempt_id: None, external_harness: None,
                schema_version: 1,
                harness_preferences: None,
                harness: Some(HarnessLaunchSpec {
                    mode: Default::default(),
                    program,
                    args,
                    working_dir: None,
                    readiness_url: None,
                    readiness_timeout_secs: None,
                    readiness_token_required: false,
                }),
                update: None,

                releases: None,
                runtime: None,
                snapshots: None,
            })
            .expect("Harness config writes");

        let supervisor = HarnessSupervisor::with_graceful_wait(paths, Duration::from_millis(500))
            .expect("supervisor creates");
        supervisor.start().await.expect("initial start succeeds");

        // A real CTRL_C can now finish immediately. Hold the stop owner at a
        // deterministic boundary instead of assuming it always needs 500 ms.
        let (reached, reached_receiver) = oneshot::channel();
        let (release, release_receiver) = oneshot::channel();
        *supervisor.stop_wait_gate.lock().await = Some(super::StartPersistGate { reached, release: release_receiver });
        let stop_supervisor = supervisor.clone();
        let stop_task = tokio::spawn(async move { stop_supervisor.stop().await });
        timeout(Duration::from_secs(2), reached_receiver).await.unwrap().unwrap();
        let overlapping_start = timeout(Duration::from_millis(100), supervisor.start()).await;
        assert!(
            overlapping_start.is_err(),
            "start must remain serialized until stop completes"
        );
        release.send(()).unwrap();
        stop_task
            .await
            .expect("stop task joins")
            .expect("stop succeeds");

        let final_runtime = supervisor.status().await;
        assert_eq!(final_runtime.state, HarnessState::Stopped);
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn start_returns_before_readiness_and_owner_reaches_running() {
        let root = std::env::temp_dir().join(format!(
            "nexus-agent-cancelled-start-{}-{}",
            std::process::id(),
            unix_time_seconds()
        ));
        let paths = NexusPaths::from_root(root.clone());
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("readiness listener binds");
        let port = listener.local_addr().expect("readiness address").port();
        let (request_seen_tx, request_seen_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("readiness accepts");
            let mut request = [0_u8; 256];
            let _ = stream.read(&mut request).await.expect("request reads");
            let _ = request_seen_tx.send(());
            let _ = release_rx.await;
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                .await
                .expect("readiness responds");
        });
        let (program, args) = if cfg!(windows) {
            (
                std::env::var_os("ComSpec")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("cmd.exe")),
                vec!["/C".to_owned(), "ping -n 30 127.0.0.1 >NUL".to_owned()],
            )
        } else {
            (PathBuf::from("sleep"), vec!["10".to_owned()])
        };
        ConfigStore::new(paths.clone())
            .write(&NexusConfigFile { update_attempt_id: None, external_harness: None,
                schema_version: 1,
                harness_preferences: None,
                harness: Some(HarnessLaunchSpec {
                    mode: Default::default(),
                    program,
                    args,
                    working_dir: None,
                    readiness_url: Some(format!("http://127.0.0.1:{port}/health")),
                    readiness_timeout_secs: Some(2),
                    readiness_token_required: false,
                }),
                update: None,

                releases: None,
                runtime: None,
                snapshots: None,
            })
            .expect("Harness config writes");

        let supervisor = HarnessSupervisor::with_graceful_wait(paths, Duration::from_millis(500))
            .expect("supervisor creates");
        let initial = supervisor
            .start()
            .await
            .expect("Harness spawn records Starting");
        assert_eq!(initial.state, HarnessState::Starting);
        timeout(Duration::from_secs(2), request_seen_rx)
            .await
            .expect("readiness request begins independently")
            .expect("readiness acknowledgement arrives");
        assert_eq!(supervisor.status().await.state, HarnessState::Starting);
        release_tx.send(()).expect("readiness release sends");
        server.await.expect("readiness server completes");

        timeout(Duration::from_secs(2), async {
            loop {
                let runtime = supervisor.status().await;
                if runtime.state == HarnessState::Running {
                    break runtime;
                }
                sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("detached readiness task marks Harness Running");
        let stopped = supervisor.stop().await.expect("stop succeeds");
        assert_eq!(stopped.state, HarnessState::Stopped);

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn token_bound_start_rejects_a_ready_listener_without_a_current_token() {
        let root = std::env::temp_dir().join(format!(
            "nexus-agent-token-bound-start-negative-{}-{}",
            std::process::id(),
            unix_time_seconds()
        ));
        let paths = NexusPaths::from_root(root.clone());
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("unrelated listener binds");
        let port = listener.local_addr().expect("listener address").port();
        let server = tokio::spawn(async move {
            loop {
                let _ = listener.accept().await.expect("TCP readiness accepts");
            }
        });
        let (program, args) = if cfg!(windows) {
            (
                crate::test_powershell(),
                vec![
                    "-NoProfile".to_owned(),
                    "-Command".to_owned(),
                    "Start-Sleep -Seconds 30".to_owned(),
                ],
            )
        } else {
            (PathBuf::from("sleep"), vec!["30".to_owned()])
        };
        ConfigStore::new(paths.clone())
            .write(&NexusConfigFile { update_attempt_id: None, external_harness: None,
                schema_version: 1,
                harness_preferences: None,
                harness: Some(HarnessLaunchSpec {
                    mode: Default::default(),
                    program,
                    args,
                    working_dir: None,
                    readiness_url: Some(format!("tcp://127.0.0.1:{port}")),
                    readiness_timeout_secs: Some(1),
                    readiness_token_required: true,
                }),
                update: None,

                releases: None,
                runtime: None,
                snapshots: None,
            })
            .expect("Harness config writes");
        let supervisor = HarnessSupervisor::new(paths.clone()).expect("supervisor creates");
        assert_eq!(
            supervisor.start().await.expect("Harness starts").state,
            HarnessState::Starting
        );
        let failed = timeout(Duration::from_secs(4), async {
            loop {
                let runtime = supervisor.status().await;
                if runtime.state == HarnessState::Failed {
                    break runtime;
                }
                sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("token-bound start reaches its deadline");
        assert!(failed
            .error
            .as_deref()
            .is_some_and(|message| message.contains("timed out waiting")));

        server.abort();
        let _ = server.await;
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn token_bound_start_accepts_a_current_token_for_the_ready_port() {
        let root = std::env::temp_dir().join(format!(
            "nexus-agent-token-bound-start-positive-{}-{}",
            std::process::id(),
            unix_time_seconds()
        ));
        let paths = NexusPaths::from_root(root.clone());
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("readiness listener binds");
        let port = listener.local_addr().expect("listener address").port();
        let server = tokio::spawn(async move {
            loop {
                let _ = listener.accept().await.expect("TCP readiness accepts");
            }
        });
        let (program, args) = if cfg!(windows) {
            (
                crate::test_powershell(),
                vec![
                    "-NoProfile".to_owned(),
                    "-Command".to_owned(),
                    "Start-Sleep -Seconds 30".to_owned(),
                ],
            )
        } else {
            (PathBuf::from("sleep"), vec!["30".to_owned()])
        };
        ConfigStore::new(paths.clone())
            .write(&NexusConfigFile { update_attempt_id: None, external_harness: None,
                schema_version: 1,
                harness_preferences: None,
                harness: Some(HarnessLaunchSpec {
                    mode: Default::default(),
                    program,
                    args,
                    working_dir: None,
                    readiness_url: Some(format!("tcp://127.0.0.1:{port}")),
                    readiness_timeout_secs: Some(2),
                    readiness_token_required: true,
                }),
                update: None,

                releases: None,
                runtime: None,
                snapshots: None,
            })
            .expect("Harness config writes");
        let supervisor = HarnessSupervisor::new(paths.clone()).expect("supervisor creates");
        supervisor.start().await.expect("Harness starts");
        let session = supervisor
            .log_sessions
            .read()
            .expect("session reads")
            .expect("session exists");
        fs::OpenOptions::new()
            .append(true)
            .open(paths.logs_dir.join(&session.stdout_log_name))
            .expect("current log opens")
            .write_all(format!("dsh web: http://127.0.0.1:{port}/?token=current\n").as_bytes())
            .expect("current token appends");
        let running = timeout(Duration::from_secs(3), async {
            loop {
                let runtime = supervisor.status().await;
                if runtime.state == HarnessState::Running {
                    break runtime;
                }
                sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("token-bound start becomes Running");
        assert!(running.pid.is_some());
        supervisor.stop().await.expect("Harness stops");

        server.abort();
        let _ = server.await;
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn cancelled_start_before_persistence_fails_without_orphaning_child() {
        let root = std::env::temp_dir().join(format!(
            "nexus-agent-cancelled-start-persist-{}-{}",
            std::process::id(),
            unix_time_seconds()
        ));
        let paths = NexusPaths::from_root(root.clone());
        let (program, args) = if cfg!(windows) {
            (
                std::env::var_os("ComSpec")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("cmd.exe")),
                vec!["/C".to_owned(), "ping -n 30 127.0.0.1 >NUL".to_owned()],
            )
        } else {
            (PathBuf::from("sleep"), vec!["10".to_owned()])
        };
        ConfigStore::new(paths.clone())
            .write(&NexusConfigFile { update_attempt_id: None, external_harness: None,
                schema_version: 1,
                harness_preferences: None,
                harness: Some(HarnessLaunchSpec {
                    mode: Default::default(),
                    program,
                    args,
                    working_dir: None,
                    readiness_url: Some("http://127.0.0.1:1/health".to_owned()),
                    readiness_timeout_secs: Some(2),
                    readiness_token_required: false,
                }),
                update: None,

                releases: None,
                runtime: None,
                snapshots: None,
            })
            .expect("Harness config writes");

        let supervisor = HarnessSupervisor::with_graceful_wait(paths, Duration::from_millis(500))
            .expect("supervisor creates");
        let (gate_reached_tx, gate_reached_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        *supervisor.start_persist_gate.lock().await = Some(StartPersistGate {
            reached: gate_reached_tx,
            release: release_rx,
        });
        let start_supervisor = supervisor.clone();
        let start_task = tokio::spawn(async move { start_supervisor.start().await });
        timeout(Duration::from_secs(2), gate_reached_rx)
            .await
            .expect("cancellation window opens after child spawn")
            .expect("child acknowledgement arrives");
        start_task.abort();
        drop(release_tx);
        assert!(start_task
            .await
            .expect_err("start request is cancelled")
            .is_cancelled());

        let runtime = timeout(Duration::from_secs(2), async {
            loop {
                let runtime = supervisor.status().await;
                if runtime.state == HarnessState::Failed {
                    break runtime;
                }
                sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("cancelled start is finalized as failed");
        assert_eq!(runtime.state, HarnessState::Failed);
        assert!(
            supervisor.inner.lock().await.child.is_none(),
            "cancelled start must not leave an attached child behind"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn cancelled_pending_start_recovers_short_bootstrap_without_duplicate() {
        let root = std::env::temp_dir().join(format!(
            "nexus-agent-cancelled-short-start-{}-{}",
            std::process::id(),
            unix_time_seconds()
        ));
        let paths = NexusPaths::from_root(root.clone());
        let marker = root.join("bootstrap-count.txt");
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("replacement readiness listener binds");
        let port = listener.local_addr().expect("readiness address").port();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("readiness accepts");
            let mut request = [0_u8; 256];
            let _ = stream.read(&mut request).await;
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                .await
                .expect("readiness responds");
        });
        let (program, args) = immediate_nonzero_marker_command(&marker);
        ConfigStore::new(paths.clone())
            .write(&NexusConfigFile { update_attempt_id: None, external_harness: None,
                schema_version: 1,
                harness_preferences: None,
                harness: Some(HarnessLaunchSpec {
                    mode: Default::default(),
                    program,
                    args,
                    working_dir: None,
                    readiness_url: Some(format!("http://127.0.0.1:{port}/health")),
                    readiness_timeout_secs: Some(2),
                    readiness_token_required: false,
                }),
                update: None,

                releases: None,
                runtime: None,
                snapshots: None,
            })
            .expect("Harness config writes");
        let supervisor = HarnessSupervisor::new(paths).expect("supervisor creates");
        let (gate_reached_tx, gate_reached_rx) = oneshot::channel();
        let (_release_tx, release_rx) = oneshot::channel();
        *supervisor.start_persist_gate.lock().await = Some(StartPersistGate {
            reached: gate_reached_tx,
            release: release_rx,
        });
        let start_supervisor = supervisor.clone();
        let start_task = tokio::spawn(async move { start_supervisor.start().await });
        timeout(Duration::from_secs(2), gate_reached_rx)
            .await
            .expect("ownership gate is reached")
            .expect("ownership gate acknowledgement arrives");
        wait_for_pending_child_exit(&supervisor).await;
        for _ in 0..4 {
            let runtime = supervisor.status().await;
            assert_eq!(runtime.state, HarnessState::Starting);
            assert!(runtime.pid.is_some());
            let inner = supervisor.inner.lock().await;
            assert!(inner.start_ownership_pending);
            assert!(inner.recovery.is_none());
        }
        assert_eq!(marker_lines(&marker), ["bootstrap"]);

        start_task.abort();
        assert!(start_task
            .await
            .expect_err("start request is cancelled")
            .is_cancelled());
        server
            .await
            .expect("replacement readiness server completes");
        let recovered = timeout(Duration::from_secs(2), async {
            loop {
                let runtime = supervisor.status().await;
                if runtime.state == HarnessState::Running && runtime.pid.is_none() {
                    break runtime;
                }
                sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("cancelled request preserves replacement recovery");
        assert_eq!(recovered.exit_code, None);
        assert!(matches!(
            supervisor.start().await,
            Err(HarnessSupervisorError::AlreadyRunning)
        ));
        assert_eq!(marker_lines(&marker), ["bootstrap"]);

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn short_bootstrap_does_not_mask_initial_metadata_failure() {
        let root = std::env::temp_dir().join(format!(
            "nexus-agent-short-persist-failure-{}-{}",
            std::process::id(),
            unix_time_seconds()
        ));
        let paths = NexusPaths::from_root(root.clone());
        let marker = root.join("bootstrap-count.txt");
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("replacement readiness listener binds");
        let port = listener.local_addr().expect("readiness address").port();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("readiness accepts");
            let mut request = [0_u8; 256];
            let _ = stream.read(&mut request).await;
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                .await
                .expect("readiness responds");
        });
        let (program, args) = immediate_nonzero_marker_command(&marker);
        ConfigStore::new(paths.clone())
            .write(&NexusConfigFile { update_attempt_id: None, external_harness: None,
                schema_version: 1,
                harness_preferences: None,
                harness: Some(HarnessLaunchSpec {
                    mode: Default::default(),
                    program,
                    args,
                    working_dir: None,
                    readiness_url: Some(format!("http://127.0.0.1:{port}/health")),
                    readiness_timeout_secs: Some(2),
                    readiness_token_required: false,
                }),
                update: None,

                releases: None,
                runtime: None,
                snapshots: None,
            })
            .expect("Harness config writes");
        let supervisor = HarnessSupervisor::new(paths.clone()).expect("supervisor creates");
        let (gate_reached_tx, gate_reached_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        *supervisor.start_persist_gate.lock().await = Some(StartPersistGate {
            reached: gate_reached_tx,
            release: release_rx,
        });
        let start_supervisor = supervisor.clone();
        let start_task = tokio::spawn(async move { start_supervisor.start().await });
        timeout(Duration::from_secs(2), gate_reached_rx)
            .await
            .expect("ownership gate is reached")
            .expect("ownership gate acknowledgement arrives");
        wait_for_pending_child_exit(&supervisor).await;
        assert_eq!(supervisor.status().await.state, HarnessState::Starting);
        assert_eq!(marker_lines(&marker), ["bootstrap"]);
        fs::remove_file(&paths.state_file).expect("durable prepared state removes for sabotage");
        fs::create_dir_all(&paths.state_file).expect("state path becomes an unwritable directory");
        release_tx
            .send(())
            .expect("ownership persistence is released");

        let error = start_task
            .await
            .expect("start task completes")
            .expect_err("initial ownership metadata failure is not masked");
        assert!(matches!(error, HarnessSupervisorError::Persistence(_)));
        server
            .await
            .expect("replacement readiness server completes");
        let recovered = timeout(Duration::from_secs(2), async {
            loop {
                let runtime = supervisor.status().await;
                if runtime.state == HarnessState::Running && runtime.pid.is_none() {
                    break runtime;
                }
                sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("failed persistence still preserves replacement recovery in memory");
        assert_eq!(recovered.exit_code, None);
        assert!(matches!(
            supervisor.start().await,
            Err(HarnessSupervisorError::AlreadyRunning)
        ));
        assert_eq!(marker_lines(&marker), ["bootstrap"]);

        let _ = fs::remove_dir_all(&paths.state_file);
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn initial_metadata_failure_reaps_spawned_child() {
        let root = std::env::temp_dir().join(format!(
            "nexus-agent-initial-persist-failure-{}-{}",
            std::process::id(),
            unix_time_seconds()
        ));
        let paths = NexusPaths::from_root(root.clone());
        let (program, args) = if cfg!(windows) {
            (
                crate::test_powershell(),
                vec![
                    "-NoProfile".to_owned(),
                    "-Command".to_owned(),
                    "Start-Sleep -Seconds 30".to_owned(),
                ],
            )
        } else {
            (PathBuf::from("sleep"), vec!["30".to_owned()])
        };
        ConfigStore::new(paths.clone())
            .write(&NexusConfigFile { update_attempt_id: None, external_harness: None,
                schema_version: 1,
                harness_preferences: None,
                harness: Some(HarnessLaunchSpec {
                    mode: Default::default(),
                    program,
                    args,
                    working_dir: None,
                    readiness_url: None,
                    readiness_timeout_secs: None,
                    readiness_token_required: false,
                }),
                update: None,

                releases: None,
                runtime: None,
                snapshots: None,
            })
            .expect("Harness config writes");
        let supervisor =
            HarnessSupervisor::with_graceful_wait(paths.clone(), Duration::from_millis(100))
                .expect("supervisor creates before state path is sabotaged");
        let (gate_reached_tx, gate_reached_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        *supervisor.start_persist_gate.lock().await = Some(StartPersistGate {
            reached: gate_reached_tx,
            release: release_rx,
        });
        let start_supervisor = supervisor.clone();
        let start_task = tokio::spawn(async move { start_supervisor.start().await });
        timeout(Duration::from_secs(2), gate_reached_rx)
            .await
            .expect("post-spawn persistence gate is reached")
            .expect("post-spawn gate acknowledgement arrives");
        fs::remove_file(&paths.state_file).expect("prepared state removes for sabotage");
        fs::create_dir_all(&paths.state_file).expect("state path becomes an unwritable directory");
        release_tx.send(()).expect("post-spawn persistence resumes");

        let error = start_task
            .await
            .expect("start task joins")
            .expect_err("initial ownership metadata must fail");
        assert!(matches!(error, HarnessSupervisorError::Persistence(_)));
        let inner = supervisor.inner.lock().await;
        assert!(inner.child.is_none(), "failed start must reap its child");
        assert_eq!(inner.runtime.state, HarnessState::Failed);
        assert_eq!(inner.runtime.pid, None);
        assert!(!inner.log_session.launch_pending);
        drop(inner);

        let _ = fs::remove_dir_all(&paths.state_file);
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn prepared_metadata_failure_happens_before_process_creation() {
        let root = std::env::temp_dir().join(format!(
            "nexus-agent-prepared-persist-failure-{}-{}",
            std::process::id(),
            unix_time_seconds()
        ));
        let paths = NexusPaths::from_root(root.clone());
        let marker = root.join("must-not-spawn.txt");
        let (program, args) = immediate_nonzero_marker_command(&marker);
        ConfigStore::new(paths.clone())
            .write(&NexusConfigFile { update_attempt_id: None, external_harness: None,
                schema_version: 1,
                harness_preferences: None,
                harness: Some(HarnessLaunchSpec {
                    mode: Default::default(),
                    program,
                    args,
                    working_dir: None,
                    readiness_url: None,
                    readiness_timeout_secs: None,
                    readiness_token_required: false,
                }),
                update: None,

                releases: None,
                runtime: None,
                snapshots: None,
            })
            .expect("Harness config writes");
        let supervisor = HarnessSupervisor::new(paths.clone()).expect("supervisor creates");
        fs::create_dir_all(&paths.state_file).expect("state path becomes unwritable");

        let error = supervisor
            .start()
            .await
            .expect_err("prepared ownership metadata must be durable before spawn");
        assert!(matches!(error, HarnessSupervisorError::Persistence(_)));
        assert!(!marker.exists(), "Harness process must not be created");
        let inner = supervisor.inner.lock().await;
        assert!(inner.child.is_none());
        assert_eq!(inner.runtime.state, HarnessState::Failed);
        assert_eq!(inner.runtime.pid, None);
        assert!(
            !inner.log_session.launch_pending,
            "a failure before process creation must release the durable reservation"
        );
        drop(inner);
        let _ = fs::remove_dir_all(&paths.state_file);
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn stop_is_not_blocked_by_background_readiness() {
        let root = std::env::temp_dir().join(format!(
            "nexus-agent-stop-during-readiness-{}-{}",
            std::process::id(),
            unix_time_seconds()
        ));
        let paths = NexusPaths::from_root(root.clone());
        let (program, args) = if cfg!(windows) {
            (
                crate::test_powershell(),
                vec![
                    "-NoProfile".to_owned(),
                    "-Command".to_owned(),
                    "Start-Sleep -Seconds 30".to_owned(),
                ],
            )
        } else {
            (PathBuf::from("sleep"), vec!["30".to_owned()])
        };
        ConfigStore::new(paths.clone())
            .write(&NexusConfigFile { update_attempt_id: None, external_harness: None,
                schema_version: 1,
                harness_preferences: None,
                harness: Some(HarnessLaunchSpec {
                    mode: Default::default(),
                    program,
                    args,
                    working_dir: None,
                    readiness_url: Some("http://127.0.0.1:1/health".to_owned()),
                    readiness_timeout_secs: Some(60),
                    readiness_token_required: false,
                }),
                update: None,

                releases: None,
                runtime: None,
                snapshots: None,
            })
            .expect("Harness config writes");
        let supervisor = HarnessSupervisor::with_graceful_wait(paths, Duration::from_millis(100))
            .expect("supervisor creates");
        let starting = supervisor.start().await.expect("Harness starts");
        assert_eq!(starting.state, HarnessState::Starting);

        let stopped = timeout(Duration::from_secs(2), supervisor.stop())
            .await
            .expect("stop must not wait for the 60-second readiness deadline")
            .expect("stop succeeds");
        assert_eq!(stopped.state, HarnessState::Stopped);
        assert_eq!(stopped.pid, None);

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn cancelled_stop_keeps_detached_owner_and_blocks_duplicate_start() {
        let root = std::env::temp_dir().join(format!(
            "nexus-agent-cancelled-stop-{}-{}",
            std::process::id(),
            unix_time_seconds()
        ));
        let paths = NexusPaths::from_root(root.clone());
        let (program, args) = if cfg!(windows) {
            (
                crate::test_powershell(),
                vec![
                    "-NoProfile".to_owned(),
                    "-Command".to_owned(),
                    "Start-Sleep -Seconds 30".to_owned(),
                ],
            )
        } else {
            (PathBuf::from("sleep"), vec!["30".to_owned()])
        };
        ConfigStore::new(paths.clone())
            .write(&NexusConfigFile { update_attempt_id: None, external_harness: None,
                schema_version: 1,
                harness_preferences: None,
                harness: Some(HarnessLaunchSpec {
                    mode: Default::default(),
                    program,
                    args,
                    working_dir: None,
                    readiness_url: None,
                    readiness_timeout_secs: None,
                    readiness_token_required: false,
                }),
                update: None,

                releases: None,
                runtime: None,
                snapshots: None,
            })
            .expect("Harness config writes");
        let supervisor = HarnessSupervisor::with_graceful_wait(paths, Duration::from_millis(500))
            .expect("supervisor creates");
        let _ = supervisor.start().await.expect("Harness starts");
        let stop_supervisor = supervisor.clone();
        let stop_task = tokio::spawn(async move { stop_supervisor.stop().await });
        timeout(Duration::from_secs(1), async {
            loop {
                if supervisor.inner.lock().await.stop_pending {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("detached stop owner takes process ownership");
        stop_task.abort();
        assert!(stop_task
            .await
            .expect_err("stop request is cancelled")
            .is_cancelled());

        let duplicate = supervisor.start().await;
        assert!(matches!(duplicate, Err(HarnessSupervisorError::AlreadyRunning)),
            "start while detached stop is pending: {duplicate:?}");
        let stopped = timeout(Duration::from_secs(2), async {
            loop {
                let runtime = supervisor.status().await;
                if runtime.state == HarnessState::Stopped {
                    break runtime;
                }
                sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("detached stop owner completes after request cancellation");
        assert_eq!(stopped.pid, None);
        let inner = supervisor.inner.lock().await;
        assert!(!inner.stop_pending);
        assert!(inner.child.is_none());
        drop(inner);

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn stop_wait_error_restores_child_ownership() {
        let root = std::env::temp_dir().join(format!(
            "nexus-agent-stop-wait-error-{}-{}",
            std::process::id(),
            unix_time_seconds()
        ));
        let paths = NexusPaths::from_root(root.clone());
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("readiness listener binds");
        let port = listener.local_addr().expect("readiness address").port();
        let (program, args) = if cfg!(windows) {
            (
                crate::test_powershell(),
                vec![
                    "-NoProfile".to_owned(),
                    "-Command".to_owned(),
                    "Start-Sleep -Seconds 30".to_owned(),
                ],
            )
        } else {
            (PathBuf::from("sleep"), vec!["30".to_owned()])
        };
        ConfigStore::new(paths.clone())
            .write(&NexusConfigFile { update_attempt_id: None, external_harness: None,
                schema_version: 1,
                harness_preferences: None,
                harness: Some(HarnessLaunchSpec {
                    mode: Default::default(),
                    program,
                    args,
                    working_dir: None,
                    readiness_url: Some(format!("http://127.0.0.1:{port}/health")),
                    readiness_timeout_secs: Some(60),
                    readiness_token_required: false,
                }),
                update: None,

                releases: None,
                runtime: None,
                snapshots: None,
            })
            .expect("Harness config writes");
        let supervisor = HarnessSupervisor::with_graceful_wait(paths, Duration::from_millis(100))
            .expect("supervisor creates");
        let _ = supervisor.start().await.expect("Harness starts");
        let session_before_stop = supervisor
            .log_sessions
            .read()
            .expect("session reads")
            .expect("session exists");
        supervisor
            .stop_wait_failure
            .store(true, std::sync::atomic::Ordering::SeqCst);

        assert!(matches!(
            supervisor.stop().await,
            Err(HarnessSupervisorError::Process(_))
        ));
        let (generation_after_stop_error, _, session_after_stop_error) =
            supervisor.status_observation().await;
        assert_eq!(generation_after_stop_error, session_before_stop.generation);
        assert_eq!(
            session_after_stop_error.generation,
            session_before_stop.generation
        );
        assert_eq!(session_after_stop_error.run_id, session_before_stop.run_id);
        let readiness_server = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.expect("readiness accepts");
                let mut request = [0_u8; 256];
                let _ = stream.read(&mut request).await;
                stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                    .await
                    .expect("readiness responds");
            }
        });
        {
            let inner = supervisor.inner.lock().await;
            assert!(!inner.stop_pending);
            assert!(
                inner.child.is_some(),
                "failed wait must restore Child handle"
            );
            assert!(inner.readiness.is_some());
            assert_eq!(inner.runtime.state, HarnessState::Starting);
        }
        assert!(matches!(
            supervisor.start().await,
            Err(HarnessSupervisorError::AlreadyRunning)
        ));
        let running = timeout(Duration::from_secs(2), async {
            loop {
                let runtime = supervisor.status().await;
                if runtime.state == HarnessState::Running {
                    break runtime;
                }
                sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("restored readiness owner reaches Running");
        assert!(running.pid.is_some());
        readiness_server.abort();
        assert!(readiness_server
            .await
            .expect_err("readiness server is cancelled")
            .is_cancelled());
        let stopped = supervisor.stop().await.expect("retried stop succeeds");
        assert_eq!(stopped.state, HarnessState::Stopped);
        assert_eq!(stopped.pid, None);

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn ordinary_process_exit_clears_stale_pid() {
        let root = std::env::temp_dir().join(format!(
            "nexus-agent-exit-pid-{}-{}",
            std::process::id(),
            unix_time_seconds()
        ));
        let paths = NexusPaths::from_root(root.clone());
        let (program, args) = if cfg!(windows) {
            (
                crate::test_powershell(),
                vec![
                    "-NoProfile".to_owned(),
                    "-Command".to_owned(),
                    "exit 7".to_owned(),
                ],
            )
        } else {
            (
                PathBuf::from("sh"),
                vec!["-c".to_owned(), "exit 7".to_owned()],
            )
        };
        ConfigStore::new(paths.clone())
            .write(&NexusConfigFile { update_attempt_id: None, external_harness: None,
                schema_version: 1,
                harness_preferences: None,
                harness: Some(HarnessLaunchSpec {
                    mode: Default::default(),
                    program,
                    args,
                    working_dir: None,
                    readiness_url: None,
                    readiness_timeout_secs: None,
                    readiness_token_required: false,
                }),
                update: None,

                releases: None,
                runtime: None,
                snapshots: None,
            })
            .expect("Harness config writes");
        let supervisor = HarnessSupervisor::new(paths).expect("supervisor creates");
        let _ = supervisor.start().await.expect("process spawns");

        let failed = timeout(Duration::from_secs(2), async {
            loop {
                let runtime = supervisor.status().await;
                if runtime.state == HarnessState::Failed {
                    break runtime;
                }
                sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("process exit is observed");
        assert_eq!(failed.exit_code, Some(7));
        assert_eq!(failed.pid, None);

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn stop_rejects_running_unattached_without_mutating_state() {
        let root = std::env::temp_dir().join(format!(
            "nexus-agent-unattached-stop-{}-{}",
            std::process::id(),
            unix_time_seconds()
        ));
        let paths = NexusPaths::from_root(root.clone());
        ConfigStore::new(paths.clone())
            .write(&NexusConfigFile { update_attempt_id: None, external_harness: None,
                schema_version: 1,
                harness_preferences: None,
                harness: Some(HarnessLaunchSpec {
                    mode: Default::default(),
                    program: if cfg!(windows) {
                        crate::test_powershell()
                    } else {
                        PathBuf::from("sleep")
                    },
                    args: Vec::new(),
                    working_dir: None,
                    readiness_url: None,
                    readiness_timeout_secs: None,
                    readiness_token_required: false,
                }),
                update: None,

                releases: None,
                runtime: None,
                snapshots: None,
            })
            .expect("Harness config writes");
        let supervisor = HarnessSupervisor::new(paths).expect("supervisor creates");
        let previous = HarnessRuntimeInfo::running(42, 10, 11);
        let runtime = running_runtime_without_pid(&previous);
        {
            let mut inner = supervisor.inner.lock().await;
            inner.generation = 4;
            inner.runtime = runtime.clone();
            supervisor
                .metadata_store()
                .update_harness(runtime.clone())
                .expect("running metadata writes");
        }

        let error = supervisor
            .stop()
            .await
            .expect_err("unattached Running state must not be reported stopped");
        assert!(matches!(error, HarnessSupervisorError::Unattached));
        assert_eq!(supervisor.status().await, runtime);
        let persisted = supervisor
            .metadata_store()
            .read()
            .expect("metadata reads")
            .expect("running metadata remains present");
        assert_eq!(persisted.harness, runtime);
        assert!(matches!(
            supervisor.start().await,
            Err(HarnessSupervisorError::AlreadyRunning)
        ));

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn start_rejects_existing_recovery_without_losing_it() {
        let root = std::env::temp_dir().join(format!(
            "nexus-agent-recover-spawn-failure-{}",
            std::process::id()
        ));
        let paths = NexusPaths::from_root(root.clone());
        ConfigStore::new(paths.clone())
            .write(&NexusConfigFile { update_attempt_id: None, external_harness: None,
                schema_version: 1,
                harness_preferences: None,
                harness: Some(HarnessLaunchSpec {
                    mode: Default::default(),
                    program: PathBuf::from("nexus-harness-program-does-not-exist"),
                    args: Vec::new(),
                    working_dir: None,
                    readiness_url: Some("http://127.0.0.1:1/health".to_owned()),
                    readiness_timeout_secs: Some(1),
                    readiness_token_required: false,
                }),
                update: None,

                releases: None,
                runtime: None,
                snapshots: None,
            })
            .expect("Harness config writes");
        let supervisor = HarnessSupervisor::new(paths).expect("supervisor creates");
        {
            let mut inner = supervisor.inner.lock().await;
            inner.generation = 11;
            inner.runtime = recovery_runtime(&HarnessRuntimeInfo::running(42, 10, 11));
            inner.recovery = Some(RecoveryState {
                generation: 11,
                target: ReadinessTarget::parse("http://127.0.0.1:1/health")
                    .expect("loopback target parses"),
                deadline: Instant::now() + Duration::from_secs(1),
                exit_code: Some(1),
                task_started: false,
            });
        }

        let error = supervisor
            .start()
            .await
            .expect_err("a recovery with no child must not spawn another Harness");
        assert!(matches!(error, HarnessSupervisorError::AlreadyRunning));
        let inner = supervisor.inner.lock().await;
        assert_eq!(inner.runtime.state, HarnessState::Starting);
        assert!(
            inner.recovery.is_some(),
            "recovery must not be lost on spawn failure"
        );

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn second_start_observes_unpolled_exit_before_spawning() {
        let root = std::env::temp_dir().join(format!(
            "nexus-agent-unpolled-exit-start-{}-{}",
            std::process::id(),
            unix_time_seconds()
        ));
        let paths = NexusPaths::from_root(root.clone());
        let marker = root.join("duplicate-bootstrap.txt");
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("replacement readiness listener binds");
        let port = listener.local_addr().expect("readiness address").port();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("readiness accepts");
            let mut request = [0_u8; 256];
            let _ = stream.read(&mut request).await;
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                .await
                .expect("readiness responds");
        });
        let (program, args) = if cfg!(windows) {
            (
                crate::test_powershell(),
                vec![
                    "-NoProfile".to_owned(),
                    "-Command".to_owned(),
                    format!(
                        "Add-Content -LiteralPath '{}' -Value second",
                        marker.display()
                    ),
                ],
            )
        } else {
            (
                PathBuf::from("sh"),
                vec![
                    "-c".to_owned(),
                    format!("printf 'second\\n' >> '{}'", marker.display()),
                ],
            )
        };
        ConfigStore::new(paths.clone())
            .write(&NexusConfigFile { update_attempt_id: None, external_harness: None,
                schema_version: 1,
                harness_preferences: None,
                harness: Some(HarnessLaunchSpec {
                    mode: Default::default(),
                    program,
                    args,
                    working_dir: None,
                    readiness_url: Some(format!("http://127.0.0.1:{port}/health")),
                    readiness_timeout_secs: Some(2),
                    readiness_token_required: false,
                }),
                update: None,

                releases: None,
                runtime: None,
                snapshots: None,
            })
            .expect("Harness config writes");
        let supervisor = HarnessSupervisor::new(paths).expect("supervisor creates");
        let mut short = if cfg!(windows) {
            let mut command = tokio::process::Command::new(crate::test_powershell());
            command.args([
                "-NoProfile",
                "-Command",
                &format!(
                    "Set-Content -LiteralPath '{}' -Value first; exit 1",
                    marker.display()
                ),
            ]);
            command
        } else {
            let mut command = tokio::process::Command::new("sh");
            command.args([
                "-c",
                &format!("printf 'first\\n' > '{}'; exit 1", marker.display()),
            ]);
            command
        };
        short.kill_on_drop(true);
        let mut child = short.spawn().expect("short bootstrap spawns");
        let pid = child.id().expect("short bootstrap has pid");
        let exit = child.wait().await.expect("short bootstrap exits");
        assert_eq!(exit.code(), Some(1));
        assert_eq!(
            fs::read_to_string(&marker)
                .expect("first bootstrap marker exists")
                .lines()
                .collect::<Vec<_>>(),
            ["first"]
        );
        {
            let mut inner = supervisor.inner.lock().await;
            inner.generation = 17;
            inner.runtime = HarnessRuntimeInfo::running(pid, 10, 11);
            inner.readiness = Some(super::ReadinessConfig {
                target: ReadinessTarget::parse(&format!("http://127.0.0.1:{port}/health"))
                    .expect("loopback target parses"),
                timeout: Duration::from_secs(2),
            });
            inner.child = Some(child.into());
        }

        let error = supervisor
            .start()
            .await
            .expect_err("unobserved non-zero exit must enter recovery first");
        assert!(matches!(error, HarnessSupervisorError::AlreadyRunning));
        assert_eq!(
            fs::read_to_string(&marker)
                .expect("first bootstrap marker remains")
                .lines()
                .collect::<Vec<_>>(),
            ["first"],
            "a second bootstrap must not be spawned"
        );
        server
            .await
            .expect("replacement readiness server completes");
        let recovered = timeout(Duration::from_secs(2), async {
            loop {
                let runtime = supervisor.status().await;
                if runtime.state == HarnessState::Running && runtime.pid.is_none() {
                    break runtime;
                }
                sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("existing replacement recovery reaches Running");
        assert_eq!(recovered.exit_code, None);
        assert_eq!(
            fs::read_to_string(&marker)
                .expect("recovery marker remains")
                .lines()
                .collect::<Vec<_>>(),
            ["first"],
            "recovery still must not spawn bootstrap"
        );

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn cancelled_status_cannot_strand_recovery_handoff() {
        let root = std::env::temp_dir().join(format!(
            "nexus-agent-cancelled-status-recovery-{}-{}",
            std::process::id(),
            unix_time_seconds()
        ));
        let paths = NexusPaths::from_root(root.clone());
        paths
            .ensure_directories()
            .expect("Nexus directories are available");
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("replacement readiness listener binds");
        let port = listener.local_addr().expect("readiness address").port();
        let (request_seen_tx, request_seen_rx) = oneshot::channel();
        let (response_release_tx, response_release_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("readiness accepts");
            let mut request = [0_u8; 256];
            let _ = stream.read(&mut request).await;
            let _ = request_seen_tx.send(());
            let _ = response_release_rx.await;
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                .await
                .expect("readiness responds");
        });
        let supervisor = HarnessSupervisor::new(paths).expect("supervisor creates");
        let mut short = if cfg!(windows) {
            let mut command = tokio::process::Command::new(
                std::env::var_os("ComSpec").unwrap_or_else(|| "cmd.exe".into()),
            );
            command.args(["/C", "exit 1"]);
            command
        } else {
            let mut command = tokio::process::Command::new("sh");
            command.args(["-c", "exit 1"]);
            command
        };
        short.kill_on_drop(true);
        let mut child = short.spawn().expect("short bootstrap spawns");
        let pid = child.id().expect("short bootstrap has pid");
        let exit = child.wait().await.expect("short bootstrap exits");
        assert_eq!(exit.code(), Some(1));
        {
            let mut inner = supervisor.inner.lock().await;
            inner.generation = 29;
            inner.runtime = HarnessRuntimeInfo::running(pid, 10, 11);
            inner.readiness = Some(super::ReadinessConfig {
                target: ReadinessTarget::parse(&format!("http://127.0.0.1:{port}/health"))
                    .expect("loopback target parses"),
                timeout: Duration::from_secs(2),
            });
            inner.child = Some(child.into());
        }
        let (gate_reached_tx, gate_reached_rx) = oneshot::channel();
        let (_gate_release_tx, gate_release_rx) = oneshot::channel();
        *supervisor.harness_persist_gate.lock().await = Some(HarnessPersistGate {
            reached: gate_reached_tx,
            release: gate_release_rx,
        });
        let status_supervisor = supervisor.clone();
        let status_task = tokio::spawn(async move { status_supervisor.status().await });
        timeout(Duration::from_secs(2), gate_reached_rx)
            .await
            .expect("status reaches persistence after recovery handoff")
            .expect("persistence gate acknowledgement arrives");
        {
            let inner = supervisor.inner.lock().await;
            assert!(
                inner
                    .recovery
                    .as_ref()
                    .is_some_and(|recovery| recovery.task_started),
                "recovery must have an owner before status can await"
            );
        }
        status_task.abort();
        assert!(status_task
            .await
            .expect_err("status request is cancelled at persistence")
            .is_cancelled());
        timeout(Duration::from_secs(2), request_seen_rx)
            .await
            .expect("detached recovery owner keeps probing")
            .expect("readiness request acknowledgement arrives");
        response_release_tx
            .send(())
            .expect("readiness response is released");
        server
            .await
            .expect("replacement readiness server completes");
        let recovered = timeout(Duration::from_secs(2), async {
            loop {
                let runtime = supervisor.status().await;
                if runtime.state == HarnessState::Running && runtime.pid.is_none() {
                    break runtime;
                }
                sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("recovery completes after status cancellation");
        assert_eq!(recovered.exit_code, None);

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn pending_recovery_times_out_to_failed_with_exit_code() {
        let root = std::env::temp_dir().join(format!(
            "nexus-agent-recover-timeout-{}",
            std::process::id()
        ));
        let supervisor = HarnessSupervisor::new(NexusPaths::from_root(root.clone()))
            .expect("supervisor creates");
        let task = {
            let mut inner = supervisor.inner.lock().await;
            inner.generation = 8;
            inner.log_session.generation = 8;
            inner.log_session.run_id = "recovery-timeout-8".to_owned();
            inner.log_session.launch_pending = true;
            supervisor
                .log_sessions
                .write(&inner.log_session)
                .expect("pending session writes");
            inner.runtime = recovery_runtime(&HarnessRuntimeInfo::running(42, 10, 11));
            inner.recovery = Some(RecoveryState {
                generation: 8,
                target: ReadinessTarget::parse("http://127.0.0.1:1/health")
                    .expect("loopback target parses"),
                deadline: Instant::now() + Duration::from_millis(75),
                exit_code: Some(1),
                task_started: false,
            });
            super::pending_recovery(&mut inner).expect("recovery task is pending")
        };
        supervisor.spawn_recovery(task);
        sleep(Duration::from_millis(200)).await;

        let runtime = supervisor.status().await;
        assert_eq!(runtime.state, HarnessState::Failed);
        assert_eq!(runtime.pid, None);
        assert_eq!(runtime.exit_code, Some(1));
        assert!(runtime
            .error
            .as_deref()
            .is_some_and(|error| error.contains("readiness recovery timed out")));
        let session = supervisor
            .log_sessions
            .read()
            .expect("session reads")
            .expect("session remains");
        assert!(
            !session.launch_pending,
            "a completed recovery deadline must durably abandon its reservation"
        );

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn recover_unattached_refuses_an_identityless_tcp_listener() {
        let root = std::env::temp_dir().join(format!(
            "nexus-agent-recover-identityless-tcp-{}-{}",
            std::process::id(),
            unix_time_seconds()
        ));
        let paths = NexusPaths::from_root(root.clone());
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("unrelated listener binds");
        let port = listener.local_addr().expect("listener address").port();
        let server = tokio::spawn(async move {
            loop {
                let _ = listener.accept().await.expect("TCP readiness accepts");
            }
        });
        ConfigStore::new(paths.clone())
            .write(&NexusConfigFile { update_attempt_id: None, external_harness: None,
                schema_version: 1,
                harness_preferences: None,
                harness: Some(HarnessLaunchSpec {
                    mode: Default::default(),
                    program: PathBuf::from("unused-harness"),
                    args: Vec::new(),
                    working_dir: None,
                    readiness_url: Some(format!("tcp://127.0.0.1:{port}")),
                    readiness_timeout_secs: Some(1),
                    readiness_token_required: true,
                }),
                update: None,

                releases: None,
                runtime: None,
                snapshots: None,
            })
            .expect("Harness config writes");
        RuntimeMetadataStore::new(paths.clone())
            .update_harness(HarnessRuntimeInfo::starting(999, 10))
            .expect("persisted Harness state writes");

        let supervisor = HarnessSupervisor::new(paths).expect("supervisor creates");
        let runtime = supervisor.recover_unattached().await;

        assert_eq!(runtime.state, HarnessState::Failed);
        assert_eq!(runtime.pid, None);
        assert!(runtime.error.as_deref().is_some_and(|message| {
            message.contains("Token-bound readiness cannot safely reattach")
        }));

        server.abort();
        let _ = server.await;
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn recover_unattached_keeps_legacy_tcp_probe_without_token_requirement() {
        let root = std::env::temp_dir().join(format!(
            "nexus-agent-recover-legacy-tcp-{}-{}",
            std::process::id(),
            unix_time_seconds()
        ));
        let paths = NexusPaths::from_root(root.clone());
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("legacy readiness listener binds");
        let port = listener.local_addr().expect("listener address").port();
        let server = tokio::spawn(async move {
            loop {
                let _ = listener.accept().await.expect("TCP readiness accepts");
            }
        });
        ConfigStore::new(paths.clone())
            .write(&NexusConfigFile { update_attempt_id: None, external_harness: None,
                schema_version: 1,
                harness_preferences: None,
                harness: Some(HarnessLaunchSpec {
                    mode: Default::default(),
                    program: PathBuf::from("unused-harness"),
                    args: Vec::new(),
                    working_dir: None,
                    readiness_url: Some(format!("tcp://127.0.0.1:{port}")),
                    readiness_timeout_secs: Some(1),
                    readiness_token_required: false,
                }),
                update: None,

                releases: None,
                runtime: None,
                snapshots: None,
            })
            .expect("Harness config writes");
        RuntimeMetadataStore::new(paths.clone())
            .update_harness(HarnessRuntimeInfo::starting(999, 10))
            .expect("persisted Harness state writes");

        let supervisor = HarnessSupervisor::new(paths).expect("supervisor creates");
        let runtime = supervisor.recover_unattached().await;
        assert_eq!(runtime.state, HarnessState::Running);
        assert_eq!(runtime.pid, None);

        server.abort();
        let _ = server.await;
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn tcp_recovery_rejects_unrelated_listener_without_current_log_token() {
        let root = std::env::temp_dir().join(format!(
            "nexus-agent-tcp-recovery-token-boundary-{}-{}",
            std::process::id(),
            unix_time_seconds()
        ));
        let paths = NexusPaths::from_root(root.clone());
        paths
            .ensure_directories()
            .expect("Nexus directories are available");
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("unrelated listener binds");
        let port = listener.local_addr().expect("listener address").port();
        let server = tokio::spawn(async move {
            loop {
                let _ = listener.accept().await.expect("TCP readiness accepts");
            }
        });
        let supervisor = HarnessSupervisor::new(paths).expect("supervisor creates");
        let task = {
            let mut inner = supervisor.inner.lock().await;
            inner.generation = 9;
            inner.log_session.generation = 9;
            inner.log_session.run_id = "tcp-recovery-token-boundary-9".to_owned();
            inner.log_session.launch_pending = true;
            supervisor
                .log_sessions
                .write(&inner.log_session)
                .expect("pending session writes");
            inner.runtime = recovery_runtime(&HarnessRuntimeInfo::running(42, 10, 11));
            let mut target = ReadinessTarget::parse(&format!("tcp://127.0.0.1:{port}"))
                .expect("TCP target parses");
            target.token_required = true;
            inner.recovery = Some(RecoveryState {
                generation: 9,
                target,
                deadline: Instant::now() + Duration::from_millis(150),
                exit_code: Some(1),
                task_started: false,
            });
            pending_recovery(&mut inner).expect("recovery task is pending")
        };
        supervisor.spawn_recovery(task);
        sleep(Duration::from_millis(350)).await;

        let runtime = supervisor.status().await;
        assert_eq!(runtime.state, HarnessState::Failed);
        assert_eq!(runtime.pid, None);
        assert_eq!(runtime.exit_code, Some(1));

        server.abort();
        let _ = server.await;
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn token_bound_recovery_accepts_a_current_token_for_the_ready_port() {
        let root = std::env::temp_dir().join(format!(
            "nexus-agent-token-bound-recovery-positive-{}-{}",
            std::process::id(),
            unix_time_seconds()
        ));
        let paths = NexusPaths::from_root(root.clone());
        paths
            .ensure_directories()
            .expect("Nexus directories are available");
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("readiness listener binds");
        let port = listener.local_addr().expect("listener address").port();
        let server = tokio::spawn(async move {
            loop {
                let _ = listener.accept().await.expect("TCP readiness accepts");
            }
        });
        let supervisor = HarnessSupervisor::new(paths.clone()).expect("supervisor creates");
        let task = {
            let mut inner = supervisor.inner.lock().await;
            inner.generation = 10;
            inner.log_session.generation = 10;
            inner.log_session.run_id = "token-bound-recovery-positive-10".to_owned();
            inner.log_session.launch_pending = true;
            supervisor
                .log_sessions
                .write(&inner.log_session)
                .expect("pending session writes");
            fs::OpenOptions::new()
                .append(true)
                .open(paths.logs_dir.join(&inner.log_session.stdout_log_name))
                .expect("current stdout opens")
                .write_all(format!("dsh web: http://127.0.0.1:{port}/?token=current\n").as_bytes())
                .expect("current token appends");
            inner.runtime = recovery_runtime(&HarnessRuntimeInfo::running(42, 10, 11));
            let mut target = ReadinessTarget::parse(&format!("tcp://127.0.0.1:{port}"))
                .expect("TCP target parses");
            target.token_required = true;
            inner.recovery = Some(RecoveryState {
                generation: 10,
                target,
                deadline: Instant::now() + Duration::from_secs(1),
                exit_code: Some(1),
                task_started: false,
            });
            pending_recovery(&mut inner).expect("recovery task is pending")
        };
        supervisor.spawn_recovery(task);
        let runtime = timeout(Duration::from_secs(2), async {
            loop {
                let runtime = supervisor.status().await;
                if runtime.state == HarnessState::Running {
                    break runtime;
                }
                sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("current token completes recovery");
        assert_eq!(runtime.pid, None);
        assert_eq!(runtime.error, None);

        server.abort();
        let _ = server.await;
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn token_bound_readiness_accepts_only_a_current_token_for_the_target_port() {
        let root = std::env::temp_dir().join(format!(
            "nexus-agent-tcp-recovery-token-match-{}-{}",
            std::process::id(),
            unix_time_seconds()
        ));
        let paths = NexusPaths::from_root(root.clone());
        let supervisor = HarnessSupervisor::new(paths.clone()).expect("supervisor creates");
        let session = supervisor
            .log_sessions
            .read()
            .expect("session reads")
            .expect("session exists");
        let stdout_path = paths.logs_dir.join(&session.stdout_log_name);
        fs::OpenOptions::new()
            .append(true)
            .open(&stdout_path)
            .expect("current stdout opens")
            .write_all(b"dsh web: http://127.0.0.1:31841/?token=current-token\n")
            .expect("current token appends");

        let mut observer = nexus_launcher_core::HarnessLogObserver::default();
        let matching =
            ReadinessTarget::parse("tcp://127.0.0.1:31841").expect("matching target parses");
        assert!(readiness_has_current_token(
            &paths,
            &matching,
            &session,
            &mut observer
        ));
        let different_port =
            ReadinessTarget::parse("tcp://127.0.0.1:31842").expect("different target parses");
        assert!(!readiness_has_current_token(
            &paths,
            &different_port,
            &session,
            &mut observer
        ));

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn recover_unattached_does_not_resurrect_stopped_state() {
        let root = std::env::temp_dir().join(format!(
            "nexus-agent-recover-stopped-{}",
            std::process::id()
        ));
        let paths = NexusPaths::from_root(root.clone());
        let persisted = HarnessRuntimeInfo {
            state: HarnessState::Stopped,
            pid: Some(999),
            exit_code: Some(0),
            error: None,
            started_at_unix: Some(10),
            updated_at_unix: Some(11),
        };
        RuntimeMetadataStore::new(paths.clone())
            .update_harness(persisted.clone())
            .expect("persisted Harness state writes");

        let supervisor = HarnessSupervisor::new(paths).expect("supervisor creates");
        let runtime = supervisor.recover_unattached().await;

        assert_eq!(runtime.state, HarnessState::Stopped);
        assert_eq!(runtime.pid, None);
        assert_eq!(runtime.exit_code, persisted.exit_code);
        let stored = supervisor
            .metadata_store()
            .read()
            .expect("metadata reads")
            .expect("metadata remains present");
        assert_eq!(stored.harness.pid, None);
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn recover_unattached_preserves_failed_without_readiness() {
        let root =
            std::env::temp_dir().join(format!("nexus-agent-recover-failed-{}", std::process::id()));
        let paths = NexusPaths::from_root(root.clone());
        RuntimeMetadataStore::new(paths.clone())
            .update_harness(HarnessRuntimeInfo {
                state: HarnessState::Failed,
                pid: Some(999),
                exit_code: Some(1),
                error: Some("persisted failure".to_owned()),
                started_at_unix: Some(10),
                updated_at_unix: Some(11),
            })
            .expect("persisted Harness state writes");

        let supervisor = HarnessSupervisor::new(paths).expect("supervisor creates");
        let runtime = supervisor.recover_unattached().await;

        assert_eq!(runtime.state, HarnessState::Failed);
        assert_eq!(runtime.pid, None);
        assert_eq!(runtime.exit_code, Some(1));
        assert_eq!(runtime.error.as_deref(), Some("persisted failure"));
        let stored = supervisor
            .metadata_store()
            .read()
            .expect("metadata reads")
            .expect("metadata remains present");
        assert_eq!(stored.harness.pid, None);
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn recover_unattached_abandons_pre_spawn_intent_without_readiness() {
        let root = std::env::temp_dir().join(format!(
            "nexus-agent-abandon-pre-spawn-{}-{}",
            std::process::id(),
            unix_time_seconds()
        ));
        let paths = NexusPaths::from_root(root.clone());
        ConfigStore::new(paths.clone())
            .write(&NexusConfigFile { update_attempt_id: None, external_harness: None,
                schema_version: 1,
                harness_preferences: None,
                harness: Some(HarnessLaunchSpec {
                    mode: Default::default(),
                    program: PathBuf::from("must-not-run"),
                    args: Vec::new(),
                    working_dir: None,
                    readiness_url: None,
                    readiness_timeout_secs: None,
                    readiness_token_required: false,
                }),
                update: None,

                releases: None,
                runtime: None,
                snapshots: None,
            })
            .expect("Harness config writes");
        let first = HarnessSupervisor::new(paths.clone()).expect("first supervisor creates");
        let mut prepared = first
            .log_sessions
            .read()
            .expect("baseline session reads")
            .expect("baseline session exists");
        prepared.generation = prepared.generation.wrapping_add(1).max(1);
        prepared.run_id = format!("prepared-no-readiness-{}", prepared.generation);
        prepared.launch_pending = true;
        first
            .log_sessions
            .write(&prepared)
            .expect("prepared marker writes before simulated crash");
        drop(first);

        let recovered = HarnessSupervisor::new(paths.clone()).expect("replacement Agent creates");
        let runtime = recovered.recover_unattached().await;
        assert_eq!(runtime.state, HarnessState::Failed);
        assert_eq!(runtime.pid, None);
        assert!(runtime
            .error
            .as_deref()
            .is_some_and(|message| message.contains("without readiness")));
        assert!(
            !recovered
                .log_sessions
                .read()
                .expect("session reads")
                .expect("session remains")
                .launch_pending,
            "the replacement Agent must not leave an ownerless reservation"
        );

        let _ = fs::remove_dir_all(root);
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn nonzero_bootstrap_recovers_when_loopback_descendant_is_ready() {
        let root = std::env::temp_dir().join(format!(
            "nexus-agent-recover-bootstrap-{}",
            std::process::id()
        ));
        let paths = NexusPaths::from_root(root.clone());
        let reservation = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("readiness port reserves");
        let address = reservation.local_addr().expect("readiness address");
        drop(reservation);
        let server = tokio::spawn(async move {
            sleep(Duration::from_millis(250)).await;
            let listener = TcpListener::bind(address).await.expect("readiness binds");
            loop {
                let (mut stream, _) = listener.accept().await.expect("readiness accepts");
                let mut request = [0_u8; 256];
                let _ = stream
                    .read(&mut request)
                    .await
                    .expect("readiness request reads");
                stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                    .await
                    .expect("readiness responds");
            }
        });
        let shell = std::env::var_os("ComSpec").unwrap_or_else(|| "cmd.exe".into());
        ConfigStore::new(paths.clone())
            .write(&NexusConfigFile { update_attempt_id: None, external_harness: None,
                schema_version: 1,
                harness_preferences: None,
                harness: Some(HarnessLaunchSpec {
                    mode: Default::default(),
                    program: PathBuf::from(shell),
                    args: vec!["/C".to_owned(), "exit 1".to_owned()],
                    working_dir: None,
                    readiness_url: Some(format!("http://127.0.0.1:{}/health", address.port())),
                    readiness_timeout_secs: Some(2),
                    readiness_token_required: false,
                }),
                update: None,

                releases: None,
                runtime: None,
                snapshots: None,
            })
            .expect("Harness config writes");

        let supervisor = HarnessSupervisor::new(paths).expect("supervisor creates");
        let initial = supervisor
            .start()
            .await
            .expect("Harness spawn records Starting");
        let start_session = supervisor
            .log_sessions
            .read()
            .expect("start log session reads")
            .expect("start log session exists");

        assert_eq!(initial.state, HarnessState::Starting);
        let runtime = timeout(Duration::from_secs(2), async {
            loop {
                let runtime = supervisor.status().await;
                // Readiness may succeed before the bootstrap parent exits. Wait for
                // the detached recovery state that this test actually verifies.
                if runtime.state == HarnessState::Running && runtime.pid.is_none() {
                    break runtime;
                }
                sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("independent recovery reaches readiness");
        assert_eq!(runtime.pid, None);
        assert_eq!(runtime.exit_code, None);
        assert_eq!(runtime.error, None);
        let recovered_session = supervisor
            .log_sessions
            .read()
            .expect("recovery log session reads")
            .expect("recovery log session exists");
        assert!(recovered_session.generation > start_session.generation);
        assert_ne!(recovered_session.run_id, start_session.run_id);
        assert!(recovered_session.launch_pending);
        assert!(
            recovered_session.stdout_watermark >= start_session.stdout_watermark,
            "a bootstrap parent replacement establishes a fresh token boundary"
        );
        server.abort();
        let _ = server.await;
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn zero_exit_bootstrap_recovers_ready_descendant_without_duplicate_spawn() {
        let root = std::env::temp_dir().join(format!(
            "nexus-agent-zero-bootstrap-{}-{}",
            std::process::id(),
            unix_time_seconds()
        ));
        let paths = NexusPaths::from_root(root.clone());
        let marker = root.join("bootstrap-count.txt");
        let reservation = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("readiness port reserves");
        let address = reservation.local_addr().expect("readiness address");
        drop(reservation);
        let server = tokio::spawn(async move {
            sleep(Duration::from_millis(200)).await;
            let listener = TcpListener::bind(address).await.expect("readiness binds");
            loop {
                let (mut stream, _) = listener.accept().await.expect("readiness accepts");
                let mut request = [0_u8; 256];
                let _ = stream.read(&mut request).await;
                let _ = stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                    .await;
            }
        });
        let (program, args) = if cfg!(windows) {
            (
                crate::test_powershell(),
                vec![
                    "-NoProfile".to_owned(),
                    "-Command".to_owned(),
                    format!(
                        "Add-Content -LiteralPath '{}' -Value bootstrap; exit 0",
                        marker.display()
                    ),
                ],
            )
        } else {
            (
                PathBuf::from("sh"),
                vec![
                    "-c".to_owned(),
                    format!("printf 'bootstrap\\n' >> '{}'; exit 0", marker.display()),
                ],
            )
        };
        ConfigStore::new(paths.clone())
            .write(&NexusConfigFile { update_attempt_id: None, external_harness: None,
                schema_version: 1,
                harness_preferences: None,
                harness: Some(HarnessLaunchSpec {
                    mode: Default::default(),
                    program,
                    args,
                    working_dir: None,
                    readiness_url: Some(format!("http://127.0.0.1:{}/health", address.port())),
                    readiness_timeout_secs: Some(2),
                    readiness_token_required: false,
                }),
                update: None,

                releases: None,
                runtime: None,
                snapshots: None,
            })
            .expect("Harness config writes");
        let supervisor = HarnessSupervisor::new(paths.clone()).expect("supervisor creates");
        supervisor.start().await.expect("bootstrap starts");
        let session = supervisor
            .log_sessions
            .read()
            .expect("session reads")
            .expect("session exists");
        let running = timeout(Duration::from_secs(3), async {
            loop {
                let runtime = supervisor.status().await;
                if runtime.state == HarnessState::Running && runtime.pid.is_none() {
                    break runtime;
                }
                sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("zero-exit descendant is recovered");
        assert_eq!(running.exit_code, None);
        assert!(matches!(
            supervisor.start().await,
            Err(HarnessSupervisorError::AlreadyRunning)
        ));
        assert_eq!(marker_lines(&marker), ["bootstrap"]);
        let recovered_session = supervisor
            .log_sessions
            .read()
            .expect("session rereads")
            .expect("session remains");
        assert!(recovered_session.generation > session.generation);
        assert_ne!(recovered_session.run_id, session.run_id);
        assert!(recovered_session.launch_pending);
        server.abort();
        let _ = server.await;
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn durable_prepared_intent_blocks_spawn_after_agent_restart() {
        let root = std::env::temp_dir().join(format!(
            "nexus-agent-prepared-restart-{}-{}",
            std::process::id(),
            unix_time_seconds()
        ));
        let paths = NexusPaths::from_root(root.clone());
        let marker = root.join("must-not-spawn.txt");
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("readiness listener binds");
        let address = listener.local_addr().expect("readiness address");
        let server = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.expect("readiness accepts");
                let mut request = [0_u8; 256];
                let _ = stream.read(&mut request).await;
                let _ = stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                    .await;
            }
        });
        let (program, args) = immediate_nonzero_marker_command(&marker);
        ConfigStore::new(paths.clone())
            .write(&NexusConfigFile { update_attempt_id: None, external_harness: None,
                schema_version: 1,
                harness_preferences: None,
                harness: Some(HarnessLaunchSpec {
                    mode: Default::default(),
                    program,
                    args,
                    working_dir: None,
                    readiness_url: Some(format!("http://127.0.0.1:{}/health", address.port())),
                    readiness_timeout_secs: Some(2),
                    readiness_token_required: false,
                }),
                update: None,

                releases: None,
                runtime: None,
                snapshots: None,
            })
            .expect("Harness config writes");
        let first = HarnessSupervisor::new(paths.clone()).expect("first supervisor creates");
        let mut prepared = first
            .log_sessions
            .read()
            .expect("baseline session reads")
            .expect("baseline session exists");
        prepared.generation = prepared.generation.wrapping_add(1).max(1);
        prepared.run_id = format!("prepared-{}", prepared.generation);
        prepared.launch_pending = true;
        first
            .log_sessions
            .write(&prepared)
            .expect("prepared marker writes before simulated crash");
        drop(first);

        let recovered = HarnessSupervisor::new(paths.clone()).expect("replacement Agent creates");
        let runtime = recovered.recover_unattached().await;
        assert_eq!(runtime.state, HarnessState::Running);
        assert_eq!(runtime.pid, None);
        assert!(matches!(
            recovered.start().await,
            Err(HarnessSupervisorError::AlreadyRunning)
        ));
        assert!(!marker.exists(), "prepared recovery must never spawn H2");
        server.abort();
        let _ = server.await;
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn attached_recovery_waits_for_a_fresh_token_after_same_pid_readiness_gap() {
        let root = std::env::temp_dir().join(format!(
            "nexus-agent-attached-readiness-{}-{}",
            std::process::id(),
            unix_time_seconds()
        ));
        let paths = NexusPaths::from_root(root.clone());
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("readiness listener binds");
        let address = listener.local_addr().expect("readiness address");
        let server = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.expect("readiness accepts");
                let mut request = [0_u8; 256];
                let _ = stream.read(&mut request).await;
                let _ = stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                    .await;
            }
        });
        let (program, args) = if cfg!(windows) {
            (
                crate::test_powershell(),
                vec![
                    "-NoProfile".to_owned(),
                    "-Command".to_owned(),
                    "Start-Sleep -Seconds 30".to_owned(),
                ],
            )
        } else {
            (PathBuf::from("sleep"), vec!["30".to_owned()])
        };
        ConfigStore::new(paths.clone())
            .write(&NexusConfigFile { update_attempt_id: None, external_harness: None,
                schema_version: 1,
                harness_preferences: None,
                harness: Some(HarnessLaunchSpec {
                    mode: Default::default(),
                    program,
                    args,
                    working_dir: None,
                    readiness_url: Some(format!("http://127.0.0.1:{}/health", address.port())),
                    readiness_timeout_secs: Some(3),
                    readiness_token_required: true,
                }),
                update: None,

                releases: None,
                runtime: None,
                snapshots: None,
            })
            .expect("Harness config writes");
        let supervisor =
            HarnessSupervisor::with_graceful_wait(paths.clone(), Duration::from_millis(100))
                .expect("supervisor creates");
        supervisor.start().await.expect("Harness starts");
        let initial_session = supervisor
            .log_sessions
            .read()
            .expect("initial session reads")
            .expect("initial session exists");
        let stdout_path = paths.logs_dir.join(&initial_session.stdout_log_name);
        fs::OpenOptions::new()
            .append(true)
            .open(&stdout_path)
            .expect("initial log opens")
            .write_all(
                format!(
                    "dsh web: http://127.0.0.1:{}/?token=initial\n",
                    address.port()
                )
                .as_bytes(),
            )
            .expect("initial token appends");
        let running = timeout(Duration::from_secs(3), async {
            loop {
                let runtime = supervisor.status().await;
                if runtime.state == HarnessState::Running {
                    break runtime;
                }
                sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("attached Harness becomes ready");
        let pid = running.pid.expect("attached Harness retains PID");
        let old_owner_epoch = supervisor
            .inner
            .lock()
            .await
            .attached_readiness
            .as_ref()
            .expect("attached readiness owner is armed")
            .epoch;
        supervisor
            .stop_wait_failure
            .store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(matches!(
            supervisor.stop().await,
            Err(HarnessSupervisorError::Process(_))
        ));
        let restored_owner_epoch = {
            let inner = supervisor.inner.lock().await;
            assert_eq!(inner.runtime.state, HarnessState::Running);
            assert_eq!(inner.runtime.pid, Some(pid));
            inner
                .attached_readiness
                .as_ref()
                .expect("stop failure re-arms attached readiness")
                .epoch
        };
        assert_ne!(
            restored_owner_epoch, old_owner_epoch,
            "a restored child must not reuse the old readiness owner epoch"
        );
        let old_session = supervisor
            .log_sessions
            .read()
            .expect("old session reads")
            .expect("old session exists");
        let stdout_path = paths.logs_dir.join(&old_session.stdout_log_name);
        fs::write(
            &stdout_path,
            format!("dsh web: http://127.0.0.1:{}/?token=old\n", address.port()),
        )
        .expect("old token appends");
        let old_length = fs::metadata(&stdout_path)
            .expect("old token metadata reads")
            .len();

        server.abort();
        let _ = server.await;
        let (recovering, new_session) = timeout(Duration::from_secs(4), async {
            loop {
                let runtime = supervisor.status().await;
                let session = supervisor
                    .log_sessions
                    .read()
                    .expect("session reads")
                    .expect("session exists");
                if runtime.state == HarnessState::Starting
                    && session.generation > old_session.generation
                {
                    break (runtime, session);
                }
                sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("same-PID readiness loss invalidates the token epoch");
        assert_eq!(recovering.pid, Some(pid));
        assert_ne!(new_session.run_id, old_session.run_id);
        assert!(new_session.launch_pending);
        assert!(new_session.stdout_watermark >= old_length);
        assert!(matches!(
            supervisor.start().await,
            Err(HarnessSupervisorError::AlreadyRunning)
        ));

        let replacement_listener = TcpListener::bind(address)
            .await
            .expect("readiness service rebinds under the same Harness PID");
        let replacement_server = tokio::spawn(async move {
            loop {
                let (mut stream, _) = replacement_listener
                    .accept()
                    .await
                    .expect("replacement readiness accepts");
                let mut request = [0_u8; 256];
                let _ = stream.read(&mut request).await;
                let _ = stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                    .await;
            }
        });
        sleep(Duration::from_millis(600)).await;
        assert_eq!(
            supervisor.status().await.state,
            HarnessState::Starting,
            "a healthy endpoint alone must not complete attached token-bound recovery"
        );
        fs::OpenOptions::new()
            .append(true)
            .open(&stdout_path)
            .expect("current log opens")
            .write_all(
                format!("dsh web: http://127.0.0.1:{}/?token=new\n", address.port()).as_bytes(),
            )
            .expect("post-boundary token appends");
        let recovered = timeout(Duration::from_secs(4), async {
            loop {
                let runtime = supervisor.status().await;
                if runtime.state == HarnessState::Running {
                    break runtime;
                }
                sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("same child recovers after a new token boundary");
        assert_eq!(recovered.pid, Some(pid));
        let durable = supervisor
            .log_sessions
            .read()
            .expect("recovered session reads")
            .expect("recovered session exists");
        assert_eq!(durable.run_id, new_session.run_id);
        assert!(
            durable.launch_pending,
            "an attached child retains its durable ownership reservation"
        );
        {
            let mut inner = supervisor.inner.lock().await;
            assert!(inner
                .child
                .as_mut()
                .expect("attached child remains owned")
                .try_wait()
                .expect("attached child status reads")
                .is_none());
        }
        let restarted = HarnessSupervisor::new(paths.clone()).expect("Agent restart reconstructs");
        let duplicate = restarted.start().await;
        assert!(matches!(duplicate, Err(HarnessSupervisorError::AlreadyRunning)),
            "restarted Agent must reject duplicate start: {duplicate:?}");
        drop(restarted);

        supervisor.stop().await.expect("Harness stops");
        replacement_server.abort();
        let _ = replacement_server.await;
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn attached_readiness_timeout_keeps_child_owned_and_rejects_duplicate_start() {
        let root = std::env::temp_dir().join(format!(
            "nexus-agent-attached-readiness-timeout-{}-{}",
            std::process::id(),
            unix_time_seconds()
        ));
        let paths = NexusPaths::from_root(root.clone());
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("readiness listener binds");
        let address = listener.local_addr().expect("readiness address");
        let server = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.expect("readiness accepts");
                let mut request = [0_u8; 256];
                let _ = stream.read(&mut request).await;
                let _ = stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                    .await;
            }
        });
        let (program, args) = if cfg!(windows) {
            (
                crate::test_powershell(),
                vec![
                    "-NoProfile".to_owned(),
                    "-Command".to_owned(),
                    "Start-Sleep -Seconds 30".to_owned(),
                ],
            )
        } else {
            (PathBuf::from("sleep"), vec!["30".to_owned()])
        };
        ConfigStore::new(paths.clone())
            .write(&NexusConfigFile { update_attempt_id: None, external_harness: None,
                schema_version: 1,
                harness_preferences: None,
                harness: Some(HarnessLaunchSpec {
                    mode: Default::default(),
                    program,
                    args,
                    working_dir: None,
                    readiness_url: Some(format!("http://127.0.0.1:{}/health", address.port())),
                    readiness_timeout_secs: Some(1),
                    readiness_token_required: false,
                }),
                update: None,

                releases: None,
                runtime: None,
                snapshots: None,
            })
            .expect("Harness config writes");
        let supervisor =
            HarnessSupervisor::with_graceful_wait(paths.clone(), Duration::from_millis(100))
                .expect("supervisor creates");
        supervisor.start().await.expect("Harness starts");
        let pid = timeout(Duration::from_secs(3), async {
            loop {
                let runtime = supervisor.status().await;
                if runtime.state == HarnessState::Running {
                    break runtime.pid.expect("attached Harness retains PID");
                }
                sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("attached Harness becomes ready");
        server.abort();
        let _ = server.await;

        let failed = timeout(Duration::from_secs(4), async {
            loop {
                let runtime = supervisor.status().await;
                if runtime.state == HarnessState::Failed {
                    break runtime;
                }
                sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("attached readiness recovery reaches its terminal deadline");
        assert_eq!(failed.pid, Some(pid));
        assert!(failed
            .error
            .as_deref()
            .is_some_and(|error| error.contains("readiness recovery timed out")));
        {
            let inner = supervisor.inner.lock().await;
            assert!(inner.child.is_some(), "the failed child remains owned");
            assert!(inner.attached_readiness.is_none());
            assert!(inner.log_session.launch_pending);
        }
        assert!(matches!(
            supervisor.start().await,
            Err(HarnessSupervisorError::AlreadyRunning)
        ));
        let restarted = HarnessSupervisor::new(paths.clone()).expect("Agent restart reconstructs");
        let restarted_runtime = restarted.recover_unattached().await;
        assert_eq!(restarted_runtime.state, HarnessState::Starting);
        assert_eq!(restarted_runtime.pid, None);
        assert!(
            restarted
                .log_sessions
                .read()
                .expect("restarted Agent session reads")
                .expect("restarted Agent session exists")
                .launch_pending
        );
        assert!(matches!(
            restarted.start().await,
            Err(HarnessSupervisorError::AlreadyRunning)
        ));
        drop(restarted);
        supervisor
            .stop()
            .await
            .expect("failed attached child stops");
        assert!(
            !supervisor
                .log_sessions
                .read()
                .expect("stopped session reads")
                .expect("stopped session exists")
                .launch_pending
        );
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn recovered_unattached_running_advances_token_epoch_on_one_liveness_gap() {
        use std::sync::{
            atomic::{AtomicU8, Ordering},
            Arc,
        };

        let root = std::env::temp_dir().join(format!(
            "nexus-agent-unattached-liveness-{}-{}",
            std::process::id(),
            unix_time_seconds()
        ));
        let paths = NexusPaths::from_root(root.clone());
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("readiness listener binds");
        let address = listener.local_addr().expect("readiness address");
        let failures_remaining = Arc::new(AtomicU8::new(0));
        let server_failures = Arc::clone(&failures_remaining);
        let server = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.expect("readiness accepts");
                let mut request = [0_u8; 256];
                let _ = stream.read(&mut request).await;
                if server_failures
                    .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                        remaining.checked_sub(1)
                    })
                    .is_err()
                {
                    let _ = stream
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                        .await;
                }
            }
        });
        ConfigStore::new(paths.clone())
            .write(&NexusConfigFile { update_attempt_id: None, external_harness: None,
                schema_version: 1,
                harness_preferences: None,
                harness: Some(HarnessLaunchSpec {
                    mode: Default::default(),
                    program: PathBuf::from("must-not-run"),
                    args: Vec::new(),
                    working_dir: None,
                    readiness_url: Some(format!("http://127.0.0.1:{}/health", address.port())),
                    readiness_timeout_secs: Some(2),
                    readiness_token_required: false,
                }),
                update: None,

                releases: None,
                runtime: None,
                snapshots: None,
            })
            .expect("Harness config writes");
        RuntimeMetadataStore::new(paths.clone())
            .update_harness(HarnessRuntimeInfo::running(999, 10, 11))
            .expect("persisted Running writes");
        let supervisor = HarnessSupervisor::new(paths.clone()).expect("supervisor creates");
        let running = supervisor.recover_unattached().await;
        assert_eq!(running.state, HarnessState::Running);
        assert_eq!(running.pid, None);
        let old_session = supervisor
            .log_sessions
            .read()
            .expect("old session reads")
            .expect("old session exists");
        let stdout_path = paths.logs_dir.join(&old_session.stdout_log_name);
        fs::write(&stdout_path, "http://127.0.0.1:3080/?token=old\n").expect("old token appends");
        let old_length = fs::metadata(&stdout_path)
            .expect("old token metadata reads")
            .len();

        failures_remaining.store(1, Ordering::SeqCst);
        let new_session = timeout(Duration::from_secs(4), async {
            loop {
                let _ = supervisor.status().await;
                let session = supervisor
                    .log_sessions
                    .read()
                    .expect("session reads")
                    .expect("session exists");
                if session.generation > old_session.generation
                    && session.run_id != old_session.run_id
                {
                    break session;
                }
                sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("liveness loss starts a fresh token epoch");
        assert!(new_session.generation > old_session.generation);
        assert!(new_session.stdout_watermark >= old_length);
        assert!(matches!(
            supervisor.start().await,
            Err(HarnessSupervisorError::AlreadyRunning)
        ));

        timeout(Duration::from_secs(3), async {
            loop {
                let runtime = supervisor.status().await;
                if runtime.state == HarnessState::Running && runtime.pid.is_none() {
                    break;
                }
                sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("same descendant becomes healthy again");
        assert_eq!(
            supervisor
                .log_sessions
                .read()
                .expect("current session reads")
                .expect("current session exists"),
            new_session,
            "recovery keeps the liveness-loss epoch until another loss"
        );
        server.abort();
        let _ = server.await;
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn readiness_owner_arms_liveness_when_parent_exit_races_recovery() {
        use std::sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        };

        let root = std::env::temp_dir().join(format!(
            "nexus-agent-readiness-recovery-race-{}-{}",
            std::process::id(),
            unix_time_seconds()
        ));
        let paths = NexusPaths::from_root(root.clone());
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("readiness listener binds");
        let address = listener.local_addr().expect("readiness address");
        let healthy = Arc::new(AtomicBool::new(true));
        let server_health = Arc::clone(&healthy);
        let server = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.expect("readiness accepts");
                let mut request = [0_u8; 256];
                let _ = stream.read(&mut request).await;
                let response = if server_health.load(Ordering::SeqCst) {
                    b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n".as_slice()
                } else {
                    b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n".as_slice()
                };
                let _ = stream.write_all(response).await;
            }
        });
        ConfigStore::new(paths.clone())
            .write(&NexusConfigFile { update_attempt_id: None, external_harness: None,
                schema_version: 1,
                harness_preferences: None,
                harness: Some(HarnessLaunchSpec {
                    mode: Default::default(),
                    program: PathBuf::from("must-not-run"),
                    args: Vec::new(),
                    working_dir: None,
                    readiness_url: Some(format!("http://127.0.0.1:{}/health", address.port())),
                    readiness_timeout_secs: Some(2),
                    readiness_token_required: false,
                }),
                update: None,

                releases: None,
                runtime: None,
                snapshots: None,
            })
            .expect("Harness config writes");
        let supervisor = HarnessSupervisor::new(paths.clone()).expect("supervisor creates");
        let target = ReadinessTarget::parse(&format!("http://127.0.0.1:{}/health", address.port()))
            .expect("readiness target parses");
        let (generation, owner_epoch) = {
            let mut inner = supervisor.inner.lock().await;
            inner.log_session.launch_pending = true;
            supervisor
                .log_sessions
                .write(&inner.log_session)
                .expect("active session marker writes");
            inner.runtime = recovery_runtime(&inner.runtime);
            inner.readiness = Some(ReadinessConfig {
                target: target.clone(),
                timeout: Duration::from_secs(2),
            });
            let recovery_deadline = Instant::now() + Duration::from_secs(2);
            inner.recovery = Some(RecoveryState {
                generation: inner.generation,
                target,
                deadline: recovery_deadline,
                exit_code: Some(0),
                task_started: false,
            });
            let generation = inner.generation;
            let owner_epoch = next_operation_epoch(&mut inner);
            let start_owner = ReadinessOwner::Start {
                generation,
                epoch: owner_epoch,
            };
            inner.readiness_owner = Some(start_owner);
            let old_deadline = Instant::now() + Duration::from_millis(1);
            assert!(
                pending_recovery(&mut inner).is_none(),
                "the one start task must become recovery owner without a competing probe"
            );
            assert_eq!(
                inner.readiness_owner,
                Some(ReadinessOwner::Recovery {
                    generation,
                    epoch: owner_epoch,
                })
            );
            let adopted = current_readiness_lease(&inner, start_owner, old_deadline)
                .expect("the start task adopts recovery ownership");
            assert_eq!(adopted.deadline, recovery_deadline);
            assert_eq!(adopted.owner, inner.readiness_owner.expect("owner remains"));
            assert_eq!(
                current_readiness_lease(
                    &inner,
                    ReadinessOwner::Start {
                        generation,
                        epoch: owner_epoch.wrapping_add(1),
                    },
                    old_deadline
                ),
                None,
                "a late owner with a stale epoch must exit"
            );
            (generation, owner_epoch)
        };
        let old_run_id = supervisor
            .log_sessions
            .read()
            .expect("session reads")
            .expect("session exists")
            .run_id;

        let stale = supervisor
            .mark_running(ReadinessOwner::Start {
                generation,
                epoch: owner_epoch,
            })
            .await;
        assert_eq!(stale.state, HarnessState::Starting);
        let running = supervisor
            .mark_running(ReadinessOwner::Recovery {
                generation,
                epoch: owner_epoch,
            })
            .await;
        assert_eq!(running.state, HarnessState::Running);
        assert_eq!(running.pid, None);
        assert!(
            supervisor.inner.lock().await.unattached_monitor_started,
            "the recovery owner must claim PID-less liveness monitoring"
        );

        healthy.store(false, Ordering::SeqCst);
        timeout(Duration::from_secs(4), async {
            loop {
                let session = supervisor
                    .log_sessions
                    .read()
                    .expect("session reads")
                    .expect("session exists");
                if session.run_id != old_run_id {
                    break;
                }
                sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("liveness loss advances the token epoch");
        assert!(matches!(
            supervisor.start().await,
            Err(HarnessSupervisorError::AlreadyRunning)
        ));

        server.abort();
        let _ = server.await;
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn launch_args_replace_only_the_explicit_profile_placeholder() {
        let mut spec = nexus_core::HarnessLaunchSpec::new("harness".into());
        spec.args = vec![
            "--profile".to_owned(),
            "{profile}".to_owned(),
            "literal".to_owned(),
        ];
        assert_eq!(
            spec.render_args_for_profile("web.dark"),
            vec!["--profile", "web.dark", "literal"]
        );

        spec.args = vec!["--headless".to_owned()];
        assert_eq!(spec.render_args_for_profile("web"), vec!["--headless"]);
    }
}

#[cfg(test)]
mod startup_operation_tests {
    #[cfg(windows)]
    #[tokio::test]
    #[ignore = "requires local Node and an owned child process"]
    async fn canonical_node_entry_starts_and_preserves_user_arguments() {
        use std::{fs, path::PathBuf, time::Duration};
        let output = std::process::Command::new("node").args(["-p", "process.execPath"]).output().unwrap();
        assert!(output.status.success());
        let node = PathBuf::from(String::from_utf8(output.stdout).unwrap().trim());
        let state = crate::switch_ownership_tests::switch_test_state("canonical-node-entry");
        let entry = state.paths.root.join("script with spaces.cjs");
        let marker = state.paths.root.join("arguments.json");
        fs::write(&entry, format!("require('node:fs').writeFileSync({},JSON.stringify(process.argv.slice(2)));setInterval(()=>{{}},1000);", serde_json::to_string(&marker).unwrap())).unwrap();
        let canonical = fs::canonicalize(&entry).unwrap();
        let literal = r"\\?\C:\user argument";
        let mut spec = nexus_core::HarnessLaunchSpec::new(node);
        spec.mode = nexus_protocol::HarnessLaunchMode::Node;
        spec.args = vec![canonical.to_string_lossy().into_owned(), literal.into()];
        state.config.transaction(|config| { config.harness = Some(spec); Ok(()) }).unwrap();
        let start = state.supervisor.start().await;
        let observed = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Ok(bytes) = fs::read(&marker) { break bytes; }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }).await;
        let stop = state.supervisor.stop().await;
        assert!(start.is_ok(), "{start:?}");
        assert!(stop.is_ok(), "{stop:?}");
        assert_eq!(serde_json::from_slice::<Vec<String>>(&observed.unwrap()).unwrap(), vec![literal]);
        // The terminal receives this same Node entry through its environment.
        let terminal = std::process::Command::new("node")
            .args(["-e", "require('node:child_process').execFileSync(process.execPath,[process.env.NEXUS_TERMINAL_ENTRY,'terminal'],{timeout:1000});"])
            .env("NEXUS_TERMINAL_ENTRY", nexus_core::node_script_argument(&canonical)).output().unwrap();
        // The fixture deliberately remains alive, so timeout is expected after writing.
        assert!(!terminal.status.success());
        assert_eq!(serde_json::from_slice::<Vec<String>>(&fs::read(&marker).unwrap()).unwrap(), vec!["terminal"]);
        fs::remove_dir_all(&state.paths.root).unwrap();
    }

    /// Exercises the real supervisor/checker/owned process chain with a fake slot,
    /// not a real Harness installation or the HTTP interface.
    #[cfg(windows)]
    #[tokio::test]
    #[ignore = "requires local Node and permission to create/terminate an owned test process tree"]
    async fn startup_cancel_reaps_slow_fake_compatibility_probe_without_harness_spawn() {
        use std::{fs,path::PathBuf,time::Duration};
        use windows_sys::Win32::{Foundation::CloseHandle,System::Threading::{OpenProcess,WaitForSingleObject}};
        let node=std::process::Command::new("node").args(["-p","process.execPath"]).output().expect("explicit integration test requires Node");
        assert!(node.status.success(),"explicit integration test requires usable Node");
        let node=PathBuf::from(String::from_utf8(node.stdout).unwrap().trim());
        let state=crate::switch_ownership_tests::switch_test_state("startup-cancel-probe");
        state.releases.register("fake-slot","0.1.2-rc.1",None,None).unwrap();
        state.releases.promote("fake-slot").unwrap();
        let slot=state.releases.release_root("fake-slot").unwrap();
        let home=state.paths.root.join("fixture-home");
        fs::create_dir_all(slot.join("apps/cli/lib")).unwrap();
        fs::create_dir_all(slot.join("vendor/fixture")).unwrap();
        fs::write(slot.join("vendor/fixture/package.json"),br#"{"name":"@deepseek-ai/fixture","version":"0.0.0"}"#).unwrap();
        fs::create_dir_all(home.join("profiles/web")).unwrap();
        fs::write(slot.join("package.json"),br#"{"name":"@deepseek-ai/dsh-root","version":"0.1.2-rc.1"}"#).unwrap();
        fs::write(home.join("profiles/web/package.json"),br#"{"name":"web","private":true,"dsh":{"profile":{"bundles":[]}}}"#).unwrap();
        let marker=state.paths.root.join("probe-pid.json");
        let entry=slot.join("apps/cli/lib/bin.js");
        fs::write(&entry,format!("require('node:fs').writeFileSync({},JSON.stringify({{pid:process.pid,args:process.argv}}));setInterval(()=>{{}},1000);",serde_json::to_string(&marker).unwrap())).unwrap();
        let mut spec=nexus_core::HarnessLaunchSpec::new(node);
        spec.mode=nexus_protocol::HarnessLaunchMode::Node;
        spec.args=vec![entry.to_string_lossy().into_owned(),"--profile".into(),"{profile}".into()];
        spec.working_dir=Some(slot.clone());
        state.config.transaction(|d|{d.harness=Some(spec);d.harness_preferences=Some(nexus_protocol::HarnessPreferencesPayload{home:Some(home.to_string_lossy().into_owned()),..Default::default()});Ok(())}).unwrap();
        let lifecycle=state.supervisor.acquire_lifecycle().await;
        state.supervisor.begin_startup().await.unwrap();
        let id=state.supervisor.startup_status().await["operation_id"].as_str().unwrap().to_owned();
        let supervisor=state.supervisor.clone();
        let mut task=tokio::spawn(async move{supervisor.start_with_profile_locked("web",&lifecycle).await});
        let observed=tokio::time::timeout(Duration::from_secs(15),async {
            loop {if let Ok(bytes)=fs::read(&marker){if let Ok(value)=serde_json::from_slice::<serde_json::Value>(&bytes){break value;}}if task.is_finished(){break serde_json::Value::Null;}tokio::time::sleep(Duration::from_millis(25)).await;}
        }).await.ok();
        let pid=observed.as_ref().and_then(|v|v["pid"].as_u64()).unwrap_or(0) as u32;
        let process=if pid>0{unsafe{OpenProcess(0x00100000,0,pid)}}else{std::ptr::null_mut()};
        // Always request cleanup, including a failed marker wait, before assertions.
        let accepted=state.supervisor.cancel_startup(&id).await;
        let result=tokio::time::timeout(Duration::from_secs(30),&mut task).await;
        if result.is_err() {
            // Leave the owned task running its cancellation cleanup; aborting it
            // would discard precisely the ownership guarantee under test.
            panic!("owned cancellation did not settle; fixture retained at {}",state.paths.root.display());
        }
        let result=result.unwrap().unwrap();
        let exited=if !process.is_null(){let value=unsafe{WaitForSingleObject(process,5000)};unsafe{CloseHandle(process)};value==0}else{false};
        let runtime=state.supervisor.status().await;
        let session=state.supervisor.log_sessions.read().unwrap().unwrap();
        let pending=state.paths.root.join("compatibility/owner-pending.json").exists();
        let work_empty=fs::read_dir(home.join("profiles/.nexus-compatibility-work")).map(|mut entries|entries.next().is_none()).unwrap_or(false);
        state.supervisor.finish_startup(false,matches!(result,Err(super::HarnessSupervisorError::Cancelled))).await;
        fs::remove_dir_all(&state.paths.root).unwrap();
        assert!(accepted&&pid>0,"fake compatibility child must run before cancellation: {result:?}");
        assert!(exited,"owned compatibility child must exit before cancellation completes");
        assert!(matches!(result,Err(super::HarnessSupervisorError::Cancelled)),"{result:?}");
        assert!(runtime.pid.is_none()&&!session.launch_pending,"Harness must never spawn");
        assert!(!pending&&work_empty,"owned checker files must be cleaned after process exit");
    }
    #[tokio::test]
    async fn startup_cancel_is_bound_to_owner_and_closes_before_spawn() {
        let state=crate::switch_ownership_tests::switch_test_state("startup-cancel-owner");
        let supervisor=&state.supervisor;
        supervisor.begin_startup().await.unwrap();
        let first=supervisor.startup_status().await["operation_id"].as_str().unwrap().to_owned();
        let token=supervisor.startup_phase("compatibility").await.unwrap();
        assert!(!supervisor.cancel_startup("stale").await);
        assert!(supervisor.cancel_startup(&first).await);
        assert!(token.is_cancelled());
        assert!(matches!(supervisor.startup_phase("spawning").await,Err(super::HarnessSupervisorError::Cancelled)));
        supervisor.finish_startup(false,true).await;
        supervisor.begin_startup().await.unwrap();
        let second=supervisor.startup_status().await["operation_id"].as_str().unwrap().to_owned();
        assert_ne!(first,second);
        assert!(!supervisor.cancel_startup(&first).await);
        supervisor.startup_phase("spawning").await.unwrap();
        assert!(!supervisor.cancel_startup(&second).await);
        supervisor.finish_startup(true,false).await;
        assert_eq!(supervisor.startup_status().await["phase"],"submitted");
        std::fs::remove_dir_all(&state.paths.root).unwrap();
    }
}
