//! Persisted cold-install orchestration for upstream Harness tags.

#[path = "offline.rs"]
pub(crate) mod offline;

use std::{
    ffi::OsString,
    fs, io,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use nexus_core::{
    build_pnpm_args, build_runtime_child_env, redact_diagnostics_payload, resolve_runtime_command,
    unix_time_nanos_for_update, unix_time_seconds, validate_update_ref, write_json_atomic,
    CancellationToken, HarnessLaunchSpec, NexusPaths, RuntimeConfig, RuntimePin,
};
use nexus_protocol::{
    ColdOperation, ColdOperationPhase, HarnessLaunchMode, RuntimeInstallMode, RuntimeOwnership,
    RuntimePlanActionKind, RuntimePlanRequest, RuntimePlanToolState, RuntimeSource,
    UpdateRuntimeInfo, UpdateState,
};
use tokio::sync::Mutex;

use crate::AppState;

const COLD_STATE_FILE: &str = "cold-operation.json";
const PUBLICATION_FILE: &str = "cold-publication.json";
const APPROVED_UPSTREAM: &str = "https://github.com/deepseek-ai/deepseek-harness";
const COMMAND_TIMEOUT: Duration = Duration::from_secs(900);
const COMMAND_DIAGNOSTIC_BYTES: usize = 64 * 1024;

#[derive(Debug)]
struct ColdCommandFailure {
    message: String,
    owner_quiescent: bool,
}

impl std::fmt::Display for ColdCommandFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ColdCommandFailure {}

pub(crate) fn command_owner_quiescent(error: &io::Error) -> bool {
    error
        .get_ref()
        .and_then(|source| source.downcast_ref::<ColdCommandFailure>())
        .map_or(true, |failure| failure.owner_quiescent)
}

#[derive(Debug)]
struct PublicationConflict;
impl std::fmt::Display for PublicationConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Publication recovery conflicts with externally changed Nexus configuration; preserve the current settings and export diagnostics. No configuration was replaced.")
    }
}
impl std::error::Error for PublicationConflict {}
#[cfg(test)]
fn is_publication_conflict(error: &io::Error) -> bool {
    error.get_ref().is_some_and(|inner| inner.is::<PublicationConflict>())
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PublicationIntent {
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    prepare_only: bool,
    #[serde(default)]
    previous_profiles: Option<nexus_core::ProfileCatalog>,
    #[serde(default)]
    target_profiles: Option<nexus_core::ProfileCatalog>,
    committed: bool,
    #[serde(default)]
    preserve_current: bool,
    #[serde(default)]
    owned_runtime: Option<String>,
    #[serde(default)]
    owned_environment: bool,
    #[serde(default)]
    preserve_release: bool,
    operation: ColdOperation,
    previous_config: nexus_core::NexusConfigFile,
    target_config: nexus_core::NexusConfigFile,
    previous_current: Option<String>,
    previous_lkg: Option<String>,
    #[serde(default)]
    target_lkg: Option<String>,
    previous_update: UpdateRuntimeInfo,
    new_slot: bool,
}

#[derive(Clone)]
pub(crate) struct ColdCoordinator {
    paths: NexusPaths,
    cancellation: Arc<Mutex<Option<(String, CancellationToken)>>>,
    gate: Arc<Mutex<()>>,
    owner_active: Arc<AtomicBool>,
}

impl ColdCoordinator {
    pub(crate) fn new(paths: NexusPaths) -> Self {
        Self {
            paths,
            cancellation: Arc::new(Mutex::new(None)),
            gate: Arc::new(Mutex::new(())),
            owner_active: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(crate) fn approved_upstream() -> &'static str {
        APPROVED_UPSTREAM
    }

    pub(crate) fn try_acquire_maintenance(&self) -> io::Result<tokio::sync::OwnedMutexGuard<()>> {
        let guard = Arc::clone(&self.gate).try_lock_owned()
            .map_err(|_| io::Error::new(io::ErrorKind::ResourceBusy, "cold operation is busy"))?;
        if self.owner_active.load(Ordering::Acquire) || self.publication_pending()
            || self.load()?.is_some_and(|operation| !operation.phase.is_terminal()
                || operation.cleanup_pending || !operation.owner_quiescent) {
            return Err(io::Error::new(io::ErrorKind::ResourceBusy, "cold operation or recovery blocks reset"));
        }
        Ok(guard)
    }

    pub(crate) fn recover(&self) -> io::Result<()> {
        // Startup only: never dismiss a new failure while the user is reading
        // it, nor discard a transaction that still needs recovery.
        if let Err(error) = self.prune_uninstalled_history() {
            tracing::warn!(%error, "Could not clear obsolete installation history; record retained");
        }
        self.recover_publication()?;
        let Some(mut operation) = self.load()? else {
            return Ok(());
        };
        if !operation.phase.is_terminal()
            && operation.phase != ColdOperationPhase::AwaitingConfirmation
        {
            operation.phase = ColdOperationPhase::Failed;
            operation.progress_percent = 100;
            operation.updated_at_unix = Some(unix_time_seconds());
            let next = match operation.kind.as_str() {
                "offline_import" => "retry the offline import",
                "offline_export" => "retry the offline export",
                _ => "start the tag switch again",
            };
            operation.error = Some(format!("previous cold-install owner was not attached; {next}"));
            operation.owner_quiescent = true;
            operation.cleanup_pending = true;
            operation.cleanup_error = None;
            self.write(&operation)?;
        }
        if operation.cleanup_pending {
            // A previous in-memory owner cannot survive Agent restart. Retain
            // the primary terminal result and retry only the owned residue.
            operation.owner_quiescent = true;
            match offline::cleanup_candidate(&self.paths, &operation)
            {
                Ok(()) => {
                    operation.cleanup_pending = false;
                    operation.cleanup_error = None;
                }
                Err(error) => {
                    operation.cleanup_error = Some(format!("candidate cleanup failed: {error}"));
                }
            }
            self.write(&operation)?;
        }
        Ok(())
    }

    fn prune_uninstalled_history(&self) -> io::Result<bool> {
        if self.publication_pending() || self.owner_active.load(Ordering::Acquire) {
            return Ok(false);
        }
        let Some(operation) = self.load()? else { return Ok(false); };
        if operation.kind == "offline_export" { return Ok(false); }
        if !operation.phase.is_terminal() || operation.cleanup_pending || !operation.owner_quiescent {
            return Ok(false);
        }
        // Residual candidate ownership must remain visible even if an old
        // record incorrectly omitted cleanup_pending.
        match fs::symlink_metadata(&operation.candidate) {
            Ok(_) => return Ok(false),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {},
            Err(error) => return Err(error),
        }
        let catalog = nexus_core::ReleaseStore::new(self.paths.clone()).load()?;
        if catalog.find(&operation.release_id).is_some() { return Ok(false); }
        let updates = nexus_core::UpdateStateStore::new(self.paths.clone());
        let update = updates.load()?;
        if update.state == UpdateState::Running { return Ok(false); }
        if update.release_id.as_deref() == Some(&operation.release_id)
            && update.started_at_unix == Some(operation.started_at_unix) {
            updates.write(&UpdateRuntimeInfo::idle())?;
        }
        fs::remove_file(self.paths.root.join(COLD_STATE_FILE))?;
        tracing::info!("Cleared finished installation history whose release is no longer installed");
        Ok(true)
    }

    fn intent_path(&self) -> PathBuf {
        self.paths.root.join(PUBLICATION_FILE)
    }

    pub(crate) fn publication_pending(&self) -> bool {
        self.intent_path().exists()
    }

    pub(crate) fn cleanup_pending(&self) -> io::Result<bool> {
        Ok(self
            .load()?
            .is_some_and(|operation| operation.cleanup_pending || !operation.owner_quiescent
                || (!operation.phase.is_terminal() && operation.phase != ColdOperationPhase::AwaitingConfirmation)))
    }

    // Only the live owner may pass its own busy-operation guard at publication.
    // Other mutations must continue to treat this operation as busy.
    pub(crate) async fn owns_verifying_publication(&self, id: &str) -> io::Result<bool> {
        if !self.owner_active.load(Ordering::Acquire) || !self.load()?.is_some_and(|operation|
            operation.operation_id == id && operation.phase == ColdOperationPhase::Verifying
                && !operation.cleanup_pending) { return Ok(false); }
        Ok(self.cancellation.lock().await.as_ref().is_some_and(|(owner, token)| owner == id && !token.is_cancelled()))
    }

    fn read_intent(&self) -> io::Result<PublicationIntent> {
        let bytes = nexus_core::read_regular_file_bounded(&self.intent_path(), 16 * 1024 * 1024)?
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "No publication recovery is pending"))?;
        nexus_core::decode_versioned_record(&bytes)
    }

    pub(crate) fn publication_status(&self) -> io::Result<Option<serde_json::Value>> {
        // A live transaction's journal is normal progress, not interrupted recovery.
        if self.owner_active.load(Ordering::Acquire) { return Ok(None); }
        if !self.publication_pending() { return Ok(None); }
        let intent = self.read_intent()?;
        Ok(Some(serde_json::json!({"operation_id":intent.operation.operation_id,
            "pending":true,"preserve_current":intent.preserve_current,
            "reason":"Publication recovery is pending. Retry recovery, or preserve current configuration and all version/candidate files to end this operation."})))
    }

    fn finish_preserved_publication(&self, intent: &PublicationIntent) -> io::Result<ColdOperation> {
        use std::io::Write;
        nexus_core::ConfigStore::new(self.paths.clone()).preserve_current_after(|| Ok(()))?;
        let id = &intent.operation.operation_id;
        if id.is_empty() || id.len() > 160 || !id.bytes().all(|v| v.is_ascii_alphanumeric() || v == b'-') {
            return Err(io::Error::other("Invalid recovery operation identity"));
        }
        // Archive contains original credentials, never part of shared diagnostics.
        let archive = self.paths.root.join(format!("publication-preserved-{id}.json"));
        let bytes = serde_json::to_vec(intent).map_err(io::Error::other)?;
        if let Some(existing) = nexus_core::read_regular_file_bounded(&archive, 16 * 1024 * 1024)? {
            if existing != bytes { return Err(io::Error::other("Recovery archive differs; all files were preserved")); }
        } else {
            let temp = self.paths.root.join(format!(".publication-archive-{}.tmp", unix_time_nanos_for_update()));
            let result = (|| {
                let mut file = nexus_private_file::create_new_private(&temp)?;
                file.write_all(&bytes)?;
                file.sync_all()?;
                drop(file);
                // Atomic create-new name without exposing a partially written archive.
                fs::hard_link(&temp, &archive)
            })();
            let _ = fs::remove_file(&temp);
            result?;
        }
        let mut operation = intent.operation.clone();
        operation.phase = ColdOperationPhase::Cancelled;
        operation.progress_percent = 100;
        operation.owner_quiescent = true;
        operation.cleanup_pending = false;
        operation.cleanup_error = None;
        operation.error = Some("Publication recovery ended by keeping current configuration and all existing version/candidate files. Retained candidate files are not automatically cleaned.".into());
        operation.updated_at_unix = Some(unix_time_seconds());
        nexus_core::UpdateStateStore::new(self.paths.clone()).write(&UpdateRuntimeInfo {
            state: UpdateState::Failed, release_id: None, started_at_unix: Some(operation.started_at_unix),
            finished_at_unix: Some(unix_time_seconds()), exit_code: None, error: operation.error.clone(),
        })?;
        Ok(operation)
    }

    pub(crate) fn recover_explicit(&self, operation_id: &str, preserve_current: bool) -> io::Result<()> {
        if self.owner_active.load(Ordering::Acquire) { return Err(io::Error::new(io::ErrorKind::ResourceBusy, "Publication owner is still active")); }
        let mut intent = self.read_intent()?;
        if intent.operation.operation_id != operation_id {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "Stale publication operation ID"));
        }
        if preserve_current || intent.preserve_current {
            nexus_core::ConfigStore::new(self.paths.clone()).preserve_current_after(|| {
                if !intent.preserve_current { intent.preserve_current=true; self.write_intent(&intent)?; }
                Ok(())
            })?;
        } else { nexus_core::ConfigStore::new(self.paths.clone()).load()?; }
        self.recover_publication()
    }

    pub(crate) async fn acquire_recovery(&self) -> io::Result<tokio::sync::OwnedMutexGuard<()>> {
        self.gate.clone().try_lock_owned().map_err(|_| io::Error::new(io::ErrorKind::ResourceBusy, "Publication owner is busy"))
    }

    fn write_intent(&self, intent: &PublicationIntent) -> io::Result<()> {
        nexus_core::write_versioned_record(&self.paths.root, &self.intent_path(), intent)
    }

    fn prepare_publication(
        &self,
        operation: &ColdOperation,
        new_slot: bool,
        target_config: nexus_core::NexusConfigFile,
    ) -> io::Result<PublicationIntent> {
        self.prepare_publication_inner(operation, new_slot, target_config, false)
    }

    fn prepare_publication_inner(&self, operation: &ColdOperation, new_slot: bool,
        mut target_config: nexus_core::NexusConfigFile, prepare_only: bool) -> io::Result<PublicationIntent> {
        if !prepare_only { target_config.update_attempt_id = None; }
        let catalog = nexus_core::ReleaseStore::new(self.paths.clone()).load()?;
        if new_slot && self.paths.releases_dir.join(&operation.release_id).exists() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "cold target slot already exists",
            ));
        }
        let intent = PublicationIntent {
            prepare_only,
            previous_profiles: None,
            target_profiles: None,
            committed: false,
            preserve_current: false,
            owned_runtime: None,
            owned_environment: false,
            preserve_release: false,
            target_config,
            operation: operation.clone(),
            previous_config: nexus_core::ConfigStore::new(self.paths.clone()).load()?,
            previous_current: catalog.current_release,
            previous_lkg: catalog.last_known_good,
            target_lkg: nexus_core::ReleaseStore::new(self.paths.clone()).verified_fallback(Some(&operation.release_id))?,
            previous_update: nexus_core::UpdateStateStore::new(self.paths.clone()).load()?,
            new_slot,
        };
        self.write_intent(&intent)?;
        Ok(intent)
    }

    // The commit record is the only decision. Every replay step is idempotent;
    // the record remains present until the complete tuple and cleanup persist.
    fn recover_publication(&self) -> io::Result<()> {
        if let Some(operation) = self.reconcile_publication()? {
            self.finish_publication(&operation)?;
        }
        Ok(())
    }

    fn finish_publication(&self, operation: &ColdOperation) -> io::Result<()> {
        self.write(operation)?;
        fs::remove_file(self.intent_path())
    }

    fn reconcile_publication(&self) -> io::Result<Option<ColdOperation>> {
        if !self.intent_path().exists() {
            return Ok(None);
        }
        let intent = self.read_intent()?;
        if intent.preserve_current { return self.finish_preserved_publication(&intent).map(Some); }
        let current = nexus_core::ConfigStore::new(self.paths.clone()).load()?;
        if current != intent.previous_config && current != intent.target_config {
            return Err(io::Error::new(io::ErrorKind::WouldBlock, PublicationConflict));
        }
        if let (Some(previous), Some(target)) = (&intent.previous_profiles, &intent.target_profiles) {
            let store = nexus_core::ProfileStore::new(self.paths.clone());
            let current = store.load()?;
            if &current != previous && &current != target { return Err(io::Error::other("Imported profile selection conflicts with a later change")); }
            store.write(if intent.committed { target } else { previous })?;
        }
        let releases = nexus_core::ReleaseStore::new(self.paths.clone());
        let updater = nexus_core::UpdateStateStore::new(self.paths.clone());
        let mut operation = intent.operation;
        if intent.committed {
            if !intent.prepare_only {
            nexus_core::ConfigStore::new(self.paths.clone()).write_recovery_document(&intent.target_config)?;
            let lkg = intent.target_lkg.as_deref();
            releases.restore_release_pointers(if intent.preserve_release { intent.previous_current.as_deref() } else { Some(&operation.release_id) }, if intent.preserve_release { intent.previous_lkg.as_deref() } else { lkg })?;
            }
            updater.write(&UpdateRuntimeInfo {
                state: if intent.prepare_only { UpdateState::Prepared } else { UpdateState::Succeeded },
                release_id: Some(operation.release_id.clone()),
                started_at_unix: Some(operation.started_at_unix),
                finished_at_unix: Some(unix_time_seconds()),
                exit_code: Some(0),
                error: None,
            })?;
            operation.phase = if intent.prepare_only { ColdOperationPhase::Prepared } else { ColdOperationPhase::Succeeded };
            operation.error = None;
            operation.owner_quiescent = true;
            operation.cleanup_pending = false;
            operation.cleanup_error = None;
        } else {
            if !intent.prepare_only {
            nexus_core::ConfigStore::new(self.paths.clone()).write_recovery_document(&intent.previous_config)?;
            releases.restore_release_pointers(
                intent.previous_current.as_deref(),
                intent.previous_lkg.as_deref(),
            )?;
            }
            updater.write(&intent.previous_update)?;
            if intent.new_slot {
                // Includes rename-before-manifest cuts; slot id is server generated.
                nexus_core::validate_release_id(&operation.release_id)?;
                remove_owned_directory(
                    &self.paths.releases_dir,
                    &self.paths.releases_dir.join(&operation.release_id),
                )?;
            }
            if let Some(runtime) = &intent.owned_runtime {
                if runtime != &format!("offline-{}", operation.operation_id) { return Err(io::Error::other("Invalid owned offline runtime identity")); }
                remove_owned_directory(&self.paths.runtimes_dir,&self.paths.runtimes_dir.join(runtime))?;
            }
            if intent.owned_environment {
                let root = offline::environment_root(&self.paths, &operation.operation_id)?;
                remove_owned_directory(root.parent().ok_or_else(|| io::Error::other("Environment parent missing"))?, &root)?;
            }
            operation.phase = ColdOperationPhase::Failed;
            operation.error =
                Some("publication interrupted before commit; previous selection restored".into());
            operation.owner_quiescent = true;
            operation.cleanup_pending = false;
            operation.cleanup_error = None;
        }
        offline::cleanup_candidate(&self.paths, &operation)?;
        operation.progress_percent = 100;
        operation.updated_at_unix = Some(unix_time_seconds());
        Ok(Some(operation))
    }

    pub(crate) fn load(&self) -> io::Result<Option<ColdOperation>> {
        let path = self.paths.root.join(COLD_STATE_FILE);
        if !path.exists() {
            return Ok(None);
        }
        let bytes = fs::read(path)?;
        nexus_core::decode_versioned_record(&bytes).map(Some)
    }

    fn write(&self, operation: &ColdOperation) -> io::Result<()> {
        self.paths.ensure_directories()?;
        let path = self.paths.root.join(COLD_STATE_FILE);
        nexus_core::write_versioned_record(&self.paths.root, &path, operation)
    }

    fn write_failure_pending(
        &self,
        operation_id: &str,
        primary: &str,
        owner_quiescent: bool,
        cleanup_error: String,
    ) -> io::Result<()> {
        let Some(mut operation) = self
            .load()?
            .filter(|operation| operation.operation_id == operation_id)
        else {
            return Ok(());
        };
        operation.phase = if operation.phase == ColdOperationPhase::Cancelling {
            ColdOperationPhase::Cancelled
        } else {
            ColdOperationPhase::Failed
        };
        operation.progress_percent = 100;
        operation.updated_at_unix = Some(unix_time_seconds());
        operation.error = Some(primary.to_owned());
        operation.owner_quiescent = owner_quiescent;
        operation.cleanup_pending = true;
        operation.cleanup_error = Some(cleanup_error);
        self.write(&operation)
    }

    async fn update(
        &self,
        operation_id: &str,
        phase: ColdOperationPhase,
        progress: u8,
        error: Option<String>,
    ) -> io::Result<ColdOperation> {
        let _gate = self.gate.lock().await;
        let mut operation = self
            .load()?
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "cold operation not found"))?;
        if operation.operation_id != operation_id {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cold operation changed",
            ));
        }
        if (operation.phase.is_terminal() || operation.phase == ColdOperationPhase::Cancelling)
            && operation.phase != phase
            && phase != ColdOperationPhase::Cancelled
        {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "cold operation is already terminal",
            ));
        }
        operation.phase = phase;
        operation.progress_percent = progress.min(100);
        operation.updated_at_unix = Some(unix_time_seconds());
        operation.error = error;
        self.write(&operation)?;
        Ok(operation)
    }

    pub(crate) async fn clear_finished(&self, operation_id: &str) -> io::Result<()> {
        let _gate = self.gate.lock().await;
        let operation = self.load()?.ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "cold operation not found")
        })?;
        if operation.operation_id != operation_id {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "cold operation changed"));
        }
        if !operation.phase.is_terminal()
            || operation.cleanup_pending
            || self.owner_active.load(Ordering::Acquire)
            || self.intent_path().exists()
        {
            return Err(io::Error::new(io::ErrorKind::ResourceBusy,
                "cold operation or cleanup is still active"));
        }
        // Forget only this finished attempt. Slots and runtime state belong to
        // separate stores and must survive dismissal of a diagnostic record.
        fs::remove_file(self.paths.root.join(COLD_STATE_FILE))
    }

    pub(crate) async fn begin(
        &self,
        tag: String,
        source: RuntimeSource,
        mode: RuntimeInstallMode,
    ) -> io::Result<ColdOperation> {
        self.begin_with_details(tag, source, mode, "cold_switch", None, None).await
    }

    async fn begin_with_details(&self, tag: String, source: RuntimeSource, mode: RuntimeInstallMode,
        kind: &str, archive_path: Option<String>, selected_release: Option<String>) -> io::Result<ColdOperation> {
        validate_update_ref(&tag)?;
        if self.intent_path().exists() {
            return Err(io::Error::new(
                io::ErrorKind::ResourceBusy,
                "publication recovery is pending",
            ));
        }
        let _gate = self.gate.lock().await;
        if self.owner_active.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::ResourceBusy,
                "cold owner cleanup is pending",
            ));
        }
        if let Some(current) = self.load()? {
            if current.cleanup_pending {
                return Err(io::Error::new(
                    io::ErrorKind::ResourceBusy,
                    "cold cleanup is pending; retry cancel or restart Nexus",
                ));
            }
            if !current.phase.is_terminal() {
                return Err(io::Error::new(
                    io::ErrorKind::ResourceBusy,
                    "a cold-install operation is already active",
                ));
            }
        }
        let now = unix_time_seconds();
        let suffix = unix_time_nanos_for_update();
        let release_id = release_id_for_tag(&tag, suffix);
        let operation_id = format!("cold-{suffix}");
        let candidate = self
            .paths
            .downloads_dir
            .join(format!(".{operation_id}-{release_id}"));
        let operation = ColdOperation {
            credential_recovery_path: None,
            offline_contents: None,
            operation_id: operation_id.clone(),
            kind: kind.into(),
            archive_path,
            phase: ColdOperationPhase::Queued,
            tag,
            source,
            mode,
            release_id: selected_release.unwrap_or(release_id),
            candidate: candidate.to_string_lossy().into_owned(),
            candidate_revision: None,
            progress_percent: 0,
            started_at_unix: now,
            updated_at_unix: Some(now),
            foundation_plan_id: None,
            warning: None,
            output_tail: None,
            supply_plan: None,
            confirmation: None,
            error: None,
            owner_quiescent: false,
            cleanup_pending: false,
            cleanup_error: None,
        };
        self.write(&operation)?;
        let token = CancellationToken::default();
        *self.cancellation.lock().await = Some((operation_id, token));
        self.owner_active.store(true, Ordering::Release);
        Ok(operation)
    }

    pub(crate) async fn token(&self, operation_id: &str) -> CancellationToken {
        let mut current = self.cancellation.lock().await;
        if let Some((id, token)) = current.as_ref() {
            if id == operation_id {
                return token.clone();
            }
        }
        let token = CancellationToken::default();
        *current = Some((operation_id.to_owned(), token.clone()));
        token
    }

    pub(crate) async fn cancel(&self, operation_id: &str) -> io::Result<ColdOperation> {
        // Signal the owned probe before waiting for the publication gate.
        // The tuple prevents a stale request from cancelling a newer owner.
        {
            let current = self.cancellation.lock().await;
            if let Some((id, token)) = current.as_ref() {
                if id == operation_id {
                    token.cancel();
                }
            }
        }
        let _gate = self.gate.lock().await;
        let mut operation = self
            .load()?
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "cold operation not found"))?;
        if operation.operation_id != operation_id {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "stale cold operation id",
            ));
        }
        if operation.phase.is_terminal() {
            if operation.cleanup_pending && operation.owner_quiescent {
                match offline::cleanup_candidate(&self.paths, &operation) {
                    Ok(()) => {
                        operation.cleanup_pending = false;
                        operation.cleanup_error = None;
                    }
                    Err(error) => {
                        operation.cleanup_error =
                            Some(format!("candidate cleanup failed: {error}"));
                    }
                }
                operation.updated_at_unix = Some(unix_time_seconds());
                self.write(&operation)?;
            }
            return Ok(operation);
        }
        if let Some((id, token)) = self.cancellation.lock().await.as_ref() {
            if id == operation_id {
                token.cancel();
            }
        }
        if operation.phase == ColdOperationPhase::AwaitingConfirmation {
            operation.phase = ColdOperationPhase::Cancelled;
            operation.progress_percent = 100;
            operation.owner_quiescent = true;
            operation.cleanup_pending = true;
            operation.error = Some("cold install cancelled".to_owned());
            match offline::cleanup_candidate(&self.paths, &operation)
            {
                Ok(()) => {
                    operation.cleanup_pending = false;
                    operation.cleanup_error = None;
                }
                Err(error) => {
                    operation.cleanup_error = Some(format!("candidate cleanup failed: {error}"))
                }
            }
        } else {
            operation.phase = ColdOperationPhase::Cancelling;
        }
        operation.updated_at_unix = Some(unix_time_seconds());
        self.write(&operation)?;
        Ok(operation)
    }
}

pub(crate) async fn prepare(state: AppState, operation_id: String) {
    let offline = state.cold.load().ok().flatten().is_some_and(|operation| operation.kind.starts_with("offline_"));
    let result = if offline { offline::run(&state,&operation_id).await } else { prepare_inner(&state,&operation_id).await };
    if let Err(error) = result {
        let _ = settle_failure(&state, &operation_id, error).await;
    }
    if state.cold.load().ok().flatten().is_some_and(|operation| {
        operation.operation_id == operation_id
            && operation.owner_quiescent
            && (operation.phase.is_terminal()
                || operation.phase == ColdOperationPhase::AwaitingConfirmation)
    }) {
        state.cold.owner_active.store(false, Ordering::Release);
    }
}

async fn prepare_inner(state: &AppState, operation_id: &str) -> io::Result<()> {
    let operation = state
        .cold
        .load()?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "cold operation not found"))?;
    let cancellation = state.cold.token(operation_id).await;
    ensure_not_cancelled(&cancellation)?;
    if state
        .releases
        .load()?
        .releases
        .iter()
        .any(|release| release.version == operation.tag)
    {
        return promote_existing(state, operation_id, &operation.tag).await;
    }
    state.releases.ensure_capacity_for_new()?;
    // Disk preflight: the clone plus pnpm build needs several GiB on the
    // data-root volume; fail before the clone instead of mid-install.
    nexus_core::disk::ensure_free_space(
        Path::new(&operation.candidate),
        nexus_core::disk::MIN_INSTALL_FREE_BYTES,
    )?;
    state
        .cold
        .update(operation_id, ColdOperationPhase::Cloning, 10, None)
        .await?;
    let candidate = PathBuf::from(&operation.candidate);
    if candidate.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "server-owned candidate already exists",
        ));
    }
    let runtime = resolved_runtime_config(state).await?;
    crate::git_worker::clone_candidate(
        APPROVED_UPSTREAM,
        &operation.tag,
        &candidate,
        &state.paths.run_dir,
        COMMAND_TIMEOUT,
        &cancellation,
        crate::git_worker::selected_external(&runtime),
    )
    .await?;
    let revision =
        candidate_revision(&runtime, &candidate, &state.paths.run_dir, &cancellation).await?;
    record_candidate_revision(state, operation_id, &revision).await?;
    ensure_not_cancelled(&cancellation)?;
    state
        .cold
        .update(operation_id, ColdOperationPhase::Planning, 25, None)
        .await?;
    let plan = plan_candidate(state, &operation, &candidate).await?;
    if plan.suggested_actions.iter().all(|action| {
        matches!(
            action.action,
            RuntimePlanActionKind::UsePinned | RuntimePlanActionKind::UseExisting
        )
    }) {
        let warnings = plan
            .tools
            .iter()
            .filter_map(|tool| {
                tool.warning
                    .as_deref()
                    .map(|warning| format!("{}: {}", tool.name, warning))
            })
            .collect::<Vec<_>>()
            .join("; ");
        record_foundation_plan(
            state,
            operation_id,
            &plan.plan_id,
            (!warnings.is_empty()).then_some(warnings),
        )
        .await?;
        let runtime = runtime_from_plan(&plan)?;
        return build_and_publish(state, operation_id, runtime).await;
    }
    if plan
        .suggested_actions
        .iter()
        .any(|action| action.action == RuntimePlanActionKind::ConfigureExternal)
    {
        // Runtime provisioning by download is retired; an unresolvable tool
        // is a user-actionable failure, not a supply request.
        let details = plan
            .tools
            .iter()
            .filter(|tool| tool.name != "git" && tool.state != RuntimePlanToolState::Reusable)
            .map(|tool| {
                format!(
                    "{}: {}",
                    tool.name,
                    tool.reason.as_deref().unwrap_or("unavailable")
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "no usable node/pnpm runtime ({details}); select paths in settings or reinstall Nexus to use its bundled runtime"
            ),
        ));
    }
    // assemble_runtime_plan emits only UsePinned/UseExisting/ConfigureExternal;
    // retired provisioning actions would mean a protocol regression.
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "runtime plan produced a retired provisioning action",
    ))
}

async fn ensure_publication_admission(state: &AppState, operation_id: &str) -> io::Result<()> {
    if let Err(response) = super::ensure_mutation_ready_for_owner(state, Some(operation_id)).await {
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024).await.map_err(io::Error::other)?;
        let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
        return Err(io::Error::new(io::ErrorKind::ResourceBusy, format!(
            "Cold installation publication blocked ({}; HTTP {}): {}",
            value["code"].as_str().unwrap_or("mutation_blocked"), status.as_u16(),
            value["message"].as_str().unwrap_or("Inspect recovery status before retrying"))));
    }
    Ok(())
}

async fn build_and_publish(
    state: &AppState,
    operation_id: &str,
    runtime: RuntimeConfig,
) -> io::Result<()> {
    let operation = state
        .cold
        .load()?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "cold operation not found"))?;
    let cancellation = state.cold.token(operation_id).await;
    let candidate = PathBuf::from(&operation.candidate);
    let _tail_watcher_guard = TailWatcherStop::spawn(state.clone(), operation_id.to_owned());
    let revision = operation.candidate_revision.as_deref().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "cold candidate revision is missing")
    })?;
    state
        .cold
        .update(operation_id, ColdOperationPhase::Installing, 55, None)
        .await?;
    run_pnpm(
        &runtime,
        ["install", "--frozen-lockfile"],
        &candidate,
        revision,
        &state.paths.run_dir,
        &cancellation,
    )
    .await
    .map_err(|error| dependency_registry_hint(error, runtime.source))?;
    state
        .cold
        .update(operation_id, ColdOperationPhase::Building, 72, None)
        .await?;
    run_pnpm(
        &runtime,
        ["build"],
        &candidate,
        revision,
        &state.paths.run_dir,
        &cancellation,
    )
    .await?;
    state
        .cold
        .update(operation_id, ColdOperationPhase::Verifying, 85, None)
        .await?;
    verify_built_cli(&candidate)?;
    ensure_not_cancelled(&cancellation)?;
    let revision =
        candidate_revision(&runtime, &candidate, &state.paths.run_dir, &cancellation).await?;
    if operation.candidate_revision.as_deref() != Some(&revision) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "cold candidate revision changed before publication",
        ));
    }

    let fresh = plan_candidate(state, &operation, &candidate).await?;
    if operation
        .foundation_plan_id
        .as_ref()
        .is_some_and(|id| id != &fresh.plan_id)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "candidate or runtime plan changed before publication",
        ));
    }

    let lifecycle = state.supervisor.acquire_lifecycle().await;
    ensure_publication_admission(state, operation_id).await?;
    let _updater = state
        .updater
        .try_acquire_gate()
        .map_err(|error| io::Error::new(io::ErrorKind::ResourceBusy, error.to_string()))?;
    super::ensure_harness_selection_quiescent(
        state,
        &lifecycle,
        "release_change_conflict",
        "cannot publish a cold release while Harness is active",
    )
    .await
    .map_err(|_| {
        io::Error::new(
            io::ErrorKind::ResourceBusy,
            "Harness must be positively stopped before cold release publication",
        )
    })?;
    state.releases.ensure_capacity_for_new()?;
    let _cold_commit = state.cold.gate.lock().await;
    ensure_not_cancelled(&cancellation)?;
    let mut final_operation = state
        .cold
        .load()?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "cold operation not found"))?;
    if final_operation.operation_id != operation_id || final_operation.phase != ColdOperationPhase::Verifying
        || final_operation.cleanup_pending {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
                "cold operation cannot be committed",
        ));
    }
    final_operation.phase = ColdOperationPhase::Registering;
    final_operation.progress_percent = 90;
    final_operation.updated_at_unix = Some(unix_time_seconds());
    state.cold.write(&final_operation)?;
    let node = runtime
        .node
        .as_ref()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "verified Node pin is missing"))?
        .path
        .clone();
    // The launch configuration must reference the `{release_root}`
    // placeholder, never a concrete slot directory, so later switches,
    // rollbacks, and checkpoint restores keep the launch on the current
    // pointer.
    let harness = HarnessLaunchSpec {
        mode: HarnessLaunchMode::Node,
        program: node,
        args: vec![
            "{release_root}\\apps/cli/lib/bin.js".to_owned(),
            "--profile".to_owned(),
            "{profile}".to_owned(),
        ],
        working_dir: Some(PathBuf::from("{release_root}")),
        readiness_url: None,
        readiness_timeout_secs: None,
        readiness_token_required: false,
    };
    let previous_config = state.config.load()?;
    let mut config = previous_config.clone();
    config.runtime = Some(runtime.clone());
    config.harness = Some(harness.clone());
    config.update = Some(nexus_core::UpdateSpec {
        source: APPROVED_UPSTREAM.to_owned(),
        ref_name: operation.tag.clone(),
        git_program: runtime
            .git
            .as_ref()
            .map(|pin| pin.path.clone())
            .unwrap_or_else(|| PathBuf::from("git")),
        build_program: None,
        build_args: Vec::new(),
        verify_program: None,
        verify_args: Vec::new(),
        timeout_secs: Some(COMMAND_TIMEOUT.as_secs()),
    });
    if config.external_harness.is_none() {
        if let Err(error) = state.releases.ensure_rollback_protection(&operation.release_id) {
            if error.kind() != io::ErrorKind::WouldBlock { return Err(error); }
            return prepare_repair_slot(state, &final_operation, &candidate);
        }
    }
    let mut intent = state
        .cold
        .prepare_publication(&final_operation, true, config.clone())?;
    let catalog = state.releases.register_prepared(
        &candidate,
        &operation.release_id,
        &operation.tag,
        Some(APPROVED_UPSTREAM.to_owned()),
        Some("Nexus cold install".to_owned()),
    )?;
    crate::compatibility::prepare(
        &state.paths,
        &state.snapshots.configured_dsh_home()?,
        &state.profiles.load()?.active_profile,
        &operation.release_id,
        &state.releases.release_root(&operation.release_id)?,
        &harness.program,
        true,
        &cancellation,
    )
    .await?;
    ensure_not_cancelled(&cancellation)?;
    state.config.write(&config)?;
    final_operation.phase = ColdOperationPhase::Promoting;
    final_operation.progress_percent = 96;
    final_operation.updated_at_unix = Some(unix_time_seconds());
    state.cold.write(&final_operation)?;
    let before = state.releases.load()?;
    let promoted = match if config.external_harness.is_some() { state.releases.promote(&operation.release_id) } else { state.releases.promote_with_rollback(&operation.release_id) } {
        Ok(catalog) => catalog,
        Err(error) => {
            let _ = state.config.write_recovery_document(&previous_config);
            let _ = state.releases.remove(&operation.release_id);
            return Err(error);
        }
    };
    if let Err(error) = super::persist_release_catalog_state(state, &promoted, false).await {
        let _ = state.releases.restore_release_pointers(
            before.current_release.as_deref(),
            before.last_known_good.as_deref(),
        );
        let _ = state.config.write_recovery_document(&previous_config);
        return Err(io::Error::other(format!(
            "failed to persist Agent current release: {error}"
        )));
    }
    let release = catalog.find(&operation.release_id).cloned();
    let finished = UpdateRuntimeInfo {
        state: UpdateState::Succeeded,
        release_id: release.map(|item| item.id),
        started_at_unix: Some(operation.started_at_unix),
        finished_at_unix: Some(unix_time_seconds()),
        exit_code: Some(0),
        error: None,
    };
    intent.committed = true;
    state.cold.write_intent(&intent)?;
    state.updater.state_store().write(&finished)?;
    final_operation.phase = ColdOperationPhase::Succeeded;
    final_operation.progress_percent = 100;
    final_operation.updated_at_unix = Some(unix_time_seconds());
    final_operation.owner_quiescent = true;
    final_operation.cleanup_pending = false;
    final_operation.cleanup_error = None;
    state.cold.write(&final_operation)?;
    fs::remove_file(state.cold.intent_path())?;
    Ok(())
}

fn prepare_repair_slot(state: &AppState, operation: &ColdOperation, candidate: &Path) -> io::Result<()> {
    let mut prepared = operation.clone();
    prepared.warning = Some("rollback_health_required: Version prepared only. Select it in Release slots and confirm a manual switch; the current selection and configuration are unchanged.".into());
    let mut intent = state.cold.prepare_publication_inner(&prepared, true, state.config.load()?, true)?;
    state.releases.register_prepared(candidate, &prepared.release_id, &prepared.tag, Some(APPROVED_UPSTREAM.into()), Some("Prepared for explicit manual recovery".into()))?;
    intent.committed = true;
    state.cold.write_intent(&intent)?;
    state.cold.recover_publication()
}

async fn promote_existing(state: &AppState, operation_id: &str, tag: &str) -> io::Result<()> {
    let lifecycle = state.supervisor.acquire_lifecycle().await;
    let _updater = state
        .updater
        .try_acquire_gate()
        .map_err(|error| io::Error::new(io::ErrorKind::ResourceBusy, error.to_string()))?;
    super::ensure_harness_selection_quiescent(
        state,
        &lifecycle,
        "release_change_conflict",
        "cannot switch release while Harness is active",
    )
    .await
    .map_err(|_| {
        io::Error::new(
            io::ErrorKind::ResourceBusy,
            "Harness must be positively stopped before release switch",
        )
    })?;
    let cancellation = state.cold.token(operation_id).await;
    let _cold_commit = state.cold.gate.lock().await;
    ensure_not_cancelled(&cancellation)?;
    let mut final_operation = state
        .cold
        .load()?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "cold operation not found"))?;
    if final_operation.operation_id != operation_id || final_operation.phase.is_terminal() {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "cold operation cannot be committed",
        ));
    }
    let release = state
        .releases
        .load()?
        .releases
        .into_iter()
        .filter(|item| item.version == tag)
        .max_by_key(|item| item.installed_at_unix)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "installed tag disappeared"))?;
    crate::compatibility::for_release(state, &release.id, true, &cancellation).await?;
    ensure_not_cancelled(&cancellation)?;
    final_operation.release_id = release.id.clone();
    let external = state.config.load()?.external_harness.is_some();
    if !external { state.releases.ensure_rollback_protection(&release.id)?; }
    let mut intent =
        state
            .cold
            .prepare_publication(&final_operation, false, state.config.load()?)?;
    let before = state.releases.load()?;
    let catalog = if external { state.releases.promote(&release.id) } else { state.releases.promote_with_rollback(&release.id) }?;
    if let Err(error) = super::persist_release_catalog_state(state, &catalog, true).await {
        let _ = state.releases.restore_release_pointers(
            before.current_release.as_deref(),
            before.last_known_good.as_deref(),
        );
        return Err(io::Error::other(format!(
            "failed to persist Agent current release: {error}"
        )));
    }
    intent.committed = true;
    state.cold.write_intent(&intent)?;
    state.cold.recover_publication()?;
    Ok(())
}

async fn record_foundation_plan(
    state: &AppState,
    operation_id: &str,
    plan_id: &str,
    warning: Option<String>,
) -> io::Result<()> {
    let _gate = state.cold.gate.lock().await;
    let mut operation = state
        .cold
        .load()?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "cold operation not found"))?;
    if operation.operation_id != operation_id {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "cold operation changed",
        ));
    }
    operation.foundation_plan_id = Some(plan_id.to_owned());
    operation.warning = warning;
    operation.updated_at_unix = Some(unix_time_seconds());
    state.cold.write(&operation)
}

async fn record_candidate_revision(
    state: &AppState,
    operation_id: &str,
    revision: &str,
) -> io::Result<()> {
    let _gate = state.cold.gate.lock().await;
    let mut operation = state
        .cold
        .load()?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "cold operation not found"))?;
    if operation.operation_id != operation_id {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "cold operation changed",
        ));
    }
    operation.candidate_revision = Some(revision.to_owned());
    operation.updated_at_unix = Some(unix_time_seconds());
    state.cold.write(&operation)
}

async fn plan_candidate(
    state: &AppState,
    operation: &ColdOperation,
    candidate: &Path,
) -> io::Result<nexus_protocol::RuntimePlanResponse> {
    crate::runtime_plan::plan_candidate_release(
        candidate,
        RuntimePlanRequest {
            release_id: operation.release_id.clone(),
            source: operation.source,
            mode: operation.mode,
        },
        &state.config,
        &crate::runtime::RuntimeRequestContext::production(),
    )
    .await
}

pub(crate) async fn resolved_runtime_config(state: &AppState) -> io::Result<RuntimeConfig> {
    let configured = state.config.load()?.runtime.unwrap_or_default();
    let request = crate::runtime::RuntimeRequestContext::production();
    let observed =
        crate::runtime::observe_runtime_selection_until(&state.paths, Some(&configured), &request)
            .await;
    let mut runtime = configured;
    for tool in observed.tools.into_iter().filter(|tool| tool.available) {
        let Some(path) = tool.path.map(PathBuf::from) else {
            continue;
        };
        if !path.is_absolute() {
            continue;
        }
        let pin = RuntimePin {
            path,
            ownership: match tool.source.as_deref() {
                Some("nexus") => RuntimeOwnership::Nexus,
                Some("bundled") => RuntimeOwnership::Bundled,
                _ => RuntimeOwnership::System,
            },
        };
        match tool.name.as_str() {
            "git" => runtime.git = Some(pin),
            "node" => runtime.node = Some(pin),
            "pnpm" => runtime.pnpm = Some(pin),
            _ => {}
        }
    }
    Ok(runtime)
}

/// The first install on a clean machine fetches every upstream dependency.
/// When that fetch fails on the official registry, point the user at the
/// mirror option instead of leaving a bare network error.
fn dependency_registry_hint(error: io::Error, source: RuntimeSource) -> io::Error {
    if source == RuntimeSource::Official {
        return io::Error::new(
            error.kind(),
            format!(
                "{error}; if this failed to fetch packages or timed out, switch the dependency registry to npmmirror in Settings and retry"
            ),
        );
    }
    error
}

fn runtime_from_plan(plan: &nexus_protocol::RuntimePlanResponse) -> io::Result<RuntimeConfig> {
    let mut runtime = RuntimeConfig {
        source: plan.source,
        mode: plan.mode,
        ..RuntimeConfig::default()
    };
    for tool in &plan.tools {
        if tool.name == "git" && tool.state != nexus_protocol::RuntimePlanToolState::Reusable {
            continue;
        }
        let path = tool
            .path
            .as_ref()
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("reusable {} path is not absolute", tool.name),
                )
            })?;
        let pin = RuntimePin {
            path,
            ownership: tool.ownership.unwrap_or(RuntimeOwnership::System),
        };
        match tool.name.as_str() {
            "git" => runtime.git = Some(pin),
            "node" => runtime.node = Some(pin),
            "pnpm" => runtime.pnpm = Some(pin),
            _ => {}
        }
    }
    runtime.validate()?;
    Ok(runtime)
}

async fn run_pnpm(
    runtime: &RuntimeConfig,
    args: impl IntoIterator<Item = &'static str>,
    cwd: &Path,
    revision: &str,
    diagnostic_dir: &Path,
    cancellation: &CancellationToken,
) -> io::Result<()> {
    let command = resolve_runtime_command(runtime, "pnpm")?.ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "verified pnpm runtime is missing")
    })?;
    let mut args: Vec<OsString> = args.into_iter().map(OsString::from).collect();
    if args.first().is_some_and(|arg| arg == "install") {
        args.push("--reporter=append-only".into());
    }
    let args = build_pnpm_args(runtime, args);
    ensure_not_cancelled(cancellation)?;
    let mut child = std::process::Command::new(&command.program);
    child
        .args(command.prefix_args.into_iter().chain(args))
        .current_dir(cwd)
        // Upstream accepts this metadata without invoking git.exe. Embedded
        // Git supplies the recorded revision, but is not a Git CLI on PATH.
        .env("DSH_CLIENT_COMMIT_HASH", revision)
        .stdin(Stdio::null())
        .stdout(Stdio::null());
    for (key, value) in build_runtime_child_env(runtime, std::env::var_os("PATH").as_deref())? {
        child.env(key, value);
    }
    run_owned_command_diagnostics(
        child,
        "pnpm",
        COMMAND_TIMEOUT,
        diagnostic_dir,
        cancellation,
        true,
    )
    .await
}

pub(crate) async fn run_owned_command(
    command: std::process::Command,
    phase: &str,
    duration: Duration,
    diagnostic_dir: &Path,
    cancellation: &CancellationToken,
) -> io::Result<()> {
    run_owned_command_diagnostics(
        command,
        phase,
        duration,
        diagnostic_dir,
        cancellation,
        false,
    )
    .await
}

async fn run_owned_command_diagnostics(
    mut command: std::process::Command,
    phase: &str,
    duration: Duration,
    diagnostic_dir: &Path,
    cancellation: &CancellationToken,
    capture_stdout: bool,
) -> io::Result<()> {
    fs::create_dir_all(diagnostic_dir)?;
    let diagnostic_path = diagnostic_dir.join(format!(
        "cold-command-{}-{}.stderr.tmp",
        std::process::id(),
        unix_time_nanos_for_update()
    ));
    let diagnostic = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&diagnostic_path)?;
    command.stderr(Stdio::from(diagnostic));
    let stdout_path = if capture_stdout {
        let path = diagnostic_path.with_extension("stdout.tmp");
        let file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)?;
        command.stdout(Stdio::from(file));
        Some(path)
    } else {
        None
    };
    let token = cancellation.clone();
    if let Some(name) = token.job_name() { command.env("NEXUS_OWNED_JOB_NAME", name); }
    let phase = phase.to_owned();
    let result = tokio::task::spawn_blocking(move || {
        crate::dsh::run_cold_process(&mut command, duration, || token.is_cancelled())
    })
    .await
    .map_err(|error| {
        io::Error::other(ColdCommandFailure {
            message: format!("owned command worker failed: {error}"),
            owner_quiescent: false,
        })
    })?;
    let per_stream_limit = if capture_stdout {
        COMMAND_DIAGNOSTIC_BYTES / 2
    } else {
        COMMAND_DIAGNOSTIC_BYTES
    };
    let diagnostic = read_command_diagnostic(&diagnostic_path, per_stream_limit);
    let stdout = stdout_path
        .as_ref()
        .map(|path| read_command_diagnostic(path, per_stream_limit))
        .transpose();
    let cleanup = fs::remove_file(&diagnostic_path)
        .and_then(|_| stdout_path.as_ref().map_or(Ok(()), fs::remove_file));
    let (diagnostic, truncated) = diagnostic.map_err(|error| {
        io::Error::other(ColdCommandFailure {
            message: format!("owned command diagnostic failed: {error}"),
            owner_quiescent: result
                .as_ref()
                .err()
                .map_or(true, crate::dsh::cold_process_owner_quiescent),
        })
    })?;
    let mut suffix = if diagnostic.is_empty() {
        String::new()
    } else {
        format!(
            "; stderr{}: {}",
            if truncated { " (tail truncated)" } else { "" },
            diagnostic.trim()
        )
    };
    let stdout = stdout.map_err(|error| {
        io::Error::other(ColdCommandFailure {
            message: format!("owned command stdout diagnostic failed: {error}"),
            owner_quiescent: result
                .as_ref()
                .err()
                .map_or(true, crate::dsh::cold_process_owner_quiescent),
        })
    })?;
    if let Some((stdout, truncated)) = stdout.filter(|(text, _)| !text.is_empty()) {
        suffix.push_str(&format!(
            "; stdout{}: {}",
            if truncated { " (tail truncated)" } else { "" },
            stdout.trim()
        ));
    }
    let command_result = match result {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(io::Error::other(format!(
            "{phase} exited with {status}{suffix}"
        ))),
        Err(error) => {
            let owner_quiescent = crate::dsh::cold_process_owner_quiescent(&error);
            Err(io::Error::new(
                error.kind(),
                ColdCommandFailure {
                    message: format!("{phase} failed: {error}{suffix}"),
                    owner_quiescent,
                },
            ))
        }
    };
    match (command_result, cleanup) {
        (result, Ok(())) => result,
        (Ok(()), Err(cleanup)) => Err(io::Error::new(
            cleanup.kind(),
            format!("{phase} diagnostic cleanup failed: {cleanup}"),
        )),
        (Err(primary), Err(cleanup)) => {
            let owner_quiescent = primary
                .get_ref()
                .and_then(|source| source.downcast_ref::<ColdCommandFailure>())
                .map_or(true, |failure| failure.owner_quiescent);
            Err(io::Error::new(
                primary.kind(),
                ColdCommandFailure {
                    message: format!("{primary}; diagnostic cleanup also failed: {cleanup}"),
                    owner_quiescent,
                },
            ))
        }
    }
}

fn read_command_diagnostic(path: &Path, maximum: usize) -> io::Result<(String, bool)> {
    use io::{Read, Seek, SeekFrom};

    let mut file = fs::File::open(path)?;
    let length = file.metadata()?.len();
    let truncated = length > maximum as u64;
    if truncated {
        file.seek(SeekFrom::End(-(maximum as i64)))?;
    }
    let mut bytes = Vec::with_capacity(length.min(maximum as u64) as usize);
    file.take(maximum as u64).read_to_end(&mut bytes)?;
    if truncated {
        if let Some(boundary) = bytes.iter().position(|byte| matches!(*byte, b'\n' | b'\r')) {
            bytes.drain(..=boundary);
        } else {
            bytes.clear();
        }
    }
    let lossy = String::from_utf8_lossy(&bytes);
    let redacted = redact_diagnostics_payload(lossy.as_bytes()).0;
    let mut text = String::from_utf8_lossy(&redacted).into_owned();
    let expanded = text.len() > maximum;
    if expanded {
        let mut boundary = text.len() - maximum;
        while !text.is_char_boundary(boundary) {
            boundary += 1;
        }
        text.drain(..boundary);
    }
    Ok((text, truncated || expanded))
}

async fn candidate_revision(
    runtime: &RuntimeConfig,
    candidate: &Path,
    diagnostic_dir: &Path,
    cancellation: &CancellationToken,
) -> io::Result<String> {
    ensure_not_cancelled(cancellation)?;
    crate::git_worker::head(
        candidate,
        diagnostic_dir,
        cancellation,
        crate::git_worker::selected_external(runtime),
    )
    .await
}
fn verify_built_cli(root: &Path) -> io::Result<()> {
    let manifest = fs::read(root.join("apps/cli/package.json"))?;
    if manifest.len() > 1024 * 1024 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "CLI package manifest is too large",
        ));
    }
    let value: serde_json::Value = serde_json::from_slice(&manifest)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let bin_matches = match value.get("bin") {
        Some(serde_json::Value::String(path)) => path == "lib/bin.js",
        Some(serde_json::Value::Object(entries)) => entries
            .values()
            .any(|value| value.as_str() == Some("lib/bin.js")),
        _ => false,
    };
    if !bin_matches {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "apps/cli package bin does not identify lib/bin.js",
        ));
    }
    let entry = root.join("apps/cli/lib/bin.js");
    if !entry.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "built CLI entry apps/cli/lib/bin.js is missing",
        ));
    }
    Ok(())
}

fn release_id_for_tag(tag: &str, suffix: u128) -> String {
    let clean: String = tag
        .chars()
        .take(72)
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    format!("harness-{}-{}", clean.trim_matches('-'), suffix)
}

fn ensure_not_cancelled(token: &CancellationToken) -> io::Result<()> {
    if token.is_cancelled() {
        Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "cold install cancelled",
        ))
    } else {
        Ok(())
    }
}

async fn settle_failure(
    state: &AppState,
    operation_id: &str,
    primary: io::Error,
) -> io::Result<()> {
    let primary_text = primary.to_string();
    let owner_quiescent = primary
        .get_ref()
        .and_then(|source| source.downcast_ref::<ColdCommandFailure>())
        .map_or(true, |failure| failure.owner_quiescent);
    let _lifecycle = state.supervisor.acquire_lifecycle().await;
    let _updater = match state.updater.try_acquire_gate() {
        Ok(updater) => updater,
        Err(error) => {
            let error = io::Error::new(io::ErrorKind::ResourceBusy, error.to_string());
            let _gate = state.cold.gate.lock().await;
            state.cold.write_failure_pending(
                operation_id,
                &primary_text,
                owner_quiescent,
                format!("failure reconciliation is pending: {error}"),
            )?;
            return Err(error);
        }
    };
    let _gate = state.cold.gate.lock().await;
    if !owner_quiescent {
        state.cold.write_failure_pending(operation_id, &primary_text, false,
            "owned process cleanup did not prove quiescence; publication files retained until recovery".into())?;
        return Err(primary);
    }
    let reconciled = match state.cold.reconcile_publication() {
        Ok(operation) => operation,
        Err(reconcile_error) => {
            state.cold.write_failure_pending(
                operation_id,
                &primary_text,
                owner_quiescent,
                format!("publication reconciliation failed: {reconcile_error}"),
            )?;
            return Err(reconcile_error);
        }
    };
    if let Some(operation) = reconciled {
        let catalog = state.releases.load()?;
        if let Err(error) = super::persist_release_catalog_state(state, &catalog, false).await {
            let error = io::Error::other(error);
            state.cold.write_failure_pending(
                operation_id,
                &primary_text,
                owner_quiescent,
                format!("publication state synchronization failed: {error}"),
            )?;
            return Err(error);
        }
        if let Err(error) = state.cold.finish_publication(&operation) {
            state.cold.write_failure_pending(
                operation_id,
                &primary_text,
                owner_quiescent,
                format!("publication finalization failed: {error}"),
            )?;
            return Err(error);
        }
        if matches!(operation.phase, ColdOperationPhase::Succeeded | ColdOperationPhase::Prepared) {
            return Ok(());
        }
    }
    let Some(mut operation) = state
        .cold
        .load()?
        .filter(|op| op.operation_id == operation_id)
    else {
        return Ok(());
    };
    operation.phase = if operation.phase == ColdOperationPhase::Cancelling {
        ColdOperationPhase::Cancelled
    } else {
        ColdOperationPhase::Failed
    };
    operation.progress_percent = 100;
    operation.updated_at_unix = Some(unix_time_seconds());
    operation.error = Some(primary_text);
    operation.owner_quiescent = owner_quiescent;
    operation.cleanup_pending = true;
    operation.cleanup_error = if owner_quiescent {
        None
    } else {
        Some("owned process cleanup did not prove quiescence; restart Nexus to recover".into())
    };
    state.cold.write(&operation)?;

    if owner_quiescent {
        match offline::cleanup_candidate(&state.paths, &operation) {
            Ok(()) => {
                operation.cleanup_pending = false;
                operation.cleanup_error = None;
            }
            Err(error) => {
                operation.cleanup_error = Some(format!("candidate cleanup failed: {error}"));
            }
        }
        operation.updated_at_unix = Some(unix_time_seconds());
        state.cold.write(&operation)?;
    }
    Ok(())
}

pub(crate) fn remove_owned_directory(parent: &Path, target: &Path) -> io::Result<()> {
    let parent = fs::canonicalize(parent)?;
    let Some(target_parent) = target.parent() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "cleanup target escapes owned root",
        ));
    };
    let target_parent = fs::canonicalize(target_parent)?;
    if target_parent != parent {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "cleanup target escapes owned root",
        ));
    }
    let metadata = match fs::symlink_metadata(target) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(cleanup_entry_error(target, "root", error)),
    };
    if is_link_or_reparse(&metadata) {
        return remove_link_object(target, &metadata)
            .map_err(|error| cleanup_entry_error(target, "root reparse point", error));
    }
    let target = fs::canonicalize(target)?;
    if target == parent || !nexus_core::is_within(&parent, &target) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "cleanup target escapes owned root",
        ));
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let mut stack = vec![(target, false)];
    let mut visited = 0usize;
    while let Some((path, expanded)) = stack.pop() {
        visited += 1;
        if visited > 2_000_000 || std::time::Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "owned candidate cleanup exceeded its bound",
            ));
        }
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| cleanup_entry_error(&path, "metadata", error))?;
        if metadata.is_dir() && !is_link_or_reparse(&metadata) {
            if expanded {
                remove_directory_entry(&path)
                    .map_err(|error| cleanup_entry_error(&path, "directory", error))?;
            } else {
                stack.push((path.clone(), true));
                for entry in fs::read_dir(&path)
                    .map_err(|error| cleanup_entry_error(&path, "read directory", error))?
                {
                    if stack.len() > 1_000_000 || std::time::Instant::now() >= deadline {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "owned candidate cleanup exceeded its bound",
                        ));
                    }
                    let entry = entry
                        .map_err(|error| cleanup_entry_error(&path, "directory entry", error))?;
                    stack.push((entry.path(), false));
                }
            }
        } else if is_link_or_reparse(&metadata) {
            remove_link_object(&path, &metadata)
                .map_err(|error| cleanup_entry_error(&path, "reparse point", error))?;
        } else {
            remove_file_entry(&path).map_err(|error| cleanup_entry_error(&path, "file", error))?;
        }
    }
    Ok(())
}

fn cleanup_entry_error(path: &Path, entry_type: &str, error: io::Error) -> io::Error {
    io::Error::new(
        error.kind(),
        format!(
            "{entry_type} cleanup failed at {} (os error {:?}): {error}",
            path.display(),
            error.raw_os_error()
        ),
    )
}

fn is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes()
            & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
            != 0
    }
    #[cfg(not(windows))]
    false
}

fn remove_link_object(path: &Path, metadata: &fs::Metadata) -> io::Result<()> {
    if metadata.is_dir() {
        match fs::remove_dir(path) {
            Ok(()) => Ok(()),
            Err(first) => fs::remove_file(path).map_err(|_| first),
        }
    } else {
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(first) => fs::remove_dir(path).map_err(|_| first),
        }
    }
}

fn remove_file_entry(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(first) => {
            #[cfg(windows)]
            {
                let mut permissions = fs::metadata(path)?.permissions();
                if permissions.readonly() {
                    permissions.set_readonly(false);
                    fs::set_permissions(path, permissions)?;
                    return fs::remove_file(path);
                }
            }
            Err(first)
        }
    }
}

fn remove_directory_entry(path: &Path) -> io::Result<()> {
    match fs::remove_dir(path) {
        Ok(()) => Ok(()),
        Err(first) => {
            #[cfg(windows)]
            {
                let mut permissions = fs::metadata(path)?.permissions();
                if permissions.readonly() {
                    permissions.set_readonly(false);
                    fs::set_permissions(path, permissions)?;
                    return fs::remove_dir(path);
                }
            }
            Err(first)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn interrupted_operation_recovery_guidance_matches_operation_kind() {
        for (kind, next) in [("offline_import", "retry the offline import"),
            ("offline_export", "retry the offline export"), ("install", "start the tag switch again")] {
            let state = crate::switch_ownership_tests::switch_test_state(&format!("cold-guidance-{kind}"));
            let mut operation = state.cold.begin("v-interrupted".into(), RuntimeSource::Official,
                RuntimeInstallMode::Portable).await.unwrap();
            operation.kind = kind.into();
            operation.phase = ColdOperationPhase::Verifying;
            state.cold.write(&operation).unwrap();
            state.cold.owner_active.store(false, Ordering::Release);
            ColdCoordinator::new(state.paths.clone()).recover().unwrap();
            let recovered = state.cold.load().unwrap().unwrap();
            assert_eq!(recovered.phase, ColdOperationPhase::Failed);
            assert_eq!(recovered.operation_id, operation.operation_id);
            assert_eq!(recovered.error.as_deref(), Some(format!("previous cold-install owner was not attached; {next}").as_str()));
            assert!(recovered.owner_quiescent);
            fs::remove_dir_all(&state.paths.root).unwrap();
        }
    }

    #[tokio::test]
    async fn cold_future_schema_and_unknown_fields_preserve_records() {
        let state=crate::switch_ownership_tests::switch_test_state("cold-format-guard");
        let operation=state.cold.begin("v-new".into(),RuntimeSource::Official,RuntimeInstallMode::Portable).await.unwrap();
        let path=state.paths.root.join(COLD_STATE_FILE);
        for future_version in [true,false] {
            let mut value=serde_json::to_value(&operation).unwrap();
            if future_version {value["schema_version"]=2.into();}else{value["future"]=true.into();}
            let bytes=serde_json::to_vec(&value).unwrap();fs::write(&path,&bytes).unwrap();
            assert!(state.cold.load().is_err());assert!(state.cold.write(&operation).is_err());
            assert_eq!(fs::read(&path).unwrap(),bytes);
        }
        fs::remove_dir_all(&state.paths.root).unwrap();
    }
    #[tokio::test]
    async fn maintenance_lease_excludes_begin_and_unresolved_cold_state() {
        let state = crate::switch_ownership_tests::switch_test_state("maintenance-cold");
        let cold = &state.cold;
        let lease = cold.try_acquire_maintenance().unwrap();
        assert!(tokio::time::timeout(Duration::from_millis(25), cold.begin(
            "v-new".into(), RuntimeSource::Official, RuntimeInstallMode::Portable)).await.is_err());
        drop(lease);
        let mut operation = cold.begin("v-new".into(), RuntimeSource::Official, RuntimeInstallMode::Portable).await.unwrap();
        assert!(cold.try_acquire_maintenance().is_err());
        cold.owner_active.store(false, Ordering::Release);
        assert!(cold.try_acquire_maintenance().is_err());
        operation.phase = ColdOperationPhase::Failed;
        operation.owner_quiescent = true;
        operation.cleanup_pending = true;
        cold.write(&operation).unwrap();
        assert!(cold.try_acquire_maintenance().is_err());
        operation.cleanup_pending = false;
        cold.write(&operation).unwrap();
        fs::write(cold.intent_path(), "pending").unwrap();
        assert!(cold.try_acquire_maintenance().is_err());
        fs::remove_file(cold.intent_path()).unwrap();
        drop(cold.try_acquire_maintenance().unwrap());
        fs::write(state.paths.root.join(COLD_STATE_FILE), "invalid json").unwrap();
        assert!(cold.try_acquire_maintenance().is_err());
        fs::remove_dir_all(&state.paths.root).unwrap();
    }

    #[tokio::test]
    async fn publication_admission_allows_only_its_live_verifying_owner() {
        let state = crate::switch_ownership_tests::switch_test_state("publication-admission");
        let cold = &state.cold;
        let mut operation = cold.begin("v-new".into(), RuntimeSource::Official,
            RuntimeInstallMode::Portable).await.unwrap();
        let id = operation.operation_id.clone();
        assert!(ensure_publication_admission(&state, &id).await.is_err());
        operation.phase = ColdOperationPhase::Verifying;
        cold.write(&operation).unwrap();
        assert!(super::super::ensure_checkpoint_mutation_ready(&state).await.is_err());
        ensure_publication_admission(&state, &id).await.unwrap();
        assert!(ensure_publication_admission(&state, "another-operation").await.is_err());
        operation.cleanup_pending = true;
        cold.write(&operation).unwrap();
        assert!(ensure_publication_admission(&state, &id).await.is_err());
        operation.cleanup_pending = false;
        cold.write(&operation).unwrap();
        cold.owner_active.store(false, Ordering::Release);
        assert!(ensure_publication_admission(&state, &id).await.is_err());
        cold.owner_active.store(true, Ordering::Release);
        fs::write(cold.intent_path(), "pending").unwrap();
        let error = ensure_publication_admission(&state, &id).await.unwrap_err().to_string();
        assert!(error.contains("cold_publication_pending"), "{error}");
        fs::remove_file(cold.intent_path()).unwrap();
        ensure_publication_admission(&state, &id).await.unwrap();
        let journal = state.paths.run_dir.join("checkpoint-restore.json");
        fs::write(&journal, "broken recovery record").unwrap();
        let error = ensure_publication_admission(&state, &id).await.unwrap_err().to_string();
        assert!(error.contains("checkpoint_recovery_failed"), "{error}");
        assert_eq!(fs::read_to_string(&journal).unwrap(), "broken recovery record");
        fs::remove_file(journal).unwrap();
        let canary_dir = state.paths.root.join("canary");
        fs::create_dir_all(&canary_dir).unwrap();
        let canary_record = canary_dir.join("latest.json");
        fs::write(&canary_record, serde_json::to_vec(&serde_json::json!({
            "format_version": 1, "operation_id": "a".repeat(64), "phase": "running"
        })).unwrap()).unwrap();
        let error = ensure_publication_admission(&state, &id).await.unwrap_err().to_string();
        assert!(error.contains("canary_pending"), "{error}");
        fs::remove_file(canary_record).unwrap();
        ensure_publication_admission(&state, &id).await.unwrap();
        cold.cancellation.lock().await.as_ref().unwrap().1.cancel();
        let error = ensure_publication_admission(&state, &id).await.unwrap_err().to_string();
        assert!(error.contains("cold_install_owner_conflict"), "{error}");
        fs::remove_dir_all(&state.paths.root).unwrap();
    }

    #[tokio::test]
    async fn obsolete_history_is_pruned_without_touching_configuration_or_residue() {
        let state = crate::switch_ownership_tests::switch_test_state("obsolete-history");
        let cold = &state.cold;
        let mut operation = cold.begin("v-old".into(), RuntimeSource::Official,
            RuntimeInstallMode::Portable).await.unwrap();
        operation.phase = ColdOperationPhase::Succeeded;
        operation.owner_quiescent = true;
        operation.cleanup_pending = false;
        cold.write(&operation).unwrap();
        cold.owner_active.store(false, Ordering::Release);
        let updates = nexus_core::UpdateStateStore::new(state.paths.clone());
        updates.write(&UpdateRuntimeInfo {
            state: UpdateState::Succeeded, release_id: Some(operation.release_id.clone()),
            started_at_unix: Some(operation.started_at_unix), finished_at_unix: Some(unix_time_seconds()),
            exit_code: Some(0), error: None,
        }).unwrap();
        let config = state.config.load().unwrap();
        let residue = state.paths.releases_dir.join(&operation.release_id).join("node_modules");
        fs::create_dir_all(&residue).unwrap();
        cold.recover().unwrap();
        assert!(cold.load().unwrap().is_none());
        assert_eq!(updates.load().unwrap().state, UpdateState::Idle);
        assert_eq!(state.config.load().unwrap(), config);
        assert!(residue.exists());
        fs::remove_dir_all(&state.paths.root).unwrap();
    }

    #[tokio::test]
    async fn history_pruning_preserves_active_pending_and_installed_records() {
        let state = crate::switch_ownership_tests::switch_test_state("history-pruning-guards");
        let cold = &state.cold;
        let mut operation = cold.begin("v-new".into(), RuntimeSource::Official,
            RuntimeInstallMode::Portable).await.unwrap();
        cold.owner_active.store(false, Ordering::Release);
        assert!(!cold.prune_uninstalled_history().unwrap());
        operation.phase = ColdOperationPhase::Failed;
        operation.owner_quiescent = true;
        operation.cleanup_pending = true;
        cold.write(&operation).unwrap();
        assert!(!cold.prune_uninstalled_history().unwrap());
        operation.cleanup_pending = false;
        cold.write(&operation).unwrap();
        fs::write(cold.intent_path(), "pending").unwrap();
        assert!(!cold.prune_uninstalled_history().unwrap());
        fs::remove_file(cold.intent_path()).unwrap();
        fs::create_dir_all(&operation.candidate).unwrap();
        assert!(!cold.prune_uninstalled_history().unwrap());
        fs::remove_dir(&operation.candidate).unwrap();
        state.releases.register(&operation.release_id, "v-new", None, None).unwrap();
        assert!(!cold.prune_uninstalled_history().unwrap());
        assert!(cold.load().unwrap().is_some());
        fs::remove_dir_all(&state.paths.root).unwrap();
    }

    #[tokio::test]
    async fn clear_finished_guards_ownership_and_preserves_installed_data() {
        let state = crate::switch_ownership_tests::switch_test_state("cold-clear");
        let cold = &state.cold;
        state.releases.register("kept", "v-kept", None, None).unwrap();
        let before = serde_json::to_value(state.releases.load().unwrap()).unwrap();
        let mut operation = cold.begin("v-new".into(), RuntimeSource::Official,
            RuntimeInstallMode::Portable).await.unwrap();
        assert!(cold.clear_finished(&operation.operation_id).await.is_err());
        operation.phase = ColdOperationPhase::Failed;
        cold.write(&operation).unwrap();
        assert!(cold.clear_finished(&operation.operation_id).await.is_err());
        cold.owner_active.store(false, Ordering::Release);
        operation.cleanup_pending = true;
        cold.write(&operation).unwrap();
        assert!(cold.clear_finished(&operation.operation_id).await.is_err());
        operation.cleanup_pending = false;
        cold.write(&operation).unwrap();
        fs::write(cold.intent_path(), "pending").unwrap();
        assert!(cold.clear_finished(&operation.operation_id).await.is_err());
        fs::remove_file(cold.intent_path()).unwrap();
        assert!(cold.clear_finished("stale-operation").await.is_err());
        assert!(cold.load().unwrap().is_some());
        cold.clear_finished(&operation.operation_id).await.unwrap();
        assert!(cold.load().unwrap().is_none());
        assert_eq!(before, serde_json::to_value(state.releases.load().unwrap()).unwrap());
        fs::remove_dir_all(&state.paths.root).unwrap();
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn pnpm_receives_candidate_metadata_without_git_on_path() {
        let root = std::env::temp_dir().join(format!(
            "nexus-cold-metadata-{}",
            unix_time_nanos_for_update()
        ));
        fs::create_dir_all(&root).unwrap();
        let revision = "a66e470123456789012345678901234567890123456";
        let fixture = root.join("pnpm.cmd");
        fs::write(&fixture, format!(
            "@echo off\r\nset PATH=\r\ngit --version >nul 2>&1\r\nif not errorlevel 1 exit /b 91\r\nif not \"%DSH_CLIENT_COMMIT_HASH%\"==\"{revision}\" exit /b 92\r\necho %DSH_CLIENT_COMMIT_HASH%>metadata.txt\r\nexit /b 0\r\n"
        )).unwrap();
        let runtime = RuntimeConfig {
            pnpm: Some(nexus_core::RuntimePin {
                path: fixture,
                ownership: nexus_protocol::RuntimeOwnership::System,
            }),
            ..RuntimeConfig::default()
        };
        for phase in ["install", "build"] {
            run_pnpm(
                &runtime, [phase], &root, revision, &root.join("run"),
                &CancellationToken::default(),
            ).await.unwrap();
            assert_eq!(fs::read_to_string(root.join("metadata.txt")).unwrap().trim(), revision);
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn offline_publication_preserves_external_edit_and_unquiet_owner() {
        let state = crate::switch_ownership_tests::switch_test_state("offline-unquiet");
        let previous = state.config.load().unwrap();
        let mut target = previous.clone(); target.runtime = Some(RuntimeConfig::default());
        let op = state.cold.begin("v-offline".into(), RuntimeSource::Official, RuntimeInstallMode::Portable).await.unwrap();
        fs::create_dir_all(&op.candidate).unwrap();
        let runtime = state.paths.runtimes_dir.join(format!("offline-{}",op.operation_id));
        fs::create_dir_all(&runtime).unwrap();
        let mut intent = state.cold.prepare_publication(&op,true,target.clone()).unwrap();
        intent.owned_runtime = Some(runtime.file_name().unwrap().to_string_lossy().into_owned());
        state.cold.write_intent(&intent).unwrap();
        state.releases.register(&op.release_id, &op.tag, None, None).unwrap();
        let error = io::Error::other(ColdCommandFailure { message:"synthetic still-owned child".into(),owner_quiescent:false });
        assert!(settle_failure(&state,&op.operation_id,error).await.is_err());
        assert!(runtime.is_dir()); assert!(Path::new(&op.candidate).is_dir());
        assert!(state.releases.release_root(&op.release_id).unwrap().is_dir());
        assert!(state.cold.intent_path().is_file());
        assert!(!state.cold.load().unwrap().unwrap().owner_quiescent);
        // A finalized candidate must not overwrite configuration edited during its probe.
        let mut external = previous.clone();
        external.harness_preferences = Some(nexus_protocol::HarnessPreferencesPayload { telemetry_disabled:Some(true),..Default::default() });
        state.config.write(&external).unwrap();
        assert!(!state.config.write_if_current(&previous,&target).unwrap());
        assert_eq!(state.config.load().unwrap(),external);
        state.cold.owner_active.store(false,Ordering::Release);
        state.cold.recover_explicit(&op.operation_id,true).unwrap();
        assert_eq!(state.config.load().unwrap(),external);
        assert!(runtime.is_dir()); assert!(Path::new(&op.candidate).is_dir());
        fs::remove_dir_all(&state.paths.root).unwrap();
    }

    #[tokio::test]
    async fn offline_profile_selection_replays_with_its_configuration() {
        for committed in [false, true] {
            let state = crate::switch_ownership_tests::switch_test_state(if committed { "offline-profile-commit" } else { "offline-profile-rollback" });
            let previous = state.profiles.load().unwrap();
            let target = nexus_core::ProfileCatalog::new("imported", vec!["imported".into()]).unwrap();
            let op = state.cold.begin("v-offline".into(), RuntimeSource::Official, RuntimeInstallMode::Portable).await.unwrap();
            fs::create_dir_all(&op.candidate).unwrap();
            let mut intent = state.cold.prepare_publication(&op, true, state.config.load().unwrap()).unwrap();
            state.releases.register(&op.release_id, &op.tag, None, None).unwrap();
            intent.previous_profiles = Some(previous.clone()); intent.target_profiles = Some(target.clone()); intent.committed = committed;
            state.cold.write_intent(&intent).unwrap();
            // Simulate a cut after writing the opposite side of the tuple.
            state.profiles.write(if committed { &previous } else { &target }).unwrap();
            state.cold.recover_publication().unwrap();
            assert_eq!(state.profiles.load().unwrap(), if committed { target } else { previous });
            assert!(!state.cold.publication_pending());
        }
    }

    #[tokio::test]
    async fn repair_slot_preparation_preserves_broken_legacy_selection_and_recovers_interruption() {
        for committed in [false, true] {
            let state = crate::switch_ownership_tests::switch_test_state(if committed { "repair-prepare" } else { "repair-interrupted" });
            let pointer_bytes = br#"{"schema_version":1,"current_release":"missing-old","last_known_good":null}"#;
            fs::write(&state.paths.release_pointers_file, pointer_bytes).unwrap();
            let config = state.config.load().unwrap();
            let op = state.cold.begin("v-repair".into(), RuntimeSource::Official, RuntimeInstallMode::Portable).await.unwrap();
            let candidate = Path::new(&op.candidate);
            fs::create_dir_all(candidate).unwrap(); fs::write(candidate.join("prepared-marker"), b"built release").unwrap();
            if committed { prepare_repair_slot(&state, &op, candidate).unwrap(); }
            else {
                state.cold.prepare_publication_inner(&op, true, config.clone(), true).unwrap();
                state.releases.register_prepared(candidate, &op.release_id, &op.tag, None, None).unwrap();
                state.cold.recover_publication().unwrap();
            }
            assert_eq!(fs::read(&state.paths.release_pointers_file).unwrap(), pointer_bytes);
            assert_eq!(state.config.load().unwrap(), config);
            assert_eq!(state.releases.get(&op.release_id).is_ok(), committed);
            if committed {
                let finished = state.cold.load().unwrap().unwrap();
                assert_eq!(finished.phase, ColdOperationPhase::Prepared);
                assert!(finished.phase.is_terminal());
                assert!(finished.owner_quiescent && !finished.cleanup_pending);
                assert!(finished.warning.unwrap().contains("Version prepared only"));
                let status = state.updater.status().unwrap();
                assert_eq!(status.state, UpdateState::Prepared);
                assert_eq!(status.release_id.as_deref(), Some(op.release_id.as_str()));
                assert_eq!(serde_json::to_value(status).unwrap()["state"], "prepared");
                assert!(state.releases.promotion_risk_confirmation(&op.release_id).unwrap().is_some());
            }
            assert!(!state.cold.intent_path().exists());
            fs::remove_dir_all(&state.paths.root).unwrap();
        }
    }

    #[tokio::test]
    async fn data_only_publication_preserves_release_and_rolls_back_owned_home() {
        for committed in [false, true] {
            let state = crate::switch_ownership_tests::switch_test_state(if committed { "data-only-commit" } else { "data-only-rollback" });
            state.config.transaction(|config| {config.update_attempt_id=Some(nexus_core::agent_auth::random_hex()?);Ok(())}).unwrap();
            let previous_releases = state.releases.load().unwrap();
            let previous_config = state.config.load().unwrap();
            let mut op = state.cold.begin_with_details("data-only".into(), RuntimeSource::Official, RuntimeInstallMode::Portable, "offline_import", None, None).await.unwrap();
            op.credential_recovery_path = Some(state.paths.run_dir.join("credential-recovery.json").to_string_lossy().into_owned());
            fs::create_dir_all(&op.candidate).unwrap();
            let home = offline::environment_root(&state.paths, &op.operation_id).unwrap();
            fs::create_dir(&home).unwrap();
            fs::write(home.join("retained.txt"), "copied data").unwrap();
            let mut target = previous_config.clone();
            target.harness_preferences.get_or_insert_with(Default::default).home = Some(home.to_string_lossy().into_owned());
            let mut intent = state.cold.prepare_publication(&op, false, target.clone()).unwrap();
            assert!(intent.target_config.update_attempt_id.is_none());
            target = intent.target_config.clone();
            intent.preserve_release = true; intent.owned_environment = true; intent.committed = committed;
            state.cold.write_intent(&intent).unwrap();
            state.config.write(&target).unwrap();
            if !committed {
                // The synchronous failure path restores the exact journal
                // input, including an earlier attempt ID, before replay.
                state.config.write_recovery_document(&previous_config).unwrap();
            }
            state.cold.recover_publication().unwrap();
            assert_eq!(state.config.load().unwrap(), if committed { target } else { previous_config });
            assert_eq!(state.releases.load().unwrap().current_release, previous_releases.current_release);
            assert_eq!(home.exists(), committed);
            assert_eq!(state.cold.load().unwrap().unwrap().credential_recovery_path, op.credential_recovery_path);
            if committed { remove_owned_directory(home.parent().unwrap(), &home).unwrap(); }
            fs::remove_dir_all(&state.paths.root).unwrap();
        }
    }

    #[tokio::test]
    async fn publication_external_configuration_conflict_preserves_outer_record() {
        let state=crate::switch_ownership_tests::switch_test_state("cold-config-conflict");
        let previous=state.config.load().unwrap();
        let mut target=previous.clone(); target.runtime=Some(RuntimeConfig::default());
        let op=state.cold.begin("v-new".into(),RuntimeSource::Official,RuntimeInstallMode::Portable).await.unwrap();
        state.cold.prepare_publication(&op,true,target).unwrap();
        let mut external=previous;
        external.harness_preferences=Some(nexus_protocol::HarnessPreferencesPayload {telemetry_disabled:Some(true),..Default::default()});
        state.config.write(&external).unwrap();
        let error=state.cold.recover().unwrap_err();
        assert!(is_publication_conflict(&error));
        assert!(state.cold.intent_path().exists());
        assert_eq!(state.config.load().unwrap(),external);
        fs::remove_dir_all(&state.paths.root).unwrap();
    }

    #[tokio::test]
    async fn publication_preserve_current_replays_without_touching_files_or_pointers() {
        for cut in 0..6 {
            let state=crate::switch_ownership_tests::switch_test_state(&format!("preserve-cut-{cut}"));
            state.releases.register("existing","v-existing",None,None).unwrap();
            state.releases.promote("existing").unwrap();
            let old=state.config.load().unwrap(); let mut target=old.clone();target.runtime=Some(RuntimeConfig::default());
            let op=state.cold.begin("v-new".into(),RuntimeSource::Official,RuntimeInstallMode::Portable).await.unwrap();
            fs::create_dir_all(&op.candidate).unwrap();fs::write(Path::new(&op.candidate).join("keep"),b"candidate").unwrap();
            let mut intent=state.cold.prepare_publication(&op,true,target).unwrap();
            state.cold.owner_active.store(false, Ordering::Release); // Simulate the vanished publication owner.
            let mut current=old;current.harness_preferences=Some(nexus_protocol::HarnessPreferencesPayload{telemetry_disabled:Some(true),..Default::default()});
            if cut < 3 { current=intent.target_config.clone(); }
            state.config.write(&current).unwrap();
            let before=fs::read(&state.paths.config_file).unwrap();
            let pointers=state.releases.load().unwrap();
            let inner_previous=fs::read(state.paths.root.join("config.previous.json")).unwrap();
            nexus_core::write_private_json_atomic(&state.paths.root,&state.paths.root.join("config-write.pending.json"),
                &serde_json::json!({"schema":1,"committed":false,"rotate":true,
                    "old_current":nexus_protocol::encode_json(&intent.previous_config).unwrap(),
                    "old_previous":inner_previous,"target":nexus_protocol::encode_json(&intent.target_config).unwrap()})).unwrap();
            if cut % 3 == 0 { state.cold.recover_explicit(&op.operation_id,true).unwrap(); }
            else {
                intent.preserve_current=true;state.cold.write_intent(&intent).unwrap();
                if cut % 3 == 2 { state.cold.finish_preserved_publication(&intent).unwrap(); }
                ColdCoordinator::new(state.paths.clone()).recover().unwrap();
            }
            assert_eq!(fs::read(&state.paths.config_file).unwrap(),before);
            assert_eq!(state.releases.load().unwrap(),pointers);
            assert_eq!(fs::read(Path::new(&op.candidate).join("keep")).unwrap(),b"candidate");
            assert!(!state.cold.publication_pending());
            assert!(state.paths.root.join(format!("publication-preserved-{}.json",op.operation_id)).exists());
            state.config.transaction(|config| {config.harness_preferences=None;Ok(())}).unwrap();
            fs::remove_dir_all(&state.paths.root).unwrap();
        }
    }

    #[tokio::test]
    async fn publication_recovery_api_enforces_owners_and_operation_identity() {
        let state=crate::switch_ownership_tests::switch_test_state("publication-api");
        let mut target=state.config.load().unwrap(); target.runtime=Some(RuntimeConfig::default());
        let op=state.cold.begin("v-new".into(),RuntimeSource::Official,RuntimeInstallMode::Portable).await.unwrap();
        state.cold.prepare_publication(&op,true,target).unwrap();
            state.cold.owner_active.store(false, Ordering::Release); // Simulate the vanished publication owner.
        let request=|id:String| axum::Json(nexus_protocol::UpdateCommand {action:nexus_protocol::UpdateAction::PublicationAbandon,operation_id:Some(id),..Default::default()});
        let update=state.updater.try_acquire_gate().unwrap();
        assert_eq!(crate::update_control(axum::extract::State(state.clone()),request(op.operation_id.clone())).await.status(),axum::http::StatusCode::CONFLICT);
        drop(update);
        let snapshots=state.snapshots.try_acquire_configuration().unwrap();
        assert_eq!(crate::update_control(axum::extract::State(state.clone()),request(op.operation_id.clone())).await.status(),axum::http::StatusCode::CONFLICT);
        drop(snapshots);
        assert_ne!(crate::update_control(axum::extract::State(state.clone()),request("stale".into())).await.status(),axum::http::StatusCode::OK);
        assert!(state.cold.publication_pending());
        assert_eq!(crate::update_control(axum::extract::State(state.clone()),request(op.operation_id)).await.status(),axum::http::StatusCode::OK);
        assert!(!state.cold.publication_pending());
        assert!(crate::ensure_checkpoint_mutation_ready(&state).await.is_ok());
        fs::remove_dir_all(&state.paths.root).unwrap();
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn publication_retry_after_file_lock_is_released() {
        use std::os::windows::fs::OpenOptionsExt;
        let state=crate::switch_ownership_tests::switch_test_state("publication-lock-retry");
        let previous=state.config.load().unwrap();let mut target=previous.clone();target.runtime=Some(RuntimeConfig::default());
        let op=state.cold.begin("v-new".into(),RuntimeSource::Official,RuntimeInstallMode::Portable).await.unwrap();
        state.cold.prepare_publication(&op,true,target.clone()).unwrap();
            state.cold.owner_active.store(false, Ordering::Release); // Simulate the vanished publication owner.
        state.config.write(&target).unwrap();
        let lock=fs::OpenOptions::new().read(true).share_mode(3).open(&state.paths.config_file).unwrap();
        assert!(state.cold.recover_explicit(&op.operation_id,false).is_err());
        assert!(state.cold.publication_pending());drop(lock);
        state.cold.recover_explicit(&op.operation_id,false).unwrap();
        assert_eq!(state.config.load().unwrap(),previous);
        assert!(!state.cold.publication_pending());
        fs::remove_dir_all(&state.paths.root).unwrap();
    }

    #[tokio::test]
    async fn publication_restart_reconciles_every_durable_cut() {
        for cut in 0..10 {
            let state =
                crate::switch_ownership_tests::switch_test_state(&format!("cold-cut-{cut}"));
            let paths = state.paths.clone();
            let cold = &state.cold;
            let releases = &state.releases;
            releases.register("old", "v-old", None, None).unwrap();
            releases.promote("old").unwrap();
            let entry = paths.releases_dir.join("old/health-entry.js"); fs::write(&entry, "fixture").unwrap();
            let mut evidence = releases.healthy_launch_candidate("old", &entry, "web", "fixture-config".into()).unwrap();
            evidence.run_id = "old-run".into(); evidence.generation = 1; releases.record_healthy_release(evidence).unwrap();
            let previous = state.config.load().unwrap();
            let mut target = previous.clone();
            target.runtime = Some(RuntimeConfig::default());
            let op = cold
                .begin(
                    "v-new".into(),
                    RuntimeSource::Official,
                    RuntimeInstallMode::Portable,
                )
                .await
                .unwrap();
            fs::create_dir_all(&op.candidate).unwrap();
            let mut intent = cold.prepare_publication(&op, true, target.clone()).unwrap();
            let slot = paths.releases_dir.join(&op.release_id);
            if cut == 1 {
                fs::rename(&op.candidate, &slot).unwrap();
            }
            if cut >= 2 {
                releases
                    .register_prepared(
                        Path::new(&op.candidate),
                        &op.release_id,
                        &op.tag,
                        None,
                        None,
                    )
                    .unwrap();
            }
            if cut >= 3 {
                state.config.write(&target).unwrap();
            }
            if cut >= 4 {
                cold.update(&op.operation_id, ColdOperationPhase::Promoting, 96, None)
                    .await
                    .unwrap();
            }
            if cut >= 5 {
                releases.promote(&op.release_id).unwrap();
            }
            if cut >= 6 {
                super::super::persist_release_catalog_state(
                    &state,
                    &releases.load().unwrap(),
                    false,
                )
                .await
                .unwrap();
            }
            if cut >= 7 {
                intent.committed = true;
                cold.write_intent(&intent).unwrap();
            }
            if cut >= 8 {
                state
                    .updater
                    .state_store()
                    .write(&UpdateRuntimeInfo {
                        state: UpdateState::Succeeded,
                        release_id: Some(op.release_id.clone()),
                        started_at_unix: Some(op.started_at_unix),
                        finished_at_unix: Some(unix_time_seconds()),
                        exit_code: Some(0),
                        error: None,
                    })
                    .unwrap();
            }
            if cut >= 9 {
                cold.update(&op.operation_id, ColdOperationPhase::Succeeded, 100, None)
                    .await
                    .unwrap();
            }
            // Nested config publication cuts must obey the outer decision.
            if cut == 3 || cut == 7 {
                let before = nexus_protocol::encode_json(&previous).unwrap();
                let target_bytes = fs::read(&paths.config_file).unwrap();
                nexus_core::write_private_json_atomic(&paths.root, &paths.root.join("config-write.pending.json"),
                    &serde_json::json!({"schema":1,"committed":cut == 7,"rotate":true,
                        "old_current":before,"old_previous":null,"target":target_bytes})).unwrap();
            }
            let restarted = ColdCoordinator::new(paths.clone());
            restarted.recover().unwrap();
            // Recovery must first publish its terminal result. On a later
            // startup, rolled-back attempts without a slot become obsolete
            // history; successfully installed releases retain their record.
            let terminal = restarted.load().unwrap().unwrap();
            restarted.recover().unwrap();
            assert_eq!(restarted.load().unwrap().is_some(), cut >= 7, "cut {cut}");
            let current = releases.load().unwrap();
            super::super::persist_release_catalog_state(&state, &current, false)
                .await
                .unwrap();
            assert_eq!(state.runtime.read().await.release, current.current_release);
            if cut >= 7 {
                assert_eq!(
                    current.current_release.as_deref(),
                    Some(op.release_id.as_str()),
                    "cut {cut}"
                );
                assert_eq!(current.last_known_good.as_deref(), Some("old"));
                assert_eq!(state.config.load().unwrap(), target);
                assert_eq!(terminal.phase, ColdOperationPhase::Succeeded);
                assert_eq!(
                    state.updater.state_store().load().unwrap().state,
                    UpdateState::Succeeded
                );
            } else {
                assert_eq!(current.current_release.as_deref(), Some("old"), "cut {cut}");
                assert_eq!(current.last_known_good, None);
                assert_eq!(state.config.load().unwrap(), previous);
                assert_eq!(terminal.phase, ColdOperationPhase::Failed);
                assert!(!slot.exists(), "cut {cut} leaked a slot");
                assert_eq!(current.releases.len(), 1);
                assert_eq!(
                    state.updater.state_store().load().unwrap().state,
                    UpdateState::Idle
                );
            }
            assert!(!Path::new(&op.candidate).exists());
            assert!(!restarted.intent_path().exists());
            fs::remove_dir_all(&paths.root).unwrap();
        }
    }

    #[test]
    fn built_cli_verification_uses_package_bin_contract() {
        let root =
            std::env::temp_dir().join(format!("nexus-cold-cli-{}", unix_time_nanos_for_update()));
        fs::create_dir_all(root.join("apps/cli/lib")).unwrap();
        fs::write(
            root.join("apps/cli/package.json"),
            br#"{"bin":{"dsh":"lib/bin.js"}}"#,
        )
        .unwrap();
        fs::write(root.join("apps/cli/lib/bin.js"), b"#!/usr/bin/env node").unwrap();
        verify_built_cli(&root).unwrap();
        fs::write(
            root.join("apps/cli/package.json"),
            br#"{"bin":{"dsh":"dist/other.js"}}"#,
        )
        .unwrap();
        assert!(verify_built_cli(&root).is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn cold_cancel_and_timeout_reap_descendants_before_owner_release() {
        for cancel in [true, false] {
            let root = std::env::temp_dir()
                .join(format!("nexus-cold-tree-{}", unix_time_nanos_for_update()));
            let cold = ColdCoordinator::new(NexusPaths::from_root(root.clone()));
            let op = cold
                .begin(
                    "v-tree".into(),
                    RuntimeSource::Official,
                    RuntimeInstallMode::Portable,
                )
                .await
                .unwrap();
            fs::create_dir_all(&op.candidate).unwrap();
            let marker = root.join("marker.txt");
            let child = root.join("child.ps1");
            let parent = root.join("parent.ps1");
            fs::write(&child, format!("while ($true) {{ Add-Content -LiteralPath '{}' -Value 'live'; Start-Sleep -Milliseconds 20 }}", marker.display())).unwrap();
            fs::write(&parent, format!(r#"Start-Process -WindowStyle Hidden -FilePath "$PSHOME\powershell.exe" -ArgumentList @('-NoProfile','-File','{}')
while ($true) {{ Start-Sleep -Seconds 1 }}"#, child.display())).unwrap();
            let mut command = std::process::Command::new("powershell.exe");
            command
                .args(["-NoProfile", "-File"])
                .arg(&parent)
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            let token = cold.token(&op.operation_id).await;
            let diagnostic_dir = cold.paths.run_dir.clone();
            let runner = tokio::spawn(async move {
                run_owned_command(
                    command,
                    "cold fixture",
                    Duration::from_secs(20),
                    &diagnostic_dir,
                    &token,
                )
                .await
            });
            tokio::time::timeout(Duration::from_secs(15), async {
                while !marker.exists() {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .unwrap();
            if cancel {
                assert_eq!(
                    cold.cancel(&op.operation_id).await.unwrap().phase,
                    ColdOperationPhase::Cancelling
                );
            }
            assert!(cold
                .begin(
                    "v-other".into(),
                    RuntimeSource::Official,
                    RuntimeInstallMode::Portable
                )
                .await
                .is_err());
            let error = runner.await.unwrap().unwrap_err();
            assert_eq!(
                error.kind(),
                if cancel {
                    io::ErrorKind::Interrupted
                } else {
                    io::ErrorKind::TimedOut
                }
            );
            let length = fs::metadata(&marker).unwrap().len();
            tokio::time::sleep(Duration::from_millis(200)).await;
            assert_eq!(fs::metadata(&marker).unwrap().len(), length);
            assert!(cold
                .begin(
                    "v-other".into(),
                    RuntimeSource::Official,
                    RuntimeInstallMode::Portable
                )
                .await
                .is_err());
            remove_owned_directory(&cold.paths.downloads_dir, Path::new(&op.candidate)).unwrap();
            cold.update(
                &op.operation_id,
                if cancel {
                    ColdOperationPhase::Cancelled
                } else {
                    ColdOperationPhase::Failed
                },
                100,
                Some(error.to_string()),
            )
            .await
            .unwrap();
            assert!(cold
                .begin(
                    "v-other".into(),
                    RuntimeSource::Official,
                    RuntimeInstallMode::Portable
                )
                .await
                .is_err());
            cold.owner_active.store(false, Ordering::Release);
            assert!(!Path::new(&op.candidate).exists());
            cold.begin(
                "v-other".into(),
                RuntimeSource::Official,
                RuntimeInstallMode::Portable,
            )
            .await
            .unwrap();
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[cfg(windows)]
    #[test]
    fn candidate_cleanup_unlinks_reparse_points_and_clears_readonly_files() {
        use std::os::windows::fs::{symlink_dir, symlink_file};

        let root = std::env::temp_dir().join(format!(
            "nexus-cold-cleanup-links-{}",
            unix_time_nanos_for_update()
        ));
        let paths = NexusPaths::from_root(root.clone());
        paths.ensure_directories().unwrap();
        let candidate = paths.downloads_dir.join(".candidate");
        let outside = root.join("outside");
        fs::create_dir_all(candidate.join("inside-dir")).unwrap();
        fs::create_dir_all(outside.join("outside-dir")).unwrap();
        fs::write(candidate.join("inside.txt"), b"inside").unwrap();
        fs::write(outside.join("outside.txt"), b"outside").unwrap();
        symlink_dir(
            candidate.join("inside-dir"),
            candidate.join("inside-dir-link"),
        )
        .unwrap();
        symlink_dir(
            outside.join("outside-dir"),
            candidate.join("outside-dir-link"),
        )
        .unwrap();
        symlink_file(
            candidate.join("inside.txt"),
            candidate.join("inside-file-link"),
        )
        .unwrap();
        symlink_file(
            outside.join("outside.txt"),
            candidate.join("outside-file-link"),
        )
        .unwrap();
        let readonly = candidate.join("readonly.txt");
        fs::write(&readonly, b"readonly checkout file").unwrap();
        let mut permissions = fs::metadata(&readonly).unwrap().permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&readonly, permissions).unwrap();

        remove_owned_directory(&paths.downloads_dir, &candidate).unwrap();

        assert!(!candidate.exists());
        assert!(outside.join("outside-dir").is_dir());
        assert_eq!(fs::read(outside.join("outside.txt")).unwrap(), b"outside");
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn failing_command_captures_bounded_redacted_stderr() {
        let root = std::env::temp_dir().join(format!(
            "nexus-cold-command-stderr-{}",
            unix_time_nanos_for_update()
        ));
        let run_dir = root.join("run");
        let mut command = std::process::Command::new("cmd.exe");
        command.args([
            "/D",
            "/S",
            "/C",
            "(for /L %i in (1,1,7000) do @echo padding-padding-padding 1>&2) & (echo visible-stderr-sentinel 1>&2) & (echo Authorization: Bearer TOPSECRET 1>&2) & exit /b 7",
        ]);
        let error = run_owned_command(
            command,
            "synthetic-command",
            Duration::from_secs(10),
            &run_dir,
            &CancellationToken::default(),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("synthetic-command exited"));
        assert!(error.contains("tail truncated"));
        assert!(error.contains("visible-stderr-sentinel"));
        assert!(error.contains("[REDACTED]"));
        assert!(!error.contains("TOPSECRET"));
        assert!(error.len() <= COMMAND_DIAGNOSTIC_BYTES + 256);
        assert_eq!(fs::read_dir(&run_dir).unwrap().count(), 0);
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn dual_diagnostics_are_bounded_redacted_and_preserve_caller_stdout() {
        let root = std::env::temp_dir().join(format!(
            "nexus-dual-output-{}",
            unix_time_nanos_for_update()
        ));
        fs::create_dir_all(&root).unwrap();
        let mut command = std::process::Command::new("cmd.exe");
        command.args(["/D", "/C", "(for /L %i in (1,1,4000) do @echo padding-padding-padding) & (echo stdout-failure) & (echo Authorization: Bearer SECRETSTDOUT) & (echo stderr-failure 1>&2) & exit /b 7"]);
        let error = run_owned_command_diagnostics(
            command,
            "pnpm",
            Duration::from_secs(10),
            &root,
            &CancellationToken::default(),
            true,
        )
        .await
        .unwrap_err();
        assert!(command_owner_quiescent(&error));
        let error = error.to_string();
        assert!(error.contains("stdout-failure") && error.contains("stderr-failure"));
        assert!(!error.contains("SECRETSTDOUT"));
        assert!(error.contains("[REDACTED]") && error.contains("tail truncated"));
        assert!(error.len() <= COMMAND_DIAGNOSTIC_BYTES + 256);
        assert_eq!(fs::read_dir(&root).unwrap().count(), 0);

        let output = root.join("caller-output");
        let mut command = std::process::Command::new("cmd.exe");
        command
            .args(["/D", "/C", "echo machine-readable-output"])
            .stdout(fs::File::create(&output).unwrap());
        run_owned_command(
            command,
            "git-fixture",
            Duration::from_secs(5),
            &root,
            &CancellationToken::default(),
        )
        .await
        .unwrap();
        assert!(fs::read_to_string(&output)
            .unwrap()
            .contains("machine-readable-output"));
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn cleanup_error_is_secondary_and_restart_recovers_pending_candidate() {
        let state = crate::switch_ownership_tests::switch_test_state("cold-cleanup-secondary");
        let original = state
            .cold
            .begin(
                "v-cleanup".into(),
                RuntimeSource::Official,
                RuntimeInstallMode::Portable,
            )
            .await
            .unwrap();
        let outside = state.paths.root.join("outside-candidate");
        fs::create_dir_all(&outside).unwrap();
        let mut operation = original.clone();
        operation.phase = ColdOperationPhase::Cloning;
        operation.candidate = outside.to_string_lossy().into_owned();
        state.cold.write(&operation).unwrap();

        settle_failure(
            &state,
            &operation.operation_id,
            io::Error::other("synthetic primary failure"),
        )
        .await
        .unwrap();
        let failed = state.cold.load().unwrap().unwrap();
        assert_eq!(failed.phase, ColdOperationPhase::Failed);
        assert_eq!(failed.error.as_deref(), Some("synthetic primary failure"));
        assert!(failed.owner_quiescent);
        assert!(failed.cleanup_pending);
        assert!(failed
            .cleanup_error
            .as_deref()
            .unwrap()
            .contains("cleanup target escapes owned root"));
        let retried = state.cold.cancel(&operation.operation_id).await.unwrap();
        assert!(retried.cleanup_pending);
        assert!(state
            .cold
            .begin(
                "v-blocked".into(),
                RuntimeSource::Official,
                RuntimeInstallMode::Portable,
            )
            .await
            .is_err());

        fs::create_dir_all(&original.candidate).unwrap();
        let readonly = Path::new(&original.candidate).join("readonly.txt");
        fs::write(&readonly, b"readonly").unwrap();
        #[cfg(windows)]
        {
            let mut permissions = fs::metadata(&readonly).unwrap().permissions();
            permissions.set_readonly(true);
            fs::set_permissions(&readonly, permissions).unwrap();
        }
        let mut repaired = state.cold.load().unwrap().unwrap();
        repaired.candidate = original.candidate.clone();
        state.cold.write(&repaired).unwrap();
        state.cold.owner_active.store(false, Ordering::Release);
        let restarted = ColdCoordinator::new(state.paths.clone());
        restarted.recover().unwrap();
        let recovered = restarted.load().unwrap().unwrap();
        assert_eq!(
            recovered.error.as_deref(),
            Some("synthetic primary failure")
        );
        assert!(!recovered.cleanup_pending);
        assert!(recovered.cleanup_error.is_none());
        assert!(!Path::new(&original.candidate).exists());
        restarted
            .begin(
                "v-next".into(),
                RuntimeSource::Official,
                RuntimeInstallMode::Portable,
            )
            .await
            .unwrap();
        fs::remove_dir_all(&state.paths.root).unwrap();
    }

    #[tokio::test]
    async fn busy_reconciliation_gate_still_persists_primary_failure() {
        let state = crate::switch_ownership_tests::switch_test_state("cold-busy-reconcile");
        let operation = state
            .cold
            .begin(
                "v-busy".into(),
                RuntimeSource::Official,
                RuntimeInstallMode::Portable,
            )
            .await
            .unwrap();
        let updater = state.updater.try_acquire_gate().unwrap();
        let result = settle_failure(
            &state,
            &operation.operation_id,
            io::Error::other("primary survives busy gate"),
        )
        .await;
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::ResourceBusy);
        let failed = state.cold.load().unwrap().unwrap();
        assert_eq!(failed.phase, ColdOperationPhase::Failed);
        assert_eq!(failed.error.as_deref(), Some("primary survives busy gate"));
        assert!(failed.cleanup_pending);
        assert!(failed
            .cleanup_error
            .as_deref()
            .unwrap()
            .contains("failure reconciliation is pending"));
        drop(updater);
        state.cold.owner_active.store(false, Ordering::Release);
        let retried = state.cold.cancel(&operation.operation_id).await.unwrap();
        assert!(!retried.cleanup_pending);
        fs::remove_dir_all(&state.paths.root).unwrap();
    }

    #[tokio::test]
    async fn duplicate_begin_and_unknown_cancel_are_rejected() {
        let root =
            std::env::temp_dir().join(format!("nexus-cold-state-{}", unix_time_nanos_for_update()));
        let coordinator = ColdCoordinator::new(NexusPaths::from_root(root.clone()));
        let operation = coordinator
            .begin(
                "v1.2.3".to_owned(),
                RuntimeSource::Official,
                RuntimeInstallMode::Portable,
            )
            .await
            .unwrap();
        assert_eq!(
            coordinator
                .begin(
                    "v1.2.4".to_owned(),
                    RuntimeSource::Official,
                    RuntimeInstallMode::Portable,
                )
                .await
                .expect_err("a second candidate cannot be created")
                .kind(),
            io::ErrorKind::ResourceBusy
        );
        assert!(coordinator.cancel("stale").await.is_err());
        let cancelled = coordinator.cancel(&operation.operation_id).await.unwrap();
        assert_eq!(cancelled.phase, ColdOperationPhase::Cancelling);
        let _ = fs::remove_dir_all(root);
    }
}


/// Stops the output-tail watcher on every exit path of the install/build
/// phase, including the `?` error returns, so the watcher never outlives the
/// operation it serves.
/// Aborts the output-tail watcher on every exit path of the install/build
/// phase, including the `?` error returns, so the watcher never outlives the
/// operation it serves.
struct TailWatcherStop {
    handle: tokio::task::JoinHandle<()>,
}

impl TailWatcherStop {
    fn spawn(state: AppState, operation_id: String) -> Self {
        Self {
            handle: tokio::spawn(tail_command_output(state, operation_id)),
        }
    }
}

impl Drop for TailWatcherStop {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// Refresh a bounded command-output tail into the persisted operation while
/// install and build run, so the UI shows live pnpm output between phase
/// transitions. Stops when `stop` flips, the operation changes, or the
/// operation reaches a terminal phase.
async fn tail_command_output(state: AppState, operation_id: String) {
    let mut ticker = tokio::time::interval(Duration::from_secs(2));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        let Ok(Some(operation)) = state.cold.load() else {
            return;
        };
        if operation.operation_id != operation_id || operation.phase.is_terminal() {
            return;
        }
        let run_dir = state.paths.run_dir.clone();
        let tail = match tokio::task::spawn_blocking(move || newest_command_tail(&run_dir)).await {
            Ok(Some(tail)) => Some(tail),
            _ => None,
        };
        let Some(tail) = tail else {
            continue;
        };
        let _gate = state.cold.gate.lock().await;
        let Ok(Some(mut operation)) = state.cold.load() else {
            return;
        };
        if operation.operation_id != operation_id || operation.phase.is_terminal() {
            return;
        }
        operation.output_tail = Some(tail);
        operation.updated_at_unix = Some(unix_time_seconds());
        let _ = state.cold.write(&operation);
    }
}

/// Read the tail of the most recently written command diagnostic, stripped
/// of control noise, ready for display. The read is bounded by seeking near
/// the end of the file: a noisy build can write hundreds of megabytes, and
/// this must never buffer the whole thing.
fn newest_command_tail(run_dir: &Path) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    const TAIL_BYTES: u64 = 4096;
    let newest = std::fs::read_dir(run_dir)
        .ok()?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.file_name()
                .map(|name| {
                    let name = name.to_string_lossy();
                    name.starts_with("cold-command-") && name.ends_with(".stderr.tmp")
                })
                .unwrap_or(false)
        })
        .filter_map(|path| {
            let modified = std::fs::metadata(&path).ok()?.modified().ok()?;
            Some((path, modified))
        })
        .max_by_key(|(_, modified)| *modified)
        .map(|(path, _)| path)?;
    let mut file = std::fs::File::open(&newest).ok()?;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(TAIL_BYTES);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::with_capacity((len - start).min(TAIL_BYTES) as usize);
    file.take(TAIL_BYTES).read_to_end(&mut bytes).ok()?;
    // Redact secret-bearing lines exactly like the completed diagnostics,
    // then keep readable text only and bound the persisted size.
    let (redacted, _) = nexus_core::redact_diagnostics_payload(&bytes);
    let cleaned: String = String::from_utf8_lossy(&redacted)
        .chars()
        .filter(|ch| !ch.is_control())
        .collect();
    let start = cleaned.len().saturating_sub(2000);
    Some(cleaned[start..].to_owned())
}
