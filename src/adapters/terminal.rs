use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};

#[cfg(windows)]
use std::{
    env, fs,
    fs::File,
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Output, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[cfg(windows)]
const WINDOWS_PROCESS_TIMEOUT: Duration = Duration::from_secs(8);
#[cfg(windows)]
const WSL_LIST_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(windows)]
const WSL_DISTRO_TIMEOUT: Duration = Duration::from_secs(6);
#[cfg(windows)]
const COMMAND_POLL_INTERVAL: Duration = Duration::from_millis(50);
#[cfg(windows)]
static CAPTURE_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

const HISTORY_REASON: &str = "Shell history is intentionally not captured. History files can be stale until shell exit and may contain secret-bearing commands.";
const MAX_CAPTURED_COMMAND_CHARS: usize = 2048;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TerminalStatus {
    NotRequested,
    Unsupported,
    Available,
    Degraded,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TerminalSource {
    WindowsTerminalState,
    WindowsProcess,
    WslProc,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TerminalHost {
    WindowsTerminal,
    VisualStudioCode,
    Cursor,
    ConsoleHost,
    WezTerm,
    Alacritty,
    Mintty,
    Wsl,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ShellKind {
    PowerShell,
    WindowsPowerShell,
    CommandPrompt,
    Bash,
    Zsh,
    Fish,
    NuShell,
    PosixSh,
    Wsl,
    Unknown,
}

impl ShellKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::PowerShell => "PowerShell",
            Self::WindowsPowerShell => "Windows PowerShell",
            Self::CommandPrompt => "Command Prompt",
            Self::Bash => "Bash",
            Self::Zsh => "Zsh",
            Self::Fish => "Fish",
            Self::NuShell => "NuShell",
            Self::PosixSh => "POSIX shell",
            Self::Wsl => "WSL shell",
            Self::Unknown => "Unknown shell",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum TerminalEnvironment {
    Windows,
    Wsl { distro: Option<String> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkingDirectorySource {
    WindowsTerminalState,
    WslProc,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestartPlan {
    pub executable: String,
    pub args: Vec<String>,
    pub working_directory: Option<String>,
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TerminalSession {
    pub sources: Vec<TerminalSource>,
    pub host: TerminalHost,
    pub shell: ShellKind,
    pub shell_executable: Option<String>,
    pub environment: TerminalEnvironment,
    pub pid: Option<u32>,
    pub parent_pid: Option<u32>,
    pub tty: Option<String>,
    pub profile: Option<String>,
    pub title: Option<String>,
    pub working_directory: Option<String>,
    pub working_directory_source: WorkingDirectorySource,
    pub startup_command: Option<String>,
    pub foreground_command: Option<String>,
    pub restart: Option<RestartPlan>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TerminalWindowSize {
    pub width: f64,
    pub height: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TerminalLayoutAction {
    pub action: String,
    pub profile: Option<String>,
    pub commandline: Option<String>,
    pub starting_directory: Option<String>,
    pub tab_title: Option<String>,
    pub split: Option<String>,
    pub size: Option<f64>,
    pub title: Option<String>,
    pub pane_id: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowsTerminalLayout {
    pub source_path: String,
    pub window_index: usize,
    pub name: Option<String>,
    pub initial_position: Option<String>,
    pub initial_size: Option<TerminalWindowSize>,
    pub launch_mode: Option<String>,
    pub actions: Vec<TerminalLayoutAction>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalHistoryPolicy {
    pub captured: bool,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TerminalSnapshot {
    pub status: TerminalStatus,
    pub message: Option<String>,
    pub windows_terminal_layouts: Vec<WindowsTerminalLayout>,
    pub sessions: Vec<TerminalSession>,
    pub warnings: Vec<String>,
    pub history: TerminalHistoryPolicy,
}

impl TerminalSnapshot {
    pub fn not_requested() -> Self {
        Self {
            status: TerminalStatus::NotRequested,
            message: None,
            windows_terminal_layouts: Vec::new(),
            sessions: Vec::new(),
            warnings: Vec::new(),
            history: history_policy(),
        }
    }

    #[cfg(not(windows))]
    fn unsupported(message: impl Into<String>) -> Self {
        Self {
            status: TerminalStatus::Unsupported,
            message: Some(message.into()),
            windows_terminal_layouts: Vec::new(),
            sessions: Vec::new(),
            warnings: Vec::new(),
            history: history_policy(),
        }
    }

    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }

    pub fn wsl_session_count(&self) -> usize {
        self.sessions
            .iter()
            .filter(|session| matches!(session.environment, TerminalEnvironment::Wsl { .. }))
            .count()
    }
}

fn history_policy() -> TerminalHistoryPolicy {
    TerminalHistoryPolicy {
        captured: false,
        reason: HISTORY_REASON.to_owned(),
    }
}

pub fn discover() -> TerminalSnapshot {
    #[cfg(windows)]
    {
        discover_windows()
    }

    #[cfg(not(windows))]
    {
        TerminalSnapshot::unsupported(
            "terminal discovery is currently implemented for Windows and WSL only",
        )
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RawWindowsProcess {
    process_id: u32,
    parent_process_id: u32,
    name: String,
    executable_path: Option<String>,
    command_line: Option<String>,
}

#[derive(Debug, Clone)]
struct RawWslSession {
    pid: u32,
    parent_pid: u32,
    tty: String,
    shell: String,
    working_directory: String,
    commandline: String,
}

#[cfg(windows)]
fn discover_windows() -> TerminalSnapshot {
    let mut warnings = Vec::new();
    let mut degraded = false;

    let process_inventory = match query_windows_processes() {
        Ok(processes) => processes,
        Err(error) => {
            degraded = true;
            warnings.push(error);
            Vec::new()
        }
    };

    let windows_terminal_open = process_inventory.iter().any(|process| {
        matches!(
            process.name.to_ascii_lowercase().as_str(),
            "windowsterminal.exe" | "openconsole.exe"
        )
    });

    let explicit_state = env::var_os("CONTEXT_CAPSULE_TERMINAL_STATE_PATH").map(PathBuf::from);
    let state_paths = if let Some(path) = explicit_state {
        vec![path]
    } else if windows_terminal_open {
        windows_terminal_state_candidates()
    } else {
        Vec::new()
    };

    let mut layouts = Vec::new();
    let mut sessions = Vec::new();
    let mut parsed_terminal_state = false;

    for path in state_paths {
        if !path.is_file() {
            continue;
        }

        match read_json_with_retry(&path)
            .and_then(|text| parse_windows_terminal_state(&text, &path.to_string_lossy()))
        {
            Ok((mut parsed_layouts, mut parsed_sessions, mut parsed_warnings)) => {
                parsed_terminal_state |= !parsed_layouts.is_empty() || !parsed_sessions.is_empty();
                layouts.append(&mut parsed_layouts);
                sessions.append(&mut parsed_sessions);
                warnings.append(&mut parsed_warnings);
            }
            Err(error) => {
                degraded = true;
                warnings.push(format!(
                    "could not read Windows Terminal state '{}': {error}",
                    path.display()
                ));
            }
        }
    }

    if windows_terminal_open && !parsed_terminal_state {
        warnings.push(
            "Windows Terminal is open, but no persisted tab/pane layout was readable. Shell processes are still discovered, but tab geometry and current directories may be incomplete until Windows Terminal shell integration reports CWD metadata."
                .to_owned(),
        );
    }

    let process_sessions = sessions_from_windows_processes(&process_inventory);
    merge_windows_process_sessions(&mut sessions, process_sessions);

    let running_distros = match query_running_wsl_distros() {
        Ok(distros) => distros,
        Err(error) => {
            if !error.to_ascii_lowercase().contains("not installed") {
                warnings.push(error);
            }
            Vec::new()
        }
    };

    resolve_layout_wsl_distros(&mut sessions, &running_distros);

    for distro in &running_distros {
        match query_wsl_sessions(distro) {
            Ok(raw_sessions) => {
                let live_sessions = raw_sessions
                    .into_iter()
                    .map(|raw| wsl_session(distro, raw))
                    .collect::<Vec<_>>();
                merge_wsl_sessions(&mut sessions, live_sessions);
            }
            Err(error) => {
                degraded = true;
                warnings.push(format!("WSL distro '{distro}': {error}"));
            }
        }
    }

    sessions.sort_by(|left, right| {
        host_rank(&left.host)
            .cmp(&host_rank(&right.host))
            .then_with(|| left.profile.cmp(&right.profile))
            .then_with(|| left.pid.cmp(&right.pid))
    });

    let status = if degraded {
        TerminalStatus::Degraded
    } else {
        TerminalStatus::Available
    };
    let message = if sessions.is_empty() {
        Some("no open interactive terminal sessions were detected".to_owned())
    } else {
        None
    };

    TerminalSnapshot {
        status,
        message,
        windows_terminal_layouts: layouts,
        sessions,
        warnings,
        history: history_policy(),
    }
}

fn host_rank(host: &TerminalHost) -> u8 {
    match host {
        TerminalHost::WindowsTerminal => 0,
        TerminalHost::VisualStudioCode => 1,
        TerminalHost::Cursor => 2,
        TerminalHost::Wsl => 3,
        TerminalHost::WezTerm => 4,
        TerminalHost::Alacritty => 5,
        TerminalHost::Mintty => 6,
        TerminalHost::ConsoleHost => 7,
        TerminalHost::Unknown => 8,
    }
}

fn parse_windows_terminal_state(
    text: &str,
    source_path: &str,
) -> Result<
    (
        Vec<WindowsTerminalLayout>,
        Vec<TerminalSession>,
        Vec<String>,
    ),
    String,
> {
    let root: Value = serde_json::from_str(text)
        .map_err(|error| format!("invalid Windows Terminal state JSON: {error}"))?;
    let Some(windows) = root.get("persistedWindowLayouts").and_then(Value::as_array) else {
        return Ok((Vec::new(), Vec::new(), Vec::new()));
    };

    let mut layouts = Vec::new();
    let mut sessions = Vec::new();
    let mut warnings = Vec::new();

    for (window_index, window) in windows.iter().enumerate() {
        let actions = window
            .get("tabLayout")
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .map(|value| parse_layout_action(value, &mut warnings))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        let initial_size = window.get("initialSize").and_then(|size| {
            Some(TerminalWindowSize {
                width: size.get("width")?.as_f64()?,
                height: size.get("height")?.as_f64()?,
            })
        });

        let layout = WindowsTerminalLayout {
            source_path: source_path.to_owned(),
            window_index,
            name: string_field(window, "name"),
            initial_position: string_field(window, "initialPosition"),
            initial_size,
            launch_mode: string_field(window, "launchMode"),
            actions: actions.clone(),
        };

        for action in &actions {
            if matches!(action.action.as_str(), "newTab" | "splitPane") {
                sessions.push(session_from_layout_action(action));
            }
        }

        layouts.push(layout);
    }

    Ok((layouts, sessions, warnings))
}

fn parse_layout_action(value: &Value, warnings: &mut Vec<String>) -> TerminalLayoutAction {
    let original_commandline = string_field(value, "commandline");
    let commandline = original_commandline
        .as_deref()
        .and_then(sanitize_commandline);
    if original_commandline.is_some() && commandline.is_none() {
        warnings.push(
            "a Windows Terminal command line was omitted because it looked secret-bearing"
                .to_owned(),
        );
    }

    TerminalLayoutAction {
        action: string_field(value, "action").unwrap_or_else(|| "unknown".to_owned()),
        profile: string_field(value, "profile"),
        commandline,
        starting_directory: string_field(value, "startingDirectory"),
        tab_title: string_field(value, "tabTitle"),
        split: string_field(value, "split"),
        size: value.get("size").and_then(Value::as_f64),
        title: string_field(value, "title"),
        pane_id: value.get("id").and_then(Value::as_i64),
    }
}

fn session_from_layout_action(action: &TerminalLayoutAction) -> TerminalSession {
    let shell = infer_shell(action.profile.as_deref(), action.commandline.as_deref());
    let distro = infer_wsl_distro(action.profile.as_deref(), action.commandline.as_deref());
    let environment = if shell == ShellKind::Wsl || distro.is_some() {
        TerminalEnvironment::Wsl { distro }
    } else {
        TerminalEnvironment::Windows
    };
    let shell_executable = action
        .commandline
        .as_deref()
        .and_then(first_command_token)
        .map(str::to_owned);
    let working_directory_source = if action.starting_directory.is_some() {
        WorkingDirectorySource::WindowsTerminalState
    } else {
        WorkingDirectorySource::Unknown
    };

    TerminalSession {
        sources: vec![TerminalSource::WindowsTerminalState],
        host: TerminalHost::WindowsTerminal,
        shell,
        shell_executable,
        environment: environment.clone(),
        pid: None,
        parent_pid: None,
        tty: None,
        profile: action.profile.clone(),
        title: action.tab_title.clone(),
        working_directory: action.starting_directory.clone(),
        working_directory_source,
        startup_command: action.commandline.clone(),
        foreground_command: None,
        restart: restart_from_layout(
            action.profile.as_deref(),
            action.starting_directory.as_deref(),
            &environment,
        ),
    }
}

fn string_field(value: &Value, name: &str) -> Option<String> {
    value
        .get(name)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn infer_shell(profile: Option<&str>, commandline: Option<&str>) -> ShellKind {
    let combined = format!(
        "{} {}",
        profile.unwrap_or_default(),
        commandline.unwrap_or_default()
    )
    .to_ascii_lowercase();

    if combined.contains("wsl.exe")
        || combined.contains(" wsl ")
        || combined.contains("ubuntu")
        || combined.contains("debian")
        || combined.contains("kali")
        || combined.contains("opensuse")
    {
        ShellKind::Wsl
    } else if combined.contains("pwsh") || combined.contains("powershell 7") {
        ShellKind::PowerShell
    } else if combined.contains("powershell") {
        ShellKind::WindowsPowerShell
    } else if combined.contains("cmd.exe") || combined.contains("command prompt") {
        ShellKind::CommandPrompt
    } else if combined.contains("zsh") {
        ShellKind::Zsh
    } else if combined.contains("fish") {
        ShellKind::Fish
    } else if combined.contains("nu.exe") || combined.contains("nushell") {
        ShellKind::NuShell
    } else if combined.contains("bash") {
        ShellKind::Bash
    } else if combined.contains(" sh") || combined.ends_with("sh") {
        ShellKind::PosixSh
    } else {
        ShellKind::Unknown
    }
}

fn infer_shell_from_executable(name: &str) -> ShellKind {
    match name.trim().to_ascii_lowercase().as_str() {
        "pwsh.exe" | "pwsh" => ShellKind::PowerShell,
        "powershell.exe" | "powershell" => ShellKind::WindowsPowerShell,
        "cmd.exe" | "cmd" => ShellKind::CommandPrompt,
        "bash.exe" | "bash" => ShellKind::Bash,
        "zsh.exe" | "zsh" => ShellKind::Zsh,
        "fish.exe" | "fish" => ShellKind::Fish,
        "nu.exe" | "nu" | "nushell.exe" | "nushell" => ShellKind::NuShell,
        "sh.exe" | "sh" | "dash" | "ksh" => ShellKind::PosixSh,
        _ => ShellKind::Unknown,
    }
}

fn infer_wsl_distro(profile: Option<&str>, commandline: Option<&str>) -> Option<String> {
    let commandline = commandline.unwrap_or_default();
    let tokens = split_command_tokens(commandline);
    for (index, token) in tokens.iter().enumerate() {
        let lower = token.to_ascii_lowercase();
        if matches!(lower.as_str(), "-d" | "--distribution") {
            if let Some(value) = tokens.get(index + 1) {
                return Some(value.clone());
            }
        }
        if let Some(value) = token.strip_prefix("--distribution=") {
            if !value.is_empty() {
                return Some(value.to_owned());
            }
        }
    }

    profile.and_then(|profile| {
        let lower = profile.to_ascii_lowercase();
        ["ubuntu", "debian", "kali", "opensuse", "fedora", "arch"]
            .iter()
            .find(|name| lower.contains(**name))
            .map(|_| profile.to_owned())
    })
}

fn restart_from_layout(
    profile: Option<&str>,
    working_directory: Option<&str>,
    environment: &TerminalEnvironment,
) -> Option<RestartPlan> {
    if let Some(profile) = profile {
        let mut args = vec!["new-tab".to_owned(), "-p".to_owned(), profile.to_owned()];
        if let Some(directory) = working_directory {
            args.push("-d".to_owned());
            args.push(directory.to_owned());
        }
        return Some(RestartPlan {
            executable: "wt.exe".to_owned(),
            args,
            working_directory: None,
            note: Some(
                "Reopens the captured Windows Terminal profile and directory without replaying shell history or an arbitrary foreground command."
                    .to_owned(),
            ),
        });
    }

    if let TerminalEnvironment::Wsl {
        distro: Some(distro),
    } = environment
    {
        return Some(wsl_restart_plan(distro, working_directory));
    }

    None
}

fn wsl_restart_plan(distro: &str, working_directory: Option<&str>) -> RestartPlan {
    let mut args = vec!["-d".to_owned(), distro.to_owned()];
    if let Some(directory) = working_directory {
        args.push("--cd".to_owned());
        args.push(directory.to_owned());
    }

    RestartPlan {
        executable: "wsl.exe".to_owned(),
        args,
        working_directory: None,
        note: Some(
            "Starts the captured WSL distribution in the captured directory using its configured default shell."
                .to_owned(),
        ),
    }
}

fn sanitize_commandline(input: &str) -> Option<String> {
    let compact = input
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if compact.is_empty() {
        return None;
    }

    let lower = compact.to_ascii_lowercase();
    let sensitive_markers = [
        "--password",
        "--passwd",
        "--token",
        "--secret",
        "--api-key",
        "--apikey",
        "authorization:",
        "bearer ",
        "access_token",
        "refresh_token",
        "client_secret",
        "private_key",
        "secret_key",
    ];
    if sensitive_markers
        .iter()
        .any(|marker| lower.contains(marker))
        || contains_credential_url(&lower)
    {
        return None;
    }

    Some(limit_chars(&compact, MAX_CAPTURED_COMMAND_CHARS))
}

fn contains_credential_url(commandline: &str) -> bool {
    let Some(scheme_index) = commandline.find("://") else {
        return false;
    };
    let authority = &commandline[scheme_index + 3..];
    let authority = authority.split(['/', ' ', '\t']).next().unwrap_or_default();
    let Some(at_index) = authority.find('@') else {
        return false;
    };
    authority[..at_index].contains(':')
}

fn limit_chars(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_owned();
    }
    let mut limited = value.chars().take(max_chars).collect::<String>();
    limited.push('…');
    limited
}

fn first_command_token(commandline: &str) -> Option<&str> {
    let trimmed = commandline.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(rest) = trimmed.strip_prefix('"') {
        return rest.split('"').next().filter(|value| !value.is_empty());
    }
    trimmed.split_whitespace().next()
}

fn split_command_tokens(commandline: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quoted = false;

    for character in commandline.chars() {
        match character {
            '"' => quoted = !quoted,
            ' ' | '\t' if !quoted => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(character),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn parse_windows_process_inventory(text: &str) -> Result<Vec<RawWindowsProcess>, String> {
    let value: Value = serde_json::from_str(text)
        .map_err(|error| format!("invalid Windows process JSON: {error}"))?;
    if value.is_array() {
        serde_json::from_value(value)
            .map_err(|error| format!("invalid Windows process inventory: {error}"))
    } else if value.is_object() {
        serde_json::from_value(value)
            .map(|process| vec![process])
            .map_err(|error| format!("invalid Windows process inventory: {error}"))
    } else if value.is_null() {
        Ok(Vec::new())
    } else {
        Err("Windows process inventory had an unexpected JSON shape".to_owned())
    }
}

fn sessions_from_windows_processes(processes: &[RawWindowsProcess]) -> Vec<TerminalSession> {
    let by_pid = processes
        .iter()
        .map(|process| (process.process_id, process))
        .collect::<HashMap<_, _>>();

    processes
        .iter()
        .filter(|process| is_interactive_windows_shell(process))
        .map(|process| {
            let shell = infer_shell_from_executable(&process.name);
            let startup_command = process
                .command_line
                .as_deref()
                .and_then(sanitize_commandline);
            let foreground_command = foreground_child_command(process.process_id, processes);
            let host = infer_process_host(process.parent_process_id, &by_pid);
            let executable = process
                .executable_path
                .clone()
                .or_else(|| Some(process.name.clone()));
            let restart = executable.as_ref().map(|executable| RestartPlan {
                executable: executable.clone(),
                args: Vec::new(),
                working_directory: None,
                note: Some(
                    "Working directory is intentionally omitted because Windows does not expose a reliable logical shell CWD for every shell."
                        .to_owned(),
                ),
            });

            TerminalSession {
                sources: vec![TerminalSource::WindowsProcess],
                host,
                shell,
                shell_executable: executable,
                environment: TerminalEnvironment::Windows,
                pid: Some(process.process_id),
                parent_pid: Some(process.parent_process_id),
                tty: None,
                profile: None,
                title: None,
                working_directory: None,
                working_directory_source: WorkingDirectorySource::Unknown,
                startup_command,
                foreground_command,
                restart,
            }
        })
        .collect()
}

fn is_interactive_windows_shell(process: &RawWindowsProcess) -> bool {
    let name = process.name.to_ascii_lowercase();
    if !matches!(
        name.as_str(),
        "pwsh.exe"
            | "powershell.exe"
            | "cmd.exe"
            | "bash.exe"
            | "zsh.exe"
            | "fish.exe"
            | "nu.exe"
            | "nushell.exe"
            | "sh.exe"
    ) {
        return false;
    }

    let commandline = process
        .command_line
        .as_deref()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if commandline.contains("-noninteractive") {
        return false;
    }
    if name == "cmd.exe"
        && contains_switch(&commandline, "/c")
        && !contains_switch(&commandline, "/k")
    {
        return false;
    }
    if matches!(name.as_str(), "pwsh.exe" | "powershell.exe")
        && (contains_switch(&commandline, "-command") || contains_switch(&commandline, "-c"))
        && !contains_switch(&commandline, "-noexit")
    {
        return false;
    }
    if matches!(name.as_str(), "bash.exe" | "zsh.exe" | "sh.exe")
        && contains_switch(&commandline, "-c")
        && !contains_switch(&commandline, "-i")
    {
        return false;
    }
    true
}

fn contains_switch(commandline: &str, needle: &str) -> bool {
    split_command_tokens(commandline)
        .iter()
        .any(|token| token.eq_ignore_ascii_case(needle))
}

fn foreground_child_command(shell_pid: u32, processes: &[RawWindowsProcess]) -> Option<String> {
    let mut children = processes
        .iter()
        .filter(|process| process.parent_process_id == shell_pid)
        .filter(|process| !is_terminal_infrastructure(&process.name))
        .collect::<Vec<_>>();
    children.sort_by_key(|process| process.process_id);
    children
        .into_iter()
        .filter_map(|process| {
            process
                .command_line
                .as_deref()
                .and_then(sanitize_commandline)
        })
        .next()
}

fn is_terminal_infrastructure(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "conhost.exe" | "openconsole.exe" | "windowsterminal.exe" | "wslhost.exe"
    )
}

fn infer_process_host(
    parent_pid: u32,
    processes: &HashMap<u32, &RawWindowsProcess>,
) -> TerminalHost {
    let mut current = Some(parent_pid);
    let mut seen = HashSet::new();

    for _ in 0..24 {
        let Some(pid) = current else {
            break;
        };
        if !seen.insert(pid) {
            break;
        }
        let Some(process) = processes.get(&pid) else {
            break;
        };
        let name = process.name.to_ascii_lowercase();
        match name.as_str() {
            "windowsterminal.exe" | "openconsole.exe" => return TerminalHost::WindowsTerminal,
            "code.exe" => return TerminalHost::VisualStudioCode,
            "cursor.exe" => return TerminalHost::Cursor,
            "wezterm-gui.exe" | "wezterm.exe" => return TerminalHost::WezTerm,
            "alacritty.exe" => return TerminalHost::Alacritty,
            "mintty.exe" => return TerminalHost::Mintty,
            "conhost.exe" => return TerminalHost::ConsoleHost,
            _ => current = Some(process.parent_process_id),
        }
    }

    TerminalHost::Unknown
}

fn merge_windows_process_sessions(
    target: &mut Vec<TerminalSession>,
    process_sessions: Vec<TerminalSession>,
) {
    for process_session in process_sessions {
        if process_session.host == TerminalHost::WindowsTerminal {
            if let Some(existing) = target.iter_mut().find(|session| {
                session.host == TerminalHost::WindowsTerminal
                    && session.pid.is_none()
                    && shells_compatible(&session.shell, &process_session.shell)
                    && !matches!(session.environment, TerminalEnvironment::Wsl { .. })
            }) {
                merge_runtime_fields(existing, &process_session);
                continue;
            }
        }
        target.push(process_session);
    }
}

fn merge_wsl_sessions(target: &mut Vec<TerminalSession>, live_sessions: Vec<TerminalSession>) {
    for live_session in live_sessions {
        let live_distro = wsl_distro(&live_session.environment);
        if let Some(existing) = target.iter_mut().find(|session| {
            session.host == TerminalHost::WindowsTerminal
                && session.pid.is_none()
                && matches!(session.environment, TerminalEnvironment::Wsl { .. })
                && distro_compatible(wsl_distro(&session.environment), live_distro)
        }) {
            if !existing.sources.contains(&TerminalSource::WslProc) {
                existing.sources.push(TerminalSource::WslProc);
            }
            existing.pid = live_session.pid;
            existing.parent_pid = live_session.parent_pid;
            existing.tty = live_session.tty.clone();
            existing.shell = live_session.shell.clone();
            existing.shell_executable = live_session.shell_executable.clone();
            if live_session.working_directory.is_some() {
                existing.working_directory = live_session.working_directory.clone();
                existing.working_directory_source = WorkingDirectorySource::WslProc;
            }
            if existing.startup_command.is_none() {
                existing.startup_command = live_session.startup_command.clone();
            }
            if let Some(distro) = live_distro {
                existing.environment = TerminalEnvironment::Wsl {
                    distro: Some(distro.to_owned()),
                };
                existing.restart = Some(wsl_restart_plan(
                    distro,
                    existing.working_directory.as_deref(),
                ));
            }
            continue;
        }
        target.push(live_session);
    }
}

fn merge_runtime_fields(existing: &mut TerminalSession, runtime: &TerminalSession) {
    if !existing.sources.contains(&TerminalSource::WindowsProcess) {
        existing.sources.push(TerminalSource::WindowsProcess);
    }
    existing.pid = runtime.pid;
    existing.parent_pid = runtime.parent_pid;
    if existing.shell_executable.is_none() {
        existing.shell_executable = runtime.shell_executable.clone();
    }
    if existing.startup_command.is_none() {
        existing.startup_command = runtime.startup_command.clone();
    }
    existing.foreground_command = runtime.foreground_command.clone();
}

fn shells_compatible(left: &ShellKind, right: &ShellKind) -> bool {
    left == right || *left == ShellKind::Unknown || *right == ShellKind::Unknown
}

fn wsl_distro(environment: &TerminalEnvironment) -> Option<&str> {
    match environment {
        TerminalEnvironment::Wsl { distro } => distro.as_deref(),
        TerminalEnvironment::Windows => None,
    }
}

fn distro_compatible(left: Option<&str>, right: Option<&str>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => left.eq_ignore_ascii_case(right),
        _ => true,
    }
}

fn resolve_layout_wsl_distros(sessions: &mut [TerminalSession], running_distros: &[String]) {
    for session in sessions {
        let TerminalEnvironment::Wsl { distro } = &mut session.environment else {
            continue;
        };
        if distro.is_some() {
            continue;
        }

        if let Some(profile) = session.profile.as_deref() {
            if let Some(found) = running_distros.iter().find(|candidate| {
                profile.eq_ignore_ascii_case(candidate)
                    || profile
                        .to_ascii_lowercase()
                        .contains(&candidate.to_ascii_lowercase())
            }) {
                *distro = Some(found.clone());
            }
        }

        if distro.is_none() && running_distros.len() == 1 {
            *distro = Some(running_distros[0].clone());
        }

        if let Some(distro) = distro.as_deref() {
            session.restart = Some(wsl_restart_plan(
                distro,
                session.working_directory.as_deref(),
            ));
        }
    }
}

fn parse_wsl_sessions(text: &str) -> Vec<RawWslSession> {
    let mut sessions = text
        .lines()
        .filter_map(|line| {
            let mut fields = line.splitn(6, '\t');
            let pid = fields.next()?.trim().parse().ok()?;
            let parent_pid = fields.next()?.trim().parse().ok()?;
            let tty = fields.next()?.trim().to_owned();
            let shell = fields.next()?.trim().to_owned();
            let working_directory = fields.next()?.trim().to_owned();
            let commandline = fields.next().unwrap_or_default().trim().to_owned();
            if tty.is_empty() || shell.is_empty() {
                return None;
            }
            Some(RawWslSession {
                pid,
                parent_pid,
                tty,
                shell,
                working_directory,
                commandline,
            })
        })
        .collect::<Vec<_>>();

    let shells_by_pid = sessions
        .iter()
        .map(|session| (session.pid, session.parent_pid))
        .collect::<HashMap<_, _>>();
    let all_sessions = sessions.clone();
    sessions.retain(|candidate| {
        !all_sessions.iter().any(|other| {
            other.pid != candidate.pid
                && other.tty == candidate.tty
                && is_descendant_of(other.pid, candidate.pid, &shells_by_pid)
        })
    });
    sessions
}

fn is_descendant_of(mut pid: u32, ancestor: u32, parents: &HashMap<u32, u32>) -> bool {
    let mut seen = HashSet::new();
    for _ in 0..24 {
        if !seen.insert(pid) {
            return false;
        }
        let Some(parent) = parents.get(&pid).copied() else {
            return false;
        };
        if parent == ancestor {
            return true;
        }
        if parent == 0 || parent == pid {
            return false;
        }
        pid = parent;
    }
    false
}

fn wsl_session(distro: &str, raw: RawWslSession) -> TerminalSession {
    let shell = infer_shell_from_executable(&raw.shell);
    let startup_command = sanitize_commandline(&raw.commandline);
    let working_directory =
        (!raw.working_directory.is_empty()).then(|| raw.working_directory.clone());

    TerminalSession {
        sources: vec![TerminalSource::WslProc],
        host: TerminalHost::Wsl,
        shell,
        shell_executable: Some(raw.shell),
        environment: TerminalEnvironment::Wsl {
            distro: Some(distro.to_owned()),
        },
        pid: Some(raw.pid),
        parent_pid: Some(raw.parent_pid),
        tty: Some(raw.tty),
        profile: None,
        title: None,
        working_directory: working_directory.clone(),
        working_directory_source: if working_directory.is_some() {
            WorkingDirectorySource::WslProc
        } else {
            WorkingDirectorySource::Unknown
        },
        startup_command,
        foreground_command: None,
        restart: Some(wsl_restart_plan(distro, working_directory.as_deref())),
    }
}

#[cfg(windows)]
fn query_windows_processes() -> Result<Vec<RawWindowsProcess>, String> {
    let script = r#"$ErrorActionPreference = 'Stop'; [Console]::OutputEncoding = [System.Text.Encoding]::UTF8; $items = if (Get-Command Get-CimInstance -ErrorAction SilentlyContinue) { Get-CimInstance Win32_Process } else { Get-WmiObject Win32_Process }; $items | Select-Object ProcessId,ParentProcessId,Name,ExecutablePath,CommandLine | ConvertTo-Json -Compress"#;
    let mut command = Command::new("powershell.exe");
    command.args([
        "-NoLogo",
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        script,
    ]);
    let output = run_bounded(
        command,
        "Windows process inventory",
        WINDOWS_PROCESS_TIMEOUT,
    )?;
    if !output.status.success() {
        return Err(format!(
            "Windows process inventory failed: {}",
            decode_windows_output(&output.stderr).trim()
        ));
    }
    parse_windows_process_inventory(&decode_windows_output(&output.stdout))
}

#[cfg(windows)]
fn query_running_wsl_distros() -> Result<Vec<String>, String> {
    let mut command = Command::new("wsl.exe");
    command.args(["--list", "--running", "--quiet"]);
    let output = run_bounded(command, "wsl --list --running --quiet", WSL_LIST_TIMEOUT)?;
    if !output.status.success() {
        let detail = decode_windows_output(&output.stderr).trim().to_owned();
        return Err(if detail.is_empty() {
            "WSL is not installed or no WSL command is available".to_owned()
        } else {
            format!("could not list running WSL distributions: {detail}")
        });
    }

    let mut seen = HashSet::new();
    Ok(decode_windows_output(&output.stdout)
        .lines()
        .map(|line| line.trim_matches(['\u{feff}', '\0', ' ', '\t', '\r']))
        .filter(|line| !line.is_empty())
        .filter(|line| seen.insert(line.to_ascii_lowercase()))
        .map(str::to_owned)
        .collect())
}

#[cfg(windows)]
fn query_wsl_sessions(distro: &str) -> Result<Vec<RawWslSession>, String> {
    const SCRIPT: &str = r#"for p in /proc/[0-9]*; do pid=${p##*/}; [ -r "$p/comm" ] || continue; comm=$(cat "$p/comm" 2>/dev/null) || continue; case "$comm" in bash|zsh|fish|sh|dash|ksh|nu) ;; *) continue ;; esac; tty=$(readlink "$p/fd/0" 2>/dev/null || true); case "$tty" in /dev/pts/*|/dev/tty*) ;; *) continue ;; esac; cwd=$(readlink "$p/cwd" 2>/dev/null || true); stat=$(cat "$p/stat" 2>/dev/null || true); rest=${stat#*) }; rest=${rest#* }; ppid=${rest%% *}; cmd=$(tr '\000\t\r\n' ' ' < "$p/cmdline" 2>/dev/null || true); printf '%s\t%s\t%s\t%s\t%s\t%s\n' "$pid" "$ppid" "$tty" "$comm" "$cwd" "$cmd"; done"#;
    let mut command = Command::new("wsl.exe");
    command.args(["-d", distro, "--", "sh", "-lc", SCRIPT]);
    let output = run_bounded(
        command,
        &format!("WSL terminal discovery for {distro}"),
        WSL_DISTRO_TIMEOUT,
    )?;
    if !output.status.success() {
        return Err(format!(
            "terminal query failed: {}",
            decode_windows_output(&output.stderr).trim()
        ));
    }
    Ok(parse_wsl_sessions(&decode_windows_output(&output.stdout)))
}

#[cfg(windows)]
fn windows_terminal_state_candidates() -> Vec<PathBuf> {
    let Some(local_app_data) = env::var_os("LOCALAPPDATA").map(PathBuf::from) else {
        return Vec::new();
    };
    [
        local_app_data
            .join("Packages")
            .join("Microsoft.WindowsTerminal_8wekyb3d8bbwe")
            .join("LocalState")
            .join("state.json"),
        local_app_data
            .join("Packages")
            .join("Microsoft.WindowsTerminalPreview_8wekyb3d8bbwe")
            .join("LocalState")
            .join("state.json"),
        local_app_data
            .join("Packages")
            .join("Microsoft.WindowsTerminalCanary_8wekyb3d8bbwe")
            .join("LocalState")
            .join("state.json"),
        local_app_data
            .join("Microsoft")
            .join("Windows Terminal")
            .join("state.json"),
    ]
    .into_iter()
    .collect()
}

#[cfg(windows)]
fn read_json_with_retry(path: &Path) -> Result<String, String> {
    let mut last_error = None;
    for _ in 0..3 {
        match fs::read_to_string(path) {
            Ok(text) => return Ok(text),
            Err(error) => {
                last_error = Some(error);
                thread::sleep(Duration::from_millis(50));
            }
        }
    }
    Err(last_error
        .map(|error| error.to_string())
        .unwrap_or_else(|| "unknown read error".to_owned()))
}

#[cfg(windows)]
fn decode_windows_output(bytes: &[u8]) -> String {
    if bytes.len() >= 2 && (bytes[0] == 0xff && bytes[1] == 0xfe) {
        let words = bytes[2..]
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect::<Vec<_>>();
        return String::from_utf16_lossy(&words);
    }
    if bytes.iter().take(128).any(|byte| *byte == 0) {
        let words = bytes
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect::<Vec<_>>();
        return String::from_utf16_lossy(&words);
    }
    String::from_utf8_lossy(bytes).into_owned()
}

#[cfg(windows)]
fn run_bounded(
    mut command: Command,
    description: &str,
    timeout: Duration,
) -> Result<Output, String> {
    let capture = CaptureFiles::create()
        .ok_or_else(|| format!("failed to create output capture files for '{description}'"))?;
    command
        .stdout(Stdio::from(
            capture
                .stdout_writer
                .as_ref()
                .and_then(|file| file.try_clone().ok())
                .ok_or_else(|| format!("failed to capture stdout for '{description}'"))?,
        ))
        .stderr(Stdio::from(
            capture
                .stderr_writer
                .as_ref()
                .and_then(|file| file.try_clone().ok())
                .ok_or_else(|| format!("failed to capture stderr for '{description}'"))?,
        ));

    let mut child = command
        .spawn()
        .map_err(|error| format!("failed to run '{description}': {error}"))?;
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < timeout => thread::sleep(COMMAND_POLL_INTERVAL),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "'{description}' timed out after {} second(s)",
                    timeout.as_secs()
                ));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("failed while waiting for '{description}': {error}"));
            }
        }
    };

    capture
        .finish(status)
        .ok_or_else(|| format!("failed to read output from '{description}'"))
}

#[cfg(windows)]
struct CaptureFiles {
    stdout_path: PathBuf,
    stderr_path: PathBuf,
    stdout_writer: Option<File>,
    stderr_writer: Option<File>,
}

#[cfg(windows)]
impl CaptureFiles {
    fn create() -> Option<Self> {
        let id = CAPTURE_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()?
            .as_nanos();
        let prefix = format!(
            "context-capsule-terminal-{}-{timestamp}-{id}",
            std::process::id()
        );
        let directory = env::temp_dir();
        let stdout_path = directory.join(format!("{prefix}.stdout"));
        let stderr_path = directory.join(format!("{prefix}.stderr"));
        let stdout_writer = File::create(&stdout_path).ok()?;
        let stderr_writer = match File::create(&stderr_path) {
            Ok(file) => file,
            Err(_) => {
                let _ = fs::remove_file(&stdout_path);
                return None;
            }
        };
        Some(Self {
            stdout_path,
            stderr_path,
            stdout_writer: Some(stdout_writer),
            stderr_writer: Some(stderr_writer),
        })
    }

    fn close_writers(&mut self) {
        drop(self.stdout_writer.take());
        drop(self.stderr_writer.take());
    }

    fn finish(mut self, status: ExitStatus) -> Option<Output> {
        self.close_writers();
        let stdout = fs::read(&self.stdout_path).unwrap_or_default();
        let stderr = fs::read(&self.stderr_path).unwrap_or_default();
        let _ = fs::remove_file(&self.stdout_path);
        let _ = fs::remove_file(&self.stderr_path);
        Some(Output {
            status,
            stdout,
            stderr,
        })
    }
}

#[cfg(windows)]
impl Drop for CaptureFiles {
    fn drop(&mut self) {
        self.close_writers();
        let _ = fs::remove_file(&self.stdout_path);
        let _ = fs::remove_file(&self.stderr_path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TERMINAL_STATE: &str = r#"
{
  "persistedWindowLayouts": [
    {
      "initialPosition": "120,80",
      "initialSize": { "width": 1200.0, "height": 760.0 },
      "launchMode": "default",
      "tabLayout": [
        {
          "action": "newTab",
          "commandline": "\"C:\\Program Files\\PowerShell\\7\\pwsh.exe\"",
          "profile": "PowerShell",
          "startingDirectory": "C:\\work\\capsule",
          "tabTitle": "PowerShell"
        },
        {
          "action": "splitPane",
          "commandline": "wsl.exe -d Ubuntu",
          "profile": "Ubuntu",
          "startingDirectory": "//wsl.localhost/Ubuntu/home/dhia/project",
          "split": "right",
          "size": 0.5
        },
        { "action": "focusPane", "id": 1 }
      ]
    }
  ]
}
"#;

    #[test]
    fn parses_windows_terminal_layout_and_restart_metadata() {
        let (layouts, sessions, warnings) =
            parse_windows_terminal_state(TERMINAL_STATE, "state.json").expect("state parses");
        assert!(warnings.is_empty());
        assert_eq!(layouts.len(), 1);
        assert_eq!(layouts[0].actions.len(), 3);
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].shell, ShellKind::PowerShell);
        assert_eq!(
            sessions[0].working_directory.as_deref(),
            Some(r"C:\work\capsule")
        );
        assert_eq!(sessions[1].shell, ShellKind::Wsl);
        assert_eq!(wsl_distro(&sessions[1].environment), Some("Ubuntu"));
        assert_eq!(
            sessions[0]
                .restart
                .as_ref()
                .map(|plan| plan.executable.as_str()),
            Some("wt.exe")
        );
    }

    #[test]
    fn secret_bearing_commandlines_are_not_persisted() {
        let fixture = r#"{
          "persistedWindowLayouts": [{
            "tabLayout": [{
              "action": "newTab",
              "profile": "PowerShell",
              "commandline": "pwsh.exe -NoExit -Command tool --token super-secret"
            }]
          }]
        }"#;
        let (layouts, sessions, warnings) =
            parse_windows_terminal_state(fixture, "state.json").expect("state parses");
        assert_eq!(layouts[0].actions[0].commandline, None);
        assert_eq!(sessions[0].startup_command, None);
        assert!(!warnings.is_empty());
        let json = serde_json::to_string(&sessions).expect("serialize sessions");
        assert!(!json.contains("super-secret"));
    }

    #[test]
    fn process_inventory_detects_hosts_and_filters_noninteractive_probes() {
        let fixture = r#"[
          {"ProcessId":10,"ParentProcessId":1,"Name":"WindowsTerminal.exe","ExecutablePath":"C:\\Terminal\\WindowsTerminal.exe","CommandLine":"WindowsTerminal.exe"},
          {"ProcessId":11,"ParentProcessId":10,"Name":"OpenConsole.exe","ExecutablePath":"C:\\Terminal\\OpenConsole.exe","CommandLine":"OpenConsole.exe"},
          {"ProcessId":12,"ParentProcessId":11,"Name":"pwsh.exe","ExecutablePath":"C:\\Program Files\\PowerShell\\7\\pwsh.exe","CommandLine":"pwsh.exe"},
          {"ProcessId":13,"ParentProcessId":12,"Name":"node.exe","ExecutablePath":"C:\\Program Files\\nodejs\\node.exe","CommandLine":"node.exe server.js"},
          {"ProcessId":20,"ParentProcessId":1,"Name":"powershell.exe","ExecutablePath":"C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe","CommandLine":"powershell.exe -NoProfile -NonInteractive -Command test"}
        ]"#;
        let processes = parse_windows_process_inventory(fixture).expect("process fixture");
        let sessions = sessions_from_windows_processes(&processes);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].host, TerminalHost::WindowsTerminal);
        assert_eq!(sessions[0].shell, ShellKind::PowerShell);
        assert_eq!(
            sessions[0].foreground_command.as_deref(),
            Some("node.exe server.js")
        );
    }

    #[test]
    fn wsl_proc_parser_keeps_leaf_shell_per_tty_and_real_cwd() {
        let fixture = concat!(
            "100\t1\t/dev/pts/0\tbash\t/home/dhia\tbash\n",
            "101\t100\t/dev/pts/0\tzsh\t/home/dhia/project\tzsh\n",
            "200\t1\t/dev/pts/1\tbash\t/home/dhia/api\tbash\n"
        );
        let sessions = parse_wsl_sessions(fixture);
        assert_eq!(sessions.len(), 2);
        assert!(sessions.iter().any(|session| {
            session.pid == 101 && session.working_directory == "/home/dhia/project"
        }));
        assert!(sessions.iter().any(|session| {
            session.pid == 200 && session.working_directory == "/home/dhia/api"
        }));
    }

    #[test]
    fn restart_plan_for_wsl_does_not_replay_history() {
        let plan = wsl_restart_plan("Ubuntu", Some("/home/dhia/project"));
        assert_eq!(plan.executable, "wsl.exe");
        assert_eq!(
            plan.args,
            vec!["-d", "Ubuntu", "--cd", "/home/dhia/project"]
        );
        assert!(
            plan.note
                .as_deref()
                .unwrap_or_default()
                .contains("default shell")
        );
    }

    #[test]
    fn command_sanitizer_rejects_credentials_and_limits_length() {
        assert!(sanitize_commandline("curl https://user:pass@example.com/api").is_none());
        assert!(sanitize_commandline("tool --password hunter2").is_none());
        assert_eq!(
            sanitize_commandline("pnpm   dev\n").as_deref(),
            Some("pnpm dev")
        );
        let long = "x".repeat(MAX_CAPTURED_COMMAND_CHARS + 20);
        let sanitized = sanitize_commandline(&long).expect("safe command");
        assert_eq!(sanitized.chars().count(), MAX_CAPTURED_COMMAND_CHARS + 1);
        assert!(sanitized.ends_with('…'));
    }
}
