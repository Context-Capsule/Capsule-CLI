use crate::adapters::terminal::{
    TerminalHost, TerminalSession, TerminalSnapshot, TerminalSource,
};
use serde_json::Value;
use std::{
    collections::HashSet,
    ffi::c_void,
    fs,
    mem::{size_of, zeroed},
    ptr::null_mut,
};

/// Correct Windows Terminal's persisted-layout/runtime merge using the stable
/// pane GUID that Terminal exposes in both state.json (`sessionId`) and the
/// shell process environment (`WT_SESSION`).
///
/// The generic terminal adapter intentionally captures persisted layout first
/// and process inventory second. When several tabs use the same shell/profile,
/// its conservative fallback can attach a live PID to the first compatible
/// layout entry rather than the pane that actually owns that process. That is
/// unacceptable for restore matching because it can make the wrong CWD look
/// alive. This pass repairs that association before stale persisted-only entries
/// are filtered by terminal_context::enrich_for_matching.
pub(super) fn rebind(snapshot: &mut TerminalSnapshot) {
    let identities = load_persisted_identities(snapshot);
    if identities.is_empty() {
        return;
    }

    rebind_with(snapshot, &identities, |pid| {
        process_environment_variable(pid, "WT_SESSION")
    });
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PersistedIdentity {
    session_id: String,
    profile: Option<String>,
    starting_directory: Option<String>,
    tab_title: Option<String>,
}

#[derive(Debug, Clone)]
struct RuntimeBinding {
    source_index: usize,
    target_index: usize,
    pid: u32,
    parent_pid: Option<u32>,
    session_id: String,
    shell_executable: Option<String>,
    startup_command: Option<String>,
    foreground_command: Option<String>,
}

fn load_persisted_identities(snapshot: &TerminalSnapshot) -> Vec<PersistedIdentity> {
    let paths = snapshot
        .windows_terminal_layouts
        .iter()
        .map(|layout| layout.source_path.as_str())
        .filter(|path| !path.trim().is_empty())
        .collect::<HashSet<_>>();

    let mut identities = Vec::new();
    let mut seen = HashSet::new();
    for path in paths {
        let Ok(text) = fs::read_to_string(path) else {
            continue;
        };
        let Ok(root) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        for identity in persisted_identities_from_json(&root) {
            if seen.insert(identity.session_id.clone()) {
                identities.push(identity);
            }
        }
    }
    identities
}

fn persisted_identities_from_json(root: &Value) -> Vec<PersistedIdentity> {
    let Some(windows) = root.get("persistedWindowLayouts").and_then(Value::as_array) else {
        return Vec::new();
    };

    let mut identities = Vec::new();
    for window in windows {
        let Some(actions) = window.get("tabLayout").and_then(Value::as_array) else {
            continue;
        };
        for action in actions {
            let kind = action.get("action").and_then(Value::as_str).unwrap_or_default();
            if !matches!(kind, "newTab" | "splitPane") {
                continue;
            }
            let Some(session_id) = action
                .get("sessionId")
                .and_then(Value::as_str)
                .map(normalize_session_id)
                .filter(|value| !value.is_empty())
            else {
                continue;
            };
            identities.push(PersistedIdentity {
                session_id,
                profile: string_field(action, "profile"),
                starting_directory: string_field(action, "startingDirectory"),
                tab_title: string_field(action, "tabTitle"),
            });
        }
    }
    identities
}

fn string_field(value: &Value, name: &str) -> Option<String> {
    value
        .get(name)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn rebind_with<F>(
    snapshot: &mut TerminalSnapshot,
    identities: &[PersistedIdentity],
    session_id_for_pid: F,
) where
    F: Fn(u32) -> Option<String>,
{
    let mut bindings = Vec::new();

    for (source_index, session) in snapshot.sessions.iter().enumerate() {
        if session.host != TerminalHost::WindowsTerminal
            || session.pid.is_none()
            || !session.sources.contains(&TerminalSource::WindowsProcess)
        {
            continue;
        }
        let Some(pid) = session.pid else {
            continue;
        };
        let Some(raw_session_id) = session_id_for_pid(pid) else {
            continue;
        };
        let session_id = normalize_session_id(&raw_session_id);
        if session_id.is_empty() {
            continue;
        }
        let Some(identity) = identities
            .iter()
            .find(|identity| identity.session_id == session_id)
        else {
            continue;
        };

        let candidates = snapshot
            .sessions
            .iter()
            .enumerate()
            .filter(|(_, candidate)| persisted_identity_matches(identity, candidate))
            .map(|(index, _)| index)
            .collect::<Vec<_>>();

        // If metadata is insufficient to distinguish identical persisted panes,
        // do not guess. Guessing is exactly what caused the regression this code
        // exists to prevent.
        if candidates.len() != 1 {
            continue;
        }

        bindings.push(RuntimeBinding {
            source_index,
            target_index: candidates[0],
            pid,
            parent_pid: session.parent_pid,
            session_id,
            shell_executable: session.shell_executable.clone(),
            startup_command: session.startup_command.clone(),
            foreground_command: session.foreground_command.clone(),
        });
    }

    // Compute every mapping from the untouched snapshot first. A source for one
    // live process can be the target for another mis-associated process, so
    // moving them sequentially would clobber runtime evidence. Clear all source
    // associations, then apply all exact mappings as one logical operation.
    for binding in &bindings {
        clear_runtime_binding(&mut snapshot.sessions[binding.source_index]);
    }
    for binding in bindings {
        apply_runtime_binding(&mut snapshot.sessions[binding.target_index], &binding);
    }
}

fn persisted_identity_matches(identity: &PersistedIdentity, session: &TerminalSession) -> bool {
    if session.host != TerminalHost::WindowsTerminal
        || !session.sources.contains(&TerminalSource::WindowsTerminalState)
    {
        return false;
    }

    if let Some(profile) = identity.profile.as_deref() {
        if !session
            .profile
            .as_deref()
            .is_some_and(|value| value.eq_ignore_ascii_case(profile))
        {
            return false;
        }
    }
    if let Some(directory) = identity.starting_directory.as_deref() {
        if !session
            .working_directory
            .as_deref()
            .is_some_and(|value| paths_equivalent(value, directory))
        {
            return false;
        }
    }
    if let Some(title) = identity.tab_title.as_deref() {
        if !session
            .title
            .as_deref()
            .is_some_and(|value| value.eq_ignore_ascii_case(title))
        {
            return false;
        }
    }

    true
}

fn clear_runtime_binding(session: &mut TerminalSession) {
    session
        .sources
        .retain(|source| *source != TerminalSource::WindowsProcess);
    session.pid = None;
    session.parent_pid = None;
    session.foreground_command = None;
    session.tty = None;
}

fn apply_runtime_binding(session: &mut TerminalSession, binding: &RuntimeBinding) {
    if !session.sources.contains(&TerminalSource::WindowsProcess) {
        session.sources.push(TerminalSource::WindowsProcess);
    }
    session.pid = Some(binding.pid);
    session.parent_pid = binding.parent_pid;
    session.tty = Some(binding.session_id.clone());
    if session.shell_executable.is_none() {
        session.shell_executable = binding.shell_executable.clone();
    }
    if session.startup_command.is_none() {
        session.startup_command = binding.startup_command.clone();
    }
    session.foreground_command = binding.foreground_command.clone();
}

fn paths_equivalent(left: &str, right: &str) -> bool {
    normalize_path(left) == normalize_path(right)
}

fn normalize_path(value: &str) -> String {
    value
        .trim()
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_ascii_lowercase()
}

fn normalize_session_id(value: &str) -> String {
    value
        .trim()
        .trim_start_matches('{')
        .trim_end_matches('}')
        .to_ascii_lowercase()
}

// The process-parameter offsets mirror the same native structure already used
// by terminal_context's live CWD reader. Microsoft documents PEB and
// RTL_USER_PROCESS_PARAMETERS as internal/versionable structures, so this code
// validates bitness and has a real Windows integration test below.
type Handle = *mut c_void;
const PROCESS_QUERY_INFORMATION: u32 = 0x0400;
const PROCESS_VM_READ: u32 = 0x0010;

#[cfg(target_pointer_width = "64")]
const PEB_PROCESS_PARAMETERS_OFFSET: usize = 0x20;
#[cfg(target_pointer_width = "64")]
const PROCESS_PARAMETERS_ENVIRONMENT_OFFSET: usize = 0x80;

#[cfg(target_pointer_width = "32")]
const PEB_PROCESS_PARAMETERS_OFFSET: usize = 0x10;
#[cfg(target_pointer_width = "32")]
const PROCESS_PARAMETERS_ENVIRONMENT_OFFSET: usize = 0x48;

const MAX_ENVIRONMENT_WORDS: usize = 256 * 1024;
const ENVIRONMENT_CHUNK_WORDS: usize = 128;

#[repr(C)]
struct ProcessBasicInformation {
    reserved1: *mut c_void,
    peb_base_address: *mut c_void,
    reserved2: [*mut c_void; 2],
    unique_process_id: usize,
    reserved3: *mut c_void,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn OpenProcess(desired_access: u32, inherit_handle: i32, process_id: u32) -> Handle;
    fn CloseHandle(handle: Handle) -> i32;
    fn ReadProcessMemory(
        process: Handle,
        base_address: *const c_void,
        buffer: *mut c_void,
        size: usize,
        bytes_read: *mut usize,
    ) -> i32;
    fn IsWow64Process(process: Handle, wow64: *mut i32) -> i32;
    fn GetCurrentProcess() -> Handle;
}

#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtQueryInformationProcess(
        process: Handle,
        information_class: u32,
        information: *mut c_void,
        information_length: u32,
        return_length: *mut u32,
    ) -> i32;
}

struct OwnedHandle(Handle);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 as isize != -1 {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

fn process_environment_variable(pid: u32, name: &str) -> Option<String> {
    if pid == 0 || name.trim().is_empty() {
        return None;
    }

    let raw = unsafe { OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, 0, pid) };
    if raw.is_null() {
        return None;
    }
    let process = OwnedHandle(raw);
    if !same_process_bitness(process.0) {
        return None;
    }

    let mut basic: ProcessBasicInformation = unsafe { zeroed() };
    let status = unsafe {
        NtQueryInformationProcess(
            process.0,
            0,
            &mut basic as *mut _ as *mut c_void,
            size_of::<ProcessBasicInformation>() as u32,
            null_mut(),
        )
    };
    if status < 0 || basic.peb_base_address.is_null() {
        return None;
    }

    let process_parameters = read_usize(
        process.0,
        basic.peb_base_address as usize + PEB_PROCESS_PARAMETERS_OFFSET,
    )?;
    if process_parameters == 0 {
        return None;
    }
    let environment = read_usize(
        process.0,
        process_parameters + PROCESS_PARAMETERS_ENVIRONMENT_OFFSET,
    )?;
    if environment == 0 {
        return None;
    }

    let words = read_environment_words(process.0, environment)?;
    environment_variable_from_words(&words, name)
}

fn read_environment_words(process: Handle, address: usize) -> Option<Vec<u16>> {
    let mut words = Vec::new();
    let mut offset = 0usize;
    let mut previous_zero = false;

    while words.len() < MAX_ENVIRONMENT_WORDS {
        let remaining = MAX_ENVIRONMENT_WORDS - words.len();
        let chunk_words = remaining.min(ENVIRONMENT_CHUNK_WORDS);
        let mut chunk = vec![0_u16; chunk_words];
        let byte_len = chunk_words * size_of::<u16>();

        if read_exact(
            process,
            address + offset,
            chunk.as_mut_ptr() as *mut c_void,
            byte_len,
        ) {
            for word in chunk {
                words.push(word);
                if word == 0 {
                    if previous_zero {
                        return Some(words);
                    }
                    previous_zero = true;
                } else {
                    previous_zero = false;
                }
            }
            offset += byte_len;
            continue;
        }

        // A bulk read can cross the allocation boundary after the terminating
        // NULs. Fall back to one UTF-16 word so we can still reach the terminator
        // without assuming a fixed environment allocation size.
        let word = read_u16(process, address + offset)?;
        words.push(word);
        offset += size_of::<u16>();
        if word == 0 {
            if previous_zero {
                return Some(words);
            }
            previous_zero = true;
        } else {
            previous_zero = false;
        }
    }

    None
}

fn environment_variable_from_words(words: &[u16], name: &str) -> Option<String> {
    let mut start = 0usize;
    while start < words.len() {
        let end = words[start..]
            .iter()
            .position(|word| *word == 0)
            .map(|offset| start + offset)?;
        if end == start {
            return None;
        }

        let entry = String::from_utf16_lossy(&words[start..end]);
        if let Some((key, value)) = entry.split_once('=') {
            if key.eq_ignore_ascii_case(name) {
                return Some(value.to_owned());
            }
        }
        start = end + 1;
    }
    None
}

fn same_process_bitness(target: Handle) -> bool {
    let mut target_wow64 = 0;
    let mut current_wow64 = 0;
    let target_ok = unsafe { IsWow64Process(target, &mut target_wow64) } != 0;
    let current_ok = unsafe { IsWow64Process(GetCurrentProcess(), &mut current_wow64) } != 0;
    target_ok && current_ok && target_wow64 == current_wow64
}

fn read_usize(process: Handle, address: usize) -> Option<usize> {
    let mut value = 0_usize;
    read_exact(
        process,
        address,
        &mut value as *mut _ as *mut c_void,
        size_of::<usize>(),
    )
    .then_some(value)
}

fn read_u16(process: Handle, address: usize) -> Option<u16> {
    let mut value = 0_u16;
    read_exact(
        process,
        address,
        &mut value as *mut _ as *mut c_void,
        size_of::<u16>(),
    )
    .then_some(value)
}

fn read_exact(process: Handle, address: usize, buffer: *mut c_void, size: usize) -> bool {
    let mut read = 0_usize;
    unsafe {
        ReadProcessMemory(process, address as *const c_void, buffer, size, &mut read) != 0
            && read == size
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::terminal::{
        RestartPlan, ShellKind, TerminalEnvironment, TerminalHistoryPolicy, TerminalStatus,
        WorkingDirectorySource,
    };
    use serde_json::json;
    use std::{
        io::Write,
        process::{Command, Stdio},
        thread,
        time::{Duration, Instant},
    };

    fn powershell_layout(directory: &str, pid: Option<u32>) -> TerminalSession {
        let mut sources = vec![TerminalSource::WindowsTerminalState];
        if pid.is_some() {
            sources.push(TerminalSource::WindowsProcess);
        }
        TerminalSession {
            sources,
            host: TerminalHost::WindowsTerminal,
            shell: ShellKind::PowerShell,
            shell_executable: Some("pwsh.exe".to_owned()),
            environment: TerminalEnvironment::Windows,
            pid,
            parent_pid: pid.map(|_| 10),
            tty: None,
            profile: Some("PowerShell".to_owned()),
            title: Some("PowerShell".to_owned()),
            working_directory: Some(directory.to_owned()),
            working_directory_source: WorkingDirectorySource::WindowsTerminalState,
            startup_command: Some("pwsh.exe".to_owned()),
            foreground_command: None,
            restart: Some(RestartPlan {
                executable: "wt.exe".to_owned(),
                args: vec![
                    "new-tab".to_owned(),
                    "-p".to_owned(),
                    "PowerShell".to_owned(),
                    "-d".to_owned(),
                    directory.to_owned(),
                ],
                working_directory: None,
                note: None,
            }),
        }
    }

    fn snapshot(sessions: Vec<TerminalSession>) -> TerminalSnapshot {
        TerminalSnapshot {
            status: TerminalStatus::Available,
            message: None,
            windows_terminal_layouts: Vec::new(),
            sessions,
            warnings: Vec::new(),
            history: TerminalHistoryPolicy {
                captured: false,
                reason: "test".to_owned(),
            },
        }
    }

    fn identities() -> Vec<PersistedIdentity> {
        vec![
            PersistedIdentity {
                session_id: "one".to_owned(),
                profile: Some("PowerShell".to_owned()),
                starting_directory: Some(r"C:\one".to_owned()),
                tab_title: Some("PowerShell".to_owned()),
            },
            PersistedIdentity {
                session_id: "two".to_owned(),
                profile: Some("PowerShell".to_owned()),
                starting_directory: Some(r"C:\two".to_owned()),
                tab_title: Some("PowerShell".to_owned()),
            },
            PersistedIdentity {
                session_id: "three".to_owned(),
                profile: Some("PowerShell".to_owned()),
                starting_directory: Some(r"C:\three".to_owned()),
                tab_title: Some("PowerShell".to_owned()),
            },
        ]
    }

    #[test]
    fn parses_persisted_session_ids() {
        let root = json!({
            "persistedWindowLayouts": [{
                "tabLayout": [
                    {
                        "action": "newTab",
                        "sessionId": "{AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE}",
                        "profile": "PowerShell",
                        "startingDirectory": "C:/one",
                        "tabTitle": "PowerShell"
                    },
                    { "action": "focusPane", "id": 1 }
                ]
            }]
        });
        let identities = persisted_identities_from_json(&root);
        assert_eq!(identities.len(), 1);
        assert_eq!(
            identities[0].session_id,
            "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"
        );
        assert_eq!(identities[0].starting_directory.as_deref(), Some("C:/one"));
    }

    #[test]
    fn session_id_rebind_moves_live_process_to_its_real_persisted_pane() {
        // The generic adapter incorrectly merged PID 30 into C:\one. WT_SESSION
        // says that process actually belongs to C:\three.
        let mut current = snapshot(vec![
            powershell_layout(r"C:\one", Some(30)),
            powershell_layout(r"C:\two", None),
            powershell_layout(r"C:\three", None),
        ]);

        rebind_with(&mut current, &identities(), |pid| {
            (pid == 30).then(|| "{THREE}".to_owned())
        });

        assert_eq!(current.sessions[0].pid, None);
        assert!(!current.sessions[0]
            .sources
            .contains(&TerminalSource::WindowsProcess));
        assert_eq!(current.sessions[2].pid, Some(30));
        assert!(current.sessions[2]
            .sources
            .contains(&TerminalSource::WindowsProcess));
        assert_eq!(current.sessions[2].tty.as_deref(), Some("three"));
    }

    #[test]
    fn session_id_rebind_handles_multiple_live_processes_without_clobbering() {
        // Generic first-compatible pairing produced one->PID30 and two->PID31,
        // while the actual WT_SESSION identities are PID30->two and PID31->three.
        let mut current = snapshot(vec![
            powershell_layout(r"C:\one", Some(30)),
            powershell_layout(r"C:\two", Some(31)),
            powershell_layout(r"C:\three", None),
        ]);

        rebind_with(&mut current, &identities(), |pid| match pid {
            30 => Some("two".to_owned()),
            31 => Some("three".to_owned()),
            _ => None,
        });

        assert_eq!(current.sessions[0].pid, None);
        assert_eq!(current.sessions[1].pid, Some(30));
        assert_eq!(current.sessions[1].tty.as_deref(), Some("two"));
        assert_eq!(current.sessions[2].pid, Some(31));
        assert_eq!(current.sessions[2].tty.as_deref(), Some("three"));
        assert!(current.sessions[1]
            .sources
            .contains(&TerminalSource::WindowsProcess));
        assert!(current.sessions[2]
            .sources
            .contains(&TerminalSource::WindowsProcess));
    }

    #[test]
    fn parses_utf16_environment_block_case_insensitively() {
        let text = "Path=C:\\Windows\0WT_SESSION={ABC-123}\0OTHER=value\0\0";
        let words = text.encode_utf16().collect::<Vec<_>>();
        assert_eq!(
            environment_variable_from_words(&words, "wt_session").as_deref(),
            Some("{ABC-123}")
        );
    }

    #[test]
    fn reads_injected_environment_variable_from_live_child_process() {
        let expected = "{8C814B99-50F7-4D26-9D30-3A4CE3C33E02}";
        let mut child = Command::new("cmd.exe")
            .args(["/D", "/Q", "/K"])
            .env("CONTEXT_CAPSULE_TEST_SESSION", expected)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn cmd.exe");

        let deadline = Instant::now() + Duration::from_secs(4);
        let mut observed = None;
        while Instant::now() < deadline {
            observed = process_environment_variable(child.id(), "CONTEXT_CAPSULE_TEST_SESSION");
            if observed.as_deref() == Some(expected) {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }

        if let Some(mut stdin) = child.stdin.take() {
            let _ = writeln!(stdin, "exit");
            let _ = stdin.flush();
        }
        let _ = child.kill();
        let _ = child.wait();

        assert_eq!(
            observed.as_deref(),
            Some(expected),
            "remote process environment reader did not observe injected value; observed={observed:?}"
        );
    }
}
