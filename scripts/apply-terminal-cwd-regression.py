from pathlib import Path

context_path = Path("src/terminal_context.rs")
text = context_path.read_text(encoding="utf-8")


def once(old: str, new: str) -> None:
    global text
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"expected exactly one occurrence, found {count}: {old[:160]!r}")
    text = text.replace(old, new, 1)


once(
    "use crate::adapters::terminal::{\n    TerminalEnvironment, TerminalHost, TerminalSession, TerminalSnapshot,\n};",
    "use crate::adapters::terminal::{\n    TerminalEnvironment, TerminalHost, TerminalSession, TerminalSnapshot, WorkingDirectorySource,\n};",
)

old_enrich = '''fn enrich_working_directory<F>(session: &mut TerminalSession, cwd_for_pid: &F)
where
    F: Fn(u32) -> Option<String>,
{
    if !matches!(session.environment, TerminalEnvironment::Windows) {
        return;
    }

    if session.working_directory.is_none() {
        let Some(pid) = session.pid else {
            return;
        };
        let Some(directory) = cwd_for_pid(pid) else {
            return;
        };
        session.working_directory = Some(directory);
    }

    let Some(directory) = session.working_directory.clone() else {
        return;
    };
    let Some(restart) = session.restart.as_mut() else {
        return;
    };

    // Direct shell restart plans are launched with Command::current_dir during
    // restore. Windows Terminal state already encodes its starting directory
    // through `wt.exe -d`, so do not mutate that richer plan here.
    if restart.working_directory.is_none() && is_direct_windows_shell(&restart.executable) {
        restart.working_directory = Some(directory);
        restart.note = Some(
            "Starts the captured interactive shell in its captured working directory without replaying shell history or foreground commands."
                .to_owned(),
        );
    }
}'''
new_enrich = '''fn enrich_working_directory<F>(session: &mut TerminalSession, cwd_for_pid: &F)
where
    F: Fn(u32) -> Option<String>,
{
    if !matches!(session.environment, TerminalEnvironment::Windows) {
        return;
    }

    // Windows Terminal state records a tab's starting directory, not the shell's
    // current directory after `cd`/Set-Location. A live shell process CWD is
    // stronger evidence and overrides the persisted startup value when readable.
    if let Some(directory) = session.pid.and_then(|pid| cwd_for_pid(pid)) {
        session.working_directory = Some(directory);
        session.working_directory_source = WorkingDirectorySource::WindowsProcess;
    }

    let Some(directory) = session.working_directory.clone() else {
        return;
    };
    let Some(restart) = session.restart.as_mut() else {
        return;
    };

    if is_direct_windows_shell(&restart.executable) {
        restart.working_directory = Some(directory);
        restart.note = Some(
            "Starts the captured interactive shell in its captured live working directory without replaying shell history or foreground commands."
                .to_owned(),
        );
    } else if is_windows_terminal_launcher(&restart.executable) {
        set_windows_terminal_restart_directory(&mut restart.args, &directory);
        restart.note = Some(
            "Reopens the captured Windows Terminal profile in the shell's captured live working directory without replaying shell history or foreground commands."
                .to_owned(),
        );
    }
}'''
once(old_enrich, new_enrich)

once(
    "}\n\n#[cfg(windows)]\nmod windows_process {",
    '''}

fn is_windows_terminal_launcher(executable: &str) -> bool {
    let name = executable
        .rsplit(['\\\\', '/'])
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
mod windows_process {''',
)

insert_at = text.rfind("\n}")
if insert_at < 0:
    raise SystemExit("could not find terminal_context test module end")
new_tests = r'''

    #[test]
    fn windows_terminal_live_cwd_overrides_persisted_starting_directory() {
        let startup = r"C:\startup";
        let live = r"C:\users\example\project";
        let mut wt = session(TerminalHost::WindowsTerminal, 30, "pwsh.exe");
        wt.profile = Some("PowerShell".to_owned());
        wt.working_directory = Some(startup.to_owned());
        wt.working_directory_source = WorkingDirectorySource::WindowsTerminalState;
        wt.restart = Some(RestartPlan {
            executable: "wt.exe".to_owned(),
            args: vec![
                "new-tab".to_owned(),
                "-p".to_owned(),
                "PowerShell".to_owned(),
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
        assert_eq!(restored.working_directory_source, WorkingDirectorySource::WindowsProcess);
        let plan = restored.restart.as_ref().expect("Windows Terminal restart plan");
        let directory_index = plan.args.iter().position(|arg| arg == "-d").expect("-d argument");
        assert_eq!(plan.args.get(directory_index + 1).map(String::as_str), Some(live));
    }

    #[test]
    fn windows_terminal_persisted_starting_directory_is_fallback_when_live_cwd_is_unavailable() {
        let startup = r"C:\startup";
        let mut wt = session(TerminalHost::WindowsTerminal, 31, "pwsh.exe");
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
        assert_eq!(restored.working_directory_source, WorkingDirectorySource::WindowsTerminalState);
        let plan = restored.restart.as_ref().expect("Windows Terminal restart plan");
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
                directory.replace('/', "\\").eq_ignore_ascii_case(
                    &target.to_string_lossy().replace('/', "\\")
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
                directory.replace('/', "\\").eq_ignore_ascii_case(
                    &target.to_string_lossy().replace('/', "\\")
                )
            }),
            "live cmd.exe CWD reader did not converge to target; observed={observed:?}"
        );
    }
'''
text = text[:insert_at] + new_tests + text[insert_at:]
context_path.write_text(text, encoding="utf-8")

terminal_path = Path("src/adapters/terminal.rs")
terminal = terminal_path.read_text(encoding="utf-8")
old_enum = '''pub enum WorkingDirectorySource {
    WindowsTerminalState,
    WslProc,
    Unknown,
}'''
new_enum = '''pub enum WorkingDirectorySource {
    WindowsTerminalState,
    WindowsProcess,
    WslProc,
    Unknown,
}'''
if terminal.count(old_enum) != 1:
    raise SystemExit("unexpected WorkingDirectorySource enum shape")
terminal = terminal.replace(old_enum, new_enum, 1)
terminal_path.write_text(terminal, encoding="utf-8")

# Finish the already-planned Chromium native-host framing safety correction so
# the branch does not retain the stale temporary workflow that tried to apply it.
chrome_path = Path("src/chrome.rs")
chrome = chrome_path.read_text(encoding="utf-8")
replacements = [
    (
        "const MAX_NATIVE_MESSAGE_BYTES: usize = 8 * 1024 * 1024;",
        "const MAX_NATIVE_REQUEST_BYTES: usize = 8 * 1024 * 1024;\nconst MAX_NATIVE_RESPONSE_BYTES: usize = 1024 * 1024;",
    ),
    (
        "if length == 0 || length > MAX_NATIVE_MESSAGE_BYTES {",
        "if length == 0 || length > MAX_NATIVE_REQUEST_BYTES {",
    ),
    (
        "if payload.is_empty() || payload.len() > MAX_NATIVE_MESSAGE_BYTES {",
        "if payload.is_empty() || payload.len() > MAX_NATIVE_RESPONSE_BYTES {",
    ),
]
for old, new in replacements:
    if chrome.count(old) != 1:
        raise SystemExit(f"unexpected Chrome native framing source: {old}")
    chrome = chrome.replace(old, new, 1)
marker = '''    #[test]
    fn native_ping_reads_and_writes_framed_messages() {
'''
response_test = '''    #[test]
    fn rejects_native_response_larger_than_chromium_host_limit() {
        let mut output = Vec::new();
        let payload = vec![b'x'; MAX_NATIVE_RESPONSE_BYTES + 1];
        let error = write_native_message(&mut output, &payload).unwrap_err();
        assert!(error.to_string().contains("invalid native response length"));
        assert!(output.is_empty());
    }

'''
if chrome.count(marker) != 1:
    raise SystemExit("unexpected Chrome native ping test marker")
chrome = chrome.replace(marker, response_test + marker, 1)
chrome_path.write_text(chrome, encoding="utf-8")
