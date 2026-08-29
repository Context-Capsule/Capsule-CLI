#[path = "terminal_context_legacy.rs"]
mod legacy;
#[path = "powershell_runspace.rs"]
mod powershell_runspace;
#[path = "powershell_ui_probe_v4.rs"]
mod powershell_ui_probe;

use crate::{
    adapters::terminal::{
        ShellKind, TerminalEnvironment, TerminalHost, TerminalSession, TerminalSnapshot,
        WorkingDirectorySource,
    },
    logging,
};
use std::collections::HashMap;

const TERMINAL_LOG_COMPONENT: &str = "terminal";

pub fn prepare_for_capture(
    snapshot: &TerminalSnapshot,
    vscode_semantic_available: bool,
) -> TerminalSnapshot {
    let mut prepared = legacy::prepare_for_capture(snapshot, vscode_semantic_available);
    enrich_exact_powershell_locations(&mut prepared, "capture");
    enrich_exact_powershell_locations_from_ui(&mut prepared, snapshot, "capture");
    prepared
}

pub(crate) fn enrich_for_matching(snapshot: &TerminalSnapshot) -> TerminalSnapshot {
    let mut prepared = legacy::enrich_for_matching(snapshot);
    enrich_exact_powershell_locations(&mut prepared, "restore-match");
    enrich_exact_powershell_locations_from_ui(&mut prepared, snapshot, "restore-match");
    prepared
}

fn is_windows_powershell(session: &TerminalSession) -> bool {
    matches!(session.environment, TerminalEnvironment::Windows)
        && matches!(session.shell, ShellKind::PowerShell | ShellKind::WindowsPowerShell)
}

fn is_windows_terminal_powershell(session: &TerminalSession) -> bool {
    session.host == TerminalHost::WindowsTerminal && is_windows_powershell(session)
}

/// VS Code and Cursor terminals are not restored through this generic terminal
/// path. VS Code is owned by its semantic adapter when that identity exists and
/// orphaned legacy VS Code terminal records are deliberately suppressed; Cursor
/// terminals are preserved but not restored until a Cursor adapter exists.
/// Probing those processes through PowerShell process remoting is therefore
/// pure latency and can cost several seconds per terminal.
fn should_probe_exact_powershell_location(session: &TerminalSession) -> bool {
    is_windows_powershell(session)
        && !matches!(
            session.host,
            TerminalHost::VisualStudioCode | TerminalHost::Cursor
        )
}

fn has_trusted_exact_directory(session: &TerminalSession) -> bool {
    is_windows_powershell(session)
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
    // Terminal discovery can contain multiple reconciled records for the same
    // live shell PID (for example persisted Windows Terminal metadata plus a
    // process-backed record). A PowerShell management probe is process-scoped,
    // so probing the same PID twice can only repeat the same expensive work.
    // Cache both successes and failures and apply the one result to every record.
    let mut probe_cache = HashMap::<u32, Option<String>>::new();

    for session in &mut snapshot.sessions {
        if !should_probe_exact_powershell_location(session) || has_trusted_exact_directory(session) {
            continue;
        }

        let Some(pid) = session.pid else {
            logging::info(
                TERMINAL_LOG_COMPONENT,
                format!(
                    "{stage}: exact PowerShell runspace CWD skipped for session without a PID"
                ),
            );
            continue;
        };

        let previous_directory = session.working_directory.clone();
        let previous_source = session.working_directory_source.clone();
        let directory = if let Some(cached) = probe_cache.get(&pid) {
            cached.clone()
        } else {
            let result = probe(session);
            probe_cache.insert(pid, result.clone());
            result
        };
        let Some(directory) = directory else {
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
    // RunspaceAvailability used to be queried here as an additional diagnostic,
    // but Available/Busy/unavailable were all explicitly advisory and all three
    // states continued to the same UI fallback. Each query can take seconds on
    // Windows PowerShell, so paying that cost cannot improve safety. The actual
    // safety invariant is the process inventory below: never inject a UI probe
    // while any Windows Terminal PowerShell pane has a foreground child command.
    ui_probe_is_safe_with(safety_snapshot, stage, |_| None)
}

fn ui_probe_is_safe_with<F>(
    safety_snapshot: &TerminalSnapshot,
    stage: &str,
    idle_probe: F,
) -> bool
where
    F: Fn(&TerminalSession) -> Option<bool>,
{
    let mut idle_cache = HashMap::<u32, Option<bool>>::new();

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

        let state = match session.pid {
            Some(pid) => {
                if let Some(cached) = idle_cache.get(&pid) {
                    *cached
                } else {
                    let result = idle_probe(session);
                    idle_cache.insert(pid, result);
                    result
                }
            }
            None => None,
        };

        match state {
            Some(true) => {
                logging::info(
                    TERMINAL_LOG_COMPONENT,
                    format!(
                        "{stage}: PowerShell UI CWD safety gate pid={:?}: runspace reports Available; process inventory has no foreground child command",
                        session.pid,
                    ),
                );
            }
            Some(false) => {
                logging::info(
                    TERMINAL_LOG_COMPONENT,
                    format!(
                        "{stage}: PowerShell UI CWD safety gate pid={:?}: runspace reports Busy, but that remoting signal is advisory; continuing because process inventory has no foreground child command",
                        session.pid,
                    ),
                );
            }
            None => {
                logging::info(
                    TERMINAL_LOG_COMPONENT,
                    format!(
                        "{stage}: PowerShell UI CWD safety gate pid={:?}: runspace idle advisory skipped/unavailable; process inventory has no foreground child command",
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
        format!("{stage}: entering PowerShell UI CWD fallback v4"),
    );
    let probe = powershell_ui_probe::probe(safety_snapshot);
    if probe.directories.is_empty() && probe.ordered_pids.is_empty() {
        logging::info(
            TERMINAL_LOG_COMPONENT,
            format!("{stage}: PowerShell UI CWD fallback v4 produced no results"),
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
        let Some(directory) = probe.directories.get(&pid).cloned() else {
            continue;
        };

        let previous_directory = session.working_directory.clone();
        let previous_source = session.working_directory_source.clone();
        apply_exact_directory(
            session,
            directory,
            stage,
            "PowerShellUiProbeV4",
            previous_directory,
            previous_source,
        );
    }

    reorder_windows_terminal_powershell_sessions(target, &probe.ordered_pids, stage);
}

fn reorder_windows_terminal_powershell_sessions(
    snapshot: &mut TerminalSnapshot,
    ordered_pids: &[u32],
    stage: &str,
) {
    if ordered_pids.is_empty() {
        return;
    }

    let rank = ordered_pids
        .iter()
        .copied()
        .enumerate()
        .map(|(index, pid)| (pid, index))
        .collect::<HashMap<_, _>>();

    let slots = snapshot
        .sessions
        .iter()
        .enumerate()
        .filter(|(_, session)| is_windows_terminal_powershell(session))
        .map(|(index, _)| index)
        .collect::<Vec<_>>();

    if slots.len() < 2 {
        return;
    }

    let mut terminal_sessions = slots
        .iter()
        .map(|index| snapshot.sessions[*index].clone())
        .collect::<Vec<_>>();

    terminal_sessions.sort_by_key(|session| {
        session
            .pid
            .and_then(|pid| rank.get(&pid).copied())
            .unwrap_or(usize::MAX)
    });

    for (slot, session) in slots.into_iter().zip(terminal_sessions) {
        snapshot.sessions[slot] = session;
    }

    let saved_order = snapshot
        .sessions
        .iter()
        .filter(|session| is_windows_terminal_powershell(session))
        .filter_map(|session| session.pid)
        .collect::<Vec<_>>();
    logging::info(
        TERMINAL_LOG_COMPONENT,
        format!(
            "{stage}: Windows Terminal PowerShell session order aligned to observed tab order observed={ordered_pids:?} saved={saved_order:?}"
        ),
    );
}

fn direct_powershell_executable(executable: &str) -> bool {
    let name = executable
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or(executable)
        .to_ascii_lowercase();
    matches!(
        name.as_str(),
        "pwsh" | "pwsh.exe" | "powershell" | "powershell.exe"
    )
}

fn apply_exact_directory(
    session: &mut TerminalSession,
    directory: String,
    stage: &str,
    provenance: &str,
    previous_directory: Option<String>,
    previous_source: WorkingDirectorySource,
) {
    session.working_directory = Some(directory.clone());
    session.working_directory_source = WorkingDirectorySource::WindowsTerminalState;

    if session.host != TerminalHost::WindowsTerminal {
        if let Some(restart) = session.restart.as_mut() {
            if direct_powershell_executable(&restart.executable) {
                restart.working_directory = Some(directory.clone());
                restart.note = Some(
                    "Starts the captured interactive PowerShell in its exact captured $PWD without replaying shell history or foreground commands."
                        .to_owned(),
                );
            }
        }
    }

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
    use std::cell::Cell;

    fn snapshot_with(session: TerminalSession) -> TerminalSnapshot {
        snapshot_with_sessions(vec![session])
    }

    fn snapshot_with_sessions(sessions: Vec<TerminalSession>) -> TerminalSnapshot {
        TerminalSnapshot {
            status: TerminalStatus::Available,
            message: None,
            windows_terminal_layouts: Vec::new(),
            sessions,
            warnings: Vec::new(),
            history: TerminalHistoryPolicy {
                captured: false,
                reason: "test".to_owned(),
            },
        }
    }

    fn fake_windows_terminal_powershell() -> TerminalSession {
        fake_windows_terminal_powershell_with_pid(42)
    }

    fn fake_windows_terminal_powershell_with_pid(pid: u32) -> TerminalSession {
        TerminalSession {
            sources: vec![TerminalSource::WindowsProcess],
            host: TerminalHost::WindowsTerminal,
            shell: ShellKind::WindowsPowerShell,
            shell_executable: Some("powershell.exe".to_owned()),
            environment: TerminalEnvironment::Windows,
            pid: Some(pid),
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

    fn fake_standalone_powershell(host: TerminalHost) -> TerminalSession {
        let mut session = fake_windows_terminal_powershell();
        session.host = host;
        session.profile = None;
        session
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
                "PowerShellUiProbeV4",
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
    fn duplicate_records_for_one_pid_are_probed_once() {
        let first = fake_windows_terminal_powershell_with_pid(42);
        let second = first.clone();
        let mut snapshot = snapshot_with_sessions(vec![first, second]);
        let calls = Cell::new(0usize);

        enrich_exact_powershell_locations_with(&mut snapshot, "test", |_| {
            calls.set(calls.get() + 1);
            Some(r"D:\actual-project".to_owned())
        });

        assert_eq!(calls.get(), 1);
        assert!(snapshot
            .sessions
            .iter()
            .all(|session| session.working_directory.as_deref() == Some(r"D:\actual-project")));
    }

    #[test]
    fn semantic_editor_terminals_are_not_runspace_probed() {
        for host in [TerminalHost::VisualStudioCode, TerminalHost::Cursor] {
            let mut session = fake_windows_terminal_powershell();
            session.host = host;
            let original = session.clone();
            let mut snapshot = snapshot_with(session);

            enrich_exact_powershell_locations_with(&mut snapshot, "test", |_| {
                panic!("semantic editor terminals must not use generic PowerShell CWD probing")
            });

            assert_eq!(snapshot.sessions[0], original);
        }
    }

    #[test]
    fn standalone_powershell_exact_cwd_replaces_process_fallback_and_restart_directory() {
        for host in [TerminalHost::Unknown, TerminalHost::ConsoleHost] {
            let mut snapshot = snapshot_with(fake_standalone_powershell(host));
            enrich_exact_powershell_locations_with(&mut snapshot, "test", |session| {
                (session.pid == Some(42)).then(|| r"D:\actual-project".to_owned())
            });

            let session = &snapshot.sessions[0];
            assert_eq!(session.working_directory.as_deref(), Some(r"D:\actual-project"));
            assert_eq!(
                session
                    .restart
                    .as_ref()
                    .and_then(|plan| plan.working_directory.as_deref()),
                Some(r"D:\actual-project")
            );
            assert_eq!(
                session.working_directory_source,
                WorkingDirectorySource::WindowsTerminalState
            );
        }
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
    fn observed_tab_order_reorders_saved_windows_terminal_sessions() {
        let mut first = fake_windows_terminal_powershell_with_pid(200);
        first.working_directory = Some(r"D:\second".to_owned());
        let mut second = fake_windows_terminal_powershell_with_pid(100);
        second.working_directory = Some(r"D:\first".to_owned());
        let mut unrelated = fake_windows_terminal_powershell_with_pid(300);
        unrelated.host = TerminalHost::VisualStudioCode;

        let mut snapshot = snapshot_with_sessions(vec![first, unrelated.clone(), second]);
        reorder_windows_terminal_powershell_sessions(&mut snapshot, &[100, 200], "test");

        assert_eq!(snapshot.sessions[0].pid, Some(100));
        assert_eq!(snapshot.sessions[1], unrelated);
        assert_eq!(snapshot.sessions[2].pid, Some(200));
    }

    #[test]
    fn unavailable_idle_api_does_not_suppress_ui_fallback() {
        let snapshot = snapshot_with(fake_windows_terminal_powershell());
        assert!(ui_probe_is_safe_with(&snapshot, "test", |_| None));
    }

    #[test]
    fn advisory_busy_runspace_does_not_suppress_ui_fallback_without_process_activity() {
        let snapshot = snapshot_with(fake_windows_terminal_powershell());
        assert!(ui_probe_is_safe_with(&snapshot, "test", |_| Some(false)));
    }

    #[test]
    fn duplicate_ui_safety_records_probe_idle_state_once_per_pid() {
        let first = fake_windows_terminal_powershell_with_pid(42);
        let second = first.clone();
        let snapshot = snapshot_with_sessions(vec![first, second]);
        let calls = Cell::new(0usize);

        assert!(ui_probe_is_safe_with(&snapshot, "test", |_| {
            calls.set(calls.get() + 1);
            Some(true)
        }));
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn foreground_child_command_still_blocks_ui_fallback_even_if_runspace_looks_available() {
        let mut session = fake_windows_terminal_powershell();
        session.foreground_command = Some("node server.js".to_owned());
        let snapshot = snapshot_with(session);
        assert!(!ui_probe_is_safe_with(&snapshot, "test", |_| Some(true)));
    }

    #[test]
    fn failed_runspace_probe_preserves_existing_fallback() {
        let original = fake_windows_terminal_powershell();
        let mut snapshot = snapshot_with(original.clone());
        enrich_exact_powershell_locations_with(&mut snapshot, "test", |_| None);
        assert_eq!(snapshot.sessions[0], original);
    }

    #[test]
    fn failed_standalone_runspace_probe_preserves_existing_restart_fallback() {
        let original = fake_standalone_powershell(TerminalHost::ConsoleHost);
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
