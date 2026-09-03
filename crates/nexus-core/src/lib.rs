//! UI-independent configuration, path, and state primitives for Nexus.

use std::{
    env, fs, io,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use nexus_protocol::{
    decode_json, encode_json, AgentLifecycleState, AgentStatePayload, CheckpointManifest,
    HarnessRuntimeInfo, HarnessState, NexusStateSummary, ReleaseManifest,
};
use serde::{Deserialize, Serialize};

pub const DEFAULT_AGENT_PORT: u16 = 3090;
pub const DEFAULT_PROFILE: &str = "web";
pub const MAX_PROFILE_NAME_LEN: usize = 64;
pub const PROFILE_SCHEMA_VERSION: u32 = 1;
pub const CHECKPOINT_SCHEMA_VERSION: u32 = 1;
pub const RELEASE_SCHEMA_VERSION: u32 = 1;
pub const MAX_RELEASE_ID_LEN: usize = 128;
pub const MAX_RELEASE_VERSION_LEN: usize = 128;
pub const MAX_RELEASE_TEXT_LEN: usize = 4096;
pub const DATA_DIR_ENV: &str = "NEXUS_DATA_DIR";
pub const PORT_ENV: &str = "NEXUS_AGENT_PORT";
pub const HARNESS_PROGRAM_ENV: &str = "NEXUS_HARNESS_PROGRAM";
pub const HARNESS_ARGS_ENV: &str = "NEXUS_HARNESS_ARGS";
pub const HARNESS_WORKING_DIR_ENV: &str = "NEXUS_HARNESS_WORKING_DIR";
pub const HARNESS_READINESS_URL_ENV: &str = "NEXUS_HARNESS_READINESS_URL";
pub const HARNESS_READINESS_TIMEOUT_ENV: &str = "NEXUS_HARNESS_READINESS_TIMEOUT_SECS";
pub const RUNTIME_SCHEMA_VERSION: u32 = 1;

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
    pub logs_dir: PathBuf,
    pub checkpoints_dir: PathBuf,
    pub releases_dir: PathBuf,
    pub downloads_dir: PathBuf,
    pub run_dir: PathBuf,
}

impl NexusPaths {
    pub fn from_root(root: PathBuf) -> Self {
        Self {
            config_file: root.join("config.json"),
            state_file: root.join("state.json"),
            profiles_file: root.join("profiles.json"),
            release_pointers_file: root.join("release-pointers.json"),
            logs_dir: root.join("logs"),
            checkpoints_dir: root.join("checkpoints"),
            releases_dir: root.join("releases"),
            downloads_dir: root.join("downloads"),
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

/// Nexus-only state captured in a checkpoint manifest.
pub type NexusStateSnapshot = NexusStateSummary;

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
        if state.profile != profile {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "checkpoint state profile does not match manifest profile",
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

fn validate_checkpoint_manifest(manifest: &CheckpointManifest) -> io::Result<()> {
    validate_checkpoint_id(&manifest.id)?;
    validate_profile_name(&manifest.profile)?;
    if manifest.state.profile != manifest.profile {
        return Err(invalid_data(
            "checkpoint state profile does not match profile",
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
        atomic_replace(&temp_path, destination)
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
    let bytes = encode_json(metadata).map_err(invalid_data)?;
    let temp_path = paths.root.join(format!(
        ".state.json.tmp-{}-{}",
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
        atomic_replace(&temp_path, &paths.state_file)
    })();

    if write_result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    write_result
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
    use std::{fs, path::PathBuf};

    use nexus_protocol::{AgentLifecycleState, HarnessRuntimeInfo, HarnessState};

    use super::{
        is_within, load_harness_launch_spec, read_runtime_metadata, write_runtime_metadata,
        AgentState, CheckpointStore, NexusConfig, NexusPaths, NexusRuntimeMetadata, ProfileStore,
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
            lifecycle: AgentLifecycleState::Stopped,
            harness: HarnessState::Stopped,
            profile: "web".to_owned(),
            release: Some("r1".to_owned()),
            updated_at_unix: 10,
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
        assert_eq!(
            store
                .read(&created.id)
                .expect("checkpoint reads")
                .expect("checkpoint exists"),
            created
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

    /// Render the explicitly configured profile placeholder without inferring
    /// or injecting any Harness-specific flags or environment variables.
    pub fn render_args_for_profile(&self, profile: &str) -> Vec<String> {
        self.args
            .iter()
            .map(|argument| argument.replace("{profile}", profile))
            .collect()
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct NexusConfigFile {
    #[serde(default)]
    pub harness: Option<HarnessLaunchSpec>,
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
