//! Checkpoint operations and restore transaction recovery.

use super::{
    api_error_response, canary, data_error_response, dsh, ensure_harness_selection_quiescent, io,
    settle_checkpoint_restore, snapshots, try_read_lifecycle, update_agent_state,
    update_agent_state_inner, update_error_response, validate_committed_checkpoint_restore,
    AppState, CheckpointAction, CheckpointCommand, CheckpointContentState,
    CheckpointCreateResponse, CheckpointListResponse, CheckpointManifest, CheckpointRestoreIntent,
    CheckpointRestoreJournal, CheckpointRestorePhase, CheckpointRestoreResponse, Json,
    NexusStateSnapshot, ProfileCatalog, ReleaseStore, SnapshotReference, State, StatusCode,
    DEFAULT_PROFILE,
};
use axum::response::IntoResponse;

pub(super) async fn checkpoint_list(State(state): State<AppState>) -> axum::response::Response {
    let _lifecycle = match try_read_lifecycle(&state) {
        Ok(guard) => guard,
        Err(response) => return response,
    };
    let checkpoints = match state.checkpoints.list() {
        Ok(checkpoints) => checkpoints,
        Err(error) => return data_error_response(error, "checkpoint_list_failed"),
    };
    let profile = match state.profiles.load() {
        Ok(catalog) => catalog.active_profile,
        Err(error) => return data_error_response(error, "checkpoint_profile_invalid"),
    };
    let last_capture = state.snapshots.last_capture();
    let inventory_refresh_pending = last_capture
        .get("state")
        .and_then(serde_json::Value::as_str)
        == Some("running");
    let mut diagnostic = state.snapshots.healthy_error();
    let snapshots = if inventory_refresh_pending {
        Vec::new()
    } else {
        match state.snapshots.list_inspections(profile).await {
            Ok(snapshots) => snapshots,
            Err(error) => {
                if diagnostic.is_none() {
                    diagnostic = Some(error.to_string());
                }
                Vec::new()
            }
        }
    };
    let pending_restore = match state.checkpoint_restores.load() {
        Ok(Some(journal)) => snapshots::restore_status(&journal, None),
        Ok(None) => None,
        Err(error) => return data_error_response(error, "checkpoint_restore_journal_failed"),
    };
    (
        StatusCode::OK,
        Json(
            CheckpointListResponse::new(checkpoints)
                .with_snapshot_state(snapshots, pending_restore, diagnostic)
                .with_last_capture(last_capture),
        ),
    )
        .into_response()
}

pub(super) async fn ensure_checkpoint_mutation_ready(
    state: &AppState,
) -> Result<(), axum::response::Response> {
    ensure_mutation_ready_for_owner(state, None).await
}

pub(super) async fn ensure_mutation_ready_for_owner(
    state: &AppState,
    cold_owner: Option<&str>,
) -> Result<(), axum::response::Response> {
    if let Err(error) = canary::ensure_idle(&state.paths) {
        return Err(data_error_response(error, "canary_pending"));
    }

    if state.cold.publication_pending() {
        return Err(api_error_response(
            StatusCode::CONFLICT,
            "cold_publication_pending",
            "Cold publication recovery is pending; use Retry recovery or Keep current and end recovery in Updates",
        ));
    }
    let owns_publication = match cold_owner {
        Some(id) => match state.cold.owns_verifying_publication(id).await {
            Ok(true) => true,
            Ok(false) => return Err(api_error_response(StatusCode::CONFLICT, "cold_install_owner_conflict",
                "The cold installation is no longer the active verifying owner; publication was stopped")),
            Err(error) => return Err(data_error_response(error, "cold_operation_unavailable")),
        },
        None => false,
    };
    match if owns_publication {
        Ok(false)
    } else {
        state.cold.cleanup_pending()
    } {
        Ok(true) => {
            return Err(api_error_response(
                StatusCode::CONFLICT,
                "cold_cleanup_pending",
                "cold cleanup is pending; retry cancel or restart the Agent",
            ));
        }
        Ok(false) => {}
        Err(error) => return Err(data_error_response(error, "cold_operation_unavailable")),
    }
    if let Err(error) = settle_checkpoint_restore(state).await {
        return Err(data_error_response(
            io::Error::other(error.to_string()),
            "checkpoint_recovery_failed",
        ));
    }
    let store = state.checkpoint_restores.clone();
    let pending = tokio::task::spawn_blocking(move || store.load())
        .await
        .map_err(|error| {
            data_error_response(
                io::Error::other(format!("checkpoint journal task failed: {error}")),
                "checkpoint_restore_journal_failed",
            )
        })?
        .map_err(|error| data_error_response(error, "checkpoint_restore_journal_failed"))?;
    if pending.is_some_and(|journal| journal.intent.snapshot.is_some()) {
        return Err(api_error_response(
            StatusCode::CONFLICT,
            "checkpoint_restore_pending",
            "a content restore is pending; use checkpoint retry or checkpoint abort",
        ));
    }
    Ok(())
}

pub(super) async fn checkpoint_control(
    State(state): State<AppState>,
    Json(command): Json<CheckpointCommand>,
) -> axum::response::Response {
    match command.action {
        CheckpointAction::List => checkpoint_list(State(state)).await,
        CheckpointAction::Create => checkpoint_create(state, command.note).await,
        CheckpointAction::Detail | CheckpointAction::Inspect => {
            let Some(id) = command.id else {
                return data_error_response(
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "snapshot or checkpoint id is required",
                    ),
                    "checkpoint_invalid",
                );
            };
            checkpoint_snapshot_read(state, id, command.action).await
        }
        CheckpointAction::Restore => {
            let Some(id) = command.id else {
                return data_error_response(
                    io::Error::new(io::ErrorKind::InvalidInput, "checkpoint id is required"),
                    "checkpoint_invalid",
                );
            };
            checkpoint_restore(state, id).await
        }
        CheckpointAction::Retry => checkpoint_restore_retry(state, command.id).await,
        CheckpointAction::Abort => checkpoint_restore_abort(state, command.id).await,
    }
}

pub(super) async fn checkpoint_create(
    state: AppState,
    note: Option<String>,
) -> axum::response::Response {
    let lifecycle = state.supervisor.acquire_lifecycle().await;
    if let Err(response) = ensure_checkpoint_mutation_ready(&state).await {
        return response;
    }
    let update_gate = match state.updater.try_acquire_gate() {
        Ok(gate) => gate,
        Err(error) => return update_error_response(error),
    };
    if let Err(response) = ensure_harness_selection_quiescent(
        &state,
        &lifecycle,
        "checkpoint_create_conflict",
        "cannot create a checkpoint until Harness is positively stopped and unowned",
    )
    .await
    {
        return response;
    }
    let current = state.runtime.read().await.clone();
    let profile = current
        .profile
        .clone()
        .unwrap_or_else(|| DEFAULT_PROFILE.to_owned());
    let state_snapshot = NexusStateSnapshot {
        profile: profile.clone(),
        release: current.release.clone(),
    };
    let version = selected_dsh_version(&state.releases, current.release.as_deref());
    let owner_state = state.clone();
    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let result = async {
            let (lease, capture_id) = owner_state
                .snapshots
                .acquire_capture(profile.clone(), "manual")
                .await?;
            let result = async {
                let manifest = lease.capture_manual(version, note.clone()).await?;
                owner_state.checkpoints.create_with_snapshot(
                    &profile,
                    current.release,
                    note,
                    state_snapshot,
                    Some(snapshots::snapshot_reference(&manifest)),
                )
            }
            .await;
            owner_state.snapshots.finish_capture(&capture_id, &result);
            result
        }
        .await;
        drop(update_gate);
        drop(lifecycle);
        let _ = result_tx.send(result);
    });
    match result_rx.await {
        Ok(Ok(checkpoint)) => (
            StatusCode::CREATED,
            Json(CheckpointCreateResponse::from_manifest(checkpoint)),
        )
            .into_response(),
        Ok(Err(error)) => data_error_response(error, "checkpoint_create_failed"),
        Err(_) => data_error_response(
            io::Error::other("checkpoint capture owner exited without a result"),
            "checkpoint_create_failed",
        ),
    }
}

pub(super) fn selected_dsh_version(releases: &ReleaseStore, release: Option<&str>) -> String {
    release
        .and_then(|id| releases.get(id).ok())
        .map(|manifest| manifest.version)
        .or_else(|| release.map(ToOwned::to_owned))
        .unwrap_or_else(|| "unmanaged".to_owned())
}

async fn checkpoint_snapshot_read(
    state: AppState,
    id: String,
    action: CheckpointAction,
) -> axum::response::Response {
    let (profile, snapshot_id) = match state.checkpoints.read(&id) {
        Ok(Some(checkpoint)) => match checkpoint.snapshot {
            Some(reference) => (checkpoint.profile, reference.snapshot_id),
            None => {
                return api_error_response(
                    StatusCode::CONFLICT,
                    "checkpoint_legacy_metadata_only",
                    "this legacy checkpoint has no content snapshot",
                )
            }
        },
        Ok(None) => match state.profiles.load() {
            Ok(catalog) => (catalog.active_profile, id),
            Err(error) => return data_error_response(error, "checkpoint_profile_invalid"),
        },
        Err(error) => return data_error_response(error, "checkpoint_invalid"),
    };
    match action {
        CheckpointAction::Detail => match state.snapshots.detail(profile, snapshot_id).await {
            Ok(detail) => (StatusCode::OK, Json(detail)).into_response(),
            Err(error) => data_error_response(error, "snapshot_detail_failed"),
        },
        CheckpointAction::Inspect => match state.snapshots.inspect(profile, snapshot_id).await {
            Ok(inspection) => (StatusCode::OK, Json(inspection)).into_response(),
            Err(error) => data_error_response(error, "snapshot_inspect_failed"),
        },
        _ => data_error_response(
            io::Error::new(io::ErrorKind::InvalidInput, "invalid snapshot read action"),
            "checkpoint_invalid",
        ),
    }
}

/// Resolve a restore request whose id names a healthy-start snapshot rather
/// than a checkpoint: synthesize a checkpoint that references the snapshot so
/// the standard two-phase restore and materialization apply unchanged. The
/// snapshot's harness version must still be an installed release slot.
async fn checkpoint_from_snapshot(
    state: &AppState,
    snapshot_id: &str,
) -> Result<CheckpointManifest, axum::response::Response> {
    let profiles = state
        .profiles
        .load()
        .map_err(|error| data_error_response(error, "checkpoint_profile_invalid"))?;
    let profile = profiles.active_profile.clone();
    let detail = state
        .snapshots
        .detail(profile.clone(), snapshot_id.to_owned())
        .await
        .map_err(|error| data_error_response(error, "snapshot_not_found"))?;
    let summary = &detail.summary;
    let snapshot_profile = summary.profile_name.clone();
    if snapshot_profile != profile {
        return Err(data_error_response(
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("snapshot belongs to profile {snapshot_profile}"),
            ),
            "snapshot_profile_mismatch",
        ));
    }
    let dsh_version = summary.dsh_version.clone();
    let releases = state
        .releases
        .load()
        .map_err(|error| data_error_response(error, "checkpoint_release_unavailable"))?;
    let release = releases
        .releases
        .iter()
        .find(|item| item.version == dsh_version)
        .map(|item| item.id.clone())
        .ok_or_else(|| {
            data_error_response(
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!(
                        "the snapshot's harness version {dsh_version} is not installed; cold-switch to it first"
                    ),
                ),
                "snapshot_release_not_installed",
            )
        })?;
    let state_snapshot = NexusStateSnapshot {
        profile: profile.clone(),
        release: Some(release.clone()),
    };
    let reference = SnapshotReference {
        snapshot_id: snapshot_id.to_owned(),
        summary: detail.summary.clone(),
    };
    state
        .checkpoints
        .create_with_snapshot(
            &profile,
            Some(release),
            Some(format!("restored from snapshot {snapshot_id}")),
            state_snapshot,
            Some(reference),
        )
        .map_err(|error| data_error_response(error, "snapshot_checkpoint_failed"))
}

// Resolve the requested checkpoint before planning any selection changes.
async fn resolve_checkpoint_for_restore(
    state: &AppState,
    id: &str,
) -> Result<CheckpointManifest, axum::response::Response> {
    match state.checkpoints.restore(id) {
        Ok(checkpoint) => Ok(checkpoint),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            // The id may name a healthy-start snapshot: synthesize a
            // checkpoint referencing it so the standard two-phase restore
            // and materialization apply unchanged.
            checkpoint_from_snapshot(state, id).await
        }
        Err(error) => Err(data_error_response(error, "checkpoint_restore_failed")),
    }
}

// Build the target selection before publishing the restore journal.
fn plan_checkpoint_restore(
    state: &AppState,
    checkpoint: &CheckpointManifest,
) -> Result<CheckpointRestoreIntent, axum::response::Response> {
    let previous_profiles = state
        .profiles
        .load()
        .map_err(|error| data_error_response(error, "checkpoint_profile_invalid"))?;
    let target_profiles = ProfileCatalog::new(
        checkpoint.profile.clone(),
        previous_profiles.profiles.clone(),
    )
    .map_err(|error| data_error_response(error, "checkpoint_profile_invalid"))?;
    let previous_releases = state
        .releases
        .load()
        .map_err(|error| data_error_response(error, "checkpoint_release_unavailable"))?;
    let target_releases = match state
        .releases
        .plan_checkpoint_release(checkpoint.release.as_deref())
    {
        Ok(catalog) => catalog,
        Err(error) => {
            let code = if error.kind() == io::ErrorKind::NotFound {
                "checkpoint_release_not_found"
            } else {
                "checkpoint_release_unavailable"
            };
            return Err(data_error_response(error, code));
        }
    };
    Ok(CheckpointRestoreIntent {
        checkpoint_id: checkpoint.id.clone(),
        previous_profiles,
        previous_current_release: previous_releases.current_release,
        previous_last_known_good: previous_releases.last_known_good,
        target_profiles,
        target_current_release: target_releases.current_release,
        target_last_known_good: target_releases.last_known_good,
        snapshot: None,
    })
}

pub(super) async fn checkpoint_restore(state: AppState, id: String) -> axum::response::Response {
    let lifecycle = state.supervisor.acquire_lifecycle().await;
    if let Err(response) = ensure_checkpoint_mutation_ready(&state).await {
        return response;
    }
    let update_gate = match state.updater.try_acquire_gate() {
        Ok(gate) => gate,
        Err(error) => return update_error_response(error),
    };
    if let Err(response) = ensure_harness_selection_quiescent(
        &state,
        &lifecycle,
        "checkpoint_restore_conflict",
        "cannot restore a checkpoint until Harness is positively stopped and unowned",
    )
    .await
    {
        return response;
    }
    let checkpoint = match resolve_checkpoint_for_restore(&state, &id).await {
        Ok(checkpoint) => checkpoint,
        Err(response) => return response,
    };
    let mut intent = match plan_checkpoint_restore(&state, &checkpoint) {
        Ok(intent) => intent,
        Err(response) => return response,
    };
    if let Some(reference) = checkpoint.snapshot.as_ref() {
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        let owner_state = state.clone();
        let checkpoint_for_owner = checkpoint.clone();
        let snapshot_id = reference.snapshot_id.clone();
        tokio::spawn(async move {
            let result = complete_content_checkpoint_restore(
                owner_state,
                &mut intent,
                checkpoint_for_owner,
                snapshot_id,
            )
            .await;
            drop(update_gate);
            drop(lifecycle);
            let _ = result_tx.send(result);
        });
        return content_restore_http_result(
            result_rx.await,
            "checkpoint_restore_failed",
            "checkpoint restore owner exited without a result",
        );
    }
    if let Err(error) = state.checkpoint_restores.begin(intent.clone()) {
        return data_error_response(error, "checkpoint_restore_journal_failed");
    }

    // Once Prepared is durable, an independent owner holds the supervisor
    // lifecycle gate through commit or rollback. Cancelling the HTTP request
    // cannot abandon a mixed Harness selection.
    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
    let owner_state = state.clone();
    tokio::spawn(async move {
        let result = complete_checkpoint_restore(owner_state, intent).await;
        drop(update_gate);
        drop(lifecycle);
        let _ = result_tx.send(result);
    });
    match result_rx.await {
        Ok(Ok(())) => (
            StatusCode::OK,
            Json(CheckpointRestoreResponse::restored(checkpoint)),
        )
            .into_response(),
        Ok(Err(error)) => data_error_response(error, "checkpoint_state_persistence_failed"),
        Err(_) => data_error_response(
            io::Error::other("checkpoint restore owner exited without a result"),
            "checkpoint_state_persistence_failed",
        ),
    }
}

async fn complete_content_checkpoint_restore(
    state: AppState,
    intent: &mut CheckpointRestoreIntent,
    checkpoint: nexus_protocol::CheckpointManifest,
    snapshot_id: String,
) -> io::Result<(
    nexus_protocol::CheckpointManifest,
    Option<nexus_protocol::CheckpointRestoreStatus>,
)> {
    let lease = state
        .snapshots
        .acquire(intent.target_profiles.active_profile.clone())
        .await?;
    let ticket = lease.prepare(snapshot_id).await?;
    intent.snapshot = Some(snapshots::binding_for(&lease, ticket.clone()));
    if let Err(error) = state.checkpoint_restores.begin(intent.clone()) {
        let rollback = lease.rollback(ticket).await;
        return Err(checkpoint_transaction_error(error, rollback.map(|_| ())));
    }

    let outcome = match lease.apply(ticket.clone()).await {
        Ok(outcome) => outcome,
        Err(error) => {
            return pending_content_restore(&state, intent, checkpoint, error, None);
        }
    };
    let outcome = if outcome.materialization_pending {
        match run_profile_materialization(&state, &lease, &ticket).await {
            Ok(()) => match lease.mark_materialized(ticket.clone()).await {
                Ok(outcome) => outcome,
                Err(error) => {
                    return pending_content_restore(
                        &state,
                        intent,
                        checkpoint,
                        error,
                        Some(&outcome),
                    );
                }
            },
            Err(error) => {
                return pending_content_restore(&state, intent, checkpoint, error, Some(&outcome));
            }
        }
    } else {
        outcome
    };

    if let Err(primary) = apply_checkpoint_target_selection(&state, intent).await {
        let content_rollback = lease.rollback(ticket.clone()).await.map(|_| ());
        let selection_rollback = rollback_checkpoint_selection(&state, intent).await;
        let rollback = combine_results(content_rollback, selection_rollback).and_then(|()| {
            state
                .checkpoint_restores
                .clear(CheckpointRestorePhase::Prepared, intent)
        });
        return Err(checkpoint_transaction_error(primary, rollback));
    }

    if let Err(primary) = mark_checkpoint_committed(&state, intent) {
        match state.checkpoint_restores.load() {
            Ok(Some(journal))
                if journal.intent == *intent
                    && journal.phase == CheckpointRestorePhase::Committed => {}
            Ok(Some(journal))
                if journal.intent == *intent
                    && journal.phase == CheckpointRestorePhase::Prepared =>
            {
                let content_rollback = lease.rollback(ticket.clone()).await.map(|_| ());
                let selection_rollback = rollback_checkpoint_selection(&state, intent).await;
                let rollback =
                    combine_results(content_rollback, selection_rollback).and_then(|()| {
                        state
                            .checkpoint_restores
                            .clear(CheckpointRestorePhase::Prepared, intent)
                    });
                return Err(checkpoint_transaction_error(primary, rollback));
            }
            Ok(_) => return Err(primary),
            Err(inspection) => {
                return Err(io::Error::new(
                    primary.kind(),
                    format!("{primary}; cannot inspect checkpoint commit outcome: {inspection}"),
                ));
            }
        }
    }

    match lease.commit(ticket).await {
        Ok(_) => {
            state
                .checkpoint_restores
                .clear(CheckpointRestorePhase::Committed, intent)?;
            Ok((checkpoint, None))
        }
        Err(error) => pending_content_restore(&state, intent, checkpoint, error, Some(&outcome)),
    }
}

fn pending_content_restore(
    state: &AppState,
    intent: &CheckpointRestoreIntent,
    checkpoint: nexus_protocol::CheckpointManifest,
    error: io::Error,
    outcome: Option<&nexus_snapshots::RestoreOutcome>,
) -> io::Result<(
    nexus_protocol::CheckpointManifest,
    Option<nexus_protocol::CheckpointRestoreStatus>,
)> {
    let journal = state.checkpoint_restores.load()?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "content restore failed after its outer journal disappeared",
        )
    })?;
    let diagnostic = bounded_checkpoint_diagnostic(&error);
    state
        .checkpoint_restores
        .record_error(journal.phase, intent, diagnostic)?;
    let journal = state
        .checkpoint_restores
        .load()?
        .ok_or_else(|| io::Error::other("content restore journal disappeared"))?;
    Ok((checkpoint, snapshots::restore_status(&journal, outcome)))
}

async fn run_profile_materialization(
    state: &AppState,
    lease: &snapshots::SnapshotLease,
    ticket: &nexus_snapshots::RestoreTicket,
) -> io::Result<()> {
    let paths = state.paths.clone();
    let dsh_home = lease.store().dsh_home().to_path_buf();
    let profile = ticket.profile_name.clone();
    tokio::task::spawn_blocking(move || dsh::materialize_profile(&paths, &dsh_home, &profile))
        .await
        .map_err(|error| io::Error::other(format!("materialization owner failed: {error}")))?
}

async fn apply_checkpoint_target_selection(
    state: &AppState,
    intent: &CheckpointRestoreIntent,
) -> io::Result<()> {
    state.releases.restore_release_pointers(
        intent.target_current_release.as_deref(),
        intent.target_last_known_good.as_deref(),
    )?;
    state.profiles.write(&intent.target_profiles)?;
    let profile = intent.target_profiles.active_profile.clone();
    let release = intent.target_current_release.clone();
    update_agent_state_inner(
        state,
        move |current| {
            current.set_profile(profile);
            current.set_release(release);
        },
        true,
    )
    .await
    .map(|_| ())
    .map_err(|error| io::Error::other(error.to_string()))
}

fn mark_checkpoint_committed(state: &AppState, intent: &CheckpointRestoreIntent) -> io::Result<()> {
    let result = state.checkpoint_restores.mark_committed(intent);
    #[cfg(test)]
    let result = match result {
        Ok(())
            if state
                .checkpoint_commit_result_failure
                .swap(false, std::sync::atomic::Ordering::SeqCst) =>
        {
            Err(io::Error::other(
                "injected error after durable Committed journal publication",
            ))
        }
        result => result,
    };
    result
}

pub(super) async fn checkpoint_restore_retry(
    state: AppState,
    requested_id: Option<String>,
) -> axum::response::Response {
    let lifecycle = state.supervisor.acquire_lifecycle().await;
    let update_gate = match state.updater.try_acquire_gate() {
        Ok(gate) => gate,
        Err(error) => return update_error_response(error),
    };
    if let Err(response) = ensure_harness_selection_quiescent(
        &state,
        &lifecycle,
        "checkpoint_restore_conflict",
        "cannot retry a restore until Harness is positively stopped and unowned",
    )
    .await
    {
        return response;
    }
    let journal = match load_requested_content_restore(&state, requested_id.as_deref()) {
        Ok(journal) => journal,
        Err(error) => return data_error_response(error, "checkpoint_restore_not_pending"),
    };
    let checkpoint = match state.checkpoints.get(&journal.intent.checkpoint_id) {
        Ok(checkpoint) => checkpoint,
        Err(error) => return data_error_response(error, "checkpoint_not_found"),
    };
    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
    let owner_state = state.clone();
    tokio::spawn(async move {
        let result = resume_content_checkpoint_restore(owner_state, journal, checkpoint).await;
        drop(update_gate);
        drop(lifecycle);
        let _ = result_tx.send(result);
    });
    content_restore_http_result(
        result_rx.await,
        "checkpoint_restore_retry_failed",
        "checkpoint retry owner exited without a result",
    )
}

async fn resume_content_checkpoint_restore(
    state: AppState,
    journal: CheckpointRestoreJournal,
    checkpoint: nexus_protocol::CheckpointManifest,
) -> io::Result<(
    nexus_protocol::CheckpointManifest,
    Option<nexus_protocol::CheckpointRestoreStatus>,
)> {
    let binding = journal.intent.snapshot.as_ref().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "legacy restore has no content ticket",
        )
    })?;
    let lease = state.snapshots.acquire_bound(binding).await?;
    if journal.phase == CheckpointRestorePhase::Committed {
        validate_committed_checkpoint_restore(&journal, &state.profiles, &state.releases)?;
        return match lease.commit(binding.ticket.clone()).await {
            Ok(_) => {
                state
                    .checkpoint_restores
                    .clear(CheckpointRestorePhase::Committed, &journal.intent)?;
                Ok((checkpoint, None))
            }
            Err(error) => pending_content_restore(&state, &journal.intent, checkpoint, error, None),
        };
    }

    let mut outcome = match lease.resume_apply(binding.ticket.clone()).await {
        Ok(outcome) => outcome,
        Err(error) => {
            return pending_content_restore(&state, &journal.intent, checkpoint, error, None);
        }
    };
    if outcome.materialization_pending {
        if let Err(error) = run_profile_materialization(&state, &lease, &binding.ticket).await {
            return pending_content_restore(
                &state,
                &journal.intent,
                checkpoint,
                error,
                Some(&outcome),
            );
        }
        outcome = match lease.mark_materialized(binding.ticket.clone()).await {
            Ok(outcome) => outcome,
            Err(error) => {
                return pending_content_restore(
                    &state,
                    &journal.intent,
                    checkpoint,
                    error,
                    Some(&outcome),
                );
            }
        };
    }
    if let Err(primary) = apply_checkpoint_target_selection(&state, &journal.intent).await {
        let content_rollback = lease.rollback(binding.ticket.clone()).await.map(|_| ());
        let selection_rollback = rollback_checkpoint_selection(&state, &journal.intent).await;
        let rollback = combine_results(content_rollback, selection_rollback).and_then(|()| {
            state
                .checkpoint_restores
                .clear(CheckpointRestorePhase::Prepared, &journal.intent)
        });
        return Err(checkpoint_transaction_error(primary, rollback));
    }
    if let Err(primary) = mark_checkpoint_committed(&state, &journal.intent) {
        match state.checkpoint_restores.load() {
            Ok(Some(current))
                if current.intent == journal.intent
                    && current.phase == CheckpointRestorePhase::Committed => {}
            Ok(Some(current))
                if current.intent == journal.intent
                    && current.phase == CheckpointRestorePhase::Prepared =>
            {
                let content_rollback = lease.rollback(binding.ticket.clone()).await.map(|_| ());
                let selection_rollback =
                    rollback_checkpoint_selection(&state, &journal.intent).await;
                let rollback =
                    combine_results(content_rollback, selection_rollback).and_then(|()| {
                        state
                            .checkpoint_restores
                            .clear(CheckpointRestorePhase::Prepared, &journal.intent)
                    });
                return Err(checkpoint_transaction_error(primary, rollback));
            }
            Ok(_) => return Err(primary),
            Err(inspection) => {
                return Err(io::Error::new(
                    primary.kind(),
                    format!("{primary}; cannot inspect checkpoint commit outcome: {inspection}"),
                ));
            }
        }
    }
    match lease.commit(binding.ticket.clone()).await {
        Ok(_) => {
            state
                .checkpoint_restores
                .clear(CheckpointRestorePhase::Committed, &journal.intent)?;
            Ok((checkpoint, None))
        }
        Err(error) => {
            pending_content_restore(&state, &journal.intent, checkpoint, error, Some(&outcome))
        }
    }
}

pub(super) async fn checkpoint_restore_abort(
    state: AppState,
    requested_id: Option<String>,
) -> axum::response::Response {
    let lifecycle = state.supervisor.acquire_lifecycle().await;
    let update_gate = match state.updater.try_acquire_gate() {
        Ok(gate) => gate,
        Err(error) => return update_error_response(error),
    };
    if let Err(response) = ensure_harness_selection_quiescent(
        &state,
        &lifecycle,
        "checkpoint_restore_conflict",
        "cannot abort a restore until Harness is positively stopped and unowned",
    )
    .await
    {
        return response;
    }
    let journal = match load_requested_content_restore(&state, requested_id.as_deref()) {
        Ok(journal) if journal.phase == CheckpointRestorePhase::Prepared => journal,
        Ok(_) => {
            return api_error_response(
                StatusCode::CONFLICT,
                "checkpoint_restore_committed",
                "a committed content restore can only be finished with retry",
            )
        }
        Err(error) => return data_error_response(error, "checkpoint_restore_not_pending"),
    };
    let checkpoint = match state.checkpoints.get(&journal.intent.checkpoint_id) {
        Ok(checkpoint) => checkpoint,
        Err(error) => return data_error_response(error, "checkpoint_not_found"),
    };
    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
    let owner_state = state.clone();
    tokio::spawn(async move {
        let result = async {
            let binding = journal.intent.snapshot.as_ref().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "legacy restore has no content ticket",
                )
            })?;
            let lease = owner_state.snapshots.acquire_bound(binding).await?;
            lease.rollback(binding.ticket.clone()).await?;
            rollback_checkpoint_selection(&owner_state, &journal.intent).await?;
            owner_state
                .checkpoint_restores
                .clear(CheckpointRestorePhase::Prepared, &journal.intent)?;
            Ok::<_, io::Error>(checkpoint)
        }
        .await;
        drop(update_gate);
        drop(lifecycle);
        let _ = result_tx.send(result);
    });
    match result_rx.await {
        Ok(Ok(checkpoint)) => (
            StatusCode::OK,
            Json(CheckpointRestoreResponse::content(
                checkpoint,
                false,
                CheckpointContentState::RolledBack,
                None,
            )),
        )
            .into_response(),
        Ok(Err(error)) => data_error_response(error, "checkpoint_restore_abort_failed"),
        Err(_) => data_error_response(
            io::Error::other("checkpoint abort owner exited without a result"),
            "checkpoint_restore_abort_failed",
        ),
    }
}

fn content_restore_http_result(
    result: Result<
        io::Result<(
            nexus_protocol::CheckpointManifest,
            Option<nexus_protocol::CheckpointRestoreStatus>,
        )>,
        tokio::sync::oneshot::error::RecvError,
    >,
    failure_code: &str,
    owner_error: &str,
) -> axum::response::Response {
    match result {
        Ok(Ok((checkpoint, None))) => (
            StatusCode::OK,
            Json(CheckpointRestoreResponse::content(
                checkpoint,
                true,
                CheckpointContentState::Committed,
                None,
            )),
        )
            .into_response(),
        Ok(Ok((checkpoint, Some(status)))) => (
            StatusCode::ACCEPTED,
            Json(CheckpointRestoreResponse::content(
                checkpoint,
                false,
                status.state.clone(),
                Some(status),
            )),
        )
            .into_response(),
        Ok(Err(error)) => data_error_response(error, failure_code),
        Err(_) => data_error_response(io::Error::other(owner_error), failure_code),
    }
}

fn load_requested_content_restore(
    state: &AppState,
    requested_id: Option<&str>,
) -> io::Result<CheckpointRestoreJournal> {
    let journal = state.checkpoint_restores.load()?.ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "no checkpoint restore is pending")
    })?;
    let binding = journal.intent.snapshot.as_ref().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "the pending legacy restore has no content retry or abort action",
        )
    })?;
    if requested_id
        .is_some_and(|id| id != journal.intent.checkpoint_id && id != binding.ticket.ticket_id)
    {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "requested restore does not match the pending transaction",
        ));
    }
    Ok(journal)
}

pub(super) fn bounded_checkpoint_diagnostic(error: &io::Error) -> String {
    let mut text: String = error
        .to_string()
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .take(4096)
        .collect();
    if text.is_empty() {
        text = "checkpoint restore failed".to_owned();
    }
    text
}

fn combine_results(first: io::Result<()>, second: io::Result<()>) -> io::Result<()> {
    match (first, second) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(first), Ok(())) => Err(first),
        (Ok(()), Err(second)) => Err(second),
        (Err(first), Err(second)) => Err(io::Error::new(
            first.kind(),
            format!("{first}; selection rollback also failed: {second}"),
        )),
    }
}

async fn complete_checkpoint_restore(
    state: AppState,
    intent: CheckpointRestoreIntent,
) -> io::Result<()> {
    #[cfg(test)]
    state.wait_for_checkpoint_transition_gate().await;

    let target_result = (|| -> io::Result<()> {
        state.releases.restore_release_pointers(
            intent.target_current_release.as_deref(),
            intent.target_last_known_good.as_deref(),
        )?;
        state.profiles.write(&intent.target_profiles)
    })();
    let target_result = match target_result {
        Ok(()) => {
            let active_profile = intent.target_profiles.active_profile.clone();
            let current_release = intent.target_current_release.clone();
            update_agent_state_inner(
                &state,
                |current| {
                    current.set_profile(active_profile);
                    current.set_release(current_release);
                },
                true,
            )
            .await
            .map(|_| ())
            .map_err(|error| io::Error::other(error.to_string()))
        }
        Err(error) => Err(error),
    };
    if let Err(primary) = target_result {
        return Err(checkpoint_transaction_error(
            primary,
            rollback_prepared_checkpoint(&state, &intent).await,
        ));
    }
    let commit_result = state.checkpoint_restores.mark_committed(&intent);
    #[cfg(test)]
    let commit_result = match commit_result {
        Ok(())
            if state
                .checkpoint_commit_result_failure
                .swap(false, std::sync::atomic::Ordering::SeqCst) =>
        {
            Err(io::Error::other(
                "injected error after durable Committed journal publication",
            ))
        }
        result => result,
    };
    if let Err(primary) = commit_result {
        match state.checkpoint_restores.load() {
            Ok(Some(journal))
                if journal.intent == intent
                    && journal.phase == CheckpointRestorePhase::Committed =>
            {
                tracing::warn!(error = %primary, "checkpoint commit returned an error after durable Committed publication");
            }
            Ok(Some(journal))
                if journal.intent == intent
                    && journal.phase == CheckpointRestorePhase::Prepared =>
            {
                return Err(checkpoint_transaction_error(
                    primary,
                    rollback_prepared_checkpoint(&state, &intent).await,
                ));
            }
            Ok(None) => {
                return Err(checkpoint_transaction_error(
                    primary,
                    rollback_prepared_checkpoint(&state, &intent).await,
                ));
            }
            Ok(Some(_)) => {
                return Err(io::Error::new(
                    primary.kind(),
                    format!(
                        "{primary}; checkpoint journal changed while commit result was uncertain"
                    ),
                ));
            }
            Err(inspection) => {
                // The phase is unknown. Never roll back a target that may
                // already be durably Committed; leave the journal for the
                // next fail-closed startup/control recovery.
                return Err(io::Error::new(
                    primary.kind(),
                    format!("{primary}; cannot inspect checkpoint commit outcome: {inspection}"),
                ));
            }
        }
    }
    if let Err(error) = state
        .checkpoint_restores
        .clear(CheckpointRestorePhase::Committed, &intent)
    {
        tracing::warn!(error = %error, "committed checkpoint restore journal remains for validation on next control transition");
    }
    Ok(())
}

async fn rollback_prepared_checkpoint(
    state: &AppState,
    intent: &CheckpointRestoreIntent,
) -> io::Result<()> {
    rollback_checkpoint_selection(state, intent).await?;
    state
        .checkpoint_restores
        .clear(CheckpointRestorePhase::Prepared, intent)
}

async fn rollback_checkpoint_selection(
    state: &AppState,
    intent: &CheckpointRestoreIntent,
) -> io::Result<()> {
    let mut failures = Vec::new();
    if let Err(error) = state.releases.restore_release_pointers(
        intent.previous_current_release.as_deref(),
        intent.previous_last_known_good.as_deref(),
    ) {
        failures.push(format!("release pointer rollback failed: {error}"));
    }
    if let Err(error) = state.profiles.write(&intent.previous_profiles) {
        failures.push(format!("profile rollback failed: {error}"));
    }
    let previous_profile = intent.previous_profiles.active_profile.clone();
    let previous_release = intent.previous_current_release.clone();
    if let Err(error) = update_agent_state(state, |current| {
        // The publication mutex serializes this selection rollback with an
        // Agent shutdown or Harness observation. Never replay the stale full
        // Agent snapshot captured before the restore transaction.
        current.set_profile(previous_profile);
        current.set_release(previous_release);
    })
    .await
    {
        failures.push(format!("Agent metadata rollback failed: {error}"));
    }
    if !failures.is_empty() {
        return Err(io::Error::other(failures.join("; ")));
    }
    Ok(())
}

fn checkpoint_transaction_error(primary: io::Error, rollback: io::Result<()>) -> io::Error {
    match rollback {
        Ok(()) => primary,
        Err(rollback) => io::Error::new(
            primary.kind(),
            format!("{primary}; checkpoint rollback also failed: {rollback}"),
        ),
    }
}
