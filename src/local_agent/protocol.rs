use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;
pub const MAX_IPC_MESSAGE_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentState {
    pub protocol_version: u32,
    pub pid: u32,
    pub port: u16,
    pub token: String,
    pub started_at_unix_ms: i64,
    pub executable_stamp: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentEntry {
    pub key: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CliInvocation {
    pub args: Vec<String>,
    pub current_directory: String,
    pub environment: Vec<EnvironmentEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum AgentAction {
    Ping,
    Execute { invocation: CliInvocation },
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRequest {
    pub protocol_version: u32,
    pub request_id: String,
    pub token: String,
    pub action: AgentAction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentSubsystem {
    CaptureEngine,
    RestoreEngine,
    Sqlite,
    AdapterHost,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentResponse {
    pub protocol_version: u32,
    pub request_id: String,
    pub ok: bool,
    pub exit_code: u8,
    pub stdout: String,
    pub stderr: String,
    pub error: Option<String>,
    pub subsystem: Option<AgentSubsystem>,
    pub agent_pid: u32,
}

impl AgentResponse {
    pub fn protocol_error(request_id: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            request_id: request_id.into(),
            ok: false,
            exit_code: 1,
            stdout: String::new(),
            stderr: String::new(),
            error: Some(message.into()),
            subsystem: None,
            agent_pid: std::process::id(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trip_preserves_cli_context() {
        let request = AgentRequest {
            protocol_version: PROTOCOL_VERSION,
            request_id: "request-1".to_owned(),
            token: "secret".to_owned(),
            action: AgentAction::Execute {
                invocation: CliInvocation {
                    args: vec!["restore".to_owned(), "work@2".to_owned(), "--dry-run".to_owned()],
                    current_directory: "C:/work/project".to_owned(),
                    environment: vec![EnvironmentEntry {
                        key: "CONTEXT_CAPSULE_DB".to_owned(),
                        value: "C:/tmp/capsules.db".to_owned(),
                    }],
                },
            },
        };
        let encoded = serde_json::to_string(&request).unwrap();
        let decoded: AgentRequest = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, request);
    }

    #[test]
    fn subsystem_names_are_stable_kebab_case_wire_values() {
        assert_eq!(
            serde_json::to_string(&AgentSubsystem::CaptureEngine).unwrap(),
            "\"capture-engine\""
        );
        assert_eq!(
            serde_json::to_string(&AgentSubsystem::RestoreEngine).unwrap(),
            "\"restore-engine\""
        );
    }
}
