#[path = "terminal_context_legacy.rs"]
mod legacy;
#[path = "powershell_runspace.rs"]
mod powershell_runspace;

use crate::{
    adapters::terminal::{
        ShellKind, TerminalEnvironment, TerminalHost, TerminalSnapshot, WorkingDirectorySource,
    },
    logging,
};

const TERMINAL_LOG_COMPONENT: &str = "terminal";

pub fn prepare_for_capture(
    snapshot: &TerminalSnapshot,
    vscode_semantic_available: bool,
) -> TerminalSnapshot {
    let mut prepared = legacy::prepare_for_capture(snapshot, vscode_semantic_available);
    enrich_exact_powershell_locations(&mut prepared, "capture");
    prepared
}

pub(crate) fn enrich_for_matching(snapshot: &TerminalSnapshot) -> TerminalSnapshot {
    let mut prepared = legacy::enrich_for_matching(snapshot);
    enrich_exact_powershell_locations(&mut prepared, "restore-match");
    prepared
}

fn enrich_exact_powershell_locations(snapshot: &mut TerminalSnapshot, stage: &str) {
    for session in &mut snapshot.sessions {
        if session.host != TerminalHost::WindowsTerminal
            || !matches!(session.environment, TerminalEnvironment::Windows)
            || !matches!(session.shell, ShellKind::PowerShell | ShellKind::WindowsPowerShell)
        {
            continue;
        }

        let previous_directory = session.working_directory.clone();
        let previous_source = session.working_directory_source.clone();
        let Some(directory) = powershell_runspace::working_directory(session) else {
            logging::info(
                TERMINAL_LOG_COMPONENT,
                format!(
                    "{stage}: exact PowerShell runspace CWD unavailable for pid={:?}; retaining cwd={:?} source={:?}",
                    session.pid, previous_directory, previous_source,
                ),
            );
            continue;
        };

        session.working_directory = Some(directory.clone());
        session.working_directory_source = WorkingDirectorySource::WindowsTerminalState;

        logging::info(
            TERMINAL_LOG_COMPONENT,
            format!(
                "{stage}: exact PowerShell runspace CWD selected pid={:?} exact_cwd={:?} replaced_cwd={:?} replaced_source={:?} serialized_trust_source=WindowsTerminalState provenance=PowerShellRunspace",
                session.pid, directory, previous_directory, previous_source,
            ),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::terminal::{
        RestartPlan, TerminalHistoryPolicy, TerminalSession, TerminalSource, TerminalStatus,
    };

    fn snapshot_with(session: TerminalSession) -> TerminalSnapshot {
        TerminalSnapshot {
            status: TerminalStatus::Available,
            message: None,
            windows_terminal_layouts: Vec::new(),
            sessions: vec![session],
            warnings: Vec::new(),
            history: TerminalHistoryPolicy {
                captured: false,
                reason: "test".to_owned(),
            },
        }
    }

    fn fake_windows_terminal_powershell() -> TerminalSession {
        TerminalSession {
            sources: vec![TerminalSource::WindowsProcess],
            host: TerminalHost::WindowsTerminal,
            shell: ShellKind::WindowsPowerShell,
            shell_executable: Some("powershell.exe".to_owned()),
            environment: TerminalEnvironment::Windows,
            pid: Some(u32::MAX),
            parent_pid: None,
            tty: None,
            profile: Some("Windows PowerShell".to_owned()),
            title: None,
            working_directory: Some(r"C:\Users\fallback".to_owned()),
            working_directory_source: WorkingDirectorySource::Unknown,
            startup_command: None,
            foreground_command: None,
            restart: Some(RestartPlan {
                executable: "powershell.exe".to_owned(),
                args: Vec::new(),
                working_directory: Some(r"C:\Users\fallback".to_owned()),
                note: None,
            }),
        }
    }

    #[cfg(not(windows))]
    #[test]
    fn failed_direct_probe_preserves_existing_metadata() {
        let original = fake_windows_terminal_powershell();
        let mut snapshot = snapshot_with(original.clone());
        enrich_exact_powershell_locations(&mut snapshot, "test");
        assert_eq!(snapshot.sessions[0], original);
    }
}
