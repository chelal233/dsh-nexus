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

/// Health status is intentionally small so it can be consumed by scripts.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HealthStatus {
    Ok,
    ShuttingDown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HealthResponse {
    pub api_version: String,
    pub service: String,
    pub status: HealthStatus,
}

impl HealthResponse {
    pub fn healthy() -> Self {
        Self {
            api_version: API_VERSION.to_owned(),
            service: "nexus-agent".to_owned(),
            status: HealthStatus::Ok,
        }
    }

    pub fn shutting_down() -> Self {
        Self {
            api_version: API_VERSION.to_owned(),
            service: "nexus-agent".to_owned(),
            status: HealthStatus::ShuttingDown,
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
pub struct HarnessCommand {
    pub action: HarnessAction,
}

/// Snapshot of the externally supervised Harness process.
///
/// Optional fields intentionally disappear from JSON when unavailable so
/// clients can consume the state even before a process has been started.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
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
}

/// Alias retained as a descriptive name for clients that use GET semantics.
pub type HarnessStatusResponse = HarnessResponse;

impl HarnessResponse {
    pub fn from_runtime(harness: HarnessRuntimeInfo) -> Self {
        Self {
            api_version: API_VERSION.to_owned(),
            harness,
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
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProfileCommand {
    pub action: ProfileAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProfileListResponse {
    pub api_version: String,
    pub active_profile: String,
    pub profiles: Vec<String>,
}

pub type ProfileStatusResponse = ProfileListResponse;

impl ProfileListResponse {
    pub fn new(active_profile: impl Into<String>, profiles: Vec<String>) -> Self {
        Self {
            api_version: API_VERSION.to_owned(),
            active_profile: active_profile.into(),
            profiles,
        }
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

/// The Nexus-only state included in a checkpoint.  It is deliberately a
/// summary rather than a copy of Harness data or credentials.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NexusStateSummary {
    pub lifecycle: AgentLifecycleState,
    pub harness: HarnessState,
    pub profile: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release: Option<String>,
    pub updated_at_unix: u64,
}

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

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointAction {
    List,
    Create,
    Restore,
}

impl Default for CheckpointAction {
    fn default() -> Self {
        Self::List
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
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
    pub state: NexusStateSummary,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CheckpointListResponse {
    pub api_version: String,
    pub checkpoints: Vec<CheckpointManifest>,
}

impl CheckpointListResponse {
    pub fn new(checkpoints: Vec<CheckpointManifest>) -> Self {
        Self {
            api_version: API_VERSION.to_owned(),
            checkpoints,
        }
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
}

impl CheckpointRestoreResponse {
    pub fn restored(checkpoint: CheckpointManifest) -> Self {
        Self {
            api_version: API_VERSION.to_owned(),
            restored: true,
            checkpoint,
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
            api_version: API_VERSION.to_owned(),
            current_release,
            last_known_good,
            releases,
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
}

impl Default for ReleaseAction {
    fn default() -> Self {
        Self::List
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReleaseCommand {
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

#[cfg(test)]
mod tests {
    use super::{
        AgentLifecycleState, CheckpointCreateRequest, CheckpointCreateResponse, CheckpointManifest,
        CheckpointRestoreRequest, HarnessAction, HarnessCommand, HarnessResponse,
        HarnessRuntimeInfo, HarnessState, LifecycleAction, LifecycleCommand, NexusStateSummary,
        ProfileListResponse, ProfileSelectRequest, ReleaseAction, ReleaseCommand,
        ReleaseListResponse, ReleaseManifest,
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
            state: NexusStateSummary {
                lifecycle: AgentLifecycleState::Stopped,
                harness: HarnessState::Stopped,
                profile: "web".to_owned(),
                release: Some("r1".to_owned()),
                updated_at_unix: 123,
            },
        };
        let response = CheckpointCreateResponse::from_manifest(manifest.clone());
        let encoded = serde_json::to_vec(&response).expect("checkpoint response serializes");
        let decoded: CheckpointCreateResponse =
            serde_json::from_slice(&encoded).expect("checkpoint response parses");
        assert_eq!(decoded.checkpoint, manifest);

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
}
