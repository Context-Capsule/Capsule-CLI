use crate::adapters::terminal::{
    TerminalEnvironment, TerminalHost, TerminalSession, TerminalSnapshot, TerminalSource,
};
use std::collections::HashSet;

#[cfg(windows)]
#[path = "windows_terminal_identity.rs"]
mod windows_terminal_identity;

/// Prepare generic terminal discovery for durable capsule storage.
///
/// Two kinds of sessions must not become independent restart plans:
/// - the shell that is currently hosting the `capsule` command itself;
/// - VS Code integrated terminals when the semantic VS Code adapter is live,
///   because the editor snapshot already owns those terminals and their CWDs.
///
/// Standalone Windows shells are also enriched with their live process current
/// directory where Windows permits it. The directory is copied into the safe
/// restart plan, so a restored cmd/PowerShell session starts where it was saved.
pub fn prepare_for_capture(
    snapshot: &TerminalSnapshot,
    vscode_semantic_available: bool,
) -> TerminalSnapshot {
    #[cfg(windows)]
    {
        let ancestors = current_process_ancestors();
        return prepare_with(snapshot, vscode_semantic_available, &ancestors, |pid| {
            process_working_directory(pid)
        });
    }

    #[cfg(not(windows))]
    {
        prepare_with(snapshot, vscode_semantic_available, &HashSet::new(), |_| {
            None
        })
    }
}

/// Enrich live terminal discovery with the same process CWD metadata used at
/// capture time. Restore matching needs this so a saved cmd.exe in C:\work is
/// not confused with another cmd.exe in a different directory, nor duplicated
/// merely because the older generic process discovery reported an unknown CWD.
pub(crate) fn enrich_for_matching(snapshot: &TerminalSnapshot) -> TerminalSnapshot {
    let mut prepared = snapshot.clone();

    #[cfg(windows)]
    {
        // Windows Terminal state.json and each live pane share the same stable
        // WT_SESSION GUID. Repair any first-compatible same-shell process merge
        // before consulting CWDs or deciding which persisted panes are alive.
        windows_terminal_identity::rebind(&mut prepared);

        for session in &mut prepared.sessions {
            enrich_working_directory(session, &process_working_directory);
        }
    }

    // Windows Terminal's persistedWindowLayouts are restart metadata, not a
    // reliable inventory of tabs that are alive right now. A closed tab can
    // remain in that state file and previously satisfied restore matching even
    // though its shell process was gone. Keep those records for capture, but
    // require runtime evidence before a Windows Terminal session can suppress a
    // saved tab during restore.
    prepared.sessions.retain(terminal_session_is_live_for_matching);

    prepared
}

fn terminal_session_is_live_for_matching(session: &TerminalSession) -> bool {
    if session.host != TerminalHost::WindowsTerminal {
        return true;
    }

    session.sources.iter().any(|source| {
        matches!(
            source,
            TerminalSource::WindowsProcess | TerminalSource::WslProc
        )
    })
}

fn prepare_with<F>(
    snapshot: &TerminalSnapshot,
    vscode_semantic_available: bool,
    command_ancestors: &HashSet<u32>,
    cwd_for_pid: F,
) -> TerminalSnapshot
where
    F: Fn(u32) -> Option<String>,
{
    let mut prepared = snapshot.clone();

    prepared.sessions.retain(|session| {
        if vscode_semantic_available && session.host == TerminalHost::VisualStudioCode {
            return false;
        }

        !session
            .pid
            .is_some_and(|pid| command_ancestors.contains(&pid))
    });

    for session in &mut prepared.sessions {
        enrich_working_directory(session, &cwd_for_pid);
    }

    prepared
}

fn enrich_working_directory<F>(session: &mut TerminalSession, cwd_for_pid: &F)
where
    F: Fn(u32) -> Option<String>,
{
    if !matches!(session.environment, TerminalEnvironment::Windows) {
        return;
    }

    let process_directory = session.pid.and_then(|pid| cwd_for_pid(pid));
    let has_terminal_reported_powershell_location = session.host == TerminalHost::WindowsTerminal
        && matches!(
            session.shell,
            crate::adapters::terminal::ShellKind::PowerShell
                | crate::adapters::terminal::ShellKind::WindowsPowerShell
        )
        && session.working_directory.is_some()
        && matches!(
            session.working_directory_source,
            crate::adapters::terminal::WorkingDirectorySource::WindowsTerminalState
        );

    // cmd.exe keeps the Win32 process CWD in sync with `cd`, so the live process
    // value is authoritative and can replace a stale Windows Terminal starting
    // directory. PowerShell is different: each runspace owns its own `$PWD`, and
    // Microsoft explicitly documents that this is not the process CWD. When
    // Windows Terminal has a shell-reported location for PowerShell, preserve it;
    // the PEB value is only a fallback when no terminal-reported location exists.
    if !has_terminal_reported_powershell_location {
        if let Some(directory) = process_directory {
            session.working_directory = Some(directory);
        }
    }

    let Some(directory) = session.working_directory.clone() else {
        return;
    };
    let Some(restart) = session.restart.as_mut() else {
        return;
    };

    if is_direct_windows_shell(&restart.executable) {
        // Direct shell restart plans are launched with Command::current_dir.
        restart.working_directory = Some(directory);
        restart.note = Some(
            "Starts the captured interactive shell in its captured working directory without replaying shell history or foreground commands."
                .to_owned(),
        );
    } else if is_windows_terminal_launcher(&restart.executable) {
        // A Windows Terminal child shell does not reliably inherit the CLI's
        // working directory. wt.exe therefore needs an explicit -d argument.
        set_windows_terminal_restart_directory(&mut restart.args, &directory);
        restart.note = Some(
            "Reopens the captured Windows Terminal profile in the shell's captured working directory without replaying shell history or foreground commands."
                .to_owned(),
        );
    }
}

fn is_direct_windows_shell(executable: &str) -> bool {
    let name = executable
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or(executable)
        .to_ascii_lowercase();
    matches!(
        name.as_str(),
        "cmd.exe"
            | "cmd"
            | "powershell.exe"
            | "powershell"
            | "pwsh.exe"
            | "pwsh"
            | "bash.exe"
            | "bash"
            | "zsh.exe"
            | "zsh"
            | "fish.exe"
            | "fish"
            | "nu.exe"
            | "nu"
            | "nushell.exe"
            | "nushell"
            | "sh.exe"
            | "sh"
    )
}

fn is_windows_terminal_launcher(executable: &str) -> bool {
    let name = executable
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or(executable)
        .to_ascii_lowercase();
    matches!(name.as_str(), "wt.exe" | "wt")
}

fn set_windows_terminal_restart_directory(args: &mut Vec<String>, directory: &str) {
    if let Some(index) = args.iter().position(|argument| {
        argument.eq_ignore_ascii_case("-d")
            || argument.eq_ignore_ascii_case("--startingDirectory")
            || argument.eq_ignore_ascii_case("--starting-directory")
    }) {
        if let Some(value) = args.get_mut(index + 1) {
            *value = directory.to_owned();
        } else {
            args.push(directory.to_owned());
        }
        return;
    }

    args.push("-d".to_owned());
    args.push(directory.to_owned());
}

#[cfg(windows)]
mod windows_process {
    use std::{
        collections::{HashMap, HashSet},
        ffi::c_void,
        mem::{size_of, zeroed},
        path::Path,
        ptr::null_mut,
    };

    type Handle = *mut c_void;
    const PROCESS_QUERY_INFORMATION: u32 = 0x0400;
    const PROCESS_VM_READ: u32 = 0x0010;
    const TH32CS_SNAPPROCESS: u32 = 0x0000_0002;
    const MAX_PATH: usize = 260;

    #[cfg(target_pointer_width = "64")]
    const PEB_PROCESS_PARAMETERS_OFFSET: usize = 0x20;
    #[cfg(target_pointer_width = "64")]
    const CURRENT_DIRECTORY_UNICODE_OFFSET: usize = 0x38;
    #[cfg(target_pointer_width = "64")]
    const CURRENT_DIRECTORY_BUFFER_OFFSET: usize = 0x40;

    #[cfg(target_pointer_width = "32")]
    const PEB_PROCESS_PARAMETERS_OFFSET: usize = 0x10;
    #[cfg(target_pointer_width = "32")]
    const CURRENT_DIRECTORY_UNICODE_OFFSET: usize = 0x24;
    #[cfg(target_pointer_width = "32")]
    const CURRENT_DIRECTORY_BUFFER_OFFSET: usize = 0x28;

    #[repr(C)]
    struct ProcessBasicInformation {
        reserved1: *mut c_void,
        peb_base_address: *mut c_void,
        reserved2: [*mut c_void; 2],
        unique_process_id: usize,
        reserved3: *mut c_void,
    }

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
        fn CreateToolhelp32Snapshot(flags: u32, process_id: u32) -> Handle;
        fn Process32FirstW(snapshot: Handle, entry: *mut ProcessEntry32W) -> i32;
        fn Process32NextW(snapshot: Handle, entry: *mut ProcessEntry32W) -> i32;
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

    pub fn current_process_ancestors() -> HashSet<u32> {
        let parents = process_parent_map();
        let mut result = HashSet::new();
        let mut current = std::process::id();

        for _ in 0..64 {
            let Some(parent) = parents.get(&current).copied() else {
                break;
            };
            if parent == 0 || parent == current || !result.insert(parent) {
                break;
            }
            current = parent;
        }
        result
    }

    fn process_parent_map() -> HashMap<u32, u32> {
        let mut result = HashMap::new();
        let raw = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if raw as isize == -1 || raw.is_null() {
            return result;
        }
        let snapshot = OwnedHandle(raw);
        let mut entry: ProcessEntry32W = unsafe { zeroed() };
        entry.size = size_of::<ProcessEntry32W>() as u32;

        let mut ok = unsafe { Process32FirstW(snapshot.0, &mut entry) } != 0;
        while ok {
            result.insert(entry.process_id, entry.parent_process_id);
            ok = unsafe { Process32NextW(snapshot.0, &mut entry) } != 0;
        }
        result
    }

    pub fn process_working_directory(pid: u32) -> Option<String> {
        if pid == 0 {
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

        let byte_length = read_u16(
            process.0,
            process_parameters + CURRENT_DIRECTORY_UNICODE_OFFSET,
        )? as usize;
        if byte_length == 0 || byte_length > 32_768 || byte_length % 2 != 0 {
            return None;
        }
        let buffer = read_usize(
            process.0,
            process_parameters + CURRENT_DIRECTORY_BUFFER_OFFSET,
        )?;
        if buffer == 0 {
            return None;
        }

        let mut words = vec![0_u16; byte_length / 2];
        if !read_exact(
            process.0,
            buffer,
            words.as_mut_ptr() as *mut c_void,
            byte_length,
        ) {
            return None;
        }
        let directory = String::from_utf16_lossy(&words)
            .trim_end_matches('\0')
            .trim()
            .to_owned();
        if directory.is_empty() || !Path::new(&directory).is_dir() {
            return None;
        }
        Some(directory)
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
}

#[cfg(windows)]
use windows_process::{current_process_ancestors, process_working_directory};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::terminal::{
        RestartPlan, ShellKind, TerminalHistoryPolicy, TerminalSource, TerminalStatus,
        WorkingDirectorySource,
    };

    fn session(host: TerminalHost, pid: u32, executable: &str) -> TerminalSession {
        TerminalSession {
            sources: vec![TerminalSource::WindowsProcess],
            host,
            shell: ShellKind::CommandPrompt,
            shell_executable: Some(executable.to_owned()),
            environment: TerminalEnvironment::Windows,
            pid: Some(pid),
            parent_pid: None,
            tty: None,
            profile: None,
            title: None,
            working_directory: None,
            working_directory_source: WorkingDirectorySource::Unknown,
            startup_command: None,
            foreground_command: None,
            restart: Some(RestartPlan {
                executable: executable.to_owned(),
                args: Vec::new(),
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

    #[test]
    fn active_capsule_host_shell_is_not_saved_as_a_restartable_session() {
        let value = snapshot(vec![
            session(TerminalHost::ConsoleHost, 10, "cmd.exe"),
            session(TerminalHost::ConsoleHost, 20, "cmd.exe"),
        ]);
        let ancestors = [10_u32].into_iter().collect::<HashSet<_>>();
        let prepared = prepare_with(&value, false, &ancestors, |_| None);
        assert_eq!(prepared.sessions.len(), 1);
        assert_eq!(prepared.sessions[0].pid, Some(20));
    }

    #[test]
    fn vscode_semantic_snapshot_owns_integrated_terminal_sessions() {
        let value = snapshot(vec![
            session(TerminalHost::VisualStudioCode, 10, "cmd.exe"),
            session(TerminalHost::ConsoleHost, 20, "cmd.exe"),
        ]);
        let prepared = prepare_with(&value, true, &HashSet::new(), |_| None);
        assert_eq!(prepared.sessions.len(), 1);
        assert_eq!(prepared.sessions[0].host, TerminalHost::ConsoleHost);
    }

    #[test]
    fn standalone_cmd_cwd_is_copied_into_its_restart_plan() {
        let value = snapshot(vec![session(TerminalHost::ConsoleHost, 20, "cmd.exe")]);
        let prepared = prepare_with(&value, false, &HashSet::new(), |pid| {
            (pid == 20).then(|| r"C:\work\project".to_owned())
        });
        let restored = &prepared.sessions[0];
        assert_eq!(
            restored.working_directory.as_deref(),
            Some(r"C:\work\project")
        );
        assert_eq!(
            restored
                .restart
                .as_ref()
                .and_then(|plan| plan.working_directory.as_deref()),
            Some(r"C:\work\project")
        );
    }

    #[test]
    fn matching_enrichment_preserves_session_identity_and_adds_cwd() {
        let value = snapshot(vec![session(TerminalHost::ConsoleHost, 20, "cmd.exe")]);
        let mut prepared = value.clone();
        for session in &mut prepared.sessions {
            enrich_working_directory(session, &|pid| {
                (pid == 20).then(|| r"C:\work\project".to_owned())
            });
        }
        assert_eq!(prepared.sessions.len(), value.sessions.len());
        assert_eq!(prepared.sessions[0].pid, value.sessions[0].pid);
        assert_eq!(
            prepared.sessions[0].working_directory.as_deref(),
            Some(r"C:\work\project")
        );
    }

    #[test]
    fn restore_matching_ignores_closed_powershell_tabs_left_in_terminal_state() {
        let mut live = session(TerminalHost::WindowsTerminal, 30, "pwsh.exe");
        live.shell = ShellKind::PowerShell;
        live.profile = Some("PowerShell".to_owned());
        live.sources = vec![
            TerminalSource::WindowsTerminalState,
            TerminalSource::WindowsProcess,
        ];

        let mut closed_one = live.clone();
        closed_one.pid = None;
        closed_one.sources = vec![TerminalSource::WindowsTerminalState];

        let mut closed_two = closed_one.clone();
        closed_two.profile = Some("PowerShell".to_owned());

        let mut current = snapshot(vec![live, closed_one, closed_two]);
        current.sessions.retain(terminal_session_is_live_for_matching);

        assert_eq!(current.sessions.len(), 1);
        assert_eq!(current.sessions[0].pid, Some(30));
        assert!(current.sessions[0]
            .sources
            .contains(&TerminalSource::WindowsProcess));
    }

    #[test]
    fn windows_terminal_cmd_live_cwd_overrides_persisted_starting_directory() {
        let startup = r"C:\startup";
        let live = r"C:\users\example\project";
        let mut wt = session(TerminalHost::WindowsTerminal, 30, "cmd.exe");
        wt.profile = Some("Command Prompt".to_owned());
        wt.working_directory = Some(startup.to_owned());
        wt.working_directory_source = WorkingDirectorySource::WindowsTerminalState;
        wt.restart = Some(RestartPlan {
            executable: "wt.exe".to_owned(),
            args: vec![
                "new-tab".to_owned(),
                "-p".to_owned(),
                "Command Prompt".to_owned(),
                "-d".to_owned(),
                startup.to_owned(),
            ],
            working_directory: None,
            note: None,
        });

        let prepared = prepare_with(&snapshot(vec![wt]), false, &HashSet::new(), |pid| {
            (pid == 30).then(|| live.to_owned())
        });
        let restored = &prepared.sessions[0];
        assert_eq!(restored.working_directory.as_deref(), Some(live));
        let plan = restored
            .restart
            .as_ref()
            .expect("Windows Terminal restart plan");
        let directory_index = plan
            .args
            .iter()
            .position(|arg| arg == "-d")
            .expect("-d argument");
        assert_eq!(
            plan.args.get(directory_index + 1).map(String::as_str),
            Some(live)
        );
    }

    #[test]
    fn windows_terminal_powershell_prefers_terminal_reported_location_over_process_cwd() {
        let reported = r"C:\users\example\project";
        let stale_process = r"C:\startup";
        let mut wt = session(TerminalHost::WindowsTerminal, 31, "pwsh.exe");
        wt.shell = ShellKind::PowerShell;
        wt.profile = Some("PowerShell".to_owned());
        wt.working_directory = Some(reported.to_owned());
        wt.working_directory_source = WorkingDirectorySource::WindowsTerminalState;
        wt.restart = Some(RestartPlan {
            executable: "wt.exe".to_owned(),
            args: vec![
                "new-tab".to_owned(),
                "-p".to_owned(),
                "PowerShell".to_owned(),
                "-d".to_owned(),
                reported.to_owned(),
            ],
            working_directory: None,
            note: None,
        });

        let prepared = prepare_with(&snapshot(vec![wt]), false, &HashSet::new(), |pid| {
            (pid == 31).then(|| stale_process.to_owned())
        });
        let restored = &prepared.sessions[0];
        assert_eq!(restored.working_directory.as_deref(), Some(reported));
        let plan = restored
            .restart
            .as_ref()
            .expect("Windows Terminal restart plan");
        let directory_index = plan
            .args
            .iter()
            .position(|arg| arg == "-d")
            .expect("-d argument");
        assert_eq!(
            plan.args.get(directory_index + 1).map(String::as_str),
            Some(reported)
        );
    }

    #[test]
    fn windows_terminal_persisted_starting_directory_is_fallback_when_live_cwd_is_unavailable() {
        let startup = r"C:\startup";
        let mut wt = session(TerminalHost::WindowsTerminal, 32, "pwsh.exe");
        wt.shell = ShellKind::PowerShell;
        wt.working_directory = Some(startup.to_owned());
        wt.working_directory_source = WorkingDirectorySource::WindowsTerminalState;
        wt.restart = Some(RestartPlan {
            executable: "wt.exe".to_owned(),
            args: vec!["new-tab".to_owned(), "-d".to_owned(), startup.to_owned()],
            working_directory: None,
            note: None,
        });

        let prepared = prepare_with(&snapshot(vec![wt]), false, &HashSet::new(), |_| None);
        let restored = &prepared.sessions[0];
        assert_eq!(restored.working_directory.as_deref(), Some(startup));
        assert_eq!(
            restored.working_directory_source,
            WorkingDirectorySource::WindowsTerminalState
        );
        let plan = restored
            .restart
            .as_ref()
            .expect("Windows Terminal restart plan");
        assert_eq!(plan.args.last().map(String::as_str), Some(startup));
    }

    #[cfg(windows)]
    #[test]
    fn windows_process_cwd_reader_observes_a_live_cmd_directory_change() {
        use std::{
            fs,
            io::Write,
            process::{Command, Stdio},
            thread,
            time::{Duration, Instant, SystemTime, UNIX_EPOCH},
        };

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "context-capsule-cwd-test-{}-{nonce}",
            std::process::id()
        ));
        let start = root.join("start");
        let target = root.join("target");
        fs::create_dir_all(&start).expect("create start directory");
        fs::create_dir_all(&target).expect("create target directory");

        let mut child = Command::new("cmd.exe")
            .args(["/D", "/Q", "/K"])
            .current_dir(&start)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn cmd.exe");
        let mut stdin = child.stdin.take().expect("cmd stdin");
        writeln!(stdin, "cd /d \"{}\"", target.display()).expect("send cd command");
        stdin.flush().expect("flush cd command");

        let deadline = Instant::now() + Duration::from_secs(4);
        let mut observed = None;
        while Instant::now() < deadline {
            observed = process_working_directory(child.id());
            if observed.as_deref().is_some_and(|directory| {
                directory
                    .replace('/', "\\")
                    .trim_end_matches('\\')
                    .eq_ignore_ascii_case(
                        target
                            .to_string_lossy()
                            .replace('/', "\\")
                            .trim_end_matches('\\'),
                    )
            }) {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }

        let _ = child.kill();
        let _ = child.wait();
        let _ = fs::remove_dir_all(&root);
        assert!(
            observed.as_deref().is_some_and(|directory| {
                directory
                    .replace('/', "\\")
                    .trim_end_matches('\\')
                    .eq_ignore_ascii_case(
                        target
                            .to_string_lossy()
                            .replace('/', "\\")
                            .trim_end_matches('\\'),
                    )
            }),
            "live cmd.exe CWD reader did not converge to target; observed={observed:?}"
        );
    }
}
