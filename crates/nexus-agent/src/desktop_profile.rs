//! A plugin requests a switch; Agent retains ownership after that plugin stops.
use super::*;
use nexus_protocol::HarnessState;
use serde_json::{json, Value};
use std::{collections::HashSet, path::PathBuf, sync::{Mutex, OnceLock}};
static ACTIVE: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
fn active() -> &'static Mutex<HashSet<PathBuf>> { ACTIVE.get_or_init(Default::default) }
struct SwitchGuard(PathBuf);
impl Drop for SwitchGuard { fn drop(&mut self) { active().lock().unwrap().remove(&self.0); } }

pub(crate) async fn status(State(state): State<AppState>) -> axum::response::Response {
    let file = state.paths.run_dir.join("desktop-profile-switch.json");
    let result = (|| -> io::Result<Value> {
        let Some(bytes) = nexus_core::read_regular_file_bounded(&file, 16384)? else { return Ok(json!({"phase":"idle"})); };
        let mut value: Value = serde_json::from_slice(&bytes)?;
        if value["phase"] == "pending" && !active().lock().unwrap().contains(&file) { value["phase"] = json!("interrupted"); }
        Ok(value)
    })();
    match result { Ok(value) => Json(value).into_response(), Err(e) => data_error_response(e, "desktop_switch_unavailable") }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Selection { profile: String, run: String }

pub(crate) async fn select(State(state): State<AppState>, Json(command): Json<Selection>) -> axum::response::Response {
    if let Err(e) = nexus_core::validate_profile_name(&command.profile) { return data_error_response(e, "profile_invalid"); }
    let Some(lifecycle) = state.supervisor.try_acquire_lifecycle() else { return data_error_response(io::Error::other("Harness is busy"), "desktop_switch_busy"); };
    if let Err(response) = ensure_checkpoint_mutation_ready(&state).await { return response; }
    let (_, runtime, session) = state.supervisor.status_observation().await;
    if runtime.state != HarnessState::Running || command.run != session.run_id { return data_error_response(io::Error::other("The requesting Harness generation is no longer current"), "desktop_switch_stale"); }
    let update = match state.updater.try_acquire_gate() { Ok(g) => g, Err(e) => return update_error_response(e) };
    if let Err(response) = ensure_update_idle(&state) { return response; }
    let profiles = match state.snapshots.configured_dsh_home().and_then(|home| dsh::native_profiles(&home)) {
        Ok(profiles) if profiles.iter().any(|p| p.name == command.profile) => profiles,
        Ok(_) => return data_error_response(io::Error::other("Profile has no valid manifest"), "profile_invalid"),
        Err(e) => return data_error_response(e, "profile_invalid"),
    };
    let file = state.paths.run_dir.join("desktop-profile-switch.json");
    let id = match nexus_core::agent_auth::random_hex() { Ok(id) => id, Err(e) => return data_error_response(e, "desktop_switch_failed") };
    let record = json!({"id":id,"profile":command.profile,"phase":"pending"});
    if let Err(e) = nexus_core::write_private_bytes_atomic(&state.paths.root, &file, &serde_json::to_vec(&record).unwrap()) { return data_error_response(e, "desktop_switch_failed"); }
    active().lock().unwrap().insert(file.clone());
    let guard = SwitchGuard(file.clone());
    tokio::spawn(async move {
        let _owned = (guard, update);
        // Give the current plugin a chance to receive acceptance before teardown.
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        let result = async {
            state.supervisor.stop_locked(&lifecycle).await.map_err(|e| io::Error::other(e.to_string()))?;
            ensure_harness_selection_quiescent(&state, &lifecycle, "desktop_switch_busy", "Harness is still stopping").await
                .map_err(|_| io::Error::other("Harness is still owned; selection was preserved"))?;
            let catalog = nexus_core::ProfileCatalog::new(&command.profile, profiles.into_iter().map(|p| p.name).collect())?;
            state.profiles.write(&catalog)?;
            update_agent_state(&state, |current| current.set_profile(command.profile.clone())).await.map_err(|e| io::Error::other(e.to_string()))?;
            state.supervisor.begin_startup().await.map_err(|e| io::Error::other(e.to_string()))?;
            let start = async {
                let (report, prepared) = preflight::evaluate(state.clone()).await;
                if report["ready"] != true || report["paused"] == true { return Err(io::Error::other("Selected profile requires attention in Nexus startup checks")); }
                let prepared = prepared.ok_or_else(|| io::Error::other("Startup context unavailable"))?;
                state.supervisor.start_prepared(&command.profile, &lifecycle, prepared, false).await.map_err(|e| io::Error::other(e.to_string()))?;
                Ok::<_, io::Error>(())
            }.await;
            state.supervisor.finish_startup(start.is_ok(), false).await;
            start
        }.await;
        let _ = sync_harness_state(&state).await;
        let record = match result { Ok(()) => json!({"id":id,"profile":command.profile,"phase":"complete"}),
            Err(e) => json!({"id":id,"profile":command.profile,"phase":"failed","message":e.to_string()}) };
        let _ = nexus_core::write_private_bytes_atomic(&state.paths.root, &file, &serde_json::to_vec(&record).unwrap());
    });
    (StatusCode::ACCEPTED, Json(record)).into_response()
}
