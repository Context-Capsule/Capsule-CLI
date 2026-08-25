use crate::{
    adapters::{
        docker::{self, ComposeProject, DockerSnapshot, DockerStatus},
        terminal::{
            self, RestartPlan, TerminalEnvironment, TerminalHost, TerminalSession,
            TerminalSnapshot, TerminalStatus,
        },
    },
    restore_bus, terminal_context,
};
use serde_json::{Value, json};
use std::{collections::HashSet, time::Duration};

#[cfg(windows)]
use std::{
    os::windows::process::CommandExt,
    process::{Command, Stdio},
    thread,
    time::Instant,
};

const VSCODE_ADAPTER_TIMEOUT: Duration = Duration::from_secs(25);
const FIREFOX_ADAPTER_TIMEOUT: Duration = Duration::from_secs(60);
const CHROME_ADAPTER_TIMEOUT: Duration = Duration::from_secs(60);
#[cfg(windows)]
const TERMINAL_LAUNCH_SPACING: Duration = Duration::from_millis(120);
#[cfg(windows)]
const TERMINAL_VERIFY_TIMEOUT: Duration = Duration::from_secs(10);
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
    // adapters are then handled one at a time to avoid simultaneous large
    // browser/editor restores. External terminals are last.
    restore_docker(snapshot, dry_run, &mut report);
    restore_vscode(snapshot, dry_run, &mut report);
    restore_firefox(snapshot, dry_run, &mut report);
    restore_chrome(snapshot, dry_run, &mut report);
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

    run_bus_adapter("firefox", "Firefox", saved, FIREFOX_ADAPTER_TIMEOUT, report);
}

fn restore_chrome(snapshot: &Value, dry_run: bool, report: &mut SemanticRestoreReport) {
    let Some(saved) = snapshot
        .pointer("/browsers/chrome")
        .cloned()
        .filter(|value| !value.is_null())
    else {
        return;
    };

    if dry_run {
        report.warnings.push(
            "Chrome semantic restore: would reconcile saved tabs, tab groups and browser windows"
                .to_owned(),
        );
        return;
    }

    run_bus_adapter("chrome", "Chrome", saved, CHROME_ADAPTER_TIMEOUT, report);
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
            let diagnostic_hint = match adapter {
                "firefox" => {
                    "; inspect the persistent firefox.log to distinguish an adapter startup failure from an in-progress browser restore"
                }
                "chrome" => {
                    "; inspect the persistent chrome.log to distinguish an adapter startup failure from an in-progress browser restore"
                }
                _ => "",
            };
            report.failures.push(format!(
                "{label} semantic restore timed out after {} seconds waiting for its Context Capsule adapter{diagnostic_hint}",
                timeout.as_secs(),
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
                !current.compose_projects.iter().any(|current_project| {
                    compose_project_satisfied(saved_project, current_project)
                })
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
    saved
        .containers
        .iter()
        .all(|container| current_names.contains(&container.name.to_ascii_lowercase()))
}

fn restore_terminals(snapshot: &Value, dry_run: bool, report: &mut SemanticRestoreReport) {
    let Some(saved) = terminal_snapshot(snapshot) else {
        return;
    };
    if !matches!(
        saved.status,
        TerminalStatus::Available | TerminalStatus::Degraded
    ) {
        return;
    }

    let current = terminal_context::enrich_for_matching(&terminal::discover());
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

        if let Some(plan) = safe_restart_plan(saved_session) {
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

fn safe_restart_plan(saved: &TerminalSession) -> Option<RestartPlan> {
    if saved.host != TerminalHost::WindowsTerminal
        || !matches!(saved.environment, TerminalEnvironment::Windows)
    {
        return saved.restart.clone();
    }

    // A Windows Terminal session must always be reopened through wt.exe. Older
    // capsules could contain a process-derived restart plan such as
    // powershell.exe even though the session host was WindowsTerminal. Launching
    // that child directly inherits the caller's terminal handles and can inject
    // an interactive shell into the VS Code/console session running `capsule
    // restore`. Rebuild the plan from semantic WT metadata instead.
    let mut args = vec!["new-tab".to_owned()];
    if let Some(profile) = saved
        .profile
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        args.push("-p".to_owned());
        args.push(profile.to_owned());
    }
    if let Some(directory) = saved
        .working_directory
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        args.push("-d".to_owned());
        args.push(directory.to_owned());
    }

    // If state.json did not identify the profile, preserve only the shell
    // executable itself. Do not replay startup/foreground command lines.
    if saved.profile.as_deref().is_none_or(|profile| profile.trim().is_empty()) {
        let executable = saved
            .shell_executable
            .as_deref()
            .or_else(|| saved.restart.as_ref().map(|plan| plan.executable.as_str()))?;
        if !direct_shell_process(executable) {
            return None;
        }
        args.push(executable.to_owned());
    }

    Some(RestartPlan {
        executable: "wt.exe".to_owned(),
        args,
        working_directory: None,
        note: Some(
            "Reopens the captured Windows Terminal session through wt.exe so the restore cannot attach an interactive shell to the terminal running Context Capsule."
                .to_owned(),
        ),
    })
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
    match (
        saved.working_directory.as_deref(),
        current.working_directory.as_deref(),
    ) {
        (Some(saved_directory), Some(current_directory)) => {
            paths_equivalent(current_directory, saved_directory)
        }
        // This is only used after Context Capsule itself created this exact
        // child PID and set Command::current_dir from the restart plan. Windows
        // process discovery may temporarily be unable to read that CWD back.
        (Some(_), None) | (None, _) => true,
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
        (TerminalEnvironment::Wsl { distro: left }, TerminalEnvironment::Wsl { distro: right }) => {
            match (left.as_deref(), right.as_deref()) {
                (Some(left), Some(right)) => left.eq_ignore_ascii_case(right),
                _ => true,
            }
        }
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
    terminal_context::enrich_for_matching(&terminal::discover())
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
        let current = terminal_context::enrich_for_matching(&terminal::discover());
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
    if session.host == TerminalHost::WindowsTerminal && !windows_terminal_launcher(&plan.executable) {
        return Err(format!(
            "refusing unsafe Windows Terminal restart plan '{}'; Windows Terminal sessions must be launched through wt.exe",
            plan.executable
        ));
    }

    let mut command = Command::new(&plan.executable);
    command.args(&plan.args);
    if let Some(directory) = plan.working_directory.as_deref() {
        command.current_dir(directory);
    }

    if windows_terminal_launcher(&plan.executable) {
        // wt.exe is only a launcher. It must never inherit the stdin/stdout of
        // the terminal running Context Capsule, especially an integrated VS Code
        // terminal.
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
    } else if needs_fresh_console_window(session, plan) {
        command.creation_flags(CREATE_NEW_CONSOLE | CREATE_NEW_PROCESS_GROUP);
    }

    command
        .spawn()
        .map(|child| child.id())
        .map_err(|error| format!("failed to start '{}': {error}", plan.executable))
}

#[cfg(any(windows, test))]
fn windows_terminal_launcher(executable: &str) -> bool {
    let executable = executable
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or(executable)
        .to_ascii_lowercase();
    matches!(executable.as_str(), "wt.exe" | "wt")
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

#[cfg(any(windows, test))]
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
        docker::{ComposeProject, ContainerResource},
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
    fn windows_terminal_direct_powershell_plan_is_rewritten_through_wt() {
        let mut saved = terminal_session(TerminalHost::WindowsTerminal, ShellKind::WindowsPowerShell);
        saved.shell_executable = Some(r"C:\WINDOWS\System32\WindowsPowerShell\v1.0\powershell.exe".to_owned());
        saved.profile = Some("Windows PowerShell".to_owned());
        saved.working_directory = Some(r"D:\projects\capsule".to_owned());
        saved.restart = Some(RestartPlan {
            executable: r"C:\WINDOWS\System32\WindowsPowerShell\v1.0\powershell.exe".to_owned(),
            args: Vec::new(),
            working_directory: Some(r"C:\Users\example".to_owned()),
            note: None,
        });

        let plan = safe_restart_plan(&saved).expect("safe Windows Terminal plan");
        assert_eq!(plan.executable, "wt.exe");
        assert_eq!(
            plan.args,
            vec![
                "new-tab".to_owned(),
                "-p".to_owned(),
                "Windows PowerShell".to_owned(),
                "-d".to_owned(),
                r"D:\projects\capsule".to_owned(),
            ]
        );
        assert!(plan.working_directory.is_none());
    }

    #[test]
    fn windows_terminal_without_profile_uses_only_safe_shell_executable() {
        let mut saved = terminal_session(TerminalHost::WindowsTerminal, ShellKind::WindowsPowerShell);
        saved.shell_executable = Some(r"C:\WINDOWS\System32\WindowsPowerShell\v1.0\powershell.exe".to_owned());
        saved.working_directory = Some(r"D:\projects\capsule".to_owned());
        saved.startup_command = Some("powershell.exe -NoExit -Command dangerous-user-history".to_owned());
        saved.restart = Some(RestartPlan {
            executable: saved.shell_executable.clone().unwrap(),
            args: Vec::new(),
            working_directory: None,
            note: None,
        });

        let plan = safe_restart_plan(&saved).expect("safe Windows Terminal plan");
        assert_eq!(plan.executable, "wt.exe");
        assert_eq!(
            plan.args,
            vec![
                "new-tab".to_owned(),
                "-d".to_owned(),
                r"D:\projects\capsule".to_owned(),
                r"C:\WINDOWS\System32\WindowsPowerShell\v1.0\powershell.exe".to_owned(),
            ]
        );
        assert!(!plan.args.iter().any(|arg| arg.contains("dangerous-user-history")));
    }

    #[test]
    fn console_host_and_unknown_are_compatible_for_standalone_shells() {
        let saved = terminal_session(TerminalHost::ConsoleHost, ShellKind::CommandPrompt);
        let current = terminal_session(TerminalHost::Unknown, ShellKind::CommandPrompt);
        assert!(terminal_session_matches(&saved, &current));
    }

    #[test]
    fn spawned_direct_shell_verification_does_not_require_host_ancestry_match() {
        let mut saved = terminal_session(TerminalHost::ConsoleHost, ShellKind::CommandPrompt);
        saved.working_directory = Some(r"C:\Work".to_owned());
        let current = terminal_session(TerminalHost::WindowsTerminal, ShellKind::CommandPrompt);
        assert!(!terminal_session_matches(&saved, &current));
        assert!(launched_session_matches(&saved, &current));

        let mut wrong = current;
        wrong.working_directory = Some(r"C:\Other".to_owned());
        assert!(!launched_session_matches(&saved, &wrong));
    }

    #[test]
    fn chrome_dry_run_is_reported_without_touching_firefox() {
        let snapshot = json!({
            "browsers": {
                "chrome": { "browser": "chrome", "windows": [] },
                "firefox": null
            }
        });
        let mut report = SemanticRestoreReport::default();
        restore_chrome(&snapshot, true, &mut report);
        assert_eq!(report.warnings.len(), 1);
        assert!(report.warnings[0].starts_with("Chrome semantic restore:"));
        assert!(report.failures.is_empty());
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
        assert!(windows_terminal_launcher(&wt.executable));
    }
}
