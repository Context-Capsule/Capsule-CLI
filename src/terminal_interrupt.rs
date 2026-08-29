use std::process::{Command, ExitCode, Stdio};

#[cfg(windows)]
use std::{
    ffi::c_void,
    os::windows::process::CommandExt,
    thread,
    time::Duration,
};

const HELPER_COMMAND: &str = "__capsule-console-control";

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

fn caller_ancestry_script(caller_pid: u32) -> String {
    format!(
        r#"$ErrorActionPreference='Stop'; $current=[uint32]{caller_pid}; $seen=@{{}}; $items=if(Get-Command Get-CimInstance -ErrorAction SilentlyContinue){{Get-CimInstance -ClassName Win32_Process}}{{Get-WmiObject -Class Win32_Process}}; $byPid=@{{}}; foreach($item in $items){{$pidKey=[uint32]$item.ProcessId; $byPid[$pidKey]=$item}}; for($i=0;$i -lt 32;$i++){{if($seen.ContainsKey($current)){{break}}; $seen[$current]=$true; $p=$byPid[$current]; if(-not $p){{break}}; $name=([string]$p.Name).ToLowerInvariant(); if($name -in @('pwsh.exe','powershell.exe','cmd.exe','bash.exe','zsh.exe','fish.exe','nu.exe','nushell.exe','sh.exe')){{[Console]::WriteLine([uint32]$p.ProcessId); break}}; $current=[uint32]$p.ParentProcessId}}"#
    )
}

pub fn caller_shell_pid(caller_pid: u32) -> Result<Option<u32>, String> {
    #[cfg(windows)]
    {
        let script = caller_ancestry_script(caller_pid);
        let output = Command::new("powershell.exe")
            .args(["-NoLogo", "-NoProfile", "-NonInteractive", "-Command", &script])
            .creation_flags(CREATE_NO_WINDOW)
            .stdin(Stdio::null())
            .output()
            .map_err(|error| format!("could not inspect caller process ancestry: {error}"))?;
        if !output.status.success() {
            return Err(format!(
                "could not inspect caller process ancestry: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        let text = String::from_utf8_lossy(&output.stdout);
        return match text.trim().parse::<u32>() {
            Ok(pid) if pid > 0 => Ok(Some(pid)),
            _ => Ok(None),
        };
    }

    #[cfg(not(windows))]
    {
        let _ = caller_pid;
        Ok(None)
    }
}

pub fn send_ctrl_c(shell_pid: u32) -> Result<(), String> {
    run_helper("ctrl-c", shell_pid, None)
}

pub fn send_text(shell_pid: u32, command: &str) -> Result<(), String> {
    if command.trim().is_empty() {
        return Err("refusing to send an empty terminal command".to_owned());
    }
    if command.chars().any(char::is_control) {
        return Err("refusing to send terminal control characters as restart text".to_owned());
    }
    run_helper("send-text", shell_pid, Some(command))
}

fn run_helper(operation: &str, shell_pid: u32, text: Option<&str>) -> Result<(), String> {
    #[cfg(windows)]
    {
        let executable = std::env::current_exe()
            .map_err(|error| format!("could not resolve Context Capsule worker: {error}"))?;
        let mut command = Command::new(executable);
        command
            .arg(HELPER_COMMAND)
            .arg(operation)
            .arg(shell_pid.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .creation_flags(CREATE_NO_WINDOW);
        if let Some(text) = text {
            command.arg(text);
        }
        let output = command
            .output()
            .map_err(|error| format!("could not start terminal control helper: {error}"))?;
        if output.status.success() {
            return Ok(());
        }
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(if detail.is_empty() {
            format!("terminal control helper failed for shell PID {shell_pid}")
        } else {
            detail
        });
    }

    #[cfg(not(windows))]
    {
        let _ = (operation, shell_pid, text);
        Err("direct terminal interruption/replay is currently implemented for Windows terminals only"
            .to_owned())
    }
}

pub fn helper(arguments: &[String]) -> ExitCode {
    if arguments.first().map(String::as_str) != Some(HELPER_COMMAND) {
        eprintln!("invalid terminal control helper invocation");
        return ExitCode::from(2);
    }
    let operation = arguments.get(1).map(String::as_str);
    let pid = arguments
        .get(2)
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|pid| *pid > 0);
    let Some(pid) = pid else {
        eprintln!("terminal control helper requires a valid shell PID");
        return ExitCode::from(2);
    };

    #[cfg(windows)]
    let result = match operation {
        Some("ctrl-c") if arguments.len() == 3 => ctrl_c_attached(pid),
        Some("send-text") if arguments.len() == 4 => write_console_input(pid, &arguments[3]),
        _ => Err("invalid terminal control helper operation".to_owned()),
    };

    #[cfg(not(windows))]
    let result: Result<(), String> = {
        let _ = operation;
        Err("terminal control helper is unavailable on this platform".to_owned())
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::from(1)
        }
    }
}

#[cfg(windows)]
type Handle = *mut c_void;

#[cfg(windows)]
const CTRL_C_EVENT: u32 = 0;
#[cfg(windows)]
const KEY_EVENT: u16 = 0x0001;
#[cfg(windows)]
const VK_RETURN: u16 = 0x000D;
#[cfg(windows)]
const GENERIC_READ: u32 = 0x8000_0000;
#[cfg(windows)]
const GENERIC_WRITE: u32 = 0x4000_0000;
#[cfg(windows)]
const FILE_SHARE_READ: u32 = 0x0000_0001;
#[cfg(windows)]
const FILE_SHARE_WRITE: u32 = 0x0000_0002;
#[cfg(windows)]
const OPEN_EXISTING: u32 = 3;
#[cfg(windows)]
const INVALID_HANDLE_VALUE: Handle = (-1_isize) as Handle;
#[cfg(windows)]
const CONSOLE_INPUT_NAME: [u16; 7] = [
    b'C' as u16,
    b'O' as u16,
    b'N' as u16,
    b'I' as u16,
    b'N' as u16,
    b'$' as u16,
    0,
];

#[cfg(windows)]
#[repr(C)]
#[derive(Clone, Copy)]
union InputChar {
    unicode_char: u16,
    ascii_char: u8,
}

#[cfg(windows)]
#[repr(C)]
#[derive(Clone, Copy)]
struct KeyEventRecord {
    key_down: i32,
    repeat_count: u16,
    virtual_key_code: u16,
    virtual_scan_code: u16,
    character: InputChar,
    control_key_state: u32,
}

#[cfg(windows)]
#[repr(C)]
#[derive(Clone, Copy)]
union InputEvent {
    key_event: KeyEventRecord,
}

#[cfg(windows)]
#[repr(C)]
#[derive(Clone, Copy)]
struct InputRecord {
    event_type: u16,
    event: InputEvent,
}

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn FreeConsole() -> i32;
    fn AttachConsole(process_id: u32) -> i32;
    fn GenerateConsoleCtrlEvent(ctrl_event: u32, process_group_id: u32) -> i32;
    fn SetConsoleCtrlHandler(
        handler: Option<unsafe extern "system" fn(u32) -> i32>,
        add: i32,
    ) -> i32;
    fn CreateFileW(
        file_name: *const u16,
        desired_access: u32,
        share_mode: u32,
        security_attributes: *mut c_void,
        creation_disposition: u32,
        flags_and_attributes: u32,
        template_file: Handle,
    ) -> Handle;
    fn CloseHandle(object: Handle) -> i32;
    fn WriteConsoleInputW(
        console_input: Handle,
        buffer: *const InputRecord,
        length: u32,
        written: *mut u32,
    ) -> i32;
}

#[cfg(windows)]
fn attach_console(pid: u32) -> Result<(), String> {
    unsafe {
        let _ = FreeConsole();
        if AttachConsole(pid) == 0 {
            return Err(format!("could not attach to terminal shell PID {pid}"));
        }
    }
    Ok(())
}

#[cfg(windows)]
fn ctrl_c_attached(pid: u32) -> Result<(), String> {
    attach_console(pid)?;
    let result = unsafe {
        let _ = SetConsoleCtrlHandler(None, 1);
        let generated = GenerateConsoleCtrlEvent(CTRL_C_EVENT, 0);
        if generated != 0 {
            thread::sleep(Duration::from_millis(150));
        }
        let _ = FreeConsole();
        generated
    };
    if result == 0 {
        Err(format!("could not deliver Ctrl+C to terminal shell PID {pid}"))
    } else {
        Ok(())
    }
}

#[cfg(windows)]
fn key_record(unit: u16, down: bool) -> InputRecord {
    InputRecord {
        event_type: KEY_EVENT,
        event: InputEvent {
            key_event: KeyEventRecord {
                key_down: i32::from(down),
                repeat_count: 1,
                virtual_key_code: if unit == b'\r' as u16 { VK_RETURN } else { 0 },
                virtual_scan_code: 0,
                character: InputChar { unicode_char: unit },
                control_key_state: 0,
            },
        },
    }
}

#[cfg(windows)]
fn console_input_records(command: &str) -> Vec<InputRecord> {
    let mut records = Vec::new();
    for unit in command.encode_utf16().chain(std::iter::once(b'\r' as u16)) {
        records.push(key_record(unit, true));
        records.push(key_record(unit, false));
    }
    records
}

#[cfg(windows)]
fn open_console_input(pid: u32) -> Result<Handle, String> {
    let handle = unsafe {
        CreateFileW(
            CONSOLE_INPUT_NAME.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null_mut(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        let error = std::io::Error::last_os_error();
        return Err(format!(
            "could not open CONIN$ for terminal shell PID {pid}: {error}"
        ));
    }
    Ok(handle)
}

#[cfg(windows)]
fn write_console_input(pid: u32, command: &str) -> Result<(), String> {
    attach_console(pid)?;
    let handle = match open_console_input(pid) {
        Ok(handle) => handle,
        Err(error) => {
            unsafe {
                let _ = FreeConsole();
            }
            return Err(error);
        }
    };

    let records = console_input_records(command);
    let mut written = 0_u32;
    let ok = unsafe {
        WriteConsoleInputW(
            handle,
            records.as_ptr(),
            records.len().min(u32::MAX as usize) as u32,
            &mut written,
        )
    };
    let write_error = if ok == 0 {
        Some(std::io::Error::last_os_error())
    } else {
        None
    };
    unsafe {
        let _ = CloseHandle(handle);
        let _ = FreeConsole();
    }

    if let Some(error) = write_error {
        return Err(format!(
            "could not write restart command to terminal shell PID {pid} using CONIN$: {error} ({written}/{} input events written)",
            records.len()
        ));
    }
    if written as usize != records.len() {
        return Err(format!(
            "could not write complete restart command to terminal shell PID {pid} using CONIN$ ({written}/{} input events written)",
            records.len()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caller_ancestry_script_uses_inventory_lookup_without_cim_filter_escaping() {
        let script = caller_ancestry_script(4242);
        assert!(script.contains("$current=[uint32]4242"));
        assert!(script.contains("Get-CimInstance -ClassName Win32_Process"));
        assert!(script.contains("Get-WmiObject -Class Win32_Process"));
        assert!(script.contains("$pidKey=[uint32]$item.ProcessId"));
        assert!(script.contains("$p=$byPid[$current]"));
        assert!(!script.contains("-Filter"));
        assert!(!script.contains("\\\""));
    }

    #[test]
    fn helper_rejects_bad_pid_without_touching_a_console() {
        assert_eq!(
            helper(&[
                HELPER_COMMAND.to_owned(),
                "ctrl-c".to_owned(),
                "0".to_owned(),
            ]),
            ExitCode::from(2)
        );
    }

    #[test]
    fn send_text_rejects_control_characters_before_spawning_helper() {
        assert!(send_text(42, "npm start\nsecond").is_err());
    }

    #[cfg(windows)]
    #[test]
    fn restart_text_encodes_key_down_up_pairs_and_enter() {
        let records = console_input_records("ab");
        assert_eq!(records.len(), 6);
        unsafe {
            assert_eq!(records[0].event.key_event.key_down, 1);
            assert_eq!(records[1].event.key_event.key_down, 0);
            assert_eq!(records[4].event.key_event.virtual_key_code, VK_RETURN);
            assert_eq!(records[5].event.key_event.virtual_key_code, VK_RETURN);
            assert_eq!(records[4].event.key_event.character.unicode_char, b'\r' as u16);
        }
    }

    #[cfg(windows)]
    #[test]
    fn console_input_name_is_null_terminated_conin() {
        assert_eq!(CONSOLE_INPUT_NAME, [67, 79, 78, 73, 78, 36, 0]);
    }
}
