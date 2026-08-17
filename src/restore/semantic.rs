use crate::{
    adapters::{
        docker::{self, DockerSnapshot, DockerStatus},
        terminal::{self, RestartPlan, TerminalEnvironment, TerminalHost, TerminalSession, TerminalSnapshot, TerminalStatus},
    },
    restore_bus,
};
use serde_json::{Value, json};
use std::{collections::HashSet, process::Command, time::Duration};

const ADAPTER_TIMEOUT: Duration = Duration::from_secs(25);

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SemanticRestoreReport {
    pub warnings: Vec<String>,
    pub failures: Vec<String>,
}

impl SemanticRestoreReport {
    pub fn success(&self) -> bool {
        self.failures.is_empty()
    }
}

pub fn restore(snapshot: &Value, dry_run: bool) -> SemanticRestoreReport {
    let mut report = SemanticRestoreReport::default();

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
        report
            .warnings
            .push("Firefox semantic restore: would reconcile saved tabs, groups, containers and browser windows".to_owned());
        return;
    }

    let request = match restore_bus::write_request("firefox", saved) {
        Ok(request) => request,
        Err(error) => {
            report
                .failures
                .push(format!("Firefox semantic restore request failed: {error}"));
            return;
        }
    };

    match restore_bus::wait_for_completion("firefox", &request.request_id, ADAPTER_TIMEOUT) {
        Ok(Some(completion)) if completion.ok => {
            report.warnings.push(format!(
                "Firefox semantic restore: {} resource(s) changed, {} already satisfied/skipped",
                completion.changed, completion.skipped
            ));
            report.warnings.extend(
                completion
                    .warnings
                    .into_iter()
                    .map(|warning| format!("Firefox: {warning}")),
            );
        }
        Ok(Some(completion)) => report.failures.push(format!(
            "Firefox semantic restore failed: {}",
            completion
                .error
                .unwrap_or_else(|| "extension reported failure".to_owned())
        )),
        Ok(None) => report.failures.push(
            "Firefox semantic restore timed out waiting for the Context Capsule browser extension/native host"
                .to_owned(),
        ),
        Err(error) => report
            .failures
            .push(format!("Firefox semantic restore wait failed: {error}")),
    }
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
                format!(" and {} integrated terminal session(s)", integrated_terminals.len())
            }
        ));
        return;
    }

    let payload = json!({
        "editor": editor,
        "terminals": integrated_terminals,
    });
    let request = match restore_bus::write_request("vscode", payload) {
        Ok(request) => request,
        Err(error) => {
            report
                .failures
                .push(format!("VS Code semantic restore request failed: {error}"));
            return;
        }
    };

    match restore_bus::wait_for_completion("vscode", &request.request_id, ADAPTER_TIMEOUT) {
        Ok(Some(completion)) if completion.ok => {
            report.warnings.push(format!(
                "VS Code semantic restore: {} resource(s) changed, {} already satisfied/skipped",
                completion.changed, completion.skipped
            ));
            report.warnings.extend(
                completion
                    .warnings
                    .into_iter()
                    .map(|warning| format!("VS Code: {warning}")),
            );
        }
        Ok(Some(completion)) => report.failures.push(format!(
            "VS Code semantic restore failed: {}",
            completion
                .error
                .unwrap_or_else(|| "extension reported failure".to_owned())
        )),
        Ok(None) => report.failures.push(
            "VS Code semantic restore timed out waiting for the Context Capsule VS Code extension"
                .to_owned(),
        ),
        Err(error) => report
            .failures
            .push(format!("VS Code semantic restore wait failed: {error}")),
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

    let running_projects = current
        .compose_projects
        .iter()
        .map(|project| project.name.to_ascii_lowercase())
        .collect::<HashSet<_>>();
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
            .filter(|project| !running_projects.contains(&project.name.to_ascii_lowercase()))
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

fn restore_terminals(snapshot: &Value, dry_run: bool, report: &mut SemanticRestoreReport) {
    let Some(saved) = terminal_snapshot(snapshot) else {
        return;
    };
    if !matches!(saved.status, TerminalStatus::Available | TerminalStatus::Degraded) {
        return;
    }

    let current = terminal::discover();
    let mut used = HashSet::new();
    let mut missing = Vec::new();

    for saved_session in &saved.sessions {
        if matches!(saved_session.host, TerminalHost::VisualStudioCode) {
            continue;
        }
        if matches!(saved_session.host, TerminalHost::Cursor) {
            report.warnings.push(
                "Terminal restore: Cursor integrated terminals are preserved in the capsule but no Cursor adapter is installed yet"
                    .to_owned(),
            );
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
            missing.push(plan);
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
            report
                .warnings
                .push("Terminal restore: all safely restorable external sessions are already present".to_owned());
        }
        return;
    }
    if dry_run {
        report.warnings.push(format!(
            "Terminal restore: would start {} missing safe session(s)",
            missing.len()
        ));
        return;
    }

    #[cfg(windows)]
    {
        let mut restored = 0usize;
        for plan in missing {
            match launch_restart_plan(&plan) {
                Ok(()) => restored += 1,
                Err(error) => report.failures.push(format!("Terminal: {error}")),
            }
        }
        report.warnings.push(format!(
            "Terminal restore: started {restored} missing safe session(s)"
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
    if saved.host != current.host || !environment_matches(&saved.environment, &current.environment) {
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
    let normalize = |value: &str| value.trim().replace('/', "\\").trim_end_matches('\\').to_ascii_lowercase();
    normalize(left) == normalize(right)
}

#[cfg(windows)]
fn launch_restart_plan(plan: &RestartPlan) -> Result<(), String> {
    let mut command = Command::new(&plan.executable);
    command.args(&plan.args);
    if let Some(directory) = plan.working_directory.as_deref() {
        command.current_dir(directory);
    }
    command
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("failed to start '{}': {error}", plan.executable))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::docker::{ComposeProject, ContainerResource};

    #[test]
    fn docker_plan_omits_resource_groups_already_running() {
        let project = ComposeProject {
            name: "demo".to_owned(),
            working_directory: None,
            config_files: Vec::new(),
            services: vec!["web".to_owned()],
            containers: Vec::new(),
        };
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
            compose_projects: vec![project.clone()],
            standalone_containers: vec![container.clone()],
        };
        let current = saved.clone();
        let missing = missing_docker_resources(&saved, &current);
        assert!(missing.compose_projects.is_empty());
        assert!(missing.standalone_containers.is_empty());

        let current = DockerSnapshot {
            status: DockerStatus::Available,
            context: None,
            message: None,
            compose_projects: vec![project],
            standalone_containers: Vec::new(),
        };
        let missing = missing_docker_resources(&saved, &current);
        assert!(missing.compose_projects.is_empty());
        assert_eq!(missing.standalone_containers, vec![container]);
    }

    #[test]
    fn terminal_matching_respects_multiplicity_and_working_directory() {
        let session = TerminalSession {
            sources: Vec::new(),
            host: TerminalHost::WindowsTerminal,
            shell: crate::adapters::terminal::ShellKind::PowerShell,
            shell_executable: Some("pwsh.exe".to_owned()),
            environment: TerminalEnvironment::Windows,
            pid: None,
            parent_pid: None,
            tty: None,
            profile: Some("PowerShell".to_owned()),
            title: None,
            working_directory: Some(r"C:\Work\Project".to_owned()),
            working_directory_source: crate::adapters::terminal::WorkingDirectorySource::WindowsTerminalState,
            startup_command: None,
            foreground_command: None,
            restart: None,
        };
        let mut other = session.clone();
        other.working_directory = Some(r"C:/work/project".to_owned());
        assert!(terminal_session_matches(&session, &other));
        other.working_directory = Some(r"C:\Other".to_owned());
        assert!(!terminal_session_matches(&session, &other));
    }
}
