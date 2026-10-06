//! Offline, bounded profile-file repair. No plugin execution or package downloads.
use std::{fs, io, path::{Path, PathBuf}};
use axum::{extract::State, Json, response::IntoResponse};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use nexus_core::NexusPaths;
const FILES: [&str; 2] = ["package.json", "cordis.patch.yml"];
const LIMIT: u64 = 32 * 1024;
// Native JSON normalization can expand a previously compact manifest. Keep
// editing/startup bounds unchanged, but allow bounded inspection and recovery
// of the expanded current file, including its backup before replacement.
const RECOVERY_LIMIT: u64 = 256 * 1024;
const BACKUP_LIMIT: u64 = RECOVERY_LIMIT * 8;
fn invalid(message: &str) -> io::Error { io::Error::new(io::ErrorKind::InvalidInput, message) }
fn read_file(dir: &Path, file: &str) -> io::Result<Option<String>> {
    read_file_bounded(dir, file, RECOVERY_LIMIT)
}
fn read_file_bounded(dir: &Path, file: &str, limit: u64) -> io::Result<Option<String>> {
    if !FILES.contains(&file) { return Err(invalid("Unsupported profile repair file")); }
    nexus_core::read_regular_file_bounded(&dir.join(file), limit)?.map(|bytes| String::from_utf8(bytes).map_err(io::Error::other)).transpose()
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
    if !FILES.contains(&file) || content.as_ref().is_some_and(|text| text.len() as u64 > RECOVERY_LIMIT) { return Err(invalid("Profile file exceeds recovery limit")); }
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
    for file in FILES { backup(paths, home, profile, file, read_file_bounded(&dir, file, LIMIT)?)?; }
    Ok(())
}
pub(crate) fn backup_before_startup(paths: &NexusPaths, home: &Path, profile: &str) -> io::Result<()> {
    // A first launch may create the native profile. There is no existing
    // configuration to restore; do not create a profile on Harness's behalf.
    match super::dsh::profile_directory(home, profile) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
        Ok(_) => backup_configuration(paths, home, profile),
    }
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
        // Keep the complete-file identity for restore, but bound the editable
        // preview. Two maximally escaped 32 KiB previews fit the authenticated
        // 512 KiB response budget; full 256 KiB recovery files do not.
        let (preview, content_truncated) = match &content {
            Some(text) => {
                let mut end = text.len().min(LIMIT as usize);
                while !text.is_char_boundary(end) { end -= 1; }
                (Some(&text[..end]), end != text.len())
            },
            None => (None, false),
        };
        files.push(json!({"file":file,"content":preview,"content_truncated":content_truncated,"fingerprint":identity(&content),"error":error}));
    }
    let history = paths.root.join("profile-repair-history");
    let mut backups = Vec::new();
    if history.exists() {
        if nexus_core::path_is_reparse(&fs::symlink_metadata(&history)?) { return Err(invalid("Repair history cannot be linked")); }
        let canonical_home = fs::canonicalize(home)?;
        for entry in fs::read_dir(history)?.take(4096) {
            let entry = entry?;
            if entry.path().extension().and_then(|v| v.to_str()) != Some("json") { continue; }
            let Some(bytes) = nexus_core::read_regular_file_bounded(&entry.path(), BACKUP_LIMIT)? else { continue };
            let Ok(record) = serde_json::from_slice::<Backup>(&bytes) else { continue };
            let id = entry.path().file_stem().unwrap().to_string_lossy().into_owned();
            if record.data_root_id == nexus_core::data_root_identity(paths)? && record.profile == profile && record.home == canonical_home
                && FILES.contains(&record.file.as_str()) && !id.is_empty() && id.len() <= 128 && id.bytes().all(|b| b.is_ascii_hexdigit()) {
                backups.push(json!({"id":id,"file":record.file,"created":record.created}));
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
        let bytes = nexus_core::read_regular_file_bounded(&history.join(format!("{id}.json")), BACKUP_LIMIT)?.ok_or_else(|| invalid("Recovery point missing"))?;
        let record: Backup = serde_json::from_slice(&bytes)?;
        if record.data_root_id != nexus_core::data_root_identity(paths)? || record.profile != profile || record.home != fs::canonicalize(home)? || record.file != file { return Err(invalid("Recovery point belongs to another target")); }
        record.content
    } else {
        let text = command.content.ok_or_else(|| invalid("Configuration text missing"))?;
        validate(&file, &text)?; Some(text)
    };
    if next.as_ref().is_some_and(|text| text.len() as u64 > RECOVERY_LIMIT) { return Err(invalid("Recovery point exceeds recovery limit")); }
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
    #[tokio::test]
    async fn expanded_recovery_previews_fit_authenticated_response_budget() {
        let paths = NexusPaths::from_root(std::env::temp_dir().join(format!("nexus-recovery-response-{}", nexus_core::agent_auth::random_hex().unwrap())));
        paths.ensure_directories().unwrap();
        let home = paths.root.join("home"); let dir = home.join("profiles/web"); fs::create_dir_all(&dir).unwrap();
        for content in ["x".repeat(RECOVERY_LIMIT as usize), "\u{0001}".repeat(RECOVERY_LIMIT as usize), "界".repeat(RECOVERY_LIMIT as usize / 3)] {
            for file in FILES { fs::write(dir.join(file), &content).unwrap(); }
            let id = backup(&paths,&home,"web","package.json",Some(content.clone())).unwrap();
            let report = inspect(&paths,&home,"web").unwrap();
            for row in report["files"].as_array().unwrap() {
                assert_eq!(row["content_truncated"],true);
                assert!(row["content"].as_str().unwrap().len() as u64 <= LIMIT);
                assert_eq!(row["fingerprint"],identity(&Some(content.clone())), "restore uses full-file identity, not preview identity");
            }
            assert!(report["backups"].as_array().unwrap().iter().any(|row| row["id"] == id));
            let response = Json(report).into_response();
            let body = axum::body::to_bytes(response.into_body(), nexus_launcher_core::MAX_RESPONSE_BODY_BYTES).await.unwrap();
            assert!(body.len() < nexus_launcher_core::MAX_RESPONSE_BODY_BYTES);
        }
        fs::remove_dir_all(paths.root).unwrap();
    }
    #[test]
    fn native_pretty_print_expansion_keeps_original_recovery_available() {
        let paths = NexusPaths::from_root(std::env::temp_dir().join(format!("nexus-expanded-recovery-{}", nexus_core::agent_auth::random_hex().unwrap())));
        paths.ensure_directories().unwrap();
        let home = paths.root.join("home"); let dir = home.join("profiles/web");
        fs::create_dir_all(&dir).unwrap();
        let mut manifest = json!({"dsh":{"profile":{"bundles":["@deepseek-ai/dsh-base", "@deepseek-ai/dsh-web-app", "@deepseek-ai/dsh-experimental-schedule-bundle"]}}, "custom":vec!["x";5000]});
        let original = serde_json::to_string(&manifest).unwrap();
        assert!(original.len() as u64 <= LIMIT);
        fs::write(dir.join("package.json"), &original).unwrap();
        backup_before_startup(&paths, &home, "web").unwrap();
        let saved = inspect(&paths, &home, "web").unwrap();
        let id = saved["backups"].as_array().unwrap().iter().find(|row| row["file"] == "package.json").unwrap()["id"].as_str().unwrap();
        manifest["dsh"]["profile"]["bundles"].as_array_mut().unwrap().pop();
        let normalized = serde_json::to_string_pretty(&manifest).unwrap() + "\n";
        assert!(normalized.len() as u64 > LIMIT && normalized.len() as u64 <= RECOVERY_LIMIT);
        fs::write(dir.join("package.json"), &normalized).unwrap();
        let expanded = inspect(&paths, &home, "web").unwrap();
        assert!(expanded["files"][0]["error"].as_str().unwrap().contains("repair limit"));
        assert!(expanded["backups"].as_array().unwrap().iter().any(|row| row["id"] == id));
        let make = |action:&str, fingerprint:Option<String>, backup:Option<String>, content:Option<String>| Command {action:action.into(),profile:Some("web".into()),file:Some("package.json".into()),fingerprint,backup,content};
        let fingerprint = expanded["files"][0]["fingerprint"].as_str().map(str::to_owned);
        assert!(backup_before_startup(&paths, &home, "web").is_err(), "the prelaunch 32 KiB limit is unchanged");
        assert!(control(&paths, &home, make("save",fingerprint.clone(),None,Some(normalized.clone()))).is_err());
        assert!(control(&paths, &home, make("restore",Some("stale".into()),Some(id.into()),None)).is_err());
        assert_eq!(fs::read_to_string(dir.join("package.json")).unwrap(), normalized);
        let restored = control(&paths, &home, make("restore",fingerprint,Some(id.into()),None)).unwrap();
        assert_eq!(fs::read_to_string(dir.join("package.json")).unwrap(), original);
        let expanded_backup = restored["saved_backup"].as_str().unwrap();
        let record:Backup = serde_json::from_slice(&fs::read(paths.root.join("profile-repair-history").join(format!("{expanded_backup}.json"))).unwrap()).unwrap();
        assert_eq!(record.content.as_deref(), Some(normalized.as_str()));
        control(&paths, &home, make("restore",restored["files"][0]["fingerprint"].as_str().map(str::to_owned),Some(expanded_backup.into()),None)).unwrap();
        assert_eq!(fs::read_to_string(dir.join("package.json")).unwrap(), normalized, "the backup of the expanded file is itself recoverable");
        fs::remove_dir_all(paths.root).unwrap();
    }
    #[test]
    fn recovery_reads_remain_bounded_and_escaped_backups_remain_readable() {
        let paths = NexusPaths::from_root(std::env::temp_dir().join(format!("nexus-recovery-bounds-{}", nexus_core::agent_auth::random_hex().unwrap())));
        paths.ensure_directories().unwrap();
        let home = paths.root.join("home"); let dir = home.join("profiles/web"); fs::create_dir_all(&dir).unwrap();
        let content = "\u{0001}".repeat(RECOVERY_LIMIT as usize);
        fs::write(dir.join("package.json"), &content).unwrap();
        let id = backup(&paths,&home,"web","package.json",Some(content.clone())).unwrap();
        let recovery = inspect(&paths,&home,"web").unwrap();
        assert!(recovery["backups"].as_array().unwrap().iter().any(|row| row["id"] == id));
        control(&paths,&home,Command {action:"restore".into(),profile:Some("web".into()),file:Some("package.json".into()),content:None,fingerprint:recovery["files"][0]["fingerprint"].as_str().map(str::to_owned),backup:Some(id.clone())}).unwrap();
        let oversized = content + "x"; fs::write(dir.join("package.json"), &oversized).unwrap();
        assert!(inspect(&paths,&home,"web").is_err());
        assert!(control(&paths,&home,Command {action:"restore".into(),profile:Some("web".into()),file:Some("package.json".into()),content:None,fingerprint:recovery["files"][0]["fingerprint"].as_str().map(str::to_owned),backup:Some(id)}).is_err());
        assert!(backup(&paths,&home,"web","package.json",Some(oversized.clone())).is_err());
        assert_eq!(fs::read_to_string(dir.join("package.json")).unwrap(), oversized);
        fs::remove_dir_all(paths.root).unwrap();
    }
    #[test]
    fn native_startup_recovery_restores_manifest_and_absent_patch_without_creating_profiles() {
        let paths = NexusPaths::from_root(std::env::temp_dir().join(format!("nexus-native-recovery-{}", nexus_core::agent_auth::random_hex().unwrap())));
        paths.ensure_directories().unwrap();
        let home = paths.root.join("home"); fs::create_dir_all(home.join("profiles")).unwrap();
        backup_before_startup(&paths, &home, "web").unwrap();
        assert!(!home.join("profiles/web").exists());
        let dir = home.join("profiles/web"); fs::create_dir(&dir).unwrap();
        let original = r#"{"dsh":{"profile":{"bundles":["retired-bundle"]}}}"#;
        fs::write(dir.join("package.json"), original).unwrap();
        backup_before_startup(&paths, &home, "web").unwrap();
        let saved = inspect(&paths, &home, "web").unwrap();
        fs::write(dir.join("package.json"), r#"{"dsh":{"profile":{"bundles":[]}}}"#).unwrap();
        fs::write(dir.join("cordis.patch.yml"), "[]").unwrap();
        for file in FILES {
            let current = inspect(&paths, &home, "web").unwrap();
            let id = saved["backups"].as_array().unwrap().iter().find(|row| row["file"] == file).unwrap()["id"].as_str().unwrap();
            let fingerprint = current["files"].as_array().unwrap().iter().find(|row| row["file"] == file).unwrap()["fingerprint"].as_str().unwrap();
            control(&paths, &home, Command { action:"restore".into(), profile:Some("web".into()), file:Some(file.into()), content:None, fingerprint:Some(fingerprint.into()), backup:Some(id.into()) }).unwrap();
        }
        assert_eq!(fs::read_to_string(dir.join("package.json")).unwrap(), original);
        assert!(!dir.join("cordis.patch.yml").exists());
        let history = paths.root.join("profile-repair-history"); fs::remove_dir_all(&history).unwrap(); fs::write(&history, "not a directory").unwrap();
        assert!(backup_before_startup(&paths, &home, "web").is_err(), "startup must fail closed when the recovery point cannot be written");
        assert_eq!(fs::read_to_string(dir.join("package.json")).unwrap(), original);
        fs::remove_dir_all(paths.root).unwrap();
    }
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
