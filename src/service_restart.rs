#[path = "terminal_context.rs"]
mod terminal_context;

use crate::{
    adapters::terminal::{self, TerminalEnvironment, TerminalHost, TerminalSession},
    commands, persistence,
};
use context_capsule::{
    restore_bus,
    service_policy::{
        CALLER_PID_ENV, RestartPolicy, RestoreDecisionFile, RestoreDecisionKind, SavedService,
        ServicePlan, ServiceSource, SERVICE_DECISIONS_ENV, combined_command,
        validate_restart_command,
    },
    terminal_interrupt, vscode,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    env, fs,
    path::PathBuf,
    process::ExitCode,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const VSCODE_CONTROL_TIMEOUT: Duration = Duration::from_secs(12);
const VSCODE_START_TIMEOUT: Duration = Duration::from_secs(25);
const INTERRUPT_SETTLE_TIMEOUT: Duration = Duration::from_secs(6);
const INTERRUPT_POLL: Duration = Duration::from_millis(150);

#[derive(Debug, Clone)]
struct CapturedService {
    source: ServiceSource,
    host: String,
    shell: String,
    captured_terminal_pid: Option<u32>,
    vscode_terminal_index: Option<u32>,
    terminal_name: Option<String>,
    profile: Option<String>,
    working_directory: Option<String>,
    command: String,
}

#[derive(Debug, Deserialize)]
struct VsCodeInterruptedService {
    terminal_index: u32,
    #[serde(default)]
    terminal_name: Option<String>,
    #[serde(default)]
    shell_path: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    command: String,
}

#[derive(Debug, Clone)]
struct RestoreScope {
    reference: String,
    dry_run: bool,
    include_external: bool,
    include_vscode: bool,
}

struct ServiceStore {
    connection: Connection,
}

impl ServiceStore {
    fn open_default() -> Result<Self, String> {
        let path = persistence::default_database_path().map_err(|error| error.to_string())?;
        if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
            fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        let connection = Connection::open(path).map_err(|error| format!("SQLite error: {error}"))?;
        connection
            .busy_timeout(Duration::from_secs(5))
            .map_err(|error| format!("SQLite error: {error}"))?;
        connection
            .execute_batch(
                "PRAGMA foreign_keys = ON;\n\
                 CREATE TABLE IF NOT EXISTS capsule_terminal_services (\n\
                    capsule_id INTEGER NOT NULL,\n\
                    revision INTEGER NOT NULL,\n\
                    service_index INTEGER NOT NULL,\n\
                    source TEXT NOT NULL,\n\
                    host TEXT NOT NULL,\n\
                    shell TEXT NOT NULL,\n\
                    captured_terminal_pid INTEGER,\n\
                    vscode_terminal_index INTEGER,\n\
                    terminal_name TEXT,\n\
                    profile TEXT,\n\
                    working_directory TEXT,\n\
                    command TEXT NOT NULL,\n\
                    pre_start_command TEXT,\n\
                    restart_policy TEXT NOT NULL DEFAULT 'ask',\n\
                    updated_at_unix_ms INTEGER NOT NULL,\n\
                    PRIMARY KEY(capsule_id, revision, service_index),\n\
                    FOREIGN KEY(capsule_id) REFERENCES capsules(id) ON DELETE CASCADE\n\
                 );\n\
                 CREATE INDEX IF NOT EXISTS idx_capsule_terminal_services_revision\n\
                    ON capsule_terminal_services(capsule_id, revision, service_index);",
            )
            .map_err(|error| format!("SQLite error: {error}"))?;
        Ok(Self { connection })
    }

    fn resolve(&self, reference: &str) -> Result<(i64, String, u32), String> {
        let parsed = persistence::parse_capsule_reference(reference).map_err(|error| error.to_string())?;
        let row = self
            .connection
            .query_row(
                "SELECT id, name FROM capsules WHERE name = ?1 COLLATE NOCASE",
                [parsed.name.as_str()],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()
            .map_err(|error| format!("SQLite error: {error}"))?
            .ok_or_else(|| format!("capsule '{}' was not found", parsed.name))?;
        let revision = match parsed.revision {
            Some(revision) => {
                let exists = self
                    .connection
                    .query_row(
                        "SELECT 1 FROM capsule_revisions WHERE capsule_id = ?1 AND revision = ?2",
                        params![row.0, revision],
                        |_| Ok(()),
                    )
                    .optional()
                    .map_err(|error| format!("SQLite error: {error}"))?
                    .is_some();
                if !exists {
                    return Err(format!("capsule '{}@{revision}' was not found", row.1));
                }
                revision
            }
            None => self
                .connection
                .query_row(
                    "SELECT COALESCE(MAX(revision), 1) FROM capsule_revisions WHERE capsule_id = ?1",
                    [row.0],
                    |row| row.get::<_, u32>(0),
                )
                .map_err(|error| format!("SQLite error: {error}"))?,
        };
        Ok((row.0, row.1, revision))
    }

    fn list(&self, reference: &str) -> Result<ServicePlan, String> {
        let (capsule_id, capsule_name, revision) = self.resolve(reference)?;
        let mut statement = self
            .connection
            .prepare(
                "SELECT service_index, source, host, shell, captured_terminal_pid,\n\
                        vscode_terminal_index, terminal_name, profile, working_directory,\n\
                        command, pre_start_command, restart_policy\n\
                 FROM capsule_terminal_services\n\
                 WHERE capsule_id = ?1 AND revision = ?2\n\
                 ORDER BY service_index ASC",
            )
            .map_err(|error| format!("SQLite error: {error}"))?;
        let rows = statement
            .query_map(params![capsule_id, revision], |row| {
                let source: String = row.get(1)?;
                let policy: String = row.get(11)?;
                Ok(SavedService {
                    service_index: row.get(0)?,
                    source: if source == "visual-studio-code" {
                        ServiceSource::VisualStudioCode
                    } else {
                        ServiceSource::ExternalTerminal
                    },
                    host: row.get(2)?,
                    shell: row.get(3)?,
                    captured_terminal_pid: row.get(4)?,
                    vscode_terminal_index: row.get(5)?,
                    terminal_name: row.get(6)?,
                    profile: row.get(7)?,
                    working_directory: row.get(8)?,
                    command: row.get(9)?,
                    pre_start_command: row.get(10)?,
                    restart_policy: if policy == "always" {
                        RestartPolicy::Always
                    } else {
                        RestartPolicy::Ask
                    },
                })
            })
            .map_err(|error| format!("SQLite error: {error}"))?;
        let services = rows
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("SQLite error: {error}"))?;
        Ok(ServicePlan {
            capsule_name,
            revision,
            services,
        })
    }

    fn replace_current(&mut self, name: &str, mut services: Vec<SavedService>) -> Result<ServicePlan, String> {
        let (capsule_id, capsule_name, revision) = self.resolve(name)?;
        let transaction = self
            .connection
            .transaction()
            .map_err(|error| format!("SQLite error: {error}"))?;
        transaction
            .execute(
                "DELETE FROM capsule_terminal_services WHERE capsule_id = ?1 AND revision = ?2",
                params![capsule_id, revision],
            )
            .map_err(|error| format!("SQLite error: {error}"))?;
        let now = now_unix_ms();
        for (offset, service) in services.iter_mut().enumerate() {
            service.service_index = (offset + 1) as u32;
            transaction
                .execute(
                    "INSERT INTO capsule_terminal_services\n\
                     (capsule_id, revision, service_index, source, host, shell,\n\
                      captured_terminal_pid, vscode_terminal_index, terminal_name, profile,\n\
                      working_directory, command, pre_start_command, restart_policy, updated_at_unix_ms)\n\
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
                    params![
                        capsule_id,
                        revision,
                        service.service_index,
                        service.source.as_str(),
                        service.host,
                        service.shell,
                        service.captured_terminal_pid,
                        service.vscode_terminal_index,
                        service.terminal_name,
                        service.profile,
                        service.working_directory,
                        service.command,
                        service.pre_start_command,
                        service.restart_policy.as_str(),
                        now,
                    ],
                )
                .map_err(|error| format!("SQLite error: {error}"))?;
        }
        transaction
            .commit()
            .map_err(|error| format!("SQLite error: {error}"))?;
        Ok(ServicePlan {
            capsule_name,
            revision,
            services,
        })
    }

    fn set_pre_start(
        &mut self,
        reference: &str,
        service_index: u32,
        command: Option<&str>,
    ) -> Result<SavedService, String> {
        let (capsule_id, _, revision) = self.resolve(reference)?;
        let command = command.map(validate_restart_command).transpose()?;
        let changed = self
            .connection
            .execute(
                "UPDATE capsule_terminal_services\n\
                 SET pre_start_command = ?1, updated_at_unix_ms = ?2\n\
                 WHERE capsule_id = ?3 AND revision = ?4 AND service_index = ?5",
                params![command, now_unix_ms(), capsule_id, revision, service_index],
            )
            .map_err(|error| format!("SQLite error: {error}"))?;
        if changed == 0 {
            return Err(format!("saved service #{service_index} was not found for '{reference}'"));
        }
        self.list(reference)?
            .services
            .into_iter()
            .find(|service| service.service_index == service_index)
            .ok_or_else(|| format!("saved service #{service_index} was not found for '{reference}'"))
    }

    fn set_policy(
        &mut self,
        reference: &str,
        service_index: u32,
        policy: RestartPolicy,
    ) -> Result<(), String> {
        let (capsule_id, _, revision) = self.resolve(reference)?;
        let changed = self
            .connection
            .execute(
                "UPDATE capsule_terminal_services\n\
                 SET restart_policy = ?1, updated_at_unix_ms = ?2\n\
                 WHERE capsule_id = ?3 AND revision = ?4 AND service_index = ?5",
                params![policy.as_str(), now_unix_ms(), capsule_id, revision, service_index],
            )
            .map_err(|error| format!("SQLite error: {error}"))?;
        if changed == 0 {
            return Err(format!("saved service #{service_index} was not found for '{reference}'"));
        }
        Ok(())
    }
}

pub fn save(arguments: Vec<String>) -> ExitCode {
    let (clean_arguments, cli_force, name) = strip_cli_force(arguments);
    if !cli_force {
        return commands::save(clean_arguments);
    }
    let Some(name) = name else {
        return commands::save(clean_arguments);
    };

    println!("--cli-force: capturing running terminal commands before workspace discovery...");
    let captured = match capture_and_interrupt_services() {
        Ok(captured) => captured,
        Err(error) => {
            eprintln!("error: --cli-force: {error}");
            return ExitCode::from(1);
        }
    };

    let result = commands::save(clean_arguments);
    if result != ExitCode::SUCCESS {
        if !captured.is_empty() {
            eprintln!(
                "warning: {} terminal service(s) were interrupted before the save failed; Context Capsule did not create restart-policy metadata for an unsuccessful save",
                captured.len()
            );
        }
        return result;
    }

    let store = match persistence::CapsuleStore::open_default() {
        Ok(store) => store,
        Err(error) => {
            eprintln!("error: capsule was saved, but terminal restart metadata could not be attached: {error}");
            return ExitCode::from(1);
        }
    };
    let stored = match store.load(&name) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            eprintln!("error: capsule was saved, but terminal restart metadata could not be attached: {error}");
            return ExitCode::from(1);
        }
    };
    let services = match finalize_captured_services(captured, &stored.snapshot) {
        Ok(services) => services,
        Err(error) => {
            eprintln!("error: capsule was saved, but terminal restart metadata could not be attached safely: {error}");
            return ExitCode::from(1);
        }
    };
    let mut service_store = match ServiceStore::open_default() {
        Ok(store) => store,
        Err(error) => {
            eprintln!("error: capsule was saved, but terminal restart metadata could not be stored: {error}");
            return ExitCode::from(1);
        }
    };
    match service_store.replace_current(&name, services) {
        Ok(plan) => {
            println!(
                "  saved terminal restart commands: {} (capsule revision {}@{})",
                plan.services.len(), plan.capsule_name, plan.revision
            );
            for service in &plan.services {
                println!("    [{}] {}", service.service_index, service.command);
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("error: capsule was saved, but terminal restart metadata could not be stored: {error}");
            ExitCode::from(1)
        }
    }
}

pub fn restore(arguments: Vec<String>) -> ExitCode {
    let scope = parse_restore_scope(&arguments).ok();
    let plan = scope
        .as_ref()
        .and_then(|scope| ServiceStore::open_default().ok()?.list(&scope.reference).ok())
        .map(|plan| filter_plan(plan, scope.as_ref().expect("scope exists")));

    if let (Some(scope), Some(plan)) = (scope.as_ref(), plan.as_ref()) {
        if scope.dry_run && !plan.services.is_empty() {
            println!("Saved service restart policies:");
            for service in &plan.services {
                println!(
                    "  [{}] {} — {}",
                    service.service_index,
                    service.command,
                    match service.restart_policy {
                        RestartPolicy::Ask => "would ask before starting",
                        RestartPolicy::Always => "configured to start automatically",
                    }
                );
                if let Some(pre_start) = service.pre_start_command.as_deref() {
                    println!("      pre-start: {pre_start}");
                }
            }
        }
    }

    let result = commands::restore(arguments.clone());
    if result != ExitCode::SUCCESS {
        return result;
    }
    let Some(scope) = scope else {
        return result;
    };
    if scope.dry_run {
        return result;
    }
    let Some(plan) = plan else {
        return result;
    };
    if plan.services.is_empty() {
        return result;
    }

    let decisions = load_decisions(&plan);
    let mut service_store = match ServiceStore::open_default() {
        Ok(store) => store,
        Err(error) => {
            eprintln!("  warning: saved services were not started because restart policies could not be opened: {error}");
            return result;
        }
    };
    let mut selected = Vec::new();
    for service in plan.services {
        let decision = decisions
            .iter()
            .find(|decision| decision.0 == service.service_index)
            .map(|decision| decision.1)
            .unwrap_or_else(|| match service.restart_policy {
                RestartPolicy::Always => RestoreDecisionKind::StartOnce,
                RestartPolicy::Ask => RestoreDecisionKind::Skip,
            });
        match decision {
            RestoreDecisionKind::Skip => {}
            RestoreDecisionKind::StartOnce => selected.push(service),
            RestoreDecisionKind::Always => {
                if let Err(error) = service_store.set_policy(
                    &format!("{}@{}", plan.capsule_name, plan.revision),
                    service.service_index,
                    RestartPolicy::Always,
                ) {
                    eprintln!("  warning: could not persist Always policy for service #{}: {error}", service.service_index);
                }
                selected.push(service);
            }
        }
    }

    if selected.is_empty() {
        println!("Saved services: none started.");
        return result;
    }

    println!("Starting approved saved services...");
    let mut external = Vec::new();
    let mut vscode_services = Vec::new();
    for service in selected {
        match service.source {
            ServiceSource::ExternalTerminal => external.push(service),
            ServiceSource::VisualStudioCode => vscode_services.push(service),
        }
    }

    let mut started = 0usize;
    let mut failed = 0usize;
    for service in &external {
        match start_external_service(service) {
            Ok(()) => {
                started += 1;
                println!("  started [{}] {}", service.service_index, service.command);
            }
            Err(error) => {
                failed += 1;
                eprintln!("  failed [{}] {}: {error}", service.service_index, service.command);
            }
        }
    }
    if !vscode_services.is_empty() {
        match start_vscode_services(&scope.reference, &vscode_services) {
            Ok((changed, warnings)) => {
                started += changed;
                for warning in warnings {
                    println!("  warning: VS Code service restart: {warning}");
                }
            }
            Err(error) => {
                failed += vscode_services.len();
                eprintln!("  failed: VS Code saved service restart: {error}");
            }
        }
    }
    println!("Saved services: {started} started, {failed} failed.");
    result
}

pub fn plan(arguments: Vec<String>) -> ExitCode {
    let restore_arguments = if arguments.first().map(String::as_str) == Some("restore") {
        &arguments[1..]
    } else {
        &arguments[..]
    };
    let scope = match parse_restore_scope(restore_arguments) {
        Ok(scope) => scope,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(2);
        }
    };
    let store = match ServiceStore::open_default() {
        Ok(store) => store,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(1);
        }
    };
    let plan = match store.list(&scope.reference) {
        Ok(plan) => filter_plan(plan, &scope),
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(1);
        }
    };
    match serde_json::to_string(&plan) {
        Ok(json) => {
            println!("{json}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("could not serialize service restart plan: {error}");
            ExitCode::from(1)
        }
    }
}

pub fn command(arguments: Vec<String>) -> ExitCode {
    match arguments.as_slice() {
        [action, reference] if action == "list" => match ServiceStore::open_default()
            .and_then(|store| store.list(reference))
        {
            Ok(plan) => {
                print_plan(&plan);
                ExitCode::SUCCESS
            }
            Err(error) => command_error(error),
        },
        [action, reference, index, flag] if action == "prestart" && flag == "--clear" => {
            let index = match service_index(index) {
                Ok(index) => index,
                Err(error) => return command_error(error),
            };
            let mut store = match ServiceStore::open_default() {
                Ok(store) => store,
                Err(error) => return command_error(error),
            };
            match store.set_pre_start(reference, index, None) {
                Ok(_) => {
                    println!("Cleared pre-start command for '{reference}' service #{index}.");
                    ExitCode::SUCCESS
                }
                Err(error) => command_error(error),
            }
        }
        [action, reference, index, flag, value]
            if action == "prestart" && matches!(flag.as_str(), "-c" | "--command") =>
        {
            let index = match service_index(index) {
                Ok(index) => index,
                Err(error) => return command_error(error),
            };
            let mut store = match ServiceStore::open_default() {
                Ok(store) => store,
                Err(error) => return command_error(error),
            };
            match store.set_pre_start(reference, index, Some(value)) {
                Ok(service) => {
                    println!("Saved pre-start command for '{reference}' service #{index}.");
                    if let Some(command) = service.pre_start_command {
                        println!("  pre-start: {command}");
                    }
                    println!("  service:   {}", service.command);
                    ExitCode::SUCCESS
                }
                Err(error) => command_error(error),
            }
        }
        [action, reference, index, policy] if action == "policy" => {
            let index = match service_index(index) {
                Ok(index) => index,
                Err(error) => return command_error(error),
            };
            let policy = match policy.to_ascii_lowercase().as_str() {
                "ask" => RestartPolicy::Ask,
                "always" => RestartPolicy::Always,
                _ => return command_error("policy must be 'ask' or 'always'".to_owned()),
            };
            let mut store = match ServiceStore::open_default() {
                Ok(store) => store,
                Err(error) => return command_error(error),
            };
            match store.set_policy(reference, index, policy) {
                Ok(()) => {
                    println!("Set '{reference}' service #{index} policy to {}.", policy.as_str());
                    ExitCode::SUCCESS
                }
                Err(error) => command_error(error),
            }
        }
        _ => {
            eprintln!("error: usage:");
            eprintln!("  capsule service list <name[@revision]>");
            eprintln!("  capsule service prestart <name[@revision]> <index> -c <command>");
            eprintln!("  capsule service prestart <name[@revision]> <index> --clear");
            eprintln!("  capsule service policy <name[@revision]> <index> <ask|always>");
            ExitCode::from(2)
        }
    }
}

fn capture_and_interrupt_services() -> Result<Vec<CapturedService>, String> {
    let caller_pid = env::var(CALLER_PID_ENV)
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|pid| *pid > 0);
    let caller_shell_pid = match caller_pid {
        Some(pid) => terminal_interrupt::caller_shell_pid(pid)?,
        None => None,
    };
    let initial = terminal::discover();

    // Windows process discovery can see nested/helper shells below Code.exe in
    // addition to actual integrated-terminal shells (especially in Extension
    // Development Hosts). Preserve the observed PIDs so the VS Code adapter can
    // reconcile them against vscode.window.terminals[*].processId by identity
    // instead of comparing an unreliable raw process count.
    let mut vscode_running_shell_pids = initial
        .sessions
        .iter()
        .filter(|session| session.host == TerminalHost::VisualStudioCode)
        .filter(|session| session.pid != caller_shell_pid)
        .filter(|session| session.foreground_command.is_some())
        .filter_map(|session| session.pid)
        .collect::<Vec<_>>();
    vscode_running_shell_pids.sort_unstable();
    vscode_running_shell_pids.dedup();

    let mut captured = Vec::new();
    if !vscode_running_shell_pids.is_empty() {
        let mut vscode =
            interrupt_vscode_services(caller_shell_pid, &vscode_running_shell_pids)?;
        captured.append(&mut vscode);
    }

    let candidates = initial
        .sessions
        .iter()
        .filter(|session| session.host != TerminalHost::VisualStudioCode)
        .filter(|session| session.host != TerminalHost::Cursor)
        .filter(|session| matches!(session.environment, TerminalEnvironment::Windows))
        .filter(|session| session.pid.is_some() && session.pid != caller_shell_pid)
        .filter_map(|session| {
            let command = session.foreground_command.as_deref()?;
            Some((session, command))
        })
        .map(|(session, command)| {
            Ok(CapturedService {
                source: ServiceSource::ExternalTerminal,
                host: wire_host(&session.host),
                shell: session.shell.as_str().to_owned(),
                captured_terminal_pid: session.pid,
                vscode_terminal_index: None,
                terminal_name: session.title.clone(),
                profile: session.profile.clone(),
                working_directory: session.working_directory.clone(),
                command: validate_restart_command(command)?,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;

    for candidate in candidates {
        let pid = candidate
            .captured_terminal_pid
            .ok_or_else(|| "terminal service did not have a shell PID".to_owned())?;
        terminal_interrupt::send_ctrl_c(pid).map_err(|error| {
            format!(
                "could not stop '{}' in terminal shell PID {pid}: {error}",
                candidate.command
            )
        })?;
        wait_until_shell_idle(pid)?;
        captured.push(candidate);
    }

    Ok(captured)
}

fn interrupt_vscode_services(
    caller_shell_pid: Option<u32>,
    observed_running_shell_pids: &[u32],
) -> Result<Vec<CapturedService>, String> {
    let editor = vscode::load_recent_vscode_state()
        .map_err(|error| format!("could not read live VS Code state: {error}"))?
        .ok_or_else(|| {
            "a VS Code terminal has a running command, but no fresh Context Capsule VS Code adapter state is available"
                .to_owned()
        })?;
    let request = restore_bus::write_request(
        "vscode",
        json!({
            "editor": editor,
            "terminal_control": {
                "action": "interrupt-running-services",
                "caller_shell_pid": caller_shell_pid,
                "observed_running_shell_pids": observed_running_shell_pids,
            }
        }),
    )
    .map_err(|error| format!("could not request VS Code terminal interruption: {error}"))?;
    let completion = restore_bus::wait_for_completion(
        "vscode",
        &request.request_id,
        VSCODE_CONTROL_TIMEOUT,
    )
    .map_err(|error| format!("VS Code terminal interruption wait failed: {error}"))?
    .ok_or_else(|| "VS Code terminal interruption timed out".to_owned())?;
    if !completion.ok {
        return Err(completion
            .error
            .unwrap_or_else(|| "VS Code rejected terminal interruption".to_owned()));
    }

    let result_path = restore_bus::completion_path("vscode")
        .map_err(|error| format!("could not read VS Code terminal interruption result: {error}"))?;
    let raw = fs::read_to_string(result_path)
        .map_err(|error| format!("could not read VS Code terminal interruption result: {error}"))?;
    let value: Value = serde_json::from_str(&raw)
        .map_err(|error| format!("invalid VS Code terminal interruption result: {error}"))?;
    let services = value
        .pointer("/data/services")
        .cloned()
        .unwrap_or_else(|| Value::Array(Vec::new()));
    let services: Vec<VsCodeInterruptedService> = serde_json::from_value(services)
        .map_err(|error| format!("invalid VS Code terminal service metadata: {error}"))?;
    services
        .into_iter()
        .map(|service| {
            Ok(CapturedService {
                source: ServiceSource::VisualStudioCode,
                host: "visual-studio-code".to_owned(),
                shell: shell_label(service.shell_path.as_deref()),
                captured_terminal_pid: None,
                vscode_terminal_index: Some(service.terminal_index),
                terminal_name: service.terminal_name,
                profile: None,
                working_directory: service.cwd,
                command: validate_restart_command(&service.command)?,
            })
        })
        .collect()
}

fn wait_until_shell_idle(pid: u32) -> Result<(), String> {
    let deadline = Instant::now() + INTERRUPT_SETTLE_TIMEOUT;
    loop {
        let current = terminal::discover();
        let Some(session) = current.sessions.iter().find(|session| session.pid == Some(pid)) else {
            return Err(format!(
                "terminal shell PID {pid} exited after Ctrl+C; save was stopped because its CWD can no longer be captured reliably"
            ));
        };
        if session.foreground_command.is_none() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "terminal shell PID {pid} still has foreground command '{}' after Ctrl+C",
                session.foreground_command.as_deref().unwrap_or("unknown")
            ));
        }
        thread::sleep(INTERRUPT_POLL);
    }
}

fn finalize_captured_services(
    captured: Vec<CapturedService>,
    snapshot: &Value,
) -> Result<Vec<SavedService>, String> {
    let terminal_sessions = snapshot
        .pointer("/terminals/sessions")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let vscode_terminals = snapshot
        .pointer("/editors/vscode/integratedTerminals")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut services = Vec::new();
    for captured in captured {
        let mut working_directory = captured.working_directory.clone();
        let mut terminal_name = captured.terminal_name.clone();
        match captured.source {
            ServiceSource::ExternalTerminal => {
                let pid = captured
                    .captured_terminal_pid
                    .ok_or_else(|| "external captured service had no terminal PID".to_owned())?;
                let saved = terminal_sessions
                    .iter()
                    .find(|session| session.get("pid").and_then(Value::as_u64) == Some(pid as u64))
                    .ok_or_else(|| {
                        format!(
                            "terminal shell PID {pid} was interrupted successfully but was not present in the saved terminal snapshot"
                        )
                    })?;
                if let Some(cwd) = saved.get("working_directory").and_then(Value::as_str) {
                    working_directory = Some(cwd.to_owned());
                }
                if let Some(title) = saved.get("title").and_then(Value::as_str) {
                    terminal_name = Some(title.to_owned());
                }
            }
            ServiceSource::VisualStudioCode => {
                let index = captured
                    .vscode_terminal_index
                    .ok_or_else(|| "VS Code captured service had no terminal index".to_owned())?
                    as usize;
                let saved = vscode_terminals.get(index).ok_or_else(|| {
                    format!(
                        "VS Code terminal index {} was interrupted successfully but was not present in the saved semantic snapshot",
                        index + 1
                    )
                })?;
                if let Some(cwd) = saved.get("cwd").and_then(Value::as_str) {
                    working_directory = Some(cwd.to_owned());
                }
                if let Some(name) = saved.get("name").and_then(Value::as_str) {
                    terminal_name = Some(name.to_owned());
                }
            }
        }
        services.push(SavedService {
            service_index: 0,
            source: captured.source,
            host: captured.host,
            shell: captured.shell,
            captured_terminal_pid: captured.captured_terminal_pid,
            vscode_terminal_index: captured.vscode_terminal_index,
            terminal_name,
            profile: captured.profile,
            working_directory,
            command: captured.command,
            pre_start_command: None,
            restart_policy: RestartPolicy::Ask,
        });
    }
    services.sort_by_key(|service| {
        (
            match service.source {
                ServiceSource::ExternalTerminal => 0_u8,
                ServiceSource::VisualStudioCode => 1_u8,
            },
            service.captured_terminal_pid.unwrap_or(u32::MAX),
            service.vscode_terminal_index.unwrap_or(u32::MAX),
        )
    });
    Ok(services)
}

fn start_external_service(service: &SavedService) -> Result<(), String> {
    let command = combined_command(service)?;
    let current = terminal_context::enrich_for_matching(&terminal::discover());
    let session = current
        .sessions
        .iter()
        .filter(|session| session.host != TerminalHost::VisualStudioCode)
        .find(|session| service_matches_terminal(service, session))
        .ok_or_else(|| {
            format!(
                "restored terminal for {:?} / {} was not observable",
                service.host, service.shell
            )
        })?;
    if let Some(foreground) = session.foreground_command.as_deref() {
        return Err(format!(
            "target terminal is not idle; it is already running '{foreground}'"
        ));
    }
    let pid = session
        .pid
        .ok_or_else(|| "restored terminal has no shell PID for command replay".to_owned())?;
    terminal_interrupt::send_text(pid, &command)
}

fn start_vscode_services(
    reference: &str,
    services: &[SavedService],
) -> Result<(usize, Vec<String>), String> {
    let capsule_store = persistence::CapsuleStore::open_default().map_err(|error| error.to_string())?;
    let stored = capsule_store.load(reference).map_err(|error| error.to_string())?;
    let editor = stored
        .snapshot
        .pointer("/editors/vscode")
        .cloned()
        .filter(|value| !value.is_null())
        .ok_or_else(|| "capsule has no semantic VS Code snapshot for saved service restart".to_owned())?;
    let request = restore_bus::write_request(
        "vscode",
        json!({
            "editor": editor,
            "terminal_service_start": {
                "services": services,
            }
        }),
    )
    .map_err(|error| format!("could not request VS Code saved service start: {error}"))?;
    let completion = restore_bus::wait_for_completion(
        "vscode",
        &request.request_id,
        VSCODE_START_TIMEOUT,
    )
    .map_err(|error| format!("VS Code saved service start wait failed: {error}"))?
    .ok_or_else(|| "VS Code saved service start timed out".to_owned())?;
    if completion.ok {
        Ok((completion.changed, completion.warnings))
    } else {
        Err(completion
            .error
            .unwrap_or_else(|| "VS Code rejected saved service start".to_owned()))
    }
}

fn service_matches_terminal(service: &SavedService, session: &TerminalSession) -> bool {
    if !service_host_matches(&service.host, &session.host)
        || !session.shell.as_str().eq_ignore_ascii_case(&service.shell)
    {
        return false;
    }
    if let Some(profile) = service.profile.as_deref() {
        if !session
            .profile
            .as_deref()
            .is_some_and(|value| value.eq_ignore_ascii_case(profile))
        {
            return false;
        }
    }
    if let Some(directory) = service.working_directory.as_deref() {
        if !session
            .working_directory
            .as_deref()
            .is_some_and(|value| paths_equivalent(value, directory))
        {
            return false;
        }
    }
    true
}

fn service_host_matches(saved_host: &str, current_host: &TerminalHost) -> bool {
    let saved = saved_host.trim().to_ascii_lowercase();
    let current = wire_host(current_host).to_ascii_lowercase();
    saved == current
        || matches!(
            (saved.as_str(), current.as_str()),
            ("unknown", "console-host") | ("console-host", "unknown")
        )
}

fn load_decisions(plan: &ServicePlan) -> Vec<(u32, RestoreDecisionKind)> {
    let Some(path) = env::var_os(SERVICE_DECISIONS_ENV).map(PathBuf::from) else {
        return Vec::new();
    };
    let file = match fs::read_to_string(path) {
        Ok(raw) => serde_json::from_str::<RestoreDecisionFile>(&raw).ok(),
        Err(_) => None,
    };
    let Some(file) = file else {
        return Vec::new();
    };
    if !file.capsule_name.eq_ignore_ascii_case(&plan.capsule_name) || file.revision != plan.revision {
        return Vec::new();
    }
    file.decisions
        .into_iter()
        .filter(|decision| {
            plan.services
                .iter()
                .any(|service| service.service_index == decision.service_index)
        })
        .map(|decision| (decision.service_index, decision.decision))
        .collect()
}

fn parse_restore_scope(arguments: &[String]) -> Result<RestoreScope, String> {
    let arguments = if arguments.first().map(String::as_str) == Some("restore") {
        &arguments[1..]
    } else {
        arguments
    };
    let mut reference = None;
    let mut dry_run = false;
    let mut only_values = Vec::new();
    let mut index = 0usize;
    while index < arguments.len() {
        let value = &arguments[index];
        match value.as_str() {
            "--dry-run" => dry_run = true,
            "--append" | "--replace" | "--close-unrelated" => {}
            "--only" => {
                index += 1;
                let Some(selector) = arguments.get(index) else {
                    return Err("--only requires a restore target".to_owned());
                };
                only_values.push(selector.clone());
            }
            value if value.starts_with("--only=") => {
                only_values.push(value.trim_start_matches("--only=").to_owned());
            }
            value if value.starts_with('-') => {
                return Err(format!("unknown restore option '{value}'"));
            }
            value if reference.is_none() => reference = Some(value.to_owned()),
            value => return Err(format!("unexpected restore argument '{value}'")),
        }
        index += 1;
    }
    let reference = reference.ok_or_else(|| "restore requires a capsule name".to_owned())?;
    let mut include_external = only_values.is_empty();
    let mut include_vscode = only_values.is_empty();
    for value in only_values {
        for selector in value.split(',').map(str::trim) {
            match selector.to_ascii_lowercase().as_str() {
                "terminal" | "terminals" | "all" => include_external = true,
                "vscode" | "vs-code" | "all" => include_vscode = true,
                "apps" | "app" | "applications" | "application" | "desktop" | "firefox"
                | "zen" | "chrome" | "browser" | "browsers" | "git" | "docker"
                | "containers" | "explorer" => {}
                other => return Err(format!("unknown restore target '{other}'")),
            }
        }
    }
    Ok(RestoreScope {
        reference,
        dry_run,
        include_external,
        include_vscode,
    })
}

fn filter_plan(mut plan: ServicePlan, scope: &RestoreScope) -> ServicePlan {
    plan.services.retain(|service| match service.source {
        ServiceSource::ExternalTerminal => scope.include_external,
        ServiceSource::VisualStudioCode => scope.include_vscode,
    });
    plan
}

fn strip_cli_force(arguments: Vec<String>) -> (Vec<String>, bool, Option<String>) {
    let mut clean = Vec::new();
    let mut cli_force = false;
    let mut name = None;
    let mut index = 0usize;
    while index < arguments.len() {
        let value = &arguments[index];
        if value == "--cli-force" {
            cli_force = true;
            index += 1;
            continue;
        }
        clean.push(value.clone());
        if matches!(value.as_str(), "-m" | "--message" | "--ignore-app") {
            index += 1;
            if let Some(next) = arguments.get(index) {
                clean.push(next.clone());
            }
        } else if !value.starts_with('-') && name.is_none() {
            name = Some(value.clone());
        }
        index += 1;
    }
    (clean, cli_force, name)
}

fn print_plan(plan: &ServicePlan) {
    println!("Saved terminal services for '{}@{}':", plan.capsule_name, plan.revision);
    if plan.services.is_empty() {
        println!("  none");
        return;
    }
    for service in &plan.services {
        println!(
            "  [{}] {} / {} — {}",
            service.service_index, service.host, service.shell, service.command
        );
        if let Some(directory) = service.working_directory.as_deref() {
            println!("      cwd:       {directory}");
        }
        if let Some(pre_start) = service.pre_start_command.as_deref() {
            println!("      pre-start: {pre_start}");
        }
        println!("      policy:    {}", service.restart_policy.as_str());
    }
}

fn service_index(value: &str) -> Result<u32, String> {
    value
        .parse::<u32>()
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| "service index must be a positive integer from 'capsule service list'".to_owned())
}

fn command_error(error: String) -> ExitCode {
    eprintln!("error: {error}");
    ExitCode::from(1)
}

fn wire_host(host: &TerminalHost) -> String {
    serde_json::to_value(host)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| format!("{host:?}").to_ascii_lowercase())
}

fn shell_label(shell_path: Option<&str>) -> String {
    let name = shell_path
        .and_then(|path| path.rsplit(['\\', '/']).next())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match name.as_str() {
        "pwsh" | "pwsh.exe" => "PowerShell".to_owned(),
        "powershell" | "powershell.exe" => "Windows PowerShell".to_owned(),
        "cmd" | "cmd.exe" => "Command Prompt".to_owned(),
        "bash" | "bash.exe" => "Bash".to_owned(),
        "zsh" | "zsh.exe" => "Zsh".to_owned(),
        "fish" | "fish.exe" => "Fish".to_owned(),
        _ => shell_path.unwrap_or("Unknown shell").to_owned(),
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

fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_force_is_removed_before_existing_save_parser_runs() {
        let (clean, force, name) = strip_cli_force(vec![
            "demo".to_owned(),
            "--cli-force".to_owned(),
            "-m".to_owned(),
            "continue".to_owned(),
        ]);
        assert!(force);
        assert_eq!(name.as_deref(), Some("demo"));
        assert_eq!(clean, vec!["demo", "-m", "continue"]);
    }

    #[test]
    fn selective_restore_scope_matches_existing_only_semantics() {
        let terminal = parse_restore_scope(&[
            "demo".to_owned(),
            "--only".to_owned(),
            "terminals".to_owned(),
        ])
        .unwrap();
        assert!(terminal.include_external);
        assert!(!terminal.include_vscode);

        let vscode = parse_restore_scope(&[
            "demo".to_owned(),
            "--only=vscode".to_owned(),
        ])
        .unwrap();
        assert!(!vscode.include_external);
        assert!(vscode.include_vscode);
    }

    #[test]
    fn path_matching_is_case_and_separator_insensitive_on_windows_style_paths() {
        assert!(paths_equivalent("C:/Work/TriUp/", "c:\\work\\triup"));
    }

    #[test]
    fn external_service_host_matching_accepts_only_unknown_console_host_alias() {
        assert!(service_host_matches("unknown", &TerminalHost::ConsoleHost));
        assert!(service_host_matches("console-host", &TerminalHost::Unknown));
        assert!(service_host_matches("unknown", &TerminalHost::Unknown));
        assert!(!service_host_matches("unknown", &TerminalHost::WindowsTerminal));
        assert!(!service_host_matches("console-host", &TerminalHost::VisualStudioCode));
    }
}
