//! Agent-side serialized ownership and protocol mapping for content snapshots.

use std::{io, path::PathBuf, sync::Arc};

use crate::dsh::same_native_path;
use nexus_core::{
    BoundSnapshotRestore, CheckpointRestoreJournal, CheckpointRestorePhase, ConfigStore,
    NexusPaths, DEFAULT_HEALTHY_SNAPSHOT_SLOTS, DEFAULT_MAX_MANUAL_SNAPSHOTS,
};
use nexus_protocol::{
    CheckpointContentState, CheckpointRestoreStatus, SnapshotDetailResponse, SnapshotFilePayload,
    SnapshotFileStatePayload, SnapshotInspectionPayload, SnapshotKindPayload, SnapshotReference,
    SnapshotSummaryPayload, API_VERSION,
};
use nexus_snapshots::{
    CaptureRequest, RecoverDecision, RestoreOutcome, RestoreStatus, SnapshotContent,
    SnapshotFileState, SnapshotInspection, SnapshotKind, SnapshotManifest, SnapshotStore,
    SnapshotStoreConfig, SnapshotSummary,
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

const MAX_FILE_CONTENT_BYTES: usize = 64 * 1024;
const MAX_DETAIL_CONTENT_BYTES: usize = 256 * 1024;

#[derive(Clone)]
pub(crate) struct SnapshotCoordinator {
    paths: NexusPaths,
    dsh_home: Option<PathBuf>,
    configuration_error: Option<String>,
    owner: Arc<Semaphore>,
    healthy_error: Arc<std::sync::Mutex<Option<String>>>,
}

pub(crate) struct SnapshotLease {
    store: Arc<SnapshotStore>,
    owner: Arc<OwnedSemaphorePermit>,
}

impl SnapshotCoordinator {
    pub(crate) fn new(paths: NexusPaths, dsh_home: io::Result<PathBuf>) -> Self {
        let (dsh_home, configuration_error) = match dsh_home {
            Ok(home) => (Some(home), None),
            Err(error) => (None, Some(error.to_string())),
        };
        Self {
            paths,
            dsh_home,
            configuration_error,
            owner: Arc::new(Semaphore::new(1)),
            healthy_error: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    pub(crate) fn configured_dsh_home(&self) -> io::Result<&PathBuf> {
        self.dsh_home.as_ref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                self.configuration_error
                    .clone()
                    .unwrap_or_else(|| "DSH home is unavailable".to_owned()),
            )
        })
    }

    pub(crate) async fn acquire(&self, profile: String) -> io::Result<SnapshotLease> {
        let owner = Arc::clone(&self.owner)
            .acquire_owned()
            .await
            .map_err(|_| io::Error::other("snapshot I/O owner is closed"))?;
        let paths = self.paths.clone();
        let dsh_home = self.configured_dsh_home()?.clone();
        let store = tokio::task::spawn_blocking(move || build_store(paths, dsh_home, profile))
            .await
            .map_err(|error| io::Error::other(format!("snapshot store task failed: {error}")))??;
        Ok(SnapshotLease {
            store: Arc::new(store),
            owner: Arc::new(owner),
        })
    }

    pub(crate) async fn acquire_bound(
        &self,
        binding: &BoundSnapshotRestore,
    ) -> io::Result<SnapshotLease> {
        if binding.profile_name != binding.ticket.profile_name {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "restore ticket profile does not match its outer binding",
            ));
        }
        let lease = self.acquire(binding.profile_name.clone()).await?;
        if !same_native_path(lease.store().dsh_home(), &binding.dsh_home) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "restore ticket belongs to a different resolved DSH home",
            ));
        }
        Ok(lease)
    }

    pub(crate) async fn list_inspections(
        &self,
        profile: String,
    ) -> io::Result<Vec<SnapshotInspectionPayload>> {
        let lease = self.acquire(profile).await?;
        lease
            .call(|store| store.list_inspections())
            .await
            .map(|items| items.into_iter().map(inspection_payload).collect())
    }

    pub(crate) async fn detail(
        &self,
        profile: String,
        snapshot_id: String,
    ) -> io::Result<SnapshotDetailResponse> {
        let lease = self.acquire(profile).await?;
        let content = lease.call(move |store| store.content(&snapshot_id)).await?;
        Ok(detail_response(content))
    }

    pub(crate) async fn inspect(
        &self,
        profile: String,
        snapshot_id: String,
    ) -> io::Result<SnapshotInspectionPayload> {
        let lease = self.acquire(profile).await?;
        let content_id = snapshot_id.clone();
        let inspection = lease.call(move |store| store.inspect(&snapshot_id)).await?;
        let mut payload = inspection_payload(inspection);
        if payload.valid {
            let content = lease.call(move |store| store.content(&content_id)).await?;
            payload.files = detail_files(content);
        }
        Ok(payload)
    }

    pub(crate) fn healthy_error(&self) -> Option<String> {
        self.healthy_error
            .lock()
            .ok()
            .and_then(|error| error.clone())
    }

    pub(crate) fn record_healthy_result(&self, result: &io::Result<SnapshotReference>) {
        if let Ok(mut current) = self.healthy_error.lock() {
            *current = result.as_ref().err().map(ToString::to_string);
        }
    }

    pub(crate) async fn capture_healthy(
        &self,
        profile: String,
        dsh_version: String,
    ) -> io::Result<SnapshotReference> {
        let lease = self.acquire(profile).await?;
        let manifest = lease
            .call(move |store| store.capture_healthy(CaptureRequest { dsh_version }))
            .await?;
        Ok(snapshot_reference(&manifest))
    }
}

impl SnapshotLease {
    pub(crate) fn store(&self) -> &SnapshotStore {
        &self.store
    }

    pub(crate) async fn call<T, F>(&self, operation: F) -> io::Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&SnapshotStore) -> nexus_snapshots::Result<T> + Send + 'static,
    {
        let store = Arc::clone(&self.store);
        let owner = Arc::clone(&self.owner);
        tokio::task::spawn_blocking(move || {
            let _owner = owner;
            operation(&store).map_err(snapshot_error)
        })
        .await
        .map_err(|error| io::Error::other(format!("snapshot I/O task failed: {error}")))?
    }

    pub(crate) async fn capture_manual(
        &self,
        dsh_version: String,
        label: Option<String>,
    ) -> io::Result<SnapshotManifest> {
        self.call(move |store| store.capture_manual(CaptureRequest { dsh_version }, label))
            .await
    }

    pub(crate) async fn prepare(
        &self,
        snapshot_id: String,
    ) -> io::Result<nexus_snapshots::RestoreTicket> {
        self.call(move |store| store.prepare_restore(&snapshot_id))
            .await
    }

    pub(crate) async fn apply(
        &self,
        ticket: nexus_snapshots::RestoreTicket,
    ) -> io::Result<RestoreOutcome> {
        self.call(move |store| store.apply_restore(&ticket)).await
    }

    pub(crate) async fn resume_apply(
        &self,
        ticket: nexus_snapshots::RestoreTicket,
    ) -> io::Result<RestoreOutcome> {
        self.call(move |store| store.recover_restore(&ticket, RecoverDecision::ResumeApply))
            .await
    }

    pub(crate) async fn mark_materialized(
        &self,
        ticket: nexus_snapshots::RestoreTicket,
    ) -> io::Result<RestoreOutcome> {
        self.call(move |store| store.mark_materialized(&ticket))
            .await
    }

    pub(crate) async fn commit(
        &self,
        ticket: nexus_snapshots::RestoreTicket,
    ) -> io::Result<RestoreOutcome> {
        self.call(move |store| store.commit_restore(&ticket)).await
    }

    pub(crate) async fn rollback(
        &self,
        ticket: nexus_snapshots::RestoreTicket,
    ) -> io::Result<RestoreOutcome> {
        self.call(move |store| store.rollback_restore(&ticket))
            .await
    }
}

pub(crate) fn binding_for(
    lease: &SnapshotLease,
    ticket: nexus_snapshots::RestoreTicket,
) -> BoundSnapshotRestore {
    BoundSnapshotRestore {
        profile_name: ticket.profile_name.clone(),
        dsh_home: lease.store().dsh_home().to_path_buf(),
        ticket,
    }
}

pub(crate) fn snapshot_reference(manifest: &SnapshotManifest) -> SnapshotReference {
    SnapshotReference {
        snapshot_id: manifest.snapshot_id.clone(),
        summary: summary_payload(&SnapshotSummary::from(manifest)),
    }
}

pub(crate) fn restore_status(
    journal: &CheckpointRestoreJournal,
    outcome: Option<&RestoreOutcome>,
) -> Option<CheckpointRestoreStatus> {
    let binding = journal.intent.snapshot.as_ref()?;
    let engine_status = outcome.map(|value| value.status);
    let materialization_pending = outcome
        .map(|value| value.materialization_pending)
        .unwrap_or(
            binding.ticket.needs_materialization
                && journal.phase == CheckpointRestorePhase::Prepared,
        );
    let state = match (journal.phase, engine_status, materialization_pending) {
        (CheckpointRestorePhase::Committed, _, _) | (_, Some(RestoreStatus::Committed), _) => {
            CheckpointContentState::Committed
        }
        (_, Some(RestoreStatus::RolledBack), _) => CheckpointContentState::RolledBack,
        (_, _, true) => CheckpointContentState::MaterializationPending,
        (_, Some(RestoreStatus::Applied), _) => CheckpointContentState::Applied,
        _ => CheckpointContentState::Prepared,
    };
    Some(CheckpointRestoreStatus {
        checkpoint_id: journal.intent.checkpoint_id.clone(),
        snapshot_id: binding.ticket.snapshot_id.clone(),
        ticket_id: binding.ticket.ticket_id.clone(),
        state,
        materialization_pending,
        retryable: true,
        abortable: journal.phase == CheckpointRestorePhase::Prepared,
        error: journal.error.clone(),
    })
}

fn build_store(paths: NexusPaths, dsh_home: PathBuf, profile: String) -> io::Result<SnapshotStore> {
    let config = ConfigStore::new(paths.clone()).load()?;
    let (healthy_slots, max_manual_snapshots) = config
        .snapshots
        .map(|value| {
            (
                value.healthy_slots as usize,
                value.max_manual_snapshots as usize,
            )
        })
        .unwrap_or((DEFAULT_HEALTHY_SNAPSHOT_SLOTS, DEFAULT_MAX_MANUAL_SNAPSHOTS));
    SnapshotStore::with_config(
        paths.root,
        dsh_home,
        profile,
        SnapshotStoreConfig {
            healthy_slots,
            max_manual_snapshots,
        },
    )
    .map_err(snapshot_error)
}

fn snapshot_error(error: nexus_snapshots::SnapshotError) -> io::Error {
    io::Error::other(error.to_string())
}

fn summary_payload(summary: &SnapshotSummary) -> SnapshotSummaryPayload {
    let (kind, label) = match &summary.kind {
        SnapshotKind::Healthy => (SnapshotKindPayload::Healthy, None),
        SnapshotKind::Manual { label } => (SnapshotKindPayload::Manual, label.clone()),
    };
    SnapshotSummaryPayload {
        snapshot_id: summary.snapshot_id.clone(),
        created_unix_ms: summary.created_unix_ms,
        profile_name: summary.profile_name.clone(),
        kind,
        label,
        dsh_version: summary.dsh_version.clone(),
        plugin_count: summary.plugin_count,
        file_count: summary.file_count,
        total_bytes: summary.total_bytes,
    }
}

fn inspection_payload(inspection: SnapshotInspection) -> SnapshotInspectionPayload {
    SnapshotInspectionPayload {
        snapshot_id: inspection.snapshot_id,
        valid: inspection.valid,
        summary: inspection.summary.as_ref().map(summary_payload),
        errors: inspection.errors,
        files: Vec::new(),
    }
}

fn detail_response(content: SnapshotContent) -> SnapshotDetailResponse {
    let summary = summary_payload(&SnapshotSummary::from(&content.manifest));
    let files = detail_files(content);
    SnapshotDetailResponse {
        api_version: API_VERSION.to_owned(),
        summary,
        files,
    }
}

fn detail_files(content: SnapshotContent) -> Vec<SnapshotFilePayload> {
    let SnapshotContent { manifest, files } = content;
    let mut remaining = MAX_DETAIL_CONTENT_BYTES;
    manifest
        .files
        .into_iter()
        .zip(files)
        .map(|(record, bytes)| {
            let (content, content_truncated) = match bytes {
                Some(bytes) => {
                    let limit = remaining.min(MAX_FILE_CONTENT_BYTES).min(bytes.len());
                    let mut end = limit;
                    while end > 0 && std::str::from_utf8(&bytes[..end]).is_err() {
                        end -= 1;
                    }
                    remaining = remaining.saturating_sub(end);
                    (
                        Some(String::from_utf8_lossy(&bytes[..end]).into_owned()),
                        end < bytes.len(),
                    )
                }
                None => (None, false),
            };
            SnapshotFilePayload {
                path: record.path,
                state: match record.state {
                    SnapshotFileState::Present => SnapshotFileStatePayload::Present,
                    SnapshotFileState::Missing => SnapshotFileStatePayload::Missing,
                    SnapshotFileState::Omitted => SnapshotFileStatePayload::Omitted,
                },
                source_size: record.source_size,
                stored_size: record.stored_size,
                sha256: record.sha256,
                mode: record.mode,
                redacted_paths: record.redacted_paths,
                omitted_reason: record.omitted_reason,
                content,
                content_truncated,
                content_note: content_truncated.then(|| {
                    format!(
                        "content is truncated by the {} byte per-file and {} byte response limits",
                        MAX_FILE_CONTENT_BYTES, MAX_DETAIL_CONTENT_BYTES
                    )
                }),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexus_core::{NexusConfigFile, SnapshotsConfig};
    use std::{
        fs,
        path::Path,
        sync::atomic::{AtomicU64, Ordering},
        time::Duration,
    };

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    struct Fixture {
        root: PathBuf,
        paths: NexusPaths,
        home: PathBuf,
    }

    impl Fixture {
        fn new(label: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "nexus-agent-snapshot-{}-{}-{}",
                label,
                std::process::id(),
                NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&root);
            let paths = NexusPaths::from_root(root.join("nexus-data"));
            let home = root.join("dsh-home");
            let profile = home.join("profiles/demo");
            fs::create_dir_all(&paths.root).expect("creates synthetic Nexus data root");
            fs::create_dir_all(&profile).expect("creates synthetic profile");
            write(
                &profile.join("package.json"),
                r#"{"name":"demo","version":"1.0.0","dsh":{"profile":{"bundles":["bundle-a"]}}}"#,
            );
            write(&profile.join("pnpm-lock.yaml"), "lockfileVersion: '9.0'\n");
            write(&profile.join("pnpm-workspace.yaml"), "packages: []\n");
            write(&profile.join("cordis.patch.yml"), "[]\n");
            write(
                &profile.join(".dsh-market/state.json"),
                r#"{"installed":[]}"#,
            );
            write(
                &home.join("settings.yaml"),
                "provider:\n  apiKey: DUMMY-SNAPSHOT-SECRET\n  mode: snapshot\n",
            );
            write(&home.join("cordis.patch.yml"), "[]\n");
            Self { root, paths, home }
        }

        fn coordinator(&self) -> SnapshotCoordinator {
            SnapshotCoordinator::new(self.paths.clone(), Ok(self.home.clone()))
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn write(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("creates synthetic parent");
        }
        fs::write(path, content).expect("writes synthetic profile file");
    }

    #[tokio::test]
    async fn agent_mapping_redacts_detail_and_restore_preserves_current_secret() {
        let fixture = Fixture::new("redaction");
        let coordinator = fixture.coordinator();
        let lease = coordinator
            .acquire("demo".to_owned())
            .await
            .expect("acquires snapshot owner");
        let manifest = lease
            .capture_manual("1.2.3".to_owned(), Some("safe point".to_owned()))
            .await
            .expect("captures manual snapshot");
        assert_eq!(manifest.files[5].redacted_paths, vec!["/provider/apiKey"]);
        drop(lease);

        let detail = coordinator
            .detail("demo".to_owned(), manifest.snapshot_id.clone())
            .await
            .expect("loads bounded snapshot detail");
        let encoded = serde_json::to_string(&detail).expect("detail serializes");
        assert!(!encoded.contains("DUMMY-SNAPSHOT-SECRET"));
        assert!(encoded.contains("/provider/apiKey"));
        assert_eq!(detail.files.len(), 7);
        assert!(detail
            .files
            .iter()
            .find(|file| file.path == "profile/package.json")
            .and_then(|file| file.content.as_deref())
            .is_some_and(|content| content.contains("bundle-a")));
        assert!(detail
            .files
            .iter()
            .find(|file| file.path == "home/settings.yaml")
            .and_then(|file| file.content.as_deref())
            .is_some_and(|content| content.contains("mode: snapshot")));
        let inspection = coordinator
            .inspect("demo".to_owned(), manifest.snapshot_id.clone())
            .await
            .expect("explicit inspection includes bounded content");
        assert!(inspection.valid);
        assert_eq!(inspection.files.len(), 7);

        write(
            &fixture.home.join("settings.yaml"),
            "provider:\n  apiKey: DUMMY-CURRENT-SECRET\n  mode: current\n",
        );
        let lease = coordinator
            .acquire("demo".to_owned())
            .await
            .expect("reacquires snapshot owner");
        let ticket = lease
            .prepare(manifest.snapshot_id)
            .await
            .expect("prepares restore");
        let outcome = lease.apply(ticket.clone()).await.expect("applies restore");
        assert!(!outcome.materialization_pending);
        lease.commit(ticket).await.expect("commits restore");
        let restored = fs::read_to_string(fixture.home.join("settings.yaml"))
            .expect("reads restored settings");
        assert!(restored.contains("mode: snapshot"));
        assert!(restored.contains("DUMMY-CURRENT-SECRET"));
        assert!(!restored.contains("DUMMY-SNAPSHOT-SECRET"));
    }

    #[tokio::test]
    async fn snapshot_detail_truncates_large_allowed_content() {
        let fixture = Fixture::new("content-limit");
        let package = fixture.home.join("profiles/demo/package.json");
        write(
            &package,
            &format!(
                r#"{{"name":"demo","dsh":{{"profile":{{"bundles":[]}}}},"padding":"{}"}}"#,
                "x".repeat(MAX_FILE_CONTENT_BYTES + 1024)
            ),
        );
        let coordinator = fixture.coordinator();
        let lease = coordinator
            .acquire("demo".to_owned())
            .await
            .expect("owner acquires");
        let manifest = lease
            .capture_manual("1.0.0".to_owned(), None)
            .await
            .expect("snapshot captures");
        drop(lease);
        let detail = coordinator
            .detail("demo".to_owned(), manifest.snapshot_id)
            .await
            .expect("detail loads");
        let package = detail
            .files
            .iter()
            .find(|file| file.path == "profile/package.json")
            .expect("package record");
        assert!(package.content_truncated);
        assert!(package
            .content_note
            .as_deref()
            .is_some_and(|note| note.contains("65536 byte")));
        assert_eq!(
            package.content.as_ref().expect("bounded content").len(),
            MAX_FILE_CONTENT_BYTES
        );
    }

    #[tokio::test]
    async fn configured_healthy_ring_is_independent_from_manual_snapshots() {
        let fixture = Fixture::new("rotation");
        ConfigStore::new(fixture.paths.clone())
            .write(&NexusConfigFile {
                snapshots: Some(SnapshotsConfig {
                    healthy_slots: 2,
                    max_manual_snapshots: 4,
                }),
                ..Default::default()
            })
            .expect("writes Nexus-owned snapshot policy");
        let coordinator = fixture.coordinator();
        let first = coordinator
            .capture_healthy("demo".to_owned(), "1.0.0".to_owned())
            .await
            .expect("captures first healthy snapshot");
        let manual = {
            let lease = coordinator
                .acquire("demo".to_owned())
                .await
                .expect("acquires manual snapshot owner");
            lease
                .capture_manual("1.0.0".to_owned(), Some("manual".to_owned()))
                .await
                .expect("captures manual snapshot")
        };
        coordinator
            .capture_healthy("demo".to_owned(), "1.0.1".to_owned())
            .await
            .expect("captures second healthy snapshot");
        coordinator
            .capture_healthy("demo".to_owned(), "1.0.2".to_owned())
            .await
            .expect("rotates healthy snapshot");
        let inventory = coordinator
            .list_inspections("demo".to_owned())
            .await
            .expect("lists validated inventory");
        assert_eq!(inventory.len(), 3);
        assert!(!inventory
            .iter()
            .any(|item| item.snapshot_id == first.snapshot_id));
        assert!(inventory
            .iter()
            .any(|item| item.snapshot_id == manual.snapshot_id));
        assert_eq!(
            inventory
                .iter()
                .filter(|item| {
                    item.summary
                        .as_ref()
                        .is_some_and(|summary| summary.kind == SnapshotKindPayload::Healthy)
                })
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn bound_ticket_rejects_a_different_resolved_home() {
        let fixture = Fixture::new("binding");
        let coordinator = fixture.coordinator();
        let lease = coordinator
            .acquire("demo".to_owned())
            .await
            .expect("acquires snapshot owner");
        let manifest = lease
            .capture_manual("1.0.0".to_owned(), None)
            .await
            .expect("captures manual snapshot");
        let ticket = lease
            .prepare(manifest.snapshot_id)
            .await
            .expect("prepares ticket");
        let mut binding = binding_for(&lease, ticket);
        binding.dsh_home = fixture.root.join("other-home");
        drop(lease);
        assert!(coordinator.acquire_bound(&binding).await.is_err());
    }

    #[tokio::test]
    async fn cancelled_waiter_does_not_release_a_running_blocking_owner() {
        let fixture = Fixture::new("cancel");
        let coordinator = fixture.coordinator();
        let lease = coordinator
            .acquire("demo".to_owned())
            .await
            .expect("acquires snapshot owner");
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let operation = tokio::spawn(async move {
            lease
                .call(move |_| {
                    started_tx.send(()).expect("signals blocking owner");
                    release_rx.recv().expect("releases blocking owner");
                    Ok(())
                })
                .await
        });
        started_rx.await.expect("blocking owner starts");
        operation.abort();
        let second = tokio::spawn({
            let coordinator = coordinator.clone();
            async move { coordinator.acquire("demo".to_owned()).await }
        });
        assert!(tokio::time::timeout(Duration::from_millis(100), async {
            while !second.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .is_err());
        release_tx.send(()).expect("releases blocking operation");
        tokio::time::timeout(Duration::from_secs(3), second)
            .await
            .expect("second owner proceeds after blocking work completes")
            .expect("second owner task joins")
            .expect("second owner acquires");
    }
}
