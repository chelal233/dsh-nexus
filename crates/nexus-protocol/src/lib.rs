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

#[cfg(test)]
mod tests {
    use super::{
        HarnessAction, HarnessCommand, HarnessResponse, HarnessRuntimeInfo, HarnessState,
        LifecycleAction, LifecycleCommand,
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
}
