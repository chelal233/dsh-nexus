//! Versioned wire types shared by the headless Agent and its clients.

use serde::{de::DeserializeOwned, Deserialize, Serialize};

/// Small JSON helpers keep persistence consumers on the same wire-format
/// implementation without exposing an additional dependency surface in every
/// crate.
pub fn encode_json<T: Serialize>(value: &T) -> Result<Vec<u8>, serde_json::Error> {
    serde_json::to_vec_pretty(value)
}

pub fn decode_json<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, serde_json::Error> {
    serde_json::from_slice(bytes)
}

/// The first stable HTTP API namespace exposed by Nexus Agent.
pub const API_VERSION: &str = "v1";

/// Version of the explicit Harness launch wire contract. A Node payload uses
/// `entry` plus additional `args`; older Agents that do not advertise this
/// field are not safe targets for a Node configuration write.
pub const HARNESS_CONFIG_WIRE_VERSION: u8 = 2;

fn is_false(value: &bool) -> bool {
    !*value
}

/// Health status is intentionally small so it can be consumed by scripts.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HealthStatus {
    Ok,
    ShuttingDown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HealthResponse {
    #[serde(default)]
    pub auth_version: u8,
    pub api_version: String,
    pub service: String,
    pub status: HealthStatus,
    /// Canonical Nexus data-root identity. Launchers must compare this before
    /// adopting or stopping a process already listening on the configured
    /// loopback port.
    pub data_root_id: String,
    /// Opaque per-process correlation value for launcher metadata.
    pub instance_id: String,
    /// Absolute path of the running Agent executable. Launchers compare this
    /// against their own resolved binary to detect a stale Agent left over
    /// from an older build and restart it deliberately.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary_path: Option<String>,
    /// Identity compiled into the running executable, never read from a replaceable file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_id: Option<String>,
    /// Capability version for the split Node `entry` + `args` Harness config.
    /// Missing in older Agent responses and therefore defaults to zero.
    #[serde(default)]
    pub harness_config_wire_version: u8,
}

impl HealthResponse {
    pub fn healthy(data_root_id: String, instance_id: String) -> Self {
        Self {
            auth_version: 2,
            api_version: API_VERSION.to_owned(),
            service: "nexus-agent".to_owned(),
            status: HealthStatus::Ok,
            data_root_id,
            instance_id,
            binary_path: None,
            build_id: None,
            harness_config_wire_version: HARNESS_CONFIG_WIRE_VERSION,
        }
    }

    pub fn shutting_down(data_root_id: String, instance_id: String) -> Self {
        Self {
            auth_version: 2,
            api_version: API_VERSION.to_owned(),
            service: "nexus-agent".to_owned(),
            status: HealthStatus::ShuttingDown,
            data_root_id,
            instance_id,
            binary_path: None,
            build_id: None,
            harness_config_wire_version: HARNESS_CONFIG_WIRE_VERSION,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentLifecycleState {
    Starting,
    Running,
    ShuttingDown,
    Stopped,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HarnessState {
    Detached,
    Starting,
    Running,
    Stopped,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentStatePayload {
    pub lifecycle: AgentLifecycleState,
    pub harness: HarnessState,
    pub profile: Option<String>,
    pub release: Option<String>,
    pub started_at_unix: u64,
    pub updated_at_unix: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StateResponse {
    pub api_version: String,
    pub state: AgentStatePayload,
}

impl StateResponse {
    pub fn from_state(state: AgentStatePayload) -> Self {
        Self {
            api_version: API_VERSION.to_owned(),
            state,
        }
    }
}

/// Lifecycle commands are deliberately separate from future Harness commands.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleAction {
    Shutdown,
    /// Desktop replacement may proceed only after Agent positively proves idle.
    ShutdownIfIdle,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LifecycleCommand {
    pub action: LifecycleAction,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LifecycleAccepted {
    pub api_version: String,
    pub accepted: bool,
    pub action: LifecycleAction,
}

impl LifecycleAccepted {
    pub fn accepted(action: LifecycleAction) -> Self {
        Self {
            api_version: API_VERSION.to_owned(),
            accepted: true,
            action,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ErrorResponse {
    pub api_version: String,
    pub code: String,
    pub message: String,
}

/// Actions accepted by the external Harness supervisor endpoint.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HarnessAction {
    Start,
    Stop,
    Restart,
    Status,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HarnessCommand {
    pub action: HarnessAction,
}

/// Snapshot of the externally supervised Harness process.
///
/// Optional fields intentionally disappear from JSON when unavailable so
/// clients can consume the state even before a process has been started.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HarnessRuntimeInfo {
    pub state: HarnessState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at_unix: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at_unix: Option<u64>,
}

impl HarnessRuntimeInfo {
    pub fn detached() -> Self {
        Self {
            state: HarnessState::Detached,
            pid: None,
            exit_code: None,
            error: None,
            started_at_unix: None,
            updated_at_unix: None,
        }
    }

    pub fn starting(pid: u32, now_unix: u64) -> Self {
        Self {
            state: HarnessState::Starting,
            pid: Some(pid),
            exit_code: None,
            error: None,
            started_at_unix: Some(now_unix),
            updated_at_unix: Some(now_unix),
        }
    }

    pub fn running(pid: u32, started_at_unix: u64, now_unix: u64) -> Self {
        Self {
            state: HarnessState::Running,
            pid: Some(pid),
            exit_code: None,
            error: None,
            started_at_unix: Some(started_at_unix),
            updated_at_unix: Some(now_unix),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HarnessResponse {
    pub api_version: String,
    pub harness: HarnessRuntimeInfo,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_session_run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_session_generation: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_stdout_watermark: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_stderr_watermark: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_stdout_file_identity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_stderr_file_identity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_stdout_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_stderr_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_session_launch_pending: Option<bool>,
}

/// Alias retained as a descriptive name for clients that use GET semantics.
pub type HarnessStatusResponse = HarnessResponse;

impl HarnessResponse {
    pub fn from_runtime(harness: HarnessRuntimeInfo) -> Self {
        Self {
            api_version: API_VERSION.to_owned(),
            harness,
            generation: None,
            log_session_run_id: None,
            log_session_generation: None,
            log_stdout_watermark: None,
            log_stderr_watermark: None,
            log_stdout_file_identity: None,
            log_stderr_file_identity: None,
            log_stdout_name: None,
            log_stderr_name: None,
            log_session_launch_pending: None,
        }
    }

    pub fn from_observation(
        harness: HarnessRuntimeInfo,
        generation: u64,
        log_session_run_id: String,
        log_session_generation: u64,
        log_stdout_watermark: u64,
        log_stderr_watermark: u64,
        log_stdout_file_identity: String,
        log_stderr_file_identity: String,
        log_stdout_name: String,
        log_stderr_name: String,
        log_session_launch_pending: bool,
    ) -> Self {
        Self {
            api_version: API_VERSION.to_owned(),
            harness,
            generation: Some(generation),
            log_session_run_id: Some(log_session_run_id),
            log_session_generation: Some(log_session_generation),
            log_stdout_watermark: Some(log_stdout_watermark),
            log_stderr_watermark: Some(log_stderr_watermark),
            log_stdout_file_identity: Some(log_stdout_file_identity),
            log_stderr_file_identity: Some(log_stderr_file_identity),
            log_stdout_name: Some(log_stdout_name),
            log_stderr_name: Some(log_stderr_name),
            log_session_launch_pending: Some(log_session_launch_pending),
        }
    }
}

/// Requests for the Nexus-owned profile catalog.  The list and status
/// requests are represented by empty JSON objects so clients can use the same
/// versioned request envelope when needed.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProfileListRequest {}

pub type ProfileStatusRequest = ProfileListRequest;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProfileSelectRequest {
    pub profile: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProfileAction {
    List,
    Status,
    Select,
    CompatibilityCheck,
    PluginInventory,
    PluginRemove,
    PluginDisable,
    PluginEnable,
    PluginMove,
    PluginUndoMove,
    Create,
    Delete,
    DeletedList,
    RestoreDeleted,
    /// Open a profile-related file or directory with the system handler.
    /// Bounded targets only: `settings` (home settings.yaml), `profile_dir`,
    /// `profile_patch` (the profile's cordis.patch.yml), and
    /// `plugin_manifest` (the profile's package.json).
    OpenPath,
    /// Open an interactive terminal scoped to the selected profile: the
    /// working directory is the profile directory, `DSH_HOME` and the
    /// resolved runtime tools are injected, and generated `dsh`/`pnpm`
    /// shims are prepended to `PATH`.
    OpenTerminal,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProfileCommand {
    pub action: ProfileAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProfilePluginPayload {
    pub package: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub builtin: bool,
    pub removable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NativeProfilePayload {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order_undo_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_profile: Option<String>,
    pub bundles: Vec<String>,
    pub plugins: Vec<ProfilePluginPayload>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompatibilityDisabledPlugin {
    pub package: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompatibilityReport {
    #[serde(default)]
    pub declarations: Vec<serde_json::Value>,
    #[serde(default)]
    pub declarations_omitted: usize,
    pub checker_version: u32,
    pub status: String,
    pub source_profile: String,
    pub effective_profile: String,
    pub release_id: String,
    pub fingerprint: String,
    pub checked_at_unix: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked_disabled_plugins: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_trigger: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_at_unix: Option<u64>,
    #[serde(default)]
    pub cache_reused: bool,
    pub disabled: Vec<CompatibilityDisabledPlugin>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default)]
    pub candidates: Vec<CompatibilityDisabledPlugin>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProfileListResponse {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    pub api_version: String,
    pub active_profile: String,
    pub profiles: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub manifests: Vec<NativeProfilePayload>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compatibility: Option<CompatibilityReport>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disabled_plugins: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub legacy_selected_source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retired_profiles_directory: Option<String>,
}

pub type ProfileStatusResponse = ProfileListResponse;

impl ProfileListResponse {
    pub fn new(active_profile: impl Into<String>, profiles: Vec<String>) -> Self {
          Self {
              warnings: Vec::new(),
            api_version: API_VERSION.to_owned(),
            active_profile: active_profile.into(),
            profiles,
            manifests: Vec::new(),
            compatibility: None,
            disabled_plugins: Vec::new(),
            legacy_selected_source: None,
            retired_profiles_directory: None,
        }
    }

    pub fn with_manifests(mut self, manifests: Vec<NativeProfilePayload>) -> Self {
        self.manifests = manifests;
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProfileSelectResponse {
    pub api_version: String,
    pub selected: bool,
    pub profile: String,
    pub active_profile: String,
    pub profiles: Vec<String>,
}

impl ProfileSelectResponse {
    pub fn selected(profile: impl Into<String>, profiles: Vec<String>) -> Self {
        let profile = profile.into();
        Self {
            api_version: API_VERSION.to_owned(),
            selected: true,
            active_profile: profile.clone(),
            profile,
            profiles,
        }
    }
}

/// The Harness selection recorded by a checkpoint. Legacy manifests may
/// contain additional Agent/Harness runtime fields inside `state`; serde
/// intentionally ignores those fields when reading them, while new
/// serialization publishes only this selection.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HarnessCheckpointState {
    pub profile: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release: Option<String>,
}

/// Backward-compatible source alias for integrations which used the original
/// checkpoint state name. Its wire representation is selection-only.
pub type NexusStateSummary = HarnessCheckpointState;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CheckpointListRequest {}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CheckpointCreateRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl CheckpointCreateRequest {
    pub fn with_note(note: impl Into<String>) -> Self {
        Self {
            note: Some(note.into()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CheckpointRestoreRequest {
    pub id: String,
}

impl CheckpointRestoreRequest {
    pub fn new(id: impl Into<String>) -> Self {
        Self { id: id.into() }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointAction {
    #[default]
    List,
    Create,
    Detail,
    Inspect,
    Restore,
    Retry,
    Abort,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CheckpointCommand {
    pub action: CheckpointAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CheckpointManifest {
    pub id: String,
    pub created_at_unix: u64,
    pub profile: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub state: HarnessCheckpointState,
    /// Absent on checkpoints created before content snapshots were wired.
    /// Such manifests remain valid, but restore only the Harness selection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<SnapshotReference>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotKindPayload {
    Healthy,
    Manual,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SnapshotSummaryPayload {
    pub snapshot_id: String,
    pub created_unix_ms: u64,
    pub profile_name: String,
    pub kind: SnapshotKindPayload,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub dsh_version: String,
    pub plugin_count: u64,
    pub file_count: u64,
    pub total_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SnapshotReference {
    pub snapshot_id: String,
    pub summary: SnapshotSummaryPayload,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotFileStatePayload {
    Present,
    Missing,
    Omitted,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SnapshotFilePayload {
    pub path: String,
    pub state: SnapshotFileStatePayload,
    pub source_size: u64,
    pub stored_size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub redacted_paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub omitted_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default)]
    pub content_truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SnapshotDetailResponse {
    pub api_version: String,
    pub summary: SnapshotSummaryPayload,
    pub files: Vec<SnapshotFilePayload>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SnapshotInspectionPayload {
    pub snapshot_id: String,
    pub valid: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<SnapshotSummaryPayload>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<SnapshotFilePayload>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RecoveryLogTail {
    pub stream: String,
    pub content: String,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RecoveryStatusResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pause_error: Option<String>,
    #[serde(default)]
    pub paused: bool,
    pub api_version: String,
    pub manual_entry_available: bool,
    pub harness_stop_required: bool,
    pub harness: HarnessRuntimeInfo,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub startup_error: Option<String>,
    pub fatal_prefix_observed: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub log_tail: Vec<RecoveryLogTail>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostic_errors: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_restore: Option<CheckpointRestoreStatus>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointContentState {
    #[default]
    LegacyMetadataOnly,
    Prepared,
    Applied,
    MaterializationPending,
    Committed,
    RolledBack,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CheckpointRestoreStatus {
    pub checkpoint_id: String,
    pub snapshot_id: String,
    pub ticket_id: String,
    pub state: CheckpointContentState,
    pub materialization_pending: bool,
    pub retryable: bool,
    pub abortable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CheckpointListResponse {
    pub api_version: String,
    pub checkpoints: Vec<CheckpointManifest>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub snapshots: Vec<SnapshotInspectionPayload>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_restore: Option<CheckpointRestoreStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub healthy_capture_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_capture: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub inventory_refresh_pending: bool,
}

impl CheckpointListResponse {
    pub fn with_last_capture(mut self, value: serde_json::Value) -> Self {
        self.inventory_refresh_pending = value.get("state").and_then(serde_json::Value::as_str) == Some("running");
        self.last_capture = (!value.is_null()).then_some(value);
        self
    }

    pub fn new(checkpoints: Vec<CheckpointManifest>) -> Self {
        Self {
            api_version: API_VERSION.to_owned(),
            checkpoints,
            snapshots: Vec::new(),
            pending_restore: None,
            healthy_capture_error: None,
            last_capture: None,
            inventory_refresh_pending: false,
        }
    }

    pub fn with_snapshot_state(
        mut self,
        snapshots: Vec<SnapshotInspectionPayload>,
        pending_restore: Option<CheckpointRestoreStatus>,
        healthy_capture_error: Option<String>,
    ) -> Self {
        self.snapshots = snapshots;
        self.pending_restore = pending_restore;
        self.healthy_capture_error = healthy_capture_error;
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CheckpointCreateResponse {
    pub api_version: String,
    pub checkpoint: CheckpointManifest,
}

impl CheckpointCreateResponse {
    pub fn from_manifest(checkpoint: CheckpointManifest) -> Self {
        Self {
            api_version: API_VERSION.to_owned(),
            checkpoint,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CheckpointRestoreResponse {
    pub api_version: String,
    pub restored: bool,
    pub checkpoint: CheckpointManifest,
    #[serde(default)]
    pub content_state: CheckpointContentState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_restore: Option<CheckpointRestoreStatus>,
}

impl CheckpointRestoreResponse {
    pub fn restored(checkpoint: CheckpointManifest) -> Self {
        Self {
            api_version: API_VERSION.to_owned(),
            restored: true,
            checkpoint,
            content_state: CheckpointContentState::LegacyMetadataOnly,
            pending_restore: None,
        }
    }

    pub fn content(
        checkpoint: CheckpointManifest,
        restored: bool,
        content_state: CheckpointContentState,
        pending_restore: Option<CheckpointRestoreStatus>,
    ) -> Self {
        Self {
            api_version: API_VERSION.to_owned(),
            restored,
            checkpoint,
            content_state,
            pending_restore,
        }
    }
}

/// A Nexus-owned immutable Harness release slot.  The slot directory is
/// derived from `id` below the Nexus data root; the manifest deliberately
/// contains metadata only until the external update executor is introduced.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReleaseManifest {
    pub id: String,
    pub version: String,
    pub installed_at_unix: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Current and last-known-good pointers are stored separately from immutable
/// release manifests so promotion and rollback are one atomic metadata swap.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReleasePointers {
    pub current_release: Option<String>,
    pub last_known_good: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReleaseListResponse {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unavailable_selections: Vec<String>,
    pub api_version: String,
    pub current_release: Option<String>,
    pub last_known_good: Option<String>,
    pub releases: Vec<ReleaseManifest>,
}

impl ReleaseListResponse {
    pub fn new(
        current_release: Option<String>,
        last_known_good: Option<String>,
        releases: Vec<ReleaseManifest>,
    ) -> Self {
        Self {
            unavailable_selections: Vec::new(),
            api_version: API_VERSION.to_owned(),
            current_release,
            last_known_good,
            releases,
        }
    }
}

/// Status of one external runtime tool as observed on this machine.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimeToolStatus {
    pub name: String,
    pub available: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// "system" when resolved from PATH, "nexus" when a Nexus-owned portable
    /// runtime. For an unusable candidate, source/path identify what was
    /// rejected; both are absent when no candidate was found.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Stable machine-readable reason when a discovered candidate is not
    /// usable, for example `not_found` or `corepack_shim_unverified`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Read-only runtime environment observation for the settings UI.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimeListResponse {
    pub api_version: String,
    pub tools: Vec<RuntimeToolStatus>,
}

impl RuntimeListResponse {
    pub fn new(tools: Vec<RuntimeToolStatus>) -> Self {
        Self {
            api_version: API_VERSION.to_owned(),
            tools,
        }
    }
}

/// Ownership of one executable pinned in Nexus configuration.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeOwnership {
    System,
    Nexus,
    /// Runtime shipped inside the Nexus installation directory and upgraded
    /// with Nexus itself. Selected when no user pin and no system-wide tool
    /// satisfies the release requirements.
    Bundled,
}

/// Distribution source selected for a future runtime provisioning action.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeSource {
    #[default]
    Official,
    Npmmirror,
}

/// Installation boundary selected by the user. Portable is Nexus-owned and
/// changes only child process environments; system mode is externally owned.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeInstallMode {
    #[default]
    Portable,
    System,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimePinPayload {
    pub path: String,
    pub ownership: RuntimeOwnership,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimeConfigPayload {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<RuntimePinPayload>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pnpm: Option<RuntimePinPayload>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git: Option<RuntimePinPayload>,
    #[serde(default)]
    pub source: RuntimeSource,
    #[serde(default)]
    pub mode: RuntimeInstallMode,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimeNodeRequirement {
    pub manifest: String,
    pub range: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimePackageManagerRequirement {
    pub manifest: String,
    pub spec: String,
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimeRequirements {
    pub node: Vec<RuntimeNodeRequirement>,
    pub package_manager: RuntimePackageManagerRequirement,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RuntimePlanToolState {
    Reusable,
    Missing,
    Incompatible,
    Unverifiable,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimePlanTool {
    pub name: String,
    pub requirements: Vec<String>,
    pub state: RuntimePlanToolState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ownership: Option<RuntimeOwnership>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Non-fatal deviation accepted for reuse, for example a bundled pnpm
    /// whose major matches the release requirement while the exact version
    /// does not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RuntimePlanActionKind {
    UsePinned,
    UseExisting,
    ProvisionPortable,
    InstallSystem,
    ConfigureExternal,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimePlanAction {
    pub tool: String,
    pub action: RuntimePlanActionKind,
    pub reason: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RuntimePlanRequest {
    pub release_id: String,
    #[serde(default)]
    pub source: RuntimeSource,
    #[serde(default)]
    pub mode: RuntimeInstallMode,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimePlanResponse {
    pub api_version: String,
    /// Full deterministic serialization of this plan's revalidation facts.
    /// This is intentionally not presented as a cryptographic hash.
    pub plan_id: String,
    pub release_id: String,
    pub source: RuntimeSource,
    pub mode: RuntimeInstallMode,
    pub requirements: RuntimeRequirements,
    pub tools: Vec<RuntimePlanTool>,
    pub suggested_actions: Vec<RuntimePlanAction>,
}

/// Read-only enumeration of upstream git tags for the configured update
/// source. Tag names are rendered without the `refs/tags/` prefix and without
/// peeled `^{}` duplicates.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TagListResponse {
    pub api_version: String,
    pub source: String,
    pub tags: Vec<String>,
}

impl TagListResponse {
    pub fn new(source: String, tags: Vec<String>) -> Self {
        Self {
            api_version: API_VERSION.to_owned(),
            source,
            tags,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseAction {
    List,
    Current,
    Register,
    Promote,
    Rollback,
    Remove,
}

impl Default for ReleaseAction {
    fn default() -> Self {
        Self::List
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReleaseCommand {
    #[serde(default)]
    pub inspect_only: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rollback_confirmation: Option<String>,
    pub action: ReleaseAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UpdateAction {
    Status,
    Install,
    Switch,
    Confirm,
    Cancel,
    ClearFinished,
    PublicationRetry,
    PublicationAbandon,
    ConfigurationRetry,
    ConfigurationAbandon,
    OfflineImport,
    OfflineExport,
    OfflineInspect,
}

impl Default for UpdateAction {
    fn default() -> Self {
        Self::Status
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct UpdateCommand {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offline_contents: Option<OfflineContents>,
    pub action: UpdateAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<RuntimeSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<RuntimeInstallMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirmation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archive_path: Option<String>,
}

/// Result of an open-path request: what was opened and where it resolved.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProfileOpenPathResponse {
    pub target: String,
    pub path: String,
}

impl ProfileOpenPathResponse {
    pub fn new(target: String, path: String) -> Self {
        Self { target, path }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginRemoveResponse {
    pub api_version: String,
    pub profile: String,
    pub package: String,
    pub removed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub stdout: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub stderr: String,
    pub inventory: NativeProfilePayload,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ColdOperationPhase {
    Queued,
    Cloning,
    Planning,
    AwaitingConfirmation,
    Supplying,
    Installing,
    Building,
    Verifying,
    Registering,
    Promoting,
    Cancelling,
    Prepared,
    Succeeded,
    Cancelled,
    Failed,
}

impl ColdOperationPhase {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Prepared | Self::Succeeded | Self::Cancelled | Self::Failed)
    }
}

fn default_cold_operation_kind() -> String { "cold_switch".into() }

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OfflineContents {
    #[serde(default)]
    pub credential_policy: CredentialConflictPolicy,
    #[serde(default = "offline_runtime_default")]
    pub runtime: bool,
    #[serde(default)]
    pub environment: Option<bool>,
    #[serde(default)]
    pub sessions: bool,
    #[serde(default)]
    pub profiles: Vec<String>,
    #[serde(default)]
    pub configuration: bool,
    #[serde(default)]
    pub plugins: bool,
    #[serde(default)]
    pub credentials: bool,
}

fn offline_runtime_default() -> bool { true }

impl Default for OfflineContents {
    fn default() -> Self { Self { credential_policy: CredentialConflictPolicy::Preserve, runtime: true, environment: None, sessions: false, profiles: Vec::new(), configuration: false, plugins: false, credentials: false } }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CredentialConflictPolicy { #[default] Preserve, Replace }

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ColdOperation {
    #[serde(default)]
    pub process_owner_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_recovery_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offline_contents: Option<OfflineContents>,
    pub operation_id: String,
    #[serde(default = "default_cold_operation_kind")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archive_path: Option<String>,
    pub phase: ColdOperationPhase,
    pub tag: String,
    pub source: RuntimeSource,
    pub mode: RuntimeInstallMode,
    pub release_id: String,
    pub candidate: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate_revision: Option<String>,
    pub progress_percent: u8,
    pub started_at_unix: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at_unix: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub foundation_plan_id: Option<String>,
    /// Accepted plan deviation surfaced to the user, for example a bundled
    /// pnpm whose major matches the release requirement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
    /// Bounded tail of the running command output, refreshed while the
    /// install or build streams its diagnostics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supply_plan: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirmation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// True once the process owner has proved that no command tree remains.
    #[serde(default)]
    pub owner_quiescent: bool,
    /// Keeps the mutation gate closed while server-owned residue remains.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cleanup_pending: bool,
    /// Secondary cleanup/reconciliation failure; never replaces `error`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cleanup_error: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UpdateState {
    Idle,
    Running,
    Prepared,
    Succeeded,
    Failed,
}

impl Default for UpdateState {
    fn default() -> Self {
        Self::Idle
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct UpdateRuntimeInfo {
    pub state: UpdateState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at_unix: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at_unix: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl UpdateRuntimeInfo {
    pub fn idle() -> Self {
        Self {
            state: UpdateState::Idle,
            release_id: None,
            started_at_unix: None,
            finished_at_unix: None,
            exit_code: None,
            error: None,
        }
    }

    pub fn running(release_id: String, started_at_unix: u64) -> Self {
        Self {
            state: UpdateState::Running,
            release_id: Some(release_id),
            started_at_unix: Some(started_at_unix),
            finished_at_unix: None,
            exit_code: None,
            error: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpdateResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offline_progress: Option<serde_json::Value>,
    pub api_version: String,
    pub update: UpdateRuntimeInfo,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release: Option<ReleaseManifest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation: Option<ColdOperation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install_operation: Option<InstallOperation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publication_recovery: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub configuration_recovery: Option<serde_json::Value>,
}

/// Ordinary configured installation; distinct from a cold version switch.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InstallOperation {
    #[serde(default)]
    pub process_owner_version: u32,
    pub operation_id: String,
    #[serde(default)]
    pub job_name: Option<String>,
    pub release_id: String,
    pub candidate: String,
    pub phase: String,
    pub cancel_requested: bool,
    pub owner_quiescent: bool,
    pub cleanup_pending: bool,
    pub error: Option<String>,
    pub cleanup_error: Option<String>,
}

impl UpdateResponse {
    pub fn new(update: UpdateRuntimeInfo, release: Option<ReleaseManifest>) -> Self {
        Self {
            api_version: API_VERSION.to_owned(),
            update,
            release,
            operation: None,
            install_operation: None,
            offline_progress: None,
            publication_recovery: None,
            configuration_recovery: None,
        }
    }

    pub fn with_operation(mut self, operation: ColdOperation) -> Self {
        self.operation = Some(operation);
        self
    }
}

/// Nexus-owned diagnostic collection actions. Collection only reads a fixed
/// allowlist of Nexus metadata and redacted text logs.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticsAction {
    Status,
    Collect,
    Export,
    OpenPath,
}

impl Default for DiagnosticsAction {
    fn default() -> Self {
        Self::Status
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DiagnosticsCommand {
    pub action: DiagnosticsAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DiagnosticsFile {
    pub name: String,
    pub bytes: u64,
    #[serde(default)]
    pub redacted: bool,
    #[serde(default)]
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DiagnosticsBundle {
    pub id: String,
    pub created_at_unix: u64,
    pub directory: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub files: Vec<DiagnosticsFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiagnosticsResponse {
    pub api_version: String,
    pub bundles: Vec<DiagnosticsBundle>,
}

impl DiagnosticsResponse {
    pub fn new(bundles: Vec<DiagnosticsBundle>) -> Self {
        Self {
            api_version: API_VERSION.to_owned(),
            bundles,
        }
    }
}

/// How the immutable upstream Harness is launched.
///
/// `direct` preserves the original command model. `node` means `program` is
/// the Node runtime and `entry` names the JavaScript entry point; the Agent
/// normalizes that pair into the process argument vector it supervises.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HarnessLaunchMode {
    #[default]
    Direct,
    Node,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HarnessConfigPayload {
    #[serde(default)]
    pub mode: HarnessLaunchMode,
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// Set by a v2 caller when `args` excludes the separate Node `entry`.
    /// Omitted/false retains the legacy interpretation for older callers that
    /// included the entry as `args[0]`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub args_are_additional: bool,
    /// JavaScript entry point used by `node` mode. The persisted core spec
    /// keeps this as the first process argument for supervisor compatibility.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub readiness_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub readiness_timeout_secs: Option<u64>,
    /// When true, readiness is accepted only after a fresh Harness URL/token
    /// is observed in the current Nexus-owned log session. This is useful for
    /// TCP readiness where a listener alone cannot identify the process.
    #[serde(default, skip_serializing_if = "is_false")]
    pub readiness_token_required: bool,
}

/// A locally discoverable Harness launch target. Discovery is advisory: the
/// caller still explicitly chooses a candidate (or supplies a manual config)
/// before mutating Nexus configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HarnessCandidate {
    /// Stable for the lifetime of the installation and suitable for UI
    /// selection; it is not an authorization token.
    pub id: String,
    pub mode: HarnessLaunchMode,
    /// Direct mode uses this as the Harness program. Node mode uses this as
    /// the Node runtime executable.
    pub program: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub readiness_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub readiness_timeout_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub readiness_token_required: bool,
    /// Stable machine-readable discovery source, such as `path` or `home`.
    pub source: String,
    pub display_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HarnessDiscoveryResponse {
    pub api_version: String,
    pub candidates: Vec<HarnessCandidate>,
}

impl HarnessDiscoveryResponse {
    pub fn new(candidates: Vec<HarnessCandidate>) -> Self {
        Self {
            api_version: API_VERSION.to_owned(),
            candidates,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpdateConfigPayload {
    pub source: String,
    #[serde(default)]
    pub ref_name: String,
    #[serde(default)]
    pub git_program: String,
    #[serde(default)]
    pub build_program: Option<String>,
    #[serde(default)]
    pub build_args: Vec<String>,
    #[serde(default)]
    pub verify_program: Option<String>,
    #[serde(default)]
    pub verify_args: Vec<String>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SnapshotsConfigPayload {
    pub healthy_slots: u32,
    pub max_manual_snapshots: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HarnessPreferencesPayload {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub patch_entries: Option<Vec<HarnessPatchEntry>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_browser: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub telemetry_disabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deepseek_base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search_base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search_provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fetch_provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents_home: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundled_skill_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub patches: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools_mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens_as_success: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HarnessPatchEntry {
    pub source: String,
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github_ref_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github_ref_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github_file_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_identity: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConfigAction {
    Status,
    FetchHarnessPatches,
    ListHarnessPatchRefs,
    PreviewHarnessPatches,
    ApplyHarnessPatchPreview,
    DiscardHarnessPatchPreview,
    SetHarnessPreferences,
    SetHarness,
    SetExternalHarness,
    ClearExternalHarness,
    ClearHarness,
    SetUpdate,
    SetUpdateSource,
    ClearUpdate,
    SetRuntime,
    ClearRuntime,
    SetSnapshots,
    ClearSnapshots,
}

impl Default for ConfigAction {
    fn default() -> Self {
        Self::Status
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ConfigCommand {
    #[serde(default)]
    pub external_harness_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub patch_query: Option<HarnessPatchQuery>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<String>,
    pub action: ConfigAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_preferences: Option<HarnessPreferencesPayload>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<HarnessConfigPayload>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update: Option<UpdateConfigPayload>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<RuntimeConfigPayload>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshots: Option<SnapshotsConfigPayload>,
    /// Preserve a readiness URL whose sensitive query/userinfo was redacted
    /// from a prior ConfigResponse. The Agent resolves it from its existing
    /// on-disk Harness spec before validation.
    #[serde(default, skip_serializing_if = "is_false")]
    pub preserve_harness_readiness_url: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct HarnessPatchQuery {
    #[serde(default)]
    pub entry: Option<HarnessPatchEntry>,
    #[serde(default)]
    pub page: Option<u16>,
    #[serde(default)]
    pub preview_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConfigResponse {
    #[serde(default)]
    pub external_harness: Option<serde_json::Value>,
    pub revision: String,
    pub api_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_inputs: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_preferences: Option<HarnessPreferencesPayload>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<HarnessConfigPayload>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update: Option<UpdateConfigPayload>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<RuntimeConfigPayload>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshots: Option<SnapshotsConfigPayload>,
    /// True when the returned readiness URL is display-safe but shorter than
    /// the on-disk value. Editors must preserve it unless the user replaces or
    /// clears the field explicitly.
    #[serde(default, skip_serializing_if = "is_false")]
    pub harness_readiness_url_redacted: bool,
    /// True when one or more documented `NEXUS_HARNESS_*` variables override
    /// the persisted Harness section returned by this response.
    #[serde(default, skip_serializing_if = "is_false")]
    pub harness_env_override: bool,
    /// True when one or more documented `NEXUS_UPDATE_*` variables override
    /// the persisted update section returned by this response.
    #[serde(default, skip_serializing_if = "is_false")]
    pub update_env_override: bool,
}

impl ConfigResponse {
    pub fn new(harness: Option<HarnessConfigPayload>, update: Option<UpdateConfigPayload>) -> Self {
        Self {
            api_version: API_VERSION.to_owned(),
            revision: String::new(),
            external_harness: None,
            harness,
            update,
            launch_inputs: None,
            runtime: None,
            snapshots: None,
            harness_preferences: None,
            harness_readiness_url_redacted: false,
            harness_env_override: false,
            update_env_override: false,
        }
    }

    pub fn with_runtime(mut self, runtime: Option<RuntimeConfigPayload>) -> Self {
        self.runtime = runtime;
        self
    }

    pub fn with_harness_preferences(mut self, preferences: Option<HarnessPreferencesPayload>) -> Self {
        self.harness_preferences = preferences;
        self
    }

    pub fn with_snapshots(mut self, snapshots: Option<SnapshotsConfigPayload>) -> Self {
        self.snapshots = snapshots;
        self
    }

    pub fn with_harness_readiness_url_redacted(mut self, redacted: bool) -> Self {
        self.harness_readiness_url_redacted = redacted;
        self
    }

    pub fn with_environment_overrides(mut self, harness: bool, update: bool) -> Self {
        self.harness_env_override = harness;
        self.update_env_override = update;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CheckpointCreateRequest, CheckpointCreateResponse, CheckpointManifest,
        CheckpointRestoreRequest, ConfigAction, ConfigCommand, ConfigResponse, DiagnosticsAction,
        DiagnosticsCommand, DiagnosticsResponse, HarnessAction, HarnessCheckpointState,
        HarnessCommand, HarnessConfigPayload, HarnessLaunchMode, HarnessResponse,
        HarnessRuntimeInfo, HarnessState, LifecycleAction, LifecycleCommand, ProfileListResponse,
        ProfileSelectRequest, ReleaseAction, ReleaseCommand, ReleaseListResponse, ReleaseManifest,
        RuntimeConfigPayload, RuntimeInstallMode, RuntimeListResponse, RuntimeOwnership,
        RuntimePinPayload, RuntimePlanRequest, RuntimeSource, RuntimeToolStatus, UpdateAction,
        UpdateCommand, UpdateResponse, UpdateRuntimeInfo,
    };

    #[test]
    fn lifecycle_command_uses_stable_snake_case_json() {
        let command = LifecycleCommand {
            action: LifecycleAction::Shutdown,
        };

        let value = serde_json::to_value(command).expect("command serializes");
        assert_eq!(value["action"], "shutdown");
    }

    #[test]
    fn harness_command_and_runtime_use_stable_v1_json() {
        let command = HarnessCommand {
            action: HarnessAction::Restart,
        };
        let command_json = serde_json::to_value(command).expect("command serializes");
        assert_eq!(command_json["action"], "restart");

        let response = HarnessResponse::from_runtime(HarnessRuntimeInfo {
            state: HarnessState::Running,
            pid: Some(42),
            exit_code: None,
            error: None,
            started_at_unix: Some(100),
            updated_at_unix: Some(101),
        });
        let response_json = serde_json::to_value(response).expect("response serializes");
        assert_eq!(response_json["api_version"], "v1");
        assert_eq!(response_json["harness"]["state"], "running");
        assert_eq!(response_json["harness"]["pid"], 42);
        assert!(response_json["harness"].get("exit_code").is_none());
        assert!(response_json.get("generation").is_none());

        let observed = HarnessResponse::from_observation(
            HarnessRuntimeInfo::detached(),
            7,
            "run-7".to_owned(),
            3,
            100,
            200,
            "stdout-identity".to_owned(),
            "stderr-identity".to_owned(),
            "harness-run-7.stdout.log".to_owned(),
            "harness-run-7.stderr.log".to_owned(),
            true,
        );
        let observed_json = serde_json::to_value(observed).expect("observation serializes");
        assert_eq!(observed_json["generation"], 7);
        assert_eq!(observed_json["log_session_run_id"], "run-7");
        assert_eq!(observed_json["log_session_generation"], 3);
        assert_eq!(observed_json["log_stdout_watermark"], 100);
        assert_eq!(observed_json["log_stderr_watermark"], 200);
        assert_eq!(observed_json["log_stdout_file_identity"], "stdout-identity");
        assert_eq!(observed_json["log_stderr_file_identity"], "stderr-identity");
        assert_eq!(observed_json["log_stdout_name"], "harness-run-7.stdout.log");
        assert_eq!(observed_json["log_stderr_name"], "harness-run-7.stderr.log");
        assert_eq!(observed_json["log_session_launch_pending"], true);
    }

    #[test]
    fn harness_runtime_round_trips_optional_exit_metadata() {
        let json = r#"{
            "state":"failed",
            "exit_code":17,
            "error":"child exited"
        }"#;
        let runtime: HarnessRuntimeInfo = serde_json::from_str(json).expect("runtime parses");
        assert_eq!(runtime.state, HarnessState::Failed);
        assert_eq!(runtime.exit_code, Some(17));
        assert_eq!(runtime.error.as_deref(), Some("child exited"));
        assert_eq!(runtime.pid, None);
    }

    #[test]
    fn runtime_list_round_trips_sources_paths_and_unavailable_tools() {
        let response = RuntimeListResponse::new(vec![
            RuntimeToolStatus {
                name: "node".to_owned(),
                available: true,
                version: Some("v24.19.0".to_owned()),
                source: Some("system".to_owned()),
                path: Some("C:\\Program Files\\nodejs\\node.exe".to_owned()),
                reason: None,
            },
            RuntimeToolStatus {
                name: "pnpm".to_owned(),
                available: true,
                version: Some("10.15.0".to_owned()),
                source: Some("nexus".to_owned()),
                path: Some("C:\\Users\\test\\Nexus\\runtimes\\pnpm.cmd".to_owned()),
                reason: None,
            },
            RuntimeToolStatus {
                name: "git".to_owned(),
                available: false,
                version: None,
                source: None,
                path: None,
                reason: Some("not_found".to_owned()),
            },
        ]);
        let json = serde_json::to_value(&response).expect("runtime list serializes");
        assert_eq!(json["api_version"], "v1");
        assert_eq!(json["tools"][0]["source"], "system");
        assert_eq!(json["tools"][1]["source"], "nexus");
        assert!(json["tools"][2].get("version").is_none());
        assert!(json["tools"][2].get("path").is_none());

        let decoded: RuntimeListResponse =
            serde_json::from_value(json).expect("runtime list deserializes");
        assert_eq!(decoded, response);

        let legacy: RuntimeToolStatus = serde_json::from_value(serde_json::json!({
            "name": "git",
            "available": false
        }))
        .expect("legacy runtime status deserializes");
        assert_eq!(legacy.reason, None);
    }

    #[test]
    fn runtime_plan_request_defaults_preferences_and_rejects_manifest_paths() {
        let request: RuntimePlanRequest = serde_json::from_value(serde_json::json!({
            "release_id": "release-a"
        }))
        .expect("minimal runtime plan request parses");
        assert_eq!(request.source, RuntimeSource::Official);
        assert_eq!(request.mode, RuntimeInstallMode::Portable);
        assert!(
            serde_json::from_value::<RuntimePlanRequest>(serde_json::json!({
                "release_id": "release-a",
                "manifest_path": "C:/outside/package.json"
            }))
            .is_err()
        );
    }

    #[test]
    fn runtime_config_protocol_uses_explicit_pin_ownership() {
        let payload = RuntimeConfigPayload {
            node: Some(RuntimePinPayload {
                path: "C:\\Nexus\\runtimes\\node.exe".to_owned(),
                ownership: RuntimeOwnership::Nexus,
            }),
            source: RuntimeSource::Npmmirror,
            ..RuntimeConfigPayload::default()
        };
        let json = serde_json::to_value(payload).expect("runtime config serializes");
        assert_eq!(json["node"]["ownership"], "nexus");
        assert_eq!(json["source"], "npmmirror");
        assert_eq!(json["mode"], "portable");

        let set_runtime =
            serde_json::to_value(ConfigAction::SetRuntime).expect("runtime action serializes");
        let clear_runtime =
            serde_json::to_value(ConfigAction::ClearRuntime).expect("runtime action serializes");
        assert_eq!(set_runtime, "set_runtime");
        assert_eq!(clear_runtime, "clear_runtime");
    }

    #[test]
    fn profile_protocol_uses_stable_v1_json() {
        let request = ProfileSelectRequest {
            profile: "web.dark".to_owned(),
        };
        let request_json = serde_json::to_value(request).expect("profile request serializes");
        assert_eq!(request_json, serde_json::json!({"profile":"web.dark"}));

        let response =
            ProfileListResponse::new("web.dark", vec!["web".to_owned(), "web.dark".to_owned()]);
        let response_json = serde_json::to_value(response).expect("profile response serializes");
        assert_eq!(response_json["api_version"], "v1");
        assert_eq!(response_json["active_profile"], "web.dark");
        assert_eq!(response_json["profiles"][0], "web");
    }

    #[test]
    fn checkpoint_protocol_round_trips_manifest_and_requests() {
        let create = CheckpointCreateRequest::with_note("before migration");
        let create_json = serde_json::to_value(create).expect("checkpoint create serializes");
        assert_eq!(create_json["note"], "before migration");

        let manifest = CheckpointManifest {
            id: "cp-123".to_owned(),
            created_at_unix: 123,
            profile: "web".to_owned(),
            release: Some("r1".to_owned()),
            note: Some("before migration".to_owned()),
            state: HarnessCheckpointState {
                profile: "web".to_owned(),
                release: Some("r1".to_owned()),
            },
            snapshot: None,
        };
        let response = CheckpointCreateResponse::from_manifest(manifest.clone());
        let encoded = serde_json::to_vec(&response).expect("checkpoint response serializes");
        let value: serde_json::Value =
            serde_json::from_slice(&encoded).expect("checkpoint response JSON parses");
        let state = value["checkpoint"]["state"]
            .as_object()
            .expect("checkpoint state is an object");
        assert_eq!(state.len(), 2);
        assert_eq!(state["profile"], "web");
        assert_eq!(state["release"], "r1");
        assert!(!state.contains_key("lifecycle"));
        assert!(!state.contains_key("harness"));
        assert!(!state.contains_key("updated_at_unix"));
        let decoded: CheckpointCreateResponse =
            serde_json::from_slice(&encoded).expect("checkpoint response parses");
        assert_eq!(decoded.checkpoint, manifest);

        let legacy = serde_json::json!({
            "id": "cp-legacy",
            "created_at_unix": 122,
            "profile": "web",
            "release": "r1",
            "state": {
                "lifecycle": "running",
                "harness": "stopped",
                "profile": "web",
                "release": "r1",
                "updated_at_unix": 122
            }
        });
        let legacy: CheckpointManifest =
            serde_json::from_value(legacy).expect("legacy checkpoint manifest parses");
        assert_eq!(legacy.state.profile, "web");
        assert_eq!(legacy.state.release.as_deref(), Some("r1"));
        assert_eq!(legacy.snapshot, None);
        let migrated = serde_json::to_value(legacy).expect("legacy checkpoint reserializes");
        assert!(migrated["state"].get("lifecycle").is_none());
        assert!(migrated["state"].get("harness").is_none());
        assert!(migrated["state"].get("updated_at_unix").is_none());

        let restore = CheckpointRestoreRequest::new("cp-123");
        assert_eq!(
            serde_json::to_value(restore).expect("restore serializes")["id"],
            "cp-123"
        );
    }

    #[test]
    fn release_protocol_uses_stable_pointer_and_manifest_json() {
        let command = ReleaseCommand {
            action: ReleaseAction::Promote,
            id: Some("harness-rc1".to_owned()),
            ..ReleaseCommand::default()
        };
        let command_json = serde_json::to_value(command).expect("release command serializes");
        assert_eq!(command_json["action"], "promote");
        assert_eq!(command_json["id"], "harness-rc1");
        assert!(command_json.get("version").is_none());

        let response = ReleaseListResponse::new(
            Some("harness-rc1".to_owned()),
            Some("harness-alpha5".to_owned()),
            vec![ReleaseManifest {
                id: "harness-rc1".to_owned(),
                version: "rc.1".to_owned(),
                installed_at_unix: 123,
                source: Some("git".to_owned()),
                note: None,
            }],
        );
        let json = serde_json::to_value(response).expect("release response serializes");
        assert_eq!(json["api_version"], "v1");
        assert_eq!(json["current_release"], "harness-rc1");
        assert_eq!(json["releases"][0]["version"], "rc.1");
    }

    #[test]
    fn update_protocol_uses_stable_install_json() {
        let command = UpdateCommand {
            action: UpdateAction::Install,
            tag: None,
            release_id: Some("harness-rc1".to_owned()),
            version: Some("rc.1".to_owned()),
            ..UpdateCommand::default()
        };
        let json = serde_json::to_value(command).expect("update command serializes");
        assert_eq!(json["action"], "install");
        assert_eq!(json["release_id"], "harness-rc1");

        let confirm = UpdateCommand {
            action: UpdateAction::Confirm,
            operation_id: Some("cold-1".to_owned()),
            confirmation: Some("sha256:abc".to_owned()),
            ..UpdateCommand::default()
        };
        let json = serde_json::to_value(confirm).expect("confirmation serializes");
        assert_eq!(json["action"], "confirm");
        assert_eq!(json["operation_id"], "cold-1");

        let response = UpdateResponse::new(
            UpdateRuntimeInfo::running("harness-rc1".to_owned(), 100),
            None,
        );
        let response_json = serde_json::to_value(response).expect("update response serializes");
        assert_eq!(response_json["api_version"], "v1");
        assert_eq!(response_json["update"]["state"], "running");
        assert!(response_json["update"].get("finished_at_unix").is_none());
    }

    #[test]
    fn diagnostics_protocol_marks_redaction_and_uses_v1_json() {
        let command = DiagnosticsCommand {
            action: DiagnosticsAction::Collect,
            note: Some("before update".to_owned()),
            ..Default::default()
        };
        let command_json = serde_json::to_value(command).expect("diagnostics command serializes");
        assert_eq!(command_json["action"], "collect");
        assert_eq!(command_json["note"], "before update");

        let response = DiagnosticsResponse::new(Vec::new());
        let response_json =
            serde_json::to_value(response).expect("diagnostics response serializes");
        assert_eq!(response_json["api_version"], "v1");
        assert!(response_json["bundles"]
            .as_array()
            .is_some_and(Vec::is_empty));
    }

    #[test]
    fn config_protocol_round_trips_explicit_mutation_actions() {
        let command = ConfigCommand {
            external_harness_path: None,
            patch_query: None,
            expected_revision: Some("test-revision".into()),
            harness_preferences: None,
            action: ConfigAction::SetHarness,
            harness: Some(HarnessConfigPayload {
                mode: HarnessLaunchMode::Direct,
                program: "harness".to_owned(),
                args: vec!["--profile".to_owned(), "{profile}".to_owned()],
                args_are_additional: false,
                entry: None,
                working_dir: None,
                readiness_url: None,
                readiness_timeout_secs: None,
                readiness_token_required: false,
            }),
            update: None,
            runtime: None,
            snapshots: None,
            preserve_harness_readiness_url: false,
        };
        let json = serde_json::to_value(command).expect("config command serializes");
        assert_eq!(json["action"], "set_harness");
        assert_eq!(json["harness"]["args"][1], "{profile}");

        let response = ConfigResponse::new(None, None);
        assert_eq!(
            serde_json::to_value(response).expect("config response serializes")["api_version"],
            "v1"
        );
    }

    #[test]
    fn harness_launch_mode_defaults_for_legacy_payloads_and_round_trips_node() {
        let legacy: HarnessConfigPayload = serde_json::from_value(serde_json::json!({
            "program": "harness"
        }))
        .expect("legacy payload deserializes");
        assert_eq!(legacy.mode, HarnessLaunchMode::Direct);
        assert_eq!(legacy.entry, None);

        let node = HarnessConfigPayload {
            mode: HarnessLaunchMode::Node,
            program: "node".to_owned(),
            args: vec!["--port".to_owned(), "3080".to_owned()],
            args_are_additional: true,
            entry: Some("dist/index.js".to_owned()),
            working_dir: Some("harness".to_owned()),
            readiness_url: None,
            readiness_timeout_secs: Some(20),
            readiness_token_required: false,
        };
        let value = serde_json::to_value(node).expect("node payload serializes");
        assert_eq!(value["mode"], "node");
        assert_eq!(value["entry"], "dist/index.js");
    }
}


#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CanaryAction { Start, Cancel, History }
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CanaryMode { DiagnosticOnly, Bisect }
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CanaryCommand {
    pub action: CanaryAction,
    pub mode: Option<CanaryMode>,
    pub profile: Option<String>,
    pub operation_id: Option<String>,
}
/// Operations whose callers must retain one request ID across uncertain retries.
pub fn request_receipt_kind(path: &str, action: &str) -> Option<&'static str> {
    match (path, action) {
        ("/v1/harness", "restart") => Some("harness_restart"),
        ("/v1/releases", "rollback") => Some("rollback"),
        ("/v1/updates", "switch") => Some("cold_switch"),
        ("/v1/updates", "offline_import") => Some("offline_import"),
        ("/v1/checkpoints", "restore") => Some("snapshot_restore"),
        ("/v1/profiles", "delete") => Some("profile_delete"),
        ("/v1/profiles", "restore_deleted") => Some("profile_restore"),
        _ => None,
    }
}
