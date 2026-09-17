//! Private config/undo publication. Prepared records only roll back; committed
//! records never overwrite external edits. Raw records are not diagnostics.
use super::*;
use sha2::{Digest, Sha256};
use std::io::Write;
const FILE: &str = "config-write.pending.json";
const LIMIT: u64 = 4 * 1024 * 1024;
// Four bounded 4MiB documents may be encoded as JSON byte arrays.
const JOURNAL_LIMIT: u64 = 80 * 1024 * 1024;
#[derive(Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct PreviousTarget { bytes: Option<Vec<u8>> }
#[derive(Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct Pending {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    previous_override: Option<PreviousTarget>,
    schema: u32,
    committed: bool,
    rotate: bool,
    old_current: Option<Vec<u8>>,
    old_previous: Option<Vec<u8>>,
    target: Vec<u8>,
}
impl Pending {
    fn next_previous(&self) -> &Option<Vec<u8>> {
        if let Some(target) = &self.previous_override { &target.bytes }
        else if self.rotate { &self.old_current } else { &self.old_previous }
    }
}
#[derive(Debug)]
struct PendingError { cause: String }
impl std::fmt::Display for PendingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Configuration recovery is pending ({}).", self.cause)?;
        f.write_str(" Close programs locking Nexus configuration files, then retry Settings refresh. If files were externally changed, preserve them and export diagnostics; Nexus will not overwrite them.")
    }
}
impl std::error::Error for PendingError {}
pub fn is_config_transaction_error(error: &io::Error) -> bool {
    error.get_ref().is_some_and(|inner| inner.is::<PendingError>())
}
fn pending_error(error: io::Error) -> io::Error {
    // Never echo serde errors (they can contain a user supplied field/value).
    let detail = error.to_string();
    let cause = match detail.as_str() {
        "Committed configuration was externally changed" | "Pending configuration was externally changed"
        | "Configuration changed during recovery" | "Invalid configuration transaction" => detail,
        _ => format!("file operation {:?}; OS code {:?}", error.kind(), error.raw_os_error()),
    };
    io::Error::new(io::ErrorKind::WouldBlock, PendingError { cause })
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreserveDecision { pending_id: String, current_id: String }
fn bytes_id(bytes: &[u8]) -> String { format!("{:x}", Sha256::digest(bytes)) }
impl ConfigStore {
    fn preserve_path(&self) -> PathBuf { self.paths.root.join("config-preserve.pending.json") }
    fn outer_preserves_current(&self) -> io::Result<bool> {
        let Some(bytes)=read_regular_file_bounded(&self.paths.root.join("cold-publication.json"),16*1024*1024)? else { return Ok(false); };
        let value:serde_json::Value=serde_json::from_slice(&bytes).map_err(|_| invalid_data("Invalid outer publication record"))?;
        let fields = value.as_object().ok_or_else(|| invalid_data("Invalid outer publication record"))?;
        // A missing schema_version is treated as v1 on purpose: pending cold
        // publications written before the field existed must still be able to
        // finish or roll back after an upgrade. An explicit foreign version is
        // always rejected.
        if fields.get("schema_version").is_some_and(|v| v.as_u64()!=Some(1)) || fields.keys().any(|key| !["schema_version","committed","preserve_current","prepare_only","owned_runtime","owned_environment","preserve_release","operation","previous_config","target_config","previous_profiles","target_profiles","previous_current","previous_lkg","target_lkg","previous_update","new_slot"].contains(&key.as_str())) { return Err(invalid_data("Unsupported outer publication record")); }
        for key in ["owned_environment", "preserve_release", "prepare_only"] { if fields.get(key).is_some_and(|v| !v.is_boolean()) { return Err(invalid_data("Invalid outer publication record")); } }
        if fields.get("prepare_only").and_then(|v| v.as_bool()) == Some(true)
            && (fields.get("previous_config") != fields.get("target_config")
                || fields.get("new_slot").and_then(|v| v.as_bool()) != Some(true)
                || ["previous_profiles", "target_profiles", "owned_runtime"].iter().any(|key| fields.get(*key).is_some_and(|v| !v.is_null()))
                || fields.get("owned_environment").and_then(|v| v.as_bool()) == Some(true)) {
            return Err(invalid_data("Prepared-only publication cannot change the active environment"));
        }
        let mut profile_records = 0;
        for key in ["previous_profiles", "target_profiles"] {
            if let Some(value) = fields.get(key).filter(|value| !value.is_null()) {
                let mut catalog: ProfileCatalog = serde_json::from_value(value.clone()).map_err(|_| invalid_data("Unsupported outer profiles"))?;
                catalog.normalize()?; profile_records += 1;
            }
        }
        if profile_records == 1 { return Err(invalid_data("Incomplete outer profile selection")); }
        for key in ["previous_config", "target_config"] {
            let config: NexusConfigFile = serde_json::from_value(fields.get(key).cloned().ok_or_else(|| invalid_data("Invalid outer publication record"))?).map_err(|_| invalid_data("Unsupported outer configuration"))?;
            validate_config_document(&self.paths, &config)?;
        }
        let _: nexus_protocol::ColdOperation = serde_json::from_value(fields.get("operation").cloned().ok_or_else(|| invalid_data("Invalid outer publication record"))?).map_err(|_| invalid_data("Unsupported outer operation"))?;
        let _: nexus_protocol::UpdateRuntimeInfo = serde_json::from_value(fields.get("previous_update").cloned().ok_or_else(|| invalid_data("Invalid outer publication record"))?).map_err(|_| invalid_data("Unsupported outer update"))?;
        if !fields.get("committed").is_some_and(|v|v.is_boolean()) || !fields.get("new_slot").is_some_and(|v|v.is_boolean()) || fields.get("preserve_current").is_some_and(|v|!v.is_boolean()) { return Err(invalid_data("Invalid outer publication record")); }
        for key in ["owned_runtime", "previous_current", "previous_lkg", "target_lkg"] {
            if fields.get(key).is_some_and(|v| !v.is_null() && !v.is_string()) { return Err(invalid_data("Invalid outer publication record")); }
        }
        Ok(value.get("preserve_current").and_then(|v|v.as_bool())==Some(true))
    }
    fn validated_current_bytes(&self) -> io::Result<Vec<u8>> {
        let bytes=read_regular_file_bounded(&self.paths.config_file,LIMIT)?.ok_or_else(|| invalid_data("Current configuration is missing; cannot preserve unknown settings"))?;
        let document=decode_config_document(&bytes)?;
        validate_config_document(&self.paths,&document)?;
        Ok(bytes)
    }
    pub fn pending_configuration_status(&self) -> io::Result<Option<serde_json::Value>> {
        let _guard=self.lock_gate()?;
        let id=if let Some(bytes)=read_regular_file_bounded(&self.pending_path(),JOURNAL_LIMIT)? { bytes_id(&bytes) }
        else if let Some(raw)=read_regular_file_bounded(&self.preserve_path(),4096)? {
            let decision:PreserveDecision=decode_versioned_record(&raw)?;
            decision.pending_id
        } else { return Ok(None); };
        Ok(Some(serde_json::json!({"operation_id":id,"pending":true,"can_preserve":self.validated_current_bytes().is_ok()})))
    }
    /// Explicit preservation does not settle a prepared transaction first.
    /// Outer callers persist their decision while CONFIG_WRITE_GATE is held;
    /// subsequent readers also respect that decision if the inner write fails.
    pub fn preserve_current_after(&self, decide: impl FnOnce()->io::Result<()>) -> io::Result<()> {
        let _guard=self.lock_gate()?;
        self.outer_preserves_current()?;
        self.validated_current_bytes()?;
        decide()?;
        self.preserve_pending_inner()
    }
    fn verify_pending_identity(&self, operation_id:&str) -> io::Result<()> {
        let id=if let Some(bytes)=read_regular_file_bounded(&self.pending_path(),JOURNAL_LIMIT)? { bytes_id(&bytes) }
        else if let Some(raw)=read_regular_file_bounded(&self.preserve_path(),4096)? {
            let decision:PreserveDecision=decode_versioned_record(&raw)?;
            decision.pending_id
        } else {return Err(invalid_data("No pending configuration operation"));};
        if id!=operation_id {return Err(invalid_data("Stale configuration operation ID"));}
        Ok(())
    }
    pub fn retry_configuration(&self, operation_id:&str) -> io::Result<()> {
        let _guard=self.lock_gate()?;
        self.verify_pending_identity(operation_id)?;
        self.settle_pending_unlocked()
    }
    pub fn preserve_current_configuration(&self, operation_id:&str) -> io::Result<()> {
        let _guard=self.lock_gate()?;
        self.outer_preserves_current()?;
        self.verify_pending_identity(operation_id)?;
        let current=self.validated_current_bytes()?;
        if let Some(raw)=read_regular_file_bounded(&self.preserve_path(),4096)? {
            let mut decision:PreserveDecision=decode_versioned_record(&raw)?;
            decision.current_id=bytes_id(&current);
            write_versioned_record(&self.paths.root,&self.preserve_path(),&decision)?;
        }
        self.preserve_pending_inner()
    }
    fn preserve_pending_inner(&self) -> io::Result<()> {
        self.outer_preserves_current()?;
        let current=self.validated_current_bytes()?;
        let Some(bytes)=read_regular_file_bounded(&self.pending_path(),JOURNAL_LIMIT)? else {
            if let Some(raw)=read_regular_file_bounded(&self.preserve_path(),4096)? {
                let decision:PreserveDecision=decode_versioned_record(&raw)?;
                if decision.current_id!=bytes_id(&current) {return Err(invalid_data("Configuration changed during recovery"));}
                fs::remove_file(self.preserve_path())?;
            }
            return Ok(());
        };
        let pending: Pending = serde_json::from_slice(&bytes).map_err(|_| invalid_data("Invalid configuration transaction"))?;
        self.validate_pending_configs(&pending)?;
        let id=bytes_id(&bytes);
        let decision=if let Some(raw)=read_regular_file_bounded(&self.preserve_path(),4096)? {
            let saved:PreserveDecision=decode_versioned_record(&raw)?;
            if saved.pending_id!=id || saved.current_id!=bytes_id(&current) {return Err(invalid_data("Configuration changed during recovery"));}
            saved
        } else {
            let saved=PreserveDecision{pending_id:id.clone(),current_id:bytes_id(&current)};
            write_versioned_record(&self.paths.root,&self.preserve_path(),&saved)?;
            saved
        };
        let archive=self.paths.root.join(format!("config-preserved-{}.json",decision.pending_id));
        if let Some(existing)=read_regular_file_bounded(&archive,JOURNAL_LIMIT)? {
            if existing!=bytes {return Err(invalid_data("Configuration archive differs"));}
        } else {
            let temp=self.paths.root.join(format!(".config-preserve-{}.tmp",unix_time_nanos_for_update()));
            let result=(|| {
                let mut file=nexus_private_file::create_new_private(&temp)?;
                file.write_all(&bytes)?;file.sync_all()?;drop(file);
                fs::hard_link(&temp,&archive)
            })();
            let _=fs::remove_file(&temp);result?;
        }
        if self.validated_current_bytes()?!=current || read_regular_file_bounded(&self.pending_path(),JOURNAL_LIMIT)?.as_deref()!=Some(bytes.as_slice()) {
            return Err(invalid_data("Configuration changed during recovery"));
        }
        fs::remove_file(self.pending_path())?;
        fs::remove_file(self.preserve_path())
    }
    fn pending_path(&self) -> PathBuf { self.paths.root.join(FILE) }
    fn validate_pending_configs(&self, p: &Pending) -> io::Result<()> {
        if p.schema != 1 { return Err(invalid_data("Invalid configuration transaction")); }
        for bytes in [Some(&p.target), p.old_current.as_ref(), p.old_previous.as_ref(), p.previous_override.as_ref().and_then(|value| value.bytes.as_ref())].into_iter().flatten() {
            if bytes.len() as u64 > LIMIT { return Err(invalid_data("Invalid configuration transaction")); }
            let document = decode_config_document(bytes)?;
            validate_config_document(&self.paths, &document)?;
        }
        Ok(())
    }
    fn save_pending(&self, pending: &Pending) -> io::Result<()> {
        let bytes = serde_json::to_vec(pending).map_err(invalid_data)?;
        if bytes.len() as u64 > JOURNAL_LIMIT { return Err(invalid_data("Configuration transaction is too large")); }
        write_private_bytes_atomic(&self.paths.root, &self.pending_path(), &bytes)
    }
    pub(super) fn settle_pending_unlocked(&self) -> io::Result<()> {
        self.settle_pending_inner().map_err(pending_error)
    }
    fn settle_pending_inner(&self) -> io::Result<()> {
        let outer_preserves = self.outer_preserves_current()?;
        if read_regular_file_bounded(&self.preserve_path(),4096)?.is_some() || outer_preserves {
            return self.preserve_pending_inner();
        }
        let Some(bytes) = read_regular_file_bounded(&self.pending_path(), JOURNAL_LIMIT)? else { return Ok(()); };
        let p: Pending = serde_json::from_slice(&bytes).map_err(invalid_data)?;
        self.validate_pending_configs(&p)?;
        if p.schema != 1 || p.target.len() as u64 > LIMIT
            || p.old_current.as_ref().is_some_and(|v| v.len() as u64 > LIMIT)
            || p.old_previous.as_ref().is_some_and(|v| v.len() as u64 > LIMIT)
            || (p.rotate && p.old_current.is_none()) { return Err(invalid_data("Invalid configuration transaction")); }
        let backup = self.paths.root.join(PREVIOUS_CONFIG_FILE);
        let current = read_regular_file_bounded(&self.paths.config_file, LIMIT)?;
        let previous = read_regular_file_bounded(&backup, LIMIT)?;
        let next_previous = p.next_previous();
        if p.committed {
            if current.as_ref() != Some(&p.target) || &previous != next_previous {
                return Err(invalid_data("Committed configuration was externally changed"));
            }
        } else {
            if (current != p.old_current && current.as_ref() != Some(&p.target))
                || (previous != p.old_previous && &previous != next_previous) {
                return Err(invalid_data("Pending configuration was externally changed"));
            }
            // Validate both before changing either. A failed restoration keeps
            // the prepared record for the next caller; target is never replayed.
            self.restore_bytes(&self.paths.config_file, &current, &p.old_current)?;
            self.restore_bytes(&backup, &previous, &p.old_previous)?;
        }
        fs::remove_file(self.pending_path())
    }
    fn accept_committed(&self, p: &Pending) -> io::Result<()> {
        let current = read_regular_file_bounded(&self.paths.config_file, LIMIT).map_err(pending_error)?;
        let previous = read_regular_file_bounded(&self.paths.root.join(PREVIOUS_CONFIG_FILE), LIMIT).map_err(pending_error)?;
        let expected = p.next_previous();
        if current.as_ref() != Some(&p.target) || &previous != expected {
            return Err(pending_error(invalid_data("Committed configuration was externally changed")));
        }
        let _ = fs::remove_file(self.pending_path());
        Ok(())
    }
    fn restore_bytes(&self, path: &Path, observed: &Option<Vec<u8>>, target: &Option<Vec<u8>>) -> io::Result<()> {
        if observed == target { return Ok(()); }
        if read_regular_file_bounded(path, LIMIT)? != *observed { return Err(invalid_data("Configuration changed during recovery")); }
        match target {
            Some(bytes) => write_private_bytes_atomic(&self.paths.root, path, bytes),
            None => fs::remove_file(path),
        }
    }
    pub(super) fn publish_config_unlocked(&self, document: &NexusConfigFile, valid_current: bool) -> io::Result<()> {
        self.publish_config_pair(encode_json(document).map_err(invalid_data)?, valid_current, None, None)
    }
    fn publish_config_pair(&self, target: Vec<u8>, valid_current: bool, previous_override: Option<PreviousTarget>, expected: Option<(&[u8], &Option<Vec<u8>>)>) -> io::Result<()> {
        let old_current = read_regular_file_bounded(&self.paths.config_file, LIMIT)?;
        let backup = self.paths.root.join(PREVIOUS_CONFIG_FILE);
        let old_previous = read_regular_file_bounded(&backup, LIMIT)?;
        if expected.is_some_and(|(current,previous)|old_current.as_deref()!=Some(current) || &old_previous!=previous) {
            return Err(io::Error::new(io::ErrorKind::WouldBlock,"Configuration pair changed before recovery publication; newer files were retained"));
        }
        if let Some(bytes) = &old_previous { validate_config_document(&self.paths, &decode_config_document(bytes)?)?; }
        if target.len() as u64 > LIMIT { return Err(invalid_data("Configuration exceeds 4MiB")); }
        let mut p = Pending { previous_override, schema: 1, committed: false, rotate: valid_current && old_current.is_some(), old_current, old_previous, target };
        self.validate_pending_configs(&p)?;
        self.save_pending(&p)?;
        let result = (|| {
            self.restore_bytes(&self.paths.config_file, &p.old_current, &Some(p.target.clone()))?;
            self.restore_bytes(&backup, &p.old_previous, p.next_previous())?;
            p.committed = true;
            self.save_pending(&p)
        })();
        if let Err(error) = result {
            // Read the durable decision, including an ambiguous commit write.
            let durable = read_regular_file_bounded(&self.pending_path(), JOURNAL_LIMIT)
                .ok().flatten().and_then(|bytes| serde_json::from_slice::<Pending>(&bytes).ok());
            if durable.as_ref().is_some_and(|record| record.committed && record == &p) {
                self.accept_committed(&p)?;
                return Ok(());
            }
            self.settle_pending_unlocked()?;
            return Err(error);
        }
        // Commit is durable. Failure to remove the record is harmless and may
        // be retried by load; reporting failure would trigger an outer rollback.
        let _ = fs::remove_file(self.pending_path());
        Ok(())
    }
    /// Undo only an uncommitted update attempt, including its undo pollution.
    /// Current and previous are published as one recoverable transaction.
    pub fn restore_failed_update_ref(&self, attempted: &NexusConfigFile, original: &NexusConfigFile,
        original_bytes: &[u8], original_undo: &Option<Vec<u8>>) -> io::Result<()> {
        let _guard = self.lock_gate()?;
        self.settle_pending_unlocked()?;
        let current_bytes=read_regular_file_bounded(&self.paths.config_file,LIMIT)?.ok_or_else(||invalid_data("Current configuration is missing"))?;
        let mut current = decode_config_document(&current_bytes)?;
        validate_config_document(&self.paths,&current)?;
        if attempted.update_attempt_id.is_none() || current.update_attempt_id != attempted.update_attempt_id || current.update != attempted.update {
            return Err(io::Error::new(io::ErrorKind::WouldBlock, "Update settings changed concurrently; the newer settings were retained"));
        }
        let mut previous = read_regular_file_bounded(&self.paths.root.join(PREVIOUS_CONFIG_FILE), LIMIT)?;
        let observed_previous=previous.clone();
        let unchanged = current == *attempted;
        if unchanged && previous.as_deref() == Some(original_bytes) {
            previous = original_undo.clone();
        } else if let Some(bytes) = &previous {
            let mut document = decode_config_document(bytes)?;
            if document.update_attempt_id == attempted.update_attempt_id && document.update == attempted.update {
                if let Some(spec) = document.update.as_mut() { spec.ref_name = original.update.as_ref().ok_or_else(|| invalid_data("Original update settings missing"))?.ref_name.clone(); }
                document.update_attempt_id = None;
                previous = Some(encode_json(&document).map_err(invalid_data)?);
            }
        }
        if let Some(spec) = current.update.as_mut() { spec.ref_name = original.update.as_ref().ok_or_else(|| invalid_data("Original update settings missing"))?.ref_name.clone(); }
        current.update_attempt_id = None;
        let target = if unchanged { original_bytes.to_vec() } else { encode_json(&current).map_err(invalid_data)? };
        self.publish_config_pair(target, false, Some(PreviousTarget { bytes: previous }),Some((&current_bytes,&observed_previous)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (ConfigStore, PathBuf) {
        let root = env::temp_dir().join(format!("nexus-config-txn-{}-{}", std::process::id(), unix_time_nanos_for_update()));
        let store = ConfigStore::new(NexusPaths::from_root(root.clone()));
        store.paths.ensure_directories().unwrap();
        (store, root)
    }
    fn config(value: bool) -> NexusConfigFile {
        NexusConfigFile { external_harness: None, harness_preferences: Some(nexus_protocol::HarnessPreferencesPayload { telemetry_disabled: Some(value), ..Default::default() }), ..Default::default() }
    }
    fn outer_preserve_fixture() -> serde_json::Value {
        serde_json::json!({"schema_version":1,"committed":false,"preserve_current":true,"new_slot":false,
            "previous_config":config(false),"target_config":config(true),"previous_update":nexus_protocol::UpdateRuntimeInfo::idle(),
            "operation":{"operation_id":"cold-test","phase":"failed","tag":"test","source":"official","mode":"portable",
                "release_id":"test","candidate":"test","progress_percent":0,"started_at_unix":1}})
    }
    #[test]
    fn prepare_only_publication_rejects_every_active_environment_change() {
        let (store, root) = fixture();
        store.write(&config(false)).unwrap();
        let current = fs::read(&store.paths.config_file).unwrap();
        let mut valid = outer_preserve_fixture();
        valid["prepare_only"] = true.into();
        valid["new_slot"] = true.into();
        valid["target_config"] = valid["previous_config"].clone();
        let path = root.join("cold-publication.json");
        for (field, value) in [
            ("target_config", serde_json::to_value(config(true)).unwrap()),
            ("new_slot", false.into()),
            ("previous_profiles", serde_json::json!({"active_profile":"web","profiles":["web"]})),
            ("target_profiles", serde_json::json!({"active_profile":"web","profiles":["web"]})),
            ("owned_runtime", "runtime-new".into()),
            ("owned_environment", true.into()),
        ] {
            let mut invalid = valid.clone(); invalid[field] = value;
            write_private_json_atomic(&root, &path, &invalid).unwrap();
            let error = store.outer_preserves_current().unwrap_err();
            assert_eq!(error.to_string(), "Prepared-only publication cannot change the active environment", "{field}");
            // Exercise the real read/recovery entry point too: no config or
            // publication record may be discarded while admission is blocked.
            let before = fs::read(&path).unwrap();
            assert!(store.load().is_err(), "{field}");
            assert_eq!(fs::read(&store.paths.config_file).unwrap(), current);
            assert_eq!(fs::read(&path).unwrap(), before);
        }
        for record in [valid, outer_preserve_fixture()] {
            write_private_json_atomic(&root, &path, &record).unwrap();
            assert!(store.outer_preserves_current().unwrap());
            assert_eq!(store.load().unwrap(), config(false));
            assert_eq!(fs::read(&store.paths.config_file).unwrap(), current);
        }
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn revision_cas_rejects_stale_raw_bytes_and_preserves_unknown_formats() {
        let (store, root) = fixture();
        let absent = store.snapshot().unwrap();
        assert_eq!(absent.revision, "missing");
        let (saved, ()) = store.transaction_if_revision(&absent.revision, |d| { *d=config(false); Ok(()) }).unwrap();
        let bytes = fs::read(&store.paths.config_file).unwrap();
        let (same, ()) = store.transaction_if_revision(&saved.revision, |_| Ok(())).unwrap();
        assert_eq!(same.revision, saved.revision);
        assert_eq!(fs::read(&store.paths.config_file).unwrap(), bytes);
        let (changed, ()) = store.transaction_if_revision(&saved.revision, |d| { *d=config(true); Ok(()) }).unwrap();
        assert_ne!(changed.revision, saved.revision);
        let error = store.transaction_if_revision(&saved.revision, |_| -> io::Result<()> { panic!("stale closure must not run") }).unwrap_err();
        assert!(is_config_revision_conflict(&error));
        let mut raw = fs::read(&store.paths.config_file).unwrap(); raw.push(b' ');
        fs::write(&store.paths.config_file, &raw).unwrap();
        assert!(is_config_revision_conflict(&store.transaction_if_revision(&changed.revision, |_| Ok(())).unwrap_err()));
        let before_backup = fs::read(root.join(PREVIOUS_CONFIG_FILE)).unwrap();
        for invalid in [serde_json::json!({"schema_version":2}),serde_json::json!({"future":true}),serde_json::json!({"runtime":{"future":true}}),serde_json::json!({"harness_preferences":{"future":true}})] {
            let raw=serde_json::to_vec(&invalid).unwrap();fs::write(&store.paths.config_file,&raw).unwrap();
            assert!(store.snapshot().is_err()); assert!(store.write(&config(false)).is_err()); assert!(store.restore_previous().is_err());
            assert_eq!(fs::read(&store.paths.config_file).unwrap(),raw);
            assert_eq!(fs::read(root.join(PREVIOUS_CONFIG_FILE)).unwrap(),before_backup);
            assert!(!store.pending_path().exists());
        }
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn legacy_harness_is_read_without_rewriting_and_future_pending_is_retained() {
        let (store, root)=fixture();
        let spec=HarnessLaunchSpec::new(PathBuf::from("legacy-harness"));
        let raw=serde_json::to_vec(&spec).unwrap();fs::write(&store.paths.config_file,&raw).unwrap();
        assert_eq!(store.load().unwrap().harness,Some(spec.clone()));
        assert_eq!(load_harness_launch_spec(&store.paths).unwrap(),Some(spec));
        assert_eq!(fs::read(&store.paths.config_file).unwrap(),raw);
        let target=serde_json::to_vec(&serde_json::json!({"schema_version":2})).unwrap();
        store.save_pending(&Pending { previous_override: None,schema:1,committed:false,rotate:false,old_current:Some(raw.clone()),old_previous:None,target}).unwrap();
        assert!(store.load().is_err());assert!(store.pending_path().exists());
        assert_eq!(fs::read(&store.paths.config_file).unwrap(),raw);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn future_update_and_outer_records_block_writes_and_preserve_markers() {
        let (store, root)=fixture();store.write(&config(false)).unwrap();
        let updates=UpdateStateStore::new(store.paths.clone());
        for value in [serde_json::json!({"schema_version":2,"update":{"state":"running"}}),serde_json::json!({"schema_version":1,"update":{"state":"running","future":true}})] {
            let bytes=serde_json::to_vec(&value).unwrap();fs::write(&store.paths.update_state_file,&bytes).unwrap();
            assert!(updates.load().is_err());assert!(updates.recover_unattached().is_err());assert!(updates.write(&nexus_protocol::UpdateRuntimeInfo::idle()).is_err());
            assert_eq!(fs::read(&store.paths.update_state_file).unwrap(),bytes);
        }
        let current=fs::read(&store.paths.config_file).unwrap();
        let p=Pending { previous_override: None,schema:1,committed:false,rotate:false,old_current:Some(current.clone()),old_previous:None,target:encode_json(&config(true)).unwrap()};
        store.save_pending(&p).unwrap();
        let pending=fs::read(store.pending_path()).unwrap();
        let decision=PreserveDecision{pending_id:bytes_id(&pending),current_id:bytes_id(&current)};
        write_versioned_record(&root,&store.preserve_path(),&decision).unwrap();
        let mut outer=outer_preserve_fixture();outer["schema_version"]=2.into();
        write_private_json_atomic(&root,&root.join("cold-publication.json"),&outer).unwrap();
        assert!(store.load().is_err());assert!(store.preserve_current_configuration(&bytes_id(&pending)).is_err());
        assert_eq!(fs::read(store.pending_path()).unwrap(),pending);assert!(store.preserve_path().exists());
        assert_eq!(fs::read(&store.paths.config_file).unwrap(),current);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn profile_runtime_and_diagnostics_unknown_formats_are_preserved() {
        let (config,root)=fixture();
        let profiles=ProfileStore::new(config.paths.clone());
        let profile=profiles.load().unwrap();
        for future in [true,false] {
            let mut value=serde_json::to_value(&profile).unwrap();
            if future {value["schema_version"]=2.into();}else{value["future"]=true.into();}
            let bytes=serde_json::to_vec(&value).unwrap();fs::write(&config.paths.profiles_file,&bytes).unwrap();
            assert!(profiles.read().is_err());assert!(profiles.load().is_err());assert!(profiles.select("next").is_err());assert!(profiles.write(&profile).is_err());
            assert_eq!(fs::read(&config.paths.profiles_file).unwrap(),bytes);
        }
        let runtime=RuntimeMetadataStore::new(config.paths.clone());let metadata=NexusRuntimeMetadata::detached();
        for mode in 0..3 {
            let mut value=serde_json::to_value(&metadata).unwrap();
            match mode {0=>value["schema_version"]=2.into(),1=>value["future"]=true.into(),_=>value["harness"]["future"]=true.into()};
            let bytes=serde_json::to_vec(&value).unwrap();fs::write(&config.paths.state_file,&bytes).unwrap();
            assert!(runtime.read().is_err());assert!(runtime.write(&metadata).is_err());assert!(runtime.update_harness(nexus_protocol::HarnessRuntimeInfo::detached()).is_err());
            assert_eq!(fs::read(&config.paths.state_file).unwrap(),bytes);
        }
        let diagnostics=DiagnosticsStore::new(config.paths.clone());
        let bundle=diagnostics.collect(None).unwrap();let path=PathBuf::from(&bundle.directory).join("diagnostics.json");
        let original:serde_json::Value=serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        for nested in [false,true] {
            let mut value=original.clone();if nested{value["bundle"]["future"]=true.into();}else{value["future"]=true.into();}
            let bytes=serde_json::to_vec(&value).unwrap();fs::write(&path,&bytes).unwrap();
            let (visible,warnings)=diagnostics.list_with_warnings().unwrap();
            assert!(visible.is_empty());assert_eq!(warnings.len(),1);assert_eq!(warnings[0]["bundle_id"],bundle.id);
            assert!(diagnostics.open_path(&bundle.id,None).is_err());
            assert_eq!(fs::read(&path).unwrap(),bytes);
        }
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn prepared_cuts_roll_back_and_committed_cuts_only_settle() {
        for committed in [false, true] {
            for changed_previous in [false, true] {
                if committed && !changed_previous { continue; }
                let (store, root) = fixture();
                let a=encode_json(&config(false)).unwrap(); let b=encode_json(&config(true)).unwrap();
                let p=Pending { previous_override: None, schema:1, committed, rotate:true, old_current:Some(a.clone()), old_previous:None, target:b.clone() };
                store.save_pending(&p).unwrap();
                write_private_bytes_atomic(&root,&store.paths.config_file,&b).unwrap();
                if changed_previous { write_private_bytes_atomic(&root,&root.join(PREVIOUS_CONFIG_FILE),&a).unwrap(); }
                assert_eq!(store.load().unwrap(), config(committed));
                assert!(!store.pending_path().exists());
                assert_eq!(root.join(PREVIOUS_CONFIG_FILE).exists(), committed);
                // The outer publication can safely restore its own previous.
                store.write(&config(false)).unwrap();
                assert_eq!(ConfigStore::new(store.paths.clone()).load().unwrap(),config(false));
                fs::remove_dir_all(root).unwrap();
            }
        }
    }
    #[test]
    fn paired_recovery_cuts_restore_both_documents_or_accept_both_targets() {
        for committed in [false,true] { for write_current in [false,true] { for write_previous in [false,true] {
            if committed && (!write_current || !write_previous) { continue; }
            for remove_previous in [false,true] {
                let (store,root)=fixture();
                let old=encode_json(&config(false)).unwrap();let target=encode_json(&config(true)).unwrap();
                write_private_bytes_atomic(&root,&store.paths.config_file,&old).unwrap();
                write_private_bytes_atomic(&root,&root.join(PREVIOUS_CONFIG_FILE),&old).unwrap();
                let undo=if remove_previous {None} else {Some(target.clone())};
                let p=Pending {schema:1,committed,rotate:false,old_current:Some(old.clone()),old_previous:Some(old.clone()),target:target.clone(),previous_override:Some(PreviousTarget{bytes:undo.clone()})};
                store.save_pending(&p).unwrap();
                if write_current {write_private_bytes_atomic(&root,&store.paths.config_file,&target).unwrap();}
                if write_previous {store.restore_bytes(&root.join(PREVIOUS_CONFIG_FILE),&Some(old.clone()),&undo).unwrap();}
                store.load().unwrap();
                assert_eq!(fs::read(&store.paths.config_file).unwrap(),if committed {target} else {old.clone()});
                assert_eq!(read_regular_file_bounded(&root.join(PREVIOUS_CONFIG_FILE),LIMIT).unwrap(),if committed {undo} else {Some(old)});
                assert!(!store.pending_path().exists());fs::remove_dir_all(root).unwrap();
            }
        }}}
    }
    #[test]
    fn paired_recovery_rejects_external_edits_since_target_calculation() {
        for edit_previous in [false,true] {
            let (store,root)=fixture();let old=encode_json(&config(false)).unwrap();let newer=encode_json(&config(true)).unwrap();
            write_private_bytes_atomic(&root,&store.paths.config_file,&old).unwrap();
            let backup=root.join(PREVIOUS_CONFIG_FILE);write_private_bytes_atomic(&root,&backup,&old).unwrap();
            let expected_previous=Some(old.clone());
            let changed=if edit_previous {&backup} else {&store.paths.config_file};
            write_private_bytes_atomic(&root,changed,&newer).unwrap();
            let error=store.publish_config_pair(old.clone(),false,Some(PreviousTarget{bytes:None}),Some((&old,&expected_previous))).unwrap_err();
            assert_eq!(error.kind(),io::ErrorKind::WouldBlock);
            assert_eq!(fs::read(changed).unwrap(),newer);assert!(!store.pending_path().exists());
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn failed_attempt_does_not_rewrite_an_undo_document_owned_by_another_save() {
        let (store,root)=fixture();let mut original=config(false);
        original.update=Some(serde_json::from_value(serde_json::json!({"source":"https://example.com/repo.git","ref_name":"old"})).unwrap());
        store.write(&original).unwrap();let original_bytes=fs::read(&store.paths.config_file).unwrap();
        let original_undo=read_regular_file_bounded(&root.join(PREVIOUS_CONFIG_FILE),LIMIT).unwrap();
        let (attempted,())=store.transaction(|config| {config.update.as_mut().unwrap().ref_name="new".into();config.update_attempt_id=Some(crate::agent_auth::random_hex()?);Ok(())}).unwrap();
        store.transaction(|config| {config.harness_preferences.as_mut().unwrap().open_browser=Some(false);Ok(())}).unwrap();
        let mut other=attempted.clone();other.update_attempt_id=Some(crate::agent_auth::random_hex().unwrap());
        let other_bytes=encode_json(&other).unwrap();
        write_private_bytes_atomic(&root,&root.join(PREVIOUS_CONFIG_FILE),&other_bytes).unwrap();
        store.restore_failed_update_ref(&attempted,&original,&original_bytes,&original_undo).unwrap();
        assert_eq!(fs::read(root.join(PREVIOUS_CONFIG_FILE)).unwrap(),other_bytes);
        let restored=store.load().unwrap();assert_eq!(restored.update.unwrap().ref_name,"old");
        assert_eq!(restored.harness_preferences.unwrap().open_browser,Some(false));
        assert!(restored.update_attempt_id.is_none());
        // Restoring a historical snapshot is a new explicit write and must
        // not revive that snapshot's rollback authority.
        store.restore_previous().unwrap();assert!(store.load().unwrap().update_attempt_id.is_none());
        fs::remove_dir_all(root).unwrap();
    }
    #[cfg(windows)]
    #[test]
    fn locked_current_does_not_rotate_undo_and_locked_backup_rolls_current_back() {
        use std::os::windows::fs::OpenOptionsExt;
        let (store, root)=fixture();
        store.write(&config(false)).unwrap();
        assert!(!root.join(PREVIOUS_CONFIG_FILE).exists());
        store.write(&config(true)).unwrap();
        for path in [&store.paths.config_file, &root.join(PREVIOUS_CONFIG_FILE)] {
            let before=fs::read(&store.paths.config_file).unwrap();
            let undo=fs::read(root.join(PREVIOUS_CONFIG_FILE)).unwrap();
            let lock=fs::OpenOptions::new().read(true).share_mode(3).open(path).unwrap();
            assert!(store.write(&config(false)).is_err());
            assert_eq!(fs::read(&store.paths.config_file).unwrap(),before);
            assert_eq!(fs::read(root.join(PREVIOUS_CONFIG_FILE)).unwrap(),undo);
            drop(lock);
            assert_eq!(store.load().unwrap(),config(true));
            assert!(!store.pending_path().exists());
        }
        fs::remove_dir_all(root).unwrap();
    }
    #[cfg(windows)]
    #[test]
    fn accepted_commit_survives_locked_cleanup_and_reports_safe_causes() {
        use std::os::windows::fs::OpenOptionsExt;
        let (store,root)=fixture();
        let target=encode_json(&config(true)).unwrap();
        let p=Pending { previous_override: None,schema:1,committed:true,rotate:false,old_current:None,old_previous:None,target:target.clone()};
        store.save_pending(&p).unwrap();
        fs::write(&store.paths.config_file,&target).unwrap();
        let lock=fs::OpenOptions::new().read(true).share_mode(3).open(store.pending_path()).unwrap();
        store.accept_committed(&p).unwrap();
        assert!(store.pending_path().exists());
        drop(lock);
        assert_eq!(store.load().unwrap(),config(true));
        let external=pending_error(invalid_data("Pending configuration was externally changed")).to_string();
        assert!(external.contains("externally changed"));
        assert!(!pending_error(invalid_data("unknown field SECRET_KEY")).to_string().contains("SECRET_KEY"));
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn pending_blocks_direct_release_delete_and_launch_reader_settles() {
        let (store,root)=fixture();
        let releases=ReleaseStore::new(store.paths.clone());
        releases.register("kept","v-kept",None,None).unwrap();
        let slot=releases.release_root("kept").unwrap();
        let mut old=config(false);
        old.harness=Some(HarnessLaunchSpec::new(slot.join("node.exe")));
        store.write(&old).unwrap();
        assert!(releases.remove("kept").is_err());
        let p=Pending { previous_override: None,schema:1,committed:false,rotate:true,old_current:Some(encode_json(&old).unwrap()),old_previous:None,target:encode_json(&config(true)).unwrap()};
        store.save_pending(&p).unwrap();
        fs::write(&store.paths.config_file,&p.target).unwrap();
        assert!(releases.remove("kept").is_err());
        assert_eq!(load_harness_launch_spec(&store.paths).unwrap(),old.harness);
        assert!(releases.remove("kept").is_err());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn environment_and_node_entry_references_protect_release() {
        const CHILD: &str = "NEXUS_TEST_REFERENCE_CHILD";
        if let Ok(mode) = env::var(CHILD) {
            let (store,root)=fixture();
            let releases=ReleaseStore::new(store.paths.clone());
            releases.register("old","v-old",None,None).unwrap();
            let slot=releases.release_root("old").unwrap();
            let script=slot.join("main={literal}.js"); fs::write(&script,b"test").unwrap();
            let mut document=config(false);
            let mut harness=HarnessLaunchSpec::new(root.join("node.exe"));
            harness.mode=HarnessLaunchMode::Node;
            harness.args=vec![if mode == "entry" { script.to_string_lossy().into_owned() } else { root.join("elsewhere.js").to_string_lossy().into_owned() }];
            document.harness=Some(harness);
            store.write(&document).unwrap();
            match mode.as_str() {
                "program" => { env::set_var(HARNESS_PROGRAM_ENV,slot.join("node.exe")); },
                "cwd" => { env::set_var(HARNESS_WORKING_DIR_ENV,&slot); },
                "args" => { env::set_var(HARNESS_ARGS_ENV,serde_json::to_string(&vec![script.to_string_lossy().into_owned()]).unwrap()); },
                _ => {},
            }
            assert!(releases.remove("old").is_err());
            assert!(script.exists());
            fs::remove_dir_all(root).unwrap();
        } else {
            for mode in ["entry","program","cwd","args"] {
                let status=std::process::Command::new(env::current_exe().unwrap())
                    .args(["--exact","config_transaction::tests::environment_and_node_entry_references_protect_release"])
                    .env(CHILD,mode).status().unwrap();
                assert!(status.success(),"{mode}");
            }
        }
    }
    #[test]
    fn preserve_prepared_b_and_external_x_without_touching_previous() {
        for external in [false,true] {
            let (store,root)=fixture();
            let a=encode_json(&config(false)).unwrap();let b=encode_json(&config(true)).unwrap();
            let p=Pending { previous_override: None,schema:1,committed:false,rotate:true,old_current:Some(a),old_previous:Some(encode_json(&config(false)).unwrap()),target:b.clone()};
            store.save_pending(&p).unwrap();
            let x=if external {encode_json(&NexusConfigFile::default()).unwrap()}else{b};
            fs::write(&store.paths.config_file,&x).unwrap();
            fs::write(root.join(PREVIOUS_CONFIG_FILE),encode_json(&config(false)).unwrap()).unwrap();
            let status=store.pending_configuration_status().unwrap().unwrap();
            assert!(store.preserve_current_configuration("stale").is_err());
            store.preserve_current_configuration(status["operation_id"].as_str().unwrap()).unwrap();
            assert_eq!(fs::read(&store.paths.config_file).unwrap(),x);
            assert_eq!(fs::read(root.join(PREVIOUS_CONFIG_FILE)).unwrap(),encode_json(&config(false)).unwrap());
            assert!(!store.pending_path().exists());
            assert!(store.pending_configuration_status().unwrap().is_none());
            fs::remove_dir_all(root).unwrap();
        }
    }
    #[test]
    fn outer_preserve_before_inner_marker_failure_blocks_rollback_and_replays() {
        let (store,root)=fixture();
        let a=encode_json(&config(false)).unwrap();let b=encode_json(&config(true)).unwrap();
        store.save_pending(&Pending { previous_override: None,schema:1,committed:false,rotate:true,old_current:Some(a),old_previous:None,target:b.clone()}).unwrap();
        fs::write(&store.paths.config_file,&b).unwrap();
        fs::create_dir(store.preserve_path()).unwrap();
        assert!(store.preserve_current_after(|| write_private_json_atomic(&root,&root.join("cold-publication.json"),&outer_preserve_fixture())).is_err());
        assert!(store.load().is_err());
        assert_eq!(fs::read(&store.paths.config_file).unwrap(),b);
        fs::remove_dir(store.preserve_path()).unwrap();
        assert_eq!(store.load().unwrap(),config(true));
        assert!(!store.pending_path().exists());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn concurrent_reader_observes_preserve_decision_without_rollback() {
        let (store,root)=fixture();
        let b=encode_json(&config(true)).unwrap();
        store.save_pending(&Pending { previous_override: None,schema:1,committed:false,rotate:true,old_current:Some(encode_json(&config(false)).unwrap()),old_previous:None,target:b.clone()}).unwrap();
        fs::write(&store.paths.config_file,&b).unwrap();
        let reader=store.clone();let (start_tx,start_rx)=std::sync::mpsc::channel();
        let handle=std::thread::spawn(move || {start_rx.recv().unwrap();reader.load().unwrap()});
        store.preserve_current_after(|| {
            write_private_json_atomic(&root,&root.join("cold-publication.json"),&outer_preserve_fixture())?;
            start_tx.send(()).unwrap();Ok(())
        }).unwrap();
        assert_eq!(handle.join().unwrap(),config(true));
        assert_eq!(fs::read(&store.paths.config_file).unwrap(),b);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn external_change_preserves_pending_and_never_replays_target() {
        let (store,root)=fixture();
        let p=Pending { previous_override: None,schema:1,committed:false,rotate:false,old_current:None,old_previous:None,target:encode_json(&config(true)).unwrap()};
        store.save_pending(&p).unwrap();
        fs::write(&store.paths.config_file,b"external").unwrap();
        assert!(is_config_transaction_error(&store.load().unwrap_err()));
        assert_eq!(fs::read(&store.paths.config_file).unwrap(),b"external");
        assert!(store.pending_path().exists());
        fs::remove_dir_all(root).unwrap();
    }
}
