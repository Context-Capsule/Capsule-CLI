use crate::{
    adapters::terminal::{ShellKind, TerminalSession},
    logging,
};

#[cfg(windows)]
use std::{
    io::Read,
    os::windows::process::CommandExt,
    path::Path,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const TERMINAL_LOG_COMPONENT: &str = "terminal";
#[cfg(windows)]
const SERVICE_RESTART_LOG_COMPONENT: &str = "service-restart";

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
#[cfg(windows)]
const PROBE_TIMEOUT: Duration = Duration::from_secs(4);
#[cfg(windows)]
const PROBE_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Read the filesystem location of the primary interactive PowerShell runspace
/// hosted by `session.pid` without sending input to the terminal and without
/// attaching the PowerShell debugger.
///
/// This is the preferred, non-invasive path. Windows PowerShell 5.1 can expose
/// the host process through the named-pipe management connection while still
/// refusing SessionStateProxy access to the interactive runspace. When that
/// happens, execute a tiny read-only pipeline inside the already-idle target
/// runspace instead. That observes the target runspace's real `$PWD` without
/// typing into the terminal or adding a probe command to PSReadLine history.
pub(super) fn working_directory(session: &TerminalSession) -> Option<String> {
    #[cfg(not(windows))]
    {
        let _ = session;
        None
    }

    #[cfg(windows)]
    {
        let query = r#"
$currentManagementRunspace = [System.Management.Automation.Runspaces.Runspace]::DefaultRunspace
$deadline = [DateTime]::UtcNow.AddMilliseconds(1200)
$target = $null
do {
    $target = @(
        Get-Runspace |
            Where-Object {
                $_ -ne $currentManagementRunspace -and
                $_.RunspaceStateInfo.State -eq [System.Management.Automation.Runspaces.RunspaceState]::Opened
            } |
            Sort-Object Id
    )[0]
    if ($null -eq $target) { return }
    if ($target.RunspaceAvailability -eq [System.Management.Automation.Runspaces.RunspaceAvailability]::Available) {
        break
    }
    Start-Sleep -Milliseconds 50
} while ([DateTime]::UtcNow -lt $deadline)
if ($target.RunspaceAvailability -ne [System.Management.Automation.Runspaces.RunspaceAvailability]::Available) { return }

# First try the zero-pipeline SessionStateProxy path. Some hosts, notably
# Windows PowerShell 5.1 under process remoting, expose the runspace but reject
# this proxy even while the interactive runspace is idle.
try {
    $location = $target.SessionStateProxy.Path.CurrentLocation
    if ($null -ne $location -and $location.Provider.Name -eq 'FileSystem') {
        [string]$location.Path
        return
    }
}
catch {
}

# Fallback: invoke a read-only pipeline in the exact idle interactive runspace.
# This does not use terminal input/PSReadLine and therefore does not alter the
# user's command history. Assign the here-string before AddScript so Windows
# PowerShell 5.1 parses the closing delimiter on its own line.
$probe = $null
try {
    $probeScript = @'
$location = $ExecutionContext.SessionState.Path.CurrentLocation
if ($null -ne $location -and $location.Provider.Name -eq 'FileSystem') {
    [string]$location.Path
}
'@
    $probe = [System.Management.Automation.PowerShell]::Create()
    $probe.Runspace = $target
    [void]$probe.AddScript($probeScript)
    $result = @($probe.Invoke())
    if (-not $probe.HadErrors -and $result.Count -gt 0 -and $null -ne $result[0]) {
        [string]$result[0]
    }
}
catch {
}
finally {
    if ($null -ne $probe) { $probe.Dispose() }
}
"#;

        let Some(output) = run_management_query(session, query, "CWD") else {
            logging::info(
                SERVICE_RESTART_LOG_COMPONENT,
                format!(
                    "PowerShell exact CWD probe pid={:?} shell={} result=unavailable stage=management-query",
                    session.pid,
                    session.shell.as_str(),
                ),
            );
            return None;
        };
        let directory = output.trim().to_owned();
        if directory.is_empty() {
            if let Some(pid) = session.pid {
                logging::info(
                    TERMINAL_LOG_COMPONENT,
                    format!(
                        "PowerShell runspace CWD probe: pid={pid} returned no accessible idle filesystem location"
                    ),
                );
            }
            logging::info(
                SERVICE_RESTART_LOG_COMPONENT,
                format!(
                    "PowerShell exact CWD probe pid={:?} shell={} result=unavailable stage=target-runspace",
                    session.pid,
                    session.shell.as_str(),
                ),
            );
            return None;
        }
        if !Path::new(&directory).is_dir() {
            if let Some(pid) = session.pid {
                logging::warn(
                    TERMINAL_LOG_COMPONENT,
                    format!(
                        "PowerShell runspace CWD probe: pid={pid} returned non-directory path {:?}",
                        directory,
                    ),
                );
            }
            logging::warn(
                SERVICE_RESTART_LOG_COMPONENT,
                format!(
                    "PowerShell exact CWD probe pid={:?} shell={} result=non-directory cwd={directory:?}",
                    session.pid,
                    session.shell.as_str(),
                ),
            );
            return None;
        }

        if let Some(pid) = session.pid {
            logging::info(
                TERMINAL_LOG_COMPONENT,
                format!("PowerShell runspace CWD probe: pid={pid} exact_cwd={directory:?}"),
            );
        }
        logging::info(
            SERVICE_RESTART_LOG_COMPONENT,
            format!(
                "PowerShell exact CWD probe pid={:?} shell={} result=trusted cwd={directory:?} source=runspace",
                session.pid,
                session.shell.as_str(),
            ),
        );
        Some(directory)
    }
}

/// Return whether the primary interactive runspace is provably idle.
///
/// `Some(true)` means PowerShell reports RunspaceAvailability::Available.
/// `Some(false)` means the runspace exists but is Busy/otherwise unavailable.
/// `None` means Context Capsule could not prove either state and callers must
/// treat that as unsafe for keyboard/UI injection.
pub(super) fn is_idle(session: &TerminalSession) -> Option<bool> {
    #[cfg(not(windows))]
    {
        let _ = session;
        None
    }

    #[cfg(windows)]
    {
        let query = r#"
$currentManagementRunspace = [System.Management.Automation.Runspaces.Runspace]::DefaultRunspace
$target = @(
    Get-Runspace |
        Where-Object {
            $_ -ne $currentManagementRunspace -and
            $_.RunspaceStateInfo.State -eq [System.Management.Automation.Runspaces.RunspaceState]::Opened
        } |
        Sort-Object Id
)[0]
if ($null -eq $target) {
    'UNKNOWN'
    return
}
if ($target.RunspaceAvailability -eq [System.Management.Automation.Runspaces.RunspaceAvailability]::Available) {
    'IDLE'
} else {
    'BUSY'
}
"#;

        let output = run_management_query(session, query, "idle-state")?;
        let pid = session.pid?;
        match output.trim() {
            "IDLE" => {
                logging::info(
                    TERMINAL_LOG_COMPONENT,
                    format!("PowerShell runspace idle gate: pid={pid} state=Available"),
                );
                Some(true)
            }
            "BUSY" => {
                logging::info(
                    TERMINAL_LOG_COMPONENT,
                    format!("PowerShell runspace idle gate: pid={pid} state=Busy"),
                );
                Some(false)
            }
            other => {
                logging::info(
                    TERMINAL_LOG_COMPONENT,
                    format!(
                        "PowerShell runspace idle gate: pid={pid} state could not be proven output={other:?}"
                    ),
                );
                None
            }
        }
    }
}

#[cfg(windows)]
fn run_management_query(
    session: &TerminalSession,
    query_script: &str,
    label: &str,
) -> Option<String> {
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

    // Encode the inner query so arbitrary quotes/newlines in the Rust literal do
    // not have to be escaped through two nested PowerShell parsers.
    let encoded_query = query_script
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();

    let script = r#"
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
$targetPid = [int]$env:CONTEXT_CAPSULE_TARGET_POWERSHELL_PID
$queryHex = [string]$env:CONTEXT_CAPSULE_POWERSHELL_QUERY_HEX
$queryBytes = New-Object byte[] ($queryHex.Length / 2)
for ($i = 0; $i -lt $queryBytes.Length; $i++) {
    $queryBytes[$i] = [Convert]::ToByte($queryHex.Substring($i * 2, 2), 16)
}
$queryScript = [Text.Encoding]::UTF8.GetString($queryBytes)
$conn = [System.Management.Automation.Runspaces.NamedPipeConnectionInfo]::new($targetPid)
$conn.OpenTimeout = 1500
$conn.OperationTimeout = 2500
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
    [Console]::Error.Write($_.Exception.Message)
    exit 2
}
finally {
    if ($null -ne $managementRunspace) { $managementRunspace.Dispose() }
}
"#;

    let mut child = match Command::new(client)
        .args(["-NoLogo", "-NoProfile", "-NonInteractive", "-Command", script])
        .env("CONTEXT_CAPSULE_TARGET_POWERSHELL_PID", pid.to_string())
        .env("CONTEXT_CAPSULE_POWERSHELL_QUERY_HEX", encoded_query)
        .creation_flags(CREATE_NO_WINDOW)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            logging::warn(
                TERMINAL_LOG_COMPONENT,
                format!(
                    "PowerShell runspace {label} probe: pid={pid} client={client} could not start: {error}"
                ),
            );
            return None;
        }
    };

    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < PROBE_TIMEOUT => {
                thread::sleep(PROBE_POLL_INTERVAL);
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                logging::warn(
                    TERMINAL_LOG_COMPONENT,
                    format!(
                        "PowerShell runspace {label} probe: pid={pid} client={client} timed out after {} ms",
                        PROBE_TIMEOUT.as_millis(),
                    ),
                );
                return None;
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                logging::warn(
                    TERMINAL_LOG_COMPONENT,
                    format!(
                        "PowerShell runspace {label} probe: pid={pid} client={client} wait failed: {error}"
                    ),
                );
                return None;
            }
        }
    };

    let mut stdout = Vec::new();
    if let Some(mut reader) = child.stdout.take() {
        let _ = reader.read_to_end(&mut stdout);
    }
    let mut stderr = Vec::new();
    if let Some(mut reader) = child.stderr.take() {
        let _ = reader.read_to_end(&mut stderr);
    }
    let _ = child.wait();

    if !status.success() {
        let detail = String::from_utf8_lossy(&stderr).trim().to_owned();
        logging::info(
            TERMINAL_LOG_COMPONENT,
            format!(
                "PowerShell runspace {label} probe: pid={pid} client={client} unavailable (exit={:?}){}",
                status.code(),
                if detail.is_empty() {
                    String::new()
                } else {
                    format!(" detail={detail:?}")
                },
            ),
        );
        return None;
    }

    Some(String::from_utf8_lossy(&stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::terminal::{
        TerminalEnvironment, TerminalHost, TerminalSource, WorkingDirectorySource,
    };

    fn fake_session(shell: ShellKind) -> TerminalSession {
        TerminalSession {
            sources: vec![TerminalSource::WindowsProcess],
            host: TerminalHost::WindowsTerminal,
            shell,
            shell_executable: None,
            environment: TerminalEnvironment::Windows,
            pid: Some(u32::MAX),
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

    #[cfg(not(windows))]
    #[test]
    fn runspace_probe_is_a_noop_off_windows() {
        let session = fake_session(ShellKind::WindowsPowerShell);
        assert_eq!(working_directory(&session), None);
        assert_eq!(is_idle(&session), None);
    }

    #[test]
    fn helper_accepts_terminal_session_shape_without_mutating_it() {
        let session = fake_session(ShellKind::WindowsPowerShell);
        let before = session.clone();
        let _ = working_directory(&session);
        let _ = is_idle(&session);
        assert_eq!(session, before);
    }
}
