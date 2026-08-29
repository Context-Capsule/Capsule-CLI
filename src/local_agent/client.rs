use crate::{
    local_agent::{
        AgentError, ipc, paths,
        protocol::{
            AgentAction, AgentRequest, AgentResponse, AgentState, CliInvocation, EnvironmentEntry,
            PROTOCOL_VERSION,
        },
    },
    logging,
    service_policy::{
        CALLER_PID_ENV, RestartPolicy, RestoreDecision, RestoreDecisionFile, RestoreDecisionKind,
        SERVICE_DECISIONS_ENV, ServicePlan,
    },
};
use std::{
    env, fs,
    fs::OpenOptions,
    io::{self, IsTerminal, Write},
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream},
    path::PathBuf,
    process::{Command, ExitCode, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(windows)]
use std::{
    io::{BufRead, BufReader},
    os::windows::process::CommandExt,
};

const SERVER_FLAG: &str = "--agent-serve";
const CONNECT_TIMEOUT: Duration = Duration::from_millis(300);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const SERVICE_PLAN_TIMEOUT: Duration = RESPONSE_TIMEOUT;
const START_ATTEMPTS: usize = 120;
const START_POLL: Duration = Duration::from_millis(25);
const EXISTING_START_ATTEMPTS: usize = 80;
const SERVICE_RESTART_LOG_COMPONENT: &str = "service-restart";
static REQUEST_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PromptInputKind {
    InheritedStdin,
    #[cfg(any(windows, test))]
    WindowsConsole,
}

impl PromptInputKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::InheritedStdin => "stdin",
            #[cfg(any(windows, test))]
            Self::WindowsConsole => "windows-console",
        }
    }
}

enum ServicePromptInput {
    InheritedStdin,
    #[cfg(windows)]
    WindowsConsole {
        reader: BufReader<fs::File>,
        writer: fs::File,
    },
}

impl ServicePromptInput {
    fn kind(&self) -> PromptInputKind {
        match self {
            Self::InheritedStdin => PromptInputKind::InheritedStdin,
            #[cfg(windows)]
            Self::WindowsConsole { .. } => PromptInputKind::WindowsConsole,
        }
    }

    fn read_line(&mut self, input: &mut String) -> io::Result<usize> {
        match self {
            Self::InheritedStdin => io::stdin().read_line(input),
            #[cfg(windows)]
            Self::WindowsConsole { reader, .. } => reader.read_line(input),
        }
    }

    fn write_text(&mut self, text: &str) -> io::Result<()> {
        match self {
            Self::InheritedStdin => {
                let mut stdout = io::stdout();
                stdout.write_all(text.as_bytes())?;
                stdout.flush()
            }
            #[cfg(windows)]
            Self::WindowsConsole { writer, .. } => {
                writer.write_all(text.as_bytes())?;
                writer.flush()
            }
        }
    }

    fn write_line(&mut self, text: &str) -> io::Result<()> {
        self.write_text(text)?;
        self.write_text("\r\n")
    }
}

fn open_service_prompt_input() -> Option<ServicePromptInput> {
    if io::stdin().is_terminal() {
        return Some(ServicePromptInput::InheritedStdin);
    }

    #[cfg(windows)]
    {
        // A CLI runner commonly redirects stdin/stdout so it can capture the
        // child's streams while leaving the child attached to the same Windows
        // console as the user. Standard-handle terminal detection is false in
        // that arrangement. CONIN$/CONOUT$ address the attached console directly
        // and therefore keep the prompt both readable and visible even if the
        // runner buffers redirected output. Require both handles: if there is no
        // visible attached console (GUI/extension/background invocation), fall
        // back to the existing noninteractive StartOnce behavior rather than
        // blocking on a prompt the user cannot see.
        let reader = OpenOptions::new().read(true).open("CONIN$");
        let writer = OpenOptions::new().write(true).open("CONOUT$");
        if let (Ok(reader), Ok(writer)) = (reader, writer) {
            return Some(ServicePromptInput::WindowsConsole {
                reader: BufReader::new(reader),
                writer,
            });
        }
    }

    None
}

#[cfg(test)]
fn prompt_input_kind_for_capabilities(
    inherited_stdin_is_terminal: bool,
    attached_windows_console_available: bool,
) -> Option<PromptInputKind> {
    if inherited_stdin_is_terminal {
        Some(PromptInputKind::InheritedStdin)
    } else if attached_windows_console_available {
        Some(PromptInputKind::WindowsConsole)
    } else {
        None
    }
}

pub fn run(args: Vec<String>) -> ExitCode {
    if args.first().is_some_and(|value| value == "agent") {
        return run_agent_command(&args[1..]);
    }

    let mut invocation = match capture_invocation(args) {
        Ok(invocation) => invocation,
        Err(error) => return print_agent_error(error),
    };
    let is_restore = invocation.args.first().map(String::as_str) == Some("restore");
    if is_restore {
        logging::info(
            SERVICE_RESTART_LOG_COMPONENT,
            format!(
                "restore client begin args={:?} cwd={:?} stdin_interactive={}",
                invocation.args,
                invocation.current_directory,
                io::stdin().is_terminal()
            ),
        );
    }

    let state = match ensure_running() {
        Ok(state) => state,
        Err(error) => {
            if is_restore {
                logging::error(
                    SERVICE_RESTART_LOG_COMPONENT,
                    format!("restore client could not start/connect Local Agent: {error}"),
                );
            }
            return print_agent_error(error);
        }
    };
    if is_restore {
        logging::info(
            SERVICE_RESTART_LOG_COMPONENT,
            format!("restore client using Local Agent pid={} port={}", state.pid, state.port),
        );
    }

    let decision_file = match prepare_service_decisions(&state, &mut invocation) {
        Ok(path) => path,
        Err(error) => {
            if is_restore {
                logging::error(
                    SERVICE_RESTART_LOG_COMPONENT,
                    format!("restore decision preparation failed: {error}"),
                );
            }
            return print_agent_error(error);
        }
    };
    let response = request(
        &state,
        AgentAction::Execute { invocation },
        RESPONSE_TIMEOUT,
    );
    if let Some(path) = decision_file {
        let _ = fs::remove_file(path);
    }
    match response {
        Ok(response) => {
            if is_restore {
                log_restore_response(&response);
            }
            render_response(response)
        }
        Err(error) => {
            if is_restore {
                logging::error(
                    SERVICE_RESTART_LOG_COMPONENT,
                    format!("restore request failed before worker response: {error}"),
                );
            }
            print_agent_error(error)
        }
    }
}

fn run_agent_command(args: &[String]) -> ExitCode {
    match args.first().map(String::as_str) {
        Some("start") if args.len() == 1 => match ensure_running() {
            Ok(state) => {
                println!("Local Agent is running (pid {}, 127.0.0.1:{}).", state.pid, state.port);
                ExitCode::SUCCESS
            }
            Err(error) => print_agent_error(error),
        },
        Some("status") if args.len() == 1 => match running_state() {
            Ok(Some(state)) => {
                println!("Local Agent: running");
                println!("  pid:      {}", state.pid);
                println!("  endpoint: 127.0.0.1:{}", state.port);
                println!("  protocol: {}", state.protocol_version);
                ExitCode::SUCCESS
            }
            Ok(None) => {
                println!("Local Agent: not running");
                ExitCode::from(1)
            }
            Err(error) => print_agent_error(error),
        },
        Some("stop") if args.len() == 1 => match running_state() {
            Ok(Some(state)) => match request(&state, AgentAction::Shutdown, Duration::from_secs(3)) {
                Ok(response) if response.ok => {
                    wait_for_shutdown();
                    println!("Local Agent stopped.");
                    ExitCode::SUCCESS
                }
                Ok(response) => print_agent_error(AgentError::Protocol(
                    response.error.unwrap_or_else(|| "Local Agent rejected shutdown".to_owned()),
                )),
                Err(error) => print_agent_error(error),
            },
            Ok(None) => {
                println!("Local Agent is not running.");
                ExitCode::SUCCESS
            }
            Err(error) => print_agent_error(error),
        },
        Some("restart") if args.len() == 1 => {
            if let Ok(Some(state)) = running_state() {
                let _ = request(&state, AgentAction::Shutdown, Duration::from_secs(3));
                wait_for_shutdown();
            }
            cleanup_stale_files();
            match ensure_running() {
                Ok(state) => {
                    println!("Local Agent restarted (pid {}, 127.0.0.1:{}).", state.pid, state.port);
                    ExitCode::SUCCESS
                }
                Err(error) => print_agent_error(error),
            }
        }
        Some("-h" | "--help") | None => {
            print_agent_usage();
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("error: usage: capsule agent <start|status|stop|restart>");
            ExitCode::from(2)
        }
    }
}

fn capture_invocation(args: Vec<String>) -> Result<CliInvocation, AgentError> {
    let current_directory = env::current_dir()
        .map_err(|error| AgentError::Runtime(format!("could not read current directory: {error}")))?
        .to_string_lossy()
        .into_owned();
    let mut environment = env::vars_os()
        .filter_map(|(key, value)| {
            let key = key.into_string().ok()?;
            if key == CALLER_PID_ENV || key == SERVICE_DECISIONS_ENV {
                return None;
            }
            Some(EnvironmentEntry {
                key,
                value: value.into_string().ok()?,
            })
        })
        .collect::<Vec<_>>();
    environment.push(EnvironmentEntry {
        key: CALLER_PID_ENV.to_owned(),
        value: std::process::id().to_string(),
    });
    Ok(CliInvocation {
        args,
        current_directory,
        environment,
    })
}

fn prepare_service_decisions(
    state: &AgentState,
    invocation: &mut CliInvocation,
) -> Result<Option<PathBuf>, AgentError> {
    if invocation.args.first().map(String::as_str) != Some("restore")
        || invocation.args.iter().any(|value| value == "--dry-run")
    {
        return Ok(None);
    }

    let mut plan_args = vec!["__service-plan".to_owned()];
    plan_args.extend(invocation.args.clone());
    let plan_invocation = CliInvocation {
        args: plan_args,
        current_directory: invocation.current_directory.clone(),
        environment: invocation.environment.clone(),
    };
    // In debug/development mode the Local Agent intentionally launches the
    // compatibility worker through Cargo in an isolated target directory. A
    // cold/rebuilt worker can legitimately take well over 15 seconds to compile.
    // The service-plan phase is part of the restore transaction, so give it the
    // same bounded timeout as the actual restore instead of timing out early and
    // silently losing the user's Ask decisions.
    let response = request(
        state,
        AgentAction::Execute {
            invocation: plan_invocation,
        },
        SERVICE_PLAN_TIMEOUT,
    )
    .map_err(|error| {
        AgentError::Runtime(format!(
            "could not prepare saved-service restart prompts before restore: {error}"
        ))
    })?;
    if !response.ok || response.exit_code != 0 {
        let detail = response
            .error
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| {
                let stderr = response.stderr.trim();
                (!stderr.is_empty()).then_some(stderr)
            })
            .unwrap_or("the service-plan worker did not return a successful plan");
        logging::error(
            SERVICE_RESTART_LOG_COMPONENT,
            format!(
                "service-plan failed ok={} exit={} detail={detail:?}",
                response.ok, response.exit_code
            ),
        );
        return Err(AgentError::Runtime(format!(
            "could not prepare saved-service restart prompts before restore: {detail}"
        )));
    }
    let plan: ServicePlan = serde_json::from_str(response.stdout.trim()).map_err(|error| {
        logging::error(
            SERVICE_RESTART_LOG_COMPONENT,
            format!(
                "service-plan JSON parse failed: {error}; stdout={:?}",
                response.stdout.trim()
            ),
        );
        AgentError::Protocol(format!(
            "saved-service restart plan was invalid and restore was stopped before silently skipping services: {error}"
        ))
    })?;
    if plan.services.is_empty() {
        logging::info(
            SERVICE_RESTART_LOG_COMPONENT,
            format!(
                "service-plan capsule={} revision={} services=0",
                plan.capsule_name, plan.revision
            ),
        );
        return Ok(None);
    }

    // A runner can redirect stdin/stdout while leaving this process attached to
    // the same Windows console as the user. Prefer inherited terminal stdin,
    // then the attached console channel, and only use the noninteractive default
    // when neither a visible prompt nor keyboard input is available.
    let inherited_stdin_interactive = io::stdin().is_terminal();
    let mut prompt_input = open_service_prompt_input();
    let prompt_source = prompt_input
        .as_ref()
        .map(|input| input.kind().as_str())
        .unwrap_or("none");
    logging::info(
        SERVICE_RESTART_LOG_COMPONENT,
        format!(
            "service-plan capsule={} revision={} services={} stdin_interactive={} prompt_input={prompt_source}",
            plan.capsule_name,
            plan.revision,
            plan.services.len(),
            inherited_stdin_interactive,
        ),
    );

    let mut decisions = Vec::new();
    let mut auto_started_noninteractive = false;
    for service in &plan.services {
        if service.restart_policy == RestartPolicy::Always {
            logging::info(
                SERVICE_RESTART_LOG_COMPONENT,
                format!(
                    "service decision index={} policy=always command={:?} source=persisted-policy",
                    service.service_index, service.command
                ),
            );
            continue;
        }
        let decision = if let Some(input) = prompt_input.as_mut() {
            if let Some(pre_start) = service.pre_start_command.as_deref() {
                input
                    .write_line(&format!("  pre-start: {pre_start}"))
                    .map_err(AgentError::Io)?;
            }
            prompt_service_decision(&service.command, input)?
        } else {
            auto_started_noninteractive = true;
            // A saved running service is part of the captured working state.
            // When the caller has no interactive input source (for example an
            // app or extension invoking `capsule restore`), skipping Ask
            // services makes restore silently incomplete. Preserve explicit
            // Skip decisions from interactive callers, but otherwise start the
            // captured service once for this restore.
            noninteractive_service_decision()
        };
        logging::info(
            SERVICE_RESTART_LOG_COMPONENT,
            format!(
                "service decision index={} policy=ask command={:?} decision={decision:?} source={}",
                service.service_index,
                service.command,
                if prompt_input.is_some() { prompt_source } else { "noninteractive-default" }
            ),
        );
        decisions.push(RestoreDecision {
            service_index: service.service_index,
            decision,
        });
    }
    if auto_started_noninteractive {
        eprintln!(
            "warning: no interactive console input is available; saved Ask services will start once for this restore"
        );
        logging::warn(
            SERVICE_RESTART_LOG_COMPONENT,
            "no interactive console input is available; Ask services defaulted to StartOnce instead of being silently skipped",
        );
    }
    if decisions.is_empty() {
        logging::info(
            SERVICE_RESTART_LOG_COMPONENT,
            "service-plan contains only Always services; no decision file required",
        );
        return Ok(None);
    }

    let decision_file = RestoreDecisionFile {
        capsule_name: plan.capsule_name,
        revision: plan.revision,
        decisions,
    };
    let path = decision_file_path();
    let bytes = serde_json::to_vec(&decision_file)
        .map_err(|error| AgentError::Protocol(format!("could not encode service decisions: {error}")))?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(AgentError::Io)?;
    file.write_all(&bytes).map_err(AgentError::Io)?;
    file.flush().map_err(AgentError::Io)?;
    #[cfg(unix)]
    {
        let mut permissions = file.metadata().map_err(AgentError::Io)?.permissions();
        permissions.set_mode(0o600);
        fs::set_permissions(&path, permissions).map_err(AgentError::Io)?;
    }
    invocation.environment.retain(|entry| entry.key != SERVICE_DECISIONS_ENV);
    invocation.environment.push(EnvironmentEntry {
        key: SERVICE_DECISIONS_ENV.to_owned(),
        value: path.to_string_lossy().into_owned(),
    });
    logging::info(
        SERVICE_RESTART_LOG_COMPONENT,
        format!(
            "decision file prepared path={:?} capsule={} revision={} decisions={}",
            path,
            decision_file.capsule_name,
            decision_file.revision,
            decision_file.decisions.len()
        ),
    );
    Ok(Some(path))
}

fn noninteractive_service_decision() -> RestoreDecisionKind {
    RestoreDecisionKind::StartOnce
}

fn prompt_service_decision(
    command: &str,
    input: &mut ServicePromptInput,
) -> Result<RestoreDecisionKind, AgentError> {
    loop {
        input
            .write_text(&format!(
                "Start saved service? {command}  [Y] Once [A] Always for this capsule [N] Skip  "
            ))
            .map_err(AgentError::Io)?;
        let mut response = String::new();
        let read = input.read_line(&mut response).map_err(AgentError::Io)?;
        if read == 0 {
            input.write_line("N").map_err(AgentError::Io)?;
            return Ok(RestoreDecisionKind::Skip);
        }
        match response.trim().to_ascii_lowercase().as_str() {
            "y" | "yes" => return Ok(RestoreDecisionKind::StartOnce),
            "a" | "always" => return Ok(RestoreDecisionKind::Always),
            "n" | "no" | "" => return Ok(RestoreDecisionKind::Skip),
            _ => input
                .write_line("Please enter Y, A, or N.")
                .map_err(AgentError::Io)?,
        }
    }
}

fn decision_file_path() -> PathBuf {
    env::temp_dir().join(format!(
        "context-capsule-service-decisions-{}.json",
        next_request_id()
    ))
}

fn ensure_running() -> Result<AgentState, AgentError> {
    if let Some(state) = running_state()? {
        return Ok(state);
    }

    // Another CLI may already be starting the singleton. Give it a short
    // window to publish its authenticated endpoint before treating the lock as
    // stale. This avoids duplicate agents during concurrent first commands.
    if paths::lock_path()?.exists() {
        for _ in 0..EXISTING_START_ATTEMPTS {
            if let Some(state) = running_state()? {
                return Ok(state);
            }
            if !paths::lock_path()?.exists() {
                break;
            }
            thread::sleep(START_POLL);
        }
    }

    cleanup_stale_files();
    start_server_process()?;
    for _ in 0..START_ATTEMPTS {
        if let Some(state) = running_state()? {
            return Ok(state);
        }
        thread::sleep(START_POLL);
    }

    Err(AgentError::Runtime(format!(
        "Local Agent did not become ready; inspect {}",
        paths::log_path()?.display()
    )))
}

fn running_state() -> Result<Option<AgentState>, AgentError> {
    let path = paths::state_path()?;
    if !path.exists() {
        return Ok(None);
    }
    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let state: AgentState = match serde_json::from_str(&raw) {
        Ok(state) => state,
        Err(_) => return Ok(None),
    };
    if state.protocol_version != PROTOCOL_VERSION {
        return Ok(None);
    }
    let response = match request(&state, AgentAction::Ping, Duration::from_secs(2)) {
        Ok(response) if response.ok && response.protocol_version == PROTOCOL_VERSION => response,
        _ => return Ok(None),
    };

    let executable = env::current_exe().map_err(|error| {
        AgentError::Runtime(format!("could not resolve capsule executable: {error}"))
    })?;
    let current_stamp = paths::executable_stamp(&executable)?;
    if state.executable_stamp != current_stamp {
        // The executable was replaced while an older agent was alive. Shut the
        // old process down instead of silently routing a new CLI into old code.
        let _ = request(&state, AgentAction::Shutdown, Duration::from_secs(3));
        return Ok(None);
    }

    if response.agent_pid != state.pid {
        return Ok(None);
    }
    Ok(Some(state))
}

fn start_server_process() -> Result<(), AgentError> {
    let executable = env::current_exe().map_err(|error| {
        AgentError::Runtime(format!("could not resolve capsule executable: {error}"))
    })?;
    let mut command = Command::new(executable);
    command
        .arg(SERVER_FLAG)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    #[cfg(windows)]
    {
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    }

    command
        .spawn()
        .map(|_| ())
        .map_err(|error| AgentError::Runtime(format!("could not start Local Agent: {error}")))
}

fn request(
    state: &AgentState,
    action: AgentAction,
    response_timeout: Duration,
) -> Result<AgentResponse, AgentError> {
    let endpoint = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), state.port);
    let mut stream = TcpStream::connect_timeout(&endpoint, CONNECT_TIMEOUT)?;
    stream.set_read_timeout(Some(response_timeout))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let request = AgentRequest {
        protocol_version: PROTOCOL_VERSION,
        request_id: next_request_id(),
        token: state.token.clone(),
        action,
    };
    ipc::write_message(&mut stream, &request)?;
    let response = ipc::read_message::<AgentResponse>(&mut stream)?;
    if response.protocol_version != PROTOCOL_VERSION {
        return Err(AgentError::Protocol(format!(
            "Local Agent response protocol mismatch: expected {}, got {}",
            PROTOCOL_VERSION, response.protocol_version
        )));
    }
    if response.request_id != request.request_id {
        return Err(AgentError::Protocol(
            "Local Agent response request ID did not match the request".to_owned(),
        ));
    }
    Ok(response)
}

fn log_restore_response(response: &AgentResponse) {
    logging::info(
        SERVICE_RESTART_LOG_COMPONENT,
        format!(
            "restore worker response ok={} exit={} subsystem={:?} stdout={:?} stderr={:?} error={:?}",
            response.ok,
            response.exit_code,
            response.subsystem,
            response.stdout.trim(),
            response.stderr.trim(),
            response.error
        ),
    );
}

fn render_response(response: AgentResponse) -> ExitCode {
    if !response.stdout.is_empty() {
        let _ = io::stdout().write_all(response.stdout.as_bytes());
        let _ = io::stdout().flush();
    }
    if !response.stderr.is_empty() {
        let _ = io::stderr().write_all(response.stderr.as_bytes());
        let _ = io::stderr().flush();
    }
    if !response.ok {
        eprintln!(
            "error: {}",
            response.error.unwrap_or_else(|| "Local Agent request failed".to_owned())
        );
        return ExitCode::from(1);
    }
    ExitCode::from(response.exit_code)
}

fn cleanup_stale_files() {
    if let Ok(path) = paths::state_path() {
        let _ = fs::remove_file(path);
    }
    if let Ok(path) = paths::lock_path() {
        let _ = fs::remove_file(path);
    }
}

fn wait_for_shutdown() {
    for _ in 0..80 {
        let state_exists = paths::state_path().is_ok_and(|path| path.exists());
        let lock_exists = paths::lock_path().is_ok_and(|path| path.exists());
        if !state_exists && !lock_exists {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn next_request_id() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let sequence = REQUEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{}-{now}-{sequence}", std::process::id())
}

fn print_agent_error(error: AgentError) -> ExitCode {
    eprintln!("error: Local Agent: {error}");
    ExitCode::from(1)
}

fn print_agent_usage() {
    println!("Local Agent management");
    println!("Usage:");
    println!("  capsule agent start");
    println!("  capsule agent status");
    println!("  capsule agent stop");
    println!("  capsule agent restart");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_ids_are_unique() {
        assert_ne!(next_request_id(), next_request_id());
    }

    #[test]
    fn invocation_capture_preserves_arguments_and_protects_caller_pid() {
        let invocation = capture_invocation(vec!["show".to_owned(), "demo@2".to_owned()]).unwrap();
        assert_eq!(invocation.args, vec!["show", "demo@2"]);
        assert!(!invocation.current_directory.is_empty());
        assert!(invocation.environment.iter().any(|entry| {
            entry.key == CALLER_PID_ENV && entry.value == std::process::id().to_string()
        }));
    }

    #[test]
    fn prompt_decision_file_name_is_unique_and_outside_capsule_database() {
        assert_ne!(decision_file_path(), decision_file_path());
    }

    #[test]
    fn service_plan_waits_as_long_as_the_restore_it_protects() {
        assert_eq!(SERVICE_PLAN_TIMEOUT, RESPONSE_TIMEOUT);
        assert!(SERVICE_PLAN_TIMEOUT >= Duration::from_secs(60));
    }

    #[test]
    fn prompt_prefers_inherited_terminal_stdin() {
        assert_eq!(
            prompt_input_kind_for_capabilities(true, true),
            Some(PromptInputKind::InheritedStdin)
        );
    }

    #[test]
    fn redirected_stdin_can_fall_back_to_attached_windows_console() {
        assert_eq!(
            prompt_input_kind_for_capabilities(false, true),
            Some(PromptInputKind::WindowsConsole)
        );
    }

    #[test]
    fn background_call_without_console_remains_noninteractive() {
        assert_eq!(prompt_input_kind_for_capabilities(false, false), None);
    }

    #[test]
    fn noninteractive_restore_starts_ask_services_once() {
        assert_eq!(
            noninteractive_service_decision(),
            RestoreDecisionKind::StartOnce
        );
    }
}
