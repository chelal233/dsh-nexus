//! External update executor for immutable Harness release slots.

use std::{fmt, fs, io, path::Path, process::Stdio, sync::Arc, time::Duration};

use nexus_core::{
    load_update_spec, unix_time_nanos_for_update, unix_time_seconds, validate_release_id,
    validate_release_version, validate_update_ref, validate_update_source, ConfigStore, NexusPaths,
    ReleaseCatalog, ReleaseStore, UpdateSpec, UpdateStateStore,
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
    #[cfg(test)]
    switch_promotion_gate: Arc<Mutex<Option<UpdateCommandGate>>>,
    #[cfg(test)]
    switch_promotion_failure: Arc<std::sync::atomic::AtomicBool>,
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
            #[cfg(test)]
            switch_promotion_gate: Arc::new(Mutex::new(None)),
            #[cfg(test)]
            switch_promotion_failure: Arc::new(std::sync::atomic::AtomicBool::new(false)),
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
    pub(crate) async fn observe_next_command(
        &self,
        reached: tokio::sync::oneshot::Sender<()>,
        release: tokio::sync::oneshot::Receiver<()>,
    ) {
        *self.command_gate.lock().await = Some(UpdateCommandGate { reached, release });
    }

    #[cfg(test)]
    pub(crate) async fn observe_next_switch_promotion(
        &self,
        reached: tokio::sync::oneshot::Sender<()>,
        release: tokio::sync::oneshot::Receiver<()>,
    ) {
        *self.switch_promotion_gate.lock().await = Some(UpdateCommandGate { reached, release });
    }

    #[cfg(test)]
    fn fail_next_switch_promotion(&self) {
        self.switch_promotion_failure
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// One-click tag switch. The tag becomes the update ref (persisted while
    /// holding the executor gate), then either promotes an already-installed
    /// slot with that version (fast path) or installs it and promotes the new
    /// slot. The caller must ensure Harness is quiescent; promotion here does
    /// not re-check supervisor state.
    pub(crate) async fn switch_tag_owned(
        &self,
        tag: String,
        _guard: &OwnedMutexGuard<()>,
    ) -> Result<UpdateResponse, UpdateExecutorError> {
        validate_update_ref(&tag).map_err(UpdateExecutorError::Configuration)?;
        let config_store = ConfigStore::new(self.paths.clone());
        config_store
            .transaction(|document| {
                let spec = document.update.as_mut().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, "update is not configured")
                })?;
                if spec.ref_name != tag {
                    spec.ref_name = tag.clone();
                }
                Ok(())
            })
            .map_err(|error| {
                if error.kind() == io::ErrorKind::NotFound {
                    UpdateExecutorError::NotConfigured
                } else {
                    UpdateExecutorError::Configuration(error)
                }
            })?;
        if let Some(manifest) = self.latest_slot_for_tag(&tag)? {
            let started_at = unix_time_seconds();
            let catalog = match self.promote_for_switch(&manifest.id).await {
                Ok(catalog) => catalog,
                Err(error) => {
                    return Err(self.record_switch_failure(
                        Some(manifest.id),
                        Some(started_at),
                        error,
                        _guard,
                    ));
                }
            };
            let finished = UpdateRuntimeInfo {
                state: UpdateState::Succeeded,
                release_id: Some(manifest.id.clone()),
                started_at_unix: Some(started_at),
                finished_at_unix: Some(unix_time_seconds()),
                exit_code: Some(0),
                error: None,
            };
            self.state
                .write(&finished)
                .map_err(UpdateExecutorError::Persistence)?;
            return Ok(UpdateResponse::new(
                finished,
                catalog.find(&manifest.id).cloned(),
            ));
        }
        self.state
            .recover_unattached()
            .map_err(UpdateExecutorError::Persistence)?;
        let spec = load_update_spec(&self.paths)
            .map_err(UpdateExecutorError::Configuration)?
            .ok_or(UpdateExecutorError::NotConfigured)?;
        let release_id = resolve_release_id(None, &spec)?;
        let version = resolve_release_version(None, &spec)?;
        let started_at = unix_time_seconds();
        let running = UpdateRuntimeInfo::running(release_id.clone(), started_at);
        self.state
            .write(&running)
            .map_err(UpdateExecutorError::Persistence)?;
        let candidate = self.paths.downloads_dir.join(format!(
            ".update-{release_id}-{}",
            unix_time_nanos_for_update()
        ));
        if let Err(error) = self
            .install_inner(&spec, &candidate, &release_id, &version)
            .await
        {
            let _ = fs::remove_dir_all(&candidate);
            return Err(self.record_switch_failure(
                Some(release_id),
                Some(started_at),
                error,
                _guard,
            ));
        }
        let manifest = match self.latest_slot_for_tag(&tag) {
            Ok(Some(manifest)) => manifest,
            Ok(None) => {
                let error = UpdateExecutorError::Persistence(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("installed release for tag {tag} was not found in the catalog"),
                ));
                return Err(self.record_switch_failure(
                    Some(release_id),
                    Some(started_at),
                    error,
                    _guard,
                ));
            }
            Err(error) => {
                return Err(self.record_switch_failure(
                    Some(release_id),
                    Some(started_at),
                    error,
                    _guard,
                ));
            }
        };
        let catalog = match self.promote_for_switch(&manifest.id).await {
            Ok(catalog) => catalog,
            Err(error) => {
                return Err(self.record_switch_failure(
                    Some(release_id),
                    Some(started_at),
                    error,
                    _guard,
                ));
            }
        };
        let finished = UpdateRuntimeInfo {
            state: UpdateState::Succeeded,
            release_id: Some(manifest.id.clone()),
            started_at_unix: Some(started_at),
            finished_at_unix: Some(unix_time_seconds()),
            exit_code: Some(0),
            error: None,
        };
        self.state
            .write(&finished)
            .map_err(UpdateExecutorError::Persistence)?;
        Ok(UpdateResponse::new(
            finished,
            catalog.find(&manifest.id).cloned(),
        ))
    }

    async fn promote_for_switch(
        &self,
        release_id: &str,
    ) -> Result<ReleaseCatalog, UpdateExecutorError> {
        #[cfg(test)]
        if let Some(gate) = self.switch_promotion_gate.lock().await.take() {
            let _ = gate.reached.send(());
            let _ = gate.release.await;
        }
        #[cfg(test)]
        if self
            .switch_promotion_failure
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(UpdateExecutorError::Persistence(io::Error::other(
                "injected switch promotion failure",
            )));
        }
        self.releases
            .promote(release_id)
            .map_err(UpdateExecutorError::Persistence)
    }

    pub(crate) fn record_switch_failure(
        &self,
        release_id: Option<String>,
        started_at: Option<u64>,
        error: UpdateExecutorError,
        _guard: &OwnedMutexGuard<()>,
    ) -> UpdateExecutorError {
        let failed = UpdateRuntimeInfo {
            state: UpdateState::Failed,
            release_id,
            started_at_unix: started_at,
            finished_at_unix: Some(unix_time_seconds()),
            exit_code: error_exit_code(&error),
            error: Some(error.to_string()),
        };
        match self.state.write(&failed) {
            Ok(()) => error,
            Err(persistence) => UpdateExecutorError::Persistence(io::Error::new(
                persistence.kind(),
                format!("{error}; failed to persist terminal update state: {persistence}"),
            )),
        }
    }

    fn latest_slot_for_tag(&self, tag: &str) -> Result<Option<ReleaseManifest>, UpdateExecutorError> {
        let catalog = self.releases.load().map_err(UpdateExecutorError::Persistence)?;
        Ok(catalog
            .releases
            .iter()
            .filter(|item| item.version == tag)
            .max_by_key(|item| item.installed_at_unix)
            .cloned())
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

/// Parse `git ls-remote --tags` output into bare tag names. Peeled
/// `^{}` duplicates are dropped and the order is reversed so the
/// newest tag renders first in UI lists.
pub fn parse_ls_remote_tags(stdout: &str) -> Vec<String> {
    let mut tags: Vec<String> = Vec::new();
    for line in stdout.lines() {
        let Some(target) = line.split_whitespace().nth(1) else {
            continue;
        };
        let Some(tag) = target.strip_prefix("refs/tags/") else {
            continue;
        };
        if tag.ends_with("^{}") || tags.iter().any(|existing| existing == tag) {
            continue;
        }
        if validate_update_ref(tag).is_err() {
            continue;
        }
        tags.push(tag.to_owned());
    }
    tags.reverse();
    tags
}

/// Enumerate upstream tags with one bounded `git ls-remote --tags` call.
/// The update source itself is validated before the process is spawned.
pub async fn list_remote_tags(
    source: &str,
    git_program: &Path,
    command_timeout: Duration,
) -> Result<Vec<String>, UpdateExecutorError> {
    const PHASE: &str = "tags";
    validate_update_source(source).map_err(|source| UpdateExecutorError::Configuration(source))?;
    let mut command = Command::new(git_program);
    command
        .args(["ls-remote", "--tags", source])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let child = command
        .spawn()
        .map_err(|source| UpdateExecutorError::Spawn { phase: PHASE, source })?;
    // Dropping the timed-out future drops the child; kill_on_drop then
    // terminates the process, so no explicit kill is needed here.
    let wait = timeout(command_timeout, child.wait_with_output()).await;
    let output = match wait {
        Ok(Ok(output)) if output.status.success() => output,
        Ok(Ok(output)) => {
            return Err(UpdateExecutorError::Failed {
                phase: PHASE,
                code: output.status.code(),
            });
        }
        Ok(Err(source)) => {
            return Err(UpdateExecutorError::Process {
                phase: PHASE,
                source,
            });
        }
        Err(_) => {
            return Err(UpdateExecutorError::TimedOut {
                phase: PHASE,
                timeout: command_timeout,
            });
        }
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(parse_ls_remote_tags(&stdout))
}

#[cfg(test)]
mod tests {
    use super::{parse_ls_remote_tags, resolve_release_id, resolve_release_version, UpdateExecutor, UpdateExecutorError};
    #[test]
    fn parse_ls_remote_tags_dedupes_and_reverses() {
        let stdout = "abc	refs/tags/v0.9.0
def	refs/tags/v0.9.0^{}
123	refs/tags/v1.0.0-rc.1
456	refs/heads/main
";
        assert_eq!(
            parse_ls_remote_tags(stdout),
            vec!["v1.0.0-rc.1".to_owned(), "v0.9.0".to_owned()]
        );
        assert!(parse_ls_remote_tags("").is_empty());
    }

    use nexus_core::{ConfigStore, NexusConfigFile, NexusPaths, ReleaseStore, UpdateSpec};
    use nexus_protocol::UpdateState;
    use std::{fs, path::PathBuf, time::Duration};
    use tokio::{sync::oneshot, time::timeout};

    fn fake_git_program(root: &std::path::Path) -> PathBuf {
        #[cfg(windows)]
        {
            let program = root.join("fake-git.cmd");
            fs::write(&program, "@echo off\r\nmkdir \"%~8\"\r\nexit /b 0\r\n")
                .expect("fake git command writes");
            program
        }
        #[cfg(not(windows))]
        {
            use std::os::unix::fs::PermissionsExt;

            let program = root.join("fake-git.sh");
            fs::write(&program, "#!/bin/sh\nmkdir -p \"$8\"\n").expect("fake git command writes");
            let mut permissions = fs::metadata(&program)
                .expect("fake git metadata reads")
                .permissions();
            permissions.set_mode(0o700);
            fs::set_permissions(&program, permissions).expect("fake git becomes executable");
            program
        }
    }

    fn write_test_update(paths: &NexusPaths, git_program: PathBuf) {
        ConfigStore::new(paths.clone())
            .write(&NexusConfigFile {
                harness: None,
                update: Some(UpdateSpec {
                    source: "https://example.invalid/repo".to_owned(),
                    ref_name: "main".to_owned(),
                    git_program,
                    build_program: None,
                    build_args: Vec::new(),
                    verify_program: None,
                    verify_args: Vec::new(),
                    timeout_secs: Some(5),
                }),
                releases: None,
                runtime: None,
            })
            .expect("update config writes");
    }

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
            
                releases: None,
                runtime: None,
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

    #[tokio::test]
    async fn cold_switch_promotes_before_success_and_reports_promotion_failure() {
        let root = std::env::temp_dir().join(format!(
            "nexus-switch-promote-{}-{}",
            std::process::id(),
            nexus_core::unix_time_nanos_for_update()
        ));
        let paths = NexusPaths::from_root(root.clone());
        paths.ensure_directories().expect("test directories create");
        write_test_update(&paths, fake_git_program(&root));
        let releases = ReleaseStore::new(paths.clone());
        let executor = UpdateExecutor::new(paths.clone(), releases.clone());

        let installed = executor
            .install(
                Some("install-only".to_owned()),
                Some("v-install".to_owned()),
            )
            .await
            .expect("ordinary install succeeds");
        assert_eq!(installed.update.state, UpdateState::Succeeded);
        assert!(
            releases
                .load()
                .expect("install-only catalog loads")
                .current_release
                .is_none(),
            "ordinary install must remain install-only"
        );

        let (promotion_reached, promotion_reached_rx) = oneshot::channel();
        let (promotion_release, promotion_release_rx) = oneshot::channel();
        executor
            .observe_next_switch_promotion(promotion_reached, promotion_release_rx)
            .await;
        executor.fail_next_switch_promotion();
        let owner = executor.clone();
        let switch = tokio::spawn(async move {
            let guard = owner.try_acquire_gate()?;
            owner.switch_tag_owned("v-switch".to_owned(), &guard).await
        });
        timeout(Duration::from_secs(3), promotion_reached_rx)
            .await
            .expect("cold switch reaches promotion before deadline")
            .expect("promotion signal arrives");
        assert_eq!(
            executor.status().expect("running switch state loads").state,
            UpdateState::Running,
            "a prepared cold release must not publish Succeeded before promotion"
        );
        assert!(matches!(
            executor.try_acquire_gate(),
            Err(UpdateExecutorError::AlreadyRunning)
        ));
        promotion_release
            .send(())
            .expect("promotion failure path releases");
        assert!(matches!(
            switch.await.expect("switch task joins"),
            Err(UpdateExecutorError::Persistence(_))
        ));
        let terminal = executor.status().expect("failed switch state loads");
        assert_eq!(terminal.state, UpdateState::Failed);
        assert!(terminal
            .error
            .as_deref()
            .is_some_and(|message| message.contains("injected switch promotion failure")));
        assert!(
            releases
                .load()
                .expect("failed-promotion catalog loads")
                .current_release
                .is_none(),
            "failed promotion must not select the prepared release"
        );
        assert!(
            executor.try_acquire_gate().is_ok(),
            "terminal failure releases gate"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn fast_switch_preserves_validation_and_configuration_errors() {
        let root = std::env::temp_dir().join(format!(
            "nexus-switch-fast-{}-{}",
            std::process::id(),
            nexus_core::unix_time_nanos_for_update()
        ));
        let paths = NexusPaths::from_root(root.clone());
        paths.ensure_directories().expect("test directories create");
        write_test_update(&paths, fake_git_program(&root));
        let releases = ReleaseStore::new(paths.clone());
        releases
            .register("fast-slot", "v-fast", None, None)
            .expect("fast slot registers");
        let executor = UpdateExecutor::new(paths.clone(), releases.clone());

        let guard = executor.try_acquire_gate().expect("switch gate acquires");
        let response = executor
            .switch_tag_owned("v-fast".to_owned(), &guard)
            .await
            .expect("fast switch succeeds");
        drop(guard);
        assert_eq!(response.update.state, UpdateState::Succeeded);
        assert_eq!(response.update.release_id.as_deref(), Some("fast-slot"));
        assert_eq!(
            releases
                .load()
                .expect("fast catalog loads")
                .current_release
                .as_deref(),
            Some("fast-slot")
        );
        assert_eq!(
            ConfigStore::new(paths.clone())
                .load()
                .expect("switched config loads")
                .update
                .expect("switched update exists")
                .ref_name,
            "v-fast"
        );

        let guard = executor
            .try_acquire_gate()
            .expect("validation gate acquires");
        assert!(matches!(
            executor
                .switch_tag_owned("bad tag".to_owned(), &guard)
                .await,
            Err(UpdateExecutorError::Configuration(_))
        ));
        drop(guard);
        ConfigStore::new(paths.clone())
            .write(&NexusConfigFile {
                harness: None,
                update: None,
                releases: None,
                runtime: None,
            })
            .expect("update config clears");
        let guard = executor
            .try_acquire_gate()
            .expect("missing-config gate acquires");
        assert!(matches!(
            executor
                .switch_tag_owned("v-missing".to_owned(), &guard)
                .await,
            Err(UpdateExecutorError::NotConfigured)
        ));
        drop(guard);
        assert!(executor.try_acquire_gate().is_ok());
        let _ = fs::remove_dir_all(root);
    }
}
