//! Bounded, confirmation-gated runtime provisioning for DSH Nexus.
//!
//! This crate never writes Nexus configuration and never changes the parent
//! process environment.  Callers obtain a [`SupplyPlan`], present its token to
//! the user, and pass the same plan plus a freshly computed foundation runtime
//! plan to [`RuntimeSupplier::execute_confirmed`].

mod archive;
mod portable;
mod source;
mod system;

#[cfg(test)]
mod tests;

use std::{
    fmt, io,
    path::{Component, Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use nexus_core::RuntimeConfig;
use nexus_protocol::{RuntimeInstallMode, RuntimeOwnership, RuntimePlanResponse, RuntimeSource};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub use archive::{extract_node_zip, extract_pnpm_tarball, ArchiveLimits};
pub use portable::{discover_corepack_pnpm, CorepackPnpmCandidate};
pub use source::{
    DownloadClient, DownloadReceipt, HttpDownloadClient, SourcePolicy, TypedDownloadRequest,
    TypedDownloadResponse,
};
pub use system::{
    CommandProcessRunner, ProcessOutcome, ProcessRunner, SystemInstallKind, SystemInstallResult,
    SystemInstallSpec, TypedProcessRequest,
};

pub const SOURCE_POLICY_REVISION: &str = "nexus-runtime-source-v1-2026-09-05";

#[derive(Debug)]
pub enum SupplyError {
    InvalidPlan(String),
    Network(String),
    Integrity(String),
    UnsafeArchive(String),
    Process(String),
    Busy(String),
    Cancelled,
    Io(io::Error),
}

impl fmt::Display for SupplyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPlan(message) => write!(formatter, "invalid supply plan: {message}"),
            Self::Network(message) => write!(formatter, "runtime download failed: {message}"),
            Self::Integrity(message) => {
                write!(formatter, "runtime integrity check failed: {message}")
            }
            Self::UnsafeArchive(message) => write!(formatter, "unsafe runtime archive: {message}"),
            Self::Process(message) => write!(formatter, "runtime process failed: {message}"),
            Self::Busy(message) => write!(formatter, "runtime supply is busy: {message}"),
            Self::Cancelled => formatter.write_str("runtime supply was cancelled"),
            Self::Io(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for SupplyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for SupplyError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

pub type Result<T> = std::result::Result<T, SupplyError>;

#[derive(Debug, Clone, Default)]
pub struct CancellationToken(Arc<AtomicBool>);

impl CancellationToken {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    pub(crate) fn check(&self) -> Result<()> {
        if self.is_cancelled() {
            Err(SupplyError::Cancelled)
        } else {
            Ok(())
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HostOs {
    Windows,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HostArch {
    X64,
    Arm64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct HostPlatform {
    pub os: HostOs,
    pub arch: HostArch,
}

impl HostPlatform {
    pub fn current_windows() -> Result<Self> {
        let arch = match std::env::consts::ARCH {
            "x86_64" => HostArch::X64,
            "aarch64" => HostArch::Arm64,
            other => {
                return Err(SupplyError::InvalidPlan(format!(
                    "unsupported Windows architecture: {other}"
                )))
            }
        };
        Ok(Self {
            os: HostOs::Windows,
            arch,
        })
    }

    pub(crate) fn node_arch(self) -> &'static str {
        match self.arch {
            HostArch::X64 => "x64",
            HostArch::Arm64 => "arm64",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    NodeZip,
    NodeMsi,
    PnpmTarball,
    PnpmInstallScript,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ArtifactIdentity {
    pub kind: ArtifactKind,
    pub locator: String,
    pub filename: String,
    pub digest_algorithm: String,
    pub digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signing_key_id: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SupplyDisposition {
    ReuseExisting,
    ReuseCorepackCache,
    ReuseOwnedCache,
    PublishPortable,
    InstallSystem,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlannedTool {
    pub name: String,
    pub version: String,
    pub disposition: SupplyDisposition,
    pub path: PathBuf,
    pub ownership: RuntimeOwnership,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<ArtifactIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_identity: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SupplyPlan {
    pub schema_version: u32,
    pub foundation_plan_id: String,
    pub release_id: String,
    pub source_policy_revision: String,
    pub source: RuntimeSource,
    pub mode: RuntimeInstallMode,
    pub host: HostPlatform,
    pub destination_root: PathBuf,
    pub node: PlannedTool,
    pub pnpm: PlannedTool,
    pub supply_plan_id: String,
}

impl SupplyPlan {
    fn token(&self) -> Result<String> {
        let mut unsigned = self.clone();
        unsigned.supply_plan_id.clear();
        let bytes = serde_json::to_vec(&unsigned)
            .map_err(|error| SupplyError::InvalidPlan(error.to_string()))?;
        Ok(format!("sha256:{}", hex_sha256(&bytes)))
    }

    fn seal(mut self) -> Result<Self> {
        self.supply_plan_id = self.token()?;
        Ok(self)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SupplyOutcome {
    pub runtime: RuntimeConfig,
    pub node_disposition: SupplyDisposition,
    pub pnpm_disposition: SupplyDisposition,
    pub reboot_required: bool,
}

pub struct RuntimeSupplyPlanner<'a, D: DownloadClient + ?Sized> {
    downloader: &'a D,
    policy: SourcePolicy,
    host: HostPlatform,
    cache_root: PathBuf,
    corepack_root: Option<PathBuf>,
}

impl<'a, D: DownloadClient + ?Sized> RuntimeSupplyPlanner<'a, D> {
    pub fn new(
        downloader: &'a D,
        source: RuntimeSource,
        host: HostPlatform,
        cache_root: PathBuf,
        corepack_root: Option<PathBuf>,
    ) -> Result<Self> {
        validate_absolute_root(&cache_root, "runtime cache root")?;
        if let Some(root) = &corepack_root {
            validate_absolute_root(root, "Corepack cache root")?;
        }
        Ok(Self {
            downloader,
            policy: SourcePolicy::new(source),
            host,
            cache_root,
            corepack_root,
        })
    }

    pub async fn plan(
        &self,
        runtime_plan: &RuntimePlanResponse,
        cancellation: &CancellationToken,
    ) -> Result<SupplyPlan> {
        portable::build_supply_plan(
            self.downloader,
            &self.policy,
            self.host,
            &self.cache_root,
            self.corepack_root.as_deref(),
            runtime_plan,
            cancellation,
        )
        .await?
        .seal()
    }
}

pub struct RuntimeSupplier<'a, D: DownloadClient + ?Sized, P: ProcessRunner + ?Sized> {
    downloader: &'a D,
    runner: &'a P,
    cache_root: PathBuf,
    corepack_root: Option<PathBuf>,
    host: HostPlatform,
}

impl<'a, D: DownloadClient + ?Sized, P: ProcessRunner + ?Sized> RuntimeSupplier<'a, D, P> {
    pub fn new(
        downloader: &'a D,
        runner: &'a P,
        host: HostPlatform,
        cache_root: PathBuf,
        corepack_root: Option<PathBuf>,
    ) -> Result<Self> {
        validate_absolute_root(&cache_root, "runtime cache root")?;
        if let Some(root) = &corepack_root {
            validate_absolute_root(root, "Corepack cache root")?;
        }
        Ok(Self {
            downloader,
            runner,
            cache_root,
            corepack_root,
            host,
        })
    }

    pub async fn execute_confirmed(
        &self,
        fresh_runtime_plan: &RuntimePlanResponse,
        plan: &SupplyPlan,
        confirmation: &str,
        cancellation: &CancellationToken,
    ) -> Result<SupplyOutcome> {
        if confirmation != plan.supply_plan_id || plan.token()? != plan.supply_plan_id {
            return Err(SupplyError::InvalidPlan(
                "confirmation does not match the deterministic supply plan".to_owned(),
            ));
        }
        if plan.foundation_plan_id != fresh_runtime_plan.plan_id
            || plan.release_id != fresh_runtime_plan.release_id
            || plan.source != fresh_runtime_plan.source
            || plan.mode != fresh_runtime_plan.mode
        {
            return Err(SupplyError::InvalidPlan(
                "foundation runtime plan changed while awaiting confirmation".to_owned(),
            ));
        }
        if plan.source_policy_revision != SOURCE_POLICY_REVISION
            || plan.host != self.host
            || plan.destination_root != self.cache_root
        {
            return Err(SupplyError::InvalidPlan(
                "host, destination, or source policy changed".to_owned(),
            ));
        }
        portable::validate_plan_against_policy(plan, &SourcePolicy::new(plan.source))?;
        cancellation.check()?;
        portable::execute_plan(
            self.downloader,
            self.runner,
            &SourcePolicy::new(plan.source),
            &self.cache_root,
            self.corepack_root.as_deref(),
            fresh_runtime_plan,
            plan,
            cancellation,
        )
        .await
    }
}

pub(crate) fn validate_absolute_root(path: &Path, label: &str) -> Result<()> {
    if !path.is_absolute()
        || path.as_os_str().is_empty()
        || path
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
        || path.to_string_lossy().chars().any(char::is_control)
    {
        return Err(SupplyError::InvalidPlan(format!(
            "{label} must be an absolute normalized path"
        )));
    }
    Ok(())
}

pub(crate) fn hex_sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
