//! Offline, bounded profile-file repair. No plugin execution or package downloads.
use std::{fs, io, path::{Path, PathBuf}};
use axum::{extract::State, Json, response::IntoResponse};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use nexus_core::NexusPaths;
const FILES: [&str; 2] = ["package.json", "cordis.patch.yml"];
const LIMIT: u64 = 32 * 1024;
fn invalid(message: &str) -> io::Error { io::Error::new(io::ErrorKind::InvalidInput, message) }
fn read_file(dir: &Path, file: &str) -> io::Result<Option<String>> {
    if !FILES.contains(&file) { return Err(invalid("Unsupported profile repair file")); }
    nexus_core::read_regular_file_bounded(&dir.join(file), LIMIT)?.map(|bytes| String::from_utf8(bytes).map_err(io::Error::other)).transpose()
}
fn identity(value: &Option<String>) -> String {
    format!("{:x}", Sha256::digest(serde_json::to_vec(value).unwrap()))
}
fn validate(file: &str, text: &str) -> io::Result<()> {
    if text.len() as u64 > LIMIT { return Err(invalid("Profile file exceeds repair limit")); }
    if file == "package.json" {
        let value: Value = serde_json::from_str(text).map_err(io::Error::other)?;
        if !value.pointer("/dsh/profile/bundles").and_then(Value::as_array)
            .is_some_and(|items| items.iter().all(|item| item.as_str().is_some_and(|s| !s.trim().is_empty()))) {
            return Err(invalid("package.json requires dsh.profile.bundles as an array of package names"));
        }
        for name in ["dependencies", "devDependencies", "optionalDependencies"] {
            if let Some(value) = value.get(name) {
                if !value.as_object().is_some_and(|entries| entries.values().all(Value::is_string)) {
                    return Err(invalid("Package dependency declarations must map names to strings"));
                }
            }
        }
    } else if file == "cordis.patch.yml" {
        let value: serde_yaml_ng::Value = serde_yaml_ng::from_str(text).map_err(io::Error::other)?;
        if !value.is_sequence() && !value.is_null() { return Err(invalid("Profile patch must be a YAML sequence")); }
    } else { return Err(invalid("Unsupported profile repair file")); }
    Ok(())
}
#[derive(Serialize, Deserialize)]
struct Backup { data_root_id: String, profile: String, home: PathBuf, file: String, content: Option<String>, created: u64 }
fn backup(paths: &NexusPaths, home: &Path, profile: &str, file: &str, content: Option<String>) -> io::Result<String> {
    let history = paths.root.join("profile-repair-history");
    match fs::symlink_metadata(&history) {
        Ok(meta) if meta.is_dir() && !nexus_core::path_is_reparse(&meta) => (),
        Ok(_) => return Err(invalid("Repair history must be an ordinary directory")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => fs::create_dir(&history)?,
        Err(error) => return Err(error),
    }
    let id = nexus_core::agent_auth::random_hex()?;
    let record = Backup { data_root_id: nexus_core::data_root_identity(paths)?, profile: profile.into(), home: fs::canonicalize(home)?, file: file.into(), content,
        created: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs() };
    nexus_core::write_private_json_atomic(&paths.root, &paths.root.join("profile-repair-history").join(format!("{id}.json")), &record)?;
    Ok(id)
}
pub(crate) fn backup_configuration(paths: &NexusPaths, home: &Path, profile: &str) -> io::Result<()> {
    let dir = super::dsh::profile_directory(home, profile)?;
    for file in FILES { backup(paths, home, profile, file, read_file(&dir, file)?)?; }
    Ok(())
}
fn inspect(paths: &NexusPaths, home: &Path, profile: &str) -> io::Result<Value> {
    let dir = super::dsh::profile_directory(home, profile)?;
    let mut files = Vec::new();
    for file in FILES {
        let content = read_file(&dir, file)?;
        let error = match &content {
            Some(text) => validate(file, text).err().map(|e| e.to_string()),
            None if file == "package.json" => Some("package.json is missing".into()),
            None => None,
        };
        files.push(json!({"file":file,"content":content,"fingerprint":identity(&content),"error":error}));
    }
    let history = paths.root.join("profile-repair-history");
    let mut backups = Vec::new();
    if history.exists() {
        if nexus_core::path_is_reparse(&fs::symlink_metadata(&history)?) { return Err(invalid("Repair history cannot be linked")); }
        let canonical_home = fs::canonicalize(home)?;
        for entry in fs::read_dir(history)?.take(4096) {
            let entry = entry?;
            if entry.path().extension().and_then(|v| v.to_str()) != Some("json") { continue; }
            let Some(bytes) = nexus_core::read_regular_file_bounded(&entry.path(), LIMIT * 8)? else { continue };
            let Ok(record) = serde_json::from_slice::<Backup>(&bytes) else { continue };
            if record.data_root_id == nexus_core::data_root_identity(paths)? && record.profile == profile && record.home == canonical_home {
                backups.push(json!({"id":entry.path().file_stem().unwrap().to_string_lossy(),"file":record.file,"created":record.created}));
            }
        }
    }
    backups.sort_by_key(|item| std::cmp::Reverse(item["created"].as_u64())); backups.truncate(40);
    Ok(json!({"profile":profile,"files":files,"backups":backups,"startup_verified":false}))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Command { action: String, profile: Option<String>, file: Option<String>, content: Option<String>, fingerprint: Option<String>, backup: Option<String> }
fn control(paths: &NexusPaths, home: &Path, command: Command) -> io::Result<Value> {
    if command.action == "list" {
        let mut profiles = Vec::new();
        let parent = home.join("profiles");
        if parent.exists() {
            for entry in fs::read_dir(parent)?.take(257) {
                let name = entry?.file_name().to_string_lossy().into_owned();
                if name == "node_modules" || name.starts_with('.') { continue; }
                if super::dsh::profile_directory(home, &name).is_ok() { profiles.push(name); }
            }
        }
        profiles.sort(); return Ok(json!({"profiles":profiles}));
    }
    let profile = command.profile.ok_or_else(|| invalid("Choose a repair target"))?;
    let dir = super::dsh::profile_directory(home, &profile)?;
    if command.action == "inspect" { return inspect(paths, home, &profile); }
    if !matches!(command.action.as_str(), "save" | "restore") { return Err(invalid("Unknown profile repair operation")); }
    let file = command.file.ok_or_else(|| invalid("Choose a configuration file"))?;
    let original = read_file(&dir, &file)?;
    if command.fingerprint.as_deref() != Some(identity(&original).as_str()) { return Err(invalid("Configuration changed; inspect again before saving")); }
    let next = if command.action == "restore" {
        let id = command.backup.ok_or_else(|| invalid("Choose a recovery point"))?;
        if id.is_empty() || !id.bytes().all(|b| b.is_ascii_hexdigit()) || id.len() > 128 { return Err(invalid("Invalid recovery point")); }
        let history = paths.root.join("profile-repair-history");
        let meta = fs::symlink_metadata(&history)?;
        if !meta.is_dir() || nexus_core::path_is_reparse(&meta) { return Err(invalid("Repair history must be an ordinary directory")); }
        let bytes = nexus_core::read_regular_file_bounded(&history.join(format!("{id}.json")), LIMIT * 8)?.ok_or_else(|| invalid("Recovery point missing"))?;
        let record: Backup = serde_json::from_slice(&bytes)?;
        if record.data_root_id != nexus_core::data_root_identity(paths)? || record.profile != profile || record.home != fs::canonicalize(home)? || record.file != file { return Err(invalid("Recovery point belongs to another target")); }
        record.content
    } else {
        let text = command.content.ok_or_else(|| invalid("Configuration text missing"))?;
        validate(&file, &text)?; Some(text)
    };
    if next.as_ref().is_some_and(|text| text.len() as u64 > LIMIT) { return Err(invalid("Recovery point exceeds repair limit")); }
    let saved = backup(paths, home, &profile, &file, original.clone())?;
    if read_file(&dir, &file)? != original { return Err(invalid("Configuration changed during backup; nothing overwritten")); }
    if let Some(text) = &next { nexus_core::write_private_bytes_atomic(&dir, &dir.join(&file), text.as_bytes())?; }
    else if original.is_some() { fs::remove_file(dir.join(&file))?; }
    if read_file(&dir, &file)? != next { return Err(invalid("Configuration read-back verification failed")); }
    let mut result = inspect(paths, home, &profile)?; result["saved_backup"] = saved.into();
    Ok(result)
}
pub(crate) async fn handle(State(state): State<super::AppState>, Json(command): Json<Command>) -> axum::response::Response {
    let lifecycle = state.supervisor.acquire_lifecycle().await;
    if let Err(response) = super::ensure_checkpoint_mutation_ready(&state).await { return response; }
    if let Err(response) = super::ensure_harness_stopped(&state, &lifecycle).await { return response; }
    let update = match state.updater.try_acquire_gate() { Ok(g) => g, Err(e) => return super::update_error_response(e) };
    if let Err(response) = super::ensure_update_idle(&state) { return response; }
    let snapshots = match state.snapshots.try_acquire_configuration() { Ok(g) => g, Err(e) => return super::data_error_response(e,"profile_repair_busy") };
    let cold = match state.cold.try_acquire_maintenance() { Ok(g) => g, Err(e) => return super::data_error_response(e,"profile_repair_busy") };
    let owner = tokio::spawn(async move {
        let _guards = (lifecycle, update, snapshots, cold);
        tokio::task::spawn_blocking(move || {
            nexus_core::terminal_lease::ensure_all_idle(&state.paths)?;
            control(&state.paths, &state.snapshots.configured_dsh_home()?, command)
        }).await
    });
    match owner.await {
        Ok(Ok(Ok(value))) => Json(value).into_response(),
        Ok(Ok(Err(error))) => super::data_error_response(error,"profile_repair_failed"),
        other => super::data_error_response(io::Error::other(format!("Repair interrupted: {other:?}")),"profile_repair_failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn damaged_profile_is_visible_save_is_backed_up_and_restore_is_exact() {
        let paths = NexusPaths::from_root(std::env::temp_dir().join(format!("nexus-offline-repair-{}", nexus_core::agent_auth::random_hex().unwrap())));
        paths.ensure_directories().unwrap();
        let home = paths.root.join("home"); let dir = home.join("profiles/broken");
        fs::create_dir_all(&dir).unwrap(); fs::write(dir.join("package.json"), "{broken").unwrap();
        let make = |action: &str, content: Option<String>, fingerprint: Option<String>, backup: Option<String>| Command {action:action.into(),profile:Some("broken".into()),file:Some("package.json".into()),content,fingerprint,backup};
        assert_eq!(control(&paths,&home,make("list",None,None,None)).unwrap()["profiles"], json!(["broken"]));
        let before = inspect(&paths,&home,"broken").unwrap();
        assert!(before["files"][0]["error"].is_string());
        let good = r#"{"dsh":{"profile":{"bundles":[]}}}"#.to_string();
        assert!(control(&paths,&home,make("save",Some(good.clone()),Some("stale".into()),None)).is_err());
        assert_eq!(fs::read_to_string(dir.join("package.json")).unwrap(), "{broken");
        let saved = control(&paths,&home,make("save",Some(good.clone()),before["files"][0]["fingerprint"].as_str().map(str::to_owned),None)).unwrap();
        assert!(saved["files"][0]["error"].is_null()); assert_eq!(saved["startup_verified"], false);
        let restored = control(&paths,&home,make("restore",None,saved["files"][0]["fingerprint"].as_str().map(str::to_owned),saved["saved_backup"].as_str().map(str::to_owned))).unwrap();
        assert_eq!(fs::read_to_string(dir.join("package.json")).unwrap(), "{broken");
        assert!(restored["files"][0]["error"].is_string());
        let mut other = make("restore",None,restored["files"][0]["fingerprint"].as_str().map(str::to_owned),saved["saved_backup"].as_str().map(str::to_owned));
        fs::create_dir(home.join("profiles/other")).unwrap();
        fs::write(home.join("profiles/other/package.json"), "{broken").unwrap();
        other.profile = Some("other".into());
        assert!(control(&paths,&home,other).is_err());
        assert_eq!(fs::read_to_string(home.join("profiles/other/package.json")).unwrap(), "{broken");
        assert!(read_file(&dir,"../package.json").is_err());
        assert!(validate("cordis.patch.yml", "- [").is_err());
        fs::remove_dir_all(paths.root).unwrap();
    }
}
