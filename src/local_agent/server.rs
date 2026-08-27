use crate::local_agent::{
    AgentError,
    components::LocalAgentRuntime,
    ipc,
    paths,
    protocol::{AgentAction, AgentRequest, AgentResponse, AgentState, PROTOCOL_VERSION},
};
use std::{
    collections::hash_map::DefaultHasher,
    fs::{self, File, OpenOptions},
    hash::{Hash, Hasher},
    io::Write,
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    process,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const CONNECTION_TIMEOUT: Duration = Duration::from_secs(5);

pub fn serve() -> Result<(), AgentError> {
    let lock_path = paths::lock_path()?;
    let _lock = AgentLock::acquire(lock_path)?;
    let listener = TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))?;
    let port = listener.local_addr()?.port();
    let token = generate_token();
    let executable = std::env::current_exe()?;
    let state_path = paths::state_path()?;
    let state = AgentState {
        protocol_version: PROTOCOL_VERSION,
        pid: process::id(),
        port,
        token: token.clone(),
        started_at_unix_ms: now_unix_ms(),
        executable_stamp: paths::executable_stamp(&executable)?,
    };
    let _state = AgentStateGuard::write(state_path, &state)?;
    let runtime = LocalAgentRuntime::new()?;

    log_line(&format!(
        "Local Agent started pid={} port={} protocol={}",
        state.pid, state.port, state.protocol_version
    ));

    for connection in listener.incoming() {
        match connection {
            Ok(mut stream) => {
                if let Err(error) = configure_stream(&stream) {
                    log_line(&format!("IPC connection configuration failed: {error}"));
                    continue;
                }
                match handle_connection(&mut stream, &token, &runtime) {
                    Ok(should_stop) => {
                        if should_stop {
                            log_line("Local Agent shutdown requested");
                            break;
                        }
                    }
                    Err(error) => log_line(&format!("IPC request failed: {error}")),
                }
            }
            Err(error) => log_line(&format!("IPC accept failed: {error}")),
        }
    }

    log_line("Local Agent stopped");
    Ok(())
}

fn configure_stream(stream: &TcpStream) -> Result<(), std::io::Error> {
    stream.set_read_timeout(Some(CONNECTION_TIMEOUT))?;
    stream.set_write_timeout(Some(CONNECTION_TIMEOUT))?;
    stream.set_nodelay(true)?;
    Ok(())
}

fn handle_connection(
    stream: &mut TcpStream,
    token: &str,
    runtime: &LocalAgentRuntime,
) -> Result<bool, AgentError> {
    let request: AgentRequest = ipc::read_message(stream)?;
    let request_id = request.request_id.clone();

    if request.protocol_version != PROTOCOL_VERSION {
        ipc::write_message(
            stream,
            &AgentResponse::protocol_error(
                request_id,
                format!(
                    "unsupported Local Agent protocol {}; expected {}",
                    request.protocol_version, PROTOCOL_VERSION
                ),
            ),
        )?;
        return Ok(false);
    }
    if !tokens_equal(&request.token, token) {
        ipc::write_message(
            stream,
            &AgentResponse::protocol_error(request_id, "Local Agent authentication failed"),
        )?;
        return Ok(false);
    }

    match request.action {
        AgentAction::Ping => {
            ipc::write_message(
                stream,
                &AgentResponse {
                    protocol_version: PROTOCOL_VERSION,
                    request_id,
                    ok: true,
                    exit_code: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                    error: None,
                    subsystem: None,
                    agent_pid: process::id(),
                },
            )?;
            Ok(false)
        }
        AgentAction::Shutdown => {
            ipc::write_message(
                stream,
                &AgentResponse {
                    protocol_version: PROTOCOL_VERSION,
                    request_id,
                    ok: true,
                    exit_code: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                    error: None,
                    subsystem: None,
                    agent_pid: process::id(),
                },
            )?;
            Ok(true)
        }
        AgentAction::Execute { invocation } => {
            match runtime.execute(&invocation) {
                Ok(result) => {
                    log_line(&format!(
                        "request={} subsystem={:?} exit={}",
                        request_id, result.subsystem, result.exit_code
                    ));
                    ipc::write_message(
                        stream,
                        &AgentResponse {
                            protocol_version: PROTOCOL_VERSION,
                            request_id,
                            ok: true,
                            exit_code: result.exit_code,
                            stdout: result.stdout,
                            stderr: result.stderr,
                            error: None,
                            subsystem: Some(result.subsystem),
                            agent_pid: process::id(),
                        },
                    )?;
                }
                Err(error) => {
                    log_line(&format!("request={} runtime error: {}", request_id, error));
                    ipc::write_message(
                        stream,
                        &AgentResponse {
                            protocol_version: PROTOCOL_VERSION,
                            request_id,
                            ok: false,
                            exit_code: 1,
                            stdout: String::new(),
                            stderr: String::new(),
                            error: Some(error.to_string()),
                            subsystem: None,
                            agent_pid: process::id(),
                        },
                    )?;
                }
            }
            Ok(false)
        }
    }
}

fn generate_token() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let pid = process::id();
    let stack_marker = &now as *const u128 as usize;

    let mut first = DefaultHasher::new();
    now.hash(&mut first);
    pid.hash(&mut first);
    stack_marker.hash(&mut first);

    let mut second = DefaultHasher::new();
    first.finish().hash(&mut second);
    now.rotate_left(37).hash(&mut second);
    process::id().rotate_left(13).hash(&mut second);

    format!("{:016x}{:016x}", first.finish(), second.finish())
}

fn tokens_equal(left: &str, right: &str) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.as_bytes()
        .iter()
        .zip(right.as_bytes())
        .fold(0u8, |difference, (left, right)| difference | (left ^ right))
        == 0
}

fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

fn log_line(message: &str) {
    let Ok(path) = paths::log_path() else {
        return;
    };
    let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) else {
        return;
    };
    let _ = writeln!(file, "{} {}", now_unix_ms(), message);
}

struct AgentLock {
    path: PathBuf,
    _file: File,
}

impl AgentLock {
    fn acquire(path: PathBuf) -> Result<Self, AgentError> {
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::AlreadyExists {
                    AgentError::Runtime(
                        "another Context Capsule Local Agent already owns the agent lock".to_owned(),
                    )
                } else {
                    AgentError::Io(error)
                }
            })?;
        Ok(Self { path, _file: file })
    }
}

impl Drop for AgentLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

struct AgentStateGuard {
    path: PathBuf,
}

impl AgentStateGuard {
    fn write(path: PathBuf, state: &AgentState) -> Result<Self, AgentError> {
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)?;
        let temporary = parent.join(format!(
            ".{}.{}.tmp",
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("local-agent"),
            process::id()
        ));
        let encoded = serde_json::to_vec(state).map_err(|error| {
            AgentError::Protocol(format!("could not encode Local Agent state: {error}"))
        })?;
        {
            let mut file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&temporary)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                file.set_permissions(fs::Permissions::from_mode(0o600))?;
            }
            file.write_all(&encoded)?;
            file.sync_all()?;
        }
        if path.exists() {
            fs::remove_file(&path)?;
        }
        fs::rename(&temporary, &path)?;
        Ok(Self { path })
    }
}

impl Drop for AgentStateGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_compare_without_early_byte_exit() {
        assert!(tokens_equal("abcdef", "abcdef"));
        assert!(!tokens_equal("abcdef", "abcdeg"));
        assert!(!tokens_equal("short", "longer"));
    }

    #[test]
    fn generated_tokens_are_nonempty_and_fixed_width() {
        let first = generate_token();
        let second = generate_token();
        assert_eq!(first.len(), 32);
        assert_eq!(second.len(), 32);
        assert!(!first.chars().any(char::is_whitespace));
    }
}
