//! Persisted cold-install orchestration for upstream Harness tags.

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

#[derive(serde::Serialize, serde::Deserialize)]
struct PublicationIntent {
    committed: bool,
    operation: ColdOperation,
    previous_config: nexus_core::NexusConfigFile,
    target_config: nexus_core::NexusConfigFile,
    previous_current: Option<String>,
    previous_lkg: Option<String>,
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

    pub(crate) fn recover(&self) -> io::Result<()> {
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
            operation.error = Some(
                "previous cold-install owner was not attached; start the tag switch again"
                    .to_owned(),
            );
            operation.owner_quiescent = true;
            operation.cleanup_pending = true;
            operation.cleanup_error = None;
            self.write(&operation)?;
        }
        if operation.cleanup_pending {
            // A previous in-memory owner cannot survive Agent restart. Retain
            // the primary terminal result and retry only the owned residue.
            operation.owner_quiescent = true;
            match remove_owned_directory(&self.paths.downloads_dir, Path::new(&operation.candidate))
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

    fn intent_path(&self) -> PathBuf {
        self.paths.root.join(PUBLICATION_FILE)
    }

    pub(crate) fn publication_pending(&self) -> bool {
        self.intent_path().exists()
    }

    pub(crate) fn cleanup_pending(&self) -> io::Result<bool> {
        Ok(self
            .load()?
            .is_some_and(|operation| operation.cleanup_pending))
    }

    fn write_intent(&self, intent: &PublicationIntent) -> io::Result<()> {
        write_json_atomic(&self.paths.root, &self.intent_path(), intent)
    }

    fn prepare_publication(
        &self,
        operation: &ColdOperation,
        new_slot: bool,
        target_config: nexus_core::NexusConfigFile,
    ) -> io::Result<PublicationIntent> {
        let catalog = nexus_core::ReleaseStore::new(self.paths.clone()).load()?;
        if new_slot && self.paths.releases_dir.join(&operation.release_id).exists() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "cold target slot already exists",
            ));
        }
        let intent = PublicationIntent {
            committed: false,
            target_config,
            operation: operation.clone(),
            previous_config: nexus_core::ConfigStore::new(self.paths.clone()).load()?,
            previous_current: catalog.current_release,
            previous_lkg: catalog.last_known_good,
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
        let intent: PublicationIntent =
            serde_json::from_slice(&fs::read(self.intent_path())?).map_err(io::Error::other)?;
        let releases = nexus_core::ReleaseStore::new(self.paths.clone());
        let updater = nexus_core::UpdateStateStore::new(self.paths.clone());
        let mut operation = intent.operation;
        if intent.committed {
            nexus_core::ConfigStore::new(self.paths.clone()).write(&intent.target_config)?;
            let lkg = if intent.previous_current.as_deref() == Some(&operation.release_id) {
                intent.previous_lkg.as_deref()
            } else {
                intent
                    .previous_current
                    .as_deref()
                    .or(intent.previous_lkg.as_deref())
            };
            releases.restore_release_pointers(Some(&operation.release_id), lkg)?;
            updater.write(&UpdateRuntimeInfo {
                state: UpdateState::Succeeded,
                release_id: Some(operation.release_id.clone()),
                started_at_unix: Some(operation.started_at_unix),
                finished_at_unix: Some(unix_time_seconds()),
                exit_code: Some(0),
                error: None,
            })?;
            operation.phase = ColdOperationPhase::Succeeded;
            operation.error = None;
            operation.owner_quiescent = true;
            operation.cleanup_pending = false;
            operation.cleanup_error = None;
        } else {
            nexus_core::ConfigStore::new(self.paths.clone()).write(&intent.previous_config)?;
            releases.restore_release_pointers(
                intent.previous_current.as_deref(),
                intent.previous_lkg.as_deref(),
            )?;
            updater.write(&intent.previous_update)?;
            if intent.new_slot {
                // Includes rename-before-manifest cuts; slot id is server generated.
                nexus_core::validate_release_id(&operation.release_id)?;
                remove_owned_directory(
                    &self.paths.releases_dir,
                    &self.paths.releases_dir.join(&operation.release_id),
                )?;
            }
            operation.phase = ColdOperationPhase::Failed;
            operation.error =
                Some("publication interrupted before commit; previous selection restored".into());
            operation.owner_quiescent = true;
            operation.cleanup_pending = false;
            operation.cleanup_error = None;
        }
        remove_owned_directory(&self.paths.downloads_dir, Path::new(&operation.candidate))?;
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
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
    }

    fn write(&self, operation: &ColdOperation) -> io::Result<()> {
        self.paths.ensure_directories()?;
        let path = self.paths.root.join(COLD_STATE_FILE);
        write_json_atomic(&self.paths.root, &path, operation)
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

    pub(crate) async fn begin(
        &self,
        tag: String,
        source: RuntimeSource,
        mode: RuntimeInstallMode,
    ) -> io::Result<ColdOperation> {
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
            operation_id: operation_id.clone(),
            phase: ColdOperationPhase::Queued,
            tag,
            source,
            mode,
            release_id,
            candidate: candidate.to_string_lossy().into_owned(),
            candidate_revision: None,
            progress_percent: 0,
            started_at_unix: now,
            updated_at_unix: Some(now),
            foundation_plan_id: None,
            warning: None,
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
                match remove_owned_directory(
                    &self.paths.downloads_dir,
                    Path::new(&operation.candidate),
                ) {
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
            match remove_owned_directory(&self.paths.downloads_dir, Path::new(&operation.candidate))
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
    if let Err(error) = prepare_inner(&state, &operation_id).await {
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
    state
        .cold
        .update(operation_id, ColdOperationPhase::Installing, 55, None)
        .await?;
    run_pnpm(
        &runtime,
        ["install", "--frozen-lockfile"],
        &candidate,
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
    super::ensure_checkpoint_mutation_ready(state)
        .await
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::ResourceBusy,
                "checkpoint recovery blocks cold install",
            )
        })?;
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
    if final_operation.operation_id != operation_id || final_operation.phase.is_terminal() {
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
        state.snapshots.configured_dsh_home()?,
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
    let promoted = match state.releases.promote(&operation.release_id) {
        Ok(catalog) => catalog,
        Err(error) => {
            let _ = state.config.write(&previous_config);
            let _ = state.releases.remove(&operation.release_id);
            return Err(error);
        }
    };
    if let Err(error) = super::persist_release_catalog_state(state, &promoted, false).await {
        let _ = state.releases.restore_release_pointers(
            before.current_release.as_deref(),
            before.last_known_good.as_deref(),
        );
        let _ = state.config.write(&previous_config);
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
    let mut intent =
        state
            .cold
            .prepare_publication(&final_operation, false, state.config.load()?)?;
    let before = state.releases.load()?;
    let catalog = state.releases.promote(&release.id)?;
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
            ownership: if tool.source.as_deref() == Some("nexus") {
                RuntimeOwnership::Nexus
            } else {
                RuntimeOwnership::System
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
    runtime.validate_for_paths_dummy()?;
    Ok(runtime)
}

async fn run_pnpm(
    runtime: &RuntimeConfig,
    args: impl IntoIterator<Item = &'static str>,
    cwd: &Path,
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
    run_command(
        "pnpm",
        &command.program,
        command.prefix_args.into_iter().chain(args),
        Some(cwd),
        runtime,
        diagnostic_dir,
        cancellation,
    )
    .await
}

async fn run_command(
    phase: &str,
    program: &Path,
    args: impl IntoIterator<Item = OsString>,
    cwd: Option<&Path>,
    runtime: &RuntimeConfig,
    diagnostic_dir: &Path,
    cancellation: &CancellationToken,
) -> io::Result<()> {
    ensure_not_cancelled(cancellation)?;
    let mut command = std::process::Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null());
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    for (key, value) in build_runtime_child_env(runtime, std::env::var_os("PATH").as_deref())? {
        command.env(key, value);
    }
    run_owned_command_diagnostics(
        command,
        phase,
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
        if operation.phase == ColdOperationPhase::Succeeded {
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
        match remove_owned_directory(&state.paths.downloads_dir, Path::new(&operation.candidate)) {
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

trait RuntimeConfigValidationExt {
    fn validate_for_paths_dummy(&self) -> io::Result<()>;
}
impl RuntimeConfigValidationExt for RuntimeConfig {
    fn validate_for_paths_dummy(&self) -> io::Result<()> {
        self.validate()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            let restarted = ColdCoordinator::new(paths.clone());
            restarted.recover().unwrap();
            restarted.recover().unwrap();
            let current = releases.load().unwrap();
            super::super::persist_release_catalog_state(&state, &current, false)
                .await
                .unwrap();
            assert_eq!(state.runtime.read().await.release, current.current_release);
            let terminal = restarted.load().unwrap().unwrap();
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
                    Duration::from_secs(5),
                    &diagnostic_dir,
                    &token,
                )
                .await
            });
            tokio::time::timeout(Duration::from_secs(4), async {
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
