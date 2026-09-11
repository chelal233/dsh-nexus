//! Recovery of Nexus-owned profile indexes; Harness files are never replaced.
use std::{fs, io::{self, Read}, path::PathBuf, sync::{Arc, Mutex}};
use axum::{extract::State, response::{IntoResponse, Response}, Json};
use nexus_core::{NexusPaths, ProfileCatalog};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

const LIMIT: u64 = 4 * 1024 * 1024;
const RECORDS: &[(&str, &str)] = &[("profiles", "profiles.json"), ("update", "update-state.json"),
    ("releases", "release-pointers.json"), ("runtime", "state.json"), ("log_session", "run/harness-log-session.json")];
#[derive(Clone)]
pub(crate) struct RecoveryRecords { paths: NexusPaths, gate: Arc<Mutex<()>>, allow_prepare: bool }
impl RecoveryRecords {
    pub(crate) fn new(paths: NexusPaths) -> Self { Self { paths, gate:Arc::new(Mutex::new(())),allow_prepare:true } }
    pub(crate) fn read_only(paths:NexusPaths)->Self {Self{allow_prepare:false,..Self::new(paths)}}
    fn path(&self, id: &str) -> io::Result<PathBuf> {
        let relative = RECORDS.iter().find(|(key, _)| *key == id).ok_or_else(|| invalid("Unknown recovery record"))?.1;
        let path = self.paths.root.join(relative);
        for ancestor in path.parent().unwrap().ancestors() {
            if !ancestor.starts_with(&self.paths.root) { break; }
            let metadata = fs::symlink_metadata(ancestor)?;
            if !metadata.is_dir() || nexus_core::path_is_reparse(&metadata) { return Err(invalid("Recovery path must have ordinary directory ancestors")); }
        }
        Ok(path)
    }
    fn read(&self, id: &str) -> io::Result<(PathBuf, Vec<u8>, String)> {
        let path = self.path(id)?;
        // Recheck the opened object and bytes; source publication is deliberately
        // manual because the ordinary atomic writer does not offer file CAS.
        let mut options = fs::OpenOptions::new(); options.read(true);
        #[cfg(windows)] { use std::os::windows::fs::OpenOptionsExt; options.custom_flags(0x00200000).share_mode(1); }
        let file = options.open(&path)?;
        if nexus_core::path_is_reparse(&file.metadata()?) || !file.metadata()?.is_file() { return Err(invalid("Recovery record is not ordinary")); }
        let identity = nexus_core::log_file_identity(&file)?;
        if file.metadata()?.len() > LIMIT { return Err(invalid("Recovery record exceeds limit")); }
        let mut bytes = Vec::new(); file.take(LIMIT + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > LIMIT { return Err(invalid("Recovery record exceeds limit")); }
        let revision = format!("{:x}", Sha256::digest([identity.as_bytes(), &bytes].concat()));
        Ok((path, bytes, revision))
    }
    fn inspect(&self) -> Value {
        self.inspect_page(0)
    }
    fn inspect_page(&self, offset:usize) -> Value {
        let records = RECORDS.iter().map(|(id, relative)| match self.read(id) {
            Ok((path, bytes, revision)) => {
                let format = format_class(id, &bytes);
                json!({"id":id,"path":path,"revision":revision,"format":format,"can_backup":true,
                    "can_prepare":*id == "profiles" && format == "supported" && self.quiescent().is_ok(),
                    "blocked_reason": if *id != "profiles" { Some("manual_record_repair") } else if format != "supported" { Some("unsupported_record_format") } else if self.quiescent().is_err() { Some("ownership_not_quiescent") } else { None },
                    "automatic_replace":false})
            },
            Err(_) => json!({"id":id,"path":self.paths.root.join(relative),"format":"unreadable","can_backup":false,"can_prepare":false,"blocked_reason":"unreadable_record","automatic_replace":false}),
        }).collect::<Vec<_>>();
        let (artifacts,truncated,next_offset)=self.artifacts(offset);
        let history = nexus_core::profile_history::list(&self.paths);
        let revision = self.read("profiles").ok().map(|(_,_,revision)|revision);
        let blocked = self.quiescent().err().map(|error|error.to_string());
        json!({"records":if self.allow_prepare {records}else{vec![]},"artifacts":artifacts,"artifacts_truncated":truncated,"next_offset":next_offset,"allow_prepare":self.allow_prepare,"automatic_replace":false,
            "recovery_points":history.as_ref().map(|points|points.iter().map(|point|json!({"id":point["id"],"created_at_unix":point["created_at_unix"],"active_profile":point["catalog"]["active_profile"],"profile_count":point["catalog"]["profiles"].as_array().map(Vec::len)})).collect::<Vec<_>>()).unwrap_or_default(),
            "history_error":history.err().map(|error|error.to_string()),"expected_revision":revision,"restore_blocked":blocked})
    }
    fn artifact_summary(&self,id:&str)->Value {
        match self.artifact_manifest(id){Ok((directory,manifest))=>json!({"artifact_id":id,"record_id":manifest["record_id"],"source_path":self.path(manifest["record_id"].as_str().unwrap()).ok(),"backup_path":directory.join("original.bin"),"replacement_path":manifest["replacement_sha256"].as_str().map(|_|directory.join("profiles.replacement.json")),"source_sha256":manifest["source_sha256"],"created_at_unix":manifest["created_at_unix"],"available":true}),Err(_)=>json!({"artifact_id":id,"available":false})}
    }
    fn artifacts(&self,offset:usize)->(Vec<Value>,bool,Option<usize>) {
        self.artifacts_budget(offset,std::time::Duration::from_secs(2))
    }
    fn artifacts_budget(&self,offset:usize,budget:std::time::Duration)->(Vec<Value>,bool,Option<usize>) {
        let parent=self.paths.root.join("recovery-records");
        let Ok(metadata)=fs::symlink_metadata(&parent) else{return(vec![],false,None)};
        if !metadata.is_dir()||nexus_core::path_is_reparse(&metadata){return(vec![],true,None)}
        let mut artifacts=Vec::new();let mut latest=None;
        // Read latest independently, before spending the bounded history budget.
        if let Ok(Some(bytes))=nexus_core::read_regular_file_bounded(&self.paths.root.join("recovery-latest.json"),4096){
            if let Ok(pointer)=serde_json::from_slice::<Value>(&bytes){if pointer["format_version"]==1{if let Some(id)=pointer["artifact_id"].as_str(){if self.artifact_manifest(id).is_ok(){artifacts.push(self.artifact_summary(id));latest=Some(id.to_owned());}}}}
        }
        let deadline=std::time::Instant::now()+budget;
        let Ok(mut entries)=fs::read_dir(parent)else{return(artifacts,true,None)};
        let mut consumed=0;
        while consumed<offset {if std::time::Instant::now()>=deadline{return(artifacts,true,Some(offset))} if entries.next().is_none(){return(artifacts,false,None)} consumed+=1;}
        while consumed<offset+64 {
            if std::time::Instant::now()>=deadline{return(artifacts,true,Some(consumed))}
            let Some(entry)=entries.next()else{return(artifacts,false,None)};consumed+=1;
            if let Ok(entry)=entry {if let Some(id)=entry.file_name().to_str(){if latest.as_deref()!=Some(id){artifacts.push(self.artifact_summary(id));}}}
        }
        (artifacts,true,if consumed<16384{Some(consumed)}else{None})
    }
    fn publish_latest(&self,id:&Value)->io::Result<()> {
        let path=self.paths.root.join("recovery-latest.json");
        if let Some(bytes)=nexus_core::read_regular_file_bounded(&path,4096)? {
            let value:Value=serde_json::from_slice(&bytes)?;
            if value["format_version"]!=1||value.as_object().is_none_or(|o|o.keys().any(|key|key!="format_version"&&key!="artifact_id"))||value["artifact_id"].as_str().is_none_or(|id|!valid_artifact_id(id)){return Err(invalid("Unsupported recovery index preserved"));}
        }
        nexus_core::write_private_json_atomic(&self.paths.root,&path,&json!({"format_version":1,"artifact_id":id}))
    }
    fn quiescent(&self) -> io::Result<()> {
        for relative in ["config-write.pending.json", "config-preserve.pending.json", "cold-publication.json"] {
            if self.paths.root.join(relative).try_exists()? { return Err(invalid("Resolve pending recovery transactions first")); }
        }
        if nexus_core::CheckpointRestoreJournalStore::new(self.paths.clone()).load()?.is_some() { return Err(invalid("Resolve the checkpoint transaction first")); }
        if let Some(metadata) = nexus_core::RuntimeMetadataStore::new(self.paths.clone()).read()? {
            if metadata.harness.pid.is_some() || !matches!(metadata.harness.state, nexus_protocol::HarnessState::Stopped | nexus_protocol::HarnessState::Detached) {
                return Err(invalid("Harness ownership is not proven stopped"));
            }
        }
        if nexus_core::HarnessLogSessionStore::new(self.paths.clone()).read()?.is_some_and(|session| session.launch_pending) { return Err(invalid("Harness launch ownership is unresolved")); }
        for relative in ["cold-operation.json", "install-operation.json"] {
            if let Some(bytes) = nexus_core::read_regular_file_bounded(&self.paths.root.join(relative), LIMIT)? {
                let settled = if relative == "cold-operation.json" {
                    let operation: nexus_protocol::ColdOperation = nexus_core::decode_versioned_record(&bytes)?;
                    operation.owner_quiescent && !operation.cleanup_pending && operation.phase.is_terminal()
                } else {
                    let operation: nexus_protocol::InstallOperation = nexus_core::decode_versioned_record(&bytes)?;
                    operation.owner_quiescent && !operation.cleanup_pending && matches!(operation.phase.as_str(), "succeeded" | "failed" | "cancelled")
                };
                if !settled { return Err(invalid("Installation ownership is unresolved")); }
            }
        }
        if nexus_core::UpdateStateStore::new(self.paths.clone()).load()?.state == nexus_protocol::UpdateState::Running { return Err(invalid("Update is still running")); }
        Ok(())
    }
    fn prepare(&self, command: Value) -> io::Result<Value> {
        let _gate = self.gate.lock().map_err(|_| invalid("Recovery artifact gate failed"))?;
        let action = command["action"].as_str().unwrap_or("");
        if action=="select" {let id=command["artifact_id"].as_str().ok_or_else(||invalid("Artifact ID is required"))?;self.artifact_manifest(id)?;return Ok(self.artifact_summary(id));}
        if action=="list" {let offset=command["offset"].as_u64().unwrap_or(0);if offset>16384{return Err(invalid("Recovery history scan limit reached"));}return Ok(self.inspect_page(offset as usize));}
        if matches!(action,"verify"|"reveal") { return self.verify_artifact(&command,action=="reveal"); }
        if !self.allow_prepare {return Err(io::Error::new(io::ErrorKind::PermissionDenied,"Normal Agent permits only existing recovery artifact inspection"));}
        if action == "restore" { return self.restore(&command); }
        if !matches!(action, "backup" | "prepare") { return Err(invalid("Choose backup or prepare; automatic replacement is unavailable")); }
        let id = command["record_id"].as_str().unwrap_or("");
        let (source, bytes, revision) = self.read(id)?;
        if command["expected_revision"].as_str() != Some(&revision) { return Err(invalid("Recovery record changed; inspect it again")); }
        let replacement = if action == "prepare" {
            if id != "profiles" || format_class(id, &bytes) != "supported" { return Err(invalid("Only an explicitly supported profile catalog can have a replacement prepared")); }
            self.quiescent()?;
            let active = command["active_profile"].as_str().ok_or_else(|| invalid("Choose the active profile name"))?;
            let names: Vec<String> = serde_json::from_value(command["profiles"].clone())?;
            if names.len() > 1024 { return Err(invalid("Profile list exceeds limit")); }
            Some(serde_json::to_vec_pretty(&ProfileCatalog::new(active, names)?)?)
        } else { None };
        let parent = self.paths.root.join("recovery-records");
        fs::create_dir_all(&parent)?;
        if nexus_core::path_is_reparse(&fs::symlink_metadata(&parent)?) { return Err(invalid("Recovery artifact directory cannot be linked")); }
        let directory = parent.join(nexus_core::new_instance_id()); fs::create_dir(&directory)?;
        let backup = directory.join("original.bin");
        nexus_core::write_private_bytes_atomic(&directory, &backup, &bytes)?;
        let replacement_path = if let Some(replacement) = replacement {
            // A concurrent source edit invalidates prepared guidance. Retain the
            // private backup as evidence, but do not publish a replacement.
            if self.read(id)?.2 != revision { return Err(invalid("Recovery record changed while backing up; original remains untouched")); }
            let path = directory.join("profiles.replacement.json");
            nexus_core::write_private_bytes_atomic(&directory, &path, &replacement)?;
            Some(path)
        } else { None };
        let replacement_sha256=replacement_path.as_ref().map(|path| fs::read(path).map(|bytes|format!("{:x}",Sha256::digest(bytes)))).transpose()?;
        let mut result = json!({"format_version":1,"created_at_unix":nexus_core::unix_time_seconds(),"artifact_id":directory.file_name().unwrap().to_string_lossy(),"record_id":id,"source_path":source,"backup_path":backup,"replacement_path":replacement_path,"replacement_sha256":replacement_sha256,
            "source_revision":revision,"source_sha256":format!("{:x}", Sha256::digest(&bytes)),"automatic_replace":false,"original_unchanged":true});
        nexus_core::write_private_json_atomic(&directory, &directory.join("manifest.json"), &result)?;
        if self.publish_latest(&result["artifact_id"]).is_err(){result["index_warning"]=json!("Recovery files were created, but the latest index could not be updated. Keep the artifact ID to reopen them.");}
        Ok(result)
    }
    // Called with the degraded Agent's single-writer gate, or all normal
    // lifecycle/configuration gates. No client-supplied filesystem paths.
    fn restore(&self, command: &Value) -> io::Result<Value> {
        self.quiescent()?;
        let id = command["point_id"].as_str().ok_or_else(||invalid("Choose a recovery time"))?;
        let (replacement, point) = nexus_core::profile_history::load(&self.paths, id)?;
        let (source, original, revision) = self.read("profiles")?;
        if command["expected_revision"].as_str() != Some(&revision) { return Err(invalid("Recovery record changed; refresh recovery times")); }
        // Malformed JSON can be recovered, but a recognizable unsupported
        // schema is preserved rather than silently downgraded.
        if serde_json::from_slice::<Value>(&original).is_ok() && format_class("profiles", &original) != "supported" {
            return Err(invalid("Unsupported record format preserved; use a compatible Nexus version"));
        }
        let parent = self.paths.root.join("recovery-records");
        fs::create_dir_all(&parent)?;
        let metadata = fs::symlink_metadata(&parent)?;
        if !metadata.is_dir() || nexus_core::path_is_reparse(&metadata) { return Err(invalid("Recovery backup directory cannot be linked")); }
        let directory = parent.join(nexus_core::new_instance_id());
        fs::create_dir(&directory)?;
        nexus_core::write_private_bytes_atomic(&directory, &directory.join("original.bin"), &original)?;
        nexus_core::write_private_json_atomic(&directory, &directory.join("restore.json"), &json!({"point_id":id,"original_sha256":format!("{:x}",Sha256::digest(&original)),"created_at_unix":nexus_core::unix_time_seconds()}))?;
        if self.read("profiles")?.2 != revision { return Err(invalid("Record changed while backing up; original preserved")); }
        self.quiescent()?;
        // Atomic publication either keeps the old record or publishes the
        // validated complete file. Failed writes never delete the old record.
        // Native Agent restart also schedules Harness bootstrap; a durable
        // pause prevents that from launching the restored profile implicitly.
        super::recovery_mode::set_paused(&self.paths, true)?;
        nexus_core::write_private_bytes_atomic(&self.paths.root, &source, &replacement)?;
        Ok(json!({"restored":true,"point_id":id,"active_profile":point["catalog"]["active_profile"],"backup_path":directory.join("original.bin"),"restart_required":true}))
    }
    fn artifact_manifest(&self,id:&str)->io::Result<(PathBuf,Value)> {
        if !valid_artifact_id(id) {return Err(invalid("Invalid artifact ID"));}
        let parent=self.paths.root.join("recovery-records"); let directory=parent.join(id);
        for path in [&parent,&directory] {let metadata=fs::symlink_metadata(path)?;if !metadata.is_dir()||nexus_core::path_is_reparse(&metadata){return Err(invalid("Artifact directory cannot be linked"));}}
        let file=directory.join("manifest.json");
        let bytes=nexus_core::read_regular_file_bounded(&file,64*1024)?.ok_or_else(||invalid("Artifact manifest missing"))?;
        nexus_core::verify_private_file(&fs::File::open(file)?)?;
        let manifest:Value=serde_json::from_slice(&bytes)?;
        if manifest["artifact_id"]!=id {return Err(invalid("Artifact identity mismatch"));}
        let object=manifest.as_object().ok_or_else(||invalid("Unsupported recovery manifest"))?;
        if object.get("format_version").is_some_and(|v|v!=1)||object.keys().any(|key| !["format_version","created_at_unix","artifact_id","record_id","source_path","backup_path","replacement_path","replacement_sha256","source_revision","source_sha256","automatic_replace","original_unchanged"].contains(&key.as_str())){return Err(invalid("Unsupported recovery manifest; preserved"));}
        self.path(manifest["record_id"].as_str().ok_or_else(||invalid("Record identity missing"))?)?;
        Ok((directory,manifest))
    }
    fn verify_artifact(&self, command: &Value, reveal: bool) -> io::Result<Value> {
        let id=command["artifact_id"].as_str().ok_or_else(||invalid("Artifact ID is required"))?;
        let (directory,manifest)=self.artifact_manifest(id)?;
        let record=manifest["record_id"].as_str().ok_or_else(||invalid("Record identity missing"))?;
        let source=self.path(record)?;
        let hash=|path:&std::path::Path| -> io::Result<String> {let bytes=nexus_core::read_regular_file_bounded(path,LIMIT)?.ok_or_else(||invalid("Artifact file missing"))?;Ok(format!("{:x}",Sha256::digest(bytes)))};
        let backup=directory.join("original.bin");
        if hash(&backup)?!=manifest["source_sha256"].as_str().unwrap_or("") {return Err(invalid("Private backup integrity check failed"));}
        let replacement=directory.join("profiles.replacement.json");
        let candidate=manifest["replacement_sha256"].as_str();
        if let Some(expected)=candidate {if record!="profiles" || hash(&replacement)?!=expected {return Err(invalid("Replacement integrity check failed"));}
            let bytes=fs::read(&replacement)?; if format_class(record,&bytes)!="supported" {return Err(invalid("Unsupported replacement format"));}
            let catalog:ProfileCatalog=serde_json::from_slice(&bytes)?; ProfileCatalog::new(catalog.active_profile,catalog.profiles)?;
        }
        let current=self.read(record).ok().map(|(_,bytes,_)|format!("{:x}",Sha256::digest(bytes)));
        let state=if current.as_deref()==candidate && candidate.is_some(){"replacement_matches"}else if current.as_deref()==manifest["source_sha256"].as_str(){"original_matches"}else if current.is_none(){"source_unreadable"}else{"source_changed"};
        if reveal {
            let target=match command["target"].as_str(){Some("source")=>source,Some("backup")=>backup,Some("replacement") if candidate.is_some()=>replacement,_=>return Err(invalid("Select a verified artifact location"))};
            #[cfg(windows)] {std::process::Command::new("explorer.exe").arg(format!("/select,{}",target.display())).spawn()?;}
            #[cfg(not(windows))] {let _=target;return Err(invalid("Open this path manually on this platform"));}
        }
        Ok(json!({"artifact_id":id,"state":state,"backup_verified":true,"replacement_verified":candidate.is_some(),"automatic_replace":false}))
    }
}
fn valid_artifact_id(id:&str)->bool { !id.is_empty()&&id.len()<=128&&id.bytes().all(|c|c.is_ascii_alphanumeric()||c==b'-') }
fn invalid(message: &str) -> io::Error { io::Error::new(io::ErrorKind::InvalidInput, message) }
fn format_class(id: &str, bytes: &[u8]) -> &'static str {
    let Ok(value) = serde_json::from_slice::<Value>(bytes) else { return "unknown"; };
    if id != "profiles" { return "manual"; }
    let Some(object) = value.as_object() else { return "unknown"; };
    let Some(version) = object.get("schema_version").and_then(Value::as_u64) else { return "unknown"; };
    if version > u64::from(nexus_core::PROFILE_SCHEMA_VERSION) { return "future"; }
    if version != u64::from(nexus_core::PROFILE_SCHEMA_VERSION) || object.keys().any(|key| !matches!(key.as_str(), "schema_version" | "active_profile" | "profiles")) { return "unknown"; }
    "supported"
}
pub(crate) async fn inspect(State(store): State<RecoveryRecords>) -> Response {
    let path = store.paths.root.clone();
    match super::runtime::RuntimeRequestContext::production().run_blocking_io(super::runtime::BlockingStage::Diagnostics, path, move || Ok(store.inspect())).await {
        Ok(value) => Json(value).into_response(), Err(error) => super::data_error_response(error, "recovery_record_inspection_failed"),
    }
}
pub(crate) async fn prepare(State(store): State<RecoveryRecords>, Json(command): Json<Value>) -> Response {
    let path = store.paths.root.clone();
    match super::runtime::RuntimeRequestContext::production().run_blocking_io(super::runtime::BlockingStage::Diagnostics, path, move || store.prepare(command)).await {
        Ok(value) => Json(value).into_response(), Err(error) => super::data_error_response(error, "recovery_record_prepare_failed"),
    }
}

pub(crate) async fn inspect_normal(State(state): State<super::AppState>) -> Response {
    inspect(State(RecoveryRecords::read_only(state.paths.clone()))).await
}
pub(crate) async fn control_normal(State(state): State<super::AppState>, Json(command): Json<Value>) -> Response {
    if command["action"] != "restore" {
        return prepare(State(RecoveryRecords::read_only(state.paths.clone())), Json(command)).await;
    }
    let lifecycle = state.supervisor.acquire_lifecycle().await;
    if let Err(response) = super::ensure_checkpoint_mutation_ready(&state).await { return response; }
    if let Err(response) = super::ensure_harness_stopped(&state, &lifecycle).await { return response; }
    let update = match state.updater.try_acquire_gate() { Ok(guard)=>guard, Err(error)=>return super::update_error_response(error) };
    if let Err(response) = super::ensure_update_idle(&state) { return response; }
    let snapshots = match state.snapshots.try_acquire_configuration() { Ok(guard)=>guard, Err(error)=>return super::data_error_response(error,"recovery_conflict") };
    let cold = match state.cold.try_acquire_maintenance() { Ok(guard)=>guard, Err(error)=>return super::data_error_response(error,"recovery_conflict") };
    // A disconnected HTTP client cannot release mutation gates between file
    // publication and synchronization of the in-memory active profile.
    let result = tokio::spawn(async move {
        let _guards = (lifecycle, update, snapshots, cold);
        let paths = state.paths.clone();
        let result = tokio::task::spawn_blocking(move || RecoveryRecords::new(paths).prepare(command)).await.map_err(io::Error::other)??;
        let active = result["active_profile"].as_str().ok_or_else(||invalid("Restored profile is missing"))?.to_owned();
        let mut result = result;
        if let Err(error) = super::update_agent_state(&state, |current| current.set_profile(active)).await {
            result["state_warning"] = json!(error.to_string());
        }
        Ok::<_,io::Error>(result)
    }).await;
    match result {
        Ok(Ok(value))=>Json(value).into_response(),
        Ok(Err(error))=>super::data_error_response(error,"recovery_restore_failed"),
        Err(error)=>super::data_error_response(io::Error::other(error),"recovery_restore_failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn saved_point(store: &RecoveryRecords) -> Value {
        let original = fs::read(&store.paths.profiles_file).unwrap();
        let catalog = ProfileCatalog::new("web", vec!["web".into(), "work".into()]).unwrap();
        nexus_core::write_json_atomic(&store.paths.root, &store.paths.profiles_file, &catalog).unwrap();
        nexus_core::profile_history::capture(&store.paths, &catalog).unwrap();
        fs::write(&store.paths.profiles_file, original).unwrap();
        store.inspect()["recovery_points"][0].clone()
    }
    fn restore_command(store: &RecoveryRecords, point: &Value) -> Value {
        json!({"action":"restore","point_id":point["id"],"expected_revision":store.read("profiles").unwrap().2})
    }
    #[test]
    fn dated_restore_recovers_corrupt_catalog_and_preserves_original() {
        let store = fixture();
        assert!(store.inspect()["recovery_points"].as_array().unwrap().is_empty());
        let point = saved_point(&store);
        let external = store.paths.root.join("external-harness-marker"); fs::write(&external,b"untouched").unwrap();
        for original in [b"{broken".to_vec(), br#"{"schema_version":1,"active_profile":"bad/name","profiles":[]}"#.to_vec()] {
            fs::write(&store.paths.profiles_file,&original).unwrap();
            let result = store.prepare(restore_command(&store,&point)).unwrap();
            assert_eq!(result["restored"],true);
            assert!(super::super::recovery_mode::paused(&store.paths).unwrap());
            assert!(super::super::recovery_mode::ensure_start_allowed(&store.paths).is_err());
            assert_eq!(fs::read(result["backup_path"].as_str().unwrap()).unwrap(),original);
            let catalog = nexus_core::profile_history::validate(&fs::read(&store.paths.profiles_file).unwrap()).unwrap();
            assert_eq!(catalog.active_profile,"web");
            assert_eq!(catalog.profiles,vec!["web","work"]);
        }
        assert_eq!(fs::read(external).unwrap(),b"untouched");
        fs::remove_dir_all(store.paths.root).unwrap();
    }
    #[test]
    fn dated_restore_rejects_stale_future_pending_and_bad_backup_without_changes() {
        let store=fixture(); let point=saved_point(&store);
        let stale=restore_command(&store,&point);
        fs::write(&store.paths.profiles_file,b"changed").unwrap();
        assert!(store.prepare(stale).is_err());
        assert_eq!(fs::read(&store.paths.profiles_file).unwrap(),b"changed");
        for original in [br#"{"schema_version":99}"#.as_slice(),br#"{"schema_version":1,"extra":true}"#.as_slice()] {
            fs::write(&store.paths.profiles_file,original).unwrap();
            assert!(store.prepare(restore_command(&store,&point)).is_err());
            assert_eq!(fs::read(&store.paths.profiles_file).unwrap(),original);
        }
        fs::write(&store.paths.profiles_file,b"{broken").unwrap();
        fs::write(store.paths.root.join("config-write.pending.json"),b"{}").unwrap();
        assert!(store.prepare(restore_command(&store,&point)).is_err());
        fs::remove_file(store.paths.root.join("config-write.pending.json")).unwrap();
        // A blocked backup destination must prevent publication.
        fs::write(store.paths.root.join("recovery-records"),b"not a directory").unwrap();
        assert!(store.prepare(restore_command(&store,&point)).is_err());
        fs::remove_file(store.paths.root.join("recovery-records")).unwrap();
        fs::write(store.paths.root.join("profile-history").join(point["id"].as_str().unwrap()),b"damaged point").unwrap();
        assert!(store.prepare(restore_command(&store,&point)).is_err());
        assert_eq!(fs::read(&store.paths.profiles_file).unwrap(),b"{broken");
        assert!(store.inspect()["recovery_points"].as_array().unwrap().is_empty());
        fs::remove_dir_all(store.paths.root).unwrap();
    }
    fn fixture() -> RecoveryRecords {
        let paths = NexusPaths::from_root(std::env::temp_dir().join(format!("nexus-repair-{}", nexus_core::new_instance_id())));
        paths.ensure_directories().unwrap();
        fs::write(paths.root.join("profiles.json"), br#"{"schema_version":1,"active_profile":"bad/name","profiles":[]}"#).unwrap();
        RecoveryRecords::new(paths)
    }
    fn command(store: &RecoveryRecords, action: &str) -> Value {
        json!({"action":action,"record_id":"profiles","expected_revision":store.read("profiles").unwrap().2,"active_profile":"default","profiles":["default"]})
    }
    #[test]
    fn prepared_catalog_preserves_original_and_private_backup() {
        let store=fixture(); let original=fs::read(store.paths.root.join("profiles.json")).unwrap();
        let result=store.prepare(command(&store,"prepare")).unwrap();
        let backup=PathBuf::from(result["backup_path"].as_str().unwrap());
        assert_eq!(fs::read(&backup).unwrap(),original);
        nexus_core::verify_private_file(&fs::File::open(backup).unwrap()).unwrap();
        let replacement: ProfileCatalog=serde_json::from_slice(&fs::read(result["replacement_path"].as_str().unwrap()).unwrap()).unwrap();
        assert_eq!(replacement.active_profile,"default");
        assert_eq!(fs::read(store.paths.root.join("profiles.json")).unwrap(),original);
        assert_eq!(result["automatic_replace"],false);
        let reopened=RecoveryRecords::read_only(store.paths.clone());
        assert!(reopened.inspect()["records"].as_array().unwrap().is_empty());
        assert_eq!(reopened.inspect()["artifacts"][0]["artifact_id"],result["artifact_id"]);
        assert!(reopened.prepare(command(&reopened,"backup")).is_err());
        assert!(reopened.prepare(json!({"action":"verify","artifact_id":result["artifact_id"]})).is_ok());
        let verify=json!({"action":"verify","artifact_id":result["artifact_id"]});
        assert_eq!(store.prepare(verify.clone()).unwrap()["state"],"original_matches");
        fs::copy(result["replacement_path"].as_str().unwrap(),store.paths.root.join("profiles.json")).unwrap();
        assert_eq!(store.prepare(verify.clone()).unwrap()["state"],"replacement_matches");
        fs::write(result["backup_path"].as_str().unwrap(),b"changed").unwrap();
        assert!(store.prepare(verify).is_err());
        assert!(store.prepare(json!({"action":"verify","artifact_id":"../escape"})).is_err());
        fs::remove_dir_all(store.paths.root).unwrap();
    }
    #[test]
    fn newest_artifact_is_reachable_beyond_first_history_page() {
        let store=fixture();let parent=store.paths.root.join("recovery-records");fs::create_dir(&parent).unwrap();
        for index in 0..80 {fs::create_dir(parent.join(format!("000-old-{index:03}"))).unwrap();}
        let result=store.prepare(command(&store,"backup")).unwrap();
        let reopened=RecoveryRecords::read_only(store.paths.clone());let listed=reopened.inspect();
        let (latest,limited,cursor)=reopened.artifacts_budget(0,std::time::Duration::ZERO);assert_eq!(latest[0]["artifact_id"],result["artifact_id"]);assert!(limited);assert_eq!(cursor,Some(0));
        assert_eq!(listed["artifacts"][0]["artifact_id"],result["artifact_id"]);assert_eq!(listed["next_offset"],64);
        assert!(reopened.prepare(json!({"action":"list","offset":64})).unwrap()["artifacts"].as_array().unwrap().len()>1);
        fs::remove_dir_all(store.paths.root).unwrap();
    }
    #[test] fn future_index_is_preserved_and_artifact_remains_selectable() {
        let store=fixture();let pointer=store.paths.root.join("recovery-latest.json");
        for value in [json!({"format_version":99}),json!({"format_version":1}),json!({"format_version":1,"artifact_id":[]}),json!({"format_version":1,"artifact_id":"../bad"}),json!({"format_version":1,"artifact_id":""})] {
            let bytes=value.to_string().into_bytes();fs::write(&pointer,&bytes).unwrap();
            let result=store.prepare(command(&store,"backup")).unwrap();assert!(result["index_warning"].is_string());assert_eq!(fs::read(&pointer).unwrap(),bytes);
            let selected=RecoveryRecords::read_only(store.paths.clone()).prepare(json!({"action":"select","artifact_id":result["artifact_id"]})).unwrap();assert_eq!(selected["available"],true);
        }
        fs::remove_dir_all(store.paths.root).unwrap();
    }
    #[test]
    fn future_unknown_stale_and_pending_records_cannot_be_prepared() {
        let store=fixture();
        for value in [json!({"schema_version":99}),json!({"profiles":[]}),json!({"schema_version":1,"future":true})] {
            fs::write(store.paths.root.join("profiles.json"),value.to_string()).unwrap();
            assert!(store.prepare(command(&store,"prepare")).is_err());
            assert!(store.prepare(command(&store,"backup")).is_ok());
            assert_eq!(fs::read_to_string(store.paths.root.join("profiles.json")).unwrap(),value.to_string());
        }
        fs::write(store.paths.root.join("profiles.json"),br#"{"schema_version":1,"profiles":[]}"#).unwrap();
        let stale=command(&store,"prepare");
        fs::write(store.paths.root.join("profiles.json"),br#"{"schema_version":1,"profiles":["new"]}"#).unwrap();
        assert!(store.prepare(stale).is_err());
        fs::write(store.paths.root.join("config-write.pending.json"),b"{}").unwrap();
        assert!(store.prepare(command(&store,"prepare")).is_err());
        assert!(store.prepare(command(&store,"backup")).is_ok());
        assert!(store.read("../profiles.json").is_err());
        fs::remove_dir_all(store.paths.root).unwrap();
    }
    #[test]
    fn settled_versioned_install_record_does_not_block_candidate() {
        let store=fixture();
        let operation=nexus_protocol::InstallOperation {operation_id:"install-test".into(),job_name:None,release_id:"test".into(),candidate:"test".into(),phase:"succeeded".into(),cancel_requested:false,owner_quiescent:true,cleanup_pending:false,error:None,cleanup_error:None};
        nexus_core::write_versioned_record(&store.paths.root,&store.paths.root.join("install-operation.json"),&operation).unwrap();
        assert!(store.prepare(command(&store,"prepare")).is_ok());
        fs::remove_dir_all(store.paths.root).unwrap();
    }
}
