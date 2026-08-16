use super::terminal::{
    RestartPlan, ShellKind, TerminalEnvironment, TerminalHost, TerminalLayoutAction, TerminalSnapshot,
    TerminalSource, TerminalStatus, TerminalWindowSize, WindowsTerminalLayout,
};
use std::{
    collections::HashSet,
    path::Path,
    process::{Command, Stdio},
    thread,
    time::Duration,
};

const LAUNCH_GAP: Duration = Duration::from_millis(140);

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TerminalRestoreReport {
    pub sessions_total: usize,
    pub sessions_already_satisfied: usize,
    pub sessions_planned: usize,
    pub sessions_launched: usize,
    pub sessions_delegated: usize,
    pub sessions_unrestorable: usize,
    pub layouts_total: usize,
    pub layouts_already_satisfied: usize,
    pub layouts_planned: usize,
    pub layouts_launched: usize,
    pub warnings: Vec<String>,
    pub failures: Vec<String>,
}

impl TerminalRestoreReport {
    pub fn success(&self) -> bool {
        self.failures.is_empty()
    }
}

pub fn restore(snapshot: &TerminalSnapshot, dry_run: bool) -> TerminalRestoreReport {
    let mut report = TerminalRestoreReport {
        sessions_total: snapshot.sessions.len(),
        layouts_total: snapshot.windows_terminal_layouts.len(),
        ..TerminalRestoreReport::default()
    };

    if !matches!(snapshot.status, TerminalStatus::Available | TerminalStatus::Degraded) {
        report.warnings.push(
            snapshot
                .message
                .clone()
                .unwrap_or_else(|| "terminal state was not restorable in this capsule".to_owned()),
        );
        return report;
    }

    #[cfg(not(windows))]
    {
        let _ = dry_run;
        report
            .warnings
            .push("terminal restore is currently implemented for Windows and WSL only".to_owned());
        return report;
    }

    #[cfg(windows)]
    {
        restore_windows(snapshot, dry_run, &mut report);
        report
    }
}

#[cfg(windows)]
fn restore_windows(
    snapshot: &TerminalSnapshot,
    dry_run: bool,
    report: &mut TerminalRestoreReport,
) {
    let current = super::terminal::discover();
    let mut used_current_sessions = HashSet::new();

    for saved in &snapshot.sessions {
        if is_vscode_integrated(saved.host.clone()) {
            report.sessions_delegated += 1;
            continue;
        }
        if saved.sources.contains(&TerminalSource::WindowsTerminalState)
            && !snapshot.windows_terminal_layouts.is_empty()
        {
            continue;
        }

        if let Some(index) = current.sessions.iter().enumerate().find_map(|(index, candidate)| {
            (!used_current_sessions.contains(&index) && session_matches(saved, candidate))
                .then_some(index)
        }) {
            used_current_sessions.insert(index);
            report.sessions_already_satisfied += 1;
            continue;
        }

        let Some(plan) = saved.restart.as_ref() else {
            report.sessions_unrestorable += 1;
            report.warnings.push(format!(
                "{} session has no safe restart plan",
                describe_session(saved.host.clone(), &saved.shell)
            ));
            continue;
        };

        report.sessions_planned += 1;
        if dry_run {
            continue;
        }
        match launch_restart_plan(plan) {
            Ok(()) => report.sessions_launched += 1,
            Err(error) => report.failures.push(format!(
                "{}: {error}",
                describe_session(saved.host.clone(), &saved.shell)
            )),
        }
        thread::sleep(LAUNCH_GAP);
    }

    let mut used_current_layouts = HashSet::new();
    for saved_layout in &snapshot.windows_terminal_layouts {
        if let Some(index) = current
            .windows_terminal_layouts
            .iter()
            .enumerate()
            .find_map(|(index, candidate)| {
                (!used_current_layouts.contains(&index) && layout_matches(saved_layout, candidate))
                    .then_some(index)
            })
        {
            used_current_layouts.insert(index);
            report.layouts_already_satisfied += 1;
            report.sessions_already_satisfied += layout_session_count(saved_layout);
            continue;
        }

        match windows_terminal_plan(saved_layout) {
            Ok(Some(plan)) => {
                report.layouts_planned += 1;
                report.sessions_planned += layout_session_count(saved_layout);
                if dry_run {
                    continue;
                }
                match launch_restart_plan(&plan) {
                    Ok(()) => {
                        report.layouts_launched += 1;
                        report.sessions_launched += layout_session_count(saved_layout);
                    }
                    Err(error) => report.failures.push(format!(
                        "Windows Terminal window {}: {error}",
                        saved_layout.window_index + 1
                    )),
                }
                thread::sleep(LAUNCH_GAP);
            }
            Ok(None) => {
                let count = layout_session_count(saved_layout);
                report.sessions_unrestorable += count;
                report.warnings.push(format!(
                    "Windows Terminal window {} contains no safe restorable tab/pane actions",
                    saved_layout.window_index + 1
                ));
            }
            Err(error) => report.failures.push(format!(
                "Windows Terminal window {}: {error}",
                saved_layout.window_index + 1
            )),
        }
    }
}

fn is_vscode_integrated(host: TerminalHost) -> bool {
    matches!(host, TerminalHost::VisualStudioCode | TerminalHost::Cursor)
}

fn describe_session(host: TerminalHost, shell: &ShellKind) -> String {
    format!("{:?} {}", host, shell.as_str())
}

fn session_matches(
    saved: &super::terminal::TerminalSession,
    current: &super::terminal::TerminalSession,
) -> bool {
    if saved.host != current.host {
        return false;
    }
    if !shell_compatible(&saved.shell, &current.shell) {
        return false;
    }
    if !environment_matches(&saved.environment, &current.environment) {
        return false;
    }
    if !optional_text_matches(saved.profile.as_deref(), current.profile.as_deref()) {
        return false;
    }
    if !optional_directory_matches(
        saved.working_directory.as_deref(),
        current.working_directory.as_deref(),
    ) {
        return false;
    }
    optional_text_matches(saved.title.as_deref(), current.title.as_deref())
}

fn shell_compatible(saved: &ShellKind, current: &ShellKind) -> bool {
    saved == current || *saved == ShellKind::Unknown || *current == ShellKind::Unknown
}

fn environment_matches(saved: &TerminalEnvironment, current: &TerminalEnvironment) -> bool {
    match (saved, current) {
        (TerminalEnvironment::Windows, TerminalEnvironment::Windows) => true,
        (
            TerminalEnvironment::Wsl { distro: saved },
            TerminalEnvironment::Wsl { distro: current },
        ) => optional_text_matches(saved.as_deref(), current.as_deref()),
        _ => false,
    }
}

fn optional_text_matches(saved: Option<&str>, current: Option<&str>) -> bool {
    match saved.map(str::trim).filter(|value| !value.is_empty()) {
        None => true,
        Some(saved) => current
            .map(str::trim)
            .is_some_and(|current| saved.eq_ignore_ascii_case(current)),
    }
}

fn optional_directory_matches(saved: Option<&str>, current: Option<&str>) -> bool {
    match saved.map(str::trim).filter(|value| !value.is_empty()) {
        None => true,
        Some(saved) => current.is_some_and(|current| normalize_directory(saved) == normalize_directory(current)),
    }
}

fn normalize_directory(value: &str) -> String {
    let mut value = value.trim().replace('/', "\\");
    while value.ends_with('\\') && value.len() > 3 {
        value.pop();
    }
    value.to_ascii_lowercase()
}

fn layout_matches(saved: &WindowsTerminalLayout, current: &WindowsTerminalLayout) -> bool {
    let saved_actions = layout_signature(saved);
    let current_actions = layout_signature(current);
    !saved_actions.is_empty() && saved_actions == current_actions
}

fn layout_signature(layout: &WindowsTerminalLayout) -> Vec<String> {
    layout
        .actions
        .iter()
        .filter(|action| matches!(action.action.as_str(), "newTab" | "splitPane"))
        .map(action_signature)
        .collect()
}

fn action_signature(action: &TerminalLayoutAction) -> String {
    format!(
        "{}|{}|{}|{}|{}|{}",
        action.action.to_ascii_lowercase(),
        action.profile.as_deref().unwrap_or_default().to_ascii_lowercase(),
        action
            .starting_directory
            .as_deref()
            .map(normalize_directory)
            .unwrap_or_default(),
        action.tab_title.as_deref().unwrap_or_default().to_ascii_lowercase(),
        action.split.as_deref().unwrap_or_default().to_ascii_lowercase(),
        action
            .size
            .map(|value| format!("{value:.3}"))
            .unwrap_or_default(),
    )
}

fn layout_session_count(layout: &WindowsTerminalLayout) -> usize {
    layout
        .actions
        .iter()
        .filter(|action| matches!(action.action.as_str(), "newTab" | "splitPane"))
        .count()
}

fn windows_terminal_plan(layout: &WindowsTerminalLayout) -> Result<Option<RestartPlan>, String> {
    let actions = layout
        .actions
        .iter()
        .filter(|action| matches!(action.action.as_str(), "newTab" | "splitPane"))
        .collect::<Vec<_>>();
    if actions.is_empty() {
        return Ok(None);
    }

    let mut args = vec!["-w".to_owned(), "new".to_owned()];
    append_window_options(&mut args, layout);

    for (index, action) in actions.iter().enumerate() {
        if index > 0 {
            args.push(";".to_owned());
        }
        append_terminal_action(&mut args, action)?;
    }

    Ok(Some(RestartPlan {
        executable: "wt.exe".to_owned(),
        args,
        working_directory: None,
        note: Some(
            "Recreates the saved Windows Terminal tabs and panes without replaying shell history or arbitrary foreground commands."
                .to_owned(),
        ),
    }))
}

fn append_window_options(args: &mut Vec<String>, layout: &WindowsTerminalLayout) {
    match layout.launch_mode.as_deref().map(str::to_ascii_lowercase).as_deref() {
        Some("maximized") | Some("maximizedfocus") => args.push("--maximized".to_owned()),
        Some("fullscreen") => args.push("--fullscreen".to_owned()),
        _ => {}
    }
    if let Some(position) = layout.initial_position.as_deref().map(str::trim).filter(|value| !value.is_empty()) {
        args.push("--pos".to_owned());
        args.push(position.to_owned());
    }
    if let Some(TerminalWindowSize { width, height }) = layout.initial_size.as_ref() {
        if width.is_finite() && height.is_finite() && *width > 0.0 && *height > 0.0 {
            args.push("--size".to_owned());
            args.push(format!("{},{}", width.round() as i64, height.round() as i64));
        }
    }
}

fn append_terminal_action(args: &mut Vec<String>, action: &TerminalLayoutAction) -> Result<(), String> {
    match action.action.as_str() {
        "newTab" => args.push("new-tab".to_owned()),
        "splitPane" => {
            args.push("split-pane".to_owned());
            match action.split.as_deref().map(str::to_ascii_lowercase).as_deref() {
                Some("horizontal") | Some("down") | Some("up") => args.push("-H".to_owned()),
                Some("vertical") | Some("left") | Some("right") => args.push("-V".to_owned()),
                Some("auto") | None => {}
                Some(other) => return Err(format!("unsupported saved pane split direction '{other}'")),
            }
            if let Some(size) = action.size.filter(|value| value.is_finite() && *value > 0.0 && *value < 1.0) {
                args.push("--size".to_owned());
                args.push(format!("{size:.3}"));
            }
        }
        other => return Err(format!("unsupported Windows Terminal action '{other}'")),
    }

    if let Some(profile) = action.profile.as_deref().map(str::trim).filter(|value| !value.is_empty()) {
        args.push("--profile".to_owned());
        args.push(profile.to_owned());
    }
    if let Some(directory) = action
        .starting_directory
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        args.push("--startingDirectory".to_owned());
        args.push(directory.to_owned());
    }
    if let Some(title) = action.tab_title.as_deref().map(str::trim).filter(|value| !value.is_empty()) {
        args.push("--title".to_owned());
        args.push(title.to_owned());
    }
    Ok(())
}

fn launch_restart_plan(plan: &RestartPlan) -> Result<(), String> {
    if plan.executable.trim().is_empty() {
        return Err("restart executable is empty".to_owned());
    }
    let mut command = Command::new(&plan.executable);
    command.args(&plan.args);
    if let Some(directory) = plan
        .working_directory
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        if !Path::new(directory).is_dir() {
            return Err(format!("working directory '{directory}' no longer exists"));
        }
        command.current_dir(directory);
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("failed to launch '{}': {error}", plan.executable))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::terminal::{
        TerminalHistoryPolicy, TerminalSession, WorkingDirectorySource,
    };

    fn session(host: TerminalHost, directory: Option<&str>) -> TerminalSession {
        TerminalSession {
            sources: vec![TerminalSource::WindowsProcess],
            host,
            shell: ShellKind::PowerShell,
            shell_executable: Some("pwsh.exe".to_owned()),
            environment: TerminalEnvironment::Windows,
            pid: None,
            parent_pid: None,
            tty: None,
            profile: None,
            title: None,
            working_directory: directory.map(str::to_owned),
            working_directory_source: WorkingDirectorySource::Unknown,
            startup_command: None,
            foreground_command: None,
            restart: Some(RestartPlan {
                executable: "pwsh.exe".to_owned(),
                args: Vec::new(),
                working_directory: directory.map(str::to_owned),
                note: None,
            }),
        }
    }

    fn action(kind: &str, profile: &str, split: Option<&str>) -> TerminalLayoutAction {
        TerminalLayoutAction {
            action: kind.to_owned(),
            profile: Some(profile.to_owned()),
            commandline: None,
            starting_directory: Some(r"C:\Work".to_owned()),
            tab_title: Some("Work".to_owned()),
            split: split.map(str::to_owned),
            size: Some(0.4),
            title: None,
            pane_id: None,
        }
    }

    #[test]
    fn session_match_is_case_insensitive_and_directory_aware() {
        assert!(session_matches(
            &session(TerminalHost::ConsoleHost, Some(r"C:\Work")),
            &session(TerminalHost::ConsoleHost, Some(r"c:/work/")),
        ));
        assert!(!session_matches(
            &session(TerminalHost::ConsoleHost, Some(r"C:\Work")),
            &session(TerminalHost::ConsoleHost, Some(r"C:\Other")),
        ));
    }

    #[test]
    fn windows_terminal_plan_recreates_tabs_and_panes_without_commandline() {
        let layout = WindowsTerminalLayout {
            source_path: "state.json".to_owned(),
            window_index: 0,
            name: None,
            initial_position: Some("20,30".to_owned()),
            initial_size: Some(TerminalWindowSize {
                width: 120.0,
                height: 36.0,
            }),
            launch_mode: Some("maximized".to_owned()),
            actions: vec![
                action("newTab", "PowerShell", None),
                action("splitPane", "Ubuntu", Some("vertical")),
            ],
        };
        let plan = windows_terminal_plan(&layout).unwrap().unwrap();
        assert_eq!(plan.executable, "wt.exe");
        assert_eq!(&plan.args[..2], ["-w", "new"]);
        assert!(plan.args.contains(&";".to_owned()));
        assert!(plan.args.contains(&"split-pane".to_owned()));
        assert!(plan.args.contains(&"-V".to_owned()));
        assert!(!plan.args.iter().any(|arg| arg.contains("commandline")));
    }

    #[test]
    fn vscode_integrated_sessions_are_delegated_not_spawned_as_external_shells() {
        assert!(is_vscode_integrated(TerminalHost::VisualStudioCode));
        assert!(is_vscode_integrated(TerminalHost::Cursor));
        assert!(!is_vscode_integrated(TerminalHost::WindowsTerminal));
    }

    #[test]
    fn non_restorable_snapshot_is_graceful() {
        let snapshot = TerminalSnapshot {
            status: TerminalStatus::Unsupported,
            message: Some("unsupported".to_owned()),
            windows_terminal_layouts: Vec::new(),
            sessions: Vec::new(),
            warnings: Vec::new(),
            history: TerminalHistoryPolicy {
                captured: false,
                reason: "test".to_owned(),
            },
        };
        let report = restore(&snapshot, true);
        assert!(report.success());
        assert_eq!(report.warnings, ["unsupported"]);
    }
}
