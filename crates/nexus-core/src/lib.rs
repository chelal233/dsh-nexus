//! UI-independent configuration, path, and state primitives for Nexus.
mod external_harness;
pub use external_harness::ExternalHarness;

use std::{
    collections::HashSet,
    env,
    ffi::{OsStr, OsString},
    fs, io,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
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
pub mod profile_history;
pub mod agent_auth;
pub mod terminal_lease;
pub mod log_retention;
/// Verifies the ACL/mode of a private evidence file without changing it.
pub fn verify_private_file(file: &std::fs::File) -> std::io::Result<()> { nexus_private_file::verify_private(file) }
pub mod disk;
mod harness_preferences;
pub use harness_preferences::*;
mod config_protection;
pub use config_protection::*;
pub mod maintenance;

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

/// Root of the bundled runtimes shipped inside the Nexus installation
/// (`<exe dir>/runtime`); `None` when the executable path is unavailable.
/// Development builds have no such directory, so no bundled candidates are
/// observed there.
pub fn bundled_runtime_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let root = exe.parent()?.join("runtime");
    root.is_absolute().then_some(root)
}

/// Cooperative cancellation flag shared across command owners.
#[derive(Debug, Clone, Default)]
pub struct CancellationToken(Arc<AtomicBool>, Option<Arc<str>>, Option<Arc<PathBuf>>);

impl CancellationToken {
    pub fn with_job_name(name: String) -> Self { Self(Arc::new(AtomicBool::new(false)), Some(name.into()), None) }
    pub fn with_process_registry(&self, path: PathBuf) -> Self { Self(self.0.clone(), self.1.clone(), Some(Arc::new(path))) }
    pub fn process_registry(&self) -> Option<&Path> { self.2.as_deref().map(PathBuf::as_path) }
    pub fn job_name(&self) -> Option<&str> { self.1.as_deref() }
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
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
#[serde(deny_unknown_fields)]
pub struct RuntimePin {
    pub path: PathBuf,
    pub ownership: RuntimeOwnership,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
        if self.schema_version > PROFILE_SCHEMA_VERSION { return Err(invalid_data("Unsupported profile catalog schema")); }
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
    history_observed: Arc<Mutex<Option<ProfileCatalog>>>,
}

impl ProfileStore {
    pub fn new(paths: NexusPaths) -> Self {
        Self {
            paths,
            write_gate: Arc::new(Mutex::new(())),
            history_observed: Arc::new(Mutex::new(None)),
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
        if let Some(previous)=self.read_unlocked()? { self.capture_history(&previous); }
        let mut catalog = catalog.clone();
        catalog.normalize()?;
        self.publish_with_history(&catalog)
    }

    /// Select a valid profile and add it to the known catalog if necessary.
    /// This changes metadata only; it never starts or restarts Harness.
    pub fn select(&self, name: &str) -> io::Result<ProfileCatalog> {
        validate_profile_name(name)?;
        let _guard = self.lock_gate()?;
        let mut catalog = self.load_unlocked()?;
        catalog.active_profile = name.to_owned();
        catalog.normalize()?;
        self.publish_with_history(&catalog)?;
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
        self.publish_with_history(&catalog)?;
        Ok(catalog)
    }

    fn load_unlocked(&self) -> io::Result<ProfileCatalog> {
        let Some(mut catalog) = self.read_unlocked()? else {
            let catalog = ProfileCatalog::default();
            self.publish_with_history(&catalog)?;
            return Ok(catalog);
        };
        let before = catalog.clone();
        catalog.normalize()?;
        if catalog != before {
            self.publish_with_history(&catalog)?;
        } else {
            self.capture_history(&catalog);
        }
        Ok(catalog)
    }

    fn publish_with_history(&self, catalog: &ProfileCatalog) -> io::Result<()> {
        write_json_atomic(&self.paths.root, &self.paths.profiles_file, catalog)?;
        self.capture_history(catalog);
        Ok(())
    }

    fn capture_history(&self, catalog: &ProfileCatalog) {
        let Ok(mut observed) = self.history_observed.lock() else { return; };
        if observed.as_ref() == Some(catalog) { return; }
        // History is auxiliary. Its failure must not turn a committed profile
        // change into an API failure or break an otherwise healthy Agent.
        if let Err(error) = profile_history::capture(&self.paths, catalog) {
            eprintln!("Nexus profile recovery history unavailable: {error}");
        }
        *observed = Some(catalog.clone());
    }

    fn read_unlocked(&self) -> io::Result<Option<ProfileCatalog>> {
        let Some(bytes) = read_regular_file_bounded(&self.paths.profiles_file, 4 * 1024 * 1024)? else { return Ok(None); };
        let catalog: ProfileCatalog = decode_json(&bytes).map_err(|error| invalid_data(format!("{}: {error}", self.paths.profiles_file.display())))?;
        if catalog.schema_version > PROFILE_SCHEMA_VERSION { return Err(invalid_data("Unsupported profile catalog schema")); }
        Ok(Some(catalog))
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
            let manifest: CheckpointManifest = decode_json(&bytes).map_err(|error| invalid_data(format!("{}: {error}", path.display())))?;
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
        let bytes = fs::read(&path)?;
        let manifest: CheckpointManifest = decode_json(&bytes).map_err(|error| invalid_data(format!("{}: {error}", path.display())))?;
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct BoundSnapshotRestore {
    pub ticket: RestoreTicket,
    pub dsh_home: PathBuf,
    pub profile_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CheckpointRestoreJournal {
    #[serde(default)]
    pub process_owner_version: u32,
    pub schema_version: u32,
    pub phase: CheckpointRestorePhase,
    pub intent: CheckpointRestoreIntent,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct CheckpointRestoreJournalDocument {
    #[serde(default = "default_checkpoint_restore_schema", deserialize_with = "deserialize_record_schema")]
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
            process_owner_version: 1,
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
    /// Read-time diagnostics only. Missing selections are never launchable.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unavailable_selections: Vec<String>,
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
            unavailable_selections: Vec::new(),
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
#[serde(deny_unknown_fields)]
struct ReleasePointerDocument {
    #[serde(default = "default_release_schema")]
    schema_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    current_release: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_known_good: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    healthy: Vec<HealthyReleaseEvidence>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HealthyReleaseEvidence {
    pub release_id: String,
    pub profile: String,
    pub config_revision: String,
    pub run_id: String,
    pub generation: u64,
    pub verified_at_unix: u64,
    entry: PathBuf,
    fingerprint: String,
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
    #[cfg(test)]
    prepared_failure_cut: Arc<std::sync::atomic::AtomicU8>,
}

impl ReleaseStore {
    fn health_records(&self) -> io::Result<Vec<HealthyReleaseEvidence>> {
        let Some(bytes) = read_regular_file_bounded(&self.paths.release_pointers_file, 1024 * 1024)? else { return Ok(Vec::new()); };
        let document: ReleasePointerDocument = decode_json(&bytes).map_err(|error| invalid_data(format!("{}: {error}", self.paths.release_pointers_file.display())))?;
        if document.schema_version != RELEASE_SCHEMA_VERSION || document.healthy.len() > 256 { return Err(invalid_data("Unsupported release health record")); }
        Ok(document.healthy)
    }

    fn health_fingerprint(&self, id: &str, relative_entry: &Path) -> io::Result<String> {
        use sha2::{Digest, Sha256};
        if relative_entry.is_absolute() || relative_entry.components().any(|c| !matches!(c, std::path::Component::Normal(_))) { return Err(invalid_data("Invalid release health entry")); }
        let slot = self.slot_dir(id)?;
        let entry = slot.join(relative_entry);
        if !fs::canonicalize(&entry)?.starts_with(fs::canonicalize(&slot)?) { return Err(invalid_data("Release health entry escapes its slot")); }
        let manifest = read_regular_file_bounded(&slot.join("manifest.json"), 65536)?.ok_or_else(|| invalid_data("Missing release manifest"))?;
        let bytes = read_regular_file_bounded(&entry, 32 * 1024 * 1024)?.ok_or_else(|| invalid_data("Missing release entry"))?;
        let mut hash = Sha256::new(); hash.update(manifest); hash.update(bytes);
        Ok(format!("{:x}", hash.finalize()))
    }

    pub fn healthy_launch_candidate(&self, id: &str, entry: &Path, profile: &str, config_revision: String) -> io::Result<HealthyReleaseEvidence> {
        validate_profile_name(profile)?;
        let slot = fs::canonicalize(self.slot_dir(id)?)?;
        let entry = fs::canonicalize(entry)?.strip_prefix(&slot).map_err(invalid_data)?.to_owned();
        Ok(HealthyReleaseEvidence { release_id: id.into(), profile: profile.into(), config_revision, run_id: String::new(), generation: 0, verified_at_unix: 0,
            fingerprint: self.health_fingerprint(id, &entry)?, entry })
    }

    pub fn record_healthy_release(&self, mut evidence: HealthyReleaseEvidence) -> io::Result<()> {
        let _gate = self.lock_gate()?;
        if evidence.run_id.is_empty() || evidence.config_revision.is_empty() || evidence.fingerprint != self.health_fingerprint(&evidence.release_id, &evidence.entry)? {
            return Err(invalid_data("Release changed since the healthy launch"));
        }
        let catalog = self.load_unlocked()?;
        if catalog.current_release.as_deref() != Some(&evidence.release_id) { return Err(invalid_data("Healthy observation belongs to an older selection")); }
        let mut healthy = self.health_records()?;
        healthy.retain(|item| item.release_id != evidence.release_id && catalog.find(&item.release_id).is_some());
        evidence.verified_at_unix = unix_time_seconds(); healthy.push(evidence);
        if healthy.len() > 256 { return Err(invalid_data("Release health record capacity reached")); }
        let document = ReleasePointerDocument { schema_version: RELEASE_SCHEMA_VERSION, current_release: catalog.current_release, last_known_good: catalog.last_known_good, healthy };
        write_json_atomic(&self.paths.root, &self.paths.release_pointers_file, &document)
    }

    pub fn verified_fallback(&self, selected: Option<&str>) -> io::Result<Option<String>> {
        let mut records = self.health_records()?;
        records.reverse();
        records.sort_by_key(|record| std::cmp::Reverse(record.verified_at_unix));
        for record in records {
            if selected != Some(record.release_id.as_str()) && self.health_evidence_matches(&record)? {
                return Ok(Some(record.release_id));
            }
        }
        Ok(None)
    }
    fn health_evidence_matches(&self, record: &HealthyReleaseEvidence) -> io::Result<bool> {
        match self.health_fingerprint(&record.release_id, &record.entry) {
            Ok(fingerprint) => Ok(fingerprint == record.fingerprint),
            Err(error) if matches!(error.kind(), io::ErrorKind::NotFound | io::ErrorKind::InvalidData | io::ErrorKind::InvalidInput) => Ok(false),
            // A locked or temporarily unavailable file is not negative health
            // evidence. Let the caller retry without rewriting the pointers.
            Err(error) => Err(error),
        }
    }
    pub fn new(paths: NexusPaths) -> Self {
        Self {
            paths,
            write_gate: Arc::new(Mutex::new(())),
            max_slots: DEFAULT_MAX_RELEASE_SLOTS,
            #[cfg(test)]
            prepared_failure_cut: Arc::new(std::sync::atomic::AtomicU8::new(0)),
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
        if self.pending_cleanup(id)?.is_some() {
            return Err(io::Error::new(io::ErrorKind::ResourceBusy, "Finish the previous cleanup before reusing this release id"));
        }
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
        if self.pending_cleanup(id)?.is_some() {
            return Err(io::Error::new(io::ErrorKind::ResourceBusy, "Finish the previous cleanup before reusing this release id"));
        }
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
        match fs::symlink_metadata(candidate.join("manifest.json")) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
            Ok(_) => return Err(io::Error::new(io::ErrorKind::InvalidData, "prepared candidate must not supply a release manifest")),
        }
        let links = Self::prepared_absolute_links(&candidate)?;
        let manifest = ReleaseManifest {
            id: id.to_owned(), version: version.to_owned(),
            installed_at_unix: unix_time_seconds(), source, note,
        };
        let marker = candidate.join(".nexus-cleanup.json");
        if fs::symlink_metadata(&marker).is_ok() { return Err(invalid_data("Prepared candidate supplies a cleanup marker")); }
        write_json_atomic(&candidate, &marker, &manifest)?;
        fs::rename(&candidate, &slot_dir)?;
        #[cfg(test)]
        if self.prepared_failure_cut.load(std::sync::atomic::Ordering::SeqCst) == 1 { return Err(io::Error::other("Injected prepared link repair failure")); }
        // pnpm uses absolute Windows junctions for workspace dependencies.
        // Moving the tree alone leaves them pointing at downloads/.cold-*.
        // Repair before writing the manifest: a failed repair stays unselectable.
        for (relative, target, directory) in links {
            let link = slot_dir.join(relative);
            let target = slot_dir.join(target);
            Self::ensure_module_directory(link.parent().ok_or_else(|| io::Error::other("prepared link has no parent"))?)?;
            if !is_within(&fs::canonicalize(&slot_dir)?, &fs::canonicalize(&target)?) {
                return Err(io::Error::other("prepared link target changed outside its slot"));
            }
            if directory {
                Self::replace_module_link(&link, &target)?;
            } else {
                fs::remove_file(&link)?;
                #[cfg(windows)]
                std::os::windows::fs::symlink_file(&target, &link)?;
                #[cfg(not(windows))]
                std::os::unix::fs::symlink(&target, &link)?;
            }
        }
        #[cfg(test)]
        if self.prepared_failure_cut.load(std::sync::atomic::Ordering::SeqCst) == 2 { return Err(io::Error::other("Injected prepared manifest failure")); }
        // The marker already contains the final manifest. Publish it in one
        // rename so a ready slot never depends on a subsequent marker deletion.
        Self::publish_prepared_manifest(&slot_dir)?;
        catalog.releases.push(manifest);
        catalog.normalize()?;
        Ok(catalog)
    }

    fn publish_prepared_manifest(slot: &Path) -> io::Result<()> {
        fs::rename(slot.join(".nexus-cleanup.json"), slot.join("manifest.json"))
    }

    /// Inspect links without descending into them. Resolve targets while the
    /// candidate still exists, including chains, so publication never relies on
    /// external directories or on the order in which links are recreated.
    fn prepared_absolute_links(root: &Path) -> io::Result<Vec<(PathBuf, PathBuf, bool)>> {
        let mut pending = vec![root.to_path_buf()];
        let mut links = Vec::new();
        let mut count = 0usize;
        while let Some(directory) = pending.pop() {
            for entry in fs::read_dir(directory)? {
                let path = entry?.path();
                count += 1;
                if count > 250_000 {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "prepared release has too many entries"));
                }
                let metadata = fs::symlink_metadata(&path)?;
                #[cfg(windows)]
                let is_link = {
                    use std::os::windows::fs::MetadataExt;
                    metadata.file_attributes() & 0x400 != 0
                };
                #[cfg(not(windows))]
                let is_link = metadata.file_type().is_symlink();
                if is_link {
                    let raw = fs::read_link(&path)?;
                    let resolved = fs::canonicalize(&path)?;
                    if !is_within(root, &resolved) || resolved == root {
                        return Err(io::Error::new(io::ErrorKind::InvalidData, "prepared release link leaves its candidate"));
                    }
                    if raw.is_absolute() {
                        links.push((path.strip_prefix(root).unwrap().to_path_buf(),
                            resolved.strip_prefix(root).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "prepared link path identity mismatch"))?.to_path_buf(),
                            resolved.is_dir()));
                    }
                } else if metadata.is_dir() {
                    pending.push(path);
                }
            }
        }
        Ok(links)
    }

    /// Atomically select a registered slot and retain only a verified fallback.
    /// Selecting a slot does not establish health or modify its manifest.
    pub fn promote(&self, id: &str) -> io::Result<ReleaseCatalog> {
        self.promote_inner(id, false, None)
    }

    /// Forward publication must retain an independently observed fallback.
    pub fn promote_with_rollback(&self, id: &str) -> io::Result<ReleaseCatalog> {
        self.promote_inner(id, true, None)
    }

    pub fn promote_confirmed(&self, id: &str, confirmation: Option<&str>) -> io::Result<ReleaseCatalog> {
        self.promote_inner(id, true, confirmation)
    }

    pub fn promotion_risk_confirmation(&self, id: &str) -> io::Result<Option<String>> {
        let _gate = self.lock_gate()?;
        self.release_root_unlocked(id, &self.load_unlocked()?)?;
        match self.ensure_rollback_protection_unlocked(id) {
            Ok(()) => Ok(None),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => self.promotion_confirmation_unlocked(id).map(Some),
            Err(error) => Err(error),
        }
    }

    fn promotion_confirmation_unlocked(&self, id: &str) -> io::Result<String> {
        use sha2::{Digest, Sha256};
        let mut hash = Sha256::new(); hash.update(id.as_bytes());
        for file in [&self.paths.release_pointers_file, &self.paths.config_file, &self.slot_dir(id)?.join("manifest.json")] {
            let bytes = read_regular_file_bounded(file, 1024 * 1024)?.unwrap_or_default();
            hash.update((bytes.len() as u64).to_le_bytes()); hash.update(bytes);
        }
        Ok(format!("unprotected-promotion-{:x}", hash.finalize()))
    }

    pub fn ensure_rollback_protection(&self, id: &str) -> io::Result<()> {
        let _gate = self.lock_gate()?;
        self.ensure_rollback_protection_unlocked(id)
    }

    fn ensure_rollback_protection_unlocked(&self, id: &str) -> io::Result<()> {
        let Some(bytes) = read_regular_file_bounded(&self.paths.release_pointers_file, 1024 * 1024)? else { return Ok(()); };
        let document: ReleasePointerDocument = decode_json(&bytes).map_err(|error| invalid_data(format!("{}: {error}", self.paths.release_pointers_file.display())))?;
        if document.schema_version != RELEASE_SCHEMA_VERSION { return Err(invalid_data("Unsupported release pointer schema")); }
        let (current, legacy_lkg) = (document.current_release, document.last_known_good);
        if current.as_deref() == Some(id) || (current.is_none() && legacy_lkg.is_none()) { return Ok(()); }
        if self.verified_fallback(Some(id))?.is_none() {
            return Err(io::Error::new(io::ErrorKind::WouldBlock,
                "rollback_health_required: No verified rollback target is available. Start the current Harness and complete its authenticated readiness check before switching versions. Legacy version pointers alone are not health evidence."));
        }
        Ok(())
    }

    fn promote_inner(&self, id: &str, require_rollback: bool, confirmation: Option<&str>) -> io::Result<ReleaseCatalog> {
        validate_release_id(id)?;
        let _guard = self.lock_gate()?;
        let mut catalog = self.load_unlocked()?;
        if catalog.find(id).is_none() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("release {id} was not found"),
            ));
        }
        if require_rollback {
            if let Err(error) = self.ensure_rollback_protection_unlocked(id) {
                if error.kind() != io::ErrorKind::WouldBlock || confirmation != Some(self.promotion_confirmation_unlocked(id)?.as_str()) { return Err(error); }
            }
        }
        if catalog.current_release.as_deref() != Some(id) {
            let previous = catalog.current_release.clone();
            catalog.current_release = Some(id.to_owned());
            catalog.last_known_good = self.verified_fallback(Some(id))?;
            catalog.normalize()?;
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
        self.remove_with_digest(id, None)
    }

    pub(crate) fn remove_with_digest(&self, id: &str, digest: Option<&str>) -> io::Result<ReleaseCatalog> {
        validate_release_id(id)?;
        let _guard = self.lock_gate()?;
        let mut catalog = self.load_unlocked()?;
        let pending = self.pending_cleanup(id)?;
        if catalog.find(id).is_none() && pending.is_none() {
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
        terminal_lease::ensure_release_idle(&self.paths, id)?;
        ensure_configuration_paths_preserved(&self.paths, &slot_dir)?;
        ensure_harness_homes_preserved(&self.paths.root, &slot_dir)?;
        if let Some(journal) = CheckpointRestoreJournalStore::new(self.paths.clone()).load()? {
            let intent = journal.intent;
            if [intent.previous_current_release, intent.previous_last_known_good,
                intent.target_current_release, intent.target_last_known_good]
                .iter().any(|reference| reference.as_deref() == Some(id)) {
                return Err(io::Error::new(io::ErrorKind::ResourceBusy,
                    format!("release {id} is referenced by an unfinished checkpoint restore")));
            }
        }
        // External ownership survives even a crash after deleting the final
        // in-slot metadata. A pending target is excluded from the catalog.
        let manifest = pending.or_else(|| catalog.find(id).cloned()).ok_or_else(|| invalid_data("Cleanup manifest missing"))?;
        let ownership = maintenance::CleanupOwnership::begin_bound(&self.paths, "release", id, serde_json::to_value(&manifest)?, digest)?;
        ownership.delete(&self.paths)?;
        catalog.releases.retain(|item| item.id != id);
        catalog.normalize()?;
        Ok(catalog)
    }

    /// A failed deletion retains ownership for explicit retry, without
    /// representing the partially removed slot as an installed version.
    pub fn pending_cleanup(&self, id: &str) -> io::Result<Option<ReleaseManifest>> {
        validate_release_id(id)?;
        if let Some(ownership) = maintenance::CleanupOwnership::load(&self.paths, "release", id)? {
            let manifest: ReleaseManifest = serde_json::from_value(ownership.manifest)?;
            validate_release_manifest(&manifest)?;
            if manifest.id != id { return Err(invalid_data("Pending release cleanup does not match its slot")); }
            return Ok(Some(manifest));
        }
        let slot = self.slot_dir(id)?;
        let marker = slot.join(".nexus-cleanup.json");
        let metadata = match fs::symlink_metadata(&marker) {
            Ok(value) => value, Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None), Err(error) => return Err(error),
        };
        if path_is_reparse(&metadata) || !metadata.is_file() || path_is_reparse(&fs::symlink_metadata(&slot)?) {
            return Err(invalid_data("Unsafe pending release cleanup record"));
        }
        let mut bytes = Vec::new();
        use std::io::Read;
        fs::File::open(&marker)?.take(256 * 1024 + 1).read_to_end(&mut bytes)?;
        if bytes.len() > 256 * 1024 { return Err(invalid_data("Pending release cleanup record is too large")); }
        let manifest: ReleaseManifest = decode_json(&bytes).map_err(|error| invalid_data(format!("{}: {error}", marker.display())))?;
        validate_release_manifest(&manifest)?;
        if manifest.id != id { return Err(invalid_data("Pending release cleanup does not match its slot")); }
        Ok(Some(manifest))
    }

/// Keep the profile module link farm (`~/.dsh/profiles/node_modules`)
/// pointing at the running slot's own workspace packages. The Harness boot
/// maintains the same farm itself, but a crashed boot can leave links from a
/// previous slot behind; repointing before spawn makes plugin resolution
/// deterministic for the selected release. Returns repaired links or an error;
/// real directories and indirect farm parents are never replaced.
pub fn heal_module_farm(dsh_home: &Path, slot_root: &Path) -> io::Result<usize> {
    let farm = dsh_home.join("profiles").join("node_modules");
    // A cold installation can finish before Harness has ever created its home.
    Self::ensure_module_directory(dsh_home)?;
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
    if !path.is_absolute() || path.components().any(|component| matches!(component, std::path::Component::ParentDir)) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "module farm directory must be absolute without parent traversal"));
    }
    // Walk from the volume root before creating anything: a linked ancestor
    // must not redirect a newly selected home into somebody else's directory.
    let ancestors: Vec<_> = path.ancestors().collect();
    for directory in ancestors.into_iter().rev() {
        match fs::symlink_metadata(directory) {
            Ok(metadata) if metadata.is_dir() && !Self::is_module_link(&metadata) => {}
            Ok(_) => return Err(io::Error::other(format!("module farm parent is not a real directory: {}", directory.display()))),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                match fs::create_dir(directory) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(error),
                }
                let metadata = fs::symlink_metadata(directory)?;
                if !metadata.is_dir() || Self::is_module_link(&metadata) {
                    return Err(io::Error::other(format!("module farm directory changed during creation: {}", directory.display())));
                }
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
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
    use std::os::windows::{ffi::OsStrExt, fs::OpenOptionsExt, io::AsRawHandle};
    use windows_sys::Win32::{
        Storage::FileSystem::{FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE, MAXIMUM_REPARSE_DATA_BUFFER_SIZE},
        System::{IO::DeviceIoControl, Ioctl::FSCTL_SET_REPARSE_POINT},
    };
    let target = fs::canonicalize(target)?;
    if !target.is_dir() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "junction target must be a directory"));
    }
    let target_wide: Vec<u16> = target.as_os_str().encode_wide().collect();
    let prefix: Vec<u16> = r"\\?\".encode_utf16().collect();
    if !target_wide.starts_with(&prefix) || target_wide.get(5) != Some(&(b':' as u16)) {
        return Err(io::Error::new(io::ErrorKind::Unsupported, "junction target must be on a local Windows volume"));
    }
    let substitute: Vec<u16> = r"\??\".encode_utf16().chain(target_wide[4..].iter().copied()).collect();
    let print: Vec<u16> = target_wide[4..].to_vec();
    let data_length = 8 + (substitute.len() + print.len() + 2) * 2;
    if data_length + 8 > MAXIMUM_REPARSE_DATA_BUFFER_SIZE as usize {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "junction target is too long"));
    }
    // MOUNT_POINT reparse data has an 8-byte common header, four u16 name
    // offsets/lengths, then two NUL-terminated UTF-16 paths. No shell parses it.
    let mut buffer = Vec::with_capacity(data_length + 8);
    buffer.extend_from_slice(&0xa0000003u32.to_le_bytes()); // IO_REPARSE_TAG_MOUNT_POINT
    for value in [data_length as u16, 0, 0, (substitute.len() * 2) as u16,
        ((substitute.len() + 1) * 2) as u16, (print.len() * 2) as u16] {
        buffer.extend_from_slice(&value.to_le_bytes());
    }
    for value in substitute.into_iter().chain([0]).chain(print).chain([0]) {
        buffer.extend_from_slice(&value.to_le_bytes());
    }
    fs::create_dir(link)?;
    let result = (|| {
        let handle = fs::OpenOptions::new().write(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
            .open(link)?;
        let mut returned = 0;
        let succeeded = unsafe { DeviceIoControl(handle.as_raw_handle(), FSCTL_SET_REPARSE_POINT,
            buffer.as_ptr().cast(), buffer.len() as u32, std::ptr::null_mut(), 0,
            &mut returned, std::ptr::null_mut()) };
        if succeeded == 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
    })();
    if result.is_err() { let _ = fs::remove_dir(link); }
    result
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
            catalog.current_release = restored;
            catalog.last_known_good = self.verified_fallback(catalog.current_release.as_deref())?;
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
        self.load_unlocked()
    }

    /// Transaction evidence is the durable tuple, not the health-filtered UI
    /// view. Old journals must settle without granting old pointers health.
    pub fn stored_release_pointers(&self) -> io::Result<(Option<String>, Option<String>)> {
        let _gate = self.lock_gate()?;
        let Some(bytes) = read_regular_file_bounded(&self.paths.release_pointers_file, 1024 * 1024)? else { return Ok((None, None)); };
        let document: ReleasePointerDocument = decode_json(&bytes).map_err(|error| invalid_data(format!("{}: {error}", self.paths.release_pointers_file.display())))?;
        if document.schema_version != RELEASE_SCHEMA_VERSION { return Err(invalid_data("Unsupported release pointer schema")); }
        let catalog = self.load_unlocked()?;
        for id in [&document.current_release, &document.last_known_good].into_iter().flatten() {
            self.release_root_unlocked(id, &catalog)?;
        }
        Ok((document.current_release, document.last_known_good))
    }

    /// Select the verified fallback without starting Harness. The previous
    /// selection only remains a fallback if it has matching health evidence.
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
        catalog.current_release = Some(last_known_good.clone());
        catalog.last_known_good = self.verified_fallback(Some(&last_known_good))?;
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
                healthy: Vec::new(),
            }
        };
        if pointers.schema_version != RELEASE_SCHEMA_VERSION { return Err(invalid_data("Unsupported release pointer schema")); }

        let mut verified_lkg = None;
        if let Some(id) = pointers.last_known_good.as_deref() {
            for record in pointers.healthy.iter().filter(|record| record.release_id == id) {
                if self.health_evidence_matches(record)? { verified_lkg = Some(id.to_owned()); break; }
            }
        }
        let mut catalog = ReleaseCatalog {
            unavailable_selections: Vec::new(),
            schema_version: RELEASE_SCHEMA_VERSION,
            current_release: pointers.current_release,
            last_known_good: verified_lkg,
            releases: Vec::new(),
        };
        if self.paths.releases_dir.exists() {
            for entry in fs::read_dir(&self.paths.releases_dir)? {
                let entry = entry?;
                if !entry.file_type()?.is_dir() {
                    continue;
                }
                let slot_id = entry.file_name().to_string_lossy().into_owned();
                if is_valid_release_id(&slot_id) {
                    let ownership_path = maintenance::CleanupOwnership::record_path(&self.paths, "release", &slot_id)?;
                    match fs::symlink_metadata(ownership_path) {
                        Ok(_) => continue,
                        Err(error) if error.kind() == io::ErrorKind::NotFound => (),
                        Err(_) => continue,
                    }
                }
                if fs::symlink_metadata(entry.path().join(".nexus-cleanup.json")).is_ok() {
                    // Partial cleanup is unavailable, but must not poison the
                    // healthy slots or current selection in this catalog.
                    continue;
                }
                let manifest_path = entry.path().join("manifest.json");
                if !manifest_path.exists() {
                    // A partially prepared slot is not selectable until its
                    // immutable manifest is published.
                    continue;
                }
                let bytes = fs::read(&manifest_path)?;
                let manifest: ReleaseManifest = decode_json(&bytes).map_err(|error| invalid_data(format!("{}: {error}", manifest_path.display())))?;
                validate_release_manifest(&manifest)?;
                if manifest.id != slot_id {
                    return Err(invalid_data("release directory does not match manifest id"));
                }
                catalog.releases.push(manifest);
            }
        }
        // A deleted/incomplete slot must not take the control plane offline.
        // Keep the pointer document and residual files untouched for diagnosis;
        // only exclude unresolved selections from this effective catalog.
        for pointer in [&mut catalog.current_release, &mut catalog.last_known_good] {
            if let Some(id) = pointer.as_deref() {
                validate_release_id(id)?;
                if !catalog.releases.iter().any(|release| release.id == id) {
                    catalog.unavailable_selections.push(id.to_owned());
                    *pointer = None;
                }
            }
        }
        catalog.unavailable_selections.sort();
        catalog.unavailable_selections.dedup();
        catalog.normalize()?;
        Ok(catalog)
    }

    fn write_pointers(&self, catalog: &ReleaseCatalog) -> io::Result<()> {
        let pointers = ReleasePointerDocument {
            schema_version: RELEASE_SCHEMA_VERSION,
            current_release: catalog.current_release.clone(),
            last_known_good: catalog.last_known_good.clone(),
            healthy: self.health_records()?,
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
    write_atomic_bytes(root, destination, &bytes, false)
}

pub fn write_private_json_atomic<T: Serialize>(root: &Path, destination: &Path, value: &T) -> io::Result<()> {
    let bytes = encode_json(value).map_err(invalid_data)?;
    write_private_bytes_atomic(root, destination, &bytes)
}

pub fn write_private_bytes_atomic(root: &Path, destination: &Path, bytes: &[u8]) -> io::Result<()> {
    write_atomic_bytes(root, destination, bytes, true)
}

/// Read existing ordinary files without following reparse points or allocating
/// from an untrusted size. Missing files retain their normal optional meaning.
pub fn read_regular_file_bounded(path: &Path, limit: u64) -> io::Result<Option<Vec<u8>>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !metadata.is_file() || path_is_reparse(&metadata) || metadata.len() > limit {
        return Err(invalid_data("Configuration or backup must be an ordinary bounded file"));
    }
    let mut options = fs::OpenOptions::new(); options.read(true);
    #[cfg(windows)] { use std::os::windows::fs::OpenOptionsExt; options.custom_flags(0x00200000); }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || path_is_reparse(&metadata) { return Err(invalid_data("Configuration or backup file identity changed")); }
    use io::Read;
    let mut bytes = Vec::new(); file.take(limit.saturating_add(1)).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit { return Err(invalid_data("Configuration or backup is too large")); }
    Ok(Some(bytes))
}

fn write_atomic_bytes(root: &Path, destination: &Path, bytes: &[u8], private: bool) -> io::Result<()> {
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
        let mut file = if private { nexus_private_file::create_new_private(&temp_path)? } else {
            fs::OpenOptions::new().write(true).create_new(true).open(&temp_path)?
        };
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
#[serde(deny_unknown_fields)]
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
/// Correlation metadata only. Never add arguments, URLs or environment values.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OperationContext {
    pub build_id: Option<String>,
    pub version: Option<String>,
    pub profile: Option<String>,
    pub config_revision: Option<String>,
    pub run_id: Option<String>,
}

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
    #[serde(default)]
    pub context: Option<OperationContext>,
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
            context: None,
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
    if metadata.schema_version != RUNTIME_SCHEMA_VERSION { return Err(invalid_data("Unsupported runtime metadata schema")); }
    read_runtime_metadata(paths)?;
    paths.ensure_directories()?;
    write_json_atomic(&paths.root, &paths.state_file, metadata)
}

pub fn read_runtime_metadata(paths: &NexusPaths) -> io::Result<Option<NexusRuntimeMetadata>> {
    let Some(bytes) = read_regular_file_bounded(&paths.state_file, 1024 * 1024)? else { return Ok(None); };
    let metadata: NexusRuntimeMetadata = decode_json(&bytes).map_err(invalid_data)?;
    if metadata.schema_version != RUNTIME_SCHEMA_VERSION { return Err(invalid_data("Unsupported runtime metadata schema")); }
    Ok(Some(metadata))
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
#[serde(deny_unknown_fields)]
struct UpdateStateDocument {
    #[serde(default = "default_update_schema", deserialize_with = "deserialize_record_schema")]
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
        let document: UpdateStateDocument = decode_json(&bytes).map_err(|error| invalid_data(format!("{}: {error}", self.paths.update_state_file.display())))?;
        Ok(document.update)
    }

    pub fn write(&self, update: &UpdateRuntimeInfo) -> io::Result<()> {
        let _guard = self.lock_gate()?;
        if let Some(bytes) = read_regular_file_bounded(&self.paths.update_state_file, 4 * 1024 * 1024)? { let _: UpdateStateDocument = decode_json(&bytes).map_err(invalid_data)?; }
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
        let document: UpdateStateDocument = decode_json(&bytes).map_err(|error| invalid_data(format!("{}: {error}", self.paths.update_state_file.display())))?;
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
#[serde(deny_unknown_fields)]
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
        self.list_with_warnings().map(|(bundles,_)|bundles)
    }
    pub fn list_with_warnings(&self) -> io::Result<(Vec<DiagnosticsBundle>,Vec<serde_json::Value>)> {
        let _guard = self.lock_gate()?;
        match fs::symlink_metadata(&self.paths.diagnostics_dir) {Ok(meta) if meta.is_dir()&&!path_is_reparse(&meta)=>{},Ok(_)=>return Err(invalid_data("Diagnostic directory is not ordinary")),Err(error) if error.kind()==io::ErrorKind::NotFound=>return Ok((vec![],vec![])),Err(error)=>return Err(error)}
        let mut bundles = Vec::new();
        let mut warnings=Vec::new();
        for entry in fs::read_dir(&self.paths.diagnostics_dir)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            if let Some(name) = entry.file_name().to_str().filter(|name| is_valid_release_id(name)) {
                let ownership = maintenance::CleanupOwnership::record_path(&self.paths, "diagnostic", name)?;
                if fs::symlink_metadata(ownership).is_ok() { continue; }
            }
            let manifest_path = entry.path().join("diagnostics.json");
            let document = (|| -> io::Result<DiagnosticsDocument> {
                let bytes=read_regular_file_bounded(&manifest_path,4*1024*1024)?.ok_or_else(||invalid_data("diagnostics manifest missing"))?;
                let document:DiagnosticsDocument=decode_json(&bytes).map_err(invalid_data)?;
                if document.schema_version!=DIAGNOSTICS_SCHEMA_VERSION || entry.file_name().to_str()!=Some(document.bundle.id.as_str()) {return Err(invalid_data("unsupported diagnostics identity or version"));}
                validate_diagnostics_bundle(&document.bundle)?;Ok(document)
            })();
            // A damaged historical bundle does not hide independent healthy evidence.
            let document=match document {Ok(value)=>value,Err(_)=>{if warnings.len()<64{warnings.push(serde_json::json!({"bundle_id":entry.file_name().to_string_lossy(),"reason":"Unreadable or unsupported diagnostic record was preserved"}));}continue;}};
            bundles.push(document.bundle);
        }
        bundles.sort_by(|left, right| {
            right
                .created_at_unix
                .cmp(&left.created_at_unix)
                .then_with(|| right.id.cmp(&left.id))
        });
        bundles.truncate(MAX_DIAGNOSTICS_BUNDLES);
        Ok((bundles,warnings))
    }

    pub fn collect(&self, note: Option<String>) -> io::Result<DiagnosticsBundle> {
        self.collect_with_context(note, None)
    }

    /// Context must use the public, credential-redacted configuration contract.
    pub fn collect_with_context(&self, note: Option<String>, context: Option<serde_json::Value>) -> io::Result<DiagnosticsBundle> {
        validate_optional_diagnostics_text(note.as_deref(), "diagnostics note")?;
        let note = note.map(|value| String::from_utf8_lossy(&redact_diagnostics_payload(value.as_bytes()).0).trim_end().to_owned());
        let _guard = self.lock_gate()?;
        self.paths.ensure_directories()?;
        let id = format!("diag-{}", unix_time_nanos());
        validate_release_id(&id)?;
        let bundle_dir = self.paths.diagnostics_dir.join(&id);
        let files_dir = bundle_dir.join("files");
        fs::create_dir(&bundle_dir)?;
        fs::create_dir(&files_dir)?;

        let mut files = Vec::new();
        let mut portable_files = serde_json::Map::new();
        if let Some(context) = context {
            let bytes = serde_json::to_vec_pretty(&context)?;
            let (mut payload, mut redacted) = if bytes.len() <= MAX_DIAGNOSTICS_FILE_BYTES {
                redact_diagnostics_payload(&bytes)
            } else { (b"Configuration context exceeded the diagnostic bound".to_vec(), true) };
            let context_truncated = bytes.len() > MAX_DIAGNOSTICS_FILE_BYTES || payload.len() > MAX_DIAGNOSTICS_FILE_BYTES;
            if payload.len() > MAX_DIAGNOSTICS_FILE_BYTES { payload = b"Redacted configuration exceeded the diagnostic bound".to_vec(); redacted = true; }
            write_diagnostics_file(&files_dir.join("effective-config.json"), &payload)?;
            portable_files.insert("effective-config.json".into(), serde_json::json!(String::from_utf8_lossy(&payload)));
            files.push(DiagnosticsFile { name: "effective-config.json".into(), bytes: payload.len() as u64, redacted, truncated: context_truncated });
        }

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
        for relative in ["install-operation.json", "cold-operation.json", "cold-publication.json", "run/harness-log-session.json", "run/checkpoint-restore.json", "run/harness-recovery.json", "run/harness-effective.json",
            "run/last-capture.json", "run/cleanup-result.json", "compatibility/latest.json", "canary/latest.json", "run/agent.json", "run/launcher-agent.json"] {
            let path = self.paths.root.join(relative);
            if is_regular_diagnostics_file(&path) { sources.push((path, relative.into(), MAX_DIAGNOSTICS_FILE_BYTES)); }
        }
        if let Ok(executable) = std::env::current_exe() {
            if let Some(parent) = executable.parent() {
                let identity = parent.join("release-identity.json");
                if is_regular_diagnostics_file(&identity) { sources.push((identity, "release-identity.json".into(), MAX_DIAGNOSTICS_FILE_BYTES)); }
            }
        }
        if self.paths.logs_dir.is_dir() {
            let session = HarnessLogSessionStore::new(self.paths.clone()).read().ok().flatten();
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
            let priority = |path: &Path| {
                let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
                session.as_ref().is_some_and(|s| name == s.stdout_log_name || name == s.stderr_log_name)
            };
            logs.sort_by(|left, right| priority(&right.0).cmp(&priority(&left.0))
                .then_with(|| right.0.metadata().and_then(|m| m.modified()).ok().cmp(&left.0.metadata().and_then(|m| m.modified()).ok()))
                .then_with(|| left.1.cmp(&right.1)));
            sources.extend(logs);
        }
        sources.truncate(MAX_DIAGNOSTICS_FILES.saturating_sub(files.len()));

        for (source, relative, limit) in sources {
            let tail = relative.starts_with("logs");
            let raw = if tail { read_diagnostics_tail(&source, limit)? } else { read_diagnostics_file(&source, limit)? };
            let mut truncated = raw.len() > limit;
            // Drop a partial leading line, then handle known multiline secret
            // boundaries. Unmarked arbitrary text cannot be classified as secret.
            let bounded = if truncated && tail { raw.iter().position(|b| *b == b'\n').map_or(&raw[raw.len()..], |i| &raw[i + 1..]) }
                else if truncated { &raw[..limit] } else { &raw[..] };
            let (bounded, boundary_redacted) = if truncated && tail {
                omit_truncated_private_key_prefix(bounded)
            } else { (bounded, false) };
            let (mut payload, redacted) = redact_diagnostics_payload(bounded);
            let mut redacted = redacted || boundary_redacted;
            if payload.len() > limit { payload = b"Redacted content exceeded the diagnostic bound".to_vec(); redacted = true; truncated = true; }
            let destination = files_dir.join(&relative);
            let Some(parent) = destination.parent() else {
                continue;
            };
            fs::create_dir_all(parent)?;
            write_diagnostics_file(&destination, &payload)?;
            portable_files.insert(relative.to_string_lossy().replace('\\', "/"), serde_json::json!(String::from_utf8_lossy(&payload)));
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
        // Export the bytes just collected, never re-read caller-controlled file
        // paths. This single ordinary JSON file is portable and needs no tools.
        write_json_atomic(&bundle_dir, &bundle_dir.join("export.json"), &serde_json::json!({
            "format": "nexus-diagnostics", "schema_version": 1, "bundle": &bundle, "files": portable_files,
        }))?;
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
            Some("export.json") => directory.join("export.json"),
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
    if let Ok(mut value @ (serde_json::Value::Object(_) | serde_json::Value::Array(_))) = serde_json::from_str::<serde_json::Value>(text) {
        let changed = redact_diagnostic_json(&mut value);
        if changed { return (serde_json::to_vec_pretty(&value).unwrap_or_else(|_| b"[REDACTED]".to_vec()), true); }
    }
    redact_diagnostic_text(text)
}

fn redact_diagnostic_json(value: &mut serde_json::Value) -> bool {
    let mut changed = false;
    match value {
        serde_json::Value::Object(object) => for (key, value) in object {
            if diagnostics_line_is_sensitive(&format!("{key}:")) || key.eq_ignore_ascii_case("key") || key == "system_prompt"
                || matches!(key.as_str(), "args" | "build_args" | "verify_args") {
                *value = serde_json::json!("[REDACTED]"); changed = true;
            } else { changed |= redact_diagnostic_json(value); }
        },
        serde_json::Value::Array(values) => for value in values { changed |= redact_diagnostic_json(value); },
        serde_json::Value::String(value) => {
            let (bytes, redacted) = redact_diagnostic_text(value);
            if redacted { *value = String::from_utf8_lossy(&bytes).into_owned(); changed = true; }
        },
        _ => {},
    }
    changed
}

fn redact_diagnostic_text(text: &str) -> (Vec<u8>, bool) {
    let mut redacted = false;
    let mut private_key = false;
    let mut continuation = false;
    let mut sensitive_depth = 0isize;
    let mut output = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let pem_start = line.contains("-----BEGIN") && line.contains("PRIVATE KEY-----");
        let sensitive = diagnostics_line_is_sensitive(line);
        if private_key || pem_start || continuation || sensitive || sensitive_depth > 0 {
            output.push_str("[REDACTED]\n");
            redacted = true;
            private_key = (private_key || pem_start) && !line.contains("-----END");
            continuation = sensitive && line.trim_end().ends_with(':');
            if sensitive_depth > 0 || (sensitive && line.trim_end().ends_with(['{', '['])) {
                sensitive_depth = (sensitive_depth + diagnostic_container_delta(line)).max(0);
            }
        } else {
            // Query strings, fragments and URL userinfo are unnecessary for
            // diagnostics, even when a provider uses an unrecognized key name.
            for word in line.split_inclusive(char::is_whitespace) {
                let trimmed = word.trim_end();
                if let Some(scheme) = trimmed.find("://") {
                    let end = trimmed[scheme + 3..].find(['?', '#']).map_or(trimmed.len(), |i| scheme + 3 + i);
                    let base = &trimmed[..end];
                    let start = scheme + 3;
                    if start <= base.len() {
                        let authority_end = base[start..].find('/').map_or(base.len(), |i| start + i);
                        let authority = &base[start..authority_end];
                        let host = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
                        let safe = format!("{}{}{}", &base[..start], host, &base[authority_end..]);
                        redacted |= safe != trimmed;
                        output.push_str(&safe); output.push_str(&word[trimmed.len()..]);
                        continue;
                    }
                }
                output.push_str(word);
            }
        }
    }
    (output.into_bytes(), redacted)
}

fn diagnostic_container_delta(line: &str) -> isize {
    let (mut quoted, mut escaped, mut depth) = (false, false, 0);
    for c in line.chars() {
        if escaped { escaped = false; continue; }
        if quoted && c == '\\' { escaped = true; continue; }
        if c == '"' { quoted = !quoted; continue; }
        if !quoted { match c { '{' | '[' => depth += 1, '}' | ']' => depth -= 1, _ => {} } }
    }
    depth
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

fn read_diagnostics_tail(path: &Path, limit: usize) -> io::Result<Vec<u8>> {
    use io::{Read, Seek, SeekFrom};
    let mut file = fs::File::open(path)?;
    let start = file.metadata()?.len().saturating_sub(limit as u64 + 1);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// A tail may start inside a PEM body after its BEGIN marker was discarded.
/// When its first key boundary is END, omit the entire ambiguous prefix. Other
/// unlabelled fragments remain outside the guarantees of marker-based redaction.
pub fn omit_truncated_private_key_prefix(bytes: &[u8]) -> (&[u8], bool) {
    let mut offset = 0;
    for line in bytes.split_inclusive(|byte| *byte == b'\n') {
        offset += line.len();
        let text = String::from_utf8_lossy(line);
        if text.contains("PRIVATE KEY-----") {
            if text.contains("-----BEGIN") { return (bytes, false); }
            if text.contains("-----END") { return (&bytes[offset..], true); }
        }
    }
    (bytes, false)
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
    fn data_root_identity_does_not_merge_distinct_directories() {
        #[cfg(not(target_os = "macos"))]
        use std::{ffi::OsString, os::unix::ffi::OsStringExt};

        let parent = unique_test_root("non-utf8-root-identity");
        #[cfg(not(target_os = "macos"))]
        let first = parent.join(OsString::from_vec(vec![b'r', 0x80]));
        #[cfg(not(target_os = "macos"))]
        let second = parent.join(OsString::from_vec(vec![b'r', 0x81]));
        // APFS rejects non-UTF8 names before identity lookup. Use distinct
        // valid Unicode names there, while retaining byte-name coverage elsewhere.
        #[cfg(target_os = "macos")]
        let (first, second) = (parent.join("目录甲"), parent.join("目录乙"));
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
        { let result = store.promote("harness-a").expect("alpha promoted"); mark_fixture_healthy(&store, "harness-a"); result };

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
    fn release_removal_protects_all_pending_restore_references() {
        for index in 0..4 {
            let root = unique_test_root("release-restore-protection");
            let paths = NexusPaths::from_root(root.clone());
            let releases = ReleaseStore::new(paths.clone());
            releases.register("referenced", "1", None, None).unwrap();
            let profiles = ProfileStore::new(paths.clone()).load().unwrap();
            let mut intent = CheckpointRestoreIntent {
                checkpoint_id: "checkpoint-a".into(), previous_profiles: profiles.clone(),
                target_profiles: profiles, previous_current_release: None,
                previous_last_known_good: None, target_current_release: None,
                target_last_known_good: None, snapshot: None,
            };
            let fields = [&mut intent.previous_current_release, &mut intent.previous_last_known_good,
                &mut intent.target_current_release, &mut intent.target_last_known_good];
            *fields.into_iter().nth(index).unwrap() = Some("referenced".into());
            let journal = CheckpointRestoreJournalStore::new(paths.clone());
            journal.begin(intent.clone()).unwrap();
            assert_eq!(releases.remove("referenced").unwrap_err().kind(), std::io::ErrorKind::ResourceBusy);
            assert!(paths.releases_dir.join("referenced").exists());
            journal.clear(CheckpointRestorePhase::Prepared, &intent).unwrap();
            fs::write(paths.run_dir.join("checkpoint-restore.json"), "broken").unwrap();
            assert!(releases.remove("referenced").is_err());
            fs::remove_file(paths.run_dir.join("checkpoint-restore.json")).unwrap();
            releases.remove("referenced").unwrap();
            fs::remove_dir_all(root).unwrap();
        }
    }

    fn mark_fixture_healthy(store: &super::ReleaseStore, id: &str) {
        let entry = store.slot_dir(id).unwrap().join("health-entry.js");
        fs::write(&entry, "fixture entry").unwrap();
        let mut evidence = store.healthy_launch_candidate(id, &entry, "web", "fixture-config".into()).unwrap();
        evidence.run_id = format!("run-{id}"); evidence.generation = 1;
        store.record_healthy_release(evidence).unwrap();
    }
    #[test]
    #[cfg(windows)]
    fn locked_health_evidence_is_retryable_and_does_not_erase_fallback() {
        use std::os::windows::fs::OpenOptionsExt;
        let root=unique_test_root("locked-health-evidence");let paths=NexusPaths::from_root(root.clone());
        let store=ReleaseStore::new(paths.clone());
        for id in ["old","new"] { store.register(id,"1",None,None).unwrap(); }
        store.promote("old").unwrap();mark_fixture_healthy(&store,"old");store.promote("new").unwrap();
        let bytes=fs::read(&paths.release_pointers_file).unwrap();
        let locked=fs::OpenOptions::new().read(true).share_mode(0).open(store.slot_dir("old").unwrap().join("health-entry.js")).unwrap();
        assert!(store.load().is_err());assert!(store.rollback().is_err());
        assert!(store.ensure_rollback_protection("old").is_err());
        assert_eq!(fs::read(&paths.release_pointers_file).unwrap(),bytes);
        drop(locked);
        assert_eq!(store.rollback().unwrap().current_release.as_deref(),Some("old"));
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn prepared_registration_failure_remains_visible_for_cleanup_and_retry() {
        for cut in [1,2] {
            let root=unique_test_root("prepared-registration-cut");let paths=NexusPaths::from_root(root.clone());paths.ensure_directories().unwrap();
            let store=ReleaseStore::new(paths.clone());let candidate=paths.downloads_dir.join("candidate");
            fs::create_dir(&candidate).unwrap();fs::write(candidate.join("payload"),b"ready").unwrap();
            store.prepared_failure_cut.store(cut,std::sync::atomic::Ordering::SeqCst);
            assert!(store.register_prepared(&candidate,"slot","1",None,None).is_err());
            assert!(store.load().unwrap().find("slot").is_none());
            assert!(store.pending_cleanup("slot").unwrap().is_some());
            let slot=store.slot_dir("slot").unwrap();assert_eq!(fs::read(slot.join("payload")).unwrap(),b"ready");
            fs::remove_dir_all(slot).unwrap();
            store.prepared_failure_cut.store(0,std::sync::atomic::Ordering::SeqCst);
            fs::create_dir(&candidate).unwrap();fs::write(candidate.join("payload"),b"retry").unwrap();
            assert!(store.register_prepared(&candidate,"slot","1",None,None).unwrap().find("slot").is_some());
            assert!(store.pending_cleanup("slot").unwrap().is_none());
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    #[cfg(windows)]
    fn prepared_manifest_publication_has_no_post_commit_cleanup_window() {
        use std::os::windows::fs::OpenOptionsExt;
        let root=unique_test_root("prepared-manifest-atomic");fs::create_dir_all(&root).unwrap();
        let marker=root.join(".nexus-cleanup.json");fs::write(&marker,b"prepared manifest").unwrap();
        let lock=fs::OpenOptions::new().read(true).share_mode(1).open(&marker).unwrap();
        assert!(ReleaseStore::publish_prepared_manifest(&root).is_err());
        assert!(!root.join("manifest.json").exists());assert!(marker.exists());
        drop(lock);
        ReleaseStore::publish_prepared_manifest(&root).unwrap();
        assert_eq!(fs::read(root.join("manifest.json")).unwrap(),b"prepared manifest");
        assert!(!marker.exists());fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn legacy_upgrade_requires_real_health_before_forward_publication() {
        let root = unique_test_root("legacy-rollback-protection");
        let paths = NexusPaths::from_root(root.clone());
        let store = super::ReleaseStore::new(paths.clone());
        for id in ["old", "legacy-lkg", "new"] { store.register(id, "1", None, None).unwrap(); }
        store.promote_with_rollback("old").unwrap();
        fs::write(&paths.release_pointers_file, br#"{"schema_version":1,"current_release":"old","last_known_good":"legacy-lkg"}"#).unwrap();
        let before = fs::read(&paths.release_pointers_file).unwrap();
        assert!(store.load().unwrap().last_known_good.is_none());
        assert!(store.promote_with_rollback("new").unwrap_err().to_string().contains("rollback_health_required"));
        assert_eq!(fs::read(&paths.release_pointers_file).unwrap(), before);
        store.promote_with_rollback("old").unwrap();
        mark_fixture_healthy(&store, "old");
        store.ensure_rollback_protection("new").unwrap();
        fs::write(store.slot_dir("old").unwrap().join("health-entry.js"), "changed after admission").unwrap();
        assert!(store.promote_with_rollback("new").is_err());
        mark_fixture_healthy(&store, "old");
        store.promote_with_rollback("new").unwrap();
        assert_eq!(store.rollback().unwrap().current_release.as_deref(), Some("old"));
        fs::write(&paths.release_pointers_file, br#"{"schema_version":1,"current_release":"missing","last_known_good":"legacy-lkg"}"#).unwrap();
        assert!(store.load().unwrap().current_release.is_none());
        assert!(store.promote_with_rollback("new").is_err(), "missing selection is not a first install");
        let confirmation = store.promotion_risk_confirmation("new").unwrap().unwrap();
        assert!(store.promote_confirmed("legacy-lkg", Some(&confirmation)).is_err(), "confirmation binds target");
        fs::write(&paths.config_file, b"{}").unwrap();
        assert!(store.promote_confirmed("new", Some(&confirmation)).is_err(), "confirmation binds config revision");
        let confirmation = store.promotion_risk_confirmation("new").unwrap().unwrap();
        store.promote_confirmed("new", Some(&confirmation)).unwrap();
        assert!(store.load().unwrap().last_known_good.is_none(), "manual consent is not health evidence");
        assert!(store.promote_confirmed("old", Some(&confirmation)).is_err());
        store.restore_release_pointers(Some("old"), Some("legacy-lkg")).unwrap();
        assert_eq!(store.stored_release_pointers().unwrap(), (Some("old".into()), Some("legacy-lkg".into())));
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn release_health_requires_observation_and_unchanged_entry() {
        let root = unique_test_root("verified-release");
        let store = super::ReleaseStore::new(NexusPaths::from_root(root.clone()));
        for id in ["good", "unstarted", "failed"] { store.register(id, "1", None, None).unwrap(); }
        store.promote("good").unwrap();
        store.promote("unstarted").unwrap();
        assert!(store.load().unwrap().last_known_good.is_none());
        assert!(store.rollback().is_err());
        store.promote("good").unwrap(); mark_fixture_healthy(&store, "good");
        store.promote("unstarted").unwrap();
        store.promote("failed").unwrap();
        assert_eq!(store.load().unwrap().last_known_good.as_deref(), Some("good"));
        store.rollback().unwrap();
        assert!(store.load().unwrap().last_known_good.is_none(), "failed release cannot become a fallback");
        store.promote("failed").unwrap();
        fs::write(store.slot_dir("good").unwrap().join("health-entry.js"), "changed").unwrap();
        assert!(store.load().unwrap().last_known_good.is_none());
        assert!(store.rollback().is_err());
        fs::remove_dir_all(root).unwrap();
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

        let promoted_alpha = { let result = store.promote("harness-alpha5").expect("alpha promotes"); mark_fixture_healthy(&store, "harness-alpha5"); result };
        assert_eq!(
            promoted_alpha.current_release.as_deref(),
            Some("harness-alpha5")
        );
        assert!(promoted_alpha.last_known_good.is_none());

        let promoted_rc = { let result = store.promote("harness-rc1").expect("rc promotes"); mark_fixture_healthy(&store, "harness-rc1"); result };
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
    fn missing_release_selections_remain_diagnostic_and_allow_reinstallation() {
        let root = unique_test_root("missing-release-selection");
        let paths = NexusPaths::from_root(root.clone());
        let store = super::ReleaseStore::new(paths.clone());
        store.register("missing-current", "1", None, None).unwrap();
        store.promote("missing-current").unwrap();
        let pointer = fs::read(&paths.release_pointers_file).unwrap();
        let slot = paths.releases_dir.join("missing-current");
        fs::remove_file(slot.join("manifest.json")).unwrap();
        fs::create_dir(slot.join("node_modules")).unwrap();
        let catalog = store.load().unwrap();
        assert!(catalog.current_release.is_none());
        assert_eq!(catalog.unavailable_selections, ["missing-current"]);
        assert_eq!(fs::read(&paths.release_pointers_file).unwrap(), pointer);
        assert!(slot.join("node_modules").exists());
        assert!(store.promote("missing-current").is_err());
        assert!(store.restore_release_pointers(Some("missing-current"), None).is_err());
        store.register("reinstalled", "1", None, None).unwrap();
        store.promote("reinstalled").unwrap();
        let catalog = store.load().unwrap();
        assert_eq!(catalog.current_release.as_deref(), Some("reinstalled"));
        assert!(catalog.unavailable_selections.is_empty());
        assert!(slot.join("node_modules").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn missing_current_can_promote_the_remaining_last_known_good() {
        let root = unique_test_root("missing-current-promote-lkg");
        let paths = NexusPaths::from_root(root.clone());
        let store = super::ReleaseStore::new(paths.clone());
        store.register("good", "1", None, None).unwrap();
        { let result = store.promote("good").unwrap(); mark_fixture_healthy(&store, "good"); result };
        store.register("missing", "2", None, None).unwrap();
        { let result = store.promote("missing").unwrap(); mark_fixture_healthy(&store, "missing"); result };
        fs::remove_file(paths.releases_dir.join("missing/manifest.json")).unwrap();
        assert_eq!(store.load().unwrap().last_known_good.as_deref(), Some("good"));
        { let result = store.promote("good").unwrap(); mark_fixture_healthy(&store, "good"); result };
        let catalog = store.load().unwrap();
        assert_eq!(catalog.current_release.as_deref(), Some("good"));
        assert!(catalog.last_known_good.is_none());
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
        { let result = store.promote("harness-a").expect("release A promotes"); mark_fixture_healthy(&store, "harness-a"); result };
        { let result = store.promote("harness-b").expect("release B promotes"); mark_fixture_healthy(&store, "harness-b"); result };

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
        assert_eq!(cleared.last_known_good.as_deref(), Some("harness-b"));
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
    fn prepared_publication_retargets_workspace_directory_links() {
        let root = unique_test_root("prepared-links");
        let paths = NexusPaths::from_root(root.clone());
        paths.ensure_directories().unwrap();
        let candidate = paths.downloads_dir.join("candidate");
        let target = candidate.join("packages/boot");
        let link = candidate.join("apps/cli/node_modules/boot");
        fs::create_dir_all(&target).unwrap();
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        fs::write(target.join("index.js"), "export default 1").unwrap();
        ReleaseStore::create_dir_junction(&link, &target).unwrap();
        ReleaseStore::create_dir_junction(&candidate.join("boot-alias"), &link).unwrap();
        let store = ReleaseStore::new(paths.clone());
        store.register_prepared(&candidate, "harness-links", "rc.1", None, None).unwrap();
        let slot = paths.releases_dir.join("harness-links");
        assert!(!candidate.exists());
        assert_eq!(fs::read_to_string(slot.join("apps/cli/node_modules/boot/index.js")).unwrap(), "export default 1");
        assert_eq!(fs::canonicalize(slot.join("apps/cli/node_modules/boot")).unwrap(), fs::canonicalize(slot.join("packages/boot")).unwrap());
        assert_eq!(fs::read_to_string(slot.join("boot-alias/index.js")).unwrap(), "export default 1");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn prepared_publication_rejects_external_directory_links_before_move() {
        let root = unique_test_root("prepared-external-link");
        let paths = NexusPaths::from_root(root.clone());
        paths.ensure_directories().unwrap();
        let candidate = paths.downloads_dir.join("candidate");
        let outside = root.join("user-data");
        fs::create_dir_all(&candidate).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("keep.txt"), "user data").unwrap();
        ReleaseStore::create_dir_junction(&candidate.join("external"), &outside).unwrap();
        let store = ReleaseStore::new(paths.clone());
        assert!(store.register_prepared(&candidate, "harness-links", "rc.1", None, None).is_err());
        assert!(candidate.exists());
        assert!(!paths.releases_dir.join("harness-links").exists());
        assert_eq!(fs::read_to_string(outside.join("keep.txt")).unwrap(), "user data");
        fs::remove_dir_all(root).unwrap();
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
    fn diagnostics_prioritizes_current_session_and_captures_failure_tail() {
        let root = unique_test_root("diagnostic-current-tail");
        let paths = NexusPaths::from_root(root.clone());
        paths.ensure_directories().unwrap();
        fs::create_dir_all(root.join("compatibility")).unwrap();
        fs::write(root.join("compatibility/latest.json"), "{\"status\":\"failed\"}").unwrap();
        fs::create_dir_all(root.join("canary/work-test")).unwrap();
        fs::write(root.join("canary/latest.json"), "{\"phase\":\"failed\",\"token\":\"CANARY_SECRET\"}").unwrap();
        fs::write(root.join("canary/work-test/.env"), "PRIVATE_WORK_SECRET").unwrap();
        fs::write(root.join("run/checkpoint-restore.json"), "{\"phase\":\"prepared\"}").unwrap();
        let stdout = "zz-current.stdout.log";
        let stderr = "zz-current.stderr.log";
        let session = HarnessLogSession::new("current-run".into(), 1, 0, 0, "out".into(), "err".into(), stdout.into(), stderr.into(), false, 1);
        HarnessLogSessionStore::new(paths.clone()).write(&session).unwrap();
        let mut output = "old startup output\n".repeat(super::MAX_DIAGNOSTICS_LOG_BYTES / 10);
        output.push_str("CURRENT_FAILURE_DETAIL\nAuthorization: Bearer MUST-NOT-LEAK\n");
        fs::write(paths.logs_dir.join(stdout), output).unwrap();
        fs::write(paths.logs_dir.join(stderr), "CURRENT_STDERR\n").unwrap();
        for i in 0..80 { fs::write(paths.logs_dir.join(format!("a-old-{i:03}.log")), "old").unwrap(); }
        let bundle = DiagnosticsStore::new(paths.clone()).collect_with_context(None, Some(serde_json::json!({"configured":true}))).unwrap();
        assert!(bundle.files.len() <= super::MAX_DIAGNOSTICS_FILES);
        let file = bundle.files.iter().find(|f| f.name == format!("logs/{stdout}")).unwrap();
        assert!(file.truncated);
        assert!(bundle.files.iter().any(|f| f.name == format!("logs/{stderr}")));
        let bytes = fs::read_to_string(PathBuf::from(&bundle.directory).join("files").join(&file.name)).unwrap();
        assert!(bytes.contains("CURRENT_FAILURE_DETAIL"));
        assert!(!bytes.contains("MUST-NOT-LEAK"));
        assert!(bundle.files.iter().any(|f| f.name == "effective-config.json"));
        assert!(bundle.files.iter().any(|f| f.name == "compatibility/latest.json"));
        assert!(bundle.files.iter().any(|f| f.name == "canary/latest.json"));
        assert!(!bundle.files.iter().any(|f| f.name.contains("work-test")));
        let canary = fs::read_to_string(PathBuf::from(&bundle.directory).join("files/canary/latest.json")).unwrap();
        assert!(!canary.contains("CANARY_SECRET"));
        assert!(bundle.files.iter().any(|f| f.name == "run/checkpoint-restore.json"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn diagnostics_redacts_query_credentials_structured_secrets_and_private_keys() {
        let json = serde_json::json!({"update":{"source":"https://user:PASS@host/repo?key=QUERY#FRAGMENT"},
            "password":{"nested":"NESTED"}, "args":["https://host/?custom=ARGSECRET"], "normal":"keep"});
        let (bytes, changed) = super::redact_diagnostics_payload(&serde_json::to_vec_pretty(&json).unwrap());
        assert!(changed);
        let text = String::from_utf8(bytes).unwrap();
        assert!(serde_json::from_str::<serde_json::Value>(&text).is_ok());
        assert!(text.contains("https://host/repo") && text.contains("keep"));
        for secret in ["PASS", "QUERY", "FRAGMENT", "NESTED", "ARGSECRET"] { assert!(!text.contains(secret)); }
        let log = "failure\n-----BEGIN PRIVATE KEY-----\nPEMBODY\n-----END PRIVATE KEY-----\n\"token\": {\n\"inner\": {\"value\": \"MULTILINE\"}\n}\nAuthorization:\nCONTINUED\nuseful failure\n";
        let (bytes, _) = super::redact_diagnostics_payload(log.as_bytes());
        let text = String::from_utf8(bytes).unwrap();
        for secret in ["PEMBODY", "MULTILINE", "CONTINUED"] { assert!(!text.contains(secret)); }
        assert!(text.contains("useful failure"));
    }

    #[test]
    fn diagnostics_cold_journal_omits_argument_values_without_changing_source() {
        let root = unique_test_root("diagnostics-cold-journal-arguments");
        let paths = NexusPaths::from_root(root.clone());
        paths.ensure_directories().unwrap();
        let journal = serde_json::json!({"phase":"prepared", "previous_config": {
            "harness":{"args":["--token", "OPAQUE_ONE"]},
            "update":{"build_args":["--key", "OPAQUE_TWO"],"verify_args":["--auth", "OPAQUE_THREE"]}
        }, "target_config":{"harness":{"args":["--credential", "OPAQUE_FOUR"]}}});
        let original = serde_json::to_vec_pretty(&journal).unwrap();
        let journal_path = root.join("cold-publication.json");
        fs::write(&journal_path, &original).unwrap();
        let bundle = DiagnosticsStore::new(paths).collect(None).unwrap();
        let exported = fs::read_to_string(PathBuf::from(bundle.directory).join("files/cold-publication.json")).unwrap();
        for value in ["OPAQUE_ONE", "OPAQUE_TWO", "OPAQUE_THREE", "OPAQUE_FOUR"] { assert!(!exported.contains(value)); }
        assert_eq!(fs::read(journal_path).unwrap(), original);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn diagnostics_truncated_tail_omits_private_key_body_without_begin_marker() {
        let root = unique_test_root("diagnostics-pem-boundary");
        let paths = NexusPaths::from_root(root.clone());
        paths.ensure_directories().unwrap();
        for kind in ["PRIVATE KEY", "RSA PRIVATE KEY", "ENCRYPTED PRIVATE KEY"] {
            let mut log = format!("old context\n-----BEGIN {kind}-----\n");
            // Force the actual bounded file read to start within the body.
            while log.len() <= super::MAX_DIAGNOSTICS_LOG_BYTES + 512 {
                log.push_str("DUMMY_PRIVATE_BODY_0123456789abcdefghijklmnopqrstuvwxyz\n");
            }
            log.push_str(&format!("-----END {kind}-----\nCURRENT_FAILURE_AFTER_KEY\n"));
            fs::write(paths.logs_dir.join("current.stderr.log"), log).unwrap();
            let bundle = DiagnosticsStore::new(paths.clone()).collect(None).unwrap();
            let item = bundle.files.iter().find(|item| item.name == "logs/current.stderr.log").unwrap();
            assert!(item.redacted && item.truncated);
            let exported = fs::read_to_string(PathBuf::from(&bundle.directory).join("files/logs/current.stderr.log")).unwrap();
            assert!(!exported.contains("DUMMY_PRIVATE_BODY"));
            assert!(!exported.contains("PRIVATE KEY"));
            assert!(exported.contains("CURRENT_FAILURE_AFTER_KEY"));
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn diagnostic_export_is_portable_redacted_and_excludes_raw_configuration_backup() {
        let root = unique_test_root("diagnostic-export");
        let paths = NexusPaths::from_root(root.clone());
        paths.ensure_directories().unwrap();
        fs::write(paths.logs_dir.join("current.stderr.log"), "CURRENT_FAILURE\napi_key=LOG_SECRET\n").unwrap();
        fs::write(paths.root.join(super::PREVIOUS_CONFIG_FILE), "BACKUP_SECRET").unwrap();
        let store = DiagnosticsStore::new(paths);
        let bundle = store.collect_with_context(Some("token=NOTE_SECRET".into()), Some(serde_json::json!({"token":"CONFIG_SECRET","runtime":"bundled"}))).unwrap();
        let path = store.open_path(&bundle.id, Some("export.json")).unwrap();
        let bytes = fs::read(&path).unwrap();
        let text = String::from_utf8(bytes.clone()).unwrap();
        for secret in ["LOG_SECRET", "NOTE_SECRET", "CONFIG_SECRET", "BACKUP_SECRET"] { assert!(!text.contains(secret)); }
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["format"], "nexus-diagnostics");
        assert!(value["files"]["logs/current.stderr.log"].as_str().unwrap().contains("CURRENT_FAILURE"));
        assert_eq!(value["files"].as_object().unwrap().len(), bundle.files.len());
        assert_eq!(bundle.note.as_deref(), Some("[REDACTED]"));
        fs::remove_dir_all(root).unwrap();
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

        let document = NexusConfigFile { update_attempt_id: None, external_harness: None,
            schema_version: 1,
            harness_preferences: None,
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
            .write(&NexusConfigFile { external_harness: None,
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
        #[cfg(windows)]
        let pnpm_verbatim = PathBuf::from(
            r"\\?\C:\Users\Fixture\AppData\Local\node\corepack\v1\pnpm\11.7.0\bin\pnpm.mjs",
        );
        #[cfg(not(windows))]
        let pnpm_verbatim = root.join("pnpm/bin/pnpm.mjs");
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
        #[cfg(windows)]
        assert_eq!(
            command.prefix_args,
            vec![OsString::from(
                r"C:\Users\Fixture\AppData\Local\node\corepack\v1\pnpm\11.7.0\bin\pnpm.mjs"
            )]
        );
        #[cfg(not(windows))]
        assert_eq!(command.prefix_args, vec![pnpm_verbatim.as_os_str().to_owned()]);
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
    fn module_farm_initializes_a_missing_first_run_home() {
        let root = unique_test_root("module-farm-first-run");
        let home = root.join("new parent/another parent/.dsh");
        let slot = root.join("slot");
        let package = slot.join("packages/example");
        fs::create_dir_all(&package).unwrap();
        fs::write(package.join("package.json"), r#"{"name":"@dsh/example"}"#).unwrap();
        assert!(!home.exists());
        let result = ReleaseStore::heal_module_farm(&home, &slot);
        if let Err(error) = &result {
            fs::remove_dir_all(&root).unwrap();
            panic!("first startup could not prepare the missing Harness home: {error}");
        }
        assert_eq!(result.unwrap(), 1);
        assert!(ReleaseStore::same_directory(&home.join("profiles/node_modules/@dsh/example"), &package));
        assert_eq!(ReleaseStore::heal_module_farm(&home, &slot).unwrap(), 0);
        fs::remove_dir_all(root).unwrap();
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

    #[test]
    fn module_farm_rejects_linked_home_ancestors_before_creating_directories() {
        let root = unique_test_root("module-farm-home-ancestor");
        let outside = root.join("outside");
        fs::create_dir_all(outside.join("existing-home")).unwrap();
        let alias = root.join("alias");
        ReleaseStore::create_dir_junction(&alias, &outside).unwrap();
        for home in [alias.join("new/nested/home"), alias.join("existing-home")] {
            assert!(ReleaseStore::heal_module_farm(&home, &root.join("slot")).is_err());
        }
        assert!(!outside.join("new").exists());
        assert_eq!(fs::read_dir(outside.join("existing-home")).unwrap().count(), 0);
        ReleaseStore::remove_module_link(&alias).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn module_farm_junction_preserves_shell_characters_as_literal_paths() {
        let root = unique_test_root("module-farm-literal");
        let home = root.join("用户 home & %PATH%/.dsh");
        let slot = root.join("版本 slot & %TEMP%");
        let package = slot.join("packages/example");
        fs::create_dir_all(&package).unwrap();
        fs::write(package.join("package.json"), r#"{"name":"@dsh/example"}"#).unwrap();
        assert_eq!(ReleaseStore::heal_module_farm(&home, &slot).unwrap(), 1);
        assert!(ReleaseStore::same_directory(&home.join("profiles/node_modules/@dsh/example"), &package));
        fs::remove_dir_all(root).unwrap();
    }
}

/// The immutable upstream Harness is launched only through this external
/// process specification. Nexus never silently chooses a discovered
/// installation; discovery is an advisory API and the selected command is
/// persisted here. Node mode is normalized to `program=node` and an entry
/// script as the first process argument so the supervisor remains unchanged.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NexusConfigFile {
    /// Ownership of automatic rollback for one in-flight update attempt.
    /// Explicit replacement of update settings revokes this ownership.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update_attempt_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_harness: Option<ExternalHarness>,
    #[serde(default = "current_record_schema", deserialize_with = "deserialize_record_schema")]
    pub schema_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_preferences: Option<nexus_protocol::HarnessPreferencesPayload>,
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

/// Version 1 is also the explicitly supported pre-versioned format.
pub fn current_record_schema() -> u32 { 1 }
pub fn deserialize_record_schema<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u32, D::Error> {
    let version = u32::deserialize(d)?;
    if version != 1 { return Err(serde::de::Error::custom("Unsupported persisted record schema")); }
    Ok(version)
}
impl Default for NexusConfigFile {
    fn default() -> Self { Self { update_attempt_id: None, external_harness: None, schema_version: 1, harness_preferences: None, harness: None, update: None, releases: None, runtime: None, snapshots: None } }
}
impl NexusConfigFile {
    /// Explicit user intent revokes a prior automatic rollback, even when the
    /// newly saved update settings happen to equal the previous settings.
    pub fn set_update(&mut self, update: Option<UpdateSpec>) {
        self.update = update;
        self.update_attempt_id = None;
    }
}
/// Read a versioned record; missing version is the supported legacy v1 layout.
pub fn decode_versioned_record<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> io::Result<T> {
    let mut value: serde_json::Value = serde_json::from_slice(bytes).map_err(|_| invalid_data("Invalid persisted record"))?;
    let fields = value.as_object_mut().ok_or_else(|| invalid_data("Invalid persisted record"))?;
    if fields.remove("schema_version").is_some_and(|version| version.as_u64() != Some(1)) {
        return Err(invalid_data("Unsupported persisted record schema"));
    }
    serde_json::from_value(value).map_err(|_| invalid_data("Unsupported persisted record fields"))
}
pub fn write_versioned_record<T: Serialize + serde::de::DeserializeOwned>(root: &Path, path: &Path, record: &T) -> io::Result<()> {
    if let Some(bytes) = read_regular_file_bounded(path, 16 * 1024 * 1024)? { let _: T = decode_versioned_record(&bytes)?; }
    let mut value = serde_json::to_value(record).map_err(invalid_data)?;
    value.as_object_mut().ok_or_else(|| invalid_data("Invalid persisted record"))?.insert("schema_version".into(), 1.into());
    let _: T = decode_versioned_record(&serde_json::to_vec(&value).map_err(invalid_data)?)?;
    write_private_json_atomic(root, path, &value)
}
/// Strict dispatch preserves the two documented legacy single-section forms.
fn decode_config_document(bytes: &[u8]) -> io::Result<NexusConfigFile> {
    let value: serde_json::Value = serde_json::from_slice(bytes).map_err(|error| invalid_data(format!("Invalid configuration format at line {} column {}", error.line(), error.column())))?;
    let object = value.as_object().ok_or_else(|| invalid_data("Invalid configuration format"))?;
    let document = if !object.contains_key("schema_version") && object.contains_key("program") {
        NexusConfigFile { external_harness: None, harness: Some(serde_json::from_value(value).map_err(|_| invalid_data("Invalid legacy Harness configuration"))?), ..Default::default() }
    } else if !object.contains_key("schema_version") && object.contains_key("source") {
        NexusConfigFile { external_harness: None, update: Some(serde_json::from_value(value).map_err(|_| invalid_data("Invalid legacy update configuration"))?), ..Default::default() }
    } else { serde_json::from_value(value).map_err(|_| invalid_data("Unsupported configuration schema or fields"))? };
    Ok(document)
}

/// Nexus-owned release slot capacity settings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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

/// Nexus-owned configuration writer, previous valid config and home protection. Harness
/// source, working directories, and `$HOME/.dsh` are never modified here.
#[derive(Debug, Clone)]
pub struct ConfigSnapshot { pub document: NexusConfigFile, pub revision: String }
#[derive(Debug)]
struct ConfigRevisionConflict;
impl std::fmt::Display for ConfigRevisionConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str("Configuration changed since this draft was loaded. Keep the draft and refresh before saving again.") }
}
impl std::error::Error for ConfigRevisionConflict {}
pub fn is_config_revision_conflict(error: &io::Error) -> bool { error.get_ref().is_some_and(|inner| inner.is::<ConfigRevisionConflict>()) }

#[derive(Clone)]
pub struct ConfigStore {
    paths: NexusPaths,
}

mod config_transaction;
pub use config_transaction::is_config_transaction_error;
static CONFIG_WRITE_GATE: Mutex<()> = Mutex::new(());

impl ConfigStore {
    pub fn new(paths: NexusPaths) -> Self {
        Self { paths }
    }

    pub fn paths(&self) -> &NexusPaths {
        &self.paths
    }

    pub fn snapshot(&self) -> io::Result<ConfigSnapshot> {
        let _guard = self.lock_gate()?;
        self.settle_pending_unlocked()?;
        self.snapshot_unlocked()
    }
    fn snapshot_unlocked(&self) -> io::Result<ConfigSnapshot> {
        use sha2::{Digest, Sha256};
        let bytes = read_regular_file_bounded(&self.paths.config_file, 4 * 1024 * 1024)?;
        let document = bytes.as_deref().map(decode_config_document).transpose().map_err(|error| io::Error::new(error.kind(), format!("{}: {error}", self.paths.config_file.display())))?.unwrap_or_default();
        validate_config_document(&self.paths, &document)?;
        let revision = bytes.as_ref().map(|b| format!("sha256:{:x}", Sha256::digest(b))).unwrap_or_else(|| "missing".into());
        Ok(ConfigSnapshot { document, revision })
    }
    pub fn transaction_if_revision<T>(&self, expected: &str, update: impl FnOnce(&mut NexusConfigFile) -> io::Result<T>) -> io::Result<(ConfigSnapshot, T)> {
        let _guard = self.lock_gate()?;
        self.settle_pending_unlocked()?;
        let mut snapshot = self.snapshot_unlocked()?;
        if snapshot.revision != expected { return Err(io::Error::new(io::ErrorKind::WouldBlock, ConfigRevisionConflict)); }
        let result = update(&mut snapshot.document)?;
        validate_config_document(&self.paths, &snapshot.document)?;
        self.write_unlocked(&snapshot.document)?;
        Ok((self.snapshot_unlocked()?, result))
    }
    pub fn restore_previous_if_revision(&self, expected: &str) -> io::Result<ConfigSnapshot> {
        self.transaction_if_revision(expected, |document| {
            let bytes = read_regular_file_bounded(&self.paths.root.join(PREVIOUS_CONFIG_FILE), 4 * 1024 * 1024)?
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "No previous valid Nexus configuration is available"))?;
            *document = decode_config_document(&bytes)?;
            document.update_attempt_id = None;
            Ok(())
        }).map(|(snapshot, ())| snapshot)
    }
    pub fn load(&self) -> io::Result<NexusConfigFile> {
        let _guard = self.lock_gate()?;
        self.settle_pending_unlocked()?;
        self.load_unlocked()
    }

    fn load_unlocked(&self) -> io::Result<NexusConfigFile> {
        let Some(bytes) = read_regular_file_bounded(&self.paths.config_file, 4 * 1024 * 1024)? else {
            return Ok(NexusConfigFile::default());
        };
        let document = decode_config_document(&bytes).map_err(|error| io::Error::new(error.kind(), format!("{}: {error}", self.paths.config_file.display())))?;
        validate_config_document(&self.paths, &document)?;
        Ok(document)
    }

    pub fn write(&self, document: &NexusConfigFile) -> io::Result<()> {
        let mut document = document.clone();
        document.update_attempt_id = None;
        self.write_recovery_document(&document)
    }

    /// Replay an already validated publication decision exactly. Ordinary
    /// full-document saves must use `write` to revoke prior rollback ownership.
    pub fn write_recovery_document(&self, document: &NexusConfigFile) -> io::Result<()> {
        validate_config_document(&self.paths, document)?;
        let _guard = self.lock_gate()?;
        self.write_unlocked(document)
    }

    /// Publish only if the configuration still matches the transaction's captured input.
    pub fn write_if_current(&self, expected: &NexusConfigFile, document: &NexusConfigFile) -> io::Result<bool> {
        validate_config_document(&self.paths, document)?;
        let _guard = self.lock_gate()?;
        self.settle_pending_unlocked()?;
        if self.load_unlocked()? != *expected { return Ok(false); }
        let mut document = document.clone();document.update_attempt_id = None;
        self.write_unlocked(&document)?;
        Ok(true)
    }

    fn write_unlocked(&self, document: &NexusConfigFile) -> io::Result<()> {
        self.settle_pending_unlocked()?;
        let previous = Some(self.load_unlocked()?);
        let mut document = document.clone();
        if previous.as_ref().is_some_and(|old| old.update != document.update && old.update_attempt_id == document.update_attempt_id) {
            document.update_attempt_id = None;
        }
        let document = &document;
        if let Some(bytes) = read_regular_file_bounded(&self.paths.root.join(PREVIOUS_CONFIG_FILE), 4 * 1024 * 1024)? { validate_config_document(&self.paths, &decode_config_document(&bytes)?)?; }
        self.paths.ensure_directories()?;
        let old_home = configured_harness_home(&self.paths.root)?;
        let new_home = document.harness_preferences.as_ref().and_then(|p| p.home.as_deref())
            .map(str::trim).filter(|value| !value.is_empty()).map(PathBuf::from);
        let inherited = env::var_os("DSH_HOME").filter(|value| !value.is_empty()).map(PathBuf::from);
        // Program sources have their own protection registry below. Recording
        // them as data homes as well exhausts that unrelated registry's limit.
        let homes: Vec<_> = old_home.into_iter().chain(new_home).chain(inherited).collect();
        protect_harness_homes(&self.paths.root, &homes)?;
        external_harness::protect_locations(&self.paths,previous.as_ref().and_then(|p|p.external_harness.as_ref()),document.external_harness.as_ref())?;
        // Only these explicitly Nexus-owned files are tightened. A no-op save
        // must not rotate or rewrite the previous valid configuration bytes.
        for path in [&self.paths.config_file, &self.paths.root.join(PREVIOUS_CONFIG_FILE)] {
            if read_regular_file_bounded(path, 4 * 1024 * 1024)?.is_some() {
                nexus_private_file::secure_existing_private(path)?;
            }
        }
        if previous.as_ref() == Some(document) && self.paths.config_file.is_file() { return Ok(()); }
        self.publish_config_unlocked(document, previous.is_some())
    }

    pub fn restore_previous(&self) -> io::Result<NexusConfigFile> {
        let _guard = self.lock_gate()?;
        self.settle_pending_unlocked()?;
        let path = self.paths.root.join(PREVIOUS_CONFIG_FILE);
        let bytes = read_regular_file_bounded(&path, 4 * 1024 * 1024)?
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "No previous valid Nexus configuration is available"))?;
        let mut previous = decode_config_document(&bytes)?;
        previous.update_attempt_id = None;
        validate_config_document(&self.paths, &previous)?;
        // write_unlocked checks both old and new Harness homes and preserves
        // the current valid configuration as the next undo point.
        self.write_unlocked(&previous)?;
        Ok(previous)
    }

    /// Atomically read, modify, validate, and replace the shared config file.
    pub fn transaction<T>(
        &self,
        update: impl FnOnce(&mut NexusConfigFile) -> io::Result<T>,
    ) -> io::Result<(NexusConfigFile, T)> {
        let _guard = self.lock_gate()?;
        self.settle_pending_unlocked()?;
        let mut document = self.load_unlocked()?;
        let result = update(&mut document)?;
        validate_config_document(&self.paths, &document)?;
        self.write_unlocked(&document)?;
        Ok((self.load_unlocked()?, result))
    }

    fn lock_gate(&self) -> io::Result<std::sync::MutexGuard<'static, ()>> {
        CONFIG_WRITE_GATE
            .lock()
            .map_err(|_| io::Error::other("config lock is poisoned"))
    }
}

fn validate_config_document(paths: &NexusPaths, document: &NexusConfigFile) -> io::Result<()> {
    if document.update_attempt_id.as_ref().is_some_and(|id| id.len()!=64 || !id.bytes().all(|b|b.is_ascii_hexdigit())) {
        return Err(invalid_data("Invalid update attempt identity"));
    }
    if let Some(source)=&document.external_harness {
        if !source.root.is_absolute() || source.identity.is_empty() || source.fingerprint.len()!=64 {return Err(invalid_data("Invalid external Harness identity"));}
        if config_protection::paths_overlap_by_identity(&paths.root,&source.root)? {return Err(invalid_data("External Harness overlaps Nexus directories"));}
        if let Some(home)=document.harness_preferences.as_ref().and_then(|p|p.home.as_ref()) {
            if config_protection::paths_overlap_by_identity(&source.root,Path::new(home))? {return Err(invalid_data("Harness home overlaps external program directory"));}
        }
    }

    if document.schema_version != 1 { return Err(invalid_data("Unsupported configuration schema")); }
    if let Some(preferences) = &document.harness_preferences {
        normalize_harness_preferences(preferences.clone())?;
    }
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

/// Read-only launch references shared by preview and deletion. Never acquire
/// CONFIG_WRITE_GATE or RELEASE_GATE here: deletion already owns its release gate.
fn configuration_launch_paths(paths: &NexusPaths) -> io::Result<Vec<PathBuf>> {
    match fs::symlink_metadata(paths.root.join("config-write.pending.json")) {
        Ok(_) => return Err(io::Error::new(io::ErrorKind::WouldBlock, "Configuration recovery must finish before deleting a release")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {},
        Err(error) => return Err(error),
    }
    let bytes = read_regular_file_bounded(&paths.config_file, 4 * 1024 * 1024)?;
    let config: NexusConfigFile = bytes.as_deref().map(decode_config_document).transpose()?.unwrap_or_default();
    let mut references = Vec::new();
    if let Some(preferences) = &config.harness_preferences {
        references.extend(preferences.patches.iter().flatten().map(PathBuf::from));
        references.extend(preferences.patch_entries.iter().flatten().filter(|entry| entry.enabled && !entry.source.starts_with("https://")).map(|entry| PathBuf::from(&entry.source)));
    }
    if let Some(source)=&config.external_harness { references.push(source.root.clone()); }
    if let Some(runtime) = config.runtime {
        for pin in [runtime.node, runtime.pnpm, runtime.git].into_iter().flatten() { references.push(pin.path); }
    }
    if let Some(harness) = parse_harness_launch_spec(bytes.as_deref())? {
        let pointers = read_regular_file_bounded(&paths.release_pointers_file, 1024 * 1024)?
            .map(|bytes| decode_json::<ReleasePointerDocument>(&bytes).map_err(invalid_data)).transpose()?;
        if pointers.as_ref().is_some_and(|p| p.schema_version != RELEASE_SCHEMA_VERSION) {
            return Err(invalid_data("Unsupported release pointer schema"));
        }
        let release_id = pointers.as_ref().and_then(|p| p.current_release.as_deref());
        if let Some(id) = release_id { validate_release_id(id)?; }
        let release_root = release_id.map(|id| paths.releases_dir.join(id));
        let profile = ProfileStore::new(paths.clone()).read_unlocked()?
            .map(|catalog| catalog.active_profile).unwrap_or_else(|| DEFAULT_PROFILE.to_owned());
        validate_profile_name(&profile)?;
        // An unresolved context stays relative and is protected conservatively.
        // Do not guess an Agent working directory for arbitrary relative paths.
        let render = |path: &Path| harness.render_path_for_context(path, &profile, release_id, release_root.as_deref())
            .unwrap_or_else(|_| path.to_path_buf());
        let cwd = harness.working_dir.as_deref().map(&render);
        if let Some(directory) = &cwd { references.push(directory.clone()); }
        let program = render(&harness.program);
        if program.is_absolute() { references.push(program.clone()); }
        else if program.components().count() > 1 {
            references.push(cwd.as_ref().map_or_else(|| program.clone(), |cwd| cwd.join(&program)));
        }
        if harness_program_is_node_runtime(&program) || harness.mode == HarnessLaunchMode::Node {
            let args = harness.render_args_for_context(&profile, release_id, release_root.as_deref())
                .unwrap_or_else(|_| harness.args.clone());
            for argument in &args {
                let value = if argument.starts_with('-') { argument.split_once('=').map_or(argument.as_str(), |(_, value)| value) } else { argument.as_str() };
                if value.starts_with('-') || value.contains("{dsh_home}") || value.is_empty() { continue; }
                let path = PathBuf::from(value);
                references.push(if path.is_absolute() { path } else { cwd.as_ref().map_or_else(|| path.clone(), |cwd| cwd.join(&path)) });
            }
        }
    }
    Ok(references)
}

fn configuration_paths_overlap(target: &Path, references: &[PathBuf]) -> io::Result<bool> {
    for reference in references {
        if !reference.is_absolute() || config_protection::paths_overlap_by_identity(target, reference)? { return Ok(true); }
    }
    Ok(false)
}

fn ensure_configuration_paths_preserved(paths: &NexusPaths, target: &Path) -> io::Result<()> {
    if configuration_paths_overlap(target, &configuration_launch_paths(paths)?)? {
        return Err(io::Error::new(io::ErrorKind::ResourceBusy, "Release contains a configured runtime or Harness launch path"));
    }
    Ok(())
}

/// Load the optional external Harness command from Nexus-owned configuration
/// and then apply explicit environment overrides. A missing program is a
/// valid, intentional control-plane-only configuration.
pub fn load_harness_launch_spec(paths: &NexusPaths) -> io::Result<Option<HarnessLaunchSpec>> {
    let bytes = {
        let store = ConfigStore::new(paths.clone());
        let _guard = store.lock_gate()?;
        store.settle_pending_unlocked()?;
        read_regular_file_bounded(&paths.config_file, 4 * 1024 * 1024)?
    };
    if let Some(bytes) = &bytes {
        if let Some(source) = decode_config_document(bytes)?.external_harness { return Ok(Some(source.launch_spec(paths))); }
    }
    parse_harness_launch_spec(bytes.as_deref())
}

/// Pure parsing and environment application, shared with deletion protection.
fn parse_harness_launch_spec(bytes: Option<&[u8]>) -> io::Result<Option<HarnessLaunchSpec>> {
    let mut spec = if let Some(bytes) = bytes {
        let document = decode_config_document(bytes)?;
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
    let bytes = {
        let store = ConfigStore::new(paths.clone());
        let _guard = store.lock_gate()?;
        store.settle_pending_unlocked()?;
        read_regular_file_bounded(&paths.config_file, 4 * 1024 * 1024)?
    };
    parse_update_spec(bytes.as_deref())
}

fn parse_update_spec(bytes: Option<&[u8]>) -> io::Result<Option<UpdateSpec>> {
    let mut spec = if let Some(bytes) = bytes {
        let document = decode_config_document(&bytes)?;
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

/// Apply environment overrides to exactly the caller's captured document.
pub fn effective_config_document(mut document: NexusConfigFile) -> io::Result<NexusConfigFile> {
    let bytes = encode_json(&document).map_err(invalid_data)?;
    document.harness = parse_harness_launch_spec(Some(&bytes))?;
    document.update = parse_update_spec(Some(&bytes))?;
    Ok(document)
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

#[cfg(test)]
mod private_config_tests {
    use super::*;
    #[test]
    fn private_config_restore_keeps_undo_and_rejects_unknown_home_or_oversize() {
        let root = env::temp_dir().join(format!("nexus-private-config-{}", unix_time_nanos_for_update()));
        let paths = NexusPaths::from_root(root.clone());
        let store = ConfigStore::new(paths.clone());
        let a = NexusConfigFile { external_harness: None, harness_preferences: Some(nexus_protocol::HarnessPreferencesPayload { telemetry_disabled: Some(true), ..Default::default() }), ..Default::default() };
        let b = NexusConfigFile { external_harness: None, harness_preferences: Some(nexus_protocol::HarnessPreferencesPayload { telemetry_disabled: Some(false), ..Default::default() }), ..Default::default() };
        store.write(&a).unwrap(); store.write(&b).unwrap();
        let backup = root.join(PREVIOUS_CONFIG_FILE);
        let original = fs::read(&backup).unwrap();
        store.write(&b).unwrap();
        assert_eq!(fs::read(&backup).unwrap(), original);
        assert_eq!(store.restore_previous().unwrap(), a);
        assert_eq!(store.load().unwrap(), a);
        assert_eq!(store.restore_previous().unwrap(), b);
        for path in [&paths.config_file, &backup] {
            nexus_private_file::verify_private(&fs::File::open(path).unwrap()).unwrap();
        }
        let saved_backup = fs::read(&backup).unwrap();
        fs::write(&paths.config_file, b"{").unwrap();
        assert!(store.restore_previous().is_err());
        assert_eq!(fs::read(&paths.config_file).unwrap(), b"{");
        assert_eq!(fs::read(&backup).unwrap(), saved_backup);
        fs::write(&paths.config_file, vec![b'x'; 4 * 1024 * 1024 + 1]).unwrap();
        assert!(store.load().is_err());
        assert!(store.restore_previous().is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn private_atomic_replacement_preserves_acl_and_old_value_when_locked() {
        let root = env::temp_dir().join(format!("nexus-private-atomic-{}", unix_time_nanos_for_update()));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("config.json");
        fs::write(&path, b"public old").unwrap();
        write_private_bytes_atomic(&root, &path, b"private new").unwrap();
        nexus_private_file::verify_private(&fs::File::open(&path).unwrap()).unwrap();
        #[cfg(windows)] {
            use std::os::windows::fs::OpenOptionsExt;
            let locked = fs::OpenOptions::new().read(true).share_mode(1).open(&path).unwrap();
            assert!(write_private_bytes_atomic(&root, &path, b"must not publish").is_err());
            drop(locked);
            assert_eq!(fs::read(&path).unwrap(), b"private new");
            assert_eq!(fs::read_dir(&root).unwrap().count(), 1);
        }
        fs::remove_dir_all(root).unwrap();
    }
}

pub use nexus_private_file::create_new_private_directory;

pub use config_protection::paths_overlap_by_identity;
