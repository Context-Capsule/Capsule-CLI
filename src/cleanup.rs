use crate::{
    desktop::{ApplicationInfo, IgnoredCandidate},
    restore::{SavedApplication, SavedDesktop},
};
use serde_json::Value;

#[cfg(windows)]
use std::{
    collections::{HashMap, HashSet},
    ffi::c_void,
    mem::{size_of, zeroed},
    process::{Command, Stdio},
    thread,
    time::Duration,
};

#[cfg(windows)]
type Handle = *mut c_void;
#[cfg(windows)]
type Hwnd = *mut c_void;
#[cfg(windows)]
type Bool = i32;
#[cfg(windows)]
type EnumWindowsProc = Option<unsafe extern "system" fn(Hwnd, isize) -> Bool>;

#[cfg(windows)]
const TH32CS_SNAPPROCESS: u32 = 0x0000_0002;
#[cfg(windows)]
const MAX_PATH: usize = 260;
#[cfg(windows)]
const WM_CLOSE: u32 = 0x0010;
#[cfg(windows)]
const CLOSE_SETTLE: Duration = Duration::from_millis(1_200);
#[cfg(windows)]
const FORCE_SETTLE: Duration = Duration::from_millis(900);

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
    priority_class_base: i32,
    flags: u32,
    exe_file: [u16; MAX_PATH],
}

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateToolhelp32Snapshot(flags: u32, process_id: u32) -> Handle;
    fn Process32FirstW(snapshot: Handle, entry: *mut ProcessEntry32W) -> i32;
    fn Process32NextW(snapshot: Handle, entry: *mut ProcessEntry32W) -> i32;
    fn CloseHandle(handle: Handle) -> i32;
    fn GetCurrentProcessId() -> u32;
}

#[cfg(windows)]
#[link(name = "user32")]
unsafe extern "system" {
    fn EnumWindows(callback: EnumWindowsProc, lparam: isize) -> Bool;
    fn IsWindowVisible(hwnd: Hwnd) -> Bool;
    fn GetWindowTextLengthW(hwnd: Hwnd) -> i32;
    fn GetWindowTextW(hwnd: Hwnd, text: *mut u16, max_count: i32) -> i32;
    fn GetWindowThreadProcessId(hwnd: Hwnd, process_id: *mut u32) -> u32;
    fn PostMessageW(hwnd: Hwnd, message: u32, wparam: usize, lparam: isize) -> Bool;
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApplicationCleanupReport {
    pub applications_detected: usize,
    pub applications_in_capsule: usize,
    pub applications_planned_to_close: usize,
    pub close_requests_sent: usize,
    pub applications_closed: usize,
    pub applications_protected: usize,
    pub applications_remaining: usize,
    pub warnings: Vec<String>,
    pub failures: Vec<String>,
}

impl ApplicationCleanupReport {
    pub fn success(&self) -> bool {
        self.failures.is_empty() && self.applications_remaining == 0
    }
}

pub fn close_unrelated_applications(snapshot: &Value, dry_run: bool) -> ApplicationCleanupReport {
    #[cfg(windows)]
    {
        close_unrelated_windows(snapshot, dry_run)
    }

    #[cfg(not(windows))]
    {
        let mut report = ApplicationCleanupReport::default();
        let _ = (snapshot, dry_run);
        report.failures.push(
            "replace-mode application cleanup is currently implemented for Windows only".to_owned(),
        );
        report
    }
}

fn normalized_path(value: &str) -> String {
    value.trim().replace('/', "\\").to_ascii_lowercase()
}

fn file_name(value: &str) -> Option<&str> {
    value.rsplit(['\\', '/']).find(|part| !part.is_empty())
}

fn current_matches_saved(current: &ApplicationInfo, saved: &SavedApplication) -> bool {
    let mut saved_has_strong_identity = false;
    let mut observed_comparable_identity = false;

    if let Some(saved_aumid) = saved.app_user_model_id.as_deref() {
        saved_has_strong_identity = true;
        if let Some(current_aumid) = current.app_user_model_id.as_deref() {
            observed_comparable_identity = true;
            if current_aumid.eq_ignore_ascii_case(saved_aumid) {
                return true;
            }
        }
    }

    if let Some(saved_path) = saved.executable_path.as_deref() {
        saved_has_strong_identity = true;
        if let Some(current_path) = current.executable_path.as_deref() {
            observed_comparable_identity = true;
            if normalized_path(current_path) == normalized_path(saved_path) {
                return true;
            }
        }
    }

    if let Some(launch) = saved.launch.as_ref() {
        match launch.strategy.as_str() {
            "app-user-model-id" => {
                saved_has_strong_identity = true;
                if let Some(current_aumid) = current.app_user_model_id.as_deref() {
                    observed_comparable_identity = true;
                    if current_aumid.eq_ignore_ascii_case(&launch.target) {
                        return true;
                    }
                }
            }
            "executable" => {
                saved_has_strong_identity = true;
                if let Some(current_path) = current.executable_path.as_deref() {
                    observed_comparable_identity = true;
                    if normalized_path(current_path) == normalized_path(&launch.target) {
                        return true;
                    }
                }
            }
            _ => {}
        }
    }

    // A real comparable strong-identity mismatch is authoritative. If Windows
    // could not expose the comparable metadata at all, fail safe to a name
    // match instead of classifying a plausible capsule app as unrelated.
    if saved_has_strong_identity && observed_comparable_identity {
        return false;
    }
    current.name.eq_ignore_ascii_case(&saved.name)
}

fn current_executable_name(application: &ApplicationInfo) -> Option<&str> {
    application
        .executable_path
        .as_deref()
        .or_else(|| {
            application
                .launch
                .as_ref()
                .map(|launch| launch.target.as_str())
        })
        .and_then(file_name)
}

fn named_or_executable(application: &ApplicationInfo, values: &[&str]) -> bool {
    values
        .iter()
        .any(|value| application.name.eq_ignore_ascii_case(value))
        || current_executable_name(application)
            .is_some_and(|name| values.iter().any(|value| name.eq_ignore_ascii_case(value)))
}

fn is_windows_terminal(application: &ApplicationInfo) -> bool {
    application
        .app_user_model_id
        .as_deref()
        .is_some_and(|value| value.to_ascii_lowercase().contains("windowsterminal"))
        || named_or_executable(
            application,
            &["Windows Terminal", "WindowsTerminal.exe", "wt.exe"],
        )
}

fn is_vscode(application: &ApplicationInfo) -> bool {
    named_or_executable(
        application,
        &["Visual Studio Code", "VS Code", "Code", "Code.exe"],
    )
}

fn is_cursor(application: &ApplicationInfo) -> bool {
    named_or_executable(application, &["Cursor", "Cursor.exe"])
}

fn is_wezterm(application: &ApplicationInfo) -> bool {
    named_or_executable(application, &["WezTerm", "wezterm-gui.exe", "wezterm.exe"])
}

fn is_alacritty(application: &ApplicationInfo) -> bool {
    named_or_executable(application, &["Alacritty", "alacritty.exe"])
}

fn is_mintty(application: &ApplicationInfo) -> bool {
    named_or_executable(application, &["Mintty", "mintty.exe"])
}

fn is_firefox_family(application: &ApplicationInfo) -> bool {
    named_or_executable(
        application,
        &[
            "Zen",
            "Zen Browser",
            "zen.exe",
            "Firefox",
            "Mozilla Firefox",
            "firefox.exe",
        ],
    )
}

fn is_docker_desktop(application: &ApplicationInfo) -> bool {
    named_or_executable(
        application,
        &[
            "Docker Desktop",
            "Docker Desktop.exe",
            "DockerDesktop.exe",
            "Docker Desktop Backend.exe",
            "com.docker.backend.exe",
        ],
    )
}

fn is_explorer(application: &ApplicationInfo) -> bool {
    named_or_executable(application, &["Explorer", "File Explorer", "explorer.exe"])
}

fn terminal_snapshot_mentions_host(snapshot: &Value, host: &str) -> bool {
    snapshot
        .pointer("/terminals/sessions")
        .and_then(Value::as_array)
        .is_some_and(|sessions| {
            sessions
                .iter()
                .any(|session| session.get("host").and_then(Value::as_str) == Some(host))
        })
}

fn capsule_has_docker_resources(snapshot: &Value) -> bool {
    let compose = snapshot
        .pointer("/docker/compose_projects")
        .and_then(Value::as_array)
        .is_some_and(|values| !values.is_empty());
    let standalone = snapshot
        .pointer("/docker/standalone_containers")
        .and_then(Value::as_array)
        .is_some_and(|values| !values.is_empty());
    compose || standalone
}

fn owned_by_semantic_resource(snapshot: &Value, application: &ApplicationInfo) -> bool {
    if snapshot
        .pointer("/browsers/firefox")
        .is_some_and(|value| !value.is_null())
        && is_firefox_family(application)
    {
        return true;
    }
    if snapshot
        .pointer("/editors/vscode")
        .is_some_and(|value| !value.is_null())
        && is_vscode(application)
    {
        return true;
    }
    if capsule_has_docker_resources(snapshot) && is_docker_desktop(application) {
        return true;
    }

    (terminal_snapshot_mentions_host(snapshot, "windows-terminal")
        && is_windows_terminal(application))
        || (terminal_snapshot_mentions_host(snapshot, "visual-studio-code")
            && is_vscode(application))
        || (terminal_snapshot_mentions_host(snapshot, "cursor") && is_cursor(application))
        || (terminal_snapshot_mentions_host(snapshot, "wez-term") && is_wezterm(application))
        || (terminal_snapshot_mentions_host(snapshot, "alacritty") && is_alacritty(application))
        || (terminal_snapshot_mentions_host(snapshot, "mintty") && is_mintty(application))
}

fn belongs_to_capsule(
    snapshot: &Value,
    saved: &[SavedApplication],
    application: &ApplicationInfo,
) -> bool {
    saved
        .iter()
        .any(|candidate| current_matches_saved(application, candidate))
        || owned_by_semantic_resource(snapshot, application)
}

fn is_application_frame_host(candidate: &IgnoredCandidate) -> bool {
    candidate
        .executable
        .eq_ignore_ascii_case("ApplicationFrameHost.exe")
}

fn ignored_surface_belongs_to_capsule(snapshot: &Value, current: &IgnoredCandidate) -> bool {
    let Some(current_title) = current.window_title.as_deref() else {
        return true;
    };
    snapshot
        .pointer("/desktop/ignored")
        .and_then(Value::as_array)
        .is_some_and(|saved| {
            saved.iter().any(|candidate| {
                candidate
                    .get("executable")
                    .and_then(Value::as_str)
                    .is_some_and(|value| value.eq_ignore_ascii_case(&current.executable))
                    && candidate
                        .get("window_title")
                        .and_then(Value::as_str)
                        .is_some_and(|value| value.eq_ignore_ascii_case(current_title))
            })
        })
}

#[cfg(windows)]
fn process_parents() -> HashMap<u32, u32> {
    let mut result = HashMap::new();
    let handle = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if handle as isize == -1 {
        return result;
    }

    let mut entry: ProcessEntry32W = unsafe { zeroed() };
    entry.size = size_of::<ProcessEntry32W>() as u32;
    let mut ok = unsafe { Process32FirstW(handle, &mut entry) } != 0;
    while ok {
        result.insert(entry.process_id, entry.parent_process_id);
        entry = unsafe { zeroed() };
        entry.size = size_of::<ProcessEntry32W>() as u32;
        ok = unsafe { Process32NextW(handle, &mut entry) } != 0;
    }
    unsafe {
        CloseHandle(handle);
    }
    result
}

#[cfg(windows)]
fn protected_process_chain() -> HashSet<u32> {
    let parents = process_parents();
    let mut protected = HashSet::new();
    let mut current = unsafe { GetCurrentProcessId() };
    while current != 0 && protected.insert(current) {
        current = parents.get(&current).copied().unwrap_or(0);
    }
    protected
}

#[cfg(windows)]
fn hosts_current_command(application: &ApplicationInfo, protected_pids: &HashSet<u32>) -> bool {
    if application
        .pids
        .iter()
        .any(|pid| protected_pids.contains(pid))
    {
        return true;
    }
    if std::env::var_os("WT_SESSION").is_some() && is_windows_terminal(application) {
        return true;
    }
    if let Ok(program) = std::env::var("TERM_PROGRAM") {
        let program = program.to_ascii_lowercase();
        if program.contains("vscode") && is_vscode(application) {
            return true;
        }
        if program.contains("cursor") && is_cursor(application) {
            return true;
        }
        if program.contains("wezterm") && is_wezterm(application) {
            return true;
        }
    }
    false
}

#[cfg(windows)]
struct WindowCloseContext {
    pids: HashSet<u32>,
    title: Option<String>,
    posted: usize,
}

#[cfg(windows)]
unsafe extern "system" fn enum_close_window(hwnd: Hwnd, data: isize) -> Bool {
    let context = unsafe { &mut *(data as *mut WindowCloseContext) };
    if unsafe { IsWindowVisible(hwnd) } == 0 {
        return 1;
    }

    let mut pid = 0_u32;
    unsafe {
        GetWindowThreadProcessId(hwnd, &mut pid);
    }
    if !context.pids.contains(&pid) {
        return 1;
    }

    let title = window_title(hwnd);
    if title.is_empty() || title.eq_ignore_ascii_case("Program Manager") {
        return 1;
    }
    if context
        .title
        .as_deref()
        .is_some_and(|expected| !title.eq_ignore_ascii_case(expected))
    {
        return 1;
    }

    if unsafe { PostMessageW(hwnd, WM_CLOSE, 0, 0) } != 0 {
        context.posted += 1;
    }
    1
}

#[cfg(windows)]
fn window_title(hwnd: Hwnd) -> String {
    let length = unsafe { GetWindowTextLengthW(hwnd) };
    if length <= 0 {
        return String::new();
    }
    let mut buffer = vec![0_u16; length as usize + 1];
    let copied = unsafe { GetWindowTextW(hwnd, buffer.as_mut_ptr(), buffer.len() as i32) };
    if copied <= 0 {
        String::new()
    } else {
        String::from_utf16_lossy(&buffer[..copied as usize])
            .trim()
            .to_owned()
    }
}

#[cfg(windows)]
fn post_close_to_windows(pids: HashSet<u32>, title: Option<&str>) -> Result<usize, String> {
    if pids.is_empty() {
        return Ok(0);
    }
    let mut context = WindowCloseContext {
        pids,
        title: title.map(str::to_owned),
        posted: 0,
    };
    let data = (&mut context as *mut WindowCloseContext) as isize;
    if unsafe { EnumWindows(Some(enum_close_window), data) } == 0 {
        return Err("EnumWindows failed while sending WM_CLOSE".to_owned());
    }
    Ok(context.posted)
}

#[cfg(windows)]
fn application_pids(application: &ApplicationInfo) -> HashSet<u32> {
    let mut pids = application.pids.iter().copied().collect::<HashSet<_>>();
    if pids.is_empty() {
        pids.insert(application.primary_pid);
    }
    pids
}

#[cfg(windows)]
fn graceful_close(application: &ApplicationInfo) -> Vec<String> {
    let mut errors = Vec::new();
    let pids = application_pids(application);
    if let Err(error) = post_close_to_windows(pids.clone(), None) {
        errors.push(error);
    }

    // Explorer folder windows share the shell process. Never terminate that
    // process; WM_CLOSE on the folder windows is the correct operation.
    if is_explorer(application) {
        return errors;
    }

    for pid in pids {
        let pid_text = pid.to_string();
        let output = Command::new("taskkill")
            .args(["/PID", pid_text.as_str(), "/T"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output();
        match output {
            Ok(output) if output.status.success() => {}
            Ok(output) => {
                let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
                if !message.is_empty() {
                    errors.push(format!("PID {pid}: {message}"));
                }
            }
            Err(error) => errors.push(format!("PID {pid}: could not run taskkill: {error}")),
        }
    }
    errors
}

#[cfg(windows)]
fn force_close(application: &ApplicationInfo) -> Vec<String> {
    if is_explorer(application) {
        return post_close_to_windows(application_pids(application), None)
            .err()
            .into_iter()
            .collect();
    }

    let mut errors = Vec::new();
    for pid in application_pids(application) {
        let pid_text = pid.to_string();
        let output = Command::new("taskkill")
            .args(["/PID", pid_text.as_str(), "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output();
        match output {
            Ok(output) if output.status.success() => {}
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
                let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
                let detail = if !stderr.is_empty() { stderr } else { stdout };
                errors.push(format!(
                    "PID {pid}: force-close failed{}",
                    if detail.is_empty() {
                        String::new()
                    } else {
                        format!(": {detail}")
                    }
                ));
            }
            Err(error) => errors.push(format!("PID {pid}: could not run forced taskkill: {error}")),
        }
    }
    errors
}

#[cfg(windows)]
fn close_ignored_surface(candidate: &IgnoredCandidate) -> Result<(), String> {
    let Some(title) = candidate.window_title.as_deref() else {
        return Ok(());
    };
    let pids = [candidate.pid].into_iter().collect::<HashSet<_>>();
    let posted = post_close_to_windows(pids, Some(title))?;
    if posted == 0 {
        return Err(format!(
            "no visible '{}' window owned by PID {} accepted WM_CLOSE",
            title, candidate.pid
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn current_matches_application(left: &ApplicationInfo, right: &ApplicationInfo) -> bool {
    if let (Some(left_aumid), Some(right_aumid)) = (
        left.app_user_model_id.as_deref(),
        right.app_user_model_id.as_deref(),
    ) {
        if left_aumid.eq_ignore_ascii_case(right_aumid) {
            return true;
        }
    }
    if let (Some(left_path), Some(right_path)) = (
        left.executable_path.as_deref(),
        right.executable_path.as_deref(),
    ) {
        if normalized_path(left_path) == normalized_path(right_path) {
            return true;
        }
    }
    left.name.eq_ignore_ascii_case(&right.name)
}

fn ignored_surface_still_present(
    current: &[IgnoredCandidate],
    original: &IgnoredCandidate,
) -> bool {
    let Some(title) = original.window_title.as_deref() else {
        return false;
    };
    current.iter().any(|candidate| {
        candidate
            .executable
            .eq_ignore_ascii_case(&original.executable)
            && candidate
                .window_title
                .as_deref()
                .is_some_and(|value| value.eq_ignore_ascii_case(title))
    })
}

#[cfg(windows)]
fn close_unrelated_windows(snapshot: &Value, dry_run: bool) -> ApplicationCleanupReport {
    let mut report = ApplicationCleanupReport::default();
    let saved_desktop = match SavedDesktop::from_capsule(snapshot) {
        Ok(Some(desktop)) => desktop,
        Ok(None) => {
            report.failures.push(
                "replace mode requires an available saved desktop application inventory; this capsule does not have one, so Context Capsule refused to guess which running applications are unrelated"
                    .to_owned(),
            );
            return report;
        }
        Err(error) => {
            report.failures.push(error);
            return report;
        }
    };
    let current = match crate::desktop::discover() {
        Ok(desktop) => desktop,
        Err(error) => {
            report.failures.push(format!(
                "could not inspect running applications for replace mode: {error}"
            ));
            return report;
        }
    };

    let protected_pids = protected_process_chain();
    let mut candidates = Vec::new();
    let mut ignored_surface_candidates = Vec::new();

    for application in current.applications {
        report.applications_detected += 1;
        if belongs_to_capsule(snapshot, &saved_desktop.applications, &application) {
            report.applications_in_capsule += 1;
            continue;
        }
        if hosts_current_command(&application, &protected_pids) {
            report.applications_protected += 1;
            report.warnings.push(format!(
                "Preserved '{}' because it hosts the currently running Context Capsule command.",
                application.name
            ));
            continue;
        }
        report.applications_planned_to_close += 1;
        candidates.push(application);
    }

    // Some packaged/UWP applications expose their user-facing window through
    // ApplicationFrameHost.exe. Desktop capture intentionally classifies that
    // host process as a helper, so replace mode must consider its visible
    // surface separately. Compare against the saved ignored-window inventory so
    // a packaged app that was already present in the capsule is preserved.
    for candidate in current.ignored {
        if !is_application_frame_host(&candidate) || candidate.window_title.is_none() {
            continue;
        }
        report.applications_detected += 1;
        if ignored_surface_belongs_to_capsule(snapshot, &candidate) {
            report.applications_in_capsule += 1;
            continue;
        }
        report.applications_planned_to_close += 1;
        ignored_surface_candidates.push(candidate);
    }

    if dry_run || (candidates.is_empty() && ignored_surface_candidates.is_empty()) {
        return report;
    }

    for application in &candidates {
        report.close_requests_sent += 1;
        for error in graceful_close(application) {
            report
                .warnings
                .push(format!("{}: {error}", application.name));
        }
    }
    for candidate in &ignored_surface_candidates {
        report.close_requests_sent += 1;
        if let Err(error) = close_ignored_surface(candidate) {
            report.warnings.push(format!(
                "{}: {error}",
                candidate
                    .window_title
                    .as_deref()
                    .unwrap_or(&candidate.executable)
            ));
        }
    }

    thread::sleep(CLOSE_SETTLE);
    let after_grace = match crate::desktop::discover() {
        Ok(desktop) => desktop,
        Err(error) => {
            report.failures.push(format!(
                "cleanup requests were sent, but Context Capsule could not verify the resulting desktop state: {error}"
            ));
            return report;
        }
    };

    // Replace mode is explicit. A graceful request is attempted first so normal
    // shutdown hooks can run, but any unrelated non-shell application that is
    // still alive is force-terminated. Explorer is special: close its folder
    // windows again, never explorer.exe itself.
    for application in &candidates {
        if after_grace
            .applications
            .iter()
            .any(|current| current_matches_application(current, application))
        {
            for error in force_close(application) {
                report
                    .warnings
                    .push(format!("{}: {error}", application.name));
            }
        }
    }
    for candidate in &ignored_surface_candidates {
        if ignored_surface_still_present(&after_grace.ignored, candidate) {
            if let Err(error) = close_ignored_surface(candidate) {
                report.warnings.push(format!(
                    "{}: {error}",
                    candidate
                        .window_title
                        .as_deref()
                        .unwrap_or(&candidate.executable)
                ));
            }
        }
    }

    thread::sleep(FORCE_SETTLE);
    let final_state = match crate::desktop::discover() {
        Ok(desktop) => desktop,
        Err(error) => {
            report.failures.push(format!(
                "replace-mode cleanup could not verify its final desktop state: {error}"
            ));
            return report;
        }
    };

    for application in candidates {
        let still_running = final_state
            .applications
            .iter()
            .any(|current| current_matches_application(current, &application));
        if still_running {
            report.applications_remaining += 1;
            report.failures.push(format!(
                "'{}' is still running after replace-mode close and force-close attempts",
                application.name
            ));
        } else {
            report.applications_closed += 1;
        }
    }

    for candidate in ignored_surface_candidates {
        if ignored_surface_still_present(&final_state.ignored, &candidate) {
            report.applications_remaining += 1;
            report.failures.push(format!(
                "'{}' is still open after replace-mode WM_CLOSE attempts",
                candidate
                    .window_title
                    .as_deref()
                    .unwrap_or(&candidate.executable)
            ));
        } else {
            report.applications_closed += 1;
        }
    }

    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desktop::{ApplicationClassification, LaunchSpec, LaunchStrategy};
    use crate::restore::SavedLaunchSpec;

    fn current(name: &str, path: Option<&str>, aumid: Option<&str>) -> ApplicationInfo {
        ApplicationInfo {
            primary_pid: 1,
            pids: vec![1],
            parent_pid: None,
            name: name.to_owned(),
            executable_path: path.map(str::to_owned),
            app_user_model_id: aumid.map(str::to_owned),
            file_version: None,
            classification: ApplicationClassification::UserApplication,
            confidence: 100,
            classification_reason: "test".to_owned(),
            launch: path.map(|target| LaunchSpec {
                strategy: LaunchStrategy::Executable,
                target: target.to_owned(),
            }),
            windows: Vec::new(),
            discovered_as_background: false,
        }
    }

    fn saved(name: &str, path: Option<&str>, aumid: Option<&str>) -> SavedApplication {
        SavedApplication {
            name: name.to_owned(),
            executable_path: path.map(str::to_owned),
            app_user_model_id: aumid.map(str::to_owned),
            file_version: None,
            classification: "user-application".to_owned(),
            launch: path.map(|target| SavedLaunchSpec {
                strategy: "executable".to_owned(),
                target: target.to_owned(),
            }),
            windows: Vec::new(),
            discovered_as_background: false,
        }
    }

    fn ignored(pid: u32, executable: &str, title: &str) -> IgnoredCandidate {
        IgnoredCandidate {
            pid,
            parent_pid: None,
            executable: executable.to_owned(),
            executable_path: None,
            window_title: Some(title.to_owned()),
            classification: ApplicationClassification::ApplicationHelper,
            confidence: 98,
            reason: "test".to_owned(),
        }
    }

    #[test]
    fn cleanup_uses_strong_executable_identity_when_available() {
        let current = current("Editor", Some(r"C:\Apps\Editor.exe"), None);
        let saved = saved("Renamed Editor", Some(r"c:/apps/editor.exe"), None);
        assert!(current_matches_saved(&current, &saved));
    }

    #[test]
    fn strong_identity_mismatch_does_not_fall_back_to_name() {
        let current = current("Editor", Some(r"C:\Apps\Editor-v2.exe"), None);
        let saved = saved("Editor", Some(r"C:\Apps\Editor.exe"), None);
        assert!(!current_matches_saved(&current, &saved));
    }

    #[test]
    fn missing_current_strong_metadata_fails_safe_to_name() {
        let current = current("Editor", None, None);
        let saved = saved("Editor", Some(r"C:\Apps\Editor.exe"), None);
        assert!(current_matches_saved(&current, &saved));
    }

    #[test]
    fn name_is_used_when_saved_application_has_no_strong_identity() {
        let current = current("Notes", None, None);
        let saved = saved("notes", None, None);
        assert!(current_matches_saved(&current, &saved));
    }

    #[test]
    fn docker_desktop_is_semantically_owned_when_docker_resources_exist() {
        let docker = current(
            "Docker Desktop",
            Some(r"C:\Program Files\Docker\Docker\Docker Desktop.exe"),
            None,
        );
        let snapshot = serde_json::json!({
            "docker": {
                "compose_projects": [{ "name": "demo" }],
                "standalone_containers": []
            }
        });
        assert!(owned_by_semantic_resource(&snapshot, &docker));
    }

    #[test]
    fn new_application_frame_host_surface_is_unrelated() {
        let current = ignored(41, "ApplicationFrameHost.exe", "WhatsApp");
        let snapshot = serde_json::json!({
            "desktop": { "ignored": [] }
        });
        assert!(is_application_frame_host(&current));
        assert!(!ignored_surface_belongs_to_capsule(&snapshot, &current));
    }

    #[test]
    fn saved_application_frame_host_surface_is_preserved() {
        let current = ignored(41, "ApplicationFrameHost.exe", "WhatsApp");
        let snapshot = serde_json::json!({
            "desktop": {
                "ignored": [{
                    "executable": "applicationframehost.exe",
                    "window_title": "whatsapp"
                }]
            }
        });
        assert!(ignored_surface_belongs_to_capsule(&snapshot, &current));
    }
}
