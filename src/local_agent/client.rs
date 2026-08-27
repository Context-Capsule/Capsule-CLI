use crate::local_agent::{
    AgentError, ipc, paths,
    protocol::{
        AgentAction, AgentRequest, AgentResponse, AgentState, CliInvocation, EnvironmentEntry,
        PROTOCOL_VERSION,
    },
};
use std::{
    env, fs,
    io::{self, Write},
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream},
    process::{Command, ExitCode, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

const SERVER_FLAG: &str = "--agent-serve";
const CONNECT_TIMEOUT: Duration = Duration::from_millis(300);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const START_ATTEMPTS: usize = 120;
const START_POLL: Duration = Duration::from_millis(25);
const EXISTING_START_ATTEMPTS: usize = 80;
static REQUEST_COUNTER: AtomicU64 = AtomicU64::new(1);

pub fn run(args: Vec<String>) -> ExitCode {
    if args.first().is_some_and(|value| value == "agent") {
        return run_agent_command(&args[1..]);
    }

    let invocation = match capture_invocation(args) {
        Ok(invocation) => invocation,
        Err(error) => return print_agent_error(error),
    };
    let state = match ensure_running() {
        Ok(state) => state,
        Err(error) => return print_agent_error(error),
    };
    let response = match request(
        &state,
        AgentAction::Execute { invocation },
        RESPONSE_TIMEOUT,
    ) {
        Ok(response) => response,
        Err(error) => return print_agent_error(error),
    };
    render_response(response)
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
    let environment = env::vars_os()
        .filter_map(|(key, value)| {
            Some(EnvironmentEntry {
                key: key.into_string().ok()?,
                value: value.into_string().ok()?,
            })
        })
        .collect();
    Ok(CliInvocation {
        args,
        current_directory,
        environment,
    })
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
    fn invocation_capture_preserves_arguments() {
        let invocation = capture_invocation(vec!["show".to_owned(), "demo@2".to_owned()]).unwrap();
        assert_eq!(invocation.args, vec!["show", "demo@2"]);
        assert!(!invocation.current_directory.is_empty());
    }
}
