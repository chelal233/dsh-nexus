//! UI-independent configuration, path, and state primitives for Nexus.

use std::{
    env, fs, io,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use nexus_protocol::{
    decode_json, encode_json, AgentLifecycleState, AgentStatePayload, CheckpointManifest,
    DiagnosticsBundle, DiagnosticsFile, HarnessCheckpointState, HarnessConfigPayload,
    HarnessRuntimeInfo, HarnessState, ReleaseManifest, UpdateConfigPayload, UpdateRuntimeInfo,
    UpdateState,
};
use serde::{Deserialize, Serialize};

pub const DEFAULT_AGENT_PORT: u16 = 3090;
pub const DEFAULT_PROFILE: &str = "web";
pub const MAX_PROFILE_NAME_LEN: usize = 64;
pub const PROFILE_SCHEMA_VERSION: u32 = 1;
pub const CHECKPOINT_SCHEMA_VERSION: u32 = 1;
pub const CHECKPOINT_RESTORE_SCHEMA_VERSION: u32 = 1;
pub const RELEASE_SCHEMA_VERSION: u32 = 1;
pub const MAX_RELEASE_ID_LEN: usize = 128;
pub const MAX_RELEASE_VERSION_LEN: usize = 128;
pub const MAX_RELEASE_TEXT_LEN: usize = 4096;
pub const UPDATE_SCHEMA_VERSION: u32 = 1;
pub const MAX_UPDATE_SOURCE_LEN: usize = 2048;
pub const MAX_UPDATE_REF_LEN: usize = 256;
pub const MAX_UPDATE_TEXT_LEN: usize = 4096;
pub const DEFAULT_UPDATE_TIMEOUT_SECS: u64 = 900;
pub const DIAGNOSTICS_SCHEMA_VERSION: u32 = 1;
pub const MAX_DIAGNOSTICS_FILES: usize = 64;
pub const MAX_DIAGNOSTICS_BUNDLES: usize = 32;
pub const MAX_DIAGNOSTICS_FILE_BYTES: usize = 512 * 1024;
pub const MAX_DIAGNOSTICS_LOG_BYTES: usize = 256 * 1024;
pub const MAX_DIAGNOSTICS_NOTE_LEN: usize = 4096;
pub const DATA_DIR_ENV: &str = "NEXUS_DATA_DIR";
pub const PORT_ENV: &str = "NEXUS_AGENT_PORT";
pub const HARNESS_PROGRAM_ENV: &str = "NEXUS_HARNESS_PROGRAM";
pub const HARNESS_ARGS_ENV: &str = "NEXUS_HARNESS_ARGS";
pub const HARNESS_WORKING_DIR_ENV: &str = "NEXUS_HARNESS_WORKING_DIR";
pub const HARNESS_READINESS_URL_ENV: &str = "NEXUS_HARNESS_READINESS_URL";
pub const HARNESS_READINESS_TIMEOUT_ENV: &str = "NEXUS_HARNESS_READINESS_TIMEOUT_SECS";
pub const UPDATE_SOURCE_ENV: &str = "NEXUS_UPDATE_SOURCE";
pub const UPDATE_REF_ENV: &str = "NEXUS_UPDATE_REF";
pub const UPDATE_GIT_PROGRAM_ENV: &str = "NEXUS_UPDATE_GIT_PROGRAM";
pub const UPDATE_BUILD_PROGRAM_ENV: &str = "NEXUS_UPDATE_BUILD_PROGRAM";
pub const UPDATE_BUILD_ARGS_ENV: &str = "NEXUS_UPDATE_BUILD_ARGS";
pub const UPDATE_VERIFY_PROGRAM_ENV: &str = "NEXUS_UPDATE_VERIFY_PROGRAM";
pub const UPDATE_VERIFY_ARGS_ENV: &str = "NEXUS_UPDATE_VERIFY_ARGS";
pub const UPDATE_TIMEOUT_ENV: &str = "NEXUS_UPDATE_TIMEOUT_SECS";
pub const RUNTIME_SCHEMA_VERSION: u32 = 1;
pub const HARNESS_LOG_SESSION_SCHEMA_VERSION: u32 = 2;

/// Runtime configuration intentionally binds only to loopback.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NexusConfig {
    pub data_dir: Option<PathBuf>,
    pub port: u16,
}

impl Default for NexusConfig {
    fn default() -> Self {
        Self {
            data_dir: None,
            port: DEFAULT_AGENT_PORT,
        }
    }
}

impl NexusConfig {
    /// Read non-secret, optional overrides while retaining safe defaults for invalid values.
    pub fn from_env() -> Self {
        let mut config = Self::default();

        if let Some(data_dir) = env::var_os(DATA_DIR_ENV).filter(|value| !value.is_empty()) {
            config.data_dir = Some(PathBuf::from(data_dir));
        }

        if let Some(port) = env::var(PORT_ENV)
            .ok()
            .and_then(|value| value.parse::<u16>().ok())
            .filter(|port| *port != 0)
        {
            config.port = port;
        }

        config
    }

    pub fn paths(&self) -> NexusPaths {
        match &self.data_dir {
            Some(data_dir) => NexusPaths::from_root(data_dir.clone()),
            None => NexusPaths::from_root(default_data_root()),
        }
    }

    pub fn bind_addr(&self) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), self.port)
    }
}

/// All Nexus-owned state is kept outside the Harness data directory.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NexusPaths {
    pub root: PathBuf,
    pub config_file: PathBuf,
    pub state_file: PathBuf,
    pub profiles_file: PathBuf,
    pub release_pointers_file: PathBuf,
    pub update_state_file: PathBuf,
    pub logs_dir: PathBuf,
    pub checkpoints_dir: PathBuf,
    pub releases_dir: PathBuf,
    pub downloads_dir: PathBuf,
    pub diagnostics_dir: PathBuf,
    pub run_dir: PathBuf,
}

impl NexusPaths {
    pub fn from_root(root: PathBuf) -> Self {
        Self {
            config_file: root.join("config.json"),
            state_file: root.join("state.json"),
            profiles_file: root.join("profiles.json"),
            release_pointers_file: root.join("release-pointers.json"),
            update_state_file: root.join("update-state.json"),
            logs_dir: root.join("logs"),
            checkpoints_dir: root.join("checkpoints"),
            releases_dir: root.join("releases"),
            downloads_dir: root.join("downloads"),
            diagnostics_dir: root.join("diagnostics"),
            run_dir: root.join("run"),
            root,
        }
    }

    pub fn ensure_directories(&self) -> io::Result<()> {
        for directory in [
            &self.root,
            &self.logs_dir,
            &self.checkpoints_dir,
            &self.releases_dir,
            &self.downloads_dir,
            &self.diagnostics_dir,
            &self.run_dir,
        ] {
            fs::create_dir_all(directory)?;
        }

        Ok(())
    }
}

/// Nexus-owned profile catalog.  Profiles are names only in this phase; the
/// catalog deliberately does not contain Harness settings or user data.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProfileCatalog {
    #[serde(default = "default_profile_schema")]
    pub schema_version: u32,
    pub active_profile: String,
    #[serde(default = "default_profiles")]
    pub profiles: Vec<String>,
}

impl Default for ProfileCatalog {
    fn default() -> Self {
        Self {
            schema_version: PROFILE_SCHEMA_VERSION,
            active_profile: DEFAULT_PROFILE.to_owned(),
            profiles: vec![DEFAULT_PROFILE.to_owned()],
        }
    }
}

impl ProfileCatalog {
    pub fn new(active_profile: impl Into<String>, profiles: Vec<String>) -> io::Result<Self> {
        let mut catalog = Self {
            schema_version: PROFILE_SCHEMA_VERSION,
            active_profile: active_profile.into(),
            profiles,
        };
        catalog.normalize()?;
        Ok(catalog)
    }

    pub fn validate_name(name: &str) -> io::Result<()> {
        validate_profile_name(name)
    }

    fn normalize(&mut self) -> io::Result<()> {
        if self.schema_version == 0 {
            self.schema_version = PROFILE_SCHEMA_VERSION;
        }
        validate_profile_name(&self.active_profile)?;
        if self.profiles.is_empty() {
            self.profiles.push(DEFAULT_PROFILE.to_owned());
        }
        for name in &self.profiles {
            validate_profile_name(name)?;
        }
        if !self
            .profiles
            .iter()
            .any(|name| name == &self.active_profile)
        {
            self.profiles.push(self.active_profile.clone());
        }
        self.profiles.sort_unstable();
        self.profiles.dedup();
        Ok(())
    }
}

fn default_profile_schema() -> u32 {
    PROFILE_SCHEMA_VERSION
}

fn default_profiles() -> Vec<String> {
    vec![DEFAULT_PROFILE.to_owned()]
}

/// Return whether `name` is safe to use as a profile identifier and catalog
/// value.  It is intentionally stricter than a platform path parser.
pub fn is_valid_profile_name(name: &str) -> bool {
    validate_profile_name(name).is_ok()
}

pub fn validate_profile_name(name: &str) -> io::Result<()> {
    let valid_chars = name
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'));
    if name.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "profile name cannot be empty",
        ));
    }
    if name.len() > MAX_PROFILE_NAME_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("profile name exceeds {MAX_PROFILE_NAME_LEN} bytes"),
        ));
    }
    if name == "." || name == ".." || !valid_chars {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "profile name must contain only ASCII letters, digits, '.', '_', or '-'",
        ));
    }
    Ok(())
}

/// Durable store for the active profile and known profile names.
#[derive(Clone)]
pub struct ProfileStore {
    paths: NexusPaths,
    write_gate: Arc<Mutex<()>>,
}

impl ProfileStore {
    pub fn new(paths: NexusPaths) -> Self {
        Self {
            paths,
            write_gate: Arc::new(Mutex::new(())),
        }
    }

    pub fn paths(&self) -> &NexusPaths {
        &self.paths
    }

    /// Load the catalog, creating an atomic default `profiles.json` on first
    /// use.  No Harness directory is consulted.
    pub fn load(&self) -> io::Result<ProfileCatalog> {
        let _guard = self.lock_gate()?;
        self.load_unlocked()
    }

    pub fn read(&self) -> io::Result<Option<ProfileCatalog>> {
        let _guard = self.lock_gate()?;
        self.read_unlocked()
    }

    pub fn write(&self, catalog: &ProfileCatalog) -> io::Result<()> {
        let _guard = self.lock_gate()?;
        let mut catalog = catalog.clone();
        catalog.normalize()?;
        write_json_atomic(&self.paths.root, &self.paths.profiles_file, &catalog)
    }

    /// Select a valid profile and add it to the known catalog if necessary.
    /// This changes metadata only; it never starts or restarts Harness.
    pub fn select(&self, name: &str) -> io::Result<ProfileCatalog> {
        validate_profile_name(name)?;
        let _guard = self.lock_gate()?;
        let mut catalog = self.load_unlocked()?;
        catalog.active_profile = name.to_owned();
        catalog.normalize()?;
        write_json_atomic(&self.paths.root, &self.paths.profiles_file, &catalog)?;
        Ok(catalog)
    }

    fn load_unlocked(&self) -> io::Result<ProfileCatalog> {
        let Some(mut catalog) = self.read_unlocked()? else {
            let catalog = ProfileCatalog::default();
            write_json_atomic(&self.paths.root, &self.paths.profiles_file, &catalog)?;
            return Ok(catalog);
        };
        let before = catalog.clone();
        catalog.normalize()?;
        if catalog != before {
            write_json_atomic(&self.paths.root, &self.paths.profiles_file, &catalog)?;
        }
        Ok(catalog)
    }

    fn read_unlocked(&self) -> io::Result<Option<ProfileCatalog>> {
        if !self.paths.profiles_file.exists() {
            return Ok(None);
        }
        let bytes = fs::read(&self.paths.profiles_file)?;
        decode_json(&bytes).map(Some).map_err(invalid_data)
    }

    fn lock_gate(&self) -> io::Result<std::sync::MutexGuard<'_, ()>> {
        self.write_gate
            .lock()
            .map_err(|_| io::Error::other("profile catalog lock is poisoned"))
    }
}

/// Alias used by integrations that call the on-disk object a profile catalog.
pub type ProfileCatalogStore = ProfileStore;

/// Harness selection captured in a checkpoint manifest. The alias is retained
/// for source compatibility, but it no longer contains Agent or runtime state.
pub type NexusStateSnapshot = HarnessCheckpointState;

/// Durable manifests stored below the Nexus-owned `checkpoints/` directory.
#[derive(Clone)]
pub struct CheckpointStore {
    paths: NexusPaths,
    write_gate: Arc<Mutex<()>>,
}

impl CheckpointStore {
    pub fn new(paths: NexusPaths) -> Self {
        Self {
            paths,
            write_gate: Arc::new(Mutex::new(())),
        }
    }

    pub fn paths(&self) -> &NexusPaths {
        &self.paths
    }

    pub fn create(
        &self,
        profile: &str,
        release: Option<String>,
        note: Option<String>,
        state: NexusStateSnapshot,
    ) -> io::Result<CheckpointManifest> {
        validate_profile_name(profile)?;
        if let Some(release) = release.as_deref() {
            validate_release_id(release)?;
        }
        if state.profile != profile {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "checkpoint state profile does not match manifest profile",
            ));
        }
        if state.release != release {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "checkpoint state release does not match manifest release",
            ));
        }
        if let Some(note) = note.as_ref() {
            if note.len() > 4096 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "checkpoint note exceeds 4096 bytes",
                ));
            }
        }
        let _guard = self.lock_gate()?;
        self.paths.ensure_directories()?;
        let id = self.next_id()?;
        let manifest = CheckpointManifest {
            id: id.clone(),
            created_at_unix: unix_time_seconds(),
            profile: profile.to_owned(),
            release,
            note,
            state,
        };
        let path = self.path_for_id(&id)?;
        write_json_atomic(&self.paths.checkpoints_dir, &path, &manifest)?;
        Ok(manifest)
    }

    pub fn list(&self) -> io::Result<Vec<CheckpointManifest>> {
        let _guard = self.lock_gate()?;
        if !self.paths.checkpoints_dir.exists() {
            return Ok(Vec::new());
        }
        let mut manifests = Vec::new();
        for entry in fs::read_dir(&self.paths.checkpoints_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
                continue;
            }
            let bytes = fs::read(&path)?;
            let manifest: CheckpointManifest = decode_json(&bytes).map_err(invalid_data)?;
            validate_checkpoint_manifest(&manifest)?;
            if path.file_stem().and_then(|stem| stem.to_str()) != Some(&manifest.id) {
                return Err(invalid_data(
                    "checkpoint filename does not match manifest id",
                ));
            }
            manifests.push(manifest);
        }
        manifests.sort_by(|left, right| {
            right
                .created_at_unix
                .cmp(&left.created_at_unix)
                .then_with(|| right.id.cmp(&left.id))
        });
        Ok(manifests)
    }

    pub fn read(&self, id: &str) -> io::Result<Option<CheckpointManifest>> {
        validate_checkpoint_id(id)?;
        let _guard = self.lock_gate()?;
        let path = self.path_for_id(id)?;
        if !path.exists() {
            return Ok(None);
        }
        let bytes = fs::read(path)?;
        let manifest: CheckpointManifest = decode_json(&bytes).map_err(invalid_data)?;
        validate_checkpoint_manifest(&manifest)?;
        Ok(Some(manifest))
    }

    pub fn get(&self, id: &str) -> io::Result<CheckpointManifest> {
        self.read(id)?.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("checkpoint {id} was not found"),
            )
        })
    }

    /// Restore is metadata-only in this phase.  It reads and validates the
    /// manifest; the Agent decides when to apply its profile/release fields.
    pub fn restore(&self, id: &str) -> io::Result<CheckpointManifest> {
        self.get(id)
    }

    fn next_id(&self) -> io::Result<String> {
        let timestamp = unix_time_nanos();
        let base = format!("cp-{timestamp}");
        let mut candidate = base.clone();
        let mut suffix = 0_u32;
        while self
            .paths
            .checkpoints_dir
            .join(format!("{candidate}.json"))
            .exists()
        {
            suffix = suffix.saturating_add(1);
            candidate = format!("{base}-{suffix}");
        }
        validate_checkpoint_id(&candidate)?;
        Ok(candidate)
    }

    fn path_for_id(&self, id: &str) -> io::Result<PathBuf> {
        validate_checkpoint_id(id)?;
        Ok(self.paths.checkpoints_dir.join(format!("{id}.json")))
    }

    fn lock_gate(&self) -> io::Result<std::sync::MutexGuard<'_, ()>> {
        self.write_gate
            .lock()
            .map_err(|_| io::Error::other("checkpoint store lock is poisoned"))
    }
}

pub const MAX_CHECKPOINT_ID_LEN: usize = 64;

pub fn is_valid_checkpoint_id(id: &str) -> bool {
    validate_checkpoint_id(id).is_ok()
}

pub fn validate_checkpoint_id(id: &str) -> io::Result<()> {
    let valid_chars = id
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'));
    if id.is_empty() || id.len() > MAX_CHECKPOINT_ID_LEN || id == "." || id == ".." || !valid_chars
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "checkpoint id must use a safe ASCII identifier",
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointRestorePhase {
    Prepared,
    Committed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CheckpointRestoreIntent {
    pub checkpoint_id: String,
    pub previous_profiles: ProfileCatalog,
    pub previous_current_release: Option<String>,
    pub previous_last_known_good: Option<String>,
    pub target_profiles: ProfileCatalog,
    pub target_current_release: Option<String>,
    pub target_last_known_good: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CheckpointRestoreJournal {
    pub schema_version: u32,
    pub phase: CheckpointRestorePhase,
    pub intent: CheckpointRestoreIntent,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct CheckpointRestoreJournalDocument {
    #[serde(default = "default_checkpoint_restore_schema")]
    schema_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pending: Option<CheckpointRestoreJournal>,
}

fn default_checkpoint_restore_schema() -> u32 {
    CHECKPOINT_RESTORE_SCHEMA_VERSION
}

#[derive(Clone)]
pub struct CheckpointRestoreJournalStore {
    paths: NexusPaths,
    write_gate: Arc<Mutex<()>>,
}

impl CheckpointRestoreJournalStore {
    pub fn new(paths: NexusPaths) -> Self {
        Self {
            paths,
            write_gate: Arc::new(Mutex::new(())),
        }
    }

    pub fn load(&self) -> io::Result<Option<CheckpointRestoreJournal>> {
        let _guard = self.lock_gate()?;
        self.load_unlocked()
    }

    pub fn begin(&self, intent: CheckpointRestoreIntent) -> io::Result<()> {
        validate_checkpoint_restore_intent(&intent)?;
        let _guard = self.lock_gate()?;
        if self.load_unlocked()?.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "a checkpoint restore transaction is already pending",
            ));
        }
        self.write_unlocked(Some(CheckpointRestoreJournal {
            schema_version: CHECKPOINT_RESTORE_SCHEMA_VERSION,
            phase: CheckpointRestorePhase::Prepared,
            intent,
        }))
    }

    pub fn mark_committed(&self, intent: &CheckpointRestoreIntent) -> io::Result<()> {
        let _guard = self.lock_gate()?;
        let Some(mut journal) = self.load_unlocked()? else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "checkpoint restore journal is missing",
            ));
        };
        if journal.phase != CheckpointRestorePhase::Prepared || journal.intent != *intent {
            return Err(invalid_data(
                "checkpoint restore journal changed before commit",
            ));
        }
        journal.phase = CheckpointRestorePhase::Committed;
        self.write_unlocked(Some(journal))
    }

    pub fn clear(
        &self,
        phase: CheckpointRestorePhase,
        intent: &CheckpointRestoreIntent,
    ) -> io::Result<()> {
        let _guard = self.lock_gate()?;
        let Some(journal) = self.load_unlocked()? else {
            return Ok(());
        };
        if journal.phase != phase || journal.intent != *intent {
            return Err(invalid_data(
                "checkpoint restore journal changed before clear",
            ));
        }
        self.write_unlocked(None)
    }

    fn load_unlocked(&self) -> io::Result<Option<CheckpointRestoreJournal>> {
        let path = self.paths.run_dir.join("checkpoint-restore.json");
        if !path.exists() {
            return Ok(None);
        }
        let bytes = fs::read(path)?;
        let document: CheckpointRestoreJournalDocument =
            decode_json(&bytes).map_err(invalid_data)?;
        if document.schema_version != CHECKPOINT_RESTORE_SCHEMA_VERSION {
            return Err(invalid_data(
                "unsupported checkpoint restore journal schema",
            ));
        }
        if let Some(journal) = document.pending.as_ref() {
            if journal.schema_version != CHECKPOINT_RESTORE_SCHEMA_VERSION {
                return Err(invalid_data("unsupported checkpoint restore entry schema"));
            }
            validate_checkpoint_restore_intent(&journal.intent)?;
        }
        Ok(document.pending)
    }

    fn write_unlocked(&self, pending: Option<CheckpointRestoreJournal>) -> io::Result<()> {
        self.paths.ensure_directories()?;
        let document = CheckpointRestoreJournalDocument {
            schema_version: CHECKPOINT_RESTORE_SCHEMA_VERSION,
            pending,
        };
        write_json_atomic(
            &self.paths.run_dir,
            &self.paths.run_dir.join("checkpoint-restore.json"),
            &document,
        )
    }

    fn lock_gate(&self) -> io::Result<std::sync::MutexGuard<'_, ()>> {
        self.write_gate
            .lock()
            .map_err(|_| io::Error::other("checkpoint restore journal lock is poisoned"))
    }
}

fn validate_checkpoint_restore_intent(intent: &CheckpointRestoreIntent) -> io::Result<()> {
    validate_checkpoint_id(&intent.checkpoint_id)?;
    let mut previous_profiles = intent.previous_profiles.clone();
    previous_profiles.normalize()?;
    if previous_profiles != intent.previous_profiles {
        return Err(invalid_data(
            "previous checkpoint profile catalog is not normalized",
        ));
    }
    let mut target_profiles = intent.target_profiles.clone();
    target_profiles.normalize()?;
    if target_profiles != intent.target_profiles {
        return Err(invalid_data(
            "target checkpoint profile catalog is not normalized",
        ));
    }
    for id in [
        intent.previous_current_release.as_deref(),
        intent.previous_last_known_good.as_deref(),
        intent.target_current_release.as_deref(),
        intent.target_last_known_good.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        validate_release_id(id)?;
    }
    if intent.previous_current_release.is_some()
        && intent.previous_current_release == intent.previous_last_known_good
    {
        return Err(invalid_data(
            "previous checkpoint release pointers are equal",
        ));
    }
    if intent.target_current_release.is_some()
        && intent.target_current_release == intent.target_last_known_good
    {
        return Err(invalid_data("target checkpoint release pointers are equal"));
    }
    Ok(())
}

fn validate_checkpoint_manifest(manifest: &CheckpointManifest) -> io::Result<()> {
    validate_checkpoint_id(&manifest.id)?;
    validate_profile_name(&manifest.profile)?;
    if let Some(release) = manifest.release.as_deref() {
        validate_release_id(release)?;
    }
    if manifest.state.profile != manifest.profile {
        return Err(invalid_data(
            "checkpoint state profile does not match profile",
        ));
    }
    if manifest.state.release != manifest.release {
        return Err(invalid_data(
            "checkpoint state release does not match release",
        ));
    }
    Ok(())
}

/// Nexus-owned release catalog. Release manifests are immutable after
/// registration; only the current/LKG pointer document changes on promotion
/// and rollback.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReleaseCatalog {
    #[serde(default = "default_release_schema")]
    pub schema_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_release: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_known_good: Option<String>,
    #[serde(default)]
    pub releases: Vec<ReleaseManifest>,
}

impl Default for ReleaseCatalog {
    fn default() -> Self {
        Self {
            schema_version: RELEASE_SCHEMA_VERSION,
            current_release: None,
            last_known_good: None,
            releases: Vec::new(),
        }
    }
}

impl ReleaseCatalog {
    pub fn find(&self, id: &str) -> Option<&ReleaseManifest> {
        self.releases.iter().find(|release| release.id == id)
    }

    fn normalize(&mut self) -> io::Result<()> {
        if self.schema_version == 0 {
            self.schema_version = RELEASE_SCHEMA_VERSION;
        }
        let mut ids = std::collections::HashSet::new();
        for release in &self.releases {
            validate_release_manifest(release)?;
            if !ids.insert(release.id.as_str()) {
                return Err(invalid_data("release catalog contains duplicate ids"));
            }
        }
        for pointer in [&self.current_release, &self.last_known_good] {
            if let Some(id) = pointer {
                validate_release_id(id)?;
                if !ids.contains(id.as_str()) {
                    return Err(invalid_data(format!(
                        "release pointer references unknown id: {id}"
                    )));
                }
            }
        }
        if self.current_release.is_some() && self.current_release == self.last_known_good {
            return Err(invalid_data(
                "current and last-known-good release must differ",
            ));
        }
        self.releases.sort_by(|left, right| {
            right
                .installed_at_unix
                .cmp(&left.installed_at_unix)
                .then_with(|| right.id.cmp(&left.id))
        });
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct ReleasePointerDocument {
    #[serde(default = "default_release_schema")]
    schema_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    current_release: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_known_good: Option<String>,
}

/// Durable store for immutable release manifests and atomic current/LKG
/// pointers. It never downloads, builds, or edits Harness source.
#[derive(Clone)]
pub struct ReleaseStore {
    paths: NexusPaths,
    write_gate: Arc<Mutex<()>>,
}

impl ReleaseStore {
    pub fn new(paths: NexusPaths) -> Self {
        Self {
            paths,
            write_gate: Arc::new(Mutex::new(())),
        }
    }

    pub fn paths(&self) -> &NexusPaths {
        &self.paths
    }

    /// Resolve a registered release to its canonical immutable slot directory.
    /// The manifest must be present in the catalog before a caller can use the
    /// path for process launch, which prevents pointers to partial candidates.
    pub fn release_root(&self, id: &str) -> io::Result<PathBuf> {
        validate_release_id(id)?;
        let _guard = self.lock_gate()?;
        let catalog = self.load_unlocked()?;
        self.release_root_unlocked(id, &catalog)
    }

    fn release_root_unlocked(&self, id: &str, catalog: &ReleaseCatalog) -> io::Result<PathBuf> {
        if catalog.find(id).is_none() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("release {id} was not found"),
            ));
        }
        let releases_root = fs::canonicalize(&self.paths.releases_dir)?;
        let slot = fs::canonicalize(self.slot_dir(id)?)?;
        if slot == releases_root || !is_within(&releases_root, &slot) || !slot.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "registered release resolves outside the Nexus release root",
            ));
        }
        Ok(slot)
    }

    pub fn load(&self) -> io::Result<ReleaseCatalog> {
        let _guard = self.lock_gate()?;
        self.load_unlocked()
    }

    pub fn get(&self, id: &str) -> io::Result<ReleaseManifest> {
        validate_release_id(id)?;
        self.load()?.find(id).cloned().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("release {id} was not found"),
            )
        })
    }

    /// Register a slot manifest without making it active. The slot's
    /// `manifest.json` is immutable once written; update executors may fill
    /// the sibling slot contents before or after this metadata operation.
    pub fn register(
        &self,
        id: &str,
        version: &str,
        source: Option<String>,
        note: Option<String>,
    ) -> io::Result<ReleaseCatalog> {
        validate_release_id(id)?;
        validate_release_version(version)?;
        validate_optional_release_text(source.as_deref(), "release source")?;
        validate_optional_release_text(note.as_deref(), "release note")?;

        let _guard = self.lock_gate()?;
        let mut catalog = self.load_unlocked()?;
        if catalog.find(id).is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("release {id} is already registered"),
            ));
        }
        let manifest = ReleaseManifest {
            id: id.to_owned(),
            version: version.to_owned(),
            installed_at_unix: unix_time_seconds(),
            source,
            note,
        };
        let slot_dir = self.slot_dir(id)?;
        fs::create_dir_all(&slot_dir)?;
        write_json_atomic(&slot_dir, &slot_dir.join("manifest.json"), &manifest)?;
        catalog.releases.push(manifest);
        catalog.normalize()?;
        Ok(catalog)
    }

    /// Publish a prepared candidate directory as an immutable release slot.
    /// The candidate must live below Nexus `downloads/`; it is renamed into
    /// `releases/<id>` and the manifest is written last so an interrupted
    /// preparation can never become selectable.
    pub fn register_prepared(
        &self,
        candidate_dir: &Path,
        id: &str,
        version: &str,
        source: Option<String>,
        note: Option<String>,
    ) -> io::Result<ReleaseCatalog> {
        validate_release_id(id)?;
        validate_release_version(version)?;
        validate_optional_release_text(source.as_deref(), "release source")?;
        validate_optional_release_text(note.as_deref(), "release note")?;

        let _guard = self.lock_gate()?;
        let mut catalog = self.load_unlocked()?;
        if catalog.find(id).is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("release {id} is already registered"),
            ));
        }
        self.paths.ensure_directories()?;
        let candidate = fs::canonicalize(candidate_dir)?;
        if !candidate.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "prepared release candidate is not a directory",
            ));
        }
        let downloads_root = fs::canonicalize(&self.paths.downloads_dir)?;
        if !is_within(&downloads_root, &candidate) || candidate == downloads_root {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "prepared release candidate must be below Nexus downloads",
            ));
        }

        let slot_dir = self.slot_dir(id)?;
        if slot_dir.exists() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("release slot {} already exists", slot_dir.display()),
            ));
        }
        fs::rename(&candidate, &slot_dir)?;
        let manifest = ReleaseManifest {
            id: id.to_owned(),
            version: version.to_owned(),
            installed_at_unix: unix_time_seconds(),
            source,
            note,
        };
        if let Err(error) = write_json_atomic(&slot_dir, &slot_dir.join("manifest.json"), &manifest)
        {
            // Leave the renamed directory in place without a manifest. Load
            // deliberately ignores such an incomplete slot for recovery.
            return Err(error);
        }
        catalog.releases.push(manifest);
        catalog.normalize()?;
        Ok(catalog)
    }

    /// Atomically make an already-registered slot current. The previous
    /// current pointer becomes last-known-good; no manifest is modified.
    pub fn promote(&self, id: &str) -> io::Result<ReleaseCatalog> {
        validate_release_id(id)?;
        let _guard = self.lock_gate()?;
        let mut catalog = self.load_unlocked()?;
        if catalog.find(id).is_none() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("release {id} was not found"),
            ));
        }
        if catalog.current_release.as_deref() != Some(id) {
            if let Some(previous) = catalog.current_release.replace(id.to_owned()) {
                catalog.last_known_good = Some(previous);
            }
            self.write_pointers(&catalog)?;
        }
        Ok(catalog)
    }

    /// Atomically restore the release selection captured by a checkpoint.
    /// A selected release must still be a registered, contained slot. A
    /// checkpoint without a release clears the current pointer. The prior
    /// current release remains the reversible last-known-good selection.
    pub fn restore_checkpoint_release(
        &self,
        release_id: Option<&str>,
    ) -> io::Result<ReleaseCatalog> {
        if let Some(id) = release_id {
            validate_release_id(id)?;
        }
        let _guard = self.lock_gate()?;
        let mut catalog = self.load_unlocked()?;
        self.apply_checkpoint_release(&mut catalog, release_id)?;
        self.write_pointers(&catalog)?;
        Ok(catalog)
    }

    /// Validate and calculate the release pointers a checkpoint restore would
    /// publish without changing durable state. This lets callers durably
    /// record a complete transaction intent before the first metadata write.
    pub fn plan_checkpoint_release(&self, release_id: Option<&str>) -> io::Result<ReleaseCatalog> {
        if let Some(id) = release_id {
            validate_release_id(id)?;
        }
        let _guard = self.lock_gate()?;
        let mut catalog = self.load_unlocked()?;
        self.apply_checkpoint_release(&mut catalog, release_id)?;
        Ok(catalog)
    }

    fn apply_checkpoint_release(
        &self,
        catalog: &mut ReleaseCatalog,
        release_id: Option<&str>,
    ) -> io::Result<()> {
        if let Some(id) = release_id {
            self.release_root_unlocked(id, catalog)?;
        }
        let restored = release_id.map(str::to_owned);
        if catalog.current_release != restored {
            let previous = std::mem::replace(&mut catalog.current_release, restored);
            if let Some(previous) = previous {
                catalog.last_known_good = Some(previous);
            } else if catalog.last_known_good == catalog.current_release {
                catalog.last_known_good = None;
            }
            catalog.normalize()?;
        }
        Ok(())
    }

    /// Restore an exact pair of release pointers after a larger metadata
    /// transaction fails. Both optional targets must still resolve to safe,
    /// registered immutable slots before either pointer is published.
    pub fn restore_release_pointers(
        &self,
        current_release: Option<&str>,
        last_known_good: Option<&str>,
    ) -> io::Result<ReleaseCatalog> {
        for id in [current_release, last_known_good].into_iter().flatten() {
            validate_release_id(id)?;
        }
        let _guard = self.lock_gate()?;
        let mut catalog = self.load_unlocked()?;
        for id in [current_release, last_known_good].into_iter().flatten() {
            self.release_root_unlocked(id, &catalog)?;
        }
        let current_release = current_release.map(str::to_owned);
        let last_known_good = last_known_good.map(str::to_owned);
        if catalog.current_release != current_release || catalog.last_known_good != last_known_good
        {
            catalog.current_release = current_release;
            catalog.last_known_good = last_known_good;
            catalog.normalize()?;
            self.write_pointers(&catalog)?;
        }
        Ok(catalog)
    }

    /// Swap current and last-known-good pointers. This is intentionally a
    /// reversible metadata operation and does not start Harness.
    pub fn rollback(&self) -> io::Result<ReleaseCatalog> {
        let _guard = self.lock_gate()?;
        let mut catalog = self.load_unlocked()?;
        let Some(last_known_good) = catalog.last_known_good.clone() else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "no last-known-good release is available",
            ));
        };
        let previous = catalog.current_release.replace(last_known_good);
        catalog.last_known_good = previous;
        self.write_pointers(&catalog)?;
        Ok(catalog)
    }

    fn load_unlocked(&self) -> io::Result<ReleaseCatalog> {
        let pointers = if self.paths.release_pointers_file.exists() {
            let bytes = fs::read(&self.paths.release_pointers_file)?;
            decode_json::<ReleasePointerDocument>(&bytes).map_err(invalid_data)?
        } else {
            ReleasePointerDocument {
                schema_version: RELEASE_SCHEMA_VERSION,
                current_release: None,
                last_known_good: None,
            }
        };
        if pointers.schema_version == 0 {
            // Version zero was never published, but accepting it here keeps
            // the same forward-compatible convention as other Nexus stores.
        }

        let mut catalog = ReleaseCatalog {
            schema_version: RELEASE_SCHEMA_VERSION,
            current_release: pointers.current_release,
            last_known_good: pointers.last_known_good,
            releases: Vec::new(),
        };
        if self.paths.releases_dir.exists() {
            for entry in fs::read_dir(&self.paths.releases_dir)? {
                let entry = entry?;
                if !entry.file_type()?.is_dir() {
                    continue;
                }
                let slot_id = entry.file_name().to_string_lossy().into_owned();
                let manifest_path = entry.path().join("manifest.json");
                if !manifest_path.exists() {
                    // A partially prepared slot is not selectable until its
                    // immutable manifest is published.
                    continue;
                }
                let bytes = fs::read(&manifest_path)?;
                let manifest: ReleaseManifest = decode_json(&bytes).map_err(invalid_data)?;
                validate_release_manifest(&manifest)?;
                if manifest.id != slot_id {
                    return Err(invalid_data("release directory does not match manifest id"));
                }
                catalog.releases.push(manifest);
            }
        }
        catalog.normalize()?;
        Ok(catalog)
    }

    fn write_pointers(&self, catalog: &ReleaseCatalog) -> io::Result<()> {
        let pointers = ReleasePointerDocument {
            schema_version: RELEASE_SCHEMA_VERSION,
            current_release: catalog.current_release.clone(),
            last_known_good: catalog.last_known_good.clone(),
        };
        write_json_atomic(
            &self.paths.root,
            &self.paths.release_pointers_file,
            &pointers,
        )
    }

    fn slot_dir(&self, id: &str) -> io::Result<PathBuf> {
        validate_release_id(id)?;
        Ok(self.paths.releases_dir.join(id))
    }

    fn lock_gate(&self) -> io::Result<std::sync::MutexGuard<'_, ()>> {
        self.write_gate
            .lock()
            .map_err(|_| io::Error::other("release store lock is poisoned"))
    }
}

fn default_release_schema() -> u32 {
    RELEASE_SCHEMA_VERSION
}

pub fn is_valid_release_id(id: &str) -> bool {
    validate_release_id(id).is_ok()
}

pub fn validate_release_id(id: &str) -> io::Result<()> {
    let valid_chars = id
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'));
    if id.is_empty() || id.len() > MAX_RELEASE_ID_LEN || id == "." || id == ".." || !valid_chars {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "release id must use a safe ASCII identifier",
        ));
    }
    Ok(())
}

pub fn validate_release_version(version: &str) -> io::Result<()> {
    if version.is_empty() || version.len() > MAX_RELEASE_VERSION_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("release version must be 1-{MAX_RELEASE_VERSION_LEN} bytes"),
        ));
    }
    validate_release_text(version, "release version")
}

fn validate_optional_release_text(value: Option<&str>, label: &str) -> io::Result<()> {
    if let Some(value) = value {
        if value.len() > MAX_RELEASE_TEXT_LEN {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{label} exceeds {MAX_RELEASE_TEXT_LEN} bytes"),
            ));
        }
        validate_release_text(value, label)?;
    }
    Ok(())
}

fn validate_release_text(value: &str, label: &str) -> io::Result<()> {
    if value.chars().any(|character| character.is_control()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{label} contains a control character"),
        ));
    }
    Ok(())
}

fn validate_release_manifest(manifest: &ReleaseManifest) -> io::Result<()> {
    validate_release_id(&manifest.id)?;
    validate_release_version(&manifest.version)?;
    validate_optional_release_text(manifest.source.as_deref(), "release source")?;
    validate_optional_release_text(manifest.note.as_deref(), "release note")
}

fn write_json_atomic<T: Serialize>(root: &Path, destination: &Path, value: &T) -> io::Result<()> {
    let bytes = encode_json(value).map_err(invalid_data)?;
    fs::create_dir_all(root)?;
    let file_name = destination
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| invalid_data("destination has no valid filename"))?;
    let temp_path = root.join(format!(
        ".{file_name}.tmp-{}-{}",
        std::process::id(),
        unix_time_nanos()
    ));
    let write_result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)?;
        use io::Write;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        atomic_replace(&temp_path, destination)?;
        // On Unix, fsync the containing directory after rename so the name
        // itself is durable across a crash, not just the temporary file's
        // contents. Windows' replace operation provides the platform-specific
        // durability boundary and does not support opening directories this
        // way.
        #[cfg(unix)]
        {
            let directory = destination.parent().unwrap_or(root);
            fs::File::open(directory)?.sync_all()?;
        }
        Ok(())
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    write_result
}

/// Resolve a user-level data root without relying on a platform-specific crate.
pub fn default_data_root() -> PathBuf {
    if let Some(data_dir) = env::var_os(DATA_DIR_ENV).filter(|value| !value.is_empty()) {
        return PathBuf::from(data_dir);
    }

    #[cfg(windows)]
    {
        if let Some(local_app_data) = env::var_os("LOCALAPPDATA").filter(|value| !value.is_empty())
        {
            return PathBuf::from(local_app_data).join("Nexus");
        }
    }

    #[cfg(target_os = "macos")]
    {
        if let Some(home) = home_dir() {
            return home
                .join("Library")
                .join("Application Support")
                .join("Nexus");
        }
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if let Some(state_home) = env::var_os("XDG_STATE_HOME").filter(|value| !value.is_empty()) {
            return PathBuf::from(state_home).join("nexus");
        }
        if let Some(home) = home_dir() {
            return home.join(".local").join("state").join("nexus");
        }
    }

    home_dir()
        .map(|home| home.join(".nexus"))
        .unwrap_or_else(|| PathBuf::from(".nexus"))
}

fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        env::var_os("USERPROFILE")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    }

    #[cfg(not(windows))]
    {
        env::var_os("HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentState {
    pub lifecycle: AgentLifecycleState,
    pub harness: HarnessState,
    pub profile: Option<String>,
    pub release: Option<String>,
    pub started_at_unix: u64,
    pub updated_at_unix: u64,
}

impl AgentState {
    pub fn starting() -> Self {
        let now = unix_time_seconds();
        Self {
            lifecycle: AgentLifecycleState::Starting,
            harness: HarnessState::Detached,
            profile: Some(DEFAULT_PROFILE.to_owned()),
            release: None,
            started_at_unix: now,
            updated_at_unix: now,
        }
    }

    pub fn mark_running(&mut self) {
        self.lifecycle = AgentLifecycleState::Running;
        self.touch();
    }

    pub fn request_shutdown(&mut self) {
        self.lifecycle = AgentLifecycleState::ShuttingDown;
        self.touch();
    }

    pub fn mark_stopped(&mut self) {
        self.lifecycle = AgentLifecycleState::Stopped;
        self.touch();
    }

    pub fn set_harness(&mut self, harness: HarnessState) {
        self.harness = harness;
        self.touch();
    }

    pub fn set_profile(&mut self, profile: impl Into<String>) {
        self.profile = Some(profile.into());
        self.touch();
    }

    pub fn set_release(&mut self, release: Option<String>) {
        self.release = release;
        self.touch();
    }

    pub fn as_payload(&self) -> AgentStatePayload {
        AgentStatePayload {
            lifecycle: self.lifecycle,
            harness: self.harness,
            profile: self.profile.clone(),
            release: self.release.clone(),
            started_at_unix: self.started_at_unix,
            updated_at_unix: self.updated_at_unix,
        }
    }

    fn touch(&mut self) {
        self.updated_at_unix = unix_time_seconds();
    }
}

/// Durable Nexus-owned runtime metadata. The state file is intentionally
/// separate from any Harness working/data directory and contains no secrets.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NexusRuntimeMetadata {
    pub schema_version: u32,
    pub lifecycle: AgentLifecycleState,
    pub harness: HarnessRuntimeInfo,
    pub profile: Option<String>,
    pub release: Option<String>,
    pub started_at_unix: u64,
    pub updated_at_unix: u64,
}

/// Durable boundary that separates authentication URLs emitted by different
/// Harness runs while retaining the append-only Nexus logs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HarnessLogSession {
    pub schema_version: u32,
    pub run_id: String,
    pub generation: u64,
    pub stdout_watermark: u64,
    pub stderr_watermark: u64,
    pub stdout_file_identity: String,
    pub stderr_file_identity: String,
    /// Safe filenames below Nexus' logs directory. Fresh Harness launches use
    /// fresh files, preventing copy-truncate of an earlier run from reviving
    /// an earlier authentication token.
    #[serde(default = "legacy_stdout_log_name")]
    pub stdout_log_name: String,
    #[serde(default = "legacy_stderr_log_name")]
    pub stderr_log_name: String,
    /// Set before process creation and retained for the active/recovering run.
    /// It is cleared only after Nexus proves the run terminated, so an Agent
    /// restart cannot treat an uncertain descendant as permission to spawn a
    /// duplicate Harness.
    #[serde(default)]
    pub launch_pending: bool,
    pub created_at_unix: u64,
}

impl HarnessLogSession {
    pub fn new(
        run_id: String,
        generation: u64,
        stdout_watermark: u64,
        stderr_watermark: u64,
        stdout_file_identity: String,
        stderr_file_identity: String,
        stdout_log_name: String,
        stderr_log_name: String,
        launch_pending: bool,
        created_at_unix: u64,
    ) -> Self {
        Self {
            schema_version: HARNESS_LOG_SESSION_SCHEMA_VERSION,
            run_id,
            generation,
            stdout_watermark,
            stderr_watermark,
            stdout_file_identity,
            stderr_file_identity,
            stdout_log_name,
            stderr_log_name,
            launch_pending,
            created_at_unix,
        }
    }

    pub fn is_current_schema(&self) -> bool {
        self.schema_version == HARNESS_LOG_SESSION_SCHEMA_VERSION
    }
}

#[derive(Clone)]
pub struct HarnessLogSessionStore {
    paths: NexusPaths,
}

impl HarnessLogSessionStore {
    pub fn new(paths: NexusPaths) -> Self {
        Self { paths }
    }

    pub fn path(&self) -> PathBuf {
        self.paths.run_dir.join("harness-log-session.json")
    }

    pub fn read(&self) -> io::Result<Option<HarnessLogSession>> {
        let path = self.path();
        if !path.exists() {
            return Ok(None);
        }
        let bytes = fs::read(path)?;
        let session: HarnessLogSession = decode_json(&bytes).map_err(invalid_data)?;
        if !matches!(
            session.schema_version,
            1 | HARNESS_LOG_SESSION_SCHEMA_VERSION
        ) || session.run_id.is_empty()
            || session.run_id.len() > 128
            || session.run_id.chars().any(char::is_control)
            || !valid_log_file_identity(&session.stdout_file_identity)
            || !valid_log_file_identity(&session.stderr_file_identity)
            || !valid_log_name(&session.stdout_log_name)
            || !valid_log_name(&session.stderr_log_name)
        {
            return Err(invalid_data("invalid Harness log session marker"));
        }
        Ok(Some(session))
    }

    pub fn write(&self, session: &HarnessLogSession) -> io::Result<()> {
        if session.schema_version != HARNESS_LOG_SESSION_SCHEMA_VERSION
            || session.run_id.is_empty()
            || session.run_id.len() > 128
            || session.run_id.chars().any(char::is_control)
            || !valid_log_file_identity(&session.stdout_file_identity)
            || !valid_log_file_identity(&session.stderr_file_identity)
            || !valid_log_name(&session.stdout_log_name)
            || !valid_log_name(&session.stderr_log_name)
        {
            return Err(invalid_data("invalid Harness log session marker"));
        }
        write_json_atomic(&self.paths.run_dir, &self.path(), session)
    }
}

fn legacy_stdout_log_name() -> String {
    "harness.stdout.log".to_owned()
}

fn legacy_stderr_log_name() -> String {
    "harness.stderr.log".to_owned()
}

fn valid_log_file_identity(value: &str) -> bool {
    !value.is_empty() && value.len() <= 128 && !value.chars().any(char::is_control)
}

fn valid_log_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 192
        && value != "."
        && value != ".."
        && !value.chars().any(char::is_control)
        && Path::new(value)
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)))
        && Path::new(value).components().count() == 1
}

/// Canonical identity advertised by the Agent and checked by the Launcher
/// before it adopts or stops anything already bound to the configured port.
pub fn data_root_identity(paths: &NexusPaths) -> io::Result<String> {
    let canonical = fs::canonicalize(&paths.root)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        let metadata = fs::metadata(canonical)?;
        return Ok(format!("unix:{}:{}", metadata.dev(), metadata.ino()));
    }

    #[cfg(windows)]
    {
        use std::{os::windows::ffi::OsStrExt, ptr::null_mut};
        use windows_sys::Win32::{
            Foundation::{CloseHandle, INVALID_HANDLE_VALUE},
            Storage::FileSystem::{
                CreateFileW, GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
                FILE_FLAG_BACKUP_SEMANTICS, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE,
                FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
            },
        };

        let wide: Vec<u16> = canonical
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                FILE_READ_ATTRIBUTES,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                null_mut(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS,
                null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let mut information: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        let succeeded = unsafe { GetFileInformationByHandle(handle, &mut information) };
        unsafe { CloseHandle(handle) };
        if succeeded == 0 {
            return Err(io::Error::last_os_error());
        }
        let file_index =
            ((information.nFileIndexHigh as u64) << 32) | information.nFileIndexLow as u64;
        return Ok(format!(
            "windows:{}:{}",
            information.dwVolumeSerialNumber, file_index
        ));
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = canonical;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "data-root identity is unavailable on this platform",
        ))
    }
}

/// Per-process opaque value used to correlate a launch record with one Agent
/// instance. It is not a credential and is exposed only on loopback health.
pub fn new_instance_id() -> String {
    format!(
        "{}-{}-{}",
        std::process::id(),
        unix_time_nanos(),
        unix_time_seconds()
    )
}

/// Stable identity of an already-open log file. It is used together with the
/// byte watermark so a same-path replacement cannot be mistaken for the file
/// that received the current Harness child's output.
pub fn log_file_identity(file: &fs::File) -> io::Result<String> {
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
        };

        let mut information: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        let succeeded =
            unsafe { GetFileInformationByHandle(file.as_raw_handle() as _, &mut information) };
        if succeeded == 0 {
            return Err(io::Error::last_os_error());
        }
        let file_index =
            ((information.nFileIndexHigh as u64) << 32) | information.nFileIndexLow as u64;
        return Ok(format!(
            "windows:{}:{}",
            information.dwVolumeSerialNumber, file_index
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = file.metadata()?;
        return Ok(format!("unix:{}:{}", metadata.dev(), metadata.ino()));
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = file;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "this platform cannot identify an open log file",
        ))
    }
}

impl NexusRuntimeMetadata {
    pub fn from_state(state: &AgentState, harness: HarnessRuntimeInfo) -> Self {
        Self {
            schema_version: RUNTIME_SCHEMA_VERSION,
            lifecycle: state.lifecycle,
            harness,
            profile: state.profile.clone(),
            release: state.release.clone(),
            started_at_unix: state.started_at_unix,
            updated_at_unix: state.updated_at_unix,
        }
    }

    pub fn detached() -> Self {
        let now = unix_time_seconds();
        Self {
            schema_version: RUNTIME_SCHEMA_VERSION,
            lifecycle: AgentLifecycleState::Stopped,
            harness: HarnessRuntimeInfo::detached(),
            profile: Some(DEFAULT_PROFILE.to_owned()),
            release: None,
            started_at_unix: now,
            updated_at_unix: now,
        }
    }
}

/// Serialize and replace `state.json` as one atomic publication. The
/// temporary file is created beside the destination and flushed before the
/// platform-specific atomic replace operation.
pub fn write_runtime_metadata(
    paths: &NexusPaths,
    metadata: &NexusRuntimeMetadata,
) -> io::Result<()> {
    paths.ensure_directories()?;
    write_json_atomic(&paths.root, &paths.state_file, metadata)
}

pub fn read_runtime_metadata(paths: &NexusPaths) -> io::Result<Option<NexusRuntimeMetadata>> {
    if !paths.state_file.exists() {
        return Ok(None);
    }

    let bytes = fs::read(&paths.state_file)?;
    decode_json(&bytes).map(Some).map_err(invalid_data)
}

/// In-process serialization gate shared by the Agent and supervisor. It
/// prevents a child-exit observer from racing an Agent lifecycle write while
/// each publication remains independently atomic on disk.
#[derive(Clone)]
pub struct RuntimeMetadataStore {
    paths: NexusPaths,
    write_gate: Arc<Mutex<()>>,
}

impl RuntimeMetadataStore {
    pub fn new(paths: NexusPaths) -> Self {
        Self {
            paths,
            write_gate: Arc::new(Mutex::new(())),
        }
    }

    pub fn paths(&self) -> &NexusPaths {
        &self.paths
    }

    pub fn read(&self) -> io::Result<Option<NexusRuntimeMetadata>> {
        let _guard = self.lock_gate()?;
        read_runtime_metadata(&self.paths)
    }

    pub fn write(&self, metadata: &NexusRuntimeMetadata) -> io::Result<()> {
        let _guard = self.lock_gate()?;
        write_runtime_metadata(&self.paths, metadata)
    }

    pub fn write_snapshot(
        &self,
        state: &AgentState,
        harness: HarnessRuntimeInfo,
    ) -> io::Result<()> {
        self.write(&NexusRuntimeMetadata::from_state(state, harness))
    }

    pub fn update_harness(&self, harness: HarnessRuntimeInfo) -> io::Result<()> {
        let _guard = self.lock_gate()?;
        let mut metadata =
            read_runtime_metadata(&self.paths)?.unwrap_or_else(NexusRuntimeMetadata::detached);
        metadata.harness = harness;
        metadata.updated_at_unix = unix_time_seconds();
        write_runtime_metadata(&self.paths, &metadata)
    }

    fn lock_gate(&self) -> io::Result<std::sync::MutexGuard<'_, ()>> {
        self.write_gate
            .lock()
            .map_err(|_| io::Error::other("runtime metadata lock is poisoned"))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct UpdateStateDocument {
    #[serde(default = "default_update_schema")]
    schema_version: u32,
    update: UpdateRuntimeInfo,
}

/// Durable status for the in-process external update executor. It is kept in
/// its own file so update failures cannot corrupt Agent/Harness runtime state.
#[derive(Clone)]
pub struct UpdateStateStore {
    paths: NexusPaths,
    write_gate: Arc<Mutex<()>>,
}

impl UpdateStateStore {
    pub fn new(paths: NexusPaths) -> Self {
        Self {
            paths,
            write_gate: Arc::new(Mutex::new(())),
        }
    }

    pub fn paths(&self) -> &NexusPaths {
        &self.paths
    }

    pub fn load(&self) -> io::Result<UpdateRuntimeInfo> {
        let _guard = self.lock_gate()?;
        if !self.paths.update_state_file.exists() {
            return Ok(UpdateRuntimeInfo::idle());
        }
        let bytes = fs::read(&self.paths.update_state_file)?;
        let document: UpdateStateDocument = decode_json(&bytes).map_err(invalid_data)?;
        Ok(document.update)
    }

    pub fn write(&self, update: &UpdateRuntimeInfo) -> io::Result<()> {
        let _guard = self.lock_gate()?;
        let document = UpdateStateDocument {
            schema_version: UPDATE_SCHEMA_VERSION,
            update: update.clone(),
        };
        write_json_atomic(&self.paths.root, &self.paths.update_state_file, &document)
    }

    /// An Agent restart cannot reattach to an update child that was running
    /// in the previous process. Mark that stale state failed rather than
    /// reporting a job that no longer exists.
    pub fn recover_unattached(&self) -> io::Result<UpdateRuntimeInfo> {
        let _guard = self.lock_gate()?;
        if !self.paths.update_state_file.exists() {
            return Ok(UpdateRuntimeInfo::idle());
        }
        let bytes = fs::read(&self.paths.update_state_file)?;
        let document: UpdateStateDocument = decode_json(&bytes).map_err(invalid_data)?;
        let mut update = document.update;
        if update.state == UpdateState::Running {
            update.state = UpdateState::Failed;
            update.finished_at_unix = Some(unix_time_seconds());
            update.error =
                Some("previous update process was not attached to this Agent instance".to_owned());
            let document = UpdateStateDocument {
                schema_version: UPDATE_SCHEMA_VERSION,
                update: update.clone(),
            };
            write_json_atomic(&self.paths.root, &self.paths.update_state_file, &document)?;
        }
        Ok(update)
    }

    fn lock_gate(&self) -> io::Result<std::sync::MutexGuard<'_, ()>> {
        self.write_gate
            .lock()
            .map_err(|_| io::Error::other("update state lock is poisoned"))
    }
}

fn default_update_schema() -> u32 {
    UPDATE_SCHEMA_VERSION
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct DiagnosticsDocument {
    #[serde(default = "default_diagnostics_schema")]
    schema_version: u32,
    bundle: DiagnosticsBundle,
}

/// Nexus-only, bounded diagnostics snapshots. Collection reads a fixed set of
/// Nexus metadata and text logs; it never traverses `$HOME/.dsh`, Harness data,
/// or the process environment.
#[derive(Clone)]
pub struct DiagnosticsStore {
    paths: NexusPaths,
    write_gate: Arc<Mutex<()>>,
}

impl DiagnosticsStore {
    pub fn new(paths: NexusPaths) -> Self {
        Self {
            paths,
            write_gate: Arc::new(Mutex::new(())),
        }
    }

    pub fn paths(&self) -> &NexusPaths {
        &self.paths
    }

    pub fn list(&self) -> io::Result<Vec<DiagnosticsBundle>> {
        let _guard = self.lock_gate()?;
        if !self.paths.diagnostics_dir.exists() {
            return Ok(Vec::new());
        }
        let mut bundles = Vec::new();
        for entry in fs::read_dir(&self.paths.diagnostics_dir)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let manifest_path = entry.path().join("diagnostics.json");
            if !manifest_path.exists() {
                continue;
            }
            let bytes = fs::read(&manifest_path)?;
            let document: DiagnosticsDocument = decode_json(&bytes).map_err(invalid_data)?;
            if document.schema_version != DIAGNOSTICS_SCHEMA_VERSION {
                return Err(invalid_data("unsupported diagnostics schema version"));
            }
            validate_diagnostics_bundle(&document.bundle)?;
            bundles.push(document.bundle);
        }
        bundles.sort_by(|left, right| {
            right
                .created_at_unix
                .cmp(&left.created_at_unix)
                .then_with(|| right.id.cmp(&left.id))
        });
        bundles.truncate(MAX_DIAGNOSTICS_BUNDLES);
        Ok(bundles)
    }

    pub fn collect(&self, note: Option<String>) -> io::Result<DiagnosticsBundle> {
        validate_optional_diagnostics_text(note.as_deref(), "diagnostics note")?;
        let _guard = self.lock_gate()?;
        self.paths.ensure_directories()?;
        let id = format!("diag-{}", unix_time_nanos());
        validate_release_id(&id)?;
        let bundle_dir = self.paths.diagnostics_dir.join(&id);
        let files_dir = bundle_dir.join("files");
        fs::create_dir(&bundle_dir)?;
        fs::create_dir(&files_dir)?;

        let mut sources = Vec::new();
        for path in [
            &self.paths.state_file,
            &self.paths.profiles_file,
            &self.paths.release_pointers_file,
            &self.paths.update_state_file,
        ] {
            if is_regular_diagnostics_file(path) {
                if let Some(name) = path.file_name().and_then(|value| value.to_str()) {
                    sources.push((
                        path.clone(),
                        PathBuf::from(name),
                        MAX_DIAGNOSTICS_FILE_BYTES,
                    ));
                }
            }
        }
        if self.paths.logs_dir.is_dir() {
            let mut logs = fs::read_dir(&self.paths.logs_dir)?
                .filter_map(Result::ok)
                .filter(|entry| {
                    entry
                        .file_type()
                        .map(|kind| kind.is_file())
                        .unwrap_or(false)
                })
                .filter_map(|entry| {
                    let name = entry.file_name().to_str()?.to_owned();
                    Some((
                        entry.path(),
                        PathBuf::from("logs").join(name),
                        MAX_DIAGNOSTICS_LOG_BYTES,
                    ))
                })
                .collect::<Vec<_>>();
            logs.sort_by(|left, right| left.1.cmp(&right.1));
            sources.extend(logs);
        }
        sources.truncate(MAX_DIAGNOSTICS_FILES);

        let mut files = Vec::new();
        for (source, relative, limit) in sources {
            let raw = read_diagnostics_file(&source, limit)?;
            let truncated = raw.len() > limit;
            let bounded = if truncated { &raw[..limit] } else { &raw[..] };
            let (payload, redacted) = redact_diagnostics_payload(bounded);
            let destination = files_dir.join(&relative);
            let Some(parent) = destination.parent() else {
                continue;
            };
            fs::create_dir_all(parent)?;
            write_diagnostics_file(&destination, &payload)?;
            files.push(DiagnosticsFile {
                name: relative.to_string_lossy().replace('\\', "/"),
                bytes: payload.len() as u64,
                redacted,
                truncated,
            });
        }
        files.sort_by(|left, right| left.name.cmp(&right.name));
        let directory = fs::canonicalize(&bundle_dir)?
            .to_string_lossy()
            .into_owned();
        let bundle = DiagnosticsBundle {
            id,
            created_at_unix: unix_time_seconds(),
            directory,
            note,
            files,
        };
        validate_diagnostics_bundle(&bundle)?;
        let document = DiagnosticsDocument {
            schema_version: DIAGNOSTICS_SCHEMA_VERSION,
            bundle: bundle.clone(),
        };
        write_json_atomic(&bundle_dir, &bundle_dir.join("diagnostics.json"), &document)?;
        Ok(bundle)
    }

    fn lock_gate(&self) -> io::Result<std::sync::MutexGuard<'_, ()>> {
        self.write_gate
            .lock()
            .map_err(|_| io::Error::other("diagnostics lock is poisoned"))
    }
}

fn default_diagnostics_schema() -> u32 {
    DIAGNOSTICS_SCHEMA_VERSION
}

fn validate_optional_diagnostics_text(value: Option<&str>, label: &str) -> io::Result<()> {
    if let Some(value) = value {
        if value.len() > MAX_DIAGNOSTICS_NOTE_LEN || value.chars().any(char::is_control) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{label} is too long or contains a control character"),
            ));
        }
    }
    Ok(())
}

fn validate_diagnostics_bundle(bundle: &DiagnosticsBundle) -> io::Result<()> {
    validate_release_id(&bundle.id)?;
    if bundle.directory.is_empty() || bundle.directory.chars().any(char::is_control) {
        return Err(invalid_data("diagnostics directory is invalid"));
    }
    validate_optional_diagnostics_text(bundle.note.as_deref(), "diagnostics note")?;
    if bundle.files.len() > MAX_DIAGNOSTICS_FILES {
        return Err(invalid_data("diagnostics bundle contains too many files"));
    }
    let mut previous = None;
    for file in &bundle.files {
        let segments = file.name.split('/').collect::<Vec<_>>();
        if file.name.is_empty()
            || file.name.starts_with('/')
            || segments
                .iter()
                .any(|segment| segment.is_empty() || *segment == "." || *segment == "..")
            || file.name.chars().any(char::is_control)
        {
            return Err(invalid_data("diagnostics file name is unsafe"));
        }
        if let Some(previous) = previous {
            if previous >= file.name.as_str() {
                return Err(invalid_data("diagnostics files are not sorted"));
            }
        }
        previous = Some(file.name.as_str());
    }
    Ok(())
}

fn redact_diagnostics_payload(payload: &[u8]) -> (Vec<u8>, bool) {
    let Ok(text) = std::str::from_utf8(payload) else {
        return (b"[binary diagnostics payload omitted]\n".to_vec(), true);
    };
    let mut redacted = false;
    let mut output = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        if diagnostics_line_is_sensitive(line) {
            output.push_str("[REDACTED]\n");
            redacted = true;
        } else {
            output.push_str(line);
        }
    }
    (output.into_bytes(), redacted)
}

fn is_regular_diagnostics_file(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|metadata| metadata.file_type().is_file())
        .unwrap_or(false)
}

fn read_diagnostics_file(path: &Path, limit: usize) -> io::Result<Vec<u8>> {
    use io::Read;
    let file = fs::File::open(path)?;
    let mut bytes = Vec::new();
    file.take((limit as u64).saturating_add(1))
        .read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn write_diagnostics_file(path: &Path, payload: &[u8]) -> io::Result<()> {
    use io::Write;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(payload)?;
    file.sync_all()
}

fn diagnostics_line_is_sensitive(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    [
        "password",
        "passwd",
        "secret",
        "authorization",
        "api_key",
        "apikey",
        "access_token",
        "refresh_token",
        "cookie",
        "private_key",
    ]
    .iter()
    .any(|marker| lower.contains(marker) && (line.contains('=') || line.contains(':')))
}

fn unix_time_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default()
}

#[cfg(unix)]
fn atomic_replace(temp_path: &Path, state_path: &Path) -> io::Result<()> {
    fs::rename(temp_path, state_path)
}

#[cfg(windows)]
fn atomic_replace(temp_path: &Path, state_path: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn MoveFileExW(from: *const u16, to: *const u16, flags: u32) -> i32;
    }

    let from: Vec<u16> = temp_path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let to: Vec<u16> = state_path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;

    // SAFETY: both vectors are NUL-terminated UTF-16 paths that remain alive
    // for the duration of the OS call; the flags request replacement and a
    // write-through publication.
    let result = unsafe {
        MoveFileExW(
            from.as_ptr(),
            to.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(any(unix, windows)))]
fn atomic_replace(temp_path: &Path, state_path: &Path) -> io::Result<()> {
    fs::rename(temp_path, state_path)
}

pub fn unix_time_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

pub fn is_within(root: &Path, path: &Path) -> bool {
    path.strip_prefix(root).is_ok()
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
    };

    use nexus_protocol::{AgentLifecycleState, HarnessRuntimeInfo, HarnessState};

    use super::{
        is_within, load_harness_launch_spec, read_runtime_metadata, write_runtime_metadata,
        AgentState, CheckpointRestoreIntent, CheckpointRestoreJournalStore, CheckpointRestorePhase,
        CheckpointStore, ConfigStore, DiagnosticsStore, HarnessLaunchSpec, HarnessLogSession,
        HarnessLogSessionStore, NexusConfig, NexusConfigFile, NexusPaths, NexusRuntimeMetadata,
        ProfileCatalog, ProfileStore, ReleaseStore, UpdateSpec,
    };

    #[test]
    fn custom_paths_are_platform_neutral() {
        let paths = NexusPaths::from_root(PathBuf::from("workspace").join("nexus"));
        assert_eq!(
            paths.state_file,
            PathBuf::from("workspace/nexus/state.json")
        );
        assert_eq!(
            paths.releases_dir,
            PathBuf::from("workspace/nexus/releases")
        );
        assert_eq!(
            paths.release_pointers_file,
            PathBuf::from("workspace/nexus/release-pointers.json")
        );
        assert_eq!(
            paths.update_state_file,
            PathBuf::from("workspace/nexus/update-state.json")
        );
        assert_eq!(
            paths.diagnostics_dir,
            PathBuf::from("workspace/nexus/diagnostics")
        );
    }

    #[test]
    fn config_binds_to_ipv4_loopback() {
        let config = NexusConfig::default();
        assert!(config.bind_addr().ip().is_loopback());
    }

    #[test]
    fn state_transitions_are_explicit() {
        let mut state = AgentState::starting();
        assert_eq!(state.lifecycle, AgentLifecycleState::Starting);
        assert_eq!(state.harness, HarnessState::Detached);

        state.mark_running();
        assert_eq!(state.lifecycle, AgentLifecycleState::Running);
        state.request_shutdown();
        assert_eq!(state.lifecycle, AgentLifecycleState::ShuttingDown);
    }

    #[test]
    fn containment_check_does_not_accept_sibling_paths() {
        let root = PathBuf::from("root");
        assert!(is_within(&root, &root.join("child")));
        assert!(!is_within(&root, &PathBuf::from("root-other/file")));
    }

    #[test]
    fn harness_spec_is_loaded_from_nexus_config_not_home_data() {
        let root = unique_test_root("config");
        let paths = NexusPaths::from_root(root.clone());
        fs::create_dir_all(&root).expect("test root creates");
        fs::write(
            &paths.config_file,
            r#"{
                "harness": {
                    "program": "bin/fake-harness",
                    "args": ["--headless"],
                    "working_dir": "runtime",
                    "readiness_url": "http://127.0.0.1:3210/health",
                    "readiness_timeout_secs": 2
                }
            }"#,
        )
        .expect("config writes");

        let spec = load_harness_launch_spec(&paths)
            .expect("config reads")
            .expect("harness is configured");
        assert_eq!(spec.program, PathBuf::from("bin/fake-harness"));
        assert_eq!(spec.args, vec!["--headless"]);
        assert_eq!(spec.working_dir, Some(PathBuf::from("runtime")));
        assert_eq!(spec.readiness_timeout_secs, Some(2));
        assert_ne!(paths.state_file, root.join(".dsh").join("state.json"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn runtime_metadata_round_trips_through_atomic_state_file() {
        let root = unique_test_root("state");
        let paths = NexusPaths::from_root(root.clone());
        let mut agent = AgentState::starting();
        agent.mark_running();
        let harness = HarnessRuntimeInfo::running(42, agent.started_at_unix, agent.updated_at_unix);
        let metadata = NexusRuntimeMetadata::from_state(&agent, harness);

        write_runtime_metadata(&paths, &metadata).expect("state writes");
        let loaded = read_runtime_metadata(&paths)
            .expect("state reads")
            .expect("state exists");
        assert_eq!(loaded, metadata);
        assert!(paths.state_file.exists());
        assert!(!paths.root.join("harness").join("state.json").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn data_root_identity_does_not_merge_distinct_non_utf8_directories() {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt};

        let parent = unique_test_root("non-utf8-root-identity");
        let first = parent.join(OsString::from_vec(vec![b'r', 0x80]));
        let second = parent.join(OsString::from_vec(vec![b'r', 0x81]));
        fs::create_dir_all(&first).expect("first non-UTF8 root creates");
        fs::create_dir_all(&second).expect("second non-UTF8 root creates");
        let first =
            super::data_root_identity(&NexusPaths::from_root(first)).expect("first identity reads");
        let second = super::data_root_identity(&NexusPaths::from_root(second))
            .expect("second identity reads");
        assert_ne!(first, second);
        let _ = fs::remove_dir_all(parent);
    }

    #[test]
    fn harness_log_session_round_trips_below_run_directory() {
        let root = unique_test_root("harness-log-session");
        let paths = NexusPaths::from_root(root.clone());
        let store = HarnessLogSessionStore::new(paths.clone());
        let session = HarnessLogSession::new(
            "run-42".to_owned(),
            42,
            100,
            200,
            "stdout-identity".to_owned(),
            "stderr-identity".to_owned(),
            "harness-run-42.stdout.log".to_owned(),
            "harness-run-42.stderr.log".to_owned(),
            true,
            300,
        );

        store.write(&session).expect("log session writes");
        assert_eq!(
            store.read().expect("log session reads"),
            Some(session.clone())
        );
        assert_eq!(store.path(), paths.run_dir.join("harness-log-session.json"));
        assert!(!root.join(".dsh").exists());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn profile_store_defaults_to_web_and_persists_selected_names() {
        let root = unique_test_root("profiles");
        let paths = NexusPaths::from_root(root.clone());
        let store = ProfileStore::new(paths.clone());

        let initial = store.load().expect("profile catalog loads");
        assert_eq!(initial.active_profile, "web");
        assert_eq!(initial.profiles, vec!["web"]);
        assert!(paths.profiles_file.exists());

        let selected = store.select("web.dark").expect("profile selects");
        assert_eq!(selected.active_profile, "web.dark");
        assert!(selected.profiles.iter().any(|name| name == "web.dark"));
        assert_eq!(store.load().expect("profile catalog reloads"), selected);
        assert!(store.select("../escape").is_err());
        assert!(!root.join(".dsh").exists());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn checkpoint_store_writes_and_lists_nexus_only_manifests() {
        let root = unique_test_root("checkpoints");
        let paths = NexusPaths::from_root(root.clone());
        let store = CheckpointStore::new(paths.clone());
        let state = super::NexusStateSnapshot {
            profile: "web".to_owned(),
            release: Some("r1".to_owned()),
        };

        let created = store
            .create(
                "web",
                Some("r1".to_owned()),
                Some("before".to_owned()),
                state,
            )
            .expect("checkpoint creates");
        assert!(created.id.starts_with("cp-"));
        assert!(paths
            .checkpoints_dir
            .join(format!("{}.json", created.id))
            .exists());
        assert_eq!(store.list().expect("checkpoints list").len(), 1);
        let encoded = String::from_utf8(
            fs::read(paths.checkpoints_dir.join(format!("{}.json", created.id)))
                .expect("checkpoint manifest reads"),
        )
        .expect("checkpoint manifest is UTF-8 JSON");
        assert!(!encoded.contains("\"lifecycle\""));
        assert!(!encoded.contains("\"harness\""));
        assert!(!encoded.contains("\"updated_at_unix\""));
        assert_eq!(
            store
                .read(&created.id)
                .expect("checkpoint reads")
                .expect("checkpoint exists"),
            created
        );

        let legacy_path = paths.checkpoints_dir.join("cp-legacy.json");
        fs::write(
            &legacy_path,
            br#"{
                "id":"cp-legacy",
                "created_at_unix":9,
                "profile":"web",
                "release":"r1",
                "state":{
                    "lifecycle":"running",
                    "harness":"stopped",
                    "profile":"web",
                    "release":"r1",
                    "updated_at_unix":9
                }
            }"#,
        )
        .expect("legacy checkpoint writes");
        let legacy = store
            .read("cp-legacy")
            .expect("legacy checkpoint reads")
            .expect("legacy checkpoint exists");
        assert_eq!(legacy.state.profile, "web");
        assert_eq!(legacy.state.release.as_deref(), Some("r1"));
        assert_eq!(store.list().expect("mixed checkpoints list").len(), 2);

        fs::write(
            paths.checkpoints_dir.join("cp-legacy-mismatch.json"),
            br#"{
                "id":"cp-legacy-mismatch",
                "created_at_unix":8,
                "profile":"web",
                "release":"r1",
                "state":{
                    "lifecycle":"running",
                    "harness":"stopped",
                    "profile":"other",
                    "release":"r1",
                    "updated_at_unix":8
                }
            }"#,
        )
        .expect("mismatched legacy checkpoint writes");
        assert_eq!(
            store
                .read("cp-legacy-mismatch")
                .expect_err("legacy profile mismatch remains rejected")
                .kind(),
            std::io::ErrorKind::InvalidData
        );
        assert!(!root.join(".dsh").exists());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn release_store_registers_promotes_and_rolls_back_atomically() {
        let root = unique_test_root("releases");
        let paths = NexusPaths::from_root(root.clone());
        let store = super::ReleaseStore::new(paths.clone());

        let initial = store.load().expect("release catalog loads");
        assert!(initial.current_release.is_none());
        assert!(initial.releases.is_empty());

        let alpha = store
            .register(
                "harness-alpha5",
                "alpha.5",
                Some("git:alpha5".to_owned()),
                None,
            )
            .expect("alpha registers");
        assert!(alpha.find("harness-alpha5").is_some());
        assert!(paths
            .releases_dir
            .join("harness-alpha5")
            .join("manifest.json")
            .exists());

        let rc = store
            .register("harness-rc1", "rc.1", Some("git:rc1".to_owned()), None)
            .expect("rc registers");
        assert_eq!(rc.releases.len(), 2);
        assert!(store.register("harness-rc1", "rc.1", None, None).is_err());

        let promoted_alpha = store.promote("harness-alpha5").expect("alpha promotes");
        assert_eq!(
            promoted_alpha.current_release.as_deref(),
            Some("harness-alpha5")
        );
        assert!(promoted_alpha.last_known_good.is_none());

        let promoted_rc = store.promote("harness-rc1").expect("rc promotes");
        assert_eq!(promoted_rc.current_release.as_deref(), Some("harness-rc1"));
        assert_eq!(
            promoted_rc.last_known_good.as_deref(),
            Some("harness-alpha5")
        );

        let rolled_back = store.rollback().expect("rollback succeeds");
        assert_eq!(
            rolled_back.current_release.as_deref(),
            Some("harness-alpha5")
        );
        assert_eq!(rolled_back.last_known_good.as_deref(), Some("harness-rc1"));
        assert!(store.rollback().is_ok());
        assert!(store.promote("../escape").is_err());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn release_store_restores_checkpoint_release_as_authoritative_current() {
        let root = unique_test_root("checkpoint-release");
        let paths = NexusPaths::from_root(root.clone());
        let store = super::ReleaseStore::new(paths);
        store
            .register("harness-a", "a", None, None)
            .expect("release A registers");
        store
            .register("harness-b", "b", None, None)
            .expect("release B registers");
        store.promote("harness-a").expect("release A promotes");
        store.promote("harness-b").expect("release B promotes");

        let restored = store
            .restore_checkpoint_release(Some("harness-a"))
            .expect("checkpoint release restores");
        assert_eq!(restored.current_release.as_deref(), Some("harness-a"));
        assert_eq!(restored.last_known_good.as_deref(), Some("harness-b"));
        assert_eq!(
            store
                .load()
                .expect("restored pointers reload")
                .current_release
                .as_deref(),
            Some("harness-a")
        );

        assert!(store
            .restore_checkpoint_release(Some("../unknown"))
            .is_err());
        assert!(store.restore_checkpoint_release(Some("missing")).is_err());
        assert_eq!(
            store
                .load()
                .expect("failed restore keeps pointers")
                .current_release
                .as_deref(),
            Some("harness-a")
        );
        let cleared = store
            .restore_checkpoint_release(None)
            .expect("checkpoint without a release restores");
        assert_eq!(cleared.current_release, None);
        assert_eq!(cleared.last_known_good.as_deref(), Some("harness-a"));
        let rolled_back = store
            .restore_release_pointers(Some("harness-a"), Some("harness-b"))
            .expect("prior pointers restore exactly");
        assert_eq!(rolled_back.current_release.as_deref(), Some("harness-a"));
        assert_eq!(rolled_back.last_known_good.as_deref(), Some("harness-b"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn checkpoint_restore_plan_is_non_mutating_and_journal_is_two_phase() {
        let root = unique_test_root("checkpoint-restore-journal");
        let paths = NexusPaths::from_root(root.clone());
        let releases = ReleaseStore::new(paths.clone());
        releases
            .register("harness-a", "a", None, None)
            .expect("release A registers");
        releases
            .register("harness-b", "b", None, None)
            .expect("release B registers");
        releases.promote("harness-a").expect("release A promotes");
        releases.promote("harness-b").expect("release B promotes");
        let previous_releases = releases.load().expect("previous releases load");
        let target_releases = releases
            .plan_checkpoint_release(Some("harness-a"))
            .expect("restore plan validates");
        assert_eq!(
            releases.load().expect("plan does not write"),
            previous_releases
        );

        let profiles = ProfileStore::new(paths.clone());
        let previous_profiles = profiles.load().expect("previous profiles load");
        let target_profiles = ProfileCatalog::new("restored", previous_profiles.profiles.clone())
            .expect("target profiles validate");
        let intent = CheckpointRestoreIntent {
            checkpoint_id: "checkpoint-a".to_owned(),
            previous_profiles,
            previous_current_release: previous_releases.current_release,
            previous_last_known_good: previous_releases.last_known_good,
            target_profiles,
            target_current_release: target_releases.current_release,
            target_last_known_good: target_releases.last_known_good,
        };
        let journal = CheckpointRestoreJournalStore::new(paths);
        journal.begin(intent.clone()).expect("Prepared writes");
        assert_eq!(
            journal
                .load()
                .expect("Prepared loads")
                .expect("entry")
                .phase,
            CheckpointRestorePhase::Prepared
        );
        assert!(journal.begin(intent.clone()).is_err());
        journal.mark_committed(&intent).expect("Committed writes");
        assert_eq!(
            journal
                .load()
                .expect("Committed loads")
                .expect("entry")
                .phase,
            CheckpointRestorePhase::Committed
        );
        assert!(journal
            .clear(CheckpointRestorePhase::Prepared, &intent)
            .is_err());
        journal
            .clear(CheckpointRestorePhase::Committed, &intent)
            .expect("Committed clears");
        assert!(journal.load().expect("cleared journal loads").is_none());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn update_spec_rejects_embedded_credentials_and_renders_placeholders() {
        let mut spec = super::UpdateSpec {
            source: "https://user:secret@example.invalid/repo".to_owned(),
            ref_name: "main".to_owned(),
            git_program: PathBuf::from("git"),
            build_program: Some(PathBuf::from("builder")),
            build_args: vec!["--source".to_owned(), "{source}".to_owned()],
            verify_program: None,
            verify_args: Vec::new(),
            timeout_secs: Some(10),
        };
        assert!(spec.validate().is_err());
        spec.source = "https://example.invalid/repo".to_owned();
        spec.validate().expect("safe update spec validates");
        assert_eq!(
            spec.render_args(&spec.build_args, Path::new("candidate"), "harness-rc1"),
            vec!["--source", "candidate"]
        );
    }

    #[test]
    fn update_state_store_recovers_stale_running_job() {
        let root = unique_test_root("update-state");
        let paths = NexusPaths::from_root(root.clone());
        let store = super::UpdateStateStore::new(paths.clone());
        let running = nexus_protocol::UpdateRuntimeInfo::running("harness-rc1".to_owned(), 1);
        store.write(&running).expect("update state writes");
        let recovered = store.recover_unattached().expect("stale state recovers");
        assert_eq!(recovered.state, nexus_protocol::UpdateState::Failed);
        assert!(recovered.error.is_some());
        assert_eq!(store.load().expect("state reloads"), recovered);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn release_store_publishes_prepared_candidate_only_below_downloads() {
        let root = unique_test_root("prepared-release");
        let paths = NexusPaths::from_root(root.clone());
        paths.ensure_directories().expect("directories create");
        let candidate = paths.downloads_dir.join("candidate");
        fs::create_dir_all(&candidate).expect("candidate creates");
        fs::write(candidate.join("harness.txt"), "immutable upstream").expect("payload writes");

        let catalog = ReleaseStore::new(paths.clone())
            .register_prepared(
                &candidate,
                "harness-rc1",
                "rc.1",
                Some("file://fixture".to_owned()),
                None,
            )
            .expect("prepared release registers");
        assert!(catalog.find("harness-rc1").is_some());
        assert!(!candidate.exists());
        assert!(paths
            .releases_dir
            .join("harness-rc1")
            .join("harness.txt")
            .exists());
        assert!(paths
            .releases_dir
            .join("harness-rc1")
            .join("manifest.json")
            .exists());

        let outside = root.join("outside");
        fs::create_dir_all(&outside).expect("outside creates");
        assert!(ReleaseStore::new(paths.clone())
            .register_prepared(&outside, "harness-escape", "bad", None, None)
            .is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn release_root_and_launch_context_are_explicit_and_contained() {
        let root = unique_test_root("release-context");
        let paths = NexusPaths::from_root(root.clone());
        let store = ReleaseStore::new(paths.clone());
        store
            .register("harness-rc1", "rc.1", None, None)
            .expect("release registers");
        let release_root = store
            .release_root("harness-rc1")
            .expect("release root resolves");
        assert!(release_root.ends_with(Path::new("releases").join("harness-rc1")));

        let mut spec = HarnessLaunchSpec::new(PathBuf::from("{release_root}/bin/harness"));
        spec.args = vec![
            "--profile".to_owned(),
            "{profile}".to_owned(),
            "--release".to_owned(),
            "{release}".to_owned(),
        ];
        let program = spec
            .render_path_for_context(
                &spec.program,
                "web.dark",
                Some("harness-rc1"),
                Some(&release_root),
            )
            .expect("program renders");
        assert!(program
            .to_string_lossy()
            .contains(&*release_root.to_string_lossy()));
        assert!(program
            .to_string_lossy()
            .replace('/', "\\")
            .ends_with("bin\\harness"));
        assert_eq!(
            spec.render_args_for_context("web.dark", Some("harness-rc1"), Some(&release_root))
                .expect("arguments render"),
            vec!["--profile", "web.dark", "--release", "harness-rc1"]
        );
        assert!(spec.render_args_for_context("web", None, None).is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn diagnostics_collects_bounded_redacted_nexus_files_only() {
        let root = unique_test_root("diagnostics");
        let paths = NexusPaths::from_root(root.clone());
        paths.ensure_directories().expect("directories create");
        fs::write(
            paths.logs_dir.join("harness.stdout.log"),
            "normal failure\nAuthorization: bearer secret-value\n",
        )
        .expect("diagnostic log writes");
        let store = DiagnosticsStore::new(paths.clone());
        let bundle = store
            .collect(Some("after failed start".to_owned()))
            .expect("diagnostics collect");
        assert!(bundle.id.starts_with("diag-"));
        let log = bundle
            .files
            .iter()
            .find(|file| file.name == "logs/harness.stdout.log")
            .expect("harness log is included");
        assert!(log.redacted);
        let log_path = PathBuf::from(&bundle.directory).join("files/logs/harness.stdout.log");
        let copied = fs::read_to_string(log_path).expect("copied diagnostics log reads");
        assert!(copied.contains("[REDACTED]"));
        assert!(!copied.contains("secret-value"));
        assert_eq!(store.list().expect("diagnostics list").len(), 1);
        assert!(!root.join(".dsh").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn config_store_round_trips_nexus_owned_specs_atomically() {
        let root = unique_test_root("config-store");
        let paths = NexusPaths::from_root(root.clone());
        let store = ConfigStore::new(paths.clone());
        assert_eq!(
            store.load().expect("missing config defaults"),
            NexusConfigFile::default()
        );

        let document = NexusConfigFile {
            harness: Some(HarnessLaunchSpec {
                program: PathBuf::from("bin/harness"),
                args: vec!["--profile".to_owned(), "{profile}".to_owned()],
                working_dir: Some(PathBuf::from("runtime")),
                readiness_url: Some("http://127.0.0.1:3080/health".to_owned()),
                readiness_timeout_secs: Some(5),
            }),
            update: Some(UpdateSpec {
                source: "file:///fixtures/harness".to_owned(),
                ref_name: "main".to_owned(),
                git_program: PathBuf::from("git"),
                build_program: None,
                build_args: Vec::new(),
                verify_program: None,
                verify_args: Vec::new(),
                timeout_secs: Some(30),
            }),
        };

        store.write(&document).expect("config writes");
        assert!(paths.config_file.exists());
        assert_eq!(store.load().expect("config reloads"), document);
        assert!(!root.join(".dsh").exists());

        let mut invalid = document.clone();
        invalid.harness.as_mut().expect("harness exists").program = PathBuf::from("bad\nprogram");
        assert!(store.write(&invalid).is_err());
        assert_eq!(store.load().expect("invalid write leaves config"), document);
        let _ = fs::remove_dir_all(root);
    }

    fn unique_test_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "nexus-core-{label}-{}-{}",
            std::process::id(),
            super::unix_time_seconds()
        ))
    }
}

/// The immutable upstream Harness is launched only through this external
/// process specification. Nexus never infers a Harness installation from a
/// home directory or from the current working directory.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HarnessLaunchSpec {
    pub program: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub working_dir: Option<PathBuf>,
    #[serde(default)]
    pub readiness_url: Option<String>,
    #[serde(default)]
    pub readiness_timeout_secs: Option<u64>,
}

impl HarnessLaunchSpec {
    pub fn new(program: PathBuf) -> Self {
        Self {
            program,
            args: Vec::new(),
            working_dir: None,
            readiness_url: None,
            readiness_timeout_secs: None,
        }
    }

    pub fn validate(&self) -> io::Result<()> {
        if self.program.as_os_str().is_empty()
            || self.program.to_string_lossy().chars().any(char::is_control)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Harness program must be non-empty and contain no control characters",
            ));
        }
        if let Some(working_dir) = &self.working_dir {
            if working_dir.to_string_lossy().chars().any(char::is_control) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Harness working_dir contains a control character",
                ));
            }
        }
        validate_launch_args(&self.args, "Harness arguments")?;
        if let Some(readiness_url) = &self.readiness_url {
            if readiness_url.is_empty()
                || readiness_url.len() > MAX_UPDATE_SOURCE_LEN
                || readiness_url.chars().any(char::is_control)
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Harness readiness_url is empty, too long, or contains a control character",
                ));
            }
        }
        if let Some(timeout) = self.readiness_timeout_secs {
            if timeout == 0 || timeout > 86_400 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Harness readiness timeout must be between 1 and 86400 seconds",
                ));
            }
        }
        Ok(())
    }

    pub fn to_payload(&self) -> HarnessConfigPayload {
        HarnessConfigPayload {
            program: self.program.to_string_lossy().into_owned(),
            args: self.args.clone(),
            working_dir: self
                .working_dir
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned()),
            readiness_url: self.readiness_url.clone(),
            readiness_timeout_secs: self.readiness_timeout_secs,
        }
    }

    pub fn from_payload(payload: HarnessConfigPayload) -> io::Result<Self> {
        let spec = Self {
            program: PathBuf::from(payload.program),
            args: payload.args,
            working_dir: payload.working_dir.map(PathBuf::from),
            readiness_url: payload.readiness_url,
            readiness_timeout_secs: payload.readiness_timeout_secs,
        };
        spec.validate()?;
        Ok(spec)
    }

    /// Render the explicitly configured profile placeholder without inferring
    /// or injecting any Harness-specific flags or environment variables.
    pub fn render_args_for_profile(&self, profile: &str) -> Vec<String> {
        self.args
            .iter()
            .map(|argument| argument.replace("{profile}", profile))
            .collect()
    }

    /// Render only placeholders explicitly supplied by Nexus. A release-aware
    /// launch is opt-in: static Harness commands continue to work when no
    /// release has been selected, while `{release}`/`{release_root}` fail
    /// clearly instead of silently launching the wrong tree.
    pub fn render_args_for_context(
        &self,
        profile: &str,
        release_id: Option<&str>,
        release_root: Option<&Path>,
    ) -> io::Result<Vec<String>> {
        self.args
            .iter()
            .map(|argument| render_launch_text(argument, profile, release_id, release_root))
            .collect()
    }

    pub fn render_path_for_context(
        &self,
        path: &Path,
        profile: &str,
        release_id: Option<&str>,
        release_root: Option<&Path>,
    ) -> io::Result<PathBuf> {
        Ok(PathBuf::from(render_launch_text(
            &path.to_string_lossy(),
            profile,
            release_id,
            release_root,
        )?))
    }
}

fn validate_launch_args(args: &[String], label: &str) -> io::Result<()> {
    if args.len() > 128 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{label} contain too many entries"),
        ));
    }
    if args.iter().any(|argument| {
        argument.len() > MAX_UPDATE_TEXT_LEN || argument.chars().any(char::is_control)
    }) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{label} contain an invalid entry"),
        ));
    }
    Ok(())
}

fn render_launch_text(
    value: &str,
    profile: &str,
    release_id: Option<&str>,
    release_root: Option<&Path>,
) -> io::Result<String> {
    let mut rendered = value.replace("{profile}", profile);
    if rendered.contains("{release}") {
        let Some(release_id) = release_id else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Harness launch uses {release} but no current release is selected",
            ));
        };
        rendered = rendered.replace("{release}", release_id);
    }
    if rendered.contains("{release_root}") {
        let Some(release_root) = release_root else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Harness launch uses {release_root} but no current release is selected",
            ));
        };
        rendered = rendered.replace("{release_root}", &release_root.to_string_lossy());
    }
    Ok(rendered)
}

/// Monotonic-enough timestamp helper for naming disposable update
/// candidates. It is not used as a security token or a release version.
pub fn unix_time_nanos_for_update() -> u128 {
    unix_time_nanos()
}

/// External update commands are configured by Nexus and run in a disposable
/// candidate directory. No command is inferred from the Harness source tree.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpdateSpec {
    pub source: String,
    #[serde(default = "default_update_ref")]
    pub ref_name: String,
    #[serde(default = "default_git_program")]
    pub git_program: PathBuf,
    #[serde(default)]
    pub build_program: Option<PathBuf>,
    #[serde(default)]
    pub build_args: Vec<String>,
    #[serde(default)]
    pub verify_program: Option<PathBuf>,
    #[serde(default)]
    pub verify_args: Vec<String>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

impl UpdateSpec {
    pub fn validate(&self) -> io::Result<()> {
        validate_update_source(&self.source)?;
        validate_update_ref(&self.ref_name)?;
        validate_update_program(&self.git_program, "git program")?;
        validate_update_optional_program(self.build_program.as_deref(), "build program")?;
        validate_update_optional_program(self.verify_program.as_deref(), "verify program")?;
        validate_update_args(&self.build_args, "build arguments")?;
        validate_update_args(&self.verify_args, "verify arguments")?;
        if let Some(timeout) = self.timeout_secs {
            if timeout == 0 || timeout > 86_400 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "update timeout must be between 1 and 86400 seconds",
                ));
            }
        }
        Ok(())
    }

    pub fn timeout(&self) -> Duration {
        Duration::from_secs(self.timeout_secs.unwrap_or(DEFAULT_UPDATE_TIMEOUT_SECS))
    }

    pub fn render_args(&self, args: &[String], source_dir: &Path, release_id: &str) -> Vec<String> {
        let source = source_dir.to_string_lossy();
        args.iter()
            .map(|argument| {
                argument
                    .replace("{source}", &source)
                    .replace("{release}", release_id)
                    .replace("{ref}", &self.ref_name)
            })
            .collect()
    }

    pub fn to_payload(&self) -> UpdateConfigPayload {
        UpdateConfigPayload {
            source: self.source.clone(),
            ref_name: self.ref_name.clone(),
            git_program: self.git_program.to_string_lossy().into_owned(),
            build_program: self
                .build_program
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned()),
            build_args: self.build_args.clone(),
            verify_program: self
                .verify_program
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned()),
            verify_args: self.verify_args.clone(),
            timeout_secs: self.timeout_secs,
        }
    }

    pub fn from_payload(payload: UpdateConfigPayload) -> io::Result<Self> {
        let spec = Self {
            source: payload.source,
            ref_name: payload.ref_name,
            git_program: PathBuf::from(payload.git_program),
            build_program: payload.build_program.map(PathBuf::from),
            build_args: payload.build_args,
            verify_program: payload.verify_program.map(PathBuf::from),
            verify_args: payload.verify_args,
            timeout_secs: payload.timeout_secs,
        };
        spec.validate()?;
        Ok(spec)
    }
}

fn default_update_ref() -> String {
    "main".to_owned()
}

fn default_git_program() -> PathBuf {
    PathBuf::from("git")
}

fn validate_update_source(source: &str) -> io::Result<()> {
    if source.is_empty() || source.len() > MAX_UPDATE_SOURCE_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("update source must be 1-{MAX_UPDATE_SOURCE_LEN} bytes"),
        ));
    }
    if source.chars().any(|character| character.is_control()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "update source contains a control character",
        ));
    }
    // Do not persist or execute URLs with embedded credentials. A future
    // authenticated design must supply credentials through a separate secret
    // provider rather than a git remote string.
    if let Some((_, authority_and_path)) = source.split_once("://") {
        let authority = authority_and_path
            .split(['/', '?', '#'])
            .next()
            .unwrap_or_default();
        if authority.contains('@') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "update source must not contain embedded credentials",
            ));
        }
    }
    Ok(())
}

fn validate_update_ref(ref_name: &str) -> io::Result<()> {
    if ref_name.is_empty() || ref_name.len() > MAX_UPDATE_REF_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("update ref must be 1-{MAX_UPDATE_REF_LEN} bytes"),
        ));
    }
    if ref_name
        .chars()
        .any(|character| character.is_control() || character.is_whitespace())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "update ref must not contain whitespace or control characters",
        ));
    }
    Ok(())
}

fn validate_update_program(program: &Path, label: &str) -> io::Result<()> {
    if program.as_os_str().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{label} cannot be empty"),
        ));
    }
    if program
        .to_string_lossy()
        .chars()
        .any(|character| character.is_control())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{label} contains a control character"),
        ));
    }
    Ok(())
}

fn validate_update_optional_program(program: Option<&Path>, label: &str) -> io::Result<()> {
    if let Some(program) = program {
        validate_update_program(program, label)?;
    }
    Ok(())
}

fn validate_update_args(args: &[String], label: &str) -> io::Result<()> {
    if args.len() > 128 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{label} contain too many entries"),
        ));
    }
    for argument in args {
        if argument.len() > MAX_UPDATE_TEXT_LEN
            || argument.chars().any(|character| character.is_control())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{label} contain an invalid entry"),
            ));
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct NexusConfigFile {
    #[serde(default)]
    pub harness: Option<HarnessLaunchSpec>,
    #[serde(default)]
    pub update: Option<UpdateSpec>,
}

/// Nexus-owned configuration writer. It owns only `config.json`; Harness
/// source, working directories, and `$HOME/.dsh` are never modified here.
#[derive(Clone)]
pub struct ConfigStore {
    paths: NexusPaths,
    write_gate: Arc<Mutex<()>>,
}

impl ConfigStore {
    pub fn new(paths: NexusPaths) -> Self {
        Self {
            paths,
            write_gate: Arc::new(Mutex::new(())),
        }
    }

    pub fn paths(&self) -> &NexusPaths {
        &self.paths
    }

    pub fn load(&self) -> io::Result<NexusConfigFile> {
        let _guard = self.lock_gate()?;
        if !self.paths.config_file.exists() {
            return Ok(NexusConfigFile::default());
        }
        let bytes = fs::read(&self.paths.config_file)?;
        let document: NexusConfigFile = decode_json(&bytes).map_err(invalid_data)?;
        validate_config_document(&document)?;
        Ok(document)
    }

    pub fn write(&self, document: &NexusConfigFile) -> io::Result<()> {
        validate_config_document(document)?;
        let _guard = self.lock_gate()?;
        self.paths.ensure_directories()?;
        write_json_atomic(&self.paths.root, &self.paths.config_file, document)
    }

    fn lock_gate(&self) -> io::Result<std::sync::MutexGuard<'_, ()>> {
        self.write_gate
            .lock()
            .map_err(|_| io::Error::other("config lock is poisoned"))
    }
}

fn validate_config_document(document: &NexusConfigFile) -> io::Result<()> {
    if let Some(harness) = &document.harness {
        harness.validate()?;
    }
    if let Some(update) = &document.update {
        update.validate()?;
    }
    Ok(())
}

/// Load the optional external Harness command from Nexus-owned configuration
/// and then apply explicit environment overrides. A missing program is a
/// valid, intentional control-plane-only configuration.
pub fn load_harness_launch_spec(paths: &NexusPaths) -> io::Result<Option<HarnessLaunchSpec>> {
    let mut spec = if paths.config_file.exists() {
        let bytes = fs::read(&paths.config_file)?;
        let document: NexusConfigFile = decode_json(&bytes).map_err(invalid_data)?;
        match document.harness {
            Some(spec) => Some(spec),
            None => decode_json::<HarnessLaunchSpec>(&bytes).ok(),
        }
    } else {
        None
    };

    if let Some(program) = non_empty_env(HARNESS_PROGRAM_ENV) {
        let mut configured = spec
            .take()
            .unwrap_or_else(|| HarnessLaunchSpec::new(PathBuf::from(&program)));
        configured.program = PathBuf::from(program);
        spec = Some(configured);
    }

    let Some(mut configured) = spec else {
        return Ok(None);
    };

    if let Some(args) = non_empty_env(HARNESS_ARGS_ENV) {
        configured.args = parse_args_override(&args);
    }
    if let Some(working_dir) = non_empty_env(HARNESS_WORKING_DIR_ENV) {
        configured.working_dir = Some(PathBuf::from(working_dir));
    }
    if let Some(readiness_url) = non_empty_env(HARNESS_READINESS_URL_ENV) {
        configured.readiness_url = Some(readiness_url);
    }
    if let Some(timeout) = non_empty_env(HARNESS_READINESS_TIMEOUT_ENV)
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|timeout| *timeout > 0)
    {
        configured.readiness_timeout_secs = Some(timeout);
    }

    configured.validate()?;
    Ok(Some(configured))
}

/// Load the optional external update plan from Nexus-owned configuration and
/// apply explicit environment overrides. A missing plan is intentional: the
/// Agent can still supervise an already configured Harness without updates.
pub fn load_update_spec(paths: &NexusPaths) -> io::Result<Option<UpdateSpec>> {
    let mut spec = if paths.config_file.exists() {
        let bytes = fs::read(&paths.config_file)?;
        let document: NexusConfigFile = decode_json(&bytes).map_err(invalid_data)?;
        match document.update {
            Some(spec) => Some(spec),
            None => decode_json::<UpdateSpec>(&bytes).ok(),
        }
    } else {
        None
    };

    if let Some(source) = non_empty_env(UPDATE_SOURCE_ENV) {
        let mut configured = spec.take().unwrap_or_else(|| UpdateSpec {
            source: source.clone(),
            ref_name: default_update_ref(),
            git_program: default_git_program(),
            build_program: None,
            build_args: Vec::new(),
            verify_program: None,
            verify_args: Vec::new(),
            timeout_secs: None,
        });
        configured.source = source;
        spec = Some(configured);
    }

    let Some(mut configured) = spec else {
        return Ok(None);
    };
    if let Some(ref_name) = non_empty_env(UPDATE_REF_ENV) {
        configured.ref_name = ref_name;
    }
    if let Some(program) = non_empty_env(UPDATE_GIT_PROGRAM_ENV) {
        configured.git_program = PathBuf::from(program);
    }
    if let Some(program) = non_empty_env(UPDATE_BUILD_PROGRAM_ENV) {
        configured.build_program = Some(PathBuf::from(program));
    }
    if let Some(args) = non_empty_env(UPDATE_BUILD_ARGS_ENV) {
        configured.build_args = parse_args_override(&args);
    }
    if let Some(program) = non_empty_env(UPDATE_VERIFY_PROGRAM_ENV) {
        configured.verify_program = Some(PathBuf::from(program));
    }
    if let Some(args) = non_empty_env(UPDATE_VERIFY_ARGS_ENV) {
        configured.verify_args = parse_args_override(&args);
    }
    if let Some(timeout) = non_empty_env(UPDATE_TIMEOUT_ENV) {
        configured.timeout_secs = Some(timeout.parse::<u64>().map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "NEXUS_UPDATE_TIMEOUT_SECS must be an integer",
            )
        })?);
    }
    configured.validate()?;
    Ok(Some(configured))
}

fn invalid_data(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

fn non_empty_env(name: &str) -> Option<String> {
    env::var(name).ok().filter(|value| !value.trim().is_empty())
}

fn parse_args_override(value: &str) -> Vec<String> {
    decode_json::<Vec<String>>(value.as_bytes())
        .unwrap_or_else(|_| value.split_whitespace().map(str::to_owned).collect())
}
