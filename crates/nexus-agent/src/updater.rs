//! External update executor for immutable Harness release slots.

use std::{fmt, fs, io, path::Path, process::Stdio, sync::Arc, time::Duration};

use nexus_core::{
    load_update_spec, unix_time_nanos_for_update, unix_time_seconds, validate_release_id,
    validate_release_version, NexusPaths, ReleaseStore, UpdateSpec, UpdateStateStore,
};
use nexus_protocol::{ReleaseManifest, UpdateResponse, UpdateRuntimeInfo, UpdateState};
use tokio::{
    process::Command,
    sync::{oneshot, Mutex, OwnedMutexGuard},
    time::timeout,
};

#[derive(Debug)]
pub enum UpdateExecutorError {
    NotConfigured,
    AlreadyRunning,
    Configuration(io::Error),
    Spawn {
        phase: &'static str,
        source: io::Error,
    },
    Process {
        phase: &'static str,
        source: io::Error,
    },
    Failed {
        phase: &'static str,
        code: Option<i32>,
    },
    TimedOut {
        phase: &'static str,
        timeout: Duration,
    },
    Persistence(io::Error),
}

impl fmt::Display for UpdateExecutorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotConfigured => formatter.write_str(
                "update is not configured; set update.source in Nexus config.json or NEXUS_UPDATE_SOURCE",
            ),
            Self::AlreadyRunning => formatter.write_str("an update is already running"),
            Self::Configuration(error) => write!(formatter, "invalid update configuration: {error}"),
            Self::Spawn { phase, source } => write!(formatter, "failed to spawn {phase} command: {source}"),
            Self::Process { phase, source } => write!(formatter, "{phase} command failed: {source}"),
            Self::Failed { phase, code } => match code {
                Some(code) => write!(formatter, "{phase} command exited with code {code}"),
                None => write!(formatter, "{phase} command exited without a code"),
            },
            Self::TimedOut { phase, timeout } => {
                write!(formatter, "{phase} command timed out after {}s", timeout.as_secs())
            }
            Self::Persistence(error) => write!(formatter, "failed to persist update state: {error}"),
        }
    }
}

impl std::error::Error for UpdateExecutorError {}

#[derive(Clone)]
pub struct UpdateExecutor {
    paths: NexusPaths,
    releases: ReleaseStore,
    state: UpdateStateStore,
    gate: Arc<Mutex<()>>,
    #[cfg(test)]
    command_gate: Arc<Mutex<Option<UpdateCommandGate>>>,
}

#[cfg(test)]
struct UpdateCommandGate {
    reached: tokio::sync::oneshot::Sender<()>,
    release: tokio::sync::oneshot::Receiver<()>,
}

impl UpdateExecutor {
    pub fn new(paths: NexusPaths, releases: ReleaseStore) -> Self {
        Self {
            state: UpdateStateStore::new(paths.clone()),
            paths,
            releases,
            gate: Arc::new(Mutex::new(())),
            #[cfg(test)]
            command_gate: Arc::new(Mutex::new(None)),
        }
    }

    pub fn state_store(&self) -> UpdateStateStore {
        self.state.clone()
    }

    pub fn recover_unattached(&self) -> io::Result<UpdateRuntimeInfo> {
        self.state.recover_unattached()
    }

    pub fn status(&self) -> Result<UpdateRuntimeInfo, UpdateExecutorError> {
        self.state.load().map_err(UpdateExecutorError::Persistence)
    }

    pub(crate) fn try_acquire_gate(&self) -> Result<OwnedMutexGuard<()>, UpdateExecutorError> {
        Arc::clone(&self.gate)
            .try_lock_owned()
            .map_err(|_| UpdateExecutorError::AlreadyRunning)
    }

    #[cfg(test)]
    async fn observe_next_command(
        &self,
        reached: tokio::sync::oneshot::Sender<()>,
        release: tokio::sync::oneshot::Receiver<()>,
    ) {
        *self.command_gate.lock().await = Some(UpdateCommandGate { reached, release });
    }

    pub async fn install(
        &self,
        requested_id: Option<String>,
        requested_version: Option<String>,
    ) -> Result<UpdateResponse, UpdateExecutorError> {
        let guard = self.try_acquire_gate()?;
        let owner = self.clone();
        let (result_tx, result_rx) = oneshot::channel();
        tokio::spawn(async move {
            let result = owner
                .install_owned(requested_id, requested_version, guard)
                .await;
            let _ = result_tx.send(result);
        });
        result_rx.await.map_err(|_| {
            UpdateExecutorError::Persistence(io::Error::other(
                "detached update owner exited without a terminal result",
            ))
        })?
    }

    async fn install_owned(
        &self,
        requested_id: Option<String>,
        requested_version: Option<String>,
        _guard: OwnedMutexGuard<()>,
    ) -> Result<UpdateResponse, UpdateExecutorError> {
        self.state
            .recover_unattached()
            .map_err(UpdateExecutorError::Persistence)?;
        let spec = load_update_spec(&self.paths)
            .map_err(UpdateExecutorError::Configuration)?
            .ok_or(UpdateExecutorError::NotConfigured)?;
        let release_id = resolve_release_id(requested_id, &spec)?;
        let version = resolve_release_version(requested_version, &spec)?;
        let started_at = unix_time_seconds();
        let running = UpdateRuntimeInfo::running(release_id.clone(), started_at);
        self.state
            .write(&running)
            .map_err(UpdateExecutorError::Persistence)?;

        let candidate = self.paths.downloads_dir.join(format!(
            ".update-{release_id}-{}",
            unix_time_nanos_for_update()
        ));
        let result = self
            .install_inner(&spec, &candidate, &release_id, &version)
            .await;
        match result {
            Ok(release) => {
                let finished = UpdateRuntimeInfo {
                    state: UpdateState::Succeeded,
                    release_id: Some(release_id),
                    started_at_unix: Some(started_at),
                    finished_at_unix: Some(unix_time_seconds()),
                    exit_code: Some(0),
                    error: None,
                };
                self.state
                    .write(&finished)
                    .map_err(UpdateExecutorError::Persistence)?;
                Ok(UpdateResponse::new(finished, Some(release)))
            }
            Err(error) => {
                let _ = fs::remove_dir_all(&candidate);
                let failed = UpdateRuntimeInfo {
                    state: UpdateState::Failed,
                    release_id: Some(release_id),
                    started_at_unix: Some(started_at),
                    finished_at_unix: Some(unix_time_seconds()),
                    exit_code: error_exit_code(&error),
                    error: Some(error.to_string()),
                };
                match self.state.write(&failed) {
                    Ok(()) => Err(error),
                    Err(persistence) => Err(UpdateExecutorError::Persistence(io::Error::new(
                        persistence.kind(),
                        format!("{error}; failed to persist terminal update state: {persistence}"),
                    ))),
                }
            }
        }
    }

    async fn install_inner(
        &self,
        spec: &UpdateSpec,
        candidate: &Path,
        release_id: &str,
        version: &str,
    ) -> Result<ReleaseManifest, UpdateExecutorError> {
        self.paths
            .ensure_directories()
            .map_err(UpdateExecutorError::Configuration)?;
        if candidate.exists() {
            return Err(UpdateExecutorError::Configuration(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "update candidate directory already exists",
            )));
        }
        let clone_args = vec![
            "clone".to_owned(),
            "--no-tags".to_owned(),
            "--depth".to_owned(),
            "1".to_owned(),
            "--branch".to_owned(),
            spec.ref_name.clone(),
            spec.source.clone(),
            candidate.to_string_lossy().into_owned(),
        ];
        run_logged_command(
            &self.paths,
            "git-clone",
            release_id,
            &spec.git_program,
            &clone_args,
            None,
            spec.timeout(),
            #[cfg(test)]
            &self.command_gate,
        )
        .await?;

        if let Some(program) = spec.build_program.as_deref() {
            let args = spec.render_args(&spec.build_args, candidate, release_id);
            run_logged_command(
                &self.paths,
                "build",
                release_id,
                program,
                &args,
                Some(candidate),
                spec.timeout(),
                #[cfg(test)]
                &self.command_gate,
            )
            .await?;
        }
        if let Some(program) = spec.verify_program.as_deref() {
            let args = spec.render_args(&spec.verify_args, candidate, release_id);
            run_logged_command(
                &self.paths,
                "verify",
                release_id,
                program,
                &args,
                Some(candidate),
                spec.timeout(),
                #[cfg(test)]
                &self.command_gate,
            )
            .await?;
        }

        self.releases
            .register_prepared(
                candidate,
                release_id,
                version,
                Some(spec.source.clone()),
                None,
            )
            .map_err(UpdateExecutorError::Persistence)?
            .find(release_id)
            .cloned()
            .ok_or_else(|| {
                UpdateExecutorError::Persistence(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "registered release was not returned by the catalog",
                ))
            })
    }
}

fn resolve_release_id(
    requested: Option<String>,
    spec: &UpdateSpec,
) -> Result<String, UpdateExecutorError> {
    let id = requested.unwrap_or_else(|| {
        let ref_name = spec
            .ref_name
            .chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() {
                    character.to_ascii_lowercase()
                } else {
                    '-'
                }
            })
            .collect::<String>();
        format!("harness-{ref_name}-{}", unix_time_nanos_for_update())
    });
    validate_release_id(&id).map_err(UpdateExecutorError::Configuration)?;
    Ok(id)
}

fn resolve_release_version(
    requested: Option<String>,
    spec: &UpdateSpec,
) -> Result<String, UpdateExecutorError> {
    let version = requested.unwrap_or_else(|| spec.ref_name.clone());
    validate_release_version(&version).map_err(UpdateExecutorError::Configuration)?;
    Ok(version)
}

fn error_exit_code(error: &UpdateExecutorError) -> Option<i32> {
    match error {
        UpdateExecutorError::Failed { code, .. } => *code,
        _ => None,
    }
}

async fn run_logged_command(
    paths: &NexusPaths,
    phase: &'static str,
    release_id: &str,
    program: &Path,
    args: &[String],
    working_dir: Option<&Path>,
    command_timeout: Duration,
    #[cfg(test)] command_gate: &Arc<Mutex<Option<UpdateCommandGate>>>,
) -> Result<(), UpdateExecutorError> {
    paths
        .ensure_directories()
        .map_err(UpdateExecutorError::Configuration)?;
    let stdout_path = paths
        .logs_dir
        .join(format!("update-{release_id}-{phase}.stdout.log"));
    let stderr_path = paths
        .logs_dir
        .join(format!("update-{release_id}-{phase}.stderr.log"));
    let stdout = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(stdout_path)
        .map_err(|source| UpdateExecutorError::Spawn { phase, source })?;
    let stderr = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(stderr_path)
        .map_err(|source| UpdateExecutorError::Spawn { phase, source })?;
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .kill_on_drop(true);
    if let Some(working_dir) = working_dir {
        command.current_dir(working_dir);
    }
    let mut child = command
        .spawn()
        .map_err(|source| UpdateExecutorError::Spawn { phase, source })?;
    #[cfg(test)]
    if let Some(gate) = command_gate.lock().await.take() {
        let _ = gate.reached.send(());
        let _ = gate.release.await;
    }
    let wait = timeout(command_timeout, child.wait()).await;
    match wait {
        Ok(Ok(status)) if status.success() => Ok(()),
        Ok(Ok(status)) => Err(UpdateExecutorError::Failed {
            phase,
            code: status.code(),
        }),
        Ok(Err(source)) => Err(UpdateExecutorError::Process { phase, source }),
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            Err(UpdateExecutorError::TimedOut {
                phase,
                timeout: command_timeout,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{resolve_release_id, resolve_release_version, UpdateExecutor, UpdateExecutorError};
    use nexus_core::{ConfigStore, NexusConfigFile, NexusPaths, ReleaseStore, UpdateSpec};
    use nexus_protocol::UpdateState;
    use std::{fs, path::PathBuf, time::Duration};
    use tokio::{sync::oneshot, time::timeout};

    #[test]
    fn derived_release_identifiers_are_safe() {
        let spec = UpdateSpec {
            source: "https://example.invalid/repo".to_owned(),
            ref_name: "feature/test".to_owned(),
            git_program: PathBuf::from("git"),
            build_program: None,
            build_args: Vec::new(),
            verify_program: None,
            verify_args: Vec::new(),
            timeout_secs: Some(1),
        };
        let id = resolve_release_id(None, &spec).expect("id derives");
        assert!(id.starts_with("harness-feature-test-"));
        let version = resolve_release_version(None, &spec).expect("version derives");
        assert_eq!(version, "feature/test");
    }

    #[tokio::test]
    async fn update_config_gate_excludes_install() {
        let root = std::env::temp_dir().join(format!(
            "nexus-update-gate-{}-{}",
            std::process::id(),
            nexus_core::unix_time_seconds()
        ));
        let paths = NexusPaths::from_root(root.clone());
        let executor = UpdateExecutor::new(paths.clone(), ReleaseStore::new(paths));
        let config_guard = executor
            .try_acquire_gate()
            .expect("config transaction acquires update gate");
        assert!(matches!(
            executor.install(None, None).await,
            Err(UpdateExecutorError::AlreadyRunning)
        ));
        drop(config_guard);
        assert!(!matches!(
            executor.install(None, None).await,
            Err(UpdateExecutorError::AlreadyRunning)
        ));
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn cancelled_install_keeps_owner_gate_until_child_is_reaped_and_terminal() {
        let root = std::env::temp_dir().join(format!(
            "nexus-update-cancel-{}-{}",
            std::process::id(),
            nexus_core::unix_time_seconds()
        ));
        let paths = NexusPaths::from_root(root.clone());
        ConfigStore::new(paths.clone())
            .write(&NexusConfigFile {
                harness: None,
                update: Some(UpdateSpec {
                    source: "https://example.invalid/repo".to_owned(),
                    ref_name: "main".to_owned(),
                    git_program: if cfg!(windows) {
                        PathBuf::from("cmd.exe")
                    } else {
                        PathBuf::from("/bin/false")
                    },
                    build_program: None,
                    build_args: Vec::new(),
                    verify_program: None,
                    verify_args: Vec::new(),
                    timeout_secs: Some(5),
                }),
            })
            .expect("update config writes");
        let executor = UpdateExecutor::new(paths.clone(), ReleaseStore::new(paths));
        let (started, started_rx) = oneshot::channel();
        let (release, release_rx) = oneshot::channel();
        executor.observe_next_command(started, release_rx).await;

        let owner = executor.clone();
        let request = tokio::spawn(async move {
            owner
                .install(Some("cancelled-update".to_owned()), Some("test".to_owned()))
                .await
        });
        timeout(Duration::from_secs(3), started_rx)
            .await
            .expect("update command starts before deadline")
            .expect("update command start signal arrives");
        request.abort();
        assert!(request.await.expect_err("request aborts").is_cancelled());
        assert!(matches!(
            executor
                .install(Some("second-update".to_owned()), Some("test".to_owned()))
                .await,
            Err(UpdateExecutorError::AlreadyRunning)
        ));

        release.send(()).expect("detached update owner remains");
        let terminal = timeout(Duration::from_secs(3), async {
            loop {
                let state = executor.status().expect("update state loads");
                if state.state != UpdateState::Running {
                    break state;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("detached owner reaps child and publishes terminal state");
        assert_eq!(terminal.state, UpdateState::Failed);
        let _ = fs::remove_dir_all(root);
    }
}
