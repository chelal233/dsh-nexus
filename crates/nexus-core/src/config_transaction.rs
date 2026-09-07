//! Private config/undo publication. Prepared records only roll back; committed
//! records never overwrite external edits. Raw records are not diagnostics.
use super::*;
use sha2::{Digest, Sha256};
use std::io::Write;
const FILE: &str = "config-write.pending.json";
const LIMIT: u64 = 4 * 1024 * 1024;
// Compact JSON byte arrays use at most four characters per byte. Three 4MiB
// inputs plus fixed fields fit below 49MiB; 64MiB is the read AND write bound.
const JOURNAL_LIMIT: u64 = 64 * 1024 * 1024;
#[derive(Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct Pending {
    schema: u32,
    committed: bool,
    rotate: bool,
    old_current: Option<Vec<u8>>,
    old_previous: Option<Vec<u8>>,
    target: Vec<u8>,
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
        Ok(value.get("preserve_current").and_then(|v|v.as_bool())==Some(true))
    }
    fn validated_current_bytes(&self) -> io::Result<Vec<u8>> {
        let bytes=read_regular_file_bounded(&self.paths.config_file,LIMIT)?.ok_or_else(|| invalid_data("Current configuration is missing; cannot preserve unknown settings"))?;
        let document:NexusConfigFile=decode_json(&bytes).map_err(|_| invalid_data("Current configuration is invalid; no files changed"))?;
        validate_config_document(&self.paths,&document)?;
        Ok(bytes)
    }
    pub fn pending_configuration_status(&self) -> io::Result<Option<serde_json::Value>> {
        let _guard=self.lock_gate()?;
        let id=if let Some(bytes)=read_regular_file_bounded(&self.pending_path(),JOURNAL_LIMIT)? { bytes_id(&bytes) }
        else if let Some(raw)=read_regular_file_bounded(&self.preserve_path(),4096)? {
            let decision:PreserveDecision=serde_json::from_slice(&raw).map_err(|_|invalid_data("Invalid preserve decision"))?;
            decision.pending_id
        } else { return Ok(None); };
        Ok(Some(serde_json::json!({"operation_id":id,"pending":true,"can_preserve":self.validated_current_bytes().is_ok()})))
    }
    /// Explicit preservation does not settle a prepared transaction first.
    /// Outer callers persist their decision while CONFIG_WRITE_GATE is held;
    /// subsequent readers also respect that decision if the inner write fails.
    pub fn preserve_current_after(&self, decide: impl FnOnce()->io::Result<()>) -> io::Result<()> {
        let _guard=self.lock_gate()?;
        self.validated_current_bytes()?;
        decide()?;
        self.preserve_pending_inner()
    }
    fn verify_pending_identity(&self, operation_id:&str) -> io::Result<()> {
        let id=if let Some(bytes)=read_regular_file_bounded(&self.pending_path(),JOURNAL_LIMIT)? { bytes_id(&bytes) }
        else if let Some(raw)=read_regular_file_bounded(&self.preserve_path(),4096)? {
            let decision:PreserveDecision=serde_json::from_slice(&raw).map_err(|_|invalid_data("Invalid preserve decision"))?;
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
        self.verify_pending_identity(operation_id)?;
        let current=self.validated_current_bytes()?;
        if let Some(raw)=read_regular_file_bounded(&self.preserve_path(),4096)? {
            let mut decision:PreserveDecision=serde_json::from_slice(&raw).map_err(|_|invalid_data("Invalid preserve decision"))?;
            decision.current_id=bytes_id(&current);
            write_private_json_atomic(&self.paths.root,&self.preserve_path(),&decision)?;
        }
        self.preserve_pending_inner()
    }
    fn preserve_pending_inner(&self) -> io::Result<()> {
        let current=self.validated_current_bytes()?;
        let Some(bytes)=read_regular_file_bounded(&self.pending_path(),JOURNAL_LIMIT)? else {
            if let Some(raw)=read_regular_file_bounded(&self.preserve_path(),4096)? {
                let decision:PreserveDecision=serde_json::from_slice(&raw).map_err(|_|invalid_data("Invalid preserve decision"))?;
                if decision.current_id!=bytes_id(&current) {return Err(invalid_data("Configuration changed during recovery"));}
                fs::remove_file(self.preserve_path())?;
            }
            return Ok(());
        };
        let id=bytes_id(&bytes);
        let decision=if let Some(raw)=read_regular_file_bounded(&self.preserve_path(),4096)? {
            let saved:PreserveDecision=serde_json::from_slice(&raw).map_err(|_|invalid_data("Invalid preserve decision"))?;
            if saved.pending_id!=id || saved.current_id!=bytes_id(&current) {return Err(invalid_data("Configuration changed during recovery"));}
            saved
        } else {
            let saved=PreserveDecision{pending_id:id.clone(),current_id:bytes_id(&current)};
            write_private_json_atomic(&self.paths.root,&self.preserve_path(),&saved)?;
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
    fn save_pending(&self, pending: &Pending) -> io::Result<()> {
        let bytes = serde_json::to_vec(pending).map_err(invalid_data)?;
        if bytes.len() as u64 > JOURNAL_LIMIT { return Err(invalid_data("Configuration transaction is too large")); }
        write_private_bytes_atomic(&self.paths.root, &self.pending_path(), &bytes)
    }
    pub(super) fn settle_pending_unlocked(&self) -> io::Result<()> {
        self.settle_pending_inner().map_err(pending_error)
    }
    fn settle_pending_inner(&self) -> io::Result<()> {
        if read_regular_file_bounded(&self.preserve_path(),4096)?.is_some() || self.outer_preserves_current()? {
            return self.preserve_pending_inner();
        }
        let Some(bytes) = read_regular_file_bounded(&self.pending_path(), JOURNAL_LIMIT)? else { return Ok(()); };
        let p: Pending = serde_json::from_slice(&bytes).map_err(invalid_data)?;
        if p.schema != 1 || p.target.len() as u64 > LIMIT
            || p.old_current.as_ref().is_some_and(|v| v.len() as u64 > LIMIT)
            || p.old_previous.as_ref().is_some_and(|v| v.len() as u64 > LIMIT)
            || (p.rotate && p.old_current.is_none()) { return Err(invalid_data("Invalid configuration transaction")); }
        let backup = self.paths.root.join(PREVIOUS_CONFIG_FILE);
        let current = read_regular_file_bounded(&self.paths.config_file, LIMIT)?;
        let previous = read_regular_file_bounded(&backup, LIMIT)?;
        let next_previous = if p.rotate { &p.old_current } else { &p.old_previous };
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
        let expected = if p.rotate { &p.old_current } else { &p.old_previous };
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
        let old_current = read_regular_file_bounded(&self.paths.config_file, LIMIT)?;
        let backup = self.paths.root.join(PREVIOUS_CONFIG_FILE);
        let old_previous = read_regular_file_bounded(&backup, LIMIT)?;
        let target = encode_json(document).map_err(invalid_data)?;
        if target.len() as u64 > LIMIT { return Err(invalid_data("Configuration exceeds 4MiB")); }
        let mut p = Pending { schema: 1, committed: false, rotate: valid_current && old_current.is_some(), old_current, old_previous, target };
        self.save_pending(&p)?;
        let result = (|| {
            self.restore_bytes(&self.paths.config_file, &p.old_current, &Some(p.target.clone()))?;
            if p.rotate { self.restore_bytes(&backup, &p.old_previous, &p.old_current)?; }
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
        NexusConfigFile { harness_preferences: Some(nexus_protocol::HarnessPreferencesPayload { telemetry_disabled: Some(value), ..Default::default() }), ..Default::default() }
    }
    #[test]
    fn prepared_cuts_roll_back_and_committed_cuts_only_settle() {
        for committed in [false, true] {
            for changed_previous in [false, true] {
                if committed && !changed_previous { continue; }
                let (store, root) = fixture();
                let a=encode_json(&config(false)).unwrap(); let b=encode_json(&config(true)).unwrap();
                let p=Pending { schema:1, committed, rotate:true, old_current:Some(a.clone()), old_previous:None, target:b.clone() };
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
        let p=Pending {schema:1,committed:true,rotate:false,old_current:None,old_previous:None,target:target.clone()};
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
        let p=Pending{schema:1,committed:false,rotate:true,old_current:Some(encode_json(&old).unwrap()),old_previous:None,target:encode_json(&config(true)).unwrap()};
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
            let p=Pending{schema:1,committed:false,rotate:true,old_current:Some(a),old_previous:Some(b"old backup bytes".to_vec()),target:b.clone()};
            store.save_pending(&p).unwrap();
            let x=if external {encode_json(&NexusConfigFile::default()).unwrap()}else{b};
            fs::write(&store.paths.config_file,&x).unwrap();
            fs::write(root.join(PREVIOUS_CONFIG_FILE),b"old backup bytes").unwrap();
            let status=store.pending_configuration_status().unwrap().unwrap();
            assert!(store.preserve_current_configuration("stale").is_err());
            store.preserve_current_configuration(status["operation_id"].as_str().unwrap()).unwrap();
            assert_eq!(fs::read(&store.paths.config_file).unwrap(),x);
            assert_eq!(fs::read(root.join(PREVIOUS_CONFIG_FILE)).unwrap(),b"old backup bytes");
            assert!(!store.pending_path().exists());
            assert!(store.pending_configuration_status().unwrap().is_none());
            fs::remove_dir_all(root).unwrap();
        }
    }
    #[test]
    fn outer_preserve_before_inner_marker_failure_blocks_rollback_and_replays() {
        let (store,root)=fixture();
        let a=encode_json(&config(false)).unwrap();let b=encode_json(&config(true)).unwrap();
        store.save_pending(&Pending{schema:1,committed:false,rotate:true,old_current:Some(a),old_previous:None,target:b.clone()}).unwrap();
        fs::write(&store.paths.config_file,&b).unwrap();
        fs::create_dir(store.preserve_path()).unwrap();
        assert!(store.preserve_current_after(|| write_private_json_atomic(&root,&root.join("cold-publication.json"),&serde_json::json!({"preserve_current":true}))).is_err());
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
        store.save_pending(&Pending{schema:1,committed:false,rotate:true,old_current:Some(encode_json(&config(false)).unwrap()),old_previous:None,target:b.clone()}).unwrap();
        fs::write(&store.paths.config_file,&b).unwrap();
        let reader=store.clone();let (start_tx,start_rx)=std::sync::mpsc::channel();
        let handle=std::thread::spawn(move || {start_rx.recv().unwrap();reader.load().unwrap()});
        store.preserve_current_after(|| {
            write_private_json_atomic(&root,&root.join("cold-publication.json"),&serde_json::json!({"preserve_current":true}))?;
            start_tx.send(()).unwrap();Ok(())
        }).unwrap();
        assert_eq!(handle.join().unwrap(),config(true));
        assert_eq!(fs::read(&store.paths.config_file).unwrap(),b);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn external_change_preserves_pending_and_never_replays_target() {
        let (store,root)=fixture();
        let p=Pending {schema:1,committed:false,rotate:false,old_current:None,old_previous:None,target:encode_json(&config(true)).unwrap()};
        store.save_pending(&p).unwrap();
        fs::write(&store.paths.config_file,b"external").unwrap();
        assert!(is_config_transaction_error(&store.load().unwrap_err()));
        assert_eq!(fs::read(&store.paths.config_file).unwrap(),b"external");
        assert!(store.pending_path().exists());
        fs::remove_dir_all(root).unwrap();
    }
}
