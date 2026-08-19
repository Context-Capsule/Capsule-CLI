use crate::{
    adapters::{
        docker::{self, ComposeProject, DockerSnapshot, DockerStatus},
        terminal::{
            self, RestartPlan, TerminalEnvironment, TerminalHost, TerminalSession,
            TerminalSnapshot, TerminalStatus,
        },
    },
    restore_bus,
};
use serde_json::{Value, json};
use std::{collections::HashSet, time::Duration};

#[cfg(windows)]
use std::{
    os::windows::process::CommandExt,
    process::Command,
    thread,
    time::Instant,
};

const VSCODE_ADAPTER_TIMEOUT: Duration = Duration::from_secs(25);
const FIREFOX_ADAPTER_TIMEOUT: Duration = Duration::from_secs(60);
#[cfg(windows)]
const TERMINAL_LAUNCH_SPACING: Duration = Duration::from_millis(120);
#[cfg(windows)]
const TERMINAL_VERIFY_TIMEOUT: Duration = Duration::from_secs(6);
#[cfg(windows)]
const TERMINAL_VERIFY_POLL: Duration = Duration::from_millis(250);
#[cfg(windows)]
const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
#[cfg(windows)]
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SemanticRestoreReport {
    pub warnings: Vec<String>,
    pub failures: Vec<String>,
}

pub fn restore(snapshot: &Value, dry_run: bool) -> SemanticRestoreReport {
    let mut report = SemanticRestoreReport::default();

    // Docker can be restored without a GUI host, so converge it first. The GUI
    // adapters are then handled one at a time to avoid a large browser + editor
    // state restore hitting the machine simultaneously. External terminals are last.
    restore_docker(snapshot, dry_run, &mut report);
    restore_vscode(snapshot, dry_run, &mut report);
    restore_firefox(snapshot, dry_run, &mut report);
    restore_terminals(snapshot, dry_run, &mut report);

    report
}

fn restore_firefox(snapshot: &Value, dry_run: bool, report: &mut SemanticRestoreReport) {
    let Some(saved) = snapshot
        .pointer("/browsers/firefox")
        .cloned()
        .filter(|value| !value.is_null())
    else {
        return;
    };

    if dry_run {
        report.warnings.push(
            "Firefox semantic restore: would reconcile saved tabs, groups, containers and browser windows"
                .to_owned(),
        );
        return;
    }

    run_bus_adapter(
        "firefox",
        "Firefox",
        saved,
        FIREFOX_ADAPTER_TIMEOUT,
        report,
    );
}

fn restore_vscode(snapshot: &Value, dry_run: bool, report: &mut SemanticRestoreReport) {
    let editor = snapshot
        .pointer("/editors/vscode")
        .cloned()
        .filter(|value| !value.is_null());
    let integrated_terminals = terminal_snapshot(snapshot)
        .map(|terminals| {
            terminals
                .sessions
                .into_iter()
                .filter(|session| session.host == TerminalHost::VisualStudioCode)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    if editor.is_none() && integrated_terminals.is_empty() {
        return;
    }

    if dry_run {
        report.warnings.push(format!(
            "VS Code semantic restore: would reconcile editor state{}",
            if integrated_terminals.is_empty() {
                String::new()
            } else {
                format!(
                    " and {} integrated terminal session(s)",
                    integrated_terminals.len()
                )
            }
        ));
        return;
    }

    run_bus_adapter(
        "vscode",
        "VS Code",
        json!({
            "editor": editor,
            "terminals": integrated_terminals,
        }),
        VSCODE_ADAPTER_TIMEOUT,
        report,
    );
}

fn run_bus_adapter(
    adapter: &str,
    label: &str,
    payload: Value,
    timeout: Duration,
    report: &mut SemanticRestoreReport,
) {
    let request = match restore_bus::write_request(adapter, payload) {
        Ok(request) => request,
        Err(error) => {
            report
                .failures
                .push(format!("{label} semantic restore request failed: {error}"));
            return;
        }
    };

    match restore_bus::wait_for_completion(adapter, &request.request_id, timeout) {
        Ok(Some(completion)) if completion.ok => {
            report.warnings.push(format!(
                "{label} semantic restore: {} resource(s) changed, {} already satisfied/skipped",
                completion.changed, completion.skipped
            ));
            report.warnings.extend(
                completion
                    .warnings
                    .into_iter()
                    .map(|warning| format!("{label}: {warning}")),
            );
        }
        Ok(Some(completion)) => report.failures.push(format!(
            "{label} semantic restore failed: {}",
            completion
                .error
                .unwrap_or_else(|| "adapter reported failure".to_owned())
        )),
        Ok(None) => {
            let _ = restore_bus::cancel_request(adapter, &request.request_id);
            report.failures.push(format!(
                "{label} semantic restore timed out after {} seconds waiting for its Context Capsule adapter{}",
                timeout.as_secs(),
                if adapter == "firefox" {
                    "; inspect the persistent firefox.log to distinguish an adapter startup failure from an in-progress browser restore"
                } else {
                    ""
                }
            ));
        }
        Err(error) => {
            let _ = restore_bus::cancel_request(adapter, &request.request_id);
            report
                .failures
                .push(format!("{label} semantic restore wait failed: {error}"));
        }
    }
}

fn restore_docker(snapshot: &Value, dry_run: bool, report: &mut SemanticRestoreReport) {
    let Some(saved_value) = snapshot.get("docker").cloned() else {
        return;
    };
    let saved: DockerSnapshot = match serde_json::from_value(saved_value) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            report
                .failures
                .push(format!("Docker restore metadata is invalid: {error}"));
            return;
        }
    };
    if !matches!(saved.status, DockerStatus::Available) || saved.running_container_count() == 0 {
        return;
    }

    let current = docker::discover();
    let missing = missing_docker_resources(&saved, &current);
    let missing_count = missing.compose_projects.len() + missing.standalone_containers.len();
    if missing_count == 0 {
        report
            .warnings
            .push("Docker restore: all captured resource groups are already running".to_owned());
        return;
    }
    if dry_run {
        report.warnings.push(format!(
            "Docker restore: would start {missing_count} missing resource group(s)"
        ));
        return;
    }

    let docker_report = docker::restore(&missing);
    report.warnings.push(format!(
        "Docker restore: restored {}/{} missing resource group(s)",
        docker_report.restored_resources, docker_report.attempted_resources
    ));
    report.warnings.extend(
        docker_report
            .warnings
            .into_iter()
            .map(|warning| format!("Docker: {warning}")),
    );
    report.failures.extend(
        docker_report
            .failures
            .into_iter()
            .map(|failure| format!("Docker: {failure}")),
    );
}

fn missing_docker_resources(saved: &DockerSnapshot, current: &DockerSnapshot) -> DockerSnapshot {
    if !matches!(current.status, DockerStatus::Available) {
        return saved.clone();
    }

    let running_containers = current
        .standalone_containers
        .iter()
        .map(|container| container.name.to_ascii_lowercase())
        .collect::<HashSet<_>>();

    DockerSnapshot {
        status: DockerStatus::Available,
        context: saved.context.clone(),
        message: None,
        compose_projects: saved
            .compose_projects
            .iter()
            .filter(|saved_project| {
                !current
                    .compose_projects
                    .iter()
                    .any(|current_project| compose_project_satisfied(saved_project, current_project))
            })
            .cloned()
            .collect(),
        standalone_containers: saved
            .standalone_containers
            .iter()
            .filter(|container| !running_containers.contains(&container.name.to_ascii_lowercase()))
            .cloned()
            .collect(),
    }
}

fn compose_project_satisfied(saved: &ComposeProject, current: &ComposeProject) -> bool {
    if !saved.name.eq_ignore_ascii_case(&current.name) {
        return false;
    }

    if !saved.services.is_empty() {
        let current_services = current
            .services
            .iter()
            .map(|service| service.to_ascii_lowercase())
            .collect::<HashSet<_>>();
        return saved
            .services
            .iter()
            .all(|service| current_services.contains(&service.to_ascii_lowercase()));
    }

    let current_names = current
        .containers
        .iter()
        .map(|container| container.name.to_ascii_lowercase())
        .collect::<HashSet<_>>();
    saved.containers.iter().all(|container| {
        current_names.contains(&container.name.to_ascii_lowercase())
    })
}

fn restore_terminals(snapshot: &Value, dry_run: bool, report: &mut SemanticRestoreReport) {
    let Some(saved) = terminal_snapshot(snapshot) else {
        return;
    };
    if !matches!(saved.status, TerminalStatus::Available | TerminalStatus::Degraded) {
        return;
    }

    let current = terminal::discover();
    let mut used = HashSet::new();
    let mut missing: Vec<(TerminalSession, RestartPlan)> = Vec::new();
    let mut cursor_warning_added = false;

    for saved_session in &saved.sessions {
        if saved_session.host == TerminalHost::VisualStudioCode {
            continue;
        }
        if saved_session.host == TerminalHost::Cursor {
            if !cursor_warning_added {
                report.warnings.push(
                    "Terminal restore: Cursor integrated terminals are preserved in the capsule but no Cursor adapter is installed yet"
                        .to_owned(),
                );
                cursor_warning_added = true;
            }
            continue;
        }

        if let Some(index) = current
            .sessions
            .iter()
            .enumerate()
            .find(|(index, current_session)| {
                !used.contains(index) && terminal_session_matches(saved_session, current_session)
            })
            .map(|(index, _)| index)
        {
            used.insert(index);
            continue;
        }

        if let Some(plan) = saved_session.restart.clone() {
            missing.push((saved_session.clone(), plan));
        } else {
            report.warnings.push(format!(
                "Terminal restore: {:?} / {} has no safe restart plan",
                saved_session.host,
                saved_session.shell.as_str()
            ));
        }
    }

    if missing.is_empty() {
        if !saved.sessions.is_empty() {
            report.warnings.push(
                "Terminal restore: all safely restorable external sessions are already present"
                    .to_owned(),
            );
        }
        return;
    }
    if dry_run {
        report.warnings.push(format!(
            "Terminal restore: would start and verify {} missing safe session(s)",
            missing.len()
        ));
        return;
    }

    #[cfg(windows)]
    {
        let total = missing.len();
        let mut verified = 0usize;
        for (index, (saved_session, plan)) in missing.into_iter().enumerate() {
            let baseline = matching_terminal_count(&saved_session);
            match launch_restart_plan(&saved_session, &plan) {
                Ok(pid) => {
                    if wait_for_terminal_launch(
                        &saved_session,
                        &plan,
                        pid,
                        baseline,
                        TERMINAL_VERIFY_TIMEOUT,
                    ) {
                        verified += 1;
                    } else {
                        report.failures.push(format!(
                            "Terminal: started '{}' as process {pid}, but no matching interactive {:?} / {} session became observable within {} ms",
                            plan.executable,
                            saved_session.host,
                            saved_session.shell.as_str(),
                            TERMINAL_VERIFY_TIMEOUT.as_millis()
                        ));
                    }
                }
                Err(error) => report.failures.push(format!("Terminal: {error}")),
            }
            if index + 1 < total {
                thread::sleep(TERMINAL_LAUNCH_SPACING);
            }
        }
        report.warnings.push(format!(
            "Terminal restore: verified {verified}/{total} missing safe session(s)"
        ));
    }

    #[cfg(not(windows))]
    {
        let _ = missing;
        report
            .warnings
            .push("Terminal restore is currently implemented for Windows/WSL only".to_owned());
    }
}

fn terminal_snapshot(snapshot: &Value) -> Option<TerminalSnapshot> {
    let value = snapshot.get("terminals")?.clone();
    serde_json::from_value(value).ok()
}

fn terminal_session_matches(saved: &TerminalSession, current: &TerminalSession) -> bool {
    if !terminal_hosts_compatible(&saved.host, &current.host)
        || !environment_matches(&saved.environment, &current.environment)
    {
        return false;
    }
    if saved.shell != current.shell
        && saved.shell.as_str() != "Unknown shell"
        && current.shell.as_str() != "Unknown shell"
    {
        return false;
    }
    if let Some(profile) = saved.profile.as_deref() {
        if !current
            .profile
            .as_deref()
            .is_some_and(|value| value.eq_ignore_ascii_case(profile))
        {
            return false;
        }
    }
    match saved.working_directory.as_deref() {
        Some(directory) => current
            .working_directory
            .as_deref()
            .is_some_and(|value| paths_equivalent(value, directory)),
        None => true,
    }
}

#[cfg(any(windows, test))]
fn launched_session_matches(saved: &TerminalSession, current: &TerminalSession) -> bool {
    if !environment_matches(&saved.environment, &current.environment) {
        return false;
    }
    if saved.shell != current.shell
        && saved.shell.as_str() != "Unknown shell"
        && current.shell.as_str() != "Unknown shell"
    {
        return false;
    }
    match saved.working_directory.as_deref() {
        Some(directory) => current
            .working_directory
            .as_deref()
            .is_some_and(|value| paths_equivalent(value, directory)),
        None => true,
    }
}

fn terminal_hosts_compatible(saved: &TerminalHost, current: &TerminalHost) -> bool {
    saved == current
        || matches!(
            (saved, current),
            (TerminalHost::ConsoleHost, TerminalHost::Unknown)
                | (TerminalHost::Unknown, TerminalHost::ConsoleHost)
        )
}

fn environment_matches(left: &TerminalEnvironment, right: &TerminalEnvironment) -> bool {
    match (left, right) {
        (TerminalEnvironment::Windows, TerminalEnvironment::Windows) => true,
        (
            TerminalEnvironment::Wsl { distro: left },
            TerminalEnvironment::Wsl { distro: right },
        ) => match (left.as_deref(), right.as_deref()) {
            (Some(left), Some(right)) => left.eq_ignore_ascii_case(right),
            _ => true,
        },
        _ => false,
    }
}

fn paths_equivalent(left: &str, right: &str) -> bool {
    let normalize = |value: &str| {
        value
            .trim()
            .replace('/', "\\")
            .trim_end_matches('\\')
            .to_ascii_lowercase()
    };
    normalize(left) == normalize(right)
}

#[cfg(windows)]
fn matching_terminal_count(saved: &TerminalSession) -> usize {
    terminal::discover()
        .sessions
        .iter()
        .filter(|current| terminal_session_matches(saved, current))
        .count()
}

#[cfg(windows)]
fn wait_for_terminal_launch(
    saved: &TerminalSession,
    plan: &RestartPlan,
    spawned_pid: u32,
    baseline: usize,
    timeout: Duration,
) -> bool {
    let deadline = Instant::now() + timeout;
    let direct_shell =
        needs_fresh_console_window(saved, plan) && direct_shell_process(&plan.executable);

    loop {
        let current = terminal::discover();
        if direct_shell {
            if current.sessions.iter().any(|session| {
                session.pid == Some(spawned_pid) && launched_session_matches(saved, session)
            }) {
                return true;
            }
        } else if current
            .sessions
            .iter()
            .filter(|session| terminal_session_matches(saved, session))
            .count()
            > baseline
        {
            return true;
        }

        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(TERMINAL_VERIFY_POLL);
    }
}

#[cfg(windows)]
fn launch_restart_plan(session: &TerminalSession, plan: &RestartPlan) -> Result<u32, String> {
    let mut command = Command::new(&plan.executable);
    command.args(&plan.args);
    if let Some(directory) = plan.working_directory.as_deref() {
        command.current_dir(directory);
    }

    if needs_fresh_console_window(session, plan) {
        command.creation_flags(CREATE_NEW_CONSOLE | CREATE_NEW_PROCESS_GROUP);
    }

    command
        .spawn()
        .map(|child| child.id())
        .map_err(|error| format!("failed to start '{}': {error}", plan.executable))
}

#[cfg(windows)]
fn needs_fresh_console_window(session: &TerminalSession, plan: &RestartPlan) -> bool {
    if !matches!(
        &session.host,
        TerminalHost::ConsoleHost | TerminalHost::Unknown | TerminalHost::Wsl
    ) {
        return false;
    }
    fresh_console_executable(&plan.executable)
}

#[cfg(windows)]
fn fresh_console_executable(executable: &str) -> bool {
    let executable = executable
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or(executable)
        .to_ascii_lowercase();
    matches!(
        executable.as_str(),
        "cmd.exe"
            | "cmd"
            | "powershell.exe"
            | "powershell"
            | "pwsh.exe"
            | "pwsh"
            | "wsl.exe"
            | "wsl"
            | "bash.exe"
            | "bash"
            | "zsh.exe"
            | "zsh"
            | "fish.exe"
            | "fish"
            | "nu.exe"
            | "nu"
            | "nushell.exe"
            | "nushell"
            | "sh.exe"
            | "sh"
    )
}

#[cfg(windows)]
fn direct_shell_process(executable: &str) -> bool {
    let executable = executable
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or(executable)
        .to_ascii_lowercase();
    matches!(
        executable.as_str(),
        "cmd.exe"
            | "cmd"
            | "powershell.exe"
            | "powershell"
            | "pwsh.exe"
            | "pwsh"
            | "bash.exe"
            | "bash"
            | "zsh.exe"
            | "zsh"
            | "fish.exe"
            | "fish"
            | "nu.exe"
            | "nu"
            | "nushell.exe"
            | "nushell"
            | "sh.exe"
            | "sh"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::{
        docker::{ContainerResource, ComposeProject},
        terminal::{ShellKind, WorkingDirectorySource},
    };

    fn project(name: &str, services: &[&str]) -> ComposeProject {
        ComposeProject {
            name: name.to_owned(),
            working_directory: None,
            config_files: Vec::new(),
            services: services.iter().map(|value| (*value).to_owned()).collect(),
            containers: Vec::new(),
        }
    }

    fn terminal_session(host: TerminalHost, shell: ShellKind) -> TerminalSession {
        TerminalSession {
            sources: Vec::new(),
            host,
            shell,
            shell_executable: Some("cmd.exe".to_owned()),
            environment: TerminalEnvironment::Windows,
            pid: None,
            parent_pid: None,
            tty: None,
            profile: None,
            title: None,
            working_directory: None,
            working_directory_source: WorkingDirectorySource::Unknown,
            startup_command: None,
            foreground_command: None,
            restart: None,
        }
    }

    #[test]
    fn docker_plan_omits_only_fully_satisfied_resource_groups() {
        let container = ContainerResource {
            id: "1".to_owned(),
            name: "redis".to_owned(),
            image: None,
            ports: Vec::new(),
            mounts: Vec::new(),
            networks: Vec::new(),
        };
        let saved = DockerSnapshot {
            status: DockerStatus::Available,
            context: None,
            message: None,
            compose_projects: vec![project("demo", &["web", "worker"])],
            standalone_containers: vec![container.clone()],
        };
        let current = DockerSnapshot {
            status: DockerStatus::Available,
            context: None,
            message: None,
            compose_projects: vec![project("demo", &["web"])],
            standalone_containers: vec![container.clone()],
        };
        let missing = missing_docker_resources(&saved, &current);
        assert_eq!(
            missing.compose_projects.len(),
            1,
            "missing worker must trigger project restore"
        );
        assert!(missing.standalone_containers.is_empty());

        let current = saved.clone();
        let missing = missing_docker_resources(&saved, &current);
        assert!(missing.compose_projects.is_empty());
        assert!(missing.standalone_containers.is_empty());
    }

    #[test]
    fn terminal_matching_respects_working_directory() {
        let session = TerminalSession {
            profile: Some("PowerShell".to_owned()),
            working_directory: Some(r"C:\Work\Project".to_owned()),
            working_directory_source: WorkingDirectorySource::WindowsTerminalState,
            shell: ShellKind::PowerShell,
            host: TerminalHost::WindowsTerminal,
            shell_executable: Some("pwsh.exe".to_owned()),
            ..terminal_session(TerminalHost::WindowsTerminal, ShellKind::PowerShell)
        };
        let mut other = session.clone();
        other.working_directory = Some(r"C:/work/project".to_owned());
        assert!(terminal_session_matches(&session, &other));
        other.working_directory = Some(r"C:\Other".to_owned());
        assert!(!terminal_session_matches(&session, &other));
    }

    #[test]
    fn console_host_and_unknown_are_compatible_for_standalone_shells() {
        let saved = terminal_session(TerminalHost::ConsoleHost, ShellKind::CommandPrompt);
        let current = terminal_session(TerminalHost::Unknown, ShellKind::CommandPrompt);
        assert!(terminal_session_matches(&saved, &current));
    }

    #[test]
    fn spawned_direct_shell_verification_does_not_require_host_ancestry_match() {
        let saved = terminal_session(TerminalHost::ConsoleHost, ShellKind::CommandPrompt);
        let current = terminal_session(TerminalHost::WindowsTerminal, ShellKind::CommandPrompt);
        assert!(!terminal_session_matches(&saved, &current));
        assert!(launched_session_matches(&saved, &current));
    }

    #[cfg(windows)]
    #[test]
    fn raw_console_shell_restart_requests_a_fresh_console_only_for_standalone_hosts() {
        let session = terminal_session(TerminalHost::Unknown, ShellKind::CommandPrompt);
        let plan = RestartPlan {
            executable: r"C:\Windows\System32\cmd.exe".to_owned(),
            args: Vec::new(),
            working_directory: None,
            note: None,
        };
        assert!(needs_fresh_console_window(&session, &plan));
        assert!(direct_shell_process(&plan.executable));

        let windows_terminal =
            terminal_session(TerminalHost::WindowsTerminal, ShellKind::CommandPrompt);
        assert!(!needs_fresh_console_window(&windows_terminal, &plan));

        let wt = RestartPlan {
            executable: "wt.exe".to_owned(),
            ..plan
        };
        assert!(!needs_fresh_console_window(&session, &wt));
        assert!(!direct_shell_process(&wt.executable));
    }
}
