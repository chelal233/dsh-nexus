//! External update executor for immutable Harness release slots.

use std::{fmt, fs, io, path::Path, process::Stdio, sync::Arc, time::Duration};

use nexus_core::{
    load_update_spec, unix_time_nanos_for_update, unix_time_seconds, validate_release_id,
    validate_release_version, validate_update_ref, ConfigStore, NexusPaths,
    ReleaseCatalog, ReleaseStore, UpdateSpec, UpdateStateStore,
};
use nexus_protocol::{InstallOperation, ReleaseManifest, UpdateResponse, UpdateRuntimeInfo, UpdateState};
use tokio::{
    sync::{oneshot, Mutex, OwnedMutexGuard},
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
    cleanup_unconfirmed: Arc<std::sync::atomic::AtomicBool>,
    cancellation: Arc<Mutex<Option<(String, nexus_core::CancellationToken)>>>,
    operation_gate: Arc<std::sync::Mutex<()>>,
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
            cleanup_unconfirmed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            cancellation: Arc::new(Mutex::new(None)),
            operation_gate: Arc::new(std::sync::Mutex::new(())),
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

    pub(crate) fn install_operation(&self) -> io::Result<Option<InstallOperation>> {
        let path = self.paths.root.join("install-operation.json");
        let metadata = match fs::symlink_metadata(&path) {
            Ok(value) => value,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        if !metadata.is_file() || nexus_core::path_is_reparse(&metadata) || metadata.len() > 256 * 1024 {
            return Err(io::Error::other("Invalid install operation record"));
        }
        let operation: InstallOperation = nexus_core::decode_versioned_record(&fs::read(path)?)?;
        validate_release_id(&operation.operation_id)?;
        validate_release_id(&operation.release_id)?;
        let candidate = Path::new(&operation.candidate);
        if !operation.operation_id.starts_with("install-")
            || candidate.parent() != Some(self.paths.downloads_dir.as_path())
            || !candidate.file_name().and_then(|s| s.to_str()).is_some_and(|name| name.starts_with(".update-"))
            || !["installing", "succeeded", "cancelled", "failed"].contains(&operation.phase.as_str()) {
            return Err(io::Error::other("Install operation identity/path is invalid"));
        }
        if operation.job_name.as_ref().is_some_and(|name| name != &format!("Global\\NexusInstall-{}", operation.operation_id)) {
            return Err(io::Error::other("Install operation Job identity is invalid"));
        }
        Ok(Some(operation))
    }

    fn write_install_operation(&self, operation: &InstallOperation) -> io::Result<()> {
        let _guard = self.operation_gate.lock().map_err(|_| io::Error::other("Install operation lock poisoned"))?;
        let mut operation = operation.clone();
        if self.install_operation()?.is_some_and(|current| current.operation_id == operation.operation_id && current.cancel_requested) {
            operation.cancel_requested = true;
        }
        nexus_core::write_versioned_record(&self.paths.root, &self.paths.root.join("install-operation.json"), &operation)
    }

    fn finish_install_cleanup(&self, operation: &mut InstallOperation) -> io::Result<()> {
        if !operation.owner_quiescent {
            operation.cleanup_error = Some("Process-tree shutdown is unconfirmed; candidate preserved".into());
        } else {
            match crate::cold::remove_owned_directory(&self.paths.downloads_dir, Path::new(&operation.candidate)) {
                Ok(()) => { operation.cleanup_pending = false; operation.cleanup_error = None; },
                Err(error) => { operation.cleanup_error = Some(format!("Candidate cleanup failed: {error}")); },
            }
        }
        self.write_install_operation(operation)
    }

    pub(crate) async fn cancel_install(&self, operation_id: &str) -> io::Result<InstallOperation> {
        let operation = self.install_operation()?.ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Installation not found"))?;
        if operation.operation_id != operation_id { return Err(io::Error::new(io::ErrorKind::InvalidInput, "Stale installation id")); }
        {
            let cancellation = self.cancellation.lock().await;
            if let Some((id, token)) = cancellation.as_ref() {
                if id == operation_id {
                    token.cancel();
                    let mut operation = operation.clone(); operation.cancel_requested = true;
                    self.write_install_operation(&operation)?;
                }
            }
        }
        // Keep the cancel owner alive separately from the HTTP request too.
        let _guard = self.gate.lock().await;
        let mut operation = self.install_operation()?.ok_or_else(|| io::Error::other("Installation record disappeared"))?;
        if operation.operation_id != operation_id { return Err(io::Error::other("Stale installation id")); }
        if operation.cleanup_pending {
            if !operation.owner_quiescent {
                if let Some(name) = operation.job_name.as_deref() {
                    operation.owner_quiescent = crate::dsh::named_operation_job_is_empty(name)?;
                }
            }
            self.finish_install_cleanup(&mut operation)?;
        }
        Ok(operation)
    }

    pub fn recover_unattached(&self) -> io::Result<UpdateRuntimeInfo> {
        if let Err(error) = self.recover_install_unattached() {
            // Keep the control plane and diagnostics available. The ordinary
            // write gate still reads this same record and fails closed.
            tracing::warn!(%error, "Installation recovery remains pending; record preserved and update mutations blocked");
        }
        // Preserve the pre-existing fatal policy for the shared update state.
        self.state.recover_unattached()
    }

    fn recover_install_unattached(&self) -> io::Result<()> {
        if let Some(mut operation) = self.install_operation()? {
            if !operation.owner_quiescent {
                if let Some(name) = operation.job_name.as_deref() {
                    match crate::dsh::named_operation_job_is_empty(name) {
                        Ok(true) => operation.owner_quiescent = true,
                        Ok(false) => {},
                        Err(error) => operation.cleanup_error = Some(format!("Cannot verify previous operation Job: {error}")),
                    }
                }
                operation.phase = "failed".into();
                operation.cleanup_pending = true;
                operation.error.get_or_insert_with(|| "Installation interrupted before its owner confirmed shutdown".into());
                operation.cleanup_error.get_or_insert_with(|| "Previous process-tree shutdown is unconfirmed; candidate preserved. Export diagnostics before manual recovery.".into());
                self.write_install_operation(&operation)?;
            }
            if operation.cleanup_pending && operation.owner_quiescent { self.finish_install_cleanup(&mut operation)?; }
        }
        Ok(())
    }

    pub fn status(&self) -> Result<UpdateRuntimeInfo, UpdateExecutorError> {
        self.state.load().map_err(UpdateExecutorError::Persistence)
    }

    pub(crate) fn try_acquire_gate(&self) -> Result<OwnedMutexGuard<()>, UpdateExecutorError> {
        let guard = Arc::clone(&self.gate)
            .try_lock_owned()
            .map_err(|_| UpdateExecutorError::AlreadyRunning)?;
        if self.install_operation().map_err(UpdateExecutorError::Persistence)?
            .is_some_and(|operation| !operation.owner_quiescent || operation.cleanup_pending) {
            return Err(UpdateExecutorError::AlreadyRunning);
        }
        if self.cleanup_unconfirmed.load(std::sync::atomic::Ordering::Acquire) {
            return Err(UpdateExecutorError::AlreadyRunning);
        }
        Ok(guard)
    }

    fn cleanup_failed_candidate(&self, candidate: &Path, error: &UpdateExecutorError) -> Result<(), UpdateExecutorError> {
        let quiescent = match error {
            UpdateExecutorError::Configuration(source) => crate::cold::command_owner_quiescent(source), _ => true,
        };
        self.cleanup_candidate(candidate, quiescent).map_err(UpdateExecutorError::Persistence)
    }

    fn cleanup_candidate(&self, candidate: &Path, owner_quiescent: bool) -> io::Result<()> {
        let mut operation = self.install_operation()?.filter(|operation| Path::new(&operation.candidate) == candidate)
            .unwrap_or_else(|| InstallOperation {
                operation_id: format!("install-{}", nexus_core::new_instance_id()), job_name: None, release_id: "legacy-update".into(),
                candidate: candidate.to_string_lossy().into_owned(), phase: "failed".into(), cancel_requested: false,
                owner_quiescent, cleanup_pending: true, error: Some("Update candidate cleanup required".into()), cleanup_error: None,
            });
        operation.owner_quiescent = owner_quiescent;
        operation.cleanup_pending = true;
        self.write_install_operation(&operation)?;
        self.finish_install_cleanup(&mut operation)
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
    /// not re-check supervisor state. A failed attempt restores its update ref
    /// without undoing unrelated configuration edits made in the meantime.
    #[allow(dead_code)]
    pub(crate) async fn switch_tag_owned(
        &self,
        tag: String,
        _guard: &OwnedMutexGuard<()>,
    ) -> Result<UpdateResponse, UpdateExecutorError> {
        validate_update_ref(&tag).map_err(UpdateExecutorError::Configuration)?;
        let config_store = ConfigStore::new(self.paths.clone());
        let (attempted_config, (original, original_bytes, original_undo)) = config_store
            .transaction(|document| {
                let original = document.clone();
                let original_bytes = nexus_core::read_regular_file_bounded(&self.paths.config_file, 4 * 1024 * 1024)?
                    .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "configuration is missing"))?;
                let original_undo = nexus_core::read_regular_file_bounded(&self.paths.root.join("config.previous.json"), 4 * 1024 * 1024)?;
                let spec = document.update.as_mut().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, "update is not configured")
                })?;
                if spec.ref_name != tag {
                    spec.ref_name = tag.clone();
                }
                document.update_attempt_id = Some(nexus_core::agent_auth::random_hex()?);
                Ok((original, original_bytes, original_undo))
            })
            .map_err(|error| {
                if error.kind() == io::ErrorKind::NotFound {
                    UpdateExecutorError::NotConfigured
                } else {
                    UpdateExecutorError::Configuration(error)
                }
            })?;
        let mut promoted = false;
        let mut attempt_release = None;
        let attempt_started = unix_time_seconds();
        let outcome = self.switch_configured_tag_owned(tag, _guard, &mut promoted, &mut attempt_release, attempt_started).await;
        if let Err(error) = outcome {
            // Promotion has its own durable commit. A later status-file error
            // must not put the ref back while the new release remains selected.
            if promoted { return Err(error); }
            let restored = config_store.restore_failed_update_ref(&attempted_config, &original, &original_bytes, &original_undo);
            let summary = match &restored {
                Ok(()) => "Previous update ref and undo configuration restored".to_owned(),
                Err(restore) => format!("Configuration recovery did not overwrite newer or unavailable files: {restore}"),
            };
            // Publish the terminal explanation after configuration recovery, so
            // diagnostics describe the actual result, including CAS conflicts.
            let failed = UpdateRuntimeInfo {
                state: UpdateState::Failed, release_id: attempt_release,
                started_at_unix: Some(attempt_started), finished_at_unix: Some(unix_time_seconds()),
                exit_code: error_exit_code(&error), error: Some(format!("{error}; {summary}")),
            };
            if let Err(persistence) = self.state.write(&failed) {
                return Err(UpdateExecutorError::Persistence(io::Error::new(persistence.kind(),
                    format!("{error}; {summary}; failed to persist recovery result: {persistence}"))));
            }
            return match restored {
                Ok(()) => Err(error),
                Err(restore) => Err(UpdateExecutorError::Configuration(io::Error::new(restore.kind(), format!("{error}; {summary}")))),
            };
        }
        outcome
    }

    async fn switch_configured_tag_owned(&self, tag: String, _guard: &OwnedMutexGuard<()>, promoted: &mut bool, attempt_release: &mut Option<String>, started_at: u64)
        -> Result<UpdateResponse, UpdateExecutorError> {
        if let Some(manifest) = self.latest_slot_for_tag(&tag)? {
            *attempt_release = Some(manifest.id.clone());
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
            *promoted = true;
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
        *attempt_release = Some(release_id.clone());
        let version = resolve_release_version(None, &spec)?;
        let running = UpdateRuntimeInfo::running(release_id.clone(), started_at);
        self.state
            .write(&running)
            .map_err(UpdateExecutorError::Persistence)?;
        let candidate = self.paths.downloads_dir.join(format!(
            ".update-{release_id}-{}",
            unix_time_nanos_for_update()
        ));
        if let Err(error) = self
            .install_inner(&spec, &candidate, &release_id, &version, &nexus_core::CancellationToken::default())
            .await
        {
            self.cleanup_failed_candidate(&candidate, &error)?;
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
        *promoted = true;
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
        // releases.promote retargets stale harness launch paths internally.
        let external = nexus_core::ConfigStore::new(self.paths.clone()).load().map_err(UpdateExecutorError::Persistence)?.external_harness.is_some();
        let catalog = if external { self.releases.promote(release_id) } else { self.releases.promote_with_rollback(release_id) }
            .map_err(UpdateExecutorError::Persistence)?;
        Ok(catalog)
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
        let spec = load_update_spec(&self.paths).map_err(UpdateExecutorError::Configuration)?
            .ok_or(UpdateExecutorError::NotConfigured)?;
        let release_id = resolve_release_id(requested_id, &spec)?;
        let version = resolve_release_version(requested_version, &spec)?;
        let started_at = unix_time_seconds();
        let candidate = self.paths.downloads_dir.join(format!(".update-{release_id}-{}", unix_time_nanos_for_update()));
        let operation_id = format!("install-{}", nexus_core::new_instance_id());
        let job_name = format!("Global\\NexusInstall-{operation_id}");
        let token = nexus_core::CancellationToken::with_job_name(job_name.clone());
        let mut operation = InstallOperation {
            operation_id, job_name: Some(job_name), release_id: release_id.clone(),
            candidate: candidate.to_string_lossy().into_owned(), phase: "installing".into(),
            cancel_requested: false, owner_quiescent: false, cleanup_pending: false, error: None, cleanup_error: None,
        };
        let mut cancellation_owner = self.cancellation.lock().await;
        self.write_install_operation(&operation).map_err(UpdateExecutorError::Persistence)?;
        *cancellation_owner = Some((operation.operation_id.clone(), token.clone()));
        drop(cancellation_owner);
        let result = match self.state.write(&UpdateRuntimeInfo::running(release_id.clone(), started_at)) {
            Ok(()) => self.install_inner(&spec, &candidate, &release_id, &version, &token).await,
            Err(error) => Err(UpdateExecutorError::Persistence(error)),
        };
        *self.cancellation.lock().await = None;
        operation.cancel_requested = token.is_cancelled();
        operation.owner_quiescent = result.as_ref().err().map_or(true, |error| match error {
            UpdateExecutorError::Configuration(error) => crate::cold::command_owner_quiescent(error), _ => true,
        });
        operation.phase = if result.is_ok() { "succeeded" } else if token.is_cancelled() { "cancelled" } else { "failed" }.into();
        operation.error = result.as_ref().err().map(ToString::to_string);
        if result.is_err() {
            operation.cleanup_pending = true;
            self.write_install_operation(&operation).map_err(UpdateExecutorError::Persistence)?;
            self.finish_install_cleanup(&mut operation).map_err(UpdateExecutorError::Persistence)?;
        } else {
            self.write_install_operation(&operation).map_err(UpdateExecutorError::Persistence)?;
        }
        let terminal = UpdateRuntimeInfo {
            state: if result.is_ok() { UpdateState::Succeeded } else { UpdateState::Failed },
            release_id: Some(release_id), started_at_unix: Some(started_at), finished_at_unix: Some(unix_time_seconds()),
            exit_code: result.as_ref().map_or_else(|error| error_exit_code(error), |_| Some(0)), error: operation.error.clone(),
        };
        self.state.write(&terminal).map_err(UpdateExecutorError::Persistence)?;
        result.map(|release| {
            let mut response = UpdateResponse::new(terminal, Some(release));
            response.install_operation = Some(operation); response
        })
    }

    async fn install_inner(
        &self,
        spec: &UpdateSpec,
        candidate: &Path,
        release_id: &str,
        version: &str,
        cancellation: &nexus_core::CancellationToken,
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
        // Clone/build size is not knowable before execution; this is a floor,
        // on the candidate's actual volume. Publication renames this tree.
        nexus_core::disk::ensure_free_space(candidate, nexus_core::disk::MIN_INSTALL_FREE_BYTES)
            .map_err(UpdateExecutorError::Configuration)?;
        let clone_spec = spec.clone();
        let clone_candidate = candidate.to_owned();
        let clone_directory = self.paths.run_dir.clone();
        let clone_cancellation = cancellation.clone();
        let clone = tokio::spawn(async move {
            crate::git_worker::clone_candidate(&clone_spec.source, &clone_spec.ref_name, &clone_candidate,
                &clone_directory, clone_spec.timeout(), &clone_cancellation,
                Some(crate::git_worker::ExternalGit { program: clone_spec.git_program.clone(), prefix: Vec::new() })).await
        });
        #[cfg(test)]
        if let Some(gate) = self.command_gate.lock().await.take() {
            let _ = gate.reached.send(());
            let _ = gate.release.await;
        }
        clone.await.map_err(|error| UpdateExecutorError::Configuration(io::Error::other(error)))?
            .map_err(UpdateExecutorError::Configuration)?;
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
                cancellation,
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
                cancellation,
                #[cfg(test)]
                &self.command_gate,
            )
            .await?;
        }

        if cancellation.is_cancelled() {
            return Err(UpdateExecutorError::Configuration(io::Error::new(io::ErrorKind::Interrupted, "Installation cancelled before publication")));
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
    cancellation: &nexus_core::CancellationToken,
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
    let mut command = std::process::Command::new(program);
    command.args(args).stdin(Stdio::null()).stdout(Stdio::from(stdout)).stderr(Stdio::from(stderr));
    if let Some(working_dir) = working_dir { command.current_dir(working_dir); }
    #[cfg(test)]
    if let Some(gate) = command_gate.lock().await.take() {
        let _ = gate.reached.send(()); let _ = gate.release.await;
    }
    crate::cold::run_owned_command(command, phase, command_timeout, &paths.run_dir, cancellation)
        .await.map_err(UpdateExecutorError::Configuration)
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

#[cfg(test)]
mod tests {
    use super::{run_logged_command, InstallOperation};
    use std::{path::Path, sync::Arc};
    use tokio::sync::Mutex;
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
            fs::write(&program, "@echo off\r\nshift\r\nshift\r\nmkdir \"%~8\"\r\nexit /b 0\r\n")
                .expect("fake git command writes");
            program
        }
        #[cfg(not(windows))]
        {
            use std::os::unix::fs::PermissionsExt;

            let program = root.join("fake-git.sh");
            fs::write(&program, "#!/bin/sh\nshift 2\nmkdir -p \"$8\"\n").expect("fake git command writes");
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
            .write(&NexusConfigFile { update_attempt_id: None, external_harness: None,
                schema_version: 1,
                harness_preferences: None,
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
                snapshots: None,
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

    #[test]
    fn install_recovery_preserves_specific_cleanup_error_and_shared_state_errors() {
        let root = std::env::temp_dir().join(format!("nexus-install-recovery-errors-{}", nexus_core::new_instance_id()));
        let paths = NexusPaths::from_root(root.clone()); paths.ensure_directories().unwrap();
        let executor = UpdateExecutor::new(paths.clone(), ReleaseStore::new(paths.clone()));
        executor.write_install_operation(&InstallOperation {
            operation_id: format!("install-{}", nexus_core::new_instance_id()), job_name: None,
            release_id: "interrupted".into(), candidate: paths.downloads_dir.join(".update-interrupted").to_string_lossy().into_owned(),
            phase: "failed".into(), cancel_requested: false, owner_quiescent: false, cleanup_pending: true,
            error: Some("Primary failure".into()), cleanup_error: Some("Cannot verify previous operation Job: access denied".into()),
        }).unwrap();
        executor.recover_unattached().unwrap();
        let record = executor.install_operation().unwrap().unwrap();
        assert_eq!(record.cleanup_error.as_deref(), Some("Cannot verify previous operation Job: access denied"));
        assert!(executor.try_acquire_gate().is_err());
        fs::write(&paths.update_state_file, "invalid shared state").unwrap();
        assert!(executor.recover_unattached().is_err(), "Other existing startup errors remain fatal");
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn explicit_install_cancel_is_id_bound_and_preserves_committed_release() {
        let root = std::env::temp_dir().join(format!("nexus-explicit-cancel-{}", nexus_core::new_instance_id()));
        fs::create_dir_all(&root).unwrap();
        let paths = NexusPaths::from_root(root.clone());
        write_test_update(&paths, fake_git_program(&root));
        let executor = UpdateExecutor::new(paths.clone(), ReleaseStore::new(paths));
        let (started, reached) = oneshot::channel();
        let (release, wait) = oneshot::channel();
        executor.observe_next_command(started, wait).await;
        let owner = executor.clone();
        let install = tokio::spawn(async move { owner.install(Some("cancel-me".into()), Some("test".into())).await });
        timeout(Duration::from_secs(5), reached).await.unwrap().unwrap();
        let operation = executor.install_operation().unwrap().unwrap();
        assert!(executor.cancel_install("install-stale").await.is_err());
        let cancel_owner = executor.clone();
        let id = operation.operation_id;
        let cancel = tokio::spawn(async move { cancel_owner.cancel_install(&id).await });
        timeout(Duration::from_secs(3), async {
            while !executor.install_operation().unwrap().unwrap().cancel_requested { tokio::task::yield_now().await; }
        }).await.unwrap();
        release.send(()).unwrap();
        assert!(install.await.unwrap().is_err());
        let cancelled = cancel.await.unwrap().unwrap();
        assert_eq!(cancelled.phase, "cancelled");
        assert!(cancelled.owner_quiescent && !cancelled.cleanup_pending);
        assert!(!Path::new(&cancelled.candidate).exists());
        let result = executor.install(Some("committed".into()), Some("test".into())).await.unwrap();
        let id = result.install_operation.unwrap().operation_id;
        assert_eq!(executor.cancel_install(&id).await.unwrap().phase, "succeeded");
        assert!(executor.releases.get("committed").is_ok());
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn install_named_job_blocks_recovery_until_cancel_reaps_descendants() {
        let root = std::env::temp_dir().join(format!("nexus-install-tree-{}", nexus_core::new_instance_id()));
        let paths = NexusPaths::from_root(root.clone()); paths.ensure_directories().unwrap();
        let executor = UpdateExecutor::new(paths.clone(), ReleaseStore::new(paths.clone()));
        let id = format!("install-{}", nexus_core::new_instance_id());
        let name = format!("Global\\NexusInstall-{id}");
        let token = nexus_core::CancellationToken::with_job_name(name.clone());
        let candidate = paths.downloads_dir.join(".update-tree"); fs::create_dir(&candidate).unwrap();
        fs::write(candidate.join("keep"), "data").unwrap();
        executor.write_install_operation(&InstallOperation { operation_id: id.clone(), job_name: Some(name.clone()),
            release_id: "tree".into(), candidate: candidate.to_string_lossy().into_owned(), phase: "installing".into(),
            cancel_requested: false, owner_quiescent: false, cleanup_pending: false, error: None, cleanup_error: None }).unwrap();
        let marker = root.join("descendant.txt");
        let child_script = root.join("child.ps1");
        fs::write(&child_script, format!("while ($true) {{ Add-Content -LiteralPath '{}' -Value 'owned'; Start-Sleep -Milliseconds 20 }}", marker.display().to_string().replace('\'', "''"))).unwrap();
        let parent_script = root.join("parent.ps1");
        fs::write(&parent_script, format!("Start-Process -WindowStyle Hidden -FilePath \"$PSHOME\\powershell.exe\" -ArgumentList @('-NoProfile','-File','{}'); while ($true) {{ Start-Sleep -Seconds 1 }}", child_script.display().to_string().replace('\'', "''"))).unwrap();
        let powershell = std::path::PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join("System32/WindowsPowerShell/v1.0/powershell.exe");
        let args = vec!["-NoProfile".into(), "-File".into(), parent_script.to_string_lossy().into_owned()];
        let owned_paths = paths.clone(); let owned_token = token.clone();
        let command = tokio::spawn(async move { run_logged_command(&owned_paths, "build", "tree", &powershell, &args, None,
            Duration::from_secs(15), &owned_token, &Arc::new(Mutex::new(None))).await });
        timeout(Duration::from_secs(8), async { while !marker.is_file() { tokio::time::sleep(Duration::from_millis(20)).await; } }).await.unwrap();
        assert!(!crate::dsh::named_operation_job_is_empty(&name).unwrap());
        let fresh = UpdateExecutor::new(paths.clone(), ReleaseStore::new(paths));
        fresh.recover_unattached().unwrap();
        assert!(candidate.join("keep").exists());
        assert!(fresh.try_acquire_gate().is_err());
        token.cancel(); assert!(command.await.unwrap().is_err());
        let length = fs::metadata(&marker).unwrap().len();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(fs::metadata(&marker).unwrap().len(), length);
        assert!(crate::dsh::named_operation_job_is_empty(&name).unwrap());
        let settled = fresh.cancel_install(&id).await.unwrap();
        assert!(settled.owner_quiescent && !settled.cleanup_pending);
        assert!(!candidate.exists()); assert!(fresh.try_acquire_gate().is_ok());
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn install_cleanup_failure_is_durable_and_retryable_after_restart() {
        use std::os::windows::fs::OpenOptionsExt;
        let root = std::env::temp_dir().join(format!("nexus-install-cleanup-{}", nexus_core::new_instance_id()));
        let paths = NexusPaths::from_root(root.clone()); paths.ensure_directories().unwrap();
        let candidate = paths.downloads_dir.join(".update-cleanup"); fs::create_dir(&candidate).unwrap();
        let locked = fs::OpenOptions::new().write(true).create(true).share_mode(0).open(candidate.join("locked")).unwrap();
        let executor = UpdateExecutor::new(paths.clone(), ReleaseStore::new(paths.clone()));
        executor.cleanup_candidate(&candidate, true).unwrap();
        let operation = executor.install_operation().unwrap().unwrap();
        assert!(operation.cleanup_pending && operation.cleanup_error.is_some());
        let fresh = UpdateExecutor::new(paths.clone(), ReleaseStore::new(paths));
        fresh.recover_unattached().unwrap(); assert!(fresh.try_acquire_gate().is_err());
        drop(locked);
        let settled = fresh.cancel_install(&operation.operation_id).await.unwrap();
        assert!(!settled.cleanup_pending && settled.cleanup_error.is_none());
        assert!(!candidate.exists()); assert!(fresh.try_acquire_gate().is_ok());
        fs::remove_dir_all(root).unwrap();
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

    #[test]
    fn unconfirmed_process_cleanup_preserves_checkout_and_blocks_updates() {
        let root = std::env::temp_dir().join(format!("nexus-update-cleanup-{}", nexus_core::unix_time_nanos_for_update()));
        let paths = NexusPaths::from_root(root.clone());
        paths.ensure_directories().unwrap();
        let candidate = paths.downloads_dir.join(".update-owned-candidate");
        fs::create_dir(&candidate).unwrap();
        fs::write(candidate.join("keep.txt"), "active").unwrap();
        let executor = UpdateExecutor::new(paths.clone(), ReleaseStore::new(paths.clone()));
        executor.cleanup_candidate(&candidate, false).unwrap();
        assert_eq!(fs::read_to_string(candidate.join("keep.txt")).unwrap(), "active");
        assert!(matches!(executor.try_acquire_gate(), Err(UpdateExecutorError::AlreadyRunning)));
        assert!(matches!(executor.clone().try_acquire_gate(), Err(UpdateExecutorError::AlreadyRunning)));
        let fresh = UpdateExecutor::new(paths.clone(), ReleaseStore::new(paths));
        fresh.cleanup_candidate(&candidate, true).unwrap();
        assert!(!candidate.exists());
        fs::remove_dir_all(root).unwrap();
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
            .write(&NexusConfigFile { update_attempt_id: None, external_harness: None,
                schema_version: 1,
                harness_preferences: None,
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
                snapshots: None,
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
    async fn rejected_switch_restores_its_ref_without_overwriting_concurrent_edits() {
        for edit in ["none", "preferences", "preferences-twice", "update", "same-update", "aba-update", "full-save"] {
            let root = std::env::temp_dir().join(format!("nexus-switch-ref-{edit}-{}", nexus_core::unix_time_nanos_for_update()));
            let paths = NexusPaths::from_root(root.clone()); paths.ensure_directories().unwrap();
            write_test_update(&paths, fake_git_program(&root));
            let store = ConfigStore::new(paths.clone());
            let original = store.load().unwrap();
            let original_bytes=fs::read(&paths.config_file).unwrap();
            let undo_path=paths.root.join("config.previous.json");
            let original_undo=nexus_core::read_regular_file_bounded(&undo_path,4*1024*1024).unwrap();
            let releases = ReleaseStore::new(paths.clone());
            releases.register("old", "v-old", None, None).unwrap();
            releases.register("new", "v-new", None, None).unwrap();
            releases.promote("old").unwrap(); // Upgraded installation without healthy evidence.
            let executor = UpdateExecutor::new(paths.clone(), releases.clone());
            let (reached, wait) = oneshot::channel(); let (resume, pause) = oneshot::channel();
            executor.observe_next_switch_promotion(reached, pause).await;
            let owner = executor.clone();
            let attempt = tokio::spawn(async move {
                let guard = owner.try_acquire_gate()?;
                owner.switch_tag_owned("v-new".into(), &guard).await
            });
            timeout(Duration::from_secs(3), wait).await.unwrap().unwrap();
            if edit == "full-save" {
                store.write(&store.load().unwrap()).unwrap();
            } else if edit != "none" {
                store.transaction(|config| {
                    if edit.starts_with("preferences") { config.harness_preferences.get_or_insert_with(Default::default).telemetry_disabled = Some(true); }
                    else if edit=="same-update" { config.set_update(config.update.clone()); }
                    else { config.update.as_mut().unwrap().ref_name = "v-external".into(); }
                    Ok(())
                }).unwrap();
                if edit=="preferences-twice" {
                    store.transaction(|config| {config.harness_preferences.as_mut().unwrap().open_browser=Some(false);Ok(())}).unwrap();
                }
                if edit=="aba-update" { store.transaction(|config| {config.update.as_mut().unwrap().ref_name="v-new".into();Ok(())}).unwrap(); }
            }
            resume.send(()).unwrap();
            let error = attempt.await.unwrap().unwrap_err().to_string();
            assert!(error.contains("rollback_health_required"), "{error}");
            let mut expected = original;
            if edit.starts_with("preferences") { expected.harness_preferences.get_or_insert_with(Default::default).telemetry_disabled = Some(true); }
            if edit=="preferences-twice" { expected.harness_preferences.as_mut().unwrap().open_browser=Some(false); }
            let conflict=matches!(edit,"update"|"same-update"|"aba-update"|"full-save");
            if conflict {
                expected.update.as_mut().unwrap().ref_name = if edit=="update" {"v-external"} else {"v-new"}.into();
                assert!(error.contains("changed concurrently"));
            }
            assert_eq!(store.load().unwrap(), expected, "{edit}");
            assert_eq!(releases.load().unwrap().current_release.as_deref(), Some("old"));
            assert_eq!(executor.status().unwrap().state, UpdateState::Failed);
            let explanation=executor.status().unwrap().error.unwrap();
            if conflict { assert!(explanation.contains("newer settings were retained")); }
            else {
                assert!(explanation.contains("Previous update ref and undo configuration restored"));
                if edit=="none" {
                    assert_eq!(fs::read(&paths.config_file).unwrap(),original_bytes);
                    assert_eq!(nexus_core::read_regular_file_bounded(&undo_path,4*1024*1024).unwrap(),original_undo);
                } else {
                    let undo: nexus_core::NexusConfigFile=serde_json::from_slice(&fs::read(&undo_path).unwrap()).unwrap();
                    assert_eq!(undo.update.as_ref().unwrap().ref_name,expected.update.as_ref().unwrap().ref_name);
                    if edit=="preferences-twice" {
                        assert_eq!(undo.harness_preferences.as_ref().unwrap().telemetry_disabled,Some(true));
                        assert_ne!(undo.harness_preferences.as_ref().unwrap().open_browser,Some(false));
                    }
                    store.restore_previous().unwrap();
                    assert_eq!(store.load().unwrap().update.unwrap().ref_name,expected.update.unwrap().ref_name);
                }
            }
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[tokio::test]
    async fn early_switch_failure_does_not_relabel_a_previous_success() {
        let root=std::env::temp_dir().join(format!("nexus-early-switch-{}",nexus_core::unix_time_nanos_for_update()));
        let paths=NexusPaths::from_root(root.clone());paths.ensure_directories().unwrap();write_test_update(&paths,fake_git_program(&root));
        let releases=ReleaseStore::new(paths.clone());releases.register("old","v-old",None,None).unwrap();
        let executor=UpdateExecutor::new(paths.clone(),releases);
        executor.state.write(&nexus_protocol::UpdateRuntimeInfo{state:UpdateState::Succeeded,release_id:Some("old".into()),started_at_unix:Some(1),finished_at_unix:Some(2),exit_code:Some(0),error:None}).unwrap();
        fs::write(&paths.release_pointers_file,b"{broken").unwrap();
        let guard=executor.try_acquire_gate().unwrap();
        assert!(executor.switch_tag_owned("v-new".into(),&guard).await.is_err());
        let result=executor.status().unwrap();assert_eq!(result.state,UpdateState::Failed);assert_eq!(result.release_id,None);
        assert!(result.started_at_unix.unwrap()>2);assert!(result.error.unwrap().contains("Previous update ref and undo configuration restored"));
        fs::remove_dir_all(root).unwrap();
    }
    #[tokio::test]
    #[cfg(windows)]
    async fn committed_switch_keeps_new_ref_when_final_status_file_is_locked() {
        use std::os::windows::fs::OpenOptionsExt;
        for fast in [true, false] {
            let root = std::env::temp_dir().join(format!("nexus-switch-status-{fast}-{}", nexus_core::unix_time_nanos_for_update()));
            let paths = NexusPaths::from_root(root.clone()); paths.ensure_directories().unwrap();
            write_test_update(&paths, fake_git_program(&root));
            let releases = ReleaseStore::new(paths.clone());
            if fast { releases.register("fast-post", "v-post", None, None).unwrap(); }
            let executor = UpdateExecutor::new(paths.clone(), releases.clone());
            executor.state.write(&nexus_protocol::UpdateRuntimeInfo::idle()).unwrap();
            let (reached, wait) = oneshot::channel(); let (resume, pause) = oneshot::channel();
            executor.observe_next_switch_promotion(reached, pause).await;
            let owner = executor.clone();
            let attempt = tokio::spawn(async move {
                let guard = owner.try_acquire_gate()?;
                owner.switch_tag_owned("v-post".into(), &guard).await
            });
            timeout(Duration::from_secs(5), wait).await.unwrap().unwrap();
            let lock = fs::OpenOptions::new().read(true).share_mode(3).open(&paths.update_state_file).unwrap();
            let before = fs::read(&paths.update_state_file).unwrap();
            resume.send(()).unwrap();
            let error = attempt.await.unwrap().unwrap_err();
            assert!(matches!(error, UpdateExecutorError::Persistence(_)), "{error}");
            let catalog = releases.load().unwrap();
            assert_eq!(catalog.find(catalog.current_release.as_deref().unwrap()).unwrap().version, "v-post");
            assert_eq!(ConfigStore::new(paths.clone()).load().unwrap().update.unwrap().ref_name, "v-post");
            assert_eq!(fs::read(&paths.update_state_file).unwrap(), before);
            drop(lock);
            fs::remove_dir_all(root).unwrap();
        }
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
            .write(&NexusConfigFile { update_attempt_id: None, external_harness: None,
                schema_version: 1,
                harness_preferences: None,
                harness: None,
                update: None,
                releases: None,
                runtime: None,
                snapshots: None,
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
