use crate::adapters::terminal::{ShellKind, TerminalEnvironment, TerminalSession};
use context_capsule::service_policy::validate_restart_command;

#[cfg(windows)]
use std::{
    io::Read,
    os::windows::process::CommandExt,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
#[cfg(windows)]
const HISTORY_PROBE_TIMEOUT: Duration = Duration::from_secs(4);
#[cfg(windows)]
const HISTORY_PROBE_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Prefer the exact line accepted by PSReadLine in this PowerShell process.
///
/// PowerShell may resolve a command such as `python` to a virtual-environment
/// executable before process discovery sees it. PSReadLine retains the exact
/// line the user submitted, so use that line when it is available and safe to
/// persist. The process-derived foreground command remains the fallback.
pub(crate) fn capture_command(
    session: &TerminalSession,
    foreground_command: &str,
) -> Result<String, String> {
    let typed_command = last_typed_command(session);
    choose_command(foreground_command, typed_command.as_deref())
}

fn choose_command(
    foreground_command: &str,
    typed_command: Option<&str>,
) -> Result<String, String> {
    if let Some(typed_command) = typed_command {
        if let Ok(command) = validate_restart_command(typed_command) {
            return Ok(command);
        }
    }
    validate_restart_command(foreground_command)
}

fn supports_psreadline_history(session: &TerminalSession) -> bool {
    matches!(session.environment, TerminalEnvironment::Windows)
        && matches!(
            session.shell,
            ShellKind::PowerShell | ShellKind::WindowsPowerShell
        )
        && session.pid.is_some()
}

fn last_typed_command(session: &TerminalSession) -> Option<String> {
    if !supports_psreadline_history(session) {
        return None;
    }

    #[cfg(not(windows))]
    {
        let _ = session;
        None
    }

    #[cfg(windows)]
    {
        let output = run_history_probe(session)?;
        let command = output.trim().to_owned();
        (!command.is_empty()).then_some(command)
    }
}

#[cfg(windows)]
fn run_history_probe(session: &TerminalSession) -> Option<String> {
    let pid = session.pid?;
    let default_client = match session.shell {
        ShellKind::WindowsPowerShell => "powershell.exe",
        ShellKind::PowerShell => "pwsh.exe",
        _ => return None,
    };
    let client = session
        .shell_executable
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(default_client);

    // The management runspace executes inside the target PowerShell process.
    // PSConsoleReadLine's static history therefore belongs to this exact shell
    // process rather than to the helper process or another terminal window.
    let query = r#"
try {
    $items = [Microsoft.PowerShell.PSConsoleReadLine]::GetHistoryItems()
    if ($null -eq $items -or $items.Count -eq 0) { return }

    for ($i = $items.Count - 1; $i -ge 0; $i--) {
        $item = $items[$i]
        if ($null -eq $item) { continue }
        if ([bool]$item.FromOtherSession) { continue }
        if ([bool]$item.FromHistoryFile) { continue }

        $line = [string]$item.CommandLine
        if (-not [string]::IsNullOrWhiteSpace($line)) {
            $line
            break
        }
    }
}
catch {
    # Hosts without an initialized PSReadLine instance intentionally fall back
    # to the foreground process command discovered by the existing code path.
}
"#;

    let encoded_query = query
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();

    let script = r#"
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
$targetPid = [int]$env:CONTEXT_CAPSULE_TARGET_POWERSHELL_PID
$queryHex = [string]$env:CONTEXT_CAPSULE_POWERSHELL_HISTORY_QUERY_HEX
$queryBytes = New-Object byte[] ($queryHex.Length / 2)
for ($i = 0; $i -lt $queryBytes.Length; $i++) {
    $queryBytes[$i] = [Convert]::ToByte($queryHex.Substring($i * 2, 2), 16)
}
$queryScript = [Text.Encoding]::UTF8.GetString($queryBytes)
$conn = [System.Management.Automation.Runspaces.NamedPipeConnectionInfo]::new($targetPid)
$conn.OpenTimeout = 1500
$conn.OperationTimeout = 1500
$managementRunspace = [System.Management.Automation.Runspaces.RunspaceFactory]::CreateRunspace($conn)
try {
    $managementRunspace.Open()
    $ps = [System.Management.Automation.PowerShell]::Create()
    try {
        $ps.Runspace = $managementRunspace
        [void]$ps.AddScript($queryScript)
        $result = @($ps.Invoke())
        if ($ps.Streams.Error.Count -gt 0) { exit 3 }
        if ($result.Count -gt 0 -and $null -ne $result[0]) {
            [Console]::Out.Write([string]$result[0])
        }
    }
    finally {
        if ($null -ne $ps) { $ps.Dispose() }
    }
}
catch {
    exit 2
}
finally {
    if ($null -ne $managementRunspace) { $managementRunspace.Dispose() }
}
"#;

    let mut child = Command::new(client)
        .args(["-NoLogo", "-NoProfile", "-NonInteractive", "-Command", script])
        .env("CONTEXT_CAPSULE_TARGET_POWERSHELL_PID", pid.to_string())
        .env(
            "CONTEXT_CAPSULE_POWERSHELL_HISTORY_QUERY_HEX",
            encoded_query,
        )
        .creation_flags(CREATE_NO_WINDOW)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < HISTORY_PROBE_TIMEOUT => {
                thread::sleep(HISTORY_PROBE_POLL_INTERVAL);
            }
            Ok(None) | Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };

    let mut stdout = Vec::new();
    if let Some(mut reader) = child.stdout.take() {
        let _ = reader.read_to_end(&mut stdout);
    }
    let _ = child.wait();
    if !status.success() {
        return None;
    }

    Some(String::from_utf8_lossy(&stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::terminal::{
        TerminalHost, TerminalSource, WorkingDirectorySource,
    };

    fn fake_session(shell: ShellKind, environment: TerminalEnvironment) -> TerminalSession {
        TerminalSession {
            sources: vec![TerminalSource::WindowsProcess],
            host: TerminalHost::WindowsTerminal,
            shell,
            shell_executable: None,
            environment,
            pid: Some(42),
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
    fn exact_typed_command_wins_over_resolved_venv_executable() {
        let resolved = r#""C:\Users\monji\OneDrive\Bureau\P\SW\Dino-Game-Auto-Player\venv\Scripts\python.exe" -m app"#;
        assert_eq!(
            choose_command(resolved, Some("python -m app")).unwrap(),
            "python -m app"
        );
    }

    #[test]
    fn missing_or_unsafe_history_falls_back_to_process_command() {
        assert_eq!(
            choose_command("python -m app", None).unwrap(),
            "python -m app"
        );
        assert_eq!(
            choose_command("python -m app", Some("tool --token secret")).unwrap(),
            "python -m app"
        );
    }

    #[test]
    fn probe_is_limited_to_windows_powershell_sessions() {
        let powershell = fake_session(ShellKind::PowerShell, TerminalEnvironment::Windows);
        assert!(supports_psreadline_history(&powershell));

        let bash = fake_session(ShellKind::Bash, TerminalEnvironment::Windows);
        assert!(!supports_psreadline_history(&bash));

        let wsl = fake_session(
            ShellKind::PowerShell,
            TerminalEnvironment::Wsl {
                distro: Some("Ubuntu".to_owned()),
            },
        );
        assert!(!supports_psreadline_history(&wsl));
    }

    #[cfg(not(windows))]
    #[test]
    fn history_probe_is_a_noop_off_windows() {
        let session = fake_session(ShellKind::PowerShell, TerminalEnvironment::Windows);
        assert_eq!(last_typed_command(&session), None);
    }
}
