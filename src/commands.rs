use crate::{
    adapters::{
        docker::{self, DockerSnapshot, DockerStatus},
        terminal::{self, RestartPlan, TerminalEnvironment, TerminalSnapshot, TerminalStatus},
    },
    discovery,
    persistence::{CapsuleStore, StoredCapsuleSnapshot},
    snapshot::{self, CaptureOptions},
};
use context_capsule::{
    cleanup,
    restore::{self, RestoreOptions},
};
use serde_json::Value;
use std::process::ExitCode;

#[derive(Debug, Clone, PartialEq, Eq)]
struct SaveArguments {
    name: String,
    force: bool,
    ignored_applications: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum RestoreMode {
    #[default]
    Append,
    Replace,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RestoreArguments {
    name: String,
    dry_run: bool,
    mode: RestoreMode,
}

pub fn save(arguments: Vec<String>) -> ExitCode {
    let parsed = match parse_save_arguments(arguments) {
        Ok(parsed) => parsed,
        Err(error) => return usage_error(error),
    };
    let name = parsed.name;

    println!("Discovering workspace for capsule '{name}'...");
    let discovery = match discovery::discover(true, true, true, true) {
        Ok(snapshot) => snapshot,
        Err(error) => return command_error(format!("discovery failed: {error}")),
    };
    let ignored_names = match snapshot::validate_ignored_applications(
        &discovery,
        &parsed.ignored_applications,
    ) {
        Ok(names) => names,
        Err(error) => return usage_error(error),
    };
    let capture_options = CaptureOptions {
        ignored_applications: parsed.ignored_applications,
    };
    let stored = match snapshot::capture_snapshot_with_options(&discovery, &capture_options) {
        Ok(snapshot) => snapshot,
        Err(error) => return command_error(error.to_string()),
    };
    let mut store = match CapsuleStore::open_default() {
        Ok(store) => store,
        Err(error) => return command_error(error.to_string()),
    };
    let database_path = store.path().display().to_string();

    if let Err(error) = store.save(&name, &stored, parsed.force) {
        return command_error(error.to_string());
    }

    let applications = stored
        .snapshot
        .pointer("/desktop/applications")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    let terminal_sessions = stored
        .snapshot
        .pointer("/terminals/sessions")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    let wsl_sessions = stored
        .snapshot
        .pointer("/terminals/sessions")
        .and_then(Value::as_array)
        .map(|sessions| {
            sessions
                .iter()
                .filter(|session| {
                    session
                        .pointer("/environment/kind")
                        .and_then(Value::as_str)
                        == Some("wsl")
                })
                .count()
        })
        .unwrap_or(0);

    println!("Saved capsule '{name}'.");
    println!("  applications: {applications}");
    if !ignored_names.is_empty() {
        println!("  ignored applications: {}", ignored_names.len());
        for ignored in &ignored_names {
            println!("    - {ignored}");
        }
    }
    println!("  developer tools: {}", discovery.tools.len());
    println!("  terminal sessions: {terminal_sessions}");
    println!("  WSL terminal sessions: {wsl_sessions}");
    println!(
        "  running containers: {}",
        discovery.docker.running_container_count()
    );
    println!("  database: {database_path}");

    if matches!(discovery.docker.status, DockerStatus::Unavailable) {
        println!(
            "  Docker: {}",
            discovery.docker.message.as_deref().unwrap_or("unavailable")
        );
    }
    if matches!(discovery.terminals.status, TerminalStatus::Degraded) {
        println!("  terminals: captured with warnings; use 'capsule terminal inspect' for details");
    }

    ExitCode::SUCCESS
}

pub fn restore(arguments: Vec<String>) -> ExitCode {
    let parsed = match parse_restore_arguments(arguments) {
        Ok(parsed) => parsed,
        Err(error) => return usage_error(error),
    };
    let name = parsed.name;

    let store = match CapsuleStore::open_default() {
        Ok(store) => store,
        Err(error) => return command_error(error.to_string()),
    };
    let stored = match store.load(&name) {
        Ok(snapshot) => snapshot,
        Err(error) => return command_error(error.to_string()),
    };

    if parsed.dry_run {
        println!("Planning restore for capsule '{name}' (dry run)...");
    } else {
        println!("Restoring capsule '{name}'...");
    }
    println!(
        "  mode: {}",
        match parsed.mode {
            RestoreMode::Append => "append (preserve unrelated running applications)",
            RestoreMode::Replace => "replace (close unrelated running applications first)",
        }
    );

    if parsed.mode == RestoreMode::Replace {
        let cleanup = cleanup::close_unrelated_applications(&stored.snapshot, parsed.dry_run);
        println!("Application cleanup:");
        println!("  detected user applications: {}", cleanup.applications_detected);
        println!("  belonging to capsule:       {}", cleanup.applications_in_capsule);
        if parsed.dry_run {
            println!("  would close:                {}", cleanup.applications_planned_to_close);
        } else {
            println!("  close requests sent:        {}", cleanup.close_requests_sent);
            println!("  closed:                     {}", cleanup.applications_closed);
            if cleanup.applications_remaining > 0 {
                println!("  still running:              {}", cleanup.applications_remaining);
            }
        }
        if cleanup.applications_protected > 0 {
            println!("  protected hosts/shells:     {}", cleanup.applications_protected);
        }
        for warning in &cleanup.warnings {
            println!("  warning: {warning}");
        }
        for failure in &cleanup.failures {
            eprintln!("  failed: {failure}");
        }
        if !cleanup.success() {
            eprintln!("Replace-mode cleanup could not be established; restore was not started.");
            return ExitCode::from(1);
        }
    }

    // Keep the proven restore engine unchanged. Append mode reaches exactly the
    // same call it did before this feature; replace mode differs only by the
    // opt-in cleanup pass above.
    let report = restore::restore_snapshot(
        &stored.snapshot,
        RestoreOptions {
            dry_run: parsed.dry_run,
        },
    );
    let desktop = &report.desktop;
    println!("Desktop:");
    println!("  applications in capsule: {}", desktop.applications_total);
    println!(
        "  already running:         {}",
        desktop.applications_already_running
    );
    if parsed.dry_run {
        println!(
            "  would launch:            {}",
            desktop.applications_planned_to_launch
        );
        println!(
            "  windows already placed: {}",
            desktop.windows_already_placed
        );
        println!(
            "  windows to reposition:  {}",
            desktop.windows_planned_to_move
        );
    } else {
        println!("  launched:                {}", desktop.applications_launched);
        println!(
            "  windows already placed: {}",
            desktop.windows_already_placed
        );
        println!("  windows repositioned:   {}", desktop.windows_moved);
    }
    if desktop.applications_unlaunchable > 0 {
        println!(
            "  apps without launch identity: {}",
            desktop.applications_unlaunchable
        );
    }
    if desktop.windows_missing > 0 {
        println!("  saved windows not observed: {}", desktop.windows_missing);
    }

    for warning in report.warnings.iter().chain(desktop.warnings.iter()) {
        println!("  warning: {warning}");
    }
    for failure in report.failures.iter().chain(desktop.failures.iter()) {
        eprintln!("  failed: {failure}");
    }

    if report.success() {
        if parsed.dry_run {
            println!("Dry run complete; no applications or windows were changed.");
        } else {
            println!("Restore pass complete.");
        }
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

pub fn list(arguments: Vec<String>) -> ExitCode {
    if !arguments.is_empty() {
        return usage_error("'list' does not accept arguments".to_owned());
    }

    let store = match CapsuleStore::open_default() {
        Ok(store) => store,
        Err(error) => return command_error(error.to_string()),
    };
    let capsules = match store.list() {
        Ok(capsules) => capsules,
        Err(error) => return command_error(error.to_string()),
    };

    if capsules.is_empty() {
        println!("No capsules saved.");
        return ExitCode::SUCCESS;
    }

    println!("Saved capsules: {}", capsules.len());
    for capsule in capsules {
        println!(
            "  {}  [schema {}, updated {}]",
            capsule.name, capsule.schema_version, capsule.updated_at_unix_ms
        );
    }
    ExitCode::SUCCESS
}

pub fn show(arguments: Vec<String>) -> ExitCode {
    let (name, json) = match parse_show_arguments(arguments) {
        Ok(parsed) => parsed,
        Err(error) => return usage_error(error),
    };

    let store = match CapsuleStore::open_default() {
        Ok(store) => store,
        Err(error) => return command_error(error.to_string()),
    };
    let stored = match store.load(&name) {
        Ok(snapshot) => snapshot,
        Err(error) => return command_error(error.to_string()),
    };

    if json {
        match serde_json::to_string_pretty(&stored) {
            Ok(output) => println!("{output}"),
            Err(error) => return command_error(format!("failed to render snapshot: {error}")),
        }
        return ExitCode::SUCCESS;
    }

    print_capsule_summary(&name, &stored);
    ExitCode::SUCCESS
}

pub fn delete(arguments: Vec<String>) -> ExitCode {
    if arguments.len() != 1 {
        return usage_error("usage: capsule delete <name>".to_owned());
    }

    let name = &arguments[0];
    let mut store = match CapsuleStore::open_default() {
        Ok(store) => store,
        Err(error) => return command_error(error.to_string()),
    };
    match store.delete(name) {
        Ok(()) => {
            println!("Deleted capsule '{name}'.");
            ExitCode::SUCCESS
        }
        Err(error) => command_error(error.to_string()),
    }
}

pub fn docker(arguments: Vec<String>) -> ExitCode {
    match arguments.as_slice() {
        [command] if command == "inspect" => {
            let snapshot = docker::discover();
            print_docker_snapshot(&snapshot, true);
            ExitCode::SUCCESS
        }
        [command, name] if command == "restore" => restore_docker(name),
        _ => usage_error(
            "usage: capsule docker inspect | capsule docker restore <capsule-name>".to_owned(),
        ),
    }
}

pub fn terminal(arguments: Vec<String>) -> ExitCode {
    match arguments.as_slice() {
        [command] if command == "inspect" => {
            let snapshot = terminal::discover();
            print_terminal_snapshot(&snapshot, true);
            ExitCode::SUCCESS
        }
        [command, flag] if command == "inspect" && flag == "--json" => {
            let snapshot = terminal::discover();
            match serde_json::to_string_pretty(&snapshot) {
                Ok(output) => {
                    println!("{output}");
                    ExitCode::SUCCESS
                }
                Err(error) => command_error(format!("failed to render terminal snapshot: {error}")),
            }
        }
        _ => usage_error("usage: capsule terminal inspect [--json]".to_owned()),
    }
}

pub fn print_docker_snapshot(snapshot: &DockerSnapshot, verbose: bool) {
    match snapshot.status {
        DockerStatus::NotRequested => println!("Docker: not inspected"),
        DockerStatus::Unavailable => println!(
            "Docker: unavailable ({})",
            snapshot.message.as_deref().unwrap_or("unknown reason")
        ),
        DockerStatus::Available => {
            println!(
                "Docker: {} running container(s)",
                snapshot.running_container_count()
            );
            if let Some(context) = snapshot.context.as_ref() {
                println!("  context: {context}");
            }
            if let Some(message) = snapshot.message.as_ref() {
                println!("  warning: {message}");
            }

            for project in &snapshot.compose_projects {
                println!(
                    "  Compose project '{}' — {} container(s), services: {}",
                    project.name,
                    project.containers.len(),
                    if project.services.is_empty() {
                        "(unknown)".to_owned()
                    } else {
                        project.services.join(", ")
                    }
                );
                if verbose {
                    if let Some(directory) = project.working_directory.as_ref() {
                        println!("    directory: {directory}");
                    }
                    for config in &project.config_files {
                        println!("    compose:   {config}");
                    }
                    for container in &project.containers {
                        print_container(container, "    ");
                    }
                }
            }

            for container in &snapshot.standalone_containers {
                println!("  Standalone container '{}':", container.name);
                if verbose {
                    print_container(container, "    ");
                }
            }
        }
    }
}

pub fn print_terminal_snapshot(snapshot: &TerminalSnapshot, verbose: bool) {
    match snapshot.status {
        TerminalStatus::NotRequested => println!("Terminals: not inspected"),
        TerminalStatus::Unsupported => println!(
            "Terminals: unsupported ({})",
            snapshot
                .message
                .as_deref()
                .unwrap_or("unsupported platform")
        ),
        TerminalStatus::Available | TerminalStatus::Degraded => {
            let qualifier = if snapshot.status == TerminalStatus::Degraded {
                " (captured with warnings)"
            } else {
                ""
            };
            println!(
                "Terminals: {} open interactive session(s){qualifier}",
                snapshot.session_count()
            );
            println!("  WSL sessions: {}", snapshot.wsl_session_count());
            println!(
                "  Windows Terminal layouts: {}",
                snapshot.windows_terminal_layouts.len()
            );
            if let Some(message) = snapshot.message.as_ref() {
                println!("  note: {message}");
            }
            println!("  history captured: no");

            for (index, session) in snapshot.sessions.iter().enumerate() {
                println!(
                    "  [{}] {} — host {:?}",
                    index + 1,
                    session.shell.as_str(),
                    session.host
                );
                if let TerminalEnvironment::Wsl { distro } = &session.environment {
                    println!(
                        "      WSL distro: {}",
                        distro.as_deref().unwrap_or("unknown")
                    );
                }
                if let Some(profile) = session.profile.as_ref() {
                    println!("      profile: {profile}");
                }
                if let Some(directory) = session.working_directory.as_ref() {
                    println!(
                        "      cwd: {directory} ({:?})",
                        session.working_directory_source
                    );
                } else {
                    println!("      cwd: unknown");
                }
                if let Some(pid) = session.pid {
                    println!("      PID: {pid}");
                }
                if let Some(tty) = session.tty.as_ref() {
                    println!("      TTY: {tty}");
                }
                if verbose {
                    if let Some(command) = session.startup_command.as_ref() {
                        println!("      startup: {command}");
                    }
                    if let Some(command) = session.foreground_command.as_ref() {
                        println!("      foreground: {command}");
                    }
                    if let Some(restart) = session.restart.as_ref() {
                        println!("      restart: {}", render_restart_plan(restart));
                    }
                    println!("      sources: {:?}", session.sources);
                }
            }

            for warning in &snapshot.warnings {
                println!("  warning: {warning}");
            }
        }
    }
}

fn render_restart_plan(plan: &RestartPlan) -> String {
    let mut parts = vec![quote_for_display(&plan.executable)];
    parts.extend(plan.args.iter().map(|argument| quote_for_display(argument)));
    parts.join(" ")
}

fn quote_for_display(value: &str) -> String {
    if value.contains([' ', '\t', '"']) {
        format!("\"{}\"", value.replace('"', "\\\""))
    } else {
        value.to_owned()
    }
}

fn restore_docker(name: &str) -> ExitCode {
    let store = match CapsuleStore::open_default() {
        Ok(store) => store,
        Err(error) => return command_error(error.to_string()),
    };
    let stored = match store.load(name) {
        Ok(snapshot) => snapshot,
        Err(error) => return command_error(error.to_string()),
    };
    let snapshot = match stored.docker() {
        Ok(snapshot) => snapshot,
        Err(error) => return command_error(error.to_string()),
    };

    println!("Restoring Docker resources from capsule '{name}'...");
    let report = docker::restore(&snapshot);
    println!(
        "  restored: {}/{} resource group(s)",
        report.restored_resources, report.attempted_resources
    );
    for warning in &report.warnings {
        println!("  warning: {warning}");
    }
    for failure in &report.failures {
        eprintln!("  failed: {failure}");
    }

    if report.success() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

fn print_container(container: &crate::adapters::docker::ContainerResource, indent: &str) {
    println!("{indent}name: {}", container.name);
    if let Some(image) = container.image.as_ref() {
        println!("{indent}image: {image}");
    }
    if !container.ports.is_empty() {
        println!("{indent}ports: {}", container.ports.len());
    }
    if !container.mounts.is_empty() {
        println!("{indent}mounts: {}", container.mounts.len());
    }
    if !container.networks.is_empty() {
        println!("{indent}networks: {}", container.networks.join(", "));
    }
}

fn print_capsule_summary(name: &str, stored: &StoredCapsuleSnapshot) {
    println!("Capsule: {name}");
    println!("  schema: {}", stored.schema_version);
    println!("  captured: {}", stored.captured_at_unix_ms);

    if let Some(directory) = stored
        .snapshot
        .get("current_directory")
        .and_then(Value::as_str)
    {
        println!("  directory: {directory}");
    }

    let tool_count = stored
        .snapshot
        .get("tools")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    let app_count = stored
        .snapshot
        .pointer("/desktop/applications")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    let terminal_count = stored
        .snapshot
        .pointer("/terminals/sessions")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    println!("  developer tools: {tool_count}");
    println!("  applications: {app_count}");
    println!("  terminal sessions: {terminal_count}");

    if let Some(ignored) = stored
        .snapshot
        .pointer("/capture_options/ignored_applications")
        .and_then(Value::as_array)
        .filter(|values| !values.is_empty())
    {
        println!("  ignored applications: {}", ignored.len());
        for application in ignored.iter().filter_map(Value::as_str) {
            println!("    - {application}");
        }
    }

    match stored.docker() {
        Ok(docker) => {
            println!("  running containers: {}", docker.running_container_count());
            println!("  compose projects: {}", docker.compose_projects.len());
        }
        Err(error) => println!("  Docker metadata: {error}"),
    }

    println!("  use 'capsule show {name} --json' for the complete stored snapshot");
}

fn parse_save_arguments(arguments: Vec<String>) -> Result<SaveArguments, String> {
    let mut name = None;
    let mut force = false;
    let mut ignored_applications = Vec::new();
    let mut index = 0;

    while index < arguments.len() {
        let argument = &arguments[index];
        match argument.as_str() {
            "--force" | "-f" => force = true,
            "--ignore-app" => {
                index += 1;
                let Some(selector) = arguments.get(index) else {
                    return Err("--ignore-app requires an application name, executable, path, or AUMID".to_owned());
                };
                if selector.trim().is_empty() || selector.starts_with('-') {
                    return Err("--ignore-app requires an application name, executable, path, or AUMID".to_owned());
                }
                ignored_applications.push(selector.clone());
            }
            value if value.starts_with("--ignore-app=") => {
                let selector = value.trim_start_matches("--ignore-app=").trim();
                if selector.is_empty() {
                    return Err("--ignore-app requires a non-empty selector".to_owned());
                }
                ignored_applications.push(selector.to_owned());
            }
            value if value.starts_with('-') => {
                return Err(format!("unknown save option '{value}'"));
            }
            value if name.is_none() => name = Some(value.to_owned()),
            value => return Err(format!("unexpected save argument '{value}'")),
        }
        index += 1;
    }

    name.map(|name| SaveArguments {
        name,
        force,
        ignored_applications,
    })
    .ok_or_else(|| {
        "usage: capsule save <name> [--force] [--ignore-app <application>]...".to_owned()
    })
}

fn parse_restore_arguments(arguments: Vec<String>) -> Result<RestoreArguments, String> {
    let mut name = None;
    let mut dry_run = false;
    let mut mode = RestoreMode::Append;
    let mut explicit_mode: Option<RestoreMode> = None;

    for argument in arguments {
        match argument.as_str() {
            "--dry-run" => dry_run = true,
            "--append" => {
                if explicit_mode == Some(RestoreMode::Replace) {
                    return Err("--append cannot be combined with --replace/--close-unrelated".to_owned());
                }
                mode = RestoreMode::Append;
                explicit_mode = Some(RestoreMode::Append);
            }
            "--replace" | "--close-unrelated" => {
                if explicit_mode == Some(RestoreMode::Append) {
                    return Err("--replace/--close-unrelated cannot be combined with --append".to_owned());
                }
                mode = RestoreMode::Replace;
                explicit_mode = Some(RestoreMode::Replace);
            }
            value if value.starts_with('-') => {
                return Err(format!("unknown restore option '{value}'"));
            }
            value if name.is_none() => name = Some(value.to_owned()),
            value => return Err(format!("unexpected restore argument '{value}'")),
        }
    }

    name.map(|name| RestoreArguments {
        name,
        dry_run,
        mode,
    })
    .ok_or_else(|| {
        "usage: capsule restore <name> [--dry-run] [--append | --replace]".to_owned()
    })
}

fn parse_show_arguments(arguments: Vec<String>) -> Result<(String, bool), String> {
    let mut name = None;
    let mut json = false;

    for argument in arguments {
        match argument.as_str() {
            "--json" => json = true,
            value if value.starts_with('-') => {
                return Err(format!("unknown show option '{value}'"));
            }
            value if name.is_none() => name = Some(value.to_owned()),
            value => return Err(format!("unexpected show argument '{value}'")),
        }
    }

    name.map(|name| (name, json))
        .ok_or_else(|| "usage: capsule show <name> [--json]".to_owned())
}

fn usage_error(error: String) -> ExitCode {
    eprintln!("error: {error}");
    ExitCode::from(2)
}

fn command_error(error: String) -> ExitCode {
    eprintln!("error: {error}");
    ExitCode::from(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_parser_requires_one_name_and_supports_force_and_repeated_ignores() {
        assert_eq!(
            parse_save_arguments(vec![
                "demo".to_owned(),
                "--force".to_owned(),
                "--ignore-app".to_owned(),
                "Zen".to_owned(),
                "--ignore-app=Code.exe".to_owned(),
            ])
            .unwrap(),
            SaveArguments {
                name: "demo".to_owned(),
                force: true,
                ignored_applications: vec!["Zen".to_owned(), "Code.exe".to_owned()],
            }
        );
        assert!(parse_save_arguments(Vec::new()).is_err());
        assert!(parse_save_arguments(vec!["a".to_owned(), "b".to_owned()]).is_err());
        assert!(parse_save_arguments(vec!["demo".to_owned(), "--ignore-app".to_owned()]).is_err());
    }

    #[test]
    fn restore_parser_defaults_to_append_and_supports_replace_and_dry_run() {
        assert_eq!(
            parse_restore_arguments(vec!["demo".to_owned(), "--dry-run".to_owned()]).unwrap(),
            RestoreArguments {
                name: "demo".to_owned(),
                dry_run: true,
                mode: RestoreMode::Append,
            }
        );
        assert_eq!(
            parse_restore_arguments(vec!["demo".to_owned(), "--replace".to_owned()]).unwrap(),
            RestoreArguments {
                name: "demo".to_owned(),
                dry_run: false,
                mode: RestoreMode::Replace,
            }
        );
        assert_eq!(
            parse_restore_arguments(vec!["demo".to_owned(), "--close-unrelated".to_owned()])
                .unwrap()
                .mode,
            RestoreMode::Replace
        );
        assert_eq!(
            parse_restore_arguments(vec!["demo".to_owned()]).unwrap().mode,
            RestoreMode::Append
        );
        assert!(parse_restore_arguments(Vec::new()).is_err());
        assert!(parse_restore_arguments(vec!["demo".to_owned(), "--bad".to_owned()]).is_err());
        assert!(parse_restore_arguments(vec!["one".to_owned(), "two".to_owned()]).is_err());
        assert!(
            parse_restore_arguments(vec![
                "demo".to_owned(),
                "--append".to_owned(),
                "--replace".to_owned(),
            ])
            .is_err()
        );
    }

    #[test]
    fn show_parser_supports_json() {
        assert_eq!(
            parse_show_arguments(vec!["demo".to_owned(), "--json".to_owned()]).unwrap(),
            ("demo".to_owned(), true)
        );
        assert!(parse_show_arguments(vec!["--bad".to_owned()]).is_err());
    }

    #[test]
    fn restart_plan_rendering_quotes_arguments_for_display() {
        let plan = RestartPlan {
            executable: "wt.exe".to_owned(),
            args: vec![
                "new-tab".to_owned(),
                "-d".to_owned(),
                "C:\\Work Space".to_owned(),
            ],
            working_directory: None,
            note: None,
        };
        assert_eq!(
            render_restart_plan(&plan),
            "wt.exe new-tab -d \"C:\\Work Space\""
        );
    }
}
