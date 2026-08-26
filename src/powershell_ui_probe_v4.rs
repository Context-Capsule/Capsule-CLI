use crate::{
    adapters::terminal::{ShellKind, TerminalHost, TerminalSnapshot},
    logging,
};
use std::collections::HashMap;

const TERMINAL_LOG_COMPONENT: &str = "terminal";

#[derive(Debug, Default)]
pub(super) struct ProbeResult {
    pub directories: HashMap<u32, String>,
    pub ordered_pids: Vec<u32>,
}

#[cfg(windows)]
use std::{
    collections::{HashMap as StdHashMap, HashSet},
    ffi::c_void,
    fs,
    mem::{size_of, zeroed},
    os::windows::process::CommandExt,
    path::Path,
    process::{Command, Stdio},
    ptr,
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
const WINDOW_FOCUS_TIMEOUT: Duration = Duration::from_millis(1400);
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
const CASCADIA_WINDOW_CLASS: &str = "CASCADIA_HOSTING_WINDOW_CLASS";

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
#[derive(Clone, Copy)]
struct TerminalWindow {
    hwnd: Hwnd,
    pid: u32,
}

#[cfg(windows)]
#[link(name = "user32")]
unsafe extern "system" {
    fn EnumWindows(callback: EnumWindowsProc, lparam: isize) -> Bool;
    fn IsWindowVisible(hwnd: Hwnd) -> Bool;
    fn IsIconic(hwnd: Hwnd) -> Bool;
    fn IsWindow(hwnd: Hwnd) -> Bool;
    fn GetWindowThreadProcessId(hwnd: Hwnd, process_id: *mut u32) -> u32;
    fn GetForegroundWindow() -> Hwnd;
    fn SetForegroundWindow(hwnd: Hwnd) -> Bool;
    fn AttachThreadInput(id_attach: u32, id_attach_to: u32, attach: Bool) -> Bool;
    fn GetClassNameW(hwnd: Hwnd, class_name: *mut u16, max_count: i32) -> i32;
    fn SendInput(count: u32, inputs: *const Input, size: i32) -> u32;
}

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateToolhelp32Snapshot(flags: u32, process_id: u32) -> Handle;
    fn Process32FirstW(snapshot: Handle, entry: *mut ProcessEntry32W) -> Bool;
    fn Process32NextW(snapshot: Handle, entry: *mut ProcessEntry32W) -> Bool;
    fn CloseHandle(handle: Handle) -> Bool;
    fn GetCurrentThreadId() -> u32;
}

pub(super) fn probe(snapshot: &TerminalSnapshot) -> ProbeResult {
    #[cfg(not(windows))]
    {
        let _ = snapshot;
        ProbeResult::default()
    }

    #[cfg(windows)]
    {
        probe_windows(snapshot)
    }
}

#[cfg(windows)]
fn probe_windows(snapshot: &TerminalSnapshot) -> ProbeResult {
    let live_windows_terminal = snapshot
        .sessions
        .iter()
        .filter(|session| session.host == TerminalHost::WindowsTerminal && session.pid.is_some())
        .collect::<Vec<_>>();

    logging::info(
        TERMINAL_LOG_COMPONENT,
        format!(
            "PowerShell UI CWD probe v4 begin: live_windows_terminal_sessions={}",
            live_windows_terminal.len()
        ),
    );

    if live_windows_terminal.is_empty() {
        return ProbeResult::default();
    }

    if let Some(session) = live_windows_terminal.iter().find(|session| {
        !matches!(session.shell, ShellKind::PowerShell | ShellKind::WindowsPowerShell)
    }) {
        logging::info(
            TERMINAL_LOG_COMPONENT,
            format!(
                "PowerShell UI CWD probe v4 skipped: Windows Terminal contains non-PowerShell shell={} pid={:?}",
                session.shell.as_str(), session.pid,
            ),
        );
        return ProbeResult::default();
    }

    if let Some(session) = live_windows_terminal
        .iter()
        .find(|session| session.foreground_command.is_some())
    {
        logging::info(
            TERMINAL_LOG_COMPONENT,
            format!(
                "PowerShell UI CWD probe v4 skipped: pid={:?} has foreground command {:?}",
                session.pid, session.foreground_command,
            ),
        );
        return ProbeResult::default();
    }

    let expected_pids = live_windows_terminal
        .iter()
        .filter_map(|session| session.pid)
        .collect::<HashSet<_>>();
    if expected_pids.is_empty() {
        return ProbeResult::default();
    }

    let windows = windows_terminal_windows();
    logging::info(
        TERMINAL_LOG_COMPONENT,
        format!(
            "PowerShell UI CWD probe v4: candidate_terminal_windows={} expected_shell_pids={:?}",
            windows.len(), expected_pids
        ),
    );
    if windows.is_empty() {
        logging::warn(
            TERMINAL_LOG_COMPONENT,
            "PowerShell UI CWD probe v4: no visible non-minimized Windows Terminal/Cascadia top-level window was found",
        );
        return ProbeResult::default();
    }

    let original_foreground = unsafe { GetForegroundWindow() };
    let mut result = ProbeResult::default();
    let mut focus_interrupted = false;

    for (window_index, window) in windows.into_iter().enumerate() {
        if result.ordered_pids.len() >= expected_pids.len() {
            break;
        }

        logging::info(
            TERMINAL_LOG_COMPONENT,
            format!(
                "PowerShell UI CWD probe v4: focusing without geometry change window_index={window_index} hwnd={:p} pid={}",
                window.hwnd, window.pid
            ),
        );
        if !focus_window_without_geometry_change(window.hwnd) {
            logging::warn(
                TERMINAL_LOG_COMPONENT,
                format!(
                    "PowerShell UI CWD probe v4: focus failed for window_index={window_index}; window geometry was left untouched"
                ),
            );
            continue;
        }

        let nonce = probe_nonce(window_index);
        let mut step = 0usize;
        let Some(original_probe) = probe_active_pane(window.hwnd, &nonce, step) else {
            logging::warn(
                TERMINAL_LOG_COMPONENT,
                format!(
                    "PowerShell UI CWD probe v4: original active pane did not answer in window_index={window_index}"
                ),
            );
            continue;
        };
        step += 1;
        let original_pid = original_probe.0;
        record_probe_result(
            &mut result,
            &expected_pids,
            original_probe,
            window_index,
            None,
            false,
        );

        let mut original_tab_index = None;
        let mut first_pids_by_tab = HashSet::new();

        for tab_index in 0..MAX_TABS_PER_WINDOW {
            if unsafe { GetForegroundWindow() } != window.hwnd {
                focus_interrupted = true;
                logging::warn(
                    TERMINAL_LOG_COMPONENT,
                    format!(
                        "PowerShell UI CWD probe v4: foreground left Terminal before tab_index={tab_index}; aborting further input"
                    ),
                );
                break;
            }

            if !focus_terminal_tab(tab_index) {
                logging::warn(
                    TERMINAL_LOG_COMPONENT,
                    format!(
                        "PowerShell UI CWD probe v4: wt.exe focus-tab failed at tab_index={tab_index}"
                    ),
                );
                break;
            }
            thread::sleep(Duration::from_millis(100));

            if unsafe { GetForegroundWindow() } != window.hwnd {
                focus_interrupted = true;
                break;
            }

            let Some(first_probe) = probe_active_pane(window.hwnd, &nonce, step) else {
                logging::warn(
                    TERMINAL_LOG_COMPONENT,
                    format!(
                        "PowerShell UI CWD probe v4: tab_index={tab_index} did not answer; stopping tab enumeration"
                    ),
                );
                break;
            };
            step += 1;
            let first_pid = first_probe.0;

            if !first_pids_by_tab.insert(first_pid) {
                logging::info(
                    TERMINAL_LOG_COMPONENT,
                    format!(
                        "PowerShell UI CWD probe v4: tab enumeration complete at tab_index={tab_index} repeated_pid={first_pid}"
                    ),
                );
                break;
            }

            if first_pid == original_pid {
                original_tab_index = Some(tab_index);
            }
            record_probe_result(
                &mut result,
                &expected_pids,
                first_probe,
                window_index,
                Some(tab_index),
                true,
            );

            let mut pane_pids = HashSet::new();
            pane_pids.insert(first_pid);
            for _ in 1..MAX_PANES_PER_TAB {
                if !move_focus_next_pane() {
                    break;
                }
                thread::sleep(Duration::from_millis(70));
                if unsafe { GetForegroundWindow() } != window.hwnd {
                    focus_interrupted = true;
                    break;
                }
                let Some(probe) = probe_active_pane(window.hwnd, &nonce, step) else {
                    break;
                };
                step += 1;
                let pid = probe.0;
                if !pane_pids.insert(pid) {
                    break;
                }
                record_probe_result(
                    &mut result,
                    &expected_pids,
                    probe,
                    window_index,
                    Some(tab_index),
                    true,
                );
            }

            if focus_interrupted || result.ordered_pids.len() >= expected_pids.len() {
                break;
            }
        }

        if focus_interrupted {
            break;
        }

        if let Some(tab_index) = original_tab_index {
            let _ = focus_terminal_tab(tab_index);
            thread::sleep(Duration::from_millis(70));
        }
    }

    if !focus_interrupted
        && !original_foreground.is_null()
        && unsafe { IsWindow(original_foreground) } != 0
    {
        let _ = focus_window_without_geometry_change(original_foreground);
    }

    logging::info(
        TERMINAL_LOG_COMPONENT,
        format!(
            "PowerShell UI CWD probe v4 complete: exact_results={} ordered_pids={:?} expected_sessions={} focus_interrupted={focus_interrupted}",
            result.directories.len(), result.ordered_pids, expected_pids.len(),
        ),
    );
    result
}

#[cfg(windows)]
fn record_probe_result(
    output: &mut ProbeResult,
    expected_pids: &HashSet<u32>,
    probe: (u32, String),
    window_index: usize,
    tab_index: Option<usize>,
    record_order: bool,
) {
    let (pid, directory) = probe;
    if !expected_pids.contains(&pid) {
        logging::warn(
            TERMINAL_LOG_COMPONENT,
            format!(
                "PowerShell UI CWD probe v4: window_index={window_index} tab_index={tab_index:?} answered from unexpected pid={pid}; result ignored"
            ),
        );
        return;
    }

    output.directories.insert(pid, directory.clone());
    if record_order && !output.ordered_pids.contains(&pid) {
        output.ordered_pids.push(pid);
    }

    logging::info(
        TERMINAL_LOG_COMPONENT,
        format!(
            "PowerShell UI CWD probe v4: window_index={window_index} tab_index={tab_index:?} pid={pid} exact_cwd={directory:?} record_order={record_order}"
        ),
    );
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
fn focus_window_without_geometry_change(hwnd: Hwnd) -> bool {
    if hwnd.is_null() || unsafe { IsWindow(hwnd) } == 0 {
        return false;
    }

    // Never restore/minimize/maximize/reposition a window as part of save-time
    // terminal probing. A minimized Terminal is skipped rather than having its
    // layout mutated just so Context Capsule can inspect it.
    if unsafe { IsIconic(hwnd) } != 0 {
        logging::info(
            TERMINAL_LOG_COMPONENT,
            "PowerShell UI CWD probe v4: minimized window skipped to preserve layout",
        );
        return false;
    }

    if unsafe { GetForegroundWindow() } == hwnd {
        return true;
    }

    unsafe {
        SetForegroundWindow(hwnd);
    }
    if wait_until_foreground(hwnd, Duration::from_millis(250)) {
        return true;
    }

    let current_thread = unsafe { GetCurrentThreadId() };
    let foreground = unsafe { GetForegroundWindow() };
    let foreground_thread = if foreground.is_null() {
        0
    } else {
        unsafe { GetWindowThreadProcessId(foreground, ptr::null_mut()) }
    };
    let target_thread = unsafe { GetWindowThreadProcessId(hwnd, ptr::null_mut()) };

    let attached_foreground = foreground_thread != 0 && foreground_thread != current_thread;
    let attached_target = target_thread != 0
        && target_thread != current_thread
        && target_thread != foreground_thread;

    if attached_foreground {
        unsafe {
            AttachThreadInput(current_thread, foreground_thread, 1);
        }
    }
    if attached_target {
        unsafe {
            AttachThreadInput(current_thread, target_thread, 1);
        }
    }

    unsafe {
        SetForegroundWindow(hwnd);
    }

    if attached_target {
        unsafe {
            AttachThreadInput(current_thread, target_thread, 0);
        }
    }
    if attached_foreground {
        unsafe {
            AttachThreadInput(current_thread, foreground_thread, 0);
        }
    }

    wait_until_foreground(hwnd, WINDOW_FOCUS_TIMEOUT)
}

#[cfg(windows)]
fn wait_until_foreground(hwnd: Hwnd, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
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
    let index_text = index.to_string();
    run_wt_command(&["-w", "0", "focus-tab", "-t", &index_text])
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
                format!("PowerShell UI CWD probe v4: failed to start wt.exe: {error}"),
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
                return false;
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
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
    process_names: StdHashMap<u32, String>,
    windows: Vec<TerminalWindow>,
}

#[cfg(windows)]
unsafe extern "system" fn enum_window(hwnd: Hwnd, lparam: isize) -> Bool {
    let state = unsafe { &mut *(lparam as *mut WindowEnumeration) };
    if unsafe { IsWindowVisible(hwnd) } == 0 || unsafe { IsIconic(hwnd) } != 0 {
        return 1;
    }

    let class_name = window_class_name(hwnd);
    let mut pid = 0u32;
    unsafe {
        GetWindowThreadProcessId(hwnd, &mut pid);
    }
    let process_name = state
        .process_names
        .get(&pid)
        .map(String::as_str)
        .unwrap_or_default();

    let is_cascadia = class_name.eq_ignore_ascii_case(CASCADIA_WINDOW_CLASS);
    let is_terminal_process = process_name.eq_ignore_ascii_case("WindowsTerminal.exe")
        || process_name.eq_ignore_ascii_case("OpenConsole.exe");

    if is_cascadia || is_terminal_process {
        state.windows.push(TerminalWindow { hwnd, pid });
    }
    1
}

#[cfg(windows)]
fn window_class_name(hwnd: Hwnd) -> String {
    let mut buffer = [0u16; 128];
    let length = unsafe { GetClassNameW(hwnd, buffer.as_mut_ptr(), buffer.len() as i32) };
    if length <= 0 {
        return String::new();
    }
    String::from_utf16_lossy(&buffer[..length as usize])
}

#[cfg(windows)]
fn windows_terminal_windows() -> Vec<TerminalWindow> {
    let process_names = process_names();
    let mut state = WindowEnumeration {
        process_names,
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
fn process_names() -> StdHashMap<u32, String> {
    let mut result = StdHashMap::new();
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
        result.insert(entry.process_id, name);
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
        assert!(command.contains("$PID+'.txt'"));
        assert!(command.contains("[string]$PWD.Path"));
        assert!(!command.contains("Clear-Host"));
    }

    #[test]
    fn initial_active_probe_does_not_define_tab_order() {
        let expected = [10_u32, 20_u32].into_iter().collect::<HashSet<_>>();
        let mut output = ProbeResult::default();
        record_probe_result(
            &mut output,
            &expected,
            (20, "C:\\second".to_owned()),
            0,
            None,
            false,
        );
        record_probe_result(
            &mut output,
            &expected,
            (10, "C:\\first".to_owned()),
            0,
            Some(0),
            true,
        );
        record_probe_result(
            &mut output,
            &expected,
            (20, "C:\\second".to_owned()),
            0,
            Some(1),
            true,
        );
        assert_eq!(output.ordered_pids, vec![10, 20]);
    }

    #[test]
    fn probe_source_contains_no_geometry_mutation_api() {
        let source = include_str!("powershell_ui_probe_v4.rs");
        assert!(!source.contains("ShowWindow("));
        assert!(!source.contains("SW_RESTORE"));
        assert!(!source.contains("SetWindowPos("));
        assert!(!source.contains("MoveWindow("));
    }
}
