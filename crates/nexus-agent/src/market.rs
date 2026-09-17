//! Explicit, per-profile marketplace selection. No installation during startup.
use super::*;
use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Selection { profile: String, provider: String, scope: String }

fn read(home: &std::path::Path, profile: &str) -> io::Result<Value> {
    let directory = dsh::profile_directory(home, profile)?;
    let bytes = nexus_core::read_regular_file_bounded(&directory.join(".nexus-market.json"), 16384)?;
    let mut value = match bytes {
        Some(bytes) => serde_json::from_slice::<Value>(&bytes).map_err(io::Error::other)?,
        None => json!({"provider":"none", "status":"ready"}),
    };
    if !value.is_object() || !matches!(value["provider"].as_str(), Some("none" | "dsh-market"))
        || !matches!(value["status"].as_str(), Some("ready" | "pending" | "failed")) {
        return Err(io::Error::other("Invalid marketplace selection; configuration was not changed"));
    }
    value["profile"] = json!(profile);
    value["scope"] = json!(directory.to_string_lossy());
    value["installed"] = json!(dsh::native_profile(home, profile)?.bundles.iter().any(|p| p == "dshmarket"));
    Ok(value)
}

pub(crate) async fn status(State(state): State<AppState>) -> axum::response::Response {
    let _lifecycle = match try_read_lifecycle(&state) { Ok(g) => g, Err(r) => return r };
    let result = (|| { let home = state.snapshots.configured_dsh_home()?;
        read(&home, &state.profiles.load()?.active_profile) })();
    match result { Ok(v) => Json(v).into_response(), Err(e) => data_error_response(e, "market_unavailable") }
}

pub(crate) async fn select(state: State<AppState>, command: Json<Selection>) -> axum::response::Response {
    // Keep lifecycle ownership if the UI disconnects during package installation.
    match tokio::spawn(select_owned(state, command)).await {
        Ok(r) => r, Err(e) => data_error_response(io::Error::other(e), "market_failed"),
    }
}

async fn select_owned(State(state): State<AppState>, Json(command): Json<Selection>) -> axum::response::Response {
    if !matches!(command.provider.as_str(), "none" | "dsh-market") {
        return data_error_response(io::Error::other("Unknown marketplace provider"), "market_invalid");
    }
    let lifecycle = state.supervisor.acquire_lifecycle().await;
    if let Err(r) = ensure_checkpoint_mutation_ready(&state).await { return r; }
    if let Err(r) = ensure_harness_selection_quiescent(&state, &lifecycle, "market_busy", "Stop Harness before changing marketplace selection").await { return r; }
    let update = match state.updater.try_acquire_gate() { Ok(g) => g, Err(e) => return update_error_response(e) };
    if let Err(r) = ensure_update_idle(&state) { return r; }
    let configuration = match state.snapshots.try_acquire_configuration() { Ok(g) => g, Err(e) => return data_error_response(e, "market_busy") };
    let cold = match state.cold.try_acquire_maintenance() { Ok(g) => g, Err(e) => return data_error_response(e, "market_busy") };
    let result = tokio::task::spawn_blocking(move || -> io::Result<Value> {
        let _guards = (lifecycle, update, configuration, cold);
        if state.profiles.load()?.active_profile != command.profile { return Err(io::Error::other("Selected profile changed; refresh and retry")); }
        let home = state.snapshots.configured_dsh_home()?;
        let directory = dsh::profile_directory(&home, &command.profile)?;
        if directory.to_string_lossy() != command.scope { return Err(io::Error::other("Harness data directory changed; refresh and retry")); }
        let file = directory.join(".nexus-market.json");
        let write = |status: &str| nexus_core::write_private_bytes_atomic(&directory, &file,
            &serde_json::to_vec(&json!({"provider":command.provider, "status":status}))?);
        if command.provider == "none" {
            // Self-managed means no package removal or edits to third-party choices.
            write("ready")?;
            return read(&home, &command.profile);
        }
        let root = source_context::resolve(&state.paths, &state.releases)?.root
            .ok_or_else(|| io::Error::other("Select a built Harness before installing a marketplace"))?;
        write("pending")?;
        let outcome = dsh::install_market(&state.paths, &home, &root, &command.profile);
        match outcome {
            Ok((outcome, inventory)) if outcome.exit_code == Some(0) && inventory.bundles.iter().any(|p| p == "dshmarket") => {
                write("ready")?;
                read(&home, &command.profile)
            }
            Ok((outcome, _)) => {
                write("failed")?;
                Err(io::Error::other(format!("Marketplace installation failed ({:?}). {}\n{}", outcome.exit_code, outcome.stderr, outcome.stdout)))
            }
            Err(e) => { write("failed")?; Err(e) }
        }
    }).await;
    match result { Ok(Ok(v)) => Json(v).into_response(), Ok(Err(e)) => data_error_response(e, "market_failed"), Err(e) => data_error_response(io::Error::other(e), "market_failed") }
}
