#[path = "terminal_context_legacy.rs"]
mod legacy;
#[path = "powershell_runspace.rs"]
mod powershell_runspace;
#[path = "powershell_ui_probe_v2.rs"]
mod powershell_ui_probe;

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

    // Prefer the non-invasive PowerShell management/runspace API first. Some
    // Windows PowerShell hosts do not expose their interactive runspace through
    // that connection, so unresolved Windows Terminal sessions then fall back to
    // a guarded UI probe that asks the real terminal pane for $PWD.
    enrich_exact_powershell_locations(&mut prepared, "capture");

    // The UI probe gets the raw inventory for safety checks. prepare_for_capture
    // intentionally removes the shell that is hosting the current capsule
    // command, but that shell must remain visible to the UI safety gate so we
    // never type into a busy command-host tab merely because it was filtered out
    // of the capsule payload.
    enrich_exact_powershell_locations_from_ui(&mut prepared, snapshot, "capture");
    prepared
}

pub(crate) fn enrich_for_matching(snapshot: &TerminalSnapshot) -> TerminalSnapshot {
    let mut prepared = legacy::enrich_for_matching(snapshot);

    // Restore matching needs the same exact logical PowerShell CWD used during
    // capture. Otherwise multiple identical PowerShell tabs are indistinguishable
    // and a surviving tab can be mistaken for a missing one.
    enrich_exact_powershell_locations(&mut prepared, "restore-match");
    enrich_exact_powershell_locations_from_ui(&mut prepared, snapshot, "restore-match");
    prepared
}

fn is_windows_terminal_powershell(session: &TerminalSession) -> bool {
    session.host == TerminalHost::WindowsTerminal
        && matches!(session.environment, TerminalEnvironment::Windows)
        && matches!(session.shell, ShellKind::PowerShell | ShellKind::WindowsPowerShell)
}

fn has_trusted_exact_directory(session: &TerminalSession) -> bool {
    is_windows_terminal_powershell(session)
        && session.working_directory.is_some()
        && matches!(
            session.working_directory_source,
            WorkingDirectorySource::WindowsTerminalState
        )
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
        if !is_windows_terminal_powershell(session) || has_trusted_exact_directory(session) {
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

        apply_exact_directory(
            session,
            directory,
            stage,
            "PowerShellRunspace",
            previous_directory,
            previous_source,
        );
    }
}

fn ui_probe_is_safe(safety_snapshot: &TerminalSnapshot, stage: &str) -> bool {
    ui_probe_is_safe_with(safety_snapshot, stage, powershell_runspace::is_idle)
}

fn ui_probe_is_safe_with<F>(
    safety_snapshot: &TerminalSnapshot,
    stage: &str,
    idle_probe: F,
) -> bool
where
    F: Fn(&TerminalSession) -> Option<bool>,
{
    for session in safety_snapshot
        .sessions
        .iter()
        .filter(|session| is_windows_terminal_powershell(session))
    {
        if session.foreground_command.is_some() {
            logging::info(
                TERMINAL_LOG_COMPONENT,
                format!(
                    "{stage}: PowerShell UI CWD fallback skipped because pid={:?} has foreground command {:?}",
                    session.pid, session.foreground_command,
                ),
            );
            return false;
        }

        match idle_probe(session) {
            Some(true) => {
                logging::info(
                    TERMINAL_LOG_COMPONENT,
                    format!(
                        "{stage}: PowerShell UI CWD safety gate pid={:?}: runspace explicitly Available",
                        session.pid,
                    ),
                );
            }
            Some(false) => {
                logging::info(
                    TERMINAL_LOG_COMPONENT,
                    format!(
                        "{stage}: PowerShell UI CWD fallback skipped because pid={:?} runspace is explicitly Busy",
                        session.pid,
                    ),
                );
                return false;
            }
            None => {
                // The process-attach API is known to be incomplete on some
                // Windows PowerShell 5.1 hosts (the same hosts that cannot expose
                // SessionStateProxy/$PWD). Treating that API failure as a veto
                // made the UI fallback unreachable. We still refuse any session
                // with a discovered foreground child command, while the UI probe
                // itself verifies foreground ownership before every SendInput.
                logging::info(
                    TERMINAL_LOG_COMPONENT,
                    format!(
                        "{stage}: PowerShell UI CWD safety gate pid={:?}: idle API unavailable; continuing because process inventory shows no foreground child command",
                        session.pid,
                    ),
                );
            }
        }
    }
    true
}

fn enrich_exact_powershell_locations_from_ui(
    target: &mut TerminalSnapshot,
    safety_snapshot: &TerminalSnapshot,
    stage: &str,
) {
    if !target
        .sessions
        .iter()
        .any(|session| is_windows_terminal_powershell(session) && !has_trusted_exact_directory(session))
    {
        return;
    }

    if !ui_probe_is_safe(safety_snapshot, stage) {
        return;
    }

    logging::info(
        TERMINAL_LOG_COMPONENT,
        format!("{stage}: entering PowerShell UI CWD fallback"),
    );
    let exact_by_pid = powershell_ui_probe::working_directories(safety_snapshot);
    if exact_by_pid.is_empty() {
        logging::info(
            TERMINAL_LOG_COMPONENT,
            format!("{stage}: PowerShell UI CWD fallback produced no exact directory results"),
        );
        return;
    }

    for session in &mut target.sessions {
        if !is_windows_terminal_powershell(session) || has_trusted_exact_directory(session) {
            continue;
        }
        let Some(pid) = session.pid else {
            continue;
        };
        let Some(directory) = exact_by_pid.get(&pid).cloned() else {
            continue;
        };

        let previous_directory = session.working_directory.clone();
        let previous_source = session.working_directory_source.clone();
        apply_exact_directory(
            session,
            directory,
            stage,
            "PowerShellUiProbeV2",
            previous_directory,
            previous_source,
        );
    }
}

fn apply_exact_directory(
    session: &mut TerminalSession,
    directory: String,
    stage: &str,
    provenance: &str,
    previous_directory: Option<String>,
    previous_source: WorkingDirectorySource,
) {
    // WorkingDirectorySource predates exact PowerShell probing. The existing
    // WindowsTerminalState variant is the serialized trust channel understood by
    // restore matching. terminal.log records the true provenance explicitly so
    // this does not masquerade as state.json discovery during diagnostics.
    session.working_directory = Some(directory.clone());
    session.working_directory_source = WorkingDirectorySource::WindowsTerminalState;

    logging::info(
        TERMINAL_LOG_COMPONENT,
        format!(
            "{stage}: exact PowerShell CWD selected pid={:?} exact_cwd={:?} replaced_cwd={:?} replaced_source={:?} serialized_trust_source=WindowsTerminalState provenance={provenance}",
            session.pid, directory, previous_directory, previous_source,
        ),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::terminal::{
        RestartPlan, TerminalHistoryPolicy, TerminalSource, TerminalStatus,
    };
    use std::collections::HashMap;

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

    fn apply_ui_results_for_test(
        snapshot: &mut TerminalSnapshot,
        exact_by_pid: &HashMap<u32, String>,
    ) {
        for session in &mut snapshot.sessions {
            if !is_windows_terminal_powershell(session) || has_trusted_exact_directory(session) {
                continue;
            }
            let Some(pid) = session.pid else {
                continue;
            };
            let Some(directory) = exact_by_pid.get(&pid).cloned() else {
                continue;
            };
            let previous_directory = session.working_directory.clone();
            let previous_source = session.working_directory_source.clone();
            apply_exact_directory(
                session,
                directory,
                "test",
                "PowerShellUiProbeV2",
                previous_directory,
                previous_source,
            );
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
    fn ui_cwd_overrides_same_untrusted_fallback_when_api_cannot_answer() {
        let mut snapshot = snapshot_with(fake_windows_terminal_powershell());
        let exact = HashMap::from([(42_u32, r"D:\actual-project".to_owned())]);
        apply_ui_results_for_test(&mut snapshot, &exact);

        let session = &snapshot.sessions[0];
        assert_eq!(session.working_directory.as_deref(), Some(r"D:\actual-project"));
        assert_eq!(
            session.working_directory_source,
            WorkingDirectorySource::WindowsTerminalState
        );
    }

    #[test]
    fn unavailable_idle_api_no_longer_suppresses_ui_fallback() {
        let snapshot = snapshot_with(fake_windows_terminal_powershell());
        assert!(ui_probe_is_safe_with(&snapshot, "test", |_| None));
    }

    #[test]
    fn explicitly_busy_runspace_still_blocks_ui_fallback() {
        let snapshot = snapshot_with(fake_windows_terminal_powershell());
        assert!(!ui_probe_is_safe_with(&snapshot, "test", |_| Some(false)));
    }

    #[test]
    fn foreground_child_command_still_blocks_ui_fallback() {
        let mut session = fake_windows_terminal_powershell();
        session.foreground_command = Some("node server.js".to_owned());
        let snapshot = snapshot_with(session);
        assert!(!ui_probe_is_safe_with(&snapshot, "test", |_| None));
    }

    #[test]
    fn failed_runspace_probe_preserves_existing_fallback() {
        let original = fake_windows_terminal_powershell();
        let mut snapshot = snapshot_with(original.clone());
        enrich_exact_powershell_locations_with(&mut snapshot, "test", |_| None);
        assert_eq!(snapshot.sessions[0], original);
    }

    #[test]
    fn trusted_terminal_directory_is_not_reprobed_or_replaced() {
        let mut session = fake_windows_terminal_powershell();
        session.working_directory = Some(r"D:\trusted".to_owned());
        session.working_directory_source = WorkingDirectorySource::WindowsTerminalState;
        let mut snapshot = snapshot_with(session);

        enrich_exact_powershell_locations_with(&mut snapshot, "test", |_| {
            panic!("trusted exact directory must not be probed again")
        });
        assert_eq!(snapshot.sessions[0].working_directory.as_deref(), Some(r"D:\trusted"));
    }
}
