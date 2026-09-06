//! UI-independent configuration, path, and state primitives for Nexus.

use std::{
    collections::HashSet,
    env,
    ffi::{OsStr, OsString},
    fs, io,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use nexus_protocol::{
    decode_json, encode_json, AgentLifecycleState, AgentStatePayload, CheckpointManifest,
    DiagnosticsBundle, DiagnosticsFile, HarnessCandidate, HarnessCheckpointState,
    HarnessConfigPayload, HarnessDiscoveryResponse, HarnessLaunchMode, HarnessRuntimeInfo,
    HarnessState, ReleaseManifest, RuntimeConfigPayload, RuntimeInstallMode, RuntimeOwnership,
    RuntimePinPayload, RuntimeSource, SnapshotReference, SnapshotsConfigPayload,
    UpdateConfigPayload, UpdateRuntimeInfo, UpdateState,
};
use nexus_snapshots::RestoreTicket;
use serde::{Deserialize, Serialize};

pub mod runtime_requirements;

pub const DEFAULT_AGENT_PORT: u16 = 3090;
pub const DEFAULT_PROFILE: &str = "web";
pub const MAX_PROFILE_NAME_LEN: usize = 64;
pub const PROFILE_SCHEMA_VERSION: u32 = 1;
pub const CHECKPOINT_SCHEMA_VERSION: u32 = 1;
pub const CHECKPOINT_RESTORE_SCHEMA_VERSION: u32 = 1;
pub const DEFAULT_HEALTHY_SNAPSHOT_SLOTS: usize = 3;
pub const DEFAULT_MAX_MANUAL_SNAPSHOTS: usize = 64;
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

/// Discovery record published by a running Agent so launchers and CLIs can
/// find it without assuming a fixed port. The record binds the port to the
/// data-root identity and instance id, so a stale record for a different
/// Agent generation is rejected by the normal identity checks.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentDiscoveryRecord {
    pub port: u16,
    pub instance_id: String,
    pub data_root_id: String,
    pub pid: u32,
    pub updated_at_unix: u64,
}

impl NexusPaths {
    pub fn publish_agent_discovery(&self, record: &AgentDiscoveryRecord) -> io::Result<()> {
        self.ensure_directories()?;
        write_json_atomic(
            &self.run_dir,
            &self.agent_discovery_file(),
            record,
        )
    }

    pub fn read_agent_discovery(&self) -> io::Result<Option<AgentDiscoveryRecord>> {
        let path = self.agent_discovery_file();
        if !path.exists() {
            return Ok(None);
        }
        let bytes = fs::read(&path)?;
        Ok(Some(decode_json::<AgentDiscoveryRecord>(&bytes)?))
    }
}

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
    pub runtimes_dir: PathBuf,
    pub downloads_dir: PathBuf,
    pub diagnostics_dir: PathBuf,
    pub run_dir: PathBuf,
}

impl NexusPaths {
    pub fn agent_discovery_file(&self) -> PathBuf {
        self.run_dir.join("agent.json")
    }
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
            runtimes_dir: root.join("runtimes"),
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
            &self.runtimes_dir,
            &self.downloads_dir,
            &self.diagnostics_dir,
            &self.run_dir,
        ] {
            fs::create_dir_all(directory)?;
        }

        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimePin {
    pub path: PathBuf,
    pub ownership: RuntimeOwnership,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimeConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<RuntimePin>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pnpm: Option<RuntimePin>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git: Option<RuntimePin>,
    #[serde(default)]
    pub source: RuntimeSource,
    #[serde(default)]
    pub mode: RuntimeInstallMode,
}

impl RuntimeConfig {
    pub fn validate(&self) -> io::Result<()> {
        for (name, pin) in [
            ("node", self.node.as_ref()),
            ("pnpm", self.pnpm.as_ref()),
            ("git", self.git.as_ref()),
        ] {
            if let Some(pin) = pin {
                if !pin.path.is_absolute()
                    || pin.path.as_os_str().is_empty()
                    || pin.path.to_string_lossy().chars().any(char::is_control)
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("runtime.{name}.path must be an absolute path without control characters"),
                    ));
                }
            }
        }
        Ok(())
    }

    pub fn validate_for_paths(&self, paths: &NexusPaths) -> io::Result<()> {
        self.validate()?;
        for (name, pin) in [
            ("node", self.node.as_ref()),
            ("pnpm", self.pnpm.as_ref()),
            ("git", self.git.as_ref()),
        ] {
            if let Some(pin) = pin {
                if pin.path.components().any(|component| {
                    matches!(
                        component,
                        std::path::Component::CurDir | std::path::Component::ParentDir
                    )
                }) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("runtime.{name}.path cannot contain '.' or '..' components"),
                    ));
                }
                if pin.ownership == RuntimeOwnership::Nexus
                    && !lexically_within(&paths.runtimes_dir, &pin.path)
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("runtime.{name} owned by Nexus must be below runtimes_dir"),
                    ));
                }
            }
        }
        Ok(())
    }

    pub fn pin(&self, name: &str) -> Option<&RuntimePin> {
        match name {
            "node" => self.node.as_ref(),
            "pnpm" => self.pnpm.as_ref(),
            "git" => self.git.as_ref(),
            _ => None,
        }
    }

    pub fn to_payload(&self) -> RuntimeConfigPayload {
        RuntimeConfigPayload {
            node: self.node.as_ref().map(RuntimePin::to_payload),
            pnpm: self.pnpm.as_ref().map(RuntimePin::to_payload),
            git: self.git.as_ref().map(RuntimePin::to_payload),
            source: self.source,
            mode: self.mode,
        }
    }

    pub fn from_payload(payload: RuntimeConfigPayload) -> io::Result<Self> {
        let config = Self {
            node: payload.node.map(RuntimePin::from_payload),
            pnpm: payload.pnpm.map(RuntimePin::from_payload),
            git: payload.git.map(RuntimePin::from_payload),
            source: payload.source,
            mode: payload.mode,
        };
        config.validate()?;
        Ok(config)
    }
}

fn lexically_within(root: &Path, path: &Path) -> bool {
    #[cfg(windows)]
    {
        let root = root
            .to_string_lossy()
            .trim_end_matches(['\\', '/'])
            .to_ascii_lowercase();
        let path = path.to_string_lossy().to_ascii_lowercase();
        path == root
            || path
                .strip_prefix(&root)
                .is_some_and(|rest| rest.starts_with(['\\', '/']))
    }
    #[cfg(not(windows))]
    {
        path == root || path.starts_with(root)
    }
}

impl RuntimePin {
    fn to_payload(&self) -> RuntimePinPayload {
        RuntimePinPayload {
            path: self.path.to_string_lossy().into_owned(),
            ownership: self.ownership,
        }
    }

    fn from_payload(payload: RuntimePinPayload) -> Self {
        Self {
            path: PathBuf::from(payload.path),
            ownership: payload.ownership,
        }
    }
}

/// Build the PATH value for a Nexus child without changing process or system
/// environment. Consumers execute pinned programs directly and use this PATH
/// only for their child processes and transitive executable lookup.
pub fn build_runtime_child_env(
    config: &RuntimeConfig,
    ambient_path: Option<&OsStr>,
) -> io::Result<Vec<(OsString, OsString)>> {
    config.validate()?;
    let mut directories = Vec::<PathBuf>::new();
    for pin in [&config.node, &config.pnpm, &config.git]
        .into_iter()
        .flatten()
    {
        let directory = pin.path.parent().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "runtime pin has no parent directory",
            )
        })?;
        if !directories.iter().any(|existing| existing == directory) {
            directories.push(directory.to_owned());
        }
    }
    if let Some(ambient_path) = ambient_path {
        for directory in env::split_paths(ambient_path) {
            if !directories.iter().any(|existing| existing == &directory) {
                directories.push(directory);
            }
        }
    }
    if directories.is_empty() {
        return Ok(Vec::new());
    }
    let path = env::join_paths(directories).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("runtime child PATH cannot be encoded: {error}"),
        )
    })?;
    Ok(vec![(OsString::from("PATH"), path)])
}

/// Process description derived from the single configured runtime pin set.
/// A pinned pnpm JavaScript entry is always launched through the pinned Node,
/// so downstream consumers never duplicate platform-specific command logic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeCommandSpec {
    pub program: PathBuf,
    pub prefix_args: Vec<OsString>,
}

pub fn resolve_runtime_command(
    config: &RuntimeConfig,
    name: &str,
) -> io::Result<Option<RuntimeCommandSpec>> {
    config.validate()?;
    let Some(pin) = config.pin(name) else {
        return if matches!(name, "node" | "pnpm" | "git") {
            Ok(None)
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unknown runtime tool: {name}"),
            ))
        };
    };
    if name == "pnpm"
        && pin
            .path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| {
                matches!(
                    extension.to_ascii_lowercase().as_str(),
                    "js" | "cjs" | "mjs"
                )
            })
    {
        let node = config.node.as_ref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "a pnpm JavaScript entry requires a pinned Node runtime",
            )
        })?;
        return Ok(Some(RuntimeCommandSpec {
            program: node.path.clone(),
            // Node's script loader treats a Win32 verbatim path as a literal
            // path segment and attempts to lstat `C:`. Keep the configured
            // path unchanged for identity and containment, but pass the
            // ordinary drive/UNC spelling at this process boundary.
            prefix_args: vec![node_script_argument(&pin.path)],
        }));
    }
    Ok(Some(RuntimeCommandSpec {
        program: pin.path.clone(),
        prefix_args: Vec::new(),
    }))
}

/// Convert an already validated script path at the Node process boundary.
/// The stored/canonical path remains unchanged for containment and identity.
pub fn node_script_argument(path: &Path) -> OsString {
    normalize_discovery_path(path).into_os_string()
}

pub const PNPM_MINIMUM_RELEASE_AGE_ARG: &str = "--config.minimumReleaseAge=0";
pub const PNPM_OFFICIAL_REGISTRY_ARG: &str = "--config.registry=https://registry.npmjs.org";
pub const PNPM_NPMMIRROR_REGISTRY_ARG: &str = "--config.registry=https://registry.npmmirror.com";

/// Add the shared process-local pnpm policy without writing user pnpm config.
pub fn build_pnpm_args(
    config: &RuntimeConfig,
    args: impl IntoIterator<Item = OsString>,
) -> Vec<OsString> {
    let registry = match config.source {
        RuntimeSource::Official => PNPM_OFFICIAL_REGISTRY_ARG,
        RuntimeSource::Npmmirror => PNPM_NPMMIRROR_REGISTRY_ARG,
    };
    let mut result = vec![
        OsString::from(PNPM_MINIMUM_RELEASE_AGE_ARG),
        OsString::from(registry),
    ];
    result.extend(args);
    result
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

    /// Create a new profile: add the name to the Nexus catalog and
    /// initialize the Harness profile directory from the shipped `web`
    /// template (manifest with the two base bundles, an empty user patch
    /// layer, and the pnpm settings out-of-tree plugins need). An existing
    /// catalog name or on-disk directory fails closed. Metadata only; it
    /// never starts or restarts Harness.
    pub fn create(&self, name: &str, dsh_home: &Path) -> io::Result<ProfileCatalog> {
        validate_profile_name(name)?;
        let _guard = self.lock_gate()?;
        let mut catalog = self.load_unlocked()?;
        if catalog.profiles.iter().any(|item| item == name) || catalog.active_profile == name {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("profile {name} already exists"),
            ));
        }
        let profile_dir = dsh_home.join("profiles").join(name);
        if profile_dir.exists() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "profile directory already exists on disk: {}",
                    profile_dir.display()
                ),
            ));
        }
        let staging = dsh_home.join("profiles").join(format!(
            ".{name}.creating-{}",
            unix_time_nanos()
        ));
        fs::create_dir_all(&staging)?;
        let publish = |result: io::Result<()>| -> io::Result<()> {
            if let Err(error) = result {
                let _ = fs::remove_dir_all(&staging);
                return Err(error);
            }
            Ok(())
        };
        let manifest = serde_json::json!({
            "name": format!("dsh-profile-{name}"),
            "private": true,
            "dependencies": {},
            "dsh": {
                "profile": {
                    "bundles": ["@deepseek-ai/dsh-base", "@deepseek-ai/dsh-web-app"],
                    "patchReload": "live",
                },
            },
        });
        publish(write_json_atomic(
            &staging,
            &staging.join("package.json"),
            &manifest,
        ))?;
        publish(fs::write(
            staging.join("cordis.patch.yml"),
            PROFILE_PATCH_TEMPLATE,
        ))?;
        publish(fs::write(
            staging.join("pnpm-workspace.yaml"),
            PROFILE_PNPM_WORKSPACE,
        ))?;
        fs::rename(&staging, &profile_dir)?;
        catalog.profiles.push(name.to_owned());
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
        self.create_with_snapshot(profile, release, note, state, None)
    }

    pub fn create_with_snapshot(
        &self,
        profile: &str,
        release: Option<String>,
        note: Option<String>,
        state: NexusStateSnapshot,
        snapshot: Option<SnapshotReference>,
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
            snapshot,
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

/// Shipped `web` profile template content (upstream app-boot PROFILE_TEMPLATES).
pub const PROFILE_PATCH_TEMPLATE: &str = r#"# Your patch layer for this dsh profile, applied after every bundle layer:
# a top-level YAML array of loader patch entries (id-targeted config
# overrides, disables, and insert lists; `!!js` expressions allowed).
[]
"#;
pub const PROFILE_PNPM_WORKSPACE: &str = r#"packages:
  - .

nodeLinker: hoisted
autoInstallPeers: false
"#;

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
    /// Present for content restores. The path binding prevents a durable ticket
    /// from being replayed against a different DSH home after Agent restart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<BoundSnapshotRestore>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BoundSnapshotRestore {
    pub ticket: RestoreTicket,
    pub dsh_home: PathBuf,
    pub profile_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CheckpointRestoreJournal {
    pub schema_version: u32,
    pub phase: CheckpointRestorePhase,
    pub intent: CheckpointRestoreIntent,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
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
            error: None,
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
        journal.error = None;
        self.write_unlocked(Some(journal))
    }

    pub fn record_error(
        &self,
        phase: CheckpointRestorePhase,
        intent: &CheckpointRestoreIntent,
        error: impl Into<String>,
    ) -> io::Result<()> {
        let _guard = self.lock_gate()?;
        let Some(mut journal) = self.load_unlocked()? else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "checkpoint restore journal is missing",
            ));
        };
        if journal.phase != phase || journal.intent != *intent {
            return Err(invalid_data(
                "checkpoint restore journal changed before diagnostic update",
            ));
        }
        let error = error.into();
        if error.is_empty() || error.len() > 4096 || error.chars().any(char::is_control) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "checkpoint restore diagnostic is empty, too long, or contains control characters",
            ));
        }
        journal.error = Some(error);
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
    if let Some(snapshot) = &intent.snapshot {
        if !snapshot.dsh_home.is_absolute()
            || snapshot.dsh_home.as_os_str().is_empty()
            || snapshot
                .dsh_home
                .to_string_lossy()
                .chars()
                .any(char::is_control)
        {
            return Err(invalid_data(
                "checkpoint snapshot restore must bind an absolute DSH home",
            ));
        }
        validate_profile_name(&snapshot.profile_name)?;
        if snapshot.profile_name != snapshot.ticket.profile_name
            || snapshot.profile_name != intent.target_profiles.active_profile
        {
            return Err(invalid_data(
                "checkpoint snapshot restore profile binding is inconsistent",
            ));
        }
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
    if let Some(snapshot) = &manifest.snapshot {
        if snapshot.snapshot_id != snapshot.summary.snapshot_id
            || snapshot.summary.profile_name != manifest.profile
        {
            return Err(invalid_data(
                "checkpoint snapshot reference does not match its manifest",
            ));
        }
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
/// Release slots are bounded so an unbounded stream of installed upstream
/// tags can never fill the data root silently. The user removes slots
/// explicitly; Nexus never auto-deletes one.
pub const DEFAULT_MAX_RELEASE_SLOTS: usize = 3;

#[derive(Clone)]
pub struct ReleaseStore {
    paths: NexusPaths,
    write_gate: Arc<Mutex<()>>,
    max_slots: usize,
}

impl ReleaseStore {
    pub fn new(paths: NexusPaths) -> Self {
        Self {
            paths,
            write_gate: Arc::new(Mutex::new(())),
            max_slots: DEFAULT_MAX_RELEASE_SLOTS,
        }
    }

    pub fn with_max_slots(mut self, max_slots: usize) -> Self {
        self.max_slots = max_slots.max(1);
        self
    }

    fn ensure_slot_capacity(&self, catalog: &ReleaseCatalog) -> io::Result<()> {
        if catalog.releases.len() >= self.max_slots {
            let ids: Vec<&str> = catalog
                .releases
                .iter()
                .map(|item| item.id.as_str())
                .collect();
            return Err(io::Error::new(
                io::ErrorKind::ResourceBusy,
                format!(
                    "release slots are full ({}/{}); remove a slot first: {}",
                    catalog.releases.len(),
                    self.max_slots,
                    ids.join(", ")
                ),
            ));
        }
        Ok(())
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
        // canonicalize returns the `\\?\C:\...` verbatim form on Windows;
        // strip it because Node and other launch consumers mis-resolve
        // verbatim paths mixed with forward-slash placeholders.
        let text = slot.as_os_str().to_string_lossy();
        let stripped = if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
            format!(r"\\{}", rest)
        } else if let Some(rest) = text.strip_prefix(r"\\?\") {
            rest.to_owned()
        } else {
            text.into_owned()
        };
        Ok(PathBuf::from(stripped))
    }

    pub fn load(&self) -> io::Result<ReleaseCatalog> {
        let _guard = self.lock_gate()?;
        self.load_unlocked()
    }

    /// Refuse an expensive acquisition before it starts when no immutable
    /// release slot can be published. The definitive check is repeated by
    /// `register_prepared` under the store write gate.
    pub fn ensure_capacity_for_new(&self) -> io::Result<()> {
        let _guard = self.lock_gate()?;
        let catalog = self.load_unlocked()?;
        self.ensure_slot_capacity(&catalog)
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
        self.ensure_slot_capacity(&catalog)?;
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

        self.ensure_slot_capacity(&catalog)?;
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
            let previous = catalog.current_release.clone();
            if let Some(previous) = catalog.current_release.replace(id.to_owned()) {
                catalog.last_known_good = Some(previous);
            }
            self.retarget_launch_placeholder(&previous.unwrap_or_default(), Some(id))?;
            self.write_pointers(&catalog)?;
        }
        Ok(catalog)
    }

    /// Remove one non-selected release slot. The current and last-known-good
    /// slots are protected: promoting another slot first is the explicit path.
    /// The manifest-bearing directory is the disk truth, so removing it is the
    /// whole operation; pointers never referenced the removed id here.
    pub fn remove(&self, id: &str) -> io::Result<ReleaseCatalog> {
        validate_release_id(id)?;
        let _guard = self.lock_gate()?;
        let mut catalog = self.load_unlocked()?;
        if catalog.find(id).is_none() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("release {id} was not found"),
            ));
        }
        if catalog.current_release.as_deref() == Some(id) {
            return Err(io::Error::new(
                io::ErrorKind::ResourceBusy,
                format!("release {id} is the current slot; promote another release first"),
            ));
        }
        if catalog.last_known_good.as_deref() == Some(id) {
            return Err(io::Error::new(
                io::ErrorKind::ResourceBusy,
                format!("release {id} is the last-known-good slot; promote another release first"),
            ));
        }
        let slot_dir = self.slot_dir(id)?;
        fs::remove_dir_all(&slot_dir)?;
        catalog.releases.retain(|item| item.id != id);
        catalog.normalize()?;
        Ok(catalog)
    }

/// Keep the profile module link farm (`~/.dsh/profiles/node_modules`)
/// pointing at the running slot's own workspace packages. The Harness boot
/// maintains the same farm itself, but a crashed boot can leave links from a
/// previous slot behind; repointing before spawn makes plugin resolution
/// deterministic for the selected release. Returns repaired links or an error;
/// real directories and indirect farm parents are never replaced.
pub fn heal_module_farm(dsh_home: &Path, slot_root: &Path) -> io::Result<usize> {
    let farm = dsh_home.join("profiles").join("node_modules");
    Self::ensure_module_directory(&dsh_home.join("profiles"))?;
    Self::ensure_module_directory(&farm)?;
    let mut repaired = 0;
    let mut roots: Vec<PathBuf> = Vec::new();
    for first in ["vendor", "packages"] {
        let dir = slot_root.join(first);
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries {
            let path = entry?.path();
            if path.is_dir() {
                roots.push(path);
            }
        }
    }
    // packages/*/* adds a second level; vendor/* is flat.
    if let Ok(entries) = fs::read_dir(slot_root.join("packages")) {
        for entry in entries {
            let Ok(entry) = entry else { continue };
            let scope = entry.path();
            if !scope.is_dir() {
                continue;
            }
            if let Ok(children) = fs::read_dir(&scope) {
                for child in children {
                    let Ok(child) = child else { continue };
                    if child.path().is_dir() {
                        roots.push(child.path());
                    }
                }
            }
        }
    }
    for package_dir in &roots {
        let manifest = package_dir.join("package.json");
        let Ok(bytes) = fs::read(&manifest) else {
            continue;
        };
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            continue;
        };
        let Some(name) = value.get("name").and_then(|item| item.as_str()) else {
            continue;
        };
        Self::validate_module_name(name)?;
        let link = farm.join(name.replace('/', std::path::MAIN_SEPARATOR_STR));
        if let Some(parent) = link.parent() {
            Self::ensure_module_directory(parent)?;
        }
        let canonical_package = fs::canonicalize(package_dir)?;
        if !canonical_package.starts_with(fs::canonicalize(slot_root)?) {
            return Err(io::Error::other("module target escapes release slot"));
        }
        if let Ok(metadata) = fs::symlink_metadata(&link) {
            if !Self::is_module_link(&metadata) {
                return Err(io::Error::other(format!("refusing to replace real module path: {}", link.display())));
            }
        }
        if Self::same_directory(&link, package_dir) {
            continue;
        }
        Self::replace_module_link(&link, &canonical_package)?;
        repaired += 1;
    }
    Ok(repaired)
}

fn same_directory(link: &Path, target: &Path) -> bool {
    match (fs::canonicalize(link), fs::canonicalize(target)) {
        (Ok(actual), Ok(expected)) => actual == expected,
        _ => false,
    }
}

fn validate_module_name(name: &str) -> io::Result<()> {
    let valid_part = |part: &str| {
        !part.is_empty() && part != "." && part != ".."
            && !part.ends_with('.')
            && part.bytes().all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
    };
    let valid = if let Some(scoped) = name.strip_prefix('@') {
        scoped.split_once('/').is_some_and(|(scope, package)| valid_part(scope) && valid_part(package))
    } else {
        valid_part(name)
    };
    if valid { Ok(()) } else { Err(io::Error::new(io::ErrorKind::InvalidData, "invalid module package name")) }
}

fn is_module_link(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    { metadata.file_type().is_symlink() }
}

fn ensure_module_directory(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !Self::is_module_link(&metadata) => Ok(()),
        Ok(_) => Err(io::Error::other(format!("module farm parent is not a real directory: {}", path.display()))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => fs::create_dir(path),
        Err(error) => Err(error),
    }
}

fn replace_module_link(link: &Path, target: &Path) -> io::Result<()> {
    // Prepare first, so a junction-creation failure leaves the old link intact.
    let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
        .map_err(io::Error::other)?.as_nanos();
    let temporary = link.with_file_name(format!(".nexus-module-{}-{nonce}.new", std::process::id()));
    let backup = temporary.with_extension("old");
    Self::create_dir_junction(&temporary, target)?;
    let had_link = fs::symlink_metadata(link).is_ok();
    if had_link {
        if let Err(error) = fs::rename(link, &backup) {
            let _ = Self::remove_module_link(&temporary);
            return Err(error);
        }
    }
    if let Err(error) = fs::rename(&temporary, link) {
        let rollback = if had_link { fs::rename(&backup, link) } else { Ok(()) };
        let _ = Self::remove_module_link(&temporary);
        return Err(io::Error::other(format!("module link replacement failed: {error}; rollback: {rollback:?}")));
    }
    if had_link { Self::remove_module_link(&backup)?; }
    Ok(())
}

fn remove_module_link(path: &Path) -> io::Result<()> {
    #[cfg(windows)]
    { fs::remove_dir(path) }
    #[cfg(not(windows))]
    { fs::remove_file(path) }
}

#[cfg(windows)]
fn create_dir_junction(link: &Path, target: &Path) -> io::Result<()> {
    use std::os::windows::process::CommandExt;
    // cmd builtins interpret forward slashes as switches, unlike Rust paths.
    let link = link.to_string_lossy().replace('/', "\\");
    let target = target.to_string_lossy().replace('/', "\\");
    let output = std::process::Command::new("cmd")
        .creation_flags(0x08000000) // CREATE_NO_WINDOW: no console per package.
        .args([
            "/C",
            "mklink",
            "/J",
            &link,
            &target,
        ])
        .stdin(std::process::Stdio::null())
        .output()?;
    if output.status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "mklink failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}

#[cfg(not(windows))]
fn create_dir_junction(link: &Path, target: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

    /// Rewrite persisted Harness launch paths that still reference the
    /// previously promoted slot into `{release_root}` placeholders, so the
    /// next launch follows the current pointer instead of a stale slot
    /// directory. A cleared target (no release) is left untouched.
    fn retarget_launch_placeholder(
        &self,
        old_current: &str,
        new_current: Option<&str>,
    ) -> io::Result<()> {
        let Some(new_current) = new_current else {
            return Ok(());
        };
        if old_current.is_empty() || old_current == new_current {
            return Ok(());
        }
        let store = ConfigStore::new(self.paths.clone());
        store
            .transaction(|document| {
                let Some(harness) = document.harness.as_mut() else {
                    return Ok(false);
                };
                // Windows paths are case-insensitive and a shorter slot id
                // must never match a longer sibling (`rc1` inside `rc10`), so
                // the search lowercases and requires a non-identifier
                // boundary after the match.
                let needle = format!("releases\\{old_current}");
                let needle_forward = format!("releases/{old_current}");
                let rewrite = |value: &mut String| -> bool {
                    let lowered = value.to_lowercase();
                    for candidate in [&needle, &needle_forward] {
                        let lowered_candidate = candidate.to_lowercase();
                        if let Some(position) = lowered.find(lowered_candidate.as_str()) {
                            let end = position + candidate.len();
                            let rest = value[end..].chars().next();
                            if rest.is_some_and(|character| {
                                character.is_alphanumeric()
                                    || character == '_'
                                    || character == '.'
                            }) {
                                continue;
                            }
                            // Cut everything before the slot segment too: the
                            // parent prefix must not survive, or rendering
                            // would produce a doubled absolute path.
                            *value = format!("{{release_root}}{}", &value[end..]);
                            return true;
                        }
                    }
                    false
                };
                let mut changed = false;
                let mut program = harness.program.to_string_lossy().into_owned();
                changed |= rewrite(&mut program);
                harness.program = program.into();
                for argument in &mut harness.args {
                    changed |= rewrite(argument);
                }
                if let Some(working_dir) = harness.working_dir.as_mut() {
                    let mut text = working_dir.to_string_lossy().into_owned();
                    changed |= rewrite(&mut text);
                    *working_dir = text.into();
                }
                Ok(changed)
            })?;
        Ok(())
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
        let previous_current = catalog.current_release.clone();
        self.apply_checkpoint_release(&mut catalog, release_id)?;
        self.retarget_launch_placeholder(
            previous_current.as_deref().unwrap_or_default(),
            catalog.current_release.as_deref(),
        )?;
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
        let previous_current = catalog.current_release.clone();
        let previous = catalog.current_release.replace(last_known_good.clone());
        catalog.last_known_good = previous;
        self.retarget_launch_placeholder(
            previous_current.as_deref().unwrap_or_default(),
            Some(&last_known_good),
        )?;
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

pub fn write_json_atomic<T: Serialize>(
    root: &Path,
    destination: &Path,
    value: &T,
) -> io::Result<()> {
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
    /// Durable once-per-run latch. It records an attempted healthy snapshot,
    /// including a failed attempt, so an Agent restart does not duplicate it.
    #[serde(default)]
    pub healthy_snapshot_attempted: bool,
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
            healthy_snapshot_attempted: false,
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

    /// Resolve only existing catalogued diagnostics, never a caller-supplied path.
    pub fn open_path(&self, id: &str, file: Option<&str>) -> io::Result<PathBuf> {
        validate_release_id(id)?;
        let bundle = self.list()?.into_iter().find(|bundle| bundle.id == id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "diagnostic bundle not found"))?;
        let directory = self.paths.diagnostics_dir.join(id);
        let path = match file {
            None => directory.clone(),
            Some("diagnostics.json") => directory.join("diagnostics.json"),
            Some(name) => {
                if name.contains(['\\', ':']) || !bundle.files.iter().any(|item| item.name == name) {
                    return Err(invalid_data("file is not in this diagnostic bundle"));
                }
                directory.join("files").join(name)
            }
        };
        let root = fs::canonicalize(&self.paths.diagnostics_dir)?;
        let resolved_dir = fs::canonicalize(&directory)?;
        let resolved = fs::canonicalize(&path)?;
        if !resolved_dir.starts_with(&root) || resolved_dir == root || !resolved.starts_with(&resolved_dir)
            || (file.is_some() && !resolved.is_file()) || (file.is_none() && !resolved.is_dir()) {
            return Err(invalid_data("diagnostic path is outside its bundle or has an invalid type"));
        }
        Ok(path)
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

/// Redact token- and credential-shaped text using the same policy as exported
/// diagnostics bundles. Callers must still bound input before invoking this.
pub fn redact_diagnostics_payload(payload: &[u8]) -> (Vec<u8>, bool) {
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
    if lower.contains("bearer ") || lower.split_ascii_whitespace().any(|field| field == "token") {
        return true;
    }
    [
        "password",
        "passwd",
        "secret",
        "authorization",
        "token",
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
        ffi::OsString,
        fs,
        path::{Path, PathBuf},
    };

    use nexus_protocol::{
        AgentLifecycleState, HarnessConfigPayload, HarnessLaunchMode, HarnessRuntimeInfo,
        HarnessState, RuntimeOwnership, RuntimeSource,
    };

    use super::{
        build_pnpm_args, build_runtime_child_env, discover_harness_candidates_in_roots, is_within,
        load_harness_launch_spec, normalize_discovery_path, read_runtime_metadata,
        resolve_runtime_command, write_runtime_metadata, AgentState, CheckpointRestoreIntent,
        CheckpointRestoreJournalStore, CheckpointRestorePhase, CheckpointStore, ConfigStore,
        DiagnosticsStore, HarnessLaunchSpec, HarnessLogSession, HarnessLogSessionStore,
        NexusConfig, NexusConfigFile, NexusPaths, NexusRuntimeMetadata, ProfileCatalog,
        ProfileStore, ReleaseStore, ReleasesConfig, RuntimeConfig, RuntimePin, UpdateSpec,
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
            paths.runtimes_dir,
            PathBuf::from("workspace/nexus/runtimes")
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
    fn legacy_official_harness_config_gets_runtime_readiness_defaults() {
        let root = unique_test_root("legacy-harness-readiness");
        let paths = NexusPaths::from_root(root.clone());
        fs::create_dir_all(&root).expect("test root creates");
        fs::write(
            &paths.config_file,
            r#"{
                "harness": {
                    "mode": "direct",
                    "program": "deepseek-harness.exe"
                }
            }"#,
        )
        .expect("config writes");

        let spec = load_harness_launch_spec(&paths)
            .expect("config reads")
            .expect("harness is configured");
        assert_eq!(spec.readiness_url.as_deref(), Some("tcp://127.0.0.1:3080"));
        assert_eq!(spec.readiness_timeout_secs, Some(30));
        assert!(spec.readiness_token_required);
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
    fn profile_store_creates_profile_with_template_files() {
        let root = unique_test_root("profiles-create");
        let paths = NexusPaths::from_root(root.clone());
        let dsh_home = root.join("dsh-home");
        fs::create_dir_all(&dsh_home).unwrap();
        let store = ProfileStore::new(paths.clone());

        let catalog = store.create("team-x", &dsh_home).expect("profile created");
        assert!(catalog.profiles.iter().any(|item| item == "team-x"));

        let dir = dsh_home.join("profiles").join("team-x");
        assert!(dir.join("package.json").exists());
        assert!(dir.join("cordis.patch.yml").exists());
        assert!(dir.join("pnpm-workspace.yaml").exists());
        let manifest: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(dir.join("package.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            manifest["dsh"]["profile"]["bundles"][0],
            "@deepseek-ai/dsh-base"
        );
        assert_eq!(manifest["name"], "dsh-profile-team-x");

        let duplicate = store
            .create("team-x", &dsh_home)
            .expect_err("duplicate must fail");
        assert_eq!(duplicate.kind(), std::io::ErrorKind::AlreadyExists);
    }

    #[test]
    fn release_store_rejects_register_when_slots_full() {
        let root = unique_test_root("releases-full");
        let paths = NexusPaths::from_root(root.clone());
        let store = super::ReleaseStore::new(paths.clone()).with_max_slots(2);
        store
            .register("harness-a", "1", None, None)
            .expect("first slot registers");
        store
            .register("harness-b", "2", None, None)
            .expect("second slot registers");
        let preflight = store
            .ensure_capacity_for_new()
            .expect_err("cold acquisition preflight rejects full capacity");
        assert_eq!(preflight.kind(), std::io::ErrorKind::ResourceBusy);
        let error = store
            .register("harness-c", "3", None, None)
            .expect_err("third slot must be rejected when full");
        assert_eq!(error.kind(), std::io::ErrorKind::ResourceBusy);
        assert!(error.to_string().contains("release slots are full (2/2)"));
        let catalog = store.load().expect("catalog still loads");
        assert_eq!(catalog.releases.len(), 2);
    }

    #[test]
    fn release_store_remove_rejects_selected_slots_and_deletes_free_ones() {
        let root = unique_test_root("releases-remove");
        let paths = NexusPaths::from_root(root.clone());
        let store = super::ReleaseStore::new(paths.clone());
        store
            .register("harness-a", "1", None, None)
            .expect("alpha registers");
        store
            .register("harness-b", "2", None, None)
            .expect("beta registers");
        store.promote("harness-a").expect("alpha promoted");

        let protected = store.remove("harness-a").expect_err("current is protected");
        assert_eq!(protected.kind(), std::io::ErrorKind::ResourceBusy);
        assert!(paths.releases_dir.join("harness-a").exists());

        let removed = store.remove("harness-b").expect("free slot is removed");
        assert!(removed.find("harness-b").is_none());
        assert!(!paths.releases_dir.join("harness-b").exists());
        assert_eq!(removed.current_release.as_deref(), Some("harness-a"));

        let missing = store.remove("harness-b").expect_err("already removed");
        assert_eq!(missing.kind(), std::io::ErrorKind::NotFound);
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
            snapshot: None,
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
    fn harness_launch_modes_are_explicit_and_legacy_specs_remain_direct() {
        let direct = HarnessLaunchSpec {
            mode: HarnessLaunchMode::Direct,
            program: PathBuf::from("node"),
            args: vec!["--version".to_owned()],
            working_dir: None,
            readiness_url: None,
            readiness_timeout_secs: None,
            readiness_token_required: false,
        };
        let direct_payload = direct.to_payload();
        assert_eq!(direct_payload.mode, HarnessLaunchMode::Direct);
        assert_eq!(direct_payload.args, vec!["--version"]);
        assert_eq!(direct_payload.entry, None);

        let node = HarnessLaunchSpec::from_payload(HarnessConfigPayload {
            mode: HarnessLaunchMode::Node,
            program: "node".to_owned(),
            args: vec!["--port".to_owned(), "3080".to_owned()],
            args_are_additional: true,
            entry: Some("dist/index.js".to_owned()),
            working_dir: None,
            readiness_url: None,
            readiness_timeout_secs: None,
            readiness_token_required: false,
        })
        .expect("node payload validates");
        assert_eq!(node.mode, HarnessLaunchMode::Node);
        assert_eq!(node.args[0], "dist/index.js");
        let node_payload = node.to_payload();
        assert_eq!(node_payload.mode, HarnessLaunchMode::Node);
        assert_eq!(node_payload.entry.as_deref(), Some("dist/index.js"));
        assert_eq!(node_payload.args, vec!["--port", "3080"]);

        let legacy_node = HarnessLaunchSpec::from_payload(HarnessConfigPayload {
            mode: HarnessLaunchMode::Node,
            program: "node".to_owned(),
            args: vec![
                "dist/index.js".to_owned(),
                "--port".to_owned(),
                "3080".to_owned(),
            ],
            args_are_additional: false,
            entry: None,
            working_dir: None,
            readiness_url: None,
            readiness_timeout_secs: None,
            readiness_token_required: false,
        })
        .expect("legacy node payload validates");
        assert_eq!(legacy_node.args, vec!["dist/index.js", "--port", "3080"]);

        let duplicate_entry = HarnessLaunchSpec::from_payload(HarnessConfigPayload {
            mode: HarnessLaunchMode::Node,
            program: "node".to_owned(),
            args: vec![
                "dist/index.js".to_owned(),
                "--port".to_owned(),
                "3080".to_owned(),
            ],
            args_are_additional: false,
            entry: Some("dist/index.js".to_owned()),
            working_dir: None,
            readiness_url: None,
            readiness_timeout_secs: None,
            readiness_token_required: false,
        })
        .expect("duplicate entry payload validates");
        assert_eq!(
            duplicate_entry.args,
            vec!["dist/index.js", "--port", "3080"]
        );

        let repeated_argument = HarnessLaunchSpec::from_payload(HarnessConfigPayload {
            mode: HarnessLaunchMode::Node,
            program: "node".to_owned(),
            args: vec!["dist/index.js".to_owned(), "--port".to_owned()],
            args_are_additional: true,
            entry: Some("dist/index.js".to_owned()),
            working_dir: None,
            readiness_url: None,
            readiness_timeout_secs: None,
            readiness_token_required: false,
        })
        .expect("canonical Node payload preserves repeated arguments");
        assert_eq!(
            repeated_argument.args,
            vec!["dist/index.js", "dist/index.js", "--port"]
        );

        let legacy: HarnessLaunchSpec = serde_json::from_value(serde_json::json!({
            "program": "node",
            "args": ["dist/index.js", "--port", "3080"]
        }))
        .expect("legacy spec deserializes");
        assert_eq!(legacy.mode, HarnessLaunchMode::Direct);
        assert_eq!(legacy.to_payload().mode, HarnessLaunchMode::Direct);
        assert_eq!(legacy.to_payload().args, legacy.args);
    }

    #[test]
    fn harness_discovery_finds_official_node_fixture_and_bounded_direct_targets() {
        let root = unique_test_root("harness-discovery");
        let node_program = root.join("runtime").join("node.exe");
        let direct_program = root.join("bin").join("deepseek-harness.exe");
        let package_dir = root.join("deepseek-harness").join("apps").join("cli");
        let entry = package_dir.join("lib").join("bin.js");
        fs::create_dir_all(node_program.parent().expect("node parent creates"))
            .expect("node parent creates");
        fs::create_dir_all(direct_program.parent().expect("direct parent creates"))
            .expect("direct parent creates");
        fs::create_dir_all(entry.parent().expect("entry parent creates"))
            .expect("entry parent creates");
        fs::write(&node_program, b"node fixture").expect("node fixture writes");
        fs::write(&direct_program, b"harness fixture").expect("direct fixture writes");
        fs::write(&entry, b"console.log('fixture')").expect("entry fixture writes");
        fs::write(
            package_dir.join("package.json"),
            r#"{"name":"@deepseek-ai/dsh","version":"alpha.3","bin":{"dsh":"lib/bin.js"}}"#,
        )
        .expect("package manifest writes");

        let ignored_package = root.join("node_modules").join("deepseek-harness");
        fs::create_dir_all(ignored_package.join("lib")).expect("ignored package creates");
        fs::write(
            ignored_package.join("package.json"),
            r#"{"name":"@deepseek-ai/dsh","version":"ignored","bin":{"dsh":"lib/bin.js"}}"#,
        )
        .expect("ignored manifest writes");
        fs::write(ignored_package.join("lib").join("bin.js"), b"ignored")
            .expect("ignored entry writes");

        let internal_package = root
            .join("deepseek-harness")
            .join("packages")
            .join("internal");
        fs::create_dir_all(&internal_package).expect("internal package creates");
        fs::write(
            internal_package.join("package.json"),
            r#"{"name":"@deepseek-ai/dsh-internal","version":"ignored","main":"index.js"}"#,
        )
        .expect("internal manifest writes");
        fs::write(internal_package.join("index.js"), b"internal").expect("internal entry writes");

        let roots = vec![(root.clone(), "fixture".to_owned())];
        let candidates = discover_harness_candidates_in_roots(&roots, Some(&node_program));
        let direct = candidates
            .iter()
            .find(|candidate| candidate.mode == HarnessLaunchMode::Direct)
            .expect("direct fixture is discovered");
        assert!(direct.program.ends_with("deepseek-harness.exe"));
        assert_eq!(
            direct.readiness_url.as_deref(),
            Some("tcp://127.0.0.1:3080")
        );
        assert_eq!(direct.readiness_timeout_secs, Some(30));
        assert!(direct.readiness_token_required);
        let node = candidates
            .iter()
            .find(|candidate| candidate.mode == HarnessLaunchMode::Node)
            .expect("official node fixture is discovered");
        assert_eq!(node.version.as_deref(), Some("alpha.3"));
        assert_eq!(
            node.program,
            normalize_discovery_path(
                &fs::canonicalize(&node_program).expect("node path canonicalizes")
            )
            .to_string_lossy()
        );
        assert_eq!(
            node.args,
            vec![
                "--profile".to_owned(),
                "{profile}".to_owned(),
                "--no-open".to_owned(),
                "--host".to_owned(),
                "127.0.0.1".to_owned(),
                "--port".to_owned(),
                "3080".to_owned()
            ]
        );
        assert_eq!(node.readiness_url.as_deref(), Some("tcp://127.0.0.1:3080"));
        assert!(node.readiness_token_required);
        assert!(node
            .entry
            .as_deref()
            .is_some_and(|path| path.contains("apps")
                && path.contains("cli")
                && path.ends_with("bin.js")));
        assert!(!candidates.iter().any(|candidate| {
            candidate
                .entry
                .as_deref()
                .is_some_and(|path| path.contains("node_modules"))
        }));
        assert!(!candidates.iter().any(|candidate| {
            candidate.entry.as_deref().is_some_and(|path| {
                path.contains("packages\\internal") || path.contains("packages/internal")
            })
        }));

        assert!(discover_harness_candidates_in_roots(&[], Some(&node_program)).is_empty());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn harness_discovery_anchors_on_data_root_parent_for_sibling_checkout() {
        let sandbox = unique_test_root("harness-discovery-sibling");
        let data_root = sandbox.join("nexus-data");
        let sibling = sandbox.join("deepseek-harness");
        fs::create_dir_all(&data_root).expect("data root creates");
        fs::create_dir_all(&sibling).expect("sibling creates");
        fs::write(sibling.join("deepseek-harness.exe"), b"harness fixture")
            .expect("sibling executable writes");

        let response =
            super::discover_harness_candidates_with_paths(&NexusPaths::from_root(data_root));
        assert!(response
            .candidates
            .iter()
            .any(|candidate| candidate.program.ends_with("deepseek-harness.exe")));
        let _ = fs::remove_dir_all(sandbox);
    }

    #[test]
    fn diagnostics_collects_bounded_redacted_nexus_files_only() {
        let root = unique_test_root("diagnostics");
        let paths = NexusPaths::from_root(root.clone());
        paths.ensure_directories().expect("directories create");
        fs::write(
            paths.logs_dir.join("harness.stdout.log"),
            concat!(
                "normal failure\n",
                "dsh web: http://127.0.0.1:3080/?token=real-dsh-token\n",
                "request=http://127.0.0.1:3080/health?api_token=query-token\n",
                "Authorization: Bearer authorization-secret\n",
                "Bearer standalone-bearer-secret\n",
                "token standalone-token-secret\n",
            ),
        )
        .expect("diagnostic log writes");
        let store = DiagnosticsStore::new(paths.clone());
        let bundle = store
            .collect(Some("after failed start".to_owned()))
            .expect("diagnostics collect");
        assert!(bundle.id.starts_with("diag-"));
        assert!(store.open_path(&bundle.id, None).unwrap().is_dir());
        assert!(store.open_path(&bundle.id, Some("diagnostics.json")).unwrap().is_file());
        assert!(store.open_path(&bundle.id, Some("logs/harness.stdout.log")).unwrap().is_file());
        for name in ["../diagnostics.json", "logs\\harness.stdout.log", "C:/Windows/notepad.exe", "missing.txt"] {
            assert!(store.open_path(&bundle.id, Some(name)).is_err());
        }
        assert!(store.open_path("../escape", None).is_err());
        assert!(store.open_path("diag-missing", None).is_err());
        let log = bundle
            .files
            .iter()
            .find(|file| file.name == "logs/harness.stdout.log")
            .expect("harness log is included");
        assert!(log.redacted);
        let log_path = PathBuf::from(&bundle.directory).join("files/logs/harness.stdout.log");
        let copied = fs::read_to_string(log_path).expect("copied diagnostics log reads");
        assert!(copied.contains("[REDACTED]"));
        for secret in [
            "real-dsh-token",
            "query-token",
            "authorization-secret",
            "standalone-bearer-secret",
            "standalone-token-secret",
        ] {
            assert!(!copied.contains(secret), "diagnostics leaked {secret}");
        }
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
                mode: Default::default(),
                program: PathBuf::from("bin/harness"),
                args: vec!["--profile".to_owned(), "{profile}".to_owned()],
                working_dir: Some(PathBuf::from("runtime")),
                readiness_url: Some("http://127.0.0.1:3080/health".to_owned()),
                readiness_timeout_secs: Some(5),
                readiness_token_required: false,
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
            releases: None,
            runtime: None,
            snapshots: None,
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

    #[test]
    fn config_transactions_from_distinct_instances_preserve_unrelated_fields() {
        let root = unique_test_root("config-transaction-instances");
        let paths = NexusPaths::from_root(root.clone());
        let first = ConfigStore::new(paths.clone());
        let second = ConfigStore::new(paths.clone());
        let (first_loaded_tx, first_loaded_rx) = std::sync::mpsc::channel();
        let (release_first_tx, release_first_rx) = std::sync::mpsc::channel();
        let first_thread = std::thread::spawn(move || {
            first
                .transaction(|document| {
                    document.releases = Some(ReleasesConfig { max_slots: 7 });
                    first_loaded_tx.send(()).expect("first load is observed");
                    release_first_rx.recv().expect("first write is released");
                    Ok(())
                })
                .expect("first transaction succeeds");
        });
        first_loaded_rx.recv().expect("first transaction loaded");

        let (second_done_tx, second_done_rx) = std::sync::mpsc::channel();
        let second_thread = std::thread::spawn(move || {
            second
                .transaction(|document| {
                    document.runtime = Some(RuntimeConfig::default());
                    Ok(())
                })
                .expect("second transaction succeeds");
            second_done_tx
                .send(())
                .expect("second completion is observed");
        });
        let _ = second_done_rx.recv_timeout(std::time::Duration::from_millis(100));
        release_first_tx
            .send(())
            .expect("first transaction releases");
        first_thread.join().expect("first transaction joins");
        second_thread.join().expect("second transaction joins");

        let document = ConfigStore::new(paths)
            .load()
            .expect("combined config loads");
        assert_eq!(document.releases.map(|value| value.max_slots), Some(7));
        assert_eq!(document.runtime, Some(RuntimeConfig::default()));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn runtime_config_is_structural_and_survives_removed_pins() {
        let root = unique_test_root("runtime-config-structural");
        let paths = NexusPaths::from_root(root.clone());
        let missing_node = paths.runtimes_dir.join("node/node.exe");
        let runtime = RuntimeConfig {
            node: Some(RuntimePin {
                path: missing_node.clone(),
                ownership: RuntimeOwnership::Nexus,
            }),
            ..RuntimeConfig::default()
        };
        let store = ConfigStore::new(paths.clone());
        store
            .write(&NexusConfigFile {
                runtime: Some(runtime.clone()),
                ..NexusConfigFile::default()
            })
            .expect("missing runtime pin is structurally valid");
        fs::remove_dir_all(&paths.runtimes_dir).expect("runtime directory removes");
        assert_eq!(
            store.load().expect("config remains repairable").runtime,
            Some(runtime)
        );

        let legacy: NexusConfigFile =
            serde_json::from_str(r#"{"releases":{"max_slots":3}}"#).expect("legacy config parses");
        assert_eq!(legacy.runtime, None);
        let invalid = RuntimeConfig {
            node: Some(RuntimePin {
                path: PathBuf::from("relative/node"),
                ownership: RuntimeOwnership::Nexus,
            }),
            ..RuntimeConfig::default()
        };
        assert!(invalid.validate().is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn shared_runtime_command_and_child_environment_cover_pnpm_js_entries() {
        let root = unique_test_root("runtime-command");
        let node = root.join("node/node.exe");
        let pnpm_verbatim = PathBuf::from(
            r"\\?\C:\Users\PC\AppData\Local\node\corepack\v1\pnpm\11.7.0\bin\pnpm.mjs",
        );
        let config = RuntimeConfig {
            node: Some(RuntimePin {
                path: node.clone(),
                ownership: RuntimeOwnership::Nexus,
            }),
            pnpm: Some(RuntimePin {
                path: pnpm_verbatim.clone(),
                ownership: RuntimeOwnership::Nexus,
            }),
            source: RuntimeSource::Npmmirror,
            ..RuntimeConfig::default()
        };
        let command = resolve_runtime_command(&config, "pnpm")
            .expect("pnpm command resolves")
            .expect("pnpm pin exists");
        assert_eq!(command.program, node);
        assert_eq!(
            command.prefix_args,
            vec![OsString::from(
                r"C:\Users\PC\AppData\Local\node\corepack\v1\pnpm\11.7.0\bin\pnpm.mjs"
            )]
        );
        let environment = build_runtime_child_env(&config, None).expect("child PATH builds");
        let path_entries: Vec<_> = std::env::split_paths(&environment[0].1).collect();
        assert_eq!(path_entries[0], root.join("node"));
        assert_eq!(path_entries[1], pnpm_verbatim.parent().unwrap());
        let args = build_pnpm_args(&config, ["install".into()]);
        assert_eq!(args[0], "--config.minimumReleaseAge=0");
        assert_eq!(args[1], "--config.registry=https://registry.npmmirror.com");
        assert_eq!(args[2], "install");

        let paths = NexusPaths::from_root(root.clone());
        let outside = RuntimeConfig {
            node: Some(RuntimePin {
                path: root.join("outside/node.exe"),
                ownership: RuntimeOwnership::Nexus,
            }),
            ..RuntimeConfig::default()
        };
        assert!(outside.validate_for_paths(&paths).is_err());
    }

    #[test]
    fn verbatim_runtime_paths_normalize_only_at_process_argument_boundary() {
        assert_eq!(
            normalize_discovery_path(Path::new(r"\\?\C:\runtime\node.exe")),
            PathBuf::from(r"C:\runtime\node.exe")
        );
        assert_eq!(
            normalize_discovery_path(Path::new(r"\\?\UNC\server\share\pnpm.mjs")),
            PathBuf::from(r"\\server\share\pnpm.mjs")
        );
    }

    fn unique_test_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "nexus-core-{label}-{}-{}",
            std::process::id(),
            super::unix_time_seconds()
        ))
    }

    #[test]
    fn module_farm_retargets_real_links_and_preserves_user_directories() {
        let root = unique_test_root("module-farm-retarget");
        let home = root.join("home");
        fs::create_dir_all(&home).unwrap();
        let a = root.join("a");
        let b = root.join("b");
        for slot in [&a, &b] {
            let package = slot.join("packages/example");
            fs::create_dir_all(&package).unwrap();
            fs::write(package.join("package.json"), r#"{"name":"@dsh/example"}"#).unwrap();
        }
        let link = home.join("profiles/node_modules/@dsh/example");
        assert_eq!(ReleaseStore::heal_module_farm(&home, &a).unwrap(), 1);
        assert!(ReleaseStore::same_directory(&link, &a.join("packages/example")));
        assert_eq!(ReleaseStore::heal_module_farm(&home, &b).unwrap(), 1);
        assert!(ReleaseStore::same_directory(&link, &b.join("packages/example")));
        assert_eq!(ReleaseStore::heal_module_farm(&home, &b).unwrap(), 0);
        assert_eq!(ReleaseStore::heal_module_farm(&home, &a).unwrap(), 1);
        assert!(ReleaseStore::same_directory(&link, &a.join("packages/example")));
        ReleaseStore::remove_module_link(&link).unwrap();
        fs::create_dir(&link).unwrap();
        fs::write(link.join("user.txt"), "preserve").unwrap();
        assert!(ReleaseStore::heal_module_farm(&home, &b).is_err());
        assert_eq!(fs::read_to_string(link.join("user.txt")).unwrap(), "preserve");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn module_farm_rejects_escaped_names_and_linked_parents() {
        for name in ["../escape", "@scope/../../escape", "C:/escape", "a\\b", "/absolute", "@scope/", ".", ".."] {
            assert!(ReleaseStore::validate_module_name(name).is_err(), "{name}");
        }
        let root = unique_test_root("module-farm-parent");
        let home = root.join("home");
        let outside = root.join("outside");
        let slot = root.join("slot");
        fs::create_dir_all(home.join("profiles/node_modules")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::create_dir_all(slot.join("packages/example")).unwrap();
        fs::write(slot.join("packages/example/package.json"), r#"{"name":"@dsh/example"}"#).unwrap();
        let scope = home.join("profiles/node_modules/@dsh");
        ReleaseStore::create_dir_junction(&scope, &outside).unwrap();
        assert!(ReleaseStore::heal_module_farm(&home, &slot).is_err());
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
        ReleaseStore::remove_module_link(&scope).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn module_farm_failed_preparation_preserves_existing_link() {
        let root = unique_test_root("module-farm-failure");
        let target = root.join("target");
        fs::create_dir_all(&target).unwrap();
        let link = root.join("link");
        ReleaseStore::create_dir_junction(&link, &target).unwrap();
        // Embedded NUL is rejected by the OS before link creation on every platform.
        assert!(ReleaseStore::replace_module_link(&link, Path::new("bad\0target")).is_err());
        assert!(ReleaseStore::same_directory(&link, &target));
        ReleaseStore::remove_module_link(&link).unwrap();
        fs::remove_dir_all(root).unwrap();
    }
}

/// The immutable upstream Harness is launched only through this external
/// process specification. Nexus never silently chooses a discovered
/// installation; discovery is an advisory API and the selected command is
/// persisted here. Node mode is normalized to `program=node` and an entry
/// script as the first process argument so the supervisor remains unchanged.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HarnessLaunchSpec {
    #[serde(default)]
    pub mode: HarnessLaunchMode,
    pub program: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub working_dir: Option<PathBuf>,
    #[serde(default)]
    pub readiness_url: Option<String>,
    #[serde(default)]
    pub readiness_timeout_secs: Option<u64>,
    #[serde(default)]
    pub readiness_token_required: bool,
}

impl HarnessLaunchSpec {
    pub fn new(program: PathBuf) -> Self {
        Self {
            mode: HarnessLaunchMode::Direct,
            program,
            args: Vec::new(),
            working_dir: None,
            readiness_url: None,
            readiness_timeout_secs: None,
            readiness_token_required: false,
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
        if matches!(self.mode, HarnessLaunchMode::Node) {
            if !harness_program_is_node_runtime(&self.program) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Harness node launch mode requires a node runtime program",
                ));
            }
            if self.args.first().map(String::is_empty).unwrap_or(true) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Harness node launch mode requires an entry script",
                ));
            }
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
        if self.readiness_token_required && self.readiness_url.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Harness token-bound readiness requires a readiness URL",
            ));
        }
        Ok(())
    }

    pub fn to_payload(&self) -> HarnessConfigPayload {
        let mode = self.mode;
        let (entry, args) = match mode {
            HarnessLaunchMode::Node => (
                self.args.first().cloned(),
                self.args.iter().skip(1).cloned().collect(),
            ),
            HarnessLaunchMode::Direct => (None, self.args.clone()),
        };
        HarnessConfigPayload {
            mode,
            program: self.program.to_string_lossy().into_owned(),
            args,
            args_are_additional: matches!(mode, HarnessLaunchMode::Node),
            entry,
            working_dir: self
                .working_dir
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned()),
            readiness_url: self.readiness_url.clone(),
            readiness_timeout_secs: self.readiness_timeout_secs,
            readiness_token_required: self.readiness_token_required,
        }
    }

    pub fn from_payload(payload: HarnessConfigPayload) -> io::Result<Self> {
        let HarnessConfigPayload {
            mode,
            program,
            mut args,
            args_are_additional,
            entry,
            working_dir,
            readiness_url,
            readiness_timeout_secs,
            readiness_token_required,
        } = payload;
        if matches!(mode, HarnessLaunchMode::Direct) && entry.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Harness entry is only valid for node launch mode",
            ));
        }
        if matches!(mode, HarnessLaunchMode::Node) {
            // Current clients send `entry` separately. Older clients sent the
            // Node entry as args[0], so accept that representation while
            // normalizing both forms to the internal argv shape.
            let (entry, entry_was_separate) = match entry {
                Some(entry) => (entry, true),
                None => (
                    args.first().cloned().ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "Harness node launch mode requires an entry script",
                        )
                    })?,
                    false,
                ),
            };
            if entry.is_empty() || entry.chars().any(char::is_control) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Harness node entry must be non-empty and contain no control characters",
                ));
            }
            if entry_was_separate
                && (args_are_additional
                    || args
                        .first()
                        .map(|argument| argument != &entry)
                        .unwrap_or(true))
            {
                args.insert(0, entry);
            }
        }
        let spec = Self {
            mode,
            program: PathBuf::from(program),
            args,
            working_dir: working_dir.map(PathBuf::from),
            readiness_url,
            readiness_timeout_secs,
            readiness_token_required,
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

fn harness_program_is_node_runtime(program: &Path) -> bool {
    let Some(name) = program.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let name = name.to_ascii_lowercase();
    matches!(name.as_str(), "node" | "node.exe" | "nodejs" | "nodejs.exe")
}

const HARNESS_DISCOVERY_MAX_DEPTH: usize = 4;
const HARNESS_DISCOVERY_MAX_CANDIDATES: usize = 32;
const HARNESS_DISCOVERY_MAX_DIRECTORIES: usize = 512;
const HARNESS_DISCOVERY_MAX_ENTRIES_PER_DIR: usize = 256;
const HARNESS_DISCOVERY_MAX_MANIFEST_BYTES: u64 = 512 * 1024;

/// Discover likely immutable Harness launch targets without scanning an
/// entire volume. The result is deliberately advisory; the caller must
/// explicitly select a candidate before writing it to Nexus config.
pub fn discover_harness_candidates() -> HarnessDiscoveryResponse {
    let roots = discovery_roots();
    let node_program = find_node_program();
    let mut candidates = discover_harness_candidates_in_roots(&roots, node_program.as_deref());
    append_path_direct_candidates(&mut candidates);
    HarnessDiscoveryResponse::new(candidates)
}

/// Discover using the Agent's configured data root as an additional bounded
/// anchor. A common local layout keeps `nexus-data` beside the checked-out
/// `deepseek-harness` tree, which is not necessarily below the process or
/// user-home directory.
pub fn discover_harness_candidates_with_paths(paths: &NexusPaths) -> HarnessDiscoveryResponse {
    // The Agent's own data-root anchor is the most actionable source (and in
    // the supported local layout it sits beside the checked-out Harness).
    // Put it first so a busy home directory cannot consume the bounded
    // traversal budget before the configured workspace is inspected.
    let mut roots = Vec::new();
    if let Some(parent) = paths.root.parent() {
        push_discovery_root(&mut roots, parent.to_path_buf(), "data_root_parent");
    }
    push_discovery_root(&mut roots, paths.root.clone(), "data_root");
    for (root, source) in discovery_roots() {
        push_discovery_root(&mut roots, root, &source);
    }
    let node_program = find_node_program();
    let mut candidates = discover_harness_candidates_in_roots(&roots, node_program.as_deref());
    append_path_direct_candidates(&mut candidates);
    HarnessDiscoveryResponse::new(candidates)
}

fn discovery_roots() -> Vec<(PathBuf, String)> {
    let mut roots = Vec::new();
    if let Some(root) = non_empty_env("NEXUS_HARNESS_ROOT") {
        push_discovery_root(&mut roots, PathBuf::from(root), "configured");
    }
    if let Some(root) = non_empty_env("DEEPSEEK_HARNESS_ROOT") {
        push_discovery_root(&mut roots, PathBuf::from(root), "configured");
    }
    if let Some(root) = non_empty_env("DSH_HOME") {
        push_discovery_root(&mut roots, PathBuf::from(root), "configured");
    }
    if let Ok(current_dir) = env::current_dir() {
        push_discovery_root(&mut roots, current_dir, "current_dir");
    }
    if let Ok(current_exe) = env::current_exe() {
        if let Some(parent) = current_exe.parent() {
            push_discovery_root(&mut roots, parent.to_path_buf(), "current_exe");
        }
    }

    if let Some(home) = user_home_dir() {
        push_discovery_root(&mut roots, home.clone(), "home");
        for relative in [
            ".dsh",
            "dsh",
            "deepseek-harness",
            "AppData/Local/dsh",
            "AppData/Local/deepseek-harness",
            "AppData/Roaming/dsh",
            "AppData/Roaming/deepseek-harness",
        ] {
            push_discovery_root(&mut roots, home.join(relative), "home");
        }
    }
    roots
}

fn user_home_dir() -> Option<PathBuf> {
    if let Some(home) = non_empty_env("HOME") {
        return Some(PathBuf::from(home));
    }
    non_empty_env("USERPROFILE").map(PathBuf::from)
}

fn push_discovery_root(roots: &mut Vec<(PathBuf, String)>, root: PathBuf, source: &str) {
    let Ok(root) = fs::canonicalize(root) else {
        return;
    };
    if !root.is_dir() || roots.iter().any(|(known, _)| known == &root) {
        return;
    }
    roots.push((root, source.to_owned()));
}

fn discover_harness_candidates_in_roots(
    roots: &[(PathBuf, String)],
    node_program: Option<&Path>,
) -> Vec<HarnessCandidate> {
    let mut candidates = Vec::new();
    let mut seen = HashSet::new();
    let mut visited_directories = 0;
    for (root, source) in roots {
        if candidates.len() >= HARNESS_DISCOVERY_MAX_CANDIDATES {
            break;
        }
        let Ok(root) = fs::canonicalize(root) else {
            continue;
        };
        if !root.is_dir() {
            continue;
        }
        scan_discovery_dir(
            &root,
            &root,
            0,
            source,
            node_program,
            &mut candidates,
            &mut seen,
            &mut visited_directories,
        );
    }
    candidates.sort_by(|left, right| left.id.cmp(&right.id));
    candidates
}

fn scan_discovery_dir(
    root: &Path,
    directory: &Path,
    depth: usize,
    source: &str,
    node_program: Option<&Path>,
    candidates: &mut Vec<HarnessCandidate>,
    seen: &mut HashSet<String>,
    visited_directories: &mut usize,
) {
    if candidates.len() >= HARNESS_DISCOVERY_MAX_CANDIDATES
        || *visited_directories >= HARNESS_DISCOVERY_MAX_DIRECTORIES
    {
        return;
    }
    *visited_directories += 1;

    for name in [
        "deepseek-harness",
        "deepseek-harness.exe",
        "dsh-harness",
        "dsh-harness.exe",
        "harness",
        "harness.exe",
    ] {
        add_direct_candidate(root, &directory.join(name), source, candidates, seen);
        if candidates.len() >= HARNESS_DISCOVERY_MAX_CANDIDATES {
            return;
        }
    }

    if let Some(node_program) = node_program {
        add_node_manifest_candidate(root, directory, source, node_program, candidates, seen);
    }

    if depth >= HARNESS_DISCOVERY_MAX_DEPTH {
        return;
    }
    let mut directories = Vec::new();
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries
        .flatten()
        .take(HARNESS_DISCOVERY_MAX_ENTRIES_PER_DIR)
    {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let name = entry.file_name();
        if name == ".git" || name == "target" || name == "node_modules" {
            continue;
        }
        directories.push(entry.path());
    }
    directories.sort();
    for child in directories {
        let Ok(child) = fs::canonicalize(child) else {
            continue;
        };
        if !is_within(root, &child) || !child.is_dir() {
            continue;
        }
        scan_discovery_dir(
            root,
            &child,
            depth + 1,
            source,
            node_program,
            candidates,
            seen,
            visited_directories,
        );
        if candidates.len() >= HARNESS_DISCOVERY_MAX_CANDIDATES {
            return;
        }
    }
}

fn add_direct_candidate(
    root: &Path,
    program: &Path,
    source: &str,
    candidates: &mut Vec<HarnessCandidate>,
    seen: &mut HashSet<String>,
) {
    let Some(program) = canonical_file_within(root, program) else {
        return;
    };
    let Some(parent) = program.parent() else {
        return;
    };
    let program = normalize_discovery_path(&program);
    let parent = normalize_discovery_path(parent);
    let program_text = program.to_string_lossy().into_owned();
    let id = discovery_candidate_id(HarnessLaunchMode::Direct, &program_text, None);
    if !seen.insert(id.clone()) {
        return;
    }
    let display_name = program
        .file_stem()
        .or_else(|| program.file_name())
        .and_then(|name| name.to_str())
        .unwrap_or("Harness")
        .to_owned();
    let (readiness_url, readiness_timeout_secs, readiness_token_required) =
        discovered_direct_readiness(&program);
    candidates.push(HarnessCandidate {
        id,
        mode: HarnessLaunchMode::Direct,
        program: program_text,
        entry: None,
        args: Vec::new(),
        working_dir: Some(parent.to_string_lossy().into_owned()),
        readiness_url,
        readiness_timeout_secs,
        readiness_token_required,
        source: source.to_owned(),
        display_name,
        version: None,
    });
}

/// The official native DSH build uses the same loopback web service and token
/// log contract as the Node package. Keep generic `harness(.exe)` discoveries
/// manual-only because their port and authentication behavior are unknown.
fn discovered_direct_readiness(program: &Path) -> (Option<String>, Option<u64>, bool) {
    let official_binary = program
        .file_stem()
        .and_then(|name| name.to_str())
        .map(str::to_ascii_lowercase)
        .is_some_and(|name| matches!(name.as_str(), "deepseek-harness" | "dsh-harness"));
    if official_binary {
        (Some("tcp://127.0.0.1:3080".to_owned()), Some(30), true)
    } else {
        (None, None, false)
    }
}

fn add_node_manifest_candidate(
    root: &Path,
    directory: &Path,
    source: &str,
    node_program: &Path,
    candidates: &mut Vec<HarnessCandidate>,
    seen: &mut HashSet<String>,
) {
    let Some(node_program) = fs::canonicalize(node_program)
        .ok()
        .filter(|path| path.is_file())
    else {
        return;
    };
    let manifest_path = directory.join("package.json");
    let Ok(metadata) = fs::metadata(&manifest_path) else {
        return;
    };
    if !metadata.is_file() || metadata.len() > HARNESS_DISCOVERY_MAX_MANIFEST_BYTES {
        return;
    }
    let Some(manifest_path) = canonical_file_within(root, &manifest_path) else {
        return;
    };
    let Ok(bytes) = fs::read(&manifest_path) else {
        return;
    };
    let Ok(manifest) = serde_json::from_slice::<NodePackageManifest>(&bytes) else {
        return;
    };
    let mut bin_name = manifest.bin.as_ref().and_then(node_manifest_bin_name);
    let package_name = manifest.name.as_deref().unwrap_or_default();
    if bin_name.is_none()
        && manifest
            .bin
            .as_ref()
            .is_some_and(serde_json::Value::is_string)
    {
        // A string-valued `bin` has no command key to use as a hint. The
        // package name still identifies the official @deepseek-ai/dsh
        // package and is enough for the bounded discovery check.
        bin_name = Some(package_name.to_owned());
    }
    if !looks_like_harness_package(package_name, bin_name.as_deref(), directory) {
        return;
    }
    let Some(entry) = node_manifest_entry(&manifest, bin_name.as_deref()) else {
        return;
    };
    let Some(entry) = canonical_file_within(root, &directory.join(entry)) else {
        return;
    };
    let Some(package_dir) = manifest_path.parent() else {
        return;
    };
    let node_program = normalize_discovery_path(&node_program);
    let entry = normalize_discovery_path(&entry);
    let package_dir = normalize_discovery_path(package_dir);
    let node_program = node_program.to_string_lossy().into_owned();
    let entry_text = entry.to_string_lossy().into_owned();
    let id = discovery_candidate_id(HarnessLaunchMode::Node, &node_program, Some(&entry_text));
    if !seen.insert(id.clone()) {
        return;
    }
    let display_name = if package_name.is_empty() {
        entry
            .file_stem()
            .and_then(|name| name.to_str())
            .unwrap_or("Node Harness")
            .to_owned()
    } else {
        package_name.to_owned()
    };
    let official_dsh = is_official_dsh_package(package_name);
    candidates.push(HarnessCandidate {
        id,
        mode: HarnessLaunchMode::Node,
        program: node_program,
        entry: Some(entry_text),
        args: if official_dsh {
            vec![
                "--profile".to_owned(),
                "{profile}".to_owned(),
                "--no-open".to_owned(),
                "--host".to_owned(),
                "127.0.0.1".to_owned(),
                "--port".to_owned(),
                "3080".to_owned(),
            ]
        } else {
            Vec::new()
        },
        working_dir: Some(package_dir.to_string_lossy().into_owned()),
        // DSH deliberately protects `/` with its browser token and returns
        // 401 before a cookie is minted. A TCP listener probe is explicit and
        // avoids treating an arbitrary authentication failure as HTTP health.
        readiness_url: official_dsh.then(|| "tcp://127.0.0.1:3080".to_owned()),
        readiness_timeout_secs: official_dsh.then_some(30),
        readiness_token_required: official_dsh,
        source: source.to_owned(),
        display_name,
        version: manifest.version,
    });
}

#[derive(Debug, Deserialize)]
struct NodePackageManifest {
    name: Option<String>,
    version: Option<String>,
    main: Option<String>,
    bin: Option<serde_json::Value>,
}

fn node_manifest_bin_name(bin: &serde_json::Value) -> Option<String> {
    match bin {
        serde_json::Value::String(_) => None,
        serde_json::Value::Object(values) => values
            .keys()
            .find(|key| key.to_ascii_lowercase().contains("harness"))
            .cloned()
            .or_else(|| values.keys().next().cloned()),
        _ => None,
    }
}

fn node_manifest_entry(manifest: &NodePackageManifest, bin_name: Option<&str>) -> Option<String> {
    match manifest.bin.as_ref() {
        Some(serde_json::Value::String(entry)) => Some(entry.clone()),
        Some(serde_json::Value::Object(values)) => bin_name
            .and_then(|name| values.get(name))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .or_else(|| {
                values
                    .values()
                    .find_map(serde_json::Value::as_str)
                    .map(str::to_owned)
            }),
        _ => manifest.main.clone(),
    }
}

fn looks_like_harness_package(
    package_name: &str,
    bin_name: Option<&str>,
    directory: &Path,
) -> bool {
    [Some(package_name), bin_name]
        .into_iter()
        .flatten()
        .map(str::to_ascii_lowercase)
        .any(|name| {
            name.contains("deepseek-harness")
                || name.contains("dsh-harness")
                || name == "@deepseek-ai/dsh"
                || name == "@deepseek-ai/dsh-root"
                || name == "harness"
                || name.ends_with("-harness")
        })
        // A checkout may contain many internal `@deepseek-ai/dsh-*`
        // packages. Only the directory itself may provide the generic
        // `dsh`/`deepseek-harness` hint; accepting any ancestor would turn
        // every library below a Harness checkout into a launch candidate.
        || directory
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_ascii_lowercase)
            .is_some_and(|name| name == "dsh" || name == "deepseek-harness")
}

fn is_official_dsh_package(package_name: &str) -> bool {
    matches!(
        package_name.to_ascii_lowercase().as_str(),
        "@deepseek-ai/dsh" | "@deepseek-ai/dsh-root"
    )
}

fn canonical_file_within(root: &Path, path: &Path) -> Option<PathBuf> {
    let canonical = fs::canonicalize(path).ok()?;
    if canonical.is_file() && is_within(root, &canonical) {
        Some(canonical)
    } else {
        None
    }
}

/// Windows canonicalization may return a verbatim `\\?\` path. It is valid
/// for Win32 APIs but noisy in a user-facing candidate list and less portable
/// when the selection is copied into a config file. Keep containment checks on
/// the canonical path, then normalize only the advisory payload.
fn normalize_discovery_path(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix("\\\\?\\UNC\\") {
        return PathBuf::from(format!("\\\\{rest}"));
    }
    if let Some(rest) = text.strip_prefix("\\\\?\\") {
        return PathBuf::from(rest);
    }
    path.to_path_buf()
}

fn discovery_candidate_id(mode: HarnessLaunchMode, program: &str, entry: Option<&str>) -> String {
    let mode = match mode {
        HarnessLaunchMode::Direct => "direct",
        HarnessLaunchMode::Node => "node",
    };
    format!(
        "{mode}:{}:{}",
        program.to_ascii_lowercase(),
        entry.unwrap_or_default().to_ascii_lowercase()
    )
}

fn find_on_path(names: &[&str]) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    for directory in env::split_paths(&path) {
        for name in names {
            let candidate = directory.join(name);
            if let Ok(candidate) = fs::canonicalize(candidate) {
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

/// Resolve a Node runtime for automatic package discovery without assuming
/// that a GUI process inherited the user's interactive shell PATH. Explicit
/// environment overrides win, then PATH, then a small set of conventional
/// per-user/system installation locations. Every result is canonicalized and
/// must be a regular file before it is exposed to the UI.
fn find_node_program() -> Option<PathBuf> {
    for variable in ["NEXUS_NODE_PROGRAM", "NODE_BINARY"] {
        if let Some(value) = non_empty_env(variable) {
            if let Ok(path) = fs::canonicalize(value) {
                if path.is_file() {
                    return Some(path);
                }
            }
        }
    }
    if let Some(path) = find_on_path(&["node", "node.exe", "nodejs", "nodejs.exe"]) {
        return Some(path);
    }

    let mut candidates = Vec::new();
    if let Some(home) = user_home_dir() {
        candidates.extend([
            home.join(".volta/bin/node"),
            home.join(".nvm/current/bin/node"),
            home.join(".local/bin/node"),
            home.join("bin/node"),
        ]);
    }
    #[cfg(windows)]
    {
        for variable in ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"] {
            if let Some(base) = non_empty_env(variable) {
                let base = PathBuf::from(base);
                candidates.push(base.join("nodejs/node.exe"));
                candidates.push(base.join("Programs/nodejs/node.exe"));
            }
        }
    }
    candidates.into_iter().find_map(|candidate| {
        let path = fs::canonicalize(candidate).ok()?;
        path.is_file().then_some(path)
    })
}

fn append_path_direct_candidates(candidates: &mut Vec<HarnessCandidate>) {
    if candidates.len() >= HARNESS_DISCOVERY_MAX_CANDIDATES {
        return;
    }
    let Some(path) = env::var_os("PATH") else {
        return;
    };
    let mut seen: HashSet<String> = candidates
        .iter()
        .map(|candidate| candidate.id.clone())
        .collect();
    for directory in env::split_paths(&path) {
        for name in [
            "deepseek-harness",
            "deepseek-harness.exe",
            "dsh-harness",
            "dsh-harness.exe",
            "harness",
            "harness.exe",
        ] {
            if candidates.len() >= HARNESS_DISCOVERY_MAX_CANDIDATES {
                candidates.sort_by(|left, right| left.id.cmp(&right.id));
                return;
            }
            let candidate = directory.join(name);
            let Ok(candidate) = fs::canonicalize(candidate) else {
                continue;
            };
            let Some(parent) = candidate.parent() else {
                continue;
            };
            let before = candidates.len();
            add_direct_candidate(parent, &candidate, "path", candidates, &mut seen);
            if candidates.len() == before {
                continue;
            }
        }
    }
    candidates.sort_by(|left, right| left.id.cmp(&right.id));
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

pub fn validate_update_source(source: &str) -> io::Result<()> {
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

pub fn validate_update_ref(ref_name: &str) -> io::Result<()> {
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub releases: Option<ReleasesConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<RuntimeConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshots: Option<SnapshotsConfig>,
}

/// Nexus-owned release slot capacity settings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReleasesConfig {
    #[serde(default = "default_max_release_slots")]
    pub max_slots: u32,
}

impl ReleasesConfig {
    pub fn max_slots_usize(&self) -> usize {
        self.max_slots as usize
    }
}

fn default_max_release_slots() -> u32 {
    DEFAULT_MAX_RELEASE_SLOTS as u32
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SnapshotsConfig {
    #[serde(default = "default_healthy_snapshot_slots")]
    pub healthy_slots: u32,
    #[serde(default = "default_max_manual_snapshots")]
    pub max_manual_snapshots: u32,
}

impl SnapshotsConfig {
    pub fn to_payload(&self) -> SnapshotsConfigPayload {
        SnapshotsConfigPayload {
            healthy_slots: self.healthy_slots,
            max_manual_snapshots: self.max_manual_snapshots,
        }
    }

    pub fn from_payload(payload: SnapshotsConfigPayload) -> Self {
        Self {
            healthy_slots: payload.healthy_slots,
            max_manual_snapshots: payload.max_manual_snapshots,
        }
    }
}

fn default_healthy_snapshot_slots() -> u32 {
    DEFAULT_HEALTHY_SNAPSHOT_SLOTS as u32
}

fn default_max_manual_snapshots() -> u32 {
    DEFAULT_MAX_MANUAL_SNAPSHOTS as u32
}

/// Nexus-owned configuration writer. It owns only `config.json`; Harness
/// source, working directories, and `$HOME/.dsh` are never modified here.
#[derive(Clone)]
pub struct ConfigStore {
    paths: NexusPaths,
}

static CONFIG_WRITE_GATE: Mutex<()> = Mutex::new(());

impl ConfigStore {
    pub fn new(paths: NexusPaths) -> Self {
        Self { paths }
    }

    pub fn paths(&self) -> &NexusPaths {
        &self.paths
    }

    pub fn load(&self) -> io::Result<NexusConfigFile> {
        let _guard = self.lock_gate()?;
        self.load_unlocked()
    }

    fn load_unlocked(&self) -> io::Result<NexusConfigFile> {
        if !self.paths.config_file.exists() {
            return Ok(NexusConfigFile::default());
        }
        let bytes = fs::read(&self.paths.config_file)?;
        let document: NexusConfigFile = decode_json(&bytes).map_err(invalid_data)?;
        validate_config_document(&self.paths, &document)?;
        Ok(document)
    }

    pub fn write(&self, document: &NexusConfigFile) -> io::Result<()> {
        validate_config_document(&self.paths, document)?;
        let _guard = self.lock_gate()?;
        self.write_unlocked(document)
    }

    fn write_unlocked(&self, document: &NexusConfigFile) -> io::Result<()> {
        self.paths.ensure_directories()?;
        write_json_atomic(&self.paths.root, &self.paths.config_file, document)
    }

    /// Atomically read, modify, validate, and replace the shared config file.
    pub fn transaction<T>(
        &self,
        update: impl FnOnce(&mut NexusConfigFile) -> io::Result<T>,
    ) -> io::Result<(NexusConfigFile, T)> {
        let _guard = self.lock_gate()?;
        let mut document = self.load_unlocked()?;
        let result = update(&mut document)?;
        validate_config_document(&self.paths, &document)?;
        self.write_unlocked(&document)?;
        Ok((document, result))
    }

    fn lock_gate(&self) -> io::Result<std::sync::MutexGuard<'static, ()>> {
        CONFIG_WRITE_GATE
            .lock()
            .map_err(|_| io::Error::other("config lock is poisoned"))
    }
}

fn validate_config_document(paths: &NexusPaths, document: &NexusConfigFile) -> io::Result<()> {
    if let Some(harness) = &document.harness {
        harness.validate()?;
    }
    if let Some(update) = &document.update {
        update.validate()?;
    }
    if let Some(releases) = &document.releases {
        if releases.max_slots == 0 || releases.max_slots > 32 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "releases.max_slots must be between 1 and 32",
            ));
        }
    }
    if let Some(runtime) = &document.runtime {
        runtime.validate_for_paths(paths)?;
    }
    if let Some(snapshots) = &document.snapshots {
        if snapshots.healthy_slots == 0 || snapshots.healthy_slots > 32 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "snapshots.healthy_slots must be between 1 and 32",
            ));
        }
        if snapshots.max_manual_snapshots == 0 || snapshots.max_manual_snapshots > 1024 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "snapshots.max_manual_snapshots must be between 1 and 1024",
            ));
        }
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

    apply_known_harness_readiness_defaults(&mut configured);
    configured.validate()?;
    Ok(Some(configured))
}

/// Known DSH launch shapes use the loopback web service on port 3080 and emit
/// a fresh browser token in the Nexus-owned Harness log. Fill that readiness
/// contract in memory when an older config predates the fields, so an Agent
/// restart can still supervise the current Harness generation. Explicit
/// readiness settings always win and are never overwritten.
fn apply_known_harness_readiness_defaults(spec: &mut HarnessLaunchSpec) {
    if spec.readiness_url.is_some() {
        return;
    }

    let direct = discovered_direct_readiness(&spec.program).0.is_some();
    let node = harness_program_is_node_runtime(&spec.program)
        && (spec.args.iter().any(|value| is_known_dsh_path(value))
            || spec
                .working_dir
                .as_deref()
                .is_some_and(|value| is_known_dsh_path(&value.to_string_lossy())));
    if direct || node {
        spec.readiness_url = Some("tcp://127.0.0.1:3080".to_owned());
        spec.readiness_timeout_secs = Some(30);
        spec.readiness_token_required = true;
    }
}

fn is_known_dsh_path(value: &str) -> bool {
    let value = value.to_ascii_lowercase();
    value.contains("deepseek-harness")
        || value.contains("dsh-harness")
        || value.contains("@deepseek-ai/dsh")
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
