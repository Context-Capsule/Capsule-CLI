#[path = "terminal_context_legacy.rs"]
mod legacy;
#[path = "powershell_runspace.rs"]
mod powershell_runspace;

use crate::{
    adapters::terminal::{
        ShellKind, TerminalEnvironment, TerminalHost, TerminalSession, TerminalSnapshot,
        WorkingDirectorySource,
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
    enrich_exact_powershell_locations_with(snapshot, stage, powershell_runspace::working_directory);
}

fn enrich_exact_powershell_locations_with<F>(
    snapshot: &mut TerminalSnapshot,
    stage: &str,
    probe: F,
) where
    F: Fn(&TerminalSession) -> Option<String>,
{
    for session in &mut snapshot.sessions {
        if session.host != TerminalHost::WindowsTerminal
            || !matches!(session.environment, TerminalEnvironment::Windows)
            || !matches!(session.shell, ShellKind::PowerShell | ShellKind::WindowsPowerShell)
        {
            continue;
        }

        let previous_directory = session.working_directory.clone();
        let previous_source = session.working_directory_source.clone();
        let Some(directory) = probe(session) else {
            logging::info(
                TERMINAL_LOG_COMPONENT,
                format!(
                    "{stage}: exact PowerShell runspace CWD unavailable for pid={:?}; retaining cwd={:?} source={:?}",
                    session.pid, previous_directory, previous_source,
                ),
            );
            continue;
        };

        // WorkingDirectorySource predates direct runspace probing. The existing
        // WindowsTerminalState value is the serialized trust channel understood
        // by restore matching; terminal.log records the true PowerShellRunspace
        // provenance explicitly until the snapshot schema grows a dedicated
        // source variant.
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
        RestartPlan, TerminalHistoryPolicy, TerminalSource, TerminalStatus,
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
            pid: Some(42),
            parent_pid: None,
            tty: None,
            profile: Some("Windows PowerShell".to_owned()),
            title: None,
            working_directory: Some(r"C:\Users\monji".to_owned()),
            working_directory_source: WorkingDirectorySource::Unknown,
            startup_command: None,
            foreground_command: None,
            restart: Some(RestartPlan {
                executable: "powershell.exe".to_owned(),
                args: Vec::new(),
                working_directory: Some(r"C:\Users\monji".to_owned()),
                note: None,
            }),
        }
    }

    #[test]
    fn exact_runspace_cwd_overrides_untrusted_process_fallback() {
        let mut snapshot = snapshot_with(fake_windows_terminal_powershell());
        enrich_exact_powershell_locations_with(&mut snapshot, "test", |session| {
            (session.pid == Some(42)).then(|| r"D:\actual-project".to_owned())
        });

        let session = &snapshot.sessions[0];
        assert_eq!(session.working_directory.as_deref(), Some(r"D:\actual-project"));
        assert_eq!(
            session.working_directory_source,
            WorkingDirectorySource::WindowsTerminalState
        );
    }

    #[test]
    fn failed_runspace_probe_preserves_existing_fallback() {
        let original = fake_windows_terminal_powershell();
        let mut snapshot = snapshot_with(original.clone());
        enrich_exact_powershell_locations_with(&mut snapshot, "test", |_| None);
        assert_eq!(snapshot.sessions[0], original);
    }
}
