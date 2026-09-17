use std::{fs, io::{self, Read}};
use nexus_core::{NexusPaths, HarnessLaunchSpec};
use serde_json::{json, Value};
use nexus_protocol::HarnessState;
use super::*;

pub(crate) fn preferences(paths: &NexusPaths) -> Value {
    let file = paths.root.join("notification-settings.json");
    if !fs::symlink_metadata(&file).is_ok_and(|m| m.is_file() && !nexus_core::path_is_reparse(&m) && m.len() <= 16384) { return json!({}); }
    fs::read(file).ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_else(|| json!({}))
}
pub(crate) async fn configure(State(state): State<AppState>, Json(value): Json<Value>) -> axum::response::Response {
    let valid = value.as_object().is_some_and(|o| o.keys().all(|k| ["desktop","terminal","method","categories","observer"].contains(&k.as_str())))
        && (value.get("observer").is_none() || value["observer"].is_boolean())
        && ["desktop", "terminal"].iter().all(|key| matches!(value[*key].as_str(), Some("off" | "unfocused" | "always")))
        && matches!(value["method"].as_str(), Some("auto" | "osc9" | "bel"))
        && value["categories"].as_object().is_some_and(|o| o.len() == 9 && o.iter().all(|(k,v)| v.is_boolean() && ["completed","failed","approval","question","blocked","job-completed","job-failed","harness-failed","update-ready"].contains(&k.as_str())));
    if !valid { return data_error_response(io::Error::other("Invalid notification settings"), "notifications_invalid"); }
    match nexus_core::write_private_bytes_atomic(&state.paths.root, &state.paths.root.join("notification-settings.json"), &serde_json::to_vec(&value).unwrap()) {
        Ok(()) => Json(value).into_response(),
        Err(e) => data_error_response(e, "notifications_save_failed"),
    }
}

pub(crate) fn prepare(paths: &NexusPaths, spec: &mut HarnessLaunchSpec) -> io::Result<bool> {
    if preferences(paths)["observer"] == false { return Ok(false); }
    if !nexus_core::harness_preferences_cli_supported(spec) { return Ok(false); }
    let module = paths.run_dir.join("plugins/nexus-notifications/index.mjs");
    fs::create_dir_all(module.parent().unwrap())?;
    nexus_core::write_private_bytes_atomic(&paths.root, &module, include_bytes!("../../../plugins/nexus-notifications/src/index.mjs"))?;
    let patch = paths.run_dir.join("notification-plugin.json");
    // ESM imports require a file URL on Windows, with URL metacharacters escaped.
    let body = json!([{ "insert": [{ "id": "nexus-notifications", "name": crate::desktop_plugins::module_url(&module)? }] }]);
    nexus_core::write_private_bytes_atomic(&paths.root, &patch, &serde_json::to_vec(&body)?)?;
    let start = usize::from(spec.mode == nexus_protocol::HarnessLaunchMode::Node);
    spec.args.splice(start..start, ["--patch".into(), patch.to_string_lossy().into_owned()]);
    Ok(true)
}

pub(crate) async fn status(State(state): State<AppState>) -> axum::response::Response {
    let (_, runtime, session) = state.supervisor.status_observation().await;
    let read = || -> io::Result<Value> {
        let file = state.paths.run_dir.join("notifications.json");
        let meta = fs::symlink_metadata(&file)?;
        if !meta.is_file() || nexus_core::path_is_reparse(&meta) || meta.len() > 262144 { return Err(io::Error::other("Invalid notification snapshot")); }
        let mut bytes = Vec::new();
        fs::File::open(file)?.take(262145).read_to_end(&mut bytes)?;
        if bytes.len() > 262144 { return Err(io::Error::other("Notification snapshot too large")); }
        let value: Value = serde_json::from_slice(&bytes)?;
        if !value.is_object() || !value["events"].as_array().is_some_and(|v| v.len() <= 128) || !value["epoch"].is_string() || !value["sequence"].is_u64() {
            return Err(io::Error::other("Invalid notification snapshot"));
        }
        Ok(value)
    };
    match read() {
        Ok(mut value) if runtime.state == HarnessState::Running && value["run"].as_str() == Some(session.run_id.as_str()) => { value["settings"] = preferences(&state.paths); Json(value).into_response() },
        _ => Json(json!({ "settings": preferences(&state.paths), "capabilities": [], "events": [], "sequence": 0 })).into_response(),
    }
}
