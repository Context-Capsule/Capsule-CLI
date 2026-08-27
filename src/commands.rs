mod selective_restore;

use self::selective_restore::RestoreSelection;
use crate::{
    adapters::{
        docker::{self, DockerSnapshot, DockerStatus},
        terminal::{self, RestartPlan, TerminalEnvironment, TerminalSnapshot, TerminalStatus},
    },
    continuation_notes,
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
    message: Option<String>,
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
    only: Option<RestoreSelection>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NoteArguments {
    name: String,
    message: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RestoreCompletion {
    Complete,
    SuccessfulWithWarnings,
}

pub fn save(arguments: Vec<String>) -> ExitCode {
    let parsed = match parse_save_arguments(arguments) {
        Ok(parsed) => parsed,
        Err(error) => return usage_error(error),
    };
    let SaveArguments {
        name,
        force,
        ignored_applications,
        message,
    } = parsed;

    println!("Discovering workspace for capsule '{name}'...");
    let discovery = match discovery::discover(true, true, true, true) {
        Ok(snapshot) => snapshot,
        Err(error) => return command_error(format!("discovery failed: {error}")),
    };
    let ignored_names = match snapshot::validate_ignored_applications(
        &discovery,
        &ignored_applications,
    ) {
        Ok(names) => names,
        Err(error) => return usage_error(error),
    };
    let capture_options = CaptureOptions {
        ignored_applications,
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

    let summary = match store.save(&name, &stored, force) {
        Ok(summary) => summary,
        Err(error) => return command_error(error.to_string()),
    };

    if let Some(message) = message.as_deref() {
        let reference = format!("{}@{}", summary.name, summary.current_revision);
        if let Err(error) = continuation_notes::set(&reference, message) {
            return command_error(format!(
                "capsule '{}' was saved as revision {}, but its continuation note could not be stored: {error}",
                summary.name, summary.current_revision
            ));
        }
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
    if let Some(message) = message.as_deref() {
        print_indented_message("continuation note", message, "  ");
    }
    println!("  database: {database_path}");

    // Docker is optional. A save should not look unhealthy just because Docker
    // is not installed or its daemon is not running. Explicit Docker inspection
    // and `capsule doctor` still report Docker availability when requested.
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
    let continuation_note = match continuation_notes::get(&name) {
        Ok(note) => note,
        Err(error) => {
            eprintln!("warning: continuation note could not be read: {error}");
            None
        }
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
    if let Some(selection) = parsed.only.as_ref() {
        println!("  only: {}", selection.display());
    }
    if let Some(note) = continuation_note.as_ref() {
        print_indented_message("Last note", &note.message, "  ");
    }

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

    let selected_snapshot = parsed
        .only
        .as_ref()
        .map(|selection| selective_restore::filter_snapshot(&stored.snapshot, selection));
    let restore_snapshot = selected_snapshot.as_ref().unwrap_or(&stored.snapshot);

    // The restore engine already isolates subsystem failures and continues with
    // the remaining resources. Selective restore supplies a filtered in-memory
    // snapshot, leaving the persisted capsule untouched and reusing the same
    // proven restore stages for the selected resources.
    let report = restore::restore_snapshot(
        restore_snapshot,
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

    match classify_restore_completion(&report) {
        RestoreCompletion::Complete => {
            if parsed.dry_run {
                println!("Dry run complete; no applications or windows were changed.");
            } else {
                println!("Restore pass complete.");
            }
        }
        RestoreCompletion::SuccessfulWithWarnings => {
            let failures = restore_failure_count(&report);
            if parsed.dry_run {
                println!(
                    "Dry run complete (successful-with-warnings); {failures} resource issue(s) were isolated and did not prevent the remaining restore plan from being evaluated."
                );
            } else {
                println!(
                    "Restore pass complete (successful-with-warnings); {failures} resource issue(s) were isolated and other resources were restored where possible."
                );
            }
        }
    }
    ExitCode::SUCCESS
}

pub fn note(arguments: Vec<String>) -> ExitCode {
    let parsed = match parse_note_arguments(arguments) {
        Ok(parsed) => parsed,
        Err(error) => return usage_error(error),
    };

    // Open the primary store first so legacy databases receive their normal
    // revision migration before the additive continuation-note table resolves
    // a revision reference.
    let store = match CapsuleStore::open_default() {
        Ok(store) => store,
        Err(error) => return command_error(error.to_string()),
    };
    if let Err(error) = store.load(&parsed.name) {
        return command_error(error.to_string());
    }

    match parsed.message.as_deref() {
        Some(message) => match continuation_notes::set(&parsed.name, message) {
            Ok(note) => {
                println!(
                    "Saved continuation note for '{}@{}'.",
                    note.capsule_name, note.revision
                );
                print_indented_message("note", &note.message, "  ");
                ExitCode::SUCCESS
            }
            Err(error) => command_error(error.to_string()),
        },
        None => match continuation_notes::get(&parsed.name) {
            Ok(Some(note)) => {
                println!(
                    "Continuation note for '{}@{}':",
                    note.capsule_name, note.revision
                );
                print_message_block(&note.message, "  ");
                ExitCode::SUCCESS
            }
            Ok(None) => {
                println!("No continuation note is saved for '{}'.", parsed.name);
                ExitCode::SUCCESS
            }
            Err(error) => command_error(error.to_string()),
        },
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
    match continuation_notes::get(&name) {
        Ok(Some(note)) => print_indented_message("Last note", &note.message, "  "),
        Ok(None) => {}
        Err(error) => println!("  warning: continuation note could not be read: {error}"),
    }
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

fn classify_restore_completion(report: &restore::RestoreReport) -> RestoreCompletion {
    if report.success() {
        RestoreCompletion::Complete
    } else {
        RestoreCompletion::SuccessfulWithWarnings
    }
}

fn restore_failure_count(report: &restore::RestoreReport) -> usize {
    report.failures.len()
        + report.desktop.failures.len()
        + report.desktop.applications_failed
}

fn print_indented_message(label: &str, message: &str, indent: &str) {
    let mut lines = message.lines();
    if let Some(first) = lines.next() {
        println!("{indent}{label}: {first}");
    }
    let continuation_indent = " ".repeat(label.chars().count() + 2);
    for line in lines {
        println!("{indent}{continuation_indent}{line}");
    }
}

fn print_message_block(message: &str, indent: &str) {
    for line in message.lines() {
        println!("{indent}{line}");
    }
}

fn parse_save_arguments(arguments: Vec<String>) -> Result<SaveArguments, String> {
    let mut name = None;
    let mut force = false;
    let mut ignored_applications = Vec::new();
    let mut message = None;
    let mut index = 0;

    while index < arguments.len() {
        let argument = &arguments[index];
        match argument.as_str() {
            "--force" | "-f" => force = true,
            "-m" | "--message" => {
                index += 1;
                let Some(value) = arguments.get(index) else {
                    return Err("-m/--message requires a continuation note".to_owned());
                };
                set_message(&mut message, value)?;
            }
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
            value if value.starts_with("--message=") => {
                set_message(&mut message, value.trim_start_matches("--message="))?;
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
        message,
    })
    .ok_or_else(|| {
        "usage: capsule save <name> [-m <note>] [--force] [--ignore-app <application>]..."
            .to_owned()
    })
}

fn parse_restore_arguments(arguments: Vec<String>) -> Result<RestoreArguments, String> {
    let mut name = None;
    let mut dry_run = false;
    let mut mode = RestoreMode::Append;
    let mut explicit_mode: Option<RestoreMode> = None;
    let mut only = None;
    let mut index = 0;

    while index < arguments.len() {
        let argument = &arguments[index];
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
            "--only" => {
                index += 1;
                let Some(value) = arguments.get(index) else {
                    return Err("--only requires a restore target".to_owned());
                };
                if value.starts_with('-') {
                    return Err("--only requires a restore target".to_owned());
                }
                add_restore_selection(&mut only, value)?;
            }
            value if value.starts_with("--only=") => {
                add_restore_selection(&mut only, value.trim_start_matches("--only="))?;
            }
            value if value.starts_with('-') => {
                return Err(format!("unknown restore option '{value}'"));
            }
            value if name.is_none() => name = Some(value.to_owned()),
            value => return Err(format!("unexpected restore argument '{value}'")),
        }
        index += 1;
    }

    if only.is_some() && mode == RestoreMode::Replace {
        return Err(
            "--only cannot be combined with --replace/--close-unrelated because selective restore must leave unselected applications untouched"
                .to_owned(),
        );
    }

    name.map(|name| RestoreArguments {
        name,
        dry_run,
        mode,
        only,
    })
    .ok_or_else(|| {
        "usage: capsule restore <name> [--dry-run] [--append | --replace] [--only <targets>]"
            .to_owned()
    })
}

fn add_restore_selection(
    target: &mut Option<RestoreSelection>,
    value: &str,
) -> Result<(), String> {
    target
        .get_or_insert_with(RestoreSelection::default)
        .add_selector_list(value)
}

fn parse_note_arguments(arguments: Vec<String>) -> Result<NoteArguments, String> {
    let mut name = None;
    let mut message = None;
    let mut index = 0;

    while index < arguments.len() {
        let argument = &arguments[index];
        match argument.as_str() {
            "-m" | "--message" => {
                index += 1;
                let Some(value) = arguments.get(index) else {
                    return Err("-m/--message requires a continuation note".to_owned());
                };
                set_message(&mut message, value)?;
            }
            value if value.starts_with("--message=") => {
                set_message(&mut message, value.trim_start_matches("--message="))?;
            }
            value if value.starts_with('-') => {
                return Err(format!("unknown note option '{value}'"));
            }
            value if name.is_none() => name = Some(value.to_owned()),
            value => return Err(format!("unexpected note argument '{value}'")),
        }
        index += 1;
    }

    name.map(|name| NoteArguments { name, message })
        .ok_or_else(|| "usage: capsule note <name[@revision]> [-m <note>]".to_owned())
}

fn set_message(target: &mut Option<String>, value: &str) -> Result<(), String> {
    if target.is_some() {
        return Err("continuation note may be specified only once".to_owned());
    }
    let value = value.trim();
    if value.is_empty() {
        return Err("continuation note cannot be empty".to_owned());
    }
    *target = Some(value.to_owned());
    Ok(())
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
    fn save_parser_requires_one_name_and_supports_force_repeated_ignores_and_message() {
        assert_eq!(
            parse_save_arguments(vec![
                "demo".to_owned(),
                "--force".to_owned(),
                "-m".to_owned(),
                "Fix the next failing integration test".to_owned(),
                "--ignore-app".to_owned(),
                "Zen".to_owned(),
                "--ignore-app=Code.exe".to_owned(),
            ])
            .unwrap(),
            SaveArguments {
                name: "demo".to_owned(),
                force: true,
                ignored_applications: vec!["Zen".to_owned(), "Code.exe".to_owned()],
                message: Some("Fix the next failing integration test".to_owned()),
            }
        );
        assert!(parse_save_arguments(Vec::new()).is_err());
        assert!(parse_save_arguments(vec!["a".to_owned(), "b".to_owned()]).is_err());
        assert!(parse_save_arguments(vec!["demo".to_owned(), "--ignore-app".to_owned()]).is_err());
        assert!(parse_save_arguments(vec!["demo".to_owned(), "-m".to_owned()]).is_err());
        assert!(
            parse_save_arguments(vec![
                "demo".to_owned(),
                "-m".to_owned(),
                "one".to_owned(),
                "--message=two".to_owned(),
            ])
            .is_err()
        );
    }

    #[test]
    fn restore_parser_defaults_to_append_and_supports_replace_and_dry_run() {
        assert_eq!(
            parse_restore_arguments(vec!["demo".to_owned(), "--dry-run".to_owned()]).unwrap(),
            RestoreArguments {
                name: "demo".to_owned(),
                dry_run: true,
                mode: RestoreMode::Append,
                only: None,
            }
        );
        assert_eq!(
            parse_restore_arguments(vec!["demo".to_owned(), "--replace".to_owned()]).unwrap(),
            RestoreArguments {
                name: "demo".to_owned(),
                dry_run: false,
                mode: RestoreMode::Replace,
                only: None,
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
    fn restore_parser_supports_comma_separated_and_repeated_only_targets() {
        let parsed = parse_restore_arguments(vec![
            "demo@2".to_owned(),
            "--dry-run".to_owned(),
            "--only".to_owned(),
            "vscode,terminals".to_owned(),
            "--only=git".to_owned(),
        ])
        .unwrap();
        assert_eq!(parsed.name, "demo@2");
        assert!(parsed.dry_run);
        assert_eq!(parsed.mode, RestoreMode::Append);
        assert_eq!(parsed.only.unwrap().display(), "vscode,terminals,git");
    }

    #[test]
    fn restore_parser_rejects_invalid_only_and_replace_combination() {
        assert!(
            parse_restore_arguments(vec!["demo".to_owned(), "--only".to_owned()]).is_err()
        );
        assert!(
            parse_restore_arguments(vec![
                "demo".to_owned(),
                "--only".to_owned(),
                "vscode,unknown".to_owned(),
            ])
            .is_err()
        );
        assert!(
            parse_restore_arguments(vec![
                "demo".to_owned(),
                "--only".to_owned(),
                "vscode".to_owned(),
                "--replace".to_owned(),
            ])
            .is_err()
        );
        assert!(
            parse_restore_arguments(vec![
                "demo".to_owned(),
                "--replace".to_owned(),
                "--only=git".to_owned(),
            ])
            .is_err()
        );
    }

    #[test]
    fn explicit_append_can_be_combined_with_only() {
        let parsed = parse_restore_arguments(vec![
            "demo".to_owned(),
            "--append".to_owned(),
            "--only=browsers,git".to_owned(),
        ])
        .unwrap();
        assert_eq!(parsed.mode, RestoreMode::Append);
        assert_eq!(parsed.only.unwrap().display(), "firefox,chrome,git");
    }

    #[test]
    fn resource_failures_are_classified_as_successful_with_warnings() {
        let mut report = restore::RestoreReport::default();
        assert_eq!(
            classify_restore_completion(&report),
            RestoreCompletion::Complete
        );

        report.failures.push("Docker adapter unavailable".to_owned());
        assert_eq!(
            classify_restore_completion(&report),
            RestoreCompletion::SuccessfulWithWarnings
        );
        assert_eq!(restore_failure_count(&report), 1);

        report.desktop.applications_failed = 2;
        assert_eq!(restore_failure_count(&report), 3);
    }

    #[test]
    fn note_parser_supports_read_and_message_modes() {
        assert_eq!(
            parse_note_arguments(vec!["demo@2".to_owned()]).unwrap(),
            NoteArguments {
                name: "demo@2".to_owned(),
                message: None,
            }
        );
        assert_eq!(
            parse_note_arguments(vec![
                "demo".to_owned(),
                "--message=continue with auth tests".to_owned(),
            ])
            .unwrap(),
            NoteArguments {
                name: "demo".to_owned(),
                message: Some("continue with auth tests".to_owned()),
            }
        );
        assert!(parse_note_arguments(vec!["demo".to_owned(), "-m".to_owned()]).is_err());
        assert!(parse_note_arguments(vec!["--bad".to_owned()]).is_err());
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
