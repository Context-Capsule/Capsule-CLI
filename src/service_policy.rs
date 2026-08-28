use serde::{Deserialize, Serialize};

pub const SERVICE_DECISIONS_ENV: &str = "CONTEXT_CAPSULE_SERVICE_DECISIONS_PATH";
pub const CALLER_PID_ENV: &str = "CONTEXT_CAPSULE_CALLER_PID";
pub const MAX_RESTART_COMMAND_CHARS: usize = 8192;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ServiceSource {
    ExternalTerminal,
    VisualStudioCode,
}

impl ServiceSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ExternalTerminal => "external-terminal",
            Self::VisualStudioCode => "visual-studio-code",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RestartPolicy {
    Ask,
    Always,
}

impl RestartPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::Always => "always",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedService {
    pub service_index: u32,
    pub source: ServiceSource,
    pub host: String,
    pub shell: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub captured_terminal_pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vscode_terminal_index: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub working_directory: Option<String>,
    /// User-facing command spelling when it can be recovered safely.
    pub command: String,
    /// Resolved process command proven to have been running at save time. This
    /// is used for reliable execution while `command` remains what is shown to
    /// the user. Older capsules omit it and automatically fall back to command.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution_command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pre_start_command: Option<String>,
    pub restart_policy: RestartPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServicePlan {
    pub capsule_name: String,
    pub revision: u32,
    pub services: Vec<SavedService>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RestoreDecisionKind {
    StartOnce,
    Always,
    Skip,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreDecision {
    pub service_index: u32,
    pub decision: RestoreDecisionKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreDecisionFile {
    pub capsule_name: String,
    pub revision: u32,
    pub decisions: Vec<RestoreDecision>,
}

pub fn validate_restart_command(command: &str) -> Result<String, String> {
    let command = command.trim();
    if command.is_empty() {
        return Err("restart command cannot be empty".to_owned());
    }
    if command.chars().count() > MAX_RESTART_COMMAND_CHARS {
        return Err(format!(
            "restart command cannot exceed {MAX_RESTART_COMMAND_CHARS} characters"
        ));
    }
    if command.chars().any(char::is_control) {
        return Err("restart command cannot contain terminal control characters".to_owned());
    }

    let lower = command.to_ascii_lowercase();
    let sensitive_markers = [
        "--password",
        "--passwd",
        "--token",
        "--secret",
        "--api-key",
        "--apikey",
        "authorization:",
        "bearer ",
        "access_token",
        "refresh_token",
        "client_secret",
        "private_key",
        "secret_key",
    ];
    if sensitive_markers.iter().any(|marker| lower.contains(marker))
        || contains_credential_url(&lower)
    {
        return Err(
            "restart command looks secret-bearing; Context Capsule will not persist or replay it"
                .to_owned(),
        );
    }

    Ok(command.to_owned())
}

pub fn combined_command(service: &SavedService) -> Result<String, String> {
    let service_command = validate_restart_command(
        service
            .execution_command
            .as_deref()
            .unwrap_or(service.command.as_str()),
    )?;
    let Some(pre_start) = service.pre_start_command.as_deref() else {
        return Ok(service_command);
    };
    let pre_start = validate_restart_command(pre_start)?;
    let shell = service.shell.to_ascii_lowercase();
    if shell.contains("powershell") {
        Ok(format!("{pre_start}; if ($?) {{ {service_command} }}"))
    } else {
        Ok(format!("{pre_start} && {service_command}"))
    }
}

fn contains_credential_url(commandline: &str) -> bool {
    let Some(scheme_index) = commandline.find("://") else {
        return false;
    };
    let authority = &commandline[scheme_index + 3..];
    let authority = authority
        .split(['/', ' ', '\t'])
        .next()
        .unwrap_or_default();
    let Some(at_index) = authority.find('@') else {
        return false;
    };
    authority[..at_index].contains(':')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service(shell: &str, pre_start: Option<&str>) -> SavedService {
        SavedService {
            service_index: 1,
            source: ServiceSource::ExternalTerminal,
            host: "windows-terminal".to_owned(),
            shell: shell.to_owned(),
            captured_terminal_pid: Some(42),
            vscode_terminal_index: None,
            terminal_name: None,
            profile: Some("PowerShell".to_owned()),
            working_directory: Some("C:/work".to_owned()),
            command: "npm run start:dev".to_owned(),
            execution_command: None,
            pre_start_command: pre_start.map(str::to_owned),
            restart_policy: RestartPolicy::Ask,
        }
    }

    #[test]
    fn safe_service_commands_are_accepted() {
        assert_eq!(
            validate_restart_command("npm run start:dev").unwrap(),
            "npm run start:dev"
        );
        assert!(validate_restart_command("source .venv/bin/activate").is_ok());
    }

    #[test]
    fn secret_bearing_commands_are_rejected() {
        assert!(validate_restart_command("tool --token abc").is_err());
        assert!(validate_restart_command("curl https://user:password@example.com").is_err());
        assert!(validate_restart_command("npm start\u{1b}[31m").is_err());
    }

    #[test]
    fn resolved_execution_command_does_not_replace_user_facing_command() {
        let mut saved = service("Windows PowerShell", None);
        saved.command = "python -m app".to_owned();
        saved.execution_command = Some(
            r#""C:\work\venv\Scripts\python.exe" -m app"#.to_owned(),
        );
        assert_eq!(saved.command, "python -m app");
        assert_eq!(
            combined_command(&saved).unwrap(),
            r#""C:\work\venv\Scripts\python.exe" -m app"#
        );
    }

    #[test]
    fn old_capsules_without_execution_command_still_execute_command() {
        let saved = service("Windows PowerShell", None);
        assert_eq!(combined_command(&saved).unwrap(), "npm run start:dev");
    }

    #[test]
    fn pre_start_stays_in_the_same_shell_before_service() {
        assert_eq!(
            combined_command(&service("Windows PowerShell", Some(". .\\env.ps1"))).unwrap(),
            ". .\\env.ps1; if ($?) { npm run start:dev }"
        );
        assert_eq!(
            combined_command(&service("Bash", Some("source .venv/bin/activate"))).unwrap(),
            "source .venv/bin/activate && npm run start:dev"
        );
    }

    #[test]
    fn decision_file_round_trip_is_stable() {
        let file = RestoreDecisionFile {
            capsule_name: "work".to_owned(),
            revision: 3,
            decisions: vec![RestoreDecision {
                service_index: 1,
                decision: RestoreDecisionKind::Always,
            }],
        };
        let encoded = serde_json::to_string(&file).unwrap();
        assert_eq!(serde_json::from_str::<RestoreDecisionFile>(&encoded).unwrap(), file);
    }
}
