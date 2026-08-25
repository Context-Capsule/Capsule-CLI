use crate::{
    adapters::terminal::{ShellKind, TerminalHost, TerminalSnapshot},
    logging,
};
use std::collections::HashMap;

const TERMINAL_LOG_COMPONENT: &str = "terminal";

#[cfg(windows)]
use std::{
    collections::HashSet,
    ffi::c_void,
    fs,
    mem::{size_of, zeroed},
    os::windows::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[cfg(windows)]
type Hwnd = *mut c_void;
#[cfg(windows)]
type Handle = *mut c_void;
#[cfg(windows)]
type Bool = i32;
#[cfg(windows)]
type EnumWindowsProc = Option<unsafe extern "system" fn(Hwnd, isize) -> Bool>;

#[cfg(windows)]
const TH32CS_SNAPPROCESS: u32 = 0x0000_0002;
#[cfg(windows)]
const MAX_PATH: usize = 260;
#[cfg(windows)]
const INPUT_KEYBOARD: u32 = 1;
#[cfg(windows)]
const KEYEVENTF_KEYUP: u32 = 0x0002;
#[cfg(windows)]
const KEYEVENTF_UNICODE: u32 = 0x0004;
#[cfg(windows)]
const VK_RETURN: u16 = 0x0D;
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
#[cfg(windows)]
const WINDOW_FOCUS_TIMEOUT: Duration = Duration::from_millis(700);
#[cfg(windows)]
const WINDOW_FOCUS_POLL: Duration = Duration::from_millis(20);
#[cfg(windows)]
const PROBE_RESULT_TIMEOUT: Duration = Duration::from_secs(2);
#[cfg(windows)]
const PROBE_RESULT_POLL: Duration = Duration::from_millis(25);
#[cfg(windows)]
const WT_COMMAND_TIMEOUT: Duration = Duration::from_secs(2);
#[cfg(windows)]
const MAX_TABS_PER_WINDOW: usize = 64;
#[cfg(windows)]
const MAX_PANES_PER_TAB: usize = 32;

#[cfg(windows)]
#[repr(C)]
#[derive(Clone, Copy)]
struct MouseInput {
    dx: i32,
    dy: i32,
    mouse_data: u32,
    flags: u32,
    time: u32,
    extra_info: usize,
}

#[cfg(windows)]
#[repr(C)]
#[derive(Clone, Copy)]
struct KeyboardInput {
    virtual_key: u16,
    scan_code: u16,
    flags: u32,
    time: u32,
    extra_info: usize,
}

#[cfg(windows)]
#[repr(C)]
#[derive(Clone, Copy)]
struct HardwareInput {
    message: u32,
    parameter_low: u16,
    parameter_high: u16,
}

#[cfg(windows)]
#[repr(C)]
union InputData {
    mouse: MouseInput,
    keyboard: KeyboardInput,
    hardware: HardwareInput,
}

#[cfg(windows)]
#[repr(C)]
struct Input {
    input_type: u32,
    data: InputData,
}

#[cfg(windows)]
#[repr(C)]
struct ProcessEntry32W {
    size: u32,
    usage: u32,
    process_id: u32,
    default_heap_id: usize,
    module_id: u32,
    threads: u32,
    parent_process_id: u32,
    priority_base: i32,
    flags: u32,
    executable_file: [u16; MAX_PATH],
}

#[cfg(windows)]
#[link(name = "user32")]
unsafe extern "system" {
    fn EnumWindows(callback: EnumWindowsProc, lparam: isize) -> Bool;
    fn IsWindowVisible(hwnd: Hwnd) -> Bool;
    fn IsIconic(hwnd: Hwnd) -> Bool;
    fn GetWindowThreadProcessId(hwnd: Hwnd, process_id: *mut u32) -> u32;
    fn GetForegroundWindow() -> Hwnd;
    fn SetForegroundWindow(hwnd: Hwnd) -> Bool;
    fn IsWindow(hwnd: Hwnd) -> Bool;
    fn SendInput(count: u32, inputs: *const Input, size: i32) -> u32;
}

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateToolhelp32Snapshot(flags: u32, process_id: u32) -> Handle;
    fn Process32FirstW(snapshot: Handle, entry: *mut ProcessEntry32W) -> Bool;
    fn Process32NextW(snapshot: Handle, entry: *mut ProcessEntry32W) -> Bool;
    fn CloseHandle(handle: Handle) -> Bool;
}

/// Last-resort exact PowerShell CWD capture for Windows Terminal.
///
/// This is intentionally conservative. It only runs when every live Windows
/// Terminal session visible to Context Capsule is an idle PowerShell session.
/// The probe focuses the real Terminal window, selects each tab/pane, types a
/// short PowerShell command that writes `$PWD.Path` to a unique temp file, and
/// immediately erases only the probe line from the visible terminal buffer.
/// The temp filename contains the answering PowerShell PID, so results map back
/// to the already discovered TerminalSession without guessing tab titles.
///
/// If focus changes unexpectedly, SendInput is blocked, a command is busy, or
/// any probe does not answer, the automation aborts instead of continuing to
/// type blindly.
pub(super) fn working_directories(snapshot: &TerminalSnapshot) -> HashMap<u32, String> {
    #[cfg(not(windows))]
    {
        let _ = snapshot;
        HashMap::new()
    }

    #[cfg(windows)]
    {
        working_directories_windows(snapshot)
    }
}

#[cfg(windows)]
fn working_directories_windows(snapshot: &TerminalSnapshot) -> HashMap<u32, String> {
    let live_windows_terminal = snapshot
        .sessions
        .iter()
        .filter(|session| session.host == TerminalHost::WindowsTerminal && session.pid.is_some())
        .collect::<Vec<_>>();

    if live_windows_terminal.is_empty() {
        return HashMap::new();
    }

    if let Some(session) = live_windows_terminal.iter().find(|session| {
        !matches!(session.shell, ShellKind::PowerShell | ShellKind::WindowsPowerShell)
    }) {
        logging::info(
            TERMINAL_LOG_COMPONENT,
            format!(
                "PowerShell UI CWD probe skipped: Windows Terminal also contains non-PowerShell shell={} pid={:?}; refusing to type a PowerShell command into an unknown tab",
                session.shell.as_str(),
                session.pid,
            ),
        );
        return HashMap::new();
    }

    if let Some(session) = live_windows_terminal
        .iter()
        .find(|session| session.foreground_command.is_some())
    {
        logging::info(
            TERMINAL_LOG_COMPONENT,
            format!(
                "PowerShell UI CWD probe skipped: pid={:?} has active foreground command {:?}; refusing to inject input into a busy terminal",
                session.pid,
                session.foreground_command,
            ),
        );
        return HashMap::new();
    }

    let expected_pids = live_windows_terminal
        .iter()
        .filter_map(|session| session.pid)
        .collect::<HashSet<_>>();
    if expected_pids.is_empty() {
        return HashMap::new();
    }

    let windows = windows_terminal_windows();
    if windows.is_empty() {
        logging::warn(
            TERMINAL_LOG_COMPONENT,
            "PowerShell UI CWD probe: no visible non-minimized Windows Terminal top-level window was found",
        );
        return HashMap::new();
    }

    let original_foreground = unsafe { GetForegroundWindow() };
    let mut results = HashMap::new();
    let mut user_focus_changed = false;

    for (window_index, hwnd) in windows.into_iter().enumerate() {
        if results.len() >= expected_pids.len() {
            break;
        }
        if !focus_window(hwnd) {
            logging::warn(
                TERMINAL_LOG_COMPONENT,
                format!(
                    "PowerShell UI CWD probe: could not focus Windows Terminal window index={window_index}; skipped without sending input"
                ),
            );
            continue;
        }

        let nonce = probe_nonce(window_index);
        let mut step = 0usize;
        let Some(original_probe) = probe_active_pane(hwnd, &nonce, step) else {
            logging::warn(
                TERMINAL_LOG_COMPONENT,
                format!(
                    "PowerShell UI CWD probe: active pane in window index={window_index} did not answer; aborting this window"
                ),
            );
            continue;
        };
        step += 1;
        let original_pid = original_probe.0;
        record_probe_result(&mut results, &expected_pids, original_probe, window_index, None);

        let mut original_tab_index = None;
        let mut first_pids_by_tab = HashSet::new();

        for tab_index in 0..MAX_TABS_PER_WINDOW {
            if unsafe { GetForegroundWindow() } != hwnd {
                user_focus_changed = true;
                logging::warn(
                    TERMINAL_LOG_COMPONENT,
                    format!(
                        "PowerShell UI CWD probe: foreground changed before tab {tab_index} in window index={window_index}; aborting all further input"
                    ),
                );
                break;
            }

            if !focus_terminal_tab(tab_index) {
                logging::warn(
                    TERMINAL_LOG_COMPONENT,
                    format!(
                        "PowerShell UI CWD probe: wt.exe could not focus tab index={tab_index} in window index={window_index}"
                    ),
                );
                break;
            }
            thread::sleep(Duration::from_millis(70));
            if unsafe { GetForegroundWindow() } != hwnd {
                user_focus_changed = true;
                logging::warn(
                    TERMINAL_LOG_COMPONENT,
                    "PowerShell UI CWD probe: focused application changed after Windows Terminal tab selection; aborting all further input",
                );
                break;
            }

            let Some(first_probe) = probe_active_pane(hwnd, &nonce, step) else {
                logging::warn(
                    TERMINAL_LOG_COMPONENT,
                    format!(
                        "PowerShell UI CWD probe: tab index={tab_index} did not answer; stopping enumeration of window index={window_index}"
                    ),
                );
                break;
            };
            step += 1;
            let first_pid = first_probe.0;

            // focus-tab leaves the current tab unchanged when an out-of-range
            // index is requested. Seeing the same first pane PID again therefore
            // marks the end of the tab list without relying on a configurable
            // Ctrl+Tab key binding.
            if !first_pids_by_tab.insert(first_pid) {
                break;
            }

            if first_pid == original_pid {
                original_tab_index = Some(tab_index);
            }
            record_probe_result(
                &mut results,
                &expected_pids,
                first_probe,
                window_index,
                Some(tab_index),
            );

            // Preserve split-pane support as well. move-focus nextInOrder stays
            // inside the selected tab's pane tree. We cycle until its first pane
            // PID repeats, which also restores that tab's original active pane.
            let mut pane_pids = HashSet::new();
            pane_pids.insert(first_pid);
            for _ in 1..MAX_PANES_PER_TAB {
                if !move_focus_next_pane() {
                    break;
                }
                thread::sleep(Duration::from_millis(50));
                if unsafe { GetForegroundWindow() } != hwnd {
                    user_focus_changed = true;
                    break;
                }
                let Some(probe) = probe_active_pane(hwnd, &nonce, step) else {
                    break;
                };
                step += 1;
                let pid = probe.0;
                if !pane_pids.insert(pid) {
                    break;
                }
                record_probe_result(
                    &mut results,
                    &expected_pids,
                    probe,
                    window_index,
                    Some(tab_index),
                );
            }
            if user_focus_changed {
                break;
            }
        }

        if user_focus_changed {
            break;
        }

        if let Some(tab_index) = original_tab_index {
            let _ = focus_terminal_tab(tab_index);
            thread::sleep(Duration::from_millis(50));
        }
    }

    if !user_focus_changed
        && !original_foreground.is_null()
        && unsafe { IsWindow(original_foreground) } != 0
    {
        let _ = unsafe { SetForegroundWindow(original_foreground) };
    }

    logging::info(
        TERMINAL_LOG_COMPONENT,
        format!(
            "PowerShell UI CWD probe complete: exact_results={} expected_sessions={} focus_interrupted={user_focus_changed}",
            results.len(),
            expected_pids.len(),
        ),
    );
    results
}

#[cfg(windows)]
fn record_probe_result(
    output: &mut HashMap<u32, String>,
    expected_pids: &HashSet<u32>,
    probe: (u32, String),
    window_index: usize,
    tab_index: Option<usize>,
) {
    let (pid, directory) = probe;
    if !expected_pids.contains(&pid) {
        logging::warn(
            TERMINAL_LOG_COMPONENT,
            format!(
                "PowerShell UI CWD probe: window_index={window_index} tab_index={tab_index:?} answered from unexpected pid={pid}; result ignored"
            ),
        );
        return;
    }
    logging::info(
        TERMINAL_LOG_COMPONENT,
        format!(
            "PowerShell UI CWD probe: window_index={window_index} tab_index={tab_index:?} pid={pid} exact_cwd={directory:?}"
        ),
    );
    output.insert(pid, directory);
}

#[cfg(windows)]
fn probe_active_pane(hwnd: Hwnd, nonce: &str, step: usize) -> Option<(u32, String)> {
    if unsafe { GetForegroundWindow() } != hwnd {
        return None;
    }

    let prefix = format!("context-capsule-cwd-{nonce}-{step}-");
    let command = probe_command(nonce, step);
    if !send_unicode_text(&command) || unsafe { GetForegroundWindow() } != hwnd {
        return None;
    }
    if !send_virtual_key(VK_RETURN) {
        return None;
    }

    wait_for_probe_file(&std::env::temp_dir(), &prefix, PROBE_RESULT_TIMEOUT)
}

fn probe_command(nonce: &str, step: usize) -> String {
    // The scriptblock gives helper variables child scope. The final VT sequence
    // erases only the submitted probe line/current blank line, not scrollback.
    format!(
        "&{{$f=Join-Path $env:TEMP ('context-capsule-cwd-{nonce}-{step}-'+$PID+'.txt');[IO.File]::WriteAllText($f,[string]$PWD.Path);$e=[char]27;[Console]::Write($e+'[1A'+$e+'[2K'+$e+'[1G'+$e+'[1B'+$e+'[2K'+$e+'[1G')}}"
    )
}

#[cfg(windows)]
fn wait_for_probe_file(directory: &Path, prefix: &str, timeout: Duration) -> Option<(u32, String)> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(entries) = fs::read_dir(directory) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                let Some(rest) = name.strip_prefix(prefix) else {
                    continue;
                };
                let Some(pid_text) = rest.strip_suffix(".txt") else {
                    continue;
                };
                let Ok(pid) = pid_text.parse::<u32>() else {
                    continue;
                };
                let path = entry.path();
                let content = fs::read_to_string(&path).ok();
                let _ = fs::remove_file(&path);
                if let Some(directory) = content
                    .map(|value| value.trim().to_owned())
                    .filter(|value| !value.is_empty())
                    .filter(|value| Path::new(value).is_dir())
                {
                    return Some((pid, directory));
                }
            }
        }
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(PROBE_RESULT_POLL);
    }
}

#[cfg(windows)]
fn focus_window(hwnd: Hwnd) -> bool {
    if hwnd.is_null() || unsafe { IsWindow(hwnd) } == 0 || unsafe { IsIconic(hwnd) } != 0 {
        return false;
    }
    if unsafe { GetForegroundWindow() } == hwnd {
        return true;
    }
    let _ = unsafe { SetForegroundWindow(hwnd) };
    let deadline = Instant::now() + WINDOW_FOCUS_TIMEOUT;
    while Instant::now() < deadline {
        if unsafe { GetForegroundWindow() } == hwnd {
            return true;
        }
        thread::sleep(WINDOW_FOCUS_POLL);
    }
    false
}

#[cfg(windows)]
fn focus_terminal_tab(index: usize) -> bool {
    run_wt_command(&[
        "-w",
        "0",
        "focus-tab",
        "-t",
        &index.to_string(),
    ])
}

#[cfg(windows)]
fn move_focus_next_pane() -> bool {
    run_wt_command(&["-w", "0", "move-focus", "nextInOrder"])
}

#[cfg(windows)]
fn run_wt_command(args: &[&str]) -> bool {
    let mut child = match Command::new("wt.exe")
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            logging::warn(
                TERMINAL_LOG_COMPONENT,
                format!("PowerShell UI CWD probe: failed to start wt.exe: {error}"),
            );
            return false;
        }
    };

    let deadline = Instant::now() + WT_COMMAND_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                logging::warn(
                    TERMINAL_LOG_COMPONENT,
                    "PowerShell UI CWD probe: wt.exe command timed out",
                );
                return false;
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                logging::warn(
                    TERMINAL_LOG_COMPONENT,
                    format!("PowerShell UI CWD probe: wt.exe wait failed: {error}"),
                );
                return false;
            }
        }
    }
}

#[cfg(windows)]
fn send_unicode_text(text: &str) -> bool {
    let mut inputs = Vec::with_capacity(text.encode_utf16().count() * 2);
    for code_unit in text.encode_utf16() {
        inputs.push(keyboard_input(0, code_unit, KEYEVENTF_UNICODE));
        inputs.push(keyboard_input(
            0,
            code_unit,
            KEYEVENTF_UNICODE | KEYEVENTF_KEYUP,
        ));
    }
    send_inputs(&inputs)
}

#[cfg(windows)]
fn send_virtual_key(virtual_key: u16) -> bool {
    let inputs = [
        keyboard_input(virtual_key, 0, 0),
        keyboard_input(virtual_key, 0, KEYEVENTF_KEYUP),
    ];
    send_inputs(&inputs)
}

#[cfg(windows)]
fn keyboard_input(virtual_key: u16, scan_code: u16, flags: u32) -> Input {
    Input {
        input_type: INPUT_KEYBOARD,
        data: InputData {
            keyboard: KeyboardInput {
                virtual_key,
                scan_code,
                flags,
                time: 0,
                extra_info: 0,
            },
        },
    }
}

#[cfg(windows)]
fn send_inputs(inputs: &[Input]) -> bool {
    if inputs.is_empty() {
        return true;
    }
    unsafe {
        SendInput(
            inputs.len() as u32,
            inputs.as_ptr(),
            size_of::<Input>() as i32,
        ) == inputs.len() as u32
    }
}

#[cfg(windows)]
fn probe_nonce(window_index: usize) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!("{}-{window_index}-{nanos}", std::process::id())
}

#[cfg(windows)]
struct WindowEnumeration {
    terminal_pids: HashSet<u32>,
    windows: Vec<Hwnd>,
}

#[cfg(windows)]
unsafe extern "system" fn enum_window(hwnd: Hwnd, lparam: isize) -> Bool {
    let state = unsafe { &mut *(lparam as *mut WindowEnumeration) };
    if unsafe { IsWindowVisible(hwnd) } == 0 || unsafe { IsIconic(hwnd) } != 0 {
        return 1;
    }
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
    if pid != 0 && state.terminal_pids.contains(&pid) {
        state.windows.push(hwnd);
    }
    1
}

#[cfg(windows)]
fn windows_terminal_windows() -> Vec<Hwnd> {
    let terminal_pids = windows_terminal_process_ids();
    if terminal_pids.is_empty() {
        return Vec::new();
    }
    let mut state = WindowEnumeration {
        terminal_pids,
        windows: Vec::new(),
    };
    unsafe {
        EnumWindows(
            Some(enum_window),
            &mut state as *mut WindowEnumeration as isize,
        );
    }
    state.windows
}

#[cfg(windows)]
fn windows_terminal_process_ids() -> HashSet<u32> {
    let mut result = HashSet::new();
    let raw = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if raw.is_null() || raw as isize == -1 {
        return result;
    }
    let mut entry: ProcessEntry32W = unsafe { zeroed() };
    entry.size = size_of::<ProcessEntry32W>() as u32;
    let mut ok = unsafe { Process32FirstW(raw, &mut entry) } != 0;
    while ok {
        let length = entry
            .executable_file
            .iter()
            .position(|value| *value == 0)
            .unwrap_or(entry.executable_file.len());
        let name = String::from_utf16_lossy(&entry.executable_file[..length]);
        if name.eq_ignore_ascii_case("WindowsTerminal.exe") {
            result.insert(entry.process_id);
        }
        ok = unsafe { Process32NextW(raw, &mut entry) } != 0;
    }
    unsafe {
        CloseHandle(raw);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_command_writes_pid_scoped_file_and_requests_real_pwd() {
        let command = probe_command("123-0-456", 7);
        assert!(command.contains("context-capsule-cwd-123-0-456-7-"));
        assert!(command.contains("$PID+'.txt'"));
        assert!(command.contains("[string]$PWD.Path"));
        assert!(!command.contains("Clear-Host"));
    }

    #[test]
    fn probe_command_has_no_user_supplied_directory_or_command_replay() {
        let command = probe_command("10-1-20", 3);
        assert!(!command.contains("Set-Location"));
        assert!(!command.contains("cd "));
        assert!(!command.contains("Invoke-Expression"));
    }
}
