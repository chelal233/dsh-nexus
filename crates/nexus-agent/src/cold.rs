//! Persisted cold-install orchestration for upstream Harness tags.

use std::{
    ffi::OsString,
    fs, io,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use nexus_core::{
    build_pnpm_args, build_runtime_child_env, resolve_runtime_command, unix_time_nanos_for_update,
    unix_time_seconds, validate_update_ref, write_json_atomic, HarnessLaunchSpec, NexusPaths,
    RuntimeConfig, RuntimePin,
};
use nexus_protocol::{
    ColdOperation, ColdOperationPhase, HarnessLaunchMode, RuntimeInstallMode, RuntimeOwnership,
    RuntimePlanActionKind, RuntimePlanRequest, RuntimeSource, UpdateRuntimeInfo, UpdateState,
};
use nexus_runtime_supply::{
    CancellationToken, CommandProcessRunner, HostPlatform, HttpDownloadClient, RuntimeSupplier,
    RuntimeSupplyPlanner, SupplyPlan,
};
use tokio::{
    process::Command,
    sync::Mutex,
    time::{sleep, timeout},
};

use crate::AppState;

const COLD_STATE_FILE: &str = "cold-operation.json";
const APPROVED_UPSTREAM: &str = "https://github.com/deepseek-ai/deepseek-harness";
const COMMAND_TIMEOUT: Duration = Duration::from_secs(900);

#[derive(Clone)]
pub(crate) struct ColdCoordinator {
    paths: NexusPaths,
    cancellation: Arc<Mutex<Option<(String, CancellationToken)>>>,
    gate: Arc<Mutex<()>>,
}

impl ColdCoordinator {
    pub(crate) fn new(paths: NexusPaths) -> Self {
        Self {
            paths,
            cancellation: Arc::new(Mutex::new(None)),
            gate: Arc::new(Mutex::new(())),
        }
    }

    pub(crate) fn approved_upstream() -> &'static str {
        APPROVED_UPSTREAM
    }

    pub(crate) fn recover(&self) -> io::Result<()> {
        let Some(mut operation) = self.load()? else {
            return Ok(());
        };
        if !operation.phase.is_terminal()
            && operation.phase != ColdOperationPhase::AwaitingConfirmation
        {
            operation.phase = ColdOperationPhase::Failed;
            operation.updated_at_unix = Some(unix_time_seconds());
            operation.error = Some(
                "previous cold-install owner was not attached; start the tag switch again"
                    .to_owned(),
            );
            self.write(&operation)?;
        }
        Ok(())
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
        if operation.phase.is_terminal() && operation.phase != phase {
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
        let _gate = self.gate.lock().await;
        if let Some(current) = self.load()? {
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
            supply_plan: None,
            confirmation: None,
            error: None,
        };
        self.write(&operation)?;
        let token = CancellationToken::default();
        *self.cancellation.lock().await = Some((operation_id, token));
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
        let operation = self
            .load()?
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "cold operation not found"))?;
        if operation.operation_id != operation_id {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "stale cold operation id",
            ));
        }
        if operation.phase.is_terminal() {
            return Ok(operation);
        }
        if let Some((id, token)) = self.cancellation.lock().await.as_ref() {
            if id == operation_id {
                token.cancel();
            }
        }
        self.update(
            operation_id,
            ColdOperationPhase::Cancelled,
            operation.progress_percent,
            None,
        )
        .await
    }

    pub(crate) async fn claim_confirmation(
        &self,
        operation_id: &str,
        confirmation: &str,
    ) -> io::Result<ColdOperation> {
        let _gate = self.gate.lock().await;
        let mut operation = self
            .load()?
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "cold operation not found"))?;
        if operation.operation_id != operation_id
            || operation.phase != ColdOperationPhase::AwaitingConfirmation
            || operation.confirmation.as_deref() != Some(confirmation)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "stale or mismatched cold-install confirmation",
            ));
        }
        operation.phase = ColdOperationPhase::Supplying;
        operation.progress_percent = 40;
        operation.updated_at_unix = Some(unix_time_seconds());
        self.write(&operation)?;
        Ok(operation)
    }

    async fn fail(&self, operation_id: &str, error: impl ToString) {
        if self.load().ok().flatten().is_some_and(|operation| {
            operation.operation_id == operation_id
                && operation.phase == ColdOperationPhase::Cancelled
        }) {
            return;
        }
        let message = error.to_string();
        let _ = self
            .update(operation_id, ColdOperationPhase::Failed, 100, Some(message))
            .await;
    }
}

pub(crate) async fn prepare(state: AppState, operation_id: String) {
    if let Err(error) = prepare_inner(&state, &operation_id).await {
        cleanup_candidate(&state, &operation_id);
        state.cold.fail(&operation_id, error).await;
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
    let git = resolve_runtime_command(&runtime, "git")?.ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Git is required for cold install; install Git or configure an absolute Git runtime pin"))?;
    let args = [
        "clone",
        "--no-tags",
        "--depth",
        "1",
        "--branch",
        &operation.tag,
        APPROVED_UPSTREAM,
        &operation.candidate,
    ];
    run_command(
        "git-clone",
        &git.program,
        git.prefix_args
            .into_iter()
            .chain(args.into_iter().map(OsString::from)),
        None,
        &runtime,
        &cancellation,
    )
    .await?;
    let revision = candidate_revision(&runtime, &candidate, &cancellation).await?;
    record_candidate_revision(state, operation_id, &revision).await?;
    ensure_not_cancelled(&cancellation)?;
    state
        .cold
        .update(operation_id, ColdOperationPhase::Planning, 25, None)
        .await?;
    let plan = plan_candidate(state, &operation, &candidate).await?;
    if plan
        .suggested_actions
        .iter()
        .any(|action| action.action == RuntimePlanActionKind::ConfigureExternal)
    {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "Git became unavailable; Nexus does not install Git automatically",
        ));
    }
    if plan.suggested_actions.iter().all(|action| {
        matches!(
            action.action,
            RuntimePlanActionKind::UsePinned | RuntimePlanActionKind::UseExisting
        )
    }) {
        record_foundation_plan(state, operation_id, &plan.plan_id).await?;
        let runtime = runtime_from_plan(&plan)?;
        return build_and_publish(state, operation_id, runtime).await;
    }
    let downloader = HttpDownloadClient::new().map_err(supply_error)?;
    let planner = RuntimeSupplyPlanner::new(
        &downloader,
        operation.source,
        HostPlatform::current_windows().map_err(supply_error)?,
        state.paths.runtimes_dir.clone(),
        corepack_root(),
    )
    .map_err(supply_error)?;
    let supply = planner
        .plan(&plan, &cancellation)
        .await
        .map_err(supply_error)?;
    let _gate = state.cold.gate.lock().await;
    let mut current = state
        .cold
        .load()?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "cold operation not found"))?;
    if current.operation_id != operation_id {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "cold operation changed",
        ));
    }
    current.phase = ColdOperationPhase::AwaitingConfirmation;
    current.progress_percent = 35;
    current.updated_at_unix = Some(unix_time_seconds());
    current.foundation_plan_id = Some(plan.plan_id);
    current.confirmation = Some(supply.supply_plan_id.clone());
    current.supply_plan = Some(serde_json::to_value(supply).map_err(io::Error::other)?);
    state.cold.write(&current)
}

pub(crate) async fn confirm(state: AppState, operation_id: String, confirmation: String) {
    if let Err(error) = confirm_inner(&state, &operation_id, &confirmation).await {
        cleanup_candidate(&state, &operation_id);
        state.cold.fail(&operation_id, error).await;
    }
}

async fn confirm_inner(state: &AppState, operation_id: &str, confirmation: &str) -> io::Result<()> {
    let operation = state
        .cold
        .load()?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "cold operation not found"))?;
    if operation.operation_id != operation_id
        || operation.phase != ColdOperationPhase::Supplying
        || operation.confirmation.as_deref() != Some(confirmation)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "stale or mismatched cold-install confirmation",
        ));
    }
    let candidate = PathBuf::from(&operation.candidate);
    let runtime = resolved_runtime_config(state).await?;
    let revision = candidate_revision(&runtime, &candidate, &state.cold.token(operation_id).await).await?;
    if operation.candidate_revision.as_deref() != Some(&revision) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "cold candidate revision changed while awaiting confirmation"));
    }
    let fresh = plan_candidate(state, &operation, &candidate).await?;
    if operation.foundation_plan_id.as_deref() != Some(&fresh.plan_id) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "runtime plan or configuration changed while awaiting confirmation",
        ));
    }
    let supply: SupplyPlan =
        serde_json::from_value(operation.supply_plan.clone().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "server-owned supply plan is missing",
            )
        })?)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let cancellation = state.cold.token(operation_id).await;
    let downloader = HttpDownloadClient::new().map_err(supply_error)?;
    let supplier = RuntimeSupplier::new(
        &downloader,
        &CommandProcessRunner,
        HostPlatform::current_windows().map_err(supply_error)?,
        state.paths.runtimes_dir.clone(),
        corepack_root(),
    )
    .map_err(supply_error)?;
    let outcome = supplier
        .execute_confirmed(&fresh, &supply, confirmation, &cancellation)
        .await
        .map_err(supply_error)?;
    build_and_publish(state, operation_id, outcome.runtime).await
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
        &cancellation,
    )
    .await?;
    state
        .cold
        .update(operation_id, ColdOperationPhase::Building, 72, None)
        .await?;
    run_pnpm(&runtime, ["build"], &candidate, &cancellation).await?;
    state
        .cold
        .update(operation_id, ColdOperationPhase::Verifying, 85, None)
        .await?;
    verify_built_cli(&candidate)?;
    ensure_not_cancelled(&cancellation)?;
    let revision = candidate_revision(&runtime, &candidate, &cancellation).await?;
    if operation.candidate_revision.as_deref() != Some(&revision) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "cold candidate revision changed before publication"));
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
    let mut final_operation = state.cold.load()?.ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "cold operation not found")
    })?;
    if final_operation.operation_id != operation_id || final_operation.phase.is_terminal() {
        return Err(io::Error::new(io::ErrorKind::Interrupted, "cold operation cannot be committed"));
    }
    final_operation.phase = ColdOperationPhase::Registering;
    final_operation.progress_percent = 90;
    final_operation.updated_at_unix = Some(unix_time_seconds());
    state.cold.write(&final_operation)?;
    let catalog = state.releases.register_prepared(
        &candidate,
        &operation.release_id,
        &operation.tag,
        Some(APPROVED_UPSTREAM.to_owned()),
        Some("Nexus cold install".to_owned()),
    )?;
    let release_root = state.releases.release_root(&operation.release_id)?;
    let node = runtime
        .node
        .as_ref()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "verified Node pin is missing"))?
        .path
        .clone();
    let entry = release_root.join("apps/cli/lib/bin.js");
    let harness = HarnessLaunchSpec {
        mode: HarnessLaunchMode::Node,
        program: node,
        args: vec![
            entry.to_string_lossy().into_owned(),
            "--profile".to_owned(),
            "{profile}".to_owned(),
        ],
        working_dir: Some(release_root),
        readiness_url: None,
        readiness_timeout_secs: None,
        readiness_token_required: false,
    };
    let previous_config = state.config.load()?;
    if let Err(error) = state.config.transaction(|config| {
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
        Ok(())
    }) {
        let _ = state.releases.remove(&operation.release_id);
        return Err(error);
    }
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
    state.updater.state_store().write(&finished)?;
    final_operation.phase = ColdOperationPhase::Succeeded;
    final_operation.progress_percent = 100;
    final_operation.updated_at_unix = Some(unix_time_seconds());
    state.cold.write(&final_operation)?;
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
    let mut final_operation = state.cold.load()?.ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "cold operation not found")
    })?;
    if final_operation.operation_id != operation_id || final_operation.phase.is_terminal() {
        return Err(io::Error::new(io::ErrorKind::Interrupted, "cold operation cannot be committed"));
    }
    let release = state
        .releases
        .load()?
        .releases
        .into_iter()
        .filter(|item| item.version == tag)
        .max_by_key(|item| item.installed_at_unix)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "installed tag disappeared"))?;
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
    final_operation.phase = ColdOperationPhase::Succeeded;
    final_operation.progress_percent = 100;
    final_operation.updated_at_unix = Some(unix_time_seconds());
    state.cold.write(&final_operation)?;
    Ok(())
}

async fn record_foundation_plan(
    state: &AppState,
    operation_id: &str,
    plan_id: &str,
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
    operation.updated_at_unix = Some(unix_time_seconds());
    state.cold.write(&operation)
}

async fn record_candidate_revision(
    state: &AppState,
    operation_id: &str,
    revision: &str,
) -> io::Result<()> {
    let _gate = state.cold.gate.lock().await;
    let mut operation = state.cold.load()?.ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "cold operation not found")
    })?;
    if operation.operation_id != operation_id {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "cold operation changed"));
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

async fn resolved_runtime_config(state: &AppState) -> io::Result<RuntimeConfig> {
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

fn runtime_from_plan(plan: &nexus_protocol::RuntimePlanResponse) -> io::Result<RuntimeConfig> {
    let mut runtime = RuntimeConfig {
        source: plan.source,
        mode: plan.mode,
        ..RuntimeConfig::default()
    };
    for tool in &plan.tools {
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
    cancellation: &CancellationToken,
) -> io::Result<()> {
    let command = resolve_runtime_command(runtime, "pnpm")?.ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "verified pnpm runtime is missing")
    })?;
    let args = build_pnpm_args(runtime, args.into_iter().map(OsString::from));
    run_command(
        "pnpm",
        &command.program,
        command.prefix_args.into_iter().chain(args),
        Some(cwd),
        runtime,
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
    cancellation: &CancellationToken,
) -> io::Result<()> {
    ensure_not_cancelled(cancellation)?;
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    for (key, value) in build_runtime_child_env(runtime, std::env::var_os("PATH").as_deref())? {
        command.env(key, value);
    }
    let mut child = command.spawn().map_err(|error| {
        io::Error::new(error.kind(), format!("failed to spawn {phase}: {error}"))
    })?;
    let wait = async {
        loop {
            if cancellation.is_cancelled() {
                let _ = child.kill().await;
                let _ = child.wait().await;
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "cold install cancelled",
                ));
            }
            if let Some(status) = child.try_wait()? {
                return if status.success() {
                    Ok(())
                } else {
                    Err(io::Error::other(format!("{phase} exited with {status}")))
                };
            }
            sleep(Duration::from_millis(100)).await;
        }
    };
    timeout(COMMAND_TIMEOUT, wait)
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, format!("{phase} timed out")))?
}

async fn candidate_revision(
    runtime: &RuntimeConfig,
    candidate: &Path,
    cancellation: &CancellationToken,
) -> io::Result<String> {
    ensure_not_cancelled(cancellation)?;
    let git = resolve_runtime_command(runtime, "git")?.ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "verified Git runtime is missing")
    })?;
    let mut command = Command::new(&git.program);
    command
        .args(git.prefix_args)
        .args(["rev-parse", "--verify", "HEAD"])
        .current_dir(candidate)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    for (key, value) in build_runtime_child_env(runtime, std::env::var_os("PATH").as_deref())? {
        command.env(key, value);
    }
    let output = timeout(Duration::from_secs(30), command.output())
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "git revision probe timed out"))??;
    if !output.status.success() || output.stdout.len() > 128 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "candidate Git revision is unavailable"));
    }
    let revision = String::from_utf8(output.stdout)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?
        .trim()
        .to_ascii_lowercase();
    if (revision.len() != 40 && revision.len() != 64)
        || !revision.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "candidate Git revision is invalid"));
    }
    Ok(revision)
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

fn corepack_root() -> Option<PathBuf> {
    std::env::var_os("COREPACK_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            std::env::var_os("LOCALAPPDATA")
                .map(PathBuf::from)
                .map(|path| path.join("node/corepack"))
                .filter(|path| path.is_absolute())
        })
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

fn supply_error(error: impl ToString) -> io::Error {
    io::Error::other(error.to_string())
}

fn cleanup_candidate(state: &AppState, operation_id: &str) {
    let Some(operation) = state
        .cold
        .load()
        .ok()
        .flatten()
        .filter(|operation| operation.operation_id == operation_id)
    else {
        return;
    };
    let candidate = PathBuf::from(operation.candidate);
    if let (Ok(downloads), Ok(candidate_root)) = (
        fs::canonicalize(&state.paths.downloads_dir),
        fs::canonicalize(&candidate),
    ) {
        if candidate_root != downloads && nexus_core::is_within(&downloads, &candidate_root) {
            let _ = fs::remove_dir_all(candidate_root);
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

    #[tokio::test]
    async fn stale_confirmation_and_cancel_are_terminal() {
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
        let mut waiting = coordinator.load().unwrap().unwrap();
        waiting.phase = ColdOperationPhase::AwaitingConfirmation;
        waiting.confirmation = Some("sha256:test".to_owned());
        coordinator.write(&waiting).unwrap();
        assert!(coordinator
            .claim_confirmation(&operation.operation_id, "sha256:stale")
            .await
            .is_err());
        coordinator
            .claim_confirmation(&operation.operation_id, "sha256:test")
            .await
            .expect("exact confirmation is claimed once");
        assert!(coordinator
            .claim_confirmation(&operation.operation_id, "sha256:test")
            .await
            .is_err());
        assert!(coordinator.cancel("stale").await.is_err());
        let cancelled = coordinator.cancel(&operation.operation_id).await.unwrap();
        assert_eq!(cancelled.phase, ColdOperationPhase::Cancelled);
        let _ = fs::remove_dir_all(root);
    }
}
