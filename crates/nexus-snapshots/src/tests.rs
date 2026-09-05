use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use crate::{
    restore::RestoreFault, CaptureRequest, FileScope, RecoverDecision, RestoreStatus,
    SnapshotError, SnapshotFileState, SnapshotKind, SnapshotStore, SnapshotStoreConfig,
    FILE_POLICY,
};

static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    root: PathBuf,
    data: PathBuf,
    home: PathBuf,
    profile: PathBuf,
    store: SnapshotStore,
}

impl Fixture {
    fn new() -> Self {
        Self::with_config(SnapshotStoreConfig::default())
    }

    fn with_config(config: SnapshotStoreConfig) -> Self {
        let sequence = TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "nexus-snapshots-test-{}-{sequence}",
            std::process::id()
        ));
        let data = root.join("nexus-data");
        let home = root.join("dsh-home");
        let profile = home.join("profiles").join("demo");
        fs::create_dir_all(&data).expect("data root creates");
        fs::create_dir_all(&profile).expect("profile creates");
        let store = SnapshotStore::with_config(&data, &home, "demo", config)
            .expect("snapshot store creates");
        let fixture = Self {
            root,
            data,
            home,
            profile,
            store,
        };
        fixture.write_complete_profile("1.0.0", "dark");
        fixture
    }

    fn write_complete_profile(&self, version: &str, theme: &str) {
        write(
            &self.profile.join("package.json"),
            &format!(
                r#"{{"name":"demo","version":"{version}","dsh":{{"profile":{{"bundles":["bundle-a","bundle-b"]}}}}}}"#
            ),
        );
        write(
            &self.profile.join("pnpm-lock.yaml"),
            "lockfileVersion: '9.0'\n",
        );
        write(&self.profile.join("pnpm-workspace.yaml"), "packages: []\n");
        write(
            &self.profile.join("cordis.patch.yml"),
            "- id: synthetic\n  config:\n    enabled: true\n",
        );
        write(
            &self.profile.join(".dsh-market/state.json"),
            r#"{"installed":[]}"#,
        );
        write(
            &self.home.join("settings.yaml"),
            &format!("theme: {theme}\n"),
        );
        write(&self.home.join("cordis.patch.yml"), "[]\n");
    }

    fn capture_healthy(&self) -> crate::SnapshotManifest {
        self.store
            .capture_healthy(CaptureRequest {
                dsh_version: "0.1.2-alpha.3".to_owned(),
            })
            .expect("healthy snapshot captures")
    }

    fn capture_manual(&self, label: &str) -> crate::SnapshotManifest {
        self.store
            .capture_manual(
                CaptureRequest {
                    dsh_version: "0.1.2-alpha.3".to_owned(),
                },
                Some(label.to_owned()),
            )
            .expect("manual snapshot captures")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn healthy_ring_rotates_without_touching_manual_snapshots() {
    let fixture = Fixture::with_config(SnapshotStoreConfig {
        healthy_slots: 2,
        max_manual_snapshots: 2,
    });
    let first = fixture.capture_healthy();
    fixture.write_complete_profile("1.0.1", "light");
    let second = fixture.capture_healthy();
    let manual_one = fixture.capture_manual("before experiment");
    fixture.write_complete_profile("1.0.2", "blue");
    let third = fixture.capture_healthy();
    let manual_two = fixture.capture_manual("after experiment");

    let list = fixture.store.list().expect("inventory loads");
    assert_eq!(list.len(), 4);
    assert!(!list
        .iter()
        .any(|item| item.snapshot_id == first.snapshot_id));
    assert!(list
        .iter()
        .any(|item| item.snapshot_id == second.snapshot_id));
    assert!(list
        .iter()
        .any(|item| item.snapshot_id == third.snapshot_id));
    assert!(list
        .iter()
        .any(|item| item.snapshot_id == manual_one.snapshot_id));
    assert!(list
        .iter()
        .any(|item| item.snapshot_id == manual_two.snapshot_id));
    assert!(
        list.iter()
            .filter(|item| matches!(item.kind, SnapshotKind::Healthy))
            .count()
            == 2
    );

    let error = fixture
        .store
        .capture_manual(
            CaptureRequest {
                dsh_version: "0.1.2-alpha.3".to_owned(),
            },
            Some("over capacity".to_owned()),
        )
        .expect_err("manual retention remains bounded");
    assert!(matches!(error, SnapshotError::Capacity(_)));
}

#[test]
fn missing_and_unparseable_files_are_explicit_manifest_states() {
    let fixture = Fixture::new();
    fs::remove_file(fixture.home.join("cordis.patch.yml")).expect("home patch removes");
    write(
        &fixture.profile.join("cordis.patch.yml"),
        "unterminated: [\n",
    );
    let manifest = fixture.capture_healthy();

    assert_eq!(manifest.files[3].state, SnapshotFileState::Omitted);
    assert!(manifest.files[3]
        .omitted_reason
        .as_deref()
        .is_some_and(|reason| reason.contains("invalid YAML")));
    assert_eq!(manifest.files[6].state, SnapshotFileState::Missing);
    assert_eq!(manifest.files[6].source_size, 0);
}

#[test]
fn required_package_must_be_present_and_valid() {
    let missing = Fixture::new();
    fs::remove_file(missing.profile.join("package.json")).expect("package removes");
    let error = missing
        .store
        .capture_healthy(CaptureRequest {
            dsh_version: "0.1.2-alpha.3".to_owned(),
        })
        .expect_err("missing package cannot become a recovery point");
    assert!(matches!(error, SnapshotError::InvalidManifest(_)));

    let invalid = Fixture::new();
    write(&invalid.profile.join("package.json"), "{broken");
    let error = invalid
        .store
        .capture_manual(
            CaptureRequest {
                dsh_version: "0.1.2-alpha.3".to_owned(),
            },
            Some("invalid package".to_owned()),
        )
        .expect_err("unparseable package cannot become a recovery point");
    assert!(matches!(error, SnapshotError::InvalidManifest(_)));

    let non_object = Fixture::new();
    write(&non_object.profile.join("package.json"), "[]");
    let error = non_object
        .store
        .capture_healthy(CaptureRequest {
            dsh_version: "0.1.2-alpha.3".to_owned(),
        })
        .expect_err("non-object package cannot become a recovery point");
    assert!(matches!(error, SnapshotError::InvalidManifest(_)));
}

#[test]
fn loaded_manifest_with_zero_present_files_is_rejected() {
    let fixture = Fixture::new();
    let manifest = fixture.capture_healthy();
    let directory = fixture
        .store
        .snapshot_directory(&manifest.snapshot_id)
        .expect("snapshot directory resolves");
    let manifest_path = directory.join("manifest.json");
    let mut value: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest_path).expect("manifest reads"))
            .expect("manifest parses");
    value["plugin_count"] = 0.into();
    value["file_count"] = 0.into();
    value["total_bytes"] = 0.into();
    for file in value["files"].as_array_mut().expect("files are an array") {
        file["state"] = "missing".into();
        file["source_size"] = 0.into();
        file["stored_size"] = 0.into();
        file["sha256"] = serde_json::Value::Null;
        file["mode"] = serde_json::Value::Null;
        file["redacted_paths"] = serde_json::json!([]);
        file["omitted_reason"] = serde_json::Value::Null;
    }
    for entry in fs::read_dir(directory.join("files")).expect("blob directory reads") {
        fs::remove_file(entry.expect("blob entry reads").path()).expect("blob removes");
    }
    fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&value).expect("manifest encodes"),
    )
    .expect("manifest mutates");

    let error = fixture
        .store
        .prepare_restore(&manifest.snapshot_id)
        .expect_err("zero-present snapshot cannot prepare");
    assert!(matches!(error, SnapshotError::InvalidManifest(_)));
}

#[test]
fn each_optional_file_may_be_missing_without_invalidating_restore() {
    for policy in FILE_POLICY.iter().skip(1) {
        let fixture = Fixture::new();
        let path = match policy.scope {
            FileScope::Profile => fixture.profile.join(policy.relative_path),
            FileScope::Home => fixture.home.join(policy.relative_path),
        };
        fs::remove_file(&path).expect("optional file removes");
        let manifest = fixture.capture_healthy();
        let entry = manifest
            .files
            .iter()
            .find(|entry| entry.path == policy.manifest_path)
            .expect("policy entry exists");
        assert_eq!(entry.state, SnapshotFileState::Missing);
        fixture
            .store
            .prepare_restore(&manifest.snapshot_id)
            .expect("optional missing file remains restorable");
    }
}

#[test]
fn healthy_rotation_recovers_both_publication_rename_crash_cuts() {
    let config = SnapshotStoreConfig {
        healthy_slots: 1,
        max_manual_snapshots: 2,
    };
    let fixture = Fixture::with_config(config);
    let original = fixture.capture_healthy();
    fixture.write_complete_profile("2.0.0", "new-before-old-cut");
    let error = fixture
        .store
        .capture_healthy_with_publication_fault(
            CaptureRequest {
                dsh_version: "0.1.2-alpha.3".to_owned(),
            },
            1,
        )
        .expect_err("crash cut follows destination-to-old rename");
    assert!(matches!(error, SnapshotError::InjectedFailure(_)));

    let recovered = SnapshotStore::with_config(&fixture.data, &fixture.home, "demo", config)
        .expect("store reconstructs");
    let list = recovered.list().expect("old slot recovers before listing");
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].snapshot_id, original.snapshot_id);

    fixture.write_complete_profile("3.0.0", "new-after-publish-cut");
    let error = recovered
        .capture_healthy_with_publication_fault(
            CaptureRequest {
                dsh_version: "0.1.2-alpha.3".to_owned(),
            },
            2,
        )
        .expect_err("crash cut follows next-to-destination rename");
    assert!(matches!(error, SnapshotError::InjectedFailure(_)));

    let recovered = SnapshotStore::with_config(&fixture.data, &fixture.home, "demo", config)
        .expect("store reconstructs again");
    let list = recovered
        .list()
        .expect("published slot wins over old orphan");
    assert_eq!(list.len(), 1);
    assert_ne!(list[0].snapshot_id, original.snapshot_id);
    let healthy = fixture.data.join("snapshots/demo/healthy");
    assert_eq!(
        fs::read_dir(healthy)
            .expect("healthy directory reads")
            .count(),
        1,
        "recovery removes only the slot-bound orphan"
    );
}

#[test]
fn oversized_source_is_rejected_before_parsing() {
    let fixture = Fixture::new();
    fs::write(
        fixture.profile.join("package.json"),
        vec![b'x'; FILE_POLICY[0].max_bytes as usize + 1],
    )
    .expect("oversized file writes");
    let error = fixture
        .store
        .capture_healthy(CaptureRequest {
            dsh_version: "0.1.2-alpha.3".to_owned(),
        })
        .expect_err("oversized source is rejected");
    assert!(matches!(error, SnapshotError::Oversized { .. }));
}

#[test]
fn corrupted_blob_is_reported_and_cannot_be_prepared() {
    let fixture = Fixture::new();
    let manifest = fixture.capture_healthy();
    let directory = fixture
        .store
        .snapshot_directory(&manifest.snapshot_id)
        .expect("snapshot directory resolves");
    fs::write(directory.join("files/0"), b"{}").expect("blob corrupts");

    let inspection = fixture
        .store
        .inspect(&manifest.snapshot_id)
        .expect("inspection completes without content");
    assert!(!inspection.valid);
    assert!(inspection.summary.is_none());
    let inventory = fixture
        .store
        .list_inspections()
        .expect("corrupt slot does not break bounded inventory");
    assert_eq!(inventory.len(), 1);
    assert!(!inventory[0].valid);
    let error = fixture
        .store
        .prepare_restore(&manifest.snapshot_id)
        .expect_err("corrupt snapshot cannot prepare");
    assert!(matches!(error, SnapshotError::Integrity(_)));
}

#[test]
fn unknown_and_traversal_manifest_paths_are_rejected() {
    for malicious in ["profile/unknown.yml", "../.credentials.yaml"] {
        let fixture = Fixture::new();
        let manifest = fixture.capture_healthy();
        let directory = fixture
            .store
            .snapshot_directory(&manifest.snapshot_id)
            .expect("snapshot directory resolves");
        let manifest_path = directory.join("manifest.json");
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&manifest_path).expect("manifest reads"))
                .expect("manifest parses");
        value["files"][0]["path"] = serde_json::Value::String(malicious.to_owned());
        fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&value).expect("manifest encodes"),
        )
        .expect("manifest mutates");

        let error = fixture
            .store
            .detail(&manifest.snapshot_id)
            .expect_err("unknown manifest path is rejected");
        assert!(matches!(error, SnapshotError::InvalidPath(_)));
    }
}

#[test]
fn snapshot_and_inspection_never_expose_dummy_secrets_and_restore_preserves_current_secret() {
    const SNAPSHOT_SECRET: &str = "DUMMY-SNAPSHOT-API-KEY-111";
    const CURRENT_SECRET: &str = "DUMMY-CURRENT-API-KEY-222";
    let fixture = Fixture::new();
    write(
        &fixture.home.join("settings.yaml"),
        &format!("provider:\n  apiKey: {SNAPSHOT_SECRET}\n  mode: snapshot\n"),
    );
    write(
        &fixture.home.join(".credentials.yaml"),
        "apiKey: DUMMY-FORBIDDEN-CREDENTIAL\n",
    );
    write(&fixture.home.join(".env"), "TOKEN=DUMMY-FORBIDDEN-ENV\n");
    write(
        &fixture.home.join("sessions/synthetic.json"),
        r#"{"token":"DUMMY-FORBIDDEN-SESSION"}"#,
    );
    let manifest = fixture.capture_healthy();
    assert_eq!(manifest.files[5].redacted_paths, vec!["/provider/apiKey"]);
    assert!(!tree_contains(&fixture.data, SNAPSHOT_SECRET.as_bytes()));

    write(
        &fixture.home.join("settings.yaml"),
        &format!(
            "provider:\n  apiKey: {CURRENT_SECRET}\n  password: DUMMY-CURRENT-PASSWORD\n  mode: current\n"
        ),
    );
    let ticket = fixture
        .store
        .prepare_restore(&manifest.snapshot_id)
        .expect("restore prepares");
    fixture
        .store
        .apply_restore(&ticket)
        .expect("restore applies");
    let restored =
        fs::read_to_string(fixture.home.join("settings.yaml")).expect("restored settings read");
    assert!(restored.contains(CURRENT_SECRET));
    assert!(restored.contains("DUMMY-CURRENT-PASSWORD"));
    assert!(!restored.contains(SNAPSHOT_SECRET));
    assert!(restored.contains("mode: snapshot"));
    let inspection = fixture
        .store
        .inspect(&manifest.snapshot_id)
        .expect("inspect works");
    let inspection_json = serde_json::to_string(&inspection).expect("inspection serializes");
    assert!(!inspection_json.contains(SNAPSHOT_SECRET));
    assert!(!inspection_json.contains(CURRENT_SECRET));
    assert!(!tree_contains(&fixture.data, SNAPSHOT_SECRET.as_bytes()));
    assert!(!tree_contains(&fixture.data, CURRENT_SECRET.as_bytes()));
    assert!(!tree_contains(&fixture.data, b"DUMMY-CURRENT-PASSWORD"));
    assert!(!tree_contains(&fixture.data, b"DUMMY-FORBIDDEN-CREDENTIAL"));
    assert!(!tree_contains(&fixture.data, b"DUMMY-FORBIDDEN-ENV"));
    assert!(!tree_contains(&fixture.data, b"DUMMY-FORBIDDEN-SESSION"));
    fixture
        .store
        .commit_restore(&ticket)
        .expect("restore commits");
    assert!(!fixture.home.join(".nexus-restore/demo").exists());
}

#[test]
fn missing_snapshot_entry_deletes_on_apply_and_rename_backup_rolls_back() {
    let fixture = Fixture::new();
    fs::remove_file(fixture.home.join("cordis.patch.yml")).expect("file removes before capture");
    let manifest = fixture.capture_healthy();
    write(&fixture.home.join("cordis.patch.yml"), "current: true\n");
    let ticket = fixture
        .store
        .prepare_restore(&manifest.snapshot_id)
        .expect("restore prepares");
    fixture
        .store
        .apply_restore(&ticket)
        .expect("restore applies");
    assert!(!fixture.home.join("cordis.patch.yml").exists());
    fixture
        .store
        .recover_restore(&ticket, RecoverDecision::Rollback)
        .expect("explicit rollback restores original");
    assert_eq!(
        fixture
            .store
            .rollback_restore(&ticket)
            .expect("rollback is idempotent")
            .status,
        RestoreStatus::RolledBack
    );
    assert_eq!(
        fs::read_to_string(fixture.home.join("cordis.patch.yml")).expect("file restored"),
        "current: true\n"
    );
}

#[test]
fn deterministic_mid_apply_failure_supports_explicit_rollback() {
    let fixture = Fixture::new();
    let manifest = fixture.capture_healthy();
    fixture.write_complete_profile("2.0.0", "current");
    let ticket = fixture
        .store
        .prepare_restore(&manifest.snapshot_id)
        .expect("restore prepares");
    let error = fixture
        .store
        .apply_restore_with_fault(&ticket, 1)
        .expect_err("fault is injected after one operation");
    assert!(matches!(error, SnapshotError::InjectedFailure(_)));
    let pending = fixture
        .store
        .pending_restores()
        .expect("pending restore lists");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].status, RestoreStatus::Applying);

    fixture
        .store
        .recover_restore(&ticket, RecoverDecision::Rollback)
        .expect("outer prepared decision rolls back");
    let package = fs::read_to_string(fixture.profile.join("package.json"))
        .expect("package restored to pre-apply content");
    assert!(package.contains("2.0.0"));
    assert!(fixture
        .store
        .pending_restores()
        .expect("pending lists")
        .is_empty());
}

#[test]
fn deterministic_mid_apply_failure_can_resume_then_commit_after_materialization() {
    let fixture = Fixture::new();
    let manifest = fixture.capture_healthy();
    fixture.write_complete_profile("2.0.0", "current");
    let ticket = fixture
        .store
        .prepare_restore(&manifest.snapshot_id)
        .expect("restore prepares");
    fixture
        .store
        .apply_restore_with_fault(&ticket, 2)
        .expect_err("fault is injected");
    let resumed = fixture
        .store
        .recover_restore(&ticket, RecoverDecision::ResumeApply)
        .expect("outer decision resumes apply");
    assert_eq!(resumed.status, RestoreStatus::Applied);
    assert_eq!(
        fixture
            .store
            .apply_restore(&ticket)
            .expect("apply is idempotent")
            .status,
        RestoreStatus::Applied
    );
    assert!(resumed.materialization_pending);
    let blocked = fixture
        .store
        .commit_restore(&ticket)
        .expect_err("commit waits for dependency materialization");
    assert!(matches!(blocked, SnapshotError::InvalidState(_)));
    fixture
        .store
        .mark_materialized(&ticket)
        .expect("caller reports successful pnpm install");
    let committed = fixture
        .store
        .recover_restore(&ticket, RecoverDecision::ResumeCommit)
        .expect("outer committed decision removes undo copies");
    assert_eq!(committed.status, RestoreStatus::Committed);
    assert_eq!(
        fixture
            .store
            .commit_restore(&ticket)
            .expect("commit is idempotent")
            .status,
        RestoreStatus::Committed
    );
}

#[test]
fn unparseable_current_structured_file_fails_closed_before_mutation() {
    let fixture = Fixture::new();
    let manifest = fixture.capture_healthy();
    write(&fixture.home.join("settings.yaml"), "broken: [\n");
    let error = fixture
        .store
        .prepare_restore(&manifest.snapshot_id)
        .expect_err("unsafe current YAML blocks restore");
    assert!(matches!(error, SnapshotError::StructuredData { .. }));
    assert_eq!(
        fs::read_to_string(fixture.home.join("settings.yaml")).expect("current file remains"),
        "broken: [\n"
    );
}

#[test]
fn rollback_rejects_a_reparse_backup_directory_at_point_of_use() {
    let fixture = Fixture::new();
    let manifest = fixture.capture_healthy();
    fixture.write_complete_profile("2.0.0", "current");
    let ticket = fixture
        .store
        .prepare_restore(&manifest.snapshot_id)
        .expect("restore prepares");
    fixture
        .store
        .apply_restore_with_fault(&ticket, 1)
        .expect_err("first operation applies before interruption");

    let backup = fixture.store.backup_root().join(&ticket.ticket_id);
    let outside = fixture.root.join("outside-backup");
    fs::rename(&backup, &outside).expect("real backup moves outside its trusted path");
    create_directory_link(&outside, &backup);

    let error = fixture
        .store
        .rollback_restore(&ticket)
        .expect_err("rollback rejects a linked backup ancestor");
    assert!(matches!(error, SnapshotError::UnsafePath(_)));
    assert!(outside.join("0.original").exists());
    let current = fs::read_to_string(fixture.profile.join("package.json"))
        .expect("current package remains readable");
    assert!(current.contains("1.0.0"));

    remove_directory_link(&backup);
}

#[test]
fn restore_namespace_crash_cuts_are_idempotently_recoverable() {
    let fixture = Fixture::new();
    let manifest = fixture.capture_healthy();
    fixture.write_complete_profile("2.0.0", "current");
    let ticket = fixture
        .store
        .prepare_restore(&manifest.snapshot_id)
        .expect("restore prepares");
    fixture
        .store
        .apply_restore(&ticket)
        .expect("restore applies");

    let error = fixture
        .store
        .rollback_restore_with_fault(&ticket, RestoreFault::AfterNamespace(0))
        .expect_err("rollback stops after durable namespace change");
    assert!(matches!(error, SnapshotError::InjectedFailure(_)));
    let recovered = SnapshotStore::new(&fixture.data, &fixture.home, "demo")
        .expect("store reconstructs after rollback cut");
    recovered
        .rollback_restore(&ticket)
        .expect("rollback resumes from durable namespace state");
    let package = fs::read_to_string(fixture.profile.join("package.json"))
        .expect("original package is restored");
    assert!(package.contains("2.0.0"));

    let ticket = recovered
        .prepare_restore(&manifest.snapshot_id)
        .expect("record-cut restore prepares");
    recovered
        .apply_restore(&ticket)
        .expect("record-cut restore applies");
    let error = recovered
        .rollback_restore_with_fault(&ticket, RestoreFault::AfterRecord(0))
        .expect_err("rollback stops after its durable operation record");
    assert!(matches!(error, SnapshotError::InjectedFailure(_)));
    let recovered_after_record = SnapshotStore::new(&fixture.data, &fixture.home, "demo")
        .expect("store reconstructs after rollback record cut");
    recovered_after_record
        .rollback_restore(&ticket)
        .expect("rollback record cut is already idempotent");
    let package = fs::read_to_string(fixture.profile.join("package.json"))
        .expect("original package remains restored");
    assert!(package.contains("2.0.0"));

    let second = fixture.capture_healthy();
    fixture.write_complete_profile("3.0.0", "changed-again");
    let ticket = recovered
        .prepare_restore(&second.snapshot_id)
        .expect("second restore prepares");
    recovered
        .apply_restore(&ticket)
        .expect("second restore applies");
    recovered
        .mark_materialized(&ticket)
        .expect("materialization acknowledged");
    let error = recovered
        .commit_restore_with_fault(&ticket, RestoreFault::AfterNamespace(0))
        .expect_err("commit stops after durable backup cleanup");
    assert!(matches!(error, SnapshotError::InjectedFailure(_)));
    let recovered_again = SnapshotStore::new(&fixture.data, &fixture.home, "demo")
        .expect("store reconstructs after commit cut");
    let committed = recovered_again
        .commit_restore(&ticket)
        .expect("commit resumes after cleanup-before-record cut");
    assert_eq!(committed.status, RestoreStatus::Committed);
    assert!(!tree_contains(
        &fixture.home.join(".nexus-restore"),
        b"3.0.0"
    ));

    let third = recovered_again
        .capture_healthy(CaptureRequest {
            dsh_version: "0.1.2-alpha.3".to_owned(),
        })
        .expect("third snapshot captures");
    fixture.write_complete_profile("4.0.0", "changed-third-time");
    let ticket = recovered_again
        .prepare_restore(&third.snapshot_id)
        .expect("commit record-cut restore prepares");
    recovered_again
        .apply_restore(&ticket)
        .expect("commit record-cut restore applies");
    recovered_again
        .mark_materialized(&ticket)
        .expect("third materialization acknowledged");
    let error = recovered_again
        .commit_restore_with_fault(&ticket, RestoreFault::AfterRecord(0))
        .expect_err("commit stops after its durable terminal record");
    assert!(matches!(error, SnapshotError::InjectedFailure(_)));
    let recovered_terminal = SnapshotStore::new(&fixture.data, &fixture.home, "demo")
        .expect("store reconstructs after commit record cut");
    let committed = recovered_terminal
        .commit_restore(&ticket)
        .expect("terminal commit remains idempotent");
    assert_eq!(committed.status, RestoreStatus::Committed);
}

#[test]
fn reparse_or_symlink_ancestor_is_rejected() {
    let fixture = Fixture::new();
    let market = fixture.profile.join(".dsh-market");
    fs::remove_dir_all(&market).expect("real market directory removes");
    let outside = fixture.root.join("outside");
    fs::create_dir(&outside).expect("outside directory creates");
    write(&outside.join("state.json"), r#"{"token":"DUMMY-OUTSIDE"}"#);

    create_directory_link(&outside, &market);

    let error = fixture
        .store
        .capture_healthy(CaptureRequest {
            dsh_version: "0.1.2-alpha.3".to_owned(),
        })
        .expect_err("reparse ancestor is rejected");
    assert!(matches!(error, SnapshotError::UnsafePath(_)));

    remove_directory_link(&market);
}

#[cfg(windows)]
fn create_directory_link(target: &Path, link: &Path) {
    use std::process::{Command, Stdio};
    let status = Command::new("cmd")
        .arg("/C")
        .arg("mklink")
        .arg("/J")
        .arg(link)
        .arg(target)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("mklink command runs");
    assert!(status.success(), "junction fixture must be created");
}

#[cfg(unix)]
fn create_directory_link(target: &Path, link: &Path) {
    std::os::unix::fs::symlink(target, link).expect("symlink fixture creates");
}

#[cfg(windows)]
fn remove_directory_link(link: &Path) {
    fs::remove_dir(link).expect("junction removes without traversing target");
}

#[cfg(unix)]
fn remove_directory_link(link: &Path) {
    fs::remove_file(link).expect("symlink removes without traversing target");
}

fn write(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("parent directories create");
    }
    fs::write(path, content.as_bytes()).expect("synthetic fixture writes");
}

fn tree_contains(root: &Path, needle: &[u8]) -> bool {
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        for entry in fs::read_dir(path).expect("synthetic tree reads") {
            let entry = entry.expect("synthetic entry reads");
            let metadata = fs::symlink_metadata(entry.path()).expect("synthetic metadata reads");
            if metadata.is_dir() && !metadata.file_type().is_symlink() {
                pending.push(entry.path());
            } else if metadata.is_file() {
                let bytes = fs::read(entry.path()).expect("synthetic file reads");
                if bytes.windows(needle.len()).any(|window| window == needle) {
                    return true;
                }
            }
        }
    }
    false
}
