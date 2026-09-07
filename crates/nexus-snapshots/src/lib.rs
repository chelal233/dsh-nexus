//! Bounded, content-only snapshots for DeepSeek Harness profiles.
//!
//! The crate deliberately has no dependency on `nexus-core`: the Agent owns
//! transport DTO mapping and the outer lifecycle intent journal.  Snapshot
//! contents are restricted to [`FILE_POLICY`], and restore is exposed as an
//! explicit prepare/apply/materialize/commit transaction.

mod restore;
mod validation;

use std::{
    fmt, fs, io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

pub use restore::{
    RecoverDecision, RestoreOutcome, RestoreStatus, RestoreTicket, TransactionSummary,
};

pub const SNAPSHOT_SCHEMA_VERSION: u32 = 1;
pub const DEFAULT_HEALTHY_SLOTS: usize = 3;
pub const DEFAULT_MAX_MANUAL_SNAPSHOTS: usize = 64;
pub const MAX_TRANSACTION_RECORDS: usize = 64;
pub const MAX_MANIFEST_BYTES: u64 = 256 * 1024;
pub const MAX_TOTAL_SNAPSHOT_BYTES: u64 = 41 * 1024 * 1024;
const MAX_METADATA_TEXT_BYTES: usize = 128;

static UNIQUE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
pub enum SnapshotError {
    Io { context: String, source: io::Error },
    InvalidProfileName(String),
    InvalidIdentifier(String),
    InvalidPath(String),
    UnsafePath(String),
    Oversized { path: String, size: u64, limit: u64 },
    Capacity(String),
    InvalidManifest(String),
    Integrity(String),
    InvalidState(String),
    ConcurrentModification(String),
    StructuredData { path: String, message: String },
    InjectedFailure(String),
}

impl SnapshotError {
    pub(crate) fn io(context: impl Into<String>, source: io::Error) -> Self {
        Self::Io {
            context: context.into(),
            source,
        }
    }
}

impl fmt::Display for SnapshotError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { context, source } => write!(formatter, "{context}: {source}"),
            Self::InvalidProfileName(name) => write!(formatter, "invalid profile name: {name}"),
            Self::InvalidIdentifier(id) => write!(formatter, "invalid snapshot identifier: {id}"),
            Self::InvalidPath(path) => write!(formatter, "snapshot path is not allowed: {path}"),
            Self::UnsafePath(path) => write!(formatter, "unsafe filesystem path: {path}"),
            Self::Oversized { path, size, limit } => {
                write!(formatter, "{path} is {size} bytes; limit is {limit} bytes")
            }
            Self::Capacity(message) => formatter.write_str(message),
            Self::InvalidManifest(message) => {
                write!(formatter, "invalid snapshot manifest: {message}")
            }
            Self::Integrity(message) => write!(formatter, "snapshot integrity failure: {message}"),
            Self::InvalidState(message) => write!(formatter, "invalid restore state: {message}"),
            Self::ConcurrentModification(message) => {
                write!(formatter, "restore target changed concurrently: {message}")
            }
            Self::StructuredData { path, message } => {
                write!(
                    formatter,
                    "cannot safely process structured file {path}: {message}"
                )
            }
            Self::InjectedFailure(point) => {
                write!(formatter, "injected restore failure at {point}")
            }
        }
    }
}

impl std::error::Error for SnapshotError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

pub type Result<T> = std::result::Result<T, SnapshotError>;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FileScope {
    Profile,
    Home,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StructuredFormat {
    Json,
    Yaml,
}

#[derive(Debug, Clone, Copy)]
pub struct SnapshotFilePolicy {
    pub manifest_path: &'static str,
    pub scope: FileScope,
    pub relative_path: &'static str,
    pub max_bytes: u64,
    pub format: StructuredFormat,
}

pub const FILE_POLICY: [SnapshotFilePolicy; 7] = [
    SnapshotFilePolicy {
        manifest_path: "profile/package.json",
        scope: FileScope::Profile,
        relative_path: "package.json",
        max_bytes: 1024 * 1024,
        format: StructuredFormat::Json,
    },
    SnapshotFilePolicy {
        manifest_path: "profile/pnpm-lock.yaml",
        scope: FileScope::Profile,
        relative_path: "pnpm-lock.yaml",
        max_bytes: 32 * 1024 * 1024,
        format: StructuredFormat::Yaml,
    },
    SnapshotFilePolicy {
        manifest_path: "profile/pnpm-workspace.yaml",
        scope: FileScope::Profile,
        relative_path: "pnpm-workspace.yaml",
        max_bytes: 1024 * 1024,
        format: StructuredFormat::Yaml,
    },
    SnapshotFilePolicy {
        manifest_path: "profile/cordis.patch.yml",
        scope: FileScope::Profile,
        relative_path: "cordis.patch.yml",
        max_bytes: 1024 * 1024,
        format: StructuredFormat::Yaml,
    },
    SnapshotFilePolicy {
        manifest_path: "profile/.dsh-market/state.json",
        scope: FileScope::Profile,
        relative_path: ".dsh-market/state.json",
        max_bytes: 1024 * 1024,
        format: StructuredFormat::Json,
    },
    SnapshotFilePolicy {
        manifest_path: "home/settings.yaml",
        scope: FileScope::Home,
        relative_path: "settings.yaml",
        max_bytes: 4 * 1024 * 1024,
        format: StructuredFormat::Yaml,
    },
    SnapshotFilePolicy {
        manifest_path: "home/cordis.patch.yml",
        scope: FileScope::Home,
        relative_path: "cordis.patch.yml",
        max_bytes: 1024 * 1024,
        format: StructuredFormat::Yaml,
    },
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotKind {
    Healthy,
    Manual { label: Option<String> },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotFileState {
    Present,
    Missing,
    Omitted,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SnapshotFileRecord {
    pub path: String,
    pub scope: FileScope,
    pub state: SnapshotFileState,
    pub source_size: u64,
    pub stored_size: u64,
    pub sha256: Option<String>,
    pub mode: Option<u32>,
    pub redacted_paths: Vec<String>,
    pub omitted_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SnapshotManifest {
    pub schema: u32,
    pub snapshot_id: String,
    pub created_unix_ms: u64,
    pub profile_name: String,
    pub kind: SnapshotKind,
    pub dsh_version: String,
    pub plugin_count: u64,
    pub file_count: u64,
    pub total_bytes: u64,
    pub files: Vec<SnapshotFileRecord>,
}

/// A fully validated manifest and its fixed-policy stored blobs. Missing and
/// intentionally omitted records have no content. Callers cannot select paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotContent {
    pub manifest: SnapshotManifest,
    pub files: Vec<Option<Vec<u8>>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SnapshotSummary {
    pub snapshot_id: String,
    pub created_unix_ms: u64,
    pub profile_name: String,
    pub kind: SnapshotKind,
    pub dsh_version: String,
    pub plugin_count: u64,
    pub file_count: u64,
    pub total_bytes: u64,
}

impl From<&SnapshotManifest> for SnapshotSummary {
    fn from(manifest: &SnapshotManifest) -> Self {
        Self {
            snapshot_id: manifest.snapshot_id.clone(),
            created_unix_ms: manifest.created_unix_ms,
            profile_name: manifest.profile_name.clone(),
            kind: manifest.kind.clone(),
            dsh_version: manifest.dsh_version.clone(),
            plugin_count: manifest.plugin_count,
            file_count: manifest.file_count,
            total_bytes: manifest.total_bytes,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SnapshotInspection {
    pub snapshot_id: String,
    pub valid: bool,
    pub summary: Option<SnapshotSummary>,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CaptureRequest {
    pub dsh_version: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotStoreConfig {
    pub healthy_slots: usize,
    pub max_manual_snapshots: usize,
}

impl Default for SnapshotStoreConfig {
    fn default() -> Self {
        Self {
            healthy_slots: DEFAULT_HEALTHY_SLOTS,
            max_manual_snapshots: DEFAULT_MAX_MANUAL_SNAPSHOTS,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SnapshotStore {
    data_root: PathBuf,
    dsh_home: PathBuf,
    profile_name: String,
    profile_dir: PathBuf,
    snapshot_root: PathBuf,
    transaction_root: PathBuf,
    backup_root: PathBuf,
    config: SnapshotStoreConfig,
}

impl SnapshotStore {
    pub fn new(
        data_root: impl AsRef<Path>,
        dsh_home: impl AsRef<Path>,
        profile_name: impl Into<String>,
    ) -> Result<Self> {
        Self::with_config(
            data_root,
            dsh_home,
            profile_name,
            SnapshotStoreConfig::default(),
        )
    }

    pub fn with_config(
        data_root: impl AsRef<Path>,
        dsh_home: impl AsRef<Path>,
        profile_name: impl Into<String>,
        config: SnapshotStoreConfig,
    ) -> Result<Self> {
        if config.healthy_slots == 0 || config.healthy_slots > 32 {
            return Err(SnapshotError::Capacity(
                "healthy slot count must be between 1 and 32".to_owned(),
            ));
        }
        if config.max_manual_snapshots == 0 || config.max_manual_snapshots > 1024 {
            return Err(SnapshotError::Capacity(
                "manual snapshot limit must be between 1 and 1024".to_owned(),
            ));
        }
        let profile_name = profile_name.into();
        validation::validate_profile_name(&profile_name)?;
        let data_root = validation::canonical_secure_root(data_root.as_ref())?;
        let dsh_home = validation::canonical_secure_root(dsh_home.as_ref())?;
        if data_root.starts_with(&dsh_home) || dsh_home.starts_with(&data_root) {
            return Err(SnapshotError::UnsafePath(
                "Nexus data root and DSH home must be disjoint so transaction backups never enter the snapshot store"
                    .to_owned(),
            ));
        }
        let profile_dir = dsh_home.join("profiles").join(&profile_name);
        let snapshot_root = data_root.join("snapshots").join(&profile_name);
        let transaction_root = data_root.join("snapshot-transactions").join(&profile_name);
        let backup_root = dsh_home.join(".nexus-restore").join(&profile_name);
        Ok(Self {
            data_root,
            dsh_home,
            profile_name,
            profile_dir,
            snapshot_root,
            transaction_root,
            backup_root,
            config,
        })
    }

    pub fn data_root(&self) -> &Path {
        &self.data_root
    }

    pub fn dsh_home(&self) -> &Path {
        &self.dsh_home
    }

    pub fn profile_name(&self) -> &str {
        &self.profile_name
    }

    pub fn capture_healthy(&self, request: CaptureRequest) -> Result<SnapshotManifest> {
        self.capture(SnapshotKind::Healthy, request, None)
    }

    pub fn capture_manual(
        &self,
        request: CaptureRequest,
        label: Option<String>,
    ) -> Result<SnapshotManifest> {
        if let Some(label) = &label {
            validation::validate_metadata_text("manual label", label, MAX_METADATA_TEXT_BYTES)?;
        }
        self.capture(SnapshotKind::Manual { label }, request, None)
    }

    fn capture(
        &self,
        kind: SnapshotKind,
        request: CaptureRequest,
        publication_fault_after: Option<usize>,
    ) -> Result<SnapshotManifest> {
        validation::validate_metadata_text(
            "DSH version",
            &request.dsh_version,
            MAX_METADATA_TEXT_BYTES,
        )?;
        self.ensure_store_directories()?;
        let snapshot_id = new_identifier("snapshot");
        let created_unix_ms = unix_millis()?;
        let destination = match &kind {
            SnapshotKind::Healthy => {
                self.recover_orphaned_healthy_slots()?;
                self.next_healthy_slot()?
            }
            SnapshotKind::Manual { .. } => {
                let manual_root = self.snapshot_root.join("manual");
                validation::ensure_directory_tree(&self.data_root, &manual_root)?;
                let count = validation::bounded_subdirectories(
                    &manual_root,
                    self.config.max_manual_snapshots + 1,
                )?
                .len();
                if count >= self.config.max_manual_snapshots {
                    return Err(SnapshotError::Capacity(format!(
                        "manual snapshot limit {} reached",
                        self.config.max_manual_snapshots
                    )));
                }
                manual_root.join(&snapshot_id)
            }
        };
        let staging = match &kind {
            SnapshotKind::Healthy => {
                let index = self.healthy_slot_index(&destination)?;
                self.healthy_orphan_path(index, "next")
            }
            SnapshotKind::Manual { .. } => {
                self.snapshot_root.join(format!(".staging-{snapshot_id}"))
            }
        };
        validation::ensure_new_directory(&self.data_root, &staging)?;
        let mut publication_started = false;
        let result = (|| {
            let files_dir = staging.join("files");
            validation::ensure_new_directory(&self.data_root, &files_dir)?;
            let mut files = Vec::with_capacity(FILE_POLICY.len());
            let mut total_bytes = 0_u64;
            let mut plugin_count = 0_u64;
            for (index, policy) in FILE_POLICY.iter().enumerate() {
                let source = self.source_path(policy)?;
                let captured = validation::capture_allowed_file(&source, policy)?;
                if index == 0 {
                    if captured.record.state != SnapshotFileState::Present {
                        return Err(SnapshotError::InvalidManifest(
                            "profile/package.json must be present and parseable before capture"
                                .to_owned(),
                        ));
                    }
                    validation::validate_profile_package(captured.bytes.as_deref().ok_or_else(
                        || {
                            SnapshotError::InvalidManifest(
                                "profile/package.json has no restorable content".to_owned(),
                            )
                        },
                    )?)?;
                    plugin_count = captured
                        .bytes
                        .as_deref()
                        .map(validation::plugin_count_from_package_bytes)
                        .unwrap_or(0);
                }
                total_bytes = total_bytes
                    .checked_add(captured.stored_size)
                    .ok_or_else(|| SnapshotError::Oversized {
                        path: "snapshot total".to_owned(),
                        size: u64::MAX,
                        limit: MAX_TOTAL_SNAPSHOT_BYTES,
                    })?;
                if total_bytes > MAX_TOTAL_SNAPSHOT_BYTES {
                    return Err(SnapshotError::Oversized {
                        path: "snapshot total".to_owned(),
                        size: total_bytes,
                        limit: MAX_TOTAL_SNAPSHOT_BYTES,
                    });
                }
                if let Some(bytes) = captured.bytes {
                    validation::write_durable(&files_dir.join(index.to_string()), &bytes)?;
                }
                files.push(captured.record);
            }
            let file_count = files
                .iter()
                .filter(|file| file.state == SnapshotFileState::Present)
                .count() as u64;
            let manifest = SnapshotManifest {
                schema: SNAPSHOT_SCHEMA_VERSION,
                snapshot_id: snapshot_id.clone(),
                created_unix_ms,
                profile_name: self.profile_name.clone(),
                kind: kind.clone(),
                dsh_version: request.dsh_version.clone(),
                plugin_count,
                file_count,
                total_bytes,
                files,
            };
            let encoded = serde_json::to_vec_pretty(&manifest).map_err(|error| {
                SnapshotError::InvalidManifest(format!("cannot encode new manifest: {error}"))
            })?;
            validation::write_durable(&staging.join("manifest.json"), &encoded)?;
            validation::sync_directory(&staging)?;
            match kind {
                SnapshotKind::Healthy => self.publish_healthy_snapshot(
                    &staging,
                    &destination,
                    &snapshot_id,
                    publication_fault_after,
                    &mut publication_started,
                )?,
                SnapshotKind::Manual { .. } => {
                    let candidate = validation::load_valid_snapshot(self, &staging)?;
                    if candidate.snapshot_id != snapshot_id
                        || !matches!(candidate.kind, SnapshotKind::Manual { .. })
                    {
                        return Err(SnapshotError::Integrity(
                            "manual publication candidate identity changed".to_owned(),
                        ));
                    }
                    validation::rename_durable(&staging, &destination)?;
                }
            }
            Ok(manifest)
        })();
        if result.is_err() && !publication_started && staging.exists() {
            let _ = fs::remove_dir_all(&staging);
        }
        result
    }

    #[cfg(test)]
    pub(crate) fn capture_healthy_with_publication_fault(
        &self,
        request: CaptureRequest,
        fail_after_namespace_mutations: usize,
    ) -> Result<SnapshotManifest> {
        self.capture(
            SnapshotKind::Healthy,
            request,
            Some(fail_after_namespace_mutations),
        )
    }

    pub fn list(&self) -> Result<Vec<SnapshotSummary>> {
        let mut manifests = Vec::new();
        for directory in self.snapshot_directories()? {
            manifests.push(validation::load_valid_snapshot(self, &directory)?);
        }
        manifests.sort_by_key(|manifest| std::cmp::Reverse(manifest.created_unix_ms));
        Ok(manifests.iter().map(SnapshotSummary::from).collect())
    }

    /// Return one bounded integrity result per retained slot. A corrupt slot
    /// therefore does not hide healthy recovery choices from the UI.
    pub fn list_inspections(&self) -> Result<Vec<SnapshotInspection>> {
        let mut inspections = Vec::new();
        for directory in self.snapshot_directories()? {
            let fallback_id = directory
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "unreadable-slot".to_owned());
            let candidate_id = validation::read_manifest_bounded(&directory)
                .map(|manifest| manifest.snapshot_id)
                .unwrap_or(fallback_id);
            match validation::load_valid_snapshot(self, &directory) {
                Ok(manifest) => inspections.push(SnapshotInspection {
                    snapshot_id: manifest.snapshot_id.clone(),
                    valid: true,
                    summary: Some(SnapshotSummary::from(&manifest)),
                    errors: Vec::new(),
                }),
                Err(error) => inspections.push(SnapshotInspection {
                    snapshot_id: candidate_id,
                    valid: false,
                    summary: None,
                    errors: vec![error.to_string()],
                }),
            }
        }
        inspections.sort_by_key(|inspection| {
            std::cmp::Reverse(
                inspection
                    .summary
                    .as_ref()
                    .map_or(0, |summary| summary.created_unix_ms),
            )
        });
        Ok(inspections)
    }

    pub fn detail(&self, snapshot_id: &str) -> Result<SnapshotManifest> {
        let directory = self.find_snapshot_directory(snapshot_id)?;
        validation::load_valid_snapshot(self, &directory)
    }

    pub fn content(&self, snapshot_id: &str) -> Result<SnapshotContent> {
        let manifest = self.detail(snapshot_id)?;
        let mut files = Vec::with_capacity(manifest.files.len());
        for (index, record) in manifest.files.iter().enumerate() {
            files.push(match record.state {
                SnapshotFileState::Present => {
                    let bytes = validation::read_snapshot_blob(self, snapshot_id, index)?;
                    let digest = validation::sha256_hex(&bytes);
                    if bytes.len() as u64 != record.stored_size
                        || record.sha256.as_deref() != Some(digest.as_str())
                    {
                        return Err(SnapshotError::Integrity(format!(
                            "stored content changed while reading {}",
                            record.path
                        )));
                    }
                    Some(bytes)
                }
                SnapshotFileState::Missing | SnapshotFileState::Omitted => None,
            });
        }
        Ok(SnapshotContent { manifest, files })
    }

    pub fn inspect(&self, snapshot_id: &str) -> Result<SnapshotInspection> {
        let directory = self.find_snapshot_directory(snapshot_id)?;
        match validation::load_valid_snapshot(self, &directory) {
            Ok(manifest) => Ok(SnapshotInspection {
                snapshot_id: snapshot_id.to_owned(),
                valid: true,
                summary: Some(SnapshotSummary::from(&manifest)),
                errors: Vec::new(),
            }),
            Err(error) => Ok(SnapshotInspection {
                snapshot_id: snapshot_id.to_owned(),
                valid: false,
                summary: None,
                errors: vec![error.to_string()],
            }),
        }
    }

    fn ensure_store_directories(&self) -> Result<()> {
        validation::ensure_directory_tree(&self.data_root, &self.snapshot_root)?;
        validation::ensure_directory_tree(&self.data_root, &self.transaction_root)
    }

    fn source_path(&self, policy: &SnapshotFilePolicy) -> Result<PathBuf> {
        match policy.scope {
            FileScope::Profile => validation::resolve_under(
                &self.dsh_home,
                &self.profile_dir,
                Path::new(policy.relative_path),
            ),
            FileScope::Home => validation::resolve_under(
                &self.dsh_home,
                &self.dsh_home,
                Path::new(policy.relative_path),
            ),
        }
    }

    fn next_healthy_slot(&self) -> Result<PathBuf> {
        let healthy = self.snapshot_root.join("healthy");
        validation::ensure_directory_tree(&self.data_root, &healthy)?;
        let mut oldest: Option<(u64, PathBuf)> = None;
        for index in 1..=self.config.healthy_slots {
            let slot = healthy.join(format!("slot-{index}"));
            if !slot.exists() {
                return Ok(slot);
            }
            let created = validation::read_manifest_bounded(&slot)
                .map(|manifest| manifest.created_unix_ms)
                .unwrap_or(0);
            if oldest
                .as_ref()
                .is_none_or(|(current, _)| created < *current)
            {
                oldest = Some((created, slot));
            }
        }
        oldest
            .map(|(_, slot)| slot)
            .ok_or_else(|| SnapshotError::Capacity("no healthy snapshot slot available".to_owned()))
    }

    fn publish_healthy_snapshot(
        &self,
        staging: &Path,
        destination: &Path,
        snapshot_id: &str,
        publication_fault_after: Option<usize>,
        publication_started: &mut bool,
    ) -> Result<()> {
        let index = self.healthy_slot_index(destination)?;
        let expected_staging = self.healthy_orphan_path(index, "next");
        if staging != expected_staging {
            return Err(SnapshotError::InvalidState(
                "healthy publication staging is not bound to its slot".to_owned(),
            ));
        }
        let candidate = validation::load_valid_snapshot(self, staging)?;
        if candidate.snapshot_id != snapshot_id || candidate.kind != SnapshotKind::Healthy {
            return Err(SnapshotError::Integrity(
                "healthy publication candidate identity changed".to_owned(),
            ));
        }
        let old = self.healthy_orphan_path(index, "old");
        if validation::validate_optional_directory_tree(&self.data_root, &old)? {
            return Err(SnapshotError::InvalidState(format!(
                "unrecovered healthy-slot old directory: {}",
                old.display()
            )));
        }
        let mut completed = 0;
        if validation::validate_optional_directory_tree(&self.data_root, destination)? {
            let previous = validation::load_valid_snapshot(self, destination)?;
            if previous.kind != SnapshotKind::Healthy {
                return Err(SnapshotError::Integrity(
                    "healthy slot contains a non-healthy snapshot".to_owned(),
                ));
            }
            *publication_started = true;
            validation::rename_durable(destination, &old)?;
            completed += 1;
            maybe_inject_publication_failure(publication_fault_after, completed)?;
        }
        *publication_started = true;
        if let Err(error) = validation::rename_durable(staging, destination) {
            if validation::validate_optional_directory_tree(&self.data_root, &old).unwrap_or(false)
            {
                let _ = validation::rename_durable(&old, destination);
            }
            return Err(error);
        }
        completed += 1;
        maybe_inject_publication_failure(publication_fault_after, completed)?;
        if validation::validate_optional_directory_tree(&self.data_root, &old)? {
            validation::remove_directory_all_durable(&old)?;
        }
        Ok(())
    }

    fn recover_orphaned_healthy_slots(&self) -> Result<()> {
        for entry in fs::read_dir(&self.snapshot_root).map_err(|error| {
            SnapshotError::io(
                format!("read snapshot root {}", self.snapshot_root.display()),
                error,
            )
        })? {
            let entry = entry.map_err(|error| {
                SnapshotError::io(
                    format!("read entry in {}", self.snapshot_root.display()),
                    error,
                )
            })?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let staging = validation::is_generated_name(&name, ".staging-snapshot-");
            if !matches!(name.as_str(), "healthy" | "manual") && !staging {
                return Err(SnapshotError::UnsafePath(format!(
                    "unidentified snapshot publication orphan: {}",
                    entry.path().display()
                )));
            }
            validation::validate_directory_tree(&self.data_root, &entry.path())?;
        }
        let healthy = self.snapshot_root.join("healthy");
        if !validation::validate_optional_directory_tree(&self.data_root, &healthy)? {
            return Ok(());
        }
        self.validate_healthy_inventory(&healthy)?;
        for index in 1..=self.config.healthy_slots {
            let destination = healthy.join(format!("slot-{index}"));
            let old = self.healthy_orphan_path(index, "old");
            let next = self.healthy_orphan_path(index, "next");
            let old_present = validation::validate_optional_directory_tree(&self.data_root, &old)?;
            let mut next_present =
                validation::validate_optional_directory_tree(&self.data_root, &next)?;
            let destination_present =
                validation::validate_optional_directory_tree(&self.data_root, &destination)?;
            if !old_present && !next_present {
                continue;
            }
            if destination_present {
                self.validate_healthy_candidate(&destination)?;
            }
            if old_present {
                self.validate_healthy_candidate(&old)?;
            }
            if next_present {
                if self.validate_healthy_candidate(&next).is_err() {
                    // Capture has not published a valid snapshot. Preserve the
                    // entire candidate, including unknown contents, outside the
                    // published inventory and free this slot for a future capture.
                    let quarantine = self.snapshot_root.join(format!(".staging-{}", new_identifier("snapshot")));
                    validation::rename_durable(&next, &quarantine)?;
                    next_present = false;
                }
            }
            if destination_present {
                if next_present {
                    validation::remove_directory_all_durable(&next)?;
                }
                if old_present {
                    validation::remove_directory_all_durable(&old)?;
                }
            } else if old_present {
                validation::rename_durable(&old, &destination)?;
                if next_present {
                    validation::remove_directory_all_durable(&next)?;
                }
            } else if next_present {
                validation::rename_durable(&next, &destination)?;
            }
        }
        Ok(())
    }

    fn validate_healthy_inventory(&self, healthy: &Path) -> Result<()> {
        let mut count = 0usize;
        for entry in fs::read_dir(healthy).map_err(|error| {
            SnapshotError::io(
                format!("read healthy directory {}", healthy.display()),
                error,
            )
        })? {
            let entry = entry.map_err(|error| {
                SnapshotError::io(format!("read entry in {}", healthy.display()), error)
            })?;
            let metadata = fs::symlink_metadata(entry.path()).map_err(|error| {
                SnapshotError::io(format!("inspect {}", entry.path().display()), error)
            })?;
            if !metadata.is_dir() {
                return Err(SnapshotError::UnsafePath(format!(
                    "unexpected healthy-slot entry: {}",
                    entry.path().display()
                )));
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let index = ["slot-", ".old-slot-", ".next-slot-"]
                .into_iter()
                .find_map(|prefix| name.strip_prefix(prefix))
                .and_then(|value| value.parse::<usize>().ok())
                .filter(|index| (1..=self.config.healthy_slots).contains(index));
            if index.is_none() {
                return Err(SnapshotError::UnsafePath(format!(
                    "unidentified healthy-slot orphan: {}",
                    entry.path().display()
                )));
            }
            count += 1;
            if count > self.config.healthy_slots.saturating_mul(3) {
                return Err(SnapshotError::Capacity(
                    "healthy-slot inventory exceeds recovery bound".to_owned(),
                ));
            }
        }
        Ok(())
    }

    fn validate_healthy_candidate(&self, directory: &Path) -> Result<()> {
        let manifest = validation::load_valid_snapshot(self, directory)?;
        if manifest.kind != SnapshotKind::Healthy {
            return Err(SnapshotError::Integrity(format!(
                "healthy-slot candidate is not healthy: {}",
                directory.display()
            )));
        }
        Ok(())
    }

    fn healthy_slot_index(&self, destination: &Path) -> Result<usize> {
        let healthy = self.snapshot_root.join("healthy");
        if destination.parent() != Some(healthy.as_path()) {
            return Err(SnapshotError::InvalidPath(
                destination.display().to_string(),
            ));
        }
        let index = destination
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_prefix("slot-"))
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|index| (1..=self.config.healthy_slots).contains(index))
            .ok_or_else(|| SnapshotError::InvalidPath(destination.display().to_string()))?;
        Ok(index)
    }

    fn healthy_orphan_path(&self, index: usize, kind: &str) -> PathBuf {
        self.snapshot_root
            .join("healthy")
            .join(format!(".{kind}-slot-{index}"))
    }

    fn snapshot_directories(&self) -> Result<Vec<PathBuf>> {
        if !self.snapshot_root.exists() {
            return Ok(Vec::new());
        }
        validation::validate_directory_tree(&self.data_root, &self.snapshot_root)?;
        self.recover_orphaned_healthy_slots()?;
        let mut directories = Vec::new();
        let healthy = self.snapshot_root.join("healthy");
        if healthy.exists() {
            validation::validate_directory_tree(&self.data_root, &healthy)?;
            for index in 1..=self.config.healthy_slots {
                let slot = healthy.join(format!("slot-{index}"));
                if slot.exists() {
                    directories.push(slot);
                }
            }
        }
        let manual = self.snapshot_root.join("manual");
        if manual.exists() {
            directories.extend(validation::bounded_subdirectories(
                &manual,
                self.config.max_manual_snapshots + 1,
            )?);
            if directories.len() > self.config.healthy_slots + self.config.max_manual_snapshots {
                return Err(SnapshotError::Capacity(
                    "snapshot inventory exceeds configured bounds".to_owned(),
                ));
            }
        }
        Ok(directories)
    }

    fn find_snapshot_directory(&self, snapshot_id: &str) -> Result<PathBuf> {
        validation::validate_identifier(snapshot_id)?;
        for directory in self.snapshot_directories()? {
            if let Ok(manifest) = validation::read_manifest_bounded(&directory) {
                if manifest.snapshot_id == snapshot_id {
                    return Ok(directory);
                }
            }
        }
        Err(SnapshotError::InvalidIdentifier(format!(
            "snapshot not found: {snapshot_id}"
        )))
    }

    pub(crate) fn snapshot_directory(&self, snapshot_id: &str) -> Result<PathBuf> {
        self.find_snapshot_directory(snapshot_id)
    }

    pub(crate) fn target_path(&self, policy: &SnapshotFilePolicy) -> Result<PathBuf> {
        self.source_path(policy)
    }

    pub(crate) fn transaction_root(&self) -> &Path {
        &self.transaction_root
    }

    pub(crate) fn backup_root(&self) -> &Path {
        &self.backup_root
    }
}

pub(crate) fn unix_millis() -> Result<u64> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| {
            SnapshotError::InvalidState(format!("system clock is before Unix epoch: {error}"))
        })?;
    u64::try_from(duration.as_millis())
        .map_err(|_| SnapshotError::InvalidState("system clock value does not fit u64".to_owned()))
}

pub(crate) fn new_identifier(prefix: &str) -> String {
    let millis = unix_millis().unwrap_or_default();
    let sequence = UNIQUE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}-{millis}-{}-{sequence}", std::process::id())
}

fn maybe_inject_publication_failure(fail_after: Option<usize>, completed: usize) -> Result<()> {
    if fail_after == Some(completed) {
        return Err(SnapshotError::InjectedFailure(format!(
            "after healthy publication namespace mutation {completed}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
