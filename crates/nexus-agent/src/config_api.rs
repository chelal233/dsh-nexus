//! Configuration mutations, response construction and secret redaction.

use super::{
    api_error_response, config_status, data_error_response, ensure_checkpoint_mutation_ready,
    ensure_harness_stopped, ensure_update_idle, env, io, runtime_patches, supervisor,
    update_error_response, AppState, ConfigAction, ConfigCommand, ConfigResponse,
    HarnessLaunchSpec, Json, NexusConfigFile, RuntimeConfig, SnapshotsConfig, State, StatusCode,
    UpdateSpec, HARNESS_ARGS_ENV, HARNESS_PROGRAM_ENV, HARNESS_READINESS_TIMEOUT_ENV,
    HARNESS_READINESS_URL_ENV, HARNESS_WORKING_DIR_ENV, UPDATE_BUILD_ARGS_ENV,
    UPDATE_BUILD_PROGRAM_ENV, UPDATE_GIT_PROGRAM_ENV, UPDATE_REF_ENV, UPDATE_SOURCE_ENV,
    UPDATE_TIMEOUT_ENV, UPDATE_VERIFY_ARGS_ENV, UPDATE_VERIFY_PROGRAM_ENV,
};
use axum::response::IntoResponse;

pub(super) async fn config_control(
    State(state): State<AppState>,
    Json(command): Json<ConfigCommand>,
) -> axum::response::Response {
    if command.action != ConfigAction::Status
        && command
            .expected_revision
            .as_deref()
            .is_none_or(str::is_empty)
    {
        return api_error_response(
            StatusCode::PRECONDITION_REQUIRED,
            "config_revision_required",
            "Refresh configuration before saving; expected_revision is required",
        );
    }
    let expected = command.expected_revision.as_deref().unwrap_or("");
    match command.action {
        ConfigAction::Status => config_status(State(state)).await,
        ConfigAction::DiscardHarnessPatchPreview => {
            let id = command
                .patch_query
                .as_ref()
                .and_then(|query| query.preview_id.as_deref())
                .unwrap_or("");
            runtime_patches::discard_preview(&state.paths, id);
            Json(serde_json::json!({"discarded":true})).into_response()
        }
        ConfigAction::ListHarnessPatchRefs => {
            let query = command.patch_query.unwrap_or_default();
            let Some(entry) = query.entry else {
                return data_error_response(
                    io::Error::new(io::ErrorKind::InvalidInput, "Patch entry is required"),
                    "patch_query_invalid",
                );
            };
            match runtime_patches::list_refs(&entry, query.page.unwrap_or(1)).await {
                Ok(value) => (StatusCode::OK, Json(value)).into_response(),
                Err(error) => data_error_response(error, "patch_refs_failed"),
            }
        }
        ConfigAction::SetExternalHarness | ConfigAction::ClearExternalHarness => {
            let lifecycle = state.supervisor.acquire_lifecycle().await;
            if let Err(response) = ensure_checkpoint_mutation_ready(&state).await {
                return response;
            }
            if let Err(response) = ensure_harness_stopped(&state, &lifecycle).await {
                return response;
            }
            let _update = match state.updater.try_acquire_gate() {
                Ok(g) => g,
                Err(e) => return update_error_response(e),
            };
            let _configuration = match state.snapshots.try_acquire_configuration() {
                Ok(g) => g,
                Err(e) => return data_error_response(e, "source_busy"),
            };
            let external = if command.action == ConfigAction::SetExternalHarness {
                let Some(path) = command.external_harness_path else {
                    return data_error_response(
                        io::Error::other("External Harness path is required"),
                        "source_invalid",
                    );
                };
                let paths = state.paths.clone();
                match tokio::task::spawn_blocking(move || {
                    nexus_core::ExternalHarness::inspect(&paths, std::path::Path::new(&path))
                })
                .await
                .map_err(io::Error::other)
                .and_then(|r| r)
                {
                    Ok(source) => Some(source),
                    Err(e) => return data_error_response(e, "external_source_invalid"),
                }
            } else {
                None
            };
            transact_config_response(&state, expected, move |document| {
                document.external_harness = external;
                Ok(())
            })
        }
        ConfigAction::SetHarness => {
            let Some(payload) = command.harness else {
                return data_error_response(
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "harness configuration is required",
                    ),
                    "config_invalid",
                );
            };
            let lifecycle = state.supervisor.acquire_lifecycle().await;
            if let Err(response) = ensure_checkpoint_mutation_ready(&state).await {
                return response;
            }
            if let Err(response) = ensure_harness_stopped(&state, &lifecycle).await {
                return response;
            }
            let preserve = command.preserve_harness_readiness_url;
            let releases = state.releases.clone();
            transact_config_response(&state, expected, move |document| {
                let mut payload = payload;
                if preserve {
                    if let Some(existing) = document
                        .harness
                        .as_ref()
                        .and_then(|harness| harness.readiness_url.clone())
                    {
                        payload.readiness_url = Some(existing);
                    } else if env::var_os(HARNESS_READINESS_URL_ENV)
                        .is_some_and(|value| !value.is_empty())
                    {
                        payload.readiness_url = None;
                    } else {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "cannot preserve a readiness URL when no existing URL is configured",
                        ));
                    }
                }
                let mut spec = HarnessLaunchSpec::from_payload(payload)?;
                supervisor::normalize_managed_launch(&mut spec, &releases)?;
                document.harness = Some(spec);
                Ok(())
            })
        }
        ConfigAction::ClearHarness => {
            let lifecycle = state.supervisor.acquire_lifecycle().await;
            if let Err(response) = ensure_checkpoint_mutation_ready(&state).await {
                return response;
            }
            if let Err(response) = ensure_harness_stopped(&state, &lifecycle).await {
                return response;
            }
            transact_config_response(&state, expected, |document| {
                document.harness = None;
                Ok(())
            })
        }
        ConfigAction::SetUpdate | ConfigAction::SetUpdateSource => {
            let source_only = command.action == ConfigAction::SetUpdateSource;
            let Some(payload) = command.update else {
                return data_error_response(
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "update configuration is required",
                    ),
                    "config_invalid",
                );
            };
            if let Err(error) = nexus_core::validate_update_source(&payload.source) {
                return data_error_response(error, "config_invalid");
            }
            if let Err(response) = ensure_checkpoint_mutation_ready(&state).await {
                return response;
            }
            let _update_gate = match state.updater.try_acquire_gate() {
                Ok(gate) => gate,
                Err(error) => return update_error_response(error),
            };
            if let Err(response) = ensure_update_idle(&state) {
                return response;
            }
            transact_config_response(&state, expected, move |document| {
                let update = if source_only {
                    let mut current = document.update.clone().map(Ok).unwrap_or_else(|| {
                        serde_json::from_value::<UpdateSpec>(
                            serde_json::json!({"source":payload.source}),
                        )
                        .map_err(io::Error::other)
                    })?;
                    current.source = payload.source;
                    current.validate()?;
                    current
                } else {
                    UpdateSpec::from_payload(payload)?
                };
                document.set_update(Some(update));
                Ok(())
            })
        }
        ConfigAction::ClearUpdate => {
            if let Err(response) = ensure_checkpoint_mutation_ready(&state).await {
                return response;
            }
            let _update_gate = match state.updater.try_acquire_gate() {
                Ok(gate) => gate,
                Err(error) => return update_error_response(error),
            };
            if let Err(response) = ensure_update_idle(&state) {
                return response;
            }
            transact_config_response(&state, expected, |document| {
                document.set_update(None);
                Ok(())
            })
        }
        ConfigAction::SetHarnessPreferences
        | ConfigAction::FetchHarnessPatches
        | ConfigAction::PreviewHarnessPatches
        | ConfigAction::ApplyHarnessPatchPreview => {
            let preview_id = command
                .patch_query
                .as_ref()
                .and_then(|query| query.preview_id.as_deref());
            let payload = if command.action == ConfigAction::ApplyHarnessPatchPreview {
                match preview_id
                    .ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidInput, "Patch preview ID is required")
                    })
                    .and_then(|id| runtime_patches::preview_candidate(&state.paths, expected, id))
                {
                    Ok(payload) => Some(payload),
                    Err(error) => return data_error_response(error, "patch_preview_invalid"),
                }
            } else {
                command.harness_preferences
            };
            let Some(payload) = payload else {
                return data_error_response(
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "harness_preferences is required; use an empty object to inherit",
                    ),
                    "config_invalid",
                );
            };
            let mut preferences = match nexus_core::normalize_harness_preferences(payload) {
                Ok(value) => value,
                Err(error) => return data_error_response(error, "config_invalid"),
            };
            for value in [&preferences.deepseek_base_url, &preferences.search_base_url]
                .into_iter()
                .flatten()
            {
                if value
                    .parse::<axum::http::Uri>()
                    .ok()
                    .and_then(|url| url.host().map(str::to_owned))
                    .is_none()
                {
                    return data_error_response(
                        io::Error::new(io::ErrorKind::InvalidInput, "Invalid API base URL"),
                        "config_invalid",
                    );
                }
            }
            let lifecycle = state.supervisor.acquire_lifecycle().await;
            if let Err(response) = ensure_checkpoint_mutation_ready(&state).await {
                return response;
            }
            if let Err(response) = ensure_harness_stopped(&state, &lifecycle).await {
                return response;
            }
            let _update_gate = match state.updater.try_acquire_gate() {
                Ok(gate) => gate,
                Err(error) => return update_error_response(error),
            };
            if let Err(response) = ensure_update_idle(&state) {
                return response;
            }
            let _snapshot_gate = match state.snapshots.try_acquire_configuration() {
                Ok(gate) => gate,
                Err(error) => return data_error_response(error, "config_change_conflict"),
            };
            if matches!(
                command.action,
                ConfigAction::FetchHarnessPatches | ConfigAction::PreviewHarnessPatches
            ) {
                match state.config.snapshot() {
                    Ok(current) if current.revision == expected => {}
                    Ok(_) => {
                        return api_error_response(
                            StatusCode::CONFLICT,
                            "config_revision_conflict",
                            "Configuration changed; keep your draft and reload before downloading",
                        )
                    }
                    Err(error) => return data_error_response(error, "config_unavailable"),
                }
                if command.action == ConfigAction::PreviewHarnessPatches {
                    return match runtime_patches::preview_update(
                        &state.paths,
                        expected,
                        preferences,
                    )
                    .await
                    {
                        Ok(value) => (StatusCode::OK, Json(value)).into_response(),
                        Err(error) => data_error_response(error, "patch_preview_failed"),
                    };
                }
                preferences = match runtime_patches::fetch(&state.paths, preferences).await {
                    Ok(value) => value,
                    Err(error) => return data_error_response(error, "patch_download_failed"),
                };
            }
            if command.action == ConfigAction::ApplyHarnessPatchPreview {
                preferences = match runtime_patches::preview_candidate(
                    &state.paths,
                    expected,
                    preview_id.unwrap_or(""),
                ) {
                    Ok(preferences) => preferences,
                    Err(error) => return data_error_response(error, "patch_preview_invalid"),
                };
            }
            let acknowledged = preferences.clone();
            let response = transact_config_response(&state, expected, move |document| {
                document.harness_preferences =
                    (preferences != Default::default()).then_some(preferences);
                Ok(())
            });
            if response.status().is_success() {
                if let Some(id) = preview_id {
                    runtime_patches::finish_preview(id);
                }
                if let Err(error) =
                    runtime_patches::acknowledge_disabled(&state.paths, &acknowledged)
                {
                    return data_error_response(error, "patch_acknowledgement_failed");
                }
            }
            response
        }
        ConfigAction::SetRuntime | ConfigAction::ClearRuntime => {
            let runtime = if command.action == ConfigAction::SetRuntime {
                let Some(payload) = command.runtime else {
                    return data_error_response(
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "runtime configuration is required",
                        ),
                        "config_invalid",
                    );
                };
                match RuntimeConfig::from_payload(payload) {
                    Ok(runtime) => Some(runtime),
                    Err(error) => return data_error_response(error, "config_invalid"),
                }
            } else {
                None
            };
            let lifecycle = state.supervisor.acquire_lifecycle().await;
            if let Err(response) = ensure_checkpoint_mutation_ready(&state).await {
                return response;
            }
            if let Err(response) = ensure_harness_stopped(&state, &lifecycle).await {
                return response;
            }
            let _update_gate = match state.updater.try_acquire_gate() {
                Ok(gate) => gate,
                Err(error) => return update_error_response(error),
            };
            if let Err(response) = ensure_update_idle(&state) {
                return response;
            }
            transact_config_response(&state, expected, move |document| {
                document.runtime = runtime;
                Ok(())
            })
        }
        ConfigAction::SetSnapshots | ConfigAction::ClearSnapshots => {
            let snapshots = if command.action == ConfigAction::SetSnapshots {
                let Some(payload) = command.snapshots else {
                    return data_error_response(
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "snapshot configuration is required",
                        ),
                        "config_invalid",
                    );
                };
                Some(SnapshotsConfig::from_payload(payload))
            } else {
                None
            };
            let lifecycle = state.supervisor.acquire_lifecycle().await;
            if let Err(response) = ensure_checkpoint_mutation_ready(&state).await {
                return response;
            }
            if let Err(response) = ensure_harness_stopped(&state, &lifecycle).await {
                return response;
            }
            let _update_gate = match state.updater.try_acquire_gate() {
                Ok(gate) => gate,
                Err(error) => return update_error_response(error),
            };
            transact_config_response(&state, expected, move |document| {
                document.snapshots = snapshots;
                Ok(())
            })
        }
    }
}

fn config_response(document: NexusConfigFile) -> ConfigResponse {
    let harness_readiness_url_redacted = document
        .harness
        .as_ref()
        .and_then(|harness| harness.readiness_url.as_ref())
        .is_some_and(|url| redact_config_url(Some(url.clone())).as_deref() != Some(url.as_str()));
    let snapshots = document.snapshots.map(|snapshots| snapshots.to_payload());
    let external_harness = document
        .external_harness
        .as_ref()
        .map(|s| serde_json::to_value(s).expect("source serialization"));
    let mut response = ConfigResponse::new(
        document.harness.map(|harness| {
            let mut payload = harness.to_payload();
            payload.args = redact_config_args(payload.args);
            payload.readiness_url = redact_config_url(payload.readiness_url);
            payload
        }),
        document.update.map(|update| {
            let mut payload = update.to_payload();
            payload.build_args = redact_config_args(payload.build_args);
            payload.verify_args = redact_config_args(payload.verify_args);
            payload
        }),
    )
    .with_runtime(document.runtime.map(|runtime| runtime.to_payload()))
    .with_snapshots(snapshots)
    .with_harness_preferences(document.harness_preferences)
    .with_harness_readiness_url_redacted(harness_readiness_url_redacted);
    response.external_harness = external_harness;
    response
}

pub(super) fn config_response_for_paths(
    _paths: &nexus_core::NexusPaths,
    snapshot: nexus_core::ConfigSnapshot,
) -> io::Result<ConfigResponse> {
    let harness_env_override = [
        HARNESS_PROGRAM_ENV,
        HARNESS_ARGS_ENV,
        HARNESS_WORKING_DIR_ENV,
        HARNESS_READINESS_URL_ENV,
        HARNESS_READINESS_TIMEOUT_ENV,
    ]
    .iter()
    .any(|key| env::var_os(key).is_some_and(|value| !value.is_empty()));
    let update_env_override = [
        UPDATE_SOURCE_ENV,
        UPDATE_REF_ENV,
        UPDATE_GIT_PROGRAM_ENV,
        UPDATE_BUILD_PROGRAM_ENV,
        UPDATE_BUILD_ARGS_ENV,
        UPDATE_VERIFY_PROGRAM_ENV,
        UPDATE_VERIFY_ARGS_ENV,
        UPDATE_TIMEOUT_ENV,
    ]
    .iter()
    .any(|key| env::var_os(key).is_some_and(|value| !value.is_empty()));

    let effective = nexus_core::effective_config_document(snapshot.document)?;
    let mut response = config_response(effective)
        .with_environment_overrides(harness_env_override, update_env_override);
    response.revision = snapshot.revision;
    Ok(response)
}

pub(super) fn redact_config_args(values: Vec<String>) -> Vec<String> {
    let mut redacted = Vec::with_capacity(values.len());
    let mut redact_next = false;
    for value in values {
        if redact_next {
            redacted.push("[REDACTED]".to_owned());
            redact_next = false;
            continue;
        }

        if let Some((name, _)) = value.split_once('=') {
            if is_sensitive_config_value(name) {
                redacted.push(format!("{name}=[REDACTED]"));
                continue;
            }
        }

        if is_sensitive_config_value(&value) {
            if let Some((name, _)) = value.split_once('=') {
                if is_sensitive_config_value(name) {
                    redacted.push(format!("{name}=[REDACTED]"));
                } else {
                    redacted.push("[REDACTED]".to_owned());
                }
            } else if value.starts_with('-') {
                redacted.push(value);
                redact_next = true;
            } else {
                redacted.push("[REDACTED]".to_owned());
            }
        } else {
            redacted.push(value);
        }
    }
    redacted
}

fn is_sensitive_config_value(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    // Header-style inline credentials are values even when the option name is
    // innocuous (for example, `--header=Authorization: Bearer ...`).
    if lower.contains("bearer ") || lower.contains("authorization:") || lower.contains("cookie:") {
        return true;
    }
    let key = value
        .split_once('=')
        .map_or(value, |(name, _)| name)
        .trim_start_matches('-')
        .trim_matches(|character: char| !character.is_ascii_alphanumeric() && character != '_');
    let mut normalized = String::with_capacity(key.len() + 4);
    let mut previous_is_lower = false;
    for character in key.chars() {
        if character.is_ascii_uppercase() && previous_is_lower {
            normalized.push('_');
        }
        if character.is_ascii_alphanumeric() {
            normalized.push(character.to_ascii_lowercase());
            previous_is_lower = character.is_ascii_lowercase() || character.is_ascii_digit();
        } else {
            normalized.push('_');
            previous_is_lower = false;
        }
    }
    let segments = normalized.split('_').filter(|segment| !segment.is_empty());
    segments.clone().any(|segment| {
        matches!(
            segment,
            "password"
                | "passwd"
                | "secret"
                | "authorization"
                | "token"
                | "cookie"
                | "bearer"
                | "auth"
                | "apikey"
                | "key"
        )
    }) || ["access_token", "refresh_token", "api_key", "private_key"]
        .iter()
        .any(|marker| normalized == *marker)
}

/// Remove query/fragment credentials and userinfo before configuration is
/// returned to a UI. The on-disk config remains unchanged; this is a display
/// boundary only. Dropping the complete query is intentionally conservative:
/// a readiness URL is never a place where Nexus needs to preserve arguments.
pub(super) fn redact_config_url(value: Option<String>) -> Option<String> {
    let value = value?;
    let query_start = value.find(['?', '#']).unwrap_or(value.len());
    let base = &value[..query_start];
    let Some(scheme_end) = base.find("://") else {
        return Some(base.to_owned());
    };
    let authority_start = scheme_end + 3;
    let authority_end = base[authority_start..]
        .find('/')
        .map_or(base.len(), |offset| authority_start + offset);
    let authority = &base[authority_start..authority_end];
    let authority = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let mut sanitized = String::with_capacity(base.len());
    sanitized.push_str(&base[..authority_start]);
    sanitized.push_str(authority);
    sanitized.push_str(&base[authority_end..]);
    Some(sanitized)
}

fn transact_config_response(
    state: &AppState,
    expected: &str,
    update: impl FnOnce(&mut NexusConfigFile) -> io::Result<()>,
) -> axum::response::Response {
    match state.config.transaction_if_revision(expected, update) {
        Ok((document, ())) => match config_response_for_paths(&state.paths, document) {
            Ok(response) => (StatusCode::OK, Json(response)).into_response(),
            Err(error) => data_error_response(error, "config_unavailable"),
        },
        Err(error) => data_error_response(error, "config_write_failed"),
    }
}
