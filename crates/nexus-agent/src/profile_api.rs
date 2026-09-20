//! Profile selection, plugins and terminal endpoints.

#[cfg(windows)]
use super::windows_terminal;
use super::{
    api_error_response, compatibility, data_error_response, dsh, ensure_checkpoint_mutation_ready,
    ensure_harness_selection_quiescent, ensure_harness_stopped, ensure_update_idle, io,
    profile_archive, settle_checkpoint_restore, source_context, try_read_lifecycle,
    update_agent_state, update_error_response, AppState, HarnessLaunchSpec, Json,
    PluginRemoveResponse, ProfileAction, ProfileCatalog, ProfileCommand, ProfileListResponse,
    ProfileOpenPathResponse, ProfileSelectResponse, State, StatusCode, DEFAULT_PROFILE,
};
use axum::response::IntoResponse;

pub(super) fn profile_list_response(
    state: &AppState,
    catalog: ProfileCatalog,
) -> io::Result<ProfileListResponse> {
    let home = state.snapshots.configured_dsh_home()?;
    let mut warnings = Vec::new();
    let mut manifests = dsh::native_profiles_with_warnings(&home, &mut warnings)?;
    for manifest in &mut manifests {
        manifest.order_undo_id = dsh::order_undo_id(&state.paths, &home, &manifest.name)?;
    }
    let names = manifests
        .iter()
        .map(|profile| profile.name.clone())
        .collect();
    let mut response =
        ProfileListResponse::new(catalog.active_profile, names).with_manifests(manifests);
    response.warnings = warnings;
    response.legacy_selected_source = compatibility::source_profile(&home, &response.active_profile).ok()
        .filter(|source| source != &response.active_profile);
    let retired = home.join(".nexus-retired-profiles");
    if std::fs::symlink_metadata(&retired).is_ok_and(|metadata| metadata.is_dir() && !nexus_core::path_is_reparse(&metadata)) {
        response.retired_profiles_directory = Some(retired.to_string_lossy().into_owned());
    }
    response.compatibility = compatibility::latest_for_selection(
        &state.paths,
        &state.snapshots.configured_dsh_home()?,
        &response.active_profile,
        source_context::compatibility_id(&state.paths, &state.releases)?.as_deref(),
    );
    let policy_profile = response
        .compatibility
        .as_ref()
        .map(|report| report.source_profile.as_str())
        .unwrap_or(&response.active_profile);
    response.disabled_plugins =
        compatibility::disabled_plugins(&state.snapshots.configured_dsh_home()?, policy_profile)?;
    Ok(response)
}

pub(super) async fn profile_list(State(state): State<AppState>) -> axum::response::Response {
    let _lifecycle = match try_read_lifecycle(&state) {
        Ok(guard) => guard,
        Err(response) => return response,
    };
    if let Err(error) = settle_checkpoint_restore(&state).await {
        return data_error_response(
            io::Error::other(error.to_string()),
            "checkpoint_recovery_failed",
        );
    }
    match state
        .profiles
        .load()
        .and_then(|catalog| profile_list_response(&state, catalog))
    {
        Ok(response) => (StatusCode::OK, Json(response)).into_response(),
        Err(error) => data_error_response(error, "profile_catalog_unavailable"),
    }
}

pub(super) async fn profile_control(
    state: State<AppState>,
    command: Json<ProfileCommand>,
) -> axum::response::Response {
    if matches!(
        command.0.action,
        ProfileAction::Select
            | ProfileAction::CompatibilityCheck
            | ProfileAction::Delete
            | ProfileAction::RestoreDeleted
    ) {
        // Selecting a profile now owns a long startup probe. Keep its lifecycle
        // and update gates until it finishes even if the caller disconnects.
        return match tokio::spawn(profile_control_inner(state, command)).await {
            Ok(response) => response,
            Err(error) => {
                data_error_response(io::Error::other(error.to_string()), "profile_owner_failed")
            }
        };
    }
    profile_control_inner(state, command).await
}

async fn profile_control_inner(
    State(state): State<AppState>,
    Json(command): Json<ProfileCommand>,
) -> axum::response::Response {
    match command.action {
        ProfileAction::DeletedList => {
            let _lifecycle = match try_read_lifecycle(&state) {
                Ok(guard) => guard,
                Err(response) => return response,
            };
            let result = state
                .snapshots
                .configured_dsh_home()
                .and_then(|home| profile_archive::list(&state.paths, &home));
            match result {
                Ok(value) => Json(value).into_response(),
                Err(error) => data_error_response(error, "profile_archive_unavailable"),
            }
        }
        ProfileAction::Delete | ProfileAction::RestoreDeleted => {
            let restore = command.action == ProfileAction::RestoreDeleted;
            let Some(target) = command.profile else {
                return data_error_response(
                    io::Error::other("Choose a profile"),
                    "profile_invalid",
                );
            };
            if command.package.is_some() || command.target.is_some() {
                return data_error_response(
                    io::Error::other("Unexpected profile archive parameters"),
                    "profile_invalid",
                );
            }
            let lifecycle = state.supervisor.acquire_lifecycle().await;
            if let Err(response) = ensure_checkpoint_mutation_ready(&state).await {
                return response;
            }
            if let Err(response) = ensure_harness_stopped(&state, &lifecycle).await {
                return response;
            }
            let update = match state.updater.try_acquire_gate() {
                Ok(guard) => guard,
                Err(error) => return update_error_response(error),
            };
            if let Err(response) = ensure_update_idle(&state) {
                return response;
            }
            let snapshots = match state.snapshots.try_acquire_configuration() {
                Ok(guard) => guard,
                Err(error) => return data_error_response(error, "profile_archive_conflict"),
            };
            let cold = match state.cold.try_acquire_maintenance() {
                Ok(guard) => guard,
                Err(error) => return data_error_response(error, "profile_archive_conflict"),
            };
            let home = match state.snapshots.configured_dsh_home() {
                Ok(home) => home,
                Err(error) => return data_error_response(error, "profile_archive_unavailable"),
            };
            let result = tokio::task::spawn_blocking(move || {
                let _guards = (lifecycle, update, snapshots, cold);
                profile_archive::recover_if_present(&state.paths, &state.profiles)?;
                profile_archive::change(&state.paths, &state.profiles, &home, &target, restore)
            })
            .await;
            match result {
                Ok(Ok(value)) => Json(value).into_response(),
                Ok(Err(error)) => data_error_response(error, "profile_archive_failed"),
                Err(error) => {
                    data_error_response(io::Error::other(error), "profile_archive_failed")
                }
            }
        }
        ProfileAction::List | ProfileAction::Status | ProfileAction::PluginInventory => {
            if command.profile.is_some() || command.package.is_some() {
                return data_error_response(
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "profile inventory actions do not accept parameters",
                    ),
                    "profile_invalid",
                );
            }
            profile_list(State(state)).await
        }
        ProfileAction::Select => {
            if command.package.is_some() {
                return data_error_response(
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "profile select does not accept a package",
                    ),
                    "profile_invalid",
                );
            }
            let Some(profile) = command.profile.as_deref() else {
                return data_error_response(
                    io::Error::new(io::ErrorKind::InvalidInput, "profile is required"),
                    "profile_invalid",
                );
            };
            let manifests = match dsh::native_profiles(&match state.snapshots.configured_dsh_home()
            {
                Ok(home) => home,
                Err(error) => return data_error_response(error, "profile_catalog_unavailable"),
            }) {
                Ok(manifests) => manifests,
                Err(error) => return data_error_response(error, "profile_catalog_unavailable"),
            };
            if !manifests.iter().any(|item| item.name == profile) {
                return data_error_response(
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        "profile has no valid native manifest",
                    ),
                    "profile_invalid",
                );
            }
            let lifecycle = state.supervisor.acquire_lifecycle().await;
            if let Err(response) = ensure_checkpoint_mutation_ready(&state).await {
                return response;
            }
            let _update_gate = match state.updater.try_acquire_gate() {
                Ok(gate) => gate,
                Err(error) => return update_error_response(error),
            };
            if let Err(response) = ensure_harness_selection_quiescent(
                &state,
                &lifecycle,
                "profile_change_conflict",
                "cannot switch profile until Harness is positively stopped and unowned",
            )
            .await
            {
                return response;
            }
            // Selection enables repair; actual startup owns runtime/plugin validation.
            let catalog = match ProfileCatalog::new(
                profile,
                manifests.iter().map(|item| item.name.clone()).collect(),
            )
            .and_then(|catalog| {
                state.profiles.write(&catalog)?;
                Ok(catalog)
            }) {
                Ok(catalog) => catalog,
                Err(error) => return data_error_response(error, "profile_invalid"),
            };
            let active_profile = catalog.active_profile.clone();
            let current = match update_agent_state(&state, |current| {
                current.set_profile(active_profile);
            })
            .await
            {
                Ok((current, _)) => current,
                Err(error) => {
                    return data_error_response(
                        io::Error::other(error.to_string()),
                        "profile_state_persistence_failed",
                    )
                }
            };
            (
                StatusCode::OK,
                Json(ProfileSelectResponse::selected(
                    current
                        .profile
                        .unwrap_or_else(|| DEFAULT_PROFILE.to_owned()),
                    catalog.profiles,
                )),
            )
                .into_response()
        }
        ProfileAction::PluginRemove => profile_plugin_remove(state, command).await,
        ProfileAction::CompatibilityCheck => profile_compatibility_check(state, command).await,
        ProfileAction::PluginMove | ProfileAction::PluginUndoMove => {
            profile_plugin_move(state, command).await
        }
        ProfileAction::PluginDisable | ProfileAction::PluginEnable => {
            profile_plugin_isolation(state, command).await
        }
        ProfileAction::OpenPath => profile_open_path(state, command).await,
        ProfileAction::OpenTerminal => profile_open_terminal(state, command).await,
        ProfileAction::Create => profile_create(state, command).await,
    }
}

async fn profile_compatibility_check(
    state: AppState,
    command: ProfileCommand,
) -> axum::response::Response {
    if command.package.is_some() || command.target.is_some() {
        return data_error_response(
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "plugin verification does not accept package or target",
            ),
            "profile_invalid",
        );
    }
    let lifecycle = state.supervisor.acquire_lifecycle().await;
    if let Err(response) = ensure_checkpoint_mutation_ready(&state).await {
        return response;
    }
    let _update = match state.updater.try_acquire_gate() {
        Ok(gate) => gate,
        Err(error) => return update_error_response(error),
    };
    let _cold = match state.cold.try_acquire_maintenance() {
        Ok(gate) => gate,
        Err(error) => return data_error_response(error, "profile_check_conflict"),
    };
    let _configuration = match state.snapshots.try_acquire_configuration() {
        Ok(gate) => gate,
        Err(error) => return data_error_response(error, "profile_check_conflict"),
    };
    if let Err(response) = ensure_harness_selection_quiescent(
        &state,
        &lifecycle,
        "profile_check_conflict",
        "stop Harness before verifying plugins",
    )
    .await
    {
        return response;
    }
    let result = async {
        let catalog = state.profiles.load()?;
        let home = state.snapshots.configured_dsh_home()?;
        let source = compatibility::source_profile(&home, &catalog.active_profile)?;
        if command
            .profile
            .as_deref()
            .is_some_and(|profile| profile != source)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "verify the currently selected source profile",
            ));
        }
        compatibility::check_selected(&state, &source).await?;
        profile_list_response(&state, catalog)
    }
    .await;
    match result {
        Ok(response) => (StatusCode::OK, Json(response)).into_response(),
        Err(error) => data_error_response(error, "profile_compatibility_failed"),
    }
}

async fn profile_plugin_isolation(
    state: AppState,
    command: ProfileCommand,
) -> axum::response::Response {
    let (Some(profile), Some(package)) = (command.profile.as_deref(), command.package.as_deref())
    else {
        return data_error_response(
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "profile and package are required",
            ),
            "profile_invalid",
        );
    };
    if command.target.is_some() {
        return data_error_response(
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "plugin isolation does not accept a target",
            ),
            "profile_invalid",
        );
    }
    let lifecycle = state.supervisor.acquire_lifecycle().await;
    if let Err(response) = ensure_checkpoint_mutation_ready(&state).await {
        return response;
    }
    let _update_gate = match state.updater.try_acquire_gate() {
        Ok(gate) => gate,
        Err(error) => return update_error_response(error),
    };
    if let Err(response) = ensure_harness_selection_quiescent(
        &state,
        &lifecycle,
        "plugin_isolation_conflict",
        "stop Harness before changing plugin isolation",
    )
    .await
    {
        return response;
    }
    let result = (|| -> io::Result<ProfileListResponse> {
        let home = state.snapshots.configured_dsh_home()?;
        let catalog = state.profiles.load()?;
        let source = compatibility::source_profile(&home, profile)?;
        let failed_target = compatibility::latest(&state.paths).is_some_and(|report| {
            report.status == "needs_choice"
                && report.trigger.as_deref() == Some("profile_switch")
                && report.source_profile == source
                && source_context::compatibility_id(&state.paths, &state.releases)
                    .ok()
                    .flatten()
                    .as_deref()
                    == Some(report.release_id.as_str())
        });
        if source != compatibility::source_profile(&home, &catalog.active_profile)?
            && !failed_target
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "plugin isolation must belong to the selected profile",
            ));
        }
        compatibility::set_plugin_disabled(
            &home,
            profile,
            package,
            command.action == ProfileAction::PluginDisable,
        )?;
        profile_list_response(&state, catalog)
    })();
    match result {
        Ok(response) => (StatusCode::OK, Json(response)).into_response(),
        Err(error) => data_error_response(error, "plugin_isolation_failed"),
    }
}

/// Create a new profile from the shipped `web` template. Metadata only:
/// the new profile is never selected and Harness is never restarted.
async fn profile_create(state: AppState, command: ProfileCommand) -> axum::response::Response {
    let lifecycle = state.supervisor.acquire_lifecycle().await;
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
    let _snapshot_gate = match state.snapshots.try_acquire_configuration() {
        Ok(gate) => gate,
        Err(error) => return data_error_response(error, "profile_change_conflict"),
    };
    if let Err(response) = ensure_harness_selection_quiescent(
        &state,
        &lifecycle,
        "profile_change_conflict",
        "Stop Harness before creating a profile",
    )
    .await
    {
        return response;
    }
    let Some(name) = command.profile.as_deref() else {
        return data_error_response(
            io::Error::new(io::ErrorKind::InvalidInput, "profile name is required"),
            "profile_name_required",
        );
    };
    let dsh_home = match state.snapshots.configured_dsh_home() {
        Ok(home) => home.clone(),
        Err(error) => return data_error_response(error, "dsh_home_unavailable"),
    };
    match state.profiles.create(name, &dsh_home) {
        Ok(catalog) => {
            let response =
                ProfileListResponse::new(catalog.active_profile.clone(), catalog.profiles.clone());
            (StatusCode::CREATED, Json(response)).into_response()
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            data_error_response(error, "profile_already_exists")
        }
        Err(error) => data_error_response(error, "profile_create_failed"),
    }
}

// The child cannot use the release until its durable lease has been published.
// An Agent crash before registration therefore closes an inert terminal.
const DSH_TERMINAL_WAIT: &str = r#"$wait = [Diagnostics.Stopwatch]::StartNew(); while (-not (Test-Path -LiteralPath $env:NEXUS_TERMINAL_READY -PathType Leaf)) { if ($wait.Elapsed.TotalSeconds -ge 10) { exit 1 }; Start-Sleep -Milliseconds 100 }; Remove-Item -LiteralPath $env:NEXUS_TERMINAL_READY -ErrorAction Stop; "#;
// Command text is fixed; environment data never becomes shell source.
const DSH_TERMINAL_INIT: &str = r#"function global:dsh { $launch = if ($env:NEXUS_TERMINAL_ARGS) { @(ConvertFrom-Json -InputObject $env:NEXUS_TERMINAL_ARGS) } else { @("--profile", $env:NEXUS_TERMINAL_PROFILE) }; & $env:NEXUS_TERMINAL_NODE $env:NEXUS_TERMINAL_ENTRY @launch @args }; function global:npm { & $env:NEXUS_TERMINAL_NODE $env:NEXUS_TERMINAL_NPM @args }; function global:pnpm { if ($env:NEXUS_TERMINAL_PNPM_SCRIPT -eq '1') { & $env:NEXUS_TERMINAL_NODE $env:NEXUS_TERMINAL_PNPM @args } else { & $env:NEXUS_TERMINAL_PNPM @args } }"#;

/// Open an interactive terminal prepared for working with the selected
/// profile: the shell starts in the profile directory with `DSH_HOME` set,
/// resolved tools on `PATH`, and per-session `dsh`/`pnpm` functions.
async fn profile_open_terminal(
    state: AppState,
    command: ProfileCommand,
) -> axum::response::Response {
    let _lifecycle = state.supervisor.acquire_lifecycle().await;
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
    let dsh_home = match state.snapshots.configured_dsh_home() {
        Ok(home) => home.clone(),
        Err(error) => return data_error_response(error, "dsh_home_unavailable"),
    };
    let profiles = match state.profiles.load() {
        Ok(catalog) => catalog,
        Err(error) => return data_error_response(error, "profile_catalog_unavailable"),
    };
    let profile = command
        .profile
        .clone()
        .unwrap_or_else(|| profiles.active_profile.clone());
    // The name becomes a path segment and lands in generated cmd shims;
    // only the validated character set is accepted.
    if let Err(error) = nexus_core::validate_profile_name(&profile) {
        return data_error_response(error, "profile_invalid");
    }
    let profile_dir = dsh_home.join("profiles").join(&profile);
    if !profile_dir.is_dir() {
        return data_error_response(
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("profile directory not found: {}", profile_dir.display()),
            ),
            "profile_dir_missing",
        );
    }
    let source = match source_context::resolve_async(&state.paths, &state.releases).await {
        Ok(s) => s,
        Err(e) => return data_error_response(e, "source_invalid"),
    };
    let release_id = source
        .release_id
        .unwrap_or_else(|| "external-harness".into());
    let Some(release_root) = source.root else {
        return data_error_response(
            io::Error::other("Select a Harness source"),
            "release_none_current",
        );
    };
    let entry = release_root
        .join("apps")
        .join("cli")
        .join("lib")
        .join("bin.js");
    if !entry.is_file() {
        return data_error_response(
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("release CLI entry is missing: {}", entry.display()),
            ),
            "release_entry_missing",
        );
    }
    let runtime = match crate::cold::resolved_runtime_config(&state).await {
        Ok(runtime) => runtime,
        Err(error) => return data_error_response(error, "runtime_unavailable"),
    };
    let Some(node) = runtime.node.as_ref().map(|pin| pin.path.clone()) else {
        return data_error_response(
            io::Error::new(io::ErrorKind::NotFound, "no usable node runtime"),
            "node_missing",
        );
    };
    let preferences = match nexus_core::load_harness_preferences(&state.paths) {
        Ok(p) => p,
        Err(e) => return data_error_response(e, "preferences_invalid"),
    };
    if let Err(e) = crate::runtime_patches::validate_for_paths(&state.paths, &preferences) {
        return data_error_response(e, "patch_invalid");
    }
    let capabilities = match crate::preference_capabilities::resolve(
        Some(&release_root),
        &dsh_home,
        &profile,
        &preferences,
    ) {
        Ok(c) => c,
        Err(e) => return data_error_response(e, "preferences_invalid"),
    };
    let mut terminal_spec = HarnessLaunchSpec::new(node.clone());
    terminal_spec.mode = nexus_protocol::HarnessLaunchMode::Node;
    terminal_spec.args = vec![
        entry.to_string_lossy().into_owned(),
        "--profile".into(),
        profile.clone(),
    ];
    nexus_core::apply_harness_preferences(&mut terminal_spec, &preferences, &capabilities);
    #[cfg(windows)]
    let terminal_args =
        serde_json::to_string(&terminal_spec.args[1..]).expect("terminal args serialization");
    let pnpm_pin = runtime.pnpm.as_ref().map(|pin| pin.path.clone());
    let pnpm_is_script = pnpm_pin.as_ref().is_some_and(|path| {
        path.extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| {
                matches!(
                    extension.to_ascii_lowercase().as_str(),
                    "js" | "cjs" | "mjs"
                )
            })
    });
    let envs =
        match nexus_core::build_runtime_child_env(&runtime, std::env::var_os("PATH").as_deref()) {
            Ok(envs) => envs,
            Err(error) => return data_error_response(error, "runtime_env_unavailable"),
        };
    #[cfg(windows)]
    {
        let powershell = std::env::var_os("SystemRoot")
            .map(|root| {
                std::path::PathBuf::from(root)
                    .join("System32")
                    .join("WindowsPowerShell")
                    .join("v1.0")
                    .join("powershell.exe")
            })
            .filter(|path| path.is_file())
            .unwrap_or_else(|| std::path::PathBuf::from("powershell.exe"));
        let ready = state.paths.run_dir.join(format!(
            "terminal-start-{}.ready",
            match nexus_core::agent_auth::random_hex() {
                Ok(id) => id,
                Err(error) => return data_error_response(error, "terminal_preparation_failed"),
            }
        ));
        let notification_monitor = command.target.as_deref() == Some("notifications");
        let monitor = state.paths.run_dir.join("notification-terminal.mjs");
        if notification_monitor {
            if let Err(e) = nexus_core::write_private_bytes_atomic(&state.paths.root, &monitor, include_bytes!("../../../plugins/nexus-notifications/terminal/monitor.mjs")) {
                return data_error_response(e, "notification_terminal_failed");
            }
        }
        let mut command = std::process::Command::new(powershell);
        command.env("NEXUS_NOTIFICATION_MONITOR", nexus_core::node_script_argument(&monitor));
        command.env("NEXUS_NOTIFICATION_FILE", nexus_core::node_script_argument(&state.paths.run_dir.join("notifications.json")));
        command.env("NEXUS_NOTIFICATION_SETTINGS", nexus_core::node_script_argument(&state.paths.root.join("notification-settings.json")));
        command
            .args(["-NoLogo", "-NoProfile", "-NoExit", "-Command"])
            .arg(if notification_monitor {
                format!("{DSH_TERMINAL_WAIT}& $env:NEXUS_TERMINAL_NODE $env:NEXUS_NOTIFICATION_MONITOR $env:NEXUS_NOTIFICATION_FILE $env:NEXUS_NOTIFICATION_SETTINGS")
            } else { format!("{DSH_TERMINAL_WAIT}{DSH_TERMINAL_INIT}") })
            .current_dir(&profile_dir);
        for (key, value) in nexus_core::harness_preferences_environment(&preferences, &capabilities)
        {
            command.env(key, value);
        }
        command.env("NEXUS_TERMINAL_ARGS", &terminal_args);
        for (key, value) in &envs {
            command.env(key, value);
        }
        command
            .env("DSH_HOME", &dsh_home)
            .env("NEXUS_TERMINAL_READY", &ready)
            .env("NEXUS_TERMINAL_NODE", &node)
            .env(
                "NEXUS_TERMINAL_ENTRY",
                nexus_core::node_script_argument(&entry),
            )
            .env(
                "NEXUS_TERMINAL_NPM",
                nexus_core::node_script_argument(
                    &node
                        .parent()
                        .expect("resolved node has parent")
                        .join("node_modules/npm/bin/npm-cli.js"),
                ),
            )
            .env("NEXUS_TERMINAL_PROFILE", &profile)
            .env(
                "NEXUS_TERMINAL_PNPM",
                pnpm_pin.as_deref().unwrap_or(std::path::Path::new("")),
            )
            .env(
                "NEXUS_TERMINAL_PNPM_SCRIPT",
                if pnpm_is_script { "1" } else { "0" },
            );
        // Give the new console its own handles, not the headless Agent's pipes.
        let mut child = match windows_terminal::spawn(&command, true) {
            Ok(child) => child,
            Err(error) => return data_error_response(error, "terminal_spawn_failed"),
        };
        let lease =
            match nexus_core::terminal_lease::register(&state.paths, &release_id, child.id()) {
                Ok(lease) => lease,
                Err(error) => {
                    let _ = child.kill();
                    // wait polls the console handle for up to 5 seconds; keep
                    // that off the async worker threads.
                    let _ = tokio::task::spawn_blocking(move || child.wait()).await;
                    return data_error_response(error, "terminal_registration_failed");
                }
            };
        if let Err(error) =
            nexus_core::write_private_bytes_atomic(&state.paths.root, &ready, b"ready")
        {
            let _ = child.kill();
            let _ = tokio::task::spawn_blocking(move || child.wait()).await;
            let _ = std::fs::remove_file(&lease);
            return data_error_response(error, "terminal_registration_failed");
        }
        tokio::spawn(async move {
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => {
                        let _ = std::fs::remove_file(&lease);
                        let _ = std::fs::remove_file(&ready);
                        break;
                    }
                    Err(_) => break,
                    Ok(None) => tokio::time::sleep(std::time::Duration::from_secs(1)).await,
                }
            }
        });
    }
    #[cfg(target_os = "macos")]
    {
        let mut terminal_env = envs;
        terminal_env.extend(nexus_core::harness_preferences_environment(&preferences, &capabilities));
        terminal_env.push(("DSH_HOME".into(), dsh_home.as_os_str().to_owned()));
        let options = crate::macos_terminal::Options {
            node: &node, entry: &entry, args: &terminal_spec.args[1..],
            pnpm: pnpm_pin.as_deref(), pnpm_is_script, profile_dir: &profile_dir,
            env: &terminal_env, notification_monitor: command.target.as_deref() == Some("notifications"),
        };
        if let Err(error) = crate::macos_terminal::open(&state.paths, &release_id, &options).await {
            return data_error_response(error, "terminal_spawn_failed");
        }
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        // This terminal action requires a platform terminal emulator.
        // An invisible shell would not provide the requested interactive UI.
        return data_error_response(
            io::Error::new(
                io::ErrorKind::Unsupported,
                "DSH terminal supports Windows and macOS",
            ),
            "terminal_unsupported",
        );
    }
    #[cfg(any(windows, target_os = "macos"))]
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "status": "ok",
            "profile": profile,
            "profile_dir": profile_dir.to_string_lossy(),
            "release_id": release_id,
        })),
    )
        .into_response()
}

/// Open a bounded profile-related file or directory with the system handler.
/// Targets derive only from the DSH home and the validated profile name; no
/// caller-supplied path is accepted.
async fn profile_open_path(state: AppState, command: ProfileCommand) -> axum::response::Response {
    let Some(target) = command.target.as_deref() else {
        return data_error_response(
            io::Error::new(io::ErrorKind::InvalidInput, "target is required"),
            "open_path_target_required",
        );
    };
    // Repair entry points must remain usable even when the profile catalog is
    // unreadable. Resolve only fixed, Agent-owned roots, never client paths.
    if target == "nexus_data" {
        return open_resolved_profile_path(target, state.paths.root.clone(), true);
    }
    let dsh_home = match state.snapshots.configured_dsh_home() {
        Ok(home) => home.clone(),
        Err(error) => return data_error_response(error, "dsh_home_unavailable"),
    };
    if target == "harness_data" {
        return open_resolved_profile_path(target, dsh_home, true);
    }
    let profiles = match state.profiles.load() {
        Ok(catalog) => catalog,
        Err(error) => return data_error_response(error, "profile_catalog_unavailable"),
    };
    let profile = command
        .profile
        .clone()
        .unwrap_or_else(|| profiles.active_profile.clone());
    // The name becomes a path segment under the profiles root; reject
    // traversal and other unsafe characters up front.
    if let Err(error) = nexus_core::validate_profile_name(&profile) {
        return data_error_response(error, "profile_invalid");
    }
    let profile_dir = dsh_home.join("profiles").join(&profile);
    if !profile_dir.is_dir() && target != "retired_profiles" {
        return data_error_response(
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("profile directory not found: {}", profile_dir.display()),
            ),
            "profile_dir_missing",
        );
    }
    let (path, open_dir) = match target {
        "retired_profiles" => {
            let path = dsh_home.join(".nexus-retired-profiles");
            if !std::fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.is_dir() && !nexus_core::path_is_reparse(&metadata)) {
                return data_error_response(io::Error::other("Retired profile directory is unavailable"), "open_path_missing");
            }
            (path, true)
        }
        "settings" => (dsh_home.join("settings.yaml"), false),
        "profile_dir" => (profile_dir.clone(), true),
        "profile_patch" => (profile_dir.join("cordis.patch.yml"), false),
        "plugin_manifest" => (profile_dir.join("package.json"), false),
        other => {
            return data_error_response(
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown open target: {other}"),
                ),
                "open_path_target_invalid",
            );
        }
    };
    open_resolved_profile_path(target, path, open_dir)
}

fn open_resolved_profile_path(target: &str, path: std::path::PathBuf, open_dir: bool) -> axum::response::Response {
    if !path.exists() {
        return data_error_response(
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("path not found: {}", path.display()),
            ),
            "open_path_missing",
        );
    }
    #[cfg(windows)]
    if !open_dir && path.as_os_str().to_string_lossy().contains('%') {
        // The file branch launches through `cmd /C start`, which expands
        // %VAR% even inside quoted arguments. The path derives from the
        // profile directory, but refuse percent characters so a crafted
        // dsh_home cannot expand to a different target.
        return data_error_response(
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "the path contains a percent character",
            ),
            "open_path_invalid",
        );
    }
    #[cfg(windows)]
    let opened = {
        use std::os::windows::process::CommandExt;
        // `explorer` opens directories in a window; for files it selects them
        // in the parent. `start` opens files with the default association.
        let output = if open_dir {
            std::process::Command::new("explorer")
                .arg(path.as_os_str())
                .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
                .output()
        } else {
            std::process::Command::new("cmd")
                .args(["/C", "start", ""])
                .arg(path.as_os_str())
                .creation_flags(0x0800_0000)
                .output()
        };
        output.map(|out| out.status.success()).unwrap_or(false)
    };
    #[cfg(target_os = "macos")]
    let opened = std::process::Command::new("/usr/bin/open")
        .arg(path.as_os_str()).status().map(|status| status.success()).unwrap_or(false);
    #[cfg(not(any(windows, target_os = "macos")))]
    let opened = {
        std::process::Command::new("xdg-open")
            .arg(path.as_os_str())
            .output()
            .map(|out| out.status.success())
            .unwrap_or(false)
    };
    if !opened {
        return data_error_response(
            io::Error::other("the system handler did not accept the path"),
            "open_path_failed",
        );
    }
    (
        StatusCode::OK,
        Json(ProfileOpenPathResponse::new(
            target.to_owned(),
            path.to_string_lossy().into_owned(),
        )),
    )
        .into_response()
}

async fn profile_plugin_move(state: AppState, command: ProfileCommand) -> axum::response::Response {
    let Some(profile) = command.profile else {
        return data_error_response(
            io::Error::other("Profile is required"),
            "plugin_move_invalid",
        );
    };
    let undo = command.action == ProfileAction::PluginUndoMove;
    if (!undo && command.package.is_none())
        || (undo && (command.target.is_none() || command.package.is_some()))
    {
        return data_error_response(
            io::Error::other("Reorder requires package; undo requires its saved operation ID"),
            "plugin_move_invalid",
        );
    }
    let lifecycle = state.supervisor.acquire_lifecycle().await;
    if let Err(response) = ensure_checkpoint_mutation_ready(&state).await {
        return response;
    }
    let update = match state.updater.try_acquire_gate() {
        Ok(gate) => gate,
        Err(error) => return update_error_response(error),
    };
    if let Err(response) = ensure_update_idle(&state) {
        return response;
    }
    let snapshots = match state.snapshots.try_acquire_configuration() {
        Ok(gate) => gate,
        Err(error) => return data_error_response(error, "plugin_move_conflict"),
    };
    if let Err(response) = ensure_harness_selection_quiescent(
        &state,
        &lifecycle,
        "plugin_move_conflict",
        "stop Harness before changing plugin load order",
    )
    .await
    {
        return response;
    }
    let home = match state.snapshots.configured_dsh_home() {
        Ok(home) => home,
        Err(error) => return data_error_response(error, "profile_catalog_unavailable"),
    };
    let paths = state.paths.clone();
    let result = tokio::task::spawn_blocking(move || {
        let _owners = (lifecycle, update, snapshots);
        if undo {
            dsh::undo_profile_order(&paths, &home, &profile, command.target.as_deref().unwrap())
        } else {
            dsh::move_profile_plugin(
                &paths,
                &home,
                &profile,
                command.package.as_deref().unwrap(),
                command.target.as_deref(),
            )
        }
    })
    .await
    .unwrap_or_else(|error| Err(io::Error::other(error.to_string())));
    match result {
        Ok(inventory) => (StatusCode::OK, Json(inventory)).into_response(),
        Err(error) => data_error_response(error, "plugin_move_failed"),
    }
}

async fn profile_plugin_remove(
    state: AppState,
    command: ProfileCommand,
) -> axum::response::Response {
    let Some(profile) = command.profile else {
        return data_error_response(
            io::Error::new(io::ErrorKind::InvalidInput, "profile is required"),
            "plugin_remove_invalid",
        );
    };
    let Some(package) = command.package else {
        return data_error_response(
            io::Error::new(io::ErrorKind::InvalidInput, "package is required"),
            "plugin_remove_invalid",
        );
    };
    let catalog = match state.profiles.load() {
        Ok(catalog) => catalog,
        Err(error) => return data_error_response(error, "profile_catalog_unavailable"),
    };
    if catalog.active_profile != profile {
        return api_error_response(
            StatusCode::CONFLICT,
            "plugin_profile_conflict",
            "plugins can be removed only from the selected profile",
        );
    }
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
        "plugin_remove_conflict",
        "cannot remove a plugin until Harness is positively stopped and unowned",
    )
    .await
    {
        return response;
    }
    let release_id = match state.releases.load() {
        Ok(catalog) => match catalog.current_release {
            Some(id) => id,
            None => {
                return data_error_response(
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        "no verified DSH release is selected",
                    ),
                    "plugin_cli_unavailable",
                )
            }
        },
        Err(error) => return data_error_response(error, "release_catalog_unavailable"),
    };
    let release_root = match state.releases.release_root(&release_id) {
        Ok(root) => root,
        Err(error) => return data_error_response(error, "plugin_cli_unavailable"),
    };
    let paths = state.paths.clone();
    let dsh_home = match state.snapshots.configured_dsh_home() {
        Ok(home) => home.clone(),
        Err(error) => return data_error_response(error, "profile_catalog_unavailable"),
    };
    let owner = tokio::spawn(async move {
        let _lifecycle = lifecycle;
        let _update_gate = update_gate;
        tokio::task::spawn_blocking(move || {
            dsh::remove_profile_plugin(&paths, &dsh_home, &release_root, &profile, &package).map(
                |(outcome, inventory)| {
                    let removed = outcome.exit_code == Some(0)
                        && !inventory.plugins.iter().any(|item| item.package == package);
                    PluginRemoveResponse {
                        api_version: nexus_protocol::API_VERSION.to_owned(),
                        profile,
                        package,
                        removed,
                        exit_code: outcome.exit_code,
                        stdout: outcome.stdout,
                        stderr: outcome.stderr,
                        inventory,
                    }
                },
            )
        })
        .await
        .map_err(|error| io::Error::other(format!("plugin removal task failed: {error}")))?
    });
    match owner.await {
        Ok(Ok(response)) => (StatusCode::OK, Json(response)).into_response(),
        Ok(Err(error)) => data_error_response(error, "plugin_remove_failed"),
        Err(error) => data_error_response(
            io::Error::other(format!("plugin removal owner failed: {error}")),
            "plugin_remove_failed",
        ),
    }
}
