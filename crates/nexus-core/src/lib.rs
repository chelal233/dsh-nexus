//! UI-independent configuration, path, and state primitives for Nexus.

use std::{
    env, fs, io,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use nexus_protocol::{AgentLifecycleState, AgentStatePayload, HarnessState};
use serde::{Deserialize, Serialize};

pub const DEFAULT_AGENT_PORT: u16 = 3090;
pub const DATA_DIR_ENV: &str = "NEXUS_DATA_DIR";
pub const PORT_ENV: &str = "NEXUS_AGENT_PORT";

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
            profile: None,
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
    use std::path::PathBuf;

    use nexus_protocol::{AgentLifecycleState, HarnessState};

    use super::{is_within, AgentState, NexusConfig, NexusPaths};

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
}
