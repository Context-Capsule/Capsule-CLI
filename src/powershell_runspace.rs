use crate::{
    adapters::terminal::{ShellKind, TerminalSession},
    logging,
};

#[cfg(windows)]
use std::{
    os::windows::process::CommandExt,
    path::Path,
    process::Command,
};

const TERMINAL_LOG_COMPONENT: &str = "terminal";

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Read the filesystem location of the interactive PowerShell runspace hosted by
/// `session.pid` without sending input to the terminal and without attaching the
/// PowerShell debugger.
///
/// PowerShell exposes a local named-pipe management connection specifically for
/// attach-to-process scenarios. The management runspace created by that
/// connection can enumerate the process' existing runspaces. We only inspect
/// their SessionStateProxy.Path.CurrentLocation and never call Debug-Runspace,
/// Set-Location, or execute code inside the user's interactive runspace.
pub(super) fn working_directory(session: &TerminalSession) -> Option<String> {
    #[cfg(not(windows))]
    {
        let _ = session;
        None
    }

    #[cfg(windows)]
    {
        let pid = session.pid?;
        let client = match session.shell {
            ShellKind::WindowsPowerShell => "powershell.exe",
            ShellKind::PowerShell => "pwsh.exe",
            _ => return None,
        };

        let script = r#"
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
$targetPid = [int]$env:CONTEXT_CAPSULE_TARGET_POWERSHELL_PID
$conn = [System.Management.Automation.Runspaces.NamedPipeConnectionInfo]::new($targetPid)
$conn.OpenTimeout = 1500
$conn.OperationTimeout = 1500
$managementRunspace = [System.Management.Automation.Runspaces.RunspaceFactory]::CreateRunspace($conn)
try {
    $managementRunspace.Open()
    $ps = [System.Management.Automation.PowerShell]::Create()
    try {
        $ps.Runspace = $managementRunspace
        [void]$ps.AddScript(@'
$currentManagementRunspace = [System.Management.Automation.Runspaces.Runspace]::DefaultRunspace
$candidates = @(
    Get-Runspace |
        Where-Object {
            $_ -ne $currentManagementRunspace -and
            $_.State -eq 'Opened'
        } |
        ForEach-Object {
            try {
                $location = $_.SessionStateProxy.Path.CurrentLocation
                if ($null -ne $location -and $location.Provider.Name -eq 'FileSystem') {
                    [pscustomobject]@{
                        Id = $_.Id
                        Availability = [string]$_.RunspaceAvailability
                        Path = [string]$location.Path
                    }
                }
            }
            catch {
                # A busy runspace can reject SessionStateProxy access. Ignore it
                # instead of disturbing the command that is currently running.
            }
        }
)

$candidates |
    Sort-Object @{ Expression = { if ($_.Availability -eq 'Available') { 0 } else { 1 } } }, Id |
    Select-Object -First 1 -ExpandProperty Path
'@)
        $result = @($ps.Invoke())
        if ($ps.Streams.Error.Count -gt 0) {
            exit 3
        }
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

        let output = match Command::new(client)
            .args(["-NoLogo", "-NoProfile", "-NonInteractive", "-Command", script])
            .env("CONTEXT_CAPSULE_TARGET_POWERSHELL_PID", pid.to_string())
            .creation_flags(CREATE_NO_WINDOW)
            .output()
        {
            Ok(output) => output,
            Err(error) => {
                logging::warn(
                    TERMINAL_LOG_COMPONENT,
                    format!(
                        "PowerShell runspace CWD probe: pid={pid} client={client} could not start: {error}"
                    ),
                );
                return None;
            }
        };

        if !output.status.success() {
            logging::info(
                TERMINAL_LOG_COMPONENT,
                format!(
                    "PowerShell runspace CWD probe: pid={pid} client={client} unavailable (exit={:?})",
                    output.status.code(),
                ),
            );
            return None;
        }

        let directory = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if directory.is_empty() {
            logging::info(
                TERMINAL_LOG_COMPONENT,
                format!(
                    "PowerShell runspace CWD probe: pid={pid} client={client} returned no idle filesystem runspace"
                ),
            );
            return None;
        }
        if !Path::new(&directory).is_dir() {
            logging::warn(
                TERMINAL_LOG_COMPONENT,
                format!(
                    "PowerShell runspace CWD probe: pid={pid} returned non-directory path {:?}",
                    directory,
                ),
            );
            return None;
        }

        logging::info(
            TERMINAL_LOG_COMPONENT,
            format!(
                "PowerShell runspace CWD probe: pid={pid} client={client} exact_cwd={:?}",
                directory,
            ),
        );
        Some(directory)
    }
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
        assert_eq!(working_directory(&fake_session(ShellKind::WindowsPowerShell)), None);
    }

    #[test]
    fn helper_accepts_terminal_session_shape_without_mutating_it() {
        let session = fake_session(ShellKind::WindowsPowerShell);
        let before = session.clone();
        let _ = working_directory(&session);
        assert_eq!(session, before);
    }
}
